//! The operation registry: one declarative table drives the CLI (clap tree and
//! help), the MCP tool list, the HTTP API, `pext commands` and `pext schema`.

use crate::args::Args;
use crate::ctx::{Ctx, Role};
use chrono::NaiveDate;
use pext_core::dates::parse_date_arg;
use pext_core::money::parse_decimal;
use pext_core::{Error, Result};
use serde_json::{json, Map, Value};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Str,
    Int,
    Bool,
    /// Decimal string in major units (never a float).
    Amount,
    Date,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Str => "string",
            Kind::Int => "integer",
            Kind::Bool => "boolean",
            Kind::Amount => "amount",
            Kind::Date => "date",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Param {
    pub name: &'static str,
    pub kind: Kind,
    pub help: &'static str,
    pub required: bool,
    pub multi: bool,
    pub positional: bool,
    pub default: Option<&'static str>,
    pub choices: &'static [&'static str],
}

impl Param {
    fn new(name: &'static str, kind: Kind, help: &'static str) -> Self {
        Param {
            name,
            kind,
            help,
            required: false,
            multi: false,
            positional: false,
            default: None,
            choices: &[],
        }
    }
    pub fn s(name: &'static str, help: &'static str) -> Self {
        Self::new(name, Kind::Str, help)
    }
    pub fn i(name: &'static str, help: &'static str) -> Self {
        Self::new(name, Kind::Int, help)
    }
    pub fn b(name: &'static str, help: &'static str) -> Self {
        Self::new(name, Kind::Bool, help)
    }
    pub fn amount(name: &'static str, help: &'static str) -> Self {
        Self::new(name, Kind::Amount, help)
    }
    pub fn date(name: &'static str, help: &'static str) -> Self {
        Self::new(name, Kind::Date, help)
    }
    pub fn req(mut self) -> Self {
        self.required = true;
        self
    }
    pub fn multi(mut self) -> Self {
        self.multi = true;
        self
    }
    pub fn pos(mut self) -> Self {
        self.positional = true;
        self
    }
    pub fn def(mut self, d: &'static str) -> Self {
        self.default = Some(d);
        self
    }
    pub fn choices(mut self, c: &'static [&'static str]) -> Self {
        self.choices = c;
        self
    }
}

/// A money parameter is declared as a pair: `name` (decimal, major units) and
/// `name_minor` (integer minor units).
pub fn money(name: &'static str, minor_name: &'static str, help: &'static str) -> Vec<Param> {
    vec![
        Param::amount(name, help),
        Param::i(
            minor_name,
            "same amount as an integer in minor units (cents/paise)",
        ),
    ]
}

pub type Handler = fn(&Ctx, &Args) -> Result<Value>;

pub struct OpSpec {
    pub name: &'static str,
    pub about: &'static str,
    pub access: Role,
    pub mutating: bool,
    pub needs_db: bool,
    pub creates_db: bool,
    /// Usable by tokens restricted to specific accounts.
    pub scoped_ok: bool,
    pub params: Vec<Param>,
    pub examples: Vec<&'static str>,
    pub handler: Handler,
}

impl OpSpec {
    pub fn new(name: &'static str, about: &'static str, handler: Handler) -> Self {
        OpSpec {
            name,
            about,
            access: Role::Read,
            mutating: false,
            needs_db: true,
            creates_db: false,
            scoped_ok: false,
            params: vec![],
            examples: vec![],
            handler,
        }
    }
    pub fn write(mut self) -> Self {
        self.mutating = true;
        self.access = Role::Write;
        self
    }
    pub fn append(mut self) -> Self {
        self.mutating = true;
        self.access = Role::Append;
        self
    }
    pub fn admin(mut self) -> Self {
        self.access = Role::Admin;
        self
    }
    pub fn admin_write(mut self) -> Self {
        self.mutating = true;
        self.access = Role::Admin;
        self
    }
    pub fn no_db(mut self) -> Self {
        self.needs_db = false;
        self
    }
    pub fn creates(mut self) -> Self {
        self.creates_db = true;
        self
    }
    pub fn scoped(mut self) -> Self {
        self.scoped_ok = true;
        self
    }
    pub fn p(mut self, p: Param) -> Self {
        self.params.push(p);
        self
    }
    pub fn ps(mut self, ps: Vec<Param>) -> Self {
        self.params.extend(ps);
        self
    }
    pub fn ex(mut self, e: &'static str) -> Self {
        self.examples.push(e);
        self
    }
}

