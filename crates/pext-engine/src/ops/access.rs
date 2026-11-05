//! Access control: scoped tokens (read / append / write / full), per-account
//! allowlists, expiry, revocation and output redaction.

use crate::args::Args;
use crate::audit::sha256_hex;
use crate::crypto::{hex, random_bytes};
use crate::ctx::{iv, osv, sv, Actor, Ctx, Role};
use crate::ops::accounts::{load_account, resolve_account};
use crate::ops::categories::collapse_ws;
use crate::registry::{OpSpec, Param};
use crate::EngineConfig;
use chrono::NaiveDate;
use pext_core::{Error, Result};
use serde_json::{json, Value};

pub const TOKEN_PREFIX: &str = "pext_";

/// Fields whose text comes from outside (or is private) and is masked for
/// redacting actors.
pub const REDACT_KEYS: &[&str] = &["payee", "memo", "note", "person", "lender", "counterparty"];

/// Fields holding imported, user-controlled text: consumers such as LLM agents
/// must treat them as data, never as instructions.
pub const UNTRUSTED_KEYS: &[&str] = &[
    "payee",
    "memo",
    "note",
    "person",
    "lender",
    "name",
    "description",
    "tags",
    "institution",
];

pub fn redact(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                if REDACT_KEYS.contains(&k.as_str()) {
                    match val {
                        Value::String(s) if !s.is_empty() => *s = "***".into(),
                        Value::Array(_) | Value::Object(_) => redact(val),
                        _ => {}
                    }
                } else {
                    redact(val);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(redact),
        _ => {}
    }
}

pub fn authenticate(cfg: &EngineConfig, token: &str, today: NaiveDate) -> Result<Actor> {
    let invalid = || Error::auth("invalid_token", "the access token is not valid");
    if !token.starts_with(TOKEN_PREFIX)
        || token.len() != TOKEN_PREFIX.len() + 64
        || !token.is_ascii()
    {
        return Err(invalid());
    }
    let conn = crate::db::open_readonly(&cfg.db_path)?;
    let hash = sha256_hex(token);
    let row = conn
        .query_row(
            "SELECT id, name, role, accounts, redact, expires_at, revoked_at FROM tokens WHERE secret_hash=?1",
            [&hash],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => invalid(),
            other => crate::ctx::db_err(other),
        })?;
    let (id, name, role, accounts, redact, expires, revoked) = row;
    if revoked.is_some() {
        return Err(Error::auth(
            "token_revoked",
            format!("token '{name}' has been revoked"),
        ));
    }
    if let Some(e) = expires {
        if pext_core::dates::parse_date(&e)
            .map(|d| d < today)
            .unwrap_or(true)
        {
            return Err(Error::auth(
                "token_expired",
                format!("token '{name}' expired on {e}"),
            ));
        }
    }
    let accounts: Option<Vec<i64>> =
        match accounts {
            Some(a) => Some(serde_json::from_str(&a).map_err(|_| {
                Error::internal("bad_token_record", "token account list is corrupt")
            })?),
            None => None,
        };
    Ok(Actor {
        name: format!("token:{name}"),
        role: Role::parse(&role)
            .ok_or_else(|| Error::internal("bad_token_record", "token role is corrupt"))?,
        accounts,
        authenticated: true,
        token_id: Some(id),
        redact: redact == 1,
    })
}

