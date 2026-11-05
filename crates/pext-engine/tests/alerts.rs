mod common;
use common::H;
use serde_json::{json, Value};

fn codes(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|a| a["code"].as_str().unwrap().to_string())
        .collect()
}

fn get<'a>(v: &'a Value, code: &str) -> &'a Value {
    v.as_array()
        .unwrap()
        .iter()
        .find(|a| a["code"] == code)
        .unwrap_or_else(|| panic!("no {code} alert in {v}"))
}

fn base() -> H {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "100000");
    h
}

#[test]
fn quiet_when_nothing_is_wrong() {
    let h = base();
    assert!(h
        .ok("alert.check", json!({}))
        .as_array()
        .unwrap()
        .is_empty());
    let o = h.call("alert.check", json!({"fail_on": "info"})).unwrap();
    assert!(o.meta.get("exit_code").is_none());
}

#[test]
fn budget_states_raise_alerts() {
    let h = base();
    h.txn("Bank", "-750.50", "2026-11-02", Some("food.dining"));
    h.ok(
        "budget.set",
        json!({"category": "food", "amount": "10000", "start": "2026-11-01"}),
    );
    assert!(h
        .ok("alert.check", json!({}))
        .as_array()
        .unwrap()
        .is_empty());
    h.ok("budget.set", json!({"category": "food", "amount": "900"}));
    let a = h.ok("alert.check", json!({}));
    assert_eq!(codes(&a), vec!["budget_near"]);
    assert_eq!(a[0]["severity"], "warning");
    assert!(a[0]["message"].as_str().unwrap().contains("food"));
    h.ok("budget.set", json!({"category": "food", "amount": "700"}));
    let a = h.ok("alert.check", json!({}));
    assert_eq!(codes(&a), vec!["budget_over"]);
    assert_eq!(a[0]["severity"], "critical");
    assert_eq!(a[0]["data"]["spent_minor"], 75_050);
}

