use crate::ast::binary_op::BinaryOp;
use crate::ast::quantifier::QuantifierKind;
use crate::ast::unary_op::UnaryOp;
// expr.rs
use crate::ast::{
    literal::Literal,
    name::Name,
};

#[derive(Clone, Debug)]
pub enum Expr {
    /* ========= Atomic ========= */

    Literal(Literal),

    /// Variable reference (context field, let binding, use-imported name, etc.)
    Var(Name),

    /// Field access: a.b
    Field {
        base: Box<Expr>,
        field: Name,
    },

    /* ========= Arithmetic / Comparison / Logic ========= */

    Binary {
        lhs: Box<Expr>,
        op: BinaryOp,
        rhs: Box<Expr>,
    },

    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },

    /* ========= Quantifier (TLA+ subset) ========= */

    Quantifier {
        kind: QuantifierKind,
        relation: Name,
        var: Name,
        pk_binding: Box<Expr>,
        body: Box<Expr>,
    },
}
