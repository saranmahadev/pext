mod common;
use common::H;
use serde_json::json;
use std::io::Write;

fn cat_of(h: &H, id: i64) -> serde_json::Value {
    h.ok("txn.get", json!({"id": id}))["category"].clone()
}

#[test]
fn rules_categorise_new_transactions() {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "0");
    h.ok(
        "rule.add",
        json!({"name": "swiggy", "query": "payee:swiggy", "category": "food.delivery"}),
    );
    let id = h.ok(
        "txn.add",
        json!({"account": "Bank", "amount": "-100", "payee": "SWIGGY ORDER 123"}),
    )["id"]
        .as_i64()
        .unwrap();
    assert_eq!(cat_of(&h, id), "food.delivery");
    // explicit category wins over rules
    let id = h.ok(
        "txn.add",
        json!({"account": "Bank", "amount": "-100", "payee": "swiggy", "category": "food.dining"}),
    )["id"]
        .as_i64()
        .unwrap();
    assert_eq!(cat_of(&h, id), "food.dining");
    // no match -> uncategorised
    let id = h.ok(
        "txn.add",
        json!({"account": "Bank", "amount": "-100", "payee": "Other"}),
    )["id"]
        .as_i64()
        .unwrap();
    assert!(cat_of(&h, id).is_null());
}

#[test]
fn priority_tags_and_payee_rename() {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "0");
    h.ok("rule.add", json!({"name": "generic", "query": "payee:amazon", "category": "shopping.household", "priority": 200}));
    h.ok("rule.add", json!({"name": "kindle", "query": "payee:amazon memo:kindle", "category": "entertainment", "priority": 10, "tag": ["books"]}));
    h.ok("rule.add", json!({"name": "tidy", "query": "payee:amazon", "set_payee": "Amazon", "tag": ["online", "Books"], "priority": 50}));
    let t = h.ok(
        "txn.add",
        json!({"account": "Bank", "amount": "-299", "payee": "AMAZON PAY", "memo": "Kindle book"}),
    );
    assert_eq!(t["category"], "entertainment"); // lowest priority number wins
    assert_eq!(t["payee"], "Amazon");
    let mut tags: Vec<String> = t["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    tags.sort();
    assert_eq!(tags, vec!["books", "online"]); // union, normalised, de-duplicated
    let t = h.ok(
        "txn.add",
        json!({"account": "Bank", "amount": "-50", "payee": "amazon", "memo": "cable"}),
    );
    assert_eq!(t["category"], "shopping.household");
}

#[test]
fn rule_validation() {
    let h = H::on("2026-11-15");
    assert_eq!(
        h.err(
            "rule.add",
            json!({"name": "a", "query": "", "category": "food"})
        )
        .code,
        "empty_rule_query"
    );
    assert_eq!(
        h.err(
            "rule.add",
            json!({"name": "a", "query": "payee:x group:month", "category": "food"})
        )
        .code,
        "invalid_rule_query"
    );
    assert_eq!(
        h.err("rule.add", json!({"name": "a", "query": "payee:x"}))
            .code,
        "missing_action"
    );
    assert_eq!(
        h.err(
            "rule.add",
            json!({"name": "a", "query": "payee:x", "category": "nope"})
        )
        .code,
        "category_not_found"
    );
    assert_eq!(
        h.err(
            "rule.add",
            json!({"name": "a", "query": "wat:x", "category": "food"})
        )
        .kind
        .as_str(),
        "usage"
    );
    h.ok(
        "rule.add",
        json!({"name": "a", "query": "payee:x", "category": "food"}),
    );
    assert_eq!(
        h.err(
            "rule.add",
            json!({"name": "A", "query": "payee:y", "category": "food"})
        )
        .code,
        "already_exists"
    );
    assert_eq!(
        h.err("rule.edit", json!({"rule": "a"})).code,
        "nothing_to_change"
    );
    assert_eq!(
        h.err("rule.edit", json!({"rule": "a", "category": "none"}))
            .code,
        "missing_action"
    );
    assert_eq!(
        h.err("rule.get", json!({"rule": "zzz"})).code,
        "rule_not_found"
    );
}

#[test]
fn enable_disable_and_edit() {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "0");
    h.ok(
        "rule.add",
        json!({"name": "r", "query": "payee:uber", "category": "transport.taxi"}),
    );
    h.ok("rule.edit", json!({"rule": "r", "disable": true}));
    let t = h.ok(
        "txn.add",
        json!({"account": "Bank", "amount": "-10", "payee": "uber"}),
    );
    assert!(t["category"].is_null());
    h.ok(
        "rule.edit",
        json!({"rule": "r", "enable": true, "category": "transport.public", "query": "payee:ola"}),
    );
    let t = h.ok(
        "txn.add",
        json!({"account": "Bank", "amount": "-10", "payee": "ola cabs"}),
    );
    assert_eq!(t["category"], "transport.public");
    assert_eq!(
        h.err(
            "rule.edit",
            json!({"rule": "r", "enable": true, "disable": true})
        )
        .code,
        "conflicting_arguments"
    );
    h.ok("rule.rm", json!({"rule": "r"}));
    assert!(h.ok("rule.list", json!({})).as_array().unwrap().is_empty());
}

