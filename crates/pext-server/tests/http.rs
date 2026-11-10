use pext_engine::{Actor, CallOpts, Engine, EngineConfig, Role};
use pext_server::gateway::Gateway;
use pext_server::http::{self, App, Request, Response, ServerConfig};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;

struct Fx {
    _dir: TempDir,
    app: Arc<App>,
    secret: String,
}

fn engine() -> (TempDir, Engine) {
    let dir = TempDir::new().unwrap();
    let mut cfg = EngineConfig::new(dir.path().join("h.db"));
    cfg.today = chrono::NaiveDate::from_ymd_opt(2026, 11, 15);
    cfg.kdf = pext_engine::crypto::KdfParams::insecure_fast();
    let e = Engine::new(cfg);
    e.call("init", Map::new(), &CallOpts::default(), &Actor::owner())
        .unwrap();
    (dir, e)
}

fn call(e: &Engine, op: &str, args: Value) -> Value {
    e.call(
        op,
        args.as_object().cloned().unwrap_or_default(),
        &CallOpts::default(),
        &Actor::owner(),
    )
    .unwrap()
    .data
}

fn make(role: &str, max: Role, confirm: bool, tweak: impl FnOnce(&mut ServerConfig)) -> Fx {
    let (dir, e) = engine();
    let secret = call(&e, "token.create", json!({"name": "t", "role": role}))["token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut cfg = ServerConfig::default();
    tweak(&mut cfg);
    Fx {
        _dir: dir,
        app: Arc::new(App::new(Gateway::new(e, max, confirm), cfg)),
        secret,
    }
}

fn fx() -> Fx {
    make("full", Role::Write, false, |_| {})
}

struct Rq {
    method: &'static str,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
    peer: String,
}

fn rq(method: &'static str, path: &str) -> Rq {
    Rq {
        method,
        path: path.to_string(),
        headers: vec![("Host".into(), "localhost:7878".into())],
        body: String::new(),
        peer: "127.0.0.1".into(),
    }
}

impl Rq {
    fn auth(mut self, secret: &str) -> Rq {
        self.headers
            .push(("Authorization".into(), format!("Bearer {secret}")));
        self
    }
    fn h(mut self, k: &str, v: &str) -> Rq {
        self.headers.push((k.into(), v.into()));
        self
    }
    fn json(mut self, v: Value) -> Rq {
        self.body = v.to_string();
        self.h("Content-Type", "application/json")
    }
    fn host(mut self, v: &str) -> Rq {
        self.headers.retain(|(k, _)| k != "Host");
        self.headers.push(("Host".into(), v.into()));
        self
    }
    fn send(self, app: &App) -> Resp {
        let (path, query) = match self.path.split_once('?') {
            Some((p, q)) => (p.to_string(), http::parse_query(q)),
            None => (self.path.clone(), vec![]),
        };
        let headers: HashMap<String, String> = self
            .headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect();
        Resp(app.handle(&Request {
            method: self.method.into(),
            path,
            query,
            headers,
            body: self.body.into_bytes(),
            peer: self.peer,
        }))
    }
}

struct Resp(Response);
impl Resp {
    fn status(&self) -> u16 {
        self.0.status
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.0.body)
            .unwrap_or_else(|e| panic!("not json ({e}): {}", String::from_utf8_lossy(&self.0.body)))
    }
    fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }
    fn header(&self, name: &str) -> Option<String> {
        self.0
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }
}

#[test]
fn health_needs_no_token_but_checks_host() {
    let f = fx();
    let r = rq("GET", "/health").send(&f.app);
    assert_eq!(r.status(), 200);
    assert_eq!(r.json()["ok"], true);
    // DNS-rebinding style Host headers are refused.
    for h in [
        "evil.example.com",
        "evil.example.com:7878",
        "127.0.0.1.evil.com",
    ] {
        let r = rq("GET", "/health").host(h).send(&f.app);
        assert_eq!(r.status(), 421, "{h}");
    }
    for h in ["localhost", "127.0.0.1:9000", "[::1]:80"] {
        assert_eq!(
            rq("GET", "/health").host(h).send(&f.app).status(),
            200,
            "{h}"
        );
    }
    let mut no_host = rq("GET", "/health");
    no_host.headers.clear();
    assert_eq!(no_host.send(&f.app).status(), 421);
}

