#![allow(dead_code)]
#![allow(clippy::result_large_err)]

use chrono::NaiveDate;
use pext_core::Error;
use pext_engine::{Actor, CallOpts, Engine, EngineConfig, Output};
use serde_json::{json, Map, Value};
use tempfile::TempDir;

pub struct H {
    pub dir: TempDir,
    pub e: Engine,
}

pub fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()
}

impl H {
    /// A fresh, initialised database (INR, default categories).
    pub fn new() -> H {
        let h = H::bare();
        h.ok("init", json!({}));
        h
    }

    pub fn bare() -> H {
        H::bare_on(today())
    }

    pub fn bare_on(d: NaiveDate) -> H {
        let dir = TempDir::new().unwrap();
        let mut cfg = EngineConfig::new(dir.path().join("t.db"));
        cfg.today = Some(d);
        cfg.kdf = pext_engine::crypto::KdfParams::insecure_fast();
        cfg.passphrase = Some(pext_engine::crypto::Secret("correct horse battery".into()));
        H {
            dir,
            e: Engine::new(cfg),
        }
    }

    /// Initialised database where "today" is the given YYYY-MM-DD.
    pub fn on(date: &str) -> H {
        let h = H::bare_on(NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap());
        h.ok("init", json!({}));
        h
    }

    /// Run an op as if "today" were a different date.
    pub fn call_on(&self, date: &str, op: &str, args: Value) -> Result<Output, Error> {
        let mut cfg = self.e.cfg.clone();
        cfg.today = Some(NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap());
        let m = match args {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        Engine::new(cfg).call(op, m, &CallOpts::default(), &Actor::owner())
    }

    /// Create a token as the owner and return (secret, actor).
    pub fn token(&self, args: Value) -> (String, Actor) {
        let secret = self.ok("token.create", args)["token"]
            .as_str()
            .unwrap()
            .to_string();
        let actor = self.e.authenticate(&secret).unwrap();
        (secret, actor)
    }

    pub fn as_actor(&self, actor: &Actor, op: &str, args: Value) -> Result<Output, Error> {
        self.raw(op, args, &CallOpts::default(), actor)
    }

    pub fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("t.db")
    }

    pub fn raw(
        &self,
        op: &str,
        args: Value,
        opts: &CallOpts,
        actor: &Actor,
    ) -> Result<Output, Error> {
        let m: Map<String, Value> = match args {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        self.e.call(op, m, opts, actor)
    }

    pub fn call(&self, op: &str, args: Value) -> Result<Output, Error> {
        self.raw(op, args, &CallOpts::default(), &Actor::owner())
    }

    pub fn ok(&self, op: &str, args: Value) -> Value {
        match self.call(op, args.clone()) {
            Ok(o) => o.data,
            Err(e) => panic!("{op} {args} failed: {} ({})", e.message, e.code),
        }
    }

    pub fn err(&self, op: &str, args: Value) -> Error {
        match self.call(op, args.clone()) {
            Ok(o) => panic!("{op} {args} unexpectedly succeeded: {}", o.data),
            Err(e) => e,
        }
    }

    pub fn dry(&self, op: &str, args: Value) -> Output {
        let opts = CallOpts {
            dry_run: true,
            ..Default::default()
        };
        self.raw(op, args, &opts, &Actor::owner()).unwrap()
    }

    pub fn yes(&self, op: &str, args: Value) -> Result<Output, Error> {
        let opts = CallOpts {
            yes: true,
            ..Default::default()
        };
        self.raw(op, args, &opts, &Actor::owner())
    }

    pub fn conn(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.path()).unwrap()
    }

    pub fn acct(&self, name: &str, kind: &str, opening: &str) -> i64 {
        self.ok(
            "acct.add",
            json!({"name": name, "kind": kind, "opening": opening}),
        )["id"]
            .as_i64()
            .unwrap()
    }

    pub fn txn(&self, account: &str, amount: &str, date: &str, category: Option<&str>) -> i64 {
        let mut a = json!({"account": account, "amount": amount, "date": date});
        if let Some(c) = category {
            a["category"] = json!(c);
        }
        self.ok("txn.add", a)["id"].as_i64().unwrap()
    }

    pub fn balance(&self, account: &str) -> i64 {
        self.ok("acct.balance", json!({"account": account}))["balance_minor"]
            .as_i64()
            .unwrap()
    }

    /// Canonical dump of all user data (everything except audit/idempotency).
    pub fn dump(&self) -> String {
        let c = self.conn();
        let mut tables: Vec<String> = c
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        tables.retain(|t| t != "audit" && t != "idempotency");
        let mut out = String::new();
        for t in tables {
            let rows =
                pext_engine::ctx::query_json(&c, &format!("SELECT * FROM {t} ORDER BY 1"), &[])
                    .unwrap();
            out.push_str(&format!("## {t}\n"));
            for r in rows {
                out.push_str(&Value::Object(r).to_string());
                out.push('\n');
            }
        }
        out
    }

    pub fn doctor_ok(&self) {
        let d = self.ok("doctor", json!({}));
        assert_eq!(d["ok"], true, "doctor failed: {d}");
    }
}
