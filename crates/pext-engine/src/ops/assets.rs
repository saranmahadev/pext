//! Manually valued assets (property, vehicles, gold, retirement funds ...)
//! with a dated value history.

use crate::args::Args;
use crate::ctx::{iv, sv, Ctx};
use crate::ops::accounts::parse_stored_date;
use crate::ops::categories::collapse_ws;
use crate::ops::fx;
use crate::ops::txns::{check_date, fmt_date};
use crate::registry::{money, OpSpec, Param};
use chrono::NaiveDate;
use pext_core::money::{check_amount, decimals_for, mul_div_round};
use pext_core::{Error, Result};
use serde_json::{json, Value};

pub const KINDS: &[&str] = &[
    "property",
    "vehicle",
    "gold",
    "retirement",
    "jewellery",
    "collectible",
    "business",
    "other",
];

pub struct Asset {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub currency: String,
    pub archived: bool,
}

pub fn load_assets(ctx: &Ctx, include_archived: bool) -> Result<Vec<Asset>> {
    Ok(ctx
        .rows("SELECT * FROM assets ORDER BY name", &[])?
        .into_iter()
        .map(|r| Asset {
            id: r["id"].as_i64().unwrap_or(0),
            name: r["name"].as_str().unwrap_or("").into(),
            kind: r["kind"].as_str().unwrap_or("").into(),
            currency: r["currency"].as_str().unwrap_or("").into(),
            archived: r["archived"].as_i64() == Some(1),
        })
        .filter(|a| include_archived || !a.archived)
        .collect())
}

fn find(ctx: &Ctx, r: &str) -> Result<Asset> {
    let want = r.trim();
    load_assets(ctx, true)?
        .into_iter()
        .find(|a| a.name.eq_ignore_ascii_case(want) || want.parse::<i64>().ok() == Some(a.id))
        .ok_or_else(|| {
            Error::not_found("asset_not_found", format!("no asset '{r}'"))
                .with_hint("run `pext asset list`")
        })
}

/// Latest recorded value on or before `date`: (date, value).
pub fn value_on(ctx: &Ctx, asset_id: i64, date: NaiveDate) -> Result<Option<(NaiveDate, i64)>> {
    let rows = ctx.rows(
        "SELECT date, value_minor FROM asset_values WHERE asset_id=?1 AND date<=?2 ORDER BY date DESC LIMIT 1",
        &[iv(asset_id), sv(&fmt_date(date))],
    )?;
    match rows.first() {
        Some(r) => Ok(Some((
            parse_stored_date(r["date"].as_str().unwrap_or(""))?,
            r["value_minor"].as_i64().unwrap_or(0),
        ))),
        None => Ok(None),
    }
}

fn set_value(ctx: &Ctx, asset: &Asset, value: i64, date: NaiveDate, note: &str) -> Result<()> {
    check_amount(value)?;
    if value < 0 {
        return Err(
            Error::validation("invalid_amount", "an asset value cannot be negative")
                .with_field("value"),
        );
    }
    check_date(ctx, date)?;
    match ctx.opt_i64(
        "SELECT id FROM asset_values WHERE asset_id=?1 AND date=?2",
        &[iv(asset.id), sv(&fmt_date(date))],
    )? {
        Some(id) => ctx.update(
            "asset_values",
            id,
            &[("value_minor", iv(value)), ("note", sv(note))],
        )?,
        None => {
            ctx.insert(
                "asset_values",
                &[
                    ("asset_id", iv(asset.id)),
                    ("date", sv(&fmt_date(date))),
                    ("value_minor", iv(value)),
                    ("note", sv(note)),
                ],
            )?;
        }
    }
    Ok(())
}

