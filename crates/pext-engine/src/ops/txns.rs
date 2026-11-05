//! Transactions: creation, listing, editing, splits, transfers, tags.

use crate::args::Args;
use crate::ctx::{iv, oiv, osv, sv, Ctx};
use crate::lines::fetch_lines;
use crate::ops::accounts::{load_account, resolve_account, AccountRow};
use crate::ops::categories::{category_id, normalize_payee};
use crate::ops::rules::{auto, AutoInput};
use crate::registry::{money, OpSpec, Param};
use chrono::NaiveDate;
use pext_core::dates::{add_days, parse_date};
use pext_core::money::{check_amount, decimals_for, parse_amount};
use pext_core::query::{line_matches, Field, Line, Op, Query, Term};
use pext_core::{Error, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

pub const STATUSES: &[&str] = &["pending", "cleared", "reconciled"];
pub const TYPES: &[&str] = &["expense", "income", "transfer", "adjustment"];

pub struct SplitIn {
    pub category_id: Option<i64>,
    pub amount_minor: i64,
    pub note: String,
}

pub struct NewTxn {
    pub account: AccountRow,
    pub date: NaiveDate,
    pub amount_minor: i64,
    pub payee: String,
    pub memo: String,
    pub status: String,
    pub kind: &'static str,
    pub transfer_id: Option<i64>,
    pub external_ref: Option<String>,
    pub fingerprint: Option<String>,
    pub import_batch: Option<i64>,
    pub splits: Vec<SplitIn>,
    pub tags: Vec<String>,
}

pub fn fmt_date(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

pub fn normalize_tag(raw: &str) -> Result<String> {
    let t = raw.trim().to_ascii_lowercase();
    if t.is_empty()
        || t.len() > 32
        || !t
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-:./".contains(&b))
    {
        return Err(
            Error::validation("invalid_tag", format!("'{raw}' is not a valid tag"))
                .with_field("tag")
                .with_hint("tags are 1-32 characters: letters, digits and _ - : . /"),
        );
    }
    Ok(t)
}

pub fn check_date(ctx: &Ctx, d: NaiveDate) -> Result<()> {
    let min = pext_core::dates::min_date();
    let max = add_days(ctx.today, 366)?;
    if d < min || d > max {
        return Err(Error::validation(
            "date_out_of_range",
            format!(
                "date {} is outside {}..{}",
                fmt_date(d),
                fmt_date(min),
                fmt_date(max)
            ),
        )
        .with_field("date")
        .with_hint("check the year; far-future items belong in recurring schedules"));
    }
    Ok(())
}

pub fn create_txn(ctx: &Ctx, n: NewTxn) -> Result<i64> {
    if n.account.archived {
        return Err(Error::conflict(
            "account_archived",
            format!("account '{}' is archived", n.account.name),
        )
        .with_hint("restore it with `pext acct archive --unarchive`"));
    }
    if let Some(c) = &n.account.closed_on {
        if parse_date(c).map(|c| c < n.date).unwrap_or(false) {
            return Err(Error::conflict(
                "account_closed",
                format!("account '{}' closed on {c}", n.account.name),
            ));
        }
    }
    check_date(ctx, n.date)?;
    check_amount(n.amount_minor)?;
    if n.amount_minor == 0 {
        return Err(Error::validation("zero_amount", "amount cannot be zero").with_field("amount"));
    }
    if n.memo.chars().count() > 1000 || n.memo.chars().any(|c| c.is_control() && c != '\n') {
        return Err(
            Error::validation("invalid_memo", "memo must be at most 1000 characters")
                .with_field("memo"),
        );
    }
    let sum: i128 = n.splits.iter().map(|s| s.amount_minor as i128).sum();
    if n.splits.is_empty() || sum != n.amount_minor as i128 {
        return Err(Error::validation(
            "split_sum_mismatch",
            format!("splits sum to {sum} but the amount is {}", n.amount_minor),
        ));
    }
    let id = ctx.insert(
        "transactions",
        &[
            ("account_id", iv(n.account.id)),
            ("date", sv(&fmt_date(n.date))),
            ("amount_minor", iv(n.amount_minor)),
            ("currency", sv(&n.account.currency)),
            ("payee", sv(&n.payee)),
            ("memo", sv(&n.memo)),
            ("status", sv(&n.status)),
            ("kind", sv(n.kind)),
            ("transfer_id", oiv(n.transfer_id)),
            ("external_ref", osv(n.external_ref.as_deref())),
            ("fingerprint", osv(n.fingerprint.as_deref())),
            ("import_batch", oiv(n.import_batch)),
            ("created_at", sv(&ctx.now)),
        ],
    )?;
    for s in &n.splits {
        ctx.insert(
            "splits",
            &[
                ("txn_id", iv(id)),
                ("category_id", oiv(s.category_id)),
                ("amount_minor", iv(s.amount_minor)),
                ("note", sv(&s.note)),
            ],
        )?;
    }
    let mut seen = std::collections::BTreeSet::new();
    for t in &n.tags {
        if seen.insert(t.clone()) {
            ctx.insert("txn_tags", &[("txn_id", iv(id)), ("tag", sv(t))])?;
        }
    }
    Ok(id)
}

/// Parse "category=amount" specs (amounts are magnitudes; the sign of the
/// transaction is applied) and require them to add up to |total|.
pub fn parse_splits(
    ctx: &Ctx,
    specs: &[String],
    total_minor: i64,
    decimals: u32,
) -> Result<Vec<SplitIn>> {
    let sign: i64 = if total_minor < 0 { -1 } else { 1 };
    let mut out = Vec::new();
    let mut sum: i128 = 0;
    for spec in specs {
        let (cat, amt) = spec.split_once('=').ok_or_else(|| {
            Error::validation(
                "invalid_split",
                format!("split '{spec}' must look like category=amount"),
            )
            .with_field("split")
        })?;
        let cat_id = match cat.trim().to_ascii_lowercase().as_str() {
            "none" | "uncategorized" => None,
            c => Some(category_id(ctx, c)?),
        };
        let m = parse_amount(amt, decimals).map_err(|e| e.with_field("split"))?;
        if m <= 0 {
            return Err(Error::validation(
                "invalid_split",
                "split amounts must be positive (the transaction's sign is applied)",
            )
            .with_field("split"));
        }
        sum += m as i128;
        out.push(SplitIn {
            category_id: cat_id,
            amount_minor: m * sign,
            note: String::new(),
        });
    }
    if sum != (total_minor as i128).abs() {
        return Err(Error::validation(
            "split_sum_mismatch",
            format!(
                "splits sum to {sum} but the transaction amount is {} (minor units)",
                total_minor.abs()
            ),
        )
        .with_field("split"));
    }
    Ok(out)
}

pub fn txn_json(ctx: &Ctx, id: i64) -> Result<Value> {
    let rows = ctx.rows(
        "SELECT t.id, t.date, t.account_id, a.name AS account, t.amount_minor, t.currency, t.payee, t.memo,
                t.status, t.kind, t.transfer_id, t.external_ref, t.import_batch, t.deleted_at
         FROM transactions t JOIN accounts a ON a.id=t.account_id WHERE t.id=?1",
        &[iv(id)],
    )?;
    let mut m = rows.into_iter().next().ok_or_else(|| {
        Error::not_found("txn_not_found", format!("no transaction #{id}"))
            .with_hint("run `pext txn list`")
    })?;
    ctx.scope_check(m["account_id"].as_i64().unwrap_or(0))?;
    let splits = ctx.rows(
        "SELECT s.id, c.path AS category, s.amount_minor, s.note FROM splits s
         LEFT JOIN categories c ON c.id=s.category_id WHERE s.txn_id=?1 ORDER BY s.id",
        &[iv(id)],
    )?;
    let tags: Vec<Value> = ctx
        .rows(
            "SELECT tag FROM txn_tags WHERE txn_id=?1 ORDER BY tag",
            &[iv(id)],
        )?
        .into_iter()
        .map(|r| r["tag"].clone())
        .collect();
    let category = if splits.len() == 1 {
        splits[0]["category"].clone()
    } else {
        json!("(split)")
    };
    let deleted = !m["deleted_at"].is_null();
    m.insert("category".into(), category);
    m.insert("deleted".into(), json!(deleted));
    m.insert("tags".into(), Value::Array(tags));
    m.insert(
        "splits".into(),
        Value::Array(splits.into_iter().map(Value::Object).collect()),
    );
    Ok(Value::Object(m))
}

fn load_txn_row(ctx: &Ctx, id: i64) -> Result<Map<String, Value>> {
    let rows = ctx.rows("SELECT * FROM transactions WHERE id=?1", &[iv(id)])?;
    let row = rows.into_iter().next().ok_or_else(|| {
        Error::not_found("txn_not_found", format!("no transaction #{id}"))
            .with_hint("run `pext txn list`")
    })?;
    ctx.scope_check(row["account_id"].as_i64().unwrap_or(0))?;
    Ok(row)
}

fn warn_duplicates(
    ctx: &Ctx,
    account_id: i64,
    date: NaiveDate,
    amount: i64,
    payee: &str,
) -> Result<()> {
    let dups = ctx.rows(
        "SELECT id FROM transactions WHERE account_id=?1 AND date=?2 AND amount_minor=?3 AND payee=?4 COLLATE NOCASE AND deleted_at IS NULL LIMIT 3",
        &[iv(account_id), sv(&fmt_date(date)), iv(amount), sv(payee)],
    )?;
    if !dups.is_empty() {
        let ids: Vec<Value> = dups.into_iter().map(|r| r["id"].clone()).collect();
        ctx.warn(
            "possible_duplicate",
            "an identical transaction already exists on that date",
            json!({ "existing_ids": ids }),
        );
    }
    Ok(())
}

fn add(ctx: &Ctx, a: &Args) -> Result<Value> {
    let acct = resolve_account(ctx, &a.str("account")?)?;
    let dec = acct.decimals();
    let amount = a.req_money("amount", dec)?;
    let date = a.opt_date("date")?.unwrap_or(ctx.today);
    let mut payee = normalize_payee(ctx, &a.opt_str("payee").unwrap_or_default())?;
    let memo = a.opt_str("memo").unwrap_or_default();
    let status = a.opt_str("status").unwrap_or_else(|| "cleared".into());
    let mut tags: Vec<String> = a
        .list("tag")
        .iter()
        .map(|t| normalize_tag(t))
        .collect::<Result<_>>()?;
    let specs = a.list("split");
    let category = a.opt_str("category");
    if category.is_some() && !specs.is_empty() {
        return Err(Error::usage(
            "conflicting_arguments",
            "use either --category or --split, not both",
        ));
    }
    let splits = if !specs.is_empty() {
        parse_splits(ctx, &specs, amount, dec)?
    } else if let Some(c) = category {
        vec![SplitIn {
            category_id: Some(category_id(ctx, &c)?),
            amount_minor: amount,
            note: String::new(),
        }]
    } else {
        let r = auto(
            ctx,
            &AutoInput {
                account_id: acct.id,
                payee: &payee,
                memo: &memo,
                amount_minor: amount,
                date,
            },
        )?;
        if let Some(p) = r.payee {
            payee = p;
        }
        tags.extend(r.tags);
        vec![SplitIn {
            category_id: r.category_id,
            amount_minor: amount,
            note: String::new(),
        }]
    };
    warn_duplicates(ctx, acct.id, date, amount, &payee)?;
    let id = create_txn(
        ctx,
        NewTxn {
            account: acct,
            date,
            amount_minor: amount,
            payee,
            memo,
            status,
            kind: "standard",
            transfer_id: None,
            external_ref: a.opt_str("external_ref"),
            fingerprint: None,
            import_batch: None,
            splits,
            tags,
        },
    )?;
    txn_json(ctx, id)
}

pub fn filter_params() -> Vec<Param> {
    vec![
        Param::s("account", "only these accounts (id or name)").multi(),
        Param::s(
            "category",
            "only these categories (includes sub-categories; 'none' = uncategorised)",
        )
        .multi(),
        Param::s("payee", "payee contains (case-insensitive)").multi(),
        Param::s("memo", "memo contains").multi(),
        Param::s("tag", "has any of these tags").multi(),
        Param::s("status", "pending, cleared or reconciled")
            .multi()
            .choices(STATUSES),
        Param::s("type", "expense, income, transfer or adjustment")
            .multi()
            .choices(TYPES),
        Param::date("from", "on or after this date"),
        Param::date("to", "on or before this date"),
        Param::amount("min", "absolute amount at least (major units)"),
        Param::amount("max", "absolute amount at most (major units)"),
        Param::b("uncategorized", "only uncategorised transactions"),
    ]
}

pub fn filter_query(ctx: &Ctx, a: &Args) -> Result<Query> {
    let mut q = Query {
        include_transfers: true,
        ..Default::default()
    };
    let mut terms: Vec<Term> = vec![];
    let multi = |f: Field, op: Op, k: &str, out: &mut Vec<Term>| {
        let v = a.list(k);
        if !v.is_empty() {
            out.push(Term::new(f, op, v));
        }
    };
    multi(Field::Account, Op::Eq, "account", &mut terms);
    multi(Field::Category, Op::Eq, "category", &mut terms);
    multi(Field::Payee, Op::Contains, "payee", &mut terms);
    multi(Field::Memo, Op::Contains, "memo", &mut terms);
    multi(Field::Tag, Op::Eq, "tag", &mut terms);
    multi(Field::Status, Op::Eq, "status", &mut terms);
    multi(Field::Type, Op::Eq, "type", &mut terms);
    if let Some(d) = a.opt_date("from")? {
        terms.push(Term::new(Field::Date, Op::Ge, vec![fmt_date(d)]));
    }
    if let Some(d) = a.opt_date("to")? {
        terms.push(Term::new(Field::Date, Op::Le, vec![fmt_date(d)]));
    }
    if let Some(v) = a.opt_str("min") {
        terms.push(Term::new(Field::Amount, Op::Ge, vec![v]));
    }
    if let Some(v) = a.opt_str("max") {
        terms.push(Term::new(Field::Amount, Op::Le, vec![v]));
    }
    if a.flag("uncategorized") {
        terms.push(Term::new(Field::Category, Op::Eq, vec!["none".into()]));
    }
    if let Some(qs) = a.opt_str("query") {
        q = crate::ops::query::parse_into(ctx, &qs, q)?;
    }
    for t in terms {
        q.and(t);
    }
    Ok(q)
}

/// Lines matching a query, restricted to the actor's account scope.
pub fn load_matching(ctx: &Ctx, q: &Query) -> Result<Vec<Line>> {
    let (lo, hi) = q.date_bounds();
    let lines = fetch_lines(ctx.conn, lo, hi, false)?;
    let mut out = Vec::new();
    for l in lines {
        if let Some(scope) = &ctx.actor.accounts {
            if !scope.contains(&l.account_id) {
                continue;
            }
        }
        if line_matches(q, &l)? {
            out.push(l);
        }
    }
    Ok(out)
}

struct TxnAgg {
    first: Line,
    categories: Vec<Option<String>>,
}

fn group_txns(lines: Vec<Line>) -> Vec<TxnAgg> {
    let mut idx: HashMap<i64, usize> = HashMap::new();
    let mut out: Vec<TxnAgg> = Vec::new();
    for l in lines {
        match idx.get(&l.txn_id) {
            Some(i) => out[*i].categories.push(l.category.clone()),
            None => {
                idx.insert(l.txn_id, out.len());
                out.push(TxnAgg {
                    categories: vec![l.category.clone()],
                    first: l,
                });
            }
        }
    }
    out
}

fn list(ctx: &Ctx, a: &Args) -> Result<Value> {
    let q = filter_query(ctx, a)?;
    let mut txns = group_txns(load_matching(ctx, &q)?);
    // Splits matching the filter are only part of a transaction; report the
    // full split count for transparency.
    let split_counts: HashMap<i64, i64> = ctx
        .rows(
            "SELECT txn_id, COUNT(*) AS n FROM splits GROUP BY txn_id",
            &[],
        )?
        .into_iter()
        .map(|r| {
            (
                r["txn_id"].as_i64().unwrap_or(0),
                r["n"].as_i64().unwrap_or(0),
            )
        })
        .collect();
    let sort = a.opt_str("sort").unwrap_or_else(|| "-date".into());
    match sort.as_str() {
        "date" => txns.sort_by_key(|x| (x.first.date, x.first.txn_id)),
        "-date" => txns.sort_by_key(|x| std::cmp::Reverse((x.first.date, x.first.txn_id))),
        "amount" => txns.sort_by_key(|t| (t.first.txn_amount_minor, t.first.txn_id)),
        "-amount" => {
            txns.sort_by_key(|t| (std::cmp::Reverse(t.first.txn_amount_minor), t.first.txn_id))
        }
        "id" => txns.sort_by_key(|t| t.first.txn_id),
        _ => txns.sort_by_key(|t| std::cmp::Reverse(t.first.txn_id)),
    }
    let total = txns.len();
    let offset = a.opt_i64("offset")?.unwrap_or(0).max(0) as usize;
    let limit = a.opt_i64("limit")?.map(|l| l.max(0) as usize);
    let page: Vec<TxnAgg> = txns
        .into_iter()
        .skip(offset)
        .take(limit.unwrap_or(usize::MAX))
        .collect();
    if page.len() + offset < total {
        ctx.warn(
            "truncated",
            format!("showing {} of {total} matching transactions", page.len()),
            json!({ "total": total, "returned": page.len(), "offset": offset }),
        );
    }
    Ok(Value::Array(
        page.into_iter()
            .map(|t| {
                let l = &t.first;
                let n = *split_counts.get(&l.txn_id).unwrap_or(&1);
                let category = if n > 1 {
                    json!("(split)")
                } else {
                    json!(t.categories.first().cloned().flatten())
                };
                json!({
                    "id": l.txn_id, "date": fmt_date(l.date), "account": l.account,
                    "amount_minor": l.txn_amount_minor, "currency": l.currency,
                    "payee": l.payee, "category": category, "status": l.status, "kind": l.kind,
                    "tags": l.tags, "memo": l.memo, "splits": n,
                })
            })
            .collect(),
    ))
}

fn get(ctx: &Ctx, a: &Args) -> Result<Value> {
    txn_json(ctx, a.i64("id")?)
}

fn edit(ctx: &Ctx, a: &Args) -> Result<Value> {
    let id = a.i64("id")?;
    let row = load_txn_row(ctx, id)?;
    if !row["deleted_at"].is_null() {
        return Err(
            Error::conflict("txn_deleted", format!("transaction #{id} is deleted"))
                .with_hint("restore it with `pext txn restore`"),
        );
    }
    let kind = row["kind"].as_str().unwrap_or("standard").to_string();
    let acct = load_account(ctx, row["account_id"].as_i64().unwrap_or(0))?;
    let mut cols: Vec<(&str, rusqlite::types::Value)> = vec![];
    let mut touched = false;
    if let Some(d) = a.opt_date("date")? {
        check_date(ctx, d)?;
        cols.push(("date", sv(&fmt_date(d))));
        touched = true;
    }
    if let Some(m) = a.opt_str("memo") {
        cols.push(("memo", sv(&m)));
        touched = true;
    }
    if let Some(s) = a.opt_str("status") {
        cols.push(("status", sv(&s)));
        touched = true;
    }
    let wants_payee = a.opt_str("payee");
    let wants_cat = a.opt_str("category");
    let new_amount = a.money("amount", acct.decimals())?;
    let new_account = a.opt_str("move_to");
    if kind == "transfer"
        && (wants_payee.is_some()
            || wants_cat.is_some()
            || new_amount.is_some()
            || new_account.is_some())
    {
        return Err(Error::validation(
            "transfer_leg_immutable",
            "transfer legs only allow changing date, memo and status",
        )
        .with_hint("delete the transfer and create it again"));
    }
    if let Some(p) = wants_payee {
        cols.push(("payee", sv(&normalize_payee(ctx, &p)?)));
        touched = true;
    }
    let splits = ctx.rows(
        "SELECT id FROM splits WHERE txn_id=?1 ORDER BY id",
        &[iv(id)],
    )?;
    if let Some(c) = wants_cat {
        if splits.len() != 1 {
            return Err(
                Error::validation("use_split", "this transaction has several splits")
                    .with_hint("use `pext txn split`"),
            );
        }
        let cid = match c.to_ascii_lowercase().as_str() {
            "none" | "uncategorized" => None,
            p => Some(category_id(ctx, p)?),
        };
        ctx.update(
            "splits",
            splits[0]["id"].as_i64().unwrap_or(0),
            &[("category_id", oiv(cid))],
        )?;
        touched = true;
    }
    if let Some(m) = new_amount {
        check_amount(m)?;
        if m == 0 {
            return Err(
                Error::validation("zero_amount", "amount cannot be zero").with_field("amount")
            );
        }
        if splits.len() != 1 {
            return Err(
                Error::validation("use_split", "this transaction has several splits")
                    .with_hint("re-specify all splits with `pext txn split`"),
            );
        }
        ctx.update(
            "splits",
            splits[0]["id"].as_i64().unwrap_or(0),
            &[("amount_minor", iv(m))],
        )?;
        cols.push(("amount_minor", iv(m)));
        touched = true;
    }
    if let Some(r) = new_account {
        let na = resolve_account(ctx, &r)?;
        if na.currency != acct.currency {
            return Err(Error::validation(
                "currency_mismatch",
                "can only move a transaction to an account with the same currency",
            ));
        }
        cols.push(("account_id", iv(na.id)));
        touched = true;
    }
    if !touched {
        return Err(
            Error::usage("nothing_to_change", "no changes given").with_hint(
                "pass --date, --payee, --memo, --category, --amount, --status or --move-to",
            ),
        );
    }
    ctx.update("transactions", id, &cols)?;
    if kind == "transfer" {
        if let Some(other) = row["transfer_id"].as_i64() {
            let shared: Vec<_> = cols
                .iter()
                .filter(|(c, _)| matches!(*c, "date" | "memo" | "status"))
                .cloned()
                .collect();
            ctx.update("transactions", other, &shared)?;
        }
    }
    txn_json(ctx, id)
}

fn set_deleted(ctx: &Ctx, id: i64, deleted: bool) -> Result<Vec<i64>> {
    let row = load_txn_row(ctx, id)?;
    let is_deleted = !row["deleted_at"].is_null();
    if is_deleted == deleted {
        return Err(Error::conflict(
            if deleted {
                "already_deleted"
            } else {
                "not_deleted"
            },
            format!(
                "transaction #{id} is {}",
                if deleted {
                    "already deleted"
                } else {
                    "not deleted"
                }
            ),
        ));
    }
    let mut ids = vec![id];
    if let Some(o) = row["transfer_id"].as_i64() {
        ids.push(o);
    }
    for i in &ids {
        let v = if deleted {
            sv(&ctx.now)
        } else {
            rusqlite::types::Value::Null
        };
        ctx.update("transactions", *i, &[("deleted_at", v)])?;
    }
    Ok(ids)
}

fn rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let ids = set_deleted(ctx, a.i64("id")?, true)?;
    Ok(json!({ "deleted": ids, "undo_hint": "pext undo" }))
}

fn restore(ctx: &Ctx, a: &Args) -> Result<Value> {
    let ids = set_deleted(ctx, a.i64("id")?, false)?;
    Ok(json!({ "restored": ids }))
}

fn split(ctx: &Ctx, a: &Args) -> Result<Value> {
    let id = a.i64("id")?;
    let row = load_txn_row(ctx, id)?;
    if row["kind"].as_str() == Some("transfer") {
        return Err(Error::validation(
            "transfer_leg_immutable",
            "transfers cannot be categorised or split",
        ));
    }
    let amount = row["amount_minor"].as_i64().unwrap_or(0);
    let dec = decimals_for(row["currency"].as_str().unwrap_or("INR"));
    let specs = a.list("split");
    let new = if !specs.is_empty() {
        parse_splits(ctx, &specs, amount, dec)?
    } else if let Some(c) = a.opt_str("category") {
        let cid = match c.to_ascii_lowercase().as_str() {
            "none" | "uncategorized" => None,
            p => Some(category_id(ctx, p)?),
        };
        vec![SplitIn {
            category_id: cid,
            amount_minor: amount,
            note: String::new(),
        }]
    } else {
        return Err(Error::usage(
            "missing_argument",
            "pass --split category=amount (repeatable) or --category",
        ));
    };
    for s in ctx.rows("SELECT id FROM splits WHERE txn_id=?1", &[iv(id)])? {
        ctx.delete("splits", s["id"].as_i64().unwrap_or(0))?;
    }
    for s in new {
        ctx.insert(
            "splits",
            &[
                ("txn_id", iv(id)),
                ("category_id", oiv(s.category_id)),
                ("amount_minor", iv(s.amount_minor)),
                ("note", sv(&s.note)),
            ],
        )?;
    }
    txn_json(ctx, id)
}

fn tag(ctx: &Ctx, a: &Args) -> Result<Value> {
    let id = a.i64("id")?;
    load_txn_row(ctx, id)?;
    for t in a.list("tag") {
        let t = normalize_tag(&t)?;
        if ctx
            .opt_i64(
                "SELECT id FROM txn_tags WHERE txn_id=?1 AND tag=?2",
                &[iv(id), sv(&t)],
            )?
            .is_none()
        {
            ctx.insert("txn_tags", &[("txn_id", iv(id)), ("tag", sv(&t))])?;
        }
    }
    txn_json(ctx, id)
}

fn untag(ctx: &Ctx, a: &Args) -> Result<Value> {
    let id = a.i64("id")?;
    load_txn_row(ctx, id)?;
    for t in a.list("tag") {
        let t = normalize_tag(&t)?;
        if let Some(rid) = ctx.opt_i64(
            "SELECT id FROM txn_tags WHERE txn_id=?1 AND tag=?2",
            &[iv(id), sv(&t)],
        )? {
            ctx.delete("txn_tags", rid)?;
        }
    }
    txn_json(ctx, id)
}

fn transfer(ctx: &Ctx, a: &Args) -> Result<Value> {
    let from = resolve_account(ctx, &a.str("from")?)?;
    let to = resolve_account(ctx, &a.str("to")?)?;
    if from.id == to.id {
        return Err(Error::validation(
            "same_account",
            "source and destination accounts must differ",
        ));
    }
    let amount = a.req_money("amount", from.decimals())?;
    if amount <= 0 {
        return Err(
            Error::validation("invalid_amount", "transfer amount must be positive")
                .with_field("amount"),
        );
    }
    let to_amount = if from.currency == to.currency {
        if a.has("to_amount") || a.has("to_amount_minor") {
            let t = a.req_money("to_amount", to.decimals())?;
            if t != amount {
                return Err(Error::validation(
                    "amount_mismatch",
                    "same-currency transfers must have equal amounts",
                ));
            }
        }
        amount
    } else {
        let t = a.money("to_amount", to.decimals())?.ok_or_else(|| {
            Error::usage(
                "missing_argument",
                format!(
                    "accounts use different currencies ({} -> {}); pass --to-amount",
                    from.currency, to.currency
                ),
            )
            .with_field("to_amount")
        })?;
        if t <= 0 {
            return Err(
                Error::validation("invalid_amount", "destination amount must be positive")
                    .with_field("to_amount"),
            );
        }
        t
    };
    let date = a.opt_date("date")?.unwrap_or(ctx.today);
    let memo = a.opt_str("memo").unwrap_or_default();
    let status = a.opt_str("status").unwrap_or_else(|| "cleared".into());
    let leg = |acct: &AccountRow, amt: i64, payee: String, link: Option<i64>| NewTxn {
        account: acct.clone(),
        date,
        amount_minor: amt,
        payee,
        memo: memo.clone(),
        status: status.clone(),
        kind: "transfer",
        transfer_id: link,
        external_ref: None,
        fingerprint: None,
        import_batch: None,
        splits: vec![SplitIn {
            category_id: None,
            amount_minor: amt,
            note: String::new(),
        }],
        tags: vec![],
    };
    let out_id = create_txn(
        ctx,
        leg(&from, -amount, format!("Transfer to {}", to.name), None),
    )?;
    let in_id = create_txn(
        ctx,
        leg(
            &to,
            to_amount,
            format!("Transfer from {}", from.name),
            Some(out_id),
        ),
    )?;
    ctx.update("transactions", out_id, &[("transfer_id", iv(in_id))])?;
    Ok(json!({ "out": txn_json(ctx, out_id)?, "in": txn_json(ctx, in_id)? }))
}

fn payee_similar(a: &str, b: &str) -> bool {
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    a.is_empty()
        || b.is_empty()
        || a == b
        || (a.len() >= 4 && b.contains(&a))
        || (b.len() >= 4 && a.contains(&b))
}

fn dedupe(ctx: &Ctx, a: &Args) -> Result<Value> {
    let window = a.opt_i64("window_days")?.unwrap_or(3).clamp(0, 31);
    let mut q = Query {
        include_transfers: false,
        ..Default::default()
    };
    let accts = a.list("account");
    if !accts.is_empty() {
        q.and(Term::new(Field::Account, Op::Eq, accts));
    }
    let mut seen = std::collections::HashSet::new();
    let lines: Vec<Line> = load_matching(ctx, &q)?
        .into_iter()
        .filter(|l| seen.insert(l.txn_id))
        .collect();
    let mut by_key: HashMap<(i64, i64), Vec<&Line>> = HashMap::new();
    for l in &lines {
        by_key
            .entry((l.account_id, l.txn_amount_minor))
            .or_default()
            .push(l);
    }
    let mut pairs = Vec::new();
    let mut keys: Vec<_> = by_key.keys().cloned().collect();
    keys.sort();
    for k in keys {
        let mut group = by_key[&k].clone();
        group.sort_by_key(|l| (l.date, l.txn_id));
        for i in 0..group.len() {
            for j in (i + 1)..group.len() {
                let (x, y) = (group[i], group[j]);
                if (y.date - x.date).num_days() > window {
                    break;
                }
                if payee_similar(&x.payee, &y.payee) {
                    pairs.push(json!({
                        "account": x.account, "amount_minor": x.txn_amount_minor, "currency": x.currency,
                        "first_id": x.txn_id, "first_date": fmt_date(x.date), "first_payee": x.payee,
                        "second_id": y.txn_id, "second_date": fmt_date(y.date), "second_payee": y.payee,
                    }));
                }
            }
        }
    }
    pairs.truncate(500);
    Ok(Value::Array(pairs))
}

fn recategorize(ctx: &Ctx, a: &Args) -> Result<Value> {
    let q = filter_query(ctx, a)?;
    let cat = a.str("set_category")?;
    let cid = match cat.to_ascii_lowercase().as_str() {
        "none" | "uncategorized" => None,
        p => Some(category_id(ctx, p)?),
    };
    let only_uncat = a.flag("only_uncategorized");
    let mut seen = std::collections::HashSet::new();
    let (mut updated, mut skipped) = (0, 0);
    for l in load_matching(ctx, &q)? {
        if !seen.insert(l.txn_id) {
            continue;
        }
        let splits = ctx.rows(
            "SELECT id, category_id FROM splits WHERE txn_id=?1",
            &[iv(l.txn_id)],
        )?;
        if l.kind != "standard"
            || splits.len() != 1
            || (only_uncat && !splits[0]["category_id"].is_null())
        {
            skipped += 1;
            continue;
        }
        if splits[0]["category_id"].as_i64() == cid {
            skipped += 1;
            continue;
        }
        ctx.update(
            "splits",
            splits[0]["id"].as_i64().unwrap_or(0),
            &[("category_id", oiv(cid))],
        )?;
        updated += 1;
    }
    Ok(json!({ "updated": updated, "skipped": skipped, "category": cat }))
}

pub fn ops() -> Vec<OpSpec> {
    let mut list_op = OpSpec::new("txn.list", "List transactions (filterable)", list)
        .scoped()
        .ps(filter_params())
        .p(Param::s("sort", "date, -date, amount, -amount, id, -id")
            .choices(&["date", "-date", "amount", "-amount", "id", "-id"])
            .def("-date"))
        .p(Param::i("limit", "maximum rows"))
        .p(Param::i("offset", "skip this many rows"))
        .ex("pext txn list --from 2026-10-01 --category food --min 500")
        .ex("pext txn list --uncategorized --output json");
    list_op = crate::ops::query::with_query_param(list_op);
    vec![
        OpSpec::new("txn.add", "Add a transaction (negative = money out)", add)
            .append()
            .scoped()
            .p(Param::s("account", "account id or name").req())
            .ps(money("amount", "amount_minor", "signed amount: negative for expenses, positive for income"))
            .p(Param::date("date", "date (default: today)"))
            .p(Param::s("payee", "payee/merchant"))
            .p(Param::s("memo", "free-text note"))
            .p(Param::s("category", "category path, e.g. food.delivery"))
            .p(Param::s("split", "category=amount (repeatable; amounts are magnitudes and must sum to |amount|)").multi())
            .p(Param::s("tag", "tag (repeatable)").multi())
            .p(Param::s("status", "pending or cleared").choices(STATUSES).def("cleared"))
            .p(Param::s("external_ref", "your own reference (unique per account)"))
            .ex("pext txn add --account hdfc --amount -450.50 --payee Swiggy --category food.delivery")
            .ex("pext txn add --account hdfc --amount -500 --split food.groceries=300 --split shopping.household=200"),
        list_op,
        OpSpec::new("txn.get", "Show one transaction with splits and tags", get)
            .scoped()
            .p(Param::i("id", "transaction id").req().pos()),
        OpSpec::new("txn.edit", "Edit a transaction", edit)
            .write()
            .p(Param::i("id", "transaction id").req().pos())
            .p(Param::date("date", "new date"))
            .p(Param::s("payee", "new payee"))
            .p(Param::s("memo", "new memo"))
            .p(Param::s("category", "new category (single-split transactions)"))
            .p(Param::amount("amount", "new signed amount (single-split transactions)"))
            .p(Param::s("status", "new status").choices(STATUSES))
            .p(Param::s("move_to", "move to another account with the same currency")),
        OpSpec::new("txn.rm", "Delete a transaction (soft; undoable)", rm)
            .write()
            .p(Param::i("id", "transaction id").req().pos()),
        OpSpec::new("txn.restore", "Restore a deleted transaction", restore)
            .write()
            .p(Param::i("id", "transaction id").req().pos()),
        OpSpec::new("txn.split", "Replace a transaction's splits", split)
            .write()
            .p(Param::i("id", "transaction id").req().pos())
            .p(Param::s("split", "category=amount (repeatable)").multi())
            .p(Param::s("category", "single category for the whole amount")),
        OpSpec::new("txn.tag", "Add tags", tag)
            .write()
            .p(Param::i("id", "transaction id").req().pos())
            .p(Param::s("tag", "tag (repeatable)").req().multi()),
        OpSpec::new("txn.untag", "Remove tags", untag)
            .write()
            .p(Param::i("id", "transaction id").req().pos())
            .p(Param::s("tag", "tag (repeatable)").req().multi()),
        OpSpec::new("txn.transfer", "Move money between your accounts (two linked legs)", transfer)
            .write()
            .p(Param::s("from", "source account").req())
            .p(Param::s("to", "destination account").req())
            .ps(money("amount", "amount_minor", "positive amount leaving the source account"))
            .ps(money("to_amount", "to_amount_minor", "amount received (required if currencies differ)"))
            .p(Param::date("date", "date (default: today)"))
            .p(Param::s("memo", "note"))
            .p(Param::s("status", "pending or cleared").choices(STATUSES).def("cleared"))
            .ex("pext txn transfer --from hdfc --to wallet --amount 2000"),
        OpSpec::new("txn.dedupe", "Find likely duplicate transactions", dedupe)
            .p(Param::s("account", "limit to these accounts").multi())
            .p(Param::i("window_days", "maximum days apart").def("3")),
        {
            let mut o = OpSpec::new("txn.recategorize", "Set the category of all matching single-split transactions", recategorize)
                .write()
                .ps(filter_params())
                .p(Param::s("set_category", "category to assign (path, or 'none')").req())
                .p(Param::b("only_uncategorized", "leave already-categorised transactions alone"))
                .ex("pext txn recategorize --payee swiggy --set-category food.delivery --only-uncategorized --dry-run");
            o = crate::ops::query::with_query_param(o);
            o
        },
    ]
}
