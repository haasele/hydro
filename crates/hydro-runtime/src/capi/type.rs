//! Real CPython 3.14 `PyTypeObject` layout and the type/object allocation API.
//!
//! Unlike the previous opaque `TypeHandle`, every object handed to an
//! extension is now a CPython-layout handle: instances start with
//! `PyVarObject` (`ob_refcnt` at 0, `ob_type` at 8, `ob_size` at 16), and type
//! objects are a real `PyTypeObject` mirror. Shim builtins additionally mirror
//! the sub-layouts the inline C macros use: tuples store their item pointers
//! inline at offset 32 and lists publish their `ob_item` pointer at 24 (see
//! [`super::tuple_layout`]).
//!
//! # Type roles
//!
//! Every `ob_type` points at one of these:
//!
//! - [`TypeRole::Shim`]: the instance is a [`super::ShimObject`] and its
//!   payload lives in [`super::ShimKind`]. These are the builtin value types
//!   (`NoneType`, `int`, `bytes`, `list`, ...).
//! - [`TypeRole::Instance`]: the instance is a raw allocation of
//!   `tp_basicsize` (+ `tp_itemsize * ob_size`) bytes owned by an extension.
//!   This is what `PyType_FromSpec` creates and what `PyType_GenericAlloc`
//!   allocates.
//! - [`TypeRole::Metatype`]: the object *is* a type object (`Py_TYPE` returns
//!   the shared metatype). Static exception classes use this role.
//!
//! The role lives in the padding past CPython's `tp_versions_used` field, so it
//! is invisible to extensions; the CPython prefix is byte-for-byte compatible
//! and asserted below.
//!
//! # Scope
//!
//! `PyType_FromSpec` accepts multiple bases (C3 linearisation), members and
//! getset descriptors, the number/sequence/mapping suites, iteration,
//! rich comparison, `tp_clear`, `tp_del` and the construction/deallocation
//! slots. Every accepted slot has a real code path: construction
//! (`tp_new`/`tp_init`), deallocation (`tp_dealloc`/`tp_del`/`tp_free`), calls
//! (`tp_call`), attribute lookup (`tp_getattro`/`tp_getattr`/descriptors),
//! `tp_repr`/`tp_str`, `tp_hash`, buffer export, iteration and GC tracking.
//!
//! GC tracking is not bookkeeping only: [`collect_cycles`] is the body of the
//! exported `PyGC_Collect` and runs CPython's cycle algorithm over the tracked
//! instances, driving each type's `tp_traverse` to find internal references
//! and each unreachable instance's `tp_clear` to break the cycle exactly once.
//! The default deallocator clears an instance only when the collector did not.
//!
//! All CPython 3.14 builtin types are exported as real data symbols
//! (`PyLong_Type`, `PyType_Type`, ...), and `ob_type` of every handle points at
//! the corresponding symbol.
//!
//! Type objects created by `PyType_FromSpec` are reference-counted like
//! CPython's heap types; static types (and the `PyExc_*` classes) are immortal.

#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(private_interfaces)]

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, Once, PoisonError};

use super::buffer::PyBufferProcs;
use super::{
    dec_ref, inc_ref, read_c_string, read_method_table, set_error, value_object, GilGuard,
    MethodSpec, PyMethodDefMirror, ShimObject, ShimValue, IMMORTAL_REFCNT, MIN_VAR_BASICSIZE,
    PY_OBJECT_HEAD_SIZE,
};

/// `Py_TPFLAGS_*` bits from CPython 3.14's `object.h`.
pub(crate) const PY_TPFLAGS_HAVE_FINALIZE: c_ulong = 1 << 0;
/// Set on exception classes; `PyExceptionClass_Check`/PyO3 require it.
pub(crate) const PY_TPFLAGS_BASE_EXC_SUBCLASS: c_ulong = 1 << 30;
/// `PyType_Check` in CPython 3.14 tests this bit on `Py_TYPE(op)`.
pub(crate) const PY_TPFLAGS_TYPE_SUBCLASS: c_ulong = 1 << 31;
pub(crate) const PY_TPFLAGS_LONG_SUBCLASS: c_ulong = 1 << 24;
pub(crate) const PY_TPFLAGS_LIST_SUBCLASS: c_ulong = 1 << 25;
pub(crate) const PY_TPFLAGS_TUPLE_SUBCLASS: c_ulong = 1 << 26;
pub(crate) const PY_TPFLAGS_BYTES_SUBCLASS: c_ulong = 1 << 27;
pub(crate) const PY_TPFLAGS_UNICODE_SUBCLASS: c_ulong = 1 << 28;
pub(crate) const PY_TPFLAGS_DICT_SUBCLASS: c_ulong = 1 << 29;
const PY_TPFLAGS_BUILTIN_SUBCLASSES: c_ulong = PY_TPFLAGS_LONG_SUBCLASS
    | PY_TPFLAGS_LIST_SUBCLASS
    | PY_TPFLAGS_TUPLE_SUBCLASS
    | PY_TPFLAGS_BYTES_SUBCLASS
    | PY_TPFLAGS_UNICODE_SUBCLASS
    | PY_TPFLAGS_DICT_SUBCLASS
    | PY_TPFLAGS_BASE_EXC_SUBCLASS
    | PY_TPFLAGS_TYPE_SUBCLASS;
const PY_TPFLAGS_INLINE_VALUES: c_ulong = 1 << 2;
const PY_TPFLAGS_MANAGED_WEAKREF: c_ulong = 1 << 3;
const PY_TPFLAGS_MANAGED_DICT: c_ulong = 1 << 4;
const PY_TPFLAGS_SEQUENCE: c_ulong = 1 << 5;
const PY_TPFLAGS_MAPPING: c_ulong = 1 << 6;
pub(crate) const PY_TPFLAGS_DISALLOW_INSTANTIATION: c_ulong = 1 << 7;
pub(crate) const PY_TPFLAGS_IMMUTABLETYPE: c_ulong = 1 << 8;
pub(crate) const PY_TPFLAGS_HEAPTYPE: c_ulong = 1 << 9;
pub(crate) const PY_TPFLAGS_BASETYPE: c_ulong = 1 << 10;
pub(crate) const PY_TPFLAGS_HAVE_VECTORCALL: c_ulong = 1 << 11;
pub(crate) const PY_TPFLAGS_READY: c_ulong = 1 << 12;
const PY_TPFLAGS_READYING: c_ulong = 1 << 13;
pub(crate) const PY_TPFLAGS_HAVE_GC: c_ulong = 1 << 14;
const PY_TPFLAGS_HAVE_STACKLESS_EXTENSION: c_ulong = 3 << 15;
pub(crate) const PY_TPFLAGS_HAVE_VERSION_TAG: c_ulong = 1 << 18;
const PY_TPFLAGS_VALID_VERSION_TAG: c_ulong = 1 << 19;
const PY_TPFLAGS_IS_ABSTRACT: c_ulong = 1 << 20;

/// Flags a `PyType_Spec` may request. `Py_TPFLAGS_DEFAULT` is zero in 3.14;
/// the remaining accepted bits all have a real implementation here. Every
/// other bit (managed dict/weakref, inline values, sequence/mapping markers,
/// item storage, abstractness, ...) is rejected by `PyType_FromSpec`.
pub(crate) const SUPPORTED_TYPE_FLAGS: c_ulong = PY_TPFLAGS_HAVE_FINALIZE
    | PY_TPFLAGS_DISALLOW_INSTANTIATION
    | PY_TPFLAGS_IMMUTABLETYPE
    | PY_TPFLAGS_BASETYPE
    | PY_TPFLAGS_HAVE_VECTORCALL
    | PY_TPFLAGS_HAVE_GC
    | PY_TPFLAGS_HAVE_VERSION_TAG;

/// `Py_TPFLAGS` bits that are valid on a type but not requestable by a spec.
const INVALID_SPEC_FLAGS: c_ulong = PY_TPFLAGS_HEAPTYPE
    | PY_TPFLAGS_READY
    | PY_TPFLAGS_READYING
    | PY_TPFLAGS_VALID_VERSION_TAG
    | PY_TPFLAGS_IS_ABSTRACT
    | PY_TPFLAGS_INLINE_VALUES
    | PY_TPFLAGS_MANAGED_WEAKREF
    | PY_TPFLAGS_MANAGED_DICT
    | PY_TPFLAGS_SEQUENCE
    | PY_TPFLAGS_MAPPING
    | PY_TPFLAGS_HAVE_STACKLESS_EXTENSION;

/// Upper bound on the entries read from a `PyType_Slot` table.
const MAX_TYPE_SLOTS: usize = 256;

/// Upper bounds on the allocation sizes accepted from a type spec. A corrupt
/// or hostile spec must not make the first allocation request an absurd
/// amount of memory.
const MAX_BASICSIZE: isize = 1 << 24;
const MAX_ITEMSIZE: isize = 1 << 24;
const MAX_INSTANCE_SIZE: usize = 1 << 30;

/// How the instance payload of a type is represented.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TypeRole {
    /// Instances are [`ShimObject`] values with a tagged payload.
    Shim,
    /// Instances are raw `tp_basicsize` byte allocations owned by an extension.
    Instance,
    /// The object at this address is itself a type object.
    Metatype,
}

/// Mirrors `PyType_Slot`: `{ slot, pfunc }` followed by the padding that
/// aligns `pfunc` to a pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct PyTypeSlotMirror {
    pub(crate) slot: c_int,
    pub(crate) pfunc: *mut c_void,
}

// CPython 3.14 x86_64: one `int`, four bytes of padding, one pointer.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<PyTypeSlotMirror>() == 16);
    assert!(std::mem::offset_of!(PyTypeSlotMirror, slot) == 0);
    assert!(std::mem::offset_of!(PyTypeSlotMirror, pfunc) == 8);
};

/// Mirrors `PyType_Spec` from CPython 3.14 (`object.h`):
/// `{ name, basicsize, itemsize, flags, slots }`.
#[repr(C)]
pub(crate) struct PyTypeSpecMirror {
    pub(crate) name: *const c_char,
    pub(crate) basicsize: c_int,
    pub(crate) itemsize: c_int,
    pub(crate) flags: c_uint,
    pub(crate) slots: *mut PyTypeSlotMirror,
}

// CPython 3.14 x86_64: pointer, two ints, one unsigned int, one pointer.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<PyTypeSpecMirror>() == 32);
    assert!(std::mem::offset_of!(PyTypeSpecMirror, name) == 0);
    assert!(std::mem::offset_of!(PyTypeSpecMirror, basicsize) == 8);
    assert!(std::mem::offset_of!(PyTypeSpecMirror, itemsize) == 12);
    assert!(std::mem::offset_of!(PyTypeSpecMirror, flags) == 16);
    assert!(std::mem::offset_of!(PyTypeSpecMirror, slots) == 24);
};

/// Mirrors `PyGetSetDef` from CPython 3.14's `descrobject.h`:
/// `{ name, get, set, doc, closure }`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct PyGetSetDefMirror {
    pub(crate) name: *const c_char,
    pub(crate) get: usize,
    pub(crate) set: usize,
    pub(crate) doc: *const c_char,
    pub(crate) closure: *mut c_void,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<PyGetSetDefMirror>() == 40);
    assert!(std::mem::offset_of!(PyGetSetDefMirror, get) == 8);
    assert!(std::mem::offset_of!(PyGetSetDefMirror, closure) == 32);
};

/// Mirrors `PyMemberDef` from CPython 3.14's `descrobject.h`:
/// `{ name, type, offset, flags, doc }`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct PyMemberDefMirror {
    pub(crate) name: *const c_char,
    pub(crate) type_: c_int,
    pub(crate) offset: isize,
    pub(crate) flags: c_int,
    pub(crate) doc: *const c_char,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<PyMemberDefMirror>() == 40);
    assert!(std::mem::offset_of!(PyMemberDefMirror, type_) == 8);
    assert!(std::mem::offset_of!(PyMemberDefMirror, offset) == 16);
    assert!(std::mem::offset_of!(PyMemberDefMirror, flags) == 24);
    assert!(std::mem::offset_of!(PyMemberDefMirror, doc) == 32);
};

/// `PyMemberDef` type identifiers from CPython 3.14's `descrobject.h`.
pub(crate) const T_SHORT: c_int = 0;
pub(crate) const T_INT: c_int = 1;
pub(crate) const T_LONG: c_int = 2;
pub(crate) const T_FLOAT: c_int = 3;
pub(crate) const T_DOUBLE: c_int = 4;
pub(crate) const T_STRING: c_int = 5;
pub(crate) const T_OBJECT: c_int = 6;
pub(crate) const T_CHAR: c_int = 7;
pub(crate) const T_BYTE: c_int = 8;
pub(crate) const T_UBYTE: c_int = 9;
pub(crate) const T_USHORT: c_int = 10;
pub(crate) const T_UINT: c_int = 11;
pub(crate) const T_ULONG: c_int = 12;
pub(crate) const T_STRING_INPLACE: c_int = 13;
pub(crate) const T_BOOL: c_int = 14;
pub(crate) const T_OBJECT_EX: c_int = 16;
pub(crate) const T_LONGLONG: c_int = 17;
pub(crate) const T_ULONGLONG: c_int = 18;
pub(crate) const T_PYSSIZET: c_int = 19;
pub(crate) const T_NONE: c_int = 20;

/// `PyMemberDef` flags.
pub(crate) const READONLY: c_int = 1;
#[allow(dead_code)] // ABI mirror constant; not all flags are consumed yet.
pub(crate) const READ_RESTRICTED: c_int = 2;
#[allow(dead_code)] // ABI mirror constant; not all flags are consumed yet.
pub(crate) const WRITE_RESTRICTED: c_int = 4;
pub(crate) const RELATIVE_OFFSET: c_int = 8;

/// Mirrors `PyNumberMethods` (36 function pointers).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct PyNumberMethods {
    pub(crate) slots: [usize; 36],
}

/// Mirrors `PySequenceMethods` (10 pointers, including the two retired slice
/// slots CPython keeps for layout compatibility).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct PySequenceMethods {
    pub(crate) slots: [usize; 10],
}

/// Mirrors `PyMappingMethods` (`mp_length`, `mp_subscript`, `mp_ass_subscript`).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct PyMappingMethods {
    pub(crate) slots: [usize; 3],
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<PyNumberMethods>() == 36 * 8);
    assert!(std::mem::size_of::<PySequenceMethods>() == 10 * 8);
    assert!(std::mem::size_of::<PyMappingMethods>() == 3 * 8);
};

/// Slot identifier to `PyNumberMethods` index. Slots 6..=38 and 75/76.
pub(crate) fn number_slot_index(slot: c_int) -> Option<usize> {
    Some(match slot {
        7 => 0,   // nb_add
        36 => 1,  // nb_subtract
        29 => 2,  // nb_multiply
        34 => 3,  // nb_remainder
        10 => 4,  // nb_divmod
        33 => 5,  // nb_power
        30 => 6,  // nb_negative
        32 => 7,  // nb_positive
        6 => 8,   // nb_absolute
        9 => 9,   // nb_bool
        27 => 10, // nb_invert
        28 => 11, // nb_lshift
        35 => 12, // nb_rshift
        8 => 13,  // nb_and
        38 => 14, // nb_xor
        31 => 15, // nb_or
        26 => 16, // nb_int
        11 => 18, // nb_float
        14 => 19, // nb_inplace_add
        23 => 20, // nb_inplace_subtract
        18 => 21, // nb_inplace_multiply
        21 => 22, // nb_inplace_remainder
        20 => 23, // nb_inplace_power
        17 => 24, // nb_inplace_lshift
        22 => 25, // nb_inplace_rshift
        15 => 26, // nb_inplace_and
        25 => 27, // nb_inplace_xor
        19 => 28, // nb_inplace_or
        12 => 29, // nb_floor_divide
        37 => 30, // nb_true_divide
        16 => 31, // nb_inplace_floor_divide
        24 => 32, // nb_inplace_true_divide
        13 => 33, // nb_index
        75 => 34, // nb_matrix_multiply
        76 => 35, // nb_inplace_matrix_multiply
        _ => return None,
    })
}

/// Slot identifier to `PySequenceMethods` index. Slots 39..=46.
pub(crate) fn sequence_slot_index(slot: c_int) -> Option<usize> {
    Some(match slot {
        45 => 0, // sq_length
        40 => 1, // sq_concat
        46 => 2, // sq_repeat
        44 => 3, // sq_item
        39 => 5, // sq_ass_item
        41 => 7, // sq_contains
        42 => 8, // sq_inplace_concat
        43 => 9, // sq_inplace_repeat
        _ => return None,
    })
}

/// Slot identifier to `PyMappingMethods` index. Slots 3..=5.
pub(crate) fn mapping_slot_index(slot: c_int) -> Option<usize> {
    Some(match slot {
        4 => 0, // mp_length
        5 => 1, // mp_subscript
        3 => 2, // mp_ass_subscript
        _ => return None,
    })
}

/// Real `PyTypeObject` layout for CPython 3.14 on x86_64.
///
/// The CPython prefix is exactly the field sequence from
/// `Include/cpython/object.h`; the internal `role`, `owns_name` and
/// `owns_buffer` fields live in the tail padding after `tp_versions_used` and
/// are never visible to an extension (the limited API treats `PyTypeObject` as
/// opaque).
#[repr(C)]
#[allow(dead_code)]
pub(crate) struct TypeObject {
    // PyObject_VAR_HEAD
    pub(crate) ob_refcnt: AtomicIsize,
    pub(crate) ob_type: *mut TypeObject,
    pub(crate) ob_size: AtomicIsize,

    pub(crate) tp_name: *const c_char,
    pub(crate) tp_basicsize: isize,
    pub(crate) tp_itemsize: isize,

    pub(crate) tp_dealloc: usize,
    pub(crate) tp_vectorcall_offset: isize,
    pub(crate) tp_getattr: usize,
    pub(crate) tp_setattr: usize,
    pub(crate) tp_as_async: *mut c_void,
    pub(crate) tp_repr: usize,

    pub(crate) tp_as_number: *mut c_void,
    pub(crate) tp_as_sequence: *mut c_void,
    pub(crate) tp_as_mapping: *mut c_void,

    pub(crate) tp_hash: usize,
    pub(crate) tp_call: usize,
    pub(crate) tp_str: usize,
    pub(crate) tp_getattro: usize,
    pub(crate) tp_setattro: usize,

    pub(crate) tp_as_buffer: *mut PyBufferProcs,

    pub(crate) tp_flags: c_ulong,

    pub(crate) tp_doc: *const c_char,

    pub(crate) tp_traverse: usize,
    pub(crate) tp_clear: usize,
    pub(crate) tp_richcompare: usize,
    pub(crate) tp_weaklistoffset: isize,
    pub(crate) tp_iter: usize,
    pub(crate) tp_iternext: usize,
    pub(crate) tp_methods: *mut PyMethodDefMirror,
    pub(crate) tp_members: *mut c_void,
    pub(crate) tp_getset: *mut c_void,
    pub(crate) tp_base: *mut TypeObject,
    pub(crate) tp_dict: *mut c_void,
    pub(crate) tp_descr_get: usize,
    pub(crate) tp_descr_set: usize,
    pub(crate) tp_dictoffset: isize,
    pub(crate) tp_init: usize,
    pub(crate) tp_alloc: usize,
    pub(crate) tp_new: usize,
    pub(crate) tp_free: usize,
    pub(crate) tp_is_gc: usize,
    pub(crate) tp_bases: *mut c_void,
    pub(crate) tp_mro: *mut c_void,
    pub(crate) tp_cache: *mut c_void,
    pub(crate) tp_subclasses: *mut c_void,
    pub(crate) tp_weaklist: *mut c_void,
    pub(crate) tp_del: usize,
    pub(crate) tp_version_tag: c_uint,
    pub(crate) tp_finalize: usize,
    pub(crate) tp_vectorcall: usize,
    pub(crate) tp_watched: u8,
    pub(crate) tp_versions_used: u16,

    // Internal tail (CPython padding). Never visible to extensions.
    pub(crate) role: TypeRole,
    pub(crate) owns_name: bool,
    pub(crate) owns_buffer: bool,
}

// CPython 3.14 x86_64: 416 bytes, offsets fixed by the stable ABI-facing
// layout. A mismatch would corrupt every extension that inspects a type
// through `PyType_GetSlot` or reads an instance header.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<TypeObject>() == 416);
    assert!(std::mem::offset_of!(TypeObject, ob_refcnt) == 0);
    assert!(std::mem::offset_of!(TypeObject, ob_type) == 8);
    assert!(std::mem::offset_of!(TypeObject, ob_size) == 16);
    assert!(std::mem::offset_of!(TypeObject, tp_name) == 24);
    assert!(std::mem::offset_of!(TypeObject, tp_basicsize) == 32);
    assert!(std::mem::offset_of!(TypeObject, tp_itemsize) == 40);
    assert!(std::mem::offset_of!(TypeObject, tp_dealloc) == 48);
    assert!(std::mem::offset_of!(TypeObject, tp_vectorcall_offset) == 56);
    assert!(std::mem::offset_of!(TypeObject, tp_repr) == 88);
    assert!(std::mem::offset_of!(TypeObject, tp_as_buffer) == 160);
    assert!(std::mem::offset_of!(TypeObject, tp_flags) == 168);
    assert!(std::mem::offset_of!(TypeObject, tp_doc) == 176);
    assert!(std::mem::offset_of!(TypeObject, tp_traverse) == 184);
    assert!(std::mem::offset_of!(TypeObject, tp_methods) == 232);
    assert!(std::mem::offset_of!(TypeObject, tp_base) == 256);
    assert!(std::mem::offset_of!(TypeObject, tp_init) == 296);
    assert!(std::mem::offset_of!(TypeObject, tp_alloc) == 304);
    assert!(std::mem::offset_of!(TypeObject, tp_new) == 312);
    assert!(std::mem::offset_of!(TypeObject, tp_free) == 320);
    assert!(std::mem::offset_of!(TypeObject, tp_version_tag) == 384);
    assert!(std::mem::offset_of!(TypeObject, tp_finalize) == 392);
    assert!(std::mem::offset_of!(TypeObject, tp_vectorcall) == 400);
    assert!(std::mem::offset_of!(TypeObject, tp_watched) == 408);
    assert!(std::mem::offset_of!(TypeObject, tp_versions_used) == 410);
    assert!(std::mem::offset_of!(TypeObject, role) == 412);
};

// Type objects are shared read-only tables after `PyType_Ready`; all mutation
// happens under the emulated GIL (or during single-threaded setup) and the
// reference count is atomic. The raw function pointers are C data, not Rust
// pointers to non-`Sync` data, so sharing the table is sound under that
// protocol.
unsafe impl Send for TypeObject {}
unsafe impl Sync for TypeObject {}

impl TypeObject {
    /// Builds an immutable static type object named `name`.
    pub(crate) const fn builtin(name: *const c_char, role: TypeRole, flags: c_ulong) -> TypeObject {
        TypeObject {
            ob_refcnt: AtomicIsize::new(IMMORTAL_REFCNT),
            ob_type: ptr::null_mut(),
            ob_size: AtomicIsize::new(0),
            tp_name: name,
            tp_basicsize: PY_OBJECT_HEAD_SIZE as isize,
            tp_itemsize: 0,
            tp_dealloc: 0,
            tp_vectorcall_offset: 0,
            tp_getattr: 0,
            tp_setattr: 0,
            tp_as_async: ptr::null_mut(),
            tp_repr: 0,
            tp_as_number: ptr::null_mut(),
            tp_as_sequence: ptr::null_mut(),
            tp_as_mapping: ptr::null_mut(),
            tp_hash: 0,
            tp_call: 0,
            tp_str: 0,
            tp_getattro: 0,
            tp_setattro: 0,
            tp_as_buffer: ptr::null_mut(),
            tp_flags: flags,
            tp_doc: ptr::null(),
            tp_traverse: 0,
            tp_clear: 0,
            tp_richcompare: 0,
            tp_weaklistoffset: 0,
            tp_iter: 0,
            tp_iternext: 0,
            tp_methods: ptr::null_mut(),
            tp_members: ptr::null_mut(),
            tp_getset: ptr::null_mut(),
            tp_base: ptr::null_mut(),
            tp_dict: ptr::null_mut(),
            tp_descr_get: 0,
            tp_descr_set: 0,
            tp_dictoffset: 0,
            tp_init: 0,
            tp_alloc: 0,
            tp_new: 0,
            tp_free: 0,
            tp_is_gc: 0,
            tp_bases: ptr::null_mut(),
            tp_mro: ptr::null_mut(),
            tp_cache: ptr::null_mut(),
            tp_subclasses: ptr::null_mut(),
            tp_weaklist: ptr::null_mut(),
            tp_del: 0,
            tp_version_tag: 0,
            tp_finalize: 0,
            tp_vectorcall: 0,
            tp_watched: 0,
            tp_versions_used: 0,
            role,
            owns_name: false,
            owns_buffer: false,
        }
    }

    /// True when this type uses CPython's cyclic GC protocol.
    pub(crate) fn uses_gc(&self) -> bool {
        self.tp_flags & PY_TPFLAGS_HAVE_GC != 0
    }

    /// Full name (`module.Type`) as a Rust string.
    pub(crate) fn name(&self) -> String {
        if self.tp_name.is_null() {
            return "<unknown>".to_string();
        }
        // SAFETY: `tp_name` is NUL-terminated by construction: the builtin
        // statics use C string literals and heap types use CStrings.
        unsafe { CStr::from_ptr(self.tp_name) }
            .to_string_lossy()
            .into_owned()
    }

    /// Short name (the component after the last dot).
    pub(crate) fn short_name(&self) -> String {
        let name = self.name();
        match name.rsplit_once('.') {
            Some((_, short)) => short.to_string(),
            None => name,
        }
    }
}

