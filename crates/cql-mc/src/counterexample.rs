//! Unified counterexample format shared by both backends, with rendering
//! (human-readable trace + CQL `test`-block sketch, `doc/model-check.md` §7).

use std::fmt;

use crate::eval::State;
use crate::ir::McSpec;

/// One step of a counterexample trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CexStep {
    /// e.g. `"init"`, `"transfer(1, 2, 6000) applied"`, `"transfer(1, 1, 100) rejected"`.
    pub label: String,
    pub state: State,
}

/// A bounded trace witnessing a property violation (or a liveness witness).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Counterexample {
    pub property: String,
    pub steps: Vec<CexStep>,
}

/// Backend-neutral verdict for one property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Unconditional proof within the modeled fragment: Stateright exhausted
    /// the (finite) reachable space, or Z3 k-induction succeeded.
    /// Carries the property name and which backend proved it.
    Proved { property: String, by: &'static str },
    /// No violation found within the trace bound — *not* a proof
    /// (reporting discipline, `doc/model-check.md` §8).
    BoundedOk { property: String, depth: u32 },
    /// Safety violation found.
    Counterexample { property: String, cex: Counterexample },
    /// `<>φ`: no avoiding path found by the explicit backend
    /// (subject to Stateright's acyclic-path caveat, see `stateright_be`).
    EventuallyHolds { property: String },
    /// The backend cannot check this property kind.
    Unsupported { property: String, reason: String },
}

impl Verdict {
    pub fn property(&self) -> &str {
        match self {
            Verdict::Proved { property, .. }
            | Verdict::BoundedOk { property, .. }
            | Verdict::Counterexample { property, .. }
            | Verdict::EventuallyHolds { property }
            | Verdict::Unsupported { property, .. } => property,
        }
    }
}

fn render_state(spec: &McSpec, state: &State) -> String {
    let mut parts = Vec::new();
    for (t, table) in state.tables.iter().enumerate() {
        let name = spec
            .tables
            .get(t)
            .map(|d| d.name.as_str())
            .unwrap_or("<table>");
        let rows: Vec<String> = table.iter().map(|(k, v)| format!("{k}: {v}")).collect();
        parts.push(format!("{name} {{ {} }}", rows.join(", ")));
    }
    parts.join("  ")
}

impl Counterexample {
    pub fn render(&self, spec: &McSpec) -> String {
        let mut out = format!("counterexample for `{}`:\n", self.property);
        for (i, step) in self.steps.iter().enumerate() {
            out.push_str(&format!(
                "  [{i}] {:40} {}\n",
                step.label,
                render_state(spec, &step.state)
            ));
        }
        out
    }

    /// Render as a CQL `test`-block sketch (fixture from the initial state),
    /// for regression-replay workflows (`doc/model-check.md` §7).
    pub fn render_test_block(&self, spec: &McSpec) -> String {
        let mut out = format!("test counterexample_{} {{\n", self.property);
        if let Some(first) = self.steps.first() {
            for (t, table) in first.state.tables.iter().enumerate() {
                let name = spec
                    .tables
                    .get(t)
                    .map(|d| d.name.as_str())
                    .unwrap_or("<table>");
                let rows: Vec<String> = table
                    .iter()
                    .map(|(k, v)| format!("        record {{ id: {k}, value: {v} }}"))
                    .collect();
                out.push_str(&format!(
                    "    fixture {name} == [\n{}\n    ];\n",
                    rows.join(",\n")
                ));
            }
        }
        out.push_str("    -- TODO: replay action sequence and add expect clause\n}\n");
        out
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Proved { property, by } => write!(f, "PROVED({by}) {property}"),
            Verdict::BoundedOk { property, depth } => {
                write!(f, "BOUNDED-OK(k={depth}) {property}")
            }
            Verdict::Counterexample { property, .. } => write!(f, "COUNTEREXAMPLE {property}"),
            Verdict::EventuallyHolds { property } => write!(f, "EVENTUALLY-HOLDS {property}"),
            Verdict::Unsupported { property, reason } => {
                write!(f, "UNSUPPORTED {property}: {reason}")
            }
        }
    }
}
