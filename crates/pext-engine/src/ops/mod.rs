//! All operations, grouped by domain. `all_ops` feeds the registry.

use crate::registry::OpSpec;

pub mod accounts;
pub mod alerts;
pub mod assets;
pub mod budgets;
pub mod categories;
pub mod fx;
pub mod goals;
pub mod imports;
pub mod invest;
pub mod ious;
pub mod loans;
pub mod networth;
pub mod query;
pub mod recurring;
pub mod reports;
pub mod rules;
pub mod subs;
pub mod system;
pub mod txns;

pub fn all_ops() -> Vec<OpSpec> {
    let mut v = Vec::new();
    v.extend(system::ops());
    v.extend(accounts::ops());
    v.extend(categories::ops());
    v.extend(txns::ops());
    v.extend(imports::ops());
    v.extend(fx::ops());
    v.extend(budgets::ops());
    v.extend(alerts::ops());
    v.extend(goals::ops());
    v.extend(loans::ops());
    v.extend(ious::ops());
    v.extend(assets::ops());
    v.extend(invest::ops());
    v.extend(networth::ops());
    v.extend(reports::ops());
    v.extend(recurring::ops());
    v.extend(subs::ops());
    v.extend(rules::ops());
    v.extend(query::ops());
    v.extend(crate::audit::ops());
    v
}
