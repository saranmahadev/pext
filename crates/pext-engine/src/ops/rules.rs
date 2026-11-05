//! Auto-categorisation rules. A rule is a query plus actions (set category,
//! rename payee, add tags). Rules run when a transaction is added or imported
//! without an explicit category, and can be applied to existing history.

use crate::args::Args;
use crate::ctx::{iv, oiv, osv, sv, Ctx};
use crate::ops::accounts::load_account;
use crate::ops::categories::{category_id, category_path};
use crate::ops::query::parse_text;
use crate::ops::txns::{fmt_date, load_matching, normalize_tag};
use crate::registry::{OpSpec, Param};
use chrono::NaiveDate;
use pext_core::query::{line_matches, Line, Query};
use pext_core::{Error, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Default, Clone)]
pub struct AutoResult {
    pub category_id: Option<i64>,
    pub payee: Option<String>,
    pub tags: Vec<String>,
    pub rule_id: Option<i64>,
}

pub struct AutoInput<'a> {
    pub account_id: i64,
    pub payee: &'a str,
    pub memo: &'a str,
    pub amount_minor: i64,
    pub date: NaiveDate,
}

struct Rule {
    id: i64,
    query: Query,
    category_id: Option<i64>,
    payee: Option<String>,
    tags: Vec<String>,
}

fn load_rules(ctx: &Ctx, only: Option<i64>, enabled_only: bool) -> Result<Vec<Rule>> {
    let rows = ctx.rows(
        "SELECT id, name, query, set_category_id, set_payee, add_tags, enabled FROM rules ORDER BY priority, id",
        &[],
    )?;
    let mut out = vec![];
    for r in rows {
        let id = r["id"].as_i64().unwrap_or(0);
        if only.is_some_and(|o| o != id) || (enabled_only && r["enabled"].as_i64() != Some(1)) {
            continue;
        }
        let name = r["name"].as_str().unwrap_or("").to_string();
        let query = match parse_text(ctx, r["query"].as_str().unwrap_or("")) {
            Ok(q) => q,
            Err(e) => {
                ctx.warn(
                    "rule_invalid",
                    format!("rule '{name}' was skipped: {}", e.message),
                    json!({ "rule_id": id }),
                );
                continue;
            }
        };
        out.push(Rule {
            id,
            query,
            category_id: r["set_category_id"].as_i64(),
            payee: r["set_payee"].as_str().map(String::from),
            tags: r["add_tags"]
                .as_str()
                .unwrap_or("")
                .split(',')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
        });
    }
    Ok(out)
}

fn evaluate(rules: &[Rule], line: &Line) -> Result<AutoResult> {
    let mut res = AutoResult::default();
    for r in rules {
        if !line_matches(&r.query, line)? {
            continue;
        }
        if res.category_id.is_none() {
            if let Some(c) = r.category_id {
                res.category_id = Some(c);
                res.rule_id = Some(r.id);
            }
        }
        if res.payee.is_none() {
            res.payee = r.payee.clone();
        }
        for t in &r.tags {
            if !res.tags.contains(t) {
                res.tags.push(t.clone());
            }
        }
    }
    Ok(res)
}

pub fn auto(ctx: &Ctx, input: &AutoInput) -> Result<AutoResult> {
    let rules = load_rules(ctx, None, true)?;
    if rules.is_empty() {
        return Ok(AutoResult::default());
    }
    let acct = load_account(ctx, input.account_id)?;
    let line = Line {
        txn_id: 0,
        split_id: 0,
        date: input.date,
        account_id: acct.id,
        account: acct.name,
        currency: acct.currency,
        payee: input.payee.to_string(),
        memo: input.memo.to_string(),
        status: "cleared".into(),
        kind: "standard".into(),
        txn_amount_minor: input.amount_minor,
        amount_minor: input.amount_minor,
        category: None,
        tags: vec![],
    };
    evaluate(&rules, &line)
}

// ---- operations -------------------------------------------------------------

fn validate_rule_query(ctx: &Ctx, text: &str) -> Result<()> {
    let q = parse_text(ctx, text)?;
    if q.any_of.is_empty() {
        return Err(
            Error::validation("empty_rule_query", "a rule needs at least one condition")
                .with_field("query")
                .with_hint("e.g. 'payee:swiggy' or 'memo:upi amount:>10000'"),
        );
    }
    if !q.group_by.is_empty()
        || !q.aggs.is_empty()
        || q.limit.is_some()
        || q.sort.is_some()
        || q.convert.is_some()
    {
        return Err(Error::validation(
            "invalid_rule_query",
            "rule queries may only contain filters, not group/agg/sort/limit/convert",
        )
        .with_field("query"));
    }
    Ok(())
}

