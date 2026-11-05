//! Categories, payee aliases and tags.

use crate::args::Args;
use crate::ctx::{iv, osv, sv, Ctx, DbResult};
use crate::registry::{OpSpec, Param};
use pext_core::{Error, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// (path, kind, group) in parent-before-child order.
pub const DEFAULT_CATEGORIES: &[(&str, &str, &str)] = &[
    ("food", "expense", "needs"),
    ("food.groceries", "expense", "needs"),
    ("food.dining", "expense", "wants"),
    ("food.delivery", "expense", "wants"),
    ("transport", "expense", "needs"),
    ("transport.fuel", "expense", "needs"),
    ("transport.public", "expense", "needs"),
    ("transport.taxi", "expense", "wants"),
    ("housing", "expense", "needs"),
    ("housing.rent", "expense", "needs"),
    ("housing.utilities", "expense", "needs"),
    ("housing.maintenance", "expense", "needs"),
    ("health", "expense", "needs"),
    ("health.medical", "expense", "needs"),
    ("health.pharmacy", "expense", "needs"),
    ("health.fitness", "expense", "wants"),
    ("shopping", "expense", "wants"),
    ("shopping.clothing", "expense", "wants"),
    ("shopping.electronics", "expense", "wants"),
    ("shopping.household", "expense", "needs"),
    ("entertainment", "expense", "wants"),
    ("entertainment.streaming", "expense", "wants"),
    ("entertainment.events", "expense", "wants"),
    ("education", "expense", "needs"),
    ("travel", "expense", "wants"),
    ("bills", "expense", "needs"),
    ("bills.phone", "expense", "needs"),
    ("bills.internet", "expense", "needs"),
    ("bills.insurance", "expense", "needs"),
    ("fees", "expense", "needs"),
    ("fees.bank", "expense", "needs"),
    ("gifts", "expense", "wants"),
    ("taxes", "expense", "needs"),
    ("other", "expense", "wants"),
    ("salary", "income", "income"),
    ("business", "income", "income"),
    ("interest", "income", "income"),
    ("dividends", "income", "income"),
    ("rental", "income", "income"),
    ("gifts_received", "income", "income"),
    ("other_income", "income", "income"),
];

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

pub fn normalize_path(raw: &str) -> Result<String> {
    let p = raw.trim().to_ascii_lowercase();
    let segs: Vec<&str> = p.split('.').collect();
    if segs.is_empty() || segs.len() > 4 || !segs.iter().all(|s| valid_segment(s)) || p.len() > 64 {
        return Err(Error::validation(
            "invalid_category",
            format!("'{raw}' is not a valid category path"),
        )
        .with_field("category")
        .with_hint("use lowercase letters, digits, _ and - in up to 4 dot-separated levels, e.g. food.delivery"));
    }
    Ok(p)
}

pub fn category_id(ctx: &Ctx, path: &str) -> Result<i64> {
    let p = normalize_path(path)?;
    ctx.opt_i64("SELECT id FROM categories WHERE path=?1", &[sv(&p)])?
        .ok_or_else(|| {
            Error::not_found("category_not_found", format!("no category '{p}'"))
                .with_field("category")
                .with_hint("run `pext cat list`, or create it with `pext cat add`")
        })
}

pub fn category_path(ctx: &Ctx, id: i64) -> Result<String> {
    ctx.opt_string("SELECT path FROM categories WHERE id=?1", &[iv(id)])?
        .ok_or_else(|| Error::not_found("category_not_found", format!("no category #{id}")))
}

/// Create `path` and any missing ancestors. Returns the leaf id.
pub fn ensure_category(ctx: &Ctx, path: &str, kind: &str, group: Option<&str>) -> Result<i64> {
    let p = normalize_path(path)?;
    let mut parent: Option<i64> = None;
    let mut prefix = String::new();
    let segs: Vec<&str> = p.split('.').collect();
    let mut kind = kind.to_string();
    for (i, seg) in segs.iter().enumerate() {
        if !prefix.is_empty() {
            prefix.push('.');
        }
        prefix.push_str(seg);
        if let Some(id) = ctx.opt_i64("SELECT id FROM categories WHERE path=?1", &[sv(&prefix)])? {
            let existing_kind = ctx
                .opt_string("SELECT kind FROM categories WHERE id=?1", &[iv(id)])?
                .unwrap_or_default();
            if i + 1 < segs.len() || existing_kind == kind {
                kind = existing_kind;
            } else {
                return Err(Error::conflict(
                    "category_kind_mismatch",
                    format!("'{prefix}' already exists as {existing_kind}"),
                ));
            }
            parent = Some(id);
            continue;
        }
        let g = if i + 1 == segs.len() { group } else { None };
        let id = ctx.insert(
            "categories",
            &[
                ("path", sv(&prefix)),
                ("parent_id", crate::ctx::oiv(parent)),
                ("name", sv(seg)),
                ("kind", sv(&kind)),
                ("group_name", osv(g)),
            ],
        )?;
        parent = Some(id);
    }
    parent.ok_or_else(|| Error::internal("empty_path", "empty category path"))
}

pub fn seed_defaults(ctx: &Ctx) -> Result<usize> {
    for (path, kind, group) in DEFAULT_CATEGORIES {
        ensure_category(ctx, path, kind, Some(group))?;
    }
    Ok(DEFAULT_CATEGORIES.len())
}

fn cat_json(ctx: &Ctx, id: i64) -> Result<Value> {
    let rows = ctx.rows(
        "SELECT id, path, kind, group_name AS \"group\", parent_id FROM categories WHERE id=?1",
        &[iv(id)],
    )?;
    rows.into_iter()
        .next()
        .map(Value::Object)
        .ok_or_else(|| Error::not_found("category_not_found", format!("no category #{id}")))
}

fn add(ctx: &Ctx, a: &Args) -> Result<Value> {
    let path = normalize_path(&a.str("path")?)?;
    if ctx
        .opt_i64("SELECT id FROM categories WHERE path=?1", &[sv(&path)])?
        .is_some()
    {
        return Err(Error::conflict(
            "already_exists",
            format!("category '{path}' already exists"),
        ));
    }
    let kind = a.opt_str("kind").unwrap_or_else(|| "expense".into());
    let id = ensure_category(ctx, &path, &kind, a.opt_str("group").as_deref())?;
    cat_json(ctx, id)
}

fn list(ctx: &Ctx, a: &Args) -> Result<Value> {
    let mut sql = String::from(
        "SELECT c.id, c.path, c.kind, c.group_name AS \"group\",
           (SELECT COUNT(*) FROM splits s JOIN transactions t ON t.id=s.txn_id
             WHERE s.category_id=c.id AND t.deleted_at IS NULL) AS uses
         FROM categories c WHERE 1=1",
    );
    let mut params = vec![];
    if let Some(k) = a.opt_str("kind") {
        params.push(sv(&k));
        sql.push_str(&format!(" AND c.kind=?{}", params.len()));
    }
    sql.push_str(" ORDER BY c.kind DESC, c.path");
    Ok(Value::Array(
        ctx.rows(&sql, &params)?
            .into_iter()
            .map(Value::Object)
            .collect(),
    ))
}

fn tree(ctx: &Ctx, a: &Args) -> Result<Value> {
    let flat = ctx.rows("SELECT id, path, name, kind, group_name AS \"group\", parent_id FROM categories ORDER BY path", &[])?;
    let mut children: BTreeMap<Option<i64>, Vec<&serde_json::Map<String, Value>>> = BTreeMap::new();
    for r in &flat {
        children.entry(r["parent_id"].as_i64()).or_default().push(r);
    }
    fn build(
        parent: Option<i64>,
        kids: &BTreeMap<Option<i64>, Vec<&serde_json::Map<String, Value>>>,
        kind: Option<&str>,
    ) -> Vec<Value> {
        let mut out = vec![];
        for r in kids.get(&parent).cloned().unwrap_or_default() {
            if kind.is_some_and(|k| r["kind"].as_str() != Some(k)) {
                continue;
            }
            let id = r["id"].as_i64();
            out.push(json!({
                "path": r["path"], "name": r["name"], "kind": r["kind"], "group": r["group"],
                "children": build(id, kids, kind),
            }));
        }
        out
    }
    Ok(Value::Array(build(
        None,
        &children,
        a.opt_str("kind").as_deref(),
    )))
}

fn ref_tables(ctx: &Ctx) -> Result<Vec<(&'static str, &'static str)>> {
    let mut v = Vec::new();
    for (t, c) in [
        ("budgets", "category_id"),
        ("rules", "set_category_id"),
        ("recurring", "category_id"),
    ] {
        if ctx
            .opt_i64(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                &[sv(t)],
            )?
            .is_some()
        {
            v.push((t, c));
        }
    }
    Ok(v)
}

fn rename(ctx: &Ctx, a: &Args) -> Result<Value> {
    let old = normalize_path(&a.str("path")?)?;
    let id = category_id(ctx, &old)?;
    let new_name = a.str("new_name")?.trim().to_ascii_lowercase();
    if !valid_segment(&new_name) {
        return Err(Error::validation(
            "invalid_category",
            "new name must be lowercase letters, digits, _ or -",
        )
        .with_field("new_name"));
    }
    let new_path = match old.rsplit_once('.') {
        Some((parent, _)) => format!("{parent}.{new_name}"),
        None => new_name.clone(),
    };
    if ctx
        .opt_i64("SELECT id FROM categories WHERE path=?1", &[sv(&new_path)])?
        .is_some()
    {
        return Err(Error::conflict(
            "already_exists",
            format!("category '{new_path}' already exists"),
        ));
    }
    let prefix = format!("{old}.");
    let rows = ctx.rows(
        "SELECT id, path FROM categories WHERE path=?1 OR substr(path,1,?2)=?3 ORDER BY path",
        &[sv(&old), iv(prefix.len() as i64), sv(&prefix)],
    )?;
    for r in rows {
        let rid = r["id"].as_i64().unwrap_or(0);
        let p = r["path"].as_str().unwrap_or("");
        let np = format!("{new_path}{}", &p[old.len()..]);
        let mut cols = vec![("path", sv(&np))];
        if rid == id {
            cols.push(("name", sv(&new_name)));
        }
        ctx.update("categories", rid, &cols)?;
    }
    cat_json(ctx, id)
}

fn rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let path = normalize_path(&a.str("path")?)?;
    let id = category_id(ctx, &path)?;
    if ctx.one_i64(
        "SELECT COUNT(*) FROM categories WHERE parent_id=?1",
        &[iv(id)],
    )? > 0
    {
        return Err(Error::conflict(
            "category_has_children",
            format!("'{path}' has sub-categories"),
        )
        .with_hint("remove or merge the sub-categories first"));
    }
    let mut uses = ctx.one_i64(
        "SELECT COUNT(*) FROM splits WHERE category_id=?1",
        &[iv(id)],
    )?;
    for (t, c) in ref_tables(ctx)? {
        uses += ctx.one_i64(&format!("SELECT COUNT(*) FROM {t} WHERE {c}=?1"), &[iv(id)])?;
    }
    if uses > 0 {
        return Err(Error::conflict(
            "category_in_use",
            format!("'{path}' is used by {uses} record(s)"),
        )
        .with_hint("use `pext cat merge --from X --into Y` to move them"));
    }
    ctx.delete("categories", id)?;
    Ok(json!({ "removed": path }))
}

