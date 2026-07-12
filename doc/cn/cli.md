# cqlc 工具与配置文件

`cqlc` 是 CQL 工具链的命令行入口（crate `crates/cql-cli`，clap 子命令）。本文所有
命令均在仓库根目录执行（Windows + Git Bash，路径用正斜杠即可）；构建：

```sh
cargo build -p cql-cli --offline   # 产物 ./target/debug/cqlc
```

## 1. 目标解析规则（全部子命令共用）

`cqlc <cmd> [path]`（path 缺省 = 当前目录）：

1. 从 `path` 向上找到 `cql.toml` ⇒ **工程模式**：编译 `source_root` 下全部 `**/*.cql`；
2. 未找到且 `path` 是 `.cql` 文件 ⇒ **单文件零配置模式**：按独立模块编译（out_dir
   固定为当前目录 `target/cql`，后端固定 `rust`）；
3. 否则报错并提示 `cqlc new <name>`。

## 2. 子命令

### `cqlc new <name>` — 脚手架

在当前目录创建 `<name>/cql.toml` + `<name>/src/main.cql`（目录已存在则报错）。

```text
$ ./target/debug/cqlc new demo
created CQL project `demo` at .../demo
```

生成的 `src/main.cql` 含一个 `query hello() -> string` 模板。

### `cqlc check [path]` — 检查

跑完整流水线（解析 → … → 优化），只报诊断。退出码：成功 0，有编译错误 1。

```text
$ ./target/debug/cqlc check examples/shop_project
check passed: `shop` (2 module(s), 0 warning(s))

$ ./target/debug/cqlc check examples/analytics.cql     # 单文件零配置
check passed: `analytics` (1 module(s), 0 warning(s))
```

诊断经 miette 图形化渲染（含源码片段、行列、help 与规范章节引用）：

```text
  x operands of `+` must have the same numeric type, found `int` and `string`
   ,-[bad.cql:6:25]
 6 |     read(t, lambda(x) { x.id + "str" })
   :                         ^^^^^^|^^^^^
   `----
  help: no implicit conversions; use `as` to convert (§2.4)
error: could not compile `bad` due to 2 error(s)
```

### `cqlc build [path] [--backend rust|mududb]` — 代码生成 + 构建

check 通过后按依赖拓扑序逐模块生成代码到 `out_dir`，再执行
`cargo build --offline`。`--backend` 覆盖 `cql.toml` 设置；未知后端报错。

- `rust`（默认）：写出一个独立 cargo crate（`Cargo.toml` 绝对路径依赖
  cql-runtime + 空 `[workspace]`；`src/lib.rs` + 每模块一个 `src/<module>.rs`），
  然后 cargo build。输出示例：

```text
$ ./target/debug/cqlc build examples/shop_project
generated Rust crate for `shop` (2 module(s): util, shop) at .../examples/shop_project/target/cql
cargo build succeeded (.../examples/shop_project/target/cql)
```

- `mududb`：提案态占位，每个模块写一个 `<module>.mududb-plan.txt`（部署计划
  文本，见 [后端与工具生态](backends.md)），**不做组件构建**：

```text
$ ./target/debug/cqlc build examples/shop_project --backend mududb
generated mududb deployment plan (PROPOSAL) for `shop` (2 module(s): util, shop) at ...
note: syscall contract is a proposal — no component build yet (doc/backend-mududb.md §9)
```

退出码：成功 0；编译/生成/cargo 失败 1。

### `cqlc test [path]` — 运行 CQL test 块

与 build 相同地生成 crate，然后 `cargo test --offline`（CQL `test` 块被编译为
Rust `#[test]`，fixture 按主键构造内存表，`expect` 用谓词相等比较）。要求 rust
后端。退出码：成功 0，失败 1。

```text
$ ./target/debug/cqlc test examples/bank_project
...
running 1 test
test bank::cql_tests::test_transfer_basic ... ok

cargo test succeeded (.../examples/bank_project/target/cql)
```

