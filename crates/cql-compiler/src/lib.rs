//! cql-compiler: the CQL compiler library.
//!
//! # Pipeline (doc/cql.md §D.3)
//!
//! ```text
//! parse → name resolution → effect check → type check →
//! termination pass → desugar → optimize → codegen
//! ```
//!
//! Every pass consumes and produces an immutable [`ast::Module`] / [`ast::Expr`]
//! tree. The single `Expr` type carries surface-only nodes (eliminated by
//! desugaring), core nodes, and resolved nodes (produced by name resolution
//! and effect checking); see [`ast::ExprKind`].
//!
//! Diagnostics flow through [`diag::DiagBag`]: passes return either
//! `Result<T, DiagBag>` or `(T, DiagBag)`.
//!
//! # Status
//!
//! - `lower` — tree-sitter based parser producing the surface AST
//!   (`parse_module`), plus `frontend`/`frontend_with_imports` running
//!   resolve → effect → types → terminate with error isolation. ✅
//! - `resolve` — name resolution; rewrites `Call` into resolved nodes. ✅
//! - `effect` — effect-level (L0/L1/L2) checking. ✅
//! - `terminate` — termination pass (`decreases` / `depth`). ✅
//! - `types` — type checking and `MethodCall` dispatch. ✅
//! - `desugar` — surface → core lowering (doc/cql.md §D.2). ✅
//! - `optimize` — read-plan classification (§5.5) into a `ReadPlan` side
//!   table; the desugared AST is not rewritten. ✅
//! - `pipeline` — `compile_module` / `compile_module_with_imports` running
//!   the whole chain. ✅
//! - `project` — multi-module projects: `use`-graph topological
//!   compilation with typed public interfaces (`ModuleInterface`),
//!   cross-module type checking and CIR lowering. ✅
//! - `cir` — portable Codegen IR (lambda lifting, pattern compilation,
//!   monomorphization, read-plan materialization; cross-module references
//!   lower to `crate::<module>::<item>` paths). ✅
//! - `codegen` — askama templates → Rust/cql-runtime code. ✅
//! - `mc_lower` — model-checking lowering: desugared AST → `cql-mc` `McSpec`
//!   (v1 fragment: bool/int expressions, int-keyed tables with a single int
//!   value field; actions → guarded transitions, invariants/`[]` → `Always`,
//!   `<>` → `Eventually`, folds → `Sum`/expanded quantifiers; unsupported
//!   constructs produce diagnostics, `doc/model-check.md` §4). ✅

pub mod ast;
pub mod cir;
pub mod codegen;
// The `miette` derive on `diag::CqlError` trips a nightly-rustc
// `unused_assignments` false positive on the diagnostic's fields (reads happen
// in derive-generated trait impls); allow it for this module only.
#[allow(unused_assignments)]
pub mod diag;
pub mod desugar;
pub mod effect;
pub mod lower;
pub mod mc_lower;
pub mod mududb_be;
pub mod optimize;
pub mod pipeline;
pub mod project;
pub mod resolve;
pub mod terminate;
pub mod types;
