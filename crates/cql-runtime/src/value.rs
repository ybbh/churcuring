//! Value types: `Date`, `Decimal`, the canonical order `CanonOrd`, and the type-erased
//! `Value` (doc/cql.md §2, §5.1, §6.2).
//!
//! - `Date`: calendar date, no time zone (§2.1); operations are in the Appendix B date group.
//! - `Decimal`: arbitrary-precision fixed-point decimal; the MVP is built on
//!   `rust_decimal::Decimal` (96-bit mantissa), so bounded precision is limited to m ≤ 28
//!   (an implementation bound — the spec allows a backend to set an upper bound, §2.1).
//! - `CanonOrd`: the canonical order, used only for deterministic sorting when materializing
//!   results (§2.3, §5.1); it is not exposed as a language operator.
//! - `Value`: type-erased runtime value (the `key_val`/`row_val` of `write_op`, §3.6, §6.2).

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};

use chrono::{Datelike, Duration, NaiveDate, Weekday};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal as RD;

use crate::collections::{CqlBag, CqlMap, CqlSet};
use crate::trap::{CqlResult, Trap};

// ---------------------------------------------------------------------------
// Date (§2.1)
// ---------------------------------------------------------------------------

/// Calendar date (no time zone), wrapping `chrono::NaiveDate`. Supports comparison and
/// hashing (§2.1, §2.3).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Date(NaiveDate);

impl Date {
    /// Construct and validate; an invalid date (such as February 30) returns `None`.
    pub fn new(year: i32, month: u32, day: u32) -> Option<Date> {
        NaiveDate::from_ymd_opt(year, month, day).map(Date)
    }

    /// Year (Appendix B `year`).
    pub fn year(&self) -> i64 {
        self.0.year() as i64
    }

    /// Month, 1–12 (Appendix B `month`).
    pub fn month(&self) -> i64 {
        self.0.month() as i64
    }

    /// Day of month, 1–31 (Appendix B `day`).
    pub fn day(&self) -> i64 {
        self.0.day() as i64
    }

    /// Add n days (Appendix B `add_days`); returns `None` outside the representable range
    /// (the spec does not define a date-overflow trap; here out-of-range is expressed as
    /// `None`).
    pub fn add_days(&self, n: i64) -> Option<Date> {
        self.0.checked_add_signed(Duration::days(n)).map(Date)
    }

    /// The number of days in `self - other` (Appendix B `days_between`).
    pub fn days_between(&self, other: &Date) -> i64 {
        self.0.signed_duration_since(other.0).num_days()
    }

    /// Parse ISO 8601 `YYYY-MM-DD` (Appendix B `parse_date`); failure returns `None`.
    pub fn parse(s: &str) -> Option<Date> {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(Date)
    }

    /// Day of week, 0 = Monday … 6 = Sunday (Appendix B `day_of_week`).
    pub fn day_of_week(&self) -> i64 {
        match self.0.weekday() {
            Weekday::Mon => 0,
            Weekday::Tue => 1,
            Weekday::Wed => 2,
            Weekday::Thu => 3,
            Weekday::Fri => 4,
            Weekday::Sat => 5,
            Weekday::Sun => 6,
        }
    }
}

impl fmt::Debug for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Date({})", self.0)
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Decimal (§2.1)
// ---------------------------------------------------------------------------

/// MVP implementation bound: a 96-bit mantissa (< 2^96 ≈ 7.9e28) ⇒ ≤ 28 significant digits
/// guarantees the mantissa does not overflow.
pub const DECIMAL_MAX_M: u32 = 28;
/// rust_decimal scale limit.
pub const DECIMAL_MAX_SCALE: u32 = 28;

/// Number of decimal digits (0 counts as 1 digit).
fn digit_count(mut v: u128) -> u32 {
    let mut n = 1;
    while v >= 10 {
        v /= 10;
        n += 1;
    }
    n
}

/// Power of 10 (n ≤ 28, guaranteed by the caller).
fn pow10(n: u32) -> i128 {
    debug_assert!(n <= DECIMAL_MAX_SCALE);
    let mut r: i128 = 1;
    for _ in 0..n {
        r *= 10;
    }
    r
}

/// Fixed-point decimal (§2.1).
///
/// - `Bounded { m, n, val }`: `decimal(m, n)`, with the invariant `val.scale() == n` (fixed
///   scale ⇒ a unique representation for a given (m, n), §2.1). `+`/`-`/`*` are exact: a
///   result with more than m significant digits ⇒ `Trap::DecimalPrecision`; additionally, if
///   the exact product of `*` cannot be represented at scale n (more than n fractional
///   digits) it also ⇒ trap (the spec only says "result exceeds m digits ⇒ trap"; the
///   unrepresentable case is handled by the same trap — an MVP interpretation that preserves
///   the "fixed scale ⇒ unique representation" invariant). `/` uses banker's rounding to
///   scale n; an integer part exceeding m-n digits ⇒ trap.
/// - `Unbounded { val }`: unbounded `decimal`; the scale is an attribute of the value
///   (`val.scale()`, §6.2). `+`/`-`/`*` are exact; `/` yields scale = max(left, right) + 6
///   with banker's rounding. Equality is numeric: comparison and hashing use the
///   trailing-zero-stripped canonical form (§2.1).
///
/// Note: the MVP is backed by a 96-bit mantissa; when an unbounded operation exceeds 96 bits,
/// the implementation bound is reported as `DecimalPrecision` (the language semantics say no
/// trap, §5.3).
#[derive(Debug, Clone, Copy)]
pub enum Decimal {
    /// A `decimal(m, n)` value; `val.scale() == n` always holds.
    Bounded { m: u32, n: u32, val: RD },
    /// An unbounded `decimal` value; the scale is `val.scale()`.
    Unbounded { val: RD },
}

