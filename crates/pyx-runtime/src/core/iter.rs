//! Runtime iterators for `for ... in` over lists, strings, dicts, tuples,
//! sets, ranges, text files and `csv.reader` snapshots (ARCHITECTURE §3.2,
//! Phase 2.0/2.7).
//!
//! Iterators are index-based: the target is not copied, so mutations made
//! during the loop are observed. Iterating a string yields one-character
//! strings by Unicode scalar value; dicts yield keys in insertion order.
//! Iterating a file yields its lines (each value keeps its trailing `\n`:
//! `str` for a text file, `bytes` for a binary file), reading one line per
//! step through the file's own cursor. A CSV reader yields a freshly built
//! list (or dict for `DictReader`) per parsed row.

use crate::exc::{self, TYPE_ERROR};
use crate::value;

/// `pyx_iter_new`: creates an iterator for a list, string, dict, tuple, set,
/// range, text file, CSV reader or a generator object.
pub(crate) fn iter_new(obj: i64) -> i64 {
    let generator = value::as_fiber(obj)
        .map(|fiber| fiber.kind == crate::fiber::FiberKind::Generator)
        .unwrap_or(false);
    if value::as_list(obj).is_some()
        || value::as_str(obj).is_some()
        || value::as_bytes(obj).is_some()
        || value::as_dict(obj).is_some()
        || value::as_defaultdict(obj).is_some()
        || value::as_tuple(obj).is_some()
        || value::as_set(obj).is_some()
        || value::as_range(obj).is_some()
        || value::as_file(obj).is_some()
        || value::as_csv_reader(obj).is_some()
        || generator
    {
        value::iter_new(obj)
    } else {
        exc::raise_with_message(
            TYPE_ERROR,
            &format!("'{}' object is not iterable", value::type_name(obj)),
        )
    }
}

/// `iter(obj)`: returns an existing iterator or generator unchanged (CPython
/// semantics), otherwise creates an iterator like [`iter_new`].
pub(crate) fn iter_of(obj: i64) -> i64 {
    let generator = value::as_fiber(obj)
        .map(|fiber| fiber.kind == crate::fiber::FiberKind::Generator)
        .unwrap_or(false);
    if generator || value::as_iter(obj).is_some() {
        obj
    } else {
        iter_new(obj)
    }
}

/// `next(obj[, default])`: advances an iterator or generator by one step.
///
/// Returns the element. At the end, raises `StopIteration` when no default was
/// given and returns the default otherwise. An exception raised inside a
/// generator propagates unchanged: it leaves a pending exception, which takes
/// precedence over the exhausted/default path.
pub(crate) fn next_value(obj: i64, default: i64, has_default: bool) -> i64 {
    let generator = value::as_fiber(obj)
        .map(|fiber| fiber.kind == crate::fiber::FiberKind::Generator)
        .unwrap_or(false);
    if !generator && value::as_iter(obj).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("'{}' object is not an iterator", value::type_name(obj)),
        );
    }
    let mut out: i64 = value::NONE;
    let produced = if generator {
        generator_next(obj, &mut out) == 1
    } else {
        iter_next(obj, &mut out) == 1
    };
    if produced {
        return out;
    }
    // A pending exception (from the iterator itself or a generator body) is
    // the result; it must not be swallowed by a default.
    if exc::pending() {
        return value::NONE;
    }
    if has_default {
        default
    } else {
        exc::raise_with_message(exc::STOP_ITERATION, "")
    }
}

