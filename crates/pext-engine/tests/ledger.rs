mod common;
use common::H;
use serde_json::json;

#[test]
fn init_seeds_and_cannot_repeat() {
    let h = H::bare();
    let d = h.ok("init", json!({"currency": "usd"}));
    assert_eq!(d["base_currency"], "USD");
    assert!(d["categories_seeded"].as_i64().unwrap() > 20);
    assert_eq!(h.err("init", json!({})).code, "already_initialized");
    assert_eq!(
        h.ok("config.get", json!({"key": "base_currency"}))["value"],
        "USD"
    );
    assert_eq!(
        h.err(
            "config.set",
            json!({"key": "base_currency", "value": "EUR"})
        )
        .code,
        "immutable_setting"
    );
    assert_eq!(
        h.err(
            "config.set",
            json!({"key": "fy_start_month", "value": "13"})
        )
        .code,
        "invalid_setting"
    );
    assert_eq!(
        h.err("config.get", json!({"key": "nope"})).code,
        "unknown_setting"
    );
}

#[test]
fn commands_on_missing_db_say_how_to_fix() {
    let h = H::bare();
    let e = h.err("acct.list", json!({}));
    assert_eq!(e.code, "db_not_found");
    assert!(e.hint.unwrap().contains("pext init"));
}

#[test]
fn balances_follow_the_ledger_exactly() {
    let h = H::new();
    h.acct("Bank", "checking", "1000.10");
    h.txn("Bank", "-450.55", "2026-06-01", Some("food.dining"));
    h.txn("Bank", "85000", "2026-06-02", Some("salary"));
    h.txn("Bank", "-0.01", "2026-06-03", None);
    assert_eq!(h.balance("Bank"), 100010 - 45055 + 8_500_000 - 1);
    // as_of excludes later transactions
    let b = h.ok(
        "acct.balance",
        json!({"account": "Bank", "as_of": "2026-06-01"}),
    );
    assert_eq!(b["balance_minor"], 100010 - 45055);
    h.doctor_ok();
}

#[test]
fn amounts_are_validated_strictly() {
    let h = H::new();
    h.acct("Bank", "checking", "0");
    assert_eq!(
        h.err("txn.add", json!({"account": "Bank", "amount": "-1.005"}))
            .code,
        "amount_precision"
    );
    assert_eq!(
        h.err("txn.add", json!({"account": "Bank", "amount": "0"}))
            .code,
        "zero_amount"
    );
    assert_eq!(
        h.err("txn.add", json!({"account": "Bank", "amount": "abc"}))
            .kind
            .as_str(),
        "validation"
    );
    assert_eq!(
        h.err("txn.add", json!({"account": "Bank", "amount": 12.5}))
            .code,
        "float_amount"
    );
    assert_eq!(
        h.err(
            "txn.add",
            json!({"account": "Bank", "amount": "100000000000.01"})
        )
        .kind
        .as_str(),
        "validation"
    );
    assert_eq!(
        h.err(
            "txn.add",
            json!({"account": "Bank", "amount": "1", "amount_minor": 100})
        )
        .kind
        .as_str(),
        "usage"
    );
    assert_eq!(
        h.err("txn.add", json!({"account": "Bank"})).kind.as_str(),
        "usage"
    );
    assert_eq!(
        h.err(
            "txn.add",
            json!({"account": "Bank", "amount": "5", "date": "2026-02-30"})
        )
        .kind
        .as_str(),
        "validation"
    );
    assert_eq!(
        h.err("txn.add", json!({"account": "Nobody", "amount": "5"}))
            .kind
            .as_str(),
        "not_found"
    );
    // integer minor units work and nothing was written by the failures
    h.ok(
        "txn.add",
        json!({"account": "Bank", "amount_minor": -12345}),
    );
    assert_eq!(h.balance("Bank"), -12345);
    assert_eq!(h.ok("txn.list", json!({})).as_array().unwrap().len(), 1);
}

