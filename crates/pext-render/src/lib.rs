//! Output renderers. Every command returns JSON; this crate turns that JSON
//! into the format the person (or agent) asked for. Amount columns named
//! `*_minor` are shown as decimals in human formats using the row's
//! `currency` (or the default currency) and keep raw integers in JSON.
#![deny(clippy::float_arithmetic)]
#![allow(clippy::result_large_err)]

use pext_core::money::{decimals_for, format_amount, format_grouped};
use pext_core::{Error, Result};
use serde_json::{json, Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Table,
    Json,
    Jsonl,
    Csv,
    Md,
    Html,
    Yaml,
}

impl Format {
    pub const NAMES: &'static [&'static str] =
        &["table", "json", "jsonl", "csv", "md", "html", "yaml"];

    pub fn parse(s: &str) -> Result<Format> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "table" => Format::Table,
            "json" => Format::Json,
            "jsonl" | "ndjson" => Format::Jsonl,
            "csv" => Format::Csv,
            "md" | "markdown" => Format::Md,
            "html" => Format::Html,
            "yaml" | "yml" => Format::Yaml,
            other => {
                return Err(Error::usage(
                    "invalid_format",
                    format!("unknown output format '{other}'"),
                )
                .with_hint(format!("choose one of: {}", Format::NAMES.join(", "))))
            }
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Opts {
    /// Keep only these columns (applies to arrays of objects and objects).
    pub fields: Option<Vec<String>>,
    pub pretty: bool,
    pub color: bool,
    /// Currency used to format `*_minor` values that have no sibling `currency`.
    pub default_currency: Option<String>,
}

/// The standard success envelope.
pub fn envelope(data: &Value, meta: &Map<String, Value>) -> Value {
    json!({ "schema": "pext/v1", "ok": true, "data": data, "meta": meta })
}

/// The standard error envelope.
pub fn error_envelope(e: &Error) -> Value {
    e.to_json()
}

pub fn to_json(v: &Value, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(v).unwrap_or_default()
    } else {
        v.to_string()
    }
}

/// Restrict rows to the requested fields. Unknown fields are an error so a
/// typo does not silently produce empty output.
pub fn select_fields(data: &Value, fields: &[String]) -> Result<Value> {
    fn pick(m: &Map<String, Value>, fields: &[String]) -> Map<String, Value> {
        let mut out = Map::new();
        for f in fields {
            if let Some(v) = m.get(f) {
                out.insert(f.clone(), v.clone());
            }
        }
        out
    }
    let sample: Option<&Map<String, Value>> = match data {
        Value::Array(a) => a.iter().find_map(|x| x.as_object()),
        Value::Object(m) => Some(m),
        _ => None,
    };
    if let Some(s) = sample {
        for f in fields {
            if !s.contains_key(f) {
                return Err(
                    Error::usage("unknown_field", format!("no field '{f}' in the output"))
                        .with_details(json!({ "fields": s.keys().collect::<Vec<_>>() }))
                        .with_hint("omit --fields to see all fields"),
                );
            }
        }
    }
    Ok(match data {
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| match x {
                    Value::Object(m) => Value::Object(pick(m, fields)),
                    other => other.clone(),
                })
                .collect(),
        ),
        Value::Object(m) => Value::Object(pick(m, fields)),
        other => other.clone(),
    })
}

/// Render a success result. JSON-ish formats include the envelope; human
/// formats show just the data.
pub fn render(
    format: Format,
    data: &Value,
    meta: &Map<String, Value>,
    opts: &Opts,
) -> Result<String> {
    let data = match &opts.fields {
        Some(f) if !f.is_empty() => select_fields(data, f)?,
        _ => data.clone(),
    };
    Ok(match format {
        Format::Json => to_json(&envelope(&data, meta), opts.pretty),
        Format::Jsonl => match &data {
            Value::Array(a) => a
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            other => other.to_string(),
        },
        Format::Yaml => {
            let mut s = String::new();
            yaml(&envelope(&data, meta), 0, &mut s);
            s
        }
        Format::Csv => csv_out(&data, opts)?,
        Format::Table => text_out(&data, opts, Style::Plain),
        Format::Md => text_out(&data, opts, Style::Markdown),
        Format::Html => html_out(&data, opts),
    })
}

