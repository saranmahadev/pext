mod common;
use common::H;
use serde_json::json;

fn setup() -> H {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "0");
    h
}

#[test]
fn add_computes_the_emi_and_schedule_summary() {
    let h = setup();
    let l = h.ok("loan.add", json!({"name": "Home", "kind": "home", "principal": "1000000", "rate": "8.5", "months": 240, "start": "2026-01-05", "lender": "HDFC"}));
    assert_eq!(l["emi_minor"], 867_823);
    assert_eq!(l["rate_pct"], "8.5");
    assert_eq!(l["instalments_paid"], 10); // Feb..Nov 5
    assert_eq!(l["instalments_left"], 230);
    assert_eq!(l["next_due"], "2026-12-05");
    assert_eq!(l["ends"], "2046-01-05");
    assert_eq!(l["outstanding_source"], "schedule");
    let out = l["outstanding_minor"].as_i64().unwrap();
    assert!(out < 100_000_000 && out > 98_000_000, "{out}");
    assert_eq!(
        l["total_paid_minor"].as_i64().unwrap(),
        100_000_000 + l["total_interest_minor"].as_i64().unwrap()
    );
    assert_eq!(
        l["repaid_pct"],
        (100_000_000 - out) * 100 / 100_000_000
            + if ((100_000_000 - out) * 100) % 100_000_000 >= 50_000_000 {
                1
            } else {
                0
            }
    );
    let g = h.ok("loan.get", json!({"loan": "home"}));
    assert_eq!(g["id"], l["id"]);
}

#[test]
fn validation() {
    let h = setup();
    let ok = json!({"name": "L", "principal": "1000", "rate": "5", "months": 12});
    let with = |k: &str, v: serde_json::Value| {
        let mut a = ok.clone();
        a[k] = v;
        a
    };
    assert_eq!(
        h.err("loan.add", with("principal", json!("0"))).code,
        "invalid_principal"
    );
    assert_eq!(
        h.err("loan.add", with("rate", json!("51"))).code,
        "invalid_rate"
    );
    assert_eq!(
        h.err("loan.add", with("rate", json!("abc"))).code,
        "invalid_rate"
    );
    assert_eq!(
        h.err("loan.add", with("rate", json!("5.1234"))).code,
        "invalid_rate"
    ); // more than 3 decimals
    assert_eq!(
        h.err("loan.add", with("months", json!(0))).code,
        "invalid_term"
    );
    assert_eq!(
        h.err("loan.add", with("months", json!(481))).code,
        "invalid_term"
    );
    assert_eq!(
        h.err("loan.add", with("account", json!("Bank"))).code,
        "invalid_loan_account"
    );
    assert_eq!(
        h.err("loan.add", with("emi", json!("10"))).code,
        "emi_too_low"
    );
    h.ok("loan.add", ok.clone());
    assert_eq!(h.err("loan.add", ok.clone()).code, "already_exists");
    assert_eq!(
        h.err("loan.get", json!({"loan": "nope"})).code,
        "loan_not_found"
    );
    // a higher-than-needed EMI is allowed and ends the loan early
    let l = h.ok(
        "loan.add",
        json!({"name": "Fast", "principal": "1000", "rate": "5", "months": 12, "emi": "200"}),
    );
    assert_eq!(
        l["instalments_left"].as_i64().unwrap() + l["instalments_paid"].as_i64().unwrap(),
        6
    );
}

#[test]
fn rate_accepts_percent_sign_and_zero() {
    let h = setup();
    let l = h.ok(
        "loan.add",
        json!({"name": "Friend", "principal": "1200", "rate": "0%", "months": 12}),
    );
    assert_eq!(l["rate_pct"], "0");
    assert_eq!(l["emi_minor"], 10_000);
    assert_eq!(l["total_interest_minor"], 0);
    let l = h.ok(
        "loan.add",
        json!({"name": "Car", "kind": "car", "principal": "600000", "rate": "9.25%", "months": 60}),
    );
    assert_eq!(l["rate_pct"], "9.25");
}

