// state.rs
use crate::ast::{name::Name, ty::Type};

pub enum UseStmt {
    State {
        source: QualifiedName,
        fields: Vec<(Name, Type)>,
    },
    Context {
        context: QualifiedName,
    },
    Type {
        ty: QualifiedName,
    },
}


// name.rs
pub struct QualifiedName {
    path: Option<String>,
    name: Name,
}

impl QualifiedName {
    pub fn local(name: Name) -> Self {
        Self { path: None, name }
    }

    pub fn with_path(path: String, name: Name) -> Self {
        Self { path: Some(path), name }
    }

    pub fn name(&self) -> &Name {
        &self.name
    }

    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }
}