fn tags_csv(a: &Args) -> Result<String> {
    let mut v = vec![];
    for t in a.list("tag") {
        let n = normalize_tag(&t)?;
        if !v.contains(&n) {
            v.push(n);
        }
    }
    Ok(v.join(","))
}

fn rule_json(ctx: &Ctx, id: i64) -> Result<Value> {
    let rows = ctx.rows("SELECT * FROM rules WHERE id=?1", &[iv(id)])?;
    let r = rows
        .first()
        .ok_or_else(|| Error::not_found("rule_not_found", format!("no rule #{id}")))?;
    let cat = match r["set_category_id"].as_i64() {
        Some(c) => Some(category_path(ctx, c)?),
        None => None,
    };
    Ok(json!({
        "id": id, "name": r["name"], "priority": r["priority"], "query": r["query"],
        "set_category": cat, "set_payee": r["set_payee"],
        "add_tags": r["add_tags"].as_str().unwrap_or("").split(',').filter(|s| !s.is_empty()).collect::<Vec<_>>(),
        "enabled": r["enabled"].as_i64() == Some(1),
    }))
}

fn find_rule(ctx: &Ctx, r: &str) -> Result<i64> {
    let by_id = r.trim().parse::<i64>().ok().and_then(|i| {
        ctx.opt_i64("SELECT id FROM rules WHERE id=?1", &[iv(i)])
            .ok()
            .flatten()
    });
    if let Some(i) = by_id {
        return Ok(i);
    }
    ctx.opt_i64("SELECT id FROM rules WHERE name=?1", &[sv(r.trim())])?
        .ok_or_else(|| {
            Error::not_found("rule_not_found", format!("no rule '{r}'"))
                .with_hint("run `pext rule list`")
        })
}

fn add(ctx: &Ctx, a: &Args) -> Result<Value> {
    let name = a.str("name")?.trim().to_string();
    let query = a.str("query")?;
    validate_rule_query(ctx, &query)?;
    let cat = match a.opt_str("category") {
        Some(c) => Some(category_id(ctx, &c)?),
        None => None,
    };
    let payee = a
        .opt_str("set_payee")
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());
    let tags = tags_csv(a)?;
    if cat.is_none() && payee.is_none() && tags.is_empty() {
        return Err(Error::usage(
            "missing_action",
            "a rule needs an action: --category, --set-payee or --tag",
        ));
    }
    let id = ctx.insert(
        "rules",
        &[
            ("name", sv(&name)),
            ("priority", iv(a.opt_i64("priority")?.unwrap_or(100))),
            ("query", sv(&query)),
            ("set_category_id", oiv(cat)),
            ("set_payee", osv(payee.as_deref())),
            ("add_tags", sv(&tags)),
            ("enabled", iv(1)),
            ("created_at", sv(&ctx.now)),
        ],
    )?;
    rule_json(ctx, id)
}

fn list(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let ids: Vec<i64> = ctx
        .rows("SELECT id FROM rules ORDER BY priority, id", &[])?
        .iter()
        .filter_map(|r| r["id"].as_i64())
        .collect();
    ids.into_iter()
        .map(|i| rule_json(ctx, i))
        .collect::<Result<Vec<_>>>()
        .map(Value::Array)
}

fn get(ctx: &Ctx, a: &Args) -> Result<Value> {
    rule_json(ctx, find_rule(ctx, &a.str("rule")?)?)
}

fn edit(ctx: &Ctx, a: &Args) -> Result<Value> {
    let id = find_rule(ctx, &a.str("rule")?)?;
    let mut cols = vec![];
    if let Some(n) = a.opt_str("new_name") {
        cols.push(("name", sv(n.trim())));
    }
    if let Some(q) = a.opt_str("query") {
        validate_rule_query(ctx, &q)?;
        cols.push(("query", sv(&q)));
    }
    if let Some(c) = a.opt_str("category") {
        if c.eq_ignore_ascii_case("none") {
            cols.push(("set_category_id", oiv(None)));
        } else {
            cols.push(("set_category_id", oiv(Some(category_id(ctx, &c)?))));
        }
    }
    if let Some(p) = a.opt_str("set_payee") {
        let p = p.trim();
        cols.push((
            "set_payee",
            osv(if p.is_empty() || p.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(p)
            }),
        ));
    }
    if a.has("tag") {
        cols.push(("add_tags", sv(&tags_csv(a)?)));
    }
    if let Some(p) = a.opt_i64("priority")? {
        cols.push(("priority", iv(p)));
    }
    if a.flag("enable") && a.flag("disable") {
        return Err(Error::usage(
            "conflicting_arguments",
            "use either --enable or --disable",
        ));
    }
    if a.flag("enable") {
        cols.push(("enabled", iv(1)));
    }
    if a.flag("disable") {
        cols.push(("enabled", iv(0)));
    }
    if cols.is_empty() {
        return Err(Error::usage(
            "nothing_to_change",
            "pass at least one field to change",
        ));
    }
    ctx.update("rules", id, &cols)?;
    let after = rule_json(ctx, id)?;
    if after["set_category"].is_null()
        && after["set_payee"].is_null()
        && after["add_tags"].as_array().is_some_and(|t| t.is_empty())
    {
        return Err(Error::validation(
            "missing_action",
            "a rule must keep at least one action",
        ));
    }
    Ok(after)
}

