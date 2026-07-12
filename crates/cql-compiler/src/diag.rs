//! Diagnostics: structured errors/warnings with source highlighting,
//! built on [`miette`].
//!
//! Every compiler pass reports problems through a [`DiagBag`]: it either
//! returns `Result<T, DiagBag>` (fatal, abort the pass) or
//! `(T, DiagBag)` (recoverable, continue and report at the end). Both
//! styles convert into each other via [`DiagBag::into_result`] and
//! [`DiagBag::from_result`].

use miette::{Diagnostic, GraphicalReportHandler, GraphicalTheme, NamedSource, SourceSpan};
use thiserror::Error;

use crate::ast::Span;

/// A single diagnostic (error or warning) with a primary labelled span.
#[derive(Debug, Clone, Error, Diagnostic)]
#[error("{message}")]
pub struct CqlError {
    /// The source this diagnostic refers to.
    #[source_code]
    pub src: NamedSource<String>,
    /// The offending source range.
    #[label("{message}")]
    pub span: SourceSpan,
    /// Human-readable description of the problem.
    pub message: String,
    /// Optional suggestion for fixing the problem.
    #[help]
    pub help: Option<String>,
}

impl CqlError {
    /// Build a diagnostic from a named source and a byte-offset span.
    pub fn new(
        src: NamedSource<String>,
        span: Span,
        message: impl Into<String>,
        help: Option<String>,
    ) -> Self {
        CqlError {
            src,
            span: to_source_span(span),
            message: message.into(),
            help,
        }
    }

    /// Convenience constructor using a `SourceSpan` directly.
    pub fn with_source_span(
        src: NamedSource<String>,
        span: SourceSpan,
        message: impl Into<String>,
        help: Option<String>,
    ) -> Self {
        CqlError {
            src,
            span,
            message: message.into(),
            help,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn help(&self) -> Option<&str> {
        self.help.as_deref()
    }

    /// Render a graphical (source-highlighting, no-color) diagnostic text.
    pub fn render(&self) -> String {
        let mut out = String::new();
        GraphicalReportHandler::new()
            .with_theme(GraphicalTheme::none())
            .render_report(&mut out, self)
            .expect("rendering a diagnostic into a String cannot fail");
        out
    }
}

/// Convert a byte-offset [`Span`] into a miette [`SourceSpan`].
pub fn to_source_span(span: Span) -> SourceSpan {
    SourceSpan::new((span.start as usize).into(), span.len() as usize)
}

/// A bag of diagnostics accumulated during a compiler pass.
#[derive(Debug, Clone, Default)]
pub struct DiagBag {
    errors: Vec<CqlError>,
    warnings: Vec<CqlError>,
}

impl DiagBag {
    pub fn new() -> Self {
        DiagBag::default()
    }

    pub fn push_error(&mut self, err: CqlError) {
        self.errors.push(err);
    }

    pub fn push_warning(&mut self, warn: CqlError) {
        self.warnings.push(warn);
    }

    /// Append all diagnostics from `other` into `self`.
    pub fn merge(&mut self, other: DiagBag) {
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.errors.is_empty() && self.warnings.is_empty()
    }

    pub fn errors(&self) -> &[CqlError] {
        &self.errors
    }

    pub fn warnings(&self) -> &[CqlError] {
        &self.warnings
    }

    pub fn error_count(&self) -> usize {
        self.errors.len()
    }

    pub fn warning_count(&self) -> usize {
        self.warnings.len()
    }

    /// Turn an accumulated bag into a `Result`: `Ok(value)` when no errors
    /// were recorded, `Err(self)` otherwise.
    pub fn into_result<T>(self, value: T) -> Result<T, DiagBag> {
        if self.has_errors() {
            Err(self)
        } else {
            Ok(value)
        }
    }

    /// Split a pass result back into `(value, warnings-only bag)` on success.
    pub fn from_result<T>(result: Result<T, DiagBag>) -> Result<(T, DiagBag), DiagBag> {
        result.map(|value| (value, DiagBag::new()))
    }

    /// Render all errors and warnings into a single report string.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for err in &self.errors {
            out.push_str(&err.render());
        }
        for warn in &self.warnings {
            out.push_str(&warn.render());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_src() -> NamedSource<String> {
        NamedSource::new(
            "sample.cql",
            "function add(a: Int, b: Int) -> Int {\n    a + b\n}\n".to_string(),
        )
    }

    #[test]
    fn span_to_source_span_conversion() {
        let ss = to_source_span(Span { start: 9, end: 12 });
        assert_eq!(ss.offset(), 9);
        assert_eq!(ss.len(), 3);
    }

    #[test]
    fn render_contains_source_highlight() {
        let src_text = "function add(a: Int, b: Int) -> Int {\n    a + b\n}\n";
        // Highlight the `b` in `a + b` on line 2: byte offset of "    a + " is 39+8.
        let line2_start = src_text.find('\n').unwrap() + 1;
        let b_off = line2_start + "    a + ".len();
        let err = CqlError::new(
            sample_src(),
            Span {
                start: b_off as u32,
                end: b_off as u32 + 1,
            },
            "unknown variable `b`",
            Some("did you mean `a`?".to_string()),
        );
        let rendered = err.render();
        assert!(rendered.contains("unknown variable `b`"), "{rendered}");
        assert!(rendered.contains("sample.cql"), "{rendered}");
        assert!(rendered.contains("a + b"), "{rendered}");
        // miette renders the labelled underline with a `-- marker.
        assert!(rendered.contains("`-- unknown variable `b`"), "{rendered}");
        assert!(rendered.contains("did you mean `a`?"), "{rendered}");
    }

    #[test]
    fn diag_bag_basics() {
        let mut bag = DiagBag::new();
        assert!(!bag.has_errors());
        assert!(bag.is_empty());
        let dummy = Span { start: 0, end: 1 };
        bag.push_warning(CqlError::new(sample_src(), dummy, "w1", None));
        assert!(!bag.has_errors());
        let value = bag.clone().into_result(42).unwrap();
        assert_eq!(value, 42);
        bag.push_error(CqlError::new(sample_src(), dummy, "e1", None));
        assert!(bag.has_errors());
        assert!(bag.clone().into_result(()).is_err());

        let mut other = DiagBag::new();
        other.push_error(CqlError::new(sample_src(), dummy, "e2", None));
        bag.merge(other);
        assert_eq!(bag.error_count(), 2);
        assert_eq!(bag.warning_count(), 1);
    }
}
