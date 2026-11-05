//! Multi-device sync through a shared folder (Dropbox, Syncthing, a USB
//! stick...). The folder holds one encrypted snapshot. Histories are compared
//! by the audit hash chain: whichever side's head is in the other's history is
//! behind; if neither is, the devices have diverged and a human decides which
//! side wins. Nothing is ever merged silently and the loser is always kept.

use crate::args::Args;
use crate::audit;
use crate::ctx::Ctx;
use crate::ops::backup::{
    self, chain_contains, check_image, open_bytes, pack, passphrase, replace_db, unpack,
    write_atomic,
};
use crate::registry::{OpSpec, Param};
use crate::{db, EngineConfig};
use pext_core::{Error, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

const SNAPSHOT: &str = "pext-sync.pextbak";

fn io(code: &str, what: &str, e: impl std::fmt::Display) -> Error {
    Error::io(code, format!("{what}: {e}"))
}

fn sidecar(cfg: &EngineConfig) -> PathBuf {
    let mut p = cfg.db_path.clone().into_os_string();
    p.push(".sync.json");
    PathBuf::from(p)
}

struct State {
    dir: PathBuf,
    last_head: String,
}

fn load_state(cfg: &EngineConfig) -> Result<State> {
    let p = sidecar(cfg);
    let txt = std::fs::read_to_string(&p).map_err(|_| {
        Error::not_found(
            "sync_not_configured",
            "sync is not set up for this database",
        )
        .with_hint("run `pext sync init --dir FOLDER`")
    })?;
    let v: Value = serde_json::from_str(&txt).map_err(|_| {
        Error::integrity("sync_state_corrupt", format!("{} is damaged", p.display()))
            .with_hint("run `pext sync init --dir FOLDER` again")
    })?;
    Ok(State {
        dir: PathBuf::from(v["dir"].as_str().unwrap_or_default()),
        last_head: v["last_head"].as_str().unwrap_or_default().to_string(),
    })
}

fn save_state(cfg: &EngineConfig, st: &State, now: &str) -> Result<()> {
    let v = json!({ "dir": st.dir.display().to_string(), "last_head": st.last_head, "updated_at": now });
    write_atomic(&sidecar(cfg), v.to_string().as_bytes())
}

fn remote_path(st: &State) -> PathBuf {
    st.dir.join(SNAPSHOT)
}

struct Remote {
    meta: backup::Meta,
    bytes: Vec<u8>,
}

fn read_remote(cfg: &EngineConfig, st: &State) -> Result<Option<Remote>> {
    let p = remote_path(st);
    let blob = match std::fs::read(&p) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(io(
                "read_failed",
                &format!("cannot read {}", p.display()),
                e,
            ))
        }
    };
    let (meta, bytes) = unpack(&blob, passphrase(cfg)?)?;
    Ok(Some(Remote { meta, bytes }))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Rel {
    NoRemote,
    NoLocal,
    InSync,
    Ahead,
    Behind,
    Diverged,
}

impl Rel {
    fn name(self) -> &'static str {
        match self {
            Rel::NoRemote => "no_remote",
            Rel::NoLocal => "no_local",
            Rel::InSync => "in_sync",
            Rel::Ahead => "ahead",
            Rel::Behind => "behind",
            Rel::Diverged => "diverged",
        }
    }
}

struct Compare {
    rel: Rel,
    local_head: Option<String>,
    local_entries: Option<i64>,
    local_chain_ok: bool,
}

fn compare(cfg: &EngineConfig, remote: &Option<Remote>) -> Result<Compare> {
    if !cfg.db_path.exists() {
        return Ok(Compare {
            rel: if remote.is_some() {
                Rel::NoLocal
            } else {
                Rel::NoRemote
            },
            local_head: None,
            local_entries: None,
            local_chain_ok: true,
        });
    }
    let conn = db::open_readonly(&cfg.db_path)?;
    let rep = audit::verify_chain(&conn)?;
    let head = audit::head_hash(&conn)?;
    let mut c = Compare {
        rel: Rel::NoRemote,
        local_head: Some(head.clone()),
        local_entries: Some(rep.entries),
        local_chain_ok: rep.ok,
    };
    let Some(r) = remote else { return Ok(c) };
    c.rel = if r.meta.audit_head == head {
        Rel::InSync
    } else if chain_contains(&conn, &r.meta.audit_head)? {
        Rel::Ahead
    } else {
        let (_s, rc) = open_bytes(&r.bytes)?;
        if chain_contains(&rc, &head)? {
            Rel::Behind
        } else {
            Rel::Diverged
        }
    };
    Ok(c)
}

