# CQL 语言教程

一篇可读的语法导览，带可编译的小例子（风格取自 `examples/`）。**规范性细节以
[../cql.md](../cql.md) 为准**（本文各处给出对应章节号）。时态性质部分另见
[../model-check.md](../model-check.md)。

## 1. 模块与工程（[../cql.md](../cql.md) §3.1）

每个 `.cql` 文件是一个模块，首行声明模块名：

```text
module shop;

use util;                       // 导入 util 模块的全部 public 项（不加限定名）

table users { id: int, name: string, city: string } primary key {id}

query large_orders() -> set<orders> == {
    read(orders, lambda(o) { is_large_amount(o.amount) })   // is_large_amount 来自 util
}
```

- 工程由 `cql.toml` 界定（见 [cqlc 工具与配置文件](cli.md)），`cqlc` 按 `use` 建
  依赖图、拓扑序编译，禁止循环依赖。
- `public` 导出；未标注的声明仅模块内可见。跨模块只能导入 public 的类型别名/
  enum/纯函数/常量；**表不可跨模块**（schema 与直接访问它的 query/action 放在同一
  模块）。
- **MVP 限制**：`use` 只支持单段名（`use util;`，多级路径 `use a::b` 未实现）；
  导入项全部不加限定名使用；`use util as m;` 别名写法保留给未来，当前只给出
  警告。跨模块泛型函数调用、跨模块 query/action 调用暂不支持（编译期报错）。

## 2. 类型（§2）

基础类型：`bool`、`int`（i64）、`float`（f64）、`string`（UTF-8）、
`decimal(m, n)` 任意精度定点（可略为无界 `decimal`）、`date`。

容器与复合类型：

```text
option<int>                     // 可能缺失；构造子 some(x) / none
vector<int>                     // 有序序列，[1, 2, 3]
set<string>                     // 无序去重；set {1, 2}，空集 set {}（元素须 hashable）
bag<float>                      // 多重集；bag {1.0, 2.0, 2.0}（只要求 eq，可装 float）
map<string, int>                // 纯关联值；map { "a": 1 }
(int, string)                   // 元组；投影 t.0 / t.1
{ id: int, name: string }       // 记录类型（结构类型，按字段集合等价）
int -> int                      // 纯函数类型（右结合）
```

- **表派生类型**（§2.2）：`table users { ... }` 自动派生三个类型——`users`（全字段
  行类型）、`key users`（键类型，复合键为元组）、`value users`（非键字段记录）。
  `lookup(users, k)` 返回 `option<value users>`。
- **enum**（§3.2）：`enum shape { circle(float), rect(float, float) }`，变体可带
  多载荷、可泛型、可递归。
- **泛型与 turbofish**：`function map<A, B>(xs: vector<A>, f: A -> B) -> vector<B>`；
  显式实参用 `f::<int>(x)`（表达式位 `ident <` 恒按比较解析）。
- 无隐式转换；`as` 转换白名单见 §2.4（如 `int as float`、`decimal(10,2) as int`）。

## 3. 声明速览（§3）

```text
const max_retries: int == 3;                          // 编译期常量
type user_id == int;                                   // 类型别名
table orders { order_id: int, user_id: int, amount: float }
    primary key {order_id}
    foreign key {user_id} references users             // 表 + 键约束（§3.3）
index sessions_by_user on sessions(user_id)            // 二级索引（非唯一）

function is_adult(age: int) -> bool == { age >= 18 }   // 纯函数（L0）
query orders_by_user(user_id: int) -> set<orders> == { // 查询（L1，读快照）
    read(orders, lambda [user_id](o) { o.user_id = user_id })
}
action add_user(id: int, name: string, city: string) -> set<write_op> == {  // 动作（L2）
    set { insert(users, record { id: id, name: name, city: city }) }
}

invariant non_negative on orders == \A o \in orders : o.amount >= 0.0
property balance_ok == [](total_balance() = 10000)     // 时态性质（model-check.md §4.1）
fairness weak == transfer                              // 公平性声明（v1 仅告警不强制）

test transfer_basic {                                  // 测试块（附录 C）
    fixture accounts == [record { id: 1, owner: "a", balance: 6000 }];
    expect total_balance() == 6000;
}
```

