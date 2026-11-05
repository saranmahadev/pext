//! Investments: instruments, trades (FIFO lots), prices, holdings, returns,
//! allocation and rebalancing. Trades live beside the cash ledger; they do not
//! move money in accounts, so record the matching transfer or expense there.

use crate::args::Args;
use crate::ctx::{iv, sv, Ctx};
use crate::ops::accounts::parse_stored_date;
use crate::ops::categories::collapse_ws;
use crate::ops::fx;
use crate::ops::txns::fmt_date;
use crate::registry::{money, OpSpec, Param};
use chrono::NaiveDate;
use pext_core::invest::{self, fmt_e8, parse_e8, Bucket, Side, Trade};
use pext_core::money::{
    check_amount, decimals_for, mul_div_round, parse_amount, pow10, validate_currency,
};
use pext_core::xirr::xirr_bp;
use pext_core::{Error, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const KINDS: &[&str] = &[
    "stock",
    "etf",
    "mutual_fund",
    "bond",
    "crypto",
    "gold",
    "fd",
    "other",
];

pub struct Instrument {
    pub id: i64,
    pub symbol: String,
    pub name: String,
    pub kind: String,
    pub currency: String,
    pub archived: bool,
}

pub fn load_instruments(ctx: &Ctx, include_archived: bool) -> Result<Vec<Instrument>> {
    Ok(ctx
        .rows("SELECT * FROM instruments ORDER BY symbol", &[])?
        .into_iter()
        .map(|r| Instrument {
            id: r["id"].as_i64().unwrap_or(0),
            symbol: r["symbol"].as_str().unwrap_or("").into(),
            name: r["name"].as_str().unwrap_or("").into(),
            kind: r["kind"].as_str().unwrap_or("").into(),
            currency: r["currency"].as_str().unwrap_or("").into(),
            archived: r["archived"].as_i64() == Some(1),
        })
        .filter(|i| include_archived || !i.archived)
        .collect())
}

pub fn find(ctx: &Ctx, r: &str) -> Result<Instrument> {
    let want = r.trim();
    load_instruments(ctx, true)?
        .into_iter()
        .find(|i| i.symbol.eq_ignore_ascii_case(want) || want.parse::<i64>().ok() == Some(i.id))
        .ok_or_else(|| {
            Error::not_found("instrument_not_found", format!("no instrument '{r}'"))
                .with_hint("run `pext invest list` or add it with `pext invest add`")
        })
}

struct TradeRow {
    id: i64,
    date: NaiveDate,
    side: Side,
    side_s: String,
    qty_e8: i64,
    price_e8: i64,
    fee: i64,
    amount: i64,
    note: String,
}

fn trades_of(ctx: &Ctx, inst_id: i64, up_to: Option<NaiveDate>) -> Result<Vec<TradeRow>> {
    let mut out = vec![];
    for r in ctx.rows(
        "SELECT * FROM trades WHERE instrument_id=?1 ORDER BY date, id",
        &[iv(inst_id)],
    )? {
        let date = parse_stored_date(r["date"].as_str().unwrap_or(""))?;
        if up_to.is_some_and(|u| date > u) {
            continue;
        }
        let side_s = r["side"].as_str().unwrap_or("").to_string();
        out.push(TradeRow {
            id: r["id"].as_i64().unwrap_or(0),
            date,
            side: Side::parse(&side_s).unwrap_or(Side::Buy),
            side_s,
            qty_e8: r["qty_e8"].as_i64().unwrap_or(0),
            price_e8: r["price_e8"].as_i64().unwrap_or(0),
            fee: r["fee_minor"].as_i64().unwrap_or(0),
            amount: r["amount_minor"].as_i64().unwrap_or(0),
            note: r["note"].as_str().unwrap_or("").into(),
        });
    }
    Ok(out)
}

fn core_trades(rows: &[TradeRow]) -> Vec<Trade> {
    rows.iter()
        .map(|t| Trade {
            id: t.id,
            date: t.date,
            side: t.side,
            qty_e8: t.qty_e8,
            amount_minor: t.amount,
        })
        .collect()
}

/// Latest known unit price on or before `date`: (price_e8, date, source).
fn price_on(
    ctx: &Ctx,
    inst_id: i64,
    date: NaiveDate,
    trades: &[TradeRow],
) -> Result<Option<(i64, NaiveDate, &'static str)>> {
    let rows = ctx.rows(
        "SELECT date, price_e8 FROM prices WHERE instrument_id=?1 AND date<=?2 ORDER BY date DESC LIMIT 1",
        &[iv(inst_id), sv(&fmt_date(date))],
    )?;
    if let Some(r) = rows.first() {
        return Ok(Some((
            r["price_e8"].as_i64().unwrap_or(0),
            parse_stored_date(r["date"].as_str().unwrap_or(""))?,
            "price",
        )));
    }
    Ok(trades
        .iter()
        .rev()
        .find(|t| t.date <= date && t.side != Side::Dividend && t.price_e8 > 0)
        .map(|t| (t.price_e8, t.date, "last_trade")))
}

/// Instruments whose trade history no longer replays (for `doctor`).
pub fn broken_positions(ctx: &Ctx) -> Result<Vec<Value>> {
    let mut bad = vec![];
    for inst in load_instruments(ctx, true)? {
        if let Err(e) = invest::position(&core_trades(&trades_of(ctx, inst.id, None)?)) {
            bad.push(json!({ "instrument": inst.symbol, "error": e.message }));
        }
    }
    Ok(bad)
}

// ---- instruments ----------------------------------------------------------------

fn add(ctx: &Ctx, a: &Args) -> Result<Value> {
    let symbol = collapse_ws(&a.str("symbol")?).to_uppercase();
    if symbol.is_empty() || symbol.chars().count() > 32 || symbol.contains(char::is_whitespace) {
        return Err(Error::validation(
            "invalid_symbol",
            "the symbol must be 1-32 characters without spaces",
        )
        .with_field("symbol"));
    }
    if ctx
        .opt_i64(
            "SELECT id FROM instruments WHERE symbol=?1 COLLATE NOCASE",
            &[sv(&symbol)],
        )?
        .is_some()
    {
        return Err(Error::conflict(
            "already_exists",
            format!("'{symbol}' already exists"),
        ));
    }
    let currency = match a.opt_str("currency") {
        Some(c) => validate_currency(&c)?,
        None => ctx.base_currency()?,
    };
    let id = ctx.insert(
        "instruments",
        &[
            ("symbol", sv(&symbol)),
            ("name", sv(a.opt_str("name").unwrap_or_default().trim())),
            ("kind", sv(&a.str("kind")?)),
            ("currency", sv(&currency)),
            ("created_at", sv(&ctx.now)),
        ],
    )?;
    Ok(
        json!({ "id": id, "symbol": symbol, "name": a.opt_str("name").unwrap_or_default().trim(), "kind": a.str("kind")?, "currency": currency }),
    )
}

fn list(ctx: &Ctx, a: &Args) -> Result<Value> {
    Ok(Value::Array(
        load_instruments(ctx, a.flag("all"))?
            .iter()
            .map(|i| json!({ "id": i.id, "symbol": i.symbol, "name": i.name, "kind": i.kind, "currency": i.currency, "archived": i.archived }))
            .collect(),
    ))
}

fn edit(ctx: &Ctx, a: &Args) -> Result<Value> {
    let i = find(ctx, &a.str("instrument")?)?;
    let mut cols: Vec<(&str, rusqlite::types::Value)> = vec![];
    if let Some(n) = a.opt_str("name") {
        cols.push(("name", sv(n.trim())));
    }
    if let Some(k) = a.opt_str("kind") {
        cols.push(("kind", sv(&k)));
    }
    if a.flag("archive") {
        let q = invest::position(&core_trades(&trades_of(ctx, i.id, None)?))?.qty_e8;
        if q > 0 {
            return Err(Error::conflict(
                "still_held",
                format!("{} is still held ({} units)", i.symbol, fmt_e8(q)),
            )
            .with_hint("record the sale first"));
        }
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
    ctx.update("instruments", i.id, &cols)?;
    let i = find(ctx, &i.id.to_string())?;
    Ok(
        json!({ "id": i.id, "symbol": i.symbol, "name": i.name, "kind": i.kind, "currency": i.currency, "archived": i.archived }),
    )
}

fn rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let i = find(ctx, &a.str("instrument")?)?;
    let nt = ctx.one_i64(
        "SELECT COUNT(*) FROM trades WHERE instrument_id=?1",
        &[iv(i.id)],
    )?;
    let np = ctx.one_i64(
        "SELECT COUNT(*) FROM prices WHERE instrument_id=?1",
        &[iv(i.id)],
    )?;
    if (nt > 0 || np > 0) && !a.flag("yes") {
        return Err(Error::usage(
            "confirmation_required",
            format!(
                "{} has {nt} trade(s) and {np} price(s); deleting removes them",
                i.symbol
            ),
        )
        .with_hint("repeat with --yes"));
    }
    for r in ctx.rows("SELECT id FROM trades WHERE instrument_id=?1", &[iv(i.id)])? {
        ctx.delete("trades", r["id"].as_i64().unwrap_or(0))?;
    }
    for r in ctx.rows("SELECT id FROM prices WHERE instrument_id=?1", &[iv(i.id)])? {
        ctx.delete("prices", r["id"].as_i64().unwrap_or(0))?;
    }
    ctx.delete("instruments", i.id)?;
    Ok(json!({ "removed": i.symbol, "trades_removed": nt, "prices_removed": np }))
}

// ---- trades -----------------------------------------------------------------------

fn trade_json(t: &TradeRow, inst: &Instrument) -> Value {
    json!({
        "id": t.id, "instrument": inst.symbol, "date": fmt_date(t.date), "side": t.side_s, "currency": inst.currency,
        "quantity": fmt_e8(t.qty_e8), "price": fmt_e8(t.price_e8), "fee_minor": t.fee, "amount_minor": t.amount, "note": t.note,
    })
}

fn trade_add(ctx: &Ctx, a: &Args) -> Result<Value> {
    let inst = find(ctx, &a.str("instrument")?)?;
    if inst.archived {
        return Err(Error::conflict(
            "instrument_archived",
            format!("{} is archived", inst.symbol),
        ));
    }
    let side_s = a.str("side")?;
    let side = Side::parse(&side_s)
        .ok_or_else(|| Error::validation("invalid_side", "side must be buy, sell or dividend"))?;
    let date = a.opt_date("date")?.unwrap_or(ctx.today);
    if date > ctx.today {
        return Err(
            Error::validation("date_in_future", "trades cannot be dated in the future")
                .with_field("date"),
        );
    }
    if date < pext_core::dates::min_date() {
        return Err(
            Error::validation("date_out_of_range", "date is too far in the past")
                .with_field("date"),
        );
    }
    let dec = decimals_for(&inst.currency);
    let fee = a.money("fee", dec)?.unwrap_or(0);
    check_amount(fee)?;
    if fee < 0 {
        return Err(
            Error::validation("invalid_amount", "the fee cannot be negative").with_field("fee"),
        );
    }
    let (qty, price, amount) = match side {
        Side::Dividend => {
            let amt = a.req_money("amount", dec)?;
            check_amount(amt)?;
            if amt <= 0 {
                return Err(
                    Error::validation("invalid_amount", "a dividend must be positive")
                        .with_field("amount"),
                );
            }
            (0, 0, amt)
        }
        _ => {
            let qty = parse_e8(&a.str("quantity")?, "quantity")?;
            let price = parse_e8(&a.str("price")?, "price")?;
            if qty == 0 {
                return Err(
                    Error::validation("invalid_quantity", "the quantity must be positive")
                        .with_field("quantity"),
                );
            }
            let gross = invest::value_minor(qty, price, dec)?;
            check_amount(gross)?;
            let amt = if side == Side::Buy {
                gross
                    .checked_add(fee)
                    .ok_or_else(|| Error::validation("amount_overflow", "amount is too large"))?
            } else {
                if fee > gross {
                    return Err(Error::validation(
                        "invalid_amount",
                        "the fee is larger than the proceeds",
                    )
                    .with_field("fee"));
                }
                gross - fee
            };
            check_amount(amt)?;
            (qty, price, amt)
        }
    };
    let id = ctx.insert(
        "trades",
        &[
            ("instrument_id", iv(inst.id)),
            ("date", sv(&fmt_date(date))),
            ("side", sv(&side_s)),
            ("qty_e8", iv(qty)),
            ("price_e8", iv(price)),
            ("fee_minor", iv(fee)),
            ("amount_minor", iv(amount)),
            ("note", sv(a.opt_str("note").unwrap_or_default().trim())),
            ("created_at", sv(&ctx.now)),
        ],
    )?;
    // The whole history must still replay: a back-dated sale may not exceed what was held.
    let rows = trades_of(ctx, inst.id, None)?;
    invest::position(&core_trades(&rows))?;
    if price > 0
        && ctx
            .opt_i64(
                "SELECT id FROM prices WHERE instrument_id=?1 AND date=?2",
                &[iv(inst.id), sv(&fmt_date(date))],
            )?
            .is_none()
    {
        ctx.insert(
            "prices",
            &[
                ("instrument_id", iv(inst.id)),
                ("date", sv(&fmt_date(date))),
                ("price_e8", iv(price)),
            ],
        )?;
    }
    let t = rows.iter().find(|t| t.id == id).expect("just inserted");
    Ok(trade_json(t, &inst))
}

fn trade_list(ctx: &Ctx, a: &Args) -> Result<Value> {
    let only = match a.opt_str("instrument") {
        Some(r) => Some(find(ctx, &r)?),
        None => None,
    };
    let insts = load_instruments(ctx, true)?;
    let (from, to) = (a.opt_date("from")?, a.opt_date("to")?);
    let side = a.opt_str("side");
    let mut out: Vec<(NaiveDate, i64, Value)> = vec![];
    for inst in insts
        .iter()
        .filter(|i| only.as_ref().is_none_or(|o| o.id == i.id))
    {
        for t in trades_of(ctx, inst.id, None)? {
            if from.is_some_and(|f| t.date < f)
                || to.is_some_and(|x| t.date > x)
                || side.as_ref().is_some_and(|s| &t.side_s != s)
            {
                continue;
            }
            out.push((t.date, t.id, trade_json(&t, inst)));
        }
    }
    out.sort_by_key(|(d, id, _)| std::cmp::Reverse((*d, *id)));
    let limit = a.opt_i64("limit")?.unwrap_or(100).clamp(1, 10_000) as usize;
    Ok(Value::Array(
        out.into_iter().take(limit).map(|x| x.2).collect(),
    ))
}

fn trade_rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let id = a.i64("id")?;
    let inst_id = ctx
        .opt_i64("SELECT instrument_id FROM trades WHERE id=?1", &[iv(id)])?
        .ok_or_else(|| Error::not_found("trade_not_found", format!("no trade #{id}")))?;
    ctx.delete("trades", id)?;
    let rows = trades_of(ctx, inst_id, None)?;
    invest::position(&core_trades(&rows)).map_err(|e| {
        Error::conflict(
            "would_oversell",
            format!(
                "removing trade #{id} would leave a later sale without units: {}",
                e.message
            ),
        )
        .with_hint("remove the later sale first")
    })?;
    Ok(json!({ "removed": id }))
}

