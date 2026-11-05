mod common;
use common::H;
use serde_json::json;

fn charge(h: &H, payee: &str, amount: &str, date: &str) {
    h.ok(
        "txn.add",
        json!({"account": "Bank", "amount": amount, "date": date, "payee": payee}),
    );
}

fn seed() -> H {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "0");
    for m in ["06", "07", "08", "09", "10", "11"] {
        charge(&h, "Netflix", "-649", &format!("2026-{m}-05"));
    }
    for (m, a) in [
        ("07", "-119"),
        ("08", "-119"),
        ("09", "-119"),
        ("10", "-139"),
        ("11", "-139"),
    ] {
        charge(&h, "Spotify", a, &format!("2026-{m}-10"));
    }
    // irregular coffee: not a subscription
    for d in [
        "2026-10-01",
        "2026-10-04",
        "2026-10-19",
        "2026-11-02",
        "2026-11-09",
    ] {
        charge(&h, "Cafe", "-180", d);
    }
    // a service that stopped 8 months ago
    for m in ["01", "02", "03"] {
        charge(&h, "OldGym", "-1500", &format!("2026-{m}-01"));
    }
    h
}

#[test]
fn detects_regular_charges_only() {
    let h = seed();
    let d = h.ok("sub.detect", json!({}));
    let names: Vec<&str> = d
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["payee"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["Netflix", "Spotify"],
        "sorted by annual cost, lapsed and irregular excluded: {d}"
    );
    let netflix = &d[0];
    assert_eq!(netflix["frequency"], "monthly");
    assert_eq!(netflix["occurrences"], 6);
    assert_eq!(netflix["last_amount_minor"], 64_900);
    assert_eq!(netflix["annual_minor"], 64_900 * 12);
    assert_eq!(netflix["next_expected"], "2026-12-05");
    assert_eq!(netflix["amount_stable"], true);
    assert_eq!(netflix["status"], "active");
    assert!(netflix["price_change_pct"].is_null());
    let spotify = &d[1];
    assert_eq!(spotify["price_change_pct"], 16); // 119 -> 139
    let all = h.ok("sub.detect", json!({"include_lapsed": true}));
    assert!(all
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["payee"] == "OldGym" && r["status"] == "lapsed"));
}

#[test]
fn track_a_detected_subscription() {
    let h = seed();
    let a = h.ok("sub.add", json!({"payee": "netflix"}));
    assert_eq!(a["amount_minor"], -64_900);
    assert_eq!(a["next_due"], "2026-12-05");
    h.ok("sub.add", json!({"payee": "Spotify", "name": "music"}));
    assert_eq!(
        h.err("sub.add", json!({"payee": "Nobody"})).code,
        "subscription_not_found"
    );
    let l = h.ok("sub.list", json!({}));
    let rows = l.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    let total = rows.iter().find(|r| r["name"] == "TOTAL").unwrap();
    assert_eq!(total["annual_minor"], (64_900 + 13_900) * 12);
    assert_eq!(total["monthly_minor"], 64_900 + 13_900);
    let d = h.ok("sub.detect", json!({}));
    assert!(d.as_array().unwrap().iter().all(|r| r["tracked"] == true));
    // a tracked subscription posts like any recurring item
    let p = h.ok("recur.post", json!({"through": "2026-12-31"}));
    assert_eq!(p["posted"], 2);
}

// ---- goals --------------------------------------------------------------

fn goal_setup() -> H {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "0");
    h
}

