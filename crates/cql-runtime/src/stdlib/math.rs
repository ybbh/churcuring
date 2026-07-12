//! Math group (doc/cql.md Appendix B).

use crate::trap::{CqlResult, Trap};

/// `abs(x)`: overflow on `i64::MIN` ⇒ `Trap::IntOverflow` (§5.3).
pub fn abs(x: i64) -> CqlResult<i64> {
    x.checked_abs().ok_or(Trap::IntOverflow)
}

/// `min(a, b)`.
pub fn min(a: i64, b: i64) -> i64 {
    a.min(b)
}

/// `max(a, b)`.
pub fn max(a: i64, b: i64) -> i64 {
    a.max(b)
}

/// `floor(x)`: round toward negative infinity.
pub fn floor(x: f64) -> f64 {
    x.floor()
}

/// `ceil(x)`: round toward positive infinity.
pub fn ceil(x: f64) -> f64 {
    x.ceil()
}

/// `round(x)`: round to nearest, ties away from zero (IEEE `roundTiesToAway`, Rust `f64::round`).
pub fn round(x: f64) -> f64 {
    x.round()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_math() {
        assert_eq!(abs(-5), Ok(5));
        assert_eq!(abs(i64::MIN), Err(Trap::IntOverflow));
        assert_eq!(min(3, 4), 3);
        assert_eq!(max(3, 4), 4);
    }

    #[test]
    fn float_math() {
        assert_eq!(floor(2.7), 2.0);
        assert_eq!(floor(-2.3), -3.0);
        assert_eq!(ceil(2.3), 3.0);
        assert_eq!(ceil(-2.7), -2.0);
        assert_eq!(round(2.5), 3.0); // ties away from zero
        assert_eq!(round(-2.5), -3.0);
        assert_eq!(round(2.4), 2.0);
    }
}
