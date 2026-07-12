//! Patterns used in `let` bindings, match arms, lambdas and generators.

use super::span::{Ident, Span};

/// A pattern with its source location.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub kind: PatternKind,
    pub span: Span,
}

/// The kind of a [`Pattern`] node (wildcard, binding, literal, variant, ...).
#[derive(Debug, Clone, PartialEq)]
pub enum PatternKind {
    /// `_`
    Wildcard,
    /// `x` — binds the matched value.
    Bind(Ident),
    /// A literal pattern: `1`, `"x"`, `true`.
    Lit(PatLit),
    /// `None`
    None,
    /// `Some(p)`
    Some(Box<Pattern>),
    /// `Variant(p1, p2, ...)` — an enum variant pattern.
    Variant { name: Ident, args: Vec<Pattern> },
    /// `(p1, p2, ...)`
    Tuple(Vec<Pattern>),
    /// `{ a, b, c }` — record destructuring (shorthand for field puns).
    Record(Vec<Ident>),
    /// `[]` — the empty list/cons-nil pattern.
    ConsNil,
    /// `head :: tail`
    Cons {
        head: Box<Pattern>,
        tail: Box<Pattern>,
    },
}

/// A literal inside a pattern (a subset of expression literals).
#[derive(Debug, Clone, PartialEq)]
pub enum PatLit {
    Int(i64),
    Str(String),
    Bool(bool),
}

impl Pattern {
    pub fn new(kind: PatternKind, span: Span) -> Self {
        Pattern { kind, span }
    }

    /// All identifiers bound by this pattern, in source order.
    pub fn bound_idents(&self) -> Vec<&Ident> {
        let mut out = Vec::new();
        self.collect_bound(&mut out);
        out
    }

    fn collect_bound<'a>(&'a self, out: &mut Vec<&'a Ident>) {
        match &self.kind {
            PatternKind::Wildcard | PatternKind::Lit(_) | PatternKind::None | PatternKind::ConsNil => {}
            PatternKind::Bind(id) => out.push(id),
            PatternKind::Some(inner) => inner.collect_bound(out),
            PatternKind::Variant { args, .. } => {
                for p in args {
                    p.collect_bound(out);
                }
            }
            PatternKind::Tuple(pats) => {
                for p in pats {
                    p.collect_bound(out);
                }
            }
            PatternKind::Record(ids) => {
                for id in ids {
                    out.push(id);
                }
            }
            PatternKind::Cons { head, tail } => {
                head.collect_bound(out);
                tail.collect_bound(out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Spanned;

    fn ident(name: &str) -> Ident {
        Spanned::new(name.to_string(), Span::new_dummy())
    }

    fn pat(kind: PatternKind) -> Pattern {
        Pattern::new(kind, Span::new_dummy())
    }

    fn names(p: &Pattern) -> Vec<&str> {
        p.bound_idents().iter().map(|i| i.node.as_str()).collect()
    }

    #[test]
    fn bound_idents_collects_all_bindings_in_order() {
        // `(Some(x), {a, b}, h :: t)` — nested binds.
        let p = pat(PatternKind::Tuple(vec![
            pat(PatternKind::Some(Box::new(pat(PatternKind::Bind(ident("x")))))),
            pat(PatternKind::Record(vec![ident("a"), ident("b")])),
            pat(PatternKind::Cons {
                head: Box::new(pat(PatternKind::Bind(ident("h")))),
                tail: Box::new(pat(PatternKind::Bind(ident("t")))),
            }),
        ]));
        assert_eq!(names(&p), vec!["x", "a", "b", "h", "t"]);
    }

    #[test]
    fn bound_idents_ignores_non_bindings() {
        let p = pat(PatternKind::Variant {
            name: ident("Ok"),
            args: vec![
                pat(PatternKind::Wildcard),
                pat(PatternKind::Lit(PatLit::Int(1))),
                pat(PatternKind::None),
                pat(PatternKind::ConsNil),
                pat(PatternKind::Bind(ident("y"))),
            ],
        });
        assert_eq!(names(&p), vec!["y"]);
    }
}
