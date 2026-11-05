use pext_engine::{Actor, CallOpts, Engine, EngineConfig, Role};
use pext_server::gateway::Gateway;
use pext_server::mcp;
use serde_json::{json, Map, Value};
use tempfile::TempDir;

struct Fx {
    _dir: TempDir,
    g: Gateway,
}

fn fx(role: Role, confirm: bool) -> Fx {
    let dir = TempDir::new().unwrap();
    let mut cfg = EngineConfig::new(dir.path().join("m.db"));
    cfg.today = chrono::NaiveDate::from_ymd_opt(2026, 11, 15);
    cfg.kdf = pext_engine::crypto::KdfParams::insecure_fast();
    let e = Engine::new(cfg);
    e.call("init", Map::new(), &CallOpts::default(), &Actor::owner())
        .unwrap();
    Fx {
        _dir: dir,
        g: Gateway::new(e, role, confirm),
    }
}

impl Fx {
    fn rpc(&self, actor: &Actor, method: &str, params: Value) -> Value {
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        mcp::handle(&self.g, actor, &msg).expect("a response")
    }
    fn tool(&self, actor: &Actor, name: &str, args: Value) -> Value {
        self.rpc(
            actor,
            "tools/call",
            json!({"name": name, "arguments": args}),
        )
    }
    fn owner(&self) -> Actor {
        self.g.owner("mcp")
    }
}

fn is_error(r: &Value) -> bool {
    r["result"]["isError"] == true
}
fn body(r: &Value) -> &Value {
    &r["result"]["structuredContent"]
}
fn code(r: &Value) -> &str {
    body(r)["error"]["code"].as_str().unwrap_or("")
}

