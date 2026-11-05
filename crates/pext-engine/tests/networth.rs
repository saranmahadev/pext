mod common;
use common::H;
use serde_json::{json, Value};

fn group<'a>(v: &'a Value, g: &str) -> &'a Value {
    v["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["group"] == g)
        .unwrap_or_else(|| panic!("no group {g}: {v}"))
}

/// A little of everything.
fn world() -> H {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "100000");
    h.acct("Card", "credit_card", "0");
    h.txn("Card", "-20000", "2026-11-05", None);
    h.ok("loan.add", json!({"name": "Home", "principal": "1000000", "rate": "8.5", "months": 240, "start": "2026-01-05"}));
    h.ok(
        "asset.add",
        json!({"name": "Flat", "kind": "property", "value": "5000000"}),
    );
    h.ok("invest.add", json!({"symbol": "NIFTY", "kind": "etf"}));
    h.ok("trade.add", json!({"instrument": "NIFTY", "side": "buy", "quantity": "3", "price": "300", "date": "2026-10-01"}));
    h.ok("price.set", json!({"instrument": "NIFTY", "price": "320"}));
    h.ok(
        "iou.add",
        json!({"person": "Ravi", "direction": "lent", "amount": "5000"}),
    );
    h.ok(
        "iou.add",
        json!({"person": "Sam", "direction": "borrowed", "amount": "2000"}),
    );
    h
}

#[test]
fn net_worth_adds_up_across_every_source() {
    let h = world();
    let loan_out = h.ok("loan.get", json!({"loan": "Home"}))["outstanding_minor"]
        .as_i64()
        .unwrap();
    let nw = h.ok("networth", json!({}));
    let expected_assets = 10_000_000 /* bank */ + 96_000 /* 3 x 320 */ + 500_000_000 /* flat */ + 500_000 /* lent */;
    let expected_liabs = 2_000_000 /* card */ + loan_out + 200_000 /* borrowed */;
    assert_eq!(nw["assets_minor"], expected_assets);
    assert_eq!(nw["liabilities_minor"], expected_liabs);
    assert_eq!(nw["net_worth_minor"], expected_assets - expected_liabs);
    assert_eq!(group(&nw, "accounts")["assets_minor"], 10_000_000);
    assert_eq!(group(&nw, "accounts")["liabilities_minor"], 2_000_000);
    assert_eq!(group(&nw, "investments")["net_minor"], 96_000);
    assert_eq!(group(&nw, "assets")["net_minor"], 500_000_000);
    assert_eq!(group(&nw, "loans")["liabilities_minor"], loan_out);
    assert_eq!(group(&nw, "ious")["net_minor"], 300_000);
    // the group totals reconcile with the headline
    let net_sum: i64 = nw["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["net_minor"].as_i64().unwrap())
        .sum();
    assert_eq!(net_sum, nw["net_worth_minor"].as_i64().unwrap());
    let item_sum: i64 = nw["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["value_base_minor"].as_i64().unwrap())
        .sum();
    assert_eq!(item_sum, nw["net_worth_minor"].as_i64().unwrap());
    h.doctor_ok();
}

#[test]
fn a_loan_linked_to_an_account_is_not_counted_twice() {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "100000");
    h.acct("Mortgage", "mortgage", "-950000");
    h.ok("loan.add", json!({"name": "Home", "principal": "1000000", "rate": "8.5", "months": 240, "start": "2026-01-05", "account": "Mortgage"}));
    let nw = h.ok("networth", json!({}));
    assert_eq!(nw["net_worth_minor"], 10_000_000 - 95_000_000);
    assert!(nw["groups"]
        .as_array()
        .unwrap()
        .iter()
        .all(|g| g["group"] != "loans"));
}

#[test]
fn foreign_currency_needs_a_rate_and_is_flagged_without_one() {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "1000");
    h.ok(
        "acct.add",
        json!({"name": "Dollars", "kind": "savings", "currency": "USD", "opening": "100"}),
    );
    let o = h.call("networth", json!({})).unwrap();
    assert_eq!(o.data["net_worth_minor"], 100_000);
    assert_eq!(o.meta["warnings"][0]["code"], "fx_missing");
    assert_eq!(o.meta["warnings"][0]["items"][0], "Dollars");
    h.ok(
        "fx.set",
        json!({"from": "USD", "to": "INR", "rate": "83.5"}),
    );
    let o = h.call("networth", json!({})).unwrap();
    assert_eq!(o.data["net_worth_minor"], 100_000 + 835_000);
    assert!(o
        .meta
        .get("warnings")
        .is_none_or(|w| w.as_array().is_none_or(|a| a.is_empty())));
}

