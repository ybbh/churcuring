// compare.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareOp {
    Eq,     // =
    Neq,    // != or #
    Lt,     // <
    Le,     // <=
    Gt,     // >
    Ge,     // >=
}
