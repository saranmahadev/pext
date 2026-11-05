//! Loans (EMI, amortisation, prepayment what-ifs), multi-debt payoff plans and
//! credit-card utilisation.

use crate::args::Args;
use crate::ctx::{iv, oiv, sv, Ctx};
use crate::ops::accounts::{balance_at, load_account, parse_stored_date, resolve_account};
use crate::ops::categories::collapse_ws;
use crate::ops::fx;
use crate::ops::txns::fmt_date;
use crate::registry::{money, OpSpec, Param};
use chrono::NaiveDate;
use pext_core::dates::add_months;
use pext_core::loan::{self, Debt, Schedule, Strategy};
use pext_core::money::{check_amount, decimals_for, mul_div_round, parse_amount};
use pext_core::{Error, Result};
use serde_json::{json, Value};

pub struct Loan {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub currency: String,
    pub principal: i64,
    pub rate_milli: i64,
    pub months: i64,
    pub start: NaiveDate,
    pub emi: i64,
    pub account_id: Option<i64>,
    pub lender: String,
    pub archived: bool,
}

impl Loan {
    pub fn schedule(&self, extra: i64) -> Result<Schedule> {
        loan::schedule(
            self.principal,
            self.rate_milli,
            self.months,
            self.emi,
            self.start,
            extra,
        )
    }
    /// Scheduled balance today, assuming every instalment so far was paid on time.
    pub fn scheduled_outstanding(&self, today: NaiveDate) -> Result<i64> {
        Ok(self.schedule(0)?.outstanding_on(self.principal, today))
    }
    /// Balance used for net worth and planning: the linked account when there
    /// is one (it reflects what was really paid), otherwise the schedule.
    pub fn outstanding(&self, ctx: &Ctx) -> Result<(i64, &'static str)> {
        match self.account_id {
            Some(id) => {
                let a = load_account(ctx, id)?;
                let bal = balance_at(ctx, &a, ctx.today, false)?;
                Ok(((-bal).max(0), "account"))
            }
            None => Ok((self.scheduled_outstanding(ctx.today)?, "schedule")),
        }
    }
}

pub fn load_loans(ctx: &Ctx, include_archived: bool) -> Result<Vec<Loan>> {
    let mut out = vec![];
    for r in ctx.rows("SELECT * FROM loans ORDER BY name", &[])? {
        let l = Loan {
            id: r["id"].as_i64().unwrap_or(0),
            name: r["name"].as_str().unwrap_or("").into(),
            kind: r["kind"].as_str().unwrap_or("").into(),
            currency: r["currency"].as_str().unwrap_or("").into(),
            principal: r["principal_minor"].as_i64().unwrap_or(0),
            rate_milli: r["rate_milli"].as_i64().unwrap_or(0),
            months: r["months"].as_i64().unwrap_or(0),
            start: parse_stored_date(r["start_date"].as_str().unwrap_or(""))?,
            emi: r["emi_minor"].as_i64().unwrap_or(0),
            account_id: r["account_id"].as_i64(),
            lender: r["lender"].as_str().unwrap_or("").into(),
            archived: r["archived"].as_i64() == Some(1),
        };
        if include_archived || !l.archived {
            out.push(l);
        }
    }
    Ok(out)
}

fn find(ctx: &Ctx, r: &str) -> Result<Loan> {
    let want = r.trim();
    load_loans(ctx, true)?
        .into_iter()
        .find(|l| l.name.eq_ignore_ascii_case(want) || want.parse::<i64>().ok() == Some(l.id))
        .ok_or_else(|| {
            Error::not_found("loan_not_found", format!("no loan '{r}'"))
                .with_hint("run `pext loan list`")
        })
}

fn rate_arg(s: &str) -> Result<i64> {
    let v = parse_amount(s.trim().trim_end_matches('%'), 3).map_err(|e| {
        Error::validation(
            "invalid_rate",
            format!("'{s}' is not a valid interest rate: {}", e.message),
        )
        .with_field("rate")
    })?;
    if !(0..=loan::MAX_RATE_MILLI).contains(&v) {
        return Err(Error::validation(
            "invalid_rate",
            "the interest rate must be between 0% and 50% a year",
        )
        .with_field("rate"));
    }
    Ok(v)
}

