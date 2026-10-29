//! CPython-compatible layout of the handle prefix and of inline tuples.
//!
//! Every handle starts with a `PyVarObject`: `ob_refcnt` at offset 0,
//! `ob_type` at offset 8 and `ob_size` at offset 16. What follows `ob_size` is
//! interpreted by CPython's inline macros on a per-type basis, so this host
//! lays out the bytes it can support exactly where CPython 3.14 puts them.
//!
//! The supported inline macros are:
//!
//! | Macro | Bytes read/written |
//! | --- | --- |
//! | `Py_REFCNT` | `ob_refcnt` at 0 |
//! | `Py_TYPE` | `ob_type` at 8 |
//! | `Py_SIZE` | `ob_size` at 16 |
//! | `PyTuple_GET_SIZE`, `PyList_GET_SIZE`, `PyBytes_GET_SIZE` | `ob_size` at 16 |
//! | `PyTuple_GET_ITEM`, `PyTuple_SET_ITEM` | inline `ob_item[]` at 32 |
//! | `PyList_GET_ITEM`, `PyList_SET_ITEM` | `ob_item` pointer at 24 |
//!
//! Everything else must go through the API functions, because the bytes those
//! macros use are occupied by the Rust payload here:
//!
//! * `PyUnicode_GET_LENGTH`/`PyUnicode_DATA`/`PyUnicode_KIND` read the
//!   `PyASCIIObject` fields at 16/24/32 — use `PyUnicode_GetLength`,
//!   `PyUnicode_AsUTF8AndSize` and the `PyUnicode_DATA` entry point instead
//!   (`PyUnicode_KIND` is not provided).
//! * `PyBytes_AS_STRING` returns `PyBytesObject.ob_sval` at 32 — use
//!   `PyBytes_AsString`/`PyBytes_AsStringAndSize` instead.
//! * `PyByteArray_AS_STRING` and the `PyByteArrayObject` fields at 24..40 —
//!   use the `PyByteArray_*` functions instead.
//! * `PyListObject.allocated` at 32 — use `PyList_Size`/`PyList_GetItem`.
//!
//! Those macros are not silently faked: reading them yields the Rust payload
//! bytes, so code that needs their values has to call the API.
//!
//! Tuples are the only variable-size payload. Their item pointers live inline
//! at offset 32, exactly where `PyTupleObject.ob_item` sits in CPython 3.14
//! (after `ob_hash` at 24), and the owning `ShimKind` follows the array.
//! [`TupleItems`] is a view over those inline slots, so an extension writing
//! through `PyTuple_SET_ITEM` is observed by every Rust reader.

use std::ops::Deref;
use std::sync::atomic::Ordering;

use super::ShimObject;

/// Offset of `ob_refcnt` (`Py_REFCNT`).
pub(crate) const REFCNT_OFFSET: usize = 0;
/// Offset of `ob_type` (`Py_TYPE`).
pub(crate) const TYPE_OFFSET: usize = 8;
/// Offset of `ob_size` (`PyVarObject`, `Py_SIZE`).
pub(crate) const SIZE_OFFSET: usize = 16;
/// Offset of the `PyVarObject`-trailing word: `ob_hash` for tuples and bytes,
/// `ob_item` for lists.
pub(crate) const AUX_OFFSET: usize = 24;
/// Size of `PyVarObject`: `PyObject_HEAD` plus `ob_size`.
pub(crate) const VAR_HEAD_SIZE: usize = 24;
/// Bytes of the prefix every handle starts with. The `ShimKind` payload of a
/// non-tuple handle follows it.
pub(crate) const PREFIX_SIZE: usize = 32;
/// Offset of the first inline tuple item (`PyTupleObject.ob_item`, which
/// CPython 3.14 places after `ob_hash`).
pub(crate) const TUPLE_ITEMS_OFFSET: usize = 32;
/// `ob_hash` of a tuple that has not been hashed yet, as CPython 3.14 writes
/// it.
pub(crate) const TUPLE_HASH_UNSET: isize = -1;

/// Number of bytes the inline item array of a tuple with `count` items takes.
pub(crate) fn tuple_items_size(count: usize) -> usize {
    count
        .checked_mul(std::mem::size_of::<usize>())
        .expect("tuple item array size overflow")
}

/// Offset of a tuple's `ShimKind` payload, just past the inline item array.
pub(crate) fn tuple_kind_offset(count: usize) -> usize {
    TUPLE_ITEMS_OFFSET + tuple_items_size(count)
}

/// The items of a tuple handle, stored inline in the handle allocation.
///
/// The length is the handle's `ob_size`, the same value CPython's
/// `PyTuple_GET_SIZE` reads, and the pointers live at
/// [`TUPLE_ITEMS_OFFSET`]… exactly where `PyTuple_GET_ITEM` looks. Because the
/// representation is the CPython one, an extension writing through
/// `PyTuple_SET_ITEM` is immediately visible here; there is no second copy to
/// keep in sync.
#[derive(Clone, Copy)]
pub(crate) struct TupleItems {
    object: *mut ShimObject,
}

// The handle is a stable heap allocation; the view only names it. `ShimObject`
// declares `Send`/`Sync`, so the view inherits the same contract.
unsafe impl Send for TupleItems {}
unsafe impl Sync for TupleItems {}

impl TupleItems {
    /// Names the inline items of `object`.
    ///
    /// # Safety
    /// `object` must be a live tuple handle whose `ob_size` describes the
    /// inline array.
    pub(crate) unsafe fn new(object: *mut ShimObject) -> Self {
        TupleItems { object }
    }

    /// Number of items, from `ob_size`.
    pub(crate) fn len(&self) -> usize {
        // SAFETY: the constructor contract keeps the handle live.
        let count = unsafe { (*self.object).ob_size.load(Ordering::Relaxed) };
        count.max(0) as usize
    }

    fn items_pointer(&self) -> *mut usize {
        // SAFETY: the inline array starts at a fixed offset from a live
        // handle and holds `ob_size` pointers.
        unsafe { (self.object as *mut u8).add(TUPLE_ITEMS_OFFSET) as *mut usize }
    }

    /// The inline items.
    pub(crate) fn as_slice(&self) -> &[usize] {
        // SAFETY: the allocation holds `len()` initialized pointers at the
        // inline offset.
        unsafe { std::slice::from_raw_parts(self.items_pointer(), self.len()) }
    }
}

impl Deref for TupleItems {
    type Target = [usize];

    fn deref(&self) -> &[usize] {
        self.as_slice()
    }
}

impl<'a> IntoIterator for &'a TupleItems {
    type Item = &'a usize;
    type IntoIter = std::slice::Iter<'a, usize>;

    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_offsets_match_cpython() {
        assert_eq!(REFCNT_OFFSET, 0);
        assert_eq!(TYPE_OFFSET, 8);
        assert_eq!(SIZE_OFFSET, 16);
        assert_eq!(AUX_OFFSET, 24);
        assert_eq!(VAR_HEAD_SIZE, 24);
        assert_eq!(PREFIX_SIZE, 32);
        assert_eq!(TUPLE_ITEMS_OFFSET, 32);
        assert_eq!(tuple_kind_offset(0), PREFIX_SIZE);
        assert_eq!(tuple_kind_offset(3), 32 + 3 * std::mem::size_of::<usize>());
    }
}
