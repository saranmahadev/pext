//! SQLite connection setup and versioned migrations.

use crate::ctx::DbResult;
use pext_core::{Error, Result};
use rusqlite::Connection;
use std::path::Path;

/// Migration scripts, applied in order. `PRAGMA user_version` records progress.
pub const MIGRATIONS: &[&str] = &[M1, M2];

pub fn latest_version() -> i64 {
    MIGRATIONS.len() as i64
}

pub fn open(path: &Path, create: bool) -> Result<Connection> {
    if !create && !path.exists() {
        return Err(
            Error::not_found("db_not_found", format!("no database at {}", path.display()))
                .with_hint("run `pext init` (or pass --db PATH / set PEXT_DB)"),
        );
    }
    if create {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    Error::io(
                        "create_dir_failed",
                        format!("cannot create {}: {e}", parent.display()),
                    )
                })?;
            }
        }
    }
    let conn = Connection::open(path).db()?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

pub fn open_readonly(path: &Path) -> Result<Connection> {
    if !path.exists() {
        return Err(
            Error::not_found("db_not_found", format!("no database at {}", path.display()))
                .with_hint("run `pext init`"),
        );
    }
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .db()?;
    conn.execute_batch("PRAGMA query_only=ON; PRAGMA busy_timeout=5000;")
        .db()?;
    Ok(conn)
}

pub fn configure(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout=5000;",
    )
    .db()
}

pub fn version(conn: &Connection) -> Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0)).db()
}

pub fn migrate(conn: &Connection) -> Result<()> {
    let cur = version(conn)?;
    if cur > latest_version() {
        return Err(Error::conflict(
            "db_newer",
            format!(
                "database schema v{cur} is newer than this pext (v{})",
                latest_version()
            ),
        )
        .with_hint("upgrade pext"));
    }
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let v = i as i64 + 1;
        if v <= cur {
            continue;
        }
        conn.execute_batch("BEGIN IMMEDIATE").db()?;
        let r = conn
            .execute_batch(sql)
            .and_then(|_| conn.execute_batch(&format!("PRAGMA user_version={v}")));
        match r {
            Ok(_) => conn.execute_batch("COMMIT").db()?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(Error::internal(
                    "migration_failed",
                    format!("migration {v} failed: {e}"),
                ));
            }
        }
    }
    Ok(())
}

const M1: &str = r#"
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;

CREATE TABLE settings(
  id INTEGER PRIMARY KEY,
  key TEXT NOT NULL UNIQUE,
  value TEXT NOT NULL
);

CREATE TABLE accounts(
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE COLLATE NOCASE CHECK(length(name) BETWEEN 1 AND 64),
  kind TEXT NOT NULL CHECK(kind IN ('checking','savings','credit_card','cash','wallet','brokerage',
       'retirement','loan','mortgage','crypto','property','vehicle','other_asset','other_liability')),
  currency TEXT NOT NULL CHECK(length(currency)=3),
  institution TEXT,
  group_name TEXT,
  opening_minor INTEGER NOT NULL DEFAULT 0 CHECK(abs(opening_minor) <= 10000000000000),
  opened_on TEXT NOT NULL,
  closed_on TEXT,
  archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1)),
  credit_limit_minor INTEGER CHECK(credit_limit_minor IS NULL OR credit_limit_minor >= 0),
  created_at TEXT NOT NULL
);

CREATE TABLE categories(
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE COLLATE NOCASE,
  parent_id INTEGER REFERENCES categories(id),
  name TEXT NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('expense','income')),
  group_name TEXT
);

CREATE TABLE import_batches(
  id INTEGER PRIMARY KEY,
  source TEXT NOT NULL,
  kind TEXT NOT NULL,
  account_id INTEGER NOT NULL REFERENCES accounts(id),
  created_at TEXT NOT NULL,
  row_count INTEGER NOT NULL,
  rolled_back INTEGER NOT NULL DEFAULT 0 CHECK(rolled_back IN (0,1))
);

CREATE TABLE import_profiles(
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  config TEXT NOT NULL
);

