//! Execution context handed to every operation handler. All writes go through
//! `insert`/`update`/`delete`, which record row-level before/after snapshots so
//! that every mutation is auditable and undoable.

use crate::EngineConfig;
use chrono::NaiveDate;
use pext_core::{Error, Result};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{params_from_iter, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Read = 1,
    Append = 2,
    Write = 3,
    Admin = 4,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Append => "append",
            Role::Write => "write",
            Role::Admin => "full",
        }
    }
    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "read" => Role::Read,
            "append" => Role::Append,
            "write" => Role::Write,
            "full" | "admin" => Role::Admin,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Actor {
    pub name: String,
    pub role: Role,
    /// `Some(ids)` restricts the actor to those accounts.
    pub accounts: Option<Vec<i64>>,
    pub authenticated: bool,
    pub token_id: Option<i64>,
    /// Mask payees, memos and notes in everything this actor reads.
    pub redact: bool,
}

impl Actor {
    /// The local owner running the CLI directly.
    pub fn owner() -> Self {
        Actor {
            name: "local".into(),
            role: Role::Admin,
            accounts: None,
            authenticated: false,
            token_id: None,
            redact: false,
        }
    }
}

pub trait DbResult<T> {
    fn db(self) -> Result<T>;
}

impl<T> DbResult<T> for rusqlite::Result<T> {
    fn db(self) -> Result<T> {
        self.map_err(db_err)
    }
}

pub fn db_err(e: rusqlite::Error) -> Error {
    let msg = e.to_string();
    if let rusqlite::Error::SqliteFailure(f, _) = &e {
        if f.code == rusqlite::ErrorCode::ConstraintViolation {
            if let Some(rest) = msg.strip_prefix("UNIQUE constraint failed: ") {
                return Error::conflict("already_exists", format!("{rest} already exists"))
                    .with_hint("choose a different value or edit the existing record");
            }
            return Error::conflict("constraint_violation", msg)
                .with_hint("the change violates an integrity rule");
        }
        if f.code == rusqlite::ErrorCode::DatabaseBusy
            || f.code == rusqlite::ErrorCode::DatabaseLocked
        {
            return Error::io("db_busy", "the database is locked by another process")
                .with_hint("retry in a moment");
        }
    }
    Error::internal("db_error", msg)
}

const TABLES: &[&str] = &[
    "settings",
    "accounts",
    "categories",
    "transactions",
    "splits",
    "txn_tags",
    "payee_aliases",
    "import_batches",
    "import_profiles",
    "budgets",
    "rules",
    "recurring",
    "goals",
    "goal_contributions",
    "saved_views",
    "alert_settings",
    "loans",
    "ious",
    "instruments",
    "trades",
    "prices",
    "alloc_targets",
    "assets",
    "asset_values",
    "fx_rates",
    "snapshots",
    "tokens",
    "plugins",
];

fn check_table(t: &str) -> Result<()> {
    if TABLES.contains(&t) {
        Ok(())
    } else {
        Err(Error::internal(
            "bad_table",
            format!("table '{t}' is not auditable"),
        ))
    }
}

pub fn value_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(format!("blob:{}", b.len())),
    }
}

