// program.rs

use crate::ast::context_decl::ContextDecl;
use crate::ast::state_decl::StateDecl;
use crate::ast::type_decl::TypeDecl;

pub struct Program {
    types: Vec<TypeDecl>,
    contexts: Vec<ContextDecl>,
    states: Vec<StateDecl>,
}

impl Program {
    pub fn new(
        types: Vec<TypeDecl>,
        contexts: Vec<ContextDecl>,
        states: Vec<StateDecl>,
    ) -> Self {
        Self { types, contexts, states }
    }

    pub fn types(&self) -> &[TypeDecl] {
        &self.types
    }

    pub fn contexts(&self) -> &[ContextDecl] {
        &self.contexts
    }

    pub fn states(&self) -> &[StateDecl] {
        &self.states
    }
}