#[test]
fn schedule_what_if_prepayment() {
    let h = setup();
    h.ok("loan.add", json!({"name": "Home", "principal": "1000000", "rate": "8.5", "months": 240, "start": "2026-01-05"}));
    let s = h.ok("loan.schedule", json!({"loan": "Home"}));
    assert_eq!(s["rows"].as_array().unwrap().len(), 12);
    assert_eq!(s["rows"][0]["n"], 11);
    assert_eq!(s["rows"][0]["due"], "2026-12-05");
    assert_eq!(s["months_saved"], 0);
    assert_eq!(s["interest_saved_minor"], 0);
    let s2 = h.ok(
        "loan.schedule",
        json!({"loan": "Home", "extra": "10000", "limit": 0}),
    );
    assert!(
        s2["months_saved"].as_i64().unwrap() > 40,
        "{}",
        s2["months_saved"]
    );
    assert!(s2["interest_saved_minor"].as_i64().unwrap() > 0);
    assert_eq!(
        s2["rows"].as_array().unwrap().len() as i64,
        s2["with_extra"]["instalments_left"]
    );
    assert_eq!(
        s2["rows"].as_array().unwrap().last().unwrap()["balance_minor"],
        0
    );
    assert_eq!(
        h.err("loan.schedule", json!({"loan": "Home", "extra": "-5"}))
            .code,
        "invalid_amount"
    );
}

#[test]
fn a_finished_loan_has_nothing_left() {
    let h = setup();
    h.ok("loan.add", json!({"name": "Old", "principal": "1000", "rate": "5", "months": 6, "start": "2025-01-01"}));
    let g = h.ok("loan.get", json!({"loan": "Old"}));
    assert_eq!(g["outstanding_minor"], 0);
    assert!(g["next_due"].is_null());
    let s = h.ok("loan.schedule", json!({"loan": "Old"}));
    assert_eq!(s["rows"].as_array().unwrap().len(), 0);
}

#[test]
fn linked_account_overrides_the_schedule() {
    let h = setup();
    h.ok(
        "acct.add",
        json!({"name": "Home loan acct", "kind": "mortgage", "opening": "-950000"}),
    );
    let l = h.ok("loan.add", json!({"name": "Home", "principal": "1000000", "rate": "8.5", "months": 240, "start": "2026-01-05", "account": "Home loan acct"}));
    assert_eq!(l["outstanding_minor"], 95_000_000);
    assert_eq!(l["outstanding_source"], "account");
    assert!(l["scheduled_outstanding_minor"].as_i64().unwrap() != 95_000_000);
    // paying the loan down reduces what is owed
    h.ok(
        "txn.add",
        json!({"account": "Home loan acct", "amount": "50000", "date": "2026-11-10"}),
    );
    assert_eq!(
        h.ok("loan.get", json!({"loan": "Home"}))["outstanding_minor"],
        90_000_000
    );
    assert_eq!(
        h.ok("loan.edit", json!({"loan": "Home", "unlink": true}))["outstanding_source"],
        "schedule"
    );
}

#[test]
fn list_edit_archive_remove() {
    let h = setup();
    h.ok(
        "loan.add",
        json!({"name": "A", "principal": "1000", "rate": "5", "months": 12, "start": "2026-10-01"}),
    );
    h.ok(
        "loan.add",
        json!({"name": "B", "principal": "2000", "rate": "6", "months": 12, "start": "2026-10-01"}),
    );
    let l = h.ok("loan.list", json!({}));
    let rows = l.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2]["name"], "TOTAL");
    assert_eq!(
        rows[2]["emi_minor"].as_i64().unwrap(),
        rows[0]["emi_minor"].as_i64().unwrap() + rows[1]["emi_minor"].as_i64().unwrap()
    );
    assert_eq!(
        h.err("loan.edit", json!({"loan": "A"})).code,
        "nothing_to_change"
    );
    assert_eq!(
        h.err("loan.edit", json!({"loan": "A", "new_name": "b"}))
            .code,
        "already_exists"
    );
    h.ok(
        "loan.edit",
        json!({"loan": "A", "new_name": "Alpha", "lender": "SBI", "archive": true}),
    );
    assert_eq!(h.ok("loan.list", json!({})).as_array().unwrap().len(), 2);
    assert_eq!(
        h.ok("loan.list", json!({"all": true}))
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        h.err("loan.rm", json!({"loan": "B"})).code,
        "confirmation_required"
    );
    h.yes("loan.rm", json!({"loan": "B"})).unwrap();
    h.ok("undo", json!({}));
    assert_eq!(h.ok("loan.get", json!({"loan": "B"}))["name"], "B");
    h.doctor_ok();
}

