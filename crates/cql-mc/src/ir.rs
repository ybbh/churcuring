//! Checker-neutral model-checking IR (`doc/model-check.md` §2.5).
//!
//! A [`McSpec`] is a finite transition system: states are snapshots of tables
//! (`Key ⇀ Value` partial maps), transitions are named actions with bounded
//! parameter domains, properties are `always`/`eventually` predicates.
//! Both backends (Stateright explicit-state, z3.rs symbolic BMC) consume this
//! single IR, so their semantics are identical by construction and can be
//! differentially tested against each other.

/// Expression/value type. v1 covers `bool` and `int` (i64) only — the fragment
/// that is faithfully encodable in both concrete evaluation and SMT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    Bool,
    Int,
}

/// IR expression: the fragment shared by the concrete evaluator (`eval.rs`)
/// and the SMT encoder (`encode.rs`, z3 backend).
#[derive(Clone, Debug, PartialEq)]
pub enum McExpr {
    BoolLit(bool),
    IntLit(i64),
    /// Transition parameter, positional (index into `Transition::params`).
    Param(usize),
    /// Table lookup by key. The value is only meaningful when the key is
    /// present; well-formed specs guard lookups with [`McExpr::Contains`].
    Select { table: usize, key: Box<McExpr> },
    /// Key presence test.
    Contains { table: usize, key: Box<McExpr> },
    Not(Box<McExpr>),
    And(Vec<McExpr>),
    Or(Vec<McExpr>),
    Implies(Box<McExpr>, Box<McExpr>),
    Eq(Box<McExpr>, Box<McExpr>),
    Ne(Box<McExpr>, Box<McExpr>),
    Lt(Box<McExpr>, Box<McExpr>),
    Le(Box<McExpr>, Box<McExpr>),
    Gt(Box<McExpr>, Box<McExpr>),
    Ge(Box<McExpr>, Box<McExpr>),
    Add(Box<McExpr>, Box<McExpr>),
    Sub(Box<McExpr>, Box<McExpr>),
    Mul(Box<McExpr>, Box<McExpr>),
    /// Sum of a table's values over an explicit finite key domain
    /// (absent keys contribute 0). Bounded quantification stays concrete
    /// and quantifier-free for the SMT backend.
    Sum { table: usize, domain: Vec<i64> },
}

// ---- builder helpers (keep call sites terse) ----

pub fn param(i: usize) -> McExpr {
    McExpr::Param(i)
}
pub fn int(v: i64) -> McExpr {
    McExpr::IntLit(v)
}
pub fn bool_(v: bool) -> McExpr {
    McExpr::BoolLit(v)
}
pub fn select(table: usize, key: McExpr) -> McExpr {
    McExpr::Select {
        table,
        key: Box::new(key),
    }
}
pub fn contains(table: usize, key: McExpr) -> McExpr {
    McExpr::Contains {
        table,
        key: Box::new(key),
    }
}
pub fn not(e: McExpr) -> McExpr {
    McExpr::Not(Box::new(e))
}
pub fn and(es: Vec<McExpr>) -> McExpr {
    McExpr::And(es)
}
pub fn or(es: Vec<McExpr>) -> McExpr {
    McExpr::Or(es)
}
pub fn implies(a: McExpr, b: McExpr) -> McExpr {
    McExpr::Implies(Box::new(a), Box::new(b))
}
pub fn eq(a: McExpr, b: McExpr) -> McExpr {
    McExpr::Eq(Box::new(a), Box::new(b))
}
pub fn ne(a: McExpr, b: McExpr) -> McExpr {
    McExpr::Ne(Box::new(a), Box::new(b))
}
pub fn lt(a: McExpr, b: McExpr) -> McExpr {
    McExpr::Lt(Box::new(a), Box::new(b))
}
pub fn le(a: McExpr, b: McExpr) -> McExpr {
    McExpr::Le(Box::new(a), Box::new(b))
}
pub fn gt(a: McExpr, b: McExpr) -> McExpr {
    McExpr::Gt(Box::new(a), Box::new(b))
}
pub fn ge(a: McExpr, b: McExpr) -> McExpr {
    McExpr::Ge(Box::new(a), Box::new(b))
}
pub fn add(a: McExpr, b: McExpr) -> McExpr {
    McExpr::Add(Box::new(a), Box::new(b))
}
pub fn sub(a: McExpr, b: McExpr) -> McExpr {
    McExpr::Sub(Box::new(a), Box::new(b))
}
pub fn mul(a: McExpr, b: McExpr) -> McExpr {
    McExpr::Mul(Box::new(a), Box::new(b))
}
pub fn sum(table: usize, domain: Vec<i64>) -> McExpr {
    McExpr::Sum { table, domain }
}

