# 构建指南

本文档覆盖在本仓库构建、测试全部组件所需的步骤。本机环境：**Windows + Git Bash**，
**无网络**，因此所有 cargo 命令一律带 `--offline`；已验证工具链：rustc/cargo
1.94.0-nightly、tree-sitter CLI 0.26.10。

## 1. 前置条件

| 工具 | 版本要求 | 用途 |
| --- | --- | --- |
| Rust nightly + cargo | 1.94.0-nightly（本机 `0e8999942 2025-12-30`） | 构建全部 Rust crate（必需） |
| tree-sitter CLI | 0.26.x（`npm install -g tree-sitter-cli`） | 语法开发：`tree-sitter generate/test/highlight`（仅改 grammar 时需要） |
| Node.js + npm + tsc | TypeScript ^5.3 | VSCode 插件构建（可选） |
| emscripten（或 Docker） | — | `tree-sitter build --wasm` 语法包 wasm 构建（可选，**本机暂不可用**，见 §4） |

## 2. Rust workspace 构建与测试

```sh
# 全 workspace 构建（cqlc 产物在 ./target/debug/cqlc）
cargo build --workspace --offline

# 单 crate 构建
cargo build -p cql-cli --offline
cargo build -p cql-compiler --offline
cargo build -p cql-runtime --offline

# 全量测试（约 289 个测试）
cargo test --workspace --offline

# 单 crate 测试
cargo test -p cql-mc --offline
```

### 2.1 离线 / 隔离网络（air-gapped）说明

- 全部构建都使用 `--offline`；依赖已预置在 cargo 缓存与本仓库的 `Cargo.lock` 中。
- **z3 与 gh-release 限流问题**：`z3 0.20` 的 `gh-release` feature 在构建期通过
  GitHub API 下载预编译 Z3 4.16，受限/限流（403）时构建脚本会 panic。为此
  **workspace 默认不为 cql-compiler / cql-cli 开启 z3**：两者都以
  `default-features = false, features = ["stateright"]` 依赖 cql-mc，`cqlc verify`
  只暴露 Stateright 后端。
- 若确实需要 z3 后端（`cargo build -p cql-mc --features z3 --offline`）且网络受限，
  已知变通方法（cache-copy workaround）：
  1. 找到已成功的旧构建缓存：`target/debug/build/z3-sys-<oldhash>/out/z3-4.16.0`；
  2. 复制到新 hash 目录：`target/debug/build/z3-sys-<newhash>/out/z3-4.16.0`；
  3. 重新构建，构建脚本会识别缓存而跳过下载。
  也可设环境变量 `Z3_LIBRARY_PATH_OVERRIDE` 指向本地 `libz3.lib`（参见
  `doc/model-check.md` §7.3）。

### 2.2 生成代码的构建（cqlc build 的产物）

`cqlc build` 在工程的 `out_dir`（默认 `target/cql`）写入一个**独立 cargo crate**：

- `Cargo.toml` 以**绝对路径**依赖 `crates/cql-runtime`，并带空的 `[workspace]` 段
  （避免被上层 workspace 吸收）；
- `cqlc build` 随后自动在该目录执行 `cargo build --offline`；`cqlc test` 执行
  `cargo test --offline`。
- 产物可 `cqlc clean <path>` 删除。

## 3. tree-sitter 语法（crates/tree-sitter-cql）

```sh
cd crates/tree-sitter-cql
tree-sitter generate    # 由 grammar.js 生成 src/parser.c 等
tree-sitter test        # corpus 测试，当前 43/43 通过
tree-sitter highlight --check <file.cql>   # 校验 queries/highlights.scm（如配置了色表）
```

corpus 位于 `test/corpus/*.txt`（01_lexical ～ 05_properties）。grammar.js 改动后
**必须重新 `tree-sitter generate`**，否则 Rust binding 仍编译旧的 parser.c。

## 4. VSCode 插件（editors/vscode-cql）

```sh
cd editors/vscode-cql
npm install --no-audit --no-fund   # typescript、@types/vscode、web-tree-sitter 等
npm run compile                    # 等价于 npx tsc -p ./
```

调试：在 VSCode 中打开 `editors/vscode-cql` 目录后按 **F5**（配置见
`.vscode/launch.json`，启动 Extension Development Host），打开任意 `examples/**/*.cql`
查看高亮。

**wasm 语法包（当前挂起）**：插件期望根目录有 `tree-sitter-cql.wasm`；本机无
emscripten/Docker，该产物**尚未构建且未入库**，插件在无 wasm 时优雅降级（仅基础
编辑器特性，semantic tokens 不启用）。构建步骤（待 emscripten 环境）：

```sh
cd crates/tree-sitter-cql
tree-sitter build --wasm            # 产出 tree-sitter-cql.wasm
cp tree-sitter-cql.wasm ../../editors/vscode-cql/
```

`grammar.js` 变化后需重新构建 wasm。

## 5. 平台已知问题（gotchas）

- **Windows UAC 误拦**：测试二进制文件名含 "update" 时会被 Windows 以 error 740
  （需要提升权限）拒绝启动。约定：测试二进制/target 名中避免 "update"；`cqlc` 生成
  crate 时也会把包名中的 `update` 改写为 `upd`（见 `crates/cql-cli/src/project.rs`
  的 `cargo_pkg_name`）。
- **nightly rustc 误报 `unused_assignments`**：`miette` derive 宏生成的代码会触发
  nightly 编译器对 `diag::CqlError` 字段的 `unused_assignments` 误报；已在
  `crates/cql-compiler/src/lib.rs` 用模块级 `#[allow(unused_assignments)]` 处理，
  勿删。
- **z3 gh-release 403**：见 §2.1。

## 6. 常用组合（日常开发循环）

```sh
cargo test --workspace --offline        # Rust 测试全绿
cd crates/tree-sitter-cql && tree-sitter test   # 语法 corpus
./target/debug/cqlc check examples/shop_project # 示例工程冒烟
```
