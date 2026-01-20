// decl.rs
use crate::ast::{name::Name, ty::Type};

pub struct TypeDecl {
    name: Name,
    fields: Vec<(Name, Type)>,
}

impl TypeDecl {
    pub fn new(name: Name, fields: Vec<(Name, Type)>) -> Result<Self, String> {
        if fields.is_empty() {
            return Err("type must have at least one field".into());
        }
        Ok(Self { name, fields })
    }

    pub fn name(&self) -> &Name {
        &self.name
    }

    pub fn fields(&self) -> &[(Name, Type)] {
        &self.fields
    }
}