/// Write kind, mirroring CQL `write_op` existence semantics (`doc/cql.md` §3.6):
/// `Insert` requires the key to be absent, `Update` requires it present,
/// `Delete` is a no-op when absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateKind {
    Insert,
    Update,
    Delete,
}

/// One write within a transition. `key`/`value` are evaluated against the
/// *current* state (CQL "application-time evaluation", §3.6).
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub table: usize,
    pub key: McExpr,
    pub kind: UpdateKind,
    /// New value for `Insert`/`Update`; ignored for `Delete`.
    pub value: Option<McExpr>,
}

/// A named action: bounded parameters, an enabling guard, and a set of writes.
///
/// Outcome semantics (`doc/model-check.md` §2.3):
/// - guard false ⇒ **disabled** (no edge);
/// - guard true but write-op conflict (two writes on the same `(table, key)`)
///   or an existence-constraint violation ⇒ **rejected** self-loop;
/// - otherwise ⇒ **applied**, all writes take effect atomically.
#[derive(Clone, Debug, PartialEq)]
pub struct Transition {
    pub name: String,
    pub params: Vec<Ty>,
    /// Finite domain per parameter (small-scope bound).
    pub param_domains: Vec<Vec<i64>>,
    pub guard: McExpr,
    pub updates: Vec<Update>,
}

/// A checkable property over states.
#[derive(Clone, Debug, PartialEq)]
pub struct Property {
    pub name: String,
    pub kind: PropertyKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PropertyKind {
    /// `[]φ` — must hold in every reachable state (safety).
    Always(McExpr),
    /// `<>φ` — must be reachable / unavoidable (liveness; backend support
    /// varies, see `doc/model-check.md` §7).
    Eventually(McExpr),
}

/// Table declaration (v1: `int` keys, `int` values; presence tracked
/// separately, i.e. a table is a partial map `int ⇀ int`).
#[derive(Clone, Debug, PartialEq)]
pub struct TableDecl {
    pub name: String,
}

/// The full bounded model.
#[derive(Clone, Debug, PartialEq)]
pub struct McSpec {
    pub tables: Vec<TableDecl>,
    /// Fixture initial rows: `(table, key, value)`.
    pub init: Vec<(usize, i64, i64)>,
    pub transitions: Vec<Transition>,
    pub properties: Vec<Property>,
    /// Trace bound k for the symbolic backend and reporting.
    pub depth: u32,
}

impl McSpec {
    /// All keys that any expression may observe: fixture keys ∪ parameter
    /// domains ∪ sum domains. Used by the SMT backend to frame array reads.
    pub fn relevant_keys(&self) -> Vec<i64> {
        let mut keys: Vec<i64> = self.init.iter().map(|(_, k, _)| *k).collect();
        for t in &self.transitions {
            for d in &t.param_domains {
                keys.extend(d.iter().copied());
            }
        }
        let from_expr = |e: &McExpr, keys: &mut Vec<i64>| {
            fn walk(e: &McExpr, keys: &mut Vec<i64>) {
                match e {
                    McExpr::Sum { domain, .. } => keys.extend(domain.iter().copied()),
                    McExpr::Select { key, .. } | McExpr::Contains { key, .. } => walk(key, keys),
                    McExpr::Not(a) => walk(a, keys),
                    McExpr::And(es) | McExpr::Or(es) => es.iter().for_each(|e| walk(e, keys)),
                    McExpr::Implies(a, b)
                    | McExpr::Eq(a, b)
                    | McExpr::Ne(a, b)
                    | McExpr::Lt(a, b)
                    | McExpr::Le(a, b)
                    | McExpr::Gt(a, b)
                    | McExpr::Ge(a, b)
                    | McExpr::Add(a, b)
                    | McExpr::Sub(a, b)
                    | McExpr::Mul(a, b) => {
                        walk(a, keys);
                        walk(b, keys);
                    }
                    _ => {}
                }
            }
            walk(e, keys);
        };
        for t in &self.transitions {
            from_expr(&t.guard, &mut keys);
            for u in &t.updates {
                from_expr(&u.key, &mut keys);
                if let Some(v) = &u.value {
                    from_expr(v, &mut keys);
                }
            }
        }
        for p in &self.properties {
            match &p.kind {
                PropertyKind::Always(e) | PropertyKind::Eventually(e) => from_expr(e, &mut keys),
            }
        }
        keys.sort_unstable();
        keys.dedup();
        keys
    }
}
