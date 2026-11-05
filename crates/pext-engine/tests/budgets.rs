mod common;
use common::H;
use serde_json::{json, Value};

fn seed() -> H {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "0");
    let add = |amt: &str, date: &str, cat: &str| {
        h.ok(
            "txn.add",
            json!({"account": "Bank", "amount": amt, "date": date, "category": cat}),
        );
    };
    add("-450.50", "2026-11-01", "food.delivery");
    add("-300", "2026-11-02", "food.dining");
    add("-1200", "2026-10-20", "food.groceries");
    add("-2500", "2026-09-10", "shopping.electronics");
    add("85000", "2026-11-01", "salary");
    h
}

fn row<'a>(s: &'a Value, cat: &str) -> &'a Value {
    s["budgets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["category"] == cat)
        .unwrap_or_else(|| panic!("no budget row for {cat}: {s}"))
}

#[test]
fn spent_remaining_and_pace() {
    let h = seed();
    h.ok(
        "budget.set",
        json!({"category": "food", "amount": "10000", "start": "2026-11-01"}),
    );
    let s = h.ok("budget.status", json!({}));
    let r = row(&s, "food");
    assert_eq!(r["budget_minor"], 1_000_000);
    assert_eq!(r["spent_minor"], 75_050); // sub-categories roll up; October groceries excluded
    assert_eq!(r["remaining_minor"], 924_950);
    assert_eq!(r["pct_used"], 8); // 7.505% rounds to 8
    assert_eq!(r["state"], "ok");
    assert_eq!(r["period_start"], "2026-11-01");
    assert_eq!(r["period_end"], "2026-11-30");
    assert_eq!(r["days_left"], 15);
    assert_eq!(r["daily_allowance_minor"], 924_950 / 15);
    // 750.50 over 15 days elapsed projects to 1501.00 for the 30-day month
    assert_eq!(r["projected_minor"], 150_100);
    assert_eq!(r["projected_over"], false);
    assert_eq!(s["totals"]["spent_minor"], 75_050);
    assert_eq!(s["currency"], "INR");
}

#[test]
fn warning_and_over_states() {
    let h = seed();
    h.ok(
        "budget.set",
        json!({"category": "food", "amount": "900", "start": "2026-11-01"}),
    );
    assert_eq!(
        row(&h.ok("budget.status", json!({})), "food")["state"],
        "warning"
    ); // 83%
    h.ok("budget.set", json!({"category": "food", "amount": "700"}));
    let s = h.ok("budget.status", json!({}));
    assert_eq!(row(&s, "food")["state"], "over");
    assert_eq!(row(&s, "food")["remaining_minor"], 70_000 - 75_050);
    assert_eq!(row(&s, "food")["daily_allowance_minor"], 0);
    assert_eq!(h.ok("budget.list", json!({})).as_array().unwrap().len(), 1); // upsert, not duplicate
}

#[test]
fn rollover_carries_unspent_but_not_overspend() {
    let h = seed();
    // Sept: 0 spent on food -> carry 2000; Oct: 4000 available, 1200 spent -> carry 2800; Nov available 4800
    h.ok(
        "budget.set",
        json!({"category": "food", "amount": "2000", "rollover": true, "start": "2026-09-01"}),
    );
    let r = h.ok("budget.status", json!({}));
    let r = row(&r, "food");
    assert_eq!(r["carried_minor"], 280_000);
    assert_eq!(r["available_minor"], 480_000);
    assert_eq!(r["remaining_minor"], 480_000 - 75_050);
    // overspending is not carried: Oct 1200 spent vs 1000 budget -> carry 0
    let h2 = seed();
    h2.ok(
        "budget.set",
        json!({"category": "food", "amount": "1000", "rollover": true, "start": "2026-10-01"}),
    );
    let r2 = h2.ok("budget.status", json!({}));
    assert_eq!(row(&r2, "food")["carried_minor"], 0);
    // looking at October itself
    let oct = h2.ok("budget.status", json!({"date": "2026-10-31"}));
    assert_eq!(row(&oct, "food")["spent_minor"], 120_000);
    assert_eq!(row(&oct, "food")["state"], "over");
    // before the budget starts there is no row
    let sep = h2.ok("budget.status", json!({"date": "2026-09-15"}));
    assert!(sep["budgets"].as_array().unwrap().is_empty());
}

#[test]
fn refunds_reduce_spending_and_transfers_are_ignored() {
    let h = seed();
    h.ok(
        "budget.set",
        json!({"category": "food", "amount": "1000", "start": "2026-11-01"}),
    );
    h.ok("txn.add", json!({"account": "Bank", "amount": "100", "category": "food.dining", "date": "2026-11-03"}));
    assert_eq!(
        row(&h.ok("budget.status", json!({})), "food")["spent_minor"],
        65_050
    );
    h.acct("Other", "savings", "0");
    h.ok(
        "txn.transfer",
        json!({"from": "Bank", "to": "Other", "amount": "5000", "date": "2026-11-04"}),
    );
    assert_eq!(
        row(&h.ok("budget.status", json!({})), "food")["spent_minor"],
        65_050
    );
}

