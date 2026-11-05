mod common;
use common::H;
use pext_engine::{Actor, CallOpts};
use serde_json::{json, Value};

fn world() -> H {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "10000");
    h.acct("Savings", "savings", "50000");
    h.ok("txn.add", json!({"account": "Bank", "amount": "-250", "date": "2026-11-02", "payee": "Cafe Secret", "memo": "lunch with Dana", "category": "food.dining"}));
    h.ok("txn.add", json!({"account": "Savings", "amount": "1000", "date": "2026-11-03", "payee": "Employer", "category": "salary"}));
    h
}

fn code(r: Result<pext_engine::Output, pext_core::Error>) -> String {
    r.err().map(|e| e.code).unwrap_or_else(|| "ok".into())
}

#[test]
fn tokens_are_shown_once_and_stored_hashed() {
    let h = world();
    let t = h.ok("token.create", json!({"name": "claude", "role": "read"}));
    let secret = t["token"].as_str().unwrap();
    assert!(
        secret.starts_with("pext_") && secret.len() == 69,
        "{secret}"
    );
    assert!(t["warning"].as_str().unwrap().contains("only time"));
    let l = h.ok("token.list", json!({}));
    assert_eq!(l[0]["name"], "claude");
    assert_eq!(l[0]["status"], "active");
    assert!(!l.to_string().contains(secret));
    assert_eq!(l[0]["prefix"], &secret[..13]);
    // the secret appears nowhere in the database or the audit log
    let raw = std::fs::read(h.path()).unwrap();
    assert!(!raw.windows(secret.len()).any(|w| w == secret.as_bytes()));
    let audit = h.ok("audit.list", json!({"limit": 5}));
    assert!(!audit.to_string().contains(secret));
    // a second token differs
    let t2 = h.ok("token.create", json!({"name": "other"}));
    assert_ne!(t2["token"], t["token"]);
    assert_eq!(
        h.err("token.create", json!({"name": "CLAUDE"})).code,
        "already_exists"
    );
}

#[test]
fn authentication_failures_are_specific_but_safe() {
    let h = world();
    let (secret, _) = h.token(json!({"name": "a"}));
    assert_eq!(
        h.e.authenticate("garbage").unwrap_err().code,
        "invalid_token"
    );
    assert_eq!(
        h.e.authenticate(&format!("pext_{}", "0".repeat(64)))
            .unwrap_err()
            .code,
        "invalid_token"
    );
    assert_eq!(h.e.authenticate("").unwrap_err().code, "invalid_token");
    assert_eq!(
        h.e.authenticate(&secret[..20]).unwrap_err().code,
        "invalid_token"
    );
    assert_eq!(
        h.e.authenticate("pext_é").unwrap_err().code,
        "invalid_token"
    );
    h.ok("token.revoke", json!({"name": "a"}));
    assert_eq!(h.e.authenticate(&secret).unwrap_err().code, "token_revoked");
    assert_eq!(
        h.err("token.revoke", json!({"name": "a"})).code,
        "already_revoked"
    );
    assert_eq!(
        h.err("token.revoke", json!({"name": "zzz"})).code,
        "token_not_found"
    );
    assert_eq!(h.ok("token.list", json!({}))[0]["status"], "revoked");
}

#[test]
fn expiry() {
    let h = world();
    assert_eq!(
        h.err(
            "token.create",
            json!({"name": "x", "expires": "2026-11-15"})
        )
        .code,
        "invalid_date"
    );
    let (secret, _) = h.token(json!({"name": "temp", "expires": "2026-12-01"}));
    assert!(h.e.authenticate(&secret).is_ok());
    let mut cfg = h.e.cfg.clone();
    cfg.today = Some(chrono::NaiveDate::from_ymd_opt(2026, 12, 2).unwrap());
    let later = pext_engine::Engine::new(cfg);
    assert_eq!(
        later.authenticate(&secret).unwrap_err().code,
        "token_expired"
    );
    assert_eq!(h.ok("token.list", json!({}))[0]["status"], "active");
}