// ---- prices -------------------------------------------------------------------------

fn price_set(ctx: &Ctx, a: &Args) -> Result<Value> {
    let inst = find(ctx, &a.str("instrument")?)?;
    let price = parse_e8(&a.str("price")?, "price")?;
    if price == 0 {
        return Err(
            Error::validation("invalid_price", "the price must be positive").with_field("price"),
        );
    }
    let date = a.opt_date("date")?.unwrap_or(ctx.today);
    upsert_price(ctx, &inst, date, price)
}

/// Insert or replace one daily price (validated: positive, not in the future).
pub fn upsert_price(
    ctx: &Ctx,
    inst: &Instrument,
    date: chrono::NaiveDate,
    price: i64,
) -> Result<Value> {
    if price <= 0 {
        return Err(
            Error::validation("invalid_price", "the price must be positive").with_field("price"),
        );
    }
    if date > ctx.today {
        return Err(
            Error::validation("date_in_future", "prices cannot be dated in the future")
                .with_field("date"),
        );
    }
    match ctx.opt_i64(
        "SELECT id FROM prices WHERE instrument_id=?1 AND date=?2",
        &[iv(inst.id), sv(&fmt_date(date))],
    )? {
        Some(id) => ctx.update("prices", id, &[("price_e8", iv(price))])?,
        None => {
            ctx.insert(
                "prices",
                &[
                    ("instrument_id", iv(inst.id)),
                    ("date", sv(&fmt_date(date))),
                    ("price_e8", iv(price)),
                ],
            )?;
        }
    }
    Ok(
        json!({ "instrument": inst.symbol, "date": fmt_date(date), "price": fmt_e8(price), "currency": inst.currency }),
    )
}

