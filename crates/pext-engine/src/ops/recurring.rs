//! Recurring items: bills, EMIs, salary and subscriptions. Occurrences are
//! computed from the start date (so "the 31st" survives short months) and are
//! posted as ordinary transactions with a unique reference, which makes
//! posting idempotent.

use crate::args::Args;
use crate::ctx::{iv, oiv, sv, Ctx};
use crate::ops::accounts::{load_account, resolve_account};
use crate::ops::categories::{category_id, category_path, collapse_ws};
use crate::ops::txns::{create_txn, fmt_date, NewTxn, SplitIn};
use crate::registry::{money, OpSpec, Param};
use chrono::NaiveDate;
use pext_core::dates::{add_days, add_months};
use pext_core::money::{check_amount, mul_div_round};
use pext_core::{Error, Result};
use serde_json::{json, Value};

pub const KINDS: &[&str] = &["bill", "income", "emi", "subscription", "other"];
pub const FREQS: &[&str] = &["daily", "weekly", "monthly", "quarterly", "yearly"];
const MAX_POST: i64 = 366;

/// The `n`th due date (0-based) of a schedule.
pub fn occurrence(start: NaiveDate, freq: &str, every: i64, n: i64) -> Result<NaiveDate> {
    let months = |m: i64| -> Result<NaiveDate> {
        let m = i32::try_from(m).map_err(|_| {
            Error::validation(
                "date_out_of_range",
                "schedule runs out of the supported date range",
            )
        })?;
        add_months(start, m)
    };
    match freq {
        "daily" => add_days(start, every.saturating_mul(n)),
        "weekly" => add_days(start, every.saturating_mul(7).saturating_mul(n)),
        "monthly" => months(every.saturating_mul(n)),
        "quarterly" => months(every.saturating_mul(3).saturating_mul(n)),
        "yearly" => months(every.saturating_mul(12).saturating_mul(n)),
        other => Err(Error::internal(
            "bad_freq",
            format!("unknown frequency '{other}'"),
        )),
    }
}

/// Occurrences per year x1000 for a schedule (used for annual cost).
pub fn annual_cost(amount_minor: i64, freq: &str, every: i64) -> i64 {
    let (num, den): (i128, i128) = match freq {
        "daily" => (365, every as i128),
        "weekly" => (365, 7 * every as i128),
        "monthly" => (12, every as i128),
        "quarterly" => (4, every as i128),
        _ => (1, every as i128),
    };
    mul_div_round(amount_minor as i128, num, den.max(1)).unwrap_or(0) as i64
}

#[derive(Debug, Clone)]
pub struct Item {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub account_id: i64,
    pub amount_minor: i64,
    pub category_id: Option<i64>,
    pub payee: String,
    pub freq: String,
    pub every: i64,
    pub start: NaiveDate,
    pub next_index: i64,
    pub next_due: NaiveDate,
    pub end: Option<NaiveDate>,
    pub auto_post: bool,
    pub remind_days: i64,
    pub active: bool,
}

fn parse_date(s: &str) -> Result<NaiveDate> {
    crate::ops::accounts::parse_stored_date(s)
}