CREATE TABLE transactions(
  id INTEGER PRIMARY KEY,
  account_id INTEGER NOT NULL REFERENCES accounts(id),
  date TEXT NOT NULL,
  amount_minor INTEGER NOT NULL CHECK(amount_minor != 0 AND abs(amount_minor) <= 10000000000000),
  currency TEXT NOT NULL CHECK(length(currency)=3),
  payee TEXT NOT NULL DEFAULT '',
  memo TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL DEFAULT 'cleared' CHECK(status IN ('pending','cleared','reconciled')),
  kind TEXT NOT NULL DEFAULT 'standard' CHECK(kind IN ('standard','transfer','adjustment')),
  transfer_id INTEGER,
  external_ref TEXT,
  fingerprint TEXT,
  import_batch INTEGER REFERENCES import_batches(id),
  deleted_at TEXT,
  created_at TEXT NOT NULL
);
CREATE INDEX idx_txn_account_date ON transactions(account_id, date);
CREATE INDEX idx_txn_date ON transactions(date);
CREATE INDEX idx_txn_batch ON transactions(import_batch);
CREATE UNIQUE INDEX uq_txn_extref ON transactions(account_id, external_ref)
  WHERE external_ref IS NOT NULL AND deleted_at IS NULL;
CREATE UNIQUE INDEX uq_txn_fingerprint ON transactions(account_id, fingerprint)
  WHERE fingerprint IS NOT NULL AND deleted_at IS NULL;

CREATE TABLE splits(
  id INTEGER PRIMARY KEY,
  txn_id INTEGER NOT NULL REFERENCES transactions(id),
  category_id INTEGER REFERENCES categories(id),
  amount_minor INTEGER NOT NULL CHECK(amount_minor != 0),
  note TEXT NOT NULL DEFAULT ''
);
CREATE INDEX idx_splits_txn ON splits(txn_id);
CREATE INDEX idx_splits_cat ON splits(category_id);

CREATE TABLE txn_tags(
  id INTEGER PRIMARY KEY,
  txn_id INTEGER NOT NULL REFERENCES transactions(id),
  tag TEXT NOT NULL COLLATE NOCASE,
  UNIQUE(txn_id, tag)
);

CREATE TABLE payee_aliases(
  id INTEGER PRIMARY KEY,
  alias TEXT NOT NULL UNIQUE COLLATE NOCASE,
  canonical TEXT NOT NULL
);

CREATE TABLE audit(
  id INTEGER PRIMARY KEY,
  ts TEXT NOT NULL,
  actor TEXT NOT NULL,
  op TEXT NOT NULL,
  args TEXT NOT NULL,
  changes TEXT NOT NULL,
  prev_hash TEXT NOT NULL,
  hash TEXT NOT NULL,
  undone_by INTEGER
);
CREATE TRIGGER audit_no_delete BEFORE DELETE ON audit
BEGIN SELECT RAISE(ABORT, 'audit log is append-only'); END;
CREATE TRIGGER audit_only_undone_by BEFORE UPDATE ON audit
WHEN NEW.id != OLD.id OR NEW.ts != OLD.ts OR NEW.actor != OLD.actor OR NEW.op != OLD.op
  OR NEW.args != OLD.args OR NEW.changes != OLD.changes OR NEW.prev_hash != OLD.prev_hash OR NEW.hash != OLD.hash
BEGIN SELECT RAISE(ABORT, 'audit log is append-only'); END;

CREATE TABLE idempotency(
  key TEXT PRIMARY KEY,
  op TEXT NOT NULL,
  args_hash TEXT NOT NULL,
  result TEXT NOT NULL,
  audit_id INTEGER,
  created_at TEXT NOT NULL
);
"#;

/// Phase 2: exchange rates, saved views, rules, budgets, recurring items, goals, alert settings.
const M2: &str = r#"
CREATE TABLE fx_rates(
  id INTEGER PRIMARY KEY,
  base TEXT NOT NULL CHECK(length(base)=3),
  quote TEXT NOT NULL CHECK(length(quote)=3),
  date TEXT NOT NULL,
  rate_e8 INTEGER NOT NULL CHECK(rate_e8 > 0 AND rate_e8 <= 100000000000000000),
  source TEXT NOT NULL DEFAULT 'manual',
  CHECK(base != quote),
  UNIQUE(base, quote, date)
);

