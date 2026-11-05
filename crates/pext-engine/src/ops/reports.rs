//! Reports over income and spending. Amounts are converted to the base
//! currency on the transaction date; transfers and balance adjustments are
//! excluded. Income/expense is decided by the category kind (uncategorised
//! lines by sign), so refunds reduce spending instead of counting as income.

use crate::args::Args;
use crate::ctx::Ctx;
use crate::lines::fetch_lines;
use crate::ops::fx;
use crate::ops::query::{parse_ctx, parse_text};
use crate::ops::txns::fmt_date;
use crate::registry::{OpSpec, Param};
use chrono::NaiveDate;
use pext_core::dates::{add_days, add_months, month_end, month_start};
use pext_core::money::mul_div_round;
use pext_core::qlang::parse_period;
use pext_core::query::{line_matches, Line, Query};
use pext_core::Result;
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flow {
    Income,
    Expense,
}

pub struct Row {
    pub line: Line,
    /// Signed amount in the base currency (income positive, expense negative).
    pub base: i64,
    pub flow: Flow,
}

impl Row {
    /// Positive magnitude in its own direction: income received or net spent.
    pub fn amount(&self) -> i64 {
        match self.flow {
            Flow::Income => self.base,
            Flow::Expense => -self.base,
        }
    }
}

pub struct Data {
    pub rows: Vec<Row>,
    pub base: String,
    pub skipped: usize,
}