pub fn load_items(ctx: &Ctx, only: Option<i64>, include_inactive: bool) -> Result<Vec<Item>> {
    let rows = ctx.rows("SELECT * FROM recurring ORDER BY next_due, id", &[])?;
    let mut out = vec![];
    for r in rows {
        let id = r["id"].as_i64().unwrap_or(0);
        if only.is_some_and(|o| o != id) {
            continue;
        }
        let active = r["active"].as_i64() == Some(1);
        if !active && !include_inactive {
            continue;
        }
        out.push(Item {
            id,
            name: r["name"].as_str().unwrap_or("").into(),
            kind: r["kind"].as_str().unwrap_or("").into(),
            account_id: r["account_id"].as_i64().unwrap_or(0),
            amount_minor: r["amount_minor"].as_i64().unwrap_or(0),
            category_id: r["category_id"].as_i64(),
            payee: r["payee"].as_str().unwrap_or("").into(),
            freq: r["freq"].as_str().unwrap_or("").into(),
            every: r["every"].as_i64().unwrap_or(1),
            start: parse_date(r["start_date"].as_str().unwrap_or(""))?,
            next_index: r["next_index"].as_i64().unwrap_or(0),
            next_due: parse_date(r["next_due"].as_str().unwrap_or(""))?,
            end: match r["end_date"].as_str() {
                Some(s) => Some(parse_date(s)?),
                None => None,
            },
            auto_post: r["auto_post"].as_i64() == Some(1),
            remind_days: r["remind_days"].as_i64().unwrap_or(3),
            active,
        });
    }
    Ok(out)
}

fn item_json(ctx: &Ctx, it: &Item) -> Result<Value> {
    let acct = load_account(ctx, it.account_id)?;
    let days_until = (it.next_due - ctx.today).num_days();
    Ok(json!({
        "id": it.id, "name": it.name, "kind": it.kind, "account": acct.name, "currency": acct.currency,
        "amount_minor": it.amount_minor,
        "category": match it.category_id { Some(c) => Some(category_path(ctx, c)?), None => None },
        "payee": it.payee, "freq": it.freq, "every": it.every,
        "start": fmt_date(it.start), "next_due": fmt_date(it.next_due), "days_until": days_until,
        "overdue": it.active && days_until < 0, "end": it.end.map(fmt_date),
        "auto_post": it.auto_post, "remind_days": it.remind_days, "active": it.active,
        "annual_minor": annual_cost(it.amount_minor, &it.freq, it.every),
    }))
}

/// Upcoming occurrences from each active item's next due date up to `through`.
pub fn upcoming(
    ctx: &Ctx,
    through: NaiveDate,
    cap_per_item: i64,
) -> Result<Vec<(Item, NaiveDate, i64)>> {
    let mut out = vec![];
    for it in load_items(ctx, None, false)? {
        let mut n = it.next_index;
        let mut taken = 0;
        loop {
            let d = match occurrence(it.start, &it.freq, it.every, n) {
                Ok(d) => d,
                Err(_) => break,
            };
            if d > through || it.end.is_some_and(|e| d > e) || taken >= cap_per_item {
                break;
            }
            out.push((it.clone(), d, n));
            n += 1;
            taken += 1;
        }
    }
    out.sort_by_key(|(it, d, _)| (*d, it.id));
    Ok(out)
}

fn signed_amount(kind: &str, amount: i64) -> Result<i64> {
    check_amount(amount)?;
    if amount == 0 {
        return Err(Error::validation("zero_amount", "amount cannot be zero").with_field("amount"));
    }
    match kind {
        "income" => {
            if amount < 0 {
                return Err(
                    Error::validation("invalid_amount", "income must be positive")
                        .with_field("amount"),
                );
            }
            Ok(amount)
        }
        "other" => Ok(amount),
        _ => Ok(-amount.abs()),
    }
}

