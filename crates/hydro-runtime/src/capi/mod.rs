//! CPython 3.14 host surface for `abi3` extension modules.
//!
//! Hydro never links `libpython`. When an extension is loaded with `dlopen`,
//! its undefined `Py*` symbols resolve against the definitions exported from
//! this module (and [`types`], [`buffer`] and `parse.c`); the `tea` executable
//! is linked with `--export-dynamic` so those definitions also land in the
//! dynamic symbol table. Only the surface implemented here exists: an
//! extension that calls anything else fails to load instead of being silently
//! mis-emulated. Tests assert that a libpython-only entry point is absent from
//! the process.
//!
//! [`anchor`] is called once from `hydro_runtime_init`: it references every
//! exported entry point so the staticlib linker keeps the object files.
//!
//! # Object model
//!
//! Every handle handed to an extension starts with CPython's `PyVarObject`:
//! `ob_refcnt` at offset 0, `ob_type` at offset 8 and `ob_size` at offset 16.
//! The word at offset 24 and the bytes after it follow CPython 3.14's
//! per-type layouts for the macros that are supported inline (see
//! [`tuple_layout`]): tuples store their item pointers inline at offset 32,
//! lists store their `ob_item` pointer at offset 24, and `ob_size` carries the
//! length of tuples, lists, bytes and bytearrays. Extension instances
//! allocated through `PyType_GenericAlloc`/`PyObject_New` are raw
//! `tp_basicsize` allocations with that same header. [`types::TypeObject`] is
//! a byte-compatible CPython 3.14 `PyTypeObject`; its CPython prefix is
//! asserted at compile time.
//!
//! Reference counts are real. `Py_IncRef`/`Py_DecRef` (and the `_Py_*` spellings
//! used by 3.12+ limited-API headers), `Py_REFCNT`, container mutation and
//! instance allocation all maintain them. When a handle's count reaches zero
//! the handle is freed: `ShimObject` payloads release their owned children,
//! capsules run their destructor, and extension instances run
//! `tp_finalize`/`tp_dealloc` (or `tp_free`) before the storage is released.
//! Immutable singletons (`None`, `True`, `False`, `Ellipsis`,
//! `NotImplemented`, the interned small integers and the empty
//! `str`/`bytes`/`tuple`), static type objects and the exported `PyExc_*`
//! classes are immortal, exactly like CPython's static objects.
//!
//! Hydro values and handles do not share a heap: conversion copies scalars and
//! containers in both directions. A handle therefore never points into the
//! Hydro heap, the Hydro collector has no handle edges to trace, and freeing a
//! handle can never free Hydro memory. (Handle-internal cycles, such as a list
//! containing itself, are not collected; they leak instead of dangling.)
//!
//! # Types and calls
//!
//! [`types::PyType_FromSpec`] builds a real heap type with multiple bases
//! (C3 linearisation), `tp_name`, `tp_basicsize`, `tp_itemsize`, flags,
//! methods, doc, buffer slots, members/getset descriptors, iteration, rich
//! comparison, the number/sequence/mapping suites, `tp_clear`, `tp_del` and
//! the construction/deallocation slots. `PyType_Ready`, `PyType_GetSlot`,
//! `PyType_GetName`, `PyType_GetQualName`, `PyType_GetFlags`,
//! `PyType_GenericAlloc`, `PyType_GenericNew`, `_PyObject_New`,
//! `_PyObject_NewVar`, `PyObject_Init` and `PyObject_InitVar` are implemented.
//! Every accepted slot has a real code path; only the async suites and the
//! heap-type token are rejected, with a tagged error naming the slot.
//! `tp_mro` stores the merged bases (the type itself is re-prepended by
//! `mro_of`), which avoids a reference-count cycle.
//!
//! Calls go through PEP 590 vectorcall: `PyObject_Vectorcall`,
//! `PyObject_VectorcallDict`, `PyObject_VectorcallMethod`, `PyVectorcall_Call`,
//! `PyVectorcall_NARGS`, `PyObject_Call`, `PyObject_CallObject` and
//! `PyObject_CallFunctionObjArgs` (implemented in `parse.c`). `PyCFunction`
//! objects created by `PyCFunction_NewEx` are callable and dispatch every
//! `METH_*` convention, including `METH_FASTCALL` with and without keywords.
//!
//! Extension instances and type objects cross to Hydro as opaque handles
//! ([`ShimValue::Object`]) that own one reference to the original `PyObject *`.
//! Attribute lookup, method calls, construction, truth, `repr` and
//! `tp_richcompare` all run in the extension; passing a handle back yields the
//! same pointer with an additional reference, so identity is preserved.
//!
//! Hydro values travel the other way as [`ShimKind::Hydro`] handles that own a
//! persistent GC root. Classes, instances, functions, modules and the
//! containers/scalars nested in them keep their identity across the boundary:
//! `PyObject_GetAttr*` and `PyObject_SetAttr*` run Hydro's descriptor-aware
//! lookup, `PyObject_Call*` invokes Hydro code (classes construct, functions
//! and bound methods call), and `PyObject_Repr`/`Str`, truth, `len`, `hash`,
//! `PyObject_IsInstance`/`IsSubclass`, `PyType_GetName`/`GetFlags`/`IsSubtype`
//! and rich comparison dispatch to the runtime. The wrapper type's
//! `tp_basicsize` describes this host's handle, never Hydro heap memory, and
//! releasing the last C reference releases the root without freeing the value.
//! A Hydro value with no handle representation raises instead of producing a
//! fake object.
//!
//! # Buffers
//!
//! [`buffer`] implements the full request/validation surface: read-only and
//! writable, contiguous and strided, one-dimensional and multi-dimensional
//! views. `bytes` exports a read-only `B` view; extension types export through
//! `Py_bf_getbuffer`/`Py_bf_releasebuffer`. Every returned field is validated
//! before a view escapes (see [`buffer`] for the exact rules), and a malformed
//! view invokes the exporter's release hook and is replaced with a zeroed
//! structure plus a `BufferError`.
//!
//! # GIL emulation and scope
//!
//! A single process-wide, reentrant lock stands in for the GIL. It is acquired
//! around every foreign call the host makes (module initialisation and
//! `Py_mod_exec` callbacks, method/type calls, deallocators, finalizers,
//! buffer exporters, `tp_getattro`/`tp_repr`/`tp_hash` and friends) and
//! exposed through `PyGILState_Ensure`/`PyGILState_Release`,
//! `PyGILState_GetThisThreadState`, `PyEval_SaveThread`/`PyEval_RestoreThread`
//! and `PyThreadState_Get`. Reentrancy makes nested `Ensure`/`Release` and
//! nested calls safe on one thread; `PyEval_SaveThread` releases the lock
//! completely so an extension can block without deadlocking another thread,
//! and `PyEval_RestoreThread` reacquires it.
//!
//! The lock is not a full CPython GIL: there is no per-thread interpreter
//! state, no frame evaluation and no free-threading. It serialises this host's
//! foreign calls only. Extensions that rely on CPython's GIL for object
//! operations still get mutual exclusion for the calls that cross this
//! boundary; direct `Py_DecRef`/`Py_IncRef` calls from multiple threads are
//! atomic, but concurrent mutation of one container is not supported (as in
//! CPython).
//!
//! # GC integration
//!
//! `PyObject_GC_Track`/`PyObject_GC_UnTrack`/`PyObject_GC_IsTracked`,
//! `PyObject_GC_Del`, `_PyObject_GC_New`/`_PyObject_GC_NewVar`,
//! `PyObject_IS_GC` and the `Py_TPFLAGS_HAVE_GC`/`tp_is_gc` protocol are
//! implemented. `PyType_GenericAlloc` tracks instances of GC types
//! automatically; deallocation untracks. The tracked set records the handles
//! this host owns. `tp_traverse` is preserved in the type layout, returned by
//! `PyType_GetSlot`, and executable: `capi::types::gc_traverse` walks one
//! instance's edges and `capi::types::gc_walk_tracked` enumerates every tracked
//! instance's references. A GC type must install `tp_traverse`; `PyType_Ready`
//! rejects the `Py_TPFLAGS_HAVE_GC`/no-traverse combination with CPython's
//! `SystemError`.
//!
//! Both slots run in production: `PyGC_Collect` is the exported entry point
//! that drives a real cycle-collection pass over the tracked instances. It
//! counts the references each instance receives from other tracked instances
//! through `tp_traverse`, treats an instance with no reference from outside
//! that set as unreachable, and breaks the cycle with one `tp_clear` call
//! before the instance is freed. The default deallocation path (a GC type with
//! no `tp_dealloc`) runs `tp_clear` exactly once before releasing the instance,
//! and skips it when the collector already ran it, so declared edges are
//! dropped once and never twice. A type with its own `tp_dealloc` owns that
//! duty, as in CPython.
//!
//! Because Hydro values cross as rooted handles rather than raw heap
//! pointers, a handle held by an extension already carries its own persistent
//! root; the collector's pass therefore never frees Hydro memory, only the
//! C-side handles whose cycles it breaks. Handle-internal cycles that do not
//! involve a GC-tracked instance (a list containing itself) still leak rather
//! than dangle.
//!
//! Static C `PyTypeObject` declarations (`PyVarObject_HEAD_INIT(NULL, 0)` or
//! `&PyType_Type`) are adopted by `PyType_Ready`: the object is linked to the
//! metatype, its internal role is set to `Instance`, it is made immortal, and
//! `tp_base`/`tp_bases`/`tp_mro`/`tp_dict` plus the unspecialised slots are
//! filled from the base chain. Invalid layouts are rejected with a diagnostic
//! and never written through.
//!
//! # Remaining limits
//!
//! Full NumPy/pydantic compatibility is **not** claimed. Instance attribute
//! dictionaries/weak references, the async protocol suites and Python source
//! evaluation are absent; the evaluation entry points fail with a tagged
//! `RuntimeError` instead of resolving to nothing. `PyImport_ImportModule` and
//! friends resolve extension modules loaded into the extension registry and
//! Hydro native/user modules from the runtime registry; a name that matches a
//! compiled module body registered by `hydro_main` runs that body on demand
//! (see [`crate::module_init`]), so an extension can import a compiled module
//! whose initializer has not reached the startup loop yet. Every other name
//! fails with a tagged `ImportError`. Attribute access on a runtime-backed
//! module resolves native functions and values; classes and other values with
//! no C representation raise `AttributeError` instead of being fabricated.
//!
//! Integers are 64-bit in the C API: `PyLong_FromUnsignedLongLong`,
//! `PyLong_FromNativeBytes` and `PyLong_AsNativeBytes` raise `OverflowError`
//! when a value does not fit, instead of silently truncating. `PyUnicode_New`
//! strings expose writable UCS storage through `PyUnicode_DATA` (1/2/4-byte
//! code units, exactly as `PyUnicode_New` selects); unpaired surrogates are
//! replaced when the string is read back as text. Traceback frames are never
//! created, so `PyTraceBack_Print` only accepts `NULL`/`None`.
//!
//! # Exported data
//!
//! Every CPython 3.14 builtin type is a real exported `PyTypeObject` data
//! symbol (`PyLong_Type`, `PyType_Type`, `PyBaseObject_Type`, descriptor and
//! function types, ...), and `ob_type` of each handle points at the matching
//! address. `Py_Version`, `_Py_ascii_whitespace`, `_PyByteArray_empty_string`,
//! `_Py_NoneStruct`, `_Py_TrueStruct`, `_Py_FalseStruct` and the
//! `Py_None`/`Py_True`/`Py_False` pointer spellings are exported as well.

#![allow(non_snake_case)]
// CPython's exported data symbols keep their historical C spelling
// (`_Py_NoneStruct`, `PyExc_ValueError`, ...); renaming them is not an option.
#![allow(non_upper_case_globals)]
// These are C ABI entry points: every argument is a raw `PyObject *` supplied
// by an extension module, so the usual "take a reference" API shape does not
// apply.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// The exported constant objects are typed as the `pub(crate)` shim object;
// extensions only ever see them through an opaque `PyObject *`.
#![allow(private_interfaces)]

pub(crate) mod buffer;
pub(crate) mod container_api;
pub(crate) mod descriptor_api;
pub(crate) mod numeric_api;
pub(crate) mod object_api;
pub(crate) mod tuple_layout;
#[path = "type.rs"]
pub(crate) mod types;
pub(crate) mod unicode_api;

#[allow(unused_imports)]
pub(crate) use buffer::{
    PyBuffer_FillInfo, PyBuffer_Release, PyObject_CheckBuffer, PyObject_GetBuffer, Py_buffer,
    PYBUF_C_CONTIGUOUS, PYBUF_FORMAT, PYBUF_F_CONTIGUOUS, PYBUF_ND, PYBUF_SIMPLE, PYBUF_STRIDES,
    PYBUF_WRITABLE,
};
#[allow(unused_imports)]
pub(crate) use types::{
    PyTypeSlotMirror, PyTypeSpecMirror, TypeObject, TypeRole, PY_TPFLAGS_BASETYPE,
    PY_TPFLAGS_BASE_EXC_SUBCLASS, PY_TPFLAGS_DISALLOW_INSTANTIATION, PY_TPFLAGS_HAVE_GC,
    PY_TPFLAGS_HAVE_VECTORCALL, PY_TPFLAGS_HEAPTYPE, PY_TPFLAGS_IMMUTABLETYPE,
};

// The type/object allocation API lives in `type.rs`; re-export it so the link
// anchor and the module tests can name it directly, and so every `Py*` entry
// point is reachable from one place.
#[allow(unused_imports)]
pub(crate) use types::{
    _PyObject_GC_New, _PyObject_GC_NewVar, _PyObject_New, _PyObject_NewVar, PyObject_GC_Del,
    PyObject_GC_IsTracked, PyObject_GC_Track, PyObject_GC_UnTrack, PyObject_IS_GC, PyObject_Init,
    PyObject_InitVar, PyObject_New, PyObject_NewVar, PyObject_SelfIter, PyType_FromSpec,
    PyType_FromSpecWithBases, PyType_GenericAlloc, PyType_GenericNew, PyType_GetFlags,
    PyType_GetName, PyType_GetQualName, PyType_GetSlot, PyType_Ready,
};

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::ptr;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Once, PoisonError};

use tuple_layout::TupleItems;
use types::{ensure_builtin_types, type_of, TypeObject as PyTypeObject};

// ---------------------------------------------------------------------------
// CPython mirror constants
// ---------------------------------------------------------------------------

/// `METH_*` bits from CPython's `methodobject.h` (limited-API subset).
const METH_VARARGS: c_int = 0x0001;
const METH_KEYWORDS: c_int = 0x0002;
const METH_NOARGS: c_int = 0x0004;
const METH_O: c_int = 0x0008;
/// `METH_CLASS`: a type method that receives the class as its receiver.
const METH_CLASS: c_int = 0x0010;
/// `METH_STATIC`: a type method that receives NULL as its receiver.
const METH_STATIC: c_int = 0x0020;
/// `METH_COEXIST`: a type method replaces an inherited attribute of the same
/// name instead of being skipped. It has no call-time meaning.
const METH_COEXIST: c_int = 0x0040;
/// `METH_FASTCALL` (stable ABI since 3.10) selects the
/// `_PyCFunctionFast`/`_PyCFunctionFastWithKeywords` calling convention.
const METH_FASTCALL: c_int = 0x0080;
/// `METH_METHOD`: the function receives its defining class
/// (`PyCMethod`). Creation is supported; invocation is reported as unsupported
/// because this host has no `PyCMethod` call convention.
const METH_METHOD: c_int = 0x0200;
/// Bits that select the calling convention. CPython's `PyCMethod_New` masks
/// the same bits when choosing a vectorcall entry point, so the binding-only
/// `METH_CLASS`/`METH_STATIC`/`METH_COEXIST` flags do not affect a call.
const METHOD_CALL_FLAGS: c_int =
    METH_VARARGS | METH_KEYWORDS | METH_NOARGS | METH_O | METH_FASTCALL;
/// Flags that only affect how a type method is bound as an attribute.
const METHOD_BINDING_FLAGS: c_int = METH_CLASS | METH_STATIC | METH_COEXIST;

/// `Py_GetConstant` identifiers from CPython 3.13's `object.h`.
const PY_CONSTANT_NONE: c_uint = 0;
const PY_CONSTANT_FALSE: c_uint = 1;
const PY_CONSTANT_TRUE: c_uint = 2;
const PY_CONSTANT_ELLIPSIS: c_uint = 3;
const PY_CONSTANT_NOT_IMPLEMENTED: c_uint = 4;
const PY_CONSTANT_ZERO: c_uint = 5;
const PY_CONSTANT_ONE: c_uint = 6;
const PY_CONSTANT_EMPTY_STR: c_uint = 7;
const PY_CONSTANT_EMPTY_BYTES: c_uint = 8;
const PY_CONSTANT_EMPTY_TUPLE: c_uint = 9;

/// Upper bound on the entries read from a `PyMethodDef` table. CPython has no
/// declared limit; bounding it keeps a corrupt table from scanning forever.
const MAX_METHODS: usize = 4096;

/// Upper bound on the entries read from a `PyModuleDef_Slot` table.
const MAX_MODULE_SLOTS: usize = 64;

/// `Py_mod_*` slot identifiers from CPython's `moduleobject.h` (stable ABI).
const PY_MOD_CREATE: c_int = 1;
const PY_MOD_EXEC: c_int = 2;
const PY_MOD_MULTIPLE_INTERPRETERS: c_int = 3;
const PY_MOD_GIL: c_int = 4;

const PY_MOD_MULTIPLE_INTERPRETERS_PER_INTERPRETER_GIL_SUPPORTED: c_int = 2;
const PY_MOD_GIL_NOT_USED: c_int = 1;

/// `PY_VECTORCALL_ARGUMENTS_OFFSET`: the top bit of `nargsf`.
const PY_VECTORCALL_ARGUMENTS_OFFSET: usize = 1usize << (usize::BITS - 1);

/// `PyObject_HEAD` size: `ob_refcnt` + `ob_type`.
pub(crate) const PY_OBJECT_HEAD_SIZE: usize = 16;

/// `sizeof(PyVarObject)`: the header plus `ob_size`.
pub(crate) const MIN_VAR_BASICSIZE: usize = tuple_layout::VAR_HEAD_SIZE;

/// Sentinel stored in `ob_refcnt` for objects that must never be freed.
///
/// CPython uses a similar "immortal" encoding; equality with this value makes
/// the check cheap and unambiguous. It is large enough that a `Py_REFCNT`
/// observer sees an obviously immortal count.
pub(crate) const IMMORTAL_REFCNT: isize = isize::MAX;

/// `PyObject_HEAD`: the CPython-compatible header at offset 0 of every handle.
#[repr(C)]
pub(crate) struct PyObjectHead {
    pub(crate) ob_refcnt: AtomicIsize,
    pub(crate) ob_type: *mut TypeObject,
}

/// `PyVarObject`: `PyObject_HEAD` plus `ob_size`.
#[repr(C)]
#[allow(dead_code)]
pub(crate) struct PyVarObjectHead {
    pub(crate) ob_base: PyObjectHead,
    pub(crate) ob_size: AtomicIsize,
}

// The offsets that every extension observes through `Py_TYPE`, `Py_REFCNT`,
// `Py_SIZE` and its own struct declarations. Only the x86_64 layout is
// claimed, matching the rest of this mirror.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::offset_of!(PyObjectHead, ob_refcnt) == 0);
    assert!(std::mem::offset_of!(PyObjectHead, ob_type) == 8);
    assert!(std::mem::size_of::<PyObjectHead>() == PY_OBJECT_HEAD_SIZE);
    assert!(std::mem::offset_of!(PyVarObjectHead, ob_base) == 0);
    assert!(std::mem::offset_of!(PyVarObjectHead, ob_size) == 16);
    assert!(std::mem::size_of::<PyVarObjectHead>() == MIN_VAR_BASICSIZE);
};

/// Mirrors `PyModuleDef` from CPython 3.14 on x86_64 (`PyModuleDef_Base` plus
/// the trailing name/doc/size/methods/slots/traverse/clear/free fields).
#[repr(C)]
#[allow(dead_code)]
pub(crate) struct PyModuleDefMirror {
    ob_refcnt: isize,
    ob_type: *mut c_void,
    m_init: *mut c_void,
    m_index: isize,
    m_copy: *mut c_void,
    m_name: *const c_char,
    m_doc: *const c_char,
    m_size: isize,
    m_methods: *mut PyMethodDefMirror,
    m_slots: *mut PyModuleDefSlotMirror,
    m_traverse: *mut c_void,
    m_clear: *mut c_void,
    m_free: *mut c_void,
}

/// Mirrors `PyModuleDef_Slot`: `{ slot, value }`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct PyModuleDefSlotMirror {
    slot: c_int,
    value: *mut c_void,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<PyModuleDefSlotMirror>() == 16);
    assert!(std::mem::offset_of!(PyModuleDefSlotMirror, slot) == 0);
    assert!(std::mem::offset_of!(PyModuleDefSlotMirror, value) == 8);
};

/// Mirrors `PyMethodDef`: `{ ml_name, ml_meth, ml_flags, ml_doc }`.
#[repr(C)]
#[allow(dead_code)]
pub(crate) struct PyMethodDefMirror {
    pub(crate) ml_name: *const c_char,
    pub(crate) ml_meth: *mut c_void,
    pub(crate) ml_flags: c_int,
    pub(crate) ml_doc: *const c_char,
}

/// One owned reference to an extension handle.
///
/// Used by [`ShimValue::Object`] so cloning a contract value keeps every handle
/// alive and dropping it releases exactly one reference. `into_raw` transfers
/// the owned reference to the caller (the pointer then behaves like a new
/// reference, matching every other [`value_object`] result).
#[derive(Debug)]
pub(crate) struct OwnedShimObject(*mut ShimObject);

// The shim heap is process-wide and handles are reference counted with
// atomics; registry access is additionally serialised by its mutex.
unsafe impl Send for OwnedShimObject {}

impl OwnedShimObject {
    #[cfg(test)]
    pub(crate) fn as_ptr(&self) -> *mut ShimObject {
        self.0
    }
}

impl OwnedShimObject {
    /// Wraps `object`, owning one new reference to it.
    pub(crate) fn new(object: *mut ShimObject) -> Self {
        inc_ref(object);
        OwnedShimObject(object)
    }

    /// Relinquishes the owned reference and returns the raw handle.
    pub(crate) fn into_raw(self) -> *mut ShimObject {
        let object = self.0;
        std::mem::forget(self);
        object
    }
}

impl Clone for OwnedShimObject {
    fn clone(&self) -> Self {
        inc_ref(self.0);
        OwnedShimObject(self.0)
    }
}

impl Drop for OwnedShimObject {
    fn drop(&mut self) {
        dec_ref(self.0);
    }
}

std::thread_local! {
    /// Identity of the GC registration that owns this thread's persistent
    /// root slots. Used to keep a root release on the registering thread; a
    /// slot index is only meaningful inside its own registration.
    static ROOT_OWNER_TOKEN: u8 = const { 0 };
}

/// Address that identifies the calling thread's root registration.
fn root_owner_token() -> usize {
    ROOT_OWNER_TOKEN.with(|token| token as *const u8 as usize)
}

/// One persistent GC root keeping a Hydro value alive for C references.
///
/// The root is registered when the first handle is created and removed when
/// the last clone (the [`ShimValue`] variant and the owning
/// [`ShimKind::Hydro`]) is dropped. Sharing is reference-counted with an
/// `Arc`, so a value that travels through several contract copies stays
/// rooted exactly once.
struct HydroRoot {
    value: i64,
    /// Persistent-root slot, or `usize::MAX` outside the runtime.
    slot: usize,
    /// Thread whose registration owns `slot`.
    owner: usize,
}

impl HydroRoot {
    fn new(value: i64) -> Arc<HydroRoot> {
        let owner = root_owner_token();
        let slot = crate::heap::add_persistent_root(value);
        Arc::new(HydroRoot { value, slot, owner })
    }
}

impl Drop for HydroRoot {
    fn drop(&mut self) {
        // A persistent root belongs to the registration that created it. A
        // handle released on another thread (an extension may decref
        // anywhere) leaves the root to that registration; it is cleared when
        // the thread exits or the run resets. Never touch another
        // registration's slot.
        if self.slot == usize::MAX || root_owner_token() != self.owner {
            return;
        }
        crate::heap::release_persistent_root(self.slot, self.value);
    }
}

impl std::fmt::Debug for HydroRoot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HydroRoot")
            .field("value", &self.value)
            .finish_non_exhaustive()
    }
}

/// A Hydro value crossing to an extension as a real `PyObject *`.
///
/// The handle owns a persistent GC root, so the collector treats the value as
/// reachable for as long as any C reference to the wrapper exists. Cloning
/// shares that root; the last drop releases it without touching the Hydro
/// object itself (the collector reclaims it when it really is unreachable).
#[derive(Clone, Debug)]
pub(crate) struct HydroValue {
    root: Arc<HydroRoot>,
    /// Python-visible type name, used by diagnostics and the wrapper's type.
    type_name: String,
}

impl HydroValue {
    /// Roots `value` and snapshots its Python-visible type name.
    pub(crate) fn new(value: i64) -> HydroValue {
        HydroValue {
            root: HydroRoot::new(value),
            type_name: crate::value::type_name_of(value),
        }
    }

    /// The tagged Hydro value this handle names.
    pub(crate) fn value(&self) -> i64 {
        self.root.value
    }

    /// Python-visible type name of the wrapped value.
    pub(crate) fn type_name(&self) -> &str {
        &self.type_name
    }
}

/// The crate-internal value contract shared with the extension loader.
///
/// Containers are owned copies: passing a Hydro list to an extension copies it,
/// and a container returned by an extension is converted into a fresh Hydro
/// container. [`ShimValue::Object`] is the exception: it carries an owned
/// reference to the extension's own handle, so the same pointer can be passed
/// back and the extension's identity, attributes and call semantics are
/// preserved. [`ShimValue::Hydro`] is the mirror image: a Hydro class,
/// instance, function, module or other value crosses as a rooted handle, and
/// passing it back yields the identical Hydro value.
#[derive(Clone, Debug)]
pub(crate) enum ShimValue {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<ShimValue>),
    Tuple(Vec<ShimValue>),
    Dict(Vec<(ShimValue, ShimValue)>),
    /// A set of contract values. Hydro has a single set kind, so a C
    /// `frozenset` converts to this arm as well.
    Set(Vec<ShimValue>),
    /// Opaque pointer capsule. The pointer belongs to the extension; its
    /// destructor now runs when the handle is freed.
    Capsule {
        ptr: usize,
        name: Option<String>,
    },
    /// An extension-owned object or type object, kept as an owned handle.
    ///
    /// `type_name` is the Python-visible type name used by diagnostics and
    /// `type()`. The handle is passed back to the extension unchanged (with an
    /// additional reference), so instances retain their identity.
    Object {
        object: OwnedShimObject,
        type_name: String,
    },
    /// A Hydro value (class, instance, function, module, container, scalar)
    /// kept alive by a persistent GC root while C references it.
    Hydro(HydroValue),
    NotImplemented,
    Ellipsis,
}

/// A snapshot of a module attribute that denotes one of the module's methods.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ShimCallable {
    pub module: u64,
    pub method: u32,
}

/// A module attribute as seen by `get_attr`.
#[derive(Clone, Debug)]
pub(crate) enum ShimAttr {
    Value(ShimValue),
    Callable(ShimCallable),
    /// A heap type created by `PyType_FromSpec`, stored as the address of its
    /// type object. Calling it goes through vectorcall.
    Type(usize),
}

/// Mutable character storage behind a string created by `PyUnicode_New`.
///
/// `PyUnicode_DATA` hands the extension a pointer into `data`; the string's
/// textual value is derived from it whenever the object is read. Code unit
/// width follows CPython's kind selection (1/2/4 bytes, little endian), so an
/// extension that writes UCS-1/2/4 data observes the documented layout.
#[derive(Clone, Debug)]
pub(crate) struct RawText {
    pub(crate) width: u8,
    pub(crate) data: Vec<u8>,
}

/// Maximum nesting depth accepted when converting containers in either
/// direction.
pub(crate) const MAX_CONVERSION_DEPTH: usize = 64;

/// Values stored by one `PyContextVar` per thread.
///
/// The map owns a reference to every stored value; dropping the last `Arc`
/// (when the variable object dies) releases them, mirroring CPython's
/// per-thread context storage.
#[derive(Debug)]
pub(crate) struct ContextVarValues {
    values: Mutex<HashMap<u64, usize>>,
}

impl ContextVarValues {
    fn new() -> Self {
        Self {
            values: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<u64, usize>> {
        self.values.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for ContextVarValues {
    fn drop(&mut self) {
        let values = std::mem::take(&mut *self.lock());
        for (_, value) in values {
            if value != 0 {
                dec_ref(value as *mut ShimObject);
            }
        }
    }
}

/// Storage behind a memoryview: the exported `Py_buffer` plus its release
/// hook. `PyBuffer_Release` runs exactly once, when the memoryview dies.
pub(crate) struct MemoryViewBuffer {
    view: Mutex<buffer::Py_buffer>,
    /// Offsets, relative to the start of the exporting `Py_buffer`, of the
    /// `shape`/`strides` pointers that referenced that temporary structure (as
    /// `PyBuffer_FillInfo` builds them). They are re-pointed at the stored
    /// copy, whose address is only known once the buffer reaches its final
    /// location.
    shape_offset: Option<usize>,
    strides_offset: Option<usize>,
}

impl MemoryViewBuffer {
    /// `source` is the address of the `Py_buffer` the exporter filled. A
    /// self-referential `shape`/`strides` (as `PyBuffer_FillInfo` builds them)
    /// points into that temporary structure, not into `view`.
    fn new(view: buffer::Py_buffer, source: *const buffer::Py_buffer) -> Self {
        let base = source as usize;
        let end = base + std::mem::size_of::<buffer::Py_buffer>();
        let self_offset = |pointer: *mut c_void| {
            let address = pointer as usize;
            (address >= base && address < end).then_some(address - base)
        };
        let shape_offset = self_offset(view.shape.cast::<c_void>());
        let strides_offset = self_offset(view.strides.cast::<c_void>());
        Self {
            view: Mutex::new(view),
            shape_offset,
            strides_offset,
        }
    }

    /// Re-points `shape`/`strides` at the stored view. Must run after the
    /// buffer is in its final (heap-stable) location.
    fn relocate_self_references(&self) {
        if self.shape_offset.is_none() && self.strides_offset.is_none() {
            return;
        }
        let mut view = self.lock();
        let base = (&mut *view as *mut buffer::Py_buffer) as usize;
        if let Some(offset) = self.shape_offset {
            view.shape = (base + offset) as *mut isize;
        }
        if let Some(offset) = self.strides_offset {
            view.strides = (base + offset) as *mut isize;
        }
    }

    fn lock(&self) -> MutexGuard<'_, buffer::Py_buffer> {
        self.view.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for MemoryViewBuffer {
    fn drop(&mut self) {
        let mut view = self.lock();
        if !view.buf.is_null() {
            let pointer: *mut buffer::Py_buffer = &mut *view;
            // The view was produced by `PyObject_GetBuffer` and has not been
            // released; releasing runs the exporter's release hook and drops
            // the reference to the exporter, exactly once.
            buffer::PyBuffer_Release(pointer);
        }
    }
}

/// Internal representation of one shim object.
#[derive(Clone)]
pub(crate) enum ShimKind {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// `c_text` is the NUL-terminated form used by `PyArg_ParseTuple("s")` and
    /// `PyUnicode_AsUTF8`; absent when the string contains an embedded NUL.
    /// `raw` is present for strings created by `PyUnicode_New`, whose storage
    /// an extension may fill through `PyUnicode_DATA`; `text` and `c_text` are
    /// refreshed from it on every read.
    Str {
        text: String,
        c_text: Option<CString>,
        raw: Option<RawText>,
    },
    /// `data` is the bytes content; `c_data` is `data` plus a trailing NUL.
    Bytes {
        data: Vec<u8>,
        c_data: Vec<u8>,
    },
    /// Mutable byte buffer, `c_data` NUL-terminated as CPython's bytearray.
    ByteArray {
        data: Vec<u8>,
        c_data: Vec<u8>,
    },
    /// Mutable set of object handles. Entries are unique by key equality.
    Set(Vec<usize>),
    /// Immutable set of object handles.
    FrozenSet(Vec<usize>),
    /// Complex number payload.
    Complex {
        real: f64,
        imag: f64,
    },
    /// Exception instance: its class handle, positional arguments and the
    /// `__cause__`/`__context__`/`__traceback__` slots (0 for "not set").
    Exception {
        exc_type: usize,
        args: Vec<usize>,
        cause: usize,
        context: usize,
        traceback: usize,
    },
    /// Native/user module function, dispatched through the runtime registry.
    RuntimeFunction {
        module: String,
        name: String,
    },
    List(Vec<usize>),
    /// A tuple whose item pointers are stored inline in the handle allocation,
    /// at the offset CPython's `PyTuple_GET_ITEM`/`PyTuple_SET_ITEM` use.
    /// `TupleItems` only names that array; there is no second copy.
    Tuple(TupleItems),
    Dict(Vec<(usize, usize)>),
    /// `destructor` is invoked when the capsule's reference count reaches zero.
    Capsule {
        ptr: usize,
        name: Option<CString>,
        context: usize,
        destructor: usize,
    },
    /// A bound method: `function(self_object, ...)`. Both are owned.
    Method {
        function: usize,
        self_object: usize,
    },
    /// `slice(start, stop, step)`; each component owns a reference and is
    /// `None` when the constructor argument was NULL.
    Slice {
        start: usize,
        stop: usize,
        step: usize,
    },
    /// A read-only mapping view over an owned dict, created by
    /// `PyDictProxy_New`.
    DictProxy {
        dict: usize,
    },
    /// `types.GenericAlias(origin, args)`.
    GenericAlias {
        origin: usize,
        args: usize,
    },
    /// An exporting `memoryview` over an acquired `Py_buffer`.
    MemoryView {
        buffer: Arc<MemoryViewBuffer>,
    },
    /// The concrete iterator returned by `PySeqIter_New`.
    SeqIter {
        sequence: usize,
        index: isize,
    },
    /// `contextvars.ContextVar` created through the C API.
    ///
    /// `default` is CPython's second `PyContextVar_New` argument: any object
    /// (a *new* reference owned by the variable, or 0 for none), used by
    /// `PyContextVar_Get` when the variable has no value in the current
    /// context. It is not a doc string.
    ContextVar {
        name: String,
        default: usize,
        values: Arc<ContextVarValues>,
    },
    /// Token returned by `PyContextVar_Set`; `previous` is 0 when the variable
    /// had no value in this thread.
    ContextVarToken {
        var: usize,
        previous: usize,
    },
    /// A `PyCFunction` bound to `self_object`, created by
    /// `PyCFunction_NewEx`. `method` is the address of the `PyMethodDef`.
    CFunction {
        method: usize,
        self_object: usize,
        module: usize,
    },
    NotImplemented,
    Ellipsis,
    Module(u64),
    /// A `PyModuleDef` wrapped by `PyModuleDef_Init`.
    ModuleDef {
        def: usize,
    },
    /// A Hydro value handed to the extension as a real object.
    ///
    /// The payload owns the persistent GC root, so attribute access, calls,
    /// `repr`/`str`, truth and comparison dispatch to Hydro while any C
    /// reference keeps the wrapper alive.
    Hydro(HydroValue),
    /// Non-NULL sentinel returned by `PyErr_Occurred`.
    Error,
}

/// Private object handed to extensions as an opaque `PyObject *`.
///
/// The CPython-compatible prefix comes first (`PyVarObject` plus the
/// `PyVarObject`-trailing word), then the tagged payload. Tuples are the one
/// exception to "payload at [`tuple_layout::PREFIX_SIZE`]": their item
/// pointers are stored inline at that offset and the [`ShimKind`] follows the
/// array, so `PyTuple_GET_ITEM`/`PyTuple_SET_ITEM` work. Every access to the
/// payload therefore goes through [`kind_pointer`].
#[repr(C)]
pub(crate) struct ShimObject {
    /// `ob_refcnt` at 0, `ob_type` at 8.
    pub(crate) head: PyObjectHead,
    /// `ob_size` at 16: the length CPython's `Py_SIZE` and the container
    /// `Py*_GET_SIZE` macros read.
    pub(crate) ob_size: AtomicIsize,
    /// The word at 24, interpreted per type: `ob_hash` for tuples and bytes
    /// (CPython starts at -1), `ob_item` for lists (the data pointer), unused
    /// (0) for every other kind.
    pub(crate) aux: AtomicUsize,
    /// The payload of a non-tuple handle, at offset 32.
    kind: ShimKind,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::offset_of!(ShimObject, head) == tuple_layout::REFCNT_OFFSET);
    assert!(std::mem::offset_of!(ShimObject, ob_size) == tuple_layout::SIZE_OFFSET);
    assert!(std::mem::offset_of!(ShimObject, aux) == tuple_layout::AUX_OFFSET);
    assert!(std::mem::offset_of!(ShimObject, kind) == tuple_layout::PREFIX_SIZE);
    assert!(tuple_layout::REFCNT_OFFSET == 0);
    assert!(tuple_layout::TYPE_OFFSET == tuple_layout::REFCNT_OFFSET + 8);
    assert!(tuple_layout::PREFIX_SIZE == 32);
    // The inline tuple payload starts on an 8-byte boundary and `ShimKind`
    // repeats every 8 bytes, so `tuple_kind_offset` needs no extra padding.
    assert!(std::mem::align_of::<ShimKind>() == 8);
};

// A handle's payload is immutable while it is shared: mutation happens only
// while this thread owns the last reference (freeing) or under the emulated
// GIL (container writes). Reference counts are atomic. The raw `ob_type`
// pointer names a process-lifetime or reference-counted type table, never
// thread-local Rust data. These impls also let handles live in `static mut`
// constant singletons without tripping the static thread-safety bound.
unsafe impl Send for ShimObject {}
unsafe impl Sync for ShimObject {}

/// Shape of the `PyInit_<name>` entry point in a limited-API extension.
type ExtensionInit = unsafe extern "C" fn() -> *mut ShimObject;

// ---------------------------------------------------------------------------
// GIL emulation
// ---------------------------------------------------------------------------

/// `PyGILState_LOCKED`: the thread already held the lock.
pub(crate) const PY_GILSTATE_LOCKED: c_int = 0;
/// `PyGILState_UNLOCKED`: the thread did not hold the lock.
pub(crate) const PY_GILSTATE_UNLOCKED: c_int = 1;

static GIL_LOCK: Mutex<()> = Mutex::new(());

std::thread_local! {
    static GIL_DEPTH: Cell<usize> = const { Cell::new(0) };
    static GIL_GUARD: RefCell<Option<MutexGuard<'static, ()>>> =
        const { RefCell::new(None) };
    /// Identity of the thread that owns this thread's depth/guard slot.
    ///
    /// The depth and guard are thread-local, so ownership already follows the
    /// thread; the token makes that explicit and lets `PyGILState_Check`
    /// distinguish "this thread holds the lock" from another thread's hold.
    static GIL_OWNER: Cell<usize> = const { Cell::new(0) };
    static THREAD_STATE_TOKEN: u8 = const { 0 };
}

/// Stable per-thread identity used as the GIL owner token.
fn gil_thread_token() -> usize {
    THREAD_STATE_TOKEN.with(|token| token as *const u8 as usize)
}

/// Acquires the emulated GIL, reentrantly on the current thread.
pub(crate) fn gil_acquire() {
    let depth = GIL_DEPTH.with(Cell::get);
    if depth == 0 {
        let guard = GIL_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        GIL_GUARD.with(|slot| *slot.borrow_mut() = Some(guard));
        GIL_OWNER.with(|owner| owner.set(gil_thread_token()));
    }
    GIL_DEPTH.with(|slot| slot.set(depth.saturating_add(1)));
}

/// Releases one level of the emulated GIL.
pub(crate) fn gil_release() {
    let depth = GIL_DEPTH.with(Cell::get);
    if depth == 0 {
        return;
    }
    GIL_DEPTH.with(|slot| slot.set(depth - 1));
    if depth == 1 {
        let guard = GIL_GUARD.with(|slot| slot.borrow_mut().take());
        drop(guard);
        GIL_OWNER.with(|owner| owner.set(0));
    }
}

/// True when the calling thread currently holds the emulated GIL.
pub(crate) fn gil_owned_by_current_thread() -> bool {
    GIL_DEPTH.with(Cell::get) > 0 && GIL_OWNER.with(Cell::get) == gil_thread_token()
}

/// RAII guard for the emulated GIL. Reentrant, so nesting is safe.
pub(crate) struct GilGuard;

impl GilGuard {
    pub(crate) fn new() -> Self {
        gil_acquire();
        GilGuard
    }
}

impl Drop for GilGuard {
    fn drop(&mut self) {
        gil_release();
    }
}

/// Opaque thread-state token. There is no interpreter state behind it; it
/// exists so `PyThreadState_Get` returns a stable non-NULL pointer per thread.
#[repr(C)]
#[allow(dead_code)]
pub struct PyThreadState {
    _private: [u8; 0],
}

fn thread_state_token() -> *mut PyThreadState {
    // `with` yields a reference into this thread's TLS block, whose address is
    // stable for the lifetime of the thread. Only the address is used.
    THREAD_STATE_TOKEN.with(|token| token as *const u8 as *mut PyThreadState)
}

/// `PyGILState_Ensure`: acquires the GIL and reports the previous state.
///
/// Every call increments the calling thread's nesting depth. The state it
/// returns describes what the thread held *before* the call:
/// `PyGILState_LOCKED` for a reentrant call, `PyGILState_UNLOCKED` for the
/// call that acquired the lock.
#[no_mangle]
pub extern "C" fn PyGILState_Ensure() -> c_int {
    let previous = if GIL_DEPTH.with(Cell::get) > 0 {
        PY_GILSTATE_LOCKED
    } else {
        PY_GILSTATE_UNLOCKED
    };
    gil_acquire();
    previous
}

/// `PyGILState_Release`: balances `PyGILState_Ensure`.
///
/// Like CPython, each `Ensure` must be matched by one `Release`, including the
/// reentrant ones that returned `PyGILState_LOCKED`; the physical lock is
/// dropped when the depth returns to zero. An out-of-range state is a
/// `SystemError` and leaves the depth untouched.
#[no_mangle]
pub extern "C" fn PyGILState_Release(state: c_int) {
    if state == PY_GILSTATE_UNLOCKED || state == PY_GILSTATE_LOCKED {
        gil_release();
    } else {
        set_error(format!(
            "SystemError: PyGILState_Release: invalid state {state}"
        ));
    }
}

/// `PyGILState_GetThisThreadState`: the current thread's token.
#[no_mangle]
pub extern "C" fn PyGILState_GetThisThreadState() -> *mut PyThreadState {
    thread_state_token()
}

/// `PyGILState_Check`: 1 when the calling thread holds the emulated GIL.
///
/// The state is thread-local: a thread that never called
/// `PyGILState_Ensure` reports 0 even while another thread holds the lock,
/// matching CPython's per-thread answer.
#[no_mangle]
pub extern "C" fn PyGILState_Check() -> c_int {
    gil_owned_by_current_thread() as c_int
}

/// `PyThreadState_Get`: the current thread's token.
#[no_mangle]
pub extern "C" fn PyThreadState_Get() -> *mut PyThreadState {
    thread_state_token()
}

/// `PyThreadState_GetUnchecked`: same as [`PyThreadState_Get`] here.
#[no_mangle]
pub extern "C" fn PyThreadState_GetUnchecked() -> *mut PyThreadState {
    thread_state_token()
}

/// `PyEval_SaveThread`: releases the GIL completely and returns the token.
///
/// Releasing all levels rather than the current one is deliberate: an
/// extension uses this to block without deadlocking, and a partially held lock
/// would defeat that. Pair it with [`PyEval_RestoreThread`].
#[no_mangle]
pub extern "C" fn PyEval_SaveThread() -> *mut PyThreadState {
    GIL_DEPTH.with(|slot| slot.set(0));
    let guard = GIL_GUARD.with(|slot| slot.borrow_mut().take());
    drop(guard);
    GIL_OWNER.with(|owner| owner.set(0));
    thread_state_token()
}

/// `PyEval_RestoreThread`: reacquires the GIL.
#[no_mangle]
pub extern "C" fn PyEval_RestoreThread(_state: *mut PyThreadState) {
    gil_acquire();
}

// ---------------------------------------------------------------------------
// Reference counting
// ---------------------------------------------------------------------------

/// Creates a shim object and returns it with a reference count of one.
///
/// Tuples are built with [`new_tuple`] because their item array is stored
/// inline and the payload moves past it.
pub(crate) fn new_object(kind: ShimKind) -> *mut ShimObject {
    assert!(
        !matches!(&kind, ShimKind::Tuple(_)),
        "tuples must be built with new_tuple"
    );
    let type_object = kind_type(&kind);
    let (ob_size, aux) = prefix_words(&kind);
    Box::into_raw(Box::new(ShimObject {
        head: PyObjectHead {
            ob_refcnt: AtomicIsize::new(1),
            ob_type: type_object,
        },
        ob_size: AtomicIsize::new(ob_size),
        aux: AtomicUsize::new(aux),
        kind,
    }))
}

/// Creates a tuple with its item pointers stored inline.
///
/// The allocation is `PyTupleObject`-shaped: `PyVarObject`, `ob_hash` and the
/// item array at [`tuple_layout::TUPLE_ITEMS_OFFSET`]. The [`ShimKind`]
/// payload follows the array, which is why [`kind_pointer`] must be used
/// instead of the `kind` field for tuples.
pub(crate) fn new_tuple(items: Vec<usize>) -> *mut ShimObject {
    let count = items.len();
    let size = tuple_layout::tuple_kind_offset(count) + std::mem::size_of::<ShimKind>();
    let layout = std::alloc::Layout::from_size_align(size, std::mem::align_of::<ShimKind>())
        .expect("tuple allocation size is representable");
    // SAFETY: `layout` has a non-zero size (`ShimKind` is non-empty).
    let memory = unsafe { std::alloc::alloc(layout) } as *mut ShimObject;
    if memory.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    // SAFETY: `memory` is a fresh allocation of `layout`; every field is
    // written before the handle escapes. The tuple prefix is written field by
    // field because the `kind` field of `ShimObject` would overlap the inline
    // item array.
    unsafe {
        ptr::write(
            ptr::addr_of_mut!((*memory).head),
            PyObjectHead {
                ob_refcnt: AtomicIsize::new(1),
                ob_type: ptr::addr_of_mut!(types::TUPLE_TYPE),
            },
        );
        ptr::write(
            ptr::addr_of_mut!((*memory).ob_size),
            AtomicIsize::new(count as isize),
        );
        ptr::write(
            ptr::addr_of_mut!((*memory).aux),
            AtomicUsize::new(tuple_layout::TUPLE_HASH_UNSET as usize),
        );
        let items_pointer = (memory as *mut u8).add(tuple_layout::TUPLE_ITEMS_OFFSET) as *mut usize;
        ptr::copy_nonoverlapping(items.as_ptr(), items_pointer, count);
        let kind_pointer =
            (memory as *mut u8).add(tuple_layout::tuple_kind_offset(count)) as *mut ShimKind;
        ptr::write(kind_pointer, ShimKind::Tuple(TupleItems::new(memory)));
    }
    memory
}

/// The CPython prefix words of a non-tuple payload.
///
/// `ob_size` is the length CPython's macros read for tuples, lists, bytes and
/// bytearrays. The word at offset 24 is `ob_shash` (-1, "not hashed") for
/// bytes and the list data pointer for lists, matching CPython 3.14. Unicode
/// objects carry no `ob_size` in CPython 3.14 (their length is a `PyASCIIObject`
/// bit-field this host does not mirror), so it stays zero.
fn prefix_words(kind: &ShimKind) -> (isize, usize) {
    match kind {
        ShimKind::List(items) => (items.len() as isize, list_items_pointer(items)),
        // CPython's `PyDictObject.ma_used` sits where the shim header keeps
        // `ob_size`; C code (PyO3's dict iterator, among others) reads it
        // directly, so it always mirrors the entry count.
        ShimKind::Dict(pairs) => (pairs.len() as isize, 0),
        ShimKind::Bytes { data, .. } => {
            (data.len() as isize, tuple_layout::TUPLE_HASH_UNSET as usize)
        }
        ShimKind::ByteArray { data, .. } => (data.len() as isize, 0),
        _ => (0, 0),
    }
}

/// Address of a list's `ob_item` array for a shim `Vec`.
///
/// An empty list reports NULL, as CPython does when nothing is allocated.
fn list_items_pointer(items: &[usize]) -> usize {
    if items.is_empty() {
        0
    } else {
        items.as_ptr() as usize
    }
}

/// Address of a live handle's [`ShimKind`] payload.
///
/// Tuples keep their items inline at [`tuple_layout::TUPLE_ITEMS_OFFSET`] and
/// the payload follows the array; every other kind keeps it at
/// [`tuple_layout::PREFIX_SIZE`].
///
/// # Safety
/// `object` must be a live shim handle (or a static constant object) whose
/// `ob_type` is readable.
pub(crate) unsafe fn kind_pointer(object: *mut ShimObject) -> *mut ShimKind {
    let base = object as *mut u8;
    // SAFETY: every handle starts with the CPython header.
    let type_object = unsafe { (*object).head.ob_type };
    if !type_object.is_null() && type_object == ptr::addr_of_mut!(types::TUPLE_TYPE) {
        // SAFETY: a tuple's `ob_size` describes its inline array.
        let count = unsafe { (*object).ob_size.load(Ordering::Relaxed) }.max(0) as usize;
        return base.add(tuple_layout::tuple_kind_offset(count)) as *mut ShimKind;
    }
    base.add(tuple_layout::PREFIX_SIZE) as *mut ShimKind
}

/// Mutable access to a live handle's payload.
///
/// # Safety
/// `object` must be a non-NULL live shim handle; the caller holds the GIL.
pub(crate) unsafe fn kind_mut(object: *mut ShimObject) -> &'static mut ShimKind {
    // SAFETY: delegated to the caller; the handle is heap-stable.
    unsafe { &mut *kind_pointer(object) }
}

/// Re-reads a list's `ob_size` and `ob_item` from its `Vec`.
///
/// The prefix is what an extension's inline macros see, so every mutation of
/// the `Vec` must republish both words before control returns to the
/// extension. A `Vec` reallocation changes the data pointer; the append,
/// insert, slice and sort paths call this afterwards.
pub(crate) fn sync_list_prefix(object: *mut ShimObject) {
    if object.is_null() {
        return;
    }
    // SAFETY: the caller owns a reference and holds the GIL; the payload is
    // at the fixed offset because this handle is a list, never a tuple.
    unsafe {
        if let ShimKind::List(items) = &*kind_pointer(object) {
            (*object)
                .ob_size
                .store(items.len() as isize, Ordering::Relaxed);
            (*object)
                .aux
                .store(list_items_pointer(items), Ordering::Relaxed);
        }
    }
}

/// Allocation layout of a live handle, used to release it.
fn object_layout(object: *mut ShimObject) -> std::alloc::Layout {
    // SAFETY: every handle starts with the CPython header.
    let type_object = unsafe { (*object).head.ob_type };
    if !type_object.is_null() && type_object == ptr::addr_of_mut!(types::TUPLE_TYPE) {
        // SAFETY: a tuple's `ob_size` describes its inline array.
        let count = unsafe { (*object).ob_size.load(Ordering::Relaxed) }.max(0) as usize;
        let size = tuple_layout::tuple_kind_offset(count) + std::mem::size_of::<ShimKind>();
        std::alloc::Layout::from_size_align(size, std::mem::align_of::<ShimKind>())
            .expect("tuple allocation size is representable")
    } else {
        std::alloc::Layout::new::<ShimObject>()
    }
}

/// Maps a payload to its builtin type object (address only; the metatype patch
/// is irrelevant here).
fn kind_type(kind: &ShimKind) -> *mut TypeObject {
    match kind {
        ShimKind::None => ptr::addr_of_mut!(types::NONE_TYPE),
        ShimKind::Bool(_) => ptr::addr_of_mut!(types::BOOL_TYPE),
        ShimKind::Int(_) => ptr::addr_of_mut!(types::INT_TYPE),
        ShimKind::Float(_) => ptr::addr_of_mut!(types::FLOAT_TYPE),
        ShimKind::Str { .. } => ptr::addr_of_mut!(types::STR_TYPE),
        ShimKind::Bytes { .. } => ptr::addr_of_mut!(types::BYTES_TYPE),
        ShimKind::ByteArray { .. } => ptr::addr_of_mut!(types::BYTEARRAY_TYPE),
        ShimKind::Set(_) => ptr::addr_of_mut!(types::SET_TYPE),
        ShimKind::FrozenSet(_) => ptr::addr_of_mut!(types::FROZENSET_TYPE),
        ShimKind::Complex { .. } => ptr::addr_of_mut!(types::COMPLEX_TYPE),
        ShimKind::List(_) => ptr::addr_of_mut!(types::LIST_TYPE),
        ShimKind::Tuple(_) => ptr::addr_of_mut!(types::TUPLE_TYPE),
        ShimKind::Dict(_) => ptr::addr_of_mut!(types::DICT_TYPE),
        ShimKind::Capsule { .. } => ptr::addr_of_mut!(types::CAPSULE_TYPE),
        ShimKind::Method { .. } => ptr::addr_of_mut!(types::METHOD_TYPE),
        ShimKind::Slice { .. } => ptr::addr_of_mut!(types::SLICE_TYPE),
        ShimKind::DictProxy { .. } => ptr::addr_of_mut!(types::DICT_PROXY_TYPE),
        ShimKind::GenericAlias { .. } => ptr::addr_of_mut!(types::GENERIC_ALIAS_TYPE),
        ShimKind::MemoryView { .. } => ptr::addr_of_mut!(types::MEMORYVIEW_TYPE),
        ShimKind::SeqIter { .. } => ptr::addr_of_mut!(types::SEQITER_TYPE),
        ShimKind::ContextVar { .. } => ptr::addr_of_mut!(types::CONTEXT_VAR_TYPE),
        ShimKind::ContextVarToken { .. } => ptr::addr_of_mut!(types::CONTEXT_TOKEN_TYPE),
        ShimKind::CFunction { .. } => ptr::addr_of_mut!(types::CFUNCTION_TYPE),
        ShimKind::RuntimeFunction { .. } => ptr::addr_of_mut!(types::CFUNCTION_TYPE),
        // An exception instance's `ob_type` is its exception class.
        ShimKind::Exception { exc_type, .. } => *exc_type as *mut TypeObject,
        ShimKind::NotImplemented => ptr::addr_of_mut!(types::NOT_IMPLEMENTED_TYPE),
        ShimKind::Ellipsis => ptr::addr_of_mut!(types::ELLIPSIS_TYPE),
        ShimKind::Module(_) | ShimKind::ModuleDef { .. } => {
            ptr::addr_of_mut!(types::MODULE_TYPE)
        }
        // A Hydro value uses the opaque wrapper type: `tp_basicsize` names
        // this host's wrapper, never Hydro heap storage. The Python-visible
        // type name comes from `object_type_name`. Date/time/timedelta/tzinfo
        // values instead point at the `datetime_CAPI` class objects, so the
        // CPython header macros (`PyDate_Check`, `PyDateTime_Check`, ...)
        // answer exactly as they do for a CPython datetime instance.
        // A Hydro class handle is a Python type: its metatype opts into
        // `Py_TPFLAGS_TYPE_SUBCLASS` (`PyType_Check`) and derives from `type`.
        ShimKind::Hydro(value) => types::datetime_capi_type_for(value.value())
            .or_else(|| types::hydro_class_metatype(value.value()))
            .unwrap_or(ptr::addr_of_mut!(types::HYDRO_TYPE)),
        ShimKind::Error => ptr::addr_of_mut!(types::ERROR_TYPE),
    }
}

/// Marks an object immortal and returns it.
fn make_immortal(object: *mut ShimObject) -> *mut ShimObject {
    // SAFETY: every handle starts with the CPython header.
    unsafe {
        (*object)
            .head
            .ob_refcnt
            .store(IMMORTAL_REFCNT, Ordering::Relaxed)
    };
    object
}

/// Increments a reference count, skipping immortal objects.
/// Whether a pointer can be a shim handle. Tagged Hydro immediates are not
/// pointers; a misaligned value must never be dereferenced while releasing a
/// malformed container.
fn is_aligned_handle(object: *mut ShimObject) -> bool {
    (object as usize) >= std::mem::align_of::<ShimObject>()
        && (object as usize).is_multiple_of(std::mem::align_of::<ShimObject>())
}

pub(crate) fn inc_ref(object: *mut ShimObject) {
    if object.is_null() || !is_aligned_handle(object) {
        return;
    }
    // SAFETY: every handle starts with the CPython header.
    let counter = unsafe { &(*object).head.ob_refcnt };
    let count = counter.load(Ordering::Relaxed);
    if count == IMMORTAL_REFCNT {
        return;
    }
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Decrements a reference count, freeing the handle at zero.
pub(crate) fn dec_ref(object: *mut ShimObject) {
    if object.is_null() || !is_aligned_handle(object) {
        return;
    }
    // SAFETY: every handle starts with the CPython header.
    let counter = unsafe { &(*object).head.ob_refcnt };
    let mut count = counter.load(Ordering::Relaxed);
    loop {
        if count == IMMORTAL_REFCNT || count <= 0 {
            return;
        }
        if count == 1 {
            match counter.compare_exchange(1, 0, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => {
                    free_object(object);
                    return;
                }
                Err(observed) => {
                    count = observed;
                    continue;
                }
            }
        }
        match counter.compare_exchange_weak(count, count - 1, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => count = observed,
        }
    }
}

/// Dispatches a zero-refcount handle to the right destructor.
fn free_object(object: *mut ShimObject) {
    // Finalisers and destructors are foreign code: serialise them.
    let _gripper = GilGuard::new();
    ensure_builtin_types();
    if types::is_type_pointer(object) {
        types::free_type_object(object as *mut TypeObject);
        return;
    }
    // SAFETY: every handle starts with the CPython header.
    let type_object = unsafe { (*object).head.ob_type };
    if type_object.is_null() {
        free_shim_object(object);
        return;
    }
    // SAFETY: `type_object` is a live type (the handle is being destroyed).
    let role = unsafe { (*type_object).role };
    if role == TypeRole::Shim {
        free_shim_object(object);
    } else {
        types::free_instance(object);
    }
}

/// Releases a shim payload's owned state, then its storage.
fn free_shim_object(object: *mut ShimObject) {
    // SAFETY: `object` is a shim allocation created by `new_object` or
    // `new_tuple`, and the reference count just reached zero, so this thread
    // owns it. The allocation stays alive while `release_kind` runs so capsule
    // destructors receive a valid (if dying) object, as in CPython.
    let layout = object_layout(object);
    let kind = unsafe { ptr::read(kind_pointer(object)) };
    drop_buffer_cache_entry(object as usize);
    release_kind(kind, object);
    // SAFETY: nothing references the handle any more and `layout` is the one
    // it was allocated with.
    unsafe { std::alloc::dealloc(object as *mut u8, layout) };
}

/// Side storage for the raw buffers handed to extensions.
///
/// `PyUnicode_DATA` and `PyUnicode_AsUTF8AndSize` must return pointers that
/// stay valid while the string object lives, including strings whose payload
/// cannot be expressed as a `CString` (embedded NULs, `PyUnicode_New` storage).
/// Buffers are keyed by object address and removed when the object is freed.
fn buffer_cache() -> MutexGuard<'static, HashMap<usize, Vec<u8>>> {
    static CACHE: LazyLock<Mutex<HashMap<usize, Vec<u8>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    CACHE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn drop_buffer_cache_entry(address: usize) {
    buffer_cache().remove(&address);
}

/// Returns a cached buffer for `object`, building it with `build` on first
/// use. The vector is never mutated again, so the returned pointer is stable
/// for the object's lifetime.
fn cached_buffer(object: *mut ShimObject, build: impl FnOnce() -> Vec<u8>) -> *const u8 {
    let mut cache = buffer_cache();
    let entry = cache.entry(object as usize).or_insert_with(build);
    entry.as_ptr()
}

/// Recomputes a `PyUnicode_New` string's text from the storage an extension
/// may have written through `PyUnicode_DATA`.
///
/// # Safety
/// `object` must be a live shim object; the caller holds the GIL, which is
/// what serialises writes to the raw buffer.
unsafe fn refresh_raw_unicode(object: *mut ShimObject) {
    // SAFETY: the caller upholds the liveness contract.
    let kind = unsafe { kind_mut(object) };
    let ShimKind::Str {
        text,
        c_text,
        raw: Some(raw),
    } = kind
    else {
        return;
    };
    let decoded = match raw.width {
        1 => raw
            .data
            .iter()
            .map(|&byte| byte as char)
            .collect::<String>(),
        2 => {
            let (chunks, _) = raw.data.as_chunks::<2>();
            let units: Vec<u16> = chunks
                .iter()
                .map(|&pair| u16::from_le_bytes(pair))
                .collect();
            String::from_utf16_lossy(&units)
        }
        4 => {
            let mut decoded = String::with_capacity(raw.data.len() / 4);
            let (chunks, _) = raw.data.as_chunks::<4>();
            for chunk in chunks {
                let code = u32::from_le_bytes(*chunk);
                decoded.push(char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER));
            }
            decoded
        }
        _ => return,
    };
    if *text != decoded {
        *text = decoded;
    }
    *c_text = CString::new(text.as_bytes()).ok();
}

/// Drops one payload, decrementing owned children and running capsule
/// destructors.
fn release_kind(kind: ShimKind, address: *mut ShimObject) {
    match kind {
        ShimKind::List(items) => {
            for item in items {
                if item != 0 {
                    dec_ref(item as *mut ShimObject);
                }
            }
        }
        // The items were read from the inline array; the allocation is still
        // alive here, so the view is valid until `free_shim_object` deallocs.
        ShimKind::Tuple(items) => {
            for &item in items.as_slice() {
                if item != 0 {
                    dec_ref(item as *mut ShimObject);
                }
            }
        }
        ShimKind::Dict(pairs) => {
            for (key, value) in pairs {
                if key != 0 {
                    dec_ref(key as *mut ShimObject);
                }
                if value != 0 {
                    dec_ref(value as *mut ShimObject);
                }
            }
        }
        ShimKind::Capsule {
            ptr: _,
            name: _,
            context: _,
            destructor,
        } => {
            if destructor != 0 {
                // SAFETY: the extension stored a `void (*)(PyObject *)` in the
                // destructor slot, which is its documented type.
                let destructor: unsafe extern "C" fn(*mut ShimObject) =
                    unsafe { std::mem::transmute(destructor as *const c_void) };
                // SAFETY: `address` is the live capsule object, as CPython
                // passes to capsule destructors.
                unsafe { destructor(address) };
            }
        }
        ShimKind::CFunction {
            method: _,
            self_object,
            module,
        } => {
            if self_object != 0 {
                dec_ref(self_object as *mut ShimObject);
            }
            if module != 0 {
                dec_ref(module as *mut ShimObject);
            }
        }
        ShimKind::Set(items) | ShimKind::FrozenSet(items) => {
            for item in items {
                if item != 0 {
                    dec_ref(item as *mut ShimObject);
                }
            }
        }
        ShimKind::Exception {
            exc_type: _,
            args,
            cause,
            context,
            traceback,
        } => {
            for arg in args {
                if arg != 0 {
                    dec_ref(arg as *mut ShimObject);
                }
            }
            if cause != 0 {
                dec_ref(cause as *mut ShimObject);
            }
            if context != 0 {
                dec_ref(context as *mut ShimObject);
            }
            if traceback != 0 {
                dec_ref(traceback as *mut ShimObject);
            }
        }
        ShimKind::Method {
            function,
            self_object,
        } => {
            if function != 0 {
                dec_ref(function as *mut ShimObject);
            }
            if self_object != 0 {
                dec_ref(self_object as *mut ShimObject);
            }
        }
        ShimKind::Slice { start, stop, step } => {
            for component in [start, stop, step] {
                if component != 0 {
                    dec_ref(component as *mut ShimObject);
                }
            }
        }
        ShimKind::DictProxy { dict } => {
            if dict != 0 {
                dec_ref(dict as *mut ShimObject);
            }
        }
        ShimKind::GenericAlias { origin, args } => {
            if origin != 0 {
                dec_ref(origin as *mut ShimObject);
            }
            if args != 0 {
                dec_ref(args as *mut ShimObject);
            }
        }
        ShimKind::MemoryView { buffer } => {
            // Dropping the last `Arc` releases the acquired buffer through
            // `MemoryViewBuffer::drop`.
            drop(buffer);
        }
        ShimKind::SeqIter { sequence, .. } => {
            if sequence != 0 {
                dec_ref(sequence as *mut ShimObject);
            }
        }
        ShimKind::ContextVar {
            default, values, ..
        } => {
            if default != 0 {
                dec_ref(default as *mut ShimObject);
            }
            drop(values);
        }
        ShimKind::ContextVarToken { var, previous } => {
            if var != 0 {
                dec_ref(var as *mut ShimObject);
            }
            if previous != 0 {
                dec_ref(previous as *mut ShimObject);
            }
        }
        ShimKind::None
        | ShimKind::Bool(_)
        | ShimKind::Int(_)
        | ShimKind::Float(_)
        | ShimKind::Str { .. }
        | ShimKind::ByteArray { .. }
        | ShimKind::Complex { .. }
        | ShimKind::RuntimeFunction { .. }
        | ShimKind::Bytes { .. }
        | ShimKind::NotImplemented
        | ShimKind::Ellipsis
        | ShimKind::Module(_)
        | ShimKind::ModuleDef { .. }
        | ShimKind::Hydro(_)
        | ShimKind::Error => {}
    }
}

/// `Py_IncRef`.
#[no_mangle]
pub extern "C" fn Py_IncRef(object: *mut ShimObject) {
    inc_ref(object);
}

/// `Py_DecRef`.
#[no_mangle]
pub extern "C" fn Py_DecRef(object: *mut ShimObject) {
    dec_ref(object);
}

/// `_Py_IncRef`: the non-NULL spelling used by 3.12+ limited-API headers.
#[no_mangle]
pub extern "C" fn _Py_IncRef(object: *mut ShimObject) {
    inc_ref(object);
}

/// `_Py_DecRef`: the non-NULL spelling used by 3.12+ limited-API headers.
#[no_mangle]
pub extern "C" fn _Py_DecRef(object: *mut ShimObject) {
    dec_ref(object);
}

/// `Py_REFCNT` (stable-ABI function since 3.14).
#[no_mangle]
pub extern "C" fn Py_REFCNT(object: *mut ShimObject) -> isize {
    if object.is_null() {
        return 0;
    }
    // SAFETY: every handle starts with the CPython header.
    unsafe { (*object).head.ob_refcnt.load(Ordering::Relaxed) }
}

/// `Py_TYPE` (stable-ABI function since 3.14).
#[no_mangle]
pub extern "C" fn Py_TYPE(object: *mut ShimObject) -> *mut PyTypeObject {
    type_of(object)
}

// ---------------------------------------------------------------------------
// Exported constant singletons
// ---------------------------------------------------------------------------

macro_rules! constant_objects {
    ($($symbol:ident => $kind:expr),+ $(,)?) => {
        $(
            /// Exported CPython constant singleton.
            #[no_mangle]
            pub static mut $symbol: ShimObject = ShimObject {
                head: PyObjectHead {
                    ob_refcnt: AtomicIsize::new(IMMORTAL_REFCNT),
                    ob_type: ptr::null_mut(),
                },
                ob_size: AtomicIsize::new(0),
                aux: AtomicUsize::new(0),
                kind: $kind,
            };
        )+

        /// References every constant object so the staticlib linker keeps the
        /// data even though nothing in Rust reads it directly.
        fn anchor_constant_objects() {
            let symbols: [*const ShimObject; _] = [$(
                std::ptr::addr_of!($symbol),
            )+];
            let _ = std::hint::black_box(symbols);
        }
    };
}

constant_objects! {
    _Py_NoneStruct => ShimKind::None,
    _Py_FalseStruct => ShimKind::Bool(false),
    _Py_TrueStruct => ShimKind::Bool(true),
    _Py_NotImplementedStruct => ShimKind::NotImplemented,
    _Py_EllipsisObject => ShimKind::Ellipsis,
}

/// Pointer spellings of the constant singletons (`Py_None`, `Py_True`,
/// `Py_False`). CPython 3.14 headers normally macro-expand these names, but
/// extension code built against older headers (or taking the address
/// explicitly) expects real data symbols.
#[no_mangle]
pub static mut Py_None: *mut ShimObject = ptr::null_mut();
#[no_mangle]
pub static mut Py_True: *mut ShimObject = ptr::null_mut();
#[no_mangle]
pub static mut Py_False: *mut ShimObject = ptr::null_mut();

/// `Py_Version`: CPython's `PY_VERSION_HEX` for 3.14.9 final.
#[no_mangle]
pub static Py_Version: std::os::raw::c_ulong = 0x030E_09F0;

/// `_Py_ascii_whitespace`: 128-entry table, one for ASCII whitespace.
#[no_mangle]
pub static _Py_ascii_whitespace: [u8; 128] = {
    let mut table = [0u8; 128];
    table[0x09] = 1; // tab
    table[0x0A] = 1; // newline
    table[0x0B] = 1; // vertical tab
    table[0x0C] = 1; // form feed
    table[0x0D] = 1; // carriage return
    table[0x20] = 1; // space
    table
};

/// `_PyByteArray_empty_string`: the shared empty bytearray buffer.
#[no_mangle]
pub static mut _PyByteArray_empty_string: [u8; 1] = [0];

/// Patches the constant singletons' `ob_type` after the builtin type table is
/// ready. Called once from [`types::ensure_builtin_types`].
pub(crate) fn patch_constant_types() {
    // SAFETY: runs inside a `Once`, before any extension can observe the
    // constants; the writes are plain stores of static addresses.
    unsafe {
        _Py_NoneStruct.head.ob_type = ptr::addr_of_mut!(types::NONE_TYPE);
        _Py_FalseStruct.head.ob_type = ptr::addr_of_mut!(types::BOOL_TYPE);
        _Py_TrueStruct.head.ob_type = ptr::addr_of_mut!(types::BOOL_TYPE);
        _Py_NotImplementedStruct.head.ob_type = ptr::addr_of_mut!(types::NOT_IMPLEMENTED_TYPE);
        _Py_EllipsisObject.head.ob_type = ptr::addr_of_mut!(types::ELLIPSIS_TYPE);
        Py_None = ptr::addr_of_mut!(_Py_NoneStruct);
        Py_True = ptr::addr_of_mut!(_Py_TrueStruct);
        Py_False = ptr::addr_of_mut!(_Py_FalseStruct);
    }
}

/// Interned constants: small integers and the empty `str`/`bytes`/`tuple`.
struct InternedConstants {
    /// `-5..=256`, indexed by `value + 5`.
    ints: [usize; 262],
    empty_str: usize,
    empty_bytes: usize,
    empty_tuple: usize,
}

static INTERNED: LazyLock<InternedConstants> = LazyLock::new(|| {
    let mut ints = [0usize; 262];
    for (index, slot) in ints.iter_mut().enumerate() {
        let value = index as i64 - 5;
        *slot = make_immortal(new_object(ShimKind::Int(value))) as usize;
    }
    let empty_str = make_immortal(new_object(ShimKind::Str {
        text: String::new(),
        c_text: Some(CString::default()),
        raw: None,
    })) as usize;
    let empty_bytes = make_immortal(new_object(ShimKind::Bytes {
        data: Vec::new(),
        c_data: vec![0],
    })) as usize;
    let empty_tuple = make_immortal(new_tuple(Vec::new())) as usize;
    InternedConstants {
        ints,
        empty_str,
        empty_bytes,
        empty_tuple,
    }
});

fn interned() -> &'static InternedConstants {
    &INTERNED
}

/// Returns the cached integer object for `-5..=256`.
fn cached_int(value: i64) -> Option<*mut ShimObject> {
    if (-5..=256).contains(&value) {
        Some(interned().ints[(value + 5) as usize] as *mut ShimObject)
    } else {
        None
    }
}

fn new_int(value: i64) -> *mut ShimObject {
    cached_int(value).unwrap_or_else(|| new_object(ShimKind::Int(value)))
}

fn new_str(text: String) -> *mut ShimObject {
    if text.is_empty() {
        return interned().empty_str as *mut ShimObject;
    }
    // A `CString` keeps the `const char *` handed out by
    // `PyArg_ParseTuple("s")` valid while the object lives.
    let c_text = CString::new(text.as_bytes()).ok();
    new_object(ShimKind::Str {
        text,
        c_text,
        raw: None,
    })
}

fn new_bytes(data: Vec<u8>) -> *mut ShimObject {
    if data.is_empty() {
        return interned().empty_bytes as *mut ShimObject;
    }
    let mut c_data = data.clone();
    c_data.push(0);
    new_object(ShimKind::Bytes { data, c_data })
}

fn empty_tuple() -> *mut ShimObject {
    interned().empty_tuple as *mut ShimObject
}

/// Resolves a `Py_GetConstant` identifier.
fn constant_object(constant_id: c_uint) -> Option<*mut ShimObject> {
    let pointer = match constant_id {
        PY_CONSTANT_NONE => ptr::addr_of_mut!(_Py_NoneStruct),
        PY_CONSTANT_FALSE => ptr::addr_of_mut!(_Py_FalseStruct),
        PY_CONSTANT_TRUE => ptr::addr_of_mut!(_Py_TrueStruct),
        PY_CONSTANT_ELLIPSIS => ptr::addr_of_mut!(_Py_EllipsisObject),
        PY_CONSTANT_NOT_IMPLEMENTED => ptr::addr_of_mut!(_Py_NotImplementedStruct),
        PY_CONSTANT_ZERO => cached_int(0)?,
        PY_CONSTANT_ONE => cached_int(1)?,
        PY_CONSTANT_EMPTY_STR => interned().empty_str as *mut ShimObject,
        PY_CONSTANT_EMPTY_BYTES => interned().empty_bytes as *mut ShimObject,
        PY_CONSTANT_EMPTY_TUPLE => empty_tuple(),
        _ => return None,
    };
    Some(pointer)
}

/// `Py_GetConstant`: returns a new reference to a Python constant.
#[no_mangle]
pub extern "C" fn Py_GetConstant(constant_id: c_uint) -> *mut ShimObject {
    let Some(object) = constant_object(constant_id) else {
        set_error(format!(
            "TypeError: Py_GetConstant: unknown constant id {constant_id}"
        ));
        return ptr::null_mut();
    };
    inc_ref(object);
    object
}

/// `Py_GetConstantBorrowed`: returns a borrowed reference to a constant.
#[no_mangle]
pub extern "C" fn Py_GetConstantBorrowed(constant_id: c_uint) -> *mut ShimObject {
    match constant_object(constant_id) {
        Some(object) => object,
        None => {
            set_error(format!(
                "TypeError: Py_GetConstantBorrowed: unknown constant id {constant_id}"
            ));
            ptr::null_mut()
        }
    }
}

/// `Py_Is`.
#[no_mangle]
pub extern "C" fn Py_Is(left: *mut ShimObject, right: *mut ShimObject) -> c_int {
    (left == right) as c_int
}

/// `Py_IsNone`.
#[no_mangle]
pub extern "C" fn Py_IsNone(object: *mut ShimObject) -> c_int {
    (object == ptr::addr_of_mut!(_Py_NoneStruct)) as c_int
}

/// `Py_IsTrue`.
#[no_mangle]
pub extern "C" fn Py_IsTrue(object: *mut ShimObject) -> c_int {
    (object == ptr::addr_of_mut!(_Py_TrueStruct)) as c_int
}

/// `Py_IsFalse`.
#[no_mangle]
pub extern "C" fn Py_IsFalse(object: *mut ShimObject) -> c_int {
    (object == ptr::addr_of_mut!(_Py_FalseStruct)) as c_int
}

// ---------------------------------------------------------------------------
// Exception classes
// ---------------------------------------------------------------------------

macro_rules! exception_type_symbols {
    ($($symbol:ident => $name:literal),+ $(,)?) => {
        $(
            /// Exported CPython exception-class singleton.
            #[no_mangle]
            pub static mut $symbol: *mut ShimObject = ptr::null_mut();
        )+

        /// Populates every exported exception class, exactly once.
        fn initialize_exception_types() {
            static ONCE: Once = Once::new();
            ONCE.call_once(|| {
                ensure_builtin_types();
                unsafe {
                    $(
                        $symbol = types::allocate_exception_type(
                            $name,
                            PY_TPFLAGS_IMMUTABLETYPE
                                | PY_TPFLAGS_BASETYPE
                                | PY_TPFLAGS_BASE_EXC_SUBCLASS,
                        ) as *mut ShimObject;
                    )+
                }
                link_exception_bases();
            });
        }

        /// Resolves an exported exception class by its short name.
        fn exception_class_by_name(name: &str) -> *mut ShimObject {
            initialize_exception_types();
            unsafe {
                $(
                    if name == $name {
                        return $symbol;
                    }
                )+
            }
            ptr::null_mut()
        }

        /// References every class so the staticlib linker keeps the data.
        fn anchor_exception_types() {
            let symbols = [$(
                std::ptr::addr_of_mut!($symbol),
            )+];
            let _ = std::hint::black_box(symbols);
        }
    };
}

exception_type_symbols! {
    PyExc_ArithmeticError => "ArithmeticError",
    PyExc_AssertionError => "AssertionError",
    PyExc_AttributeError => "AttributeError",
    PyExc_BaseException => "BaseException",
    PyExc_BaseExceptionGroup => "BaseExceptionGroup",
    PyExc_BlockingIOError => "BlockingIOError",
    PyExc_BrokenPipeError => "BrokenPipeError",
    PyExc_BufferError => "BufferError",
    PyExc_BytesWarning => "BytesWarning",
    PyExc_ChildProcessError => "ChildProcessError",
    PyExc_ConnectionAbortedError => "ConnectionAbortedError",
    PyExc_ConnectionError => "ConnectionError",
    PyExc_ConnectionRefusedError => "ConnectionRefusedError",
    PyExc_ConnectionResetError => "ConnectionResetError",
    PyExc_DeprecationWarning => "DeprecationWarning",
    PyExc_EncodingWarning => "EncodingWarning",
    PyExc_EnvironmentError => "EnvironmentError",
    PyExc_EOFError => "EOFError",
    PyExc_Exception => "Exception",
    PyExc_FileExistsError => "FileExistsError",
    PyExc_FileNotFoundError => "FileNotFoundError",
    PyExc_FloatingPointError => "FloatingPointError",
    PyExc_FutureWarning => "FutureWarning",
    PyExc_GeneratorExit => "GeneratorExit",
    PyExc_ImportError => "ImportError",
    PyExc_ImportWarning => "ImportWarning",
    PyExc_IndentationError => "IndentationError",
    PyExc_IndexError => "IndexError",
    PyExc_InterruptedError => "InterruptedError",
    PyExc_IOError => "IOError",
    PyExc_IsADirectoryError => "IsADirectoryError",
    PyExc_KeyboardInterrupt => "KeyboardInterrupt",
    PyExc_KeyError => "KeyError",
    PyExc_LookupError => "LookupError",
    PyExc_MemoryError => "MemoryError",
    PyExc_ModuleNotFoundError => "ModuleNotFoundError",
    PyExc_NameError => "NameError",
    PyExc_NotADirectoryError => "NotADirectoryError",
    PyExc_NotImplementedError => "NotImplementedError",
    PyExc_OSError => "OSError",
    PyExc_OverflowError => "OverflowError",
    PyExc_PendingDeprecationWarning => "PendingDeprecationWarning",
    PyExc_PermissionError => "PermissionError",
    PyExc_ProcessLookupError => "ProcessLookupError",
    PyExc_RecursionError => "RecursionError",
    PyExc_ReferenceError => "ReferenceError",
    PyExc_ResourceWarning => "ResourceWarning",
    PyExc_RuntimeError => "RuntimeError",
    PyExc_RuntimeWarning => "RuntimeWarning",
    PyExc_StopAsyncIteration => "StopAsyncIteration",
    PyExc_StopIteration => "StopIteration",
    PyExc_SyntaxError => "SyntaxError",
    PyExc_SyntaxWarning => "SyntaxWarning",
    PyExc_SystemError => "SystemError",
    PyExc_SystemExit => "SystemExit",
    PyExc_TabError => "TabError",
    PyExc_TimeoutError => "TimeoutError",
    PyExc_TypeError => "TypeError",
    PyExc_UnboundLocalError => "UnboundLocalError",
    PyExc_UnicodeDecodeError => "UnicodeDecodeError",
    PyExc_UnicodeEncodeError => "UnicodeEncodeError",
    PyExc_UnicodeError => "UnicodeError",
    PyExc_UnicodeTranslateError => "UnicodeTranslateError",
    PyExc_UnicodeWarning => "UnicodeWarning",
    PyExc_UserWarning => "UserWarning",
    PyExc_ValueError => "ValueError",
    PyExc_Warning => "Warning",
    PyExc_WindowsError => "WindowsError",
    PyExc_ZeroDivisionError => "ZeroDivisionError",
}

/// Sets `tp_base` on one exception class so `is_subtype`/`PyErr_GivenExceptionMatches`
/// see the documented CPython hierarchy.
fn set_exception_base(child: *mut ShimObject, base: *mut ShimObject) {
    if child.is_null() || base.is_null() || child == base {
        return;
    }
    // SAFETY: both pointers are live exception classes created by
    // `initialize_exception_types`; the metatype link is already patched.
    unsafe {
        (*(child as *mut TypeObject)).tp_base = base as *mut TypeObject;
    }
}

macro_rules! exception_bases {
    ($($child:ident => $base:ident),+ $(,)?) => {
        /// Links every exception class to its CPython parent.
        fn link_exception_bases() {
            $(
                set_exception_base(unsafe { $child }, unsafe { $base });
            )+
        }
    };
}

exception_bases! {
    PyExc_Exception => PyExc_BaseException,
    PyExc_BaseExceptionGroup => PyExc_BaseException,
    PyExc_GeneratorExit => PyExc_BaseException,
    PyExc_KeyboardInterrupt => PyExc_BaseException,
    PyExc_SystemExit => PyExc_BaseException,
    PyExc_ArithmeticError => PyExc_Exception,
    PyExc_AssertionError => PyExc_Exception,
    PyExc_AttributeError => PyExc_Exception,
    PyExc_BufferError => PyExc_Exception,
    PyExc_EOFError => PyExc_Exception,
    PyExc_ImportError => PyExc_Exception,
    PyExc_LookupError => PyExc_Exception,
    PyExc_MemoryError => PyExc_Exception,
    PyExc_NameError => PyExc_Exception,
    PyExc_OSError => PyExc_Exception,
    PyExc_ReferenceError => PyExc_Exception,
    PyExc_RuntimeError => PyExc_Exception,
    PyExc_StopAsyncIteration => PyExc_Exception,
    PyExc_StopIteration => PyExc_Exception,
    PyExc_SyntaxError => PyExc_Exception,
    PyExc_SystemError => PyExc_Exception,
    PyExc_TypeError => PyExc_Exception,
    PyExc_ValueError => PyExc_Exception,
    PyExc_Warning => PyExc_Exception,
    PyExc_ModuleNotFoundError => PyExc_ImportError,
    PyExc_IndexError => PyExc_LookupError,
    PyExc_KeyError => PyExc_LookupError,
    PyExc_FloatingPointError => PyExc_ArithmeticError,
    PyExc_OverflowError => PyExc_ArithmeticError,
    PyExc_ZeroDivisionError => PyExc_ArithmeticError,
    PyExc_BlockingIOError => PyExc_OSError,
    PyExc_BrokenPipeError => PyExc_OSError,
    PyExc_ChildProcessError => PyExc_OSError,
    PyExc_ConnectionError => PyExc_OSError,
    PyExc_FileExistsError => PyExc_OSError,
    PyExc_FileNotFoundError => PyExc_OSError,
    PyExc_InterruptedError => PyExc_OSError,
    PyExc_IsADirectoryError => PyExc_OSError,
    PyExc_NotADirectoryError => PyExc_OSError,
    PyExc_PermissionError => PyExc_OSError,
    PyExc_ProcessLookupError => PyExc_OSError,
    PyExc_TimeoutError => PyExc_OSError,
    PyExc_ConnectionAbortedError => PyExc_ConnectionError,
    PyExc_ConnectionRefusedError => PyExc_ConnectionError,
    PyExc_ConnectionResetError => PyExc_ConnectionError,
    PyExc_UnboundLocalError => PyExc_NameError,
    PyExc_IndentationError => PyExc_SyntaxError,
    PyExc_TabError => PyExc_IndentationError,
    PyExc_UnicodeError => PyExc_ValueError,
    PyExc_UnicodeDecodeError => PyExc_UnicodeError,
    PyExc_UnicodeEncodeError => PyExc_UnicodeError,
    PyExc_UnicodeTranslateError => PyExc_UnicodeError,
    PyExc_BytesWarning => PyExc_Warning,
    PyExc_DeprecationWarning => PyExc_Warning,
    PyExc_EncodingWarning => PyExc_Warning,
    PyExc_FutureWarning => PyExc_Warning,
    PyExc_ImportWarning => PyExc_Warning,
    PyExc_PendingDeprecationWarning => PyExc_Warning,
    PyExc_ResourceWarning => PyExc_Warning,
    PyExc_RuntimeWarning => PyExc_Warning,
    PyExc_SyntaxWarning => PyExc_Warning,
    PyExc_UnicodeWarning => PyExc_Warning,
    PyExc_UserWarning => PyExc_Warning,
    // `EnvironmentError`, `IOError` and `WindowsError` are aliases of `OSError`
    // in CPython 3; the exported symbols stay distinct but share the parent.
    PyExc_EnvironmentError => PyExc_OSError,
    PyExc_IOError => PyExc_OSError,
    PyExc_WindowsError => PyExc_OSError,
}

/// Maps an exported exception class to the loader's message prefix.
fn exception_prefix(exception: *mut ShimObject) -> String {
    ensure_builtin_types();
    initialize_exception_types();
    if !exception.is_null() {
        if types::is_type_pointer(exception) {
            let name = types::type_name(exception as *mut TypeObject);
            if name != "type" {
                return name;
            }
        } else if let Some(ShimKind::Exception { exc_type, .. }) = kind_ref(exception) {
            return types::type_name(*exc_type as *mut TypeObject);
        } else {
            let type_object = type_of(exception);
            if !type_object.is_null() {
                // SAFETY: live type object.
                let role = unsafe { (*type_object).role };
                if role == TypeRole::Metatype {
                    return types::type_name(type_object);
                }
            }
        }
    }
    "RuntimeError".to_string()
}

/// Builds an exception instance of `exc_type` with positional `args`.
fn new_exception_instance(exc_type: usize, args: Vec<*mut ShimObject>) -> *mut ShimObject {
    for &arg in &args {
        inc_ref(arg);
    }
    new_object(ShimKind::Exception {
        exc_type,
        args: args.into_iter().map(|arg| arg as usize).collect(),
        cause: 0,
        context: 0,
        traceback: 0,
    })
}

/// `str()` of an exception instance: `""`, `str(arg)` or `str(tuple(args))`.
fn exception_text(kind: &ShimKind) -> String {
    let ShimKind::Exception { args, .. } = kind else {
        return String::new();
    };
    match args.len() {
        0 => String::new(),
        1 => value_text(args[0] as *mut ShimObject),
        _ => {
            let items: Vec<String> = args
                .iter()
                .map(|&arg| value_text(arg as *mut ShimObject))
                .collect();
            format!("({})", items.join(", "))
        }
    }
}

/// `repr()` of an exception instance: `Name(arg, ...)`.
fn exception_repr(exc_type: usize, args: &[usize], depth: usize) -> String {
    let name = types::type_name(exc_type as *mut TypeObject);
    let rendered: Vec<String> = args
        .iter()
        .map(|&arg| shim_repr_at(arg as *mut ShimObject, depth + 1))
        .collect();
    format!("{name}({})", rendered.join(", "))
}

/// `str()` of any handle: strings unquoted, everything else through `repr`.
fn value_text(object: *mut ShimObject) -> String {
    match kind_ref(object) {
        Some(ShimKind::Str { text, .. }) => text.clone(),
        _ => shim_repr(object),
    }
}

/// Splits the loader spelling `"<Type>: <message>"` and materialises the
/// exception instance it describes.
fn exception_from_message(message: &str) -> *mut ShimObject {
    let (name, text) = match message.split_once(": ") {
        Some((name, text)) => (name, text),
        None => ("RuntimeError", message),
    };
    let exc_type = {
        let known = exception_class_by_name(name);
        if known.is_null() {
            unsafe { PyExc_RuntimeError }
        } else {
            known
        }
    };
    let args = if text.is_empty() {
        Vec::new()
    } else {
        vec![new_str(text.to_string())]
    };
    // The constructor takes ownership of a vector and keeps its own references,
    // so pass a copy and release the caller's references below.
    let instance = new_exception_instance(exc_type as usize, args.clone());
    for arg in args {
        dec_ref(arg);
    }
    instance
}

// ---------------------------------------------------------------------------
// Values and objects
// ---------------------------------------------------------------------------

/// Turns a contract value into a handle.
///
/// Every return value is a new reference, including [`ShimValue::Object`],
/// which transfers its owned reference.
pub(crate) fn value_object(value: ShimValue) -> *mut ShimObject {
    match value {
        ShimValue::None => ptr::addr_of_mut!(_Py_NoneStruct),
        ShimValue::Bool(true) => ptr::addr_of_mut!(_Py_TrueStruct),
        ShimValue::Bool(false) => ptr::addr_of_mut!(_Py_FalseStruct),
        ShimValue::NotImplemented => ptr::addr_of_mut!(_Py_NotImplementedStruct),
        ShimValue::Ellipsis => ptr::addr_of_mut!(_Py_EllipsisObject),
        ShimValue::Int(number) => new_int(number),
        ShimValue::Float(number) => new_object(ShimKind::Float(number)),
        ShimValue::Str(text) => new_str(text),
        ShimValue::Bytes(data) => new_bytes(data),
        ShimValue::List(items) => new_object(ShimKind::List(
            items
                .into_iter()
                .map(|item| value_object(item) as usize)
                .collect(),
        )),
        ShimValue::Tuple(items) => {
            if items.is_empty() {
                empty_tuple()
            } else {
                new_tuple(
                    items
                        .into_iter()
                        .map(|item| value_object(item) as usize)
                        .collect(),
                )
            }
        }
        ShimValue::Dict(pairs) => new_object(ShimKind::Dict(
            pairs
                .into_iter()
                .map(|(key, value)| (value_object(key) as usize, value_object(value) as usize))
                .collect(),
        )),
        ShimValue::Set(items) => new_object(ShimKind::Set(
            items
                .into_iter()
                .map(|item| value_object(item) as usize)
                .collect(),
        )),
        ShimValue::Capsule { ptr, name } => new_object(ShimKind::Capsule {
            ptr,
            name: name.and_then(|name| CString::new(name).ok()),
            context: 0,
            destructor: 0,
        }),
        // The wrapped value's root moves into the handle; releasing the last
        // C reference drops it again.
        ShimValue::Hydro(handle) => new_object(ShimKind::Hydro(handle)),
        // Transfer the owned reference: the returned pointer is a new
        // reference, exactly like the freshly allocated arms above.
        ShimValue::Object { object, .. } => object.into_raw(),
    }
}

/// Builds the positional argument tuple passed to a `METH_VARARGS` method.
/// Only the tests construct tuples through this helper; the call paths build
/// them from already-owned pointers.
#[cfg(test)]
fn tuple_object(arguments: &[ShimValue]) -> *mut ShimObject {
    if arguments.is_empty() {
        return empty_tuple();
    }
    new_tuple(
        arguments
            .iter()
            .cloned()
            .map(|value| value_object(value) as usize)
            .collect(),
    )
}

/// Borrows the kind of a live shim object.
///
/// Returns `None` for NULL, type objects and extension-owned instances, so no
/// caller can mistake an instance payload for a `ShimKind`.
pub(crate) fn kind_ref(object: *mut ShimObject) -> Option<&'static ShimKind> {
    if object.is_null() {
        return None;
    }
    ensure_builtin_types();
    // SAFETY: every handle starts with the CPython header.
    let type_object = unsafe { (*object).head.ob_type };
    if type_object.is_null() {
        return None;
    }
    // SAFETY: live type object.
    let role = unsafe { (*type_object).role };
    if role != TypeRole::Shim {
        return None;
    }
    // SAFETY: the payload follows the header for every shim object, and the
    // GIL serialises the refresh against extension writes through
    // `PyUnicode_DATA`.
    unsafe {
        refresh_raw_unicode(object);
        Some(&*kind_pointer(object))
    }
}

/// Python type name of any handle.
fn object_type_name(object: *mut ShimObject) -> String {
    if object.is_null() {
        return "NULL".to_string();
    }
    if let Some(handle) = hydro_handle(object) {
        return handle.type_name().to_string();
    }
    if types::is_type_pointer(object) {
        return "type".to_string();
    }
    let type_object = type_of(object);
    if type_object.is_null() {
        return "<unknown>".to_string();
    }
    types::type_name(type_object)
}

/// Wraps `object` as an opaque contract value owning one new reference.
fn opaque_object(object: *mut ShimObject, type_name: String) -> ShimValue {
    ShimValue::Object {
        object: OwnedShimObject::new(object),
        type_name,
    }
}

/// Converts an extension's return object to the contract value.
pub(crate) fn object_to_value(object: *mut ShimObject) -> Result<ShimValue, String> {
    object_to_value_at(object, 0, &mut Vec::new())
}

fn object_to_value_at(
    object: *mut ShimObject,
    depth: usize,
    path: &mut Vec<usize>,
) -> Result<ShimValue, String> {
    if object.is_null() {
        return Err("cannot convert a NULL object".to_string());
    }
    if types::is_type_pointer(object) {
        // Type objects cross the boundary as opaque handles: they are
        // callable, expose `__name__`/`__module__` and participate in
        // `isinstance`.
        return Ok(opaque_object(
            object,
            types::type_name(object as *mut TypeObject),
        ));
    }
    let Some(kind) = kind_ref(object) else {
        // An extension-owned instance (including instances of types created by
        // the extension) has no shim payload; keep the handle itself.
        return Ok(opaque_object(object, object_type_name(object)));
    };
    match kind {
        ShimKind::None => Ok(ShimValue::None),
        ShimKind::Bool(flag) => Ok(ShimValue::Bool(*flag)),
        ShimKind::Int(number) => Ok(ShimValue::Int(*number)),
        ShimKind::Float(number) => Ok(ShimValue::Float(*number)),
        ShimKind::Str { text, .. } => Ok(ShimValue::Str(text.clone())),
        ShimKind::Bytes { data, .. } => Ok(ShimValue::Bytes(data.clone())),
        ShimKind::List(items) => {
            begin_container(object as usize, "list", depth, path)?;
            let mut converted = Vec::with_capacity(items.len());
            for &item in items {
                if item == 0 {
                    return Err("extension returned a list with an uninitialized item".to_string());
                }
                converted.push(object_to_value_at(
                    item as *mut ShimObject,
                    depth + 1,
                    path,
                )?);
            }
            path.pop();
            Ok(ShimValue::List(converted))
        }
        ShimKind::Tuple(items) => {
            begin_container(object as usize, "tuple", depth, path)?;
            let mut converted = Vec::with_capacity(items.len());
            for &item in items {
                if item == 0 {
                    return Err("extension returned a tuple with an uninitialized item".to_string());
                }
                converted.push(object_to_value_at(
                    item as *mut ShimObject,
                    depth + 1,
                    path,
                )?);
            }
            path.pop();
            Ok(ShimValue::Tuple(converted))
        }
        ShimKind::Dict(pairs) => {
            begin_container(object as usize, "dict", depth, path)?;
            let mut converted = Vec::with_capacity(pairs.len());
            for &(key, value) in pairs {
                if key == 0 || value == 0 {
                    return Err("extension returned a dict with an uninitialized entry".to_string());
                }
                let key = object_to_value_at(key as *mut ShimObject, depth + 1, path)?;
                let value = object_to_value_at(value as *mut ShimObject, depth + 1, path)?;
                converted.push((key, value));
            }
            path.pop();
            Ok(ShimValue::Dict(converted))
        }
        ShimKind::Set(items) | ShimKind::FrozenSet(items) => {
            begin_container(object as usize, "set", depth, path)?;
            let mut converted = Vec::with_capacity(items.len());
            for &item in items {
                if item == 0 {
                    return Err("extension returned a set with an uninitialized item".to_string());
                }
                converted.push(object_to_value_at(
                    item as *mut ShimObject,
                    depth + 1,
                    path,
                )?);
            }
            path.pop();
            Ok(ShimValue::Set(converted))
        }
        ShimKind::Capsule { ptr, name, .. } => {
            // A Hydro capsule stores only the raw pointer; it has no
            // ownership protocol or destructor slot. Retain the handle so the
            // extension's destructor cannot invalidate the pointer while the
            // Hydro value still names it. Capsules that never escape (freed
            // before conversion) still run their destructor normally.
            inc_ref(object);
            Ok(ShimValue::Capsule {
                ptr: *ptr,
                name: name
                    .as_ref()
                    .map(|name| name.to_string_lossy().into_owned()),
            })
        }
        // A Hydro-backed handle keeps the identity of the value it wraps:
        // the contract value shares the same persistent root, so passing it
        // back to Hydro yields the identical tagged value.
        ShimKind::Hydro(handle) => Ok(ShimValue::Hydro(handle.clone())),
        // Every other handle is opaque: callables (C functions, bound
        // methods), containers Hydro has no native counterpart for, exception
        // instances, modules and extension-owned protocol objects all keep
        // their identity and extension semantics through the same handle.
        // Only the internal error sentinel is not a real object.
        ShimKind::NotImplemented => Ok(ShimValue::NotImplemented),
        ShimKind::Ellipsis => Ok(ShimValue::Ellipsis),
        ShimKind::Error => Err("extension returned an error object".to_string()),
        _ => Ok(opaque_object(object, object_type_name(object))),
    }
}

fn begin_container(
    address: usize,
    kind: &str,
    depth: usize,
    path: &mut Vec<usize>,
) -> Result<(), String> {
    if depth >= MAX_CONVERSION_DEPTH {
        return Err(format!(
            "extension returned a {kind} nested more than {MAX_CONVERSION_DEPTH} levels deep"
        ));
    }
    if path.contains(&address) {
        return Err(format!(
            "extension returned a cyclic {kind} object, which cannot be converted"
        ));
    }
    path.push(address);
    Ok(())
}

/// Converts an extension result graph directly into Hydro values, preserving
/// the identity of containers.
///
/// One source object always maps to one Hydro value, so a mutation made
/// through one alias is visible through every other alias: pydantic's
/// `gather_schemas_for_cleaning` returns the schema dictionary both as the
/// root and nested in `definition-ref` entries and mutates the nested copy in
/// place. List and dict cycles survive; Hydro tuples are immutable, so only a
/// cycle running through a tuple is rejected.
fn object_to_hydro(object: *mut ShimObject) -> Result<i64, String> {
    object_to_hydro_with(object, &HashMap::new())
}

/// Converts a result graph, mapping objects that were materialised from a
/// Hydro argument back to that same Hydro value (`origins`). A C function may
/// return a container it received, and the caller then mutates it in place
/// expecting the original Hydro object to change.
fn object_to_hydro_with(
    object: *mut ShimObject,
    origins: &HashMap<usize, i64>,
) -> Result<i64, String> {
    let mut converted: HashMap<usize, i64> = HashMap::new();
    let mut tuple_stack: HashSet<usize> = HashSet::new();
    object_to_hydro_at(object, 0, &mut converted, &mut tuple_stack, origins)
}

fn object_to_hydro_at(
    object: *mut ShimObject,
    depth: usize,
    converted: &mut HashMap<usize, i64>,
    tuple_stack: &mut HashSet<usize>,
    origins: &HashMap<usize, i64>,
) -> Result<i64, String> {
    if object.is_null() {
        return Err("cannot convert a NULL object".to_string());
    }
    if depth >= MAX_CONVERSION_DEPTH {
        return Err(format!(
            "extension returned a value nested more than {MAX_CONVERSION_DEPTH} levels deep"
        ));
    }
    let address = object as usize;
    if let Some(&value) = origins.get(&address) {
        return Ok(value);
    }
    if let Some(&value) = converted.get(&address) {
        return Ok(value);
    }
    if types::is_type_pointer(object) {
        return Ok(opaque_hydro_object(
            object,
            types::type_name(object as *mut TypeObject),
        ));
    }
    let Some(kind) = kind_ref(object) else {
        return Ok(opaque_hydro_object(object, object_type_name(object)));
    };
    match kind {
        ShimKind::None => Ok(crate::value::NONE),
        ShimKind::Bool(flag) => Ok(crate::value::bool_value(*flag)),
        ShimKind::Int(number) => crate::value::encode_int(*number).ok_or_else(|| {
            format!("integer result {number} is outside the supported Hydro int range")
        }),
        ShimKind::Float(number) => Ok(crate::value::float_new(*number)),
        ShimKind::Str { text, .. } => Ok(crate::value::str_alloc(text.clone())),
        ShimKind::Bytes { data, .. } => Ok(crate::value::bytes_new(data.clone())),
        ShimKind::List(items) => {
            // Publish the empty container before converting the items so a
            // back-reference resolves to this same Hydro list.
            let list = crate::value::list_new(Vec::new());
            let roots = crate::heap::root_values(&[list]);
            converted.insert(address, list);
            let outcome = (|| -> Result<(), String> {
                for &item in items {
                    if item == 0 {
                        return Err(
                            "extension returned a list with an uninitialized item".to_string()
                        );
                    }
                    let item = object_to_hydro_at(
                        item as *mut ShimObject,
                        depth + 1,
                        converted,
                        tuple_stack,
                        origins,
                    )?;
                    crate::container::list_append(list, item);
                }
                Ok(())
            })();
            crate::heap::release_roots(roots);
            outcome?;
            Ok(list)
        }
        ShimKind::Tuple(items) => {
            if !tuple_stack.insert(address) {
                return Err(
                    "extension returned a cyclic tuple object, which cannot be converted"
                        .to_string(),
                );
            }
            // Accumulate in a rooted list so every converted item stays
            // reachable while the later ones allocate.
            let accumulator = crate::value::list_new(Vec::new());
            let roots = crate::heap::root_values(&[accumulator]);
            let outcome = (|| -> Result<Vec<i64>, String> {
                for &item in items {
                    if item == 0 {
                        return Err(
                            "extension returned a tuple with an uninitialized item".to_string()
                        );
                    }
                    let item = object_to_hydro_at(
                        item as *mut ShimObject,
                        depth + 1,
                        converted,
                        tuple_stack,
                        origins,
                    )?;
                    crate::container::list_append(accumulator, item);
                }
                Ok(crate::value::as_list(accumulator)
                    .map(|items| items.to_vec())
                    .unwrap_or_default())
            })();
            let tuple = outcome.map(crate::value::tuple_new);
            crate::heap::release_roots(roots);
            tuple_stack.remove(&address);
            let tuple = tuple?;
            converted.insert(address, tuple);
            Ok(tuple)
        }
        ShimKind::Dict(pairs) => {
            let dict = crate::value::dict_new(Vec::new());
            let roots = crate::heap::root_values(&[dict]);
            converted.insert(address, dict);
            let outcome = (|| -> Result<(), String> {
                for &(key, item) in pairs {
                    if key == 0 || item == 0 {
                        return Err(
                            "extension returned a dict with an uninitialized entry".to_string()
                        );
                    }
                    let key = object_to_hydro_at(
                        key as *mut ShimObject,
                        depth + 1,
                        converted,
                        tuple_stack,
                        origins,
                    )?;
                    let item = object_to_hydro_at(
                        item as *mut ShimObject,
                        depth + 1,
                        converted,
                        tuple_stack,
                        origins,
                    )?;
                    crate::container::dict_set(dict, key, item);
                    if crate::exc::pending() {
                        return Err(take_hydro_error(
                            "cannot convert an extension dictionary key",
                        ));
                    }
                }
                Ok(())
            })();
            crate::heap::release_roots(roots);
            outcome?;
            Ok(dict)
        }
        // Hydro has a single set representation (`frozenset` builds a set
        // too), so both C kinds convert to a Hydro set. Sets cannot contain
        // themselves or be nested in a hashable tuple, so no cycle can run
        // through one.
        ShimKind::Set(items) | ShimKind::FrozenSet(items) => {
            let set = crate::value::set_new(Vec::new());
            let roots = crate::heap::root_values(&[set]);
            converted.insert(address, set);
            let outcome = (|| -> Result<(), String> {
                for &item in items {
                    if item == 0 {
                        return Err(
                            "extension returned a set with an uninitialized item".to_string()
                        );
                    }
                    let item = object_to_hydro_at(
                        item as *mut ShimObject,
                        depth + 1,
                        converted,
                        tuple_stack,
                        origins,
                    )?;
                    crate::container::set_add(set, item);
                    if crate::exc::pending() {
                        return Err(take_hydro_error("cannot convert an extension set item"));
                    }
                }
                Ok(())
            })();
            crate::heap::release_roots(roots);
            outcome?;
            Ok(set)
        }
        ShimKind::Capsule { ptr, name, .. } => {
            // The Hydro capsule names the C pointer; keep the source handle
            // (and therefore its destructor) alive for as long as the value
            // can be observed.
            inc_ref(object);
            Ok(crate::value::capsule_new(
                *ptr,
                name.as_ref()
                    .map(|name| name.to_string_lossy().into_owned()),
            ))
        }
        ShimKind::Hydro(handle) => Ok(handle.value()),
        ShimKind::NotImplemented => Ok(crate::value::NOT_IMPLEMENTED),
        ShimKind::Ellipsis => {
            Err("extension returned the Ellipsis singleton, which has no Hydro value".to_string())
        }
        ShimKind::Error => Err("extension returned an error object".to_string()),
        _ => Ok(opaque_hydro_object(object, object_type_name(object))),
    }
}

/// Wraps an opaque extension handle as a Hydro extension object owning one new
/// reference.
fn opaque_hydro_object(object: *mut ShimObject, type_name: String) -> i64 {
    inc_ref(object);
    crate::value::ext_object_new(object, type_name)
}

/// Materialises one Hydro argument as a C handle.
///
/// Container and extension-object identities are preserved through `created`
/// (the same Hydro value yields the same C object, so cycles and aliases in an
/// argument graph survive), and `origins` records the reverse mapping so a
/// result that returns this object converts back to the original Hydro value
/// instead of a copy.
fn hydro_to_object(
    value_arg: i64,
    depth: usize,
    created: &mut HashMap<i64, usize>,
    origins: &mut HashMap<usize, i64>,
) -> Result<*mut ShimObject, String> {
    if value_arg == crate::value::NONE {
        return Ok(value_object(ShimValue::None));
    }
    if value_arg == crate::value::TRUE {
        return Ok(value_object(ShimValue::Bool(true)));
    }
    if value_arg == crate::value::FALSE {
        return Ok(value_object(ShimValue::Bool(false)));
    }
    if value_arg == crate::value::NOT_IMPLEMENTED {
        return Ok(value_object(ShimValue::NotImplemented));
    }
    if crate::value::is_int(value_arg) {
        return Ok(new_int(value_arg >> 3));
    }
    if let Some(number) = crate::value::as_float(value_arg) {
        return Ok(value_object(ShimValue::Float(number)));
    }
    if let Some(text) = crate::value::as_str(value_arg) {
        return Ok(new_str(text.to_string()));
    }
    if let Some(bytes) = crate::value::as_bytes(value_arg) {
        return Ok(new_bytes(bytes.to_vec()));
    }
    if let Some(state) = crate::value::as_ext_object(value_arg) {
        if let Some(&cached) = created.get(&value_arg) {
            let object = cached as *mut ShimObject;
            inc_ref(object);
            return Ok(object);
        }
        let object = state.object;
        inc_ref(object);
        created.insert(value_arg, object as usize);
        origins.insert(object as usize, value_arg);
        return Ok(object);
    }
    if let Some((pointer, name)) = crate::value::as_capsule(value_arg) {
        if let Some(&cached) = created.get(&value_arg) {
            let object = cached as *mut ShimObject;
            inc_ref(object);
            return Ok(object);
        }
        let capsule = value_object(ShimValue::Capsule {
            ptr: pointer,
            name: name.map(str::to_string),
        });
        if capsule.is_null() {
            return Err("MemoryError: cannot create a capsule argument".to_string());
        }
        created.insert(value_arg, capsule as usize);
        origins.insert(capsule as usize, value_arg);
        return Ok(capsule);
    }
    if let Some(items) = crate::value::as_list(value_arg) {
        if let Some(&cached) = created.get(&value_arg) {
            let object = cached as *mut ShimObject;
            inc_ref(object);
            return Ok(object);
        }
        if depth >= MAX_CONVERSION_DEPTH {
            return Err(format!(
                "cannot pass a 'list' object nested more than {MAX_CONVERSION_DEPTH} levels \
                 deep to a C extension function"
            ));
        }
        let list = PyList_New(0);
        if list.is_null() {
            return Err(take_error()
                .unwrap_or_else(|| "MemoryError: cannot create an argument list".to_string()));
        }
        created.insert(value_arg, list as usize);
        origins.insert(list as usize, value_arg);
        for &item in items {
            let object = match hydro_to_object(item, depth + 1, created, origins) {
                Ok(object) => object,
                Err(error) => {
                    dec_ref(list);
                    return Err(error);
                }
            };
            if PyList_Append(list, object) != 0 {
                dec_ref(object);
                dec_ref(list);
                return Err(take_error().unwrap_or_else(|| {
                    "TypeError: cannot append to an argument list".to_string()
                }));
            }
            dec_ref(object);
        }
        return Ok(list);
    }
    if let Some(items) = crate::value::as_tuple(value_arg) {
        if let Some(&cached) = created.get(&value_arg) {
            let object = cached as *mut ShimObject;
            inc_ref(object);
            return Ok(object);
        }
        if depth >= MAX_CONVERSION_DEPTH {
            return Err(format!(
                "cannot pass a 'tuple' object nested more than {MAX_CONVERSION_DEPTH} levels \
                 deep to a C extension function"
            ));
        }
        let tuple = PyTuple_New(items.len() as isize);
        if tuple.is_null() {
            return Err(take_error()
                .unwrap_or_else(|| "MemoryError: cannot create an argument tuple".to_string()));
        }
        created.insert(value_arg, tuple as usize);
        origins.insert(tuple as usize, value_arg);
        for (index, &item) in items.iter().enumerate() {
            let object = match hydro_to_object(item, depth + 1, created, origins) {
                Ok(object) => object,
                Err(error) => {
                    dec_ref(tuple);
                    return Err(error);
                }
            };
            if PyTuple_SetItem(tuple, index as isize, object) != 0 {
                dec_ref(object);
                dec_ref(tuple);
                return Err(take_error()
                    .unwrap_or_else(|| "TypeError: cannot fill an argument tuple".to_string()));
            }
        }
        return Ok(tuple);
    }
    if let Some(pairs) = crate::value::as_dict(value_arg) {
        if let Some(&cached) = created.get(&value_arg) {
            let object = cached as *mut ShimObject;
            inc_ref(object);
            return Ok(object);
        }
        if depth >= MAX_CONVERSION_DEPTH {
            return Err(format!(
                "cannot pass a 'dict' object nested more than {MAX_CONVERSION_DEPTH} levels \
                 deep to a C extension function"
            ));
        }
        let dict = PyDict_New();
        if dict.is_null() {
            return Err(take_error()
                .unwrap_or_else(|| "MemoryError: cannot create an argument dict".to_string()));
        }
        created.insert(value_arg, dict as usize);
        origins.insert(dict as usize, value_arg);
        for &(key, item) in pairs {
            let key = match hydro_to_object(key, depth + 1, created, origins) {
                Ok(object) => object,
                Err(error) => {
                    dec_ref(dict);
                    return Err(error);
                }
            };
            let item = match hydro_to_object(item, depth + 1, created, origins) {
                Ok(object) => object,
                Err(error) => {
                    dec_ref(key);
                    dec_ref(dict);
                    return Err(error);
                }
            };
            if PyDict_SetItem(dict, key, item) != 0 {
                dec_ref(key);
                dec_ref(item);
                dec_ref(dict);
                return Err(take_error()
                    .unwrap_or_else(|| "TypeError: cannot fill an argument dict".to_string()));
            }
            dec_ref(key);
            dec_ref(item);
        }
        return Ok(dict);
    }
    if let Some(items) = crate::value::as_set(value_arg) {
        if let Some(&cached) = created.get(&value_arg) {
            let object = cached as *mut ShimObject;
            inc_ref(object);
            return Ok(object);
        }
        if depth >= MAX_CONVERSION_DEPTH {
            return Err(format!(
                "cannot pass a 'set' object nested more than {MAX_CONVERSION_DEPTH} levels \
                 deep to a C extension function"
            ));
        }
        // Hydro has one set representation, so it crosses as a mutable C set.
        let set = PySet_New(ptr::null_mut());
        if set.is_null() {
            return Err(take_error()
                .unwrap_or_else(|| "MemoryError: cannot create an argument set".to_string()));
        }
        created.insert(value_arg, set as usize);
        origins.insert(set as usize, value_arg);
        for &item in items {
            let object = match hydro_to_object(item, depth + 1, created, origins) {
                Ok(object) => object,
                Err(error) => {
                    dec_ref(set);
                    return Err(error);
                }
            };
            if PySet_Add(set, object) != 0 {
                dec_ref(object);
                dec_ref(set);
                return Err(take_error()
                    .unwrap_or_else(|| "TypeError: cannot add to an argument set".to_string()));
            }
            // The C set owns its own reference to every inserted item.
            dec_ref(object);
        }
        return Ok(set);
    }
    // Classes, instances, functions and modules cross as rooted handles.
    if crate::modules::extension::hydro_handle_supported(value_arg) {
        return Ok(value_object(ShimValue::Hydro(HydroValue::new(value_arg))));
    }
    Err(format!(
        "cannot pass '{}' object to a C extension function",
        crate::value::type_name(value_arg)
    ))
}

/// Formats one component of a complex number the way `complex.__repr__` does:
/// shortest round-tripping digits, with the trailing `.0` of integral values
/// removed.
fn complex_part(value: f64) -> String {
    let text = crate::core::value::format_float(value);
    if value.is_finite() && value.fract() == 0.0 {
        text.strip_suffix(".0").map(str::to_string).unwrap_or(text)
    } else {
        text
    }
}

/// `repr()` of a complex number: `<imag>j` when the real part is positive
/// zero, otherwise `(<real><sign><imag>j)`.
fn complex_repr(real: f64, imag: f64) -> String {
    let imag_text = complex_part(imag.abs());
    if real == 0.0 && !real.is_sign_negative() {
        return format!(
            "{}{}j",
            if imag.is_sign_negative() { "-" } else { "" },
            imag_text
        );
    }
    format!(
        "({}{}{}j)",
        complex_part(real),
        if imag.is_sign_negative() { "-" } else { "+" },
        imag_text
    )
}

/// `repr()` of a set or frozenset: `{...}`, `set()`/`frozenset()` when empty
/// and `frozenset({...})` for the immutable form.
fn set_repr(items: &[usize], frozen: bool, depth: usize) -> String {
    if items.is_empty() {
        return if frozen { "frozenset()" } else { "set()" }.to_string();
    }
    let rendered: Vec<String> = items
        .iter()
        .map(|item| shim_repr_at(*item as *mut ShimObject, depth + 1))
        .collect();
    let body = format!("{{{}}}", rendered.join(", "));
    if frozen {
        format!("frozenset({body})")
    } else {
        body
    }
}

/// Renders a handle for `PyErr_SetObject`.
fn shim_repr(object: *mut ShimObject) -> String {
    shim_repr_at(object, 0)
}

fn shim_repr_at(object: *mut ShimObject, depth: usize) -> String {
    if object.is_null() {
        return "<NULL>".to_string();
    }
    if types::is_type_pointer(object) {
        return format!("<class '{}'>", types::type_name(object as *mut TypeObject));
    }
    if depth >= MAX_CONVERSION_DEPTH {
        return "...".to_string();
    }
    let Some(kind) = kind_ref(object) else {
        return format!(
            "<{} object at 0x{:x}>",
            object_type_name(object),
            object as usize
        );
    };
    match kind {
        ShimKind::None => "None".to_string(),
        ShimKind::Bool(flag) => (if *flag { "True" } else { "False" }).to_string(),
        ShimKind::Int(number) => number.to_string(),
        ShimKind::Float(number) => format!("{number:?}"),
        ShimKind::Str { text, .. } => format!("'{}'", text.escape_debug()),
        ShimKind::Bytes { data, .. } => {
            let mut rendered = String::from("b'");
            for byte in data {
                if byte.is_ascii_graphic() || *byte == b' ' {
                    rendered.push(*byte as char);
                } else {
                    rendered.push_str(&format!("\\x{byte:02x}"));
                }
            }
            rendered.push('\'');
            rendered
        }
        ShimKind::ByteArray { data, .. } => {
            let mut rendered = String::from("bytearray(b'");
            for byte in data {
                if byte.is_ascii_graphic() || *byte == b' ' {
                    rendered.push(*byte as char);
                } else {
                    rendered.push_str(&format!("\\x{byte:02x}"));
                }
            }
            rendered.push_str("')");
            rendered
        }
        ShimKind::Set(items) => set_repr(items, false, depth),
        ShimKind::FrozenSet(items) => set_repr(items, true, depth),
        ShimKind::Complex { real, imag } => complex_repr(*real, *imag),
        ShimKind::Exception { exc_type, args, .. } => exception_repr(*exc_type, args, depth),
        ShimKind::RuntimeFunction { name, .. } => {
            format!("<built-in function {name}>")
        }
        ShimKind::List(items) => {
            let items: Vec<String> = items
                .iter()
                .map(|item| shim_repr_at(*item as *mut ShimObject, depth + 1))
                .collect();
            format!("[{}]", items.join(", "))
        }
        ShimKind::Tuple(items) => {
            let items: Vec<String> = items
                .iter()
                .map(|item| shim_repr_at(*item as *mut ShimObject, depth + 1))
                .collect();
            match items.len() {
                1 => format!("({},)", items[0]),
                _ => format!("({})", items.join(", ")),
            }
        }
        ShimKind::Dict(pairs) => {
            let pairs: Vec<String> = pairs
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{}: {}",
                        shim_repr_at(*key as *mut ShimObject, depth + 1),
                        shim_repr_at(*value as *mut ShimObject, depth + 1)
                    )
                })
                .collect();
            format!("{{{}}}", pairs.join(", "))
        }
        ShimKind::Capsule { name, .. } => match name {
            Some(name) => format!("<capsule object \"{}\">", name.to_string_lossy()),
            None => "<capsule object NULL>".to_string(),
        },
        ShimKind::CFunction { method, .. } => {
            let definition = *method as *mut PyMethodDefMirror;
            let name = if definition.is_null() {
                "<unknown>".to_string()
            } else {
                // SAFETY: the method table belongs to the extension and stays
                // loaded for the process lifetime.
                read_c_string(unsafe { (*definition).ml_name })
            };
            format!("<built-in function {name}>")
        }
        ShimKind::Method {
            function,
            self_object,
        } => {
            let function_text = shim_repr_at(*function as *mut ShimObject, depth + 1);
            if *self_object == 0 {
                function_text
            } else {
                format!(
                    "<bound method {} of {}>",
                    function_text
                        .trim_start_matches("<built-in function ")
                        .trim_end_matches('>'),
                    shim_repr_at(*self_object as *mut ShimObject, depth + 1)
                )
            }
        }
        ShimKind::Slice { start, stop, step } => format!(
            "slice({}, {}, {})",
            shim_repr_at(*start as *mut ShimObject, depth + 1),
            shim_repr_at(*stop as *mut ShimObject, depth + 1),
            shim_repr_at(*step as *mut ShimObject, depth + 1)
        ),
        ShimKind::DictProxy { dict } => format!(
            "mappingproxy({})",
            shim_repr_at(*dict as *mut ShimObject, depth + 1)
        ),
        ShimKind::GenericAlias { origin, args } => {
            let origin_text = if types::is_type_pointer(*origin as *mut ShimObject) {
                types::type_name(*origin as *mut TypeObject)
            } else {
                shim_repr_at(*origin as *mut ShimObject, depth + 1)
            };
            let arguments = match kind_ref(*args as *mut ShimObject) {
                Some(ShimKind::Tuple(items)) => items
                    .iter()
                    .map(|item| shim_repr_at(*item as *mut ShimObject, depth + 1))
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => shim_repr_at(*args as *mut ShimObject, depth + 1),
            };
            format!("{origin_text}[{arguments}]")
        }
        ShimKind::MemoryView { buffer } => {
            let pointer = buffer.lock().buf as usize;
            format!("<memory at 0x{pointer:x}>")
        }
        ShimKind::SeqIter { sequence, .. } => format!(
            "<iterator object at 0x{:x} over {}>",
            object as usize,
            object_type_name(*sequence as *mut ShimObject)
        ),
        ShimKind::ContextVar { name, .. } => {
            format!("<ContextVar name='{name}' at 0x{:x}>", object as usize)
        }
        ShimKind::ContextVarToken { var, .. } => format!(
            "<Token var={} at 0x{:x}>",
            shim_repr_at(*var as *mut ShimObject, depth + 1),
            object as usize
        ),
        ShimKind::NotImplemented => "NotImplemented".to_string(),
        ShimKind::Ellipsis => "Ellipsis".to_string(),
        ShimKind::Module(_) => "<module>".to_string(),
        ShimKind::ModuleDef { .. } => "<module definition>".to_string(),
        // Hydro renders its own objects; a failing `__repr__` leaves a pending
        // Hydro exception, which this diagnostic path swallows in favour of
        // the conventional address rendering (the caller that can propagate
        // the error handles it in `PyObject_Repr`).
        ShimKind::Hydro(handle) => match hydro_repr_text(handle.value()) {
            Ok(text) => text,
            Err(_) => format!("<{} object at 0x{:x}>", handle.type_name(), object as usize),
        },
        ShimKind::Error => "<error>".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Thread-local error state
// ---------------------------------------------------------------------------

std::thread_local! {
    /// Pending error set by an extension or by a failing conversion.
    ///
    /// `message` keeps CPython's `"<Type>: <message>"` spelling so the loader
    /// can map it back to a Hydro exception tag. `exception` holds an owning
    /// reference to an exception instance when one was supplied through
    /// `PyErr_SetRaisedException`; almost every setter stores only the message
    /// and the instance is materialised on demand by
    /// `PyErr_GetRaisedException`.
    static CALL_ERROR: RefCell<RaisedError> = const { RefCell::new(RaisedError::EMPTY) };

    /// Exception instance that accompanied the most recently consumed error
    /// message, keyed by that message.
    ///
    /// The C error state owns one reference to a raised exception instance;
    /// [`take_error`] moves it here so the Hydro raise path can preserve the
    /// original object instead of only its text. The next `take_error`
    /// replaces (and releases) it.
    static TAKEN_ERROR_EXCEPTION: RefCell<Option<(usize, String)>> =
        const { RefCell::new(None) };
}

/// Thread-local exception state shared by the error API.
struct RaisedError {
    message: Option<String>,
    /// Owning reference, released when the state is replaced or cleared.
    exception: Option<usize>,
}

impl RaisedError {
    const EMPTY: Self = Self {
        message: None,
        exception: None,
    };
}

/// Replaces the error state, releasing the previous exception reference.
fn replace_error_state(message: Option<String>, exception: Option<usize>) {
    let previous = CALL_ERROR.with(|slot| {
        let mut state = slot.borrow_mut();
        let previous = std::mem::replace(&mut *state, RaisedError { message, exception });
        previous.exception
    });
    if let Some(previous) = previous {
        dec_ref(previous as *mut ShimObject);
    }
}

/// Records a pending error, using CPython's `"<Type>: <message>"` spelling so
/// the loader can map it back to a Hydro exception tag.
pub(crate) fn set_error(message: impl Into<String>) {
    replace_error_state(Some(message.into()), None);
}

fn set_type_error(message: impl Into<String>) {
    set_error(format!("TypeError: {}", message.into()));
}

/// Records a conversion failure reported by `PyArg_ParseTuple`.
#[no_mangle]
pub extern "C" fn hydro_capi_set_type_error(message: *const c_char) {
    if message.is_null() {
        set_type_error("PyArg_ParseTuple failed");
        return;
    }
    // SAFETY: `parse.c` passes a valid NUL-terminated string literal.
    let text = unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned();
    set_type_error(text);
}

fn clear_error() {
    replace_error_state(None, None);
    let previous = TAKEN_ERROR_EXCEPTION.with(|slot| slot.replace(None));
    if let Some((previous, _)) = previous {
        dec_ref(previous as *mut ShimObject);
    }
}

pub(crate) fn error_pending() -> bool {
    CALL_ERROR.with(|slot| slot.borrow().message.is_some())
}

pub(crate) fn take_error() -> Option<String> {
    let (message, exception) = CALL_ERROR.with(|slot| {
        let mut state = slot.borrow_mut();
        (state.message.take(), state.exception.take())
    });
    replace_error_state(None, None);
    // Hand the exception reference to the raise path; a later `take_error`
    // releases it. Nothing else observes it, so a caller that only wants the
    // text simply leaves it for the next error. The message keys the entry so
    // a stale exception cannot attach to an unrelated raise.
    let stashed = match (message.as_ref(), exception) {
        (Some(message), Some(exception)) => Some((exception, message.clone())),
        (_, exception) => {
            if let Some(exception) = exception {
                dec_ref(exception as *mut ShimObject);
            }
            None
        }
    };
    let previous = TAKEN_ERROR_EXCEPTION.with(|slot| slot.replace(stashed));
    if let Some((previous, _)) = previous {
        dec_ref(previous as *mut ShimObject);
    }
    message
}

/// Takes the exception instance recorded with the consumed error `message`, if
/// the C error state carried one, as an owned reference.
///
/// `PyErr_Restore` wraps an extension-owned exception instance it cannot
/// classify as a shim exception into a shim exception whose sole argument is
/// the original instance; that wrapper is unwrapped here so the Hydro raise
/// path preserves the extension object itself. Returns `None` for plain shim
/// exceptions, whose Hydro payload already renders correctly.
pub(crate) fn take_error_exception(message: &str) -> Option<*mut ShimObject> {
    let taken = TAKEN_ERROR_EXCEPTION.with(|slot| slot.take())?;
    let (object, recorded) = taken;
    if recorded != message {
        dec_ref(object as *mut ShimObject);
        return None;
    }
    let object = object as *mut ShimObject;
    // An opaque extension instance is already the original object.
    if kind_ref(object).is_none() {
        if object_is_exception(object) {
            return Some(object);
        }
        dec_ref(object);
        return None;
    }
    // `PyErr_Restore`/`PyErr_NormalizeException` wrap an extension instance
    // they cannot classify into a shim exception of that same class whose sole
    // argument is the original instance.
    let inner = match kind_ref(object) {
        Some(ShimKind::Exception { exc_type, args, .. }) => match args.as_slice() {
            [argument]
                if kind_ref(*argument as *mut ShimObject).is_none()
                    && type_of(*argument as *mut ShimObject) as usize == *exc_type =>
            {
                *argument as *mut ShimObject
            }
            _ => ptr::null_mut(),
        },
        _ => ptr::null_mut(),
    };
    if inner.is_null() || !object_is_exception(inner) {
        dec_ref(object);
        return None;
    }
    inc_ref(inner);
    dec_ref(object);
    Some(inner)
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

/// Reads a NUL-terminated C string, replacing invalid UTF-8 lossily.
pub(crate) fn read_c_string(pointer: *const c_char) -> String {
    if pointer.is_null() {
        return String::new();
    }
    // SAFETY: callers only pass NUL-terminated strings (CPython C API contract).
    unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned()
}

fn as_int(object: *mut ShimObject) -> Result<i64, String> {
    match kind_ref(object) {
        Some(ShimKind::Int(number)) => Ok(*number),
        Some(ShimKind::Bool(flag)) => Ok(*flag as i64),
        _ => Err("an integer is required".to_string()),
    }
}

fn as_float(object: *mut ShimObject) -> Result<f64, String> {
    match kind_ref(object) {
        Some(ShimKind::Float(number)) => Ok(*number),
        Some(ShimKind::Int(number)) => Ok(*number as f64),
        Some(ShimKind::Bool(flag)) => Ok(if *flag { 1.0 } else { 0.0 }),
        _ => Err("a float is required".to_string()),
    }
}

fn as_predicate(object: *mut ShimObject) -> Result<bool, String> {
    match kind_ref(object) {
        Some(ShimKind::None) => Ok(false),
        Some(ShimKind::Bool(flag)) => Ok(*flag),
        Some(ShimKind::Int(number)) => Ok(*number != 0),
        Some(ShimKind::Float(number)) => Ok(*number != 0.0),
        Some(ShimKind::Str { text, .. }) => Ok(!text.is_empty()),
        Some(ShimKind::Bytes { data, .. }) => Ok(!data.is_empty()),
        Some(ShimKind::List(items)) => Ok(!items.is_empty()),
        Some(ShimKind::Tuple(items)) => Ok(!items.is_empty()),
        Some(ShimKind::Dict(pairs)) => Ok(!pairs.is_empty()),
        Some(_) => Ok(true),
        None => Err("cannot convert a NULL object to a predicate".to_string()),
    }
}

fn list_slots(object: *mut ShimObject) -> Result<&'static [usize], String> {
    match kind_ref(object) {
        Some(ShimKind::List(items)) => Ok(items.as_slice()),
        Some(_) => Err("TypeError: expected a list object".to_string()),
        None => Err("TypeError: expected a list object, got NULL".to_string()),
    }
}

fn list_slots_mut(object: *mut ShimObject) -> Result<&'static mut Vec<usize>, String> {
    if object.is_null() {
        return Err("TypeError: expected a list object, got NULL".to_string());
    }
    // SAFETY: shim objects are heap-stable; the caller holds a reference and
    // the GIL contract forbids concurrent mutation.
    match unsafe { kind_mut(object) } {
        ShimKind::List(items) => Ok(items),
        _ => Err("TypeError: expected a list object".to_string()),
    }
}

fn tuple_slots(object: *mut ShimObject) -> Result<&'static [usize], String> {
    match kind_ref(object) {
        Some(ShimKind::Tuple(items)) => Ok(items.as_slice()),
        Some(_) => Err("TypeError: expected a tuple object".to_string()),
        None => Err("TypeError: expected a tuple object, got NULL".to_string()),
    }
}

fn tuple_slots_mut(object: *mut ShimObject) -> Result<&'static mut [usize], String> {
    if object.is_null() {
        return Err("TypeError: expected a tuple object, got NULL".to_string());
    }
    // SAFETY: shim objects are heap-stable; the caller holds a reference and
    // the GIL contract forbids concurrent mutation.
    match unsafe { kind_mut(object) } {
        ShimKind::Tuple(items) => {
            let length = items.len();
            // SAFETY: the inline array owns `ob_size` pointer slots in the
            // live allocation (zero before `PyTuple_SetItem` fills them).
            Ok(unsafe {
                let pointer =
                    (object as *mut u8).add(tuple_layout::TUPLE_ITEMS_OFFSET) as *mut usize;
                std::slice::from_raw_parts_mut(pointer, length)
            })
        }
        _ => Err("TypeError: expected a tuple object".to_string()),
    }
}

fn dict_slots(object: *mut ShimObject) -> Result<&'static [(usize, usize)], String> {
    match kind_ref(object) {
        Some(ShimKind::Dict(pairs)) => Ok(pairs.as_slice()),
        Some(_) => Err("TypeError: expected a dict object".to_string()),
        None => Err("TypeError: expected a dict object, got NULL".to_string()),
    }
}

fn dict_slots_mut(object: *mut ShimObject) -> Result<&'static mut Vec<(usize, usize)>, String> {
    if object.is_null() {
        return Err("TypeError: expected a dict object, got NULL".to_string());
    }
    // SAFETY: see `list_slots_mut`.
    match unsafe { kind_mut(object) } {
        ShimKind::Dict(pairs) => Ok(pairs),
        _ => Err("TypeError: expected a dict object".to_string()),
    }
}

/// Borrows a bytes object's content.
pub(crate) fn bytes_data(object: *mut ShimObject) -> Result<&'static [u8], String> {
    match kind_ref(object) {
        Some(ShimKind::Bytes { data, .. }) => Ok(data.as_slice()),
        Some(_) => Err("TypeError: expected a bytes object".to_string()),
        None => Err("TypeError: expected a bytes object, got NULL".to_string()),
    }
}

pub(crate) fn bytes_c_data(object: *mut ShimObject) -> Result<&'static [u8], String> {
    match kind_ref(object) {
        Some(ShimKind::Bytes { c_data, .. }) => Ok(c_data.as_slice()),
        Some(_) => Err("TypeError: expected a bytes object".to_string()),
        None => Err("TypeError: expected a bytes object, got NULL".to_string()),
    }
}

/// Structural equality for dict keys.
fn key_equal(left: *mut ShimObject, right: *mut ShimObject) -> bool {
    if left == right {
        return true;
    }
    let (Some(left), Some(right)) = (kind_ref(left), kind_ref(right)) else {
        return false;
    };
    match (left, right) {
        (ShimKind::None, ShimKind::None) => true,
        (ShimKind::Bool(a), ShimKind::Bool(b)) => a == b,
        (ShimKind::Int(a), ShimKind::Int(b)) => a == b,
        (ShimKind::Float(a), ShimKind::Float(b)) => a == b,
        (ShimKind::Bool(a), ShimKind::Int(b)) | (ShimKind::Int(b), ShimKind::Bool(a)) => {
            *a as i64 == *b
        }
        (ShimKind::Bool(a), ShimKind::Float(b)) | (ShimKind::Float(b), ShimKind::Bool(a)) => {
            (if *a { 1.0 } else { 0.0 }) == *b
        }
        (ShimKind::Int(a), ShimKind::Float(b)) | (ShimKind::Float(b), ShimKind::Int(a)) => {
            *a as f64 == *b
        }
        (ShimKind::Str { text: a, .. }, ShimKind::Str { text: b, .. }) => a == b,
        (ShimKind::Bytes { data: a, .. }, ShimKind::Bytes { data: b, .. }) => a == b,
        (ShimKind::ByteArray { data: a, .. }, ShimKind::ByteArray { data: b, .. }) => a == b,
        (ShimKind::Complex { real: ar, imag: ai }, ShimKind::Complex { real: br, imag: bi }) => {
            ar == br && ai == bi
        }
        (ShimKind::Tuple(a), ShimKind::Tuple(b)) => sequence_equal(a, b),
        (ShimKind::List(a), ShimKind::List(b)) => sequence_equal(a, b),
        (ShimKind::Set(a), ShimKind::Set(b))
        | (ShimKind::Set(a), ShimKind::FrozenSet(b))
        | (ShimKind::FrozenSet(a), ShimKind::Set(b))
        | (ShimKind::FrozenSet(a), ShimKind::FrozenSet(b)) => set_equal(a, b),
        // Exceptions compare equal when they are the same object; CPython's
        // default `BaseException` equality is identity, handled above.
        _ => false,
    }
}

/// Element-wise equality of two handle sequences.
fn sequence_equal(left: &[usize], right: &[usize]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(&left, &right)| key_equal(left as *mut ShimObject, right as *mut ShimObject))
}

/// Set equality: same cardinality and every element of `left` in `right`.
fn set_equal(left: &[usize], right: &[usize]) -> bool {
    left.len() == right.len()
        && left.iter().all(|&item| {
            let item = item as *mut ShimObject;
            right
                .iter()
                .any(|&other| key_equal(item, other as *mut ShimObject))
        })
}

/// Whether a handle can be stored in a set or used as a dict key.
fn hashable(object: *mut ShimObject) -> bool {
    !matches!(
        kind_ref(object),
        Some(ShimKind::List(_))
            | Some(ShimKind::Dict(_))
            | Some(ShimKind::Set(_))
            | Some(ShimKind::ByteArray { .. })
    )
}

fn dict_position(pairs: &[(usize, usize)], key: *mut ShimObject) -> Option<usize> {
    pairs
        .iter()
        .position(|&(existing, _)| key_equal(existing as *mut ShimObject, key))
}

/// Stores a key/value pair, taking a new reference to both.
/// Republishes a dict's entry count into the `ma_used` header word.
pub(crate) fn sync_dict_prefix(dict: *mut ShimObject) {
    let Ok(pairs) = dict_slots(dict) else {
        return;
    };
    // SAFETY: `dict` is a live header-bearing handle (validated by
    // `dict_slots`); `ob_size` is the CPython `ma_used` slot.
    unsafe {
        (*dict)
            .ob_size
            .store(pairs.len() as isize, Ordering::Relaxed);
    }
}

fn dict_set(
    dict: *mut ShimObject,
    key: *mut ShimObject,
    value: *mut ShimObject,
) -> Result<(), String> {
    let position = dict_position(dict_slots(dict)?, key);
    match position {
        Some(index) => {
            let previous = {
                let pairs = dict_slots_mut(dict)?;
                let previous = pairs[index].1;
                pairs[index].1 = value as usize;
                previous
            };
            inc_ref(value);
            if previous != 0 {
                dec_ref(previous as *mut ShimObject);
            }
            sync_dict_prefix(dict);
            Ok(())
        }
        None => {
            inc_ref(key);
            inc_ref(value);
            let pairs = dict_slots_mut(dict)?;
            if pairs.try_reserve(1).is_err() {
                dec_ref(key);
                dec_ref(value);
                return Err("MemoryError: cannot grow dict".to_string());
            }
            pairs.push((key as usize, value as usize));
            sync_dict_prefix(dict);
            Ok(())
        }
    }
}

fn as_c_string(object: *mut ShimObject) -> Result<*const c_char, String> {
    match kind_ref(object) {
        Some(ShimKind::Str {
            c_text: Some(text), ..
        }) => Ok(text.as_ptr()),
        Some(ShimKind::Str { c_text: None, .. }) => {
            Err("string contains an embedded null character".to_string())
        }
        Some(_) => Err("a string is required".to_string()),
        None => Err("cannot read a string from a NULL object".to_string()),
    }
}

/// Extracts the string payload of a string handle, for internal lookups.
fn as_string(object: *mut ShimObject) -> Option<String> {
    if let Some(ShimKind::Str { text, .. }) = kind_ref(object) {
        Some(text.clone())
    } else {
        None
    }
}

fn store_argument<T>(out: *mut T, value: T, format_character: &str) -> c_int {
    if out.is_null() {
        set_type_error(format!("NULL output pointer for '{format_character}'"));
        return 0;
    }
    // SAFETY: the caller of `PyArg_ParseTuple` passed a pointer to a writable
    // `T` for exactly this format character.
    unsafe { out.write(value) };
    1
}

// ---------------------------------------------------------------------------
// Module registry
// ---------------------------------------------------------------------------

/// Parsed, owned copy of one `PyMethodDef` entry.
#[derive(Clone, Debug)]
pub(crate) struct MethodSpec {
    pub(crate) name: String,
    pub(crate) flags: c_int,
    pub(crate) function: usize,
    /// Address of the original `PyMethodDef`, for `PyCFunction_NewEx`.
    pub(crate) definition: usize,
}

struct ModuleEntry {
    name: String,
    /// NUL-terminated spelling returned by `PyModule_GetName`.
    c_name: CString,
    /// NUL-terminated spelling returned by `PyModule_GetFilename`.
    filename: CString,
    /// The extension-owned `PyModuleDef`, or 0 for a module created by
    /// `PyImport_AddModule`/`PyModule_NewObject`.
    definition: usize,
    /// Per-module state block (`m_size` bytes), or 0.
    state: usize,
    /// Module `__dict__`, created lazily.
    dict: usize,
    #[allow(dead_code)]
    handle: usize,
    /// Module objects are owned by the registry (the initializer's new
    /// reference is transferred here).
    module_object: usize,
    attrs: HashMap<String, ShimAttr>,
    methods: Vec<MethodSpec>,
    /// Name of the Hydro native/user module backing this object, when it was
    /// resolved from the runtime registry instead of a loaded DSO.
    runtime: Option<String>,
}

struct Registry {
    next_id: u64,
    modules: HashMap<u64, ModuleEntry>,
    by_path: HashMap<String, u64>,
    by_name: HashMap<String, u64>,
    /// `sys.modules`-equivalent dict, created lazily.
    module_dict: usize,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            next_id: 1,
            modules: HashMap::new(),
            by_path: HashMap::new(),
            by_name: HashMap::new(),
            module_dict: 0,
        }
    }
}

fn registry_lock() -> MutexGuard<'static, Registry> {
    static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

fn allocate_module_id() -> u64 {
    let mut registry = registry_lock();
    let id = registry.next_id;
    registry.next_id = id.checked_add(1).unwrap_or(1);
    id
}

/// Reads a NULL-terminated `PyMethodDef` table.
fn read_method_table(
    methods: *mut PyMethodDefMirror,
    owner: &str,
) -> Result<Vec<MethodSpec>, String> {
    let mut entries = Vec::new();
    if methods.is_null() {
        return Ok(entries);
    }
    let mut cursor = methods;
    loop {
        // SAFETY: `methods` points at a NULL-terminated array of PyMethodDef.
        let method = unsafe { &*cursor };
        if method.ml_name.is_null() {
            break;
        }
        if entries.len() >= MAX_METHODS {
            return Err(format!("{owner} defines more than {MAX_METHODS} methods"));
        }
        let method_name = read_c_string(method.ml_name);
        if method.ml_meth.is_null() {
            return Err(format!(
                "{owner} method '{method_name}' has a NULL implementation"
            ));
        }
        entries.push(MethodSpec {
            name: method_name,
            flags: method.ml_flags,
            function: method.ml_meth as usize,
            definition: cursor as usize,
        });
        // SAFETY: advancing one entry inside the NULL-terminated table.
        cursor = unsafe { cursor.add(1) };
    }
    Ok(entries)
}

fn read_module_definition(
    definition: *mut PyModuleDefMirror,
) -> Result<(String, Vec<MethodSpec>), String> {
    if definition.is_null() {
        return Err("extension module definition is NULL".to_string());
    }
    // SAFETY: the extension owns a live PyModuleDef for at least the duration
    // of its PyInit call; the mirror matches CPython 3.14 x86_64.
    let definition = unsafe { &*definition };
    if definition.m_name.is_null() {
        return Err("extension module definition has no name".to_string());
    }
    let name = read_c_string(definition.m_name);
    let methods = read_method_table(definition.m_methods, &format!("extension module '{name}'"))?;
    Ok((name, methods))
}

fn create_module_from_definition(
    definition: *mut PyModuleDefMirror,
) -> Result<(u64, *mut ShimObject), String> {
    let (name, methods) = read_module_definition(definition)?;
    // SAFETY: the definition is live for the duration of module creation.
    let (doc, size) = unsafe {
        (
            if (*definition).m_doc.is_null() {
                None
            } else {
                Some(read_c_string((*definition).m_doc))
            },
            (*definition).m_size,
        )
    };
    let id = allocate_module_id();
    let module_object = make_immortal(new_object(ShimKind::Module(id)));

    let mut attrs = HashMap::with_capacity(methods.len() + 4);
    for (index, method) in methods.iter().enumerate() {
        attrs.insert(
            method.name.clone(),
            ShimAttr::Callable(ShimCallable {
                module: id,
                method: index as u32,
            }),
        );
    }
    attrs.insert(
        "__name__".to_string(),
        ShimAttr::Value(ShimValue::Str(name.clone())),
    );
    attrs.insert(
        "__file__".to_string(),
        ShimAttr::Value(ShimValue::Str(name.clone())),
    );
    attrs.insert(
        "__package__".to_string(),
        ShimAttr::Value(ShimValue::Str(String::new())),
    );
    attrs.insert(
        "__doc__".to_string(),
        ShimAttr::Value(match doc {
            Some(doc) => ShimValue::Str(doc),
            None => ShimValue::None,
        }),
    );

    let state = if size > 0 {
        // SAFETY: `malloc` returns zeroed storage after the memset below, or
        // NULL on failure (reported lazily by `PyModule_GetState`).
        let memory = unsafe { libc::calloc(1, size as usize) };
        memory as usize
    } else {
        0
    };

    let c_name = CString::new(name.clone())
        .map_err(|_| "extension module name contains a NUL".to_string())?;
    let filename = c_name.clone();

    let mut registry = registry_lock();
    registry.by_name.insert(name.clone(), id);
    registry.module_dict_invalidate(id, &name, module_object as usize);
    registry.modules.insert(
        id,
        ModuleEntry {
            name,
            c_name,
            filename,
            definition: definition as usize,
            state,
            dict: 0,
            handle: 0,
            module_object: module_object as usize,
            attrs,
            methods,
            runtime: None,
        },
    );
    Ok((id, module_object))
}

impl Registry {
    /// Adds a freshly created module to the cached `sys.modules` dict when it
    /// already exists.
    fn module_dict_invalidate(&mut self, _id: u64, name: &str, module: usize) {
        if self.module_dict == 0 {
            return;
        }
        let key = new_str(name.to_string());
        let status = PyDict_SetItem(
            self.module_dict as *mut ShimObject,
            key,
            module as *mut ShimObject,
        );
        dec_ref(key);
        if status != 0 {
            clear_error();
        }
    }
}

fn definition_name(definition: &PyModuleDefMirror) -> String {
    if definition.m_name.is_null() {
        "<unnamed>".to_string()
    } else {
        read_c_string(definition.m_name)
    }
}

type ModuleExec = unsafe extern "C" fn(*mut ShimObject) -> c_int;

/// Runs the `Py_mod_exec` callbacks declared by a multi-phase definition.
fn run_module_slots(
    definition: *mut PyModuleDefMirror,
    module: *mut ShimObject,
) -> Result<(), String> {
    if definition.is_null() {
        return Err("PyModuleDef_Init: module definition is NULL".to_string());
    }
    // SAFETY: the extension owns a live PyModuleDef for at least the duration
    // of its initialisation.
    let definition = unsafe { &*definition };
    if definition.m_slots.is_null() {
        return Ok(());
    }
    let name = definition_name(definition);
    let mut cursor = definition.m_slots;
    for index in 0..MAX_MODULE_SLOTS {
        // SAFETY: `m_slots` is NULL-terminated.
        let slot = unsafe { &*cursor };
        if slot.slot == 0 {
            return Ok(());
        }
        match slot.slot {
            PY_MOD_EXEC => {
                if slot.value.is_null() {
                    return Err(format!(
                        "module definition '{name}' has a NULL Py_mod_exec callback"
                    ));
                }
                // SAFETY: a Py_mod_exec value is `int (*)(PyObject *)`.
                let callback: ModuleExec = unsafe { std::mem::transmute(slot.value) };
                clear_error();
                // SAFETY: the module is live and the callback is owned by the
                // loaded extension.
                let status = {
                    let _gripper = GilGuard::new();
                    unsafe { callback(module) }
                };
                if status != 0 {
                    return Err(take_error().unwrap_or_else(|| {
                        format!(
                            "Py_mod_exec callback {index} of module definition '{name}' failed \
                             with status {status}"
                        )
                    }));
                }
                clear_error();
            }
            PY_MOD_MULTIPLE_INTERPRETERS => {
                let value = slot.value as isize;
                if !(0..=PY_MOD_MULTIPLE_INTERPRETERS_PER_INTERPRETER_GIL_SUPPORTED as isize)
                    .contains(&value)
                {
                    return Err(format!(
                        "module definition '{name}' has an invalid \
                         Py_mod_multiple_interpreters value {value}"
                    ));
                }
            }
            PY_MOD_GIL => {
                let value = slot.value as isize;
                if !(0..=PY_MOD_GIL_NOT_USED as isize).contains(&value) {
                    return Err(format!(
                        "module definition '{name}' has an invalid Py_mod_gil value {value}"
                    ));
                }
            }
            PY_MOD_CREATE => {
                return Err(format!(
                    "module definition '{name}' uses Py_mod_create, which is not supported"
                ));
            }
            other => {
                return Err(format!(
                    "module definition '{name}' uses unsupported Py_mod slot {other}"
                ));
            }
        }
        // SAFETY: advancing one entry inside the NULL-terminated table.
        cursor = unsafe { cursor.add(1) };
    }
    Err(format!(
        "module definition '{name}' has more than {MAX_MODULE_SLOTS} slots without a zero \
         terminator"
    ))
}

fn module_id(object: *mut ShimObject) -> Result<u64, String> {
    match kind_ref(object) {
        Some(ShimKind::Module(id)) => Ok(*id),
        _ => Err("object is not a module".to_string()),
    }
}

/// Builds a new reference for a module attribute, for its `__dict__`.
fn attribute_handle(entry: &ModuleEntry, attribute: &ShimAttr) -> Option<*mut ShimObject> {
    match attribute {
        ShimAttr::Value(value) => Some(value_object(value.clone())),
        ShimAttr::Type(address) => {
            let pointer = *address as *mut ShimObject;
            inc_ref(pointer);
            Some(pointer)
        }
        ShimAttr::Callable(callable) => {
            let definition = entry
                .methods
                .get(callable.method as usize)
                .map(|spec| spec.definition)
                .unwrap_or(0);
            if definition == 0 {
                return None;
            }
            let result = PyCFunction_NewEx(
                definition as *mut PyMethodDefMirror,
                entry.module_object as *mut ShimObject,
                ptr::null_mut(),
            );
            if result.is_null() {
                clear_error();
                None
            } else {
                Some(result)
            }
        }
    }
}

fn add_attribute(object: *mut ShimObject, name: String, attribute: ShimAttr) -> Result<(), String> {
    let id = module_id(object)?;
    let mut registry = registry_lock();
    let entry = registry
        .modules
        .get_mut(&id)
        .ok_or_else(|| format!("unknown extension module {id}"))?;
    entry.attrs.insert(name.clone(), attribute);
    if entry.dict != 0 {
        let snapshot = entry.attrs.get(&name).cloned();
        let handle = snapshot.and_then(|attribute| attribute_handle(entry, &attribute));
        if let Some(handle) = handle {
            let key = new_str(name);
            let status = PyDict_SetItem(entry.dict as *mut ShimObject, key, handle);
            dec_ref(key);
            dec_ref(handle);
            if status != 0 {
                return Err(take_error().unwrap_or_else(|| {
                    "AttributeError: cannot update the module dictionary".into()
                }));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Loader contract
// ---------------------------------------------------------------------------

/// Loads an extension shared object and runs its `PyInit_<init_name>`.
pub(crate) fn load_extension(path: &str, init_name: &str) -> Result<u64, String> {
    let cached = registry_lock().by_path.get(path).copied();
    if let Some(module) = cached {
        return Ok(module);
    }

    let path_c = CString::new(path)
        .map_err(|_| "extension path contains an embedded null character".to_string())?;
    let symbol = CString::new(format!("PyInit_{init_name}"))
        .map_err(|_| "extension init name contains an embedded null character".to_string())?;

    // SAFETY: `path_c` is a NUL-terminated string that outlives the call.
    let handle = unsafe { libc::dlopen(path_c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return Err(dl_error());
    }

    // SAFETY: `handle` is live and `symbol` is NUL-terminated.
    let entry_point = unsafe { libc::dlsym(handle, symbol.as_ptr()) };
    if entry_point.is_null() {
        let error = dl_error();
        // SAFETY: no initializer ran, so closing cannot invalidate state.
        unsafe { libc::dlclose(handle) };
        return Err(error);
    }

    // SAFETY: `PyInit_*` has the CPython module initialization signature.
    let init: ExtensionInit = unsafe { std::mem::transmute(entry_point) };
    ensure_builtin_types();
    // SAFETY: calling the initializer is exactly what CPython does. The GIL is
    // held across the call; the registry lock is not, so the initializer's
    // calls back into `PyModule_Create2` can take it.
    let result = {
        let _gripper = GilGuard::new();
        unsafe { init() }
    };
    if result.is_null() {
        let error = take_error().unwrap_or_else(|| format!("PyInit_{init_name} returned NULL"));
        // SAFETY: the initializer failed and handed nothing back.
        unsafe { libc::dlclose(handle) };
        return Err(error);
    }
    clear_error();

    let kind = match kind_ref(result) {
        Some(kind) => ShimKind::clone(kind),
        None => {
            dec_ref(result);
            // SAFETY: no module object was produced.
            unsafe { libc::dlclose(handle) };
            return Err(format!("PyInit_{init_name} did not create a module"));
        }
    };
    let id = match kind {
        ShimKind::Module(id) => id,
        ShimKind::ModuleDef { def } => {
            // The wrapper object was only a carrier for the extension-owned
            // definition; the registry will own the created module.
            dec_ref(result);
            let definition = def as *mut PyModuleDefMirror;
            let (id, module_object) = match create_module_from_definition(definition) {
                Ok(created) => created,
                Err(error) => {
                    // SAFETY: no module was registered.
                    unsafe { libc::dlclose(handle) };
                    return Err(error);
                }
            };
            if let Err(error) = run_module_slots(definition, module_object) {
                // SAFETY: the partially initialised module is discarded and
                // never becomes reachable through the registry.
                unsafe { libc::dlclose(handle) };
                let _ = registry_lock().modules.remove(&id);
                dec_ref(module_object);
                return Err(error);
            }
            id
        }
        _ => {
            dec_ref(result);
            // SAFETY: no shim module was produced.
            unsafe { libc::dlclose(handle) };
            return Err(format!("PyInit_{init_name} did not create a module"));
        }
    };

    let mut registry = registry_lock();
    let Some(entry) = registry.modules.get_mut(&id) else {
        return Err(format!("PyInit_{init_name} registered no module"));
    };
    entry.handle = handle as usize;
    registry.by_path.insert(path.to_string(), id);
    Ok(id)
}

fn dl_error() -> String {
    // SAFETY: `dlerror` returns NULL or a NUL-terminated loader-owned message.
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        "dynamic loader operation failed".to_string()
    } else {
        // SAFETY: non-NULL, NUL-terminated message from `dlerror`.
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }
}

/// Looks up one attribute (value or callable) of a loaded module.
pub(crate) fn get_attr(module: u64, name: &str) -> Result<ShimAttr, String> {
    let registry = registry_lock();
    let entry = registry
        .modules
        .get(&module)
        .ok_or_else(|| format!("unknown extension module {module}"))?;
    entry.attrs.get(name).cloned().ok_or_else(|| {
        format!(
            "extension module '{}' has no attribute '{name}'",
            entry.name
        )
    })
}

/// Every Python-visible attribute name of a loaded module.
///
/// `dir(extension_module)` must agree with `getattr`/`hasattr`; the registry's
/// attribute table is the authoritative export list (`PyModule_AddObject`,
/// `PyModule_AddStringConstant` and module-dict writes all land there).
pub(crate) fn module_attribute_names(module: u64) -> Vec<String> {
    let registry = registry_lock();
    let Some(entry) = registry.modules.get(&module) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entry.attrs.keys().cloned().collect();
    names.extend(["__name__", "__doc__", "__dict__"].map(str::to_string));
    names.sort_unstable();
    names.dedup();
    names
}

/// Layout and construction facts of an extension type used as a Hydro base.
pub(crate) fn extension_type_layout(address: usize) -> Result<(isize, isize, bool), String> {
    let type_object = types::as_type(address as *mut ShimObject)?;
    // SAFETY: `as_type` validated the pointer as a live type object.
    unsafe {
        let constructible = ((*type_object).tp_new != 0 || (*type_object).tp_call != 0)
            && (*type_object).tp_flags & PY_TPFLAGS_DISALLOW_INSTANTIATION == 0;
        Ok((
            (*type_object).tp_basicsize,
            (*type_object).tp_itemsize,
            constructible,
        ))
    }
}

/// Runs one module method and returns its owned result, releasing the
/// materialised argument handles first.
#[cfg(test)]
fn call_method_raw(
    module: u64,
    method: u32,
    args: &[ShimValue],
) -> Result<*mut ShimObject, String> {
    let objects: Vec<usize> = args
        .iter()
        .cloned()
        .map(|value| value_object(value) as usize)
        .collect();
    let pointers: Vec<*mut ShimObject> = objects
        .iter()
        .map(|&object| object as *mut ShimObject)
        .collect();
    let result = invoke_module_method(module, method, &pointers, ptr::null_mut());
    for object in objects {
        dec_ref(object as *mut ShimObject);
    }
    result
}

/// Runs one module method with already-materialised C arguments.
fn invoke_module_method(
    module: u64,
    method: u32,
    pointers: &[*mut ShimObject],
    kwnames: *mut ShimObject,
) -> Result<*mut ShimObject, String> {
    let (spec, module_object) = {
        let registry = registry_lock();
        let entry = registry
            .modules
            .get(&module)
            .ok_or_else(|| format!("unknown extension module {module}"))?;
        let spec = entry
            .methods
            .get(method as usize)
            .cloned()
            .ok_or_else(|| format!("module '{}' has no method {method}", entry.name))?;
        (spec, entry.module_object as *mut ShimObject)
    };
    let result = {
        let _gripper = GilGuard::new();
        invoke_method(
            spec.function,
            spec.flags,
            module_object,
            &spec.name,
            pointers,
            kwnames,
        )
    };
    if result.is_null() {
        return Err(take_error().unwrap_or_else(|| "extension function failed".to_string()));
    }
    clear_error();
    Ok(result)
}

/// Shim-value form of the module call, used by the in-crate tests that
/// inspect a result through [`ShimValue`] rather than Hydro.
#[cfg(test)]
fn call_method(module: u64, method: u32, args: &[ShimValue]) -> Result<ShimValue, String> {
    let result = call_method_raw(module, method, args)?;
    let value = object_to_value(result);
    dec_ref(result);
    value
}

/// Calls a module method with Hydro arguments, preserving argument identity.
///
/// The same Hydro container becomes one C object (aliases and cycles survive
/// the call), and a result that returns such an object converts back to the
/// original Hydro value, so an in-place mutation performed after the call is
/// visible through every alias in the caller's graph.
pub(crate) fn call_method_hydro_args(
    module: u64,
    method: u32,
    args: &[i64],
) -> Result<i64, String> {
    let mut created: HashMap<i64, usize> = HashMap::new();
    let mut origins: HashMap<usize, i64> = HashMap::new();
    let mut pointers: Vec<*mut ShimObject> = Vec::with_capacity(args.len());
    for &arg in args {
        match hydro_to_object(arg, 0, &mut created, &mut origins) {
            Ok(pointer) => pointers.push(pointer),
            Err(error) => {
                for pointer in pointers {
                    dec_ref(pointer);
                }
                return Err(error);
            }
        }
    }
    let result = match invoke_module_method(module, method, &pointers, ptr::null_mut()) {
        Ok(result) => result,
        Err(error) => {
            for pointer in pointers {
                dec_ref(pointer);
            }
            return Err(error);
        }
    };
    let value = object_to_hydro_with(result, &origins);
    dec_ref(result);
    // Release the arguments only after the result was converted: an extension
    // may return one of them, and the origin map keys on their addresses.
    for pointer in pointers {
        dec_ref(pointer);
    }
    value
}

/// Runs a heap type object and returns its owned result.
fn call_type_object_raw(address: usize, args: &[ShimValue]) -> Result<*mut ShimObject, String> {
    let type_object = address as *mut ShimObject;
    types::as_type(type_object)?;
    let objects: Vec<usize> = args
        .iter()
        .cloned()
        .map(|value| value_object(value) as usize)
        .collect();
    let pointers: Vec<*mut ShimObject> = objects
        .iter()
        .map(|&object| object as *mut ShimObject)
        .collect();
    let result = {
        let _gripper = GilGuard::new();
        PyObject_Vectorcall(
            type_object,
            pointers.as_ptr(),
            pointers.len(),
            ptr::null_mut(),
        )
    };
    for object in objects {
        dec_ref(object as *mut ShimObject);
    }
    if result.is_null() {
        return Err(take_error().unwrap_or_else(|| "extension constructor failed".to_string()));
    }
    clear_error();
    Ok(result)
}

/// Shim-value form of [`call_type_object_hydro`], used by the in-crate tests.
#[cfg(test)]
fn call_type_object(address: usize, args: &[ShimValue]) -> Result<ShimValue, String> {
    let result = call_type_object_raw(address, args)?;
    let value = object_to_value(result);
    dec_ref(result);
    value
}

/// Like [`call_type_object`], preserving container identity in the result.
pub(crate) fn call_type_object_hydro(address: usize, args: &[ShimValue]) -> Result<i64, String> {
    let result = call_type_object_raw(address, args)?;
    let value = object_to_hydro(result);
    dec_ref(result);
    value
}

/// Full name of a type handle, for diagnostics in the extension bridge.
pub(crate) fn type_display_name(address: usize) -> String {
    ensure_builtin_types();
    if types::is_type_pointer(address as *mut ShimObject) {
        types::type_name(address as *mut TypeObject)
    } else {
        "<unknown type>".to_string()
    }
}

/// True when `object` names an extension type object.
pub(crate) fn object_is_type(object: *mut ShimObject) -> bool {
    types::is_type_pointer(object)
}

/// True when `object` can be called through vectorcall / `tp_call`.
pub(crate) fn object_callable(object: *mut ShimObject) -> bool {
    types::PyCallable_Check(object) != 0
}

/// Looks up `name` on an arbitrary extension handle and returns the owned
/// attribute reference.
fn object_get_attr_raw(object: *mut ShimObject, name: &str) -> Result<*mut ShimObject, String> {
    if object.is_null() {
        return Err(format!(
            "AttributeError: NULL object has no attribute '{name}'"
        ));
    }
    attribute_lookup(object, name)
}

/// Like [`object_get_attr`], preserving container identity in the result.
pub(crate) fn object_get_attr_hydro(object: *mut ShimObject, name: &str) -> Result<i64, String> {
    let attribute = object_get_attr_raw(object, name)?;
    let converted = object_to_hydro(attribute);
    dec_ref(attribute);
    converted
}

/// Builds the keyword dictionary for a vectorcall from contract values.
///
/// Materialises a Hydro argument list and the C keyword dictionary.
fn materialize_hydro_call(
    args: &[i64],
    keywords: &[(String, i64)],
    created: &mut HashMap<i64, usize>,
    origins: &mut HashMap<usize, i64>,
    pointers: &mut Vec<*mut ShimObject>,
) -> Result<*mut ShimObject, String> {
    for &arg in args {
        pointers.push(hydro_to_object(arg, 0, created, origins)?);
    }
    if keywords.is_empty() {
        return Ok(ptr::null_mut());
    }
    let kwargs = PyDict_New();
    if kwargs.is_null() {
        return Err(take_error()
            .unwrap_or_else(|| "MemoryError: cannot allocate a keyword dictionary".to_string()));
    }
    for (name, value_arg) in keywords {
        let key = new_str(name.clone());
        let value = match hydro_to_object(*value_arg, 0, created, origins) {
            Ok(value) => value,
            Err(error) => {
                dec_ref(key);
                dec_ref(kwargs);
                return Err(error);
            }
        };
        if PyDict_SetItem(kwargs, key, value) != 0 {
            dec_ref(key);
            dec_ref(value);
            dec_ref(kwargs);
            return Err(
                take_error().unwrap_or_else(|| format!("TypeError: cannot bind keyword '{name}'"))
            );
        }
        dec_ref(key);
        dec_ref(value);
    }
    Ok(kwargs)
}

/// Calls an arbitrary extension handle with Hydro arguments, preserving
/// argument identity in the same way as [`call_method_hydro_args`].
pub(crate) fn object_call_hydro_args(
    object: *mut ShimObject,
    args: &[i64],
    keywords: &[(String, i64)],
) -> Result<i64, String> {
    if object.is_null() {
        return Err("TypeError: cannot call a NULL object".to_string());
    }
    let mut created: HashMap<i64, usize> = HashMap::new();
    let mut origins: HashMap<usize, i64> = HashMap::new();
    let mut pointers: Vec<*mut ShimObject> = Vec::with_capacity(args.len());
    let kwargs =
        match materialize_hydro_call(args, keywords, &mut created, &mut origins, &mut pointers) {
            Ok(kwargs) => kwargs,
            Err(error) => {
                for pointer in pointers {
                    dec_ref(pointer);
                }
                return Err(error);
            }
        };
    let result = if kwargs.is_null() {
        PyObject_Vectorcall(object, pointers.as_ptr(), pointers.len(), ptr::null_mut())
    } else {
        PyObject_VectorcallDict(object, pointers.as_ptr(), pointers.len(), kwargs)
    };
    if !kwargs.is_null() {
        dec_ref(kwargs);
    }
    if result.is_null() {
        for pointer in pointers {
            dec_ref(pointer);
        }
        return Err(take_error().unwrap_or_else(|| "extension call failed".to_string()));
    }
    clear_error();
    let value = object_to_hydro_with(result, &origins);
    dec_ref(result);
    // Arguments stay alive until the result has been mapped back to its
    // Hydro origins (an extension may return one of them).
    for pointer in pointers {
        dec_ref(pointer);
    }
    value
}

/// Resolves `name` on `object` and calls it with Hydro arguments.
pub(crate) fn object_call_method_hydro_args(
    object: *mut ShimObject,
    name: &str,
    args: &[i64],
    keywords: &[(String, i64)],
) -> Result<i64, String> {
    let method = attribute_lookup(object, name)?;
    let outcome = object_call_hydro_args(method, args, keywords);
    dec_ref(method);
    outcome
}

/// `repr()` of an arbitrary extension handle.
///
/// A failing `tp_repr` keeps its exception pending but still yields the
/// conventional `<Type object at 0x...>` fallback, because this renderer
/// returns plain text.
pub(crate) fn object_repr(object: *mut ShimObject) -> String {
    if object.is_null() {
        return "<NULL>".to_string();
    }
    let result = PyObject_Repr(object);
    if result.is_null() {
        return format!(
            "<{} object at 0x{:x}>",
            object_type_name(object),
            object as usize
        );
    }
    let text = match kind_ref(result) {
        Some(ShimKind::Str { text, .. }) => text.clone(),
        _ => object_type_name(result),
    };
    dec_ref(result);
    text
}

/// `str()` of an arbitrary extension handle, falling back to the address
/// rendering when `tp_str` fails (the pending C error is consumed).
pub(crate) fn object_str(object: *mut ShimObject) -> String {
    if object.is_null() {
        return "<NULL>".to_string();
    }
    let result = PyObject_Str(object);
    if result.is_null() {
        let _ = take_error();
        return format!(
            "<{} object at 0x{:x}>",
            object_type_name(object),
            object as usize
        );
    }
    let text = match kind_ref(result) {
        Some(ShimKind::Str { text, .. }) => text.clone(),
        _ => object_type_name(result),
    };
    dec_ref(result);
    text
}

/// True when `object` is an exception instance: a shim exception payload or an
/// extension-owned instance whose MRO reaches `BaseException`.
pub(crate) fn object_is_exception(object: *mut ShimObject) -> bool {
    if object.is_null() {
        return false;
    }
    if matches!(kind_ref(object), Some(ShimKind::Exception { .. })) {
        return true;
    }
    let type_object = type_of(object);
    if type_object.is_null() {
        return false;
    }
    initialize_exception_types();
    // SAFETY: the static exception table is initialised above.
    let base = unsafe { PyExc_BaseException as *mut TypeObject };
    types::is_subtype(type_object, base)
}

/// Wraps an owned extension exception instance as a Hydro extension object.
///
/// The reference is transferred to the payload, which releases it when the
/// collector drops the object.
pub(crate) fn exception_object_to_hydro(exception: *mut ShimObject) -> i64 {
    let type_name = object_type_name(exception);
    crate::value::ext_object_new(exception, type_name)
}

/// Truthiness of an arbitrary extension handle.
pub(crate) fn object_truthy(object: *mut ShimObject) -> Result<bool, String> {
    let result = types::object_is_true(object);
    if result < 0 {
        let message = take_error();
        // An extension instance whose type defines neither `__bool__`,
        // `__len__` nor a number slot is truthy, like CPython's default
        // `object.__bool__`; `as_predicate` only knows shim payloads.
        if kind_ref(object).is_none()
            && message
                .as_deref()
                .is_none_or(|text| text.contains("predicate"))
        {
            return Ok(true);
        }
        return Err(message.unwrap_or_else(|| {
            "SystemError: __bool__ failed without setting an exception".into()
        }));
    }
    Ok(result != 0)
}

/// `==` between two extension handles.
///
/// Uses the extension's `tp_richcompare` when either type declares one,
/// falling back to identity when the comparison is unsupported. Without a
/// rich comparison the result is pointer identity, matching Python's default
/// `object.__eq__`.
pub(crate) fn objects_equal(left: *mut ShimObject, right: *mut ShimObject) -> Result<bool, String> {
    if left == right {
        return Ok(true);
    }
    if left.is_null() || right.is_null() {
        return Ok(false);
    }
    ensure_builtin_types();
    let left_rich = unsafe { (*type_of(left)).tp_richcompare };
    let right_rich = unsafe { (*type_of(right)).tp_richcompare };
    if left_rich == 0 && right_rich == 0 {
        return Ok(false);
    }
    // `PY_EQ` from `Include/object.h`.
    const PY_EQ: c_int = 2;
    let result = types::PyObject_RichCompare(left, right, PY_EQ);
    if result.is_null() {
        return Err(
            take_error().unwrap_or_else(|| "RuntimeError: extension comparison failed".to_string())
        );
    }
    if matches!(kind_ref(result), Some(ShimKind::NotImplemented)) {
        dec_ref(result);
        return Ok(false);
    }
    let truth = types::object_is_true(result);
    dec_ref(result);
    if truth < 0 {
        return Err(take_error()
            .unwrap_or_else(|| "RuntimeError: comparison result is not truth-testable".into()));
    }
    Ok(truth != 0)
}

/// `isinstance(object, class)` against an extension type object.
pub(crate) fn object_isinstance(
    object: *mut ShimObject,
    class: *mut ShimObject,
) -> Result<bool, String> {
    let result = PyObject_IsInstance(object, class);
    if result < 0 {
        return Err(take_error()
            .unwrap_or_else(|| "TypeError: isinstance() argument check failed".to_string()));
    }
    Ok(result != 0)
}

// ---------------------------------------------------------------------------
// Hydro-backed handles
// ---------------------------------------------------------------------------

/// The Hydro value behind a handle, if it wraps one.
pub(crate) fn hydro_tagged(object: *mut ShimObject) -> Option<i64> {
    hydro_handle(object).map(HydroValue::value)
}

/// The rooted handle behind a Hydro-backed object.
fn hydro_handle(object: *mut ShimObject) -> Option<&'static HydroValue> {
    match kind_ref(object) {
        Some(ShimKind::Hydro(handle)) => Some(handle),
        _ => None,
    }
}

/// Wraps a Hydro value as a contract value that owns one GC root.
pub(crate) fn hydro_shim(value: i64) -> ShimValue {
    ShimValue::Hydro(HydroValue::new(value))
}

/// `repr()` of a Hydro value, propagating a raising `__repr__`.
fn hydro_repr_text(value: i64) -> Result<String, String> {
    let _gripper = GilGuard::new();
    let text = crate::value::repr(value);
    if crate::exc::pending() {
        return Err(take_hydro_error("repr() failed"));
    }
    Ok(text)
}

/// `str()` of a Hydro value, propagating a raising `__str__`.
fn hydro_str_text(value: i64) -> Result<String, String> {
    let _gripper = GilGuard::new();
    let text = crate::value::to_string(value);
    if crate::exc::pending() {
        return Err(take_hydro_error("str() failed"));
    }
    Ok(text)
}

/// `object.name` dispatched to Hydro (methods, descriptors and data
/// attributes run in Hydro).
fn hydro_attribute(object: *mut ShimObject, name: &str) -> Result<*mut ShimObject, String> {
    let Some(value) = hydro_tagged(object) else {
        return Err(format!("AttributeError: object has no attribute '{name}'"));
    };
    let _gripper = GilGuard::new();
    let attribute = crate::methods::getattr(value, name.as_ptr(), name.len());
    if crate::exc::pending() {
        return Err(take_hydro_error(&format!(
            "attribute '{name}' lookup failed"
        )));
    }
    match crate::modules::extension::value_to_shim(attribute) {
        Ok(shim) => Ok(value_object(shim)),
        Err(error) => Err(format!("TypeError: {error}")),
    }
}

/// `object.name = value` (or deletion with a NULL value) dispatched to
/// Hydro's `setattr`/`delattr` implementation.
fn hydro_attribute_set(
    object: *mut ShimObject,
    name: &str,
    value: *mut ShimObject,
) -> Result<(), String> {
    let Some(target) = hydro_tagged(object) else {
        return Err(format!("AttributeError: object has no attribute '{name}'"));
    };
    let new_value = if value.is_null() {
        crate::value::NONE
    } else {
        let shim = object_to_value(value)?;
        crate::modules::extension::shim_to_value(shim)
            .map_err(|error| format!("TypeError: {error}"))?
    };
    let _gripper = GilGuard::new();
    if value.is_null() {
        crate::methods::delete_attr(target, name.as_ptr(), name.len());
    } else {
        crate::methods::setattr(target, name.as_ptr(), name.len(), new_value);
    }
    if crate::exc::pending() {
        return Err(take_hydro_error(&format!(
            "cannot assign attribute '{name}'"
        )));
    }
    Ok(())
}

/// `len()` of a Hydro-backed value.
pub(crate) fn hydro_length(object: *mut ShimObject) -> isize {
    let Some(value) = hydro_tagged(object) else {
        set_error("SystemError: PyObject_Length requires a live object");
        return -1;
    };
    let _gripper = GilGuard::new();
    let result = crate::builtins::len(value);
    if crate::exc::pending() {
        set_error(take_hydro_error("len() failed"));
        return -1;
    }
    match crate::value::as_integer(result) {
        Some(length) => match isize::try_from(length) {
            Ok(length) => length,
            Err(_) => {
                set_error("OverflowError: integer out of range for Py_ssize_t");
                -1
            }
        },
        None => {
            set_error("TypeError: __len__ returned a non-integer");
            -1
        }
    }
}

/// Whether a Hydro value is callable (function, class, bound method or an
/// instance whose class defines `__call__`).
pub(crate) fn hydro_value_callable(value: i64) -> bool {
    if crate::value::as_function(value).is_some()
        || crate::value::as_class(value).is_some()
        || crate::value::as_ext_function(value).is_some()
    {
        return true;
    }
    match crate::value::as_instance(value) {
        Some(instance) => crate::value::class_lookup(instance.class, "__call__").is_some(),
        None => false,
    }
}

/// `isinstance(instance, class)` with `class` a Hydro class value.
fn hydro_isinstance_value(instance: *mut ShimObject, class_value: i64) -> Result<bool, String> {
    let instance_value = object_to_value(instance)
        .and_then(crate::modules::extension::shim_to_value)
        .map_err(|error| format!("TypeError: {error}"))?;
    let _gripper = GilGuard::new();
    let result = crate::builtins::isinstance(instance_value, class_value);
    if crate::exc::pending() {
        return Err(take_hydro_error("isinstance() failed"));
    }
    Ok(result == crate::value::TRUE)
}

/// `issubclass(derived, class)` for Hydro class values.
pub(crate) fn hydro_issubclass(derived: i64, class: i64) -> Result<bool, String> {
    let _gripper = GilGuard::new();
    let result = crate::builtins::issubclass(derived, class);
    if crate::exc::pending() {
        return Err(take_hydro_error("issubclass() failed"));
    }
    Ok(result == crate::value::TRUE)
}

/// `Py_TYPE`-independent type check for a Hydro class value.
pub(crate) fn hydro_class_value(object: *mut ShimObject) -> Option<i64> {
    let value = hydro_tagged(object)?;
    crate::value::as_class(value).map(|_| value)
}

/// Python-visible short name of a Hydro class handle.
pub(crate) fn hydro_type_name_text(object: *mut ShimObject) -> Option<String> {
    let value = hydro_class_value(object)?;
    crate::value::class_display_name(value).map(str::to_string)
}

/// Qualified name of a Hydro class handle (`__qualname__` when assigned).
pub(crate) fn hydro_type_qualname_text(object: *mut ShimObject) -> Option<String> {
    let value = hydro_class_value(object)?;
    let _gripper = GilGuard::new();
    let qualname = crate::methods::getattr(value, b"__qualname__".as_ptr(), 12);
    if crate::exc::pending() {
        crate::exc::clear();
    } else if let Some(text) = crate::value::as_str(qualname) {
        return Some(text.to_string());
    }
    crate::value::class_display_name(value).map(str::to_string)
}

/// Module component of a Hydro class handle (`__module__`).
pub(crate) fn hydro_type_module_text(object: *mut ShimObject) -> Option<String> {
    let value = hydro_class_value(object)?;
    let _gripper = GilGuard::new();
    let module = crate::methods::getattr(value, b"__module__".as_ptr(), 10);
    if !crate::exc::pending() {
        if let Some(text) = crate::value::as_str(module) {
            return Some(text.to_string());
        }
    } else {
        crate::exc::clear();
    }
    let name = crate::value::class_qualname(value)?;
    Some(
        name.rsplit_once('.')
            .map(|(module, _)| module.to_string())
            .unwrap_or_else(|| "builtins".to_string()),
    )
}

/// Flags reported for a Hydro class handle.
///
/// Hydro classes are mutable heap types that support subclassing; no C slot
/// suite exists, so the wrapper reports only the conceptual type flags.
pub(crate) fn hydro_type_flags(object: *mut ShimObject) -> Option<c_ulong> {
    hydro_class_value(object)?;
    Some(
        (types::PY_TPFLAGS_HEAPTYPE | types::PY_TPFLAGS_BASETYPE | types::PY_TPFLAGS_READY)
            as c_ulong,
    )
}

/// `left <op> right` where at least one side is Hydro-backed.
///
/// `op` uses CPython's `Py_LT..Py_GE` numbering. Both operands are converted
/// to Hydro values and compared by the runtime, so a user-defined `__eq__`
/// keeps working through the C boundary.
pub(crate) fn hydro_richcompare(
    left: *mut ShimObject,
    right: *mut ShimObject,
    op: c_int,
) -> Result<bool, String> {
    let convert = |object: *mut ShimObject| -> Result<i64, String> {
        object_to_value(object)
            .and_then(crate::modules::extension::shim_to_value)
            .map_err(|error| format!("TypeError: {error}"))
    };
    let left_value = convert(left)?;
    let right_value = convert(right)?;
    let operation = match op {
        0 => crate::ops::OP_LT,
        1 => crate::ops::OP_LE,
        2 => crate::ops::OP_EQ,
        3 => crate::ops::OP_NE,
        4 => crate::ops::OP_GT,
        5 => crate::ops::OP_GE,
        _ => {
            return Err(format!("SystemError: invalid rich comparison op {op}"));
        }
    };
    let _gripper = GilGuard::new();
    let result = crate::ops::binop(operation, left_value, right_value);
    if crate::exc::pending() {
        return Err(take_hydro_error("comparison failed"));
    }
    Ok(result == crate::value::TRUE)
}

/// Tagged values whose heap members stay rooted while a Rust-side buffer is
/// built and passed to Hydro.
///
/// The collector never scans Rust heap payloads, so every appended value is
/// registered as a persistent root immediately. Hydro code called later may
/// create roots of its own, so each root is released individually (by slot and
/// value) instead of truncating the shared registry.
struct RootedValues {
    values: Vec<i64>,
    roots: Vec<(usize, i64)>,
}

impl RootedValues {
    fn with_capacity(capacity: usize) -> RootedValues {
        RootedValues {
            values: Vec::with_capacity(capacity),
            roots: Vec::with_capacity(capacity),
        }
    }

    fn push(&mut self, value: i64) {
        self.values.push(value);
        let last = self.values.len() - 1;
        let slot = crate::heap::root_values(&self.values[last..]);
        self.roots.push((slot, value));
    }

    fn as_slice(&self) -> &[i64] {
        &self.values
    }

    fn len(&self) -> usize {
        self.values.len()
    }
}

impl Drop for RootedValues {
    fn drop(&mut self) {
        for &(slot, value) in self.roots.iter().rev() {
            crate::heap::release_persistent_root(slot, value);
        }
    }
}

/// Calls a Hydro-backed callable with vectorcall arguments.
///
/// Every argument and keyword value is converted to a Hydro value (Hydro
/// handles keep their identity), rooted for the duration of the call and
/// released afterwards. A Hydro exception becomes a pending C error and a
/// NULL result; a result that has no contract representation does the same.
fn call_hydro(
    handle: &HydroValue,
    values: &[*mut ShimObject],
    names: &[usize],
    positional_count: usize,
) -> *mut ShimObject {
    let keyword_count = names.len();
    let mut arguments = RootedValues::with_capacity(positional_count);
    for &value in &values[..positional_count] {
        match object_to_value(value).and_then(crate::modules::extension::shim_to_value) {
            Ok(argument) => arguments.push(argument),
            Err(error) => {
                set_error(format!("TypeError: {error}"));
                return ptr::null_mut();
            }
        }
    }
    let mut keyword_names = RootedValues::with_capacity(keyword_count);
    let mut keyword_values = RootedValues::with_capacity(keyword_count);
    for (index, &name) in names.iter().enumerate() {
        let text = match kind_ref(name as *mut ShimObject) {
            Some(ShimKind::Str { text, .. }) => text.clone(),
            _ => {
                set_error("TypeError: keywords must be strings");
                return ptr::null_mut();
            }
        };
        let key = crate::value::str_alloc(text);
        keyword_names.push(key);
        let argument = values[positional_count + index];
        match object_to_value(argument).and_then(crate::modules::extension::shim_to_value) {
            Ok(converted) => keyword_values.push(converted),
            Err(error) => {
                set_error(format!("TypeError: {error}"));
                return ptr::null_mut();
            }
        }
    }
    // Flatten `name, value` pairs into the ABI's keyword buffer.
    let mut pairs = RootedValues::with_capacity(keyword_count.saturating_mul(2));
    for (index, &name) in keyword_names.as_slice().iter().enumerate() {
        pairs.push(name);
        let value = keyword_values
            .as_slice()
            .get(index)
            .copied()
            .unwrap_or(crate::value::NONE);
        pairs.push(value);
    }
    let result = {
        let _gripper = GilGuard::new();
        if keyword_count == 0 {
            crate::builtins::call(
                handle.value(),
                arguments.len(),
                arguments.as_slice().as_ptr(),
            )
        } else {
            crate::builtins::call_kw(
                handle.value(),
                arguments.len(),
                arguments.as_slice().as_ptr(),
                keyword_count,
                pairs.as_slice().as_ptr(),
            )
        }
    };
    let failure = if crate::exc::pending() {
        Some(take_hydro_error(&format!(
            "{}() raised an exception",
            handle.type_name()
        )))
    } else {
        None
    };
    if let Some(error) = failure {
        set_error(error);
        return ptr::null_mut();
    }
    // The buffers stay alive (and rooted) until the result is converted: the
    // call may return one of its arguments.
    match crate::modules::extension::value_to_shim(result) {
        Ok(shim) => value_object(shim),
        Err(error) => {
            set_error(format!(
                "TypeError: cannot represent the result of a Hydro call: {error}"
            ));
            ptr::null_mut()
        }
    }
}

/// Converts a pending Hydro exception into the shim's `"<Type>: <message>"`
/// spelling and clears it.
pub(crate) fn take_hydro_error(fallback: &str) -> String {
    let pending = crate::exc::pending_value();
    let message = crate::value::exception_data(pending)
        .map(|data| format!("{}: {}", data.name, data.msg))
        .unwrap_or_else(|| format!("RuntimeError: {fallback}"));
    crate::exc::clear();
    message
}

/// Calls a Hydro native/user module function with shim arguments.
fn call_runtime_function(
    module: &str,
    function: &str,
    values: &[*mut ShimObject],
    names: &[usize],
    positional_count: usize,
) -> *mut ShimObject {
    let keyword_count = names.len();
    if positional_count > values.len() || positional_count + keyword_count > values.len() {
        set_error("SystemError: malformed C API argument vector");
        return ptr::null_mut();
    }
    let mut arguments = RootedValues::with_capacity(positional_count);
    for &value in &values[..positional_count] {
        let shim = match object_to_value(value) {
            Ok(shim) => shim,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        };
        match crate::modules::extension::shim_to_value(shim) {
            Ok(value) => arguments.push(value),
            Err(error) => {
                set_error(format!("TypeError: {error}"));
                return ptr::null_mut();
            }
        }
    }
    let mut pairs = RootedValues::with_capacity(keyword_count.saturating_mul(2));
    for index in 0..keyword_count {
        let name = names[index] as *mut ShimObject;
        let name = match object_to_value(name) {
            Ok(ShimValue::Str(text)) => text,
            Ok(_) => {
                set_error("TypeError: keywords must be strings");
                return ptr::null_mut();
            }
            Err(error) => {
                set_error(format!("TypeError: {error}"));
                return ptr::null_mut();
            }
        };
        let value = values[positional_count + index];
        let value = match object_to_value(value).and_then(crate::modules::extension::shim_to_value)
        {
            Ok(value) => value,
            Err(error) => {
                set_error(format!("TypeError: {error}"));
                return ptr::null_mut();
            }
        };
        pairs.push(crate::value::str_alloc(name));
        pairs.push(value);
    }
    let result = if keyword_count == 0 {
        crate::modules::call_native(
            module,
            function,
            arguments.len(),
            arguments.as_slice().as_ptr(),
        )
    } else {
        crate::modules::call_native_kw(
            module,
            function,
            arguments.len(),
            arguments.as_slice().as_ptr(),
            keyword_count,
            pairs.as_slice().as_ptr(),
        )
    };
    if crate::exc::pending() {
        set_error(take_hydro_error(&format!(
            "{module}.{function}() raised an exception"
        )));
        return ptr::null_mut();
    }
    match crate::modules::extension::value_to_shim(result) {
        Ok(shim) => value_object(shim),
        Err(error) => {
            set_error(format!(
                "TypeError: cannot convert the result of {module}.{function}(): {error}"
            ));
            ptr::null_mut()
        }
    }
}

/// Resolves one attribute of a runtime-backed module.
fn runtime_module_attribute(module: &str, name: &str) -> Result<*mut ShimObject, String> {
    // Native functions are addressable by name and dispatch through the
    // module registry, so expose them as callable handles.
    if hydro_ir::stdlib::function(module, name).is_some() {
        return Ok(new_object(ShimKind::RuntimeFunction {
            module: module.to_string(),
            name: name.to_string(),
        }));
    }
    let value = crate::modules::module_get_by_name(module, name);
    if crate::exc::pending() {
        return Err(take_hydro_error(&format!(
            "module '{module}' has no attribute '{name}'"
        )));
    }
    match crate::modules::extension::value_to_shim(value) {
        Ok(shim) => Ok(value_object(shim)),
        Err(error) => Err(format!(
            "AttributeError: attribute '{name}' of module '{module}' has no C API \
             representation ({error})"
        )),
    }
}

// ---------------------------------------------------------------------------
// Exported module API
// ---------------------------------------------------------------------------

/// `PyModule_Create2`.
#[no_mangle]
pub extern "C" fn PyModule_Create2(
    definition: *mut PyModuleDefMirror,
    _api_version: c_int,
) -> *mut ShimObject {
    match create_module_from_definition(definition) {
        Ok((_, module_object)) => module_object,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyModuleDef_Init`: wraps a `PyModuleDef` for multi-phase initialisation.
#[no_mangle]
pub extern "C" fn PyModuleDef_Init(definition: *mut PyModuleDefMirror) -> *mut ShimObject {
    if definition.is_null() {
        set_error("PyModuleDef_Init: module definition is NULL");
        return ptr::null_mut();
    }
    new_object(ShimKind::ModuleDef {
        def: definition as usize,
    })
}

/// `PyModule_AddStringConstant`.
#[no_mangle]
pub extern "C" fn PyModule_AddStringConstant(
    module: *mut ShimObject,
    name: *const c_char,
    value: *const c_char,
) -> c_int {
    if name.is_null() {
        set_error("PyModule_AddStringConstant: name is NULL");
        return -1;
    }
    if value.is_null() {
        set_error("PyModule_AddStringConstant: value is NULL");
        return -1;
    }
    let name = read_c_string(name);
    let value = read_c_string(value);
    match add_attribute(module, name, ShimAttr::Value(ShimValue::Str(value))) {
        Ok(()) => 0,
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

/// `PyModule_AddObject`: adds an attribute, stealing the value reference.
#[no_mangle]
pub extern "C" fn PyModule_AddObject(
    module: *mut ShimObject,
    name: *const c_char,
    value: *mut ShimObject,
) -> c_int {
    if name.is_null() {
        set_error("PyModule_AddObject: name is NULL");
        return -1;
    }
    if value.is_null() {
        set_error("PyModule_AddObject: value is NULL");
        return -1;
    }
    let name = read_c_string(name);
    if types::is_type_pointer(value) {
        // The module keeps the caller's reference on success (CPython's steal
        // contract); on failure the caller still owns it.
        return match add_attribute(module, name, ShimAttr::Type(value as usize)) {
            Ok(()) => 0,
            Err(error) => {
                set_error(error);
                -1
            }
        };
    }
    let attribute = match object_to_value(value) {
        Ok(converted) => ShimAttr::Value(converted),
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    match add_attribute(module, name, attribute) {
        Ok(()) => {
            // The module owns the reference on success.
            dec_ref(value);
            0
        }
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

// ---------------------------------------------------------------------------
// Module, import, evaluation, sys and allocator API
// ---------------------------------------------------------------------------

/// Creates a module object with no extension definition (used by the import
/// registry). The module object is immortal, like the loader-created ones.
fn create_named_module(name: String) -> Result<(u64, *mut ShimObject), String> {
    create_module_entry(name, None)
}

/// Creates a module object that resolves its attributes through the Hydro
/// runtime registry ([`crate::modules::registry_lookup`]).
fn create_runtime_module(name: String) -> Result<(u64, *mut ShimObject), String> {
    create_module_entry(name.clone(), Some(name))
}

/// Shared body of [`create_named_module`] and [`create_runtime_module`].
fn create_module_entry(
    name: String,
    runtime: Option<String>,
) -> Result<(u64, *mut ShimObject), String> {
    let id = allocate_module_id();
    let module_object = make_immortal(new_object(ShimKind::Module(id)));
    let c_name =
        CString::new(name.clone()).map_err(|_| "module name contains a NUL".to_string())?;
    let mut attrs = HashMap::new();
    attrs.insert(
        "__name__".to_string(),
        ShimAttr::Value(ShimValue::Str(name.clone())),
    );
    attrs.insert(
        "__file__".to_string(),
        ShimAttr::Value(ShimValue::Str(name.clone())),
    );
    attrs.insert(
        "__package__".to_string(),
        ShimAttr::Value(ShimValue::Str(String::new())),
    );
    attrs.insert("__doc__".to_string(), ShimAttr::Value(ShimValue::None));
    let mut registry = registry_lock();
    registry.by_name.insert(name.clone(), id);
    registry.modules.insert(
        id,
        ModuleEntry {
            name,
            c_name: c_name.clone(),
            filename: c_name,
            definition: 0,
            state: 0,
            dict: 0,
            handle: 0,
            module_object: module_object as usize,
            attrs,
            methods: Vec::new(),
            runtime,
        },
    );
    Ok((id, module_object))
}

/// Adds one `PyMethodDef` to a module and its attribute table.
fn add_module_method(module: *mut ShimObject, method: MethodSpec) -> Result<(), String> {
    let id = module_id(module)?;
    let mut registry = registry_lock();
    let entry = registry
        .modules
        .get_mut(&id)
        .ok_or_else(|| format!("unknown module {id}"))?;
    let index = entry.methods.len();
    let name = method.name.clone();
    entry.methods.push(method);
    let attribute = ShimAttr::Callable(ShimCallable {
        module: id,
        method: index as u32,
    });
    entry.attrs.insert(name.clone(), attribute);
    if entry.dict != 0 {
        let snapshot = entry.attrs.get(&name).cloned();
        if let Some(handle) = snapshot.and_then(|attribute| attribute_handle(entry, &attribute)) {
            let key = new_str(name);
            let status = PyDict_SetItem(entry.dict as *mut ShimObject, key, handle);
            dec_ref(key);
            dec_ref(handle);
            if status != 0 {
                return Err(take_error()
                    .unwrap_or_else(|| "AttributeError: cannot update module dictionary".into()));
            }
        }
    }
    Ok(())
}

fn module_entry_pointer<F, T>(module: *mut ShimObject, f: F) -> Result<T, String>
where
    F: FnOnce(&ModuleEntry) -> T,
{
    let id = module_id(module)?;
    let registry = registry_lock();
    registry
        .modules
        .get(&id)
        .map(f)
        .ok_or_else(|| format!("SystemError: unknown module {id}"))
}

/// `PyModule_GetName`.
#[no_mangle]
pub extern "C" fn PyModule_GetName(module: *mut ShimObject) -> *const c_char {
    match module_entry_pointer(module, |entry| entry.c_name.as_ptr()) {
        Ok(pointer) => pointer,
        Err(error) => {
            set_error(error);
            ptr::null()
        }
    }
}

/// `PyModule_GetNameObject`.
#[no_mangle]
pub extern "C" fn PyModule_GetNameObject(module: *mut ShimObject) -> *mut ShimObject {
    match module_entry_pointer(module, |entry| entry.name.clone()) {
        Ok(name) => new_str(name),
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyModule_GetFilename`.
#[no_mangle]
pub extern "C" fn PyModule_GetFilename(module: *mut ShimObject) -> *const c_char {
    match module_entry_pointer(module, |entry| entry.filename.as_ptr()) {
        Ok(pointer) => pointer,
        Err(error) => {
            set_error(error);
            ptr::null()
        }
    }
}

/// `PyModule_GetFilenameObject`.
#[no_mangle]
pub extern "C" fn PyModule_GetFilenameObject(module: *mut ShimObject) -> *mut ShimObject {
    match module_entry_pointer(module, |entry| {
        entry.filename.to_string_lossy().into_owned()
    }) {
        Ok(name) => new_str(name),
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyModule_GetDef`.
#[no_mangle]
pub extern "C" fn PyModule_GetDef(module: *mut ShimObject) -> *mut PyModuleDefMirror {
    match module_entry_pointer(module, |entry| entry.definition) {
        Ok(definition) => definition as *mut PyModuleDefMirror,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyModule_GetState`.
///
/// Returns the module's per-module state block, or NULL for a module whose
/// `PyModuleDef.m_size` is zero, without setting an exception: that is
/// CPython's `_PyModule_GetState` contract, and extensions distinguish "no
/// state" from a failed call by looking at `PyErr_Occurred`. A non-module
/// argument raises `TypeError`.
#[no_mangle]
pub extern "C" fn PyModule_GetState(module: *mut ShimObject) -> *mut c_void {
    match module_entry_pointer(module, |entry| entry.state) {
        Ok(state) => state as *mut c_void,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyModule_GetDict`: returns the module's borrowed `__dict__`.
#[no_mangle]
pub extern "C" fn PyModule_GetDict(module: *mut ShimObject) -> *mut ShimObject {
    let id = match module_id(module) {
        Ok(id) => id,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let mut registry = registry_lock();
    let existing = match registry.modules.get(&id) {
        Some(entry) => entry.dict,
        None => {
            drop(registry);
            set_error(format!("SystemError: unknown module {id}"));
            return ptr::null_mut();
        }
    };
    if existing != 0 {
        return existing as *mut ShimObject;
    }
    let names: Vec<(String, ShimAttr)> = match registry.modules.get(&id) {
        Some(entry) => entry
            .attrs
            .iter()
            .map(|(name, attribute)| (name.clone(), attribute.clone()))
            .collect(),
        None => Vec::new(),
    };
    let dict = PyDict_New();
    if dict.is_null() {
        return ptr::null_mut();
    }
    for (name, attribute) in names {
        let Some(entry) = registry.modules.get(&id) else {
            break;
        };
        let Some(value) = attribute_handle(entry, &attribute) else {
            continue;
        };
        let key = new_str(name);
        let status = PyDict_SetItem(dict, key, value);
        dec_ref(key);
        dec_ref(value);
        if status != 0 {
            let error = take_error()
                .unwrap_or_else(|| "SystemError: cannot populate the module dictionary".into());
            dec_ref(dict);
            drop(registry);
            set_error(error);
            return ptr::null_mut();
        }
    }
    if let Some(entry) = registry.modules.get_mut(&id) {
        entry.dict = dict as usize;
    }
    dict
}

/// `PyModule_NewObject`.
#[no_mangle]
pub extern "C" fn PyModule_NewObject(name: *mut ShimObject) -> *mut ShimObject {
    let Some(name) = as_string(name) else {
        set_error("TypeError: PyModule_NewObject requires a str name");
        return ptr::null_mut();
    };
    match create_named_module(name) {
        Ok((_, module)) => module,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyModule_AddIntConstant`.
#[allow(clippy::unnecessary_cast)] // `c_long` is `i64` on x86_64.
#[no_mangle]
pub extern "C" fn PyModule_AddIntConstant(
    module: *mut ShimObject,
    name: *const c_char,
    value: c_long,
) -> c_int {
    if name.is_null() {
        set_error("TypeError: PyModule_AddIntConstant: name is NULL");
        return -1;
    }
    let name = read_c_string(name);
    match add_attribute(module, name, ShimAttr::Value(ShimValue::Int(value as i64))) {
        Ok(()) => 0,
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

/// `PyModule_AddFunctions`.
#[no_mangle]
pub extern "C" fn PyModule_AddFunctions(
    module: *mut ShimObject,
    methods: *mut PyMethodDefMirror,
) -> c_int {
    if methods.is_null() {
        set_error("TypeError: PyModule_AddFunctions: methods table is NULL");
        return -1;
    }
    let table = match read_method_table(methods, "PyModule_AddFunctions") {
        Ok(table) => table,
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    for method in table {
        if let Err(error) = add_module_method(module, method) {
            set_error(error);
            return -1;
        }
    }
    0
}

/// `PyModule_ExecDef`.
#[no_mangle]
pub extern "C" fn PyModule_ExecDef(
    module: *mut ShimObject,
    definition: *mut PyModuleDefMirror,
) -> c_int {
    match run_module_slots(definition, module) {
        Ok(()) => 0,
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

/// `PyModule_FromDefAndSpec2`.
#[no_mangle]
pub extern "C" fn PyModule_FromDefAndSpec2(
    definition: *mut PyModuleDefMirror,
    _spec: *mut c_void,
    _api_version: c_int,
) -> *mut ShimObject {
    let (id, module_object) = match create_module_from_definition(definition) {
        Ok(created) => created,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    if let Err(error) = run_module_slots(definition, module_object) {
        set_error(error);
        {
            let mut registry = registry_lock();
            registry.modules.remove(&id);
            registry.by_name.retain(|_, module| *module != id);
        }
        dec_ref(module_object);
        return ptr::null_mut();
    }
    module_object
}

/// `PyState_FindModule`: borrowed module whose definition is `definition`.
#[no_mangle]
pub extern "C" fn PyState_FindModule(definition: *mut PyModuleDefMirror) -> *mut ShimObject {
    if definition.is_null() {
        return ptr::null_mut();
    }
    let registry = registry_lock();
    for entry in registry.modules.values() {
        if entry.definition == definition as usize {
            return entry.module_object as *mut ShimObject;
        }
    }
    ptr::null_mut()
}

// -- imports ----------------------------------------------------------------

/// Mirrors CPython's submodule binding: a successful import of `pkg.sub`
/// stores `sub` on the `pkg` namespace.
///
/// CPython's importer sets the attribute on the parent package as part of the
/// import. Compiled code that reads `from pkg import sub` without a statically
/// linked binding resolves the name from `pkg`'s global table, so the attribute
/// must exist there after an extension imports the submodule at runtime
/// (NumPy's `_multiarray_umath` imports `numpy.dtypes`, whose
/// `_add_dtype_helper` reads `from numpy import dtypes`).
///
/// Native modules resolve members from their central spec and keep no global
/// side store, so only compiled/user parents are updated. The parent does not
/// need to be fully initialized: a package that is still running its body
/// already owns a global table, exactly like CPython's partially initialized
/// module.
fn publish_submodule(name: &str) {
    let Some((parent, leaf)) = name.rsplit_once('.') else {
        return;
    };
    if parent.is_empty() || leaf.is_empty() || hydro_ir::stdlib::module(parent).is_some() {
        return;
    }
    // Only a module the runtime registry knows can be published; a failed body
    // publishes nothing, so a missing name stays visible to the caller.
    let Some(value) = crate::modules::registry_lookup(name) else {
        return;
    };
    crate::core::globals::set_value(parent, leaf, value);
}

/// Looks up a loaded module by name and returns it with a new reference.
fn import_loaded(name: &str) -> Result<*mut ShimObject, String> {
    let existing = {
        let registry = registry_lock();
        match registry.by_name.get(name).copied() {
            Some(id) => {
                let Some(entry) = registry.modules.get(&id) else {
                    return Err(format!(
                        "ImportError: dangling module registry entry for '{name}'"
                    ));
                };
                let module = entry.module_object as *mut ShimObject;
                inc_ref(module);
                Some(module)
            }
            None => None,
        }
    };
    if let Some(module) = existing {
        // Outside the C registry lock: publishing a submodule touches the
        // Hydro global store, which must never be acquired while holding it.
        publish_submodule(name);
        return Ok(module);
    }
    // Not a loaded DSO: fall back to the Hydro runtime registry, which holds
    // every native stdlib module and every compiled user module.
    if crate::modules::registry_lookup(name).is_some() {
        let (_, module) = create_runtime_module(name.to_string())?;
        inc_ref(module);
        publish_submodule(name);
        return Ok(module);
    }
    // The name may be a compiled module whose body has not run yet. This is
    // the dynamic-import bridge a C extension uses (`numpy.exceptions`): the
    // startup loop has registered the body, run it on demand and return the
    // module it populated. The body is Hydro user code, so it runs under the
    // same (reentrant) emulated GIL as any other call from an extension.
    let executed = {
        let _gripper = GilGuard::new();
        crate::module_init::initialize(name)
    };
    if executed != 0 {
        if crate::exc::pending() {
            return Err(take_hydro_error(&format!(
                "initializing module '{name}' failed"
            )));
        }
        // `initialize` published the module value in `sys.modules`; expose the
        // same module through an extension-visible module object.
        let (_, module) = create_runtime_module(name.to_string())?;
        inc_ref(module);
        publish_submodule(name);
        return Ok(module);
    }
    Err(format!("ImportError: No module named '{name}'"))
}

/// `PyImport_ImportModule`.
#[no_mangle]
pub extern "C" fn PyImport_ImportModule(name: *const c_char) -> *mut ShimObject {
    if name.is_null() {
        set_error("SystemError: PyImport_ImportModule: name is NULL");
        return ptr::null_mut();
    }
    match import_loaded(&read_c_string(name)) {
        Ok(module) => module,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyImport_Import`.
#[no_mangle]
pub extern "C" fn PyImport_Import(name: *mut ShimObject) -> *mut ShimObject {
    let Some(name) = as_string(name) else {
        set_error("TypeError: PyImport_Import requires a str module name");
        return ptr::null_mut();
    };
    match import_loaded(&name) {
        Ok(module) => module,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyImport_ImportModuleLevel`.
#[no_mangle]
pub extern "C" fn PyImport_ImportModuleLevel(
    name: *const c_char,
    _globals: *mut ShimObject,
    _locals: *mut ShimObject,
    _fromlist: *mut ShimObject,
    level: c_int,
) -> *mut ShimObject {
    if level > 0 {
        set_error(
            "ImportError: relative imports are not supported by the embedded extension bridge",
        );
        return ptr::null_mut();
    }
    PyImport_ImportModule(name)
}

/// `PyImport_ImportModuleLevelObject`.
#[no_mangle]
pub extern "C" fn PyImport_ImportModuleLevelObject(
    name: *mut ShimObject,
    _globals: *mut ShimObject,
    _locals: *mut ShimObject,
    _fromlist: *mut ShimObject,
    level: c_int,
) -> *mut ShimObject {
    if level > 0 {
        set_error(
            "ImportError: relative imports are not supported by the embedded extension bridge",
        );
        return ptr::null_mut();
    }
    PyImport_Import(name)
}

/// `PyImport_GetModule`: borrowed reference to a loaded module, or NULL.
#[no_mangle]
pub extern "C" fn PyImport_GetModule(name: *mut ShimObject) -> *mut ShimObject {
    let Some(name) = as_string(name) else {
        set_error("TypeError: PyImport_GetModule requires a str module name");
        return ptr::null_mut();
    };
    let registry = registry_lock();
    registry
        .by_name
        .get(&name)
        .and_then(|id| registry.modules.get(id))
        .map(|entry| entry.module_object as *mut ShimObject)
        .unwrap_or(ptr::null_mut())
}

/// `PyImport_AddModule`: borrowed module object, created on demand.
#[no_mangle]
pub extern "C" fn PyImport_AddModule(name: *const c_char) -> *mut ShimObject {
    if name.is_null() {
        set_error("SystemError: PyImport_AddModule: name is NULL");
        return ptr::null_mut();
    }
    let name = read_c_string(name);
    {
        let registry = registry_lock();
        if let Some(id) = registry.by_name.get(&name) {
            if let Some(entry) = registry.modules.get(id) {
                return entry.module_object as *mut ShimObject;
            }
        }
    }
    match create_named_module(name) {
        Ok((_, module)) => module,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyImport_AddModuleRef`: like [`PyImport_AddModule`] with a new reference.
#[no_mangle]
pub extern "C" fn PyImport_AddModuleRef(name: *const c_char) -> *mut ShimObject {
    let module = PyImport_AddModule(name);
    if !module.is_null() {
        inc_ref(module);
    }
    module
}

/// `PyImport_GetModuleDict`: borrowed `sys.modules`-equivalent dictionary.
#[no_mangle]
pub extern "C" fn PyImport_GetModuleDict() -> *mut ShimObject {
    let mut registry = registry_lock();
    if registry.module_dict == 0 {
        let dict = PyDict_New();
        if dict.is_null() {
            return ptr::null_mut();
        }
        registry.module_dict = dict as usize;
    }
    let dict = registry.module_dict as *mut ShimObject;
    let entries: Vec<(String, usize)> = registry
        .modules
        .values()
        .map(|entry| (entry.name.clone(), entry.module_object))
        .collect();
    for (name, module) in entries {
        let key = new_str(name);
        let status = PyDict_SetItem(dict, key, module as *mut ShimObject);
        dec_ref(key);
        if status != 0 {
            clear_error();
        }
    }
    dict
}

// -- evaluation and sys -----------------------------------------------------

fn dict_set_string(dict: *mut ShimObject, name: &str, value: *mut ShimObject) {
    let key = new_str(name.to_string());
    let status = PyDict_SetItem(dict, key, value);
    dec_ref(key);
    if status != 0 {
        clear_error();
    }
}

fn builtins_dict() -> usize {
    static BUILTINS: LazyLock<usize> = LazyLock::new(|| {
        ensure_builtin_types();
        let dict = PyDict_New();
        if dict.is_null() {
            return 0;
        }
        unsafe {
            for (name, pointer) in [
                ("None", ptr::addr_of_mut!(_Py_NoneStruct)),
                ("True", ptr::addr_of_mut!(_Py_TrueStruct)),
                ("False", ptr::addr_of_mut!(_Py_FalseStruct)),
                (
                    "NotImplemented",
                    ptr::addr_of_mut!(_Py_NotImplementedStruct),
                ),
                ("Ellipsis", ptr::addr_of_mut!(_Py_EllipsisObject)),
            ] {
                dict_set_string(dict, name, pointer);
            }
            for (name, type_object) in [
                ("bool", ptr::addr_of_mut!(types::BOOL_TYPE)),
                ("int", ptr::addr_of_mut!(types::INT_TYPE)),
                ("float", ptr::addr_of_mut!(types::FLOAT_TYPE)),
                ("complex", ptr::addr_of_mut!(types::COMPLEX_TYPE)),
                ("str", ptr::addr_of_mut!(types::STR_TYPE)),
                ("bytes", ptr::addr_of_mut!(types::BYTES_TYPE)),
                ("bytearray", ptr::addr_of_mut!(types::BYTEARRAY_TYPE)),
                ("list", ptr::addr_of_mut!(types::LIST_TYPE)),
                ("tuple", ptr::addr_of_mut!(types::TUPLE_TYPE)),
                ("dict", ptr::addr_of_mut!(types::DICT_TYPE)),
                ("set", ptr::addr_of_mut!(types::SET_TYPE)),
                ("frozenset", ptr::addr_of_mut!(types::FROZENSET_TYPE)),
                ("slice", ptr::addr_of_mut!(types::SLICE_TYPE)),
                ("memoryview", ptr::addr_of_mut!(types::MEMORYVIEW_TYPE)),
                ("range", ptr::addr_of_mut!(types::RANGE_TYPE)),
                ("object", ptr::addr_of_mut!(types::OBJECT_TYPE)),
                ("type", ptr::addr_of_mut!(types::TYPE_TYPE)),
            ] {
                dict_set_string(dict, name, type_object as *mut ShimObject);
            }
            let exceptions: [(*const c_char, *mut ShimObject); 6] = [
                (c"Exception".as_ptr(), PyExc_Exception),
                (c"BaseException".as_ptr(), PyExc_BaseException),
                (c"TypeError".as_ptr(), PyExc_TypeError),
                (c"ValueError".as_ptr(), PyExc_ValueError),
                (c"AttributeError".as_ptr(), PyExc_AttributeError),
                (c"RuntimeError".as_ptr(), PyExc_RuntimeError),
            ];
            for (name, exception) in exceptions {
                dict_set_string(dict, &read_c_string(name), exception);
            }
        }
        dict as usize
    });
    *BUILTINS
}

std::thread_local! {
    static EVAL_GLOBALS: RefCell<Option<usize>> = const { RefCell::new(None) };
    static EVAL_LOCALS: RefCell<Option<usize>> = const { RefCell::new(None) };
}

fn frame_dict(slot: &'static std::thread::LocalKey<RefCell<Option<usize>>>) -> usize {
    slot.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            let dict = PyDict_New();
            if dict.is_null() {
                return 0;
            }
            let name = new_str("__hydro__".to_string());
            dict_set_string(dict, "__name__", name);
            dec_ref(name);
            let builtins = builtins_dict();
            if builtins != 0 {
                dict_set_string(dict, "__builtins__", builtins as *mut ShimObject);
            }
            *cell = Some(dict as usize);
        }
        (*cell).unwrap_or(0)
    })
}

/// `PyEval_GetBuiltins`: borrowed builtins dictionary.
#[no_mangle]
pub extern "C" fn PyEval_GetBuiltins() -> *mut ShimObject {
    builtins_dict() as *mut ShimObject
}

/// `PyEval_GetGlobals`: borrowed per-thread globals dictionary.
#[no_mangle]
pub extern "C" fn PyEval_GetGlobals() -> *mut ShimObject {
    frame_dict(&EVAL_GLOBALS) as *mut ShimObject
}

/// `PyEval_GetLocals`: borrowed per-thread locals dictionary.
#[no_mangle]
pub extern "C" fn PyEval_GetLocals() -> *mut ShimObject {
    frame_dict(&EVAL_LOCALS) as *mut ShimObject
}

/// Copies one runtime `sys.<name>` value into the C-visible dictionary.
///
/// The value is resolved through the native module table and converted with
/// the same tagged-value bridge extension calls use; the wrapper owns the
/// Hydro root for as long as the dictionary holds it.
fn sys_copy_runtime_value(dict: *mut ShimObject, name: &str) {
    let value = crate::modules::module_get_by_name("sys", name);
    if crate::exc::pending() {
        // The runtime has no such value; leave the entry absent rather than
        // exposing a placeholder. `PySys_GetObject` then returns NULL.
        crate::exc::clear();
        return;
    }
    if let Ok(shim) = crate::modules::extension::value_to_shim(value) {
        let object = value_object(shim);
        if !object.is_null() {
            dict_set_string(dict, name, object);
            dec_ref(object);
        }
    }
    // A value with no C representation stays absent; `PySys_GetObject`
    // then reports it as missing instead of returning an object with an
    // unrelated exception pending. Every name resolved above has a C
    // representation, which the sys-surface test asserts.
}

/// The C-visible `sys` dictionary behind `PySys_GetObject`.
///
/// Every name an extension commonly reads is populated from the runtime's own
/// `sys` module, so `sys.flags.optimize`, `sys.version`, `sys.platform`,
/// `sys.implementation` and friends report the values Hydro code sees instead
/// of a C-only stub. `sys.modules` stays the live C registry dictionary that
/// `PyImport_GetModuleDict` returns, and the two option containers are real
/// empty objects because this runtime parses no `-W`/`-X` options.
///
/// The entries that wrap Hydro values are only valid for the heap generation
/// that produced them, so a runtime reset (`hydro_runtime_init`) rebuilds the
/// dictionary instead of handing out handles into the freed heap.
fn sys_dict() -> usize {
    static SYS: LazyLock<Mutex<(u64, usize)>> = LazyLock::new(|| Mutex::new((u64::MAX, 0)));
    loop {
        let generation = crate::heap::generation();
        {
            let cached = SYS.lock().unwrap_or_else(PoisonError::into_inner);
            if cached.0 == generation && cached.1 != 0 {
                return cached.1;
            }
        }
        // Build without holding the cache lock: the values come from the
        // runtime's module table, which takes its own locks.
        let dict = build_sys_dict();
        if dict == 0 {
            return 0;
        }
        let mut cached = SYS.lock().unwrap_or_else(PoisonError::into_inner);
        if crate::heap::generation() != generation {
            // A reset raced the build; its heap is gone, so try again.
            drop(cached);
            dec_ref(dict as *mut ShimObject);
            continue;
        }
        if cached.0 == generation && cached.1 != 0 {
            // Another thread published this generation's dictionary first.
            let winner = cached.1;
            drop(cached);
            dec_ref(dict as *mut ShimObject);
            return winner;
        }
        let previous = cached.1;
        cached.0 = generation;
        cached.1 = dict;
        drop(cached);
        if previous != 0 {
            dec_ref(previous as *mut ShimObject);
        }
        return dict;
    }
}

/// Builds one generation's C-visible `sys` dictionary.
fn build_sys_dict() -> usize {
    ensure_builtin_types();
    let dict = PyDict_New();
    if dict.is_null() {
        return 0;
    }
    let modules = PyImport_GetModuleDict();
    if !modules.is_null() {
        dict_set_string(dict, "modules", modules);
    }
    for name in [
        "flags",
        "implementation",
        "version",
        "version_info",
        "platform",
        "byteorder",
        "maxsize",
        "path",
        "argv",
    ] {
        sys_copy_runtime_value(dict, name);
    }
    for (name, container) in [("warnoptions", PyList_New(0)), ("_xoptions", PyDict_New())] {
        if !container.is_null() {
            dict_set_string(dict, name, container);
            dec_ref(container);
        }
    }
    dict as usize
}

/// `PySys_GetObject`: borrowed object from the `sys` dictionary, or NULL.
#[no_mangle]
pub extern "C" fn PySys_GetObject(name: *const c_char) -> *mut ShimObject {
    if name.is_null() {
        set_error("SystemError: PySys_GetObject: name is NULL");
        return ptr::null_mut();
    }
    let dict = sys_dict();
    if dict == 0 {
        set_error("MemoryError: cannot create the sys dictionary");
        return ptr::null_mut();
    }
    PyDict_GetItemString(dict as *mut ShimObject, name)
}

/// `PySys_SetObject`.
#[no_mangle]
pub extern "C" fn PySys_SetObject(name: *const c_char, value: *mut ShimObject) -> c_int {
    if name.is_null() {
        set_error("SystemError: PySys_SetObject: name is NULL");
        return -1;
    }
    let dict = sys_dict();
    if dict == 0 {
        set_error("MemoryError: cannot create the sys dictionary");
        return -1;
    }
    if value.is_null() {
        set_error("TypeError: PySys_SetObject: value is NULL");
        return -1;
    }
    PyDict_SetItemString(dict as *mut ShimObject, name, value)
}

fn unsupported_eval(name: &str) -> String {
    format!(
        "RuntimeError: {name} is not available: this runtime has no Python source evaluator \
         bridge, so extension code must not execute Python source"
    )
}

/// `PyRun_SimpleString`: explicit failure (no evaluator bridge).
#[no_mangle]
pub extern "C" fn PyRun_SimpleString(_source: *const c_char) -> c_int {
    set_error(unsupported_eval("PyRun_SimpleString"));
    -1
}

/// `PyRun_SimpleStringFlags`: explicit failure.
#[no_mangle]
pub extern "C" fn PyRun_SimpleStringFlags(_source: *const c_char, _flags: *mut c_void) -> c_int {
    set_error(unsupported_eval("PyRun_SimpleStringFlags"));
    -1
}

/// `PyRun_String`: explicit failure.
#[no_mangle]
pub extern "C" fn PyRun_String(
    _source: *const c_char,
    _start: c_int,
    _globals: *mut ShimObject,
    _locals: *mut ShimObject,
) -> *mut ShimObject {
    set_error(unsupported_eval("PyRun_String"));
    ptr::null_mut()
}

/// `PyRun_StringFlags`: explicit failure.
#[no_mangle]
pub extern "C" fn PyRun_StringFlags(
    _source: *const c_char,
    _start: c_int,
    _globals: *mut ShimObject,
    _locals: *mut ShimObject,
    _flags: *mut c_void,
) -> *mut ShimObject {
    set_error(unsupported_eval("PyRun_StringFlags"));
    ptr::null_mut()
}

// -- allocators -------------------------------------------------------------

/// `PyMem_Malloc`.
#[no_mangle]
pub extern "C" fn PyMem_Malloc(size: usize) -> *mut c_void {
    // SAFETY: `malloc` is the process allocator; size 0 still yields a unique
    // pointer, matching CPython's guarantee.
    unsafe { libc::malloc(size.max(1)) }
}

/// `PyMem_Free`.
#[no_mangle]
pub extern "C" fn PyMem_Free(pointer: *mut c_void) {
    if !pointer.is_null() {
        // SAFETY: the pointer came from this allocator family.
        unsafe { libc::free(pointer) };
    }
}

/// `PyMem_Realloc`.
#[no_mangle]
pub extern "C" fn PyMem_Realloc(pointer: *mut c_void, size: usize) -> *mut c_void {
    // SAFETY: `realloc` accepts NULL and any pointer from `malloc`/`realloc`.
    unsafe { libc::realloc(pointer, size.max(1)) }
}

/// `PyMem_Calloc`.
#[no_mangle]
pub extern "C" fn PyMem_Calloc(count: usize, size: usize) -> *mut c_void {
    // SAFETY: `calloc` returns zeroed storage or NULL.
    unsafe { libc::calloc(count.max(1), size.max(1)) }
}

/// `PyObject_Malloc`.
#[no_mangle]
pub extern "C" fn PyObject_Malloc(size: usize) -> *mut c_void {
    PyMem_Malloc(size)
}

/// `PyObject_Free`.
#[no_mangle]
pub extern "C" fn PyObject_Free(pointer: *mut c_void) {
    PyMem_Free(pointer);
}

/// `PyObject_Realloc`.
#[no_mangle]
pub extern "C" fn PyObject_Realloc(pointer: *mut c_void, size: usize) -> *mut c_void {
    PyMem_Realloc(pointer, size)
}

/// `PyObject_Calloc`.
#[no_mangle]
pub extern "C" fn PyObject_Calloc(count: usize, size: usize) -> *mut c_void {
    PyMem_Calloc(count, size)
}

/// `PyMem_RawMalloc`.
#[no_mangle]
pub extern "C" fn PyMem_RawMalloc(size: usize) -> *mut c_void {
    PyMem_Malloc(size)
}

/// `PyMem_RawFree`.
#[no_mangle]
pub extern "C" fn PyMem_RawFree(pointer: *mut c_void) {
    PyMem_Free(pointer);
}

/// `PyMem_RawRealloc`.
#[no_mangle]
pub extern "C" fn PyMem_RawRealloc(pointer: *mut c_void, size: usize) -> *mut c_void {
    PyMem_Realloc(pointer, size)
}

/// `PyMem_RawCalloc`.
#[no_mangle]
pub extern "C" fn PyMem_RawCalloc(count: usize, size: usize) -> *mut c_void {
    PyMem_Calloc(count, size)
}

// -- Unicode writer ---------------------------------------------------------

/// Opaque `PyUnicodeWriter` (CPython 3.14 declares it opaque).
struct UnicodeWriter {
    text: String,
}

fn unicode_writer<'a>(writer: *mut c_void) -> Option<&'a mut UnicodeWriter> {
    if writer.is_null() {
        set_error("SystemError: NULL PyUnicodeWriter");
        return None;
    }
    // SAFETY: the pointer came from `PyUnicodeWriter_Create`, which boxes one
    // `UnicodeWriter`; the caller keeps it alive until Finish/Discard.
    Some(unsafe { &mut *(writer as *mut UnicodeWriter) })
}

/// `PyUnicodeWriter_Create`.
#[no_mangle]
pub extern "C" fn PyUnicodeWriter_Create(length: isize) -> *mut c_void {
    let capacity = usize::try_from(length).unwrap_or(0);
    Box::into_raw(Box::new(UnicodeWriter {
        text: String::with_capacity(capacity),
    })) as *mut c_void
}

/// `PyUnicodeWriter_Discard`.
#[no_mangle]
pub extern "C" fn PyUnicodeWriter_Discard(writer: *mut c_void) {
    if !writer.is_null() {
        // SAFETY: see `unicode_writer`.
        drop(unsafe { Box::from_raw(writer as *mut UnicodeWriter) });
    }
}

/// `PyUnicodeWriter_Finish`: returns the accumulated string, consuming writer.
#[no_mangle]
pub extern "C" fn PyUnicodeWriter_Finish(writer: *mut c_void) -> *mut ShimObject {
    if writer.is_null() {
        set_error("SystemError: NULL PyUnicodeWriter");
        return ptr::null_mut();
    }
    // SAFETY: see `unicode_writer`; Finish consumes the box.
    let mut boxed = unsafe { Box::from_raw(writer as *mut UnicodeWriter) };
    let text = std::mem::take(&mut boxed.text);
    new_str(text)
}

/// `PyUnicodeWriter_WriteChar`.
#[no_mangle]
pub extern "C" fn PyUnicodeWriter_WriteChar(writer: *mut c_void, character: c_uint) -> c_int {
    let Some(writer) = unicode_writer(writer) else {
        return -1;
    };
    match char::from_u32(character) {
        Some(character) => {
            writer.text.push(character);
            0
        }
        None => {
            set_error(format!(
                "ValueError: {character:#x} is not a valid Unicode code point"
            ));
            -1
        }
    }
}

/// `PyUnicodeWriter_WriteUTF8`.
#[no_mangle]
pub extern "C" fn PyUnicodeWriter_WriteUTF8(
    writer: *mut c_void,
    data: *const c_char,
    length: isize,
) -> c_int {
    let Some(writer) = unicode_writer(writer) else {
        return -1;
    };
    let Ok(length) = usize::try_from(length) else {
        set_error("ValueError: negative string length");
        return -1;
    };
    if data.is_null() && length != 0 {
        set_error("SystemError: NULL string with a non-zero length");
        return -1;
    }
    // SAFETY: the caller supplies `length` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, length) };
    match std::str::from_utf8(bytes) {
        Ok(text) => {
            writer.text.push_str(text);
            0
        }
        Err(_) => {
            set_error("UnicodeDecodeError: invalid UTF-8 string");
            -1
        }
    }
}

/// `PyUnicodeWriter_WriteASCII`.
#[no_mangle]
pub extern "C" fn PyUnicodeWriter_WriteASCII(
    writer: *mut c_void,
    data: *const c_char,
    length: isize,
) -> c_int {
    let Some(writer) = unicode_writer(writer) else {
        return -1;
    };
    let Ok(length) = usize::try_from(length) else {
        set_error("ValueError: negative string length");
        return -1;
    };
    if data.is_null() && length != 0 {
        set_error("SystemError: NULL string with a non-zero length");
        return -1;
    }
    // SAFETY: the caller supplies `length` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, length) };
    if !bytes.is_ascii() {
        set_error("UnicodeEncodeError: string is not ASCII");
        return -1;
    }
    // SAFETY: ASCII bytes are valid UTF-8.
    writer
        .text
        .push_str(unsafe { std::str::from_utf8_unchecked(bytes) });
    0
}

/// `PyUnicodeWriter_WriteWideChar` (`wchar_t` is a signed 32-bit value here).
#[no_mangle]
pub extern "C" fn PyUnicodeWriter_WriteWideChar(
    writer: *mut c_void,
    data: *const i32,
    length: isize,
) -> c_int {
    let Some(writer) = unicode_writer(writer) else {
        return -1;
    };
    let Ok(length) = usize::try_from(length) else {
        set_error("ValueError: negative string length");
        return -1;
    };
    if data.is_null() && length != 0 {
        set_error("SystemError: NULL string with a non-zero length");
        return -1;
    }
    for index in 0..length {
        // SAFETY: the caller supplies `length` values.
        let value = unsafe { *data.add(index) };
        match u32::try_from(value).ok().and_then(char::from_u32) {
            Some(character) => writer.text.push(character),
            None => {
                set_error(format!(
                    "ValueError: {value:#x} is not a valid Unicode code point"
                ));
                return -1;
            }
        }
    }
    0
}

/// `PyUnicodeWriter_WriteUCS4`.
#[no_mangle]
pub extern "C" fn PyUnicodeWriter_WriteUCS4(
    writer: *mut c_void,
    data: *const c_uint,
    length: isize,
) -> c_int {
    let Some(writer) = unicode_writer(writer) else {
        return -1;
    };
    let Ok(length) = usize::try_from(length) else {
        set_error("ValueError: negative string length");
        return -1;
    };
    if data.is_null() && length != 0 {
        set_error("SystemError: NULL string with a non-zero length");
        return -1;
    }
    for index in 0..length {
        // SAFETY: the caller supplies `length` values.
        let value = unsafe { *data.add(index) };
        match char::from_u32(value) {
            Some(character) => writer.text.push(character),
            None => {
                set_error(format!(
                    "ValueError: {value:#x} is not a valid Unicode code point"
                ));
                return -1;
            }
        }
    }
    0
}

/// `PyUnicodeWriter_WriteStr`.
#[no_mangle]
pub extern "C" fn PyUnicodeWriter_WriteStr(writer: *mut c_void, object: *mut ShimObject) -> c_int {
    let Some(writer) = unicode_writer(writer) else {
        return -1;
    };
    match as_string(object) {
        Some(text) => {
            writer.text.push_str(&text);
            0
        }
        None => {
            set_error("TypeError: PyUnicodeWriter_WriteStr requires a str");
            -1
        }
    }
}

/// `PyUnicodeWriter_WriteRepr`.
#[no_mangle]
pub extern "C" fn PyUnicodeWriter_WriteRepr(writer: *mut c_void, object: *mut ShimObject) -> c_int {
    let Some(writer) = unicode_writer(writer) else {
        return -1;
    };
    let repr = PyObject_Repr(object);
    if repr.is_null() {
        return -1;
    }
    let outcome = as_string(repr);
    dec_ref(repr);
    match outcome {
        Some(text) => {
            writer.text.push_str(&text);
            0
        }
        None => {
            set_error("TypeError: repr() did not return a str");
            -1
        }
    }
}

/// `PyUnicodeWriter_WriteSubstring`.
#[no_mangle]
pub extern "C" fn PyUnicodeWriter_WriteSubstring(
    writer: *mut c_void,
    object: *mut ShimObject,
    start: isize,
    end: isize,
) -> c_int {
    let Some(writer) = unicode_writer(writer) else {
        return -1;
    };
    let Some(text) = as_string(object) else {
        set_error("TypeError: PyUnicodeWriter_WriteSubstring requires a str");
        return -1;
    };
    let characters: Vec<char> = text.chars().collect();
    let length = characters.len() as isize;
    if start < 0 || end < start || end > length {
        set_error(format!(
            "SystemError: invalid substring bounds {start}:{end} for a string of length {length}"
        ));
        return -1;
    }
    writer
        .text
        .extend(&characters[start as usize..end as usize]);
    0
}

fn decode_error(error: &std::str::Utf8Error) -> String {
    format!(
        "UnicodeDecodeError: invalid UTF-8 at byte {}: {}",
        error.valid_up_to(),
        error
    )
}

/// `PyUnicodeWriter_DecodeUTF8Stateful`.
#[no_mangle]
pub extern "C" fn PyUnicodeWriter_DecodeUTF8Stateful(
    writer: *mut c_void,
    data: *const c_char,
    length: isize,
    errors: *const c_char,
    consumed: *mut isize,
) -> c_int {
    let Some(writer) = unicode_writer(writer) else {
        return -1;
    };
    let Ok(length) = usize::try_from(length) else {
        set_error("ValueError: negative string length");
        return -1;
    };
    if data.is_null() && length != 0 {
        set_error("SystemError: NULL string with a non-zero length");
        return -1;
    }
    let error_mode = if errors.is_null() {
        "strict".to_string()
    } else {
        read_c_string(errors)
    };
    // SAFETY: the caller supplies `length` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, length) };
    let result = match error_mode.as_str() {
        "strict" => std::str::from_utf8(bytes)
            .map(|text| text.to_string())
            .map_err(|error| decode_error(&error)),
        "ignore" => Ok(String::from_utf8_lossy(bytes)
            .chars()
            .filter(|character| *character != '\u{FFFD}')
            .collect::<String>()),
        "replace" => Ok(String::from_utf8_lossy(bytes).into_owned()),
        other => Err(format!("LookupError: unknown error handler name '{other}'")),
    };
    match result {
        Ok(text) => {
            writer.text.push_str(&text);
            if !consumed.is_null() {
                // SAFETY: the caller passes a writable Py_ssize_t.
                unsafe { *consumed = length as isize };
            }
            0
        }
        Err(error) => {
            if !consumed.is_null() {
                // SAFETY: the caller passes a writable Py_ssize_t.
                unsafe { *consumed = 0 };
            }
            set_error(error);
            -1
        }
    }
}

// -- long writer ------------------------------------------------------------

/// Opaque `PyLongWriter` (CPython 3.14 declares it opaque).
struct LongWriter {
    negative: bool,
    digits: Vec<u32>,
}

/// `PyLongWriter_Create`: exposes a zeroed base-2^30 digit buffer.
#[no_mangle]
pub extern "C" fn PyLongWriter_Create(
    negative: c_int,
    ndigits: isize,
    digits: *mut *mut c_void,
) -> *mut c_void {
    let Ok(count) = usize::try_from(ndigits) else {
        set_error("ValueError: negative digit count");
        return ptr::null_mut();
    };
    let mut vector: Vec<u32> = Vec::new();
    if vector.try_reserve_exact(count).is_err() {
        set_error("MemoryError: cannot allocate long digits");
        return ptr::null_mut();
    }
    vector.resize(count, 0);
    if !digits.is_null() {
        // SAFETY: the caller supplies a writable `void *` output slot.
        unsafe { *digits = vector.as_mut_ptr() as *mut c_void };
    }
    Box::into_raw(Box::new(LongWriter {
        negative: negative != 0,
        digits: vector,
    })) as *mut c_void
}

/// `PyLongWriter_Finish`: builds the integer, consuming the writer.
#[no_mangle]
pub extern "C" fn PyLongWriter_Finish(writer: *mut c_void) -> *mut ShimObject {
    if writer.is_null() {
        set_error("SystemError: NULL PyLongWriter");
        return ptr::null_mut();
    }
    // SAFETY: the pointer came from `PyLongWriter_Create`.
    let boxed = unsafe { Box::from_raw(writer as *mut LongWriter) };
    let mut magnitude: u128 = 0;
    // CPython stores digits little-endian, 30 bits each on this platform.
    for &digit in boxed.digits.iter().rev() {
        magnitude = match magnitude
            .checked_mul(1 << 30)
            .and_then(|value| value.checked_add(digit as u128))
        {
            Some(value) => value,
            None => {
                set_error(
                    "OverflowError: PyLongWriter result does not fit in a Python int on this host",
                );
                return ptr::null_mut();
            }
        };
        if magnitude > (i64::MAX as u128) + 1 {
            set_error(
                "OverflowError: PyLongWriter result does not fit in a Python int on this host",
            );
            return ptr::null_mut();
        }
    }
    let number: i128 = if boxed.negative {
        -(magnitude as i128)
    } else {
        magnitude as i128
    };
    match i64::try_from(number) {
        Ok(value) => new_int(value),
        Err(_) => {
            set_error(
                "OverflowError: PyLongWriter result does not fit in a Python int on this host",
            );
            ptr::null_mut()
        }
    }
}

/// `PyLongWriter_Discard`.
#[no_mangle]
pub extern "C" fn PyLongWriter_Discard(writer: *mut c_void) {
    if !writer.is_null() {
        // SAFETY: see `PyLongWriter_Finish`.
        drop(unsafe { Box::from_raw(writer as *mut LongWriter) });
    }
}

// -- PyMutex ----------------------------------------------------------------

/// `PyMutex_Lock`: a real mutual-exclusion lock on CPython's one-byte layout.
#[no_mangle]
pub extern "C" fn PyMutex_Lock(mutex: *mut u8) {
    if mutex.is_null() {
        return;
    }
    // SAFETY: `PyMutex` is exactly one byte; the caller owns it.
    let atomic = unsafe { &*(mutex as *const std::sync::atomic::AtomicU8) };
    loop {
        if atomic
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        std::thread::yield_now();
    }
}

/// `PyMutex_Unlock`.
#[no_mangle]
pub extern "C" fn PyMutex_Unlock(mutex: *mut u8) {
    if mutex.is_null() {
        return;
    }
    // SAFETY: `PyMutex` is exactly one byte; the caller owns it.
    let atomic = unsafe { &*(mutex as *const std::sync::atomic::AtomicU8) };
    atomic.store(0, Ordering::Release);
}

/// `PyMutex_IsLocked`.
#[no_mangle]
pub extern "C" fn PyMutex_IsLocked(mutex: *mut u8) -> c_int {
    if mutex.is_null() {
        return 0;
    }
    // SAFETY: as above.
    let atomic = unsafe { &*(mutex as *const std::sync::atomic::AtomicU8) };
    (atomic.load(Ordering::Relaxed) != 0) as c_int
}

// -- type helpers -----------------------------------------------------------

/// `PyType_GetModuleName`: the module component of `tp_name`.
#[no_mangle]
pub extern "C" fn PyType_GetModuleName(type_object: *mut ShimObject) -> *mut ShimObject {
    if let Some(module) = hydro_type_module_text(type_object) {
        return value_object(ShimValue::Str(module));
    }
    if hydro_tagged(type_object).is_some() {
        set_error("TypeError: PyType_GetModuleName: expected a class object");
        return ptr::null_mut();
    }
    match types::as_type(type_object) {
        Ok(type_object) => {
            let name = types::type_name(type_object);
            let module = name
                .rsplit_once('.')
                .map(|(module, _)| module)
                .unwrap_or("builtins");
            new_str(module.to_string())
        }
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyType_FromModuleAndSpec`.
///
/// The module is validated and associated with the new heap type, so
/// `PyType_GetModule`/`PyType_GetModuleState` return the module registered by
/// `PyModuleDef_Init` instead of a stale or fabricated pointer.
#[no_mangle]
pub extern "C" fn PyType_FromModuleAndSpec(
    module: *mut ShimObject,
    spec: *mut types::PyTypeSpecMirror,
    bases: *mut ShimObject,
) -> *mut ShimObject {
    if !module.is_null() && module_entry_pointer(module, |_| ()).is_err() {
        set_error("TypeError: PyType_FromModuleAndSpec: module is not a module object");
        return ptr::null_mut();
    }
    let type_object = PyType_FromSpecWithBases(spec, bases);
    if type_object.is_null() || module.is_null() {
        return type_object;
    }
    types::associate_type_module(type_object as *mut TypeObject, module);
    type_object
}

/// `PyType_GetModule`: the module associated with a heap type.
///
/// Returns a borrowed reference, as in CPython. A static type, a type created
/// by plain `PyType_FromSpec`, or a type whose module association was never
/// filled is rejected with `TypeError`; no garbage pointer is returned.
#[no_mangle]
pub extern "C" fn PyType_GetModule(type_object: *mut ShimObject) -> *mut ShimObject {
    let type_object = match types::as_type(type_object) {
        Ok(type_object) => type_object,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    // SAFETY: validated, live type object.
    if unsafe { (*type_object).tp_flags } & PY_TPFLAGS_HEAPTYPE == 0 {
        set_error("TypeError: PyType_GetModule: type is not a heap type");
        return ptr::null_mut();
    }
    match types::type_module(type_object) {
        Some(module) => module,
        None => {
            set_error("TypeError: PyType_GetModule: type has no module");
            ptr::null_mut()
        }
    }
}

/// `PyType_GetModuleState`: the module state block of a heap type's module.
///
/// Mirrors CPython: a type without an associated module yields NULL with the
/// `TypeError` from [`PyType_GetModule`], and a module whose
/// `PyModuleDef.m_size` is zero yields NULL without an exception. No
/// arbitrary address is ever returned.
#[no_mangle]
pub extern "C" fn PyType_GetModuleState(type_object: *mut ShimObject) -> *mut c_void {
    let module = PyType_GetModule(type_object);
    if module.is_null() {
        // `PyType_GetModule` already set the diagnostic.
        return ptr::null_mut();
    }
    PyModule_GetState(module)
}

/// `PyType_FromMetaclass`.
#[no_mangle]
pub extern "C" fn PyType_FromMetaclass(
    metaclass: *mut ShimObject,
    _module: *mut ShimObject,
    spec: *mut types::PyTypeSpecMirror,
    bases: *mut ShimObject,
) -> *mut ShimObject {
    if !metaclass.is_null() && types::is_type_pointer(metaclass) {
        let name = types::type_name(metaclass as *mut TypeObject);
        if name != "type" {
            set_error(format!(
                "TypeError: PyType_FromMetaclass: metaclass '{name}' is not supported"
            ));
            return ptr::null_mut();
        }
    }
    PyType_FromSpecWithBases(spec, bases)
}

// ---------------------------------------------------------------------------
// Scalar, container and capsule API

// ---------------------------------------------------------------------------

/// `PyLong_FromLong`.
///
/// `c_long` is `i64` on LP64 and `i32` elsewhere; the widening cast is a no-op
/// on x86_64 but required for the 32-bit targets. `i64::from` would instead
/// trip the identity-conversion lint on x86_64.
#[allow(clippy::unnecessary_cast)]
#[no_mangle]
pub extern "C" fn PyLong_FromLong(value: c_long) -> *mut ShimObject {
    new_int(value as i64)
}

/// `PyLong_AsLong`.
#[no_mangle]
pub extern "C" fn PyLong_AsLong(object: *mut ShimObject) -> c_long {
    match as_int(object)
        .and_then(|value| c_long::try_from(value).map_err(|_| "integer out of range".to_string()))
    {
        Ok(value) => value,
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

fn non_negative_size(size: isize, what: &str) -> Result<usize, String> {
    usize::try_from(size).map_err(|_| format!("ValueError: negative {what} size"))
}

fn py_ssize_of(len: usize) -> isize {
    isize::try_from(len).unwrap_or(isize::MAX)
}

/// `PyUnicode_FromString`.
#[no_mangle]
pub extern "C" fn PyUnicode_FromString(value: *const c_char) -> *mut ShimObject {
    if value.is_null() {
        set_error("TypeError: PyUnicode_FromString requires a non-NULL string");
        return ptr::null_mut();
    }
    new_str(read_c_string(value))
}

/// `PyUnicode_AsUTF8`.
#[no_mangle]
pub extern "C" fn PyUnicode_AsUTF8(object: *mut ShimObject) -> *const c_char {
    match as_c_string(object) {
        Ok(pointer) => pointer,
        Err(error) => {
            set_error(format!("TypeError: {error}"));
            ptr::null()
        }
    }
}

/// `PyFloat_FromDouble`.
#[no_mangle]
pub extern "C" fn PyFloat_FromDouble(value: f64) -> *mut ShimObject {
    new_object(ShimKind::Float(value))
}

/// `PyFloat_AsDouble`.
#[no_mangle]
pub extern "C" fn PyFloat_AsDouble(object: *mut ShimObject) -> f64 {
    match as_float(object) {
        Ok(value) => value,
        Err(error) => {
            set_error(format!("TypeError: {error}"));
            -1.0
        }
    }
}

/// `PyBytes_FromStringAndSize`.
#[no_mangle]
pub extern "C" fn PyBytes_FromStringAndSize(
    value: *const c_char,
    length: isize,
) -> *mut ShimObject {
    let length = match non_negative_size(length, "bytes") {
        Ok(length) => length,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    if value.is_null() && length != 0 {
        set_error("TypeError: NULL bytes buffer with a non-zero length");
        return ptr::null_mut();
    }
    if length == 0 {
        return new_bytes(Vec::new());
    }
    let mut data = Vec::new();
    if data.try_reserve_exact(length).is_err() {
        set_error("MemoryError: cannot allocate bytes");
        return ptr::null_mut();
    }
    // SAFETY: the caller promises `value` points at `length` readable bytes.
    let source = unsafe { std::slice::from_raw_parts(value as *const u8, length) };
    data.extend_from_slice(source);
    new_bytes(data)
}

/// `PyBytes_AsString`.
#[no_mangle]
pub extern "C" fn PyBytes_AsString(object: *mut ShimObject) -> *mut c_char {
    match bytes_c_data(object) {
        Ok(data) => data.as_ptr() as *mut c_char,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyBytes_Size`.
#[no_mangle]
pub extern "C" fn PyBytes_Size(object: *mut ShimObject) -> isize {
    match bytes_data(object) {
        Ok(data) => py_ssize_of(data.len()),
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

fn capsule_fields(
    object: *mut ShimObject,
) -> Result<(usize, Option<&'static CString>, usize, usize), String> {
    match kind_ref(object) {
        Some(ShimKind::Capsule {
            ptr,
            name,
            context,
            destructor,
        }) => Ok((*ptr, name.as_ref(), *context, *destructor)),
        Some(_) => Err("TypeError: a capsule object is required".to_string()),
        None => Err("TypeError: a capsule object is required, got NULL".to_string()),
    }
}

/// `PyCapsule_New`.
///
/// The destructor now runs when the capsule is freed, as in CPython.
#[no_mangle]
pub extern "C" fn PyCapsule_New(
    pointer: *mut c_void,
    name: *const c_char,
    destructor: *mut c_void,
) -> *mut ShimObject {
    if pointer.is_null() {
        set_error("ValueError: PyCapsule_New called with a NULL pointer");
        return ptr::null_mut();
    }
    let name = if name.is_null() {
        None
    } else {
        CString::new(read_c_string(name)).ok()
    };
    new_object(ShimKind::Capsule {
        ptr: pointer as usize,
        name,
        context: 0,
        destructor: destructor as usize,
    })
}

/// `PyCapsule_GetPointer`.
#[no_mangle]
pub extern "C" fn PyCapsule_GetPointer(
    capsule: *mut ShimObject,
    name: *const c_char,
) -> *mut c_void {
    let (pointer, stored, _, _) = match capsule_fields(capsule) {
        Ok(fields) => fields,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let problem = match (name.is_null(), stored) {
        (true, None) => None,
        (true, Some(_)) => Some("PyCapsule_GetPointer called with no name, but capsule has a name"),
        (false, None) => Some("PyCapsule_GetPointer called with a name, but capsule has no name"),
        (false, Some(stored)) => {
            // SAFETY: the CPython contract requires a NUL-terminated name.
            let requested = unsafe { CStr::from_ptr(name) };
            (requested != stored.as_c_str())
                .then_some("PyCapsule_GetPointer called with an incorrect name")
        }
    };
    if let Some(problem) = problem {
        set_error(format!("ValueError: {problem}"));
        return ptr::null_mut();
    }
    if pointer == 0 {
        set_error("ValueError: capsule pointer is NULL");
        return ptr::null_mut();
    }
    pointer as *mut c_void
}

/// `PyCapsule_GetName`.
#[no_mangle]
pub extern "C" fn PyCapsule_GetName(capsule: *mut ShimObject) -> *const c_char {
    match capsule_fields(capsule) {
        Ok((_, Some(name), _, _)) => name.as_ptr(),
        Ok((_, None, _, _)) => ptr::null(),
        Err(error) => {
            set_error(error);
            ptr::null()
        }
    }
}

/// `PyCapsule_IsValid`.
#[no_mangle]
pub extern "C" fn PyCapsule_IsValid(capsule: *mut ShimObject, name: *const c_char) -> c_int {
    let Ok((pointer, stored, _, _)) = capsule_fields(capsule) else {
        return 0;
    };
    if pointer == 0 {
        return 0;
    }
    let matches = match (name.is_null(), stored) {
        (true, None) => true,
        (false, Some(stored)) => {
            // SAFETY: the CPython contract requires a NUL-terminated name.
            let requested = unsafe { CStr::from_ptr(name) };
            requested == stored.as_c_str()
        }
        _ => false,
    };
    matches as c_int
}

/// `PyCapsule_SetPointer`.
#[no_mangle]
pub extern "C" fn PyCapsule_SetPointer(capsule: *mut ShimObject, pointer: *mut c_void) -> c_int {
    if pointer.is_null() {
        set_error("ValueError: PyCapsule_SetPointer called with a NULL pointer");
        return -1;
    }
    if capsule.is_null() {
        set_error("TypeError: a capsule object is required, got NULL");
        return -1;
    }
    // SAFETY: shim objects are heap-stable.
    match unsafe { kind_mut(capsule) } {
        ShimKind::Capsule { ptr, .. } => {
            *ptr = pointer as usize;
            0
        }
        _ => {
            set_error("TypeError: a capsule object is required");
            -1
        }
    }
}

/// `PyList_New`.
#[no_mangle]
pub extern "C" fn PyList_New(size: isize) -> *mut ShimObject {
    let count = match non_negative_size(size, "list") {
        Ok(count) => count,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let mut items = Vec::new();
    if items.try_reserve_exact(count).is_err() {
        set_error("MemoryError: cannot allocate list");
        return ptr::null_mut();
    }
    items.resize(count, 0);
    new_object(ShimKind::List(items))
}

/// `PyList_Size`.
#[no_mangle]
pub extern "C" fn PyList_Size(object: *mut ShimObject) -> isize {
    match list_slots(object) {
        Ok(items) => py_ssize_of(items.len()),
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

/// `PyList_GetItem`.
#[no_mangle]
pub extern "C" fn PyList_GetItem(object: *mut ShimObject, index: isize) -> *mut ShimObject {
    let items = match list_slots(object) {
        Ok(items) => items,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    match usize::try_from(index)
        .ok()
        .and_then(|index| items.get(index))
    {
        Some(&item) if item != 0 => item as *mut ShimObject,
        Some(_) => {
            set_error("SystemError: list contains an uninitialized item");
            ptr::null_mut()
        }
        None => {
            set_error("IndexError: list index out of range");
            ptr::null_mut()
        }
    }
}

/// `PyList_SetItem`: steals the item reference.
#[no_mangle]
pub extern "C" fn PyList_SetItem(
    object: *mut ShimObject,
    index: isize,
    item: *mut ShimObject,
) -> c_int {
    if item.is_null() {
        set_error("TypeError: list item is NULL");
        return -1;
    }
    let previous = {
        let items = match list_slots_mut(object) {
            Ok(items) => items,
            Err(error) => {
                set_error(error);
                return -1;
            }
        };
        match usize::try_from(index)
            .ok()
            .and_then(|index| items.get_mut(index))
        {
            Some(slot) => {
                let previous = *slot;
                *slot = item as usize;
                previous
            }
            None => {
                set_error("IndexError: list assignment index out of range");
                return -1;
            }
        }
    };
    if previous != 0 {
        dec_ref(previous as *mut ShimObject);
    }
    0
}

/// `PyList_Append`: appends one item, taking a new reference.
#[no_mangle]
pub extern "C" fn PyList_Append(object: *mut ShimObject, item: *mut ShimObject) -> c_int {
    if item.is_null() {
        set_error("TypeError: list item is NULL");
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
    inc_ref(item);
    items.push(item as usize);
    sync_list_prefix(object);
    0
}

/// `PyTuple_New`.
#[no_mangle]
pub extern "C" fn PyTuple_New(size: isize) -> *mut ShimObject {
    let count = match non_negative_size(size, "tuple") {
        Ok(count) => count,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    if count == 0 {
        return empty_tuple();
    }
    let mut items = Vec::new();
    if items.try_reserve_exact(count).is_err() {
        set_error("MemoryError: cannot allocate tuple");
        return ptr::null_mut();
    }
    items.resize(count, 0);
    new_tuple(items)
}

/// `PyTuple_Size`.
#[no_mangle]
pub extern "C" fn PyTuple_Size(object: *mut ShimObject) -> isize {
    match tuple_slots(object) {
        Ok(items) => py_ssize_of(items.len()),
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

/// `PyTuple_GetItem`.
#[no_mangle]
pub extern "C" fn PyTuple_GetItem(object: *mut ShimObject, index: isize) -> *mut ShimObject {
    let items = match tuple_slots(object) {
        Ok(items) => items,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    match usize::try_from(index)
        .ok()
        .and_then(|index| items.get(index))
    {
        Some(&item) if item != 0 => item as *mut ShimObject,
        Some(_) => {
            set_error("SystemError: tuple contains an uninitialized item");
            ptr::null_mut()
        }
        None => {
            set_error("IndexError: tuple index out of range");
            ptr::null_mut()
        }
    }
}

/// `PyTuple_SetItem`: steals the item reference.
#[no_mangle]
pub extern "C" fn PyTuple_SetItem(
    object: *mut ShimObject,
    index: isize,
    item: *mut ShimObject,
) -> c_int {
    if item.is_null() {
        set_error("TypeError: tuple item is NULL");
        return -1;
    }
    let previous = {
        let items = match tuple_slots_mut(object) {
            Ok(items) => items,
            Err(error) => {
                set_error(error);
                return -1;
            }
        };
        match usize::try_from(index)
            .ok()
            .and_then(|index| items.get_mut(index))
        {
            Some(slot) => {
                let previous = *slot;
                *slot = item as usize;
                previous
            }
            None => {
                set_error("IndexError: tuple assignment index out of range");
                return -1;
            }
        }
    };
    if previous != 0 {
        dec_ref(previous as *mut ShimObject);
    }
    0
}

/// `PyDict_New`.
#[no_mangle]
pub extern "C" fn PyDict_New() -> *mut ShimObject {
    new_object(ShimKind::Dict(Vec::new()))
}

/// `PyDict_SetItem`.
#[no_mangle]
pub extern "C" fn PyDict_SetItem(
    object: *mut ShimObject,
    key: *mut ShimObject,
    value: *mut ShimObject,
) -> c_int {
    if key.is_null() {
        set_error("TypeError: dict key is NULL");
        return -1;
    }
    if value.is_null() {
        set_error("TypeError: dict value is NULL");
        return -1;
    }
    match dict_set(object, key, value) {
        Ok(()) => 0,
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

/// `PyDict_SetItemString`.
#[no_mangle]
pub extern "C" fn PyDict_SetItemString(
    object: *mut ShimObject,
    key: *const c_char,
    value: *mut ShimObject,
) -> c_int {
    if key.is_null() {
        set_error("TypeError: dict key is NULL");
        return -1;
    }
    let key = new_str(read_c_string(key));
    let status = match dict_set(object, key, value) {
        Ok(()) => 0,
        Err(error) => {
            set_error(error);
            -1
        }
    };
    dec_ref(key);
    status
}

/// `PyDict_GetItem`.
#[no_mangle]
pub extern "C" fn PyDict_GetItem(object: *mut ShimObject, key: *mut ShimObject) -> *mut ShimObject {
    if key.is_null() {
        set_error("TypeError: dict key is NULL");
        return ptr::null_mut();
    }
    let pairs = match dict_slots(object) {
        Ok(pairs) => pairs,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    match dict_position(pairs, key) {
        Some(index) => pairs[index].1 as *mut ShimObject,
        None => ptr::null_mut(),
    }
}

/// `PyDict_GetItemString`.
#[no_mangle]
pub extern "C" fn PyDict_GetItemString(
    object: *mut ShimObject,
    key: *const c_char,
) -> *mut ShimObject {
    if key.is_null() {
        set_error("TypeError: dict key is NULL");
        return ptr::null_mut();
    }
    let name = read_c_string(key);
    let pairs = match dict_slots(object) {
        Ok(pairs) => pairs,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let position = pairs.iter().position(|&(existing, _)| {
        matches!(
            kind_ref(existing as *mut ShimObject),
            Some(ShimKind::Str { text, .. }) if text == &name
        )
    });
    match position {
        Some(index) => pairs[index].1 as *mut ShimObject,
        None => ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
// Error state API
// ---------------------------------------------------------------------------

/// `PyErr_Occurred`.
#[no_mangle]
pub extern "C" fn PyErr_Occurred() -> *mut ShimObject {
    if !error_pending() {
        return ptr::null_mut();
    }
    static SENTINEL: LazyLock<usize> = LazyLock::new(|| new_object(ShimKind::Error) as usize);
    *SENTINEL as *mut ShimObject
}

/// `PyErr_SetString`.
#[no_mangle]
pub extern "C" fn PyErr_SetString(exception: *mut ShimObject, message: *const c_char) {
    let text = if message.is_null() {
        "extension error".to_string()
    } else {
        read_c_string(message)
    };
    set_error(format!("{}: {text}", exception_prefix(exception)));
}

/// `PyErr_SetNone`.
#[no_mangle]
pub extern "C" fn PyErr_SetNone(exception: *mut ShimObject) {
    set_error(format!("{}: ", exception_prefix(exception)));
}

/// `PyErr_SetObject`.
#[no_mangle]
pub extern "C" fn PyErr_SetObject(exception: *mut ShimObject, value: *mut ShimObject) {
    let text = match kind_ref(value) {
        Some(ShimKind::Str { text, .. }) => text.clone(),
        _ => shim_repr(value),
    };
    let message = format!("{}: {text}", exception_prefix(exception));
    // CPython normalizes an instance value into the raised exception; keep the
    // instance so the Hydro raise path can preserve the original object.
    if object_is_exception(value) {
        inc_ref(value);
        replace_error_state(Some(message), Some(value as usize));
    } else {
        set_error(message);
    }
}

/// `PyErr_NoMemory`.
#[no_mangle]
pub extern "C" fn PyErr_NoMemory() -> *mut ShimObject {
    set_error("MemoryError: out of memory");
    ptr::null_mut()
}

/// `PyErr_BadArgument`.
#[no_mangle]
pub extern "C" fn PyErr_BadArgument() -> c_int {
    set_error("TypeError: bad argument to internal function");
    0
}

/// `PyErr_BadInternalCall`.
#[no_mangle]
pub extern "C" fn PyErr_BadInternalCall() {
    set_error("SystemError: bad argument to internal function");
}

/// `PyErr_SetFromErrno`.
#[no_mangle]
pub extern "C" fn PyErr_SetFromErrno(exception: *mut ShimObject) -> *mut ShimObject {
    let prefix = if exception.is_null() {
        "OSError".to_string()
    } else {
        exception_prefix(exception)
    };
    let error = std::io::Error::last_os_error();
    set_error(format!("{prefix}: {error}"));
    ptr::null_mut()
}

/// Records a message formatted by `PyErr_Format` in `parse.c`.
#[no_mangle]
pub extern "C" fn hydro_capi_set_exception_message(
    exception: *mut ShimObject,
    message: *const c_char,
) {
    let text = if message.is_null() {
        String::new()
    } else {
        read_c_string(message)
    };
    set_error(format!("{}: {text}", exception_prefix(exception)));
}

/// `PyErr_Clear`.
#[no_mangle]
pub extern "C" fn PyErr_Clear() {
    clear_error();
}

// ---------------------------------------------------------------------------
// Call machinery
// ---------------------------------------------------------------------------

type VectorcallFunc = unsafe extern "C" fn(
    *mut ShimObject,
    *const *mut ShimObject,
    usize,
    *mut ShimObject,
) -> *mut ShimObject;
type TernaryFunc =
    unsafe extern "C" fn(*mut ShimObject, *mut ShimObject, *mut ShimObject) -> *mut ShimObject;
type InitProc = unsafe extern "C" fn(*mut ShimObject, *mut ShimObject, *mut ShimObject) -> c_int;

fn vectorcall_nargs(nargsf: usize) -> usize {
    nargsf & !PY_VECTORCALL_ARGUMENTS_OFFSET
}

fn object_slice(
    args: *const *mut ShimObject,
    nargs: usize,
) -> Result<&'static [*mut ShimObject], String> {
    if nargs == 0 {
        return Ok(&[]);
    }
    if args.is_null() {
        return Err("SystemError: NULL argument vector".to_string());
    }
    if nargs > (isize::MAX as usize) / std::mem::size_of::<*mut ShimObject>() {
        return Err("SystemError: argument vector is too large".to_string());
    }
    // SAFETY: the vectorcall contract provides `nargs` readable pointers.
    Ok(unsafe { std::slice::from_raw_parts(args, nargs) })
}

/// Validates a vectorcall keyword-name tuple and returns its entries.
fn keyword_names(kwnames: *mut ShimObject) -> Result<&'static [usize], String> {
    if kwnames.is_null() {
        return Ok(&[]);
    }
    let items = tuple_slots(kwnames)?;
    for &item in items {
        if !matches!(
            kind_ref(item as *mut ShimObject),
            Some(ShimKind::Str { .. })
        ) {
            return Err("TypeError: keywords must be strings".to_string());
        }
    }
    Ok(items)
}

/// Builds a fresh tuple from borrowed object pointers, taking new references.
fn tuple_from_borrowed(values: &[*mut ShimObject]) -> *mut ShimObject {
    if values.is_empty() {
        return empty_tuple();
    }
    for &value in values {
        inc_ref(value);
    }
    new_tuple(values.iter().map(|&value| value as usize).collect())
}

/// Builds a keyword dict from a name tuple and the matching vector values.
fn build_kwargs_dict(
    kwnames: *mut ShimObject,
    values: &[*mut ShimObject],
) -> Result<*mut ShimObject, String> {
    let names = keyword_names(kwnames)?;
    if names.len() != values.len() {
        return Err("SystemError: keyword count does not match the argument vector".to_string());
    }
    if names.is_empty() {
        return Ok(ptr::null_mut());
    }
    let dict = PyDict_New();
    for (&name, &value) in names.iter().zip(values.iter()) {
        let text = as_string(name as *mut ShimObject)
            .ok_or_else(|| "TypeError: keywords must be strings".to_string())?;
        let key = new_str(text);
        if PyDict_SetItem(dict, key, value) != 0 {
            dec_ref(key);
            dec_ref(dict);
            return Err(take_error().unwrap_or_else(|| "TypeError: cannot build keywords".into()));
        }
        dec_ref(key);
    }
    Ok(dict)
}

/// Dispatches one `PyMethodDef`-style function.
///
/// `values` is the complete vectorcall value vector (positional arguments
/// followed by keyword values) and `kwnames` the matching tuple of keyword
/// names (or NULL). The return value is a new reference, or NULL with a
/// pending error.
fn invoke_method(
    function: usize,
    flags: c_int,
    self_object: *mut ShimObject,
    name: &str,
    values: &[*mut ShimObject],
    kwnames: *mut ShimObject,
) -> *mut ShimObject {
    if function == 0 {
        set_error(format!("SystemError: {name}() has a NULL implementation"));
        return ptr::null_mut();
    }
    if flags & METH_CLASS != 0 && flags & METH_STATIC != 0 {
        set_error(format!(
            "ValueError: {name}() cannot be both class and static"
        ));
        return ptr::null_mut();
    }
    if flags & !(METHOD_CALL_FLAGS | METHOD_BINDING_FLAGS) != 0 {
        set_error(format!(
            "TypeError: {name}() uses unsupported METH flags {flags:#x}"
        ));
        return ptr::null_mut();
    }
    let signature = flags & (METH_VARARGS | METH_NOARGS | METH_O | METH_FASTCALL);
    if signature.count_ones() > 1 {
        set_error(format!(
            "TypeError: {name}() combines incompatible METH flags {flags:#x}"
        ));
        return ptr::null_mut();
    }
    if signature == 0 {
        set_error(format!(
            "TypeError: {name}() uses unsupported METH flags {flags:#x}"
        ));
        return ptr::null_mut();
    }
    let kw_names = match keyword_names(kwnames) {
        Ok(names) => names,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let kwcount = kw_names.len();
    if kwcount > 0 && flags & METH_KEYWORDS == 0 {
        set_error(format!("TypeError: {name}() takes no keyword arguments"));
        return ptr::null_mut();
    }
    if kwcount > values.len() {
        set_error(format!(
            "SystemError: {name}() received more keyword names than arguments"
        ));
        return ptr::null_mut();
    }
    // CPython's vectorcall layout: `values[..positional_count]` are the
    // positional arguments and the trailing `kwcount` values are the keyword
    // values, paired in order with `kwnames`. A FASTCALL callee receives
    // `nargs = positional_count`; keyword values follow in the same array
    // (`_PyCFunctionFastWithKeywords` indexes them at `args + nargs`).
    let positional_count = values.len() - kwcount;
    let pointer = values.as_ptr();
    let nargs = positional_count as isize;

    clear_error();
    let address = function as *const c_void;

    if signature == METH_FASTCALL {
        if flags & METH_KEYWORDS != 0 {
            // `_PyCFunctionFastWithKeywords`: (self, args, nargs, kwnames).
            let function: unsafe extern "C" fn(
                *mut ShimObject,
                *const *mut ShimObject,
                isize,
                *mut ShimObject,
            ) -> *mut ShimObject = unsafe { std::mem::transmute(address) };
            // SAFETY: the argument values are live for the duration of the
            // call; a NULL `kwnames` means "no keyword arguments", which
            // CPython permits.
            let kwnames = if kwcount == 0 {
                ptr::null_mut()
            } else {
                kwnames
            };
            return unsafe { function(self_object, pointer, nargs, kwnames) };
        }
        // `_PyCFunctionFast`: (self, args, nargs).
        let function: unsafe extern "C" fn(
            *mut ShimObject,
            *const *mut ShimObject,
            isize,
        ) -> *mut ShimObject = unsafe { std::mem::transmute(address) };
        // SAFETY: as above; the values outlive the call.
        return unsafe { function(self_object, pointer, nargs) };
    }

    if signature == METH_NOARGS {
        if !values.is_empty() {
            set_error(format!(
                "TypeError: {name}() takes no arguments ({} given)",
                values.len()
            ));
            return ptr::null_mut();
        }
        // SAFETY: METH_NOARGS is `(self, NULL)`.
        let function: unsafe extern "C" fn(*mut ShimObject, *mut ShimObject) -> *mut ShimObject =
            unsafe { std::mem::transmute(address) };
        // SAFETY: both arguments follow the documented convention.
        return unsafe { function(self_object, ptr::null_mut()) };
    }

    if signature == METH_O {
        if positional_count != 1 || kwcount != 0 {
            set_error(format!(
                "TypeError: {name}() takes exactly one argument ({} given)",
                positional_count + kwcount
            ));
            return ptr::null_mut();
        }
        // SAFETY: METH_O passes the single positional object.
        let function: unsafe extern "C" fn(*mut ShimObject, *mut ShimObject) -> *mut ShimObject =
            unsafe { std::mem::transmute(address) };
        // SAFETY: the object pointer comes from the live value vector.
        return unsafe { function(self_object, values[0]) };
    }

    // METH_VARARGS, optionally with keyword support.
    let tuple = tuple_from_borrowed(&values[..positional_count]);
    if flags & METH_KEYWORDS != 0 {
        let dict = match build_kwargs_dict(kwnames, &values[positional_count..]) {
            Ok(dict) => dict,
            Err(error) => {
                dec_ref(tuple);
                set_error(error);
                return ptr::null_mut();
            }
        };
        // SAFETY: METH_VARARGS | METH_KEYWORDS is `(self, args, kwargs)`.
        let function: unsafe extern "C" fn(
            *mut ShimObject,
            *mut ShimObject,
            *mut ShimObject,
        ) -> *mut ShimObject = unsafe { std::mem::transmute(address) };
        // SAFETY: the tuple and dict are live for the call.
        let result = unsafe { function(self_object, tuple, dict) };
        dec_ref(tuple);
        if !dict.is_null() {
            dec_ref(dict);
        }
        return result;
    }
    // SAFETY: METH_VARARGS is `(self, args)`.
    let function: unsafe extern "C" fn(*mut ShimObject, *mut ShimObject) -> *mut ShimObject =
        unsafe { std::mem::transmute(address) };
    // SAFETY: the tuple is live for the call.
    let result = unsafe { function(self_object, tuple) };
    dec_ref(tuple);
    result
}

/// Dispatches a vectorcall to the `PyMethodDef`-style implementation stored in
/// a `PyCFunction`.
///
/// `self_object` is the C-level `self` argument. A bound method passes its
/// bound object here instead of prepending it to `values`, which keeps
/// `METH_NOARGS` implementations callable as methods.
fn invoke_cfunction(
    method: usize,
    self_object: *mut ShimObject,
    values: &[*mut ShimObject],
    kwnames: *mut ShimObject,
) -> *mut ShimObject {
    let definition = method as *mut PyMethodDefMirror;
    if definition.is_null() {
        set_error("SystemError: C function has no PyMethodDef");
        return ptr::null_mut();
    }
    // SAFETY: the table belongs to the live extension.
    let (function, flags, name) = unsafe {
        (
            (*definition).ml_meth as usize,
            (*definition).ml_flags,
            read_c_string((*definition).ml_name),
        )
    };
    invoke_method(function, flags, self_object, &name, values, kwnames)
}

/// Calls a heap type: `tp_vectorcall` when present, otherwise
/// `tp_new` + `tp_init`.
fn type_call(
    type_object: *mut TypeObject,
    args: *mut ShimObject,
    kwds: *mut ShimObject,
) -> *mut ShimObject {
    let _gripper = GilGuard::new();
    if let Err(error) = types::ensure_ready(type_object) {
        set_error(error);
        return ptr::null_mut();
    }
    // SAFETY: `type_object` was validated before this call.
    let flags = unsafe { (*type_object).tp_flags };
    if flags & PY_TPFLAGS_DISALLOW_INSTANTIATION != 0 {
        set_error(format!(
            "TypeError: cannot create '{}' instances",
            types::type_name(type_object)
        ));
        return ptr::null_mut();
    }
    let new_function = unsafe { (*type_object).tp_new };
    // A static exception class now exposes `tp_new` for PyO3 inheritance, but
    // constructing it from Python still builds the shim exception instance
    // holding the positional arguments (the previous tp_new-less path).
    let static_exception = unsafe { (*type_object).role == TypeRole::Shim }
        && flags & types::PY_TPFLAGS_BASE_EXC_SUBCLASS != 0;
    if new_function == 0 || static_exception {
        initialize_exception_types();
        let base = unsafe { PyExc_BaseException as *mut TypeObject };
        if !base.is_null() && types::is_subtype(type_object, base) {
            return exception_instance_from_arguments(type_object, args, kwds);
        }
        set_error(format!(
            "TypeError: cannot create '{}' instances: the type has no Py_tp_new",
            types::type_name(type_object)
        ));
        return ptr::null_mut();
    }
    // SAFETY: `newfunc` has this signature.
    let new_function: unsafe extern "C" fn(
        *mut TypeObject,
        *mut ShimObject,
        *mut ShimObject,
    ) -> *mut ShimObject = unsafe { std::mem::transmute(new_function as *const c_void) };
    // SAFETY: the type and argument objects are live; the callee borrows them.
    let result = unsafe { new_function(type_object, args, kwds) };
    if result.is_null() {
        if !error_pending() {
            set_error(format!(
                "SystemError: {} constructor returned NULL without setting an exception",
                types::type_name(type_object)
            ));
        }
        return ptr::null_mut();
    }
    let init_function = unsafe { (*type_object).tp_init };
    if init_function != 0 {
        // SAFETY: `initproc` has this signature.
        let init_function: InitProc =
            unsafe { std::mem::transmute(init_function as *const c_void) };
        // SAFETY: the new instance is live and borrowed by the callee.
        let status = unsafe { init_function(result, args, kwds) };
        if status != 0 {
            dec_ref(result);
            if !error_pending() {
                set_error(format!(
                    "SystemError: {} initialiser failed without setting an exception",
                    types::type_name(type_object)
                ));
            }
            return ptr::null_mut();
        }
    }
    let result_type = type_of(result);
    if !types::is_subtype(result_type, type_object) {
        set_error(format!(
            "TypeError: {}.__new__ returned a non-{} object",
            types::type_name(type_object),
            types::type_name(type_object)
        ));
        dec_ref(result);
        return ptr::null_mut();
    }
    result
}

/// Builds an exception instance from the call arguments of its class.
///
/// `BaseException` and its C subclasses take only positional arguments;
/// keywords are rejected exactly like CPython's `BaseException_init`.
fn exception_instance_from_arguments(
    type_object: *mut TypeObject,
    args: *mut ShimObject,
    kwds: *mut ShimObject,
) -> *mut ShimObject {
    let name = types::type_name(type_object);
    if !kwds.is_null() {
        match kind_ref(kwds) {
            Some(ShimKind::Dict(pairs)) if pairs.is_empty() => {}
            _ => {
                set_error(format!("TypeError: {name}() takes no keyword arguments"));
                return ptr::null_mut();
            }
        }
    }
    let items = if args.is_null() {
        &[][..]
    } else {
        match tuple_slots(args) {
            Ok(items) => items,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        }
    };
    if type_object as *mut ShimObject == unsafe { PyExc_UnicodeDecodeError } {
        if let Err(error) = validate_unicode_decode_arguments(items) {
            set_error(error);
            return ptr::null_mut();
        }
    }
    let pointers: Vec<*mut ShimObject> =
        items.iter().map(|&item| item as *mut ShimObject).collect();
    new_exception_instance(type_object as usize, pointers)
}

/// Validates the five arguments of a `UnicodeDecodeError` instance.
///
/// CPython's `UnicodeDecodeError_init` parses `(encoding, object, start, end,
/// reason)`, requires a bytes-like object and coerces the bounds to integers.
fn validate_unicode_decode_arguments(items: &[usize]) -> Result<(), String> {
    if items.len() != 5 {
        return Err(format!(
            "TypeError: function takes exactly 5 arguments ({} given)",
            items.len()
        ));
    }
    let encoding = items[0] as *mut ShimObject;
    let object = items[1] as *mut ShimObject;
    let start = items[2] as *mut ShimObject;
    let end = items[3] as *mut ShimObject;
    let reason = items[4] as *mut ShimObject;
    if as_string(encoding).is_none() {
        return Err("TypeError: argument 1 must be str".to_string());
    }
    if !matches!(kind_ref(object), Some(ShimKind::Bytes { .. })) {
        return Err(format!(
            "TypeError: a bytes-like object is required, not '{}'",
            object_type_name(object)
        ));
    }
    if as_int(start).is_err() || as_int(end).is_err() {
        return Err("TypeError: start and end must be integers".to_string());
    }
    if as_string(reason).is_none() {
        return Err("TypeError: argument 5 must be str".to_string());
    }
    Ok(())
}

/// `PyObject_Vectorcall`.
#[no_mangle]
pub extern "C" fn PyObject_Vectorcall(
    callable: *mut ShimObject,
    args: *const *mut ShimObject,
    nargsf: usize,
    kwnames: *mut ShimObject,
) -> *mut ShimObject {
    let _gripper = GilGuard::new();
    if callable.is_null() {
        set_error("SystemError: PyObject_Vectorcall: callable is NULL");
        return ptr::null_mut();
    }
    ensure_builtin_types();
    let nargs = vectorcall_nargs(nargsf);
    let values = match object_slice(args, nargs) {
        Ok(values) => values,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let names = match keyword_names(kwnames) {
        Ok(names) => names,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    if names.len() > values.len() {
        set_error("SystemError: more keyword names than arguments");
        return ptr::null_mut();
    }
    let keyword_count = names.len();
    let positional_count = values.len() - keyword_count;

    // A Hydro-backed callable (class, function, bound method or instance with
    // `__call__`) invokes Hydro code and converts the result back.
    if let Some(ShimKind::Hydro(handle)) = kind_ref(callable) {
        return call_hydro(handle, values, names, positional_count);
    }

    if types::is_type_pointer(callable) {
        let type_object = callable as *mut TypeObject;
        // SAFETY: validated type object.
        let vectorcall = unsafe { (*type_object).tp_vectorcall };
        if vectorcall != 0 {
            // SAFETY: `vectorcallfunc` has this exact signature; the vector is
            // caller-owned and live for the call.
            let vectorcall: VectorcallFunc =
                unsafe { std::mem::transmute(vectorcall as *const c_void) };
            return unsafe { vectorcall(callable, args, nargsf, kwnames) };
        }
        let tuple = tuple_from_borrowed(&values[..positional_count]);
        let dict = if keyword_count == 0 {
            ptr::null_mut()
        } else {
            match build_kwargs_dict(kwnames, &values[positional_count..]) {
                Ok(dict) => dict,
                Err(error) => {
                    dec_ref(tuple);
                    set_error(error);
                    return ptr::null_mut();
                }
            }
        };
        let result = type_call(type_object, tuple, dict);
        dec_ref(tuple);
        if !dict.is_null() {
            dec_ref(dict);
        }
        return result;
    }

    let cfunction = if let Some(ShimKind::CFunction {
        method,
        self_object,
        module: _,
    }) = kind_ref(callable)
    {
        Some((*method, *self_object))
    } else {
        None
    };
    if let Some((method, self_object)) = cfunction {
        return invoke_cfunction(method, self_object as *mut ShimObject, values, kwnames);
    }
    if let Some(ShimKind::Method {
        function,
        self_object,
    }) = kind_ref(callable)
    {
        let function = *function as *mut ShimObject;
        let self_object = *self_object as *mut ShimObject;
        let c_method = match kind_ref(function) {
            Some(ShimKind::CFunction { method, .. }) => Some(*method),
            _ => None,
        };
        if let Some(method) = c_method {
            return invoke_cfunction(method, self_object, values, kwnames);
        }
        let mut combined = Vec::with_capacity(values.len() + 1);
        combined.push(self_object);
        combined.extend_from_slice(values);
        return PyObject_Vectorcall(function, combined.as_ptr(), combined.len(), kwnames);
    }
    if let Some(ShimKind::RuntimeFunction { module, name }) = kind_ref(callable) {
        let module = module.clone();
        let name = name.clone();
        return call_runtime_function(&module, &name, values, names, positional_count);
    }

    let type_object = type_of(callable);
    if type_object.is_null() {
        set_error("SystemError: callable has no type");
        return ptr::null_mut();
    }
    // SAFETY: live type object.
    let flags = unsafe { (*type_object).tp_flags };
    let offset = unsafe { (*type_object).tp_vectorcall_offset };
    if flags & PY_TPFLAGS_HAVE_VECTORCALL != 0 && offset > 0 {
        let basicsize = unsafe { (*type_object).tp_basicsize };
        if (offset as usize) + std::mem::size_of::<usize>() <= basicsize as usize {
            // SAFETY: the instance declares an in-bounds vectorcall slot.
            let vectorcall =
                unsafe { *((callable as *mut u8).add(offset as usize) as *const usize) };
            if vectorcall != 0 {
                let vectorcall: VectorcallFunc =
                    unsafe { std::mem::transmute(vectorcall as *const c_void) };
                // SAFETY: `vectorcallfunc` has this signature.
                return unsafe { vectorcall(callable, args, nargsf, kwnames) };
            }
        } else {
            set_error(format!(
                "SystemError: type '{}' has tp_vectorcall_offset {offset} outside its instance",
                types::type_name(type_object)
            ));
            return ptr::null_mut();
        }
    }
    let call = unsafe { (*type_object).tp_call };
    if call != 0 {
        let tuple = tuple_from_borrowed(&values[..positional_count]);
        let dict = if keyword_count == 0 {
            ptr::null_mut()
        } else {
            match build_kwargs_dict(kwnames, &values[positional_count..]) {
                Ok(dict) => dict,
                Err(error) => {
                    dec_ref(tuple);
                    set_error(error);
                    return ptr::null_mut();
                }
            }
        };
        // SAFETY: `tp_call` is `(callable, args, kwargs)`.
        let call: TernaryFunc = unsafe { std::mem::transmute(call as *const c_void) };
        // SAFETY: all arguments are live for the call.
        let result = unsafe { call(callable, tuple, dict) };
        dec_ref(tuple);
        if !dict.is_null() {
            dec_ref(dict);
        }
        return result;
    }
    set_error(format!(
        "TypeError: '{}' object is not callable",
        types::type_name(type_object)
    ));
    ptr::null_mut()
}

/// `PyObject_VectorcallDict`.
#[no_mangle]
pub extern "C" fn PyObject_VectorcallDict(
    callable: *mut ShimObject,
    args: *const *mut ShimObject,
    nargs: usize,
    kwargs: *mut ShimObject,
) -> *mut ShimObject {
    if kwargs.is_null() {
        return PyObject_Vectorcall(callable, args, nargs, ptr::null_mut());
    }
    let positional = match object_slice(args, nargs) {
        Ok(values) => values,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let pairs = match dict_slots(kwargs) {
        Ok(pairs) => pairs,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    if pairs.len() > isize::MAX as usize {
        set_error("SystemError: too many keyword arguments");
        return ptr::null_mut();
    }
    // Keyword names must be strings; build the tuple CPython's vectorcall
    // convention expects.
    let names_tuple = PyTuple_New(pairs.len() as isize);
    if names_tuple.is_null() {
        return ptr::null_mut();
    }
    for (index, &(key, _)) in pairs.iter().enumerate() {
        let Some(text) = as_string(key as *mut ShimObject) else {
            dec_ref(names_tuple);
            set_error("TypeError: keywords must be strings");
            return ptr::null_mut();
        };
        let name = new_str(text);
        if PyTuple_SetItem(names_tuple, index as isize, name) != 0 {
            // PyTuple_SetItem does not steal on failure.
            dec_ref(name);
            dec_ref(names_tuple);
            return ptr::null_mut();
        }
    }
    let mut values: Vec<*mut ShimObject> = positional.to_vec();
    values.extend(pairs.iter().map(|&(_, value)| value as *mut ShimObject));
    let result = PyObject_Vectorcall(callable, values.as_ptr(), values.len(), names_tuple);
    dec_ref(names_tuple);
    result
}

/// `PyVectorcall_NARGS`.
#[no_mangle]
pub extern "C" fn PyVectorcall_NARGS(nargsf: usize) -> isize {
    py_ssize_of(vectorcall_nargs(nargsf))
}

/// `PyVectorcall_Call`.
#[no_mangle]
pub extern "C" fn PyVectorcall_Call(
    callable: *mut ShimObject,
    tuple: *mut ShimObject,
    dict: *mut ShimObject,
) -> *mut ShimObject {
    let positional: &[usize] = if tuple.is_null() {
        &[]
    } else {
        match tuple_slots(tuple) {
            Ok(items) => items,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        }
    };
    let values: Vec<*mut ShimObject> = positional
        .iter()
        .map(|&value| value as *mut ShimObject)
        .collect();
    PyObject_VectorcallDict(callable, values.as_ptr(), values.len(), dict)
}

/// `PyObject_Call`.
#[no_mangle]
pub extern "C" fn PyObject_Call(
    callable: *mut ShimObject,
    args: *mut ShimObject,
    kwargs: *mut ShimObject,
) -> *mut ShimObject {
    if args.is_null() {
        return PyObject_VectorcallDict(callable, ptr::null(), 0, kwargs);
    }
    let positional = match tuple_slots(args) {
        Ok(items) => items,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let values: Vec<*mut ShimObject> = positional
        .iter()
        .map(|&value| value as *mut ShimObject)
        .collect();
    PyObject_VectorcallDict(callable, values.as_ptr(), values.len(), kwargs)
}

/// `PyObject_CallObject`.
#[no_mangle]
pub extern "C" fn PyObject_CallObject(
    callable: *mut ShimObject,
    args: *mut ShimObject,
) -> *mut ShimObject {
    if args.is_null() {
        return PyObject_VectorcallDict(callable, ptr::null(), 0, ptr::null_mut());
    }
    let positional = match tuple_slots(args) {
        Ok(items) => items,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let values: Vec<*mut ShimObject> = positional
        .iter()
        .map(|&value| value as *mut ShimObject)
        .collect();
    PyObject_VectorcallDict(callable, values.as_ptr(), values.len(), ptr::null_mut())
}

/// `PyObject_VectorcallMethod`.
#[no_mangle]
pub extern "C" fn PyObject_VectorcallMethod(
    name: *mut ShimObject,
    args: *const *mut ShimObject,
    nargsf: usize,
    kwnames: *mut ShimObject,
) -> *mut ShimObject {
    let _gripper = GilGuard::new();
    let Some(method_name) = as_string(name) else {
        set_error("TypeError: method name must be a string");
        return ptr::null_mut();
    };
    let nargs = vectorcall_nargs(nargsf);
    if nargs == 0 {
        set_error("TypeError: PyObject_VectorcallMethod requires at least one argument");
        return ptr::null_mut();
    }
    let values = match object_slice(args, nargs) {
        Ok(values) => values,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let receiver = values[0];
    let callable = match attribute_lookup(receiver, &method_name) {
        Ok(callable) => callable,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    // SAFETY: `args` has at least one entry, so `args + 1` is inside (or one
    // past) the vector.
    let shifted = unsafe { args.add(1) };
    let result = PyObject_Vectorcall(callable, shifted, nargsf - 1, kwnames);
    dec_ref(callable);
    result
}

/// Helper used by `parse.c` for `PyObject_CallFunctionObjArgs`.
#[no_mangle]
pub extern "C" fn hydro_capi_vectorcall(
    callable: *mut ShimObject,
    args: *const *mut ShimObject,
    nargs: usize,
) -> *mut ShimObject {
    PyObject_Vectorcall(callable, args, nargs, ptr::null_mut())
}

/// `PyCFunction_NewEx`.
#[no_mangle]
pub extern "C" fn PyCFunction_NewEx(
    method: *mut PyMethodDefMirror,
    self_object: *mut ShimObject,
    module: *mut ShimObject,
) -> *mut ShimObject {
    if method.is_null() {
        set_error("SystemError: PyCFunction_NewEx: PyMethodDef is NULL");
        return ptr::null_mut();
    }
    // SAFETY: the extension owns the live method definition.
    let (function, name) = unsafe { ((*method).ml_meth, (*method).ml_name) };
    if function.is_null() {
        set_error("SystemError: PyCFunction_NewEx: method has a NULL implementation");
        return ptr::null_mut();
    }
    if name.is_null() {
        set_error("SystemError: PyCFunction_NewEx: method has no name");
        return ptr::null_mut();
    }
    inc_ref(self_object);
    inc_ref(module);
    new_object(ShimKind::CFunction {
        method: method as usize,
        self_object: self_object as usize,
        module: module as usize,
    })
}

/// `PyCFunction_New`.
#[no_mangle]
pub extern "C" fn PyCFunction_New(
    method: *mut PyMethodDefMirror,
    self_object: *mut ShimObject,
) -> *mut ShimObject {
    PyCFunction_NewEx(method, self_object, ptr::null_mut())
}

fn cfunction_fields(object: *mut ShimObject) -> Result<(usize, usize, usize), String> {
    match kind_ref(object) {
        Some(ShimKind::CFunction {
            method,
            self_object,
            module,
        }) => Ok((*method, *self_object, *module)),
        _ => Err("SystemError: object is not a PyCFunction".to_string()),
    }
}

/// `PyCFunction_GetFunction`.
#[no_mangle]
pub extern "C" fn PyCFunction_GetFunction(object: *mut ShimObject) -> *mut c_void {
    match cfunction_fields(object) {
        Ok((method, _, _)) => {
            let definition = method as *mut PyMethodDefMirror;
            if definition.is_null() {
                ptr::null_mut()
            } else {
                // SAFETY: the definition is owned by the extension.
                unsafe { (*definition).ml_meth }
            }
        }
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyCFunction_GetSelf`.
#[no_mangle]
pub extern "C" fn PyCFunction_GetSelf(object: *mut ShimObject) -> *mut ShimObject {
    match cfunction_fields(object) {
        Ok((_, self_object, _)) => self_object as *mut ShimObject,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyCFunction_GetFlags`.
#[no_mangle]
pub extern "C" fn PyCFunction_GetFlags(object: *mut ShimObject) -> c_int {
    match cfunction_fields(object) {
        Ok((method, _, _)) => {
            let definition = method as *mut PyMethodDefMirror;
            if definition.is_null() {
                set_error("SystemError: C function has no PyMethodDef");
                -1
            } else {
                // SAFETY: the definition is owned by the extension.
                unsafe { (*definition).ml_flags }
            }
        }
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

/// `PyCFunction_GetModule`.
#[no_mangle]
pub extern "C" fn PyCFunction_GetModule(object: *mut ShimObject) -> *mut ShimObject {
    match cfunction_fields(object) {
        Ok((_, _, module)) => module as *mut ShimObject,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// Attribute, repr, string and hash API
// ---------------------------------------------------------------------------

/// Looks up an attribute of a loaded module through the registry.
fn module_attribute(
    module: *mut ShimObject,
    name: &str,
) -> Option<Result<*mut ShimObject, String>> {
    let id = match kind_ref(module) {
        Some(ShimKind::Module(id)) => *id,
        _ => return None,
    };
    if name == "__dict__" {
        let dict = PyModule_GetDict(module);
        if !dict.is_null() {
            // The registry owns the dictionary; attribute lookup returns a new
            // reference like every other `getattr`.
            inc_ref(dict);
        }
        return Some(Ok(dict));
    }
    let (runtime, module_name, attribute, definition, module_object) = {
        let registry = registry_lock();
        let entry = registry.modules.get(&id);
        let Some(entry) = entry else {
            return Some(Err(format!("AttributeError: unknown module {id}")));
        };
        if entry.dict != 0 {
            let key = new_str(name.to_string());
            let value = PyDict_GetItem(entry.dict as *mut ShimObject, key);
            dec_ref(key);
            if !value.is_null() {
                inc_ref(value);
                return Some(Ok(value));
            }
        }
        let attribute = entry.attrs.get(name).cloned();
        let definition = match &attribute {
            Some(ShimAttr::Callable(callable)) => entry
                .methods
                .get(callable.method as usize)
                .map(|spec| spec.definition)
                .unwrap_or(0),
            _ => 0,
        };
        (
            entry.runtime.clone(),
            entry.name.clone(),
            attribute,
            definition,
            entry.module_object,
        )
    };
    match attribute {
        Some(ShimAttr::Value(value)) => Some(Ok(value_object(value))),
        Some(ShimAttr::Type(address)) => {
            inc_ref(address as *mut ShimObject);
            Some(Ok(address as *mut ShimObject))
        }
        Some(ShimAttr::Callable(_)) => {
            if definition == 0 {
                return Some(Err(format!(
                    "AttributeError: module '{module_name}' has no method implementation \
                     for '{name}'"
                )));
            }
            let result = PyCFunction_NewEx(
                definition as *mut PyMethodDefMirror,
                module_object as *mut ShimObject,
                ptr::null_mut(),
            );
            if result.is_null() {
                return Some(Err(take_error().unwrap_or_else(|| {
                    format!("AttributeError: cannot bind module method '{name}'")
                })));
            }
            Some(Ok(result))
        }
        None => match runtime {
            Some(runtime) => Some(runtime_module_attribute(&runtime, name)),
            None => Some(Err(format!(
                "AttributeError: module '{module_name}' has no attribute '{name}'"
            ))),
        },
    }
}

/// Class-level attributes exposed by this host without a type dictionary.
fn type_attribute(
    type_pointer: *mut ShimObject,
    name: &str,
) -> Option<Result<*mut ShimObject, String>> {
    let type_object = type_pointer as *mut TypeObject;
    // SAFETY: `type_pointer` was validated with `is_type_pointer`.
    unsafe {
        let dict = (*type_object).tp_dict as *mut ShimObject;
        if !dict.is_null() {
            let Ok(c_name) = CString::new(name) else {
                return Some(Err("ValueError: attribute name contains a NUL".to_string()));
            };
            let stored = PyDict_GetItemString(dict, c_name.as_ptr());
            if !stored.is_null() {
                inc_ref(stored);
                return Some(Ok(stored));
            }
        }
        Some(match name {
            "__name__" => Ok(new_str(
                types::type_name(type_object)
                    .rsplit('.')
                    .next()
                    .unwrap_or("")
                    .to_string(),
            )),
            "__qualname__" => Ok(new_str(types::type_name(type_object))),
            "__module__" => {
                let full = types::type_name(type_object);
                let module = full
                    .rsplit_once('.')
                    .map(|(module, _)| module)
                    .unwrap_or("builtins");
                Ok(new_str(module.to_string()))
            }
            "__doc__" => {
                let doc = (*type_object).tp_doc;
                if doc.is_null() {
                    Ok(value_object(ShimValue::None))
                } else {
                    Ok(new_str(read_c_string(doc)))
                }
            }
            "__flags__" => Ok(new_int((*type_object).tp_flags as i64)),
            "__basicsize__" => Ok(new_int((*type_object).tp_basicsize as i64)),
            "__itemsize__" => Ok(new_int((*type_object).tp_itemsize as i64)),
            "__dictoffset__" => Ok(new_int((*type_object).tp_dictoffset as i64)),
            "__weakrefoffset__" => Ok(new_int((*type_object).tp_weaklistoffset as i64)),
            "__vectorcalloffset__" => Ok(new_int((*type_object).tp_vectorcall_offset as i64)),
            "__base__" => {
                let base = (*type_object).tp_base;
                if base.is_null() {
                    Ok(value_object(ShimValue::None))
                } else {
                    inc_ref(base as *mut ShimObject);
                    Ok(base as *mut ShimObject)
                }
            }
            "__bases__" => {
                let bases = (*type_object).tp_bases as *mut ShimObject;
                if bases.is_null() {
                    let tuple = PyTuple_New(0);
                    Ok(tuple)
                } else {
                    inc_ref(bases);
                    Ok(bases)
                }
            }
            "__mro__" => {
                let order = types::mro_of(type_object);
                let tuple = PyTuple_New(order.len() as isize);
                if tuple.is_null() {
                    return Some(Err(take_error().unwrap_or_else(|| {
                        "MemoryError: cannot create __mro__".to_string()
                    })));
                }
                for (index, &entry) in order.iter().enumerate() {
                    inc_ref(entry as *mut ShimObject);
                    if PyTuple_SetItem(tuple, index as isize, entry as *mut ShimObject) != 0 {
                        dec_ref(entry as *mut ShimObject);
                        dec_ref(tuple);
                        return Some(Err(take_error()
                            .unwrap_or_else(|| "TypeError: cannot fill __mro__".to_string())));
                    }
                }
                Ok(tuple)
            }
            // Static and class methods are reachable through the class, as in
            // CPython; a plain instance method still requires an instance.
            _ => match types::find_method(type_object, name) {
                Some(spec) if spec.flags & (METH_STATIC | METH_CLASS) != 0 => {
                    let receiver = if spec.flags & METH_STATIC != 0 {
                        ptr::null_mut()
                    } else {
                        type_pointer
                    };
                    let method = spec.definition as *mut PyMethodDefMirror;
                    match PyCFunction_NewEx(method, receiver, ptr::null_mut()) {
                        function if !function.is_null() => Ok(function),
                        _ => Err(take_error().unwrap_or_else(|| {
                            format!("AttributeError: cannot bind method '{name}'")
                        })),
                    }
                }
                _ => return None,
            },
        })
    }
}

/// The shared descriptor-aware attribute lookup used by
/// `PyObject_GenericGetAttr` and by the host's own `PyObject_GetAttr*` path.
fn generic_attribute(object: *mut ShimObject, name: &str) -> Result<*mut ShimObject, String> {
    if types::is_type_pointer(object) {
        return type_attribute(object, name).unwrap_or_else(|| {
            Err(format!(
                "AttributeError: type object '{}' has no attribute '{name}'",
                types::type_name(object as *mut TypeObject)
            ))
        });
    }
    if let Some(module) = module_attribute(object, name) {
        return module;
    }
    if let Some(ShimKind::Method {
        function,
        self_object,
    }) = kind_ref(object)
    {
        return match name {
            "__func__" => {
                let function = *function as *mut ShimObject;
                inc_ref(function);
                Ok(function)
            }
            "__self__" => {
                if *self_object == 0 {
                    Ok(value_object(ShimValue::None))
                } else {
                    let self_object = *self_object as *mut ShimObject;
                    inc_ref(self_object);
                    Ok(self_object)
                }
            }
            "__name__" | "__qualname__" => {
                let function = *function as *mut ShimObject;
                attribute_lookup(function, name)
            }
            _ => Err(format!(
                "AttributeError: 'method' object has no attribute '{name}'"
            )),
        };
    }
    if let Some(ShimKind::CFunction {
        method,
        self_object,
        module,
    }) = kind_ref(object)
    {
        let definition = *method as *mut PyMethodDefMirror;
        return match name {
            "__name__" | "__qualname__" => {
                let name = if definition.is_null() {
                    "<unknown>".to_string()
                } else {
                    // SAFETY: extension-owned, process-lifetime method table.
                    unsafe { read_c_string((*definition).ml_name) }
                };
                Ok(new_str(name))
            }
            "__self__" => {
                if *self_object == 0 {
                    Ok(value_object(ShimValue::None))
                } else {
                    let receiver = *self_object as *mut ShimObject;
                    inc_ref(receiver);
                    Ok(receiver)
                }
            }
            "__module__" => {
                if *module == 0 {
                    Ok(value_object(ShimValue::None))
                } else {
                    let module_object = *module as *mut ShimObject;
                    inc_ref(module_object);
                    Ok(module_object)
                }
            }
            "__doc__" => {
                if definition.is_null() || unsafe { (*definition).ml_doc }.is_null() {
                    Ok(value_object(ShimValue::None))
                } else {
                    // SAFETY: extension-owned doc string.
                    Ok(new_str(unsafe { read_c_string((*definition).ml_doc) }))
                }
            }
            _ => Err(format!(
                "AttributeError: 'builtin_function_or_method' object has no attribute '{name}'"
            )),
        };
    }
    if let Some(ShimKind::GenericAlias { origin, args }) = kind_ref(object) {
        return match name {
            "__origin__" => {
                let origin = *origin as *mut ShimObject;
                inc_ref(origin);
                Ok(origin)
            }
            "__args__" => {
                let args = *args as *mut ShimObject;
                inc_ref(args);
                Ok(args)
            }
            _ => Err(format!(
                "AttributeError: 'types.GenericAlias' object has no attribute '{name}'"
            )),
        };
    }
    let type_object = type_of(object);
    if type_object.is_null() {
        return Err(format!("AttributeError: object has no attribute '{name}'"));
    }
    if let Some(definition) = types::find_member(type_object, name) {
        return types::member_get(object, definition);
    }
    if let Some(definition) = types::find_getset(type_object, name) {
        return types::getset_get(object, definition);
    }
    if let Some(spec) = types::find_method(type_object, name) {
        // SAFETY: the definition belongs to the live extension.
        let method = spec.definition as *mut PyMethodDefMirror;
        // `METH_STATIC` methods receive NULL; `METH_CLASS` methods receive
        // the class the attribute was read from; everything else is bound to
        // the receiver itself, as CPython's method descriptors do.
        let receiver = if spec.flags & METH_STATIC != 0 {
            ptr::null_mut()
        } else if spec.flags & METH_CLASS != 0 {
            type_of(object) as *mut ShimObject
        } else {
            object
        };
        let result = PyCFunction_NewEx(method, receiver, ptr::null_mut());
        if result.is_null() {
            return Err(take_error()
                .unwrap_or_else(|| format!("AttributeError: cannot bind method '{name}'")));
        }
        return Ok(result);
    }
    Err(format!(
        "AttributeError: '{}' object has no attribute '{name}'",
        types::type_name(type_object)
    ))
}

/// Looks up `name` on `object`, returning a new reference.
fn attribute_lookup(object: *mut ShimObject, name: &str) -> Result<*mut ShimObject, String> {
    ensure_builtin_types();
    if object.is_null() {
        return Err(format!(
            "AttributeError: NULL object has no attribute '{name}'"
        ));
    }
    // A Hydro-backed handle resolves its attributes in Hydro: classes expose
    // their methods/descriptors, instances run `__getattribute__` and modules
    // resolve their attributes.
    if hydro_handle(object).is_some() {
        return hydro_attribute(object, name);
    }
    if types::is_type_pointer(object) {
        return generic_attribute(object, name);
    }
    let type_object = type_of(object);
    if type_object.is_null() {
        return Err(format!("AttributeError: object has no attribute '{name}'"));
    }
    // SAFETY: live type object.
    let getattro = unsafe { (*type_object).tp_getattro };
    let generic = PyObject_GenericGetAttr as *const () as usize;
    if getattro != 0 && getattro != generic {
        let name_object = new_str(name.to_string());
        let getattro: unsafe extern "C" fn(*mut ShimObject, *mut ShimObject) -> *mut ShimObject =
            unsafe { std::mem::transmute(getattro as *const c_void) };
        let _gripper = GilGuard::new();
        // SAFETY: `tp_getattro` has this signature and borrows its arguments.
        let result = unsafe { getattro(object, name_object) };
        dec_ref(name_object);
        if result.is_null() {
            if !error_pending() {
                set_error(format!(
                    "AttributeError: '{}' object has no attribute '{name}'",
                    types::type_name(type_object)
                ));
            }
            return Err(take_error().unwrap_or_else(|| {
                format!(
                    "AttributeError: '{}' object has no attribute '{name}'",
                    types::type_name(type_object)
                )
            }));
        }
        clear_error();
        return Ok(result);
    }
    if getattro == 0 {
        let getattr = unsafe { (*type_object).tp_getattr };
        if getattr != 0 {
            let c_name = match CString::new(name) {
                Ok(c_name) => c_name,
                Err(_) => return Err(format!("AttributeError: invalid attribute name '{name}'")),
            };
            let getattr: unsafe extern "C" fn(*mut ShimObject, *const c_char) -> *mut ShimObject =
                unsafe { std::mem::transmute(getattr as *const c_void) };
            let _gripper = GilGuard::new();
            // SAFETY: `tp_getattr` has this signature.
            let result = unsafe { getattr(object, c_name.as_ptr()) };
            if result.is_null() {
                return Err(take_error().unwrap_or_else(|| {
                    format!(
                        "AttributeError: '{}' object has no attribute '{name}'",
                        types::type_name(type_object)
                    )
                }));
            }
            clear_error();
            return Ok(result);
        }
    }
    generic_attribute(object, name)
}

/// `PyObject_GenericGetAttr`: descriptor-aware lookup used as the default
/// `tp_getattro` for heap types.
#[no_mangle]
pub extern "C" fn PyObject_GenericGetAttr(
    object: *mut ShimObject,
    name: *mut ShimObject,
) -> *mut ShimObject {
    let Some(name) = as_string(name) else {
        set_error("TypeError: attribute name must be a string");
        return ptr::null_mut();
    };
    if hydro_handle(object).is_some() {
        return match hydro_attribute(object, &name) {
            Ok(attribute) => attribute,
            Err(error) => {
                set_error(error);
                ptr::null_mut()
            }
        };
    }
    match generic_attribute(object, &name) {
        Ok(attribute) => attribute,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// Descriptor-aware attribute assignment used as the default `tp_setattro`.
fn generic_setattr(
    object: *mut ShimObject,
    name: &str,
    value: *mut ShimObject,
) -> Result<(), String> {
    // Hydro handles own their attribute semantics: class overlays, instance
    // descriptors and module overrides all run in Hydro.
    if hydro_handle(object).is_some() {
        return hydro_attribute_set(object, name, value);
    }
    if types::is_type_pointer(object) {
        let type_object = object as *mut TypeObject;
        // SAFETY: validated type pointer; heap types own a mutable tp_dict.
        let flags = unsafe { (*type_object).tp_flags };
        let dict = unsafe { (*type_object).tp_dict as *mut ShimObject };
        if flags & types::PY_TPFLAGS_HEAPTYPE == 0
            || flags & types::PY_TPFLAGS_IMMUTABLETYPE != 0
            || dict.is_null()
        {
            return Err(format!(
                "AttributeError: cannot set '{name}' on immutable type '{}'",
                types::type_name(type_object)
            ));
        }
        let Ok(c_name) = CString::new(name) else {
            return Err("ValueError: attribute name contains a NUL".to_string());
        };
        let status = if value.is_null() {
            PyDict_DelItemString(dict, c_name.as_ptr())
        } else {
            PyDict_SetItemString(dict, c_name.as_ptr(), value)
        };
        return if status == 0 {
            Ok(())
        } else {
            Err(take_error()
                .unwrap_or_else(|| "AttributeError: type assignment failed".to_string()))
        };
    }
    if let Some(ShimKind::Module(_)) = kind_ref(object) {
        if value.is_null() {
            return Err(format!(
                "AttributeError: cannot delete module attribute '{name}'"
            ));
        }
        let attribute = if types::is_type_pointer(value) {
            ShimAttr::Type(value as usize)
        } else {
            ShimAttr::Value(object_to_value(value)?)
        };
        let id = module_id(object)?;
        let mut registry = registry_lock();
        let entry = registry
            .modules
            .get_mut(&id)
            .ok_or_else(|| format!("AttributeError: unknown module {id}"))?;
        entry.attrs.insert(name.to_string(), attribute);
        if entry.dict != 0 {
            let key = new_str(name.to_string());
            let status = PyDict_SetItem(entry.dict as *mut ShimObject, key, value);
            dec_ref(key);
            if status != 0 {
                return Err(take_error().unwrap_or_else(|| {
                    format!("AttributeError: cannot set module attribute '{name}'")
                }));
            }
        }
        return Ok(());
    }
    let type_object = type_of(object);
    if type_object.is_null() {
        return Err(format!("AttributeError: object has no attribute '{name}'"));
    }
    if let Some(definition) = types::find_member(type_object, name) {
        return types::member_set(object, definition, value);
    }
    if let Some(definition) = types::find_getset(type_object, name) {
        return types::getset_set(object, definition, value);
    }
    Err(format!(
        "AttributeError: '{}' object has no attribute '{name}'",
        types::type_name(type_object)
    ))
}

/// `PyObject_GenericSetAttr`.
#[no_mangle]
pub extern "C" fn PyObject_GenericSetAttr(
    object: *mut ShimObject,
    name: *mut ShimObject,
    value: *mut ShimObject,
) -> c_int {
    let Some(name) = as_string(name) else {
        set_error("TypeError: attribute name must be a string");
        return -1;
    };
    match generic_setattr(object, &name, value) {
        Ok(()) => 0,
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

/// `PyObject_GetAttrString`.
#[no_mangle]
pub extern "C" fn PyObject_GetAttrString(
    object: *mut ShimObject,
    name: *const c_char,
) -> *mut ShimObject {
    if name.is_null() {
        set_error("TypeError: PyObject_GetAttrString: name is NULL");
        return ptr::null_mut();
    }
    let name = read_c_string(name);
    match attribute_lookup(object, &name) {
        Ok(attribute) => attribute,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyObject_GetAttr`.
#[no_mangle]
pub extern "C" fn PyObject_GetAttr(
    object: *mut ShimObject,
    name: *mut ShimObject,
) -> *mut ShimObject {
    let Some(name) = as_string(name) else {
        set_error("TypeError: attribute name must be a string");
        return ptr::null_mut();
    };
    match attribute_lookup(object, &name) {
        Ok(attribute) => attribute,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyObject_SetAttrString`.
#[no_mangle]
pub extern "C" fn PyObject_SetAttrString(
    object: *mut ShimObject,
    name: *const c_char,
    value: *mut ShimObject,
) -> c_int {
    if name.is_null() {
        set_error("TypeError: PyObject_SetAttrString: name is NULL");
        return -1;
    }
    let name = read_c_string(name);
    let type_object = type_of(object);
    if type_object.is_null() {
        set_error(format!("AttributeError: object has no attribute '{name}'"));
        return -1;
    }
    // SAFETY: live type object.
    let setattro = unsafe { (*type_object).tp_setattro };
    if setattro != 0 {
        let name_object = new_str(name);
        let setattro: unsafe extern "C" fn(
            *mut ShimObject,
            *mut ShimObject,
            *mut ShimObject,
        ) -> c_int = unsafe { std::mem::transmute(setattro as *const c_void) };
        let _gripper = GilGuard::new();
        // SAFETY: `tp_setattro` has this signature.
        let status = unsafe { setattro(object, name_object, value) };
        dec_ref(name_object);
        if status != 0 && !error_pending() {
            set_error("AttributeError: assignment failed");
        }
        return status;
    }
    let setattr = unsafe { (*type_object).tp_setattr };
    if setattr != 0 {
        let c_name = match CString::new(name) {
            Ok(c_name) => c_name,
            Err(_) => {
                set_error("ValueError: attribute name contains a NUL");
                return -1;
            }
        };
        let setattr: unsafe extern "C" fn(
            *mut ShimObject,
            *const c_char,
            *mut ShimObject,
        ) -> c_int = unsafe { std::mem::transmute(setattr as *const c_void) };
        let _gripper = GilGuard::new();
        // SAFETY: `tp_setattr` has this signature.
        return unsafe { setattr(object, c_name.as_ptr(), value) };
    }
    match generic_setattr(object, &name, value) {
        Ok(()) => 0,
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

/// `PyObject_SetAttr`.
#[no_mangle]
pub extern "C" fn PyObject_SetAttr(
    object: *mut ShimObject,
    name: *mut ShimObject,
    value: *mut ShimObject,
) -> c_int {
    let Some(name) = as_string(name) else {
        set_error("TypeError: attribute name must be a string");
        return -1;
    };
    match CString::new(name) {
        Ok(c_name) => PyObject_SetAttrString(object, c_name.as_ptr(), value),
        Err(_) => {
            set_error("ValueError: attribute name contains a NUL");
            -1
        }
    }
}

/// `PyObject_Repr`.
#[no_mangle]
pub extern "C" fn PyObject_Repr(object: *mut ShimObject) -> *mut ShimObject {
    if object.is_null() {
        set_error("SystemError: PyObject_Repr: object is NULL");
        return ptr::null_mut();
    }
    if let Some(value) = hydro_tagged(object) {
        return match hydro_repr_text(value) {
            Ok(text) => new_str(text),
            Err(error) => {
                set_error(error);
                ptr::null_mut()
            }
        };
    }
    let type_object = type_of(object);
    if type_object.is_null() {
        return new_str(shim_repr(object));
    }
    // SAFETY: live type object.
    let repr = unsafe { (*type_object).tp_repr };
    if repr != 0 {
        let repr: unsafe extern "C" fn(*mut ShimObject) -> *mut ShimObject =
            unsafe { std::mem::transmute(repr as *const c_void) };
        let _gripper = GilGuard::new();
        // SAFETY: `tp_repr` has this signature.
        let result = unsafe { repr(object) };
        if result.is_null() && !error_pending() {
            set_error("SystemError: tp_repr returned NULL without setting an exception");
        }
        return result;
    }
    new_str(shim_repr(object))
}

/// `PyObject_Str`.
#[no_mangle]
pub extern "C" fn PyObject_Str(object: *mut ShimObject) -> *mut ShimObject {
    if object.is_null() {
        set_error("SystemError: PyObject_Str: object is NULL");
        return ptr::null_mut();
    }
    if let Some(value) = hydro_tagged(object) {
        return match hydro_str_text(value) {
            Ok(text) => new_str(text),
            Err(error) => {
                set_error(error);
                ptr::null_mut()
            }
        };
    }
    let type_object = type_of(object);
    if type_object.is_null() {
        if let Some(ShimKind::Str { text, .. }) = kind_ref(object) {
            return new_str(text.clone());
        }
        return PyObject_Repr(object);
    }
    // SAFETY: live type object.
    let str_function = unsafe { (*type_object).tp_str };
    if str_function != 0 {
        let str_function: unsafe extern "C" fn(*mut ShimObject) -> *mut ShimObject =
            unsafe { std::mem::transmute(str_function as *const c_void) };
        let _gripper = GilGuard::new();
        // SAFETY: `tp_str` has this signature.
        let result = unsafe { str_function(object) };
        if result.is_null() && !error_pending() {
            set_error("SystemError: tp_str returned NULL without setting an exception");
        }
        return result;
    }
    if let Some(ShimKind::Str { text, .. }) = kind_ref(object) {
        return new_str(text.clone());
    }
    PyObject_Repr(object)
}

/// `PyObject_Hash`.
#[no_mangle]
pub extern "C" fn PyObject_Hash(object: *mut ShimObject) -> isize {
    match hash_object(object, 0) {
        Ok(hash) => hash,
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

fn hash_object(object: *mut ShimObject, depth: usize) -> Result<isize, String> {
    if object.is_null() {
        return Err("SystemError: PyObject_Hash: object is NULL".to_string());
    }
    if depth > MAX_CONVERSION_DEPTH {
        return Err("RecursionError: hash of a container is too deep".to_string());
    }
    let type_object = type_of(object);
    if type_object.is_null() {
        return Err(format!(
            "TypeError: unhashable type: '{}'",
            object_type_name(object)
        ));
    }
    // SAFETY: live type object.
    let hash_function = unsafe { (*type_object).tp_hash };
    if hash_function != 0 {
        let hash_function: unsafe extern "C" fn(*mut ShimObject) -> isize =
            unsafe { std::mem::transmute(hash_function as *const c_void) };
        let _gripper = GilGuard::new();
        // SAFETY: `tp_hash` has this signature.
        let hash = unsafe { hash_function(object) };
        if hash == -1 && error_pending() {
            return Err(take_error().unwrap_or_else(|| "SystemError: tp_hash failed".to_string()));
        }
        return Ok(if hash == -1 { -2 } else { hash });
    }
    let value = match kind_ref(object) {
        Some(ShimKind::None) => 0x1F0F_0F0F,
        Some(ShimKind::Bool(flag)) => *flag as isize,
        Some(ShimKind::Int(number)) => {
            if *number == -1 {
                -2
            } else {
                *number as isize
            }
        }
        Some(ShimKind::Float(number)) => {
            if *number == 0.0 {
                0
            } else {
                number.to_bits() as isize
            }
        }
        Some(ShimKind::Str { text, .. }) => hash_bytes(text.as_bytes()),
        Some(ShimKind::Bytes { data, .. }) => hash_bytes(data),
        Some(ShimKind::Tuple(items)) => {
            let mut hash: isize = 0x345678;
            for &item in items {
                let child = hash_object(item as *mut ShimObject, depth + 1)?;
                hash = hash
                    .wrapping_mul(1000003)
                    .wrapping_add(child)
                    .wrapping_add(0x9E37);
            }
            hash
        }
        None if types::is_type_pointer(object) => {
            return Ok(object as isize);
        }
        // Hydro values hash through Python's rules: classes and instances by
        // identity (or a user `__hash__`), containers by content.
        Some(ShimKind::Hydro(handle)) => {
            let _gripper = GilGuard::new();
            let hash = crate::builtins::hash(handle.value());
            if crate::exc::pending() {
                return Err(take_hydro_error("hash() failed"));
            }
            match crate::value::as_integer(hash) {
                Some(hash) => hash as isize,
                None => -2,
            }
        }
        _ => {
            return Err(format!(
                "TypeError: unhashable type: '{}'",
                object_type_name(object)
            ))
        }
    };
    Ok(if value == -1 { -2 } else { value })
}

fn hash_bytes(bytes: &[u8]) -> isize {
    let mut hash: u64 = 1469598103934665603;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    hash as isize
}

// ---------------------------------------------------------------------------
// Accessors used by `parse.c`
// ---------------------------------------------------------------------------

/// Number of positional arguments in a tuple object (0 for non-tuples).
#[no_mangle]
pub extern "C" fn hydro_capi_arg_count(args: *mut ShimObject) -> usize {
    match kind_ref(args) {
        Some(ShimKind::Tuple(items)) => items.len(),
        _ => 0,
    }
}

/// Positional argument at `index`, or NULL when out of range.
#[no_mangle]
pub extern "C" fn hydro_capi_arg(args: *mut ShimObject, index: usize) -> *mut ShimObject {
    match kind_ref(args) {
        Some(ShimKind::Tuple(items)) => items
            .get(index)
            .map(|item| *item as *mut ShimObject)
            .unwrap_or(ptr::null_mut()),
        _ => ptr::null_mut(),
    }
}

/// `1` when `keywords` is NULL or a shim dict, `0` for any other object.
#[no_mangle]
pub extern "C" fn hydro_capi_kw_is_dict(keywords: *mut ShimObject) -> c_int {
    if keywords.is_null() {
        return 1;
    }
    matches!(kind_ref(keywords), Some(ShimKind::Dict(_))) as c_int
}

/// Number of entries in a keyword dict.
#[no_mangle]
pub extern "C" fn hydro_capi_kw_count(keywords: *mut ShimObject) -> usize {
    dict_slots(keywords).map(|pairs| pairs.len()).unwrap_or(0)
}

/// Key of keyword entry `index`, or NULL when it is not a string.
#[no_mangle]
pub extern "C" fn hydro_capi_kw_key(keywords: *mut ShimObject, index: usize) -> *const c_char {
    let Ok(pairs) = dict_slots(keywords) else {
        return ptr::null();
    };
    match pairs.get(index) {
        Some(&(key, _)) => as_c_string(key as *mut ShimObject).unwrap_or(ptr::null()),
        None => ptr::null(),
    }
}

/// Value of keyword entry `index`, or NULL when out of range.
#[no_mangle]
pub extern "C" fn hydro_capi_kw_value(keywords: *mut ShimObject, index: usize) -> *mut ShimObject {
    let Ok(pairs) = dict_slots(keywords) else {
        return ptr::null_mut();
    };
    pairs
        .get(index)
        .map(|&(_, value)| value as *mut ShimObject)
        .unwrap_or(ptr::null_mut())
}

/// Records an error constructed on the C side of `PyArg_ParseTuple`.
#[no_mangle]
pub extern "C" fn hydro_capi_set_error(message: *const c_char) {
    if message.is_null() {
        set_error("extension reported an error");
    } else {
        set_error(read_c_string(message));
    }
}

/// `"i"` conversion: C `int`.
#[no_mangle]
pub extern "C" fn hydro_capi_parse_i(object: *mut ShimObject, out: *mut c_int) -> c_int {
    match as_int(object).and_then(|value| {
        i32::try_from(value).map_err(|_| "integer out of range for C int".to_string())
    }) {
        Ok(value) => store_argument(out, value, "i"),
        Err(error) => {
            set_type_error(error);
            0
        }
    }
}

/// `"l"` conversion: C `long`.
#[no_mangle]
pub extern "C" fn hydro_capi_parse_l(object: *mut ShimObject, out: *mut c_long) -> c_int {
    match as_int(object).and_then(|value| {
        c_long::try_from(value).map_err(|_| "integer out of range for C long".to_string())
    }) {
        Ok(value) => store_argument(out, value, "l"),
        Err(error) => {
            set_type_error(error);
            0
        }
    }
}

/// `"n"` conversion: `Py_ssize_t`.
#[no_mangle]
pub extern "C" fn hydro_capi_parse_n(object: *mut ShimObject, out: *mut isize) -> c_int {
    match as_int(object)
        .and_then(|value| isize::try_from(value).map_err(|_| "integer out of range".to_string()))
    {
        Ok(value) => store_argument(out, value, "n"),
        Err(error) => {
            set_type_error(error);
            0
        }
    }
}

/// `"d"` conversion: C `double`.
#[no_mangle]
pub extern "C" fn hydro_capi_parse_d(object: *mut ShimObject, out: *mut f64) -> c_int {
    match as_float(object) {
        Ok(value) => store_argument(out, value, "d"),
        Err(error) => {
            set_type_error(error);
            0
        }
    }
}

/// `"s"` conversion: `const char *`.
#[no_mangle]
pub extern "C" fn hydro_capi_parse_s(object: *mut ShimObject, out: *mut *const c_char) -> c_int {
    match as_c_string(object) {
        Ok(value) => store_argument(out, value, "s"),
        Err(error) => {
            set_type_error(error);
            0
        }
    }
}

/// `"O"` conversion: the object pointer itself.
#[no_mangle]
pub extern "C" fn hydro_capi_parse_o(object: *mut ShimObject, out: *mut *mut ShimObject) -> c_int {
    if object.is_null() {
        set_type_error("cannot convert a NULL object");
        return 0;
    }
    store_argument(out, object, "O")
}

/// `"p"` conversion: C `int` truth value.
#[no_mangle]
pub extern "C" fn hydro_capi_parse_p(object: *mut ShimObject, out: *mut c_int) -> c_int {
    match as_predicate(object) {
        Ok(value) => store_argument(out, value as c_int, "p"),
        Err(error) => {
            set_type_error(error);
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Raised exception state, exception classes and warning output
// ---------------------------------------------------------------------------

/// `PyErr_GetRaisedException`: new reference to the raised exception, or NULL
/// with no exception set. Clears the error indicator.
#[no_mangle]
pub extern "C" fn PyErr_GetRaisedException() -> *mut ShimObject {
    let (message, exception) = CALL_ERROR.with(|slot| {
        let mut state = slot.borrow_mut();
        (state.message.take(), state.exception.take())
    });
    match (message, exception) {
        (None, None) => ptr::null_mut(),
        // The state owned one reference; hand it to the caller.
        (_, Some(exception)) => exception as *mut ShimObject,
        (Some(message), None) => exception_from_message(&message),
    }
}

/// `PyErr_SetRaisedException`: steals a reference to `exception` and installs
/// it as the raised exception; a NULL argument clears the error.
#[no_mangle]
pub extern "C" fn PyErr_SetRaisedException(exception: *mut ShimObject) {
    if exception.is_null() {
        clear_error();
        return;
    }
    let message = match kind_ref(exception) {
        Some(kind @ ShimKind::Exception { .. }) => {
            format!("{}: {}", exception_prefix(exception), exception_text(kind))
        }
        _ => format!("{}: {}", object_type_name(exception), shim_repr(exception)),
    };
    replace_error_state(Some(message), Some(exception as usize));
}

/// `PyErr_GivenExceptionMatches`.
#[no_mangle]
pub extern "C" fn PyErr_GivenExceptionMatches(
    given: *mut ShimObject,
    exception: *mut ShimObject,
) -> c_int {
    if given.is_null() || exception.is_null() {
        return 0;
    }
    if let Some(ShimKind::Tuple(items)) = kind_ref(exception) {
        return items
            .iter()
            .any(|&item| PyErr_GivenExceptionMatches(given, item as *mut ShimObject) != 0)
            as c_int;
    }
    let given = exception_class_of(given);
    let exception = exception_class_of(exception);
    if types::is_type_pointer(given) && types::is_type_pointer(exception) {
        let derived = types::is_subtype(given as *mut TypeObject, exception as *mut TypeObject);
        return derived as c_int;
    }
    (given == exception) as c_int
}

/// The class of an exception class or instance, or the object itself.
fn exception_class_of(object: *mut ShimObject) -> *mut ShimObject {
    match kind_ref(object) {
        Some(ShimKind::Exception { exc_type, .. }) => *exc_type as *mut ShimObject,
        _ => object,
    }
}

/// `PyErr_NewExceptionWithDoc`.
#[no_mangle]
pub extern "C" fn PyErr_NewExceptionWithDoc(
    name: *const c_char,
    doc: *const c_char,
    base: *mut ShimObject,
    dict: *mut ShimObject,
) -> *mut ShimObject {
    if name.is_null() {
        PyErr_BadInternalCall();
        return ptr::null_mut();
    }
    let full_name = read_c_string(name);
    let Some((module, short)) = full_name.rsplit_once('.') else {
        set_error("SystemError: PyErr_NewException: name must be module.class");
        return ptr::null_mut();
    };
    if short.is_empty() || module.is_empty() {
        set_error("SystemError: PyErr_NewException: name must be module.class");
        return ptr::null_mut();
    }
    initialize_exception_types();
    let base = if base.is_null() {
        unsafe { PyExc_Exception }
    } else {
        base
    };
    if !types::is_type_pointer(base) {
        set_error(format!(
            "TypeError: PyErr_NewException: base must be an exception class, not '{}'",
            object_type_name(base)
        ));
        return ptr::null_mut();
    }
    if !dict.is_null() && !matches!(kind_ref(dict), Some(ShimKind::Dict(_))) {
        set_error("TypeError: PyErr_NewException: dict must be a dict");
        return ptr::null_mut();
    }
    // `Py_TPFLAGS_BASE_EXC_SUBCLASS` is what PyO3's `PyExceptionClass_Check`
    // macro tests when it validates an exception class returned here.
    let type_object = types::allocate_exception_type(
        &full_name,
        PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_BASETYPE | (1 << 30),
    );
    if type_object.is_null() {
        set_error("MemoryError: cannot allocate the exception class");
        return ptr::null_mut();
    }
    // The class is a real member of the exception hierarchy, so
    // `PyErr_GivenExceptionMatches` and `issubclass` see the relation.
    set_exception_base(type_object as *mut ShimObject, base);
    let doc_text = if !doc.is_null() {
        Some(read_c_string(doc))
    } else {
        dict_doc(dict)
    };
    if let Some(doc_text) = doc_text {
        if let Ok(doc_c) = CString::new(doc_text) {
            // SAFETY: `type_object` is uniquely owned here; `tp_doc` is
            // documented as an extension-lifetime string, so the copy leaks
            // with the class exactly like CPython's static docstrings.
            unsafe {
                (*type_object).tp_doc = doc_c.into_raw() as *const c_char;
            }
        }
    }
    type_object as *mut ShimObject
}

/// Reads `__doc__` from a caller-supplied class dictionary, if present.
fn dict_doc(dict: *mut ShimObject) -> Option<String> {
    if dict.is_null() {
        return None;
    }
    let key = new_str("__doc__".to_string());
    let value = PyDict_GetItem(dict, key);
    dec_ref(key);
    as_string(value)
}

/// `PyErr_PrintEx`: prints the raised exception to stderr and clears it.
#[no_mangle]
pub extern "C" fn PyErr_PrintEx(_set_sys_last_vars: c_int) {
    let exception = PyErr_GetRaisedException();
    if exception.is_null() {
        return;
    }
    let (name, text) = exception_display_parts(exception);
    if let Some(ShimKind::Exception { traceback, .. }) = kind_ref(exception) {
        if *traceback != 0 {
            eprintln!("Traceback (most recent call last):");
        }
    }
    if text.is_empty() {
        eprintln!("{name}");
    } else {
        eprintln!("{name}: {text}");
    }
    dec_ref(exception);
}

/// Type name and message of a handle for the printing entry points.
fn exception_display_parts(exception: *mut ShimObject) -> (String, String) {
    match kind_ref(exception) {
        Some(kind @ ShimKind::Exception { .. }) => {
            let name = types::type_name(match kind {
                ShimKind::Exception { exc_type, .. } => *exc_type as *mut TypeObject,
                _ => unreachable!(),
            });
            (name, exception_text(kind))
        }
        _ => (object_type_name(exception), value_text(exception)),
    }
}

/// `PyErr_WriteUnraisable`: reports an exception that cannot be raised, then
/// clears it.
#[no_mangle]
pub extern "C" fn PyErr_WriteUnraisable(object: *mut ShimObject) {
    let exception = PyErr_GetRaisedException();
    if exception.is_null() {
        return;
    }
    if !object.is_null() && object != ptr::addr_of_mut!(_Py_NoneStruct) {
        eprintln!("Exception ignored in: {}", value_text(object));
    }
    let (name, text) = exception_display_parts(exception);
    if text.is_empty() {
        eprintln!("{name}");
    } else {
        eprintln!("{name}: {text}");
    }
    dec_ref(exception);
}

/// Tag of a warning category class, or `None` when it is not a warning.
fn warning_category_tag(category: *mut ShimObject) -> Option<u32> {
    if category.is_null() {
        return Some(crate::exc::USER_WARNING);
    }
    if !types::is_type_pointer(category) {
        return None;
    }
    let name = types::type_name(category as *mut TypeObject);
    let short = name.rsplit('.').next().unwrap_or(&name);
    crate::modules::extension::exception_tag(short)
}

/// `PyErr_WarnEx`: emits a warning through the runtime `warnings` module.
#[no_mangle]
pub extern "C" fn PyErr_WarnEx(
    category: *mut ShimObject,
    message: *const c_char,
    stack_level: isize,
) -> c_int {
    if message.is_null() {
        PyErr_BadInternalCall();
        return -1;
    }
    let Some(tag) = warning_category_tag(category) else {
        set_error("TypeError: category must be a Warning subclass");
        return -1;
    };
    if !crate::exc::tag_is_subclass(tag, crate::exc::WARNING) {
        set_error("TypeError: category must be a Warning subclass");
        return -1;
    }
    let text = read_c_string(message);
    let message_value = crate::value::str_alloc(text);
    let category_value = if category.is_null() {
        crate::value::NONE
    } else {
        crate::exc::exception_class(tag)
    };
    let stack_value = crate::ops::int_result(stack_level as i64);
    let result =
        crate::modules::warnings::call("warn", &[message_value, category_value, stack_value]);
    if crate::exc::pending() {
        set_error(take_hydro_error("warning could not be emitted"));
        return -1;
    }
    debug_assert_eq!(result, crate::value::NONE);
    0
}

/// `PyException_GetCause`: new reference to `__cause__`, or NULL.
#[no_mangle]
pub extern "C" fn PyException_GetCause(exception: *mut ShimObject) -> *mut ShimObject {
    match kind_ref(exception) {
        Some(ShimKind::Exception { cause, .. }) if *cause != 0 => {
            let cause = *cause as *mut ShimObject;
            inc_ref(cause);
            cause
        }
        Some(ShimKind::Exception { .. }) => ptr::null_mut(),
        _ => {
            set_error("SystemError: PyException_GetCause: not an exception instance");
            ptr::null_mut()
        }
    }
}

/// `PyException_SetCause`: steals a reference to `cause`.
#[no_mangle]
pub extern "C" fn PyException_SetCause(exception: *mut ShimObject, cause: *mut ShimObject) {
    if exception.is_null() {
        set_error("SystemError: PyException_SetCause: not an exception instance");
        return;
    }
    // SAFETY: the object is live, the GIL is held and only the cause slot is
    // written while this frame owns the exclusive borrow.
    let ShimKind::Exception { cause: slot, .. } = (unsafe { kind_mut(exception) }) else {
        set_error("SystemError: PyException_SetCause: not an exception instance");
        return;
    };
    let previous = std::mem::replace(slot, cause as usize);
    if previous != 0 && previous != cause as usize {
        dec_ref(previous as *mut ShimObject);
    }
    // CPython also sets `__suppress_context__ = True`, which this host does not
    // expose as an attribute slot.
}

/// `PyException_GetTraceback`: new reference to `__traceback__`, or NULL.
#[no_mangle]
pub extern "C" fn PyException_GetTraceback(exception: *mut ShimObject) -> *mut ShimObject {
    match kind_ref(exception) {
        Some(ShimKind::Exception { traceback, .. }) if *traceback != 0 => {
            let traceback = *traceback as *mut ShimObject;
            inc_ref(traceback);
            traceback
        }
        Some(ShimKind::Exception { .. }) => ptr::null_mut(),
        _ => {
            set_error("SystemError: PyException_GetTraceback: not an exception instance");
            ptr::null_mut()
        }
    }
}

/// `PyException_SetTraceback`: stores `traceback` (or `None`), returning 0 on
/// success and -1 with a `TypeError` for a non-traceback value.
#[no_mangle]
pub extern "C" fn PyException_SetTraceback(
    exception: *mut ShimObject,
    traceback: *mut ShimObject,
) -> c_int {
    if !traceback.is_null()
        && traceback != ptr::addr_of_mut!(_Py_NoneStruct)
        && type_of(traceback) != ptr::addr_of_mut!(types::TRACEBACK_TYPE)
    {
        set_error("TypeError: __traceback__ must be a traceback or None");
        return -1;
    }
    let Some(kind) = kind_ref(exception) else {
        set_error("SystemError: PyException_SetTraceback: not an exception instance");
        return -1;
    };
    if !matches!(kind, ShimKind::Exception { .. }) {
        set_error("SystemError: PyException_SetTraceback: not an exception instance");
        return -1;
    }
    let stored = if traceback.is_null() || traceback == ptr::addr_of_mut!(_Py_NoneStruct) {
        0
    } else {
        traceback as usize
    };
    // SAFETY: see `PyException_SetCause`; the kind was just validated.
    let ShimKind::Exception {
        traceback: slot, ..
    } = (unsafe { kind_mut(exception) })
    else {
        unreachable!("kind validated above")
    };
    let previous = std::mem::replace(slot, stored);
    if previous != 0 && previous != stored {
        dec_ref(previous as *mut ShimObject);
    }
    0
}

// ---------------------------------------------------------------------------
// Dictionary, set, mapping, list and iterator API
// ---------------------------------------------------------------------------

/// Reports `PyErr_BadInternalCall` for a wrong or NULL container argument.
fn bad_internal_call() {
    set_error("SystemError: bad argument to internal function");
}

/// `PyDict_Clear`.
#[no_mangle]
pub extern "C" fn PyDict_Clear(object: *mut ShimObject) {
    if object.is_null() || !matches!(kind_ref(object), Some(ShimKind::Dict(_))) {
        bad_internal_call();
        return;
    }
    let removed = match dict_slots_mut(object) {
        Ok(pairs) => std::mem::take(pairs),
        Err(error) => {
            set_error(error);
            return;
        }
    };
    for (key, value) in removed {
        dec_ref(key as *mut ShimObject);
        dec_ref(value as *mut ShimObject);
    }
    sync_dict_prefix(object);
}

/// `PyDict_Contains`: 1 present, 0 absent, -1 error.
#[no_mangle]
pub extern "C" fn PyDict_Contains(object: *mut ShimObject, key: *mut ShimObject) -> c_int {
    if key.is_null() {
        bad_internal_call();
        return -1;
    }
    if !hashable(key) {
        set_error(format!(
            "TypeError: unhashable type: '{}'",
            object_type_name(key)
        ));
        return -1;
    }
    if object.is_null() || !matches!(kind_ref(object), Some(ShimKind::Dict(_))) {
        bad_internal_call();
        return -1;
    }
    let pairs = match dict_slots(object) {
        Ok(pairs) => pairs,
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    dict_position(pairs, key).is_some() as c_int
}

/// `PyDict_Copy`: a shallow copy, or NULL with an exception.
#[no_mangle]
pub extern "C" fn PyDict_Copy(object: *mut ShimObject) -> *mut ShimObject {
    if object.is_null() || !matches!(kind_ref(object), Some(ShimKind::Dict(_))) {
        bad_internal_call();
        return ptr::null_mut();
    }
    let pairs = match dict_slots(object) {
        Ok(pairs) => pairs,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    for &(key, value) in pairs {
        inc_ref(key as *mut ShimObject);
        inc_ref(value as *mut ShimObject);
    }
    new_object(ShimKind::Dict(pairs.to_vec()))
}

/// `PyDict_DelItem`.
#[no_mangle]
pub extern "C" fn PyDict_DelItem(object: *mut ShimObject, key: *mut ShimObject) -> c_int {
    if key.is_null() {
        bad_internal_call();
        return -1;
    }
    if !hashable(key) {
        set_error(format!(
            "TypeError: unhashable type: '{}'",
            object_type_name(key)
        ));
        return -1;
    }
    if object.is_null() || !matches!(kind_ref(object), Some(ShimKind::Dict(_))) {
        bad_internal_call();
        return -1;
    }
    let position = {
        let pairs = match dict_slots(object) {
            Ok(pairs) => pairs,
            Err(error) => {
                set_error(error);
                return -1;
            }
        };
        dict_position(pairs, key)
    };
    let Some(position) = position else {
        set_error(format!("KeyError: {}", shim_repr(key)));
        return -1;
    };
    let removed = {
        let pairs = match dict_slots_mut(object) {
            Ok(pairs) => pairs,
            Err(error) => {
                set_error(error);
                return -1;
            }
        };
        pairs.remove(position)
    };
    dec_ref(removed.0 as *mut ShimObject);
    dec_ref(removed.1 as *mut ShimObject);
    sync_dict_prefix(object);
    0
}

/// `PyDict_GetItemRef`: 1 with a new reference in `*result`, 0 when missing,
/// -1 with an exception.
#[no_mangle]
pub extern "C" fn PyDict_GetItemRef(
    object: *mut ShimObject,
    key: *mut ShimObject,
    result: *mut *mut ShimObject,
) -> c_int {
    if result.is_null() {
        bad_internal_call();
        return -1;
    }
    // SAFETY: the caller passed a writable slot for the borrowed result.
    unsafe { *result = ptr::null_mut() };
    if key.is_null() {
        bad_internal_call();
        return -1;
    }
    if !hashable(key) {
        set_error(format!(
            "TypeError: unhashable type: '{}'",
            object_type_name(key)
        ));
        return -1;
    }
    if object.is_null() || !matches!(kind_ref(object), Some(ShimKind::Dict(_))) {
        bad_internal_call();
        return -1;
    }
    let pairs = match dict_slots(object) {
        Ok(pairs) => pairs,
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    match dict_position(pairs, key) {
        Some(position) => {
            let value = pairs[position].1 as *mut ShimObject;
            inc_ref(value);
            // SAFETY: validated above.
            unsafe { *result = value };
            1
        }
        None => 0,
    }
}

/// `PyDict_Keys`: a new list of the keys, or NULL with an exception.
#[no_mangle]
pub extern "C" fn PyDict_Keys(object: *mut ShimObject) -> *mut ShimObject {
    if object.is_null() || !matches!(kind_ref(object), Some(ShimKind::Dict(_))) {
        bad_internal_call();
        return ptr::null_mut();
    }
    let pairs = match dict_slots(object) {
        Ok(pairs) => pairs,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    for &(key, _) in pairs {
        inc_ref(key as *mut ShimObject);
    }
    new_object(ShimKind::List(pairs.iter().map(|&(key, _)| key).collect()))
}

/// `PyDict_Next`: iterates with a caller-held position. Returns 0 at the end
/// (also for a non-dict, as CPython's `_PyDict_Next` does).
#[no_mangle]
pub extern "C" fn PyDict_Next(
    object: *mut ShimObject,
    position: *mut isize,
    key: *mut *mut ShimObject,
    value: *mut *mut ShimObject,
) -> c_int {
    if position.is_null() {
        bad_internal_call();
        return 0;
    }
    let pairs = match kind_ref(object) {
        Some(ShimKind::Dict(pairs)) => pairs,
        _ => return 0,
    };
    // SAFETY: the caller passed a writable position slot.
    let index = unsafe { *position };
    let Ok(index) = usize::try_from(index) else {
        return 0;
    };
    let Some(&(entry_key, entry_value)) = pairs.get(index) else {
        return 0;
    };
    // SAFETY: `key`/`value` are either NULL or writable slots.
    unsafe {
        if !key.is_null() {
            *key = entry_key as *mut ShimObject;
        }
        if !value.is_null() {
            *value = entry_value as *mut ShimObject;
        }
        *position = index as isize + 1;
    }
    1
}

/// `PyDict_Update`: merges a dict or mapping into `object`.
#[no_mangle]
pub extern "C" fn PyDict_Update(object: *mut ShimObject, other: *mut ShimObject) -> c_int {
    if object.is_null() || other.is_null() || !matches!(kind_ref(object), Some(ShimKind::Dict(_))) {
        bad_internal_call();
        return -1;
    }
    if matches!(kind_ref(other), Some(ShimKind::Dict(_))) {
        let pairs = match dict_slots(other) {
            Ok(pairs) => pairs,
            Err(error) => {
                set_error(error);
                return -1;
            }
        };
        for &(key, value) in pairs {
            if let Err(error) = dict_set(object, key as *mut ShimObject, value as *mut ShimObject) {
                set_error(error);
                return -1;
            }
        }
        return 0;
    }
    // Not a dict: require the mapping protocol (`keys()` plus item access).
    let keys = match attribute_lookup(other, "keys") {
        Ok(keys) => keys,
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    let keys = {
        let result = PyObject_CallNoArgs(keys);
        dec_ref(keys);
        result
    };
    if keys.is_null() {
        return -1;
    }
    let entries = match iterable_elements(keys) {
        Ok(entries) => entries,
        Err(error) => {
            dec_ref(keys);
            set_error(error);
            return -1;
        }
    };
    dec_ref(keys);
    for &key in &entries {
        let value = types::PyObject_GetItem(other, key as *mut ShimObject);
        if value.is_null() {
            let error = take_error()
                .unwrap_or_else(|| "TypeError: cannot read the mapping value".to_string());
            release_elements(&entries);
            set_error(error);
            return -1;
        }
        let status = dict_set(object, key as *mut ShimObject, value);
        dec_ref(value);
        if let Err(error) = status {
            release_elements(&entries);
            set_error(error);
            return -1;
        }
    }
    release_elements(&entries);
    0
}

/// `PyMapping_Items`: a list of `(key, value)` tuples.
#[no_mangle]
pub extern "C" fn PyMapping_Items(object: *mut ShimObject) -> *mut ShimObject {
    if object.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    let pairs: Vec<(usize, usize)> = if let Some(ShimKind::Dict(pairs)) = kind_ref(object) {
        pairs.clone()
    } else {
        let items = match attribute_lookup(object, "items") {
            Ok(items) => items,
            Err(_) => {
                set_error(format!(
                    "TypeError: argument of type '{}' is not a mapping",
                    object_type_name(object)
                ));
                return ptr::null_mut();
            }
        };
        let result = PyObject_CallNoArgs(items);
        dec_ref(items);
        if result.is_null() {
            return ptr::null_mut();
        }
        let entries = match iterable_elements(result) {
            Ok(entries) => entries,
            Err(error) => {
                dec_ref(result);
                set_error(error);
                return ptr::null_mut();
            }
        };
        let collected: Result<Vec<(usize, usize)>, String> = entries
            .iter()
            .map(|&entry| {
                let pair = iterable_elements(entry as *mut ShimObject)?;
                if pair.len() != 2 {
                    release_elements(&pair);
                    return Err(format!(
                        "TypeError: mapping items must be (key, value) pairs, got {} elements",
                        pair.len()
                    ));
                }
                let pair = (pair[0], pair[1]);
                inc_ref(pair.0 as *mut ShimObject);
                inc_ref(pair.1 as *mut ShimObject);
                Ok(pair)
            })
            .collect();
        release_elements(&entries);
        dec_ref(result);
        match collected {
            Ok(pairs) => pairs,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        }
    };
    let mut list = Vec::with_capacity(pairs.len());
    for (key, value) in pairs {
        let tuple = PyTuple_New(2);
        if tuple.is_null() {
            release_elements(&list);
            return ptr::null_mut();
        }
        inc_ref(key as *mut ShimObject);
        inc_ref(value as *mut ShimObject);
        if PyTuple_SetItem(tuple, 0, key as *mut ShimObject) != 0
            || PyTuple_SetItem(tuple, 1, value as *mut ShimObject) != 0
        {
            let error = take_error()
                .unwrap_or_else(|| "TypeError: cannot build a mapping item".to_string());
            dec_ref(tuple);
            release_elements(&list);
            set_error(error);
            return ptr::null_mut();
        }
        list.push(tuple as usize);
    }
    new_object(ShimKind::List(list))
}

/// Releases handles returned by [`iterable_elements`].
fn release_elements(elements: &[usize]) {
    for &element in elements {
        dec_ref(element as *mut ShimObject);
    }
}

/// Collects the elements of an iterable as new references.
///
/// Shim containers are enumerated directly; extension iterators are driven
/// through `PyObject_GetIter`/`PyIter_NextItem`.
fn iterable_elements(object: *mut ShimObject) -> Result<Vec<usize>, String> {
    if object.is_null() {
        return Err("TypeError: cannot iterate a NULL object".to_string());
    }
    match kind_ref(object) {
        Some(ShimKind::List(items))
        | Some(ShimKind::Set(items))
        | Some(ShimKind::FrozenSet(items)) => {
            for &item in items {
                inc_ref(item as *mut ShimObject);
            }
            Ok(items.clone())
        }
        Some(ShimKind::Tuple(items)) => {
            for &item in items.iter() {
                inc_ref(item as *mut ShimObject);
            }
            Ok(items.to_vec())
        }
        Some(ShimKind::Dict(pairs)) => {
            let keys: Vec<usize> = pairs.iter().map(|&(key, _)| key).collect();
            for &key in &keys {
                inc_ref(key as *mut ShimObject);
            }
            Ok(keys)
        }
        Some(ShimKind::Str { text, .. }) => Ok(text
            .chars()
            .map(|character| new_str(character.to_string()) as usize)
            .collect()),
        Some(ShimKind::Bytes { data, .. }) => Ok(data
            .iter()
            .map(|&byte| new_int(byte as i64) as usize)
            .collect()),
        _ => {
            let iterator = types::PyObject_GetIter(object);
            if iterator.is_null() {
                return Err(take_error().unwrap_or_else(|| {
                    format!(
                        "TypeError: '{}' object is not iterable",
                        object_type_name(object)
                    )
                }));
            }
            let mut elements = Vec::new();
            loop {
                let mut item: *mut ShimObject = ptr::null_mut();
                match PyIter_NextItem(iterator, &mut item) {
                    1 => elements.push(item as usize),
                    0 => break,
                    _ => {
                        let error = take_error().unwrap_or_else(|| {
                            "TypeError: iterator raised an exception".to_string()
                        });
                        release_elements(&elements);
                        dec_ref(iterator);
                        return Err(error);
                    }
                }
            }
            dec_ref(iterator);
            Ok(elements)
        }
    }
}

/// Builds a set/frozenset from an optional iterable.
fn new_set(object: *mut ShimObject, frozen: bool) -> *mut ShimObject {
    let mut items: Vec<usize> = Vec::new();
    if !object.is_null() {
        let elements = match iterable_elements(object) {
            Ok(elements) => elements,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        };
        for element in elements {
            let element_ref = element as *mut ShimObject;
            if !hashable(element_ref) {
                set_error(format!(
                    "TypeError: unhashable type: '{}'",
                    object_type_name(element_ref)
                ));
                dec_ref(element_ref);
                release_elements(&items);
                return ptr::null_mut();
            }
            if !items
                .iter()
                .any(|&existing| key_equal(existing as *mut ShimObject, element_ref))
            {
                items.push(element);
            } else {
                dec_ref(element_ref);
            }
        }
    }
    new_object(if frozen {
        ShimKind::FrozenSet(items)
    } else {
        ShimKind::Set(items)
    })
}

/// `PySet_New`.
#[no_mangle]
pub extern "C" fn PySet_New(iterable: *mut ShimObject) -> *mut ShimObject {
    new_set(iterable, false)
}

/// `PyFrozenSet_New`.
#[no_mangle]
pub extern "C" fn PyFrozenSet_New(iterable: *mut ShimObject) -> *mut ShimObject {
    new_set(iterable, true)
}

/// Mutable element storage of a set handle.
fn set_slots_mut(object: *mut ShimObject) -> Option<&'static mut Vec<usize>> {
    if object.is_null() {
        return None;
    }
    // SAFETY: the handle is live and the GIL forbids concurrent mutation.
    unsafe {
        match kind_mut(object) {
            ShimKind::Set(items) => Some(items),
            _ => None,
        }
    }
}

/// Records CPython's error for mutating an immutable `frozenset`.
fn set_not_mutable() -> c_int {
    set_error("SystemError: <class 'frozenset'> is not mutable");
    -1
}

/// `PySet_Add`.
#[no_mangle]
pub extern "C" fn PySet_Add(object: *mut ShimObject, key: *mut ShimObject) -> c_int {
    if key.is_null() {
        bad_internal_call();
        return -1;
    }
    match kind_ref(object) {
        Some(ShimKind::Set(_)) => {}
        Some(ShimKind::FrozenSet(_)) => return set_not_mutable(),
        _ => {
            bad_internal_call();
            return -1;
        }
    }
    if !hashable(key) {
        set_error(format!(
            "TypeError: unhashable type: '{}'",
            object_type_name(key)
        ));
        return -1;
    }
    let Some(items) = set_slots_mut(object) else {
        bad_internal_call();
        return -1;
    };
    if items
        .iter()
        .any(|&existing| key_equal(existing as *mut ShimObject, key))
    {
        return 0;
    }
    inc_ref(key);
    items.push(key as usize);
    0
}

/// `PySet_Contains`: 1 present, 0 absent, -1 error.
#[no_mangle]
pub extern "C" fn PySet_Contains(object: *mut ShimObject, key: *mut ShimObject) -> c_int {
    if key.is_null() {
        bad_internal_call();
        return -1;
    }
    let items = match kind_ref(object) {
        Some(ShimKind::Set(items)) | Some(ShimKind::FrozenSet(items)) => items,
        _ => {
            bad_internal_call();
            return -1;
        }
    };
    if !hashable(key) {
        set_error(format!(
            "TypeError: unhashable type: '{}'",
            object_type_name(key)
        ));
        return -1;
    }
    items
        .iter()
        .any(|&existing| key_equal(existing as *mut ShimObject, key)) as c_int
}

/// `PySet_Size`.
#[no_mangle]
pub extern "C" fn PySet_Size(object: *mut ShimObject) -> isize {
    match kind_ref(object) {
        Some(ShimKind::Set(items)) | Some(ShimKind::FrozenSet(items)) => py_ssize_of(items.len()),
        _ => {
            bad_internal_call();
            -1
        }
    }
}

/// `PyList_AsTuple`.
#[no_mangle]
pub extern "C" fn PyList_AsTuple(object: *mut ShimObject) -> *mut ShimObject {
    if object.is_null() || !matches!(kind_ref(object), Some(ShimKind::List(_))) {
        bad_internal_call();
        return ptr::null_mut();
    }
    let items = match list_slots(object) {
        Ok(items) => items,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    if items.is_empty() {
        return empty_tuple();
    }
    for &item in items {
        if item == 0 {
            set_error("SystemError: list contains an uninitialized item");
            return ptr::null_mut();
        }
        inc_ref(item as *mut ShimObject);
    }
    new_tuple(items.to_vec())
}

/// `PyList_GetSlice`: a new list for the clamped `[low, high)` range.
#[no_mangle]
pub extern "C" fn PyList_GetSlice(
    object: *mut ShimObject,
    low: isize,
    high: isize,
) -> *mut ShimObject {
    if object.is_null() || !matches!(kind_ref(object), Some(ShimKind::List(_))) {
        bad_internal_call();
        return ptr::null_mut();
    }
    let items = match list_slots(object) {
        Ok(items) => items,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let length = py_ssize_of(items.len());
    let low = low.max(0).min(length);
    let high = high.max(low).min(length);
    let slice = &items[low as usize..high as usize];
    for &item in slice {
        if item == 0 {
            set_error("SystemError: list contains an uninitialized item");
            return ptr::null_mut();
        }
        inc_ref(item as *mut ShimObject);
    }
    new_object(ShimKind::List(slice.to_vec()))
}

/// `PyIter_NextItem`: 1 with a new reference in `*item`, 0 when exhausted
/// (clearing `StopIteration`), -1 with an exception.
#[no_mangle]
pub extern "C" fn PyIter_NextItem(iterator: *mut ShimObject, item: *mut *mut ShimObject) -> c_int {
    if item.is_null() {
        bad_internal_call();
        return -1;
    }
    // SAFETY: the caller passed a writable slot.
    unsafe { *item = ptr::null_mut() };
    if iterator.is_null() {
        set_error("TypeError: expected an iterator, got 'NULL'");
        return -1;
    }
    let type_object = type_of(iterator);
    // SAFETY: live type object.
    let next = unsafe { (*type_object).tp_iternext };
    if next == 0 {
        set_error(format!(
            "TypeError: expected an iterator, got '{}'",
            object_type_name(iterator)
        ));
        return -1;
    }
    // SAFETY: `iternextfunc` has this signature.
    let next: unsafe extern "C" fn(*mut ShimObject) -> *mut ShimObject =
        unsafe { std::mem::transmute(next as *const c_void) };
    let _gripper = GilGuard::new();
    let result = unsafe { next(iterator) };
    if !result.is_null() {
        // SAFETY: validated slot.
        unsafe { *item = result };
        return 1;
    }
    if !error_pending() {
        return 0;
    }
    if pending_error_matches(unsafe { PyExc_StopIteration }) {
        clear_error();
        return 0;
    }
    -1
}

/// True when the pending error's class matches `class` (a type handle).
fn pending_error_matches(class: *mut ShimObject) -> bool {
    let Some(message) = CALL_ERROR.with(|slot| slot.borrow().message.clone()) else {
        return false;
    };
    let name = message
        .split_once(": ")
        .map(|(name, _)| name)
        .unwrap_or(&message);
    let pending = {
        let known = exception_class_by_name(name);
        if known.is_null() {
            // Module-qualified custom classes: match on the short name.
            let short = name.rsplit('.').next().unwrap_or(name);
            exception_class_by_name(short)
        } else {
            known
        }
    };
    if pending.is_null() || class.is_null() {
        return false;
    }
    let other = exception_class_of(class);
    types::is_type_pointer(pending)
        && types::is_type_pointer(other)
        && types::is_subtype(pending as *mut TypeObject, other as *mut TypeObject)
}

/// `PyByteArray_FromStringAndSize`.
#[no_mangle]
pub extern "C" fn PyByteArray_FromStringAndSize(
    value: *const c_char,
    length: isize,
) -> *mut ShimObject {
    if length < 0 {
        set_error("SystemError: Negative size passed to PyByteArray_FromStringAndSize");
        return ptr::null_mut();
    }
    let Ok(length) = usize::try_from(length) else {
        set_error("MemoryError: bytearray is too large");
        return ptr::null_mut();
    };
    if value.is_null() && length != 0 {
        set_error("SystemError: NULL bytearray buffer with a positive size");
        return ptr::null_mut();
    }
    let data = if length == 0 {
        Vec::new()
    } else {
        // SAFETY: the caller passes `length` readable bytes.
        unsafe { std::slice::from_raw_parts(value as *const u8, length).to_vec() }
    };
    let mut c_data = data.clone();
    c_data.push(0);
    new_object(ShimKind::ByteArray { data, c_data })
}

/// `PyByteArray_Size`.
#[no_mangle]
pub extern "C" fn PyByteArray_Size(object: *mut ShimObject) -> isize {
    match kind_ref(object) {
        Some(ShimKind::ByteArray { data, .. }) => py_ssize_of(data.len()),
        _ => {
            bad_internal_call();
            -1
        }
    }
}

/// `PyByteArray_AsString`: the NUL-terminated mutable buffer.
#[no_mangle]
pub extern "C" fn PyByteArray_AsString(object: *mut ShimObject) -> *mut c_char {
    match kind_ref(object) {
        Some(ShimKind::ByteArray { c_data, .. }) => c_data.as_ptr() as *mut c_char,
        _ => {
            bad_internal_call();
            ptr::null_mut()
        }
    }
}

/// `PyComplex_FromDoubles`.
#[no_mangle]
pub extern "C" fn PyComplex_FromDoubles(real: f64, imag: f64) -> *mut ShimObject {
    new_object(ShimKind::Complex { real, imag })
}

/// `PyComplex_RealAsDouble`.
#[no_mangle]
pub extern "C" fn PyComplex_RealAsDouble(object: *mut ShimObject) -> f64 {
    match kind_ref(object) {
        Some(ShimKind::Complex { real, .. }) => *real,
        _ => match as_float(object) {
            Ok(value) => value,
            Err(error) => {
                set_error(format!("TypeError: {error}"));
                -1.0
            }
        },
    }
}

/// `PyComplex_ImagAsDouble`.
#[no_mangle]
pub extern "C" fn PyComplex_ImagAsDouble(object: *mut ShimObject) -> f64 {
    match kind_ref(object) {
        Some(ShimKind::Complex { imag, .. }) => *imag,
        _ => match as_float(object) {
            Ok(_) => 0.0,
            Err(error) => {
                set_error(format!("TypeError: {error}"));
                -1.0
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Long (native bytes and export) API
// ---------------------------------------------------------------------------

/// CPython's digit width on 64-bit platforms.
const PYLONG_SHIFT: u32 = 30;
/// Largest magnitude held in a single CPython digit.
const PYLONG_COMPACT_LIMIT: i64 = 1 << PYLONG_SHIFT;

/// `PyLongLayout` mirrored for `PyLong_GetNativeLayout`.
#[repr(C)]
pub struct PyLongLayoutMirror {
    pub bits_per_digit: u8,
    pub digit_size: u8,
    pub digits_order: i8,
    pub digit_endianness: i8,
}

/// `PyLongExport` mirrored for `PyLong_Export`/`PyLong_FreeExport`.
#[repr(C)]
pub struct PyLongExportMirror {
    pub value: i64,
    pub negative: u8,
    pub ndigits: isize,
    pub digits: *const c_void,
    pub reserved: usize,
}

/// `PyLong_GetNativeLayout`.
#[no_mangle]
pub extern "C" fn PyLong_GetNativeLayout() -> *const PyLongLayoutMirror {
    static LAYOUT: PyLongLayoutMirror = PyLongLayoutMirror {
        bits_per_digit: PYLONG_SHIFT as u8,
        // `digit` is `uint32_t` on every supported target.
        digit_size: 4,
        // -1: least significant digit first.
        digits_order: -1,
        // -1 on little-endian hosts, 1 on big-endian ones.
        digit_endianness: if cfg!(target_endian = "little") {
            -1
        } else {
            1
        },
    };
    &LAYOUT
}

/// `PyLong_Export`: fills `export_data` with a compact value when possible.
#[no_mangle]
pub extern "C" fn PyLong_Export(
    object: *mut ShimObject,
    export_data: *mut PyLongExportMirror,
) -> c_int {
    if export_data.is_null() {
        bad_internal_call();
        return -1;
    }
    // SAFETY: the caller passed a writable PyLongExport slot.
    unsafe { ptr::write_bytes(export_data, 0, 1) };
    let value = match kind_ref(object) {
        Some(ShimKind::Int(number)) => *number,
        Some(ShimKind::Bool(flag)) => *flag as i64,
        _ => {
            if object.is_null() {
                set_error("TypeError: expect int, got NULL");
            } else {
                set_error(format!(
                    "TypeError: expect int, got {}",
                    object_type_name(object)
                ));
            }
            return -1;
        }
    };
    // Every int this host can hold fits into `int64_t`, so the fast path from
    // CPython's `PyLong_Export` always applies and no digit array is exposed.
    // SAFETY: validated above.
    unsafe {
        (*export_data).value = value;
        (*export_data).negative = 0;
        (*export_data).ndigits = 0;
        (*export_data).digits = ptr::null();
        (*export_data).reserved = 0;
    }
    0
}

/// `PyLong_FreeExport`: releases the reference a non-compact export pinned.
#[no_mangle]
pub extern "C" fn PyLong_FreeExport(export_data: *mut PyLongExportMirror) {
    if export_data.is_null() {
        return;
    }
    // SAFETY: the caller passes its PyLongExport slot.
    let reserved = unsafe { (*export_data).reserved };
    if reserved != 0 {
        unsafe {
            (*export_data).reserved = 0;
        }
        dec_ref(reserved as *mut ShimObject);
    }
}

/// `PyLong_AsNativeBytes` endianness resolution.
fn resolve_native_endianness(flags: c_int) -> bool {
    // `-1` (all bits) and the native-endian bit select the host order.
    if flags == -1 || (flags & 2) != 0 {
        cfg!(target_endian = "little")
    } else {
        (flags & 1) != 0
    }
}

/// Whether `value` fits in `bits` two's-complement bits.
fn fits_in_n_bits(value: i64, bits: i64) -> bool {
    if bits >= 64 {
        return true;
    }
    let extended = value >> (bits - 1) as u32;
    extended == 0 || extended == -1
}

/// Writes the low `n` bytes of `value`'s two's complement, sign extended.
fn write_twos_complement(value: i64, buffer: *mut u8, n: usize, little_endian: bool) {
    for index in 0..n {
        let shift = if little_endian { index } else { n - 1 - index };
        let byte = if shift >= 8 {
            if value < 0 {
                0xFF
            } else {
                0x00
            }
        } else {
            ((value as u64) >> (shift * 8)) as u8
        };
        // SAFETY: the caller provides `n` writable bytes.
        unsafe { *buffer.add(index) = byte };
    }
}

/// Reads `n` bytes as a two's-complement (or unsigned) integer.
fn read_native_bytes(
    buffer: *const u8,
    n: usize,
    little_endian: bool,
    signed: bool,
) -> Result<i64, String> {
    if n == 0 {
        return Ok(0);
    }
    // Bytes beyond the first 16 must be pure sign extension to fit an i128.
    let mut accumulator: i128 = 0;
    for index in 0..n {
        let shift = if little_endian { index } else { n - 1 - index };
        // SAFETY: the caller provides `n` readable bytes.
        let byte = unsafe { *buffer.add(index) };
        if shift < 16 {
            accumulator |= (byte as i128) << (shift * 8);
        } else {
            let fill: u8 = if signed && accumulator < 0 {
                0xFF
            } else {
                0x00
            };
            if byte != fill {
                return Err(
                    "OverflowError: integer does not fit this host's 64-bit int model".to_string(),
                );
            }
        }
    }
    if signed && n < 16 {
        let sign_bit = 1i128 << (n * 8 - 1);
        if accumulator & sign_bit != 0 {
            accumulator |= -1i128 << (n * 8);
        }
    }
    let converted: Result<i64, _> = i64::try_from(accumulator);
    converted
        .map_err(|_| "OverflowError: integer does not fit this host's 64-bit int model".to_string())
}

/// `PyLong_FromNativeBytes`.
#[no_mangle]
pub extern "C" fn PyLong_FromNativeBytes(
    buffer: *const c_void,
    n_bytes: usize,
    flags: c_int,
) -> *mut ShimObject {
    if buffer.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    let little_endian = resolve_native_endianness(flags);
    let signed = flags == -1 || (flags & 4) == 0;
    match read_native_bytes(buffer as *const u8, n_bytes, little_endian, signed) {
        Ok(value) => new_int(value),
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyLong_AsNativeBytes`.
#[no_mangle]
pub extern "C" fn PyLong_AsNativeBytes(
    object: *mut ShimObject,
    buffer: *mut c_void,
    n_bytes: isize,
    flags: c_int,
) -> isize {
    if object.is_null() || n_bytes < 0 {
        bad_internal_call();
        return -1;
    }
    let little_endian = resolve_native_endianness(flags);
    let value = match kind_ref(object) {
        Some(ShimKind::Int(number)) => *number,
        Some(ShimKind::Bool(flag)) => *flag as i64,
        _ => {
            set_error(format!(
                "TypeError: expect int, got {}",
                object_type_name(object)
            ));
            return -1;
        }
    };
    if flags != -1 && (flags & 8) != 0 && value < 0 {
        set_error("ValueError: Cannot convert negative int");
        return -1;
    }
    let n = n_bytes as usize;
    if n == 0 {
        // The caller only asked for the required size.
        return if (-PYLONG_COMPACT_LIMIT..PYLONG_COMPACT_LIMIT).contains(&value) {
            std::mem::size_of::<isize>() as isize
        } else {
            magnitude_bytes(value) as isize
        };
    }
    if buffer.is_null() {
        bad_internal_call();
        return -1;
    }
    let unsigned_buffer = flags == -1 || (flags & 4) != 0;
    if (-PYLONG_COMPACT_LIMIT..PYLONG_COMPACT_LIMIT).contains(&value) {
        // Compact path: a full native word is copied (and truncated) exactly as
        // CPython does.
        let word_size = std::mem::size_of::<isize>();
        let mut word = [0u8; std::mem::size_of::<isize>()];
        word.copy_from_slice(&(value as isize).to_ne_bytes());
        if n <= word_size {
            for index in 0..n {
                let source = if little_endian == cfg!(target_endian = "little") {
                    index
                } else {
                    word_size - 1 - index
                };
                // SAFETY: `buffer` has `n` writable bytes.
                unsafe { *((buffer as *mut u8).add(index)) = word[source] };
            }
            let mut required = word_size;
            if fits_in_n_bits(value, (n * 8) as i64) {
                required = n;
            } else if value > 0 && fits_in_n_bits(value, n as i64 * 8 + 1) {
                required = if unsigned_buffer { n } else { n + 1 };
            }
            return required as isize;
        }
        write_twos_complement(value, buffer as *mut u8, n, little_endian);
        return word_size as isize;
    }
    // Non-compact path: write the truncated two's complement and report the
    // magnitude's byte count plus the sign bit, with CPython's two edge cases.
    write_twos_complement(value, buffer as *mut u8, n, little_endian);
    let magnitude_bits = magnitude_bits(value);
    let mut required = magnitude_bits as usize / 8 + 1;
    if required == n + 1 && magnitude_bits.is_multiple_of(8) {
        let bytes = buffer as *const u8;
        if value < 0 {
            // 0x80..00 negative values use every buffer bit for the sign.
            let mut edge = true;
            for index in 0..n {
                // SAFETY: `buffer` has `n` readable bytes.
                let byte = unsafe { *bytes.add(index) };
                let expected = if index == 0 {
                    if little_endian {
                        0x00
                    } else {
                        0x80
                    }
                } else if index < n - 1 {
                    0x00
                } else if little_endian {
                    0x80
                } else {
                    0x00
                };
                if byte != expected {
                    edge = false;
                    break;
                }
            }
            if edge {
                required = n;
            }
        } else {
            // SAFETY: `buffer` has `n` readable bytes.
            let msb = unsafe { *bytes.add(if little_endian { n - 1 } else { 0 }) };
            if msb & 0x80 != 0 {
                required = if unsigned_buffer { n } else { n + 1 };
            }
        }
    }
    required as isize
}

/// Number of bytes needed to hold the magnitude of a non-compact value.
fn magnitude_bytes(value: i64) -> usize {
    (magnitude_bits(value) as usize) / 8 + 1
}

/// Bit length of `value`'s magnitude.
fn magnitude_bits(value: i64) -> u32 {
    let magnitude = value.unsigned_abs();
    64 - magnitude.leading_zeros()
}

/// `PyLong_AsUnsignedLongLong`.
#[no_mangle]
pub extern "C" fn PyLong_AsUnsignedLongLong(object: *mut ShimObject) -> u64 {
    if object.is_null() {
        bad_internal_call();
        return u64::MAX;
    }
    let value = match kind_ref(object) {
        Some(ShimKind::Int(number)) => *number,
        Some(ShimKind::Bool(flag)) => *flag as i64,
        _ => {
            set_error("TypeError: an integer is required");
            return u64::MAX;
        }
    };
    if value < 0 {
        set_error("OverflowError: can't convert negative int to unsigned");
        return u64::MAX;
    }
    value as u64
}

/// `PyLong_FromSsize_t`.
#[no_mangle]
pub extern "C" fn PyLong_FromSsize_t(value: isize) -> *mut ShimObject {
    new_int(value as i64)
}

/// `PyLong_FromUnsignedLongLong`.
#[no_mangle]
pub extern "C" fn PyLong_FromUnsignedLongLong(value: u64) -> *mut ShimObject {
    match i64::try_from(value) {
        Ok(value) => new_int(value),
        Err(_) => {
            set_error(format!(
                "OverflowError: integer {value} does not fit this host's 64-bit int model"
            ));
            ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// Unicode API
// ---------------------------------------------------------------------------

/// Interned strings, shared by `PyUnicode_InternInPlace`.
fn intern_table() -> MutexGuard<'static, HashMap<String, usize>> {
    static TABLE: LazyLock<Mutex<HashMap<String, usize>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    TABLE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `PyUnicode_FromStringAndSize`: decodes UTF-8 into a new string.
#[no_mangle]
pub extern "C" fn PyUnicode_FromStringAndSize(
    value: *const c_char,
    length: isize,
) -> *mut ShimObject {
    if length < 0 {
        set_error("SystemError: Negative size passed to PyUnicode_FromStringAndSize");
        return ptr::null_mut();
    }
    if value.is_null() {
        if length > 0 {
            set_error(
                "SystemError: NULL string with positive size passed to \
                 PyUnicode_FromStringAndSize",
            );
            return ptr::null_mut();
        }
        return interned().empty_str as *mut ShimObject;
    }
    let Ok(length) = usize::try_from(length) else {
        set_error("MemoryError: string is too large");
        return ptr::null_mut();
    };
    // SAFETY: the caller passes `length` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(value as *const u8, length) };
    match std::str::from_utf8(bytes) {
        Ok(text) => new_str(text.to_string()),
        Err(error) => {
            let position = error.valid_up_to();
            let reason = match error.error_len() {
                Some(_) => "invalid start byte",
                None => "unexpected end of data",
            };
            set_error(format!(
                "UnicodeDecodeError: 'utf-8' codec can't decode byte 0x{:02x} in position \
                 {position}: {reason}",
                bytes.get(position).copied().unwrap_or(0)
            ));
            ptr::null_mut()
        }
    }
}

/// Code unit width CPython would choose for `text`.
fn unicode_kind_width(text: &str) -> u8 {
    let max = text
        .chars()
        .map(|character| character as u32)
        .max()
        .unwrap_or(0);
    if max < 0x100 {
        1
    } else if max < 0x10000 {
        2
    } else {
        4
    }
}

/// Encodes `text` in the UCS storage layout `PyUnicode_DATA` exposes: 1, 2 or
/// 4 bytes per code point, little endian, with a trailing NUL unit.
fn unicode_data_buffer(text: &str) -> Vec<u8> {
    let width = unicode_kind_width(text) as usize;
    let mut buffer = Vec::with_capacity((text.chars().count() + 1) * width);
    for character in text.chars() {
        let code = character as u32;
        match width {
            1 => buffer.push(code as u8),
            2 => buffer.extend_from_slice(&(code as u16).to_le_bytes()),
            _ => buffer.extend_from_slice(&code.to_le_bytes()),
        }
    }
    buffer.resize(buffer.len() + width, 0);
    buffer
}

/// `PyUnicode_New`: a string of `size` code points backed by writable UCS
/// storage of the kind selected by `maxchar`.
#[no_mangle]
pub extern "C" fn PyUnicode_New(size: isize, maxchar: c_uint) -> *mut ShimObject {
    if size < 0 {
        set_error("SystemError: Negative size passed to PyUnicode_New");
        return ptr::null_mut();
    }
    if maxchar > 0x10FFFF {
        set_error("SystemError: invalid maximum character passed to PyUnicode_New");
        return ptr::null_mut();
    }
    if size == 0 {
        return interned().empty_str as *mut ShimObject;
    }
    let Ok(size) = usize::try_from(size) else {
        set_error("MemoryError: string is too large");
        return ptr::null_mut();
    };
    let width: u8 = if maxchar < 0x100 {
        1
    } else if maxchar < 0x10000 {
        2
    } else {
        4
    };
    let Some(bytes) = size.checked_mul(width as usize) else {
        set_error("MemoryError: string is too large");
        return ptr::null_mut();
    };
    let mut data = Vec::new();
    if data.try_reserve_exact(bytes).is_err() {
        set_error("MemoryError: cannot allocate the string buffer");
        return ptr::null_mut();
    }
    data.resize(bytes, 0);
    new_object(ShimKind::Str {
        text: "\0".repeat(size),
        c_text: None,
        raw: Some(RawText { width, data }),
    })
}

/// `PyUnicode_DATA`: the string's UCS storage.
///
/// For strings created by `PyUnicode_New` this is the buffer the extension may
/// fill. Ordinary strings get a cached, correctly encoded buffer.
#[no_mangle]
pub extern "C" fn PyUnicode_DATA(object: *mut ShimObject) -> *mut c_void {
    match kind_ref(object) {
        Some(ShimKind::Str { raw: Some(raw), .. }) => raw.data.as_ptr() as *mut c_void,
        Some(ShimKind::Str { text, .. }) => {
            let text = text.clone();
            cached_buffer(object, || unicode_data_buffer(&text)) as *mut c_void
        }
        _ => {
            set_error("SystemError: PyUnicode_DATA requires a str object");
            ptr::null_mut()
        }
    }
}

/// `PyUnicode_AsUTF8AndSize`: NUL-terminated UTF-8 bytes plus their length.
#[no_mangle]
pub extern "C" fn PyUnicode_AsUTF8AndSize(
    object: *mut ShimObject,
    size: *mut isize,
) -> *const c_char {
    if !size.is_null() {
        // SAFETY: the caller passed a writable length slot.
        unsafe { *size = -1 };
    }
    let text = match kind_ref(object) {
        Some(ShimKind::Str { text, .. }) => text.clone(),
        _ => {
            // CPython's `PyErr_BadArgument`.
            set_error("TypeError: bad argument to internal function");
            return ptr::null();
        }
    };
    let length = text.len();
    let buffer = cached_buffer(object, || {
        let mut bytes = Vec::with_capacity(length + 1);
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(0);
        bytes
    });
    if !size.is_null() {
        // SAFETY: validated above.
        unsafe { *size = length as isize };
    }
    buffer as *const c_char
}

/// Validates an error handler name for `PyUnicode_AsEncodedString`.
fn unicode_error_handler(errors: &str) -> Result<&str, String> {
    match errors {
        "strict" | "ignore" | "replace" => Ok(errors),
        other => Err(format!("LookupError: unknown error handler name '{other}'")),
    }
}

/// Normalizes an encoding name the way CPython's shortcuts do.
fn normalize_encoding(encoding: &str) -> String {
    let name = encoding.trim().to_ascii_lowercase().replace('-', "_");
    match name.as_str() {
        "utf" | "utf8" | "u8" | "utf_8" => "utf_8".to_string(),
        "ascii" | "us_ascii" | "646" => "ascii".to_string(),
        "latin" | "latin1" | "latin_1" | "iso_8859_1" | "iso8859_1" | "8859" | "l1" => {
            "latin_1".to_string()
        }
        "utf16" | "utf_16" | "u16" => "utf_16".to_string(),
        "utf16le" | "utf_16le" | "utf_16_le" => "utf_16_le".to_string(),
        "utf16be" | "utf_16be" | "utf_16_be" => "utf_16_be".to_string(),
        "utf32" | "utf_32" | "u32" => "utf_32".to_string(),
        "utf32le" | "utf_32le" | "utf_32_le" => "utf_32_le".to_string(),
        "utf32be" | "utf_32be" | "utf_32_be" => "utf_32_be".to_string(),
        _ => name,
    }
}

/// Encodes `text` with `encoding`, applying `errors` for unencodable code
/// points.
fn encode_unicode_text(text: &str, encoding: &str, errors: &str) -> Result<Vec<u8>, String> {
    if encoding == "utf_16" {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        return Ok(bytes);
    }
    if encoding == "utf_16_le" || encoding == "utf_16_be" {
        let little = encoding == "utf_16_le";
        let mut bytes = Vec::with_capacity(text.len() * 2);
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&if little {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        return Ok(bytes);
    }
    if encoding == "utf_32" {
        let mut bytes = vec![0xFF, 0xFE, 0x00, 0x00];
        for character in text.chars() {
            bytes.extend_from_slice(&(character as u32).to_le_bytes());
        }
        return Ok(bytes);
    }
    if encoding == "utf_32_le" || encoding == "utf_32_be" {
        let little = encoding == "utf_32_le";
        let mut bytes = Vec::with_capacity(text.len() * 4);
        for character in text.chars() {
            let code = character as u32;
            bytes.extend_from_slice(&if little {
                code.to_le_bytes()
            } else {
                code.to_be_bytes()
            });
        }
        return Ok(bytes);
    }
    match encoding {
        "utf_8" => Ok(text.as_bytes().to_vec()),
        "ascii" | "latin_1" => {
            let limit = if encoding == "ascii" { 0x7F } else { 0xFF };
            let mut bytes = Vec::with_capacity(text.len());
            for (position, character) in text.chars().enumerate() {
                let code = character as u32;
                if code <= limit {
                    bytes.push(code as u8);
                    continue;
                }
                match errors {
                    "ignore" => {}
                    "replace" => bytes.push(b'?'),
                    _ => {
                        return Err(format!(
                            "UnicodeEncodeError: '{}' codec can't encode character \
                             '\\u{code:04x}' in position {position}: ordinal not in range({})",
                            if encoding == "ascii" {
                                "ascii"
                            } else {
                                "latin-1"
                            },
                            limit + 1
                        ))
                    }
                }
            }
            Ok(bytes)
        }
        other => Err(format!("LookupError: unknown encoding: {other}")),
    }
}

/// `PyUnicode_AsEncodedString`.
#[no_mangle]
pub extern "C" fn PyUnicode_AsEncodedString(
    object: *mut ShimObject,
    encoding: *const c_char,
    errors: *const c_char,
) -> *mut ShimObject {
    let text = match kind_ref(object) {
        Some(ShimKind::Str { text, .. }) => text.clone(),
        _ => {
            set_error("TypeError: bad argument to internal function");
            return ptr::null_mut();
        }
    };
    let errors = if errors.is_null() {
        "strict".to_string()
    } else {
        read_c_string(errors)
    };
    let normalized_errors = match unicode_error_handler(&errors) {
        Ok(errors) => errors.to_string(),
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let encoding = if encoding.is_null() {
        "utf_8".to_string()
    } else {
        normalize_encoding(&read_c_string(encoding))
    };
    match encode_unicode_text(&text, &encoding, &normalized_errors) {
        Ok(bytes) => new_bytes(bytes),
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyUnicode_EqualToUTF8AndSize`: 1 when equal, 0 otherwise.
#[no_mangle]
pub extern "C" fn PyUnicode_EqualToUTF8AndSize(
    object: *mut ShimObject,
    value: *const c_char,
    size: isize,
) -> c_int {
    if value.is_null() || size < 0 {
        return 0;
    }
    let Some(ShimKind::Str { text, .. }) = kind_ref(object) else {
        return 0;
    };
    let Ok(size) = usize::try_from(size) else {
        return 0;
    };
    // SAFETY: the caller provides `size` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(value as *const u8, size) };
    (text.as_bytes() == bytes) as c_int
}

/// `PyUnicode_InternInPlace`: replaces the string with the interned instance.
#[no_mangle]
pub extern "C" fn PyUnicode_InternInPlace(object: *mut *mut ShimObject) {
    if object.is_null() {
        return;
    }
    // SAFETY: the caller passes a writable `PyObject *` slot.
    let current = unsafe { *object };
    let Some(ShimKind::Str { text, .. }) = kind_ref(current) else {
        return;
    };
    let text = text.clone();
    let mut table = intern_table();
    if let Some(&interned) = table.get(&text) {
        if interned != current as usize {
            inc_ref(interned as *mut ShimObject);
            dec_ref(current);
            // SAFETY: validated slot.
            unsafe { *object = interned as *mut ShimObject };
        }
        return;
    }
    table.insert(text, current as usize);
    // Interned strings live for the process lifetime, like CPython's immortal
    // interned objects; this also keeps the table entry valid.
    make_immortal(current);
}

/// `PyUnicodeDecodeError_Create`: a `UnicodeDecodeError` instance.
#[no_mangle]
pub extern "C" fn PyUnicodeDecodeError_Create(
    encoding: *const c_char,
    object: *const c_char,
    length: isize,
    start: isize,
    end: isize,
    reason: *const c_char,
) -> *mut ShimObject {
    if encoding.is_null() || object.is_null() || reason.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    if length < 0 {
        set_error("SystemError: Negative size passed to PyUnicodeDecodeError_Create");
        return ptr::null_mut();
    }
    let Ok(length) = usize::try_from(length) else {
        set_error("MemoryError: error object is too large");
        return ptr::null_mut();
    };
    // SAFETY: the caller provides `length` readable bytes.
    let data = unsafe { std::slice::from_raw_parts(object as *const u8, length) }.to_vec();
    let encoding_value = new_str(read_c_string(encoding));
    let object_value = new_bytes(data);
    let start_value = new_int(start as i64);
    let end_value = new_int(end as i64);
    let reason_value = new_str(read_c_string(reason));
    let instance = new_exception_instance(
        unsafe { PyExc_UnicodeDecodeError } as usize,
        vec![
            encoding_value,
            object_value,
            start_value,
            end_value,
            reason_value,
        ],
    );
    for value in [
        encoding_value,
        object_value,
        start_value,
        end_value,
        reason_value,
    ] {
        dec_ref(value);
    }
    instance
}

// ---------------------------------------------------------------------------
// Object protocol, interpreter state and deallocation
// ---------------------------------------------------------------------------

/// Opaque `PyInterpreterState` handle for `PyInterpreterState_Get`.
#[repr(C)]
pub struct PyInterpreterState {
    _private: [u8; 0],
}

/// The single interpreter this host runs, with CPython's main-interpreter id 0.
static MAIN_INTERPRETER: u8 = 0;

/// `PyInterpreterState_Get`.
#[no_mangle]
pub extern "C" fn PyInterpreterState_Get() -> *mut PyInterpreterState {
    ptr::addr_of!(MAIN_INTERPRETER) as *mut PyInterpreterState
}

/// `PyInterpreterState_GetID`.
#[no_mangle]
pub extern "C" fn PyInterpreterState_GetID(interpreter: *mut PyInterpreterState) -> i64 {
    if interpreter.is_null() {
        set_error("ValueError: interpreter state is NULL");
        return -1;
    }
    if interpreter as *const u8 != ptr::addr_of!(MAIN_INTERPRETER) {
        set_error("ValueError: invalid interpreter state");
        return -1;
    }
    0
}

/// `Py_IsInitialized`: this host has no finalization entry point, so the
/// embedded interpreter is initialised for the lifetime of the process.
#[no_mangle]
pub extern "C" fn Py_IsInitialized() -> c_int {
    1
}

/// `Py_IsFinalizing`.
#[no_mangle]
pub extern "C" fn Py_IsFinalizing() -> c_int {
    0
}

/// `_Py_Dealloc`: releases a handle whose reference count already reached zero.
#[no_mangle]
pub extern "C" fn _Py_Dealloc(object: *mut ShimObject) {
    if object.is_null() {
        return;
    }
    ensure_builtin_types();
    // SAFETY: every handle starts with the CPython header.
    let counter = unsafe { &(*object).head.ob_refcnt };
    if counter.load(Ordering::Relaxed) == IMMORTAL_REFCNT {
        return;
    }
    counter.store(0, Ordering::Release);
    free_object(object);
}

/// `PyCMethod_New`.
#[no_mangle]
pub extern "C" fn PyCMethod_New(
    method: *mut PyMethodDefMirror,
    self_object: *mut ShimObject,
    module: *mut ShimObject,
    class: *mut PyTypeObject,
) -> *mut ShimObject {
    if method.is_null() {
        set_error("SystemError: PyCMethod_New: PyMethodDef is NULL");
        return ptr::null_mut();
    }
    // SAFETY: the extension owns the live method definition.
    let (name, flags) = unsafe { ((*method).ml_name, (*method).ml_flags) };
    let method_name = read_c_string(name);
    if flags & METH_METHOD != 0 {
        if class.is_null() {
            set_error(
                "SystemError: attempting to create PyCMethod with a METH_METHOD flag but \
                 no class",
            );
            return ptr::null_mut();
        }
    } else if !class.is_null() {
        set_error(
            "SystemError: attempting to create PyCFunction with class but no METH_METHOD \
             flag",
        );
        return ptr::null_mut();
    }
    let signature =
        flags & (METH_VARARGS | METH_FASTCALL | METH_NOARGS | METH_O | METH_KEYWORDS | METH_METHOD);
    let supported = signature == METH_VARARGS
        || signature == (METH_VARARGS | METH_KEYWORDS)
        || signature == METH_FASTCALL
        || signature == (METH_FASTCALL | METH_KEYWORDS)
        || signature == METH_NOARGS
        || signature == METH_O
        || signature == (METH_METHOD | METH_FASTCALL | METH_KEYWORDS);
    if !supported {
        set_error(format!(
            "SystemError: {method_name}() method: bad call flags"
        ));
        return ptr::null_mut();
    }
    PyCFunction_NewEx(method, self_object, module)
}

/// `PyObject_CallNoArgs`.
#[no_mangle]
pub extern "C" fn PyObject_CallNoArgs(callable: *mut ShimObject) -> *mut ShimObject {
    if callable.is_null() {
        set_error("SystemError: null argument to internal routine");
        return ptr::null_mut();
    }
    PyObject_Vectorcall(callable, ptr::null(), 0, ptr::null_mut())
}

/// `PyObject_CallOneArg`.
#[no_mangle]
pub extern "C" fn PyObject_CallOneArg(
    callable: *mut ShimObject,
    argument: *mut ShimObject,
) -> *mut ShimObject {
    if callable.is_null() || argument.is_null() {
        set_error("SystemError: null argument to internal routine");
        return ptr::null_mut();
    }
    let arguments = [argument];
    PyObject_Vectorcall(callable, arguments.as_ptr(), 1, ptr::null_mut())
}

/// `PyObject_GenericGetDict`.
#[no_mangle]
pub extern "C" fn PyObject_GenericGetDict(
    object: *mut ShimObject,
    _context: *mut c_void,
) -> *mut ShimObject {
    if object.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    // A Hydro instance/class exposes `__dict__` through Hydro; a value with
    // no attribute dict reports AttributeError like CPython's default.
    if hydro_handle(object).is_some() {
        return match attribute_lookup(object, "__dict__") {
            Ok(dict) => dict,
            Err(error) => {
                set_error(error);
                ptr::null_mut()
            }
        };
    }
    if matches!(kind_ref(object), Some(ShimKind::Module(_))) {
        let dict = PyModule_GetDict(object);
        if !dict.is_null() {
            inc_ref(dict);
        }
        return dict;
    }
    set_error("AttributeError: __dict__");
    ptr::null_mut()
}

/// `PyObject_GenericSetDict`.
#[no_mangle]
pub extern "C" fn PyObject_GenericSetDict(
    object: *mut ShimObject,
    value: *mut ShimObject,
    _context: *mut c_void,
) -> c_int {
    if value.is_null() {
        set_error("TypeError: cannot delete __dict__");
        return -1;
    }
    if hydro_handle(object).is_some() {
        return match hydro_attribute_set(object, "__dict__", value) {
            Ok(()) => 0,
            Err(error) => {
                set_error(error);
                -1
            }
        };
    }
    if !matches!(kind_ref(value), Some(ShimKind::Dict(_))) {
        set_error(format!(
            "TypeError: __dict__ must be set to a dictionary, not a '{}'",
            object_type_name(value)
        ));
        return -1;
    }
    if !matches!(kind_ref(object), Some(ShimKind::Module(_))) {
        set_error("AttributeError: __dict__");
        return -1;
    }
    PyDict_Clear(PyModule_GetDict(object));
    PyDict_Update(PyModule_GetDict(object), value)
}

/// `PyObject_HasAttrWithError`: 1 present, 0 absent, -1 error.
#[no_mangle]
pub extern "C" fn PyObject_HasAttrWithError(
    object: *mut ShimObject,
    name: *mut ShimObject,
) -> c_int {
    if object.is_null() {
        bad_internal_call();
        return -1;
    }
    let Some(name) = as_string(name) else {
        set_error("TypeError: attribute name must be string");
        return -1;
    };
    match attribute_lookup(object, &name) {
        Ok(value) => {
            dec_ref(value);
            1
        }
        Err(error) => {
            if error.starts_with("AttributeError") {
                0
            } else {
                set_error(error);
                -1
            }
        }
    }
}

/// `PyObject_IsInstance`.
#[no_mangle]
pub extern "C" fn PyObject_IsInstance(instance: *mut ShimObject, class: *mut ShimObject) -> c_int {
    if instance.is_null() || class.is_null() {
        bad_internal_call();
        return -1;
    }
    if let Some(ShimKind::Tuple(classes)) = kind_ref(class) {
        for &candidate in classes {
            match PyObject_IsInstance(instance, candidate as *mut ShimObject) {
                result if result != 0 => return result,
                _ if error_pending() => return -1,
                _ => {}
            }
        }
        return 0;
    }
    if !types::is_type_pointer(class) {
        // A Hydro class (or a native type value such as `int`) answers
        // through Hydro's `isinstance`, including virtual subclasses and
        // `__instancecheck__`.
        if let Some(class_value) = hydro_tagged(class) {
            return match hydro_isinstance_value(instance, class_value) {
                Ok(equal) => equal as c_int,
                Err(error) => {
                    set_error(error);
                    -1
                }
            };
        }
        set_error("TypeError: isinstance() arg 2 must be a type, a tuple of types, or a union");
        return -1;
    }
    let instance_type = type_of(instance);
    if instance_type.is_null() {
        return 0;
    }
    types::is_subtype(instance_type, class as *mut TypeObject) as c_int
}

/// `PyObject_IsSubclass`.
#[no_mangle]
pub extern "C" fn PyObject_IsSubclass(derived: *mut ShimObject, class: *mut ShimObject) -> c_int {
    if derived.is_null() || class.is_null() {
        bad_internal_call();
        return -1;
    }
    if let Some(ShimKind::Tuple(classes)) = kind_ref(class) {
        for &candidate in classes {
            match PyObject_IsSubclass(derived, candidate as *mut ShimObject) {
                result if result != 0 => return result,
                _ if error_pending() => return -1,
                _ => {}
            }
        }
        return 0;
    }
    if !types::is_type_pointer(derived) {
        // Hydro classes are not C type objects; answer from Hydro when the
        // other operand is a Hydro class, and from the wrapper otherwise.
        if let Some(derived_class) = hydro_tagged(derived) {
            if let Some(class_value) = hydro_tagged(class) {
                return match hydro_issubclass(derived_class, class_value) {
                    Ok(equal) => equal as c_int,
                    Err(error) => {
                        set_error(error);
                        -1
                    }
                };
            }
            if types::is_type_pointer(class)
                && types::type_name(class as *mut TypeObject) == "object"
            {
                return 1;
            }
            if !types::is_type_pointer(class) {
                set_error(
                    "TypeError: issubclass() arg 2 must be a class, a tuple of classes, or a union",
                );
                return -1;
            }
            return 0;
        }
        set_error("TypeError: issubclass() arg 1 must be a class");
        return -1;
    }
    if !types::is_type_pointer(class) {
        // A C type cannot derive from a Hydro class.
        if hydro_class_value(class).is_some() {
            return 0;
        }
        set_error("TypeError: issubclass() arg 2 must be a class, a tuple of classes, or a union");
        return -1;
    }
    types::is_subtype(derived as *mut TypeObject, class as *mut TypeObject) as c_int
}

/// `PyObject_LengthHint`: `len(obj)`, then `__length_hint__`, then `default`.
#[no_mangle]
pub extern "C" fn PyObject_LengthHint(object: *mut ShimObject, default_value: isize) -> isize {
    if object.is_null() {
        return default_value;
    }
    let length = types::object_length(object);
    if !error_pending() {
        return length;
    }
    let error = take_error().unwrap_or_default();
    if !error.starts_with("TypeError") {
        set_error(error);
        return -1;
    }
    let hint = match attribute_lookup(object, "__length_hint__") {
        Ok(hint) => hint,
        Err(_) => return default_value,
    };
    let result = PyObject_CallNoArgs(hint);
    dec_ref(hint);
    if result.is_null() {
        if error_pending() {
            let error = take_error().unwrap_or_default();
            if error.starts_with("TypeError") {
                return default_value;
            }
            set_error(error);
        }
        return -1;
    }
    if result == ptr::addr_of_mut!(_Py_NotImplementedStruct) {
        dec_ref(result);
        return default_value;
    }
    let value = match as_int(result) {
        Ok(value) => value,
        Err(_) => {
            set_error(format!(
                "TypeError: __length_hint__ must be an integer, not '{}'",
                object_type_name(result)
            ));
            dec_ref(result);
            return -1;
        }
    };
    dec_ref(result);
    if value < 0 {
        set_error("ValueError: __length_hint__() should return >= 0");
        return -1;
    }
    isize::try_from(value).unwrap_or(isize::MAX)
}

/// `PyObject_Dir`.
#[no_mangle]
pub extern "C" fn PyObject_Dir(object: *mut ShimObject) -> *mut ShimObject {
    if object.is_null() {
        // There is no executing frame to take locals from.
        return new_object(ShimKind::List(Vec::new()));
    }
    let mut names: Vec<String> = Vec::new();
    if let Some(ShimKind::Module(_)) = kind_ref(object) {
        let dict = PyModule_GetDict(object);
        if !dict.is_null() {
            if let Ok(pairs) = dict_slots(dict) {
                for &(key, _) in pairs {
                    if let Some(name) = as_string(key as *mut ShimObject) {
                        names.push(name);
                    }
                }
            }
        }
        names.push("__dict__".to_string());
    } else if types::is_type_pointer(object) {
        for name in [
            "__name__",
            "__qualname__",
            "__module__",
            "__doc__",
            "__flags__",
            "__basicsize__",
            "__itemsize__",
            "__dictoffset__",
            "__weakrefoffset__",
            "__vectorcalloffset__",
            "__base__",
            "__bases__",
            "__mro__",
        ] {
            names.push(name.to_string());
        }
        let type_object = object as *mut TypeObject;
        // SAFETY: validated type pointer.
        let methods = unsafe { (*type_object).tp_methods };
        if !methods.is_null() {
            if let Ok(table) = read_method_table(methods, "type") {
                names.extend(table.into_iter().map(|spec| spec.name));
            }
        }
    }
    names.sort();
    names.dedup();
    let mut items = Vec::with_capacity(names.len());
    for name in names {
        items.push(new_str(name) as usize);
    }
    new_object(ShimKind::List(items))
}

/// `PyTraceBack_Print`.
///
/// This host never builds traceback frames, so only `NULL` and `None` are
/// meaningful inputs; a real traceback object is rejected instead of being
/// silently ignored.
#[no_mangle]
pub extern "C" fn PyTraceBack_Print(traceback: *mut ShimObject, _file: *mut ShimObject) -> c_int {
    if traceback.is_null() || traceback == ptr::addr_of_mut!(_Py_NoneStruct) {
        return 0;
    }
    set_error("TypeError: traceback object is required");
    -1
}

/// The CPython `datetime` C API capsule exposed as `datetime.datetime_CAPI`.
///
/// The structure layout is CPython 3.14's `PyDateTime_CAPI`. The five type
/// slots are real type objects (`PyDateTimeAPI->DateType`, ...); the handles
/// returned by the constructor callbacks use those same objects as `ob_type`,
/// so the header macros `PyDate_Check`/`PyDateTime_Check`/`PyTime_Check`/
/// `PyDelta_Check`/`PyTZInfo_Check` and `PyType_IsSubtype` answer correctly.
///
/// Every callback is backed by the native `datetime` module constructors:
///
/// | capsule slot | native constructor |
/// |---|---|
/// | `Date_FromDate` | `datetime.date(...)` |
/// | `DateTime_FromDateAndTime` | `datetime.datetime(...)` |
/// | `Time_FromTime` | `datetime.time(...)` |
/// | `Delta_FromDelta` | `datetime.timedelta(...)` |
/// | `DateTime_FromDateAndTimeAndFold` | `datetime.datetime(..., fold=...)` |
/// | `Time_FromTimeAndFold` | `datetime.time(..., fold=...)` |
/// | `DateTime_FromTimestamp` | `datetime.datetime.fromtimestamp(...)` |
/// | `Date_FromTimestamp` | `datetime.datetime.fromtimestamp(...).date()` |
/// | `TimeZone_FromTimeZone` | `datetime.timezone(offset, name)` |
///
/// `TimeZone_UTC` is the native `datetime.timezone.utc` singleton, held
/// immortal by the capsule (a `tzinfo` subclass, so `PyTZInfo_Check` holds).
/// No slot in the capsule is NULL.
#[repr(C)]
struct PyDateTimeCAPI {
    date_type: *mut types::TypeObject,
    datetime_type: *mut types::TypeObject,
    time_type: *mut types::TypeObject,
    delta_type: *mut types::TypeObject,
    tzinfo_type: *mut types::TypeObject,
    timezone_utc: *mut ShimObject,
    date_from_date: *mut c_void,
    datetime_from_date_and_time: *mut c_void,
    time_from_time: *mut c_void,
    delta_from_delta: *mut c_void,
    timezone_from_timezone: *mut c_void,
    datetime_from_timestamp: *mut c_void,
    date_from_timestamp: *mut c_void,
    datetime_from_date_and_time_and_fold: *mut c_void,
    time_from_time_and_fold: *mut c_void,
}

/// Encodes a C integer argument as the tagged Hydro value the native
/// datetime constructors expect.
fn datetime_capi_int(value: c_int) -> i64 {
    crate::value::encode_int(i64::from(value)).unwrap_or(crate::value::NONE)
}

/// Converts one C argument into a Hydro value, preserving the handle identity
/// of Hydro objects (notably a tzinfo instance).
fn datetime_capi_argument(argument: *mut ShimObject) -> Result<i64, String> {
    if argument.is_null() {
        return Ok(crate::value::NONE);
    }
    let shim = object_to_value(argument)?;
    crate::modules::extension::shim_to_value(shim)
}

/// Decodes the optional `tzinfo` argument shared by the date/time
/// constructors. `NULL` means "no tzinfo", as CPython's macros pass NULL for
/// a naive value.
fn datetime_capi_tzinfo(tzinfo: *mut ShimObject) -> Result<i64, String> {
    if tzinfo.is_null() || tzinfo == ptr::addr_of_mut!(_Py_NoneStruct) {
        return Ok(crate::value::NONE);
    }
    if let Some(handle) = hydro_handle(tzinfo) {
        return Ok(handle.value());
    }
    Err("TypeError: tzinfo argument must be None or a datetime.tzinfo instance".to_string())
}

/// Validates the `cls` argument the capsule callbacks receive.
///
/// The macros pass the capsule's own type object. A subclass is not
/// representable: the native `date`/`datetime` classes have no C-level
/// subclass layout, so fabricating an instance of one would hand the extension
/// memory that disagrees with its type. It is rejected with a diagnostic.
fn datetime_capi_check_class(
    class: *mut ShimObject,
    expected: *mut types::TypeObject,
    label: &str,
) -> Result<(), String> {
    if class.is_null() || class as *mut types::TypeObject == expected {
        return Ok(());
    }
    if types::is_type_pointer(class) {
        return Err(format!(
            "TypeError: {label}: subclassing is not supported by this runtime (got '{}')",
            types::type_name(class as *mut types::TypeObject)
        ));
    }
    Err(format!("TypeError: {label}: class argument is not a type"))
}

/// Wraps a successfully constructed native datetime value.
fn datetime_capi_wrap(result: i64) -> *mut ShimObject {
    if result == crate::value::NONE || crate::exc::pending() {
        if crate::exc::pending() {
            let message = take_hydro_error("datetime constructor failed");
            set_error(message);
        } else {
            set_error("SystemError: datetime constructor returned None");
        }
        return ptr::null_mut();
    }
    value_object(ShimValue::Hydro(HydroValue::new(result)))
}

/// Runs one of the native datetime constructors and converts the result.
fn datetime_capi_construct(
    class_name: &str,
    positional: &[i64],
    keywords: &[(String, i64)],
) -> *mut ShimObject {
    let class = crate::modules::datetime::constant(class_name);
    let result = if keywords.is_empty() {
        crate::modules::datetime::instantiate(class, positional)
    } else {
        crate::modules::datetime::instantiate_kw(class, positional, keywords)
    };
    let Some(result) = result else {
        set_error(format!(
            "SystemError: native datetime.{class_name} constructor is unavailable"
        ));
        return ptr::null_mut();
    };
    datetime_capi_wrap(result)
}

/// `PyDate_FromDate` (capsule `Date_FromDate`).
unsafe extern "C" fn date_from_date(
    year: c_int,
    month: c_int,
    day: c_int,
    class: *mut ShimObject,
) -> *mut ShimObject {
    if let Err(error) = datetime_capi_check_class(
        class,
        ptr::addr_of_mut!(types::DATE_CAPI_TYPE),
        "PyDate_FromDate",
    ) {
        set_error(error);
        return ptr::null_mut();
    }
    let args = [
        datetime_capi_int(year),
        datetime_capi_int(month),
        datetime_capi_int(day),
    ];
    datetime_capi_construct("date", &args, &[])
}

/// `PyDateTime_FromDateAndTime` (capsule `DateTime_FromDateAndTime`).
unsafe extern "C" fn datetime_from_date_and_time(
    year: c_int,
    month: c_int,
    day: c_int,
    hour: c_int,
    minute: c_int,
    second: c_int,
    microsecond: c_int,
    tzinfo: *mut ShimObject,
    class: *mut ShimObject,
) -> *mut ShimObject {
    if let Err(error) = datetime_capi_check_class(
        class,
        ptr::addr_of_mut!(types::DATETIME_CAPI_TYPE),
        "PyDateTime_FromDateAndTime",
    ) {
        set_error(error);
        return ptr::null_mut();
    }
    let tzinfo = match datetime_capi_tzinfo(tzinfo) {
        Ok(tzinfo) => tzinfo,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let args = [
        datetime_capi_int(year),
        datetime_capi_int(month),
        datetime_capi_int(day),
        datetime_capi_int(hour),
        datetime_capi_int(minute),
        datetime_capi_int(second),
        datetime_capi_int(microsecond),
        tzinfo,
    ];
    datetime_capi_construct("datetime", &args, &[])
}

/// `PyDateTime_FromDateAndTimeAndFold` (capsule
/// `DateTime_FromDateAndTimeAndFold`).
unsafe extern "C" fn datetime_from_date_and_time_and_fold(
    year: c_int,
    month: c_int,
    day: c_int,
    hour: c_int,
    minute: c_int,
    second: c_int,
    microsecond: c_int,
    tzinfo: *mut ShimObject,
    fold: c_int,
    class: *mut ShimObject,
) -> *mut ShimObject {
    if let Err(error) = datetime_capi_check_class(
        class,
        ptr::addr_of_mut!(types::DATETIME_CAPI_TYPE),
        "PyDateTime_FromDateAndTimeAndFold",
    ) {
        set_error(error);
        return ptr::null_mut();
    }
    let tzinfo = match datetime_capi_tzinfo(tzinfo) {
        Ok(tzinfo) => tzinfo,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let args = [
        datetime_capi_int(year),
        datetime_capi_int(month),
        datetime_capi_int(day),
        datetime_capi_int(hour),
        datetime_capi_int(minute),
        datetime_capi_int(second),
        datetime_capi_int(microsecond),
        tzinfo,
    ];
    let keywords = [("fold".to_string(), datetime_capi_int(fold))];
    datetime_capi_construct("datetime", &args, &keywords)
}

/// `PyTime_FromTime` (capsule `Time_FromTime`).
unsafe extern "C" fn time_from_time(
    hour: c_int,
    minute: c_int,
    second: c_int,
    microsecond: c_int,
    tzinfo: *mut ShimObject,
    class: *mut ShimObject,
) -> *mut ShimObject {
    if let Err(error) = datetime_capi_check_class(
        class,
        ptr::addr_of_mut!(types::TIME_CAPI_TYPE),
        "PyTime_FromTime",
    ) {
        set_error(error);
        return ptr::null_mut();
    }
    let tzinfo = match datetime_capi_tzinfo(tzinfo) {
        Ok(tzinfo) => tzinfo,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let args = [
        datetime_capi_int(hour),
        datetime_capi_int(minute),
        datetime_capi_int(second),
        datetime_capi_int(microsecond),
        tzinfo,
    ];
    datetime_capi_construct("time", &args, &[])
}

/// `PyTime_FromTimeAndFold` (capsule `Time_FromTimeAndFold`).
unsafe extern "C" fn time_from_time_and_fold(
    hour: c_int,
    minute: c_int,
    second: c_int,
    microsecond: c_int,
    tzinfo: *mut ShimObject,
    fold: c_int,
    class: *mut ShimObject,
) -> *mut ShimObject {
    if let Err(error) = datetime_capi_check_class(
        class,
        ptr::addr_of_mut!(types::TIME_CAPI_TYPE),
        "PyTime_FromTimeAndFold",
    ) {
        set_error(error);
        return ptr::null_mut();
    }
    let tzinfo = match datetime_capi_tzinfo(tzinfo) {
        Ok(tzinfo) => tzinfo,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let args = [
        datetime_capi_int(hour),
        datetime_capi_int(minute),
        datetime_capi_int(second),
        datetime_capi_int(microsecond),
        tzinfo,
    ];
    let keywords = [("fold".to_string(), datetime_capi_int(fold))];
    datetime_capi_construct("time", &args, &keywords)
}

/// `PyDelta_FromDSU` (capsule `Delta_FromDelta`).
///
/// The native `timedelta` constructor always normalises its components; the
/// capsule's `normalize` flag therefore cannot produce CPython's internal
/// non-normalised representation, and the normalised value is returned in both
/// cases.
unsafe extern "C" fn delta_from_delta(
    days: c_int,
    seconds: c_int,
    microseconds: c_int,
    _normalize: c_int,
    class: *mut ShimObject,
) -> *mut ShimObject {
    if let Err(error) = datetime_capi_check_class(
        class,
        ptr::addr_of_mut!(types::TIMEDELTA_CAPI_TYPE),
        "PyDelta_FromDSU",
    ) {
        set_error(error);
        return ptr::null_mut();
    }
    let args = [
        datetime_capi_int(days),
        datetime_capi_int(seconds),
        datetime_capi_int(microseconds),
    ];
    datetime_capi_construct("timedelta", &args, &[])
}

/// Decoded `(positional, keyword)` arguments of a `DateTime_FromTimestamp`
/// tuple/dict pair. Named so the decoder's return type stays readable (and
/// clippy::type_complexity-quiet) without changing the calling convention.
type TimestampArguments = (Vec<i64>, Vec<(String, i64)>);

/// Decodes a `DateTime_FromTimestamp` argument tuple/dict pair.
fn datetime_capi_timestamp_arguments(
    args: *mut ShimObject,
    keywords: *mut ShimObject,
) -> Result<TimestampArguments, String> {
    let mut positional: Vec<i64> = Vec::new();
    if !args.is_null() {
        match kind_ref(args) {
            Some(ShimKind::Tuple(items)) => {
                for &item in items {
                    positional.push(datetime_capi_argument(item as *mut ShimObject)?);
                }
            }
            _ => return Err("TypeError: timestamp arguments must be a tuple".to_string()),
        }
    }
    let mut named: Vec<(String, i64)> = Vec::new();
    if !keywords.is_null() {
        for &(key, value) in dict_slots(keywords)? {
            let Some(name) = as_string(key as *mut ShimObject) else {
                return Err("TypeError: keyword names must be strings".to_string());
            };
            named.push((name, datetime_capi_argument(value as *mut ShimObject)?));
        }
    }
    Ok((positional, named))
}

/// Shared body of `DateTime_FromTimestamp`/`Date_FromTimestamp`.
fn datetime_capi_from_timestamp(
    class: *mut ShimObject,
    args: *mut ShimObject,
    keywords: *mut ShimObject,
    date_only: bool,
) -> *mut ShimObject {
    let label = if date_only {
        "PyDate_FromTimestamp"
    } else {
        "PyDateTime_FromTimestamp"
    };
    let expected = if date_only {
        ptr::addr_of_mut!(types::DATE_CAPI_TYPE)
    } else {
        ptr::addr_of_mut!(types::DATETIME_CAPI_TYPE)
    };
    if let Err(error) = datetime_capi_check_class(class, expected, label) {
        set_error(error);
        return ptr::null_mut();
    }
    let (mut positional, named) = match datetime_capi_timestamp_arguments(args, keywords) {
        Ok(arguments) => arguments,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    for (name, value) in named {
        match name.as_str() {
            // CPython's `fromtimestamp(timestamp, tz=None)`.
            "tz" if positional.len() == 1 => positional.push(value),
            other => {
                set_error(format!(
                    "TypeError: fromtimestamp() got an unexpected keyword argument '{other}'"
                ));
                return ptr::null_mut();
            }
        }
    }
    if positional.is_empty() {
        set_error("TypeError: fromtimestamp() missing required argument 'timestamp'");
        return ptr::null_mut();
    }
    let moment = crate::modules::datetime::call("_datetime_fromtimestamp", &positional);
    if crate::exc::pending() {
        let message = take_hydro_error("fromtimestamp() raised an exception");
        set_error(message);
        return ptr::null_mut();
    }
    if !date_only {
        return datetime_capi_wrap(moment);
    }
    let date = crate::modules::datetime::call("_datetime_date", &[moment]);
    datetime_capi_wrap(date)
}

/// `PyDateTime_FromTimestamp` (capsule `DateTime_FromTimestamp`).
unsafe extern "C" fn datetime_from_timestamp(
    class: *mut ShimObject,
    args: *mut ShimObject,
    keywords: *mut ShimObject,
) -> *mut ShimObject {
    datetime_capi_from_timestamp(class, args, keywords, false)
}

/// `PyDate_FromTimestamp` (capsule `Date_FromTimestamp`).
unsafe extern "C" fn date_from_timestamp(
    class: *mut ShimObject,
    args: *mut ShimObject,
) -> *mut ShimObject {
    datetime_capi_from_timestamp(class, args, ptr::null_mut(), true)
}

/// `PyTimeZone_FromOffset`/`PyTimeZone_FromTimeZone` (capsule
/// `TimeZone_FromTimeZone`).
///
/// `offset` is a native `datetime.timedelta` handle; `name` is a `str` or
/// `NULL` (auto-generated from the offset), and `NULL` offset means the zero
/// offset, matching `PyTimeZone_FromOffset(NULL)`'s documented UTC spelling.
unsafe extern "C" fn timezone_from_timezone(
    offset: *mut ShimObject,
    name: *mut ShimObject,
) -> *mut ShimObject {
    let offset_value = if offset.is_null() {
        let delta_class = crate::modules::datetime::constant("timedelta");
        match crate::modules::datetime::instantiate(delta_class, &[datetime_capi_int(0)]) {
            Some(value) if value != crate::value::NONE && !crate::exc::pending() => value,
            _ => {
                if crate::exc::pending() {
                    let message = take_hydro_error("timedelta(0) failed");
                    set_error(message);
                } else {
                    set_error("SystemError: native datetime.timedelta constructor is unavailable");
                }
                return ptr::null_mut();
            }
        }
    } else {
        match datetime_capi_argument(offset) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        }
    };
    let class = crate::modules::datetime::constant("timezone");
    let result = if name.is_null() {
        crate::modules::datetime::instantiate(class, &[offset_value])
    } else {
        let name_value = match datetime_capi_argument(name) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        };
        crate::modules::datetime::instantiate_kw(
            class,
            &[offset_value],
            &[("name".to_string(), name_value)],
        )
    };
    let Some(result) = result else {
        set_error("SystemError: native datetime.timezone constructor is unavailable");
        return ptr::null_mut();
    };
    datetime_capi_wrap(result)
}

/// `PyDateTime_TimeZone_UTC`: the native `datetime.timezone.utc` singleton.
///
/// The capsule holds it for the process lifetime and marks it immortal, so an
/// extension may `Py_DecRef` the borrowed pointer it receives. The wrapped type
/// is `datetime.timezone`, a `tzinfo` subclass, so `PyTZInfo_Check` succeeds.
fn datetime_capsule_utc() -> *mut ShimObject {
    static UTC: LazyLock<usize> = LazyLock::new(|| {
        let class = crate::modules::datetime::constant("timezone");
        let utc = crate::methods::getattr(class, b"utc".as_ptr(), 3);
        let lookup_failed = crate::exc::pending();
        if lookup_failed {
            crate::exc::clear();
        }
        let value = if lookup_failed || utc == crate::value::NONE {
            // `timezone_class()` always publishes `utc`; fall back to a
            // zero-offset zone rather than a NULL capsule slot.
            match crate::modules::datetime::instantiate(
                class,
                &[
                    match crate::modules::datetime::instantiate(
                        crate::modules::datetime::constant("timedelta"),
                        &[datetime_capi_int(0)],
                    ) {
                        Some(delta) => delta,
                        None => crate::value::NONE,
                    },
                ],
            ) {
                Some(value) => value,
                None => crate::value::NONE,
            }
        } else {
            utc
        };
        if value == crate::value::NONE || crate::exc::pending() {
            if crate::exc::pending() {
                crate::exc::clear();
            }
            return 0;
        }
        // The capsule holds the only (process-lifetime) reference and
        // extensions may decref what they receive, so it is immortal.
        make_immortal(value_object(ShimValue::Hydro(HydroValue::new(value)))) as usize
    });
    *UTC as *mut ShimObject
}

/// Returns the `datetime_CAPI` capsule value (a new reference).
pub(crate) fn datetime_capsule() -> i64 {
    static CAPI: LazyLock<usize> = LazyLock::new(|| {
        ensure_builtin_types();
        let capi = Box::new(PyDateTimeCAPI {
            date_type: ptr::addr_of_mut!(types::DATE_CAPI_TYPE),
            datetime_type: ptr::addr_of_mut!(types::DATETIME_CAPI_TYPE),
            time_type: ptr::addr_of_mut!(types::TIME_CAPI_TYPE),
            delta_type: ptr::addr_of_mut!(types::TIMEDELTA_CAPI_TYPE),
            tzinfo_type: ptr::addr_of_mut!(types::TZINFO_CAPI_TYPE),
            timezone_utc: datetime_capsule_utc(),
            date_from_date: date_from_date as *const () as *mut c_void,
            datetime_from_date_and_time: datetime_from_date_and_time as *const () as *mut c_void,
            time_from_time: time_from_time as *const () as *mut c_void,
            delta_from_delta: delta_from_delta as *const () as *mut c_void,
            timezone_from_timezone: timezone_from_timezone as *const () as *mut c_void,
            datetime_from_timestamp: datetime_from_timestamp as *const () as *mut c_void,
            date_from_timestamp: date_from_timestamp as *const () as *mut c_void,
            datetime_from_date_and_time_and_fold: datetime_from_date_and_time_and_fold as *const ()
                as *mut c_void,
            time_from_time_and_fold: time_from_time_and_fold as *const () as *mut c_void,
        });
        Box::into_raw(capi) as usize
    });
    crate::value::capsule_new(*CAPI, Some("datetime.datetime_CAPI".to_string()))
}

/// `PyCapsule_Import`: resolves a dotted `module.attr...` name, verifies the
/// capsule's own name and returns its pointer.
#[no_mangle]
pub extern "C" fn PyCapsule_Import(name: *const c_char, _no_block: c_int) -> *mut c_void {
    if name.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    let full_name = read_c_string(name);
    let segments: Vec<&str> = full_name.split('.').collect();
    if segments.len() < 2 || segments.iter().any(|segment| segment.is_empty()) {
        set_error(format!(
            "AttributeError: PyCapsule_Import \"{full_name}\" is not valid"
        ));
        return ptr::null_mut();
    }
    let module_name = match CString::new(segments[0]) {
        Ok(module_name) => module_name,
        Err(_) => {
            bad_internal_call();
            return ptr::null_mut();
        }
    };
    let mut object = PyImport_ImportModule(module_name.as_ptr());
    if object.is_null() {
        let _ = take_error();
        set_error(format!(
            "ImportError: PyCapsule_Import could not import module \"{}\"",
            segments[0]
        ));
        return ptr::null_mut();
    }
    for segment in &segments[1..] {
        let attribute = match CString::new(*segment) {
            Ok(attribute) => attribute,
            Err(_) => {
                dec_ref(object);
                bad_internal_call();
                return ptr::null_mut();
            }
        };
        let next = PyObject_GetAttrString(object, attribute.as_ptr());
        dec_ref(object);
        if next.is_null() {
            // CPython keeps the attribute error from the lookup.
            return ptr::null_mut();
        }
        object = next;
    }
    let capsule_name = match CString::new(full_name.as_str()) {
        Ok(capsule_name) => capsule_name,
        Err(_) => {
            dec_ref(object);
            bad_internal_call();
            return ptr::null_mut();
        }
    };
    let pointer = PyCapsule_GetPointer(object, capsule_name.as_ptr());
    dec_ref(object);
    if pointer.is_null() {
        let _ = take_error();
        set_error(format!(
            "AttributeError: PyCapsule_Import \"{full_name}\" is not valid"
        ));
    }
    pointer
}

// ---------------------------------------------------------------------------
// Remaining CPython 3.14 entry points
// ---------------------------------------------------------------------------

/// `Py_complex`: the C representation of a Python complex number.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Py_complex {
    pub real: f64,
    pub imag: f64,
}

/// `PyBool_FromLong`: returns one of the (immortal) bool singletons.
#[no_mangle]
pub extern "C" fn PyBool_FromLong(value: c_long) -> *mut ShimObject {
    if value == 0 {
        ptr::addr_of_mut!(_Py_FalseStruct)
    } else {
        ptr::addr_of_mut!(_Py_TrueStruct)
    }
}

/// `PyBytes_FromString`.
#[no_mangle]
pub extern "C" fn PyBytes_FromString(value: *const c_char) -> *mut ShimObject {
    if value.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    new_bytes(read_c_string(value).into_bytes())
}

/// `PyBytes_AsStringAndSize`: the buffer and its length.
///
/// With a NULL `length` the string must not contain an embedded NUL, exactly
/// like CPython.
#[no_mangle]
pub extern "C" fn PyBytes_AsStringAndSize(
    object: *mut ShimObject,
    string: *mut *mut c_char,
    length: *mut isize,
) -> c_int {
    if string.is_null() {
        bad_internal_call();
        return -1;
    }
    let data = match bytes_data(object) {
        Ok(data) => data,
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    if length.is_null() && data.contains(&0) {
        set_error("ValueError: embedded null byte");
        return -1;
    }
    let pointer = match bytes_c_data(object) {
        Ok(data) => data.as_ptr(),
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    // SAFETY: the caller supplied a writable `char *` slot for this API.
    unsafe {
        *string = pointer as *mut c_char;
        if !length.is_null() {
            *length = py_ssize_of(data.len());
        }
    }
    0
}

/// `PyComplex_AsCComplex`.
#[no_mangle]
pub extern "C" fn PyComplex_AsCComplex(object: *mut ShimObject) -> Py_complex {
    match kind_ref(object) {
        Some(ShimKind::Complex { real, imag }) => Py_complex {
            real: *real,
            imag: *imag,
        },
        Some(ShimKind::Float(number)) => Py_complex {
            real: *number,
            imag: 0.0,
        },
        Some(ShimKind::Int(number)) => Py_complex {
            real: *number as f64,
            imag: 0.0,
        },
        Some(ShimKind::Bool(flag)) => Py_complex {
            real: if *flag { 1.0 } else { 0.0 },
            imag: 0.0,
        },
        _ => {
            set_error(format!(
                "TypeError: complex() argument must be a string or a number, not '{}'",
                object_type_name(object)
            ));
            Py_complex {
                real: -1.0,
                imag: 0.0,
            }
        }
    }
}

/// `PyComplex_FromCComplex`.
#[no_mangle]
pub extern "C" fn PyComplex_FromCComplex(value: Py_complex) -> *mut ShimObject {
    new_object(ShimKind::Complex {
        real: value.real,
        imag: value.imag,
    })
}

/// `PyCapsule_GetContext`.
#[no_mangle]
pub extern "C" fn PyCapsule_GetContext(capsule: *mut ShimObject) -> *mut c_void {
    match capsule_fields(capsule) {
        Ok((_, _, context, _)) => context as *mut c_void,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyCapsule_SetContext`.
#[no_mangle]
pub extern "C" fn PyCapsule_SetContext(capsule: *mut ShimObject, context: *mut c_void) -> c_int {
    if capsule.is_null() {
        set_error("TypeError: a capsule object is required, got NULL");
        return -1;
    }
    // SAFETY: shim objects are heap-stable and the GIL serialises writes.
    match unsafe { kind_mut(capsule) } {
        ShimKind::Capsule { context: slot, .. } => {
            *slot = context as usize;
            0
        }
        _ => {
            set_error("TypeError: a capsule object is required");
            -1
        }
    }
}

/// `PyCapsule_SetName`.
#[no_mangle]
pub extern "C" fn PyCapsule_SetName(capsule: *mut ShimObject, name: *const c_char) -> c_int {
    if name.is_null() {
        set_error("ValueError: PyCapsule_SetName called with NULL name");
        return -1;
    }
    if capsule.is_null() {
        set_error("TypeError: a capsule object is required, got NULL");
        return -1;
    }
    let Some(name) = CString::new(read_c_string(name)).ok() else {
        set_error("ValueError: capsule name contains a NUL byte");
        return -1;
    };
    // SAFETY: see `PyCapsule_SetContext`.
    match unsafe { kind_mut(capsule) } {
        ShimKind::Capsule { name: slot, .. } => {
            *slot = Some(name);
            0
        }
        _ => {
            set_error("TypeError: a capsule object is required");
            -1
        }
    }
}

/// `PyMethod_New`: a bound method, or `func` itself when `self` is NULL.
#[no_mangle]
pub extern "C" fn PyMethod_New(
    function: *mut ShimObject,
    self_object: *mut ShimObject,
) -> *mut ShimObject {
    if function.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    if self_object.is_null() {
        inc_ref(function);
        return function;
    }
    inc_ref(function);
    inc_ref(self_object);
    new_object(ShimKind::Method {
        function: function as usize,
        self_object: self_object as usize,
    })
}

/// `PySlice_New`: each NULL component becomes `None`.
#[no_mangle]
pub extern "C" fn PySlice_New(
    start: *mut ShimObject,
    stop: *mut ShimObject,
    step: *mut ShimObject,
) -> *mut ShimObject {
    let component = |value: *mut ShimObject| {
        if value.is_null() {
            ptr::addr_of_mut!(_Py_NoneStruct) as usize
        } else {
            inc_ref(value);
            value as usize
        }
    };
    new_object(ShimKind::Slice {
        start: component(start),
        stop: component(stop),
        step: component(step),
    })
}

/// Converts one slice component to a C `Py_ssize_t`, clamping like CPython.
fn slice_component(object: *mut ShimObject) -> Result<Option<isize>, String> {
    if object.is_null() || object == ptr::addr_of_mut!(_Py_NoneStruct) {
        return Ok(None);
    }
    let index = types::PyNumber_Index(object);
    if index.is_null() {
        if !error_pending() {
            set_error(
                "TypeError: slice indices must be integers or None or have an __index__ method",
            );
        }
        return Err(take_error().unwrap_or_else(|| {
            "TypeError: slice indices must be integers or None or have an __index__ method"
                .to_string()
        }));
    }
    let converted = as_int(index);
    dec_ref(index);
    match converted {
        Ok(value) => Ok(Some(
            value.clamp(isize::MIN as i64, isize::MAX as i64) as isize
        )),
        Err(_) => Err(
            "TypeError: slice indices must be integers or None or have an __index__ method"
                .to_string(),
        ),
    }
}

/// `PySlice_Unpack`: the three adjusted components of a slice.
#[no_mangle]
pub extern "C" fn PySlice_Unpack(
    slice: *mut ShimObject,
    start: *mut isize,
    stop: *mut isize,
    step: *mut isize,
) -> c_int {
    if start.is_null() || stop.is_null() || step.is_null() {
        bad_internal_call();
        return -1;
    }
    let Some(ShimKind::Slice {
        start: start_slot,
        stop: stop_slot,
        step: step_slot,
    }) = kind_ref(slice)
    else {
        set_error("TypeError: PySlice_Unpack: a slice object is required");
        return -1;
    };
    let start_value = match slice_component(*start_slot as *mut ShimObject) {
        Ok(value) => value.unwrap_or(0),
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    let stop_value = match slice_component(*stop_slot as *mut ShimObject) {
        Ok(value) => value.unwrap_or(isize::MAX),
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    let step_value = match slice_component(*step_slot as *mut ShimObject) {
        Ok(value) => value.unwrap_or(1),
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    if step_value == 0 {
        set_error("ValueError: slice step cannot be zero");
        return -1;
    }
    // SAFETY: all three output slots were validated as writable above.
    unsafe {
        *start = start_value;
        *stop = stop_value;
        *step = step_value;
    }
    0
}

/// `PySlice_AdjustIndices`: clamps indices to `length` and returns the number
/// of elements the slice selects. The arithmetic is CPython's.
#[no_mangle]
pub extern "C" fn PySlice_AdjustIndices(
    length: isize,
    start: *mut isize,
    stop: *mut isize,
    step: isize,
) -> isize {
    if start.is_null() || stop.is_null() {
        set_error("SystemError: PySlice_AdjustIndices: NULL index pointer");
        return 0;
    }
    if length < 0 {
        set_error("ValueError: length should not be negative");
        return 0;
    }
    if step == 0 {
        set_error("ValueError: slice step cannot be zero");
        return 0;
    }
    // SAFETY: both pointers were validated above.
    unsafe {
        if *start < 0 {
            *start += length;
            if *start < 0 {
                *start = if step < 0 { -1 } else { 0 };
            }
        } else if *start >= length {
            *start = if step < 0 { length - 1 } else { length };
        }
        if *stop < 0 {
            *stop += length;
            if *stop < 0 {
                *stop = if step < 0 { -1 } else { 0 };
            }
        } else if *stop >= length {
            *stop = if step < 0 { length - 1 } else { length };
        }
        if step < 0 {
            if *stop < *start {
                (*start - *stop - 1) / (-step) + 1
            } else {
                0
            }
        } else if *start < *stop {
            (*stop - *start - 1) / step + 1
        } else {
            0
        }
    }
}

// -- dicts ------------------------------------------------------------------

/// `PyDict_Size`.
#[no_mangle]
pub extern "C" fn PyDict_Size(object: *mut ShimObject) -> isize {
    match dict_slots(object) {
        Ok(pairs) => py_ssize_of(pairs.len()),
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

/// `PyDict_Values`: a new list of the values.
#[no_mangle]
pub extern "C" fn PyDict_Values(object: *mut ShimObject) -> *mut ShimObject {
    let pairs = match dict_slots(object) {
        Ok(pairs) => pairs,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    for &(_, value) in pairs {
        inc_ref(value as *mut ShimObject);
    }
    new_object(ShimKind::List(
        pairs.iter().map(|&(_, value)| value).collect(),
    ))
}

/// `PyDict_ContainsString`.
#[no_mangle]
pub extern "C" fn PyDict_ContainsString(object: *mut ShimObject, key: *const c_char) -> c_int {
    if key.is_null() {
        bad_internal_call();
        return -1;
    }
    let key = new_str(read_c_string(key));
    let status = PyDict_Contains(object, key);
    dec_ref(key);
    status
}

/// `PyDict_DelItemString`.
#[no_mangle]
pub extern "C" fn PyDict_DelItemString(object: *mut ShimObject, key: *const c_char) -> c_int {
    if key.is_null() {
        bad_internal_call();
        return -1;
    }
    let key = new_str(read_c_string(key));
    let status = PyDict_DelItem(object, key);
    dec_ref(key);
    status
}

/// `PyDict_GetItemStringRef`: 1/0/-1 with a new reference in `*result`.
#[no_mangle]
pub extern "C" fn PyDict_GetItemStringRef(
    object: *mut ShimObject,
    key: *const c_char,
    result: *mut *mut ShimObject,
) -> c_int {
    if key.is_null() {
        bad_internal_call();
        return -1;
    }
    let key = new_str(read_c_string(key));
    let status = PyDict_GetItemRef(object, key, result);
    dec_ref(key);
    status
}

/// `PyDict_GetItemWithError`: a borrowed value, with errors for invalid keys.
#[no_mangle]
pub extern "C" fn PyDict_GetItemWithError(
    object: *mut ShimObject,
    key: *mut ShimObject,
) -> *mut ShimObject {
    if key.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    if !hashable(key) {
        set_error(format!(
            "TypeError: unhashable type: '{}'",
            object_type_name(key)
        ));
        return ptr::null_mut();
    }
    let pairs = match dict_slots(object) {
        Ok(pairs) => pairs,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    match dict_position(pairs, key) {
        Some(index) => pairs[index].1 as *mut ShimObject,
        None => ptr::null_mut(),
    }
}

/// `PyDict_SetDefaultRef`: inserts `default` only when the key is absent.
///
/// Returns 1 when the key already existed, 0 when it was inserted, and -1 on
/// error. `*result` receives a new reference either way.
#[no_mangle]
pub extern "C" fn PyDict_SetDefaultRef(
    object: *mut ShimObject,
    key: *mut ShimObject,
    default_value: *mut ShimObject,
    result: *mut *mut ShimObject,
) -> c_int {
    if result.is_null() {
        bad_internal_call();
        return -1;
    }
    // SAFETY: caller-supplied writable slot.
    unsafe { *result = ptr::null_mut() };
    if key.is_null() || default_value.is_null() {
        bad_internal_call();
        return -1;
    }
    if !hashable(key) {
        set_error(format!(
            "TypeError: unhashable type: '{}'",
            object_type_name(key)
        ));
        return -1;
    }
    let pairs = match dict_slots(object) {
        Ok(pairs) => pairs,
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    if let Some(index) = dict_position(pairs, key) {
        let value = pairs[index].1 as *mut ShimObject;
        inc_ref(value);
        // SAFETY: validated slot.
        unsafe { *result = value };
        return 1;
    }
    if let Err(error) = dict_set(object, key, default_value) {
        set_error(error);
        return -1;
    }
    inc_ref(default_value);
    // SAFETY: validated slot.
    unsafe { *result = default_value };
    0
}

/// `PyDict_Merge`: merges `other` into `object`, optionally skipping keys that
/// are already present.
#[no_mangle]
pub extern "C" fn PyDict_Merge(
    object: *mut ShimObject,
    other: *mut ShimObject,
    override_: c_int,
) -> c_int {
    if object.is_null() || other.is_null() || !matches!(kind_ref(object), Some(ShimKind::Dict(_))) {
        bad_internal_call();
        return -1;
    }
    if override_ != 0 {
        return PyDict_Update(object, other);
    }
    if let Some(ShimKind::Dict(pairs)) = kind_ref(other) {
        for &(key, value) in pairs {
            let key = key as *mut ShimObject;
            let value = value as *mut ShimObject;
            let exists = matches!(
                kind_ref(object),
                Some(ShimKind::Dict(existing)) if dict_position(existing, key).is_some()
            );
            if exists {
                continue;
            }
            if let Err(error) = dict_set(object, key, value) {
                set_error(error);
                return -1;
            }
        }
        return 0;
    }
    let keys = match attribute_lookup(other, "keys") {
        Ok(keys) => keys,
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    let keys = {
        let result = PyObject_CallNoArgs(keys);
        dec_ref(keys);
        result
    };
    if keys.is_null() {
        return -1;
    }
    let entries = match iterable_elements(keys) {
        Ok(entries) => entries,
        Err(error) => {
            dec_ref(keys);
            set_error(error);
            return -1;
        }
    };
    dec_ref(keys);
    for &key in &entries {
        let key = key as *mut ShimObject;
        let value = types::PyObject_GetItem(other, key);
        if value.is_null() {
            let error = take_error()
                .unwrap_or_else(|| "TypeError: cannot read the mapping value".to_string());
            release_elements(&entries);
            set_error(error);
            return -1;
        }
        let exists = matches!(
            kind_ref(object),
            Some(ShimKind::Dict(existing)) if dict_position(existing, key).is_some()
        );
        if !exists {
            if let Err(error) = dict_set(object, key, value) {
                dec_ref(value);
                release_elements(&entries);
                set_error(error);
                return -1;
            }
        }
        dec_ref(value);
    }
    release_elements(&entries);
    0
}

/// `PyDictProxy_New`: a read-only mapping view over `mapping`.
#[no_mangle]
pub extern "C" fn PyDictProxy_New(mapping: *mut ShimObject) -> *mut ShimObject {
    if mapping.is_null() || !matches!(kind_ref(mapping), Some(ShimKind::Dict(_))) {
        set_error("TypeError: PyDictProxy_New: a dict is required");
        return ptr::null_mut();
    }
    inc_ref(mapping);
    new_object(ShimKind::DictProxy {
        dict: mapping as usize,
    })
}

// -- exception state --------------------------------------------------------

/// The class of the raised exception, without clearing the error indicator.
fn pending_error_class() -> *mut ShimObject {
    let (message, exception) = CALL_ERROR.with(|slot| {
        let state = slot.borrow();
        (state.message.clone(), state.exception)
    });
    if let Some(exception) = exception {
        let object = exception as *mut ShimObject;
        let class = exception_class_of(object);
        if !class.is_null() {
            return class;
        }
    }
    let Some(message) = message else {
        return ptr::null_mut();
    };
    let name = message
        .split_once(": ")
        .map(|(name, _)| name)
        .unwrap_or(&message);
    let known = exception_class_by_name(name);
    if known.is_null() {
        exception_class_by_name(name.rsplit('.').next().unwrap_or(name))
    } else {
        known
    }
}

/// `PyErr_ExceptionMatches`.
#[no_mangle]
pub extern "C" fn PyErr_ExceptionMatches(exception: *mut ShimObject) -> c_int {
    let pending = pending_error_class();
    if pending.is_null() {
        return 0;
    }
    PyErr_GivenExceptionMatches(pending, exception)
}

/// `PyErr_CheckSignals`: this host installs no signal handlers, so there is
/// never a pending signal to deliver.
#[no_mangle]
pub extern "C" fn PyErr_CheckSignals() -> c_int {
    0
}

/// `PyErr_Print`: the historical spelling of `PyErr_PrintEx(1)`.
#[no_mangle]
pub extern "C" fn PyErr_Print() {
    PyErr_PrintEx(1);
}

/// `PyErr_Fetch`: splits the raised exception into type/value/traceback.
#[no_mangle]
pub extern "C" fn PyErr_Fetch(
    exception_type: *mut *mut ShimObject,
    exception_value: *mut *mut ShimObject,
    exception_traceback: *mut *mut ShimObject,
) {
    let raised = PyErr_GetRaisedException();
    // SAFETY: all three pointers are caller-provided writable slots; callers
    // that do not want one pass NULL.
    unsafe {
        if raised.is_null() {
            if !exception_type.is_null() {
                *exception_type = ptr::null_mut();
            }
            if !exception_value.is_null() {
                *exception_value = ptr::null_mut();
            }
            if !exception_traceback.is_null() {
                *exception_traceback = ptr::null_mut();
            }
            return;
        }
        let class = exception_class_of(raised);
        inc_ref(class);
        let traceback = match kind_ref(raised) {
            Some(ShimKind::Exception { traceback, .. }) if *traceback != 0 => {
                let traceback = *traceback as *mut ShimObject;
                inc_ref(traceback);
                traceback
            }
            _ => ptr::addr_of_mut!(_Py_NoneStruct),
        };
        if !exception_type.is_null() {
            *exception_type = class;
        } else {
            dec_ref(class);
        }
        if !exception_value.is_null() {
            *exception_value = raised;
        } else {
            dec_ref(raised);
        }
        if !exception_traceback.is_null() {
            *exception_traceback = traceback;
        } else {
            dec_ref(traceback);
        }
    }
}

/// `PyErr_NormalizeException`: materialises the class/value pair into an
/// exception instance and replaces the traceback NULL with `None`.
#[no_mangle]
pub extern "C" fn PyErr_NormalizeException(
    exception_type: *mut *mut ShimObject,
    exception_value: *mut *mut ShimObject,
    exception_traceback: *mut *mut ShimObject,
) {
    if exception_type.is_null() || exception_value.is_null() || exception_traceback.is_null() {
        bad_internal_call();
        return;
    }
    // SAFETY: the three slots were validated as writable.
    unsafe {
        let class = *exception_type;
        let value = *exception_value;
        let traceback = *exception_traceback;
        if class.is_null() {
            if !value.is_null() {
                dec_ref(value);
                *exception_value = ptr::null_mut();
            }
            if !traceback.is_null() {
                dec_ref(traceback);
                *exception_traceback = ptr::null_mut();
            }
            return;
        }
        let class = if types::is_type_pointer(class) {
            class
        } else if let Some(ShimKind::Exception { exc_type, .. }) = kind_ref(class) {
            // An instance was supplied in the type slot; promote its class.
            let instance = class;
            let promoted = *exc_type as *mut ShimObject;
            inc_ref(promoted);
            dec_ref(instance);
            *exception_type = promoted;
            promoted
        } else {
            set_error("SystemError: PyErr_NormalizeException: exception class expected");
            return;
        };
        let normalized = match kind_ref(value) {
            Some(ShimKind::Exception { exc_type, .. }) => {
                let value_class = *exc_type as *mut ShimObject;
                if value_class == class
                    || (types::is_type_pointer(value_class)
                        && types::is_type_pointer(class)
                        && types::is_subtype(
                            value_class as *mut TypeObject,
                            class as *mut TypeObject,
                        ))
                {
                    value
                } else {
                    // Re-wrap a mis-typed instance as an argument to `class`.
                    let instance = new_exception_instance(class as usize, vec![value]);
                    dec_ref(value);
                    instance
                }
            }
            Some(ShimKind::Tuple(items)) => {
                let arguments: Vec<*mut ShimObject> =
                    items.iter().map(|&item| item as *mut ShimObject).collect();
                let instance = new_exception_instance(class as usize, arguments);
                dec_ref(value);
                instance
            }
            Some(ShimKind::None) => {
                let instance = new_exception_instance(class as usize, Vec::new());
                dec_ref(value);
                instance
            }
            _ if value.is_null() => new_exception_instance(class as usize, Vec::new()),
            _ => {
                let arguments = vec![value];
                let instance = new_exception_instance(class as usize, arguments);
                dec_ref(value);
                instance
            }
        };
        *exception_value = normalized;
        let traceback = if traceback.is_null() {
            inc_ref(ptr::addr_of_mut!(_Py_NoneStruct));
            ptr::addr_of_mut!(_Py_NoneStruct)
        } else {
            traceback
        };
        *exception_traceback = traceback;
    }
}

/// `PyErr_Restore`: the inverse of [`PyErr_Fetch`]; steals all three
/// references.
#[no_mangle]
pub extern "C" fn PyErr_Restore(
    exception_type: *mut ShimObject,
    exception_value: *mut ShimObject,
    exception_traceback: *mut ShimObject,
) {
    if exception_type.is_null() {
        dec_ref(exception_value);
        dec_ref(exception_traceback);
        clear_error();
        return;
    }
    let class = exception_class_of(exception_type);
    let instance = if matches!(kind_ref(exception_value), Some(ShimKind::Exception { .. })) {
        exception_value
    } else if exception_value.is_null() {
        new_exception_instance(class as usize, Vec::new())
    } else {
        let arguments = vec![exception_value];
        let instance = new_exception_instance(class as usize, arguments);
        dec_ref(exception_value);
        instance
    };
    dec_ref(exception_type);
    if !exception_traceback.is_null() {
        let stored = if exception_traceback == ptr::addr_of_mut!(_Py_NoneStruct) {
            0
        } else {
            exception_traceback as usize
        };
        // SAFETY: `instance` is a fresh or supplied exception instance.
        if let ShimKind::Exception { traceback, .. } = unsafe { kind_mut(instance) } {
            let previous = std::mem::replace(traceback, stored);
            if previous != 0 && previous != stored {
                dec_ref(previous as *mut ShimObject);
            }
        }
        if stored == 0 {
            dec_ref(exception_traceback);
        }
    }
    PyErr_SetRaisedException(instance);
}

/// `_PyErr_BadInternalCall`: the internal spelling that names the caller's
/// source location in the diagnostic.
#[no_mangle]
pub extern "C" fn _PyErr_BadInternalCall(filename: *const c_char, line: c_int) {
    let location = read_c_string(filename);
    set_error(format!(
        "SystemError: {location}:{line}: bad argument to internal function"
    ));
}

/// `PyException_SetContext`: stores `__context__`, stealing `context`.
#[no_mangle]
pub extern "C" fn PyException_SetContext(exception: *mut ShimObject, context: *mut ShimObject) {
    if exception.is_null() {
        set_error("SystemError: PyException_SetContext: not an exception instance");
        return;
    }
    // SAFETY: live object; the context slot is written under the GIL.
    let ShimKind::Exception { context: slot, .. } = (unsafe { kind_mut(exception) }) else {
        set_error("SystemError: PyException_SetContext: not an exception instance");
        return;
    };
    let previous = std::mem::replace(slot, context as usize);
    if previous != 0 && previous != context as usize {
        dec_ref(previous as *mut ShimObject);
    }
}

// -- integers and floats ----------------------------------------------------

/// Shared implementation of the `PyLong_As*AndOverflow` family.
fn long_and_overflow(object: *mut ShimObject, overflow: *mut c_int) -> c_long {
    if !overflow.is_null() {
        // SAFETY: caller-supplied writable slot.
        unsafe { *overflow = 0 };
    }
    let value = match as_int(object) {
        Ok(value) => value,
        Err(_) => {
            set_error("TypeError: an integer is required");
            return -1;
        }
    };
    match c_long::try_from(value) {
        Ok(value) => value,
        Err(_) => {
            if !overflow.is_null() {
                // SAFETY: validated above.
                unsafe { *overflow = if value < 0 { -1 } else { 1 } };
            }
            -1
        }
    }
}

/// `PyLong_AsLongAndOverflow`.
#[no_mangle]
pub extern "C" fn PyLong_AsLongAndOverflow(
    object: *mut ShimObject,
    overflow: *mut c_int,
) -> c_long {
    long_and_overflow(object, overflow)
}

/// `PyLong_AsLongLong`.
#[no_mangle]
pub extern "C" fn PyLong_AsLongLong(object: *mut ShimObject) -> i64 {
    match as_int(object) {
        Ok(value) => value,
        Err(_) => {
            set_error("TypeError: an integer is required");
            -1
        }
    }
}

/// `PyLong_AsLongLongAndOverflow`.
#[no_mangle]
pub extern "C" fn PyLong_AsLongLongAndOverflow(
    object: *mut ShimObject,
    overflow: *mut c_int,
) -> i64 {
    if !overflow.is_null() {
        // SAFETY: caller-supplied writable slot.
        unsafe { *overflow = 0 };
    }
    match as_int(object) {
        Ok(value) => value,
        Err(_) => {
            set_error("TypeError: an integer is required");
            -1
        }
    }
}

/// `PyLong_AsSsize_t`.
#[no_mangle]
pub extern "C" fn PyLong_AsSsize_t(object: *mut ShimObject) -> isize {
    match as_int(object) {
        Ok(value) => value as isize,
        Err(_) => {
            set_error("TypeError: an integer is required");
            -1
        }
    }
}

/// `PyLong_AsUnsignedLong`.
#[no_mangle]
pub extern "C" fn PyLong_AsUnsignedLong(object: *mut ShimObject) -> c_ulong {
    let value = match as_int(object) {
        Ok(value) => value,
        Err(_) => {
            set_error("TypeError: an integer is required");
            return c_ulong::MAX;
        }
    };
    if value < 0 {
        set_error("OverflowError: can't convert negative value to unsigned long");
        return c_ulong::MAX;
    }
    value as c_ulong
}

/// `PyLong_AsVoidPtr`.
#[no_mangle]
pub extern "C" fn PyLong_AsVoidPtr(object: *mut ShimObject) -> *mut c_void {
    match as_int(object) {
        Ok(value) => value as usize as *mut c_void,
        Err(_) => {
            set_error("TypeError: an integer is required");
            ptr::null_mut()
        }
    }
}

/// `PyLong_FromLongLong`.
#[no_mangle]
pub extern "C" fn PyLong_FromLongLong(value: i64) -> *mut ShimObject {
    new_int(value)
}

/// `PyLong_FromVoidPtr`.
#[no_mangle]
pub extern "C" fn PyLong_FromVoidPtr(pointer: *mut c_void) -> *mut ShimObject {
    new_int(pointer as usize as i64)
}

/// `PyLong_FromUnsignedLong`.
#[no_mangle]
pub extern "C" fn PyLong_FromUnsignedLong(value: c_ulong) -> *mut ShimObject {
    PyLong_FromUnsignedLongLong(value)
}

/// `PyLong_FromDouble`: truncates toward zero like CPython's float-to-int
/// conversion and rejects NaN/infinity and values outside the host's 64-bit
/// integer model.
#[no_mangle]
pub extern "C" fn PyLong_FromDouble(value: f64) -> *mut ShimObject {
    if value.is_nan() {
        set_error("OverflowError: cannot convert float NaN to integer");
        return ptr::null_mut();
    }
    if value.is_infinite() {
        set_error("OverflowError: cannot convert float infinity to integer");
        return ptr::null_mut();
    }
    let truncated = value.trunc();
    if truncated < i64::MIN as f64 || truncated >= i64::MAX as f64 {
        set_error(format!(
            "OverflowError: integer {value} does not fit this host's 64-bit int model"
        ));
        return ptr::null_mut();
    }
    new_int(truncated as i64)
}

/// Parses the integer text accepted by `int(string, base)`.
fn parse_integer_text(text: &str, base: c_int) -> Result<i64, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "ValueError: invalid literal for int() with base {base}: {text:?}"
        ));
    }
    let (negative, body) = match trimmed.as_bytes()[0] {
        b'+' => (false, &trimmed[1..]),
        b'-' => (true, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let (base, digits) = match base {
        0 => {
            let lower = body.to_ascii_lowercase();
            if lower.starts_with("0x") {
                (16, &body[2..])
            } else if lower.starts_with("0o") {
                (8, &body[2..])
            } else if lower.starts_with("0b") {
                (2, &body[2..])
            } else {
                (10, body)
            }
        }
        2..=36 => {
            let lower = body.to_ascii_lowercase();
            let prefix = match base {
                16 => Some("0x"),
                8 => Some("0o"),
                2 => Some("0b"),
                _ => None,
            };
            match prefix {
                Some(prefix) if lower.starts_with(prefix) => (base, &body[2..]),
                _ => (base, body),
            }
        }
        _ => return Err("ValueError: int() base must be >= 2 and <= 36, or 0".to_string()),
    };
    let bytes = digits.as_bytes();
    let mut value: i128 = 0;
    let mut seen_digit = false;
    let mut previous_underscore = false;
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'_' {
            let valid = index > 0
                && !previous_underscore
                && index + 1 < bytes.len()
                && bytes[index - 1].is_ascii_alphanumeric()
                && bytes[index + 1].is_ascii_alphanumeric();
            if !valid {
                return Err(format!(
                    "ValueError: invalid literal for int() with base {base}: {text:?}"
                ));
            }
            previous_underscore = true;
            continue;
        }
        previous_underscore = false;
        let digit = match (byte as char).to_digit(base as u32) {
            Some(digit) => digit,
            None => {
                return Err(format!(
                    "ValueError: invalid literal for int() with base {base}: {text:?}"
                ))
            }
        };
        seen_digit = true;
        value = value
            .checked_mul(base as i128)
            .and_then(|value| value.checked_add(digit as i128))
            .ok_or_else(|| "OverflowError: int too large to convert".to_string())?;
    }
    if !seen_digit {
        return Err(format!(
            "ValueError: invalid literal for int() with base {base}: {text:?}"
        ));
    }
    let value = if negative { -value } else { value };
    i64::try_from(value).map_err(|_| {
        "OverflowError: int too large to convert this host's 64-bit int model".to_string()
    })
}

/// `PyLong_FromUnicodeObject`: `int(string, base)`.
#[no_mangle]
pub extern "C" fn PyLong_FromUnicodeObject(
    object: *mut ShimObject,
    base: c_int,
) -> *mut ShimObject {
    let Some(ShimKind::Str { text, .. }) = kind_ref(object) else {
        set_error("TypeError: PyLong_FromUnicodeObject: argument must be a string");
        return ptr::null_mut();
    };
    match parse_integer_text(text, base) {
        Ok(value) => new_int(value),
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyLong_IsZero`.
#[no_mangle]
pub extern "C" fn PyLong_IsZero(object: *mut ShimObject) -> c_int {
    matches!(
        kind_ref(object),
        Some(ShimKind::Int(0) | ShimKind::Bool(false))
    )
    .into()
}

/// `PyFloat_FromString`: parses the text accepted by `float(string)`.
#[no_mangle]
pub extern "C" fn PyFloat_FromString(object: *mut ShimObject) -> *mut ShimObject {
    let text = match kind_ref(object) {
        Some(ShimKind::Str { text, .. }) => text.clone(),
        Some(ShimKind::Bytes { data, .. }) | Some(ShimKind::ByteArray { data, .. }) => {
            match String::from_utf8(data.clone()) {
                Ok(text) => text,
                Err(_) => {
                    set_error("ValueError: could not convert bytes to float");
                    return ptr::null_mut();
                }
            }
        }
        _ => {
            set_error(format!(
                "TypeError: float() argument must be a string or a real number, not '{}'",
                object_type_name(object)
            ));
            return ptr::null_mut();
        }
    };
    match parse_float_text(&text) {
        Ok(value) => new_object(ShimKind::Float(value)),
        Err(()) => {
            set_error(format!(
                "ValueError: could not convert string to float: {}",
                shim_repr(object)
            ));
            ptr::null_mut()
        }
    }
}

/// Parses CPython's float-string grammar (surrounding whitespace, optional
/// sign, `inf`/`infinity`/`nan`, and underscores between digits).
fn parse_float_text(text: &str) -> Result<f64, ()> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(());
    }
    let (sign, body) = match trimmed.as_bytes()[0] {
        b'+' => (1.0, &trimmed[1..]),
        b'-' => (-1.0, &trimmed[1..]),
        _ => (1.0, trimmed),
    };
    let lower = body.to_ascii_lowercase();
    if lower == "inf" || lower == "infinity" {
        return Ok(f64::INFINITY.copysign(sign));
    }
    if lower == "nan" {
        return Ok(f64::NAN);
    }
    let bytes = body.as_bytes();
    if bytes.first() == Some(&b'_') || bytes.last() == Some(&b'_') {
        return Err(());
    }
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'_' {
            let previous = bytes[index - 1];
            let next = bytes[index + 1];
            if !previous.is_ascii_digit() || !next.is_ascii_digit() {
                return Err(());
            }
        }
    }
    let cleaned: String = body.chars().filter(|&character| character != '_').collect();
    let value: f64 = cleaned.parse().map_err(|_| ())?;
    Ok(if sign < 0.0 { -value } else { value })
}

/// `frexp`: splits `value` into a normalized fraction and a base-2 exponent.
fn frexp(value: f64) -> (f64, i32) {
    if value == 0.0 || !value.is_finite() {
        return (value, 0);
    }
    let bits = value.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as i32;
    if exponent == 0 {
        let (fraction, sub_exponent) = frexp(value * 2f64.powi(64));
        return (fraction, sub_exponent - 64);
    }
    let fraction_bits = (bits & !(0x7ff << 52)) | (1022u64 << 52);
    (f64::from_bits(fraction_bits), exponent - 1022)
}

/// `_Py_HashDouble`: CPython's numeric hash for a float.
#[no_mangle]
pub extern "C" fn _Py_HashDouble(object: *mut ShimObject, value: f64) -> isize {
    const HASH_BITS: u32 = 61;
    const MODULUS: u64 = (1u64 << HASH_BITS) - 1;
    const HASH_INF: u64 = 314_159;
    if !value.is_finite() {
        if value.is_infinite() {
            return if value > 0.0 {
                HASH_INF as isize
            } else {
                -(HASH_INF as isize)
            };
        }
        // NaN: CPython hashes the object identity, remapped away from -1.
        let hash = object as isize;
        return if hash == -1 { -2 } else { hash };
    }
    let (mut fraction, exponent) = frexp(value);
    let mut exponent = exponent as i64;
    let sign: i64 = if fraction < 0.0 {
        fraction = -fraction;
        -1
    } else {
        1
    };
    let mut hash: u64 = 0;
    while fraction != 0.0 {
        hash = ((hash << 28) & MODULUS) | (hash >> (HASH_BITS - 28));
        fraction *= 268_435_456.0; // 2**28
        exponent -= 28;
        let chunk = fraction as u64;
        fraction -= chunk as f64;
        hash = hash.wrapping_add(chunk);
        if hash >= MODULUS {
            hash -= MODULUS;
        }
    }
    let shift = if exponent >= 0 {
        exponent as u32 % HASH_BITS
    } else {
        HASH_BITS - 1 - ((-1 - exponent) as u32 % HASH_BITS)
    };
    hash = ((hash << shift) & MODULUS) | (hash >> (HASH_BITS - shift));
    let mut result = if sign < 0 {
        (hash as isize).wrapping_neg()
    } else {
        hash as isize
    };
    if result == -1 {
        result = -2;
    }
    result
}

/// `PyUnstable_Object_IsUniqueReferencedTemporary`: true for a handle whose
/// only reference is the caller's.
#[no_mangle]
pub extern "C" fn PyUnstable_Object_IsUniqueReferencedTemporary(object: *mut ShimObject) -> c_int {
    if object.is_null() {
        return 0;
    }
    // SAFETY: every handle starts with the CPython header.
    let count = unsafe { (*object).head.ob_refcnt.load(Ordering::Relaxed) };
    (count == 1) as c_int
}

/// `PyInterpreterState_Main`: the host runs exactly one interpreter.
#[no_mangle]
pub extern "C" fn PyInterpreterState_Main() -> *mut PyInterpreterState {
    ptr::addr_of!(MAIN_INTERPRETER) as *mut PyInterpreterState
}

/// `PyGC_Collect`: runs one collection cycle and returns the number of
/// unreachable objects it reclaimed.
///
/// The objects this entry point collects are the GC-tracked extension
/// instances registered by `PyObject_GC_Track`/`PyType_GenericAlloc`. The pass
/// calls every tracked type's `tp_traverse`, treats an instance whose
/// references all come from other tracked instances as unreachable, runs its
/// `tp_clear` exactly once and frees it, which is CPython's contract for
/// `tp_traverse`/`tp_clear`. Hydro values are not part of this graph: a Hydro
/// value held by an extension is kept alive by its own persistent root, so
/// breaking a C-side cycle can never free Hydro memory.
///
/// The Hydro heap collector is deliberately not run here: this entry point can
/// be called while a foreign thread holds the emulated GIL, and a Hydro
/// collection would have to wait for other threads' safepoints. `gc.collect()`
/// drives the Hydro heap through `hydro_gc_collect`.
#[no_mangle]
pub extern "C" fn PyGC_Collect() -> isize {
    types::collect_cycles() as isize
}

// -- Unicode ----------------------------------------------------------------

/// `PyUnicode_AsASCIIString`.
#[no_mangle]
pub extern "C" fn PyUnicode_AsASCIIString(object: *mut ShimObject) -> *mut ShimObject {
    PyUnicode_AsEncodedString(object, c"ascii".as_ptr(), ptr::null())
}

/// `PyUnicode_AsLatin1String`.
#[no_mangle]
pub extern "C" fn PyUnicode_AsLatin1String(object: *mut ShimObject) -> *mut ShimObject {
    PyUnicode_AsEncodedString(object, c"latin-1".as_ptr(), ptr::null())
}

/// `PyUnicode_AsUTF8String`.
#[no_mangle]
pub extern "C" fn PyUnicode_AsUTF8String(object: *mut ShimObject) -> *mut ShimObject {
    PyUnicode_AsEncodedString(object, c"utf-8".as_ptr(), ptr::null())
}

/// Reads a string object's text or reports `TypeError: bad argument`.
fn unicode_text(object: *mut ShimObject) -> Result<&'static str, String> {
    match kind_ref(object) {
        Some(ShimKind::Str { text, .. }) => Ok(text.as_str()),
        _ => Err("TypeError: bad argument to internal function".to_string()),
    }
}

/// `PyUnicode_GetLength`: the number of code points.
#[no_mangle]
pub extern "C" fn PyUnicode_GetLength(object: *mut ShimObject) -> isize {
    match unicode_text(object) {
        Ok(text) => py_ssize_of(text.chars().count()),
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

/// `PyUnicode_Compare`.
#[no_mangle]
pub extern "C" fn PyUnicode_Compare(left: *mut ShimObject, right: *mut ShimObject) -> c_int {
    let (left, right) = match (unicode_text(left), unicode_text(right)) {
        (Ok(left), Ok(right)) => (left, right),
        _ => {
            set_error("TypeError: PyUnicode_Compare: arguments must be strings");
            return -1;
        }
    };
    match left.cmp(right) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// `PyUnicode_CompareWithASCIIString`.
#[no_mangle]
pub extern "C" fn PyUnicode_CompareWithASCIIString(
    object: *mut ShimObject,
    value: *const c_char,
) -> c_int {
    let text = match unicode_text(object) {
        Ok(text) => text,
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    if value.is_null() {
        bad_internal_call();
        return -1;
    }
    // SAFETY: the caller passes a NUL-terminated byte string.
    let bytes = unsafe { CStr::from_ptr(value) }.to_bytes();
    let other: Vec<char> = bytes.iter().map(|&byte| byte as char).collect();
    let characters: Vec<char> = text.chars().collect();
    match characters.cmp(&other) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// `PyUnicode_Concat`.
#[no_mangle]
pub extern "C" fn PyUnicode_Concat(
    left: *mut ShimObject,
    right: *mut ShimObject,
) -> *mut ShimObject {
    let (left, right) = match (unicode_text(left), unicode_text(right)) {
        (Ok(left), Ok(right)) => (left, right),
        _ => {
            set_error("TypeError: PyUnicode_Concat: arguments must be strings");
            return ptr::null_mut();
        }
    };
    let mut text = String::with_capacity(left.len() + right.len());
    text.push_str(left);
    text.push_str(right);
    new_str(text)
}

/// `PyUnicode_Contains`: 1/0/-1 for `substr in str`.
#[no_mangle]
pub extern "C" fn PyUnicode_Contains(object: *mut ShimObject, substring: *mut ShimObject) -> c_int {
    let (text, substring) = match (unicode_text(object), unicode_text(substring)) {
        (Ok(text), Ok(substring)) => (text, substring),
        _ => {
            set_error("TypeError: PyUnicode_Contains: arguments must be strings");
            return -1;
        }
    };
    text.contains(substring) as c_int
}

/// `PyUnicode_AsUCS4`: writes the code points into a caller buffer.
#[no_mangle]
pub extern "C" fn PyUnicode_AsUCS4(
    object: *mut ShimObject,
    buffer: *mut u32,
    buflen: isize,
    copy_null: c_int,
) -> *mut u32 {
    let text = match unicode_text(object) {
        Ok(text) => text,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    if buffer.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    let length = text.chars().count();
    let required = length + usize::from(copy_null != 0);
    if buflen < 0 || (buflen as usize) < required {
        set_error("SystemError: buffer is too small for PyUnicode_AsUCS4");
        return ptr::null_mut();
    }
    // SAFETY: the caller guarantees `buflen` writable `Py_UCS4` slots.
    unsafe {
        for (index, character) in text.chars().enumerate() {
            *buffer.add(index) = character as u32;
        }
        if copy_null != 0 {
            *buffer.add(length) = 0;
        }
    }
    buffer
}

/// `PyUnicode_AsUCS4Copy`: a `PyMem_Malloc`ed, NUL-terminated copy.
#[no_mangle]
pub extern "C" fn PyUnicode_AsUCS4Copy(object: *mut ShimObject) -> *mut u32 {
    let text = match unicode_text(object) {
        Ok(text) => text,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let length = text.chars().count();
    let pointer = PyMem_Malloc((length + 1) * std::mem::size_of::<u32>()) as *mut u32;
    if pointer.is_null() {
        set_error("MemoryError: cannot allocate the UCS4 buffer");
        return ptr::null_mut();
    }
    // SAFETY: `PyMem_Malloc` returned `length + 1` writable `u32` slots.
    unsafe {
        for (index, character) in text.chars().enumerate() {
            *pointer.add(index) = character as u32;
        }
        *pointer.add(length) = 0;
    }
    pointer
}

/// `PyUnicode_FromKindAndData`.
#[no_mangle]
pub extern "C" fn PyUnicode_FromKindAndData(
    kind: c_int,
    data: *const c_void,
    size: isize,
) -> *mut ShimObject {
    if !matches!(kind, 1 | 2 | 4) {
        set_error("SystemError: PyUnicode_FromKindAndData: invalid kind");
        return ptr::null_mut();
    }
    if size < 0 {
        set_error("SystemError: negative size passed to PyUnicode_FromKindAndData");
        return ptr::null_mut();
    }
    let Ok(size) = usize::try_from(size) else {
        set_error("MemoryError: string is too large");
        return ptr::null_mut();
    };
    if size == 0 {
        return interned().empty_str as *mut ShimObject;
    }
    if data.is_null() {
        set_error(
            "SystemError: NULL data with a positive size passed to PyUnicode_FromKindAndData",
        );
        return ptr::null_mut();
    }
    let mut text = String::new();
    // SAFETY: the caller provides `size` units of `kind` bytes.
    unsafe {
        match kind {
            1 => {
                let units = std::slice::from_raw_parts(data as *const u8, size);
                for &unit in units {
                    text.push(unit as char);
                }
            }
            2 => {
                let units = std::slice::from_raw_parts(data as *const u16, size);
                for &unit in units {
                    text.push(char::from_u32(unit as u32).unwrap_or(char::REPLACEMENT_CHARACTER));
                }
            }
            _ => {
                let units = std::slice::from_raw_parts(data as *const u32, size);
                for &unit in units {
                    text.push(char::from_u32(unit).unwrap_or(char::REPLACEMENT_CHARACTER));
                }
            }
        }
    }
    new_str(text)
}

/// `PyUnicode_FromEncodedObject`: decodes a bytes-like object.
#[no_mangle]
pub extern "C" fn PyUnicode_FromEncodedObject(
    object: *mut ShimObject,
    encoding: *const c_char,
    errors: *const c_char,
) -> *mut ShimObject {
    let data: Vec<u8> = match kind_ref(object) {
        Some(ShimKind::Bytes { data, .. }) | Some(ShimKind::ByteArray { data, .. }) => data.clone(),
        _ => {
            set_error(format!(
                "TypeError: decoding to str: need a bytes-like object, {} found",
                object_type_name(object)
            ));
            return ptr::null_mut();
        }
    };
    let encoding = if encoding.is_null() {
        "utf_8".to_string()
    } else {
        normalize_encoding(&read_c_string(encoding))
    };
    let errors = if errors.is_null() {
        "strict".to_string()
    } else {
        read_c_string(errors)
    };
    let errors = match unicode_error_handler(&errors) {
        Ok(errors) => errors,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    match decode_unicode_bytes(&data, &encoding, errors) {
        Ok(text) => new_str(text),
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// Decodes `data` with the subset of codecs CPython extensions commonly use.
fn decode_unicode_bytes(data: &[u8], encoding: &str, errors: &str) -> Result<String, String> {
    match encoding {
        "utf_8" => {
            let mut text = String::with_capacity(data.len());
            let mut offset = 0usize;
            loop {
                let remaining = &data[offset..];
                match std::str::from_utf8(remaining) {
                    Ok(valid) => {
                        text.push_str(valid);
                        return Ok(text);
                    }
                    Err(error) => {
                        let valid = error.valid_up_to();
                        // SAFETY: `valid` bytes were reported as valid UTF-8.
                        text.push_str(unsafe {
                            std::str::from_utf8_unchecked(&remaining[..valid])
                        });
                        let position = offset + valid;
                        let bad = error.error_len().unwrap_or(remaining.len() - valid);
                        match errors {
                            "ignore" => {}
                            "replace" => text.push(char::REPLACEMENT_CHARACTER),
                            _ => {
                                return Err(format!(
                                    "UnicodeDecodeError: 'utf-8' codec can't decode byte 0x{:02x} \
                                     in position {position}: invalid start byte",
                                    data.get(position).copied().unwrap_or(0)
                                ))
                            }
                        }
                        offset = (position + bad).min(data.len());
                    }
                }
            }
        }
        "ascii" | "latin_1" => {
            let limit = if encoding == "ascii" { 0x7F } else { 0xFF };
            let mut text = String::with_capacity(data.len());
            for (position, &byte) in data.iter().enumerate() {
                if byte <= limit {
                    text.push(byte as char);
                } else {
                    match errors {
                        "ignore" => {}
                        "replace" => text.push(char::REPLACEMENT_CHARACTER),
                        _ => {
                            return Err(format!(
                                "UnicodeDecodeError: '{}' codec can't decode byte 0x{byte:02x} \
                                 in position {position}: ordinal not in range({})",
                                if encoding == "ascii" {
                                    "ascii"
                                } else {
                                    "latin-1"
                                },
                                limit + 1
                            ))
                        }
                    }
                }
            }
            Ok(text)
        }
        "utf_16" | "utf_16_le" | "utf_16_be" => {
            let (little, body) = match encoding {
                "utf_16" => match data {
                    [0xFF, 0xFE, ..] => (true, &data[2..]),
                    [0xFE, 0xFF, ..] => (false, &data[2..]),
                    _ => (true, data),
                },
                "utf_16_le" => (true, data),
                _ => (false, data),
            };
            if body.len() % 2 != 0 {
                if errors == "ignore" {
                    // drop the trailing byte
                } else if errors == "replace" {
                    let mut text = decode_utf16_units(body, little, errors)?;
                    text.push(char::REPLACEMENT_CHARACTER);
                    return Ok(text);
                } else {
                    return Err(format!(
                        "UnicodeDecodeError: '{}' codec can't decode bytes in position {}: \
                         truncated data",
                        encoding,
                        body.len() - 1
                    ));
                }
            }
            decode_utf16_units(&body[..body.len() - body.len() % 2], little, errors)
        }
        "utf_32" | "utf_32_le" | "utf_32_be" => {
            let (little, body) = match encoding {
                "utf_32" => match data {
                    [0xFF, 0xFE, 0x00, 0x00, ..] => (true, &data[4..]),
                    [0x00, 0x00, 0xFE, 0xFF, ..] => (false, &data[4..]),
                    _ => (true, data),
                },
                "utf_32_le" => (true, data),
                _ => (false, data),
            };
            let usable = body.len() - body.len() % 4;
            if usable != body.len() && errors == "strict" {
                return Err(format!(
                    "UnicodeDecodeError: '{}' codec can't decode bytes in position {}: \
                     truncated data",
                    encoding, usable
                ));
            }
            let mut text = String::new();
            for (index, chunk) in body[..usable].as_chunks::<4>().0.iter().enumerate() {
                let code = if little {
                    u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
                } else {
                    u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
                };
                match char::from_u32(code) {
                    Some(character) => text.push(character),
                    None => match errors {
                        "ignore" => {}
                        "replace" => text.push(char::REPLACEMENT_CHARACTER),
                        _ => {
                            return Err(format!(
                                "UnicodeDecodeError: '{}' codec can't decode bytes in position \
                                 {}: invalid code point",
                                encoding,
                                index * 4
                            ))
                        }
                    },
                }
            }
            Ok(text)
        }
        other => Err(format!("LookupError: unknown encoding: {other}")),
    }
}

/// Decodes native/little/big-endian UTF-16 units with the requested error
/// handler.
fn decode_utf16_units(data: &[u8], little: bool, errors: &str) -> Result<String, String> {
    let mut text = String::new();
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| {
            if little {
                u16::from_le_bytes([chunk[0], chunk[1]])
            } else {
                u16::from_be_bytes([chunk[0], chunk[1]])
            }
        })
        .collect();
    for (index, result) in char::decode_utf16(units).enumerate() {
        match result {
            Ok(character) => text.push(character),
            Err(_) => match errors {
                "ignore" => {}
                "replace" => text.push(char::REPLACEMENT_CHARACTER),
                _ => {
                    return Err(format!(
                        "UnicodeDecodeError: 'utf-16' codec can't decode bytes in position {}: \
                         illegal surrogate",
                        index * 2
                    ))
                }
            },
        }
    }
    Ok(text)
}

/// `PyUnicode_Replace`.
#[no_mangle]
pub extern "C" fn PyUnicode_Replace(
    object: *mut ShimObject,
    old: *mut ShimObject,
    new: *mut ShimObject,
    max_count: isize,
) -> *mut ShimObject {
    let (text, old, new) = match (unicode_text(object), unicode_text(old), unicode_text(new)) {
        (Ok(text), Ok(old), Ok(new)) => (text, old, new),
        _ => {
            set_error("TypeError: PyUnicode_Replace: arguments must be strings");
            return ptr::null_mut();
        }
    };
    new_str(replace_text(text, old, new, max_count))
}

/// `str.replace` with a replacement-count limit.
fn replace_text(text: &str, old: &str, new: &str, max_count: isize) -> String {
    let limit = if max_count < 0 {
        usize::MAX
    } else {
        max_count as usize
    };
    if limit == 0 {
        return text.to_string();
    }
    if old.is_empty() {
        let characters: Vec<char> = text.chars().collect();
        let mut result = String::new();
        let mut replaced = 0;
        for (index, character) in characters.iter().enumerate() {
            if replaced < limit {
                result.push_str(new);
                replaced += 1;
            }
            result.push(*character);
            let _ = index;
        }
        if replaced < limit {
            result.push_str(new);
        }
        return result;
    }
    let mut result = String::with_capacity(text.len());
    let mut start = 0;
    let mut replaced = 0;
    while replaced < limit {
        let Some(offset) = text[start..].find(old) else {
            break;
        };
        let position = start + offset;
        result.push_str(&text[start..position]);
        result.push_str(new);
        start = position + old.len();
        replaced += 1;
    }
    result.push_str(&text[start..]);
    result
}

/// `PyUnicode_Substring`.
#[no_mangle]
pub extern "C" fn PyUnicode_Substring(
    object: *mut ShimObject,
    start: isize,
    end: isize,
) -> *mut ShimObject {
    let text = match unicode_text(object) {
        Ok(text) => text,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let characters: Vec<char> = text.chars().collect();
    let length = characters.len() as isize;
    let start = start.max(0).min(length);
    let end = end.max(0).min(length);
    if start >= end {
        return interned().empty_str as *mut ShimObject;
    }
    new_str(characters[start as usize..end as usize].iter().collect())
}

/// Normalizes the `start`/`end` pair of a tail-match call.
fn normalize_substring_bounds(length: usize, start: isize, end: isize) -> (usize, usize) {
    let length = length as isize;
    let start = if start < 0 {
        (start + length).max(0)
    } else {
        start.min(length)
    };
    let end = if end < 0 {
        (end + length).max(0)
    } else {
        end.min(length)
    };
    (start.max(0) as usize, end.max(0) as usize)
}

/// `PyUnicode_Tailmatch`: startswith (`direction < 0`), endswith
/// (`direction > 0`) or substring containment (`direction == 0`).
#[no_mangle]
pub extern "C" fn PyUnicode_Tailmatch(
    object: *mut ShimObject,
    substring: *mut ShimObject,
    start: isize,
    end: isize,
    direction: c_int,
) -> isize {
    let (text, substring) = match (unicode_text(object), unicode_text(substring)) {
        (Ok(text), Ok(substring)) => (text, substring),
        _ => {
            set_error("TypeError: PyUnicode_Tailmatch: arguments must be strings");
            return -1;
        }
    };
    let characters: Vec<char> = text.chars().collect();
    let (start, end) = normalize_substring_bounds(characters.len(), start, end);
    if start > end {
        return 0;
    }
    let window = &characters[start..end];
    let needle: Vec<char> = substring.chars().collect();
    let found = if direction > 0 {
        window.len() >= needle.len() && window[window.len() - needle.len()..] == needle[..]
    } else if direction < 0 {
        window.len() >= needle.len() && window[..needle.len()] == needle[..]
    } else if needle.is_empty() {
        true
    } else {
        window
            .windows(needle.len())
            .any(|part| part == needle.as_slice())
    };
    found as isize
}

/// `_PyUnicode_IsAlpha`.
#[no_mangle]
pub extern "C" fn _PyUnicode_IsAlpha(character: u32) -> c_int {
    char::from_u32(character)
        .map(|ch| ch.is_alphabetic() as c_int)
        .unwrap_or(0)
}

/// `_PyUnicode_IsDecimalDigit`.
#[no_mangle]
pub extern "C" fn _PyUnicode_IsDecimalDigit(character: u32) -> c_int {
    crate::modules::unicodedata::numeric_entry(character)
        .map(|(_, decimal, _, _)| (decimal >= 0) as c_int)
        .unwrap_or(0)
}

/// `_PyUnicode_IsDigit`.
#[no_mangle]
pub extern "C" fn _PyUnicode_IsDigit(character: u32) -> c_int {
    crate::modules::unicodedata::numeric_entry(character)
        .map(|(digit, _, _, _)| (digit >= 0) as c_int)
        .unwrap_or(0)
}

/// `_PyUnicode_IsNumeric`.
#[no_mangle]
pub extern "C" fn _PyUnicode_IsNumeric(character: u32) -> c_int {
    crate::modules::unicodedata::numeric_entry(character)
        .is_some()
        .into()
}

/// `_PyUnicode_IsLowercase`.
#[no_mangle]
pub extern "C" fn _PyUnicode_IsLowercase(character: u32) -> c_int {
    char::from_u32(character)
        .map(|ch| ch.is_lowercase() as c_int)
        .unwrap_or(0)
}

/// `_PyUnicode_IsUppercase`.
#[no_mangle]
pub extern "C" fn _PyUnicode_IsUppercase(character: u32) -> c_int {
    char::from_u32(character)
        .map(|ch| ch.is_uppercase() as c_int)
        .unwrap_or(0)
}

/// `_PyUnicode_IsTitlecase`.
#[no_mangle]
pub extern "C" fn _PyUnicode_IsTitlecase(character: u32) -> c_int {
    match char::from_u32(character) {
        Some(ch) => (crate::modules::unicodedata::category_of(ch) == "Lt") as c_int,
        None => 0,
    }
}

/// `_PyUnicode_IsWhitespace`.
#[no_mangle]
pub extern "C" fn _PyUnicode_IsWhitespace(character: u32) -> c_int {
    char::from_u32(character)
        .map(|ch| ch.is_whitespace() as c_int)
        .unwrap_or(0)
}

/// `PyUnicode_InternFromString`: an interned string, with a new reference.
#[no_mangle]
pub extern "C" fn PyUnicode_InternFromString(value: *const c_char) -> *mut ShimObject {
    if value.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    let text = read_c_string(value);
    {
        let table = intern_table();
        if let Some(&interned) = table.get(&text) {
            let interned = interned as *mut ShimObject;
            inc_ref(interned);
            return interned;
        }
    }
    let object = new_str(text.clone());
    if object == interned().empty_str as *mut ShimObject {
        return object;
    }
    let mut table = intern_table();
    if let Some(&interned) = table.get(&text) {
        let interned = interned as *mut ShimObject;
        dec_ref(object);
        inc_ref(interned);
        return interned;
    }
    table.insert(text, object as usize);
    make_immortal(object);
    object
}

// -- tuples, sequences and object helpers ----------------------------------

/// `PyTuple_Check` without the stable-ABI macro: 1 for a tuple.
#[no_mangle]
pub extern "C" fn hydro_capi_is_tuple(object: *mut ShimObject) -> c_int {
    matches!(kind_ref(object), Some(ShimKind::Tuple(_))) as c_int
}

/// The Python type name of a handle, for C diagnostics.
#[no_mangle]
pub extern "C" fn hydro_capi_object_type_name(object: *mut ShimObject) -> *mut ShimObject {
    new_str(object_type_name(object))
}

/// Builds a tuple from a C array of borrowed references.
#[no_mangle]
pub extern "C" fn hydro_capi_tuple_pack(
    count: isize,
    items: *const *mut ShimObject,
) -> *mut ShimObject {
    if count < 0 {
        set_error("SystemError: negative tuple size");
        return ptr::null_mut();
    }
    if count > 0 && items.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    let tuple = PyTuple_New(count);
    if tuple.is_null() {
        return ptr::null_mut();
    }
    for index in 0..count {
        // SAFETY: the caller provides `count` entries in the array.
        let item = unsafe { *items.add(index as usize) };
        if item.is_null() {
            dec_ref(tuple);
            set_error("SystemError: NULL item passed to PyTuple_Pack");
            return ptr::null_mut();
        }
        inc_ref(item);
        if PyTuple_SetItem(tuple, index, item) != 0 {
            dec_ref(item);
            dec_ref(tuple);
            return ptr::null_mut();
        }
    }
    tuple
}

/// `PyTuple_GetSlice`: a new tuple for the clamped `[low, high)` range.
#[no_mangle]
pub extern "C" fn PyTuple_GetSlice(
    object: *mut ShimObject,
    low: isize,
    high: isize,
) -> *mut ShimObject {
    let items = match tuple_slots(object) {
        Ok(items) => items,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let length = py_ssize_of(items.len());
    let low = low.max(0).min(length);
    let high = high.max(low).min(length);
    let slice = &items[low as usize..high as usize];
    if slice.is_empty() {
        return empty_tuple();
    }
    for &item in slice {
        if item == 0 {
            set_error("SystemError: tuple contains an uninitialized item");
            return ptr::null_mut();
        }
        inc_ref(item as *mut ShimObject);
    }
    new_tuple(slice.to_vec())
}

/// `PySequence_Fast`: a list (or the sequence itself) for fast indexing.
#[no_mangle]
pub extern "C" fn PySequence_Fast(
    object: *mut ShimObject,
    message: *const c_char,
) -> *mut ShimObject {
    if object.is_null() {
        if message.is_null() {
            set_error("TypeError: argument is not a sequence");
        } else {
            set_type_error(read_c_string(message));
        }
        return ptr::null_mut();
    }
    if matches!(
        kind_ref(object),
        Some(ShimKind::List(_) | ShimKind::Tuple(_))
    ) {
        inc_ref(object);
        return object;
    }
    let elements = match iterable_elements(object) {
        Ok(elements) => elements,
        Err(error) => {
            let _ = take_error();
            if message.is_null() {
                set_error(error);
            } else {
                set_type_error(read_c_string(message));
            }
            return ptr::null_mut();
        }
    };
    new_object(ShimKind::List(elements))
}

/// `PySequence_Tuple`: an owned tuple for any iterable.
#[no_mangle]
pub extern "C" fn PySequence_Tuple(object: *mut ShimObject) -> *mut ShimObject {
    if object.is_null() {
        set_error("SystemError: PySequence_Tuple called with NULL");
        return ptr::null_mut();
    }
    if matches!(kind_ref(object), Some(ShimKind::Tuple(_))) {
        inc_ref(object);
        return object;
    }
    if let Some(ShimKind::List(items)) = kind_ref(object) {
        for &item in items {
            inc_ref(item as *mut ShimObject);
        }
        return new_tuple(items.clone());
    }
    match iterable_elements(object) {
        Ok(elements) => new_tuple(elements),
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PySeqIter_New`: an iterator over `sequence`.
#[no_mangle]
pub extern "C" fn PySeqIter_New(sequence: *mut ShimObject) -> *mut ShimObject {
    if sequence.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    inc_ref(sequence);
    new_object(ShimKind::SeqIter {
        sequence: sequence as usize,
        index: 0,
    })
}

/// The `tp_iternext` slot of `PySeqIter_Type`.
pub(crate) extern "C" fn seqiter_next(object: *mut ShimObject) -> *mut ShimObject {
    if object.is_null() {
        set_error("SystemError: sequence iterator is NULL");
        return ptr::null_mut();
    }
    let (sequence, index) = match kind_ref(object) {
        Some(ShimKind::SeqIter { sequence, index }) => (*sequence as *mut ShimObject, *index),
        _ => {
            set_error("TypeError: expected a sequence iterator");
            return ptr::null_mut();
        }
    };
    let item = types::sequence_item_at(sequence, index);
    if item.is_null() {
        if pending_error_matches(unsafe { PyExc_IndexError }) {
            clear_error();
        }
        return ptr::null_mut();
    }
    // SAFETY: the payload was validated above and the GIL serialises access.
    if let ShimKind::SeqIter { index, .. } = unsafe { kind_mut(object) } {
        *index += 1;
    }
    item
}

/// `PyObject_AsFileDescriptor`.
#[no_mangle]
pub extern "C" fn PyObject_AsFileDescriptor(object: *mut ShimObject) -> c_int {
    if let Ok(value) = as_int(object) {
        return value as c_int;
    }
    let fileno = match attribute_lookup(object, "fileno") {
        Ok(fileno) => fileno,
        Err(_) => {
            set_error("TypeError: an integer is required (got type that has no fileno() method)");
            return -1;
        }
    };
    let result = PyObject_CallNoArgs(fileno);
    dec_ref(fileno);
    if result.is_null() {
        return -1;
    }
    let status = match as_int(result) {
        Ok(value) => value as c_int,
        Err(_) => {
            set_error("TypeError: fileno() returned a non-integer");
            -1
        }
    };
    dec_ref(result);
    status
}

/// `PyObject_Bytes`: `bytes(obj)`.
#[no_mangle]
pub extern "C" fn PyObject_Bytes(object: *mut ShimObject) -> *mut ShimObject {
    if object.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    match kind_ref(object) {
        Some(ShimKind::Bytes { .. }) => {
            inc_ref(object);
            return object;
        }
        Some(ShimKind::ByteArray { data, .. }) => return new_bytes(data.clone()),
        _ => {}
    }
    match attribute_lookup(object, "__bytes__") {
        Ok(method) => {
            let result = PyObject_CallNoArgs(method);
            dec_ref(method);
            if result.is_null() {
                return ptr::null_mut();
            }
            if !matches!(kind_ref(result), Some(ShimKind::Bytes { .. })) {
                dec_ref(result);
                set_error("TypeError: __bytes__ returned non-bytes");
                return ptr::null_mut();
            }
            return result;
        }
        Err(error) if error.starts_with("AttributeError:") => {}
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    }
    set_error(format!(
        "TypeError: cannot convert '{}' object to bytes",
        object_type_name(object)
    ));
    ptr::null_mut()
}

/// `PyObject_Format`: `format(obj, format_spec)`.
#[no_mangle]
pub extern "C" fn PyObject_Format(
    object: *mut ShimObject,
    format_spec: *mut ShimObject,
) -> *mut ShimObject {
    if object.is_null() {
        set_error("SystemError: PyObject_Format: object is NULL");
        return ptr::null_mut();
    }
    let spec = if format_spec.is_null() {
        String::new()
    } else {
        match unicode_text(format_spec) {
            Ok(text) => text.to_string(),
            Err(_) => {
                set_error("TypeError: format_spec must be a string");
                return ptr::null_mut();
            }
        }
    };
    match attribute_lookup(object, "__format__") {
        Ok(method) => {
            let spec_object = new_str(spec);
            let result = PyObject_CallOneArg(method, spec_object);
            dec_ref(spec_object);
            dec_ref(method);
            if !result.is_null() && !matches!(kind_ref(result), Some(ShimKind::Str { .. })) {
                dec_ref(result);
                set_error("TypeError: __format__ must return a str");
                return ptr::null_mut();
            }
            return result;
        }
        Err(error) if error.starts_with("AttributeError:") => {}
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    }
    match object_to_value(object) {
        Ok(value) => {
            let hydro = match crate::modules::extension::shim_to_value(value) {
                Ok(hydro) => hydro,
                Err(error) => {
                    set_error(error);
                    return ptr::null_mut();
                }
            };
            match crate::core::format::format_value(hydro, &spec, 0) {
                Ok(text) => new_str(text),
                Err(error) => {
                    if error.tag == crate::exc::VALUE_ERROR {
                        set_error(format!("ValueError: {}", error.message));
                    } else {
                        set_error(format!("TypeError: {}", error.message));
                    }
                    ptr::null_mut()
                }
            }
        }
        Err(_) if spec.is_empty() => PyObject_Str(object),
        Err(_) => {
            set_error(format!(
                "TypeError: unsupported format string passed to {}.__format__",
                object_type_name(object)
            ));
            ptr::null_mut()
        }
    }
}

/// `PyObject_GetOptionalAttr`: 1 found, 0 absent, -1 error.
#[no_mangle]
pub extern "C" fn PyObject_GetOptionalAttr(
    object: *mut ShimObject,
    name: *mut ShimObject,
    result: *mut *mut ShimObject,
) -> c_int {
    if result.is_null() {
        bad_internal_call();
        return -1;
    }
    // SAFETY: caller-supplied writable slot.
    unsafe { *result = ptr::null_mut() };
    let Some(name_text) = as_string(name) else {
        set_error("TypeError: attribute name must be string");
        return -1;
    };
    match attribute_lookup(object, &name_text) {
        Ok(value) => {
            // SAFETY: validated slot.
            unsafe { *result = value };
            1
        }
        Err(error) if error.starts_with("AttributeError:") => 0,
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

/// `PyObject_HasAttrString`: 1/0, suppressing lookup errors.
#[no_mangle]
pub extern "C" fn PyObject_HasAttrString(object: *mut ShimObject, name: *const c_char) -> c_int {
    if name.is_null() {
        return 0;
    }
    let value = PyObject_GetAttrString(object, name);
    if value.is_null() {
        let _ = take_error();
        return 0;
    }
    dec_ref(value);
    1
}

/// `PyObject_Print`.
#[no_mangle]
pub extern "C" fn PyObject_Print(
    object: *mut ShimObject,
    file: *mut libc::FILE,
    flags: c_int,
) -> c_int {
    if object.is_null() {
        bad_internal_call();
        return -1;
    }
    if file.is_null() {
        set_error("SystemError: PyObject_Print: file is NULL");
        return -1;
    }
    let text = if flags & 1 != 0 {
        PyObject_Str(object)
    } else {
        PyObject_Repr(object)
    };
    if text.is_null() {
        return -1;
    }
    let mut length: isize = 0;
    let pointer = PyUnicode_AsUTF8AndSize(text, &mut length);
    if pointer.is_null() {
        dec_ref(text);
        return -1;
    }
    // SAFETY: `pointer` addresses `length` readable bytes belonging to `text`.
    let written = unsafe { libc::fwrite(pointer as *const c_void, 1, length as usize, file) };
    dec_ref(text);
    if written != length as usize {
        set_error("OSError: failed to write to the output stream");
        return -1;
    }
    0
}

/// `PyObject_ClearWeakRefs`.
///
/// This host does not export `PyWeakref_NewRef`, so no weak reference to a
/// shim object can exist; there is nothing to clear. The argument is still
/// validated so an extension passing a bogus pointer is diagnosed.
#[no_mangle]
pub extern "C" fn PyObject_ClearWeakRefs(object: *mut ShimObject) {
    if object.is_null() {
        bad_internal_call();
    }
}

// -- memoryview -------------------------------------------------------------

static MEMORYVIEW_BUFFER_PROCS: LazyLock<buffer::PyBufferProcs> =
    LazyLock::new(|| buffer::PyBufferProcs {
        bf_getbuffer: memoryview_getbuffer as *const () as usize,
        bf_releasebuffer: 0,
    });

/// Returns the buffer suite installed on `PyMemoryView_Type`.
pub(crate) fn memoryview_buffer_procs() -> *mut buffer::PyBufferProcs {
    &*MEMORYVIEW_BUFFER_PROCS as *const buffer::PyBufferProcs as *mut buffer::PyBufferProcs
}

/// `bf_getbuffer` for a memoryview: re-exports the stored view.
pub(crate) extern "C" fn memoryview_getbuffer(
    object: *mut ShimObject,
    view: *mut buffer::Py_buffer,
    flags: c_int,
) -> c_int {
    if view.is_null() {
        bad_internal_call();
        return -1;
    }
    let Some(ShimKind::MemoryView { buffer }) = kind_ref(object) else {
        set_error("TypeError: memoryview buffer expected");
        return -1;
    };
    let source = buffer.lock();
    if flags & buffer::PYBUF_WRITABLE != 0 && source.readonly != 0 {
        set_error("BufferError: memoryview is read-only");
        return -1;
    }
    // SAFETY: `view` is caller-owned storage for a `Py_buffer`.
    unsafe {
        ptr::write_bytes(view as *mut u8, 0, std::mem::size_of::<buffer::Py_buffer>());
        (*view).buf = source.buf;
        (*view).obj = object;
        (*view).len = source.len;
        (*view).itemsize = source.itemsize;
        (*view).readonly = source.readonly;
        (*view).ndim = source.ndim;
        (*view).format = source.format;
        (*view).shape = source.shape;
        (*view).strides = source.strides;
        (*view).suboffsets = source.suboffsets;
        (*view).internal = source.internal;
    }
    inc_ref(object);
    0
}

/// `PyMemoryView_FromObject`: acquires a full read-only buffer.
///
/// The buffer keeps the exporter's raw `Py_buffer` alive and is only touched
/// while the GIL is held, so `MemoryViewBuffer` is intentionally not
/// `Send`/`Sync`; the `Arc` is shared within one interpreter, never across
/// threads.
#[allow(clippy::arc_with_non_send_sync)]
#[no_mangle]
pub extern "C" fn PyMemoryView_FromObject(object: *mut ShimObject) -> *mut ShimObject {
    if object.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    let flags = buffer::PYBUF_FORMAT | buffer::PYBUF_STRIDES;
    // SAFETY: an all-zero `Py_buffer` is the documented empty state.
    let mut view: buffer::Py_buffer = unsafe { std::mem::zeroed() };
    if buffer::PyObject_GetBuffer(object, &mut view, flags) != 0 {
        return ptr::null_mut();
    }
    let source = ptr::addr_of!(view);
    let buffer = Arc::new(MemoryViewBuffer::new(view, source));
    // `PyBuffer_FillInfo`-style exporters point `shape`/`strides` into the
    // `Py_buffer` they were handed; re-point them at the stored copy now that
    // the `Arc` gives it a stable address.
    buffer.relocate_self_references();
    new_object(ShimKind::MemoryView { buffer })
}

// -- generic aliases --------------------------------------------------------

/// `Py_GenericAlias`: `types.GenericAlias(origin, args)`.
#[no_mangle]
pub extern "C" fn Py_GenericAlias(
    origin: *mut ShimObject,
    args: *mut ShimObject,
) -> *mut ShimObject {
    if origin.is_null() || args.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    if !matches!(kind_ref(args), Some(ShimKind::Tuple(_))) {
        set_error("TypeError: Py_GenericAlias: args must be a tuple");
        return ptr::null_mut();
    }
    inc_ref(origin);
    inc_ref(args);
    new_object(ShimKind::GenericAlias {
        origin: origin as usize,
        args: args as usize,
    })
}

// -- context variables ------------------------------------------------------

/// Identifies the calling OS thread for context-variable storage.
fn current_thread_key() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut hasher);
    hasher.finish()
}

/// `PyContextVar_New`.
///
/// CPython's second argument is the variable's *default object* — any
/// `PyObject *`, not a doc string — and the newly created variable owns a new
/// reference to it. `PyContextVar_Get` returns it when no value has been set
/// for the current context.
#[no_mangle]
pub extern "C" fn PyContextVar_New(
    name: *const c_char,
    default: *mut ShimObject,
) -> *mut ShimObject {
    if name.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    let name = read_c_string(name);
    if !default.is_null() {
        inc_ref(default);
    }
    new_object(ShimKind::ContextVar {
        name,
        default: default as usize,
        values: Arc::new(ContextVarValues::new()),
    })
}

/// `PyContextVar_Get`: writes a new reference to the value or default.
///
/// `default_value` overrides the variable only when it is non-NULL; otherwise
/// the variable's creation default (if any) is written. When neither exists,
/// `*value` is set to NULL and the call still succeeds, as in CPython.
#[no_mangle]
pub extern "C" fn PyContextVar_Get(
    variable: *mut ShimObject,
    default_value: *mut ShimObject,
    value: *mut *mut ShimObject,
) -> c_int {
    if value.is_null() {
        bad_internal_call();
        return -1;
    }
    // SAFETY: caller-supplied writable slot.
    unsafe { *value = ptr::null_mut() };
    let Some(ShimKind::ContextVar {
        default, values, ..
    }) = kind_ref(variable)
    else {
        set_error("TypeError: a context variable is required");
        return -1;
    };
    let stored_default = *default;
    let key = current_thread_key();
    let found = {
        let map = values.lock();
        map.get(&key).copied()
    };
    let result = match found {
        Some(found) => found,
        None if !default_value.is_null() => default_value as usize,
        None => stored_default,
    };
    if result != 0 {
        inc_ref(result as *mut ShimObject);
        // SAFETY: validated slot.
        unsafe { *value = result as *mut ShimObject };
    }
    0
}

/// `PyContextVar_Set`: stores `value` for the current thread and returns a
/// token describing the previous state.
#[no_mangle]
pub extern "C" fn PyContextVar_Set(
    variable: *mut ShimObject,
    value: *mut ShimObject,
) -> *mut ShimObject {
    if variable.is_null() || value.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    let Some(ShimKind::ContextVar { values, .. }) = kind_ref(variable) else {
        set_error("TypeError: a context variable is required");
        return ptr::null_mut();
    };
    let key = current_thread_key();
    inc_ref(value);
    let previous = {
        let mut map = values.lock();
        map.insert(key, value as usize).unwrap_or(0)
    };
    inc_ref(variable);
    new_object(ShimKind::ContextVarToken {
        var: variable as usize,
        previous,
    })
}

// -- C recursion accounting -------------------------------------------------

const RECURSION_LIMIT: usize = 1000;

thread_local! {
    static RECURSION_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// `Py_EnterRecursiveCall`.
#[no_mangle]
pub extern "C" fn Py_EnterRecursiveCall(_where: *const c_char) -> c_int {
    let depth = RECURSION_DEPTH.with(Cell::get);
    if depth >= RECURSION_LIMIT {
        set_error("RecursionError: maximum recursion depth exceeded");
        return -1;
    }
    RECURSION_DEPTH.with(|cell| cell.set(depth + 1));
    0
}

/// `Py_LeaveRecursiveCall`.
#[no_mangle]
pub extern "C" fn Py_LeaveRecursiveCall() {
    RECURSION_DEPTH.with(|cell| cell.set(cell.get().saturating_sub(1)));
}

/// `PyTraceMalloc_Track`: this host does not implement the tracemalloc
/// allocator hooks, so tracking reports "not tracing" (-1), which is the
/// documented signal for callers to stop reporting allocations.
#[no_mangle]
pub extern "C" fn PyTraceMalloc_Track(_domain: c_uint, _pointer: usize, _size: usize) -> c_int {
    -1
}

/// `PyTraceMalloc_Untrack`: see [`PyTraceMalloc_Track`].
#[no_mangle]
pub extern "C" fn PyTraceMalloc_Untrack(_domain: c_uint, _pointer: usize) -> c_int {
    -1
}

/// `ascii(obj)`: the repr with every non-ASCII character escaped.
#[no_mangle]
pub extern "C" fn hydro_capi_ascii_repr(object: *mut ShimObject) -> *mut ShimObject {
    if object.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    let repr = PyObject_Repr(object);
    if repr.is_null() {
        return ptr::null_mut();
    }
    let Some(ShimKind::Str { text, .. }) = kind_ref(repr) else {
        dec_ref(repr);
        set_error("SystemError: repr() returned a non-string object");
        return ptr::null_mut();
    };
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        let code = character as u32;
        if code < 0x80 {
            escaped.push(character);
        } else if code <= 0xFF {
            escaped.push_str(&format!("\\x{code:02x}"));
        } else if code <= 0xFFFF {
            escaped.push_str(&format!("\\u{code:04x}"));
        } else {
            escaped.push_str(&format!("\\U{code:08x}"));
        }
    }
    dec_ref(repr);
    new_str(escaped)
}

/// `PyUnicode_Format`: the `%` operator on strings.
#[no_mangle]
pub extern "C" fn PyUnicode_Format(
    format: *mut ShimObject,
    arguments: *mut ShimObject,
) -> *mut ShimObject {
    let text = match unicode_text(format) {
        Ok(text) => text.to_string(),
        Err(_) => {
            set_error("TypeError: PyUnicode_Format: format must be a string");
            return ptr::null_mut();
        }
    };
    if arguments.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    let value = match object_to_value(arguments).and_then(crate::modules::extension::shim_to_value)
    {
        Ok(value) => value,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    match crate::core::format::percent_format(&text, value) {
        Ok(rendered) => new_str(rendered),
        Err(error) => {
            if error.tag == crate::exc::VALUE_ERROR {
                set_error(format!("ValueError: {}", error.message));
            } else {
                set_error(format!("TypeError: {}", error.message));
            }
            ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// Link anchor
// ---------------------------------------------------------------------------

extern "C" {
    /// Defined in `parse.c`; declared here so [`anchor`] can reference it.
    fn PyArg_ParseTuple(args: *mut c_void, format: *const c_char, ...) -> c_int;

    /// Defined in `parse.c`; see [`PyArg_ParseTuple`].
    fn PyArg_ParseTupleAndKeywords(
        args: *mut c_void,
        keywords: *mut c_void,
        format: *const c_char,
        names: *const *const c_char,
        ...
    ) -> c_int;

    /// Defined in `parse.c`; formats its arguments and hands the text to
    /// [`hydro_capi_set_exception_message`].
    fn PyErr_Format(exception: *mut c_void, format: *const c_char, ...) -> *mut c_void;

    /// Defined in `parse.c`; the variadic form of [`PyObject_Call`].
    fn PyObject_CallFunctionObjArgs(callable: *mut c_void, ...) -> *mut c_void;

    /// Defined in `parse.c`; printf-style formatting into a Unicode writer.
    fn PyUnicodeWriter_Format(writer: *mut c_void, format: *const c_char, ...) -> c_int;

    /// Defined in `parse.c`; the variadic builder behind `Py_BuildValue`.
    fn Py_BuildValue(format: *const c_char, ...) -> *mut c_void;

    /// Defined in `parse.c`; the `va_list` form of the keyword parser.
    fn PyArg_VaParseTupleAndKeywords(
        args: *mut c_void,
        keywords: *mut c_void,
        format: *const c_char,
        names: *const *const c_char,
        arguments: *mut c_void,
    ) -> c_int;

    /// Defined in `parse.c`; tuple unpacking without a format string.
    fn PyArg_UnpackTuple(
        args: *mut c_void,
        name: *const c_char,
        minimum: isize,
        maximum: isize,
        ...
    ) -> c_int;

    /// Defined in `parse.c`; builds a tuple from variadic arguments.
    fn PyTuple_Pack(count: isize, ...) -> *mut c_void;

    /// Defined in `parse.c`; builds arguments with `Py_BuildValue`.
    fn PyObject_CallFunction(callable: *mut c_void, format: *const c_char, ...) -> *mut c_void;

    /// Defined in `parse.c`; bound attribute call with built arguments.
    fn PyObject_CallMethod(
        object: *mut c_void,
        name: *const c_char,
        format: *const c_char,
        ...
    ) -> *mut c_void;

    /// Defined in `parse.c`; CPython's Unicode format mini-language.
    fn PyUnicode_FromFormat(format: *const c_char, ...) -> *mut c_void;

    /// Defined in `parse.c`; the `va_list` form of `PyUnicode_FromFormat`.
    fn PyUnicode_FromFormatV(format: *const c_char, arguments: *mut c_void) -> *mut c_void;

    /// Defined in `parse.c`; formats and emits a warning.
    fn PyErr_WarnFormat(
        category: *mut c_void,
        stack_level: isize,
        format: *const c_char,
        ...
    ) -> c_int;

    /// Defined in `parse.c`; the C99 snprintf wrapper.
    fn PyOS_snprintf(buffer: *mut c_char, size: usize, format: *const c_char, ...) -> c_int;

    /// Defined in `parse.c`; `strtod` with CPython's error reporting.
    fn PyOS_string_to_double(
        text: *const c_char,
        end: *mut *mut c_char,
        overflow: *mut c_void,
    ) -> f64;

    /// Defined in `parse.c`; `strtol` wrapper.
    fn PyOS_strtol(text: *const c_char, end: *mut *mut c_char, base: c_int) -> c_long;

    /// Defined in `parse.c`; `strtoul` wrapper.
    fn PyOS_strtoul(text: *const c_char, end: *mut *mut c_char, base: c_int) -> c_ulong;
}

/// References every exported entry point so the linker keeps the definitions.
pub(crate) fn anchor() {
    types::ensure_builtin_types();
    initialize_exception_types();
    anchor_exception_types();
    anchor_constant_objects();
    // Builtin type data symbols: staticlib linkers keep a definition only when
    // something references it, so take every address here.
    let type_symbols: [*const TypeObject; 41] = {
        [
            ptr::addr_of!(types::TYPE_TYPE),
            ptr::addr_of!(types::OBJECT_TYPE),
            ptr::addr_of!(types::NONE_TYPE),
            ptr::addr_of!(types::BOOL_TYPE),
            ptr::addr_of!(types::INT_TYPE),
            ptr::addr_of!(types::FLOAT_TYPE),
            ptr::addr_of!(types::COMPLEX_TYPE),
            ptr::addr_of!(types::STR_TYPE),
            ptr::addr_of!(types::BYTES_TYPE),
            ptr::addr_of!(types::BYTEARRAY_TYPE),
            ptr::addr_of!(types::LIST_TYPE),
            ptr::addr_of!(types::TUPLE_TYPE),
            ptr::addr_of!(types::DICT_TYPE),
            ptr::addr_of!(types::SET_TYPE),
            ptr::addr_of!(types::FROZENSET_TYPE),
            ptr::addr_of!(types::SLICE_TYPE),
            ptr::addr_of!(types::MEMORYVIEW_TYPE),
            ptr::addr_of!(types::RANGE_TYPE),
            ptr::addr_of!(types::CAPSULE_TYPE),
            ptr::addr_of!(types::MODULE_TYPE),
            ptr::addr_of!(types::CFUNCTION_TYPE),
            ptr::addr_of!(types::ERROR_TYPE),
            ptr::addr_of!(types::NOT_IMPLEMENTED_TYPE),
            ptr::addr_of!(types::ELLIPSIS_TYPE),
            ptr::addr_of!(types::FUNCTION_TYPE),
            ptr::addr_of!(types::METHOD_TYPE),
            ptr::addr_of!(types::PROPERTY_TYPE),
            ptr::addr_of!(types::GETSET_DESCR_TYPE),
            ptr::addr_of!(types::MEMBER_DESCR_TYPE),
            ptr::addr_of!(types::METHOD_DESCR_TYPE),
            ptr::addr_of!(types::WRAPPER_DESCR_TYPE),
            ptr::addr_of!(types::CLASSMETHOD_DESCR_TYPE),
            ptr::addr_of!(types::STATICMETHOD_TYPE),
            ptr::addr_of!(types::CLASSMETHOD_TYPE),
            ptr::addr_of!(types::DICT_PROXY_TYPE),
            ptr::addr_of!(types::TRACEBACK_TYPE),
            ptr::addr_of!(types::OBJECT_ALIAS_TYPE),
            ptr::addr_of!(types::GENERIC_ALIAS_TYPE),
            ptr::addr_of!(types::SEQITER_TYPE),
            ptr::addr_of!(types::CONTEXT_VAR_TYPE),
            ptr::addr_of!(types::CONTEXT_TOKEN_TYPE),
        ]
    };
    let data_symbols: [*const u8; 6] = {
        [
            ptr::addr_of!(Py_Version) as *const u8,
            ptr::addr_of!(_Py_ascii_whitespace) as *const u8,
            ptr::addr_of!(_PyByteArray_empty_string) as *const u8,
            ptr::addr_of!(Py_None) as *const u8,
            ptr::addr_of!(Py_True) as *const u8,
            ptr::addr_of!(Py_False) as *const u8,
        ]
    };
    let _ = std::hint::black_box(type_symbols);
    let _ = std::hint::black_box(data_symbols);
    let symbols: &[*const ()] = &[
        PyModule_Create2 as *const (),
        PyModuleDef_Init as *const (),
        PyModule_AddStringConstant as *const (),
        PyModule_AddObject as *const (),
        PyLong_FromLong as *const (),
        PyLong_AsLong as *const (),
        PyUnicode_FromString as *const (),
        PyUnicode_AsUTF8 as *const (),
        PyFloat_FromDouble as *const (),
        PyFloat_AsDouble as *const (),
        PyBytes_FromStringAndSize as *const (),
        PyBytes_AsString as *const (),
        PyBytes_Size as *const (),
        PyCapsule_New as *const (),
        PyCapsule_GetPointer as *const (),
        PyCapsule_GetName as *const (),
        PyCapsule_IsValid as *const (),
        PyCapsule_SetPointer as *const (),
        PyObject_CheckBuffer as *const (),
        PyObject_GetBuffer as *const (),
        PyBuffer_Release as *const (),
        PyBuffer_FillInfo as *const (),
        PyList_New as *const (),
        PyList_Size as *const (),
        PyList_GetItem as *const (),
        PyList_SetItem as *const (),
        PyList_Append as *const (),
        PyTuple_New as *const (),
        PyTuple_Size as *const (),
        PyTuple_GetItem as *const (),
        PyTuple_SetItem as *const (),
        PyDict_New as *const (),
        PyDict_SetItem as *const (),
        PyDict_SetItemString as *const (),
        PyDict_GetItem as *const (),
        PyDict_GetItemString as *const (),
        Py_IncRef as *const (),
        Py_DecRef as *const (),
        _Py_IncRef as *const (),
        _Py_DecRef as *const (),
        Py_TYPE as *const (),
        Py_REFCNT as *const (),
        PyErr_Occurred as *const (),
        PyErr_SetString as *const (),
        PyErr_SetNone as *const (),
        PyErr_SetObject as *const (),
        PyErr_NoMemory as *const (),
        PyErr_BadArgument as *const (),
        PyErr_BadInternalCall as *const (),
        PyErr_SetFromErrno as *const (),
        PyErr_Clear as *const (),
        Py_GetConstant as *const (),
        Py_GetConstantBorrowed as *const (),
        Py_Is as *const (),
        Py_IsNone as *const (),
        Py_IsTrue as *const (),
        Py_IsFalse as *const (),
        PyType_FromSpec as *const (),
        PyType_FromSpecWithBases as *const (),
        PyType_GetName as *const (),
        PyType_GetQualName as *const (),
        PyType_GetFlags as *const (),
        PyType_GetSlot as *const (),
        PyType_Ready as *const (),
        PyType_GenericAlloc as *const (),
        PyType_GenericNew as *const (),
        PyObject_Init as *const (),
        PyObject_InitVar as *const (),
        PyObject_New as *const (),
        PyObject_NewVar as *const (),
        _PyObject_New as *const (),
        _PyObject_NewVar as *const (),
        PyObject_GC_Track as *const (),
        PyObject_GC_UnTrack as *const (),
        PyObject_GC_IsTracked as *const (),
        PyObject_GC_Del as *const (),
        PyGC_Collect as *const (),
        PyObject_IS_GC as *const (),
        _PyObject_GC_New as *const (),
        _PyObject_GC_NewVar as *const (),
        PyObject_Vectorcall as *const (),
        PyObject_VectorcallDict as *const (),
        PyObject_VectorcallMethod as *const (),
        PyVectorcall_Call as *const (),
        PyVectorcall_NARGS as *const (),
        PyObject_Call as *const (),
        PyObject_CallObject as *const (),
        PyCFunction_New as *const (),
        PyCFunction_NewEx as *const (),
        PyCFunction_GetFunction as *const (),
        PyCFunction_GetSelf as *const (),
        PyCFunction_GetFlags as *const (),
        PyCFunction_GetModule as *const (),
        PyObject_GetAttrString as *const (),
        PyObject_GetAttr as *const (),
        PyObject_SetAttrString as *const (),
        PyObject_SetAttr as *const (),
        PyObject_Repr as *const (),
        PyObject_Str as *const (),
        PyObject_Hash as *const (),
        PyGILState_Ensure as *const (),
        PyGILState_Release as *const (),
        PyGILState_GetThisThreadState as *const (),
        PyGILState_Check as *const (),
        PyEval_SaveThread as *const (),
        PyEval_RestoreThread as *const (),
        PyThreadState_Get as *const (),
        PyThreadState_GetUnchecked as *const (),
        hydro_capi_vectorcall as *const (),
        PyArg_ParseTuple as *const (),
        PyArg_ParseTupleAndKeywords as *const (),
        PyErr_Format as *const (),
        PyObject_CallFunctionObjArgs as *const (),
        hydro_capi_parse_i as *const (),
        hydro_capi_parse_l as *const (),
        hydro_capi_parse_n as *const (),
        hydro_capi_parse_d as *const (),
        hydro_capi_parse_s as *const (),
        hydro_capi_parse_o as *const (),
        hydro_capi_parse_p as *const (),
        hydro_capi_arg_count as *const (),
        hydro_capi_arg as *const (),
        hydro_capi_kw_is_dict as *const (),
        hydro_capi_kw_count as *const (),
        hydro_capi_kw_key as *const (),
        hydro_capi_kw_value as *const (),
        hydro_capi_set_error as *const (),
        hydro_capi_set_type_error as *const (),
        hydro_capi_set_exception_message as *const (),
        // Module / import / evaluation / allocator surface.
        PyModule_GetName as *const (),
        PyModule_GetNameObject as *const (),
        PyModule_GetFilename as *const (),
        PyModule_GetFilenameObject as *const (),
        PyModule_GetDef as *const (),
        PyModule_GetState as *const (),
        PyModule_GetDict as *const (),
        PyModule_NewObject as *const (),
        PyModule_AddIntConstant as *const (),
        PyModule_AddFunctions as *const (),
        PyModule_ExecDef as *const (),
        PyModule_FromDefAndSpec2 as *const (),
        PyState_FindModule as *const (),
        PyImport_ImportModule as *const (),
        PyImport_Import as *const (),
        PyImport_ImportModuleLevel as *const (),
        PyImport_ImportModuleLevelObject as *const (),
        PyImport_GetModule as *const (),
        PyImport_AddModule as *const (),
        PyImport_AddModuleRef as *const (),
        PyImport_GetModuleDict as *const (),
        PyEval_GetBuiltins as *const (),
        PyEval_GetGlobals as *const (),
        PyEval_GetLocals as *const (),
        PySys_GetObject as *const (),
        PySys_SetObject as *const (),
        PyRun_SimpleString as *const (),
        PyRun_SimpleStringFlags as *const (),
        PyRun_String as *const (),
        PyRun_StringFlags as *const (),
        PyMem_Malloc as *const (),
        PyMem_Free as *const (),
        PyMem_Realloc as *const (),
        PyMem_Calloc as *const (),
        PyObject_Malloc as *const (),
        PyObject_Free as *const (),
        PyObject_Realloc as *const (),
        PyObject_Calloc as *const (),
        PyMem_RawMalloc as *const (),
        PyMem_RawFree as *const (),
        PyMem_RawRealloc as *const (),
        PyMem_RawCalloc as *const (),
        PyUnicodeWriter_Create as *const (),
        PyUnicodeWriter_Discard as *const (),
        PyUnicodeWriter_Finish as *const (),
        PyUnicodeWriter_WriteChar as *const (),
        PyUnicodeWriter_WriteUTF8 as *const (),
        PyUnicodeWriter_WriteASCII as *const (),
        PyUnicodeWriter_WriteWideChar as *const (),
        PyUnicodeWriter_WriteUCS4 as *const (),
        PyUnicodeWriter_WriteStr as *const (),
        PyUnicodeWriter_WriteRepr as *const (),
        PyUnicodeWriter_WriteSubstring as *const (),
        PyUnicodeWriter_DecodeUTF8Stateful as *const (),
        PyUnicodeWriter_Format as *const (),
        PyLongWriter_Create as *const (),
        PyLongWriter_Finish as *const (),
        PyLongWriter_Discard as *const (),
        PyMutex_Lock as *const (),
        PyMutex_Unlock as *const (),
        PyMutex_IsLocked as *const (),
        PyType_GetModuleName as *const (),
        PyType_GetModule as *const (),
        PyType_GetModuleState as *const (),
        PyType_FromModuleAndSpec as *const (),
        PyType_FromMetaclass as *const (),
        PyObject_GenericGetAttr as *const (),
        PyObject_GenericSetAttr as *const (),
        // Slot-driven protocol surface (defined in `type.rs`).
        types::PyNumber_Add as *const (),
        types::PyNumber_Subtract as *const (),
        types::PyNumber_Multiply as *const (),
        types::PyNumber_Remainder as *const (),
        types::PyNumber_Divmod as *const (),
        types::PyNumber_Power as *const (),
        types::PyNumber_Negative as *const (),
        types::PyNumber_Positive as *const (),
        types::PyNumber_Absolute as *const (),
        types::PyNumber_Invert as *const (),
        types::PyNumber_Lshift as *const (),
        types::PyNumber_Rshift as *const (),
        types::PyNumber_And as *const (),
        types::PyNumber_Xor as *const (),
        types::PyNumber_Or as *const (),
        types::PyNumber_FloorDivide as *const (),
        types::PyNumber_TrueDivide as *const (),
        types::PyNumber_MatrixMultiply as *const (),
        types::PyNumber_Int as *const (),
        types::PyNumber_Long as *const (),
        types::PyNumber_Float as *const (),
        types::PyNumber_Index as *const (),
        types::PyNumber_InPlaceAdd as *const (),
        types::PyNumber_InPlaceSubtract as *const (),
        types::PyNumber_InPlaceMultiply as *const (),
        types::PyNumber_InPlaceRemainder as *const (),
        types::PyNumber_InPlacePower as *const (),
        types::PyNumber_InPlaceLshift as *const (),
        types::PyNumber_InPlaceRshift as *const (),
        types::PyNumber_InPlaceAnd as *const (),
        types::PyNumber_InPlaceXor as *const (),
        types::PyNumber_InPlaceOr as *const (),
        types::PyNumber_InPlaceFloorDivide as *const (),
        types::PyNumber_InPlaceTrueDivide as *const (),
        types::PyNumber_InPlaceMatrixMultiply as *const (),
        types::PyNumber_AsSsize_t as *const (),
        types::PyNumber_Check as *const (),
        types::PyIndex_Check as *const (),
        types::PySequence_Length as *const (),
        types::PySequence_Size as *const (),
        types::PySequence_Check as *const (),
        types::PySequence_GetItem as *const (),
        types::PySequence_SetItem as *const (),
        types::PySequence_DelItem as *const (),
        types::PySequence_Concat as *const (),
        types::PySequence_Repeat as *const (),
        types::PySequence_InPlaceConcat as *const (),
        types::PySequence_InPlaceRepeat as *const (),
        types::PySequence_Contains as *const (),
        types::PyMapping_Length as *const (),
        types::PyMapping_Size as *const (),
        types::PyMapping_GetItemString as *const (),
        types::PyMapping_SetItemString as *const (),
        types::PyObject_Length as *const (),
        types::PyObject_Size as *const (),
        types::PyObject_GetItem as *const (),
        types::PyObject_SetItem as *const (),
        types::PyObject_DelItem as *const (),
        types::PyObject_GetIter as *const (),
        types::PyObject_SelfIter as *const (),
        types::PyObject_Next as *const (),
        types::PyIter_Check as *const (),
        types::PyIter_Next as *const (),
        types::PyObject_RichCompare as *const (),
        types::PyObject_RichCompareBool as *const (),
        types::PyObject_IsTrue as *const (),
        types::PyObject_Not as *const (),
        types::PyCallable_Check as *const (),
        types::PyType_Freeze as *const (),
        types::PyType_IsSubtype as *const (),
        types::PyType_Modified as *const (),
        types::PyUnstable_Object_IsUniquelyReferenced as *const (),
        // Remaining argument, object and protocol entry points.
        PyArg_UnpackTuple as *const (),
        PyArg_VaParseTupleAndKeywords as *const (),
        Py_BuildValue as *const (),
        PyTuple_Pack as *const (),
        PyObject_CallFunction as *const (),
        PyObject_CallMethod as *const (),
        PyUnicode_FromFormat as *const (),
        PyUnicode_FromFormatV as *const (),
        PyErr_WarnFormat as *const (),
        PyOS_snprintf as *const (),
        PyOS_string_to_double as *const (),
        PyOS_strtol as *const (),
        PyOS_strtoul as *const (),
        PyBool_FromLong as *const (),
        PyBytes_AsStringAndSize as *const (),
        PyBytes_FromString as *const (),
        PyCapsule_GetContext as *const (),
        PyCapsule_SetContext as *const (),
        PyCapsule_SetName as *const (),
        PyComplex_AsCComplex as *const (),
        PyComplex_FromCComplex as *const (),
        PyContextVar_Get as *const (),
        PyContextVar_New as *const (),
        PyContextVar_Set as *const (),
        PyDictProxy_New as *const (),
        PyDict_ContainsString as *const (),
        PyDict_DelItemString as *const (),
        PyDict_GetItemStringRef as *const (),
        PyDict_GetItemWithError as *const (),
        PyDict_Merge as *const (),
        PyDict_SetDefaultRef as *const (),
        PyDict_Size as *const (),
        PyDict_Values as *const (),
        PyErr_CheckSignals as *const (),
        PyErr_ExceptionMatches as *const (),
        PyErr_Fetch as *const (),
        PyErr_NormalizeException as *const (),
        PyErr_Print as *const (),
        PyErr_Restore as *const (),
        PyErr_WriteUnraisable as *const (),
        PyException_SetContext as *const (),
        PyFloat_FromString as *const (),
        PyInterpreterState_Main as *const (),
        PyLong_AsLongAndOverflow as *const (),
        PyLong_AsLongLong as *const (),
        PyLong_AsLongLongAndOverflow as *const (),
        PyLong_AsSsize_t as *const (),
        PyLong_AsUnsignedLong as *const (),
        PyLong_AsVoidPtr as *const (),
        PyLong_FromDouble as *const (),
        PyLong_FromLongLong as *const (),
        PyLong_FromVoidPtr as *const (),
        PyLong_FromUnicodeObject as *const (),
        PyLong_FromUnsignedLong as *const (),
        PyLong_IsZero as *const (),
        PyMemoryView_FromObject as *const (),
        PyMethod_New as *const (),
        PyObject_AsFileDescriptor as *const (),
        PyObject_Bytes as *const (),
        PyObject_ClearWeakRefs as *const (),
        PyObject_Format as *const (),
        PyObject_GetOptionalAttr as *const (),
        PyObject_HasAttrString as *const (),
        PyObject_Print as *const (),
        PySeqIter_New as *const (),
        PySequence_Fast as *const (),
        PySequence_Tuple as *const (),
        PySlice_AdjustIndices as *const (),
        PySlice_New as *const (),
        PySlice_Unpack as *const (),
        PyTraceMalloc_Track as *const (),
        PyTraceMalloc_Untrack as *const (),
        PyTuple_GetSlice as *const (),
        PyUnicode_AsASCIIString as *const (),
        PyUnicode_AsLatin1String as *const (),
        PyUnicode_AsUCS4 as *const (),
        PyUnicode_AsUCS4Copy as *const (),
        PyUnicode_AsUTF8String as *const (),
        PyUnicode_Compare as *const (),
        PyUnicode_CompareWithASCIIString as *const (),
        PyUnicode_Concat as *const (),
        PyUnicode_Contains as *const (),
        PyUnicode_Format as *const (),
        PyUnicode_FromEncodedObject as *const (),
        PyUnicode_FromKindAndData as *const (),
        PyUnicode_GetLength as *const (),
        PyUnicode_InternFromString as *const (),
        PyUnicode_Replace as *const (),
        PyUnicode_Substring as *const (),
        PyUnicode_Tailmatch as *const (),
        PyUnstable_Object_IsUniqueReferencedTemporary as *const (),
        Py_EnterRecursiveCall as *const (),
        Py_LeaveRecursiveCall as *const (),
        Py_GenericAlias as *const (),
        _PyErr_BadInternalCall as *const (),
        _PyUnicode_IsAlpha as *const (),
        _PyUnicode_IsDecimalDigit as *const (),
        _PyUnicode_IsDigit as *const (),
        _PyUnicode_IsLowercase as *const (),
        _PyUnicode_IsNumeric as *const (),
        _PyUnicode_IsTitlecase as *const (),
        _PyUnicode_IsUppercase as *const (),
        _PyUnicode_IsWhitespace as *const (),
        _Py_HashDouble as *const (),
        hydro_capi_ascii_repr as *const (),
        hydro_capi_is_tuple as *const (),
        hydro_capi_object_type_name as *const (),
        hydro_capi_tuple_pack as *const (),
    ];
    let _ = std::hint::black_box(symbols);
    // Symbols implemented in the focused API modules. Keep them referenced so
    // the staticlib linker preserves every definition.
    let api_symbols: &[*const ()] = &[
        container_api::PyDict_Pop as *const (),
        container_api::PyDict_SetDefault as *const (),
        container_api::_PyDict_GetItem_KnownHash as *const (),
        container_api::_PyDict_NewPresized as *const (),
        container_api::PyList_Insert as *const (),
        container_api::PyList_SetSlice as *const (),
        container_api::PyList_Sort as *const (),
        container_api::PySequence_List as *const (),
        container_api::PySet_Discard as *const (),
        container_api::PySet_Pop as *const (),
        descriptor_api::PyArg_ValidateKeywordArguments as *const (),
        descriptor_api::PyClassMethod_New as *const (),
        descriptor_api::PyDescr_NewClassMethod as *const (),
        descriptor_api::PyStaticMethod_New as *const (),
        descriptor_api::PyErr_NewException as *const (),
        numeric_api::PyLong_AsDouble as *const (),
        numeric_api::PyLong_AsInt as *const (),
        numeric_api::PyLong_AsUnsignedLongLongMask as *const (),
        numeric_api::PyLong_FromSize_t as *const (),
        numeric_api::PyLong_FromString as *const (),
        numeric_api::_PyLong_Copy as *const (),
        numeric_api::PyOS_double_to_string as *const (),
        numeric_api::PyMemoryView_FromMemory as *const (),
        object_api::PyObject_CallFinalizerFromDealloc as *const (),
        object_api::PyObject_CallMethodObjArgs as *const (),
        object_api::PyObject_ClearManagedDict as *const (),
        object_api::PyObject_GC_IsFinalized as *const (),
        object_api::PyObject_HasAttr as *const (),
        object_api::PyObject_VisitManagedDict as *const (),
        object_api::_PyObject_GetDictPtr as *const (),
        object_api::_PyType_Lookup as *const (),
        object_api::PyGC_Disable as *const (),
        object_api::PyGC_Enable as *const (),
        object_api::PyWeakref_GetRef as *const (),
        object_api::PyWeakref_NewRef as *const (),
        object_api::PyThread_allocate_lock as *const (),
        object_api::PyThread_free_lock as *const (),
        object_api::PyThreadState_GetFrame as *const (),
        object_api::PyTraceBack_Here as *const (),
        object_api::PyFrame_New as *const (),
        object_api::PyCode_NewEmpty as *const (),
        object_api::PyUnstable_Code_NewWithPosOnlyArgs as *const (),
        object_api::PyUnstable_Object_EnableDeferredRefcount as *const (),
        object_api::_Py_FatalErrorFunc as *const (),
        unicode_api::PyUnicode_CopyCharacters as *const (),
        unicode_api::PyUnicode_DecodeLocale as *const (),
        unicode_api::PyUnicode_DecodeUTF8 as *const (),
        unicode_api::PyUnicode_EncodeLocale as *const (),
        unicode_api::PyUnicode_Equal as *const (),
        unicode_api::PyUnicode_Find as *const (),
        unicode_api::PyUnicode_FindChar as *const (),
        unicode_api::PyUnicode_FromOrdinal as *const (),
        unicode_api::PyUnicode_FromWideChar as *const (),
        unicode_api::PyUnicode_Join as *const (),
        unicode_api::PyUnicode_Resize as *const (),
        unicode_api::PyUnicode_Split as *const (),
        unicode_api::PyBytes_Join as *const (),
    ];
    let _ = std::hint::black_box(api_symbols);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::raw::{c_char, c_int, c_long, c_uint, c_ulong, c_void};

    const SEQ: std::sync::atomic::Ordering = std::sync::atomic::Ordering::SeqCst;

    // -- helpers ----------------------------------------------------------

    fn make_type(
        name: &std::ffi::CStr,
        basicsize: c_int,
        itemsize: c_int,
        flags: c_uint,
        slots: &mut [PyTypeSlotMirror],
    ) -> *mut ShimObject {
        let mut spec = PyTypeSpecMirror {
            name: name.as_ptr(),
            basicsize,
            itemsize,
            flags,
            slots: slots.as_mut_ptr(),
        };
        let type_object = PyType_FromSpec(&mut spec);
        assert!(
            !type_object.is_null(),
            "type creation failed: {:?}",
            take_error()
        );
        type_object
    }

    fn empty_slots() -> [PyTypeSlotMirror; 1] {
        [PyTypeSlotMirror {
            slot: 0,
            pfunc: std::ptr::null_mut(),
        }]
    }

    /// Names the payload variant so assertion messages stay readable without
    /// requiring `Debug` on `ShimKind`'s non-`Debug` payloads.
    fn kind_variant(kind: Option<&ShimKind>) -> &'static str {
        match kind {
            None => "<detached>",
            Some(ShimKind::None) => "None",
            Some(ShimKind::Bool(_)) => "Bool",
            Some(ShimKind::Int(_)) => "Int",
            Some(ShimKind::Float(_)) => "Float",
            Some(ShimKind::Str { .. }) => "Str",
            Some(ShimKind::Bytes { .. }) => "Bytes",
            Some(ShimKind::ByteArray { .. }) => "ByteArray",
            Some(ShimKind::Set(_)) => "Set",
            Some(ShimKind::FrozenSet(_)) => "FrozenSet",
            Some(ShimKind::Complex { .. }) => "Complex",
            Some(ShimKind::Exception { .. }) => "Exception",
            Some(ShimKind::RuntimeFunction { .. }) => "RuntimeFunction",
            Some(ShimKind::List(_)) => "List",
            Some(ShimKind::Tuple(_)) => "Tuple",
            Some(ShimKind::Dict(_)) => "Dict",
            Some(ShimKind::Capsule { .. }) => "Capsule",
            Some(ShimKind::Method { .. }) => "Method",
            Some(ShimKind::Slice { .. }) => "Slice",
            Some(ShimKind::DictProxy { .. }) => "DictProxy",
            Some(ShimKind::GenericAlias { .. }) => "GenericAlias",
            Some(ShimKind::MemoryView { .. }) => "MemoryView",
            Some(ShimKind::SeqIter { .. }) => "SeqIter",
            Some(ShimKind::ContextVar { .. }) => "ContextVar",
            Some(ShimKind::ContextVarToken { .. }) => "ContextVarToken",
            Some(ShimKind::CFunction { .. }) => "CFunction",
            Some(ShimKind::NotImplemented) => "NotImplemented",
            Some(ShimKind::Ellipsis) => "Ellipsis",
            Some(ShimKind::Module(_)) => "Module",
            Some(ShimKind::ModuleDef { .. }) => "ModuleDef",
            Some(ShimKind::Hydro(_)) => "Hydro",
            Some(ShimKind::Error) => "Error",
        }
    }

    // -- object header layout --------------------------------------------

    #[test]
    fn handle_layout_matches_cpython() {
        assert_eq!(std::mem::size_of::<PyObjectHead>(), 16);
        assert_eq!(std::mem::offset_of!(PyObjectHead, ob_refcnt), 0);
        assert_eq!(std::mem::offset_of!(PyObjectHead, ob_type), 8);
        assert_eq!(std::mem::size_of::<PyVarObjectHead>(), 24);
        assert_eq!(std::mem::offset_of!(PyVarObjectHead, ob_size), 16);
        assert_eq!(std::mem::offset_of!(ShimObject, head), 0);
        assert_eq!(std::mem::offset_of!(ShimObject, ob_size), 16);
        assert_eq!(std::mem::offset_of!(ShimObject, aux), 24);
        assert_eq!(std::mem::offset_of!(ShimObject, kind), 32);

        assert_eq!(std::mem::size_of::<types::TypeObject>(), 416);
        assert_eq!(std::mem::offset_of!(types::TypeObject, ob_refcnt), 0);
        assert_eq!(std::mem::offset_of!(types::TypeObject, ob_type), 8);
        assert_eq!(std::mem::offset_of!(types::TypeObject, ob_size), 16);
        assert_eq!(std::mem::offset_of!(types::TypeObject, tp_name), 24);
        assert_eq!(std::mem::offset_of!(types::TypeObject, tp_flags), 168);
        assert_eq!(std::mem::offset_of!(types::TypeObject, tp_doc), 176);
        assert_eq!(std::mem::offset_of!(types::TypeObject, tp_methods), 232);
        assert_eq!(std::mem::offset_of!(types::TypeObject, tp_base), 256);
        assert_eq!(std::mem::offset_of!(types::TypeObject, tp_init), 296);
        assert_eq!(std::mem::offset_of!(types::TypeObject, tp_alloc), 304);
        assert_eq!(std::mem::offset_of!(types::TypeObject, tp_new), 312);
        assert_eq!(std::mem::offset_of!(types::TypeObject, tp_free), 320);
        assert_eq!(std::mem::offset_of!(types::TypeObject, tp_vectorcall), 400);

        // The stable-ABI `Py_buffer` mirror (checked again from the tests).
        assert_eq!(std::mem::size_of::<Py_buffer>(), 80);
        assert_eq!(std::mem::offset_of!(Py_buffer, obj), 8);
        assert_eq!(std::mem::offset_of!(Py_buffer, itemsize), 24);
        assert_eq!(std::mem::offset_of!(Py_buffer, readonly), 32);
        assert_eq!(std::mem::offset_of!(Py_buffer, format), 40);
        assert_eq!(std::mem::offset_of!(Py_buffer, shape), 48);
        assert_eq!(std::mem::offset_of!(Py_buffer, strides), 56);
        assert_eq!(std::mem::offset_of!(Py_buffer, suboffsets), 64);
    }

    /// CPython's inline macros read raw offsets. This test performs exactly
    /// those reads/writes so a layout regression fails here instead of in an
    /// extension.
    #[test]
    fn tuple_and_list_prefix_is_cpython_shaped() {
        anchor();
        let first = new_int(11);
        let second = new_int(22);
        let tuple = new_tuple(vec![first as usize, second as usize]);
        // SAFETY: `tuple` is a live handle; the offsets are the documented
        // CPython 3.14 ones.
        unsafe {
            let base = tuple as *mut u8;
            // `Py_SIZE`, `PyTuple_GET_SIZE`.
            assert_eq!(*(base.add(16) as *const isize), 2);
            // `PyTupleObject.ob_hash` starts at -1 in CPython 3.14.
            assert_eq!(*(base.add(24) as *const isize), -1);
            // `PyTuple_GET_ITEM`.
            assert_eq!(*(base.add(32) as *const usize), first as usize);
            assert_eq!(*(base.add(40) as *const usize), second as usize);
        }

        // `PyTuple_SET_ITEM` replacement is immediately visible, exactly like
        // an extension writing the inline slot. Release the displaced
        // reference like a caller that replaced a non-NULL slot.
        let third = new_int(33);
        unsafe {
            let base = tuple as *mut u8;
            *(base.add(32) as *mut usize) = third as usize;
        }
        dec_ref(first);
        assert_eq!(PyTuple_GetItem(tuple, 0), third);
        assert_eq!(PyTuple_Size(tuple), 2);
        dec_ref(tuple);

        let list = PyList_New(2);
        assert!(!list.is_null());
        let list_first = new_int(44);
        let list_second = new_int(55);
        // SAFETY: `list` is a live list handle.
        unsafe {
            let base = list as *mut u8;
            // `PyList_GET_SIZE`.
            assert_eq!(*(base.add(16) as *const isize), 2);
            // `PyList_GET_ITEM` reads `ob_item` at 24.
            let data = *(base.add(24) as *const *mut usize);
            assert!(!data.is_null());
            assert_eq!(*data, 0);
            // `PyList_SET_ITEM`.
            *data = list_first as usize;
        }
        assert_eq!(PyList_GetItem(list, 0), list_first);
        // Appending can move the `Vec`; the prefix must be republished before
        // control returns to the extension.
        assert_eq!(PyList_Append(list, list_second), 0);
        // SAFETY: as above.
        unsafe {
            let base = list as *mut u8;
            assert_eq!(*(base.add(16) as *const isize), 3);
            let data = *(base.add(24) as *const *mut usize);
            assert_eq!(*data.add(0), list_first as usize);
            assert_eq!(*data.add(2), list_second as usize);
        }
        dec_ref(list);
        // The list took its own reference in `PyList_Append`.
        dec_ref(list_second);
    }

    // -- refcounts and freeing -------------------------------------------

    static CAPSULE_DROPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static LIST_DROPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    extern "C" fn count_capsule_drop(_capsule: *mut ShimObject) {
        CAPSULE_DROPS.fetch_add(1, SEQ);
    }

    extern "C" fn count_list_capsule_drop(_capsule: *mut ShimObject) {
        LIST_DROPS.fetch_add(1, SEQ);
    }

    #[test]
    fn reference_counts_free_capsules_and_run_destructors() {
        anchor();
        static VALUE: i64 = 7;
        CAPSULE_DROPS.store(0, SEQ);
        let capsule = PyCapsule_New(
            &VALUE as *const i64 as *mut c_void,
            std::ptr::null(),
            count_capsule_drop as *const () as *mut c_void,
        );
        assert!(!capsule.is_null());
        assert_eq!(Py_REFCNT(capsule), 1);
        Py_IncRef(capsule);
        assert_eq!(Py_REFCNT(capsule), 2);
        Py_DecRef(capsule);
        assert_eq!(Py_REFCNT(capsule), 1);
        assert_eq!(CAPSULE_DROPS.load(SEQ), 0, "destructor waits for zero");
        Py_DecRef(capsule);
        assert_eq!(CAPSULE_DROPS.load(SEQ), 1);

        // A capsule that escapes to a Hydro value keeps a retained reference:
        // the Hydro side stores only the pointer, so the destructor must not
        // invalidate it.
        let escaped = PyCapsule_New(
            &VALUE as *const i64 as *mut c_void,
            std::ptr::null(),
            count_capsule_drop as *const () as *mut c_void,
        );
        match object_to_value(escaped) {
            Ok(ShimValue::Capsule { ptr, name: None }) => {
                assert_eq!(ptr, &VALUE as *const i64 as usize)
            }
            other => panic!("unexpected capsule conversion: {other:?}"),
        }
        Py_DecRef(escaped);
        assert_eq!(
            CAPSULE_DROPS.load(SEQ),
            1,
            "an escaping capsule must outlive the caller reference"
        );
        Py_DecRef(escaped);
        assert_eq!(CAPSULE_DROPS.load(SEQ), 2);

        // NULL is accepted and never frees anything.
        Py_IncRef(std::ptr::null_mut());
        Py_DecRef(std::ptr::null_mut());
        assert_eq!(Py_REFCNT(std::ptr::null_mut()), 0);
    }

    #[test]
    fn containers_release_their_children() {
        anchor();
        static VALUE: i64 = 7;
        LIST_DROPS.store(0, SEQ);
        let list = PyList_New(0);
        let capsule = PyCapsule_New(
            &VALUE as *const i64 as *mut c_void,
            std::ptr::null(),
            count_list_capsule_drop as *const () as *mut c_void,
        );
        assert_eq!(PyList_Append(list, capsule), 0);
        assert_eq!(Py_REFCNT(capsule), 2, "append takes a new reference");
        Py_DecRef(capsule);
        assert_eq!(LIST_DROPS.load(SEQ), 0);
        Py_DecRef(list);
        assert_eq!(LIST_DROPS.load(SEQ), 1, "list destruction drops the child");
    }

    #[test]
    fn small_ints_and_empty_singletons_are_cached() {
        anchor();
        let first = PyLong_FromLong(5);
        let second = PyLong_FromLong(5);
        assert_eq!(first, second, "cached small int identity");

        let big_first = PyLong_FromLong(1_000_000);
        let big_second = PyLong_FromLong(1_000_000);
        assert_ne!(big_first, big_second);
        assert_eq!(Py_REFCNT(big_first), 1);
        Py_DecRef(big_first);
        Py_DecRef(big_second);

        assert_eq!(PyLong_FromLong(0), Py_GetConstantBorrowed(PY_CONSTANT_ZERO));
        assert_eq!(PyLong_FromLong(1), Py_GetConstantBorrowed(PY_CONSTANT_ONE));
        assert_eq!(
            PyUnicode_FromString(c"".as_ptr()),
            Py_GetConstantBorrowed(PY_CONSTANT_EMPTY_STR)
        );
        assert_eq!(
            PyTuple_New(0),
            Py_GetConstantBorrowed(PY_CONSTANT_EMPTY_TUPLE)
        );
        let huge_refcnt: isize = 1 << 40;
        assert!(Py_REFCNT(Py_GetConstantBorrowed(PY_CONSTANT_NONE)) > huge_refcnt);

        // Immortal objects ignore refcount traffic.
        let none = Py_GetConstantBorrowed(PY_CONSTANT_NONE);
        let before = Py_REFCNT(none);
        Py_IncRef(none);
        Py_DecRef(none);
        assert_eq!(Py_REFCNT(none), before);
    }

    // -- type creation, layout, slots ------------------------------------

    extern "C" fn method_seven(_self: *mut ShimObject, _args: *mut ShimObject) -> *mut ShimObject {
        PyLong_FromLong(7)
    }

    static INIT_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    extern "C" fn initialise_instance(
        _self: *mut ShimObject,
        _args: *mut ShimObject,
        _kwds: *mut ShimObject,
    ) -> c_int {
        INIT_CALLS.fetch_add(1, SEQ);
        0
    }

    extern "C" fn construct_instance(
        type_object: *mut ShimObject,
        _args: *mut ShimObject,
        _kwds: *mut ShimObject,
    ) -> *mut ShimObject {
        PyType_GenericAlloc(type_object, 0)
    }

    #[test]
    fn type_spec_exposes_names_flags_and_slots() {
        anchor();
        let mut methods = [
            PyMethodDefMirror {
                ml_name: c"seven".as_ptr(),
                ml_meth: method_seven as *const () as *mut c_void,
                ml_flags: METH_NOARGS,
                ml_doc: std::ptr::null(),
            },
            PyMethodDefMirror {
                ml_name: std::ptr::null(),
                ml_meth: std::ptr::null_mut(),
                ml_flags: 0,
                ml_doc: std::ptr::null(),
            },
        ];
        let mut slots = [
            PyTypeSlotMirror {
                slot: 64,
                pfunc: methods.as_mut_ptr() as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 65,
                pfunc: construct_instance as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 60,
                pfunc: initialise_instance as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 56,
                pfunc: c"probe doc".as_ptr() as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: std::ptr::null_mut(),
            },
        ];
        let type_object = make_type(c"probe.Thing", 40, 0, 0, &mut slots);
        assert_eq!(Py_REFCNT(type_object), 1);

        match object_to_value(PyType_GetName(type_object)) {
            Ok(ShimValue::Str(name)) => assert_eq!(name, "Thing"),
            other => panic!("unexpected name: {other:?}"),
        }
        match object_to_value(PyType_GetQualName(type_object)) {
            Ok(ShimValue::Str(name)) => assert_eq!(name, "probe.Thing"),
            other => panic!("unexpected qualified name: {other:?}"),
        }
        let flags = PyType_GetFlags(type_object);
        assert_ne!(flags & PY_TPFLAGS_HEAPTYPE, 0);
        let ready_flag: c_ulong = 1 << 12;
        assert_ne!(flags & ready_flag, 0, "PyType_Ready ran");
        assert_eq!(PyType_Ready(type_object), 0, "idempotent");
        assert!(
            !PyType_GetSlot(type_object, 47).is_null(),
            "tp_alloc default"
        );
        assert_eq!(
            PyType_GetSlot(type_object, 60),
            initialise_instance as *const () as *mut c_void
        );
        assert_eq!(
            PyType_GetSlot(type_object, 64),
            methods.as_mut_ptr() as *mut c_void
        );
        let doc = PyType_GetSlot(type_object, 56) as *const c_char;
        assert!(!doc.is_null());
        assert_eq!(unsafe { std::ffi::CStr::from_ptr(doc) }, c"probe doc");
        assert!(PyType_GetSlot(type_object, 62).is_null(), "unset tp_iter");
        assert!(!error_pending(), "an unset slot must not set an error");

        clear_error();
        assert!(PyType_GetSlot(type_object, 0).is_null());
        let message = take_error().expect("invalid slot");
        assert!(message.contains("invalid slot"), "{message}");
        assert!(PyType_GetSlot(type_object, 99).is_null());
        assert!(take_error().unwrap().contains("invalid slot"));

        // Construction goes through tp_new + tp_init.
        INIT_CALLS.store(0, SEQ);
        let instance = PyObject_Call(type_object, std::ptr::null_mut(), std::ptr::null_mut());
        assert!(!instance.is_null(), "{:?}", take_error());
        assert_eq!(INIT_CALLS.load(SEQ), 1);
        assert_eq!(Py_TYPE(instance), type_object as *mut types::TypeObject);
        assert_eq!(Py_REFCNT(instance), 1);
        assert_eq!(Py_REFCNT(type_object), 2, "instance owns a type reference");
        Py_DecRef(instance);
        assert_eq!(Py_REFCNT(type_object), 1);
        Py_DecRef(type_object);
    }

    #[test]
    fn type_spec_rejects_unsupported_slots_and_flags() {
        anchor();
        let mut slots = [
            PyTypeSlotMirror {
                slot: 77, // Py_am_await (async suites are not implemented)
                pfunc: std::ptr::null_mut(),
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: std::ptr::null_mut(),
            },
        ];
        let mut spec = PyTypeSpecMirror {
            name: c"probe.Bad".as_ptr(),
            basicsize: 16,
            itemsize: 0,
            flags: 0,
            slots: slots.as_mut_ptr(),
        };
        clear_error();
        assert!(PyType_FromSpec(&mut spec).is_null());
        let message = take_error().expect("unsupported slot");
        assert!(message.contains("Py_am_await"), "{message}");
        assert!(message.starts_with("TypeError: "), "{message}");

        // Managed-dict layout is an interpreter-only flag.
        spec.flags = 1 << 4;
        assert!(PyType_FromSpec(&mut spec).is_null());
        let message = take_error().expect("unsupported flag");
        assert!(message.contains("flags"), "{message}");

        // An unknown slot identifier is rejected too.
        spec.flags = 0;
        let mut unknown_slots = [
            PyTypeSlotMirror {
                slot: 99,
                pfunc: std::ptr::null_mut(),
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: std::ptr::null_mut(),
            },
        ];
        spec.slots = unknown_slots.as_mut_ptr();
        assert!(PyType_FromSpec(&mut spec).is_null());
        assert!(take_error().unwrap().contains("invalid type slot 99"));

        // A NULL slot table is an error, not an empty type.
        spec.slots = std::ptr::null_mut();
        assert!(PyType_FromSpec(&mut spec).is_null());
        assert!(take_error().unwrap().contains("no slot table"));
        assert!(PyType_FromSpec(std::ptr::null_mut()).is_null());
        assert!(take_error().unwrap().contains("type spec is NULL"));
        assert!(!error_pending());
    }

    #[test]
    fn type_spec_bases_accept_object_and_reject_unsupported_bases() {
        anchor();
        let mut slots = empty_slots();
        let mut spec = PyTypeSpecMirror {
            name: c"probe.Based".as_ptr(),
            basicsize: 0,
            itemsize: 0,
            flags: 0,
            slots: slots.as_mut_ptr(),
        };
        let object_type = std::ptr::addr_of_mut!(types::OBJECT_TYPE) as *mut ShimObject;
        let direct = PyType_FromSpecWithBases(&mut spec, object_type);
        assert!(!direct.is_null(), "{:?}", take_error());
        Py_DecRef(direct);

        let empty = PyTuple_New(0);
        let wrapped = PyType_FromSpecWithBases(&mut spec, empty);
        assert!(!wrapped.is_null(), "{:?}", take_error());
        Py_DecRef(wrapped);
        Py_DecRef(empty);

        let number = PyLong_FromLong(1_000_000);
        let int_type = Py_TYPE(number);
        Py_DecRef(number);
        clear_error();
        assert!(PyType_FromSpecWithBases(&mut spec, int_type as *mut ShimObject).is_null());
        let message = take_error().expect("shim base");
        assert!(message.contains("base"), "{message}");

        // Multiple bases are accepted and linearised with C3.
        let pair = PyTuple_New(2);
        Py_IncRef(object_type);
        assert_eq!(PyTuple_SetItem(pair, 0, object_type), 0);
        Py_IncRef(object_type);
        assert_eq!(PyTuple_SetItem(pair, 1, object_type), 0);
        let multiple = PyType_FromSpecWithBases(&mut spec, pair);
        assert!(!multiple.is_null(), "{:?}", take_error());
        let mro = PyObject_GetAttrString(multiple, c"__mro__".as_ptr());
        assert!(!mro.is_null(), "{:?}", take_error());
        assert_eq!(PyTuple_Size(mro), 2, "duplicate base is merged once");
        Py_DecRef(mro);
        Py_DecRef(multiple);
        Py_DecRef(pair);
    }

    #[test]
    fn datetime_capi_types_are_subclassable_bases() {
        anchor();
        let tzinfo = std::ptr::addr_of_mut!(types::TZINFO_CAPI_TYPE) as *mut ShimObject;
        // pydantic-core's `TzInfo` is `#[pyclass(extends = PyTzInfo, frozen)]`:
        // PyO3 passes the base in a `Py_tp_base` slot and a layout of one
        // `PyObject` header plus its single `i32` field (24 bytes on x86_64).
        let mut slots = [
            PyTypeSlotMirror {
                slot: 48, // Py_tp_base
                pfunc: tzinfo as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: std::ptr::null_mut(),
            },
        ];
        let mut spec = PyTypeSpecMirror {
            name: c"probe.TzInfo".as_ptr(),
            basicsize: 24,
            itemsize: 0,
            flags: 0,
            slots: slots.as_mut_ptr(),
        };
        let subclass = PyType_FromSpec(&mut spec);
        assert!(!subclass.is_null(), "{:?}", take_error());
        assert_eq!(
            types::PyType_IsSubtype(subclass, tzinfo),
            1,
            "the heap type derives from datetime.tzinfo"
        );
        let instance = PyType_GenericAlloc(subclass, 0);
        assert!(!instance.is_null(), "{:?}", take_error());
        assert_eq!(Py_TYPE(instance), subclass as *mut types::TypeObject);
        assert!(
            object_type_check(instance, tzinfo),
            "PyTZInfo_Check accepts the subclass instance"
        );
        Py_DecRef(instance);
        Py_DecRef(subclass);

        // A Shim base that does not opt into subclassing stays rejected: the
        // relaxation is limited to types CPython declares subclassable.
        let mut unsupported_slots = [
            PyTypeSlotMirror {
                slot: 48,
                pfunc: std::ptr::addr_of_mut!(types::NONE_TYPE) as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: std::ptr::null_mut(),
            },
        ];
        let mut unsupported = PyTypeSpecMirror {
            name: c"probe.BadTzInfo".as_ptr(),
            basicsize: 24,
            itemsize: 0,
            flags: 0,
            slots: unsupported_slots.as_mut_ptr(),
        };
        clear_error();
        assert!(PyType_FromSpec(&mut unsupported).is_null());
        let message = take_error().expect("shim base without Py_TPFLAGS_BASETYPE");
        assert!(message.contains("cannot use"), "{message}");
    }

    #[test]
    fn generic_allocation_initialises_cpython_headers() {
        anchor();
        let mut slots = empty_slots();
        let type_object = make_type(c"probe.Alloc", 32, 8, 0, &mut slots);
        let instance = PyType_GenericAlloc(type_object, 3);
        assert!(!instance.is_null(), "{:?}", take_error());
        assert_eq!(Py_REFCNT(instance), 1);
        assert_eq!(Py_TYPE(instance), type_object as *mut types::TypeObject);
        // Variable-size objects expose ob_size at offset 16.
        let variable = instance as *mut PyVarObjectHead;
        let size = unsafe { (*variable).ob_size.load(SEQ) };
        assert_eq!(size, 3);
        Py_DecRef(instance);

        // `_PyObject_New` goes through tp_alloc.
        let fresh = _PyObject_New(type_object);
        assert!(!fresh.is_null(), "{:?}", take_error());
        assert_eq!(Py_TYPE(fresh), type_object as *mut types::TypeObject);
        Py_DecRef(fresh);

        // `PyObject_Init` initialises caller-owned storage.
        let storage = unsafe { libc::malloc(32) } as *mut ShimObject;
        assert!(!storage.is_null());
        let object = PyObject_Init(storage, type_object);
        assert_eq!(object, storage);
        assert_eq!(Py_REFCNT(object), 1);
        Py_DecRef(object);
        Py_DecRef(type_object);
    }

    extern "C" fn noop_traverse(
        _self: *mut ShimObject,
        _visit: *mut c_void,
        _argument: *mut c_void,
    ) -> c_int {
        0
    }

    #[test]
    fn gc_types_track_and_untrack_instances() {
        anchor();
        let mut slots = [
            PyTypeSlotMirror {
                slot: 71, // Py_tp_traverse: required by the GC flag
                pfunc: noop_traverse as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: std::ptr::null_mut(),
            },
        ];
        let type_object = make_type(c"probe.GC", 32, 0, PY_TPFLAGS_HAVE_GC as c_uint, &mut slots);
        let instance = PyType_GenericAlloc(type_object, 0);
        assert!(!instance.is_null());
        assert_eq!(PyObject_IS_GC(instance), 1);
        assert_eq!(PyObject_GC_IsTracked(instance), 1);
        PyObject_GC_UnTrack(instance as *mut c_void);
        assert_eq!(PyObject_GC_IsTracked(instance), 0);
        PyObject_GC_Track(instance as *mut c_void);
        assert_eq!(PyObject_GC_IsTracked(instance), 1);
        Py_DecRef(instance);
        Py_DecRef(type_object);

        // Tracking a non-GC object is a SystemError, not a crash.
        let mut plain_slots = empty_slots();
        let plain = make_type(c"probe.NoGC", 32, 0, 0, &mut plain_slots);
        let object = PyType_GenericAlloc(plain, 0);
        clear_error();
        PyObject_GC_Track(object as *mut c_void);
        let message = take_error().expect("non-GC tracking error");
        assert!(message.contains("not a GC type"), "{message}");
        assert_eq!(PyObject_GC_IsTracked(object), 0);
        Py_DecRef(object);
        Py_DecRef(plain);
    }

    // -- vectorcall and PyCFunction --------------------------------------

    extern "C" fn fast_sum(
        _self: *mut ShimObject,
        args: *const *mut ShimObject,
        nargs: isize,
        kwnames: *mut ShimObject,
    ) -> *mut ShimObject {
        // `nargs` counts positional arguments only; keyword values follow
        // them in the same vector, as `_PyCFunctionFastWithKeywords` defines.
        let positional_count = nargs.max(0) as usize;
        let keyword_count = if kwnames.is_null() {
            0
        } else {
            PyTuple_Size(kwnames).max(0) as usize
        };
        let mut total = 0i64;
        for index in 0..positional_count + keyword_count {
            let item = unsafe { *args.add(index) };
            if let Some(ShimKind::Int(value)) = kind_ref(item) {
                total += *value;
            }
        }
        PyLong_FromLong((total + 1000 * keyword_count as i64) as c_long)
    }

    /// `_PyCFunctionFastWithKeywords` receives the *positional* count; the
    /// keyword values follow in the same vector. PyO3's argument extractor
    /// relies on this (`args + nargs` are the keyword values).
    #[test]
    fn fastcall_nargs_counts_positional_arguments_only() {
        anchor();
        clear_error();
        static RECORDED_NARGS: std::sync::atomic::AtomicIsize =
            std::sync::atomic::AtomicIsize::new(-1);
        static RECORDED_KEYWORDS: std::sync::atomic::AtomicIsize =
            std::sync::atomic::AtomicIsize::new(-1);
        unsafe extern "C" fn record(
            _self_object: *mut ShimObject,
            _args: *const *mut ShimObject,
            nargs: isize,
            kwnames: *mut ShimObject,
        ) -> *mut ShimObject {
            RECORDED_NARGS.store(nargs, SEQ);
            RECORDED_KEYWORDS.store(
                if kwnames.is_null() {
                    0
                } else {
                    PyTuple_Size(kwnames)
                },
                SEQ,
            );
            ptr::addr_of_mut!(_Py_NoneStruct)
        }
        let mut definition = PyMethodDefMirror {
            ml_name: c"record".as_ptr(),
            ml_meth: record as *const () as *mut c_void,
            ml_flags: METH_FASTCALL | METH_KEYWORDS,
            ml_doc: ptr::null(),
        };
        let function = PyCFunction_New(&mut definition, ptr::null_mut());
        assert!(!function.is_null(), "{:?}", take_error());
        let values = [PyLong_FromLong(1), PyLong_FromLong(2), PyLong_FromLong(3)];
        let names = PyTuple_New(1);
        assert_eq!(
            PyTuple_SetItem(names, 0, PyUnicode_FromString(c"three".as_ptr())),
            0
        );
        let result = PyObject_Vectorcall(function, values.as_ptr(), 3, names);
        assert!(!result.is_null(), "{:?}", take_error());
        assert_eq!(RECORDED_NARGS.load(SEQ), 2, "nargs excludes keywords");
        assert_eq!(RECORDED_KEYWORDS.load(SEQ), 1);
        dec_ref(result);
        dec_ref(names);
        dec_ref(function);
        for value in values {
            dec_ref(value);
        }
    }

    fn fast_method_def() -> PyMethodDefMirror {
        PyMethodDefMirror {
            ml_name: c"fast_sum".as_ptr(),
            ml_meth: fast_sum as *const () as *mut c_void,
            ml_flags: METH_FASTCALL | METH_KEYWORDS,
            ml_doc: std::ptr::null(),
        }
    }

    #[test]
    fn vectorcall_dispatches_fastcall_with_keywords() {
        anchor();
        let mut definition = fast_method_def();
        let function = PyCFunction_New(&mut definition, std::ptr::null_mut());
        assert!(!function.is_null());
        assert_eq!(
            PyCFunction_GetFlags(function),
            METH_FASTCALL | METH_KEYWORDS
        );
        assert_eq!(
            PyCFunction_GetFunction(function),
            fast_sum as *const () as *mut c_void
        );

        let values = [PyLong_FromLong(2), PyLong_FromLong(3), PyLong_FromLong(4)];
        let names = PyTuple_New(1);
        assert_eq!(
            PyTuple_SetItem(names, 0, PyUnicode_FromString(c"c".as_ptr())),
            0
        );
        let result = PyObject_Vectorcall(function, values.as_ptr(), 3, names);
        assert!(!result.is_null(), "{:?}", take_error());
        assert_eq!(PyLong_AsLong(result), 9 + 1000);
        Py_DecRef(result);
        Py_DecRef(names);

        // `PyObject_Call` converts a tuple + dict to the same vector.
        let args = PyTuple_New(2);
        assert_eq!(PyTuple_SetItem(args, 0, PyLong_FromLong(10)), 0);
        assert_eq!(PyTuple_SetItem(args, 1, PyLong_FromLong(20)), 0);
        let kwargs = PyDict_New();
        assert_eq!(
            PyDict_SetItemString(kwargs, c"z".as_ptr(), PyLong_FromLong(5)),
            0
        );
        let result = PyObject_Call(function, args, kwargs);
        assert!(!result.is_null(), "{:?}", take_error());
        assert_eq!(PyLong_AsLong(result), 35 + 1000);
        Py_DecRef(result);

        // `PyVectorcall_Call` and `PyObject_CallObject` exist as well.
        let result = PyVectorcall_Call(function, args, kwargs);
        assert!(!result.is_null(), "{:?}", take_error());
        assert_eq!(PyLong_AsLong(result), 35 + 1000);
        Py_DecRef(result);
        let result = PyObject_CallObject(function, args);
        assert!(!result.is_null(), "{:?}", take_error());
        assert_eq!(PyLong_AsLong(result), 30);
        Py_DecRef(result);

        // `PyObject_CallFunctionObjArgs` is the variadic convenience form.
        let result = unsafe {
            super::PyObject_CallFunctionObjArgs(
                function as *mut c_void,
                PyLong_FromLong(6) as *mut c_void,
                PyLong_FromLong(7) as *mut c_void,
                std::ptr::null_mut::<c_void>(),
            )
        } as *mut ShimObject;
        assert!(!result.is_null(), "{:?}", take_error());
        assert_eq!(PyLong_AsLong(result), 13);
        Py_DecRef(result);

        Py_DecRef(kwargs);
        Py_DecRef(args);
        for value in values {
            Py_DecRef(value);
        }
        Py_DecRef(function);
    }

    #[test]
    fn vectorcall_method_binds_instance_methods() {
        anchor();
        let mut methods = [
            PyMethodDefMirror {
                ml_name: c"seven".as_ptr(),
                ml_meth: method_seven as *const () as *mut c_void,
                ml_flags: METH_NOARGS,
                ml_doc: std::ptr::null(),
            },
            PyMethodDefMirror {
                ml_name: std::ptr::null(),
                ml_meth: std::ptr::null_mut(),
                ml_flags: 0,
                ml_doc: std::ptr::null(),
            },
        ];
        let mut slots = [
            PyTypeSlotMirror {
                slot: 64,
                pfunc: methods.as_mut_ptr() as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: std::ptr::null_mut(),
            },
        ];
        let type_object = make_type(c"probe.Methods", 32, 0, 0, &mut slots);
        let instance = PyType_GenericAlloc(type_object, 0);
        let name = PyUnicode_FromString(c"seven".as_ptr());
        let args = [instance];
        let result = PyObject_VectorcallMethod(name, args.as_ptr(), 1, std::ptr::null_mut());
        assert!(!result.is_null(), "{:?}", take_error());
        assert_eq!(PyLong_AsLong(result), 7);
        Py_DecRef(result);

        // Attribute lookup returns the same bound method.
        let method = PyObject_GetAttrString(instance, c"seven".as_ptr());
        assert!(!method.is_null(), "{:?}", take_error());
        let result = PyObject_CallObject(method, std::ptr::null_mut());
        assert!(!result.is_null(), "{:?}", take_error());
        assert_eq!(PyLong_AsLong(result), 7);
        Py_DecRef(result);
        Py_DecRef(method);
        Py_DecRef(name);

        clear_error();
        assert!(PyObject_GetAttrString(instance, c"missing".as_ptr()).is_null());
        assert!(take_error().unwrap().contains("has no attribute"));

        Py_DecRef(instance);
        Py_DecRef(type_object);
    }

    // -- buffers ----------------------------------------------------------

    static mut STRIDED_DATA: [i64; 6] = [1, 2, 3, 4, 5, 6];
    static mut STRIDED_SHAPE: [isize; 2] = [2, 3];
    static mut STRIDED_STRIDES: [isize; 2] = [24, 8];
    static STRIDED_RELEASES: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static mut BAD_DATA: [i64; 3] = [0, 0, 0];
    static mut BAD_SHAPE: [isize; 2] = [2, 3];
    static mut BAD_STRIDES: [isize; 2] = [24, 8];
    static BAD_RELEASES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    extern "C" fn strided_getbuffer(
        object: *mut ShimObject,
        view: *mut Py_buffer,
        _flags: c_int,
    ) -> c_int {
        if view.is_null() {
            return -1;
        }
        inc_ref(object);
        // SAFETY: `view` is caller-owned storage for a Py_buffer.
        unsafe {
            (*view).buf = std::ptr::addr_of_mut!(STRIDED_DATA) as *mut i64 as *mut c_void;
            (*view).obj = object;
            (*view).len = 48;
            (*view).itemsize = 8;
            (*view).readonly = 0;
            (*view).ndim = 2;
            (*view).format = c"l".as_ptr() as *mut c_char;
            (*view).shape = std::ptr::addr_of_mut!(STRIDED_SHAPE) as *mut isize;
            (*view).strides = std::ptr::addr_of_mut!(STRIDED_STRIDES) as *mut isize;
            (*view).suboffsets = std::ptr::null_mut();
            (*view).internal = std::ptr::null_mut();
        }
        0
    }

    extern "C" fn strided_releasebuffer(_object: *mut ShimObject, _view: *mut Py_buffer) {
        STRIDED_RELEASES.fetch_add(1, SEQ);
    }

    extern "C" fn bad_getbuffer(
        object: *mut ShimObject,
        view: *mut Py_buffer,
        _flags: c_int,
    ) -> c_int {
        if view.is_null() {
            return -1;
        }
        inc_ref(object);
        // SAFETY: `view` is caller-owned storage.
        unsafe {
            (*view).buf = std::ptr::addr_of_mut!(BAD_DATA) as *mut i64 as *mut c_void;
            (*view).obj = object;
            // Deliberately inconsistent: shape says 2*3*8 = 48 bytes.
            (*view).len = 40;
            (*view).itemsize = 8;
            (*view).readonly = 0;
            (*view).ndim = 2;
            (*view).format = std::ptr::null_mut();
            (*view).shape = std::ptr::addr_of_mut!(BAD_SHAPE) as *mut isize;
            (*view).strides = std::ptr::addr_of_mut!(BAD_STRIDES) as *mut isize;
            (*view).suboffsets = std::ptr::null_mut();
            (*view).internal = std::ptr::null_mut();
        }
        0
    }

    extern "C" fn bad_releasebuffer(_object: *mut ShimObject, _view: *mut Py_buffer) {
        BAD_RELEASES.fetch_add(1, SEQ);
    }

    #[test]
    fn writable_strided_multi_dimensional_buffers_work() {
        anchor();
        let mut slots = [
            PyTypeSlotMirror {
                slot: 1,
                pfunc: strided_getbuffer as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 2,
                pfunc: strided_releasebuffer as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: std::ptr::null_mut(),
            },
        ];
        let type_object = make_type(c"probe.Strided", 32, 0, 0, &mut slots);
        let instance = PyType_GenericAlloc(type_object, 0);
        assert_eq!(PyObject_CheckBuffer(instance), 1);
        STRIDED_RELEASES.store(0, SEQ);

        let mut view: Py_buffer = unsafe { std::mem::zeroed() };

        // A (2, 3) C-contiguous array with strides (24, 8) is not
        // F-contiguous. The request must fail, the exporter's release hook
        // must run, and no view reference may leak.
        let f_flags = PYBUF_WRITABLE | PYBUF_FORMAT | PYBUF_ND | PYBUF_STRIDES | PYBUF_F_CONTIGUOUS;
        clear_error();
        assert_eq!(PyObject_GetBuffer(instance, &mut view, f_flags), -1);
        let message = take_error().expect("F-contiguous request must fail");
        assert!(message.contains("F_CONTIGUOUS"), "{message}");
        assert_eq!(STRIDED_RELEASES.load(SEQ), 1, "release hook ran");
        assert!(view.obj.is_null());
        assert_eq!(view.len, 0);
        assert_eq!(Py_REFCNT(instance), 1, "no leaked view reference");

        let flags = PYBUF_WRITABLE | PYBUF_FORMAT | PYBUF_ND | PYBUF_STRIDES | PYBUF_C_CONTIGUOUS;
        assert_eq!(
            PyObject_GetBuffer(instance, &mut view, flags),
            0,
            "{:?}",
            take_error()
        );
        assert_eq!(view.obj, instance);
        assert_eq!(view.len, 48);
        assert_eq!(view.itemsize, 8);
        assert_eq!(view.readonly, 0);
        assert_eq!(view.ndim, 2);
        assert_eq!(unsafe { *view.shape }, 2);
        assert_eq!(unsafe { *view.shape.add(1) }, 3);
        assert_eq!(unsafe { *view.strides }, 24);
        assert_eq!(unsafe { *view.strides.add(1) }, 8);
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(view.format as *const c_char) },
            c"l"
        );

        // The view is genuinely writable and strided.
        unsafe {
            *(view.buf as *mut i64) = 99;
            *(view.buf as *mut i64).add(4) = 77; // row 1, column 1
        }
        assert_eq!(unsafe { (*std::ptr::addr_of!(STRIDED_DATA))[0] }, 99);
        assert_eq!(unsafe { (*std::ptr::addr_of!(STRIDED_DATA))[4] }, 77);

        PyBuffer_Release(&mut view);
        assert_eq!(STRIDED_RELEASES.load(SEQ), 2);
        assert!(view.obj.is_null());
        assert_eq!(view.len, 0);
        assert_eq!(Py_REFCNT(instance), 1, "the view reference was released");
        Py_DecRef(instance);
        Py_DecRef(type_object);
    }

    #[test]
    fn bytes_buffers_follow_the_request_flags() {
        anchor();
        let bytes = PyBytes_FromStringAndSize(b"hi".as_ptr() as *const c_char, 2);
        assert_eq!(PyObject_CheckBuffer(bytes), 1);
        assert_eq!(PyObject_CheckBuffer(PyLong_FromLong(1)), 0);

        // PyBUF_SIMPLE: no format, no shape, no strides.
        let mut view: Py_buffer = unsafe { std::mem::zeroed() };
        assert_eq!(PyObject_GetBuffer(bytes, &mut view, PYBUF_SIMPLE), 0);
        assert_eq!(view.len, 2);
        assert_eq!(view.itemsize, 1);
        assert_eq!(view.readonly, 1);
        assert_eq!(view.ndim, 1);
        assert!(view.format.is_null());
        assert!(view.shape.is_null());
        assert!(view.strides.is_null());
        PyBuffer_Release(&mut view);

        // The full record request fills format/shape/stride pointers.
        let flags = PYBUF_FORMAT | PYBUF_ND | PYBUF_STRIDES | PYBUF_C_CONTIGUOUS;
        assert_eq!(PyObject_GetBuffer(bytes, &mut view, flags), 0);
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(view.format as *const c_char) },
            c"B"
        );
        assert_eq!(unsafe { *view.shape }, 2);
        assert_eq!(unsafe { *view.strides }, 1);
        PyBuffer_Release(&mut view);

        clear_error();
        assert_eq!(PyObject_GetBuffer(bytes, &mut view, PYBUF_WRITABLE), -1);
        let message = take_error().expect("read-only bytes");
        assert!(message.contains("not writable"), "{message}");
        assert!(view.obj.is_null());
        Py_DecRef(bytes);
    }

    #[test]
    fn malformed_exporter_views_are_rejected_and_released() {
        anchor();
        let mut slots = [
            PyTypeSlotMirror {
                slot: 1,
                pfunc: bad_getbuffer as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 2,
                pfunc: bad_releasebuffer as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: std::ptr::null_mut(),
            },
        ];
        let type_object = make_type(c"probe.BadBuffer", 32, 0, 0, &mut slots);
        let instance = PyType_GenericAlloc(type_object, 0);
        BAD_RELEASES.store(0, SEQ);

        let mut view: Py_buffer = unsafe { std::mem::zeroed() };
        clear_error();
        assert_eq!(PyObject_GetBuffer(instance, &mut view, PYBUF_WRITABLE), -1);
        let message = take_error().expect("invalid view");
        assert!(message.contains("does not match shape"), "{message}");
        assert_eq!(BAD_RELEASES.load(SEQ), 1, "release hook ran");
        assert!(view.obj.is_null());
        assert_eq!(view.len, 0);
        assert_eq!(Py_REFCNT(instance), 1, "no leaked view reference");

        // Unknown request bits are rejected instead of being ignored.
        clear_error();
        assert_eq!(PyObject_GetBuffer(instance, &mut view, 0x4000), -1);
        assert!(take_error().unwrap().contains("unsupported buffer request"));
        Py_DecRef(instance);
        Py_DecRef(type_object);
    }

    // -- GIL --------------------------------------------------------------

    static GIL_IN_USE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static GIL_MAX_SEEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static GIL_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    extern "C" fn gil_probe(_self: *mut ShimObject, _args: *mut ShimObject) -> *mut ShimObject {
        // Nested Ensure/Release must not deadlock.
        let outer = PyGILState_Ensure();
        let inner = PyGILState_Ensure();
        PyGILState_Release(inner);
        PyGILState_Release(outer);

        let previous = GIL_IN_USE.swap(1, SEQ);
        if previous != 0 {
            GIL_MAX_SEEN.store(2, SEQ);
        } else {
            GIL_MAX_SEEN.fetch_max(1, SEQ);
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
        GIL_IN_USE.store(0, SEQ);
        GIL_CALLS.fetch_add(1, SEQ);
        PyLong_FromLong(1)
    }

    extern "C" fn save_restore(_self: *mut ShimObject, _args: *mut ShimObject) -> *mut ShimObject {
        let state = PyEval_SaveThread();
        PyEval_RestoreThread(state);
        PyLong_FromLong(42)
    }

    #[test]
    fn gil_is_reentrant_and_serialises_callbacks() {
        anchor();
        let mut definition = PyMethodDefMirror {
            ml_name: c"gil_probe".as_ptr(),
            ml_meth: gil_probe as *const () as *mut c_void,
            ml_flags: METH_NOARGS,
            ml_doc: std::ptr::null(),
        };
        let function = PyCFunction_New(&mut definition, std::ptr::null_mut());
        GIL_IN_USE.store(0, SEQ);
        GIL_MAX_SEEN.store(0, SEQ);
        GIL_CALLS.store(0, SEQ);

        let address = function as usize;
        let handles: Vec<std::thread::JoinHandle<()>> = (0..2)
            .map(|_| {
                std::thread::spawn(move || {
                    let function = address as *mut ShimObject;
                    let result =
                        PyObject_Vectorcall(function, std::ptr::null(), 0, std::ptr::null_mut());
                    assert!(!result.is_null(), "{:?}", take_error());
                    assert_eq!(PyLong_AsLong(result), 1);
                    Py_DecRef(result);
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("probe thread");
        }
        assert_eq!(GIL_CALLS.load(SEQ), 2);
        assert_eq!(
            GIL_MAX_SEEN.load(SEQ),
            1,
            "the GIL must serialise foreign callbacks"
        );
        Py_DecRef(function);

        // Save/restore releases the lock completely and reacquires it.
        let mut definition = PyMethodDefMirror {
            ml_name: c"save_restore".as_ptr(),
            ml_meth: save_restore as *const () as *mut c_void,
            ml_flags: METH_NOARGS,
            ml_doc: std::ptr::null(),
        };
        let function = PyCFunction_New(&mut definition, std::ptr::null_mut());
        let result = PyObject_Vectorcall(function, std::ptr::null(), 0, std::ptr::null_mut());
        assert!(!result.is_null(), "{:?}", take_error());
        assert_eq!(PyLong_AsLong(result), 42);
        Py_DecRef(result);
        Py_DecRef(function);

        // Bare thread-state accessors return a stable non-NULL token.
        assert!(!PyThreadState_Get().is_null());
        assert_eq!(PyThreadState_Get(), PyGILState_GetThisThreadState());
    }

    #[test]
    fn gil_state_is_per_thread_reentrant_and_balanced() {
        anchor();
        assert_eq!(PyGILState_Check(), 0);
        let outer = PyGILState_Ensure();
        assert_eq!(outer, PY_GILSTATE_UNLOCKED);
        assert_eq!(PyGILState_Check(), 1);
        let inner = PyGILState_Ensure();
        assert_eq!(
            inner, PY_GILSTATE_LOCKED,
            "nested ensure sees the lock held"
        );
        PyGILState_Release(inner);
        assert_eq!(PyGILState_Check(), 1, "inner release keeps the outer level");
        PyGILState_Release(outer);
        assert_eq!(PyGILState_Check(), 0);

        // Save/restore drops every level and reacquires exactly one.
        let token = PyEval_SaveThread();
        assert_eq!(PyGILState_Check(), 0);
        PyEval_RestoreThread(token);
        assert_eq!(PyGILState_Check(), 1);
        PyEval_SaveThread();
        assert_eq!(PyGILState_Check(), 0);

        // Concurrent threads must each see their own state and never
        // deadlock on the shared lock.
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let handles: Vec<std::thread::JoinHandle<()>> = (0..4)
            .map(|_| {
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..32 {
                        assert_eq!(PyGILState_Check(), 0);
                        let state = PyGILState_Ensure();
                        assert_eq!(PyGILState_Check(), 1);
                        let nested = PyGILState_Ensure();
                        assert_eq!(nested, PY_GILSTATE_LOCKED);
                        PyGILState_Release(nested);
                        PyGILState_Release(state);
                        assert_eq!(PyGILState_Check(), 0);
                        let token = PyEval_SaveThread();
                        PyEval_RestoreThread(token);
                        PyEval_SaveThread();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("gil thread");
        }
        assert_eq!(PyGILState_Check(), 0);
    }

    // -- static C PyTypeObject readiness ---------------------------------

    static mut STATIC_NULL_HEAD: types::TypeObject =
        types::TypeObject::builtin(c"probe.StaticNullHead".as_ptr(), types::TypeRole::Shim, 0);
    static mut STATIC_LINKED_HEAD: types::TypeObject =
        types::TypeObject::builtin(c"probe.StaticLinkedHead".as_ptr(), types::TypeRole::Shim, 0);
    static mut STATIC_BAD: types::TypeObject =
        types::TypeObject::builtin(c"probe.StaticBad".as_ptr(), types::TypeRole::Shim, 0);
    static mut STATIC_CUSTOM_METATYPE: types::TypeObject =
        types::TypeObject::builtin(c"probe.StaticCustomMeta".as_ptr(), types::TypeRole::Shim, 0);
    static mut STATIC_METACLASS: types::TypeObject =
        types::TypeObject::builtin(c"probe.StaticMetaclass".as_ptr(), types::TypeRole::Shim, 0);
    static mut STATIC_UNREADY_BASE: types::TypeObject = types::TypeObject::builtin(
        c"probe.StaticUnreadyBase".as_ptr(),
        types::TypeRole::Shim,
        0,
    );
    static mut STATIC_DERIVED: types::TypeObject =
        types::TypeObject::builtin(c"probe.StaticDerived".as_ptr(), types::TypeRole::Shim, 0);
    static mut STATIC_INVALID_BASE: types::TypeObject = types::TypeObject::builtin(
        c"probe.StaticInvalidBase".as_ptr(),
        types::TypeRole::Shim,
        0,
    );

    #[test]
    fn py_type_ready_adopts_static_c_type_objects() {
        anchor();
        // A `PyVarObject_HEAD_INIT(NULL, 0)` type: ob_type is NULL and the
        // extension-owned storage has no completed metadata.
        let pointer = std::ptr::addr_of_mut!(STATIC_NULL_HEAD);
        unsafe {
            (*pointer).ob_refcnt = std::sync::atomic::AtomicIsize::new(1);
            (*pointer).tp_flags = 0;
        }
        assert_eq!(
            PyType_Ready(pointer as *mut ShimObject),
            0,
            "{:?}",
            take_error()
        );
        unsafe {
            assert_ne!((*pointer).tp_flags & types::PY_TPFLAGS_READY, 0);
            assert_eq!(
                (*pointer).tp_flags & PY_TPFLAGS_HEAPTYPE,
                0,
                "an adopted static type is not a heap type"
            );
            assert_eq!((*pointer).role, types::TypeRole::Instance);
            assert_eq!((*pointer).ob_type, std::ptr::addr_of_mut!(types::TYPE_TYPE));
            assert_eq!(
                (*pointer).tp_base,
                std::ptr::addr_of_mut!(types::OBJECT_TYPE)
            );
            assert_ne!((*pointer).tp_alloc, 0);
            assert_ne!((*pointer).tp_free, 0);
            assert_ne!((*pointer).tp_new, 0);
            assert!(!(*pointer).tp_bases.is_null());
            assert!(!(*pointer).tp_mro.is_null());
            assert!(!(*pointer).tp_dict.is_null());
            assert_eq!(
                (*pointer).ob_refcnt.load(SEQ),
                IMMORTAL_REFCNT,
                "static types are immortal"
            );
        }
        // The ready type allocates raw instances and ignores the type's own
        // reference count.
        let instance = PyType_GenericAlloc(pointer as *mut ShimObject, 0);
        assert!(!instance.is_null(), "{:?}", take_error());
        assert_eq!(Py_TYPE(instance), pointer);
        Py_DecRef(instance);
        Py_DecRef(pointer as *mut ShimObject);
        unsafe {
            assert_eq!((*pointer).ob_refcnt.load(SEQ), IMMORTAL_REFCNT);
        }

        // A static type whose initialiser already names `&PyType_Type` is
        // adopted the same way; its internal tail is not trusted.
        let linked = std::ptr::addr_of_mut!(STATIC_LINKED_HEAD);
        unsafe {
            (*linked).ob_refcnt = std::sync::atomic::AtomicIsize::new(1);
            (*linked).ob_type = std::ptr::addr_of_mut!(types::TYPE_TYPE);
            (*linked).tp_flags = 0;
        }
        assert_eq!(
            PyType_Ready(linked as *mut ShimObject),
            0,
            "{:?}",
            take_error()
        );
        unsafe {
            assert_eq!((*linked).role, types::TypeRole::Instance);
            assert_eq!((*linked).ob_refcnt.load(SEQ), IMMORTAL_REFCNT);
        }

        // A static metaclass declares `tp_base = &PyType_Type`, exactly the
        // `PyArrayDTypeMeta_Type` pattern.
        let metaclass = std::ptr::addr_of_mut!(STATIC_METACLASS);
        unsafe {
            (*metaclass).ob_refcnt = std::sync::atomic::AtomicIsize::new(1);
            (*metaclass).tp_flags = 0;
            (*metaclass).tp_base = std::ptr::addr_of_mut!(types::TYPE_TYPE);
        }
        assert_eq!(
            PyType_Ready(metaclass as *mut ShimObject),
            0,
            "{:?}",
            take_error()
        );
        unsafe {
            assert_eq!(
                (*metaclass).tp_base,
                std::ptr::addr_of_mut!(types::TYPE_TYPE)
            );
            assert_ne!((*metaclass).tp_flags & types::PY_TPFLAGS_READY, 0);
        }

        // A static type may name another static type as its metatype before
        // `PyType_Ready`, as NumPy does with `Py_SET_TYPE(&PyArrayDescr_Type,
        // &PyArrayDTypeMeta_Type)`. The adopted metaclass is a known type
        // object, so the type is adopted (and its link normalised to this
        // host's metatype) instead of being rejected as an instance.
        let custom = std::ptr::addr_of_mut!(STATIC_CUSTOM_METATYPE);
        unsafe {
            (*custom).ob_refcnt = std::sync::atomic::AtomicIsize::new(1);
            (*custom).ob_type = metaclass;
            (*custom).tp_flags = 0;
        }
        assert_eq!(
            PyType_Ready(custom as *mut ShimObject),
            0,
            "{:?}",
            take_error()
        );
        unsafe {
            assert_eq!((*custom).role, types::TypeRole::Instance);
            assert_eq!((*custom).ob_type, std::ptr::addr_of_mut!(types::TYPE_TYPE));
            assert_ne!((*custom).tp_flags & types::PY_TPFLAGS_READY, 0);
        }

        // CPython readies an unready `tp_base` on demand, and NumPy relies on
        // it: `numpy.dtypes.Int8DType.tp_base` names
        // `PyArray_IntAbstractDType`, which the same module initialization
        // readies later. Both types are static objects whose metatype is the
        // adopted metaclass above.
        let unready_base = std::ptr::addr_of_mut!(STATIC_UNREADY_BASE);
        let derived = std::ptr::addr_of_mut!(STATIC_DERIVED);
        unsafe {
            (*unready_base).ob_refcnt = std::sync::atomic::AtomicIsize::new(1);
            (*unready_base).ob_type = metaclass;
            (*unready_base).tp_flags = 0;
            // Its own base is an already adopted static type, as NumPy's
            // abstract dtype has `PyArrayDescr_Type`.
            (*unready_base).tp_base = pointer;
            (*derived).ob_refcnt = std::sync::atomic::AtomicIsize::new(1);
            (*derived).ob_type = metaclass;
            (*derived).tp_flags = 0;
            (*derived).tp_base = unready_base;
        }
        assert_eq!(
            PyType_Ready(derived as *mut ShimObject),
            0,
            "{:?}",
            take_error()
        );
        unsafe {
            assert_ne!(
                (*unready_base).tp_flags & types::PY_TPFLAGS_READY,
                0,
                "the unready base is readied first"
            );
            assert_ne!((*derived).tp_flags & types::PY_TPFLAGS_READY, 0);
            assert_eq!((*derived).tp_base, unready_base);
        }

        // A base that is not a type object is still rejected.
        let invalid = std::ptr::addr_of_mut!(STATIC_INVALID_BASE);
        let instance = new_int(1008);
        unsafe {
            (*invalid).ob_refcnt = std::sync::atomic::AtomicIsize::new(1);
            (*invalid).ob_type = metaclass;
            (*invalid).tp_flags = 0;
            (*invalid).tp_base = instance as *mut types::TypeObject;
        }
        assert_eq!(PyType_Ready(invalid as *mut ShimObject), -1);
        let message = take_error().expect("invalid tp_base");
        assert!(message.contains("invalid tp_base"), "{message}");
        dec_ref(instance);

        // A static type with a negative layout is rejected with a diagnostic
        // and left untouched.
        let bad = std::ptr::addr_of_mut!(STATIC_BAD);
        unsafe {
            (*bad).ob_refcnt = std::sync::atomic::AtomicIsize::new(1);
            (*bad).tp_basicsize = -1;
        }
        assert_eq!(PyType_Ready(bad as *mut ShimObject), -1);
        let message = take_error().expect("negative static layout");
        assert!(message.contains("negative sizes"), "{message}");

        // A non-type (an instance handle) is rejected instead of being
        // overwritten.
        let number = PyLong_FromLong(7);
        assert_eq!(PyType_Ready(number), -1);
        assert!(take_error().unwrap().contains("not a static type object"));
        assert_eq!(PyLong_AsLong(number), 7, "the instance was not modified");
        Py_DecRef(number);
    }

    /// CPython stores a dict's entry count in the `ma_used` word at offset 16,
    /// the shim header's `ob_size`. Extensions (PyO3's dict iterator) read it
    /// directly instead of calling `PyDict_Size`, so it must track mutations.
    #[test]
    fn dict_header_publishes_cpython_ma_used() {
        anchor();
        clear_error();
        let dict = PyDict_New();
        assert!(!dict.is_null(), "{:?}", take_error());
        assert_eq!(unsafe { (*dict).ob_size.load(SEQ) }, 0);
        let key_a = new_str("a".to_string());
        let key_b = new_str("b".to_string());
        let key_c = new_str("c".to_string());
        let value = new_int(1);
        assert_eq!(PyDict_SetItemString(dict, c"a".as_ptr(), value), 0);
        assert_eq!(unsafe { (*dict).ob_size.load(SEQ) }, 1);
        assert_eq!(PyDict_SetItemString(dict, c"b".as_ptr(), value), 0);
        assert_eq!(PyDict_SetItemString(dict, c"c".as_ptr(), value), 0);
        assert_eq!(unsafe { (*dict).ob_size.load(SEQ) }, 3);
        // Replacing a value does not change the entry count.
        assert_eq!(PyDict_SetItemString(dict, c"a".as_ptr(), key_a), 0);
        assert_eq!(unsafe { (*dict).ob_size.load(SEQ) }, 3);
        assert_eq!(PyDict_DelItemString(dict, c"b".as_ptr()), 0);
        assert_eq!(unsafe { (*dict).ob_size.load(SEQ) }, 2);
        assert_eq!(
            unsafe { super::container_api::PyDict_Pop(dict, key_c, ptr::null_mut()) },
            1
        );
        assert_eq!(unsafe { (*dict).ob_size.load(SEQ) }, 1);
        PyDict_Clear(dict);
        assert_eq!(unsafe { (*dict).ob_size.load(SEQ) }, 0);
        dec_ref(dict);
        dec_ref(key_a);
        dec_ref(key_b);
        dec_ref(key_c);
        dec_ref(value);
    }

    /// PyO3's `PyClassInitializer` fetches the container base's `tp_new` and
    /// calls it as `tp_new(subtype, (), NULL)` to allocate a Python subclass
    /// (pydantic-core's `ValidationError` extends `ValueError`). Static
    /// exception classes expose that slot, while constructing them directly
    /// still yields the shim exception instance.
    #[test]
    fn native_exception_base_exposes_tp_new_for_pyo3_subclasses() {
        anchor();
        clear_error();
        let exception = unsafe { PyExc_ValueError };
        let slot = PyType_GetSlot(exception, 65);
        assert!(!slot.is_null(), "PyExc_ValueError must expose Py_tp_new");
        assert!(
            !PyType_GetSlot(exception, 74).is_null(),
            "PyExc_ValueError must expose Py_tp_free for the subclass deallocator"
        );

        // `#[pyclass(extends=PyTzInfo)]` (pydantic-core's datetime types) also
        // fetches the base constructor and free hook from the datetime C API
        // type objects.
        let tzinfo = ptr::addr_of_mut!(types::TZINFO_CAPI_TYPE) as *mut ShimObject;
        assert!(!PyType_GetSlot(tzinfo, 65).is_null(), "tzinfo tp_new");
        assert!(!PyType_GetSlot(tzinfo, 74).is_null(), "tzinfo tp_free");

        let mut slots = empty_slots();
        let mut spec = PyTypeSpecMirror {
            name: c"probe.ExceptionSubclass".as_ptr(),
            basicsize: 0,
            itemsize: 0,
            flags: 0,
            slots: slots.as_mut_ptr(),
        };
        let bases = PyTuple_New(1);
        inc_ref(exception);
        assert_eq!(PyTuple_SetItem(bases, 0, exception), 0);
        let subclass = PyType_FromSpecWithBases(&mut spec, bases);
        assert!(!subclass.is_null(), "{:?}", take_error());

        // SAFETY: the slot is the exported native base constructor.
        let new_function: unsafe extern "C" fn(
            *mut types::TypeObject,
            *mut ShimObject,
            *mut ShimObject,
        ) -> *mut ShimObject = unsafe { std::mem::transmute(slot) };
        let empty = PyTuple_New(0);
        let instance =
            unsafe { new_function(subclass as *mut types::TypeObject, empty, ptr::null_mut()) };
        assert!(!instance.is_null(), "{:?}", take_error());
        assert_eq!(Py_TYPE(instance) as *mut ShimObject, subclass);
        dec_ref(empty);
        dec_ref(instance);
        dec_ref(subclass);
        dec_ref(bases);

        // Constructing the static class itself still builds the shim payload.
        let arguments = PyTuple_New(1);
        let message = new_str("boom".to_string());
        assert_eq!(PyTuple_SetItem(arguments, 0, message), 0);
        let raised = PyObject_CallObject(exception, arguments);
        assert!(!raised.is_null(), "{:?}", take_error());
        assert!(matches!(kind_ref(raised), Some(ShimKind::Exception { .. })));
        dec_ref(raised);
        dec_ref(arguments);
    }

    /// A raise that crosses the C-API boundary keeps the extension exception
    /// instance: the Hydro exception delegates `str`, attributes and methods
    /// to it while still carrying the handler-compatible tag.
    #[test]
    fn extension_exception_instance_survives_the_hydro_raise() {
        anchor();
        clear_error();

        unsafe extern "C" fn str_slot(_self: *mut ShimObject) -> *mut ShimObject {
            PyUnicode_FromString(c"boom".as_ptr())
        }
        unsafe extern "C" fn errors_method(
            _self: *mut ShimObject,
            _args: *mut ShimObject,
        ) -> *mut ShimObject {
            let list = PyList_New(0);
            let item = PyUnicode_FromString(c"x".as_ptr());
            assert_eq!(PyList_Append(list, item), 0);
            dec_ref(item);
            list
        }

        let mut methods = [
            PyMethodDefMirror {
                ml_name: c"errors".as_ptr(),
                ml_meth: errors_method as *const () as *mut c_void,
                ml_flags: METH_VARARGS,
                ml_doc: ptr::null(),
            },
            PyMethodDefMirror {
                ml_name: ptr::null(),
                ml_meth: ptr::null_mut(),
                ml_flags: 0,
                ml_doc: ptr::null(),
            },
        ];
        let mut slots = [
            PyTypeSlotMirror {
                slot: 64, // Py_tp_methods
                pfunc: methods.as_mut_ptr() as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 70, // Py_tp_str
                pfunc: str_slot as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: ptr::null_mut(),
            },
        ];
        let exception = unsafe { PyExc_ValueError };
        let mut spec = PyTypeSpecMirror {
            name: c"probe.ExtensionError".as_ptr(),
            basicsize: 0,
            itemsize: 0,
            flags: 0,
            slots: slots.as_mut_ptr(),
        };
        let bases = PyTuple_New(1);
        inc_ref(exception);
        assert_eq!(PyTuple_SetItem(bases, 0, exception), 0);
        let subclass = PyType_FromSpecWithBases(&mut spec, bases);
        assert!(!subclass.is_null(), "{:?}", take_error());

        // Allocate an opaque instance through the exception base's `tp_new`,
        // exactly as PyO3's `PyClassInitializer` does.
        let slot = PyType_GetSlot(exception, 65);
        // SAFETY: the slot is the exported native base constructor.
        let new_function: unsafe extern "C" fn(
            *mut types::TypeObject,
            *mut ShimObject,
            *mut ShimObject,
        ) -> *mut ShimObject = unsafe { std::mem::transmute(slot) };
        let empty = PyTuple_New(0);
        let instance =
            unsafe { new_function(subclass as *mut types::TypeObject, empty, ptr::null_mut()) };
        assert!(!instance.is_null(), "{:?}", take_error());
        dec_ref(empty);
        assert!(object_is_exception(instance));

        // Raising it stores the instance in the C error state; the bridge
        // recovers it from the consumed message and preserves it in the
        // pending Hydro exception.
        PyErr_SetRaisedException(instance);
        let message = take_error().expect("raised exception message");
        let preserved = take_error_exception(&message).expect("extension instance");
        assert!(object_is_exception(preserved));
        let object = exception_object_to_hydro(preserved);
        assert_eq!(
            crate::exc::raise_extension(object, crate::exc::RUNTIME_ERROR, &message),
            crate::value::NONE
        );
        let pending = crate::exc::pending_value();
        assert_eq!(
            crate::exc::pending_tag(),
            crate::exc::RUNTIME_ERROR as i64,
            "the frontend approximation is the handler tag"
        );
        assert_eq!(
            crate::value::exception_data(pending).and_then(|data| data.object),
            Some(object)
        );

        // `str` and methods delegate to the original instance. The raise
        // machinery's pending exception stays pending here because no
        // generated handler consumed it; dispatch must neither replace it nor
        // leave a new exception behind.
        assert_eq!(crate::value::to_string(pending), "boom");
        let errors = crate::methods::call(pending, b"errors".as_ptr(), 6, 0, ptr::null());
        let pending_after = crate::exc::pending_value();
        if pending_after != pending {
            let data = crate::value::exception_data(pending_after);
            panic!(
                "extension method dispatch raised tag {:?}: {:?}",
                data.map(|data| data.tag),
                data.map(|data| data.msg.clone())
            );
        }
        let items = crate::value::as_list(errors).expect("list result");
        assert_eq!(items.len(), 1);
        assert_eq!(crate::value::as_str(items[0]), Some("x"));

        crate::exc::clear();
        dec_ref(subclass);
        dec_ref(bases);
    }

    /// A Hydro class crossing to an extension must pass CPython's inline
    /// `PyType_Check` (`Py_TYPE(op)->tp_flags & Py_TPFLAGS_TYPE_SUBCLASS`), so
    /// PyO3's `&Bound<PyType>` extraction accepts pydantic's model `cls`.
    #[test]
    fn hydro_class_handles_pass_c_type_checks_and_expose_names() {
        anchor();
        clear_error();
        let class = crate::value::class_new("CProbe".to_string(), crate::value::NONE, Vec::new());
        let module = crate::value::str_alloc("probe_module".to_string());
        assert!(crate::value::class_set_attr(
            class,
            "__module__".to_string(),
            module
        ));
        let roots = crate::heap::root_values(&[class]);
        let handle = value_object(ShimValue::Hydro(HydroValue::new(class)));

        let metatype = Py_TYPE(handle);
        assert!(!metatype.is_null());
        // SAFETY: metatype is the synthetic type object owned by the handle.
        assert_ne!(
            unsafe { (*metatype).tp_flags } & types::PY_TPFLAGS_TYPE_SUBCLASS,
            0,
            "PyType_Check must accept a class handle"
        );
        assert_eq!(
            types::PyType_IsSubtype(
                handle,
                std::ptr::addr_of_mut!(types::TYPE_TYPE) as *mut ShimObject
            ),
            1,
            "a class handle is a subtype of type"
        );
        assert_eq!(
            PyObject_IsInstance(
                handle,
                std::ptr::addr_of_mut!(types::TYPE_TYPE) as *mut ShimObject
            ),
            1,
            "isinstance(cls, type)"
        );

        let name = PyType_GetName(handle);
        assert_eq!(
            as_string(name).as_deref(),
            Some("CProbe"),
            "{:?}",
            take_error()
        );
        dec_ref(name);
        let qualname = PyType_GetQualName(handle);
        assert_eq!(as_string(qualname).as_deref(), Some("CProbe"));
        dec_ref(qualname);
        let module_name = PyType_GetModuleName(handle);
        assert_eq!(as_string(module_name).as_deref(), Some("probe_module"));
        dec_ref(module_name);

        dec_ref(handle);
        crate::heap::release_roots(roots);
    }

    // -- GC traverse / clear ----------------------------------------------

    static GC_CLEAR_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static GC_CHILD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    extern "C" fn probe_traverse(
        _self: *mut ShimObject,
        visit: *mut c_void,
        argument: *mut c_void,
    ) -> c_int {
        if visit.is_null() {
            return 0;
        }
        let visit: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int =
            unsafe { std::mem::transmute(visit) };
        // SAFETY: the visitor and its argument come from the host walker.
        // `visitproc` receives the visited object first, then the context.
        unsafe { visit(GC_CHILD.load(SEQ) as *mut c_void, argument) }
    }

    extern "C" fn probe_clear(_self: *mut ShimObject) {
        GC_CLEAR_CALLS.fetch_add(1, SEQ);
    }

    #[test]
    fn gc_traverse_enumerates_edges_and_default_dealloc_clears() {
        anchor();
        let mut slots = [
            PyTypeSlotMirror {
                slot: 71, // Py_tp_traverse
                pfunc: probe_traverse as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 51, // Py_tp_clear
                pfunc: probe_clear as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: std::ptr::null_mut(),
            },
        ];
        let type_object = make_type(
            c"probe.Traverse",
            32,
            0,
            PY_TPFLAGS_HAVE_GC as c_uint,
            &mut slots,
        );
        let instance = PyType_GenericAlloc(type_object, 0);
        assert!(!instance.is_null(), "{:?}", take_error());
        let child = PyLong_FromLong(11);
        GC_CHILD.store(child as usize, SEQ);
        GC_CLEAR_CALLS.store(0, SEQ);

        let mut visited: Vec<*mut ShimObject> = Vec::new();
        types::gc_traverse(instance, &mut |edge| {
            visited.push(edge);
            std::ops::ControlFlow::Continue(())
        });
        assert_eq!(visited, vec![child], "tp_traverse is called with the child");

        Py_DecRef(instance);
        assert_eq!(
            GC_CLEAR_CALLS.load(SEQ),
            1,
            "tp_clear runs once at default dealloc"
        );
        Py_DecRef(child);
        Py_DecRef(type_object);

        // CPython rejects a GC type that cannot be traversed; so does this
        // host, with the same diagnostic.
        let mut bad_slots = empty_slots();
        let mut bad_spec = PyTypeSpecMirror {
            name: c"probe.GCNoTraverse".as_ptr(),
            basicsize: 0,
            itemsize: 0,
            flags: PY_TPFLAGS_HAVE_GC as c_uint,
            slots: bad_slots.as_mut_ptr(),
        };
        clear_error();
        assert!(PyType_FromSpec(&mut bad_spec).is_null());
        let message = take_error().expect("GC without traverse");
        assert!(message.contains("has no tp_traverse function"), "{message}");
    }

    // -- production GC pass (PyGC_Collect) --------------------------------

    /// One clear per instance whose cycle the collector broke.
    static CYCLE_CLEARS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// Serialises tests that run a collection pass: the tracked set is
    /// process-wide, so two passes at once could break each other's cycle.
    static GC_PASS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn gc_pass_lock() -> std::sync::MutexGuard<'static, ()> {
        GC_PASS_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `tp_traverse` of the cycle probe: visits the child stored directly after
    /// `PyObject_HEAD`, the CPython instance layout this host allocates.
    extern "C" fn cycle_traverse(
        self_object: *mut ShimObject,
        visit: *mut c_void,
        argument: *mut c_void,
    ) -> c_int {
        if visit.is_null() {
            return 0;
        }
        let visit: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int =
            unsafe { std::mem::transmute(visit) };
        // SAFETY: instances of this type store their child at that offset and
        // the pass holds a reference to the object.
        let child =
            unsafe { *((self_object as *mut u8).add(PY_OBJECT_HEAD_SIZE) as *mut *mut ShimObject) };
        if child.is_null() {
            0
        } else {
            // SAFETY: the visitor comes from `gc_traverse`; `visitproc` takes
            // the visited object first and the context second.
            unsafe { visit(child as *mut c_void, argument) }
        }
    }

    /// `tp_clear` of the cycle probe: releases the stored handle and counts.
    extern "C" fn cycle_clear(self_object: *mut ShimObject) -> c_int {
        CYCLE_CLEARS.fetch_add(1, SEQ);
        // SAFETY: same layout as `cycle_traverse`; the object is live.
        let slot =
            unsafe { (self_object as *mut u8).add(PY_OBJECT_HEAD_SIZE) as *mut *mut ShimObject };
        let child = unsafe { *slot };
        if !child.is_null() {
            unsafe { *slot = std::ptr::null_mut() };
            Py_DecRef(child);
        }
        0
    }

    /// Creates the cycle probe type (GC flag, traverse and clear slots).
    fn cycle_probe_type() -> *mut ShimObject {
        let mut slots = [
            PyTypeSlotMirror {
                slot: 71, // Py_tp_traverse
                pfunc: cycle_traverse as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 51, // Py_tp_clear
                pfunc: cycle_clear as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: std::ptr::null_mut(),
            },
        ];
        make_type(
            c"probe.Cycle",
            (PY_OBJECT_HEAD_SIZE + std::mem::size_of::<*mut c_void>()) as c_int,
            0,
            PY_TPFLAGS_HAVE_GC as c_uint,
            &mut slots,
        )
    }

    /// Stores `child` in the probe instance's trailing pointer, taking a
    /// reference.
    fn set_cycle_child(object: *mut ShimObject, child: *mut ShimObject) {
        // SAFETY: the object is a live cycle-probe instance.
        let slot = unsafe { (object as *mut u8).add(PY_OBJECT_HEAD_SIZE) as *mut *mut ShimObject };
        let previous = unsafe { *slot };
        Py_IncRef(child);
        unsafe { *slot = child };
        if !previous.is_null() {
            Py_DecRef(previous);
        }
    }

    #[test]
    fn py_gc_collect_breaks_a_tracked_cycle_exactly_once() {
        anchor();
        let _guard = gc_pass_lock();
        let type_object = cycle_probe_type();
        let first = PyType_GenericAlloc(type_object, 0);
        let second = PyType_GenericAlloc(type_object, 0);
        assert!(!first.is_null() && !second.is_null(), "{:?}", take_error());
        set_cycle_child(first, second);
        set_cycle_child(second, first);
        CYCLE_CLEARS.store(0, SEQ);
        // The caller's references go away; only the cycle keeps each alive.
        Py_DecRef(first);
        Py_DecRef(second);

        let collected = PyGC_Collect();
        assert!(
            collected >= 2,
            "the collector did not reclaim the cycle: {collected}"
        );
        assert_eq!(
            CYCLE_CLEARS.load(SEQ),
            2,
            "tp_clear must run exactly once per collected instance"
        );
        // A second pass finds no candidate and must not clear again.
        PyGC_Collect();
        assert_eq!(
            CYCLE_CLEARS.load(SEQ),
            2,
            "a collected cycle must not be cleared twice"
        );
        Py_DecRef(type_object);
    }

    #[test]
    fn py_gc_collect_keeps_a_cycle_reachable_from_outside() {
        anchor();
        let _guard = gc_pass_lock();
        let type_object = cycle_probe_type();
        let first = PyType_GenericAlloc(type_object, 0);
        let second = PyType_GenericAlloc(type_object, 0);
        assert!(!first.is_null() && !second.is_null(), "{:?}", take_error());
        set_cycle_child(first, second);
        set_cycle_child(second, first);
        CYCLE_CLEARS.store(0, SEQ);
        // Drop `second`'s allocation reference: afterwards only `first` has a
        // reference from this frame, which is outside the tracked set.
        // `second` is only referenced from `first`, so it must be reached
        // through `tp_traverse` rather than collected.
        Py_DecRef(second);
        PyGC_Collect();
        assert_eq!(
            CYCLE_CLEARS.load(SEQ),
            0,
            "a cycle reachable from outside must not be cleared"
        );

        // Dropping the outside reference leaves the bare cycle behind.
        Py_DecRef(first);
        let collected = PyGC_Collect();
        assert!(
            collected >= 2,
            "the unreachable cycle was not collected: {collected}"
        );
        assert_eq!(CYCLE_CLEARS.load(SEQ), 2);
        Py_DecRef(type_object);
    }

    // -- C fixture: real extension instances, traverse/clear --------------

    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// A real stable-ABI extension compiled at test time. It exposes a
    /// `Py_TPFLAGS_HAVE_GC` type whose `tp_traverse` and `tp_clear` count their
    /// calls and release a stored handle, plus the threaded-callback probe.
    ///
    /// The fixture imports nothing beyond `<Python.h>`; every one of those
    /// symbols is bound to this process's runtime definition through
    /// [`install_host_exports`], so it exercises the real C API exactly like
    /// the checked-in integration fixtures.
    const GC_PROBE_SOURCE: &str = r#"#define Py_LIMITED_API 0x030C0000
#include <Python.h>
#include <pthread.h>
#include <stddef.h>
#include <stdlib.h>

typedef struct {
    PyObject_HEAD
    PyObject *child;
} NodeObject;

static PyObject *NodeType;
static PyObject *kept_node;

static long traverse_calls;
static long clear_calls;
static long finalize_calls;
static long children_set;
static long children_cleared;
static long callback_calls;

static int Node_traverse(PyObject *self, visitproc visit, void *arg) {
    NodeObject *node = (NodeObject *)self;
    traverse_calls++;
    Py_VISIT(node->child);
    return 0;
}

static int Node_clear(PyObject *self) {
    NodeObject *node = (NodeObject *)self;
    clear_calls++;
    if (node->child != NULL) {
        children_cleared++;
        Py_CLEAR(node->child);
    }
    return 0;
}

static void Node_finalize(PyObject *self) {
    (void)self;
    finalize_calls++;
}

static PyObject *Node_new(PyTypeObject *type, PyObject *args, PyObject *kwds) {
    NodeObject *self;
    (void)args;
    (void)kwds;
    self = (NodeObject *)PyType_GenericAlloc(type, 0);
    if (self != NULL) {
        self->child = NULL;
    }
    return (PyObject *)self;
}

static int Node_set_child(PyObject *self, PyObject *value, void *closure) {
    (void)closure;
    if (value == NULL) {
        PyErr_SetString(PyExc_AttributeError, "cannot delete child");
        return -1;
    }
    {
        NodeObject *node = (NodeObject *)self;
        PyObject *previous = node->child;
        Py_INCREF(value);
        node->child = value;
        Py_XDECREF(previous);
    }
    children_set++;
    return 0;
}

static PyObject *Node_get_child(PyObject *self, void *closure) {
    PyObject *child = ((NodeObject *)self)->child;
    (void)closure;
    if (child == NULL) {
        Py_RETURN_NONE;
    }
    Py_INCREF(child);
    return child;
}

static PyGetSetDef Node_getset[] = {
    {"child", Node_get_child, Node_set_child, NULL, NULL},
    {NULL, NULL, NULL, NULL, NULL}
};

static PyType_Slot Node_slots[] = {
    {Py_tp_new, (void *)Node_new},
    {Py_tp_getset, (void *)Node_getset},
    {Py_tp_traverse, (void *)Node_traverse},
    {Py_tp_clear, (void *)Node_clear},
    {Py_tp_finalize, (void *)Node_finalize},
    {0, NULL}
};

static PyType_Spec Node_spec = {
    "gc_probe.Node",
    (int)sizeof(NodeObject),
    0,
    Py_TPFLAGS_DEFAULT | Py_TPFLAGS_HAVE_GC,
    Node_slots
};

static PyObject *new_node(void) {
    return PyObject_CallNoArgs(NodeType);
}

static int link_node(PyObject *from, PyObject *to) {
    return Node_set_child(from, to, NULL);
}

static PyObject *probe_make_cycle(PyObject *module, PyObject *unused) {
    PyObject *a;
    PyObject *b;
    (void)module;
    (void)unused;
    a = new_node();
    b = new_node();
    if (a == NULL || b == NULL) {
        Py_XDECREF(a);
        Py_XDECREF(b);
        return NULL;
    }
    if (link_node(a, b) < 0 || link_node(b, a) < 0) {
        Py_DECREF(a);
        Py_DECREF(b);
        return NULL;
    }
    Py_DECREF(a);
    Py_DECREF(b);
    Py_RETURN_NONE;
}

static PyObject *probe_keep_cycle(PyObject *module, PyObject *unused) {
    PyObject *a;
    PyObject *b;
    (void)module;
    (void)unused;
    a = new_node();
    b = new_node();
    if (a == NULL || b == NULL) {
        Py_XDECREF(a);
        Py_XDECREF(b);
        return NULL;
    }
    if (link_node(a, b) < 0 || link_node(b, a) < 0) {
        Py_DECREF(a);
        Py_DECREF(b);
        return NULL;
    }
    Py_CLEAR(kept_node);
    kept_node = a;
    Py_DECREF(b);
    Py_RETURN_NONE;
}

static PyObject *probe_drop_kept(PyObject *module, PyObject *unused) {
    (void)module;
    (void)unused;
    Py_CLEAR(kept_node);
    Py_RETURN_NONE;
}

static PyObject *probe_teardown(PyObject *module, PyObject *unused) {
    PyObject *node;
    PyObject *child;
    (void)module;
    (void)unused;
    node = new_node();
    if (node == NULL) {
        return NULL;
    }
    child = PyLong_FromLong(11);
    if (child == NULL) {
        Py_DECREF(node);
        return NULL;
    }
    if (link_node(node, child) < 0) {
        Py_DECREF(child);
        Py_DECREF(node);
        return NULL;
    }
    Py_DECREF(child);
    Py_DECREF(node);
    Py_RETURN_NONE;
}

static PyObject *probe_collect(PyObject *module, PyObject *unused) {
    (void)module;
    (void)unused;
    return PyLong_FromLong((long)PyGC_Collect());
}

static PyObject *probe_reset_stats(PyObject *module, PyObject *unused) {
    (void)module;
    (void)unused;
    traverse_calls = 0;
    clear_calls = 0;
    finalize_calls = 0;
    children_set = 0;
    children_cleared = 0;
    callback_calls = 0;
    Py_RETURN_NONE;
}

static int add_long(PyObject *tuple, Py_ssize_t index, long value) {
    PyObject *number = PyLong_FromLong(value);
    if (number == NULL) {
        return -1;
    }
    if (PyTuple_SetItem(tuple, index, number) < 0) {
        Py_DECREF(number);
        return -1;
    }
    return 0;
}

static PyObject *probe_stats(PyObject *module, PyObject *unused) {
    PyObject *tuple;
    (void)module;
    (void)unused;
    tuple = PyTuple_New(6);
    if (tuple == NULL) {
        return NULL;
    }
    if (add_long(tuple, 0, traverse_calls) < 0 ||
        add_long(tuple, 1, clear_calls) < 0 ||
        add_long(tuple, 2, finalize_calls) < 0 ||
        add_long(tuple, 3, children_set) < 0 ||
        add_long(tuple, 4, children_cleared) < 0 ||
        add_long(tuple, 5, callback_calls) < 0) {
        Py_DECREF(tuple);
        return NULL;
    }
    return tuple;
}

static PyObject *probe_callback(PyObject *module, PyObject *unused) {
    (void)module;
    (void)unused;
    callback_calls++;
    return PyLong_FromLong(7);
}

typedef struct {
    PyObject *callable;
    PyObject *handle;
    long iterations;
    long failures;
} Worker;

static void *worker_main(void *raw) {
    Worker *work = (Worker *)raw;
    long i;
    for (i = 0; i < work->iterations; i++) {
        int state = PyGILState_Ensure();
        PyObject *result = PyObject_CallNoArgs(work->callable);
        if (result == NULL) {
            work->failures++;
            PyErr_Clear();
        } else {
            Py_DECREF(result);
        }
        Py_INCREF(work->handle);
        Py_DECREF(work->handle);
        PyGILState_Release(state);
    }
    return NULL;
}

static PyObject *probe_threaded_call(PyObject *module, PyObject *args) {
    PyObject *handle;
    PyObject *callable;
    PyThreadState *saved;
    pthread_t ids[32];
    Worker *work;
    long threads = 4;
    long iterations = 64;
    long started = 0;
    long failures = 0;
    long i;
    if (!PyArg_ParseTuple(args, "O|ll:threaded_call", &handle, &threads, &iterations)) {
        return NULL;
    }
    if (threads < 1 || threads > 32 || iterations < 1 || iterations > 1000000) {
        PyErr_SetString(PyExc_ValueError, "threaded_call: argument out of range");
        return NULL;
    }
    callable = PyObject_GetAttrString(module, "callback");
    if (callable == NULL) {
        return NULL;
    }
    work = (Worker *)calloc((size_t)threads, sizeof(Worker));
    if (work == NULL) {
        Py_DECREF(callable);
        return PyErr_NoMemory();
    }
    Py_INCREF(handle);
    for (i = 0; i < threads; i++) {
        work[i].callable = callable;
        work[i].handle = handle;
        work[i].iterations = iterations;
        work[i].failures = 0;
        if (pthread_create(&ids[i], NULL, worker_main, &work[i]) != 0) {
            break;
        }
        started++;
    }
    saved = PyEval_SaveThread();
    for (i = 0; i < started; i++) {
        pthread_join(ids[i], NULL);
    }
    PyEval_RestoreThread(saved);
    for (i = 0; i < started; i++) {
        failures += work[i].failures;
    }
    free(work);
    Py_DECREF(callable);
    if (started != threads || failures != 0) {
        Py_DECREF(handle);
        PyErr_SetString(PyExc_RuntimeError, "threaded_call: callback failed");
        return NULL;
    }
    return handle;
}

static PyMethodDef probe_methods[] = {
    {"make_cycle", probe_make_cycle, METH_NOARGS, NULL},
    {"keep_cycle", probe_keep_cycle, METH_NOARGS, NULL},
    {"drop_kept", probe_drop_kept, METH_NOARGS, NULL},
    {"teardown", probe_teardown, METH_NOARGS, NULL},
    {"collect", probe_collect, METH_NOARGS, NULL},
    {"stats", probe_stats, METH_NOARGS, NULL},
    {"reset_stats", probe_reset_stats, METH_NOARGS, NULL},
    {"callback", probe_callback, METH_NOARGS, NULL},
    {"threaded_call", probe_threaded_call, METH_VARARGS, NULL},
    {NULL, NULL, 0, NULL}
};

static struct PyModuleDef gc_probe_module = {
    PyModuleDef_HEAD_INIT,
    "gc_probe",
    NULL,
    -1,
    probe_methods,
    NULL,
    NULL,
    NULL,
    NULL
};

PyMODINIT_FUNC PyInit_gc_probe(void) {
    PyObject *module = PyModule_Create(&gc_probe_module);
    if (module == NULL) {
        return NULL;
    }
    NodeType = PyType_FromSpec(&Node_spec);
    if (NodeType == NULL) {
        Py_DECREF(module);
        return NULL;
    }
    if (PyModule_AddObject(module, "Node", NodeType) < 0) {
        Py_DECREF(NodeType);
        Py_DECREF(module);
        return NULL;
    }
    return module;
}
"#;

    /// Reads a little-endian `u16` from an ELF image at `offset`.
    #[cfg(target_os = "linux")]
    fn elf_u16(image: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(image[offset..offset + 2].try_into().expect("ELF u16"))
    }

    /// Reads a little-endian `u32` from an ELF image at `offset`.
    #[cfg(target_os = "linux")]
    fn elf_u32(image: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(image[offset..offset + 4].try_into().expect("ELF u32"))
    }

    /// Reads a little-endian `u64` from an ELF image at `offset`.
    #[cfg(target_os = "linux")]
    fn elf_u64(image: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(image[offset..offset + 8].try_into().expect("ELF u64"))
    }

    /// One `Elf64_Shdr`, reduced to the fields the symbol reader needs.
    #[cfg(target_os = "linux")]
    struct ElfSection {
        kind: u32,
        offset: usize,
        size: usize,
        link: usize,
        entry_size: usize,
    }

    /// Load base of the main executable (`dlpi_addr` of an empty `dlpi_name`).
    #[cfg(target_os = "linux")]
    unsafe extern "C" fn find_main_executable(
        info: *mut libc::dl_phdr_info,
        _size: usize,
        context: *mut c_void,
    ) -> c_int {
        // SAFETY: the dynamic loader passes a live descriptor for the call.
        let info = unsafe { &*info };
        let name = info.dlpi_name;
        if name.is_null() || unsafe { *name } == 0 {
            // SAFETY: `context` is the `usize` out-parameter below.
            unsafe { *(context as *mut usize) = info.dlpi_addr as usize };
            return 1;
        }
        0
    }

    /// Returns the address the main executable was mapped at.
    #[cfg(target_os = "linux")]
    fn main_executable_base() -> usize {
        let mut base = 0usize;
        // SAFETY: the callback only reads the descriptor it is handed.
        unsafe {
            libc::dl_iterate_phdr(
                Some(find_main_executable),
                &mut base as *mut usize as *mut c_void,
            );
        }
        base
    }

    /// Collects every `Py*`/`_Py*` definition in the test executable's own
    /// symbol table, as `(name, runtime address)`.
    ///
    /// The library's unit-test harness is linked without `--export-dynamic`:
    /// Cargo's `rustc-link-arg-tests` reaches integration test targets only,
    /// so the harness's dynamic symbol table contains imports but no
    /// definitions. The static symbol table is still complete, and these are
    /// the same real definitions the integration fixtures resolve through the
    /// dynamic table.
    #[cfg(target_os = "linux")]
    fn executable_py_symbols() -> Vec<(String, usize)> {
        use std::collections::BTreeMap;

        let path = std::env::current_exe().expect("the test executable path");
        let image = std::fs::read(&path).expect("read the test executable");
        assert!(
            image.len() > 64 && image.starts_with(b"\x7fELF") && image[4] == 2 && image[5] == 1,
            "the unit-test harness is not an ELF64 little-endian image: {}",
            path.display()
        );
        let section_offset = elf_u64(&image, 0x28) as usize;
        let section_size = elf_u16(&image, 0x3a) as usize;
        let section_count = elf_u16(&image, 0x3c) as usize;
        let mut sections = Vec::with_capacity(section_count);
        for index in 0..section_count {
            let header = section_offset + index * section_size;
            sections.push(ElfSection {
                kind: elf_u32(&image, header + 4),
                offset: elf_u64(&image, header + 0x18) as usize,
                size: elf_u64(&image, header + 0x20) as usize,
                link: elf_u32(&image, header + 0x28) as usize,
                entry_size: elf_u64(&image, header + 0x38) as usize,
            });
        }
        let symbol_table = sections
            .iter()
            .find(|section| section.kind == 2)
            .expect("the test executable has no .symtab; it must not be stripped");
        let strings = &sections[symbol_table.link];
        let base = main_executable_base();
        let mut symbols: BTreeMap<String, usize> = BTreeMap::new();
        for index in 0..symbol_table.size / symbol_table.entry_size {
            let entry = symbol_table.offset + index * symbol_table.entry_size;
            let name_offset = elf_u32(&image, entry) as usize;
            let info = image[entry + 4];
            let section = elf_u16(&image, entry + 6);
            let value = elf_u64(&image, entry + 8) as usize;
            // Only defined global/weak functions and objects can back a
            // dynamic relocation; undefined, absolute, local and TLS entries
            // cannot.
            let binding = info >> 4;
            let kind = info & 0xf;
            if section == 0 || section >= 0xff00 || !(binding == 1 || binding == 2) || kind > 2 {
                continue;
            }
            let start = strings.offset + name_offset;
            let length = image[start..]
                .iter()
                .position(|&byte| byte == 0)
                .expect("unterminated symbol name");
            let name = std::str::from_utf8(&image[start..start + length]).expect("symbol name");
            if !(name.starts_with("Py") || name.starts_with("_Py")) {
                continue;
            }
            symbols.entry(name.to_string()).or_insert(base + value);
        }
        assert!(
            !symbols.is_empty(),
            "no CPython symbols in {}; the test harness must not be stripped",
            path.display()
        );
        symbols.into_iter().collect()
    }

    /// Builds and loads a shared object that binds every CPython symbol this
    /// executable defines to its absolute address.
    ///
    /// A fixture compiled against `<Python.h>` imports `Py*` definitions at
    /// load time. Once this object is loaded `RTLD_GLOBAL`, those imports
    /// resolve to the runtime definitions in this process, which is precisely
    /// what `--export-dynamic` provides for the integration fixtures. The
    /// object contains only `.set` absolute symbols; no code or state is
    /// duplicated.
    #[cfg(target_os = "linux")]
    fn load_host_exports() {
        let directory =
            std::env::temp_dir().join(format!("hydro-capi-host-exports-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("host-export directory");
        let source = directory.join("host_exports.S");
        let library = directory.join("host_exports.so");
        let mut assembly =
            String::from("# Generated by the C API tests: absolute host bindings.\n");
        for (name, address) in executable_py_symbols() {
            assembly.push_str(&format!(".globl {name}\n.set {name}, 0x{address:x}\n"));
        }
        assembly.push_str(".section .note.GNU-stack,\"\",@progbits\n");
        std::fs::write(&source, assembly).expect("write the host-export source");
        let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
        let output = Command::new(compiler)
            .args(["-shared", "-nostdlib", "-o"])
            .arg(&library)
            .arg(&source)
            .output()
            .expect("run the C compiler for the host-export shim");
        assert!(
            output.status.success(),
            "the host-export shim failed to compile: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let path = std::ffi::CString::new(library.to_str().expect("host-export path is UTF-8"))
            .expect("host-export path has no NUL");
        // SAFETY: `path` is NUL-terminated and outlives the call. The handle is
        // deliberately kept open: a later `dlclose` would unmap the symbol
        // bindings the fixtures resolved against.
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        assert!(
            !handle.is_null(),
            "the host-export shim failed to load: {}",
            dl_error()
        );
    }

    /// Makes the executable's CPython definitions available to `dlopen`ed
    /// fixtures, once per test process.
    #[cfg(target_os = "linux")]
    fn install_host_exports() {
        static LOADED: std::sync::LazyLock<()> = std::sync::LazyLock::new(load_host_exports);
        let _ = &*LOADED;
    }

    /// Non-ELF hosts rely on `--export-dynamic` for the fixture's imports.
    #[cfg(not(target_os = "linux"))]
    fn install_host_exports() {}

    /// Compiles the probe fixture once. `None` means the environment has no
    /// `python3-config`/compiler; a fixture that fails to compile panics
    /// instead of being reported as a skip.
    fn build_gc_probe() -> Option<PathBuf> {
        let include_output = Command::new("python3-config")
            .arg("--includes")
            .output()
            .ok()?;
        if !include_output.status.success() {
            return None;
        }
        let directory =
            std::env::temp_dir().join(format!("hydro-capi-gc-probe-{}", std::process::id()));
        std::fs::create_dir_all(&directory).ok()?;
        let source = directory.join("gc_probe.c");
        std::fs::write(&source, GC_PROBE_SOURCE).ok()?;
        let output = directory.join("gc_probe.abi3.so");
        let includes = String::from_utf8_lossy(&include_output.stdout);
        let mut command = Command::new("cc");
        command.args(["-shared", "-fPIC", "-O1", "-Wall", "-Wextra", "-pthread"]);
        for include in includes.split_whitespace() {
            command.arg(include);
        }
        command.arg(&source).arg("-o").arg(&output);
        let status = command.status().ok()?;
        assert!(
            status.success(),
            "the GC probe fixture failed to compile: {}",
            source.display()
        );
        Some(output)
    }

    static GC_PROBE: std::sync::LazyLock<Option<PathBuf>> =
        std::sync::LazyLock::new(build_gc_probe);

    /// Loads the probe fixture and returns its module id.
    fn gc_probe_module() -> Option<u64> {
        let path = GC_PROBE.as_deref()?;
        install_host_exports();
        let path = path.to_str().expect("fixture path is UTF-8");
        Some(load_extension(path, "gc_probe").expect("the GC probe fixture must load"))
    }

    fn fixture_call(module: u64, member: &str, args: &[ShimValue]) -> ShimValue {
        match get_attr(module, member) {
            Ok(ShimAttr::Callable(callable)) => call_method(callable.module, callable.method, args)
                .unwrap_or_else(|error| panic!("{member}: {error}")),
            other => panic!("{member} is not callable: {other:?}"),
        }
    }

    fn fixture_int(value: ShimValue) -> i64 {
        match value {
            ShimValue::Int(number) => number,
            other => panic!("expected an int result, got {other:?}"),
        }
    }

    #[derive(Debug)]
    struct GcProbeStats {
        traverse: i64,
        clear: i64,
        finalize: i64,
        children_set: i64,
        children_cleared: i64,
        callbacks: i64,
    }

    fn fixture_stats(module: u64) -> GcProbeStats {
        let value = fixture_call(module, "stats", &[]);
        let items = match value {
            ShimValue::Tuple(items) => items,
            other => panic!("stats must return a tuple, got {other:?}"),
        };
        let field = |index: usize| match items.get(index) {
            Some(ShimValue::Int(number)) => *number,
            other => panic!("stats[{index}] is {other:?}"),
        };
        GcProbeStats {
            traverse: field(0),
            clear: field(1),
            finalize: field(2),
            children_set: field(3),
            children_cleared: field(4),
            callbacks: field(5),
        }
    }

    #[test]
    fn extension_gc_traverse_and_clear_run_exactly_once() {
        anchor();
        let _guard = gc_pass_lock();
        let Some(module) = gc_probe_module() else {
            eprintln!("skipping: no C compiler or Python headers for the fixture");
            return;
        };
        fixture_call(module, "reset_stats", &[]);

        // 1. Default teardown of a GC instance runs the finalizer and one
        //    `tp_clear`, which releases the declared handle.
        fixture_call(module, "teardown", &[]);
        let stats = fixture_stats(module);
        assert_eq!(stats.finalize, 1, "tp_finalize runs once at dealloc");
        assert_eq!(stats.clear, 1, "tp_clear runs once at default dealloc");
        assert_eq!(
            stats.children_cleared, 1,
            "tp_clear must release the stored handle"
        );

        // 2. A two-instance cycle is collected through the exported
        //    `PyGC_Collect`: `tp_traverse` finds the edges and `tp_clear` runs
        //    exactly once per instance.
        fixture_call(module, "reset_stats", &[]);
        fixture_call(module, "make_cycle", &[]);
        let collected = fixture_int(fixture_call(module, "collect", &[]));
        assert!(collected >= 2, "the cycle was not collected: {collected}");
        let stats = fixture_stats(module);
        assert!(
            stats.traverse >= 2,
            "tp_traverse must be invoked by the collector: {stats:?}"
        );
        assert_eq!(stats.children_set, 2, "both edges were declared");
        assert_eq!(
            stats.children_cleared, 2,
            "both handles were released by tp_clear"
        );
        assert_eq!(stats.clear, 2, "tp_clear runs exactly once per instance");
        assert_eq!(stats.finalize, 2, "each instance is deallocated once");

        // 3. A second pass must not clear or free anything again.
        fixture_int(fixture_call(module, "collect", &[]));
        let again = fixture_stats(module);
        assert_eq!(again.clear, 2, "no second tp_clear (double free)");
        assert_eq!(again.finalize, 2, "no second deallocation");

        // 4. A cycle with an external reference is kept, together with the
        //    instance only reachable through it; dropping the reference makes
        //    both collectable.
        fixture_call(module, "reset_stats", &[]);
        fixture_call(module, "keep_cycle", &[]);
        fixture_int(fixture_call(module, "collect", &[]));
        let reachable = fixture_stats(module);
        assert_eq!(
            reachable.clear, 0,
            "a cycle reachable from outside must not be cleared"
        );
        fixture_call(module, "drop_kept", &[]);
        let collected = fixture_int(fixture_call(module, "collect", &[]));
        assert!(
            collected >= 2,
            "the released cycle was not collected: {collected}"
        );
        let released = fixture_stats(module);
        assert_eq!(released.clear, 2);
        assert_eq!(released.finalize, 2);
    }

    /// Hydro callback invoked from the fixture's OS threads under the GIL.
    /// It returns an immediate so no Hydro heap object is allocated on a
    /// thread that is not registered with the collector.
    extern "C" fn stress_callback(_argv: *const i64) -> i64 {
        crate::abi::hydro_int(7)
    }

    #[test]
    fn threaded_extension_callbacks_run_under_the_gil_during_collection() {
        anchor();
        crate::exc::init();
        let _guard = gc_pass_lock();
        let Some(module) = gc_probe_module() else {
            eprintln!("skipping: no C compiler or Python headers for the fixture");
            return;
        };
        fixture_call(module, "reset_stats", &[]);
        let path = GC_PROBE
            .as_deref()
            .and_then(Path::to_str)
            .expect("fixture path is UTF-8")
            .to_string();
        // A Hydro function crosses as a rooted handle: the extension stores it
        // while its threads call back, and the handle must survive the
        // concurrent Hydro collections.
        let handle = crate::abi::hydro_function_new(
            stress_callback as *const u8,
            0,
            b"stress_callback".as_ptr(),
            15,
            0,
        );
        assert_eq!(
            crate::abi::hydro_exc_pending(),
            0,
            "hydro_function_new failed"
        );
        let argv = [handle];

        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = crate::abi::hydro_extension_call(
                path.as_ptr(),
                path.len(),
                b"threaded_call".as_ptr(),
                13,
                argv.len(),
                argv.as_ptr(),
            );
            let _ = sender.send(result);
        });

        // Collect on this thread while the fixture's threads run their
        // callbacks. `handle` stays on this stack, which the collector scans.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut collected_total = 0;
        let result = loop {
            for _ in 0..64 {
                let _ = crate::abi::hydro_str_new(b"gc-stress".as_ptr(), 9);
            }
            collected_total += crate::heap::collect();
            match receiver.recv_timeout(std::time::Duration::from_millis(2)) {
                Ok(result) => break result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "threaded extension callbacks deadlocked"
                    );
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("the extension call thread panicked")
                }
            }
        };
        worker.join().expect("extension call thread");
        assert_eq!(
            crate::abi::hydro_exc_pending(),
            0,
            "the threaded callback reported an error"
        );
        assert_eq!(
            crate::abi::hydro_binop(15, result, handle),
            crate::abi::hydro_true(),
            "the extension must return the same handle it was given"
        );
        assert!(
            collected_total > 0,
            "no Hydro collection ran while the callbacks were in flight"
        );
        let stats = fixture_stats(module);
        assert_eq!(
            stats.callbacks,
            4 * 64,
            "every worker iteration must invoke the callback"
        );
    }

    // -- pinned ecosystem ABI probes --------------------------------------

    /// Locates a pinned wheel extension under `.hydro/ecosystem/site`.
    fn pinned_extension(directory: &str, stem: &str) -> Option<PathBuf> {
        let site = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()?
            .parent()?
            .join(".hydro/ecosystem/site")
            .join(directory);
        for entry in std::fs::read_dir(&site).ok()?.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with(stem) && name.ends_with(".so") {
                return Some(path);
            }
        }
        None
    }

    /// `pydantic_core.SchemaValidator({'type': 'int'}).validate_python(7)`.
    ///
    /// The extension is loaded straight through the C API bridge (not through
    /// the Python importer), so this exercises the same `PyType_FromSpec`
    /// class creation, `tp_new`, vectorcall and argument parsing pydantic-core
    /// uses in production. pydantic-core's classes are GC types whose
    /// `tp_traverse` walks the Python schema object, so the probe also gives
    /// `PyGC_Collect` a real PyO3 type to traverse.
    #[test]
    fn pydantic_core_schema_validator_constructs_and_validates() {
        anchor();
        // The probe creates GC-tracked PyO3 instances; keep the process-wide
        // collection passes away while it constructs them.
        let _guard = gc_pass_lock();
        let Some(path) = pinned_extension("pydantic_core", "_pydantic_core") else {
            eprintln!("skipping: the pinned pydantic-core wheel is not installed");
            return;
        };
        let path = path.to_str().expect("pinned extension path is UTF-8");
        install_host_exports();
        let module = load_extension(path, "_pydantic_core")
            .unwrap_or_else(|error| panic!("pydantic-core initialization failed: {error}"));
        let validator_type = match get_attr(module, "SchemaValidator") {
            Ok(ShimAttr::Type(address)) => address,
            other => panic!("SchemaValidator is not a type: {other:?}"),
        };
        // `core_schema.int_schema()` is exactly this dictionary.
        let schema = ShimValue::Dict(vec![(
            ShimValue::Str("type".to_string()),
            ShimValue::Str("int".to_string()),
        )]);
        let validator = call_type_object(validator_type, &[schema])
            .unwrap_or_else(|error| panic!("SchemaValidator({{'type': 'int'}}) failed: {error}"));
        let validator = value_object(validator);
        assert!(!validator.is_null());
        let validate = PyObject_GetAttrString(validator, c"validate_python".as_ptr());
        assert!(!validate.is_null(), "validate_python: {:?}", take_error());
        let argument = PyLong_FromLong(7);
        let result = PyObject_Vectorcall(validate, &argument, 1, std::ptr::null_mut());
        assert!(!result.is_null(), "validate_python(7): {:?}", take_error());
        assert_eq!(PyLong_AsLong(result), 7);
        Py_DecRef(result);
        Py_DecRef(argument);
        Py_DecRef(validate);
        Py_DecRef(validator);
    }

    /// NumPy array/dtype probe through the pinned `_multiarray_umath`.
    ///
    /// The semantic half of this probe is blocked before any array exists:
    /// `_multiarray_umath_exec` calls `npy_import("numpy._globals",
    /// "_signature_descriptor")` (see
    /// `numpy/_core/src/multiarray/multiarraymodule.c`), and `numpy._globals`
    /// is pure Python. A runtime-only process has no compiled body for it, so
    /// `Py_mod_exec` fails before `PyArrayDTypeMeta_Type`/`PyArray_Type` are
    /// made ready. What can be verified without the Python package is the
    /// runtime ABI surface: the pinned wheel resolves every undefined CPython
    /// symbol against this host and exports its initializer.
    #[test]
    fn numpy_multiarray_links_against_the_host_abi() {
        anchor();
        let Some(path) = pinned_extension("numpy/_core", "_multiarray_umath") else {
            eprintln!("skipping: the pinned NumPy wheel is not installed");
            return;
        };
        let path = std::ffi::CString::new(path.to_str().expect("pinned extension path is UTF-8"))
            .expect("path has no NUL");
        install_host_exports();
        // `RTLD_NOW` is the runtime form of the link-time ABI inventory: an
        // unresolved `Py*` symbol fails the load instead of deferring to the
        // first call. The initializer is not run, so closing is safe.
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        assert!(
            !handle.is_null(),
            "the pinned NumPy extension does not resolve against this host: {}",
            dl_error()
        );
        let initializer = unsafe { libc::dlsym(handle, c"PyInit__multiarray_umath".as_ptr()) };
        assert!(
            !initializer.is_null(),
            "PyInit__multiarray_umath is missing from the pinned wheel"
        );
        unsafe { libc::dlclose(handle) };
    }

    // -- bytearray buffer -------------------------------------------------

    #[test]
    fn bytearray_exports_a_writable_buffer_and_commits_on_release() {
        anchor();
        let bytearray = PyByteArray_FromStringAndSize(c"abc".as_ptr(), 3);
        assert!(!bytearray.is_null());
        assert_eq!(PyObject_CheckBuffer(bytearray), 1);

        let mut view: super::Py_buffer = unsafe { std::mem::zeroed() };
        assert_eq!(
            PyObject_GetBuffer(bytearray, &mut view, PYBUF_WRITABLE),
            0,
            "{:?}",
            take_error()
        );
        assert_eq!(view.len, 3);
        assert_eq!(view.readonly, 0, "PyBUF_WRITABLE yields a writable view");
        assert_eq!(view.itemsize, 1);
        assert_eq!(view.ndim, 1);
        unsafe {
            let data = view.buf as *mut u8;
            *data = b'Z';
            *data.add(2) = b'!';
        }
        PyBuffer_Release(&mut view);

        // Writes through the view are visible after release, both through
        // `PyByteArray_AsString` and the logical byte content.
        let pointer = PyByteArray_AsString(bytearray);
        assert!(!pointer.is_null());
        unsafe {
            assert_eq!(std::slice::from_raw_parts(pointer as *const u8, 3), b"Zb!");
        }

        // A read-only request reports readonly and cannot be modified.
        let mut read_only: super::Py_buffer = unsafe { std::mem::zeroed() };
        assert_eq!(
            PyObject_GetBuffer(bytearray, &mut read_only, PYBUF_SIMPLE),
            0
        );
        assert_eq!(read_only.readonly, 1);
        PyBuffer_Release(&mut read_only);
        Py_DecRef(bytearray);
    }

    // -- datetime.datetime_CAPI -------------------------------------------

    type DateFromDate =
        unsafe extern "C" fn(c_int, c_int, c_int, *mut ShimObject) -> *mut ShimObject;
    type DateTimeFromDateAndTime = unsafe extern "C" fn(
        c_int,
        c_int,
        c_int,
        c_int,
        c_int,
        c_int,
        c_int,
        *mut ShimObject,
        *mut ShimObject,
    ) -> *mut ShimObject;
    type TimeFromTime = unsafe extern "C" fn(
        c_int,
        c_int,
        c_int,
        c_int,
        *mut ShimObject,
        *mut ShimObject,
    ) -> *mut ShimObject;
    type DeltaFromDelta =
        unsafe extern "C" fn(c_int, c_int, c_int, c_int, *mut ShimObject) -> *mut ShimObject;

    fn object_type_check(object: *mut ShimObject, class: *mut ShimObject) -> bool {
        Py_TYPE(object) == class as *mut types::TypeObject
            || types::PyType_IsSubtype(Py_TYPE(object) as *mut ShimObject, class) == 1
    }

    #[test]
    fn datetime_capsule_backs_all_constructors_with_native_classes() {
        anchor();
        let pointer = PyCapsule_Import(c"datetime.datetime_CAPI".as_ptr(), 0);
        assert!(!pointer.is_null(), "{:?}", take_error());
        // SAFETY: the capsule owns a live PyDateTimeCAPI for the process.
        let capi = unsafe { &*(pointer as *const PyDateTimeCAPI) };
        assert_eq!(
            capi.date_type,
            std::ptr::addr_of_mut!(types::DATE_CAPI_TYPE)
        );
        assert_eq!(
            capi.datetime_type,
            std::ptr::addr_of_mut!(types::DATETIME_CAPI_TYPE)
        );
        assert_eq!(
            capi.time_type,
            std::ptr::addr_of_mut!(types::TIME_CAPI_TYPE)
        );
        assert_eq!(
            capi.delta_type,
            std::ptr::addr_of_mut!(types::TIMEDELTA_CAPI_TYPE)
        );
        assert_eq!(
            capi.tzinfo_type,
            std::ptr::addr_of_mut!(types::TZINFO_CAPI_TYPE)
        );
        assert!(
            !capi.timezone_utc.is_null(),
            "TimeZone_UTC is a real tzinfo object"
        );
        assert!(object_type_check(
            capi.timezone_utc,
            capi.tzinfo_type as *mut ShimObject
        ));

        let date_from_date: DateFromDate = unsafe { std::mem::transmute(capi.date_from_date) };
        let date = unsafe { date_from_date(2024, 2, 29, capi.date_type as *mut ShimObject) };
        assert!(!date.is_null(), "{:?}", take_error());
        assert_eq!(
            Py_TYPE(date),
            capi.date_type,
            "PyDate_Check sees the date type"
        );
        assert!(object_type_check(date, capi.date_type as *mut ShimObject));
        let year = PyObject_GetAttrString(date, c"year".as_ptr());
        assert_eq!(PyLong_AsLong(year), 2024);
        Py_DecRef(year);

        let datetime_from_date_and_time: DateTimeFromDateAndTime =
            unsafe { std::mem::transmute(capi.datetime_from_date_and_time) };
        let moment = unsafe {
            datetime_from_date_and_time(
                2024,
                2,
                29,
                1,
                2,
                3,
                4,
                std::ptr::null_mut(),
                capi.datetime_type as *mut ShimObject,
            )
        };
        assert!(!moment.is_null(), "{:?}", take_error());
        assert_eq!(
            Py_TYPE(moment),
            capi.datetime_type,
            "PyDateTime_Check sees the datetime type"
        );
        assert!(
            types::PyType_IsSubtype(
                Py_TYPE(moment) as *mut ShimObject,
                capi.date_type as *mut ShimObject,
            ) == 1,
            "PyDate_Check accepts a datetime through subtyping"
        );

        let time_from_time: TimeFromTime = unsafe { std::mem::transmute(capi.time_from_time) };
        let moment_time = unsafe {
            time_from_time(
                12,
                34,
                56,
                789,
                std::ptr::null_mut(),
                capi.time_type as *mut ShimObject,
            )
        };
        assert!(!moment_time.is_null(), "{:?}", take_error());
        assert_eq!(Py_TYPE(moment_time), capi.time_type);
        let hour = PyObject_GetAttrString(moment_time, c"hour".as_ptr());
        assert_eq!(PyLong_AsLong(hour), 12);
        Py_DecRef(hour);

        let delta_from_delta: DeltaFromDelta =
            unsafe { std::mem::transmute(capi.delta_from_delta) };
        let delta = unsafe { delta_from_delta(1, 2, 3, 1, capi.delta_type as *mut ShimObject) };
        assert!(!delta.is_null(), "{:?}", take_error());
        assert_eq!(
            Py_TYPE(delta),
            capi.delta_type,
            "PyDelta_Check sees the delta type"
        );
        let days = PyObject_GetAttrString(delta, c"days".as_ptr());
        assert_eq!(PyLong_AsLong(days), 1);
        Py_DecRef(days);

        // Fold variants use the native keyword-only `fold` parameter.
        let fold_variant: unsafe extern "C" fn(
            c_int,
            c_int,
            c_int,
            c_int,
            c_int,
            c_int,
            c_int,
            *mut ShimObject,
            c_int,
            *mut ShimObject,
        ) -> *mut ShimObject =
            unsafe { std::mem::transmute(capi.datetime_from_date_and_time_and_fold) };
        let folded = unsafe {
            fold_variant(
                2020,
                1,
                2,
                3,
                4,
                5,
                6,
                std::ptr::null_mut(),
                1,
                capi.datetime_type as *mut ShimObject,
            )
        };
        assert!(!folded.is_null(), "{:?}", take_error());
        let fold = PyObject_GetAttrString(folded, c"fold".as_ptr());
        assert_eq!(PyLong_AsLong(fold), 1);
        Py_DecRef(fold);

        // Timestamp constructors delegate to datetime.fromtimestamp.
        let args = PyTuple_New(1);
        let stamp = PyLong_FromLong(0);
        assert_eq!(PyTuple_SetItem(args, 0, stamp), 0);
        let datetime_from_timestamp: unsafe extern "C" fn(
            *mut ShimObject,
            *mut ShimObject,
            *mut ShimObject,
        ) -> *mut ShimObject = unsafe { std::mem::transmute(capi.datetime_from_timestamp) };
        let epoch = unsafe {
            datetime_from_timestamp(
                capi.datetime_type as *mut ShimObject,
                args,
                std::ptr::null_mut(),
            )
        };
        assert!(!epoch.is_null(), "{:?}", take_error());
        let epoch_year = PyObject_GetAttrString(epoch, c"year".as_ptr());
        assert_eq!(PyLong_AsLong(epoch_year), 1970);
        Py_DecRef(epoch_year);

        let date_from_timestamp: unsafe extern "C" fn(
            *mut ShimObject,
            *mut ShimObject,
        ) -> *mut ShimObject = unsafe { std::mem::transmute(capi.date_from_timestamp) };
        let epoch_date = unsafe { date_from_timestamp(capi.date_type as *mut ShimObject, args) };
        assert!(!epoch_date.is_null(), "{:?}", take_error());
        assert_eq!(Py_TYPE(epoch_date), capi.date_type);
        Py_DecRef(args);

        // Invalid values fail with the native constructor's exception.
        clear_error();
        let invalid = unsafe { date_from_date(2024, 13, 1, capi.date_type as *mut ShimObject) };
        assert!(invalid.is_null());
        let message = take_error().expect("month range error");
        assert!(message.starts_with("ValueError: "), "{message}");

        // The timezone slot builds the native timezone class; PyTZInfo_Check
        // accepts it through the timezone -> tzinfo subtype edge.
        let timezone_from_timezone: unsafe extern "C" fn(
            *mut ShimObject,
            *mut ShimObject,
        ) -> *mut ShimObject = unsafe { std::mem::transmute(capi.timezone_from_timezone) };
        let offset_delta =
            unsafe { delta_from_delta(0, 3_600, 0, 1, capi.delta_type as *mut ShimObject) };
        assert!(!offset_delta.is_null(), "{:?}", take_error());
        let zone = unsafe { timezone_from_timezone(offset_delta, std::ptr::null_mut()) };
        assert!(!zone.is_null(), "{:?}", take_error());
        assert_eq!(
            Py_TYPE(zone),
            std::ptr::addr_of_mut!(types::TIMEZONE_CAPI_TYPE)
        );
        assert!(
            types::PyType_IsSubtype(
                Py_TYPE(zone) as *mut ShimObject,
                capi.tzinfo_type as *mut ShimObject,
            ) == 1,
            "timezone is a tzinfo subclass"
        );
        // An aware datetime accepts the constructed zone as tzinfo.
        let aware = unsafe {
            datetime_from_date_and_time(
                2024,
                1,
                2,
                3,
                4,
                5,
                6,
                zone,
                capi.datetime_type as *mut ShimObject,
            )
        };
        assert!(!aware.is_null(), "{:?}", take_error());

        for object in [
            date,
            moment,
            moment_time,
            delta,
            offset_delta,
            folded,
            epoch,
            epoch_date,
            zone,
            aware,
        ] {
            Py_DecRef(object);
        }
    }

    // -- module association ------------------------------------------------

    #[test]
    fn type_module_association_and_module_state_are_validated() {
        anchor();
        let mut definition = empty_definition();
        definition.m_name = c"probe_stateful".as_ptr();
        definition.m_size = 16;
        let module = PyModule_Create2(&mut definition, 3);
        assert!(!module.is_null(), "{:?}", take_error());

        let mut slots = empty_slots();
        let mut spec = PyTypeSpecMirror {
            name: c"probe.Stateful".as_ptr(),
            basicsize: 0,
            itemsize: 0,
            flags: 0,
            slots: slots.as_mut_ptr(),
        };
        let type_object = PyType_FromModuleAndSpec(module, &mut spec, std::ptr::null_mut());
        assert!(!type_object.is_null(), "{:?}", take_error());
        assert_eq!(PyType_GetModule(type_object), module);
        let state = PyType_GetModuleState(type_object);
        assert!(!state.is_null());
        assert_eq!(state, PyModule_GetState(module));

        // A type created without a module has no association; asking for one
        // reports the gap instead of returning garbage.
        let plain = PyType_FromSpec(&mut spec);
        assert!(!plain.is_null(), "{:?}", take_error());
        clear_error();
        assert!(PyType_GetModule(plain).is_null());
        assert!(take_error().unwrap().contains("type has no module"));
        clear_error();
        assert!(PyType_GetModuleState(plain).is_null());
        assert!(take_error().unwrap().contains("type has no module"));

        // A module with `m_size == 0` has no state block; CPython returns
        // NULL without an exception, and so does this host, for both
        // spellings. No bogus pointer is ever returned.
        let mut stateless = empty_definition();
        stateless.m_name = c"probe_stateless".as_ptr();
        let stateless_module = PyModule_Create2(&mut stateless, 3);
        assert!(!stateless_module.is_null(), "{:?}", take_error());
        let associated =
            PyType_FromModuleAndSpec(stateless_module, &mut spec, std::ptr::null_mut());
        assert!(!associated.is_null(), "{:?}", take_error());
        assert_eq!(PyType_GetModule(associated), stateless_module);
        clear_error();
        assert!(PyType_GetModuleState(associated).is_null());
        assert!(take_error().is_none(), "a stateless module is not an error");
        clear_error();
        assert!(PyModule_GetState(stateless_module).is_null());
        assert!(take_error().is_none(), "a stateless module is not an error");

        // The module association lives in a side table and is removed when the
        // heap type is freed, so a later allocation that reuses the address
        // cannot observe a stale module (and the module reference is dropped).
        for _ in 0..8 {
            let temporary = PyType_FromModuleAndSpec(module, &mut spec, std::ptr::null_mut());
            assert!(!temporary.is_null(), "{:?}", take_error());
            assert_eq!(PyType_GetModule(temporary), module);
            let address = temporary as *mut types::TypeObject;
            assert!(types::type_module(address).is_some());
            Py_DecRef(temporary);
            assert!(
                types::type_module(address).is_none(),
                "the association must be dropped with the type"
            );
        }

        Py_DecRef(plain);
        Py_DecRef(type_object);
        Py_DecRef(associated);
    }

    // -- parser accessors and errors --------------------------------------

    #[test]
    fn parse_tuple_and_keywords_still_work() {
        anchor();
        let args = tuple_object(&[ShimValue::Int(1), ShimValue::Int(2)]);
        let mut first: c_int = 0;
        let mut second: c_int = 0;
        let status = unsafe {
            super::PyArg_ParseTuple(
                args as *mut c_void,
                c"ii:add".as_ptr(),
                &mut first as *mut c_int,
                &mut second as *mut c_int,
            )
        };
        assert_eq!(status, 1, "{:?}", take_error());
        assert_eq!((first, second), (1, 2));

        let names = [c"a".as_ptr(), c"b".as_ptr(), std::ptr::null()];
        let positional = tuple_object(&[ShimValue::Int(3)]);
        let kwargs = PyDict_New();
        assert_eq!(
            PyDict_SetItemString(kwargs, c"b".as_ptr(), value_object(ShimValue::Int(5))),
            0
        );
        first = 0;
        second = 0;
        let status = unsafe {
            super::PyArg_ParseTupleAndKeywords(
                positional as *mut c_void,
                kwargs as *mut c_void,
                c"ii:add".as_ptr(),
                names.as_ptr(),
                &mut first as *mut c_int,
                &mut second as *mut c_int,
            )
        };
        assert_eq!(status, 1, "{:?}", take_error());
        assert_eq!((first, second), (3, 5));

        Py_DecRef(kwargs);
        Py_DecRef(positional);
        Py_DecRef(args);
    }

    #[test]
    fn exception_classes_keep_their_names() {
        anchor();
        clear_error();
        PyErr_SetString(unsafe { PyExc_ValueError }, c"bad".as_ptr());
        assert_eq!(take_error().as_deref(), Some("ValueError: bad"));
        PyErr_SetString(unsafe { PyExc_FileNotFoundError }, c"gone".as_ptr());
        assert_eq!(take_error().as_deref(), Some("FileNotFoundError: gone"));

        let number = PyLong_FromLong(1);
        PyErr_SetString(number, c"odd".as_ptr());
        assert_eq!(take_error().as_deref(), Some("RuntimeError: odd"));
        Py_DecRef(number);

        PyErr_SetObject(
            unsafe { PyExc_KeyError },
            value_object(ShimValue::List(vec![ShimValue::Int(1)])),
        );
        assert_eq!(take_error().as_deref(), Some("KeyError: [1]"));

        assert!(PyErr_NoMemory().is_null());
        assert_eq!(take_error().as_deref(), Some("MemoryError: out of memory"));
        PyErr_BadInternalCall();
        assert_eq!(
            take_error().as_deref(),
            Some("SystemError: bad argument to internal function")
        );
        assert!(!error_pending());
    }

    #[test]
    fn module_slots_run_and_validate() {
        use std::sync::atomic::AtomicUsize;

        static CALLS: AtomicUsize = AtomicUsize::new(0);
        extern "C" fn exec(_module: *mut ShimObject) -> c_int {
            CALLS.fetch_add(1, SEQ);
            0
        }
        let mut slots = [
            PyModuleDefSlotMirror {
                slot: PY_MOD_EXEC,
                value: exec as *const () as *mut c_void,
            },
            PyModuleDefSlotMirror {
                slot: PY_MOD_EXEC,
                value: exec as *const () as *mut c_void,
            },
            PyModuleDefSlotMirror {
                slot: 0,
                value: std::ptr::null_mut(),
            },
        ];
        let mut definition = empty_definition();
        definition.m_slots = slots.as_mut_ptr();
        let (id, module) = create_module_from_definition(&mut definition).expect("module");
        CALLS.store(0, SEQ);
        run_module_slots(&mut definition, module).expect("exec slots");
        assert_eq!(CALLS.load(SEQ), 2);
        let _ = registry_lock().modules.remove(&id);

        let mut invalid = [
            PyModuleDefSlotMirror {
                slot: PY_MOD_GIL,
                value: std::ptr::without_provenance_mut(2),
            },
            PyModuleDefSlotMirror {
                slot: 0,
                value: std::ptr::null_mut(),
            },
        ];
        definition.m_slots = invalid.as_mut_ptr();
        let message = run_module_slots(&mut definition, std::ptr::null_mut())
            .expect_err("invalid GIL policy");
        assert!(message.contains("Py_mod_gil"), "{message}");
    }

    fn empty_definition() -> PyModuleDefMirror {
        PyModuleDefMirror {
            ob_refcnt: 1,
            ob_type: std::ptr::null_mut(),
            m_init: std::ptr::null_mut(),
            m_index: 0,
            m_copy: std::ptr::null_mut(),
            m_name: c"slot_test_module".as_ptr(),
            m_doc: std::ptr::null(),
            m_size: 0,
            m_methods: std::ptr::null_mut(),
            m_slots: std::ptr::null_mut(),
            m_traverse: std::ptr::null_mut(),
            m_clear: std::ptr::null_mut(),
            m_free: std::ptr::null_mut(),
        }
    }

    #[test]
    fn module_definitions_and_attributes_round_trip() {
        anchor();
        let mut definition = empty_definition();
        let (id, module) = create_module_from_definition(&mut definition).expect("module");
        assert_eq!(
            PyModule_AddStringConstant(module, c"tag".as_ptr(), c"abi3".as_ptr()),
            0
        );
        match get_attr(id, "tag") {
            Ok(ShimAttr::Value(ShimValue::Str(text))) => assert_eq!(text, "abi3"),
            other => panic!("unexpected attribute: {other:?}"),
        }
        assert!(get_attr(id, "missing").is_err());

        let mut slots = empty_slots();
        let type_object = make_type(c"probe.Attr", 32, 0, 0, &mut slots);
        assert_eq!(
            PyModule_AddObject(module, c"Thing".as_ptr(), type_object),
            0
        );
        match get_attr(id, "Thing") {
            Ok(ShimAttr::Type(address)) => assert_eq!(address, type_object as usize),
            other => panic!("unexpected attribute: {other:?}"),
        }
        // The module owns the type reference; remove the entry and release it.
        let _ = registry_lock().modules.remove(&id);
        Py_DecRef(type_object);
    }

    #[test]
    fn no_libpython_symbols_are_present() {
        // No libpython is loaded into the test process, so a CPython-only
        // symbol must not resolve.
        let symbol =
            unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"Py_InitializeFromConfig".as_ptr()) };
        assert!(symbol.is_null(), "the runtime must not link libpython");

        // Every exported entry point this runtime claims is a real function
        // address (the linker keeps them through `anchor`).
        anchor();
        let exported: [*const (); 9] = [
            PyType_FromSpec as *const (),
            PyType_GetSlot as *const (),
            PyObject_Vectorcall as *const (),
            PyObject_GetBuffer as *const (),
            PyObject_GC_Track as *const (),
            PyGC_Collect as *const (),
            PyGILState_Ensure as *const (),
            super::PyObject_CallFunctionObjArgs as *const (),
            super::PyArg_ParseTuple as *const (),
        ];
        assert!(exported.iter().all(|pointer| !pointer.is_null()));
    }

    // -- full ABI layer -------------------------------------------------

    #[test]
    fn builtin_type_symbols_match_handle_types() {
        use std::ptr::addr_of_mut;
        anchor();
        let int_object = PyLong_FromLong(1_000_000);
        assert_eq!(
            Py_TYPE(int_object) as usize,
            addr_of_mut!(types::INT_TYPE) as usize
        );
        Py_DecRef(int_object);
        let float_object = PyFloat_FromDouble(1.5);
        assert_eq!(
            Py_TYPE(float_object) as usize,
            addr_of_mut!(types::FLOAT_TYPE) as usize
        );
        Py_DecRef(float_object);
        let text = PyUnicode_FromString(c"x".as_ptr());
        assert_eq!(
            Py_TYPE(text) as usize,
            addr_of_mut!(types::STR_TYPE) as usize
        );
        Py_DecRef(text);
        let bytes = PyBytes_FromStringAndSize(b"x".as_ptr() as *const c_char, 1);
        assert_eq!(
            Py_TYPE(bytes) as usize,
            addr_of_mut!(types::BYTES_TYPE) as usize
        );
        Py_DecRef(bytes);
        let list = PyList_New(0);
        assert_eq!(
            Py_TYPE(list) as usize,
            addr_of_mut!(types::LIST_TYPE) as usize
        );
        Py_DecRef(list);
        let tuple = PyTuple_New(0);
        assert_eq!(
            Py_TYPE(tuple) as usize,
            addr_of_mut!(types::TUPLE_TYPE) as usize
        );
        Py_DecRef(tuple);
        let dict = PyDict_New();
        assert_eq!(
            Py_TYPE(dict) as usize,
            addr_of_mut!(types::DICT_TYPE) as usize
        );
        Py_DecRef(dict);
        assert_eq!(
            Py_TYPE(Py_GetConstantBorrowed(PY_CONSTANT_NONE)) as usize,
            addr_of_mut!(types::NONE_TYPE) as usize
        );
        assert_eq!(
            Py_TYPE(Py_GetConstantBorrowed(PY_CONSTANT_TRUE)) as usize,
            addr_of_mut!(types::BOOL_TYPE) as usize
        );
        // Type objects point at the shared metatype, and the exported
        // `PyType_Type` address is that metatype.
        let object_type = addr_of_mut!(types::OBJECT_TYPE) as *mut ShimObject;
        assert_eq!(
            Py_TYPE(object_type) as usize,
            addr_of_mut!(types::TYPE_TYPE) as usize
        );
        let type_name = PyType_GetName(object_type);
        assert!(!type_name.is_null());
        Py_DecRef(type_name);
        // A heap type gets the same metatype.
        let mut slots = empty_slots();
        let heap = make_type(c"probe.Identity", 32, 0, 0, &mut slots);
        assert_eq!(
            Py_TYPE(heap) as usize,
            addr_of_mut!(types::TYPE_TYPE) as usize
        );
        Py_DecRef(heap);
        // Version and whitespace data symbols carry their documented values.
        assert_eq!(Py_Version, 0x030E_09F0);
        assert_eq!(_Py_ascii_whitespace[0x20], 1);
        assert_eq!(_Py_ascii_whitespace[0x41], 0);
        assert_eq!(unsafe { _PyByteArray_empty_string[0] }, 0);
        // Copy the pointer values out of the `static mut`s instead of taking
        // shared references to them.
        let (none, truth, falsity) = unsafe { (Py_None, Py_True, Py_False) };
        assert!(!none.is_null());
        assert!(!truth.is_null());
        assert!(!falsity.is_null());
        assert_eq!(none, addr_of_mut!(_Py_NoneStruct));
        assert_eq!(truth, addr_of_mut!(_Py_TrueStruct));
        assert_eq!(falsity, addr_of_mut!(_Py_FalseStruct));
    }

    static LAST_SET: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    static GETSET_SETS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static ADD_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static ITEM_INDEX: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(-1);
    static NEXT_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static LAST_OP: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

    extern "C" fn slot_getset_get(
        _self: *mut ShimObject,
        _closure: *mut c_void,
    ) -> *mut ShimObject {
        PyLong_FromLong(123)
    }

    extern "C" fn slot_getset_set(
        _self: *mut ShimObject,
        value: *mut ShimObject,
        _closure: *mut c_void,
    ) -> c_int {
        match as_int(value) {
            Ok(value) => {
                GETSET_SETS.fetch_add(1, SEQ);
                LAST_SET.store(value, SEQ);
                0
            }
            Err(error) => {
                set_error(format!("TypeError: {error}"));
                -1
            }
        }
    }

    extern "C" fn slot_add(left: *mut ShimObject, right: *mut ShimObject) -> *mut ShimObject {
        ADD_CALLS.fetch_add(1, SEQ);
        // The slot is only reachable through the custom type instance.
        if types::type_name(types::type_of(left)) != "probe.Slots" {
            set_error("TypeError: slot add received a non-instance left operand");
            return std::ptr::null_mut();
        }
        match as_int(right) {
            Ok(right) => PyLong_FromLong((right + 10) as c_long),
            Err(error) => {
                set_error(format!("TypeError: {error}"));
                std::ptr::null_mut()
            }
        }
    }

    extern "C" fn slot_negative(object: *mut ShimObject) -> *mut ShimObject {
        match as_int(object) {
            Ok(value) => PyLong_FromLong(-value as c_long),
            Err(error) => {
                set_error(format!("TypeError: {error}"));
                std::ptr::null_mut()
            }
        }
    }

    extern "C" fn slot_bool(_object: *mut ShimObject) -> c_int {
        1
    }

    extern "C" fn slot_length(_object: *mut ShimObject) -> isize {
        3
    }

    extern "C" fn slot_item(_object: *mut ShimObject, index: isize) -> *mut ShimObject {
        ITEM_INDEX.store(index, SEQ);
        PyLong_FromLong(index as c_long * 10)
    }

    extern "C" fn slot_ass_item(
        _object: *mut ShimObject,
        index: isize,
        value: *mut ShimObject,
    ) -> c_int {
        ITEM_INDEX.store(index, SEQ);
        match as_int(value) {
            Ok(value) => {
                LAST_SET.store(value, SEQ);
                0
            }
            Err(error) => {
                set_error(format!("TypeError: {error}"));
                -1
            }
        }
    }

    extern "C" fn slot_mp_length(_object: *mut ShimObject) -> isize {
        4
    }

    extern "C" fn slot_mp_subscript(
        _object: *mut ShimObject,
        key: *mut ShimObject,
    ) -> *mut ShimObject {
        match as_int(key) {
            Ok(key) => PyLong_FromLong((key + 100) as c_long),
            Err(_) => {
                set_error("KeyError: fixture key");
                std::ptr::null_mut()
            }
        }
    }

    extern "C" fn slot_mp_ass_subscript(
        _object: *mut ShimObject,
        key: *mut ShimObject,
        value: *mut ShimObject,
    ) -> c_int {
        if value.is_null() {
            set_error("TypeError: fixture mapping does not delete");
            return -1;
        }
        match (as_int(key), as_int(value)) {
            (Ok(key), Ok(value)) => {
                ITEM_INDEX.store(key as isize, SEQ);
                LAST_SET.store(value, SEQ);
                0
            }
            _ => {
                set_error("TypeError: fixture mapping requires integers");
                -1
            }
        }
    }

    extern "C" fn slot_iter(object: *mut ShimObject) -> *mut ShimObject {
        inc_ref(object);
        object
    }

    extern "C" fn slot_iternext(_object: *mut ShimObject) -> *mut ShimObject {
        let index = NEXT_CALLS.fetch_add(1, SEQ);
        if index < 2 {
            PyLong_FromLong(index as c_long)
        } else {
            std::ptr::null_mut()
        }
    }

    extern "C" fn slot_richcompare(
        left: *mut ShimObject,
        right: *mut ShimObject,
        op: c_int,
    ) -> *mut ShimObject {
        LAST_OP.store(op, SEQ);
        let equal = as_int(left).ok() == as_int(right).ok();
        Py_GetConstant(if equal {
            PY_CONSTANT_TRUE
        } else {
            PY_CONSTANT_FALSE
        })
    }

    extern "C" fn slot_hash(_object: *mut ShimObject) -> isize {
        77
    }

    extern "C" fn slot_call(
        _object: *mut ShimObject,
        _args: *mut ShimObject,
        _kwds: *mut ShimObject,
    ) -> *mut ShimObject {
        PyLong_FromLong(5)
    }

    #[test]
    fn slot_suites_dispatch_through_the_c_function_pointers() {
        anchor();
        static mut MEMBER_VALUE: i64 = 0;
        let mut members = [
            types::PyMemberDefMirror {
                name: c"count".as_ptr(),
                type_: types::T_LONG,
                offset: 16,
                flags: 0,
                doc: std::ptr::null(),
            },
            types::PyMemberDefMirror {
                name: std::ptr::null(),
                type_: 0,
                offset: 0,
                flags: 0,
                doc: std::ptr::null(),
            },
        ];
        let mut getsets = [
            types::PyGetSetDefMirror {
                name: c"computed".as_ptr(),
                get: slot_getset_get as *const () as usize,
                set: slot_getset_set as *const () as usize,
                doc: std::ptr::null(),
                closure: std::ptr::addr_of_mut!(MEMBER_VALUE) as *mut c_void,
            },
            types::PyGetSetDefMirror {
                name: std::ptr::null(),
                get: 0,
                set: 0,
                doc: std::ptr::null(),
                closure: std::ptr::null_mut(),
            },
        ];
        let mut slots = [
            PyTypeSlotMirror {
                slot: 3,
                pfunc: slot_mp_ass_subscript as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 4,
                pfunc: slot_mp_length as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 5,
                pfunc: slot_mp_subscript as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 7,
                pfunc: slot_add as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 9,
                pfunc: slot_bool as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 30,
                pfunc: slot_negative as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 39,
                pfunc: slot_ass_item as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 44,
                pfunc: slot_item as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 45,
                pfunc: slot_length as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 50,
                pfunc: slot_call as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 59,
                pfunc: slot_hash as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 62,
                pfunc: slot_iter as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 63,
                pfunc: slot_iternext as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 67,
                pfunc: slot_richcompare as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 72,
                pfunc: members.as_mut_ptr() as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 73,
                pfunc: getsets.as_mut_ptr() as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: std::ptr::null_mut(),
            },
        ];
        let type_object = make_type(c"probe.Slots", 48, 0, 0, &mut slots);
        let instance = PyType_GenericAlloc(type_object, 0);
        assert!(!instance.is_null(), "{:?}", take_error());

        // Members: zeroed storage, then a real write/read round trip.
        let value = PyObject_GetAttrString(instance, c"count".as_ptr());
        assert!(!value.is_null(), "{:?}", take_error());
        assert_eq!(PyLong_AsLong(value), 0);
        Py_DecRef(value);
        let forty_two = PyLong_FromLong(42);
        assert_eq!(
            PyObject_SetAttrString(instance, c"count".as_ptr(), forty_two),
            0
        );
        let value = PyObject_GetAttrString(instance, c"count".as_ptr());
        assert_eq!(PyLong_AsLong(value), 42);
        Py_DecRef(value);
        Py_DecRef(forty_two);

        // getset getter and setter.
        let computed = PyObject_GetAttrString(instance, c"computed".as_ptr());
        assert!(!computed.is_null(), "{:?}", take_error());
        assert_eq!(PyLong_AsLong(computed), 123);
        Py_DecRef(computed);
        let nine = PyLong_FromLong(9);
        assert_eq!(
            PyObject_SetAttrString(instance, c"computed".as_ptr(), nine),
            0
        );
        assert_eq!(GETSET_SETS.load(SEQ), 1);
        assert_eq!(LAST_SET.load(SEQ), 9);
        Py_DecRef(nine);

        // Number suite.
        ADD_CALLS.store(0, SEQ);
        let left = PyLong_FromLong(2);
        let right = PyLong_FromLong(3);
        // Exercise the custom type's `nb_add` (the instance is the left
        // operand); builtin integers have no slot suite of their own.
        let sum = types::PyNumber_Add(instance, right);
        assert!(!sum.is_null(), "{:?}", take_error());
        assert_eq!(PyLong_AsLong(sum), 13);
        assert_eq!(ADD_CALLS.load(SEQ), 1);
        Py_DecRef(sum);
        let negative = types::PyNumber_Negative(right);
        assert_eq!(PyLong_AsLong(negative), -3);
        Py_DecRef(negative);
        assert_eq!(types::PyObject_IsTrue(instance), 1);
        assert_eq!(types::PyNumber_Check(instance), 1);

        // Sequence suite, including `types::PyObject_Length` precedence over mapping.
        ITEM_INDEX.store(-1, SEQ);
        assert_eq!(types::PySequence_Length(instance), 3);
        assert_eq!(types::PySequence_Size(instance), 3);
        let item = types::PySequence_GetItem(instance, 4);
        assert_eq!(PyLong_AsLong(item), 40);
        assert_eq!(ITEM_INDEX.load(SEQ), 4);
        Py_DecRef(item);
        let seventeen = PyLong_FromLong(17);
        assert_eq!(types::PySequence_SetItem(instance, 2, seventeen), 0);
        assert_eq!(ITEM_INDEX.load(SEQ), 2);
        assert_eq!(LAST_SET.load(SEQ), 17);

        // Mapping suite and the abstract item API.
        assert_eq!(types::PyMapping_Length(instance), 4);
        assert_eq!(types::PyObject_Length(instance), 4);
        let key = PyLong_FromLong(5);
        let mapped = types::PyObject_GetItem(instance, key);
        assert_eq!(PyLong_AsLong(mapped), 105);
        Py_DecRef(mapped);
        let mapped = types::PyMapping_GetItemString(instance, c"x".as_ptr());
        assert!(mapped.is_null());
        assert!(take_error().unwrap().contains("KeyError"));
        assert_eq!(types::PyObject_SetItem(instance, key, seventeen), 0);
        assert_eq!(ITEM_INDEX.load(SEQ), 5);
        Py_DecRef(key);

        // Iteration, including the NULL -> StopIteration protocol.
        NEXT_CALLS.store(0, SEQ);
        let iterator = types::PyObject_GetIter(instance);
        assert!(!iterator.is_null(), "{:?}", take_error());
        assert_eq!(types::PyIter_Check(iterator), 1);
        let first = types::PyIter_Next(iterator);
        assert_eq!(PyLong_AsLong(first), 0);
        Py_DecRef(first);
        let second = types::PyIter_Next(iterator);
        assert_eq!(PyLong_AsLong(second), 1);
        Py_DecRef(second);
        clear_error();
        assert!(types::PyIter_Next(iterator).is_null());
        assert!(take_error().unwrap().contains("StopIteration"));
        Py_DecRef(iterator);

        // Rich comparison and hash.
        let result = types::PyObject_RichCompare(left, left, 2);
        assert!(!result.is_null());
        assert_eq!(types::PyObject_IsTrue(result), 1);
        Py_DecRef(result);
        assert_eq!(types::PyObject_RichCompareBool(left, left, 2), 1);
        assert_eq!(PyObject_Hash(instance), 77);
        LAST_OP.store(-1, SEQ);
        let rich = types::PyObject_RichCompare(instance, instance, 0);
        assert!(!rich.is_null(), "{:?}", take_error());
        assert_eq!(LAST_OP.load(SEQ), 0);
        Py_DecRef(rich);

        // tp_call through the vectorcall dispatch.
        let called = PyObject_Call(instance, std::ptr::null_mut(), std::ptr::null_mut());
        assert!(!called.is_null(), "{:?}", take_error());
        assert_eq!(PyLong_AsLong(called), 5);
        Py_DecRef(called);

        // `PyType_GetSlot` reports every suite slot.
        assert_eq!(
            PyType_GetSlot(type_object, 7),
            slot_add as *const () as *mut c_void
        );
        assert_eq!(
            PyType_GetSlot(type_object, 45),
            slot_length as *const () as *mut c_void
        );
        assert_eq!(
            PyType_GetSlot(type_object, 3),
            slot_mp_ass_subscript as *const () as *mut c_void
        );
        assert_eq!(
            PyType_GetSlot(type_object, 72),
            members.as_mut_ptr() as *mut c_void
        );
        assert_eq!(
            PyType_GetSlot(type_object, 73),
            getsets.as_mut_ptr() as *mut c_void
        );
        assert_eq!(
            PyType_GetSlot(type_object, 67),
            slot_richcompare as *const () as *mut c_void
        );

        Py_DecRef(left);
        Py_DecRef(right);
        Py_DecRef(seventeen);
        Py_DecRef(instance);
        Py_DecRef(type_object);
    }

    #[test]
    fn multiple_bases_use_c3_linearisation() {
        anchor();
        let mut sequence_slots = [
            PyTypeSlotMirror {
                slot: 45,
                pfunc: slot_length as *const () as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: std::ptr::null_mut(),
            },
        ];
        let first = make_type(
            c"probe.First",
            32,
            0,
            PY_TPFLAGS_BASETYPE as c_uint,
            &mut sequence_slots,
        );
        let mut getsets = [
            types::PyGetSetDefMirror {
                name: c"first_attr".as_ptr(),
                get: slot_getset_get as *const () as usize,
                set: 0,
                doc: std::ptr::null(),
                closure: std::ptr::null_mut(),
            },
            types::PyGetSetDefMirror {
                name: std::ptr::null(),
                get: 0,
                set: 0,
                doc: std::ptr::null(),
                closure: std::ptr::null_mut(),
            },
        ];
        let mut getset_slots = [
            PyTypeSlotMirror {
                slot: 73,
                pfunc: getsets.as_mut_ptr() as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: std::ptr::null_mut(),
            },
        ];
        let second = make_type(
            c"probe.Second",
            32,
            0,
            PY_TPFLAGS_BASETYPE as c_uint,
            &mut getset_slots,
        );

        let mut derived_slots = empty_slots();
        let bases = PyTuple_New(2);
        Py_IncRef(first);
        assert_eq!(PyTuple_SetItem(bases, 0, first), 0);
        Py_IncRef(second);
        assert_eq!(PyTuple_SetItem(bases, 1, second), 0);
        let mut spec = PyTypeSpecMirror {
            name: c"probe.Derived".as_ptr(),
            basicsize: 0,
            itemsize: 0,
            flags: PY_TPFLAGS_BASETYPE as c_uint,
            slots: derived_slots.as_mut_ptr(),
        };
        let derived = PyType_FromSpecWithBases(&mut spec, bases);
        assert!(!derived.is_null(), "{:?}", take_error());
        Py_DecRef(bases);

        let instance = PyType_GenericAlloc(derived, 0);
        assert!(!instance.is_null(), "{:?}", take_error());
        assert_eq!(
            types::PySequence_Length(instance),
            3,
            "inherited sequence suite"
        );
        let attribute = PyObject_GetAttrString(instance, c"first_attr".as_ptr());
        assert!(!attribute.is_null(), "{:?}", take_error());
        assert_eq!(PyLong_AsLong(attribute), 123);
        Py_DecRef(attribute);

        // `__mro__` is (Derived, First, Second, object).
        let mro = PyObject_GetAttrString(derived, c"__mro__".as_ptr());
        assert!(!mro.is_null(), "{:?}", take_error());
        assert_eq!(PyTuple_Size(mro), 4);
        assert_eq!(PyTuple_GetItem(mro, 1) as usize, first as usize);
        assert_eq!(PyTuple_GetItem(mro, 2) as usize, second as usize);
        assert_eq!(
            PyTuple_GetItem(mro, 3) as usize,
            std::ptr::addr_of_mut!(types::OBJECT_TYPE) as usize
        );
        Py_DecRef(mro);

        // An inconsistent base order is a C3 conflict, not silent MRO.
        let left_bases = PyTuple_New(2);
        Py_IncRef(first);
        PyTuple_SetItem(left_bases, 0, first);
        Py_IncRef(second);
        PyTuple_SetItem(left_bases, 1, second);
        let mut left_slots = empty_slots();
        let mut left_spec = PyTypeSpecMirror {
            name: c"probe.Left".as_ptr(),
            basicsize: 0,
            itemsize: 0,
            flags: PY_TPFLAGS_BASETYPE as c_uint,
            slots: left_slots.as_mut_ptr(),
        };
        let left = PyType_FromSpecWithBases(&mut left_spec, left_bases);
        assert!(!left.is_null(), "{:?}", take_error());
        Py_DecRef(left_bases);

        let right_bases = PyTuple_New(2);
        Py_IncRef(second);
        PyTuple_SetItem(right_bases, 0, second);
        Py_IncRef(first);
        PyTuple_SetItem(right_bases, 1, first);
        let mut right_slots = empty_slots();
        let mut right_spec = PyTypeSpecMirror {
            name: c"probe.Right".as_ptr(),
            basicsize: 0,
            itemsize: 0,
            flags: PY_TPFLAGS_BASETYPE as c_uint,
            slots: right_slots.as_mut_ptr(),
        };
        let right = PyType_FromSpecWithBases(&mut right_spec, right_bases);
        assert!(!right.is_null(), "{:?}", take_error());
        Py_DecRef(right_bases);

        let conflict_bases = PyTuple_New(2);
        Py_IncRef(left);
        PyTuple_SetItem(conflict_bases, 0, left);
        Py_IncRef(right);
        PyTuple_SetItem(conflict_bases, 1, right);
        let mut conflict_slots = empty_slots();
        let mut conflict_spec = PyTypeSpecMirror {
            name: c"probe.Conflict".as_ptr(),
            basicsize: 0,
            itemsize: 0,
            flags: PY_TPFLAGS_BASETYPE as c_uint,
            slots: conflict_slots.as_mut_ptr(),
        };
        clear_error();
        let conflict = PyType_FromSpecWithBases(&mut conflict_spec, conflict_bases);
        assert!(conflict.is_null());
        let message = take_error().expect("MRO conflict");
        assert!(message.contains("method resolution order"), "{message}");
        Py_DecRef(conflict_bases);

        Py_DecRef(instance);
        Py_DecRef(derived);
        Py_DecRef(left);
        Py_DecRef(right);
        Py_DecRef(first);
        Py_DecRef(second);
        // Keep the method tables alive until the types are gone (they live on
        // the stack of this test until here).
        let _ = &mut getsets;
        let _ = &mut sequence_slots;
        let _ = &mut getset_slots;
        let _ = &mut derived_slots;
        let _ = &mut left_slots;
        let _ = &mut right_slots;
        let _ = &mut conflict_slots;
    }

    #[test]
    fn module_import_and_sys_surface_behaves() {
        anchor();
        let mut methods = [
            PyMethodDefMirror {
                ml_name: c"seven".as_ptr(),
                ml_meth: method_seven as *const () as *mut c_void,
                ml_flags: METH_NOARGS,
                ml_doc: std::ptr::null(),
            },
            PyMethodDefMirror {
                ml_name: std::ptr::null(),
                ml_meth: std::ptr::null_mut(),
                ml_flags: 0,
                ml_doc: std::ptr::null(),
            },
        ];
        let mut definition = empty_definition();
        definition.m_name = c"hydro_abi_fixture".as_ptr();
        definition.m_doc = c"fixture documentation".as_ptr();
        definition.m_size = 16;
        definition.m_methods = methods.as_mut_ptr();
        let module = PyModule_Create2(&mut definition, 3);
        assert!(!module.is_null(), "{:?}", take_error());

        unsafe {
            assert_eq!(
                std::ffi::CStr::from_ptr(PyModule_GetName(module)),
                c"hydro_abi_fixture"
            );
            assert_eq!(
                std::ffi::CStr::from_ptr(PyModule_GetFilename(module)),
                c"hydro_abi_fixture"
            );
        }
        assert_eq!(
            PyModule_GetDef(module) as usize,
            &mut definition as *mut PyModuleDefMirror as usize
        );
        assert!(!PyModule_GetState(module).is_null());
        assert_eq!(PyModule_AddIntConstant(module, c"answer".as_ptr(), 42), 0);
        let dict = PyModule_GetDict(module);
        assert!(!dict.is_null(), "{:?}", take_error());
        assert_eq!(PyModule_GetDict(module), dict, "the dictionary is stable");
        let value = PyDict_GetItemString(dict, c"answer".as_ptr());
        assert_eq!(PyLong_AsLong(value), 42);
        let name = PyModule_GetNameObject(module);
        assert_eq!(as_string(name).as_deref(), Some("hydro_abi_fixture"));
        Py_DecRef(name);
        assert_eq!(PyModule_AddFunctions(module, methods.as_mut_ptr()), 0);
        assert!(!PyDict_GetItemString(dict, c"seven".as_ptr()).is_null());
        // The module's `__doc__` came from the definition.
        let doc = PyDict_GetItemString(dict, c"__doc__".as_ptr());
        assert_eq!(as_string(doc).as_deref(), Some("fixture documentation"));

        let imported = PyImport_ImportModule(c"hydro_abi_fixture".as_ptr());
        assert_eq!(imported, module, "the registry resolves the module by name");
        Py_DecRef(imported);
        let named = PyImport_AddModule(c"hydro_abi_dynamic".as_ptr());
        assert!(!named.is_null(), "{:?}", take_error());
        unsafe {
            assert_eq!(
                std::ffi::CStr::from_ptr(PyModule_GetName(named)),
                c"hydro_abi_dynamic"
            );
        }
        let module_dict = PyImport_GetModuleDict();
        assert!(!module_dict.is_null());
        assert!(!PyDict_GetItemString(module_dict, c"hydro_abi_fixture".as_ptr()).is_null());
        let modules = PySys_GetObject(c"modules".as_ptr());
        assert_eq!(modules, module_dict);
        let probe = PyLong_FromLong(7);
        assert_eq!(PySys_SetObject(c"hydro_probe".as_ptr(), probe), 0);
        Py_DecRef(probe);
        assert_eq!(PyLong_AsLong(PySys_GetObject(c"hydro_probe".as_ptr())), 7);
        let builtins = PyEval_GetBuiltins();
        assert!(!builtins.is_null());
        let int_type = PyDict_GetItemString(builtins, c"int".as_ptr());
        assert_eq!(
            int_type as usize,
            std::ptr::addr_of_mut!(types::INT_TYPE) as usize
        );
        assert!(!PyEval_GetGlobals().is_null());
        assert!(!PyEval_GetLocals().is_null());

        // Unsupported evaluation fails loudly instead of returning a fake
        // result.
        clear_error();
        assert_eq!(PyRun_SimpleString(c"x = 1".as_ptr()), -1);
        assert!(take_error().unwrap().contains("PyRun_SimpleString"));
        clear_error();
        assert!(PyRun_String(
            c"x".as_ptr(),
            256,
            std::ptr::null_mut(),
            std::ptr::null_mut()
        )
        .is_null());
        assert!(take_error().unwrap().contains("PyRun_String"));

        clear_error();
        assert!(PyImport_ImportModule(c"not_a_loaded_module".as_ptr()).is_null());
        assert!(take_error().unwrap().contains("ImportError"));
    }

    /// `PySys_GetObject` exposes the runtime's own `sys` values, not a C-only
    /// stub. Extension module initialisation depends on this: NumPy 2.5 fails
    /// with `cannot get sys.flags` unless `sys.flags.optimize` resolves.
    #[test]
    fn sys_dictionary_exposes_runtime_values() {
        anchor();
        let flags = PySys_GetObject(c"flags".as_ptr());
        assert!(!flags.is_null(), "sys.flags is missing");
        // `npy_static_data.c` reads exactly this field.
        let optimize = PyObject_GetAttrString(flags, c"optimize".as_ptr());
        assert!(
            !optimize.is_null(),
            "sys.flags.optimize: {:?}",
            take_error()
        );
        assert_eq!(PyLong_AsLong(optimize), 0);
        Py_DecRef(optimize);
        for name in [c"debug", c"utf8_mode", c"verbose", c"dev_mode"] {
            let field = PyObject_GetAttrString(flags, name.as_ptr());
            assert!(
                !field.is_null(),
                "sys.flags.{}: {:?}",
                name.to_str().unwrap(),
                take_error()
            );
            Py_DecRef(field);
        }

        let version = PySys_GetObject(c"version".as_ptr());
        assert!(!version.is_null(), "sys.version is missing");
        let text = as_string(version).expect("sys.version is a string");
        assert!(text.contains("3.14") && text.contains("Hydro"), "{text}");

        let platform = PySys_GetObject(c"platform".as_ptr());
        assert!(!platform.is_null(), "sys.platform is missing");
        assert!(!as_string(platform)
            .expect("sys.platform is a string")
            .is_empty());

        let byteorder = PySys_GetObject(c"byteorder".as_ptr());
        assert_eq!(
            as_string(byteorder).as_deref(),
            Some(if cfg!(target_endian = "little") {
                "little"
            } else {
                "big"
            })
        );

        let version_info = PySys_GetObject(c"version_info".as_ptr());
        assert!(!version_info.is_null(), "sys.version_info is missing");
        assert_eq!(PyTuple_Size(version_info), 5);

        let maxsize = PySys_GetObject(c"maxsize".as_ptr());
        assert!(!maxsize.is_null(), "sys.maxsize is missing");
        assert_eq!(PyLong_AsLong(maxsize), crate::value::MAX_INT);

        let implementation = PySys_GetObject(c"implementation".as_ptr());
        assert!(!implementation.is_null(), "sys.implementation is missing");
        let name = PyObject_GetAttrString(implementation, c"name".as_ptr());
        assert_eq!(
            as_string(name).as_deref(),
            Some("hydro"),
            "{:?}",
            take_error()
        );
        Py_DecRef(name);

        // No `-W`/`-X` options are parsed, so the real containers are empty.
        let warnoptions = PySys_GetObject(c"warnoptions".as_ptr());
        assert!(!warnoptions.is_null(), "sys.warnoptions is missing");
        assert_eq!(PyList_Size(warnoptions), 0);
        let xoptions = PySys_GetObject(c"_xoptions".as_ptr());
        assert!(!xoptions.is_null(), "sys._xoptions is missing");
        assert_eq!(PyDict_Size(xoptions), 0);

        assert!(!PySys_GetObject(c"path".as_ptr()).is_null());
        assert!(!PySys_GetObject(c"argv".as_ptr()).is_null());
    }

    #[test]
    fn writers_allocators_and_mutex_work() {
        anchor();
        // Unicode writer.
        let writer = PyUnicodeWriter_Create(8);
        assert!(!writer.is_null());
        assert_eq!(
            PyUnicodeWriter_WriteUTF8(writer, b"ab".as_ptr() as *const c_char, 2),
            0
        );
        assert_eq!(PyUnicodeWriter_WriteChar(writer, 'c' as c_uint), 0);
        let text = PyUnicode_FromString(c"d".as_ptr());
        assert_eq!(PyUnicodeWriter_WriteStr(writer, text), 0);
        Py_DecRef(text);
        let result = PyUnicodeWriter_Finish(writer);
        assert_eq!(as_string(result).as_deref(), Some("abcd"));
        Py_DecRef(result);
        let writer = PyUnicodeWriter_Create(0);
        assert_eq!(
            PyUnicodeWriter_WriteASCII(writer, b"ok".as_ptr() as *const c_char, 2),
            0
        );
        let result = PyUnicodeWriter_Finish(writer);
        assert_eq!(as_string(result).as_deref(), Some("ok"));
        Py_DecRef(result);
        let writer = PyUnicodeWriter_Create(0);
        PyUnicodeWriter_Discard(writer);

        // Long writer: little-endian base-2^30 digits.
        let mut digits: *mut c_void = std::ptr::null_mut();
        let writer = PyLongWriter_Create(0, 2, &mut digits);
        assert!(!writer.is_null());
        assert!(!digits.is_null());
        unsafe {
            *(digits as *mut u32) = 0;
            *(digits as *mut u32).add(1) = 1;
        }
        let value = PyLongWriter_Finish(writer);
        assert_eq!(PyLong_AsLong(value), 1 << 30);
        Py_DecRef(value);
        let writer = PyLongWriter_Create(1, 1, &mut digits);
        unsafe {
            *(digits as *mut u32) = 5;
        }
        let value = PyLongWriter_Finish(writer);
        assert_eq!(PyLong_AsLong(value), -5);
        Py_DecRef(value);
        PyLongWriter_Discard(PyLongWriter_Create(0, 1, std::ptr::null_mut()));

        // Allocators.
        let memory = PyMem_Malloc(16) as *mut u8;
        assert!(!memory.is_null());
        unsafe {
            *memory = 7;
        }
        let memory = PyMem_Realloc(memory as *mut c_void, 32) as *mut u8;
        assert!(!memory.is_null());
        assert_eq!(unsafe { *memory }, 7);
        PyMem_Free(memory as *mut c_void);
        let zeroed = PyObject_Calloc(4, 4) as *const u8;
        assert!(!zeroed.is_null());
        assert_eq!(unsafe { *zeroed.add(15) }, 0);
        PyObject_Free(zeroed as *mut c_void);
        let raw = PyMem_RawMalloc(4);
        assert!(!raw.is_null());
        PyMem_RawFree(raw);

        // PyMutex on CPython's one-byte layout.
        static mut LOCK: u8 = 0;
        let lock = std::ptr::addr_of_mut!(LOCK);
        assert_eq!(PyMutex_IsLocked(lock), 0);
        PyMutex_Lock(lock);
        assert_eq!(PyMutex_IsLocked(lock), 1);
        PyMutex_Unlock(lock);
        assert_eq!(PyMutex_IsLocked(lock), 0);
    }

    #[test]
    fn dlopened_fixture_type_spec_dispatches() {
        // SAFETY: the fixture exports C-layout type tables and callbacks.
        let library =
            unsafe { libc::dlopen(FIXTURE_PATH.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        assert!(!library.is_null(), "dlopen failed: {}", dl_error());
        // SAFETY: symbol names are NUL-terminated.
        let spec_symbol = unsafe { libc::dlsym(library, c"hydro_fixture_spec".as_ptr()) };
        assert!(!spec_symbol.is_null(), "fixture spec symbol");
        let spec_function: unsafe extern "C" fn() -> *mut c_void =
            unsafe { std::mem::transmute(spec_symbol) };
        let spec = unsafe { spec_function() } as *mut PyTypeSpecMirror;
        assert!(!spec.is_null());
        let query = |name: &std::ffi::CStr| -> unsafe extern "C" fn() -> c_long {
            let symbol = unsafe { libc::dlsym(library, name.as_ptr()) };
            assert!(!symbol.is_null(), "fixture symbol {name:?}");
            unsafe { std::mem::transmute(symbol) }
        };
        let getset_sets = query(c"hydro_fixture_getset_sets");
        let next_calls = query(c"hydro_fixture_next_calls");
        let item_index = query(c"hydro_fixture_item_index");
        let last_op = query(c"hydro_fixture_last_op");

        let type_object = PyType_FromSpec(spec);
        assert!(!type_object.is_null(), "{:?}", take_error());
        let instance = PyType_GenericAlloc(type_object, 0);
        assert!(!instance.is_null(), "{:?}", take_error());

        // Member descriptor from the fixture's PyMemberDef table.
        let member = PyObject_GetAttrString(instance, c"count".as_ptr());
        assert_eq!(PyLong_AsLong(member), 0);
        Py_DecRef(member);
        let forty = PyLong_FromLong(40);
        assert_eq!(
            PyObject_SetAttrString(instance, c"count".as_ptr(), forty),
            0
        );
        let member = PyObject_GetAttrString(instance, c"count".as_ptr());
        assert_eq!(PyLong_AsLong(member), 40);
        Py_DecRef(member);
        Py_DecRef(forty);

        // getset descriptor: the getter returns `self` (borrowed), so give the
        // result a reference the caller will own.
        Py_IncRef(instance);
        let getset = PyObject_GetAttrString(instance, c"self_ref".as_ptr());
        assert_eq!(getset, instance);
        Py_DecRef(getset);
        let value = PyLong_FromLong(1);
        assert_eq!(
            PyObject_SetAttrString(instance, c"self_ref".as_ptr(), value),
            0
        );
        assert_eq!(unsafe { getset_sets() }, 1);
        Py_DecRef(value);

        // Number, sequence, iteration, comparison and hash slots.
        let three = PyLong_FromLong(3);
        Py_IncRef(three);
        let sum = types::PyNumber_Add(instance, three);
        assert_eq!(sum, three, "fixture nb_add returns its second operand");
        Py_DecRef(sum);
        assert_eq!(types::PyObject_IsTrue(instance), 1);
        assert_eq!(types::PySequence_Length(instance), 7);
        Py_IncRef(instance);
        let item = types::PySequence_GetItem(instance, 4);
        assert_eq!(item, instance);
        assert_eq!(unsafe { item_index() }, 4);
        Py_DecRef(item);
        assert_eq!(PyObject_Hash(instance), 99);

        Py_IncRef(instance);
        let iterator = types::PyObject_GetIter(instance);
        assert_eq!(iterator, instance);
        assert_eq!(types::PyIter_Check(instance), 1);
        clear_error();
        assert!(types::PyIter_Next(iterator).is_null());
        let error = take_error().unwrap_or_default();
        assert!(error.contains("StopIteration"), "{error:?}");
        assert_eq!(unsafe { next_calls() }, 1);
        Py_DecRef(iterator);

        Py_IncRef(instance);
        let comparison = types::PyObject_RichCompare(instance, instance, 0);
        assert_eq!(comparison, instance);
        assert_eq!(unsafe { last_op() }, 0);
        Py_DecRef(comparison);

        // `PyType_GetSlot` returns the fixture's function pointers.
        let slot_pointer = |slot: c_int| -> *mut c_void {
            let mut cursor = unsafe { (*spec).slots };
            loop {
                let entry = unsafe { &*cursor };
                if entry.slot == 0 {
                    panic!("fixture slot {slot} is missing");
                }
                if entry.slot == slot {
                    return entry.pfunc;
                }
                cursor = unsafe { cursor.add(1) };
            }
        };
        for slot in [7, 9, 44, 45, 59, 62, 63, 67, 72, 73] {
            assert_eq!(PyType_GetSlot(type_object, slot), slot_pointer(slot));
        }

        Py_DecRef(instance);
        Py_DecRef(type_object);
        Py_DecRef(three);
    }

    static FIXTURE_PATH: std::sync::LazyLock<std::ffi::CString> = std::sync::LazyLock::new(|| {
        let directory =
            std::env::temp_dir().join(format!("hydro-capi-fixture-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("fixture directory");
        let source = directory.join("fixture.c");
        let library = directory.join("fixture.so");
        std::fs::write(&source, FIXTURE_SOURCE).expect("write fixture source");
        let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
        let output = std::process::Command::new(compiler)
            .arg("-shared")
            .arg("-fPIC")
            .arg("-O1")
            .arg("-o")
            .arg(&library)
            .arg(&source)
            .output()
            .expect("run the C compiler for the fixture");
        assert!(
            output.status.success(),
            "fixture compilation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::ffi::CString::new(library.to_string_lossy().as_bytes()).expect("fixture path")
    });

    const FIXTURE_SOURCE: &str = r#"
typedef struct PyObject PyObject;
typedef struct { int slot; void *pfunc; } PyType_Slot;
typedef struct {
    const char *name;
    int basicsize;
    int itemsize;
    unsigned int flags;
    PyType_Slot *slots;
} PyType_Spec;
typedef struct {
    const char *name;
    int type;
    long offset;
    int flags;
    const char *doc;
} PyMemberDef;
typedef struct {
    const char *name;
    void *get;
    void *set;
    const char *doc;
    void *closure;
} PyGetSetDef;

static long getset_sets;
static long next_calls;
static long item_index;
static long last_op;

static PyObject *fixture_get(PyObject *self, void *closure)
{
    (void)closure;
    return self;
}
static int fixture_set(PyObject *self, PyObject *value, void *closure)
{
    (void)self; (void)value; (void)closure;
    getset_sets++;
    return 0;
}
static PyObject *fixture_add(PyObject *left, PyObject *right)
{
    (void)left;
    return right;
}
static int fixture_bool(PyObject *self) { (void)self; return 1; }
static long fixture_length(PyObject *self) { (void)self; return 7; }
static PyObject *fixture_item(PyObject *self, long index)
{
    item_index = index;
    return self;
}
static PyObject *fixture_iter(PyObject *self) { return self; }
static PyObject *fixture_next(PyObject *self)
{
    (void)self;
    next_calls++;
    return 0;
}
static PyObject *fixture_richcompare(PyObject *left, PyObject *right, int op)
{
    (void)right;
    last_op = op;
    return left;
}
static long fixture_hash(PyObject *self) { (void)self; return 99; }

static PyMemberDef fixture_members[] = {
    {"count", 2, 16, 0, 0},
    {0, 0, 0, 0, 0},
};
static PyGetSetDef fixture_getsets[] = {
    {"self_ref", (void *)fixture_get, (void *)fixture_set, 0, 0},
    {0, 0, 0, 0, 0},
};
static PyType_Slot fixture_slots[] = {
    {72, fixture_members},
    {73, fixture_getsets},
    {7, (void *)fixture_add},
    {9, (void *)fixture_bool},
    {45, (void *)fixture_length},
    {44, (void *)fixture_item},
    {59, (void *)fixture_hash},
    {62, (void *)fixture_iter},
    {63, (void *)fixture_next},
    {67, (void *)fixture_richcompare},
    {0, 0},
};
static PyType_Spec fixture_spec = {
    "hydro_fixture.Type", 32, 0, 0, fixture_slots,
};

void *hydro_fixture_spec(void) { return &fixture_spec; }
long hydro_fixture_getset_sets(void) { return getset_sets; }
long hydro_fixture_next_calls(void) { return next_calls; }
long hydro_fixture_item_index(void) { return item_index; }
long hydro_fixture_last_op(void) { return last_op; }
"#;

    // -- CPython 3.14 entry points ---------------------------------------

    #[test]
    fn raised_exception_state_round_trips() {
        anchor();
        clear_error();
        assert!(PyErr_GetRaisedException().is_null());

        PyErr_SetString(unsafe { PyExc_ValueError }, c"bad".as_ptr());
        assert!(!PyErr_Occurred().is_null());
        assert_eq!(
            PyErr_GivenExceptionMatches(unsafe { PyExc_ValueError }, unsafe { PyExc_Exception }),
            1
        );
        assert_eq!(
            PyErr_GivenExceptionMatches(unsafe { PyExc_TypeError }, unsafe { PyExc_ValueError }),
            0
        );
        assert_eq!(
            PyErr_GivenExceptionMatches(unsafe { PyExc_ValueError }, unsafe { PyExc_ValueError }),
            1
        );

        let exception = PyErr_GetRaisedException();
        assert!(!exception.is_null(), "no exception materialised");
        assert!(!error_pending());
        match kind_ref(exception) {
            Some(ShimKind::Exception { exc_type, args, .. }) => {
                assert_eq!(types::type_name(*exc_type as *mut TypeObject), "ValueError");
                assert_eq!(args.len(), 1);
                assert_eq!(
                    as_string(args[0] as *mut ShimObject).as_deref(),
                    Some("bad")
                );
            }
            other => panic!("unexpected payload: {}", kind_variant(other)),
        }

        assert!(PyException_GetCause(exception).is_null());
        let cause = new_exception_instance(unsafe { PyExc_KeyError } as usize, Vec::new());
        PyException_SetCause(exception, cause);
        let observed = PyException_GetCause(exception);
        assert_eq!(observed, cause);
        dec_ref(observed);

        assert!(PyException_GetTraceback(exception).is_null());
        let not_a_traceback = new_int(1);
        assert_eq!(PyException_SetTraceback(exception, not_a_traceback), -1);
        assert!(take_error().unwrap_or_default().starts_with("TypeError"));
        assert_eq!(
            PyException_SetTraceback(exception, ptr::addr_of_mut!(_Py_NoneStruct)),
            0
        );
        dec_ref(not_a_traceback);

        PyErr_SetRaisedException(exception);
        assert!(error_pending());
        assert_eq!(take_error().as_deref(), Some("ValueError: bad"));
    }

    #[test]
    fn new_exception_classes_join_the_hierarchy() {
        anchor();
        clear_error();
        let class = PyErr_NewExceptionWithDoc(
            c"mymod.MyError".as_ptr(),
            c"module doc".as_ptr(),
            unsafe { PyExc_Exception },
            ptr::null_mut(),
        );
        assert!(!class.is_null(), "{:?}", take_error());
        assert_eq!(types::type_name(class as *mut TypeObject), "mymod.MyError");
        assert_eq!(PyObject_IsSubclass(class, unsafe { PyExc_Exception }), 1);
        assert_eq!(
            PyErr_GivenExceptionMatches(class, unsafe { PyExc_Exception }),
            1
        );
        let doc = PyObject_GetAttrString(class, c"__doc__".as_ptr());
        assert_eq!(as_string(doc).as_deref(), Some("module doc"));
        dec_ref(doc);

        let message = new_str("boom".to_string());
        let instance = PyObject_CallOneArg(class, message);
        dec_ref(message);
        assert!(!instance.is_null(), "{:?}", take_error());
        assert_eq!(exception_prefix(instance), "mymod.MyError");
        dec_ref(instance);

        let missing_dot = PyErr_NewExceptionWithDoc(
            c"NoModule".as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null_mut(),
        );
        assert!(missing_dot.is_null());
        assert!(take_error().unwrap_or_default().starts_with("SystemError"));
    }

    #[test]
    fn dict_api_operates_on_shim_dicts() {
        anchor();
        clear_error();
        let dict = PyDict_New();
        let key = new_str("a".to_string());
        let value = new_int(1);
        assert_eq!(PyDict_SetItem(dict, key, value), 0);
        assert_eq!(PyDict_Contains(dict, key), 1);

        let mut found: *mut ShimObject = ptr::null_mut();
        assert_eq!(PyDict_GetItemRef(dict, key, &mut found), 1);
        assert_eq!(found, value);
        dec_ref(found);
        let missing = new_str("zzz".to_string());
        found = new_int(9);
        assert_eq!(PyDict_GetItemRef(dict, missing, &mut found), 0);
        assert!(found.is_null());

        let copy = PyDict_Copy(dict);
        assert_eq!(PyDict_Contains(copy, key), 1);
        let keys = PyDict_Keys(dict);
        assert_eq!(PyList_Size(keys), 1);
        assert_eq!(PyList_GetItem(keys, 0), key);

        let mut position: isize = 0;
        let (mut iter_key, mut iter_value) = (ptr::null_mut(), ptr::null_mut());
        assert_eq!(
            PyDict_Next(dict, &mut position, &mut iter_key, &mut iter_value),
            1
        );
        assert_eq!((iter_key, iter_value), (key, value));
        assert_eq!(
            PyDict_Next(dict, &mut position, &mut iter_key, &mut iter_value),
            0
        );

        let extra_key = new_str("b".to_string());
        let extra_value = new_int(2);
        assert_eq!(PyDict_SetItem(copy, extra_key, extra_value), 0);
        assert_eq!(PyDict_Update(dict, copy), 0);
        assert_eq!(PyDict_Contains(dict, extra_key), 1);
        assert_eq!(PyDict_DelItem(dict, extra_key), 0);
        assert_eq!(PyDict_DelItem(dict, extra_key), -1);
        assert!(take_error().unwrap_or_default().starts_with("KeyError"));

        let unhashable = PyList_New(0);
        assert_eq!(PyDict_Contains(dict, unhashable), -1);
        assert!(take_error().unwrap_or_default().starts_with("TypeError"));

        let items = PyMapping_Items(dict);
        assert_eq!(PyList_Size(items), 1);

        PyDict_Clear(dict);
        assert_eq!(PyList_Size(PyDict_Keys(dict)), 0);

        for object in [
            dict,
            copy,
            keys,
            items,
            key,
            value,
            extra_key,
            extra_value,
            missing,
            unhashable,
        ] {
            dec_ref(object);
        }
    }

    #[test]
    fn set_list_and_complex_helpers() {
        anchor();
        clear_error();
        let list = PyList_New(0);
        for value in 1..=3 {
            let item = new_int(value);
            assert_eq!(PyList_Append(list, item), 0);
            dec_ref(item);
        }
        let tuple = PyList_AsTuple(list);
        assert_eq!(PyTuple_Size(tuple), 3);
        let slice = PyList_GetSlice(list, 1, 2);
        assert_eq!(PyList_Size(slice), 1);
        assert_eq!(as_int(PyList_GetItem(slice, 0)).unwrap(), 2);

        let set = PySet_New(list);
        assert_eq!(PySet_Size(set), 3);
        let two = new_int(2);
        assert_eq!(PySet_Contains(set, two), 1);
        let four = new_int(4);
        assert_eq!(PySet_Add(set, four), 0);
        assert_eq!(PySet_Size(set), 4);
        assert_eq!(PySet_Contains(set, four), 1);
        let five = new_int(5);
        assert_eq!(PySet_Contains(set, five), 0);

        let frozen = PyFrozenSet_New(list);
        assert_eq!(PySet_Size(frozen), 3);
        assert_eq!(PySet_Add(frozen, five), -1);
        assert_eq!(
            take_error().unwrap_or_default(),
            "SystemError: <class 'frozenset'> is not mutable"
        );
        assert_eq!(PySet_Size(frozen), 3);
        assert_eq!(PySet_Contains(frozen, five), 0);

        let complex = PyComplex_FromDoubles(1.5, -2.5);
        assert_eq!(PyComplex_RealAsDouble(complex), 1.5);
        assert_eq!(PyComplex_ImagAsDouble(complex), -2.5);
        let integer = new_int(7);
        assert_eq!(PyComplex_RealAsDouble(integer), 7.0);
        assert_eq!(PyComplex_ImagAsDouble(integer), 0.0);

        let bytes = PyByteArray_FromStringAndSize(c"abc".as_ptr(), 3);
        assert_eq!(PyByteArray_Size(bytes), 3);
        let buffer = PyByteArray_AsString(bytes);
        // SAFETY: bytearray storage is NUL terminated.
        assert_eq!(unsafe { CStr::from_ptr(buffer) }.to_str().unwrap(), "abc");
        // SAFETY: three data bytes plus the terminator are live.
        assert_eq!(
            unsafe { std::slice::from_raw_parts(buffer as *const u8, 4) },
            &b"abc\0"[..]
        );

        for object in [
            list, tuple, slice, set, frozen, complex, integer, bytes, two, four, five,
        ] {
            dec_ref(object);
        }
    }

    #[test]
    fn long_native_bytes_and_export() {
        anchor();
        clear_error();
        let layout = unsafe { &*PyLong_GetNativeLayout() };
        assert_eq!(layout.bits_per_digit, 30);
        assert_eq!(layout.digit_size, 4);
        assert_eq!(layout.digits_order, -1);
        assert_eq!(
            layout.digit_endianness,
            if cfg!(target_endian = "little") {
                -1
            } else {
                1
            }
        );

        let number = new_int(300);
        let mut exported = PyLongExportMirror {
            value: 0,
            negative: 1,
            ndigits: 5,
            digits: ptr::null(),
            reserved: 0,
        };
        assert_eq!(PyLong_Export(number, &mut exported), 0);
        assert_eq!(exported.value, 300);
        assert_eq!((exported.negative, exported.ndigits), (0, 0));
        assert!(exported.digits.is_null());
        PyLong_FreeExport(&mut exported);

        let mut buffer = [0u8; 8];
        assert_eq!(
            PyLong_AsNativeBytes(number, buffer.as_mut_ptr() as *mut c_void, 1, -1),
            8
        );
        assert_eq!(buffer[0], 0x2c);
        assert_eq!(
            PyLong_AsNativeBytes(number, buffer.as_mut_ptr() as *mut c_void, 2, -1),
            2
        );
        assert_eq!(buffer[..2].to_vec(), vec![0x2c, 0x01]);

        let little = PyLong_FromNativeBytes(buffer.as_ptr() as *const c_void, 2, 1);
        assert_eq!(as_int(little).unwrap(), 300);
        dec_ref(little);
        let negative = [0xffu8];
        let minus_one = PyLong_FromNativeBytes(negative.as_ptr() as *const c_void, 1, 1);
        assert_eq!(as_int(minus_one).unwrap(), -1);
        dec_ref(minus_one);

        let big = new_int(1 << 40);
        assert_eq!(
            PyLong_AsNativeBytes(big, ptr::null_mut(), 0, -1),
            6,
            "required size of a two-digit value"
        );
        dec_ref(big);

        let negative_number = new_int(-1);
        assert_eq!(PyLong_AsUnsignedLongLong(negative_number), u64::MAX);
        assert!(take_error().unwrap_or_default().contains("negative"));
        dec_ref(negative_number);

        assert!(PyLong_FromUnsignedLongLong(u64::MAX).is_null());
        assert!(take_error()
            .unwrap_or_default()
            .starts_with("OverflowError"));
        let fits = PyLong_FromUnsignedLongLong(7);
        assert_eq!(as_int(fits).unwrap(), 7);
        dec_ref(fits);
        let ssize = PyLong_FromSsize_t(-9);
        assert_eq!(as_int(ssize).unwrap(), -9);

        let not_int = new_str("x".to_string());
        assert_eq!(PyLong_Export(not_int, &mut exported), -1);
        assert!(take_error().unwrap_or_default().starts_with("TypeError"));

        for object in [number, ssize, not_int] {
            dec_ref(object);
        }
    }

    #[test]
    fn unicode_storage_encoding_and_interning() {
        anchor();
        clear_error();
        let source = "héllo".as_bytes();
        let text = PyUnicode_FromStringAndSize(source.as_ptr() as *const c_char, 6);
        assert!(!text.is_null(), "{:?}", take_error());
        let mut length: isize = 0;
        let utf8 = PyUnicode_AsUTF8AndSize(text, &mut length);
        assert_eq!(length, 6);
        // SAFETY: the returned buffer holds `length` bytes plus a NUL.
        assert_eq!(
            unsafe { std::slice::from_raw_parts(utf8 as *const u8, length as usize) },
            source
        );
        assert_eq!(
            PyUnicode_EqualToUTF8AndSize(text, source.as_ptr() as *const c_char, 6),
            1
        );
        assert_eq!(PyUnicode_EqualToUTF8AndSize(text, c"hallo".as_ptr(), 5), 0);

        let latin = PyUnicode_AsEncodedString(text, c"latin-1".as_ptr(), ptr::null());
        assert!(!latin.is_null(), "{:?}", take_error());
        assert_eq!(bytes_data(latin).unwrap(), &b"h\xe9llo"[..]);
        dec_ref(latin);

        let ascii = PyUnicode_AsEncodedString(text, c"ascii".as_ptr(), ptr::null());
        assert!(ascii.is_null());
        assert!(take_error()
            .unwrap_or_default()
            .starts_with("UnicodeEncodeError"));

        let utf16 = PyUnicode_AsEncodedString(text, c"utf-16".as_ptr(), c"replace".as_ptr());
        assert_eq!(bytes_data(utf16).unwrap()[..2].to_vec(), vec![0xFF, 0xFE]);
        dec_ref(utf16);

        // `PyUnicode_New` exposes writable UCS-2 storage.
        let fresh = PyUnicode_New(3, 0xFFFF);
        assert!(!fresh.is_null(), "{:?}", take_error());
        let data = PyUnicode_DATA(fresh) as *mut u16;
        for (index, unit) in [b'h' as u16, b'e' as u16, b'y' as u16]
            .into_iter()
            .enumerate()
        {
            // SAFETY: three UCS-2 units were allocated.
            unsafe { *data.add(index) = unit };
        }
        let mut fresh_length: isize = 0;
        let fresh_utf8 = PyUnicode_AsUTF8AndSize(fresh, &mut fresh_length);
        assert_eq!(fresh_length, 3);
        // SAFETY: the UTF-8 buffer holds the three bytes plus a NUL.
        assert_eq!(
            unsafe { std::slice::from_raw_parts(fresh_utf8 as *const u8, 3) },
            b"hey"
        );

        let first = new_str("intern-me".to_string());
        let second = new_str("intern-me".to_string());
        let mut first_slot = first;
        let mut second_slot = second;
        PyUnicode_InternInPlace(&mut first_slot);
        PyUnicode_InternInPlace(&mut second_slot);
        assert_eq!(first_slot, second_slot);

        let error = PyUnicodeDecodeError_Create(
            c"utf-8".as_ptr(),
            b"\xff".as_ptr() as *const c_char,
            1,
            0,
            1,
            c"invalid start byte".as_ptr(),
        );
        assert!(!error.is_null(), "{:?}", take_error());
        match kind_ref(error) {
            Some(ShimKind::Exception { exc_type, args, .. }) => {
                assert_eq!(
                    types::type_name(*exc_type as *mut TypeObject),
                    "UnicodeDecodeError"
                );
                assert_eq!(args.len(), 5);
            }
            other => panic!("unexpected payload: {}", kind_variant(other)),
        }

        let invalid = PyUnicode_FromStringAndSize(b"\xff".as_ptr() as *const c_char, 1);
        assert!(invalid.is_null());
        assert!(take_error()
            .unwrap_or_default()
            .starts_with("UnicodeDecodeError"));

        for object in [text, fresh, error] {
            dec_ref(object);
        }
    }

    #[test]
    fn capsule_import_resolves_loaded_module_attributes() {
        anchor();
        clear_error();
        let (_, module) = create_named_module("hydro_capsule_test".to_string()).expect("module");
        add_attribute(
            module,
            "pointer".to_string(),
            ShimAttr::Value(ShimValue::Capsule {
                ptr: 0x1234,
                name: Some("hydro_capsule_test.pointer".to_string()),
            }),
        )
        .expect("attribute");

        let pointer = PyCapsule_Import(c"hydro_capsule_test.pointer".as_ptr(), 0);
        assert_eq!(pointer as usize, 0x1234, "{:?}", take_error());

        let wrong_name = PyCapsule_Import(c"hydro_capsule_test.missing".as_ptr(), 0);
        assert!(wrong_name.is_null());
        assert_eq!(
            take_error().as_deref(),
            Some("AttributeError: module 'hydro_capsule_test' has no attribute 'missing'")
        );

        let unknown_module = PyCapsule_Import(c"hydro_no_such_module.ptr".as_ptr(), 0);
        assert!(unknown_module.is_null());
        assert_eq!(
            take_error().as_deref(),
            Some("ImportError: PyCapsule_Import could not import module \"hydro_no_such_module\"")
        );
    }

    #[test]
    fn import_module_resolves_runtime_modules_and_calls_them() {
        anchor();
        clear_error();
        assert!(crate::modules::registry_lookup("math").is_some());
        let module = PyImport_ImportModule(c"math".as_ptr());
        assert!(!module.is_null(), "{:?}", take_error());
        let gcd = PyObject_GetAttrString(module, c"gcd".as_ptr());
        assert!(!gcd.is_null(), "{:?}", take_error());
        assert!(matches!(
            kind_ref(gcd),
            Some(ShimKind::RuntimeFunction { .. })
        ));

        let arguments = [new_int(12), new_int(18)];
        let result = PyObject_Vectorcall(gcd, arguments.as_ptr(), 2, ptr::null_mut());
        assert!(!result.is_null(), "{:?}", take_error());
        assert_eq!(as_int(result).unwrap(), 6);
        dec_ref(result);
        for argument in arguments {
            dec_ref(argument);
        }

        let missing = PyImport_ImportModule(c"hydro_no_such_module".as_ptr());
        assert!(missing.is_null());
        assert_eq!(
            take_error().as_deref(),
            Some("ImportError: No module named 'hydro_no_such_module'")
        );

        dec_ref(gcd);
        dec_ref(module);
    }

    /// Serialises the on-demand initializer tests with `hydro_runtime_init`
    /// calls in the ABI tests: the reset runs while holding the heap reset
    /// lock, which it would need to clear the process-wide registry.
    fn initializer_test_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::heap::heap_reset_test_guard()
    }

    static ON_DEMAND_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    unsafe extern "C" fn on_demand_module_body() -> i64 {
        ON_DEMAND_RUNS.fetch_add(1, SEQ);
        crate::value::NONE
    }

    #[test]
    fn import_initializes_a_compiled_module_on_demand() {
        let _guard = initializer_test_guard();
        anchor();
        clear_error();
        crate::module_init::register("hydro_dynamic_import_probe", on_demand_module_body);
        ON_DEMAND_RUNS.store(0, SEQ);

        let module = PyImport_ImportModule(c"hydro_dynamic_import_probe".as_ptr());
        assert!(!module.is_null(), "{:?}", take_error());
        assert_eq!(
            ON_DEMAND_RUNS.load(SEQ),
            1,
            "the compiled module body must run before the module is returned"
        );
        // The module value and the extension-visible module object now exist.
        assert!(crate::modules::registry_lookup("hydro_dynamic_import_probe").is_some());
        assert_eq!(
            crate::module_init::initialize("hydro_dynamic_import_probe"),
            0,
            "an import must not re-run the body"
        );

        // A second import resolves through the extension module registry.
        let again = PyImport_ImportModule(c"hydro_dynamic_import_probe".as_ptr());
        assert!(!again.is_null(), "{:?}", take_error());
        assert_eq!(ON_DEMAND_RUNS.load(SEQ), 1);
        dec_ref(again);
        dec_ref(module);
    }

    unsafe extern "C" fn failing_module_body() -> i64 {
        crate::exc::raise_with_message(crate::exc::VALUE_ERROR, "on-demand import failed")
    }

    unsafe extern "C" fn dotted_submodule_body() -> i64 {
        crate::value::NONE
    }

    #[test]
    fn import_publishes_a_compiled_submodule_on_its_parent() {
        let _guard = initializer_test_guard();
        anchor();
        clear_error();
        crate::exc::clear();
        // The parent package is deliberately not registered: CPython binds a
        // submodule on a partially initialized parent too, and compiled code
        // reads `from pkg import sub` from that package's global table.
        crate::module_init::register("hydro_publish_parent.child", dotted_submodule_body);

        let module = PyImport_ImportModule(c"hydro_publish_parent.child".as_ptr());
        assert!(!module.is_null(), "{:?}", take_error());
        let child = crate::modules::registry_lookup("hydro_publish_parent.child")
            .expect("the imported submodule is registered in sys.modules");
        let published = crate::core::globals::lookup("hydro_publish_parent", "child")
            .expect("the import must bind the submodule on its parent package");
        assert_eq!(
            published, child,
            "the parent attribute is the imported module value"
        );

        // A second import resolves the C module registry and must keep the
        // binding identical, not replace it with a fresh module object.
        let again = PyImport_ImportModule(c"hydro_publish_parent.child".as_ptr());
        assert!(!again.is_null(), "{:?}", take_error());
        assert_eq!(
            crate::core::globals::lookup("hydro_publish_parent", "child"),
            Some(child)
        );

        dec_ref(again);
        dec_ref(module);
    }

    #[test]
    fn import_propagates_an_initializer_exception() {
        let _guard = initializer_test_guard();
        anchor();
        clear_error();
        crate::exc::clear();
        crate::module_init::register("hydro_dynamic_import_failing", failing_module_body);

        let module = PyImport_ImportModule(c"hydro_dynamic_import_failing".as_ptr());
        assert!(module.is_null(), "a failing initializer yields no module");
        assert_eq!(
            take_error().as_deref(),
            Some("ValueError: on-demand import failed")
        );
        assert!(
            !crate::exc::pending(),
            "the Hydro exception was translated into the C API error"
        );
        assert!(
            crate::modules::registry_lookup("hydro_dynamic_import_failing").is_none(),
            "a failed module body publishes no module value"
        );
    }

    #[test]
    fn object_protocol_helpers() {
        anchor();
        clear_error();
        let instance =
            new_exception_instance(unsafe { PyExc_FileNotFoundError } as usize, Vec::new());
        assert_eq!(PyObject_IsInstance(instance, unsafe { PyExc_OSError }), 1);
        assert_eq!(
            PyObject_IsInstance(instance, unsafe { PyExc_ValueError }),
            0
        );
        assert_eq!(
            PyObject_IsSubclass(unsafe { PyExc_FileNotFoundError }, unsafe { PyExc_OSError }),
            1
        );
        let tuple_of_classes = PyTuple_New(2);
        inc_ref(unsafe { PyExc_ValueError });
        assert_eq!(
            PyTuple_SetItem(tuple_of_classes, 0, unsafe { PyExc_ValueError },),
            0
        );
        inc_ref(unsafe { PyExc_OSError });
        assert_eq!(
            PyTuple_SetItem(tuple_of_classes, 1, unsafe { PyExc_OSError }),
            0
        );
        assert_eq!(PyObject_IsInstance(instance, tuple_of_classes), 1);
        dec_ref(tuple_of_classes);

        let list = PyList_New(0);
        assert_eq!(PyObject_LengthHint(list, 9), 0);
        for value in 1..=3 {
            let item = new_int(value);
            PyList_Append(list, item);
            dec_ref(item);
        }
        assert_eq!(PyObject_LengthHint(list, 9), 3);
        assert_eq!(PyObject_LengthHint(ptr::addr_of_mut!(_Py_NoneStruct), 9), 9);

        let mut item: *mut ShimObject = ptr::null_mut();
        assert_eq!(PyIter_NextItem(list, &mut item), -1);
        assert!(item.is_null());
        assert!(take_error().unwrap_or_default().starts_with("TypeError"));

        let (_, module) = create_named_module("hydro_dir_test".to_string()).expect("module");
        assert_eq!(
            PyObject_HasAttrWithError(module, new_str("__name__".to_string())),
            1
        );
        assert_eq!(
            PyObject_HasAttrWithError(module, new_str("absent".to_string())),
            0
        );
        let names = PyObject_Dir(module);
        assert!(PyList_Size(names) >= 1);
        let dict = PyObject_GenericGetDict(module, ptr::null_mut());
        assert!(!dict.is_null());
        assert_eq!(
            PyObject_GenericSetDict(module, ptr::addr_of_mut!(_Py_NoneStruct), ptr::null_mut()),
            -1
        );
        assert!(take_error().unwrap_or_default().starts_with("TypeError"));

        assert_eq!(PyTraceBack_Print(ptr::null_mut(), ptr::null_mut()), 0);
        assert_eq!(Py_IsInitialized(), 1);
        assert_eq!(Py_IsFinalizing(), 0);
        let interpreter = PyInterpreterState_Get();
        assert!(!interpreter.is_null());
        assert_eq!(PyInterpreterState_GetID(interpreter), 0);
        assert_eq!(PyInterpreterState_GetID(ptr::null_mut()), -1);
        assert!(take_error().unwrap_or_default().starts_with("ValueError"));

        // `_Py_Dealloc` releases a uniquely owned handle.
        let doomed = new_int(123_456);
        _Py_Dealloc(doomed);

        for object in [instance, list, names, dict] {
            dec_ref(object);
        }
    }

    #[test]
    fn cmethod_new_validates_flags_and_class() {
        anchor();
        clear_error();
        extern "C" fn noop(_self: *mut ShimObject, _args: *mut ShimObject) -> *mut ShimObject {
            ptr::addr_of_mut!(_Py_NoneStruct)
        }
        let mut definition = PyMethodDefMirror {
            ml_name: c"noop".as_ptr(),
            ml_meth: noop as *const () as *mut c_void,
            ml_flags: METH_NOARGS,
            ml_doc: ptr::null(),
        };
        // Exception classes are real type objects; `PyCMethod_New` needs the
        // type handle, not the class instance pointer.
        let class = unsafe { PyExc_ValueError } as *mut PyTypeObject;
        assert!(!class.is_null());
        assert!(PyCMethod_New(&mut definition, ptr::null_mut(), ptr::null_mut(), class).is_null());
        assert!(take_error().unwrap_or_default().starts_with("SystemError"));

        let function = PyCMethod_New(
            &mut definition,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        );
        assert!(!function.is_null(), "{:?}", take_error());
        dec_ref(function);

        definition.ml_flags = METH_METHOD | METH_FASTCALL | METH_KEYWORDS;
        assert!(PyCMethod_New(
            &mut definition,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut()
        )
        .is_null());
        assert!(take_error().unwrap_or_default().starts_with("SystemError"));
        let method = PyCMethod_New(&mut definition, ptr::null_mut(), ptr::null_mut(), class);
        assert!(!method.is_null(), "{:?}", take_error());
        dec_ref(method);
    }

    #[test]
    fn slice_sequence_and_iterator_surface() {
        anchor();
        clear_error();
        let stop = new_int(5);
        let slice = PySlice_New(ptr::null_mut(), stop, ptr::null_mut());
        dec_ref(stop);
        assert!(!slice.is_null());
        let (mut start, mut stop_value, mut step) = (0, 0, 0);
        assert_eq!(
            PySlice_Unpack(slice, &mut start, &mut stop_value, &mut step),
            0
        );
        assert_eq!((start, stop_value, step), (0, 5, 1));
        let mut low = -3isize;
        let mut high = -1isize;
        assert_eq!(PySlice_AdjustIndices(10, &mut low, &mut high, 1), 2);
        assert_eq!((low, high), (7, 9));
        dec_ref(slice);

        let one = new_int(1);
        let two = new_int(2);
        let values = [one, two];
        let tuple = hydro_capi_tuple_pack(2, values.as_ptr());
        assert!(!tuple.is_null(), "{:?}", take_error());
        assert_eq!(PyTuple_Size(tuple), 2);
        dec_ref(one);
        dec_ref(two);

        let fast = PySequence_Fast(tuple, c"expected a sequence".as_ptr());
        assert_eq!(fast, tuple);
        assert_eq!(Py_REFCNT(tuple), 2);
        dec_ref(fast);

        let slice_tuple = PyTuple_GetSlice(tuple, 1, 2);
        assert_eq!(PyTuple_Size(slice_tuple), 1);
        assert_eq!(as_int(PyTuple_GetItem(slice_tuple, 0)).unwrap(), 2);
        dec_ref(slice_tuple);

        let list = PyList_New(0);
        assert_eq!(PyList_Append(list, one), 0);
        let copied = PySequence_Tuple(list);
        assert_eq!(PyTuple_Size(copied), 1);
        dec_ref(copied);
        dec_ref(list);

        let iterator = PySeqIter_New(tuple);
        assert!(!iterator.is_null());
        let mut item: *mut ShimObject = ptr::null_mut();
        assert_eq!(PyIter_NextItem(iterator, &mut item), 1);
        assert_eq!(as_int(item).unwrap(), 1);
        dec_ref(item);
        assert_eq!(PyIter_NextItem(iterator, &mut item), 1);
        assert_eq!(as_int(item).unwrap(), 2);
        dec_ref(item);
        assert_eq!(PyIter_NextItem(iterator, &mut item), 0);
        dec_ref(iterator);
        dec_ref(tuple);
    }

    #[test]
    fn dict_surface_roundtrip() {
        anchor();
        clear_error();
        let dict = PyDict_New();
        let value = new_int(7);
        assert_eq!(PyDict_SetItemString(dict, c"answer".as_ptr(), value), 0);
        assert_eq!(PyDict_Size(dict), 1);
        assert_eq!(PyDict_ContainsString(dict, c"answer".as_ptr()), 1);
        assert_eq!(PyDict_ContainsString(dict, c"missing".as_ptr()), 0);

        let mut fetched: *mut ShimObject = ptr::null_mut();
        assert_eq!(
            PyDict_GetItemStringRef(dict, c"answer".as_ptr(), &mut fetched),
            1
        );
        assert_eq!(as_int(fetched).unwrap(), 7);
        dec_ref(fetched);

        let fallback = new_int(9);
        let key = new_str("other".to_string());
        assert_eq!(PyDict_SetDefaultRef(dict, key, fallback, &mut fetched), 0);
        assert_eq!(as_int(fetched).unwrap(), 9);
        dec_ref(fetched);
        assert_eq!(PyDict_SetDefaultRef(dict, key, fallback, &mut fetched), 1);
        dec_ref(fetched);
        dec_ref(key);
        dec_ref(fallback);

        let values = PyDict_Values(dict);
        assert_eq!(PyList_Size(values), 2);
        dec_ref(values);

        let merge = PyDict_New();
        assert_eq!(PyDict_SetItemString(merge, c"answer".as_ptr(), value), 0);
        let replacement = new_int(11);
        assert_eq!(
            PyDict_SetItemString(merge, c"answer".as_ptr(), replacement),
            0
        );
        dec_ref(replacement);
        assert_eq!(PyDict_Merge(merge, dict, 0), 0);
        let mut merged: *mut ShimObject = ptr::null_mut();
        assert_eq!(
            PyDict_GetItemStringRef(merge, c"answer".as_ptr(), &mut merged),
            1
        );
        assert_eq!(as_int(merged).unwrap(), 11);
        dec_ref(merged);
        dec_ref(merge);

        let proxy = PyDictProxy_New(dict);
        assert!(!proxy.is_null());
        assert_eq!(PyDict_DelItemString(dict, c"missing".as_ptr()), -1);
        assert!(take_error().unwrap_or_default().starts_with("KeyError"));
        dec_ref(proxy);
        dec_ref(dict);
        dec_ref(value);
    }

    #[test]
    fn capsule_and_unicode_surface() {
        anchor();
        clear_error();
        let capsule = PyCapsule_New(
            0x1234usize as *mut c_void,
            c"first".as_ptr(),
            ptr::null_mut(),
        );
        assert!(!capsule.is_null());
        assert!(PyCapsule_GetContext(capsule).is_null());
        assert_eq!(PyCapsule_SetContext(capsule, 0x99usize as *mut c_void), 0);
        assert_eq!(PyCapsule_GetContext(capsule) as usize, 0x99);
        assert_eq!(PyCapsule_SetName(capsule, c"second".as_ptr()), 0);
        // SAFETY: the name is a NUL-terminated string owned by the capsule.
        let name = unsafe { CStr::from_ptr(PyCapsule_GetName(capsule)) };
        assert_eq!(name.to_str().unwrap(), "second");
        assert_eq!(
            PyCapsule_GetPointer(capsule, c"second".as_ptr()) as usize,
            0x1234
        );
        assert!(PyCapsule_GetPointer(capsule, c"first".as_ptr()).is_null());
        assert!(take_error().unwrap_or_default().starts_with("ValueError"));
        dec_ref(capsule);

        let text = PyUnicode_FromString(c"héllo".as_ptr());
        assert_eq!(PyUnicode_GetLength(text), 5);
        assert!(PyUnicode_CompareWithASCIIString(text, c"hello".as_ptr()) > 0);
        let suffix = PyUnicode_FromString(c"llo".as_ptr());
        assert_eq!(PyUnicode_Tailmatch(text, suffix, 0, isize::MAX, 1), 1);
        assert_eq!(PyUnicode_Tailmatch(text, suffix, 0, isize::MAX, -1), 0);
        assert_eq!(PyUnicode_Contains(text, suffix), 1);
        let old = PyUnicode_FromString(c"l".as_ptr());
        let new = PyUnicode_FromString(c"L".as_ptr());
        let replaced = PyUnicode_Replace(text, old, new, 1);
        assert_eq!(
            unsafe { CStr::from_ptr(PyUnicode_AsUTF8(replaced)) }
                .to_str()
                .unwrap(),
            "héLlo"
        );
        let substring = PyUnicode_Substring(text, 1, 3);
        assert_eq!(
            unsafe { CStr::from_ptr(PyUnicode_AsUTF8(substring)) }
                .to_str()
                .unwrap(),
            "él"
        );
        let bang = PyUnicode_FromString(c"!".as_ptr());
        let joined = PyUnicode_Concat(text, bang);
        dec_ref(bang);
        assert_eq!(PyUnicode_GetLength(joined), 6);
        let encoded = PyUnicode_AsUTF8String(joined);
        assert_eq!(PyBytes_Size(encoded), 7);
        let mut ucs4 = [0u32; 8];
        let ucs4_pointer = ucs4.as_mut_ptr();
        assert_eq!(PyUnicode_AsUCS4(text, ucs4_pointer, 8, 1), ucs4_pointer);
        assert_eq!(ucs4[1], 'é' as u32);
        assert_eq!(ucs4[5], 0);
        let copy = PyUnicode_AsUCS4Copy(text);
        assert!(!copy.is_null());
        assert_eq!(unsafe { *copy.add(1) }, 'é' as u32);
        PyMem_Free(copy as *mut c_void);
        let interned = PyUnicode_InternFromString(c"interned-name".as_ptr());
        let interned_again = PyUnicode_InternFromString(c"interned-name".as_ptr());
        assert_eq!(interned, interned_again);
        assert_eq!(
            unsafe { CStr::from_ptr(PyUnicode_AsUTF8(interned)) }
                .to_str()
                .unwrap(),
            "interned-name"
        );
        assert_eq!(_PyUnicode_IsAlpha('A' as u32), 1);
        assert_eq!(_PyUnicode_IsUppercase('A' as u32), 1);
        assert_eq!(_PyUnicode_IsLowercase('a' as u32), 1);
        assert_eq!(_PyUnicode_IsWhitespace(' ' as u32), 1);
        assert_eq!(_PyUnicode_IsDigit('5' as u32), 1);
        assert_eq!(_PyUnicode_IsDecimalDigit('5' as u32), 1);
        assert_eq!(_PyUnicode_IsNumeric('5' as u32), 1);
        assert_eq!(_PyUnicode_IsTitlecase('A' as u32), 0);

        let number = new_int(7);
        let label = new_str("x".to_string());
        let arguments = hydro_capi_tuple_pack(2, [number, label].as_ptr());
        dec_ref(number);
        dec_ref(label);
        let template = PyUnicode_FromString(c"%d-%s".as_ptr());
        let formatted = PyUnicode_Format(template, arguments);
        dec_ref(template);
        assert_eq!(
            unsafe { CStr::from_ptr(PyUnicode_AsUTF8(formatted)) }
                .to_str()
                .unwrap(),
            "7-x"
        );
        let raw = new_bytes(b"abc".to_vec());
        let decoded = PyUnicode_FromEncodedObject(raw, c"utf-8".as_ptr(), ptr::null());
        dec_ref(raw);
        assert_eq!(
            unsafe { CStr::from_ptr(PyUnicode_AsUTF8(decoded)) }
                .to_str()
                .unwrap(),
            "abc"
        );

        for object in [
            text,
            suffix,
            old,
            new,
            replaced,
            substring,
            joined,
            encoded,
            interned_again,
            arguments,
            formatted,
            decoded,
        ] {
            dec_ref(object);
        }
        dec_ref(interned);
    }

    #[test]
    fn numeric_and_object_conversions() {
        anchor();
        clear_error();
        let truncated = PyLong_FromDouble(-3.9);
        assert_eq!(as_int(truncated).unwrap(), -3);
        assert_eq!(PyLong_AsLongLong(truncated), -3);
        assert_eq!(PyLong_AsSsize_t(truncated), -3);
        let mut overflow = 0;
        assert_eq!(PyLong_AsLongAndOverflow(truncated, &mut overflow), -3);
        assert_eq!(overflow, 0);
        dec_ref(truncated);

        assert!(PyLong_AsUnsignedLong(new_int(-1)) == c_ulong::MAX);
        assert!(take_error()
            .unwrap_or_default()
            .starts_with("OverflowError"));
        assert_eq!(PyLong_IsZero(new_int(0)), 1);

        let parsed = PyFloat_FromString(new_str("1_000.5".to_string()));
        assert_eq!(as_float(parsed).unwrap(), 1000.5);
        dec_ref(parsed);
        let nan = PyFloat_FromString(new_str("nan".to_string()));
        assert!(as_float(nan).unwrap().is_nan());
        dec_ref(nan);
        let invalid = PyFloat_FromString(new_str("nope".to_string()));
        assert!(invalid.is_null());
        assert!(take_error().unwrap_or_default().starts_with("ValueError"));

        let hex = PyLong_FromUnicodeObject(new_str("0xff".to_string()), 0);
        assert_eq!(as_int(hex).unwrap(), 255);
        dec_ref(hex);
        let bad = PyLong_FromUnicodeObject(new_str("zz".to_string()), 10);
        assert!(bad.is_null());
        assert!(take_error().unwrap_or_default().starts_with("ValueError"));

        let complex = PyComplex_FromCComplex(Py_complex {
            real: 1.5,
            imag: -2.0,
        });
        let value = PyComplex_AsCComplex(complex);
        assert_eq!((value.real, value.imag), (1.5, -2.0));
        dec_ref(complex);

        assert_eq!(_Py_HashDouble(ptr::null_mut(), 1.0), 1);
        assert_eq!(_Py_HashDouble(ptr::null_mut(), 0.0), 0);
        assert_eq!(_Py_HashDouble(ptr::null_mut(), f64::INFINITY), 314_159);

        let integer = new_int(42);
        let spec = new_str("04d".to_string());
        let formatted = PyObject_Format(integer, spec);
        dec_ref(integer);
        dec_ref(spec);
        assert_eq!(
            unsafe { CStr::from_ptr(PyUnicode_AsUTF8(formatted)) }
                .to_str()
                .unwrap(),
            "0042"
        );
        dec_ref(formatted);
        let raw_bytes = new_bytes(b"raw".to_vec());
        let bytes = PyObject_Bytes(raw_bytes);
        assert_eq!(PyBytes_Size(bytes), 3);
        dec_ref(bytes);
        dec_ref(raw_bytes);
    }

    #[test]
    fn contextvars_methods_and_generic_alias() {
        anchor();
        clear_error();
        let variable = PyContextVar_New(c"counter".as_ptr(), ptr::null_mut());
        assert!(!variable.is_null());
        let default = new_int(5);
        let mut fetched: *mut ShimObject = ptr::null_mut();
        assert_eq!(PyContextVar_Get(variable, default, &mut fetched), 0);
        assert_eq!(fetched, default);
        dec_ref(fetched);
        dec_ref(default);
        let stored = new_int(8);
        let token = PyContextVar_Set(variable, stored);
        assert!(!token.is_null());
        assert_eq!(PyContextVar_Get(variable, ptr::null_mut(), &mut fetched), 0);
        assert_eq!(as_int(fetched).unwrap(), 8);
        dec_ref(fetched);
        dec_ref(token);
        dec_ref(stored);
        dec_ref(variable);

        extern "C" fn noop(_self: *mut ShimObject, _args: *mut ShimObject) -> *mut ShimObject {
            ptr::addr_of_mut!(_Py_NoneStruct)
        }
        let mut definition = PyMethodDefMirror {
            ml_name: c"noop".as_ptr(),
            ml_meth: noop as *const () as *mut c_void,
            ml_flags: METH_NOARGS,
            ml_doc: ptr::null(),
        };
        let function = PyCFunction_New(&mut definition, ptr::null_mut());
        let self_object = new_int(3);
        let method = PyMethod_New(function, self_object);
        assert!(!method.is_null());
        let result = PyObject_CallNoArgs(method);
        assert_eq!(result, ptr::addr_of_mut!(_Py_NoneStruct));
        dec_ref(result);
        let bound = PyObject_GetAttrString(method, c"__self__".as_ptr());
        assert_eq!(bound, self_object);
        dec_ref(bound);
        dec_ref(method);
        dec_ref(function);
        dec_ref(self_object);

        let origin = ptr::addr_of_mut!(types::INT_TYPE) as *mut ShimObject;
        let argument = new_int(3);
        let args = hydro_capi_tuple_pack(1, [argument].as_ptr());
        let alias = Py_GenericAlias(origin, args);
        assert!(!alias.is_null());
        let representation = PyObject_Repr(alias);
        assert_eq!(
            unsafe { CStr::from_ptr(PyUnicode_AsUTF8(representation)) }
                .to_str()
                .unwrap(),
            "int[3]"
        );
        let alias_origin = PyObject_GetAttrString(alias, c"__origin__".as_ptr());
        assert_eq!(alias_origin, origin);
        dec_ref(alias_origin);
        let alias_args = PyObject_GetAttrString(alias, c"__args__".as_ptr());
        assert_eq!(alias_args, args);
        dec_ref(alias_args);
        dec_ref(representation);
        dec_ref(alias);
        dec_ref(args);
        dec_ref(argument);
    }

    /// `PyContextVar_New`'s second argument is the default object, not a doc
    /// string. NumPy 2.5 creates the `current_allocator` variable with
    /// `PyDataMem_DefaultHandler` (a capsule/function), which must be accepted
    /// and returned by `PyContextVar_Get` while the variable is unset.
    #[test]
    fn context_var_accepts_any_default_object() {
        anchor();
        clear_error();
        extern "C" fn noop(_self: *mut ShimObject, _args: *mut ShimObject) -> *mut ShimObject {
            ptr::addr_of_mut!(_Py_NoneStruct)
        }
        let mut definition = PyMethodDefMirror {
            ml_name: c"handler".as_ptr(),
            ml_meth: noop as *const () as *mut c_void,
            ml_flags: METH_NOARGS,
            ml_doc: ptr::null(),
        };
        let function = PyCFunction_New(&mut definition, ptr::null_mut());
        assert!(!function.is_null(), "{:?}", take_error());
        let baseline = Py_REFCNT(function);

        let variable = PyContextVar_New(c"current_allocator".as_ptr(), function);
        assert!(!variable.is_null(), "{:?}", take_error());
        assert_eq!(
            Py_REFCNT(function),
            baseline + 1,
            "the variable owns a reference to its default"
        );

        let mut fetched: *mut ShimObject = ptr::null_mut();
        assert_eq!(PyContextVar_Get(variable, ptr::null_mut(), &mut fetched), 0);
        assert_eq!(
            fetched, function,
            "the creation default is returned while unset"
        );
        dec_ref(fetched);

        let override_default = new_str("override".to_string());
        assert_eq!(
            PyContextVar_Get(variable, override_default, &mut fetched),
            0
        );
        assert_eq!(
            fetched, override_default,
            "an explicit fallback takes precedence"
        );
        dec_ref(fetched);
        dec_ref(override_default);

        let stored = new_str("stored".to_string());
        let token = PyContextVar_Set(variable, stored);
        assert!(!token.is_null(), "{:?}", take_error());
        assert_eq!(PyContextVar_Get(variable, ptr::null_mut(), &mut fetched), 0);
        assert_eq!(
            as_string(fetched).as_deref(),
            Some("stored"),
            "a context value takes precedence over the default"
        );
        dec_ref(fetched);
        dec_ref(token);
        dec_ref(stored);

        dec_ref(variable);
        assert_eq!(
            Py_REFCNT(function),
            baseline,
            "freeing the variable releases its default"
        );
        dec_ref(function);
    }

    #[test]
    fn error_state_recursion_and_memoryview() {
        anchor();
        clear_error();
        // SAFETY: exported exception singletons are process-lifetime objects.
        unsafe { PyErr_SetString(PyExc_ValueError, c"boom".as_ptr()) };
        assert_eq!(PyErr_ExceptionMatches(unsafe { PyExc_Exception }), 1);
        assert_eq!(PyErr_ExceptionMatches(unsafe { PyExc_TypeError }), 0);
        let mut kind: *mut ShimObject = ptr::null_mut();
        let mut value: *mut ShimObject = ptr::null_mut();
        let mut traceback: *mut ShimObject = ptr::null_mut();
        PyErr_Fetch(&mut kind, &mut value, &mut traceback);
        assert!(!kind.is_null());
        assert!(!value.is_null());
        assert!(PyErr_Occurred().is_null());
        PyErr_Restore(kind, value, traceback);
        let restored = PyErr_GetRaisedException();
        assert!(matches!(
            kind_ref(restored),
            Some(ShimKind::Exception { .. })
        ));
        dec_ref(restored);

        let mut error_kind = unsafe { PyExc_ValueError };
        let message = new_str("x".to_string());
        let mut error_value = hydro_capi_tuple_pack(1, [message].as_ptr());
        dec_ref(message);
        let mut error_traceback: *mut ShimObject = ptr::null_mut();
        PyErr_NormalizeException(&mut error_kind, &mut error_value, &mut error_traceback);
        assert!(matches!(
            kind_ref(error_value),
            Some(ShimKind::Exception { .. })
        ));
        dec_ref(error_kind);
        dec_ref(error_value);
        dec_ref(error_traceback);

        let mut depth = 0;
        while Py_EnterRecursiveCall(c"test".as_ptr()) == 0 {
            depth += 1;
            assert!(depth < 10_000);
        }
        assert!(take_error()
            .unwrap_or_default()
            .starts_with("RecursionError"));
        for _ in 0..depth {
            Py_LeaveRecursiveCall();
        }
        assert_eq!(Py_EnterRecursiveCall(c"test".as_ptr()), 0);
        Py_LeaveRecursiveCall();

        let bytes = new_bytes(vec![1, 2, 3, 4]);
        let memory = PyMemoryView_FromObject(bytes);
        assert!(!memory.is_null(), "{:?}", take_error());
        assert_eq!(buffer::PyObject_CheckBuffer(memory), 1);
        let mut view: buffer::Py_buffer = unsafe { std::mem::zeroed() };
        assert_eq!(
            buffer::PyObject_GetBuffer(memory, &mut view, buffer::PYBUF_SIMPLE),
            0
        );
        assert_eq!(view.len, 4);
        assert_eq!(view.readonly, 1);
        buffer::PyBuffer_Release(&mut view);
        dec_ref(memory);
        dec_ref(bytes);
    }

    #[test]
    fn variadic_c_entry_points() {
        anchor();
        clear_error();

        // SAFETY: the C entry points are process-lifetime definitions from
        // `parse.c` and every argument matches the format characters.
        let built = unsafe {
            super::Py_BuildValue(c"(i[s])".as_ptr(), 3 as c_int, c"x".as_ptr()) as *mut ShimObject
        };
        assert!(!built.is_null(), "{:?}", take_error());
        match kind_ref(built) {
            Some(ShimKind::Tuple(items)) => {
                assert_eq!(items.len(), 2);
                assert_eq!(as_int(items[0] as *mut ShimObject).unwrap(), 3);
                assert!(matches!(
                    kind_ref(items[1] as *mut ShimObject),
                    Some(ShimKind::List(_))
                ));
            }
            _ => panic!("unexpected Py_BuildValue result"),
        }
        dec_ref(built);

        let integer_type = ptr::addr_of_mut!(types::INT_TYPE) as *mut ShimObject;
        let number = new_int(9);
        // SAFETY: "O!" takes the object followed by its expected type.
        let checked = unsafe {
            super::Py_BuildValue(c"O!".as_ptr(), number, integer_type) as *mut ShimObject
        };
        assert!(!checked.is_null(), "{:?}", take_error());
        assert_eq!(as_int(checked).unwrap(), 9);
        dec_ref(checked);
        let wrong = new_str("not-an-int".to_string());
        // SAFETY: see above.
        let rejected =
            unsafe { super::Py_BuildValue(c"O!".as_ptr(), wrong, integer_type) as *mut ShimObject };
        assert!(rejected.is_null());
        assert!(take_error().unwrap_or_default().starts_with("TypeError"));
        dec_ref(wrong);
        dec_ref(number);

        let argument = new_int(7);
        // SAFETY: "%d/%s/%R" take an int, a C string and an object.
        let formatted = unsafe {
            super::PyUnicode_FromFormat(c"[%d:%s:%R]".as_ptr(), 4 as c_int, c"z".as_ptr(), argument)
                as *mut ShimObject
        };
        assert!(!formatted.is_null(), "{:?}", take_error());
        assert_eq!(
            unsafe { CStr::from_ptr(PyUnicode_AsUTF8(formatted)) }
                .to_str()
                .unwrap(),
            "[4:z:7]"
        );
        dec_ref(formatted);

        let first = new_int(1);
        let second = new_int(2);
        // SAFETY: two pointers are supplied for a count of two.
        let packed = unsafe { super::PyTuple_Pack(2isize, first, second) as *mut ShimObject };
        assert_eq!(PyTuple_Size(packed), 2);
        let mut unpacked_first: *mut ShimObject = ptr::null_mut();
        let mut unpacked_second: *mut ShimObject = ptr::null_mut();
        // SAFETY: the tuple has exactly two elements and both slots are valid.
        let status = unsafe {
            super::PyArg_UnpackTuple(
                packed as *mut c_void,
                c"unpack".as_ptr(),
                2,
                2,
                &mut unpacked_first as *mut *mut ShimObject,
                &mut unpacked_second as *mut *mut ShimObject,
            )
        };
        assert_eq!(status, 1);
        assert_eq!(unpacked_first, first);
        assert_eq!(unpacked_second, second);
        dec_ref(packed);
        dec_ref(first);
        dec_ref(second);
        dec_ref(argument);

        let mut text: *mut c_char = ptr::null_mut();
        // SAFETY: the format writes one int and one C string.
        let mut buffer = [0 as c_char; 32];
        let written = unsafe {
            super::PyOS_snprintf(
                buffer.as_mut_ptr(),
                buffer.len(),
                c"%d:%s".as_ptr(),
                5 as c_int,
                c"ok".as_ptr(),
            )
        };
        assert!(written > 0);
        assert_eq!(
            unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_str().unwrap(),
            "5:ok"
        );
        // SAFETY: `end` receives a pointer into the input literal.
        let value =
            unsafe { super::PyOS_string_to_double(c"1.5x".as_ptr(), &mut text, ptr::null_mut()) };
        assert_eq!(value, 1.5);
        assert_eq!(unsafe { CStr::from_ptr(text) }.to_str().unwrap(), "x");
        // SAFETY: `end` receives a pointer into the input literal.
        let parsed = unsafe { super::PyOS_strtol(c"ff".as_ptr(), &mut text, 16) };
        assert_eq!(parsed, 255);
    }

    /// CPython's `Py_BuildValue` returns a tuple when the format names more
    /// than one top-level value (`"OO"`, `"NN"`, `"ONc"`, `"iis#"`), which
    /// NumPy's `_multiarray_umath` initialisation relies on.
    #[test]
    fn build_value_returns_a_tuple_for_multiple_top_level_values() {
        anchor();
        clear_error();

        let left = new_int(1000);
        let right = new_int(1001);
        // SAFETY: "OO" supplies two borrowed objects.
        let pair = unsafe { super::Py_BuildValue(c"OO".as_ptr(), left, right) as *mut ShimObject };
        assert!(!pair.is_null(), "{:?}", take_error());
        let items = match kind_ref(pair) {
            Some(ShimKind::Tuple(items)) => *items,
            other => panic!("expected a tuple, got {}", kind_variant(other)),
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0] as *mut ShimObject, left);
        assert_eq!(items[1] as *mut ShimObject, right);
        assert_eq!(Py_REFCNT(left), 2, "'O' borrows; the tuple owns one ref");
        dec_ref(pair);
        assert_eq!(Py_REFCNT(left), 1);
        dec_ref(left);
        dec_ref(right);

        // A stolen 'N' reference is transferred to the tuple.
        let borrowed = new_int(1002);
        let stolen = new_int(1003);
        // SAFETY: "ON" supplies one borrowed and one stolen object.
        let mixed =
            unsafe { super::Py_BuildValue(c"ON".as_ptr(), borrowed, stolen) as *mut ShimObject };
        assert!(!mixed.is_null(), "{:?}", take_error());
        assert_eq!(PyTuple_Size(mixed), 2);
        assert_eq!(Py_REFCNT(borrowed), 2);
        assert_eq!(Py_REFCNT(stolen), 1, "'N' steals the caller's reference");
        dec_ref(mixed);
        assert_eq!(Py_REFCNT(borrowed), 1);
        dec_ref(borrowed);

        // Basic scalar units take part in the implicit tuple too.
        // SAFETY: "iis#" takes two ints, a C string and its length.
        let scalars = unsafe {
            super::Py_BuildValue(
                c"iis#".as_ptr(),
                5 as c_int,
                6 as c_int,
                c"abc".as_ptr(),
                2isize,
            ) as *mut ShimObject
        };
        assert!(!scalars.is_null(), "{:?}", take_error());
        assert_eq!(PyTuple_Size(scalars), 3);
        assert_eq!(
            as_int(PyTuple_GetItem(scalars, 0)).unwrap(),
            5,
            "the first scalar is an int"
        );
        assert_eq!(
            as_string(PyTuple_GetItem(scalars, 2)).as_deref(),
            Some("ab")
        );
        dec_ref(scalars);

        // A three-value tuple and the `l` (long) unit are the remaining
        // shapes used by NumPy's type tables.
        let first = new_int(1005);
        let second = new_int(1006);
        let third = new_int(1007);
        // SAFETY: "(OOO)" supplies three borrowed objects.
        let triple = unsafe {
            super::Py_BuildValue(c"(OOO)".as_ptr(), first, second, third) as *mut ShimObject
        };
        assert_eq!(PyTuple_Size(triple), 3);
        dec_ref(triple);
        dec_ref(first);
        dec_ref(second);
        dec_ref(third);
        // SAFETY: "l" takes one C long.
        let long_value =
            unsafe { super::Py_BuildValue(c"l".as_ptr(), 123456789 as c_long) as *mut ShimObject };
        assert_eq!(as_int(long_value).unwrap(), 123_456_789);
        dec_ref(long_value);

        // `z` maps a NULL C string to None, as CPython does.
        // SAFETY: "z" takes a C string pointer.
        let none = unsafe {
            super::Py_BuildValue(c"z".as_ptr(), ptr::null::<c_char>()) as *mut ShimObject
        };
        assert!(!none.is_null(), "{:?}", take_error());
        assert_eq!(none, ptr::addr_of_mut!(_Py_NoneStruct));
        dec_ref(none);

        // "{ON}" is NumPy's type-dictionary form: a borrowed key and a
        // stolen value inside a dict.
        let key = new_str("signature".to_string());
        let value = new_int(1004);
        // SAFETY: "{ON}" takes a key and a stolen value.
        let dictionary =
            unsafe { super::Py_BuildValue(c"{ON}".as_ptr(), key, value) as *mut ShimObject };
        assert!(!dictionary.is_null(), "{:?}", take_error());
        assert_eq!(
            PyDict_GetItemString(dictionary, c"signature".as_ptr()),
            value
        );
        assert_eq!(Py_REFCNT(value), 1, "the dict owns the stolen reference");
        dec_ref(dictionary);
        dec_ref(key);

        // An empty format still yields None.
        // SAFETY: no arguments are read.
        let empty = unsafe { super::Py_BuildValue(c"".as_ptr()) as *mut ShimObject };
        assert_eq!(empty, ptr::addr_of_mut!(_Py_NoneStruct));
        dec_ref(empty);
    }

    /// `PyErr_Format` shares CPython's `PyUnicode_FromFormat` mini-language:
    /// object conversions (`%R`/`%S`/`%A`/`%U`/`%V`/`%T`) plus C99 units,
    /// flags, width and precision. NumPy formats error messages with these.
    #[test]
    fn py_err_format_uses_cpython_conversions() {
        anchor();
        clear_error();
        let exception = unsafe { PyExc_ValueError } as *mut c_void;
        let text = new_str("hello".to_string());
        let number = new_int(1000);

        // SAFETY: each format below consumes exactly the arguments it names.
        unsafe { super::PyErr_Format(exception, c"%R".as_ptr(), text) };
        assert_eq!(take_error().as_deref(), Some("ValueError: 'hello'"));

        unsafe { super::PyErr_Format(exception, c"%S".as_ptr(), number) };
        assert_eq!(take_error().as_deref(), Some("ValueError: 1000"));

        unsafe { super::PyErr_Format(exception, c"%A".as_ptr(), text) };
        assert_eq!(take_error().as_deref(), Some("ValueError: 'hello'"));

        unsafe { super::PyErr_Format(exception, c"%U".as_ptr(), text) };
        assert_eq!(take_error().as_deref(), Some("ValueError: hello"));

        // C99 units keep their meaning, including the Py_ssize_t length.
        unsafe {
            super::PyErr_Format(
                exception,
                c"%s %d %zd %c".as_ptr(),
                c"plain".as_ptr(),
                5 as c_int,
                7_isize,
                b'A' as c_int,
            )
        };
        let message = take_error().expect("formatted message");
        assert!(message.starts_with("ValueError: plain 5 7 A"), "{message}");

        unsafe { super::PyErr_Format(exception, c"%p".as_ptr(), text as *mut c_void) };
        assert!(take_error().unwrap_or_default().contains("0x"));

        // Flags, width and precision apply as in CPython.
        unsafe { super::PyErr_Format(exception, c"%10R".as_ptr(), text) };
        assert_eq!(take_error().as_deref(), Some("ValueError:    'hello'"));
        unsafe { super::PyErr_Format(exception, c"%.3R".as_ptr(), text) };
        assert_eq!(take_error().as_deref(), Some("ValueError: 'he"));

        // `%V` falls back to its C string when the object is NULL.
        unsafe {
            super::PyErr_Format(
                exception,
                c"%V".as_ptr(),
                ptr::null::<ShimObject>(),
                c"fallback".as_ptr(),
            )
        };
        assert_eq!(take_error().as_deref(), Some("ValueError: fallback"));

        // `%T` renders the qualified type name.
        unsafe {
            super::PyErr_Format(
                exception,
                c"%T".as_ptr(),
                ptr::addr_of_mut!(types::STR_TYPE) as *mut ShimObject,
            )
        };
        let message = take_error().expect("type qualname");
        assert!(message.contains("str"), "{message}");

        // `%n` is refused instead of writing through the pointer.
        clear_error();
        unsafe { super::PyErr_Format(exception, c"%n".as_ptr(), ptr::null::<c_int>()) };
        let message = take_error().expect("%n is rejected");
        assert!(
            message.contains("unsupported format character"),
            "{message}"
        );

        dec_ref(text);
        dec_ref(number);
    }

    /// `METH_STATIC`/`METH_CLASS`/`METH_COEXIST` are binding flags: they do
    /// not change the calling convention (`PyCMethod_New` masks them out), and
    /// a static type method is bound with a NULL receiver while a class method
    /// receives the class it was read from.
    #[test]
    fn method_flags_static_class_and_coexist_dispatch() {
        anchor();
        clear_error();
        static METHOD_SELF: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(usize::MAX);
        static FASTCALL_SELF: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(usize::MAX);
        static FASTCALL_NARGS: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(usize::MAX);

        unsafe extern "C" fn record_varargs(
            self_object: *mut ShimObject,
            _args: *mut ShimObject,
        ) -> *mut ShimObject {
            METHOD_SELF.store(self_object as usize, SEQ);
            ptr::addr_of_mut!(_Py_NoneStruct)
        }
        unsafe extern "C" fn record_fastcall(
            self_object: *mut ShimObject,
            _args: *const *mut ShimObject,
            nargs: isize,
            _kwnames: *mut ShimObject,
        ) -> *mut ShimObject {
            FASTCALL_SELF.store(self_object as usize, SEQ);
            FASTCALL_NARGS.store(nargs as usize, SEQ);
            ptr::addr_of_mut!(_Py_NoneStruct)
        }

        let mut methods = [
            PyMethodDefMirror {
                ml_name: c"probe_static".as_ptr(),
                ml_meth: record_varargs as *const () as *mut c_void,
                // COEXIST only affects type-dict replacement, never the call.
                ml_flags: METH_VARARGS | METH_STATIC | METH_COEXIST,
                ml_doc: ptr::null(),
            },
            PyMethodDefMirror {
                ml_name: c"probe_class".as_ptr(),
                ml_meth: record_varargs as *const () as *mut c_void,
                ml_flags: METH_VARARGS | METH_CLASS,
                ml_doc: ptr::null(),
            },
            PyMethodDefMirror {
                ml_name: ptr::null(),
                ml_meth: ptr::null_mut(),
                ml_flags: 0,
                ml_doc: ptr::null(),
            },
        ];
        let mut slots = [
            PyTypeSlotMirror {
                slot: 64, // Py_tp_methods
                pfunc: methods.as_mut_ptr() as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: ptr::null_mut(),
            },
        ];
        let type_object = make_type(
            c"probe.MethodFlags",
            PY_OBJECT_HEAD_SIZE as c_int,
            0,
            0,
            &mut slots,
        );
        let instance = PyType_GenericAlloc(type_object, 0);
        assert!(!instance.is_null(), "{:?}", take_error());

        // A static method read from the class or an instance receives NULL.
        for receiver in [type_object, instance] {
            METHOD_SELF.store(usize::MAX, SEQ);
            let function = PyObject_GetAttrString(receiver, c"probe_static".as_ptr());
            assert!(!function.is_null(), "{:?}", take_error());
            let result = PyObject_Vectorcall(function, ptr::null(), 0, ptr::null_mut());
            assert!(!result.is_null(), "{:?}", take_error());
            assert_eq!(METHOD_SELF.load(SEQ), 0, "METH_STATIC passes NULL");
            dec_ref(result);
            dec_ref(function);
        }

        // A class method receives the class it was read from.
        for receiver in [type_object, instance] {
            METHOD_SELF.store(usize::MAX, SEQ);
            let function = PyObject_GetAttrString(receiver, c"probe_class".as_ptr());
            assert!(!function.is_null(), "{:?}", take_error());
            let result = PyObject_Vectorcall(function, ptr::null(), 0, ptr::null_mut());
            assert!(!result.is_null(), "{:?}", take_error());
            assert_eq!(
                METHOD_SELF.load(SEQ),
                type_object as usize,
                "METH_CLASS passes the accessed class"
            );
            dec_ref(result);
            dec_ref(function);
        }

        // A PyCFunction with NumPy/PyO3's `FASTCALL|KEYWORDS|STATIC` flags is
        // dispatched as fastcall with its bound receiver, exactly as CPython
        // (the static bit never changes the calling convention).
        let mut definition = PyMethodDefMirror {
            ml_name: c"module_static".as_ptr(),
            ml_meth: record_fastcall as *const () as *mut c_void,
            ml_flags: METH_FASTCALL | METH_KEYWORDS | METH_STATIC,
            ml_doc: ptr::null(),
        };
        let marker = new_int(1010);
        let function = PyCFunction_NewEx(&mut definition, marker, ptr::null_mut());
        assert!(!function.is_null(), "{:?}", take_error());
        FASTCALL_SELF.store(usize::MAX, SEQ);
        FASTCALL_NARGS.store(usize::MAX, SEQ);
        let result = PyObject_Vectorcall(function, ptr::null(), 0, ptr::null_mut());
        assert!(!result.is_null(), "{:?}", take_error());
        assert_eq!(FASTCALL_SELF.load(SEQ), marker as usize);
        assert_eq!(FASTCALL_NARGS.load(SEQ), 0);
        dec_ref(result);
        dec_ref(function);
        dec_ref(marker);

        dec_ref(instance);
        dec_ref(type_object);
    }

    /// A returned container graph keeps CPython aliasing: the same C object
    /// maps to one Hydro object, so a mutation through one alias is visible
    /// through every other alias, and a list/dict cycle survives. pydantic's
    /// `gather_schemas_for_cleaning` depends on this.
    #[test]
    fn extension_result_preserves_container_aliases_and_cycles() {
        anchor();
        clear_error();
        unsafe extern "C" fn aliased_graph(
            _self_object: *mut ShimObject,
            _args: *mut ShimObject,
        ) -> *mut ShimObject {
            let inner = PyDict_New();
            let outer = PyDict_New();
            if inner.is_null() || outer.is_null() {
                dec_ref(inner);
                dec_ref(outer);
                return ptr::null_mut();
            }
            if PyDict_SetItemString(outer, c"first".as_ptr(), inner) != 0
                || PyDict_SetItemString(outer, c"second".as_ptr(), inner) != 0
                || PyDict_SetItemString(outer, c"self".as_ptr(), outer) != 0
            {
                dec_ref(inner);
                dec_ref(outer);
                return ptr::null_mut();
            }
            dec_ref(inner);
            outer
        }
        let mut methods = [
            PyMethodDefMirror {
                ml_name: c"aliased_graph".as_ptr(),
                ml_meth: aliased_graph as *const () as *mut c_void,
                ml_flags: METH_VARARGS,
                ml_doc: ptr::null(),
            },
            PyMethodDefMirror {
                ml_name: ptr::null(),
                ml_meth: ptr::null_mut(),
                ml_flags: 0,
                ml_doc: ptr::null(),
            },
        ];
        let mut definition = empty_definition();
        definition.m_name = c"hydro_alias_probe".as_ptr();
        definition.m_methods = methods.as_mut_ptr();
        let module = PyModule_Create2(&mut definition, 3);
        assert!(!module.is_null(), "{:?}", take_error());
        let id = module_id(module).expect("module id");
        let value = super::call_method_hydro_args(id, 0, &[]).expect("the graph converts");
        let roots = crate::heap::root_values(&[value]);

        let pairs = crate::value::as_dict(value).expect("Hydro dict");
        let field = |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| crate::value::as_str(*key) == Some(name))
                .map(|&(_, item)| item)
        };
        let first = field("first").expect("first entry");
        let second = field("second").expect("second entry");
        assert_eq!(first, second, "one C dict maps to one Hydro object");
        assert_eq!(
            field("self"),
            Some(value),
            "a dict that contains itself keeps its identity"
        );

        // Mutating the alias changes the shared object pydantic later reads.
        let key = crate::value::str_alloc("resolved".to_string());
        let marker = crate::value::str_alloc("yes".to_string());
        crate::container::dict_set(first, key, marker);
        assert!(!crate::exc::pending(), "the mutation must not raise");
        let shared = crate::value::as_dict(second).expect("second dict");
        assert!(
            shared
                .iter()
                .any(|&(k, v)| crate::value::as_str(k) == Some("resolved")
                    && crate::value::as_str(v) == Some("yes")),
            "a mutation through one alias is visible through the other"
        );

        crate::heap::release_roots(roots);
        dec_ref(module);
    }

    /// A C-returned `set`/`frozenset` becomes one Hydro set: iterable, equal
    /// to a set built from the same items, and aliased like other containers.
    /// pydantic's `__pydantic_fields_set__` is such an extension set.
    #[test]
    fn extension_result_converts_c_sets_to_hydro_sets() {
        anchor();
        clear_error();
        unsafe extern "C" fn set_graph(
            _self_object: *mut ShimObject,
            _args: *mut ShimObject,
        ) -> *mut ShimObject {
            let items = PyList_New(0);
            if items.is_null() {
                return ptr::null_mut();
            }
            for number in [2i64, 1] {
                let item = PyLong_FromLong(number);
                if item.is_null() || PyList_Append(items, item) != 0 {
                    dec_ref(item);
                    dec_ref(items);
                    return ptr::null_mut();
                }
                dec_ref(item);
            }
            let mutable = PySet_New(items);
            let frozen = PyFrozenSet_New(items);
            dec_ref(items);
            if mutable.is_null() || frozen.is_null() {
                dec_ref(mutable);
                dec_ref(frozen);
                return ptr::null_mut();
            }
            let outer = PyDict_New();
            if outer.is_null() {
                dec_ref(mutable);
                dec_ref(frozen);
                return ptr::null_mut();
            }
            if PyDict_SetItemString(outer, c"set".as_ptr(), mutable) != 0
                || PyDict_SetItemString(outer, c"set_alias".as_ptr(), mutable) != 0
                || PyDict_SetItemString(outer, c"frozen".as_ptr(), frozen) != 0
            {
                dec_ref(mutable);
                dec_ref(frozen);
                dec_ref(outer);
                return ptr::null_mut();
            }
            dec_ref(mutable);
            dec_ref(frozen);
            outer
        }
        let mut methods = [
            PyMethodDefMirror {
                ml_name: c"set_graph".as_ptr(),
                ml_meth: set_graph as *const () as *mut c_void,
                ml_flags: METH_VARARGS,
                ml_doc: ptr::null(),
            },
            PyMethodDefMirror {
                ml_name: ptr::null(),
                ml_meth: ptr::null_mut(),
                ml_flags: 0,
                ml_doc: ptr::null(),
            },
        ];
        let mut definition = empty_definition();
        definition.m_name = c"hydro_set_probe".as_ptr();
        definition.m_methods = methods.as_mut_ptr();
        let module = PyModule_Create2(&mut definition, 3);
        assert!(!module.is_null(), "{:?}", take_error());
        let id = module_id(module).expect("module id");
        let value = super::call_method_hydro_args(id, 0, &[]).expect("the sets convert");
        let roots = crate::heap::root_values(&[value]);

        let pairs = crate::value::as_dict(value).expect("Hydro dict");
        let field = |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| crate::value::as_str(*key) == Some(name))
                .map(|&(_, item)| item)
        };
        let set = field("set").expect("set entry");
        assert_eq!(
            field("set_alias"),
            Some(set),
            "one C set maps to one Hydro object"
        );

        let expected = crate::value::set_new(vec![
            crate::value::encode_int(1).expect("int"),
            crate::value::encode_int(2).expect("int"),
        ]);
        let expected_roots = crate::heap::root_values(&[expected]);
        for name in ["set", "frozen"] {
            let set = field(name).unwrap_or_else(|| panic!("{name} entry"));
            assert!(
                crate::value::as_set(set).is_some(),
                "{name} must convert to a Hydro set"
            );
            assert!(
                crate::container::equals(set, expected),
                "{name} must equal a Hydro set built from the same items"
            );
            assert!(crate::container::equals(expected, set));

            let iterator = crate::iter::iter_of(set);
            let iterator_roots = crate::heap::root_values(&[iterator]);
            assert!(!crate::exc::pending(), "{name} must be iterable");
            let mut seen = Vec::new();
            let mut item = crate::value::NONE;
            while crate::iter::iter_next(iterator, &mut item) == 1 {
                seen.push(item);
            }
            assert!(!crate::exc::pending(), "{name} iteration must not raise");
            assert_eq!(seen.len(), 2, "{name} yields both items");
            let seen_set = crate::value::set_new(seen);
            let seen_roots = crate::heap::root_values(&[seen_set]);
            assert!(
                crate::container::equals(seen_set, expected),
                "{name} iteration yields the same items"
            );
            crate::heap::release_roots(seen_roots);
            crate::heap::release_roots(iterator_roots);
        }
        crate::heap::release_roots(expected_roots);
        crate::heap::release_roots(roots);
        dec_ref(module);
    }

    /// A Hydro set crosses inbound as a real C set: the callee sees every
    /// item, and returning the handle maps back to the caller's set.
    #[test]
    fn hydro_set_arguments_cross_as_c_sets_and_come_back() {
        anchor();
        clear_error();
        unsafe extern "C" fn inspect_set(
            _self_object: *mut ShimObject,
            args: *mut ShimObject,
        ) -> *mut ShimObject {
            let set = PyTuple_GetItem(args, 0);
            if set.is_null() {
                return ptr::null_mut();
            }
            let one = PyLong_FromLong(1);
            let two = PyLong_FromLong(2);
            if one.is_null() || two.is_null() {
                dec_ref(one);
                dec_ref(two);
                return ptr::null_mut();
            }
            let contains_one = PySet_Contains(set, one);
            let contains_two = PySet_Contains(set, two);
            dec_ref(one);
            dec_ref(two);
            if contains_one != 1 || contains_two != 1 || PySet_Size(set) != 2 {
                set_error("the inbound set lost its items".to_string());
                return ptr::null_mut();
            }
            inc_ref(set);
            set
        }
        let mut methods = [
            PyMethodDefMirror {
                ml_name: c"inspect_set".as_ptr(),
                ml_meth: inspect_set as *const () as *mut c_void,
                ml_flags: METH_VARARGS,
                ml_doc: ptr::null(),
            },
            PyMethodDefMirror {
                ml_name: ptr::null(),
                ml_meth: ptr::null_mut(),
                ml_flags: 0,
                ml_doc: ptr::null(),
            },
        ];
        let mut definition = empty_definition();
        definition.m_name = c"hydro_inbound_set_probe".as_ptr();
        definition.m_methods = methods.as_mut_ptr();
        let module = PyModule_Create2(&mut definition, 3);
        assert!(!module.is_null(), "{:?}", take_error());
        let id = module_id(module).expect("module id");

        let set = crate::value::set_new(vec![
            crate::value::encode_int(1).expect("int"),
            crate::value::encode_int(2).expect("int"),
        ]);
        let roots = crate::heap::root_values(&[set]);
        let result = super::call_method_hydro_args(id, 0, &[set])
            .expect("the set crosses inbound and returns");
        let result_roots = crate::heap::root_values(&[result]);
        assert_eq!(
            result, set,
            "returning the C set maps back to the caller's Hydro set"
        );
        assert!(crate::value::as_set(result).is_some());
        crate::heap::release_roots(result_roots);
        crate::heap::release_roots(roots);
        dec_ref(module);
    }

    /// Reading a Hydro instance attribute that holds a set through the C API
    /// yields a real C set (pydantic reads `__pydantic_fields_set__` this
    /// way before serialising).
    #[test]
    fn hydro_set_attribute_reads_back_as_a_c_set() {
        anchor();
        clear_error();
        let object = crate::value::object_class();
        let class = crate::value::class_new("SetReadProbe".to_string(), object, Vec::new());
        let class_roots = crate::heap::root_values(&[class]);
        let instance = crate::value::instance_new(class);
        let instance_roots = crate::heap::root_values(&[instance]);
        let set = crate::value::set_new(vec![
            crate::value::encode_int(1).expect("int"),
            crate::value::encode_int(2).expect("int"),
        ]);
        let set_roots = crate::heap::root_values(&[set]);
        crate::methods::setattr(instance, b"fields".as_ptr(), 6, set);
        assert!(
            !crate::exc::pending(),
            "the attribute assignment must not raise"
        );

        let handle = value_object(hydro_shim(instance));
        let read = super::hydro_attribute(handle, "fields")
            .expect("a Hydro set attribute must cross back as a C set");
        dec_ref(handle);
        assert!(
            matches!(kind_ref(read), Some(ShimKind::Set(_))),
            "the attribute crosses as a C set, not an opaque handle"
        );
        assert_eq!(PySet_Size(read), 2);
        let one = PyLong_FromLong(1);
        let two = PyLong_FromLong(2);
        assert_eq!(PySet_Contains(read, one), 1);
        assert_eq!(PySet_Contains(read, two), 1);
        dec_ref(one);
        dec_ref(two);
        dec_ref(read);
        crate::heap::release_roots(set_roots);
        crate::heap::release_roots(instance_roots);
        crate::heap::release_roots(class_roots);
    }

    /// A set assigned through the C attribute API (pydantic's
    /// `__pydantic_fields_set__`) is stored as a Hydro set, so Hydro can
    /// iterate and compare it afterwards.
    #[test]
    fn c_set_attribute_assignment_stores_a_hydro_set() {
        anchor();
        clear_error();
        let object = crate::value::object_class();
        let class = crate::value::class_new("SetAttrProbe".to_string(), object, Vec::new());
        let class_roots = crate::heap::root_values(&[class]);
        let instance = crate::value::instance_new(class);
        let instance_roots = crate::heap::root_values(&[instance]);

        let set = PySet_New(ptr::null_mut());
        assert!(!set.is_null(), "{:?}", take_error());
        for number in [2i64, 1] {
            let item = PyLong_FromLong(number);
            assert!(!item.is_null(), "{:?}", take_error());
            assert_eq!(PySet_Add(set, item), 0, "{:?}", take_error());
            dec_ref(item);
        }
        let handle = value_object(hydro_shim(instance));
        super::hydro_attribute_set(handle, "fields", set)
            .expect("attribute assignment converts the set");
        dec_ref(handle);
        dec_ref(set);

        let stored = crate::methods::getattr(instance, b"fields".as_ptr(), 6);
        assert!(!crate::exc::pending(), "the stored attribute is readable");
        assert!(
            crate::value::as_set(stored).is_some(),
            "the attribute is a Hydro set"
        );
        let expected = crate::value::set_new(vec![
            crate::value::encode_int(1).expect("int"),
            crate::value::encode_int(2).expect("int"),
        ]);
        let expected_roots = crate::heap::root_values(&[expected]);
        assert!(crate::container::equals(stored, expected));
        crate::heap::release_roots(expected_roots);
        crate::heap::release_roots(instance_roots);
        crate::heap::release_roots(class_roots);
    }

    /// A result that hands back an argument container must convert to the
    /// caller's *original* Hydro object, not a copy: pydantic mutates the
    /// gathered `definition-ref` dictionaries in place and expects the root
    /// schema to observe it.
    #[test]
    fn extension_result_maps_argument_objects_back_to_hydro_identity() {
        anchor();
        clear_error();
        unsafe extern "C" fn wrap_first(
            _self_object: *mut ShimObject,
            args: *mut ShimObject,
        ) -> *mut ShimObject {
            let item = PyTuple_GetItem(args, 0);
            if item.is_null() {
                return ptr::null_mut();
            }
            let outer = PyDict_New();
            if outer.is_null() {
                return ptr::null_mut();
            }
            if PyDict_SetItemString(outer, c"inner".as_ptr(), item) != 0 {
                dec_ref(outer);
                return ptr::null_mut();
            }
            outer
        }
        let mut methods = [
            PyMethodDefMirror {
                ml_name: c"wrap_first".as_ptr(),
                ml_meth: wrap_first as *const () as *mut c_void,
                ml_flags: METH_VARARGS,
                ml_doc: ptr::null(),
            },
            PyMethodDefMirror {
                ml_name: ptr::null(),
                ml_meth: ptr::null_mut(),
                ml_flags: 0,
                ml_doc: ptr::null(),
            },
        ];
        let mut definition = empty_definition();
        definition.m_name = c"hydro_origin_probe".as_ptr();
        definition.m_methods = methods.as_mut_ptr();
        let module = PyModule_Create2(&mut definition, 3);
        assert!(!module.is_null(), "{:?}", take_error());
        let id = module_id(module).expect("module id");

        // Hydro argument: {"a": 1}
        let argument = crate::value::dict_new(Vec::new());
        let argument_roots = crate::heap::root_values(&[argument]);
        crate::container::dict_set(
            argument,
            crate::value::str_alloc("a".to_string()),
            crate::value::encode_int(1).expect("int"),
        );

        let result =
            super::call_method_hydro_args(id, 0, &[argument]).expect("the wrapped result converts");
        let roots = crate::heap::root_values(&[result]);
        let pairs = crate::value::as_dict(result).expect("result dict");
        let inner = pairs
            .iter()
            .find(|(key, _)| crate::value::as_str(*key) == Some("inner"))
            .map(|&(_, item)| item)
            .expect("inner entry");
        assert_eq!(
            inner, argument,
            "an argument container keeps its Hydro identity across the call"
        );

        // Mutating the returned entry changes the caller's object.
        crate::container::dict_set(
            inner,
            crate::value::str_alloc("seen".to_string()),
            crate::value::str_alloc("yes".to_string()),
        );
        assert!(!crate::exc::pending(), "the mutation must not raise");
        let updated = crate::value::as_dict(argument).expect("argument dict");
        assert!(
            updated
                .iter()
                .any(|&(k, v)| crate::value::as_str(k) == Some("seen")
                    && crate::value::as_str(v) == Some("yes")),
            "the caller's dict observes the mutation"
        );

        crate::heap::release_roots(roots);
        crate::heap::release_roots(argument_roots);
        dec_ref(module);
    }
    #[test]
    fn hydro_handles_wrap_hydro_values() {
        ensure_builtin_types();
        // An immediate value still becomes a real handle whose behaviour
        // dispatches to Hydro; `tp_basicsize` of the wrapper type names this
        // host's storage (it never exposes Hydro heap memory).
        let tagged = 5i64 << 3;
        let handle = value_object(hydro_shim(tagged));
        assert!(matches!(kind_ref(handle), Some(ShimKind::Hydro(_))));
        assert_eq!(object_type_name(handle), "int");
        assert_eq!(
            Py_TYPE(handle) as usize,
            ptr::addr_of_mut!(types::HYDRO_TYPE) as usize
        );
        assert_eq!(types::type_name(Py_TYPE(handle)), "object");

        let repr = PyObject_Repr(handle);
        let rendered = match kind_ref(repr) {
            Some(ShimKind::Str { text, .. }) => text.clone(),
            _ => panic!("repr did not produce a string: {:?}", take_error()),
        };
        assert_eq!(rendered, "5");
        dec_ref(repr);
        assert_eq!(types::object_is_true(handle), 1);
        assert_eq!(hydro_tagged(handle), Some(tagged));
        assert!(hash_object(handle, 0).is_ok());

        match object_to_value(handle).expect("Hydro handle converts") {
            ShimValue::Hydro(value) => assert_eq!(value.value(), tagged),
            other => panic!("expected a Hydro contract value, got {other:?}"),
        }

        // Passing the tagged value back through Hydro creates a handle that
        // names the same value.
        let again = value_object(hydro_shim(tagged));
        assert_eq!(hydro_tagged(again), Some(tagged));
        dec_ref(again);
        dec_ref(handle);

        // A Hydro class handle reports type metadata through the C API.
        let class = crate::value::class_new("Widget".to_string(), crate::value::NONE, Vec::new());
        let class_handle = value_object(hydro_shim(class));
        assert_eq!(
            hydro_type_name_text(class_handle).as_deref(),
            Some("Widget")
        );
        assert!(hydro_type_flags(class_handle).is_some());
        assert_eq!(types::PyType_IsSubtype(class_handle, class_handle), 1);

        // Unsupported objects (an iterator here) are rejected, not faked.
        let iterator = crate::value::iter_new(crate::value::list_new(vec![tagged]));
        let outcome = crate::modules::extension::value_to_shim(iterator);
        assert!(
            outcome.is_err(),
            "an iterator must not cross as a fake object"
        );
        dec_ref(class_handle);
    }
}
