use chrono::NaiveDate;
use pext_core::invest::{position, rebalance, Bucket, Side, Trade};
use pext_core::loan::{emi, payoff, schedule, Debt, Strategy};
use proptest::prelude::*;

fn start() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 1, 31).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn schedules_always_clear_exactly(p in 1_000i64..5_000_000_000_000, rate in 0i64..=50_000, months in 1i64..=480, extra in 0i64..1_000_000) {
        let e = emi(p, rate, months).unwrap();
        prop_assert!(e >= 1);
        let s = schedule(p, rate, months, e, start(), extra).unwrap();
        prop_assert_eq!(s.rows.last().unwrap().closing, 0);
        prop_assert!(s.rows.len() as i64 <= months);
        let mut prev = p;
        let (mut paid, mut interest, mut principal) = (0i128, 0i128, 0i128);
        for r in &s.rows {
            prop_assert_eq!(r.opening, prev);
            prop_assert_eq!(r.opening + r.interest - r.payment, r.closing);
            prop_assert!(r.closing >= 0);
            prev = r.closing;
            paid += r.payment as i128;
            interest += r.interest as i128;
            principal += r.principal as i128;
        }
        prop_assert_eq!(principal, p as i128);
        prop_assert_eq!(paid, p as i128 + interest);
        prop_assert_eq!(s.total_interest as i128, interest);
        // extra payments never cost more interest
        let base = schedule(p, rate, months, e, start(), 0).unwrap();
        prop_assert!(s.total_interest <= base.total_interest);
    }

    #[test]
    fn payoff_conserves_money(b1 in 1i64..50_000_000, b2 in 1i64..50_000_000, r1 in 0i64..30_000, r2 in 0i64..30_000, extra in 0i64..500_000, avalanche in any::<bool>()) {
        // minimums comfortably above monthly interest so the plan terminates
        let min = |b: i64, r: i64| (b as i128 * r as i128 / 1_200_000) as i64 + b / 100 + 1_000;
        let debts = vec![
            Debt { name: "a".into(), balance: b1, rate_milli: r1, min_payment: min(b1, r1) },
            Debt { name: "b".into(), balance: b2, rate_milli: r2, min_payment: min(b2, r2) },
        ];
        let st = if avalanche { Strategy::Avalanche } else { Strategy::Snowball };
        let plan = payoff(&debts, extra, st, true).unwrap();
        prop_assert_eq!(plan.total_paid as i128, b1 as i128 + b2 as i128 + plan.total_interest as i128);
        let baseline = payoff(&debts, 0, st, false).unwrap();
        prop_assert_eq!(baseline.total_paid as i128, b1 as i128 + b2 as i128 + baseline.total_interest as i128);
        prop_assert!(plan.months <= baseline.months);
        prop_assert!(plan.order.iter().all(|o| o.month >= 1 && o.month <= plan.months));
    }

    #[test]
    fn fifo_never_creates_or_loses_cost(buys in proptest::collection::vec((1i64..1_000_000_000, 1i64..10_000_000), 1..8), sells in proptest::collection::vec((1u32..100, 1i64..10_000_000), 0..6)) {
        let mut trades = vec![];
        let mut id = 1;
        let mut held: i128 = 0;
        for (i, (q, amt)) in buys.iter().enumerate() {
            trades.push(Trade { id, date: NaiveDate::from_ymd_opt(2026, 1, 1 + i as u32).unwrap(), side: Side::Buy, qty_e8: *q, amount_minor: *amt });
            held += *q as i128;
            id += 1;
        }
        // sell fractions (percent) of what is held at that point, all after the last buy
        for (j, (pct, proceeds)) in sells.iter().enumerate() {
            let q = (held * *pct as i128 / 100).max(1);
            if q > held { continue; }
            trades.push(Trade { id, date: NaiveDate::from_ymd_opt(2026, 2, 1 + j as u32).unwrap(), side: Side::Sell, qty_e8: q as i64, amount_minor: *proceeds });
            held -= q;
            id += 1;
        }
        let p = position(&trades).unwrap();
        prop_assert_eq!(p.qty_e8 as i128, held);
        prop_assert_eq!(p.cost_minor as i128 + (p.sold_minor as i128 - p.realised_minor as i128), p.bought_minor as i128);
        prop_assert!(p.cost_minor >= 0);
        if held == 0 { prop_assert_eq!(p.cost_minor, 0); }
        // the order trades are listed in does not change the answer
        let mut rev = trades.clone();
        rev.reverse();
        prop_assert_eq!(position(&rev).unwrap(), p);
    }

    #[test]
    fn rebalance_conserves_value(vals in proptest::collection::vec(0i64..1_000_000_000, 2..6), contribute in 0i64..1_000_000_000) {
        let n = vals.len() as i64;
        let buckets: Vec<Bucket> = vals.iter().enumerate().map(|(i, v)| Bucket { class: format!("c{i}"), value: *v, target_bp: 10_000 / n + if (i as i64) < 10_000 % n { 1 } else { 0 } }).collect();
        let moves = rebalance(&buckets, contribute).unwrap();
        let total: i128 = vals.iter().map(|v| *v as i128).sum::<i128>() + contribute as i128;
        let net: i128 = moves.iter().map(|m| m.delta as i128).sum();
        if contribute > 0 {
            prop_assert_eq!(net, contribute as i128);
            prop_assert!(moves.iter().all(|m| m.delta >= 0));
        } else {
            prop_assert!(net.abs() <= n as i128, "net {net} should be rounding dust only");
        }
        let after: i128 = moves.iter().map(|m| m.current as i128 + m.delta as i128).sum();
        prop_assert!((after - total).abs() <= n as i128);
    }
}