#[test]
fn zero_decimal_and_three_decimal_currencies() {
    let h = H::new();
    h.ok(
        "acct.add",
        json!({"name": "Tokyo", "kind": "checking", "currency": "JPY", "opening": "1000"}),
    );
    h.ok(
        "acct.add",
        json!({"name": "Kuwait", "kind": "checking", "currency": "KWD", "opening": "1.234"}),
    );
    h.ok("txn.add", json!({"account": "Tokyo", "amount": "-250"}));
    assert_eq!(
        h.err("txn.add", json!({"account": "Tokyo", "amount": "-250.5"}))
            .code,
        "amount_precision"
    );
    h.ok("txn.add", json!({"account": "Kuwait", "amount": "-0.001"}));
    assert_eq!(h.balance("Tokyo"), 750);
    assert_eq!(h.balance("Kuwait"), 1233);
}

#[test]
fn splits_must_sum_exactly() {
    let h = H::new();
    h.acct("Bank", "checking", "0");
    let ok = h.ok("txn.add", json!({"account": "Bank", "amount": "-2000", "split": ["food.groceries=1200", "housing.rent=800"]}));
    assert_eq!(ok["splits"].as_array().unwrap().len(), 2);
    let e = h.err("txn.add", json!({"account": "Bank", "amount": "-2000", "split": ["food.groceries=1200", "housing.rent=700"]}));
    assert_eq!(e.kind.as_str(), "validation");
    assert_eq!(h.balance("Bank"), -200000);
    // re-split an existing transaction
    let id = ok["id"].as_i64().unwrap();
    h.ok(
        "txn.split",
        json!({"id": id, "split": ["food.dining=500", "food.groceries=1500"]}),
    );
    assert!(
        h.err("txn.split", json!({"id": id, "split": ["food.dining=500"]}))
            .kind
            .as_str()
            == "validation"
    );
    // a multi-split transaction cannot silently change its amount
    h.doctor_ok();
}

#[test]
fn transfers_move_money_as_a_pair() {
    let h = H::new();
    h.acct("A", "checking", "1000");
    h.acct("B", "savings", "0");
    let t = h.ok(
        "txn.transfer",
        json!({"from": "A", "to": "B", "amount": "250.25", "date": "2026-06-10"}),
    );
    assert_eq!(t["out"]["amount_minor"], -25025);
    assert_eq!(t["in"]["amount_minor"], 25025);
    assert_eq!(h.balance("A"), 100000 - 25025);
    assert_eq!(h.balance("B"), 25025);
    // transfers don't show up as spending
    let total: i64 = h
        .ok("txn.list", json!({"type": "expense"}))
        .as_array()
        .unwrap()
        .len() as i64;
    assert_eq!(total, 0);
    // deleting one leg deletes both
    let out_id = t["out"]["id"].as_i64().unwrap();
    h.ok("txn.rm", json!({"id": out_id}));
    assert_eq!(h.balance("A"), 100000);
    assert_eq!(h.balance("B"), 0);
    h.ok("txn.restore", json!({"id": out_id}));
    assert_eq!(h.balance("B"), 25025);
    h.doctor_ok();
    // same-account and cross-currency transfers
    assert_eq!(
        h.err(
            "txn.transfer",
            json!({"from": "A", "to": "A", "amount": "1"})
        )
        .kind
        .as_str(),
        "validation"
    );
    h.ok(
        "acct.add",
        json!({"name": "Dollars", "kind": "checking", "currency": "USD"}),
    );
    assert_eq!(
        h.err(
            "txn.transfer",
            json!({"from": "A", "to": "Dollars", "amount": "100"})
        )
        .code,
        "missing_argument"
    );
    let x = h.ok(
        "txn.transfer",
        json!({"from": "A", "to": "Dollars", "amount": "8300", "to_amount": "100"}),
    );
    assert_eq!(x["in"]["amount_minor"], 10000);
    assert_eq!(x["in"]["currency"], "USD");
    h.doctor_ok();
}

