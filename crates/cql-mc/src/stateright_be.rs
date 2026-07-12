//! Explicit-state backend: adapts [`McSpec`] to Stateright's `Model` trait.
//!
//! Mapping (`doc/model-check.md` §7):
//! - `Model::State`   = concrete snapshot ([`State`])
//! - `Model::Action`  = `(transition index, parameter values)`
//! - `init_states`    = the fixture state
//! - `actions`        = enabled transitions × parameter-domain product
//! - `next_state`     = [`step`] (rejected ⇒ self-loop, state unchanged)
//! - `always` / `eventually` properties map to Stateright `Property`s.
//!
//! Stateright explores the *entire* finite reachable space, so "no
//! counterexample" for an `always` property is reported as `Proved` for the
//! bounded model (stronger than k-bounded). `eventually` properties inherit
//! Stateright's documented caveat: only paths ending in terminal states or
//! checking boundaries count, so cycle-heavy models can yield false negatives
//! (see Stateright docs for `Property::eventually`).

use stateright::{Checker, Model, Property as SrProperty};

use crate::counterexample::{CexStep, Counterexample, Verdict};
use crate::eval::{eval, param_space, step, State};
use crate::ir::{McSpec, PropertyKind};

/// Stateright model wrapping a spec.
#[derive(Clone, Debug)]
pub struct SrModel {
    pub spec: McSpec,
}

/// Property dispatcher: Stateright conditions are plain `fn` pointers (no
/// captures), so we route through a const-generic index into the spec's
/// property list. Supports up to 32 properties per spec.
fn eval_property_at<const I: usize>(m: &SrModel, s: &State) -> bool {
    let e = match &m.spec.properties[I].kind {
        PropertyKind::Always(e) | PropertyKind::Eventually(e) => e,
    };
    eval(s, e, &[]).as_bool()
}

macro_rules! dispatch_arm {
    ($i:expr, $($n:literal),+ $(,)?) => {
        match $i {
            $( $n => eval_property_at::<$n>, )+
            _ => unreachable!("cql-mc supports at most 32 properties per spec"),
        }
    };
}

fn property_fn(i: usize) -> fn(&SrModel, &State) -> bool {
    dispatch_arm!(
        i, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26, 27, 28, 29, 30, 31
    )
}

impl Model for SrModel {
    type State = State;
    type Action = (usize, Vec<i64>);

    fn init_states(&self) -> Vec<State> {
        vec![self.spec.init_state()]
    }

    fn actions(&self, state: &State, actions: &mut Vec<Self::Action>) {
        for (tr, t) in self.spec.transitions.iter().enumerate() {
            for params in param_space(&self.spec, tr) {
                if eval(state, &t.guard, &params).as_bool() {
                    actions.push((tr, params));
                }
            }
        }
    }

    fn next_state(&self, last_state: &State, action: Self::Action) -> Option<State> {
        let (outcome, next) = step(&self.spec, last_state, action.0, &action.1);
        match outcome {
            // Disabled actions are never generated; rejected ⇒ self-loop.
            crate::eval::StepOutcome::Disabled => None,
            _ => Some(next),
        }
    }

    fn properties(&self) -> Vec<SrProperty<Self>> {
        assert!(
            self.spec.properties.len() <= 32,
            "cql-mc supports at most 32 properties per spec"
        );
        self.spec
            .properties
            .iter()
            .enumerate()
            .map(|(i, p)| {
                // Leak the name once per check run; bounded and tiny.
                let name: &'static str = Box::leak(p.name.clone().into_boxed_str());
                match p.kind {
                    PropertyKind::Always(_) => SrProperty::always(name, property_fn(i)),
                    PropertyKind::Eventually(_) => SrProperty::eventually(name, property_fn(i)),
                }
            })
            .collect()
    }
}

/// Run the explicit-state check. One verdict per property.
pub fn check(spec: &McSpec) -> Vec<Verdict> {
    let model = SrModel { spec: spec.clone() };
    let checker = model.checker().spawn_bfs().join();
    let discoveries = checker.discoveries();

    spec.properties
        .iter()
        .map(|p| {
            if let Some(path) = discoveries.get(p.name.as_str()) {
                return Verdict::Counterexample {
                    property: p.name.clone(),
                    cex: path_to_cex(spec, &p.name, path.clone().into()),
                };
            }
            match p.kind {
                PropertyKind::Always(_) => Verdict::Proved {
                    property: p.name.clone(),
                    by: "stateright-exhaustive",
                },
                PropertyKind::Eventually(_) => Verdict::EventuallyHolds {
                    property: p.name.clone(),
                },
            }
        })
        .collect()
}

/// Convert a Stateright path into the unified counterexample, recomputing
/// step outcomes (applied/rejected) for labels.
fn path_to_cex(
    spec: &McSpec,
    property: &str,
    path: Vec<(State, Option<(usize, Vec<i64>)>)>,
) -> Counterexample {
    let mut steps = Vec::with_capacity(path.len());
    for (i, (state, _)) in path.iter().enumerate() {
        // Stateright stores the action alongside the state it departs from,
        // so the transition into step i is recorded at index i-1.
        let label = if i == 0 {
            "init".to_string()
        } else {
            match &path[i - 1].1 {
                Some((tr, params)) => {
                    let (outcome, _) = step(spec, &path[i - 1].0, *tr, params);
                    format!(
                        "{}({}) {:?}",
                        spec.transitions[*tr].name,
                        params
                            .iter()
                            .map(|v| v.to_string())
                            .collect::<Vec<_>>()
                            .join(", "),
                        outcome
                    )
                }
                None => "stutter".to_string(),
            }
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
