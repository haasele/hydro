//! List/dict/tuple construction, indexing, unpacking and structural equality
//! (ARCHITECTURE §3.2).
//!
//! Every entry point is total: malformed pointers or values yield a pending
//! exception (or an empty container for a `NULL` literal buffer), never a
//! panic.
//!
//! Containers are not internally synchronised. Once `_thread` has started a
//! second thread (shared heap, no GIL), mutating the same list/dict/set from
//! several threads requires a user lock (`_thread.allocate_lock`); the
//! runtime only keeps its own registry and heap metadata mutex-protected.

use std::collections::HashMap;

use crate::exc::{self, INDEX_ERROR, KEY_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value::{self, Num, NONE};

// Literals

/// Copies `argc` tagged values from `argv`. A `NULL` buffer with `argc == 0`
/// is the empty list; a `NULL` buffer with items present yields an empty list
/// defensively instead of dereferencing it.
pub(crate) fn list_new(argc: usize, argv: *const i64) -> i64 {
    value::list_new(copy_values(argc, argv))
}

/// Copies `argc` tagged values from `argv` into a fresh tuple. A `NULL`
/// buffer yields the empty tuple defensively.
pub(crate) fn tuple_new(argc: usize, argv: *const i64) -> i64 {
    value::tuple_new(copy_values(argc, argv))
}

fn copy_values(argc: usize, argv: *const i64) -> Vec<i64> {
    if argc == 0 || argv.is_null() || argc > isize::MAX as usize / std::mem::size_of::<i64>() {
        return Vec::new();
    }
    // SAFETY: ABI contract: `argv` points at `argc` readable values; length checked above.
    let items = unsafe { std::slice::from_raw_parts(argv, argc) };
    items.to_vec()
}

/// Builds a dict from parallel key/value buffers. Duplicate keys keep their
/// first position and the last value. Unhashable keys raise `TypeError` and
/// produce no object (`None`).
pub(crate) fn dict_new(argc: usize, keys: *const i64, values: *const i64) -> i64 {
    if argc == 0 || keys.is_null() || values.is_null() {
        return value::dict_new(Vec::new());
    }
    if argc > isize::MAX as usize / std::mem::size_of::<i64>() {
        return value::dict_new(Vec::new());
    }
    // SAFETY: ABI contract: both buffers hold `argc` readable values; length checked above.
    let (keys, values) = unsafe {
        (
            std::slice::from_raw_parts(keys, argc),
            std::slice::from_raw_parts(values, argc),
        )
    };
    let mut pairs = DictData::new(Vec::new());
    for (key, val) in keys.iter().zip(values.iter()) {
        if let Some(name) = unhashable_name(*key) {
            return exc::raise_with_message(TYPE_ERROR, &unhashable_message(name));
        }
        // No barrier: the dict does not exist yet; it is allocated below with
        // the complete pair list and is born grey while marking is active.
        dict_insert(&mut pairs, *key, *val);
    }
    value::dict_new_data(pairs)
}

/// `set` literal: dedupes `argc` tagged items while preserving first
/// occurrence. Every item must be hashable; unhashable ones raise `TypeError`.
pub(crate) fn set_new(argc: usize, argv: *const i64) -> i64 {
    build_set(copy_values(argc, argv))
}

/// `set(iterable)`: copies elements from a list, tuple, set or dict (keys),
/// or the characters of a string. Anything else raises `TypeError`.
pub(crate) fn set_from(iterable: i64) -> i64 {
    if value::as_str(iterable).is_some() {
        // The set is allocated first and held in a tagged local so the
        // collector can trace the per-character strings pushed into it.
        let result = value::set_new(Vec::new());
        let text = value::as_str(iterable).unwrap_or("").to_string();
        for ch in text.chars() {
            let item = value::str_alloc(ch.to_string());
            insert_set_item(result, item);
        }
        return result;
    }
    let supported = value::as_list(iterable).is_some()
        || value::as_tuple(iterable).is_some()
        || value::as_set(iterable).is_some()
        || value::as_dict(iterable).is_some();
    if !supported {
        return type_error(format!(
            "'{}' object is not iterable",
            value::type_name(iterable)
        ));
    }
    if value::as_list(iterable).is_some() {
        return set_from_sequence(iterable);
    }
    if value::as_tuple(iterable).is_some() {
        return set_from_sequence(iterable);
    }
    if value::as_set(iterable).is_some() {
        return set_from_sequence(iterable);
    }
    // Dict: the elements are its keys.
    if let Some(pairs) = value::as_dict(iterable) {
        if let Some(err) = hashability_error(&pairs.iter().map(|(key, _)| *key).collect::<Vec<_>>())
        {
            return err;
        }
    }
    let result = value::set_new(Vec::new());
    if let Some(pairs) = value::as_dict(iterable) {
        for (key, _) in pairs {
            insert_set_item(result, *key);
        }
    }
    result
}

/// Copies a list/tuple/set input into a fresh set. The source value stays a
/// root across the allocation, so its elements cannot be collected while the
/// result object is being created.
fn set_from_sequence(iterable: i64) -> i64 {
    let source: Vec<i64> = if let Some(items) = value::as_list(iterable) {
        items.to_vec()
    } else if let Some(items) = value::as_tuple(iterable) {
        items.to_vec()
    } else {
        value::as_set(iterable).unwrap_or(&[]).to_vec()
    };
    if let Some(err) = hashability_error(&source) {
        return err;
    }
    // Allocate before reading the source again; `iterable` is used below, so
    // the collector sees it as a live root during this allocation.
    let result = value::set_new(Vec::new());
    if let Some(items) = value::as_list(iterable) {
        for &item in items {
            insert_set_item(result, item);
        }
    } else if let Some(items) = value::as_tuple(iterable) {
        for &item in items {
            insert_set_item(result, item);
        }
    } else if let Some(items) = value::as_set(iterable) {
        for &item in items {
            insert_set_item(result, item);
        }
    }
    result
}

/// Returns the pending-exception value for the first unhashable element.
fn hashability_error(items: &[i64]) -> Option<i64> {
    items.iter().find_map(|&item| {
        unhashable_name(item)
            .map(|name| exc::raise_with_message(TYPE_ERROR, &unhashable_message(name)))
    })
}

/// Validates hashability of every item, then creates a set with the first
/// occurrence of each item. The result is allocated before items are inserted
/// so the collector traces them during the incremental build.
fn build_set(items: Vec<i64>) -> i64 {
    if let Some(err) = hashability_error(&items) {
        return err;
    }
    let result = value::set_new(Vec::new());
    for item in items {
        insert_set_item(result, item);
    }
    result
}

/// Inserts `item` into `set` unless Python equality says it is present.
pub(crate) fn insert_set_item(set: i64, item: i64) {
    if let Some(mut items) = value::set_items_mut(set) {
        if set_find(&items, item).is_none() {
            items.push(item);
            crate::heap::write_barrier_edge(set, item);
        }
    }
}

/// First position of `target` under Python equality.
pub(crate) fn set_find(items: &[i64], target: i64) -> Option<usize> {
    items.iter().position(|&item| equals(item, target))
}

/// Membership under Python equality.
pub(crate) fn set_contains(items: &[i64], target: i64) -> bool {
    set_find(items, target).is_some()
}

/// Length of a value that can be unpacked without consuming an iterator.
///
/// Strings count Unicode scalar values, matching `iter(str)` and `str` index
/// access.
fn sequence_len(value: i64) -> Option<usize> {
    if let Some(items) = value::as_list(value) {
        Some(items.len())
    } else if let Some(items) = value::as_tuple(value) {
        Some(items.len())
    } else if let Some(bytes) = value::as_bytes(value) {
        Some(bytes.len())
    } else if let Some(view) = value::as_memoryview(value) {
        Some(view.len())
    } else {
        value::as_str(value).map(|text| text.chars().count())
    }
}

/// Raises the length-mismatch error for `expected` when `len` differs.
fn check_unpack_len(len: usize, expected: usize) -> i64 {
    if len > expected {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!("too many values to unpack (expected {})", expected),
        );
    }
    if len < expected {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!(
                "not enough values to unpack (expected {}, got {})",
                expected, len
            ),
        );
    }
    NONE
}

/// `a, b, ... = value`: checks that `value` is a list, tuple, string or bytes
/// with exactly `expected` elements. On success returns `None`; mismatches
/// raise `ValueError` and non-sequences `TypeError`.
pub(crate) fn unpack(value: i64, expected: usize) -> i64 {
    let Some(len) = sequence_len(value) else {
        return type_error(format!(
            "cannot unpack non-iterable {} object",
            value::type_name(value)
        ));
    };
    check_unpack_len(len, expected)
}

/// Sequence-producing variant of [`unpack`] for unpacking assignments.
///
/// Lists, tuples, strings and bytes are returned unchanged after the length
/// check. Every other iterable (including an existing iterator or generator)
/// is consumed into a fresh list, which the caller keeps rooted while it reads
/// the elements. Raises `ValueError` on a length mismatch and `TypeError` for
/// non-iterables, returning `None` in both cases.
pub(crate) fn unpack_seq(value: i64, expected: usize) -> i64 {
    if let Some(len) = sequence_len(value) {
        check_unpack_len(len, expected);
        return if exc::pending() { NONE } else { value };
    }

    let iterator = crate::iter::iter_of(value);
    if exc::pending() {
        // `iter()` reports its generic not-iterable error; unpacking keeps its
        // own wording.
        exc::clear();
        return type_error(format!(
            "cannot unpack non-iterable {} object",
            value::type_name(value)
        ));
    }
    let result = value::list_new(Vec::new());
    let mut count = 0usize;
    loop {
        let mut item = NONE;
        let status = crate::iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        if count == expected {
            return exc::raise_with_message(
                VALUE_ERROR,
                &format!("too many values to unpack (expected {})", expected),
            );
        }
        if let Some(mut items) = value::list_items_mut(result) {
            items.push(item);
            crate::heap::write_barrier_edge(result, item);
        }
        count += 1;
    }
    if count < expected {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!(
                "not enough values to unpack (expected {}, got {})",
                expected, count
            ),
        );
    }
    result
}

/// Starred unpacking-assignment source (`a, *rest, b = value`).
///
/// `before` targets bind elements from the front and `after` from the back; the
/// starred target receives a fresh list of everything between them. Returns a
/// fresh list with `before + 1 + after` elements whose slot `before` is that
/// middle list, so codegen reads it exactly like the exact-length case.
///
/// Lists and tuples are read in place (the caller keeps the source rooted);
/// every other iterable is consumed into a list first, including strings,
/// bytes, ranges and generators. Fewer than `before + after` elements raises
/// CPython's "not enough values" `ValueError`; a non-iterable raises the
/// unpack-specific `TypeError`.
pub(crate) fn unpack_star(value: i64, before: usize, after: usize) -> i64 {
    let minimum = match before.checked_add(after) {
        Some(minimum) => minimum,
        None => {
            return exc::raise_with_message(TYPE_ERROR, "unpacking has too many targets");
        }
    };
    let items_list = if value::as_list(value).is_some() || value::as_tuple(value).is_some() {
        value
    } else {
        let iterator = crate::iter::iter_of(value);
        if exc::pending() {
            // `iter()` reports its generic not-iterable error; unpacking keeps
            // its own wording.
            exc::clear();
            return type_error(format!(
                "cannot unpack non-iterable {} object",
                value::type_name(value)
            ));
        }
        let collected = value::list_new(Vec::new());
        loop {
            let mut item = NONE;
            let status = crate::iter::iter_next(iterator, &mut item);
            if exc::pending() {
                return NONE;
            }
            if status == 0 {
                break;
            }
            if let Some(mut items) = value::list_items_mut(collected) {
                items.push(item);
                crate::heap::write_barrier_edge(collected, item);
            }
        }
        collected
    };
    let items: Vec<i64> = if let Some(items) = value::as_list(items_list) {
        items.to_vec()
    } else if let Some(items) = value::as_tuple(items_list) {
        items.to_vec()
    } else {
        Vec::new()
    };
    if items.len() < minimum {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!(
                "not enough values to unpack (expected at least {}, got {})",
                minimum,
                items.len()
            ),
        );
    }
    let tail = items.len() - after;
    // The result list is allocated first so it roots the values as they are
    // appended; the middle list is then built from the still-rooted source.
    let result = value::list_new(Vec::with_capacity(minimum + 1));
    let middle = value::list_new(items[before..tail].to_vec());
    let values = items[..before]
        .iter()
        .chain(std::iter::once(&middle))
        .chain(items[tail..].iter());
    for &item in values {
        if let Some(mut items) = value::list_items_mut(result) {
            items.push(item);
            crate::heap::write_barrier_edge(result, item);
        }
    }
    result
}

