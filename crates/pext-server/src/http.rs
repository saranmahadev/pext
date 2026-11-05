//! Local HTTP API and web dashboard.
//!
//! Security model: loopback-only by default; every `/v1` call needs a bearer
//! token (cookies are never used, so CSRF does not apply); the Host header is
//! checked to defeat DNS rebinding; failed logins and per-token request rates
//! are limited; bodies and responses are size-capped; and the dashboard sets
//! a strict Content-Security-Policy and renders data with `textContent` only.

use crate::gateway::{check_size, err_envelope, ok_envelope, tool_summary, CallFlags, Gateway};
use pext_core::{Error, ErrorKind, Result};
use pext_engine::registry::{self, Kind};
use pext_engine::Actor;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const MAX_BODY: usize = 1024 * 1024;
const WINDOW: Duration = Duration::from_secs(60);
const FAILED_AUTH_PER_WINDOW: u32 = 10;
const DASHBOARD: &str = include_str!("dashboard.html");

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub addr: String,
    pub web: bool,
    pub api: bool,
    /// Allow binding to a non-loopback address (put TLS in front of it).
    pub allow_remote: bool,
    /// Requests per minute per token.
    pub rate_per_min: u32,
    /// Seconds a client may take to send its request (and to read the reply).
    pub io_timeout_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            addr: "127.0.0.1:7878".into(),
            web: true,
            api: true,
            allow_remote: false,
            rate_per_min: 300,
            io_timeout_secs: 10,
        }
    }
}

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    /// Header names lower-cased.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub peer: String,
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Default)]
struct Limiter {
    windows: Mutex<HashMap<String, (Instant, u32)>>,
}

impl Limiter {
    /// Count one hit; `Err(retry_after_secs)` when over the limit.
    fn hit(&self, key: &str, limit: u32) -> std::result::Result<(), u64> {
        let mut m = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if m.len() > 10_000 {
            m.retain(|_, (t, _)| now.duration_since(*t) < WINDOW);
        }
        let e = m.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(e.0) >= WINDOW {
            *e = (now, 0);
        }
        if e.1 >= limit {
            return Err((WINDOW - now.duration_since(e.0)).as_secs().max(1));
        }
        e.1 += 1;
        Ok(())
    }
    fn count(&self, key: &str) -> u32 {
        let m = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        match m.get(key) {
            Some((t, n)) if Instant::now().duration_since(*t) < WINDOW => *n,
            _ => 0,
        }
    }
}

pub struct App {
    pub gw: Gateway,
    pub cfg: ServerConfig,
    limiter: Limiter,
}

fn json_response(status: u16, v: &Value) -> Response {
    Response {
        status,
        content_type: "application/json",
        headers: vec![("Cache-Control".into(), "no-store".into())],
        body: v.to_string().into_bytes(),
    }
}

fn error_response(e: &Error, status: u16) -> Response {
    json_response(status, &err_envelope(e))
}

fn status_for(e: &Error) -> u16 {
    match e.kind {
        ErrorKind::Usage => 400,
        ErrorKind::Validation => 422,
        ErrorKind::NotFound => 404,
        ErrorKind::Conflict => 409,
        ErrorKind::Auth => 403,
        ErrorKind::Io => 500,
        ErrorKind::Integrity => 500,
        ErrorKind::Internal => 500,
    }
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match h {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (pct_decode(k), pct_decode(v)),
            None => (pct_decode(p), String::new()),
        })
        .collect()
}

fn loopback_host(host: &str) -> bool {
    let h = host.trim().to_ascii_lowercase();
    let name = if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or("").to_string()
    } else {
        h.split(':').next().unwrap_or("").to_string()
    };
    matches!(name.as_str(), "localhost" | "127.0.0.1" | "::1")
}

fn base64(bytes: &[u8]) -> String {
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

fn hash_b64(s: &str) -> String {
    let hex = pext_engine::audit::sha256_hex(s);
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0))
        .collect();
    base64(&bytes)
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> &'a str {
    match s.find(open) {
        Some(a) => {
            let from = a + open.len();
            match s[from..].find(close) {
                Some(b) => &s[from..from + b],
                None => "",
            }
        }
        None => "",
    }
}