#[test]
fn rules_apply_to_imports() {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "0");
    h.ok(
        "rule.add",
        json!({"name": "swiggy", "query": "payee:swiggy", "category": "food.delivery"}),
    );
    let p = h.dir.path().join("s.csv");
    std::fs::File::create(&p)
        .unwrap()
        .write_all(b"date,desc,amount\n2026-11-01,SWIGGY 1,-100\n2026-11-02,Misc,-5\n")
        .unwrap();
    h.ok("import.csv", json!({"account": "Bank", "file": p.to_string_lossy(), "date_col": "date", "payee_col": "desc", "amount_col": "amount"}));
    let rows = h.ok("txn.list", json!({"sort": "date"}));
    assert_eq!(rows[0]["category"], "food.delivery");
    assert!(rows[1]["category"].is_null());
}

#[test]
fn apply_to_history_with_dry_run_overwrite_and_undo() {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "0");
    let a = h.txn("Bank", "-100", "2026-11-01", None);
    let b = h.txn("Bank", "-200", "2026-11-02", Some("food.dining"));
    let c = h.txn("Bank", "-300", "2026-11-03", None);
    for id in [a, b, c] {
        h.ok("txn.edit", json!({"id": id, "payee": "Swiggy"}));
    }
    h.ok("rule.add", json!({"name": "swiggy", "query": "payee:swiggy", "category": "food.delivery", "tag": ["app"]}));
    let before = h.dump();
    let d = h.dry("rule.apply", json!({}));
    assert_eq!(d.data["recategorized"], 2);
    assert_eq!(d.data["tags_added"], 3);
    assert_eq!(before, h.dump());

    let r = h.ok("rule.apply", json!({}));
    assert_eq!(r["recategorized"], 2);
    assert_eq!(cat_of(&h, a), "food.delivery");
    assert_eq!(cat_of(&h, b), "food.dining"); // not overwritten
                                              // idempotent
    assert_eq!(h.ok("rule.apply", json!({}))["recategorized"], 0);
    // overwrite changes the categorised one too
    assert_eq!(
        h.ok("rule.apply", json!({"overwrite": true}))["recategorized"],
        1
    );
    assert_eq!(cat_of(&h, b), "food.delivery");
    // undo the overwrite
    h.ok("undo", json!({}));
    assert_eq!(cat_of(&h, b), "food.dining");
    // scope with a query
    h.ok(
        "rule.edit",
        json!({"rule": "swiggy", "category": "food.dining"}),
    );
    let r = h.ok(
        "rule.apply",
        json!({"overwrite": true, "query": "amount:>250"}),
    );
    assert_eq!(r["examined"], 1);
    assert_eq!(cat_of(&h, c), "food.dining");
    assert_eq!(cat_of(&h, a), "food.delivery");
    h.doctor_ok();
}

#[test]
fn apply_skips_splits_and_transfers() {
    let h = H::on("2026-11-15");
    h.acct("A", "checking", "0");
    h.acct("B", "checking", "0");
    let s = h.ok("txn.add", json!({"account": "A", "amount": "-100", "payee": "Swiggy", "split": ["food.dining=60", "food.groceries=40"]}))["id"].as_i64().unwrap();
    h.ok(
        "txn.transfer",
        json!({"from": "A", "to": "B", "amount": "10"}),
    );
    h.ok(
        "rule.add",
        json!({"name": "all", "query": "amount:>0", "category": "other"}),
    );
    let r = h.ok("rule.apply", json!({"overwrite": true}));
    assert_eq!(r["examined"], 0);
    assert_eq!(
        h.ok("txn.get", json!({"id": s}))["splits"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn test_preview_and_suggestions() {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "0");
    for d in ["01", "02", "03", "04"] {
        h.ok("txn.add", json!({"account": "Bank", "amount": "-100", "payee": "Zomato", "category": "food.delivery", "date": format!("2026-11-{d}")}));
    }
    h.ok(
        "txn.add",
        json!({"account": "Bank", "amount": "-100", "payee": "Zomato", "date": "2026-11-10"}),
    );
    h.ok("txn.add", json!({"account": "Bank", "amount": "-5", "payee": "Rare", "category": "other", "date": "2026-11-10"}));
    let t = h.ok("rule.test", json!({"query": "payee:zomato"}));
    assert_eq!(t["matches"], 5);
    assert_eq!(t["uncategorized"], 1);
    let s = h.ok("rule.suggest", json!({}));
    assert_eq!(s.as_array().unwrap().len(), 1);
    assert_eq!(s[0]["query"], "payee:zomato");
    assert_eq!(s[0]["category"], "food.delivery");
    assert_eq!(s[0]["uncategorized_matching"], 1);
    // once a rule exists the suggestion disappears
    h.ok(
        "rule.add",
        json!({"name": "z", "query": "payee:zomato", "category": "food.delivery"}),
    );
    assert!(h
        .ok("rule.suggest", json!({}))
        .as_array()
        .unwrap()
        .is_empty());
    let by_rule = h.ok("rule.test", json!({"rule": "z"}));
    assert_eq!(by_rule["matches"], 5);
    assert_eq!(h.err("rule.test", json!({})).code, "conflicting_arguments");
}

#[test]
fn category_changes_flow_into_rules() {
    let h = H::on("2026-11-15");
    h.ok(
        "rule.add",
        json!({"name": "r", "query": "payee:x", "category": "food.dining"}),
    );
    assert_eq!(
        h.err("cat.rm", json!({"path": "food.dining"})).code,
        "category_in_use"
    );
    h.ok(
        "cat.merge",
        json!({"from": "food.dining", "into": "food.groceries"}),
    );
    assert_eq!(
        h.ok("rule.get", json!({"rule": "r"}))["set_category"],
        "food.groceries"
    );
}
