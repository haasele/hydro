//! `bisect` / `_bisect`: binary search and insertion over lists.
//!
//! Semantics follow the Hydro compatibility contract: `lo`/`hi` are normalised
//! like list slice bounds (negative offsets count from the end, then clamp to
//! `0..=len`), and the `key` callable is applied to the list elements but not
//! to the searched value. Comparisons go through `ops::binop`, so ordering
//! errors propagate unchanged.

use crate::exc::{self, TYPE_ERROR};
use crate::value::{self, NONE};
use crate::{builtins, heap, ops};

/// Normalises one `lo`/`hi` bound the way CPython list slice bounds behave.
fn normalize_bound(raw: i64, len: usize) -> usize {
    // `Vec` lengths never exceed `isize::MAX`, so the conversion fits.
    let len = len as i64;
    let raw = if raw < 0 {
        len.saturating_add(raw)
    } else {
        raw
    };
    raw.clamp(0, len) as usize
}

/// `lo=None` defaults to 0; `hi=None` defaults to the list length.
fn bound(argument: i64, len: usize, default: usize, name: &str) -> Result<usize, i64> {
    if argument == NONE {
        return Ok(default);
    }
    let Some(raw) = value::as_integer(argument) else {
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{name} must be an integer, not {}",
                value::type_name(argument)
            ),
        ));
    };
    Ok(normalize_bound(raw, len))
}

fn list_argument(argument: i64) -> Result<Vec<i64>, i64> {
    match value::as_list(argument) {
        // Copy before any callback runs: a `key` call can mutate (and
        // reallocate) the source list, invalidating a borrowed slice.
        Some(items) => Ok(items.to_vec()),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "argument 'a' must be a list, not {}",
                value::type_name(argument)
            ),
        )),
    }
}

fn callable_key(argument: i64) -> Result<(), i64> {
    if argument == NONE
        || value::as_function(argument).is_some()
        || value::as_class(argument).is_some()
        || value::as_ext_function(argument).is_some()
    {
        return Ok(());
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!("key must be callable, not {}", value::type_name(argument)),
    ))
}

/// `a < b` (or the requested relational operator) with pending exceptions
/// surfaced as `Err`.
fn compare(op: u32, a: i64, b: i64) -> Result<bool, i64> {
    let result = ops::binop(op, a, b);
    if exc::pending() {
        Err(result)
    } else {
        Ok(value::truthy(result))
    }
}

/// Applies `key` to a list element; `None` means the element itself.
fn keyed(key: i64, item: i64) -> Result<i64, i64> {
    if key == NONE {
        return Ok(item);
    }
    let arguments = [item];
    let result = builtins::call(key, 1, arguments.as_ptr());
    if exc::pending() {
        Err(result)
    } else {
        Ok(result)
    }
}

