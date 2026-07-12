// Taken from doc/cql.md §8 (revised for the 2026-07-12 syntax: lambda keyword form, filter-style if, turbofish, mandatory let semicolons)
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

// §3.5: parameterless and parameterized queries
query active_user_names() -> set<string> == {
    set { u.name : u \in set { v \in users if v.active } }
}

query orders_by_user(user_id: int) -> set<orders> == {
    read(orders, lambda [user_id](o) { o.user_id = user_id })
}

// §8.2: average session duration of active users (by city)
query avg_session_by_city() -> vector<{ key: string, agg: float }> == {
    let pairs == set { record { city: u.city, session_id: s.session_id, duration: s.duration }
                       : u \in set { x \in users if x.active },
                         s \in read(sessions, lambda [u](y) { y.user_id = u.id /\ pure_is_long_session(y.duration) }) };
    aggregate(pairs,
              group_key: lambda(r) { r.city },
              value: lambda(r) { (r.duration as float, 1) },
              reducer: lambda((a, c), (b, d)) { (a + b, c + d) },
              init: (0.0, 0),
              finalize: lambda((total, count)) { total / (count as float) })
}

// §8.3: removing inactive users
action remove_inactive_users() -> set<write_op> == {
    set { delete(users, u.id) : u \in set { v \in users if ~v.active } }
}

// §8.4: outer joins, parameterization, and ordered output
query order_report(min_amount: float) -> vector<{ order_id: int, who: string }> == {
    take(sort_by(to_vector(set { record { order_id: o.order_id,
                                          who: lookup(users, o.user_id).map(lambda(u) { u.name }).unwrap_or("<deleted>") }
                                 : o \in set { x \in orders if x.amount >= min_amount } }),
                 lambda(r) { r.order_id }),
         5)
}

// §8.5: quantifiers and bag comprehensions
query active_users_all_named() -> bool == {
    \A u \in set { v \in users if v.active } : u.name /= ""
}

query amount_histogram() -> vector<{ key: int, agg: int }> == {
    count_by(bag { o.amount : o \in orders }, lambda(a) { floor(a) as int })
}

query total_amount_by_user() -> vector<{ key: int, agg: float }> == {
    sum_by(read(orders, lambda(o) { true }), lambda(o) { o.user_id }, lambda(o) { o.amount })
}

// §8.6: structural recursion and general recursion
enum tree { leaf(int), node(tree, int, tree) }

function recursive inorder(t: tree) -> vector<int> == {
    match t {
        leaf(v)       => [v],
        node(l, x, r) => concat_vector(concat_vector(inorder(l), [x]), inorder(r))
    }
}

function gcd(a: int, b: int) -> int == { if b = 0 then a else gcd(b, a % b) }

table employees { id: int, name: string, manager_id: option<int> } primary key {id}

query subordinates(mgr_id: int) -> set<int> with depth 32 == {
    let direct == set { e.id : e \in set { x \in employees if x.manager_id = some(mgr_id) } };
    direct \cup union_all(set { subordinates(d) : d \in direct })
}

// Appendix C: test blocks
test avg_session_by_city_basic {
    fixture users == [
        record { id: 1, name: "a", active: true, city: "x" },
        record { id: 2, name: "b", active: false, city: "x" }
    ];
    fixture sessions == [
        record { session_id: 10, user_id: 1, duration: 600 }
    ];
    expect avg_session_by_city() == [record { key: "x", agg: 600.0 }];
}
