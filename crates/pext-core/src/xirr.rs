//! Annualised return from dated cash flows (XIRR). This is the one place that
//! uses floating point: it is an analytic estimate shown in basis points and
//! never feeds back into stored money.
#![allow(clippy::float_arithmetic)]

use chrono::NaiveDate;

/// Solve for the annual rate `r` where the net present value of `flows` is 0.
/// Outflows are negative, inflows positive. Returns basis points, or `None`
/// when no rate exists (all flows one sign, or no solution in -99.99%..100000%).
pub fn xirr_bp(flows: &[(NaiveDate, i64)]) -> Option<i64> {
    if flows.len() < 2 || !flows.iter().any(|f| f.1 < 0) || !flows.iter().any(|f| f.1 > 0) {
        return None;
    }
    let t0 = flows.iter().map(|f| f.0).min()?;
    let pts: Vec<(f64, f64)> = flows
        .iter()
        .map(|(d, a)| (((*d - t0).num_days() as f64) / 365.0, *a as f64))
        .collect();
    let npv = |r: f64| -> f64 { pts.iter().map(|(t, a)| a / (1.0 + r).powf(*t)).sum() };
    let (mut lo, mut hi) = (-0.9999f64, 1000.0f64);
    let (flo, fhi) = (npv(lo), npv(hi));
    if !flo.is_finite() || !fhi.is_finite() || flo.signum() == fhi.signum() {
        return None;
    }
    for _ in 0..200 {
        let mid = (lo + hi) / 2.0;
        let f = npv(mid);
        if f.signum() == npv(lo).signum() {
            lo = mid;
        } else {
            hi = mid;
        }
        if (hi - lo).abs() < 1e-12 {
            break;
        }
    }
    let r = (lo + hi) / 2.0;
    Some((r * 10_000.0).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn one_year_ten_percent() {
        assert_eq!(
            xirr_bp(&[(d("2025-01-01"), -100_000), (d("2026-01-01"), 110_000)]),
            Some(1000)
        );
    }

    #[test]
    fn two_years_compounding() {
        // 100 -> 121 in two years is 10% a year
        let r = xirr_bp(&[(d("2024-01-01"), -10_000), (d("2026-01-01"), 12_100)]).unwrap();
        assert!((999..=1001).contains(&r), "{r}");
    }

    #[test]
    fn losses_and_edge_cases() {
        assert_eq!(
            xirr_bp(&[(d("2025-01-01"), -100_000), (d("2026-01-01"), 90_000)]),
            Some(-1000)
        );
        assert_eq!(xirr_bp(&[]), None);
        assert_eq!(xirr_bp(&[(d("2025-01-01"), -5)]), None);
        assert_eq!(xirr_bp(&[(d("2025-01-01"), 5), (d("2026-01-01"), 7)]), None);
    }

    #[test]
    fn monthly_sip() {
        // 12 monthly buys of 1,000 growing to 12,600 at the end
        let mut f: Vec<(NaiveDate, i64)> = (1..=12)
            .map(|m| (d(&format!("2025-{m:02}-01")), -100_000))
            .collect();
        f.push((d("2026-01-01"), 1_260_000));
        let r = xirr_bp(&f).unwrap();
        assert!(r > 800 && r < 1200, "{r}");
    }
}
