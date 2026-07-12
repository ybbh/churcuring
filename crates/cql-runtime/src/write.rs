//! write_op and atomic application (doc/cql.md §3.6, §5.2).
//!
//! - `WriteOp`: type-erased update descriptor (`insert`/`update`/`delete`, §3.6).
//! - `FunVal`: type-erased runtime value of a pure function (definition site + captured
//!   values, §3.6).
//! - `apply_write_ops`: atomic application at runtime — conflict checking, existence
//!   constraints, FK checking, invariant hooks; any violation ⇒ the whole action is rejected
//!   (no write is applied, §5.2).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use thiserror::Error;

use crate::value::{CanonOrd, Value};

// ---------------------------------------------------------------------------
// TableRef / FunVal (§3.6 runtime descriptors)
// ---------------------------------------------------------------------------

/// Table identifier: static id + name (§3.6 `table_ref`, §6.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableRef {
    pub id: u64,
    pub name: String,
}

impl TableRef {
    /// Create a table identifier from a static id and a name.
    pub fn new(id: u64, name: impl Into<String>) -> Self {
        TableRef { id, name: name.into() }
    }
}

impl CanonOrd for TableRef {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        self.id.cmp(&other.id).then_with(|| self.name.canon_cmp(&other.name))
    }
}

/// Type-erased runtime value of a pure function (§3.6 `fun_val`): definition site + captured
/// values.
///
/// Equality/hashing is by `(def_id, captures)` (required by the §3.6 conflict rule: two
/// distinct write constructions on the same key must not be silently merged by set
/// deduplication). Captured values must not contain function-typed values (guaranteed at
/// compile time, §4.2).
pub trait FunVal {
    /// Evaluate against the current row value at application time (§3.6
    /// evaluate-at-application-time semantics).
    fn call(&self, v: &Value) -> Value;
    /// Definition site (a static id assigned at compile time).
    fn def_id(&self) -> u64;
    /// Capture list (§4.2 explicit captures, statically enumerable).
    fn captures(&self) -> &[Value];
}

/// A `FunVal` implementation over a closure: wraps a Rust closure for hosts/tests.
pub struct ClosureFunVal<F> {
    def_id: u64,
    captures: Vec<Value>,
    f: F,
}

impl<F> ClosureFunVal<F> {
    /// Wrap a Rust closure as a `FunVal` with the given definition-site id and captures.
    pub fn new(def_id: u64, captures: Vec<Value>, f: F) -> Self {
        ClosureFunVal { def_id, captures, f }
    }
}

impl<F: Fn(&Value) -> Value> FunVal for ClosureFunVal<F> {
    fn call(&self, v: &Value) -> Value {
        (self.f)(v)
    }

    fn def_id(&self) -> u64 {
        self.def_id
    }

    fn captures(&self) -> &[Value] {
        &self.captures
    }
}

// ---------------------------------------------------------------------------
// WriteOp (§3.6)
// ---------------------------------------------------------------------------

/// Type-erased update descriptor (§3.6 `enum write_op`). Built-in hashable, so it can be an
/// element of a `set`.
#[derive(Clone)]
pub enum WriteOp {
    /// `insert(t, row)`: add a new row; the key is extracted from the key fields inside the
    /// row and must not already exist.
    Insert { table: TableRef, row: Value },
    /// `update(t, k, f)`: evaluate `f(v)` against the current row value at application time
    /// and overwrite the non-key fields; the key must exist.
    Update { table: TableRef, key: Value, transform: Arc<dyn FunVal> },
    /// `delete(t, k)`: delete; a missing key is a no-op.
    Delete { table: TableRef, key: Value },
}

impl WriteOp {
    /// The (table, key) this op acts on. The key of an `Insert` must be extracted from the
    /// row via the registry's key columns (see `TableRegistry::row_key`).
    pub fn explicit_key(&self) -> Option<(&TableRef, &Value)> {
        match self {
            WriteOp::Update { table, key, .. } | WriteOp::Delete { table, key } => Some((table, key)),
            WriteOp::Insert { .. } => None,
        }
    }
}

impl fmt::Debug for WriteOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteOp::Insert { table, row } => {
                write!(f, "Insert({:?}, {:?})", table.name, row)
            }
            WriteOp::Update { table, key, transform } => write!(
                f,
                "Update({:?}, {:?}, fun#{})",
                table.name,
                key,
                transform.def_id()
            ),
            WriteOp::Delete { table, key } => {
                write!(f, "Delete({:?}, {:?})", table.name, key)
            }
        }
    }
}

