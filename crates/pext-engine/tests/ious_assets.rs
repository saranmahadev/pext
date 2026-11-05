mod common;
use common::H;
use serde_json::json;

fn base() -> H {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "10000");
    h
}

#[test]
fn iou_lifecycle() {
    let h = base();
    let a = h.ok("iou.add", json!({"person": "Ravi", "direction": "lent", "amount": "5000", "date": "2026-11-01", "due": "2026-11-10", "note": "bike repair"}));
    assert_eq!(a["status"], "open");
    assert_eq!(a["overdue"], true);
    assert_eq!(a["outstanding_minor"], 500_000);
    h.ok(
        "iou.add",
        json!({"person": "Sam", "direction": "borrowed", "amount": "2000"}),
    );
    assert_eq!(h.ok("iou.list", json!({})).as_array().unwrap().len(), 2);
    assert_eq!(
        h.ok("iou.list", json!({"direction": "lent"}))
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        h.ok("iou.list", json!({"person": "sa"}))[0]["person"],
        "Sam"
    );

    let s = h.ok("iou.settle", json!({"id": 1, "amount": "2000"}));
    assert_eq!(s["status"], "partial");
    assert_eq!(s["outstanding_minor"], 300_000);
    assert_eq!(s["settled_now_minor"], 200_000);
    assert_eq!(
        h.err("iou.settle", json!({"id": 1, "amount": "3000.01"}))
            .code,
        "exceeds_outstanding"
    );
    assert_eq!(
        h.err("iou.settle", json!({"id": 1, "amount": "0"})).code,
        "invalid_amount"
    );
    let s = h.ok("iou.settle", json!({"id": 1}));
    assert_eq!(s["status"], "settled");
    assert_eq!(s["outstanding_minor"], 0);
    assert_eq!(s["overdue"], false);
    assert_eq!(
        h.err("iou.settle", json!({"id": 1})).code,
        "already_settled"
    );
    assert_eq!(h.ok("iou.list", json!({})).as_array().unwrap().len(), 1);
    assert_eq!(
        h.ok("iou.list", json!({"all": true}))
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(h.err("iou.settle", json!({"id": 99})).code, "iou_not_found");
    h.ok("iou.rm", json!({"id": 1}));
    h.doctor_ok();
}

#[test]
fn iou_validation() {
    let h = base();
    let ok = json!({"person": "X", "direction": "lent", "amount": "10"});
    let with = |k: &str, v: serde_json::Value| {
        let mut a = ok.clone();
        a[k] = v;
        a
    };
    assert_eq!(
        h.err("iou.add", with("amount", json!("0"))).code,
        "invalid_amount"
    );
    assert_eq!(
        h.err("iou.add", with("amount", json!("-5"))).code,
        "invalid_amount"
    );
    assert_eq!(
        h.err("iou.add", with("person", json!("   "))).code,
        "invalid_person"
    );
    assert_eq!(
        h.err("iou.add", with("direction", json!("gifted"))).code,
        "invalid_argument"
    );
    assert_eq!(h.err("iou.add", json!({"person": "X", "direction": "lent", "amount": "10", "date": "2026-11-10", "due": "2026-11-01"})).code, "invalid_date");
    assert_eq!(
        h.err("iou.add", with("amount", json!("10.123"))).code,
        "amount_precision"
    );
}

#[test]
fn settling_can_move_money_in_an_account() {
    let h = base();
    h.ok(
        "acct.add",
        json!({"name": "Dollars", "kind": "cash", "currency": "USD"}),
    );
    h.ok(
        "iou.add",
        json!({"person": "Ravi", "direction": "lent", "amount": "5000"}),
    );
    h.ok(
        "iou.add",
        json!({"person": "Sam", "direction": "borrowed", "amount": "2000"}),
    );
    let s = h.ok(
        "iou.settle",
        json!({"id": 1, "amount": "1500", "account": "Bank"}),
    );
    assert!(s["transaction_id"].is_i64());
    assert_eq!(h.balance("Bank"), 1_000_000 + 150_000); // they paid me back
    h.ok("iou.settle", json!({"id": 2, "account": "Bank"}));
    assert_eq!(h.balance("Bank"), 1_150_000 - 200_000); // I paid Sam
    let t = h.ok("txn.get", json!({"id": s["transaction_id"]}));
    assert_eq!(t["payee"], "Ravi");
    assert_eq!(
        h.err("iou.settle", json!({"id": 1, "account": "Dollars"}))
            .code,
        "currency_mismatch"
    );
    // undo reverses both the settlement and the transaction
    h.ok("undo", json!({}));
    assert_eq!(h.balance("Bank"), 1_150_000);
    assert_eq!(
        h.ok("iou.list", json!({"person": "Sam"}))[0]["settled_minor"],
        0
    );
    h.doctor_ok();
}

#[test]
fn iou_summary_nets_per_person() {
    let h = base();
    h.ok(
        "iou.add",
        json!({"person": "Ravi", "direction": "lent", "amount": "5000"}),
    );
    h.ok(
        "iou.add",
        json!({"person": "Ravi", "direction": "borrowed", "amount": "1200"}),
    );
    h.ok(
        "iou.add",
        json!({"person": "Sam", "direction": "borrowed", "amount": "2000"}),
    );
    h.ok(
        "iou.add",
        json!({"person": "Joe", "direction": "lent", "amount": "10", "currency": "USD"}),
    );
    let o = h.call("iou.summary", json!({})).unwrap();
    assert_eq!(o.meta["warnings"][0]["code"], "fx_missing");
    let s = o.data;
    assert_eq!(s["owed_to_me_minor"], 500_000);
    assert_eq!(s["i_owe_minor"], 320_000);
    assert_eq!(s["net_minor"], 180_000);
    assert_eq!(s["people"][0]["person"], "Ravi");
    assert_eq!(s["people"][0]["net_minor"], 380_000);
    assert_eq!(s["people"][1]["net_minor"], -200_000);
    h.ok("fx.set", json!({"from": "USD", "to": "INR", "rate": "80"}));
    assert_eq!(
        h.ok("iou.summary", json!({}))["owed_to_me_minor"],
        500_000 + 80_000
    );
}

// ---- assets --------------------------------------------------------------

#[test]
fn asset_valuation_history() {
    let h = base();
    let a = h.ok("asset.add", json!({"name": "Flat", "kind": "property", "value": "5000000", "date": "2026-01-01", "note": "purchase price"}));
    assert_eq!(a["value_minor"], 500_000_000);
    assert_eq!(a["since_first_value_minor"], 0);
    let a = h.ok(
        "asset.value",
        json!({"asset": "flat", "value": "5500000", "date": "2026-11-01"}),
    );
    assert_eq!(a["value_minor"], 550_000_000);
    assert_eq!(a["since_first_value_pct"], 10);
    assert_eq!(a["valued_on"], "2026-11-01");
    // a back-dated valuation does not replace the latest one
    h.ok(
        "asset.value",
        json!({"asset": "Flat", "value": "5200000", "date": "2026-06-01"}),
    );
    assert_eq!(h.ok("asset.list", json!({}))[0]["value_minor"], 550_000_000);
    // same date again updates it
    h.ok(
        "asset.value",
        json!({"asset": "Flat", "value": "5600000", "date": "2026-11-01"}),
    );
    let hist = h.ok("asset.history", json!({"asset": "Flat"}));
    let rows = hist.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["change_minor"], serde_json::Value::Null);
    assert_eq!(rows[1]["change_minor"], 20_000_000);
    assert_eq!(rows[1]["change_pct"], 4);
    assert_eq!(rows[2]["value_minor"], 560_000_000);
    // dates in the future are rejected beyond a year; negative values always
    assert_eq!(
        h.err("asset.value", json!({"asset": "Flat", "value": "-1"}))
            .code,
        "invalid_amount"
    );
    assert_eq!(
        h.err(
            "asset.value",
            json!({"asset": "Flat", "value": "1", "date": "2030-01-01"})
        )
        .code,
        "date_out_of_range"
    );
    assert_eq!(
        h.err("asset.add", json!({"name": "FLAT", "value": "1"}))
            .code,
        "already_exists"
    );
    assert_eq!(
        h.err(
            "asset.add",
            json!({"name": "x", "kind": "spaceship", "value": "1"})
        )
        .code,
        "invalid_argument"
    );
}

