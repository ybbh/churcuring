//! Expressions.
//!
//! A single `Expr`/`ExprKind` type carries the whole pipeline: surface-only
//! nodes (produced by the parser, eliminated by desugaring), core nodes
//! (produced by desugaring), and resolved nodes (produced by name resolution
//! and effect checking by rewriting `Call` nodes). Each variant is annotated
//! with the stage(s) it lives in.

use super::literal::Literal;
use super::pattern::Pattern;
use super::span::{Ident, Span};
use super::ty::Type;
use super::write_con::WriteCon;

/// An expression with its source location.
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

impl Expr {
    pub fn new(kind: ExprKind, span: Span) -> Self {
        Expr { kind, span }
    }
}

/// The kind of an [`Expr`] node; each variant is annotated with the pipeline
/// stage(s) it lives in (see the module docs).
#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    /// Literal value. (surface + core)
    Lit(Literal),
    /// Variable reference. (surface + core)
    Var(Ident),

    /// `{ let ...; let ...; tail }` — a block of let statements followed by a
    /// tail expression. (surface-only; desugared into nested `Let`)
    Block { lets: Vec<LetStmt>, tail: Box<Expr> },
    /// Nested let binding. (core-only; produced by desugaring `Block`)
    Let {
        pat: Pattern,
        value: Box<Expr>,
        body: Box<Expr>,
    },

    /// Lambda expression. (surface + core)
    Lambda(Lambda),
    /// Application of an arbitrary function value. (surface + core)
    App { func: Box<Expr>, args: Vec<Arg> },
    /// A named call `f::<T>(args)`, possibly with named arguments.
    /// (surface; rewritten to a resolved node or kept by name resolution)
    Call(Call),

    /// `match scrutinee { arms }` (surface + core)
    Match { scrutinee: Box<Expr>, arms: Vec<MatchArm> },
    /// `if cond { then } else { else }` (surface + core)
    If {
        cond: Box<Expr>,
        then_br: Box<Expr>,
        else_br: Box<Expr>,
    },

    /// `e?` — the try operator. (surface-only; desugared into `Match`)
    Try(Box<Expr>),

    /// `{ a: 1, b: 2 }` — record literal. (surface + core)
    RecordLit { fields: Vec<FieldInit> },
    /// `{ base with a: 1 }` — record update. (surface + core)
    RecordUpd { base: Box<Expr>, fields: Vec<FieldInit> },
    /// `(a, b, c)` (surface + core)
    Tuple(Vec<Expr>),

    /// `[1, 2, 3]` — vector literal. (surface + core)
    Vector(Vec<Expr>),
    /// `{1, 2, 3}` — set literal. (surface + core)
    SetLiteral(Vec<Expr>),
    /// `{ pat <- source | pred }` — set comprehension by filter. (surface + core)
    SetFilter {
        pat: Pattern,
        source: Box<Expr>,
        pred: Box<Expr>,
    },
    /// `{ elem | gens }` — set comprehension by mapping. (surface + core)
    SetMap { elem: Box<Expr>, gens: Vec<Generator> },
    /// `bag {1, 2}` — bag literal. (surface + core)
    BagLiteral(Vec<Expr>),
    /// Bag comprehension by mapping. (surface + core)
    BagMap { elem: Box<Expr>, gens: Vec<Generator> },
    /// `map {k: v, ...}` — map literal. (surface + core)
    MapLit(Vec<(Expr, Expr)>),
    /// `Some(e)` (surface + core)
    OptionSome(Box<Expr>),
    /// `None` (surface + core)
    OptionNone,

    /// `"a \(e) b"` — string interpolation. (surface-only; desugared into
    /// concatenation / `str` calls)
    StrInterp(Vec<StrPart>),

    /// `forall gens: body` / `exists gens: body` (surface + core)
    Quantifier {
        kind: QuantKind,
        gens: Vec<Generator>,
        body: Box<Expr>,
    },

    /// `e as T` — explicit cast. (surface + core)
    Cast { expr: Box<Expr>, ty: Type },

    /// Binary operator. (surface + core)
    BinOp {
        op: BinOpKind,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// Unary operator. (surface + core)
    UnOp { op: UnOpKind, operand: Box<Expr> },

    /// `base.name` — record/struct field access. (surface + core)
    Field { base: Box<Expr>, name: Ident },
    /// `base.0` — tuple projection. (surface + core)
    TupleProj { base: Box<Expr>, index: u32 },
    /// `recv.name(args)` — method-call sugar; dispatched during type checking.
    /// (surface-only)
    MethodCall {
        recv: Box<Expr>,
        name: Ident,
        args: Vec<Arg>,
    },

    /// `e'` — next-state (prime) reference; only produced by lowering inside
    /// `property` bodies (doc/model-check.md §4.1). Transparent for the
    /// semantic passes: it types and resolves as its operand. (surface + core)
    Primed(Box<Expr>),

    // ---- resolved nodes: produced by name resolution / effect checking
    // by rewriting `Call` nodes that name primitives. ----
    /// A table read primitive `read<Table>(predicate)`. (resolved)
    ReadPrim { table: Ident, predicate: Box<Expr> },
    /// A write constructor (`insert` / `update` / `delete`). (resolved)
    WriteCon(WriteCon),
    /// Enum variant construction `Name(args)`. (resolved)
    EnumConstruct { name: Ident, args: Vec<Expr> },
}

/// A `let` statement inside a block. (surface-only)
#[derive(Debug, Clone, PartialEq)]
pub struct LetStmt {
    pub pat: Pattern,
    pub ty: Option<Type>,
    pub value: Expr,
}

/// A lambda expression: `\captures |params| -> Ret { body }`.
#[derive(Debug, Clone, PartialEq)]
pub struct Lambda {
    /// Explicitly captured identifiers (may be empty).
    pub captures: Vec<Ident>,
    pub params: Vec<LambdaParam>,
    pub ret: Option<Type>,
    pub body: Box<Expr>,
}

/// A single lambda parameter with an optional type annotation.
#[derive(Debug, Clone, PartialEq)]
pub struct LambdaParam {
    pub pat: Pattern,
    pub ty: Option<Type>,
}

/// A call argument, optionally named (`name = value`).
#[derive(Debug, Clone, PartialEq)]
pub struct Arg {
    pub name: Option<Ident>,
    pub value: Expr,
}

/// A `match` arm.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pat: Pattern,
    pub body: Expr,
}

/// A record literal / update field initializer.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldInit {
    pub name: Ident,
    pub value: Expr,
}

/// A generator `pat <- source` in comprehensions and quantifiers.
#[derive(Debug, Clone, PartialEq)]
pub struct Generator {
    pub pat: Pattern,
    pub source: Expr,
}

/// Quantifier kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantKind {
    Forall,
    Exists,
}

/// A part of an interpolated string literal. (surface-only)
#[derive(Debug, Clone, PartialEq)]
pub enum StrPart {
    Lit(String),
    Interp(Expr),
}

/// Binary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOpKind {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
    /// `==>` — implication.
    Impl,
    /// `\in`
    In,
    /// `\subseteq`
    SubsetEq,
    /// `\cup`
    Cup,
    /// `\cap`
    Cap,
    /// `\` — set difference.
    Diff,
    /// `\X` — cartesian product.
    Cartesian,
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOpKind {
    Not,
    Neg,
}

/// A named call `name::<type_args>(args)`. (surface)
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub name: Ident,
    pub type_args: Option<Vec<Type>>,
    pub args: Vec<Arg>,
}