fn add(ctx: &Ctx, a: &Args) -> Result<Value> {
    let name = collapse_ws(&a.str("name")?);
    let kind = a.opt_str("kind").unwrap_or_else(|| "bill".into());
    let acct = resolve_account(ctx, &a.str("account")?)?;
    if acct.archived {
        return Err(Error::conflict(
            "account_archived",
            format!("account '{}' is archived", acct.name),
        ));
    }
    let amount = signed_amount(&kind, a.req_money("amount", acct.decimals())?)?;
    let freq = a.opt_str("freq").unwrap_or_else(|| "monthly".into());
    let every = a.opt_i64("every")?.unwrap_or(1);
    let start = a.opt_date("start")?.unwrap_or(ctx.today);
    let end = a.opt_date("end")?;
    if end.is_some_and(|e| e < start) {
        return Err(
            Error::validation("invalid_end", "the end date is before the start date")
                .with_field("end"),
        );
    }
    let cat = match a.opt_str("category") {
        Some(c) => Some(category_id(ctx, &c)?),
        None => None,
    };
    let payee = a
        .opt_str("payee")
        .map(|p| collapse_ws(&p))
        .unwrap_or_else(|| name.clone());
    let remind = a.opt_i64("remind_days")?.unwrap_or(3);
    let id = ctx.insert(
        "recurring",
        &[
            ("name", sv(&name)),
            ("kind", sv(&kind)),
            ("account_id", iv(acct.id)),
            ("amount_minor", iv(amount)),
            ("category_id", oiv(cat)),
            ("payee", sv(&payee)),
            ("freq", sv(&freq)),
            ("every", iv(every)),
            ("start_date", sv(&fmt_date(start))),
            ("next_index", iv(0)),
            ("next_due", sv(&fmt_date(start))),
            ("end_date", crate::ctx::osv(end.map(fmt_date).as_deref())),
            ("auto_post", iv(a.flag("auto_post") as i64)),
            ("remind_days", iv(remind)),
            ("active", iv(1)),
            ("created_at", sv(&ctx.now)),
        ],
    )?;
    item_json(ctx, &load_items(ctx, Some(id), true)?.remove(0))
}

fn find(ctx: &Ctx, r: &str) -> Result<Item> {
    let items = load_items(ctx, None, true)?;
    let by_id = r.trim().parse::<i64>().ok();
    items
        .into_iter()
        .find(|i| Some(i.id) == by_id || i.name.eq_ignore_ascii_case(r.trim()))
        .ok_or_else(|| {
            Error::not_found("recurring_not_found", format!("no recurring item '{r}'"))
                .with_hint("run `pext recur list --all`")
        })
}

fn list(ctx: &Ctx, a: &Args) -> Result<Value> {
    let items = load_items(ctx, None, a.flag("all"))?;
    items
        .iter()
        .map(|i| item_json(ctx, i))
        .collect::<Result<Vec<_>>>()
        .map(Value::Array)
}

fn get(ctx: &Ctx, a: &Args) -> Result<Value> {
    item_json(ctx, &find(ctx, &a.str("item")?)?)
}

fn edit(ctx: &Ctx, a: &Args) -> Result<Value> {
    let it = find(ctx, &a.str("item")?)?;
    let mut cols = vec![];
    if let Some(n) = a.opt_str("new_name") {
        cols.push(("name", sv(&collapse_ws(&n))));
    }
    let acct = load_account(ctx, it.account_id)?;
    if let Some(m) = a.money("amount", acct.decimals())? {
        cols.push(("amount_minor", iv(signed_amount(&it.kind, m)?)));
    }
    if let Some(c) = a.opt_str("category") {
        cols.push((
            "category_id",
            oiv(if c.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(category_id(ctx, &c)?)
            }),
        ));
    }
    if let Some(p) = a.opt_str("payee") {
        cols.push(("payee", sv(&collapse_ws(&p))));
    }
    if let Some(e) = a.opt_date("end")? {
        if e < it.start {
            return Err(
                Error::validation("invalid_end", "the end date is before the start date")
                    .with_field("end"),
            );
        }
        cols.push(("end_date", sv(&fmt_date(e))));
    }
    if let Some(r) = a.opt_i64("remind_days")? {
        cols.push(("remind_days", iv(r)));
    }
    if a.flag("auto_post") {
        cols.push(("auto_post", iv(1)));
    }
    if a.flag("manual") {
        cols.push(("auto_post", iv(0)));
    }
    if a.flag("pause") {
        cols.push(("active", iv(0)));
    }
    if a.flag("resume") {
        cols.push(("active", iv(1)));
    }
    if cols.is_empty() {
        return Err(Error::usage(
            "nothing_to_change",
            "pass at least one field to change",
        ));
    }
    ctx.update("recurring", it.id, &cols)?;
    item_json(ctx, &load_items(ctx, Some(it.id), true)?.remove(0))
}

