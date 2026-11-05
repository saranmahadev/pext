//! Encrypted backups, restore, verification, full export and wipe. Snapshot
//! helpers here are shared with sync.

use crate::args::Args;
use crate::audit;
use crate::crypto::{self, hex, random_bytes};
use crate::ctx::Ctx;
use crate::registry::{OpSpec, Param};
use crate::{db, EngineConfig};
use pext_core::{Error, Result};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const EXT: &str = "pextbak";

fn io(code: &str, what: &str, e: impl std::fmt::Display) -> Error {
    Error::io(code, format!("{what}: {e}"))
}

pub fn passphrase(cfg: &EngineConfig) -> Result<&str> {
    cfg.passphrase.as_ref().map(|s| s.0.as_str()).ok_or_else(|| {
        Error::auth("passphrase_required", "this needs a passphrase")
            .with_hint("set PEXT_PASSPHRASE or pass --passphrase-file PATH (never put the passphrase on the command line)")
    })
}

/// A scratch file that is removed when dropped.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(tag: &str) -> Result<Scratch> {
        let name = format!(
            "pext-{tag}-{}-{}",
            std::process::id(),
            hex(&random_bytes::<6>()?)
        );
        Ok(Scratch(std::env::temp_dir().join(name)))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut p = self.0.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(p);
        }
    }
}

#[derive(Clone, Debug)]
pub struct Meta {
    pub created_at: String,
    pub audit_head: String,
    pub audit_entries: i64,
    pub schema_version: i64,
    pub db_sha256: String,
    pub db_bytes: u64,
}

impl Meta {
    pub fn to_json(&self) -> Value {
        json!({
            "format": 1, "pext_version": crate::ops::system::VERSION, "created_at": self.created_at,
            "audit_head": self.audit_head, "audit_entries": self.audit_entries, "schema_version": self.schema_version,
            "db_sha256": self.db_sha256, "db_bytes": self.db_bytes,
        })
    }
    fn from_json(v: &Value) -> Result<Meta> {
        let bad = || Error::validation("bad_backup", "the backup metadata is malformed");
        Ok(Meta {
            created_at: v["created_at"].as_str().ok_or_else(bad)?.into(),
            audit_head: v["audit_head"].as_str().ok_or_else(bad)?.into(),
            audit_entries: v["audit_entries"].as_i64().ok_or_else(bad)?,
            schema_version: v["schema_version"].as_i64().ok_or_else(bad)?,
            db_sha256: v["db_sha256"].as_str().ok_or_else(bad)?.into(),
            db_bytes: v["db_bytes"].as_u64().ok_or_else(bad)?,
        })
    }
}

fn sha256_bytes(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(b))
}

