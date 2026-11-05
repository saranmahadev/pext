#![allow(clippy::inconsistent_digit_grouping)]
mod common;
use common::{today, H};
use serde_json::{json, Value};

/// Two devices (each with its own database) sharing one folder.
struct Pair {
    a: H,
    b: H,
    shared: std::path::PathBuf,
}

fn pair() -> Pair {
    let a = H::on("2026-03-15");
    a.acct("Bank", "checking", "1000.00");
    a.txn("Bank", "-250.50", "2026-03-10", Some("food"));
    let b = H::bare_on(today());
    let shared = a.dir.path().join("shared");
    Pair { a, b, shared }
}

impl Pair {
    fn dir(&self) -> Value {
        json!({"dir": self.shared.to_string_lossy()})
    }
    /// A pushes; B joins and pulls.
    fn linked() -> Pair {
        let p = pair();
        p.a.ok("sync.init", p.dir());
        p.a.ok("sync.push", json!({}));
        p.b.ok("sync.init", p.dir());
        p.b.ok("sync.pull", json!({}));
        p
    }
    fn state(h: &H) -> String {
        h.ok("sync.status", json!({}))["state"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

#[test]
fn second_device_clones_the_first() {
    let p = pair();
    let i = p.a.ok("sync.init", p.dir());
    assert_eq!(i["state"], "no_remote");
    let push = p.a.ok("sync.push", json!({}));
    assert_eq!(push["pushed"], true);
    let i = p.b.ok("sync.init", p.dir());
    assert_eq!(i["state"], "no_local");
    let pull = p.b.ok("sync.pull", json!({}));
    assert_eq!(pull["pulled"], true);
    assert_eq!(p.b.dump(), p.a.dump());
    assert_eq!(p.b.balance("Bank"), 1000_00 - 250_50);
    p.b.doctor_ok();
    assert_eq!(Pair::state(&p.a), "in_sync");
    assert_eq!(Pair::state(&p.b), "in_sync");
}

#[test]
fn ahead_and_behind_follow_the_audit_chain() {
    let p = Pair::linked();
    p.a.txn("Bank", "-10.00", "2026-03-11", Some("food"));
    assert_eq!(Pair::state(&p.a), "ahead");
    assert_eq!(Pair::state(&p.b), "in_sync"); // folder not updated yet
    p.a.ok("sync.push", json!({}));
    assert_eq!(Pair::state(&p.a), "in_sync");
    assert_eq!(Pair::state(&p.b), "behind");
    // B cannot push while behind.
    let e = p.b.err("sync.push", json!({}));
    assert_eq!(e.code, "sync_conflict");
    assert_eq!(e.kind.as_str(), "conflict");
    p.b.ok("sync.pull", json!({}));
    assert_eq!(Pair::state(&p.b), "in_sync");
    assert_eq!(p.b.dump(), p.a.dump());
    // Pull again is a no-op.
    assert_eq!(p.b.ok("sync.pull", json!({}))["pulled"], false);
}

#[test]
fn changes_flow_both_ways_in_turn() {
    let p = Pair::linked();
    p.b.txn("Bank", "-5.00", "2026-03-12", Some("food"));
    p.b.ok("sync.push", json!({}));
    p.a.ok("sync.pull", json!({}));
    p.a.txn("Bank", "-7.00", "2026-03-13", Some("food"));
    p.a.ok("sync.push", json!({}));
    p.b.ok("sync.pull", json!({}));
    assert_eq!(p.a.dump(), p.b.dump());
    assert_eq!(p.a.balance("Bank"), 1000_00 - 250_50 - 5_00 - 7_00);
    p.a.doctor_ok();
    p.b.doctor_ok();
}

#[test]
fn pull_when_ahead_and_push_when_in_sync_are_noops() {
    let p = Pair::linked();
    assert_eq!(p.a.ok("sync.push", json!({}))["pushed"], false);
    p.a.txn("Bank", "-1.00", "2026-03-11", None);
    let before = p.a.dump();
    let r = p.a.ok("sync.pull", json!({}));
    assert_eq!(r["pulled"], false);
    assert_eq!(r["state"], "ahead");
    assert_eq!(p.a.dump(), before);
}

#[test]
fn independent_changes_diverge_and_nothing_is_merged_silently() {
    let p = Pair::linked();
    p.a.txn("Bank", "-11.00", "2026-03-11", Some("food"));
    p.b.txn("Bank", "-22.00", "2026-03-12", Some("food"));
    p.a.ok("sync.push", json!({}));
    assert_eq!(Pair::state(&p.b), "diverged");
    let (da, db_) = (p.a.dump(), p.b.dump());
    for op in ["sync.push", "sync.pull"] {
        let e = p.b.err(op, json!({}));
        assert_eq!(e.code, "sync_conflict", "{op}");
        assert_eq!(e.details.as_ref().unwrap()["state"], "diverged");
    }
    assert_eq!(p.a.dump(), da);
    assert_eq!(p.b.dump(), db_);
}

#[test]
fn resolve_requires_confirmation_and_dry_run_changes_nothing() {
    let p = Pair::linked();
    p.a.txn("Bank", "-11.00", "2026-03-11", None);
    p.b.txn("Bank", "-22.00", "2026-03-12", None);
    p.a.ok("sync.push", json!({}));
    let before = p.b.dump();
    let r = p.b.ok("sync.resolve", json!({"keep": "remote"}));
    assert_eq!(r["resolved"], false);
    let r = p.b.dry("sync.resolve", json!({"keep": "remote"})).data;
    assert_eq!(r["resolved"], false);
    assert_eq!(p.b.dump(), before);
    assert_eq!(Pair::state(&p.b), "diverged");
}

#[test]
fn resolve_keep_remote_replaces_local_and_keeps_a_copy() {
    let p = Pair::linked();
    p.a.txn("Bank", "-11.00", "2026-03-11", None);
    p.b.txn("Bank", "-22.00", "2026-03-12", None);
    p.a.ok("sync.push", json!({}));
    let r =
        p.b.yes("sync.resolve", json!({"keep": "remote"}))
            .unwrap()
            .data;
    assert_eq!(r["resolved"], true);
    assert_eq!(p.b.dump(), p.a.dump());
    assert_eq!(p.b.balance("Bank"), 1000_00 - 250_50 - 11_00);
    let kept = r["previous_database_kept_as"].as_str().unwrap();
    let c = rusqlite::Connection::open(kept).unwrap();
    let n: i64 = c
        .query_row("SELECT COUNT(*) FROM transactions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2, "B's own transaction survives in the copy");
    assert_eq!(Pair::state(&p.b), "in_sync");
    p.b.doctor_ok();
}

#[test]
fn resolve_keep_local_publishes_and_saves_the_remote_as_a_copy() {
    let p = Pair::linked();
    p.a.txn("Bank", "-11.00", "2026-03-11", None);
    p.b.txn("Bank", "-22.00", "2026-03-12", None);
    p.a.ok("sync.push", json!({}));
    let r =
        p.b.yes("sync.resolve", json!({"keep": "local"}))
            .unwrap()
            .data;
    assert_eq!(r["resolved"], true);
    let copy = r["remote_kept_as"].as_str().unwrap();
    assert!(std::path::Path::new(copy).exists());
    assert_eq!(Pair::state(&p.b), "in_sync");
    // The other device now simply fast-forwards to B's data.
    assert_eq!(Pair::state(&p.a), "diverged");
    p.a.yes("sync.resolve", json!({"keep": "remote"})).unwrap();
    assert_eq!(p.a.dump(), p.b.dump());
    assert_eq!(p.a.balance("Bank"), 1000_00 - 250_50 - 22_00);
    // The conflict copy is a normal backup that can be inspected.
    assert_eq!(p.a.ok("backup.verify", json!({"file": copy}))["ok"], true);
}

#[test]
fn resolve_when_not_diverged_is_refused() {
    let p = Pair::linked();
    let e =
        p.a.yes("sync.resolve", json!({"keep": "local"}))
            .unwrap_err();
    assert_eq!(e.code, "nothing_to_resolve");
    assert_eq!(
        p.a.err("sync.resolve", json!({"keep": "sideways"})).code,
        "invalid_argument"
    );
}

#[test]
fn an_unrelated_database_is_never_overwritten() {
    let p = Pair::linked();
    let other = H::on("2026-05-01");
    other.acct("Cash", "cash", "5.00");
    other.ok("sync.init", p.dir());
    assert_eq!(Pair::state(&other), "diverged");
    let before = other.dump();
    assert_eq!(other.err("sync.push", json!({})).code, "sync_conflict");
    assert_eq!(other.err("sync.pull", json!({})).code, "sync_conflict");
    assert_eq!(other.dump(), before);
    // The shared snapshot is untouched.
    assert_eq!(Pair::state(&p.a), "in_sync");
}

#[test]
fn not_configured_and_empty_folder_errors() {
    let p = pair();
    assert_eq!(
        p.a.err("sync.status", json!({})).code,
        "sync_not_configured"
    );
    assert_eq!(p.a.err("sync.push", json!({})).code, "sync_not_configured");
    p.b.ok("sync.init", p.dir());
    assert_eq!(p.b.err("sync.pull", json!({})).code, "sync_remote_empty");
}

#[test]
fn dry_run_push_and_pull_write_nothing() {
    let p = pair();
    p.a.ok("sync.init", p.dir());
    let d = p.a.dry("sync.push", json!({})).data;
    assert_eq!(d["pushed"], false);
    assert_eq!(d["would_push"], true);
    assert!(!p.shared.join("pext-sync.pextbak").exists());
    p.a.ok("sync.push", json!({}));
    p.b.ok("sync.init", p.dir());
    let d = p.b.dry("sync.pull", json!({})).data;
    assert_eq!(d["would_pull"], true);
    assert!(!p.b.path().exists());
}

#[test]
fn wrong_passphrase_cannot_read_the_folder() {
    let p = Pair::linked();
    let mut cfg = p.b.e.cfg.clone();
    cfg.passphrase = Some(pext_engine::crypto::Secret("not the right one".into()));
    let e = pext_engine::Engine::new(cfg);
    let r = e.call(
        "sync.status",
        serde_json::Map::new(),
        &Default::default(),
        &pext_engine::Actor::owner(),
    );
    assert_eq!(r.unwrap_err().code, "decrypt_failed");
}

#[test]
fn a_tampered_snapshot_is_rejected_and_local_data_is_untouched() {
    let p = Pair::linked();
    p.a.txn("Bank", "-3.00", "2026-03-11", None);
    p.a.ok("sync.push", json!({}));
    let f = p.shared.join("pext-sync.pextbak");
    let mut bytes = std::fs::read(&f).unwrap();
    let n = bytes.len() / 2;
    bytes[n] ^= 0x80;
    std::fs::write(&f, bytes).unwrap();
    let before = p.b.dump();
    assert_eq!(p.b.err("sync.pull", json!({})).code, "decrypt_failed");
    assert_eq!(p.b.dump(), before);
}

#[test]
fn a_broken_local_audit_chain_is_never_published() {
    let p = Pair::linked();
    p.a.txn("Bank", "-3.00", "2026-03-11", None);
    {
        let c = p.a.conn();
        c.execute_batch("DROP TRIGGER audit_only_undone_by; UPDATE audit SET args='{\"tampered\":true}' WHERE id=2;").unwrap();
    }
    assert_eq!(p.a.ok("sync.status", json!({}))["local_chain_ok"], false);
    assert_eq!(p.a.err("sync.push", json!({})).code, "local_chain_broken");
}

#[test]
fn sync_ops_are_local_only_admin_and_not_audited() {
    let p = Pair::linked();
    let head = p.a.ok("audit.verify", json!({}));
    p.a.ok("sync.status", json!({}));
    p.a.ok("sync.push", json!({}));
    assert_eq!(
        p.a.ok("audit.verify", json!({})),
        head,
        "sync must not alter the history it syncs"
    );
    let (_s, w) = p.a.token(json!({"name": "w", "role": "write"}));
    assert_eq!(
        p.a.as_actor(&w, "sync.push", json!({}))
            .unwrap_err()
            .kind
            .as_str(),
        "auth"
    );
}