fn rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let it = find(ctx, &a.str("item")?)?;
    ctx.delete("recurring", it.id)?;
    Ok(json!({ "removed": it.id, "note": "transactions already posted are kept" }))
}

fn due(ctx: &Ctx, a: &Args) -> Result<Value> {
    let within = a.opt_i64("within")?.unwrap_or(30).clamp(0, 3660);
    let through = add_days(ctx.today, within)?;
    let mut out = vec![];
    for (it, d, _) in upcoming(ctx, through, 100)? {
        let acct = load_account(ctx, it.account_id)?;
        let days = (d - ctx.today).num_days();
        out.push(json!({
            "item": it.name, "kind": it.kind, "due": fmt_date(d), "days_until": days, "overdue": days < 0,
            "account": acct.name, "amount_minor": it.amount_minor, "currency": acct.currency,
        }));
    }
    Ok(Value::Array(out))
}

fn advance(ctx: &Ctx, it: &Item, new_index: i64) -> Result<()> {
    let next = occurrence(it.start, &it.freq, it.every, new_index)?;
    let finished = it.end.is_some_and(|e| next > e);
    let mut cols = vec![
        ("next_index", iv(new_index)),
        ("next_due", sv(&fmt_date(next))),
    ];
    if finished {
        cols.push(("active", iv(0)));
    }
    ctx.update("recurring", it.id, &cols)
}

fn post(ctx: &Ctx, a: &Args) -> Result<Value> {
    let through = a.opt_date("through")?.unwrap_or(ctx.today);
    let only = match a.opt_str("item") {
        Some(r) => Some(find(ctx, &r)?.id),
        None => None,
    };
    let status = a.opt_str("status").unwrap_or_else(|| "pending".into());
    let mut posted = vec![];
    let mut left = MAX_POST;
    let mut truncated = false;
    for it in load_items(ctx, only, false)? {
        if a.flag("auto_only") && !it.auto_post {
            continue;
        }
        let acct = load_account(ctx, it.account_id)?;
        let mut n = it.next_index;
        loop {
            let d = occurrence(it.start, &it.freq, it.every, n)?;
            if d > through || it.end.is_some_and(|e| d > e) {
                break;
            }
            if left == 0 {
                truncated = true;
                break;
            }
            if acct.archived {
                return Err(Error::conflict(
                    "account_archived",
                    format!(
                        "account '{}' is archived; pause '{}' first",
                        acct.name, it.name
                    ),
                ));
            }
            let eref = format!("recur:{}:{}", it.id, n);
            let exists = ctx.opt_i64(
                "SELECT id FROM transactions WHERE account_id=?1 AND external_ref=?2 AND deleted_at IS NULL",
                &[iv(acct.id), sv(&eref)],
            )?;
            if exists.is_none() {
                let id = create_txn(
                    ctx,
                    NewTxn {
                        account: acct.clone(),
                        date: d,
                        amount_minor: it.amount_minor,
                        payee: it.payee.clone(),
                        memo: format!("recurring: {}", it.name),
                        status: status.clone(),
                        kind: "standard",
                        transfer_id: None,
                        external_ref: Some(eref),
                        fingerprint: None,
                        import_batch: None,
                        splits: vec![SplitIn {
                            category_id: it.category_id,
                            amount_minor: it.amount_minor,
                            note: String::new(),
                        }],
                        tags: vec![],
                    },
                )?;
                posted.push(json!({ "item": it.name, "txn_id": id, "date": fmt_date(d), "amount_minor": it.amount_minor, "currency": acct.currency }));
            }
            n += 1;
            left -= 1;
        }
        if n != it.next_index {
            advance(ctx, &it, n)?;
        }
    }
    if truncated {
        ctx.warn(
            "post_limit",
            format!("posted the first {MAX_POST} occurrences; run again to continue"),
            json!({}),
        );
    }
    Ok(json!({ "through": fmt_date(through), "posted": posted.len(), "transactions": posted }))
}

