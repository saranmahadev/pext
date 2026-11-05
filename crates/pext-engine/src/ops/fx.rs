//! Exchange rates. Rates are stored scaled by 1e8 and looked up as of a date
//! (the latest rate on or before it), directly or by inversion.

use crate::args::Args;
use crate::ctx::{iv, osv, sv, Ctx, DbResult};
use crate::ops::txns::fmt_date;
use crate::registry::{OpSpec, Param};
use chrono::NaiveDate;
use pext_core::money::{
    convert_minor, decimals_for, mul_div_round, parse_decimal, validate_currency,
};
use pext_core::{Error, Result};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

pub const ONE: i64 = 100_000_000;

fn direct(
    conn: &Connection,
    base: &str,
    quote: &str,
    date: NaiveDate,
) -> Result<Option<(i64, String)>> {
    conn.query_row(
        "SELECT rate_e8, date FROM fx_rates WHERE base=?1 AND quote=?2 AND date<=?3 ORDER BY date DESC LIMIT 1",
        rusqlite::params![base, quote, fmt_date(date)],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
    .db()
}

/// Rate (scaled 1e8) to convert one unit of `from` into `to` as of `date`.
pub fn rate_e8(conn: &Connection, from: &str, to: &str, date: NaiveDate) -> Result<i64> {
    if from == to {
        return Ok(ONE);
    }
    if let Some((r, _)) = direct(conn, from, to, date)? {
        return Ok(r);
    }
    if let Some((r, _)) = direct(conn, to, from, date)? {
        // 1e16 / r, rounded half-even
        let inv = mul_div_round(ONE as i128, ONE as i128, r as i128)?;
        return i64::try_from(inv).ok().filter(|v| *v > 0).ok_or_else(|| {
            Error::validation(
                "fx_rate_invalid",
                format!("rate {from}/{to} is too extreme to invert"),
            )
        });
    }
    Err(Error::validation(
        "fx_rate_missing",
        format!("no exchange rate for {from}->{to} on or before {date}"),
    )
    .with_hint(format!(
        "add one: pext fx set --from {from} --to {to} --rate <rate> --date {date}"
    )))
}

/// Convert a minor amount between currencies as of `date`.
pub fn convert(
    conn: &Connection,
    amount_minor: i64,
    from: &str,
    to: &str,
    date: NaiveDate,
) -> Result<i64> {
    if from == to {
        return Ok(amount_minor);
    }
    let r = rate_e8(conn, from, to, date)?;
    convert_minor(amount_minor, decimals_for(from), decimals_for(to), r)
}

fn parse_rate(s: &str) -> Result<i64> {
    let d = parse_decimal(s)?;
    if d.scale > 8 {
        return Err(Error::validation(
            "rate_precision",
            "exchange rates support at most 8 decimal places",
        )
        .with_field("rate"));
    }
    let v = d.to_minor(8).map_err(|e| e.with_field("rate"))?;
    if v <= 0 {
        return Err(
            Error::validation("invalid_rate", "the rate must be positive").with_field("rate"),
        );
    }
    Ok(v)
}

fn fmt_rate(r: i64) -> String {
    pext_core::money::format_amount(r, 8)
}

fn set(ctx: &Ctx, a: &Args) -> Result<Value> {
    let from = validate_currency(&a.str("from")?)?;
    let to = validate_currency(&a.str("to")?)?;
    if from == to {
        return Err(Error::validation(
            "invalid_pair",
            "from and to must be different currencies",
        ));
    }
    let r = parse_rate(&a.str("rate")?)?;
    let date = a.opt_date("date")?.unwrap_or(ctx.today);
    let existing = ctx.opt_i64(
        "SELECT id FROM fx_rates WHERE base=?1 AND quote=?2 AND date=?3",
        &[sv(&from), sv(&to), sv(&fmt_date(date))],
    )?;
    let id = match existing {
        Some(id) => {
            ctx.update(
                "fx_rates",
                id,
                &[("rate_e8", iv(r)), ("source", sv("manual"))],
            )?;
            id
        }
        None => ctx.insert(
            "fx_rates",
            &[
                ("base", sv(&from)),
                ("quote", sv(&to)),
                ("date", sv(&fmt_date(date))),
                ("rate_e8", iv(r)),
                ("source", sv("manual")),
            ],
        )?,
    };
    Ok(
        json!({ "id": id, "from": from, "to": to, "date": fmt_date(date), "rate": fmt_rate(r), "rate_e8": r }),
    )
}

fn list(ctx: &Ctx, a: &Args) -> Result<Value> {
    let mut sql = String::from(
        "SELECT id, base AS 'from', quote AS 'to', date, rate_e8, source FROM fx_rates WHERE 1=1",
    );
    let mut p = vec![];
    for (k, col) in [("from", "base"), ("to", "quote")] {
        if let Some(c) = a.opt_str(k) {
            p.push(sv(&validate_currency(&c)?));
            sql.push_str(&format!(" AND {col}=?{}", p.len()));
        }
    }
    sql.push_str(" ORDER BY date DESC, id DESC");
    let rows = ctx.rows(&sql, &p)?;
    Ok(Value::Array(
        rows.into_iter()
            .map(|mut r| {
                let e8 = r.get("rate_e8").and_then(|v| v.as_i64()).unwrap_or(0);
                r.insert("rate".into(), json!(fmt_rate(e8)));
                Value::Object(r)
            })
            .collect(),
    ))
}

fn rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let id = a.i64("id")?;
    if ctx
        .opt_i64("SELECT id FROM fx_rates WHERE id=?1", &[iv(id)])?
        .is_none()
    {
        return Err(Error::not_found(
            "rate_not_found",
            format!("no exchange rate #{id}"),
        ));
    }
    ctx.delete("fx_rates", id)?;
    Ok(json!({ "removed": id }))
}