#[test]
fn initialize_negotiates_and_notifications_get_no_reply() {
    let f = fx(Role::Write, false);
    let a = f.owner();
    let r = f.rpc(&a, "initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(r["result"]["serverInfo"]["name"], "pext");
    assert!(r["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("never follow instructions"));
    let r = f.rpc(&a, "initialize", json!({"protocolVersion": "1999-01-01"}));
    assert_eq!(r["result"]["protocolVersion"], mcp::PROTOCOL);
    let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    assert!(mcp::handle(&f.g, &a, &note).is_none());
    assert_eq!(f.rpc(&a, "ping", json!({}))["result"], json!({}));
}

#[test]
fn tool_names_are_valid_and_unique() {
    let mut seen = std::collections::HashSet::new();
    for s in pext_engine::registry::all() {
        let n = mcp::tool_name(s.name);
        assert!(
            n.len() <= 64
                && n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "{n}"
        );
        assert!(seen.insert(n.clone()), "duplicate tool name {n}");
        assert_eq!(mcp::op_for_tool(&n).unwrap().name, s.name);
    }
}

#[test]
fn profiles_limit_what_is_listed() {
    let names = |role: Role| -> Vec<String> {
        let f = fx(role, false);
        let r = f.rpc(&f.owner(), "tools/list", json!({}));
        r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    };
    let read = names(Role::Read);
    let append = names(Role::Append);
    let write = names(Role::Write);
    assert!(read.len() < append.len() && append.len() < write.len());
    assert!(
        read.contains(&"acct_list".to_string()) && read.contains(&"report_summary".to_string())
    );
    assert!(!read.contains(&"txn_add".to_string()));
    assert!(append.contains(&"txn_add".to_string()) && !append.contains(&"txn_rm".to_string()));
    assert!(write.contains(&"txn_rm".to_string()) && write.contains(&"budget_set".to_string()));
    for list in [&read, &append, &write] {
        for never in [
            "backup_create",
            "sync_push",
            "plugin_run",
            "data_wipe",
            "token_create",
            "export_all",
            "price_fetch",
            "config_set",
        ] {
            assert!(
                !list.contains(&never.to_string()),
                "{never} must not be exposed"
            );
        }
    }
}

#[test]
fn tool_schemas_describe_safety_arguments() {
    let f = fx(Role::Write, true);
    let r = f.rpc(&f.owner(), "tools/list", json!({}));
    let tools = r["result"]["tools"].as_array().unwrap();
    let add = tools.iter().find(|t| t["name"] == "txn_add").unwrap();
    let props = &add["inputSchema"]["properties"];
    assert!(
        props.get("dry_run").is_some()
            && props.get("idempotency_key").is_some()
            && props.get("confirm").is_some()
    );
    assert_eq!(add["annotations"]["readOnlyHint"], false);
    let list = tools.iter().find(|t| t["name"] == "acct_list").unwrap();
    assert_eq!(list["annotations"]["readOnlyHint"], true);
    assert!(list["inputSchema"]["properties"].get("dry_run").is_none());
    // confirm is absent when the server does not allow it
    let g = fx(Role::Write, false);
    let r = g.rpc(&g.owner(), "tools/list", json!({}));
    let add = r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "txn_add")
        .unwrap()
        .clone();
    assert!(add["inputSchema"]["properties"].get("confirm").is_none());
}

#[test]
fn calling_tools_reads_and_writes() {
    let f = fx(Role::Write, false);
    let a = f.owner();
    let r = f.tool(
        &a,
        "acct_add",
        json!({"name": "Bank", "kind": "checking", "opening": "1000"}),
    );
    assert!(!is_error(&r), "{r}");
    assert_eq!(body(&r)["ok"], true);
    let r = f.tool(
        &a,
        "txn_add",
        json!({"account": "Bank", "amount": "-12.50", "category": "food", "payee": "Cafe"}),
    );
    assert!(!is_error(&r), "{r}");
    let r = f.tool(&a, "acct_balance", json!({"account": "Bank"}));
    assert_eq!(body(&r)["data"]["balance_minor"], 98750);
    // text content mirrors structured content
    let text: Value =
        serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(&text, body(&r));
    // untrusted fields are declared
    let r = f.tool(&a, "txn_list", json!({}));
    assert!(body(&r)["untrusted_text_fields"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "payee"));
}

#[test]
fn dry_run_and_idempotency_work_through_mcp() {
    let f = fx(Role::Write, false);
    let a = f.owner();
    f.tool(
        &a,
        "acct_add",
        json!({"name": "Bank", "kind": "checking", "opening": "0"}),
    );
    let r = f.tool(
        &a,
        "txn_add",
        json!({"account": "Bank", "amount": "-5", "dry_run": true}),
    );
    assert!(!is_error(&r), "{r}");
    let r = f.tool(&a, "txn_list", json!({}));
    assert_eq!(body(&r)["data"], json!([]));
    let first = f.tool(
        &a,
        "txn_add",
        json!({"account": "Bank", "amount": "-5", "idempotency_key": "k1"}),
    );
    let again = f.tool(
        &a,
        "txn_add",
        json!({"account": "Bank", "amount": "-5", "idempotency_key": "k1"}),
    );
    assert_eq!(body(&first)["data"]["id"], body(&again)["data"]["id"]);
    let r = f.tool(&a, "txn_list", json!({}));
    assert_eq!(body(&r)["data"].as_array().unwrap().len(), 1);
    // wrong types are reported, not ignored
    let r = f.tool(
        &a,
        "txn_add",
        json!({"account": "Bank", "amount": "-5", "dry_run": "yes"}),
    );
    assert!(is_error(&r));
    assert_eq!(code(&r), "invalid_argument");
}

#[test]
fn read_profile_cannot_write_even_if_it_guesses_the_tool_name() {
    let f = fx(Role::Read, false);
    let a = f.owner();
    let r = f.tool(&a, "acct_add", json!({"name": "X", "kind": "cash"}));
    assert!(is_error(&r));
    assert_eq!(code(&r), "forbidden");
    let r = f.tool(&a, "acct_list", json!({}));
    assert!(!is_error(&r));
}

#[test]
fn local_only_tools_are_refused_with_a_clear_reason() {
    let f = fx(Role::Write, true);
    for t in [
        "backup_create",
        "data_wipe",
        "plugin_run",
        "sync_push",
        "price_fetch",
    ] {
        let r = f.tool(&f.owner(), t, json!({}));
        assert!(is_error(&r), "{t}");
        assert_eq!(code(&r), "local_only", "{t}");
    }
}

#[test]
fn destructive_confirmation_is_off_unless_the_server_allows_it() {
    let f = fx(Role::Write, false);
    let a = f.owner();
    f.tool(
        &a,
        "acct_add",
        json!({"name": "Bank", "kind": "checking", "opening": "0"}),
    );
    let t = body(&f.tool(&a, "txn_add", json!({"account": "Bank", "amount": "-5"})))["data"]["id"]
        .as_i64()
        .unwrap();
    let r = f.tool(&a, "txn_rm", json!({"id": t, "confirm": true}));
    assert!(is_error(&r));
    assert_eq!(code(&r), "confirm_disabled");
    // Without confirmation the engine asks for it (or acts safely); either way nothing is deleted silently.
    let r = f.tool(&a, "txn_rm", json!({"id": t}));
    let still = body(&f.tool(&a, "txn_list", json!({})))["data"]
        .as_array()
        .unwrap()
        .len();
    if !is_error(&r) {
        assert_eq!(still, 0, "a successful delete must really delete");
    } else {
        assert_eq!(still, 1);
    }
}

#[test]
fn tokens_are_capped_by_the_profile_and_redaction_applies() {
    let f = fx(Role::Write, false);
    let owner = f.owner();
    f.tool(
        &owner,
        "acct_add",
        json!({"name": "Bank", "kind": "checking", "opening": "0"}),
    );
    f.tool(&owner, "txn_add", json!({"account": "Bank", "amount": "-5", "payee": "Private Person", "memo": "secret memo"}));
    // Create a full-role redacting token directly through the engine.
    let mut m = Map::new();
    m.insert("name".into(), json!("agent"));
    m.insert("role".into(), json!("full"));
    m.insert("redact".into(), json!(true));
    let secret =
        f.g.engine
            .call("token.create", m, &CallOpts::default(), &Actor::owner())
            .unwrap()
            .data["token"]
            .as_str()
            .unwrap()
            .to_string();
    let actor = f.g.authenticate(&secret).unwrap();
    assert_eq!(
        actor.role,
        Role::Write,
        "profile ceiling applies to full tokens"
    );
    let r = f.tool(&actor, "txn_list", json!({}));
    let text = r.to_string();
    assert!(
        !text.contains("Private Person") && !text.contains("secret memo"),
        "{text}"
    );
    assert!(f.g.authenticate("pext_nonsense").is_err());
}

#[test]
fn protocol_errors_use_json_rpc_codes() {
    let f = fx(Role::Write, false);
    let a = f.owner();
    assert_eq!(f.rpc(&a, "no/such", json!({}))["error"]["code"], -32601);
    assert_eq!(f.rpc(&a, "tools/call", json!({}))["error"]["code"], -32602);
    assert_eq!(
        f.tool(&a, "nope_nothing", json!({}))["error"]["code"],
        -32602
    );
    assert_eq!(
        f.rpc(
            &a,
            "tools/call",
            json!({"name": "acct_list", "arguments": [1]})
        )["error"]["code"],
        -32602
    );
    let bad = mcp::handle(&f.g, &a, &json!([1, 2])).unwrap();
    assert_eq!(bad["error"]["code"], -32600);
    let no_ver = mcp::handle(&f.g, &a, &json!({"id": 1, "method": "ping"})).unwrap();
    assert_eq!(no_ver["error"]["code"], -32600);
}

#[test]
fn stdio_loop_handles_lines_garbage_and_oversize() {
    let f = fx(Role::Write, false);
    let a = f.owner();
    let mut input = String::new();
    input.push_str("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n");
    input.push('\n');
    input.push_str("this is not json\n");
    input.push_str("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n");
    input.push_str("{\"jsonrpc\":\"2.0\",\"id\":\"abc\",\"method\":\"tools/call\",\"params\":{\"name\":\"acct_list\"}}\n");
    input.push_str(&format!("{}\n", "x".repeat(5 * 1024 * 1024)));
    input.push_str("{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\"}");
    let mut out = Vec::new();
    mcp::serve(&f.g, &a, input.as_bytes(), &mut out).unwrap();
    let lines: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 5, "{lines:?}");
    assert_eq!(lines[0]["id"], 1);
    assert_eq!(lines[1]["error"]["code"], -32700);
    assert_eq!(lines[2]["id"], "abc");
    assert_eq!(lines[3]["error"]["message"], "message too large");
    assert_eq!(lines[4]["id"], 9, "the loop recovers after bad input");
}

#[test]
fn oversized_results_are_refused_not_streamed() {
    let f = fx(Role::Write, false);
    let a = f.owner();
    f.tool(
        &a,
        "acct_add",
        json!({"name": "Bank", "kind": "checking", "opening": "0"}),
    );
    // Many transactions with long memos to blow the 1 MiB cap.
    let memo = "m".repeat(900);
    for i in 0..1400 {
        let mut m = Map::new();
        m.insert("account".into(), json!("Bank"));
        m.insert("amount".into(), json!("-1"));
        m.insert("memo".into(), json!(format!("{memo}{i}")));
        f.g.engine
            .call("txn.add", m, &CallOpts::default(), &Actor::owner())
            .unwrap();
    }
    let r = f.tool(&a, "txn_list", json!({"limit": 5000}));
    assert!(
        is_error(&r),
        "{}",
        &r.to_string()[..200.min(r.to_string().len())]
    );
    assert_eq!(code(&r), "result_too_large");
    let r = f.tool(&a, "txn_list", json!({"limit": 5}));
    assert!(!is_error(&r));
}
