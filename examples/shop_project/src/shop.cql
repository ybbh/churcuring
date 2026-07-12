// Taken from doc/cql.md §6.4: the shop module (new syntax)
module shop;

use util;

table users { id: int, name: string, city: string } primary key {id}
table orders { order_id: int, user_id: int, amount: float }
    primary key {order_id}
    foreign key {user_id} references users

query user_order_totals() -> vector<{ key: string, agg: float }> == {
    aggregate(bag { (o, u) : o \in orders, u \in lookup(users, o.user_id) },
              group_key: lambda(p) { p.1.name },
              value: lambda(p) { p.0.amount },
              reducer: lambda(a, b) { a + b },
              init: 0.0,
              finalize: lambda(s) { s })
}

// Cross-module call of a public pure function (imported via use util)
query large_orders() -> set<orders> == {
    read(orders, lambda(o) { is_large_amount(o.amount) })
}

// ? propagation sugar + string interpolation
query greet_user(user_id: int) -> option<string> == {
    let u == lookup(users, user_id)?;
    some("hello \(u.name), city: \(u.city)")
}

// §3.6: actions
action move_user(user_id: int, new_city: string) -> set<write_op> == {
    set { update(users, user_id, lambda [new_city](v) { record { v with city: new_city } }) }
}

action add_user(id: int, name: string, city: string) -> set<write_op> == {
    set { insert(users, record { id: id, name: name, city: city }) }
}