fn state_json(st: &State, c: &Compare, r: &Option<Remote>) -> Value {
    json!({
        "folder": st.dir.display().to_string(), "state": c.rel.name(),
        "local_head": c.local_head, "local_entries": c.local_entries, "local_chain_ok": c.local_chain_ok,
        "remote_head": r.as_ref().map(|r| r.meta.audit_head.clone()), "remote_entries": r.as_ref().map(|r| r.meta.audit_entries),
        "remote_written_at": r.as_ref().map(|r| r.meta.created_at.clone()), "last_synced_head": st.last_head,
    })
}

fn advice(rel: Rel) -> &'static str {
    match rel {
        Rel::NoRemote => "nothing in the folder yet: run `pext sync push`",
        Rel::NoLocal => "no local database yet: run `pext sync pull` to create it from the folder",
        Rel::InSync => "up to date",
        Rel::Ahead => "this device has newer changes: run `pext sync push`",
        Rel::Behind => "the folder has newer changes: run `pext sync pull`",
        Rel::Diverged => "both sides changed independently: pick a winner with `pext sync resolve --keep local|remote --yes` (the other side is kept as a copy)",
    }
}

fn status_inner(ctx: &Ctx, st: &State) -> Result<(Compare, Option<Remote>)> {
    let remote = read_remote(ctx.cfg, st)?;
    let c = compare(ctx.cfg, &remote)?;
    Ok((c, remote))
}

fn init(ctx: &Ctx, a: &Args) -> Result<Value> {
    let dir = PathBuf::from(a.str("dir")?);
    std::fs::create_dir_all(&dir).map_err(|e| {
        io(
            "create_dir_failed",
            &format!("cannot create {}", dir.display()),
            e,
        )
    })?;
    let st = State {
        dir,
        last_head: String::new(),
    };
    if !a.flag("dry_run") {
        save_state(ctx.cfg, &st, &ctx.now)?;
    }
    let (c, r) = status_inner(ctx, &st)?;
    let mut out = state_json(&st, &c, &r);
    out["next"] = json!(advice(c.rel));
    out["configured"] = json!(!a.flag("dry_run"));
    Ok(out)
}

fn status(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let st = load_state(ctx.cfg)?;
    let (c, r) = status_inner(ctx, &st)?;
    let mut out = state_json(&st, &c, &r);
    out["next"] = json!(advice(c.rel));
    Ok(out)
}

fn conflict(c: &Compare, st: &State, r: &Option<Remote>) -> Error {
    Error::conflict(
        "sync_conflict",
        format!(
            "cannot continue: this device is {} relative to the folder",
            c.rel.name()
        ),
    )
    .with_hint(advice(c.rel))
    .with_details(state_json(st, c, r))
}

fn do_push(ctx: &Ctx, st: &mut State, c: &Compare) -> Result<Value> {
    if !c.local_chain_ok {
        return Err(Error::integrity(
            "local_chain_broken",
            "the local audit log fails verification; refusing to publish it",
        )
        .with_hint("run `pext doctor`"));
    }
    let (blob, meta) = pack(ctx.cfg, &ctx.now)?;
    std::fs::create_dir_all(&st.dir).map_err(|e| {
        io(
            "create_dir_failed",
            &format!("cannot create {}", st.dir.display()),
            e,
        )
    })?;
    write_atomic(&remote_path(st), &blob)?;
    st.last_head = meta.audit_head.clone();
    save_state(ctx.cfg, st, &ctx.now)?;
    Ok(
        json!({ "pushed": true, "head": meta.audit_head, "entries": meta.audit_entries, "bytes": blob.len() }),
    )
}

fn push(ctx: &Ctx, a: &Args) -> Result<Value> {
    let mut st = load_state(ctx.cfg)?;
    let (c, r) = status_inner(ctx, &st)?;
    match c.rel {
        Rel::NoLocal => Err(
            Error::not_found("db_not_found", "no local database to push")
                .with_hint("run `pext sync pull` first"),
        ),
        Rel::Behind | Rel::Diverged => Err(conflict(&c, &st, &r)),
        Rel::InSync => {
            Ok(json!({ "pushed": false, "note": "already up to date", "state": c.rel.name() }))
        }
        Rel::NoRemote | Rel::Ahead => {
            if a.flag("dry_run") {
                return Ok(
                    json!({ "pushed": false, "would_push": true, "state": c.rel.name(), "note": "dry run: nothing was written" }),
                );
            }
            let mut out = do_push(ctx, &mut st, &c)?;
            out["state"] = json!(c.rel.name());
            Ok(out)
        }
    }
}

fn do_pull(ctx: &Ctx, st: &mut State, remote: &Remote, c: &Compare) -> Result<Value> {
    let chk = check_image(&remote.bytes)?;
    if chk["ok"] != json!(true) {
        return Err(Error::integrity(
            "remote_unhealthy",
            "the snapshot in the folder fails its health checks; refusing to apply it",
        )
        .with_details(chk));
    }
    let saved = replace_db(ctx.cfg, &remote.bytes, "pre-sync", &ctx.now)?;
    st.last_head = remote.meta.audit_head.clone();
    save_state(ctx.cfg, st, &ctx.now)?;
    Ok(json!({
        "pulled": true, "head": remote.meta.audit_head, "entries": remote.meta.audit_entries,
        "previous_state": c.rel.name(), "previous_database_kept_as": saved.map(|p| p.display().to_string()),
    }))
}

