mod common;
use common::H;
use serde_json::{json, Value};

fn setup() -> H {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "0");
    h.ok(
        "invest.add",
        json!({"symbol": "niftybees", "kind": "etf", "name": "Nifty 50 ETF"}),
    );
    h
}

/// 10 @ 245.50 (+12 fee), 5 @ 260, sell 12 @ 300 (-20 fee), dividend 150, price 320.
fn traded() -> H {
    let h = setup();
    let t = |side: &str, qty: &str, price: &str, fee: &str, date: &str| {
        h.ok("trade.add", json!({"instrument": "NIFTYBEES", "side": side, "quantity": qty, "price": price, "fee": fee, "date": date}))
    };
    t("buy", "10", "245.50", "12", "2026-01-10");
    t("buy", "5", "260", "0", "2026-03-10");
    t("sell", "12", "300", "20", "2026-06-01");
    h.ok("trade.add", json!({"instrument": "NIFTYBEES", "side": "dividend", "amount": "150", "date": "2026-07-01"}));
    h.ok(
        "price.set",
        json!({"instrument": "NIFTYBEES", "price": "320", "date": "2026-11-10"}),
    );
    h
}

fn holding<'a>(v: &'a Value, sym: &str) -> &'a Value {
    v["holdings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["instrument"] == sym)
        .unwrap_or_else(|| panic!("no holding {sym}: {v}"))
}

#[test]
fn instruments_are_normalised_and_unique() {
    let h = setup();
    assert_eq!(h.ok("invest.list", json!({}))[0]["symbol"], "NIFTYBEES");
    assert_eq!(
        h.err("invest.add", json!({"symbol": "NiftyBees", "kind": "etf"}))
            .code,
        "already_exists"
    );
    assert_eq!(
        h.err("invest.add", json!({"symbol": "a b", "kind": "etf"}))
            .code,
        "invalid_symbol"
    );
    assert_eq!(
        h.err("invest.add", json!({"symbol": "X", "kind": "lottery"}))
            .code,
        "invalid_argument"
    );
    assert_eq!(
        h.err(
            "trade.add",
            json!({"instrument": "NOPE", "side": "buy", "quantity": "1", "price": "1"})
        )
        .code,
        "instrument_not_found"
    );
}

