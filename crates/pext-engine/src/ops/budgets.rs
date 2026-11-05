//! Budgets per category (and its sub-categories) with optional rollover.
//!
//! Periods: monthly = calendar month, quarterly = calendar quarter, yearly =
//! the financial year (see `fy_start_month`). Spending is measured in the base
//! currency and counts refunds as negative spending; transfers are ignored.
//! Rollover carries *unspent* money forward; overspending is not carried.

use crate::args::Args;
use crate::ctx::{iv, sv, Ctx};
use crate::lines::fetch_lines;
use crate::ops::categories::{category_id, category_path};
use crate::ops::fx;
use crate::ops::query::parse_ctx;
use crate::ops::txns::fmt_date;
use crate::registry::{money, OpSpec, Param};
use chrono::{Datelike, NaiveDate};
use pext_core::dates::{add_days, add_months, month_end, month_start, quarter_start};
use pext_core::money::{decimals_for, mul_div_round};
use pext_core::query::Line;
use pext_core::{Error, Result};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct BudgetRow {
    pub id: i64,
    pub category_id: i64,
    pub path: String,
    pub period: String,
    pub amount_minor: i64,
    pub rollover: bool,
    pub start: NaiveDate,
}

pub fn load_budgets(ctx: &Ctx) -> Result<Vec<BudgetRow>> {
    let rows = ctx.rows(
        "SELECT b.id, b.category_id, c.path, b.period, b.amount_minor, b.rollover, b.start_date
         FROM budgets b JOIN categories c ON c.id=b.category_id ORDER BY c.path, b.period",
        &[],
    )?;
    rows.into_iter()
        .map(|r| {
            Ok(BudgetRow {
                id: r["id"].as_i64().unwrap_or(0),
                category_id: r["category_id"].as_i64().unwrap_or(0),
                path: r["path"].as_str().unwrap_or("").to_string(),
                period: r["period"].as_str().unwrap_or("").to_string(),
                amount_minor: r["amount_minor"].as_i64().unwrap_or(0),
                rollover: r["rollover"].as_i64() == Some(1),
                start: crate::ops::accounts::parse_stored_date(
                    r["start_date"].as_str().unwrap_or(""),
                )?,
            })
        })
        .collect()
}

/// Inclusive range of the budget period containing `d`.
pub fn period_range(
    period: &str,
    d: NaiveDate,
    fy_start_month: u32,
) -> Result<(NaiveDate, NaiveDate)> {
    match period {
        "monthly" => Ok((month_start(d), month_end(d))),
        "quarterly" => {
            let s = quarter_start(d);
            Ok((s, add_days(add_months(s, 3)?, -1)?))
        }
        "yearly" => {
            let y = if d.month() >= fy_start_month {
                d.year()
            } else {
                d.year() - 1
            };
            let s = NaiveDate::from_ymd_opt(y, fy_start_month, 1)
                .ok_or_else(|| Error::internal("bad_fy", "invalid financial year start"))?;
            Ok((s, add_days(add_months(s, 12)?, -1)?))
        }
        other => Err(Error::internal(
            "bad_period",
            format!("unknown budget period '{other}'"),
        )),
    }
}

fn in_subtree(line_cat: &Option<String>, path: &str) -> bool {
    line_cat
        .as_deref()
        .is_some_and(|c| c == path || c.starts_with(&format!("{path}.")))
}

