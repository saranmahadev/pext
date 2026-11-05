mod common;
use chrono::NaiveDate;
use common::H;
use pext_engine::ops::recurring::{annual_cost, occurrence};
use serde_json::json;

fn d(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}

#[test]
fn occurrences_keep_the_anchor_day() {
    let s = d("2026-01-31");
    let m = |n| occurrence(s, "monthly", 1, n).unwrap().to_string();
    assert_eq!(m(0), "2026-01-31");
    assert_eq!(m(1), "2026-02-28");
    assert_eq!(m(2), "2026-03-31"); // back to the 31st, not the 28th
    assert_eq!(m(3), "2026-04-30");
    assert_eq!(
        occurrence(d("2024-02-29"), "yearly", 1, 1)
            .unwrap()
            .to_string(),
        "2025-02-28"
    );
    assert_eq!(
        occurrence(d("2024-02-29"), "yearly", 1, 4)
            .unwrap()
            .to_string(),
        "2028-02-29"
    );
    assert_eq!(
        occurrence(s, "weekly", 2, 3).unwrap().to_string(),
        "2026-03-14"
    );
    assert_eq!(
        occurrence(s, "daily", 1, 365).unwrap().to_string(),
        "2027-01-31"
    );
    assert_eq!(
        occurrence(s, "quarterly", 1, 1).unwrap().to_string(),
        "2026-04-30"
    );
    assert_eq!(
        occurrence(s, "monthly", 3, 2).unwrap().to_string(),
        "2026-07-31"
    );
    assert!(occurrence(s, "monthly", 1, i64::MAX / 4).is_err());
}

#[test]
fn annual_cost_math() {
    assert_eq!(annual_cost(-100_000, "monthly", 1), -1_200_000);
    assert_eq!(annual_cost(100_000, "quarterly", 1), 400_000);
    assert_eq!(annual_cost(100_000, "yearly", 1), 100_000);
    assert_eq!(annual_cost(7_000, "weekly", 1), 365_000);
    assert_eq!(annual_cost(100_000, "monthly", 2), 600_000);
}

fn setup() -> H {
    let h = H::on("2026-11-15");
    h.acct("Bank", "checking", "100000");
    h
}

#[test]
fn bills_are_outflows_and_income_inflows() {
    let h = setup();
    let rent = h.ok("recur.add", json!({"name": "rent", "account": "Bank", "amount": "25000", "start": "2026-11-01", "category": "housing.rent"}));
    assert_eq!(rent["amount_minor"], -2_500_000);
    let sal = h.ok("recur.add", json!({"name": "salary", "kind": "income", "account": "Bank", "amount": "85000", "start": "2026-11-28", "category": "salary"}));
    assert_eq!(sal["amount_minor"], 8_500_000);
    assert_eq!(
        h.err(
            "recur.add",
            json!({"name": "x", "kind": "income", "account": "Bank", "amount": "-5"})
        )
        .code,
        "invalid_amount"
    );
    assert_eq!(
        h.err(
            "recur.add",
            json!({"name": "rent", "account": "Bank", "amount": "5"})
        )
        .code,
        "already_exists"
    );
    assert_eq!(
        h.err(
            "recur.add",
            json!({"name": "y", "account": "Bank", "amount": "0"})
        )
        .code,
        "zero_amount"
    );
    assert_eq!(h.err("recur.add", json!({"name": "y", "account": "Bank", "amount": "5", "start": "2026-11-10", "end": "2026-11-01"})).code, "invalid_end");
    let list = h.ok("recur.list", json!({}));
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(list[0]["name"], "rent"); // sorted by next due
    assert_eq!(list[0]["overdue"], true);
    assert_eq!(list[0]["annual_minor"], -30_000_000);
}