fn skip(ctx: &Ctx, a: &Args) -> Result<Value> {
    let it = find(ctx, &a.str("item")?)?;
    let count = a.opt_i64("count")?.unwrap_or(1);
    if !(1..=120).contains(&count) {
        return Err(
            Error::validation("invalid_count", "count must be between 1 and 120")
                .with_field("count"),
        );
    }
    let skipped = occurrence(it.start, &it.freq, it.every, it.next_index + count - 1)?;
    advance(ctx, &it, it.next_index + count)?;
    Ok(json!({ "item": it.name, "skipped": count, "last_skipped_due": fmt_date(skipped) }))
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new("recur.add", "Add a recurring bill, EMI, income or subscription", add)
            .write()
            .p(Param::s("name", "short name, e.g. 'rent'").req())
            .p(Param::s("account", "account it is paid from / into").req())
            .ps(money("amount", "amount_minor", "amount per occurrence (bills are stored as outflows, income as inflow)"))
            .p(Param::s("kind", "bill (default), income, emi, subscription or other").choices(KINDS))
            .p(Param::s("freq", "daily, weekly, monthly (default), quarterly or yearly").choices(FREQS))
            .p(Param::i("every", "every N periods (default 1)"))
            .p(Param::date("start", "first due date (default today)"))
            .p(Param::date("end", "last possible due date"))
            .p(Param::s("category", "category for posted transactions"))
            .p(Param::s("payee", "payee for posted transactions (default: the name)"))
            .p(Param::b("auto_post", "include in `recur post --auto-only` runs"))
            .p(Param::i("remind_days", "alert this many days before it is due (default 3)"))
            .ex("pext recur add --name rent --account hdfc --amount 25000 --start 2026-12-01 --category housing.rent"),
        OpSpec::new("recur.list", "List recurring items", list).p(Param::b("all", "include paused and finished items")),
        OpSpec::new("recur.get", "Show one recurring item", get).p(Param::s("item", "id or name").req().pos()),
        OpSpec::new("recur.edit", "Change a recurring item", edit)
            .write()
            .p(Param::s("item", "id or name").req().pos())
            .p(Param::s("new_name", "rename"))
            .ps(money("amount", "amount_minor", "new amount per occurrence"))
            .p(Param::s("category", "new category (or 'none')"))
            .p(Param::s("payee", "new payee"))
            .p(Param::date("end", "new end date"))
            .p(Param::i("remind_days", "reminder lead time in days"))
            .p(Param::b("auto_post", "include in automatic posting"))
            .p(Param::b("manual", "exclude from automatic posting"))
            .p(Param::b("pause", "stop generating occurrences"))
            .p(Param::b("resume", "resume a paused item")),
        OpSpec::new("recur.rm", "Delete a recurring item (posted transactions stay)", rm).write().p(Param::s("item", "id or name").req().pos()),
        OpSpec::new("recur.due", "Upcoming and overdue occurrences", due).p(Param::i("within", "look ahead this many days (default 30)")),
        OpSpec::new("recur.post", "Post due occurrences as transactions (safe to re-run)", post)
            .write()
            .p(Param::s("item", "only this item"))
            .p(Param::date("through", "post everything due up to this date (default today)"))
            .p(Param::s("status", "pending (default) or cleared").choices(&["pending", "cleared"]))
            .p(Param::b("auto_only", "only items marked auto-post (for cron)"))
            .ex("pext recur post --dry-run"),
        OpSpec::new("recur.skip", "Skip upcoming occurrence(s) without posting", skip)
            .write()
            .p(Param::s("item", "id or name").req().pos())
            .p(Param::i("count", "how many (default 1)")),
    ]
}