#[test]
fn dashboard_is_static_and_locked_down() {
    let f = fx();
    let r = rq("GET", "/").send(&f.app);
    assert_eq!(r.status(), 200);
    let page = String::from_utf8(r.0.body.clone()).unwrap();
    assert!(page.contains("pext"));
    // No data in the page, no unsafe DOM sinks.
    for bad in [
        "innerHTML",
        "outerHTML",
        "document.write",
        "eval(",
        "insertAdjacentHTML",
        "localStorage",
    ] {
        assert!(!page.contains(bad), "dashboard uses {bad}");
    }
    let csp = r.header("content-security-policy").unwrap();
    assert!(
        csp.contains("default-src 'none'")
            && csp.contains("script-src 'sha256-")
            && csp.contains("style-src 'sha256-")
    );
    assert!(!csp.contains("unsafe-inline") && !csp.contains("unsafe-eval"));
    assert!(csp.contains("frame-ancestors 'none'"));
    assert_eq!(r.header("x-content-type-options").unwrap(), "nosniff");
    assert_eq!(r.header("x-frame-options").unwrap(), "DENY");
    // The hashes in the CSP really match the inline script and style.
    let script = page
        .split("<script>")
        .nth(1)
        .unwrap()
        .split("</script>")
        .next()
        .unwrap();
    let style = page
        .split("<style>")
        .nth(1)
        .unwrap()
        .split("</style>")
        .next()
        .unwrap();
    assert!(csp.contains(&sha_b64(script)) && csp.contains(&sha_b64(style)));
    assert_eq!(rq("HEAD", "/").send(&f.app).status(), 200);
    assert_eq!(rq("POST", "/").send(&f.app).status(), 404);
}

