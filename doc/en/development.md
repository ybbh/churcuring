# Development Guide

A contributor-oriented code tour: workspace structure, compilation pipeline, design
invariants you must know, recipes for common extension points, and testing conventions.
Language semantics are governed by `doc/cql.md`, the codegen architecture by
`doc/codegen-backend.md`; this document only points directions, it does not restate the
spec.

## 1. Workspace structure

| Crate | Responsibility | Key modules/files |
| --- | --- | --- |
| `crates/tree-sitter-cql` | tree-sitter grammar + Rust binding | `grammar.js` (grammar), `src/parser.c` (generated), `test/corpus/*.txt` (43 corpus cases) |
| `crates/cql-compiler` | Compiler library (frontend → middle → codegen) | `ast/` (one file per AST struct, `mod.rs` re-exports), `lower.rs` (CST→AST), `resolve.rs`, `effect.rs`, `types.rs`, `terminate.rs`, `desugar.rs`, `optimize.rs` (read-plan classification), `pipeline.rs` (single-module pipeline), `project.rs` (multi-module project), `cir.rs` (CIR lowering), `codegen.rs` (Backend trait + Rust backend), `mududb_be.rs` (mududb placeholder backend), `mc_lower.rs` (desugared AST → McSpec), `diag.rs` (miette diagnostics) |
| `crates/cql-runtime` | Runtime depended on by generated code | `value.rs` (Date/Decimal/CanonOrd/Value), `collections.rs` (CqlSet/CqlBag/CqlMap), `table.rs` (Table trait, MemTable), `write.rs` (WriteOp, TableRegistry, atomic apply), `trap.rs` (Trap, checked arithmetic), `stdlib/` (appendix-B pure functions: string/math/decimal/date/vector/set_bag/map/option/aggregate) |
| `crates/cql-mc` | Model checker (checker-agnostic IR + dual backends) | `ir.rs` (McSpec), `eval.rs` (concrete evaluator), `encode.rs`/`z3_be.rs` (z3 backend, feature `z3`), `stateright_be.rs` (explicit-state backend, feature `stateright`), `counterexample.rs` (unified Verdict/Counterexample format) |
| `crates/cql-cli` | `cqlc` binary (clap subcommands) | `main.rs` (new/check/build/test/verify/clean), `manifest.rs` (cql.toml/verify.toml parsing), `project.rs` (target resolution, codegen emission, cargo invocation) |
| `editors/vscode-cql` | VSCode highlighting plugin (TypeScript) | `src/extension.ts` (semantic tokens), `queries/highlights.scm`, `language-configuration.json` |

## 2. Compilation pipeline

```text
parse (tree-sitter → lower.rs)
  → resolve      name resolution, visibility, use graph, lambda-capture checking
  → effect       L0/L1/L2 effect-tier checking (doc/cql.md §3.7)
  → types        bidirectional local inference, table-derived types, as whitelist, match exhaustiveness
  → terminate    structural-recursion subterm check, SCC rejects mutual recursion
  → desugar      surface syntax → ~12-node core language (doc/cql.md appendix D.2)
  → optimize     read-predicate plan classification (point lookup/index scan/full scan), written into the ReadPlan side table
  → CIR (cir.rs) lambda lifting, pattern compilation, monomorphization, read-plan materialization
  → Backend trait (codegen.rs)
        ├─ RustBackend     askama templates + recursive emitter → Rust source (depends on cql-runtime)
        └─ MududbBackend   CIR → deployment-plan text (PROPOSAL status, doc/backend-mududb.md)

Model-checking branch (cqlc verify):
  desugared AST → mc_lower.rs → cql-mc McSpec → stateright_be (explicit-state enumeration)
```

Single-module entry: `pipeline::compile_module`; multi-module project entry:
`project::compile_project` (compiles in topological order of the `use` dependency graph,
each module carrying the `ModuleInterface` of its dependencies).

## 3. Key design invariants (read before changing code)

1. **Single Expr/ExprKind**: all passes share one `ast::Expr`; variants fall into three
   groups — surface nodes (eliminated by desugaring), core nodes, and resolved nodes
   (produced by resolve/effect, e.g. resolved call).
2. **Side tables keyed by Span**: name-resolution results, expression types
   (`expr_tys`), generic instantiations, and read plans (`ReadPlan`) all live in side
   tables indexed by `Span`, never written back into the AST. New passes should follow
   this pattern for their metadata.
