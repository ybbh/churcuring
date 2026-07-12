//! Table abstraction and in-memory table implementations (doc/cql.md §2.2, §4.3, §5.2, §6.2).
//!
//! - `Table<K, V>`: the read-only table interface — `lookup` returns an Object (`value t`,
//!   §2.2), `scan_all` iterates in ascending key order (= canonical order, §5.1).
//! - `IndexedTable`: a reserved interface for secondary-index equality scans (§3.3, §5.5
//!   index plans); the MVP may implement it with a full scan plus filter.
//! - `MemTable`: an in-memory table backed by `BTreeMap`, used for tests and fixtures
//!   (Appendix C); `snapshot()` clones to obtain a consistent snapshot (§5.2 snapshot
//!   isolation).

use std::collections::BTreeMap;

use crate::value::Value;

/// Read-only table abstraction: a table is a partial map `{ Key → Object }` (§1, §2.2).
///
/// Compiled queries read tables through this interface (§6.4: a component imports the
/// `table` resource).
pub trait Table<K, V> {
    /// `lookup(t, k)`: point lookup, returns an Object (the non-key fields); missing ⇒
    /// `None` (§4.3).
    fn lookup(&self, key: &K) -> Option<&V>;

    /// Full-table scan, in ascending key order (= canonical order, §5.1, §6.4).
    fn scan_all(&self) -> Box<dyn Iterator<Item = (&K, &V)> + '_>;

    /// Number of rows.
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Secondary-index extension (§3.3): once declared, the optimizer automatically uses it for
/// equality predicates matching its columns (§5.5 index plans).
///
/// The MVP may implement it with a full scan plus filter; the interface is reserved so it can
/// later be replaced by a real index structure.
pub trait IndexedTable<K, V, I>: Table<K, V> {
    /// Index equality scan: `idx` is the index identifier, `eq_vals` the equality value for
    /// each column of the index (type-erased, §6.2). The result is still in ascending key
    /// order.
    fn idx_scan(&self, idx: I, eq_vals: &[Value]) -> Box<dyn Iterator<Item = (&K, &V)> + '_>;
}

/// In-memory table backed by `BTreeMap`: used for tests and fixtures (Appendix C).
///
/// Corresponds to the concrete tables such as `Table<i64, UserValue>` consumed by compiled
/// code in §6.4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemTable<K, V> {
    rows: BTreeMap<K, V>,
}

impl<K: Ord, V> MemTable<K, V> {
    /// Create an empty in-memory table.
    pub fn new() -> Self {
        MemTable { rows: BTreeMap::new() }
    }

    /// Construct from key-value pairs (fixture literals).
    pub fn from_entries<I: IntoIterator<Item = (K, V)>>(entries: I) -> Self {
        MemTable { rows: entries.into_iter().collect() }
    }

    /// Consistent snapshot: cloning is the snapshot (§5.2: a query obtains a single
    /// consistent snapshot at call time and reads from it throughout; the snapshot is
    /// isolated from later writes).
    pub fn snapshot(&self) -> MemTable<K, V>
    where
        K: Clone,
        V: Clone,
    {
        self.clone()
    }

    /// Insert/overwrite (used for fixture assembly and mutable scenarios).
    pub fn insert(&mut self, k: K, v: V) {
        self.rows.insert(k, v);
    }

    /// Update an existing key in place; returns `false` if the key does not exist.
    pub fn update(&mut self, k: &K, f: impl FnOnce(&V) -> V) -> bool
    where
        K: Clone,
    {
        match self.rows.get(k) {
            Some(v) => {
                let new_v = f(v);
                self.rows.insert(k.clone(), new_v);
                true
            }
            None => false,
        }
    }

    /// Delete; a missing key is a no-op (§3.6 lenient delete semantics).
    pub fn delete(&mut self, k: &K) {
        self.rows.remove(k);
    }
}

impl<K: Ord, V> Default for MemTable<K, V> {
    fn default() -> Self {
        MemTable::new()
    }
}

impl<K: Ord, V> Table<K, V> for MemTable<K, V> {
    fn lookup(&self, key: &K) -> Option<&V> {
        self.rows.get(key)
    }

    fn scan_all(&self) -> Box<dyn Iterator<Item = (&K, &V)> + '_> {
        Box::new(self.rows.iter())
    }

    fn len(&self) -> usize {
        self.rows.len()
    }
}

