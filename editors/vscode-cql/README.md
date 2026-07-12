# vscode-cql

Syntax highlighting for **CQL** (Churcuring Query Language), powered by
tree-sitter running as WebAssembly inside VSCode.

## Features

- **Semantic highlighting** via `DocumentSemanticTokensProvider`: the
  tree-sitter CQL grammar (`crates/tree-sitter-cql`) parses the document and
  `queries/highlights.scm` captures are mapped to semantic tokens
  (keyword / type / function / variable / parameter / property / enumMember /
  namespace / string / number / comment / operator).
- **Incremental reparsing**: document edits are applied to the syntax tree
  with `tree.edit(...)` (byte-accurate `startPosition` / `oldEndPosition` /
  `newEndPosition` computation) and reparsed incrementally.
- **Graceful degradation**: if `tree-sitter-cql.wasm` is absent from the
  extension root, a note is written once to the **CQL** output channel and
  the extension activates without the semantic token provider (no errors).

Basic editor features (bracket matching, comment toggling, indentation
rules) come from `language-configuration.json` and work even without the
wasm grammar.

## Building the wasm grammar

The extension expects `tree-sitter-cql.wasm` in the extension root
(`editors/vscode-cql/tree-sitter-cql.wasm`). Building it requires the
tree-sitter CLI and an Emscripten toolchain (or Docker):

```sh
# 1. tree-sitter CLI (0.26.x; the grammar was generated with it)
npm install -g tree-sitter-cli

# 2. Emscripten — see https://emscripten.org/docs/getting_started/downloads.html
#    (emsdk install latest && emsdk activate latest && source emsdk_env.sh),
#    or install Docker and let the CLI use the official build image.

# 3. Build the wasm grammar (run in the grammar crate)
cd ../../crates/tree-sitter-cql
tree-sitter build --wasm

# 4. Copy the artifact into the extension root
cp tree-sitter-cql.wasm ../../editors/vscode-cql/
```

> The build environment used to develop this extension had neither `emcc`
> nor `docker` available, so the wasm artifact is **not** checked in; the
> steps above were verified up to the toolchain requirement. Re-run
> `tree-sitter build --wasm` whenever `grammar.js` changes.

## Development

```sh
cd editors/vscode-cql
npm install --no-audit --no-fund   # installs typescript, @types/vscode,
                                   # @types/node, web-tree-sitter
npm run compile                    # tsc -p ./  (must pass)
npm run watch                      # incremental compile
```

Press **F5** in VSCode (with this folder open) to launch an *Extension
Development Host*; open any `examples/**/*.cql` file to see highlighting.
Watch the **CQL** output channel for grammar-loading diagnostics.

## Packaging

```sh
npm install -g @vscode/vsce
vsce package            # produces vscode-cql-0.1.0.vsix
```

Make sure `tree-sitter-cql.wasm` has been built and copied into this
directory before packaging (see above). `web-tree-sitter`'s own runtime
(`node_modules/web-tree-sitter/tree-sitter.wasm`) is bundled automatically
via `dependencies`.

## Verifying the highlight queries

`queries/highlights.scm` is written against the node/field names in
`crates/tree-sitter-cql/grammar.js` (see `src/node-types.json`). To check
that the query compiles and every capture matches real nodes:

```sh
cd crates/tree-sitter-cql
tree-sitter query ../../editors/vscode-cql/queries/highlights.scm \
    ../../examples/analytics.cql
```

For a full end-to-end check, point tree-sitter at the query from the
grammar crate (temporary, do not commit):

```sh
mkdir -p crates/tree-sitter-cql/queries
cp editors/vscode-cql/queries/highlights.scm crates/tree-sitter-cql/queries/
tree-sitter highlight --check examples/analytics.cql   # needs a config with
                                                       # parser-directories
rm -rf crates/tree-sitter-cql/queries                  # clean up afterwards
```

Both checks pass for `examples/analytics.cql` and all files under
`examples/*/src/*.cql`.