impl Decimal {
    /// Construct a bounded decimal and validate representability (§2.1: m ≥ 1, 0 ≤ n ≤ m,
    /// significant digits ≤ m, fractional digits ≤ n, no implicit rounding). Invalid
    /// arguments or an unrepresentable value ⇒ `None`.
    pub fn bounded(m: u32, n: u32, val: RD) -> Option<Decimal> {
        if m < 1 || n > m || m > DECIMAL_MAX_M || n > DECIMAL_MAX_SCALE {
            return None;
        }
        let norm = val.normalize();
        let s = norm.scale();
        if s > n {
            return None; // more than n fractional digits: not exactly representable
        }
        // Compute the mantissa at scale n as i128, check significant digits first, then
        // construct (avoids overflowing the 96-bit mantissa).
        let mant = norm.mantissa() * pow10(n - s);
        if digit_count(mant.unsigned_abs()) > m {
            return None;
        }
        Some(Decimal::Bounded { m, n, val: RD::from_i128_with_scale(mant, n) })
    }

    /// Construct an unbounded decimal; the scale is taken from the value itself (§2.1).
    pub fn unbounded(val: RD) -> Decimal {
        Decimal::Unbounded { val }
    }

    /// The precision (m, n) of a bounded decimal.
    pub fn precision(&self) -> Option<(u32, u32)> {
        match *self {
            Decimal::Bounded { m, n, .. } => Some((m, n)),
            Decimal::Unbounded { .. } => None,
        }
    }

    /// Number of digits of the integer part (for division/cast out-of-range checks; an
    /// integer part of 0 counts as 0 digits).
    fn int_digits(&self) -> u32 {
        let v = match *self {
            Decimal::Bounded { val, .. } => val,
            Decimal::Unbounded { val } => val,
        };
        let t = v.trunc().mantissa().unsigned_abs();
        if t == 0 {
            0
        } else {
            digit_count(t)
        }
    }

    /// `decimal(m, n)` addition: exact; a result with more than m significant digits ⇒
    /// `Trap::DecimalPrecision` (§2.1).
    pub fn add(&self, other: &Decimal) -> CqlResult<Decimal> {
        self.add_sub(other, true)
    }

    /// `decimal(m, n)` subtraction: exact; out of range ⇒ trap.
    pub fn sub(&self, other: &Decimal) -> CqlResult<Decimal> {
        self.add_sub(other, false)
    }

    fn add_sub(&self, other: &Decimal, is_add: bool) -> CqlResult<Decimal> {
        match (*self, *other) {
            (Decimal::Bounded { m, n, val: a }, Decimal::Bounded { m: m2, n: n2, val: b }) => {
                if (m, n) != (m2, n2) {
                    return Err(Trap::Msg("decimal: precision mismatch"));
                }
                let r = if is_add { a.checked_add(b) } else { a.checked_sub(b) }
                    .ok_or(Trap::DecimalPrecision)?;
                Decimal::bounded(m, n, r).ok_or(Trap::DecimalPrecision)
            }
            (Decimal::Unbounded { val: a }, Decimal::Unbounded { val: b }) => {
                let r = if is_add { a.checked_add(b) } else { a.checked_sub(b) }
                    .ok_or(Trap::DecimalPrecision)?;
                Ok(Decimal::Unbounded { val: r })
            }
            _ => Err(Trap::Msg("decimal: bounded/unbounded mismatch")),
        }
    }

    /// `decimal(m, n)` multiplication: exact (unrepresentable or over-precision ⇒
    /// `Trap::DecimalPrecision`, see the type documentation).
    pub fn mul(&self, other: &Decimal) -> CqlResult<Decimal> {
        match (*self, *other) {
            (Decimal::Bounded { m, n, val: a }, Decimal::Bounded { m: m2, n: n2, val: b }) => {
                if (m, n) != (m2, n2) {
                    return Err(Trap::Msg("decimal: precision mismatch"));
                }
                let p = a.checked_mul(b).ok_or(Trap::DecimalPrecision)?;
                Decimal::bounded(m, n, p).ok_or(Trap::DecimalPrecision)
            }
            (Decimal::Unbounded { val: a }, Decimal::Unbounded { val: b }) => {
                let p = a.checked_mul(b).ok_or(Trap::DecimalPrecision)?;
                Ok(Decimal::Unbounded { val: p })
            }
            _ => Err(Trap::Msg("decimal: bounded/unbounded mismatch")),
        }
    }

