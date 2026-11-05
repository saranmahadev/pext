//! IOUs: money lent to or borrowed from people, with partial settlement.

use crate::args::Args;
use crate::ctx::{iv, sv, Ctx};
use crate::ops::accounts::resolve_account;
use crate::ops::categories::collapse_ws;
use crate::ops::fx;
use crate::ops::txns::{check_date, create_txn, fmt_date, NewTxn, SplitIn};
use crate::registry::{money, OpSpec, Param};
use chrono::NaiveDate;
use pext_core::money::{check_amount, decimals_for};
use pext_core::{Error, Result};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub struct Iou {
    pub id: i64,
    pub person: String,
    pub direction: String,
    pub amount: i64,
    pub settled: i64,
    pub currency: String,
    pub date: String,
    pub due: Option<String>,
    pub note: String,
}

impl Iou {
    pub fn outstanding(&self) -> i64 {
        self.amount - self.settled
    }
}

pub fn load_ious(ctx: &Ctx) -> Result<Vec<Iou>> {
    Ok(ctx
        .rows("SELECT * FROM ious ORDER BY date, id", &[])?
        .into_iter()
        .map(|r| Iou {
            id: r["id"].as_i64().unwrap_or(0),
            person: r["person"].as_str().unwrap_or("").into(),
            direction: r["direction"].as_str().unwrap_or("").into(),
            amount: r["amount_minor"].as_i64().unwrap_or(0),
            settled: r["settled_minor"].as_i64().unwrap_or(0),
            currency: r["currency"].as_str().unwrap_or("").into(),
            date: r["date"].as_str().unwrap_or("").into(),
            due: r["due_date"].as_str().map(String::from),
            note: r["note"].as_str().unwrap_or("").into(),
        })
        .collect())
}

fn find(ctx: &Ctx, id: i64) -> Result<Iou> {
    load_ious(ctx)?
        .into_iter()
        .find(|i| i.id == id)
        .ok_or_else(|| {
            Error::not_found("iou_not_found", format!("no IOU #{id}"))
                .with_hint("run `pext iou list --all`")
        })
}

fn item(i: &Iou, today: NaiveDate) -> Value {
    let overdue = i.outstanding() > 0
        && i.due
            .as_deref()
            .and_then(|d| pext_core::dates::parse_date(d).ok())
            .is_some_and(|d| d < today);
    json!({
        "id": i.id, "person": i.person, "direction": i.direction, "currency": i.currency,
        "amount_minor": i.amount, "settled_minor": i.settled, "outstanding_minor": i.outstanding(),
        "date": i.date, "due": i.due, "overdue": overdue, "note": i.note,
        "status": if i.outstanding() == 0 { "settled" } else if i.settled > 0 { "partial" } else { "open" },
    })
}

fn add(ctx: &Ctx, a: &Args) -> Result<Value> {
    let person = collapse_ws(&a.str("person")?);
    if person.is_empty() || person.chars().count() > 64 {
        return Err(Error::validation(
            "invalid_person",
            "the person's name must be 1-64 characters",
        )
        .with_field("person"));
    }
    let currency = match a.opt_str("currency") {
        Some(c) => pext_core::money::validate_currency(&c)?,
        None => ctx.base_currency()?,
    };
    let amount = a.req_money("amount", decimals_for(&currency))?;
    check_amount(amount)?;
    if amount <= 0 {
        return Err(
            Error::validation("invalid_amount", "the amount must be positive").with_field("amount"),
        );
    }
    let date = a.opt_date("date")?.unwrap_or(ctx.today);
    check_date(ctx, date)?;
    let due = a.opt_date("due")?;
    if let Some(d) = due {
        if d < date {
            return Err(
                Error::validation("invalid_date", "the due date is before the IOU date")
                    .with_field("due"),
            );
        }
    }
    let id = ctx.insert(
        "ious",
        &[
            ("person", sv(&person)),
            ("direction", sv(&a.str("direction")?)),
            ("amount_minor", iv(amount)),
            ("currency", sv(&currency)),
            ("date", sv(&fmt_date(date))),
            ("due_date", crate::ctx::osv(due.map(fmt_date).as_deref())),
            ("note", sv(a.opt_str("note").unwrap_or_default().trim())),
            ("created_at", sv(&ctx.now)),
        ],
    )?;
    Ok(item(&find(ctx, id)?, ctx.today))
}

fn list(ctx: &Ctx, a: &Args) -> Result<Value> {
    let person = a.opt_str("person").map(|p| p.to_lowercase());
    let dir = a.opt_str("direction");
    Ok(Value::Array(
        load_ious(ctx)?
            .iter()
            .filter(|i| a.flag("all") || i.outstanding() > 0)
            .filter(|i| {
                person
                    .as_ref()
                    .is_none_or(|p| i.person.to_lowercase().contains(p))
            })
            .filter(|i| dir.as_ref().is_none_or(|d| &i.direction == d))
            .map(|i| item(i, ctx.today))
            .collect(),
    ))
}

