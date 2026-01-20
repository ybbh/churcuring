use crate::ast::name::Name;
use crate::ast::ty::Type;

pub struct ContextDecl {
    name: Name,
    fields: Vec<(Name, Type)>,
}

impl ContextDecl {
    pub fn new(name: Name, fields: Vec<(Name, Type)>) -> Result<Self, String> {
        Ok(Self { name, fields })
    }

    pub fn name(&self) -> &Name {
        &self.name
    }

    pub fn fields(&self) -> &[(Name, Type)] {
        &self.fields
    }
}
