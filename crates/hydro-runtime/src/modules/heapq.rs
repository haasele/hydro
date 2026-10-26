//! `heapq` / `_heapq`: min-heap operations over Python lists.
//!
//! The heap algorithms follow CPython's `_heapq`: `heappush` sifts the new
//! item up toward the root, while `heappop`/`heapreplace` move the last item
//! to the root and sift it down. All ordering goes through `ops::binop`, so
//! comparison errors propagate and no input can panic.
//!
//! `nlargest`/`nsmallest` decorate each element with its key exactly once and
//! keep at most `n` candidates in a bounded heap, preferring earlier elements
//! when keys compare equal.

use std::cmp::Ordering;

use crate::exc::{self, INDEX_ERROR, TYPE_ERROR};
use crate::value::{self, NONE};
use crate::{builtins, heap as gc, iter, ops};

/// One heap entry. `key` drives the heap ordering; `order` is the element's
/// original position, used to keep ties stable (and to evict later elements
/// first in `nlargest`/`nsmallest`); `item` is the value returned to Python.
#[derive(Clone, Copy)]
struct Entry {
    key: i64,
    order: i64,
    item: i64,
}

/// Ordering used by heap operations. `Min` is the ordinary min-heap;
/// `Max` reverses the key comparison for `nsmallest`'s bounded heap.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HeapKind {
    Min,
    Max,
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn index_error() -> i64 {
    exc::raise_with_message(INDEX_ERROR, "index out of range")
}

/// `a < b` with a pending exception surfaced as `Err`.
fn compare(op: u32, a: i64, b: i64) -> Result<bool, i64> {
    let result = ops::binop(op, a, b);
    if exc::pending() {
        Err(result)
    } else {
        Ok(value::truthy(result))
    }
}

/// Compares two keys: `Some(Less)`/`Some(Greater)` for a strict order and
/// `None` when neither operand is less (equal or unordered, like NaN).
fn key_order(a: i64, b: i64) -> Result<Option<Ordering>, i64> {
    if compare(ops::OP_LT, a, b)? {
        return Ok(Some(Ordering::Less));
    }
    if compare(ops::OP_LT, b, a)? {
        return Ok(Some(Ordering::Greater));
    }
    Ok(None)
}

/// Heap ordering. Equal keys order later elements as "smaller" so they sit at
/// the root and are the first to be replaced, keeping earlier ties.
fn heap_less(a: &Entry, b: &Entry, kind: HeapKind) -> Result<bool, i64> {
    match key_order(a.key, b.key)? {
        Some(Ordering::Less) => Ok(kind == HeapKind::Min),
        Some(Ordering::Greater) => Ok(kind == HeapKind::Max),
        Some(Ordering::Equal) | None => Ok(a.order > b.order),
    }
}

fn sift_down(
    entries: &mut [Entry],
    start: usize,
    mut pos: usize,
    kind: HeapKind,
) -> Result<(), i64> {
    let new_item = entries[pos];
    while pos > start {
        let parent = (pos - 1) / 2;
        if heap_less(&new_item, &entries[parent], kind)? {
            entries[pos] = entries[parent];
            pos = parent;
        } else {
            break;
        }
    }
    entries[pos] = new_item;
    Ok(())
}

fn sift_up(entries: &mut [Entry], pos: usize, kind: HeapKind) -> Result<(), i64> {
    let end = entries.len();
    let start = pos;
    let new_item = entries[pos];
    let mut pos = pos;
    let mut child = 2 * pos + 1;
    while child < end {
        let right = child + 1;
        if right < end && !heap_less(&entries[child], &entries[right], kind)? {
            child = right;
        }
        entries[pos] = entries[child];
        pos = child;
        child = 2 * pos + 1;
    }
    entries[pos] = new_item;
    sift_down(entries, start, pos, kind)
}

fn heap_push(entries: &mut Vec<Entry>, item: Entry, kind: HeapKind) -> Result<(), i64> {
    entries.push(item);
    let pos = entries.len() - 1;
    sift_down(entries, 0, pos, kind)
}

fn heap_pop(entries: &mut Vec<Entry>, kind: HeapKind) -> Result<Entry, i64> {
    let Some(last) = entries.pop() else {
        return Err(index_error());
    };
    if entries.is_empty() {
        return Ok(last);
    }
    let result = entries[0];
    entries[0] = last;
    sift_up(entries, 0, kind)?;
    Ok(result)
}

fn heap_replace(entries: &mut [Entry], item: Entry, kind: HeapKind) -> Result<Entry, i64> {
    if entries.is_empty() {
        return Err(index_error());
    }
    let result = entries[0];
    entries[0] = item;
    sift_up(entries, 0, kind)?;
    Ok(result)
}