/// Spending (positive = spent) of a line in the base currency; `None` if no FX rate.
pub fn spend_in_base(ctx: &Ctx, base: &str, l: &Line, skipped: &mut usize) -> Result<Option<i64>> {
    if l.kind != "standard" {
        return Ok(None);
    }
    let v = match fx::convert(ctx.conn, l.amount_minor, &l.currency, base, l.date) {
        Ok(v) => v,
        Err(e) if e.code == "fx_rate_missing" => {
            *skipped += 1;
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    Ok(Some(-v))
}

pub struct Status {
    pub b: BudgetRow,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub carried: i64,
    pub available: i64,
    pub spent: i64,
}

impl Status {
    pub fn remaining(&self) -> i64 {
        self.available - self.spent
    }
    pub fn pct_used(&self) -> i64 {
        if self.available <= 0 {
            return if self.spent > 0 { 999 } else { 0 };
        }
        mul_div_round(self.spent.max(0) as i128, 100, self.available as i128)
            .unwrap_or(0)
            .min(999) as i64
    }
    pub fn state(&self) -> &'static str {
        if self.spent > self.available {
            "over"
        } else if self.pct_used() >= 80 {
            "warning"
        } else {
            "ok"
        }
    }
}

/// Compute statuses for `budgets` (all of one set of lines) as of `on`.
pub fn statuses(ctx: &Ctx, budgets: &[BudgetRow], on: NaiveDate) -> Result<(Vec<Status>, usize)> {
    if budgets.is_empty() {
        return Ok((vec![], 0));
    }
    let fy = parse_ctx(ctx)?.fy_start_month;
    let base = ctx.base_currency()?;
    let mut earliest = on;
    for b in budgets {
        let (s, _) = period_range(&b.period, on, fy)?;
        earliest = earliest.min(if b.rollover {
            period_range(&b.period, b.start, fy)?.0.min(s)
        } else {
            s
        });
    }
    let (_, latest) = budgets
        .iter()
        .map(|b| period_range(&b.period, on, fy))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .fold((on, on), |acc, r| (acc.0.min(r.0), acc.1.max(r.1)));
    let lines = fetch_lines(ctx.conn, Some(earliest), Some(latest), false)?;
    let mut skipped = 0usize;
    // Pre-convert once: (date, category, spend)
    let mut spend: Vec<(NaiveDate, Option<String>, i64)> = vec![];
    for l in &lines {
        if let Some(v) = spend_in_base(ctx, &base, l, &mut skipped)? {
            spend.push((l.date, l.category.clone(), v));
        }
    }
    let sum = |path: &str, from: NaiveDate, to: NaiveDate| -> Result<i64> {
        let mut t: i128 = 0;
        for (d, c, v) in &spend {
            if *d >= from && *d <= to && in_subtree(c, path) {
                t += *v as i128;
            }
        }
        i64::try_from(t)
            .map_err(|_| Error::validation("amount_overflow", "spending total is too large"))
    };
    let mut out = vec![];
    for b in budgets {
        let (from, to) = period_range(&b.period, on, fy)?;
        let first = period_range(&b.period, b.start, fy)?.0;
        if from < first {
            continue; // budget has not started yet
        }
        let mut carried = 0i64;
        if b.rollover && first < from {
            let mut ps = first;
            while ps < from {
                let (a, z) = period_range(&b.period, ps, fy)?;
                let avail = b.amount_minor.saturating_add(carried);
                carried = (avail - sum(&b.path, a, z)?).max(0);
                ps = add_days(z, 1)?;
            }
        }
        let spent = sum(&b.path, from, to)?;
        out.push(Status {
            b: b.clone(),
            from,
            to,
            carried,
            available: b.amount_minor.saturating_add(carried),
            spent,
        });
    }
    Ok((out, skipped))
}

pub fn status_json(s: &Status, on: NaiveDate) -> Value {
    let total_days = (s.to - s.from).num_days() + 1;
    let elapsed = ((on.min(s.to) - s.from).num_days() + 1).clamp(1, total_days);
    let days_left = (s.to - on).num_days().max(0);
    let projected = mul_div_round(s.spent.max(0) as i128, total_days as i128, elapsed as i128)
        .unwrap_or(0) as i64;
    let rem = s.remaining();
    let daily = if days_left > 0 && rem > 0 {
        rem / days_left
    } else {
        0
    };
    json!({
        "category": s.b.path, "period": s.b.period,
        "period_start": fmt_date(s.from), "period_end": fmt_date(s.to),
        "budget_minor": s.b.amount_minor, "carried_minor": s.carried, "available_minor": s.available,
        "spent_minor": s.spent, "remaining_minor": rem, "pct_used": s.pct_used(), "state": s.state(),
        "days_left": days_left, "daily_allowance_minor": daily,
        "projected_minor": projected, "projected_over": projected > s.available,
        "rollover": s.b.rollover,
    })
}

fn set(ctx: &Ctx, a: &Args) -> Result<Value> {
    let path = a.str("category")?;
    let cid = category_id(ctx, &path)?;
    let kind = ctx
        .opt_string("SELECT kind FROM categories WHERE id=?1", &[iv(cid)])?
        .unwrap_or_default();
    if kind != "expense" {
        return Err(Error::validation(
            "invalid_budget_category",
            "budgets apply to expense categories only",
        )
        .with_field("category"));
    }
    let base = ctx.base_currency()?;
    let amount = a.req_money("amount", decimals_for(&base))?;
    if amount <= 0 {
        return Err(
            Error::validation("invalid_amount", "a budget must be positive").with_field("amount"),
        );
    }
    let period = a.opt_str("period").unwrap_or_else(|| "monthly".into());
    let start = a.opt_date("start")?.unwrap_or(ctx.today);
    let rollover = a.flag("rollover");
    let existing = ctx.opt_i64(
        "SELECT id FROM budgets WHERE category_id=?1 AND period=?2",
        &[iv(cid), sv(&period)],
    )?;
    let id = match existing {
        Some(id) => {
            let mut cols = vec![
                ("amount_minor", iv(amount)),
                ("rollover", iv(rollover as i64)),
            ];
            if a.has("start") {
                cols.push(("start_date", sv(&fmt_date(start))));
            }
            ctx.update("budgets", id, &cols)?;
            id
        }
        None => ctx.insert(
            "budgets",
            &[
                ("category_id", iv(cid)),
                ("period", sv(&period)),
                ("amount_minor", iv(amount)),
                ("rollover", iv(rollover as i64)),
                ("start_date", sv(&fmt_date(start))),
                ("created_at", sv(&ctx.now)),
            ],
        )?,
    };
    Ok(
        json!({ "id": id, "category": category_path(ctx, cid)?, "period": period, "amount_minor": amount, "currency": base, "rollover": rollover }),
    )
}

fn list(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let base = ctx.base_currency()?;
    Ok(Value::Array(
        load_budgets(ctx)?
            .into_iter()
            .map(|b| json!({ "id": b.id, "category": b.path, "period": b.period, "amount_minor": b.amount_minor, "currency": base, "rollover": b.rollover, "start": fmt_date(b.start) }))
            .collect(),
    ))
}

fn rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let cid = category_id(ctx, &a.str("category")?)?;
    let period = a.opt_str("period").unwrap_or_else(|| "monthly".into());
    let id = ctx
        .opt_i64(
            "SELECT id FROM budgets WHERE category_id=?1 AND period=?2",
            &[iv(cid), sv(&period)],
        )?
        .ok_or_else(|| Error::not_found("budget_not_found", "no such budget"))?;
    ctx.delete("budgets", id)?;
    Ok(json!({ "removed": id }))
}