    /// `decimal(m, n)` division.
    ///
    /// Bounded: banker's rounding to scale n; an integer part exceeding m-n digits ⇒
    /// `Trap::DecimalPrecision`; unbounded: result scale = max(left, right) + 6, banker's
    /// rounding (§2.1). Division by zero ⇒ `Trap::DivZero`.
    pub fn div(&self, other: &Decimal) -> CqlResult<Decimal> {
        match (*self, *other) {
            (Decimal::Bounded { m, n, val: a }, Decimal::Bounded { m: m2, n: n2, val: b }) => {
                if (m, n) != (m2, n2) {
                    return Err(Trap::Msg("decimal: precision mismatch"));
                }
                if b.is_zero() {
                    return Err(Trap::DivZero);
                }
                let q = a.checked_div(b).ok_or(Trap::DecimalPrecision)?;
                // round_dp = banker's rounding (MidpointNearestEven)
                let d = Decimal::bounded(m, n, q.round_dp(n)).ok_or(Trap::DecimalPrecision)?;
                if d.int_digits() > m - n {
                    return Err(Trap::DecimalPrecision);
                }
                Ok(d)
            }
            (Decimal::Unbounded { val: a }, Decimal::Unbounded { val: b }) => {
                if b.is_zero() {
                    return Err(Trap::DivZero);
                }
                let q = a.checked_div(b).ok_or(Trap::DecimalPrecision)?;
                let scale = (a.scale().max(b.scale()) + 6).min(DECIMAL_MAX_SCALE);
                Ok(Decimal::Unbounded { val: q.round_dp(scale) })
            }
            _ => Err(Trap::Msg("decimal: bounded/unbounded mismatch")),
        }
    }

    /// Unary negation.
    pub fn neg(&self) -> Decimal {
        match *self {
            Decimal::Bounded { m, n, val } => Decimal::Bounded { m, n, val: -val },
            Decimal::Unbounded { val } => Decimal::Unbounded { val: -val },
        }
    }

    // -- `as` conversion whitelist (§2.4)---------------------------------------

    /// `decimal as int`: truncation toward zero; out of range ⇒ `Trap::CastOutOfRange`
    /// (§2.4).
    pub fn as_int(&self) -> CqlResult<i64> {
        let v = match *self {
            Decimal::Bounded { val, .. } => val,
            Decimal::Unbounded { val } => val,
        };
        v.trunc().to_i64().ok_or(Trap::CastOutOfRange)
    }

    /// `decimal as float`: round to nearest; may lose precision; never fails (§2.4).
    pub fn as_float(&self) -> f64 {
        let v = match *self {
            Decimal::Bounded { val, .. } => val,
            Decimal::Unbounded { val } => val,
        };
        v.to_f64().unwrap_or(if v.is_sign_negative() {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        })
    }

    /// `decimal(m, n) as decimal`: drop the bound; exact; never fails (§2.4).
    pub fn as_unbounded(&self) -> Decimal {
        match *self {
            Decimal::Bounded { val, .. } => Decimal::Unbounded { val: val.normalize() },
            u @ Decimal::Unbounded { .. } => u,
        }
    }

    /// `decimal as decimal(m, n)` and `decimal(m1, n1) as decimal(m2, n2)`: exact when the
    /// target scale ≥ source scale, otherwise banker's rounding; an integer part exceeding
    /// m-n digits ⇒ `Trap::CastOutOfRange` (§2.4).
    pub fn as_bounded(&self, m: u32, n: u32) -> CqlResult<Decimal> {
        if m < 1 || n > m || m > DECIMAL_MAX_M || n > DECIMAL_MAX_SCALE {
            return Err(Trap::Msg("decimal: invalid precision"));
        }
        let (val, src_scale) = match *self {
            Decimal::Bounded { val, n, .. } => (val, n),
            Decimal::Unbounded { val } => (val, val.scale()),
        };
        let rounded = if n >= src_scale { val } else { val.round_dp(n) };
        let d = Decimal::bounded(m, n, rounded).ok_or(Trap::CastOutOfRange)?;
        if d.int_digits() > m - n {
            return Err(Trap::CastOutOfRange);
        }
        Ok(d)
    }
}

impl PartialEq for Decimal {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Decimal::Bounded { m, n, val },
                Decimal::Bounded { m: m2, n: n2, val: v2 },
            ) => (m, n, val) == (m2, n2, v2),
            (Decimal::Unbounded { val }, Decimal::Unbounded { val: v2 }) => {
                // Numeric equality: compare the trailing-zero-stripped canonical forms (§2.1).
                let (a, b) = (val.normalize(), v2.normalize());
                a.scale() == b.scale() && a.mantissa() == b.mantissa()
            }
            _ => false,
        }
    }
}
impl Eq for Decimal {}

impl Hash for Decimal {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Decimal::Bounded { m, n, val } => {
                0u8.hash(state);
                m.hash(state);
                n.hash(state);
                val.mantissa().hash(state);
            }
            Decimal::Unbounded { val } => {
                1u8.hash(state);
                let v = val.normalize();
                v.scale().hash(state);
                v.mantissa().hash(state);
            }
        }
    }
}

impl fmt::Display for Decimal {
    /// Bounded: print with a fixed n fractional digits; unbounded: print with the value's own
    /// scale (Appendix B `to_string_decimal`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Decimal::Bounded { val, .. } => write!(f, "{val}"),
            Decimal::Unbounded { val } => write!(f, "{val}"),
        }
    }
}

// -- `as` conversions: the int/float side (§2.4)--------------------------------

/// `int as decimal(m, n)`: exact; an integer part exceeding m-n digits ⇒
/// `Trap::CastOutOfRange` (§2.4).
pub fn int_as_decimal(m: u32, n: u32, x: i64) -> CqlResult<Decimal> {
    if m < 1 || n > m || m > DECIMAL_MAX_M || n > DECIMAL_MAX_SCALE {
        return Err(Trap::Msg("decimal: invalid precision"));
    }
    if x != 0 && digit_count(x.unsigned_abs() as u128) > m - n {
        return Err(Trap::CastOutOfRange);
    }
    let mant = (x as i128) * pow10(n);
    Ok(Decimal::Bounded { m, n, val: RD::from_i128_with_scale(mant, n) })
}

