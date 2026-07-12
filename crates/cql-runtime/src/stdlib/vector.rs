//! Vector / iteration group (doc/cql.md §4.8.1, §4.8.2, Appendix B).
//!
//! On the Rust side, `vector<T>` is represented by slices/Vec.

use std::hash::Hash;

use crate::collections::CqlSet;
use crate::value::CanonOrd;

/// `fold(xs, init, step)`: general iteration (§4.8.1).
pub fn fold<T, A>(xs: &[T], init: A, step: impl Fn(A, &T) -> A) -> A {
    xs.iter().fold(init, step)
}

/// `map(xs, f)` (vector version; the option version is `crate::stdlib::option::option_map`).
pub fn vec_map<A, B>(xs: &[A], f: impl Fn(&A) -> B) -> Vec<B> {
    xs.iter().map(f).collect()
}

/// `filter(xs, p)`.
pub fn filter<T: Clone>(xs: &[T], p: impl Fn(&T) -> bool) -> Vec<T> {
    xs.iter().filter(|x| p(x)).cloned().collect()
}

/// `append(xs, x)`.
pub fn append<T: Clone>(xs: &[T], x: T) -> Vec<T> {
    let mut v = xs.to_vec();
    v.push(x);
    v
}

/// `to_vector(S)`: materialize in canonical order (§4.8.2, §5.1).
pub fn to_vector<T: CanonOrd + Eq + Hash + Clone>(s: &CqlSet<T>) -> Vec<T> {
    s.to_vector()
}

/// `sort_by(xs, key)`: stable sort, ascending, `K: ord` (§4.8.2; uses canonical order here, §2.3).
pub fn sort_by<T: Clone, K: CanonOrd>(xs: &[T], key: impl Fn(&T) -> K) -> Vec<T> {
    let mut v = xs.to_vec();
    v.sort_by(|a, b| key(a).canon_cmp(&key(b))); // slice::sort_by is stable
    v
}

/// `take(xs, n)`: the first n elements; n ≤ 0 ⇒ empty, n > len ⇒ all.
pub fn take<T: Clone>(xs: &[T], n: i64) -> Vec<T> {
    xs.iter().take(n.max(0) as usize).cloned().collect()
}

/// `drop(xs, n)`: remove the first n elements.
pub fn drop<T: Clone>(xs: &[T], n: i64) -> Vec<T> {
    xs.iter().skip(n.max(0) as usize).cloned().collect()
}

/// `to_set(xs)`: convert to a set (deduplicated).
pub fn to_set<T: CanonOrd + Eq + Hash + Clone>(xs: Vec<T>) -> CqlSet<T> {
    CqlSet::from_elems(xs)
}

/// `length(xs)` (vector version).
pub fn vec_length<T>(xs: &[T]) -> i64 {
    xs.len() as i64
}

/// `is_empty(xs)`.
pub fn is_empty<T>(xs: &[T]) -> bool {
    xs.is_empty()
}

/// `concat_vector(a, b)`.
pub fn concat_vector<T: Clone>(a: &[T], b: &[T]) -> Vec<T> {
    let mut v = a.to_vec();
    v.extend_from_slice(b);
    v
}

/// `scan_left(xs, init, step)`: produces the accumulator value **after** each step;
/// the output has the same length as the input (§4.8.4).
///
/// `A: Clone` is needed to retain the accumulator value at each step.
pub fn scan_left<T, A: Clone>(xs: &[T], init: A, step: impl Fn(A, &T) -> A) -> Vec<A> {
    let mut acc = init;
    let mut out = Vec::with_capacity(xs.len());
    for x in xs {
        acc = step(acc, x);
        out.push(acc.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_map_filter() {
        let xs = [1, 2, 3, 4];
        assert_eq!(fold(&xs, 0, |a, x| a + x), 10);
        assert_eq!(vec_map(&xs, |x| x * 2), vec![2, 4, 6, 8]);
        assert_eq!(filter(&xs, |x| x % 2 == 0), vec![2, 4]);
        assert_eq!(append(&xs, 5), vec![1, 2, 3, 4, 5]);
        assert_eq!(concat_vector(&[1], &[2, 3]), vec![1, 2, 3]);
        assert_eq!(vec_length(&xs), 4);
        assert!(!is_empty(&xs));
    }

    #[test]
    fn sort_take_drop() {
        let xs = [3, 1, 2, 1];
        // Stable sort: equal elements keep their original order
        // (verified using (key, original index))
        let pairs = [(3, 'a'), (1, 'b'), (2, 'c'), (1, 'd')];
        let sorted = sort_by(&pairs, |p| p.0);
        assert_eq!(sorted, vec![(1, 'b'), (1, 'd'), (2, 'c'), (3, 'a')]);
        assert_eq!(take(&xs, 2), vec![3, 1]);
        assert_eq!(take(&xs, 99), xs.to_vec());
        assert_eq!(take(&xs, -1), Vec::<i32>::new());
        assert_eq!(drop(&xs, 2), vec![2, 1]);
    }

    #[test]
    fn scan_left_running_sum() {
        // §4.8.4 running-total pattern
        let xs = [1, 2, 3];
        assert_eq!(scan_left(&xs, 0, |acc, x| acc + x), vec![1, 3, 6]);
        // row-number pattern
        assert_eq!(scan_left(&xs, 0, |n, _| n + 1), vec![1, 2, 3]);
    }

    #[test]
    fn set_conversions() {
        let s = to_set(vec![3, 1, 3, 2]);
        assert_eq!(s.as_slice(), &[1, 2, 3]);
        assert_eq!(to_vector(&s), vec![1, 2, 3]);
    }
}
