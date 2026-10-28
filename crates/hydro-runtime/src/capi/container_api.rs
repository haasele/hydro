//! Container C-API additions: `dict`, `list`, `sequence` and `set` entry
//! points that the pinned extension wheels reference but the shim did not
//! export yet.
//!
//! Every function keeps CPython 3.14's ownership contract: borrowed results
//! stay borrowed, the shim containers own one reference per stored handle, and
//! helper entry points that hand out a new reference transfer it from the
//! container. Failures record a Python error (`"<Type>: <message>"`) and return
//! CPython's failure value; none of these functions panic.

use std::os::raw::c_int;
use std::ptr;

use super::*;

/// `Py_LT` from CPython's `object.h`; the only rich comparison op used here.
const PY_LT: c_int = 0;

/// Upper bound on `_PyDict_NewPresized`'s capacity hint. CPython caps the
/// presized table at `2**17` slots, i.e. `USABLE_FRACTION(2**17)` entries.
const MAX_PRESIZED_DICT: usize = 87381;

/// Records CPython's `TypeError` for an unhashable dict key or set element.
fn unhashable_key(key: *mut ShimObject) {
    set_error(format!(
        "TypeError: unhashable type: '{}'",
        object_type_name(key)
    ));
}

// ---------------------------------------------------------------------------
// Dictionary API
// ---------------------------------------------------------------------------