/// `int as float`: widening; may lose precision; never fails (§2.4).
pub fn int_as_float(x: i64) -> f64 {
    x as f64
}

/// `float as int`: truncation toward zero; NaN ⇒ `Trap::NaNToInt`; out of range ⇒
/// `Trap::CastOutOfRange` (§2.4, §5.3).
pub fn float_as_int(x: f64) -> CqlResult<i64> {
    if x.is_nan() {
        return Err(Trap::NaNToInt);
    }
    let t = x.trunc();
    // The representable range of i64 is [-2^63, 2^63); the f64 representation of 2^63 is
    // exactly 9223372036854775808.0.
    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&t) {
        return Err(Trap::CastOutOfRange);
    }
    Ok(t as i64)
}

// ---------------------------------------------------------------------------
// CanonOrd: the canonical order (§2.3, §5.1)
// ---------------------------------------------------------------------------

/// The canonical order: a total order defined for all first-order types, used only for
/// materializing `set`/`bag`/`map` and for determinism (§2.3, §5.1); it is not exposed as a
/// language-level operator. Use `sort_by` for user-semantic ordering.
///
/// Implementors must guarantee `canon_cmp(a, b) == Equal` if and only if `a == b`
/// (consistent with `Eq`/`Hash`).
pub trait CanonOrd {
    /// Compare two values in the canonical order.
    fn canon_cmp(&self, other: &Self) -> Ordering;
}

/// Element-wise canonical order of sequences (a proper prefix is smaller).
pub(crate) fn canon_cmp_slice<T: CanonOrd>(a: &[T], b: &[T]) -> Ordering {
    let (mut i, mut j) = (0, 0);
    loop {
        match (a.get(i), b.get(j)) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => match x.canon_cmp(y) {
                Ordering::Equal => {
                    i += 1;
                    j += 1;
                }
                o => return o,
            },
        }
    }
}

impl CanonOrd for bool {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        self.cmp(other)
    }
}

impl CanonOrd for i64 {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        self.cmp(other)
    }
}

impl CanonOrd for String {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        self.cmp(other)
    }
}

impl CanonOrd for str {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        self.cmp(other)
    }
}

/// The canonical order of `float` is the IEEE 754 totalOrder (§2.3):
/// `-NaN < -inf < ... < -0 < +0 < ... < +inf < +NaN`.
impl CanonOrd for f64 {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        self.total_cmp(other)
    }
}

impl CanonOrd for Date {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        self.cmp(other)
    }
}

/// Canonical order of `decimal`: bounded < unbounded; within the same kind, numeric order
/// (bounded values are first discriminated by (m, n)).
impl CanonOrd for Decimal {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (
                Decimal::Bounded { m, n, val },
                Decimal::Bounded { m: m2, n: n2, val: v2 },
            ) => (m, n).cmp(&(m2, n2)).then_with(|| val.cmp(v2)),
            (Decimal::Bounded { .. }, Decimal::Unbounded { .. }) => Ordering::Less,
            (Decimal::Unbounded { .. }, Decimal::Bounded { .. }) => Ordering::Greater,
            (Decimal::Unbounded { val }, Decimal::Unbounded { val: v2 }) => val.cmp(v2),
        }
    }
}

impl<T: CanonOrd + ?Sized> CanonOrd for &T {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        (**self).canon_cmp(*other)
    }
}

impl<T: CanonOrd> CanonOrd for Option<T> {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(a), Some(b)) => a.canon_cmp(b),
        }
    }
}

impl<T: CanonOrd> CanonOrd for [T] {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        canon_cmp_slice(self, other)
    }
}

impl<T: CanonOrd> CanonOrd for Vec<T> {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        canon_cmp_slice(self, other)
    }
}

/// Canonical order of tuples: component-wise (§2.3).
impl<A: CanonOrd, B: CanonOrd> CanonOrd for (A, B) {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        self.0
            .canon_cmp(&other.0)
            .then_with(|| self.1.canon_cmp(&other.1))
    }
}

impl<A: CanonOrd, B: CanonOrd, C: CanonOrd> CanonOrd for (A, B, C) {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        self.0
            .canon_cmp(&other.0)
            .then_with(|| self.1.canon_cmp(&other.1))
            .then_with(|| self.2.canon_cmp(&other.2))
    }
}

impl<A: CanonOrd, B: CanonOrd, C: CanonOrd, D: CanonOrd> CanonOrd for (A, B, C, D) {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        self.0
            .canon_cmp(&other.0)
            .then_with(|| self.1.canon_cmp(&other.1))
            .then_with(|| self.2.canon_cmp(&other.2))
            .then_with(|| self.3.canon_cmp(&other.3))
    }
}

// ---------------------------------------------------------------------------
// Value: type-erased runtime values (§3.6, §6.2)
// ---------------------------------------------------------------------------

