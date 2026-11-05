use pext_cli::{run, Env};
use serde_json::Value;
use tempfile::TempDir;

struct Cli {
    dir: TempDir,
    db: String,
    token: Option<String>,
}

struct Out {
    code: i32,
    out: String,
    err: String,
}

impl Out {
    fn data(&self) -> Value {
        let v: Value = serde_json::from_str(&self.out)
            .unwrap_or_else(|e| panic!("not json ({e}): {}\nstderr: {}", self.out, self.err));
        v["data"].clone()
    }
    fn error_code(&self) -> String {
        let v: Value = serde_json::from_str(&self.out).unwrap();
        v["error"]["code"].as_str().unwrap_or("").to_string()
    }
}

impl Cli {
    fn new(db_name: &str) -> Cli {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join(db_name).to_string_lossy().to_string();
        let c = Cli {
            dir,
            db,
            token: None,
        };
        assert_eq!(c.run(&["init"]).code, 0);
        c
    }
    fn with_db(&self, db_name: &str) -> Cli {
        Cli {
            dir: TempDir::new().unwrap(),
            db: self.dir.path().join(db_name).to_string_lossy().to_string(),
            token: None,
        }
    }
    fn run(&self, args: &[&str]) -> Out {
        let env = Env {
            is_tty: false,
            db: Some(self.db.clone()),
            today: Some("2026-11-15".into()),
            token: self.token.clone(),
            passphrase: Some("correct horse battery".into()),
            kdf_fast: true,
            ..Default::default()
        };
        let mut argv = vec!["pext".to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let code = run(argv, &env, &mut o, &mut e);
        Out {
            code,
            out: String::from_utf8(o).unwrap(),
            err: String::from_utf8(e).unwrap(),
        }
    }
    fn ok(&self, args: &[&str]) -> Value {
        let r = self.run(args);
        assert_eq!(r.code, 0, "{args:?} failed: {} {}", r.out, r.err);
        r.data()
    }
    fn seed(&self) {
        self.ok(&[
            "acct",
            "add",
            "--name",
            "Bank",
            "--kind",
            "checking",
            "--opening",
            "100000",
        ]);
        self.ok(&[
            "txn",
            "add",
            "--account",
            "Bank",
            "--amount",
            "-1234.50",
            "--category",
            "food",
            "--payee",
            "Secret Cafe",
            "--memo",
            "birthday for Asha",
        ]);
    }
}

#[test]
fn backup_restore_and_wipe_from_the_command_line() {
    let c = Cli::new("a.db");
    c.seed();
    let bk = c.dir.path().join("bk").to_string_lossy().to_string();
    let made = c.ok(&["backup", "create", "--dir", &bk]);
    let file = made["file"].as_str().unwrap().to_string();
    assert_eq!(c.ok(&["backup", "verify", &file])["ok"], true);
    assert_eq!(
        c.ok(&["backup", "list", "--dir", &bk])
            .as_array()
            .unwrap()
            .len(),
        1
    );

    c.ok(&[
        "txn",
        "add",
        "--account",
        "Bank",
        "--amount",
        "-5",
        "--category",
        "food",
    ]);
    // Needs --yes; --dry-run changes nothing.
    assert_eq!(c.ok(&["backup", "restore", &file])["restored"], false);
    assert_eq!(
        c.ok(&["backup", "restore", &file, "--dry-run", "--yes"])["restored"],
        false
    );
    assert_eq!(
        c.ok(&["backup", "restore", &file, "--yes"])["restored"],
        true
    );
    let txns = c.ok(&["txn", "list"]);
    assert_eq!(txns.as_array().unwrap().len(), 1);
    assert_eq!(c.ok(&["doctor"])["ok"], true);

    let out = c
        .dir
        .path()
        .join("final.json")
        .to_string_lossy()
        .to_string();
    let w = c.run(&["data", "wipe", "--confirm", "nope"]);
    assert_eq!(w.code, 2, "{}", w.out);
    assert_eq!(
        c.ok(&["data", "wipe", "--confirm", "WIPE", "--export", &out])["wiped"],
        true
    );
    assert_eq!(c.run(&["txn", "list"]).error_code(), "db_not_found");
    assert!(std::fs::metadata(&out).unwrap().len() > 100);
}

#[test]
fn wrong_passphrase_exit_code_is_auth() {
    let c = Cli::new("a.db");
    c.seed();
    let bk = c.dir.path().join("bk").to_string_lossy().to_string();
    let file = c.ok(&["backup", "create", "--dir", &bk])["file"]
        .as_str()
        .unwrap()
        .to_string();
    let env = Env {
        db: Some(c.db.clone()),
        passphrase: Some("totally different".into()),
        kdf_fast: true,
        ..Default::default()
    };
    let (mut o, mut e) = (Vec::new(), Vec::new());
    let code = run(
        ["pext", "backup", "verify", &file].map(String::from),
        &env,
        &mut o,
        &mut e,
    );
    assert_eq!(code, 6);
    let none = Env {
        db: Some(c.db.clone()),
        kdf_fast: true,
        ..Default::default()
    };
    let (mut o, mut e) = (Vec::new(), Vec::new());
    let code = run(
        ["pext", "backup", "verify", &file].map(String::from),
        &none,
        &mut o,
        &mut e,
    );
    assert_eq!(code, 6);
}

#[test]
fn passphrase_file_flag_works() {
    let c = Cli::new("a.db");
    c.seed();
    let pf = c.dir.path().join("pass.txt");
    std::fs::write(&pf, "a file based passphrase\n").unwrap();
    let env = Env {
        db: Some(c.db.clone()),
        kdf_fast: true,
        ..Default::default()
    };
    let bk = c.dir.path().join("bk").to_string_lossy().to_string();
    let (mut o, mut e) = (Vec::new(), Vec::new());
    let argv = [
        "pext",
        "backup",
        "create",
        "--dir",
        &bk,
        "--passphrase-file",
        pf.to_str().unwrap(),
    ]
    .map(String::from);
    assert_eq!(
        run(argv, &env, &mut o, &mut e),
        0,
        "{}",
        String::from_utf8_lossy(&o)
    );
    let file = serde_json::from_slice::<Value>(&o).unwrap()["data"]["file"]
        .as_str()
        .unwrap()
        .to_string();
    // The same file decrypts with the file passphrase but not the default test one.
    assert_eq!(c.run(&["backup", "verify", &file]).code, 6);
    let (mut o, mut e) = (Vec::new(), Vec::new());
    let argv = [
        "pext",
        "backup",
        "verify",
        &file,
        "--passphrase-file",
        pf.to_str().unwrap(),
    ]
    .map(String::from);
    assert_eq!(run(argv, &env, &mut o, &mut e), 0);
    // Missing passphrase file is a clear I/O error.
    let (mut o, mut e) = (Vec::new(), Vec::new());
    let argv = [
        "pext",
        "backup",
        "verify",
        &file,
        "--passphrase-file",
        "/no/such",
    ]
    .map(String::from);
    assert_ne!(run(argv, &env, &mut o, &mut e), 0);
}

#[test]
fn two_devices_sync_from_the_command_line() {
    let a = Cli::new("a.db");
    a.seed();
    let b = a.with_db("b.db");
    let shared = a.dir.path().join("shared").to_string_lossy().to_string();
    assert_eq!(
        a.ok(&["sync", "init", "--dir", &shared])["state"],
        "no_remote"
    );
    assert_eq!(a.ok(&["sync", "push"])["pushed"], true);
    assert_eq!(
        b.ok(&["sync", "init", "--dir", &shared])["state"],
        "no_local"
    );
    assert_eq!(b.ok(&["sync", "pull"])["pulled"], true);
    assert_eq!(b.ok(&["txn", "list"]).as_array().unwrap().len(), 1);

    a.ok(&[
        "txn",
        "add",
        "--account",
        "Bank",
        "--amount",
        "-1",
        "--category",
        "food",
    ]);
    b.ok(&[
        "txn",
        "add",
        "--account",
        "Bank",
        "--amount",
        "-2",
        "--category",
        "food",
    ]);
    assert_eq!(a.ok(&["sync", "push"])["pushed"], true);
    let st = b.ok(&["sync", "status"]);
    assert_eq!(st["state"], "diverged");
    let r = b.run(&["sync", "pull"]);
    assert_eq!(r.code, 5, "conflicts exit with code 5");
    assert_eq!(r.error_code(), "sync_conflict");
    assert_eq!(
        b.run(&["sync", "resolve", "--keep", "remote"]).data()["resolved"],
        false
    );
    assert_eq!(
        b.ok(&["sync", "resolve", "--keep", "remote", "--yes"])["resolved"],
        true
    );
    assert_eq!(b.ok(&["sync", "status"])["state"], "in_sync");
    assert_eq!(b.ok(&["txn", "list"]).as_array().unwrap().len(), 2);
}

#[test]
fn tokens_gate_the_database_and_redact_hides_text() {
    let mut c = Cli::new("a.db");
    c.seed();
    let t = c.ok(&[
        "token", "create", "--name", "agent", "--role", "read", "--redact",
    ]);
    let secret = t["token"].as_str().unwrap().to_string();
    assert!(secret.starts_with("pext_"));
    c.ok(&["config", "set", "require_token", "true"]);
    // Without a token the database is closed.
    let r = c.run(&["txn", "list"]);
    assert_eq!(r.code, 6, "{}", r.out);
    // With the read token: reads work, writes do not, text is masked.
    c.token = Some(secret);
    let list = c.run(&["txn", "list"]);
    assert_eq!(list.code, 0, "{}", list.out);
    assert!(
        !list.out.contains("Secret Cafe"),
        "payee leaked: {}",
        list.out
    );
    assert!(!list.out.contains("Asha"), "memo leaked: {}", list.out);
    assert!(
        list.out.contains("123450"),
        "amounts stay visible: {}",
        list.out
    );
    assert_eq!(
        c.run(&["txn", "add", "--account", "Bank", "--amount", "-1"])
            .code,
        6
    );
    assert_eq!(
        c.run(&["audit", "list"]).code,
        6,
        "redacting actors cannot read the audit log"
    );
}

#[test]
fn private_flag_hides_amounts() {
    let c = Cli::new("a.db");
    c.seed();
    let shown = c.run(&["acct", "list", "-o", "csv"]);
    assert!(
        shown.out.contains("98765") || shown.out.contains("987.65") || shown.out.contains("98,765"),
        "{}",
        shown.out
    );
    let hidden = c.run(&["acct", "list", "-o", "csv", "--private"]);
    assert_eq!(hidden.code, 0);
    assert!(
        !hidden.out.contains("98765") && !hidden.out.contains("98,765"),
        "{}",
        hidden.out
    );
    let j = c.run(&["acct", "list", "--private"]);
    assert!(!j.out.contains("98765"), "{}", j.out);
}

#[test]
fn plugin_prices_from_the_command_line() {
    use std::os::unix::fs::PermissionsExt;
    let c = Cli::new("a.db");
    c.ok(&[
        "invest",
        "add",
        "--symbol",
        "NIFTYBEES",
        "--kind",
        "etf",
        "--name",
        "Nifty",
    ]);
    let p = c.dir.path().join("feed.sh");
    std::fs::write(&p, "#!/bin/sh\ncat >/dev/null\necho '{\"prices\":[{\"symbol\":\"NIFTYBEES\",\"price\":\"250.5\"}]}'\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ps = p.to_string_lossy().to_string();
    c.ok(&[
        "plugin",
        "install",
        "--name",
        "feed",
        "--kind",
        "price_feed",
        "--command",
        &ps,
        "--permissions",
        "network",
    ]);
    assert_eq!(c.ok(&["plugin", "list"]).as_array().unwrap().len(), 1);
    let d = c.ok(&["price", "fetch", "--plugin", "feed", "--dry-run"]);
    assert_eq!(d["applied_count"], 1);
    assert_eq!(
        c.ok(&["price", "list", "NIFTYBEES"])
            .as_array()
            .unwrap()
            .len(),
        0
    );
    c.ok(&["price", "fetch", "--plugin", "feed"]);
    assert_eq!(c.ok(&["price", "list", "NIFTYBEES"])[0]["price"], "250.5");
    c.ok(&["undo", "--yes"]);
    assert_eq!(
        c.ok(&["price", "list", "NIFTYBEES"])
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn completions_cover_every_shell_and_include_new_commands() {
    let c = Cli::new("a.db");
    for sh in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let r = c.run(&["completions", sh]);
        assert_eq!(r.code, 0, "{sh}");
        assert!(r.out.contains("pext"), "{sh}");
    }
    let bash = c.run(&["completions", "bash"]).out;
    for word in ["backup", "sync", "plugin", "token"] {
        assert!(bash.contains(word), "{word} missing");
    }
    assert_eq!(c.run(&["completions", "tcsh"]).code, 2);
}

#[test]
fn commands_listing_marks_local_only_operations() {
    let c = Cli::new("a.db");
    let list = c.ok(&["commands"]);
    let find = |n: &str| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == n)
            .unwrap()
            .clone()
    };
    for n in [
        "backup.create",
        "sync.push",
        "plugin.run",
        "data.wipe",
        "price.fetch",
    ] {
        assert_eq!(find(n)["local_only"], true, "{n}");
    }
    assert_eq!(find("txn.add")["local_only"], false);
}

#[test]
fn terminal_bar_charts() {
    let c = Cli::new("a.db");
    c.seed();
    c.ok(&[
        "txn",
        "add",
        "--account",
        "Bank",
        "--amount",
        "-500",
        "--category",
        "housing.rent",
    ]);
    let r = c.run(&["report", "spending", "-o", "table", "--chart", "bar"]);
    assert_eq!(r.code, 0, "{} {}", r.out, r.err);
    assert!(r.out.contains('█'), "{}", r.out);
    assert!(
        r.out.contains("housing.rent") && r.out.contains("food"),
        "{}",
        r.out
    );
    // the biggest row gets the longest bar (40 cells), the smaller a shorter one
    let bars: Vec<usize> = r
        .out
        .lines()
        .map(|l| l.matches('█').count())
        .filter(|n| *n > 0)
        .collect();
    assert_eq!(bars.iter().max(), Some(&40));
    assert!(bars.iter().min() < bars.iter().max());
    // negative balances use a different glyph
    c.ok(&[
        "acct",
        "add",
        "--name",
        "Card",
        "--kind",
        "credit_card",
        "--opening",
        "0",
    ]);
    c.ok(&["txn", "add", "--account", "Card", "--amount", "-100"]);
    let r = c.run(&["acct", "list", "-o", "table", "--chart", "bar"]);
    assert!(r.out.contains('▒') && r.out.contains('█'), "{}", r.out);
    // machine formats refuse, masked amounts produce no chart (and no leak)
    assert_eq!(
        c.run(&["acct", "list", "-o", "json", "--chart", "bar"])
            .code,
        2
    );
    let p = c.run(&["acct", "list", "-o", "table", "--chart", "bar", "--private"]);
    assert!(
        !p.out.contains('█') && p.err.contains("no chart"),
        "{} / {}",
        p.out,
        p.err
    );
    // other chart types are rejected up front
    assert_eq!(c.run(&["acct", "list", "--chart", "pie"]).code, 2);
    // results without rows just warn
    let r = c.run(&["doctor", "-o", "table", "--chart", "bar"]);
    assert_eq!(r.code, 0);
    assert!(r.err.contains("no chart"));
}