#[test]
fn quarterly_and_yearly_use_calendar_quarter_and_financial_year() {
    let h = seed();
    h.ok("budget.set", json!({"category": "shopping", "amount": "10000", "period": "yearly", "start": "2026-04-01"}));
    let s = h.ok("budget.status", json!({"period": "yearly"}));
    let r = row(&s, "shopping");
    assert_eq!(r["period_start"], "2026-04-01");
    assert_eq!(r["period_end"], "2027-03-31");
    assert_eq!(r["spent_minor"], 250_000);
    assert_eq!(r["pct_used"], 25);
    h.ok(
        "budget.set",
        json!({"category": "food", "amount": "5000", "period": "quarterly", "start": "2026-10-01"}),
    );
    let q = h.ok("budget.status", json!({"period": "quarterly"}));
    let r = row(&q, "food");
    assert_eq!(r["period_start"], "2026-10-01");
    assert_eq!(r["spent_minor"], 120_000 + 75_050);
    // a January FY changes the yearly window
    h.ok("config.set", json!({"key": "fy_start_month", "value": "1"}));
    let s = h.ok("budget.status", json!({"period": "yearly"}));
    assert_eq!(row(&s, "shopping")["period_start"], "2026-01-01");
}

#[test]
fn parent_and_child_budgets_do_not_double_count_in_totals() {
    let h = seed();
    h.ok(
        "budget.set",
        json!({"category": "food", "amount": "1000", "start": "2026-11-01"}),
    );
    h.ok(
        "budget.set",
        json!({"category": "food.dining", "amount": "500", "start": "2026-11-01"}),
    );
    let s = h.ok("budget.status", json!({}));
    assert_eq!(row(&s, "food.dining")["spent_minor"], 30_000);
    assert_eq!(s["totals"]["available_minor"], 100_000);
    assert_eq!(s["totals"]["spent_minor"], 75_050);
}

#[test]
fn foreign_currency_spending_converts_or_warns() {
    let h = seed();
    h.ok(
        "acct.add",
        json!({"name": "Dollar", "kind": "checking", "currency": "USD"}),
    );
    h.ok("txn.add", json!({"account": "Dollar", "amount": "-10", "date": "2026-11-05", "category": "food.dining"}));
    h.ok(
        "budget.set",
        json!({"category": "food", "amount": "1000", "start": "2026-11-01"}),
    );
    let o = h.call("budget.status", json!({})).unwrap();
    assert_eq!(o.meta["warnings"][0]["code"], "fx_missing");
    assert_eq!(row(&o.data, "food")["spent_minor"], 75_050);
    h.ok(
        "fx.set",
        json!({"from": "USD", "to": "INR", "rate": "83", "date": "2026-11-01"}),
    );
    let o = h.call("budget.status", json!({})).unwrap();
    assert!(o.meta.get("warnings").is_none());
    assert_eq!(row(&o.data, "food")["spent_minor"], 75_050 + 83_000);
}

#[test]
fn validation_and_lifecycle() {
    let h = seed();
    assert_eq!(
        h.err("budget.set", json!({"category": "salary", "amount": "100"}))
            .code,
        "invalid_budget_category"
    );
    assert_eq!(
        h.err("budget.set", json!({"category": "food", "amount": "0"}))
            .code,
        "invalid_amount"
    );
    assert_eq!(
        h.err("budget.set", json!({"category": "food", "amount": "-5"}))
            .code,
        "invalid_amount"
    );
    assert_eq!(
        h.err("budget.set", json!({"category": "nope", "amount": "5"}))
            .code,
        "category_not_found"
    );
    assert_eq!(
        h.err(
            "budget.set",
            json!({"category": "food", "amount": "5", "period": "weekly"})
        )
        .kind
        .as_str(),
        "validation"
    );
    h.ok("budget.set", json!({"category": "food", "amount": "5"}));
    assert_eq!(
        h.err("budget.rm", json!({"category": "food", "period": "yearly"}))
            .code,
        "budget_not_found"
    );
    // merging a budgeted category into one that has its own budget is refused
    h.ok(
        "budget.set",
        json!({"category": "food.groceries", "amount": "5"}),
    );
    h.ok(
        "budget.set",
        json!({"category": "food.dining", "amount": "5"}),
    );
    assert_eq!(
        h.err(
            "cat.merge",
            json!({"from": "food.groceries", "into": "food.dining"})
        )
        .code,
        "already_exists"
    );
    h.ok(
        "budget.set",
        json!({"category": "transport.fuel", "amount": "5"}),
    );
    h.ok(
        "cat.merge",
        json!({"from": "transport.fuel", "into": "transport.taxi"}),
    );
    assert_eq!(
        h.ok("budget.list", json!({}))
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["category"] == "transport.taxi")
            .count(),
        1
    );
    h.ok("budget.rm", json!({"category": "food"}));
    h.doctor_ok();
}
