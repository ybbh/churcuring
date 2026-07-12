//! cql-runtime: runtime library for code compiled from CQL.
//!
//! Module layout (designed per doc/cql.md):
//!
//! - [`value`]: `Date`/`Decimal`/canonical ordering [`value::CanonOrd`]/type-erased
//!   [`value::Value`] (§2, §5.1, §6.2);
//! - [`collections`]: `CqlSet`/`CqlBag`/`CqlMap` (§2.1, §4.7, §4.10);
//! - [`table`]: the `Table`/`IndexedTable` abstractions and in-memory tables (§2.2, §4.3, §5.2);
//! - [`write`]: `write_op`, `FunVal`, `TableRegistry` and atomic application (§3.6, §5.2);
//! - [`trap`]: `Trap` and checked `int` arithmetic (§5.3);
//! - [`stdlib`]: the full set of pure functions from Appendix B plus the `aggregate`
//!   combinator (§4.8.3, Appendix B).

#![forbid(unsafe_code)]

pub mod collections;
pub mod stdlib;
pub mod table;
pub mod trap;
pub mod value;
pub mod write;

// Top-level re-export of commonly used types.
pub use collections::{CqlBag, CqlMap, CqlSet};
pub use table::{IndexedTable, MemTable, SecondaryIndexTable, Table};
pub use trap::{
    checked_add, checked_div, checked_mul, checked_neg, checked_rem, checked_sub, CqlResult, Trap,
};
pub use value::{
    float_as_int, int_as_decimal, int_as_float, CanonOrd, Date, Decimal, Value, DECIMAL_MAX_M,
    DECIMAL_MAX_SCALE,
};
pub use write::{
    apply_write_ops, ApplyError, ClosureFunVal, FkDecl, FunVal, TableRef, TableRegistry, WriteOp,
};
