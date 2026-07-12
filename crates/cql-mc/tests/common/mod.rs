//! Shared spec builders: the bank-ledger example from `doc/model-check.md` §4.1.

use cql_mc::ir::*;

/// `transfer(from, to, amt)`: debit `from`, credit `to`.
/// `debit_only` builds the buggy variant that forgets the credit
/// (violates balance conservation).
pub fn transfer(debit_only: bool) -> Transition {
    let accounts = 0;
    let from = param(0);
    let to = param(1);
    let amt = param(2);
    let mut updates = vec![Update {
        table: accounts,
        key: from.clone(),
        kind: UpdateKind::Update,
        value: Some(sub(select(accounts, from.clone()), amt.clone())),
    }];
    if !debit_only {
        updates.push(Update {
            table: accounts,
            key: to.clone(),
            kind: UpdateKind::Update,
            value: Some(add(select(accounts, to.clone()), amt.clone())),
        });
    }
    Transition {
        name: "transfer".to_string(),
        params: vec![Ty::Int, Ty::Int, Ty::Int],
        param_domains: vec![vec![1, 2], vec![1, 2], vec![0, 100, 6000]],
        guard: and(vec![
            contains(accounts, from.clone()),
            contains(accounts, to.clone()),
            ge(select(accounts, from), amt),
        ]),
        updates,
    }
}

/// `[](total_balance = 10000)` over the two-account domain.
pub fn balance_conserved() -> Property {
    Property {
        name: "balance_conserved".to_string(),
        kind: PropertyKind::Always(eq(sum(0, vec![1, 2]), int(10000))),
    }
}

/// The bank spec: accounts {1: 6000, 2: 4000}, one transfer action.
pub fn bank_spec(debit_only: bool) -> McSpec {
    McSpec {
        tables: vec![TableDecl {
            name: "accounts".to_string(),
        }],
        init: vec![(0, 1, 6000), (0, 2, 4000)],
        transitions: vec![transfer(debit_only)],
        properties: vec![balance_conserved()],
        depth: 4,
    }
}

/// Bank spec plus a trivially-satisfied liveness property
/// (`<>(accounts[1] = 6000)` holds in the initial state).
pub fn bank_spec_with_liveness() -> McSpec {
    let mut spec = bank_spec(false);
    spec.properties.push(Property {
        name: "account_1_can_be_6000".to_string(),
        kind: PropertyKind::Eventually(eq(select(0, int(1)), int(6000))),
    });
    spec
}

/// Sum of the accounts table in a concrete state (test helper).
pub fn total(state: &cql_mc::State) -> i64 {
    state.tables[0].values().sum()
}