fn rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let id = find_rule(ctx, &a.str("rule")?)?;
    ctx.delete("rules", id)?;
    Ok(json!({ "removed": id }))
}

/// Group lines back into transactions (first split's line stands for it).
fn single_split_txns(lines: Vec<Line>) -> Vec<Line> {
    let mut count: HashMap<i64, usize> = HashMap::new();
    for l in &lines {
        *count.entry(l.txn_id).or_default() += 1;
    }
    lines
        .into_iter()
        .filter(|l| l.kind == "standard" && count[&l.txn_id] == 1)
        .collect()
}

fn test(ctx: &Ctx, a: &Args) -> Result<Value> {
    let q = match (a.opt_str("rule"), a.opt_str("query")) {
        (Some(r), None) => {
            let id = find_rule(ctx, &r)?;
            let qs = ctx
                .opt_string("SELECT query FROM rules WHERE id=?1", &[iv(id)])?
                .unwrap_or_default();
            parse_text(ctx, &qs)?
        }
        (None, Some(qs)) => {
            validate_rule_query(ctx, &qs)?;
            parse_text(ctx, &qs)?
        }
        _ => {
            return Err(Error::usage(
                "conflicting_arguments",
                "pass exactly one of --rule or --query",
            ))
        }
    };
    let matched = load_matching(ctx, &q)?;
    let n = matched.len();
    let sample: Vec<Value> = matched
        .iter()
        .rev()
        .take(a.opt_i64("limit")?.unwrap_or(10).clamp(1, 200) as usize)
        .map(|l| json!({ "txn_id": l.txn_id, "date": fmt_date(l.date), "payee": l.payee, "category": l.category, "amount_minor": l.amount_minor, "currency": l.currency }))
        .collect();
    let uncategorized = matched.iter().filter(|l| l.category.is_none()).count();
    Ok(json!({ "matches": n, "uncategorized": uncategorized, "sample": sample }))
}

fn apply(ctx: &Ctx, a: &Args) -> Result<Value> {
    let only = match a.opt_str("rule") {
        Some(r) => Some(find_rule(ctx, &r)?),
        None => None,
    };
    let rules = load_rules(ctx, only, true)?;
    let overwrite = a.flag("overwrite");
    let scope = match a.opt_str("query") {
        Some(qs) => parse_text(ctx, &qs)?,
        None => Query::default(),
    };
    let lines = single_split_txns(load_matching(ctx, &scope)?);
    let (mut recategorized, mut renamed, mut tagged, mut examined) = (0, 0, 0, 0);
    let mut changes: Vec<Value> = vec![];
    for l in lines {
        examined += 1;
        let res = evaluate(&rules, &l)?;
        let mut touched = false;
        if let Some(cid) = res.category_id {
            let cur = ctx.opt_i64(
                "SELECT category_id FROM splits WHERE id=?1",
                &[iv(l.split_id)],
            )?;
            if cur != Some(cid) && (cur.is_none() || overwrite) {
                ctx.update("splits", l.split_id, &[("category_id", iv(cid))])?;
                recategorized += 1;
                touched = true;
            }
        }
        if let Some(p) = &res.payee {
            if *p != l.payee {
                ctx.update("transactions", l.txn_id, &[("payee", sv(p))])?;
                renamed += 1;
                touched = true;
            }
        }
        for t in &res.tags {
            if ctx
                .opt_i64(
                    "SELECT id FROM txn_tags WHERE txn_id=?1 AND tag=?2",
                    &[iv(l.txn_id), sv(t)],
                )?
                .is_none()
            {
                ctx.insert("txn_tags", &[("txn_id", iv(l.txn_id)), ("tag", sv(t))])?;
                tagged += 1;
                touched = true;
            }
        }
        if touched && changes.len() < 20 {
            changes.push(json!({ "txn_id": l.txn_id, "payee": l.payee, "rule_id": res.rule_id }));
        }
    }
    Ok(
        json!({ "examined": examined, "recategorized": recategorized, "payees_renamed": renamed, "tags_added": tagged, "sample": changes }),
    )
}

