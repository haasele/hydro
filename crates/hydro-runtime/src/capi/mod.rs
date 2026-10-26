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
//! Every handle handed to an extension starts with CPython's `PyObject_HEAD`:
//! `ob_refcnt` at offset 0 and `ob_type` at offset 8. Variable-size instances
//! carry `ob_size` at offset 16. The layout of [`ShimObject`] (the private
//! value carrier) therefore starts with the same header, and extension
//! instances allocated through `PyType_GenericAlloc`/`PyObject_New` are raw
//! `tp_basicsize` allocations with that header. [`types::TypeObject`] is a
//! byte-compatible CPython 3.14 `PyTypeObject`; its CPython prefix is asserted
//! at compile time.
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
//! this host owns. Because handles hold copies rather than Hydro heap
//! references, the Hydro collector has no cross-heap edges to see; what the
//! tracked set guarantees is that handle finalisation is ordered through the
//! refcount and that no tracked object is freed without being untracked.
//! `tp_traverse` is preserved in the type layout and returned by
//! `PyType_GetSlot`; the tracked set does not walk extension instance
//! internals, because there are no handle-to-Hydro edges for it to discover.
//!
//! # Remaining limits
//!
//! Full NumPy/pydantic compatibility is **not** claimed. Instance attribute
//! dictionaries/weak references, the async protocol suites and Python source
//! evaluation are absent; the evaluation entry points fail with a tagged
//! `RuntimeError` instead of resolving to nothing. `PyImport_ImportModule` and
//! friends resolve extension modules loaded into the extension registry and
//! Hydro native/user modules from the runtime registry; every other name
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
#[path = "type.rs"]
pub(crate) mod types;

#[allow(unused_imports)]
pub(crate) use buffer::{
    PyBuffer_FillInfo, PyBuffer_Release, PyObject_CheckBuffer, PyObject_GetBuffer, Py_buffer,
    PYBUF_C_CONTIGUOUS, PYBUF_FORMAT, PYBUF_F_CONTIGUOUS, PYBUF_ND, PYBUF_SIMPLE, PYBUF_STRIDES,
    PYBUF_WRITABLE,
};
#[allow(unused_imports)]
pub(crate) use types::{
    PyTypeSlotMirror, PyTypeSpecMirror, TypeObject, TypeRole, PY_TPFLAGS_BASETYPE,
    PY_TPFLAGS_DISALLOW_INSTANTIATION, PY_TPFLAGS_HAVE_GC, PY_TPFLAGS_HAVE_VECTORCALL,
    PY_TPFLAGS_HEAPTYPE, PY_TPFLAGS_IMMUTABLETYPE,
};

// The type/object allocation API lives in `type.rs`; re-export it so the link
// anchor and the module tests can name it directly, and so every `Py*` entry
// point is reachable from one place.
#[allow(unused_imports)]
pub(crate) use types::{
    _PyObject_GC_New, _PyObject_GC_NewVar, _PyObject_New, _PyObject_NewVar, PyObject_GC_Del,
    PyObject_GC_IsTracked, PyObject_GC_Track, PyObject_GC_UnTrack, PyObject_IS_GC, PyObject_Init,
    PyObject_InitVar, PyObject_New, PyObject_NewVar, PyType_FromSpec, PyType_FromSpecWithBases,
    PyType_GenericAlloc, PyType_GenericNew, PyType_GetFlags, PyType_GetName, PyType_GetQualName,
    PyType_GetSlot, PyType_Ready,
};

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_long, c_uint, c_void};
use std::ptr;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, Once, PoisonError};

use types::{ensure_builtin_types, type_of, TypeObject as PyTypeObject};

// ---------------------------------------------------------------------------
// CPython mirror constants
// ---------------------------------------------------------------------------

/// `METH_*` bits from CPython's `methodobject.h` (limited-API subset).
const METH_VARARGS: c_int = 0x0001;
const METH_KEYWORDS: c_int = 0x0002;
const METH_NOARGS: c_int = 0x0004;
const METH_O: c_int = 0x0008;
/// `METH_FASTCALL` (stable ABI since 3.10) selects the
/// `_PyCFunctionFast`/`_PyCFunctionFastWithKeywords` calling convention.
const METH_FASTCALL: c_int = 0x0080;
/// `METH_METHOD`: the function receives its defining class
/// (`PyCMethod`). Creation is supported; invocation is reported as unsupported
/// because this host has no `PyCMethod` call convention.
const METH_METHOD: c_int = 0x0200;
const METHOD_FLAGS: c_int = METH_VARARGS | METH_KEYWORDS | METH_NOARGS | METH_O | METH_FASTCALL;

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
pub(crate) const MIN_VAR_BASICSIZE: usize = 24;

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

