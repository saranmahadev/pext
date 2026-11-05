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
    fn data(&self) -> Value {
        let v: Value = serde_json::from_str(&self.out)
            .unwrap_or_else(|e| panic!("not json ({e}): {}\nstderr: {}", self.out, self.err));
        v["data"].clone()
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
            db: Some(self.dir.path().join("p3.db").to_string_lossy().to_string()),
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
fn wealth_from_the_command_line() {
    let c = Cli::new();
    c.ok(&[
        "acct",
        "add",
        "--name",
        "Bank",
        "--kind",
        "checking",
        "--opening",
        "250000",
    ]);

    // loans
    let l = c.ok(&[
        "loan",
        "add",
        "--name",
        "Home",
        "--kind",
        "home",
        "--principal",
        "5000000",
        "--rate",
        "8.5",
        "--months",
        "240",
        "--start",
        "2026-01-05",
    ]);
    assert_eq!(l["emi_minor"], 4_339_116);
    let s = c.ok(&[
        "loan", "schedule", "Home", "--extra", "10000", "--limit", "3",
    ]);
    assert_eq!(s["rows"].as_array().unwrap().len(), 3);
    assert!(s["months_saved"].as_i64().unwrap() > 0);
    let p = c.ok(&[
        "debt",
        "plan",
        "--extra",
        "5000",
        "--debt",
        "Visa:50000:36:2500",
        "--debt",
        "Phone:20000:0:1000",
    ]);
    assert_eq!(p["payoff_order"].as_array().unwrap().len(), 3);

    // IOUs
    c.ok(&[
        "iou",
        "add",
        "--person",
        "Ravi",
        "--direction",
        "lent",
        "--amount",
        "5000",
    ]);
    c.ok(&["iou", "settle", "1", "--amount", "1000"]);
    assert_eq!(c.ok(&["iou", "summary"])["owed_to_me_minor"], 400_000);

    // assets
    c.ok(&[
        "asset", "add", "--name", "Flat", "--kind", "property", "--value", "9000000",
    ]);

    // investments (positional price/target arguments)
    c.ok(&["invest", "add", "--symbol", "NIFTYBEES", "--kind", "etf"]);
    c.ok(&[
        "trade",
        "add",
        "--instrument",
        "NIFTYBEES",
        "--side",
        "buy",
        "--quantity",
        "10",
        "--price",
        "245.5",
        "--fee",
        "12",
        "--date",
        "2026-10-01",
    ]);
    c.ok(&["price", "set", "NIFTYBEES", "260"]);
    let h = c.ok(&["invest", "holdings"]);
    assert_eq!(h["holdings"][0]["value_minor"], 260_000);
    c.ok(&["target", "set", "etf", "100"]);
    assert_eq!(
        c.ok(&["invest", "rebalance", "--contribute", "1000"])["moves"][0]["amount_minor"],
        100_000
    );

    // net worth, snapshot, forecast
    let nw = c.ok(&["networth"]);
    assert!(nw["net_worth_minor"].as_i64().unwrap() > 0);
    c.ok(&["snapshot", "take"]);
    assert_eq!(c.ok(&["snapshot", "list"]).as_array().unwrap().len(), 1);
    c.ok(&[
        "recur",
        "add",
        "--name",
        "rent",
        "--account",
        "Bank",
        "--amount",
        "20000",
        "--start",
        "2026-12-01",
    ]);
    let f = c.ok(&["cashflow", "forecast", "--days", "30"]);
    assert_eq!(f["events_total"], 1);
    c.ok(&["runway"]);

    // everything above left the books consistent
    assert_eq!(c.run(&["doctor"]).code, 0);
    // errors map to exit codes
    assert_eq!(c.run(&["loan", "get", "nope"]).code, 4);
    assert_eq!(
        c.run(&[
            "trade",
            "add",
            "--instrument",
            "NIFTYBEES",
            "--side",
            "sell",
            "--quantity",
            "99",
            "--price",
            "1"
        ])
        .code,
        3
    );
    assert_eq!(c.run(&["loan", "rm", "Home"]).code, 2); // needs --yes
}

#[test]
fn wealth_renders_in_every_format() {
    let c = Cli::new();
    c.ok(&[
        "acct",
        "add",
        "--name",
        "Bank",
        "--kind",
        "checking",
        "--opening",
        "250000",
    ]);
    c.ok(&[
        "asset", "add", "--name", "Flat", "--kind", "property", "--value", "9000000",
    ]);
    for fmt in ["table", "md", "csv", "html", "yaml", "jsonl"] {
        let r = c.run(&["networth", "-o", fmt]);
        assert_eq!(r.code, 0, "{fmt}: {} {}", r.out, r.err);
        assert!(r.out.contains("Flat"), "{fmt}: {}", r.out);
    }
    let t = c.run(&["networth", "-o", "table"]);
    assert!(
        t.out.contains("90,00,000.00"),
        "Indian grouping expected: {}",
        t.out
    ); // 9,000,000 in lakh grouping
    let csv = c.run(&["networth", "-o", "csv"]);
    assert!(
        csv.out.lines().next().unwrap().contains("group"),
        "{}",
        csv.out
    ); // flattened to item rows
    assert_eq!(csv.out.lines().count(), 3, "{}", csv.out);
}

#[test]
fn phase3_commands_are_discoverable() {
    let c = Cli::new();
    let cmds = c.ok(&["commands"]);
    let names: Vec<&str> = cmds
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap())
        .collect();
    for n in [
        "loan.add",
        "loan.schedule",
        "debt.plan",
        "credit.utilization",
        "iou.add",
        "iou.summary",
        "asset.add",
        "invest.add",
        "trade.add",
        "price.set",
        "invest.holdings",
        "invest.performance",
        "invest.allocation",
        "invest.rebalance",
        "target.set",
        "networth",
        "snapshot.take",
        "cashflow.forecast",
        "runway",
    ] {
        assert!(names.contains(&n), "missing {n}");
    }
    let s = c.ok(&["schema", "trade.add"]);
    assert_eq!(s["mutating"], true);
    assert!(s["input_schema"]["properties"]["quantity"].is_object());
}
