#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuantifierKind {
    Exists,   // \E
    ForAll,   // \A
}