/// Consistent copy of the live database as bytes.
fn snapshot_db(cfg: &EngineConfig) -> Result<Vec<u8>> {
    if !cfg.db_path.exists() {
        return Err(Error::not_found(
            "db_not_found",
            format!("no database at {}", cfg.db_path.display()),
        )
        .with_hint("run `pext init`"));
    }
    // Read-only, but without `query_only` (VACUUM INTO writes to the *target* file).
    let src = Connection::open_with_flags(&cfg.db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(crate::ctx::db_err)?;
    src.execute_batch("PRAGMA busy_timeout=5000;")
        .map_err(crate::ctx::db_err)?;
    let tmp = Scratch::new("snap")?;
    src.execute("VACUUM INTO ?1", [tmp.0.to_string_lossy().as_ref()])
        .map_err(crate::ctx::db_err)?;
    std::fs::read(&tmp.0).map_err(|e| io("snapshot_failed", "cannot read the snapshot", e))
}

/// Encrypted blob of the current database plus its metadata.
pub fn pack(cfg: &EngineConfig, now: &str) -> Result<(Vec<u8>, Meta)> {
    let pass = passphrase(cfg)?;
    crypto::check_passphrase(pass)?;
    let bytes = snapshot_db(cfg)?;
    let conn = db::open_readonly(&cfg.db_path)?;
    let rep = audit::verify_chain(&conn)?;
    let meta = Meta {
        created_at: now.to_string(),
        audit_head: audit::head_hash(&conn)?,
        audit_entries: rep.entries,
        schema_version: db::version(&conn)?,
        db_sha256: sha256_bytes(&bytes),
        db_bytes: bytes.len() as u64,
    };
    let mj = meta.to_json().to_string();
    let mut plain = Vec::with_capacity(4 + mj.len() + bytes.len());
    plain.extend_from_slice(&(mj.len() as u32).to_le_bytes());
    plain.extend_from_slice(mj.as_bytes());
    plain.extend_from_slice(&bytes);
    Ok((crypto::encrypt(&plain, pass, cfg.kdf)?, meta))
}

/// Decrypt and split a blob, checking the embedded hash.
pub fn unpack(blob: &[u8], pass: &str) -> Result<(Meta, Vec<u8>)> {
    let plain = crypto::decrypt(blob, pass)?;
    let bad = || Error::validation("bad_backup", "the backup contents are malformed");
    if plain.len() < 4 {
        return Err(bad());
    }
    let n = u32::from_le_bytes([plain[0], plain[1], plain[2], plain[3]]) as usize;
    if plain.len() < 4 + n {
        return Err(bad());
    }
    let mv: Value = serde_json::from_slice(&plain[4..4 + n]).map_err(|_| bad())?;
    let meta = Meta::from_json(&mv)?;
    let bytes = plain[4 + n..].to_vec();
    if sha256_bytes(&bytes) != meta.db_sha256 || bytes.len() as u64 != meta.db_bytes {
        return Err(Error::integrity(
            "backup_corrupt",
            "the database inside the backup does not match its checksum",
        ));
    }
    Ok((meta, bytes))
}

/// Open database bytes read-only via a scratch file.
pub fn open_bytes(bytes: &[u8]) -> Result<(Scratch, Connection)> {
    let s = Scratch::new("open")?;
    std::fs::write(&s.0, bytes)
        .map_err(|e| io("write_failed", "cannot write a scratch file", e))?;
    let conn = Connection::open_with_flags(&s.0, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(crate::ctx::db_err)?;
    Ok((s, conn))
}

pub fn chain_contains(conn: &Connection, hash: &str) -> Result<bool> {
    if hash == audit::GENESIS {
        return Ok(true);
    }
    match conn.query_row("SELECT 1 FROM audit WHERE hash=?1 LIMIT 1", [hash], |_| {
        Ok(())
    }) {
        Ok(()) => Ok(true),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(e) => Err(crate::ctx::db_err(e)),
    }
}

/// Atomically replace the live database with `bytes`; the old one is kept
/// beside it as `<db>.<tag>-<timestamp>` so nothing is ever lost.
pub fn replace_db(
    cfg: &EngineConfig,
    bytes: &[u8],
    tag: &str,
    stamp: &str,
) -> Result<Option<PathBuf>> {
    let path = &cfg.db_path;
    let saved = if path.exists() {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{tag}-{}", stamp.replace([':', '-'], "")));
        let dest = path.with_file_name(name);
        std::fs::copy(path, &dest).map_err(|e| {
            io(
                "copy_failed",
                "cannot keep a copy of the current database",
                e,
            )
        })?;
        Some(dest)
    } else {
        None
    };
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| io("create_dir_failed", "cannot create the database folder", e))?;
        }
    }
    let mut tmp = path.clone().into_os_string();
    tmp.push(".restore-tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)
        .map_err(|e| io("write_failed", "cannot write the restored database", e))?;
    for suffix in ["-wal", "-shm"] {
        let mut p = path.clone().into_os_string();
        p.push(suffix);
        let _ = std::fs::remove_file(p);
    }
    std::fs::rename(&tmp, path)
        .map_err(|e| io("write_failed", "cannot replace the database", e))?;
    Ok(saved)
}

