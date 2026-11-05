//! Net worth (accounts + investments + manual assets - loans +/- IOUs),
//! dated snapshots, cash-flow forecast and runway.

use crate::args::Args;
use crate::ctx::{iv, sv, Ctx};
use crate::ops::accounts::{balance_at, load_account, LIQUID_KINDS};
use crate::ops::assets::{load_assets, value_on};
use crate::ops::fx;
use crate::ops::invest::holdings;
use crate::ops::ious::load_ious;
use crate::ops::loans::load_loans;
use crate::ops::recurring::upcoming;
use crate::ops::reports::{collect, Flow};
use crate::ops::txns::fmt_date;
use crate::registry::{OpSpec, Param};
use chrono::NaiveDate;
use pext_core::dates::{add_days, add_months, month_end, month_start};
use pext_core::money::mul_div_round;
use pext_core::query::Query;
use pext_core::{Error, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

struct Item {
    group: &'static str,
    name: String,
    kind: String,
    currency: String,
    native_minor: i64,
    base_minor: i64,
}

fn gather(ctx: &Ctx) -> Result<(Vec<Item>, String, Vec<String>)> {
    let base = ctx.base_currency()?;
    let mut items: Vec<Item> = vec![];
    let mut missing: Vec<String> = vec![];
    let today = ctx.today;
    let push =
        |items: &mut Vec<Item>,
         group: &'static str,
         name: &str,
         kind: &str,
         cur: &str,
         native: i64,
         ctx: &Ctx,
         missing: &mut Vec<String>| match fx::convert(ctx.conn, native, cur, &base, today) {
            Ok(b) => items.push(Item {
                group,
                name: name.into(),
                kind: kind.into(),
                currency: cur.into(),
                native_minor: native,
                base_minor: b,
            }),
            Err(_) => missing.push(name.to_string()),
        };

    for r in ctx.rows("SELECT id FROM accounts ORDER BY name", &[])? {
        let acc = load_account(ctx, r["id"].as_i64().unwrap_or(0))?;
        if let Some(scope) = &ctx.actor.accounts {
            if !scope.contains(&acc.id) {
                continue;
            }
        }
        let bal = balance_at(ctx, &acc, today, false)?;
        if acc.archived && bal == 0 {
            continue;
        }
        push(
            &mut items,
            "accounts",
            &acc.name,
            &acc.kind,
            &acc.currency,
            bal,
            ctx,
            &mut missing,
        );
    }
    if ctx.actor.accounts.is_none() {
        let (hs, _, _) = holdings(ctx)?;
        for h in hs.iter().filter(|h| h.pos.qty_e8 > 0) {
            if let Some(v) = h.value {
                push(
                    &mut items,
                    "investments",
                    &h.inst.symbol,
                    &h.inst.kind,
                    &h.inst.currency,
                    v,
                    ctx,
                    &mut missing,
                );
            }
        }
        for a in load_assets(ctx, false)? {
            if let Some((_, v)) = value_on(ctx, a.id, today)? {
                push(
                    &mut items,
                    "assets",
                    &a.name,
                    &a.kind,
                    &a.currency,
                    v,
                    ctx,
                    &mut missing,
                );
            }
        }
        for l in load_loans(ctx, false)?
            .iter()
            .filter(|l| l.account_id.is_none())
        {
            let out = l.scheduled_outstanding(today)?;
            if out > 0 {
                push(
                    &mut items,
                    "loans",
                    &l.name,
                    &l.kind,
                    &l.currency,
                    -out,
                    ctx,
                    &mut missing,
                );
            }
        }
        for i in load_ious(ctx)?.iter().filter(|i| i.outstanding() > 0) {
            let v = if i.direction == "lent" {
                i.outstanding()
            } else {
                -i.outstanding()
            };
            push(
                &mut items,
                "ious",
                &i.person,
                &i.direction,
                &i.currency,
                v,
                ctx,
                &mut missing,
            );
        }
    }
    Ok((items, base, missing))
}

fn totals(items: &[Item]) -> (i128, i128) {
    let a: i128 = items
        .iter()
        .filter(|i| i.base_minor > 0)
        .map(|i| i.base_minor as i128)
        .sum();
    let l: i128 = items
        .iter()
        .filter(|i| i.base_minor < 0)
        .map(|i| -(i.base_minor as i128))
        .sum();
    (a, l)
}

fn build(ctx: &Ctx) -> Result<Value> {
    let (items, base, missing) = gather(ctx)?;
    if !missing.is_empty() {
        ctx.warn(
            "fx_missing",
            format!(
                "{} item(s) in other currencies are left out: no exchange rate",
                missing.len()
            ),
            json!({ "items": missing }),
        );
    }
    let (assets, liabilities) = totals(&items);
    let mut groups: BTreeMap<&str, (i128, i128)> = BTreeMap::new();
    for i in &items {
        let g = groups.entry(i.group).or_default();
        if i.base_minor > 0 {
            g.0 += i.base_minor as i128;
        } else {
            g.1 += -(i.base_minor as i128);
        }
    }
    let order = ["accounts", "investments", "assets", "loans", "ious"];
    let groups_json: Vec<Value> = order
        .iter()
        .filter_map(|g| groups.get(g).map(|(a, l)| json!({ "group": g, "assets_minor": *a as i64, "liabilities_minor": *l as i64, "net_minor": (*a - *l) as i64 })))
        .collect();
    let mut sorted: Vec<&Item> = items.iter().collect();
    sorted.sort_by_key(|i| {
        (
            order.iter().position(|g| *g == i.group).unwrap_or(9),
            std::cmp::Reverse(i.base_minor.abs()),
        )
    });
    Ok(json!({
        "currency": base, "date": fmt_date(ctx.today),
        "assets_minor": assets as i64, "liabilities_minor": liabilities as i64, "net_worth_minor": (assets - liabilities) as i64,
        "groups": groups_json,
        "items": sorted.iter().map(|i| json!({ "group": i.group, "name": i.name, "kind": i.kind, "currency": i.currency, "value_minor": i.native_minor, "value_base_minor": i.base_minor })).collect::<Vec<_>>(),
    }))
}

fn networth(ctx: &Ctx, _a: &Args) -> Result<Value> {
    build(ctx)
}

fn snapshot_take(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let nw = build(ctx)?;
    let detail = json!({ "groups": nw["groups"], "items": nw["items"] }).to_string();
    let date = fmt_date(ctx.today);
    let cols = [
        ("currency", sv(nw["currency"].as_str().unwrap_or(""))),
        ("assets_minor", iv(nw["assets_minor"].as_i64().unwrap_or(0))),
        (
            "liabilities_minor",
            iv(nw["liabilities_minor"].as_i64().unwrap_or(0)),
        ),
        (
            "net_worth_minor",
            iv(nw["net_worth_minor"].as_i64().unwrap_or(0)),
        ),
        ("detail", sv(&detail)),
    ];
    let replaced = match ctx.opt_i64("SELECT id FROM snapshots WHERE date=?1", &[sv(&date)])? {
        Some(id) => {
            ctx.update("snapshots", id, &cols)?;
            true
        }
        None => {
            let mut all = vec![("date", sv(&date)), ("created_at", sv(&ctx.now))];
            all.extend(cols);
            ctx.insert("snapshots", &all)?;
            false
        }
    };
    Ok(
        json!({ "date": date, "currency": nw["currency"], "net_worth_minor": nw["net_worth_minor"], "assets_minor": nw["assets_minor"], "liabilities_minor": nw["liabilities_minor"], "replaced_existing": replaced }),
    )
}

fn snapshot_list(ctx: &Ctx, a: &Args) -> Result<Value> {
    let limit = a.opt_i64("limit")?.unwrap_or(24).clamp(1, 1000);
    let mut rows = ctx.rows("SELECT date, currency, assets_minor, liabilities_minor, net_worth_minor FROM snapshots ORDER BY date DESC LIMIT ?1", &[iv(limit + 1)])?;
    let mut out = vec![];
    for k in 0..rows.len().min(limit as usize) {
        let prev = rows.get(k + 1).and_then(|p| p["net_worth_minor"].as_i64());
        let nw = rows[k]["net_worth_minor"].as_i64().unwrap_or(0);
        let mut m = std::mem::take(&mut rows[k]);
        m.insert("change_minor".into(), json!(prev.map(|p| nw - p)));
        m.insert(
            "change_pct".into(),
            match prev {
                Some(p) if p > 0 => {
                    json!(mul_div_round((nw - p) as i128 * 100, 1, p as i128)? as i64)
                }
                _ => Value::Null,
            },
        );
        out.push(Value::Object(m));
    }
    Ok(Value::Array(out))
}

fn snapshot_rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let d = a.date("date")?;
    let id = ctx
        .opt_i64(
            "SELECT id FROM snapshots WHERE date=?1",
            &[sv(&fmt_date(d))],
        )?
        .ok_or_else(|| {
            Error::not_found(
                "snapshot_not_found",
                format!("no snapshot on {}", fmt_date(d)),
            )
        })?;
    ctx.delete("snapshots", id)?;
    Ok(json!({ "removed": fmt_date(d) }))
}