/// In-memory table with secondary indexes: both rows and keys are type-erased `Value` (§6.2),
/// indexes are declared as `index identifier → column name list`; `idx_scan` is implemented
/// as a full scan plus equality filter (MVP, §5.5 full-scan plan — affects performance only,
/// not results; the interface is reserved so it can later be replaced by a real index
/// structure).
#[derive(Debug, Clone)]
pub struct SecondaryIndexTable<I> {
    table: MemTable<Value, Value>,
    indexes: Vec<(I, Vec<String>)>,
}

impl<I> SecondaryIndexTable<I> {
    /// Wrap an in-memory table with the given secondary-index declarations.
    pub fn new(table: MemTable<Value, Value>, indexes: Vec<(I, Vec<String>)>) -> Self {
        SecondaryIndexTable { table, indexes }
    }

    /// The underlying in-memory table.
    pub fn table(&self) -> &MemTable<Value, Value> {
        &self.table
    }
}

impl<I> Table<Value, Value> for SecondaryIndexTable<I> {
    fn lookup(&self, key: &Value) -> Option<&Value> {
        self.table.lookup(key)
    }

    fn scan_all(&self) -> Box<dyn Iterator<Item = (&Value, &Value)> + '_> {
        self.table.scan_all()
    }

    fn len(&self) -> usize {
        self.table.len()
    }
}

impl<I: PartialEq> IndexedTable<Value, Value, I> for SecondaryIndexTable<I> {
    fn idx_scan(&self, idx: I, eq_vals: &[Value]) -> Box<dyn Iterator<Item = (&Value, &Value)> + '_> {
        let cols = self
            .indexes
            .iter()
            .find(|(i, _)| *i == idx)
            .map(|(_, cols)| cols.clone());
        let eq_vals = eq_vals.to_vec();
        Box::new(self.table.scan_all().filter(move |(_, row)| {
            let Some(cols) = &cols else { return false };
            let Value::Record(fields) = row else { return false };
            cols.len() == eq_vals.len()
                && cols.iter().zip(&eq_vals).all(|(c, v)| fields.get(c) == Some(v))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memtable_scan_sorted_by_key() {
        let mut t = MemTable::new();
        t.insert(3, "c");
        t.insert(1, "a");
        t.insert(2, "b");
        let scanned: Vec<_> = t.scan_all().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(scanned, vec![(1, "a"), (2, "b"), (3, "c")]);
        assert_eq!(t.lookup(&2), Some(&"b"));
        assert_eq!(t.lookup(&9), None);
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn memtable_update_delete() {
        let mut t = MemTable::new();
        t.insert(1, 10);
        assert!(t.update(&1, |v| v + 1));
        assert_eq!(t.lookup(&1), Some(&11));
        assert!(!t.update(&9, |v| v + 1)); // key does not exist
        t.delete(&1);
        t.delete(&1); // no-op
        assert!(t.is_empty());
    }

    #[test]
    fn snapshot_isolated() {
        let mut t = MemTable::new();
        t.insert(1, "a");
        let snap = t.snapshot();
        t.insert(2, "b");
        t.delete(&1);
        // The snapshot remains unchanged (§5.2 snapshot isolation)
        assert_eq!(snap.len(), 1);
        assert_eq!(snap.lookup(&1), Some(&"a"));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn secondary_index_scan_filters() {
        let row = |oid: i64, uid: i64| {
            let mut r = std::collections::BTreeMap::new();
            r.insert("order_id".to_string(), Value::Int(oid));
            r.insert("user_id".to_string(), Value::Int(uid));
            (Value::Int(oid), Value::Record(r))
        };
        let t = MemTable::from_entries(vec![row(1, 10), row(2, 20), row(3, 10)]);
        let idx = SecondaryIndexTable::new(t, vec![("by_user", vec!["user_id".to_string()])]);
        let hit: Vec<_> = idx
            .idx_scan("by_user", &[Value::Int(10)])
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(hit, vec![Value::Int(1), Value::Int(3)]); // ascending key order
        assert_eq!(idx.idx_scan("by_user", &[Value::Int(99)]).count(), 0);
        assert_eq!(idx.idx_scan("no_such", &[]).count(), 0);
    }
}