/// Computes the insertion position; `right` selects `bisect_right`
/// (elements equal to `x` stay to its left).
fn bisect_index(
    items: &[i64],
    x: i64,
    lo: usize,
    hi: usize,
    key: i64,
    right: bool,
) -> Result<usize, i64> {
    let mut lo = lo.min(items.len());
    let mut hi = hi.min(items.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let middle = keyed(key, items[mid])?;
        if right {
            // `bisect_right`: x < middle -> search left, otherwise right.
            if compare(ops::OP_LT, x, middle)? {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        } else {
            // `bisect_left`: middle < x -> search right, otherwise left.
            if compare(ops::OP_LT, middle, x)? {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
    }
    Ok(lo)
}

/// `bisect_left(a, x, lo=0, hi=len(a), key=None)`.
fn bisect_left(args: &[i64]) -> i64 {
    search(args, false)
}

/// `bisect_right(a, x, lo=0, hi=len(a), key=None)` (also `bisect`).
fn bisect_right(args: &[i64]) -> i64 {
    search(args, true)
}

fn search(args: &[i64], right: bool) -> i64 {
    let Some(&list_value) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "bisect() missing required argument 'a'");
    };
    let Some(&x) = args.get(1) else {
        return exc::raise_with_message(TYPE_ERROR, "bisect() missing required argument 'x'");
    };
    let items = match list_argument(list_value) {
        Ok(items) => items,
        Err(error) => return error,
    };
    let lo = match bound(args.get(2).copied().unwrap_or(NONE), items.len(), 0, "lo") {
        Ok(lo) => lo,
        Err(error) => return error,
    };
    let hi = match bound(
        args.get(3).copied().unwrap_or(NONE),
        items.len(),
        items.len(),
        "hi",
    ) {
        Ok(hi) => hi,
        Err(error) => return error,
    };
    let key = args.get(4).copied().unwrap_or(NONE);
    if let Err(error) = callable_key(key) {
        return error;
    }
    match bisect_index(&items, x, lo, hi, key, right) {
        Ok(position) => ops::int_result(position as i64),
        Err(error) => error,
    }
}

/// `insort_left(a, x, lo=0, hi=len(a), key=None)` and `insort_right`
/// (`insort`).
fn insort(args: &[i64], right: bool) -> i64 {
    let Some(&list_value) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "insort() missing required argument 'a'");
    };
    let Some(&x) = args.get(1) else {
        return exc::raise_with_message(TYPE_ERROR, "insort() missing required argument 'x'");
    };
    let items = match list_argument(list_value) {
        Ok(items) => items,
        Err(error) => return error,
    };
    let lo = match bound(args.get(2).copied().unwrap_or(NONE), items.len(), 0, "lo") {
        Ok(lo) => lo,
        Err(error) => return error,
    };
    let hi = match bound(
        args.get(3).copied().unwrap_or(NONE),
        items.len(),
        items.len(),
        "hi",
    ) {
        Ok(hi) => hi,
        Err(error) => return error,
    };
    let key = args.get(4).copied().unwrap_or(NONE);
    if let Err(error) = callable_key(key) {
        return error;
    }
    // CPython's `insort*` applies the key to the inserted value before the
    // search (`bisect_right(a, key(x), ..., key=key)`); `bisect*` does not.
    let search_x = match keyed(key, x) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let position = match bisect_index(&items, search_x, lo, hi, key, right) {
        Ok(position) => position,
        Err(error) => return error,
    };
    let Some(mut items) = value::list_items_mut(list_value) else {
        return exc::raise_with_message(TYPE_ERROR, "argument 'a' must be a list");
    };
    let position = position.min(items.len());
    items.insert(position, x);
    heap::write_barrier_edge(list_value, x);
    NONE
}

