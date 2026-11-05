//! Plugins: external programs that speak JSON over stdin/stdout. A plugin never
//! touches the database; it receives a small request and returns data that the
//! host validates and applies through its own operations (so every change is
//! still audited and reversible).
//!
//! Safety model (honest scope): a plugin is pinned by the SHA-256 of its
//! executable and re-verified before every run; it runs with an empty
//! environment, a hard timeout and a 1 MiB output cap, and only an admin can
//! install or run one. Declared `permissions` are recorded and shown so people
//! can review them, but they are NOT enforced by an OS sandbox: only install
//! plugins you trust.

use crate::args::Args;
use crate::ctx::{sv, Ctx};
use crate::ops::invest::{find, load_instruments, upsert_price};
use crate::registry::{OpSpec, Param};
use pext_core::invest::parse_e8;
use pext_core::{Error, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const PROTOCOL: u32 = 1;
const MAX_OUTPUT: usize = 1024 * 1024;
const MAX_STDERR: usize = 4096;
const MAX_PRICES: usize = 10_000;
const PERMISSIONS: &[&str] = &["network", "read_files", "write_files"];

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn file_sha256(p: &Path) -> Result<String> {
    let bytes = std::fs::read(p).map_err(|e| {
        Error::io(
            "plugin_unreadable",
            format!("cannot read {}: {e}", p.display()),
        )
    })?;
    Ok(crate::crypto::hex(&Sha256::digest(&bytes)))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

struct Plugin {
    id: i64,
    name: String,
    version: String,
    kind: String,
    exe: PathBuf,
    args: Vec<String>,
    sha256: String,
    permissions: Vec<String>,
}

fn load_row(r: &serde_json::Map<String, Value>) -> Plugin {
    let cmd: Vec<String> =
        serde_json::from_str(r["command"].as_str().unwrap_or("[]")).unwrap_or_default();
    let perms: Vec<String> =
        serde_json::from_str(r["permissions"].as_str().unwrap_or("[]")).unwrap_or_default();
    Plugin {
        id: r["id"].as_i64().unwrap_or(0),
        name: r["name"].as_str().unwrap_or_default().into(),
        version: r["version"].as_str().unwrap_or_default().into(),
        kind: r["kind"].as_str().unwrap_or_default().into(),
        exe: PathBuf::from(cmd.first().cloned().unwrap_or_default()),
        args: cmd.into_iter().skip(1).collect(),
        sha256: r["sha256"].as_str().unwrap_or_default().into(),
        permissions: perms,
    }
}

fn find_plugin(ctx: &Ctx, name: &str) -> Result<Plugin> {
    ctx.rows(
        "SELECT * FROM plugins WHERE name = ?1 COLLATE NOCASE",
        &[sv(name)],
    )?
    .first()
    .map(load_row)
    .ok_or_else(|| {
        Error::not_found("plugin_not_found", format!("no plugin named '{name}'"))
            .with_hint("`pext plugin list`")
    })
}

fn describe(p: &Plugin) -> Value {
    json!({
        "id": p.id, "name": p.name, "version": p.version, "kind": p.kind,
        "command": p.exe.display().to_string(), "args": p.args, "sha256": p.sha256, "permissions": p.permissions,
        "exists": p.exe.exists(),
    })
}

fn install(ctx: &Ctx, a: &Args) -> Result<Value> {
    let name = a.str("name")?;
    if !valid_name(&name) {
        return Err(Error::validation(
            "invalid_plugin_name",
            "plugin names use letters, digits, '-' and '_' (max 64)",
        )
        .with_field("name"));
    }
    let raw = a.str("command")?;
    let exe = std::fs::canonicalize(&raw).map_err(|e| {
        Error::validation(
            "plugin_not_found_on_disk",
            format!("cannot find '{raw}': {e}"),
        )
        .with_field("command")
    })?;
    if !is_executable(&exe) {
        return Err(Error::validation(
            "plugin_not_executable",
            format!("{} is not an executable file", exe.display()),
        )
        .with_field("command"));
    }
    if let Some(dir) = &ctx.cfg.plugin_dir {
        let dir = std::fs::canonicalize(dir).map_err(|e| {
            Error::io(
                "plugin_dir_unreadable",
                format!("cannot read the plugin folder: {e}"),
            )
        })?;
        if !exe.starts_with(&dir) {
            return Err(Error::validation(
                "plugin_outside_dir",
                format!("plugins must live inside {}", dir.display()),
            )
            .with_field("command"));
        }
    }
    let sha = file_sha256(&exe)?;
    if let Some(pin) = a.opt_str("sha256") {
        if !pin.eq_ignore_ascii_case(&sha) {
            return Err(Error::integrity(
                "plugin_hash_mismatch",
                "the executable does not match the sha256 you pinned",
            )
            .with_details(json!({"expected": pin, "actual": sha})));
        }
    }
    let kind = a.str("kind")?;
    let mut perms = a.list("permissions");
    perms.sort();
    perms.dedup();
    if let Some(bad) = perms.iter().find(|p| !PERMISSIONS.contains(&p.as_str())) {
        return Err(Error::validation(
            "invalid_permission",
            format!(
                "unknown permission '{bad}' (use {})",
                PERMISSIONS.join(", ")
            ),
        )
        .with_field("permissions"));
    }
    let args = a.list("arg");
    let mut cmd = vec![exe.display().to_string()];
    cmd.extend(args);
    let exists = ctx.opt_i64(
        "SELECT id FROM plugins WHERE name = ?1 COLLATE NOCASE",
        &[sv(&name)],
    )?;
    if exists.is_some() {
        return Err(Error::conflict(
            "plugin_exists",
            format!("a plugin named '{name}' is already installed"),
        )
        .with_hint("remove it first with `pext plugin rm`"));
    }
    let id = ctx.insert(
        "plugins",
        &[
            ("name", sv(&name)),
            ("version", sv(&a.opt_str("release").unwrap_or_default())),
            ("kind", sv(&kind)),
            (
                "command",
                sv(&serde_json::to_string(&cmd).unwrap_or_default()),
            ),
            ("sha256", sv(&sha)),
            (
                "permissions",
                sv(&serde_json::to_string(&perms).unwrap_or_default()),
            ),
            ("created_at", sv(&ctx.now)),
        ],
    )?;
    Ok(describe(&find_plugin(ctx, &name).map_err(|_| {
        Error::internal("plugin_lost", format!("plugin {id} vanished"))
    })?))
}

fn list(ctx: &Ctx, _a: &Args) -> Result<Value> {
    Ok(Value::Array(
        ctx.rows("SELECT * FROM plugins ORDER BY name", &[])?
            .iter()
            .map(|r| describe(&load_row(r)))
            .collect(),
    ))
}

fn rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let p = find_plugin(ctx, &a.str("name")?)?;
    ctx.delete("plugins", p.id)?;
    Ok(json!({ "removed": p.name }))
}

/// Kill the plugin and everything it started. Reader threads are left to end
/// on their own when the pipes close, so a stubborn grandchild can never hang us.
fn stop(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        // SAFETY: plain signal to a process group we created (pgid == child pid).
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Run a plugin once. Returns its parsed JSON output.
fn execute(p: &Plugin, request: &Value, timeout: Duration) -> Result<(Value, u128)> {
    if !p.exe.exists() {
        return Err(Error::not_found(
            "plugin_missing",
            format!("{} no longer exists", p.exe.display()),
        ));
    }
    let now_sha = file_sha256(&p.exe)?;
    if now_sha != p.sha256 {
        return Err(Error::integrity(
            "plugin_modified",
            format!(
                "'{}' has changed since it was installed; refusing to run it",
                p.name
            ),
        )
        .with_hint("review the change, then `pext plugin rm` and install it again")
        .with_details(json!({"installed_sha256": p.sha256, "current_sha256": now_sha})));
    }
    let started = Instant::now();
    let mut cmd = Command::new(&p.exe);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // own group, so a timeout can stop the plugin's children too
    }
    let mut child = cmd
        .args(&p.args)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("PEXT_PLUGIN_PROTOCOL", PROTOCOL.to_string())
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            Error::io(
                "plugin_spawn_failed",
                format!("cannot start '{}': {e}", p.name),
            )
        })?;

    // Feed stdin from a thread so a plugin that never reads cannot block us.
    let mut stdin = child.stdin.take();
    let payload = request.to_string();
    let writer = std::thread::spawn(move || {
        if let Some(mut s) = stdin.take() {
            let _ = s.write_all(payload.as_bytes());
        }
    });
    let overflow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut out = child.stdout.take();
    let flag = overflow.clone();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(o) = out.as_mut() {
            let _ = o.take(MAX_OUTPUT as u64 + 1).read_to_end(&mut buf);
            if buf.len() > MAX_OUTPUT {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        buf
    });
    let mut err = child.stderr.take();
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(e) = err.as_mut() {
            let _ = e.take(MAX_STDERR as u64).read_to_end(&mut buf);
            let mut sink = [0u8; 4096];
            while matches!(e.read(&mut sink), Ok(n) if n > 0) {}
        }
        buf
    });

    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if overflow.load(std::sync::atomic::Ordering::SeqCst) {
                    stop(&mut child);
                    return Err(Error::io(
                        "plugin_output_too_large",
                        format!(
                            "'{}' produced more than {} bytes and was stopped",
                            p.name, MAX_OUTPUT
                        ),
                    ));
                }
                if started.elapsed() >= timeout {
                    stop(&mut child);
                    return Err(Error::io(
                        "plugin_timeout",
                        format!(
                            "'{}' did not finish within {}s and was stopped",
                            p.name,
                            timeout.as_secs()
                        ),
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                return Err(Error::io(
                    "plugin_wait_failed",
                    format!("cannot wait for '{}': {e}", p.name),
                ))
            }
        }
    };
    // Stray children that outlived the plugin must not keep our pipes open.
    #[cfg(unix)]
    unsafe {
        // SAFETY: signal to the process group created for this plugin.
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = writer.join();
    let stdout = reader.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default())
        .trim()
        .to_string();
    let elapsed = started.elapsed().as_millis();
    if stdout.len() > MAX_OUTPUT {
        return Err(Error::io(
            "plugin_output_too_large",
            format!("'{}' produced more than {} bytes", p.name, MAX_OUTPUT),
        ));
    }
    if !status.success() {
        return Err(Error::io(
            "plugin_failed",
            format!(
                "'{}' exited with {}",
                p.name,
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "a signal".into())
            ),
        )
        .with_details(json!({"stderr": stderr})));
    }
    let v: Value = serde_json::from_slice(&stdout).map_err(|e| {
        Error::validation(
            "plugin_bad_output",
            format!("'{}' did not return valid JSON: {e}", p.name),
        )
        .with_details(json!({"stderr": stderr}))
    })?;
    Ok((v, elapsed))
}

