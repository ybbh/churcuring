use crate::ast::compare_op::CompareOp;
use crate::ast::expr::Expr;
use crate::ast::name::Name;
use crate::ast::quantifier::QuantifierKind;

#[derive(Clone, Debug)]
pub enum Condition {
    /* ========= Logical ========= */

    And(Box<Condition>, Box<Condition>),
    Or(Box<Condition>, Box<Condition>),
    Implies(Box<Condition>, Box<Condition>),
    Not(Box<Condition>),

    /* ========= Relational ========= */

    Compare {
        lhs: Expr,
        op: CompareOp,
        rhs: Expr,
    },

    /* ========= Quantifier (Relation-only) ========= */

    Quantifier {
        kind: QuantifierKind,
        relation: Name,
        var: Name,
        pk_binding: Expr,
        body: Box<Condition>,
    },
}