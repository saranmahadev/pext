//! Accounts: creation, balances and statement reconciliation.

use crate::args::Args;
use crate::ctx::{iv, oiv, osv, sv, Ctx, DbResult};
use crate::registry::{money, OpSpec, Param};
use chrono::NaiveDate;
use pext_core::dates::parse_date;
use pext_core::money::{checked_add, decimals_for, validate_currency};
use pext_core::{Error, Result};
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

pub const KINDS: &[&str] = &[
    "checking",
    "savings",
    "credit_card",
    "cash",
    "wallet",
    "brokerage",
    "retirement",
    "loan",
    "mortgage",
    "crypto",
    "property",
    "vehicle",
    "other_asset",
    "other_liability",
];
pub const LIABILITY_KINDS: &[&str] = &["credit_card", "loan", "mortgage", "other_liability"];
pub const LIQUID_KINDS: &[&str] = &["checking", "savings", "cash", "wallet"];

#[derive(Clone, Debug)]
pub struct AccountRow {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub currency: String,
    pub institution: Option<String>,
    pub group_name: Option<String>,
    pub opening_minor: i64,
    pub opened_on: String,
    pub closed_on: Option<String>,
    pub archived: bool,
    pub credit_limit_minor: Option<i64>,
}

impl AccountRow {
    pub fn decimals(&self) -> u32 {
        decimals_for(&self.currency)
    }
    pub fn is_liability(&self) -> bool {
        LIABILITY_KINDS.contains(&self.kind.as_str())
    }
}

const SELECT: &str = "SELECT id, name, kind, currency, institution, group_name, opening_minor, opened_on, closed_on, archived, credit_limit_minor FROM accounts";

fn row_to_account(r: &rusqlite::Row<'_>) -> rusqlite::Result<AccountRow> {
    Ok(AccountRow {
        id: r.get(0)?,
        name: r.get(1)?,
        kind: r.get(2)?,
        currency: r.get(3)?,
        institution: r.get(4)?,
        group_name: r.get(5)?,
        opening_minor: r.get(6)?,
        opened_on: r.get(7)?,
        closed_on: r.get(8)?,
        archived: r.get::<_, i64>(9)? == 1,
        credit_limit_minor: r.get(10)?,
    })
}

pub fn load_account(ctx: &Ctx, id: i64) -> Result<AccountRow> {
    ctx.conn
        .query_row(&format!("{SELECT} WHERE id=?1"), [id], row_to_account)
        .optional()
        .db()?
        .ok_or_else(|| Error::not_found("account_not_found", format!("no account #{id}")))
}

/// Resolve an account by id or (case-insensitive) name, enforcing token scope.
pub fn resolve_account(ctx: &Ctx, r: &str) -> Result<AccountRow> {
    let by_id: Option<AccountRow> = match r.trim().parse::<i64>() {
        Ok(id) => ctx
            .conn
            .query_row(&format!("{SELECT} WHERE id=?1"), [id], row_to_account)
            .optional()
            .db()?,
        Err(_) => None,
    };
    let acct = match by_id {
        Some(a) => a,
        None => ctx
            .conn
            .query_row(
                &format!("{SELECT} WHERE name=?1"),
                [r.trim()],
                row_to_account,
            )
            .optional()
            .db()?
            .ok_or_else(|| {
                Error::not_found("account_not_found", format!("no account '{r}'"))
                    .with_hint("run `pext acct list`")
            })?,
    };
    ctx.scope_check(acct.id)?;
    Ok(acct)
}

/// Ledger balance at the end of `as_of` (opening balance + transactions).
pub fn balance_at(
    ctx: &Ctx,
    acct: &AccountRow,
    as_of: NaiveDate,
    cleared_only: bool,
) -> Result<i64> {
    let sql = if cleared_only {
        "SELECT COALESCE(SUM(amount_minor),0) FROM transactions WHERE account_id=?1 AND deleted_at IS NULL AND date<=?2 AND status IN ('cleared','reconciled')"
    } else {
        "SELECT COALESCE(SUM(amount_minor),0) FROM transactions WHERE account_id=?1 AND deleted_at IS NULL AND date<=?2"
    };
    let sum = ctx.one_i64(
        sql,
        &[iv(acct.id), sv(&as_of.format("%Y-%m-%d").to_string())],
    )?;
    checked_add(acct.opening_minor, sum)
}