pub fn collect(ctx: &Ctx, from: NaiveDate, to: NaiveDate, filter: &Query) -> Result<Data> {
    let base = ctx.base_currency()?;
    let kinds: BTreeMap<String, String> = ctx
        .rows("SELECT path, kind FROM categories", &[])?
        .into_iter()
        .map(|r| {
            (
                r["path"].as_str().unwrap_or("").to_string(),
                r["kind"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    let mut rows = vec![];
    let mut skipped = 0;
    for l in fetch_lines(ctx.conn, Some(from), Some(to), false)? {
        if l.kind != "standard" {
            continue;
        }
        if let Some(scope) = &ctx.actor.accounts {
            if !scope.contains(&l.account_id) {
                continue;
            }
        }
        if !line_matches(filter, &l)? {
            continue;
        }
        let v = match fx::convert(ctx.conn, l.amount_minor, &l.currency, &base, l.date) {
            Ok(v) => v,
            Err(e) if e.code == "fx_rate_missing" => {
                skipped += 1;
                continue;
            }
            Err(e) => return Err(e),
        };
        let flow = match l
            .category
            .as_ref()
            .and_then(|c| kinds.get(c))
            .map(String::as_str)
        {
            Some("income") => Flow::Income,
            Some(_) => Flow::Expense,
            None => {
                if v > 0 {
                    Flow::Income
                } else {
                    Flow::Expense
                }
            }
        };
        rows.push(Row {
            line: l,
            base: v,
            flow,
        });
    }
    if skipped > 0 {
        ctx.warn(
            "fx_missing",
            format!("{skipped} line(s) in other currencies were left out: no exchange rate"),
            json!({ "skipped": skipped }),
        );
    }
    Ok(Data {
        rows,
        base,
        skipped,
    })
}

fn sum(rows: &[Row], flow: Flow) -> i64 {
    rows.iter()
        .filter(|r| r.flow == flow)
        .map(|r| r.amount() as i128)
        .sum::<i128>()
        .clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn pct(n: i64, d: i64) -> Option<i64> {
    if d == 0 {
        None
    } else {
        mul_div_round(n as i128, 100, d as i128)
            .ok()
            .map(|v| v as i64)
    }
}

fn period_arg(ctx: &Ctx, a: &Args, key: &str, default: &str) -> Result<(NaiveDate, NaiveDate)> {
    let cx = parse_ctx(ctx)?;
    parse_period(&a.opt_str(key).unwrap_or_else(|| default.to_string()), &cx)
}

fn filter_arg(ctx: &Ctx, a: &Args) -> Result<Query> {
    let mut q = parse_text(ctx, &a.opt_str("query").unwrap_or_default())?;
    let accts = a.list("account");
    if !accts.is_empty() {
        q.and(pext_core::query::Term::new(
            pext_core::query::Field::Account,
            pext_core::query::Op::Eq,
            accts,
        ));
    }
    Ok(q)
}

fn ranked(
    rows: &[Row],
    flow: Flow,
    key: impl Fn(&Row) -> Vec<String>,
    top: usize,
    label: &str,
) -> Vec<Value> {
    let mut m: BTreeMap<String, (i128, usize)> = BTreeMap::new();
    for r in rows.iter().filter(|r| r.flow == flow) {
        for k in key(r) {
            let e = m.entry(k).or_default();
            e.0 += r.amount() as i128;
            e.1 += 1;
        }
    }
    let total: i128 = m.values().map(|v| v.0).sum();
    let mut v: Vec<(String, i128, usize)> = m.into_iter().map(|(k, (s, n))| (k, s, n)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.into_iter()
        .take(top)
        .map(|(k, s, n)| json!({ label: k, "amount_minor": s as i64, "share_pct": pct(s as i64, total as i64), "count": n }))
        .collect()
}

fn cat_key(r: &Row) -> Vec<String> {
    vec![r
        .line
        .category
        .clone()
        .unwrap_or_else(|| "(uncategorized)".into())]
}
fn payee_key(r: &Row) -> Vec<String> {
    vec![if r.line.payee.is_empty() {
        "(none)".into()
    } else {
        r.line.payee.clone()
    }]
}

fn summary(ctx: &Ctx, a: &Args) -> Result<Value> {
    let (from, to) = period_arg(ctx, a, "period", "this-month")?;
    let filter = filter_arg(ctx, a)?;
    let d = collect(ctx, from, to, &filter)?;
    let income = sum(&d.rows, Flow::Income);
    let expense = sum(&d.rows, Flow::Expense);
    let len = (to - from).num_days() + 1;
    let (pfrom, pto) = (add_days(from, -len)?, add_days(from, -1)?);
    let prev = collect(ctx, pfrom, pto, &filter)?;
    let (pinc, pexp) = (
        sum(&prev.rows, Flow::Income),
        sum(&prev.rows, Flow::Expense),
    );
    let mut biggest: Vec<&Row> = d.rows.iter().filter(|r| r.flow == Flow::Expense).collect();
    biggest.sort_by_key(|r| std::cmp::Reverse((r.amount(), r.line.txn_id)));
    let txns: std::collections::HashSet<i64> = d.rows.iter().map(|r| r.line.txn_id).collect();
    Ok(json!({
        "period_start": fmt_date(from), "period_end": fmt_date(to), "currency": d.base,
        "income_minor": income, "expense_minor": expense, "net_minor": income - expense,
        "savings_rate_pct": pct(income - expense, income),
        "transactions": txns.len(),
        "vs_previous": {
            "period_start": fmt_date(pfrom), "period_end": fmt_date(pto),
            "income_minor": pinc, "expense_minor": pexp,
            "income_change_minor": income - pinc, "expense_change_minor": expense - pexp,
            "expense_change_pct": pct(expense - pexp, pexp),
        },
        "top_categories": ranked(&d.rows, Flow::Expense, cat_key, 5, "name"),
        "top_payees": ranked(&d.rows, Flow::Expense, payee_key, 5, "payee"),
        "largest_expenses": biggest.iter().take(5).map(|r| json!({
            "txn_id": r.line.txn_id, "date": fmt_date(r.line.date), "payee": r.line.payee,
            "category": r.line.category, "amount_minor": r.amount(),
        })).collect::<Vec<_>>(),
    }))
}

fn depth_key(path: &str, depth: usize) -> String {
    path.split('.').take(depth).collect::<Vec<_>>().join(".")
}

fn spending(ctx: &Ctx, a: &Args) -> Result<Value> {
    let (from, to) = period_arg(ctx, a, "period", "this-month")?;
    let filter = filter_arg(ctx, a)?;
    let d = collect(ctx, from, to, &filter)?;
    let flow = if a.opt_str("kind").as_deref() == Some("income") {
        Flow::Income
    } else {
        Flow::Expense
    };
    let by = a.opt_str("by").unwrap_or_else(|| "category".into());
    let depth = a.opt_i64("depth")?.unwrap_or(0).clamp(0, 10) as usize;
    let top = a.opt_i64("top")?.unwrap_or(50).clamp(1, 1000) as usize;
    let rows = match by.as_str() {
        "category" => ranked(
            &d.rows,
            flow,
            |r| {
                let k = cat_key(r).remove(0);
                if depth > 0 && !k.starts_with('(') {
                    vec![depth_key(&k, depth)]
                } else {
                    vec![k]
                }
            },
            top,
            "name",
        ),
        "payee" => ranked(&d.rows, flow, payee_key, top, "payee"),
        "account" => ranked(&d.rows, flow, |r| vec![r.line.account.clone()], top, "name"),
        "tag" => ranked(
            &d.rows,
            flow,
            |r| {
                if r.line.tags.is_empty() {
                    vec!["(untagged)".into()]
                } else {
                    r.line.tags.clone()
                }
            },
            top,
            "name",
        ),
        "month" => ranked(
            &d.rows,
            flow,
            |r| vec![pext_core::dates::ym(r.line.date)],
            top,
            "name",
        ),
        _ => unreachable!("validated by choices"),
    };
    Ok(json!({
        "period_start": fmt_date(from), "period_end": fmt_date(to), "currency": d.base,
        "kind": if flow == Flow::Income { "income" } else { "expense" }, "by": by,
        "total_minor": sum(&d.rows, flow), "rows": rows,
    }))
}

fn trend(ctx: &Ctx, a: &Args) -> Result<Value> {
    let months = a.opt_i64("months")?.unwrap_or(12).clamp(1, 120) as i32;
    let end_month = month_start(ctx.today);
    let first = add_months(end_month, -(months - 1))?;
    let filter = filter_arg(ctx, a)?;
    let d = collect(ctx, first, month_end(end_month), &filter)?;
    let mut out = vec![];
    let (mut ti, mut te) = (0i128, 0i128);
    for i in 0..months {
        let ms = add_months(first, i)?;
        let me = month_end(ms);
        let rows: Vec<&Row> = d
            .rows
            .iter()
            .filter(|r| r.line.date >= ms && r.line.date <= me)
            .collect();
        let inc: i64 = rows
            .iter()
            .filter(|r| r.flow == Flow::Income)
            .map(|r| r.amount())
            .sum();
        let exp: i64 = rows
            .iter()
            .filter(|r| r.flow == Flow::Expense)
            .map(|r| r.amount())
            .sum();
        ti += inc as i128;
        te += exp as i128;
        out.push(json!({
            "month": pext_core::dates::ym(ms), "income_minor": inc, "expense_minor": exp, "net_minor": inc - exp,
            "savings_rate_pct": pct(inc - exp, inc),
        }));
    }
    let n = months as i128;
    Ok(json!({
        "currency": d.base, "months": out,
        "average": { "income_minor": (ti / n) as i64, "expense_minor": (te / n) as i64, "net_minor": ((ti - te) / n) as i64,
                     "savings_rate_pct": pct((ti - te) as i64, ti as i64) },
    }))
}

fn compare(ctx: &Ctx, a: &Args) -> Result<Value> {
    let (af, at) = period_arg(ctx, a, "a", "last-month")?;
    let (bf, bt) = period_arg(ctx, a, "b", "this-month")?;
    let filter = filter_arg(ctx, a)?;
    let da = collect(ctx, af, at, &filter)?;
    let db = collect(ctx, bf, bt, &filter)?;
    let depth = a.opt_i64("depth")?.unwrap_or(1).clamp(0, 10) as usize;
    let key = |r: &Row| {
        let k = cat_key(r).remove(0);
        vec![if depth > 0 && !k.starts_with('(') {
            depth_key(&k, depth)
        } else {
            k
        }]
    };
    let mut m: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for v in ranked(&da.rows, Flow::Expense, key, usize::MAX, "name") {
        m.entry(v["name"].as_str().unwrap_or("").into())
            .or_default()
            .0 = v["amount_minor"].as_i64().unwrap_or(0);
    }
    for v in ranked(&db.rows, Flow::Expense, key, usize::MAX, "name") {
        m.entry(v["name"].as_str().unwrap_or("").into())
            .or_default()
            .1 = v["amount_minor"].as_i64().unwrap_or(0);
    }
    let mut rows: Vec<Value> = m
        .into_iter()
        .map(|(k, (x, y))| json!({ "category": k, "a_minor": x, "b_minor": y, "change_minor": y - x, "change_pct": pct(y - x, x) }))
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r["change_minor"].as_i64().unwrap_or(0).abs()));
    Ok(json!({
        "currency": da.base,
        "a": { "start": fmt_date(af), "end": fmt_date(at), "expense_minor": sum(&da.rows, Flow::Expense) },
        "b": { "start": fmt_date(bf), "end": fmt_date(bt), "expense_minor": sum(&db.rows, Flow::Expense) },
        "categories": rows,
    }))
}

pub fn ops() -> Vec<OpSpec> {
    let common = |o: OpSpec| {
        o.scoped()
            .p(Param::s("account", "only these accounts (repeatable)").multi())
            .p(Param::s("query", "extra filter in the query language"))
    };
    vec![
        common(OpSpec::new(
            "report.summary",
            "Income, spending, savings rate and top movers for a period",
            summary,
        ))
        .p(Param::s(
            "period",
            "e.g. this-month (default), last-month, ytd, 2026-10, fy2026",
        )
        .def("this-month"))
        .ex("pext report summary --period last-month"),
        common(OpSpec::new(
            "report.spending",
            "Spending (or income) broken down by category, payee, account, tag or month",
            spending,
        ))
        .p(Param::s("period", "reporting period (default this-month)").def("this-month"))
        .p(
            Param::s("by", "category (default), payee, account, tag or month")
                .choices(&["category", "payee", "account", "tag", "month"])
                .def("category"),
        )
        .p(Param::s("kind", "expense (default) or income").choices(&["expense", "income"]))
        .p(Param::i(
            "depth",
            "roll categories up to this many levels (e.g. 1 = food)",
        ))
        .p(Param::i("top", "show only the biggest N rows")),
        common(OpSpec::new(
            "report.trend",
            "Month-by-month income, spending and savings rate",
            trend,
        ))
        .p(Param::i(
            "months",
            "number of months ending this month (default 12)",
        )),
        common(OpSpec::new(
            "report.compare",
            "Compare spending between two periods by category",
            compare,
        ))
        .p(Param::s("a", "first period (default last-month)"))
        .p(Param::s("b", "second period (default this-month)"))
        .p(Param::i("depth", "category depth (default 1)")),
    ]
}
