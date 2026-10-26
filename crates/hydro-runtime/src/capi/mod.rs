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
//! [`types::PyType_FromSpec`] builds a real heap type with `tp_name`,
//! `tp_basicsize`, `tp_itemsize`, flags, methods, doc, buffer slots and one
//! base. `PyType_Ready`, `PyType_GetSlot`, `PyType_GetName`,
//! `PyType_GetQualName`, `PyType_GetFlags`, `PyType_GenericAlloc`,
//! `PyType_GenericNew`, `_PyObject_New`, `_PyObject_NewVar`, `PyObject_Init`
//! and `PyObject_InitVar` are implemented. Slots without an implementation
//! (members/getset descriptors, iteration, rich comparison, number, sequence,
//! mapping protocols, `tp_del`, `tp_clear`, tokens) are rejected by
//! `PyType_FromSpec` with a tagged error naming the slot; they are never
//! silently ignored.
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
//! Full NumPy/pydantic compatibility is **not** claimed: multiple inheritance,
//! descriptors, sequence/number/mapping protocols, iteration, attribute
//! dictionaries, weak references and the C terminator/token APIs are absent and
//! fail loudly. Those are explicitly later gates.

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
    Str {
        text: String,
        c_text: Option<CString>,
    },
    /// `data` is the bytes content; `c_data` is `data` plus a trailing NUL.
    Bytes {
        data: Vec<u8>,
        c_data: Vec<u8>,
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
        ShimKind::List(_) => ptr::addr_of_mut!(types::LIST_TYPE),
        ShimKind::Tuple(_) => ptr::addr_of_mut!(types::TUPLE_TYPE),
        ShimKind::Dict(_) => ptr::addr_of_mut!(types::DICT_TYPE),
        ShimKind::Capsule { .. } => ptr::addr_of_mut!(types::CAPSULE_TYPE),
        ShimKind::CFunction { .. } => ptr::addr_of_mut!(types::CFUNCTION_TYPE),
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
    release_kind(kind, object);
    drop(boxed);
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
        ShimKind::None
        | ShimKind::Bool(_)
        | ShimKind::Int(_)
        | ShimKind::Float(_)
        | ShimKind::Str { .. }
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
    new_object(ShimKind::Str { text, c_text })
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
                        $symbol = types::allocate_named_type(
                            $name,
                            PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_BASETYPE,
                        ) as *mut ShimObject;
                    )+
                }
            });
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

