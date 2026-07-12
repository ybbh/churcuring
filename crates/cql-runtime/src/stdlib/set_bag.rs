//! Set / bag group (doc/cql.md Appendix B).

use std::hash::Hash;

use crate::collections::{CqlBag, CqlSet};
use crate::trap::CqlResult;
use crate::value::CanonOrd;

/// `size(s)`.
pub fn size<T>(s: &CqlSet<T>) -> i64 {
    s.len() as i64
}

/// `the(s)`: exactly one element ⇒ return it, otherwise `Trap::TheNonSingleton` (§5.3).
pub fn the<T: CanonOrd + Eq + Hash>(s: &CqlSet<T>) -> CqlResult<&T> {
    s.the()
}

/// `only(s)`: exactly one ⇒ `Some`; empty ⇒ `None`; multiple elements ⇒ `Trap::OnlyMulti`.
pub fn only<T: CanonOrd + Eq + Hash>(s: &CqlSet<T>) -> CqlResult<Option<&T>> {
    s.only()
}

/// `union_all(s)`: union of a family of sets.
pub fn union_all<T: CanonOrd + Eq + Hash + Clone>(s: &CqlSet<CqlSet<T>>) -> CqlSet<T> {
    CqlSet::union_all(s)
}

/// `bag_to_set(b)`: deduplicate.
pub fn bag_to_set<T: CanonOrd + Eq + Hash + Clone>(b: &CqlBag<T>) -> CqlSet<T> {
    b.to_set()
}

/// `set_to_bag(s)`: each element gets multiplicity 1.
pub fn set_to_bag<T: CanonOrd + Eq + Hash + Clone>(s: &CqlSet<T>) -> CqlBag<T> {
    CqlBag::from_set(s)
}

/// `copies_in(x, b)`: multiplicity.
pub fn copies_in<T: CanonOrd + Eq + Hash>(x: &T, b: &CqlBag<T>) -> i64 {
    b.copies_in(x)
}

/// `bag_union(a, b)`: multiplicities are added.
pub fn bag_union<T: CanonOrd + Eq + Hash + Clone>(a: &CqlBag<T>, b: &CqlBag<T>) -> CqlBag<T> {
    a.bag_union(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trap::Trap;

    #[test]
    fn set_bag_stdlib() {
        let s = CqlSet::from_elems([1, 2]);
        assert_eq!(size(&s), 2);
        assert_eq!(the(&CqlSet::from_elems([9])), Ok(&9));
        assert_eq!(the(&s), Err(Trap::TheNonSingleton));
        assert_eq!(only(&CqlSet::<i64>::new()), Ok(None));
        assert_eq!(only(&s), Err(Trap::OnlyMulti));

        let b = set_to_bag(&s);
        assert_eq!(copies_in(&1, &b), 1);
        let b2 = CqlBag::from_elems([1, 1, 3]);
        let u = bag_union(&b, &b2);
        assert_eq!(copies_in(&1, &u), 3);
        assert_eq!(bag_to_set(&u).as_slice(), &[1, 2, 3]);

        let fam = CqlSet::from_elems(vec![CqlSet::from_elems([1]), CqlSet::from_elems([1, 5])]);
        assert_eq!(union_all(&fam).as_slice(), &[1, 5]);
    }
}