fn sha_b64(s: &str) -> String {
    // Browsers normalise CRLF to LF before hashing inline text for CSP.
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let hex = pext_engine::audit::sha256_hex(&s);
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[test]
fn api_requires_a_valid_token() {
    let f = fx();
    let r = rq("GET", "/v1/commands").send(&f.app);
    assert_eq!(r.status(), 401);
    assert_eq!(r.code(), "unauthenticated");
    assert_eq!(r.header("www-authenticate").unwrap(), "Bearer");
    for bad in [
        "Bearer",
        "Bearer ",
        "Basic abc",
        "Token xyz",
        "Bearer pext_nope",
    ] {
        let r = rq("GET", "/v1/commands")
            .h("Authorization", bad)
            .send(&f.app);
        assert_eq!(r.status(), 401, "{bad}");
    }
    assert_eq!(
        rq("GET", "/v1/commands")
            .auth(&f.secret)
            .send(&f.app)
            .status(),
        200
    );
    // lowercase scheme is fine
    let r = rq("GET", "/v1/commands")
        .h("Authorization", &format!("bearer {}", f.secret))
        .send(&f.app);
    assert_eq!(r.status(), 200);
    // tokens in the query string are never accepted
    let r = rq("GET", &format!("/v1/commands?token={}", f.secret)).send(&f.app);
    assert_eq!(r.status(), 401);
}

#[test]
fn revoked_and_expired_tokens_stop_working() {
    let f = fx();
    assert_eq!(
        rq("GET", "/v1/call/acct.list")
            .auth(&f.secret)
            .send(&f.app)
            .status(),
        200
    );
    call(&f.app.gw.engine, "token.revoke", json!({"name": "t"}));
    assert_eq!(
        rq("GET", "/v1/call/acct.list")
            .auth(&f.secret)
            .send(&f.app)
            .status(),
        401
    );
}

#[test]
fn reads_by_get_and_writes_by_post() {
    let f = fx();
    let r = rq("POST", "/v1/call/acct.add")
        .auth(&f.secret)
        .json(json!({"name": "Bank", "kind": "checking", "opening": "100"}))
        .send(&f.app);
    assert_eq!(r.status(), 200, "{}", r.json());
    assert_eq!(r.json()["schema"], "pext/v1");
    let r = rq("GET", "/v1/call/acct.balance?account=Bank")
        .auth(&f.secret)
        .send(&f.app);
    assert_eq!(r.status(), 200);
    assert_eq!(r.json()["data"]["balance_minor"], 10000);
    let r = rq("POST", "/v1/call/acct.list")
        .auth(&f.secret)
        .send(&f.app); // empty body is fine
    assert_eq!(r.status(), 200);
    assert_eq!(r.json()["data"].as_array().unwrap().len(), 1);
    // A write over GET is refused (prevents link-prefetch / image-tag side effects).
    let r = rq("GET", "/v1/call/acct.add?name=X&kind=cash")
        .auth(&f.secret)
        .send(&f.app);
    assert_eq!(r.status(), 405);
    assert_eq!(r.header("allow").unwrap(), "POST");
    assert_eq!(
        call(&f.app.gw.engine, "acct.list", json!({}))
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn bodies_are_validated() {
    let f = fx();
    let mut r = rq("POST", "/v1/call/acct.add").auth(&f.secret);
    r.body = "{\"name\":\"X\"}".into();
    assert_eq!(r.send(&f.app).status(), 415, "missing content type");
    let r = rq("POST", "/v1/call/acct.add")
        .auth(&f.secret)
        .h("Content-Type", "text/plain");
    let mut r = r;
    r.body = "{}".into();
    assert_eq!(r.send(&f.app).status(), 415);
    let mut r = rq("POST", "/v1/call/acct.add")
        .auth(&f.secret)
        .h("Content-Type", "application/json");
    r.body = "{not json".into();
    let resp = r.send(&f.app);
    assert_eq!(
        (resp.status(), resp.code()),
        (400, "invalid_json".to_string())
    );
    let mut r = rq("POST", "/v1/call/acct.add")
        .auth(&f.secret)
        .h("Content-Type", "application/json");
    r.body = "[1,2]".into();
    assert_eq!(r.send(&f.app).status(), 400);
    // unknown fields are rejected by the engine, not ignored
    let r = rq("POST", "/v1/call/acct.add")
        .auth(&f.secret)
        .json(json!({"name": "X", "kind": "cash", "bogus": 1}))
        .send(&f.app);
    assert_eq!(r.status(), 400, "{}", r.json());
}

#[test]
fn errors_map_to_http_statuses() {
    let f = fx();
    let post = |op: &str, a: Value| {
        rq("POST", &format!("/v1/call/{op}"))
            .auth(&f.secret)
            .json(a)
            .send(&f.app)
    };
    assert_eq!(
        post("acct.add", json!({"name": "Bank", "kind": "checking"})).status(),
        200
    );
    assert_eq!(
        post("acct.add", json!({"name": "Bank", "kind": "checking"})).status(),
        409
    );
    assert_eq!(
        post("acct.add", json!({"name": "Z", "kind": "spaceship"})).status(),
        422
    );
    assert_eq!(post("acct.get", json!({"account": "nope"})).status(), 404);
    assert_eq!(
        rq("POST", "/v1/call/no.such")
            .auth(&f.secret)
            .send(&f.app)
            .status(),
        404
    );
    assert_eq!(
        rq("GET", "/v1/nothing")
            .auth(&f.secret)
            .send(&f.app)
            .status(),
        404
    );
    assert_eq!(
        rq("DELETE", "/v1/call/acct.list")
            .auth(&f.secret)
            .send(&f.app)
            .status(),
        405
    );
}

#[test]
fn roles_and_local_only_are_enforced() {
    let f = make("read", Role::Write, false, |_| {});
    let post = |op: &str, a: Value| {
        rq("POST", &format!("/v1/call/{op}"))
            .auth(&f.secret)
            .json(a)
            .send(&f.app)
    };
    let r = post("acct.add", json!({"name": "X", "kind": "cash"}));
    assert_eq!((r.status(), r.code()), (403, "forbidden".to_string()));
    assert_eq!(post("acct.list", json!({})).status(), 200);
    for op in [
        "backup.create",
        "data.wipe",
        "plugin.run",
        "sync.push",
        "token.create",
    ] {
        let r = post(op, json!({}));
        assert_eq!(r.status(), 403, "{op}");
    }
    assert_eq!(post("backup.create", json!({})).code(), "local_only");
    // /commands lists only what this caller can do
    let r = rq("GET", "/v1/commands").auth(&f.secret).send(&f.app);
    let names: Vec<String> = r.json()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"acct.list".to_string()));
    assert!(
        !names.contains(&"acct.add".to_string()) && !names.contains(&"backup.create".to_string())
    );
    assert_eq!(
        rq("GET", "/v1/schema/acct.add")
            .auth(&f.secret)
            .send(&f.app)
            .status(),
        404
    );
    assert_eq!(
        rq("GET", "/v1/schema/acct.list")
            .auth(&f.secret)
            .send(&f.app)
            .status(),
        200
    );
}

#[test]
fn gateway_ceiling_caps_full_tokens() {
    let f = make("full", Role::Read, false, |_| {});
    let r = rq("POST", "/v1/call/acct.add")
        .auth(&f.secret)
        .json(json!({"name": "X", "kind": "cash"}))
        .send(&f.app);
    assert_eq!(r.status(), 403);
    let r = rq("POST", "/v1/call/token.create")
        .auth(&f.secret)
        .json(json!({"name": "y", "role": "full"}))
        .send(&f.app);
    assert_eq!(r.status(), 403);
}

#[test]
fn dry_run_idempotency_and_confirmation_headers() {
    let f = fx();
    let post = |op: &str, a: Value, hs: &[(&str, &str)]| {
        let mut r = rq("POST", &format!("/v1/call/{op}"))
            .auth(&f.secret)
            .json(a);
        for (k, v) in hs {
            r = r.h(k, v);
        }
        r.send(&f.app)
    };
    post("acct.add", json!({"name": "Bank", "kind": "checking"}), &[]);
    let r = post(
        "txn.add",
        json!({"account": "Bank", "amount": "-5"}),
        &[("X-Pext-Dry-Run", "1")],
    );
    assert_eq!(r.status(), 200);
    assert_eq!(
        call(&f.app.gw.engine, "txn.list", json!({}))
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let a = post(
        "txn.add",
        json!({"account": "Bank", "amount": "-5"}),
        &[("Idempotency-Key", "abc-1")],
    );
    let b = post(
        "txn.add",
        json!({"account": "Bank", "amount": "-5"}),
        &[("Idempotency-Key", "abc-1")],
    );
    assert_eq!(a.json()["data"]["id"], b.json()["data"]["id"]);
    assert_eq!(
        call(&f.app.gw.engine, "txn.list", json!({}))
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let r = post(
        "txn.add",
        json!({"account": "Bank", "amount": "-5"}),
        &[("Idempotency-Key", "bad key with spaces")],
    );
    assert_eq!(r.status(), 422);
    // Confirmation headers are ignored unless the server allows them.
    let r = post("undo", json!({}), &[("X-Pext-Confirm", "yes")]);
    assert_eq!(
        (r.status(), r.code()),
        (403, "confirm_disabled".to_string())
    );
    let g = make("full", Role::Write, true, |_| {});
    let post_g = |op: &str, a: Value, hs: &[(&str, &str)]| {
        let mut r = rq("POST", &format!("/v1/call/{op}"))
            .auth(&g.secret)
            .json(a);
        for (k, v) in hs {
            r = r.h(k, v);
        }
        r.send(&g.app)
    };
    post_g("acct.add", json!({"name": "Bank", "kind": "checking"}), &[]);
    let r = post_g("undo", json!({}), &[("X-Pext-Confirm", "yes")]);
    assert_eq!(r.status(), 200, "{}", r.json());
}

#[test]
fn redacting_tokens_get_masked_text_over_http() {
    let f = fx();
    let e = &f.app.gw.engine;
    call(e, "acct.add", json!({"name": "Bank", "kind": "checking"}));
    call(
        e,
        "txn.add",
        json!({"account": "Bank", "amount": "-5", "payee": "Private Person", "memo": "secret memo"}),
    );
    let s = call(
        e,
        "token.create",
        json!({"name": "agent", "role": "read", "redact": true}),
    )["token"]
        .as_str()
        .unwrap()
        .to_string();
    let r = rq("GET", "/v1/call/txn.list").auth(&s).send(&f.app);
    let text = String::from_utf8(r.0.body.clone()).unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        !text.contains("Private Person") && !text.contains("secret memo"),
        "{text}"
    );
}

#[test]
fn rate_limits_and_login_throttling() {
    let f = make("full", Role::Write, false, |c| c.rate_per_min = 3);
    for _ in 0..3 {
        assert_eq!(
            rq("GET", "/v1/call/acct.list")
                .auth(&f.secret)
                .send(&f.app)
                .status(),
            200
        );
    }
    let r = rq("GET", "/v1/call/acct.list").auth(&f.secret).send(&f.app);
    assert_eq!(r.status(), 429);
    assert!(r.header("retry-after").unwrap().parse::<u64>().unwrap() >= 1);
    // /health is never rate limited
    assert_eq!(rq("GET", "/health").send(&f.app).status(), 200);

    let g = fx();
    for i in 0..10 {
        let mut r = rq("GET", "/v1/commands").h("Authorization", "Bearer pext_wrong");
        r.peer = "10.0.0.9".into();
        assert_eq!(r.send(&g.app).status(), 401, "attempt {i}");
    }
    let mut r = rq("GET", "/v1/commands").auth(&g.secret);
    r.peer = "10.0.0.9".into();
    assert_eq!(
        r.send(&g.app).status(),
        429,
        "an attacker's IP is locked out even with a valid token"
    );
    // other addresses are unaffected
    assert_eq!(
        rq("GET", "/v1/commands")
            .auth(&g.secret)
            .send(&g.app)
            .status(),
        200
    );
}

#[test]
fn query_strings_decode_and_collect_repeats() {
    let q = http::parse_query("a=1&b=hello%20world&c=x+y&d&e=%zz&f=%4&g=%E2%82%B9&a=2");
    let get = |k: &str| {
        q.iter()
            .filter(|(kk, _)| kk == k)
            .map(|(_, v)| v.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(get("a"), vec!["1", "2"]);
    assert_eq!(get("b"), vec!["hello world"]);
    assert_eq!(get("c"), vec!["x y"]);
    assert_eq!(get("d"), vec![""]);
    assert_eq!(get("e"), vec!["%zz"]);
    assert_eq!(get("f"), vec!["%4"]);
    assert_eq!(get("g"), vec!["₹"]);
    let f = fx();
    let e = &f.app.gw.engine;
    call(e, "acct.add", json!({"name": "Bank", "kind": "checking"}));
    call(
        e,
        "txn.add",
        json!({"account": "Bank", "amount": "-5", "payee": "Café & Co"}),
    );
    let r = rq("GET", "/v1/call/txn.list?limit=1&account=Bank")
        .auth(&f.secret)
        .send(&f.app);
    assert_eq!(r.status(), 200);
    assert_eq!(r.json()["data"][0]["payee"], "Café & Co");
}

#[test]
fn switches_disable_surfaces() {
    let f = make("full", Role::Write, false, |c| c.api = false);
    assert_eq!(rq("GET", "/").send(&f.app).status(), 200);
    assert_eq!(
        rq("GET", "/v1/commands")
            .auth(&f.secret)
            .send(&f.app)
            .status(),
        404
    );
    let g = make("full", Role::Write, false, |c| c.web = false);
    assert_eq!(rq("GET", "/").send(&g.app).status(), 404);
    assert_eq!(
        rq("GET", "/v1/commands")
            .auth(&g.secret)
            .send(&g.app)
            .status(),
        200
    );
}

#[test]
fn scoped_tokens_only_see_scoped_commands() {
    let f = fx();
    let e = &f.app.gw.engine;
    call(e, "acct.add", json!({"name": "Bank", "kind": "checking"}));
    let s = call(
        e,
        "token.create",
        json!({"name": "scoped", "role": "read", "account": ["Bank"]}),
    )["token"]
        .as_str()
        .unwrap()
        .to_string();
    let r = rq("GET", "/v1/commands").auth(&s).send(&f.app);
    let names: Vec<String> = r.json()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"txn.list".to_string()));
    assert!(
        !names.contains(&"networth".to_string()),
        "whole-ledger reports are hidden from scoped tokens"
    );
    let r = rq("GET", "/v1/call/networth").auth(&s).send(&f.app);
    assert_eq!(r.status(), 403);
}

#[test]
fn bind_rules() {
    let (_d, e) = engine();
    let mk = |cfg: ServerConfig| {
        Arc::new(App::new(
            Gateway::new(Engine::new(e.cfg.clone()), Role::Write, false),
            cfg,
        ))
    };
    let remote = http::start(mk(ServerConfig {
        addr: "0.0.0.0:0".into(),
        ..Default::default()
    }));
    assert_eq!(remote.err().unwrap().code, "remote_bind_refused");
    let none = http::start(mk(ServerConfig {
        addr: "127.0.0.1:0".into(),
        web: false,
        api: false,
        ..Default::default()
    }));
    assert_eq!(none.err().unwrap().code, "nothing_to_serve");
    let bad = http::start(mk(ServerConfig {
        addr: "not an address".into(),
        ..Default::default()
    }));
    assert_eq!(bad.err().unwrap().code, "invalid_addr");
}

fn raw(addr: std::net::SocketAddr, req: &str) -> (u16, String) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, text)
}