pub fn account_json(ctx: &Ctx, a: &AccountRow, as_of: NaiveDate) -> Result<Value> {
    let bal = balance_at(ctx, a, as_of, false)?;
    Ok(json!({
        "id": a.id, "name": a.name, "kind": a.kind, "currency": a.currency,
        "institution": a.institution, "group": a.group_name,
        "opening_minor": a.opening_minor, "opened_on": a.opened_on, "closed_on": a.closed_on,
        "archived": a.archived, "credit_limit_minor": a.credit_limit_minor,
        "is_liability": a.is_liability(),
        "balance_minor": bal,
    }))
}

fn valid_name(s: &str) -> Result<String> {
    let t = s.trim();
    if t.is_empty() || t.chars().count() > 64 || t.chars().any(|c| c.is_control()) {
        return Err(Error::validation(
            "invalid_name",
            "name must be 1-64 characters without control characters",
        )
        .with_field("name"));
    }
    Ok(t.to_string())
}

fn add(ctx: &Ctx, a: &Args) -> Result<Value> {
    let name = valid_name(&a.str("name")?)?;
    let kind = a.str("kind")?;
    let currency = match a.opt_str("currency") {
        Some(c) => validate_currency(&c)?,
        None => ctx.base_currency()?,
    };
    let dec = decimals_for(&currency);
    let opening = a.money("opening", dec)?.unwrap_or(0);
    let limit = a.money("credit_limit", dec)?;
    if let Some(l) = limit {
        if l < 0 {
            return Err(Error::validation(
                "invalid_credit_limit",
                "credit limit cannot be negative",
            )
            .with_field("credit_limit"));
        }
    }
    let opened = a.opt_date("opened_on")?.unwrap_or(ctx.today);
    let id = ctx.insert(
        "accounts",
        &[
            ("name", sv(&name)),
            ("kind", sv(&kind)),
            ("currency", sv(&currency)),
            ("institution", osv(a.opt_str("institution").as_deref())),
            ("group_name", osv(a.opt_str("group").as_deref())),
            ("opening_minor", iv(opening)),
            ("opened_on", sv(&opened.format("%Y-%m-%d").to_string())),
            ("credit_limit_minor", oiv(limit)),
            ("created_at", sv(&ctx.now)),
        ],
    )?;
    account_json(ctx, &load_account(ctx, id)?, ctx.today)
}

fn list(ctx: &Ctx, a: &Args) -> Result<Value> {
    let as_of = a.opt_date("as_of")?.unwrap_or(ctx.today);
    let mut sql = format!("{SELECT} WHERE (archived=0 OR ?1=1)");
    if a.opt_str("kind").is_some() {
        sql.push_str(" AND kind=?2");
    }
    sql.push_str(" ORDER BY archived, name");
    let mut stmt = ctx.conn.prepare(&sql).db()?;
    let inc = a.flag("include_archived") as i64;
    let rows: Vec<AccountRow> = if let Some(k) = a.opt_str("kind") {
        stmt.query_map(rusqlite::params![inc, k], row_to_account)
            .db()?
            .collect::<rusqlite::Result<_>>()
            .db()?
    } else {
        stmt.query_map(rusqlite::params![inc], row_to_account)
            .db()?
            .collect::<rusqlite::Result<_>>()
            .db()?
    };
    let mut out = Vec::new();
    for r in rows {
        if let Some(scope) = &ctx.actor.accounts {
            if !scope.contains(&r.id) {
                continue;
            }
        }
        out.push(account_json(ctx, &r, as_of)?);
    }
    Ok(Value::Array(out))
}