pub fn pct_text(rate_milli: i64) -> String {
    let s = pext_core::money::format_amount(rate_milli, 3);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn loan_json(ctx: &Ctx, l: &Loan) -> Result<Value> {
    let s = l.schedule(0)?;
    let (out, src) = l.outstanding(ctx)?;
    let scheduled = s.outstanding_on(l.principal, ctx.today);
    let paid_n = s.paid_count_on(ctx.today);
    let next = s.rows.iter().find(|r| r.due > ctx.today);
    let end = s.rows.last().map(|r| r.due);
    let pct = if l.principal > 0 {
        mul_div_round(
            (l.principal - out.min(l.principal)) as i128 * 100,
            1,
            l.principal as i128,
        )?
    } else {
        0
    };
    let account = match l.account_id {
        Some(id) => Some(load_account(ctx, id)?.name),
        None => None,
    };
    Ok(json!({
        "id": l.id, "name": l.name, "kind": l.kind, "lender": l.lender, "currency": l.currency,
        "principal_minor": l.principal, "rate_pct": pct_text(l.rate_milli), "term_months": l.months,
        "start_date": fmt_date(l.start), "emi_minor": l.emi, "account": account, "archived": l.archived,
        "outstanding_minor": out, "outstanding_source": src, "scheduled_outstanding_minor": scheduled,
        "instalments_paid": paid_n, "instalments_left": s.rows.len() as i64 - paid_n,
        "repaid_pct": pct, "next_due": next.map(|r| fmt_date(r.due)), "next_amount_minor": next.map(|r| r.payment),
        "ends": end.map(fmt_date),
        "total_interest_minor": s.total_interest, "total_paid_minor": s.total_paid,
    }))
}

fn add(ctx: &Ctx, a: &Args) -> Result<Value> {
    let name = collapse_ws(&a.str("name")?);
    if name.is_empty() || name.chars().count() > 64 {
        return Err(
            Error::validation("invalid_name", "the loan name must be 1-64 characters")
                .with_field("name"),
        );
    }
    if ctx
        .opt_i64(
            "SELECT id FROM loans WHERE name=?1 COLLATE NOCASE",
            &[sv(&name)],
        )?
        .is_some()
    {
        return Err(Error::conflict(
            "already_exists",
            format!("a loan named '{name}' already exists"),
        ));
    }
    let acct = match a.opt_str("account") {
        Some(r) => {
            let acc = resolve_account(ctx, &r)?;
            if !acc.is_liability() {
                return Err(Error::validation(
                    "invalid_loan_account",
                    format!("'{}' is not a liability account", acc.name),
                )
                .with_hint("link a loan, mortgage, credit_card or other_liability account"));
            }
            Some(acc)
        }
        None => None,
    };
    let currency = match (a.opt_str("currency"), &acct) {
        (Some(c), Some(acc)) if !c.eq_ignore_ascii_case(&acc.currency) => {
            return Err(Error::validation(
                "currency_mismatch",
                format!("account '{}' is in {}", acc.name, acc.currency),
            ))
        }
        (Some(c), _) => pext_core::money::validate_currency(&c)?,
        (None, Some(acc)) => acc.currency.clone(),
        (None, None) => ctx.base_currency()?,
    };
    let dec = decimals_for(&currency);
    let principal = a.req_money("principal", dec)?;
    let rate = rate_arg(&a.str("rate")?)?;
    let months = a.i64("months")?;
    loan::check_terms(principal, rate, months)?;
    let start = a.opt_date("start")?.unwrap_or(ctx.today);
    let computed = loan::emi(principal, rate, months)?;
    let emi = match a.money("emi", dec)? {
        Some(e) => {
            check_amount(e)?;
            if e <= 0 {
                return Err(
                    Error::validation("invalid_emi", "the instalment must be positive")
                        .with_field("emi"),
                );
            }
            e
        }
        None => computed,
    };
    // A custom instalment must still retire the loan by the end of the term.
    let s = loan::schedule(principal, rate, months, emi, start, 0)?;
    let last = s.rows.last().map(|r| r.payment).unwrap_or(0);
    if last > emi + emi / 20 + 1 {
        return Err(Error::validation(
            "emi_too_low",
            format!(
                "an instalment of {} leaves a balloon of {} in the final month",
                pext_core::money::format_amount(emi, dec),
                pext_core::money::format_amount(last, dec)
            ),
        )
        .with_details(json!({ "suggested_emi_minor": computed }))
        .with_hint("omit --emi to use the computed instalment"));
    }
    let kind = a.opt_str("kind").unwrap_or_else(|| "personal".into());
    let id = ctx.insert(
        "loans",
        &[
            ("name", sv(&name)),
            ("kind", sv(&kind)),
            ("currency", sv(&currency)),
            ("principal_minor", iv(principal)),
            ("rate_milli", iv(rate)),
            ("months", iv(months)),
            ("start_date", sv(&fmt_date(start))),
            ("emi_minor", iv(emi)),
            ("account_id", oiv(acct.as_ref().map(|x| x.id))),
            ("lender", sv(a.opt_str("lender").unwrap_or_default().trim())),
            ("created_at", sv(&ctx.now)),
        ],
    )?;
    loan_json(ctx, &find(ctx, &id.to_string())?)
}

fn list(ctx: &Ctx, a: &Args) -> Result<Value> {
    let mut rows = vec![];
    let (mut total, mut total_emi) = (0i128, 0i128);
    let base = ctx.base_currency()?;
    let mut skipped = 0;
    for l in load_loans(ctx, a.flag("all"))? {
        let j = loan_json(ctx, &l)?;
        if !l.archived {
            match (
                fx::convert(
                    ctx.conn,
                    j["outstanding_minor"].as_i64().unwrap_or(0),
                    &l.currency,
                    &base,
                    ctx.today,
                ),
                fx::convert(ctx.conn, l.emi, &l.currency, &base, ctx.today),
            ) {
                (Ok(o), Ok(e)) => {
                    total += o as i128;
                    total_emi += e as i128;
                }
                _ => skipped += 1,
            }
        }
        rows.push(j);
    }
    if skipped > 0 {
        ctx.warn(
            "fx_missing",
            format!(
                "{skipped} loan(s) in other currencies are not in the totals: no exchange rate"
            ),
            json!({}),
        );
    }
    if rows.len() > 1 || !rows.is_empty() {
        rows.push(json!({ "name": "TOTAL", "currency": base, "outstanding_minor": total as i64, "emi_minor": total_emi as i64 }));
    }
    Ok(Value::Array(rows))
}

fn get(ctx: &Ctx, a: &Args) -> Result<Value> {
    loan_json(ctx, &find(ctx, &a.str("loan")?)?)
}

fn schedule_op(ctx: &Ctx, a: &Args) -> Result<Value> {
    let l = find(ctx, &a.str("loan")?)?;
    let dec = decimals_for(&l.currency);
    let extra = a.money("extra", dec)?.unwrap_or(0);
    if extra < 0 {
        return Err(
            Error::validation("invalid_amount", "the extra payment cannot be negative")
                .with_field("extra"),
        );
    }
    let limit = a.opt_i64("limit")?.unwrap_or(12).clamp(0, 600) as usize;
    let full = l.schedule(0)?;
    let paid = full.paid_count_on(ctx.today) as usize;
    // What-if: start from today's balance with the instalments still to come.
    let bal = full.outstanding_on(l.principal, ctx.today);
    let left_months = l.months - paid as i64;
    let anchor = if paid > 0 {
        full.rows[paid - 1].due
    } else {
        l.start
    };
    let (base_s, plan_s) = if bal == 0 || left_months <= 0 {
        (None, None)
    } else {
        // keep absolute due dates: schedule from the loan's own start, shifted by months already paid
        let b = loan::schedule(bal, l.rate_milli, left_months, l.emi, anchor, 0)?;
        let p = loan::schedule(bal, l.rate_milli, left_months, l.emi, anchor, extra)?;
        (Some(b), Some(p))
    };
    let (rows_src, base_int, base_n, plan_int, plan_n) = match (&base_s, &plan_s) {
        (Some(b), Some(p)) => (
            p.rows.clone(),
            b.total_interest,
            b.rows.len() as i64,
            p.total_interest,
            p.rows.len() as i64,
        ),
        _ => (vec![], 0, 0, 0, 0),
    };
    let shown = if limit == 0 {
        rows_src.len()
    } else {
        limit.min(rows_src.len())
    };
    let rows: Vec<Value> = rows_src
        .iter()
        .take(shown)
        .map(|r| json!({ "n": r.n + paid as i64, "due": fmt_date(r.due), "payment_minor": r.payment, "principal_minor": r.principal, "interest_minor": r.interest, "balance_minor": r.closing }))
        .collect();
    Ok(json!({
        "loan": l.name, "currency": l.currency, "as_of": fmt_date(ctx.today),
        "outstanding_minor": bal, "instalments_left": base_n, "extra_minor": extra,
        "with_extra": {
            "instalments_left": plan_n, "interest_minor": plan_int,
            "ends": rows_src.last().map(|r| fmt_date(r.due)),
        },
        "without_extra": { "instalments_left": base_n, "interest_minor": base_int, "ends": base_s.as_ref().and_then(|s| s.rows.last()).map(|r| fmt_date(r.due)) },
        "months_saved": base_n - plan_n, "interest_saved_minor": base_int - plan_int,
        "rows_shown": shown, "rows_total": rows_src.len(),
        "rows": rows,
    }))
}

fn edit(ctx: &Ctx, a: &Args) -> Result<Value> {
    let l = find(ctx, &a.str("loan")?)?;
    let mut cols: Vec<(&str, rusqlite::types::Value)> = vec![];
    if let Some(n) = a.opt_str("new_name") {
        let n = collapse_ws(&n);
        if n.is_empty() || n.chars().count() > 64 {
            return Err(
                Error::validation("invalid_name", "the loan name must be 1-64 characters")
                    .with_field("new_name"),
            );
        }
        if ctx
            .opt_i64(
                "SELECT id FROM loans WHERE name=?1 COLLATE NOCASE AND id!=?2",
                &[sv(&n), iv(l.id)],
            )?
            .is_some()
        {
            return Err(Error::conflict(
                "already_exists",
                format!("a loan named '{n}' already exists"),
            ));
        }
        cols.push(("name", sv(&n)));
    }
    if let Some(v) = a.opt_str("lender") {
        cols.push(("lender", sv(v.trim())));
    }
    if let Some(k) = a.opt_str("kind") {
        cols.push(("kind", sv(&k)));
    }
    if let Some(r) = a.opt_str("account") {
        let acc = resolve_account(ctx, &r)?;
        if !acc.is_liability() || acc.currency != l.currency {
            return Err(Error::validation(
                "invalid_loan_account",
                "the account must be a liability in the loan's currency",
            ));
        }
        cols.push(("account_id", iv(acc.id)));
    }
    if a.flag("unlink") {
        cols.push(("account_id", oiv(None)));
    }
    if a.flag("archive") {
        cols.push(("archived", iv(1)));
    }
    if a.flag("unarchive") {
        cols.push(("archived", iv(0)));
    }
    if cols.is_empty() {
        return Err(
            Error::usage("nothing_to_change", "give at least one field to change")
                .with_hint("loan terms cannot be edited; remove the loan and add it again"),
        );
    }
    ctx.update("loans", l.id, &cols)?;
    loan_json(ctx, &find(ctx, &l.id.to_string())?)
}

fn rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let l = find(ctx, &a.str("loan")?)?;
    if !a.flag("yes") {
        return Err(Error::usage(
            "confirmation_required",
            format!(
                "removing loan '{}' deletes its terms (transactions are untouched)",
                l.name
            ),
        )
        .with_hint("repeat with --yes"));
    }
    ctx.delete("loans", l.id)?;
    Ok(json!({ "removed": l.name }))
}

