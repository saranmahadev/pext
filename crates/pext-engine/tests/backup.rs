#![allow(clippy::inconsistent_digit_grouping, clippy::result_large_err)]
mod common;
use common::H;
use pext_engine::crypto::Secret;
use pext_engine::{Actor, CallOpts, Engine};
use serde_json::{json, Map, Value};

fn seeded() -> H {
    let h = H::on("2026-03-15");
    h.acct("Bank", "checking", "1000.00");
    h.txn("Bank", "-250.50", "2026-03-10", Some("food"));
    h.txn("Bank", "5000.00", "2026-03-01", Some("salary"));
    h
}

fn engine_with(h: &H, pass: Option<&str>, db: std::path::PathBuf) -> Engine {
    let mut cfg = h.e.cfg.clone();
    cfg.passphrase = pass.map(|p| Secret(p.into()));
    cfg.db_path = db;
    Engine::new(cfg)
}

fn run(e: &Engine, op: &str, args: Value, opts: &CallOpts) -> Result<Value, pext_core::Error> {
    let m: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
    e.call(op, m, opts, &Actor::owner()).map(|o| o.data)
}

fn backup(h: &H, dir: &std::path::Path) -> String {
    h.ok("backup.create", json!({"dir": dir.to_string_lossy()}))["file"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn round_trip_restores_exact_data() {
    let h = seeded();
    let dir = h.dir.path().join("bk");
    let before = h.dump();
    let file = backup(&h, &dir);
    let v = h.ok("backup.verify", json!({"file": file}));
    assert_eq!(v["ok"], true, "{v}");
    // Change things, then restore.
    h.txn("Bank", "-99.00", "2026-03-12", Some("food"));
    assert_ne!(h.dump(), before);
    let r = h.yes("backup.restore", json!({"file": file})).unwrap().data;
    assert_eq!(r["restored"], true, "{r}");
    assert_eq!(h.dump(), before);
    h.doctor_ok();
    assert_eq!(h.balance("Bank"), 1000_00 - 250_50 + 5000_00);
}

#[test]
fn restore_requires_yes_and_dry_run_changes_nothing() {
    let h = seeded();
    let file = backup(&h, &h.dir.path().join("bk"));
    h.txn("Bank", "-1.00", "2026-03-12", None);
    let after = h.dump();
    let r = h.ok("backup.restore", json!({"file": file}));
    assert_eq!(r["restored"], false);
    assert_eq!(h.dump(), after);
    let d = h.dry("backup.restore", json!({"file": file})).data;
    assert_eq!(d["restored"], false);
    assert_eq!(h.dump(), after);
    // yes + dry-run still does not write
    let opts = CallOpts {
        yes: true,
        dry_run: true,
        ..Default::default()
    };
    let d = run(&h.e, "backup.restore", json!({"file": file}), &opts).unwrap();
    assert_eq!(d["restored"], false);
    assert_eq!(h.dump(), after);
}

#[test]
fn restore_keeps_a_copy_of_the_previous_database() {
    let h = seeded();
    let file = backup(&h, &h.dir.path().join("bk"));
    h.txn("Bank", "-1.00", "2026-03-12", None);
    let r = h.yes("backup.restore", json!({"file": file})).unwrap().data;
    let kept = r["previous_database_kept_as"].as_str().expect("copy path");
    assert!(std::path::Path::new(kept).exists());
    // The kept copy still has the extra transaction.
    let c = rusqlite::Connection::open(kept).unwrap();
    let n: i64 = c
        .query_row("SELECT COUNT(*) FROM transactions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 3);
}

#[test]
fn restore_into_a_fresh_location_needs_no_confirmation() {
    let h = seeded();
    let file = backup(&h, &h.dir.path().join("bk"));
    let fresh = engine_with(
        &h,
        Some("correct horse battery"),
        h.dir.path().join("new/fresh.db"),
    );
    let r = run(
        &fresh,
        "backup.restore",
        json!({"file": file}),
        &CallOpts::default(),
    )
    .unwrap();
    assert_eq!(r["restored"], true);
    let b = run(
        &fresh,
        "acct.balance",
        json!({"account": "Bank"}),
        &CallOpts::default(),
    )
    .unwrap();
    assert_eq!(b["balance_minor"], 1000_00 - 250_50 + 5000_00);
}

#[test]
fn wrong_passphrase_is_rejected_everywhere() {
    let h = seeded();
    let file = backup(&h, &h.dir.path().join("bk"));
    let other = engine_with(&h, Some("a different passphrase"), h.path());
    for op in ["backup.verify", "backup.restore"] {
        let e = run(
            &other,
            op,
            json!({"file": file}),
            &CallOpts {
                yes: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(e.code, "decrypt_failed", "{op}: {} ({})", e.message, e.code);
    }
    h.doctor_ok();
}

#[test]
fn missing_passphrase_is_a_clear_auth_error() {
    let h = seeded();
    let none = engine_with(&h, None, h.path());
    let e = run(
        &none,
        "backup.create",
        json!({"dir": h.dir.path().join("bk").to_string_lossy()}),
        &CallOpts::default(),
    )
    .unwrap_err();
    assert_eq!(e.code, "passphrase_required");
    assert!(!h.dir.path().join("bk").exists(), "nothing may be written");
}

#[test]
fn weak_passphrase_is_refused() {
    let h = seeded();
    let weak = engine_with(&h, Some("short"), h.path());
    let e = run(
        &weak,
        "backup.create",
        json!({"dir": h.dir.path().join("bk").to_string_lossy()}),
        &CallOpts::default(),
    )
    .unwrap_err();
    assert_eq!(e.kind.as_str(), "validation", "{} {}", e.code, e.message);
}

#[test]
fn tampering_is_detected() {
    let h = seeded();
    let file = backup(&h, &h.dir.path().join("bk"));
    let mut bytes = std::fs::read(&file).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x01;
    std::fs::write(&file, &bytes).unwrap();
    let e = h.err("backup.verify", json!({"file": file}));
    assert!(
        matches!(e.code.as_str(), "decrypt_failed" | "backup_corrupt"),
        "{}",
        e.code
    );
    let e = h.err("backup.restore", json!({"file": file}));
    assert!(
        matches!(e.code.as_str(), "decrypt_failed" | "backup_corrupt"),
        "{}",
        e.code
    );
}

#[test]
fn truncated_and_garbage_files_are_errors_not_panics() {
    let h = seeded();
    let file = backup(&h, &h.dir.path().join("bk"));
    let bytes = std::fs::read(&file).unwrap();
    for len in [0usize, 5, 20, 60, bytes.len() - 1] {
        let f = h.dir.path().join(format!("trunc{len}.pextbak"));
        std::fs::write(&f, &bytes[..len]).unwrap();
        assert!(
            h.call("backup.verify", json!({"file": f.to_string_lossy()}))
                .is_err(),
            "len {len}"
        );
    }
    let g = h.dir.path().join("garbage.pextbak");
    std::fs::write(&g, b"this is not a backup at all, just text").unwrap();
    assert!(h
        .call("backup.verify", json!({"file": g.to_string_lossy()}))
        .is_err());
    let e = h.err(
        "backup.verify",
        json!({"file": h.dir.path().join("nope.pextbak").to_string_lossy()}),
    );
    assert_eq!(e.code, "read_failed");
}

#[test]
fn backups_are_encrypted_not_plaintext() {
    let h = seeded();
    let file = backup(&h, &h.dir.path().join("bk"));
    let bytes = std::fs::read(&file).unwrap();
    assert!(bytes.starts_with(b"PEXTBAK1"));
    let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    assert!(!has(b"SQLite format"), "database header visible");
    assert!(!has(b"Bank"), "account name visible");
    assert!(!has(b"audit_head"), "metadata visible");
    // Two backups of identical data differ (fresh salt + nonce).
    let file2 = backup(&h, &h.dir.path().join("bk"));
    assert_ne!(bytes, std::fs::read(&file2).unwrap());
}

#[test]
fn retention_prunes_oldest() {
    let h = seeded();
    let dir = h.dir.path().join("bk");
    let mut all = vec![];
    for _ in 0..4 {
        let r = h.ok(
            "backup.create",
            json!({"dir": dir.to_string_lossy(), "keep": 2}),
        );
        all.push(r["file"].as_str().unwrap().to_string());
    }
    let l = h.ok("backup.list", json!({"dir": dir.to_string_lossy()}));
    assert_eq!(l.as_array().unwrap().len(), 2);
    assert!(!std::path::Path::new(&all[0]).exists());
    assert!(std::path::Path::new(&all[3]).exists());
    assert_eq!(
        h.err(
            "backup.create",
            json!({"dir": dir.to_string_lossy(), "keep": 0})
        )
        .code,
        "invalid_keep"
    );
}

#[test]
fn list_of_missing_folder_is_empty() {
    let h = seeded();
    let l = h.ok(
        "backup.list",
        json!({"dir": h.dir.path().join("nothing").to_string_lossy()}),
    );
    assert_eq!(l, json!([]));
}

#[test]
fn backup_does_not_modify_the_database_or_audit_chain() {
    let h = seeded();
    let before = h.dump();
    let head = h.ok("audit.verify", json!({}));
    backup(&h, &h.dir.path().join("bk"));
    assert_eq!(h.dump(), before);
    assert_eq!(h.ok("audit.verify", json!({})), head);
}

#[test]
fn export_all_contains_data_but_no_secrets() {
    let h = seeded();
    let (secret, _) = h.token(json!({"name": "agent", "role": "read"}));
    let x = h.ok("export.all", json!({}));
    assert_eq!(x["format"], "pext-export/1");
    let txns = x["tables"]["transactions"].as_array().unwrap();
    assert_eq!(txns.len(), 2);
    let s = x.to_string();
    assert!(!s.contains(&secret));
    if let Some(i) = s.find("secret_hash") {
        panic!(
            "secret_hash leaked: ...{}...",
            &s[i.saturating_sub(120)..(i + 120).min(s.len())]
        );
    }
    assert_eq!(x["tables"]["tokens"].as_array().unwrap().len(), 1);
}

#[test]
fn wipe_requires_explicit_confirmation() {
    let h = seeded();
    assert_eq!(
        h.err("data.wipe", json!({"confirm": "yes"})).code,
        "confirmation_required"
    );
    assert!(h.path().exists());
    let d = h.dry("data.wipe", json!({"confirm": "WIPE"})).data;
    assert_eq!(d["wiped"], false);
    assert!(h.path().exists());
}

#[test]
fn wipe_can_export_first_and_then_deletes() {
    let h = seeded();
    let out = h.dir.path().join("final.json");
    let r = h.ok(
        "data.wipe",
        json!({"confirm": "WIPE", "export": out.to_string_lossy()}),
    );
    assert_eq!(r["wiped"], true);
    assert!(!h.path().exists());
    let dump: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(dump["tables"]["transactions"].as_array().unwrap().len(), 2);
    assert_eq!(
        h.err("data.wipe", json!({"confirm": "WIPE"})).kind.as_str(),
        "not_found"
    );
}

#[test]
fn backup_ops_are_local_only_and_admin() {
    let h = seeded();
    let cmds = h.ok("commands", json!({}));
    let list = cmds.as_array().unwrap();
    for name in [
        "backup.create",
        "backup.restore",
        "backup.verify",
        "backup.list",
        "data.wipe",
        "export.all",
    ] {
        let c = list
            .iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing"));
        assert_eq!(c["local_only"], true, "{name}");
        assert_eq!(c["access"], "full", "{name}");
    }
    let (_s, writer) = h.token(json!({"name": "w", "role": "write"}));
    let e = h.as_actor(&writer, "backup.create", json!({})).unwrap_err();
    assert_eq!(e.kind.as_str(), "auth");
}