// Dict payload: insertion-ordered entries plus a hash index

/// Insertion-ordered `(key, value)` entries with an index over the key hashes.
///
/// `entries` is the observable order (iteration, `repr`, `keys()`/`values()`).
/// `index` maps [`key_hash`] to the positions of the entries sharing that
/// hash, so a lookup only compares keys inside one bucket. Positions are
/// stable across overwrites; a removal shifts the later entries and therefore
/// rebuilds the index.
pub(crate) struct DictData {
    entries: Vec<(i64, i64)>,
    index: HashMap<u64, Vec<u32>>,
}

impl DictData {
    /// Builds the payload from `entries`, deduplicating duplicate keys with
    /// Python equality (first position, last value) so the index is
    /// consistent with the observable order.
    pub(crate) fn new(entries: Vec<(i64, i64)>) -> DictData {
        let mut data = DictData {
            entries: Vec::with_capacity(entries.len()),
            index: HashMap::with_capacity(entries.len()),
        };
        for (key, value) in entries {
            data.set(key, value);
        }
        data
    }

    pub(crate) fn entries(&self) -> &[(i64, i64)] {
        &self.entries
    }

    /// Position of the entry whose key equals `key` under Python equality.
    pub(crate) fn find(&self, key: i64) -> Option<usize> {
        let bucket = self.index.get(&key_hash(key))?;
        for &position in bucket {
            let position = position as usize;
            if equals(self.entries[position].0, key) {
                return Some(position);
            }
        }
        None
    }

    /// Overwrites the value of an existing key (keeping its position) or
    /// appends a new entry. Returns `true` when a new entry was appended.
    pub(crate) fn set(&mut self, key: i64, value: i64) -> bool {
        let hash = key_hash(key);
        if let Some(bucket) = self.index.get(&hash) {
            for &position in bucket {
                let entry = &mut self.entries[position as usize];
                if equals(entry.0, key) {
                    entry.1 = value;
                    return false;
                }
            }
        }
        self.push_entry(hash, key, value);
        true
    }

    /// Appends `key` (known to be absent) and indexes its position.
    fn push_entry(&mut self, hash: u64, key: i64, value: i64) {
        let position = self.entries.len() as u32;
        self.entries.push((key, value));
        self.index.entry(hash).or_default().push(position);
    }

    /// Removes the entry for `key`, rebuilding the index because the removal
    /// shifts every later position.
    pub(crate) fn remove(&mut self, key: i64) -> Option<(i64, i64)> {
        let position = self.find(key)?;
        let pair = self.entries.remove(position);
        self.reindex();
        Some(pair)
    }

    /// Drops every entry and clears the index.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.index.clear();
    }

    /// Appends every pair with dict semantics (duplicates overwrite in place).
    pub(crate) fn extend_from_slice(&mut self, pairs: &[(i64, i64)]) {
        for &(key, value) in pairs {
            self.set(key, value);
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn reindex(&mut self) {
        self.index.clear();
        self.index.reserve(self.entries.len());
        for (position, (key, _)) in self.entries.iter().enumerate() {
            self.index
                .entry(key_hash(*key))
                .or_default()
                .push(position as u32);
        }
    }
}

/// Read side of a `(key, value)` table: the hashed [`DictData`] and the plain
/// pair vectors used by `defaultdict` and structural equality.
pub(crate) trait PairLookup {
    /// Position of the entry equal to `key`.
    fn find_pair(&self, key: i64) -> Option<usize>;
}

/// Mutation side of a `(key, value)` table.
pub(crate) trait PairMut: PairLookup {
    /// Overwrites an existing key in place or appends a new entry. Returns
    /// `true` when a new entry was appended.
    fn insert_pair(&mut self, key: i64, value: i64) -> bool;
}

impl PairLookup for [(i64, i64)] {
    fn find_pair(&self, key: i64) -> Option<usize> {
        self.iter().position(|(stored, _)| equals(*stored, key))
    }
}

impl PairLookup for Vec<(i64, i64)> {
    fn find_pair(&self, key: i64) -> Option<usize> {
        self.iter().position(|(stored, _)| equals(*stored, key))
    }
}

impl PairMut for Vec<(i64, i64)> {
    fn insert_pair(&mut self, key: i64, value: i64) -> bool {
        match self.find_pair(key) {
            Some(position) => {
                self[position].1 = value;
                false
            }
            None => {
                self.push((key, value));
                true
            }
        }
    }
}

impl PairLookup for DictData {
    fn find_pair(&self, key: i64) -> Option<usize> {
        self.find(key)
    }
}

impl PairMut for DictData {
    fn insert_pair(&mut self, key: i64, value: i64) -> bool {
        self.set(key, value)
    }
}

impl<T: PairLookup + 'static> PairLookup for value::ContainerGuard<T> {
    fn find_pair(&self, key: i64) -> Option<usize> {
        (**self).find_pair(key)
    }
}

impl<T: PairMut + 'static> PairMut for value::ContainerGuard<T> {
    fn insert_pair(&mut self, key: i64, value: i64) -> bool {
        (**self).insert_pair(key, value)
    }
}

// Key hashing

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Domain seeds keep values of different kinds in different bucket families.
const HASH_NUMERIC: u64 = FNV_OFFSET ^ 0x01;
const HASH_STR: u64 = FNV_OFFSET ^ 0x02;
const HASH_BYTES: u64 = FNV_OFFSET ^ 0x03;
const HASH_TUPLE: u64 = FNV_OFFSET ^ 0x04;
const HASH_RANGE: u64 = FNV_OFFSET ^ 0x05;
const HASH_NONE: u64 = FNV_OFFSET ^ 0x06;
const HASH_IDENTITY: u64 = FNV_OFFSET ^ 0x07;

#[inline]
fn fnv_mix(hash: u64, word: u64) -> u64 {
    (hash ^ word).wrapping_mul(FNV_PRIME)
}

fn fnv_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash = fnv_mix(hash, byte as u64);
    }
    hash
}

/// Hash of a dict key, consistent with [`equals`]: equal values always hash
/// equally, so every Python-equal int/float/bool lands in the same bucket.
///
/// Unhashable values never reach a dict, but they are still hashed by identity
/// defensively.
pub(crate) fn key_hash(v: i64) -> u64 {
    if let Some(hash) = numeric_hash(v) {
        return hash;
    }
    if let Some(s) = value::as_str(v) {
        return fnv_bytes(HASH_STR, s.as_bytes());
    }
    if let Some(bytes) = value::as_bytes(v) {
        return fnv_bytes(HASH_BYTES, bytes);
    }
    if let Some(items) = value::as_tuple(v) {
        let mut hash = fnv_mix(HASH_TUPLE, items.len() as u64);
        for &item in items {
            hash = fnv_mix(hash, key_hash(item));
        }
        return hash;
    }
    if let Some((start, stop, step)) = value::as_range(v) {
        return fnv_mix(
            fnv_mix(fnv_mix(HASH_RANGE, start as u64), stop as u64),
            step as u64,
        );
    }
    if v == NONE {
        return HASH_NONE;
    }
    fnv_mix(HASH_IDENTITY, v as u64)
}

/// Numeric keys hash by the `f64` bit pattern: `1`, `1.0` and `True` share a
/// bucket, and `-0.0` is canonicalised to `0.0`. Two equal ints always encode
/// the same bits; ints that round to the same `f64` merely collide. A heap
/// `BigInt` uses its (rounded) `f64` view for the same reason: values equal to
/// a float must land in that float's bucket, and equal integers always round
/// identically.
fn numeric_hash(v: i64) -> Option<u64> {
    let value = if let Some(i) = value::as_integer(v) {
        i as f64
    } else if let Some(n) = value::as_bigint(v) {
        value::bigint_to_f64(n).unwrap_or_else(|| {
            if n.sign() == num_bigint::Sign::Minus {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }
        })
    } else {
        value::as_float(v)?
    };
    let bits = if value == 0.0 {
        0.0f64.to_bits()
    } else {
        value.to_bits()
    };
    Some(fnv_mix(HASH_NUMERIC, bits))
}

/// Type name of an unhashable object (`list`, `dict`, `iterator`, a bytes-like
/// mutable/view value, or a tuple containing one), if any. Tuples are hashable
/// exactly when every element is.
pub(crate) fn unhashable_name(v: i64) -> Option<&'static str> {
    if value::as_list(v).is_some() {
        Some("list")
    } else if value::as_dict(v).is_some() || value::as_defaultdict(v).is_some() {
        Some("dict")
    } else if value::as_set(v).is_some() {
        Some("set")
    } else if value::as_iter(v).is_some() {
        Some("iterator")
    } else if value::as_bytearray(v).is_some() {
        Some("bytearray")
    } else if value::as_memoryview(v).is_some() {
        Some("memoryview")
    } else if let Some(view) = value::as_dict_view(v) {
        Some(view.kind.type_name())
    } else if let Some(items) = value::as_tuple(v) {
        items.iter().find_map(|item| unhashable_name(*item))
    } else {
        None
    }
}

pub(crate) fn unhashable_message(name: &str) -> String {
    format!("unhashable type: '{}'", name)
}

/// Number of entries a live `dict` view observes.
pub(crate) fn dict_view_len(view: &value::DictViewState) -> usize {
    value::as_dict(view.dict).map_or(0, |pairs| pairs.len())
}

/// Whether `item` is present in a live `dict` view.
///
/// Keys compare by key equality, values by value equality and items by a
/// `(key, value)` pair with both halves equal.
pub(crate) fn dict_view_contains(view: &value::DictViewState, item: i64) -> bool {
    let Some(pairs) = value::as_dict(view.dict) else {
        return false;
    };
    match view.kind {
        value::DictViewKind::Keys => pairs.iter().any(|(key, _)| equals(*key, item)),
        value::DictViewKind::Values => pairs.iter().any(|(_, value)| equals(*value, item)),
        value::DictViewKind::Items => {
            let pair = value::as_tuple(item)
                .or_else(|| value::as_list(item))
                .filter(|pair| pair.len() == 2);
            let Some(pair) = pair else {
                return false;
            };
            pairs
                .iter()
                .any(|(key, val)| equals(*key, pair[0]) && equals(*val, pair[1]))
        }
    }
}

/// Elements a `dict` view contributes to set equality and set operators.
///
/// `dict_values` has no set-like element view, so it returns `None`. The
/// `dict_items` tuples are accumulated in a heap list so they stay reachable
/// while later tuples are allocated.
pub(crate) fn dict_view_elements(view: &value::DictViewState) -> Option<Vec<i64>> {
    let pairs: Vec<(i64, i64)> = value::as_dict(view.dict)?.to_vec();
    match view.kind {
        value::DictViewKind::Keys => Some(pairs.into_iter().map(|(key, _)| key).collect()),
        value::DictViewKind::Values => None,
        value::DictViewKind::Items => {
            let accumulator = value::list_new(Vec::new());
            for (key, val) in pairs {
                let item = value::tuple_new(vec![key, val]);
                list_append(accumulator, item);
            }
            Some(
                value::as_list(accumulator)
                    .map(|items| items.to_vec())
                    .unwrap_or_default(),
            )
        }
    }
}

