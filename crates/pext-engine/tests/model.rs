#![allow(clippy::single_match)]
//! Model-based test: random sequences of ledger operations, checked against an
//! independent in-memory model, with invariants and undo verified at each step.
mod common;
use common::H;
use proptest::prelude::*;
use serde_json::json;
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
enum Step {
    Add {
        acct: usize,
        minor: i64,
        cat: Option<usize>,
    },
    Rm(usize),
    Restore(usize),
    Edit {
        which: usize,
        minor: i64,
    },
    Transfer {
        from: usize,
        to: usize,
        minor: i64,
    },
    Split {
        which: usize,
        parts: usize,
    },
    Undo,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => (0..3usize, prop_oneof![1..=5_000_000i64, -5_000_000..=-1i64], prop::option::of(0..4usize))
            .prop_map(|(acct, minor, cat)| Step::Add { acct, minor, cat }),
        2 => (0..50usize).prop_map(Step::Rm),
        1 => (0..50usize).prop_map(Step::Restore),
        2 => (0..50usize, prop_oneof![1..=900_000i64, -900_000..=-1i64]).prop_map(|(which, minor)| Step::Edit { which, minor }),
        2 => (0..3usize, 0..3usize, 1..=1_000_000i64).prop_map(|(from, to, minor)| Step::Transfer { from, to, minor }),
        1 => (0..50usize, 2..4usize).prop_map(|(which, parts)| Step::Split { which, parts }),
        2 => Just(Step::Undo),
    ]
}

#[derive(Clone, Default)]
struct Model {
    opening: [i64; 3],
    /// id -> (account, amount, live, twin, standard)
    txns: BTreeMap<i64, (usize, i64, bool, Option<i64>)>,
}

impl Model {
    fn balance(&self, a: usize) -> i64 {
        self.opening[a]
            + self
                .txns
                .values()
                .filter(|t| t.0 == a && t.2)
                .map(|t| t.1)
                .sum::<i64>()
    }
}

const NAMES: [&str; 3] = ["A", "B", "C"];
const CATS: [&str; 4] = ["food.dining", "food.groceries", "transport.fuel", "salary"];

fn nth<T: Clone>(m: &BTreeMap<i64, T>, i: usize) -> Option<(i64, T)> {
    if m.is_empty() {
        None
    } else {
        m.iter().nth(i % m.len()).map(|(k, v)| (*k, v.clone()))
    }
}