fn heap_pushpop(entries: &mut [Entry], item: Entry, kind: HeapKind) -> Result<Entry, i64> {
    if !entries.is_empty() && heap_less(&entries[0], &item, kind)? {
        let result = entries[0];
        entries[0] = item;
        sift_up(entries, 0, kind)?;
        Ok(result)
    } else {
        Ok(item)
    }
}

fn heapify(entries: &mut [Entry], kind: HeapKind) -> Result<(), i64> {
    for pos in (0..entries.len() / 2).rev() {
        sift_up(entries, pos, kind)?;
    }
    Ok(())
}

/// Copies the list argument into heap entries.
fn heap_argument(argument: i64) -> Result<Vec<Entry>, i64> {
    let Some(items) = value::as_list(argument) else {
        return Err(type_error("heap argument must be a list"));
    };
    Ok(items
        .iter()
        .map(|&item| Entry {
            key: item,
            order: 0,
            item,
        })
        .collect())
}

/// Writes the entries back into the Python list, publishing new edges.
fn store(heap_value: i64, entries: &[Entry]) -> i64 {
    let Some(mut items) = value::list_items_mut(heap_value) else {
        return type_error("heap argument must be a list");
    };
    items.clear();
    items.extend(entries.iter().map(|entry| entry.item));
    for entry in entries {
        gc::write_barrier_edge(heap_value, entry.item);
    }
    NONE
}

fn heap_operation(function: &str, args: &[i64]) -> i64 {
    let Some(&heap_value) = args.first() else {
        return type_error("heap argument must be a list");
    };
    let item = args.get(1).copied().unwrap_or(NONE);
    let mut entries = match heap_argument(heap_value) {
        Ok(entries) => entries,
        Err(error) => return error,
    };
    let entry = Entry {
        key: item,
        order: 0,
        item,
    };
    let result = match function {
        "heappush" => heap_push(&mut entries, entry, HeapKind::Min).map(|()| NONE),
        "heappop" => heap_pop(&mut entries, HeapKind::Min).map(|entry| entry.item),
        "heapify" => heapify(&mut entries, HeapKind::Min).map(|()| NONE),
        "heappushpop" => heap_pushpop(&mut entries, entry, HeapKind::Min).map(|entry| entry.item),
        "heapreplace" => heap_replace(&mut entries, entry, HeapKind::Min).map(|entry| entry.item),
        _ => Err(type_error(&format!(
            "module 'heapq' has no attribute '{function}'"
        ))),
    };
    let result = match result {
        Ok(result) => result,
        Err(error) => return error,
    };
    // `heappop`/`heappushpop`/`heapreplace` also mutate the list; `heapify`
    // rewrites it. `heappush` wrote `entries` through `heap_push`.
    let stored = store(heap_value, &entries);
    if exc::pending() {
        return stored;
    }
    result
}

/// Validates the optional `key` argument.
fn callable_key(argument: i64) -> Result<(), i64> {
    if argument == NONE
        || value::as_function(argument).is_some()
        || value::as_class(argument).is_some()
        || value::as_ext_function(argument).is_some()
    {
        return Ok(());
    }
    Err(type_error(&format!(
        "key must be callable, not {}",
        value::type_name(argument)
    )))
}

fn keyed(key: i64, item: i64) -> Result<i64, i64> {
    if key == NONE {
        return Ok(item);
    }
    let arguments = [item];
    let result = builtins::call(key, 1, arguments.as_ptr());
    if exc::pending() {
        Err(NONE)
    } else {
        Ok(result)
    }
}

/// Materialises an iterable into a rooted list, so every element stays alive
/// while later key calls allocate.
fn collect_items(iterable: i64) -> Result<i64, i64> {
    let iterator = iter::iter_new(iterable);
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
        if let Some(mut items) = value::list_items_mut(result) {
            items.push(item);
            gc::write_barrier_edge(result, item);
        }
    }
    Ok(result)
}

/// Computes one key per element and roots them in a list for the same reason.
fn decorate_keys(items: &[i64], key: i64) -> Result<i64, i64> {
    let keys = value::list_new(Vec::new());
    for &item in items {
        let computed = keyed(key, item)?;
        if let Some(mut stored) = value::list_items_mut(keys) {
            stored.push(computed);
            gc::write_barrier_edge(keys, computed);
        }
    }
    Ok(keys)
}

/// Final ordering: `nlargest` descending, `nsmallest` ascending, with equal
/// keys keeping their original order.
fn output_less(a: &Entry, b: &Entry, largest: bool) -> Result<bool, i64> {
    match key_order(a.key, b.key)? {
        Some(Ordering::Less) => Ok(!largest),
        Some(Ordering::Greater) => Ok(largest),
        Some(Ordering::Equal) | None => Ok(a.order < b.order),
    }
}

