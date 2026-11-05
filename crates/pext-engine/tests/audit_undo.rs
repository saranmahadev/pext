mod common;
use common::H;
use pext_engine::{Actor, CallOpts, Role};
use serde_json::json;

#[test]
fn undo_restores_exact_prior_state_for_every_write_op() {
    let h = H::new();
    h.acct("A", "checking", "5000");
    h.acct("B", "savings", "0");
    let t1 = h.txn("A", "-100", "2026-06-01", Some("food.dining"));
    let t2 = h.txn("A", "-2000", "2026-06-02", None);
    h.ok("txn.tag", json!({"id": t1, "tag": ["x"]}));

    let ops: Vec<(&str, serde_json::Value)> = vec![
        (
            "acct.add",
            json!({"name": "C", "kind": "cash", "opening": "10"}),
        ),
        ("acct.edit", json!({"account": "A", "institution": "HDFC"})),
        ("acct.archive", json!({"account": "C"})),
        ("cat.add", json!({"path": "pets.vet"})),
        (
            "cat.rename",
            json!({"path": "pets.vet", "new_name": "clinic"}),
        ),
        (
            "cat.merge",
            json!({"from": "food.delivery", "into": "food.groceries"}),
        ),
        (
            "payee.alias.add",
            json!({"alias": "foo corp", "canonical": "Foo"}),
        ),
        (
            "txn.add",
            json!({"account": "A", "amount": "-7.50", "category": "food.dining", "tag": ["a", "b"]}),
        ),
        (
            "txn.edit",
            json!({"id": t1, "amount": "-120", "payee": "Z", "category": "food.groceries"}),
        ),
        ("txn.rm", json!({"id": t1})),
        (
            "txn.split",
            json!({"id": t2, "split": ["food.dining=500", "food.groceries=1500"]}),
        ),
        ("txn.tag", json!({"id": t1, "tag": ["new"]})),
        ("txn.untag", json!({"id": t1, "tag": ["x"]})),
        (
            "txn.transfer",
            json!({"from": "A", "to": "B", "amount": "33.33"}),
        ),
        ("txn.add", json!({"account": "A", "amount": "-9"})),
        (
            "txn.recategorize",
            json!({"uncategorized": true, "set_category": "other"}),
        ),
    ];
    for (op, args) in ops {
        let before = h.dump();
        let r = h.ok(op, args.clone());
        let _ = r;
        let after = h.dump();
        assert_ne!(before, after, "{op} changed nothing");
        h.ok("undo", json!({}));
        assert_eq!(before, h.dump(), "undo of {op} did not restore state");
        h.doctor_ok();
        // redo by doing it again, so later ops build on real state
        h.ok(op, args);
    }
    h.doctor_ok();
}

#[test]
fn undo_twice_is_rejected_and_conflicts_are_detected() {
    let h = H::new();
    h.acct("A", "checking", "0");
    let t = h.txn("A", "-100", "2026-06-01", None);
    let edit = h
        .call("txn.edit", json!({"id": t, "payee": "one"}))
        .unwrap()
        .meta["audit_id"]
        .as_i64()
        .unwrap();
    h.ok("txn.edit", json!({"id": t, "payee": "two"}));
    // undoing the older edit would clobber the newer one
    let e = h.err("undo", json!({"id": edit}));
    assert_eq!(e.kind.as_str(), "conflict", "{}", e.message);
    assert_eq!(h.ok("txn.get", json!({"id": t}))["payee"], "two");
    // undo newest, then the older one works
    h.ok("undo", json!({}));
    h.ok("undo", json!({"id": edit}));
    assert_eq!(h.ok("txn.get", json!({"id": t}))["payee"], "");
    assert_eq!(h.err("undo", json!({"id": edit})).kind.as_str(), "conflict");
}

#[test]
fn undo_is_not_available_for_nothing_and_for_undo_entries() {
    let h = H::new();
    // only init is in the log; init may not be undone
    assert_eq!(h.err("undo", json!({})).code, "nothing_to_undo");
    assert_eq!(h.err("undo", json!({"id": 1})).code, "not_undoable");
    h.acct("A", "checking", "0");
    let u = h.call("undo", json!({})).unwrap().meta["audit_id"]
        .as_i64()
        .unwrap();
    assert!(h.call("undo", json!({"id": u})).is_err());
    assert_eq!(
        h.err("undo", json!({"id": 9999})).kind.as_str(),
        "not_found"
    );
}

#[test]
fn undo_frees_idempotency_key() {
    let h = H::new();
    h.acct("A", "checking", "0");
    let o = CallOpts {
        idempotency_key: Some("k".into()),
        ..Default::default()
    };
    h.raw(
        "txn.add",
        json!({"account": "A", "amount": "-1"}),
        &o,
        &Actor::owner(),
    )
    .unwrap();
    h.ok("undo", json!({}));
    let again = h
        .raw(
            "txn.add",
            json!({"account": "A", "amount": "-1"}),
            &o,
            &Actor::owner(),
        )
        .unwrap();
    assert!(again.meta.get("replayed").is_none());
    assert_eq!(h.balance("A"), -100);
}

