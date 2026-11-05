//! Loan maths in integer arithmetic: EMI, amortisation schedules, and
//! multi-debt payoff plans (avalanche / snowball).
//!
//! Rates are in *milli-percent* (8.5% = 8500). Interest accrues monthly at
//! `rate / 12` and is rounded half-to-even to a minor unit each month; the
//! final instalment clears whatever balance remains, so the schedule always
//! ends at exactly zero.

use crate::dates::add_months;
use crate::error::{Error, Result};
use crate::money::{check_amount, mul_div_round};
use chrono::NaiveDate;

pub const MAX_RATE_MILLI: i64 = 50_000; // 50% a year
pub const MAX_MONTHS: i64 = 480; // 40 years
const SCALE: i128 = 1_000_000_000_000; // fixed point for (1+r)^n

fn bad(code: &str, msg: impl Into<String>) -> Error {
    Error::validation(code, msg)
}

pub fn check_terms(principal: i64, rate_milli: i64, months: i64) -> Result<()> {
    if principal <= 0 {
        return Err(bad("invalid_principal", "the loan amount must be positive"));
    }
    check_amount(principal)?;
    if !(0..=MAX_RATE_MILLI).contains(&rate_milli) {
        return Err(bad(
            "invalid_rate",
            "the interest rate must be between 0% and 50% a year",
        ));
    }
    if !(1..=MAX_MONTHS).contains(&months) {
        return Err(bad(
            "invalid_term",
            "the term must be between 1 and 480 months",
        ));
    }
    Ok(())
}

/// (SCALE + r)^n in fixed point, left-to-right square-and-multiply so every
/// intermediate stays below the final value.
fn growth(r_scaled: i128, n: u32) -> Result<i128> {
    let base = SCALE + r_scaled;
    let mut acc = SCALE;
    for bit in (0..32).rev() {
        acc = mul_div_round(acc, acc, SCALE)?;
        if (n >> bit) & 1 == 1 {
            acc = mul_div_round(acc, base, SCALE)?;
        }
    }
    Ok(acc)
}

/// Equal monthly instalment that retires `principal` over `months`.
pub fn emi(principal: i64, rate_milli: i64, months: i64) -> Result<i64> {
    check_terms(principal, rate_milli, months)?;
    let n = months as i128;
    if rate_milli == 0 {
        return Ok((mul_div_round(principal as i128, 1, n)? as i64).max(1));
    }
    let r = mul_div_round(rate_milli as i128, SCALE, 1_200_000)?; // monthly rate, scaled
    let a = growth(r, months as u32)?;
    let x = mul_div_round(r, a, a - SCALE)?; // r * A / (A - 1), scaled
    let e = mul_div_round(principal as i128, x, SCALE)?;
    Ok((e as i64).max(1))
}

