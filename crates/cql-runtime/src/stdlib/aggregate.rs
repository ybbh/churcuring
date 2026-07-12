//! The `aggregate` combinator and its sugars (doc/cql.md §4.8.3, Appendix B).
//!
//! `aggregate` is a built-in combinator (not part of the pure-function standard library,
//! Appendix B); the `count_by`/`sum_by`/`avg_by`/`min_by`/`max_by` sugars are all defined
//! in terms of it. After compilation, grouped aggregation is implemented with a hash table
//! (no SQL `GROUP BY` is generated, §4.8.3, §6.3).

use std::collections::HashMap;
use std::hash::Hash;

use crate::value::CanonOrd;

/// Aggregation result row: `{ key: K, agg: R }` (§4.8.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggRow<K, R> {
    pub key: K,
    pub agg: R,
}

/// `aggregate(src, group_key, value, reducer, init, finalize)` (§4.8.3).
///
/// - `src`: any iterator — a `set` source yields each element once; for a `bag` source the
///   caller passes an iterator expanded by multiplicity (`CqlBag::iter_expanded`).
/// - `reducer` must be commutative and associative, and `init` its identity element (§4.8.3).
/// - Each non-empty group produces one row; an empty source produces an empty result; the
///   output is sorted in **canonical order** by group key (§5.1).
pub fn aggregate<I, T, K, V, R>(
    src: I,
    group_key: impl Fn(&T) -> K,
    value: impl Fn(&T) -> V,
    reducer: impl Fn(V, V) -> V,
    init: V,
    finalize: impl Fn(V) -> R,
) -> Vec<AggRow<K, R>>
where
    I: IntoIterator<Item = T>,
    K: CanonOrd + Eq + Hash,
    V: Clone,
{
    let mut groups: HashMap<K, V> = HashMap::new();
    for item in src {
        let k = group_key(&item);
        let v = value(&item);
        groups
            .entry(k)
            .and_modify(|acc| *acc = reducer(acc.clone(), v.clone()))
            .or_insert_with(|| reducer(init.clone(), v));
    }
    let mut rows: Vec<AggRow<K, R>> = groups
        .into_iter()
        .map(|(key, acc)| AggRow { key, agg: finalize(acc) })
        .collect();
    rows.sort_by(|a, b| a.key.canon_cmp(&b.key)); // canonical order of group keys (§5.1)
    rows
}

/// `count_by(src, key)`: count the elements of each group (Appendix B).
pub fn count_by<I, T, K>(src: I, key: impl Fn(&T) -> K) -> Vec<AggRow<K, i64>>
where
    I: IntoIterator<Item = T>,
    K: CanonOrd + Eq + Hash,
{
    aggregate(src, key, |_| 1i64, |a, b| a + b, 0, |c| c)
}

/// `sum_by(src, key, val)` (Appendix B).
pub fn sum_by<I, T, K>(src: I, key: impl Fn(&T) -> K, val: impl Fn(&T) -> f64) -> Vec<AggRow<K, f64>>
where
    I: IntoIterator<Item = T>,
    K: CanonOrd + Eq + Hash,
{
    aggregate(src, key, val, |a, b| a + b, 0.0, |s| s)
}

/// `avg_by(src, key, val)`: defined via a `(sum, count)` intermediate aggregation (Appendix B).
pub fn avg_by<I, T, K>(src: I, key: impl Fn(&T) -> K, val: impl Fn(&T) -> f64) -> Vec<AggRow<K, f64>>
where
    I: IntoIterator<Item = T>,
    K: CanonOrd + Eq + Hash,
{
    aggregate(
        src,
        key,
        |t| (val(t), 1i64),
        |(s1, c1), (s2, c2)| (s1 + s2, c1 + c2),
        (0.0, 0),
        |(s, c)| s / c as f64, // groups are non-empty ⇒ c ≥ 1
    )
}

/// `min_by(src, key, val)`: defined via an `Option` accumulator (Appendix B; `V` is compared
/// in canonical order).
pub fn min_by<I, T, K, V>(src: I, key: impl Fn(&T) -> K, val: impl Fn(&T) -> V) -> Vec<AggRow<K, V>>
where
    I: IntoIterator<Item = T>,
    K: CanonOrd + Eq + Hash,
    V: CanonOrd + Clone,
{
    min_max_by(src, key, val, false)
}