/// `pyx_iter_next`: writes the next element to `out` and returns `1`, or
/// returns `0` at the end (no pending exception). An invalid iterator or a
/// `NULL` out-pointer raises `TypeError` and returns `0`.
pub(crate) fn iter_next(iter: i64, out: *mut i64) -> i64 {
    if out.is_null() {
        exc::raise_with_message(TYPE_ERROR, "iteration output pointer is NULL");
        return 0;
    }
    let target = match value::as_iter(iter) {
        Some(state) => state.target,
        None => {
            exc::raise_with_message(
                TYPE_ERROR,
                &format!("'{}' object is not an iterator", value::type_name(iter)),
            );
            return 0;
        }
    };
    // A generator target runs user code on resume, so no iterator borrow is
    // held across the switch.
    if value::as_fiber(target).is_some() {
        return generator_next(target, out);
    }
    let Some(state) = value::iter_state_mut(iter) else {
        exc::raise_with_message(
            TYPE_ERROR,
            &format!("'{}' object is not an iterator", value::type_name(iter)),
        );
        return 0;
    };

    let target = state.target;
    // Read from the target before updating the index so a failed step never
    // advances the iterator. Files and CSV readers keep their own cursor in
    // the target; that cursor advances with each yielded line/row.
    let next = if let Some(items) = value::as_list(target) {
        items.get(state.index).copied()
    } else if let Some(s) = value::as_str(target) {
        s.chars()
            .nth(state.index)
            .map(|ch| value::str_alloc(ch.to_string()))
    } else if let Some(bytes) = value::as_bytes(target) {
        // Iterating bytes yields each byte as an integer in `0..=255`.
        bytes
            .get(state.index)
            .and_then(|byte| value::encode_int(*byte as i64))
    } else if let Some(pairs) = value::as_dict(target) {
        pairs.get(state.index).map(|(key, _)| *key)
    } else if let Some((pairs, _)) = value::as_defaultdict(target) {
        pairs.get(state.index).map(|(key, _)| *key)
    } else if let Some(items) = value::as_tuple(target) {
        items.get(state.index).copied()
    } else if let Some(items) = value::as_set(target) {
        items.get(state.index).copied()
    } else if let Some((start, stop, step)) = value::as_range(target) {
        value::range_item(start, stop, step, state.index).and_then(value::encode_int)
    } else if let Some(file) = value::as_file(target) {
        // `for line in f` yields each line including its trailing newline:
        // `bytes` for a binary file, `str` for a text file.
        let line = if file.binary() {
            file.readline_bytes(None)
                .map(|line| line.map(value::bytes_new))
        } else {
            file.readline(None).map(|line| line.map(value::str_alloc))
        };
        match line {
            Ok(Some(item)) => Some(item),
            Ok(None) => None,
            Err(error) => {
                value::raise_file_error(error);
                return 0;
            }
        }
    } else if value::as_csv_reader(target).is_some() {
        // The snapshot is native; a fresh list/dict is built per yielded row.
        crate::csv::reader_item(target)
    } else {
        exc::raise_with_message(
            TYPE_ERROR,
            &format!("'{}' object is not iterable", value::type_name(target)),
        );
        return 0;
    };

    match next {
        Some(item) => {
            state.index += 1;
            // SAFETY: the caller supplied a writable `i64` slot (checked
            // above), as required by the ABI.
            unsafe { *out = item };
            1
        }
        None => 0,
    }
}