pub fn monthly_interest(balance: i64, rate_milli: i64) -> Result<i64> {
    Ok(mul_div_round(balance as i128, rate_milli as i128, 1_200_000)? as i64)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instalment {
    pub n: i64,
    pub due: NaiveDate,
    pub opening: i64,
    pub interest: i64,
    pub principal: i64,
    pub payment: i64,
    pub closing: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    pub rows: Vec<Instalment>,
    pub total_interest: i64,
    pub total_paid: i64,
}

/// Build the schedule. `start` is the disbursal date; instalment `k` falls
/// `k` months later (day-of-month anchor preserved). `extra` is paid on top of
/// `emi` every month, which shortens the loan.
pub fn schedule(
    principal: i64,
    rate_milli: i64,
    months: i64,
    emi: i64,
    start: NaiveDate,
    extra: i64,
) -> Result<Schedule> {
    check_terms(principal, rate_milli, months)?;
    if emi <= 0 || extra < 0 {
        return Err(bad(
            "invalid_emi",
            "the instalment must be positive and extra payments cannot be negative",
        ));
    }
    let mut rows = Vec::new();
    let mut bal = principal;
    let (mut ti, mut tp) = (0i128, 0i128);
    for k in 1..=months {
        let interest = monthly_interest(bal, rate_milli)?;
        let owed = bal + interest;
        let pay = if k == months {
            owed
        } else {
            (emi + extra).min(owed)
        };
        let principal_part = pay - interest;
        let closing = bal - principal_part;
        rows.push(Instalment {
            n: k,
            due: add_months(start, k as i32)?,
            opening: bal,
            interest,
            principal: principal_part,
            payment: pay,
            closing,
        });
        ti += interest as i128;
        tp += pay as i128;
        bal = closing;
        if bal == 0 {
            break;
        }
    }
    Ok(Schedule {
        rows,
        total_interest: ti as i64,
        total_paid: tp as i64,
    })
}

impl Schedule {
    /// Balance still owed once every instalment due on or before `date` is paid.
    pub fn outstanding_on(&self, principal: i64, date: NaiveDate) -> i64 {
        self.rows
            .iter()
            .take_while(|r| r.due <= date)
            .last()
            .map(|r| r.closing)
            .unwrap_or(principal)
    }
    pub fn paid_count_on(&self, date: NaiveDate) -> i64 {
        self.rows.iter().filter(|r| r.due <= date).count() as i64
    }
}

// ---- multi-debt payoff --------------------------------------------------

#[derive(Debug, Clone)]
pub struct Debt {
    pub name: String,
    pub balance: i64,
    pub rate_milli: i64,
    pub min_payment: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Avalanche,
    Snowball,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payoff {
    pub name: String,
    pub month: i64,
    pub interest_paid: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub months: i64,
    pub total_interest: i64,
    pub total_paid: i64,
    pub order: Vec<Payoff>,
}

const MAX_SIM_MONTHS: i64 = 1200;
const MAX_RUNAWAY: i64 = 100_000_000_000_000; // a balance this large means it is not shrinking

fn never_paid() -> Error {
    Error::validation(
        "never_paid_off",
        "at these payments the debts are not cleared within 100 years",
    )
    .with_hint("raise the monthly payment above the interest being charged")
}

/// Simulate paying off `debts`. With `rollover`, the total monthly budget
/// (every minimum plus `extra`) stays constant, so freed-up minimums flow to
/// the next target. Without it each debt just pays its own minimum.
pub fn payoff(debts: &[Debt], extra: i64, strategy: Strategy, rollover: bool) -> Result<Plan> {
    if extra < 0 {
        return Err(bad(
            "invalid_amount",
            "the extra payment cannot be negative",
        ));
    }
    for d in debts {
        if d.balance < 0 || d.min_payment < 0 || !(0..=MAX_RATE_MILLI).contains(&d.rate_milli) {
            return Err(bad(
                "invalid_debt",
                format!("debt '{}' has invalid terms", d.name),
            ));
        }
    }
    let mut bal: Vec<i64> = debts.iter().map(|d| d.balance).collect();
    let mut paid_interest = vec![0i128; debts.len()];
    let mut done: Vec<Option<i64>> = debts
        .iter()
        .map(|d| if d.balance == 0 { Some(0) } else { None })
        .collect();
    let budget: i128 = debts.iter().map(|d| d.min_payment as i128).sum::<i128>() + extra as i128;
    let (mut total_paid, mut total_interest) = (0i128, 0i128);
    let mut month = 0;
    while done.iter().any(|d| d.is_none()) {
        month += 1;
        if month > MAX_SIM_MONTHS {
            return Err(never_paid());
        }
        // 1. interest accrues
        for i in 0..debts.len() {
            if done[i].is_none() {
                let int = monthly_interest(bal[i], debts[i].rate_milli)?;
                bal[i] = match bal[i].checked_add(int) {
                    Some(v) if v <= MAX_RUNAWAY => v,
                    _ => return Err(never_paid()),
                };
                paid_interest[i] += int as i128;
                total_interest += int as i128;
            }
        }
        // 2. minimums
        let mut left = if rollover { budget } else { 0 };
        for i in 0..debts.len() {
            if done[i].is_none() {
                let pay = debts[i].min_payment.min(bal[i]);
                bal[i] -= pay;
                total_paid += pay as i128;
                if rollover {
                    left -= pay as i128;
                } else if debts[i].min_payment == 0 {
                    // nothing paid: stays outstanding
                }
            }
        }
        // 3. everything left goes to the target(s), in strategy order
        if rollover {
            let mut order: Vec<usize> = (0..debts.len())
                .filter(|i| done[*i].is_none() && bal[*i] > 0)
                .collect();
            match strategy {
                Strategy::Avalanche => order.sort_by(|a, b| {
                    debts[*b]
                        .rate_milli
                        .cmp(&debts[*a].rate_milli)
                        .then(bal[*a].cmp(&bal[*b]))
                        .then(a.cmp(b))
                }),
                Strategy::Snowball => order.sort_by(|a, b| {
                    bal[*a]
                        .cmp(&bal[*b])
                        .then(debts[*b].rate_milli.cmp(&debts[*a].rate_milli))
                        .then(a.cmp(b))
                }),
            }
            for i in order {
                if left <= 0 {
                    break;
                }
                let pay = (bal[i] as i128).min(left) as i64;
                bal[i] -= pay;
                total_paid += pay as i128;
                left -= pay as i128;
            }
        } else if extra > 0 {
            // no rollover plan still applies the extra to the strategy's first target
            let mut order: Vec<usize> = (0..debts.len())
                .filter(|i| done[*i].is_none() && bal[*i] > 0)
                .collect();
            match strategy {
                Strategy::Avalanche => order.sort_by(|a, b| {
                    debts[*b]
                        .rate_milli
                        .cmp(&debts[*a].rate_milli)
                        .then(a.cmp(b))
                }),
                Strategy::Snowball => order.sort_by(|a, b| bal[*a].cmp(&bal[*b]).then(a.cmp(b))),
            }
            let mut left = extra as i128;
            for i in order {
                if left <= 0 {
                    break;
                }
                let pay = (bal[i] as i128).min(left) as i64;
                bal[i] -= pay;
                total_paid += pay as i128;
                left -= pay as i128;
            }
        }
        for i in 0..debts.len() {
            if done[i].is_none() && bal[i] == 0 {
                done[i] = Some(month);
            }
        }
    }
    let mut order: Vec<Payoff> = debts
        .iter()
        .enumerate()
        .map(|(i, d)| Payoff {
            name: d.name.clone(),
            month: done[i].unwrap_or(0),
            interest_paid: paid_interest[i] as i64,
        })
        .collect();
    order.sort_by_key(|p| p.month);
    Ok(Plan {
        months: month,
        total_interest: total_interest as i64,
        total_paid: total_paid as i64,
        order,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn emi_matches_known_values() {
        // reference values computed with 50-digit decimal arithmetic
        assert_eq!(emi(100_000_000, 8_500, 240).unwrap(), 867_823);
        assert_eq!(emi(50_000_000, 10_000, 12).unwrap(), 4_395_794);
        assert_eq!(emi(250_000_000, 7_250, 360).unwrap(), 1_705_441);
        // zero interest divides evenly
        assert_eq!(emi(120_000, 0, 12).unwrap(), 10_000);
        assert_eq!(emi(5, 0, 12).unwrap(), 1); // never zero
    }

    #[test]
    fn schedule_ends_at_exactly_zero() {
        let e = emi(100_000_000, 8_500, 240).unwrap();
        let s = schedule(100_000_000, 8_500, 240, e, d("2026-01-31"), 0).unwrap();
        assert_eq!(s.rows.len(), 240);
        assert_eq!(s.rows.last().unwrap().closing, 0);
        assert_eq!(s.rows[0].due, d("2026-02-28"));
        assert_eq!(s.rows[1].due, d("2026-03-31")); // anchor kept
        let sum_p: i64 = s.rows.iter().map(|r| r.principal).sum();
        assert_eq!(sum_p, 100_000_000);
        assert_eq!(s.total_paid, 100_000_000 + s.total_interest);
        // every row balances
        for r in &s.rows {
            assert_eq!(r.opening + r.interest - r.payment, r.closing);
        }
        // the last instalment differs from the EMI by only rounding dust
        assert!(
            (s.rows[239].payment - e).abs() < 300,
            "{} vs {e}",
            s.rows[239].payment
        );
    }

    #[test]
    fn extra_payments_shorten_the_loan_and_save_interest() {
        let e = emi(100_000_000, 8_500, 240).unwrap();
        let base = schedule(100_000_000, 8_500, 240, e, d("2026-01-01"), 0).unwrap();
        let fast = schedule(100_000_000, 8_500, 240, e, d("2026-01-01"), 200_000).unwrap();
        assert!(fast.rows.len() < base.rows.len());
        assert!(fast.total_interest < base.total_interest);
        assert_eq!(fast.rows.last().unwrap().closing, 0);
        assert_eq!(
            fast.rows.iter().map(|r| r.principal).sum::<i64>(),
            100_000_000
        );
    }

    #[test]
    fn outstanding_follows_the_calendar() {
        let e = emi(1_200_000, 12_000, 12).unwrap();
        let s = schedule(1_200_000, 12_000, 12, e, d("2026-01-15"), 0).unwrap();
        assert_eq!(s.outstanding_on(1_200_000, d("2026-02-14")), 1_200_000);
        assert_eq!(
            s.outstanding_on(1_200_000, d("2026-02-15")),
            s.rows[0].closing
        );
        assert_eq!(s.paid_count_on(d("2026-02-15")), 1);
        assert_eq!(s.outstanding_on(1_200_000, d("2030-01-01")), 0);
    }

    #[test]
    fn invalid_terms_are_rejected() {
        assert_eq!(emi(0, 1000, 12).unwrap_err().code, "invalid_principal");
        assert_eq!(emi(1000, -1, 12).unwrap_err().code, "invalid_rate");
        assert_eq!(emi(1000, 50_001, 12).unwrap_err().code, "invalid_rate");
        assert_eq!(emi(1000, 1000, 0).unwrap_err().code, "invalid_term");
        assert_eq!(emi(1000, 1000, 481).unwrap_err().code, "invalid_term");
        assert_eq!(
            emi(10_000_000_000_001, 1000, 12).unwrap_err().code,
            "amount_out_of_range"
        );
        // extreme but legal inputs must not overflow
        assert!(emi(10_000_000_000_000, 50_000, 480).is_ok());
        assert!(schedule(
            10_000_000_000_000,
            50_000,
            480,
            emi(10_000_000_000_000, 50_000, 480).unwrap(),
            d("2026-01-01"),
            0
        )
        .is_ok());
    }

    fn debts() -> Vec<Debt> {
        vec![
            Debt {
                name: "card".into(),
                balance: 5_000_000,
                rate_milli: 36_000,
                min_payment: 200_000,
            },
            Debt {
                name: "car".into(),
                balance: 20_000_000,
                rate_milli: 9_000,
                min_payment: 400_000,
            },
            Debt {
                name: "friend".into(),
                balance: 1_000_000,
                rate_milli: 0,
                min_payment: 50_000,
            },
        ]
    }

    #[test]
    fn avalanche_beats_snowball_on_interest_and_both_beat_minimums() {
        let av = payoff(&debts(), 300_000, Strategy::Avalanche, true).unwrap();
        let sn = payoff(&debts(), 300_000, Strategy::Snowball, true).unwrap();
        let min = payoff(&debts(), 0, Strategy::Avalanche, false).unwrap();
        assert!(
            av.total_interest <= sn.total_interest,
            "{} vs {}",
            av.total_interest,
            sn.total_interest
        );
        assert!(sn.total_interest < min.total_interest);
        assert!(av.months < min.months);
        // money conservation: everything paid is principal plus interest
        assert_eq!(av.total_paid, 26_000_000 + av.total_interest);
        assert_eq!(min.total_paid, 26_000_000 + min.total_interest);
        // avalanche clears the 36% card before the 9% car loan
        let pos = |p: &Plan, n: &str| p.order.iter().position(|x| x.name == n).unwrap();
        assert!(pos(&av, "card") < pos(&av, "car"));
        // snowball clears the smallest balance (friend) first
        assert_eq!(sn.order[0].name, "friend");
    }

    #[test]
    fn unpayable_debt_is_reported() {
        let d = vec![Debt {
            name: "x".into(),
            balance: 1_000_000,
            rate_milli: 36_000,
            min_payment: 1_000,
        }];
        assert_eq!(
            payoff(&d, 0, Strategy::Avalanche, false).unwrap_err().code,
            "never_paid_off"
        );
        assert!(payoff(&[], 0, Strategy::Avalanche, true).unwrap().months == 0);
        assert_eq!(
            payoff(&debts(), -1, Strategy::Avalanche, true)
                .unwrap_err()
                .code,
            "invalid_amount"
        );
    }
}