/// `max_by(src, key, val)` (Appendix B).
pub fn max_by<I, T, K, V>(src: I, key: impl Fn(&T) -> K, val: impl Fn(&T) -> V) -> Vec<AggRow<K, V>>
where
    I: IntoIterator<Item = T>,
    K: CanonOrd + Eq + Hash,
    V: CanonOrd + Clone,
{
    min_max_by(src, key, val, true)
}

fn min_max_by<I, T, K, V>(
    src: I,
    key: impl Fn(&T) -> K,
    val: impl Fn(&T) -> V,
    want_max: bool,
) -> Vec<AggRow<K, V>>
where
    I: IntoIterator<Item = T>,
    K: CanonOrd + Eq + Hash,
    V: CanonOrd + Clone,
{
    aggregate(
        src,
        key,
        |t| Some(val(t)),
        |a, b| match (&a, &b) {
            (None, _) => b,
            (_, None) => a,
            (Some(x), Some(y)) => {
                let take_b = if want_max {
                    y.canon_cmp(x) == std::cmp::Ordering::Greater
                } else {
                    y.canon_cmp(x) == std::cmp::Ordering::Less
                };
                if take_b {
                    b
                } else {
                    a
                }
            }
        },
        None,
        |o| o.expect("non-empty groups always have Some"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collections::{CqlBag, CqlSet};

    #[test]
    fn aggregate_group_order_canonical() {
        // §8.2 pattern: group by city and average (simplified to counting here)
        let rows = CqlSet::from_elems(vec![
            ("bj".to_string(), 10),
            ("sh".to_string(), 20),
            ("bj".to_string(), 30),
            ("sh".to_string(), 40),
            ("gz".to_string(), 50),
        ]);
        let result = count_by(rows.as_slice().to_vec(), |r| r.0.clone());
        let keys: Vec<_> = result.iter().map(|r| r.key.clone()).collect();
        assert_eq!(keys, vec!["bj", "gz", "sh"]); // canonical order (string lexicographic order)
        assert_eq!(result[0].agg, 2);
        assert_eq!(result[1].agg, 1);
        assert_eq!(result[2].agg, 2);
    }

    #[test]
    fn aggregate_empty_source() {
        let result: Vec<AggRow<i64, i64>> = count_by(Vec::<i64>::new(), |x| *x);
        assert!(result.is_empty());
    }

    #[test]
    fn aggregate_bag_counts_multiplicity() {
        // A bag source counts each element by multiplicity (§4.8.3): the caller passes the
        // expanded iterator
        let bag = CqlBag::from_elems([1, 2, 2, 3, 3, 3]);
        let expanded: Vec<i64> = bag.iter_expanded().copied().collect();
        let result = count_by(expanded, |x| x % 2);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].agg, 2); // even numbers {2, 2}
        assert_eq!(result[1].agg, 4); // odd numbers {1, 3, 3, 3}
    }

    #[test]
    fn sum_avg_min_max_by() {
        let data = vec![("a", 1.0), ("b", 2.0), ("a", 3.0), ("b", 4.0)];
        let sums = sum_by(data.clone(), |t| t.0, |t| t.1);
        assert_eq!(sums, vec![
            AggRow { key: "a", agg: 4.0 },
            AggRow { key: "b", agg: 6.0 },
        ]);
        let avgs = avg_by(data.clone(), |t| t.0, |t| t.1);
        assert_eq!(avgs[0].agg, 2.0);
        assert_eq!(avgs[1].agg, 3.0);
        let mins = min_by(data.clone(), |t| t.0, |t| t.1);
        assert_eq!(mins[0].agg, 1.0);
        assert_eq!(mins[1].agg, 2.0);
        let maxs = max_by(data, |t| t.0, |t| t.1);
        assert_eq!(maxs[0].agg, 3.0);
        assert_eq!(maxs[1].agg, 4.0);
    }
}