fn price_list(ctx: &Ctx, a: &Args) -> Result<Value> {
    let inst = find(ctx, &a.str("instrument")?)?;
    let limit = a.opt_i64("limit")?.unwrap_or(30).clamp(1, 10_000);
    let rows = ctx.rows(
        "SELECT date, price_e8 FROM prices WHERE instrument_id=?1 ORDER BY date DESC LIMIT ?2",
        &[iv(inst.id), iv(limit)],
    )?;
    Ok(Value::Array(rows.into_iter().map(|r| json!({ "instrument": inst.symbol, "date": r["date"], "price": fmt_e8(r["price_e8"].as_i64().unwrap_or(0)), "currency": inst.currency })).collect()))
}

// ---- holdings / performance -----------------------------------------------------------

pub struct Holding {
    pub inst: Instrument,
    pub pos: invest::Position,
    pub price: Option<(i64, NaiveDate, &'static str)>,
    pub value: Option<i64>,
    pub value_base: Option<i64>,
    pub xirr_bp: Option<i64>,
}

/// Every instrument that has trades, with valuation and XIRR as of today.
pub fn holdings(ctx: &Ctx) -> Result<(Vec<Holding>, String, usize)> {
    let base = ctx.base_currency()?;
    let mut out = vec![];
    let mut skipped = 0;
    for inst in load_instruments(ctx, true)? {
        let rows = trades_of(ctx, inst.id, Some(ctx.today))?;
        if rows.is_empty() {
            continue;
        }
        let pos = invest::position(&core_trades(&rows))?;
        if pos.qty_e8 == 0 && inst.archived {
            // fully sold and archived: still shown in performance, but not in holdings with zero value
        }
        let dec = decimals_for(&inst.currency);
        let price = price_on(ctx, inst.id, ctx.today, &rows)?;
        let value = match (&price, pos.qty_e8) {
            (_, 0) => Some(0),
            (Some(p), q) => Some(invest::value_minor(q, p.0, dec)?),
            (None, _) => None,
        };
        let conv = |v: i64, d: NaiveDate| fx::convert(ctx.conn, v, &inst.currency, &base, d);
        let value_base = match value {
            Some(v) => conv(v, ctx.today).ok(),
            None => None,
        };
        if value.is_some() && value_base.is_none() {
            skipped += 1;
        }
        // XIRR in the base currency: each cash flow at its own date's rate
        let mut flows: Vec<(NaiveDate, i64)> = vec![];
        let mut ok = true;
        for t in &rows {
            let signed = match t.side {
                Side::Buy => -t.amount,
                _ => t.amount,
            };
            match conv(signed, t.date) {
                Ok(v) => flows.push((t.date, v)),
                Err(_) => ok = false,
            }
        }
        if let Some(v) = value_base {
            if pos.qty_e8 > 0 {
                flows.push((ctx.today, v));
            }
        }
        let xirr = if ok && value.is_some() {
            xirr_bp(&flows)
        } else {
            None
        };
        out.push(Holding {
            inst,
            pos,
            price,
            value,
            value_base,
            xirr_bp: xirr,
        });
    }
    Ok((out, base, skipped))
}

fn avg_price_e8(h: &Holding) -> Result<Option<i64>> {
    if h.pos.qty_e8 == 0 {
        return Ok(None);
    }
    let v = mul_div_round(
        h.pos.cost_minor as i128 * 10i128.pow(16),
        1,
        pow10(decimals_for(&h.inst.currency)) * h.pos.qty_e8 as i128,
    )?;
    Ok(i64::try_from(v).ok())
}

fn holdings_op(ctx: &Ctx, a: &Args) -> Result<Value> {
    let (hs, base, skipped) = holdings(ctx)?;
    if skipped > 0 {
        ctx.warn(
            "fx_missing",
            format!(
                "{skipped} holding(s) in other currencies are not in the totals: no exchange rate"
            ),
            json!({}),
        );
    }
    let total: i128 = hs
        .iter()
        .filter_map(|h| h.value_base)
        .map(|v| v as i128)
        .sum();
    let mut rows = vec![];
    let (mut cost_b, mut real_b, mut div_b) = (0i128, 0i128, 0i128);
    for h in hs.iter().filter(|h| a.flag("all") || h.pos.qty_e8 > 0) {
        let unreal = h.value.map(|v| v - h.pos.cost_minor);
        let age = h.price.map(|p| (ctx.today - p.1).num_days());
        let to_base = |v: i64| fx::convert(ctx.conn, v, &h.inst.currency, &base, ctx.today).ok();
        if let Some(c) = to_base(h.pos.cost_minor) {
            cost_b += c as i128;
        }
        if let Some(c) = to_base(h.pos.realised_minor) {
            real_b += c as i128;
        }
        if let Some(c) = to_base(h.pos.dividends_minor) {
            div_b += c as i128;
        }
        if h.price.is_none() && h.pos.qty_e8 > 0 {
            ctx.warn(
                "no_price",
                format!(
                    "{} has no price; set one with `pext price set`",
                    h.inst.symbol
                ),
                json!({ "instrument": h.inst.symbol }),
            );
        }
        rows.push(json!({
            "instrument": h.inst.symbol, "name": h.inst.name, "kind": h.inst.kind, "currency": h.inst.currency,
            "quantity": fmt_e8(h.pos.qty_e8), "avg_price": avg_price_e8(h)?.map(fmt_e8),
            "cost_minor": h.pos.cost_minor,
            "price": h.price.map(|p| fmt_e8(p.0)), "price_date": h.price.map(|p| fmt_date(p.1)), "price_source": h.price.map(|p| p.2), "price_age_days": age,
            "value_minor": h.value,
            "unrealised_minor": unreal,
            "unrealised_pct": match (unreal, h.pos.cost_minor) { (Some(u), c) if c > 0 => Some(mul_div_round(u as i128 * 100, 1, c as i128)? as i64), _ => None },
            "realised_minor": h.pos.realised_minor, "dividends_minor": h.pos.dividends_minor,
            "value_base_minor": h.value_base,
            "weight_pct": match (h.value_base, total) { (Some(v), t) if t > 0 => Some(mul_div_round(v as i128 * 100, 1, t)? as i64), _ => None },
            "xirr_bp": h.xirr_bp,
        }));
    }
    let value_total = total as i64;
    Ok(json!({
        "currency": base, "as_of": fmt_date(ctx.today),
        "totals": { "value_minor": value_total, "cost_minor": cost_b as i64, "unrealised_minor": value_total - cost_b as i64, "realised_minor": real_b as i64, "dividends_minor": div_b as i64 },
        "holdings": rows,
    }))
}

fn performance(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let (hs, base, skipped) = holdings(ctx)?;
    if skipped > 0 {
        ctx.warn(
            "fx_missing",
            format!(
                "{skipped} holding(s) in other currencies are not in the totals: no exchange rate"
            ),
            json!({}),
        );
    }
    let mut flows: Vec<(NaiveDate, i64)> = vec![];
    let (mut bought, mut sold, mut divs, mut value) = (0i128, 0i128, 0i128, 0i128);
    let mut per = vec![];
    let mut incomplete = false;
    for h in &hs {
        let rows = trades_of(ctx, h.inst.id, Some(ctx.today))?;
        let conv = |v: i64, d: NaiveDate| fx::convert(ctx.conn, v, &h.inst.currency, &base, d);
        let mut inst_ok = true;
        let mut local: Vec<(NaiveDate, i64)> = vec![];
        let (mut b, mut s, mut dv) = (0i128, 0i128, 0i128);
        for t in &rows {
            match conv(t.amount, t.date) {
                Ok(v) => match t.side {
                    Side::Buy => {
                        b += v as i128;
                        local.push((t.date, -v));
                    }
                    Side::Sell => {
                        s += v as i128;
                        local.push((t.date, v));
                    }
                    Side::Dividend => {
                        dv += v as i128;
                        local.push((t.date, v));
                    }
                },
                Err(_) => inst_ok = false,
            }
        }
        let v = h.value_base;
        if h.value.is_some() && v.is_none() {
            inst_ok = false;
        }
        if !inst_ok {
            incomplete = true;
            continue;
        }
        let v = v.unwrap_or(0);
        if h.pos.qty_e8 > 0 {
            local.push((ctx.today, v));
        }
        bought += b;
        sold += s;
        divs += dv;
        value += v as i128;
        flows.extend(local);
        let gain = v as i128 + s + dv - b;
        per.push(json!({
            "instrument": h.inst.symbol, "kind": h.inst.kind, "invested_minor": b as i64, "returned_minor": (s + dv) as i64, "value_minor": v,
            "gain_minor": gain as i64, "gain_pct": if b > 0 { Some(mul_div_round(gain * 100, 1, b)? as i64) } else { None }, "xirr_bp": h.xirr_bp,
        }));
    }
    if incomplete {
        ctx.warn(
            "incomplete",
            "some instruments were left out: missing exchange rate or price",
            json!({}),
        );
    }
    let gain = value + sold + divs - bought;
    Ok(json!({
        "currency": base, "as_of": fmt_date(ctx.today),
        "invested_minor": bought as i64, "sold_minor": sold as i64, "dividends_minor": divs as i64, "value_minor": value as i64,
        "gain_minor": gain as i64, "gain_pct": if bought > 0 { Some(mul_div_round(gain * 100, 1, bought)? as i64) } else { None },
        "xirr_bp": xirr_bp(&flows),
        "instruments": per,
    }))
}

// ---- allocation ---------------------------------------------------------------------------

fn class_values(ctx: &Ctx) -> Result<(BTreeMap<String, i128>, String)> {
    let (hs, base, _) = holdings(ctx)?;
    let mut by: BTreeMap<String, i128> = BTreeMap::new();
    for h in hs.iter().filter(|h| h.pos.qty_e8 > 0) {
        if let Some(v) = h.value_base {
            *by.entry(h.inst.kind.clone()).or_default() += v as i128;
        }
    }
    Ok((by, base))
}

fn targets(ctx: &Ctx) -> Result<BTreeMap<String, i64>> {
    Ok(ctx
        .rows(
            "SELECT class, target_bp FROM alloc_targets ORDER BY class",
            &[],
        )?
        .into_iter()
        .map(|r| {
            (
                r["class"].as_str().unwrap_or("").to_lowercase(),
                r["target_bp"].as_i64().unwrap_or(0),
            )
        })
        .collect())
}

fn allocation(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let (by, base) = class_values(ctx)?;
    let tg = targets(ctx)?;
    let total: i128 = by.values().sum();
    let mut classes: Vec<String> = by.keys().cloned().chain(tg.keys().cloned()).collect();
    classes.sort();
    classes.dedup();
    let mut rows = vec![];
    for c in classes {
        let v = by.get(&c).copied().unwrap_or(0);
        let share_bp = if total > 0 {
            mul_div_round(v * 10_000, 1, total)? as i64
        } else {
            0
        };
        let t = tg.get(&c).copied();
        rows.push(json!({
            "class": c, "value_minor": v as i64, "share_bp": share_bp, "share_pct": pext_core::money::format_amount(share_bp, 2),
            "target_bp": t, "target_pct": t.map(|x| pext_core::money::format_amount(x, 2)), "drift_bp": t.map(|x| share_bp - x),
        }));
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r["value_minor"].as_i64().unwrap_or(0)));
    Ok(
        json!({ "currency": base, "total_minor": total as i64, "classes": rows, "targets_total_bp": tg.values().sum::<i64>() }),
    )
}