fn dir_arg(ctx: &Ctx, a: &Args) -> PathBuf {
    match a.opt_str("dir") {
        Some(d) => PathBuf::from(d),
        None => ctx
            .cfg
            .db_path
            .parent()
            .map(|p| p.join("backups"))
            .unwrap_or_else(|| PathBuf::from("backups")),
    }
}

fn is_backup_name(n: &str) -> bool {
    n.starts_with("pext-") && n.ends_with(&format!(".{EXT}"))
}

pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).map_err(|e| {
        io(
            "write_failed",
            &format!("cannot write {}", path.display()),
            e,
        )
    })?;
    std::fs::rename(&tmp, path).map_err(|e| {
        io(
            "write_failed",
            &format!("cannot write {}", path.display()),
            e,
        )
    })
}

fn list_files(dir: &Path) -> Result<Vec<(String, u64, std::time::SystemTime)>> {
    let mut v = vec![];
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(v),
        Err(e) => {
            return Err(io(
                "read_dir_failed",
                &format!("cannot read {}", dir.display()),
                e,
            ))
        }
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if is_backup_name(&name) {
            if let Ok(md) = ent.metadata() {
                v.push((
                    name,
                    md.len(),
                    md.modified().unwrap_or(std::time::UNIX_EPOCH),
                ));
            }
        }
    }
    v.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(v)
}

fn create(ctx: &Ctx, a: &Args) -> Result<Value> {
    let dir = dir_arg(ctx, a);
    let keep = a.opt_i64("keep")?.unwrap_or(10);
    if !(1..=1000).contains(&keep) {
        return Err(
            Error::validation("invalid_keep", "keep must be between 1 and 1000").with_field("keep"),
        );
    }
    let (blob, meta) = pack(ctx.cfg, &ctx.now)?;
    std::fs::create_dir_all(&dir).map_err(|e| {
        io(
            "create_dir_failed",
            &format!("cannot create {}", dir.display()),
            e,
        )
    })?;
    let stamp = ctx
        .now
        .replace([':', '-'], "")
        .replace('T', "-")
        .replace('Z', "");
    // Zero-padded sequence (one past the highest used for this second, so a
    // pruned gap can never make a new backup sort before older ones).
    let prefix = format!("pext-{stamp}-");
    let used = list_files(&dir)?
        .iter()
        .filter_map(|(f, _, _)| {
            f.strip_prefix(&prefix)
                .and_then(|r| r.strip_suffix(&format!(".{EXT}")))
                .and_then(|n| n.parse::<u32>().ok())
        })
        .max()
        .unwrap_or(0);
    let name = format!("{prefix}{:03}.{EXT}", used + 1);
    let path = dir.join(&name);
    write_atomic(&path, &blob)?;
    let files = list_files(&dir)?;
    let mut pruned = vec![];
    if files.len() as i64 > keep {
        for (f, _, _) in files
            .iter()
            .filter(|(f, _, _)| *f != name)
            .take(files.len() - keep as usize)
        {
            if std::fs::remove_file(dir.join(f)).is_ok() {
                pruned.push(f.clone());
            }
        }
    }
    Ok(json!({
        "file": path.display().to_string(), "bytes": blob.len(), "audit_head": meta.audit_head, "audit_entries": meta.audit_entries,
        "kept": files.len() - pruned.len(), "pruned": pruned,
    }))
}

fn list(ctx: &Ctx, a: &Args) -> Result<Value> {
    let dir = dir_arg(ctx, a);
    let mut rows = vec![];
    for (name, size, mtime) in list_files(&dir)?.into_iter().rev() {
        let t: chrono::DateTime<chrono::Utc> = mtime.into();
        rows.push(json!({ "file": name, "path": dir.join(&name).display().to_string(), "bytes": size, "modified": t.format("%Y-%m-%dT%H:%M:%SZ").to_string() }));
    }
    Ok(Value::Array(rows))
}

