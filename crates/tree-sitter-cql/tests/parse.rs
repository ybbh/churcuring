//! Integration tests for the tree-sitter CQL grammar.
//!
//! Each case parses a representative CQL source and asserts the resulting
//! syntax tree contains no `ERROR` or `MISSING` nodes. The only intentional
//! exception is the non-chainable comparison case, which must fail.

use tree_sitter::Parser;
use tree_sitter_cql::LANGUAGE;

fn parse(src: &str) -> tree_sitter::Tree {
    let mut parser = Parser::new();
    parser
        .set_language(&LANGUAGE.into())
        .expect("failed to load CQL language");
    parser.parse(src, None).expect("parse returned no tree")
}

fn assert_parses_cleanly(src: &str) {
    let tree = parse(src);
    assert!(
        !tree.root_node().has_error(),
        "source should parse without ERROR/MISSING nodes:\n{src}\n---\n{}",
        tree.root_node().to_sexp()
    );
}

#[test]
fn schema_declarations() {
    assert_parses_cleanly(
        r#"
module analytics;

table users { id: int, name: string, active: bool, city: string } primary key {id}
table orders { order_id: int, user_id: int, amount: float }
    primary key {order_id}
    foreign key {user_id} references users
table sessions { session_id: int, user_id: int, duration: int }
    primary key {session_id}
    foreign key {user_id} references users
index sessions_by_user on sessions(user_id)

function pure_is_long_session(dur: int) -> bool == { dur > 300 }
"#,
    );
}

#[test]
fn literals_and_lexical_forms() {
    assert_parses_cleanly(
        r#"
module m;
// line comment
/* block comment */
const a: int == 0x1F;
const b: int == 1_000;
const c: float == 1e-3;
const s: string == "hi\n \u{1F600} \(a + 1) end";
const d: date == date "2024-01-01";
const e: decimal(4, 2) == decimal(4, 2) 3.14;
"#,
    );
}

#[test]
fn enums_and_functions() {
    assert_parses_cleanly(
        r#"
module m;
enum tree { leaf(int), node(tree, int, tree) }
enum result<T, E> { ok(T), err(E) }
function recursive inorder(t: tree) -> vector<int> == {
    match t {
        leaf(v) => [v],
        node(l, x, r) => concat_vector(concat_vector(inorder(l), [x]), inorder(r))
    }
}
function gcd(a: int, b: int) -> int == { if b = 0 then a else gcd(b, a % b) }
"#,
    );
}

#[test]
fn query_with_named_arguments_and_lambdas() {
    assert_parses_cleanly(
        r#"
module m;
query avg_session_by_city() -> vector<{ key: string, agg: float }> == {
    let pairs == set { record { city: u.city, duration: s.duration }
                       : u \in set { x \in users if x.active },
                         s \in read(sessions, lambda [u](y) { y.user_id = u.id }) };
    aggregate(pairs,
              group_key: lambda(r) { r.city },
              value: lambda(r) { (r.duration as float, 1) },
              reducer: lambda((a, c), (b, d)) { (a + b, c + d) },
              init: (0.0, 0),
              finalize: lambda((total, count)) { total / (count as float) })
}
"#,
    );
}

#[test]
fn action_and_recursion_bounds() {
    assert_parses_cleanly(
        r#"
module m;
action remove_inactive_users() -> set<write_op> == {
    set { delete(users, u.id) : u \in set { v \in users if ~v.active } }
}
query subordinates(mgr_id: int) -> set<int> with depth 32 == {
    let direct == set { e.id : e \in set { x \in employees if x.manager_id = some(mgr_id) } };
    direct \cup union_all(set { subordinates(d) : d \in direct })
}
"#,
    );
}

#[test]
fn turbofish_and_option_generator() {
    assert_parses_cleanly(
        r#"
module m;
query q() -> set<int> == {
    set { u.id : u \in lookup(users, o.user_id) }
}
query r() -> int == { f::<int>(1) }
"#,
    );
}

#[test]
fn invariant_and_test_declarations() {
    assert_parses_cleanly(
        r#"
module m;
invariant positive_balance on accounts == \A a \in accounts : a.balance >= 0;
test counterexample_balance {
    fixture accounts == [
        record { id: 1, owner: "a", balance: 6000 },
        record { id: 2, owner: "b", balance: 4000 }
    ];
    expect total_balance() == 10000;
}
"#,
    );
}

#[test]
fn properties_and_fairness() {
    assert_parses_cleanly(
        r#"
module m;
property balance_conserved == [](total_balance() = 10000)
property transfer_preserves == [](total_balance()' = total_balance())
property pending_resolved ==
    (\E o \in orders : o.status = some_pending) ~> (\A o \in orders : o.status /= some_pending)
fairness weak == transfer, settle
"#,
    );
}

#[test]
fn lambda_forms_and_record_update() {
    assert_parses_cleanly(
        r#"
module m;
query q() -> int == {
    let f1 == lambda [new_city](v) { record { v with city: new_city } };
    let f2 == lambda(_) { 42 };
    let f3 == lambda(x: int, y: int) -> int { x + y };
    f3(1, 2)
}
"#,
    );
}

/// `a = b = c` must NOT parse: comparison is non-chainable (A.2), so the
/// grammar makes it a hard syntax error rather than left/right-associating.
#[test]
fn comparison_is_not_chainable() {
    let tree = parse("module m;\nquery q() -> bool == { a = b = c }\n");
    assert!(
        tree.root_node().has_error(),
        "chained comparison should produce an ERROR node, got:\n{}",
        tree.root_node().to_sexp()
    );
}
