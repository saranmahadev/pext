//! Loading ledger lines (one per split) for queries, budgets and reports.

use crate::ctx::DbResult;
use chrono::NaiveDate;
use pext_core::dates::parse_date;
use pext_core::query::Line;
use pext_core::{Error, Result};
use rusqlite::Connection;
use std::collections::HashMap;

pub fn fetch_lines(
    conn: &Connection,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    include_deleted: bool,
) -> Result<Vec<Line>> {
    let f = from.map(|d| d.format("%Y-%m-%d").to_string());
    let t = to.map(|d| d.format("%Y-%m-%d").to_string());
    let mut tags: HashMap<i64, Vec<String>> = HashMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT g.txn_id, g.tag FROM txn_tags g JOIN transactions t ON t.id = g.txn_id
                 WHERE (?1 IS NULL OR t.date >= ?1) AND (?2 IS NULL OR t.date <= ?2)
                 ORDER BY g.txn_id, g.tag",
            )
            .db()?;
        let mut rows = stmt.query(rusqlite::params![f, t]).db()?;
        while let Some(r) = rows.next().db()? {
            tags.entry(r.get(0).db()?).or_default().push(r.get(1).db()?);
        }
    }
    let mut stmt = conn
        .prepare(
            "SELECT t.id, s.id, t.date, t.account_id, a.name, t.currency, t.payee, t.memo, t.status, t.kind,
                    t.amount_minor, s.amount_minor, c.path
             FROM transactions t
             JOIN accounts a ON a.id = t.account_id
             JOIN splits s ON s.txn_id = t.id
             LEFT JOIN categories c ON c.id = s.category_id
             WHERE (?3 = 1 OR t.deleted_at IS NULL)
               AND (?1 IS NULL OR t.date >= ?1) AND (?2 IS NULL OR t.date <= ?2)
             ORDER BY t.date, t.id, s.id",
        )
        .db()?;
    let mut rows = stmt
        .query(rusqlite::params![f, t, include_deleted as i64])
        .db()?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().db()? {
        let txn_id: i64 = r.get(0).db()?;
        let date_s: String = r.get(2).db()?;
        let date = parse_date(&date_s).map_err(|_| {
            Error::integrity(
                "corrupt_date",
                format!("transaction #{txn_id} has invalid date '{date_s}'"),
            )
        })?;
        out.push(Line {
            txn_id,
            split_id: r.get(1).db()?,
            date,
            account_id: r.get(3).db()?,
            account: r.get(4).db()?,
            currency: r.get(5).db()?,
            payee: r.get(6).db()?,
            memo: r.get(7).db()?,
            status: r.get(8).db()?,
            kind: r.get(9).db()?,
            txn_amount_minor: r.get(10).db()?,
            amount_minor: r.get(11).db()?,
            category: r.get(12).db()?,
            tags: tags.get(&txn_id).cloned().unwrap_or_default(),
        });
    }
    Ok(out)
}