/// `PyDict_Pop`: removes `key` and stores its value in `*result`.
///
/// Returns 1 when the key was present (with a new reference in `*result`),
/// 0 when it was absent (`*result` is NULL, no error), and -1 with an
/// exception otherwise. `result` may be NULL, in which case the popped value
/// is released here, exactly as CPython does.
#[no_mangle]
pub unsafe extern "C" fn PyDict_Pop(
    dict: *mut ShimObject,
    key: *mut ShimObject,
    result: *mut *mut ShimObject,
) -> c_int {
    if !result.is_null() {
        // SAFETY: the caller passed a writable slot for the popped value.
        unsafe { *result = ptr::null_mut() };
    }
    if dict.is_null() || !matches!(kind_ref(dict), Some(ShimKind::Dict(_))) {
        bad_internal_call();
        return -1;
    }
    let pairs = match dict_slots(dict) {
        Ok(pairs) => pairs,
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    // CPython checks `ma_used` before hashing: an empty dict reports "absent"
    // even for an unhashable key.
    if pairs.is_empty() {
        return 0;
    }
    if key.is_null() {
        set_error("TypeError: dict key is NULL");
        return -1;
    }
    if !hashable(key) {
        unhashable_key(key);
        return -1;
    }
    let position = match dict_position(pairs, key) {
        Some(position) => position,
        None => return 0,
    };
    let removed = {
        let pairs = match dict_slots_mut(dict) {
            Ok(pairs) => pairs,
            Err(error) => {
                set_error(error);
                return -1;
            }
        };
        pairs.remove(position)
    };
    dec_ref(removed.0 as *mut ShimObject);
    let value = removed.1 as *mut ShimObject;
    if result.is_null() {
        dec_ref(value);
    } else {
        // SAFETY: the slot was validated at entry; the caller now owns the
        // reference the dict held.
        unsafe { *result = value };
    }
    1
}

/// `PyDict_SetDefault`: returns the value for `key`, inserting `default_object`
/// (taking new references) when the key is absent. The result is a borrowed
/// reference, as in CPython; NULL is returned with an exception on failure.
#[no_mangle]
pub unsafe extern "C" fn PyDict_SetDefault(
    dict: *mut ShimObject,
    key: *mut ShimObject,
    default_object: *mut ShimObject,
) -> *mut ShimObject {
    if dict.is_null() || !matches!(kind_ref(dict), Some(ShimKind::Dict(_))) {
        bad_internal_call();
        return ptr::null_mut();
    }
    if key.is_null() {
        set_error("TypeError: dict key is NULL");
        return ptr::null_mut();
    }
    if default_object.is_null() {
        set_error("TypeError: dict default value is NULL");
        return ptr::null_mut();
    }
    if !hashable(key) {
        unhashable_key(key);
        return ptr::null_mut();
    }
    let position = match dict_slots(dict) {
        Ok(pairs) => dict_position(pairs, key),
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    if let Some(position) = position {
        // The dict keeps owning the pair, so the value stays alive for the
        // caller's borrowed result.
        let pairs = match dict_slots(dict) {
            Ok(pairs) => pairs,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        };
        return pairs[position].1 as *mut ShimObject;
    }
    match dict_set(dict, key, default_object) {
        // The dict took its own reference to `default_object`, so returning
        // the caller's pointer is a valid borrowed result.
        Ok(()) => default_object,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `_PyDict_GetItem_KnownHash`: borrowed lookup with a caller-supplied hash.
///
/// The shim has no hash table, so `hash` is only part of the ABI: keys are
/// matched with the same structural equality the rest of the dict API uses.
/// Returns NULL without an exception when the key is absent.
#[no_mangle]
pub unsafe extern "C" fn _PyDict_GetItem_KnownHash(
    dict: *mut ShimObject,
    key: *mut ShimObject,
    _hash: isize,
) -> *mut ShimObject {
    if dict.is_null() || !matches!(kind_ref(dict), Some(ShimKind::Dict(_))) {
        bad_internal_call();
        return ptr::null_mut();
    }
    if key.is_null() {
        set_error("TypeError: dict key is NULL");
        return ptr::null_mut();
    }
    let pairs = match dict_slots(dict) {
        Ok(pairs) => pairs,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    match dict_position(pairs, key) {
        Some(position) => pairs[position].1 as *mut ShimObject,
        None => ptr::null_mut(),
    }
}

/// `_PyDict_NewPresized`: an empty dict with room for `minused` entries.
///
/// Values at or below CPython's minimum usable size (5) need no reservation;
/// larger hints are clamped to CPython's maximum presized table.
#[no_mangle]
pub unsafe extern "C" fn _PyDict_NewPresized(minused: isize) -> *mut ShimObject {
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    if minused > 5 {
        let wanted = usize::try_from(minused)
            .unwrap_or(MAX_PRESIZED_DICT)
            .min(MAX_PRESIZED_DICT);
        if pairs.try_reserve_exact(wanted).is_err() {
            set_error("MemoryError: cannot presize dict");
            return ptr::null_mut();
        }
    }
    new_object(ShimKind::Dict(pairs))
}

// ---------------------------------------------------------------------------
// List API
// ---------------------------------------------------------------------------

/// `PyList_Insert`: inserts `item` (taking a new reference) before `index`.
///
/// A negative index counts from the end and clamps to 0; an index past the end
/// appends. Returns 0, or -1 with an exception.
#[no_mangle]
pub unsafe extern "C" fn PyList_Insert(
    object: *mut ShimObject,
    index: isize,
    item: *mut ShimObject,
) -> c_int {
    if object.is_null() || !matches!(kind_ref(object), Some(ShimKind::List(_))) {
        bad_internal_call();
        return -1;
    }
    if item.is_null() {
        bad_internal_call();
        return -1;
    }
    let items = match list_slots_mut(object) {
        Ok(items) => items,
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    if items.try_reserve(1).is_err() {
        set_error("MemoryError: cannot grow list");
        return -1;
    }
    let length = items.len() as isize;
    let position = if index < 0 {
        let shifted = index.saturating_add(length);
        if shifted < 0 {
            0
        } else {
            shifted
        }
    } else if index > length {
        length
    } else {
        index
    };
    inc_ref(item);
    items.insert(position as usize, item as usize);
    0
}

/// Replaces `items[low..high]` with `replacement` and returns the handles that
/// were removed (still owning one reference each).
///
/// Indices follow CPython's `list_ass_slice` clamping: `low` is clamped to
/// `[0, len]`, `high` is clamped to `[low, len]`.
fn list_ass_slice_handles(
    object: *mut ShimObject,
    low: isize,
    high: isize,
    replacement: &[usize],
) -> Result<Vec<usize>, String> {
    let items = list_slots_mut(object)?;
    let length = items.len() as isize;
    let low = if low < 0 {
        0
    } else if low > length {
        length
    } else {
        low
    } as usize;
    let high = if high < low as isize {
        low as isize
    } else if high > length {
        length
    } else {
        high
    } as usize;
    let new_len = items.len() - (high - low) + replacement.len();
    let mut rebuilt: Vec<usize> = Vec::new();
    if rebuilt.try_reserve_exact(new_len).is_err() {
        return Err("MemoryError: cannot resize list".to_string());
    }
    let mut removed: Vec<usize> = Vec::new();
    if removed.try_reserve_exact(high - low).is_err() {
        return Err("MemoryError: cannot resize list".to_string());
    }
    rebuilt.extend_from_slice(&items[..low]);
    rebuilt.extend_from_slice(replacement);
    rebuilt.extend_from_slice(&items[high..]);
    removed.extend_from_slice(&items[low..high]);
    *items = rebuilt;
    Ok(removed)
}

/// `PyList_SetSlice`: replaces the `[low, high)` slice with the items of
/// `itemlist`, or deletes it when `itemlist` is NULL.
///
/// The replacement iterable is consumed as new references and becomes owned by
/// the list; the handles it displaces are released after the list is back in a
/// consistent shape. Returns 0, or -1 with an exception.
#[no_mangle]
pub unsafe extern "C" fn PyList_SetSlice(
    object: *mut ShimObject,
    low: isize,
    high: isize,
    itemlist: *mut ShimObject,
) -> c_int {
    if object.is_null() || !matches!(kind_ref(object), Some(ShimKind::List(_))) {
        bad_internal_call();
        return -1;
    }
    let replacement: Vec<usize> = if itemlist.is_null() {
        Vec::new()
    } else {
        match iterable_elements(itemlist) {
            Ok(elements) => elements,
            Err(error) => {
                set_error(error);
                return -1;
            }
        }
    };
    let removed = match list_ass_slice_handles(object, low, high, &replacement) {
        Ok(removed) => removed,
        Err(error) => {
            // The replacement references were not adopted by the list.
            release_elements(&replacement);
            set_error(error);
            return -1;
        }
    };
    for handle in removed {
        dec_ref(handle as *mut ShimObject);
    }
    0
}

/// Why a list sort stopped early.
enum SortFailure {
    /// The scratch buffer could not be allocated.
    Memory,
    /// A rich comparison raised; the exception is already recorded.
    Compare,
}

/// Compares two live handles with `Py_LT`, propagating a raised exception.
fn sort_less(left: usize, right: usize) -> Result<bool, SortFailure> {
    let status = super::types::PyObject_RichCompareBool(
        left as *mut ShimObject,
        right as *mut ShimObject,
        PY_LT,
    );
    match status {
        1 => Ok(true),
        0 => Ok(false),
        _ => Err(SortFailure::Compare),
    }
}

/// Stable merge sort over handles, using only fallible comparisons.
fn merge_sort(
    items: &mut [usize],
    buffer: &mut [usize],
    start: usize,
    end: usize,
) -> Result<(), SortFailure> {
    if end - start < 2 {
        return Ok(());
    }
    let middle = start + (end - start) / 2;
    merge_sort(items, buffer, start, middle)?;
    merge_sort(items, buffer, middle, end)?;
    let (mut left, mut right, mut out) = (start, middle, start);
    while left < middle && right < end {
        if sort_less(items[right], items[left])? {
            buffer[out] = items[right];
            right += 1;
        } else {
            buffer[out] = items[left];
            left += 1;
        }
        out += 1;
    }
    while left < middle {
        buffer[out] = items[left];
        left += 1;
        out += 1;
    }
    while right < end {
        buffer[out] = items[right];
        right += 1;
        out += 1;
    }
    items[start..end].copy_from_slice(&buffer[start..end]);
    Ok(())
}

/// Sorts `items` in place; `Err(Compare)` leaves a pending exception.
fn sort_handles(items: &mut Vec<usize>) -> Result<(), SortFailure> {
    let len = items.len();
    if len < 2 {
        return Ok(());
    }
    let mut buffer: Vec<usize> = Vec::new();
    if buffer.try_reserve_exact(len).is_err() {
        return Err(SortFailure::Memory);
    }
    buffer.resize(len, 0);
    merge_sort(items.as_mut_slice(), buffer.as_mut_slice(), 0, len)
}

/// `PyList_Sort`: sorts the list in place with `<`, as `list.sort()` does.
///
/// The shim keeps the items out of the list while comparing, so a comparison
/// that re-enters the container sees it empty, matching CPython. If a
/// comparison raises, the handles are put back unchanged (a safe superset of
/// CPython's "the list is left broken" contract) and -1 is returned.
#[no_mangle]
pub unsafe extern "C" fn PyList_Sort(object: *mut ShimObject) -> c_int {
    if object.is_null() || !matches!(kind_ref(object), Some(ShimKind::List(_))) {
        bad_internal_call();
        return -1;
    }
    let mut working: Vec<usize> = match list_slots_mut(object) {
        Ok(items) => std::mem::take(items),
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    let sorted = sort_handles(&mut working);
    let stale = match list_slots_mut(object) {
        Ok(items) => std::mem::replace(items, working),
        Err(_) => Vec::new(),
    };
    // Handles appended by a re-entrant comparison are not part of the sort.
    for handle in stale {
        dec_ref(handle as *mut ShimObject);
    }
    match sorted {
        Ok(()) => 0,
        Err(SortFailure::Memory) => {
            set_error("MemoryError: cannot allocate sort buffer");
            -1
        }
        Err(SortFailure::Compare) => {
            if !error_pending() {
                set_error("TypeError: unorderable objects in list sort");
            }
            -1
        }
    }
}

// ---------------------------------------------------------------------------
// Sequence API
// ---------------------------------------------------------------------------

/// `PySequence_List`: a new list with the items of any iterable.
///
/// A NULL argument is CPython's `PyErr_BadInternalCall` case. Returns NULL with
/// an exception when the argument is not iterable.
#[no_mangle]
pub unsafe extern "C" fn PySequence_List(object: *mut ShimObject) -> *mut ShimObject {
    if object.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    match iterable_elements(object) {
        Ok(elements) => new_object(ShimKind::List(elements)),
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// Set API
// ---------------------------------------------------------------------------

/// `PySet_Discard`: removes `key` when present.
///
/// Returns 1 when an element was removed, 0 when it was absent (no error), and
/// -1 with an exception. `frozenset` is rejected as not mutable, matching the
/// rest of the shim's set API.
#[no_mangle]
pub unsafe extern "C" fn PySet_Discard(object: *mut ShimObject, key: *mut ShimObject) -> c_int {
    match kind_ref(object) {
        Some(ShimKind::Set(_)) => {}
        Some(ShimKind::FrozenSet(_)) => return set_not_mutable(),
        _ => {
            bad_internal_call();
            return -1;
        }
    }
    if key.is_null() {
        bad_internal_call();
        return -1;
    }
    if !hashable(key) {
        unhashable_key(key);
        return -1;
    }
    let removed = {
        let items = match set_slots_mut(object) {
            Some(items) => items,
            None => {
                bad_internal_call();
                return -1;
            }
        };
        items
            .iter()
            .position(|&existing| key_equal(existing as *mut ShimObject, key))
            .map(|position| items.remove(position))
    };
    match removed {
        Some(handle) => {
            dec_ref(handle as *mut ShimObject);
            1
        }
        None => 0,
    }
}

/// `PySet_Pop`: removes and returns an arbitrary element as a new reference.
///
/// The set hands its reference to the caller, so no refcount adjustment is
/// needed. An empty set raises `KeyError`.
#[no_mangle]
pub unsafe extern "C" fn PySet_Pop(object: *mut ShimObject) -> *mut ShimObject {
    match kind_ref(object) {
        Some(ShimKind::Set(_)) => {}
        Some(ShimKind::FrozenSet(_)) => {
            let _ = set_not_mutable();
            return ptr::null_mut();
        }
        _ => {
            bad_internal_call();
            return ptr::null_mut();
        }
    }
    let popped = {
        let items = match set_slots_mut(object) {
            Some(items) => items,
            None => {
                bad_internal_call();
                return ptr::null_mut();
            }
        };
        items.pop()
    };
    match popped {
        Some(handle) => handle as *mut ShimObject,
        None => {
            set_error("KeyError: pop from an empty set");
            ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dict_pop_setdefault_and_presize() {
        super::anchor();
        let _ = take_error();
        let dict = PyDict_New();
        let key = new_str("alpha".to_string());
        let value = new_int(1);
        let other = new_int(2);
        assert_eq!(PyDict_SetItem(dict, key, value), 0);

        // An existing key keeps the stored value.
        assert_eq!(unsafe { PyDict_SetDefault(dict, key, other) }, value);
        // A new key adopts the default and returns it.
        let missing = new_str("beta".to_string());
        assert_eq!(unsafe { PyDict_SetDefault(dict, missing, other) }, other);
        assert_eq!(PyDict_Size(dict), 2);
        // SAFETY: plain handle arguments.
        assert_eq!(
            unsafe { _PyDict_GetItem_KnownHash(dict, missing, 0) },
            other
        );
        assert!(unsafe { _PyDict_GetItem_KnownHash(dict, value, 0) }.is_null());

        let mut popped: *mut ShimObject = ptr::null_mut();
        // SAFETY: `popped` is a live writable slot.
        assert_eq!(unsafe { PyDict_Pop(dict, key, &mut popped) }, 1);
        assert_eq!(popped, value);
        dec_ref(popped);
        let mut missing_value: *mut ShimObject = value;
        // SAFETY: `missing_value` is a live writable slot; the key is gone now.
        assert_eq!(unsafe { PyDict_Pop(dict, key, &mut missing_value) }, 0);
        assert!(missing_value.is_null());

        // SAFETY: plain handle arguments.
        let presized = unsafe { _PyDict_NewPresized(64) };
        let negative = unsafe { _PyDict_NewPresized(-3) };
        assert_eq!(PyDict_Size(presized), 0);
        assert_eq!(PyDict_Size(negative), 0);

        for object in [dict, key, value, other, missing, presized, negative] {
            dec_ref(object);
        }
    }

    #[test]
    fn list_insert_setslice_and_sort() {
        super::anchor();
        let _ = take_error();
        let list = PyList_New(0);
        for value in [3, 1, 2] {
            let item = new_int(value);
            assert_eq!(PyList_Append(list, item), 0);
            dec_ref(item);
        }
        let zero = new_int(0);
        // SAFETY: plain handle arguments.
        assert_eq!(unsafe { PyList_Insert(list, 0, zero) }, 0);
        assert_eq!(unsafe { PyList_Insert(list, -1, zero) }, 0);
        assert_eq!(PyList_Size(list), 5);
        dec_ref(zero);

        assert_eq!(unsafe { PyList_Sort(list) }, 0);
        for (index, expected) in [0, 0, 1, 2, 3].iter().enumerate() {
            assert_eq!(
                as_int(PyList_GetItem(list, index as isize)).unwrap(),
                *expected as i64
            );
        }

        let replacement = PyList_New(0);
        let ten = new_int(10);
        assert_eq!(PyList_Append(replacement, ten), 0);
        dec_ref(ten);
        // SAFETY: plain handle arguments; NULL deletes the slice.
        assert_eq!(unsafe { PyList_SetSlice(list, 1, 4, replacement) }, 0);
        assert_eq!(PyList_Size(list), 3);
        assert_eq!(as_int(PyList_GetItem(list, 1)).unwrap(), 10);
        // SAFETY: plain handle arguments; NULL deletes the slice. Negative
        // slice bounds clamp to 0, as in CPython's C-level list_ass_slice.
        assert_eq!(unsafe { PyList_SetSlice(list, 2, 3, ptr::null_mut()) }, 0);
        assert_eq!(PyList_Size(list), 2);

        let copy = unsafe { PySequence_List(list) };
        assert_eq!(PyList_Size(copy), 2);
        assert_eq!(as_int(PyList_GetItem(copy, 0)).unwrap(), 0);

        for object in [list, replacement, copy] {
            dec_ref(object);
        }
    }

    #[test]
    fn set_discard_and_pop() {
        super::anchor();
        let _ = take_error();
        let set = PySet_New(ptr::null_mut());
        let one = new_int(1);
        let two = new_int(2);
        assert_eq!(PySet_Add(set, one), 0);
        assert_eq!(PySet_Add(set, two), 0);
        // SAFETY: plain handle arguments.
        assert_eq!(unsafe { PySet_Discard(set, one) }, 1);
        assert_eq!(unsafe { PySet_Discard(set, one) }, 0);
        assert_eq!(PySet_Size(set), 1);
        let popped = unsafe { PySet_Pop(set) };
        assert_eq!(popped, two);
        assert_eq!(PySet_Size(set), 0);
        assert!(unsafe { PySet_Pop(set) }.is_null());
        assert_eq!(
            take_error().unwrap_or_default(),
            "KeyError: pop from an empty set"
        );

        for object in [set, one, two, popped] {
            dec_ref(object);
        }
    }
}