要点：定义一律 `==`，谓词相等用 `=`；算子体**一律为块** `{ ... }`；声明末尾分号
可选，块内 `let` 语句分号必选。

## 4. 效应层级（§3.7）

| 层级 | 构造 | 允许的效应 |
| --- | --- | --- |
| L0 | `function` | 无（纯） |
| L1 | `query` | 读快照（`read`/`lookup`） |
| L2 | `action` | 读快照 + 产出 `set<write_op>`（`insert`/`update`/`delete`） |

- 调用图中层级只能持平或上升：`function` 只能调 `function`；`query` 可调
  `function`/`query`（共享同一快照）；`action` 可全部调用（被调 action 的 write_op
  集合并入，原子性只在顶层）。反向调用是编译错误。
- `read`/`lookup` 只能直接出现在 query/action 体；`insert`/`update`/`delete` 只能
  在 action 体；**lambda 体恒为 L0**（read 谓词、aggregate 回调等必然纯）。
- 读位（读原语位置）：`read`/`lookup` 与生成器/量词源位（表名糖）；写位
  （写构造位置）：action 体内的 write_op 构造。

## 5. 表达式（§4）

```text
-- 块与 let（块内 let 分号必选；块的值 = 末位表达式）
{ let active == set { v \in users if v.active };
  set { u.name : u \in active } }

-- if / match（表达式；两分支类型须相同；match 穷尽性静态检查）
if f.balance >= amt then set { ... } else set {}
match lookup(users, id) { some(v) => v.name, none => "unknown" }

-- 集合推导（两形式，结果为 set<T> 去重）
set { x \in users if x.active }                        -- 过滤式（if 分隔）
set { (o.order_id, u.name) : o \in orders, u \in lookup(users, o.user_id) }  -- 映射式
bag { o.amount : o \in orders }                        -- 袋推导（保留重复）

-- 量词（源可为 set/bag/option/表名糖）
\A o \in orders : o.amount >= 0.0
\E u \in users : u.city = "x"

-- lambda：捕获列表必须显式列出引用的外层局部绑定；顶层声明不算捕获
lambda [new_city](v) { record { v with city: new_city } }
lambda(x: int) -> int { x + 1 }

-- 字符串内插（expr 须为基础类型）
"hello \(u.name), city: \(u.city)"

-- ? 传播糖：none 则整个算子体为 none（仅返回 option<T> 的算子/lambda 体内合法）
{ let u == lookup(users, user_id)?; some(u.city) }
```

其他规则：比较**不可链式**（`a < b < c` 非法）；`/\`、`\/` 短路；`=>` 为蕴含；
`e?`、命名实参（`group_key: lambda(r) { ... }`，须在位置实参之后）、方法调用糖
（`m.get(k)` ≡ `map_get(m, k)`）详见 §4.1/A.3。

## 6. 表、写操作与读计划（§3.3、§3.6、§5.2、§5.5）

- 主键恰一个且必现（复合键 `primary key {user_id, ts}`，lookup 用元组）；外键
  `foreign key {cols} references t` 携带运行时引用完整性约束，**不隐含索引、不引入
  JOIN 语义**；索引由 `index` 显式声明。
- `write_op` 三构造：`insert(t, row)`（键必须不存在）、`update(t, k, f)`（键必须
  存在，`f: value t -> value t`，对**应用时的当前行值**求值）、`delete(t, k)`
  （键不存在为 no-op）。同一 action 内同一 `(table, key)` 至多一个 write_op。
- 原子应用顺序：冲突检查 → FK 校验 → invariant 校验，任一违反**拒绝整个 action**
  （不应用任何写；是数据约束违例，不是 trap）。
- 读计划分类（只影响性能）：主键全列等值 ⇒ 点查（PointLookup）；某二级索引全列
  等值 ⇒ 索引（IndexScan）；否则全扫（FullScan），残余谓词在扫出的行上过滤。
  **MVP 说明**：IndexScan 当前编译为带过滤的全扫（运行时保留索引接口），结果语义
  不受影响。

## 7. 终止性（§3.4、§5.4）

双层制：

```text
function recursive inorder(t: tree) -> vector<int> == {   -- 结构递归：termination pass 证明终止
    match t {
        leaf(v)       => [v],
        node(l, x, r) => concat_vector(concat_vector(inorder(l), [x]), inorder(r))
    }
}                                                          -- 递归实参须是递归参数的严格子项；
                                                           -- decreases <param> 可显式指定递归参数

