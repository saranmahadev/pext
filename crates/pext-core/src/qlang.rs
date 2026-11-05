//! The pext query language: a small, forgiving text syntax that compiles to
//! [`Query`]. Examples:
//!
//! ```text
//! category:food date:this-month
//! payee:swiggy,zomato amount:>500 type:expense
//! -tag:work account:hdfc date:2026-10-01..2026-10-31
//! category:food or category:transport
//! swiggy                                  (free text: payee or memo contains)
//! category:food group:month,category sort:-sum limit:10 convert:USD
//! ```

use crate::dates::{
    add_days, add_months, days_in_month, month_end, month_start, parse_date, week_start,
};
use crate::error::{Error, Result};
use crate::money::{parse_decimal, validate_currency};
use crate::query::{Agg, Field, GroupKey, Op, Query, SortKey, Term};
use chrono::{Datelike, NaiveDate};

#[derive(Debug, Clone, Copy)]
pub struct ParseCtx {
    pub today: NaiveDate,
    /// First month of the financial year (1-12); 4 for the Indian FY.
    pub fy_start_month: u32,
    pub week_starts_sunday: bool,
}

impl ParseCtx {
    pub fn new(today: NaiveDate) -> Self {
        ParseCtx {
            today,
            fy_start_month: 4,
            week_starts_sunday: false,
        }
    }
}

fn err(code: &str, msg: impl Into<String>) -> Error {
    Error::usage(code, msg)
}

/// Split on whitespace, honouring "double" and 'single' quotes.
pub fn tokenize(s: &str) -> Result<Vec<String>> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in s.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                    started = true;
                } else if c.is_whitespace() {
                    if started || !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                        started = false;
                    }
                } else {
                    cur.push(c);
                }
            }
        }
    }
    if quote.is_some() {
        return Err(err("unterminated_quote", "the query has an unclosed quote")
            .with_hint("close the quote or remove it"));
    }
    if started || !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

fn fy_start(d: NaiveDate, m: u32) -> NaiveDate {
    let y = if d.month() >= m {
        d.year()
    } else {
        d.year() - 1
    };
    NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(d)
}

fn year_range(y: i32) -> Result<(NaiveDate, NaiveDate)> {
    let a = NaiveDate::from_ymd_opt(y, 1, 1)
        .ok_or_else(|| err("invalid_period", format!("year {y} is out of range")))?;
    let b = NaiveDate::from_ymd_opt(y, 12, 31)
        .ok_or_else(|| err("invalid_period", format!("year {y} is out of range")))?;
    Ok((a, b))
}

fn fy_range(start: NaiveDate) -> Result<(NaiveDate, NaiveDate)> {
    let end = add_days(add_months(start, 12)?, -1)?;
    Ok((start, end))
}