fn merge(ctx: &Ctx, a: &Args) -> Result<Value> {
    let from = normalize_path(&a.str("from")?)?;
    let into = normalize_path(&a.str("into")?)?;
    if from == into {
        return Err(Error::validation(
            "invalid_merge",
            "cannot merge a category into itself",
        ));
    }
    let fid = category_id(ctx, &from)?;
    let tid = category_id(ctx, &into)?;
    let kf = ctx.opt_string("SELECT kind FROM categories WHERE id=?1", &[iv(fid)])?;
    let kt = ctx.opt_string("SELECT kind FROM categories WHERE id=?1", &[iv(tid)])?;
    if kf != kt {
        return Err(Error::validation(
            "category_kind_mismatch",
            "cannot merge an expense category into an income category",
        ));
    }
    if ctx.one_i64(
        "SELECT COUNT(*) FROM categories WHERE parent_id=?1",
        &[iv(fid)],
    )? > 0
    {
        return Err(Error::conflict(
            "category_has_children",
            format!("'{from}' has sub-categories"),
        )
        .with_hint("merge or move the sub-categories first"));
    }
    let mut moved = 0;
    let mut targets: Vec<(&str, &str)> = vec![("splits", "category_id")];
    targets.extend(ref_tables(ctx)?);
    for (t, c) in targets {
        let ids: Vec<i64> = {
            let mut st = ctx
                .conn
                .prepare(&format!("SELECT id FROM {t} WHERE {c}=?1"))
                .db()?;
            let rows = st.query_map([fid], |r| r.get(0)).db()?;
            rows.collect::<rusqlite::Result<_>>().db()?
        };
        for id in ids {
            ctx.update(t, id, &[(c, iv(tid))])?;
            moved += 1;
        }
    }
    ctx.delete("categories", fid)?;
    Ok(json!({ "merged": from, "into": into, "records_moved": moved }))
}