pub fn render_error(format: Format, e: &Error, pretty: bool) -> String {
    match format {
        Format::Json | Format::Jsonl | Format::Yaml | Format::Csv => {
            to_json(&error_envelope(e), pretty)
        }
        _ => {
            let mut s = format!("error[{}]: {}", e.code, e.message);
            if let Some(f) = &e.field {
                s.push_str(&format!("\n  field: {f}"));
            }
            if let Some(d) = &e.details {
                s.push_str(&format!("\n  details: {d}"));
            }
            if let Some(h) = &e.hint {
                s.push_str(&format!("\n  hint: {h}"));
            }
            s
        }
    }
}

// ---- cells -----------------------------------------------------------------

fn is_minor(key: &str) -> bool {
    key.ends_with("_minor")
}

fn display_key(key: &str) -> String {
    key.strip_suffix("_minor").unwrap_or(key).to_string()
}

/// Names that mark the main record list of a result that carries several.
const CHART_VALUES: &[&str] = &[
    "amount_minor",
    "spent_minor",
    "net_minor",
    "value_base_minor",
    "value_minor",
    "balance_minor",
    "expense_minor",
];
const CHART_LABELS: &[&str] = &[
    "name", "category", "payee", "month", "period", "label", "symbol", "account", "group", "date",
];

/// A horizontal bar chart of the first money column against a label column,
/// for list-shaped results. Returns `None` when the data has no such shape
/// (for example when amounts are masked). Positive values are `█`, negative `▒`.
pub fn bar_chart(data: &Value, opts: &Opts, width: usize) -> Option<String> {
    let list: &Vec<Value> = match data {
        Value::Array(a) => a,
        Value::Object(m) => PREFERRED_LISTS
            .iter()
            .find_map(|k| m.get(*k).and_then(|v| v.as_array()))?,
        _ => return None,
    };
    let rows: Vec<&Map<String, Value>> = list.iter().filter_map(|v| v.as_object()).collect();
    if rows.is_empty() {
        return None;
    }
    let first = rows[0];
    let vkey = CHART_VALUES
        .iter()
        .find(|k| first.get(**k).map(|v| v.is_number()).unwrap_or(false))
        .map(|k| k.to_string())
        .or_else(|| {
            first
                .iter()
                .find(|(k, v)| is_minor(k) && v.is_number())
                .map(|(k, _)| k.clone())
        })?;
    let lkey = CHART_LABELS
        .iter()
        .find(|k| first.get(**k).map(|v| v.is_string()).unwrap_or(false))
        .map(|k| k.to_string())
        .or_else(|| {
            first
                .iter()
                .find(|(_, v)| v.is_string())
                .map(|(k, _)| k.clone())
        })?;
    let max = rows
        .iter()
        .filter_map(|r| r.get(&vkey).and_then(|v| v.as_i64()))
        .map(|v| v.unsigned_abs())
        .max()?;
    if max == 0 {
        return None;
    }
    let labels: Vec<String> = rows
        .iter()
        .map(|r| {
            r.get(&lkey)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .chars()
                .filter(|c| !c.is_control())
                .take(24)
                .collect()
        })
        .collect();
    let values: Vec<String> = rows
        .iter()
        .map(|r| cell(&vkey, r.get(&vkey).unwrap_or(&Value::Null), r, opts, true))
        .collect();
    let lw = labels.iter().map(|l| width_of(l)).max().unwrap_or(0);
    let vw = values.iter().map(|v| width_of(v)).max().unwrap_or(0);
    let mut out = format!("{} by {}\n", display_key(&vkey), display_key(&lkey));
    for (i, r) in rows.iter().enumerate() {
        let v = r.get(&vkey).and_then(|v| v.as_i64()).unwrap_or(0);
        let cells = ((v.unsigned_abs() as u128 * width as u128).div_ceil(max as u128)) as usize;
        let glyph = if v < 0 { "▒" } else { "█" };
        out.push_str(&format!(
            "{}  {}  {}\n",
            pad(&labels[i], lw, false),
            pad(&values[i], vw, true),
            glyph.repeat(cells.min(width))
        ));
    }
    Some(out)
}