// Builtin type objects. Their `ob_type` pointers are patched to the metatype
// by `ensure_builtin_types`; the addresses themselves are compile-time
// constants, so objects can be created before the patch runs.
//
// Every static carries the C symbol name CPython exports, so extensions that
// take `&PyLong_Type` (or compare `Py_TYPE(obj)`) see the same address this
// host stores in `ob_type`.
#[export_name = "PyType_Type"]
pub(crate) static mut TYPE_TYPE: TypeObject = TypeObject::builtin(
    c"type".as_ptr(),
    TypeRole::Metatype,
    PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_BASETYPE | PY_TPFLAGS_TYPE_SUBCLASS,
);
#[export_name = "PyBaseObject_Type"]
pub(crate) static mut OBJECT_TYPE: TypeObject = TypeObject::builtin(
    c"object".as_ptr(),
    TypeRole::Instance,
    PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_BASETYPE,
);
#[export_name = "PyNone_Type"]
pub(crate) static mut NONE_TYPE: TypeObject = TypeObject::builtin(
    c"NoneType".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyBool_Type"]
pub(crate) static mut BOOL_TYPE: TypeObject =
    TypeObject::builtin(c"bool".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
#[export_name = "PyLong_Type"]
pub(crate) static mut INT_TYPE: TypeObject = TypeObject::builtin(
    c"int".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_LONG_SUBCLASS,
);
#[export_name = "PyFloat_Type"]
pub(crate) static mut FLOAT_TYPE: TypeObject =
    TypeObject::builtin(c"float".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
#[export_name = "PyComplex_Type"]
pub(crate) static mut COMPLEX_TYPE: TypeObject = TypeObject::builtin(
    c"complex".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyUnicode_Type"]
pub(crate) static mut STR_TYPE: TypeObject = TypeObject::builtin(
    c"str".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_UNICODE_SUBCLASS,
);
#[export_name = "PyBytes_Type"]
pub(crate) static mut BYTES_TYPE: TypeObject = TypeObject::builtin(
    c"bytes".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_BYTES_SUBCLASS,
);
#[export_name = "PyByteArray_Type"]
pub(crate) static mut BYTEARRAY_TYPE: TypeObject = TypeObject::builtin(
    c"bytearray".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyList_Type"]
pub(crate) static mut LIST_TYPE: TypeObject = TypeObject::builtin(
    c"list".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_LIST_SUBCLASS,
);
#[export_name = "PyTuple_Type"]
pub(crate) static mut TUPLE_TYPE: TypeObject = TypeObject::builtin(
    c"tuple".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_TUPLE_SUBCLASS,
);
#[export_name = "PyDict_Type"]
pub(crate) static mut DICT_TYPE: TypeObject = TypeObject::builtin(
    c"dict".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_DICT_SUBCLASS,
);
#[export_name = "PySet_Type"]
pub(crate) static mut SET_TYPE: TypeObject =
    TypeObject::builtin(c"set".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
#[export_name = "PyFrozenSet_Type"]
pub(crate) static mut FROZENSET_TYPE: TypeObject = TypeObject::builtin(
    c"frozenset".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PySlice_Type"]
pub(crate) static mut SLICE_TYPE: TypeObject =
    TypeObject::builtin(c"slice".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
#[export_name = "PyMemoryView_Type"]
pub(crate) static mut MEMORYVIEW_TYPE: TypeObject = TypeObject::builtin(
    c"memoryview".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyRange_Type"]
pub(crate) static mut RANGE_TYPE: TypeObject =
    TypeObject::builtin(c"range".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
#[export_name = "PyCapsule_Type"]
pub(crate) static mut CAPSULE_TYPE: TypeObject = TypeObject::builtin(
    c"PyCapsule".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyModule_Type"]
pub(crate) static mut MODULE_TYPE: TypeObject =
    TypeObject::builtin(c"module".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
#[export_name = "PyCFunction_Type"]
pub(crate) static mut CFUNCTION_TYPE: TypeObject = TypeObject::builtin(
    c"builtin_function_or_method".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
pub(crate) static mut ERROR_TYPE: TypeObject =
    TypeObject::builtin(c"error".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
#[export_name = "PyNotImplemented_Type"]
pub(crate) static mut NOT_IMPLEMENTED_TYPE: TypeObject = TypeObject::builtin(
    c"NotImplementedType".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyEllipsis_Type"]
pub(crate) static mut ELLIPSIS_TYPE: TypeObject = TypeObject::builtin(
    c"ellipsis".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyFunction_Type"]
pub(crate) static mut FUNCTION_TYPE: TypeObject = TypeObject::builtin(
    c"function".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyMethod_Type"]
pub(crate) static mut METHOD_TYPE: TypeObject =
    TypeObject::builtin(c"method".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
#[export_name = "PyProperty_Type"]
pub(crate) static mut PROPERTY_TYPE: TypeObject = TypeObject::builtin(
    c"property".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyGetSetDescr_Type"]
pub(crate) static mut GETSET_DESCR_TYPE: TypeObject = TypeObject::builtin(
    c"getset_descriptor".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyMemberDescr_Type"]
pub(crate) static mut MEMBER_DESCR_TYPE: TypeObject = TypeObject::builtin(
    c"member_descriptor".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyMethodDescr_Type"]
pub(crate) static mut METHOD_DESCR_TYPE: TypeObject = TypeObject::builtin(
    c"method_descriptor".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyWrapperDescr_Type"]
pub(crate) static mut WRAPPER_DESCR_TYPE: TypeObject = TypeObject::builtin(
    c"wrapper_descriptor".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyClassMethodDescr_Type"]
pub(crate) static mut CLASSMETHOD_DESCR_TYPE: TypeObject = TypeObject::builtin(
    c"classmethod_descriptor".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyStaticMethod_Type"]
pub(crate) static mut STATICMETHOD_TYPE: TypeObject = TypeObject::builtin(
    c"staticmethod".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyClassMethod_Type"]
pub(crate) static mut CLASSMETHOD_TYPE: TypeObject = TypeObject::builtin(
    c"classmethod".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyDictProxy_Type"]
pub(crate) static mut DICT_PROXY_TYPE: TypeObject = TypeObject::builtin(
    c"mappingproxy".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
#[export_name = "PyTraceBack_Type"]
pub(crate) static mut TRACEBACK_TYPE: TypeObject = TypeObject::builtin(
    c"traceback".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
/// `PyObject_Type` is referenced by some vendored extension code even though
/// CPython itself only exports `PyBaseObject_Type`. Export a real type object
/// so such code links; this host never stores it in an `ob_type`.
#[export_name = "PyObject_Type"]
pub(crate) static mut OBJECT_ALIAS_TYPE: TypeObject =
    TypeObject::builtin(c"object".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);

/// `types.GenericAlias`, the type of the objects `Py_GenericAlias` returns.
/// Its `tp_repr`, `tp_str`, `__origin__` and `__args__` are provided by
/// `capi::mod` from the payload.
#[export_name = "PyGenericAlias_Type"]
pub(crate) static mut GENERIC_ALIAS_TYPE: TypeObject = TypeObject::builtin(
    c"types.GenericAlias".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);

/// `PySeqIter_Type`, the concrete iterator `PySeqIter_New` returns. CPython
/// exports this type object; the host sets `tp_iter`/`tp_iternext` once the
/// metatype graph is ready.
#[export_name = "PySeqIter_Type"]
pub(crate) static mut SEQITER_TYPE: TypeObject = TypeObject::builtin(
    c"iterator".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);

/// `contextvars.ContextVar`, created by `PyContextVar_New`.
pub(crate) static mut CONTEXT_VAR_TYPE: TypeObject = TypeObject::builtin(
    c"contextvars.ContextVar".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);

/// `contextvars.Token`, returned by `PyContextVar_Set`.
pub(crate) static mut CONTEXT_TOKEN_TYPE: TypeObject = TypeObject::builtin(
    c"contextvars.Token".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);

/// The `ob_type` of a handle that wraps a Hydro value.
///
/// Not an exported CPython symbol: extensions see it only as an opaque
/// pointer from `Py_TYPE`. Its `tp_basicsize` names this host's wrapper, so a
/// C extension can never reach Hydro heap storage through the handle.
pub(crate) static mut HYDRO_TYPE: TypeObject =
    TypeObject::builtin(c"object".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);

// The five class slots of CPython's `PyDateTime_CAPI` capsule. They are not
// exported data symbols (CPython does not export them either); extensions
// reach them through the capsule, and `Py_TYPE` of a date/date-time/time/
// timedelta/tzinfo handle is one of these objects so the header macros
// (`PyDate_Check`, `PyDateTime_Check`, ...) see the right type.
//
// `datetime.datetime` is a subtype of `datetime.date`, exactly like CPython:
// `PyDate_Check` accepts a datetime handle through `PyType_IsSubtype`.
pub(crate) static mut DATE_CAPI_TYPE: TypeObject = TypeObject::builtin(
    c"datetime.date".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_BASETYPE,
);
pub(crate) static mut DATETIME_CAPI_TYPE: TypeObject = TypeObject::builtin(
    c"datetime.datetime".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_BASETYPE,
);
pub(crate) static mut TIME_CAPI_TYPE: TypeObject = TypeObject::builtin(
    c"datetime.time".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_BASETYPE,
);
pub(crate) static mut TIMEDELTA_CAPI_TYPE: TypeObject = TypeObject::builtin(
    c"datetime.timedelta".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_BASETYPE,
);
pub(crate) static mut TZINFO_CAPI_TYPE: TypeObject = TypeObject::builtin(
    c"datetime.tzinfo".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_BASETYPE,
);
/// `datetime.timezone` (a `tzinfo` subclass). The capsule has no slot for it,
/// but values of the class appear through `TimeZone_UTC` and
/// `TimeZone_FromTimeZone`; mapping them keeps `PyTZInfo_Check` correct.
pub(crate) static mut TIMEZONE_CAPI_TYPE: TypeObject = TypeObject::builtin(
    c"datetime.timezone".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_BASETYPE,
);

/// Maps a Hydro value to the `datetime.datetime_CAPI` type object that should
/// appear as its C-level type, or `None` for every other value.
///
/// The runtime caches the native datetime classes under their
/// `datetime.<name>` keys; comparing the value's class against those cached
/// handles keeps an unrelated user class that happens to be called `date`
/// from being treated as a datetime.
pub(crate) fn datetime_capi_type_for(value: i64) -> Option<*mut TypeObject> {
    let class = crate::value::instance_class_value(value)?;
    let candidates: [(&str, *mut TypeObject); 6] = [
        ("datetime.date", ptr::addr_of_mut!(DATE_CAPI_TYPE)),
        ("datetime.datetime", ptr::addr_of_mut!(DATETIME_CAPI_TYPE)),
        ("datetime.time", ptr::addr_of_mut!(TIME_CAPI_TYPE)),
        ("datetime.timedelta", ptr::addr_of_mut!(TIMEDELTA_CAPI_TYPE)),
        ("datetime.tzinfo", ptr::addr_of_mut!(TZINFO_CAPI_TYPE)),
        ("datetime.timezone", ptr::addr_of_mut!(TIMEZONE_CAPI_TYPE)),
    ];
    candidates.into_iter().find_map(|(key, type_object)| {
        (crate::core::globals::registry_get(key) == Some(class)).then_some(type_object)
    })
}

/// Every static builtin type, used to patch the metatype link.
fn builtin_type_pointers() -> [*mut TypeObject; 48] {
    [
        ptr::addr_of_mut!(TYPE_TYPE),
        ptr::addr_of_mut!(OBJECT_TYPE),
        ptr::addr_of_mut!(NONE_TYPE),
        ptr::addr_of_mut!(BOOL_TYPE),
        ptr::addr_of_mut!(INT_TYPE),
        ptr::addr_of_mut!(FLOAT_TYPE),
        ptr::addr_of_mut!(COMPLEX_TYPE),
        ptr::addr_of_mut!(STR_TYPE),
        ptr::addr_of_mut!(BYTES_TYPE),
        ptr::addr_of_mut!(BYTEARRAY_TYPE),
        ptr::addr_of_mut!(LIST_TYPE),
        ptr::addr_of_mut!(TUPLE_TYPE),
        ptr::addr_of_mut!(DICT_TYPE),
        ptr::addr_of_mut!(SET_TYPE),
        ptr::addr_of_mut!(FROZENSET_TYPE),
        ptr::addr_of_mut!(SLICE_TYPE),
        ptr::addr_of_mut!(MEMORYVIEW_TYPE),
        ptr::addr_of_mut!(RANGE_TYPE),
        ptr::addr_of_mut!(CAPSULE_TYPE),
        ptr::addr_of_mut!(MODULE_TYPE),
        ptr::addr_of_mut!(CFUNCTION_TYPE),
        ptr::addr_of_mut!(ERROR_TYPE),
        ptr::addr_of_mut!(NOT_IMPLEMENTED_TYPE),
        ptr::addr_of_mut!(ELLIPSIS_TYPE),
        ptr::addr_of_mut!(FUNCTION_TYPE),
        ptr::addr_of_mut!(METHOD_TYPE),
        ptr::addr_of_mut!(PROPERTY_TYPE),
        ptr::addr_of_mut!(GETSET_DESCR_TYPE),
        ptr::addr_of_mut!(MEMBER_DESCR_TYPE),
        ptr::addr_of_mut!(METHOD_DESCR_TYPE),
        ptr::addr_of_mut!(WRAPPER_DESCR_TYPE),
        ptr::addr_of_mut!(CLASSMETHOD_DESCR_TYPE),
        ptr::addr_of_mut!(STATICMETHOD_TYPE),
        ptr::addr_of_mut!(CLASSMETHOD_TYPE),
        ptr::addr_of_mut!(DICT_PROXY_TYPE),
        ptr::addr_of_mut!(TRACEBACK_TYPE),
        ptr::addr_of_mut!(OBJECT_ALIAS_TYPE),
        ptr::addr_of_mut!(GENERIC_ALIAS_TYPE),
        ptr::addr_of_mut!(SEQITER_TYPE),
        ptr::addr_of_mut!(CONTEXT_VAR_TYPE),
        ptr::addr_of_mut!(CONTEXT_TOKEN_TYPE),
        ptr::addr_of_mut!(HYDRO_TYPE),
        ptr::addr_of_mut!(DATE_CAPI_TYPE),
        ptr::addr_of_mut!(DATETIME_CAPI_TYPE),
        ptr::addr_of_mut!(TIME_CAPI_TYPE),
        ptr::addr_of_mut!(TIMEDELTA_CAPI_TYPE),
        ptr::addr_of_mut!(TZINFO_CAPI_TYPE),
        ptr::addr_of_mut!(TIMEZONE_CAPI_TYPE),
    ]
}

static TYPES_READY: Once = Once::new();

/// Addresses of type objects this module owns outside the exported builtin
/// table: heap types it allocated (`PyExc_*` classes, `PyType_FromSpec`) and
/// C static types it adopted through `PyType_Ready`.
///
/// `PyType_Ready` must not re-adopt them as statically declared C types (their
/// role is already correct), and an adopted static type is a valid metatype
/// for another static type, as in `Py_SET_TYPE(&PyArrayDescr_Type,
/// &PyArrayDTypeMeta_Type)`.
static INTERNAL_TYPE_ADDRESSES: LazyLock<Mutex<HashSet<usize>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn remember_internal_type(type_object: *mut TypeObject) {
    if !type_object.is_null() {
        INTERNAL_TYPE_ADDRESSES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(type_object as usize);
    }
}

fn is_internal_type(type_object: *mut TypeObject) -> bool {
    INTERNAL_TYPE_ADDRESSES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(&(type_object as usize))
}

/// `object.__init__`: accepts any arguments and does nothing.
extern "C" fn object_init(
    _self: *mut ShimObject,
    _args: *mut ShimObject,
    _kwds: *mut ShimObject,
) -> c_int {
    0
}

/// Patches the metatype links once. Called lazily by every function that reads
/// a type pointer, and eagerly by [`super::anchor`].
pub(crate) fn ensure_builtin_types() {
    TYPES_READY.call_once(|| unsafe {
        let metatype = ptr::addr_of_mut!(TYPE_TYPE);
        for pointer in builtin_type_pointers() {
            (*pointer).ob_type = metatype;
        }
        (*metatype).ob_type = metatype;
        let object = ptr::addr_of_mut!(OBJECT_TYPE);
        (*metatype).tp_base = object;
        for pointer in builtin_type_pointers() {
            if pointer != metatype && pointer != object {
                (*pointer).tp_base = object;
            }
            (*pointer).tp_flags |= PY_TPFLAGS_READY;
        }
        (*object).tp_alloc = PyType_GenericAlloc as *const () as usize;
        (*object).tp_free = instance_free as *const () as usize;
        // `object` supplies the defaults every heap type inherits: generic
        // allocation/construction and the descriptor-aware attribute
        // protocol implemented by `PyObject_Generic{Get,Set}Attr`.
        (*object).tp_new = PyType_GenericNew as *const () as usize;
        (*object).tp_init = object_init as *const () as usize;
        (*object).tp_getattro = super::PyObject_GenericGetAttr as *const () as usize;
        (*object).tp_setattro = super::PyObject_GenericSetAttr as *const () as usize;
        let bytes_procs: &'static PyBufferProcs = &super::buffer::BYTES_BUFFER_PROCS;
        (*ptr::addr_of_mut!(BYTES_TYPE)).tp_as_buffer =
            bytes_procs as *const PyBufferProcs as *mut PyBufferProcs;
        let bytearray_procs: &'static PyBufferProcs = &super::buffer::BYTEARRAY_BUFFER_PROCS;
        (*ptr::addr_of_mut!(BYTEARRAY_TYPE)).tp_as_buffer =
            bytearray_procs as *const PyBufferProcs as *mut PyBufferProcs;
        // `datetime.datetime` is a subclass of `datetime.date` (as in CPython),
        // so `PyDate_Check` accepts datetime handles.
        (*ptr::addr_of_mut!(DATETIME_CAPI_TYPE)).tp_base = ptr::addr_of_mut!(DATE_CAPI_TYPE);
        (*ptr::addr_of_mut!(TIMEZONE_CAPI_TYPE)).tp_base = ptr::addr_of_mut!(TZINFO_CAPI_TYPE);
        // PyO3 allows `#[pyclass(extends=PyTzInfo)]` (pydantic-core's datetime
        // types) and fetches the base constructor through `PyType_GetSlot`.
        for pointer in [
            ptr::addr_of_mut!(DATE_CAPI_TYPE),
            ptr::addr_of_mut!(DATETIME_CAPI_TYPE),
            ptr::addr_of_mut!(TIME_CAPI_TYPE),
            ptr::addr_of_mut!(TIMEDELTA_CAPI_TYPE),
            ptr::addr_of_mut!(TZINFO_CAPI_TYPE),
            ptr::addr_of_mut!(TIMEZONE_CAPI_TYPE),
        ] {
            (*pointer).tp_new = native_base_new as *const () as usize;
            (*pointer).tp_free = instance_free as *const () as usize;
        }
        let seq_iterator = ptr::addr_of_mut!(SEQITER_TYPE);
        (*seq_iterator).tp_iter = super::PyObject_SelfIter as *const () as usize;
        (*seq_iterator).tp_iternext = super::seqiter_next as *const () as usize;
        (*ptr::addr_of_mut!(MEMORYVIEW_TYPE)).tp_as_buffer = super::memoryview_buffer_procs();
        super::patch_constant_types();
    });
}

/// True when `object` is a type object (its `ob_type` is the shared metatype).
pub(crate) fn is_type_pointer(object: *mut ShimObject) -> bool {
    if object.is_null()
        || (object as usize) < std::mem::align_of::<ShimObject>()
        || !(object as usize).is_multiple_of(std::mem::align_of::<ShimObject>())
    {
        return false;
    }
    ensure_builtin_types();
    let candidate = object as *mut TypeObject;
    // SAFETY: every object this host hands out starts with `PyObject_HEAD`, so
    // `ob_type` is readable. A random pointer is as unsupported here as it is
    // in CPython.
    unsafe { (*candidate).ob_type == ptr::addr_of_mut!(TYPE_TYPE) }
}

/// Validates that `object` is a type object and returns it.
pub(crate) fn as_type(object: *mut ShimObject) -> Result<*mut TypeObject, String> {
    if object.is_null() {
        return Err("TypeError: expected a type object, got NULL".to_string());
    }
    if !is_type_pointer(object) {
        return Err("TypeError: expected a type object".to_string());
    }
    Ok(object as *mut TypeObject)
}

/// Reads the type pointer of an object after ensuring the builtin table.
pub(crate) fn type_of(object: *mut ShimObject) -> *mut TypeObject {
    ensure_builtin_types();
    if object.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: see `is_type_pointer`.
    unsafe { (*object).head.ob_type }
}

/// Reads a type's `tp_name` as a Rust string.
pub(crate) fn type_name(type_object: *mut TypeObject) -> String {
    if type_object.is_null() {
        return "<unknown>".to_string();
    }
    // SAFETY: `type_object` is one of ours; `tp_name` is NUL-terminated.
    unsafe { (*type_object).name() }
}

/// Reads a type's short name.
pub(crate) fn type_short_name(type_object: *mut TypeObject) -> String {
    if type_object.is_null() {
        return "<unknown>".to_string();
    }
    // SAFETY: see `type_name`.
    unsafe { (*type_object).short_name() }
}

/// True when the type opts into the cyclic GC protocol.
pub(crate) fn type_uses_gc(type_object: *mut TypeObject) -> bool {
    if type_object.is_null() {
        return false;
    }
    // SAFETY: type pointers passed in are validated by callers.
    unsafe { (*type_object).uses_gc() }
}

// ---------------------------------------------------------------------------
// Type spec parsing
// ---------------------------------------------------------------------------

/// Names a `PyType_Slot` identifier, for diagnostics.
fn slot_name(slot: c_int) -> &'static str {
    match slot {
        1 => "Py_bf_getbuffer",
        2 => "Py_bf_releasebuffer",
        3 => "Py_mp_ass_subscript",
        4 => "Py_mp_length",
        5 => "Py_mp_subscript",
        6 => "Py_nb_absolute",
        7 => "Py_nb_add",
        8 => "Py_nb_and",
        9 => "Py_nb_bool",
        10 => "Py_nb_divmod",
        11 => "Py_nb_float",
        12 => "Py_nb_floor_divide",
        13 => "Py_nb_index",
        14 => "Py_nb_inplace_add",
        15 => "Py_nb_inplace_and",
        16 => "Py_nb_inplace_floor_divide",
        17 => "Py_nb_inplace_lshift",
        18 => "Py_nb_inplace_multiply",
        19 => "Py_nb_inplace_or",
        20 => "Py_nb_inplace_power",
        21 => "Py_nb_inplace_remainder",
        22 => "Py_nb_inplace_rshift",
        23 => "Py_nb_inplace_subtract",
        24 => "Py_nb_inplace_true_divide",
        25 => "Py_nb_inplace_xor",
        26 => "Py_nb_int",
        27 => "Py_nb_invert",
        28 => "Py_nb_lshift",
        29 => "Py_nb_multiply",
        30 => "Py_nb_negative",
        31 => "Py_nb_or",
        32 => "Py_nb_positive",
        33 => "Py_nb_power",
        34 => "Py_nb_remainder",
        35 => "Py_nb_rshift",
        36 => "Py_nb_subtract",
        37 => "Py_nb_true_divide",
        38 => "Py_nb_xor",
        39 => "Py_sq_ass_item",
        40 => "Py_sq_concat",
        41 => "Py_sq_contains",
        42 => "Py_sq_inplace_concat",
        43 => "Py_sq_inplace_repeat",
        44 => "Py_sq_item",
        45 => "Py_sq_length",
        46 => "Py_sq_repeat",
        47 => "Py_tp_alloc",
        48 => "Py_tp_base",
        49 => "Py_tp_bases",
        50 => "Py_tp_call",
        51 => "Py_tp_clear",
        52 => "Py_tp_dealloc",
        53 => "Py_tp_del",
        54 => "Py_tp_descr_get",
        55 => "Py_tp_descr_set",
        56 => "Py_tp_doc",
        57 => "Py_tp_getattr",
        58 => "Py_tp_getattro",
        59 => "Py_tp_hash",
        60 => "Py_tp_init",
        61 => "Py_tp_is_gc",
        62 => "Py_tp_iter",
        63 => "Py_tp_iternext",
        64 => "Py_tp_methods",
        65 => "Py_tp_new",
        66 => "Py_tp_repr",
        67 => "Py_tp_richcompare",
        68 => "Py_tp_setattr",
        69 => "Py_tp_setattro",
        70 => "Py_tp_str",
        71 => "Py_tp_traverse",
        72 => "Py_tp_members",
        73 => "Py_tp_getset",
        74 => "Py_tp_free",
        75 => "Py_nb_matrix_multiply",
        76 => "Py_nb_inplace_matrix_multiply",
        77 => "Py_am_await",
        78 => "Py_am_aiter",
        79 => "Py_am_anext",
        80 => "Py_tp_finalize",
        81 => "Py_am_send",
        82 => "Py_tp_vectorcall",
        83 => "Py_tp_token",
        _ => "unknown slot",
    }
}

/// Validated slot table, before it is copied into a type object.
struct SlotSet {
    getbuffer: usize,
    releasebuffer: usize,
    alloc: usize,
    base: usize,
    bases: usize,
    call: usize,
    clear: usize,
    dealloc: usize,
    del: usize,
    descr_get: usize,
    descr_set: usize,
    doc: usize,
    getattr: usize,
    getattro: usize,
    getset: usize,
    hash: usize,
    init: usize,
    is_gc: usize,
    iter: usize,
    iternext: usize,
    members: usize,
    methods: usize,
    new: usize,
    repr: usize,
    richcompare: usize,
    setattr: usize,
    setattro: usize,
    str_: usize,
    traverse: usize,
    free: usize,
    finalize: usize,
    vectorcall: usize,
    number: [usize; 36],
    sequence: [usize; 10],
    mapping: [usize; 3],
    has_number: bool,
    has_sequence: bool,
    has_mapping: bool,
}

impl Default for SlotSet {
    fn default() -> Self {
        Self {
            getbuffer: 0,
            releasebuffer: 0,
            alloc: 0,
            base: 0,
            bases: 0,
            call: 0,
            clear: 0,
            dealloc: 0,
            del: 0,
            descr_get: 0,
            descr_set: 0,
            doc: 0,
            getattr: 0,
            getattro: 0,
            getset: 0,
            hash: 0,
            init: 0,
            is_gc: 0,
            iter: 0,
            iternext: 0,
            members: 0,
            methods: 0,
            new: 0,
            repr: 0,
            richcompare: 0,
            setattr: 0,
            setattro: 0,
            str_: 0,
            traverse: 0,
            free: 0,
            finalize: 0,
            vectorcall: 0,
            number: [0; 36],
            sequence: [0; 10],
            mapping: [0; 3],
            has_number: false,
            has_sequence: false,
            has_mapping: false,
        }
    }
}

/// Reads a `PyType_Spec` slot table into a [`SlotSet`].
fn read_slots(slots: *mut PyTypeSlotMirror, name: &str) -> Result<SlotSet, String> {
    let mut set = SlotSet::default();
    let mut cursor = slots;
    let mut terminated = false;
    for _ in 0..MAX_TYPE_SLOTS {
        // SAFETY: `slots` points at a NULL-terminated array of PyType_Slot.
        let slot = unsafe { &*cursor };
        if slot.slot == 0 {
            terminated = true;
            break;
        }
        let identifier = slot.slot;
        let value = slot.pfunc as usize;
        if let Some(index) = number_slot_index(identifier) {
            set.number[index] = value;
            set.has_number = true;
        } else if let Some(index) = sequence_slot_index(identifier) {
            set.sequence[index] = value;
            set.has_sequence = true;
        } else if let Some(index) = mapping_slot_index(identifier) {
            set.mapping[index] = value;
            set.has_mapping = true;
        } else {
            match identifier {
                1 => set.getbuffer = value,
                2 => set.releasebuffer = value,
                47 => set.alloc = value,
                48 => set.base = value,
                49 => set.bases = value,
                50 => set.call = value,
                51 => set.clear = value,
                52 => set.dealloc = value,
                53 => set.del = value,
                54 => set.descr_get = value,
                55 => set.descr_set = value,
                56 => {
                    if set.doc != 0 {
                        return Err(format!(
                            "SystemError: PyType_FromSpec: type '{name}' has multiple Py_tp_doc \
                             slots"
                        ));
                    }
                    set.doc = value;
                }
                57 => set.getattr = value,
                58 => set.getattro = value,
                59 => set.hash = value,
                60 => set.init = value,
                61 => set.is_gc = value,
                62 => set.iter = value,
                63 => set.iternext = value,
                64 => set.methods = value,
                65 => set.new = value,
                66 => set.repr = value,
                67 => set.richcompare = value,
                68 => set.setattr = value,
                69 => set.setattro = value,
                70 => set.str_ = value,
                71 => set.traverse = value,
                72 => set.members = value,
                73 => set.getset = value,
                74 => set.free = value,
                80 => set.finalize = value,
                82 => set.vectorcall = value,
                other => {
                    let kind = if !(0..=83).contains(&other) {
                        format!("invalid type slot {other}")
                    } else {
                        format!("unsupported type slot {other} ({})", slot_name(other))
                    };
                    return Err(format!(
                        "TypeError: PyType_FromSpec: type '{name}' uses {kind}"
                    ));
                }
            }
        }
        // SAFETY: advancing one entry inside the NULL-terminated table.
        cursor = unsafe { cursor.add(1) };
    }
    if !terminated {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' has more than {MAX_TYPE_SLOTS} slots \
             without a zero terminator"
        ));
    }
    Ok(set)
}

/// Appends the type objects referenced by `bases` to `out`.
///
/// `bases` may be a single type object or a tuple of type objects, matching
/// CPython's `PyType_FromSpecWithBases` contract.
fn collect_bases(bases: *mut ShimObject, out: &mut Vec<*mut TypeObject>) -> Result<(), String> {
    use super::kind_ref;

    if bases.is_null() {
        return Ok(());
    }
    if is_type_pointer(bases) {
        out.push(bases as *mut TypeObject);
        return Ok(());
    }
    match kind_ref(bases) {
        Some(super::ShimKind::Tuple(items)) => {
            for &item in items {
                if item == 0 {
                    return Err(
                        "TypeError: PyType_FromSpecWithBases: bases tuple has an uninitialized item"
                            .to_string(),
                    );
                }
                out.push(as_type(item as *mut ShimObject)?);
            }
            Ok(())
        }
        _ => Err(
            "TypeError: PyType_FromSpecWithBases: bases must be a type or a tuple of types"
                .to_string(),
        ),
    }
}

/// Validates a NULL-terminated `PyGetSetDef` table and returns its entries.
fn read_getset_table(
    table: *mut PyGetSetDefMirror,
    owner: &str,
) -> Result<Vec<*mut PyGetSetDefMirror>, String> {
    let mut entries = Vec::new();
    if table.is_null() {
        return Ok(entries);
    }
    let mut cursor = table;
    for _ in 0..MAX_TYPE_SLOTS * 64 {
        // SAFETY: the extension owns a NULL-terminated PyGetSetDef array.
        let entry = unsafe { &*cursor };
        if entry.name.is_null() {
            return Ok(entries);
        }
        if entry.get == 0 && entry.set == 0 {
            return Err(format!(
                "TypeError: {owner} getset '{}' has neither a getter nor a setter",
                read_c_string(entry.name)
            ));
        }
        entries.push(cursor);
        // SAFETY: advancing one entry inside the terminated table.
        cursor = unsafe { cursor.add(1) };
    }
    Err(format!(
        "{owner} has more than {} getset entries without a zero terminator",
        MAX_TYPE_SLOTS * 64
    ))
}

/// Validates a NULL-terminated `PyMemberDef` table and returns its entries.
fn read_member_table(
    table: *mut PyMemberDefMirror,
    owner: &str,
) -> Result<Vec<*mut PyMemberDefMirror>, String> {
    let mut entries = Vec::new();
    if table.is_null() {
        return Ok(entries);
    }
    let mut cursor = table;
    for _ in 0..MAX_TYPE_SLOTS * 64 {
        // SAFETY: the extension owns a NULL-terminated PyMemberDef array.
        let entry = unsafe { &*cursor };
        if entry.name.is_null() {
            return Ok(entries);
        }
        if !matches!(entry.type_, 0..=14 | 16..=20) {
            return Err(format!(
                "TypeError: {owner} member '{}' has unsupported type {}",
                read_c_string(entry.name),
                entry.type_
            ));
        }
        if entry.offset < 0 {
            return Err(format!(
                "TypeError: {owner} member '{}' has a negative offset {}",
                read_c_string(entry.name),
                entry.offset
            ));
        }
        entries.push(cursor);
        // SAFETY: advancing one entry inside the terminated table.
        cursor = unsafe { cursor.add(1) };
    }
    Err(format!(
        "{owner} has more than {} member entries without a zero terminator",
        MAX_TYPE_SLOTS * 64
    ))
}

/// Method suites owned by one heap type. `tp_as_number` points at the first
/// field, which is also the allocation address registered in [`suite_owners`].
#[repr(C)]
struct MethodSuites {
    number: PyNumberMethods,
    sequence: PySequenceMethods,
    mapping: PyMappingMethods,
}

impl Default for MethodSuites {
    fn default() -> Self {
        Self {
            number: PyNumberMethods { slots: [0; 36] },
            sequence: PySequenceMethods { slots: [0; 10] },
            mapping: PyMappingMethods { slots: [0; 3] },
        }
    }
}

/// Allocations owned by heap types (method suites). Kept in a side table
/// because `PyTypeObject` has no room past CPython's `tp_versions_used`.
fn suite_owners() -> MutexGuard<'static, HashMap<usize, usize>> {
    // suite address -> owning type address (0 while the type is still a stack
    // value before `Box::into_raw`).
    static OWNERS: LazyLock<Mutex<HashMap<usize, usize>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    OWNERS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Returns the method-resolution order of `type_object`, starting with the
/// type itself.
///
/// Heap types store the merged bases in `tp_mro` (without the type, avoiding a
/// reference-count cycle); static builtins fall back to the `tp_base` chain.
pub(crate) fn mro_of(type_object: *mut TypeObject) -> Vec<*mut TypeObject> {
    let mut order = Vec::new();
    if type_object.is_null() {
        return order;
    }
    order.push(type_object);
    // SAFETY: live type object.
    unsafe {
        let stored = (*type_object).tp_mro as *mut ShimObject;
        if !stored.is_null() {
            if let Some(super::ShimKind::Tuple(items)) = super::kind_ref(stored) {
                for &item in items {
                    if item != 0 {
                        order.push(item as *mut TypeObject);
                    }
                }
                return order;
            }
        }
        let mut current = (*type_object).tp_base;
        for _ in 0..64 {
            if current.is_null() {
                break;
            }
            order.push(current);
            current = (*current).tp_base;
        }
    }
    order
}

/// C3 linearisation of `bases` (everything after the new type in its MRO).
fn linearize_bases(bases: &[*mut TypeObject]) -> Result<Vec<*mut TypeObject>, String> {
    if bases.is_empty() {
        return Ok(Vec::new());
    }
    let mut sequences: Vec<Vec<usize>> = Vec::with_capacity(bases.len() + 1);
    for &base in bases {
        sequences.push(
            mro_of(base)
                .into_iter()
                .map(|pointer| pointer as usize)
                .collect(),
        );
    }
    sequences.push(bases.iter().map(|&base| base as usize).collect());

    let names = || {
        bases
            .iter()
            .map(|&base| format!("'{}'", type_name(base)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut merged = Vec::new();
    loop {
        sequences.retain(|sequence| !sequence.is_empty());
        if sequences.is_empty() {
            break;
        }
        let mut candidate = None;
        for sequence in &sequences {
            let head = sequence[0];
            let in_tail = sequences
                .iter()
                .any(|other| other.len() > 1 && other[1..].contains(&head));
            if !in_tail {
                candidate = Some(head);
                break;
            }
        }
        let Some(candidate) = candidate else {
            return Err(format!(
                "TypeError: Cannot create a consistent method resolution order (MRO) for bases {}",
                names()
            ));
        };
        merged.push(candidate as *mut TypeObject);
        for sequence in &mut sequences {
            sequence.retain(|&entry| entry != candidate);
        }
    }
    Ok(merged)
}

/// Translates a `PyType_Spec` into a real heap type object.
pub(crate) fn type_from_spec(
    spec: *mut PyTypeSpecMirror,
    bases_in: *mut ShimObject,
) -> Result<*mut TypeObject, String> {
    ensure_builtin_types();
    if spec.is_null() {
        return Err("TypeError: PyType_FromSpec: type spec is NULL".to_string());
    }
    // SAFETY: the extension owns a live PyType_Spec for the duration of the
    // call; the mirror matches CPython 3.14 x86_64.
    let spec = unsafe { &*spec };
    if spec.name.is_null() {
        return Err("TypeError: PyType_FromSpec: type spec has no name".to_string());
    }
    let name = read_c_string(spec.name);
    if name.is_empty() {
        return Err("TypeError: PyType_FromSpec: type spec name is empty".to_string());
    }
    if spec.slots.is_null() {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' has no slot table"
        ));
    }

    let requested = spec.flags as c_ulong;
    let unsupported = requested & !(SUPPORTED_TYPE_FLAGS | INVALID_SPEC_FLAGS);
    if unsupported != 0 {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' requests unsupported flags {unsupported:#x}"
        ));
    }
    let invalid = requested & INVALID_SPEC_FLAGS;
    if invalid != 0 {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' requests flags {invalid:#x} that only the \
             interpreter may set"
        ));
    }

    let slots = read_slots(spec.slots, &name)?;

    let mut bases: Vec<*mut TypeObject> = Vec::new();
    if !bases_in.is_null() {
        collect_bases(bases_in, &mut bases)?;
    } else if slots.bases != 0 {
        collect_bases(slots.bases as *mut ShimObject, &mut bases)?;
    } else if slots.base != 0 {
        bases.push(as_type(slots.base as *mut ShimObject)?);
    }
    if bases.is_empty() {
        bases.push(ptr::addr_of_mut!(OBJECT_TYPE));
    }
    // CPython merges identical bases before computing the MRO; a duplicated
    // pointer would otherwise be rejected by C3 as its own tail entry.
    let mut unique: Vec<*mut TypeObject> = Vec::with_capacity(bases.len());
    for base in bases.drain(..) {
        if !unique.contains(&base) {
            unique.push(base);
        }
    }
    let bases = unique;
    for &base in &bases {
        if base.is_null() {
            return Err(format!(
                "TypeError: PyType_FromSpec: type '{name}' has a NULL base"
            ));
        }
        // SAFETY: `base` was validated as a type pointer above.
        let base_role = unsafe { (*base).role };
        let base_flags = unsafe { (*base).tp_flags };
        // Two kinds of Shim base are still legitimate bases for a heap type:
        //
        // * Exception classes carry the CPython exception payload in the Shim
        //   role, and PyO3 creates `#[pyclass(extends = PyException)]` types
        //   from them (`PY_TPFLAGS_BASE_EXC_SUBCLASS`).
        // * The `datetime_CAPI` type objects (`datetime.date`,
        //   `datetime.tzinfo`, ...) are Shim handles too, but CPython declares
        //   them subclassable: `#[pyclass(extends = PyTzInfo)]` must work, so a
        //   Shim base that opts into subclassing via `Py_TPFLAGS_BASETYPE` is
        //   accepted. The new type lives in the Instance role and only shares
        //   the base's `tp_basicsize` lower bound and MRO position.
        let subclassable_shim = base_role == TypeRole::Shim
            && base_flags & (PY_TPFLAGS_BASE_EXC_SUBCLASS | PY_TPFLAGS_BASETYPE) != 0;
        if base_role != TypeRole::Instance && !subclassable_shim {
            return Err(format!(
                "TypeError: PyType_FromSpec: type '{name}' cannot use '{}' as a base (only \
                 instance types can be bases)",
                type_name(base)
            ));
        }
        if base_flags & PY_TPFLAGS_BASETYPE == 0 {
            return Err(format!(
                "TypeError: PyType_FromSpec: type '{}' is not an acceptable base type",
                type_name(base)
            ));
        }
    }

    // Pick CPython's "best base": the solid base with the largest layout. All
    // bases must agree on `tp_itemsize`, and the new type must be at least as
    // large as every base.
    let mut base_size: isize = PY_OBJECT_HEAD_SIZE as isize;
    let mut base_itemsize: isize = 0;
    let mut primary = bases[0];
    for &base in &bases {
        let size = unsafe { (*base).tp_basicsize };
        let itemsize = unsafe { (*base).tp_itemsize };
        if size > base_size {
            base_size = size;
            primary = base;
        }
        if itemsize != 0 {
            if base_itemsize == 0 {
                base_itemsize = itemsize;
            } else if base_itemsize != itemsize {
                return Err(format!(
                    "TypeError: PyType_FromSpec: type '{name}' has bases with incompatible item \
                     sizes ({base_itemsize} and {itemsize})"
                ));
            }
        }
    }
    let base = primary;

    if spec.basicsize < 0 {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' requests a negative basicsize {}, which is \
             not supported",
            spec.basicsize
        ));
    }
    if spec.itemsize < 0 {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' requests a negative itemsize {}",
            spec.itemsize
        ));
    }
    let basicsize = if spec.basicsize == 0 {
        base_size.max(PY_OBJECT_HEAD_SIZE as isize)
    } else {
        spec.basicsize as isize
    };
    if basicsize < PY_OBJECT_HEAD_SIZE as isize {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' has basicsize {basicsize}, smaller than the \
             PyObject header"
        ));
    }
    if basicsize < base_size {
        return Err(format!(
            "TypeError: PyType_FromSpec: basicsize for type '{name}' ({basicsize}) is too small \
             for base '{}' ({base_size})",
            type_name(base)
        ));
    }
    if basicsize > MAX_BASICSIZE {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' has an unsupported basicsize {basicsize}"
        ));
    }
    let itemsize = spec.itemsize as isize;
    if itemsize > MAX_ITEMSIZE {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' has an unsupported itemsize {itemsize}"
        ));
    }
    if itemsize != 0 {
        if basicsize < MIN_VAR_BASICSIZE as isize {
            return Err(format!(
                "TypeError: PyType_FromSpec: variable-size type '{name}' needs a basicsize of at \
                 least {MIN_VAR_BASICSIZE}, got {basicsize}"
            ));
        }
        if base_itemsize != 0 && itemsize != base_itemsize {
            return Err(format!(
                "TypeError: PyType_FromSpec: type '{name}' item size {itemsize} does not match \
                 the base item size {base_itemsize}"
            ));
        }
    } else if base_itemsize != 0 {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' cannot derive a fixed-size type from the \
             variable-size base '{}'",
            type_name(base)
        ));
    }

    // Validate the doc / methods tables before allocating anything.
    let doc = if slots.doc == 0 {
        None
    } else {
        Some(read_c_string(slots.doc as *const c_char))
    };
    if slots.methods != 0 {
        read_method_table(
            slots.methods as *mut PyMethodDefMirror,
            &format!("type '{name}'"),
        )?;
    }
    if slots.getbuffer == 0 && slots.releasebuffer != 0 {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' sets Py_bf_releasebuffer without \
             Py_bf_getbuffer"
        ));
    }
    if slots.getset != 0 {
        read_getset_table(
            slots.getset as *mut PyGetSetDefMirror,
            &format!("type '{name}'"),
        )?;
    }
    if slots.members != 0 {
        read_member_table(
            slots.members as *mut PyMemberDefMirror,
            &format!("type '{name}'"),
        )?;
    }

    // C3 linearisation of the bases. The new type is prepended later when the
    // `__mro__` tuple is built; inheritance below walks this list directly.
    let base_mro = linearize_bases(&bases)?;
    // Exception subclasses inherit CPython's `Py_TPFLAGS_BASE_EXC_SUBCLASS`
    // marker; `PyExceptionClass_Check` (used by PyO3) requires it.
    let inherited_subclass_flags = bases.iter().fold(0, |flags, &base| {
        // SAFETY: every base was validated as a live type pointer above.
        flags | (unsafe { (*base).tp_flags } & PY_TPFLAGS_BUILTIN_SUBCLASSES)
    });

    let name_c = CString::new(name.as_str())
        .map_err(|_| "TypeError: PyType_FromSpec: type name contains a NUL".to_string())?;
    let doc_c = doc
        .map(|doc| {
            CString::new(doc.as_str())
                .map_err(|_| "TypeError: PyType_FromSpec: type doc contains a NUL".to_string())
        })
        .transpose()?;

    let mut type_object = TypeObject::builtin(
        ptr::null(),
        TypeRole::Instance,
        requested | PY_TPFLAGS_HEAPTYPE,
    );
    // `builtin` marks the object immortal; heap types use a real reference
    // count so `Py_DecRef` can release them.
    type_object.ob_refcnt = AtomicIsize::new(1);
    type_object.ob_type = ptr::addr_of_mut!(TYPE_TYPE);
    type_object.tp_name = name_c.into_raw() as *const c_char;
    type_object.tp_basicsize = basicsize;
    type_object.tp_itemsize = itemsize;
    type_object.tp_doc = doc_c.map_or(ptr::null(), |doc| doc.into_raw() as *const c_char);
    type_object.owns_name = true;
    type_object.tp_flags |= inherited_subclass_flags;
    type_object.tp_methods = slots.methods as *mut PyMethodDefMirror;
    type_object.tp_members = slots.members as *mut c_void;
    type_object.tp_getset = slots.getset as *mut c_void;
    type_object.tp_alloc = slots.alloc;
    type_object.tp_call = slots.call;
    type_object.tp_clear = slots.clear;
    type_object.tp_dealloc = slots.dealloc;
    type_object.tp_del = slots.del;
    type_object.tp_descr_get = slots.descr_get;
    type_object.tp_descr_set = slots.descr_set;
    type_object.tp_hash = slots.hash;
    type_object.tp_init = slots.init;
    type_object.tp_is_gc = slots.is_gc;
    type_object.tp_iter = slots.iter;
    type_object.tp_iternext = slots.iternext;
    type_object.tp_new = slots.new;
    type_object.tp_repr = slots.repr;
    type_object.tp_richcompare = slots.richcompare;
    type_object.tp_str = slots.str_;
    type_object.tp_traverse = slots.traverse;
    type_object.tp_free = slots.free;
    type_object.tp_finalize = slots.finalize;
    type_object.tp_vectorcall = slots.vectorcall;
    type_object.tp_getattro = slots.getattro;
    type_object.tp_setattro = slots.setattro;
    type_object.tp_getattr = slots.getattr;
    type_object.tp_setattr = slots.setattr;
    // Keep the type alive while it owns the arrays; the box below transfers
    // ownership to `free_type_object`.
    type_object.tp_base = base;
    inc_ref(base as *mut ShimObject);

    if slots.getbuffer != 0 || slots.releasebuffer != 0 {
        let procs = Box::new(PyBufferProcs {
            bf_getbuffer: slots.getbuffer,
            bf_releasebuffer: slots.releasebuffer,
        });
        type_object.tp_as_buffer = Box::into_raw(procs);
        type_object.owns_buffer = true;
    }

    // Number/sequence/mapping suites: start from the primary base's arrays and
    // overlay every slot the spec defines, then let the shared inheritance
    // below fill categories the spec does not define at all.
    let mut allocated_suite: Option<usize> = None;
    if slots.has_number || slots.has_sequence || slots.has_mapping {
        let mut suites = Box::new(MethodSuites::default());
        // Seed each suite from the first type in the MRO that defines it, so a
        // mixed set of bases contributes the same protocols CPython's MRO
        // lookup would find.
        unsafe {
            for &ancestor in &base_mro {
                if !(*ancestor).tp_as_number.is_null() {
                    suites.number = *((*ancestor).tp_as_number as *const PyNumberMethods);
                    break;
                }
            }
            for &ancestor in &base_mro {
                if !(*ancestor).tp_as_sequence.is_null() {
                    suites.sequence = *((*ancestor).tp_as_sequence as *const PySequenceMethods);
                    break;
                }
            }
            for &ancestor in &base_mro {
                if !(*ancestor).tp_as_mapping.is_null() {
                    suites.mapping = *((*ancestor).tp_as_mapping as *const PyMappingMethods);
                    break;
                }
            }
        }
        for (index, &value) in slots.number.iter().enumerate() {
            if value != 0 {
                suites.number.slots[index] = value;
            }
        }
        for (index, &value) in slots.sequence.iter().enumerate() {
            if value != 0 {
                suites.sequence.slots[index] = value;
            }
        }
        for (index, &value) in slots.mapping.iter().enumerate() {
            if value != 0 {
                suites.mapping.slots[index] = value;
            }
        }
        let address = Box::into_raw(suites);
        suite_owners().insert(address as usize, 0);
        allocated_suite = Some(address as usize);
        // Point all three suites at the allocation: each array starts as the
        // primary base's table with the spec's slots overlaid, so an always
        // non-NULL pointer is behaviourally identical and lets the destructor
        // find the allocation through `tp_as_number`.
        unsafe {
            type_object.tp_as_number = ptr::addr_of_mut!((*address).number) as *mut c_void;
            type_object.tp_as_sequence = ptr::addr_of_mut!((*address).sequence) as *mut c_void;
            type_object.tp_as_mapping = ptr::addr_of_mut!((*address).mapping) as *mut c_void;
        }
    }

    // Inherit unset operations from the first type in the MRO that defines
    // them, matching CPython's `inherit_slots` for the supported slots.
    macro_rules! inherit_value {
        ($field:ident) => {
            if type_object.$field == 0 {
                for &ancestor in &base_mro {
                    // SAFETY: every MRO entry is a live type object.
                    let candidate = unsafe { (*ancestor).$field };
                    if candidate != 0 {
                        type_object.$field = candidate;
                        break;
                    }
                }
            }
        };
    }
    macro_rules! inherit_pointer {
        ($field:ident) => {
            if type_object.$field.is_null() {
                for &ancestor in &base_mro {
                    // SAFETY: as above.
                    let candidate = unsafe { (*ancestor).$field };
                    if !candidate.is_null() {
                        type_object.$field = candidate;
                        break;
                    }
                }
            }
        };
    }

    inherit_value!(tp_dealloc);
    inherit_value!(tp_free);
    inherit_value!(tp_init);
    inherit_value!(tp_new);
    inherit_value!(tp_call);
    inherit_value!(tp_getattro);
    inherit_value!(tp_setattro);
    inherit_value!(tp_getattr);
    inherit_value!(tp_setattr);
    inherit_value!(tp_repr);
    inherit_value!(tp_str);
    inherit_value!(tp_hash);
    inherit_value!(tp_traverse);
    inherit_value!(tp_clear);
    inherit_value!(tp_richcompare);
    inherit_value!(tp_is_gc);
    inherit_value!(tp_finalize);
    inherit_value!(tp_del);
    inherit_value!(tp_iter);
    inherit_value!(tp_iternext);
    inherit_value!(tp_descr_get);
    inherit_value!(tp_descr_set);
    inherit_value!(tp_vectorcall);
    inherit_value!(tp_alloc);
    inherit_value!(tp_weaklistoffset);
    inherit_value!(tp_dictoffset);
    inherit_pointer!(tp_as_number);
    inherit_pointer!(tp_as_sequence);
    inherit_pointer!(tp_as_mapping);
    inherit_pointer!(tp_as_buffer);
    if type_object.tp_flags & PY_TPFLAGS_HAVE_VECTORCALL == 0
        && type_object.tp_vectorcall_offset == 0
    {
        for &ancestor in &base_mro {
            // SAFETY: every MRO entry is a live type object.
            unsafe {
                if (*ancestor).tp_flags & PY_TPFLAGS_HAVE_VECTORCALL != 0
                    && (*ancestor).tp_vectorcall_offset != 0
                {
                    type_object.tp_vectorcall_offset = (*ancestor).tp_vectorcall_offset;
                    break;
                }
            }
        }
    }
    // A type that sets a vectorcall function without the flag bit is still
    // callable through `tp_call`; CPython requires the flag, so add it for a
    // type that supplies the slot.
    if type_object.tp_vectorcall != 0 {
        type_object.tp_flags |= PY_TPFLAGS_HAVE_VECTORCALL;
    }

    // `tp_bases` and `tp_mro` are strong references like CPython's heap-type
    // tuples. `tp_mro` stores the merged bases (everything after the type
    // itself); `mro_of` re-prepends the type, which avoids a refcount cycle.
    let bases_tuple = match type_tuple(&bases) {
        Ok(tuple) => tuple,
        Err(error) => {
            free_type_contents(&mut type_object);
            return Err(error);
        }
    };
    type_object.tp_bases = bases_tuple as *mut c_void;
    let mro_tuple = match type_tuple(&base_mro) {
        Ok(tuple) => tuple,
        Err(error) => {
            type_object.tp_bases = ptr::null_mut();
            dec_ref(bases_tuple);
            free_type_contents(&mut type_object);
            return Err(error);
        }
    };
    type_object.tp_mro = mro_tuple as *mut c_void;
    let dict = super::PyDict_New();
    if dict.is_null() {
        free_type_contents(&mut type_object);
        return Err(super::take_error()
            .unwrap_or_else(|| "MemoryError: cannot allocate type dictionary".to_string()));
    }
    type_object.tp_dict = dict as *mut c_void;

    let pointer = Box::into_raw(Box::new(type_object));
    if let Some(address) = allocated_suite {
        if let Some(owner) = suite_owners().get_mut(&address) {
            *owner = pointer as usize;
        }
    }
    if let Err(error) = ensure_ready(pointer) {
        // `ensure_ready` only fails for impossible values here; release the
        // type and report the error rather than returning a half-built type.
        free_type_object(pointer);
        return Err(error);
    }
    Ok(pointer)
}

/// Builds a tuple of type objects, taking a new reference to each.
fn type_tuple(values: &[*mut TypeObject]) -> Result<*mut ShimObject, String> {
    let tuple = super::PyTuple_New(values.len() as isize);
    if tuple.is_null() {
        return Err(super::take_error()
            .unwrap_or_else(|| "MemoryError: cannot create a type tuple".to_string()));
    }
    for (index, &value) in values.iter().enumerate() {
        inc_ref(value as *mut ShimObject);
        if super::PyTuple_SetItem(tuple, index as isize, value as *mut ShimObject) != 0 {
            dec_ref(value as *mut ShimObject);
            dec_ref(tuple);
            return Err(super::take_error()
                .unwrap_or_else(|| "TypeError: cannot fill a type tuple".to_string()));
        }
    }
    Ok(tuple)
}

/// Drops a method-suite allocation owned by this type, if any.
fn free_method_suites(type_object: &mut TypeObject) {
    let address = type_object.tp_as_number as usize;
    if address != 0 {
        let mut owners = suite_owners();
        // Owner 0 means the type never reached the heap, so this stack value
        // still owns the suite. Boxed types free theirs in
        // `free_type_object`, which knows the type's address.
        if owners.get(&address) == Some(&0) {
            owners.remove(&address);
            // SAFETY: `address` is the base of a `MethodSuites` box created by
            // `type_from_spec` and registered in `suite_owners`.
            unsafe { drop(Box::from_raw(address as *mut MethodSuites)) };
        }
    }
    type_object.tp_as_number = ptr::null_mut();
    type_object.tp_as_sequence = ptr::null_mut();
    type_object.tp_as_mapping = ptr::null_mut();
}

/// Releases the owned buffers of a `TypeObject` that never reached the heap.
fn free_type_contents(type_object: &mut TypeObject) {
    unsafe {
        free_method_suites(type_object);
        if !type_object.tp_name.is_null() {
            drop(CString::from_raw(type_object.tp_name as *mut c_char));
            type_object.tp_name = ptr::null();
        }
        if !type_object.tp_doc.is_null() {
            drop(CString::from_raw(type_object.tp_doc as *mut c_char));
            type_object.tp_doc = ptr::null();
        }
        if !type_object.tp_as_buffer.is_null() && type_object.owns_buffer {
            drop(Box::from_raw(type_object.tp_as_buffer));
            type_object.tp_as_buffer = ptr::null_mut();
        }
        if !type_object.tp_bases.is_null() {
            dec_ref(type_object.tp_bases as *mut ShimObject);
            type_object.tp_bases = ptr::null_mut();
        }
        if !type_object.tp_mro.is_null() {
            dec_ref(type_object.tp_mro as *mut ShimObject);
            type_object.tp_mro = ptr::null_mut();
        }
        if !type_object.tp_dict.is_null() {
            dec_ref(type_object.tp_dict as *mut ShimObject);
            type_object.tp_dict = ptr::null_mut();
        }
        if !type_object.tp_base.is_null() {
            dec_ref(type_object.tp_base as *mut ShimObject);
            type_object.tp_base = ptr::null_mut();
        }
    }
}

/// Frees a heap type object. Static types are immortal and never reach this.
pub(crate) fn free_type_object(type_object: *mut TypeObject) {
    if type_object.is_null() {
        return;
    }
    // SAFETY: called from `dec_ref` when the reference count reached zero.
    unsafe {
        if (*type_object).ob_refcnt.load(Ordering::Relaxed) == IMMORTAL_REFCNT {
            return;
        }
        let bases = (*type_object).tp_bases;
        let base = (*type_object).tp_base;
        let mro = (*type_object).tp_mro;
        let owns_name = (*type_object).owns_name;
        let owns_buffer = (*type_object).owns_buffer;
        let name = (*type_object).tp_name;
        let doc = (*type_object).tp_doc;
        let buffer_procs = (*type_object).tp_as_buffer;
        let suites = (*type_object).tp_as_number as usize;
        let associated_module = type_module_owners()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(type_object as usize));
        if owns_name && !name.is_null() {
            drop(CString::from_raw(name as *mut c_char));
        }
        if owns_name && !doc.is_null() {
            drop(CString::from_raw(doc as *mut c_char));
        }
        if owns_buffer && !buffer_procs.is_null() {
            drop(Box::from_raw(buffer_procs));
        }
        if suites != 0 {
            let mut owners = suite_owners();
            if owners.get(&suites) == Some(&(type_object as usize)) {
                owners.remove(&suites);
                drop(Box::from_raw(suites as *mut MethodSuites));
            }
        }
        let _ = Box::from_raw(type_object);
        // Release the type's strong references after its own memory is gone;
        // `dec_ref` may recursively free types and must not see a dropped box.
        if !mro.is_null() {
            dec_ref(mro as *mut ShimObject);
        }
        if !bases.is_null() {
            dec_ref(bases as *mut ShimObject);
        }
        if !base.is_null() {
            dec_ref(base as *mut ShimObject);
        }
        if let Some(module) = associated_module {
            dec_ref(module as *mut ShimObject);
        }
    }
}

/// Module association for types created by `PyType_FromModuleAndSpec`.
///
/// The association lives in a side table instead of growing `TypeObject`: the
/// public layout is fixed at 416 bytes and extensions may inspect it. The
/// stored module holds one strong reference, released when the heap type is
/// freed (static types never reach `free_type_object`, matching CPython's
/// process-lifetime heap-type module slot).
static TYPE_MODULES: LazyLock<Mutex<HashMap<usize, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn type_module_owners() -> &'static Mutex<HashMap<usize, usize>> {
    &TYPE_MODULES
}

/// Associates `module` with `type_object`, taking a new reference to it.
pub(crate) fn associate_type_module(type_object: *mut TypeObject, module: *mut ShimObject) {
    if type_object.is_null() || module.is_null() {
        return;
    }
    inc_ref(module);
    let replaced = type_module_owners()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(type_object as usize, module as usize);
    if let Some(previous) = replaced {
        dec_ref(previous as *mut ShimObject);
    }
}

/// The module object associated with `type_object`, if any.
pub(crate) fn type_module(type_object: *mut TypeObject) -> Option<*mut ShimObject> {
    if type_object.is_null() {
        return None;
    }
    type_module_owners()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&(type_object as usize))
        .copied()
        .map(|module| module as *mut ShimObject)
}

/// Walks a static type's `tp_base` chain.
///
/// The chain is rejected when it contains a non-type, loops, or is longer than
/// CPython's practical inheritance depth. A valid chain lets `PyType_Ready`
/// inherit the same slots CPython's `inherit_slots` copies for a static type.
fn static_ancestors(type_object: *mut TypeObject) -> Result<Vec<*mut TypeObject>, String> {
    let mut chain: Vec<*mut TypeObject> = Vec::new();
    let mut current = unsafe { (*type_object).tp_base };
    while !current.is_null() {
        if current == type_object || chain.contains(&current) {
            return Err(format!(
                "SystemError: PyType_Ready: type '{}' has a cyclic tp_base chain",
                type_name(type_object)
            ));
        }
        if !is_type_pointer(current as *mut ShimObject) {
            return Err(format!(
                "TypeError: PyType_Ready: type '{}' has an invalid tp_base",
                type_name(type_object)
            ));
        }
        if chain.len() >= 64 {
            return Err(format!(
                "SystemError: PyType_Ready: type '{}' has a tp_base chain that is too deep",
                type_name(type_object)
            ));
        }
        chain.push(current);
        current = unsafe { (*current).tp_base };
    }
    Ok(chain)
}

/// Copies the slots CPython inherits from the base chain into an unset static
/// type. Heap types from `PyType_FromSpec` already ran the equivalent
/// inheritance pass, and builtin types are ready before this runs.
fn inherit_static_slots(type_object: *mut TypeObject, ancestors: &[*mut TypeObject]) {
    macro_rules! inherit_value {
        ($field:ident) => {
            if unsafe { (*type_object).$field } == 0 {
                for &ancestor in ancestors {
                    // SAFETY: every entry was validated by `static_ancestors`.
                    let candidate = unsafe { (*ancestor).$field };
                    if candidate != 0 {
                        unsafe { (*type_object).$field = candidate };
                        break;
                    }
                }
            }
        };
    }
    macro_rules! inherit_pointer {
        ($field:ident) => {
            if unsafe { (*type_object).$field.is_null() } {
                for &ancestor in ancestors {
                    // SAFETY: as above.
                    let candidate = unsafe { (*ancestor).$field };
                    if !candidate.is_null() {
                        unsafe { (*type_object).$field = candidate };
                        break;
                    }
                }
            }
        };
    }
    inherit_value!(tp_dealloc);
    inherit_value!(tp_free);
    inherit_value!(tp_init);
    inherit_value!(tp_new);
    inherit_value!(tp_call);
    inherit_value!(tp_getattro);
    inherit_value!(tp_setattro);
    inherit_value!(tp_getattr);
    inherit_value!(tp_setattr);
    inherit_value!(tp_repr);
    inherit_value!(tp_str);
    inherit_value!(tp_hash);
    inherit_value!(tp_traverse);
    inherit_value!(tp_clear);
    inherit_value!(tp_richcompare);
    inherit_value!(tp_is_gc);
    inherit_value!(tp_finalize);
    inherit_value!(tp_del);
    inherit_value!(tp_iter);
    inherit_value!(tp_iternext);
    inherit_value!(tp_descr_get);
    inherit_value!(tp_descr_set);
    inherit_value!(tp_vectorcall);
    inherit_value!(tp_alloc);
    inherit_value!(tp_weaklistoffset);
    inherit_value!(tp_dictoffset);
    inherit_pointer!(tp_as_number);
    inherit_pointer!(tp_as_sequence);
    inherit_pointer!(tp_as_mapping);
    inherit_pointer!(tp_as_buffer);
    if unsafe { (*type_object).tp_vectorcall_offset == 0 } {
        for &ancestor in ancestors {
            // SAFETY: every entry is a live type object.
            let offset = unsafe { (*ancestor).tp_vectorcall_offset };
            if offset != 0 {
                unsafe { (*type_object).tp_vectorcall_offset = offset };
                break;
            }
        }
    }
}

/// Fills `tp_bases`, `tp_mro` and `tp_dict` for a static type, matching what
/// CPython's `type_ready` publishes for a type declared in C.
fn fill_static_type_metadata(
    type_object: *mut TypeObject,
    ancestors: &[*mut TypeObject],
) -> Result<(), String> {
    let base = unsafe { (*type_object).tp_base };
    let bases_tuple = type_tuple(&[base])?;
    unsafe { (*type_object).tp_bases = bases_tuple as *mut c_void };
    let mut mro: Vec<*mut TypeObject> = Vec::with_capacity(ancestors.len() + 1);
    mro.push(type_object);
    mro.extend_from_slice(ancestors);
    let mro_tuple = match type_tuple(&mro) {
        Ok(tuple) => tuple,
        Err(error) => {
            unsafe {
                (*type_object).tp_bases = ptr::null_mut();
            }
            dec_ref(bases_tuple);
            return Err(error);
        }
    };
    unsafe { (*type_object).tp_mro = mro_tuple as *mut c_void };
    if unsafe { (*type_object).tp_dict.is_null() } {
        let dict = super::PyDict_New();
        if dict.is_null() {
            return Err(super::take_error()
                .unwrap_or_else(|| "MemoryError: cannot allocate type dictionary".to_string()));
        }
        unsafe { (*type_object).tp_dict = dict as *mut c_void };
    }
    Ok(())
}

/// `PyType_Ready` body: validates the layout and fills in defaults.
pub(crate) fn ensure_ready(type_object: *mut TypeObject) -> Result<(), String> {
    ensure_builtin_types();
    if type_object.is_null() {
        return Err("TypeError: PyType_Ready: type is NULL".to_string());
    }
    // SAFETY: callers validate the pointer with `as_type` first, or pass a
    // pointer this module allocated.
    unsafe {
        if (*type_object).tp_flags & PY_TPFLAGS_READY != 0 {
            return Ok(());
        }
        if (*type_object).role == TypeRole::Instance {
            if (*type_object).tp_itemsize < 0 {
                return Err(format!(
                    "TypeError: PyType_Ready: type '{}' has a negative itemsize",
                    (*type_object).name()
                ));
            }
            if (*type_object).tp_basicsize == 0 {
                let base = (*type_object).tp_base;
                (*type_object).tp_basicsize = if base.is_null() {
                    PY_OBJECT_HEAD_SIZE as isize
                } else {
                    (*base).tp_basicsize
                };
            }
            if (*type_object).tp_basicsize < PY_OBJECT_HEAD_SIZE as isize {
                return Err(format!(
                    "TypeError: PyType_Ready: type '{}' has basicsize {} smaller than the PyObject \
                     header",
                    (*type_object).name(),
                    (*type_object).tp_basicsize
                ));
            }
            if (*type_object).tp_itemsize != 0
                && (*type_object).tp_basicsize < MIN_VAR_BASICSIZE as isize
            {
                return Err(format!(
                    "TypeError: PyType_Ready: variable-size type '{}' needs a basicsize of at \
                     least {MIN_VAR_BASICSIZE}",
                    (*type_object).name()
                ));
            }
            if (*type_object).tp_base.is_null() && type_object != ptr::addr_of_mut!(OBJECT_TYPE) {
                (*type_object).tp_base = ptr::addr_of_mut!(OBJECT_TYPE);
                inc_ref((*type_object).tp_base as *mut ShimObject);
            }
            // A static type (no heap-owned `tp_bases` tuple) still receives
            // CPython's slot inheritance, plus the `tp_bases`/`tp_mro`/`tp_dict`
            // metadata every ready type publishes. Heap types completed that
            // pass in `type_from_spec`.
            if (*type_object).tp_bases.is_null() {
                let ancestors = static_ancestors(type_object)?;
                inherit_static_slots(type_object, &ancestors);
                if !(*type_object).tp_base.is_null() {
                    fill_static_type_metadata(type_object, &ancestors)?;
                }
            }
            // CPython rejects a GC type without a traversal function: the
            // collector could not enumerate its references. Both heap and
            // static types must satisfy this after inheritance.
            if (*type_object).tp_flags & PY_TPFLAGS_HAVE_GC != 0 && (*type_object).tp_traverse == 0
            {
                return Err(format!(
                    "SystemError: type '{}' has the Py_TPFLAGS_HAVE_GC flag but has no \
                     tp_traverse function",
                    (*type_object).name()
                ));
            }
            if (*type_object).tp_alloc == 0 {
                let base = (*type_object).tp_base;
                if !base.is_null() && (*base).tp_alloc != 0 {
                    (*type_object).tp_alloc = (*base).tp_alloc;
                } else {
                    (*type_object).tp_alloc = PyType_GenericAlloc as *const () as usize;
                }
            }
            if (*type_object).tp_free == 0 {
                // CPython heap types always have a `tp_free`; extensions use
                // it (through `PyType_GetSlot`) to release their instances.
                (*type_object).tp_free = instance_free as *const () as usize;
            }
            if (*type_object).tp_vectorcall_offset <= 0 && (*type_object).tp_vectorcall == 0 {
                // `Py_TPFLAGS_HAVE_VECTORCALL` only means something when the
                // instance carries a vectorcall function pointer or the type
                // has the (static-type) `tp_vectorcall` callable.
                (*type_object).tp_flags &= !PY_TPFLAGS_HAVE_VECTORCALL;
            }
        }
        (*type_object).tp_flags |= PY_TPFLAGS_READY;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Instance allocation
// ---------------------------------------------------------------------------

/// Frees an extension instance with the C allocator. This is the default
/// `tp_free`, so an extension that calls
/// `PyType_GetSlot(Py_TYPE(self), Py_tp_free)` gets a working destructor.
unsafe extern "C" fn instance_free(pointer: *mut c_void) {
    if !pointer.is_null() {
        libc::free(pointer);
    }
}

/// Frees one instance allocated by this host.
pub(crate) fn free_instance(object: *mut ShimObject) {
    let type_object = type_of(object);
    if type_object.is_null() {
        // SAFETY: the caller owned this allocation.
        unsafe { libc::free(object as *mut c_void) };
        return;
    }
    // SAFETY: every field read here belongs to a live type object.
    unsafe {
        let flags = (*type_object).tp_flags;
        // A GC instance the cycle collector already cleared must not have
        // `tp_clear` run a second time here: CPython runs it exactly once, in
        // the collector. The marker is consumed on every deallocation so an
        // address reused later starts without one.
        let cleared_by_collector = if flags & PY_TPFLAGS_HAVE_GC != 0 {
            gc_untrack(object);
            take_collector_clear_marker(object)
        } else {
            false
        };
        if (*type_object).tp_finalize != 0 {
            let finalize: unsafe extern "C" fn(*mut ShimObject) =
                std::mem::transmute((*type_object).tp_finalize as *const c_void);
            finalize(object);
        }
        if (*type_object).tp_del != 0 {
            let delete: unsafe extern "C" fn(*mut ShimObject) =
                std::mem::transmute((*type_object).tp_del as *const c_void);
            delete(object);
        }
        if (*type_object).tp_dealloc != 0 {
            // A custom deallocator owns the memory (CPython contract). Do not
            // free it again or touch the object afterwards.
            let dealloc: unsafe extern "C" fn(*mut ShimObject) =
                std::mem::transmute((*type_object).tp_dealloc as *const c_void);
            dealloc(object);
            return;
        }
        // Default deallocation of a GC type: run `tp_clear` so every edge the
        // type declared to the collector is dropped exactly once before the
        // storage is released. A type that installs its own `tp_dealloc` owns
        // this duty (CPython contract), so it never reaches this branch.
        if flags & PY_TPFLAGS_HAVE_GC != 0 && (*type_object).tp_clear != 0 && !cleared_by_collector
        {
            let clear: unsafe extern "C" fn(*mut ShimObject) =
                std::mem::transmute((*type_object).tp_clear as *const c_void);
            clear(object);
        }
        if (*type_object).tp_free != 0 {
            let free: unsafe extern "C" fn(*mut c_void) =
                std::mem::transmute((*type_object).tp_free as *const c_void);
            free(object as *mut c_void);
        } else {
            libc::free(object as *mut c_void);
        }
        if flags & PY_TPFLAGS_HEAPTYPE != 0 {
            dec_ref(type_object as *mut ShimObject);
        }
    }
}

/// Allocates and initialises a raw instance, without consulting `tp_alloc`.
pub(crate) fn allocate_instance(type_object: *mut TypeObject, nitems: isize) -> *mut ShimObject {
    let _gil = GilGuard::new();
    ensure_builtin_types();
    if let Err(error) = ensure_ready(type_object) {
        set_error(error);
        return ptr::null_mut();
    }
    if nitems < 0 {
        set_error("ValueError: negative instance item count");
        return ptr::null_mut();
    }
    // SAFETY: `type_object` is a validated, ready type.
    let (basicsize, itemsize, flags, role) = unsafe {
        (
            (*type_object).tp_basicsize,
            (*type_object).tp_itemsize,
            (*type_object).tp_flags,
            (*type_object).role,
        )
    };
    if role != TypeRole::Instance {
        // Static value types (`int`, `bytes`, ...) and metatypes have no raw
        // instance layout here; allocating one would make `ob_type` disagree
        // with the payload and could expose invalid memory.
        set_error(format!(
            "TypeError: cannot allocate instances of '{}' (not an instance type)",
            type_name(type_object)
        ));
        return ptr::null_mut();
    }
    if basicsize < PY_OBJECT_HEAD_SIZE as isize {
        set_error(format!(
            "SystemError: type '{}' has an invalid basicsize {basicsize}",
            type_name(type_object)
        ));
        return ptr::null_mut();
    }
    if flags & PY_TPFLAGS_DISALLOW_INSTANTIATION != 0 {
        set_error(format!(
            "TypeError: cannot create '{}' instances",
            type_name(type_object)
        ));
        return ptr::null_mut();
    }
    let item_bytes = match usize::try_from(itemsize)
        .ok()
        .and_then(|itemsize| itemsize.checked_mul(nitems as usize))
    {
        Some(bytes) => bytes,
        None => {
            set_error("MemoryError: instance size overflow");
            return ptr::null_mut();
        }
    };
    let size = match usize::try_from(basicsize)
        .ok()
        .and_then(|base| base.checked_add(item_bytes))
    {
        Some(size) if (PY_OBJECT_HEAD_SIZE..=MAX_INSTANCE_SIZE).contains(&size) => size,
        _ => {
            set_error("MemoryError: instance size is not supported");
            return ptr::null_mut();
        }
    };
    // SAFETY: `malloc` returns suitably aligned storage or NULL.
    let memory = unsafe { libc::malloc(size) };
    if memory.is_null() {
        set_error("MemoryError: cannot allocate an instance");
        return ptr::null_mut();
    }
    // SAFETY: `memory` is `size` writable bytes.
    unsafe {
        ptr::write_bytes(memory as *mut u8, 0, size);
        let head = memory as *mut super::PyObjectHead;
        (*head).ob_refcnt = AtomicIsize::new(1);
        (*head).ob_type = type_object;
        if itemsize != 0 {
            let var = memory as *mut super::PyVarObjectHead;
            (*var).ob_size = AtomicIsize::new(nitems);
        }
    }
    inc_ref(type_object as *mut ShimObject);
    if flags & PY_TPFLAGS_HAVE_GC != 0 {
        gc_track(memory as *mut ShimObject);
    }
    memory as *mut ShimObject
}

/// Calls a type's `tp_alloc`, defaulting to the generic allocator.
fn call_type_alloc(type_object: *mut TypeObject, nitems: isize) -> *mut ShimObject {
    if let Err(error) = ensure_ready(type_object) {
        set_error(error);
        return ptr::null_mut();
    }
    // SAFETY: the type is ready, so `tp_alloc` is either a known function or
    // supplied by the extension with the CPython `allocfunc` signature.
    let allocator = unsafe { (*type_object).tp_alloc };
    if allocator == 0 || allocator == PyType_GenericAlloc as *const () as usize {
        return allocate_instance(type_object, nitems);
    }
    let allocate: unsafe extern "C" fn(*mut TypeObject, isize) -> *mut ShimObject =
        unsafe { std::mem::transmute(allocator as *const c_void) };
    let _gil = GilGuard::new();
    // SAFETY: `allocfunc` has this exact signature.
    unsafe { allocate(type_object, nitems) }
}

// ---------------------------------------------------------------------------
// Exported object/type API
// ---------------------------------------------------------------------------

/// `PyType_FromSpec`: creates a heap type from a `PyType_Spec`.
#[no_mangle]
pub extern "C" fn PyType_FromSpec(spec: *mut PyTypeSpecMirror) -> *mut ShimObject {
    match type_from_spec(spec, ptr::null_mut()) {
        Ok(type_object) => type_object as *mut ShimObject,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyType_FromSpecWithBases`: creates a heap type with one base.
#[no_mangle]
pub extern "C" fn PyType_FromSpecWithBases(
    spec: *mut PyTypeSpecMirror,
    bases: *mut ShimObject,
) -> *mut ShimObject {
    match type_from_spec(spec, bases) {
        Ok(type_object) => type_object as *mut ShimObject,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// Adopts a statically declared `PyTypeObject` (the `PyVarObject_HEAD_INIT`
/// pattern every hand-written C extension uses).
///
/// Such an object starts with a NULL (or extension-chosen) `ob_type`;
/// CPython's `PyType_Ready` makes it immortal, links the metatype and treats
/// it as a normal static type. An aligned object whose metatype is NULL, this
/// host's `type`, an allocated heap type, or a static type already adopted
/// here, with a readable name and non-negative sizes, is adopted. Every other
/// layout is rejected with a diagnostic instead of being written to, so a
/// builtin instance handle or a corrupt pointer cannot be turned into a type
/// by accident.
fn adopt_static_type(pointer: *mut TypeObject) -> Result<*mut TypeObject, String> {
    if pointer.is_null() {
        return Err("TypeError: PyType_Ready: type is NULL".to_string());
    }
    let address = pointer as usize;
    if address < std::mem::align_of::<TypeObject>()
        || !address.is_multiple_of(std::mem::align_of::<TypeObject>())
    {
        return Err("TypeError: PyType_Ready: type pointer is misaligned".to_string());
    }
    ensure_builtin_types();
    // SAFETY: the caller owns a PyTypeObject-shaped object at this address;
    // every read below is inside the CPython prefix.
    let metatype = unsafe { (*pointer).ob_type };
    // CPython accepts any metatype; this host additionally requires the link
    // to be one it can identify: NULL, its own metatype, an allocated heap
    // type, or a static type an earlier `PyType_Ready` adopted (NumPy points
    // `PyArrayDescr_Type` at its own `PyArrayDTypeMeta_Type` this way). An
    // instance handle's class is a builtin value type, which is not in the
    // registry, so `PyType_Ready(instance)` is still rejected.
    let known_metatype = metatype.is_null()
        || metatype == ptr::addr_of_mut!(TYPE_TYPE)
        || is_internal_type(metatype);
    if !known_metatype {
        return Err(format!(
            "TypeError: PyType_Ready: object at {address:#x} is not a static type object \
             (ob_type {metatype:p} is not a known type object)"
        ));
    }
    let name = unsafe { (*pointer).tp_name };
    if name.is_null() {
        return Err("TypeError: PyType_Ready: static type has a NULL tp_name".to_string());
    }
    let basicsize = unsafe { (*pointer).tp_basicsize };
    let itemsize = unsafe { (*pointer).tp_itemsize };
    if basicsize < 0 || itemsize < 0 {
        return Err(format!(
            "TypeError: PyType_Ready: static type '{}' has negative sizes (basicsize {basicsize}, \
             itemsize {itemsize})",
            // SAFETY: `name` is the extension's NUL-terminated type name.
            unsafe { CStr::from_ptr(name) }.to_string_lossy()
        ));
    }
    // SAFETY: a static PyTypeObject is exactly the CPython prefix; the
    // internal tail lives in the struct's trailing padding, which the
    // extension's declaration also contains (sizeof(PyTypeObject) == 416).
    unsafe {
        (*pointer).ob_type = ptr::addr_of_mut!(TYPE_TYPE);
        (*pointer).role = TypeRole::Instance;
        (*pointer).owns_name = false;
        (*pointer).owns_buffer = false;
        // CPython promotes static types to immortal in `type_ready`; without
        // this a `Py_DecRef(Py_TYPE(obj))` from an extension would free data
        // owned by the shared object.
        (*pointer)
            .ob_refcnt
            .store(IMMORTAL_REFCNT, Ordering::Relaxed);
    }
    // Remember the object so another static type may name it as its metatype.
    remember_internal_type(pointer);
    Ok(pointer)
}

/// Readies `type_object`, first making every `tp_base` in its chain a type
/// object and ready.
///
/// CPython's `PyType_Ready` readies an unready base on demand, and extensions
/// rely on that: NumPy points `numpy.dtypes.Int8DType.tp_base` at
/// `PyArray_IntAbstractDType`, a static type readied later in the same module
/// initialization. A base that is not a type object is still rejected.
fn ready_with_bases(type_object: *mut TypeObject) -> Result<(), String> {
    let mut chain: Vec<*mut TypeObject> = Vec::new();
    let mut current = type_object;
    loop {
        if chain.len() >= 64 {
            return Err(format!(
                "SystemError: PyType_Ready: type '{}' has a tp_base chain that is too deep",
                type_name(type_object)
            ));
        }
        if chain.contains(&current) {
            return Err(format!(
                "SystemError: PyType_Ready: type '{}' has a cyclic tp_base chain",
                type_name(type_object)
            ));
        }
        chain.push(current);
        // SAFETY: `current` is a type object (the argument or an adopted base)
        // and `tp_base` is inside the CPython prefix.
        let base = unsafe { (*current).tp_base };
        if base.is_null() {
            break;
        }
        if !is_type_pointer(base as *mut ShimObject) {
            adopt_static_type(base).map_err(|error| {
                format!(
                    "TypeError: PyType_Ready: type '{}' has an invalid tp_base: {error}",
                    type_name(type_object)
                )
            })?;
        }
        current = base;
    }
    // Ancestors first, so every `static_ancestors` walk sees ready types.
    for &entry in chain.iter().rev() {
        ensure_ready(entry)?;
    }
    Ok(())
}

/// `PyType_Ready`: finalises a heap type from `PyType_FromSpec` or adopts a
/// statically declared C `PyTypeObject`.
#[no_mangle]
pub extern "C" fn PyType_Ready(type_object: *mut ShimObject) -> c_int {
    let pointer = match as_type(type_object) {
        Ok(pointer) => {
            // SAFETY: `as_type` validated the pointer.
            let flags = unsafe { (*pointer).tp_flags };
            let dynamic =
                flags & (PY_TPFLAGS_READY | PY_TPFLAGS_HEAPTYPE) != 0 || is_internal_type(pointer);
            if dynamic {
                pointer
            } else {
                // A static C type whose initialiser already named
                // `&PyType_Type` (instead of NULL) still needs adoption: its
                // tail role is whatever the extension's storage happened to
                // contain.
                match adopt_static_type(pointer) {
                    Ok(type_object) => type_object,
                    Err(error) => {
                        set_error(error);
                        return -1;
                    }
                }
            }
        }
        Err(_) => match adopt_static_type(type_object as *mut TypeObject) {
            Ok(type_object) => type_object,
            Err(error) => {
                set_error(error);
                return -1;
            }
        },
    };
    match ready_with_bases(pointer) {
        Ok(()) => 0,
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

/// `PyType_GetSlot`: returns the function pointer stored in `slot`.
///
/// An unset slot yields NULL without setting an error; an out-of-range slot
/// identifier is a `SystemError`, exactly as CPython's `PyType_GetSlot` does.
#[no_mangle]
pub extern "C" fn PyType_GetSlot(object: *mut ShimObject, slot: c_int) -> *mut c_void {
    // A Hydro class has no C slot suites; like CPython for a type that does
    // not provide the slot, the result is NULL (no exception).
    if super::hydro_tagged(object).is_some() {
        return ptr::null_mut();
    }
    let type_object = match as_type(object) {
        Ok(type_object) => type_object,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    if !(1..=83).contains(&slot) {
        set_error(format!(
            "SystemError: PyType_GetSlot: invalid slot identifier {slot}"
        ));
        return ptr::null_mut();
    }
    // SAFETY: `type_object` is a live type object.
    let pointer = unsafe {
        if let Some(index) = number_slot_index(slot) {
            let procs = (*type_object).tp_as_number;
            if procs.is_null() {
                0
            } else {
                (*(procs as *const PyNumberMethods)).slots[index]
            }
        } else if let Some(index) = sequence_slot_index(slot) {
            let procs = (*type_object).tp_as_sequence;
            if procs.is_null() {
                0
            } else {
                (*(procs as *const PySequenceMethods)).slots[index]
            }
        } else if let Some(index) = mapping_slot_index(slot) {
            let procs = (*type_object).tp_as_mapping;
            if procs.is_null() {
                0
            } else {
                (*(procs as *const PyMappingMethods)).slots[index]
            }
        } else {
            match slot {
                1 | 2 => {
                    let procs = (*type_object).tp_as_buffer;
                    if procs.is_null() {
                        0
                    } else if slot == 1 {
                        (*procs).bf_getbuffer
                    } else {
                        (*procs).bf_releasebuffer
                    }
                }
                47 => (*type_object).tp_alloc,
                48 => (*type_object).tp_base as usize,
                49 => (*type_object).tp_bases as usize,
                50 => (*type_object).tp_call,
                51 => (*type_object).tp_clear,
                52 => (*type_object).tp_dealloc,
                53 => (*type_object).tp_del,
                54 => (*type_object).tp_descr_get,
                55 => (*type_object).tp_descr_set,
                56 => (*type_object).tp_doc as usize,
                57 => (*type_object).tp_getattr,
                58 => (*type_object).tp_getattro,
                59 => (*type_object).tp_hash,
                60 => (*type_object).tp_init,
                61 => (*type_object).tp_is_gc,
                62 => (*type_object).tp_iter,
                63 => (*type_object).tp_iternext,
                64 => (*type_object).tp_methods as usize,
                65 => (*type_object).tp_new,
                66 => (*type_object).tp_repr,
                67 => (*type_object).tp_richcompare,
                68 => (*type_object).tp_setattr,
                69 => (*type_object).tp_setattro,
                70 => (*type_object).tp_str,
                71 => (*type_object).tp_traverse,
                72 => (*type_object).tp_members as usize,
                73 => (*type_object).tp_getset as usize,
                74 => (*type_object).tp_free,
                80 => (*type_object).tp_finalize,
                82 => (*type_object).tp_vectorcall,
                // 83 is Py_tp_token, which lives in PyHeapTypeObject; this
                // host has no token storage, so it reads as unset.
                _ => 0,
            }
        }
    };
    pointer as *mut c_void
}

/// `PyType_GetName`: returns a new reference to the type's short name.
#[no_mangle]
pub extern "C" fn PyType_GetName(object: *mut ShimObject) -> *mut ShimObject {
    if let Some(name) = super::hydro_type_name_text(object) {
        return value_object(ShimValue::Str(name));
    }
    if super::hydro_tagged(object).is_some() {
        set_error("TypeError: PyType_GetName: expected a class object");
        return ptr::null_mut();
    }
    match as_type(object) {
        Ok(type_object) => value_object(ShimValue::Str(type_short_name(type_object))),
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyType_GetQualName`: returns a new reference to the qualified type name.
#[no_mangle]
pub extern "C" fn PyType_GetQualName(object: *mut ShimObject) -> *mut ShimObject {
    if let Some(name) = super::hydro_type_qualname_text(object) {
        return value_object(ShimValue::Str(name));
    }
    if super::hydro_tagged(object).is_some() {
        set_error("TypeError: PyType_GetQualName: expected a class object");
        return ptr::null_mut();
    }
    match as_type(object) {
        Ok(type_object) => value_object(ShimValue::Str(type_name(type_object))),
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyType_GetFlags`: returns `tp_flags`.
#[no_mangle]
pub extern "C" fn PyType_GetFlags(object: *mut ShimObject) -> c_ulong {
    if let Some(flags) = super::hydro_type_flags(object) {
        return flags;
    }
    if super::hydro_tagged(object).is_some() {
        set_error("TypeError: PyType_GetFlags: expected a class object");
        return 0;
    }
    match as_type(object) {
        // SAFETY: validated type pointer.
        Ok(type_object) => unsafe { (*type_object).tp_flags },
        Err(error) => {
            set_error(error);
            0
        }
    }
}

/// `PyType_GenericAlloc`: allocates a zeroed instance of `type_object`.
#[no_mangle]
pub extern "C" fn PyType_GenericAlloc(
    type_object: *mut ShimObject,
    nitems: isize,
) -> *mut ShimObject {
    let type_object = match as_type(type_object) {
        Ok(type_object) => type_object,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    allocate_instance(type_object, nitems)
}

/// `PyType_GenericNew`: allocates an instance with `tp_alloc`.
#[no_mangle]
pub extern "C" fn PyType_GenericNew(
    type_object: *mut ShimObject,
    _args: *mut ShimObject,
    _kwds: *mut ShimObject,
) -> *mut ShimObject {
    let type_object = match as_type(type_object) {
        Ok(type_object) => type_object,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    call_type_alloc(type_object, 0)
}

/// `PyObject_Init`: initialises caller-provided storage as an object.
#[no_mangle]
pub extern "C" fn PyObject_Init(
    object: *mut ShimObject,
    type_object: *mut ShimObject,
) -> *mut ShimObject {
    if object.is_null() {
        set_error("MemoryError: PyObject_Init called with NULL storage");
        return ptr::null_mut();
    }
    let type_object = match as_type(type_object) {
        Ok(type_object) => type_object,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    // SAFETY: validated type pointer.
    let role = unsafe { (*type_object).role };
    if role != TypeRole::Instance {
        set_error(format!(
            "TypeError: PyObject_Init: '{}' has no raw instance layout",
            type_name(type_object)
        ));
        return ptr::null_mut();
    }
    // SAFETY: the caller promises `object` points at writable storage at least
    // `PyObject`-sized, per the CPython contract.
    unsafe {
        let head = object as *mut super::PyObjectHead;
        (*head).ob_refcnt = AtomicIsize::new(1);
        (*head).ob_type = type_object;
    }
    inc_ref(type_object as *mut ShimObject);
    object
}

/// `PyObject_InitVar`: like [`PyObject_Init`], also setting `ob_size`.
#[no_mangle]
pub extern "C" fn PyObject_InitVar(
    object: *mut ShimObject,
    type_object: *mut ShimObject,
    size: isize,
) -> *mut ShimObject {
    let type_object = match as_type(type_object) {
        Ok(type_object) => type_object,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    // SAFETY: validated type pointer.
    let basicsize = unsafe { (*type_object).tp_basicsize };
    if basicsize < MIN_VAR_BASICSIZE as isize {
        set_error(format!(
            "SystemError: PyObject_InitVar: type '{}' cannot hold ob_size (basicsize {basicsize})",
            type_name(type_object)
        ));
        return ptr::null_mut();
    }
    let object = PyObject_Init(object, type_object as *mut ShimObject);
    if object.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: the storage is at least `basicsize >= sizeof(PyVarObject)` bytes
    // and the caller owns it.
    unsafe {
        let var = object as *mut super::PyVarObjectHead;
        (*var).ob_size = AtomicIsize::new(size);
    }
    object
}

/// `_PyObject_New`: allocates an object through the type's `tp_alloc`.
///
/// `PyObject_New` in a stable-ABI extension expands to this symbol.
#[no_mangle]
pub extern "C" fn _PyObject_New(type_object: *mut ShimObject) -> *mut ShimObject {
    let type_object = match as_type(type_object) {
        Ok(type_object) => type_object,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    call_type_alloc(type_object, 0)
}

/// `_PyObject_NewVar`: allocates a variable-size object through `tp_alloc`.
#[no_mangle]
pub extern "C" fn _PyObject_NewVar(type_object: *mut ShimObject, nitems: isize) -> *mut ShimObject {
    let type_object = match as_type(type_object) {
        Ok(type_object) => type_object,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    if nitems < 0 {
        set_error("ValueError: negative object size");
        return ptr::null_mut();
    }
    call_type_alloc(type_object, nitems)
}

/// `PyObject_New`: non-standard exported name for the `PyObject_New` macro.
#[no_mangle]
pub extern "C" fn PyObject_New(type_object: *mut ShimObject) -> *mut ShimObject {
    _PyObject_New(type_object)
}

/// `PyObject_NewVar`: non-standard exported name for `PyObject_NewVar`.
#[no_mangle]
pub extern "C" fn PyObject_NewVar(type_object: *mut ShimObject, nitems: isize) -> *mut ShimObject {
    _PyObject_NewVar(type_object, nitems)
}

// ---------------------------------------------------------------------------
// Garbage collector integration
// ---------------------------------------------------------------------------

static TRACKED: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

fn tracked_lock() -> MutexGuard<'static, HashSet<usize>> {
    TRACKED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Registers an object as GC-tracked.
pub(crate) fn gc_track(object: *mut ShimObject) {
    if object.is_null() {
        set_error("SystemError: PyObject_GC_Track: object is NULL");
        return;
    }
    let gripper = GilGuard::new();
    let type_object = type_of(object);
    if type_object.is_null() || !type_uses_gc(type_object) {
        set_error(format!(
            "SystemError: PyObject_GC_Track: type '{}' is not a GC type",
            type_name(type_object)
        ));
        return;
    }
    tracked_lock().insert(object as usize);
    drop(gripper);
}

/// Removes an object from the tracked set.
pub(crate) fn gc_untrack(object: *mut ShimObject) {
    if object.is_null() {
        return;
    }
    tracked_lock().remove(&(object as usize));
}

/// True when the object is currently tracked.
pub(crate) fn gc_is_tracked(object: *mut ShimObject) -> bool {
    if object.is_null() {
        return false;
    }
    tracked_lock().contains(&(object as usize))
}

/// Instances whose `tp_clear` already ran in [`collect_cycles`].
///
/// CPython calls `tp_clear` once, from the collector, and the deallocator does
/// not repeat it. This host's default deallocator also clears GC instances, so
/// the collector records what it cleared and [`free_instance`] skips that one
/// call. The marker is removed when the instance is freed, so a later
/// allocation that reuses the address is never mistaken for a cleared one.
static CLEARED_BY_COLLECTOR: LazyLock<Mutex<HashSet<usize>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn cleared_lock() -> MutexGuard<'static, HashSet<usize>> {
    CLEARED_BY_COLLECTOR
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Removes and reports the "cleared by the cycle collector" marker.
pub(crate) fn take_collector_clear_marker(object: *mut ShimObject) -> bool {
    if object.is_null() {
        return false;
    }
    cleared_lock().remove(&(object as usize))
}

/// Guards against reentrant collection: `tp_traverse`/`tp_clear` may call
/// `PyGC_Collect` again.
static COLLECTING: AtomicBool = AtomicBool::new(false);

struct CollectingGuard;

impl Drop for CollectingGuard {
    fn drop(&mut self) {
        COLLECTING.store(false, Ordering::Release);
    }
}

/// Runs one cycle-collection pass over the GC-tracked extension instances.
///
/// This is the production body of `PyGC_Collect` (`capi::mod`). It implements
/// CPython's cycle algorithm for the instances this host tracks:
///
/// 1. every tracked instance that is still alive is protected with a
///    temporary reference (taken with a compare-and-swap, so a handle another
///    thread is already freeing is never resurrected);
/// 2. `tp_traverse` of every tracked instance subtracts the references that
///    come from other candidates (see [`gc_walk_tracked`]);
/// 3. an instance whose remaining count is non-zero has a reference from
///    outside the candidate set, so it and everything reachable from it
///    through tracked edges are live;
/// 4. the remaining candidates are unreachable: `tp_clear` runs once for each,
///    the instance is untracked, and releasing the temporary reference frees
///    it through the normal deallocation path.
///
/// A `tp_traverse` that reports more edges than the object's reference count
/// supports makes the pass abandon its result instead of risking a live
/// object, and a nested `PyGC_Collect` (from a traverse or clear callback)
/// returns zero. Returns the number of instances whose cycle was broken.
pub(crate) fn collect_cycles() -> usize {
    if COLLECTING.swap(true, Ordering::AcqRel) {
        return 0;
    }
    let _guard = CollectingGuard;
    collect_cycles_pass()
}

fn collect_cycles_pass() -> usize {
    // The GIL serialises every free this host performs (`free_object` takes
    // it), so holding it keeps the snapshot's addresses valid for the pass and
    // lets a concurrent `Py_DecRef` from another thread finish first.
    let _gripper = GilGuard::new();
    let addresses: Vec<usize> = tracked_lock().iter().copied().collect();
    if addresses.is_empty() {
        return 0;
    }
    // `external` starts as each candidate's reference count without the
    // temporary reference this pass takes; every edge from another candidate
    // is then subtracted. A candidate whose count already reached zero is
    // being freed by another thread (direct reference-count operations are
    // atomic and do not take the GIL) and is skipped instead of resurrected.
    let mut external: HashMap<usize, isize> = HashMap::with_capacity(addresses.len());
    let mut protected: Vec<usize> = Vec::with_capacity(addresses.len());
    for &address in &addresses {
        match protect_for_collection(address as *mut ShimObject) {
            Protection::Temporary(count) => {
                external.insert(address, count);
                protected.push(address);
            }
            Protection::Immortal => {
                external.insert(address, isize::MAX);
            }
            Protection::Dying => {}
        }
    }
    if external.is_empty() {
        return 0;
    }
    let mut edges: Vec<(usize, usize)> = Vec::new();
    gc_walk_tracked(&mut |parent, child| {
        let parent = parent as usize;
        let child = child as usize;
        if external.contains_key(&parent) {
            if let Some(count) = external.get_mut(&child) {
                *count -= 1;
                edges.push((parent, child));
            }
        }
        std::ops::ControlFlow::Continue(())
    });
    if external.values().any(|count| *count < 0) {
        // A `tp_traverse` reported more references than the object holds;
        // trusting it could free a live object. Abandon this pass.
        for &address in &protected {
            dec_ref(address as *mut ShimObject);
        }
        return 0;
    }
    let mut reachable: HashSet<usize> = external
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(&address, _)| address)
        .collect();
    let mut adjacency: HashMap<usize, Vec<usize>> = HashMap::new();
    for (parent, child) in edges {
        adjacency.entry(parent).or_default().push(child);
    }
    let mut pending: Vec<usize> = reachable.iter().copied().collect();
    while let Some(address) = pending.pop() {
        if let Some(children) = adjacency.get(&address) {
            for &child in children {
                if reachable.insert(child) {
                    pending.push(child);
                }
            }
        }
    }
    let garbage: Vec<usize> = protected
        .iter()
        .copied()
        .filter(|address| !reachable.contains(address))
        .filter(|address| gc_is_tracked(*address as *mut ShimObject))
        .collect();
    if !garbage.is_empty() {
        {
            let mut markers = cleared_lock();
            for &address in &garbage {
                markers.insert(address);
            }
        }
        for &address in &garbage {
            let object = address as *mut ShimObject;
            clear_instance(object);
            gc_untrack(object);
        }
    }
    let cleared = garbage.len();
    for &address in &protected {
        dec_ref(address as *mut ShimObject);
    }
    cleared
}

/// Outcome of taking a temporary reference on a collection candidate.
enum Protection {
    /// The reference was taken; the value is the count before it.
    Temporary(isize),
    /// The instance is immortal; it is a root and needs no temporary
    /// reference.
    Immortal,
    /// Another thread already brought the count to zero; the object is being
    /// freed and must not be resurrected.
    Dying,
}

/// Takes a temporary reference on a candidate without ever resurrecting an
/// object whose count already reached zero.
fn protect_for_collection(object: *mut ShimObject) -> Protection {
    // SAFETY: the address came from the tracked set. Every free is serialised
    // by the GIL this pass holds, so the handle is still allocated.
    let counter = unsafe { &(*object).head.ob_refcnt };
    let mut count = counter.load(Ordering::Relaxed);
    loop {
        if count == IMMORTAL_REFCNT {
            return Protection::Immortal;
        }
        if count <= 0 {
            return Protection::Dying;
        }
        match counter.compare_exchange_weak(count, count + 1, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => return Protection::Temporary(count),
            Err(observed) => count = observed,
        }
    }
}

/// Runs one instance's `tp_clear`, the collector's clear phase.
fn clear_instance(object: *mut ShimObject) {
    let type_object = type_of(object);
    if type_object.is_null() {
        return;
    }
    // SAFETY: live type object; both fields belong to it.
    let (flags, clear) = unsafe { ((*type_object).tp_flags, (*type_object).tp_clear) };
    if flags & PY_TPFLAGS_HAVE_GC == 0 || clear == 0 {
        return;
    }
    // SAFETY: `tp_clear` has CPython's `int (*)(PyObject *)` signature.
    let clear: unsafe extern "C" fn(*mut ShimObject) -> c_int =
        unsafe { std::mem::transmute(clear as *const c_void) };
    let _gripper = GilGuard::new();
    // SAFETY: `object` is live (the pass holds a reference) and of this type.
    // CPython ignores the returned status here as well; a `tp_clear` that
    // raises leaves the exception for the caller.
    let _ = unsafe { clear(object) };
}

/// Adapter between `visitproc` and a Rust closure.
///
/// This is the only implementation of CPython's `visitproc` calling
/// convention in this host; [`gc_traverse`] uses it to drive an extension's
/// `tp_traverse`, including from the `PyGC_Collect` production path.
struct TraverseVisitor<'a> {
    visit: &'a mut dyn FnMut(*mut ShimObject) -> std::ops::ControlFlow<()>,
}

/// `visitproc` adapter used by [`gc_traverse`].
///
/// CPython's convention is `int (*visitproc)(PyObject *object, void *arg)`:
/// the visited object comes first and the opaque context second.
extern "C" fn traverse_visitor(object: *mut c_void, context: *mut c_void) -> c_int {
    if context.is_null() {
        return 1;
    }
    // SAFETY: `gc_traverse` passes a live `TraverseVisitor` for the duration
    // of the call, and `visitproc` is only invoked synchronously.
    let visitor = unsafe { &mut *(context as *mut TraverseVisitor<'_>) };
    match (visitor.visit)(object as *mut ShimObject) {
        std::ops::ControlFlow::Continue(()) => 0,
        std::ops::ControlFlow::Break(()) => 1,
    }
}

/// Enumerates the references held by one tracked GC instance through its
/// `tp_traverse` slot.
///
/// `visit` returns [`std::ops::ControlFlow::Break`] to stop early, exactly like
/// a `visitproc` returning non-zero. Objects of non-GC types and types with no
/// `tp_traverse` have no traversable edges. A type that installs
/// `tp_traverse` without `Py_TPFLAGS_HAVE_GC` is not walked: CPython only
/// calls the slot for GC types.
pub(crate) fn gc_traverse(
    object: *mut ShimObject,
    visit: &mut dyn FnMut(*mut ShimObject) -> std::ops::ControlFlow<()>,
) {
    if object.is_null() {
        return;
    }
    let type_object = type_of(object);
    if type_object.is_null() || !type_uses_gc(type_object) {
        return;
    }
    // SAFETY: live type object.
    let traverse = unsafe { (*type_object).tp_traverse };
    if traverse == 0 {
        return;
    }
    // SAFETY: `tp_traverse` has CPython's
    // `int (*)(PyObject *, visitproc, void *)` signature.
    let traverse: unsafe extern "C" fn(
        *mut ShimObject,
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int>,
        *mut c_void,
    ) -> c_int = unsafe { std::mem::transmute(traverse as *const c_void) };
    let mut visitor = TraverseVisitor { visit };
    let _gripper = GilGuard::new();
    // SAFETY: the visitor outlives the call; a non-zero status only means the
    // walk stopped and is not an error for this host.
    let _ = unsafe {
        traverse(
            object,
            Some(traverse_visitor),
            &mut visitor as *mut TraverseVisitor<'_> as *mut c_void,
        )
    };
}

/// Walks every currently tracked GC instance and enumerates its `tp_traverse`
/// edges.
///
/// This is the bridge the collector uses to see references held by C
/// instances: the tracked set is the registry of GC-enabled instances, and
/// this function turns each type's `tp_traverse` into a Rust callback. Hydro
/// values cross through rooted handles (see `HydroValue`) rather than raw
/// heap pointers, so a handle held by an extension already has its own
/// persistent root; the bridge exists so the collector can follow the handle
/// graph instead of assuming a C instance is an opaque non-GC container.
/// [`collect_cycles`] is its production caller.
pub(crate) fn gc_walk_tracked(
    visit: &mut dyn FnMut(*mut ShimObject, *mut ShimObject) -> std::ops::ControlFlow<()>,
) {
    let addresses: Vec<usize> = tracked_lock().iter().copied().collect();
    for address in addresses {
        let object = address as *mut ShimObject;
        let mut child_visit = |child: *mut ShimObject| visit(object, child);
        gc_traverse(object, &mut child_visit);
    }
}

/// One synthetic metatype for a Hydro class exposed through the C API.
///
/// `PyType_Check` is an inline flag test on `Py_TYPE(op)`, so a Hydro class
/// handle needs an `ob_type` whose `tp_flags` include
/// `Py_TPFLAGS_TYPE_SUBCLASS`. The metatype is leaked: every handle created
/// from the class keeps naming it.
#[repr(C)]
struct HydroClassMetatype {
    object: TypeObject,
    name: CString,
}

/// Per-generation metatypes for Hydro class values. A runtime reset frees the
/// Hydro heap, so tagged values (and therefore the cache keys) are invalidated
/// with it.
static HYDRO_CLASS_METATYPES: LazyLock<Mutex<(u64, HashMap<i64, usize>)>> =
    LazyLock::new(|| Mutex::new((u64::MAX, HashMap::new())));

/// Metatype used as `Py_TYPE` of a Hydro class handle.
///
/// Its `tp_base` is `type`, so `PyType_IsSubtype(metatype, &PyType_Type)` and
/// `PyObject_IsInstance(handle, type)` succeed, and its flags make CPython's
/// inline `PyType_Check` accept the handle. Returns `None` for Hydro values
/// that are not classes.
pub(crate) fn hydro_class_metatype(value: i64) -> Option<*mut TypeObject> {
    crate::value::as_class(value)?;
    ensure_builtin_types();
    let generation = crate::heap::generation();
    let mut cache = HYDRO_CLASS_METATYPES
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if cache.0 != generation {
        cache.0 = generation;
        cache.1.clear();
    }
    if let Some(&pointer) = cache.1.get(&value) {
        return Some(pointer as *mut TypeObject);
    }
    let name = crate::value::class_qualname(value)
        .unwrap_or_else(|| crate::value::class_display_name(value).unwrap_or("type"))
        .to_string();
    let c_name = CString::new(name).ok()?;
    let metatype = Box::into_raw(Box::new(HydroClassMetatype {
        object: TypeObject::builtin(
            ptr::null(),
            TypeRole::Shim,
            PY_TPFLAGS_READY | PY_TPFLAGS_BASETYPE | PY_TPFLAGS_TYPE_SUBCLASS,
        ),
        name: c_name,
    }));
    // SAFETY: the box is leaked, its first field is the type object and the
    // name buffer lives in the same allocation.
    unsafe {
        (*metatype).object.tp_name = (*metatype).name.as_ptr();
        (*metatype).object.ob_type = ptr::addr_of_mut!(TYPE_TYPE);
        (*metatype).object.tp_base = ptr::addr_of_mut!(TYPE_TYPE);
    }
    cache.1.insert(value, metatype as usize);
    // SAFETY: `metatype` is the leaked box allocated above.
    Some(unsafe { ptr::addr_of_mut!((*metatype).object) })
}

/// CPython's `PyObject_IS_GC`: `HAVE_GC` plus an optional `tp_is_gc` filter.
pub(crate) fn object_is_gc(object: *mut ShimObject) -> bool {
    let type_object = type_of(object);
    if type_object.is_null() || !type_uses_gc(type_object) {
        return false;
    }
    // SAFETY: live type object.
    let predicate = unsafe { (*type_object).tp_is_gc };
    if predicate == 0 {
        return true;
    }
    let predicate: unsafe extern "C" fn(*mut ShimObject) -> c_int =
        unsafe { std::mem::transmute(predicate as *const c_void) };
    let _gripper = GilGuard::new();
    // SAFETY: `tp_is_gc` has this signature.
    unsafe { predicate(object) != 0 }
}

/// `PyObject_IS_GC`.
#[no_mangle]
pub extern "C" fn PyObject_IS_GC(object: *mut ShimObject) -> c_int {
    object_is_gc(object) as c_int
}

/// `PyObject_GC_Track`.
#[no_mangle]
pub extern "C" fn PyObject_GC_Track(object: *mut c_void) {
    gc_track(object as *mut ShimObject);
}

/// `PyObject_GC_UnTrack`.
#[no_mangle]
pub extern "C" fn PyObject_GC_UnTrack(object: *mut c_void) {
    gc_untrack(object as *mut ShimObject);
}

/// `PyObject_GC_IsTracked`.
#[no_mangle]
pub extern "C" fn PyObject_GC_IsTracked(object: *mut ShimObject) -> c_int {
    gc_is_tracked(object) as c_int
}

/// `PyObject_GC_Del`: frees the storage of a GC-allocated instance.
#[no_mangle]
pub extern "C" fn PyObject_GC_Del(object: *mut c_void) {
    if object.is_null() {
        return;
    }
    gc_untrack(object as *mut ShimObject);
    // SAFETY: the caller hands ownership of an allocation produced by
    // `PyType_GenericAlloc` or `_PyObject_New`, both of which use `malloc`.
    unsafe { libc::free(object) };
}

/// `_PyObject_GC_New`: allocates an instance and tracks it.
#[no_mangle]
pub extern "C" fn _PyObject_GC_New(type_object: *mut ShimObject) -> *mut ShimObject {
    let type_object = match as_type(type_object) {
        Ok(type_object) => type_object,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    if !type_uses_gc(type_object) {
        set_error(format!(
            "SystemError: _PyObject_GC_New: type '{}' is not a GC type",
            type_name(type_object)
        ));
        return ptr::null_mut();
    }
    allocate_instance(type_object, 0)
}

/// `_PyObject_GC_NewVar`: allocates a variable-size instance and tracks it.
#[no_mangle]
pub extern "C" fn _PyObject_GC_NewVar(
    type_object: *mut ShimObject,
    nitems: isize,
) -> *mut ShimObject {
    let type_object = match as_type(type_object) {
        Ok(type_object) => type_object,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    if !type_uses_gc(type_object) {
        set_error(format!(
            "SystemError: _PyObject_GC_NewVar: type '{}' is not a GC type",
            type_name(type_object)
        ));
        return ptr::null_mut();
    }
    if nitems < 0 {
        set_error("ValueError: negative object size");
        return ptr::null_mut();
    }
    allocate_instance(type_object, nitems)
}

/// Allocates an immortal, named type object (`PyExc_*` classes).
///
/// The name is leaked for the process lifetime, exactly like CPython's static
/// type names.
pub(crate) fn allocate_named_type(name: &str, flags: c_ulong) -> *mut TypeObject {
    ensure_builtin_types();
    let name = match CString::new(name) {
        Ok(name) => name,
        Err(_) => return ptr::null_mut(),
    };
    let mut type_object =
        TypeObject::builtin(name.into_raw() as *const c_char, TypeRole::Metatype, flags);
    type_object.owns_name = false;
    type_object.ob_type = ptr::addr_of_mut!(TYPE_TYPE);
    type_object.tp_base = ptr::addr_of_mut!(OBJECT_TYPE);
    let pointer = Box::into_raw(Box::new(type_object));
    remember_internal_type(pointer);
    pointer
}

/// `tp_new` for native base types that extension classes may inherit from.
///
/// CPython's exception and datetime C-API bases expose a real constructor;
/// PyO3's `PyClassInitializer` fetches it through `PyType_GetSlot` and calls
/// it as `tp_new(subtype, (), NULL)` to allocate the base part of a Python
/// subclass. Allocate the *subtype* exactly as CPython's `BaseException_new`
/// does; PyO3 then initialises its own fields.
///
/// # Safety
/// `subtype` must be a live type object or NULL; the return value is a new
/// reference.
pub(crate) unsafe extern "C" fn native_base_new(
    subtype: *mut TypeObject,
    _args: *mut ShimObject,
    _kwargs: *mut ShimObject,
) -> *mut ShimObject {
    if subtype.is_null() {
        set_error("SystemError: native base constructor requires a type");
        return ptr::null_mut();
    }
    // SAFETY: `subtype` was validated by the caller (it is the class being
    // constructed).
    let allocator = unsafe { (*subtype).tp_alloc };
    if allocator != 0 && allocator != PyType_GenericAlloc as *const () as usize {
        // SAFETY: `allocfunc` has this signature.
        let allocate: unsafe extern "C" fn(*mut TypeObject, isize) -> *mut ShimObject =
            unsafe { std::mem::transmute(allocator as *const c_void) };
        // SAFETY: the caller handed us a live type.
        return unsafe { allocate(subtype, 0) };
    }
    allocate_instance(subtype, 0)
}

/// Allocates an immortal exception class.
///
/// Exception classes use [`TypeRole::Shim`] so instances of them can carry the
/// [`super::ShimKind::Exception`] payload while the class object itself still
/// satisfies `is_type_pointer` (`ob_type` is the shared metatype). The class
/// starts with `object` as its base; `initialize_exception_types` links the
/// CPython hierarchy afterwards.
pub(crate) fn allocate_exception_type(name: &str, flags: c_ulong) -> *mut TypeObject {
    let type_object = allocate_named_type(name, flags);
    if !type_object.is_null() {
        // SAFETY: freshly allocated, uniquely owned box.
        unsafe {
            (*type_object).role = TypeRole::Shim;
            // PyO3 subclasses of `Exception`/`ValueError` allocate their base
            // part through this slot and free it through `tp_free` when the
            // subclass deallocator runs (its `BaseException_dealloc`
            // equivalent is not modelled here).
            (*type_object).tp_new = native_base_new as *const () as usize;
            (*type_object).tp_free = instance_free as *const () as usize;
        }
    }
    type_object
}

// ---------------------------------------------------------------------------
// Method lookup
// ---------------------------------------------------------------------------

/// Finds a method in the type's `tp_methods` tables, walking the MRO.
pub(crate) fn find_method(type_object: *mut TypeObject, name: &str) -> Option<MethodSpec> {
    for current in mro_of(type_object) {
        // SAFETY: every MRO entry is a live type object.
        let methods = unsafe { (*current).tp_methods };
        if !methods.is_null() {
            let owner = format!("type '{}'", type_name(current));
            let table = read_method_table(methods, &owner).ok()?;
            if let Some(spec) = table.into_iter().find(|spec| spec.name == name) {
                return Some(spec);
            }
        }
    }
    None
}

/// Finds a getset descriptor in the type's MRO.
pub(crate) fn find_getset(
    type_object: *mut TypeObject,
    name: &str,
) -> Option<*mut PyGetSetDefMirror> {
    for current in mro_of(type_object) {
        // SAFETY: every MRO entry is a live type object.
        let table = unsafe { (*current).tp_getset as *mut PyGetSetDefMirror };
        if table.is_null() {
            continue;
        }
        let owner = format!("type '{}'", type_name(current));
        let Ok(entries) = read_getset_table(table, &owner) else {
            continue;
        };
        for entry in entries {
            // SAFETY: entries point into the validated, terminated table.
            let candidate = unsafe { read_c_string((*entry).name) };
            if candidate == name {
                return Some(entry);
            }
        }
    }
    None
}

/// Finds a member descriptor in the type's MRO.
pub(crate) fn find_member(
    type_object: *mut TypeObject,
    name: &str,
) -> Option<*mut PyMemberDefMirror> {
    for current in mro_of(type_object) {
        // SAFETY: every MRO entry is a live type object.
        let table = unsafe { (*current).tp_members as *mut PyMemberDefMirror };
        if table.is_null() {
            continue;
        }
        let owner = format!("type '{}'", type_name(current));
        let Ok(entries) = read_member_table(table, &owner) else {
            continue;
        };
        for entry in entries {
            // SAFETY: entries point into the validated, terminated table.
            let candidate = unsafe { read_c_string((*entry).name) };
            if candidate == name {
                return Some(entry);
            }
        }
    }
    None
}

/// True when `type_object` is `base` or derives from it.
pub(crate) fn is_subtype(type_object: *mut TypeObject, base: *mut TypeObject) -> bool {
    if base.is_null() {
        return false;
    }
    mro_of(type_object).contains(&base)
}

/// Reads the function pointer for `slot` from the number suite of `type_object`.
fn number_function(type_object: *mut TypeObject, index: usize) -> usize {
    if type_object.is_null() {
        return 0;
    }
    // SAFETY: live type object; the suite pointer is validated at creation.
    unsafe {
        let procs = (*type_object).tp_as_number;
        if procs.is_null() {
            0
        } else {
            (*(procs as *const PyNumberMethods)).slots[index]
        }
    }
}

fn sequence_function(type_object: *mut TypeObject, index: usize) -> usize {
    if type_object.is_null() {
        return 0;
    }
    // SAFETY: live type object.
    unsafe {
        let procs = (*type_object).tp_as_sequence;
        if procs.is_null() {
            0
        } else {
            (*(procs as *const PySequenceMethods)).slots[index]
        }
    }
}

fn mapping_function(type_object: *mut TypeObject, index: usize) -> usize {
    if type_object.is_null() {
        return 0;
    }
    // SAFETY: live type object.
    unsafe {
        let procs = (*type_object).tp_as_mapping;
        if procs.is_null() {
            0
        } else {
            (*(procs as *const PyMappingMethods)).slots[index]
        }
    }
}

/// Type name used in operator diagnostics.
fn operator_type_name(object: *mut ShimObject) -> String {
    let type_object = type_of(object);
    if type_object.is_null() {
        "NULL".to_string()
    } else {
        type_name(type_object)
    }
}

/// Invokes a `getter` (`PyObject *(*)(PyObject *, void *)`).
pub(crate) fn getset_get(
    object: *mut ShimObject,
    definition: *mut PyGetSetDefMirror,
) -> Result<*mut ShimObject, String> {
    // SAFETY: `definition` points into a validated descriptor table.
    let getter = unsafe { (*definition).get };
    if getter == 0 {
        // SAFETY: as above.
        let name = unsafe { read_c_string((*definition).name) };
        return Err(format!(
            "AttributeError: attribute '{name}' of '{}' objects is not readable",
            operator_type_name(object)
        ));
    }
    // SAFETY: `getter` has CPython's `getter` signature.
    let getter: unsafe extern "C" fn(*mut ShimObject, *mut c_void) -> *mut ShimObject =
        unsafe { std::mem::transmute(getter as *const c_void) };
    // SAFETY: as above.
    let closure = unsafe { (*definition).closure };
    let _gripper = GilGuard::new();
    // SAFETY: the getter borrows both arguments.
    let result = unsafe { getter(object, closure) };
    if result.is_null() {
        return Err(super::take_error().unwrap_or_else(|| {
            format!(
                "SystemError: getset getter for '{}' returned NULL without setting an exception",
                operator_type_name(object)
            )
        }));
    }
    Ok(result)
}

/// Invokes a `setter` (`int (*)(PyObject *, PyObject *, void *)`).
pub(crate) fn getset_set(
    object: *mut ShimObject,
    definition: *mut PyGetSetDefMirror,
    value: *mut ShimObject,
) -> Result<(), String> {
    // SAFETY: `definition` points into a validated descriptor table.
    let setter = unsafe { (*definition).set };
    if setter == 0 {
        // SAFETY: as above.
        let name = unsafe { read_c_string((*definition).name) };
        return Err(format!(
            "AttributeError: attribute '{name}' of '{}' objects is read-only",
            operator_type_name(object)
        ));
    }
    // SAFETY: `setter` has CPython's `setter` signature.
    let setter: unsafe extern "C" fn(*mut ShimObject, *mut ShimObject, *mut c_void) -> c_int =
        unsafe { std::mem::transmute(setter as *const c_void) };
    // SAFETY: as above.
    let closure = unsafe { (*definition).closure };
    let _gripper = GilGuard::new();
    // SAFETY: the setter borrows its arguments; a NULL value means deletion.
    let status = unsafe { setter(object, value, closure) };
    if status != 0 {
        return Err(super::take_error().unwrap_or_else(|| {
            format!(
                "AttributeError: cannot set attribute '{}' on '{}' object",
                unsafe { read_c_string((*definition).name) },
                operator_type_name(object)
            )
        }));
    }
    Ok(())
}

/// Byte size of one `PyMemberDef` value, or 0 for pointer/string entries.
fn member_size(kind: c_int) -> usize {
    match kind {
        T_SHORT | T_USHORT => 2,
        T_INT | T_UINT | T_FLOAT | T_BOOL => 4,
        T_LONG | T_ULONG | T_DOUBLE | T_LONGLONG | T_ULONGLONG | T_PYSSIZET => 8,
        T_BYTE | T_UBYTE | T_CHAR | T_STRING_INPLACE => 1,
        _ => 0,
    }
}

/// Validates that a member descriptor addresses storage inside the instance.
fn member_address(
    object: *mut ShimObject,
    definition: *mut PyMemberDefMirror,
) -> Result<usize, String> {
    if object.is_null() {
        return Err("SystemError: member access on NULL object".to_string());
    }
    // SAFETY: the definition is owned by a live type.
    let (offset, kind, flags, name) = unsafe {
        (
            (*definition).offset,
            (*definition).type_,
            (*definition).flags,
            read_c_string((*definition).name),
        )
    };
    if flags & RELATIVE_OFFSET != 0 {
        return Err(format!(
            "SystemError: member '{name}' uses Py_RELATIVE_OFFSET, which this host does not \
             support"
        ));
    }
    let type_object = type_of(object);
    let basicsize = if type_object.is_null() {
        0
    } else {
        // SAFETY: live type object.
        unsafe { (*type_object).tp_basicsize }
    };
    let size = member_size(kind);
    let end = offset.saturating_add(size as isize);
    if basicsize <= 0 || end > basicsize {
        return Err(format!(
            "SystemError: member '{name}' at offset {offset} does not fit in a {basicsize}-byte \
             instance of '{}'",
            operator_type_name(object)
        ));
    }
    // SAFETY: the object points at at least `tp_basicsize` bytes.
    Ok(unsafe { (object as *mut u8).add(offset as usize) as usize })
}

/// Reads a member descriptor.
#[allow(clippy::unnecessary_cast)] // `c_long` is `i64` on x86_64.
pub(crate) fn member_get(
    object: *mut ShimObject,
    definition: *mut PyMemberDefMirror,
) -> Result<*mut ShimObject, String> {
    let address = member_address(object, definition)?;
    // SAFETY: `address` was bounds-checked against the instance layout.
    unsafe {
        let kind = (*definition).type_;
        let name = read_c_string((*definition).name);
        let int_object =
            |value: i64| -> Result<*mut ShimObject, String> { Ok(super::new_int(value)) };
        match kind {
            T_SHORT => int_object((address as *const i16).read_unaligned() as i64),
            T_INT => int_object((address as *const i32).read_unaligned() as i64),
            T_LONG => int_object((address as *const c_long).read_unaligned() as i64),
            T_LONGLONG => int_object((address as *const i64).read_unaligned()),
            T_PYSSIZET => int_object((address as *const isize).read_unaligned() as i64),
            T_BYTE => int_object((address as *const i8).read_unaligned() as i64),
            T_UBYTE => int_object((address as *const u8).read_unaligned() as i64),
            T_USHORT => int_object((address as *const u16).read_unaligned() as i64),
            T_UINT => int_object((address as *const u32).read_unaligned() as i64),
            T_ULONG => {
                let value = (address as *const c_ulong).read_unaligned();
                if value > i64::MAX as c_ulong {
                    Err(format!(
                        "OverflowError: member '{name}' value {value} does not fit in a Python int"
                    ))
                } else {
                    int_object(value as i64)
                }
            }
            T_ULONGLONG => {
                let value = (address as *const u64).read_unaligned();
                if value > i64::MAX as u64 {
                    Err(format!(
                        "OverflowError: member '{name}' value {value} does not fit in a Python int"
                    ))
                } else {
                    int_object(value as i64)
                }
            }
            T_FLOAT => Ok(super::PyFloat_FromDouble(
                (address as *const f32).read_unaligned() as f64,
            )),
            T_DOUBLE => Ok(super::PyFloat_FromDouble(
                (address as *const f64).read_unaligned(),
            )),
            T_BOOL => Ok(bool_object((address as *const u8).read_unaligned() != 0)),
            T_CHAR => {
                let byte = (address as *const u8).read_unaligned();
                Ok(super::new_str((byte as char).to_string()))
            }
            T_STRING => {
                let pointer = (address as *const *const c_char).read_unaligned();
                if pointer.is_null() {
                    Ok(super::Py_GetConstant(super::PY_CONSTANT_NONE))
                } else {
                    Ok(super::new_str(read_c_string(pointer)))
                }
            }
            T_STRING_INPLACE => Ok(super::new_str(read_c_string(address as *const c_char))),
            T_NONE => Ok(super::Py_GetConstant(super::PY_CONSTANT_NONE)),
            T_OBJECT | T_OBJECT_EX => {
                let pointer = (address as *const *mut ShimObject).read_unaligned();
                if pointer.is_null() {
                    if kind == T_OBJECT_EX {
                        Err(format!(
                            "AttributeError: member '{name}' of '{}' object is not set",
                            operator_type_name(object)
                        ))
                    } else {
                        Ok(super::Py_GetConstant(super::PY_CONSTANT_NONE))
                    }
                } else {
                    inc_ref(pointer);
                    Ok(pointer)
                }
            }
            other => Err(format!(
                "SystemError: member '{name}' has unsupported type {other}"
            )),
        }
    }
}

/// Converts a Python integer to the range of one member type.
fn member_integer(value: *mut ShimObject, kind: c_int, name: &str) -> Result<i128, String> {
    let number = super::as_int(value)? as i128;
    let fits = match kind {
        T_SHORT => (-(1i128 << 15)..(1i128 << 15)).contains(&number),
        T_USHORT => (0..(1i128 << 16)).contains(&number),
        T_INT => (-(1i128 << 31)..(1i128 << 31)).contains(&number),
        T_UINT => (0..(1i128 << 32)).contains(&number),
        T_LONG | T_LONGLONG | T_PYSSIZET => (-(1i128 << 63)..(1i128 << 63)).contains(&number),
        T_ULONG | T_ULONGLONG => (0..(1i128 << 64)).contains(&number),
        T_BYTE => (-(1i128 << 7)..(1i128 << 7)).contains(&number),
        T_UBYTE | T_BOOL => (0..256).contains(&number),
        _ => true,
    };
    if !fits {
        return Err(format!(
            "OverflowError: value out of range for member '{name}'"
        ));
    }
    Ok(number)
}

/// Writes a member descriptor. A NULL `value` clears object members.
pub(crate) fn member_set(
    object: *mut ShimObject,
    definition: *mut PyMemberDefMirror,
    value: *mut ShimObject,
) -> Result<(), String> {
    let address = member_address(object, definition)?;
    // SAFETY: `address` was bounds-checked against the instance layout.
    unsafe {
        let kind = (*definition).type_;
        let flags = (*definition).flags;
        let name = read_c_string((*definition).name);
        if flags & READONLY != 0 {
            return Err(format!(
                "AttributeError: member '{name}' of '{}' object is read-only",
                operator_type_name(object)
            ));
        }
        match kind {
            T_OBJECT | T_OBJECT_EX => {
                let slot = address as *mut *mut ShimObject;
                let previous = slot.read_unaligned();
                slot.write_unaligned(value);
                if !value.is_null() {
                    inc_ref(value);
                }
                if !previous.is_null() {
                    dec_ref(previous);
                }
                Ok(())
            }
            T_FLOAT => {
                if value.is_null() {
                    return Err(format!("TypeError: member '{name}' requires a float"));
                }
                let number = super::as_float(value)?;
                (address as *mut f32).write_unaligned(number as f32);
                Ok(())
            }
            T_DOUBLE => {
                if value.is_null() {
                    return Err(format!("TypeError: member '{name}' requires a float"));
                }
                let number = super::as_float(value)?;
                (address as *mut f64).write_unaligned(number);
                Ok(())
            }
            T_CHAR | T_STRING | T_STRING_INPLACE => Err(format!(
                "AttributeError: string member '{name}' of '{}' object is read-only",
                operator_type_name(object)
            )),
            T_NONE => Err(format!(
                "AttributeError: member '{name}' of '{}' object is read-only",
                operator_type_name(object)
            )),
            _ => {
                if value.is_null() {
                    return Err(format!("TypeError: member '{name}' requires an integer"));
                }
                let number = member_integer(value, kind, &name)?;
                match kind {
                    T_SHORT => (address as *mut i16).write_unaligned(number as i16),
                    T_USHORT => (address as *mut u16).write_unaligned(number as u16),
                    T_INT => (address as *mut i32).write_unaligned(number as i32),
                    T_UINT => (address as *mut u32).write_unaligned(number as u32),
                    T_LONG => (address as *mut c_long).write_unaligned(number as c_long),
                    T_ULONG => (address as *mut c_ulong).write_unaligned(number as c_ulong),
                    T_LONGLONG => (address as *mut i64).write_unaligned(number as i64),
                    T_ULONGLONG => (address as *mut u64).write_unaligned(number as u64),
                    T_PYSSIZET => (address as *mut isize).write_unaligned(number as isize),
                    T_BYTE => (address as *mut i8).write_unaligned(number as i8),
                    T_UBYTE => (address as *mut u8).write_unaligned(number as u8),
                    T_BOOL => (address as *mut u8).write_unaligned((number != 0) as u8),
                    _ => unreachable!(),
                }
                Ok(())
            }
        }
    }
}

/// A borrowed comparison-result singleton as a new reference.
fn bool_object(value: bool) -> *mut ShimObject {
    super::Py_GetConstant(if value {
        super::PY_CONSTANT_TRUE
    } else {
        super::PY_CONSTANT_FALSE
    })
}

/// Wraps an operator result, turning "NULL without an exception" into a
/// `SystemError` so callers never see a silent failure.
fn operator_result(result: *mut ShimObject, what: &str) -> *mut ShimObject {
    if result.is_null() && !super::error_pending() {
        super::set_error(format!(
            "SystemError: {what} returned NULL without setting an exception"
        ));
    }
    result
}

fn call_binary(
    function: usize,
    left: *mut ShimObject,
    right: *mut ShimObject,
    what: &str,
) -> *mut ShimObject {
    // SAFETY: binaryfunc has this signature.
    let function: unsafe extern "C" fn(*mut ShimObject, *mut ShimObject) -> *mut ShimObject =
        unsafe { std::mem::transmute(function as *const c_void) };
    let _gripper = GilGuard::new();
    // SAFETY: both operands are live and borrowed by the callee.
    operator_result(unsafe { function(left, right) }, what)
}

fn call_unary(function: usize, object: *mut ShimObject, what: &str) -> *mut ShimObject {
    // SAFETY: unaryfunc has this signature.
    let function: unsafe extern "C" fn(*mut ShimObject) -> *mut ShimObject =
        unsafe { std::mem::transmute(function as *const c_void) };
    let _gripper = GilGuard::new();
    // SAFETY: the operand is live and borrowed.
    operator_result(unsafe { function(object) }, what)
}

fn call_ternary(
    function: usize,
    first: *mut ShimObject,
    second: *mut ShimObject,
    third: *mut ShimObject,
    what: &str,
) -> *mut ShimObject {
    // SAFETY: ternaryfunc has this signature.
    let function: unsafe extern "C" fn(
        *mut ShimObject,
        *mut ShimObject,
        *mut ShimObject,
    ) -> *mut ShimObject = unsafe { std::mem::transmute(function as *const c_void) };
    let _gripper = GilGuard::new();
    // SAFETY: all operands are live and borrowed.
    operator_result(unsafe { function(first, second, third) }, what)
}

/// A builtin numeric operand, used when a type has no number suite.
#[derive(Clone, Copy)]
enum BuiltinNumber {
    Int(i128),
    Float(f64),
}

impl BuiltinNumber {
    fn to_f64(self) -> f64 {
        match self {
            BuiltinNumber::Int(value) => value as f64,
            BuiltinNumber::Float(value) => value,
        }
    }
}

/// Extracts a builtin int/bool/float payload. Custom number types are handled
/// by their suite first; this is the CPython-equivalent fallback for the
/// runtime's own scalar handles.
fn builtin_number(object: *mut ShimObject) -> Option<BuiltinNumber> {
    match super::kind_ref(object)? {
        super::ShimKind::Int(value) => Some(BuiltinNumber::Int(*value as i128)),
        super::ShimKind::Bool(value) => Some(BuiltinNumber::Int(*value as i128)),
        super::ShimKind::Float(value) => Some(BuiltinNumber::Float(*value)),
        _ => None,
    }
}

fn builtin_int_object(value: i128) -> *mut ShimObject {
    match i64::try_from(value) {
        Ok(value) => super::new_int(value),
        Err(_) => {
            super::set_error("OverflowError: integer result does not fit in a Python int");
            ptr::null_mut()
        }
    }
}

fn builtin_float_object(value: f64) -> *mut ShimObject {
    super::PyFloat_FromDouble(value)
}

fn builtin_zero_division(what: &str) -> *mut ShimObject {
    super::set_error(format!("ZeroDivisionError: {what}"));
    ptr::null_mut()
}

fn builtin_floor_div_int(left: i128, right: i128) -> Option<i128> {
    if right == 0 {
        return None;
    }
    let quotient = left / right;
    let remainder = left % right;
    Some(if remainder != 0 && (remainder < 0) != (right < 0) {
        quotient - 1
    } else {
        quotient
    })
}

fn builtin_remainder_int(left: i128, right: i128) -> Option<i128> {
    if right == 0 {
        return None;
    }
    let remainder = left % right;
    Some(if remainder != 0 && (remainder < 0) != (right < 0) {
        remainder + right
    } else {
        remainder
    })
}

/// Unary number fallback for builtin scalars. `None` means the slot is not a
/// numeric operation this host can perform on the value.
fn builtin_number_unary(slot: c_int, object: *mut ShimObject) -> Option<*mut ShimObject> {
    let value = builtin_number(object)?;
    Some(match (slot, value) {
        (30, BuiltinNumber::Int(value)) => builtin_int_object(value.wrapping_neg()),
        (30, BuiltinNumber::Float(value)) => builtin_float_object(-value),
        (32, BuiltinNumber::Float(value)) => builtin_float_object(value),
        (6, BuiltinNumber::Int(value)) => builtin_int_object(value.abs()),
        (6, BuiltinNumber::Float(value)) => builtin_float_object(value.abs()),
        (27, BuiltinNumber::Int(value)) => builtin_int_object(!value),
        (13 | 26 | 32, BuiltinNumber::Int(value)) => builtin_int_object(value),
        (26, BuiltinNumber::Float(value)) => match i64::try_from(value.trunc() as i128) {
            Ok(value) => super::new_int(value),
            Err(_) => {
                super::set_error("OverflowError: integer result does not fit in a Python int");
                ptr::null_mut()
            }
        },
        (11, value) => builtin_float_object(value.to_f64()),
        _ => return None,
    })
}

/// Binary number fallback for builtin scalars.
fn builtin_number_binary(
    slot: c_int,
    left: *mut ShimObject,
    right: *mut ShimObject,
) -> Option<*mut ShimObject> {
    let left = builtin_number(left)?;
    let right = builtin_number(right)?;
    Some(match slot {
        // nb_add
        7 => match (left, right) {
            (BuiltinNumber::Int(a), BuiltinNumber::Int(b)) => builtin_int_object(a + b),
            _ => builtin_float_object(left.to_f64() + right.to_f64()),
        },
        // nb_subtract
        36 => match (left, right) {
            (BuiltinNumber::Int(a), BuiltinNumber::Int(b)) => builtin_int_object(a - b),
            _ => builtin_float_object(left.to_f64() - right.to_f64()),
        },
        // nb_multiply
        29 => match (left, right) {
            (BuiltinNumber::Int(a), BuiltinNumber::Int(b)) => builtin_int_object(a * b),
            _ => builtin_float_object(left.to_f64() * right.to_f64()),
        },
        // nb_remainder
        34 => match (left, right) {
            (BuiltinNumber::Int(a), BuiltinNumber::Int(b)) => match builtin_remainder_int(a, b) {
                Some(value) => builtin_int_object(value),
                None => builtin_zero_division("integer modulo by zero"),
            },
            _ => {
                let divisor = right.to_f64();
                if divisor == 0.0 {
                    builtin_zero_division("float modulo")
                } else {
                    let dividend = left.to_f64();
                    builtin_float_object(dividend - (dividend / divisor).floor() * divisor)
                }
            }
        },
        // nb_divmod
        10 => match (left, right) {
            (BuiltinNumber::Int(a), BuiltinNumber::Int(b)) => {
                let Some(quotient) = builtin_floor_div_int(a, b) else {
                    return Some(builtin_zero_division("integer division or modulo by zero"));
                };
                let remainder = builtin_remainder_int(a, b).unwrap_or(0);
                let tuple = super::PyTuple_New(2);
                if tuple.is_null() {
                    return Some(ptr::null_mut());
                }
                let quotient_object = builtin_int_object(quotient);
                if quotient_object.is_null() {
                    dec_ref(tuple);
                    return Some(ptr::null_mut());
                }
                if super::PyTuple_SetItem(tuple, 0, quotient_object) != 0 {
                    dec_ref(quotient_object);
                    dec_ref(tuple);
                    return Some(ptr::null_mut());
                }
                let remainder_object = builtin_int_object(remainder);
                if remainder_object.is_null() {
                    dec_ref(tuple);
                    return Some(ptr::null_mut());
                }
                if super::PyTuple_SetItem(tuple, 1, remainder_object) != 0 {
                    dec_ref(remainder_object);
                    dec_ref(tuple);
                    return Some(ptr::null_mut());
                }
                tuple
            }
            _ => {
                let divisor = right.to_f64();
                if divisor == 0.0 {
                    builtin_zero_division("float divmod()")
                } else {
                    let dividend = left.to_f64();
                    let quotient = (dividend / divisor).floor();
                    let tuple = super::PyTuple_New(2);
                    if tuple.is_null() {
                        return Some(ptr::null_mut());
                    }
                    let first = builtin_float_object(quotient);
                    if first.is_null() || super::PyTuple_SetItem(tuple, 0, first) != 0 {
                        dec_ref(first);
                        dec_ref(tuple);
                        return Some(ptr::null_mut());
                    }
                    let second = builtin_float_object(dividend - quotient * divisor);
                    if second.is_null() || super::PyTuple_SetItem(tuple, 1, second) != 0 {
                        dec_ref(second);
                        dec_ref(tuple);
                        return Some(ptr::null_mut());
                    }
                    tuple
                }
            }
        },
        // nb_floor_divide
        12 => match (left, right) {
            (BuiltinNumber::Int(a), BuiltinNumber::Int(b)) => match builtin_floor_div_int(a, b) {
                Some(value) => builtin_int_object(value),
                None => builtin_zero_division("integer division or modulo by zero"),
            },
            _ => {
                let divisor = right.to_f64();
                if divisor == 0.0 {
                    builtin_zero_division("float floor division by zero")
                } else {
                    builtin_float_object((left.to_f64() / divisor).floor())
                }
            }
        },
        // nb_true_divide
        37 => {
            let divisor = right.to_f64();
            if divisor == 0.0 {
                builtin_zero_division("division by zero")
            } else {
                builtin_float_object(left.to_f64() / divisor)
            }
        }
        // nb_power
        33 => match (left, right) {
            (BuiltinNumber::Int(base), BuiltinNumber::Int(exponent)) if exponent >= 0 => {
                match u32::try_from(exponent) {
                    Ok(exponent) => match base.checked_pow(exponent) {
                        Some(value) => builtin_int_object(value),
                        None => {
                            super::set_error("OverflowError: integer power result is too large");
                            ptr::null_mut()
                        }
                    },
                    Err(_) => {
                        super::set_error("OverflowError: exponent is too large");
                        ptr::null_mut()
                    }
                }
            }
            _ => builtin_float_object(left.to_f64().powf(right.to_f64())),
        },
        // nb_lshift
        28 => match (left, right) {
            (BuiltinNumber::Int(a), BuiltinNumber::Int(b)) => match u32::try_from(b) {
                Ok(shift) => match a.checked_shl(shift) {
                    Some(value) => builtin_int_object(value),
                    None => {
                        super::set_error("OverflowError: shift result is too large");
                        ptr::null_mut()
                    }
                },
                Err(_) => {
                    super::set_error("ValueError: negative shift count");
                    ptr::null_mut()
                }
            },
            _ => return None,
        },
        // nb_rshift
        35 => match (left, right) {
            (BuiltinNumber::Int(a), BuiltinNumber::Int(b)) => match u32::try_from(b) {
                Ok(shift) => builtin_int_object(a >> shift.min(127)),
                Err(_) => {
                    super::set_error("ValueError: negative shift count");
                    ptr::null_mut()
                }
            },
            _ => return None,
        },
        // nb_and / nb_xor / nb_or
        8 | 38 | 31 => match (left, right) {
            (BuiltinNumber::Int(a), BuiltinNumber::Int(b)) => builtin_int_object(match slot {
                8 => a & b,
                38 => a ^ b,
                _ => a | b,
            }),
            _ => return None,
        },
        _ => return None,
    })
}

/// Shared implementation for binary number slots.
fn number_binary(
    slot: c_int,
    left: *mut ShimObject,
    right: *mut ShimObject,
    symbol: &str,
) -> *mut ShimObject {
    let index = number_slot_index(slot).unwrap_or(0);
    let type_object = type_of(left);
    let function = number_function(type_object, index);
    if function != 0 {
        return call_binary(function, left, right, symbol);
    }
    // Reflected operation: the right operand's slot receives the operands in
    // the opposite order, as CPython's binary_op1 does.
    let reflected = number_function(type_of(right), index);
    if reflected != 0 {
        return call_binary(reflected, right, left, symbol);
    }
    // Builtin scalars have no suite of their own; fall back to the runtime's
    // own numeric semantics after every custom slot has been consulted.
    if let Some(result) = builtin_number_binary(slot, left, right) {
        return result;
    }
    super::set_error(format!(
        "TypeError: unsupported operand type(s) for {symbol}: '{}' and '{}'",
        operator_type_name(left),
        operator_type_name(right)
    ));
    ptr::null_mut()
}

fn number_unary(slot: c_int, object: *mut ShimObject, symbol: &str) -> *mut ShimObject {
    let index = number_slot_index(slot).unwrap_or(0);
    let function = number_function(type_of(object), index);
    if function != 0 {
        return call_unary(function, object, symbol);
    }
    if let Some(result) = builtin_number_unary(slot, object) {
        return result;
    }
    super::set_error(format!(
        "TypeError: bad operand type for unary {symbol}: '{}'",
        operator_type_name(object)
    ));
    ptr::null_mut()
}

fn number_inplace(
    slot: c_int,
    left: *mut ShimObject,
    right: *mut ShimObject,
    symbol: &str,
) -> *mut ShimObject {
    let index = number_slot_index(slot).unwrap_or(0);
    let function = number_function(type_of(left), index);
    if function != 0 {
        return call_binary(function, left, right, symbol);
    }
    let reflected = number_function(type_of(right), index);
    if reflected != 0 {
        return call_binary(reflected, right, left, symbol);
    }
    if let Some(result) = builtin_number_binary(slot, left, right) {
        return result;
    }
    super::set_error(format!(
        "TypeError: unsupported operand type(s) for {symbol}: '{}' and '{}'",
        operator_type_name(left),
        operator_type_name(right)
    ));
    ptr::null_mut()
}

/// Sequence length from `sq_length`, or `-1` with a `TypeError`.
fn sequence_length_of(object: *mut ShimObject) -> isize {
    let function = sequence_function(type_of(object), 0);
    if function == 0 {
        super::set_error(format!(
            "TypeError: object of type '{}' has no len()",
            operator_type_name(object)
        ));
        return -1;
    }
    // SAFETY: lenfunc has this signature.
    let function: unsafe extern "C" fn(*mut ShimObject) -> isize =
        unsafe { std::mem::transmute(function as *const c_void) };
    let _gripper = GilGuard::new();
    let length = unsafe { function(object) };
    if length < 0 && !super::error_pending() {
        super::set_error("ValueError: __len__() should return >= 0");
        return -1;
    }
    length
}

fn mapping_length_of(object: *mut ShimObject) -> isize {
    let function = mapping_function(type_of(object), 0);
    if function == 0 {
        // Fall back to the sequence protocol.
        return sequence_length_of(object);
    }
    // SAFETY: lenfunc has this signature.
    let function: unsafe extern "C" fn(*mut ShimObject) -> isize =
        unsafe { std::mem::transmute(function as *const c_void) };
    let _gripper = GilGuard::new();
    let length = unsafe { function(object) };
    if length < 0 && !super::error_pending() {
        super::set_error("ValueError: __len__() should return >= 0");
        return -1;
    }
    length
}

/// `PyObject_Length`/`PyObject_Size` body.
pub(crate) fn object_length(object: *mut ShimObject) -> isize {
    if object.is_null() {
        super::set_error("SystemError: PyObject_Length called with NULL");
        return -1;
    }
    if let Some(kind) = super::kind_ref(object) {
        match kind {
            super::ShimKind::Dict(pairs) => return super::py_ssize_of(pairs.len()),
            super::ShimKind::DictProxy { dict } => {
                if let Some(super::ShimKind::Dict(pairs)) =
                    super::kind_ref(*dict as *mut ShimObject)
                {
                    return super::py_ssize_of(pairs.len());
                }
            }
            super::ShimKind::List(items) => return super::py_ssize_of(items.len()),
            super::ShimKind::Tuple(items) => return super::py_ssize_of(items.len()),
            super::ShimKind::Str { text, .. } => return super::py_ssize_of(text.chars().count()),
            super::ShimKind::Bytes { data, .. } => return super::py_ssize_of(data.len()),
            // Hydro containers and user objects resolve `len()` in Hydro.
            super::ShimKind::Hydro(_) => return super::hydro_length(object),
            _ => {}
        }
    }
    let type_object = type_of(object);
    if mapping_function(type_object, 0) != 0 {
        return mapping_length_of(object);
    }
    sequence_length_of(object)
}

/// Reads `index` from a builtin list/tuple payload, returning a new reference.
fn builtin_sequence_item(items: &[usize], index: i64, kind: &str) -> *mut ShimObject {
    let length = items.len() as i64;
    let resolved = if index < 0 { index + length } else { index };
    if resolved < 0 || resolved >= length {
        super::set_error(format!("IndexError: {kind} index out of range"));
        return ptr::null_mut();
    }
    let item = items[resolved as usize] as *mut ShimObject;
    if item.is_null() {
        super::set_error(format!("SystemError: {kind} item is uninitialized"));
        return ptr::null_mut();
    }
    inc_ref(item);
    item
}

/// Fetches `index` through the sequence protocol without boxing an integer key.
///
/// Builtin lists and tuples do not publish a `PySequenceMethods` suite, so
/// their payload is read directly; every other object goes through `sq_item`.
pub(crate) fn sequence_item_at(object: *mut ShimObject, index: isize) -> *mut ShimObject {
    if let Some(super::ShimKind::List(items)) = super::kind_ref(object) {
        return builtin_sequence_item(items, index as i64, "list");
    }
    if let Some(super::ShimKind::Tuple(items)) = super::kind_ref(object) {
        return builtin_sequence_item(items, index as i64, "tuple");
    }
    PySequence_GetItem(object, index)
}

/// Fetches an item through `mp_subscript` or `sq_item`.
pub(crate) fn object_getitem(object: *mut ShimObject, key: *mut ShimObject) -> *mut ShimObject {
    if object.is_null() || key.is_null() {
        super::set_error("SystemError: PyObject_GetItem called with NULL");
        return ptr::null_mut();
    }
    if let Some(kind) = super::kind_ref(object) {
        match kind {
            super::ShimKind::DictProxy { dict } => {
                let item = super::PyDict_GetItem(*dict as *mut ShimObject, key);
                if item.is_null() {
                    super::set_error(format!("KeyError: {}", super::shim_repr(key)));
                } else {
                    inc_ref(item);
                }
                return item;
            }
            super::ShimKind::Dict(_) => {
                let item = super::PyDict_GetItem(object, key);
                if item.is_null() {
                    super::set_error(format!("KeyError: {}", super::shim_repr(key)));
                } else {
                    inc_ref(item);
                }
                return item;
            }
            super::ShimKind::List(items) => {
                let index = match super::as_int(key) {
                    Ok(index) => index,
                    Err(error) => {
                        super::set_error(format!("TypeError: {error}"));
                        return ptr::null_mut();
                    }
                };
                return builtin_sequence_item(items, index, "list");
            }
            super::ShimKind::Tuple(items) => {
                let index = match super::as_int(key) {
                    Ok(index) => index,
                    Err(error) => {
                        super::set_error(format!("TypeError: {error}"));
                        return ptr::null_mut();
                    }
                };
                return builtin_sequence_item(items, index, "tuple");
            }
            _ => {}
        }
    }
    let type_object = type_of(object);
    let subscript = mapping_function(type_object, 1);
    if subscript != 0 {
        return call_binary(subscript, object, key, "__getitem__");
    }
    let item = sequence_function(type_object, 3);
    if item != 0 {
        let index = match super::as_int(key) {
            Ok(index) => index,
            Err(error) => {
                super::set_error(format!(
                    "TypeError: sequence index must be an integer ({error})"
                ));
                return ptr::null_mut();
            }
        };
        // SAFETY: ssizeargfunc has this signature.
        let function: unsafe extern "C" fn(*mut ShimObject, isize) -> *mut ShimObject =
            unsafe { std::mem::transmute(item as *const c_void) };
        let _gripper = GilGuard::new();
        return operator_result(unsafe { function(object, index as isize) }, "__getitem__");
    }
    super::set_error(format!(
        "TypeError: '{}' object is not subscriptable",
        operator_type_name(object)
    ));
    ptr::null_mut()
}

/// Stores an item through `mp_ass_subscript` or `sq_ass_item`.
pub(crate) fn object_setitem(
    object: *mut ShimObject,
    key: *mut ShimObject,
    value: *mut ShimObject,
) -> c_int {
    if object.is_null() || key.is_null() {
        super::set_error("SystemError: PyObject_SetItem called with NULL");
        return -1;
    }
    if let Some(kind) = super::kind_ref(object) {
        match kind {
            super::ShimKind::Dict(_) => {
                if value.is_null() {
                    // Deletion is not implemented on shim dicts.
                    super::set_error("TypeError: dict item deletion is not supported");
                    return -1;
                }
                return match super::PyDict_SetItem(object, key, value) {
                    0 => 0,
                    status => status,
                };
            }
            super::ShimKind::List(items) => {
                let index = match super::as_int(key) {
                    Ok(index) => index,
                    Err(error) => {
                        super::set_error(format!("TypeError: {error}"));
                        return -1;
                    }
                };
                let length = items.len() as i64;
                let resolved = if index < 0 { index + length } else { index };
                if resolved < 0 || resolved >= length {
                    super::set_error("IndexError: list assignment index out of range");
                    return -1;
                }
                if value.is_null() {
                    super::set_error("TypeError: list item deletion is not supported");
                    return -1;
                }
                let item = value as usize;
                inc_ref(value);
                let previous = {
                    let Ok(slots) = super::list_slots_mut(object) else {
                        dec_ref(value);
                        super::set_error("TypeError: expected a list object");
                        return -1;
                    };
                    let previous = slots[resolved as usize];
                    slots[resolved as usize] = item;
                    previous
                };
                if previous != 0 {
                    dec_ref(previous as *mut ShimObject);
                }
                return 0;
            }
            _ => {}
        }
    }
    let type_object = type_of(object);
    let assign = mapping_function(type_object, 2);
    if assign != 0 {
        // SAFETY: objobjargproc has this signature.
        let function: unsafe extern "C" fn(
            *mut ShimObject,
            *mut ShimObject,
            *mut ShimObject,
        ) -> c_int = unsafe { std::mem::transmute(assign as *const c_void) };
        let _gripper = GilGuard::new();
        let status = unsafe { function(object, key, value) };
        if status != 0 && !super::error_pending() {
            super::set_error("SystemError: __setitem__ failed without setting an exception");
        }
        return status;
    }
    let assign = sequence_function(type_object, 5);
    if assign != 0 {
        let index = match super::as_int(key) {
            Ok(index) => index,
            Err(error) => {
                super::set_error(format!(
                    "TypeError: sequence index must be an integer ({error})"
                ));
                return -1;
            }
        };
        // SAFETY: ssizeobjargproc has this signature.
        let function: unsafe extern "C" fn(*mut ShimObject, isize, *mut ShimObject) -> c_int =
            unsafe { std::mem::transmute(assign as *const c_void) };
        let _gripper = GilGuard::new();
        let status = unsafe { function(object, index as isize, value) };
        if status != 0 && !super::error_pending() {
            super::set_error("SystemError: __setitem__ failed without setting an exception");
        }
        return status;
    }
    super::set_error(format!(
        "TypeError: '{}' object does not support item assignment",
        operator_type_name(object)
    ));
    -1
}

/// Truthiness of any handle, consulting `nb_bool` first.
pub(crate) fn object_is_true(object: *mut ShimObject) -> c_int {
    if object.is_null() {
        super::set_error("SystemError: PyObject_IsTrue called with NULL");
        return -1;
    }
    // Hydro values use Hydro's truth rules (containers by length, objects by
    // their class's `__bool__`/`__len__` support).
    if let Some(value) = super::hydro_tagged(object) {
        return crate::value::truthy(value) as c_int;
    }
    let function = number_function(type_of(object), 9);
    if function != 0 {
        // SAFETY: inquiry has this signature.
        let function: unsafe extern "C" fn(*mut ShimObject) -> c_int =
            unsafe { std::mem::transmute(function as *const c_void) };
        let _gripper = GilGuard::new();
        let result = unsafe { function(object) };
        if result < 0 && !super::error_pending() {
            super::set_error("SystemError: __bool__ failed without setting an exception");
            return -1;
        }
        return (result != 0) as c_int;
    }
    // Fall back to `__len__` for types that define one.
    let type_object = type_of(object);
    if mapping_function(type_object, 0) != 0 || sequence_function(type_object, 0) != 0 {
        let length = object_length(object);
        if length < 0 {
            return -1;
        }
        return (length != 0) as c_int;
    }
    match super::as_predicate(object) {
        Ok(value) => value as c_int,
        Err(error) => {
            super::set_error(format!("TypeError: {error}"));
            -1
        }
    }
}

// ---------------------------------------------------------------------------
// Number, sequence and mapping API
// ---------------------------------------------------------------------------

macro_rules! binary_number_api {
    ($($name:ident => $slot:expr, $symbol:literal;)+) => {
        $(
            /// CPython binary-number entry point.
            #[no_mangle]
            pub extern "C" fn $name(
                left: *mut ShimObject,
                right: *mut ShimObject,
            ) -> *mut ShimObject {
                number_binary($slot, left, right, $symbol)
            }
        )+
    };
}

macro_rules! unary_number_api {
    ($($name:ident => $slot:expr, $symbol:literal;)+) => {
        $(
            /// CPython unary-number entry point.
            #[no_mangle]
            pub extern "C" fn $name(object: *mut ShimObject) -> *mut ShimObject {
                number_unary($slot, object, $symbol)
            }
        )+
    };
}

macro_rules! inplace_number_api {
    ($($name:ident => $slot:expr, $symbol:literal;)+) => {
        $(
            /// CPython in-place-number entry point.
            #[no_mangle]
            pub extern "C" fn $name(
                left: *mut ShimObject,
                right: *mut ShimObject,
            ) -> *mut ShimObject {
                number_inplace($slot, left, right, $symbol)
            }
        )+
    };
}

binary_number_api! {
    PyNumber_Add => 7, "+";
    PyNumber_Subtract => 36, "-";
    PyNumber_Multiply => 29, "*";
    PyNumber_Remainder => 34, "%";
    PyNumber_Divmod => 10, "divmod()";
    PyNumber_Lshift => 28, "<<";
    PyNumber_Rshift => 35, ">>";
    PyNumber_And => 8, "&";
    PyNumber_Xor => 38, "^";
    PyNumber_Or => 31, "|";
    PyNumber_FloorDivide => 12, "//";
    PyNumber_TrueDivide => 37, "/";
    PyNumber_MatrixMultiply => 75, "@";
}

unary_number_api! {
    PyNumber_Negative => 30, "-";
    PyNumber_Positive => 32, "+";
    PyNumber_Absolute => 6, "abs()";
    PyNumber_Invert => 27, "~";
    PyNumber_Int => 26, "int()";
    PyNumber_Long => 26, "int()";
    PyNumber_Float => 11, "float()";
    PyNumber_Index => 13, "index()";
}

inplace_number_api! {
    PyNumber_InPlaceAdd => 14, "+=";
    PyNumber_InPlaceSubtract => 23, "-=";
    PyNumber_InPlaceMultiply => 18, "*=";
    PyNumber_InPlaceRemainder => 21, "%=";
    PyNumber_InPlaceLshift => 17, "<<=";
    PyNumber_InPlaceRshift => 22, ">>=";
    PyNumber_InPlaceAnd => 15, "&=";
    PyNumber_InPlaceXor => 25, "^=";
    PyNumber_InPlaceOr => 19, "|=";
    PyNumber_InPlaceFloorDivide => 16, "//=";
    PyNumber_InPlaceTrueDivide => 24, "/=";
    PyNumber_InPlaceMatrixMultiply => 76, "@=";
}

/// `PyNumber_Power`: ternary (`nb_power`) with a non-NULL modulus.
#[no_mangle]
pub extern "C" fn PyNumber_Power(
    base: *mut ShimObject,
    exponent: *mut ShimObject,
    modulus: *mut ShimObject,
) -> *mut ShimObject {
    let function = number_function(type_of(base), 5);
    if function == 0 {
        // Fall back to the right operand's power slot (reflected operation).
        let reflected = number_function(type_of(exponent), 5);
        if reflected != 0 {
            return call_ternary(reflected, exponent, base, modulus, "**");
        }
        if modulus.is_null() {
            if let Some(result) = builtin_number_binary(33, base, exponent) {
                return result;
            }
        }
        super::set_error(format!(
            "TypeError: unsupported operand type(s) for ** or pow(): '{}' and '{}'",
            operator_type_name(base),
            operator_type_name(exponent)
        ));
        return ptr::null_mut();
    }
    call_ternary(function, base, exponent, modulus, "**")
}

/// `PyNumber_InPlacePower`.
#[no_mangle]
pub extern "C" fn PyNumber_InPlacePower(
    base: *mut ShimObject,
    exponent: *mut ShimObject,
    modulus: *mut ShimObject,
) -> *mut ShimObject {
    let function = number_function(type_of(base), 23);
    if function == 0 {
        return PyNumber_Power(base, exponent, modulus);
    }
    call_ternary(function, base, exponent, modulus, "**=")
}

/// `PyIndex_Check`: the type defines `nb_index`.
#[no_mangle]
pub extern "C" fn PyIndex_Check(object: *mut ShimObject) -> c_int {
    if object.is_null() {
        return 0;
    }
    if let Some(super::ShimKind::Int(_) | super::ShimKind::Bool(_)) = super::kind_ref(object) {
        return 1;
    }
    (number_function(type_of(object), 33) != 0) as c_int
}

/// `PyNumber_Check`: the type defines any forward number operation.
#[no_mangle]
pub extern "C" fn PyNumber_Check(object: *mut ShimObject) -> c_int {
    if object.is_null() {
        return 0;
    }
    if let Some(super::ShimKind::Int(_) | super::ShimKind::Bool(_) | super::ShimKind::Float(_)) =
        super::kind_ref(object)
    {
        return 1;
    }
    let type_object = type_of(object);
    let suite = unsafe { (*type_object).tp_as_number };
    (!suite.is_null()
        && (number_function(type_object, 0) != 0
            || number_function(type_object, 6) != 0
            || number_function(type_object, 16) != 0
            || number_function(type_object, 18) != 0
            || number_function(type_object, 33) != 0)) as c_int
}

/// `PyNumber_AsSsize_t`.
#[no_mangle]
pub extern "C" fn PyNumber_AsSsize_t(object: *mut ShimObject, exception: *mut ShimObject) -> isize {
    let index = number_function(type_of(object), 33);
    let converted = if index != 0 {
        call_unary(index, object, "__index__")
    } else {
        inc_ref(object);
        object
    };
    if converted.is_null() {
        return -1;
    }
    let outcome = super::as_int(converted);
    dec_ref(converted);
    match outcome {
        Ok(value) => isize::try_from(value).unwrap_or_else(|_| {
            if !exception.is_null() {
                // SAFETY: the caller supplies a live exception class.
                super::PyErr_SetString(exception, c"integer out of range".as_ptr());
            } else {
                super::set_error("OverflowError: integer out of range for Py_ssize_t");
            }
            -1
        }),
        Err(error) => {
            super::set_error(format!("TypeError: {error}"));
            -1
        }
    }
}

// -- sequence ---------------------------------------------------------------

/// `PySequence_Length`.
#[no_mangle]
pub extern "C" fn PySequence_Length(object: *mut ShimObject) -> isize {
    if object.is_null() {
        super::set_error("SystemError: PySequence_Length called with NULL");
        return -1;
    }
    sequence_length_of(object)
}

/// `PySequence_Size`: alias of [`PySequence_Length`].
#[no_mangle]
pub extern "C" fn PySequence_Size(object: *mut ShimObject) -> isize {
    PySequence_Length(object)
}

/// `PySequence_Check`: the type defines `sq_item`.
#[no_mangle]
pub extern "C" fn PySequence_Check(object: *mut ShimObject) -> c_int {
    if object.is_null() {
        return 0;
    }
    (sequence_function(type_of(object), 3) != 0) as c_int
}

/// `PySequence_GetItem`.
#[no_mangle]
pub extern "C" fn PySequence_GetItem(object: *mut ShimObject, index: isize) -> *mut ShimObject {
    let function = sequence_function(type_of(object), 3);
    if function == 0 {
        super::set_error(format!(
            "TypeError: '{}' object is not subscriptable",
            operator_type_name(object)
        ));
        return ptr::null_mut();
    }
    // SAFETY: ssizeargfunc has this signature.
    let function: unsafe extern "C" fn(*mut ShimObject, isize) -> *mut ShimObject =
        unsafe { std::mem::transmute(function as *const c_void) };
    let _gripper = GilGuard::new();
    operator_result(unsafe { function(object, index) }, "PySequence_GetItem")
}

/// `PySequence_SetItem`.
#[no_mangle]
pub extern "C" fn PySequence_SetItem(
    object: *mut ShimObject,
    index: isize,
    value: *mut ShimObject,
) -> c_int {
    let function = sequence_function(type_of(object), 5);
    if function == 0 {
        super::set_error(format!(
            "TypeError: '{}' object does not support item assignment",
            operator_type_name(object)
        ));
        return -1;
    }
    // SAFETY: ssizeobjargproc has this signature.
    let function: unsafe extern "C" fn(*mut ShimObject, isize, *mut ShimObject) -> c_int =
        unsafe { std::mem::transmute(function as *const c_void) };
    let _gripper = GilGuard::new();
    let status = unsafe { function(object, index, value) };
    if status != 0 && !super::error_pending() {
        super::set_error("SystemError: PySequence_SetItem failed without setting an exception");
    }
    status
}

/// `PySequence_DelItem`.
#[no_mangle]
pub extern "C" fn PySequence_DelItem(object: *mut ShimObject, index: isize) -> c_int {
    PySequence_SetItem(object, index, ptr::null_mut())
}

/// `PySequence_Concat`.
#[no_mangle]
pub extern "C" fn PySequence_Concat(
    left: *mut ShimObject,
    right: *mut ShimObject,
) -> *mut ShimObject {
    let function = sequence_function(type_of(left), 1);
    if function == 0 {
        super::set_error(format!(
            "TypeError: cannot concatenate '{}' and '{}' objects",
            operator_type_name(left),
            operator_type_name(right)
        ));
        return ptr::null_mut();
    }
    call_binary(function, left, right, "PySequence_Concat")
}

/// `PySequence_Repeat`.
#[no_mangle]
pub extern "C" fn PySequence_Repeat(object: *mut ShimObject, count: isize) -> *mut ShimObject {
    let function = sequence_function(type_of(object), 2);
    if function == 0 {
        super::set_error(format!(
            "TypeError: '{}' object cannot be repeated",
            operator_type_name(object)
        ));
        return ptr::null_mut();
    }
    // SAFETY: ssizeargfunc has this signature.
    let function: unsafe extern "C" fn(*mut ShimObject, isize) -> *mut ShimObject =
        unsafe { std::mem::transmute(function as *const c_void) };
    let _gripper = GilGuard::new();
    operator_result(unsafe { function(object, count) }, "PySequence_Repeat")
}

/// `PySequence_InPlaceConcat`.
#[no_mangle]
pub extern "C" fn PySequence_InPlaceConcat(
    left: *mut ShimObject,
    right: *mut ShimObject,
) -> *mut ShimObject {
    let function = sequence_function(type_of(left), 8);
    if function == 0 {
        return PySequence_Concat(left, right);
    }
    call_binary(function, left, right, "PySequence_InPlaceConcat")
}

/// `PySequence_InPlaceRepeat`.
#[no_mangle]
pub extern "C" fn PySequence_InPlaceRepeat(
    object: *mut ShimObject,
    count: isize,
) -> *mut ShimObject {
    let function = sequence_function(type_of(object), 9);
    if function == 0 {
        return PySequence_Repeat(object, count);
    }
    // SAFETY: ssizeargfunc has this signature.
    let function: unsafe extern "C" fn(*mut ShimObject, isize) -> *mut ShimObject =
        unsafe { std::mem::transmute(function as *const c_void) };
    let _gripper = GilGuard::new();
    operator_result(
        unsafe { function(object, count) },
        "PySequence_InPlaceRepeat",
    )
}

/// `PySequence_Contains`.
#[no_mangle]
pub extern "C" fn PySequence_Contains(object: *mut ShimObject, item: *mut ShimObject) -> c_int {
    if object.is_null() || item.is_null() {
        super::set_error("SystemError: PySequence_Contains called with NULL");
        return -1;
    }
    let function = sequence_function(type_of(object), 7);
    if function != 0 {
        // SAFETY: objobjproc has this signature.
        let function: unsafe extern "C" fn(*mut ShimObject, *mut ShimObject) -> c_int =
            unsafe { std::mem::transmute(function as *const c_void) };
        let _gripper = GilGuard::new();
        let result = unsafe { function(object, item) };
        if result < 0 && !super::error_pending() {
            super::set_error("SystemError: __contains__ failed without setting an exception");
            return -1;
        }
        return (result != 0) as c_int;
    }
    match super::kind_ref(object) {
        Some(super::ShimKind::Dict(pairs)) => {
            for &(key, _) in pairs {
                if key != 0 && super::key_equal(key as *mut ShimObject, item) {
                    return 1;
                }
            }
            0
        }
        Some(super::ShimKind::List(items)) => {
            for &entry in items {
                if entry == 0 {
                    continue;
                }
                let status = PyObject_RichCompareBool(
                    entry as *mut ShimObject,
                    item,
                    2, // Py_EQ
                );
                if status != 0 {
                    return status;
                }
            }
            0
        }
        Some(super::ShimKind::Tuple(items)) => {
            for &entry in items.iter() {
                if entry == 0 {
                    continue;
                }
                let status = PyObject_RichCompareBool(
                    entry as *mut ShimObject,
                    item,
                    2, // Py_EQ
                );
                if status != 0 {
                    return status;
                }
            }
            0
        }
        _ => {
            super::set_error(format!(
                "TypeError: argument of type '{}' is not iterable",
                operator_type_name(object)
            ));
            -1
        }
    }
}

// -- mapping ----------------------------------------------------------------

/// `PyMapping_Length`.
#[no_mangle]
pub extern "C" fn PyMapping_Length(object: *mut ShimObject) -> isize {
    if object.is_null() {
        super::set_error("SystemError: PyMapping_Length called with NULL");
        return -1;
    }
    mapping_length_of(object)
}

/// `PyMapping_Size`: alias of [`PyMapping_Length`].
#[no_mangle]
pub extern "C" fn PyMapping_Size(object: *mut ShimObject) -> isize {
    PyMapping_Length(object)
}

/// `PyMapping_GetItemString`.
#[no_mangle]
pub extern "C" fn PyMapping_GetItemString(
    object: *mut ShimObject,
    key: *const c_char,
) -> *mut ShimObject {
    if key.is_null() {
        super::set_error("TypeError: PyMapping_GetItemString key is NULL");
        return ptr::null_mut();
    }
    let key = super::new_str(read_c_string(key));
    let result = object_getitem(object, key);
    dec_ref(key);
    result
}

/// `PyMapping_SetItemString`.
#[no_mangle]
pub extern "C" fn PyMapping_SetItemString(
    object: *mut ShimObject,
    key: *const c_char,
    value: *mut ShimObject,
) -> c_int {
    if key.is_null() {
        super::set_error("TypeError: PyMapping_SetItemString key is NULL");
        return -1;
    }
    let key = super::new_str(read_c_string(key));
    let status = object_setitem(object, key, value);
    dec_ref(key);
    status
}

// -- abstract object operations ---------------------------------------------

/// `PyObject_Length`.
#[no_mangle]
pub extern "C" fn PyObject_Length(object: *mut ShimObject) -> isize {
    object_length(object)
}

/// `PyObject_Size`: alias of [`PyObject_Length`].
#[no_mangle]
pub extern "C" fn PyObject_Size(object: *mut ShimObject) -> isize {
    object_length(object)
}

/// `PyObject_GetItem`.
#[no_mangle]
pub extern "C" fn PyObject_GetItem(
    object: *mut ShimObject,
    key: *mut ShimObject,
) -> *mut ShimObject {
    object_getitem(object, key)
}

/// `PyObject_SetItem`.
#[no_mangle]
pub extern "C" fn PyObject_SetItem(
    object: *mut ShimObject,
    key: *mut ShimObject,
    value: *mut ShimObject,
) -> c_int {
    object_setitem(object, key, value)
}

/// `PyObject_DelItem`.
#[no_mangle]
pub extern "C" fn PyObject_DelItem(object: *mut ShimObject, key: *mut ShimObject) -> c_int {
    object_setitem(object, key, ptr::null_mut())
}

/// `PyObject_IsTrue`.
#[no_mangle]
pub extern "C" fn PyObject_IsTrue(object: *mut ShimObject) -> c_int {
    object_is_true(object)
}

/// `PyObject_Not`.
#[no_mangle]
pub extern "C" fn PyObject_Not(object: *mut ShimObject) -> c_int {
    match object_is_true(object) {
        0 => 1,
        value if value > 0 => 0,
        _ => -1,
    }
}

/// `PyCallable_Check`.
#[no_mangle]
pub extern "C" fn PyCallable_Check(object: *mut ShimObject) -> c_int {
    if object.is_null() {
        return 0;
    }
    ensure_builtin_types();
    if is_type_pointer(object) {
        return 1;
    }
    // SAFETY: every handle starts with the CPython header.
    let type_object = unsafe { (*object).head.ob_type };
    if type_object.is_null() {
        return 0;
    }
    if let Some(super::ShimKind::CFunction { .. }) = super::kind_ref(object) {
        return 1;
    }
    if let Some(super::ShimKind::RuntimeFunction { .. }) = super::kind_ref(object) {
        return 1;
    }
    if let Some(value) = super::hydro_tagged(object) {
        return super::hydro_value_callable(value) as c_int;
    }
    // SAFETY: live type object.
    unsafe {
        ((*type_object).tp_call != 0
            || ((*type_object).tp_flags & PY_TPFLAGS_HAVE_VECTORCALL != 0
                && (*type_object).tp_vectorcall_offset != 0)) as c_int
    }
}

/// `PyObject_SelfIter`: returns `object` with a new reference.
#[no_mangle]
pub extern "C" fn PyObject_SelfIter(object: *mut ShimObject) -> *mut ShimObject {
    inc_ref(object);
    object
}

/// `PyObject_GetIter`.
#[no_mangle]
pub extern "C" fn PyObject_GetIter(object: *mut ShimObject) -> *mut ShimObject {
    if object.is_null() {
        super::set_error("SystemError: PyObject_GetIter called with NULL");
        return ptr::null_mut();
    }
    // SAFETY: live type object.
    let iterator = unsafe { (*type_of(object)).tp_iter };
    if iterator == 0 {
        super::set_error(format!(
            "TypeError: '{}' object is not iterable",
            operator_type_name(object)
        ));
        return ptr::null_mut();
    }
    // SAFETY: getiterfunc has this signature.
    let iterator: unsafe extern "C" fn(*mut ShimObject) -> *mut ShimObject =
        unsafe { std::mem::transmute(iterator as *const c_void) };
    let _gripper = GilGuard::new();
    operator_result(unsafe { iterator(object) }, "PyObject_GetIter")
}

/// `PyIter_Check`: the type defines `tp_iternext`.
#[no_mangle]
pub extern "C" fn PyIter_Check(object: *mut ShimObject) -> c_int {
    if object.is_null() {
        return 0;
    }
    // SAFETY: live type object.
    (!object.is_null() && unsafe { (*type_of(object)).tp_iternext } != 0) as c_int
}

/// `PyIter_Next`.
#[no_mangle]
pub extern "C" fn PyIter_Next(object: *mut ShimObject) -> *mut ShimObject {
    if object.is_null() {
        super::set_error("SystemError: PyIter_Next called with NULL");
        return ptr::null_mut();
    }
    // SAFETY: live type object.
    let next = unsafe { (*type_of(object)).tp_iternext };
    if next == 0 {
        super::set_error(format!(
            "TypeError: '{}' object is not an iterator",
            operator_type_name(object)
        ));
        return ptr::null_mut();
    }
    // SAFETY: iternextfunc has this signature.
    let next: unsafe extern "C" fn(*mut ShimObject) -> *mut ShimObject =
        unsafe { std::mem::transmute(next as *const c_void) };
    let _gripper = GilGuard::new();
    let result = unsafe { next(object) };
    if result.is_null() && !super::error_pending() {
        // CPython's exhaustion protocol: NULL with StopIteration set.
        // Ensure the exported exception singleton is initialized before
        // naming it; the static is null until the hierarchy is built.
        super::initialize_exception_types();
        // SAFETY: the exception class is a live exported singleton.
        unsafe { super::PyErr_SetNone(super::PyExc_StopIteration) };
    }
    result
}

/// `PyObject_Next`: compatibility spelling of [`PyIter_Next`].
#[no_mangle]
pub extern "C" fn PyObject_Next(object: *mut ShimObject) -> *mut ShimObject {
    PyIter_Next(object)
}

// -- rich comparison --------------------------------------------------------

const PY_LT: c_int = 0;
const PY_LE: c_int = 1;
const PY_EQ: c_int = 2;
const PY_NE: c_int = 3;
const PY_GT: c_int = 4;
const PY_GE: c_int = 5;

fn compare_shim(left: *mut ShimObject, right: *mut ShimObject, op: c_int) -> Option<bool> {
    let left_kind = super::kind_ref(left)?;
    let right_kind = super::kind_ref(right)?;
    let ordering = match (left_kind, right_kind) {
        (super::ShimKind::Int(a), super::ShimKind::Int(b)) => (*a).partial_cmp(b),
        (super::ShimKind::Bool(a), super::ShimKind::Bool(b)) => (*a).partial_cmp(b),
        (super::ShimKind::Int(a), super::ShimKind::Float(b)) => (*a as f64).partial_cmp(b),
        (super::ShimKind::Float(a), super::ShimKind::Int(b)) => a.partial_cmp(&(*b as f64)),
        (super::ShimKind::Float(a), super::ShimKind::Float(b)) => a.partial_cmp(b),
        (super::ShimKind::Str { text: a, .. }, super::ShimKind::Str { text: b, .. }) => {
            Some(a.cmp(b))
        }
        (super::ShimKind::Bytes { data: a, .. }, super::ShimKind::Bytes { data: b, .. }) => {
            Some(a.cmp(b))
        }
        _ => return None,
    };
    use std::cmp::Ordering;
    Some(match (op, ordering) {
        (_, None) => false,
        (PY_EQ, Some(Ordering::Equal)) => true,
        (PY_EQ, _) => false,
        (PY_NE, Some(Ordering::Equal)) => false,
        (PY_NE, _) => true,
        (PY_LT, Some(Ordering::Less)) => true,
        (PY_LE, Some(Ordering::Less | Ordering::Equal)) => true,
        (PY_GT, Some(Ordering::Greater)) => true,
        (PY_GE, Some(Ordering::Greater | Ordering::Equal)) => true,
        _ => false,
    })
}

/// `PyObject_RichCompare`.
#[no_mangle]
pub extern "C" fn PyObject_RichCompare(
    left: *mut ShimObject,
    right: *mut ShimObject,
    op: c_int,
) -> *mut ShimObject {
    if !(PY_LT..=PY_GE).contains(&op) {
        super::set_error(format!("SystemError: invalid rich comparison op {op}"));
        return ptr::null_mut();
    }
    if left.is_null() || right.is_null() {
        super::set_error("SystemError: PyObject_RichCompare called with NULL");
        return ptr::null_mut();
    }
    // At least one operand is Hydro-backed: compare both through Hydro so a
    // user-defined `__eq__`/`__lt__` runs and identity is preserved.
    if super::hydro_tagged(left).is_some() || super::hydro_tagged(right).is_some() {
        return match super::hydro_richcompare(left, right, op) {
            Ok(result) => bool_object(result),
            Err(error) => {
                super::set_error(error);
                ptr::null_mut()
            }
        };
    }
    let function = unsafe { (*type_of(left)).tp_richcompare };
    let function = if function != 0 {
        function
    } else {
        // SAFETY: live type object.
        unsafe { (*type_of(right)).tp_richcompare }
    };
    if function != 0 {
        // SAFETY: richcmpfunc has this signature.
        let function: unsafe extern "C" fn(
            *mut ShimObject,
            *mut ShimObject,
            c_int,
        ) -> *mut ShimObject = unsafe { std::mem::transmute(function as *const c_void) };
        let _gripper = GilGuard::new();
        return operator_result(unsafe { function(left, right, op) }, "PyObject_RichCompare");
    }
    if op == PY_EQ || op == PY_NE {
        let equal = left == right;
        return bool_object(if op == PY_EQ { equal } else { !equal });
    }
    if let Some(value) = compare_shim(left, right, op) {
        return bool_object(value);
    }
    super::set_error(format!(
        "TypeError: '{}' not supported between instances of '{}' and '{}'",
        match op {
            PY_LT => "<",
            PY_LE => "<=",
            PY_GT => ">",
            PY_GE => ">=",
            _ => "comparison",
        },
        operator_type_name(left),
        operator_type_name(right)
    ));
    ptr::null_mut()
}

/// `PyObject_RichCompareBool`.
#[no_mangle]
pub extern "C" fn PyObject_RichCompareBool(
    left: *mut ShimObject,
    right: *mut ShimObject,
    op: c_int,
) -> c_int {
    if !(PY_LT..=PY_GE).contains(&op) {
        super::set_error(format!("SystemError: invalid rich comparison op {op}"));
        return -1;
    }
    if left == right && (op == PY_EQ || op == PY_NE) {
        return (op == PY_EQ) as c_int;
    }
    let result = PyObject_RichCompare(left, right, op);
    if result.is_null() {
        return -1;
    }
    if let Some(super::ShimKind::NotImplemented) = super::kind_ref(result) {
        dec_ref(result);
        return match op {
            PY_EQ => 0,
            PY_NE => 1,
            _ => {
                super::set_error(format!(
                    "TypeError: '{}' not supported between instances of '{}' and '{}'",
                    "comparison",
                    operator_type_name(left),
                    operator_type_name(right)
                ));
                -1
            }
        };
    }
    let truth = object_is_true(result);
    dec_ref(result);
    truth
}

// -- type-level helpers -----------------------------------------------------

/// `PyType_Freeze`: marks a type immutable and ready.
#[no_mangle]
pub extern "C" fn PyType_Freeze(object: *mut ShimObject) -> c_int {
    let type_object = match as_type(object) {
        Ok(type_object) => type_object,
        Err(error) => {
            super::set_error(error);
            return -1;
        }
    };
    // SAFETY: validated type object.
    unsafe {
        (*type_object).tp_flags |= PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_READY;
    }
    0
}

/// `PyType_IsSubtype`.
#[no_mangle]
pub extern "C" fn PyType_IsSubtype(type_object: *mut ShimObject, base: *mut ShimObject) -> c_int {
    // Hydro classes answer from their own linearization.
    if let Some(derived) = super::hydro_class_value(type_object) {
        if let Some(base_class) = super::hydro_class_value(base) {
            return match super::hydro_issubclass(derived, base_class) {
                Ok(subtype) => subtype as c_int,
                Err(_) => 0,
            };
        }
        // A class object is an instance of `type`; its synthetic metatype
        // derives from `type`, which is what PyO3's PyType_Check exercises.
        if base == ptr::addr_of_mut!(TYPE_TYPE) as *mut ShimObject {
            return 1;
        }
        if let Some(metatype) = hydro_class_metatype(derived) {
            if is_type_pointer(base) {
                return is_subtype(metatype, base as *mut TypeObject) as c_int;
            }
        }
        return 0;
    }
    if !is_type_pointer(type_object) || !is_type_pointer(base) {
        return 0;
    }
    is_subtype(type_object as *mut TypeObject, base as *mut TypeObject) as c_int
}

/// `PyType_Modified`: this host has no attribute caches, so it is a no-op.
#[no_mangle]
pub extern "C" fn PyType_Modified(_object: *mut ShimObject) {}

/// `PyUnstable_Object_IsUniquelyReferenced`.
#[no_mangle]
pub extern "C" fn PyUnstable_Object_IsUniquelyReferenced(object: *mut ShimObject) -> c_int {
    if object.is_null() {
        return 0;
    }
    // SAFETY: every handle starts with the CPython header.
    let count = unsafe { (*object).head.ob_refcnt.load(Ordering::Relaxed) };
    (count == 1) as c_int
}
