use pext_cli::{run, Env};
use serde_json::Value;
use tempfile::TempDir;

struct Cli {
    dir: TempDir,
    tty: bool,
}

struct Out {
    code: i32,
    out: String,
    err: String,
}

impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(&self.out)
            .unwrap_or_else(|e| panic!("not json ({e}): {}\nstderr: {}", self.out, self.err))
    }
}

impl Cli {
    fn new() -> Cli {
        let c = Cli {
            dir: TempDir::new().unwrap(),
            tty: false,
        };
        assert_eq!(c.run(&["init"]).code, 0);
        c
    }
    fn env(&self) -> Env {
        Env {
            is_tty: self.tty,
            db: Some(self.dir.path().join("e2e.db").to_string_lossy().to_string()),
            today: Some("2026-12-31".into()),
            ..Default::default()
        }
    }
    fn run(&self, args: &[&str]) -> Out {
        let mut argv = vec!["pext".to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let code = run(argv, &self.env(), &mut o, &mut e);
        Out {
            code,
            out: String::from_utf8(o).unwrap(),
            err: String::from_utf8(e).unwrap(),
        }
    }
}

#[test]
fn json_is_default_when_piped_and_table_on_a_terminal() {
    let mut c = Cli::new();
    let r = c.run(&[
        "acct",
        "add",
        "--name",
        "HDFC",
        "--kind",
        "checking",
        "--opening",
        "1000",
    ]);
    assert_eq!(r.code, 0);
    let v = r.json();
    assert_eq!(v["schema"], "pext/v1");
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["balance_minor"], 100000);
    c.tty = true;
    let t = c.run(&["acct", "list"]);
    assert!(t.out.contains("ID") && t.out.contains("NAME"), "{}", t.out);
    assert!(t.out.contains("1,000.00"), "{}", t.out);
    assert!(!t.out.contains('{'));
}

#[test]
fn negative_amounts_parse_as_values_not_flags() {
    let c = Cli::new();
    c.run(&["acct", "add", "--name", "A", "--kind", "checking"]);
    let r = c.run(&[
        "txn",
        "add",
        "--account",
        "A",
        "--amount",
        "-450.50",
        "--payee",
        "Swiggy",
        "--category",
        "food.delivery",
    ]);
    assert_eq!(r.code, 0, "{}", r.out);
    assert_eq!(r.json()["data"]["amount_minor"], -45050);
    let b = c.run(&["acct", "balance", "A"]);
    assert_eq!(b.json()["data"]["balance_minor"], -45050);
}

#[test]
fn exit_codes_follow_the_error_kind() {
    let c = Cli::new();
    c.run(&["acct", "add", "--name", "A", "--kind", "checking"]);
    let code = |args: &[&str]| c.run(args).code;
    assert_eq!(code(&["bogus"]), 2);
    assert_eq!(code(&["txn", "add", "--nope", "1"]), 2);
    assert_eq!(code(&["txn", "add", "--account", "A"]), 2); // missing amount
    assert_eq!(
        code(&["txn", "add", "--account", "A", "--amount", "1.005"]),
        3
    );
    assert_eq!(code(&["txn", "get", "999"]), 4);
    assert_eq!(code(&["acct", "add", "--name", "a", "--kind", "cash"]), 5);
    assert_eq!(code(&["-o", "xml", "acct", "list"]), 2);
    // missing database
    let mut env = c.env();
    env.db = Some(c.dir.path().join("none.db").to_string_lossy().to_string());
    let (mut o, mut e) = (Vec::new(), Vec::new());
    let code = run(
        ["pext".to_string(), "acct".into(), "list".into()],
        &env,
        &mut o,
        &mut e,
    );
    assert_eq!(code, 4);
}

#[test]
fn errors_are_structured_json_on_stdout_when_piped() {
    let c = Cli::new();
    let r = c.run(&["txn", "get", "999"]);
    assert_eq!(r.code, 4);
    let v = r.json();
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["kind"], "not_found");
    assert!(v["error"]["code"].is_string());
    assert!(r.err.is_empty());
    let u = c.run(&["txn", "add", "--wat"]);
    assert_eq!(u.json()["error"]["kind"], "usage");
}

#[test]
fn human_errors_go_to_stderr_on_a_terminal() {
    let mut c = Cli::new();
    c.tty = true;
    let r = c.run(&["txn", "get", "999"]);
    assert_eq!(r.code, 4);
    assert!(r.out.is_empty());
    assert!(
        r.err.starts_with("error[txn_not_found]") || r.err.starts_with("error["),
        "{}",
        r.err
    );
}

#[test]
fn output_formats() {
    let c = Cli::new();
    c.run(&["acct", "add", "--name", "A", "--kind", "checking"]);
    c.run(&[
        "txn",
        "add",
        "--account",
        "A",
        "--amount",
        "-10",
        "--payee",
        "x,\"y\"",
    ]);
    let csv = c.run(&["txn", "list", "-o", "csv"]);
    assert!(csv.out.starts_with("id,date,account,amount"), "{}", csv.out);
    assert!(csv.out.contains("\"x,\"\"y\"\"\""), "{}", csv.out);
    let jl = c.run(&["txn", "list", "-o", "jsonl"]);
    assert_eq!(jl.out.lines().count(), 1);
    let md = c.run(&["txn", "list", "-o", "md"]);
    assert!(md.out.starts_with("| id"), "{}", md.out);
    let html = c.run(&["txn", "list", "-o", "html"]);
    assert!(html.out.starts_with("<!doctype html>"));
    let yaml = c.run(&["txn", "list", "-o", "yaml"]);
    assert!(yaml.out.contains("\"ok\": true"), "{}", yaml.out);
    let pretty = c.run(&["txn", "list", "--pretty"]);
    assert!(pretty.out.lines().count() > 5);
}