/// Set-like equality: same size and every element of `left` in `right`.
pub(crate) fn set_like_equal(left: &[i64], right: &[i64]) -> bool {
    left.len() == right.len() && left.iter().all(|item| set_contains(right, *item))
}

/// Set operators supported on sets and `dict_keys`/`dict_items` views.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SetOp {
    And,
    Or,
    Sub,
    Xor,
}

/// Elements of a value that participates in set operations.
fn set_like_elements(v: i64) -> Option<Vec<i64>> {
    if let Some(items) = value::as_set(v) {
        return Some(items.to_vec());
    }
    value::as_dict_view(v).and_then(dict_view_elements)
}

/// Whether `v` participates in set operations (`set` or a keys/items view).
pub(crate) fn is_set_like(v: i64) -> bool {
    value::as_set(v).is_some()
        || value::as_dict_view(v).is_some_and(|view| view.kind != value::DictViewKind::Values)
}

/// `a & b`, `a | b`, `a - b` and `a ^ b` over sets and `dict` views.
///
/// Returns `None` when either operand is not set-like, so the caller keeps its
/// existing numeric/type-error behaviour. The result is always a fresh `set`.
pub(crate) fn set_operation(op: SetOp, a: i64, b: i64) -> Option<i64> {
    let left = set_like_elements(a)?;
    let right = set_like_elements(b)?;
    let result = value::set_new(Vec::new());
    match op {
        SetOp::And => {
            for &item in &left {
                if set_contains(&right, item) {
                    insert_set_item(result, item);
                }
            }
        }
        SetOp::Or => {
            for &item in left.iter().chain(right.iter()) {
                insert_set_item(result, item);
            }
        }
        SetOp::Sub => {
            for &item in &left {
                if !set_contains(&right, item) {
                    insert_set_item(result, item);
                }
            }
        }
        SetOp::Xor => {
            for &item in &left {
                if !set_contains(&right, item) {
                    insert_set_item(result, item);
                }
            }
            for &item in &right {
                if !set_contains(&left, item) {
                    insert_set_item(result, item);
                }
            }
        }
    }
    Some(result)
}

// Dict operations over a pair table

/// Position of `key` in a pair table (`O(1)` for [`DictData`], a scan for the
/// plain vectors used by `defaultdict`).
pub(crate) fn dict_find<P: PairLookup + ?Sized>(pairs: &P, key: i64) -> Option<usize> {
    pairs.find_pair(key)
}

/// Inserts or overwrites `key` while preserving its first insertion position
/// (the same rule as the dict constructor).
pub(crate) fn dict_insert(pairs: &mut DictData, key: i64, new_value: i64) {
    pairs.set(key, new_value);
}

/// [`dict_insert`] plus the insertion barriers for the edges it creates.
///
/// Must be called while `pairs` is borrowed through [`value::dict_pairs_mut`],
/// so the barriers run under the graph lock. A new key publishes barriers for
/// both the key (new edge) and the value; overwriting an existing key only
/// publishes the new value, because the key edge already exists. The key is
/// looked up once.
pub(crate) fn dict_insert_barrier<P: PairMut + ?Sized>(
    dict: i64,
    pairs: &mut P,
    key: i64,
    new_value: i64,
) {
    if pairs.insert_pair(key, new_value) {
        // New key: both the key and the value are new edges.
        crate::heap::write_barrier_edge(dict, key);
        crate::heap::write_barrier_edge(dict, new_value);
    } else {
        // Overwrite: the key edge already exists.
        crate::heap::write_barrier_edge(dict, new_value);
    }
}

// Comprehension builders

/// Appends `item` to a list comprehension result. Non-lists raise `TypeError`
/// and return `None`.
pub(crate) fn list_append(list: i64, item: i64) -> i64 {
    match value::list_items_mut(list) {
        Some(mut items) => {
            items.push(item);
            crate::heap::write_barrier_edge(list, item);
            NONE
        }
        None => type_error(format!(
            "descriptor 'append' for 'list' objects doesn't apply to a '{}' object",
            value::type_name(list)
        )),
    }
}

/// Adds `item` to a set comprehension result unless it is already present.
/// Unhashable items raise `TypeError`; other receiver kinds raise `TypeError`
/// too. Returns `None`.
pub(crate) fn set_add(set: i64, item: i64) -> i64 {
    if value::as_set(set).is_none() {
        return type_error(format!(
            "descriptor 'add' for 'set' objects doesn't apply to a '{}' object",
            value::type_name(set)
        ));
    }
    if let Some(name) = unhashable_name(item) {
        return type_error(unhashable_message(name));
    }
    insert_set_item(set, item);
    NONE
}

/// Sets `dict[key] = new_value` for a dict comprehension result: an existing
/// key keeps its first position and takes the last value. Unhashable keys and
/// non-dict receivers raise `TypeError`. Returns `None`.
pub(crate) fn dict_set(dict: i64, key: i64, new_value: i64) -> i64 {
    if value::as_dict(dict).is_none() {
        return type_error(format!(
            "'{}' object does not support item assignment",
            value::type_name(dict)
        ));
    }
    if let Some(name) = unhashable_name(key) {
        return type_error(unhashable_message(name));
    }
    if let Some(mut pairs) = value::dict_pairs_mut(dict) {
        dict_insert_barrier(dict, &mut pairs, key, new_value);
    }
    NONE
}

// Indexing

/// `object[index]` for lists, strings, tuples, dicts, bytes-likes and views.
pub(crate) fn subscript(obj: i64, index: i64) -> i64 {
    // Checked before `as_bytes`, which also accepts a `bytearray`.
    if let Some(bytes) = value::as_bytearray(obj) {
        let Some(raw) = value::as_integer(index) else {
            return type_error(format!(
                "bytearray indices must be integers or slices, not {}",
                value::type_name(index)
            ));
        };
        let len = bytes.len();
        return match normalize_index(raw, len) {
            Some(pos) => crate::ops::int_result(bytes[pos] as i64),
            None => exc::raise_with_message(INDEX_ERROR, "bytearray index out of range"),
        };
    }
    if let Some(view) = value::as_memoryview(obj) {
        if view.released {
            return exc::raise_with_message(VALUE_ERROR, value::MEMORYVIEW_RELEASED);
        }
        let Some(raw) = value::as_integer(index) else {
            return type_error("memoryview: invalid indices for memoryview".to_string());
        };
        let Some(pos) = normalize_index(raw, view.len()) else {
            return exc::raise_with_message(INDEX_ERROR, value::MEMORYVIEW_OUT_OF_RANGE);
        };
        return match value::memoryview_get(view, pos) {
            Ok(item) => item,
            Err(message) => memoryview_error(message),
        };
    }
    if let Some(items) = value::as_list(obj) {
        let Some(raw) = value::as_integer(index) else {
            return type_error(format!(
                "list indices must be integers or slices, not {}",
                value::type_name(index)
            ));
        };
        return match normalize_index(raw, items.len()) {
            Some(pos) => items[pos],
            None => exc::raise_with_message(INDEX_ERROR, "list index out of range"),
        };
    }
    if value::as_defaultdict(obj).is_some() {
        return crate::modules::collections::getitem(obj, index);
    }
    if let Some(s) = value::as_str(obj) {
        let Some(raw) = value::as_integer(index) else {
            return type_error(format!(
                "string indices must be integers, not {}",
                value::type_name(index)
            ));
        };
        let len = s.chars().count();
        return match normalize_index(raw, len) {
            Some(pos) => match s.chars().nth(pos) {
                Some(ch) => value::str_alloc(ch.to_string()),
                None => exc::raise_with_message(INDEX_ERROR, "string index out of range"),
            },
            None => exc::raise_with_message(INDEX_ERROR, "string index out of range"),
        };
    }
    if let Some(bytes) = value::as_bytes(obj) {
        let Some(raw) = value::as_integer(index) else {
            return type_error(format!(
                "byte indices must be integers or slices, not {}",
                value::type_name(index)
            ));
        };
        return match normalize_index(raw, bytes.len()) {
            // Indexing bytes yields an integer in `0..=255`.
            Some(pos) => crate::ops::int_result(bytes[pos] as i64),
            None => exc::raise_with_message(INDEX_ERROR, "index out of range"),
        };
    }
    if let Some(items) = value::as_tuple(obj) {
        let Some(raw) = value::as_integer(index) else {
            return type_error(format!(
                "tuple indices must be integers or slices, not {}",
                value::type_name(index)
            ));
        };
        return match normalize_index(raw, items.len()) {
            Some(pos) => items[pos],
            None => exc::raise_with_message(INDEX_ERROR, "tuple index out of range"),
        };
    }
    if let Some(dict) = value::as_dict_data(obj) {
        if let Some(name) = unhashable_name(index) {
            return type_error(unhashable_message(name));
        }
        return match dict.find(index) {
            Some(position) => dict.entries()[position].1,
            // Python's `KeyError` message is the repr of the missing key.
            None => exc::raise_with_message(KEY_ERROR, &value::repr(index)),
        };
    }
    if value::as_instance(obj).is_some() {
        let entry = value::as_instance(obj)
            .and_then(|state| value::class_lookup(state.class, "__getitem__"));
        if matches!(entry, Some(value::ClassEntry::Method(_, _))) {
            let args = [index];
            return crate::core::methods::call(
                obj,
                b"__getitem__".as_ptr(),
                11,
                args.len(),
                args.as_ptr(),
            );
        }
    }
    // Generic aliases (`typing.Tuple[int]`, `list[str]`) and ordinary classes
    // are treated as subscriptable: Hydro has no generic substitution, so the
    // class value itself is the alias. User code that evaluates a typing alias
    // at runtime (for example a module-level `CellSpan = Tuple[int, int]`)
    // gets a usable value instead of `TypeError`/`None[...]`.
    if value::as_class(obj).is_some() {
        // `types.UnionType[A, B]` is the only class subscript with real
        // semantics: it builds a native union value (PEP 604).
        if let Some(union) = crate::modules::types::union_subscript(obj, index) {
            return union;
        }
        return obj;
    }
    type_error(format!(
        "'{}' object is not subscriptable",
        value::type_name(obj)
    ))
}