#[test]
fn archived_empty_accounts_are_hidden_but_archived_balances_count() {
    let h = H::on("2026-11-15");
    h.acct("Empty", "checking", "0");
    h.acct("Old", "savings", "500");
    h.ok("acct.archive", json!({"account": "Empty"}));
    h.ok("acct.archive", json!({"account": "Old"}));
    let nw = h.ok("networth", json!({}));
    let names: Vec<&str> = nw["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["Old"]);
}

#[test]
fn empty_book_is_zero() {
    let h = H::on("2026-11-15");
    let nw = h.ok("networth", json!({}));
    assert_eq!(nw["net_worth_minor"], 0);
    assert!(nw["items"].as_array().unwrap().is_empty());
}

#[test]
fn snapshots_build_a_history() {
    let h = world();
    let s = h.ok("snapshot.take", json!({}));
    assert_eq!(s["replaced_existing"], false);
    let nw1 = h.ok("networth", json!({}))["net_worth_minor"]
        .as_i64()
        .unwrap();
    assert_eq!(s["net_worth_minor"], nw1);
    // later: a new month with a bigger balance
    h.txn("Bank", "5000", "2026-12-02", None);
    let c = h.call_on("2026-12-15", "snapshot.take", json!({})).unwrap();
    assert_eq!(c.data["replaced_existing"], false);
    // retaking on the same day replaces
    assert_eq!(
        h.call_on("2026-12-15", "snapshot.take", json!({}))
            .unwrap()
            .data["replaced_existing"],
        true
    );
    let l = h.ok("snapshot.list", json!({}));
    let rows = l.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["date"], "2026-12-15");
    assert_eq!(rows[1]["date"], "2026-11-15");
    assert!(rows[1]["change_minor"].is_null());
    assert_eq!(
        rows[0]["change_minor"].as_i64().unwrap(),
        rows[0]["net_worth_minor"].as_i64().unwrap() - nw1
    );
    assert_eq!(
        h.ok("snapshot.list", json!({"limit": 1}))
            .as_array()
            .unwrap()
            .len(),
        1
    );
    h.ok("snapshot.rm", json!({"date": "2026-12-15"}));
    assert_eq!(
        h.err("snapshot.rm", json!({"date": "2026-12-15"})).code,
        "snapshot_not_found"
    );
    h.doctor_ok();
}

// ---- forecast ------------------------------------------------------------

#[test]
fn forecast_walks_bills_and_income() {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "100000");
    h.ok(
        "recur.add",
        json!({"name": "rent", "account": "Bank", "amount": "25000", "start": "2026-12-01"}),
    );
    h.ok("recur.add", json!({"name": "salary", "kind": "income", "account": "Bank", "amount": "85000", "start": "2026-11-28"}));
    let f = h.ok("cashflow.forecast", json!({"days": 60}));
    assert_eq!(f["to"], "2027-01-14");
    assert_eq!(f["start_minor"], 10_000_000);
    assert_eq!(f["events_total"], 4); // salary Nov 28 + Dec 28, rent Dec 1 + Jan 1
    assert_eq!(f["expected_inflow_minor"], 17_000_000);
    assert_eq!(f["expected_outflow_minor"], 5_000_000);
    assert_eq!(f["end_minor"], 10_000_000 + 17_000_000 - 5_000_000);
    assert_eq!(f["lowest_minor"], 10_000_000);
    assert_eq!(f["goes_negative"], false);
    let ev = f["events"].as_array().unwrap();
    assert_eq!(ev[0]["name"], "salary");
    assert_eq!(ev[0]["balance_after_minor"], 18_500_000);
    assert_eq!(ev[1]["name"], "rent");
    assert_eq!(ev[1]["balance_after_minor"], 16_000_000);
    let w = f["weekly"].as_array().unwrap();
    assert_eq!(w[0]["date"], "2026-11-15");
    assert_eq!(w.last().unwrap()["date"], "2027-01-14");
    assert_eq!(w.last().unwrap()["balance_minor"], f["end_minor"]);
}

#[test]
fn forecast_spots_a_shortfall() {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "1000");
    h.ok(
        "recur.add",
        json!({"name": "rent", "account": "Bank", "amount": "5000", "start": "2026-11-20"}),
    );
    h.ok("recur.add", json!({"name": "salary", "kind": "income", "account": "Bank", "amount": "50000", "start": "2026-11-30"}));
    let f = h.ok("cashflow.forecast", json!({"days": 30}));
    assert_eq!(f["goes_negative"], true);
    assert_eq!(f["first_negative_on"], "2026-11-20");
    assert_eq!(f["lowest_minor"], -400_000);
    assert_eq!(f["lowest_on"], "2026-11-20");
    assert_eq!(f["negative_days"], 10); // Nov 20..29
}