fn row(ctx: &Ctx, a: &Asset) -> Result<Value> {
    let cur = value_on(ctx, a.id, ctx.today)?;
    let first = ctx.rows(
        "SELECT date, value_minor FROM asset_values WHERE asset_id=?1 ORDER BY date LIMIT 1",
        &[iv(a.id)],
    )?;
    let (gain, gain_pct) = match (&cur, first.first()) {
        (Some((_, v)), Some(f)) => {
            let f0 = f["value_minor"].as_i64().unwrap_or(0);
            (
                Some(v - f0),
                if f0 > 0 {
                    Some(mul_div_round((v - f0) as i128 * 100, 1, f0 as i128)? as i64)
                } else {
                    None
                },
            )
        }
        _ => (None, None),
    };
    let base = ctx.base_currency()?;
    let in_base = match &cur {
        Some((_, v)) => fx::convert(ctx.conn, *v, &a.currency, &base, ctx.today).ok(),
        None => None,
    };
    Ok(json!({
        "id": a.id, "name": a.name, "kind": a.kind, "currency": a.currency, "archived": a.archived,
        "value_minor": cur.as_ref().map(|c| c.1), "valued_on": cur.as_ref().map(|c| fmt_date(c.0)),
        "value_base_minor": in_base,
        "since_first_value_minor": gain, "since_first_value_pct": gain_pct,
    }))
}

fn add(ctx: &Ctx, a: &Args) -> Result<Value> {
    let name = collapse_ws(&a.str("name")?);
    if name.is_empty() || name.chars().count() > 64 {
        return Err(
            Error::validation("invalid_name", "the asset name must be 1-64 characters")
                .with_field("name"),
        );
    }
    if ctx
        .opt_i64(
            "SELECT id FROM assets WHERE name=?1 COLLATE NOCASE",
            &[sv(&name)],
        )?
        .is_some()
    {
        return Err(Error::conflict(
            "already_exists",
            format!("an asset named '{name}' already exists"),
        ));
    }
    let currency = match a.opt_str("currency") {
        Some(c) => pext_core::money::validate_currency(&c)?,
        None => ctx.base_currency()?,
    };
    let value = a.req_money("value", decimals_for(&currency))?;
    let id = ctx.insert(
        "assets",
        &[
            ("name", sv(&name)),
            (
                "kind",
                sv(&a.opt_str("kind").unwrap_or_else(|| "other".into())),
            ),
            ("currency", sv(&currency)),
            ("created_at", sv(&ctx.now)),
        ],
    )?;
    let asset = find(ctx, &id.to_string())?;
    set_value(
        ctx,
        &asset,
        value,
        a.opt_date("date")?.unwrap_or(ctx.today),
        a.opt_str("note").unwrap_or_default().trim(),
    )?;
    row(ctx, &asset)
}

fn value(ctx: &Ctx, a: &Args) -> Result<Value> {
    let asset = find(ctx, &a.str("asset")?)?;
    if asset.archived {
        return Err(Error::conflict(
            "asset_archived",
            format!("'{}' is archived", asset.name),
        ));
    }
    let v = a.req_money("value", decimals_for(&asset.currency))?;
    set_value(
        ctx,
        &asset,
        v,
        a.opt_date("date")?.unwrap_or(ctx.today),
        a.opt_str("note").unwrap_or_default().trim(),
    )?;
    row(ctx, &asset)
}

fn list(ctx: &Ctx, a: &Args) -> Result<Value> {
    let mut out = vec![];
    for x in load_assets(ctx, a.flag("all"))? {
        out.push(row(ctx, &x)?);
    }
    Ok(Value::Array(out))
}

fn history(ctx: &Ctx, a: &Args) -> Result<Value> {
    let asset = find(ctx, &a.str("asset")?)?;
    let rows = ctx.rows(
        "SELECT date, value_minor, note FROM asset_values WHERE asset_id=?1 ORDER BY date",
        &[iv(asset.id)],
    )?;
    let mut prev: Option<i64> = None;
    let mut out = vec![];
    for r in rows {
        let v = r["value_minor"].as_i64().unwrap_or(0);
        out.push(json!({
            "date": r["date"], "value_minor": v, "note": r["note"], "currency": asset.currency,
            "change_minor": prev.map(|p| v - p),
            "change_pct": match prev { Some(p) if p > 0 => Some(mul_div_round((v - p) as i128 * 100, 1, p as i128)? as i64), _ => None },
        }));
        prev = Some(v);
    }
    Ok(Value::Array(out))
}