/// `object[index] = value` for lists, dicts, bytearrays and writable views.
/// Always returns `None`; errors are reported through the pending exception.
pub(crate) fn subscript_set(obj: i64, index: i64, new_value: i64) -> i64 {
    // Checked before `as_bytes`, which also accepts a `bytearray`.
    if let Some(bytes) = value::as_bytearray(obj) {
        let Some(raw) = value::as_integer(index) else {
            return type_error(format!(
                "bytearray indices must be integers or slices, not {}",
                value::type_name(index)
            ));
        };
        let Some(byte) = byte_value(new_value) else {
            return if value::as_integer(new_value).is_some() {
                exc::raise_with_message(VALUE_ERROR, "byte must be in range(0, 256)")
            } else {
                type_error(format!(
                    "'{}' object cannot be interpreted as an integer",
                    value::type_name(new_value)
                ))
            };
        };
        let position = match normalize_index(raw, bytes.len()) {
            Some(position) => position,
            None => return exc::raise_with_message(INDEX_ERROR, "bytearray index out of range"),
        };
        // Re-borrow after the allocation-free validation above.
        return match value::bytearray_bytes_mut(obj) {
            Some(bytes) => {
                bytes[position] = byte;
                NONE
            }
            None => type_error("bytearray assignment on a non-bytearray".to_string()),
        };
    }
    if let Some(view) = value::as_memoryview(obj) {
        if view.released {
            return exc::raise_with_message(VALUE_ERROR, value::MEMORYVIEW_RELEASED);
        }
        let Some(raw) = value::as_integer(index) else {
            return type_error("memoryview: invalid indices for memoryview".to_string());
        };
        let Some(position) = normalize_index(raw, view.len()) else {
            return exc::raise_with_message(INDEX_ERROR, value::MEMORYVIEW_OUT_OF_RANGE);
        };
        return match value::memoryview_set(view, position, new_value) {
            Ok(()) => NONE,
            Err(message) => memoryview_error(message),
        };
    }
    if let Some(mut items) = value::list_items_mut(obj) {
        let Some(raw) = value::as_integer(index) else {
            // Drop the graph lock before allocating the exception object.
            drop(items);
            return type_error(format!(
                "list indices must be integers or slices, not {}",
                value::type_name(index)
            ));
        };
        return match normalize_index(raw, items.len()) {
            Some(pos) => {
                items[pos] = new_value;
                crate::heap::write_barrier_edge(obj, new_value);
                NONE
            }
            None => {
                drop(items);
                exc::raise_with_message(INDEX_ERROR, "list assignment index out of range")
            }
        };
    }
    if let Some(mut pairs) = value::dict_pairs_mut(obj) {
        if let Some(name) = unhashable_name(index) {
            drop(pairs);
            return type_error(unhashable_message(name));
        }
        dict_insert_barrier(obj, &mut pairs, index, new_value);
        return NONE;
    }
    if value::as_defaultdict(obj).is_some() {
        return crate::modules::collections::setitem(obj, index, new_value);
    }
    if value::as_instance(obj).is_some() {
        let entry = value::as_instance(obj)
            .and_then(|state| value::class_lookup(state.class, "__setitem__"));
        if matches!(entry, Some(value::ClassEntry::Method(_, _))) {
            let args = [index, new_value];
            return crate::core::methods::call(
                obj,
                b"__setitem__".as_ptr(),
                11,
                args.len(),
                args.as_ptr(),
            );
        }
    }
    type_error(format!(
        "'{}' object does not support item assignment",
        value::type_name(obj)
    ))
}

/// `del object[index]` for lists and dicts. Always returns `None`; errors are
/// reported through the pending exception.
///
/// A list index out of range raises `IndexError`, a missing dict key
/// `KeyError` (with the key's `repr`) and every other receiver `TypeError`.
pub(crate) fn delete_subscript(obj: i64, index: i64) -> i64 {
    // Deleting a bytearray element shrinks the sequence; memoryview elements
    // are fixed-size slots, so deletion is rejected like Python does.
    if let Some(bytes) = value::as_bytearray(obj) {
        let Some(raw) = value::as_integer(index) else {
            return type_error(format!(
                "bytearray indices must be integers or slices, not {}",
                value::type_name(index)
            ));
        };
        let Some(position) = normalize_index(raw, bytes.len()) else {
            return exc::raise_with_message(INDEX_ERROR, "bytearray index out of range");
        };
        return match value::bytearray_bytes_mut(obj) {
            Some(bytes) => {
                bytes.remove(position);
                NONE
            }
            None => type_error("bytearray deletion on a non-bytearray".to_string()),
        };
    }
    if value::as_memoryview(obj).is_some() {
        return type_error("'memoryview' object does not support item deletion".to_string());
    }
    if let Some(mut items) = value::list_items_mut(obj) {
        let Some(raw) = value::as_integer(index) else {
            // Drop the graph lock before allocating the exception object.
            drop(items);
            return type_error(format!(
                "list indices must be integers or slices, not {}",
                value::type_name(index)
            ));
        };
        return match normalize_index(raw, items.len()) {
            Some(pos) => {
                items.remove(pos);
                NONE
            }
            None => {
                drop(items);
                exc::raise_with_message(INDEX_ERROR, "list assignment index out of range")
            }
        };
    }
    if let Some(mut dict) = value::dict_pairs_mut(obj) {
        if let Some(name) = unhashable_name(index) {
            drop(dict);
            return type_error(unhashable_message(name));
        }
        return match dict.remove(index) {
            Some(_) => NONE,
            // Python's `KeyError` message is the repr of the missing key.
            None => {
                drop(dict);
                exc::raise_with_message(KEY_ERROR, &value::repr(index))
            }
        };
    }
    if value::as_defaultdict(obj).is_some() {
        return crate::modules::collections::delitem(obj, index);
    }
    if value::as_instance(obj).is_some() {
        let entry = value::as_instance(obj)
            .and_then(|state| value::class_lookup(state.class, "__delitem__"));
        if matches!(entry, Some(value::ClassEntry::Method(_, _))) {
            let args = [index];
            return crate::core::methods::call(
                obj,
                b"__delitem__".as_ptr(),
                11,
                args.len(),
                args.as_ptr(),
            );
        }
    }
    type_error(format!(
        "'{}' object does not support item deletion",
        value::type_name(obj)
    ))
}

/// Maps a Python index (possibly negative) into `0..len`.
pub(crate) fn normalize_index(index: i64, len: usize) -> Option<usize> {
    let len = len as i64;
    let index = if index < 0 {
        index.checked_add(len)?
    } else {
        index
    };
    if index < 0 || index >= len {
        None
    } else {
        Some(index as usize)
    }
}

fn type_error(message: String) -> i64 {
    exc::raise_with_message(TYPE_ERROR, &message)
}

/// Decodes a Python value as one byte (`0..=255`); `None` when it is not an
/// in-range integer.
pub(crate) fn byte_value(v: i64) -> Option<u8> {
    value::as_integer(v).and_then(|n| u8::try_from(n).ok())
}

/// Maps a `MemoryViewState` error message to the tagged Python exception.
///
/// A released view is a `ValueError`, a read-only write is a `TypeError`
/// ("cannot modify read-only memory") and an out-of-range index is an
/// `IndexError`, matching CPython.
pub(crate) fn memoryview_error(message: &'static str) -> i64 {
    if message == value::MEMORYVIEW_RELEASED {
        exc::raise_with_message(VALUE_ERROR, message)
    } else if message == value::MEMORYVIEW_OUT_OF_RANGE {
        exc::raise_with_message(INDEX_ERROR, message)
    } else {
        exc::raise_with_message(TYPE_ERROR, message)
    }
}

/// Length of a bytes-like exporter, `0` when it is not bytes-like.
fn as_bytes_len(buffer: i64) -> usize {
    value::as_bytes(buffer).map_or(0, |bytes| bytes.len())
}

// Slicing

enum SliceError {
    /// A bound was neither an integer nor `None`.
    Type,
    /// `step == 0`.
    Step,
}

/// `object[start:stop:step]` for lists, strings and tuples.
///
/// `start`, `stop` and `step` are tagged values; [`NONE`] means the argument
/// was omitted. Python's `slice.indices` rules are reproduced exactly
/// (clamping and negative bounds); the result is always a new object.
pub(crate) fn slice(obj: i64, start: i64, stop: i64, step: i64) -> i64 {
    if let Some(items) = value::as_list(obj) {
        let len = items.len();
        let (start, stop, step) = match slice_indices(start, stop, step, len) {
            Ok(bounds) => bounds,
            Err(err) => return slice_error(err),
        };
        let mut out = Vec::new();
        for pos in slice_positions(start, stop, step) {
            out.push(items[pos]);
        }
        return value::list_new(out);
    }
    if let Some(items) = value::as_tuple(obj) {
        let len = items.len();
        let (start, stop, step) = match slice_indices(start, stop, step, len) {
            Ok(bounds) => bounds,
            Err(err) => return slice_error(err),
        };
        let mut out = Vec::new();
        for pos in slice_positions(start, stop, step) {
            out.push(items[pos]);
        }
        return value::tuple_new(out);
    }
    if let Some(s) = value::as_str(obj) {
        let chars: Vec<char> = s.chars().collect();
        let (start, stop, step) = match slice_indices(start, stop, step, chars.len()) {
            Ok(bounds) => bounds,
            Err(err) => return slice_error(err),
        };
        let mut out = String::with_capacity(chars.len());
        for pos in slice_positions(start, stop, step) {
            out.push(chars[pos]);
        }
        return value::str_alloc(out);
    }
    // Checked before `as_bytes`, which also accepts a `bytearray`.
    if let Some(bytes) = value::as_bytearray(obj) {
        let (start, stop, step) = match slice_indices(start, stop, step, bytes.len()) {
            Ok(bounds) => bounds,
            Err(err) => return slice_error(err),
        };
        let mut out = Vec::new();
        for pos in slice_positions(start, stop, step) {
            out.push(bytes[pos]);
        }
        return value::bytearray_new(out);
    }
    if let Some(view) = value::as_memoryview(obj) {
        if view.released {
            return exc::raise_with_message(VALUE_ERROR, value::MEMORYVIEW_RELEASED);
        }
        let (start, stop, step) = match slice_indices(start, stop, step, view.len()) {
            Ok(bounds) => bounds,
            Err(err) => return slice_error(err),
        };
        let positions = slice_positions(start, stop, step);
        if positions.is_empty() {
            // No visible element, so the exact origin is unobservable; keep a
            // valid window instead of a negative offset.
            return value::memoryview_with_window(
                view.buffer,
                view.offset.min(as_bytes_len(view.buffer)),
                0,
                1,
                view.format,
            );
        }
        let first = positions[0] as i128;
        let offset = view.offset as i128 + first * view.stride as i128;
        let stride = view.stride as i128 * step as i128;
        let Ok(offset) = usize::try_from(offset) else {
            return exc::raise_with_message(INDEX_ERROR, value::MEMORYVIEW_OUT_OF_RANGE);
        };
        let Ok(stride) = isize::try_from(stride) else {
            return exc::raise_with_message(INDEX_ERROR, value::MEMORYVIEW_OUT_OF_RANGE);
        };
        return value::memoryview_with_window(
            view.buffer,
            offset,
            positions.len(),
            stride,
            view.format,
        );
    }
    if let Some(bytes) = value::as_bytes(obj) {
        let (start, stop, step) = match slice_indices(start, stop, step, bytes.len()) {
            Ok(bounds) => bounds,
            Err(err) => return slice_error(err),
        };
        let mut out = Vec::new();
        for pos in slice_positions(start, stop, step) {
            out.push(bytes[pos]);
        }
        return value::bytes_new(out);
    }
    if value::as_dict(obj).is_some() {
        // Python has no slice keys, and a `slice` is unhashable.
        return type_error(unhashable_message("slice"));
    }
    type_error(format!(
        "'{}' object is not subscriptable",
        value::type_name(obj)
    ))
}