// ---- payees ---------------------------------------------------------------

pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn normalize_payee(ctx: &Ctx, raw: &str) -> Result<String> {
    let p = collapse_ws(raw);
    if p.chars().count() > 200 || p.chars().any(|c| c.is_control()) {
        return Err(Error::validation(
            "invalid_payee",
            "payee must be at most 200 characters without control characters",
        )
        .with_field("payee"));
    }
    Ok(ctx
        .opt_string(
            "SELECT canonical FROM payee_aliases WHERE alias=?1",
            &[sv(&p)],
        )?
        .unwrap_or(p))
}

fn payee_list(ctx: &Ctx, a: &Args) -> Result<Value> {
    let limit = a.opt_i64("limit")?.unwrap_or(100).clamp(1, 10_000);
    Ok(Value::Array(
        ctx.rows(
            "SELECT payee, currency, COUNT(*) AS count, SUM(amount_minor) AS total_minor, MAX(date) AS last_date
             FROM transactions WHERE deleted_at IS NULL AND payee != '' AND kind='standard'
             GROUP BY payee COLLATE NOCASE, currency ORDER BY count DESC, payee LIMIT ?1",
            &[iv(limit)],
        )?
        .into_iter()
        .map(Value::Object)
        .collect(),
    ))
}

fn alias_add(ctx: &Ctx, a: &Args) -> Result<Value> {
    let alias = collapse_ws(&a.str("alias")?);
    let canonical = collapse_ws(&a.str("canonical")?);
    if alias.is_empty() || canonical.is_empty() || alias.eq_ignore_ascii_case(&canonical) {
        return Err(Error::validation(
            "invalid_alias",
            "alias and canonical name must be non-empty and different",
        ));
    }
    ctx.insert(
        "payee_aliases",
        &[("alias", sv(&alias)), ("canonical", sv(&canonical))],
    )?;
    Ok(json!({ "alias": alias, "canonical": canonical }))
}

