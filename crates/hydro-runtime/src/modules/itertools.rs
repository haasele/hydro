//! `itertools`: the iterator combinators used by the supported corpus.
//!
//! The runtime has no lazy general-purpose iterator protocol for arbitrary
//! user callables, so every combinator here materialises finite iterables into
//! lists (or returns an iterator over a range for `count`). The results are
//! ordinary lists, which are iterable, indexable and `list(...)`-convertible,
//! matching what the callers in `rich` observe.
//!
//! Deliberate limitations, all visible in the returned value:
//! * `chain`, `cycle`, `repeat`, `accumulate` and `tee` return lists (a single
//!   pass for `cycle`; `repeat` without a count is capped at
//!   [`REPEAT_WITHOUT_TIMES`] items);
//! * `zip_longest` fills shorter iterables with `None` and does not accept the
//!   keyword-only `fillvalue`;
//! * `count` returns an iterator over a range with an effectively unbounded
//!   stop, so repeated `next()` calls advance normally;
//! * `pairwise` returns a list of adjacent 2-tuples (CPython returns a lazy
//!   iterator), so an unbounded source is not streamed.
//!
//! `islice`, `filterfalse`, `chain` and `zip_longest` are the combinators
//! `rich` imports; the rest exist so the module surface matches CPython's.

use crate::builtins;
use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::iter;
use crate::ops;
use crate::value::{self, NONE};

/// Number of copies produced by `repeat(value)` when no count is given.
///
/// Python's `repeat` is infinite; a list cannot be, so the materialised result
/// is capped instead of exhausting memory.
const REPEAT_WITHOUT_TIMES: i64 = 4096;

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn attribute_error(name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module 'itertools' has no attribute '{name}'"),
    )
}

/// Decodes an integer argument, reporting `TypeError` on mismatch.
fn integer_argument(function: &str, position: &str, v: i64) -> Result<i64, i64> {
    match value::as_integer(v) {
        Some(n) => Ok(n),
        None => Err(type_error(&format!(
            "{function}() {position} must be an integer, not {}",
            value::type_name(v)
        ))),
    }
}

/// Decodes an optional integer argument (`None` selects the default).
fn optional_argument(function: &str, position: &str, v: i64) -> Result<Option<i64>, i64> {
    if v == NONE {
        return Ok(None);
    }
    integer_argument(function, position, v).map(Some)
}

/// Materialises an iterable into a rooted list.
///
/// Every element is appended through [`push_item`], so a collection triggered
/// by a later step still traces the elements accumulated so far.
fn collect(iterable: i64) -> Result<i64, i64> {
    let iterator = iter::iter_of(iterable);
    if exc::pending() {
        return Err(NONE);
    }
    let result = value::list_new(Vec::new());
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return Err(NONE);
        }
        if status == 0 {
            break;
        }
        push_item(result, item);
    }
    Ok(result)
}

/// Appends `item` to `list`, recording the GC edge.
fn push_item(list: i64, item: i64) {
    if let Some(mut items) = value::list_items_mut(list) {
        items.push(item);
        crate::heap::write_barrier_edge(list, item);
    }
}

/// Snapshot of a rooted list's elements.
fn snapshot(list: i64) -> Vec<i64> {
    value::as_list(list)
        .map(|items| items.to_vec())
        .unwrap_or_default()
}

/// Dispatches a native `itertools` function. Arity is enforced by the central
/// spec before this point.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "islice" => islice(args),
        "filterfalse" => filterfalse(args),
        "chain" => chain(args),
        "zip_longest" => zip_longest(args),
        "pairwise" => pairwise(args),
        "count" => count(args),
        "repeat" => repeat(args),
        "cycle" => cycle(args),
        "accumulate" => accumulate(args),
        "tee" => tee(args),
        _ => attribute_error(function),
    }
}

