//! Collection types: `CqlSet` / `CqlBag` / `CqlMap` (doc/cql.md §2.1, §4.7, §4.10, §5.1).
//!
//! All three are internally `Vec`s sorted in canonical order (`CanonOrd`), which guarantees
//! deterministic materialization (§5.1):
//! - `CqlSet<T>`: sorted and deduplicated (elements must be hashable, §2.3);
//! - `CqlBag<T>`: a sorted `(element, multiplicity)` table that preserves multiplicities (§4.4.3);
//! - `CqlMap<K, V>`: a sorted key-value table with unique keys; insert/remove return a new
//!   map (pure immutable semantics, §4.10).

use std::hash::Hash;

use crate::trap::{CqlResult, Trap};
use crate::value::CanonOrd;

// ---------------------------------------------------------------------------
// CqlSet
// ---------------------------------------------------------------------------

/// Unordered set (deduplicated), internally a deduplicated `Vec` sorted in canonical order
/// (§2.1, §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CqlSet<T> {
    /// Invariant: ascending by `CanonOrd`, no duplicates.
    elems: Vec<T>,
}

impl<T> CqlSet<T> {
    /// The empty set `set {}`.
    pub fn new() -> Self {
        CqlSet { elems: Vec::new() }
    }

    /// A single-element set.
    pub fn singleton(x: T) -> Self {
        CqlSet { elems: vec![x] }
    }

    /// Number of elements (Appendix B `size`).
    pub fn len(&self) -> usize {
        self.elems.len()
    }

    pub fn is_empty(&self) -> bool {
        self.elems.is_empty()
    }

    /// Slice in canonical order (for materialization, §5.1).
    pub fn as_slice(&self) -> &[T] {
        &self.elems
    }

    /// Iterate in canonical order.
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.elems.iter()
    }
}

impl<T: CanonOrd + Eq + Hash> CqlSet<T> {
    /// Membership test `x \in S` (§4.7).
    pub fn contains(&self, x: &T) -> bool {
        self.elems
            .binary_search_by(|e| e.canon_cmp(x))
            .is_ok()
    }

    /// `the(S)`: returns the element if there is exactly one, otherwise
    /// `Trap::TheNonSingleton` (§4.7, §5.3, Appendix B).
    pub fn the(&self) -> CqlResult<&T> {
        match self.elems.as_slice() {
            [x] => Ok(x),
            _ => Err(Trap::TheNonSingleton),
        }
    }

    /// `only(S)`: exactly one element ⇒ `Some`; empty ⇒ `None`; multiple elements ⇒
    /// `Trap::OnlyMulti` (Appendix B).
    pub fn only(&self) -> CqlResult<Option<&T>> {
        match self.elems.as_slice() {
            [] => Ok(None),
            [x] => Ok(Some(x)),
            _ => Err(Trap::OnlyMulti),
        }
    }
}

impl<T: CanonOrd + Eq + Hash + Clone> CqlSet<T> {
    /// Construct from an iterator: sort + deduplicate (set semantics, §2.3).
    pub fn from_elems<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut elems: Vec<T> = iter.into_iter().collect();
        elems.sort_by(CanonOrd::canon_cmp);
        elems.dedup_by(|a, b| a.canon_cmp(b) == std::cmp::Ordering::Equal);
        CqlSet { elems }
    }

    /// `S \cup T`: union (§4.7).
    pub fn union(&self, other: &CqlSet<T>) -> CqlSet<T> {
        CqlSet::from_elems(self.elems.iter().cloned().chain(other.elems.iter().cloned()))
    }

    /// `S \cap T`: intersection (§4.7).
    pub fn inter(&self, other: &CqlSet<T>) -> CqlSet<T> {
        CqlSet::from_elems(self.elems.iter().filter(|e| other.contains(e)).cloned())
    }

    /// `S \ T`: difference (§4.7).
    pub fn diff(&self, other: &CqlSet<T>) -> CqlSet<T> {
        CqlSet::from_elems(self.elems.iter().filter(|e| !other.contains(e)).cloned())
    }

    /// `S \subseteq T`: subset test (§4.7).
    pub fn is_subset(&self, other: &CqlSet<T>) -> bool {
        self.elems.iter().all(|e| other.contains(e))
    }

    /// `S \X T`: Cartesian product (§4.7).
    pub fn cartesian<U: CanonOrd + Eq + Hash + Clone>(&self, other: &CqlSet<U>) -> CqlSet<(T, U)> {
        CqlSet::from_elems(self.elems.iter().flat_map(|a| {
            other.elems.iter().map(move |b| (a.clone(), b.clone()))
        }))
    }

    /// `to_vector(S)`: materialize in canonical order (§4.8.2, §5.1).
    pub fn to_vector(&self) -> Vec<T> {
        self.elems.clone()
    }

    /// `union_all(S)`: union of a family of sets (Appendix B).
    pub fn union_all(sets: &CqlSet<CqlSet<T>>) -> CqlSet<T> {
        let mut acc = CqlSet::new();
        for s in sets.iter() {
            acc = acc.union(s);
        }
        acc
    }
}