#[test]
fn trade_amounts_include_fees() {
    let h = setup();
    let b = h.ok("trade.add", json!({"instrument": "niftybees", "side": "buy", "quantity": "10", "price": "245.50", "fee": "12", "date": "2026-01-10"}));
    assert_eq!(b["amount_minor"], 246_700); // 2455.00 + 12.00
    assert_eq!(b["quantity"], "10");
    assert_eq!(b["price"], "245.5");
    let s = h.ok("trade.add", json!({"instrument": "niftybees", "side": "sell", "quantity": "4", "price": "300", "fee": "20", "date": "2026-02-01"}));
    assert_eq!(s["amount_minor"], 118_000); // 1200.00 - 20.00
                                            // fractional units and prices
    let f = h.ok("trade.add", json!({"instrument": "niftybees", "side": "buy", "quantity": "0.5", "price": "100.125", "date": "2026-02-02"}));
    assert_eq!(f["amount_minor"], 5_006); // 50.0625 rounds to 50.06
                                          // prices are recorded automatically on the trade date
    assert_eq!(
        h.ok("price.list", json!({"instrument": "niftybees"}))
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn fifo_realised_gain_and_unrealised() {
    let h = traded();
    let v = h.ok("invest.holdings", json!({}));
    let n = holding(&v, "NIFTYBEES");
    assert_eq!(n["quantity"], "3");
    assert_eq!(n["cost_minor"], 78_000); // 2 of 5 units sold from the second lot
    assert_eq!(n["avg_price"], "260");
    assert_eq!(n["price"], "320");
    assert_eq!(n["price_source"], "price");
    assert_eq!(n["price_age_days"], 5);
    assert_eq!(n["value_minor"], 96_000);
    assert_eq!(n["unrealised_minor"], 18_000);
    assert_eq!(n["unrealised_pct"], 23);
    assert_eq!(n["realised_minor"], 59_300); // 358,000 - (246,700 + 52,000)
    assert_eq!(n["dividends_minor"], 15_000);
    assert_eq!(n["weight_pct"], 100);
    assert!(n["xirr_bp"].as_i64().unwrap() > 0);
    let t = &v["totals"];
    assert_eq!(t["value_minor"], 96_000);
    assert_eq!(t["unrealised_minor"], 18_000);
    assert_eq!(t["realised_minor"], 59_300);
}

#[test]
fn performance_summary() {
    let h = traded();
    let p = h.ok("invest.performance", json!({}));
    assert_eq!(p["invested_minor"], 376_700);
    assert_eq!(p["sold_minor"], 358_000);
    assert_eq!(p["dividends_minor"], 15_000);
    assert_eq!(p["value_minor"], 96_000);
    assert_eq!(p["gain_minor"], 92_300);
    assert_eq!(p["gain_pct"], 25);
    assert!(p["xirr_bp"].as_i64().unwrap() > 1000, "{}", p["xirr_bp"]);
    assert_eq!(p["instruments"][0]["instrument"], "NIFTYBEES");
}

#[test]
fn overselling_is_refused_and_leaves_no_trace() {
    let h = traded();
    let before = h.dump();
    assert_eq!(h.err("trade.add", json!({"instrument": "NIFTYBEES", "side": "sell", "quantity": "4", "price": "1", "date": "2026-11-01"})).code, "oversell");
    // a back-dated sale that would have been impossible at the time
    assert_eq!(h.err("trade.add", json!({"instrument": "NIFTYBEES", "side": "sell", "quantity": "11", "price": "1", "date": "2026-02-01"})).code, "oversell");
    assert_eq!(before, h.dump());
    // removing a buy that a later sale depends on is refused too
    assert_eq!(h.err("trade.rm", json!({"id": 1})).code, "would_oversell");
    assert_eq!(h.ok("trade.list", json!({})).as_array().unwrap().len(), 4);
    assert_eq!(before, h.dump());
    // removing the sale first works, then the buy
    h.ok("trade.rm", json!({"id": 3}));
    h.ok("trade.rm", json!({"id": 1}));
    assert_eq!(
        holding(&h.ok("invest.holdings", json!({})), "NIFTYBEES")["quantity"],
        "5"
    );
    h.doctor_ok();
}

#[test]
fn trade_validation() {
    let h = setup();
    let ok = json!({"instrument": "NIFTYBEES", "side": "buy", "quantity": "1", "price": "10"});
    let with = |k: &str, v: Value| {
        let mut a = ok.clone();
        a[k] = v;
        a
    };
    assert_eq!(
        h.err("trade.add", with("date", json!("2026-11-16"))).code,
        "date_in_future"
    );
    assert_eq!(
        h.err("trade.add", with("quantity", json!("0"))).code,
        "invalid_quantity"
    );
    assert_eq!(
        h.err("trade.add", with("quantity", json!("-1"))).code,
        "amount_out_of_range"
    );
    assert_eq!(
        h.err("trade.add", with("quantity", json!("0.000000001")))
            .code,
        "amount_precision"
    );
    assert_eq!(
        h.err("trade.add", with("price", json!("abc")))
            .kind
            .as_str(),
        "validation"
    );
    assert_eq!(
        h.err("trade.add", with("fee", json!("-1"))).code,
        "invalid_amount"
    );
    assert_eq!(
        h.err("trade.add", with("quantity", json!("100000000000")))
            .code,
        "amount_out_of_range"
    );
    assert_eq!(
        h.err("trade.add", with("side", json!("short"))).code,
        "invalid_argument"
    );
    assert!(h
        .call(
            "trade.add",
            json!({"instrument": "NIFTYBEES", "side": "buy"})
        )
        .is_err()); // no quantity or price
    assert_eq!(
        h.err(
            "trade.add",
            json!({"instrument": "NIFTYBEES", "side": "dividend"})
        )
        .kind
        .as_str(),
        "usage"
    ); // no amount
    assert_eq!(
        h.err(
            "trade.add",
            json!({"instrument": "NIFTYBEES", "side": "dividend", "amount": "0"})
        )
        .code,
        "invalid_amount"
    );
    h.ok("trade.add", ok.clone());
    assert_eq!(h.err("trade.add", json!({"instrument": "NIFTYBEES", "side": "sell", "quantity": "1", "price": "10", "fee": "11"})).code, "invalid_amount");
    // fee > proceeds
}

#[test]
fn tiny_and_huge_quantities_value_correctly() {
    let h = H::on("2026-11-15");
    h.ok("invest.add", json!({"symbol": "BTC", "kind": "crypto"}));
    h.ok("trade.add", json!({"instrument": "BTC", "side": "buy", "quantity": "0.00123456", "price": "6000000", "date": "2026-11-01"}));
    // 0.00123456 * 6,000,000 = 7407.36
    assert_eq!(h.ok("trade.list", json!({}))[0]["amount_minor"], 740_736);
    h.ok(
        "price.set",
        json!({"instrument": "BTC", "price": "6500000", "date": "2026-11-14"}),
    );
    let v = h.ok("invest.holdings", json!({}));
    assert_eq!(holding(&v, "BTC")["value_minor"], 802_464); // 8024.64
    assert_eq!(holding(&v, "BTC")["quantity"], "0.00123456");
}

#[test]
fn archive_and_remove() {
    let h = traded();
    assert_eq!(
        h.err(
            "invest.edit",
            json!({"instrument": "NIFTYBEES", "archive": true})
        )
        .code,
        "still_held"
    );
    h.ok("trade.add", json!({"instrument": "NIFTYBEES", "side": "sell", "quantity": "3", "price": "320", "date": "2026-11-14"}));
    h.ok(
        "invest.edit",
        json!({"instrument": "NIFTYBEES", "archive": true}),
    );
    assert!(h
        .ok("invest.list", json!({}))
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        h.err(
            "trade.add",
            json!({"instrument": "NIFTYBEES", "side": "buy", "quantity": "1", "price": "1"})
        )
        .code,
        "instrument_archived"
    );
    assert!(h.ok("invest.holdings", json!({}))["holdings"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        h.ok("invest.holdings", json!({"all": true}))["holdings"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        h.err("invest.rm", json!({"instrument": "NIFTYBEES"})).code,
        "confirmation_required"
    );
    let r = h
        .yes("invest.rm", json!({"instrument": "NIFTYBEES"}))
        .unwrap();
    assert_eq!(r.data["trades_removed"], 5);
    h.ok("undo", json!({}));
    assert_eq!(h.ok("trade.list", json!({})).as_array().unwrap().len(), 5);
    h.doctor_ok();
}

#[test]
fn trade_list_filters() {
    let h = traded();
    assert_eq!(
        h.ok("trade.list", json!({"side": "buy"}))
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        h.ok(
            "trade.list",
            json!({"from": "2026-03-01", "to": "2026-06-30"})
        )
        .as_array()
        .unwrap()
        .len(),
        2
    );
    assert_eq!(
        h.ok("trade.list", json!({"limit": 1}))[0]["side"],
        "dividend"
    ); // newest first
}

#[test]
fn prices_are_upserted_and_validated() {
    let h = traded();
    h.ok(
        "price.set",
        json!({"instrument": "NIFTYBEES", "price": "330", "date": "2026-11-10"}),
    );
    assert_eq!(
        holding(&h.ok("invest.holdings", json!({})), "NIFTYBEES")["price"],
        "330"
    );
    assert_eq!(
        h.err(
            "price.set",
            json!({"instrument": "NIFTYBEES", "price": "0"})
        )
        .code,
        "invalid_price"
    );
    assert_eq!(
        h.err(
            "price.set",
            json!({"instrument": "NIFTYBEES", "price": "5", "date": "2027-01-01"})
        )
        .code,
        "date_in_future"
    );
    // a price dated after today is never used; an older one still values holdings
    let list = h.ok("price.list", json!({"instrument": "NIFTYBEES", "limit": 2}));
    assert_eq!(list[0]["date"], "2026-11-10");
}

#[test]
fn missing_price_falls_back_to_the_last_trade() {
    let h = setup();
    h.ok("trade.add", json!({"instrument": "NIFTYBEES", "side": "buy", "quantity": "2", "price": "100", "date": "2026-10-01"}));
    h.ok("price.list", json!({"instrument": "NIFTYBEES"}));
    // delete the auto-recorded price to force the fallback path
    let conn = h.conn();
    conn.execute("DELETE FROM prices", []).unwrap();
    let v = h.ok("invest.holdings", json!({}));
    assert_eq!(holding(&v, "NIFTYBEES")["price_source"], "last_trade");
    assert_eq!(holding(&v, "NIFTYBEES")["value_minor"], 20_000);
}

#[test]
fn allocation_targets_and_rebalancing() {
    let h = traded();
    h.ok("invest.add", json!({"symbol": "GOLDBEES", "kind": "gold"}));
    h.ok("trade.add", json!({"instrument": "GOLDBEES", "side": "buy", "quantity": "2", "price": "600", "date": "2026-11-01"}));
    let a = h.ok("invest.allocation", json!({}));
    assert_eq!(a["total_minor"], 216_000);
    let by = |c: &str| {
        a["classes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["class"] == c)
            .unwrap()
            .clone()
    };
    assert_eq!(by("etf")["share_bp"], 4444);
    assert_eq!(by("gold")["share_bp"], 5556);
    assert_eq!(by("gold")["share_pct"], "55.56");
    assert!(by("gold")["target_bp"].is_null());

    h.ok("target.set", json!({"class": "etf", "percent": "60"}));
    assert_eq!(
        h.err("target.set", json!({"class": "gold", "percent": "41"}))
            .code,
        "targets_exceed_100"
    );
    assert_eq!(
        h.err("target.set", json!({"class": "gold", "percent": "101"}))
            .code,
        "invalid_percent"
    );
    assert_eq!(
        h.err("target.set", json!({"class": "gold", "percent": "abc"}))
            .code,
        "invalid_percent"
    );
    assert_eq!(
        h.err("target.set", json!({"class": "stonks", "percent": "5"}))
            .code,
        "invalid_argument"
    );
    h.ok("target.set", json!({"class": "gold", "percent": "40%"}));
    assert_eq!(
        h.err("target.set", json!({"class": "etf", "percent": "60.5"}))
            .code,
        "targets_exceed_100"
    );
    h.ok("target.set", json!({"class": "etf", "percent": "59.5"})); // updating an existing target is fine
    h.ok("target.set", json!({"class": "etf", "percent": "60"}));
    let l = h.ok("target.list", json!({}));
    assert_eq!(l["total_bp"], 10_000);
    assert_eq!(l["unallocated_bp"], 0);

    let a = h.ok("invest.allocation", json!({}));
    let by = |c: &str| {
        a["classes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["class"] == c)
            .unwrap()
            .clone()
    };
    assert_eq!(by("etf")["drift_bp"], -1556);
    assert_eq!(by("gold")["drift_bp"], 1556);

    // full rebalance: sell the overweight class, buy the underweight one
    let r = h.ok("invest.rebalance", json!({}));
    assert_eq!(r["mode"], "full_rebalance");
    let mv = |r: &Value, c: &str| {
        r["moves"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["class"] == c)
            .unwrap()
            .clone()
    };
    assert_eq!(mv(&r, "etf")["action"], "buy");
    assert_eq!(mv(&r, "etf")["amount_minor"], 33_600);
    assert_eq!(mv(&r, "gold")["action"], "sell");
    assert_eq!(mv(&r, "gold")["amount_minor"], 33_600);

    // with fresh money only buys are suggested, and the money is fully used
    let r = h.ok("invest.rebalance", json!({"contribute": "500"}));
    assert_eq!(r["mode"], "contribute_only_buys");
    assert_eq!(mv(&r, "etf")["amount_minor"], 50_000);
    assert_eq!(mv(&r, "gold")["action"], "hold");
    let r = h.ok("invest.rebalance", json!({"contribute": "2000"}));
    assert_eq!(mv(&r, "etf")["amount_minor"], 153_600);
    assert_eq!(mv(&r, "gold")["amount_minor"], 46_400);
    assert_eq!(
        h.err("invest.rebalance", json!({"contribute": "-5"})).code,
        "invalid_amount"
    );

    // holdings in a class without a target are flagged, not silently dropped
    h.ok("invest.add", json!({"symbol": "BTC", "kind": "crypto"}));
    h.ok("trade.add", json!({"instrument": "BTC", "side": "buy", "quantity": "1", "price": "100", "date": "2026-11-02"}));
    let o = h.call("invest.rebalance", json!({})).unwrap();
    assert!(o.meta["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w["code"] == "untargeted_holdings"));
    // targets that do not total 100% are scaled, with a warning
    h.ok("target.rm", json!({"class": "gold"}));
    let o = h.call("invest.rebalance", json!({})).unwrap();
    assert!(o.meta["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w["code"] == "targets_not_100"));
    assert_eq!(
        h.err("target.rm", json!({"class": "gold"})).code,
        "target_not_found"
    );
}

#[test]
fn rebalancing_needs_targets() {
    let h = traded();
    assert_eq!(h.err("invest.rebalance", json!({})).code, "no_targets");
    let a = h.ok("invest.allocation", json!({}));
    assert_eq!(a["targets_total_bp"], 0);
    h.doctor_ok();
}

#[test]
fn doctor_flags_a_corrupted_trade_history() {
    let h = traded();
    h.doctor_ok();
    // simulate tampering outside pext: a sale of units that were never bought
    h.conn()
        .execute(
            "INSERT INTO trades(instrument_id,date,side,qty_e8,price_e8,fee_minor,amount_minor,note,created_at) VALUES(1,'2026-11-01','sell',900000000000,1,0,1,'','x')",
            [],
        )
        .unwrap();
    let o = h.call("doctor", json!({})).unwrap();
    assert_eq!(o.data["ok"], false);
    let c = o.data["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "investment_positions")
        .unwrap()
        .clone();
    assert_eq!(c["ok"], false);
    assert_eq!(o.meta["exit_code"], 8);
}
