# 后端与工具生态

本文介绍 Rust 后端产物、mududb 后端（占位）、模型检查、VSCode 插件与 tree-sitter
语法开发。架构权威文档：[../codegen-backend.md](../codegen-backend.md)、
[../backend-mududb.md](../backend-mududb.md)、[../model-check.md](../model-check.md)。

## 1. Rust 后端（MVP，可用）

`cqlc build` 把每个 CQL 模块渲染成一个 Rust 源文件（askama 模板骨架 + 递归
emitter），全部后端只消费 CIR，不回读 AST。

### 生成物内容

- **State 结构**：每张表一个字段 `pub <table>: MemTable<TableKey, TableRow>`；
  `State::new()` 构造空库。
- **原子应用 `State::apply(&mut self, ops: &CqlSet<WriteOp>)`**：克隆到类型擦除的
  `TableRegistry` → `apply_write_ops`（冲突 → FK → invariant 校验，clone-apply-
  check-swap）→ 成功后回写，失败返回 `ApplyError` 且状态不变（§5.2 原子性）。
- **行/键结构体**：每表生成 `<Table>Row` / `<Table>Key`（实现 Eq/Hash/CanonOrd）。
- **结构记录类型**：`{ key: string, agg: float }` 这类结构记录生成为
  **按字段集散列 interned 的 `Rec_<hash>` struct**（字段按名排序），同构记录共享
  同一 Rust 类型。
- **`CanonOrd`**：所有可散列类型实现规范顺序全序（§2.3），集合/袋/map 的物化
  顺序由此保证确定性。
- **`CqlF64`**：`f64` 不满足 `Eq + Hash`，集合元素为 float 时用 `CqlF64(pub f64)`
  包装（按位模式散列）。
- **enum**：同名 Rust enum（自递归载荷自动 Box）；`T -> U` 为
  `Rc<dyn Fn(T) -> U>`（lambda lifting 产物）。
- CQL `test` 块生成 `#[test]`（`cql_tests` 模块），fixture → 内存表，
  `expect` → 断言。
- **已知 MVP 偏差**：`ReadPlan::IndexScan` 当前编译为带过滤的全扫（运行时保留
  二级索引接口），结果语义不受影响；泛型 enum 实例化、泛型类型别名实例化、
  跨模块 query/action 调用、跨模块泛型函数调用、导入函数的一等使用均报
  "not supported by codegen MVP" 类诊断。

### 消费生成的 crate

`out_dir`（默认 `target/cql`）是一个独立 cargo crate：`Cargo.toml` 以绝对路径
依赖 `cql-runtime` 并带空 `[workspace]`（不被上层 workspace 吸收）；`src/lib.rs`
聚合 `pub mod <module>;`。宿主代码 `use <pkg>::<module>::{State, ...}` 即可：
构造 `State`、调用生成的 query 函数、对 action 返回的 `CqlSet<WriteOp>` 调
`state.apply(&ops)`。

### CQL 类型 → Rust ABI 映射

| CQL 类型 | 生成的 Rust |
| --- | --- |
| `bool` / `int` / `float` / `string` | `bool` / `i64` / `f64` / `String` |
| `date` / `decimal(m, n)` | `cql_runtime::Date` / `cql_runtime::Decimal` |
| `option` / `vector` / `set` / `bag` / `map` | `Option` / `Vec` / `CqlSet` / `CqlBag` / `CqlMap` |
| 元组 | Rust 元组 |
| 记录（结构类型） | interned `Rec_<hash>` struct |
| 表行 / 键 | `<Table>Row` / `<Table>Key` |
| enum | 同名 Rust enum（自递归载荷 Box） |
| `T -> U` | `Rc<dyn Fn(T) -> U>` |
| `write_op` | `cql_runtime::WriteOp`（类型擦除，§3.6） |

注：§6.2 规定 `date`/`decimal` 在 ABI 层为 record；Rust 后端保留运行时的原生
newtype（ABI 映射只在 WASM 组件边界才重要）。

## 2. mududb 后端（占位，提案态）

`cqlc build --backend mududb` 不构建任何组件，只为每个模块输出一份
`<module>.mududb-plan.txt`（`cql_compiler::mududb_be::MududbBackend` 实现
`Backend` trait）。计划文本含三部分：

1. **组件接口骨架**：`component <mod> { import <table>: table<(s64, record {...})>;
   import syscalls: mududb_syscall_v1; export <op>: <sig> }`——CQL 类型按 §6.2 ABI
   映射（`int`→`s64`、`vector`→`list` 等），参数化算子的参数追加在导出签名尾部；
2. **每算子的 syscall 调用序列骨架**：query 为
   `session_open → snapshot_begin → 读（tbl_get/tbl_scan，标注 read 计划）→
   session_close`；action 为 `session_open → txn_begin → cmd_insert/cmd_update →
   txn_commit → session_close`（FK/invariant 由内核侧强制）；
3. **PROPOSAL 状态声明**：文件头尾均标注 syscall 名称为占位、**不含任何 syscall
   编号/签名**，待 `mududb_p/doc/lang.common` 对齐。

