//! Pure standard-library functions (doc/cql.md Appendix B).
//!
//! All functions are pure and may be inlined; the `recv.f(x)` method syntax is
//! equivalent to `f(recv, x)`. Same-name dispatch (`length` on string/vector,
//! `map` on vector/option) is naturally dispatched on the Rust side by the type
//! of the first argument; callers just import from the corresponding submodule.

pub mod aggregate;
pub mod date;
pub mod decimal;
pub mod map;
pub mod math;
pub mod option;
pub mod set_bag;
pub mod string;
pub mod vector;

pub use aggregate::{aggregate, avg_by, count_by, max_by, min_by, sum_by, AggRow};
pub use date::{add_days, day, day_of_week, days_between, month, parse_date, year};
pub use decimal::{decimal_from_string, round_to};
pub use map::{
    map_from_vector, map_get, map_insert, map_keys, map_remove, map_size, map_to_vector,
    map_values,
};
pub use math::{abs, ceil, floor, max, min, round};
pub use option::{and_then, is_none, is_some, option_map, unwrap_or};
pub use set_bag::{bag_to_set, bag_union, copies_in, only, set_to_bag, size, the, union_all};
pub use string::{
    concat, contains, ends_with, join, split, starts_with, str_length, substring,
    to_string_bool, to_string_date, to_string_decimal, to_string_float, to_string_int, trim,
};
pub use vector::{
    append, concat_vector, drop, filter, fold, is_empty, scan_left, sort_by, take, to_set,
    to_vector, vec_length, vec_map,
};