#[test]
fn roles_gate_what_a_token_can_do() {
    let h = world();
    let (_, read) = h.token(json!({"name": "r", "role": "read"}));
    let (_, append) = h.token(json!({"name": "a", "role": "append"}));
    let (_, write) = h.token(json!({"name": "w", "role": "write"}));
    let add = json!({"account": "Bank", "amount": "-10", "date": "2026-11-05"});

    assert_eq!(code(h.as_actor(&read, "txn.list", json!({}))), "ok");
    assert_eq!(code(h.as_actor(&read, "report.summary", json!({}))), "ok");
    assert_eq!(code(h.as_actor(&read, "txn.add", add.clone())), "forbidden");

    assert_eq!(code(h.as_actor(&append, "txn.add", add.clone())), "ok");
    assert_eq!(
        code(h.as_actor(&append, "txn.edit", json!({"id": 1, "memo": "x"}))),
        "forbidden"
    );
    assert_eq!(
        code(h.as_actor(&append, "txn.rm", json!({"id": 1}))),
        "forbidden"
    );
    assert_eq!(code(h.as_actor(&append, "undo", json!({}))), "forbidden");

    assert_eq!(
        code(h.as_actor(&write, "txn.edit", json!({"id": 1, "memo": "x"}))),
        "ok"
    );
    // writing does not include administration
    for op in ["token.create", "token.list", "config.set", "doctor"] {
        let args = match op {
            "token.create" => json!({"name": "evil", "role": "full"}),
            "config.set" => json!({"key": "require_token", "value": "false"}),
            _ => json!({}),
        };
        assert_eq!(code(h.as_actor(&write, op, args)), "forbidden", "{op}");
    }
    // audit entries name the token
    let audit = h.ok("audit.list", json!({"limit": 5}));
    let names: Vec<String> = audit
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["actor"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.contains(&"token:a".to_string()) && names.contains(&"token:w".to_string()),
        "{names:?}"
    );
}

#[test]
fn account_scoped_tokens_see_only_their_accounts() {
    let h = world();
    let (_, bank) = h.token(json!({"name": "bank-only", "role": "write", "account": ["Bank"]}));
    assert_eq!(bank.accounts.as_ref().unwrap().len(), 1);
    let list = h.as_actor(&bank, "txn.list", json!({})).unwrap().data;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["account"], "Bank");
    assert_eq!(
        code(h.as_actor(&bank, "acct.get", json!({"account": "Savings"}))),
        "scope_denied"
    );
    assert_eq!(
        code(h.as_actor(&bank, "acct.get", json!({"account": "Bank"}))),
        "ok"
    );
    assert_eq!(
        code(h.as_actor(
            &bank,
            "txn.add",
            json!({"account": "Savings", "amount": "-1", "date": "2026-11-05"})
        )),
        "scope_denied"
    );
    assert_eq!(
        code(h.as_actor(&bank, "txn.get", json!({"id": 2}))),
        "scope_denied"
    );
    assert_eq!(
        code(h.as_actor(&bank, "txn.rm", json!({"id": 2}))),
        "scope_denied"
    );
    // reports and net worth only add up what the token can see
    let s = h
        .as_actor(&bank, "report.summary", json!({"period": "2026-11"}))
        .unwrap()
        .data;
    assert_eq!(s["income_minor"], 0);
    assert_eq!(s["expense_minor"], 25_000);
    let nw = h.as_actor(&bank, "networth", json!({}));
    assert_eq!(code(nw), "scope_denied"); // not a scoped-safe command
                                          // operations that reach across accounts are closed to scoped tokens
    for op in [
        "budget.list",
        "goal.status",
        "loan.list",
        "snapshot.list",
        "recur.list",
        "alert.check",
        "audit.list",
    ] {
        assert_eq!(
            code(h.as_actor(&bank, op, json!({}))),
            "scope_denied",
            "{op}"
        );
    }
    assert_eq!(
        h.err(
            "token.create",
            json!({"name": "x", "role": "full", "account": ["Bank"]})
        )
        .code,
        "invalid_scope"
    );
    assert_eq!(h.ok("token.list", json!({}))[0]["accounts"][0], "Bank");
}

#[test]
fn query_language_cannot_escape_the_scope() {
    let h = world();
    let (_, bank) = h.token(json!({"name": "b", "account": ["Bank"]}));
    for q in [
        "account:Savings",
        "amount>0",
        "or payee:Employer",
        "category:salary or account:Savings",
    ] {
        let r = h
            .as_actor(&bank, "query", json!({"query": q}))
            .unwrap()
            .data;
        assert!(
            r.as_array().unwrap().iter().all(|x| x["account"] == "Bank"),
            "{q}: {r}"
        );
    }
    let g = h
        .as_actor(&bank, "query", json!({"query": "group:account agg:sum"}))
        .unwrap()
        .data;
    assert_eq!(g.as_array().unwrap().len(), 1);
}

