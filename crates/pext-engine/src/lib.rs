//! pext-engine: the single service layer behind the CLI, TUI, web UI and MCP
//! server. Every capability is an *operation* in the registry; `Engine::call`
//! runs it with validation, authorisation, a transaction, invariant checks,
//! audit logging, idempotency and dry-run support.
#![deny(clippy::float_arithmetic)]
#![allow(clippy::result_large_err)]

pub mod args;
pub mod audit;
pub mod ctx;
pub mod db;
pub mod invariants;
pub mod lines;
pub mod ops;
pub mod registry;

use chrono::NaiveDate;
pub use ctx::{Actor, Role};
use ctx::{Ctx, DbResult};
use pext_core::{Error, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Map, Value};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub db_path: PathBuf,
    /// Override "today" (tests, scripted runs).
    pub today: Option<NaiveDate>,
    pub plugin_dir: Option<PathBuf>,
}

impl EngineConfig {
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        EngineConfig {
            db_path: db_path.into(),
            today: None,
            plugin_dir: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CallOpts {
    pub dry_run: bool,
    pub yes: bool,
    pub idempotency_key: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Output {
    pub data: Value,
    pub meta: Map<String, Value>,
}

pub struct Engine {
    pub cfg: EngineConfig,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        Engine { cfg }
    }

    pub fn today(&self) -> NaiveDate {
        self.cfg
            .today
            .unwrap_or_else(|| chrono::Local::now().date_naive())
    }

    fn now(&self) -> String {
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    /// Run an operation by name.
    pub fn call(
        &self,
        op: &str,
        raw: Map<String, Value>,
        opts: &CallOpts,
        actor: &Actor,
    ) -> Result<Output> {
        let spec = registry::find(op).ok_or_else(|| {
            Error::usage("unknown_command", format!("unknown command '{op}'"))
                .with_hint("run `pext commands` to list commands")
        })?;
        if actor.role < spec.access {
            return Err(Error::auth(
                "forbidden",
                format!(
                    "role '{}' may not run '{}' (requires '{}')",
                    actor.role.as_str(),
                    spec.name,
                    spec.access.as_str()
                ),
            ));
        }
        if actor.accounts.is_some() && !spec.scoped_ok {
            return Err(Error::auth(
                "scope_denied",
                format!("account-scoped tokens may not run '{}'", spec.name),
            ));
        }
        let today = self.today();
        let mut args = registry::validate(spec, raw, today)?;
        if opts.yes {
            args.map.insert("yes".into(), json!(true));
        }
        if let Some(k) = &opts.idempotency_key {
            if !spec.mutating {
                return Err(Error::usage(
                    "idempotency_not_applicable",
                    "idempotency keys apply to mutating commands only",
                ));
            }
            if k.is_empty() || k.len() > 128 || !k.chars().all(|c| c.is_ascii_graphic()) {
                return Err(Error::validation(
                    "invalid_idempotency_key",
                    "idempotency key must be 1-128 printable ASCII characters",
                )
                .with_field("idempotency_key"));
            }
        }

        if !spec.needs_db {
            let conn = Connection::open_in_memory().db()?;
            let ctx = Ctx::new(&conn, &self.cfg, today, self.now(), actor.clone());
            let data = (spec.handler)(&ctx, &args)?;
            return Ok(assemble(data, Map::new(), &ctx));
        }

        let conn = db::open(&self.cfg.db_path, spec.creates_db)?;
        if !actor.authenticated && spec.name != "init" {
            let req: Option<String> = conn
                .query_row(
                    "SELECT value FROM settings WHERE key='require_token'",
                    [],
                    |r| r.get(0),
                )
                .optional()
                .db()?;
            if req.as_deref() == Some("true") {
                return Err(Error::auth(
                    "token_required",
                    "this database requires an access token",
                )
                .with_hint("pass --token or set PEXT_TOKEN"));
            }
        }

        if !spec.mutating {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Deferred).db()?;
            let ctx = Ctx::new(&tx, &self.cfg, today, self.now(), actor.clone());
            let data = (spec.handler)(&ctx, &args)?;
            return Ok(assemble(data, Map::new(), &ctx));
        }

        let args_json = audit::canonical_json(&Value::Object(args.map.clone()));
        let args_hash = audit::sha256_hex(&format!("{}\n{}", spec.name, args_json));
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).db()?;
        if let Some(key) = &opts.idempotency_key {
            let prev: Option<(String, String, String)> = tx
                .query_row(
                    "SELECT op, args_hash, result FROM idempotency WHERE key=?1",
                    [key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .db()?;
            if let Some((pop, phash, result)) = prev {
                if pop != spec.name || phash != args_hash {
                    return Err(Error::conflict(
                        "idempotency_conflict",
                        format!("idempotency key '{key}' was already used for a different request"),
                    )
                    .with_hint("use a fresh key for a different request"));
                }
                let stored: Value = serde_json::from_str(&result).map_err(|_| {
                    Error::internal("bad_idempotency_record", "stored result is corrupt")
                })?;
                let mut meta = Map::new();
                meta.insert("replayed".into(), json!(true));
                if let Some(w) = stored
                    .get("warnings")
                    .filter(|w| w.as_array().is_some_and(|a| !a.is_empty()))
                {
                    meta.insert("warnings".into(), w.clone());
                }
                let data = stored.get("data").cloned().unwrap_or(Value::Null);
                if let Value::Array(a) = &data {
                    meta.insert("count".into(), json!(a.len()));
                }
                return Ok(Output { data, meta });
            }
        }
        let (data, changes, warnings, undo_of, exit_code) = {
            let ctx = Ctx::new(&tx, &self.cfg, today, self.now(), actor.clone());
            let data = (spec.handler)(&ctx, &args)?;
            let changes = ctx.take_changes();
            invariants::verify(&tx, &changes)?;
            (
                data,
                changes,
                ctx.take_warnings(),
                ctx.undo_of(),
                ctx.exit_code(),
            )
        };
        let mut meta = Map::new();
        if !warnings.is_empty() {
            meta.insert("warnings".into(), Value::Array(warnings.clone()));
        }
        if exit_code != 0 {
            meta.insert("exit_code".into(), json!(exit_code));
        }
        if let Value::Array(a) = &data {
            meta.insert("count".into(), json!(a.len()));
        }
        if opts.dry_run {
            meta.insert("dry_run".into(), json!(true));
            meta.insert(
                "changes".into(),
                Value::Array(
                    changes
                        .iter()
                        .map(|c| json!({ "action": c.action(), "table": c.table, "id": c.id }))
                        .collect(),
                ),
            );
            drop(tx); // rolls back
            return Ok(Output { data, meta });
        }
        let changes_json = Value::Array(changes.iter().map(|c| c.to_json()).collect()).to_string();
        let audit_id = audit::append(
            &tx,
            &self.now(),
            &actor.name,
            spec.name,
            &args_json,
            &changes_json,
        )?;
        if let Some(target) = undo_of {
            tx.execute(
                "UPDATE audit SET undone_by=?1 WHERE id=?2",
                rusqlite::params![audit_id, target],
            )
            .db()?;
        }
        if let Some(key) = &opts.idempotency_key {
            let stored = json!({ "data": data, "warnings": warnings }).to_string();
            tx.execute(
                "INSERT INTO idempotency(key, op, args_hash, result, audit_id, created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                rusqlite::params![key, spec.name, args_hash, stored, audit_id, self.now()],
            )
            .db()?;
        }
        tx.commit().db()?;
        meta.insert("audit_id".into(), json!(audit_id));
        Ok(Output { data, meta })
    }
}

fn assemble(data: Value, mut meta: Map<String, Value>, ctx: &Ctx) -> Output {
    let warnings = ctx.take_warnings();
    if !warnings.is_empty() {
        meta.insert("warnings".into(), Value::Array(warnings));
    }
    if ctx.exit_code() != 0 {
        meta.insert("exit_code".into(), json!(ctx.exit_code()));
    }
    if let Value::Array(a) = &data {
        meta.insert("count".into(), json!(a.len()));
    }
    Output { data, meta }
}