/// `islice(iterable, stop)` / `islice(iterable, start, stop[, step])`.
///
/// `None` selects the corresponding default; only the consumed prefix is
/// materialised, so an unbounded source with a finite stop terminates.
fn islice(args: &[i64]) -> i64 {
    let (start, stop, step) = match args.len() {
        // Arity is validated by the caller; the fallback keeps the indexing
        // total for direct Rust calls.
        0 | 1 => return type_error("islice() expects 2 to 4 arguments"),
        2 => {
            let stop = match optional_argument("islice", "stop", args[1]) {
                Ok(stop) => stop,
                Err(error) => return error,
            };
            (0, stop, 1)
        }
        3 => {
            let start = match optional_argument("islice", "start", args[1]) {
                Ok(start) => start.unwrap_or(0),
                Err(error) => return error,
            };
            let stop = match optional_argument("islice", "stop", args[2]) {
                Ok(stop) => stop,
                Err(error) => return error,
            };
            (start, stop, 1)
        }
        _ => {
            let start = match optional_argument("islice", "start", args[1]) {
                Ok(start) => start.unwrap_or(0),
                Err(error) => return error,
            };
            let stop = match optional_argument("islice", "stop", args[2]) {
                Ok(stop) => stop,
                Err(error) => return error,
            };
            let step = match optional_argument("islice", "step", args[3]) {
                Ok(step) => step.unwrap_or(1),
                Err(error) => return error,
            };
            (start, stop, step)
        }
    };
    if start < 0 {
        return value_error("islice() start must not be negative");
    }
    if stop.is_some_and(|stop| stop < 0) {
        return value_error("islice() stop must not be negative");
    }
    if step <= 0 {
        return value_error("islice() step must be greater than zero");
    }

    let iterator = iter::iter_of(args[0]);
    if exc::pending() {
        return NONE;
    }
    let result = value::list_new(Vec::new());
    let mut index: i64 = 0;
    loop {
        if stop.is_some_and(|limit| index >= limit) {
            break;
        }
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        let position = index;
        index += 1;
        if position < start || (position - start) % step != 0 {
            continue;
        }
        push_item(result, item);
    }
    result
}

/// `filterfalse(predicate, iterable)`: keeps the elements the predicate
/// rejects. A `None` predicate keeps false-y elements, like CPython.
fn filterfalse(args: &[i64]) -> i64 {
    let predicate = args[0];
    let iterator = iter::iter_of(args[1]);
    if exc::pending() {
        return NONE;
    }
    let result = value::list_new(Vec::new());
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        let keep = if predicate == NONE {
            !value::truthy(item)
        } else {
            let argument = item;
            let outcome = builtins::call(predicate, 1, &argument);
            if exc::pending() {
                return NONE;
            }
            !value::truthy(outcome)
        };
        if keep {
            push_item(result, item);
        }
    }
    result
}

/// `chain(*iterables)`: concatenates every argument in order.
fn chain(args: &[i64]) -> i64 {
    let result = value::list_new(Vec::new());
    for &iterable in args {
        let iterator = iter::iter_of(iterable);
        if exc::pending() {
            return NONE;
        }
        loop {
            let mut item = NONE;
            let status = iter::iter_next(iterator, &mut item);
            if exc::pending() {
                return NONE;
            }
            if status == 0 {
                break;
            }
            push_item(result, item);
        }
    }
    result
}

/// `zip_longest(*iterables)`: zips to the longest input, filling with `None`.
///
/// The keyword-only `fillvalue` is not representable through the native module
/// call ABI (the frontend only forwards positional arguments), so the fill is
/// always `None`.
fn zip_longest(args: &[i64]) -> i64 {
    // Root the per-iterable snapshots while the remaining ones are collected.
    let sources = value::list_new(Vec::new());
    for &iterable in args {
        let collected = match collect(iterable) {
            Ok(collected) => collected,
            Err(error) => return error,
        };
        push_item(sources, collected);
    }
    let handles = snapshot(sources);
    let mut longest = 0usize;
    for &handle in &handles {
        if let Some(items) = value::as_list(handle) {
            longest = longest.max(items.len());
        }
    }
    let result = value::list_new(Vec::new());
    for index in 0..longest {
        let mut row = Vec::with_capacity(handles.len());
        for &handle in &handles {
            let item = value::as_list(handle)
                .and_then(|items| items.get(index).copied())
                .unwrap_or(NONE);
            row.push(item);
        }
        let tuple = value::tuple_new(row);
        push_item(result, tuple);
    }
    result
}

/// `pairwise(iterable)`: adjacent overlapping pairs as 2-tuples.
///
/// CPython returns a lazy iterator; like the other combinators the pairs are
/// materialised into a list. The previous element is carried in a one-slot
/// list so the collector can trace it across allocations while the source is
/// advanced.
fn pairwise(args: &[i64]) -> i64 {
    let iterator = iter::iter_of(args[0]);
    if exc::pending() {
        return NONE;
    }
    let result = value::list_new(Vec::new());
    let carry = value::list_new(vec![NONE]);
    let mut has_previous = false;
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        if has_previous {
            let previous = value::as_list(carry)
                .and_then(|items| items.first().copied())
                .unwrap_or(NONE);
            let pair = value::tuple_new(vec![previous, item]);
            push_item(result, pair);
        }
        if let Some(mut items) = value::list_items_mut(carry) {
            items[0] = item;
        }
        crate::heap::write_barrier_edge(carry, item);
        has_previous = true;
    }
    result
}

