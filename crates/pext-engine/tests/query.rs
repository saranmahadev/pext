mod common;
use common::H;
use serde_json::json;

fn seed() -> H {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "0");
    h.acct("Card", "credit_card", "0");
    let add = |acct: &str, amt: &str, date: &str, cat: &str, payee: &str, memo: &str| {
        h.ok("txn.add", json!({"account": acct, "amount": amt, "date": date, "category": cat, "payee": payee, "memo": memo}));
    };
    add(
        "Bank",
        "-450.50",
        "2026-11-01",
        "food.delivery",
        "Swiggy",
        "dinner",
    );
    add(
        "Bank",
        "-300",
        "2026-11-02",
        "food.dining",
        "Cafe Coffee",
        "team lunch",
    );
    add(
        "Card",
        "-1200",
        "2026-10-20",
        "food.groceries",
        "BigBasket",
        "",
    );
    add("Bank", "85000", "2026-11-01", "salary", "Acme Corp", "");
    add("Bank", "-600", "2026-10-05", "transport.fuel", "Shell", "");
    add(
        "Card",
        "-2500",
        "2026-09-10",
        "shopping.electronics",
        "Croma",
        "headphones",
    );
    h
}

fn n(h: &H, q: &str) -> usize {
    h.ok("query", json!({"query": q})).as_array().unwrap().len()
}

#[test]
fn query_filters() {
    let h = seed();
    assert_eq!(n(&h, ""), 6);
    assert_eq!(n(&h, "category:food"), 3);
    assert_eq!(n(&h, "category:food date:this-month"), 2);
    assert_eq!(n(&h, "date:last-month"), 2);
    assert_eq!(n(&h, "payee:swiggy or payee:shell"), 2);
    assert_eq!(n(&h, "account:card"), 2);
    assert_eq!(n(&h, "type:income"), 1);
    assert_eq!(n(&h, "amount:>1000"), 3);
    assert_eq!(n(&h, "amount:300..450.50"), 2);
    assert_eq!(n(&h, "lunch"), 1); // memo text
    assert_eq!(n(&h, "-category:food"), 3);
    assert_eq!(n(&h, "date:2026-09"), 1);
    assert_eq!(n(&h, "date:last-3m"), 6); // Aug 17 .. Nov 15 -> excludes nothing here? Sept is included
    assert_eq!(n(&h, "limit:2"), 2);
}

#[test]
fn query_grouping_and_aggregates() {
    let h = seed();
    let g = h.ok(
        "query",
        json!({"query": "type:expense date:2026-11 group:category sort:-sum"}),
    );
    let rows = g.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["category"], "food.delivery");
    assert_eq!(rows[0]["sum_minor"], -45050);
    assert_eq!(rows[0]["currency"], "INR");
    let m = h.ok("query", json!({"query": "type:expense group:month"}));
    let months: Vec<&str> = m
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["month"].as_str().unwrap())
        .collect();
    assert_eq!(months, vec!["2026-09", "2026-10", "2026-11"]);
    assert_eq!(m[2]["sum_minor"], -75050);
    assert_eq!(m[2]["count"], 2);
    let a = h.ok(
        "query",
        json!({"query": "type:expense group:account agg:sum,count,avg,min,max sort:key"}),
    );
    assert_eq!(a[0]["account"], "Bank");
    assert_eq!(a[0]["avg_minor"], -45017); // -45016.67 rounds to nearest
    assert_eq!(a[0]["min_minor"], -60000);
    assert_eq!(a[0]["max_minor"], -30000);
}

#[test]
fn transfers_are_excluded_from_grouped_results_by_default() {
    let h = seed();
    h.ok(
        "txn.transfer",
        json!({"from": "Bank", "to": "Card", "amount": "500", "date": "2026-11-03"}),
    );
    assert_eq!(n(&h, "date:2026-11-03"), 0);
    assert_eq!(n(&h, "date:2026-11-03 transfers"), 2);
    let all = h.ok("query", json!({"query": "type:transfer transfers"}));
    assert_eq!(all.as_array().unwrap().len(), 2);
}

#[test]
fn explain_describes_the_parse() {
    let h = seed();
    let e = h.ok(
        "query",
        json!({"query": "category:food date:2026-10 group:month limit:3", "explain": true}),
    );
    assert_eq!(e["group_by"][0], "month");
    assert_eq!(e["limit"], 3);
    assert_eq!(e["date_bounds"]["from"], "2026-10-01");
    assert_eq!(e["match_any_of"][0][0]["field"], "category");
}

#[test]
fn bad_queries_are_usage_errors_with_hints() {
    let h = seed();
    for q in [
        "colour:red",
        "date:soon",
        "amount:>x",
        "group:fortnight",
        "\"unterminated",
    ] {
        let e = h.err("query", json!({"query": q}));
        assert!(
            matches!(e.kind.as_str(), "usage" | "validation"),
            "{q}: {e:?}"
        );
    }
    assert!(h
        .err("query", json!({"query": "colour:red"}))
        .hint
        .is_some());
}

