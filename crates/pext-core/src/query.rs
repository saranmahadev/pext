//! Query model: a ledger "line" (one split of one transaction), filter terms,
//! and pure evaluation/aggregation. The text parser lives in `qlang.rs`.

use crate::dates::{parse_date, quarter_start, week_start, ym};
use crate::error::{Error, Result};
use crate::money::{convert_minor, decimals_for, mul_div_round, parse_decimal};
use chrono::{Datelike, NaiveDate};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};

/// One split of one transaction, denormalised for filtering and grouping.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub txn_id: i64,
    pub split_id: i64,
    pub date: NaiveDate,
    pub account_id: i64,
    pub account: String,
    pub currency: String,
    pub payee: String,
    pub memo: String,
    pub status: String,
    pub kind: String,
    pub txn_amount_minor: i64,
    pub amount_minor: i64,
    pub category: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Account,
    Category,
    Payee,
    Memo,
    Tag,
    Status,
    Type,
    Currency,
    Amount,
    Signed,
    Date,
    Id,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Term {
    pub field: Field,
    pub op: Op,
    pub values: Vec<String>,
    pub negate: bool,
}

impl Term {
    pub fn new(field: Field, op: Op, values: Vec<String>) -> Self {
        Term {
            field,
            op,
            values,
            negate: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKey {
    Day,
    Week,
    Month,
    Quarter,
    Year,
    Category,
    Payee,
    Account,
    Tag,
    Currency,
}

impl GroupKey {
    pub fn name(self) -> &'static str {
        match self {
            GroupKey::Day => "day",
            GroupKey::Week => "week",
            GroupKey::Month => "month",
            GroupKey::Quarter => "quarter",
            GroupKey::Year => "year",
            GroupKey::Category => "category",
            GroupKey::Payee => "payee",
            GroupKey::Account => "account",
            GroupKey::Tag => "tag",
            GroupKey::Currency => "currency",
        }
    }
    pub fn parse(s: &str) -> Option<GroupKey> {
        Some(match s.to_ascii_lowercase().as_str() {
            "day" => GroupKey::Day,
            "week" => GroupKey::Week,
            "month" => GroupKey::Month,
            "quarter" => GroupKey::Quarter,
            "year" => GroupKey::Year,
            "category" => GroupKey::Category,
            "payee" => GroupKey::Payee,
            "account" => GroupKey::Account,
            "tag" => GroupKey::Tag,
            "currency" => GroupKey::Currency,
            _ => return None,
        })
    }
    pub fn is_time(self) -> bool {
        matches!(
            self,
            GroupKey::Day | GroupKey::Week | GroupKey::Month | GroupKey::Quarter | GroupKey::Year
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    Sum,
    Count,
    Avg,
    Min,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Date,
    Amount,
    Sum,
    Count,
    Key,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Query {
    /// OR of AND-groups. Empty means "match everything".
    pub any_of: Vec<Vec<Term>>,
    pub group_by: Vec<GroupKey>,
    pub aggs: Vec<Agg>,
    pub sort: Option<(SortKey, bool)>,
    pub limit: Option<usize>,
    pub convert: Option<String>,
    pub include_transfers: bool,
}

impl Query {
    /// AND a term onto every OR-group (or start a group).
    pub fn and(&mut self, t: Term) {
        if self.any_of.is_empty() {
            self.any_of.push(vec![t]);
        } else {
            for g in &mut self.any_of {
                g.push(t.clone());
            }
        }
    }

    /// Conservative date bounds usable to pre-filter rows in SQL: a bound is
    /// only returned when every OR-group constrains it.
    pub fn date_bounds(&self) -> (Option<NaiveDate>, Option<NaiveDate>) {
        if self.any_of.is_empty() {
            return (None, None);
        }
        let mut lo: Option<Option<NaiveDate>> = None;
        let mut hi: Option<Option<NaiveDate>> = None;
        for g in &self.any_of {
            let mut glo: Option<NaiveDate> = None;
            let mut ghi: Option<NaiveDate> = None;
            for t in g.iter().filter(|t| t.field == Field::Date && !t.negate) {
                let Some(Ok(v)) = t.values.first().map(|v| parse_date(v)) else {
                    continue;
                };
                match t.op {
                    Op::Ge | Op::Gt | Op::Eq => glo = Some(glo.map_or(v, |c| c.max(v))),
                    _ => {}
                }
                match t.op {
                    Op::Le | Op::Lt | Op::Eq => ghi = Some(ghi.map_or(v, |c| c.min(v))),
                    _ => {}
                }
            }
            lo = Some(match (lo, glo) {
                (None, g) => g,
                (Some(Some(a)), Some(b)) => Some(a.min(b)),
                _ => None,
            });
            hi = Some(match (hi, ghi) {
                (None, g) => g,
                (Some(Some(a)), Some(b)) => Some(a.max(b)),
                _ => None,
            });
        }
        (lo.flatten(), hi.flatten())
    }

    pub fn has_type_term(&self) -> bool {
        self.any_of.iter().flatten().any(|t| t.field == Field::Type)
    }
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(&needle.to_lowercase())
}

fn str_op(op: Op, values: &[String], hay: &str, field: &str) -> Result<bool> {
    match op {
        Op::Eq => Ok(values.iter().any(|v| eq_ci(v, hay))),
        Op::Ne => Ok(!values.iter().any(|v| eq_ci(v, hay))),
        Op::Contains => Ok(values.iter().any(|v| contains_ci(hay, v))),
        _ => Err(Error::usage(
            "invalid_operator",
            format!("operator not supported for field '{field}'"),
        )),
    }
}

fn cmp_op<T: PartialOrd>(op: Op, a: &T, b: &T) -> bool {
    match op {
        Op::Eq => a == b,
        Op::Ne => a != b,
        Op::Lt => a < b,
        Op::Le => a <= b,
        Op::Gt => a > b,
        Op::Ge => a >= b,
        Op::Contains => false,
    }
}

fn type_matches(v: &str, l: &Line) -> Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "expense" => Ok(l.kind == "standard" && l.txn_amount_minor < 0),
        "income" => Ok(l.kind == "standard" && l.txn_amount_minor > 0),
        "transfer" => Ok(l.kind == "transfer"),
        "adjustment" => Ok(l.kind == "adjustment"),
        other => Err(Error::usage(
            "invalid_type",
            format!("unknown type '{other}' (use expense, income, transfer or adjustment)"),
        )),
    }
}

pub fn term_matches(t: &Term, l: &Line) -> Result<bool> {
    let first = || {
        t.values
            .first()
            .ok_or_else(|| Error::usage("missing_value", "term has no value"))
    };
    let r = match t.field {
        Field::Account => {
            let by_name = str_op(t.op, &t.values, &l.account, "account")?;
            match t.op {
                Op::Eq => by_name || t.values.iter().any(|v| *v == l.account_id.to_string()),
                Op::Ne => by_name && !t.values.iter().any(|v| *v == l.account_id.to_string()),
                _ => by_name,
            }
        }
        Field::Category => match t.op {
            Op::Eq | Op::Ne => {
                let hit = t.values.iter().any(|v| {
                    let v = v.to_lowercase();
                    match (&l.category, v.as_str()) {
                        (None, "none" | "uncategorized" | "null") => true,
                        (Some(c), _) => *c == v || c.starts_with(&format!("{v}.")),
                        _ => false,
                    }
                });
                if t.op == Op::Eq {
                    hit
                } else {
                    !hit
                }
            }
            Op::Contains => l
                .category
                .as_deref()
                .is_some_and(|c| t.values.iter().any(|v| contains_ci(c, v))),
            _ => {
                return Err(Error::usage(
                    "invalid_operator",
                    "operator not supported for field 'category'",
                ))
            }
        },
        Field::Payee => str_op(t.op, &t.values, &l.payee, "payee")?,
        Field::Memo => str_op(t.op, &t.values, &l.memo, "memo")?,
        Field::Tag => {
            let any = l
                .tags
                .iter()
                .any(|tag| str_op(Op::Eq, &t.values, tag, "tag").unwrap_or(false))
                || (t.op == Op::Contains
                    && l.tags
                        .iter()
                        .any(|tag| t.values.iter().any(|v| contains_ci(tag, v))));
            match t.op {
                Op::Ne => !any,
                Op::Eq | Op::Contains => any,
                _ => {
                    return Err(Error::usage(
                        "invalid_operator",
                        "operator not supported for field 'tag'",
                    ))
                }
            }
        }
        Field::Status => str_op(t.op, &t.values, &l.status, "status")?,
        Field::Currency => str_op(t.op, &t.values, &l.currency, "currency")?,
        Field::Type => {
            let mut any = false;
            for v in &t.values {
                if type_matches(v, l)? {
                    any = true;
                }
            }
            match t.op {
                Op::Eq => any,
                Op::Ne => !any,
                _ => {
                    return Err(Error::usage(
                        "invalid_operator",
                        "operator not supported for field 'type'",
                    ))
                }
            }
        }
        Field::Amount | Field::Signed => {
            let dec = parse_decimal(first()?)?;
            let d = decimals_for(&l.currency);
            let v = if t.field == Field::Amount {
                l.txn_amount_minor.checked_abs().unwrap_or(i64::MAX)
            } else {
                l.txn_amount_minor
            };
            // cmp_minor orders the minor-unit amount relative to the decimal.
            let ord = dec.cmp_minor(v, d);
            match t.op {
                Op::Eq => ord.is_eq(),
                Op::Ne => ord.is_ne(),
                Op::Lt => ord.is_lt(),
                Op::Le => ord.is_le(),
                Op::Gt => ord.is_gt(),
                Op::Ge => ord.is_ge(),
                Op::Contains => {
                    return Err(Error::usage(
                        "invalid_operator",
                        "operator not supported for amounts",
                    ))
                }
            }
        }
        Field::Date => {
            let v = parse_date(first()?)?;
            cmp_op(t.op, &l.date, &v)
        }
        Field::Id => {
            let v: i64 = first()?
                .parse()
                .map_err(|_| Error::usage("invalid_id", "id must be an integer"))?;
            cmp_op(t.op, &l.txn_id, &v)
        }
    };
    Ok(if t.negate { !r } else { r })
}

pub fn line_matches(q: &Query, l: &Line) -> Result<bool> {
    if !q.include_transfers && l.kind == "transfer" {
        return Ok(false);
    }
    if q.any_of.is_empty() {
        return Ok(true);
    }
    for group in &q.any_of {
        let mut all = true;
        for t in group {
            if !term_matches(t, l)? {
                all = false;
                break;
            }
        }
        if all {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn select<'a>(q: &Query, lines: &'a [Line]) -> Result<Vec<&'a Line>> {
    let mut out = Vec::new();
    for l in lines {
        if line_matches(q, l)? {
            out.push(l);
        }
    }
    Ok(out)
}

fn key_labels(g: GroupKey, l: &Line) -> Vec<String> {
    match g {
        GroupKey::Day => vec![l.date.format("%Y-%m-%d").to_string()],
        GroupKey::Week => vec![week_start(l.date).format("%Y-%m-%d").to_string()],
        GroupKey::Month => vec![ym(l.date)],
        GroupKey::Quarter => {
            let q = quarter_start(l.date);
            vec![format!("{}-Q{}", q.year(), (q.month() - 1) / 3 + 1)]
        }
        GroupKey::Year => vec![format!("{:04}", l.date.year())],
        GroupKey::Category => vec![l
            .category
            .clone()
            .unwrap_or_else(|| "(uncategorized)".to_string())],
        GroupKey::Payee => vec![if l.payee.is_empty() {
            "(none)".to_string()
        } else {
            l.payee.clone()
        }],
        GroupKey::Account => vec![l.account.clone()],
        GroupKey::Tag => {
            if l.tags.is_empty() {
                vec!["(untagged)".to_string()]
            } else {
                l.tags.clone()
            }
        }
        GroupKey::Currency => vec![l.currency.clone()],
    }
}

struct Acc {
    sum: i128,
    n: i128,
    ids: HashSet<i64>,
    min: i64,
    max: i64,
}

/// Aggregate selected lines. `fx(from, to, date)` returns a rate scaled by 1e8.
pub fn aggregate(
    q: &Query,
    lines: &[&Line],
    fx: &dyn Fn(&str, &str, NaiveDate) -> Result<i64>,
) -> Result<Vec<Map<String, Value>>> {
    let aggs: Vec<Agg> = if q.aggs.is_empty() {
        vec![Agg::Sum, Agg::Count]
    } else {
        q.aggs.clone()
    };
    let mut groups: BTreeMap<Vec<String>, Acc> = BTreeMap::new();
    for l in lines {
        let (amt, cur) = match &q.convert {
            Some(t) if *t != l.currency => {
                let rate = fx(&l.currency, t, l.date)?;
                (
                    convert_minor(
                        l.amount_minor,
                        decimals_for(&l.currency),
                        decimals_for(t),
                        rate,
                    )?,
                    t.clone(),
                )
            }
            Some(t) => (l.amount_minor, t.clone()),
            None => (l.amount_minor, l.currency.clone()),
        };
        let mut combos: Vec<Vec<String>> = vec![vec![]];
        for g in &q.group_by {
            let labels = key_labels(*g, l);
            let mut next = Vec::new();
            for c in &combos {
                for lab in &labels {
                    let mut v = c.clone();
                    v.push(lab.clone());
                    next.push(v);
                }
            }
            combos = next;
        }
        for mut key in combos {
            key.push(cur.clone());
            let a = groups.entry(key).or_insert(Acc {
                sum: 0,
                n: 0,
                ids: HashSet::new(),
                min: i64::MAX,
                max: i64::MIN,
            });
            a.sum += amt as i128;
            a.n += 1;
            a.ids.insert(l.txn_id);
            a.min = a.min.min(amt);
            a.max = a.max.max(amt);
        }
    }
    let mut rows: Vec<Map<String, Value>> = Vec::new();
    for (key, a) in groups {
        let mut m = Map::new();
        for (i, g) in q.group_by.iter().enumerate() {
            m.insert(g.name().to_string(), json!(key[i]));
        }
        m.insert("currency".into(), json!(key[key.len() - 1]));
        for ag in &aggs {
            match ag {
                Agg::Count => {
                    m.insert("count".into(), json!(a.ids.len()));
                }
                Agg::Sum => {
                    m.insert(
                        "sum_minor".into(),
                        json!(i64::try_from(a.sum).map_err(|_| {
                            Error::validation("amount_overflow", "aggregate is too large")
                        })?),
                    );
                }
                Agg::Avg => {
                    let v = mul_div_round(a.sum, 1, a.n.max(1))?;
                    m.insert("avg_minor".into(), json!(v as i64));
                }
                Agg::Min => {
                    m.insert("min_minor".into(), json!(a.min));
                }
                Agg::Max => {
                    m.insert("max_minor".into(), json!(a.max));
                }
            }
        }
        rows.push(m);
    }
    let default_time = q.group_by.first().is_some_and(|g| g.is_time());
    let (sk, desc) = q.sort.unwrap_or(if default_time {
        (SortKey::Key, false)
    } else {
        (SortKey::Sum, true)
    });
    let num = |m: &Map<String, Value>, k: &str| m.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    match sk {
        SortKey::Sum | SortKey::Amount => rows.sort_by_key(|m| num(m, "sum_minor").abs()),
        SortKey::Count => rows.sort_by_key(|m| num(m, "count")),
        SortKey::Key | SortKey::Date => {
            let gk = q.group_by.first().map(|g| g.name()).unwrap_or("currency");
            rows.sort_by(|a, b| {
                a.get(gk)
                    .and_then(|v| v.as_str())
                    .cmp(&b.get(gk).and_then(|v| v.as_str()))
            });
        }
    }
    let rows = if desc {
        rows.into_iter().rev().collect()
    } else {
        rows
    };
    Ok(match q.limit {
        Some(n) => rows.into_iter().take(n).collect(),
        None => rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: i64, date: &str, amt: i64, cat: Option<&str>, payee: &str) -> Line {
        Line {
            txn_id: id,
            split_id: id,
            date: parse_date(date).unwrap(),
            account_id: 1,
            account: "hdfc".into(),
            currency: "INR".into(),
            payee: payee.into(),
            memo: String::new(),
            status: "cleared".into(),
            kind: "standard".into(),
            txn_amount_minor: amt,
            amount_minor: amt,
            category: cat.map(String::from),
            tags: vec![],
        }
    }

    fn no_fx(_: &str, _: &str, _: NaiveDate) -> Result<i64> {
        Err(Error::validation("fx_rate_missing", "no rate"))
    }

    #[test]
    fn category_subtree_matching() {
        let l = line(1, "2026-10-01", -500, Some("food.delivery"), "Swiggy");
        let t = |v: &str| Term::new(Field::Category, Op::Eq, vec![v.into()]);
        assert!(term_matches(&t("food"), &l).unwrap());
        assert!(term_matches(&t("food.delivery"), &l).unwrap());
        assert!(!term_matches(&t("foo"), &l).unwrap());
        assert!(!term_matches(&t("food.del"), &l).unwrap());
        let u = line(2, "2026-10-01", -500, None, "x");
        assert!(term_matches(&t("uncategorized"), &u).unwrap());
    }

    #[test]
    fn amount_is_magnitude_and_exact() {
        let l = line(1, "2026-10-01", -50000, None, "x");
        let gt = |v: &str| Term::new(Field::Amount, Op::Gt, vec![v.into()]);
        assert!(term_matches(&gt("499.99"), &l).unwrap());
        assert!(!term_matches(&gt("500"), &l).unwrap());
        let s = Term::new(Field::Signed, Op::Lt, vec!["0".into()]);
        assert!(term_matches(&s, &l).unwrap());
    }

    #[test]
    fn transfers_excluded_by_default() {
        let mut l = line(1, "2026-10-01", -500, None, "x");
        l.kind = "transfer".into();
        let q = Query::default();
        assert!(!line_matches(&q, &l).unwrap());
        let q = Query {
            include_transfers: true,
            ..Default::default()
        };
        assert!(line_matches(&q, &l).unwrap());
    }

    #[test]
    fn groups_by_month_and_never_mixes_currencies() {
        let mut a = line(1, "2026-10-01", -500, Some("food"), "a");
        let b = line(2, "2026-10-09", -700, Some("food"), "b");
        let mut c = line(3, "2026-11-02", -100, Some("food"), "c");
        a.currency = "USD".into();
        c.currency = "INR".into();
        let q = Query {
            group_by: vec![GroupKey::Month],
            ..Default::default()
        };
        let rows = aggregate(&q, &[&a, &b, &c], &no_fx).unwrap();
        assert_eq!(rows.len(), 3);
        let oct_inr = rows
            .iter()
            .find(|r| r["month"] == "2026-10" && r["currency"] == "INR")
            .unwrap();
        assert_eq!(oct_inr["sum_minor"], -700);
        assert_eq!(oct_inr["count"], 1);
    }

    #[test]
    fn conversion_uses_rate_and_rounds() {
        let mut a = line(1, "2026-10-01", -10000, Some("food"), "a");
        a.currency = "USD".into();
        let q = Query {
            group_by: vec![GroupKey::Category],
            convert: Some("INR".into()),
            ..Default::default()
        };
        let fx = |_: &str, _: &str, _: NaiveDate| Ok(8_325_000_000);
        let rows = aggregate(&q, &[&a], &fx).unwrap();
        assert_eq!(rows[0]["sum_minor"], -832500);
        assert_eq!(rows[0]["currency"], "INR");
    }

    #[test]
    fn date_bounds_require_every_group() {
        let mut q = Query::default();
        q.and(Term::new(Field::Date, Op::Ge, vec!["2026-10-01".into()]));
        q.and(Term::new(Field::Date, Op::Le, vec!["2026-10-31".into()]));
        let (lo, hi) = q.date_bounds();
        assert_eq!(lo, Some(parse_date("2026-10-01").unwrap()));
        assert_eq!(hi, Some(parse_date("2026-10-31").unwrap()));
        q.any_of.push(vec![Term::new(
            Field::Payee,
            Op::Contains,
            vec!["x".into()],
        )]);
        assert_eq!(q.date_bounds(), (None, None));
    }
}
