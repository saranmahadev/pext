//! Subscription detection from transaction history.

use crate::args::Args;
use crate::ctx::{iv, sv, Ctx};
use crate::ops::accounts::load_account;
use crate::ops::query::parse_text;
use crate::ops::recurring::{annual_cost, load_items, occurrence};
use crate::ops::txns::{fmt_date, load_matching};
use crate::registry::{OpSpec, Param};
use chrono::NaiveDate;
use pext_core::dates::add_days;
use pext_core::{Error, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct Candidate {
    pub payee: String,
    pub freq: &'static str,
    pub every: i64,
    pub occurrences: usize,
    pub last_date: NaiveDate,
    pub last_amount: i64,
    pub typical_amount: i64,
    pub account_id: i64,
    pub currency: String,
    pub amount_stable: bool,
    pub price_change_pct: Option<i64>,
    pub confidence: i64,
    pub active: bool,
    pub next_expected: NaiveDate,
    pub category: Option<String>,
}

fn median(v: &mut [i64]) -> i64 {
    v.sort_unstable();
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2
    }
}

/// (freq, every, nominal days) for a median gap in days.
fn classify(gap: i64) -> Option<(&'static str, i64, i64)> {
    match gap {
        6..=8 => Some(("weekly", 1, 7)),
        13..=16 => Some(("weekly", 2, 14)),
        27..=34 => Some(("monthly", 1, 30)),
        58..=64 => Some(("monthly", 2, 61)),
        85..=96 => Some(("quarterly", 1, 91)),
        175..=190 => Some(("monthly", 6, 182)),
        350..=380 => Some(("yearly", 1, 365)),
        _ => None,
    }
}

pub fn detect_candidates(ctx: &Ctx, min_occ: usize) -> Result<Vec<Candidate>> {
    let q = parse_text(ctx, "type:expense")?;
    let lines = load_matching(ctx, &q)?;
    let mut by_payee: BTreeMap<String, Vec<_>> = BTreeMap::new();
    for l in lines {
        if !l.payee.is_empty() {
            by_payee.entry(l.payee.to_lowercase()).or_default().push(l);
        }
    }
    let mut out = vec![];
    for (_, mut ls) in by_payee {
        ls.sort_by_key(|l| (l.date, l.txn_id));
        ls.dedup_by_key(|l| l.txn_id);
        // one charge per day
        ls.dedup_by_key(|l| l.date);
        if ls.len() < min_occ {
            continue;
        }
        let gaps: Vec<i64> = ls
            .windows(2)
            .map(|w| (w[1].date - w[0].date).num_days())
            .collect();
        let med = median(&mut gaps.clone());
        let Some((freq, every, nominal)) = classify(med) else {
            continue;
        };
        let tol = (nominal / 5).max(2);
        let regular = gaps.iter().filter(|g| (**g - nominal).abs() <= tol).count();
        if regular * 100 / gaps.len() < 70 {
            continue;
        }

        let mut amts: Vec<i64> = ls.iter().map(|l| l.txn_amount_minor.abs()).collect();
        let typical = median(&mut amts);
        let stable_n = ls
            .iter()
            .filter(|l| (l.txn_amount_minor.abs() - typical).abs() * 10 <= typical)
            .count();
        let stable = stable_n * 100 / ls.len() >= 70;
        let last = &ls[ls.len() - 1];
        let prev_diff = ls
            .iter()
            .rev()
            .skip(1)
            .find(|l| l.txn_amount_minor != last.txn_amount_minor);
        let price_change = prev_diff.and_then(|p| {
            let (pa, la) = (
                p.txn_amount_minor.abs() as i128,
                last.txn_amount_minor.abs() as i128,
            );
            if pa == 0 {
                None
            } else {
                Some(((la - pa) * 100 / pa) as i64)
            }
        });
        let mut next = occurrence(last.date, freq, every, 1)?;
        let active = add_days(next, nominal / 2)? >= ctx.today;
        if next < ctx.today && active {
            // catch up if a charge is merely a few days late
            next = ctx.today;
        }
        let confidence = ((regular * 100 / gaps_len(&ls)) as i64 / 2
            + if stable { 30 } else { 0 }
            + (ls.len() as i64 * 4).min(20))
        .min(99);
        out.push(Candidate {
            payee: last.payee.clone(),
            freq,
            every,
            occurrences: ls.len(),
            last_date: last.date,
            last_amount: last.txn_amount_minor.abs(),
            typical_amount: typical,
            account_id: last.account_id,
            currency: last.currency.clone(),
            amount_stable: stable,
            price_change_pct: price_change.filter(|p| *p != 0),
            confidence,
            active,
            next_expected: next,
            category: last.category.clone(),
        });
    }
    out.sort_by_key(|c| std::cmp::Reverse(annual_cost(c.last_amount, c.freq, c.every)));
    Ok(out)
}