阅读 `.mududb-plan.txt` 时把它当作「部署蓝图草案」：接口形状和调用顺序可讨论，
但任何字段都不是稳定契约。背景与完整提案见
[../backend-mududb.md](../backend-mududb.md)（尤其 §3 syscall 契约提案与 §9 已知
缺口）。

## 3. 模型检查（cqlc verify）

架构：编译器侧 `mc_lower`（脱糖 AST → `cql-mc` 的 checker 无关 IR `McSpec`）→
Stateright 显式状态后端（z3 后端存在但本构建未启用）。

**v1 片段规则**（片段外构造产出 "not supported in the model-checking fragment"
诊断，退出码 2）：

- 表达式仅 bool/int；表 = **int 键 ⇀ 单个 int 值字段**（非 int 字段被忽略并告警）；
- 单模块工程（v1 不支持多模块 verify）；
- 初始状态仅来自 `test` 块 fixture；action 参数域由 `verify.toml [domain]` 推断；
- action 体仅支持受限形态（`match lookup(...)` 守卫 + `if ... then set{...} else
  set {}`、write_op 构造）；泛型/递归 action 不支持。

**映射**：action → 带守卫的迁移（guard + updates + param_domains），
`invariant`/`[](φ)` → `PropertyKind::Always`，`<>(φ)` → `Eventually`，
`fold(to_vector(table), ...)` → Sum/展开量词。

**拒绝/跳过（告警而非错误）**：record/string 等片段外表字段（忽略）、prime（
次态）、`~>`（leads-to）、`until`、裸 prime——这些 property 被跳过并在输出中
告警，不计入结论。

**Stateright 后端**：对有限有界模型做穷尽 BFS 枚举——安全性无反例即
`PROVED(stateright-exhaustive)`（对有界模型是证明）；有反例输出最短路径反例
（BFS）。退出码 0/1/2 见 [cqlc 工具与配置文件](cli.md)。

**bank_project 演练**：

```text
$ ./target/debug/cqlc verify examples/bank_project
verifying `bank` (stateright): 1 table(s), 1 transition(s), 2 of 2 propert(ies), k=8
  PROVED(stateright-exhaustive) balance_conserved
  PROVED(stateright-exhaustive) no_negative
result: all 2 propert(ies) hold within the bounds
```

`balance_conserved`（`[](total_balance() = 10000)`）与 `no_negative` 在
`verify.toml` 给定的界（`accounts.rows = 2`、`id ∈ 1..2`、
`balance ∈ {0, 6000, 4000}`、k=8）内穷举通过；含 prime 的 `transfer_preserves`
被跳过（Stateright 后端不支持次态）。把一个 update 改成多扣金额即可得到
`COUNTEREXAMPLE` 与最短反例迹（每步 action/参数/applied-rejected/状态差）。

## 4. VSCode 插件（editors/vscode-cql）

- **功能**：`DocumentSemanticTokensProvider` 提供语义高亮——web-tree-sitter 加载
  wasm 语法解析文档，`queries/highlights.scm` 的捕获映射到 **13 种 token 类型
  （keyword/type/typeParameter/function/variable/parameter/property/enumMember/
  namespace/string/number/comment/operator）× 4 种修饰符（declaration/readonly/
  builtin/escape）**；编辑经 `tree.edit(...)` 字节级增量重解析（incremental
  reparse）。
- **优雅降级**：扩展根目录缺 `tree-sitter-cql.wasm` 时，只在 CQL 输出通道记一条
  提示并照常激活（无 semantic tokens；括号配对/注释/缩进等基础功能来自
  `language-configuration.json`，不依赖 wasm）。
- **构建与调试**：`npm install --no-audit --no-fund && npm run compile`（tsc），
  VSCode 打开该目录按 F5（`.vscode/launch.json`）启动 Extension Development Host。
- **wasm 构建挂起**：需要 emscripten（本机无 emcc/Docker），步骤（
  `tree-sitter build --wasm` → 复制产物到扩展根目录）见插件 README 与
  [构建指南](build.md) §4；`grammar.js` 变更后需重新构建 wasm。

## 5. tree-sitter 语法开发（crates/tree-sitter-cql）

- `grammar.js` 实现 doc/cql.md 附录 A.1/A.2 文法；歧义优先用 `prec` 与文法顶部的
  `conflicts` 声明处理（如 property 体内 `[]` 的时态解读 vs 空 vector 字面量，
  见 grammar.js 中相应注释）；确实难表达的（如 `ident <` 泛型 vs 比较）在
  `lower.rs` 阶段消解。
- 工作循环：改 `grammar.js` → `tree-sitter generate` → `tree-sitter test`
  （corpus 43/43）；高亮查询用 `tree-sitter highlight` 校验。
- corpus 布局：`test/corpus/01_lexical.txt`（词法）`02_declarations.txt`（声明）
  `03_expressions.txt`（表达式）`04_queries_actions.txt`（查询/动作）
  `05_properties.txt`（时态性质），每例 `===` 标题 + 源码 + 期望 S-expression。
