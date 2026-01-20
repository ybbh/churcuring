use crate::ast::stmt::Stmt;
use crate::ast::use_stmt::UseStmt;
// state.rs
use crate::ast::{
    name::Name,
    next::NextBlock,
};

pub struct StateDecl {
    name: Name,
    uses: Vec<UseStmt>,
    body: Vec<Stmt>,
    next: NextBlock,
}

impl StateDecl {
    pub fn new(
        name: Name,
        uses: Vec<UseStmt>,
        body: Vec<Stmt>,
        next: NextBlock,
    ) -> Self {
        Self { name, uses, body, next }
    }

    pub fn name(&self) -> &Name {
        &self.name
    }

    pub fn uses(&self) -> &[UseStmt] {
        &self.uses
    }

    pub fn next(&self) -> &NextBlock {
        &self.next
    }
}