/// Maps an exported exception class to the loader's message prefix.
fn exception_prefix(exception: *mut ShimObject) -> String {
    ensure_builtin_types();
    if !exception.is_null() {
        if types::is_type_pointer(exception) {
            let name = types::type_name(exception as *mut TypeObject);
            if name != "type" {
                return name;
            }
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
    // SAFETY: the payload follows the header for every shim object.
    Some(unsafe { &(*object).kind })
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
        ShimKind::CFunction { .. } => Err(
            "extension returned a C function object, which cannot be converted to a Hydro value"
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
    static CALL_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Records a pending error, using CPython's `"<Type>: <message>"` spelling so
/// the loader can map it back to a Hydro exception tag.
pub(crate) fn set_error(message: impl Into<String>) {
    CALL_ERROR.with(|slot| *slot.borrow_mut() = Some(message.into()));
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
    CALL_ERROR.with(|slot| {
        slot.borrow_mut().take();
    });
}

pub(crate) fn error_pending() -> bool {
    CALL_ERROR.with(|slot| slot.borrow().is_some())
}

pub(crate) fn take_error() -> Option<String> {
    CALL_ERROR.with(|slot| slot.borrow_mut().take())
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
        _ => false,
    }
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
    #[allow(dead_code)]
    handle: usize,
    /// Module objects are owned by the registry (the initializer's new
    /// reference is transferred here).
    module_object: usize,
    attrs: HashMap<String, ShimAttr>,
    methods: Vec<MethodSpec>,
}

struct Registry {
    next_id: u64,
    modules: HashMap<u64, ModuleEntry>,
    by_path: HashMap<String, u64>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            next_id: 1,
            modules: HashMap::new(),
            by_path: HashMap::new(),
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
    let id = allocate_module_id();
    let module_object = new_object(ShimKind::Module(id));

    let mut attrs = HashMap::with_capacity(methods.len());
    for (index, method) in methods.iter().enumerate() {
        attrs.insert(
            method.name.clone(),
            ShimAttr::Callable(ShimCallable {
                module: id,
                method: index as u32,
            }),
        );
    }

    registry_lock().modules.insert(
        id,
        ModuleEntry {
            name,
            handle: 0,
            module_object: module_object as usize,
            attrs,
            methods,
        },
    );
    Ok((id, module_object))
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

fn add_attribute(object: *mut ShimObject, name: String, attribute: ShimAttr) -> Result<(), String> {
    let id = module_id(object)?;
    let mut registry = registry_lock();
    let entry = registry
        .modules
        .get_mut(&id)
        .ok_or_else(|| format!("unknown extension module {id}"))?;
    entry.attrs.insert(name, attribute);
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

/// Looks up `name` on `object`, returning a new reference.
fn attribute_lookup(object: *mut ShimObject, name: &str) -> Result<*mut ShimObject, String> {
    ensure_builtin_types();
    if object.is_null() {
        return Err(format!(
            "AttributeError: NULL object has no attribute '{name}'"
        ));
    }
    if types::is_type_pointer(object) {
        // Class-level attributes (a type dictionary) are not implemented; the
        // method table describes instances, not the class.
        return Err(format!(
            "AttributeError: type object '{}' has no attribute '{name}'",
            types::type_name(object as *mut TypeObject)
        ));
    }
    let type_object = type_of(object);
    if type_object.is_null() {
        return Err(format!("AttributeError: object has no attribute '{name}'"));
    }
    // SAFETY: live type object.
    let getattro = unsafe { (*type_object).tp_getattro };
    if getattro != 0 {
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
    set_error(format!(
        "AttributeError: '{}' object has no attribute '{name}'",
        types::type_name(type_object)
    ));
    -1
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
}

/// References every exported entry point so the linker keeps the definitions.
pub(crate) fn anchor() {
    types::ensure_builtin_types();
    initialize_exception_types();
    anchor_exception_types();
    anchor_constant_objects();
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
            count_capsule_drop as *mut c_void,
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
            count_capsule_drop as *mut c_void,
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
            count_list_capsule_drop as *mut c_void,
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
                ml_meth: method_seven as *mut c_void,
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
                pfunc: construct_instance as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 60,
                pfunc: initialise_instance as *mut c_void,
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
            initialise_instance as *mut c_void
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
                slot: 72, // Py_tp_members
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
        assert!(message.contains("Py_tp_members"), "{message}");
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

        let pair = PyTuple_New(2);
        Py_IncRef(object_type);
        assert_eq!(PyTuple_SetItem(pair, 0, object_type), 0);
        Py_IncRef(object_type);
        assert_eq!(PyTuple_SetItem(pair, 1, object_type), 0);
        assert!(PyType_FromSpecWithBases(&mut spec, pair).is_null());
        assert!(take_error().unwrap().contains("multiple bases"));
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
            ml_meth: fast_sum as *mut c_void,
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
        assert_eq!(PyCFunction_GetFunction(function), fast_sum as *mut c_void);

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
                ml_meth: method_seven as *mut c_void,
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
                pfunc: strided_getbuffer as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 2,
                pfunc: strided_releasebuffer as *mut c_void,
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
                pfunc: bad_getbuffer as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 2,
                pfunc: bad_releasebuffer as *mut c_void,
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
            ml_meth: gil_probe as *mut c_void,
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
            ml_meth: save_restore as *mut c_void,
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
                value: exec as *mut c_void,
            },
            PyModuleDefSlotMirror {
                slot: PY_MOD_EXEC,
                value: exec as *mut c_void,
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
}
