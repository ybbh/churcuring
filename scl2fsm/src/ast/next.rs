use crate::ast::ty::Type;
// next.rs
use crate::ast::{condition::Condition, name::Name};

pub struct NextBlock {
    cases: Vec<NextCase>,
}

impl NextBlock {
    pub fn new(cases: Vec<NextCase>) -> Result<Self, String> {
        if cases.is_empty() {
            return Err("next block must have at least one case".into());
        }
        Ok(Self { cases })
    }

    pub fn cases(&self) -> &[NextCase] {
        &self.cases
    }
}

pub enum NextCase {
    When {
        condition: Condition,
        target: Name,
        exports: Vec<(Name, Type)>,
    },
    Otherwise {
        target: Name,
        exports: Vec<(Name, Type)>,
    },
}
