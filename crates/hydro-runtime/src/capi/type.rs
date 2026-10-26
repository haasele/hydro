//! Real CPython 3.14 `PyTypeObject` layout and the type/object allocation API.
//!
//! Unlike the previous opaque `TypeHandle`, every object handed to an
//! extension is now a CPython-layout handle: instances start with
//! `PyObject_HEAD` (`ob_refcnt`, `ob_type`), variable-size instances also carry
//! `ob_size` at offset 16, and type objects are a real `PyTypeObject` mirror.
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
//! `PyType_FromSpec` supports one base at a time and only bases that are
//! themselves heap types created here (or the builtin `object` type). Multiple
//! inheritance, members/getset descriptors, number/sequence/mapping protocols
//! and iteration slots are rejected with a tagged error instead of being
//! silently ignored. Every slot that is accepted has a real code path:
//! construction (`tp_new`/`tp_init`), deallocation (`tp_dealloc`/`tp_free`),
//! calls (`tp_call`), attribute lookup (`tp_getattro`/`tp_getattr`),
//! `tp_repr`/`tp_str`, `tp_hash`, buffer export and GC tracking.
//!
//! Type objects created by `PyType_FromSpec` are reference-counted like
//! CPython's heap types; static types (and the `PyExc_*` classes) are immortal.

#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(private_interfaces)]

use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uint, c_ulong, c_void};
use std::ptr;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, Once, PoisonError};

use super::buffer::PyBufferProcs;
use super::{
    dec_ref, inc_ref, read_c_string, read_method_table, set_error, value_object, GilGuard,
    MethodSpec, PyMethodDefMirror, ShimObject, ShimValue, IMMORTAL_REFCNT, MIN_VAR_BASICSIZE,
    PY_OBJECT_HEAD_SIZE,
};