#[test]
fn debt_plan_compares_strategies() {
    let h = setup();
    h.ok("loan.add", json!({"name": "Car", "kind": "car", "principal": "200000", "rate": "9", "months": 60, "start": "2026-10-01"}));
    h.ok("loan.add", json!({"name": "Personal", "principal": "50000", "rate": "16", "months": 24, "start": "2026-10-01"}));
    let p = h.ok(
        "debt.plan",
        json!({"extra": "5000", "debt": ["Visa:50000:36:2500"]}),
    );
    assert_eq!(p["strategy"], "avalanche");
    assert_eq!(p["payoff_order"].as_array().unwrap().len(), 3);
    assert_eq!(p["payoff_order"][0]["debt"], "Visa"); // highest rate first
    assert!(p["interest_saved_minor"].as_i64().unwrap() > 0);
    assert!(p["months_saved"].as_i64().unwrap() > 0);
    assert_eq!(
        p["total_paid_minor"].as_i64().unwrap(),
        p["total_debt_minor"].as_i64().unwrap() + p["total_interest_minor"].as_i64().unwrap()
    );
    let sn = h.ok(
        "debt.plan",
        json!({"extra": "5000", "strategy": "snowball", "debt": ["Visa:50000:36:2500"]}),
    );
    assert!(
        sn["total_interest_minor"].as_i64().unwrap() >= p["total_interest_minor"].as_i64().unwrap()
    );
    assert_eq!(sn["other_strategy"]["strategy"], "avalanche");
    assert_eq!(
        sn["other_strategy"]["total_interest_minor"],
        p["total_interest_minor"]
    );
    // bad input
    assert_eq!(
        h.err("debt.plan", json!({"debt": ["nonsense"]})).code,
        "invalid_debt"
    );
    assert_eq!(
        h.err("debt.plan", json!({"debt": ["X:100:36:1"]})).code,
        "never_paid_off"
    );
}

#[test]
fn debt_plan_without_debts() {
    let h = setup();
    assert_eq!(h.err("debt.plan", json!({})).code, "no_debts");
}

#[test]
fn credit_utilization() {
    let h = setup();
    h.ok(
        "acct.add",
        json!({"name": "Visa", "kind": "credit_card", "credit_limit": "100000"}),
    );
    h.ok(
        "acct.add",
        json!({"name": "Amex", "kind": "credit_card", "credit_limit": "50000"}),
    );
    h.ok(
        "acct.add",
        json!({"name": "NoLimit", "kind": "credit_card"}),
    );
    h.txn("Visa", "-40000", "2026-11-01", None);
    h.txn("Amex", "-5000", "2026-11-02", None);
    h.txn("Amex", "1000", "2026-11-03", None); // payment
    let u = h.ok("credit.utilization", json!({}));
    let cards = u["cards"].as_array().unwrap();
    let visa = cards.iter().find(|c| c["account"] == "Visa").unwrap();
    assert_eq!(visa["used_minor"], 4_000_000);
    assert_eq!(visa["utilization_pct"], 40);
    assert_eq!(visa["status"], "fair");
    assert_eq!(visa["available_minor"], 6_000_000);
    assert_eq!(visa["pay_down_to_target_minor"], 1_000_000); // down to 30%
    let amex = cards.iter().find(|c| c["account"] == "Amex").unwrap();
    assert_eq!(amex["utilization_pct"], 8);
    assert_eq!(amex["status"], "good");
    let nl = cards.iter().find(|c| c["account"] == "NoLimit").unwrap();
    assert_eq!(nl["status"], "no_limit");
    assert!(nl["utilization_pct"].is_null());
    assert_eq!(u["overall"]["used_minor"], 4_400_000);
    assert_eq!(u["overall"]["limit_minor"], 15_000_000);
    assert_eq!(u["overall"]["utilization_pct"], 29);
    let u = h.ok("credit.utilization", json!({"target": 10}));
    assert_eq!(
        u["cards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["account"] == "Visa")
            .unwrap()["pay_down_to_target_minor"],
        3_000_000
    );
}
