//! Auto-categorisation hook used when transactions are created. The rules
//! engine proper arrives in Phase 2; this keeps the call site stable.

use crate::ctx::Ctx;
use crate::registry::OpSpec;
use pext_core::Result;

#[derive(Debug, Default, Clone)]
pub struct AutoResult {
    pub category_id: Option<i64>,
    pub payee: Option<String>,
    pub tags: Vec<String>,
    pub rule_id: Option<i64>,
}

pub struct AutoInput<'a> {
    pub account_id: i64,
    pub payee: &'a str,
    pub memo: &'a str,
    pub amount_minor: i64,
}

pub fn auto(_ctx: &Ctx, _input: &AutoInput) -> Result<AutoResult> {
    Ok(AutoResult::default())
}

pub fn ops() -> Vec<OpSpec> {
    vec![]
}