3. **Diagnostics through DiagBag, operator-level error isolation**: a pass returns
   `(T, DiagBag)` or `Result<T, DiagBag>`; an error in one operator must not block
   checking of other operators in the same module (error isolation is at operator
   granularity). All diagnostics carry a span and are rendered graphically via miette.
4. **Effect tiers only ascend**: L0 `function` / L1 `query` / L2 `action`; if a callee's
   tier is higher than the caller's it is a compile error
   (callee.level > caller.level = error). Lambda bodies are always L0.
   `read`/`lookup`/`insert`/`update`/`delete` are built-in effect primitives recognized
   by name (reserved names).
5. **Backends consume only CIR**: backends must not read the AST back; type/plan
   information travels with CIR as annotations (doc/codegen-backend.md §6 "Explicitly
   out of scope").
6. **McSpec v1 fragment**: bool/int expressions + tables with int keys and a single int
   value field; constructs outside the fragment produce a "not supported in the
   model-checking fragment" diagnostic (a lowering error, `cqlc verify` exit code 2), not
   a silent skip (per-property unsupported constructs like prime/`~>`/`until` are skipped
   with a warning instead).
7. **Determinism first**: set/bag/map materialization always follows the canonical order
   (CanonOrd); generated code must not introduce nondeterminism such as hash-order
   iteration.

## 4. Common extension points

### 4.1 Adding a standard-library function (pure)

1. Implement the Rust-side function in `crates/cql-runtime/src/stdlib/<domain>.rs`
   (follow the existing functions in the same domain);
2. Add the CQL signature (including the generic scheme) to the built-in signature table
   in `crates/cql-compiler/src/types.rs`;
3. Desugar/codegen mapping: `desugar.rs` (if it is syntax sugar) or the call-emission
   site in `codegen.rs` maps to the runtime function (method sugar `recv.f(x)` dispatches
   on the first argument, no extra work needed);
4. Documentation: `doc/cql.md` appendix B is the authoritative signature table — add the
   new function there.

### 4.2 Adding a backend

1. Implement the `codegen::Backend` trait (`name()` + `emit(&CirModule, &EmitCtx)`);
   see `mududb_be.rs` for a minimal example (CIR → plain text, roughly a one-file
   skeleton);
2. If the target language lacks closures/generics/match, append a **target-specific
   normalization pass** after the shared lowering; do not modify the CIR definition;
3. Register dispatch by `cql.toml [build] backend` name in
   `crates/cql-cli/src/project.rs`;
4. End-to-end differential test: the same examples module must produce identical results
   through the new backend and the rust backend.

### 4.3 Adding a diagnostic

In the relevant pass, `bag.push_error(CqlError::new(src, span, message, help))` (use
`push_warning` for warnings). The one-line `help` should suggest a rewrite direction.
Citing the spec section is customary (e.g. `help: ... (§2.4)`). If a new error category
affects the miette derive, mind the `unused_assignments` allow in lib.rs (§5).

## 5. Testing conventions

- **Unit tests**: `#[cfg(test)] mod tests` inside each pass file, constructing source
  strings directly, running the pass, and asserting on diagnostics/side tables.
- **Integration tests**: `crates/*/tests/` directories.
- **End-to-end generated-code tests**: set up a scratch cargo project under `target/tmp/`,
  compile and run the generated crate (`cargo test --offline`), and verify runtime
  results.
- **CLI tests**: invoke the binary via `env!("CARGO_BIN_EXE_cqlc")`; fixtures are always
  **copied** from examples into a fresh directory under `target/tmp/` before being touched
  — never dirty `examples/`.
- Avoid "update" in test binary names (Windows UAC error 740, see Build Guide §5).

## 6. Debugging tips

- No need for cargo expand: diagnostics are rendered by miette's fancy renderer (with
  source snippets, line/column, and help) — read the diagnostics first; when you want to
  see intermediate states, every pass's input/output is an immutable AST, so just
  `format!("{x:#?}")` in a unit test.
- Temporary probes: this repository's agents customarily add temporary `dbg_*.rs` probe
  tests inside a crate to locate a problem — **delete them before committing**.
- tree-sitter parsing problems: first reproduce in `crates/tree-sitter-cql` with
  `tree-sitter parse <file>` / `tree-sitter test`, then look at the CST→AST mapping in
  `lower.rs`; prefer resolving ambiguities in `grammar.js` with `prec`/`conflicts`
  (doc/todo.md key technical decisions).
