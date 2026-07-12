# Backends & Tooling Ecosystem

This document covers the Rust backend artifacts, the mududb backend (placeholder), model
checking, the VSCode plugin, and tree-sitter grammar development. Authoritative
architecture docs: [../codegen-backend.md](../codegen-backend.md),
[../backend-mududb.md](../backend-mududb.md), [../model-check.md](../model-check.md).

## 1. Rust backend (MVP, usable)

`cqlc build` renders each CQL module into a Rust source file (askama template skeleton +
recursive emitter); all backends consume only CIR and never read the AST back.

### What gets generated

- **State struct**: one field per table, `pub <table>: MemTable<TableKey, TableRow>`;
  `State::new()` constructs an empty store.
- **Atomic apply `State::apply(&mut self, ops: &CqlSet<WriteOp>)`**: clones into a
  type-erased `TableRegistry` → `apply_write_ops` (conflict → FK → invariant checks,
  clone-apply-check-swap) → on success writes back, on failure returns `ApplyError` with
  state unchanged (§5.2 atomicity).
- **Row/key structs**: each table gets `<Table>Row` / `<Table>Key` (implementing
  Eq/Hash/CanonOrd).
- **Structural record types**: structural records like `{ key: string, agg: float }` are
  generated as **interned `Rec_<hash>` structs, hashed over the field set** (fields sorted
  by name); structurally identical records share the same Rust type.
- **`CanonOrd`**: all hashable types implement the canonical-order total order (§2.3),
  which guarantees deterministic materialization order for sets/bags/maps.
- **`CqlF64`**: `f64` does not satisfy `Eq + Hash`; when a set element is a float, it is
  wrapped in `CqlF64(pub f64)` (hashed by bit pattern).
- **enum**: a Rust enum of the same name (self-recursive payloads are automatically
  boxed); `T -> U` is `Rc<dyn Fn(T) -> U>` (the product of lambda lifting).
- CQL `test` blocks generate `#[test]`s (in a `cql_tests` module), fixtures → in-memory
  tables, `expect` → assertions.
- **Known MVP deviations**: `ReadPlan::IndexScan` currently compiles to a filtered full
  scan (the runtime keeps the secondary-index interface); result semantics are unaffected;
  generic enum instantiation, generic type-alias instantiation, cross-module query/action
  calls, cross-module generic-function calls, and first-class use of imported functions
  all produce "not supported by codegen MVP"-style diagnostics.

### Consuming the generated crate

`out_dir` (default `target/cql`) is a standalone cargo crate: its `Cargo.toml` depends on
`cql-runtime` by absolute path and carries an empty `[workspace]` (so it is not absorbed
by the outer workspace); `src/lib.rs` aggregates `pub mod <module>;`. Host code simply
does `use <pkg>::<module>::{State, ...}`: construct a `State`, call the generated query
functions, and call `state.apply(&ops)` on the `CqlSet<WriteOp>` returned by an action.

### CQL type → Rust ABI mapping

| CQL type | Generated Rust |
| --- | --- |
| `bool` / `int` / `float` / `string` | `bool` / `i64` / `f64` / `String` |
| `date` / `decimal(m, n)` | `cql_runtime::Date` / `cql_runtime::Decimal` |
| `option` / `vector` / `set` / `bag` / `map` | `Option` / `Vec` / `CqlSet` / `CqlBag` / `CqlMap` |
| tuple | Rust tuple |
| record (structural type) | interned `Rec_<hash>` struct |
| table row / key | `<Table>Row` / `<Table>Key` |
| enum | Rust enum of the same name (self-recursive payload Box) |
| `T -> U` | `Rc<dyn Fn(T) -> U>` |
| `write_op` | `cql_runtime::WriteOp` (type-erased, §3.6) |

Note: §6.2 specifies `date`/`decimal` as records at the ABI layer; the Rust backend keeps
the runtime's native newtypes (the ABI mapping only matters at the WASM component
boundary).

## 2. mududb backend (placeholder, PROPOSAL status)

`cqlc build --backend mududb` builds no component; it only emits one
`<module>.mududb-plan.txt` per module (`cql_compiler::mududb_be::MududbBackend`
implements the `Backend` trait). The plan text has three parts:

1. **Component interface skeleton**: `component <mod> { import <table>: table<(s64, record {...})>;
   import syscalls: mududb_syscall_v1; export <op>: <sig> }` — CQL types mapped per the
   §6.2 ABI mapping (`int`→`s64`, `vector`→`list`, etc.), with the parameters of
   parameterized operators appended at the end of the export signature;
2. **Per-operator syscall call-sequence skeleton**: for a query,
   `session_open → snapshot_begin → reads (tbl_get/tbl_scan, annotated with the read plan)
   → session_close`; for an action, `session_open → txn_begin → cmd_insert/cmd_update →
   txn_commit → session_close` (FK/invariant enforced on the kernel side);
3. **PROPOSAL status declaration**: both the head and the tail of the file state that the
   syscall names are placeholders, **containing no syscall numbers/signatures**, pending
   alignment with `mududb_p/doc/lang.common`.

