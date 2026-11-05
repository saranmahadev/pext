mod common;
use common::H;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

fn script(h: &H, name: &str, body: &str) -> PathBuf {
    let dir = h.dir.path().join("plugins");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn setup() -> H {
    let h = H::on("2026-11-15");
    h.ok(
        "invest.add",
        json!({"symbol": "NIFTYBEES", "kind": "etf", "name": "Nifty ETF"}),
    );
    h.ok(
        "invest.add",
        json!({"symbol": "GOLDBEES", "kind": "etf", "name": "Gold ETF"}),
    );
    h
}

fn install(h: &H, name: &str, kind: &str, p: &std::path::Path) -> Value {
    h.ok("plugin.install", json!({"name": name, "kind": kind, "command": p.to_string_lossy(), "permissions": ["network"]}))
}

const FEED: &str = r#"cat >/dev/null
echo '{"prices":[{"symbol":"NIFTYBEES","price":"321.25","date":"2026-11-14"},{"symbol":"GOLDBEES","price":"58.5"}]}'"#;

#[test]
fn install_list_and_remove() {
    let h = setup();
    let p = script(&h, "feed.sh", FEED);
    let d = install(&h, "feed", "price_feed", &p);
    assert_eq!(d["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(d["permissions"], json!(["network"]));
    assert_eq!(h.ok("plugin.list", json!({})).as_array().unwrap().len(), 1);
    assert_eq!(
        h.err(
            "plugin.install",
            json!({"name": "FEED", "kind": "other", "command": p.to_string_lossy()})
        )
        .code,
        "plugin_exists"
    );
    h.ok("plugin.rm", json!({"name": "feed"}));
    assert_eq!(h.ok("plugin.list", json!({})), json!([]));
    assert_eq!(
        h.err("plugin.rm", json!({"name": "feed"})).code,
        "plugin_not_found"
    );
}

#[test]
fn install_validates_inputs() {
    let h = setup();
    let p = script(&h, "feed.sh", FEED);
    let cmd = p.to_string_lossy().to_string();
    assert_eq!(
        h.err(
            "plugin.install",
            json!({"name": "bad name!", "kind": "other", "command": cmd})
        )
        .code,
        "invalid_plugin_name"
    );
    assert_eq!(
        h.err(
            "plugin.install",
            json!({"name": "x", "kind": "other", "command": "/no/such/file"})
        )
        .code,
        "plugin_not_found_on_disk"
    );
    let plain = h.dir.path().join("plain.txt");
    std::fs::write(&plain, "hi").unwrap();
    assert_eq!(
        h.err(
            "plugin.install",
            json!({"name": "x", "kind": "other", "command": plain.to_string_lossy()})
        )
        .code,
        "plugin_not_executable"
    );
    assert_eq!(
        h.err(
            "plugin.install",
            json!({"name": "x", "kind": "other", "command": cmd, "permissions": ["root"]})
        )
        .code,
        "invalid_permission"
    );
    assert_eq!(
        h.err(
            "plugin.install",
            json!({"name": "x", "kind": "other", "command": cmd, "sha256": "00"})
        )
        .code,
        "plugin_hash_mismatch"
    );
    assert_eq!(
        h.err(
            "plugin.install",
            json!({"name": "x", "kind": "nonsense", "command": cmd})
        )
        .code,
        "invalid_argument"
    );
    // a correct pin is accepted
    let sha = install(&h, "ok", "other", &p)["sha256"]
        .as_str()
        .unwrap()
        .to_string();
    h.ok("plugin.rm", json!({"name": "ok"}));
    h.ok(
        "plugin.install",
        json!({"name": "pinned", "kind": "other", "command": cmd, "sha256": sha.to_uppercase()}),
    );
}

#[test]
fn plugin_dir_restricts_where_plugins_may_live() {
    let h = setup();
    let inside = script(&h, "feed.sh", FEED);
    let mut cfg = h.e.cfg.clone();
    cfg.plugin_dir = Some(h.dir.path().join("plugins"));
    let e = pext_engine::Engine::new(cfg);
    // Ok(()) on success, otherwise the error code.
    let run = |cmd: &std::path::Path| -> Result<(), String> {
        e.call(
            "plugin.install",
            serde_json::from_value(
                json!({"name": "p", "kind": "other", "command": cmd.to_string_lossy()}),
            )
            .unwrap(),
            &Default::default(),
            &pext_engine::Actor::owner(),
        )
        .map(|_| ())
        .map_err(|e| e.code)
    };
    assert!(run(&inside).is_ok());
    let outside = h.dir.path().join("outside.sh");
    std::fs::write(&outside, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755)).unwrap();
    e.call(
        "plugin.rm",
        serde_json::from_value(json!({"name": "p"})).unwrap(),
        &Default::default(),
        &pext_engine::Actor::owner(),
    )
    .unwrap();
    assert_eq!(run(&outside).unwrap_err(), "plugin_outside_dir");
}

#[test]
fn price_fetch_records_prices_through_the_audited_path() {
    let h = setup();
    install(&h, "feed", "price_feed", &script(&h, "feed.sh", FEED));
    let r = h.ok("price.fetch", json!({"plugin": "feed"}));
    assert_eq!(r["applied_count"], 2, "{r}");
    assert_eq!(r["rejected_count"], 0);
    let l = h.ok("price.list", json!({"instrument": "NIFTYBEES"}));
    assert_eq!(l[0]["price"], "321.25");
    assert_eq!(l[0]["date"], "2026-11-14");
    let g = h.ok("price.list", json!({"instrument": "GOLDBEES"}));
    assert_eq!(g[0]["date"], "2026-11-15", "missing date defaults to today");
    // Audited and undoable like any other change.
    let last = h.ok("audit.list", json!({"limit": 1}));
    assert_eq!(last[0]["op"], "price.fetch");
    h.ok("undo", json!({}));
    assert_eq!(
        h.ok("price.list", json!({"instrument": "NIFTYBEES"})),
        json!([])
    );
    h.doctor_ok();
}

#[test]
fn price_fetch_dry_run_stores_nothing() {
    let h = setup();
    install(&h, "feed", "price_feed", &script(&h, "feed.sh", FEED));
    let d = h.dry("price.fetch", json!({"plugin": "feed"})).data;
    assert_eq!(d["applied_count"], 2);
    assert_eq!(
        h.ok("price.list", json!({"instrument": "NIFTYBEES"})),
        json!([])
    );
}

#[test]
fn price_fetch_limits_to_requested_symbols_and_passes_the_request() {
    let h = setup();
    // The plugin echoes the symbols it was asked for as prices (proving the request is passed on).
    let p = script(
        &h,
        "echo.sh",
        r#"req=$(cat)
case "$req" in *'"protocol":1'*) ;; *) echo '{"prices":[]}'; exit 0;; esac
case "$req" in *GOLDBEES*) echo '{"prices":[{"symbol":"GOLDBEES","price":"60"}]}';; *) echo '{"prices":[{"symbol":"NIFTYBEES","price":"1"}]}';; esac"#,
    );
    install(&h, "echo", "price_feed", &p);
    let r = h.ok(
        "price.fetch",
        json!({"plugin": "echo", "symbols": ["goldbees"]}),
    );
    assert_eq!(r["applied_count"], 1, "{r}");
    assert_eq!(r["applied"][0]["instrument"], "GOLDBEES");
    assert_eq!(
        h.err(
            "price.fetch",
            json!({"plugin": "echo", "symbols": ["nope"]})
        )
        .code,
        "instrument_not_found"
    );
}

