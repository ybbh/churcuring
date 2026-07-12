//! SMT encoding of the IR for the z3 backend.
//!
//! Encoding scheme (`doc/model-check.md` §7): each table at each BMC step is
//! a pair of Z3 arrays — `val_t : Array(Int, Int)` and `has_t : Array(Int,
//! Bool)` (presence, i.e. the partial-map domain). A transition step is
//! `applied ∨ rejected ∨ disabled` per CQL §5.2 semantics, with write-op
//! conflict and existence constraints deciding `rejected`, exactly as the
//! concrete evaluator (`eval.rs`) does — the differential-test contract.

use z3::ast::{Array, Bool, Int};
use z3::{Solver, Sort};

use crate::ir::{McExpr, McSpec, UpdateKind};

/// Z3 arrays for one BMC step.
#[derive(Clone)]
pub struct StepVars {
    pub val: Vec<Array>,
    pub has: Vec<Array>,
}

/// Encoded expression.
pub enum ZA {
    B(Bool),
    I(Int),
}

impl ZA {
    pub fn into_bool(self) -> Bool {
        match self {
            ZA::B(b) => b,
            ZA::I(_) => panic!("type error: expected bool expr"),
        }
    }
    pub fn into_int(self) -> Int {
        match self {
            ZA::I(i) => i,
            ZA::B(_) => panic!("type error: expected int expr"),
        }
    }
}

pub struct Encoder<'a> {
    pub spec: &'a McSpec,
}

fn and_all(bs: &[Bool]) -> Bool {
    Bool::and(&bs.iter().collect::<Vec<_>>())
}

fn or_all(bs: &[Bool]) -> Bool {
    Bool::or(&bs.iter().collect::<Vec<_>>())
}

impl<'a> Encoder<'a> {
    pub fn new(spec: &'a McSpec) -> Self {
        Encoder { spec }
    }

    /// Fresh array variables for one step.
    pub fn step_vars(&self, step: u32) -> StepVars {
        StepVars {
            val: (0..self.spec.tables.len())
                .map(|t| Array::fresh_const(&format!("val_{t}_{step}"), &Sort::int(), &Sort::int()))
                .collect(),
            has: (0..self.spec.tables.len())
                .map(|t| Array::fresh_const(&format!("has_{t}_{step}"), &Sort::int(), &Sort::bool()))
                .collect(),
        }
    }

    fn select_int(v: &StepVars, table: usize, key: &Int) -> Int {
        v.val[table].select(key).as_int().expect("int array select")
    }

    fn select_has(v: &StepVars, table: usize, key: &Int) -> Bool {
        v.has[table].select(key).as_bool().expect("bool array select")
    }

    /// Encode an expression at one step. `params` are the transition
    /// parameter constants for that step (empty for state predicates).
    pub fn encode(&self, v: &StepVars, e: &McExpr, params: &[Int]) -> ZA {
        match e {
            McExpr::BoolLit(b) => ZA::B(Bool::from_bool(*b)),
            McExpr::IntLit(i) => ZA::I(Int::from_i64(*i)),
            McExpr::Param(i) => ZA::I(params[*i].clone()),
            McExpr::Select { table, key } => {
                let k = self.encode(v, key, params).into_int();
                ZA::I(Self::select_int(v, *table, &k))
            }
            McExpr::Contains { table, key } => {
                let k = self.encode(v, key, params).into_int();
                ZA::B(Self::select_has(v, *table, &k))
            }
            McExpr::Not(a) => ZA::B(self.encode(v, a, params).into_bool().not()),
            McExpr::And(es) => {
                let bs: Vec<Bool> = es.iter().map(|e| self.encode(v, e, params).into_bool()).collect();
                ZA::B(and_all(&bs))
            }
            McExpr::Or(es) => {
                let bs: Vec<Bool> = es.iter().map(|e| self.encode(v, e, params).into_bool()).collect();
                ZA::B(or_all(&bs))
            }
            McExpr::Implies(a, b) => {
                let za = self.encode(v, a, params).into_bool();
                let zb = self.encode(v, b, params).into_bool();
                ZA::B(za.implies(&zb))
            }
            McExpr::Eq(a, b) => ZA::B(self.encode_eq(v, a, b, params)),
            McExpr::Ne(a, b) => ZA::B(self.encode_eq(v, a, b, params).not()),
            McExpr::Lt(a, b) => {
                let x = self.encode(v, a, params).into_int();
                let y = self.encode(v, b, params).into_int();
                ZA::B(x.lt(&y))
            }
            McExpr::Le(a, b) => {
                let x = self.encode(v, a, params).into_int();
                let y = self.encode(v, b, params).into_int();
                ZA::B(x.le(&y))
            }
            McExpr::Gt(a, b) => {
                let x = self.encode(v, a, params).into_int();
                let y = self.encode(v, b, params).into_int();
                ZA::B(x.gt(&y))
            }
            McExpr::Ge(a, b) => {
                let x = self.encode(v, a, params).into_int();
                let y = self.encode(v, b, params).into_int();
                ZA::B(x.ge(&y))
            }
            McExpr::Add(a, b) => {
                let x = self.encode(v, a, params).into_int();
                let y = self.encode(v, b, params).into_int();
                ZA::I(x + y)
            }
            McExpr::Sub(a, b) => {
                let x = self.encode(v, a, params).into_int();
                let y = self.encode(v, b, params).into_int();
                ZA::I(x - y)
            }
            McExpr::Mul(a, b) => {
                let x = self.encode(v, a, params).into_int();
                let y = self.encode(v, b, params).into_int();
                ZA::I(x * y)
            }
            McExpr::Sum { table, domain } => {
                let zero = Int::from_i64(0);
                let mut acc = Int::from_i64(0);
                for k in domain {
                    let key = Int::from_i64(*k);
                    let has = Self::select_has(v, *table, &key);
                    let val = Self::select_int(v, *table, &key);
                    acc = acc + has.ite(&val, &zero);
                }
                ZA::I(acc)
            }
        }
    }