fn get(ctx: &Ctx, a: &Args) -> Result<Value> {
    let acct = resolve_account(ctx, &a.str("account")?)?;
    let mut v = account_json(ctx, &acct, ctx.today)?;
    let cleared = balance_at(ctx, &acct, ctx.today, true)?;
    let n = ctx.one_i64(
        "SELECT COUNT(*) FROM transactions WHERE account_id=?1 AND deleted_at IS NULL",
        &[iv(acct.id)],
    )?;
    let last = ctx.opt_string(
        "SELECT MAX(date) FROM transactions WHERE account_id=?1 AND deleted_at IS NULL",
        &[iv(acct.id)],
    )?;
    v["cleared_minor"] = json!(cleared);
    v["txn_count"] = json!(n);
    v["last_txn_date"] = json!(last);
    Ok(v)
}

fn edit(ctx: &Ctx, a: &Args) -> Result<Value> {
    let acct = resolve_account(ctx, &a.str("account")?)?;
    let dec = acct.decimals();
    let mut cols: Vec<(&str, rusqlite::types::Value)> = vec![];
    if let Some(n) = a.opt_str("new_name") {
        cols.push(("name", sv(&valid_name(&n)?)));
    }
    if let Some(i) = a.opt_str("institution") {
        cols.push(("institution", sv(&i)));
    }
    if let Some(g) = a.opt_str("group") {
        cols.push(("group_name", sv(&g)));
    }
    if let Some(l) = a.money("credit_limit", dec)? {
        if l < 0 {
            return Err(Error::validation(
                "invalid_credit_limit",
                "credit limit cannot be negative",
            )
            .with_field("credit_limit"));
        }
        cols.push(("credit_limit_minor", iv(l)));
    }
    if let Some(d) = a.opt_date("closed_on")? {
        cols.push(("closed_on", sv(&d.format("%Y-%m-%d").to_string())));
    }
    if cols.is_empty() {
        return Err(Error::usage("nothing_to_change", "no changes given")
            .with_hint("pass --new-name, --institution, --group, --credit-limit or --closed-on"));
    }
    ctx.update("accounts", acct.id, &cols)?;
    account_json(ctx, &load_account(ctx, acct.id)?, ctx.today)
}

fn archive(ctx: &Ctx, a: &Args) -> Result<Value> {
    let acct = resolve_account(ctx, &a.str("account")?)?;
    let un = a.flag("unarchive");
    if !un {
        let bal = balance_at(ctx, &acct, ctx.today, false)?;
        if bal != 0 {
            ctx.warn(
                "nonzero_balance",
                format!("archiving an account with a non-zero balance ({bal} minor units)"),
                json!({ "balance_minor": bal }),
            );
        }
    }
    ctx.update("accounts", acct.id, &[("archived", iv(!un as i64))])?;
    account_json(ctx, &load_account(ctx, acct.id)?, ctx.today)
}

fn balance(ctx: &Ctx, a: &Args) -> Result<Value> {
    let acct = resolve_account(ctx, &a.str("account")?)?;
    let as_of = a.opt_date("as_of")?.unwrap_or(ctx.today);
    let all = balance_at(ctx, &acct, as_of, false)?;
    let cleared = balance_at(ctx, &acct, as_of, true)?;
    Ok(json!({
        "account": acct.name, "currency": acct.currency, "as_of": as_of.format("%Y-%m-%d").to_string(),
        "balance_minor": all, "cleared_minor": cleared, "pending_minor": all - cleared,
    }))
}

