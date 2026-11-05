//! Typed access to operation arguments (a JSON object built by the CLI, the
//! MCP server or the HTTP API).

use chrono::NaiveDate;
use pext_core::dates::parse_date;
use pext_core::money::parse_amount;
use pext_core::{Error, Result};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Default)]
pub struct Args {
    pub map: Map<String, Value>,
}

fn missing(k: &str) -> Error {
    Error::usage(
        "missing_argument",
        format!("missing required argument '{k}'"),
    )
    .with_field(k)
}

impl Args {
    pub fn new(map: Map<String, Value>) -> Self {
        Args { map }
    }

    pub fn has(&self, k: &str) -> bool {
        self.map.get(k).is_some_and(|v| !v.is_null())
    }

    pub fn opt_str(&self, k: &str) -> Option<String> {
        match self.map.get(k) {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        }
    }

    pub fn str(&self, k: &str) -> Result<String> {
        self.opt_str(k).ok_or_else(|| missing(k))
    }

    pub fn opt_i64(&self, k: &str) -> Result<Option<i64>> {
        match self.map.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => n.as_i64().map(Some).ok_or_else(|| {
                Error::validation("invalid_integer", format!("'{k}' must be an integer"))
                    .with_field(k)
            }),
            Some(Value::String(s)) => s.trim().parse::<i64>().map(Some).map_err(|_| {
                Error::validation("invalid_integer", format!("'{k}' must be an integer"))
                    .with_field(k)
            }),
            _ => Err(
                Error::validation("invalid_integer", format!("'{k}' must be an integer"))
                    .with_field(k),
            ),
        }
    }

    pub fn i64(&self, k: &str) -> Result<i64> {
        self.opt_i64(k)?.ok_or_else(|| missing(k))
    }

    pub fn flag(&self, k: &str) -> bool {
        match self.map.get(k) {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => matches!(s.as_str(), "true" | "1" | "yes"),
            _ => false,
        }
    }

    pub fn list(&self, k: &str) -> Vec<String> {
        match self.map.get(k) {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect(),
            Some(Value::String(s)) => vec![s.clone()],
            _ => vec![],
        }
    }

    pub fn opt_date(&self, k: &str) -> Result<Option<NaiveDate>> {
        match self.opt_str(k) {
            Some(s) => Ok(Some(parse_date(&s).map_err(|e| e.with_field(k))?)),
            None => Ok(None),
        }
    }

    pub fn date(&self, k: &str) -> Result<NaiveDate> {
        self.opt_date(k)?.ok_or_else(|| missing(k))
    }

    /// Money given either as a decimal string in `key` (major units) or as an
    /// integer in `key_minor`. Never both.
    pub fn money(&self, key: &str, decimals: u32) -> Result<Option<i64>> {
        let minor_key = format!("{key}_minor");
        match (self.has(key), self.has(&minor_key)) {
            (true, true) => Err(Error::usage(
                "ambiguous_amount",
                format!("pass either '{key}' or '{minor_key}', not both"),
            )
            .with_field(key)),
            (true, false) => {
                let s = self.str(key)?;
                Ok(Some(
                    parse_amount(&s, decimals).map_err(|e| e.with_field(key))?,
                ))
            }
            (false, true) => {
                let v = self.i64(&minor_key)?;
                Ok(Some(
                    pext_core::money::check_amount(v).map_err(|e| e.with_field(minor_key))?,
                ))
            }
            (false, false) => Ok(None),
        }
    }

    pub fn req_money(&self, key: &str, decimals: u32) -> Result<i64> {
        self.money(key, decimals)?.ok_or_else(|| {
            Error::usage(
                "missing_argument",
                format!("provide '{key}' (major units) or '{key}_minor'"),
            )
            .with_field(key)
        })
    }
}
