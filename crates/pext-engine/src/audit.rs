//! Hash-chained, append-only audit log plus row-level undo.

use crate::args::Args;
use crate::ctx::{iv, json_to_sql, query_json, snapshot, Change, Ctx, DbResult};
use crate::registry::{OpSpec, Param};
use pext_core::{Error, Result};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

pub fn sha256_hex(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Deterministic JSON: object keys sorted recursively.
pub fn canonical_json(v: &Value) -> String {
    fn canon(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for k in keys {
                    out.insert(k.clone(), canon(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(canon).collect()),
            other => other.clone(),
        }
    }
    canon(v).to_string()
}

fn entry_hash(prev: &str, ts: &str, actor: &str, op: &str, args: &str, changes: &str) -> String {
    sha256_hex(&format!("{prev}\n{ts}\n{actor}\n{op}\n{args}\n{changes}"))
}

pub fn head_hash(conn: &Connection) -> Result<String> {
    Ok(conn
        .query_row("SELECT hash FROM audit ORDER BY id DESC LIMIT 1", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()
        .db()?
        .unwrap_or_else(|| GENESIS.to_string()))
}

pub fn append(
    conn: &Connection,
    ts: &str,
    actor: &str,
    op: &str,
    args: &str,
    changes: &str,
) -> Result<i64> {
    let prev = head_hash(conn)?;
    let hash = entry_hash(&prev, ts, actor, op, args, changes);
    conn.execute(
        "INSERT INTO audit(ts, actor, op, args, changes, prev_hash, hash) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![ts, actor, op, args, changes, prev, hash],
    )
    .db()?;
    Ok(conn.last_insert_rowid())
}

pub struct VerifyReport {
    pub ok: bool,
    pub entries: i64,
    pub head: String,
    pub first_bad: Option<i64>,
    pub reason: Option<String>,
}

pub fn verify_chain(conn: &Connection) -> Result<VerifyReport> {
    let mut stmt = conn
        .prepare("SELECT id, ts, actor, op, args, changes, prev_hash, hash FROM audit ORDER BY id")
        .db()?;
    let mut rows = stmt.query([]).db()?;
    let mut prev = GENESIS.to_string();
    let mut n = 0;
    while let Some(r) = rows.next().db()? {
        let id: i64 = r.get(0).db()?;
        let (ts, actor, op, args, changes, stored_prev, stored): (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
        ) = (
            r.get(1).db()?,
            r.get(2).db()?,
            r.get(3).db()?,
            r.get(4).db()?,
            r.get(5).db()?,
            r.get(6).db()?,
            r.get(7).db()?,
        );
        n += 1;
        if stored_prev != prev {
            return Ok(VerifyReport {
                ok: false,
                entries: n,
                head: prev,
                first_bad: Some(id),
                reason: Some("chain link broken".into()),
            });
        }
        let h = entry_hash(&stored_prev, &ts, &actor, &op, &args, &changes);
        if h != stored {
            return Ok(VerifyReport {
                ok: false,
                entries: n,
                head: prev,
                first_bad: Some(id),
                reason: Some("entry content does not match its hash".into()),
            });
        }
        prev = stored;
    }
    Ok(VerifyReport {
        ok: true,
        entries: n,
        head: prev,
        first_bad: None,
        reason: None,
    })
}

fn list(ctx: &Ctx, a: &Args) -> Result<Value> {
    let mut sql = String::from(
        "SELECT id, ts, actor, op, json_array_length(changes) AS changes, undone_by FROM audit WHERE 1=1",
    );
    let mut params: Vec<SqlValue> = vec![];
    if let Some(op) = a.opt_str("op") {
        params.push(SqlValue::Text(op));
        sql.push_str(&format!(" AND op = ?{}", params.len()));
    }
    if let Some(actor) = a.opt_str("actor") {
        params.push(SqlValue::Text(actor));
        sql.push_str(&format!(" AND actor = ?{}", params.len()));
    }
    sql.push_str(" ORDER BY id DESC");
    params.push(iv(a.opt_i64("limit")?.unwrap_or(50).clamp(1, 10_000)));
    sql.push_str(&format!(" LIMIT ?{}", params.len()));
    Ok(Value::Array(
        ctx.rows(&sql, &params)?
            .into_iter()
            .map(Value::Object)
            .collect(),
    ))
}

fn show(ctx: &Ctx, a: &Args) -> Result<Value> {
    let id = a.i64("id")?;
    let rows = query_json(ctx.conn, "SELECT * FROM audit WHERE id=?1", &[iv(id)])?;
    let mut row = rows
        .into_iter()
        .next()
        .ok_or_else(|| Error::not_found("audit_not_found", format!("no audit entry #{id}")))?;
    for k in ["args", "changes"] {
        if let Some(Value::String(s)) = row.get(k).cloned() {
            row.insert(k.into(), serde_json::from_str(&s).unwrap_or(Value::Null));
        }
    }
    Ok(Value::Object(row))
}

fn verify(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let r = verify_chain(ctx.conn)?;
    if !r.ok {
        ctx.set_exit_code(8);
    }
    Ok(
        json!({ "ok": r.ok, "entries": r.entries, "head": r.head, "first_bad_id": r.first_bad, "reason": r.reason }),
    )
}

fn cols_of(m: &Map<String, Value>) -> Result<Vec<(String, SqlValue)>> {
    m.iter()
        .filter(|(k, _)| k.as_str() != "id")
        .map(|(k, v)| Ok((k.clone(), json_to_sql(v)?)))
        .collect()
}

fn undo(ctx: &Ctx, a: &Args) -> Result<Value> {
    let id = match a.opt_i64("id")? {
        Some(i) => i,
        None => ctx
            .opt_i64(
                "SELECT id FROM audit WHERE undone_by IS NULL AND op NOT IN ('undo','init') AND changes != '[]' ORDER BY id DESC LIMIT 1",
                &[],
            )?
            .ok_or_else(|| Error::not_found("nothing_to_undo", "there is nothing to undo"))?,
    };
    let rows = query_json(
        ctx.conn,
        "SELECT op, changes, undone_by FROM audit WHERE id=?1",
        &[iv(id)],
    )?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| Error::not_found("audit_not_found", format!("no audit entry #{id}")))?;
    let op = row["op"].as_str().unwrap_or("").to_string();
    if op == "undo" {
        return Err(
            Error::validation("not_undoable", "undo entries cannot themselves be undone")
                .with_hint("repeat the original operation instead"),
        );
    }
    if op == "init" {
        return Err(
            Error::validation("not_undoable", "database initialisation cannot be undone")
                .with_hint("delete the database file to start over"),
        );
    }
    if !row["undone_by"].is_null() {
        return Err(Error::conflict(
            "already_undone",
            format!("audit entry #{id} was already undone"),
        ));
    }
    let changes_json: Value = serde_json::from_str(row["changes"].as_str().unwrap_or("[]"))
        .map_err(|_| Error::internal("bad_audit_entry", "audit entry has malformed changes"))?;
    let changes: Vec<Change> = changes_json
        .as_array()
        .ok_or_else(|| Error::internal("bad_audit_entry", "audit entry has malformed changes"))?
        .iter()
        .map(Change::from_json)
        .collect::<Result<_>>()?;
    if changes.is_empty() {
        return Err(Error::validation(
            "not_undoable",
            format!("audit entry #{id} made no changes"),
        ));
    }
    for c in changes.iter().rev() {
        let current = snapshot(ctx.conn, &c.table, c.id)?;
        if current != c.after {
            return Err(Error::conflict(
                "undo_conflict",
                format!("{} #{} was changed after audit entry #{id}", c.table, c.id),
            )
            .with_hint("undo the later changes first, or fix the record manually"));
        }
        match (&c.before, &c.after) {
            (None, _) => ctx.delete(&c.table, c.id)?,
            (Some(b), None) => {
                let cols = cols_of(b)?;
                let refs: Vec<(&str, SqlValue)> =
                    cols.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                ctx.insert_with_id(&c.table, c.id, &refs)?;
            }
            (Some(b), Some(_)) => {
                let cols = cols_of(b)?;
                let refs: Vec<(&str, SqlValue)> =
                    cols.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                ctx.update(&c.table, c.id, &refs)?;
            }
        }
    }
    ctx.conn
        .execute("DELETE FROM idempotency WHERE audit_id=?1", [id])
        .db()?;
    ctx.set_undo_of(id);
    Ok(json!({ "undone_audit_id": id, "op": op, "rows_reverted": changes.len() }))
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new("audit.list", "List audit entries (newest first)", list)
            .p(Param::s(
                "op",
                "only entries for this operation, e.g. txn.add",
            ))
            .p(Param::s("actor", "only entries by this actor"))
            .p(Param::i("limit", "maximum entries").def("50")),
        OpSpec::new(
            "audit.show",
            "Show one audit entry with before/after row snapshots",
            show,
        )
        .p(Param::i("id", "audit entry id").req().pos()),
        OpSpec::new("audit.verify", "Verify the audit hash chain", verify),
        OpSpec::new("undo", "Undo an operation (default: the most recent)", undo)
            .write()
            .p(Param::i("id", "audit entry id to undo"))
            .ex("pext undo")
            .ex("pext undo --id 12 --dry-run"),
    ]
}