pub fn json_to_sql(v: &Value) -> Result<SqlValue> {
    Ok(match v {
        Value::Null => SqlValue::Null,
        Value::Number(n) => match n.as_i64() {
            Some(i) => SqlValue::Integer(i),
            None => SqlValue::Real(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => SqlValue::Text(s.clone()),
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        _ => {
            return Err(Error::internal(
                "bad_snapshot",
                "unsupported value in snapshot",
            ))
        }
    })
}

/// SELECT * FROM table WHERE id = ? as a JSON object.
pub fn snapshot(conn: &Connection, table: &str, id: i64) -> Result<Option<Map<String, Value>>> {
    check_table(table)?;
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} WHERE id = ?1"))
        .db()?;
    let cols: Vec<String> = stmt.column_names().map(|s| s.to_string()).collect();
    let mut rows = stmt.query([id]).db()?;
    match rows.next().db()? {
        Some(row) => {
            let mut m = Map::new();
            for (i, c) in cols.iter().enumerate() {
                m.insert(c.clone(), value_to_json(row.get_ref(i).db()?));
            }
            Ok(Some(m))
        }
        None => Ok(None),
    }
}

/// Run any SELECT and return rows as JSON objects (column order preserved).
pub fn query_json(
    conn: &Connection,
    sql: &str,
    params: &[SqlValue],
) -> Result<Vec<Map<String, Value>>> {
    let mut stmt = conn.prepare(sql).db()?;
    let cols: Vec<String> = stmt.column_names().map(|s| s.to_string()).collect();
    let mut rows = stmt.query(params_from_iter(params.iter())).db()?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().db()? {
        let mut m = Map::new();
        for (i, c) in cols.iter().enumerate() {
            m.insert(c.clone(), value_to_json(row.get_ref(i).db()?));
        }
        out.push(m);
    }
    Ok(out)
}

#[derive(Clone, Debug)]
pub struct Change {
    pub table: String,
    pub id: i64,
    pub before: Option<Map<String, Value>>,
    pub after: Option<Map<String, Value>>,
}

impl Change {
    pub fn action(&self) -> &'static str {
        match (&self.before, &self.after) {
            (None, Some(_)) => "insert",
            (Some(_), None) => "delete",
            _ => "update",
        }
    }
    pub fn to_json(&self) -> Value {
        json!({
            "table": self.table, "id": self.id,
            "before": self.before, "after": self.after,
        })
    }
    pub fn from_json(v: &Value) -> Result<Change> {
        let bad = || Error::internal("bad_audit_entry", "audit entry has malformed changes");
        let obj_of = |x: &Value| -> Result<Option<Map<String, Value>>> {
            match x {
                Value::Null => Ok(None),
                Value::Object(m) => Ok(Some(m.clone())),
                _ => Err(bad()),
            }
        };
        Ok(Change {
            table: v
                .get("table")
                .and_then(|x| x.as_str())
                .ok_or_else(bad)?
                .to_string(),
            id: v.get("id").and_then(|x| x.as_i64()).ok_or_else(bad)?,
            before: obj_of(v.get("before").unwrap_or(&Value::Null))?,
            after: obj_of(v.get("after").unwrap_or(&Value::Null))?,
        })
    }
}

pub struct Ctx<'a> {
    pub conn: &'a Connection,
    pub today: NaiveDate,
    pub now: String,
    pub actor: Actor,
    pub cfg: &'a EngineConfig,
    changes: RefCell<Vec<Change>>,
    warnings: RefCell<Vec<Value>>,
    undo_of: Cell<Option<i64>>,
    /// Marks the result so the CLI can use a specific exit code.
    exit_code: Cell<i32>,
}

pub fn sv(s: &str) -> SqlValue {
    SqlValue::Text(s.to_string())
}
pub fn iv(i: i64) -> SqlValue {
    SqlValue::Integer(i)
}
pub fn osv(s: Option<&str>) -> SqlValue {
    s.map_or(SqlValue::Null, sv)
}
pub fn oiv(i: Option<i64>) -> SqlValue {
    i.map_or(SqlValue::Null, SqlValue::Integer)
}

impl<'a> Ctx<'a> {
    pub fn new(
        conn: &'a Connection,
        cfg: &'a EngineConfig,
        today: NaiveDate,
        now: String,
        actor: Actor,
    ) -> Self {
        Ctx {
            conn,
            today,
            now,
            actor,
            cfg,
            changes: RefCell::new(Vec::new()),
            warnings: RefCell::new(Vec::new()),
            undo_of: Cell::new(None),
            exit_code: Cell::new(0),
        }
    }

