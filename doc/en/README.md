# CQL Toolchain Documentation (English)

CQL (Churcuring Query Language) is a declarative, strongly typed query and business-logic
language: it borrows set comprehensions, quantifiers, and definition structure from TLA+,
uses Rust-style naming, and has **no JOINs at all** — every cross-table association goes
through an explicit `lookup`. This repository is the complete CQL toolchain: a tree-sitter
grammar, a compiler library (parse → name resolution → effect checking → type checking →
termination → desugaring → optimization → CIR → code generation), a runtime library, a
model checker (Stateright / z3 backends), the `cqlc` CLI, and a VSCode highlighting plugin.

## Documentation Index

### User and Developer Guides (this directory)

| Document | Contents |
| --- | --- |
| [Build Guide](build.md) | Prerequisites, build/test commands, offline (air-gapped) build notes, known platform issues |
| [Development Guide](development.md) | Workspace structure, compilation pipeline, key design invariants, how to add stdlib functions/backends/diagnostics, testing conventions |
| [CQL Language Tour](cql-tour.md) | Syntax tour: modules, types, declarations, effect tiers, expressions, tables, termination, temporal properties, stdlib, traps |
| [cqlc CLI & Config Files](cli.md) | `new`/`check`/`build`/`test`/`verify`/`clean` subcommands, `cql.toml` and `verify.toml` reference |
| [Backends & Tooling Ecosystem](backends.md) | Rust backend artifacts, mududb backend (placeholder), model checking, VSCode plugin, tree-sitter grammar development |

### Design Specs (authoritative — language semantics follow these files)

| Document | Contents |
| --- | --- |
| [../cql.md](../cql.md) | CQL language specification (type system, modules, expressions, semantic rules, appendices A–D) |
| [../model-check.md](../model-check.md) | Formal model-checking mechanism (bounded/temporal layers, dual-backend architecture, verify.toml) |
| [../backend-mududb.md](../backend-mududb.md) | mududb backend: query/command syscall contract (proposal) and SQL channel |
| [../codegen-backend.md](../codegen-backend.md) | Code generation backend architecture (CIR, Backend trait, checklist for new backends) |
| [../todo.md](../todo.md) | Implementation plan and approved syntax-revision records |

> In-code comments reference the design docs via flat paths such as `doc/cql.md §3.6`,
> so the design docs stay at the `doc/` root and are not moved into this directory.

## Quick Start

Prerequisite: Rust nightly 1.94 (this repository has no network access; all cargo commands
carry `--offline`).

```sh
# 1. Build the entire workspace (produces ./target/debug/cqlc)
cargo build --workspace --offline

# 2. Scaffold a new project
./target/debug/cqlc new demo

# 3. Type/effect check (works on a single file or a project directory)
./target/debug/cqlc check examples/shop_project

# 4. Code generation + cargo build (artifacts written to the project's target/cql)
./target/debug/cqlc build examples/shop_project

# 5. Run CQL test blocks (generates #[test] then runs cargo test)
./target/debug/cqlc test examples/bank_project

# 6. Model checking (Stateright explicit-state backend)
./target/debug/cqlc verify examples/bank_project
```

More examples: `examples/analytics.cql` (single file, zero config),
`examples/shop_project` (multi-module project), `examples/bank_project` (model-checking
example with `verify.toml`).

## Current Implementation Status at a Glance

- The full compilation pipeline works (`cargo test --workspace --offline`: ~289 tests,
  all green; tree-sitter corpus 43/43).
- The Rust backend is MVP (usable); the mududb backend is a placeholder skeleton (only
  produces deployment-plan text, see [Backends & Tooling Ecosystem](backends.md)).
- Model-checking v1 fragment: bool/int expressions + tables with int keys and int values;
  `--engine z3` and `--replay` are not available yet (see [cqlc CLI & Config Files](cli.md)).
- The VSCode plugin's wasm grammar bundle has not been built yet (missing emscripten; the
  plugin degrades gracefully, see [Build Guide](build.md)).