fn gaps_len(ls: &[pext_core::query::Line]) -> usize {
    ls.len().saturating_sub(1).max(1)
}

fn detect(ctx: &Ctx, a: &Args) -> Result<Value> {
    let min = a.opt_i64("min_occurrences")?.unwrap_or(3).clamp(2, 50) as usize;
    let tracked: Vec<String> = load_items(ctx, None, true)?
        .into_iter()
        .map(|i| i.payee.to_lowercase())
        .collect();
    let rows: Vec<Value> = detect_candidates(ctx, min)?
        .into_iter()
        .filter(|c| a.flag("include_lapsed") || c.active)
        .map(|c| {
            let tracked_already = tracked.contains(&c.payee.to_lowercase());
            json!({
                "payee": c.payee, "frequency": c.freq, "every": c.every, "occurrences": c.occurrences,
                "last_date": fmt_date(c.last_date), "last_amount_minor": c.last_amount, "typical_amount_minor": c.typical_amount,
                "currency": c.currency, "annual_minor": annual_cost(c.last_amount, c.freq, c.every),
                "amount_stable": c.amount_stable, "price_change_pct": c.price_change_pct, "confidence": c.confidence,
                "status": if c.active { "active" } else { "lapsed" }, "next_expected": fmt_date(c.next_expected),
                "tracked": tracked_already,
                "command": if tracked_already { Value::Null } else { json!(format!("pext sub add '{}'", c.payee)) },
            })
        })
        .collect();
    Ok(Value::Array(rows))
}

fn add(ctx: &Ctx, a: &Args) -> Result<Value> {
    let payee = a.str("payee")?;
    let cands = detect_candidates(ctx, 2)?;
    let c = cands
        .into_iter()
        .find(|c| c.payee.eq_ignore_ascii_case(payee.trim()))
        .ok_or_else(|| {
            Error::not_found(
                "subscription_not_found",
                format!("no recurring charges found for '{payee}'"),
            )
            .with_hint("run `pext sub detect`")
        })?;
    let acct = load_account(ctx, c.account_id)?;
    let name = a.opt_str("name").unwrap_or_else(|| c.payee.clone());
    let cat = match &c.category {
        Some(p) => ctx.opt_i64("SELECT id FROM categories WHERE path=?1", &[sv(p)])?,
        None => None,
    };
    let id = ctx.insert(
        "recurring",
        &[
            ("name", sv(&name)),
            ("kind", sv("subscription")),
            ("account_id", iv(acct.id)),
            ("amount_minor", iv(-c.last_amount)),
            ("category_id", crate::ctx::oiv(cat)),
            ("payee", sv(&c.payee)),
            ("freq", sv(c.freq)),
            ("every", iv(c.every)),
            ("start_date", sv(&fmt_date(c.next_expected))),
            ("next_index", iv(0)),
            ("next_due", sv(&fmt_date(c.next_expected))),
            ("auto_post", iv(0)),
            ("remind_days", iv(3)),
            ("active", iv(1)),
            ("created_at", sv(&ctx.now)),
        ],
    )?;
    Ok(
        json!({ "id": id, "name": name, "amount_minor": -c.last_amount, "currency": acct.currency, "freq": c.freq, "every": c.every, "next_due": fmt_date(c.next_expected) }),
    )
}

fn list(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let mut rows = vec![];
    let mut total: BTreeMap<String, i128> = BTreeMap::new();
    for it in load_items(ctx, None, false)?
        .into_iter()
        .filter(|i| i.kind == "subscription")
    {
        let acct = load_account(ctx, it.account_id)?;
        let annual = annual_cost(it.amount_minor.abs(), &it.freq, it.every);
        *total.entry(acct.currency.clone()).or_default() += annual as i128;
        rows.push(json!({
            "id": it.id, "name": it.name, "amount_minor": it.amount_minor.abs(), "currency": acct.currency,
            "freq": it.freq, "every": it.every, "next_due": fmt_date(it.next_due), "annual_minor": annual,
            "monthly_minor": annual / 12,
        }));
    }
    for (cur, t) in total {
        rows.push(json!({ "name": "TOTAL", "currency": cur, "annual_minor": t as i64, "monthly_minor": (t / 12) as i64 }));
    }
    Ok(Value::Array(rows))
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new(
            "sub.detect",
            "Find recurring charges in your history (subscriptions, bills)",
            detect,
        )
        .p(Param::i(
            "min_occurrences",
            "minimum charges to consider (default 3)",
        ))
        .p(Param::b(
            "include_lapsed",
            "also show charges that seem to have stopped",
        )),
        OpSpec::new(
            "sub.add",
            "Track a detected subscription as a recurring item",
            add,
        )
        .write()
        .p(Param::s("payee", "payee as shown by `sub detect`")
            .req()
            .pos())
        .p(Param::s("name", "name for the recurring item")),
        OpSpec::new(
            "sub.list",
            "Tracked subscriptions with monthly and annual cost",
            list,
        ),
    ]
}
