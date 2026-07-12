//! Decimal group (doc/cql.md Appendix B).

use rust_decimal::Decimal as RD;

use crate::trap::{CqlResult, Trap};
use crate::value::{Decimal, DECIMAL_MAX_SCALE};

/// `decimal_from_string(s)`: parse failure or unrepresentable value ⇒ `None` (Appendix B).
///
/// `precision = Some((m, n))` corresponds to the bounded `decimal(m, n)` return type
/// (the precision metavariables are known at compile time, Appendix B); `None` corresponds
/// to unbounded `decimal` (only parse failure ⇒ `None`).
pub fn decimal_from_string(s: &str, precision: Option<(u32, u32)>) -> Option<Decimal> {
    // from_str_exact: input beyond the 96-bit representable range fails to parse
    // (MVP implementation bound).
    let v = RD::from_str_exact(s).ok()?;
    match precision {
        None => Some(Decimal::unbounded(v)),
        Some((m, n)) => Decimal::bounded(m, n, v),
    }
}

/// `round_to(d, k)`: banker's rounding to k fractional digits (Appendix B).
///
/// `k < 0` ⇒ trap; bounded: `k ≥ n` ⇒ the original value (type unchanged), otherwise the
/// rounded value out of range ⇒ `Trap::DecimalPrecision`; unbounded: result scale = k.
pub fn round_to(d: &Decimal, k: i64) -> CqlResult<Decimal> {
    if k < 0 {
        return Err(Trap::Msg("round_to: negative scale"));
    }
    let k = k as u32;
    match *d {
        Decimal::Bounded { m, n, val } => {
            if k >= n {
                return Ok(*d); // k ≥ n ⇒ the original value (type unchanged)
            }
            Decimal::bounded(m, n, val.round_dp(k)).ok_or(Trap::DecimalPrecision)
        }
        Decimal::Unbounded { val } => Ok(Decimal::unbounded(val.round_dp(k.min(DECIMAL_MAX_SCALE)))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn from_string() {
        let d = decimal_from_string("123.45", Some((10, 2))).unwrap();
        assert_eq!(d.to_string(), "123.45");
        // unrepresentable ⇒ none
        assert_eq!(decimal_from_string("1234.56", Some((5, 2))), None);
        assert_eq!(decimal_from_string("abc", Some((10, 2))), None);
        assert_eq!(decimal_from_string("abc", None), None);
        // unbounded: scale taken from the value
        match decimal_from_string("1.50", None).unwrap() {
            Decimal::Unbounded { val } => assert_eq!(val.scale(), 2),
            _ => panic!(),
        }
    }

    #[test]
    fn round_to_semantics() {
        use crate::value::Decimal as D;
        let d = D::bounded(10, 4, RD::from_str("1.2345").unwrap()).unwrap();
        // Bounded: type unchanged ⇒ the value is rounded to k digits while the
        // representation keeps scale n
        assert_eq!(round_to(&d, 2).unwrap().to_string(), "1.2300");
        assert_eq!(round_to(&d, 4).unwrap(), d); // k ≥ n ⇒ the original value
        assert_eq!(round_to(&d, 10).unwrap(), d);
        assert_eq!(round_to(&d, -1), Err(Trap::Msg("round_to: negative scale")));
        // banker's rounding
        let e = D::bounded(10, 3, RD::from_str("2.345").unwrap()).unwrap();
        assert_eq!(round_to(&e, 2).unwrap().to_string(), "2.340"); // 2.345 → 2.34 (even)
        let f = D::bounded(10, 3, RD::from_str("2.355").unwrap()).unwrap();
        assert_eq!(round_to(&f, 2).unwrap().to_string(), "2.360");
        // unbounded: result scale = k
        match round_to(&D::unbounded(RD::from_str("3.14159").unwrap()), 2).unwrap() {
            D::Unbounded { val } => {
                assert_eq!(val.scale(), 2);
                assert_eq!(val.to_string(), "3.14");
            }
            _ => panic!(),
        }
    }
}
