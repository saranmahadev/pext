//! Setup, settings, introspection (`commands`, `schema`) and health checks.

use crate::args::Args;
use crate::audit;
use crate::ctx::{iv, sv, Ctx, DbResult};
use crate::ops::categories::seed_defaults;
use crate::registry::{self, OpSpec, Param};
use pext_core::money::validate_currency;
use pext_core::{Error, Result};
use serde_json::{json, Value};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Settings the user may change: key, description, validator.
const SETTINGS: &[(&str, &str)] = &[
    (
        "base_currency",
        "reporting currency (ISO 4217, fixed after init)",
    ),
    (
        "fy_start_month",
        "first month of the financial year, 1-12 (4 = April)",
    ),
    ("week_start", "first day of the week: mon or sun"),
    (
        "require_token",
        "true/false: demand an access token for every CLI call",
    ),
    (
        "default_account",
        "account name used when --account is omitted",
    ),
    (
        "large_txn_minor",
        "amount (minor units) above which a transaction raises a warning",
    ),
];

fn setting_known(k: &str) -> bool {
    SETTINGS.iter().any(|(n, _)| *n == k)
}

fn validate_setting(key: &str, value: &str) -> Result<String> {
    let bad =
        |m: &str| Error::validation("invalid_setting", format!("{key}: {m}")).with_field("value");
    match key {
        "fy_start_month" => {
            let m: u32 = value
                .trim()
                .parse()
                .map_err(|_| bad("must be a number 1-12"))?;
            if !(1..=12).contains(&m) {
                return Err(bad("must be between 1 and 12"));
            }
            Ok(m.to_string())
        }
        "week_start" => match value.to_ascii_lowercase().as_str() {
            v @ ("mon" | "sun") => Ok(v.to_string()),
            _ => Err(bad("must be mon or sun")),
        },
        "require_token" => match value.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Ok("true".into()),
            "false" | "0" | "no" => Ok("false".into()),
            _ => Err(bad("must be true or false")),
        },
        "large_txn_minor" => {
            let n: i64 = value
                .trim()
                .parse()
                .map_err(|_| bad("must be an integer"))?;
            if n < 0 {
                return Err(bad("must not be negative"));
            }
            Ok(n.to_string())
        }
        "default_account" => Ok(value.trim().to_string()),
        _ => Err(bad("cannot be changed")),
    }
}

fn init(ctx: &Ctx, a: &Args) -> Result<Value> {
    if ctx.setting("base_currency")?.is_some() {
        return Err(Error::conflict(
            "already_initialized",
            "this database is already initialised",
        )
        .with_hint("use `pext config set` to change settings"));
    }
    let cur = a
        .opt_str("currency")
        .unwrap_or_else(|| "INR".into())
        .to_ascii_uppercase();
    validate_currency(&cur)?;
    let fy = validate_setting(
        "fy_start_month",
        &a.opt_str("fy_start_month").unwrap_or_else(|| "4".into()),
    )?;
    ctx.set_setting("base_currency", &cur)?;
    ctx.set_setting("fy_start_month", &fy)?;
    ctx.set_setting("week_start", "mon")?;
    ctx.set_setting("require_token", "false")?;
    let seeded = if a.flag("no_categories") {
        0
    } else {
        seed_defaults(ctx)?
    };
    ctx.conn
        .execute(
            "INSERT OR REPLACE INTO meta(key,value) VALUES('created_at', ?1)",
            [&ctx.now],
        )
        .db()?;
    Ok(json!({
        "db": ctx.cfg.db_path.display().to_string(),
        "base_currency": cur,
        "fy_start_month": fy.parse::<i64>().unwrap_or(4),
        "categories_seeded": seeded,
        "schema_version": crate::db::latest_version(),
        "next": ["pext acct add <name> --kind checking", "pext txn add ...", "pext commands"],
    }))
}

fn version(_ctx: &Ctx, _a: &Args) -> Result<Value> {
    Ok(
        json!({ "version": VERSION, "schema_version": crate::db::latest_version(), "api": "pext/v1" }),
    )
}

fn config_list(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let mut out = vec![];
    for (k, d) in SETTINGS {
        out.push(json!({ "key": k, "value": ctx.setting(k)?, "description": d }));
    }
    Ok(Value::Array(out))
}

fn config_get(ctx: &Ctx, a: &Args) -> Result<Value> {
    let key = a.str("key")?;
    if !setting_known(&key) {
        return Err(unknown_setting(&key));
    }
    Ok(json!({ "key": key, "value": ctx.setting(&key)? }))
}

fn unknown_setting(key: &str) -> Error {
    Error::not_found("unknown_setting", format!("unknown setting '{key}'"))
        .with_details(json!({ "settings": SETTINGS.iter().map(|s| s.0).collect::<Vec<_>>() }))
        .with_hint("run `pext config list`")
}

fn config_set(ctx: &Ctx, a: &Args) -> Result<Value> {
    let key = a.str("key")?;
    if !setting_known(&key) {
        return Err(unknown_setting(&key));
    }
    if key == "base_currency" {
        return Err(Error::validation("immutable_setting", "the base currency cannot be changed after init")
            .with_hint("amounts are stored in minor units of each account's currency; create a new database to change the base"));
    }
    let value = validate_setting(&key, &a.str("value")?)?;
    if key == "require_token"
        && value == "true"
        && ctx.one_i64("SELECT COUNT(*) FROM settings WHERE key='token_count'", &[])? == 0
    {
        ctx.warn(
            "no_tokens_yet",
            "require_token is on; make sure you have created a token or you will be locked out of the CLI",
            json!({}),
        );
    }
    ctx.set_setting(&key, &value)?;
    Ok(json!({ "key": key, "value": value }))
}