fn merge_sort(mut entries: Vec<Entry>, largest: bool) -> Result<Vec<Entry>, i64> {
    let len = entries.len();
    if len <= 1 {
        return Ok(entries);
    }
    let right = entries.split_off(len / 2);
    let left = merge_sort(entries, largest)?;
    let right = merge_sort(right, largest)?;
    let mut merged = Vec::with_capacity(len);
    let (mut i, mut j) = (0usize, 0usize);
    while i < left.len() && j < right.len() {
        if output_less(&right[j], &left[i], largest)? {
            merged.push(right[j]);
            j += 1;
        } else {
            merged.push(left[i]);
            i += 1;
        }
    }
    merged.extend_from_slice(&left[i..]);
    merged.extend_from_slice(&right[j..]);
    Ok(merged)
}

fn n_argument(argument: i64) -> Result<i64, i64> {
    match value::as_integer(argument) {
        Some(n) => Ok(n),
        None => Err(type_error(&format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(argument)
        ))),
    }
}

fn nlargest_nsmallest(args: &[i64], largest: bool) -> i64 {
    let Some(&n_arg) = args.first() else {
        return type_error("missing required argument 'n'");
    };
    let Some(&iterable) = args.get(1) else {
        return type_error("missing required argument 'iterable'");
    };
    let n = match n_argument(n_arg) {
        Ok(n) => n,
        Err(error) => return error,
    };
    if n <= 0 {
        return value::list_new(Vec::new());
    }
    let n = n as usize;
    let key = args.get(2).copied().unwrap_or(NONE);
    if let Err(error) = callable_key(key) {
        return error;
    }
    let items_value = match collect_items(iterable) {
        Ok(items) => items,
        Err(error) => return error,
    };
    let items = match value::as_list(items_value) {
        Some(items) => items.to_vec(),
        None => Vec::new(),
    };
    if items.is_empty() {
        return value::list_new(Vec::new());
    }
    let keys_value = match decorate_keys(&items, key) {
        Ok(keys) => keys,
        Err(error) => return error,
    };
    let keys = match value::as_list(keys_value) {
        Some(keys) => keys.to_vec(),
        None => Vec::new(),
    };
    let mut entries: Vec<Entry> = items
        .iter()
        .enumerate()
        .map(|(order, &item)| Entry {
            key: keys.get(order).copied().unwrap_or(item),
            order: order as i64,
            item,
        })
        .collect();
    let kind = if largest {
        HeapKind::Min
    } else {
        HeapKind::Max
    };
    if n < entries.len() {
        let mut bounded = entries[..n].to_vec();
        if let Err(error) = heapify(&mut bounded, kind) {
            return error;
        }
        for &candidate in &entries[n..] {
            let replace = match heap_less(&bounded[0], &candidate, kind) {
                Ok(replace) => replace,
                Err(error) => return error,
            };
            if replace {
                if let Err(error) = heap_replace(&mut bounded, candidate, kind) {
                    return error;
                }
            }
        }
        entries = bounded;
    }
    let sorted = match merge_sort(entries, largest) {
        Ok(sorted) => sorted,
        Err(error) => return error,
    };
    value::list_new(sorted.iter().map(|entry| entry.item).collect())
}