/// Parse a period such as `this-month`, `last-30d`, `2026-10`, `2026-Q2`,
/// `fy2026`, `ytd` or `2026-01-01..2026-03-31` into an inclusive range.
pub fn parse_period(s: &str, cx: &ParseCtx) -> Result<(NaiveDate, NaiveDate)> {
    let raw = s.trim();
    let l = raw.to_ascii_lowercase();
    let bad = || {
        err("invalid_period", format!("cannot understand period '{raw}'"))
            .with_hint("try this-month, last-month, last-30d, ytd, this-fy, 2026, 2026-10, 2026-Q2, fy2026 or A..B")
    };
    if let Some((a, b)) = l.split_once("..") {
        let start = if a.is_empty() {
            crate::dates::min_date()
        } else {
            parse_period(a, cx)?.0
        };
        let end = if b.is_empty() {
            cx.today
        } else {
            parse_period(b, cx)?.1
        };
        if start > end {
            return Err(err(
                "invalid_period",
                format!("period '{raw}' ends before it starts"),
            ));
        }
        return Ok((start, end));
    }
    let t = cx.today;
    let ws = |d: NaiveDate| {
        let w = week_start(d);
        if cx.week_starts_sunday {
            add_days(w, -1).unwrap_or(w)
        } else {
            w
        }
    };
    match l.as_str() {
        "today" => return Ok((t, t)),
        "yesterday" => {
            let y = add_days(t, -1)?;
            return Ok((y, y));
        }
        "this-week" | "week" => {
            let s = ws(t);
            return Ok((s, add_days(s, 6)?));
        }
        "last-week" => {
            let s = add_days(ws(t), -7)?;
            return Ok((s, add_days(s, 6)?));
        }
        "this-month" | "month" | "mtd" => {
            return Ok((month_start(t), if l == "mtd" { t } else { month_end(t) }));
        }
        "last-month" => {
            let m = add_months(month_start(t), -1)?;
            return Ok((m, month_end(m)));
        }
        "this-quarter" | "quarter" | "qtd" => {
            let s = crate::dates::quarter_start(t);
            let e = if l == "qtd" {
                t
            } else {
                add_days(add_months(s, 3)?, -1)?
            };
            return Ok((s, e));
        }
        "last-quarter" => {
            let s = add_months(crate::dates::quarter_start(t), -3)?;
            return Ok((s, add_days(add_months(s, 3)?, -1)?));
        }
        "this-year" | "year" => return year_range(t.year()),
        "ytd" => return Ok((year_range(t.year())?.0, t)),
        "last-year" => return year_range(t.year() - 1),
        "this-fy" | "fy" => return fy_range(fy_start(t, cx.fy_start_month)),
        "fytd" => return Ok((fy_start(t, cx.fy_start_month), t)),
        "last-fy" => return fy_range(add_months(fy_start(t, cx.fy_start_month), -12)?),
        "all" => return Ok((crate::dates::min_date(), t)),
        _ => {}
    }
    // last-30d, last-12w, last-6m, last-2y  (a rolling window ending today)
    if let Some(rest) = l.strip_prefix("last-") {
        if let Some(unit) = rest
            .chars()
            .last()
            .filter(|c| matches!(c, 'd' | 'w' | 'm' | 'y'))
        {
            let n: i64 = rest[..rest.len() - 1].parse().map_err(|_| bad())?;
            if !(1..=36_500).contains(&n) {
                return Err(bad());
            }
            let start = match unit {
                'd' => add_days(t, -(n - 1))?,
                'w' => add_days(t, -(7 * n - 1))?,
                'm' => add_days(add_months(t, -(n as i32))?, 1)?,
                _ => add_days(add_months(t, -(12 * n as i32))?, 1)?,
            };
            return Ok((start, t));
        }
        return Err(bad());
    }
    // fy2026 or fy2026-27 (the FY that starts in 2026)
    if let Some(rest) = l.strip_prefix("fy") {
        let y: i32 = rest.get(..4).and_then(|y| y.parse().ok()).ok_or_else(bad)?;
        let tail = &rest[4..];
        if !(tail.is_empty()
            || (tail.len() == 3
                && tail.starts_with('-')
                && tail[1..].chars().all(|c| c.is_ascii_digit())))
        {
            return Err(bad());
        }
        let start = NaiveDate::from_ymd_opt(y, cx.fy_start_month, 1).ok_or_else(bad)?;
        return fy_range(start);
    }
    // 2026-Q2
    if l.len() == 7 && l.as_bytes()[4] == b'-' && l.as_bytes()[5] == b'q' {
        let y: i32 = l[..4].parse().map_err(|_| bad())?;
        let q: u32 = l[6..].parse().map_err(|_| bad())?;
        if !(1..=4).contains(&q) {
            return Err(bad());
        }
        let s = NaiveDate::from_ymd_opt(y, (q - 1) * 3 + 1, 1).ok_or_else(bad)?;
        return Ok((s, add_days(add_months(s, 3)?, -1)?));
    }
    if l.len() == 4 && l.chars().all(|c| c.is_ascii_digit()) {
        return year_range(l.parse().map_err(|_| bad())?);
    }
    if l.len() == 7 && l.as_bytes()[4] == b'-' {
        let y: i32 = l[..4].parse().map_err(|_| bad())?;
        let m: u32 = l[5..].parse().map_err(|_| bad())?;
        if !(1..=12).contains(&m) {
            return Err(bad());
        }
        let s = NaiveDate::from_ymd_opt(y, m, 1).ok_or_else(bad)?;
        return Ok((
            s,
            NaiveDate::from_ymd_opt(y, m, days_in_month(y, m)).ok_or_else(bad)?,
        ));
    }
    if l.len() == 10 {
        let d = parse_date(&l).map_err(|_| bad())?;
        return Ok((d, d));
    }
    Err(bad())
}

