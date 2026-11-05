//! Savings goals, either funded by manual contributions or tracking the
//! balance of a linked account.

use crate::args::Args;
use crate::ctx::{iv, osv, sv, Ctx};
use crate::ops::accounts::{balance_at, resolve_account};
use crate::ops::categories::collapse_ws;
use crate::ops::txns::fmt_date;
use crate::registry::{money, OpSpec, Param};
use chrono::NaiveDate;
use pext_core::dates::{add_days, add_months};
use pext_core::money::{check_amount, decimals_for, mul_div_round};
use pext_core::{Error, Result};
use serde_json::{json, Value};

struct Goal {
    id: i64,
    name: String,
    target: i64,
    currency: String,
    target_date: Option<NaiveDate>,
    account_id: Option<i64>,
    archived: bool,
}

fn load(ctx: &Ctx, only: Option<&str>, include_archived: bool) -> Result<Vec<Goal>> {
    let rows = ctx.rows("SELECT * FROM goals ORDER BY name", &[])?;
    let mut out = vec![];
    for r in rows {
        let g = Goal {
            id: r["id"].as_i64().unwrap_or(0),
            name: r["name"].as_str().unwrap_or("").into(),
            target: r["target_minor"].as_i64().unwrap_or(0),
            currency: r["currency"].as_str().unwrap_or("").into(),
            target_date: match r["target_date"].as_str() {
                Some(s) => Some(crate::ops::accounts::parse_stored_date(s)?),
                None => None,
            },
            account_id: r["account_id"].as_i64(),
            archived: r["archived"].as_i64() == Some(1),
        };
        if g.archived && !include_archived {
            continue;
        }
        if let Some(o) = only {
            if !(g.name.eq_ignore_ascii_case(o.trim())
                || o.trim().parse::<i64>().ok() == Some(g.id))
            {
                continue;
            }
        }
        out.push(g);
    }
    Ok(out)
}

fn find(ctx: &Ctx, r: &str) -> Result<Goal> {
    load(ctx, Some(r), true)?.into_iter().next().ok_or_else(|| {
        Error::not_found("goal_not_found", format!("no goal '{r}'"))
            .with_hint("run `pext goal list`")
    })
}

fn saved(ctx: &Ctx, g: &Goal) -> Result<i64> {
    match g.account_id {
        Some(acc) => {
            let a = crate::ops::accounts::load_account(ctx, acc)?;
            balance_at(ctx, &a, ctx.today, false)
        }
        None => ctx.one_i64(
            "SELECT COALESCE(SUM(amount_minor),0) FROM goal_contributions WHERE goal_id=?1",
            &[iv(g.id)],
        ),
    }
}

fn months_ceil(days: i64) -> i64 {
    // ceil(days / 30.4375)
    ((days.max(0) as i128 * 10_000 + 304_374) / 304_375).max(1) as i64
}

fn status_json(ctx: &Ctx, g: &Goal) -> Result<Value> {
    let saved = saved(ctx, g)?.max(0);
    let remaining = (g.target - saved).max(0);
    let pct = mul_div_round(saved as i128, 100, g.target as i128)? as i64;
    // average monthly contribution over the last 90 days (manual goals only)
    let since = add_days(ctx.today, -90)?;
    let avg = if g.account_id.is_none() {
        let s = ctx.one_i64(
            "SELECT COALESCE(SUM(amount_minor),0) FROM goal_contributions WHERE goal_id=?1 AND date>=?2 AND date<=?3",
            &[iv(g.id), sv(&fmt_date(since)), sv(&fmt_date(ctx.today))],
        )?;
        Some(s / 3)
    } else {
        None
    };
    let required = g.target_date.map(|d| {
        let months = months_ceil((d - ctx.today).num_days());
        if remaining == 0 {
            0
        } else {
            (remaining + months - 1) / months
        }
    });
    let projected = match avg {
        Some(a) if a > 0 && remaining > 0 => {
            let months = (remaining + a - 1) / a;
            add_months(ctx.today, i32::try_from(months).unwrap_or(i32::MAX)).ok()
        }
        _ => None,
    };
    let state = if remaining == 0 {
        "complete"
    } else if let Some(d) = g.target_date {
        if d < ctx.today {
            "overdue"
        } else {
            match (avg, required) {
                (Some(a), Some(r)) if a >= r => "on_track",
                (Some(_), Some(_)) => "behind",
                _ => "in_progress",
            }
        }
    } else {
        "in_progress"
    };
    Ok(json!({
        "id": g.id, "name": g.name, "currency": g.currency,
        "target_minor": g.target, "saved_minor": saved, "remaining_minor": remaining, "pct": pct,
        "target_date": g.target_date.map(fmt_date),
        "days_left": g.target_date.map(|d| (d - ctx.today).num_days()),
        "required_monthly_minor": required, "avg_monthly_minor": avg,
        "projected_completion": projected.map(fmt_date),
        "linked_account": match g.account_id { Some(a) => Some(crate::ops::accounts::load_account(ctx, a)?.name), None => None },
        "state": state, "archived": g.archived,
    }))
}