fn reconcile(ctx: &Ctx, a: &Args) -> Result<Value> {
    let acct = resolve_account(ctx, &a.str("account")?)?;
    let date = a.date("statement_date")?;
    let stmt_bal = a.req_money("statement_balance", acct.decimals())?;
    let cleared = balance_at(ctx, &acct, date, true)?;
    let diff = stmt_bal - cleared;
    let uncleared = ctx.rows(
        "SELECT id, date, amount_minor, currency, payee, status FROM transactions
         WHERE account_id=?1 AND deleted_at IS NULL AND date<=?2 AND status='pending' ORDER BY date, id",
        &[iv(acct.id), sv(&date.format("%Y-%m-%d").to_string())],
    )?;
    let mut marked = 0;
    if a.flag("mark_reconciled") {
        if diff != 0 {
            return Err(Error::conflict(
                "reconcile_mismatch",
                format!("ledger cleared balance differs from the statement by {diff} minor units"),
            )
            .with_hint("find the missing or wrong transactions first; nothing was marked"));
        }
        let ids: Vec<i64> = {
            let mut st = ctx
                .conn
                .prepare("SELECT id FROM transactions WHERE account_id=?1 AND deleted_at IS NULL AND date<=?2 AND status='cleared'")
                .db()?;
            let rows = st
                .query_map(
                    rusqlite::params![acct.id, date.format("%Y-%m-%d").to_string()],
                    |r| r.get(0),
                )
                .db()?;
            rows.collect::<rusqlite::Result<_>>().db()?
        };
        for id in &ids {
            ctx.update("transactions", *id, &[("status", sv("reconciled"))])?;
        }
        marked = ids.len();
    }
    Ok(json!({
        "account": acct.name, "currency": acct.currency, "statement_date": date.format("%Y-%m-%d").to_string(),
        "statement_minor": stmt_bal, "ledger_cleared_minor": cleared, "difference_minor": diff,
        "balanced": diff == 0, "pending_transactions": uncleared, "marked_reconciled": marked,
    }))
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new("acct.add", "Create an account", add)
            .write()
            .p(Param::s("name", "unique account name").req())
            .p(Param::s("kind", "account kind").req().choices(KINDS))
            .p(Param::s(
                "currency",
                "ISO currency (default: base currency)",
            ))
            .p(Param::s("institution", "bank or broker name"))
            .p(Param::s("group", "grouping label, e.g. joint"))
            .ps(money(
                "opening",
                "opening_minor",
                "opening balance (negative for amounts owed)",
            ))
            .ps(money(
                "credit_limit",
                "credit_limit_minor",
                "credit limit (credit cards)",
            ))
            .p(Param::date("opened_on", "opening date (default: today)"))
            .ex("pext acct add --name hdfc-savings --kind savings --opening 25000.00")
            .ex("pext acct add --name amex --kind credit_card --credit-limit 200000"),
        OpSpec::new("acct.list", "List accounts with balances", list)
            .scoped()
            .p(Param::b("include_archived", "include archived accounts"))
            .p(Param::s("kind", "only this kind").choices(KINDS))
            .p(Param::date("as_of", "balance date (default: today)")),
        OpSpec::new("acct.get", "Show one account", get)
            .scoped()
            .p(Param::s("account", "account id or name").req().pos()),
        OpSpec::new("acct.edit", "Edit an account", edit)
            .write()
            .p(Param::s("account", "account id or name").req().pos())
            .p(Param::s("new_name", "rename"))
            .p(Param::s("institution", "institution"))
            .p(Param::s("group", "group label"))
            .ps(money("credit_limit", "credit_limit_minor", "credit limit"))
            .p(Param::date("closed_on", "closing date")),
        OpSpec::new("acct.archive", "Archive (or restore) an account", archive)
            .write()
            .p(Param::s("account", "account id or name").req().pos())
            .p(Param::b("unarchive", "restore instead of archive")),
        OpSpec::new(
            "acct.balance",
            "Balance as of a date (total, cleared, pending)",
            balance,
        )
        .scoped()
        .p(Param::s("account", "account id or name").req().pos())
        .p(Param::date("as_of", "date (default: today)")),
        OpSpec::new(
            "acct.reconcile",
            "Reconcile an account against a statement",
            reconcile,
        )
        .write()
        .p(Param::s("account", "account id or name").req().pos())
        .p(Param::date("statement_date", "statement closing date").req())
        .ps(money(
            "statement_balance",
            "statement_balance_minor",
            "closing balance on the statement",
        ))
        .p(Param::b(
            "mark_reconciled",
            "mark cleared transactions up to the date as reconciled (only if balanced)",
        ))
        .ex("pext acct reconcile hdfc --statement-date 2026-10-31 --statement-balance 41200.55"),
    ]
}

#[allow(dead_code)]
pub fn parse_stored_date(s: &str) -> Result<NaiveDate> {
    parse_date(s)
}