fn convert_op(ctx: &Ctx, a: &Args) -> Result<Value> {
    let from = validate_currency(&a.str("from")?)?;
    let to = validate_currency(&a.str("to")?)?;
    let date = a.opt_date("date")?.unwrap_or(ctx.today);
    let amount = a.req_money("amount", decimals_for(&from))?;
    let r = rate_e8(ctx.conn, &from, &to, date)?;
    let out = convert_minor(amount, decimals_for(&from), decimals_for(&to), r)?;
    Ok(json!({
        "from": from, "to": to, "date": fmt_date(date), "rate": fmt_rate(r),
        "amount_minor": amount, "converted_minor": out, "currency": to,
        "from_currency": from,
    }))
}

pub fn ops() -> Vec<OpSpec> {
    let _ = osv;
    vec![
        OpSpec::new(
            "fx.set",
            "Record an exchange rate (1 FROM = RATE TO) as of a date",
            set,
        )
        .write()
        .p(Param::s("from", "currency being converted from, e.g. USD").req())
        .p(Param::s("to", "currency being converted to, e.g. INR").req())
        .p(Param::s("rate", "units of TO per 1 FROM (up to 8 decimals)").req())
        .p(Param::date("date", "effective date (default today)"))
        .ex("pext fx set --from USD --to INR --rate 83.25"),
        OpSpec::new("fx.list", "List recorded exchange rates", list)
            .p(Param::s("from", "filter by base currency"))
            .p(Param::s("to", "filter by quote currency")),
        OpSpec::new("fx.rm", "Delete an exchange rate", rm)
            .write()
            .p(Param::i("id", "rate id").req().pos()),
        OpSpec::new(
            "fx.convert",
            "Convert an amount using recorded rates",
            convert_op,
        )
        .p(Param::s("from", "source currency").req())
        .p(Param::s("to", "target currency").req())
        .p(Param::amount(
            "amount",
            "amount in the source currency, major units",
        ))
        .p(Param::i(
            "amount_minor",
            "same as an integer in minor units",
        ))
        .p(Param::date("date", "rate date (default today)")),
    ]
}
