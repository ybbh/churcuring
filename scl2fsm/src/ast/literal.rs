// literal.rs

#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    Int(i64),
    Bool(bool),
    String(String),
    Float(f64),
    Null,
}