fn settle(ctx: &Ctx, a: &Args) -> Result<Value> {
    let i = find(ctx, a.i64("id")?)?;
    let left = i.outstanding();
    if left == 0 {
        return Err(Error::conflict(
            "already_settled",
            format!("IOU #{} is already settled", i.id),
        ));
    }
    let amt = a
        .money("amount", decimals_for(&i.currency))?
        .unwrap_or(left);
    if amt <= 0 {
        return Err(
            Error::validation("invalid_amount", "the amount must be positive").with_field("amount"),
        );
    }
    if amt > left {
        return Err(Error::validation(
            "exceeds_outstanding",
            "that is more than is still outstanding",
        )
        .with_details(json!({ "outstanding_minor": left })));
    }
    let date = a.opt_date("date")?.unwrap_or(ctx.today);
    ctx.update("ious", i.id, &[("settled_minor", iv(i.settled + amt))])?;
    let mut txn_id = Value::Null;
    if let Some(r) = a.opt_str("account") {
        let acct = resolve_account(ctx, &r)?;
        if acct.currency != i.currency {
            return Err(Error::validation(
                "currency_mismatch",
                format!(
                    "account '{}' is in {}, the IOU in {}",
                    acct.name, acct.currency, i.currency
                ),
            ));
        }
        // lent money coming back is income to the account; repaying a debt is an outflow
        let signed = if i.direction == "lent" { amt } else { -amt };
        let id = create_txn(
            ctx,
            NewTxn {
                account: acct,
                date,
                amount_minor: signed,
                payee: i.person.clone(),
                memo: format!("IOU #{} settlement", i.id),
                status: "cleared".into(),
                kind: "standard",
                transfer_id: None,
                external_ref: None,
                fingerprint: None,
                import_batch: None,
                splits: vec![SplitIn {
                    category_id: None,
                    amount_minor: signed,
                    note: String::new(),
                }],
                tags: vec![],
            },
        )?;
        txn_id = json!(id);
    }
    let mut out = item(&find(ctx, i.id)?, ctx.today);
    out["settled_now_minor"] = json!(amt);
    out["transaction_id"] = txn_id;
    Ok(out)
}

fn rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let i = find(ctx, a.i64("id")?)?;
    ctx.delete("ious", i.id)?;
    Ok(json!({ "removed": i.id, "person": i.person }))
}

/// Net position per person, in the base currency.
fn summary(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let base = ctx.base_currency()?;
    let mut by: BTreeMap<String, (i128, i128, usize)> = BTreeMap::new();
    let mut skipped = 0;
    for i in load_ious(ctx)?.iter().filter(|i| i.outstanding() > 0) {
        let Ok(v) = fx::convert(ctx.conn, i.outstanding(), &i.currency, &base, ctx.today) else {
            skipped += 1;
            continue;
        };
        let e = by.entry(i.person.clone()).or_default();
        if i.direction == "lent" {
            e.0 += v as i128;
        } else {
            e.1 += v as i128;
        }
        e.2 += 1;
    }
    if skipped > 0 {
        ctx.warn(
            "fx_missing",
            format!("{skipped} IOU(s) in other currencies were left out: no exchange rate"),
            json!({}),
        );
    }
    let (mut owed_to_me, mut i_owe) = (0i128, 0i128);
    let mut people: Vec<Map<String, Value>> = vec![];
    for (p, (l, b, n)) in by {
        owed_to_me += l;
        i_owe += b;
        let mut m = Map::new();
        m.insert("person".into(), json!(p));
        m.insert("they_owe_me_minor".into(), json!(l as i64));
        m.insert("i_owe_them_minor".into(), json!(b as i64));
        m.insert("net_minor".into(), json!((l - b) as i64));
        m.insert("open_items".into(), json!(n));
        people.push(m);
    }
    people.sort_by_key(|m| std::cmp::Reverse(m["net_minor"].as_i64().unwrap_or(0).abs()));
    Ok(json!({
        "currency": base,
        "owed_to_me_minor": owed_to_me as i64, "i_owe_minor": i_owe as i64, "net_minor": (owed_to_me - i_owe) as i64,
        "people": people,
    }))
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new("iou.add", "Record money you lent or borrowed", add)
            .write()
            .p(Param::s("person", "who").req())
            .p(Param::s(
                "direction",
                "lent (they owe you) or borrowed (you owe them)",
            )
            .req()
            .choices(&["lent", "borrowed"]))
            .ps(money("amount", "amount_minor", "amount"))
            .p(Param::s("currency", "currency (default: base currency)"))
            .p(Param::date("date", "when (default today)"))
            .p(Param::date("due", "agreed repayment date"))
            .p(Param::s("note", "what it was for"))
            .ex("pext iou add --person Ravi --direction lent --amount 5000 --due 2026-12-01"),
        OpSpec::new("iou.list", "List open IOUs", list)
            .p(Param::b("all", "include settled ones"))
            .p(Param::s("person", "filter by name"))
            .p(Param::s("direction", "lent or borrowed").choices(&["lent", "borrowed"])),
        OpSpec::new("iou.settle", "Record a (partial) repayment", settle)
            .write()
            .p(Param::i("id", "IOU id").req().pos())
            .ps(money(
                "amount",
                "amount_minor",
                "amount repaid (default: everything outstanding)",
            ))
            .p(Param::date("date", "repayment date (default today)"))
            .p(Param::s(
                "account",
                "also record the money movement in this account",
            )),
        OpSpec::new("iou.rm", "Delete an IOU", rm)
            .write()
            .p(Param::i("id", "IOU id").req().pos()),
        OpSpec::new("iou.summary", "Who owes what, net per person", summary),
    ]
}