fn read_backup(ctx: &Ctx, file: &str) -> Result<(Meta, Vec<u8>)> {
    let blob =
        std::fs::read(file).map_err(|e| io("read_failed", &format!("cannot read {file}"), e))?;
    unpack(&blob, passphrase(ctx.cfg)?)
}

/// Full health check of a decrypted database image.
pub fn check_image(bytes: &[u8]) -> Result<Value> {
    let (_s, conn) = open_bytes(bytes)?;
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(crate::ctx::db_err)?;
    let rep = audit::verify_chain(&conn)?;
    let newer = db::version(&conn)? > db::latest_version();
    Ok(json!({
        "sqlite_integrity": integrity, "audit_chain_ok": rep.ok, "audit_entries": rep.entries,
        "schema_version": db::version(&conn)?, "newer_than_this_pext": newer,
        "ok": integrity == "ok" && rep.ok && !newer,
    }))
}

fn verify(ctx: &Ctx, a: &Args) -> Result<Value> {
    let file = a.str("file")?;
    let (meta, bytes) = read_backup(ctx, &file)?;
    let check = check_image(&bytes)?;
    Ok(
        json!({ "file": file, "ok": check["ok"], "created_at": meta.created_at, "audit_head": meta.audit_head, "checks": check }),
    )
}

fn restore(ctx: &Ctx, a: &Args) -> Result<Value> {
    let file = a.str("file")?;
    let (meta, bytes) = read_backup(ctx, &file)?;
    let check = check_image(&bytes)?;
    if check["ok"] != json!(true) {
        return Err(Error::integrity(
            "backup_unhealthy",
            "the backup decrypts but fails its health checks; refusing to restore",
        )
        .with_details(check));
    }
    let exists = ctx.cfg.db_path.exists();
    let live_entries: Option<i64> = if exists {
        let conn = db::open_readonly(&ctx.cfg.db_path)?;
        Some(audit::verify_chain(&conn)?.entries)
    } else {
        None
    };
    let would = json!({
        "restore_from": file, "created_at": meta.created_at, "backup_entries": meta.audit_entries,
        "current_entries": live_entries, "replaces_existing": exists,
    });
    if a.flag("dry_run") || (exists && !a.flag("yes")) {
        let mut out = would;
        out["restored"] = json!(false);
        out["note"] = json!(if a.flag("dry_run") {
            "dry run: nothing was changed"
        } else {
            "this replaces the current database; repeat with --yes (the current one is kept as a copy)"
        });
        return Ok(out);
    }
    let saved = replace_db(ctx.cfg, &bytes, "pre-restore", &ctx.now)?;
    let mut out = would;
    out["restored"] = json!(true);
    out["previous_database_kept_as"] = json!(saved.map(|p| p.display().to_string()));
    Ok(out)
}

// ---- export & wipe ---------------------------------------------------------------

const EXPORT_SKIP: &[&str] = &["sqlite_sequence"];

fn scrub(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.remove("secret_hash");
            m.values_mut().for_each(scrub);
        }
        Value::Array(a) => a.iter_mut().for_each(scrub),
        _ => {}
    }
}

fn dump_all(conn: &Connection) -> Result<Value> {
    let tables: Vec<String> = {
        let mut st = conn.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").map_err(crate::ctx::db_err)?;
        let rows = st.query_map([], |r| r.get(0)).map_err(crate::ctx::db_err)?;
        rows.collect::<rusqlite::Result<_>>()
            .map_err(crate::ctx::db_err)?
    };
    let mut out = serde_json::Map::new();
    for t in tables.iter().filter(|t| !EXPORT_SKIP.contains(&t.as_str())) {
        let rows = crate::ctx::query_json(conn, &format!("SELECT * FROM \"{t}\""), &[])?;
        let rows: Vec<Value> = rows
            .into_iter()
            .map(|mut m| {
                m.remove("secret_hash"); // credentials never leave the database
                                         // The audit log keeps before/after row images as JSON text.
                for key in ["changes", "args", "result"] {
                    if let Some(Value::String(txt)) = m.get(key) {
                        if txt.contains("secret_hash") {
                            if let Ok(mut parsed) = serde_json::from_str::<Value>(txt) {
                                scrub(&mut parsed);
                                m.insert(key.into(), Value::String(parsed.to_string()));
                            }
                        }
                    }
                }
                Value::Object(m)
            })
            .collect();
        out.insert(t.clone(), Value::Array(rows));
    }
    Ok(Value::Object(out))
}