/// write_op equality: keys/rows compare by data, transform closures by (definition site +
/// captured values) (§3.6).
impl PartialEq for WriteOp {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (WriteOp::Insert { table: t1, row: r1 }, WriteOp::Insert { table: t2, row: r2 }) => {
                t1 == t2 && r1 == r2
            }
            (
                WriteOp::Update { table: t1, key: k1, transform: f1 },
                WriteOp::Update { table: t2, key: k2, transform: f2 },
            ) => {
                t1 == t2
                    && k1 == k2
                    && f1.def_id() == f2.def_id()
                    && f1.captures() == f2.captures()
            }
            (WriteOp::Delete { table: t1, key: k1 }, WriteOp::Delete { table: t2, key: k2 }) => {
                t1 == t2 && k1 == k2
            }
            _ => false,
        }
    }
}
impl Eq for WriteOp {}

impl Hash for WriteOp {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            WriteOp::Insert { table, row } => {
                0u8.hash(state);
                table.hash(state);
                state.write(&row.canonical_bytes());
            }
            WriteOp::Update { table, key, transform } => {
                1u8.hash(state);
                table.hash(state);
                state.write(&key.canonical_bytes());
                transform.def_id().hash(state);
                for c in transform.captures() {
                    state.write(&c.canonical_bytes());
                }
            }
            WriteOp::Delete { table, key } => {
                2u8.hash(state);
                table.hash(state);
                state.write(&key.canonical_bytes());
            }
        }
    }
}

impl CanonOrd for WriteOp {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        let rank = |op: &WriteOp| match op {
            WriteOp::Insert { .. } => 0,
            WriteOp::Update { .. } => 1,
            WriteOp::Delete { .. } => 2,
        };
        rank(self).cmp(&rank(other)).then_with(|| match (self, other) {
            (WriteOp::Insert { table: t1, row: r1 }, WriteOp::Insert { table: t2, row: r2 }) => {
                t1.canon_cmp(t2).then_with(|| r1.canon_cmp(r2))
            }
            (
                WriteOp::Update { table: t1, key: k1, transform: f1 },
                WriteOp::Update { table: t2, key: k2, transform: f2 },
            ) => t1
                .canon_cmp(t2)
                .then_with(|| k1.canon_cmp(k2))
                .then_with(|| f1.def_id().cmp(&f2.def_id()))
                .then_with(|| f1.captures().canon_cmp(f2.captures())),
            (
                WriteOp::Delete { table: t1, key: k1 },
                WriteOp::Delete { table: t2, key: k2 },
            ) => t1.canon_cmp(t2).then_with(|| k1.canon_cmp(k2)),
            _ => Ordering::Equal, // equal rank ⇒ same variant
        })
    }
}

/// `Ord` delegates to the canonical order, for use by `BTreeSet<WriteOp>` (the runtime
/// representation of `set<write_op>`).
impl Ord for WriteOp {
    fn cmp(&self, other: &Self) -> Ordering {
        self.canon_cmp(other)
    }
}

impl PartialOrd for WriteOp {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// ---------------------------------------------------------------------------
// ApplyError (§3.6, §5.2: reject the whole action)
// ---------------------------------------------------------------------------

/// Reasons why applying an action fails (all are data-constraint violations, not the traps
/// of §5.3, §3.6).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ApplyError {
    /// Multiple write_ops on the same (table, key) within one action (§3.6 conflict rule).
    #[error("write conflict: multiple write_ops on the same (table, key)")]
    WriteConflict,
    /// The key of an insert already exists at application time (§3.6 existence constraint).
    #[error("insert: key already exists")]
    InsertKeyExists,
    /// The key of an update does not exist at application time (§3.6 existence constraint).
    #[error("update: key not found")]
    UpdateKeyNotFound,
    /// Foreign-key referential-integrity violation (§3.3, §5.2; includes RESTRICT delete
    /// violations).
    #[error("foreign key violation on {fk}: {detail}")]
    FkViolation { fk: String, detail: String },
    /// Invariant violation (§5.2, Appendix C).
    #[error("invariant violation: {name}")]
    InvariantViolation { name: String },
}

// ---------------------------------------------------------------------------
// TableRegistry
// ---------------------------------------------------------------------------