#[test]
fn txn_list_accepts_query_and_flags_together() {
    let h = seed();
    let rows = h.ok("txn.list", json!({"query": "category:food", "min": "400"}));
    assert_eq!(rows.as_array().unwrap().len(), 2); // 450.50 and 1200
    let rows = h.ok(
        "txn.list",
        json!({"query": "category:food or category:transport", "account": ["Bank"]}),
    );
    assert_eq!(rows.as_array().unwrap().len(), 3);
}

#[test]
fn saved_views() {
    let h = seed();
    h.ok(
        "view.save",
        json!({"name": "food-month", "query": "category:food date:this-month group:payee"}),
    );
    let r = h.ok("view.run", json!({"name": "food-month"}));
    assert_eq!(r.as_array().unwrap().len(), 2);
    // run with narrowing
    let r = h.ok(
        "view.run",
        json!({"name": "food-month", "query": "payee:swiggy"}),
    );
    assert_eq!(r.as_array().unwrap().len(), 1);
    // relative dates resolve at run time
    let later = h
        .call_on("2026-10-25", "view.run", json!({"name": "food-month"}))
        .unwrap()
        .data;
    assert_eq!(later.as_array().unwrap().len(), 1); // October: BigBasket only
                                                    // update in place
    h.ok(
        "view.save",
        json!({"name": "FOOD-MONTH", "query": "category:food", "description": "all food"}),
    );
    assert_eq!(h.ok("view.list", json!({})).as_array().unwrap().len(), 1);
    assert_eq!(
        h.err("view.save", json!({"name": "bad", "query": "wat:x"}))
            .kind
            .as_str(),
        "usage"
    );
    assert_eq!(
        h.err("view.run", json!({"name": "nope"})).code,
        "view_not_found"
    );
    h.ok("view.rm", json!({"name": "food-month"}));
    assert_eq!(h.ok("view.list", json!({})).as_array().unwrap().len(), 0);
}

#[test]
fn fx_rates_and_conversion() {
    let h = H::on("2026-11-15");
    assert_eq!(
        h.err(
            "fx.convert",
            json!({"from": "USD", "to": "INR", "amount": "10"})
        )
        .code,
        "fx_rate_missing"
    );
    h.ok(
        "fx.set",
        json!({"from": "USD", "to": "INR", "rate": "83.25", "date": "2026-11-01"}),
    );
    let c = h.ok(
        "fx.convert",
        json!({"from": "USD", "to": "INR", "amount": "10"}),
    );
    assert_eq!(c["converted_minor"], 83250);
    // inverse works
    let c = h.ok(
        "fx.convert",
        json!({"from": "INR", "to": "USD", "amount": "8325"}),
    );
    assert_eq!(c["converted_minor"], 10000);
    // as-of lookups use the latest rate on or before the date
    h.ok(
        "fx.set",
        json!({"from": "USD", "to": "INR", "rate": "84", "date": "2026-11-10"}),
    );
    let c = h.ok(
        "fx.convert",
        json!({"from": "USD", "to": "INR", "amount": "1", "date": "2026-11-05"}),
    );
    assert_eq!(c["converted_minor"], 8325);
    let c = h.ok(
        "fx.convert",
        json!({"from": "USD", "to": "INR", "amount": "1", "date": "2026-11-12"}),
    );
    assert_eq!(c["converted_minor"], 8400);
    assert_eq!(
        h.err(
            "fx.convert",
            json!({"from": "USD", "to": "INR", "amount": "1", "date": "2026-10-01"})
        )
        .code,
        "fx_rate_missing"
    );
    assert_eq!(
        h.err("fx.set", json!({"from": "USD", "to": "INR", "rate": "0"}))
            .code,
        "invalid_rate"
    );
    assert_eq!(
        h.err("fx.set", json!({"from": "USD", "to": "USD", "rate": "1"}))
            .code,
        "invalid_pair"
    );
    assert_eq!(
        h.err(
            "fx.set",
            json!({"from": "USD", "to": "INR", "rate": "1.123456789"})
        )
        .code,
        "rate_precision"
    );
    assert_eq!(h.ok("fx.list", json!({})).as_array().unwrap().len(), 2);
    // converting query aggregates uses the dated rate
    h.ok(
        "acct.add",
        json!({"name": "Dollar", "kind": "checking", "currency": "USD"}),
    );
    h.ok("txn.add", json!({"account": "Dollar", "amount": "-10", "date": "2026-11-12", "category": "food.dining"}));
    let q = h.ok(
        "query",
        json!({"query": "type:expense group:category convert:INR"}),
    );
    assert_eq!(q[0]["sum_minor"], -84000);
    assert_eq!(q[0]["currency"], "INR");
    let nofx = h.ok("fx.list", json!({}));
    h.ok("fx.rm", json!({"id": nofx[0]["id"]}));
    h.ok("fx.rm", json!({"id": nofx[1]["id"]}));
    assert_eq!(
        h.err(
            "query",
            json!({"query": "type:expense group:category convert:INR"})
        )
        .code,
        "fx_rate_missing"
    );
}
