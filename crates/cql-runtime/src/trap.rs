//! The `Trap` type and checked `int` arithmetic helpers (doc/cql.md §5.3 error semantics).
//!
//! CQL targets total functions by default; the remaining partial operations are checked at
//! runtime and trap on failure. Traps are caught by the host runtime and mapped to error
//! codes; they are not recoverable errors (recoverable errors are expressed explicitly via
//! `option`/`enum result`).

use thiserror::Error;

/// Runtime trap (doc/cql.md §5.3).
///
/// Each variant corresponds one-to-one with a clause in the spec; `Msg` is used for
/// implementation-detail traps not listed separately in the spec (such as `k < 0` in
/// `round_to`, date overflow, etc.). The message is a static string to ensure determinism.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Trap {
    /// `int`/`decimal` division by zero or remainder by zero (§5.3).
    #[error("division by zero")]
    DivZero,
    /// `int` arithmetic overflow; no wrapping (§5.3).
    #[error("integer overflow")]
    IntOverflow,
    /// `as` conversion out of range (§2.4, §5.3).
    #[error("cast out of range")]
    CastOutOfRange,
    /// `float as int` applied to NaN (§2.4, §5.3).
    #[error("NaN to int")]
    NaNToInt,
    /// A `decimal(m, n)` operation result exceeds precision m digits or is unrepresentable
    /// (§2.1, §5.3).
    #[error("decimal precision exceeded")]
    DecimalPrecision,
    /// `the(S)` applied to a non-singleton set (§5.3, Appendix B).
    #[error("the() applied to non-singleton set")]
    TheNonSingleton,
    /// `only(S)` applied to a multi-element set (Appendix B).
    #[error("only() applied to multi-element set")]
    OnlyMulti,
    /// General recursion stack exhaustion (§3.4, §5.3).
    #[error("recursion limit exceeded")]
    RecursionLimit,
    /// Other static-message traps (implementation details; messages are fixed to ensure
    /// determinism, §5.1).
    #[error("{0}")]
    Msg(&'static str),
}

/// Result type for pure computations that may trap.
pub type CqlResult<T> = Result<T, Trap>;

/// `int` (i64) checked addition: overflow ⇒ `Trap::IntOverflow` (§5.3).
pub fn checked_add(a: i64, b: i64) -> CqlResult<i64> {
    a.checked_add(b).ok_or(Trap::IntOverflow)
}

/// `int` checked subtraction: overflow ⇒ `Trap::IntOverflow`.
pub fn checked_sub(a: i64, b: i64) -> CqlResult<i64> {
    a.checked_sub(b).ok_or(Trap::IntOverflow)
}

/// `int` checked multiplication: overflow ⇒ `Trap::IntOverflow`.
pub fn checked_mul(a: i64, b: i64) -> CqlResult<i64> {
    a.checked_mul(b).ok_or(Trap::IntOverflow)
}

/// `int` checked division: division by zero ⇒ `Trap::DivZero`; `i64::MIN / -1` overflow ⇒
/// `Trap::IntOverflow`.
pub fn checked_div(a: i64, b: i64) -> CqlResult<i64> {
    if b == 0 {
        return Err(Trap::DivZero);
    }
    a.checked_div(b).ok_or(Trap::IntOverflow)
}

/// `int` checked remainder: remainder by zero ⇒ `Trap::DivZero`; `i64::MIN % -1` overflow ⇒
/// `Trap::IntOverflow`.
pub fn checked_rem(a: i64, b: i64) -> CqlResult<i64> {
    if b == 0 {
        return Err(Trap::DivZero);
    }
    a.checked_rem(b).ok_or(Trap::IntOverflow)
}

/// `int` checked negation: `i64::MIN` ⇒ `Trap::IntOverflow`.
pub fn checked_neg(a: i64) -> CqlResult<i64> {
    a.checked_neg().ok_or(Trap::IntOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_add_sub_mul() {
        assert_eq!(checked_add(1, 2), Ok(3));
        assert_eq!(checked_sub(1, 2), Ok(-1));
        assert_eq!(checked_mul(3, 4), Ok(12));
        assert_eq!(checked_add(i64::MAX, 1), Err(Trap::IntOverflow));
        assert_eq!(checked_sub(i64::MIN, 1), Err(Trap::IntOverflow));
        assert_eq!(checked_mul(i64::MAX, 2), Err(Trap::IntOverflow));
    }

    #[test]
    fn int_div_rem() {
        assert_eq!(checked_div(7, 2), Ok(3));
        assert_eq!(checked_rem(7, 2), Ok(1));
        assert_eq!(checked_div(-7, 2), Ok(-3)); // truncated toward zero
        assert_eq!(checked_div(1, 0), Err(Trap::DivZero));
        assert_eq!(checked_rem(1, 0), Err(Trap::DivZero));
        assert_eq!(checked_div(i64::MIN, -1), Err(Trap::IntOverflow));
    }

    #[test]
    fn int_neg() {
        assert_eq!(checked_neg(5), Ok(-5));
        assert_eq!(checked_neg(i64::MIN), Err(Trap::IntOverflow));
    }
}