#[test]
fn forecast_includes_overdue_bills_and_future_dated_entries() {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "10000");
    h.ok(
        "recur.add",
        json!({"name": "rent", "account": "Bank", "amount": "2500", "start": "2026-11-01"}),
    ); // unpaid, overdue
    h.txn("Bank", "-300", "2026-11-20", None);
    let f = h.ok("cashflow.forecast", json!({"days": 10, "events": 10}));
    let ev = f["events"].as_array().unwrap();
    assert_eq!(ev[0]["name"], "rent");
    assert_eq!(ev[0]["date"], "2026-11-15");
    assert_eq!(ev[0]["was_overdue"], true);
    assert_eq!(ev[1]["date"], "2026-11-20");
    assert_eq!(f["start_minor"], 1_000_000 - 30_000 + 30_000); // the future entry is not in today's balance
    assert_eq!(f["end_minor"], 1_000_000 - 250_000 - 30_000);
}

#[test]
fn forecast_account_selection() {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "1000");
    h.acct("Card", "credit_card", "0");
    h.ok(
        "recur.add",
        json!({"name": "netflix", "account": "Card", "amount": "500", "start": "2026-11-20"}),
    );
    assert_eq!(
        h.ok("cashflow.forecast", json!({"days": 10}))["events_total"],
        0
    ); // card is not liquid
    let f = h.ok(
        "cashflow.forecast",
        json!({"days": 10, "account": ["Card"]}),
    );
    assert_eq!(f["events_total"], 1);
    assert_eq!(f["end_minor"], -50_000);
    assert_eq!(
        h.err("cashflow.forecast", json!({"account": ["Nope"]}))
            .code,
        "account_not_found"
    );
    h.ok("acct.archive", json!({"account": "Bank"}));
    assert_eq!(
        h.ok(
            "cashflow.forecast",
            json!({"days": 10, "account": ["Card"]})
        )["accounts"][0],
        "Card"
    );
    let e = H::on("2026-11-15");
    assert_eq!(e.err("cashflow.forecast", json!({})).code, "no_accounts");
}

#[test]
fn forecast_is_read_only() {
    let h = world();
    let before = h.dump();
    h.ok("cashflow.forecast", json!({"days": 365}));
    h.ok("runway", json!({}));
    h.ok("networth", json!({}));
    assert_eq!(before, h.dump());
}

// ---- runway -----------------------------------------------------------------

#[test]
fn runway_from_recent_spending() {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "0");
    h.txn("Bank", "100000", "2026-07-01", Some("salary"));
    for m in ["08", "09", "10"] {
        h.txn(
            "Bank",
            "-30000",
            &format!("2026-{m}-10"),
            Some("food.groceries"),
        );
    }
    h.txn("Bank", "-9999", "2026-11-03", Some("food.groceries")); // this month is excluded from the average
    let r = h.ok("runway", json!({}));
    assert_eq!(r["avg_monthly_spend_minor"], 3_000_000);
    assert_eq!(r["liquid_minor"], 10_000_000 - 9_000_000 - 999_900);
    h.txn("Bank", "200000", "2026-11-04", Some("salary"));
    let r = h.ok("runway", json!({}));
    let liquid = 10_000_000 - 9_000_000 - 999_900 + 20_000_000;
    assert_eq!(r["liquid_minor"], liquid);
    assert_eq!(r["runway_days"], liquid * 30 / 3_000_000);
    assert_eq!(r["runway_months"], "6.7"); // 6.667 months, rounded to one decimal
    assert_eq!(r["window"], "2026-08-01 to 2026-10-31");
    // a one-month window sees only October
    let r = h.ok("runway", json!({"months": 1}));
    assert_eq!(r["avg_monthly_spend_minor"], 3_000_000);
}

#[test]
fn runway_without_spending_or_money() {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "5000");
    let r = h.ok("runway", json!({}));
    assert!(r["runway_days"].is_null());
    assert!(r["runway_months"].is_null());
    h.txn("Bank", "-9000", "2026-10-01", Some("food.groceries"));
    h.txn("Bank", "-1000", "2026-09-01", Some("food.groceries"));
    let r = h.ok("runway", json!({"months": 2}));
    assert_eq!(r["liquid_minor"], 500_000 - 1_000_000);
    assert_eq!(r["runway_days"], 0); // already negative
}