function gcd(a: int, b: int) -> int == {                   -- 一般递归：写法自由，运行时可栈溢出 trap
    if b = 0 then a else gcd(b, a % b)
}
query subordinates(mgr_id: int) -> set<int> with depth 32 == { ... }  -- 模型检查深度界
```

禁止互递归（对 `recursive` 算子）；结构检查失败给出改写提示（cons 递归 / fold /
降格一般递归）。

## 8. 性质与公平性（model-check.md §4）

`property` 体内的时态算子（TLA+ 记法）：

```text
property balance_conserved == [](total_balance() = 10000)   -- []：always
property eventually_done   == <>(\A o \in orders : o.paid)  -- <>：eventually
property pending_resolved  == (\E o \in orders : ~o.done) ~> (\A o \in orders : o.done)
property p_until           == a until b
property transfer_preserves == [](total_balance()' = total_balance())  -- prime：次态求值
```

- prime `e'` 只在 `property` 体内合法（次态 = 迁移后状态）。
- **v1 支持情况**：`[]` → Always、`<>` → Eventually 会被检查；prime、`~>`、`until`
  在 Stateright 后端**跳过并告警**（不计入结论）；`fairness weak/strong` 声明被
  接受但目前无后端强制（仅告警）。

## 9. 标准库速览（附录 B）

全部纯函数，可方法调用糖。常用（完整签名表见 [../cql.md](../cql.md) 附录 B）：

| 域 | 常用函数 |
| --- | --- |
| string | `contains` `starts_with` `length` `concat` `substring` `trim` `split` `join` `to_string_int` … |
| math | `abs` `min` `max` `floor` `ceil` `round` |
| decimal | `decimal_from_string` `round_to` `to_string_decimal` |
| date | `year` `month` `day` `add_days` `days_between` `parse_date` `day_of_week` |
| vector/迭代 | `fold` `map` `filter` `append` `to_vector` `sort_by` `take` `drop` `scan_left` `concat_vector` |
| set/bag | `size` `the`（单元素取值，否则 trap） `only` `union_all` `bag_to_set` `copies_in` |
| map | `map_get` `map_insert` `map_remove` `map_keys` `map_values` `map_size` `map_from_vector` |
| option | `map` `and_then` `unwrap_or` `is_some` `is_none` |
| 聚合（§4.8.3） | `aggregate`（内建组合子）及糖 `count_by` `sum_by` `avg_by` `min_by` `max_by` |

同名分派仅两处：`length`（string / vector）与 `map`（vector / option），按首参数
类型分派。

## 10. Trap 语义（§5.3）

CQL 以全函数为默认，剩余偏运算在运行时检查，失败即 **trap**（宿主映射为错误码）：

- `int` 除零/取模零、算术溢出（无环绕）；`float as int` 越界或 NaN；
- `decimal(m, n)` 运算结果超 m 位、`as` 转换越界；无界 `decimal` 无精度 trap
  （除零仍 trap）；
- `the(S)` 作用于非单元素集合；一般递归栈耗尽。
- `query` trap ⇒ 查询失败、无副作用；`action` trap ⇒ **不应用任何 write_op**。
- 可恢复的错误不属于 trap，用 `option` / `enum result` 显式表达。
