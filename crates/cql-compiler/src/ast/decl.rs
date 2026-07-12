//! Top-level declarations: modules, items, and their supporting types.

use super::expr::Expr;
use super::span::{Ident, Span};
use super::temporal::TemporalExpr;
use super::ty::Type;

/// A CQL module — the unit of compilation.
#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub name: Ident,
    pub items: Vec<Item>,
    pub span: Span,
}

/// A top-level item in a module.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Use(UseDecl),
    Const(ConstDecl),
    TypeAlias(TypeAliasDecl),
    Enum(EnumDecl),
    Table(TableDecl),
    Index(IndexDecl),
    Operator(OperatorDecl),
    Invariant(InvariantDecl),
    Test(TestDecl),
    Property(PropertyDecl),
    Fairness(FairnessDecl),
}

/// `use a.b.c [as alias];`
#[derive(Debug, Clone, PartialEq)]
pub struct UseDecl {
    pub path: Vec<Ident>,
    pub alias: Option<Ident>,
}

/// Visibility of a declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
}

/// `const name: T = value;`
#[derive(Debug, Clone, PartialEq)]
pub struct ConstDecl {
    pub vis: Visibility,
    pub name: Ident,
    pub ty: Type,
    pub value: Expr,
}

/// `type Name<T...> = T;`
#[derive(Debug, Clone, PartialEq)]
pub struct TypeAliasDecl {
    pub vis: Visibility,
    pub name: Ident,
    pub params: Vec<Ident>,
    pub ty: Type,
}

/// `enum Name<T...> { V1, V2(T), V3 { a: T } }`
#[derive(Debug, Clone, PartialEq)]
pub struct EnumDecl {
    pub vis: Visibility,
    pub name: Ident,
    pub params: Vec<Ident>,
    pub variants: Vec<Variant>,
}

/// An enum variant.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub name: Ident,
    pub payload: VariantPayload,
}

/// The payload shape of an enum variant.
#[derive(Debug, Clone, PartialEq)]
pub enum VariantPayload {
    None,
    Tuple(Vec<Type>),
    Record(Vec<(Ident, Type)>),
}

/// `table Name { field: T, ... } primary key (pk...) [foreign key ...]`
#[derive(Debug, Clone, PartialEq)]
pub struct TableDecl {
    pub vis: Visibility,
    pub name: Ident,
    pub fields: Vec<(Ident, Type)>,
    pub pk: Vec<Ident>,
    pub fks: Vec<FkClause>,
}

/// `foreign key (cols...) references Table`
#[derive(Debug, Clone, PartialEq)]
pub struct FkClause {
    pub cols: Vec<Ident>,
    pub references: Ident,
}

/// `index Name on Table(cols...);`
#[derive(Debug, Clone, PartialEq)]
pub struct IndexDecl {
    pub vis: Visibility,
    pub name: Ident,
    pub table: Ident,
    pub cols: Vec<Ident>,
}

/// `function|query|action name<T...>(params) -> Ret [decreases x] [depth n] { body }`
///
/// `body = None` declares an external function (no implementation).
/// When present, `body` must be an `ExprKind::Block` expression.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorDecl {
    pub vis: Visibility,
    /// Effect level: L0 function / L1 query / L2 action.
    pub level: EffectLevel,
    pub recursive: bool,
    pub name: Ident,
    pub type_params: Vec<Ident>,
    pub params: Vec<Param>,
    pub ret: Type,
    /// Termination measure parameter (for recursive operators).
    pub decreases: Option<Ident>,
    /// Bounded recursion depth (for recursive operators).
    pub depth: Option<u64>,
    pub body: Option<Expr>,
}

/// The effect level of an operator: L0 (pure) ≤ L1 (read) ≤ L2 (write).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EffectLevel {
    /// L0: pure function — no table access.
    Function,
    /// L1: query — may read tables.
    Query,
    /// L2: action — may read and write tables.
    Action,
}

impl EffectLevel {
    /// Numeric rank for effect-level comparisons (L0=0, L1=1, L2=2).
    pub fn rank(self) -> u8 {
        match self {
            EffectLevel::Function => 0,
            EffectLevel::Query => 1,
            EffectLevel::Action => 2,
        }
    }
}

/// A named, typed operator parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: Ident,
    pub ty: Type,
}

/// `invariant Name(table): body` — a table invariant checked on writes.
#[derive(Debug, Clone, PartialEq)]
pub struct InvariantDecl {
    pub name: Ident,
    pub table: Ident,
    pub body: Expr,
}

/// `test Name { stmts }` — a unit test with fixtures and expectations.
#[derive(Debug, Clone, PartialEq)]
pub struct TestDecl {
    pub name: Ident,
    pub stmts: Vec<TestStmt>,
}

/// A statement inside a `test` block.
#[derive(Debug, Clone, PartialEq)]
pub enum TestStmt {
    /// `fixture Table = [row, ...]` — `rows` is a vector literal of records.
    Fixture { table: Ident, rows: Expr },
    /// `expect lhs == rhs`
    Expect { lhs: Expr, rhs: Expr },
}

/// `property Name: temporal_body` — a temporal property for model checking.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyDecl {
    pub name: Ident,
    pub body: TemporalExpr,
}

/// `fairness weak|strong actions...` — a fairness assumption.
#[derive(Debug, Clone, PartialEq)]
pub struct FairnessDecl {
    pub kind: FairnessKind,
    pub actions: Vec<Ident>,
}

/// Weak vs. strong fairness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FairnessKind {
    Weak,
    Strong,
}