/// The crate-internal value contract shared with the extension loader.
///
/// Containers are owned copies: passing a Hydro list to an extension copies it,
/// and a container returned by an extension is converted into a fresh Hydro
/// container. Nothing is shared across the boundary.
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
    /// Opaque pointer capsule. The pointer belongs to the extension; its
    /// destructor now runs when the handle is freed.
    Capsule {
        ptr: usize,
        name: Option<String>,
    },
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

/// Internal representation of one shim object.
#[derive(Clone, Debug)]
enum ShimKind {
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
    /// `__cause__`/`__traceback__` slots (0 for "not set").
    Exception {
        exc_type: usize,
        args: Vec<usize>,
        cause: usize,
        traceback: usize,
    },
    /// Native/user module function, dispatched through the runtime registry.
    RuntimeFunction {
        module: String,
        name: String,
    },
    List(Vec<usize>),
    Tuple(Vec<usize>),
    Dict(Vec<(usize, usize)>),
    /// `destructor` is invoked when the capsule's reference count reaches zero.
    Capsule {
        ptr: usize,
        name: Option<CString>,
        destructor: usize,
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
    /// Non-NULL sentinel returned by `PyErr_Occurred`.
    Error,
}

/// Private `#[repr(C)]` object handed to extensions as an opaque `PyObject *`.
///
/// The CPython header comes first; the tagged payload follows.
#[repr(C)]
pub(crate) struct ShimObject {
    pub(crate) head: PyObjectHead,
    kind: ShimKind,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::offset_of!(ShimObject, head) == 0);
    assert!(std::mem::offset_of!(ShimObject, kind) == PY_OBJECT_HEAD_SIZE);
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
    static THREAD_STATE_TOKEN: u8 = const { 0 };
}