// ---- debt payoff ------------------------------------------------------------

fn parse_extra_debt(spec: &str, dec: u32) -> Result<Debt> {
    let parts: Vec<&str> = spec.split(':').collect();
    let bad = || {
        Error::validation(
            "invalid_debt",
            format!("'{spec}' should look like Name:balance:apr:minimum, e.g. Visa:50000:36:2500"),
        )
        .with_field("debt")
    };
    if parts.len() != 4 || parts[0].trim().is_empty() {
        return Err(bad());
    }
    Ok(Debt {
        name: parts[0].trim().to_string(),
        balance: parse_amount(parts[1].trim(), dec).map_err(|_| bad())?,
        rate_milli: rate_arg(parts[2])?,
        min_payment: parse_amount(parts[3].trim(), dec).map_err(|_| bad())?,
    })
}

fn debt_plan(ctx: &Ctx, a: &Args) -> Result<Value> {
    let base = ctx.base_currency()?;
    let dec = decimals_for(&base);
    let extra = a.money("extra", dec)?.unwrap_or(0);
    let strategy = match a.opt_str("strategy").as_deref() {
        Some("snowball") => Strategy::Snowball,
        _ => Strategy::Avalanche,
    };
    let mut debts: Vec<Debt> = vec![];
    let mut skipped = 0;
    for l in load_loans(ctx, false)? {
        let (out, _) = l.outstanding(ctx)?;
        if out == 0 {
            continue;
        }
        match (
            fx::convert(ctx.conn, out, &l.currency, &base, ctx.today),
            fx::convert(ctx.conn, l.emi, &l.currency, &base, ctx.today),
        ) {
            (Ok(b), Ok(e)) => debts.push(Debt {
                name: l.name.clone(),
                balance: b,
                rate_milli: l.rate_milli,
                min_payment: e,
            }),
            _ => skipped += 1,
        }
    }
    for s in a.list("debt") {
        debts.push(parse_extra_debt(&s, dec)?);
    }
    if skipped > 0 {
        ctx.warn(
            "fx_missing",
            format!("{skipped} loan(s) in other currencies were left out: no exchange rate"),
            json!({}),
        );
    }
    if debts.is_empty() {
        return Err(
            Error::not_found("no_debts", "there are no outstanding loans to plan for").with_hint(
                "add loans with `pext loan add` or pass --debt Name:balance:apr:minimum",
            ),
        );
    }
    let chosen = loan::payoff(&debts, extra, strategy, true)?;
    let other = loan::payoff(
        &debts,
        extra,
        if strategy == Strategy::Avalanche {
            Strategy::Snowball
        } else {
            Strategy::Avalanche
        },
        true,
    )?;
    let minimum = loan::payoff(&debts, 0, Strategy::Avalanche, false)?;
    let name = |s: Strategy| {
        if s == Strategy::Avalanche {
            "avalanche"
        } else {
            "snowball"
        }
    };
    let free = |m: i64| add_months(ctx.today, m as i32).map(fmt_date);
    let mut order = vec![];
    for p in &chosen.order {
        order.push(json!({ "debt": p.name, "paid_off_in_months": p.month, "paid_off_on": free(p.month)?, "interest_minor": p.interest_paid }));
    }
    Ok(json!({
        "currency": base, "strategy": name(strategy), "extra_minor": extra,
        "monthly_budget_minor": debts.iter().map(|d| d.min_payment as i128).sum::<i128>() as i64 + extra,
        "total_debt_minor": debts.iter().map(|d| d.balance as i128).sum::<i128>() as i64,
        "months": chosen.months, "debt_free_on": free(chosen.months)?,
        "total_interest_minor": chosen.total_interest, "total_paid_minor": chosen.total_paid,
        "payoff_order": order,
        "minimums_only": { "months": minimum.months, "debt_free_on": free(minimum.months)?, "total_interest_minor": minimum.total_interest },
        "interest_saved_minor": minimum.total_interest - chosen.total_interest,
        "months_saved": minimum.months - chosen.months,
        "other_strategy": { "strategy": name(if strategy == Strategy::Avalanche { Strategy::Snowball } else { Strategy::Avalanche }), "months": other.months, "total_interest_minor": other.total_interest },
    }))
}