/// Descriptions for command groups (the nouns in `pext <noun> <verb>`).
pub const GROUPS: &[(&str, &str)] = &[
    (
        "acct",
        "Accounts: bank, card, cash, investment and loan accounts",
    ),
    ("txn", "Transactions: add, list, edit, split, transfer, tag"),
    ("cat", "Categories (hierarchical, e.g. food.delivery)"),
    ("payee", "Payees and merchant-name normalisation"),
    ("tag", "Tags"),
    (
        "import",
        "Import statements (CSV, OFX) with de-duplication and rollback",
    ),
    ("audit", "Tamper-evident audit log"),
    ("config", "Settings"),
    ("budget", "Budgets with rollover"),
    ("rule", "Auto-categorisation rules"),
    ("recur", "Recurring items: bills, EMIs, salary"),
    ("sub", "Subscription detection"),
    ("goal", "Savings goals"),
    ("view", "Saved queries"),
    ("report", "Reports"),
    ("alert", "Alerts for budgets, bills and balances"),
    ("loan", "Loans and amortisation"),
    ("debt", "Debt payoff planning"),
    ("credit", "Credit utilisation"),
    ("iou", "Money lent and borrowed"),
    ("invest", "Investments, trades, prices and performance"),
    ("networth", "Net worth"),
    ("asset", "Manual assets (property, vehicles, ...)"),
    ("fx", "Exchange rates"),
    ("cashflow", "Cash flow, runway and forecasts"),
    ("plugin", "Plugins (importers, price feeds)"),
    ("backup", "Encrypted backups"),
    ("sync", "Snapshot sync through a shared folder"),
    (
        "token",
        "Scoped access tokens for agents and household members",
    ),
    ("invest.trade", "Buy, sell and dividend records"),
    ("invest.price", "Instrument prices"),
    ("invest.alloc", "Asset allocation and rebalancing"),
    ("invest.instrument", "Instruments (stocks, funds, ...)"),
    ("payee.alias", "Payee aliases"),
];

pub fn all() -> &'static [OpSpec] {
    static REG: OnceLock<Vec<OpSpec>> = OnceLock::new();
    REG.get_or_init(crate::ops::all_ops)
}

pub fn find(name: &str) -> Option<&'static OpSpec> {
    all().iter().find(|o| o.name == name)
}

fn bad_arg(p: &Param, msg: String) -> Error {
    Error::validation("invalid_argument", msg).with_field(p.name)
}

fn coerce(p: &Param, v: &Value, today: NaiveDate) -> Result<Value> {
    match p.kind {
        Kind::Str => {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return Err(bad_arg(p, format!("'{}' must be a string", p.name))),
            };
            if !p.choices.is_empty() {
                let l = s.to_ascii_lowercase();
                if p.choices.iter().any(|c| *c == l) {
                    return Ok(Value::String(l));
                }
                return Err(bad_arg(
                    p,
                    format!("'{}' must be one of: {}", p.name, p.choices.join(", ")),
                ));
            }
            Ok(Value::String(s))
        }
        Kind::Int => match v {
            Value::Number(n) if n.as_i64().is_some() => Ok(v.clone()),
            Value::String(s) => s
                .trim()
                .parse::<i64>()
                .map(|i| json!(i))
                .map_err(|_| bad_arg(p, format!("'{}' must be an integer", p.name))),
            _ => Err(bad_arg(p, format!("'{}' must be an integer", p.name))),
        },
        Kind::Bool => match v {
            Value::Bool(_) => Ok(v.clone()),
            Value::String(s) if matches!(s.as_str(), "true" | "1" | "yes") => Ok(json!(true)),
            Value::String(s) if matches!(s.as_str(), "false" | "0" | "no") => Ok(json!(false)),
            _ => Err(bad_arg(p, format!("'{}' must be true or false", p.name))),
        },
        Kind::Amount => {
            let s =
                match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) if n.is_i64() => n.to_string(),
                    Value::Number(_) => return Err(Error::validation(
                        "float_amount",
                        format!("'{}' was given as a floating-point number", p.name),
                    )
                    .with_field(p.name)
                    .with_hint(
                        "pass amounts as strings (\"450.50\") or use the *_minor integer parameter",
                    )),
                    _ => return Err(bad_arg(p, format!("'{}' must be a decimal string", p.name))),
                };
            parse_decimal(&s).map_err(|e| e.with_field(p.name))?;
            Ok(Value::String(s.trim().to_string()))
        }
        Kind::Date => {
            let s = match v {
                Value::String(s) => s.clone(),
                _ => {
                    return Err(bad_arg(
                        p,
                        format!("'{}' must be a date (YYYY-MM-DD)", p.name),
                    ))
                }
            };
            let d = parse_date_arg(&s, today).map_err(|e| e.with_field(p.name))?;
            Ok(Value::String(d.format("%Y-%m-%d").to_string()))
        }
    }
}