/// Acquires the emulated GIL, reentrantly on the current thread.
pub(crate) fn gil_acquire() {
    let depth = GIL_DEPTH.with(Cell::get);
    if depth == 0 {
        let guard = GIL_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        GIL_GUARD.with(|slot| *slot.borrow_mut() = Some(guard));
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
    }
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
#[no_mangle]
pub extern "C" fn PyGILState_Release(state: c_int) {
    if state == PY_GILSTATE_UNLOCKED {
        gil_release();
    } else if state != PY_GILSTATE_LOCKED {
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
pub(crate) fn new_object(kind: ShimKind) -> *mut ShimObject {
    let type_object = kind_type(&kind);
    Box::into_raw(Box::new(ShimObject {
        head: PyObjectHead {
            ob_refcnt: AtomicIsize::new(1),
            ob_type: type_object,
        },
        kind,
    }))
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
        ShimKind::CFunction { .. } => ptr::addr_of_mut!(types::CFUNCTION_TYPE),
        ShimKind::RuntimeFunction { .. } => ptr::addr_of_mut!(types::CFUNCTION_TYPE),
        // An exception instance's `ob_type` is its exception class.
        ShimKind::Exception { exc_type, .. } => *exc_type as *mut TypeObject,
        ShimKind::NotImplemented => ptr::addr_of_mut!(types::NOT_IMPLEMENTED_TYPE),
        ShimKind::Ellipsis => ptr::addr_of_mut!(types::ELLIPSIS_TYPE),
        ShimKind::Module(_) | ShimKind::ModuleDef { .. } => {
            ptr::addr_of_mut!(types::MODULE_TYPE)
        }
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
pub(crate) fn inc_ref(object: *mut ShimObject) {
    if object.is_null() {
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
    if object.is_null() {
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
    // SAFETY: `object` is a `ShimObject` allocation created by `new_object`,
    // and the reference count just reached zero, so this thread owns it. The
    // allocation stays alive while `release_kind` runs so capsule destructors
    // receive a valid (if dying) object, as in CPython.
    let mut boxed = unsafe { Box::from_raw(object) };
    let kind = std::mem::replace(&mut boxed.kind, ShimKind::None);
    drop_buffer_cache_entry(object as usize);
    release_kind(kind, object);
    drop(boxed);
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
    let kind = unsafe { &mut (*object).kind };
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
        ShimKind::List(items) | ShimKind::Tuple(items) => {
            for item in items {
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
            if traceback != 0 {
                dec_ref(traceback as *mut ShimObject);
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
    let empty_tuple = make_immortal(new_object(ShimKind::Tuple(Vec::new()))) as usize;
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
                            PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_BASETYPE,
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
fn value_object(value: ShimValue) -> *mut ShimObject {
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
                new_object(ShimKind::Tuple(
                    items
                        .into_iter()
                        .map(|item| value_object(item) as usize)
                        .collect(),
                ))
            }
        }
        ShimValue::Dict(pairs) => new_object(ShimKind::Dict(
            pairs
                .into_iter()
                .map(|(key, value)| (value_object(key) as usize, value_object(value) as usize))
                .collect(),
        )),
        ShimValue::Capsule { ptr, name } => new_object(ShimKind::Capsule {
            ptr,
            name: name.and_then(|name| CString::new(name).ok()),
            destructor: 0,
        }),
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
    new_object(ShimKind::Tuple(
        arguments
            .iter()
            .cloned()
            .map(|value| value_object(value) as usize)
            .collect(),
    ))
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
        Some(&(*object).kind)
    }
}

/// Python type name of any handle.
fn object_type_name(object: *mut ShimObject) -> String {
    if object.is_null() {
        return "NULL".to_string();
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

/// Converts an extension's return object to the contract value.
fn object_to_value(object: *mut ShimObject) -> Result<ShimValue, String> {
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
        return Err(format!(
            "extension returned the type object '{}', which cannot be converted to a Hydro value",
            types::type_name(object as *mut TypeObject)
        ));
    }
    let Some(kind) = kind_ref(object) else {
        return Err(format!(
            "TypeError: extension object of type '{}' cannot be converted to a Hydro value",
            object_type_name(object)
        ));
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
        ShimKind::CFunction { .. } | ShimKind::RuntimeFunction { .. } => Err(
            "extension returned a C function object, which cannot be converted to a Hydro value"
                .to_string(),
        ),
        ShimKind::ByteArray { .. } => Err(
            "extension returned a bytearray object, which cannot be converted to a Hydro value"
                .to_string(),
        ),
        ShimKind::Set(_) | ShimKind::FrozenSet(_) => Err(
            "extension returned a set object, which cannot be converted to a Hydro value"
                .to_string(),
        ),
        ShimKind::Complex { .. } => Err(
            "extension returned a complex object, which cannot be converted to a Hydro value"
                .to_string(),
        ),
        ShimKind::Exception { .. } => Err(
            "extension returned an exception object, which cannot be converted to a Hydro value"
                .to_string(),
        ),
        ShimKind::NotImplemented => Ok(ShimValue::NotImplemented),
        ShimKind::Ellipsis => Ok(ShimValue::Ellipsis),
        ShimKind::Module(_) => {
            Err("extension returned a module object, which is not supported".to_string())
        }
        ShimKind::ModuleDef { .. } => {
            Err("extension returned a module definition object, which is not supported".to_string())
        }
        ShimKind::Error => Err("extension returned an error object".to_string()),
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
        ShimKind::NotImplemented => "NotImplemented".to_string(),
        ShimKind::Ellipsis => "Ellipsis".to_string(),
        ShimKind::Module(_) => "<module>".to_string(),
        ShimKind::ModuleDef { .. } => "<module definition>".to_string(),
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
}

pub(crate) fn error_pending() -> bool {
    CALL_ERROR.with(|slot| slot.borrow().message.is_some())
}

pub(crate) fn take_error() -> Option<String> {
    let message = CALL_ERROR.with(|slot| slot.borrow_mut().message.take());
    replace_error_state(None, None);
    message
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
    match unsafe { &mut (*object).kind } {
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

fn tuple_slots_mut(object: *mut ShimObject) -> Result<&'static mut Vec<usize>, String> {
    if object.is_null() {
        return Err("TypeError: expected a tuple object, got NULL".to_string());
    }
    // SAFETY: see `list_slots_mut`.
    match unsafe { &mut (*object).kind } {
        ShimKind::Tuple(items) => Ok(items),
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
    match unsafe { &mut (*object).kind } {
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

/// Calls a method of a loaded module with contract values.
pub(crate) fn call_method(
    module: u64,
    method: u32,
    args: &[ShimValue],
) -> Result<ShimValue, String> {
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
        invoke_method(
            spec.function,
            spec.flags,
            module_object,
            &spec.name,
            &pointers,
            ptr::null_mut(),
        )
    };
    let outcome = take_result_value(result);
    for object in objects {
        dec_ref(object as *mut ShimObject);
    }
    outcome
}

/// Converts a *new reference* result into a contract value and releases it.
fn take_result_value(result: *mut ShimObject) -> Result<ShimValue, String> {
    if result.is_null() {
        return Err(take_error().unwrap_or_else(|| "extension function failed".to_string()));
    }
    clear_error();
    let value = object_to_value(result);
    dec_ref(result);
    value
}

/// Calls a heap type object with contract values.
pub(crate) fn call_type_object(address: usize, args: &[ShimValue]) -> Result<ShimValue, String> {
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
    let outcome = take_result_value(result);
    for object in objects {
        dec_ref(object as *mut ShimObject);
    }
    outcome
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

/// Converts a pending Hydro exception into the shim's `"<Type>: <message>"`
/// spelling and clears it.
fn take_hydro_error(fallback: &str) -> String {
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
    keyword_count: usize,
    positional_count: usize,
) -> *mut ShimObject {
    if keyword_count != 0 {
        set_error(format!(
            "TypeError: {module}.{function}() takes no keyword arguments through the C API"
        ));
        return ptr::null_mut();
    }
    debug_assert_eq!(positional_count, values.len());
    let mut arguments = Vec::with_capacity(positional_count);
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
    let result = crate::modules::call_native(module, function, arguments.len(), arguments.as_ptr());
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

/// Looks up a loaded module by name and returns it with a new reference.
fn import_loaded(name: &str) -> Result<*mut ShimObject, String> {
    {
        let registry = registry_lock();
        if let Some(id) = registry.by_name.get(name).copied() {
            let Some(entry) = registry.modules.get(&id) else {
                return Err(format!(
                    "ImportError: dangling module registry entry for '{name}'"
                ));
            };
            let module = entry.module_object as *mut ShimObject;
            inc_ref(module);
            return Ok(module);
        }
    }
    // Not a loaded DSO: fall back to the Hydro runtime registry, which holds
    // every native stdlib module and every compiled user module.
    if crate::modules::registry_lookup(name).is_some() {
        let (_, module) = create_runtime_module(name.to_string())?;
        inc_ref(module);
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

fn sys_dict() -> usize {
    static SYS: LazyLock<usize> = LazyLock::new(|| {
        let dict = PyDict_New();
        if dict.is_null() {
            return 0;
        }
        let modules = PyImport_GetModuleDict();
        if !modules.is_null() {
            dict_set_string(dict, "modules", modules);
        }
        let path = PyList_New(0);
        if !path.is_null() {
            dict_set_string(dict, "path", path);
            dec_ref(path);
        }
        let argv = PyList_New(0);
        if !argv.is_null() {
            dict_set_string(dict, "argv", argv);
            dec_ref(argv);
        }
        dict as usize
    });
    *SYS
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
#[no_mangle]
pub extern "C" fn PyType_FromModuleAndSpec(
    _module: *mut ShimObject,
    spec: *mut types::PyTypeSpecMirror,
    bases: *mut ShimObject,
) -> *mut ShimObject {
    PyType_FromSpecWithBases(spec, bases)
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
) -> Result<(usize, Option<&'static CString>, usize), String> {
    match kind_ref(object) {
        Some(ShimKind::Capsule {
            ptr,
            name,
            destructor,
        }) => Ok((*ptr, name.as_ref(), *destructor)),
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
        destructor: destructor as usize,
    })
}

/// `PyCapsule_GetPointer`.
#[no_mangle]
pub extern "C" fn PyCapsule_GetPointer(
    capsule: *mut ShimObject,
    name: *const c_char,
) -> *mut c_void {
    let (pointer, stored, _) = match capsule_fields(capsule) {
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
        Ok((_, Some(name), _)) => name.as_ptr(),
        Ok((_, None, _)) => ptr::null(),
        Err(error) => {
            set_error(error);
            ptr::null()
        }
    }
}

/// `PyCapsule_IsValid`.
#[no_mangle]
pub extern "C" fn PyCapsule_IsValid(capsule: *mut ShimObject, name: *const c_char) -> c_int {
    let Ok((pointer, stored, _)) = capsule_fields(capsule) else {
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
    match unsafe { &mut (*capsule).kind } {
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
    new_object(ShimKind::Tuple(items))
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
    set_error(format!("{}: {text}", exception_prefix(exception)));
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
    new_object(ShimKind::Tuple(
        values.iter().map(|&value| value as usize).collect(),
    ))
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
    if flags & !METHOD_FLAGS != 0 {
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
    // values, paired in order with `kwnames`. `nargs` handed to a FASTCALL
    // callee is the total (`positional_count + kwcount`), exactly as
    // `cfunction_vectorcall_FASTCALL_KEYWORDS` does.
    let positional_count = values.len() - kwcount;
    let pointer = values.as_ptr();
    let nargs = (positional_count + kwcount) as isize;

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
    if new_function == 0 {
        // Static exception classes have no `tp_new` here; calling them builds
        // the documented instance holding the positional arguments.
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
        let (function, flags, name) = {
            let definition = method as *mut PyMethodDefMirror;
            if definition.is_null() {
                set_error("SystemError: C function has no PyMethodDef");
                return ptr::null_mut();
            }
            // SAFETY: the table belongs to the live extension.
            unsafe {
                (
                    (*definition).ml_meth as usize,
                    (*definition).ml_flags,
                    read_c_string((*definition).ml_name),
                )
            }
        };
        let self_object = self_object as *mut ShimObject;
        return invoke_method(function, flags, self_object, &name, values, kwnames);
    }
    if let Some(ShimKind::RuntimeFunction { module, name }) = kind_ref(callable) {
        let module = module.clone();
        let name = name.clone();
        return call_runtime_function(&module, &name, values, keyword_count, positional_count);
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
            _ => return None,
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
        let result = PyCFunction_NewEx(method, object, ptr::null_mut());
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
    if types::is_type_pointer(object) {
        return Err(format!(
            "AttributeError: cannot set '{name}' on immutable type '{}'",
            types::type_name(object as *mut TypeObject)
        ));
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
    let type_object =
        types::allocate_exception_type(&full_name, PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_BASETYPE);
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
    let ShimKind::Exception { cause: slot, .. } = (unsafe { &mut (*exception).kind }) else {
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
    } = (unsafe { &mut (*exception).kind })
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
        | Some(ShimKind::Tuple(items))
        | Some(ShimKind::Set(items))
        | Some(ShimKind::FrozenSet(items)) => {
            for &item in items {
                inc_ref(item as *mut ShimObject);
            }
            Ok(items.clone())
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
        match &mut (*object).kind {
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
    new_object(ShimKind::Tuple(items.to_vec()))
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
        set_error("TypeError: issubclass() arg 1 must be a class");
        return -1;
    }
    if !types::is_type_pointer(class) {
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
}

/// References every exported entry point so the linker keeps the definitions.
pub(crate) fn anchor() {
    types::ensure_builtin_types();
    initialize_exception_types();
    anchor_exception_types();
    anchor_constant_objects();
    // Builtin type data symbols: staticlib linkers keep a definition only when
    // something references it, so take every address here.
    let type_symbols: [*const TypeObject; 37] = {
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
    ];
    let _ = std::hint::black_box(symbols);
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

    // -- object header layout --------------------------------------------

    #[test]
    fn handle_layout_matches_cpython() {
        assert_eq!(std::mem::size_of::<PyObjectHead>(), 16);
        assert_eq!(std::mem::offset_of!(PyObjectHead, ob_refcnt), 0);
        assert_eq!(std::mem::offset_of!(PyObjectHead, ob_type), 8);
        assert_eq!(std::mem::size_of::<PyVarObjectHead>(), 24);
        assert_eq!(std::mem::offset_of!(PyVarObjectHead, ob_size), 16);
        assert_eq!(std::mem::offset_of!(ShimObject, kind), 16);

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

    #[test]
    fn gc_types_track_and_untrack_instances() {
        anchor();
        let mut slots = empty_slots();
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
        let mut total = 0i64;
        for index in 0..nargs.max(0) as usize {
            let item = unsafe { *args.add(index) };
            if let Some(ShimKind::Int(value)) = kind_ref(item) {
                total += *value;
            }
        }
        // `kwnames` in a FASTCALL|KEYWORDS callee is a tuple of names, not the
        // keyword dict used by `PyArg_ParseTupleAndKeywords`. Count it the
        // CPython way (`PyTuple_GET_SIZE`).
        let keyword_count = if kwnames.is_null() {
            0
        } else {
            PyTuple_Size(kwnames).max(0)
        };
        PyLong_FromLong((total + 1000 * keyword_count as i64) as c_long)
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
        let exported: [*const (); 8] = [
            PyType_FromSpec as *const (),
            PyType_GetSlot as *const (),
            PyObject_Vectorcall as *const (),
            PyObject_GetBuffer as *const (),
            PyObject_GC_Track as *const (),
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
        assert!(take_error().unwrap().contains("StopIteration"));
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
            other => panic!("unexpected payload: {other:?}"),
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
            other => panic!("unexpected payload: {other:?}"),
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
}