fn find(ctx: &Ctx, r: &str) -> Result<(i64, String)> {
    let want = r.trim();
    let rows = ctx.rows("SELECT id, name FROM tokens", &[])?;
    rows.iter()
        .find(|t| {
            t["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(want))
                || want.parse::<i64>().ok() == t["id"].as_i64()
        })
        .map(|t| {
            (
                t["id"].as_i64().unwrap_or(0),
                t["name"].as_str().unwrap_or("").to_string(),
            )
        })
        .ok_or_else(|| {
            Error::not_found("token_not_found", format!("no token '{r}'"))
                .with_hint("run `pext token list`")
        })
}

fn create(ctx: &Ctx, a: &Args) -> Result<Value> {
    let name = collapse_ws(&a.str("name")?);
    if name.is_empty() || name.chars().count() > 64 {
        return Err(
            Error::validation("invalid_name", "the token name must be 1-64 characters")
                .with_field("name"),
        );
    }
    if ctx
        .opt_i64(
            "SELECT id FROM tokens WHERE name=?1 COLLATE NOCASE",
            &[sv(&name)],
        )?
        .is_some()
    {
        return Err(Error::conflict(
            "already_exists",
            format!("a token named '{name}' already exists"),
        ));
    }
    let role_s = a.opt_str("role").unwrap_or_else(|| "read".into());
    let role = Role::parse(&role_s).ok_or_else(|| {
        Error::validation("invalid_role", "role must be read, append, write or full")
    })?;
    let mut ids: Vec<i64> = vec![];
    for r in a.list("account") {
        let acc = resolve_account(ctx, &r)?;
        if !ids.contains(&acc.id) {
            ids.push(acc.id);
        }
    }
    if !ids.is_empty() && role == Role::Admin {
        return Err(Error::validation(
            "invalid_scope",
            "a full-access token cannot be limited to accounts",
        )
        .with_hint("use role read, append or write with --account"));
    }
    let expires = a.opt_date("expires")?;
    if expires.is_some_and(|d| d <= ctx.today) {
        return Err(
            Error::validation("invalid_date", "the expiry must be in the future")
                .with_field("expires"),
        );
    }
    let secret = format!("{TOKEN_PREFIX}{}", hex(&random_bytes::<32>()?));
    let id = ctx.insert(
        "tokens",
        &[
            ("name", sv(&name)),
            ("secret_hash", sv(&sha256_hex(&secret))),
            ("prefix", sv(&secret[..13])),
            ("role", sv(&role_s)),
            (
                "accounts",
                osv(if ids.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&ids).unwrap_or_default())
                }
                .as_deref()),
            ),
            ("redact", iv(a.flag("redact") as i64)),
            (
                "expires_at",
                osv(expires.map(crate::ops::txns::fmt_date).as_deref()),
            ),
            ("created_at", sv(&ctx.now)),
        ],
    )?;
    Ok(json!({
        "id": id, "name": name, "token": secret, "role": role_s, "accounts": ids, "redact": a.flag("redact"),
        "expires": expires.map(crate::ops::txns::fmt_date),
        "warning": "this is the only time the token is shown; store it somewhere safe",
    }))
}

fn list(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let mut out = vec![];
    for r in ctx.rows("SELECT id, name, prefix, role, accounts, redact, expires_at, revoked_at, created_at FROM tokens ORDER BY id", &[])? {
        let ids: Vec<i64> = r["accounts"].as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
        let mut names = vec![];
        for id in &ids {
            names.push(load_account(ctx, *id).map(|a| a.name).unwrap_or_else(|_| format!("#{id}")));
        }
        let expired = r["expires_at"].as_str().and_then(|e| pext_core::dates::parse_date(e).ok()).is_some_and(|d| d < ctx.today);
        let status = if r["revoked_at"].is_string() { "revoked" } else if expired { "expired" } else { "active" };
        out.push(json!({
            "id": r["id"], "name": r["name"], "prefix": r["prefix"], "role": r["role"], "accounts": names,
            "redact": r["redact"] == json!(1), "expires": r["expires_at"], "status": status, "created": r["created_at"],
        }));
    }
    Ok(Value::Array(out))
}

fn revoke(ctx: &Ctx, a: &Args) -> Result<Value> {
    let (id, name) = find(ctx, &a.str("name")?)?;
    if ctx
        .opt_string("SELECT revoked_at FROM tokens WHERE id=?1", &[iv(id)])?
        .is_some()
    {
        return Err(Error::conflict(
            "already_revoked",
            format!("token '{name}' is already revoked"),
        ));
    }
    ctx.update("tokens", id, &[("revoked_at", sv(&ctx.now))])?;
    Ok(json!({ "revoked": name }))
}

fn whoami(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let a = &ctx.actor;
    Ok(json!({
        "name": a.name, "role": a.role.as_str(), "authenticated": a.authenticated, "redact": a.redact,
        "accounts": a.accounts, "scoped": a.accounts.is_some(),
    }))
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new(
            "token.create",
            "Create an access token for an agent or app (shown once)",
            create,
        )
        .admin_write()
        .local()
        .p(Param::s("name", "what the token is for").req())
        .p(
            Param::s("role", "read (default), append (add only), write or full")
                .choices(&["read", "append", "write", "full"])
                .def("read"),
        )
        .p(Param::s("account", "limit to these accounts (repeatable)").multi())
        .p(Param::b(
            "redact",
            "mask payees, memos and notes in everything it reads",
        ))
        .p(Param::date("expires", "expiry date"))
        .ex("pext token create --name claude --role read --redact --expires 2027-01-01"),
        OpSpec::new("token.list", "List tokens (never shows secrets)", list)
            .admin()
            .local(),
        OpSpec::new("token.revoke", "Revoke a token immediately", revoke)
            .admin_write()
            .local()
            .p(Param::s("name", "token name or id").req().pos()),
        OpSpec::new(
            "whoami",
            "Show the identity and permissions of the caller",
            whoami,
        )
        .no_db(),
    ]
}