#[test]
fn due_lists_overdue_and_upcoming() {
    let h = setup();
    h.ok(
        "recur.add",
        json!({"name": "rent", "account": "Bank", "amount": "25000", "start": "2026-11-01"}),
    );
    h.ok("recur.add", json!({"name": "insurance", "account": "Bank", "amount": "1200", "freq": "yearly", "start": "2027-02-01"}));
    let due = h.ok("recur.due", json!({"within": 30}));
    let rows = due.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["due"], "2026-11-01");
    assert_eq!(rows[0]["overdue"], true);
    assert_eq!(rows[0]["days_until"], -14);
    assert_eq!(rows[1]["due"], "2026-12-01");
    assert_eq!(
        h.ok("recur.due", json!({"within": 120}))
            .as_array()
            .unwrap()
            .len(),
        6
    ); // Nov..Mar rent + insurance
}

#[test]
fn posting_is_idempotent_and_advances() {
    let h = setup();
    h.ok("recur.add", json!({"name": "rent", "account": "Bank", "amount": "25000", "start": "2026-11-01", "category": "housing.rent"}));
    let r = h.ok("recur.post", json!({}));
    assert_eq!(r["posted"], 1);
    assert_eq!(r["transactions"][0]["date"], "2026-11-01");
    assert_eq!(h.balance("Bank"), 10_000_000 - 2_500_000);
    let t = h.ok("txn.get", json!({"id": r["transactions"][0]["txn_id"]}));
    assert_eq!(t["status"], "pending");
    assert_eq!(t["category"], "housing.rent");
    assert_eq!(t["payee"], "rent");
    assert_eq!(t["external_ref"], "recur:1:0");
    // nothing new is due
    assert_eq!(h.ok("recur.post", json!({}))["posted"], 0);
    assert_eq!(
        h.ok("recur.get", json!({"item": "rent"}))["next_due"],
        "2026-12-01"
    );
    // catch up three months
    let r = h.ok(
        "recur.post",
        json!({"through": "2027-02-15", "status": "cleared"}),
    );
    assert_eq!(r["posted"], 3);
    assert_eq!(
        h.ok("recur.get", json!({"item": "1"}))["next_due"],
        "2027-03-01"
    );
    assert_eq!(
        h.ok("txn.list", json!({"status": ["cleared"]}))
            .as_array()
            .unwrap()
            .len(),
        3
    );
    h.doctor_ok();
}

#[test]
fn dry_run_and_undo_restore_the_schedule() {
    let h = setup();
    h.ok(
        "recur.add",
        json!({"name": "rent", "account": "Bank", "amount": "25000", "start": "2026-11-01"}),
    );
    let before = h.dump();
    let dry = h.dry("recur.post", json!({}));
    assert_eq!(dry.data["posted"], 1);
    assert_eq!(before, h.dump());
    h.ok("recur.post", json!({}));
    assert_ne!(before, h.dump());
    h.ok("undo", json!({}));
    assert_eq!(before, h.dump());
    assert_eq!(h.ok("recur.post", json!({}))["posted"], 1); // can post again after undo
}

#[test]
fn deleted_posted_transaction_is_not_reposted() {
    let h = setup();
    h.ok(
        "recur.add",
        json!({"name": "rent", "account": "Bank", "amount": "25000", "start": "2026-11-01"}),
    );
    let r = h.ok("recur.post", json!({}));
    h.ok("txn.rm", json!({"id": r["transactions"][0]["txn_id"]}));
    assert_eq!(h.ok("recur.post", json!({}))["posted"], 0);
    assert_eq!(h.balance("Bank"), 10_000_000);
}