/// `object[start:stop:step] = value` for lists.
///
/// `start`, `stop` and `step` are tagged values; [`NONE`] means the argument
/// was omitted. Bounds follow Python's `slice.indices` exactly. A `step` of 1
/// splices the materialised right-hand side into the list (any length); any
/// other step replaces the visited positions and requires exactly as many
/// values as there are positions, otherwise `ValueError`.
///
/// Strings, bytes and tuples are immutable (`TypeError`); a dict reports the
/// unhashable `slice` key like a slice read. The right-hand side may be any
/// iterable and is materialised before the target is changed, so
/// `xs[:] = xs` works. Every inserted edge publishes a GC insertion barrier
/// while the list is locked. Returns `None`; errors are reported through the
/// pending exception.
pub(crate) fn slice_set(obj: i64, start: i64, stop: i64, step: i64, new_value: i64) -> i64 {
    if value::as_bytearray(obj).is_some() || value::as_memoryview(obj).is_some() {
        if let Some(view) = value::as_memoryview(obj) {
            if view.released {
                let _ = exc::raise_with_message(VALUE_ERROR, value::MEMORYVIEW_RELEASED);
                return NONE;
            }
        }
        if let Err(err) = validate_slice_args(start, stop, step) {
            let _ = slice_error(err);
            return NONE;
        }
        let Some(values) = slice_assignment_values(new_value) else {
            return NONE;
        };
        let mut bytes = Vec::with_capacity(values.len());
        for item in values {
            let Some(byte) = byte_value(item) else {
                let _ = if value::as_integer(item).is_some() {
                    exc::raise_with_message(VALUE_ERROR, "byte must be in range(0, 256)")
                } else {
                    type_error(format!(
                        "'{}' object cannot be interpreted as an integer",
                        value::type_name(item)
                    ))
                };
                return NONE;
            };
            bytes.push(byte);
        }
        if let Some(view) = value::as_memoryview(obj) {
            let len = view.len();
            let Ok((start, stop, step)) = slice_indices(start, stop, step, len) else {
                return NONE;
            };
            let positions: Vec<usize> = if step == 1 {
                let stop = stop.max(start);
                (start as usize..(stop as usize).min(len)).collect()
            } else {
                slice_positions(start, stop, step)
            };
            if positions.len() != bytes.len() {
                let _ = exc::raise_with_message(
                    VALUE_ERROR,
                    "memoryview assignment: lvalue and rvalue have different structures",
                );
                return NONE;
            }
            for (&position, &byte) in positions.iter().zip(bytes.iter()) {
                if let Err(message) = value::memoryview_set_byte(view, position, byte) {
                    let _ = memoryview_error(message);
                    return NONE;
                }
            }
            return NONE;
        }
        let Some(len) = value::as_bytearray(obj).map(|bytes| bytes.len()) else {
            return NONE;
        };
        let Ok((start, stop, step)) = slice_indices(start, stop, step, len) else {
            return NONE;
        };
        let Some(target) = value::bytearray_bytes_mut(obj) else {
            return NONE;
        };
        if step == 1 {
            let stop = stop.max(start);
            let low = (start as usize).min(target.len());
            let high = (stop as usize).min(target.len()).max(low);
            target.splice(low..high, bytes);
            return NONE;
        }
        let positions = slice_positions(start, stop, step);
        if positions.len() != bytes.len() {
            let _ = exc::raise_with_message(
                VALUE_ERROR,
                &format!(
                    "attempt to assign bytes of size {} to extended slice of size {}",
                    bytes.len(),
                    positions.len()
                ),
            );
            return NONE;
        }
        for (position, &byte) in positions.into_iter().zip(bytes.iter()) {
            if position >= target.len() {
                break;
            }
            target[position] = byte;
        }
        return NONE;
    }
    if value::as_list(obj).is_none() {
        if value::as_dict(obj).is_some() || value::as_defaultdict(obj).is_some() {
            let _ = type_error(unhashable_message("slice"));
        } else {
            let _ = type_error(format!(
                "'{}' object does not support item assignment",
                value::type_name(obj)
            ));
        }
        return NONE;
    }
    // CPython validates the slice arguments (integers or `None`, `step != 0`)
    // before it materialises the right-hand side, but clamps the bounds only
    // afterwards: the right-hand side may run user code that mutates the
    // list. The index computations below therefore use the length observed
    // after the materialisation.
    if let Err(err) = validate_slice_args(start, stop, step) {
        let _ = slice_error(err);
        return NONE;
    }
    let Some(values) = slice_assignment_values(new_value) else {
        return NONE;
    };
    let Some(len) = value::as_list(obj).map(|items| items.len()) else {
        return NONE;
    };
    let Ok((start, stop, step)) = slice_indices(start, stop, step, len) else {
        return NONE;
    };

    let Some(mut items) = value::list_items_mut(obj) else {
        return NONE;
    };
    if step == 1 {
        // Python raises `stop` to `start`, so a "backwards" range such as
        // `xs[2:1] = [...]` inserts at index 2. Clamp again in case another
        // thread changed the list between the length read and the lock.
        let stop = stop.max(start);
        let length = items.len();
        let low = (start as usize).min(length);
        let high = (stop as usize).min(length).max(low);
        items.splice(low..high, values.iter().copied());
        for &value in &values {
            crate::heap::write_barrier_edge(obj, value);
        }
        return NONE;
    }
    let positions = slice_positions(start, stop, step);
    if positions.len() != values.len() {
        // Drop the graph lock before allocating the exception object.
        drop(items);
        let _ = exc::raise_with_message(
            VALUE_ERROR,
            &format!(
                "attempt to assign sequence of size {} to extended slice of size {}",
                values.len(),
                positions.len()
            ),
        );
        return NONE;
    }
    for (position, &value) in positions.into_iter().zip(values.iter()) {
        // Defensive: another thread may have shrunk the list despite the
        // documented user-lock requirement. Positions ascend (or descend)
        // monotonically, so stopping at the first out-of-range one is safe.
        if position >= items.len() {
            break;
        }
        items[position] = value;
        crate::heap::write_barrier_edge(obj, value);
    }
    NONE
}

/// `del object[start:stop:step]` for lists.
///
/// `start`, `stop` and `step` are tagged values; [`NONE`] means the argument
/// was omitted. Bounds follow Python's `slice.indices` exactly, so
/// `del xs[:]` clears the list and negative steps delete the visited
/// positions. Strings, bytes and tuples are immutable (`TypeError`); a dict
/// reports the unhashable `slice` key like a slice read. Every remaining
/// element keeps its object; dropped edges are reclaimed by the next
/// collection. Returns `None`; errors are reported through the pending
/// exception.
pub(crate) fn delete_slice(obj: i64, start: i64, stop: i64, step: i64) -> i64 {
    // Deleting a bytearray slice shrinks the sequence; a view has fixed slots.
    if value::as_bytearray(obj).is_some() {
        if let Err(err) = validate_slice_args(start, stop, step) {
            let _ = slice_error(err);
            return NONE;
        }
        let Some(len) = value::as_bytearray(obj).map(|bytes| bytes.len()) else {
            return NONE;
        };
        let Ok((start, stop, step)) = slice_indices(start, stop, step, len) else {
            return NONE;
        };
        let Some(bytes) = value::bytearray_bytes_mut(obj) else {
            return NONE;
        };
        if step == 1 {
            let stop = stop.max(start);
            let low = (start as usize).min(bytes.len());
            let high = (stop as usize).min(bytes.len()).max(low);
            bytes.drain(low..high);
            return NONE;
        }
        let mut positions = slice_positions(start, stop, step);
        // Descending removal keeps the earlier positions valid.
        positions.sort_unstable_by(|a, b| b.cmp(a));
        for position in positions {
            if position < bytes.len() {
                bytes.remove(position);
            }
        }
        return NONE;
    }
    if let Some(view) = value::as_memoryview(obj) {
        if view.released {
            let _ = exc::raise_with_message(VALUE_ERROR, value::MEMORYVIEW_RELEASED);
        } else {
            let _ = type_error("'memoryview' object does not support item deletion".to_string());
        }
        return NONE;
    }
    if value::as_list(obj).is_none() {
        if value::as_dict(obj).is_some() || value::as_defaultdict(obj).is_some() {
            let _ = type_error(unhashable_message("slice"));
        } else {
            let _ = type_error(format!(
                "'{}' object does not support item deletion",
                value::type_name(obj)
            ));
        }
        return NONE;
    }
    if let Err(err) = validate_slice_args(start, stop, step) {
        let _ = slice_error(err);
        return NONE;
    }
    // Clamp against the length observed after any user code ran.
    let Some(len) = value::as_list(obj).map(|items| items.len()) else {
        return NONE;
    };
    let Ok((start, stop, step)) = slice_indices(start, stop, step, len) else {
        return NONE;
    };
    let Some(mut items) = value::list_items_mut(obj) else {
        return NONE;
    };
    if step == 1 {
        // In-range indices are monotonic; clamp again in case another thread
        // changed the list between the length read and the lock.
        let length = items.len();
        let low = (start.max(0) as usize).min(length);
        let high = (stop.max(0) as usize).min(length).max(low);
        items.drain(low..high);
        return NONE;
    }
    let positions = slice_positions(start, stop, step);
    let length = items.len();
    let mut dropped = vec![false; length];
    for position in positions {
        if position < length {
            dropped[position] = true;
        }
    }
    let mut position = 0usize;
    items.retain(|_| {
        let keep = !dropped[position];
        position += 1;
        keep
    });
    NONE
}

/// Validates the raw slice arguments like CPython's `PySlice_Unpack`: every
/// bound is an integer or `None`, and `step != 0`.
fn validate_slice_args(start: i64, stop: i64, step: i64) -> Result<(), SliceError> {
    let _ = slice_bound(start).map_err(|()| SliceError::Type)?;
    let _ = slice_bound(stop).map_err(|()| SliceError::Type)?;
    match slice_bound(step).map_err(|()| SliceError::Type)? {
        None => Ok(()),
        Some(0) => Err(SliceError::Step),
        Some(_) => Ok(()),
    }
}

/// Materialises the right-hand side of a list slice assignment into a
/// snapshot, so assigning a list to one of its own slices works. Lists and
/// tuples are copied directly; every other iterable (strings, bytes, dicts,
/// sets, ranges, iterators and generators) is consumed through the runtime
/// iterator. A non-iterable raises CPython's `TypeError` ("must assign
/// iterable to extended slice", also for a step of 1); `None` is returned with
/// a pending exception in that case.
fn slice_assignment_values(new_value: i64) -> Option<Vec<i64>> {
    if let Some(items) = value::as_list(new_value) {
        return Some(items.to_vec());
    }
    if let Some(items) = value::as_tuple(new_value) {
        return Some(items.to_vec());
    }
    let iterator = crate::iter::iter_of(new_value);
    if exc::pending() {
        // `iter()` reports its generic not-iterable error; slice assignment
        // keeps CPython's wording.
        exc::clear();
        let _ = type_error("must assign iterable to extended slice".to_string());
        return None;
    }
    // The fresh list keeps every element reachable while the iterator runs
    // user code (a generator may allocate or collect).
    let collected = value::list_new(Vec::new());
    loop {
        let mut item = NONE;
        let status = crate::iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return None;
        }
        if status == 0 {
            break;
        }
        if let Some(mut items) = value::list_items_mut(collected) {
            items.push(item);
            crate::heap::write_barrier_edge(collected, item);
        }
    }
    Some(
        value::as_list(collected)
            .map(|items| items.to_vec())
            .unwrap_or_default(),
    )
}

fn slice_error(err: SliceError) -> i64 {
    match err {
        SliceError::Type => exc::raise_with_message(
            TYPE_ERROR,
            "slice indices must be integers or None or have an __index__ method",
        ),
        SliceError::Step => exc::raise_with_message(VALUE_ERROR, "slice step cannot be zero"),
    }
}

/// Decodes one slice argument: `Ok(None)` for omitted (`None`), `Ok(Some(n))`
/// for an integer (booleans included), `Err(())` for anything else.
fn slice_bound(v: i64) -> Result<Option<i64>, ()> {
    if v == NONE {
        return Ok(None);
    }
    match value::as_integer(v) {
        Some(n) => Ok(Some(n)),
        None => Err(()),
    }
}