fn timeout_arg(a: &Args) -> Result<Duration> {
    let t = a.opt_i64("timeout")?.unwrap_or(10);
    if !(1..=60).contains(&t) {
        return Err(Error::validation(
            "invalid_timeout",
            "timeout must be between 1 and 60 seconds",
        )
        .with_field("timeout"));
    }
    Ok(Duration::from_secs(t as u64))
}

fn run(ctx: &Ctx, a: &Args) -> Result<Value> {
    let p = find_plugin(ctx, &a.str("name")?)?;
    let input: Value = match a.opt_str("input") {
        Some(s) => serde_json::from_str(&s).map_err(|e| {
            Error::validation("invalid_input", format!("input must be JSON: {e}"))
                .with_field("input")
        })?,
        None => Value::Null,
    };
    let req = json!({ "protocol": PROTOCOL, "action": "run", "today": crate::ops::txns::fmt_date(ctx.today), "input": input });
    let (output, ms) = execute(&p, &req, timeout_arg(a)?)?;
    Ok(
        json!({ "plugin": p.name, "kind": p.kind, "elapsed_ms": ms, "untrusted": true, "output": output }),
    )
}

fn price_fetch(ctx: &Ctx, a: &Args) -> Result<Value> {
    let p = find_plugin(ctx, &a.str("plugin")?)?;
    if p.kind != "price_feed" {
        return Err(Error::validation(
            "wrong_plugin_kind",
            format!("'{}' is a {} plugin, not a price_feed", p.name, p.kind),
        )
        .with_field("plugin"));
    }
    let wanted = a.list("symbols");
    let all = load_instruments(ctx, false)?;
    let symbols: Vec<String> = if wanted.is_empty() {
        all.iter().map(|i| i.symbol.clone()).collect()
    } else {
        let mut v = vec![];
        for w in &wanted {
            v.push(find(ctx, w)?.symbol);
        }
        v
    };
    if symbols.is_empty() {
        return Err(Error::validation(
            "no_instruments",
            "there are no instruments to fetch prices for",
        )
        .with_hint("add one with `pext instrument add`"));
    }
    let req = json!({ "protocol": PROTOCOL, "action": "prices", "today": crate::ops::txns::fmt_date(ctx.today), "symbols": symbols });
    let (resp, ms) = execute(&p, &req, timeout_arg(a)?)?;
    let list = resp
        .get("prices")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            Error::validation(
                "plugin_bad_output",
                format!("'{}' must return {{\"prices\": [...]}}", p.name),
            )
        })?;
    if list.len() > MAX_PRICES {
        return Err(Error::validation(
            "plugin_bad_output",
            format!("'{}' returned too many prices (max {MAX_PRICES})", p.name),
        ));
    }
    let mut applied = vec![];
    let mut rejected = vec![];
    for item in list {
        let reject = |why: &str| json!({ "item": item, "reason": why });
        let Some(sym) = item.get("symbol").and_then(|v| v.as_str()) else {
            rejected.push(reject("missing symbol"));
            continue;
        };
        if !symbols.iter().any(|s| s.eq_ignore_ascii_case(sym)) {
            rejected.push(reject("symbol was not requested"));
            continue;
        }
        let price = match item.get("price") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => {
                rejected.push(reject("missing price"));
                continue;
            }
        };
        let e8 = match parse_e8(&price, "price") {
            Ok(v) => v,
            Err(e) => {
                rejected.push(reject(&e.message));
                continue;
            }
        };
        let date = match item.get("date").and_then(|v| v.as_str()) {
            None => ctx.today,
            Some(d) => match chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d") {
                Ok(d) => d,
                Err(_) => {
                    rejected.push(reject("date must be YYYY-MM-DD"));
                    continue;
                }
            },
        };
        let inst = find(ctx, sym)?;
        match upsert_price(ctx, &inst, date, e8) {
            Ok(v) => applied.push(v),
            Err(e) => rejected.push(reject(&e.message)),
        }
    }
    Ok(
        json!({ "plugin": p.name, "elapsed_ms": ms, "applied": applied, "rejected": rejected, "applied_count": applied.len(), "rejected_count": rejected.len() }),
    )
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new("plugin.install", "Register an external plugin (pinned by the SHA-256 of its executable)", install)
            .admin_write()
            .local()
            .p(Param::s("name", "plugin name").req())
            .p(Param::s("kind", "what it does").req().choices(&["importer", "price_feed", "exporter", "other"]))
            .p(Param::s("command", "path to the executable").req())
            .p(Param::s("arg", "argument passed to the executable (repeatable)").multi())
            .p(Param::s("release", "version label"))
            .p(Param::s("permissions", "declared permissions (network, read_files, write_files) — recorded, not enforced").multi())
            .p(Param::s("sha256", "refuse to install unless the executable has this hash"))
            .ex("pext plugin install --name yahoo --kind price_feed --command ~/plugins/yahoo.sh --permissions network"),
        OpSpec::new("plugin.list", "List installed plugins", list).admin().local(),
        OpSpec::new("plugin.rm", "Uninstall a plugin", rm).admin_write().local().p(Param::s("name", "plugin").req().pos()),
        OpSpec::new("plugin.run", "Run a plugin once with optional JSON input and show its (untrusted) output", run)
            .admin()
            .local()
            .p(Param::s("name", "plugin").req().pos())
            .p(Param::s("input", "JSON passed to the plugin"))
            .p(Param::i("timeout", "seconds before the plugin is stopped (default 10, max 60)")),
        OpSpec::new("price.fetch", "Fetch prices through a price_feed plugin and record them", price_fetch)
            .admin_write()
            .local()
            .p(Param::s("plugin", "price_feed plugin").req())
            .p(Param::s("symbols", "only these instruments (default: all)").multi())
            .p(Param::i("timeout", "seconds before the plugin is stopped (default 10, max 60)"))
            .ex("pext price fetch --plugin yahoo --dry-run"),
    ]
}
