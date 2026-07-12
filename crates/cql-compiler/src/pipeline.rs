//! End-to-end compiler pipeline (doc/cql.md §D.3).
//!
//! ```text
//! parse → name resolution → effect check → type check →
//! termination pass → desugar → optimize
//! ```
//!
//! [`compile_module`] runs the whole chain for a single source file;
//! [`compile_module_with_imports`] additionally supplies the public items of
//! already-compiled dependencies for `use` declarations. Multi-module
//! projects are driven by [`crate::project::compile_project`], which builds
//! the `use` dependency graph and compiles each module with the typed
//! public interfaces of its dependencies.

use crate::desugar::desugar_module;
use crate::diag::DiagBag;
use crate::lower;
use crate::optimize::{optimize_module, OptimizedModule};
use crate::resolve::ImportedModule;

/// Compile a standalone module source through desugaring and optimization.
///
/// Returns `Some(OptimizedModule)` when no errors were reported in any pass;
/// warnings are returned alongside the module.
pub fn compile_module(src: &str) -> (Option<OptimizedModule>, DiagBag) {
    compile_module_with_imports(src, &[])
}

/// Compile a module whose `use` declarations resolve against `imports`.
pub fn compile_module_with_imports(
    src: &str,
    imports: &[ImportedModule],
) -> (Option<OptimizedModule>, DiagBag) {
    let (typed, bag) = lower::frontend_with_imports(src, imports);
    match typed {
        Some(t) if !bag.has_errors() => (Some(optimize_module(desugar_module(t))), bag),
        _ => (None, bag),
    }
}
