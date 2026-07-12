//! String group (doc/cql.md Appendix B).
//!
//! Lengths and indices are always counted in **Unicode scalar values (chars)**
//! (determinism requirement, §5.1).

use crate::value::{Date, Decimal};

/// `contains(s, sub)`.
pub fn contains(s: &str, sub: &str) -> bool {
    s.contains(sub)
}

/// `starts_with(s, pre)`.
pub fn starts_with(s: &str, pre: &str) -> bool {
    s.starts_with(pre)
}

/// `ends_with(s, suf)`.
pub fn ends_with(s: &str, suf: &str) -> bool {
    s.ends_with(suf)
}

/// `length(s)`: character count (string version; the vector version is
/// `crate::stdlib::vector::vec_length`).
pub fn str_length(s: &str) -> i64 {
    s.chars().count() as i64
}

/// `concat(a, b)`.
pub fn concat(a: &str, b: &str) -> String {
    let mut s = String::with_capacity(a.len() + b.len());
    s.push_str(a);
    s.push_str(b);
    s
}

/// `to_string_int(x)`.
pub fn to_string_int(x: i64) -> String {
    x.to_string()
}

/// `to_string_float(x)`: shortest lossless representation (Rust `Display`; `1.0` prints as `"1"`).
pub fn to_string_float(x: f64) -> String {
    format!("{x}")
}

/// `to_string_date(d)`: ISO 8601 `YYYY-MM-DD`.
pub fn to_string_date(d: &Date) -> String {
    d.to_string()
}

/// `to_string_bool(b)`.
pub fn to_string_bool(b: bool) -> String {
    b.to_string()
}

/// `to_string_decimal(d)`: bounded decimals print with a fixed number of n fractional digits;
/// unbounded decimals print with the value's own scale (Appendix B).
pub fn to_string_decimal(d: &Decimal) -> String {
    d.to_string()
}

/// `substring(s, start, length)`: counted in characters; an out-of-range/negative `start` is
/// clamped to the valid range, a negative `length` ⇒ empty string (the spec does not define
/// out-of-range semantics; here we use clamping semantics, no trap).
pub fn substring(s: &str, start: i64, length: i64) -> String {
    if length <= 0 {
        return String::new();
    }
    s.chars()
        .skip(start.max(0) as usize)
        .take(length as usize)
        .collect()
}

/// `trim(s)`: remove leading and trailing whitespace.
pub fn trim(s: &str) -> String {
    s.trim().to_string()
}

/// `split(s, sep)`: split on the separator substring (an empty separator splits into the
/// character sequence, with empty strings kept at both ends, same as Rust `str::split`).
pub fn split(s: &str, sep: &str) -> Vec<String> {
    s.split(sep).map(str::to_string).collect()
}

/// `join(xs, sep)`.
pub fn join(xs: &[String], sep: &str) -> String {
    xs.join(sep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_basics() {
        assert!(contains("hello world", "lo wo"));
        assert!(starts_with("hello", "he"));
        assert!(ends_with("hello", "llo"));
        assert_eq!(str_length("héllo"), 5); // counted in characters
        assert_eq!(concat("a", "bc"), "abc");
        assert_eq!(trim("  x \n"), "x");
    }

    #[test]
    fn substring_semantics() {
        assert_eq!(substring("hello", 1, 3), "ell");
        assert_eq!(substring("hello", 0, 100), "hello");
        assert_eq!(substring("hello", -2, 3), "hel");
        assert_eq!(substring("hello", 2, -1), "");
    }

    #[test]
    fn split_join() {
        assert_eq!(split("a,b,,c", ","), vec!["a", "b", "", "c"]);
        assert_eq!(join(&["a".into(), "b".into()], "-"), "a-b");
    }

    #[test]
    fn to_string_group() {
        assert_eq!(to_string_int(-42), "-42");
        assert_eq!(to_string_float(3.5), "3.5");
        assert_eq!(to_string_bool(true), "true");
        assert_eq!(to_string_date(&Date::new(2026, 7, 11).unwrap()), "2026-07-11");
        use std::str::FromStr;
        let d = Decimal::bounded(10, 2, rust_decimal::Decimal::from_str("1.5").unwrap()).unwrap();
        assert_eq!(to_string_decimal(&d), "1.50"); // fixed n digits
        let u = Decimal::unbounded(rust_decimal::Decimal::from_str("1.50").unwrap());
        assert_eq!(to_string_decimal(&u), "1.50"); // the value's own scale
    }
}
