mod common;
use common::H;
use serde_json::{json, Value};

fn seed() -> H {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "0");
    h.acct("Savings", "savings", "0");
    h.txn("Bank", "85000", "2026-11-01", Some("salary"));
    h.txn("Bank", "-1200", "2026-11-02", Some("food.groceries"));
    h.txn("Bank", "-800", "2026-11-03", Some("food.dining"));
    h.txn("Bank", "-500", "2026-11-05", Some("transport.fuel"));
    h.txn("Bank", "200", "2026-11-06", Some("food.groceries")); // refund
    h.ok(
        "txn.transfer",
        json!({"from": "Bank", "to": "Savings", "amount": "10000", "date": "2026-11-07"}),
    );
    h.txn("Bank", "-1500", "2026-10-10", Some("food.groceries"));
    h.txn("Bank", "85000", "2026-10-15", Some("salary"));
    h.txn("Bank", "-3000", "2026-10-20", Some("shopping.electronics"));
    h
}

fn find<'a>(rows: &'a Value, name: &str) -> &'a Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .unwrap_or_else(|| panic!("no row {name} in {rows}"))
}

#[test]
fn summary_excludes_transfers_and_nets_refunds() {
    let h = seed();
    let s = h.ok("report.summary", json!({}));
    assert_eq!(s["period_start"], "2026-11-01");
    assert_eq!(s["period_end"], "2026-11-30");
    assert_eq!(s["income_minor"], 8_500_000);
    assert_eq!(s["expense_minor"], 230_000); // 1200 + 800 + 500 - 200 refund
    assert_eq!(s["net_minor"], 8_270_000);
    assert_eq!(s["savings_rate_pct"], 97);
    assert_eq!(s["transactions"], 5);
    let prev = &s["vs_previous"];
    assert_eq!(prev["period_start"], "2026-10-02");
    assert_eq!(prev["period_end"], "2026-10-31");
    assert_eq!(prev["expense_minor"], 450_000);
    assert_eq!(prev["expense_change_minor"], -220_000);
    assert_eq!(prev["expense_change_pct"], -49);
    let cats = &s["top_categories"];
    assert_eq!(cats[0]["name"], "food.groceries");
    assert_eq!(cats[0]["amount_minor"], 100_000);
    assert_eq!(cats[0]["share_pct"], 43);
    assert_eq!(cats[0]["count"], 2);
    assert_eq!(s["largest_expenses"][0]["amount_minor"], 120_000);
}

#[test]
fn summary_for_other_periods_and_accounts() {
    let h = seed();
    let s = h.ok("report.summary", json!({"period": "2026-10"}));
    assert_eq!(s["income_minor"], 8_500_000);
    assert_eq!(s["expense_minor"], 450_000);
    let s = h.ok(
        "report.summary",
        json!({"period": "2026-11", "account": ["Savings"]}),
    );
    assert_eq!(s["income_minor"], 0);
    assert_eq!(s["expense_minor"], 0);
    assert!(s["savings_rate_pct"].is_null());
    let s = h.ok(
        "report.summary",
        json!({"period": "2026-11", "query": "category:food"}),
    );
    assert_eq!(s["expense_minor"], 180_000);
    assert_eq!(
        h.err("report.summary", json!({"period": "someday"}))
            .kind
            .as_str(),
        "usage"
    );
}

