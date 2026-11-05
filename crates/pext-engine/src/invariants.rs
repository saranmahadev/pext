//! Ledger invariants. These run inside every mutating transaction just before
//! commit, so a buggy handler can never persist an inconsistent ledger.

use crate::ctx::{Change, DbResult};
use pext_core::{Error, Result};
use rusqlite::{Connection, OptionalExtension};
use std::collections::BTreeSet;

pub fn verify(conn: &Connection, changes: &[Change]) -> Result<()> {
    let mut ids = BTreeSet::new();
    for c in changes {
        match c.table.as_str() {
            "transactions" => {
                ids.insert(c.id);
            }
            "splits" => {
                for m in [&c.before, &c.after].into_iter().flatten() {
                    if let Some(t) = m.get("txn_id").and_then(|v| v.as_i64()) {
                        ids.insert(t);
                    }
                }
            }
            _ => {}
        }
    }
    for id in ids {
        check_txn(conn, id)?;
    }
    Ok(())
}

fn violation(id: i64, msg: impl Into<String>) -> Error {
    Error::integrity(
        "invariant_violation",
        format!("transaction #{id}: {}", msg.into()),
    )
    .with_hint("the change was rolled back; this is a bug, please report it")
}

pub fn check_txn(conn: &Connection, id: i64) -> Result<()> {
    type Row = (i64, String, i64, String, Option<i64>, bool);
    let row: Option<Row> = conn
        .query_row(
            "SELECT amount_minor, currency, account_id, kind, transfer_id, deleted_at IS NOT NULL
             FROM transactions WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()
        .db()?;
    let Some((amount, currency, account_id, kind, transfer_id, deleted)) = row else {
        return Ok(());
    };
    let (n, sum): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(amount_minor),0) FROM splits WHERE txn_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .db()?;
    if n == 0 {
        return Err(violation(id, "has no splits"));
    }
    if sum != amount {
        return Err(violation(
            id,
            format!("splits sum to {sum} but the amount is {amount}"),
        ));
    }
    let wrong_sign: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM splits WHERE txn_id=?1 AND ((amount_minor>0) != (?2>0))",
            rusqlite::params![id, amount],
            |r| r.get(0),
        )
        .db()?;
    if wrong_sign > 0 {
        return Err(violation(
            id,
            "a split has a different sign than the transaction",
        ));
    }
    let acct_cur: String = conn
        .query_row(
            "SELECT currency FROM accounts WHERE id=?1",
            [account_id],
            |r| r.get(0),
        )
        .db()?;
    if acct_cur != currency {
        return Err(violation(
            id,
            format!("currency {currency} differs from account currency {acct_cur}"),
        ));
    }
    if kind == "transfer" {
        let Some(other) = transfer_id else {
            return Err(violation(id, "transfer leg has no counterpart"));
        };
        let o: Option<(String, Option<i64>, i64, bool)> = conn
            .query_row(
                "SELECT kind, transfer_id, amount_minor, deleted_at IS NOT NULL FROM transactions WHERE id=?1",
                [other],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .db()?;
        let Some((okind, otid, oamt, odel)) = o else {
            return Err(violation(id, "transfer counterpart does not exist"));
        };
        if okind != "transfer" || otid != Some(id) {
            return Err(violation(id, "transfer legs are not linked to each other"));
        }
        if (oamt > 0) == (amount > 0) {
            return Err(violation(id, "transfer legs must have opposite signs"));
        }
        if odel != deleted {
            return Err(violation(
                id,
                "transfer legs must be deleted/restored together",
            ));
        }
    } else if transfer_id.is_some() {
        return Err(violation(
            id,
            "non-transfer transaction has a transfer link",
        ));
    }
    Ok(())
}