fn suggest(ctx: &Ctx, a: &Args) -> Result<Value> {
    let min = a.opt_i64("min_count")?.unwrap_or(3).max(2) as usize;
    let lines = single_split_txns(load_matching(ctx, &Query::default())?);
    let existing: Vec<String> = ctx
        .rows("SELECT query FROM rules", &[])?
        .iter()
        .filter_map(|r| r["query"].as_str().map(|s| s.to_ascii_lowercase()))
        .collect();
    let mut by_payee: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut uncategorized: BTreeMap<String, usize> = BTreeMap::new();
    for l in &lines {
        if l.payee.is_empty() {
            continue;
        }
        match &l.category {
            Some(c) => {
                *by_payee
                    .entry(l.payee.to_lowercase())
                    .or_default()
                    .entry(c.clone())
                    .or_default() += 1
            }
            None => *uncategorized.entry(l.payee.to_lowercase()).or_default() += 1,
        }
    }
    let mut out = vec![];
    for (payee, cats) in by_payee {
        let total: usize = cats.values().sum();
        let Some((best, n)) = cats.iter().max_by_key(|(_, n)| **n) else {
            continue;
        };
        let share = *n * 100 / total;
        let query = if payee.contains(' ') {
            format!("payee:\"{payee}\"")
        } else {
            format!("payee:{payee}")
        };
        if total >= min && share >= 80 && !existing.contains(&query) {
            out.push(json!({
                "query": query, "category": best, "evidence": total, "confidence_pct": share,
                "uncategorized_matching": uncategorized.get(&payee).copied().unwrap_or(0),
                "command": format!("pext rule add --name {} --query '{}' --category {}", payee.replace(' ', "_"), query, best),
            }));
        }
    }
    out.sort_by_key(|v| {
        std::cmp::Reverse((
            v["uncategorized_matching"].as_u64().unwrap_or(0),
            v["evidence"].as_u64().unwrap_or(0),
        ))
    });
    Ok(Value::Array(out))
}

pub fn ops() -> Vec<OpSpec> {
    let action = |o: OpSpec| {
        o.p(Param::s("category", "category to assign"))
            .p(Param::s("set_payee", "rename the payee to this"))
            .p(Param::s("tag", "tag to add (repeatable)").multi())
            .p(Param::i("priority", "lower runs first (default 100)"))
    };
    vec![
        action(
            OpSpec::new("rule.add", "Add an auto-categorisation rule", add)
                .write()
                .p(Param::s("name", "rule name").req())
                .p(Param::s("query", "when the transaction matches this query").req()),
        )
        .ex("pext rule add --name swiggy --query 'payee:swiggy' --category food.delivery")
        .ex("pext rule add --name rent --query 'memo:rent amount:>10000' --category housing.rent --tag fixed"),
        OpSpec::new("rule.list", "List rules in the order they run", list),
        OpSpec::new("rule.get", "Show one rule", get).p(Param::s("rule", "rule id or name").req().pos()),
        action(
            OpSpec::new("rule.edit", "Change a rule", edit)
                .write()
                .p(Param::s("rule", "rule id or name").req().pos())
                .p(Param::s("new_name", "rename the rule"))
                .p(Param::s("query", "new condition"))
                .p(Param::b("enable", "turn the rule on"))
                .p(Param::b("disable", "turn the rule off")),
        ),
        OpSpec::new("rule.rm", "Delete a rule", rm).write().p(Param::s("rule", "rule id or name").req().pos()),
        OpSpec::new("rule.test", "Show which existing transactions a rule or query would match", test)
            .p(Param::s("rule", "an existing rule"))
            .p(Param::s("query", "or a candidate query"))
            .p(Param::i("limit", "sample size (default 10)")),
        OpSpec::new("rule.apply", "Apply rules to existing transactions (uncategorised only unless --overwrite)", apply)
            .write()
            .p(Param::s("rule", "only this rule"))
            .p(Param::s("query", "only transactions matching this query"))
            .p(Param::b("overwrite", "also re-categorise already categorised transactions"))
            .ex("pext rule apply --dry-run"),
        OpSpec::new("rule.suggest", "Suggest rules from how you categorised past transactions", suggest)
            .p(Param::i("min_count", "minimum past transactions per payee (default 3)")),
    ]
}