// ---- credit utilisation -------------------------------------------------------

fn utilization(ctx: &Ctx, a: &Args) -> Result<Value> {
    let base = ctx.base_currency()?;
    let target = a.opt_i64("target")?.unwrap_or(30).clamp(1, 100);
    let mut cards = vec![];
    let (mut used_all, mut limit_all) = (0i128, 0i128);
    for r in ctx.rows(
        "SELECT id FROM accounts WHERE kind='credit_card' AND archived=0 ORDER BY name",
        &[],
    )? {
        let acc = load_account(ctx, r["id"].as_i64().unwrap_or(0))?;
        if let Some(scope) = &ctx.actor.accounts {
            if !scope.contains(&acc.id) {
                continue;
            }
        }
        let bal = balance_at(ctx, &acc, ctx.today, false)?;
        let used = (-bal).max(0);
        let credit = bal.max(0);
        let (pct, status, pay_down) = match acc.credit_limit_minor {
            Some(l) if l > 0 => {
                let pct = mul_div_round(used as i128 * 100, 1, l as i128)? as i64;
                let goal = mul_div_round(l as i128, target as i128, 100)? as i64;
                let st = if pct < 30 {
                    "good"
                } else if pct < 50 {
                    "fair"
                } else if pct < 75 {
                    "high"
                } else {
                    "very_high"
                };
                (Some(pct), st, Some((used - goal).max(0)))
            }
            _ => (None, "no_limit", None),
        };
        if let Some(l) = acc.credit_limit_minor.filter(|l| *l > 0) {
            match (
                fx::convert(ctx.conn, used, &acc.currency, &base, ctx.today),
                fx::convert(ctx.conn, l, &acc.currency, &base, ctx.today),
            ) {
                (Ok(u), Ok(lm)) => {
                    used_all += u as i128;
                    limit_all += lm as i128;
                }
                _ => ctx.warn(
                    "fx_missing",
                    format!(
                        "{} is not in the overall figure: no exchange rate",
                        acc.name
                    ),
                    json!({}),
                ),
            }
        }
        cards.push(json!({
            "account": acc.name, "currency": acc.currency, "used_minor": used, "credit_balance_minor": credit,
            "limit_minor": acc.credit_limit_minor, "available_minor": acc.credit_limit_minor.map(|l| (l - used).max(0)),
            "utilization_pct": pct, "status": status, "pay_down_to_target_minor": pay_down,
        }));
    }
    let overall = if limit_all > 0 {
        Some(mul_div_round(used_all * 100, 1, limit_all)? as i64)
    } else {
        None
    };
    Ok(json!({
        "currency": base, "target_pct": target, "cards": cards,
        "overall": { "used_minor": used_all as i64, "limit_minor": limit_all as i64, "utilization_pct": overall },
    }))
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new("loan.add", "Add a loan; computes the EMI and the full amortisation", add)
            .write()
            .p(Param::s("name", "loan name").req())
            .p(Param::s("kind", "home, car, personal, education, business or other").choices(&["home", "car", "personal", "education", "business", "other"]))
            .ps(money("principal", "principal_minor", "amount borrowed"))
            .p(Param::s("rate", "annual interest rate in percent, e.g. 8.5").req())
            .p(Param::i("months", "term in months").req())
            .p(Param::date("start", "disbursal date (default today); the first EMI falls one month later"))
            .ps(money("emi", "emi_minor", "instalment if different from the computed one"))
            .p(Param::s("account", "liability account that tracks this loan's real balance"))
            .p(Param::s("currency", "currency (default: the account's, else the base currency)"))
            .p(Param::s("lender", "bank or person"))
            .ex("pext loan add --name 'Home loan' --kind home --principal 5000000 --rate 8.5 --months 240 --start 2026-01-05"),
        OpSpec::new("loan.list", "List loans with outstanding balance and next EMI", list).p(Param::b("all", "include archived loans")),
        OpSpec::new("loan.get", "Show one loan", get).p(Param::s("loan", "loan name or id").req().pos()),
        OpSpec::new("loan.schedule", "Amortisation from today, with an optional monthly prepayment what-if", schedule_op)
            .p(Param::s("loan", "loan name or id").req().pos())
            .ps(money("extra", "extra_minor", "extra amount paid every month on top of the EMI"))
            .p(Param::i("limit", "rows to show (default 12, 0 = all)"))
            .ex("pext loan schedule 'Home loan' --extra 10000"),
        OpSpec::new("loan.edit", "Rename, relink or archive a loan", edit)
            .write()
            .p(Param::s("loan", "loan name or id").req().pos())
            .p(Param::s("new_name", "new name"))
            .p(Param::s("lender", "lender"))
            .p(Param::s("kind", "loan kind").choices(&["home", "car", "personal", "education", "business", "other"]))
            .p(Param::s("account", "link to a liability account"))
            .p(Param::b("unlink", "remove the account link"))
            .p(Param::b("archive", "archive (paid off)"))
            .p(Param::b("unarchive", "restore")),
        OpSpec::new("loan.rm", "Delete a loan (requires --yes)", rm).write().p(Param::s("loan", "loan name or id").req().pos()),
        OpSpec::new("debt.plan", "Plan the fastest way out of debt (avalanche vs snowball) with optional extra money", debt_plan)
            .p(Param::s("strategy", "avalanche (highest rate first, default) or snowball (smallest balance first)").choices(&["avalanche", "snowball"]))
            .ps(money("extra", "extra_minor", "extra amount per month beyond all minimum payments"))
            .p(Param::s("debt", "extra debt as Name:balance:apr:minimum, e.g. Visa:50000:36:2500 (repeatable)").multi())
            .ex("pext debt plan --extra 5000 --debt Visa:50000:36:2500"),
        OpSpec::new("credit.utilization", "Credit-card utilisation per card and overall", utilization)
            .scoped()
            .p(Param::i("target", "target utilisation in percent (default 30)")),
    ]
}