/// `count(start=0, step=1)`: an iterator over an effectively unbounded range.
fn count(args: &[i64]) -> i64 {
    let start = match args.first() {
        Some(&start) => match optional_argument("count", "start", start) {
            Ok(start) => start.unwrap_or(0),
            Err(error) => return error,
        },
        None => 0,
    };
    let step = match args.get(1) {
        Some(&step) => match optional_argument("count", "step", step) {
            Ok(step) => step.unwrap_or(1),
            Err(error) => return error,
        },
        None => 1,
    };
    if step == 0 {
        return value_error("count() step must not be zero");
    }
    let stop = if step > 0 { i64::MAX } else { i64::MIN };
    let range = value::range_new(start, stop, step);
    value::iter_new(range)
}

/// `repeat(value, times=None)`: a list with `times` copies of `value`.
fn repeat(args: &[i64]) -> i64 {
    let times = match args.get(1) {
        Some(&times) => match optional_argument("repeat", "times", times) {
            Ok(times) => times.unwrap_or(REPEAT_WITHOUT_TIMES),
            Err(error) => return error,
        },
        None => REPEAT_WITHOUT_TIMES,
    };
    let result = value::list_new(Vec::new());
    for _ in 0..times.max(0) {
        push_item(result, args[0]);
    }
    result
}

/// `cycle(iterable)`: a list holding one pass over `iterable`.
fn cycle(args: &[i64]) -> i64 {
    match collect(args[0]) {
        Ok(result) => result,
        Err(error) => error,
    }
}

/// `accumulate(iterable[, function[, initial]])`: the running combinations.
///
/// Without a function the values are added with the runtime's `+` operator.
fn accumulate(args: &[i64]) -> i64 {
    let function = args.get(1).copied().unwrap_or(NONE);
    let initial = args.get(2).copied().unwrap_or(NONE);
    let iterator = iter::iter_of(args[0]);
    if exc::pending() {
        return NONE;
    }
    let result = value::list_new(Vec::new());
    let mut accumulator = NONE;
    let mut has_accumulator = false;
    if args.len() > 2 && initial != NONE {
        accumulator = initial;
        has_accumulator = true;
        push_item(result, accumulator);
    }
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        if has_accumulator {
            accumulator = if function == NONE {
                ops::binop(ops::OP_ADD, accumulator, item)
            } else {
                let pair = [accumulator, item];
                builtins::call(function, 2, pair.as_ptr())
            };
            if exc::pending() {
                return NONE;
            }
        } else {
            accumulator = item;
            has_accumulator = true;
        }
        push_item(result, accumulator);
    }
    result
}