impl<T> Default for CqlSet<T> {
    fn default() -> Self {
        CqlSet { elems: Vec::new() }
    }
}

/// Canonical order of sets: element-wise (§2.3, §5.1).
impl<T: CanonOrd> CanonOrd for CqlSet<T> {
    fn canon_cmp(&self, other: &Self) -> std::cmp::Ordering {
        crate::value::canon_cmp_slice(&self.elems, &other.elems)
    }
}

// ---------------------------------------------------------------------------
// CqlBag
// ---------------------------------------------------------------------------

/// Multiset (preserves multiplicities), internally a `(element, multiplicity)` table sorted
/// in canonical order (§4.4.3, §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CqlBag<T> {
    /// Invariant: ascending by `CanonOrd`, unique elements, multiplicity ≥ 1.
    entries: Vec<(T, u64)>,
}

impl<T> CqlBag<T> {
    /// The empty bag `bag {}`.
    pub fn new() -> Self {
        CqlBag { entries: Vec::new() }
    }

    /// Number of distinct elements.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Total number of elements counting multiplicities.
    pub fn total_count(&self) -> u64 {
        self.entries.iter().map(|(_, c)| c).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The `(element, multiplicity)` table (canonical order).
    pub fn entries(&self) -> &[(T, u64)] {
        &self.entries
    }

    /// Iterate expanded by multiplicity (canonical order; a bag source for `aggregate` is
    /// expanded this way, §4.8.3).
    pub fn iter_expanded(&self) -> impl Iterator<Item = &T> {
        self.entries.iter().flat_map(|(e, c)| std::iter::repeat_n(e, *c as usize))
    }
}

impl<T: CanonOrd + Eq + Hash> CqlBag<T> {
    /// Construct from an iterator: count multiplicities (bag comprehension / bag literal
    /// semantics, §4.4.3).
    pub fn from_elems<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut counts: Vec<(T, u64)> = Vec::new();
        for x in iter {
            match counts.binary_search_by(|(e, _)| e.canon_cmp(&x)) {
                Ok(i) => counts[i].1 += 1,
                Err(i) => counts.insert(i, (x, 1)),
            }
        }
        CqlBag { entries: counts }
    }

    /// `copies_in(x, b)`: multiplicity (Appendix B).
    pub fn copies_in(&self, x: &T) -> i64 {
        match self.entries.binary_search_by(|(e, _)| e.canon_cmp(x)) {
            Ok(i) => self.entries[i].1 as i64,
            Err(_) => 0,
        }
    }

    /// Membership test `x \in b` (§4.7: a bag can serve as a membership-test source).
    pub fn contains(&self, x: &T) -> bool {
        self.copies_in(x) > 0
    }
}

impl<T: CanonOrd + Eq + Hash + Clone> CqlBag<T> {
    /// `set_to_bag`: each element gets multiplicity 1 (Appendix B).
    pub fn from_set(s: &CqlSet<T>) -> CqlBag<T> {
        CqlBag { entries: s.as_slice().iter().map(|e| (e.clone(), 1)).collect() }
    }

    /// `bag_to_set`: deduplicate (Appendix B).
    pub fn to_set(&self) -> CqlSet<T> {
        CqlSet { elems: self.entries.iter().map(|(e, _)| e.clone()).collect() }
    }

    /// `bag_union(a, b)`: multiplicities are added (Appendix B).
    pub fn bag_union(&self, other: &CqlBag<T>) -> CqlBag<T> {
        let mut entries = self.entries.clone();
        for (e, c) in &other.entries {
            match entries.binary_search_by(|(x, _)| x.canon_cmp(e)) {
                Ok(i) => entries[i].1 += c,
                Err(i) => entries.insert(i, (e.clone(), *c)),
            }
        }
        CqlBag { entries }
    }
}

impl<T> Default for CqlBag<T> {
    fn default() -> Self {
        CqlBag { entries: Vec::new() }
    }
}

