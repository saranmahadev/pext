//! The query language entry points: `query`, saved views, and the glue that
//! lets other commands accept a `--query` filter.

use crate::args::Args;
use crate::ctx::{iv, sv, Ctx};
use crate::ops::fx;
use crate::ops::txns::{fmt_date, load_matching};
use crate::registry::{OpSpec, Param};
use pext_core::qlang::{self, ParseCtx};
use pext_core::query::{aggregate, Agg, Field, GroupKey, Op, Query, SortKey};
use pext_core::{Error, Result};
use serde_json::{json, Value};

pub fn parse_ctx(ctx: &Ctx) -> Result<ParseCtx> {
    let fy = ctx
        .setting("fy_start_month")?
        .and_then(|v| v.parse().ok())
        .filter(|m| (1..=12).contains(m))
        .unwrap_or(4);
    let sun = ctx.setting("week_start")?.as_deref() == Some("sun");
    Ok(ParseCtx {
        today: ctx.today,
        fy_start_month: fy,
        week_starts_sunday: sun,
    })
}

/// Parse `text` and AND it onto `base`.
pub fn parse_into(ctx: &Ctx, text: &str, base: Query) -> Result<Query> {
    let parsed = qlang::parse(text, &parse_ctx(ctx)?)?;
    Ok(qlang::merge(base, parsed))
}

pub fn parse_text(ctx: &Ctx, text: &str) -> Result<Query> {
    qlang::parse(text, &parse_ctx(ctx)?)
}

/// Add the shared `--query` parameter to a filtering command.
pub fn with_query_param(mut op: OpSpec) -> OpSpec {
    if !op.params.iter().any(|p| p.name == "query") {
        op.params.push(Param::s(
            "query",
            "query-language filter, e.g. 'category:food date:this-month amount:>500'",
        ));
    }
    op
}

fn field_name(f: Field) -> &'static str {
    match f {
        Field::Account => "account",
        Field::Category => "category",
        Field::Payee => "payee",
        Field::Memo => "memo",
        Field::Tag => "tag",
        Field::Status => "status",
        Field::Type => "type",
        Field::Currency => "currency",
        Field::Amount => "amount",
        Field::Signed => "signed",
        Field::Date => "date",
        Field::Id => "id",
    }
}

fn op_name(o: Op) -> &'static str {
    match o {
        Op::Eq => "=",
        Op::Ne => "!=",
        Op::Lt => "<",
        Op::Le => "<=",
        Op::Gt => ">",
        Op::Ge => ">=",
        Op::Contains => "contains",
    }
}

pub fn explain(q: &Query) -> Value {
    let groups: Vec<Value> = q
        .any_of
        .iter()
        .map(|g| {
            Value::Array(
                g.iter()
                    .map(|t| json!({ "field": field_name(t.field), "op": op_name(t.op), "values": t.values, "not": t.negate }))
                    .collect(),
            )
        })
        .collect();
    let date_bounds = {
        let (lo, hi) = q.date_bounds();
        json!({ "from": lo.map(fmt_date), "to": hi.map(fmt_date) })
    };
    json!({
        "match_any_of": groups,
        "group_by": q.group_by.iter().map(|g| g.name()).collect::<Vec<_>>(),
        "aggregates": q.aggs.iter().map(|a| match a { Agg::Sum => "sum", Agg::Count => "count", Agg::Avg => "avg", Agg::Min => "min", Agg::Max => "max" }).collect::<Vec<_>>(),
        "sort": q.sort.map(|(k, d)| format!("{}{}", if d { "-" } else { "" }, match k { SortKey::Date => "date", SortKey::Amount => "amount", SortKey::Sum => "sum", SortKey::Count => "count", SortKey::Key => "key" })),
        "limit": q.limit, "convert": q.convert, "include_transfers": q.include_transfers,
        "date_bounds": date_bounds,
    })
}

/// Run a parsed query: grouped queries aggregate, plain ones list split lines.
pub fn run(ctx: &Ctx, q: &Query) -> Result<Value> {
    let lines = load_matching(ctx, q)?;
    if !q.group_by.is_empty() || !q.aggs.is_empty() {
        let refs: Vec<&_> = lines.iter().collect();
        let fxf = |f: &str, t: &str, d| fx::rate_e8(ctx.conn, f, t, d);
        let rows = aggregate(q, &refs, &fxf)?;
        return Ok(Value::Array(rows.into_iter().map(Value::Object).collect()));
    }
    let mut lines = lines;
    let (key, desc) = q.sort.unwrap_or((SortKey::Date, true));
    match key {
        SortKey::Amount | SortKey::Sum => {
            lines.sort_by_key(|l| (l.amount_minor.unsigned_abs(), l.txn_id))
        }
        _ => lines.sort_by_key(|l| (l.date, l.txn_id, l.split_id)),
    }
    if desc {
        lines.reverse();
    }
    if let Some(n) = q.limit {
        lines.truncate(n);
    }
    Ok(Value::Array(
        lines
            .into_iter()
            .map(|l| {
                json!({
                    "txn_id": l.txn_id, "date": fmt_date(l.date), "account": l.account, "payee": l.payee,
                    "category": l.category, "amount_minor": l.amount_minor, "currency": l.currency,
                    "status": l.status, "kind": l.kind, "tags": l.tags, "memo": l.memo,
                })
            })
            .collect(),
    ))
}