/// Foreign-key declaration (§3.3): the `cols` of the `from` table reference the primary key
/// of the `to` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkDecl {
    pub from: TableRef,
    pub cols: Vec<String>,
    pub to: TableRef,
}

/// A single type-erased in-memory table: rows store the full record `Value`; the key is the
/// `Value` extracted from the primary-key columns.
#[derive(Debug, Clone, Default)]
struct ErasedTable {
    tref: Option<TableRef>,
    key_cols: Vec<String>,
    rows: BTreeMap<Value, Value>,
}

/// Invariant predicate hook type (§5.2, Appendix C).
type InvariantHook = Arc<dyn Fn(&TableRegistry) -> bool>;

/// Table registry: registers multiple type-erased in-memory tables (`key_val → row_val`) by
/// `TableRef`, together with foreign-key declarations and invariant hooks (§5.2, Appendix C).
#[derive(Clone, Default)]
pub struct TableRegistry {
    tables: BTreeMap<u64, ErasedTable>,
    fks: Vec<FkDecl>,
    /// (name, predicate); `Arc` makes the registry cloneable (atomic application relies on
    /// the "clone first, then replace" strategy).
    invariants: Vec<(String, InvariantHook)>,
}

impl TableRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a table: `key_cols` are the primary-key column names (used to extract the key
    /// from a row, §2.2).
    pub fn register_table(&mut self, tref: TableRef, key_cols: Vec<String>) {
        self.tables.insert(
            tref.id,
            ErasedTable { tref: Some(tref), key_cols, rows: BTreeMap::new() },
        );
    }

    /// Declare a foreign key (§3.3).
    pub fn add_fk(&mut self, fk: FkDecl) {
        self.fks.push(fk);
    }

    /// Register an invariant hook: evaluated against the final state after application;
    /// false ⇒ reject the action (§5.2, Appendix C).
    pub fn add_invariant(
        &mut self,
        name: impl Into<String>,
        f: impl Fn(&TableRegistry) -> bool + 'static,
    ) {
        self.invariants.push((name.into(), Arc::new(f)));
    }

    /// Extract the key from a row record by the primary-key columns: single column ⇒ the
    /// column value; multiple columns ⇒ `Value::Tuple` (§2.2 composite keys).
    pub fn row_key(&self, table_id: u64, row: &Value) -> Option<Value> {
        let t = self.tables.get(&table_id)?;
        extract_key(&t.key_cols, row)
    }

    /// Insert a row directly (for fixture assembly/tests; duplicate keys overwrite).
    pub fn insert_row(&mut self, tref: &TableRef, row: Value) {
        if let Some(key) = self.row_key(tref.id, &row) {
            if let Some(t) = self.tables.get_mut(&tref.id) {
                t.rows.insert(key, row);
            }
        }
    }

    /// The runtime form of `lookup(t, k)`: returns the full row (including key fields).
    pub fn lookup(&self, table_id: u64, key: &Value) -> Option<&Value> {
        self.tables.get(&table_id)?.rows.get(key)
    }

    /// Number of rows in a table.
    pub fn table_len(&self, table_id: u64) -> usize {
        self.tables.get(&table_id).map_or(0, |t| t.rows.len())
    }

    /// Scan a table in ascending key order (canonical order) (for invariant predicates and
    /// tests).
    pub fn scan(&self, table_id: u64) -> impl Iterator<Item = (&Value, &Value)> {
        self.tables.get(&table_id).into_iter().flat_map(|t| t.rows.iter())
    }

    /// The list of identifiers of the registered tables.
    pub fn table_refs(&self) -> Vec<TableRef> {
        self.tables.values().filter_map(|t| t.tref.clone()).collect()
    }
}

/// Extract the key value from a row record by columns; a non-record row or a missing column ⇒
/// `None`.
fn extract_key(cols: &[String], row: &Value) -> Option<Value> {
    let Value::Record(fields) = row else { return None };
    let vals: Option<Vec<Value>> = cols.iter().map(|c| fields.get(c).cloned()).collect();
    let vals = vals?;
    Some(if vals.len() == 1 {
        vals.into_iter().next().unwrap()
    } else {
        Value::Tuple(vals)
    })
}

// ---------------------------------------------------------------------------
// apply_write_ops (§5.2 atomic application)
// ---------------------------------------------------------------------------