fn width_of(s: &str) -> usize {
    width(s)
}

const PREFERRED_LISTS: &[&str] = &[
    "rows",
    "items",
    "holdings",
    "cards",
    "categories",
    "months",
    "budgets",
    "moves",
    "people",
    "classes",
    "instruments",
    "events",
    "transactions",
];

/// Currency for a money column. `*_base_minor` columns are always in the
/// base currency (the row's `base_currency` if it names one, else the
/// default); every other `*_minor` column uses the row's own `currency`.
fn currency_of<'a>(key: &str, row: &'a Map<String, Value>, opts: &'a Opts) -> Option<&'a str> {
    if key.ends_with("_base_minor") {
        return row
            .get("base_currency")
            .and_then(|v| v.as_str())
            .or(opts.default_currency.as_deref());
    }
    row.get("currency")
        .and_then(|v| v.as_str())
        .or(opts.default_currency.as_deref())
}

/// Format one cell. `grouped` adds thousands separators (human formats only).
fn cell(key: &str, v: &Value, row: &Map<String, Value>, opts: &Opts, grouped: bool) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            if is_minor(key) {
                if let Some(i) = n.as_i64() {
                    return match currency_of(key, row, opts) {
                        Some(c) if grouped => format_grouped(i, c),
                        Some(c) => format_amount(i, decimals_for(c)),
                        None => i.to_string(),
                    };
                }
            }
            n.to_string()
        }
        Value::String(s) => s.clone(),
        Value::Array(a) if a.iter().all(|x| !x.is_object() && !x.is_array()) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| x.to_string())
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

fn columns(rows: &[&Map<String, Value>]) -> Vec<String> {
    let mut cols: Vec<String> = vec![];
    for r in rows {
        for k in r.keys() {
            if !cols.contains(k) {
                cols.push(k.clone());
            }
        }
    }
    cols
}

fn right_aligned(key: &str, rows: &[&Map<String, Value>]) -> bool {
    is_minor(key)
        || key.ends_with("_pct")
        || key.ends_with("_count")
        || rows
            .iter()
            .filter_map(|r| r.get(key))
            .all(|v| v.is_number() || v.is_null())
            && rows
                .iter()
                .any(|r| r.get(key).is_some_and(|v| v.is_number()))
}

// ---- text tables -----------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Style {
    Plain,
    Markdown,
}

fn width(s: &str) -> usize {
    s.chars().count()
}

fn pad(s: &str, w: usize, right: bool) -> String {
    let n = w.saturating_sub(width(s));
    if right {
        format!("{}{s}", " ".repeat(n))
    } else {
        format!("{s}{}", " ".repeat(n))
    }
}

