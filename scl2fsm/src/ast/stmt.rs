// stmt.rs

use crate::ast::assignment::Assignment;
use crate::ast::condition::Condition;
use crate::ast::expr::Expr;
use crate::ast::name::Name;
use crate::ast::ty::Type;

#[derive(Clone, Debug)]
pub enum Stmt {
    /* ========= Local Binding ========= */

    /// let x: T = expr;
    Let {
        name: Name,
        ty: Type,
        value: Expr,
    },

    /* ========= Relation Read ========= */

    /// select x: T from Relation where cond limit n;
    Select {
        name: Name,
        ty: Type,
        relation: Name,
        where_clause: Option<Condition>,
        limit: Option<u64>,
    },

    /* ========= Relation Write ========= */

    /// update Relation set a = expr, b = expr where cond;
    Update {
        relation: Name,
        assignments: Vec<Assignment>,
        where_clause: Option<Condition>,
    },

    /// insert into Relation (a, b, c) values (e1, e2, e3);
    Insert {
        relation: Name,
        columns: Vec<Name>,
        values: Vec<Expr>,
    },

    /// delete from Relation where cond;
    Delete {
        relation: Name,
        where_clause: Option<Condition>,
    },

    /* ========= Assertion ========= */

    /// assert condition;
    Assert {
        condition: Condition,
    },

    /* ========= Transaction ========= */

    /// commit;
    Commit,
}