#[test]
fn asset_in_foreign_currency_converts() {
    let h = base();
    h.ok(
        "asset.add",
        json!({"name": "US house", "kind": "property", "currency": "USD", "value": "100000"}),
    );
    assert!(h.ok("asset.list", json!({}))[0]["value_base_minor"].is_null());
    h.ok(
        "fx.set",
        json!({"from": "USD", "to": "INR", "rate": "83.5", "date": "2026-11-01"}),
    );
    assert_eq!(
        h.ok("asset.list", json!({}))[0]["value_base_minor"],
        835_000_000
    );
}

#[test]
fn asset_edit_archive_remove() {
    let h = base();
    h.ok(
        "asset.add",
        json!({"name": "Car", "kind": "vehicle", "value": "800000"}),
    );
    assert_eq!(
        h.err("asset.edit", json!({"asset": "Car"})).code,
        "nothing_to_change"
    );
    h.ok(
        "asset.edit",
        json!({"asset": "Car", "new_name": "Old car", "archive": true}),
    );
    assert!(h.ok("asset.list", json!({})).as_array().unwrap().is_empty());
    assert_eq!(
        h.ok("asset.list", json!({"all": true}))
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        h.err("asset.value", json!({"asset": "Old car", "value": "1"}))
            .code,
        "asset_archived"
    );
    assert_eq!(
        h.err("asset.rm", json!({"asset": "Old car"})).code,
        "confirmation_required"
    );
    let r = h.yes("asset.rm", json!({"asset": "Old car"})).unwrap();
    assert_eq!(r.data["values_removed"], 1);
    h.ok("undo", json!({}));
    assert_eq!(
        h.ok("asset.history", json!({"asset": "Old car"}))
            .as_array()
            .unwrap()
            .len(),
        1
    );
    h.doctor_ok();
}