fn target_set(ctx: &Ctx, a: &Args) -> Result<Value> {
    let class = a.str("class")?.trim().to_lowercase();
    if !KINDS.contains(&class.as_str()) {
        return Err(
            Error::validation("invalid_class", format!("'{class}' is not an asset class"))
                .with_details(json!({ "classes": KINDS }))
                .with_field("class"),
        );
    }
    let bp = parse_amount(a.str("percent")?.trim().trim_end_matches('%'), 2).map_err(|e| {
        Error::validation(
            "invalid_percent",
            format!("not a valid percentage: {}", e.message),
        )
        .with_field("percent")
    })?;
    if !(0..=10_000).contains(&bp) {
        return Err(
            Error::validation("invalid_percent", "a target must be between 0% and 100%")
                .with_field("percent"),
        );
    }
    let existing = ctx.opt_i64(
        "SELECT id FROM alloc_targets WHERE class=?1 COLLATE NOCASE",
        &[sv(&class)],
    )?;
    let others: i64 = targets(ctx)?
        .iter()
        .filter(|(k, _)| **k != class)
        .map(|(_, v)| *v)
        .sum();
    if others + bp > 10_000 {
        return Err(Error::validation(
            "targets_exceed_100",
            format!(
                "targets would total {}%",
                pext_core::money::format_amount(others + bp, 2)
            ),
        )
        .with_details(json!({ "other_targets_bp": others }))
        .with_hint("lower another target first"));
    }
    match existing {
        Some(id) => ctx.update("alloc_targets", id, &[("target_bp", iv(bp))])?,
        None => {
            ctx.insert(
                "alloc_targets",
                &[("class", sv(&class)), ("target_bp", iv(bp))],
            )?;
        }
    }
    Ok(json!({ "class": class, "target_bp": bp, "targets_total_bp": others + bp }))
}

