//! Source spans and spanned wrappers.

use std::fmt;

/// A byte-offset range `[start, end)` into the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    /// A span covering the smallest range that contains both `self` and `other`.
    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// A zero-length span that does not correspond to any real source location.
    pub fn new_dummy() -> Span {
        Span { start: 0, end: 0 }
    }

    /// Length of the span in bytes.
    pub fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

/// A value tagged with the source span it was produced from.
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
}

impl<T> Spanned<T> {
    pub fn new(node: T, span: Span) -> Self {
        Spanned { node, span }
    }
}

/// An identifier with its source location.
pub type Ident = Spanned<String>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_takes_outermost_range() {
        let a = Span { start: 10, end: 20 };
        let b = Span { start: 5, end: 8 };
        assert_eq!(a.merge(b), Span { start: 5, end: 20 });
        assert_eq!(b.merge(a), Span { start: 5, end: 20 });
        // Nested span.
        let c = Span { start: 12, end: 14 };
        assert_eq!(a.merge(c), a);
    }

    #[test]
    fn dummy_and_display() {
        let d = Span::new_dummy();
        assert!(d.is_empty());
        assert_eq!(d.len(), 0);
        assert_eq!(format!("{}", Span { start: 3, end: 7 }), "3..7");
        assert_eq!(Span { start: 3, end: 7 }.len(), 4);
    }
}
