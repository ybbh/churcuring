#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    // Equality
    Eq,        // =
    Neq,       // != or #

    // Comparison
    Lt,
    Le,
    Gt,
    Ge,

    // Arithmetic
    Add,
    Sub,
    Mul,
    Div,

    // Logical (TLA-style)
    And,       // /\
    Or,        // \/
    Implies,  // =>
}
