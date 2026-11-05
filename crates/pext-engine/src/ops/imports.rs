//! Statement import: CSV and OFX with de-duplication, preview (via --dry-run),
//! saved column profiles and per-batch rollback.

use crate::args::Args;
use crate::audit::sha256_hex;
use crate::ctx::{iv, sv, Ctx, DbResult};
use crate::ops::accounts::{resolve_account, AccountRow};
use crate::ops::categories::{category_id, collapse_ws, normalize_payee};
use crate::ops::rules::{auto, AutoInput};
use crate::ops::txns::{create_txn, fmt_date, NewTxn, SplitIn};
use crate::registry::{OpSpec, Param};
use chrono::NaiveDate;
use pext_core::dates::{parse_date, parse_date_with};
use pext_core::money::{parse_amount, parse_decimal};
use pext_core::{Error, Result};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Parsed {
    pub row: usize,
    pub date: NaiveDate,
    pub amount_minor: i64,
    pub payee: String,
    pub memo: String,
    pub category: Option<String>,
    pub ext_ref: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RowError {
    pub row: usize,
    pub message: String,
}

fn read_text(path: &str) -> Result<String> {
    let meta = std::fs::metadata(path)
        .map_err(|e| Error::not_found("file_not_found", format!("cannot read '{path}': {e}")))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(Error::validation(
            "file_too_large",
            format!("'{path}' is larger than 50 MiB"),
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|e| Error::io("read_failed", format!("cannot read '{path}': {e}")))?;
    let text = String::from_utf8(bytes).map_err(|_| {
        Error::validation("invalid_encoding", "file is not valid UTF-8")
            .with_hint("convert the file to UTF-8 first")
    })?;
    Ok(text.trim_start_matches('\u{feff}').to_string())
}

const SYMBOLS: &[&str] = &[
    "₹", "$", "€", "£", "¥", "rs.", "rs", "inr", "usd", "eur", "gbp",
];

/// Turn a bank-style amount cell into a strict decimal string.
pub fn clean_amount(raw: &str, decimal_comma: bool) -> Result<String> {
    let bad = || Error::validation("invalid_amount", format!("cannot read amount '{raw}'"));
    let mut s = raw.replace('\u{a0}', " ").trim().to_string();
    if s.is_empty() {
        return Err(bad());
    }
    let mut neg = false;
    if s.len() >= 2 && s.is_char_boundary(s.len() - 2) {
        match s[s.len() - 2..].to_ascii_lowercase().as_str() {
            "dr" => {
                neg = true;
                s.truncate(s.len() - 2);
            }
            "cr" => s.truncate(s.len() - 2),
            _ => {}
        }
    }
    s = s.trim().to_string();
    if s.starts_with('(') && s.ends_with(')') && s.len() >= 3 {
        if neg {
            return Err(bad());
        }
        neg = true;
        s = s[1..s.len() - 1].trim().to_string();
    }
    loop {
        let before = s.clone();
        for sym in SYMBOLS {
            let lower = s.to_lowercase();
            if lower.starts_with(sym) && s.is_char_boundary(sym.len()) {
                s = s[sym.len()..].trim().to_string();
            } else if lower.ends_with(sym) && s.is_char_boundary(s.len() - sym.len()) {
                s = s[..s.len() - sym.len()].trim().to_string();
            }
        }
        if s == before {
            break;
        }
    }
    s = s.replace(' ', "");
    s = if decimal_comma {
        s.replace('.', "").replace(',', ".")
    } else {
        s.replace(',', "")
    };
    let (sign_neg, body) = match s.strip_prefix('-') {
        Some(r) => (true, r.to_string()),
        None => (false, s.strip_prefix('+').unwrap_or(&s).to_string()),
    };
    if neg && sign_neg {
        return Err(bad());
    }
    let out = format!("{}{}", if neg || sign_neg { "-" } else { "" }, body);
    parse_decimal(&out).map_err(|_| bad())?;
    Ok(out)
}

fn fingerprint(account_id: i64, date: NaiveDate, amount: i64, payee: &str, nth: usize) -> String {
    let h = sha256_hex(&format!(
        "v1|{account_id}|{}|{amount}|{}|{nth}",
        fmt_date(date),
        payee.to_lowercase()
    ));
    h[..32].to_string()
}

fn resolve_col(spec: &str, headers: &[String]) -> Result<usize> {
    let lower = spec.trim().to_lowercase();
    if let Some(i) = headers
        .iter()
        .position(|h| h.trim().to_lowercase() == lower)
    {
        return Ok(i);
    }
    if let Ok(i) = lower.parse::<usize>() {
        if i < headers.len() {
            return Ok(i);
        }
    }
    Err(
        Error::usage("column_not_found", format!("column '{spec}' not found"))
            .with_details(json!({ "columns": headers }))
            .with_hint("use a header name or a 0-based column index"),
    )
}

const PROFILE_KEYS: &[&str] = &[
    "date_col",
    "amount_col",
    "debit_col",
    "credit_col",
    "payee_col",
    "memo_col",
    "category_col",
    "ref_col",
    "date_format",
    "delimiter",
    "skip_rows",
    "no_header",
    "decimal_comma",
    "invert",
];

fn merged_csv_args(ctx: &Ctx, a: &Args) -> Result<Args> {
    let mut map = a.map.clone();
    if let Some(name) = a.opt_str("profile") {
        let cfg = ctx
            .opt_string(
                "SELECT config FROM import_profiles WHERE name=?1",
                &[sv(&name)],
            )?
            .ok_or_else(|| {
                Error::not_found("profile_not_found", format!("no import profile '{name}'"))
                    .with_hint("run `pext import profiles`")
            })?;
        let saved: Map<String, Value> = serde_json::from_str(&cfg)
            .map_err(|_| Error::internal("bad_profile", "stored profile is corrupt"))?;
        for (k, v) in saved {
            map.entry(k).or_insert(v);
        }
    }
    Ok(Args::new(map))
}

fn parse_csv(
    a: &Args,
    text: &str,
    acct: &AccountRow,
) -> Result<(Vec<Parsed>, Vec<RowError>, usize)> {
    let skip = a.opt_i64("skip_rows")?.unwrap_or(0).max(0) as usize;
    let body: String = text.lines().skip(skip).collect::<Vec<_>>().join("\n");
    let delim = match a.opt_str("delimiter").as_deref() {
        None | Some(",") => b',',
        Some("tab") | Some("\\t") => b'\t',
        Some(d) if d.len() == 1 && d.is_ascii() => d.as_bytes()[0],
        Some(d) => {
            return Err(Error::usage(
                "invalid_delimiter",
                format!("invalid delimiter '{d}'"),
            ))
        }
    };
    let has_header = !a.flag("no_header");
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delim)
        .has_headers(has_header)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(body.as_bytes());
    let headers: Vec<String> = if has_header {
        rdr.headers()
            .map_err(|e| Error::validation("invalid_csv", format!("cannot read CSV header: {e}")))?
            .iter()
            .map(String::from)
            .collect()
    } else {
        let n = a.opt_i64("columns")?.unwrap_or(0).max(0) as usize;
        (0..n.max(12)).map(|i| i.to_string()).collect()
    };
    let date_col = resolve_col(&a.str("date_col")?, &headers)?;
    let amount_col = a
        .opt_str("amount_col")
        .map(|c| resolve_col(&c, &headers))
        .transpose()?;
    let debit_col = a
        .opt_str("debit_col")
        .map(|c| resolve_col(&c, &headers))
        .transpose()?;
    let credit_col = a
        .opt_str("credit_col")
        .map(|c| resolve_col(&c, &headers))
        .transpose()?;
    if amount_col.is_none() && debit_col.is_none() && credit_col.is_none() {
        return Err(Error::usage(
            "missing_argument",
            "pass --amount-col, or --debit-col/--credit-col",
        )
        .with_field("amount_col"));
    }
    if amount_col.is_some() && (debit_col.is_some() || credit_col.is_some()) {
        return Err(Error::usage(
            "conflicting_arguments",
            "use either --amount-col or --debit-col/--credit-col",
        ));
    }
    let payee_col = a
        .opt_str("payee_col")
        .map(|c| resolve_col(&c, &headers))
        .transpose()?;
    let memo_col = a
        .opt_str("memo_col")
        .map(|c| resolve_col(&c, &headers))
        .transpose()?;
    let cat_col = a
        .opt_str("category_col")
        .map(|c| resolve_col(&c, &headers))
        .transpose()?;
    let ref_col = a
        .opt_str("ref_col")
        .map(|c| resolve_col(&c, &headers))
        .transpose()?;
    let fmt = a
        .opt_str("date_format")
        .unwrap_or_else(|| "%Y-%m-%d".into());
    let comma = a.flag("decimal_comma");
    let invert = a.flag("invert");
    let dec = acct.decimals();

    let mut rows = Vec::new();
    let mut errors = Vec::new();
    let mut total = 0;
    for (i, rec) in rdr.records().enumerate() {
        let line = i + 1 + skip + has_header as usize;
        let rec = match rec {
            Ok(r) => r,
            Err(e) => {
                errors.push(RowError {
                    row: line,
                    message: format!("unreadable row: {e}"),
                });
                continue;
            }
        };
        if rec.iter().all(|c| c.is_empty()) {
            continue;
        }
        total += 1;
        let get = |c: usize| rec.get(c).unwrap_or("");
        let parsed: Result<Parsed> = (|| {
            let date = parse_date_with(get(date_col), &fmt)?;
            let amount_minor = if let Some(ac) = amount_col {
                parse_amount(&clean_amount(get(ac), comma)?, dec)?
            } else {
                let side = |c: Option<usize>| -> Result<i64> {
                    match c {
                        Some(c) if !get(c).is_empty() => {
                            parse_amount(clean_amount(get(c), comma)?.trim_start_matches('-'), dec)
                        }
                        _ => Ok(0),
                    }
                };
                let (d, c) = (side(debit_col)?, side(credit_col)?);
                if d != 0 && c != 0 {
                    return Err(Error::validation(
                        "invalid_amount",
                        "both debit and credit are filled in",
                    ));
                }
                c - d
            };
            let amount_minor = if invert { -amount_minor } else { amount_minor };
            Ok(Parsed {
                row: line,
                date,
                amount_minor,
                payee: payee_col.map(|c| collapse_ws(get(c))).unwrap_or_default(),
                memo: memo_col.map(|c| get(c).to_string()).unwrap_or_default(),
                category: cat_col
                    .map(|c| get(c).to_ascii_lowercase())
                    .filter(|s| !s.is_empty()),
                ext_ref: ref_col
                    .map(|c| get(c).to_string())
                    .filter(|s| !s.is_empty()),
            })
        })();
        match parsed {
            Ok(p) => rows.push(p),
            Err(e) => errors.push(RowError {
                row: line,
                message: e.message,
            }),
        }
    }
    Ok((rows, errors, total))
}

fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn ofx_tag(block: &str, tag: &str) -> Option<String> {
    let lower = block.to_ascii_lowercase();
    let open = format!("<{}>", tag.to_ascii_lowercase());
    let start = lower.find(&open)? + open.len();
    let rest = &block[start..];
    let end = rest.find(['<', '\n', '\r']).unwrap_or(rest.len());
    let v = decode_entities(rest[..end].trim());
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

pub fn parse_ofx(text: &str, acct: &AccountRow) -> Result<(Vec<Parsed>, Vec<RowError>, usize)> {
    if let Some(cur) = ofx_tag(text, "CURDEF") {
        if !cur.eq_ignore_ascii_case(&acct.currency) {
            return Err(Error::validation(
                "currency_mismatch",
                format!(
                    "file currency {cur} differs from account currency {}",
                    acct.currency
                ),
            ));
        }
    }
    let lower = text.to_ascii_lowercase();
    let mut starts = Vec::new();
    let mut pos = 0;
    while let Some(i) = lower[pos..].find("<stmttrn>") {
        starts.push(pos + i);
        pos += i + 9;
    }
    if starts.is_empty() {
        return Err(Error::validation(
            "invalid_ofx",
            "no <STMTTRN> transactions found in the file",
        ));
    }
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for (n, &s) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(text.len());
        let mut block = &text[s..end];
        if let Some(e) = block.to_ascii_lowercase().find("</stmttrn>") {
            block = &block[..e];
        }
        let row = n + 1;
        let r: Result<Parsed> = (|| {
            let dt = ofx_tag(block, "DTPOSTED")
                .ok_or_else(|| Error::validation("invalid_ofx", "missing DTPOSTED"))?;
            let date = parse_date_with(dt.get(..8).unwrap_or(&dt), "%Y%m%d")?;
            let amt = ofx_tag(block, "TRNAMT")
                .ok_or_else(|| Error::validation("invalid_ofx", "missing TRNAMT"))?;
            let amount_minor = parse_amount(&amt, acct.decimals())?;
            let name = ofx_tag(block, "NAME");
            let memo = ofx_tag(block, "MEMO");
            Ok(Parsed {
                row,
                date,
                amount_minor,
                payee: collapse_ws(&name.clone().or_else(|| memo.clone()).unwrap_or_default()),
                memo: memo.unwrap_or_default(),
                category: None,
                ext_ref: ofx_tag(block, "FITID"),
            })
        })();
        match r {
            Ok(p) => rows.push(p),
            Err(e) => errors.push(RowError {
                row,
                message: e.message,
            }),
        }
    }
    Ok((rows, errors, starts.len()))
}

#[allow(clippy::too_many_arguments)]
fn finish_import(
    ctx: &Ctx,
    acct: AccountRow,
    source: &str,
    kind: &str,
    rows: Vec<Parsed>,
    mut errors: Vec<RowError>,
    total_rows: usize,
    on_error_skip: bool,
) -> Result<Value> {
    // Resolve categories up front so a typo aborts before anything is written.
    let mut cat_ids: HashMap<String, i64> = HashMap::new();
    for r in &rows {
        if let Some(c) = &r.category {
            if !cat_ids.contains_key(c) {
                match category_id(ctx, c) {
                    Ok(id) => {
                        cat_ids.insert(c.clone(), id);
                    }
                    Err(e) => errors.push(RowError {
                        row: r.row,
                        message: e.message,
                    }),
                }
            }
        }
    }
    if !errors.is_empty() && !on_error_skip {
        let total = errors.len();
        let shown: Vec<Value> = errors
            .iter()
            .take(100)
            .map(|e| json!({ "row": e.row, "error": e.message }))
            .collect();
        return Err(Error::validation(
            "import_errors",
            format!("{total} row(s) could not be read; nothing was imported"),
        )
        .with_details(json!({ "errors": shown, "total_errors": total }))
        .with_hint("fix the file or mapping, or pass --on-error skip to import the valid rows"));
    }
    let bad_rows: HashSet<usize> = errors.iter().map(|e| e.row).collect();
    let mut nth: HashMap<(NaiveDate, i64, String), usize> = HashMap::new();
    let mut seen_refs: HashSet<String> = HashSet::new();
    let (mut dups, mut zero) = (0usize, 0usize);
    struct Todo {
        p: Parsed,
        payee: String,
        ext_ref: Option<String>,
        fp: Option<String>,
    }
    let mut todo: Vec<Todo> = Vec::new();
    for p in rows.into_iter().filter(|p| !bad_rows.contains(&p.row)) {
        if p.amount_minor == 0 {
            zero += 1;
            continue;
        }
        let payee = normalize_payee(ctx, &p.payee)?;
        let (ext_ref, fp) = match &p.ext_ref {
            Some(r) => (Some(r.clone()), None),
            None => {
                let n = nth
                    .entry((p.date, p.amount_minor, payee.to_lowercase()))
                    .or_insert(0);
                *n += 1;
                (
                    None,
                    Some(fingerprint(acct.id, p.date, p.amount_minor, &payee, *n)),
                )
            }
        };
        let exists = match (&ext_ref, &fp) {
            (Some(r), _) => {
                !seen_refs.insert(r.clone())
                    || ctx.opt_i64(
                        "SELECT id FROM transactions WHERE account_id=?1 AND external_ref=?2 AND deleted_at IS NULL",
                        &[iv(acct.id), sv(r)],
                    )?
                    .is_some()
            }
            (None, Some(f)) => ctx
                .opt_i64(
                    "SELECT id FROM transactions WHERE account_id=?1 AND fingerprint=?2 AND deleted_at IS NULL",
                    &[iv(acct.id), sv(f)],
                )?
                .is_some(),
            _ => false,
        };
        if exists {
            dups += 1;
            continue;
        }
        todo.push(Todo {
            p,
            payee,
            ext_ref,
            fp,
        });
    }
    let batch = ctx.insert(
        "import_batches",
        &[
            ("source", sv(source)),
            ("kind", sv(kind)),
            ("account_id", iv(acct.id)),
            ("created_at", sv(&ctx.now)),
            ("row_count", iv(todo.len() as i64)),
        ],
    )?;
    let (mut money_in, mut money_out) = (0i128, 0i128);
    let (mut first, mut last): (Option<NaiveDate>, Option<NaiveDate>) = (None, None);
    let imported = todo.len();
    for t in todo {
        let mut category_id_v = t.p.category.as_ref().and_then(|c| cat_ids.get(c).copied());
        let mut payee = t.payee;
        let mut tags = vec![];
        if category_id_v.is_none() {
            let r = auto(
                ctx,
                &AutoInput {
                    account_id: acct.id,
                    payee: &payee,
                    memo: &t.p.memo,
                    amount_minor: t.p.amount_minor,
                },
            )?;
            category_id_v = r.category_id;
            if let Some(np) = r.payee {
                payee = np;
            }
            tags = r.tags;
        }
        if t.p.amount_minor > 0 {
            money_in += t.p.amount_minor as i128;
        } else {
            money_out += -(t.p.amount_minor as i128);
        }
        first = Some(first.map_or(t.p.date, |f: NaiveDate| f.min(t.p.date)));
        last = Some(last.map_or(t.p.date, |l: NaiveDate| l.max(t.p.date)));
        create_txn(
            ctx,
            NewTxn {
                account: acct.clone(),
                date: t.p.date,
                amount_minor: t.p.amount_minor,
                payee,
                memo: t.p.memo.clone(),
                status: "cleared".into(),
                kind: "standard",
                transfer_id: None,
                external_ref: t.ext_ref,
                fingerprint: t.fp,
                import_batch: Some(batch),
                splits: vec![SplitIn {
                    category_id: category_id_v,
                    amount_minor: t.p.amount_minor,
                    note: String::new(),
                }],
                tags,
            },
        )?;
    }
    Ok(json!({
        "batch_id": batch, "source": source, "kind": kind, "account": acct.name, "currency": acct.currency,
        "rows_in_file": total_rows, "imported": imported, "duplicates_skipped": dups, "zero_amount_skipped": zero,
        "unreadable_rows": errors.iter().take(100).map(|e| json!({"row": e.row, "error": e.message})).collect::<Vec<_>>(),
        "money_in_minor": money_in as i64, "money_out_minor": money_out as i64,
        "first_date": first.map(fmt_date), "last_date": last.map(fmt_date),
    }))
}

fn import_csv(ctx: &Ctx, a: &Args) -> Result<Value> {
    let a = &merged_csv_args(ctx, a)?;
    let acct = resolve_account(ctx, &a.str("account")?)?;
    let file = a.str("file")?;
    let text = read_text(&file)?;
    if let Some(name) = a.opt_str("save_profile") {
        let mut cfg = Map::new();
        for k in PROFILE_KEYS {
            if let Some(v) = a.map.get(*k) {
                cfg.insert((*k).into(), v.clone());
            }
        }
        let body = Value::Object(cfg).to_string();
        match ctx.opt_i64("SELECT id FROM import_profiles WHERE name=?1", &[sv(&name)])? {
            Some(id) => ctx.update("import_profiles", id, &[("config", sv(&body))])?,
            None => {
                ctx.insert(
                    "import_profiles",
                    &[("name", sv(&name)), ("config", sv(&body))],
                )?;
            }
        }
    }
    let (rows, errors, total) = parse_csv(a, &text, &acct)?;
    finish_import(
        ctx,
        acct,
        &file,
        "csv",
        rows,
        errors,
        total,
        a.opt_str("on_error").as_deref() == Some("skip"),
    )
}

fn import_ofx(ctx: &Ctx, a: &Args) -> Result<Value> {
    let acct = resolve_account(ctx, &a.str("account")?)?;
    let file = a.str("file")?;
    let text = read_text(&file)?;
    let (rows, errors, total) = parse_ofx(&text, &acct)?;
    finish_import(
        ctx,
        acct,
        &file,
        "ofx",
        rows,
        errors,
        total,
        a.opt_str("on_error").as_deref() == Some("skip"),
    )
}

fn rollback(ctx: &Ctx, a: &Args) -> Result<Value> {
    if !a.flag("yes") {
        return Err(Error::usage(
            "confirmation_required",
            "rolling back an import needs --yes",
        )
        .with_hint("preview with --dry-run, then repeat with --yes"));
    }
    let id = a.i64("batch")?;
    let rows = ctx.rows(
        "SELECT rolled_back FROM import_batches WHERE id=?1",
        &[iv(id)],
    )?;
    let b = rows
        .first()
        .ok_or_else(|| Error::not_found("batch_not_found", format!("no import batch #{id}")))?;
    if b["rolled_back"].as_i64() == Some(1) {
        return Err(Error::conflict(
            "already_rolled_back",
            format!("batch #{id} was already rolled back"),
        ));
    }
    let ids: Vec<i64> = {
        let mut st = ctx
            .conn
            .prepare("SELECT id FROM transactions WHERE import_batch=?1 AND deleted_at IS NULL")
            .db()?;
        let r = st.query_map([id], |r| r.get(0)).db()?;
        r.collect::<rusqlite::Result<_>>().db()?
    };
    for t in &ids {
        ctx.update("transactions", *t, &[("deleted_at", sv(&ctx.now))])?;
    }
    ctx.update("import_batches", id, &[("rolled_back", iv(1))])?;
    Ok(json!({ "batch_id": id, "transactions_removed": ids.len() }))
}

fn batches(ctx: &Ctx, _a: &Args) -> Result<Value> {
    Ok(Value::Array(
        ctx.rows(
            "SELECT b.id, b.created_at, b.kind, b.source, a.name AS account, b.row_count AS imported, b.rolled_back,
                    (SELECT COUNT(*) FROM transactions t WHERE t.import_batch=b.id AND t.deleted_at IS NULL) AS active
             FROM import_batches b JOIN accounts a ON a.id=b.account_id ORDER BY b.id DESC",
            &[],
        )?
        .into_iter()
        .map(Value::Object)
        .collect(),
    ))
}

fn profiles(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let rows = ctx.rows(
        "SELECT name, config FROM import_profiles ORDER BY name",
        &[],
    )?;
    Ok(Value::Array(
        rows.into_iter()
            .map(|r| json!({ "name": r["name"], "config": serde_json::from_str::<Value>(r["config"].as_str().unwrap_or("{}")).unwrap_or(Value::Null) }))
            .collect(),
    ))
}

fn csv_params() -> Vec<Param> {
    vec![
        Param::s("account", "target account").req(),
        Param::s("file", "path to the CSV file").req().pos(),
        Param::s("profile", "saved column mapping to start from"),
        Param::s("save_profile", "save this mapping under a name"),
        Param::s("date_col", "date column (header name or 0-based index)"),
        Param::s("amount_col", "signed amount column"),
        Param::s("debit_col", "debit (money out) column"),
        Param::s("credit_col", "credit (money in) column"),
        Param::s("payee_col", "payee/description column"),
        Param::s("memo_col", "memo column"),
        Param::s("category_col", "category path column"),
        Param::s(
            "ref_col",
            "unique reference column (used for de-duplication)",
        ),
        Param::s(
            "date_format",
            "chrono date format, e.g. %d/%m/%Y (default %Y-%m-%d)",
        ),
        Param::s("delimiter", "field delimiter (',' by default; 'tab')"),
        Param::i("skip_rows", "lines to skip before the header"),
        Param::b("no_header", "the file has no header row"),
        Param::b("decimal_comma", "amounts use 1.234,56 style"),
        Param::b(
            "invert",
            "flip the sign (credit-card statements list purchases as positive)",
        ),
        Param::s("on_error", "abort (default) or skip unreadable rows").choices(&["abort", "skip"]),
    ]
}

pub fn ops() -> Vec<OpSpec> {
    // `date_col` is required unless supplied by a profile; validated in the handler.
    let mut csv_op = OpSpec::new("import.csv", "Import a CSV bank statement (use --dry-run to preview)", import_csv)
        .write()
        .ps(csv_params())
        .ex("pext import csv statement.csv --account hdfc --date-col Date --amount-col Amount --payee-col Description --date-format %d/%m/%Y --dry-run");
    for p in csv_op.params.iter_mut() {
        if p.name == "date_col" {
            p.required = false;
        }
    }
    vec![
        csv_op,
        OpSpec::new("import.ofx", "Import an OFX/QFX statement", import_ofx)
            .write()
            .p(Param::s("account", "target account").req())
            .p(Param::s("file", "path to the OFX file").req().pos())
            .p(
                Param::s("on_error", "abort (default) or skip unreadable rows")
                    .choices(&["abort", "skip"]),
            ),
        OpSpec::new(
            "import.rollback",
            "Remove every transaction from an import batch",
            rollback,
        )
        .write()
        .p(Param::i("batch", "batch id").req().pos()),
        OpSpec::new("import.batches", "List import batches", batches),
        OpSpec::new(
            "import.profiles",
            "List saved CSV column mappings",
            profiles,
        ),
    ]
}

#[allow(dead_code)]
fn _unused(_: &str) -> Result<NaiveDate> {
    parse_date("2000-01-01")
}