/// `slice.indices(len)` (CPython `PySlice_GetIndicesEx`) as `(start, stop,
/// step)`.
fn slice_indices(
    start: i64,
    stop: i64,
    step: i64,
    len: usize,
) -> Result<(i64, i64, i64), SliceError> {
    let len = len as i64;
    let step = match slice_bound(step).map_err(|()| SliceError::Type)? {
        None => 1,
        Some(0) => return Err(SliceError::Step),
        Some(step) => step,
    };
    let (lower, upper) = if step > 0 {
        (0, len)
    } else {
        (-1, len.wrapping_sub(1))
    };

    let clamp = |value: i64| {
        if value < 0 {
            let adjusted = value.saturating_add(len);
            if adjusted < lower {
                lower
            } else {
                adjusted
            }
        } else if value > upper {
            upper
        } else {
            value
        }
    };

    let start = match slice_bound(start).map_err(|()| SliceError::Type)? {
        None => {
            if step < 0 {
                upper
            } else {
                lower
            }
        }
        Some(value) => clamp(value),
    };
    let stop = match slice_bound(stop).map_err(|()| SliceError::Type)? {
        None => {
            if step < 0 {
                lower
            } else {
                upper
            }
        }
        Some(value) => clamp(value),
    };
    Ok((start, stop, step))
}

/// Indices visited by `for i in range(start, stop, step)` with Python's
/// exclusive stop. `start` and `stop` are already clamped into
/// `-1..=len`, so every yielded index is a valid element position.
fn slice_positions(start: i64, stop: i64, step: i64) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut index = start;
    if step > 0 {
        while index < stop {
            if index < 0 {
                break;
            }
            positions.push(index as usize);
            index = match index.checked_add(step) {
                Some(next) => next,
                None => break,
            };
        }
    } else {
        while index > stop {
            if index < 0 {
                break;
            }
            positions.push(index as usize);
            index = match index.checked_add(step) {
                Some(next) => next,
                None => break,
            };
        }
    }
    positions
}

// Structural equality

/// Python `==` for the value subset (numeric cross-type equality, string
/// contents, deep list equality and order-insensitive dict equality).
pub(crate) fn equals(a: i64, b: i64) -> bool {
    equals_depth(a, b, 0)
}

const MAX_EQ_DEPTH: usize = 64;

fn equals_depth(a: i64, b: i64, depth: usize) -> bool {
    match (value::num_of(a), value::num_of(b)) {
        (Some(x), Some(y)) => return num_eq(x, y),
        (Some(_), None) | (None, Some(_)) => {
            // An integer outside `i64` yields no `Num`; fall through so the
            // exact BigInt comparison below can still run.
            if value::as_bigint(a).is_none() && value::as_bigint(b).is_none() {
                return false;
            }
        }
        (None, None) => {}
    }
    // Integers too large for `num_of`: compare exact values, and compare
    // exactly against a float as well (no lossy `f64` conversion).
    if let (Some(x), Some(y)) = (value::as_bigint(a), value::as_bigint(b)) {
        return x == y;
    }
    if let Some(x) = value::as_bigint(a) {
        return match value::as_float(b) {
            Some(f) => value::bigint_cmp_f64(x, f) == Some(std::cmp::Ordering::Equal),
            None => false,
        };
    }
    if let Some(y) = value::as_bigint(b) {
        return match value::as_float(a) {
            Some(f) => value::bigint_cmp_f64(y, f) == Some(std::cmp::Ordering::Equal),
            None => false,
        };
    }
    if let (Some(s), Some(t)) = (value::as_str(a), value::as_str(b)) {
        return s == t;
    }
    if let (Some(s), Some(t)) = (value::as_bytes(a), value::as_bytes(b)) {
        return s == t;
    }
    // `memoryview` compares by format, shape and content; a view also compares
    // equal to the bytes it exposes.
    if let (Some(x), Some(y)) = (value::as_memoryview(a), value::as_memoryview(b)) {
        if x.format != y.format || x.len() != y.len() {
            return false;
        }
        return match (value::memoryview_bytes(x), value::memoryview_bytes(y)) {
            (Ok(xs), Ok(ys)) => xs == ys,
            _ => false,
        };
    }
    if let (Some(view), Some(bytes)) = (value::as_memoryview(a), value::as_bytes(b)) {
        return value::memoryview_bytes(view)
            .map(|seen| seen == bytes)
            .unwrap_or(false);
    }
    if let (Some(view), Some(bytes)) = (value::as_memoryview(b), value::as_bytes(a)) {
        return value::memoryview_bytes(view)
            .map(|seen| seen == bytes)
            .unwrap_or(false);
    }
    // `dict_keys`/`dict_items` compare as sets; `dict_values` has no content
    // equality (only the identity shortcut below).
    if let (Some(x), Some(y)) = (value::as_dict_view(a), value::as_dict_view(b)) {
        if x.kind != y.kind {
            return false;
        }
        if x.kind == value::DictViewKind::Values {
            return false;
        }
        let (Some(xs), Some(ys)) = (dict_view_elements(x), dict_view_elements(y)) else {
            return false;
        };
        return set_like_equal(&xs, &ys);
    }
    if let (Some(view), Some(items)) = (value::as_dict_view(a), value::as_set(b)) {
        return dict_view_elements(view)
            .map(|elements| set_like_equal(&elements, items))
            .unwrap_or(false);
    }
    if let (Some(view), Some(items)) = (value::as_dict_view(b), value::as_set(a)) {
        return dict_view_elements(view)
            .map(|elements| set_like_equal(items, &elements))
            .unwrap_or(false);
    }
    // Identity shortcut: equal tagged values are the same immediate or the
    // same heap object. This also terminates comparisons of self-referential
    // containers (`a == a`).
    if a == b {
        return true;
    }
    if depth >= MAX_EQ_DEPTH {
        // Self-referential containers: treat unconverged comparison as unequal
        // instead of overflowing the stack.
        return false;
    }
    if let (Some(xs), Some(ys)) = (value::as_list(a), value::as_list(b)) {
        return xs.len() == ys.len()
            && xs
                .iter()
                .zip(ys.iter())
                .all(|(x, y)| equals_depth(*x, *y, depth + 1));
    }
    if let (Some(xs), Some(ys)) = (value::as_tuple(a), value::as_tuple(b)) {
        return xs.len() == ys.len()
            && xs
                .iter()
                .zip(ys.iter())
                .all(|(x, y)| equals_depth(*x, *y, depth + 1));
    }
    if let (Some(xs), Some(ys)) = (value::as_set(a), value::as_set(b)) {
        // Sets compare equal when they have the same size and every element
        // is present, regardless of insertion order.
        return xs.len() == ys.len()
            && xs
                .iter()
                .all(|x| ys.iter().any(|y| equals_depth(*x, *y, depth + 1)));
    }
    if let (Some(xs), Some(ys)) = (value::as_dict(a), value::as_dict(b)) {
        return xs.len() == ys.len()
            && xs.iter().all(|(key, val)| {
                dict_find(ys, *key)
                    .map(|pos| equals_depth(*val, ys[pos].1, depth + 1))
                    .unwrap_or(false)
            });
    }
    // A `defaultdict` compares by content like a dict, including against a
    // plain dict.
    let left_pairs = value::as_dict(a).or_else(|| value::as_defaultdict(a).map(|(pairs, _)| pairs));
    let right_pairs =
        value::as_dict(b).or_else(|| value::as_defaultdict(b).map(|(pairs, _)| pairs));
    if let (Some(xs), Some(ys)) = (left_pairs, right_pairs) {
        return xs.len() == ys.len()
            && xs.iter().all(|(key, val)| {
                dict_find(ys, *key)
                    .map(|pos| equals_depth(*val, ys[pos].1, depth + 1))
                    .unwrap_or(false)
            });
    }
    false
}

fn num_eq(a: Num, b: Num) -> bool {
    match (a, b) {
        (Num::Int(x), Num::Int(y)) => x == y,
        (Num::Float(x), Num::Float(y)) => x == y,
        (Num::Int(x), Num::Float(y)) => int_float_eq(x, y),
        (Num::Float(x), Num::Int(y)) => int_float_eq(y, x),
    }
}