#[test]
fn bills_overdue_and_due_soon() {
    let h = base();
    h.ok(
        "recur.add",
        json!({"name": "rent", "account": "Bank", "amount": "25000", "start": "2026-11-01"}),
    );
    h.ok("recur.add", json!({"name": "insurance", "account": "Bank", "amount": "1200", "start": "2026-11-17", "freq": "yearly", "remind_days": 3}));
    h.ok(
        "recur.add",
        json!({"name": "wifi", "account": "Bank", "amount": "800", "start": "2026-11-25"}),
    );
    h.ok("recur.add", json!({"name": "salary", "kind": "income", "account": "Bank", "amount": "50000", "start": "2026-11-16"}));
    let a = h.ok("alert.check", json!({}));
    assert_eq!(
        codes(&a),
        vec!["bill_overdue", "bill_due"],
        "critical first; wifi is too far out and salary is income: {a}"
    );
    assert_eq!(a[0]["data"]["item"], "rent");
    assert_eq!(a[0]["data"]["days_until"], -14);
    assert_eq!(a[1]["data"]["item"], "insurance");
    assert_eq!(a[1]["data"]["days_until"], 2);
    // paying the rent clears the overdue alert
    h.ok("recur.post", json!({"item": "rent"}));
    assert_eq!(codes(&h.ok("alert.check", json!({}))), vec!["bill_due"]);
    // a paused bill never alerts
    h.ok("recur.edit", json!({"item": "insurance", "pause": true}));
    assert!(h
        .ok("alert.check", json!({}))
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn low_balance_alerts() {
    let h = base();
    h.acct("Old", "savings", "0");
    h.ok("alert.set", json!({"account": "Bank", "threshold": "5000"}));
    assert!(h
        .ok("alert.check", json!({}))
        .as_array()
        .unwrap()
        .is_empty()); // balance 100000
    h.txn("Bank", "-96000", "2026-11-10", None);
    let a = h.ok("alert.check", json!({}));
    assert_eq!(codes(&a), vec!["low_balance"]);
    assert_eq!(a[0]["data"]["balance_minor"], 400_000);
    assert_eq!(a[0]["data"]["threshold_minor"], 500_000);
    // changing the threshold updates rather than duplicates
    h.ok("alert.set", json!({"account": "Bank", "threshold": "3000"}));
    assert_eq!(h.ok("alert.list", json!({})).as_array().unwrap().len(), 1);
    assert!(h
        .ok("alert.check", json!({}))
        .as_array()
        .unwrap()
        .is_empty());
    // future-dated transactions do not count yet
    h.txn("Bank", "-3500", "2026-12-20", None);
    assert!(h
        .ok("alert.check", json!({}))
        .as_array()
        .unwrap()
        .is_empty());
    // archived accounts are ignored
    h.ok("alert.set", json!({"account": "Old", "threshold": "1"}));
    assert_eq!(codes(&h.ok("alert.check", json!({}))), vec!["low_balance"]);
    h.ok("acct.archive", json!({"account": "Old"}));
    assert!(h
        .ok("alert.check", json!({}))
        .as_array()
        .unwrap()
        .is_empty());
    h.ok("alert.rm", json!({"account": "Bank"}));
    assert_eq!(
        h.err("alert.rm", json!({"account": "Bank"})).code,
        "alert_not_found"
    );
    assert_eq!(
        h.err("alert.set", json!({"account": "Nope", "threshold": "5"}))
            .code,
        "account_not_found"
    );
    // a negative threshold is allowed: warn before an overdraft limit is hit
    assert_eq!(
        h.ok("alert.set", json!({"account": "Bank", "threshold": "-5"}))["threshold_minor"],
        -500
    );
}

#[test]
fn large_transactions_within_the_window() {
    let h = base();
    h.ok(
        "config.set",
        json!({"key": "large_txn_minor", "value": "500000"}),
    );
    h.txn("Bank", "-6000", "2026-11-13", Some("shopping.electronics"));
    h.txn("Bank", "-6000", "2026-11-01", Some("shopping.electronics"));
    h.txn("Bank", "-4999.99", "2026-11-14", None);
    h.txn("Bank", "90000", "2026-11-14", Some("salary")); // income is not flagged
    let a = h.ok("alert.check", json!({}));
    assert_eq!(
        codes(&a)
            .iter()
            .filter(|c| *c == "large_transaction")
            .count(),
        1
    );
    assert_eq!(get(&a, "large_transaction")["severity"], "info");
    let a = h.ok("alert.check", json!({"days": 20}));
    assert_eq!(
        codes(&a)
            .iter()
            .filter(|c| *c == "large_transaction")
            .count(),
        2
    );
}

#[test]
fn unusual_spending_pace() {
    let h = base();
    for m in ["08", "09", "10"] {
        h.txn(
            "Bank",
            "-3000",
            &format!("2026-{m}-10"),
            Some("shopping.electronics"),
        );
        h.txn(
            "Bank",
            "-1000",
            &format!("2026-{m}-11"),
            Some("food.dining"),
        );
    }
    h.txn("Bank", "-5000", "2026-11-12", Some("shopping.electronics"));
    h.txn("Bank", "-300", "2026-11-12", Some("food.dining"));
    let a = h.ok("alert.check", json!({}));
    assert_eq!(codes(&a), vec!["unusual_spend"]);
    let d = &a[0]["data"];
    assert_eq!(d["category"], "shopping");
    assert_eq!(d["average_minor"], 300_000);
    assert_eq!(d["month_to_date_minor"], 500_000);
    assert_eq!(d["projected_minor"], 1_000_000); // 15 of 30 days elapsed
                                                 // the first week of a month is too early to judge
    let o = h.call_on("2026-11-05", "alert.check", json!({})).unwrap();
    assert!(!codes(&o.data).contains(&"unusual_spend".to_string()));
}

#[test]
fn fail_on_sets_the_exit_code() {
    let h = base();
    h.txn("Bank", "-750.50", "2026-11-02", Some("food.dining"));
    h.ok(
        "budget.set",
        json!({"category": "food", "amount": "900", "start": "2026-11-01"}),
    ); // warning only
    let o = h.call("alert.check", json!({})).unwrap();
    assert!(
        o.meta.get("exit_code").is_none(),
        "no --fail-on means exit 0"
    );
    let o = h
        .call("alert.check", json!({"fail_on": "warning"}))
        .unwrap();
    assert_eq!(o.meta["exit_code"], 10);
    let o = h
        .call("alert.check", json!({"fail_on": "critical"}))
        .unwrap();
    assert!(
        o.meta.get("exit_code").is_none(),
        "a warning is below critical"
    );
    h.ok("budget.set", json!({"category": "food", "amount": "500"}));
    let o = h
        .call("alert.check", json!({"fail_on": "critical"}))
        .unwrap();
    assert_eq!(o.meta["exit_code"], 10);
    assert_eq!(
        h.err("alert.check", json!({"fail_on": "doom"})).code,
        "invalid_argument"
    );
}

#[test]
fn check_is_read_only() {
    let h = base();
    h.ok(
        "recur.add",
        json!({"name": "rent", "account": "Bank", "amount": "25000", "start": "2026-11-01"}),
    );
    let before = h.dump();
    h.ok("alert.check", json!({}));
    h.ok("alert.check", json!({"days": 90, "fail_on": "info"}));
    assert_eq!(before, h.dump());
}