#[test]
fn redaction_masks_free_text_but_not_numbers() {
    let h = world();
    let (_, r) = h.token(json!({"name": "redacted", "redact": true}));
    let t = h.as_actor(&r, "txn.list", json!({})).unwrap().data;
    let text = t.to_string();
    for secret in ["Cafe Secret", "Dana", "Employer"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    assert_eq!(t[1]["payee"], "***");
    assert_eq!(t[1]["amount_minor"], -25_000);
    let one = h.as_actor(&r, "txn.get", json!({"id": 1})).unwrap().data;
    assert_eq!(one["memo"], "***");
    let summary = h
        .as_actor(
            &r,
            "report.summary",
            json!({"period": "2026-11", "account": ["Bank"]}),
        )
        .unwrap()
        .data
        .to_string();
    assert!(!summary.contains("Cafe Secret"), "{summary}");
    let q = h
        .as_actor(
            &r,
            "report.spending",
            json!({"by": "payee", "period": "2026-11"}),
        )
        .unwrap()
        .data;
    assert!(!q.to_string().contains("Cafe Secret"), "{q}");
    // commands that echo raw text are closed to redacting tokens
    for op in [
        "audit.list",
        "rule.list",
        "view.list",
        "import.batches",
        "payee.list",
    ] {
        assert_eq!(
            code(h.as_actor(&r, op, json!({}))),
            "redaction_blocks_command",
            "{op}"
        );
    }
    let by_payee = h
        .as_actor(&r, "query", json!({"query": "group:payee agg:sum"}))
        .unwrap()
        .data
        .to_string();
    assert!(
        !by_payee.contains("Cafe Secret") && !by_payee.contains("Employer"),
        "{by_payee}"
    );
    let top = h
        .as_actor(&r, "report.summary", json!({"period": "2026-11"}))
        .unwrap()
        .data
        .to_string();
    assert!(!top.contains("Employer"), "{top}");
    // un-redacted tokens still see everything
    let (_, plain) = h.token(json!({"name": "plain"}));
    assert!(h
        .as_actor(&plain, "txn.list", json!({}))
        .unwrap()
        .data
        .to_string()
        .contains("Cafe Secret"));
}

#[test]
fn require_token_locks_out_anonymous_callers() {
    let h = world();
    let (_, full) = h.token(json!({"name": "admin", "role": "full"}));
    let o = h
        .as_actor(
            &full,
            "config.set",
            json!({"key": "require_token", "value": "true"}),
        )
        .unwrap();
    assert_eq!(o.data["value"], "true");
    // the local owner without a token is now refused, for reads and writes
    assert_eq!(h.err("txn.list", json!({})).code, "token_required");
    assert_eq!(
        h.err("txn.add", json!({"account": "Bank", "amount": "-1"}))
            .code,
        "token_required"
    );
    assert_eq!(h.err("doctor", json!({})).code, "token_required");
    // with a token everything works as before
    assert_eq!(code(h.as_actor(&full, "txn.list", json!({}))), "ok");
    // whoami works anywhere and reports the caller
    let w = h.as_actor(&full, "whoami", json!({})).unwrap().data;
    assert_eq!(w["name"], "token:admin");
    assert_eq!(w["role"], "full");
    assert_eq!(h.ok("whoami", json!({}))["name"], "local");
}

#[test]
fn creating_a_token_cannot_use_an_idempotency_key() {
    let h = world();
    let opts = CallOpts {
        idempotency_key: Some("k1".into()),
        ..Default::default()
    };
    let e = h
        .raw("token.create", json!({"name": "x"}), &opts, &Actor::owner())
        .unwrap_err();
    assert_eq!(e.code, "idempotency_not_applicable");
    assert!(h.ok("token.list", json!({})).as_array().unwrap().is_empty());
}

#[test]
fn token_changes_are_undoable_and_audited() {
    let h = world();
    h.ok("token.create", json!({"name": "t"}));
    h.ok("token.revoke", json!({"name": "t"}));
    h.ok("undo", json!({})); // un-revoke
    assert_eq!(h.ok("token.list", json!({}))[0]["status"], "active");
    h.doctor_ok();
    let _: Value = h.ok("audit.verify", json!({}));
}