/// CSP that allows only the dashboard's own inline script and style (by hash).
pub fn dashboard_csp() -> &'static str {
    static CSP: OnceLock<String> = OnceLock::new();
    CSP.get_or_init(|| {
        let script = between(DASHBOARD, "<script>", "</script>");
        let style = between(DASHBOARD, "<style>", "</style>");
        format!(
            "default-src 'none'; script-src 'sha256-{}'; style-src 'sha256-{}'; connect-src 'self'; \
             img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            hash_b64(script),
            hash_b64(style)
        )
    })
}

impl App {
    pub fn new(gw: Gateway, cfg: ServerConfig) -> App {
        App {
            gw,
            cfg,
            limiter: Limiter::default(),
        }
    }

    fn secure(&self, mut r: Response, page: bool) -> Response {
        let csp = if page {
            dashboard_csp()
        } else {
            "default-src 'none'; frame-ancestors 'none'"
        };
        r.headers.extend([
            ("X-Content-Type-Options".to_string(), "nosniff".to_string()),
            ("X-Frame-Options".to_string(), "DENY".to_string()),
            ("Referrer-Policy".to_string(), "no-referrer".to_string()),
            (
                "Cross-Origin-Resource-Policy".to_string(),
                "same-origin".to_string(),
            ),
            ("Content-Security-Policy".to_string(), csp.to_string()),
        ]);
        r
    }

    pub fn handle(&self, req: &Request) -> Response {
        let page = req.path == "/" && self.cfg.web && matches!(req.method.as_str(), "GET" | "HEAD");
        let r = self.route(req);
        self.secure(r, page)
    }

    fn route(&self, req: &Request) -> Response {
        if !self.cfg.allow_remote {
            match req.headers.get("host") {
                Some(h) if loopback_host(h) => {}
                _ => {
                    return error_response(
                        &Error::usage("bad_host", "unexpected Host header")
                            .with_hint("use http://localhost or http://127.0.0.1"),
                        421,
                    )
                }
            }
        }
        match (req.method.as_str(), req.path.as_str()) {
            ("GET" | "HEAD", "/") if self.cfg.web => Response {
                status: 200,
                content_type: "text/html; charset=utf-8",
                headers: vec![("Cache-Control".into(), "no-store".into())],
                body: if req.method == "HEAD" {
                    vec![]
                } else {
                    DASHBOARD.as_bytes().to_vec()
                },
            },
            ("GET", "/health") => json_response(
                200,
                &json!({"ok": true, "version": pext_engine::ops::system::VERSION}),
            ),
            (_, p) if p.starts_with("/v1/") && self.cfg.api => self.api(req, &p[4..]),
            _ => error_response(
                &Error::not_found(
                    "not_found",
                    format!("no route for {} {}", req.method, req.path),
                ),
                404,
            ),
        }
    }

    fn bearer(req: &Request) -> Option<String> {
        let h = req.headers.get("authorization")?;
        let (scheme, rest) = h.split_once(' ')?;
        if scheme.eq_ignore_ascii_case("bearer") && !rest.trim().is_empty() {
            Some(rest.trim().to_string())
        } else {
            None
        }
    }

    fn authenticate(&self, req: &Request) -> std::result::Result<Actor, Response> {
        let fail_key = format!("authfail:{}", req.peer);
        if self.limiter.count(&fail_key) >= FAILED_AUTH_PER_WINDOW {
            let mut r = error_response(
                &Error::auth(
                    "too_many_failures",
                    "too many failed attempts; wait a minute",
                ),
                429,
            );
            r.headers.push(("Retry-After".into(), "60".into()));
            return Err(r);
        }
        let unauth = |code: &str, msg: String, limiter: &Limiter, key: &str| {
            let _ = limiter.hit(key, u32::MAX);
            let mut r = error_response(&Error::auth(code, msg), 401);
            r.headers.push(("WWW-Authenticate".into(), "Bearer".into()));
            r
        };
        let Some(secret) = Self::bearer(req) else {
            return Err(unauth(
                "unauthenticated",
                "send an access token as 'Authorization: Bearer <token>'".into(),
                &self.limiter,
                &fail_key,
            ));
        };
        match self.gw.authenticate(&secret) {
            Ok(a) => {
                let key = format!("tok:{}", a.token_id.unwrap_or(0));
                if let Err(wait) = self.limiter.hit(&key, self.cfg.rate_per_min) {
                    let mut r = error_response(
                        &Error::auth("rate_limited", "too many requests; slow down"),
                        429,
                    );
                    r.headers.push(("Retry-After".into(), wait.to_string()));
                    return Err(r);
                }
                Ok(a)
            }
            Err(e) => Err(unauth("invalid_token", e.message, &self.limiter, &fail_key)),
        }
    }