    fn encode_eq(&self, v: &StepVars, a: &McExpr, b: &McExpr, params: &[Int]) -> Bool {
        match (self.encode(v, a, params), self.encode(v, b, params)) {
            (ZA::I(x), ZA::I(y)) => x.eq(&y),
            (ZA::B(x), ZA::B(y)) => x.eq(&y),
            _ => panic!("type error: eq on mixed bool/int"),
        }
    }

    /// Fixture initial-state constraints over all relevant keys.
    pub fn assert_init(&self, solver: &Solver, v: &StepVars) {
        let keys = self.spec.relevant_keys();
        for (t, _decl) in self.spec.tables.iter().enumerate() {
            for k in &keys {
                let key = Int::from_i64(*k);
                let present = self
                    .spec
                    .init
                    .iter()
                    .any(|(ft, fk, _)| *ft == t && fk == k);
                solver.assert(Self::select_has(v, t, &key).eq(Bool::from_bool(present)));
                if let Some((_, _, val)) = self.spec.init.iter().find(|(ft, fk, _)| *ft == t && fk == k) {
                    solver.assert(Self::select_int(v, t, &key).eq(Int::from_i64(*val)));
                }
            }
        }
    }

    /// Assert the step relation `T(from, to)`:
    /// `∨_tr (applied ∨ rejected ∨ disabled)` (cql.md §5.2).
    pub fn assert_step(&self, solver: &Solver, from: &StepVars, to: &StepVars, step_idx: u32) {
        let mut branches: Vec<Bool> = Vec::new();
        for (tr_idx, tr) in self.spec.transitions.iter().enumerate() {
            // Fresh parameter constants for this step/transition, pinned to their domains.
            let params: Vec<Int> = (0..tr.params.len())
                .map(|j| Int::fresh_const(&format!("p_{step_idx}_{tr_idx}_{j}")))
                .collect();
            for (p, domain) in params.iter().zip(&tr.param_domains) {
                let cases: Vec<Bool> = domain.iter().map(|v| p.eq(Int::from_i64(*v))).collect();
                solver.assert(or_all(&cases));
            }

            let guard = self.encode(from, &tr.guard, &params).into_bool();
            let keys: Vec<Int> = tr
                .updates
                .iter()
                .map(|u| self.encode(from, &u.key, &params).into_int())
                .collect();

            // Write-op conflict: same (table, key) twice (§3.6).
            let mut conflicts = Vec::new();
            for i in 0..keys.len() {
                for j in (i + 1)..keys.len() {
                    if tr.updates[i].table == tr.updates[j].table {
                        conflicts.push(keys[i].eq(&keys[j]));
                    }
                }
            }
            // Existence-constraint violations (§3.6/§5.2).
            let mut violations = Vec::new();
            for (u, k) in tr.updates.iter().zip(&keys) {
                let present = Self::select_has(from, u.table, k);
                match u.kind {
                    UpdateKind::Insert => violations.push(present),
                    UpdateKind::Update => violations.push(present.not()),
                    UpdateKind::Delete => {}
                }
            }
            let bad = or_all(&[conflicts, violations].concat());
            let ok = bad.not();

            // Frame + effects per table.
            let mut applied_conj: Vec<Bool> = vec![guard.clone(), ok.clone()];
            // Rejected/disabled branches change nothing: frame covers ALL tables.
            let mut frame_all: Vec<Bool> = Vec::new();
            for (t, _) in self.spec.tables.iter().enumerate() {
                frame_all.push(to.val[t].eq(&from.val[t]));
                frame_all.push(to.has[t].eq(&from.has[t]));
            }
            for (t, _) in self.spec.tables.iter().enumerate() {
                let updates_t: Vec<(&crate::ir::Update, &Int)> = tr
                    .updates
                    .iter()
                    .zip(&keys)
                    .filter(|(u, _)| u.table == t)
                    .collect();
                if !updates_t.is_empty() {
                    let mut val_chain = from.val[t].clone();
                    let mut has_chain = from.has[t].clone();
                    for (u, k) in updates_t {
                        match u.kind {
                            UpdateKind::Insert | UpdateKind::Update => {
                                let value = self
                                    .encode(from, u.value.as_ref().expect("insert/update carries a value"), &params)
                                    .into_int();
                                val_chain = val_chain.store(k, &value);
                                has_chain = has_chain.store(k, &Bool::from_bool(true));
                            }
                            UpdateKind::Delete => {
                                has_chain = has_chain.store(k, &Bool::from_bool(false));
                            }
                        }
                    }
                    applied_conj.push(to.val[t].eq(&val_chain));
                    applied_conj.push(to.has[t].eq(&has_chain));
                }
            }

            let applied = and_all(&applied_conj);
            let mut rejected_conj = vec![guard.clone(), bad.clone()];
            rejected_conj.extend(frame_all.iter().map(|b| b.clone()));
            let rejected = and_all(&rejected_conj);
            let mut disabled_conj = vec![guard.not()];
            disabled_conj.extend(frame_all.iter().map(|b| b.clone()));
            let disabled = and_all(&disabled_conj);

            branches.push(applied);
            branches.push(rejected);
            branches.push(disabled);
        }
        solver.assert(or_all(&branches));
    }
}
