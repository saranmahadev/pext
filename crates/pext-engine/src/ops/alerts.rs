//! Alerts: things that deserve attention right now. `alert check` is
//! read-only and cron-friendly; it can exit non-zero when something at or above
//! a severity exists (`--fail-on`).

use crate::args::Args;
use crate::ctx::{iv, sv, Ctx};
use crate::ops::accounts::{balance_at, load_account, resolve_account};
use crate::ops::budgets::{load_budgets, status_json, statuses};
use crate::ops::recurring::upcoming;
use crate::ops::reports::{collect, Flow};
use crate::ops::txns::fmt_date;
use crate::registry::{money, OpSpec, Param};
use pext_core::dates::{add_days, add_months, month_end, month_start};
use pext_core::money::format_grouped;
use pext_core::query::Query;
use pext_core::{Error, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const ALERT_EXIT: i64 = 10;

fn sev_rank(s: &str) -> u8 {
    match s {
        "critical" => 3,
        "warning" => 2,
        _ => 1,
    }
}

fn alert(sev: &str, code: &str, message: String, data: Value) -> Value {
    json!({ "severity": sev, "code": code, "message": message, "data": data })
}

fn check(ctx: &Ctx, a: &Args) -> Result<Value> {
    let days = a.opt_i64("days")?.unwrap_or(7).clamp(1, 90);
    let base = ctx.base_currency()?;
    let mut out: Vec<Value> = vec![];

    // Budgets
    let budgets: Vec<_> = load_budgets(ctx)?
        .into_iter()
        .filter(|b| b.period == "monthly")
        .collect();
    let (sts, _) = statuses(ctx, &budgets, ctx.today)?;
    for s in &sts {
        let j = status_json(s, ctx.today);
        let name = &s.b.path;
        match s.state() {
            "over" => out.push(alert(
                "critical",
                "budget_over",
                format!(
                    "{name} is over budget: spent {} of {}",
                    format_grouped(s.spent, &base),
                    format_grouped(s.available, &base)
                ),
                j,
            )),
            "warning" => out.push(alert(
                "warning",
                "budget_near",
                format!(
                    "{name} has used {}% of its budget ({} left)",
                    s.pct_used(),
                    format_grouped(s.remaining(), &base)
                ),
                j,
            )),
            _ => {
                if j["projected_over"] == json!(true) && s.pct_used() >= 30 {
                    out.push(alert(
                        "info",
                        "budget_pace",
                        format!(
                            "{name} is on pace to exceed its budget (projected {})",
                            format_grouped(j["projected_minor"].as_i64().unwrap_or(0), &base)
                        ),
                        j,
                    ));
                }
            }
        }
    }

    // Bills and recurring items
    for (it, due, _) in upcoming(ctx, add_days(ctx.today, 60)?, 3)? {
        let until = (due - ctx.today).num_days();
        let acct = load_account(ctx, it.account_id)?;
        let amt = format_grouped(it.amount_minor.abs(), &acct.currency);
        let data = json!({ "item": it.name, "due": fmt_date(due), "days_until": until, "amount_minor": it.amount_minor, "currency": acct.currency });
        if until < 0 {
            out.push(alert(
                "critical",
                "bill_overdue",
                format!("{} ({amt}) was due {} day(s) ago", it.name, -until),
                data,
            ));
        } else if until <= it.remind_days && it.amount_minor < 0 {
            out.push(alert(
                "warning",
                "bill_due",
                format!(
                    "{} ({amt}) is due {}",
                    it.name,
                    if until == 0 {
                        "today".to_string()
                    } else {
                        format!("in {until} day(s)")
                    }
                ),
                data,
            ));
        }
    }

    // Low balances
    for r in ctx.rows(
        "SELECT account_id, threshold_minor FROM alert_settings WHERE kind='low_balance'",
        &[],
    )? {
        let acct = load_account(ctx, r["account_id"].as_i64().unwrap_or(0))?;
        if acct.archived {
            continue;
        }
        let th = r["threshold_minor"].as_i64().unwrap_or(0);
        let bal = balance_at(ctx, &acct, ctx.today, false)?;
        if bal < th {
            out.push(alert(
                "critical",
                "low_balance",
                format!("{} balance {} is below {}", acct.name, format_grouped(bal, &acct.currency), format_grouped(th, &acct.currency)),
                json!({ "account": acct.name, "balance_minor": bal, "threshold_minor": th, "currency": acct.currency }),
            ));
        }
    }

    // Large transactions
    if let Some(limit) = ctx
        .setting("large_txn_minor")?
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
    {
        let from = add_days(ctx.today, -(days - 1))?;
        let d = collect(ctx, from, ctx.today, &Query::default())?;
        let mut seen = std::collections::HashSet::new();
        for r in d
            .rows
            .iter()
            .filter(|r| r.flow == Flow::Expense && r.amount() >= limit)
        {
            if seen.insert(r.line.txn_id) {
                out.push(alert(
                    "info",
                    "large_transaction",
                    format!("{} on {}: {}", if r.line.payee.is_empty() { "(no payee)" } else { &r.line.payee }, fmt_date(r.line.date), format_grouped(r.amount(), &d.base)),
                    json!({ "txn_id": r.line.txn_id, "amount_minor": r.amount(), "currency": d.base }),
                ));
            }
        }
    }

    // Unusual spending: month-to-date pace vs the average of the previous 3 months, per top-level category
    let this_start = month_start(ctx.today);
    let hist_start = add_months(this_start, -3)?;
    let d = collect(ctx, hist_start, month_end(ctx.today), &Query::default())?;
    let top = |p: &Option<String>| {
        p.as_deref()
            .map(|c| c.split('.').next().unwrap_or(c).to_string())
    };
    let mut hist: BTreeMap<String, i128> = BTreeMap::new();
    let mut cur: BTreeMap<String, i128> = BTreeMap::new();
    for r in d.rows.iter().filter(|r| r.flow == Flow::Expense) {
        let Some(k) = top(&r.line.category) else {
            continue;
        };
        if r.line.date < this_start {
            *hist.entry(k).or_default() += r.amount() as i128;
        } else {
            *cur.entry(k).or_default() += r.amount() as i128;
        }
    }
    let elapsed = i128::from((ctx.today - this_start).num_days() + 1);
    let total_days = i128::from((month_end(ctx.today) - this_start).num_days() + 1);
    if elapsed >= 7 {
        for (k, spent) in cur {
            let avg = hist.get(&k).copied().unwrap_or(0) / 3;
            let projected = spent * total_days / elapsed;
            if avg > 0 && projected * 2 >= avg * 3 && spent * 2 >= avg {
                out.push(alert(
                    "info",
                    "unusual_spend",
                    format!("{k} spending is on pace for {} vs a {} monthly average", format_grouped(projected as i64, &base), format_grouped(avg as i64, &base)),
                    json!({ "category": k, "month_to_date_minor": spent as i64, "projected_minor": projected as i64, "average_minor": avg as i64, "currency": base }),
                ));
            }
        }
    }

    out.sort_by_key(|v| std::cmp::Reverse(sev_rank(v["severity"].as_str().unwrap_or(""))));
    if let Some(level) = a.opt_str("fail_on") {
        let need = sev_rank(&level);
        if out
            .iter()
            .any(|v| sev_rank(v["severity"].as_str().unwrap_or("")) >= need)
        {
            ctx.set_exit_code(ALERT_EXIT as i32);
        }
    }
    Ok(Value::Array(out))
}

fn set(ctx: &Ctx, a: &Args) -> Result<Value> {
    let acct = resolve_account(ctx, &a.str("account")?)?;
    let th = a.req_money("threshold", acct.decimals())?;
    let existing = ctx.opt_i64(
        "SELECT id FROM alert_settings WHERE kind='low_balance' AND account_id=?1",
        &[iv(acct.id)],
    )?;
    match existing {
        Some(id) => ctx.update("alert_settings", id, &[("threshold_minor", iv(th))])?,
        None => {
            ctx.insert(
                "alert_settings",
                &[
                    ("kind", sv("low_balance")),
                    ("account_id", iv(acct.id)),
                    ("threshold_minor", iv(th)),
                ],
            )?;
        }
    }
    Ok(
        json!({ "kind": "low_balance", "account": acct.name, "threshold_minor": th, "currency": acct.currency }),
    )
}

fn list(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let mut out = vec![];
    for r in ctx.rows(
        "SELECT id, kind, account_id, threshold_minor FROM alert_settings ORDER BY id",
        &[],
    )? {
        let acct = load_account(ctx, r["account_id"].as_i64().unwrap_or(0))?;
        out.push(json!({ "id": r["id"], "kind": r["kind"], "account": acct.name, "threshold_minor": r["threshold_minor"], "currency": acct.currency }));
    }
    Ok(Value::Array(out))
}

fn rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let acct = resolve_account(ctx, &a.str("account")?)?;
    let id = ctx
        .opt_i64(
            "SELECT id FROM alert_settings WHERE kind='low_balance' AND account_id=?1",
            &[iv(acct.id)],
        )?
        .ok_or_else(|| {
            Error::not_found(
                "alert_not_found",
                format!("no low-balance alert on '{}'", acct.name),
            )
        })?;
    ctx.delete("alert_settings", id)?;
    Ok(json!({ "removed": id }))
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new(
            "alert.check",
            "Budgets, bills, balances and unusual spending that need attention",
            check,
        )
        .p(Param::i(
            "days",
            "look-back window for large transactions (default 7)",
        ))
        .p(Param::s(
            "fail_on",
            "exit with code 10 if any alert is at least this severe",
        )
        .choices(&["info", "warning", "critical"]))
        .ex("pext alert check --fail-on critical"),
        OpSpec::new(
            "alert.set",
            "Warn when an account balance drops below a threshold",
            set,
        )
        .write()
        .p(Param::s("account", "account").req())
        .ps(money("threshold", "threshold_minor", "minimum balance")),
        OpSpec::new("alert.list", "List configured balance alerts", list),
        OpSpec::new("alert.rm", "Remove a balance alert", rm)
            .write()
            .p(Param::s("account", "account").req().pos()),
    ]
}
