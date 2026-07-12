//! Stateright (explicit-state) backend end-to-end tests.

mod common;

use common::*;
use cql_mc::counterexample::Verdict;

#[test]
fn transfer_ok_is_proved_exhaustively() {
    let verdicts = cql_mc::stateright_be::check(&bank_spec(false));
    assert_eq!(verdicts.len(), 1);
    assert_eq!(
        verdicts[0],
        Verdict::Proved {
            property: "balance_conserved".to_string(),
            by: "stateright-exhaustive"
        }
    );
}

#[test]
fn transfer_buggy_finds_counterexample() {
    let verdicts = cql_mc::stateright_be::check(&bank_spec(true));
    let Verdict::Counterexample { property, cex } = &verdicts[0] else {
        panic!("expected counterexample, got {:?}", verdicts[0]);
    };
    assert_eq!(property, "balance_conserved");
    // BFS ⇒ shortest violation: init + one applied transfer.
    assert_eq!(cex.steps.len(), 2);
    assert_eq!(cex.steps[0].label, "init");
    assert!(cex.steps[1].label.contains("transfer"));
    assert!(cex.steps[1].label.contains("Applied"));
    assert_ne!(total(&cex.steps[1].state), 10000);
    // Rendering works.
    let text = cex.render(&bank_spec(true));
    assert!(text.contains("counterexample for `balance_conserved`"));
}

#[test]
fn eventually_holds_when_true_at_init() {
    let verdicts = cql_mc::stateright_be::check(&bank_spec_with_liveness());
    assert_eq!(
        verdicts[1],
        Verdict::EventuallyHolds {
            property: "account_1_can_be_6000".to_string()
        }
    );
}