/// Dispatches `module.function(...)` calls for `heapq` and `_heapq`.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "heappush" | "heappop" | "heapify" | "heappushpop" | "heapreplace" => {
            heap_operation(function, args)
        }
        "nlargest" => nlargest_nsmallest(args, true),
        "nsmallest" => nlargest_nsmallest(args, false),
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module 'heapq' has no attribute '{function}'"),
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

    fn numbers(tagged: i64) -> Vec<i64> {
        value::as_list(tagged)
            .expect("list")
            .iter()
            .map(|&item| value::as_integer(item).expect("integer"))
            .collect()
    }

    fn list(values: &[i64]) -> i64 {
        value::list_new(values.to_vec())
    }

    fn neg() -> i64 {
        value::native_function_new("operator", "neg", 1)
    }

    fn call_heap(name: &str, args: &[i64]) -> i64 {
        call(name, args)
    }

    #[test]
    fn push_and_pop_keep_min_order() {
        exc::init();
        let heap = list(&[]);
        for value in [5, 1, 3, 2, 4] {
            assert_eq!(call_heap("heappush", &[heap, int(value)]), NONE);
            assert!(!exc::pending());
        }
        let mut popped = Vec::new();
        for _ in 0..5 {
            popped.push(value::as_integer(call_heap("heappop", &[heap])).expect("int"));
        }
        assert_eq!(popped, vec![1, 2, 3, 4, 5]);
        assert!(numbers(heap).is_empty());
        assert!(!exc::pending());
    }

    #[test]
    fn pop_empty_raises_index_error() {
        exc::init();
        let empty = list(&[]);
        assert_eq!(call_heap("heappop", &[empty]), NONE);
        assert_eq!(exc::pending_tag(), INDEX_ERROR as i64);
        exc::clear();
        assert_eq!(call_heap("heapreplace", &[empty, int(1)]), NONE);
        assert_eq!(exc::pending_tag(), INDEX_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn heapify_sorts_with_duplicates() {
        exc::init();
        let heap = list(&[int(3), int(1), int(2), int(1), int(3)]);
        assert_eq!(call_heap("heapify", &[heap]), NONE);
        let mut order = Vec::new();
        while !numbers(heap).is_empty() {
            order.push(value::as_integer(call_heap("heappop", &[heap])).expect("int"));
        }
        assert_eq!(order, vec![1, 1, 2, 3, 3]);
        assert!(!exc::pending());
    }

    #[test]
    fn pushpop_and_replace() {
        exc::init();
        let heap = list(&[int(1), int(3), int(5)]);
        assert_eq!(call_heap("heapify", &[heap]), NONE);
        // Smaller than the root: returned unchanged, heap untouched.
        assert_eq!(
            value::as_integer(call_heap("heappushpop", &[heap, int(0)])),
            Some(0)
        );
        assert_eq!(numbers(heap), vec![1, 3, 5]);
        // Larger than the root: replaces it and returns the old minimum.
        assert_eq!(
            value::as_integer(call_heap("heappushpop", &[heap, int(2)])),
            Some(1)
        );
        assert_eq!(numbers(heap), vec![2, 3, 5]);
        assert_eq!(
            value::as_integer(call_heap("heapreplace", &[heap, int(4)])),
            Some(2)
        );
        assert_eq!(numbers(heap), vec![3, 4, 5]);
        assert!(!exc::pending());
    }

    #[test]
    fn nlargest_and_nsmallest() {
        exc::init();
        let values = list(&[int(5), int(1), int(4), int(1), int(3)]);
        assert_eq!(
            numbers(call_heap("nlargest", &[int(3), values])),
            vec![5, 4, 3]
        );
        assert_eq!(
            numbers(call_heap("nsmallest", &[int(3), values])),
            vec![1, 1, 3]
        );
        // n beyond the input, and non-positive n.
        assert_eq!(
            numbers(call_heap("nlargest", &[int(99), values])),
            vec![5, 4, 3, 1, 1]
        );
        assert!(numbers(call_heap("nsmallest", &[int(0), values])).is_empty());
        assert!(numbers(call_heap("nlargest", &[int(-2), values])).is_empty());
        assert!(!exc::pending());
    }

    #[test]
    fn nlargest_nsmallest_custom_key() {
        exc::init();
        // key=neg reverses the order: largest by key is the smallest value.
        let values = list(&[int(1), int(2), int(3), int(4)]);
        assert_eq!(
            numbers(call_heap("nlargest", &[int(2), values, neg()])),
            vec![1, 2]
        );
        assert_eq!(
            numbers(call_heap("nsmallest", &[int(2), values, neg()])),
            vec![4, 3]
        );
        // Empty input yields an empty list for every n.
        let empty = list(&[]);
        assert!(numbers(call_heap("nlargest", &[int(2), empty])).is_empty());
        assert!(numbers(call_heap("nsmallest", &[int(1), empty])).is_empty());
        assert!(numbers(call_heap("nsmallest", &[int(3), empty])).is_empty());
        assert!(!exc::pending());
    }

    #[test]
    fn heapify_mutates_in_place_and_accepts_iterables() {
        exc::init();
        let heap = list(&[int(4), int(2), int(3)]);
        assert_eq!(call_heap("heapify", &[heap]), NONE);
        assert_eq!(value::as_integer(value::as_list(heap).unwrap()[0]), Some(2));
        // nlargest accepts any iterable, not only lists.
        let tuple = value::tuple_new(vec![int(7), int(8), int(9)]);
        assert_eq!(numbers(call_heap("nlargest", &[int(2), tuple])), vec![9, 8]);
        assert!(!exc::pending());
    }

    #[test]
    fn errors_do_not_panic() {
        exc::init();
        assert_eq!(call_heap("heappush", &[int(1), int(2)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        assert_eq!(call_heap("heapify", &[int(1)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        let values = list(&[int(1), int(2)]);
        assert_eq!(call_heap("nlargest", &[int(1), values, int(3)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        assert_eq!(
            call_heap("nlargest", &[value::str_from_bytes(b"x"), values]),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        assert_eq!(call_heap("merge", &[values]), NONE);
        assert_eq!(exc::pending_tag(), exc::ATTRIBUTE_ERROR as i64);
        exc::clear();
    }
}
