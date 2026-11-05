//! Investment accounting: FIFO cost basis, realised and unrealised gains,
//! allocation drift and rebalancing. Quantities and prices are fixed-point
//! integers scaled by 1e8; money is minor units.

use crate::error::{Error, Result};
use crate::money::{allocate, mul_div_round, pow10};
use chrono::NaiveDate;
use std::collections::VecDeque;

pub const E8: i128 = 100_000_000;
/// Largest quantity or price (in e8 units) accepted: 1e8 units / 1e8 per unit.
pub const MAX_E8: i64 = 10_000_000_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
    Dividend,
}

impl Side {
    pub fn parse(s: &str) -> Option<Side> {
        match s {
            "buy" => Some(Side::Buy),
            "sell" => Some(Side::Sell),
            "dividend" => Some(Side::Dividend),
            _ => None,
        }
    }
    fn order(self) -> u8 {
        match self {
            Side::Buy => 0,
            Side::Dividend => 1,
            Side::Sell => 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Trade {
    pub id: i64,
    pub date: NaiveDate,
    pub side: Side,
    pub qty_e8: i64,
    /// Buy: total cost including fees. Sell: net proceeds after fees.
    /// Dividend: cash received.
    pub amount_minor: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lot {
    pub date: NaiveDate,
    pub qty_e8: i64,
    pub cost_minor: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Position {
    pub qty_e8: i64,
    pub cost_minor: i64,
    pub realised_minor: i64,
    pub dividends_minor: i64,
    pub bought_minor: i64,
    pub sold_minor: i64,
    pub lots: Vec<Lot>,
}

/// Replay `trades` (any order) chronologically with FIFO lots.
pub fn position(trades: &[Trade]) -> Result<Position> {
    let mut ts: Vec<&Trade> = trades.iter().collect();
    ts.sort_by_key(|t| (t.date, t.side.order(), t.id));
    let mut lots: VecDeque<Lot> = VecDeque::new();
    let mut p = Position::default();
    let (mut realised, mut divs, mut bought, mut sold) = (0i128, 0i128, 0i128, 0i128);
    for t in ts {
        match t.side {
            Side::Buy => {
                if t.qty_e8 <= 0 {
                    return Err(Error::validation(
                        "invalid_quantity",
                        "a buy needs a positive quantity",
                    ));
                }
                lots.push_back(Lot {
                    date: t.date,
                    qty_e8: t.qty_e8,
                    cost_minor: t.amount_minor,
                });
                bought += t.amount_minor as i128;
            }
            Side::Dividend => divs += t.amount_minor as i128,
            Side::Sell => {
                let mut need = t.qty_e8;
                if need <= 0 {
                    return Err(Error::validation(
                        "invalid_quantity",
                        "a sell needs a positive quantity",
                    ));
                }
                let held: i128 = lots.iter().map(|l| l.qty_e8 as i128).sum();
                if need as i128 > held {
                    return Err(Error::validation(
                        "oversell",
                        format!(
                            "cannot sell {} units on {}: only {} held",
                            fmt_e8(need),
                            t.date,
                            fmt_e8(held as i64)
                        ),
                    )
                    .with_hint("check the quantity and the trade dates"));
                }
                let mut cost_out = 0i128;
                while need > 0 {
                    let front = lots.front_mut().expect("held >= need was checked");
                    if front.qty_e8 <= need {
                        need -= front.qty_e8;
                        cost_out += front.cost_minor as i128;
                        lots.pop_front();
                    } else {
                        let part = mul_div_round(
                            front.cost_minor as i128,
                            need as i128,
                            front.qty_e8 as i128,
                        )?;
                        front.qty_e8 -= need;
                        front.cost_minor -= part as i64;
                        cost_out += part;
                        need = 0;
                    }
                }
                realised += t.amount_minor as i128 - cost_out;
                sold += t.amount_minor as i128;
            }
        }
    }
    p.qty_e8 = lots.iter().map(|l| l.qty_e8).sum();
    p.cost_minor = lots.iter().map(|l| l.cost_minor).sum();
    p.realised_minor = realised as i64;
    p.dividends_minor = divs as i64;
    p.bought_minor = bought as i64;
    p.sold_minor = sold as i64;
    p.lots = lots.into_iter().collect();
    Ok(p)
}

/// qty * price in minor units of a currency with `decimals` places.
pub fn value_minor(qty_e8: i64, price_e8: i64, decimals: u32) -> Result<i64> {
    let v = mul_div_round(qty_e8 as i128 * price_e8 as i128, pow10(decimals), E8 * E8)?;
    i64::try_from(v).map_err(|_| Error::validation("amount_overflow", "value is too large"))
}

pub fn fmt_e8(v: i64) -> String {
    let neg = v < 0;
    let a = (v as i128).unsigned_abs();
    let (i, f) = (a / E8 as u128, a % E8 as u128);
    let mut s = format!("{i}.{f:08}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    if neg {
        format!("-{s}")
    } else {
        s
    }
}

/// Parse a plain decimal ("12.5", "0.00000001") into an e8 fixed-point integer.
pub fn parse_e8(s: &str, what: &str) -> Result<i64> {
    let d = crate::money::parse_decimal(s)?;
    let v = if d.scale <= 8 {
        d.units.checked_mul(pow10(8 - d.scale))
    } else {
        let div = pow10(d.scale - 8);
        if d.units % div != 0 {
            return Err(Error::validation(
                "amount_precision",
                format!("{what} supports at most 8 decimal places"),
            ));
        }
        Some(d.units / div)
    };
    let v = v
        .and_then(|v| i64::try_from(v).ok())
        .ok_or_else(|| Error::validation("amount_out_of_range", format!("{what} is too large")))?;
    if !(0..=MAX_E8).contains(&v) {
        return Err(Error::validation(
            "amount_out_of_range",
            format!("{what} is out of range"),
        ));
    }
    Ok(v)
}

// ---- allocation & rebalancing -------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    pub class: String,
    pub value: i64,
    /// Target share in basis points (0 when no target is set).
    pub target_bp: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    pub class: String,
    pub current: i64,
    pub target: i64,
    /// Positive = buy, negative = sell.
    pub delta: i64,
}

/// How to reach the targets. With `contribute > 0` only buys are suggested and
/// the whole contribution is spent (largest-remainder split, so it sums
/// exactly); with 0 the portfolio is fully rebalanced, buys and sells.
pub fn rebalance(buckets: &[Bucket], contribute: i64) -> Result<Vec<Move>> {
    let total: i128 = buckets.iter().map(|b| b.value as i128).sum();
    let tsum: i64 = buckets.iter().map(|b| b.target_bp).sum();
    if tsum <= 0 {
        return Err(
            Error::validation("no_targets", "set allocation targets first")
                .with_hint("pext invest target set <class> <percent>"),
        );
    }
    if contribute < 0 {
        return Err(Error::validation(
            "invalid_amount",
            "the contribution cannot be negative",
        ));
    }
    let grand = total + contribute as i128;
    let mut moves: Vec<Move> = vec![];
    for b in buckets {
        let target = mul_div_round(grand, b.target_bp as i128, tsum as i128)? as i64;
        moves.push(Move {
            class: b.class.clone(),
            current: b.value,
            target,
            delta: target - b.value,
        });
    }
    if contribute > 0 {
        let short: Vec<u64> = moves.iter().map(|m| m.delta.max(0) as u64).collect();
        if short.iter().all(|s| *s == 0) {
            // everything is at or above target: spread by target weight
            let w: Vec<u64> = buckets.iter().map(|b| b.target_bp.max(0) as u64).collect();
            let parts = allocate(contribute, &w)?;
            for (m, p) in moves.iter_mut().zip(parts) {
                m.delta = p;
            }
        } else {
            let capped = short
                .iter()
                .map(|s| *s as u128)
                .sum::<u128>()
                .min(contribute as u128) as i64;
            let parts = allocate(capped, &short)?;
            let spare = contribute - capped;
            for (m, p) in moves.iter_mut().zip(parts) {
                m.delta = p;
            }
            if spare > 0 {
                let w: Vec<u64> = buckets.iter().map(|b| b.target_bp.max(0) as u64).collect();
                for (m, p) in moves.iter_mut().zip(allocate(spare, &w)?) {
                    m.delta += p;
                }
            }
        }
    }
    Ok(moves)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }
    fn t(id: i64, date: &str, side: Side, qty: i64, amt: i64) -> Trade {
        Trade {
            id,
            date: d(date),
            side,
            qty_e8: qty * E8 as i64,
            amount_minor: amt,
        }
    }

    #[test]
    fn fifo_cost_and_realised_gain() {
        let p = position(&[
            t(1, "2026-01-01", Side::Buy, 10, 100_000),
            t(2, "2026-02-01", Side::Buy, 10, 150_000),
            t(3, "2026-03-01", Side::Sell, 15, 240_000), // 10 @ 10,000 + 5 @ 15,000 cost 175,000... lot2 half
            t(4, "2026-03-02", Side::Dividend, 0, 5_000),
        ])
        .unwrap();
        assert_eq!(p.qty_e8, 5 * E8 as i64);
        assert_eq!(p.cost_minor, 75_000); // half of the second lot
        assert_eq!(p.realised_minor, 240_000 - (100_000 + 75_000));
        assert_eq!(p.dividends_minor, 5_000);
        assert_eq!(p.bought_minor, 250_000);
        assert_eq!(p.lots.len(), 1);
        // cost + realised cost == everything bought
        assert_eq!(
            p.cost_minor + (p.sold_minor - p.realised_minor),
            p.bought_minor
        );
    }

    #[test]
    fn trade_order_is_by_date_not_insertion_and_same_day_buy_precedes_sell() {
        let p = position(&[
            t(5, "2026-03-01", Side::Sell, 4, 50_000),
            t(9, "2026-03-01", Side::Buy, 4, 40_000),
        ])
        .unwrap();
        assert_eq!(p.qty_e8, 0);
        assert_eq!(p.realised_minor, 10_000);
    }

    #[test]
    fn overselling_is_an_error() {
        let e = position(&[
            t(1, "2026-01-01", Side::Buy, 1, 100),
            t(2, "2026-01-02", Side::Sell, 2, 300),
        ])
        .unwrap_err();
        assert_eq!(e.code, "oversell");
        let e = position(&[
            t(1, "2026-02-01", Side::Buy, 1, 100),
            t(2, "2026-01-02", Side::Sell, 1, 100),
        ])
        .unwrap_err();
        assert_eq!(e.code, "oversell"); // sold before it was bought
    }

    #[test]
    fn fractional_units_split_cost_exactly() {
        let p = position(&[
            Trade {
                id: 1,
                date: d("2026-01-01"),
                side: Side::Buy,
                qty_e8: 3,
                amount_minor: 100,
            },
            Trade {
                id: 2,
                date: d("2026-01-02"),
                side: Side::Sell,
                qty_e8: 1,
                amount_minor: 40,
            },
            Trade {
                id: 3,
                date: d("2026-01-03"),
                side: Side::Sell,
                qty_e8: 1,
                amount_minor: 40,
            },
            Trade {
                id: 4,
                date: d("2026-01-04"),
                side: Side::Sell,
                qty_e8: 1,
                amount_minor: 40,
            },
        ])
        .unwrap();
        assert_eq!(p.qty_e8, 0);
        assert_eq!(p.cost_minor, 0);
        assert_eq!(p.realised_minor, 120 - 100); // no cost created or lost to rounding
    }

    #[test]
    fn value_and_parsing() {
        assert_eq!(
            value_minor(15 * E8 as i64 / 10, 25_050_000_000, 2).unwrap(),
            37_575
        ); // 1.5 * 250.50
        assert_eq!(value_minor(1, 1, 2).unwrap(), 0);
        assert_eq!(parse_e8("12.5", "price").unwrap(), 1_250_000_000);
        assert_eq!(parse_e8("0.00000001", "q").unwrap(), 1);
        assert_eq!(
            parse_e8("0.000000001", "q").unwrap_err().code,
            "amount_precision"
        );
        assert!(parse_e8("-1", "q").is_err());
        assert!(parse_e8("200000000", "q").is_err());
        assert_eq!(fmt_e8(1_250_000_000), "12.5");
        assert_eq!(fmt_e8(1), "0.00000001");
        assert_eq!(fmt_e8(5 * E8 as i64), "5");
    }

    fn b(c: &str, v: i64, bp: i64) -> Bucket {
        Bucket {
            class: c.into(),
            value: v,
            target_bp: bp,
        }
    }

    #[test]
    fn full_rebalance_nets_to_zero() {
        let m = rebalance(
            &[
                b("equity", 800_000, 6000),
                b("debt", 150_000, 3000),
                b("gold", 50_000, 1000),
            ],
            0,
        )
        .unwrap();
        assert_eq!(m[0].target, 600_000);
        assert_eq!(m[0].delta, -200_000);
        assert_eq!(m[1].delta, 150_000);
        assert_eq!(m[2].delta, 50_000);
        assert_eq!(m.iter().map(|x| x.delta).sum::<i64>(), 0);
    }

    #[test]
    fn contribution_only_buys_and_is_fully_spent() {
        let bs = [
            b("equity", 800_000, 6000),
            b("debt", 150_000, 3000),
            b("gold", 50_000, 1000),
        ];
        let m = rebalance(&bs, 100_000).unwrap();
        assert!(m.iter().all(|x| x.delta >= 0));
        assert_eq!(m.iter().map(|x| x.delta).sum::<i64>(), 100_000);
        assert_eq!(m[0].delta, 0); // already over target
                                   // a huge contribution overshoots every shortfall: spare is spread by target weight
        let m = rebalance(&bs, 10_000_000).unwrap();
        assert_eq!(m.iter().map(|x| x.delta).sum::<i64>(), 10_000_000);
        assert!(m.iter().all(|x| x.delta > 0));
        assert_eq!(rebalance(&bs, -1).unwrap_err().code, "invalid_amount");
        assert_eq!(
            rebalance(&[b("x", 5, 0)], 0).unwrap_err().code,
            "no_targets"
        );
    }
}