fn query_op(ctx: &Ctx, a: &Args) -> Result<Value> {
    let q = parse_text(ctx, &a.opt_str("query").unwrap_or_default())?;
    if a.flag("explain") {
        return Ok(explain(&q));
    }
    run(ctx, &q)
}

// ---- saved views ----------------------------------------------------------

fn view_row(r: serde_json::Map<String, Value>) -> Value {
    Value::Object(r)
}

fn view_save(ctx: &Ctx, a: &Args) -> Result<Value> {
    let name = a.str("name")?.trim().to_string();
    let query = a.str("query")?;
    parse_text(ctx, &query)?; // reject invalid queries up front
    let desc = a.opt_str("description").unwrap_or_default();
    let id = match ctx.opt_i64("SELECT id FROM saved_views WHERE name=?1", &[sv(&name)])? {
        Some(id) => {
            ctx.update(
                "saved_views",
                id,
                &[("query", sv(&query)), ("description", sv(&desc))],
            )?;
            id
        }
        None => ctx.insert(
            "saved_views",
            &[
                ("name", sv(&name)),
                ("query", sv(&query)),
                ("description", sv(&desc)),
                ("created_at", sv(&ctx.now)),
            ],
        )?,
    };
    Ok(json!({ "id": id, "name": name, "query": query, "description": desc }))
}

fn view_list(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let rows = ctx.rows(
        "SELECT id, name, query, description FROM saved_views ORDER BY name",
        &[],
    )?;
    Ok(Value::Array(rows.into_iter().map(view_row).collect()))
}

fn find_view(ctx: &Ctx, name: &str) -> Result<(i64, String)> {
    let rows = ctx.rows(
        "SELECT id, query FROM saved_views WHERE name=?1",
        &[sv(name)],
    )?;
    let r = rows.first().ok_or_else(|| {
        Error::not_found("view_not_found", format!("no saved view '{name}'"))
            .with_hint("run `pext view list`")
    })?;
    Ok((
        r["id"].as_i64().unwrap_or(0),
        r["query"].as_str().unwrap_or("").to_string(),
    ))
}

fn view_run(ctx: &Ctx, a: &Args) -> Result<Value> {
    let (_, text) = find_view(ctx, &a.str("name")?)?;
    let mut q = parse_text(ctx, &text)?;
    if let Some(extra) = a.opt_str("query") {
        q = parse_into(ctx, &extra, q)?;
    }
    run(ctx, &q)
}

fn view_rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let name = a.str("name")?;
    let (id, _) = find_view(ctx, &name)?;
    ctx.delete("saved_views", id)?;
    Ok(json!({ "removed": name }))
}

pub fn ops() -> Vec<OpSpec> {
    let _ = (iv(0), GroupKey::Day);
    vec![
        OpSpec::new(
            "query",
            "Run a query: filter, group and aggregate your ledger",
            query_op,
        )
        .scoped()
        .p(Param::s(
            "query",
            "the query, e.g. 'category:food date:last-3m group:month'",
        )
        .pos())
        .p(Param::b(
            "explain",
            "show how the query was understood instead of running it",
        ))
        .ex("pext query 'type:expense date:this-month group:category sort:-sum limit:5'")
        .ex("pext query 'payee:swiggy or payee:zomato date:last-90d group:month'")
        .ex("pext query 'category:food amount:>500 date:2026-10' --explain"),
        OpSpec::new("view.save", "Save (or update) a named query", view_save)
            .write()
            .p(Param::s("name", "view name").req().pos())
            .p(Param::s("query", "query text").req().pos())
            .p(Param::s("description", "what the view is for")),
        OpSpec::new("view.list", "List saved views", view_list),
        OpSpec::new(
            "view.run",
            "Run a saved view (optionally narrowed with an extra query)",
            view_run,
        )
        .scoped()
        .p(Param::s("name", "view name").req().pos())
        .p(Param::s("query", "extra terms ANDed onto the view")),
        OpSpec::new("view.rm", "Delete a saved view", view_rm)
            .write()
            .p(Param::s("name", "view name").req().pos()),
    ]
}
