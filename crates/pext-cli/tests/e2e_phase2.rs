use pext_cli::{run, Env};
use serde_json::Value;
use tempfile::TempDir;

struct Cli {
    dir: TempDir,
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
    fn data(&self) -> Value {
        self.json()["data"].clone()
    }
}

impl Cli {
    fn new() -> Cli {
        let c = Cli {
            dir: TempDir::new().unwrap(),
        };
        assert_eq!(c.run(&["init"]).code, 0);
        c
    }
    fn run(&self, args: &[&str]) -> Out {
        let env = Env {
            is_tty: false,
            db: Some(self.dir.path().join("p2.db").to_string_lossy().to_string()),
            today: Some("2026-11-15".into()),
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
}

#[test]
fn plan_and_track_from_the_command_line() {
    let c = Cli::new();
    c.ok(&[
        "acct",
        "add",
        "--name",
        "Bank",
        "--kind",
        "checking",
        "--opening",
        "100000",
    ]);
    c.ok(&[
        "txn",
        "add",
        "--account",
        "Bank",
        "--amount",
        "-750.50",
        "--date",
        "2026-11-02",
        "--category",
        "food.dining",
    ]);
    c.ok(&[
        "txn",
        "add",
        "--account",
        "Bank",
        "--amount",
        "85000",
        "--date",
        "2026-11-01",
        "--category",
        "salary",
    ]);

    // query language + explain
    let q = c.ok(&["query", "category:food date:this-month"]);
    assert_eq!(q.as_array().unwrap().len(), 1);
    let e = c.ok(&["query", "category:food amount:>500", "--explain"]);
    assert!(e["match_any_of"].is_array());
    let g = c.ok(&["query", "type:expense group:category agg:sum"]);
    assert_eq!(g[0]["sum_minor"], -75050);
    assert_eq!(c.run(&["query", "nonsense:field"]).code, 2);

    // saved view
    c.ok(&["view", "save", "eating", "category:food"]);
    assert_eq!(
        c.ok(&["view", "run", "eating"]).as_array().unwrap().len(),
        1
    );

    // budget and alert
    c.ok(&[
        "budget",
        "set",
        "--category",
        "food",
        "--amount",
        "900",
        "--start",
        "2026-11-01",
    ]);
    let st = c.ok(&["budget", "status"]);
    assert_eq!(st["budgets"][0]["state"], "warning");
    let r = c.run(&["alert", "check", "--fail-on", "warning"]);
    assert_eq!(r.code, 10, "alert check must exit 10: {} {}", r.out, r.err);
    assert_eq!(r.json()["ok"], true);
    assert_eq!(c.run(&["alert", "check", "--fail-on", "critical"]).code, 0);

    // recurring, due, post
    c.ok(&[
        "recur",
        "add",
        "--name",
        "rent",
        "--account",
        "Bank",
        "--amount",
        "25000",
        "--start",
        "2026-11-01",
    ]);
    assert!(!c.ok(&["recur", "due"]).as_array().unwrap().is_empty());
    assert_eq!(c.ok(&["recur", "post"])["posted"], 1);
    assert_eq!(c.ok(&["recur", "post"])["posted"], 0);

    // goals
    c.ok(&[
        "goal",
        "add",
        "--name",
        "Trip",
        "--target",
        "50000",
        "--by",
        "2027-06-30",
    ]);
    c.ok(&["goal", "contribute", "Trip", "--amount", "5000"]);
    assert_eq!(c.ok(&["goal", "status"])[0]["saved_minor"], 500_000);

    // reports
    let s = c.ok(&["report", "summary"]);
    assert_eq!(s["income_minor"], 8_500_000);
    assert_eq!(s["expense_minor"], 75_050 + 2_500_000);
    let t = c.run(&["report", "spending", "--by", "category", "-o", "csv"]);
    assert_eq!(t.code, 0);
    assert!(
        t.out.starts_with(
            "period_start,period_end,currency,kind,by,total,name,amount,share_pct,count"
        ),
        "{}",
        t.out
    );
    assert_eq!(t.out.lines().count(), 3, "{}", t.out);

    // fx
    c.ok(&[
        "fx", "set", "--from", "USD", "--to", "INR", "--rate", "83.5",
    ]);
    assert_eq!(
        c.ok(&["fx", "convert", "--from", "USD", "--to", "INR", "--amount", "10"])
            ["converted_minor"],
        83_500
    );

    // everything above left the ledger consistent
    let d = c.run(&["doctor"]);
    assert_eq!(d.code, 0, "{}", d.out);
}

#[test]
fn phase2_commands_are_discoverable() {
    let c = Cli::new();
    let cmds = c.ok(&["commands"]);
    let names: Vec<&str> = cmds
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap())
        .collect();
    for n in [
        "query",
        "view.save",
        "rule.add",
        "fx.set",
        "budget.status",
        "recur.post",
        "sub.detect",
        "goal.status",
        "report.summary",
        "alert.check",
    ] {
        assert!(names.contains(&n), "missing {n}");
    }
    let s = c.ok(&["schema", "budget.set"]);
    assert!(s["input_schema"]["properties"]["amount"].is_object());
    let r = c.run(&["report", "spending", "--by", "colour"]);
    assert_eq!(r.code, 3);
}