#[test]
fn audit_log_records_everything_and_verifies() {
    let h = H::new();
    h.acct("A", "checking", "0");
    let t = h.txn("A", "-100", "2026-06-01", None);
    h.ok("txn.rm", json!({"id": t}));
    let list = h.ok("audit.list", json!({"op": "txn.rm"}));
    assert_eq!(list.as_array().unwrap().len(), 1);
    let id = list[0]["id"].as_i64().unwrap();
    let show = h.ok("audit.show", json!({"id": id}));
    assert_eq!(show["op"], "txn.rm");
    assert!(!show["changes"].as_array().unwrap().is_empty());
    let v = h.ok("audit.verify", json!({}));
    assert_eq!(v["ok"], true);
    assert!(v["entries"].as_i64().unwrap() >= 4);
}

#[test]
fn audit_log_is_append_only_and_tampering_is_detected() {
    let h = H::new();
    h.acct("A", "checking", "0");
    h.txn("A", "-100", "2026-06-01", None);
    let c = h.conn();
    assert!(c.execute("DELETE FROM audit", []).is_err());
    assert!(c
        .execute("UPDATE audit SET args='{}' WHERE id=2", [])
        .is_err());
    assert!(c
        .execute("UPDATE audit SET hash='x' WHERE id=2", [])
        .is_err());
    // an attacker who drops the trigger and edits history is caught by the hash chain
    c.execute_batch("DROP TRIGGER audit_only_undone_by")
        .unwrap();
    c.execute("UPDATE audit SET args='{\"tampered\":true}' WHERE id=2", [])
        .unwrap();
    drop(c);
    let v = h.ok("audit.verify", json!({}));
    assert_eq!(v["ok"], false);
    assert_eq!(v["first_bad_id"], 2);
    let d = h.ok("doctor", json!({}));
    assert_eq!(d["ok"], false);
}

#[test]
fn doctor_detects_ledger_corruption() {
    let h = H::new();
    h.acct("A", "checking", "0");
    let t = h.txn("A", "-100", "2026-06-01", None);
    h.conn()
        .execute("UPDATE splits SET amount_minor=-99 WHERE txn_id=?1", [t])
        .unwrap();
    let d = h.ok("doctor", json!({}));
    assert_eq!(d["ok"], false);
}

#[test]
fn roles_are_enforced() {
    let h = H::new();
    h.acct("A", "checking", "0");
    let read = Actor {
        name: "bot".into(),
        role: Role::Read,
        accounts: None,
        authenticated: true,
        token_id: None,
        redact: false,
    };
    let append = Actor {
        name: "bot".into(),
        role: Role::Append,
        accounts: None,
        authenticated: true,
        token_id: None,
        redact: false,
    };
    let o = CallOpts::default();
    assert!(h.raw("txn.list", json!({}), &o, &read).is_ok());
    assert_eq!(
        h.raw(
            "txn.add",
            json!({"account": "A", "amount": "-1"}),
            &o,
            &read
        )
        .unwrap_err()
        .kind
        .as_str(),
        "auth"
    );
    assert!(h
        .raw(
            "txn.add",
            json!({"account": "A", "amount": "-1"}),
            &o,
            &append
        )
        .is_ok());
    assert_eq!(
        h.raw("txn.rm", json!({"id": 1}), &o, &append)
            .unwrap_err()
            .kind
            .as_str(),
        "auth"
    );
    assert_eq!(
        h.raw("doctor", json!({}), &o, &append)
            .unwrap_err()
            .kind
            .as_str(),
        "auth"
    );
    // audit entries carry the actor
    let l = h.ok("audit.list", json!({"actor": "bot"}));
    assert_eq!(l.as_array().unwrap().len(), 1);
}

#[test]
fn account_scoped_actors_are_confined() {
    let h = H::new();
    let a = h.acct("A", "checking", "0");
    h.acct("B", "checking", "0");
    let scoped = Actor {
        name: "kid".into(),
        role: Role::Write,
        accounts: Some(vec![a]),
        authenticated: true,
        token_id: None,
        redact: false,
    };
    let o = CallOpts::default();
    assert!(h
        .raw(
            "txn.add",
            json!({"account": "A", "amount": "-1"}),
            &o,
            &scoped
        )
        .is_ok());
    assert_eq!(
        h.raw(
            "txn.add",
            json!({"account": "B", "amount": "-1"}),
            &o,
            &scoped
        )
        .unwrap_err()
        .code,
        "scope_denied"
    );
    let rows = h.raw("txn.list", json!({}), &o, &scoped).unwrap().data;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(
        h.raw("acct.list", json!({}), &o, &scoped)
            .unwrap()
            .data
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        h.raw("cat.add", json!({"path": "zz"}), &o, &scoped)
            .unwrap_err()
            .code,
        "scope_denied"
    );
}

#[test]
fn require_token_blocks_unauthenticated_calls() {
    let h = H::new();
    h.ok(
        "config.set",
        json!({"key": "require_token", "value": "true"}),
    );
    assert_eq!(h.err("acct.list", json!({})).code, "token_required");
    let auth = Actor {
        name: "t".into(),
        role: Role::Admin,
        accounts: None,
        authenticated: true,
        token_id: None,
        redact: false,
    };
    assert!(h
        .raw("acct.list", json!({}), &CallOpts::default(), &auth)
        .is_ok());
}