fn pull(ctx: &Ctx, a: &Args) -> Result<Value> {
    let mut st = load_state(ctx.cfg)?;
    let (c, r) = status_inner(ctx, &st)?;
    match (c.rel, r) {
        (Rel::NoRemote, _) | (_, None) => Err(Error::not_found(
            "sync_remote_empty",
            "the folder has no snapshot yet",
        )
        .with_hint("run `pext sync push` on the device that has the data")),
        (Rel::InSync, _) => {
            Ok(json!({ "pulled": false, "note": "already up to date", "state": "in_sync" }))
        }
        (Rel::Ahead, _) => Ok(
            json!({ "pulled": false, "note": "this device is ahead of the folder; nothing to pull (run `pext sync push`)", "state": "ahead" }),
        ),
        (Rel::Diverged, r) => Err(conflict(&c, &st, &r)),
        (Rel::Behind | Rel::NoLocal, Some(remote)) => {
            if a.flag("dry_run") {
                return Ok(json!({
                    "pulled": false, "would_pull": true, "state": c.rel.name(),
                    "remote_entries": remote.meta.audit_entries, "local_entries": c.local_entries, "note": "dry run: nothing was changed",
                }));
            }
            do_pull(ctx, &mut st, &remote, &c)
        }
    }
}

fn resolve(ctx: &Ctx, a: &Args) -> Result<Value> {
    let keep = a.str("keep")?;
    let mut st = load_state(ctx.cfg)?;
    let (c, r) = status_inner(ctx, &st)?;
    let Some(remote) = r else {
        return Err(Error::not_found(
            "sync_remote_empty",
            "the folder has no snapshot, so there is nothing to resolve",
        )
        .with_hint("run `pext sync push`"));
    };
    if c.rel != Rel::Diverged {
        return Err(Error::conflict(
            "nothing_to_resolve",
            format!("the devices are not diverged (state: {})", c.rel.name()),
        )
        .with_hint(advice(c.rel)));
    }
    let loser = if keep == "local" { "remote" } else { "local" };
    if a.flag("dry_run") || !a.flag("yes") {
        return Ok(json!({
            "resolved": false, "keep": keep, "discard_as_copy": loser,
            "local_entries": c.local_entries, "remote_entries": remote.meta.audit_entries,
            "note": if a.flag("dry_run") { "dry run: nothing was changed" } else { "repeat with --yes; the losing side is kept as a copy" },
        }));
    }
    if keep == "local" {
        if !c.local_chain_ok {
            return Err(Error::integrity(
                "local_chain_broken",
                "the local audit log fails verification; refusing to publish it",
            ));
        }
        let stamp = ctx
            .now
            .replace([':', '-'], "")
            .replace('T', "-")
            .replace('Z', "");
        let copy = st
            .dir
            .join(format!("pext-conflict-{stamp}.{}", backup::EXT));
        std::fs::copy(remote_path(&st), &copy).map_err(|e| {
            io(
                "copy_failed",
                "cannot keep a copy of the folder's snapshot",
                e,
            )
        })?;
        let mut out = do_push(ctx, &mut st, &c)?;
        out["resolved"] = json!(true);
        out["keep"] = json!("local");
        out["remote_kept_as"] = json!(copy.display().to_string());
        Ok(out)
    } else {
        let mut out = do_pull(ctx, &mut st, &remote, &c)?;
        out["resolved"] = json!(true);
        out["keep"] = json!("remote");
        Ok(out)
    }
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new(
            "sync.init",
            "Link this database to a shared sync folder",
            init,
        )
        .admin()
        .file_op()
        .local()
        .p(Param::s("dir", "shared folder (Dropbox, Syncthing, USB...)").req())
        .ex("pext sync init --dir ~/Dropbox/pext"),
        OpSpec::new(
            "sync.status",
            "Compare this device with the sync folder (in_sync, ahead, behind, diverged)",
            status,
        )
        .admin()
        .file_op()
        .local(),
        OpSpec::new(
            "sync.push",
            "Publish this device's data to the sync folder (only when it is ahead)",
            push,
        )
        .admin()
        .file_op()
        .local(),
        OpSpec::new(
            "sync.pull",
            "Apply the sync folder's data to this device (only when it is behind)",
            pull,
        )
        .admin()
        .file_op()
        .local(),
        OpSpec::new(
            "sync.resolve",
            "After a divergence, keep one side; the other is saved as a copy",
            resolve,
        )
        .admin()
        .file_op()
        .local()
        .p(Param::s("keep", "which side wins")
            .req()
            .choices(&["local", "remote"])),
    ]
}
