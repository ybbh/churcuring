//! Surface type syntax.

use super::span::{Ident, Span};

/// A type annotation with its source location.
#[derive(Debug, Clone, PartialEq)]
pub struct Type {
    pub kind: TypeKind,
    pub span: Span,
}

/// The kind of a surface type annotation (before name resolution).
#[derive(Debug, Clone, PartialEq)]
pub enum TypeKind {
    Bool,
    Int,
    Float,
    /// `Decimal(p, s)` — `(precision, scale)`; `None` for the bare `Decimal`.
    Decimal(Option<(u32, u32)>),
    String,
    Date,
    /// A named type: type alias, enum, table row type, or `write_op`.
    Named { name: Ident, args: Vec<Type> },
    /// `Key<Table>` — a table's key type.
    Key(Ident),
    /// `Value<Table>` — a table's row type.
    Value(Ident),
    Option(Box<Type>),
    Vector(Box<Type>),
    Set(Box<Type>),
    Bag(Box<Type>),
    Map(Box<Type>, Box<Type>),
    /// `Table<K, V>`
    Table(Box<Type>, Box<Type>),
    Tuple(Vec<Type>),
    Fun(Box<Type>, Box<Type>),
    Record(Vec<(Ident, Type)>),
}

impl Type {
    pub fn new(kind: TypeKind, span: Span) -> Self {
        Type { kind, span }
    }
}
