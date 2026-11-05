//! Hooks for the query language (implemented in Phase 2).

use crate::ctx::Ctx;
use crate::registry::OpSpec;
use pext_core::query::Query;
use pext_core::{Error, Result};

pub fn parse_into(_ctx: &Ctx, _text: &str, _base: Query) -> Result<Query> {
    Err(Error::usage(
        "query_unavailable",
        "the query language is not available yet",
    ))
}

pub fn with_query_param(op: OpSpec) -> OpSpec {
    op
}

pub fn ops() -> Vec<OpSpec> {
    vec![]
}
