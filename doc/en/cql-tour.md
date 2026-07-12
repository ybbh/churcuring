# CQL Language Tour

A readable syntax tour with small compilable examples (in the style of `examples/`).
**For normative details, [../cql.md](../cql.md) is authoritative** (section numbers are
given throughout). For temporal properties, also see [../model-check.md](../model-check.md).

## 1. Modules and projects ([../cql.md](../cql.md) §3.1)

Each `.cql` file is a module, declaring its name on the first line:

```text
module shop;

use util;                       // import all public items of module util (unqualified)

table users { id: int, name: string, city: string } primary key {id}

query large_orders() -> set<orders> == {
    read(orders, lambda(o) { is_large_amount(o.amount) })   // is_large_amount comes from util
}
```

- A project is delimited by `cql.toml` (see [cqlc CLI & Config Files](cli.md)); `cqlc`
  builds a dependency graph from `use`, compiles in topological order, and forbids cyclic
  dependencies.
- `public` exports; unannotated declarations are visible only within the module.
  Cross-module imports may only pull in public type aliases/enums/pure functions/constants;
  **tables cannot cross modules** (a schema and the queries/actions that access it
  directly live in the same module).
- **MVP limitation**: `use` supports only single-segment names (`use util;`, multi-level
  paths like `use a::b` are unimplemented); imported items are always used unqualified;
  the `use util as m;` alias form is reserved for the future and currently only produces
  a warning. Cross-module generic-function calls and cross-module query/action calls are
  not supported yet (compile error).

## 2. Types (§2)

Base types: `bool`, `int` (i64), `float` (f64), `string` (UTF-8), `decimal(m, n)`
arbitrary-precision fixed-point (can be elided to unbounded `decimal`), `date`.

Container and composite types:

```text
option<int>                     // possibly absent; constructors some(x) / none
vector<int>                     // ordered sequence, [1, 2, 3]
set<string>                     // unordered, deduplicated; set {1, 2}, empty set set {} (elements must be hashable)
bag<float>                      // multiset; bag {1.0, 2.0, 2.0} (only requires eq, can hold floats)
map<string, int>                // pure associative value; map { "a": 1 }
(int, string)                   // tuple; projections t.0 / t.1
{ id: int, name: string }       // record type (structural type, equality by field set)
int -> int                      // pure function type (right-associative)
```

- **Table-derived types** (§2.2): `table users { ... }` automatically derives three
  types — `users` (full-field row type), `key users` (key type; composite keys are
  tuples), `value users` (record of non-key fields). `lookup(users, k)` returns
  `option<value users>`.
- **enum** (§3.2): `enum shape { circle(float), rect(float, float) }`; variants may carry
  multiple payloads, be generic, and be recursive.
- **Generics and turbofish**: `function map<A, B>(xs: vector<A>, f: A -> B) -> vector<B>`;
  explicit arguments use `f::<int>(x)` (in expression position, `ident <` is always parsed
  as comparison).
- No implicit conversions; the `as` conversion whitelist is in §2.4 (e.g. `int as float`,
  `decimal(10,2) as int`).

## 3. Declarations at a glance (§3)

```text
const max_retries: int == 3;                          // compile-time constant
type user_id == int;                                   // type alias
table orders { order_id: int, user_id: int, amount: float }
    primary key {order_id}
    foreign key {user_id} references users             // table + key constraints (§3.3)
index sessions_by_user on sessions(user_id)            // secondary index (non-unique)

function is_adult(age: int) -> bool == { age >= 18 }   // pure function (L0)
query orders_by_user(user_id: int) -> set<orders> == { // query (L1, reads a snapshot)
    read(orders, lambda [user_id](o) { o.user_id = user_id })
}
action add_user(id: int, name: string, city: string) -> set<write_op> == {  // action (L2)
    set { insert(users, record { id: id, name: name, city: city }) }
}

invariant non_negative on orders == \A o \in orders : o.amount >= 0.0
property balance_ok == [](total_balance() = 10000)     // temporal property (model-check.md §4.1)
fairness weak == transfer                              // fairness declaration (v1: warned only, not enforced)

test transfer_basic {                                  // test block (appendix C)
    fixture accounts == [record { id: 1, owner: "a", balance: 6000 }];
    expect total_balance() == 6000;
}
```