// ---- forecast & runway ------------------------------------------------------------

fn liquid_accounts(ctx: &Ctx, only: &[String]) -> Result<Vec<crate::ops::accounts::AccountRow>> {
    let mut out = vec![];
    for r in ctx.rows(
        "SELECT id FROM accounts WHERE archived=0 ORDER BY name",
        &[],
    )? {
        let acc = load_account(ctx, r["id"].as_i64().unwrap_or(0))?;
        if let Some(scope) = &ctx.actor.accounts {
            if !scope.contains(&acc.id) {
                continue;
            }
        }
        let chosen = if only.is_empty() {
            LIQUID_KINDS.contains(&acc.kind.as_str())
        } else {
            only.iter()
                .any(|o| o.eq_ignore_ascii_case(&acc.name) || o.parse::<i64>().ok() == Some(acc.id))
        };
        if chosen {
            out.push(acc);
        }
    }
    if !only.is_empty() && out.len() < only.len() {
        return Err(Error::not_found(
            "account_not_found",
            "one of the accounts was not found or is archived",
        ));
    }
    Ok(out)
}

fn forecast(ctx: &Ctx, a: &Args) -> Result<Value> {
    let days = a.opt_i64("days")?.unwrap_or(90).clamp(1, 730);
    let base = ctx.base_currency()?;
    let accts = liquid_accounts(ctx, &a.list("account"))?;
    if accts.is_empty() {
        return Err(Error::not_found(
            "no_accounts",
            "no checking, savings or cash accounts to forecast",
        )
        .with_hint("pass --account to choose accounts"));
    }
    let ids: Vec<i64> = accts.iter().map(|x| x.id).collect();
    let end = add_days(ctx.today, days)?;
    let mut start: i128 = 0;
    let mut missing = 0;
    for acc in &accts {
        match fx::convert(
            ctx.conn,
            balance_at(ctx, acc, ctx.today, false)?,
            &acc.currency,
            &base,
            ctx.today,
        ) {
            Ok(v) => start += v as i128,
            Err(_) => missing += 1,
        }
    }
    // dated events: (date, label, base amount)
    let mut events: Vec<(NaiveDate, String, i64, bool)> = vec![];
    for (it, due, _) in upcoming(ctx, end, 400)? {
        if !ids.contains(&it.account_id) {
            continue;
        }
        let acc = accts
            .iter()
            .find(|x| x.id == it.account_id)
            .expect("filtered");
        let overdue = due < ctx.today;
        let when = if overdue { ctx.today } else { due };
        match fx::convert(ctx.conn, it.amount_minor, &acc.currency, &base, ctx.today) {
            Ok(v) => events.push((when, it.name.clone(), v, overdue)),
            Err(_) => missing += 1,
        }
    }
    // transactions already in the ledger but dated in the future
    for r in ctx.rows("SELECT t.date, t.payee, t.amount_minor, a.currency, t.account_id FROM transactions t JOIN accounts a ON a.id=t.account_id WHERE t.deleted_at IS NULL AND t.date>?1 AND t.date<=?2", &[sv(&fmt_date(ctx.today)), sv(&fmt_date(end))])? {
        if !ids.contains(&r["account_id"].as_i64().unwrap_or(0)) {
            continue;
        }
        let d = crate::ops::accounts::parse_stored_date(r["date"].as_str().unwrap_or(""))?;
        match fx::convert(ctx.conn, r["amount_minor"].as_i64().unwrap_or(0), r["currency"].as_str().unwrap_or(""), &base, ctx.today) {
            Ok(v) => events.push((d, r["payee"].as_str().filter(|p| !p.is_empty()).unwrap_or("scheduled transaction").to_string(), v, false)),
            Err(_) => missing += 1,
        }
    }
    if missing > 0 {
        ctx.warn(
            "fx_missing",
            format!("{missing} item(s) in other currencies are left out: no exchange rate"),
            json!({}),
        );
    }
    events.sort_by_key(|e| (e.0, e.1.clone()));
    // walk day by day
    let mut bal = start;
    let (mut min_bal, mut min_date) = (start, ctx.today);
    let mut first_neg: Option<NaiveDate> = None;
    let mut neg_days = 0;
    let mut weekly = vec![json!({ "date": fmt_date(ctx.today), "balance_minor": start as i64 })];
    let mut shown = vec![];
    let mut idx = 0;
    let limit = a.opt_i64("events")?.unwrap_or(30).clamp(0, 1000) as usize;
    for k in 0..=days {
        let d = add_days(ctx.today, k)?;
        while idx < events.len() && events[idx].0 == d {
            bal += events[idx].2 as i128;
            if shown.len() < limit {
                shown.push(json!({ "date": fmt_date(d), "name": events[idx].1, "amount_minor": events[idx].2, "balance_after_minor": bal as i64, "was_overdue": events[idx].3 }));
            }
            idx += 1;
        }
        if bal < min_bal {
            min_bal = bal;
            min_date = d;
        }
        if bal < 0 {
            neg_days += 1;
            first_neg.get_or_insert(d);
        }
        if k > 0 && (k % 7 == 0 || k == days) {
            weekly.push(json!({ "date": fmt_date(d), "balance_minor": bal as i64 }));
        }
    }
    let inflow: i128 = events.iter().filter(|e| e.2 > 0).map(|e| e.2 as i128).sum();
    let outflow: i128 = events
        .iter()
        .filter(|e| e.2 < 0)
        .map(|e| -(e.2 as i128))
        .sum();
    Ok(json!({
        "currency": base, "from": fmt_date(ctx.today), "to": fmt_date(end), "accounts": accts.iter().map(|x| x.name.clone()).collect::<Vec<_>>(),
        "start_minor": start as i64, "end_minor": bal as i64, "expected_inflow_minor": inflow as i64, "expected_outflow_minor": outflow as i64,
        "lowest_minor": min_bal as i64, "lowest_on": fmt_date(min_date),
        "goes_negative": first_neg.is_some(), "first_negative_on": first_neg.map(fmt_date), "negative_days": neg_days,
        "weekly": weekly, "events": shown, "events_total": events.len(),
    }))
}

