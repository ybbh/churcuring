// ty.rs
use crate::ast::name::Name;

#[derive(Clone, Debug)]
pub enum Type {
    Primitive(PrimitiveType),
    Named(Name),
    Generic {
        base: Name,
        param: Box<Type>,
    },
}

#[derive(Clone, Debug)]
pub enum PrimitiveType {
    Int,
    Bool,
    String,
    Float,
}