/// Type-erased runtime value, used for the `key_val`/`row_val` of `write_op`, fixture tables,
/// and the dynamic boundary of compiled code (§3.6, §6.2).
///
/// Equality/hashing follows the runtime canonical-encoding view (§3.6): `float` participates
/// in equality and hashing by bit pattern (`to_bits`), so `-0.0 /= 0.0` and NaNs with the
/// same bit pattern are equal — this does not conflict with §2.3, where `float` is not
/// comparable/hashable at the language level (`Value` is a runtime descriptor). Hashing is
/// implemented via `canonical_bytes`, consistent with `Eq`.
#[derive(Debug, Clone)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Decimal(Decimal),
    Date(Date),
    Str(String),
    Option(Option<Box<Value>>),
    Vector(Vec<Value>),
    Set(CqlSet<Value>),
    Bag(CqlBag<Value>),
    Map(CqlMap<Value, Value>),
    Tuple(Vec<Value>),
    /// Record: field names are kept in lexicographic order (`BTreeMap`), guaranteeing a
    /// unique canonical form.
    Record(BTreeMap<String, Value>),
    /// Enum variant: multiple payloads are allowed (§3.2).
    Enum { variant: String, payload: Vec<Value> },
}

impl Value {
    /// Canonical encoding: each value maps to a unique byte string, for deterministic hashing
    /// and materialization (§5.1, §6.2).
    ///
    /// The encoding guarantees uniqueness (no collisions) and determinism; the byte order
    /// itself is not promised to equal the `canon_cmp` order (the order is defined by
    /// `CanonOrd`). Fixed-width fields are big-endian; variable-length data carries an
    /// 8-byte length prefix.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }

    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Value::Bool(b) => {
                out.push(0x00);
                out.push(*b as u8);
            }
            Value::Int(i) => {
                out.push(0x01);
                out.extend_from_slice(&((*i as u64) ^ 0x8000_0000_0000_0000).to_be_bytes());
            }
            Value::Float(f) => {
                out.push(0x02);
                // Encode the bit pattern directly (the hashing view, §3.6).
                out.extend_from_slice(&f.to_bits().to_be_bytes());
            }
            Value::Decimal(d) => {
                out.push(0x03);
                match d {
                    Decimal::Bounded { m, n, val } => {
                        out.push(0);
                        out.extend_from_slice(&m.to_be_bytes());
                        out.extend_from_slice(&n.to_be_bytes());
                        out.extend_from_slice(&val.mantissa().to_be_bytes());
                    }
                    Decimal::Unbounded { val } => {
                        out.push(1);
                        let v = val.normalize();
                        out.extend_from_slice(&v.scale().to_be_bytes());
                        out.extend_from_slice(&v.mantissa().to_be_bytes());
                    }
                }
            }
            Value::Date(d) => {
                out.push(0x04);
                let days = d.0.num_days_from_ce();
                out.extend_from_slice(&((days as u32) ^ 0x8000_0000).to_be_bytes());
            }
            Value::Str(s) => {
                out.push(0x05);
                encode_bytes(out, s.as_bytes());
            }
            Value::Option(o) => {
                out.push(0x06);
                match o {
                    None => out.push(0),
                    Some(v) => {
                        out.push(1);
                        v.encode(out);
                    }
                }
            }
            Value::Vector(xs) => {
                out.push(0x07);
                encode_values(out, xs);
            }
            Value::Set(s) => {
                out.push(0x08);
                encode_values(out, s.as_slice());
            }
            Value::Bag(b) => {
                out.push(0x09);
                out.extend_from_slice(&(b.entry_count() as u64).to_be_bytes());
                for (elem, count) in b.entries() {
                    elem.encode(out);
                    out.extend_from_slice(&count.to_be_bytes());
                }
            }
            Value::Map(m) => {
                out.push(0x0A);
                out.extend_from_slice(&(m.len() as u64).to_be_bytes());
                for (k, v) in m.iter() {
                    k.encode(out);
                    v.encode(out);
                }
            }
            Value::Tuple(xs) => {
                out.push(0x0B);
                encode_values(out, xs);
            }
            Value::Record(fields) => {
                out.push(0x0C);
                out.extend_from_slice(&(fields.len() as u64).to_be_bytes());
                for (name, v) in fields {
                    encode_bytes(out, name.as_bytes());
                    v.encode(out);
                }
            }
            Value::Enum { variant, payload } => {
                out.push(0x0D);
                encode_bytes(out, variant.as_bytes());
                encode_values(out, payload);
            }
        }
    }
}

fn encode_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn encode_values(out: &mut Vec<u8>, xs: &[Value]) {
    out.extend_from_slice(&(xs.len() as u64).to_be_bytes());
    for x in xs {
        x.encode(out);
    }
}

/// Type rank: defines the cross-variant canonical order of `Value` (used only for
/// deterministic materialization, §5.1).
fn value_rank(v: &Value) -> u8 {
    match v {
        Value::Bool(_) => 0,
        Value::Int(_) => 1,
        Value::Float(_) => 2,
        Value::Decimal(_) => 3,
        Value::Date(_) => 4,
        Value::Str(_) => 5,
        Value::Option(_) => 6,
        Value::Vector(_) => 7,
        Value::Set(_) => 8,
        Value::Bag(_) => 9,
        Value::Map(_) => 10,
        Value::Tuple(_) => 11,
        Value::Record(_) => 12,
        Value::Enum { .. } => 13,
    }
}