/// `tee(iterable, n=2)`: `n` independent iterators over a shared snapshot.
fn tee(args: &[i64]) -> i64 {
    let snapshot_list = match collect(args[0]) {
        Ok(snapshot_list) => snapshot_list,
        Err(error) => return error,
    };
    let count = match args.get(1) {
        Some(&count) => match optional_argument("tee", "n", count) {
            Ok(count) => count.unwrap_or(2),
            Err(error) => return error,
        },
        None => 2,
    };
    if count < 0 {
        return value_error("tee() n must not be negative");
    }
    let iterators = value::list_new(Vec::new());
    for _ in 0..count {
        let iterator = iter::iter_of(snapshot_list);
        if exc::pending() {
            return NONE;
        }
        push_item(iterators, iterator);
    }
    value::tuple_new(snapshot(iterators))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{as_integer, as_list, encode_int};

    fn int(n: i64) -> i64 {
        encode_int(n).expect("small integer")
    }

    fn list_of(items: Vec<i64>) -> i64 {
        value::list_new(items)
    }

    fn integers(v: i64) -> Vec<i64> {
        as_list(v)
            .expect("list result")
            .iter()
            .map(|&item| as_integer(item).expect("integer element"))
            .collect()
    }

    #[test]
    fn chain_concatenates_in_order() {
        exc::init();
        let first = list_of(vec![int(1), int(2)]);
        let second = list_of(vec![int(3)]);
        assert_eq!(integers(call("chain", &[first, second])), vec![1, 2, 3]);
        assert_eq!(integers(call("chain", &[])), Vec::<i64>::new());
        assert!(!exc::pending());
    }

    #[test]
    fn islice_supports_both_spellings() {
        exc::init();
        let source = list_of(vec![int(0), int(1), int(2), int(3), int(4)]);
        assert_eq!(integers(call("islice", &[source, int(2)])), vec![0, 1]);
        assert_eq!(
            integers(call("islice", &[source, int(1), int(4)])),
            vec![1, 2, 3]
        );
        assert_eq!(
            integers(call("islice", &[source, NONE, int(3)])),
            vec![0, 1, 2]
        );
        assert_eq!(
            integers(call("islice", &[source, int(0), int(5), int(2)])),
            vec![0, 2, 4]
        );
        assert!(!exc::pending());
    }

    #[test]
    fn islice_rejects_negative_and_zero_step() {
        exc::init();
        let source = list_of(vec![int(0)]);
        call("islice", &[source, int(-1)]);
        assert!(exc::pending());
        exc::clear();
        call("islice", &[source, int(0), int(0), int(0)]);
        assert!(exc::pending());
        exc::clear();
    }

    #[test]
    fn filterfalse_keeps_rejected_elements() {
        exc::init();
        let source = list_of(vec![int(0), int(1), int(2)]);
        assert_eq!(integers(call("filterfalse", &[NONE, source])), vec![0]);
        assert!(!exc::pending());
    }

    #[test]
    fn zip_longest_fills_with_none() {
        exc::init();
        let first = list_of(vec![int(1), int(2)]);
        let second = list_of(vec![int(3)]);
        let result = call("zip_longest", &[first, second]);
        let rows = as_list(result).expect("rows");
        assert_eq!(rows.len(), 2);
        let first_row = value::as_tuple(rows[0]).expect("tuple row");
        assert_eq!(as_integer(first_row[0]), Some(1));
        assert_eq!(as_integer(first_row[1]), Some(3));
        let second_row = value::as_tuple(rows[1]).expect("tuple row");
        assert_eq!(as_integer(second_row[0]), Some(2));
        assert_eq!(second_row[1], NONE);
        assert!(!exc::pending());
    }

    #[test]
    fn pairwise_yields_adjacent_overlapping_pairs() {
        exc::init();
        let source = list_of(vec![int(1), int(2), int(3)]);
        let result = call("pairwise", &[source]);
        let rows = as_list(result).expect("list result");
        assert_eq!(rows.len(), 2);
        let first = value::as_tuple(rows[0]).expect("tuple pair");
        assert_eq!(as_integer(first[0]), Some(1));
        assert_eq!(as_integer(first[1]), Some(2));
        let second = value::as_tuple(rows[1]).expect("tuple pair");
        assert_eq!(as_integer(second[0]), Some(2));
        assert_eq!(as_integer(second[1]), Some(3));
        assert!(!exc::pending());

        for items in [Vec::new(), vec![int(7)]] {
            let pairs = call("pairwise", &[list_of(items)]);
            assert!(
                as_list(pairs).is_some_and(|items| items.is_empty()),
                "fewer than two elements produce no pairs"
            );
            assert!(!exc::pending());
        }
    }

    #[test]
    fn count_iterates_from_its_start() {
        exc::init();
        let counter = call("count", &[int(5)]);
        let first = iter::next_value(counter, NONE, false);
        let second = iter::next_value(counter, NONE, false);
        assert_eq!(as_integer(first), Some(5));
        assert_eq!(as_integer(second), Some(6));
        assert!(!exc::pending());
    }

    #[test]
    fn repeat_materialises_a_fixed_number_of_copies() {
        exc::init();
        let value = int(7);
        assert_eq!(integers(call("repeat", &[value, int(3)])), vec![7, 7, 7]);
        assert_eq!(
            integers(call("repeat", &[value, int(0)])),
            Vec::<i64>::new()
        );
        assert!(!exc::pending());
    }

    #[test]
    fn cycle_accumulate_and_tee_have_list_semantics() {
        exc::init();
        let source = list_of(vec![int(1), int(2), int(3)]);
        assert_eq!(integers(call("cycle", &[source])), vec![1, 2, 3]);
        assert_eq!(integers(call("accumulate", &[source])), vec![1, 3, 6]);
        let iterators = call("tee", &[source]);
        assert_eq!(value::as_tuple(iterators).map(|items| items.len()), Some(2));
        assert!(!exc::pending());
    }
}