fn field_of(name: &str) -> Option<Field> {
    Some(match name {
        "account" | "acct" => Field::Account,
        "category" | "cat" => Field::Category,
        "payee" => Field::Payee,
        "memo" | "note" => Field::Memo,
        "tag" => Field::Tag,
        "status" => Field::Status,
        "type" => Field::Type,
        "currency" | "cur" => Field::Currency,
        "amount" => Field::Amount,
        "signed" => Field::Signed,
        "date" => Field::Date,
        "id" => Field::Id,
        _ => return None,
    })
}

fn split_op(v: &str) -> (Option<Op>, &str) {
    for (p, op) in [
        (">=", Op::Ge),
        ("<=", Op::Le),
        ("!=", Op::Ne),
        (">", Op::Gt),
        ("<", Op::Lt),
        ("~", Op::Contains),
        ("=", Op::Eq),
    ] {
        if let Some(r) = v.strip_prefix(p) {
            return (Some(op), r);
        }
    }
    (None, v)
}

fn fmt_date(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

#[derive(Default)]
struct Builder {
    /// Segments separated by `or`; each segment is a list of alternative AND-groups.
    segments: Vec<Vec<Vec<Term>>>,
}

impl Builder {
    fn new() -> Self {
        Builder {
            segments: vec![vec![vec![]]],
        }
    }
    fn or(&mut self) {
        self.segments.push(vec![vec![]]);
    }
    fn add(&mut self, t: Term) {
        if let Some(seg) = self.segments.last_mut() {
            for g in seg.iter_mut() {
                g.push(t.clone());
            }
        }
    }
    /// AND a disjunction: each existing group is duplicated per alternative.
    fn add_any(&mut self, alts: Vec<Term>) {
        if let Some(seg) = self.segments.last_mut() {
            let mut next = vec![];
            for g in seg.iter() {
                for a in &alts {
                    let mut ng = g.clone();
                    ng.push(a.clone());
                    next.push(ng);
                }
            }
            *seg = next;
        }
    }
    fn finish(self) -> Vec<Vec<Term>> {
        let groups: Vec<Vec<Term>> = self.segments.into_iter().flatten().collect();
        if groups.iter().all(|g| g.is_empty()) {
            vec![]
        } else {
            groups
        }
    }
}

const MAX_TOKENS: usize = 200;

pub fn parse(text: &str, cx: &ParseCtx) -> Result<Query> {
    let toks = tokenize(text)?;
    if toks.len() > MAX_TOKENS {
        return Err(err(
            "query_too_long",
            format!("a query may have at most {MAX_TOKENS} terms"),
        ));
    }
    let mut b = Builder::new();
    let mut q = Query::default();
    for tok in toks {
        let lower = tok.to_ascii_lowercase();
        match lower.as_str() {
            "or" | "||" => {
                b.or();
                continue;
            }
            "and" | "&&" => continue,
            "transfers" | "include-transfers" => {
                q.include_transfers = true;
                continue;
            }
            _ => {}
        }
        let (negate, body) = match tok.strip_prefix('-').or_else(|| tok.strip_prefix('!')) {
            Some(r) if !r.is_empty() && !r.chars().next().is_some_and(|c| c.is_ascii_digit()) => {
                (true, r.to_string())
            }
            _ => (false, tok.clone()),
        };
        let Some((key, val)) = body.split_once(':') else {
            // free text: payee or memo contains
            if body.is_empty() {
                continue;
            }
            let mut p = Term::new(Field::Payee, Op::Contains, vec![body.clone()]);
            let mut m = Term::new(Field::Memo, Op::Contains, vec![body]);
            if negate {
                // not in payee AND not in memo
                p.negate = true;
                m.negate = true;
                b.add(p);
                b.add(m);
            } else {
                b.add_any(vec![p, m]);
            }
            continue;
        };
        let key_l = key.to_ascii_lowercase();
        match key_l.as_str() {
            "group" | "by" => {
                for g in val.split(',').filter(|s| !s.is_empty()) {
                    q.group_by.push(GroupKey::parse(g).ok_or_else(|| {
                        err("invalid_group", format!("cannot group by '{g}'"))
                            .with_hint("use day, week, month, quarter, year, category, payee, account, tag or currency")
                    })?);
                }
                continue;
            }
            "agg" => {
                for a in val.split(',').filter(|s| !s.is_empty()) {
                    q.aggs.push(match a.to_ascii_lowercase().as_str() {
                        "sum" => Agg::Sum,
                        "count" => Agg::Count,
                        "avg" => Agg::Avg,
                        "min" => Agg::Min,
                        "max" => Agg::Max,
                        other => {
                            return Err(err(
                                "invalid_aggregate",
                                format!("unknown aggregate '{other}'"),
                            )
                            .with_hint("use sum, count, avg, min or max"))
                        }
                    });
                }
                continue;
            }
            "sort" | "order" => {
                let (desc, name) = match val.strip_prefix('-') {
                    Some(n) => (true, n),
                    None => (false, val.strip_prefix('+').unwrap_or(val)),
                };
                let k = match name.to_ascii_lowercase().as_str() {
                    "date" => SortKey::Date,
                    "amount" => SortKey::Amount,
                    "sum" | "total" => SortKey::Sum,
                    "count" => SortKey::Count,
                    "key" | "name" => SortKey::Key,
                    other => {
                        return Err(err("invalid_sort", format!("cannot sort by '{other}'"))
                            .with_hint("use date, amount, sum, count or key"))
                    }
                };
                q.sort = Some((k, desc));
                continue;
            }
            "limit" | "top" => {
                let n: usize = val
                    .parse()
                    .map_err(|_| err("invalid_limit", "limit must be a positive integer"))?;
                if n == 0 || n > 100_000 {
                    return Err(err("invalid_limit", "limit must be between 1 and 100000"));
                }
                q.limit = Some(n);
                continue;
            }
            "convert" | "in" => {
                q.convert = Some(validate_currency(val)?);
                continue;
            }
            "include" => {
                if val.eq_ignore_ascii_case("transfers") {
                    q.include_transfers = true;
                    continue;
                }
                return Err(err(
                    "invalid_include",
                    "include: only 'transfers' is supported",
                ));
            }
            _ => {}
        }
        let field = field_of(&key_l).ok_or_else(|| {
            err("unknown_field", format!("unknown query field '{key}'"))
                .with_hint("fields: account, category, payee, memo, tag, status, type, currency, amount, signed, date, id")
        })?;
        if val.is_empty() {
            return Err(err("missing_value", format!("'{key}:' needs a value")));
        }
        let (op, rest) = split_op(val);
        match field {
            Field::Date => {
                let mk = |op: Op, d: NaiveDate| {
                    let mut t = Term::new(Field::Date, op, vec![fmt_date(d)]);
                    t.negate = false;
                    t
                };
                let terms: Vec<Term> = match op {
                    Some(o @ (Op::Gt | Op::Ge | Op::Lt | Op::Le)) => {
                        let (s, e) = parse_period(rest, cx)?;
                        // >2026-10 means after the whole period; >= starts at its first day, etc.
                        let d = match o {
                            Op::Gt => e,
                            Op::Ge => s,
                            Op::Lt => s,
                            _ => e,
                        };
                        vec![mk(o, d)]
                    }
                    Some(Op::Eq) | None => {
                        let (s, e) = parse_period(rest, cx)?;
                        vec![mk(Op::Ge, s), mk(Op::Le, e)]
                    }
                    Some(Op::Ne) => {
                        let (s, e) = parse_period(rest, cx)?;
                        // outside the period: before OR after (two alternatives)
                        let mut before = mk(Op::Lt, s);
                        let mut after = mk(Op::Gt, e);
                        before.negate = negate;
                        after.negate = negate;
                        b.add_any(vec![before, after]);
                        continue;
                    }
                    Some(Op::Contains) => {
                        return Err(err("invalid_operator", "'~' does not apply to dates"))
                    }
                };
                if negate {
                    // NOT (a AND b) == NOT a OR NOT b
                    let alts = terms
                        .into_iter()
                        .map(|mut t| {
                            t.negate = true;
                            t
                        })
                        .collect();
                    b.add_any(alts);
                } else {
                    for t in terms {
                        b.add(t);
                    }
                }
            }
            Field::Amount | Field::Signed => {
                if let (None, Some((lo, hi))) = (op, rest.split_once("..")) {
                    for (o, v) in [(Op::Ge, lo), (Op::Le, hi)] {
                        if !v.is_empty() {
                            parse_decimal(v)?;
                            let mut t = Term::new(field, o, vec![v.to_string()]);
                            t.negate = negate;
                            if negate {
                                // handled below as a single alternative set
                            }
                            b.add(t);
                        }
                    }
                    continue;
                }
                parse_decimal(rest)?;
                let mut t = Term::new(field, op.unwrap_or(Op::Eq), vec![rest.to_string()]);
                t.negate = negate;
                b.add(t);
            }
            Field::Id => {
                rest.parse::<i64>()
                    .map_err(|_| err("invalid_id", "id must be an integer"))?;
                let mut t = Term::new(field, op.unwrap_or(Op::Eq), vec![rest.to_string()]);
                t.negate = negate;
                b.add(t);
            }
            _ => {
                let values: Vec<String> = rest
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                if values.is_empty() {
                    return Err(err("missing_value", format!("'{key}:' needs a value")));
                }
                let default_op = match field {
                    Field::Payee | Field::Memo => Op::Contains,
                    _ => Op::Eq,
                };
                let o = op.unwrap_or(default_op);
                if matches!(o, Op::Lt | Op::Le | Op::Gt | Op::Ge) {
                    return Err(err(
                        "invalid_operator",
                        format!("'{key}' cannot be compared with < or >"),
                    ));
                }
                let mut t = Term::new(field, o, values);
                t.negate = negate;
                b.add(t);
            }
        }
    }
    q.any_of = b.finish();
    Ok(q)
}

/// Combine two queries: every group of `extra` is ANDed with every group of
/// `base`. Presentation settings from `extra` win when set.
pub fn merge(base: Query, extra: Query) -> Query {
    let any_of = match (base.any_of.is_empty(), extra.any_of.is_empty()) {
        (true, _) => extra.any_of,
        (_, true) => base.any_of,
        _ => {
            let mut out = vec![];
            for a in &base.any_of {
                for e in &extra.any_of {
                    let mut g = a.clone();
                    g.extend(e.iter().cloned());
                    out.push(g);
                }
            }
            out
        }
    };
    Query {
        any_of,
        group_by: if extra.group_by.is_empty() {
            base.group_by
        } else {
            extra.group_by
        },
        aggs: if extra.aggs.is_empty() {
            base.aggs
        } else {
            extra.aggs
        },
        sort: extra.sort.or(base.sort),
        limit: extra.limit.or(base.limit),
        convert: extra.convert.or(base.convert),
        include_transfers: base.include_transfers || extra.include_transfers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{line_matches, Line};

    fn cx() -> ParseCtx {
        ParseCtx::new(NaiveDate::from_ymd_opt(2026, 11, 5).unwrap())
    }
    fn d(y: i32, m: u32, dd: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, dd).unwrap()
    }
    fn line(date: NaiveDate, payee: &str, cat: Option<&str>, amt: i64) -> Line {
        Line {
            txn_id: 1,
            split_id: 1,
            date,
            account_id: 1,
            account: "HDFC".into(),
            currency: "INR".into(),
            payee: payee.into(),
            memo: "lunch with team".into(),
            status: "cleared".into(),
            kind: "standard".into(),
            txn_amount_minor: amt,
            amount_minor: amt,
            category: cat.map(String::from),
            tags: vec!["work".into()],
        }
    }
    fn m(q: &str, l: &Line) -> bool {
        line_matches(&parse(q, &cx()).unwrap(), l).unwrap()
    }

    #[test]
    fn periods() {
        let p = |s: &str| parse_period(s, &cx()).unwrap();
        assert_eq!(p("this-month"), (d(2026, 11, 1), d(2026, 11, 30)));
        assert_eq!(p("last-month"), (d(2026, 10, 1), d(2026, 10, 31)));
        assert_eq!(p("mtd"), (d(2026, 11, 1), d(2026, 11, 5)));
        assert_eq!(p("ytd"), (d(2026, 1, 1), d(2026, 11, 5)));
        assert_eq!(p("this-year"), (d(2026, 1, 1), d(2026, 12, 31)));
        assert_eq!(p("last-year"), (d(2025, 1, 1), d(2025, 12, 31)));
        assert_eq!(p("this-quarter"), (d(2026, 10, 1), d(2026, 12, 31)));
        assert_eq!(p("last-quarter"), (d(2026, 7, 1), d(2026, 9, 30)));
        assert_eq!(p("this-fy"), (d(2026, 4, 1), d(2027, 3, 31)));
        assert_eq!(p("last-fy"), (d(2025, 4, 1), d(2026, 3, 31)));
        assert_eq!(p("fy2025"), (d(2025, 4, 1), d(2026, 3, 31)));
        assert_eq!(p("FY2025-26"), (d(2025, 4, 1), d(2026, 3, 31)));
        assert_eq!(p("last-30d"), (d(2026, 10, 7), d(2026, 11, 5)));
        assert_eq!(p("last-1d"), (d(2026, 11, 5), d(2026, 11, 5)));
        assert_eq!(p("last-2w"), (d(2026, 10, 23), d(2026, 11, 5)));
        assert_eq!(p("last-3m"), (d(2026, 8, 6), d(2026, 11, 5)));
        assert_eq!(p("2026"), (d(2026, 1, 1), d(2026, 12, 31)));
        assert_eq!(p("2024-02"), (d(2024, 2, 1), d(2024, 2, 29)));
        assert_eq!(p("2026-Q2"), (d(2026, 4, 1), d(2026, 6, 30)));
        assert_eq!(p("2026-03-15"), (d(2026, 3, 15), d(2026, 3, 15)));
        assert_eq!(p("2026-01..2026-03"), (d(2026, 1, 1), d(2026, 3, 31)));
        assert_eq!(p("2026-01-10..today"), (d(2026, 1, 10), d(2026, 11, 5)));
        assert_eq!(p("..2026-02"), (crate::dates::min_date(), d(2026, 2, 28)));
        assert_eq!(p("yesterday"), (d(2026, 11, 4), d(2026, 11, 4)));
        // Thursday 2026-11-05: Monday-start week
        assert_eq!(p("this-week"), (d(2026, 11, 2), d(2026, 11, 8)));
        let mut sun = cx();
        sun.week_starts_sunday = true;
        assert_eq!(
            parse_period("this-week", &sun).unwrap(),
            (d(2026, 11, 1), d(2026, 11, 7))
        );
    }

    #[test]
    fn fy_with_other_start_month() {
        let mut c = cx();
        c.fy_start_month = 1;
        assert_eq!(
            parse_period("this-fy", &c).unwrap(),
            (d(2026, 1, 1), d(2026, 12, 31))
        );
        c.fy_start_month = 7;
        assert_eq!(
            parse_period("this-fy", &c).unwrap(),
            (d(2026, 7, 1), d(2027, 6, 30))
        );
        assert_eq!(
            parse_period("fy2025", &c).unwrap(),
            (d(2025, 7, 1), d(2026, 6, 30))
        );
    }

    #[test]
    fn bad_periods_are_rejected() {
        for s in [
            "",
            "soon",
            "2026-13",
            "2026-00",
            "2026-Q5",
            "last-0d",
            "last-5x",
            "last-d",
            "fy20",
            "2026-02-30",
            "2026-03..2026-01",
            "last-99999999d",
        ] {
            assert!(parse_period(s, &cx()).is_err(), "{s} should fail");
        }
    }

    #[test]
    fn tokenizer() {
        assert_eq!(
            tokenize(r#"a "b c" d:'e f' "" g"#).unwrap(),
            vec!["a", "b c", "d:e f", "", "g"]
        );
        assert!(tokenize("\"open").is_err());
        assert!(tokenize("   ").unwrap().is_empty());
    }

    #[test]
    fn fields_and_operators() {
        let l = line(
            d(2026, 10, 15),
            "Swiggy Order",
            Some("food.delivery"),
            -45050,
        );
        assert!(m("category:food", &l));
        assert!(m("cat:food.delivery", &l));
        assert!(!m("category:transport", &l));
        assert!(m("category:transport,food", &l));
        assert!(m("payee:swiggy", &l));
        assert!(!m("payee:=swiggy", &l)); // exact match only
        assert!(m("payee:=\"swiggy order\"", &l));
        assert!(m("account:hdfc", &l));
        assert!(m("tag:work", &l));
        assert!(m("-tag:home", &l));
        assert!(!m("-tag:work", &l));
        assert!(m("type:expense", &l));
        assert!(!m("type:income", &l));
        assert!(m("status:cleared", &l));
        assert!(m("currency:inr", &l));
        assert!(m("amount:>400", &l));
        assert!(m("amount:>=450.50", &l));
        assert!(!m("amount:>450.50", &l));
        assert!(m("amount:400..500", &l));
        assert!(!m("amount:500..600", &l));
        assert!(m("amount:..500", &l));
        assert!(m("signed:<0", &l));
        assert!(m("id:1", &l));
        assert!(m("memo:lunch", &l));
        assert!(m("", &l));
    }

    #[test]
    fn dates() {
        let l = line(d(2026, 10, 15), "x", None, -100);
        assert!(m("date:2026-10", &l));
        assert!(m("date:last-month", &l));
        assert!(m("date:2026-10-15", &l));
        assert!(!m("date:2026-09", &l));
        assert!(m("date:>2026-09", &l));
        assert!(!m("date:>2026-10", &l)); // after the whole month
        assert!(m("date:>=2026-10", &l));
        assert!(m("date:<2026-11", &l));
        assert!(m("date:<=2026-10", &l));
        assert!(m("date:2026-10-01..2026-10-31", &l));
        assert!(m("-date:2026-09", &l));
        assert!(!m("-date:2026-10", &l));
        assert!(m("date:!=2026-09", &l));
        assert!(!m("date:!=2026-10", &l));
    }

    #[test]
    fn or_and_free_text() {
        let l = line(d(2026, 10, 15), "Swiggy", Some("food.delivery"), -10000);
        assert!(m("category:transport or category:food", &l));
        assert!(!m("category:transport or category:health", &l));
        assert!(m("swiggy", &l));
        assert!(m("lunch", &l)); // memo
        assert!(m("swiggy category:food", &l));
        assert!(!m("zomato", &l));
        assert!(m("zomato or swiggy", &l));
        assert!(m("-zomato", &l));
        assert!(!m("-swiggy", &l));
        assert!(m("category:food and amount:>50", &l));
        // AND binds tighter than OR
        assert!(m("category:health amount:>50 or payee:swiggy", &l));
        assert!(!m("category:health amount:>50 or payee:zomato", &l));
        // free text combined with OR segments
        assert!(m("category:health or swiggy", &l));
    }

    #[test]
    fn presentation_clauses() {
        let q = parse("category:food group:month,category agg:sum,count sort:-sum limit:5 convert:usd transfers", &cx()).unwrap();
        assert_eq!(q.group_by, vec![GroupKey::Month, GroupKey::Category]);
        assert_eq!(q.aggs, vec![Agg::Sum, Agg::Count]);
        assert_eq!(q.sort, Some((SortKey::Sum, true)));
        assert_eq!(q.limit, Some(5));
        assert_eq!(q.convert.as_deref(), Some("USD"));
        assert!(q.include_transfers);
        assert_eq!(q.any_of.len(), 1);
    }

    #[test]
    fn errors_are_specific() {
        let code = |s: &str| parse(s, &cx()).unwrap_err().code;
        assert_eq!(code("colour:red"), "unknown_field");
        assert_eq!(code("category:"), "missing_value");
        assert_eq!(code("group:fortnight"), "invalid_group");
        assert_eq!(code("agg:median"), "invalid_aggregate");
        assert_eq!(code("sort:nope"), "invalid_sort");
        assert_eq!(code("limit:0"), "invalid_limit");
        assert_eq!(code("limit:x"), "invalid_limit");
        assert_eq!(code("amount:>abc"), "invalid_amount");
        assert_eq!(code("amount:abc"), "invalid_amount");
        assert_eq!(code("id:abc"), "invalid_id");
        assert_eq!(code("payee:>5"), "invalid_operator");
        assert_eq!(code("date:~5"), "invalid_operator");
        assert_eq!(code("date:2026-13"), "invalid_period");
        assert_eq!(code("convert:xx"), "invalid_currency");
        assert_eq!(code("\"x"), "unterminated_quote");
    }

    #[test]
    fn merge_ands_groups() {
        let a = parse("category:food or category:health", &cx()).unwrap();
        let b = parse("amount:>100 limit:3", &cx()).unwrap();
        let q = merge(a, b);
        assert_eq!(q.any_of.len(), 2);
        assert!(q.any_of.iter().all(|g| g.len() == 2));
        assert_eq!(q.limit, Some(3));
        let none = merge(Query::default(), parse("payee:x", &cx()).unwrap());
        assert_eq!(none.any_of.len(), 1);
    }

    #[test]
    fn never_panics_on_garbage() {
        for s in [
            "::",
            ":::x",
            "-",
            "--",
            "!",
            "date:",
            "amount:..",
            "a:b:c",
            "\u{1F600}:x",
            "category:\"",
            "or or or",
            "-or",
            "limit:-1",
            "date:..",
            "date:....",
            "amount:>=",
            "((",
            "\0",
        ] {
            let _ = parse(s, &cx());
        }
    }
}