fn md_escape(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn table(rows: &[&Map<String, Value>], opts: &Opts, style: Style) -> String {
    let cols = columns(rows);
    if cols.is_empty() {
        return String::new();
    }
    let heads: Vec<String> = cols.iter().map(|c| display_key(c)).collect();
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            cols.iter()
                .map(|c| {
                    let s = r
                        .get(c)
                        .map(|v| cell(c, v, r, opts, true))
                        .unwrap_or_default()
                        .replace('\n', " ");
                    if style == Style::Markdown {
                        md_escape(&s)
                    } else {
                        s
                    }
                })
                .collect()
        })
        .collect();
    let right: Vec<bool> = cols.iter().map(|c| right_aligned(c, rows)).collect();
    let widths: Vec<usize> = (0..cols.len())
        .map(|i| {
            body.iter()
                .map(|r| width(&r[i]))
                .chain(std::iter::once(width(&heads[i])))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    match style {
        Style::Plain => {
            let head: Vec<String> = heads
                .iter()
                .enumerate()
                .map(|(i, h)| pad(&h.to_uppercase(), widths[i], right[i]))
                .collect();
            let line = head.join("  ");
            if opts.color {
                out.push_str(&format!("\x1b[1m{}\x1b[0m\n", line.trim_end()));
            } else {
                out.push_str(line.trim_end());
                out.push('\n');
            }
            for r in &body {
                let l: Vec<String> = r
                    .iter()
                    .enumerate()
                    .map(|(i, c)| pad(c, widths[i], right[i]))
                    .collect();
                out.push_str(l.join("  ").trim_end());
                out.push('\n');
            }
        }
        Style::Markdown => {
            let head: Vec<String> = heads
                .iter()
                .enumerate()
                .map(|(i, h)| pad(&md_escape(h), widths[i], false))
                .collect();
            out.push_str(&format!("| {} |\n", head.join(" | ")));
            let sep: Vec<String> = (0..cols.len())
                .map(|i| {
                    let dashes = "-".repeat(widths[i].max(3));
                    if right[i] {
                        format!("{}:", &dashes[..dashes.len() - 1])
                    } else {
                        dashes
                    }
                })
                .collect();
            out.push_str(&format!("| {} |\n", sep.join(" | ")));
            for r in &body {
                let l: Vec<String> = r
                    .iter()
                    .enumerate()
                    .map(|(i, c)| pad(c, widths[i], right[i]))
                    .collect();
                out.push_str(&format!("| {} |\n", l.join(" | ")));
            }
        }
    }
    out
}

fn kv_rows(m: &Map<String, Value>, opts: &Opts) -> Vec<Map<String, Value>> {
    m.iter()
        .filter(|(_, v)| !is_section(v))
        .map(|(k, v)| {
            let mut r = Map::new();
            r.insert("field".into(), json!(display_key(k)));
            r.insert("value".into(), json!(cell(k, v, m, opts, true)));
            r
        })
        .collect()
}

fn is_section(v: &Value) -> bool {
    match v {
        Value::Array(a) => a.iter().any(|x| x.is_object()),
        Value::Object(_) => true,
        _ => false,
    }
}

fn text_out(data: &Value, opts: &Opts, style: Style) -> String {
    let mut out = String::new();
    text_value(data, opts, style, "", &mut out);
    out.trim_end().to_string()
}

fn heading(title: &str, style: Style, depth: usize) -> String {
    match style {
        Style::Markdown => format!("{} {title}\n\n", "#".repeat((depth + 2).min(6))),
        Style::Plain => format!("{title}\n"),
    }
}

fn text_value(v: &Value, opts: &Opts, style: Style, title: &str, out: &mut String) {
    let depth = if title.is_empty() { 0 } else { 1 };
    match v {
        Value::Array(a) if a.is_empty() => {
            if !title.is_empty() {
                out.push_str(&heading(title, style, depth));
            }
            out.push_str("(none)\n\n");
        }
        Value::Array(a) if a.iter().all(|x| x.is_object()) => {
            if !title.is_empty() {
                out.push_str(&heading(title, style, depth));
            }
            let rows: Vec<&Map<String, Value>> = a.iter().filter_map(|x| x.as_object()).collect();
            out.push_str(&table(&rows, opts, style));
            out.push('\n');
        }
        Value::Array(a) => {
            if !title.is_empty() {
                out.push_str(&heading(title, style, depth));
            }
            for x in a {
                out.push_str(&format!(
                    "{}\n",
                    x.as_str()
                        .map(String::from)
                        .unwrap_or_else(|| x.to_string())
                ));
            }
            out.push('\n');
        }
        Value::Object(m) => {
            if !title.is_empty() {
                out.push_str(&heading(title, style, depth));
            }
            let kv = kv_rows(m, opts);
            if !kv.is_empty() {
                let refs: Vec<&Map<String, Value>> = kv.iter().collect();
                out.push_str(&table(&refs, opts, style));
                out.push('\n');
            }
            for (k, sub) in m.iter().filter(|(_, v)| is_section(v)) {
                // Nested sections inherit the parent's currency.
                let mut o = opts.clone();
                if let Some(c) = m.get("currency").and_then(|c| c.as_str()) {
                    o.default_currency = Some(c.to_string());
                }
                text_value(sub, &o, style, &display_key(k), out);
            }
        }
        Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Null => {}
        other => {
            out.push_str(&other.to_string());
            out.push('\n');
        }
    }
}

// ---- csv -------------------------------------------------------------------

/// An object that carries one main list of records (a report with its `rows`,
/// a budget status with its `budgets`; with several lists, the first of the
/// conventional names above) exports as that list, with
/// the object's scalar fields (period, currency, ...) repeated on every row so
/// each row stands alone. Any other object exports as a single row.
fn csv_rows_of_object(m: &Map<String, Value>) -> Vec<Map<String, Value>> {
    let lists: Vec<(&String, &Vec<Value>)> = m
        .iter()
        .filter_map(|(k, v)| match v {
            Value::Array(a) if !a.is_empty() && a.iter().all(|x| x.is_object()) => Some((k, a)),
            _ => None,
        })
        .collect();
    let chosen = if lists.len() == 1 {
        Some(lists[0])
    } else {
        PREFERRED_LISTS
            .iter()
            .find_map(|name| lists.iter().find(|(k, _)| k.as_str() == *name).copied())
    };
    let Some((list_key, items)) = chosen else {
        return vec![m.clone()];
    };
    let scalars: Map<String, Value> = m
        .iter()
        .filter(|(k, v)| *k != list_key && !v.is_array() && !v.is_object())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    items
        .iter()
        .filter_map(|x| x.as_object())
        .map(|row| {
            let mut out = scalars.clone();
            for (k, v) in row {
                out.insert(k.clone(), v.clone());
            }
            out
        })
        .collect()
}

fn csv_out(data: &Value, opts: &Opts) -> Result<String> {
    let owned: Vec<Map<String, Value>> = match data {
        Value::Array(a) => a.iter().filter_map(|x| x.as_object().cloned()).collect(),
        Value::Object(m) => csv_rows_of_object(m),
        other => {
            return Ok(match other {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                o => o.to_string(),
            })
        }
    };
    let rows: Vec<&Map<String, Value>> = owned.iter().collect();
    let cols = columns(&rows);
    let mut w = csv::Writer::from_writer(vec![]);
    let io = |e: csv::Error| Error::io("csv_write_failed", e.to_string());
    w.write_record(cols.iter().map(|c| display_key(c)))
        .map_err(io)?;
    for r in &rows {
        let rec: Vec<String> = cols
            .iter()
            .map(|c| {
                let s = r
                    .get(c)
                    .map(|v| cell(c, v, r, opts, false))
                    .unwrap_or_default();
                neutralise_formula(&s)
            })
            .collect();
        w.write_record(&rec).map_err(io)?;
    }
    let bytes = w
        .into_inner()
        .map_err(|e| Error::io("csv_write_failed", e.to_string()))?;
    Ok(String::from_utf8_lossy(&bytes).trim_end().to_string())
}

/// Prevent spreadsheet formula injection from payee/memo text.
fn neutralise_formula(s: &str) -> String {
    match s.chars().next() {
        Some('=') | Some('+') | Some('@') | Some('\t') | Some('\r') => format!("'{s}"),
        Some('-') if s.parse::<f64>().is_err() => format!("'{s}"),
        _ => s.to_string(),
    }
}

// ---- yaml ------------------------------------------------------------------

fn yaml_scalar(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => Value::String(s.clone()).to_string(),
        _ => unreachable!(),
    }
}

