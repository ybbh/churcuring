//! Date group (doc/cql.md Appendix B): thin wrapper over `crate::value::Date`.

use crate::value::Date;

/// `year(d)`.
pub fn year(d: &Date) -> i64 {
    d.year()
}

/// `month(d)`: 1–12.
pub fn month(d: &Date) -> i64 {
    d.month()
}

/// `day(d)`: 1–31.
pub fn day(d: &Date) -> i64 {
    d.day()
}

/// `add_days(d, n)`; outside the representable range ⇒ `None` (the spec does not define a
/// date-overflow trap; out-of-range is expressed as `None`, see `Date::add_days`).
pub fn add_days(d: &Date, n: i64) -> Option<Date> {
    d.add_days(n)
}

/// `days_between(a, b)` = the number of days in `a - b`.
pub fn days_between(a: &Date, b: &Date) -> i64 {
    a.days_between(b)
}

/// `parse_date(s)`: ISO 8601 `YYYY-MM-DD`; failure ⇒ `None`.
pub fn parse_date(s: &str) -> Option<Date> {
    Date::parse(s)
}

/// `day_of_week(d)`: 0 = Monday.
pub fn day_of_week(d: &Date) -> i64 {
    d.day_of_week()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_stdlib() {
        let d = parse_date("2026-07-11").unwrap();
        assert_eq!((year(&d), month(&d), day(&d)), (2026, 7, 11));
        assert_eq!(day_of_week(&d), 5);
        let e = add_days(&d, 1).unwrap();
        assert_eq!(days_between(&e, &d), 1);
        assert!(parse_date("2026/07/11").is_none());
    }
}