/// Atomically apply a set of write_ops (§3.6, §5.2):
///
/// 1. At most one op per (table, key) ⇒ otherwise `ApplyError::WriteConflict`;
/// 2. An insert key must not exist, an update key must exist, delete of a missing key is a
///    no-op;
/// 3. An update transform is evaluated against the **current row value at application time**
///    (including the effects of earlier writes in the same action, §3.6);
/// 4. After application, check all foreign keys against the final state (each non-none
///    component of the referencing side must exist in the referenced key set; deleting a row
///    that is still referenced violates RESTRICT and is covered by the same check, §5.2) ⇒
///    `FkViolation`;
/// 5. After application, check all invariant hooks ⇒ `InvariantViolation`;
/// 6. All-or-nothing semantics: apply and validate on a clone first, then replace as a whole
///    on success, ensuring atomicity.
pub fn apply_write_ops(
    registry: &mut TableRegistry,
    ops: &BTreeSet<WriteOp>,
) -> Result<(), ApplyError> {
    // 1. Conflict check: at most one op per (table, key) (§3.6).
    let mut seen: BTreeSet<(u64, Value)> = BTreeSet::new();
    for op in ops {
        let (table_id, key) = match op {
            WriteOp::Insert { table, row } => {
                let key = registry.row_key(table.id, row).ok_or(ApplyError::FkViolation {
                    fk: table.name.clone(),
                    detail: "insert row is not a record with the declared key columns".into(),
                })?;
                (table.id, key)
            }
            WriteOp::Update { table, key, .. } | WriteOp::Delete { table, key } => {
                (table.id, key.clone())
            }
        };
        if !seen.insert((table_id, key)) {
            return Err(ApplyError::WriteConflict);
        }
    }

    // 2–3. Apply on a clone (ops are iterated in canonical order, deterministic).
    let mut work = registry.clone();
    for op in ops {
        match op {
            WriteOp::Insert { table, row } => {
                let key = work.row_key(table.id, row).expect("key extracted in phase 1");
                let t = work.tables.get_mut(&table.id).ok_or(ApplyError::FkViolation {
                    fk: table.name.clone(),
                    detail: "table not registered".into(),
                })?;
                if t.rows.contains_key(&key) {
                    return Err(ApplyError::InsertKeyExists);
                }
                t.rows.insert(key, row.clone());
            }
            WriteOp::Update { table, key, transform } => {
                let t = work.tables.get_mut(&table.id).ok_or(ApplyError::FkViolation {
                    fk: table.name.clone(),
                    detail: "table not registered".into(),
                })?;
                match t.rows.get(key) {
                    None => return Err(ApplyError::UpdateKeyNotFound),
                    Some(current) => {
                        let new_row = transform.call(current);
                        t.rows.insert(key.clone(), new_row);
                    }
                }
            }
            WriteOp::Delete { table, key } => {
                if let Some(t) = work.tables.get_mut(&table.id) {
                    t.rows.remove(key); // missing key is a no-op (§3.6)
                }
            }
        }
    }

    // 4. Foreign-key check (the final state after application, §5.2).
    for fk in &registry.fks {
        check_fk(&work, fk)?;
    }

    // 5. Invariant hooks (§5.2, Appendix C).
    for (name, pred) in &registry.invariants {
        if !pred(&work) {
            return Err(ApplyError::InvariantViolation { name: name.clone() });
        }
    }

    // 6. All checks passed ⇒ replace as a whole (atomicity).
    *registry = work;
    Ok(())
}

