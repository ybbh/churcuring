# cqlc CLI & Config Files

`cqlc` is the command-line entry point of the CQL toolchain (crate `crates/cql-cli`,
clap subcommands). All commands in this document are run from the repository root
(Windows + Git Bash; forward slashes are fine for paths). Build:

```sh
cargo build -p cql-cli --offline   # artifact ./target/debug/cqlc
```

## 1. Target resolution rules (shared by all subcommands)

`cqlc <cmd> [path]` (path defaults to the current directory):

1. Walk up from `path` to find `cql.toml` ⇒ **project mode**: compile all `**/*.cql`
   under `source_root`;
2. Not found and `path` is a `.cql` file ⇒ **single-file zero-config mode**: compile as a
   standalone module (out_dir fixed to `target/cql` under the current directory, backend
   fixed to `rust`);
3. Otherwise, error out and suggest `cqlc new <name>`.

## 2. Subcommands

### `cqlc new <name>` — scaffolding

Creates `<name>/cql.toml` + `<name>/src/main.cql` in the current directory (errors if the
directory already exists).

```text
$ ./target/debug/cqlc new demo
created CQL project `demo` at .../demo
```

The generated `src/main.cql` contains a `query hello() -> string` template.

### `cqlc check [path]` — checking

Runs the full pipeline (parse → … → optimize) and reports diagnostics only. Exit codes:
0 on success, 1 on compile errors.

```text
$ ./target/debug/cqlc check examples/shop_project
check passed: `shop` (2 module(s), 0 warning(s))

$ ./target/debug/cqlc check examples/analytics.cql     # single-file zero-config
check passed: `analytics` (1 module(s), 0 warning(s))
```

Diagnostics are rendered graphically via miette (with source snippets, line/column, help,
and spec-section references):

```text
  x operands of `+` must have the same numeric type, found `int` and `string`
   ,-[bad.cql:6:25]
 6 |     read(t, lambda(x) { x.id + "str" })
   :                         ^^^^^^|^^^^^
   `----
  help: no implicit conversions; use `as` to convert (§2.4)
error: could not compile `bad` due to 2 error(s)
```

### `cqlc build [path] [--backend rust|mududb]` — code generation + build

After check passes, generates code module by module in dependency topological order into
`out_dir`, then runs `cargo build --offline`. `--backend` overrides the `cql.toml`
setting; unknown backends are errors.

- `rust` (default): writes a standalone cargo crate (`Cargo.toml` depends on cql-runtime
  by absolute path + empty `[workspace]`; `src/lib.rs` + one `src/<module>.rs` per
  module), then runs cargo build. Sample output:

```text
$ ./target/debug/cqlc build examples/shop_project
generated Rust crate for `shop` (2 module(s): util, shop) at .../examples/shop_project/target/cql
cargo build succeeded (.../examples/shop_project/target/cql)
```

- `mududb`: PROPOSAL-status placeholder; writes one `<module>.mududb-plan.txt` per module
  (deployment-plan text, see [Backends & Tooling Ecosystem](backends.md)), **no component
  build is performed**:

```text
$ ./target/debug/cqlc build examples/shop_project --backend mududb
generated mududb deployment plan (PROPOSAL) for `shop` (2 module(s): util, shop) at ...
note: syscall contract is a proposal — no component build yet (doc/backend-mududb.md §9)
```

Exit codes: 0 on success; 1 on compile/generation/cargo failure.

### `cqlc test [path]` — run CQL test blocks

Generates a crate the same way as build, then runs `cargo test --offline` (CQL `test`
blocks are compiled to Rust `#[test]`, fixtures construct in-memory tables by primary key,
`expect` compares using predicate equality). Requires the rust backend. Exit codes: 0 on
success, 1 on failure.

```text
$ ./target/debug/cqlc test examples/bank_project
...
running 1 test
test bank::cql_tests::test_transfer_basic ... ok

cargo test succeeded (.../examples/bank_project/target/cql)
```

### `cqlc verify [path] [flags]` — model checking

mc_lower lowers the desugared AST to `cql-mc`'s McSpec (v1 fragment: bool/int
expressions + tables with int keys and a single int value field), and the Stateright
explicit-state backend gives a verdict per property. **Project mode only** (single files
are not supported). Exit codes:

| Code | Meaning |
| --- | --- |
| 0 | All properties hold within the bounds |
| 1 | Counterexample found (or frontend compile error) |
| 2 | verify.toml invalid / mc_lower failure (construct outside the fragment) |

Flags:

| Flag | Description |
| --- | --- |
| `--bounded` | Check only bounded-layer properties (Always: invariants/safety) |
| `--temporal` | Check only temporal properties (Eventually); giving both or neither = all |
| `--depth N` | Override the default recursion-depth bound (default 32) |
| `--trace N` | Override the trace-length bound k (default 8) |
| `--engine stateright\|z3` | Default stateright; **z3 is not available in this build** (requires the `z3` feature and prebuilt Z3, doc/model-check.md §7.3); selecting z3 errors out immediately |
| `--replay <case>` | Counterexample replay (generates a test block) — **not implemented**, errors if given |

Sample output (`examples/bank_project`):

```text
$ ./target/debug/cqlc verify examples/bank_project
  x table `accounts`: non-int field(s) owner are not part of the model and are ignored
   ...
  x property `transfer_preserves`: prime (next-state) is not supported by the stateright backend; skipped
   ...
verifying `bank` (stateright): 1 table(s), 1 transition(s), 2 of 2 propert(ies), k=8
  PROVED(stateright-exhaustive) balance_conserved
  PROVED(stateright-exhaustive) no_negative
result: all 2 propert(ies) hold within the bounds
```

- One verdict line per property: `PROVED(stateright-exhaustive)` (exhausting the bounded
  model amounts to a proof) / `COUNTEREXAMPLE` / `EventuallyHolds` etc.
  (doc/model-check.md §7.2).
- Counterexamples come with a shortest path (BFS), each step rendering the action,
  arguments, result (applied/rejected), and state diff, emitted in `cql-mc`'s unified
  Counterexample format.
- Presentation discipline: `PROVED` means proven **within the bounded model** only;
  prime/`~>`/`until` properties are skipped (warned, not counted toward the verdict).

### `cqlc clean [path]` — cleanup

Removes out_dir (prints `nothing to clean` if it does not exist).

## 3. `cql.toml` reference

```toml
[package]
name = "shop"            # required; also used as the generated crate's package name
                         # (non-identifier characters are escaped; containing "update"
                         # is rewritten to "upd" to dodge Windows UAC)
version = "0.1.0"        # optional, default "0.1.0"; currently unused by any subcommand

[build]
source_root = "src"        # source root directory, default "src"
out_dir = "target/cql"     # generated-code directory (relative to project root), default "target/cql"
backend = "rust"           # "rust" (default) | "mududb" (PROPOSAL-status placeholder)

[mududb]                   # used when backend = "mududb" (draft, backend-mududb.md §8)
app = "shop"
sql_adapter = "off"        # "off" (default) | "sqlite" | "postgres" | "mysql"
```

## 4. `verify.toml` reference

Place it at the project root (next to `cql.toml`); everything defaults if it is absent.

```toml
[depth]
default = 32          # default recursion-depth bound (D)
subordinates = 16     # per-operator override (equivalent to the source-level with depth annotation)

[domain]
accounts.rows = 2                 # row-count bound (v1 accepts but ignores: initial state comes only from fixtures)
accounts.id = "1..2"              # key/field domain: "lo..hi" range
accounts.balance = [0, 6000, 4000]  # or an explicit set of values (integers only)

[trace]
length = 8            # trace-length bound k (default 8)

[fairness]
weak = []             # weak/strong: accepted in v1 but no backend enforces it, warning only
```

**What actually takes effect in v1**:

- `[depth]` (default and per-operator overrides): effective;
- `[domain] table.field`: effective — used for action-parameter domain inference (e.g.
  `from_id`/`to_id` take the key domain, `amt` takes the `accounts.balance` domain);
  values support integers only (range strings or integer arrays);
- `[domain] table.rows`: accepted but **ignored** (v1 takes initial state only from `test`
  block fixtures);
- `[trace] length`: effective (the `k=` in the output);
- `[fairness]`: declaring it produces the warning "no backend enforces fairness yet"; all
  traces are still enumerated as usual.