#[test]
fn hostile_or_sloppy_plugin_output_is_rejected_item_by_item() {
    let h = setup();
    let bad = r#"cat >/dev/null
cat <<'EOF'
{"prices":[
 {"symbol":"NIFTYBEES","price":"100"},
 {"symbol":"UNKNOWN","price":"5"},
 {"symbol":"GOLDBEES","price":"-3"},
 {"symbol":"GOLDBEES","price":"0"},
 {"symbol":"GOLDBEES","price":"abc"},
 {"symbol":"GOLDBEES","price":"1e3"},
 {"symbol":"GOLDBEES","price":"10","date":"2099-01-01"},
 {"symbol":"GOLDBEES","price":"10","date":"yesterday"},
 {"price":"10"},
 {"symbol":"GOLDBEES"},
 {"symbol":"GOLDBEES","price":"12345678901234567890"},
 "garbage", 42, null
]}
EOF"#;
    install(&h, "bad", "price_feed", &script(&h, "bad.sh", bad));
    let r = h.ok("price.fetch", json!({"plugin": "bad"}));
    assert_eq!(
        r["applied_count"], 1,
        "only the valid price may be stored: {r}"
    );
    assert_eq!(r["rejected_count"], 13, "{r}");
    assert_eq!(
        h.ok("price.list", json!({"instrument": "GOLDBEES"})),
        json!([])
    );
    h.doctor_ok();
}

#[test]
fn malformed_responses_are_errors() {
    let h = setup();
    for (name, body, code) in [
        (
            "notjson",
            "cat >/dev/null; echo 'hello'",
            "plugin_bad_output",
        ),
        ("empty", "cat >/dev/null", "plugin_bad_output"),
        (
            "noprices",
            "cat >/dev/null; echo '{\"x\":1}'",
            "plugin_bad_output",
        ),
        (
            "exit1",
            "cat >/dev/null; echo oops >&2; exit 1",
            "plugin_failed",
        ),
    ] {
        install(
            &h,
            name,
            "price_feed",
            &script(&h, &format!("{name}.sh"), body),
        );
        let e = h.err("price.fetch", json!({"plugin": name}));
        assert_eq!(e.code, code, "{name}: {}", e.message);
    }
    // stderr is surfaced for diagnosis
    let e = h.err("price.fetch", json!({"plugin": "exit1"}));
    assert!(e.details.unwrap().to_string().contains("oops"));
    assert_eq!(
        h.ok("price.list", json!({"instrument": "NIFTYBEES"})),
        json!([])
    );
}

