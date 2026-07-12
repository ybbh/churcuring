# Churcuring

```
┌──────────────┐
│   -> { } │.│ │
│ § ->  Σ  [#] │
│   ->  λ  │:│ │
└──────────────┘
```

```
Churcuring Query Language
```

Churcuring Query Language (CQL) is a **declarative, category-theory-driven database language** that enables
**formal specifications**, compiles them into **hierarchical, effect-tiered operators**, and generates **executable logic and
database access code** in a structured, verifiable, and deterministic manner.

The name *Churcuring* honors the **Church–Turing** thesis, reflecting the system’s goal of bridging formal, symbolic structure, database and modern large language models.

---

## Key Features

The formal core of Churcuring is **CQL (Churcuring Query Language)** — a declarative,
category-theory-driven query and business-logic language for the λFoundry database
abstraction layer (see [`doc/cql.md`](doc/cql.md)):

- **No JOIN, ever** — every cross-table association is an explicit `lookup` over keyed
  tables (`table = { Key → Object }`). Joins are eliminated at the language and
  compiler level, not merely discouraged.
- **TLA+-style math, Rust-style naming** — set comprehensions (`set { e : x \in S }`),
  quantifiers (`\A`, `\E`), and definition structure (`==`), combined with `snake_case`
  conventions and a strong static type system (table-derived types `key t` / `value t`,
  `option<T>`, enums, generics).
- **Strict effect separation** — all computation is pure; the only side effects are
  table reads (`read`/`lookup`) and writes (`insert`/`update`/`delete` returning
  `set<write_op>`), organized into three operator tiers (L0 `function` / L1 `query` /
  L2 `action`) that may only compose upward.
- **Total determinism** — same snapshot + same parameters ⇒ bit-identical results,
  including identical trap behavior. Materialization follows a canonical order.
- **Provable termination by default** — comprehensions, quantifiers, `fold`, and
  structural recursion (`function recursive`, checked by a termination pass) are
  theorem-level terminating; general recursion is an escape hatch verified by
  bounded model checking (`with depth n`).
- **Algebraically optimizable** — queries are morphisms in the category of sets;
  functor and monad laws guarantee the correctness of filter fusion, map pushdown,
  and `read` plan classification (point / index / full-scan) — never a JOIN node.
- **Dual backends** — primary target is a λFoundry WASM component with native table
  operations; an experimental Rust + JOIN-free SQL backend (single-table statements
  only, `GROUP BY`-free) enables gradual migration onto Postgres/MySQL.

---

## Core Pipeline

```
§  ->  { }  ->  Σ  ->  λ  ->  [#]
AI      CQL    Formal  Compiled  Database
prompt  spec   model   operator  (λFoundry / SQL)
```

CQL compiler pipeline (§6.1 of the design doc):

```
Parse → Type check → Monad/functor algebraic optimization → Backend codegen
                                                           ├─ Backend A: Rust + JOIN-free SQL (experimental)
                                                           └─ Backend B: λFoundry WASM component (primary)
```

---

## Architecture Overview

### 1. Specification Layer `{}`

The CQL source: modules of `table` / `index` declarations, pure `function`s,
`query`s, and `action`s. Everything is statically typed and effect-checked at
compile time; reads and writes are the only effects, and associations are written
as explicit key lookups.

---

### 2. Formal Layer `Σ`

Each query is a morphism `f: table₁ × … × tableₙ × Params → Result` over set
objects; operators correspond to Kleisli morphisms of the `Id ↪ Reader ↪
Reader×Writer` monad embedding chain (§5.5). This layer is where determinism,
snapshot/transaction semantics (§5.2), trap semantics (§5.3), and the two-tier
termination guarantee (§5.4) are defined — the foundation for formal verification
and algebraic optimization.

---

### 3. Operator Layer `λ [#]`

Optimized queries are compiled into executable operator code that runs against
the database `[#]`: either a λFoundry WASM component (importing tables as
resources, exporting one function per `query`/`action`), or JOIN-free single-table
SQL plus explicit lookup loops on the Rust side.

---

## Symbol Legend

| Symbol | Meaning                              |
|--------|--------------------------------------|
| §      | AI prompt / natural language input   |
| `{}`   | Specification                        |
| Σ      | Formal Model                         |
| λ      | Operator                             |
| `[#]`  | Database                             |


---

## Project Status

Churcuring is an **open research and engineering framework**.  
Interfaces, compilers, and runtime backends are under active development.

The CQL language design (v0.1.0, draft) is documented in [`doc/cql.md`](doc/cql.md);
the codegen backend architecture in [`doc/codegen-backend.md`](doc/codegen-backend.md),
model checking in [`doc/model-check.md`](doc/model-check.md), and the mududb backend
proposal in [`doc/backend-mududb.md`](doc/backend-mududb.md).

**User & developer guides** (build, development, language tour, CLI reference, backends):
[简体中文 `doc/cn/`](doc/cn/README.md) · [English `doc/en/`](doc/en/README.md)

### Implemented toolchain (workspace `crates/`)

| Crate | Role |
|-------|------|
| `tree-sitter-cql` | tree-sitter grammar (full CQL syntax + property/fairness), corpus tests |
| `cql-compiler` | parse → resolve → effect → type → termination → desugar → optimize → CIR → codegen (`Backend` trait: `RustBackend`, `MududbBackend` placeholder); `mc_lower` → `McSpec` |
| `cql-runtime` | values/collections/tables/write-op application (conflict→FK→invariant, atomic) + full Appendix-B stdlib |
| `cql-mc` | checker-neutral `McSpec` IR + Stateright (explicit) / z3.rs (symbolic BMC) backends |
| `cql-cli` | `cqlc` — the compiler CLI |

```console
$ cqlc new shop                     # scaffold cql.toml + src/main.cql
$ cqlc check examples/shop_project  # full pipeline, miette diagnostics
check passed: `shop` (2 module(s), 0 warning(s))
$ cqlc build examples/shop_project  # → target/cql Rust crate, `cargo build --offline`
$ cqlc test  examples/bank_project  # runs CQL `test` blocks (test_transfer_basic ... ok)
$ cqlc verify examples/bank_project # bounded model checking (stateright)
  PROVED(stateright-exhaustive) balance_conserved
  PROVED(stateright-exhaustive) no_negative
result: all 2 propert(ies) hold within the bounds
$ cqlc build --backend mududb <dir> # → <module>.mududb-plan.txt (PROPOSAL, no syscall numbers)
$ cqlc clean <dir>
```

Editor support: `editors/vscode-cql` (tree-sitter semantic highlighting; the wasm
grammar build needs an emscripten environment — steps in the extension README).

### Development

```console
$ cargo test --workspace --offline   # all crates (z3 feature off by default here)
$ cd crates/tree-sitter-cql && tree-sitter test   # grammar corpus
```

Known MVP limits (documented in code): cross-module generic/enum calls, `cons`
patterns, wasm component emission, z3 verify engine (gh-release download),
multi-level `use a::b` paths.


---
