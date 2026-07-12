# 开发指南

面向贡献者的代码导览：workspace 结构、编译流水线、必须了解的设计不变量、常见扩展
点的做法与测试约定。语言语义以 `doc/cql.md`、代码生成架构以 `doc/codegen-backend.md`
为准；本文只指方向，不复述规范。

## 1. Workspace 结构

| Crate | 职责 | 关键模块/文件 |
| --- | --- | --- |
| `crates/tree-sitter-cql` | tree-sitter 语法 + Rust binding | `grammar.js`（文法）、`src/parser.c`（生成物）、`test/corpus/*.txt`（43 个 corpus 用例） |
| `crates/cql-compiler` | 编译器库（前端 → 中端 → 代码生成） | `ast/`（每个 AST struct 一个文件，`mod.rs` re-export）、`lower.rs`（CST→AST）、`resolve.rs`、`effect.rs`、`types.rs`、`terminate.rs`、`desugar.rs`、`optimize.rs`（read 计划分类）、`pipeline.rs`（单模块流水线）、`project.rs`（多模块工程）、`cir.rs`（CIR lowering）、`codegen.rs`（Backend trait + Rust 后端）、`mududb_be.rs`（mududb 占位后端）、`mc_lower.rs`（脱糖 AST → McSpec）、`diag.rs`（miette 诊断） |
| `crates/cql-runtime` | 生成代码依赖的运行时 | `value.rs`（Date/Decimal/CanonOrd/Value）、`collections.rs`（CqlSet/CqlBag/CqlMap）、`table.rs`（Table trait、MemTable）、`write.rs`（WriteOp、TableRegistry、原子应用）、`trap.rs`（Trap、checked 算术）、`stdlib/`（附录 B 纯函数：string/math/decimal/date/vector/set_bag/map/option/aggregate） |
| `crates/cql-mc` | 模型检查器（checker 无关 IR + 双后端） | `ir.rs`（McSpec）、`eval.rs`（具体求值器）、`encode.rs`/`z3_be.rs`（z3 后端，feature `z3`）、`stateright_be.rs`（显式状态后端，feature `stateright`）、`counterexample.rs`（Verdict/Counterexample 统一格式） |
| `crates/cql-cli` | 二进制 `cqlc`（clap 子命令） | `main.rs`（new/check/build/test/verify/clean）、`manifest.rs`（cql.toml/verify.toml 解析）、`project.rs`（目标解析、代码生成落地、cargo 调用） |
| `editors/vscode-cql` | VSCode 高亮插件（TypeScript） | `src/extension.ts`（semantic tokens）、`queries/highlights.scm`、`language-configuration.json` |

## 2. 编译流水线

```text
parse (tree-sitter → lower.rs)
  → resolve      名字解析、可见性、use 图、lambda 捕获校验
  → effect       L0/L1/L2 效应层级检查（doc/cql.md §3.7）
  → types        双向局部推断、表派生类型、as 白名单、match 穷尽性
  → terminate    结构递归子项检查、SCC 拒绝互递归
  → desugar      表面语法 → ~12 节点核心语言（doc/cql.md 附录 D.2）
  → optimize     read 谓词计划分类（点查/索引/全扫），写入 ReadPlan 侧表
  → CIR (cir.rs) lambda lifting、模式编译、单态化、read 计划物化
  → Backend trait (codegen.rs)
        ├─ RustBackend     askama 模板 + 递归 emitter → Rust 源码（依赖 cql-runtime）
        └─ MududbBackend   CIR → 部署计划文本（提案态，doc/backend-mududb.md）

模型检查分支（cqlc verify）：
  脱糖后 AST → mc_lower.rs → cql-mc McSpec → stateright_be（显式状态枚举）
```

单模块入口：`pipeline::compile_module`；多模块工程入口：
`project::compile_project`（按 `use` 依赖图拓扑序编译，每模块携带依赖的
`ModuleInterface`）。

## 3. 关键设计不变量（改代码前必读）

1. **单一 Expr/ExprKind**：所有 pass 共享同一个 `ast::Expr`，变体分三类——表面节点
   （脱糖消除）、核心节点、解析节点（resolve/effect 产出，如 resolved call）。
2. **侧表（side table）以 Span 为键**：名字解析结果、表达式类型
   （`expr_tys`）、泛型实例化、read 计划（`ReadPlan`）都存在按 `Span` 索引的侧表里，
   不回写 AST。新 pass 的元数据请沿用此模式。