fn commands(_ctx: &Ctx, a: &Args) -> Result<Value> {
    let filter = a.opt_str("group");
    let list: Vec<Value> = registry::all()
        .iter()
        .filter(|o| match &filter {
            Some(g) => o.name == g.as_str() || o.name.starts_with(&format!("{g}.")),
            None => true,
        })
        .map(registry::describe)
        .collect();
    if list.is_empty() {
        return Err(Error::not_found("unknown_group", "no commands match")
            .with_hint("run `pext commands` without a filter"));
    }
    Ok(Value::Array(list))
}

fn schema(_ctx: &Ctx, a: &Args) -> Result<Value> {
    let name = a.str("command")?.replace(' ', ".");
    let spec = registry::find(&name).ok_or_else(|| {
        Error::not_found("unknown_command", format!("unknown command '{name}'"))
            .with_hint("run `pext commands`")
    })?;
    Ok(json!({
        "name": spec.name,
        "description": spec.about,
        "input_schema": registry::input_schema(spec),
        "mutating": spec.mutating,
    }))
}

fn doctor(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let mut checks: Vec<Value> = vec![];
    let mut ok = true;
    let mut add = |name: &str, pass: bool, detail: Value| {
        if !pass {
            ok = false;
        }
        checks.push(json!({ "check": name, "ok": pass, "detail": detail }));
    };

    let integrity: String = ctx
        .conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .db()?;
    add("sqlite_integrity", integrity == "ok", json!(integrity));

    let fk_bad = ctx
        .conn
        .prepare("PRAGMA foreign_key_check")
        .db()?
        .query_map([], |_| Ok(()))
        .db()?
        .count();
    add("foreign_keys", fk_bad == 0, json!({ "violations": fk_bad }));

    let rep = audit::verify_chain(ctx.conn)?;
    add(
        "audit_chain",
        rep.ok,
        json!({ "entries": rep.entries, "head": rep.head, "first_bad": rep.first_bad, "reason": rep.reason }),
    );

    // Every live transaction must satisfy the ledger invariants.
    let ids: Vec<i64> = {
        let mut st = ctx
            .conn
            .prepare("SELECT id FROM transactions WHERE deleted_at IS NULL")
            .db()?;
        let rows = st.query_map([], |r| r.get(0)).db()?;
        rows.collect::<rusqlite::Result<_>>().db()?
    };
    let mut bad = vec![];
    for id in &ids {
        if let Err(e) = crate::invariants::check_txn(ctx.conn, *id) {
            bad.push(json!({ "txn": id, "error": e.message }));
        }
    }
    add(
        "ledger_invariants",
        bad.is_empty(),
        json!({ "checked": ids.len(), "violations": bad }),
    );

    let orphan_splits = ctx.one_i64(
        "SELECT COUNT(*) FROM splits s LEFT JOIN transactions t ON t.id=s.txn_id WHERE t.id IS NULL",
        &[],
    )?;
    add(
        "orphan_splits",
        orphan_splits == 0,
        json!({ "count": orphan_splits }),
    );

    let ver = crate::db::version(ctx.conn)?;
    add(
        "schema_version",
        ver == crate::db::latest_version(),
        json!({ "db": ver, "expected": crate::db::latest_version() }),
    );

    if !ok {
        ctx.set_exit_code(pext_core::ErrorKind::Integrity.exit_code());
    }
    Ok(json!({ "ok": ok, "checks": checks }))
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new("init", "Create and initialise a database", init)
            .admin_write()
            .creates()
            .p(Param::s("currency", "base (reporting) currency").def("INR"))
            .p(Param::s("fy_start_month", "first month of the financial year (1-12)").def("4"))
            .p(Param::b(
                "no_categories",
                "do not seed the default category tree",
            ))
            .ex("pext init --currency INR"),
        OpSpec::new("version", "Show the pext version", version).no_db(),
        OpSpec::new("config.list", "List settings", config_list),
        OpSpec::new("config.get", "Read one setting", config_get).p(Param::s(
            "key",
            "setting name",
        )
        .req()
        .pos()),
        OpSpec::new("config.set", "Change a setting", config_set)
            .admin_write()
            .p(Param::s("key", "setting name").req().pos())
            .p(Param::s("value", "new value").req().pos())
            .ex("pext config set fy_start_month 4"),
        OpSpec::new(
            "commands",
            "List every command with its parameters (machine-readable)",
            commands,
        )
        .no_db()
        .p(Param::s("group", "only commands in this group, e.g. txn")),
        OpSpec::new("schema", "JSON Schema for one command's input", schema)
            .no_db()
            .p(Param::s("command", "command name, e.g. txn.add")
                .req()
                .pos()),
        OpSpec::new(
            "doctor",
            "Check database, ledger and audit-log integrity",
            doctor,
        )
        .admin(),
    ]
}

#[allow(dead_code)]
fn _keep(_: i64) -> rusqlite::types::Value {
    iv(0)
}
#[allow(dead_code)]
fn _keep2() -> rusqlite::types::Value {
    sv("")
}
