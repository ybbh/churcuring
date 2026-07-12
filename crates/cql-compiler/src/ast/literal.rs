//! Literal values appearing in expressions and patterns.

/// A literal expression value.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// A calendar date literal (`d"2024-01-31"`).
    Date { year: i32, month: u8, day: u8 },
    /// A decimal literal.
    ///
    /// `repr` is the canonical decimal string as written in source; precision
    /// validation against `precision` (if given) is performed during lowering,
    /// not at parse time.
    Decimal {
        repr: String,
        precision: Option<(u32, u32)>,
    },
}
