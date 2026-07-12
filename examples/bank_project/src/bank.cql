// Taken from doc/model-check.md §4.1: bank ledger conservation (new syntax)
module bank;

table accounts { id: int, owner: string, balance: int } primary key {id}

action transfer(from_id: int, to_id: int, amt: int) -> set<write_op> == {
    match (lookup(accounts, from_id), lookup(accounts, to_id)) {
        (some(f), some(t)) =>
            if f.balance >= amt then
                set { update(accounts, from_id, lambda [amt](v) { record { v with balance: v.balance - amt } }),
                      update(accounts, to_id,   lambda [amt](v) { record { v with balance: v.balance + amt } }) }
            else set {},
        _ => set {}
    }
}

query total_balance() -> int == {
    fold(to_vector(accounts), 0, lambda(acc, a) { acc + a.balance })
}

property balance_conserved == [](total_balance() = 10000)
property no_negative       == \A a \in accounts : a.balance >= 0
// prime: the total balance is unchanged before and after each transfer step (primed = post-transition state)
property transfer_preserves == [](total_balance()' = total_balance())

test transfer_basic {
    fixture accounts == [
        record { id: 1, owner: "a", balance: 6000 },
        record { id: 2, owner: "b", balance: 4000 }
    ];
    expect total_balance() == 10000;
}