/// Resumes a generator object for `for` loops. The yielded value becomes the
/// next element; a finished generator ends iteration; a pending exception
/// inside the body is re-raised to the caller.
fn generator_next(generator: i64, out: *mut i64) -> i64 {
    if crate::fiber::is_done(generator) {
        if crate::fiber::has_error(generator) {
            crate::fiber::raise_error(generator);
        }
        return 0;
    }
    match crate::fiber::resume_value(generator) {
        Some(true) => {
            if crate::fiber::has_error(generator) {
                crate::fiber::raise_error(generator);
            }
            0
        }
        Some(false) => {
            let item = crate::fiber::yielded_value(generator);
            // SAFETY: checked above.
            unsafe { *out = item };
            1
        }
        None => {
            exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "'{}' object is not an iterator",
                    value::type_name(generator)
                ),
            );
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::encode_int;

    fn int(n: i64) -> i64 {
        encode_int(n).expect("representable")
    }

    fn string(s: &str) -> i64 {
        value::str_from_bytes(s.as_bytes())
    }

    fn take_all(iter: i64) -> Vec<i64> {
        let mut out: i64 = 0;
        let mut values = Vec::new();
        while iter_next(iter, &mut out) == 1 {
            values.push(out);
        }
        values
    }

    #[test]
    fn list_iteration() {
        exc::init();
        let list = value::list_new(vec![int(1), int(2), int(3)]);
        let iter = iter_new(list);
        assert_eq!(value::type_name(iter), "iterator");
        assert_eq!(take_all(iter), vec![int(1), int(2), int(3)]);
        assert!(!exc::pending());
    }

    #[test]
    fn string_iteration_is_unicode_aware() {
        exc::init();
        let iter = iter_new(string("hé"));
        let values = take_all(iter);
        assert_eq!(values.len(), 2);
        assert_eq!(value::as_str(values[0]), Some("h"));
        assert_eq!(value::as_str(values[1]), Some("é"));
        assert!(!exc::pending());
    }

    #[test]
    fn dict_iteration_yields_keys_in_order() {
        exc::init();
        let keys = [string("a"), string("b")];
        let values = [int(1), int(2)];
        let dict = crate::container::dict_new(2, keys.as_ptr(), values.as_ptr());
        let iter = iter_new(dict);
        let got = take_all(iter);
        assert_eq!(value::as_str(got[0]), Some("a"));
        assert_eq!(value::as_str(got[1]), Some("b"));
        assert!(!exc::pending());
    }

    #[test]
    fn tuple_iteration_yields_items() {
        exc::init();
        let t = value::tuple_new(vec![int(4), int(5)]);
        assert_eq!(take_all(iter_new(t)), vec![int(4), int(5)]);
        assert!(!exc::pending());
    }

    #[test]
    fn empty_and_invalid_iterables() {
        exc::init();
        let empty = value::list_new(Vec::new());
        assert_eq!(take_all(iter_new(empty)), Vec::<i64>::new());
        assert!(!exc::pending());

        let not_iterable = iter_new(int(1));
        assert_eq!(not_iterable, value::NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'int' object is not iterable"
        );

        exc::init();
        assert_eq!(iter_next(int(1), &mut 0), 0);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    fn set_iteration_yields_insertion_order() {
        exc::init();
        let s = crate::container::set_new(3, [int(3), int(1), int(3)].as_ptr());
        assert_eq!(take_all(iter_new(s)), vec![int(3), int(1)]);
        assert!(!exc::pending());
    }

    #[test]
    fn null_out_pointer_is_rejected() {
        exc::init();
        let list = value::list_new(vec![int(1)]);
        let iter = iter_new(list);
        assert_eq!(iter_next(iter, std::ptr::null_mut()), 0);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        // The iterator did not advance.
        let mut out = 0;
        assert_eq!(iter_next(iter, &mut out), 1);
        assert_eq!(out, int(1));
    }

    extern "C" fn yields_one_two(_argv: *const i64) -> i64 {
        crate::fiber::yield_value(int(1));
        crate::fiber::yield_value(int(2));
        value::NONE
    }

    extern "C" fn fails_after_first(_argv: *const i64) -> i64 {
        crate::fiber::yield_value(int(1));
        crate::exc::raise_with_message(crate::exc::VALUE_ERROR, "boom")
    }

    fn generator(entry: extern "C" fn(*const i64) -> i64) -> i64 {
        let created = value::fiber_generator_new(entry as usize, "gen".to_string(), Vec::new());
        created.expect("generator stack")
    }

    #[test]
    fn next_advances_generators_lazily() {
        exc::init();
        let gen = generator(yields_one_two);
        // Nothing ran yet: the body starts on the first `next`.
        assert_eq!(crate::fiber::yielded_value(gen), value::NONE);
        assert!(!crate::fiber::is_done(gen));

        assert_eq!(next_value(gen, value::NONE, false), int(1));
        assert_eq!(crate::fiber::yielded_value(gen), int(1));
        assert!(!crate::fiber::is_done(gen));

        assert_eq!(next_value(gen, value::NONE, false), int(2));
        assert!(!exc::pending());

        // Exhausted: no default raises StopIteration.
        assert_eq!(next_value(gen, value::NONE, false), value::NONE);
        assert_eq!(exc::pending_tag(), crate::exc::STOP_ITERATION as i64);
    }

    #[test]
    fn next_default_suppresses_stop_iteration() {
        exc::init();
        let exhausted = iter_of(value::list_new(Vec::new()));
        assert_eq!(next_value(exhausted, int(-1), true), int(-1));
        assert!(!exc::pending());

        let gen = generator(yields_one_two);
        assert_eq!(next_value(gen, int(-1), true), int(1));
        assert_eq!(next_value(gen, int(-1), true), int(2));
        assert_eq!(next_value(gen, int(-1), true), int(-1));
        assert!(!exc::pending());
    }

    #[test]
    fn iter_then_next_over_a_list() {
        exc::init();
        let iterator = iter_of(value::list_new(vec![int(7), int(8)]));
        assert_eq!(value::type_name(iterator), "iterator");
        assert_eq!(next_value(iterator, value::NONE, false), int(7));
        assert_eq!(next_value(iterator, value::NONE, false), int(8));
        assert_eq!(next_value(iterator, value::NONE, false), value::NONE);
        assert_eq!(exc::pending_tag(), crate::exc::STOP_ITERATION as i64);
    }

    #[test]
    fn iter_returns_existing_iterator_unchanged() {
        exc::init();
        let iterator = iter_of(value::list_new(vec![int(1)]));
        assert_eq!(iter_of(iterator), iterator);

        let gen = generator(yields_one_two);
        assert_eq!(iter_of(gen), gen);
        // The generator is accepted directly by `next`, without a wrapper.
        assert_eq!(value::type_name(gen), "generator");
        assert_eq!(next_value(gen, value::NONE, false), int(1));
    }

    #[test]
    fn next_rejects_non_iterators_with_type_error() {
        exc::init();
        let list = value::list_new(vec![int(1)]);
        assert_eq!(next_value(list, value::NONE, false), value::NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'list' object is not an iterator"
        );

        // A default does not mask the TypeError.
        exc::init();
        assert_eq!(next_value(list, int(0), true), value::NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    fn next_propagates_generator_exceptions() {
        exc::init();
        let gen = generator(fails_after_first);
        assert_eq!(next_value(gen, value::NONE, false), int(1));
        assert!(!exc::pending());

        // The body's ValueError wins over both StopIteration and the default.
        assert_eq!(next_value(gen, int(-1), true), value::NONE);
        assert_eq!(exc::pending_tag(), crate::exc::VALUE_ERROR as i64);
        assert_eq!(value::to_string(exc::pending_value()), "boom");
    }
}
