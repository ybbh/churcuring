use crate::ast::expr::Expr;
use crate::ast::name::Name;

// stmt.rs (same file)
#[derive(Clone, Debug)]
pub struct Assignment {
    field: Name,
    value: Expr,
}

impl Assignment {
    pub fn new(field: Name, value: Expr) -> Self {
        Self { field, value }
    }

    pub fn field(&self) -> &Name {
        &self.field
    }

    pub fn value(&self) -> &Expr {
        &self.value
    }
}