/// Foreign-key check: the foreign-key value of any row on the referencing side (all
/// components non-`none`) must exist in the key set of the referenced table; if any component
/// is `none` the row is exempt from the check (NULL semantics, §5.2). Deleting a row that is
/// still referenced naturally violates the check.
fn check_fk(reg: &TableRegistry, fk: &FkDecl) -> Result<(), ApplyError> {
    let Some(from) = reg.tables.get(&fk.from.id) else { return Ok(()) };
    let Some(to) = reg.tables.get(&fk.to.id) else { return Ok(()) };
    for row in from.rows.values() {
        let Value::Record(fields) = row else { continue };
        let mut vals = Vec::with_capacity(fk.cols.len());
        let mut has_none = false;
        for col in &fk.cols {
            match fields.get(col) {
                Some(Value::Option(None)) => {
                    has_none = true;
                    break;
                }
                Some(v) => vals.push(v.clone()),
                None => {
                    return Err(ApplyError::FkViolation {
                        fk: fk.from.name.clone(),
                        detail: format!("missing foreign key column `{col}`"),
                    });
                }
            }
        }
        if has_none {
            continue; // nullable foreign key: exempt from the check (§5.2)
        }
        let key = if vals.len() == 1 {
            vals.pop().unwrap()
        } else {
            Value::Tuple(vals)
        };
        if !to.rows.contains_key(&key) {
            return Err(ApplyError::FkViolation {
                fk: fk.from.name.clone(),
                detail: format!(
                    "key {:?} references missing row in table `{}`",
                    key, fk.to.name
                ),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const USERS: u64 = 1;
    const ORDERS: u64 = 2;

    fn users_ref() -> TableRef {
        TableRef::new(USERS, "users")
    }

    fn orders_ref() -> TableRef {
        TableRef::new(ORDERS, "orders")
    }

    fn user(id: i64, name: &str) -> Value {
        let mut r = BTreeMap::new();
        r.insert("id".to_string(), Value::Int(id));
        r.insert("name".to_string(), Value::Str(name.to_string()));
        Value::Record(r)
    }

    fn order(oid: i64, uid: Option<i64>) -> Value {
        let mut r = BTreeMap::new();
        r.insert("order_id".to_string(), Value::Int(oid));
        r.insert(
            "user_id".to_string(),
            match uid {
                Some(u) => Value::Int(u),
                None => Value::Option(None),
            },
        );
        Value::Record(r)
    }

    fn registry() -> TableRegistry {
        let mut reg = TableRegistry::new();
        reg.register_table(users_ref(), vec!["id".to_string()]);
        reg.register_table(orders_ref(), vec!["order_id".to_string()]);
        reg.add_fk(FkDecl {
            from: orders_ref(),
            cols: vec!["user_id".to_string()],
            to: users_ref(),
        });
        reg
    }

    fn ops(v: Vec<WriteOp>) -> BTreeSet<WriteOp> {
        v.into_iter().collect()
    }

    fn rename_transform(def_id: u64, new_name: &str) -> Arc<dyn FunVal> {
        let cap = Value::Str(new_name.to_string());
        Arc::new(ClosureFunVal::new(def_id, vec![cap.clone()], move |row: &Value| {
            let Value::Record(mut fields) = row.clone() else { return row.clone() };
            fields.insert("name".to_string(), cap.clone());
            Value::Record(fields)
        }))
    }

    #[test]
    fn insert_update_delete_happy_path() {
        let mut reg = registry();
        let result = apply_write_ops(
            &mut reg,
            &ops(vec![
                WriteOp::Insert { table: users_ref(), row: user(1, "a") },
                WriteOp::Insert { table: orders_ref(), row: order(10, Some(1)) },
            ]),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(reg.lookup(USERS, &Value::Int(1)), Some(&user(1, "a")));

        // update: the transform is evaluated against the current row value
        let result = apply_write_ops(
            &mut reg,
            &ops(vec![WriteOp::Update {
                table: users_ref(),
                key: Value::Int(1),
                transform: rename_transform(7, "b"),
            }]),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(reg.lookup(USERS, &Value::Int(1)), Some(&user(1, "b")));

        // delete an existing key + delete a missing key (no-op)
        let result = apply_write_ops(
            &mut reg,
            &ops(vec![
                WriteOp::Delete { table: orders_ref(), key: Value::Int(10) },
                WriteOp::Delete { table: users_ref(), key: Value::Int(99) },
                WriteOp::Delete { table: users_ref(), key: Value::Int(1) },
            ]),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(reg.table_len(USERS), 0);
    }

    #[test]
    fn conflict_same_table_key_rejected() {
        let mut reg = registry();
        let result = apply_write_ops(
            &mut reg,
            &ops(vec![
                WriteOp::Insert { table: users_ref(), row: user(1, "a") },
                WriteOp::Delete { table: users_ref(), key: Value::Int(1) },
            ]),
        );
        assert_eq!(result, Err(ApplyError::WriteConflict));
        // Two distinct updates (different definition sites) on the same key ⇒ conflict, not
        // silently merged by set deduplication (§3.6)
        let result = apply_write_ops(
            &mut reg,
            &ops(vec![
                WriteOp::Update {
                    table: users_ref(),
                    key: Value::Int(1),
                    transform: rename_transform(1, "x"),
                },
                WriteOp::Update {
                    table: users_ref(),
                    key: Value::Int(1),
                    transform: rename_transform(2, "y"),
                },
            ]),
        );
        assert_eq!(result, Err(ApplyError::WriteConflict));
    }

    #[test]
    fn write_op_set_dedups_identical_ops() {
        // Completely identical ops (same definition site + same captures) are deduplicated by
        // the set and do not count as a conflict (§3.6)
        let mut reg = registry();
        reg.insert_row(&users_ref(), user(1, "a"));
        let mut set = BTreeSet::new();
        set.insert(WriteOp::Update {
            table: users_ref(),
            key: Value::Int(1),
            transform: rename_transform(1, "x"),
        });
        set.insert(WriteOp::Update {
            table: users_ref(),
            key: Value::Int(1),
            transform: rename_transform(1, "x"),
        });
        assert_eq!(set.len(), 1); // structurally equal ⇒ deduplicated
        assert_eq!(apply_write_ops(&mut reg, &set), Ok(()));
        assert_eq!(reg.lookup(USERS, &Value::Int(1)), Some(&user(1, "x")));
    }

    #[test]
    fn existence_constraints() {
        let mut reg = registry();
        reg.insert_row(&users_ref(), user(1, "a"));
        // insert of an already existing key
        let result = apply_write_ops(
            &mut reg,
            &ops(vec![WriteOp::Insert { table: users_ref(), row: user(1, "b") }]),
        );
        assert_eq!(result, Err(ApplyError::InsertKeyExists));
        // update of a missing key
        let result = apply_write_ops(
            &mut reg,
            &ops(vec![WriteOp::Update {
                table: users_ref(),
                key: Value::Int(9),
                transform: rename_transform(1, "x"),
            }]),
        );
        assert_eq!(result, Err(ApplyError::UpdateKeyNotFound));
    }

    #[test]
    fn fk_violations() {
        let mut reg = registry();
        reg.insert_row(&users_ref(), user(1, "a"));
        // Inserting an order that references a nonexistent user ⇒ FkViolation
        let result = apply_write_ops(
            &mut reg,
            &ops(vec![WriteOp::Insert { table: orders_ref(), row: order(10, Some(99)) }]),
        );
        assert!(matches!(result, Err(ApplyError::FkViolation { .. })));
        assert_eq!(reg.table_len(ORDERS), 0); // atomicity: nothing was persisted

        // Nullable foreign key: user_id = none is exempt from the check
        let result = apply_write_ops(
            &mut reg,
            &ops(vec![WriteOp::Insert { table: orders_ref(), row: order(11, None) }]),
        );
        assert_eq!(result, Ok(()));

        // RESTRICT: deleting a user that is still referenced ⇒ FkViolation
        apply_write_ops(
            &mut reg,
            &ops(vec![WriteOp::Insert { table: orders_ref(), row: order(12, Some(1)) }]),
        )
        .unwrap();
        let result = apply_write_ops(
            &mut reg,
            &ops(vec![WriteOp::Delete { table: users_ref(), key: Value::Int(1) }]),
        );
        assert!(matches!(result, Err(ApplyError::FkViolation { .. })));
        assert_eq!(reg.lookup(USERS, &Value::Int(1)), Some(&user(1, "a"))); // not deleted
    }

    #[test]
    fn atomicity_partial_failure_writes_nothing() {
        let mut reg = registry();
        reg.insert_row(&users_ref(), user(1, "a"));
        // Same action: one valid insert + one invalid insert (key already exists) ⇒ the whole
        // action is rejected
        let result = apply_write_ops(
            &mut reg,
            &ops(vec![
                WriteOp::Insert { table: users_ref(), row: user(2, "b") },
                WriteOp::Insert { table: users_ref(), row: user(1, "dup") },
            ]),
        );
        assert_eq!(result, Err(ApplyError::InsertKeyExists));
        assert_eq!(reg.table_len(USERS), 1); // user(2) was not persisted
        assert_eq!(reg.lookup(USERS, &Value::Int(1)), Some(&user(1, "a")));
    }

    #[test]
    fn invariant_hook() {
        let mut reg = registry();
        reg.add_invariant("users_nonempty", |reg| reg.table_len(USERS) > 0);
        reg.insert_row(&users_ref(), user(1, "a"));
        // Deleting the last user ⇒ invariant violation
        let result = apply_write_ops(
            &mut reg,
            &ops(vec![WriteOp::Delete { table: users_ref(), key: Value::Int(1) }]),
        );
        assert_eq!(
            result,
            Err(ApplyError::InvariantViolation { name: "users_nonempty".into() })
        );
        assert_eq!(reg.table_len(USERS), 1); // nothing was persisted
    }
}
