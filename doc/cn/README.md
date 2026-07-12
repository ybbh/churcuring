# CQL 工具链文档（简体中文版）

CQL（Churcuring Query Language）是一种声明式、强类型的查询与业务逻辑语言：借鉴 TLA+
的集合推导、量词与定义结构，采用 Rust 风格命名，**完全不使用 JOIN**——一切跨表关联都
通过显式 `lookup` 完成。本仓库是 CQL 的完整工具链：tree-sitter 语法、编译器库
（解析 → 名字解析 → 效应检查 → 类型检查 → 终止性 → 脱糖 → 优化 → CIR → 代码生成）、
运行时库、模型检查器（Stateright / z3 后端）、`cqlc` 命令行工具，以及 VSCode 高亮插件。

## 文档目录

### 用户与开发指南（本目录）

| 文档 | 内容 |
| --- | --- |
| [构建指南](build.md) | 环境准备、构建/测试命令、离线（air-gapped）构建注意事项、平台已知问题 |
| [开发指南](development.md) | workspace 结构、编译流水线、关键设计不变量、如何添加标准库函数/后端/诊断、测试约定 |
| [CQL 语言教程](cql-tour.md) | 语法导览：模块、类型、声明、效应层级、表达式、表、终止性、时态性质、标准库、trap |
| [cqlc 工具与配置文件](cli.md) | `new`/`check`/`build`/`test`/`verify`/`clean` 子命令、`cql.toml` 与 `verify.toml` 参考 |
| [后端与工具生态](backends.md) | Rust 后端产物、mududb 后端（占位）、模型检查、VSCode 插件、tree-sitter 语法开发 |

### 设计规范（权威文档，语言语义以这些文件为准）

| 文档 | 内容 |
| --- | --- |
| [../cql.md](../cql.md) | CQL 语言规范（类型系统、模块、表达式、语义规则、附录 A–D） |
| [../model-check.md](../model-check.md) | 形式化模型检查机制（有界层/时态层、双后端架构、verify.toml） |
| [../backend-mududb.md](../backend-mududb.md) | mududb 后端：query/command syscall 契约（提案）与 SQL 通道 |
| [../codegen-backend.md](../codegen-backend.md) | 代码生成后端架构（CIR、Backend trait、新后端接入清单） |
| [../todo.md](../todo.md) | 实现计划与已批准的语法修订记录 |

> 仓库内代码注释以 `doc/cql.md §3.6` 这样的平铺路径引用设计文档，因此设计文档保持
> 在 `doc/` 根下，不在本目录内移动。

## 快速开始

前置条件：Rust nightly 1.94（本仓库无网络，所有 cargo 命令均带 `--offline`）。

```sh
# 1. 构建整个 workspace（生成 ./target/debug/cqlc）
cargo build --workspace --offline

# 2. 脚手架一个新工程
./target/debug/cqlc new demo

# 3. 类型/效应检查（单文件或工程目录均可）
./target/debug/cqlc check examples/shop_project

# 4. 代码生成 + cargo 构建（产物写入工程的 target/cql）
./target/debug/cqlc build examples/shop_project

# 5. 运行 CQL test 块（生成 #[test] 后 cargo test）
./target/debug/cqlc test examples/bank_project

# 6. 模型检查（Stateright 显式状态后端）
./target/debug/cqlc verify examples/bank_project
```

更多示例：`examples/analytics.cql`（单文件零配置）、`examples/shop_project`（多模块工程）、
`examples/bank_project`（带 `verify.toml` 的模型检查示例）。

## 当前实现状态速览

- 编译流水线全通（`cargo test --workspace --offline` 约 289 个测试全绿；tree-sitter
  corpus 43/43）。
- Rust 后端为 MVP（可用）；mududb 后端为占位骨架（只产出部署计划文本，见
  [后端与工具生态](backends.md)）。
- 模型检查 v1 片段：bool/int 表达式 + int 键、int 值表；`--engine z3` 与 `--replay`
  暂不可用（见 [cqlc 工具与配置文件](cli.md)）。
- VSCode 插件的 wasm 语法包尚未构建（缺 emscripten，插件会优雅降级，见
  [构建指南](build.md)）。