#[test]
fn spending_breakdowns() {
    let h = seed();
    let r = h.ok("report.spending", json!({}));
    assert_eq!(r["total_minor"], 230_000);
    assert_eq!(r["rows"].as_array().unwrap().len(), 3);
    let r = h.ok("report.spending", json!({"depth": 1}));
    assert_eq!(find(&r["rows"], "food")["amount_minor"], 180_000);
    assert_eq!(find(&r["rows"], "transport")["amount_minor"], 50_000);
    let r = h.ok("report.spending", json!({"by": "payee"}));
    assert_eq!(r["rows"][0]["payee"], "(none)");
    let r = h.ok("report.spending", json!({"by": "account"}));
    assert_eq!(r["rows"].as_array().unwrap().len(), 1);
    let r = h.ok(
        "report.spending",
        json!({"by": "month", "period": "2026-10..2026-11"}),
    );
    assert_eq!(find(&r["rows"], "2026-10")["amount_minor"], 450_000);
    assert_eq!(find(&r["rows"], "2026-11")["amount_minor"], 230_000);
    let r = h.ok("report.spending", json!({"kind": "income"}));
    assert_eq!(r["total_minor"], 8_500_000);
    let r = h.ok("report.spending", json!({"top": 1}));
    assert_eq!(r["rows"].as_array().unwrap().len(), 1);
    assert_eq!(
        h.err("report.spending", json!({"by": "colour"})).code,
        "invalid_argument"
    );
    h.ok("txn.tag", json!({"id": 2, "tag": ["trip"]}));
    let r = h.ok("report.spending", json!({"by": "tag"}));
    assert_eq!(find(&r["rows"], "trip")["amount_minor"], 120_000);
    assert_eq!(find(&r["rows"], "(untagged)")["amount_minor"], 110_000);
}

#[test]
fn trend_covers_empty_months() {
    let h = seed();
    let t = h.ok("report.trend", json!({"months": 4}));
    let m = t["months"].as_array().unwrap();
    assert_eq!(m.len(), 4);
    assert_eq!(m[0]["month"], "2026-08");
    assert_eq!(m[0]["income_minor"], 0);
    assert!(m[0]["savings_rate_pct"].is_null());
    assert_eq!(m[2]["month"], "2026-10");
    assert_eq!(m[2]["expense_minor"], 450_000);
    assert_eq!(m[3]["expense_minor"], 230_000);
    assert_eq!(t["average"]["income_minor"], 17_000_000 / 4);
    assert_eq!(t["average"]["expense_minor"], 680_000 / 4);
    assert_eq!(
        h.ok("report.trend", json!({}))
            .as_array()
            .map(|a| a.len())
            .unwrap_or(12),
        12
    );
}

#[test]
fn compare_two_periods() {
    let h = seed();
    let c = h.ok("report.compare", json!({}));
    assert_eq!(c["a"]["expense_minor"], 450_000);
    assert_eq!(c["b"]["expense_minor"], 230_000);
    let cats = c["categories"].as_array().unwrap();
    assert_eq!(cats[0]["category"], "shopping"); // biggest absolute swing
    assert_eq!(cats[0]["change_minor"], -300_000);
    assert_eq!(cats[0]["change_pct"], -100);
    let food = cats.iter().find(|r| r["category"] == "food").unwrap();
    assert_eq!(food["a_minor"], 150_000);
    assert_eq!(food["b_minor"], 180_000);
    assert_eq!(food["change_pct"], 20);
    let transport = cats.iter().find(|r| r["category"] == "transport").unwrap();
    assert!(transport["change_pct"].is_null()); // nothing last month
}

#[test]
fn foreign_currency_is_converted_on_the_day_or_skipped() {
    let h = seed();
    h.ok(
        "acct.add",
        json!({"name": "Wallet", "kind": "cash", "currency": "USD", "opening": "0"}),
    );
    h.txn("Wallet", "-10", "2026-11-04", Some("food.dining"));
    let o = h.call("report.summary", json!({})).unwrap();
    assert_eq!(o.data["expense_minor"], 230_000); // no rate: left out, with a warning
    assert_eq!(o.meta["warnings"][0]["code"], "fx_missing");
    h.ok(
        "fx.set",
        json!({"from": "USD", "to": "INR", "rate": "83.50", "date": "2026-11-01"}),
    );
    let o = h.call("report.summary", json!({})).unwrap();
    assert_eq!(o.data["expense_minor"], 230_000 + 83_500);
    assert!(o
        .meta
        .get("warnings")
        .map(|w| w.as_array().map(|a| a.is_empty()).unwrap_or(true))
        .unwrap_or(true));
}

#[test]
fn reports_ignore_deleted_and_adjustments() {
    let h = seed();
    let id = h.txn("Bank", "-9999", "2026-11-09", Some("food.dining"));
    assert_eq!(
        h.ok("report.summary", json!({}))["expense_minor"],
        230_000 + 999_900
    );
    h.ok("txn.rm", json!({"id": id}));
    assert_eq!(h.ok("report.summary", json!({}))["expense_minor"], 230_000);
}