fn alias_list(ctx: &Ctx, _a: &Args) -> Result<Value> {
    Ok(Value::Array(
        ctx.rows(
            "SELECT alias, canonical FROM payee_aliases ORDER BY canonical, alias",
            &[],
        )?
        .into_iter()
        .map(Value::Object)
        .collect(),
    ))
}

fn alias_rm(ctx: &Ctx, a: &Args) -> Result<Value> {
    let alias = collapse_ws(&a.str("alias")?);
    let id = ctx
        .opt_i64("SELECT id FROM payee_aliases WHERE alias=?1", &[sv(&alias)])?
        .ok_or_else(|| Error::not_found("alias_not_found", format!("no alias '{alias}'")))?;
    ctx.delete("payee_aliases", id)?;
    Ok(json!({ "removed": alias }))
}

fn payee_normalize(ctx: &Ctx, _a: &Args) -> Result<Value> {
    let aliases = ctx.rows("SELECT alias, canonical FROM payee_aliases", &[])?;
    let mut updated = 0;
    for al in aliases {
        let (alias, canon) = (
            al["alias"].as_str().unwrap_or(""),
            al["canonical"].as_str().unwrap_or(""),
        );
        let ids: Vec<i64> = {
            let mut st = ctx
                .conn
                .prepare("SELECT id FROM transactions WHERE payee = ?1 COLLATE NOCASE AND payee != ?2 AND deleted_at IS NULL")
                .db()?;
            let rows = st
                .query_map(rusqlite::params![alias, canon], |r| r.get(0))
                .db()?;
            rows.collect::<rusqlite::Result<_>>().db()?
        };
        for id in ids {
            ctx.update("transactions", id, &[("payee", sv(canon))])?;
            updated += 1;
        }
    }
    Ok(json!({ "updated": updated }))
}