/// Exact `int == float`; NaN and infinities never equal an integer.
fn int_float_eq(i: i64, f: f64) -> bool {
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    if !(-TWO_POW_63..TWO_POW_63).contains(&f) || f.fract() != 0.0 {
        return false;
    }
    // `f` is integral and inside the i64 range, so the cast is exact.
    f as i64 == i
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

    fn list(items: &[i64]) -> i64 {
        value::list_new(items.to_vec())
    }

    fn tuple(items: &[i64]) -> i64 {
        value::tuple_new(items.to_vec())
    }

    #[test]
    fn list_literal_and_length() {
        exc::init();
        let items = [int(1), string("a")];
        let v = list_new(items.len(), items.as_ptr());
        assert_eq!(value::as_list(v), Some(&items[..]));
        assert_eq!(value::as_integer(crate::builtins::len(v)), Some(2));
        assert!(value::truthy(v));
        assert!(!value::truthy(list_new(0, std::ptr::null())));
        assert!(!exc::pending());
    }

    #[test]
    fn dict_literal_dedupes_keeping_first_position() {
        exc::init();
        let keys = [string("a"), int(2), string("a")];
        let values = [int(1), int(3), int(4)];
        let v = dict_new(keys.len(), keys.as_ptr(), values.as_ptr());
        let pairs = value::as_dict(v).expect("dict");
        assert_eq!(pairs.len(), 2);
        assert!(equals(pairs[0].0, string("a")));
        assert_eq!(pairs[0].1, int(4));
        assert_eq!(pairs[1], (int(2), int(3)));
        assert_eq!(value::as_integer(crate::builtins::len(v)), Some(2));
        assert!(!exc::pending());
    }

    #[test]
    fn dict_rejects_unhashable_keys() {
        exc::init();
        let keys = [list(&[int(1)])];
        let values = [int(1)];
        assert_eq!(dict_new(1, keys.as_ptr(), values.as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "unhashable type: 'list'"
        );
    }

    #[test]
    fn list_indexing_wraps_and_reports_bounds() {
        exc::init();
        let v = list(&[int(10), int(20), int(30)]);
        assert_eq!(subscript(v, int(0)), int(10));
        assert_eq!(subscript(v, int(-1)), int(30));
        assert_eq!(subscript(v, int(-3)), int(10));
        assert_eq!(subscript(v, value::TRUE), int(20));

        assert_eq!(subscript(v, int(3)), NONE);
        assert_eq!(exc::pending_tag(), INDEX_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "list index out of range"
        );

        exc::init();
        assert_eq!(subscript(v, string("x")), NONE);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "list indices must be integers or slices, not str"
        );
    }

    #[test]
    fn string_indexing_counts_scalars() {
        exc::init();
        let s = string("héllo");
        assert_eq!(value::as_str(subscript(s, int(1))), Some("é"));
        assert_eq!(value::as_str(subscript(s, int(-1))), Some("o"));
        assert_eq!(subscript(s, int(5)), NONE);
        assert_eq!(exc::pending_tag(), INDEX_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "string index out of range"
        );

        exc::init();
        subscript(s, value::float_new(1.0));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "string indices must be integers, not float"
        );
    }

    #[test]
    fn dict_get_and_key_error_repr() {
        exc::init();
        let keys = [string("a"), int(1)];
        let values = [int(1), int(2)];
        let d = dict_new(2, keys.as_ptr(), values.as_ptr());
        assert_eq!(subscript(d, string("a")), int(1));
        assert_eq!(subscript(d, int(1)), int(2));

        assert_eq!(subscript(d, string("missing")), NONE);
        assert_eq!(exc::pending_tag(), KEY_ERROR as i64);
        assert_eq!(value::to_string(exc::pending_value()), "'missing'");

        exc::init();
        assert_eq!(subscript(d, list(&[])), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "unhashable type: 'list'"
        );
    }

    #[test]
    fn subscript_set_updates_and_inserts() {
        exc::init();
        let v = list(&[int(1), int(2)]);
        assert_eq!(subscript_set(v, int(-1), int(9)), NONE);
        assert_eq!(value::as_list(v), Some(&[int(1), int(9)][..]));
        assert!(!exc::pending());

        subscript_set(v, int(2), int(0));
        assert_eq!(exc::pending_tag(), INDEX_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "list assignment index out of range"
        );

        exc::init();
        let d = dict_new(0, std::ptr::null(), std::ptr::null());
        assert_eq!(subscript_set(d, string("k"), int(5)), NONE);
        assert_eq!(subscript(d, string("k")), int(5));
        assert_eq!(subscript_set(d, string("k"), int(6)), NONE);
        assert_eq!(subscript(d, string("k")), int(6));
        assert!(!exc::pending());

        subscript_set(d, list(&[]), int(1));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "unhashable type: 'list'"
        );
    }

    #[test]
    fn assignment_rejects_non_containers() {
        exc::init();
        assert_eq!(subscript_set(string("abc"), int(0), int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'str' object does not support item assignment"
        );

        exc::init();
        assert_eq!(subscript(int(1), int(0)), NONE);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'int' object is not subscriptable"
        );
    }

    #[test]
    fn structural_equality() {
        exc::init();
        assert!(equals(
            list(&[int(1), string("a")]),
            list(&[int(1), string("a")])
        ));
        assert!(!equals(list(&[int(1)]), list(&[int(1), int(2)])));
        assert!(equals(list(&[list(&[int(1)])]), list(&[list(&[int(1)])])));

        let k1 = [string("a"), string("b")];
        let v1 = [int(1), int(2)];
        let d1 = dict_new(2, k1.as_ptr(), v1.as_ptr());
        let k2 = [string("b"), string("a")];
        let v2 = [int(2), int(1)];
        let d2 = dict_new(2, k2.as_ptr(), v2.as_ptr());
        assert!(equals(d1, d2));

        let k3 = [string("b"), string("a")];
        let v3 = [int(2), int(3)];
        let d3 = dict_new(2, k3.as_ptr(), v3.as_ptr());
        assert!(!equals(d1, d3));
        assert!(!equals(d1, list(&[])));
        assert!(!equals(list(&[]), d1));

        // Numeric cross-type equality inside containers.
        assert!(equals(list(&[value::float_new(1.0)]), list(&[int(1)])));
        assert!(!equals(
            list(&[value::float_new(f64::NAN)]),
            list(&[value::float_new(f64::NAN)])
        ));
        assert!(!exc::pending());
    }

    #[test]
    fn tuple_literal_and_indexing() {
        exc::init();
        let items = [int(1), string("a"), int(3)];
        let t = tuple_new(items.len(), items.as_ptr());
        assert_eq!(value::as_tuple(t), Some(&items[..]));
        assert_eq!(value::type_name(t), "tuple");
        assert_eq!(value::to_string(t), "(1, 'a', 3)");
        assert!(!exc::pending());
        assert_eq!(value::to_string(tuple_new(0, std::ptr::null())), "()");

        assert_eq!(subscript(t, int(0)), int(1));
        assert_eq!(subscript(t, int(-1)), int(3));
        assert!(equals(subscript(t, value::TRUE), string("a")));

        exc::init();
        assert_eq!(subscript(t, int(3)), NONE);
        assert_eq!(exc::pending_tag(), INDEX_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "tuple index out of range"
        );

        exc::init();
        subscript(t, string("x"));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "tuple indices must be integers or slices, not str"
        );
    }

    #[test]
    fn tuple_slice_and_immutability() {
        exc::init();
        let t = tuple(&[int(1), int(2), int(3)]);
        let reversed = slice(t, NONE, NONE, int(-1));
        assert_eq!(value::to_string(reversed), "(3, 2, 1)");
        assert_eq!(value::to_string(slice(t, int(1), NONE, NONE)), "(2, 3)");
        assert_eq!(value::to_string(slice(t, int(9), NONE, NONE)), "()");
        // Slices are fresh tuples.
        assert_ne!(t, reversed);
        assert!(!exc::pending());

        subscript_set(t, int(0), int(9));
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'tuple' object does not support item assignment"
        );
    }

    #[test]
    fn unpack_checks_exact_length() {
        exc::init();
        let t = tuple(&[int(1), int(2)]);
        assert_eq!(unpack(t, 2), NONE);
        assert!(!exc::pending());
        assert_eq!(unpack(list(&[int(1)]), 1), NONE);
        assert!(!exc::pending());

        assert_eq!(unpack(t, 1), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "too many values to unpack (expected 1)"
        );

        exc::init();
        assert_eq!(unpack(t, 3), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "not enough values to unpack (expected 3, got 2)"
        );

        exc::init();
        assert_eq!(unpack(int(5), 1), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "cannot unpack non-iterable int object"
        );
    }

    #[test]
    fn tuple_hashability_is_recursive() {
        exc::init();
        assert_eq!(unhashable_name(tuple(&[int(1), string("a")])), None);
        assert_eq!(
            unhashable_name(tuple(&[tuple(&[list(&[int(1)])])])),
            Some("list")
        );
        assert_eq!(
            unhashable_name(tuple(&[dict_new(0, std::ptr::null(), std::ptr::null())])),
            Some("dict")
        );
        assert_eq!(unhashable_name(set_from(list(&[int(1)]))), Some("set"));

        let keys = [tuple(&[int(1), int(2)])];
        let values = [int(7)];
        let d = dict_new(1, keys.as_ptr(), values.as_ptr());
        assert!(!exc::pending());
        assert_eq!(subscript(d, tuple(&[int(1), int(2)])), int(7));
        assert!(!exc::pending());

        exc::init();
        let bad = [tuple(&[int(1), list(&[int(2)])])];
        let values = [int(1)];
        assert_eq!(dict_new(1, bad.as_ptr(), values.as_ptr()), NONE);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "unhashable type: 'list'"
        );
    }

    #[test]
    fn tuple_structural_equality() {
        exc::init();
        assert!(equals(
            tuple(&[int(1), string("a")]),
            tuple(&[int(1), string("a")])
        ));
        assert!(!equals(tuple(&[int(1)]), tuple(&[int(1), int(2)])));
        assert!(equals(
            tuple(&[tuple(&[int(1)])]),
            tuple(&[tuple(&[int(1)])])
        ));
        // Tuples never equal lists.
        assert!(!equals(tuple(&[int(1)]), list(&[int(1)])));
        assert!(!exc::pending());
    }

    #[test]
    fn type_names_for_containers() {
        assert_eq!(value::type_name(list(&[])), "list");
        assert_eq!(
            value::type_name(dict_new(0, std::ptr::null(), std::ptr::null())),
            "dict"
        );
        assert_eq!(value::type_name(value::iter_new(int(1))), "iterator");
    }

    #[test]
    fn set_literal_dedupes_and_preserves_order() {
        exc::init();
        let items = [int(1), int(2), int(2), int(3), int(1)];
        let v = set_new(items.len(), items.as_ptr());
        assert_eq!(
            value::as_set(v),
            Some(&[int(1), int(2), int(3)][..]),
            "first occurrence wins"
        );
        assert_eq!(value::as_integer(crate::builtins::len(v)), Some(3));
        assert!(!exc::pending());

        // Strings compare by value, too.
        let repeated = [string("a"), string("b"), string("a")];
        let v = set_new(repeated.len(), repeated.as_ptr());
        assert_eq!(value::as_set(v), Some(&[repeated[0], repeated[1]][..]));
    }

    #[test]
    fn set_rejects_unhashable_elements() {
        exc::init();
        let bad = [list(&[int(1)])];
        assert_eq!(set_new(1, bad.as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "unhashable type: 'list'"
        );

        exc::init();
        let nested = [tuple(&[int(1), list(&[int(2)])])];
        assert_eq!(set_new(1, nested.as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "unhashable type: 'list'"
        );

        exc::init();
        let inner = set_new(0, std::ptr::null());
        let with_set = [inner];
        assert_eq!(set_new(1, with_set.as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "unhashable type: 'set'"
        );
    }

    #[test]
    fn set_from_sequences_strings_and_dicts() {
        exc::init();
        let source = list(&[int(1), int(2), int(2), int(3)]);
        let s = set_from(source);
        let s_items = value::as_set(s).expect("set");
        assert_eq!(s_items, &[int(1), int(2), int(3)]);
        // Copy semantics: mutating the source must not affect the set.
        if let Some(mut items) = value::list_items_mut(source) {
            items[0] = int(9);
        }
        assert_eq!(value::as_set(s), Some(s_items));

        let text = string("aabb");
        let chars = set_from(text);
        let chars = value::as_set(chars).expect("set");
        assert_eq!(chars.len(), 2);
        assert_eq!(value::as_str(chars[0]), Some("a"));
        assert_eq!(value::as_str(chars[1]), Some("b"));

        let keys = [string("k"), int(1)];
        let values = [int(7), int(8)];
        let d = dict_new(2, keys.as_ptr(), values.as_ptr());
        let from_dict = set_from(d);
        let from_dict = value::as_set(from_dict).expect("set");
        assert_eq!(from_dict.len(), 2);
        assert!(set_contains(from_dict, keys[0]));
        assert!(set_contains(from_dict, int(1)));

        // Copy semantics for a set input.
        let original = set_from(list(&[int(1), int(2)]));
        let copied = set_from(original);
        assert_ne!(original, copied);
        assert_eq!(value::as_set(copied), value::as_set(original));
        assert!(!exc::pending());
    }

    #[test]
    fn set_from_non_iterable_errors() {
        exc::init();
        assert_eq!(set_from(int(3)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'int' object is not iterable"
        );

        exc::init();
        assert_eq!(set_from(NONE), NONE);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'NoneType' object is not iterable"
        );
    }

    #[test]
    fn set_equality_is_order_insensitive() {
        exc::init();
        let a = set_from(list(&[int(1), int(2), int(3)]));
        let b = set_from(list(&[int(3), int(1), int(2)]));
        let c = set_from(list(&[int(1), int(2)]));
        assert!(equals(a, b));
        assert!(!equals(a, c));
        assert!(!equals(a, list(&[int(1), int(2), int(3)])));
        assert!(equals(a, a));
        // Elements compare with Python equality (`1 == True`).
        let with_true = set_from(list(&[value::TRUE]));
        let with_one = set_from(list(&[int(1)]));
        assert!(equals(with_true, with_one));
    }

    #[test]
    fn list_append_helper_pushes_onto_lists() {
        exc::init();
        let xs = list(&[int(1)]);
        assert_eq!(list_append(xs, int(2)), NONE);
        assert_eq!(list_append(xs, string("a")), NONE);
        let items = value::as_list(xs).expect("list");
        assert_eq!(items.len(), 3);
        assert_eq!(&items[..2], &[int(1), int(2)][..]);
        assert_eq!(value::as_str(items[2]), Some("a"));
        assert!(!exc::pending());

        exc::init();
        assert_eq!(list_append(tuple(&[]), int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "descriptor 'append' for 'list' objects doesn't apply to a 'tuple' object"
        );

        exc::init();
        assert_eq!(list_append(int(1), int(2)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    fn set_add_helper_inserts_once_and_checks_hashability() {
        exc::init();
        let s = set_new(0, std::ptr::null());
        assert_eq!(set_add(s, int(1)), NONE);
        assert_eq!(set_add(s, int(2)), NONE);
        assert_eq!(set_add(s, int(1)), NONE, "duplicates are ignored");
        assert_eq!(set_add(s, value::TRUE), NONE, "1 == True is a duplicate");
        assert_eq!(value::as_set(s), Some(&[int(1), int(2)][..]));
        assert!(!exc::pending());

        exc::init();
        assert_eq!(set_add(s, list(&[int(1)])), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "unhashable type: 'list'"
        );

        exc::init();
        assert_eq!(set_add(list(&[]), int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "descriptor 'add' for 'set' objects doesn't apply to a 'list' object"
        );
    }

    #[test]
    fn dict_set_helper_updates_first_position_last_value() {
        exc::init();
        let d = dict_new(0, std::ptr::null(), std::ptr::null());
        assert_eq!(dict_set(d, string("a"), int(1)), NONE);
        assert_eq!(dict_set(d, string("b"), int(2)), NONE);
        assert_eq!(dict_set(d, string("a"), int(3)), NONE);
        let pairs = value::as_dict(d).expect("dict");
        assert_eq!(pairs.len(), 2);
        assert!(equals(pairs[0].0, string("a")));
        assert_eq!(pairs[0].1, int(3));
        assert!(equals(pairs[1].0, string("b")));
        assert_eq!(pairs[1].1, int(2));
        assert!(!exc::pending());

        exc::init();
        assert_eq!(dict_set(d, list(&[]), int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "unhashable type: 'list'"
        );
        assert_eq!(value::as_dict(d).map(|p| p.len()), Some(2));

        exc::init();
        assert_eq!(dict_set(list(&[]), int(1), int(2)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'list' object does not support item assignment"
        );
    }

    fn invoke(obj: i64, method: &str, args: &[i64]) -> i64 {
        crate::methods::call(
            obj,
            method.as_ptr(),
            method.len(),
            args.len(),
            args.as_ptr(),
        )
    }

    #[test]
    fn dict_index_handles_ten_thousand_int_keys() {
        exc::init();
        let d = dict_new(0, std::ptr::null(), std::ptr::null());
        for i in 0..10_000i64 {
            assert_eq!(dict_set(d, int(i), int(i * 2)), NONE);
        }
        let pairs = value::as_dict(d).expect("dict");
        assert_eq!(pairs.len(), 10_000);
        assert_eq!(pairs[0], (int(0), int(0)));
        assert_eq!(pairs[9_999], (int(9_999), int(19_998)));

        // Hash-bucket lookups find every key directly (correctness, not
        // timing): first, middle, last and a few in between.
        for i in [0i64, 1, 2_500, 4_999, 9_998, 9_999] {
            assert_eq!(subscript(d, int(i)), int(i * 2));
            assert_eq!(invoke(d, "get", &[int(i)]), int(i * 2));
        }
        assert!(!exc::pending());
    }

    #[test]
    fn dict_overwrite_keeps_position_and_numeric_keys_collide() {
        exc::init();
        let d = dict_new(0, std::ptr::null(), std::ptr::null());
        dict_set(d, int(1), int(10));
        dict_set(d, string("k"), int(20));
        dict_set(d, int(1), int(30));
        let pairs = value::as_dict(d).expect("dict");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], (int(1), int(30)), "first position, last value");
        assert!(equals(pairs[1].0, string("k")));

        // 1 == 1.0 == True in Python: one entry, key object unchanged.
        assert_eq!(dict_set(d, value::float_new(1.0), int(40)), NONE);
        let pairs = value::as_dict(d).expect("dict");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], (int(1), int(40)));
        assert_eq!(dict_set(d, value::TRUE, int(50)), NONE);
        assert_eq!(value::as_dict(d).expect("dict")[0], (int(1), int(50)));
        assert_eq!(subscript(d, value::float_new(1.0)), int(50));
        assert_eq!(subscript(d, value::TRUE), int(50));
        assert_eq!(invoke(d, "get", &[value::TRUE]), int(50));

        // -0.0 and 0.0 share a bucket and compare equal.
        let zero = dict_new(0, std::ptr::null(), std::ptr::null());
        dict_set(zero, value::float_new(0.0), int(1));
        dict_set(zero, value::float_new(-0.0), int(2));
        assert_eq!(value::as_dict(zero).map(|p| p.len()), Some(1));
        assert_eq!(subscript(zero, int(0)), int(2));
        assert!(!exc::pending());
    }

    #[test]
    fn dict_hashes_string_bytes_and_tuple_keys() {
        exc::init();
        let d = dict_new(0, std::ptr::null(), std::ptr::null());
        dict_set(d, string("a"), int(1));
        // A distinct string object with equal content overwrites in place.
        dict_set(d, string("a"), int(2));
        dict_set(d, value::bytes_new(vec![b'a']), int(3));
        dict_set(d, tuple(&[int(1), string("x")]), int(4));
        assert_eq!(value::as_dict(d).map(|p| p.len()), Some(3));
        assert_eq!(subscript(d, string("a")), int(2));
        assert_eq!(subscript(d, value::bytes_new(vec![b'a'])), int(3));
        assert_eq!(subscript(d, tuple(&[int(1), string("x")])), int(4));
        assert!(!exc::pending());

        // A structurally different tuple is missing, and the message is the
        // key's repr.
        assert_eq!(subscript(d, tuple(&[int(1), int(1)])), NONE);
        assert_eq!(exc::pending_tag(), KEY_ERROR as i64);
        assert_eq!(value::to_string(exc::pending_value()), "(1, 1)");
        crate::exc::clear();

        // NaN never equals itself, so a stored NaN key is never found.
        let nan = value::float_new(f64::NAN);
        let n = dict_new(0, std::ptr::null(), std::ptr::null());
        dict_set(n, nan, int(9));
        assert_eq!(value::as_dict(n).map(|p| p.len()), Some(1));
        assert_eq!(subscript(n, value::float_new(f64::NAN)), NONE);
        assert_eq!(exc::pending_tag(), KEY_ERROR as i64);
        crate::exc::clear();
    }

    #[test]
    fn dict_constructor_index_dedupes_duplicates() {
        exc::init();
        let d = value::dict_new(vec![
            (int(1), int(1)),
            (string("a"), int(2)),
            (value::float_new(1.0), int(3)),
        ]);
        let pairs = value::as_dict(d).expect("dict");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], (int(1), int(3)));
        assert!(equals(pairs[1].0, string("a")));
        assert_eq!(subscript(d, value::TRUE), int(3));
        assert!(!exc::pending());
    }

    #[test]
    fn dict_pop_clear_and_update_keep_index_consistent() {
        exc::init();
        let d = dict_new(0, std::ptr::null(), std::ptr::null());
        for i in 0..64i64 {
            dict_set(d, int(i), int(i));
        }

        assert_eq!(invoke(d, "pop", &[int(10)]), int(10));
        let pairs = value::as_dict(d).expect("dict");
        assert_eq!(pairs.len(), 63);
        assert_eq!(pairs[10], (int(11), int(11)), "later entries shift left");
        assert_eq!(pairs[62], (int(63), int(63)));
        assert_eq!(pairs[0], (int(0), int(0)));
        assert!(!exc::pending());

        // The removed key is gone (KeyError) while the shifted keys are
        // still reachable through the rebuilt index.
        assert_eq!(subscript(d, int(10)), NONE);
        assert_eq!(exc::pending_tag(), KEY_ERROR as i64);
        assert_eq!(value::to_string(exc::pending_value()), "10");
        crate::exc::clear();
        assert_eq!(subscript(d, int(11)), int(11));
        assert_eq!(subscript(d, int(63)), int(63));

        // Re-inserting appends at the end.
        dict_set(d, int(10), int(100));
        let pairs = value::as_dict(d).expect("dict");
        assert_eq!(pairs.len(), 64);
        assert_eq!(pairs[63], (int(10), int(100)));

        // update overwrites in place and appends new keys.
        let source = dict_new(0, std::ptr::null(), std::ptr::null());
        dict_set(source, int(5), int(55));
        dict_set(source, int(999), int(9999));
        invoke(d, "update", &[source]);
        let pairs = value::as_dict(d).expect("dict");
        assert_eq!(pairs.len(), 65);
        assert_eq!(pairs[5], (int(5), int(55)));
        assert_eq!(pairs[64], (int(999), int(9999)));
        assert_eq!(subscript(d, int(5)), int(55));
        assert_eq!(subscript(d, int(999)), int(9999));
        assert!(!exc::pending());

        invoke(d, "clear", &[]);
        assert_eq!(value::as_dict(d).map(|p| p.len()), Some(0));
        assert_eq!(subscript(d, int(5)), NONE);
        assert_eq!(exc::pending_tag(), KEY_ERROR as i64);
        crate::exc::clear();

        // The index is consistent again after a clear.
        dict_set(d, int(5), int(50));
        assert_eq!(value::as_dict(d).map(|p| p.len()), Some(1));
        assert_eq!(subscript(d, int(5)), int(50));
        assert!(!exc::pending());
    }

    #[test]
    fn delete_subscript_removes_list_items_and_dict_keys() {
        exc::init();
        let xs = list(&[int(1), int(2), int(3)]);
        assert_eq!(delete_subscript(xs, int(1)), NONE);
        assert_eq!(value::as_list(xs), Some(&[int(1), int(3)][..]));
        assert_eq!(delete_subscript(xs, int(-1)), NONE);
        assert_eq!(value::as_list(xs), Some(&[int(1)][..]));
        assert!(!exc::pending());

        assert_eq!(delete_subscript(xs, int(5)), NONE);
        assert_eq!(exc::pending_tag(), INDEX_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "list assignment index out of range"
        );
        exc::clear();

        let keys = [string("a"), string("b")];
        let values = [int(1), int(2)];
        let d = dict_new(keys.len(), keys.as_ptr(), values.as_ptr());
        assert_eq!(delete_subscript(d, string("a")), NONE);
        assert_eq!(value::as_dict(d).map(|pairs| pairs.len()), Some(1));
        assert_eq!(subscript(d, string("b")), int(2));
        assert!(!exc::pending());

        assert_eq!(delete_subscript(d, string("a")), NONE);
        assert_eq!(exc::pending_tag(), KEY_ERROR as i64);
        assert_eq!(value::to_string(exc::pending_value()), "'a'");
        exc::clear();

        // Immutable receivers reject deletion.
        assert_eq!(delete_subscript(string("ab"), int(0)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'str' object does not support item deletion"
        );
    }

    #[test]
    fn delete_slice_clears_and_removes_selected_positions() {
        exc::init();
        let cleared = list(&[int(1), int(2), int(3)]);
        assert_eq!(delete_slice(cleared, NONE, NONE, NONE), NONE);
        assert_eq!(value::as_list(cleared), Some(&[][..]));
        assert!(!exc::pending());

        let plain = list(&[int(1), int(2), int(3), int(4), int(5)]);
        assert_eq!(delete_slice(plain, int(1), int(3), NONE), NONE);
        assert_eq!(value::as_list(plain), Some(&[int(1), int(4), int(5)][..]));

        let stepped = list(&[int(1), int(2), int(3), int(4), int(5)]);
        assert_eq!(delete_slice(stepped, int(0), int(5), int(2)), NONE);
        assert_eq!(value::as_list(stepped), Some(&[int(2), int(4)][..]));

        let backwards = list(&[int(1), int(2), int(3), int(4)]);
        assert_eq!(delete_slice(backwards, NONE, NONE, int(-1)), NONE);
        assert_eq!(value::as_list(backwards), Some(&[][..]));

        assert_eq!(delete_slice(tuple(&[int(1)]), NONE, NONE, NONE), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'tuple' object does not support item deletion"
        );
        exc::clear();

        let zero = list(&[int(1)]);
        assert_eq!(delete_slice(zero, NONE, NONE, int(0)), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(
            value::to_string(exc::pending_value()),
            "slice step cannot be zero"
        );
        assert_eq!(value::as_list(zero), Some(&[int(1)][..]));
    }
}