Key points: definitions always use `==`, predicate equality uses `=`; operator bodies are
**always blocks** `{ ... }`; the trailing semicolon of a declaration is optional, while
`let` statements inside a block require a semicolon.

## 4. Effect tiers (§3.7)

| Tier | Construct | Permitted effects |
| --- | --- | --- |
| L0 | `function` | None (pure) |
| L1 | `query` | Read snapshot (`read`/`lookup`) |
| L2 | `action` | Read snapshot + produce `set<write_op>` (`insert`/`update`/`delete`) |

- Along the call graph, tiers may only stay level or ascend: a `function` can only call
  `function`s; a `query` can call `function`s/`query`s (sharing the same snapshot); an
  `action` can call everything (the write_op set of a called action is merged in;
  atomicity only at the top level). Calls in the reverse direction are compile errors.
- `read`/`lookup` may appear only directly in query/action bodies;
  `insert`/`update`/`delete` only in action bodies; **lambda bodies are always L0**
  (read predicates, aggregate callbacks, etc. are necessarily pure).
- Read positions (where read primitives may occur): `read`/`lookup` and
  generator/quantifier source positions (table-name sugar); write positions (where write
  constructs may occur): write_op constructs inside an action body.

## 5. Expressions (§4)

```text
-- blocks and let (let inside a block requires a semicolon; the block's value = its last expression)
{ let active == set { v \in users if v.active };
  set { u.name : u \in active } }

-- if / match (expressions; both branches must have the same type; match exhaustiveness is statically checked)
if f.balance >= amt then set { ... } else set {}
match lookup(users, id) { some(v) => v.name, none => "unknown" }

-- set comprehension (two forms; result is set<T>, deduplicated)
set { x \in users if x.active }                        -- filter form (separated by if)
set { (o.order_id, u.name) : o \in orders, u \in lookup(users, o.user_id) }  -- map form
bag { o.amount : o \in orders }                        -- bag comprehension (duplicates kept)

-- quantifiers (source may be set/bag/option/table-name sugar)
\A o \in orders : o.amount >= 0.0
\E u \in users : u.city = "x"

-- lambda: the capture list must explicitly name the referenced outer local bindings; top-level declarations are not captures
lambda [new_city](v) { record { v with city: new_city } }
lambda(x: int) -> int { x + 1 }

-- string interpolation (expr must be a base type)
"hello \(u.name), city: \(u.city)"

-- ? propagation sugar: on none the whole operator body becomes none (legal only inside operators/lambdas returning option<T>)
{ let u == lookup(users, user_id)?; some(u.city) }
```

Other rules: comparisons **cannot be chained** (`a < b < c` is illegal); `/\`, `\/`
short-circuit; `=>` is implication; `e?`, named arguments (`group_key: lambda(r) { ... }`,
must come after positional arguments), and method-call sugar (`m.get(k)` ≡
`map_get(m, k)`) are detailed in §4.1/A.3.

## 6. Tables, write operations, and read plans (§3.3, §3.6, §5.2, §5.5)

- Exactly one primary key, always required (composite key `primary key {user_id, ts}`,
  lookup takes a tuple); foreign keys `foreign key {cols} references t` carry a runtime
  referential-integrity constraint, **do not imply an index and introduce no JOIN
  semantics**; indexes are declared explicitly with `index`.
- Three `write_op` constructs: `insert(t, row)` (key must not exist), `update(t, k, f)`
  (key must exist, `f: value t -> value t`, evaluated against **the current row value at
  apply time**), `delete(t, k)` (missing key is a no-op). Within one action, at most one
  write_op per `(table, key)`.
- Atomic apply order: conflict check → FK validation → invariant validation; any violation
  **rejects the entire action** (no writes are applied; it is a data-constraint violation,
  not a trap).
- Read-plan classification (affects performance only): full-column equality on the primary
  key ⇒ point lookup (PointLookup); full-column equality on some secondary index ⇒ index
  scan (IndexScan); otherwise full scan (FullScan), with residual predicates filtered over
  the scanned rows. **MVP note**: IndexScan currently compiles to a filtered full scan
  (the runtime keeps the index interface); result semantics are unaffected.

## 7. Termination (§3.4, §5.4)

Two tiers:

```text
function recursive inorder(t: tree) -> vector<int> == {   -- structural recursion: the termination pass proves termination
    match t {
        leaf(v)       => [v],
        node(l, x, r) => concat_vector(concat_vector(inorder(l), [x]), inorder(r))
    }
}                                                          -- recursive arguments must be strict subterms of the recursion parameter;
                                                           -- decreases <param> can name the recursion parameter explicitly

