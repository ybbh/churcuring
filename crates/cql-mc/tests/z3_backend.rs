//! z3.rs (symbolic BMC + k-induction) backend end-to-end tests,
//! plus the differential contract against the Stateright backend.
#![cfg(feature = "z3")]

mod common;

use common::*;
use cql_mc::counterexample::Verdict;

#[test]
fn transfer_ok_is_proved_by_k_induction() {
    let verdicts = cql_mc::z3_be::check(&bank_spec(false));
    assert_eq!(verdicts.len(), 1);
    assert_eq!(
        verdicts[0],
        Verdict::Proved {
            property: "balance_conserved".to_string(),
            by: "z3-k-induction"
        }
    );
}

#[test]
fn transfer_buggy_finds_counterexample() {
    let verdicts = cql_mc::z3_be::check(&bank_spec(true));
    let Verdict::Counterexample { property, cex } = &verdicts[0] else {
        panic!("expected counterexample, got {:?}", verdicts[0]);
    };
    assert_eq!(property, "balance_conserved");
    // Violation reachable in one applied transfer from the fixture.
    assert_eq!(cex.steps.len(), 2);
    assert_eq!(cex.steps[0].label, "init");
    assert!(cex.steps[1].label.contains("transfer"));
    assert_ne!(total(&cex.steps[1].state), 10000);
}

/// Differential contract: both backends, same IR, same verdict class
/// and same key fact (conservation broken in the violating state).
#[test]
fn differential_buggy_both_backends() {
    let spec = bank_spec(true);
    let sr = cql_mc::stateright_be::check(&spec);
    let z3 = cql_mc::z3_be::check(&spec);

    let (Verdict::Counterexample { cex: sr_cex, .. }, Verdict::Counterexample { cex: z3_cex, .. }) =
        (&sr[0], &z3[0])
    else {
        panic!("both backends must refute; got {:?} / {:?}", sr[0], z3[0]);
    };
    assert_ne!(total(&sr_cex.steps.last().unwrap().state), 10000);
    assert_ne!(total(&z3_cex.steps.last().unwrap().state), 10000);
    // Same initial state, same violation depth.
    assert_eq!(
        sr_cex.steps.first().unwrap().state,
        z3_cex.steps.first().unwrap().state
    );
    assert_eq!(sr_cex.steps.len(), z3_cex.steps.len());
}

/// Differential contract on the clean spec: both backends prove.
#[test]
fn differential_ok_both_backends() {
    let spec = bank_spec(false);
    assert!(matches!(
        cql_mc::stateright_be::check(&spec)[0],
        Verdict::Proved { .. }
    ));
    assert!(matches!(
        cql_mc::z3_be::check(&spec)[0],
        Verdict::Proved { .. }
    ));
}

#[test]
fn liveness_is_unsupported_on_z3() {
    let verdicts = cql_mc::z3_be::check(&bank_spec_with_liveness());
    assert!(matches!(verdicts[1], Verdict::Unsupported { .. }));
}