#[test]
fn wrong_kind_is_refused() {
    let h = setup();
    install(
        &h,
        "imp",
        "importer",
        &script(&h, "imp.sh", "cat >/dev/null; echo '{}'"),
    );
    assert_eq!(
        h.err("price.fetch", json!({"plugin": "imp"})).code,
        "wrong_plugin_kind"
    );
}

#[test]
fn modifying_the_executable_blocks_it() {
    let h = setup();
    let p = script(&h, "feed.sh", FEED);
    install(&h, "feed", "price_feed", &p);
    h.ok("price.fetch", json!({"plugin": "feed"}));
    std::fs::write(
        &p,
        "#!/bin/sh\necho '{\"prices\":[{\"symbol\":\"NIFTYBEES\",\"price\":\"999999\"}]}'\n",
    )
    .unwrap();
    let e = h.err("price.fetch", json!({"plugin": "feed"}));
    assert_eq!(e.code, "plugin_modified");
    assert_eq!(e.kind.as_str(), "integrity");
    assert_eq!(
        h.err("plugin.run", json!({"name": "feed"})).code,
        "plugin_modified"
    );
    std::fs::remove_file(&p).unwrap();
    assert_eq!(
        h.err("plugin.run", json!({"name": "feed"})).code,
        "plugin_missing"
    );
}

#[test]
fn slow_plugins_are_killed() {
    let h = setup();
    install(&h, "slow", "other", &script(&h, "slow.sh", "sleep 30"));
    let t = std::time::Instant::now();
    let e = h.err("plugin.run", json!({"name": "slow", "timeout": 1}));
    assert_eq!(e.code, "plugin_timeout");
    assert!(t.elapsed().as_secs() < 10, "took {:?}", t.elapsed());
    assert_eq!(
        h.err("plugin.run", json!({"name": "slow", "timeout": 0}))
            .code,
        "invalid_timeout"
    );
    assert_eq!(
        h.err("plugin.run", json!({"name": "slow", "timeout": 61}))
            .code,
        "invalid_timeout"
    );
}

#[test]
fn runaway_output_is_stopped() {
    let h = setup();
    install(
        &h,
        "flood",
        "other",
        &script(
            &h,
            "flood.sh",
            "yes 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'",
        ),
    );
    let t = std::time::Instant::now();
    let e = h.err("plugin.run", json!({"name": "flood", "timeout": 20}));
    assert_eq!(e.code, "plugin_output_too_large", "{}", e.message);
    assert!(t.elapsed().as_secs() < 15);
}

#[test]
fn plugin_runs_with_a_clean_environment_and_gets_input() {
    let h = setup();
    std::env::set_var("PEXT_TEST_SECRET", "hunter2");
    let p = script(
        &h,
        "env.sh",
        r#"req=$(cat)
printf '{"secret":"%s","proto":"%s","req":%s}' "${PEXT_TEST_SECRET:-none}" "$PEXT_PLUGIN_PROTOCOL" "$req""#,
    );
    install(&h, "env", "other", &p);
    let r = h.ok(
        "plugin.run",
        json!({"name": "env", "input": "{\"hello\":[1,2]}"}),
    );
    assert_eq!(r["untrusted"], true);
    assert_eq!(
        r["output"]["secret"], "none",
        "environment must not leak into plugins"
    );
    assert_eq!(r["output"]["proto"], "1");
    assert_eq!(r["output"]["req"]["input"], json!({"hello": [1, 2]}));
    assert_eq!(r["output"]["req"]["protocol"], 1);
    assert_eq!(
        h.err("plugin.run", json!({"name": "env", "input": "{not json"}))
            .code,
        "invalid_input"
    );
}

#[test]
fn plugins_need_the_full_role_and_never_touch_the_database() {
    let h = setup();
    let p = script(&h, "feed.sh", FEED);
    let (_s, writer) = h.token(json!({"name": "w", "role": "write"}));
    for (op, args) in [
        (
            "plugin.install",
            json!({"name": "x", "kind": "other", "command": p.to_string_lossy()}),
        ),
        ("plugin.list", json!({})),
        ("plugin.run", json!({"name": "x"})),
        ("price.fetch", json!({"plugin": "x"})),
    ] {
        assert_eq!(
            h.as_actor(&writer, op, args).unwrap_err().kind.as_str(),
            "auth",
            "{op}"
        );
    }
    // A plugin that tries to wreck the database path only sees an empty temp cwd.
    let evil = script(
        &h,
        "evil.sh",
        &format!(
            "cat >/dev/null; rm -f '{}' 2>/dev/null; echo '{{}}'",
            "../nothing"
        ),
    );
    install(&h, "evil", "other", &evil);
    h.ok("plugin.run", json!({"name": "evil"}));
    h.doctor_ok();
}