#[test]
fn goal_progress_and_projection() {
    let h = goal_setup();
    let g = h.ok(
        "goal.add",
        json!({"name": "Emergency", "target": "120000", "by": "2027-05-15"}),
    );
    assert_eq!(g["target_minor"], 12_000_000);
    assert_eq!(g["currency"], "INR");
    assert_eq!(g["required_monthly_minor"], 2_000_000); // 6 months left
    h.ok(
        "goal.contribute",
        json!({"goal": "emergency", "amount": "20000", "date": "2026-10-01"}),
    );
    h.ok(
        "goal.contribute",
        json!({"goal": "Emergency", "amount": "10000", "date": "2026-11-10"}),
    );
    let s = h.ok("goal.status", json!({}));
    let g = &s[0];
    assert_eq!(g["saved_minor"], 3_000_000);
    assert_eq!(g["remaining_minor"], 9_000_000);
    assert_eq!(g["pct"], 25);
    assert_eq!(g["avg_monthly_minor"], 1_000_000); // 30000 in the last 90 days / 3
    assert_eq!(g["state"], "behind"); // needs 15000/month (9000... remaining/6) = 1.5M > 1.0M
    assert_eq!(g["required_monthly_minor"], 1_500_000);
    assert_eq!(g["projected_completion"], "2027-08-15"); // 9 more months at 10000
    h.ok(
        "goal.contribute",
        json!({"goal": "emergency", "amount": "90000", "date": "2026-11-14"}),
    );
    let g = h.ok("goal.status", json!({"goal": "emergency"}));
    assert_eq!(g[0]["state"], "complete");
    assert_eq!(g[0]["remaining_minor"], 0);
    assert_eq!(
        h.ok("goal.history", json!({"goal": "emergency"}))
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn goal_rules() {
    let h = goal_setup();
    h.ok("goal.add", json!({"name": "Trip", "target": "1000"}));
    assert_eq!(
        h.err("goal.add", json!({"name": "trip", "target": "5"}))
            .code,
        "already_exists"
    );
    assert_eq!(
        h.err("goal.add", json!({"name": "x", "target": "0"})).code,
        "invalid_amount"
    );
    assert_eq!(
        h.err(
            "goal.add",
            json!({"name": "x", "target": "5", "by": "2020-01-01"})
        )
        .code,
        "invalid_date"
    );
    assert_eq!(
        h.err("goal.contribute", json!({"goal": "Trip", "amount": "-5"}))
            .code,
        "invalid_amount"
    );
    assert_eq!(
        h.err("goal.withdraw", json!({"goal": "Trip", "amount": "5"}))
            .code,
        "insufficient_goal_balance"
    );
    h.ok("goal.contribute", json!({"goal": "Trip", "amount": "300"}));
    h.ok("goal.withdraw", json!({"goal": "Trip", "amount": "100"}));
    assert_eq!(h.ok("goal.status", json!({}))[0]["saved_minor"], 20_000);
    assert_eq!(
        h.err("goal.contribute", json!({"goal": "nope", "amount": "5"}))
            .code,
        "goal_not_found"
    );
    // deleting a goal with contributions needs confirmation
    assert_eq!(
        h.err("goal.rm", json!({"goal": "Trip"})).code,
        "confirmation_required"
    );
    h.ok("goal.edit", json!({"goal": "Trip", "archive": true}));
    assert!(h
        .ok("goal.status", json!({}))
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        h.ok("goal.status", json!({"all": true}))
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        h.err("goal.contribute", json!({"goal": "Trip", "amount": "5"}))
            .code,
        "goal_archived"
    );
    let r = h.yes("goal.rm", json!({"goal": "Trip"})).unwrap();
    assert_eq!(r.data["contributions_removed"], 2);
    h.doctor_ok();
    // undo brings it all back
    h.ok("undo", json!({}));
    assert_eq!(
        h.ok("goal.history", json!({"goal": "Trip"}))
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn goal_linked_to_account_follows_balance() {
    let h = H::on("2026-11-15");
    h.acct("Savings", "savings", "5000");
    h.ok(
        "goal.add",
        json!({"name": "Car", "target": "20000", "account": "Savings"}),
    );
    let g = h.ok("goal.status", json!({}));
    assert_eq!(g[0]["saved_minor"], 500_000);
    assert_eq!(g[0]["pct"], 25);
    assert_eq!(g[0]["linked_account"], "Savings");
    h.ok(
        "txn.add",
        json!({"account": "Savings", "amount": "5000", "date": "2026-11-10"}),
    );
    assert_eq!(h.ok("goal.status", json!({}))[0]["saved_minor"], 1_000_000);
    assert_eq!(
        h.err("goal.contribute", json!({"goal": "Car", "amount": "5"}))
            .code,
        "goal_is_linked"
    );
    h.ok("acct.add", json!({"name": "Card", "kind": "credit_card"}));
    assert_eq!(
        h.err(
            "goal.add",
            json!({"name": "bad", "target": "5", "account": "Card"})
        )
        .code,
        "invalid_goal_account"
    );
}