fn export_all(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let conn = db::open_readonly(&ctx.cfg.db_path)?;
    Ok(json!({
        "format": "pext-export/1", "pext_version": crate::ops::system::VERSION, "exported_at": ctx.now,
        "schema_version": db::version(&conn)?, "tables": dump_all(&conn)?,
    }))
}

fn wipe(ctx: &Ctx, a: &Args) -> Result<Value> {
    if a.str("confirm")? != "WIPE" {
        return Err(Error::usage(
            "confirmation_required",
            "type --confirm WIPE to delete all data",
        )
        .with_hint("export first with `pext export all --out FILE`, or pass --export FILE"));
    }
    let path = &ctx.cfg.db_path;
    if !path.exists() {
        return Err(Error::not_found(
            "db_not_found",
            format!("no database at {}", path.display()),
        ));
    }
    let mut exported = Value::Null;
    if let Some(f) = a.opt_str("export") {
        let conn = db::open_readonly(path)?;
        let dump = json!({ "format": "pext-export/1", "exported_at": ctx.now, "schema_version": db::version(&conn)?, "tables": dump_all(&conn)? });
        write_atomic(
            Path::new(&f),
            serde_json::to_string_pretty(&dump)
                .unwrap_or_default()
                .as_bytes(),
        )?;
        exported = json!(f);
    }
    if a.flag("dry_run") {
        return Ok(
            json!({ "wiped": false, "would_delete": path.display().to_string(), "exported_to": exported, "note": "dry run: nothing was deleted" }),
        );
    }
    let mut removed = vec![];
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.clone().into_os_string();
        p.push(suffix);
        let p = PathBuf::from(p);
        if p.exists() {
            std::fs::remove_file(&p).map_err(|e| {
                io(
                    "delete_failed",
                    &format!("cannot delete {}", p.display()),
                    e,
                )
            })?;
            removed.push(p.display().to_string());
        }
    }
    Ok(json!({ "wiped": true, "deleted": removed, "exported_to": exported }))
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new(
            "backup.create",
            "Write an encrypted backup (passphrase from PEXT_PASSPHRASE) and prune old ones",
            create,
        )
        .admin()
        .file_op()
        .local()
        .p(Param::s(
            "dir",
            "folder for backups (default: next to the database)",
        ))
        .p(Param::i("keep", "how many backups to keep (default 10)"))
        .ex("PEXT_PASSPHRASE=... pext backup create --dir ~/Backups/pext --keep 12"),
        OpSpec::new("backup.list", "List backups in a folder", list)
            .admin()
            .file_op()
            .local()
            .p(Param::s("dir", "backup folder")),
        OpSpec::new(
            "backup.verify",
            "Decrypt a backup and check its integrity and audit chain",
            verify,
        )
        .admin()
        .file_op()
        .local()
        .p(Param::s("file", "backup file").req().pos()),
        OpSpec::new(
            "backup.restore",
            "Restore a backup (use --dry-run to preview; --yes to replace an existing database)",
            restore,
        )
        .admin()
        .file_op()
        .local()
        .p(Param::s("file", "backup file").req().pos()),
        OpSpec::new(
            "export.all",
            "Export every table as JSON (secrets excluded)",
            export_all,
        )
        .admin()
        .local(),
        OpSpec::new(
            "data.wipe",
            "Delete the database and all its data (optionally exporting it first)",
            wipe,
        )
        .admin()
        .file_op()
        .local()
        .p(Param::s("confirm", "type WIPE to confirm").req())
        .p(Param::s(
            "export",
            "write a full JSON export to this file first",
        )),
    ]
}