fn add(ctx: &Ctx, a: &Args) -> Result<Value> {
    let name = collapse_ws(&a.str("name")?);
    let (currency, account) = match a.opt_str("account") {
        Some(r) => {
            let acc = resolve_account(ctx, &r)?;
            if acc.is_liability() {
                return Err(Error::validation(
                    "invalid_goal_account",
                    "a savings goal cannot track a liability account",
                )
                .with_field("account"));
            }
            (acc.currency.clone(), Some(acc.id))
        }
        None => (ctx.base_currency()?, None),
    };
    let target = a.req_money("target", decimals_for(&currency))?;
    check_amount(target)?;
    if target <= 0 {
        return Err(
            Error::validation("invalid_amount", "the target must be positive").with_field("target"),
        );
    }
    let by = a.opt_date("by")?;
    if by.is_some_and(|d| d < ctx.today) {
        return Err(
            Error::validation("invalid_date", "the target date is in the past").with_field("by"),
        );
    }
    let id = ctx.insert(
        "goals",
        &[
            ("name", sv(&name)),
            ("target_minor", iv(target)),
            ("currency", sv(&currency)),
            ("target_date", osv(by.map(fmt_date).as_deref())),
            ("account_id", crate::ctx::oiv(account)),
            ("created_at", sv(&ctx.now)),
        ],
    )?;
    status_json(ctx, &find(ctx, &id.to_string())?)
}

fn contribute_impl(ctx: &Ctx, a: &Args, sign: i64) -> Result<Value> {
    let g = find(ctx, &a.str("goal")?)?;
    if g.account_id.is_some() {
        return Err(Error::validation(
            "goal_is_linked",
            "this goal follows an account balance; move money in that account instead",
        )
        .with_field("goal"));
    }
    if g.archived {
        return Err(Error::conflict("goal_archived", "this goal is archived"));
    }
    let amt = a.req_money("amount", decimals_for(&g.currency))?;
    if amt <= 0 {
        return Err(
            Error::validation("invalid_amount", "enter a positive amount").with_field("amount"),
        );
    }
    check_amount(amt)?;
    if sign < 0 && amt > saved(ctx, &g)? {
        return Err(Error::validation(
            "insufficient_goal_balance",
            "cannot withdraw more than has been saved",
        )
        .with_field("amount"));
    }
    let date = a.opt_date("date")?.unwrap_or(ctx.today);
    ctx.insert(
        "goal_contributions",
        &[
            ("goal_id", iv(g.id)),
            ("date", sv(&fmt_date(date))),
            ("amount_minor", iv(sign * amt)),
            ("note", sv(a.opt_str("note").unwrap_or_default().trim())),
            ("created_at", sv(&ctx.now)),
        ],
    )?;
    status_json(ctx, &g)
}

fn contribute(ctx: &Ctx, a: &Args) -> Result<Value> {
    contribute_impl(ctx, a, 1)
}
fn withdraw(ctx: &Ctx, a: &Args) -> Result<Value> {
    contribute_impl(ctx, a, -1)
}

fn status(ctx: &Ctx, a: &Args) -> Result<Value> {
    let goals = match a.opt_str("goal") {
        Some(r) => vec![find(ctx, &r)?],
        None => load(ctx, None, a.flag("all"))?,
    };
    goals
        .iter()
        .map(|g| status_json(ctx, g))
        .collect::<Result<Vec<_>>>()
        .map(Value::Array)
}