/// Canonical order of bags: compare entry by entry as `(element, multiplicity)`.
impl<T: CanonOrd> CanonOrd for CqlBag<T> {
    fn canon_cmp(&self, other: &Self) -> std::cmp::Ordering {
        let (mut i, mut j) = (0, 0);
        loop {
            match (self.entries.get(i), other.entries.get(j)) {
                (None, None) => return std::cmp::Ordering::Equal,
                (None, Some(_)) => return std::cmp::Ordering::Less,
                (Some(_), None) => return std::cmp::Ordering::Greater,
                (Some((a, ca)), Some((b, cb))) => {
                    match a.canon_cmp(b).then_with(|| ca.cmp(cb)) {
                        std::cmp::Ordering::Equal => {
                            i += 1;
                            j += 1;
                        }
                        o => return o,
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// CqlMap
// ---------------------------------------------------------------------------

/// Pure, immutable associative structure `map<K, V>` (§4.10): internally a `(K, V)` table
/// sorted in canonical order with unique keys; `insert`/`remove` return a new map and do not
/// modify the original.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CqlMap<K, V> {
    /// Invariant: ascending by `CanonOrd` key, unique keys.
    entries: Vec<(K, V)>,
}

impl<K, V> CqlMap<K, V> {
    /// The empty map `map {}`.
    pub fn new() -> Self {
        CqlMap { entries: Vec::new() }
    }

    /// `map_size` (Appendix B).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate in canonical order (ascending by key).
    pub fn iter(&self) -> std::slice::Iter<'_, (K, V)> {
        self.entries.iter()
    }

    /// Slice in canonical order (for materialization).
    pub fn as_slice(&self) -> &[(K, V)] {
        &self.entries
    }
}

impl<K: CanonOrd + Eq + Hash, V> CqlMap<K, V> {
    /// `map_get` (Appendix B).
    pub fn get(&self, k: &K) -> Option<&V> {
        self.entries
            .binary_search_by(|(e, _)| e.canon_cmp(k))
            .ok()
            .map(|i| &self.entries[i].1)
    }
}

impl<K: CanonOrd + Eq + Hash + Clone, V> CqlMap<K, V> {
    /// `map_keys` (Appendix B).
    pub fn keys(&self) -> CqlSet<K> {
        CqlSet { elems: self.entries.iter().map(|(k, _)| k.clone()).collect() }
    }
}

impl<K, V: CanonOrd + Eq + Hash + Clone> CqlMap<K, V> {
    /// `map_values`: collect the values into a bag, counting occurrences (Appendix B).
    pub fn values(&self) -> CqlBag<V> {
        CqlBag::from_elems(self.entries.iter().map(|(_, v)| v.clone()))
    }
}

impl<K: CanonOrd + Eq + Hash + Clone, V: Clone> CqlMap<K, V> {
    /// `map_from_vector`: for duplicate keys, the later one wins (§4.10, Appendix B).
    pub fn from_vector<I: IntoIterator<Item = (K, V)>>(pairs: I) -> Self {
        let mut m = CqlMap::new();
        for (k, v) in pairs {
            m = m.insert(k, v);
        }
        m
    }

    /// `map_insert`: returns a new map; the original is unchanged (immutable semantics,
    /// §4.10).
    pub fn insert(&self, k: K, v: V) -> CqlMap<K, V> {
        let mut entries = self.entries.clone();
        match entries.binary_search_by(|(e, _)| e.canon_cmp(&k)) {
            Ok(i) => entries[i].1 = v,
            Err(i) => entries.insert(i, (k, v)),
        }
        CqlMap { entries }
    }

    /// `map_remove`: returns a new map (Appendix B).
    pub fn remove(&self, k: &K) -> CqlMap<K, V> {
        let mut entries = self.entries.clone();
        if let Ok(i) = entries.binary_search_by(|(e, _)| e.canon_cmp(k)) {
            entries.remove(i);
        }
        CqlMap { entries }
    }

    /// `map_to_vector`: canonical order, sorted by key (§5.1, Appendix B).
    pub fn to_vector(&self) -> Vec<(K, V)> {
        self.entries.clone()
    }
}

impl<K, V> Default for CqlMap<K, V> {
    fn default() -> Self {
        CqlMap { entries: Vec::new() }
    }
}

/// Canonical order of maps: compare entry by entry as key-value pairs (used only for
/// deterministic materialization; §2.3 states that maps are not orderable at the language
/// level).
impl<K: CanonOrd, V: CanonOrd> CanonOrd for CqlMap<K, V> {
    fn canon_cmp(&self, other: &Self) -> std::cmp::Ordering {
        let (mut i, mut j) = (0, 0);
        loop {
            match (self.entries.get(i), other.entries.get(j)) {
                (None, None) => return std::cmp::Ordering::Equal,
                (None, Some(_)) => return std::cmp::Ordering::Less,
                (Some(_), None) => return std::cmp::Ordering::Greater,
                (Some((ka, va)), Some((kb, vb))) => {
                    match ka.canon_cmp(kb).then_with(|| va.canon_cmp(vb)) {
                        std::cmp::Ordering::Equal => {
                            i += 1;
                            j += 1;
                        }
                        o => return o,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(xs: &[i64]) -> CqlSet<i64> {
        CqlSet::from_elems(xs.iter().copied())
    }

    #[test]
    fn set_dedup_sorted() {
        let s = set(&[3, 1, 2, 1, 3]);
        assert_eq!(s.as_slice(), &[1, 2, 3]);
        assert!(s.contains(&2));
        assert!(!s.contains(&4));
    }

    #[test]
    fn set_algebra() {
        let a = set(&[1, 2, 3]);
        let b = set(&[3, 4]);
        assert_eq!(a.union(&b).as_slice(), &[1, 2, 3, 4]);
        assert_eq!(a.inter(&b).as_slice(), &[3]);
        assert_eq!(a.diff(&b).as_slice(), &[1, 2]);
        assert!(set(&[1, 2]).is_subset(&a));
        assert!(!b.is_subset(&a));
        let c = set(&[1, 2]).cartesian(&set(&[10, 20]));
        assert_eq!(c.as_slice(), &[(1, 10), (1, 20), (2, 10), (2, 20)]);
    }

    #[test]
    fn set_the_only_traps() {
        assert_eq!(set(&[42]).the(), Ok(&42));
        assert_eq!(set(&[1, 2]).the(), Err(Trap::TheNonSingleton));
        assert_eq!(set(&[]).the(), Err(Trap::TheNonSingleton));
        assert_eq!(set(&[]).only(), Ok(None));
        assert_eq!(set(&[7]).only(), Ok(Some(&7)));
        assert_eq!(set(&[1, 2]).only(), Err(Trap::OnlyMulti));
    }

    #[test]
    fn set_union_all() {
        let fam = CqlSet::from_elems(vec![set(&[1, 2]), set(&[2, 3]), set(&[])]);
        assert_eq!(CqlSet::union_all(&fam).as_slice(), &[1, 2, 3]);
    }

    #[test]
    fn bag_semantics() {
        let b = CqlBag::from_elems([2, 1, 2, 3, 2]);
        assert_eq!(b.copies_in(&2), 3);
        assert_eq!(b.copies_in(&9), 0);
        assert_eq!(b.entry_count(), 3);
        assert_eq!(b.total_count(), 5);
        assert_eq!(b.to_set().as_slice(), &[1, 2, 3]);
        let expanded: Vec<_> = b.iter_expanded().copied().collect();
        assert_eq!(expanded, vec![1, 2, 2, 2, 3]); // canonical order + multiplicity expansion

        let c = CqlBag::from_elems([2, 4]);
        let u = b.bag_union(&c);
        assert_eq!(u.copies_in(&2), 4);
        assert_eq!(u.copies_in(&4), 1);

        let from_set = CqlBag::from_set(&set(&[5, 6]));
        assert_eq!(from_set.copies_in(&5), 1);
        assert_eq!(from_set.total_count(), 2);
    }

    #[test]
    fn map_semantics() {
        let m = CqlMap::from_vector(vec![("b".to_string(), 2), ("a".to_string(), 1), ("b".to_string(), 3)]);
        assert_eq!(m.len(), 2); // duplicate key: the later one wins
        assert_eq!(m.get(&"b".to_string()), Some(&3));
        assert_eq!(m.get(&"x".to_string()), None);

        let m2 = m.insert("c".to_string(), 4);
        assert_eq!(m.len(), 2); // the original map is unchanged (immutable semantics)
        assert_eq!(m2.len(), 3);
        let m3 = m2.remove(&"a".to_string());
        assert_eq!(m3.len(), 2);
        assert_eq!(m3.keys().as_slice(), &["b".to_string(), "c".to_string()]);

        let vals = m3.values();
        assert_eq!(vals.copies_in(&3), 1);
        assert_eq!(vals.copies_in(&4), 1);
        assert_eq!(
            m3.to_vector(),
            vec![("b".to_string(), 3), ("c".to_string(), 4)]
        );
    }

    #[test]
    fn collections_canon_ord_deterministic() {
        let a = set(&[1, 2]);
        let b = set(&[1, 3]);
        let c = set(&[1, 2, 3]);
        assert!(a.canon_cmp(&b) == std::cmp::Ordering::Less); // 2 < 3
        assert!(a.canon_cmp(&c) == std::cmp::Ordering::Less); // proper prefix
        assert!(a.canon_cmp(&a.clone()) == std::cmp::Ordering::Equal);
    }
}
