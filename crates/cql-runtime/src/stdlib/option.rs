//! Option group (doc/cql.md §4.6, Appendix B): direct mapping onto Rust `Option`.

/// `opt.map(f)` (option version; the vector version is `crate::stdlib::vector::vec_map`).
pub fn option_map<T, U>(opt: Option<T>, f: impl FnOnce(T) -> U) -> Option<U> {
    opt.map(f)
}

/// `opt.and_then(f)`: monadic bind.
pub fn and_then<T, U>(opt: Option<T>, f: impl FnOnce(T) -> Option<U>) -> Option<U> {
    opt.and_then(f)
}

/// `opt.unwrap_or(d)`.
pub fn unwrap_or<T>(opt: Option<T>, default: T) -> T {
    opt.unwrap_or(default)
}

/// `opt.is_some()`.
pub fn is_some<T>(opt: &Option<T>) -> bool {
    opt.is_some()
}

/// `opt.is_none()`.
pub fn is_none<T>(opt: &Option<T>) -> bool {
    opt.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_stdlib() {
        assert_eq!(option_map(Some(2), |x| x * 3), Some(6));
        assert_eq!(option_map::<i32, i32>(None, |x| x * 3), None);
        assert_eq!(and_then(Some(2), |x| Some(x + 1)), Some(3));
        assert_eq!(and_then(Some(2), |_| None::<i32>), None);
        assert_eq!(unwrap_or(None, 7), 7);
        assert_eq!(unwrap_or(Some(1), 7), 1);
        assert!(is_some(&Some(1)));
        assert!(is_none(&None::<i32>));
    }
}