    fn api(&self, req: &Request, rest: &str) -> Response {
        let actor = match self.authenticate(req) {
            Ok(a) => a,
            Err(r) => return r,
        };
        let parts: Vec<&str> = rest.split('/').collect();
        match (req.method.as_str(), parts.as_slice()) {
            ("GET", ["commands"]) => {
                let list: Vec<Value> = self
                    .gw
                    .tools(&actor)
                    .into_iter()
                    .map(tool_summary)
                    .collect();
                json_response(
                    200,
                    &json!({"schema": "pext/v1", "ok": true, "data": list, "meta": {"count": list.len()}}),
                )
            }
            ("GET", ["schema", op]) => match registry::find(op) {
                Some(s) if self.gw.visible(s, &actor) => json_response(
                    200,
                    &json!({"schema": "pext/v1", "ok": true, "data": registry::input_schema(s), "meta": {}}),
                ),
                _ => error_response(
                    &Error::not_found(
                        "unknown_command",
                        format!("no command '{op}' for this caller"),
                    ),
                    404,
                ),
            },
            ("GET" | "POST", ["call", op]) => self.call(req, op, &actor),
            (_, ["commands"] | ["schema", _] | ["call", _]) => {
                let mut r = error_response(
                    &Error::usage(
                        "method_not_allowed",
                        format!("{} is not allowed here", req.method),
                    ),
                    405,
                );
                r.headers.push(("Allow".into(), "GET, POST".into()));
                r
            }
            _ => error_response(
                &Error::not_found(
                    "not_found",
                    format!("no route for {} {}", req.method, req.path),
                ),
                404,
            ),
        }
    }

    fn call(&self, req: &Request, op: &str, actor: &Actor) -> Response {
        let Some(spec) = registry::find(op) else {
            return error_response(
                &Error::not_found("unknown_command", format!("unknown command '{op}'")),
                404,
            );
        };
        let mut args = Map::new();
        if req.method == "GET" {
            if spec.mutating {
                let mut r = error_response(
                    &Error::usage("method_not_allowed", "this command changes data: use POST"),
                    405,
                );
                r.headers.push(("Allow".into(), "POST".into()));
                return r;
            }
            for (k, v) in &req.query {
                let multi = spec
                    .params
                    .iter()
                    .find(|p| p.name == k)
                    .map(|p| p.multi)
                    .unwrap_or(false);
                let val = Value::String(v.clone());
                if multi {
                    match args.get_mut(k) {
                        Some(Value::Array(a)) => a.push(val),
                        _ => {
                            args.insert(k.clone(), Value::Array(vec![val]));
                        }
                    }
                } else {
                    args.insert(k.clone(), val);
                }
            }
            // Bool params given as ?flag=true
            for p in &spec.params {
                if p.kind == Kind::Bool {
                    if let Some(Value::String(s)) = args.get(p.name) {
                        let b = matches!(s.as_str(), "" | "1" | "true" | "yes");
                        args.insert(p.name.into(), Value::Bool(b));
                    }
                }
            }
        } else if !req.body.is_empty() {
            let ct = req
                .headers
                .get("content-type")
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            if !ct.starts_with("application/json") {
                return error_response(
                    &Error::usage(
                        "unsupported_media_type",
                        "send the body as application/json",
                    ),
                    415,
                );
            }
            match serde_json::from_slice::<Value>(&req.body) {
                Ok(Value::Object(m)) => args = m,
                Ok(_) => {
                    return error_response(
                        &Error::usage("invalid_json", "the body must be a JSON object"),
                        400,
                    )
                }
                Err(e) => {
                    return error_response(
                        &Error::usage("invalid_json", format!("invalid JSON: {e}")),
                        400,
                    )
                }
            }
        }
        let truthy = |h: &str| {
            req.headers
                .get(h)
                .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(false)
        };
        let flags = CallFlags {
            dry_run: truthy("x-pext-dry-run"),
            confirm: truthy("x-pext-confirm"),
            idempotency_key: req.headers.get("idempotency-key").cloned(),
        };
        match self.gw.call(op, args, &flags, actor) {
            Ok(out) => {
                let body = ok_envelope(&out);
                let text = body.to_string();
                match check_size(&text) {
                    Ok(()) => Response {
                        status: 200,
                        content_type: "application/json",
                        headers: vec![("Cache-Control".into(), "no-store".into())],
                        body: text.into_bytes(),
                    },
                    Err(e) => error_response(&e, 413),
                }
            }
            Err(e) => error_response(&e, status_for(&e)),
        }
    }
}

