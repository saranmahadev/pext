//! All operations, grouped by domain. `all_ops` feeds the registry.

use crate::registry::OpSpec;

pub mod accounts;
pub mod categories;
pub mod imports;
pub mod query;
pub mod rules;
pub mod system;
pub mod txns;

pub fn all_ops() -> Vec<OpSpec> {
    let mut v = Vec::new();
    v.extend(system::ops());
    v.extend(accounts::ops());
    v.extend(categories::ops());
    v.extend(txns::ops());
    v.extend(imports::ops());
    v.extend(rules::ops());
    v.extend(query::ops());
    v.extend(crate::audit::ops());
    v
}