/// `Py_TPFLAGS_*` bits from CPython 3.14's `object.h`.
pub(crate) const PY_TPFLAGS_HAVE_FINALIZE: c_ulong = 1 << 0;
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
const MAX_TYPE_SLOTS: usize = 64;

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
pub(crate) static mut TYPE_TYPE: TypeObject = TypeObject::builtin(
    c"type".as_ptr(),
    TypeRole::Metatype,
    PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_BASETYPE,
);
pub(crate) static mut OBJECT_TYPE: TypeObject = TypeObject::builtin(
    c"object".as_ptr(),
    TypeRole::Instance,
    PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_BASETYPE,
);
pub(crate) static mut NONE_TYPE: TypeObject = TypeObject::builtin(
    c"NoneType".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
pub(crate) static mut BOOL_TYPE: TypeObject =
    TypeObject::builtin(c"bool".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
pub(crate) static mut INT_TYPE: TypeObject =
    TypeObject::builtin(c"int".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
pub(crate) static mut FLOAT_TYPE: TypeObject =
    TypeObject::builtin(c"float".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
pub(crate) static mut STR_TYPE: TypeObject =
    TypeObject::builtin(c"str".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
pub(crate) static mut BYTES_TYPE: TypeObject =
    TypeObject::builtin(c"bytes".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
pub(crate) static mut LIST_TYPE: TypeObject =
    TypeObject::builtin(c"list".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
pub(crate) static mut TUPLE_TYPE: TypeObject =
    TypeObject::builtin(c"tuple".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
pub(crate) static mut DICT_TYPE: TypeObject =
    TypeObject::builtin(c"dict".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
pub(crate) static mut CAPSULE_TYPE: TypeObject = TypeObject::builtin(
    c"PyCapsule".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
pub(crate) static mut MODULE_TYPE: TypeObject =
    TypeObject::builtin(c"module".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
pub(crate) static mut CFUNCTION_TYPE: TypeObject = TypeObject::builtin(
    c"builtin_function_or_method".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
pub(crate) static mut ERROR_TYPE: TypeObject =
    TypeObject::builtin(c"error".as_ptr(), TypeRole::Shim, PY_TPFLAGS_IMMUTABLETYPE);
pub(crate) static mut NOT_IMPLEMENTED_TYPE: TypeObject = TypeObject::builtin(
    c"NotImplementedType".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);
pub(crate) static mut ELLIPSIS_TYPE: TypeObject = TypeObject::builtin(
    c"ellipsis".as_ptr(),
    TypeRole::Shim,
    PY_TPFLAGS_IMMUTABLETYPE,
);

/// Every static builtin type, used to patch the metatype link.
fn builtin_type_pointers() -> [*mut TypeObject; 17] {
    [
        ptr::addr_of_mut!(TYPE_TYPE),
        ptr::addr_of_mut!(OBJECT_TYPE),
        ptr::addr_of_mut!(NONE_TYPE),
        ptr::addr_of_mut!(BOOL_TYPE),
        ptr::addr_of_mut!(INT_TYPE),
        ptr::addr_of_mut!(FLOAT_TYPE),
        ptr::addr_of_mut!(STR_TYPE),
        ptr::addr_of_mut!(BYTES_TYPE),
        ptr::addr_of_mut!(LIST_TYPE),
        ptr::addr_of_mut!(TUPLE_TYPE),
        ptr::addr_of_mut!(DICT_TYPE),
        ptr::addr_of_mut!(CAPSULE_TYPE),
        ptr::addr_of_mut!(MODULE_TYPE),
        ptr::addr_of_mut!(CFUNCTION_TYPE),
        ptr::addr_of_mut!(ERROR_TYPE),
        ptr::addr_of_mut!(NOT_IMPLEMENTED_TYPE),
        ptr::addr_of_mut!(ELLIPSIS_TYPE),
    ]
}

static TYPES_READY: Once = Once::new();

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
        for pointer in builtin_type_pointers() {
            if pointer != metatype && pointer != object {
                (*pointer).tp_base = object;
            }
            (*pointer).tp_flags |= PY_TPFLAGS_READY;
        }
        (*object).tp_alloc = PyType_GenericAlloc as *const () as usize;
        (*object).tp_free = instance_free as *const () as usize;
        let bytes_procs: &'static PyBufferProcs = &super::buffer::BYTES_BUFFER_PROCS;
        (*ptr::addr_of_mut!(BYTES_TYPE)).tp_as_buffer =
            bytes_procs as *const PyBufferProcs as *mut PyBufferProcs;
        super::patch_constant_types();
    });
}

/// True when `object` is a type object (its `ob_type` is the shared metatype).
pub(crate) fn is_type_pointer(object: *mut ShimObject) -> bool {
    if object.is_null() {
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
#[derive(Default)]
struct SlotSet {
    getbuffer: usize,
    releasebuffer: usize,
    alloc: usize,
    base: usize,
    bases: usize,
    call: usize,
    dealloc: usize,
    doc: usize,
    getattr: usize,
    getattro: usize,
    hash: usize,
    init: usize,
    is_gc: usize,
    methods: usize,
    new: usize,
    repr: usize,
    setattr: usize,
    setattro: usize,
    str_: usize,
    traverse: usize,
    free: usize,
    finalize: usize,
    vectorcall: usize,
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
        match identifier {
            1 => set.getbuffer = value,
            2 => set.releasebuffer = value,
            47 => set.alloc = value,
            48 => set.base = value,
            49 => set.bases = value,
            50 => set.call = value,
            52 => set.dealloc = value,
            56 => {
                if set.doc != 0 {
                    return Err(format!(
                        "SystemError: PyType_FromSpec: type '{name}' has multiple Py_tp_doc slots"
                    ));
                }
                set.doc = value;
            }
            57 => set.getattr = value,
            58 => set.getattro = value,
            59 => set.hash = value,
            60 => set.init = value,
            61 => set.is_gc = value,
            64 => set.methods = value,
            65 => set.new = value,
            66 => set.repr = value,
            68 => set.setattr = value,
            69 => set.setattro = value,
            70 => set.str_ = value,
            71 => set.traverse = value,
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
    if bases.len() > 1 {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' requests multiple bases, which are not \
             supported"
        ));
    }
    let base = bases[0];
    if base.is_null() {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' has a NULL base"
        ));
    }
    // SAFETY: `base` was validated as a type pointer above.
    let base_role = unsafe { (*base).role };
    let base_flags = unsafe { (*base).tp_flags };
    if base_role != TypeRole::Instance {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' cannot use '{}' as a base (only instance \
             types can be bases)",
            type_name(base)
        ));
    }
    if base_flags & PY_TPFLAGS_BASETYPE == 0 {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{}' is not an acceptable base type",
            type_name(base)
        ));
    }
    let base_size = unsafe { (*base).tp_basicsize };
    let base_itemsize = unsafe { (*base).tp_itemsize };
    if base_itemsize != 0 {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' cannot derive from the variable-size base \
             '{}'",
            type_name(base)
        ));
    }

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
        if base_itemsize != 0 {
            return Err(format!(
                "TypeError: PyType_FromSpec: type '{name}' cannot extend the variable-size base \
                 '{}'",
                type_name(base)
            ));
        }
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
    type_object.tp_methods = slots.methods as *mut PyMethodDefMirror;
    type_object.tp_alloc = slots.alloc;
    type_object.tp_call = slots.call;
    type_object.tp_dealloc = slots.dealloc;
    type_object.tp_hash = slots.hash;
    type_object.tp_init = slots.init;
    type_object.tp_is_gc = slots.is_gc;
    type_object.tp_new = slots.new;
    type_object.tp_repr = slots.repr;
    type_object.tp_str = slots.str_;
    type_object.tp_traverse = slots.traverse;
    type_object.tp_free = slots.free;
    type_object.tp_finalize = slots.finalize;
    type_object.tp_vectorcall = slots.vectorcall;
    type_object.tp_getattro = slots.getattro;
    type_object.tp_setattro = slots.setattro;
    type_object.tp_getattr = slots.getattr;
    type_object.tp_setattr = slots.setattr;

    if slots.getbuffer != 0 || slots.releasebuffer != 0 {
        let procs = Box::new(PyBufferProcs {
            bf_getbuffer: slots.getbuffer,
            bf_releasebuffer: slots.releasebuffer,
        });
        type_object.tp_as_buffer = Box::into_raw(procs);
        type_object.owns_buffer = true;
    }

    // Single inheritance: link the base and copy the operations the subclass
    // does not define, matching CPython's `inherit_slots` for the supported
    // subset.
    type_object.tp_base = base;
    inc_ref(base as *mut ShimObject);
    unsafe {
        if type_object.tp_dealloc == 0 {
            type_object.tp_dealloc = (*base).tp_dealloc;
        }
        if type_object.tp_free == 0 {
            type_object.tp_free = (*base).tp_free;
        }
        if type_object.tp_init == 0 {
            type_object.tp_init = (*base).tp_init;
        }
        if type_object.tp_new == 0 {
            type_object.tp_new = (*base).tp_new;
        }
        if type_object.tp_call == 0 {
            type_object.tp_call = (*base).tp_call;
        }
        if type_object.tp_getattro == 0 {
            type_object.tp_getattro = (*base).tp_getattro;
        }
        if type_object.tp_setattro == 0 {
            type_object.tp_setattro = (*base).tp_setattro;
        }
        if type_object.tp_getattr == 0 {
            type_object.tp_getattr = (*base).tp_getattr;
        }
        if type_object.tp_setattr == 0 {
            type_object.tp_setattr = (*base).tp_setattr;
        }
        if type_object.tp_repr == 0 {
            type_object.tp_repr = (*base).tp_repr;
        }
        if type_object.tp_str == 0 {
            type_object.tp_str = (*base).tp_str;
        }
        if type_object.tp_hash == 0 {
            type_object.tp_hash = (*base).tp_hash;
        }
        if type_object.tp_traverse == 0 {
            type_object.tp_traverse = (*base).tp_traverse;
        }
        if type_object.tp_is_gc == 0 {
            type_object.tp_is_gc = (*base).tp_is_gc;
        }
        if type_object.tp_finalize == 0 {
            type_object.tp_finalize = (*base).tp_finalize;
        }
        if type_object.tp_vectorcall == 0 {
            type_object.tp_vectorcall = (*base).tp_vectorcall;
        }
        if type_object.tp_as_buffer.is_null() {
            type_object.tp_as_buffer = (*base).tp_as_buffer;
        }
        if type_object.tp_alloc == 0 {
            type_object.tp_alloc = (*base).tp_alloc;
        }
        if type_object.tp_flags & PY_TPFLAGS_HAVE_VECTORCALL == 0
            && (*base).tp_flags & PY_TPFLAGS_HAVE_VECTORCALL != 0
            && type_object.tp_vectorcall_offset == 0
        {
            type_object.tp_vectorcall_offset = (*base).tp_vectorcall_offset;
        }
    }

    // `tp_bases` is a strong reference like CPython's heap-type tuple.
    let bases_tuple = super::PyTuple_New(1);
    if bases_tuple.is_null() {
        free_type_contents(&mut type_object);
        return Err(super::take_error()
            .unwrap_or_else(|| "MemoryError: cannot create the bases tuple".to_string()));
    }
    inc_ref(base as *mut ShimObject);
    if super::PyTuple_SetItem(bases_tuple, 0, base as *mut ShimObject) != 0 {
        dec_ref(base as *mut ShimObject);
        dec_ref(bases_tuple);
        free_type_contents(&mut type_object);
        return Err(super::take_error()
            .unwrap_or_else(|| "TypeError: cannot fill the bases tuple".to_string()));
    }
    type_object.tp_bases = bases_tuple as *mut c_void;

    let pointer = Box::into_raw(Box::new(type_object));
    if let Err(error) = ensure_ready(pointer) {
        // `ensure_ready` only fails for impossible values here; release the
        // type and report the error rather than returning a half-built type.
        free_type_object(pointer);
        return Err(error);
    }
    Ok(pointer)
}

/// Releases the owned buffers of a `TypeObject` that never reached the heap.
fn free_type_contents(type_object: &mut TypeObject) {
    unsafe {
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
        let owns_name = (*type_object).owns_name;
        let owns_buffer = (*type_object).owns_buffer;
        let name = (*type_object).tp_name;
        let doc = (*type_object).tp_doc;
        let buffer_procs = (*type_object).tp_as_buffer;
        if owns_name && !name.is_null() {
            drop(CString::from_raw(name as *mut c_char));
        }
        if owns_name && !doc.is_null() {
            drop(CString::from_raw(doc as *mut c_char));
        }
        if owns_buffer && !buffer_procs.is_null() {
            drop(Box::from_raw(buffer_procs));
        }
        let _ = Box::from_raw(type_object);
        // Release the type's strong references after its own memory is gone;
        // `dec_ref` may recursively free types and must not see a dropped box.
        if !bases.is_null() {
            dec_ref(bases as *mut ShimObject);
        }
        if !base.is_null() {
            dec_ref(base as *mut ShimObject);
        }
    }
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
            if (*type_object).tp_vectorcall_offset <= 0 {
                // `Py_TPFLAGS_HAVE_VECTORCALL` only means something when the
                // instance carries a vectorcall function pointer.
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
        if flags & PY_TPFLAGS_HAVE_GC != 0 {
            gc_untrack(object);
        }
        if (*type_object).tp_finalize != 0 {
            let finalize: unsafe extern "C" fn(*mut ShimObject) =
                std::mem::transmute((*type_object).tp_finalize as *const c_void);
            finalize(object);
        }
        if (*type_object).tp_dealloc != 0 {
            // A custom deallocator owns the memory (CPython contract). Do not
            // free it again or touch the object afterwards.
            let dealloc: unsafe extern "C" fn(*mut ShimObject) =
                std::mem::transmute((*type_object).tp_dealloc as *const c_void);
            dealloc(object);
            return;
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

/// `PyType_Ready`: finalises a type created by `PyType_FromSpec`.
#[no_mangle]
pub extern "C" fn PyType_Ready(type_object: *mut ShimObject) -> c_int {
    match as_type(type_object) {
        Ok(type_object) => match ensure_ready(type_object) {
            Ok(()) => 0,
            Err(error) => {
                set_error(error);
                -1
            }
        },
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
            // 83 is Py_tp_token, which lives in PyHeapTypeObject; this host
            // has no token storage, so it reads as unset.
            _ => 0,
        }
    };
    pointer as *mut c_void
}

/// `PyType_GetName`: returns a new reference to the type's short name.
#[no_mangle]
pub extern "C" fn PyType_GetName(object: *mut ShimObject) -> *mut ShimObject {
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
    Box::into_raw(Box::new(type_object))
}

// ---------------------------------------------------------------------------
// Method lookup
// ---------------------------------------------------------------------------

/// Finds a method in the type's `tp_methods` table, walking `tp_base`.
pub(crate) fn find_method(type_object: *mut TypeObject, name: &str) -> Option<MethodSpec> {
    let mut current = type_object;
    // Bounded to keep a corrupt `tp_base` cycle from looping forever.
    for _ in 0..64 {
        if current.is_null() {
            return None;
        }
        // SAFETY: live type objects.
        let methods = unsafe { (*current).tp_methods };
        if !methods.is_null() {
            let owner = format!("type '{}'", type_name(current));
            let table = read_method_table(methods, &owner).ok()?;
            if let Some(spec) = table.into_iter().find(|spec| spec.name == name) {
                return Some(spec);
            }
        }
        current = unsafe { (*current).tp_base };
    }
    None
}

/// True when `type_object` is `base` or derives from it.
pub(crate) fn is_subtype(type_object: *mut TypeObject, base: *mut TypeObject) -> bool {
    let mut current = type_object;
    for _ in 0..64 {
        if current.is_null() {
            return false;
        }
        if current == base {
            return true;
        }
        current = unsafe { (*current).tp_base };
    }
    false
}