3. **诊断经 DiagBag，算子级错误隔离**：pass 返回 `(T, DiagBag)` 或
   `Result<T, DiagBag>`；某个算子出错不应阻断同模块其他算子的检查（错误隔离在算子
   粒度）。诊断统一带 span，经 miette 图形化渲染。
4. **效应层级只升不降**：L0 `function` / L1 `query` / L2 `action`；被调算子层级
   高于调用方即编译错误（callee.level > caller.level = error）。lambda 体恒为 L0。
   `read`/`lookup`/`insert`/`update`/`delete` 是按名识别的内建效应原语（保留名）。
5. **后端只消费 CIR**：禁止后端回读 AST；类型/计划信息以标注形式随 CIR 传递
   （doc/codegen-backend.md §6「明确不做」）。
6. **McSpec v1 片段**：bool/int 表达式 + int 键、单个 int 值字段的表；片段外构造
   产出「not supported in the model-checking fragment」诊断（属 lowering 错误，
   `cqlc verify` 退出码 2），不是静默忽略（property 级不可支持的如 prime/`~>`/
   `until` 则是跳过并告警）。
7. **确定性优先**：集合/袋/map 物化一律按规范顺序（CanonOrd）；生成代码不得引入
   哈希序迭代等非确定性。

## 4. 常见扩展点

### 4.1 新增标准库函数（纯函数）

1. `crates/cql-runtime/src/stdlib/<域>.rs` 实现 Rust 侧函数（参照同域既有函数）；
2. `crates/cql-compiler/src/types.rs` 的内建签名表中加入 CQL 签名（含泛型方案）；
3. 脱糖/代码生成映射：`desugar.rs`（若是语法糖）或 `codegen.rs` 的调用发射处
   映射到运行时函数（方法糖 `recv.f(x)` 按首参数分派，无需额外工作）；
4. 文档同步：`doc/cql.md` 附录 B 是签名权威表，新增函数要补进去。

### 4.2 新增后端

1. 实现 `codegen::Backend` trait（`name()` + `emit(&CirModule, &EmitCtx)`）；
   最小示例看 `mududb_be.rs`（CIR → 纯文本，约一个文件的骨架）；
2. 目标语言缺闭包/泛型/match 时，在共享 lowering 之后追加**目标专属归一 pass**，
   不得修改 CIR 定义；
3. 在 `crates/cql-cli/src/project.rs` 按 `cql.toml [build] backend` 名注册分派；
4. 端到端差分测试：同一 examples 模块经新后端与 rust 后端结果一致。

### 4.3 新增诊断

在相应 pass 里 `bag.push_error(CqlError::new(src, span, message, help))`（告警用
`push_warning`）。`help` 一行尽量给出改写方向。带"代码引用规范章节"是惯例（如
`help: ... (§2.4)`）。新增错误类别若影响 miette derive，注意 §lib.rs 的
`unused_assignments` allow。

## 5. 测试约定

- **单元测试**：各 pass 文件内 `#[cfg(test)] mod tests`，直接构造源码字符串跑
  pass 并断言诊断/侧表。
- **集成测试**：`crates/*/tests/` 目录。
- **端到端生成代码测试**：在 `target/tmp/` 下搭 scratch cargo 工程，编译并运行
  生成的 crate（`cargo test --offline`），验证运行结果。
- **CLI 测试**：用 `env!("CARGO_BIN_EXE_cqlc")` 调用二进制；fixture 一律**复制**
  examples 到 `target/tmp/` 下的新目录再操作，绝不弄脏 `examples/`。
- 测试二进制名避免含 "update"（Windows UAC error 740，见构建指南 §5）。

## 6. 调试技巧

- 不需要 cargo expand：诊断本身经 miette fancy 渲染（带源码片段、行列、help），
  优先读诊断；想看中间态时，各 pass 的输入输出都是不可变 AST，单元测试里直接
  `format!("{x:#?}")` 打印即可。
- 临时探针：本仓库 agents 的惯例是在 crate 里加临时 `dbg_*.rs` 探针测试定位
  问题，**提交前删除**。
- tree-sitter 解析问题：先在 `crates/tree-sitter-cql` 用
  `tree-sitter parse <file>` / `tree-sitter test` 复现，再看 `lower.rs` 的 CST→AST
  映射；歧义优先在 `grammar.js` 用 `prec`/`conflicts` 解决（doc/todo.md 关键技术决策）。
