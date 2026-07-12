//! Write constructors — the L2 (action-level) mutation primitives.

use super::expr::Expr;
use super::span::Ident;

/// A write constructor over a table.
///
/// Produced during name resolution / effect checking by rewriting `Call`
/// nodes that name a write primitive; the runtime executes these directly.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteCon {
    /// `insert(row)` — `row` is a record expression for the table's row type.
    Insert { table: Ident, row: Box<Expr> },
    /// `update(key, transform)` — `transform` is a lambda from row to row.
    Update {
        table: Ident,
        key: Box<Expr>,
        transform: Box<Expr>,
    },
    /// `delete(key)`
    Delete { table: Ident, key: Box<Expr> },
}