fn yaml(v: &Value, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match v {
        Value::Object(m) if m.is_empty() => out.push_str(&format!("{pad}{{}}\n")),
        Value::Object(m) => {
            for (k, val) in m {
                let key = Value::String(k.clone()).to_string();
                match val {
                    Value::Object(x) if !x.is_empty() => {
                        out.push_str(&format!("{pad}{key}:\n"));
                        yaml(val, indent + 2, out);
                    }
                    Value::Array(x) if !x.is_empty() => {
                        out.push_str(&format!("{pad}{key}:\n"));
                        yaml(val, indent + 2, out);
                    }
                    Value::Object(_) => out.push_str(&format!("{pad}{key}: {{}}\n")),
                    Value::Array(_) => out.push_str(&format!("{pad}{key}: []\n")),
                    s => out.push_str(&format!("{pad}{key}: {}\n", yaml_scalar(s))),
                }
            }
        }
        Value::Array(a) if a.is_empty() => out.push_str(&format!("{pad}[]\n")),
        Value::Array(a) => {
            for item in a {
                match item {
                    Value::Object(m) if !m.is_empty() => {
                        let mut inner = String::new();
                        yaml(item, indent + 2, &mut inner);
                        // Put the first line on the dash line.
                        let trimmed = inner.trim_start_matches(' ');
                        out.push_str(&format!("{pad}- {trimmed}"));
                    }
                    Value::Array(x) if !x.is_empty() => {
                        out.push_str(&format!("{pad}-\n"));
                        yaml(item, indent + 2, out);
                    }
                    Value::Object(_) => out.push_str(&format!("{pad}- {{}}\n")),
                    Value::Array(_) => out.push_str(&format!("{pad}- []\n")),
                    s => out.push_str(&format!("{pad}- {}\n", yaml_scalar(s))),
                }
            }
        }
        s => out.push_str(&format!("{pad}{}\n", yaml_scalar(s))),
    }
}