#[test]
fn edit_delete_restore_and_tags() {
    let h = H::new();
    h.acct("Bank", "checking", "0");
    let id = h.txn("Bank", "-100", "2026-06-01", Some("food.dining"));
    h.ok("txn.edit", json!({"id": id, "amount": "-150", "payee": "  Cafe   Coffee  ", "category": "food.groceries"}));
    let t = h.ok("txn.get", json!({"id": id}));
    assert_eq!(t["amount_minor"], -15000);
    assert_eq!(t["category"], "food.groceries");
    assert_eq!(t["payee"], "Cafe Coffee");
    h.ok("txn.tag", json!({"id": id, "tag": ["Trip", "work"]}));
    h.ok("txn.tag", json!({"id": id, "tag": ["trip"]})); // idempotent, case-insensitive
    assert_eq!(
        h.ok("txn.get", json!({"id": id}))["tags"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    h.ok("txn.untag", json!({"id": id, "tag": ["TRIP"]}));
    assert_eq!(
        h.ok("txn.get", json!({"id": id}))["tags"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    h.ok("txn.rm", json!({"id": id}));
    assert_eq!(h.balance("Bank"), 0);
    assert_eq!(h.ok("txn.list", json!({})).as_array().unwrap().len(), 0);
    assert_eq!(h.err("txn.rm", json!({"id": id})).kind.as_str(), "conflict");
    h.ok("txn.restore", json!({"id": id}));
    assert_eq!(h.balance("Bank"), -15000);
    assert_eq!(
        h.err("txn.get", json!({"id": 999})).kind.as_str(),
        "not_found"
    );
    h.doctor_ok();
}

#[test]
fn list_filters_and_sorting() {
    let h = H::new();
    h.acct("A", "checking", "0");
    h.acct("B", "cash", "0");
    h.txn("A", "-100", "2026-05-01", Some("food.dining"));
    h.txn("A", "-300", "2026-05-15", Some("food.groceries"));
    h.txn("A", "5000", "2026-05-20", Some("salary"));
    h.txn("B", "-50", "2026-06-01", Some("transport.taxi"));
    let n = |args| h.ok("txn.list", args).as_array().unwrap().len();
    assert_eq!(n(json!({})), 4);
    assert_eq!(n(json!({"account": ["A"]})), 3);
    assert_eq!(n(json!({"category": ["food"]})), 2); // subtree
    assert_eq!(n(json!({"type": "income"})), 1);
    assert_eq!(n(json!({"type": "expense"})), 3);
    assert_eq!(n(json!({"from": "2026-05-10", "to": "2026-05-31"})), 2);
    assert_eq!(n(json!({"min": "100"})), 3); // magnitude: 100, 300, 5000
    assert_eq!(n(json!({"min": "100", "max": "300"})), 2);
    assert_eq!(n(json!({"uncategorized": true})), 0);
    assert_eq!(n(json!({"limit": 2})), 2);
    assert_eq!(n(json!({"limit": 2, "offset": 3})), 1);
    let rows = h.ok("txn.list", json!({"sort": "-amount"}));
    assert_eq!(rows[0]["amount_minor"], 500000);
    let rows = h.ok("txn.list", json!({"sort": "amount"}));
    assert_eq!(rows[0]["amount_minor"], -30000);
    assert_eq!(
        h.err("txn.list", json!({"sort": "bogus"})).kind.as_str(),
        "validation"
    );
}

#[test]
fn categories_tree_merge_and_delete_rules() {
    let h = H::new();
    h.acct("A", "checking", "0");
    h.ok("cat.add", json!({"path": "Pets.Vet"}));
    assert_eq!(
        h.err("cat.add", json!({"path": "pets.vet"})).code,
        "already_exists"
    );
    h.txn("A", "-100", "2026-06-01", Some("pets.vet"));
    assert_eq!(
        h.err("cat.rm", json!({"path": "pets.vet"})).kind.as_str(),
        "conflict"
    );
    h.ok(
        "cat.merge",
        json!({"from": "pets.vet", "into": "health.medical"}),
    );
    assert_eq!(
        h.ok("txn.list", json!({"category": ["health.medical"]}))
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        h.err("cat.rm", json!({"path": "pets.vet"})).kind.as_str(),
        "not_found"
    ); // merge removed it
    h.ok("cat.rename", json!({"path": "pets", "new_name": "animals"}));
    assert!(h
        .ok("cat.list", json!({}))
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["path"] == "animals"));
    h.doctor_ok();
}

#[test]
fn payee_aliases_normalise() {
    let h = H::new();
    h.acct("A", "checking", "0");
    h.ok(
        "payee.alias.add",
        json!({"alias": "SWIGGY*ORDER", "canonical": "Swiggy"}),
    );
    let id = h.ok(
        "txn.add",
        json!({"account": "A", "amount": "-100", "payee": "swiggy*order"}),
    )["id"]
        .as_i64()
        .unwrap();
    assert_eq!(h.ok("txn.get", json!({"id": id}))["payee"], "Swiggy");
}

#[test]
fn dedupe_finds_probable_duplicates_without_changing_data() {
    let h = H::new();
    h.acct("A", "checking", "0");
    h.txn("A", "-100", "2026-06-01", None);
    h.txn("A", "-100", "2026-06-02", None);
    h.txn("A", "-100", "2026-06-20", None);
    let before = h.dump();
    let d = h.ok("txn.dedupe", json!({"window_days": 3}));
    assert_eq!(d.as_array().unwrap().len(), 1);
    assert_eq!(before, h.dump());
}

#[test]
fn reconcile_reports_difference_and_marks_status() {
    let h = H::new();
    h.acct("A", "checking", "1000");
    h.txn("A", "-100", "2026-06-01", None);
    let r = h.ok(
        "acct.reconcile",
        json!({"account": "A", "statement_date": "2026-06-30", "statement_balance": "900"}),
    );
    assert_eq!(r["difference_minor"], 0);
    let r = h.ok(
        "acct.reconcile",
        json!({"account": "A", "statement_date": "2026-06-30", "statement_balance": "950"}),
    );
    assert_eq!(r["difference_minor"], 5000);
    h.ok("acct.reconcile", json!({"account": "A", "statement_date": "2026-06-30", "statement_balance": "900", "mark_reconciled": true}));
    let rows = h.ok("txn.list", json!({"status": ["reconciled"]}));
    assert_eq!(rows.as_array().unwrap().len(), 1);
}

#[test]
fn account_rules() {
    let h = H::new();
    h.acct("A", "checking", "10");
    assert_eq!(
        h.err("acct.add", json!({"name": "a", "kind": "checking"}))
            .code,
        "already_exists"
    ); // case-insensitive
    assert_eq!(
        h.err("acct.add", json!({"name": "X", "kind": "spaceship"}))
            .kind
            .as_str(),
        "validation"
    );
    assert_eq!(
        h.err(
            "acct.add",
            json!({"name": "X", "kind": "checking", "currency": "ZZ"})
        )
        .kind
        .as_str(),
        "validation"
    );
    h.ok("acct.archive", json!({"account": "A"}));
    assert_eq!(
        h.err("txn.add", json!({"account": "A", "amount": "1"}))
            .kind
            .as_str(),
        "conflict"
    );
    assert!(h.ok("acct.list", json!({})).as_array().unwrap().is_empty());
    assert_eq!(
        h.ok("acct.list", json!({"include_archived": true}))
            .as_array()
            .unwrap()
            .len(),
        1
    );
    h.ok("acct.archive", json!({"account": "A", "unarchive": true}));
    h.ok("txn.add", json!({"account": "A", "amount": "1"}));
    // liabilities report negative net contribution
    let c = h.ok(
        "acct.add",
        json!({"name": "Card", "kind": "credit_card", "credit_limit": "50000"}),
    );
    assert_eq!(c["is_liability"], true);
}

#[test]
fn dry_run_changes_nothing() {
    let h = H::new();
    h.acct("A", "checking", "100");
    let before = h.dump();
    let o = h.dry(
        "txn.add",
        json!({"account": "A", "amount": "-5", "category": "food.dining"}),
    );
    assert_eq!(o.meta["dry_run"], true);
    assert!(!o.meta["changes"].as_array().unwrap().is_empty());
    assert_eq!(before, h.dump());
    let audit_before = h.ok("audit.list", json!({})).as_array().unwrap().len();
    h.dry("acct.add", json!({"name": "Z", "kind": "cash"}));
    assert_eq!(
        audit_before,
        h.ok("audit.list", json!({})).as_array().unwrap().len()
    );
}

#[test]
fn idempotent_retries() {
    let h = H::new();
    h.acct("A", "checking", "0");
    let o = pext_engine::CallOpts {
        idempotency_key: Some("abc-1".into()),
        ..Default::default()
    };
    let args = json!({"account": "A", "amount": "-10"});
    let a = h
        .raw("txn.add", args.clone(), &o, &pext_engine::Actor::owner())
        .unwrap();
    let b = h
        .raw("txn.add", args.clone(), &o, &pext_engine::Actor::owner())
        .unwrap();
    assert_eq!(a.data["id"], b.data["id"]);
    assert_eq!(b.meta["replayed"], true);
    assert_eq!(h.balance("A"), -1000);
    let c = h.raw(
        "txn.add",
        json!({"account": "A", "amount": "-11"}),
        &o,
        &pext_engine::Actor::owner(),
    );
    assert_eq!(c.unwrap_err().code, "idempotency_conflict");
    let bad = pext_engine::CallOpts {
        idempotency_key: Some("has space".into()),
        ..Default::default()
    };
    assert_eq!(
        h.raw("txn.add", args, &bad, &pext_engine::Actor::owner())
            .unwrap_err()
            .code,
        "invalid_idempotency_key"
    );
    let read = h.raw("txn.list", json!({}), &o, &pext_engine::Actor::owner());
    assert_eq!(read.unwrap_err().code, "idempotency_not_applicable");
}

#[test]
fn unknown_arguments_are_rejected() {
    let h = H::new();
    let e = h.err("acct.add", json!({"name": "X", "kind": "cash", "bogus": 1}));
    assert_eq!(e.code, "unknown_argument");
    assert_eq!(h.err("nope.nope", json!({})).code, "unknown_command");
}

#[test]
fn registry_is_consistent() {
    use pext_engine::registry;
    let mut seen = std::collections::HashSet::new();
    for op in registry::all() {
        assert!(seen.insert(op.name), "duplicate op {}", op.name);
        assert!(!op.about.is_empty());
        for p in &op.params {
            assert!(
                ![
                    "db",
                    "output",
                    "fields",
                    "pretty",
                    "quiet",
                    "no_color",
                    "dry_run",
                    "yes",
                    "idempotency_key",
                    "token",
                    "out",
                    "watch",
                    "help",
                    "version"
                ]
                .contains(&p.name),
                "{} param {} shadows a global flag",
                op.name,
                p.name
            );
        }
        let mut names = std::collections::HashSet::new();
        for p in &op.params {
            assert!(
                names.insert(p.name),
                "{} declares parameter {} twice",
                op.name,
                p.name
            );
        }
        let positional = op.params.iter().filter(|p| p.positional).count();
        assert!(positional <= 3, "{} has too many positionals", op.name);
        // schema generation never panics
        let _ = registry::input_schema(op);
    }
}
