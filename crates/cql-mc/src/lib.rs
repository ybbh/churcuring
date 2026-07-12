//! cql-mc: model checker for CQL bounded models.
//!
//! Architecture (`doc/model-check.md` §7): a checker-neutral IR ([`ir`]) with
//! a concrete reference evaluator ([`eval`]), consumed by two backends:
//!
//! - [`stateright_be`] (feature `stateright`): explicit-state exploration via
//!   the Stateright library — safety *and* (basic) liveness, exhaustive over
//!   the finite bounded model.
//! - [`z3_be`] (feature `z3`): symbolic bounded model checking via z3.rs —
//!   BMC for safety violations, k-induction to upgrade invariants to proofs.
//!
//! Both backends emit the same [`Verdict`] / [`Counterexample`] types, so
//! results can be differentially compared.

pub mod counterexample;
pub mod eval;
pub mod ir;

#[cfg(feature = "stateright")]
pub mod stateright_be;
#[cfg(feature = "z3")]
pub mod encode;
#[cfg(feature = "z3")]
pub mod z3_be;

pub use counterexample::{CexStep, Counterexample, Verdict};
pub use eval::{State, StepOutcome, TableData, Value};
pub use ir::{McExpr, McSpec, Property, PropertyKind, TableDecl, Transition, Ty, Update, UpdateKind};

/// Which engine to run (mirrors the future `cqlc verify --engine` flag).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Stateright,
    Z3,
}

/// Run one backend over a spec and return one verdict per property.
#[cfg(all(feature = "stateright", feature = "z3"))]
pub fn check(spec: &McSpec, engine: Engine) -> Vec<Verdict> {
    match engine {
        Engine::Stateright => stateright_be::check(spec),
        Engine::Z3 => z3_be::check(spec),
    }
}