CREATE TABLE saved_views(
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE COLLATE NOCASE CHECK(length(name) BETWEEN 1 AND 64),
  query TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL
);

CREATE TABLE rules(
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE COLLATE NOCASE CHECK(length(name) BETWEEN 1 AND 64),
  priority INTEGER NOT NULL DEFAULT 100,
  query TEXT NOT NULL,
  set_category_id INTEGER REFERENCES categories(id),
  set_payee TEXT,
  add_tags TEXT NOT NULL DEFAULT '',
  enabled INTEGER NOT NULL DEFAULT 1 CHECK(enabled IN (0,1)),
  created_at TEXT NOT NULL
);

CREATE TABLE budgets(
  id INTEGER PRIMARY KEY,
  category_id INTEGER NOT NULL REFERENCES categories(id),
  period TEXT NOT NULL CHECK(period IN ('monthly','quarterly','yearly')),
  amount_minor INTEGER NOT NULL CHECK(amount_minor > 0 AND amount_minor <= 10000000000000),
  rollover INTEGER NOT NULL DEFAULT 0 CHECK(rollover IN (0,1)),
  start_date TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(category_id, period)
);

CREATE TABLE recurring(
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE COLLATE NOCASE CHECK(length(name) BETWEEN 1 AND 64),
  kind TEXT NOT NULL CHECK(kind IN ('bill','income','emi','subscription','other')),
  account_id INTEGER NOT NULL REFERENCES accounts(id),
  amount_minor INTEGER NOT NULL CHECK(amount_minor != 0 AND abs(amount_minor) <= 10000000000000),
  category_id INTEGER REFERENCES categories(id),
  payee TEXT NOT NULL DEFAULT '',
  freq TEXT NOT NULL CHECK(freq IN ('daily','weekly','monthly','quarterly','yearly')),
  every INTEGER NOT NULL DEFAULT 1 CHECK(every BETWEEN 1 AND 366),
  start_date TEXT NOT NULL,
  next_index INTEGER NOT NULL DEFAULT 0 CHECK(next_index >= 0),
  next_due TEXT NOT NULL,
  end_date TEXT,
  auto_post INTEGER NOT NULL DEFAULT 0 CHECK(auto_post IN (0,1)),
  remind_days INTEGER NOT NULL DEFAULT 3 CHECK(remind_days BETWEEN 0 AND 60),
  active INTEGER NOT NULL DEFAULT 1 CHECK(active IN (0,1)),
  created_at TEXT NOT NULL
);

CREATE TABLE goals(
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE COLLATE NOCASE CHECK(length(name) BETWEEN 1 AND 64),
  target_minor INTEGER NOT NULL CHECK(target_minor > 0 AND target_minor <= 10000000000000),
  currency TEXT NOT NULL CHECK(length(currency)=3),
  target_date TEXT,
  account_id INTEGER REFERENCES accounts(id),
  archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1)),
  created_at TEXT NOT NULL
);

CREATE TABLE goal_contributions(
  id INTEGER PRIMARY KEY,
  goal_id INTEGER NOT NULL REFERENCES goals(id),
  date TEXT NOT NULL,
  amount_minor INTEGER NOT NULL CHECK(amount_minor != 0 AND abs(amount_minor) <= 10000000000000),
  note TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL
);
CREATE INDEX idx_goal_contrib ON goal_contributions(goal_id, date);

CREATE TABLE alert_settings(
  id INTEGER PRIMARY KEY,
  kind TEXT NOT NULL CHECK(kind IN ('low_balance')),
  account_id INTEGER NOT NULL REFERENCES accounts(id),
  threshold_minor INTEGER NOT NULL CHECK(abs(threshold_minor) <= 10000000000000),
  UNIQUE(kind, account_id)
);
"#;