fn status(ctx: &Ctx, a: &Args) -> Result<Value> {
    let on = a.opt_date("date")?.unwrap_or(ctx.today);
    let period = a.opt_str("period").unwrap_or_else(|| "monthly".into());
    let mut budgets: Vec<BudgetRow> = load_budgets(ctx)?
        .into_iter()
        .filter(|b| b.period == period)
        .collect();
    if let Some(c) = a.opt_str("category") {
        let cid = category_id(ctx, &c)?;
        budgets.retain(|b| b.category_id == cid);
    }
    let (sts, skipped) = statuses(ctx, &budgets, on)?;
    if skipped > 0 {
        ctx.warn(
            "fx_missing",
            format!(
                "{skipped} transaction line(s) in other currencies were skipped: no exchange rate"
            ),
            json!({ "skipped": skipped }),
        );
    }
    let base = ctx.base_currency()?;
    let rows: Vec<Value> = sts.iter().map(|s| status_json(s, on)).collect();
    let (mut budgeted, mut spent) = (0i128, 0i128);
    for s in &sts {
        if !sts
            .iter()
            .any(|o| o.b.id != s.b.id && s.b.path.starts_with(&format!("{}.", o.b.path)))
        {
            budgeted += s.available as i128; // top-most budgets only, to avoid double counting
            spent += s.spent as i128;
        }
    }
    Ok(json!({
        "as_of": fmt_date(on), "period": period, "currency": base,
        "budgets": rows,
        "totals": {
            "available_minor": budgeted as i64, "spent_minor": spent as i64, "remaining_minor": (budgeted - spent) as i64,
        },
    }))
}

pub fn ops() -> Vec<OpSpec> {
    let periods: &'static [&'static str] = &["monthly", "quarterly", "yearly"];
    vec![
        OpSpec::new(
            "budget.set",
            "Create or update a budget for a category (and its sub-categories)",
            set,
        )
        .write()
        .p(Param::s("category", "expense category path").req())
        .ps(money(
            "amount",
            "amount_minor",
            "budget per period, in the base currency",
        ))
        .p(Param::s(
            "period",
            "monthly (default), quarterly or yearly (financial year)",
        )
        .choices(periods))
        .p(Param::b(
            "rollover",
            "carry unspent money into the next period",
        ))
        .p(Param::date(
            "start",
            "first period this budget applies to (default today)",
        ))
        .ex("pext budget set --category food --amount 15000 --rollover"),
        OpSpec::new("budget.list", "List budgets", list),
        OpSpec::new("budget.rm", "Remove a budget", rm)
            .write()
            .p(Param::s("category", "category path").req())
            .p(Param::s("period", "monthly (default), quarterly or yearly").choices(periods)),
        OpSpec::new(
            "budget.status",
            "Spent vs budget for a period, with pace and projections",
            status,
        )
        .p(Param::date(
            "date",
            "any date inside the period (default today)",
        ))
        .p(Param::s("period", "monthly (default), quarterly or yearly").choices(periods))
        .p(Param::s("category", "only this category")),
    ]
}
