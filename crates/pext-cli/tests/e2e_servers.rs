//! Spawns the real `pext` binary to test `pext mcp` and `pext serve`.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use tempfile::TempDir;

const PASS: &str = "correct horse battery";

fn pext(db: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_pext"));
    c.env_clear()
        .env("PEXT_DB", db)
        .env("PEXT_PASSPHRASE", PASS)
        .env("PEXT_TODAY", "2026-11-15");
    c
}

fn run_json(db: &str, args: &[&str]) -> Value {
    let o = pext(db).args(args).output().unwrap();
    assert!(
        o.status.success(),
        "{args:?}: {}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_slice::<Value>(&o.stdout).unwrap()["data"].clone()
}

fn seeded() -> (TempDir, String) {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("s.db").to_string_lossy().to_string();
    run_json(&db, &["init"]);
    run_json(
        &db,
        &[
            "acct",
            "add",
            "--name",
            "Bank",
            "--kind",
            "checking",
            "--opening",
            "1000",
        ],
    );
    run_json(
        &db,
        &[
            "txn",
            "add",
            "--account",
            "Bank",
            "--amount",
            "-12.5",
            "--category",
            "food",
            "--payee",
            "Cafe",
        ],
    );
    (dir, db)
}

struct Guard(Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn http(addr: &str, req: &str) -> (u16, String) {
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).to_string();
    let st = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (st, text)
}

#[test]
fn mcp_over_real_stdio() {
    let (_d, db) = seeded();
    let mut child = pext(&db)
        .args(["mcp", "--profile", "read", "--quiet"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    fn ask(
        stdin: &mut std::process::ChildStdin,
        stdout: &mut BufReader<std::process::ChildStdout>,
        v: Value,
    ) -> Value {
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad line {line:?}: {e}"))
    }
    let init = ask(
        &mut stdin,
        &mut stdout,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "pext");
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .unwrap();
    let list = ask(
        &mut stdin,
        &mut stdout,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"txn_list") && !names.contains(&"txn_add"));
    let call = ask(
        &mut stdin,
        &mut stdout,
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "acct_balance", "arguments": {"account": "Bank"}}}),
    );
    assert_eq!(
        call["result"]["structuredContent"]["data"]["balance_minor"],
        98750
    );
    let denied = ask(
        &mut stdin,
        &mut stdout,
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "txn_add", "arguments": {"account": "Bank", "amount": "-1"}}}),
    );
    assert_eq!(denied["result"]["isError"], true);
    drop(stdin); // EOF ends the server cleanly
    let status = child.wait().unwrap();
    assert!(status.success());
}

#[test]
fn mcp_with_a_token_and_a_closed_database() {
    let (_d, db) = seeded();
    let secret = run_json(
        &db,
        &[
            "token", "create", "--name", "agent", "--role", "read", "--redact",
        ],
    )["token"]
        .as_str()
        .unwrap()
        .to_string();
    run_json(&db, &["config", "set", "require_token", "true"]);
    let mut child = pext(&db)
        .args(["mcp", "--profile", "write", "--quiet", "--token", &secret])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "txn_list", "arguments": {}}})).unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
    )
    .unwrap();
    let mut a = String::new();
    stdout.read_line(&mut a).unwrap();
    let a: Value = serde_json::from_str(&a).unwrap();
    assert!(
        !a.to_string().contains("Cafe"),
        "redaction applies through MCP: {a}"
    );
    let mut b = String::new();
    stdout.read_line(&mut b).unwrap();
    let b: Value = serde_json::from_str(&b).unwrap();
    let names: Vec<&str> = b["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        !names.contains(&"txn_add"),
        "a read token stays read-only even under the write profile"
    );
    drop(stdin);
    assert!(child.wait().unwrap().success());
    // a bad token stops the server at startup
    let o = pext(&db)
        .args(["mcp", "--token", "pext_wrong", "--quiet"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(6));
}

#[test]
fn serve_dashboard_and_api_over_the_real_binary() {
    let (_d, db) = seeded();
    let secret = run_json(&db, &["token", "create", "--name", "web", "--role", "read"])["token"]
        .as_str()
        .unwrap()
        .to_string();
    let child = pext(&db)
        .args(["serve", "--addr", "127.0.0.1:0", "--quiet"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut guard = Guard(child);
    let mut first = String::new();
    BufReader::new(guard.0.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    let hello: Value = serde_json::from_str(&first).unwrap();
    let url = hello["data"]["listening"].as_str().unwrap().to_string();
    assert_eq!(hello["data"]["max_role"], "write");
    let addr = url.trim_start_matches("http://").to_string();
    let (st, body) = http(
        &addr,
        &format!("GET / HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 200);
    assert!(body.contains("<title>pext</title>"));
    let (st, _) = http(
        &addr,
        &format!("GET /v1/call/acct.list HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 401);
    let (st, body) = http(&addr, &format!("GET /v1/call/networth HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {secret}\r\nConnection: close\r\n\r\n"));
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("net_worth_minor"));
    let (st, _) = http(&addr, &format!("POST /v1/call/acct.add HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {secret}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"));
    assert_eq!(st, 403, "a read token cannot write");
}

#[test]
fn serve_refuses_unsafe_or_pointless_configurations() {
    let (_d, db) = seeded();
    let o = pext(&db)
        .args(["serve", "--addr", "0.0.0.0:0", "--quiet"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stdout).contains("remote_bind_refused"));
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("none.db").to_string_lossy().to_string();
    let o = pext(&missing)
        .args(["serve", "--addr", "127.0.0.1:0", "--quiet"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(4));
    let o = pext(&db)
        .args(["serve", "--rate-limit", "0", "--quiet"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
}