#[test]
fn end_dates_pause_skip_and_auto_only() {
    let h = setup();
    h.ok("recur.add", json!({"name": "emi", "kind": "emi", "account": "Bank", "amount": "5000", "start": "2026-09-15", "end": "2026-11-15"}));
    h.ok("recur.add", json!({"name": "gym", "account": "Bank", "amount": "1000", "start": "2026-11-01", "auto_post": true}));
    h.ok(
        "recur.add",
        json!({"name": "wifi", "account": "Bank", "amount": "800", "start": "2026-11-01"}),
    );
    let r = h.ok("recur.post", json!({"auto_only": true}));
    assert_eq!(r["posted"], 1);
    assert_eq!(r["transactions"][0]["item"], "gym");
    let r = h.ok(
        "recur.post",
        json!({"item": "emi", "through": "2027-06-01"}),
    );
    assert_eq!(r["posted"], 3); // Sep 15, Oct 15, Nov 15 and then the end date stops it
    let emi = h.ok("recur.get", json!({"item": "emi"}));
    assert_eq!(emi["active"], false);
    assert!(h
        .ok("recur.list", json!({}))
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["name"] != "emi"));
    assert_eq!(
        h.ok("recur.list", json!({"all": true}))
            .as_array()
            .unwrap()
            .len(),
        3
    );
    // skip
    let s = h.ok("recur.skip", json!({"item": "wifi", "count": 2}));
    assert_eq!(s["last_skipped_due"], "2026-12-01");
    assert_eq!(
        h.ok("recur.get", json!({"item": "wifi"}))["next_due"],
        "2027-01-01"
    );
    assert_eq!(
        h.err("recur.skip", json!({"item": "wifi", "count": 0}))
            .code,
        "invalid_count"
    );
    // pause / resume
    h.ok("recur.edit", json!({"item": "wifi", "pause": true}));
    assert_eq!(
        h.ok("recur.post", json!({"through": "2027-03-01"}))["transactions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["item"] == "wifi")
            .count(),
        0
    );
    h.ok(
        "recur.edit",
        json!({"item": "wifi", "resume": true, "amount": "900"}),
    );
    let r = h.ok(
        "recur.post",
        json!({"item": "wifi", "through": "2027-01-10"}),
    );
    assert_eq!(r["transactions"][0]["amount_minor"], -90_000);
}

#[test]
fn posting_is_capped_per_call() {
    let h = setup();
    h.ok("recur.add", json!({"name": "daily", "account": "Bank", "amount": "1", "freq": "daily", "start": "2024-01-01"}));
    let o = h.call("recur.post", json!({})).unwrap();
    assert_eq!(o.data["posted"], 366);
    assert_eq!(o.meta["warnings"][0]["code"], "post_limit");
    let o = h.call("recur.post", json!({})).unwrap();
    assert_eq!(o.data["posted"], 366);
    h.doctor_ok();
}

#[test]
fn archived_accounts_block_posting() {
    let h = setup();
    h.ok(
        "recur.add",
        json!({"name": "rent", "account": "Bank", "amount": "5", "start": "2026-11-01"}),
    );
    h.ok("acct.archive", json!({"account": "Bank"}));
    assert_eq!(h.err("recur.post", json!({})).code, "account_archived");
    assert_eq!(
        h.err(
            "recur.add",
            json!({"name": "z", "account": "Bank", "amount": "5"})
        )
        .code,
        "account_archived"
    );
}

#[test]
fn edit_and_remove() {
    let h = setup();
    h.ok(
        "recur.add",
        json!({"name": "rent", "account": "Bank", "amount": "5"}),
    );
    assert_eq!(
        h.err("recur.edit", json!({"item": "rent"})).code,
        "nothing_to_change"
    );
    let e = h.ok("recur.edit", json!({"item": "rent", "new_name": "house rent", "category": "housing.rent", "payee": "Landlord", "remind_days": 5}));
    assert_eq!(e["name"], "house rent");
    assert_eq!(e["category"], "housing.rent");
    assert_eq!(e["remind_days"], 5);
    assert_eq!(
        h.err("recur.get", json!({"item": "rent"})).code,
        "recurring_not_found"
    );
    h.ok("recur.rm", json!({"item": "house rent"}));
    assert!(h
        .ok("recur.list", json!({"all": true}))
        .as_array()
        .unwrap()
        .is_empty());
}