impl CanonOrd for Value {
    fn canon_cmp(&self, other: &Self) -> Ordering {
        value_rank(self).cmp(&value_rank(other)).then_with(|| match (self, other) {
            (Value::Bool(a), Value::Bool(b)) => a.canon_cmp(b),
            (Value::Int(a), Value::Int(b)) => a.canon_cmp(b),
            (Value::Float(a), Value::Float(b)) => a.canon_cmp(b),
            (Value::Decimal(a), Value::Decimal(b)) => a.canon_cmp(b),
            (Value::Date(a), Value::Date(b)) => a.canon_cmp(b),
            (Value::Str(a), Value::Str(b)) => a.canon_cmp(b),
            (Value::Option(a), Value::Option(b)) => {
                a.as_deref().canon_cmp(&b.as_deref())
            }
            (Value::Vector(a), Value::Vector(b)) => a.canon_cmp(b),
            (Value::Set(a), Value::Set(b)) => a.canon_cmp(b),
            (Value::Bag(a), Value::Bag(b)) => a.canon_cmp(b),
            (Value::Map(a), Value::Map(b)) => a.canon_cmp(b),
            (Value::Tuple(a), Value::Tuple(b)) => a.canon_cmp(b),
            (Value::Record(a), Value::Record(b)) => {
                let (mut ia, mut ib) = (a.iter(), b.iter());
                loop {
                    match (ia.next(), ib.next()) {
                        (None, None) => return Ordering::Equal,
                        (None, Some(_)) => return Ordering::Less,
                        (Some(_), None) => return Ordering::Greater,
                        (Some((ka, va)), Some((kb, vb))) => {
                            match ka.cmp(kb).then_with(|| va.canon_cmp(vb)) {
                                Ordering::Equal => continue,
                                o => return o,
                            }
                        }
                    }
                }
            }
            (
                Value::Enum { variant: va, payload: pa },
                Value::Enum { variant: vb, payload: pb },
            ) => va.cmp(vb).then_with(|| pa.canon_cmp(pb)),
            _ => unreachable!("equal rank implies the same variant"),
        })
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        // Structural equality, consistent with the uniqueness of canonical_bytes (float by
        // bit pattern, §3.6).
        match (self, other) {
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Value::Decimal(a), Value::Decimal(b)) => a == b,
            (Value::Date(a), Value::Date(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Option(a), Value::Option(b)) => a == b,
            (Value::Vector(a), Value::Vector(b)) => a == b,
            (Value::Set(a), Value::Set(b)) => a == b,
            (Value::Bag(a), Value::Bag(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => a == b,
            (Value::Tuple(a), Value::Tuple(b)) => a == b,
            (Value::Record(a), Value::Record(b)) => a == b,
            (
                Value::Enum { variant: va, payload: pa },
                Value::Enum { variant: vb, payload: pb },
            ) => va == vb && pa == pb,
            _ => false,
        }
    }
}
impl Eq for Value {}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write(&self.canonical_bytes());
    }
}