### `cqlc verify [path] [flags]` — 模型检查

mc_lower 将脱糖后的 AST 降到 `cql-mc` 的 McSpec（v1 片段：bool/int 表达式 +
int 键、单 int 值字段的表），由 Stateright 显式状态后端逐条性质给出结论。
**仅工程模式**（单文件不支持）。退出码：

| 码 | 含义 |
| --- | --- |
| 0 | 全部性质在界内成立 |
| 1 | 发现反例（或前端编译错误） |
| 2 | verify.toml 无效 / mc_lower 失败（片段外构造） |

标志：

| 标志 | 说明 |
| --- | --- |
| `--bounded` | 只检查有界层性质（Always：invariant/安全性） |
| `--temporal` | 只检查时态性质（Eventually）；与 `--bounded` 同时给或都不给 = 全部 |
| `--depth N` | 覆盖递归深度界默认（默认 32） |
| `--trace N` | 覆盖迹长度界 k（默认 8） |
| `--engine stateright\|z3` | 默认 stateright；**z3 在本构建不可用**（需要 `z3` feature 与预编译 Z3，doc/model-check.md §7.3），选 z3 直接报错 |
| `--replay <case>` | 反例重放（生成 test 块）——**未实现**，给出即报错 |

输出示例（`examples/bank_project`）：

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

- 结论逐性质一行：`PROVED(stateright-exhaustive)`（穷尽有界模型即证明）/
  `COUNTEREXAMPLE` / `EventuallyHolds` 等（doc/model-check.md §7.2）。
- 反例附最短路径（BFS），每步渲染 action、参数、结果（applied/rejected）与状态差，
  由 `cql-mc` 的统一 Counterexample 格式输出。
- 表述纪律：`PROVED` 仅指**有界模型内**的证明；prime/`~>`/`until` 性质被跳过
  （告警，不计入结论）。

### `cqlc clean [path]` — 清理

删除 out_dir（不存在则提示 `nothing to clean`）。

## 3. `cql.toml` 参考

```toml
[package]
name = "shop"            # 必填；也用作生成 crate 的包名（非标识符字符转义，
                         # 含 "update" 会被改写为 "upd" 以避开 Windows UAC）
version = "0.1.0"        # 可选，默认 "0.1.0"；目前无子命令使用

[build]
source_root = "src"        # 源码根目录，默认 "src"
out_dir = "target/cql"     # 生成代码目录（相对工程根），默认 "target/cql"
backend = "rust"           # "rust"（默认）| "mududb"（提案态占位）

[mududb]                   # backend = "mududb" 时使用（草案，backend-mududb.md §8）
app = "shop"
sql_adapter = "off"        # "off"（默认）| "sqlite" | "postgres" | "mysql"
```

## 4. `verify.toml` 参考

放在工程根（与 `cql.toml` 同级）；缺失则全部默认。

```toml
[depth]
default = 32          # 递归深度界默认（D）
subordinates = 16     # 按算子名覆盖（等价于源码 with depth 标注）

[domain]
accounts.rows = 2                 # 行数界（v1 接受但忽略：初始状态只来自 fixture）
accounts.id = "1..2"              # 键/字段域："lo..hi" 区间
accounts.balance = [0, 6000, 4000]  # 或显式值集合（仅整数值）

[trace]
length = 8            # 迹长度界 k（默认 8）

[fairness]
weak = []             # weak/strong：v1 被接受但无后端强制，仅告警
```

**v1 实际生效情况**：

- `[depth]`（默认与按算子覆盖）：生效；
- `[domain] table.field`：生效——用于 action 参数域推断（如 `from_id`/`to_id` 取
  键域、`amt` 取 `accounts.balance` 域）；值仅支持整数（区间串或整数数组）；
- `[domain] table.rows`：被接受但**忽略**（v1 只从 `test` 块的 fixture 取初始状态）；
- `[trace] length`：生效（输出中的 `k=`）；
- `[fairness]`：声明即告警「no backend enforces fairness yet」，全部迹照常枚举。