// ---- html ------------------------------------------------------------------

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn html_table(rows: &[&Map<String, Value>], opts: &Opts) -> String {
    let cols = columns(rows);
    let mut s = String::from("<table>\n<thead><tr>");
    for c in &cols {
        s.push_str(&format!("<th>{}</th>", esc(&display_key(c))));
    }
    s.push_str("</tr></thead>\n<tbody>\n");
    for r in rows {
        s.push_str("<tr>");
        for c in &cols {
            let t = r
                .get(c)
                .map(|v| cell(c, v, r, opts, true))
                .unwrap_or_default();
            let class = if right_aligned(c, rows) {
                " class=\"num\""
            } else {
                ""
            };
            s.push_str(&format!("<td{class}>{}</td>", esc(&t)));
        }
        s.push_str("</tr>\n");
    }
    s.push_str("</tbody>\n</table>\n");
    s
}

fn html_value(v: &Value, opts: &Opts, title: &str, out: &mut String) {
    if !title.is_empty() {
        out.push_str(&format!("<h2>{}</h2>\n", esc(title)));
    }
    match v {
        Value::Array(a) if a.iter().all(|x| x.is_object()) && !a.is_empty() => {
            let rows: Vec<&Map<String, Value>> = a.iter().filter_map(|x| x.as_object()).collect();
            out.push_str(&html_table(&rows, opts));
        }
        Value::Array(a) if a.is_empty() => out.push_str("<p>(none)</p>\n"),
        Value::Object(m) => {
            let kv = kv_rows(m, opts);
            if !kv.is_empty() {
                let refs: Vec<&Map<String, Value>> = kv.iter().collect();
                out.push_str(&html_table(&refs, opts));
            }
            for (k, sub) in m.iter().filter(|(_, v)| is_section(v)) {
                html_value(sub, opts, &display_key(k), out);
            }
        }
        other => out.push_str(&format!(
            "<p>{}</p>\n",
            esc(&cell("", other, &Map::new(), opts, true))
        )),
    }
}

