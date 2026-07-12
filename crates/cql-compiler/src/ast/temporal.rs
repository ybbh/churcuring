//! Temporal expressions used in `property` declarations (model checking).

use super::expr::Expr;

/// A temporal-logic formula over state predicates.
#[derive(Debug, Clone, PartialEq)]
pub enum TemporalExpr {
    /// `always F` (`[]F`)
    Always(Box<TemporalExpr>),
    /// `eventually F` (`<>F`)
    Eventually(Box<TemporalExpr>),
    /// `F leads-to G` (`F ~> G`)
    LeadsTo {
        lhs: Box<TemporalExpr>,
        rhs: Box<TemporalExpr>,
    },
    /// `F until G`
    Until {
        lhs: Box<TemporalExpr>,
        rhs: Box<TemporalExpr>,
    },
    /// A primed (next-state) expression, e.g. `count' > 0`.
    Primed(Expr),
    /// A plain state predicate.
    State(Expr),
}
