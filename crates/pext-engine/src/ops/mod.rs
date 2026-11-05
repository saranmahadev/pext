//! All operations, grouped by domain. `all_ops` feeds the registry.

use crate::registry::OpSpec;

pub mod access;
pub mod accounts;
pub mod alerts;
pub mod assets;
pub mod backup;
pub mod budgets;
pub mod categories;
pub mod fx;
pub mod goals;
pub mod imports;
pub mod invest;
pub mod ious;
pub mod loans;
pub mod networth;
pub mod plugins;
pub mod query;
pub mod recurring;
pub mod reports;
pub mod rules;
pub mod subs;
pub mod sync;
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
    v.extend(access::ops());
    v.extend(backup::ops());
    v.extend(sync::ops());
    v.extend(plugins::ops());
    v.extend(reports::ops());
    v.extend(recurring::ops());
    v.extend(subs::ops());
    v.extend(rules::ops());
    v.extend(query::ops());
    v.extend(crate::audit::ops());
    v
}