fn html_out(data: &Value, opts: &Opts) -> String {
    let mut body = String::new();
    html_value(data, opts, "", &mut body);
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><title>pext</title>\n<style>\n\
body{{font:14px/1.5 system-ui,sans-serif;margin:2rem;color:#1a1a1a}}\n\
table{{border-collapse:collapse}}th,td{{padding:.3rem .8rem;border-bottom:1px solid #ddd;text-align:left}}\n\
th{{background:#f4f4f4}}td.num{{text-align:right;font-variant-numeric:tabular-nums}}\n\
@media(prefers-color-scheme:dark){{body{{background:#161616;color:#eee}}th{{background:#262626}}td,th{{border-color:#333}}}}\n\
</style></head><body>\n{body}</body></html>\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Value {
        json!([
            {"id": 1, "date": "2026-01-02", "amount_minor": -123456, "currency": "INR", "payee": "Swiggy"},
            {"id": 2, "date": "2026-01-03", "amount_minor": 5000, "currency": "INR", "payee": "=cmd|' /C calc'!A0"}
        ])
    }

    #[test]
    fn table_formats_minor_units_with_indian_grouping() {
        let s = render(Format::Table, &rows(), &Map::new(), &Opts::default()).unwrap();
        assert!(s.contains("AMOUNT"), "{s}");
        assert!(s.contains("-1,234.56"), "{s}");
        assert!(!s.contains("123456"), "{s}");
    }

    #[test]
    fn json_keeps_raw_minor_and_wraps_in_envelope() {
        let s = render(Format::Json, &rows(), &Map::new(), &Opts::default()).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["schema"], "pext/v1");
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"][0]["amount_minor"], -123456);
    }

    #[test]
    fn base_columns_use_the_base_currency_and_native_columns_the_row_currency() {
        let data = json!([{ "name": "Tokyo flat", "currency": "JPY", "value_minor": 5000000, "value_base_minor": 2850000 }]);
        let opts = Opts {
            default_currency: Some("INR".into()),
            ..Opts::default()
        };
        let csv = render(Format::Csv, &data, &Map::new(), &opts).unwrap();
        assert!(csv.contains("5000000"), "JPY has no decimals: {csv}");
        assert!(csv.contains("28500.00"), "INR has two: {csv}");
        let table = render(Format::Table, &data, &Map::new(), &opts).unwrap();
        assert!(
            table.contains("5,000,000") && table.contains("28,500.00"),
            "{table}"
        );
    }

    #[test]
    fn csv_prefers_conventional_list_names_when_there_are_several() {
        let data = json!({"currency": "INR", "groups": [{"group": "a"}], "items": [{"name": "x"}, {"name": "y"}]});
        let out = render(Format::Csv, &data, &Map::new(), &Opts::default()).unwrap();
        assert_eq!(out.lines().count(), 3, "{out}");
        assert!(out.lines().next().unwrap().ends_with("name"), "{out}");
    }

    #[test]
    fn csv_flattens_report_shaped_objects() {
        let data = json!({
            "period_start": "2026-11-01", "currency": "INR", "total_minor": 230000,
            "rows": [
                {"name": "food", "amount_minor": 180000},
                {"name": "=cmd()", "amount_minor": 50000}
            ]
        });
        let out = render(Format::Csv, &data, &Map::new(), &Opts::default()).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert!(lines[0].starts_with("period_start,currency,total"), "{out}");
        assert!(
            lines[1].contains("2026-11-01,INR,2300.00,food,1800.00"),
            "{out}"
        );
        assert!(lines[2].contains("'=cmd()"), "formula neutralised: {out}");
        // two lists: no obvious rows, so the object stays one row
        let two = json!({"a": [{"x": 1}], "b": [{"y": 2}], "n": 3});
        assert_eq!(
            render(Format::Csv, &two, &Map::new(), &Opts::default())
                .unwrap()
                .lines()
                .count(),
            2
        );
        // an empty list falls back to the single-row form
        let none = json!({"n": 3, "rows": []});
        assert_eq!(
            render(Format::Csv, &none, &Map::new(), &Opts::default())
                .unwrap()
                .lines()
                .count(),
            2
        );
    }

    #[test]
    fn csv_has_plain_decimals_and_neutralises_formulas() {
        let s = render(Format::Csv, &rows(), &Map::new(), &Opts::default()).unwrap();
        assert!(s.starts_with("id,date,amount,currency,payee"), "{s}");
        assert!(s.contains("-1234.56"), "{s}");
        assert!(s.contains("\"'=cmd") || s.contains("'=cmd"), "{s}");
    }

    #[test]
    fn jsonl_is_one_object_per_line() {
        let s = render(Format::Jsonl, &rows(), &Map::new(), &Opts::default()).unwrap();
        assert_eq!(s.lines().count(), 2);
        for l in s.lines() {
            serde_json::from_str::<Value>(l).unwrap();
        }
    }

    #[test]
    fn fields_filter_and_unknown_field_errors() {
        let o = Opts {
            fields: Some(vec!["id".into(), "payee".into()]),
            ..Default::default()
        };
        let s = render(Format::Jsonl, &rows(), &Map::new(), &o).unwrap();
        assert_eq!(s.lines().next().unwrap(), r#"{"id":1,"payee":"Swiggy"}"#);
        let bad = Opts {
            fields: Some(vec!["nope".into()]),
            ..Default::default()
        };
        assert_eq!(
            render(Format::Json, &rows(), &Map::new(), &bad)
                .unwrap_err()
                .code,
            "unknown_field"
        );
    }

    #[test]
    fn markdown_table_and_escape() {
        let d = json!([{"name": "a|b", "n": 3}]);
        let s = render(Format::Md, &d, &Map::new(), &Opts::default()).unwrap();
        assert!(s.contains("a\\|b"), "{s}");
        assert!(s.lines().nth(1).unwrap().contains("---"), "{s}");
    }

    #[test]
    fn html_escapes_content() {
        let d = json!([{"payee": "<script>alert(1)</script>"}]);
        let s = render(Format::Html, &d, &Map::new(), &Opts::default()).unwrap();
        assert!(!s.contains("<script>"), "{s}");
        assert!(s.contains("&lt;script&gt;"));
    }

    #[test]
    fn yaml_roundtrip_shape() {
        let d = json!({"a": [1, {"b": "x"}], "c": {}, "d": null});
        let mut s = String::new();
        yaml(&d, 0, &mut s);
        assert!(s.contains("\"a\":\n  - 1\n  - \"b\": \"x\""), "{s}");
        assert!(s.contains("\"c\": {}"));
        assert!(s.contains("\"d\": null"));
    }

    #[test]
    fn object_renders_key_value_and_sections() {
        let d = json!({"name": "HDFC", "currency": "INR", "balance_minor": 100000, "items": [{"x": 1}]});
        let s = render(Format::Table, &d, &Map::new(), &Opts::default()).unwrap();
        assert!(s.contains("balance"), "{s}");
        assert!(s.contains("1,000.00"), "{s}");
        assert!(s.contains("items"), "{s}");
    }

    #[test]
    fn format_parse_rejects_unknown() {
        assert!(Format::parse("xml").is_err());
        assert_eq!(Format::parse("NDJSON").unwrap(), Format::Jsonl);
    }
}