#[test]
fn fields_flag_selects_columns_and_rejects_unknown() {
    let c = Cli::new();
    c.run(&["acct", "add", "--name", "A", "--kind", "checking"]);
    c.run(&["txn", "add", "--account", "A", "--amount", "-10"]);
    let r = c.run(&["txn", "list", "--fields", "id,amount_minor", "-o", "jsonl"]);
    assert_eq!(r.out.trim(), r#"{"id":1,"amount_minor":-1000}"#);
    let bad = c.run(&["txn", "list", "--fields", "zzz"]);
    assert_eq!(bad.code, 2);
    assert_eq!(bad.json()["error"]["code"], "unknown_field");
}

#[test]
fn dry_run_and_yes_and_idempotency() {
    let c = Cli::new();
    c.run(&["acct", "add", "--name", "A", "--kind", "checking"]);
    let d = c.run(&[
        "txn",
        "add",
        "--account",
        "A",
        "--amount",
        "-10",
        "--dry-run",
    ]);
    assert_eq!(d.json()["meta"]["dry_run"], true);
    assert_eq!(
        c.run(&["txn", "list"]).json()["data"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let a = c.run(&[
        "txn",
        "add",
        "--account",
        "A",
        "--amount",
        "-10",
        "--idempotency-key",
        "k-1",
    ]);
    let b = c.run(&[
        "txn",
        "add",
        "--account",
        "A",
        "--amount",
        "-10",
        "--idempotency-key",
        "k-1",
    ]);
    assert_eq!(a.json()["data"]["id"], b.json()["data"]["id"]);
    assert_eq!(b.json()["meta"]["replayed"], true);
    assert_eq!(
        c.run(&["txn", "list"]).json()["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let r = c.run(&["import", "rollback", "1"]);
    assert_eq!(r.code, 2, "needs --yes: {}", r.out);
    assert_eq!(r.json()["error"]["code"], "confirmation_required");
}

#[test]
fn out_flag_writes_a_file() {
    let c = Cli::new();
    c.run(&["acct", "add", "--name", "A", "--kind", "cash"]);
    let path = c.dir.path().join("accts.csv");
    let r = c.run(&["acct", "list", "-o", "csv", "--out", path.to_str().unwrap()]);
    assert_eq!(r.code, 0);
    assert!(r.out.is_empty());
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .starts_with("id,name"));
}

#[test]
fn introspection_commands() {
    let c = Cli::new();
    let cmds = c.run(&["commands"]).json();
    let names: Vec<&str> = cmds["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"txn.add") && names.contains(&"undo"));
    let s = c.run(&["schema", "txn.add"]).json();
    assert_eq!(s["data"]["input_schema"]["required"][0], "account");
    assert_eq!(c.run(&["schema", "txn", "add"]).code, 2); // one positional only
    assert_eq!(c.run(&["schema", "nope"]).code, 4);
    let v = c.run(&["version"]).json();
    assert_eq!(v["data"]["api"], "pext/v1");
}

#[test]
fn help_and_version_flags() {
    let c = Cli::new();
    let h = c.run(&["--help"]);
    assert_eq!(h.code, 0);
    assert!(h.out.contains("txn") && h.out.contains("acct"));
    let h2 = c.run(&["txn", "add", "--help"]);
    assert_eq!(h2.code, 0);
    assert!(
        h2.out.contains("--amount") && h2.out.contains("Examples:") || h2.out.contains("--amount")
    );
    assert!(c.run(&["--version"]).out.starts_with("pext "));
    let group = c.run(&["txn"]);
    assert_eq!(group.code, 2); // a group without a subcommand shows usage and fails
}

#[test]
fn warnings_do_not_pollute_json_stdout() {
    let c = Cli::new();
    let r = c.run(&["doctor"]);
    assert_eq!(r.code, 0);
    assert_eq!(r.json()["data"]["ok"], true);
}

#[test]
fn full_money_workflow() {
    let c = Cli::new();
    c.run(&[
        "acct",
        "add",
        "--name",
        "Bank",
        "--kind",
        "checking",
        "--opening",
        "50000",
    ]);
    c.run(&["acct", "add", "--name", "Wallet", "--kind", "cash"]);
    c.run(&[
        "txn",
        "add",
        "--account",
        "Bank",
        "--amount",
        "85000",
        "--category",
        "salary",
        "--date",
        "2026-12-01",
    ]);
    c.run(&[
        "txn",
        "add",
        "--account",
        "Bank",
        "--amount",
        "-1200.75",
        "--category",
        "food.groceries",
        "--date",
        "2026-12-02",
    ]);
    c.run(&[
        "txn",
        "transfer",
        "--from",
        "Bank",
        "--to",
        "Wallet",
        "--amount",
        "2000",
        "--date",
        "2026-12-03",
    ]);
    let b = c.run(&["acct", "balance", "Bank"]).json();
    assert_eq!(
        b["data"]["balance_minor"],
        5_000_000 + 8_500_000 - 120_075 - 200_000
    );
    let u = c.run(&["undo"]);
    assert_eq!(u.code, 0);
    let b2 = c.run(&["acct", "balance", "Bank"]).json();
    assert_eq!(b2["data"]["balance_minor"], 5_000_000 + 8_500_000 - 120_075);
    assert_eq!(c.run(&["audit", "verify"]).json()["data"]["ok"], true);
    assert_eq!(c.run(&["doctor"]).code, 0);
}