fn check(h: &H, model: &Model) {
    for (i, n) in NAMES.iter().enumerate() {
        assert_eq!(h.balance(n), model.balance(i), "balance of {n}");
    }
    // money is conserved: transfers net to zero
    let total: i64 = (0..3).map(|i| h.balance(NAMES[i])).sum();
    let live_sum: i64 = model.opening.iter().sum::<i64>()
        + model
            .txns
            .values()
            .filter(|t| t.2)
            .map(|t| t.1)
            .sum::<i64>();
    assert_eq!(total, live_sum);
    h.doctor_ok();
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, max_shrink_iters: 200, .. ProptestConfig::default() })]
    #[test]
    fn ledger_matches_model(steps in prop::collection::vec(step(), 1..28)) {
        let h = H::new();
        let mut model = Model::default();
        for (i, n) in NAMES.iter().enumerate() {
            let open = (i as i64 + 1) * 100_000;
            h.ok("acct.add", json!({"name": n, "kind": "checking", "opening_minor": open}));
            model.opening[i] = open;
        }
        // (model, dump) before each undoable step
        let mut history: Vec<(Model, String)> = vec![];
        for s in steps {
            let before_dump = h.dump();
            let before_model = model.clone();
            let mut mutated = true;
            match s {
                Step::Add { acct, minor, cat } => {
                    let mut a = json!({"account": NAMES[acct], "amount_minor": minor, "date": "2026-06-01"});
                    if let Some(c) = cat { a["category"] = json!(CATS[c]); }
                    let id = h.ok("txn.add", a)["id"].as_i64().unwrap();
                    model.txns.insert(id, (acct, minor, true, None));
                }
                Step::Rm(i) => {
                    let live: BTreeMap<i64, _> = model.txns.iter().filter(|t| t.1.2).map(|(k, v)| (*k, *v)).collect();
                    match nth(&live, i) {
                        Some((id, (_, _, _, twin))) => {
                            h.ok("txn.rm", json!({"id": id}));
                            model.txns.get_mut(&id).unwrap().2 = false;
                            if let Some(t) = twin { model.txns.get_mut(&t).unwrap().2 = false; }
                        }
                        None => mutated = false,
                    }
                }
                Step::Restore(i) => {
                    let dead: BTreeMap<i64, _> = model.txns.iter().filter(|t| !t.1.2).map(|(k, v)| (*k, *v)).collect();
                    match nth(&dead, i) {
                        Some((id, (_, _, _, twin))) => {
                            h.ok("txn.restore", json!({"id": id}));
                            model.txns.get_mut(&id).unwrap().2 = true;
                            if let Some(t) = twin { model.txns.get_mut(&t).unwrap().2 = true; }
                        }
                        None => mutated = false,
                    }
                }
                Step::Edit { which, minor } => {
                    let plain: BTreeMap<i64, _> = model.txns.iter().filter(|t| t.1.2 && t.1.3.is_none()).map(|(k, v)| (*k, *v)).collect();
                    match nth(&plain, which) {
                        Some((id, (_, old, _, _))) => {
                            let m = if minor == old { minor + 1 } else { minor };
                            let amount = pext_core::money::format_amount(m, 2);
                            // multi-split transactions cannot change amount silently: engine decides
                            match h.call("txn.edit", json!({"id": id, "amount": amount})) {
                                Ok(_) => { model.txns.get_mut(&id).unwrap().1 = m; }
                                Err(e) => {
                                    let k = e.kind.as_str();
                                    prop_assert!(k == "validation" || k == "conflict", "unexpected {:?}", e);
                                    mutated = false;
                                }
                            }
                        }
                        None => mutated = false,
                    }
                }
                Step::Transfer { from, to, minor } => {
                    if from == to {
                        let r = h.call("txn.transfer", json!({"from": NAMES[from], "to": NAMES[to], "amount_minor": minor}));
                        prop_assert!(r.is_err(), "same-account transfer must fail");
                        mutated = false;
                    } else {
                        let r = h.ok("txn.transfer", json!({"from": NAMES[from], "to": NAMES[to], "amount_minor": minor, "date": "2026-06-02"}));
                        let (o, i) = (r["out"]["id"].as_i64().unwrap(), r["in"]["id"].as_i64().unwrap());
                        model.txns.insert(o, (from, -minor, true, Some(i)));
                        model.txns.insert(i, (to, minor, true, Some(o)));
                    }
                }
                Step::Split { which, parts } => {
                    let plain: BTreeMap<i64, _> = model.txns.iter().filter(|t| t.1.2 && t.1.3.is_none()).map(|(k, v)| (*k, *v)).collect();
                    match nth(&plain, which) {
                        Some((id, (_, amt, _, _))) => {
                            let total = amt.abs();
                            let alloc = pext_core::money::allocate(total, &vec![1u64; parts]).unwrap();
                            let split: Vec<String> = alloc.iter().enumerate()
                                .filter(|(_, a)| **a > 0)
                                .map(|(k, a)| format!("{}={}", CATS[k % CATS.len()], pext_core::money::format_amount(*a, 2)))
                                .collect();
                            // duplicate categories are fine; amounts must sum to |amount|
                            h.ok("txn.split", json!({"id": id, "split": split}));
                        }
                        None => mutated = false,
                    }
                }
                Step::Undo => {
                    match history.pop() {
                        Some((m, d)) => {
                            h.ok("undo", json!({}));
                            prop_assert_eq!(h.dump(), d, "undo must restore the exact prior state");
                            model = m;
                        }
                        None => {} // would undo the account setup; skip
                    }
                    mutated = false;
                }
            }
            if mutated {
                history.push((before_model, before_dump));
            }
            check(&h, &model);
        }
        prop_assert_eq!(h.ok("audit.verify", json!({}))["ok"].clone(), json!(true));
    }
}