/// Validate and normalise raw arguments against the spec.
pub fn validate(spec: &OpSpec, raw: Map<String, Value>, today: NaiveDate) -> Result<Args> {
    for k in raw.keys() {
        if !spec.params.iter().any(|p| p.name == k) {
            return Err(Error::usage(
                "unknown_argument",
                format!("'{}' does not accept argument '{k}'", spec.name),
            )
            .with_field(k.clone())
            .with_hint(format!(
                "run `pext commands` or `pext {} --help`",
                spec.name.replace('.', " ")
            )));
        }
    }
    let mut out = Map::new();
    for p in &spec.params {
        let given = raw.get(p.name).filter(|v| !v.is_null());
        match given {
            None => {
                if let Some(d) = p.default {
                    out.insert(p.name.into(), coerce(p, &Value::String(d.into()), today)?);
                } else if p.required {
                    return Err(Error::usage(
                        "missing_argument",
                        format!("missing required argument '{}'", p.name),
                    )
                    .with_field(p.name));
                }
            }
            Some(v) => {
                if p.multi {
                    let items: Vec<Value> = match v {
                        Value::Array(a) => a.clone(),
                        other => vec![other.clone()],
                    };
                    let mut vals = Vec::new();
                    for it in &items {
                        vals.push(coerce(p, it, today)?);
                    }
                    if p.required && vals.is_empty() {
                        return Err(Error::usage(
                            "missing_argument",
                            format!("missing required argument '{}'", p.name),
                        )
                        .with_field(p.name));
                    }
                    out.insert(p.name.into(), Value::Array(vals));
                } else {
                    out.insert(p.name.into(), coerce(p, v, today)?);
                }
            }
        }
    }
    Ok(Args::new(out))
}

/// JSON Schema (draft 2020-12 subset) for an operation's input.
pub fn input_schema(spec: &OpSpec) -> Value {
    let mut props = Map::new();
    let mut required = Vec::new();
    for p in &spec.params {
        let (ty, extra): (&str, Option<&str>) = match p.kind {
            Kind::Str => ("string", None),
            Kind::Int => ("integer", None),
            Kind::Bool => ("boolean", None),
            Kind::Amount => (
                "string",
                Some("decimal string in major units, e.g. \"-450.50\""),
            ),
            Kind::Date => ("string", Some("YYYY-MM-DD (or today/yesterday)")),
        };
        let mut desc = p.help.to_string();
        if let Some(e) = extra {
            desc = format!("{desc} [{e}]");
        }
        let mut item = json!({ "type": ty, "description": desc });
        if !p.choices.is_empty() {
            item["enum"] = json!(p.choices);
        }
        if let Some(d) = p.default {
            item["default"] = json!(d);
        }
        let item = if p.multi {
            json!({ "type": "array", "items": item })
        } else {
            item
        };
        props.insert(p.name.into(), item);
        if p.required {
            required.push(p.name);
        }
    }
    json!({ "type": "object", "properties": props, "required": required, "additionalProperties": false })
}

pub fn describe(spec: &OpSpec) -> Value {
    json!({
        "name": spec.name,
        "command": format!("pext {}", spec.name.replace('.', " ")),
        "about": spec.about,
        "access": match spec.access { Role::Read => "read", Role::Append => "append", Role::Write => "write", Role::Admin => "full" },
        "mutating": spec.mutating,
        "scoped_ok": spec.scoped_ok,
        "params": spec.params.iter().map(|p| json!({
            "name": p.name, "flag": format!("--{}", p.name.replace('_', "-")), "type": p.kind.name(),
            "required": p.required, "multiple": p.multi, "positional": p.positional,
            "default": p.default, "choices": p.choices, "help": p.help,
        })).collect::<Vec<_>>(),
        "examples": spec.examples,
    })
}