fn runway(ctx: &Ctx, a: &Args) -> Result<Value> {
    let months = a.opt_i64("months")?.unwrap_or(3).clamp(1, 24) as i32;
    let base = ctx.base_currency()?;
    let accts = liquid_accounts(ctx, &a.list("account"))?;
    let mut liquid: i128 = 0;
    for acc in &accts {
        if let Ok(v) = fx::convert(
            ctx.conn,
            balance_at(ctx, acc, ctx.today, false)?,
            &acc.currency,
            &base,
            ctx.today,
        ) {
            liquid += v as i128;
        }
    }
    let to = add_days(month_start(ctx.today), -1)?;
    let from = add_months(month_start(ctx.today), -months)?;
    let d = collect(ctx, from, month_end(to), &Query::default())?;
    let spent: i128 = d
        .rows
        .iter()
        .filter(|r| r.flow == Flow::Expense)
        .map(|r| r.amount() as i128)
        .sum();
    let income: i128 = d
        .rows
        .iter()
        .filter(|r| r.flow == Flow::Income)
        .map(|r| r.amount() as i128)
        .sum();
    let n = months as i128;
    let burn = spent / n;
    let (days, months_txt) = if burn > 0 && liquid > 0 {
        let days = mul_div_round(liquid, 30, burn)?;
        let tenths = mul_div_round(liquid * 10, 1, burn)?;
        (
            Some(days as i64),
            Some(format!("{}.{}", tenths / 10, tenths % 10)),
        )
    } else if burn > 0 {
        (Some(0), Some("0.0".to_string()))
    } else {
        (None, None)
    };
    Ok(json!({
        "currency": base, "window": format!("{} to {}", fmt_date(from), fmt_date(month_end(to))), "months_in_window": months,
        "liquid_minor": liquid as i64, "avg_monthly_spend_minor": burn as i64, "avg_monthly_income_minor": (income / n) as i64,
        "runway_days": days, "runway_months": months_txt,
        "accounts": accts.iter().map(|x| x.name.clone()).collect::<Vec<_>>(),
    }))
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new("networth", "Net worth in the base currency: accounts, investments, assets, loans and IOUs (brokerage/crypto accounts count at ledger balance; do not also track them as instruments)", networth),
        OpSpec::new("snapshot.take", "Save today's net worth (run monthly to build a history)", snapshot_take).write(),
        OpSpec::new("snapshot.list", "Net-worth history from saved snapshots", snapshot_list).p(Param::i("limit", "maximum rows (default 24)")),
        OpSpec::new("snapshot.rm", "Delete a snapshot", snapshot_rm).write().p(Param::date("date", "snapshot date").req().pos()),
        OpSpec::new("cashflow.forecast", "Project the balance of your liquid accounts from bills, income and dated entries", forecast)
            .p(Param::i("days", "days ahead (default 90, max 730)"))
            .p(Param::s("account", "accounts to include (default: checking, savings, cash, wallet)").multi())
            .p(Param::i("events", "how many upcoming events to list (default 30)"))
            .ex("pext cashflow forecast --days 60"),
        OpSpec::new("runway", "How long your liquid money lasts at your recent spending", runway)
            .p(Param::i("months", "months of history to average (default 3)"))
            .p(Param::s("account", "accounts to include (default: checking, savings, cash, wallet)").multi()),
    ]
}