fn tag_list(ctx: &Ctx, _a: &Args) -> Result<Value> {
    Ok(Value::Array(
        ctx.rows(
            "SELECT g.tag, COUNT(*) AS count FROM txn_tags g JOIN transactions t ON t.id=g.txn_id
             WHERE t.deleted_at IS NULL GROUP BY g.tag COLLATE NOCASE ORDER BY count DESC, g.tag",
            &[],
        )?
        .into_iter()
        .map(Value::Object)
        .collect(),
    ))
}

pub fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::new("cat.add", "Create a category (and missing parents)", add)
            .write()
            .p(Param::s("path", "dotted path, e.g. food.delivery")
                .req()
                .pos())
            .p(Param::s("kind", "expense or income")
                .choices(&["expense", "income"])
                .def("expense"))
            .p(Param::s(
                "group",
                "budget group: needs, wants, savings, ...",
            )),
        OpSpec::new("cat.list", "List categories with usage counts", list).p(Param::s(
            "kind",
            "expense or income",
        )
        .choices(&["expense", "income"])),
        OpSpec::new("cat.tree", "Categories as a nested tree", tree).p(Param::s(
            "kind",
            "expense or income",
        )
        .choices(&["expense", "income"])),
        OpSpec::new(
            "cat.rename",
            "Rename a category (and its sub-categories' paths)",
            rename,
        )
        .write()
        .p(Param::s("path", "current path").req().pos())
        .p(Param::s("new_name", "new last segment").req()),
        OpSpec::new("cat.rm", "Remove an unused category", rm)
            .write()
            .p(Param::s("path", "category path").req().pos()),
        OpSpec::new("cat.merge", "Merge one category into another", merge)
            .write()
            .p(Param::s("from", "category to remove").req())
            .p(Param::s("into", "category that receives its records").req()),
        OpSpec::new("payee.list", "Payees by frequency", payee_list).p(Param::i(
            "limit",
            "maximum rows",
        )
        .def("100")),
        OpSpec::new(
            "payee.alias.add",
            "Map a messy payee name to a clean one",
            alias_add,
        )
        .write()
        .p(Param::s("alias", "name as it appears in statements").req())
        .p(Param::s("canonical", "clean name").req())
        .ex("pext payee alias add --alias 'AMZN MKTP IN' --canonical Amazon"),
        OpSpec::new("payee.alias.list", "List payee aliases", alias_list),
        OpSpec::new("payee.alias.rm", "Remove a payee alias", alias_rm)
            .write()
            .p(Param::s("alias", "alias to remove").req().pos()),
        OpSpec::new(
            "payee.normalize",
            "Apply aliases to existing transactions",
            payee_normalize,
        )
        .write(),
        OpSpec::new("tag.list", "List tags with usage counts", tag_list),
    ]
}