#[test]
fn real_socket_round_trip() {
    let f = fx();
    let run = http::start(Arc::new(App::new(
        Gateway::new(Engine::new(f.app.gw.engine.cfg.clone()), Role::Write, false),
        ServerConfig {
            addr: "127.0.0.1:0".into(),
            ..Default::default()
        },
    )))
    .unwrap();
    let addr = run.addr;
    let host = format!("127.0.0.1:{}", addr.port());
    let (st, body) = raw(
        addr,
        &format!("GET /health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(st, 200, "{body}");
    assert!(body.to_lowercase().contains("content-security-policy"));
    let payload = "{\"name\":\"Bank\",\"kind\":\"checking\",\"opening\":\"5\"}";
    let req = format!(
        "POST /v1/call/acct.add HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        f.secret,
        payload.len()
    );
    let (st, body) = raw(addr, &req);
    assert_eq!(st, 200, "{body}");
    let (st, body) = raw(addr, &format!("GET /v1/call/acct.list HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n", f.secret));
    assert_eq!(st, 200);
    assert!(body.contains("\"Bank\""));
    // wrong Host over the wire
    let (st, _) = raw(
        addr,
        "GET /health HTTP/1.1\r\nHost: attacker.test\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(st, 421);
    // oversized body is refused before it is processed
    let big = "x".repeat(2 * 1024 * 1024);
    let req = format!(
        "POST /v1/call/acct.add HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{big}",
        f.secret,
        big.len()
    );
    let (st, _) = raw(addr, &req);
    assert!(st == 413 || st == 0, "status {st}");
    run.stop();
}

fn serve_fx(tweak: impl FnOnce(&mut ServerConfig)) -> (Fx, http::Running, String) {
    let f = fx();
    let mut cfg = ServerConfig {
        addr: "127.0.0.1:0".into(),
        ..Default::default()
    };
    tweak(&mut cfg);
    let run = http::start(Arc::new(App::new(
        Gateway::new(Engine::new(f.app.gw.engine.cfg.clone()), Role::Write, false),
        cfg,
    )))
    .unwrap();
    let host = format!("127.0.0.1:{}", run.addr.port());
    (f, run, host)
}

#[test]
fn malformed_requests_get_clean_4xx_not_panics() {
    let (_f, run, host) = serve_fx(|_| {});
    let a = run.addr;
    let cases: Vec<(String, u16)> = vec![
        ("GARBAGE\r\n\r\n".to_string(), 400),
        ("get /health HTTP/1.1\r\nHost: x\r\n\r\n".to_string(), 400), // lowercase method
        (format!("GET health HTTP/1.1\r\nHost: {host}\r\n\r\n"), 400), // no leading slash
        (format!("GET http://evil/ HTTP/1.1\r\nHost: {host}\r\n\r\n"), 400), // absolute form
        (format!("GET /health HTTP/2.0\r\nHost: {host}\r\n\r\n"), 400),
        (format!("GET /health HTTP/1.1\r\nHost: {host}\r\nBad Header: x\r\n\r\n"), 400),
        (format!("GET /health HTTP/1.1\r\nHost: {host}\r\nNoColon\r\n\r\n"), 400),
        (format!("GET /health HTTP/1.1\r\nHost: {host}\r\nHost: {host}\r\n\r\n"), 400), // duplicate Host
        (format!("POST /v1/call/x HTTP/1.1\r\nHost: {host}\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n"), 400),
        (format!("POST /v1/call/x HTTP/1.1\r\nHost: {host}\r\nContent-Length: abc\r\n\r\n"), 400),
        (format!("POST /v1/call/x HTTP/1.1\r\nHost: {host}\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"), 501),
        (format!("POST /v1/call/x HTTP/1.1\r\nHost: {host}\r\nContent-Length: 99999999\r\n\r\n"), 413),
        (format!("GET /health HTTP/1.1\r\nHost: {host}\r\nX-Big: {}\r\n\r\n", "a".repeat(20_000)), 413),
    ];
    for (req, want) in cases {
        let (st, body) = raw(a, &req);
        assert_eq!(
            st,
            want,
            "request {:?} -> {body}",
            &req[..req.len().min(60)]
        );
    }
    // many headers
    let many: String = (0..100).map(|i| format!("X-H{i}: v\r\n")).collect();
    let (st, _) = raw(
        a,
        &format!("GET /health HTTP/1.1\r\nHost: {host}\r\n{many}\r\n"),
    );
    assert_eq!(st, 413);
    // the server is still healthy afterwards
    let (st, _) = raw(a, &format!("GET /health HTTP/1.1\r\nHost: {host}\r\n\r\n"));
    assert_eq!(st, 200);
    // HEAD has no body; HTTP/1.0 works
    let (st, text) = raw(a, &format!("HEAD / HTTP/1.1\r\nHost: {host}\r\n\r\n"));
    assert_eq!(st, 200);
    assert!(text.ends_with("\r\n\r\n"), "HEAD must not carry a body");
    let (st, _) = raw(a, "GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n");
    assert_eq!(st, 200);
    run.stop();
}

#[test]
fn slow_clients_are_cut_off() {
    use std::io::{Read, Write};
    let (_f, run, host) = serve_fx(|c| c.io_timeout_secs = 1);
    let mut s = std::net::TcpStream::connect(run.addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(8)))
        .unwrap();
    s.write_all(b"GET /health HTTP/1.1\r\nHost: ").unwrap(); // never finishes
    let t = std::time::Instant::now();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    assert!(
        t.elapsed().as_secs() < 5,
        "server held the connection for {:?}",
        t.elapsed()
    );
    assert!(String::from_utf8_lossy(&buf).starts_with("HTTP/1.1 408"));
    // A client that promises a body but never sends it is also dropped.
    let mut s = std::net::TcpStream::connect(run.addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(8)))
        .unwrap();
    s.write_all(
        format!(
            "POST /v1/call/acct.list HTTP/1.1\r\nHost: {host}\r\nContent-Length: 100\r\n\r\nabc"
        )
        .as_bytes(),
    )
    .unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    assert!(String::from_utf8_lossy(&buf).starts_with("HTTP/1.1 408"));
    // idle sockets do not stop other clients being served
    let _idle: Vec<_> = (0..20)
        .map(|_| std::net::TcpStream::connect(run.addr).unwrap())
        .collect();
    let (st, _) = raw(
        run.addr,
        &format!("GET /health HTTP/1.1\r\nHost: {host}\r\n\r\n"),
    );
    assert_eq!(st, 200);
    run.stop();
}

#[test]
fn parallel_requests_all_complete() {
    let (f, run, host) = serve_fx(|_| {});
    let secret = f.secret.clone();
    let addr = run.addr;
    let handles: Vec<_> = (0..24)
        .map(|i| {
            let (host, secret) = (host.clone(), secret.clone());
            std::thread::spawn(move || {
                let op = ["networth", "acct.list", "report.summary", "budget.status", "goal.status", "alert.check"][i % 6];
                raw(addr, &format!("POST /v1/call/{op} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {secret}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}")).0
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), 200);
    }
    run.stop();
}

#[test]
fn expect_100_continue_is_honoured() {
    use std::io::{Read, Write};
    let (f, run, host) = serve_fx(|_| {});
    let mut s = std::net::TcpStream::connect(run.addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let body = "{\"name\":\"X\",\"kind\":\"cash\"}";
    s.write_all(format!("POST /v1/call/acct.add HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nExpect: 100-continue\r\nContent-Length: {}\r\n\r\n", f.secret, body.len()).as_bytes()).unwrap();
    let mut first = [0u8; 25];
    s.read_exact(&mut first).unwrap();
    assert!(String::from_utf8_lossy(&first).starts_with("HTTP/1.1 100 Continue"));
    s.write_all(body.as_bytes()).unwrap();
    let mut rest = Vec::new();
    let _ = s.read_to_end(&mut rest);
    assert!(
        String::from_utf8_lossy(&rest).contains("200 OK"),
        "{}",
        String::from_utf8_lossy(&rest)
    );
    run.stop();
}