/// Dispatches `module.function(...)` calls for `bisect` and `_bisect`.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "bisect_left" => bisect_left(args),
        "bisect_right" | "bisect" => bisect_right(args),
        "insort_left" => insort(args, false),
        "insort_right" | "insort" => insort(args, true),
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module 'bisect' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::encode_int;

    fn int(n: i64) -> i64 {
        encode_int(n).expect("representable")
    }

    fn number(tagged: i64) -> i64 {
        value::as_integer(tagged).expect("integer result")
    }

    fn list(values: &[i64]) -> i64 {
        value::list_new(values.to_vec())
    }

    fn items(value: i64) -> Vec<i64> {
        value::as_list(value)
            .expect("list")
            .iter()
            .map(|&item| number(item))
            .collect()
    }

    /// `operator.neg` resolved the same way user code sees it.
    fn neg() -> i64 {
        value::native_function_new("operator", "neg", 1)
    }

    fn bisect(name: &str, args: &[i64]) -> i64 {
        call(name, args)
    }

    #[test]
    fn left_and_right_positions() {
        exc::init();
        let a = list(&[int(1), int(3), int(3), int(5)]);
        assert_eq!(number(bisect("bisect_left", &[a, int(3)])), 1);
        assert_eq!(number(bisect("bisect_right", &[a, int(3)])), 3);
        assert_eq!(number(bisect("bisect", &[a, int(3)])), 3);
        assert_eq!(number(bisect("bisect_left", &[a, int(0)])), 0);
        assert_eq!(number(bisect("bisect_right", &[a, int(6)])), 4);
        assert!(!exc::pending());
    }

    #[test]
    fn bounds_are_slice_normalised() {
        exc::init();
        let a = list(&[int(1), int(2), int(3), int(4), int(5)]);
        let none = NONE;
        assert_eq!(
            number(bisect("bisect_left", &[a, int(3), int(0), int(2)])),
            2
        );
        // Negative lo counts from the end and clamps at zero.
        assert_eq!(
            number(bisect("bisect_left", &[a, int(3), int(-3), none])),
            2
        );
        assert_eq!(number(bisect("bisect_left", &[a, int(1), int(-99)])), 0);
        // Negative hi counts from the end; 5 is outside [1, 2, 3, 4].
        assert_eq!(
            number(bisect("bisect_left", &[a, int(5), int(0), int(-1)])),
            4
        );
        assert_eq!(
            number(bisect("bisect_right", &[a, int(3), int(4), int(2)])),
            4
        );
        assert!(!exc::pending());
    }

    #[test]
    fn empty_list_and_duplicates() {
        exc::init();
        let empty = list(&[]);
        assert_eq!(number(bisect("bisect_right", &[empty, int(1)])), 0);
        let a = list(&[int(2), int(2), int(2)]);
        assert_eq!(number(bisect("bisect_left", &[a, int(2)])), 0);
        assert_eq!(number(bisect("bisect_right", &[a, int(2)])), 3);
        assert!(!exc::pending());
    }

    #[test]
    fn key_applies_to_elements_only() {
        exc::init();
        // key=neg makes the keys ascending: [-1, -2, -3] -> [1, 2, 3].
        let a = list(&[int(-1), int(-2), int(-3)]);
        assert_eq!(
            number(bisect("bisect_left", &[a, int(2), NONE, NONE, neg()])),
            1
        );
        assert_eq!(
            number(bisect("bisect_right", &[a, int(2), NONE, NONE, neg()])),
            2
        );
        assert!(!exc::pending());
    }

    #[test]
    fn insort_preserves_order_in_place() {
        exc::init();
        let a = list(&[int(1), int(3), int(5)]);
        assert_eq!(bisect("insort_right", &[a, int(2)]), NONE);
        assert_eq!(items(a), vec![1, 2, 3, 5]);
        // `insort` is `insort_right`; equal values land after existing ones.
        assert_eq!(bisect("insort", &[a, int(3)]), NONE);
        assert_eq!(items(a), vec![1, 2, 3, 3, 5]);
        assert_eq!(bisect("insort_left", &[a, int(3)]), NONE);
        assert_eq!(items(a), vec![1, 2, 3, 3, 3, 5]);
        assert!(!exc::pending());
    }

    #[test]
    fn insort_respects_bounds() {
        exc::init();
        let a = list(&[int(1), int(2), int(4)]);
        assert_eq!(bisect("insort_left", &[a, int(3), int(0), int(2)]), NONE);
        assert_eq!(items(a), vec![1, 2, 3, 4]);
        // A negative hi counts from the end; insertion stays at or before it.
        let b = list(&[int(1), int(3), int(5)]);
        assert_eq!(bisect("insort_right", &[b, int(4), NONE, int(-1)]), NONE);
        assert_eq!(items(b), vec![1, 3, 4, 5]);
        assert!(!exc::pending());
    }

    #[test]
    fn type_errors_do_not_panic() {
        exc::init();
        let a = list(&[int(1), int(2)]);
        assert_eq!(bisect("bisect_left", &[int(1), int(2)]), NONE);
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        assert_eq!(
            bisect("bisect_left", &[a, int(1), value::str_from_bytes(b"lo")]),
            NONE
        );
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        assert_eq!(
            bisect("bisect_left", &[a, int(1), NONE, NONE, int(7)]),
            NONE
        );
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        assert_eq!(bisect("unknown", &[a, int(1)]), NONE);
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), exc::ATTRIBUTE_ERROR as i64);
        exc::clear();

        // A missing argument is reported rather than indexing out of bounds.
        assert_eq!(bisect("bisect_left", &[a]), NONE);
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
    }
}
