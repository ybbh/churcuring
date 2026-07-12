//! Symbolic backend: bounded model checking + k-induction via z3.rs.
//!
//! - Safety (`[]φ`): BMC loop `k = 0..=depth` searching for `¬φ`; SAT yields a
//!   counterexample decoded from the model. If no violation is found, a
//!   1-induction attempt (`φ(s) ∧ T(s,s') ⇒ φ(s')`) can upgrade the verdict
//!   to `Proved` (reporting discipline: `doc/model-check.md` §8).
//! - Liveness (`<>φ`): unsupported here by design — the explicit Stateright
//!   backend owns liveness in v1.

use z3::{SatResult, Solver};

use crate::counterexample::{CexStep, Counterexample, Verdict};
use crate::encode::{Encoder, StepVars};
use crate::eval::{param_space, step, State, StepOutcome, TableData};
use crate::ir::{McSpec, PropertyKind};

/// Run the symbolic check. One verdict per property.
pub fn check(spec: &McSpec) -> Vec<Verdict> {
    spec.properties
        .iter()
        .map(|p| match &p.kind {
            PropertyKind::Always(e) => check_safety(spec, &p.name, e),
            PropertyKind::Eventually(_) => Verdict::Unsupported {
                property: p.name.clone(),
                reason: "z3 backend checks safety only; use the stateright backend for liveness"
                    .to_string(),
            },
        })
        .collect()
}

fn check_safety(spec: &McSpec, name: &str, e: &crate::ir::McExpr) -> Verdict {
    let enc = Encoder::new(spec);
    let solver = Solver::new();
    let depth = spec.depth;
    let vars: Vec<StepVars> = (0..=depth).map(|i| enc.step_vars(i)).collect();

    enc.assert_init(&solver, &vars[0]);
    for i in 0..depth {
        enc.assert_step(&solver, &vars[i as usize], &vars[(i + 1) as usize], i);
    }

    // BMC: search for a violation at each prefix length.
    for k in 0..=depth {
        solver.push();
        let p = enc.encode(&vars[k as usize], e, &[]).into_bool();
        solver.assert(p.not());
        if solver.check() == SatResult::Sat {
            let cex = extract_counterexample(spec, &vars, &solver, name, k);
            return Verdict::Counterexample {
                property: name.to_string(),
                cex,
            };
        }
        solver.pop(1);
    }

    // 1-induction: φ(s) ∧ T(s, s') ⇒ φ(s') for arbitrary s.
    let prover = Solver::new();
    let s0 = enc.step_vars(depth + 100);
    let s1 = enc.step_vars(depth + 101);
    enc.assert_step(&prover, &s0, &s1, depth + 100);
    prover.assert(enc.encode(&s0, e, &[]).into_bool());
    prover.assert(enc.encode(&s1, e, &[]).into_bool().not());
    if prover.check() == SatResult::Unsat {
        Verdict::Proved {
            property: name.to_string(),
            by: "z3-k-induction",
        }
    } else {
        Verdict::BoundedOk {
            property: name.to_string(),
            depth,
        }
    }
}

/// Decode a satisfying model into a concrete counterexample trace: evaluate
/// the table arrays at every relevant key for steps `0..=k`, then relabel
/// each step by replaying the concrete evaluator.
fn extract_counterexample(
    spec: &McSpec,
    vars: &[StepVars],
    solver: &Solver,
    property: &str,
    k: u32,
) -> Counterexample {
    let model = solver.get_model().expect("model after SAT");
    let keys = spec.relevant_keys();

    let mut states: Vec<State> = Vec::with_capacity((k + 1) as usize);
    for i in 0..=k as usize {
        let mut tables: Vec<TableData> = vec![TableData::new(); spec.tables.len()];
        for (t, _) in spec.tables.iter().enumerate() {
            for key in &keys {
                let kz3 = z3::ast::Int::from_i64(*key);
                let has = model
                    .eval(
                        &vars[i].has[t].select(&kz3).as_bool().expect("bool select"),
                        true,
                    )
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false);
                if has {
                    let val = model
                        .eval(
                            &vars[i].val[t].select(&kz3).as_int().expect("int select"),
                            true,
                        )
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    tables[t].insert(*key, val);
                }
            }
        }
        states.push(State { tables });
    }

    // Relabel steps by concrete replay (deterministic: first matching action).
    let mut steps = Vec::with_capacity(states.len());
    for (i, state) in states.iter().enumerate() {
        let label = if i == 0 {
            "init".to_string()
        } else {
            relabel(spec, &states[i - 1], state)
        };
        steps.push(CexStep {
            label,
            state: state.clone(),
        });
    }
    Counterexample {
        property: property.to_string(),
        steps,
    }
}

/// Find the transition + parameters explaining `from → to` by concrete replay.
fn relabel(spec: &McSpec, from: &State, to: &State) -> String {
    for (tr, t) in spec.transitions.iter().enumerate() {
        for params in param_space(spec, tr) {
            let (outcome, next) = step(spec, from, tr, &params);
            if outcome != StepOutcome::Disabled && &next == to {
                return format!(
                    "{}({}) {:?}",
                    t.name,
                    params
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                    outcome
                );
            }
        }
    }
    "?? (no explaining transition found)".to_string()
}