fn target_list(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let tg = targets(ctx)?;
    let total: i64 = tg.values().sum();
    Ok(json!({
        "targets": tg.iter().map(|(k, v)| json!({ "class": k, "target_bp": v, "target_pct": pext_core::money::format_amount(*v, 2) })).collect::<Vec<_>>(),
        "total_bp": total, "unallocated_bp": 10_000 - total,
    }))
}

fn target_rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let class = a.str("class")?.trim().to_lowercase();
    let id = ctx
        .opt_i64(
            "SELECT id FROM alloc_targets WHERE class=?1 COLLATE NOCASE",
            &[sv(&class)],
        )?
        .ok_or_else(|| Error::not_found("target_not_found", format!("no target for '{class}'")))?;
    ctx.delete("alloc_targets", id)?;
    Ok(json!({ "removed": class }))
}

fn rebalance(ctx: &Ctx, a: &Args) -> Result<Value> {
    let (by, base) = class_values(ctx)?;
    let tg = targets(ctx)?;
    let contribute = a.money("contribute", decimals_for(&base))?.unwrap_or(0);
    let buckets: Vec<Bucket> = tg
        .iter()
        .filter(|(_, bp)| **bp > 0)
        .map(|(c, bp)| Bucket {
            class: c.clone(),
            value: by.get(c).copied().unwrap_or(0) as i64,
            target_bp: *bp,
        })
        .collect();
    let untargeted: Vec<&String> = by
        .keys()
        .filter(|k| !tg.get(*k).is_some_and(|v| *v > 0))
        .collect();
    if !untargeted.is_empty() {
        ctx.warn(
            "untargeted_holdings",
            format!("holdings in {untargeted:?} have no target and are left out of the plan"),
            json!({ "classes": untargeted }),
        );
    }
    let tsum: i64 = buckets.iter().map(|b| b.target_bp).sum();
    if tsum > 0 && tsum != 10_000 {
        ctx.warn(
            "targets_not_100",
            format!(
                "targets total {}%, so they are scaled to 100%",
                pext_core::money::format_amount(tsum, 2)
            ),
            json!({}),
        );
    }
    let moves = invest::rebalance(&buckets, contribute)?;
    Ok(json!({
        "currency": base, "contribution_minor": contribute, "mode": if contribute > 0 { "contribute_only_buys" } else { "full_rebalance" },
        "moves": moves.iter().map(|m| json!({
            "class": m.class, "current_minor": m.current, "target_minor": m.target,
            "action": if m.delta > 0 { "buy" } else if m.delta < 0 { "sell" } else { "hold" }, "amount_minor": m.delta.abs(), "delta_minor": m.delta,
        })).collect::<Vec<_>>(),
    }))
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new("invest.add", "Add an instrument (stock, fund, crypto ...)", add)
            .write()
            .p(Param::s("symbol", "ticker or short code").req())
            .p(Param::s("kind", "asset class").req().choices(KINDS))
            .p(Param::s("name", "full name"))
            .p(Param::s("currency", "price currency (default base)"))
            .ex("pext invest add --symbol NIFTYBEES --kind etf --name 'Nippon Nifty 50 ETF'"),
        OpSpec::new("invest.list", "List instruments", list).p(Param::b("all", "include archived")),
        OpSpec::new("invest.edit", "Rename, reclassify or archive an instrument", edit)
            .write()
            .p(Param::s("instrument", "symbol or id").req().pos())
            .p(Param::s("name", "new name"))
            .p(Param::s("kind", "asset class").choices(KINDS))
            .p(Param::b("archive", "archive (must be fully sold)"))
            .p(Param::b("unarchive", "restore")),
        OpSpec::new("invest.rm", "Delete an instrument with its trades and prices (requires --yes)", rm).write().p(Param::s("instrument", "symbol or id").req().pos()),
        OpSpec::new("trade.add", "Record a buy, sell or dividend", trade_add)
            .write()
            .p(Param::s("instrument", "symbol or id").req())
            .p(Param::s("side", "buy, sell or dividend").req().choices(&["buy", "sell", "dividend"]))
            .p(Param::s("quantity", "units (up to 8 decimals); not for dividends"))
            .p(Param::s("price", "price per unit (up to 8 decimals); not for dividends"))
            .ps(money("fee", "fee_minor", "brokerage and charges"))
            .ps(money("amount", "amount_minor", "cash received (dividends only)"))
            .p(Param::date("date", "trade date (default today)"))
            .p(Param::s("note", "note"))
            .ex("pext trade add --instrument NIFTYBEES --side buy --quantity 10 --price 245.50 --fee 12"),
        OpSpec::new("trade.list", "List trades", trade_list)
            .p(Param::s("instrument", "symbol or id"))
            .p(Param::s("side", "buy, sell or dividend").choices(&["buy", "sell", "dividend"]))
            .p(Param::date("from", "on or after"))
            .p(Param::date("to", "on or before"))
            .p(Param::i("limit", "maximum rows (default 100)")),
        OpSpec::new("trade.rm", "Delete a trade", trade_rm).write().p(Param::i("id", "trade id").req().pos()),
        OpSpec::new("price.set", "Record a unit price (latest price values holdings)", price_set)
            .write()
            .p(Param::s("instrument", "symbol or id").req().pos())
            .p(Param::s("price", "price per unit").req().pos())
            .p(Param::date("date", "price date (default today)")),
        OpSpec::new("price.list", "Price history of an instrument", price_list)
            .p(Param::s("instrument", "symbol or id").req().pos())
            .p(Param::i("limit", "maximum rows (default 30)")),
        OpSpec::new("invest.holdings", "Current holdings with value, gains and returns", holdings_op).p(Param::b("all", "include fully sold instruments")),
        OpSpec::new("invest.performance", "Portfolio and per-instrument returns (XIRR)", performance),
        OpSpec::new("invest.allocation", "Allocation by asset class against targets", allocation),
        OpSpec::new("invest.rebalance", "What to buy or sell to reach the targets (optionally with fresh money)", rebalance)
            .ps(money("contribute", "contribute_minor", "new money to invest (only buys are suggested)")),
        OpSpec::new("target.set", "Set the target share of an asset class", target_set)
            .write()
            .p(Param::s("class", "asset class").req().pos().choices(KINDS))
            .p(Param::s("percent", "target share, e.g. 60 or 12.5").req().pos()),
        OpSpec::new("target.list", "List allocation targets", target_list),
        OpSpec::new("target.rm", "Remove an allocation target", target_rm).write().p(Param::s("class", "asset class").req().pos()),
    ]
}