fn history(ctx: &Ctx, a: &Args) -> Result<Value> {
    let g = find(ctx, &a.str("goal")?)?;
    let rows = ctx.rows(
        "SELECT id, date, amount_minor, note FROM goal_contributions WHERE goal_id=?1 ORDER BY date DESC, id DESC",
        &[iv(g.id)],
    )?;
    Ok(Value::Array(
        rows.into_iter()
            .map(|mut r| {
                r.insert("currency".into(), json!(g.currency));
                Value::Object(r)
            })
            .collect(),
    ))
}

fn edit(ctx: &Ctx, a: &Args) -> Result<Value> {
    let g = find(ctx, &a.str("goal")?)?;
    let mut cols = vec![];
    if let Some(n) = a.opt_str("new_name") {
        cols.push(("name", sv(&collapse_ws(&n))));
    }
    if let Some(t) = a.money("target", decimals_for(&g.currency))? {
        if t <= 0 {
            return Err(
                Error::validation("invalid_amount", "the target must be positive")
                    .with_field("target"),
            );
        }
        cols.push(("target_minor", iv(t)));
    }
    if let Some(d) = a.opt_date("by")? {
        cols.push(("target_date", sv(&fmt_date(d))));
    }
    if a.flag("archive") {
        cols.push(("archived", iv(1)));
    }
    if a.flag("unarchive") {
        cols.push(("archived", iv(0)));
    }
    if cols.is_empty() {
        return Err(Error::usage(
            "nothing_to_change",
            "pass at least one field to change",
        ));
    }
    ctx.update("goals", g.id, &cols)?;
    status_json(ctx, &find(ctx, &g.id.to_string())?)
}

fn rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let g = find(ctx, &a.str("goal")?)?;
    let ids: Vec<i64> = ctx
        .rows(
            "SELECT id FROM goal_contributions WHERE goal_id=?1",
            &[iv(g.id)],
        )?
        .iter()
        .filter_map(|r| r["id"].as_i64())
        .collect();
    if !ids.is_empty() && !a.flag("yes") {
        return Err(Error::usage(
            "confirmation_required",
            format!(
                "'{}' has {} contribution(s); deleting it removes them",
                g.name,
                ids.len()
            ),
        )
        .with_hint("repeat with --yes (or archive it instead)"));
    }
    for id in &ids {
        ctx.delete("goal_contributions", *id)?;
    }
    ctx.delete("goals", g.id)?;
    Ok(json!({ "removed": g.name, "contributions_removed": ids.len() }))
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new("goal.add", "Create a savings goal", add)
            .write()
            .p(Param::s("name", "goal name").req())
            .ps(money("target", "target_minor", "amount to save"))
            .p(Param::date("by", "target date"))
            .p(Param::s(
                "account",
                "track this account's balance instead of manual contributions",
            ))
            .ex("pext goal add --name 'Emergency fund' --target 300000 --by 2027-06-30"),
        OpSpec::new("goal.contribute", "Add money to a goal", contribute)
            .write()
            .p(Param::s("goal", "goal id or name").req().pos())
            .ps(money("amount", "amount_minor", "amount to add"))
            .p(Param::date("date", "contribution date (default today)"))
            .p(Param::s("note", "note")),
        OpSpec::new("goal.withdraw", "Take money out of a goal", withdraw)
            .write()
            .p(Param::s("goal", "goal id or name").req().pos())
            .ps(money("amount", "amount_minor", "amount to remove"))
            .p(Param::date("date", "date (default today)"))
            .p(Param::s("note", "note")),
        OpSpec::new(
            "goal.status",
            "Progress, required monthly saving and projected completion",
            status,
        )
        .p(Param::s("goal", "only this goal"))
        .p(Param::b("all", "include archived goals")),
        OpSpec::new("goal.history", "Contributions to a goal", history).p(Param::s(
            "goal",
            "goal id or name",
        )
        .req()
        .pos()),
        OpSpec::new("goal.edit", "Change a goal", edit)
            .write()
            .p(Param::s("goal", "goal id or name").req().pos())
            .p(Param::s("new_name", "rename"))
            .ps(money("target", "target_minor", "new target"))
            .p(Param::date("by", "new target date"))
            .p(Param::b("archive", "archive the goal"))
            .p(Param::b("unarchive", "restore an archived goal")),
        OpSpec::new(
            "goal.rm",
            "Delete a goal (needs --yes if it has contributions)",
            rm,
        )
        .write()
        .p(Param::s("goal", "goal id or name").req().pos()),
    ]
}