    pub fn insert(&self, table: &str, cols: &[(&str, SqlValue)]) -> Result<i64> {
        check_table(table)?;
        let names = cols.iter().map(|c| c.0).collect::<Vec<_>>().join(",");
        let ph = (1..=cols.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        self.conn
            .execute(
                &format!("INSERT INTO {table} ({names}) VALUES ({ph})"),
                params_from_iter(cols.iter().map(|c| &c.1)),
            )
            .db()?;
        let id = self.conn.last_insert_rowid();
        let after = snapshot(self.conn, table, id)?;
        self.changes.borrow_mut().push(Change {
            table: table.into(),
            id,
            before: None,
            after,
        });
        Ok(id)
    }

    /// Insert a row with an explicit id (used by undo to restore deleted rows).
    pub fn insert_with_id(&self, table: &str, id: i64, cols: &[(&str, SqlValue)]) -> Result<()> {
        let mut all: Vec<(&str, SqlValue)> = vec![("id", iv(id))];
        all.extend(cols.iter().cloned());
        let got = self.insert(table, &all)?;
        debug_assert_eq!(got, id);
        Ok(())
    }

    pub fn update(&self, table: &str, id: i64, cols: &[(&str, SqlValue)]) -> Result<()> {
        check_table(table)?;
        if cols.is_empty() {
            return Ok(());
        }
        let before = snapshot(self.conn, table, id)?.ok_or_else(|| {
            Error::not_found("row_not_found", format!("{table} #{id} does not exist"))
        })?;
        let sets = cols
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{}=?{}", c.0, i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("UPDATE {table} SET {sets} WHERE id=?{}", cols.len() + 1);
        let mut vals: Vec<&SqlValue> = cols.iter().map(|c| &c.1).collect();
        let idv = SqlValue::Integer(id);
        vals.push(&idv);
        self.conn.execute(&sql, params_from_iter(vals)).db()?;
        let after = snapshot(self.conn, table, id)?;
        if after.as_ref() != Some(&before) {
            self.changes.borrow_mut().push(Change {
                table: table.into(),
                id,
                before: Some(before),
                after,
            });
        }
        Ok(())
    }

    pub fn delete(&self, table: &str, id: i64) -> Result<()> {
        check_table(table)?;
        let before = snapshot(self.conn, table, id)?.ok_or_else(|| {
            Error::not_found("row_not_found", format!("{table} #{id} does not exist"))
        })?;
        self.conn
            .execute(&format!("DELETE FROM {table} WHERE id=?1"), [id])
            .db()?;
        self.changes.borrow_mut().push(Change {
            table: table.into(),
            id,
            before: Some(before),
            after: None,
        });
        Ok(())
    }

    pub fn take_changes(&self) -> Vec<Change> {
        std::mem::take(&mut *self.changes.borrow_mut())
    }
    pub fn changes_snapshot(&self) -> Vec<Change> {
        self.changes.borrow().clone()
    }
    pub fn warn(&self, code: &str, message: impl Into<String>, extra: Value) {
        let mut w = json!({ "code": code, "message": message.into() });
        if let (Some(o), Value::Object(e)) = (w.as_object_mut(), extra) {
            for (k, v) in e {
                o.insert(k, v);
            }
        }
        self.warnings.borrow_mut().push(w);
    }
    pub fn take_warnings(&self) -> Vec<Value> {
        std::mem::take(&mut *self.warnings.borrow_mut())
    }
    pub fn set_undo_of(&self, id: i64) {
        self.undo_of.set(Some(id));
    }
    pub fn undo_of(&self) -> Option<i64> {
        self.undo_of.get()
    }
    pub fn set_exit_code(&self, c: i32) {
        self.exit_code.set(c);
    }
    pub fn exit_code(&self) -> i32 {
        self.exit_code.get()
    }

    // ---- reads -----------------------------------------------------------

    pub fn one_i64(&self, sql: &str, params: &[SqlValue]) -> Result<i64> {
        self.conn
            .query_row(sql, params_from_iter(params.iter()), |r| r.get::<_, i64>(0))
            .db()
    }

    pub fn opt_i64(&self, sql: &str, params: &[SqlValue]) -> Result<Option<i64>> {
        self.conn
            .query_row(sql, params_from_iter(params.iter()), |r| {
                r.get::<_, Option<i64>>(0)
            })
            .optional()
            .db()
            .map(|o| o.flatten())
    }

    pub fn opt_string(&self, sql: &str, params: &[SqlValue]) -> Result<Option<String>> {
        self.conn
            .query_row(sql, params_from_iter(params.iter()), |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()
            .db()
            .map(|o| o.flatten())
    }

    pub fn rows(&self, sql: &str, params: &[SqlValue]) -> Result<Vec<Map<String, Value>>> {
        query_json(self.conn, sql, params)
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        self.opt_string("SELECT value FROM settings WHERE key=?1", &[sv(key)])
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        match self.opt_i64("SELECT id FROM settings WHERE key=?1", &[sv(key)])? {
            Some(id) => self.update("settings", id, &[("value", sv(value))]),
            None => self
                .insert("settings", &[("key", sv(key)), ("value", sv(value))])
                .map(|_| ()),
        }
    }

    pub fn base_currency(&self) -> Result<String> {
        self.setting("base_currency")?.ok_or_else(|| {
            Error::not_found("not_initialized", "database is not initialised")
                .with_hint("run `pext init`")
        })
    }

    pub fn scope_check(&self, account_id: i64) -> Result<()> {
        match &self.actor.accounts {
            Some(list) if !list.contains(&account_id) => Err(Error::auth(
                "scope_denied",
                "this token is not allowed to access that account",
            )),
            _ => Ok(()),
        }
    }
}