// ---- socket layer ---------------------------------------------------------------
//
// A deliberately small HTTP/1.1 listener: one request per connection
// (`Connection: close`), per-connection threads under a hard cap, read/write
// timeouts (slowloris), header and body size limits, and no support for
// chunked bodies (avoids request-smuggling ambiguity).

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_CONNECTIONS: usize = 64;

pub struct Running {
    pub addr: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Running {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
    /// Block until the process is interrupted.
    pub fn wait(mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn check_bind(cfg: &ServerConfig) -> Result<std::net::SocketAddr> {
    let addr = cfg
        .addr
        .to_socket_addrs()
        .map_err(|e| {
            Error::usage(
                "invalid_addr",
                format!("cannot use address '{}': {e}", cfg.addr),
            )
        })?
        .next()
        .ok_or_else(|| {
            Error::usage("invalid_addr", format!("cannot use address '{}'", cfg.addr))
        })?;
    if !addr.ip().is_loopback() && !cfg.allow_remote {
        return Err(Error::usage(
            "remote_bind_refused",
            format!("{} is not a loopback address", addr.ip()),
        )
        .with_hint("pext serves your finances over plain HTTP: keep it on 127.0.0.1, or pass --allow-remote and put a TLS proxy in front"));
    }
    if !cfg.web && !cfg.api {
        return Err(Error::usage(
            "nothing_to_serve",
            "enable at least one of --web and --api",
        ));
    }
    Ok(addr)
}

pub fn start(app: Arc<App>) -> Result<Running> {
    let addr = check_bind(&app.cfg)?;
    let listener = std::net::TcpListener::bind(addr)
        .map_err(|e| Error::io("bind_failed", format!("cannot listen on {addr}: {e}")))?;
    let bound = listener.local_addr().unwrap_or(addr);
    listener
        .set_nonblocking(true)
        .map_err(|e| Error::io("bind_failed", format!("cannot configure the listener: {e}")))?;
    let stop = Arc::new(AtomicBool::new(false));
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let flag = stop.clone();
    let handle = std::thread::spawn(move || {
        while !flag.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, peer)) => {
                    let _ = stream.set_nonblocking(false);
                    if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                        active.fetch_sub(1, Ordering::SeqCst);
                        let _ = refuse(stream, 503, "busy", "too many open connections");
                        continue;
                    }
                    let (app, active) = (app.clone(), active.clone());
                    std::thread::spawn(move || {
                        let _ = serve_connection(&app, stream, peer.ip().to_string());
                        active.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    });
    Ok(Running {
        addr: bound,
        stop,
        handle: Some(handle),
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        421 => "Misdirected Request",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

fn write_response(
    stream: &mut std::net::TcpStream,
    r: &Response,
    head_only: bool,
) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = format!("HTTP/1.1 {} {}\r\n", r.status, reason(r.status));
    out.push_str(&format!(
        "Content-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        r.content_type,
        r.body.len()
    ));
    for (k, v) in &r.headers {
        // Header values come from our own code, but never let CR/LF through.
        if !k.contains(['\r', '\n']) && !v.contains(['\r', '\n']) {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    out.push_str("\r\n");
    stream.write_all(out.as_bytes())?;
    if !head_only {
        stream.write_all(&r.body)?;
    }
    stream.flush()
}

fn refuse(
    mut stream: std::net::TcpStream,
    status: u16,
    code: &str,
    msg: &str,
) -> std::io::Result<()> {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let r = json_response(status, &err_envelope(&Error::usage(code, msg)));
    write_response(&mut stream, &r, false)
}

enum ReadError {
    Timeout,
    Closed,
    TooLarge,
    Bad(&'static str),
    Unsupported(&'static str),
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn read_request(
    stream: &mut std::net::TcpStream,
    peer: String,
) -> std::result::Result<Request, ReadError> {
    use std::io::Read;
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(ReadError::TooLarge);
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(ReadError::Closed),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(ReadError::Timeout)
            }
            Err(_) => return Err(ReadError::Closed),
        }
    };
    if head_end > MAX_HEADER_BYTES {
        return Err(ReadError::TooLarge);
    }
    let head = std::str::from_utf8(&buf[..head_end])
        .map_err(|_| ReadError::Bad("headers must be ASCII/UTF-8"))?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let (method, target, version) = (
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
    );
    if parts.next().is_some()
        || method.is_empty()
        || !method.bytes().all(|b| b.is_ascii_uppercase())
    {
        return Err(ReadError::Bad("malformed request line"));
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(ReadError::Bad("unsupported HTTP version"));
    }
    if !target.starts_with('/') || target.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return Err(ReadError::Bad("malformed request target"));
    }
    let mut headers: HashMap<String, String> = HashMap::new();
    let mut count = 0;
    for line in lines {
        count += 1;
        if count > MAX_HEADERS {
            return Err(ReadError::TooLarge);
        }
        let (k, v) = line
            .split_once(':')
            .ok_or(ReadError::Bad("malformed header"))?;
        if k.is_empty()
            || !k
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(ReadError::Bad("malformed header name"));
        }
        let k = k.to_ascii_lowercase();
        let v = v.trim().to_string();
        if let Some(old) = headers.get(&k) {
            if k == "content-length" && *old != v {
                return Err(ReadError::Bad("conflicting Content-Length headers"));
            }
            if k == "host" || k == "authorization" {
                return Err(ReadError::Bad("duplicate header"));
            }
        }
        headers.insert(k, v);
    }
    if headers.contains_key("transfer-encoding") {
        return Err(ReadError::Unsupported(
            "chunked and other transfer encodings are not supported; send Content-Length",
        ));
    }
    let len: usize = match headers.get("content-length") {
        None => 0,
        Some(v) => v
            .parse()
            .map_err(|_| ReadError::Bad("invalid Content-Length"))?,
    };
    if len > MAX_BODY {
        return Err(ReadError::TooLarge);
    }
    let mut body: Vec<u8> = buf[head_end + 4..].to_vec();
    if body.len() > len {
        body.truncate(len); // one request per connection; ignore anything after it
    }
    if body.len() < len {
        use std::io::Write;
        if headers
            .get("expect")
            .map(|v| v.eq_ignore_ascii_case("100-continue"))
            .unwrap_or(false)
        {
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        }
        let mut rest = vec![0u8; len - body.len()];
        let mut got = 0;
        while got < rest.len() {
            match stream.read(&mut rest[got..]) {
                Ok(0) => return Err(ReadError::Closed),
                Ok(n) => got += n,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(ReadError::Timeout)
                }
                Err(_) => return Err(ReadError::Closed),
            }
        }
        body.extend_from_slice(&rest);
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target.to_string(), vec![]),
    };
    Ok(Request {
        method: method.to_string(),
        path,
        query,
        headers,
        body,
        peer,
    })
}

fn serve_connection(
    app: &App,
    mut stream: std::net::TcpStream,
    peer: String,
) -> std::io::Result<()> {
    let t = Duration::from_secs(app.cfg.io_timeout_secs.max(1));
    stream.set_read_timeout(Some(t))?;
    stream.set_write_timeout(Some(t))?;
    let _ = stream.set_nodelay(true);
    let response = match read_request(&mut stream, peer) {
        Ok(req) => {
            let head_only = req.method == "HEAD";
            let r = app.handle(&req);
            return write_response(&mut stream, &r, head_only);
        }
        Err(ReadError::Closed) => return Ok(()),
        Err(ReadError::Timeout) => app.secure(
            error_response(
                &Error::usage("request_timeout", "the request took too long"),
                408,
            ),
            false,
        ),
        Err(ReadError::TooLarge) => app.secure(
            error_response(
                &Error::usage("request_too_large", "headers or body exceed the limits"),
                413,
            ),
            false,
        ),
        Err(ReadError::Bad(m)) => {
            app.secure(error_response(&Error::usage("bad_request", m), 400), false)
        }
        Err(ReadError::Unsupported(m)) => app.secure(
            error_response(&Error::usage("not_supported", m), 501),
            false,
        ),
    };
    write_response(&mut stream, &response, false)
}
