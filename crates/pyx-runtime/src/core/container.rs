//! List/dict/tuple construction, indexing, unpacking and structural equality
//! (ARCHITECTURE §3.2, Phase 2.0).
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

// -------------------------------------------------------------------------
// Literals
// -------------------------------------------------------------------------

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
    // SAFETY: the ABI contract requires `argv` to point at `argc` readable
    // tagged values; the length check above keeps the slice in range.
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
    // SAFETY: the ABI contract requires both buffers to hold `argc` readable
    // tagged values; the length check above keeps the slices in range.
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

/// `a, b, ... = value`: checks that `value` is a list or tuple with exactly
/// `expected` elements. On success returns `None`; mismatches raise
/// `ValueError` and non-sequences `TypeError`.
pub(crate) fn unpack(value: i64, expected: usize) -> i64 {
    let len = if let Some(items) = value::as_list(value) {
        items.len()
    } else if let Some(items) = value::as_tuple(value) {
        items.len()
    } else if let Some(bytes) = value::as_bytes(value) {
        bytes.len()
    } else {
        return type_error(format!(
            "cannot unpack non-iterable {} object",
            value::type_name(value)
        ));
    };
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

// -------------------------------------------------------------------------
// Dict payload: insertion-ordered entries plus a hash index
// -------------------------------------------------------------------------

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

// -------------------------------------------------------------------------
// Key hashing
// -------------------------------------------------------------------------

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
/// the same bits; ints that round to the same `f64` merely collide.
fn numeric_hash(v: i64) -> Option<u64> {
    let value = if let Some(i) = value::as_integer(v) {
        i as f64
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

/// Type name of an unhashable object (`list`, `dict`, `iterator`, or a tuple
/// containing one), if any. Tuples are hashable exactly when every element is.
pub(crate) fn unhashable_name(v: i64) -> Option<&'static str> {
    if value::as_list(v).is_some() {
        Some("list")
    } else if value::as_dict(v).is_some() || value::as_defaultdict(v).is_some() {
        Some("dict")
    } else if value::as_set(v).is_some() {
        Some("set")
    } else if value::as_iter(v).is_some() {
        Some("iterator")
    } else if let Some(items) = value::as_tuple(v) {
        items.iter().find_map(|item| unhashable_name(*item))
    } else {
        None
    }
}

pub(crate) fn unhashable_message(name: &str) -> String {
    format!("unhashable type: '{}'", name)
}

// -------------------------------------------------------------------------
// Dict operations over a pair table
// -------------------------------------------------------------------------

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

// -------------------------------------------------------------------------
// Comprehension builders (Phase 2.8)
// -------------------------------------------------------------------------

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

// -------------------------------------------------------------------------
// Indexing
// -------------------------------------------------------------------------

/// `object[index]` for lists, strings, tuples and dicts.
pub(crate) fn subscript(obj: i64, index: i64) -> i64 {
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
    type_error(format!(
        "'{}' object is not subscriptable",
        value::type_name(obj)
    ))
}

/// `object[index] = value` for lists and dicts. Always returns `None`; errors
/// are reported through the pending exception.
pub(crate) fn subscript_set(obj: i64, index: i64, new_value: i64) -> i64 {
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
    type_error(format!(
        "'{}' object does not support item assignment",
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

// -------------------------------------------------------------------------
// Slicing
// -------------------------------------------------------------------------

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

// -------------------------------------------------------------------------
// Structural equality
// -------------------------------------------------------------------------

/// Python `==` for the value subset (numeric cross-type equality, string
/// contents, deep list equality and order-insensitive dict equality).
pub(crate) fn equals(a: i64, b: i64) -> bool {
    equals_depth(a, b, 0)
}

const MAX_EQ_DEPTH: usize = 64;

fn equals_depth(a: i64, b: i64, depth: usize) -> bool {
    match (value::num_of(a), value::num_of(b)) {
        (Some(x), Some(y)) => return num_eq(x, y),
        (Some(_), None) | (None, Some(_)) => return false,
        (None, None) => {}
    }
    if let (Some(s), Some(t)) = (value::as_str(a), value::as_str(b)) {
        return s == t;
    }
    if let (Some(s), Some(t)) = (value::as_bytes(a), value::as_bytes(b)) {
        return s == t;
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
}
