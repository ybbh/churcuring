//! Map group (doc/cql.md §4.10, Appendix B).

use std::hash::Hash;

use crate::collections::{CqlBag, CqlMap, CqlSet};
use crate::value::CanonOrd;

/// `map_get(m, k)`.
pub fn map_get<'a, K: CanonOrd + Eq + Hash, V>(
    m: &'a CqlMap<K, V>,
    k: &K,
) -> Option<&'a V> {
    m.get(k)
}

/// `map_insert(m, k, v)`: returns a new map (immutable semantics, §4.10).
pub fn map_insert<K: CanonOrd + Eq + Hash + Clone, V: Clone>(
    m: &CqlMap<K, V>,
    k: K,
    v: V,
) -> CqlMap<K, V> {
    m.insert(k, v)
}

/// `map_remove(m, k)`: returns a new map.
pub fn map_remove<K: CanonOrd + Eq + Hash + Clone, V: Clone>(
    m: &CqlMap<K, V>,
    k: &K,
) -> CqlMap<K, V> {
    m.remove(k)
}

/// `map_keys(m)`.
pub fn map_keys<K: CanonOrd + Eq + Hash + Clone, V>(m: &CqlMap<K, V>) -> CqlSet<K> {
    m.keys()
}

/// `map_values(m)`.
pub fn map_values<K, V: CanonOrd + Eq + Hash + Clone>(m: &CqlMap<K, V>) -> CqlBag<V> {
    m.values()
}

/// `map_size(m)`.
pub fn map_size<K, V>(m: &CqlMap<K, V>) -> i64 {
    m.len() as i64
}

/// `map_from_vector(pairs)`: for duplicate keys, the later one wins.
pub fn map_from_vector<K: CanonOrd + Eq + Hash + Clone, V: Clone>(
    pairs: Vec<(K, V)>,
) -> CqlMap<K, V> {
    CqlMap::from_vector(pairs)
}

/// `map_to_vector(m)`: canonical order, sorted by key (§5.1).
pub fn map_to_vector<K: CanonOrd + Eq + Hash + Clone, V: Clone>(
    m: &CqlMap<K, V>,
) -> Vec<(K, V)> {
    m.to_vector()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_stdlib() {
        let m = map_from_vector(vec![(1, "a"), (2, "b"), (1, "c")]);
        assert_eq!(map_size(&m), 2); // duplicate key: the later one wins
        assert_eq!(map_get(&m, &1), Some(&"c"));
        let m2 = map_insert(&m, 3, "d");
        assert_eq!(map_size(&m), 2); // original value unchanged
        assert_eq!(map_size(&m2), 3);
        let m3 = map_remove(&m2, &2);
        assert_eq!(map_to_vector(&m3), vec![(1, "c"), (3, "d")]);
        assert_eq!(map_keys(&m3).as_slice(), &[1, 3]);
        let vals = map_values(&m3);
        assert_eq!(vals.copies_in(&"c"), 1);
        assert_eq!(vals.copies_in(&"d"), 1);
    }
}