/// `Ord` delegates to the canonical order, for containers such as `BTreeMap<Value, _>` that
/// require `Ord` (consistent with `Eq`: floats are Equal only when their bit patterns match).
impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        self.canon_cmp(other)
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn rd(s: &str) -> RD {
        RD::from_str(s).unwrap()
    }

    fn bounded(m: u32, n: u32, s: &str) -> Decimal {
        Decimal::bounded(m, n, rd(s)).unwrap()
    }

    // -- Date ---------------------------------------------------------------

    #[test]
    fn date_validity_and_fields() {
        assert!(Date::new(2026, 2, 29).is_none());
        assert!(Date::new(2024, 2, 29).is_some());
        let d = Date::new(2026, 7, 11).unwrap();
        assert_eq!((d.year(), d.month(), d.day()), (2026, 7, 11));
    }

    #[test]
    fn date_arith() {
        let a = Date::new(2026, 7, 11).unwrap();
        let b = a.add_days(20).unwrap();
        assert_eq!(b, Date::new(2026, 7, 31).unwrap());
        assert_eq!(b.days_between(&a), 20);
        assert_eq!(a.days_between(&b), -20);
        // across a month boundary
        assert_eq!(a.add_days(365).unwrap(), Date::new(2027, 7, 11).unwrap());
    }

    #[test]
    fn date_parse_and_weekday() {
        let d = Date::parse("2026-07-11").unwrap();
        assert_eq!(d, Date::new(2026, 7, 11).unwrap());
        assert!(Date::parse("2026-13-01").is_none());
        assert!(Date::parse("not a date").is_none());
        // 2026-07-11 is a Saturday ⇒ 5; 2026-07-06 is a Monday ⇒ 0
        assert_eq!(d.day_of_week(), 5);
        assert_eq!(Date::new(2026, 7, 6).unwrap().day_of_week(), 0);
    }

    // -- Decimal construction --------------------------------------------------

    #[test]
    fn decimal_bounded_validation() {
        assert!(Decimal::bounded(0, 0, rd("1")).is_none()); // m ≥ 1
        assert!(Decimal::bounded(2, 3, rd("1")).is_none()); // n ≤ m
        assert!(Decimal::bounded(5, 2, rd("123.456")).is_none()); // fractional digits ≤ n
        assert!(Decimal::bounded(5, 2, rd("1234.56")).is_none()); // significant digits ≤ m
        assert!(Decimal::bounded(5, 2, rd("123.45")).is_some());
        assert!(Decimal::bounded(5, 2, rd("-0.01")).is_some());
        assert!(Decimal::bounded(29, 2, rd("1")).is_none()); // MVP bound m ≤ 28
    }

    #[test]
    fn decimal_unbounded_eq_hash_normalized() {
        let a = Decimal::unbounded(rd("1.50"));
        let b = Decimal::unbounded(rd("1.5"));
        assert_eq!(a, b);
        let mut h1 = std::collections::hash_map::DefaultHasher::new();
        let mut h2 = std::collections::hash_map::DefaultHasher::new();
        a.hash(&mut h1);
        b.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
        assert_ne!(a, Decimal::unbounded(rd("1.51")));
    }

    #[test]
    fn decimal_bounded_eq_requires_same_precision() {
        assert_ne!(bounded(5, 2, "1.50"), bounded(6, 2, "1.50"));
        assert_eq!(bounded(5, 2, "1.50"), bounded(5, 2, "1.5"));
    }

    // -- Decimal operations ---------------------------------------------------

    #[test]
    fn decimal_bounded_add_precision_trap() {
        let a = bounded(5, 2, "999.99");
        let b = bounded(5, 2, "0.01");
        assert_eq!(a.add(&b), Err(Trap::DecimalPrecision)); // 1000.00 ⇒ 6 digits
        assert_eq!(
            bounded(6, 2, "999.99").add(&bounded(6, 2, "0.01")).unwrap().to_string(),
            "1000.00"
        );
    }

    #[test]
    fn decimal_bounded_mul_exact_and_trap() {
        // 12.34 * 3.00 = 37.02 exactly
        let a = bounded(10, 2, "12.34");
        let b = bounded(10, 2, "3.00");
        assert_eq!(a.mul(&b).unwrap().to_string(), "37.02");
        // 1.23 * 1.23 = 1.5129: scale 4 > n=2, the exact product is unrepresentable ⇒ trap
        let c = bounded(10, 2, "1.23");
        assert_eq!(c.mul(&c), Err(Trap::DecimalPrecision));
        // more than m significant digits
        let d = bounded(5, 2, "99.99");
        assert_eq!(d.mul(&d), Err(Trap::DecimalPrecision));
    }

    #[test]
    fn decimal_bounded_div_bankers_rounding() {
        let a = bounded(10, 2, "1.00");
        let three = bounded(10, 2, "3.00");
        assert_eq!(a.div(&three).unwrap().to_string(), "0.33");
        // banker's rounding: 2.5 → 2, 3.5 → 4 (scale 0)
        let two = bounded(10, 0, "2");
        assert_eq!(bounded(10, 0, "5").div(&two).unwrap().to_string(), "2");
        assert_eq!(bounded(10, 0, "7").div(&two).unwrap().to_string(), "4");
        // 0.125 → 0.12, 0.135 → 0.14 (scale 2)
        assert_eq!(bounded(10, 3, "0.125").as_bounded(10, 2).unwrap().to_string(), "0.12");
        assert_eq!(bounded(10, 3, "0.135").as_bounded(10, 2).unwrap().to_string(), "0.14");
    }

    #[test]
    fn decimal_div_zero_and_int_part_trap() {
        let a = bounded(5, 2, "1.00");
        assert_eq!(a.div(&bounded(5, 2, "0.00")), Err(Trap::DivZero));
        // (5,2): the integer part has at most 3 digits; 999.99 / 0.01 = 9999900 ⇒ trap
        let big = bounded(5, 2, "999.99");
        assert_eq!(big.div(&bounded(5, 2, "0.01")), Err(Trap::DecimalPrecision));
    }

    #[test]
    fn decimal_unbounded_ops() {
        let a = Decimal::unbounded(rd("1.5"));
        let b = Decimal::unbounded(rd("2.25"));
        assert_eq!(a.add(&b).unwrap().to_string(), "3.75");
        assert_eq!(a.mul(&b).unwrap().to_string(), "3.375");
        // unbounded division: scale = max(1, 2) + 6 = 8
        let q = a.div(&b).unwrap();
        match q {
            Decimal::Unbounded { val } => assert_eq!(val.scale(), 8),
            _ => panic!(),
        }
        assert_eq!(a.div(&Decimal::unbounded(rd("0"))), Err(Trap::DivZero));
    }

    // -- `as` conversions (§2.4)-----------------------------------------------

    #[test]
    fn cast_int_decimal() {
        let d = int_as_decimal(5, 2, 123).unwrap();
        assert_eq!(d.to_string(), "123.00");
        assert_eq!(int_as_decimal(5, 2, 1234), Err(Trap::CastOutOfRange)); // integer part exceeds 3 digits
        assert_eq!(int_as_decimal(5, 5, 0).unwrap().to_string(), "0.00000");
    }

    #[test]
    fn cast_decimal_int_float() {
        assert_eq!(bounded(10, 2, "-12.99").as_int(), Ok(-12)); // truncated toward zero
        assert_eq!(bounded(10, 2, "12.99").as_int(), Ok(12));
        assert_eq!(
            Decimal::unbounded(rd("99999999999999999999")).as_int(),
            Err(Trap::CastOutOfRange)
        );
        assert!((bounded(10, 2, "0.1").as_float() - 0.1).abs() < 1e-12);
    }

    #[test]
    fn cast_float_int() {
        assert_eq!(float_as_int(3.7), Ok(3));
        assert_eq!(float_as_int(-3.7), Ok(-3));
        assert_eq!(float_as_int(f64::NAN), Err(Trap::NaNToInt));
        assert_eq!(float_as_int(1e20), Err(Trap::CastOutOfRange));
        assert_eq!(float_as_int(-1e20), Err(Trap::CastOutOfRange));
        assert_eq!(int_as_float(42), 42.0);
    }

    #[test]
    fn cast_decimal_decimal() {
        // bounded → unbounded: exact (trailing-zero normalization)
        match bounded(10, 2, "1.50").as_unbounded() {
            Decimal::Unbounded { val } => {
                assert_eq!(val.scale(), 1);
                assert_eq!(val.to_string(), "1.5");
            }
            _ => panic!(),
        }
        // unbounded → bounded: scale ≤ n is exact
        assert_eq!(
            Decimal::unbounded(rd("1.5")).as_bounded(10, 2).unwrap().to_string(),
            "1.50"
        );
        // unbounded → bounded: scale > n uses banker's rounding
        assert_eq!(
            Decimal::unbounded(rd("1.255")).as_bounded(10, 2).unwrap().to_string(),
            "1.26"
        );
        assert_eq!(
            Decimal::unbounded(rd("1.245")).as_bounded(10, 2).unwrap().to_string(),
            "1.24"
        );
        // unbounded → bounded: integer part out of range
        assert_eq!(
            Decimal::unbounded(rd("12345.6")).as_bounded(5, 2),
            Err(Trap::CastOutOfRange)
        );
        // bounded → bounded: n2 ≥ n1 is exact
        assert_eq!(bounded(5, 2, "1.50").as_bounded(8, 4).unwrap().to_string(), "1.5000");
        // bounded → bounded: n2 < n1 uses banker's rounding
        assert_eq!(bounded(8, 4, "1.2345").as_bounded(5, 2).unwrap().to_string(), "1.23");
    }

    // -- CanonOrd ------------------------------------------------------------

    #[test]
    fn canon_ord_float_total_order() {
        assert!(CanonOrd::canon_cmp(&f64::NEG_INFINITY, &-1.0) == Ordering::Less);
        assert!(CanonOrd::canon_cmp(&-0.0, &0.0) == Ordering::Less); // -0 < +0
        assert!(CanonOrd::canon_cmp(&0.0, &f64::NAN) == Ordering::Less); // NaN is the greatest
        assert!(CanonOrd::canon_cmp(&f64::NAN, &f64::NAN) == Ordering::Equal);
    }

    #[test]
    fn canon_ord_deterministic_sort() {
        let mut v = [
            Value::Int(3),
            Value::Str("a".into()),
            Value::Int(-1),
            Value::Bool(true),
            Value::Date(Date::new(2026, 1, 1).unwrap()),
        ];
        v.sort_by(CanonOrd::canon_cmp);
        // Type ranks: Bool < Int < Date < Str
        assert!(matches!(v[0], Value::Bool(_)));
        assert_eq!(v[1], Value::Int(-1));
        assert_eq!(v[2], Value::Int(3));
        assert!(matches!(v[3], Value::Date(_)));
        assert!(matches!(v[4], Value::Str(_)));
    }

    // -- Value: equality/hashing/canonical encoding ---------------------------

    #[test]
    fn value_float_bit_equality() {
        assert_ne!(Value::Float(0.0), Value::Float(-0.0)); // different bit patterns
        assert_eq!(Value::Float(f64::NAN), Value::Float(f64::NAN)); // NaNs with the same bit pattern are equal
        assert_ne!(
            Value::Float(0.0).canonical_bytes(),
            Value::Float(-0.0).canonical_bytes()
        );
    }

    #[test]
    fn canonical_bytes_unique() {
        let mut rec = BTreeMap::new();
        rec.insert("b".into(), Value::Int(2));
        rec.insert("a".into(), Value::Int(1));
        let rec = Value::Record(rec);
        let vals = vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::Int(0),
            Value::Int(-1),
            Value::Float(1.5),
            Value::Decimal(bounded(10, 2, "1.50")),
            Value::Decimal(Decimal::unbounded(rd("1.5"))),
            Value::Date(Date::new(2026, 7, 11).unwrap()),
            Value::Str("".into()),
            Value::Str("a".into()),
            Value::Option(None),
            Value::Option(Some(Box::new(Value::Int(1)))),
            Value::Vector(vec![Value::Int(1)]),
            Value::Tuple(vec![Value::Int(1), Value::Int(2)]),
            Value::Tuple(vec![Value::Int(1)]),
            rec.clone(),
            Value::Enum { variant: "none".into(), payload: vec![] },
            Value::Enum { variant: "some".into(), payload: vec![Value::Int(1)] },
        ];
        let mut seen = std::collections::HashSet::new();
        for v in &vals {
            assert!(seen.insert(v.canonical_bytes()), "collision: {:?}", v);
        }
        // record field order does not matter
        let mut rec2 = BTreeMap::new();
        rec2.insert("a".into(), Value::Int(1));
        rec2.insert("b".into(), Value::Int(2));
        assert_eq!(rec.canonical_bytes(), Value::Record(rec2).canonical_bytes());
    }

    #[test]
    fn value_hash_eq_consistent() {
        use std::collections::HashSet;
        let a = Value::Vector(vec![Value::Float(f64::NAN), Value::Int(1)]);
        let b = Value::Vector(vec![Value::Float(f64::NAN), Value::Int(1)]);
        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }
}