fn edit(ctx: &Ctx, a: &Args) -> Result<Value> {
    let asset = find(ctx, &a.str("asset")?)?;
    let mut cols: Vec<(&str, rusqlite::types::Value)> = vec![];
    if let Some(n) = a.opt_str("new_name") {
        let n = collapse_ws(&n);
        if n.is_empty() || n.chars().count() > 64 {
            return Err(Error::validation(
                "invalid_name",
                "the asset name must be 1-64 characters",
            )
            .with_field("new_name"));
        }
        if ctx
            .opt_i64(
                "SELECT id FROM assets WHERE name=?1 COLLATE NOCASE AND id!=?2",
                &[sv(&n), iv(asset.id)],
            )?
            .is_some()
        {
            return Err(Error::conflict(
                "already_exists",
                format!("an asset named '{n}' already exists"),
            ));
        }
        cols.push(("name", sv(&n)));
    }
    if let Some(k) = a.opt_str("kind") {
        cols.push(("kind", sv(&k)));
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
            "give at least one field to change",
        ));
    }
    ctx.update("assets", asset.id, &cols)?;
    row(ctx, &find(ctx, &asset.id.to_string())?)
}

fn rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let asset = find(ctx, &a.str("asset")?)?;
    let n = ctx.one_i64(
        "SELECT COUNT(*) FROM asset_values WHERE asset_id=?1",
        &[iv(asset.id)],
    )?;
    if !a.flag("yes") {
        return Err(Error::usage(
            "confirmation_required",
            format!(
                "'{}' has {n} recorded value(s); deleting removes them",
                asset.name
            ),
        )
        .with_hint("repeat with --yes"));
    }
    for r in ctx.rows(
        "SELECT id FROM asset_values WHERE asset_id=?1",
        &[iv(asset.id)],
    )? {
        ctx.delete("asset_values", r["id"].as_i64().unwrap_or(0))?;
    }
    ctx.delete("assets", asset.id)?;
    Ok(json!({ "removed": asset.name, "values_removed": n }))
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new(
            "asset.add",
            "Add an asset you value yourself (property, gold, vehicle ...)",
            add,
        )
        .write()
        .p(Param::s("name", "asset name").req())
        .p(Param::s(
            "kind",
            "property, vehicle, gold, retirement, jewellery, collectible, business or other",
        )
        .choices(KINDS))
        .ps(money("value", "value_minor", "current value"))
        .p(Param::s("currency", "currency (default: base currency)"))
        .p(Param::date("date", "valuation date (default today)"))
        .p(Param::s("note", "source of the valuation")),
        OpSpec::new("asset.value", "Record a new valuation", value)
            .write()
            .p(Param::s("asset", "asset name or id").req().pos())
            .ps(money("value", "value_minor", "new value"))
            .p(Param::date("date", "valuation date (default today)"))
            .p(Param::s("note", "source of the valuation")),
        OpSpec::new("asset.list", "List assets with their latest value", list)
            .p(Param::b("all", "include archived")),
        OpSpec::new("asset.history", "Valuation history of one asset", history).p(Param::s(
            "asset",
            "asset name or id",
        )
        .req()
        .pos()),
        OpSpec::new("asset.edit", "Rename or archive an asset", edit)
            .write()
            .p(Param::s("asset", "asset name or id").req().pos())
            .p(Param::s("new_name", "new name"))
            .p(Param::s("kind", "kind").choices(KINDS))
            .p(Param::b("archive", "archive (sold)"))
            .p(Param::b("unarchive", "restore")),
        OpSpec::new(
            "asset.rm",
            "Delete an asset and its history (requires --yes)",
            rm,
        )
        .write()
        .p(Param::s("asset", "asset name or id").req().pos()),
    ]
}