function gcd(a: int, b: int) -> int == {                   -- general recursion: free-form, may trap on stack overflow at runtime
    if b = 0 then a else gcd(b, a % b)
}
query subordinates(mgr_id: int) -> set<int> with depth 32 == { ... }  -- model-checking depth bound
```

Mutual recursion is forbidden (for `recursive` operators); failed structural checks come
with rewrite hints (cons recursion / fold / downgrade to general recursion).

## 8. Properties and fairness (model-check.md §4)

Temporal operators (TLA+ notation) inside `property` bodies:

```text
property balance_conserved == [](total_balance() = 10000)   -- []: always
property eventually_done   == <>(\A o \in orders : o.paid)  -- <>: eventually
property pending_resolved  == (\E o \in orders : ~o.done) ~> (\A o \in orders : o.done)
property p_until           == a until b
property transfer_preserves == [](total_balance()' = total_balance())  -- prime: next-state evaluation
```

- prime `e'` is legal only inside `property` bodies (next state = post-transition state).
- **v1 support status**: `[]` → Always and `<>` → Eventually are checked; prime, `~>`,
  and `until` are **skipped with a warning** in the Stateright backend (not counted toward
  the verdict); `fairness weak/strong` declarations are accepted but no backend enforces
  them yet (warning only).

## 9. Standard library at a glance (appendix B)

All pure functions, all usable with method-call sugar. Common ones (the complete signature
table is in [../cql.md](../cql.md) appendix B):

| Domain | Common functions |
| --- | --- |
| string | `contains` `starts_with` `length` `concat` `substring` `trim` `split` `join` `to_string_int` … |
| math | `abs` `min` `max` `floor` `ceil` `round` |
| decimal | `decimal_from_string` `round_to` `to_string_decimal` |
| date | `year` `month` `day` `add_days` `days_between` `parse_date` `day_of_week` |
| vector/iteration | `fold` `map` `filter` `append` `to_vector` `sort_by` `take` `drop` `scan_left` `concat_vector` |
| set/bag | `size` `the` (extract the single element, otherwise trap) `only` `union_all` `bag_to_set` `copies_in` |
| map | `map_get` `map_insert` `map_remove` `map_keys` `map_values` `map_size` `map_from_vector` |
| option | `map` `and_then` `unwrap_or` `is_some` `is_none` |
| aggregate (§4.8.3) | `aggregate` (built-in combinator) and sugar `count_by` `sum_by` `avg_by` `min_by` `max_by` |

Same-name dispatch exists in only two places: `length` (string / vector) and `map`
(vector / option), dispatched on the first argument's type.

## 10. Trap semantics (§5.3)

CQL defaults to total functions; the remaining partial operations are checked at runtime,
and failure is a **trap** (mapped to an error code by the host):

- `int` division by zero / modulo zero, arithmetic overflow (no wrapping);
  `float as int` out of range or NaN;
- `decimal(m, n)` operation results exceeding m digits, `as` conversions out of range;
  unbounded `decimal` has no precision trap (division by zero still traps);
- `the(S)` applied to a non-singleton set; general recursion exhausting the stack.
- `query` trap ⇒ the query fails with no side effects; `action` trap ⇒ **no write_op is
  applied**.
- Recoverable errors are not traps — express them explicitly with `option` /
  `enum result`.
