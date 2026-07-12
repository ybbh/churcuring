//! Concrete evaluator for the IR: states, expression evaluation, and the
//! transition step relation. Shared by the Stateright backend (`next_state`)
//! and available to any future interpreter — its semantics are the reference
//! against which the SMT encoding is differentially tested.

use std::collections::BTreeMap;

use crate::ir::{McExpr, McSpec, Ty, UpdateKind};

/// One table's contents, keyed in canonical (ascending) order so that `State`
/// hashing/equality is deterministic (`doc/cql.md` §5.1 canonical order).
pub type TableData = BTreeMap<i64, i64>;

/// A concrete snapshot: one partial map per table.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct State {
    pub tables: Vec<TableData>,
}

/// A concrete value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    B(bool),
    I(i64),
}

impl Value {
    pub fn as_bool(self) -> bool {
        match self {
            Value::B(b) => b,
            Value::I(_) => panic!("expected bool, got int"),
        }
    }
    pub fn as_int(self) -> i64 {
        match self {
            Value::I(i) => i,
            Value::B(_) => panic!("expected int, got bool"),
        }
    }
}

impl McSpec {
    /// The fixture initial state.
    pub fn init_state(&self) -> State {
        let mut state = State {
            tables: vec![TableData::new(); self.tables.len()],
        };
        for (t, k, v) in &self.init {
            state.tables[*t].insert(*k, *v);
        }
        state
    }
}

/// Evaluate an expression against a concrete state.
///
/// Arithmetic traps follow CQL §5.3 (division by zero, overflow); in a
/// concrete check they surface as panics — the bounded domains used in model
/// checking are expected to avoid them, and a future trap-freedom check will
/// turn them into findings instead.
pub fn eval(state: &State, e: &McExpr, params: &[i64]) -> Value {
    match e {
        McExpr::BoolLit(b) => Value::B(*b),
        McExpr::IntLit(i) => Value::I(*i),
        McExpr::Param(i) => Value::I(params[*i]),
        McExpr::Select { table, key } => {
            let k = eval(state, key, params).as_int();
            Value::I(*state.tables[*table].get(&k).unwrap_or(&0))
        }
        McExpr::Contains { table, key } => {
            let k = eval(state, key, params).as_int();
            Value::B(state.tables[*table].contains_key(&k))
        }
        McExpr::Not(a) => Value::B(!eval(state, a, params).as_bool()),
        McExpr::And(es) => Value::B(es.iter().all(|e| eval(state, e, params).as_bool())),
        McExpr::Or(es) => Value::B(es.iter().any(|e| eval(state, e, params).as_bool())),
        McExpr::Implies(a, b) => {
            Value::B(!eval(state, a, params).as_bool() || eval(state, b, params).as_bool())
        }
        McExpr::Eq(a, b) => Value::B(eval(state, a, params) == eval(state, b, params)),
        McExpr::Ne(a, b) => Value::B(eval(state, a, params) != eval(state, b, params)),
        McExpr::Lt(a, b) => {
            Value::B(eval(state, a, params).as_int() < eval(state, b, params).as_int())
        }
        McExpr::Le(a, b) => {
            Value::B(eval(state, a, params).as_int() <= eval(state, b, params).as_int())
        }
        McExpr::Gt(a, b) => {
            Value::B(eval(state, a, params).as_int() > eval(state, b, params).as_int())
        }
        McExpr::Ge(a, b) => {
            Value::B(eval(state, a, params).as_int() >= eval(state, b, params).as_int())
        }
        McExpr::Add(a, b) => Value::I(
            eval(state, a, params)
                .as_int()
                .checked_add(eval(state, b, params).as_int())
                .expect("int overflow (trap, cql.md §5.3)"),
        ),
        McExpr::Sub(a, b) => Value::I(
            eval(state, a, params)
                .as_int()
                .checked_sub(eval(state, b, params).as_int())
                .expect("int overflow (trap, cql.md §5.3)"),
        ),
        McExpr::Mul(a, b) => Value::I(
            eval(state, a, params)
                .as_int()
                .checked_mul(eval(state, b, params).as_int())
                .expect("int overflow (trap, cql.md §5.3)"),
        ),
        McExpr::Sum { table, domain } => Value::I(
            domain
                .iter()
                .map(|k| state.tables[*table].get(k).copied().unwrap_or(0))
                .sum(),
        ),
    }
}

/// Result of firing one transition (cf. `doc/model-check.md` §2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepOutcome {
    /// Writes applied; state changed (possibly to an equal state).
    Applied,
    /// Guard held but write-op conflict or existence constraint failed:
    /// rejected self-loop, state unchanged (cql.md §5.2).
    Rejected,
    /// Guard false: the transition is not enabled.
    Disabled,
}

/// Fire transition `tr` with `params` against `state`.
/// Returns the outcome and the (possibly unchanged) successor state.
pub fn step(spec: &McSpec, state: &State, tr: usize, params: &[i64]) -> (StepOutcome, State) {
    let t = &spec.transitions[tr];
    debug_assert_eq!(t.params.len(), params.len());
    debug_assert!(t.params.iter().all(|ty| *ty == Ty::Int));

    if !eval(state, &t.guard, params).as_bool() {
        return (StepOutcome::Disabled, state.clone());
    }

    // Evaluate all write keys against the current state.
    let keys: Vec<i64> = t
        .updates
        .iter()
        .map(|u| eval(state, &u.key, params).as_int())
        .collect();

    // Write-op conflict: two writes on the same (table, key) ⇒ reject (§3.6).
    for i in 0..keys.len() {
        for j in (i + 1)..keys.len() {
            if t.updates[i].table == t.updates[j].table && keys[i] == keys[j] {
                return (StepOutcome::Rejected, state.clone());
            }
        }
    }

    // Existence constraints: insert ⇒ absent, update ⇒ present (§3.6/§5.2).
    for (u, k) in t.updates.iter().zip(&keys) {
        let present = state.tables[u.table].contains_key(k);
        let violated = match u.kind {
            UpdateKind::Insert => present,
            UpdateKind::Update => !present,
            UpdateKind::Delete => false,
        };
        if violated {
            return (StepOutcome::Rejected, state.clone());
        }
    }

    // Apply atomically.
    let mut next = state.clone();
    for (u, k) in t.updates.iter().zip(&keys) {
        match u.kind {
            UpdateKind::Insert | UpdateKind::Update => {
                let v = eval(state, u.value.as_ref().expect("insert/update carries a value"), params)
                    .as_int();
                next.tables[u.table].insert(*k, v);
            }
            UpdateKind::Delete => {
                next.tables[u.table].remove(k);
            }
        }
    }
    (StepOutcome::Applied, next)
}

/// Cartesian product of the parameter domains of transition `tr`
/// (deterministic order, as required by Stateright's `Model::actions`).
pub fn param_space(spec: &McSpec, tr: usize) -> Vec<Vec<i64>> {
    let domains = &spec.transitions[tr].param_domains;
    let mut out: Vec<Vec<i64>> = vec![Vec::new()];
    for d in domains {
        let mut acc = Vec::with_capacity(out.len() * d.len());
        for prefix in &out {
            for v in d {
                let mut p = prefix.clone();
                p.push(*v);
                acc.push(p);
            }
        }
        out = acc;
    }
    out
}