When reading a `.mududb-plan.txt`, treat it as a "draft deployment blueprint": the
interface shape and call order are open for discussion, but no field is a stable contract.
For background and the full proposal see [../backend-mududb.md](../backend-mududb.md)
(especially §3 syscall-contract proposal and §9 known gaps).

## 3. Model checking (cqlc verify)

Architecture: compiler-side `mc_lower` (desugared AST → `cql-mc`'s checker-agnostic IR
`McSpec`) → Stateright explicit-state backend (the z3 backend exists but is not enabled in
this build).

**v1 fragment rules** (constructs outside the fragment produce a "not supported in the
model-checking fragment" diagnostic, exit code 2):

- Expressions limited to bool/int; tables = **int keys ⇀ a single int value field**
  (non-int fields are ignored with a warning);
- Single-module projects (v1 does not support multi-module verify);
- Initial state comes only from `test` block fixtures; action-parameter domains are
  inferred from `verify.toml [domain]`;
- Action bodies support only restricted shapes (`match lookup(...)` guards +
  `if ... then set{...} else set {}`, write_op constructs); generic/recursive actions are
  not supported.

**Mapping**: action → guarded transition (guard + updates + param_domains),
`invariant`/`[](φ)` → `PropertyKind::Always`, `<>(φ)` → `Eventually`,
`fold(to_vector(table), ...)` → Sum/unfolded quantifiers.

**Rejected/skipped (warnings, not errors)**: fragment-external table fields such as
record/string (ignored), prime (next state), `~>` (leads-to), `until`, bare prime — these
properties are skipped and warned about in the output, not counted toward the verdict.

**Stateright backend**: exhaustive BFS enumeration of the finite bounded model — no
counterexample for a safety property means `PROVED(stateright-exhaustive)` (which is a
proof for the bounded model); when a counterexample exists, the shortest-path
counterexample (BFS) is printed. Exit codes 0/1/2: see [cqlc CLI & Config Files](cli.md).

**bank_project walkthrough**:

```text
$ ./target/debug/cqlc verify examples/bank_project
verifying `bank` (stateright): 1 table(s), 1 transition(s), 2 of 2 propert(ies), k=8
  PROVED(stateright-exhaustive) balance_conserved
  PROVED(stateright-exhaustive) no_negative
result: all 2 propert(ies) hold within the bounds
```

`balance_conserved` (`[](total_balance() = 10000)`) and `no_negative` pass exhaustively
within the bounds given by `verify.toml` (`accounts.rows = 2`, `id ∈ 1..2`,
`balance ∈ {0, 6000, 4000}`, k=8); the prime-containing `transfer_preserves` is skipped
(the Stateright backend does not support next-state). Change an update to deduct more and
you get a `COUNTEREXAMPLE` with a shortest counterexample trace (each step:
action/arguments/applied-rejected/state diff).

## 4. VSCode plugin (editors/vscode-cql)

- **Features**: a `DocumentSemanticTokensProvider` provides semantic highlighting —
  web-tree-sitter loads the wasm grammar to parse documents, and the captures in
  `queries/highlights.scm` map to **13 token types (keyword/type/typeParameter/function/
  variable/parameter/property/enumMember/namespace/string/number/comment/operator) × 4
  modifiers (declaration/readonly/builtin/escape)**; edits trigger byte-level incremental
  reparse via `tree.edit(...)`.
- **Graceful degradation**: when `tree-sitter-cql.wasm` is missing from the extension
  root, a notice is logged to the CQL output channel and activation proceeds normally (no
  semantic tokens; basic features such as bracket matching/comments/indentation come from
  `language-configuration.json` and do not depend on the wasm).
- **Build & debug**: `npm install --no-audit --no-fund && npm run compile` (tsc), then
  open the directory in VSCode and press F5 (`.vscode/launch.json`) to launch the
  Extension Development Host.
- **wasm build pending**: requires emscripten (this machine has no emcc/Docker); the steps
  (`tree-sitter build --wasm` → copy the artifact to the extension root) are in the
  plugin's README and [Build Guide](build.md) §4; the wasm must be rebuilt whenever
  `grammar.js` changes.

## 5. tree-sitter grammar development (crates/tree-sitter-cql)

- `grammar.js` implements the doc/cql.md appendix A.1/A.2 grammar; prefer resolving
  ambiguities with `prec` and the `conflicts` declarations at the top of the grammar (e.g.
  the temporal reading of `[]` inside property bodies vs the empty vector literal — see the
  corresponding comments in grammar.js); genuinely hard-to-express cases (such as
  `ident <` generic vs comparison) are disambiguated at the `lower.rs` stage.
- Working loop: edit `grammar.js` → `tree-sitter generate` → `tree-sitter test` (corpus
  43/43); validate highlight queries with `tree-sitter highlight`.
- Corpus layout: `test/corpus/01_lexical.txt` (lexical) `02_declarations.txt`
  (declarations) `03_expressions.txt` (expressions) `04_queries_actions.txt`
  (queries/actions) `05_properties.txt` (temporal properties), each case a `===` title +
  source + expected S-expression.
