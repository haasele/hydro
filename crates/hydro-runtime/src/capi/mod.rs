//! Minimal CPython limited-API host surface used to load stable-ABI
//! (`abi3`) extension modules.
//!
//! Hydro does not link `libpython`. When an extension is loaded with `dlopen`
//! its undefined `Py*` symbols resolve against the definitions exported from
//! this module; the `tea` executable is linked with `--export-dynamic` so
//! those definitions also land in the dynamic symbol table. Only the small
//! subset declared here exists — an extension that calls anything else fails
//! to load instead of being silently mis-emulated.
//!
//! [`anchor`] must be called once from `hydro_runtime_init`: it references every
//! exported entry point so the linker keeps their object files in the final
//! static library instead of discarding them as unreachable.
//!
//! `parse.c` supplies the variadic entry points (`PyArg_ParseTuple`,
//! `PyArg_ParseTupleAndKeywords` and `PyErr_Format`) which cannot be written
//! with stable Rust's C variadics. It forwards every conversion and every
//! pending-error update to the `hydro_capi_*` accessors below.
//!
//! Slice limitations, deliberately documented:
//!
//! - **Constants are real objects.** `_Py_NoneStruct`, `_Py_TrueStruct`,
//!   `_Py_FalseStruct`, `_Py_NotImplementedStruct` and `_Py_EllipsisObject`
//!   are exported as shim objects, so `Py_None` (however the extension spells
//!   it) is pointer-identical to the value returned by `Py_GetConstant*`.
//!   Identity comparisons (`Py_IsNone`, `Py_IsTrue`, ...) therefore behave as
//!   in CPython; there is no reference-count immortality, because shim
//!   objects are never freed anyway. Only their addresses are meaningful —
//!   the objects have no `PyObject`/`PyLongObject` field layout, so reading
//!   their fields (for example `Py_SIZE(Py_True)`) is unsupported.
//! - **Exception classes are name carriers.** Every CPython 3.14 `PyExc_*`
//!   data symbol exists so an extension can link and pass it to the error
//!   setters. Its identity selects the Hydro exception category of the
//!   reported error; the object itself has no class layout, no bases and no
//!   attributes beyond that name.
//! - **Type handles are opaque.** `PyType_FromSpec` records the spec's name,
//!   size, flags and the `Py_tp_new`/`Py_tp_methods`/`Py_tp_doc` slots, and
//!   the resulting handle can be constructed by the extension bridge. There
//!   is no `PyTypeObject` layout, no inheritance, no GC support and no
//!   instance layout, so any other slot is rejected with a tagged error
//!   rather than silently ignored.
//! - **`METH_FASTCALL` is honoured** (`_PyCFunctionFast` and the keyword
//!   variant). Keyword arguments are never supplied by the Hydro bridge, so
//!   fastcall methods always receive a NULL `kwnames`.
//!
//! - **No GIL.** Extension functions run synchronously on the calling thread.
//!   The module registry mutex only protects lookups and is released before
//!   the extension runs, so concurrent calls into the same extension are the
//!   caller's responsibility. Extensions that assume the GIL protects their
//!   own mutable state are unsupported.
//! - **No finalization.** `Py_DecRef` decrements the reference count but never
//!   frees; every shim object lives in a process-lifetime arena. A stale shim
//!   pointer therefore cannot dangle, at the cost of retaining all objects
//!   until the process exits. `m_free`/`m_clear` module hooks are not run.
//! - **Values are limited** to the `ShimValue` variants (scalars, bytes,
//!   strings, capsules and lists/tuples/dicts of them). Container arguments
//!   are copied into the shim object graph and container results are copied
//!   back into fresh Hydro containers; nothing is shared across the boundary.
//!   Returning any other value from an extension is reported as an error
//!   rather than converted.
//! - **Capsules** carry an opaque `void *` and an optional name. The
//!   destructor is stored but never invoked, because arena objects are never
//!   finalized; an extension that relies on capsule finalization is
//!   unsupported.
//! - **Exception types** are exported as the usual `PyExc_*` data symbols so
//!   extension code can pass them to the error setters. Their names select
//!   the exception category the loader reports; `BaseException`/`Exception`
//!   and the classes without a dedicated Hydro tag fall back to
//!   `RuntimeError`.
//! - **Buffers** are read-only, contiguous bytes only: `PyObject_CheckBuffer`,
//!   `PyObject_GetBuffer` and `PyBuffer_Release` exist, `Py_buffer` mirrors
//!   CPython 3.14 x86_64, and any other object is a `TypeError`.
//! - **Dict keys** are compared structurally for the scalar types the shim
//!   supports (identity first, then type/value); there is no `__hash__` or
//!   `__eq__` dispatch.

#![allow(non_snake_case)]
// These are C ABI entry points: every argument is a raw `PyObject *` supplied
// by an extension module, so the usual "take a reference" API shape does not
// apply.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
// The exported constant objects are typed as the `pub(crate)` shim object;
// extensions only ever see them through an opaque `PyObject *`.
#![allow(private_interfaces)]

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::ptr;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

// CPython limited-API mirror structures

/// `METH_*` bits from CPython's `methodobject.h` (limited-API subset).
const METH_VARARGS: c_int = 0x0001;
const METH_KEYWORDS: c_int = 0x0002;
const METH_NOARGS: c_int = 0x0004;
const METH_O: c_int = 0x0008;
/// `METH_FASTCALL` (stable ABI since 3.10) selects the
/// `_PyCFunctionFast`/`_PyCFunctionFastWithKeywords` calling convention.
const METH_FASTCALL: c_int = 0x0080;
const METHOD_FLAGS: c_int = METH_VARARGS | METH_KEYWORDS | METH_NOARGS | METH_O | METH_FASTCALL;

/// `PyType_Slot` identifiers from CPython 3.14's `typeslots.h` that this slice
/// represents. Every other identifier is rejected by `PyType_FromSpec`.
const PY_TP_DOC: c_int = 56;
const PY_TP_METHODS: c_int = 64;
const PY_TP_NEW: c_int = 65;

/// `PyType_Spec.flags` bits this slice can honour. `Py_TPFLAGS_DEFAULT` is
/// zero in CPython 3.14; the remaining bits are the compatibility markers
/// older stable-ABI extensions still set. Anything else (subtyping, GC,
/// managed dict, vectorcall, ...) is rejected.
const PY_TPFLAGS_HAVE_FINALIZE: u32 = 1 << 0;
const PY_TPFLAGS_IMMUTABLETYPE: u32 = 1 << 8;
const PY_TPFLAGS_HAVE_VERSION_TAG: u32 = 1 << 18;
const SUPPORTED_TYPE_FLAGS: u32 =
    PY_TPFLAGS_HAVE_FINALIZE | PY_TPFLAGS_IMMUTABLETYPE | PY_TPFLAGS_HAVE_VERSION_TAG;

/// Upper bound on the entries read from a `PyType_Slot` table.
const MAX_TYPE_SLOTS: usize = 64;

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

/// Upper bound on the entries read from a `PyModuleDef_Slot` table. CPython
/// has no declared limit; bounding it keeps a corrupt table from scanning
/// forever.
const MAX_MODULE_SLOTS: usize = 64;

/// `Py_mod_*` slot identifiers from CPython's `moduleobject.h` (stable ABI).
const PY_MOD_CREATE: c_int = 1;
const PY_MOD_EXEC: c_int = 2;
const PY_MOD_MULTIPLE_INTERPRETERS: c_int = 3;
const PY_MOD_GIL: c_int = 4;

/// Largest accepted `Py_mod_multiple_interpreters` / `Py_mod_gil` values;
/// both policies use small enum values starting at zero.
const PY_MOD_MULTIPLE_INTERPRETERS_PER_INTERPRETER_GIL_SUPPORTED: c_int = 2;
const PY_MOD_GIL_NOT_USED: c_int = 1;

/// Mirrors `PyModuleDef` from CPython 3.14 on x86_64, including
/// `PyModuleDef_Base` (`ob_refcnt`, `ob_type`, `m_init`, `m_index`,
/// `m_copy`) and the trailing name/doc/size/methods/slots/traverse/clear/free
/// fields. The name, method and slot fields are inspected.
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

/// Mirrors `PyModuleDef_Slot`: `{ slot, value }` followed by the padding that
/// aligns `value` to a pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct PyModuleDefSlotMirror {
    slot: c_int,
    value: *mut c_void,
}

// CPython 3.14 x86_64: one `int`, four bytes of padding, one pointer.
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
    ml_name: *const c_char,
    ml_meth: *mut c_void,
    ml_flags: c_int,
    ml_doc: *const c_char,
}

/// Mirrors `PyType_Slot`: `{ slot, pfunc }` followed by the padding that
/// aligns `pfunc` to a pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct PyTypeSlotMirror {
    slot: c_int,
    pfunc: *mut c_void,
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
#[allow(dead_code)]
pub(crate) struct PyTypeSpecMirror {
    name: *const c_char,
    basicsize: c_int,
    itemsize: c_int,
    flags: c_uint,
    slots: *mut PyTypeSlotMirror,
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

/// Opaque heap-type handle created by `PyType_FromSpec`.
///
/// This is the whole `PyTypeObject` representation of this slice: a name, the
/// allocation sizes, the accepted flags and the three slots the host can
/// honour. It deliberately carries no instance layout, no base types and no
/// GC hooks, so an extension cannot rely on CPython's `PyTypeObject` layout.
/// The handle lives inside an arena [`ShimObject`], whose reference count
/// already exists for future refcount/GC integration.
///
/// `basicsize`, `itemsize`, `doc` and `methods` are recorded from the spec so
/// a future allocation/attribute slice can use them without re-parsing the
/// extension's tables; nothing reads them yet.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct TypeHandle {
    /// Qualified name from the spec (`module.Type`).
    pub(crate) name: String,
    /// Last component of [`TypeHandle::name`], reported by `PyType_GetName`.
    pub(crate) short_name: String,
    pub(crate) basicsize: isize,
    pub(crate) itemsize: isize,
    pub(crate) flags: u32,
    /// `Py_tp_doc` value, parsed for future `__doc__` support.
    pub(crate) doc: Option<String>,
    /// Raw `Py_tp_doc` pointer, returned by `PyType_GetSlot`.
    pub(crate) doc_pointer: usize,
    /// Parsed `Py_tp_methods` table; retained for future attribute lookup.
    pub(crate) methods: Vec<MethodSpec>,
    /// Raw `Py_tp_methods` pointer, returned by `PyType_GetSlot`.
    pub(crate) methods_pointer: usize,
    /// `Py_tp_new` function pointer, invoked when the bridge constructs an
    /// instance of this type.
    pub(crate) new_function: usize,
}

/// `Py_tp_methods` entries are ordinary `PyMethodDef` tables.
impl TypeHandle {
    pub(crate) fn has_new(&self) -> bool {
        self.new_function != 0
    }
}

/// Mirrors `Py_buffer` from CPython 3.14 on x86_64 (`pybuffer.h`).
///
/// The layout and size are part of the stable ABI: a mismatch would corrupt
/// every extension that fills in a buffer view, so the sizes and offsets are
/// asserted at compile time below.
#[repr(C)]
#[allow(non_camel_case_types)]
pub(crate) struct Py_buffer {
    pub(crate) buf: *mut c_void,
    pub(crate) obj: *mut ShimObject,
    pub(crate) len: isize,
    pub(crate) itemsize: isize,
    pub(crate) readonly: c_int,
    pub(crate) ndim: c_int,
    pub(crate) format: *mut c_char,
    pub(crate) shape: *mut isize,
    pub(crate) strides: *mut isize,
    pub(crate) suboffsets: *mut isize,
    pub(crate) internal: *mut c_void,
}

// CPython 3.14 x86_64: eleven fields, 80 bytes, no padding surprises. The
// 64-bit gate keeps other pointer widths buildable, though only x86_64 is
// claimed as supported here.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<Py_buffer>() == 80);
    assert!(std::mem::offset_of!(Py_buffer, buf) == 0);
    assert!(std::mem::offset_of!(Py_buffer, obj) == 8);
    assert!(std::mem::offset_of!(Py_buffer, len) == 16);
    assert!(std::mem::offset_of!(Py_buffer, itemsize) == 24);
    assert!(std::mem::offset_of!(Py_buffer, readonly) == 32);
    assert!(std::mem::offset_of!(Py_buffer, ndim) == 36);
    assert!(std::mem::offset_of!(Py_buffer, format) == 40);
    assert!(std::mem::offset_of!(Py_buffer, shape) == 48);
    assert!(std::mem::offset_of!(Py_buffer, strides) == 56);
    assert!(std::mem::offset_of!(Py_buffer, suboffsets) == 64);
    assert!(std::mem::offset_of!(Py_buffer, internal) == 72);
};

/// `PyBUF_WRITABLE`: the only request bit this read-only shim rejects.
const PYBUF_WRITABLE: c_int = 0x0001;

// Shim values and objects

/// The crate-internal value contract shared with the extension loader.
///
/// Container variants are owned copies: passing a Hydro list to an extension
/// copies it, and a container returned by an extension is converted into a
/// fresh Hydro container. Nothing is shared across the boundary.
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
    /// Opaque pointer capsule. The pointer belongs to the extension; this
    /// host only carries it across calls. Destructors are never called in
    /// this slice, so the capsule's lifetime is the process lifetime.
    Capsule {
        ptr: usize,
        name: Option<String>,
    },
    /// Python's `NotImplemented` singleton: a real Hydro value.
    NotImplemented,
    /// Python's `Ellipsis` singleton. Hydro has no `Ellipsis` value, so this
    /// variant exists for the shim boundary only; returning one to Hydro
    /// raises a tagged error instead of inventing a representation.
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
    /// An opaque type handle created by `PyType_FromSpec`, stored as the
    /// address of its arena object. Calling it through [`call_type_object`]
    /// invokes its `Py_tp_new`.
    Type(usize),
}

/// Maximum nesting depth accepted when converting containers in either
/// direction. Beyond it the conversion reports an error instead of recursing
/// without bound.
pub(crate) const MAX_CONVERSION_DEPTH: usize = 64;

/// Internal representation of one shim object.
///
/// Container entries and module objects are stored as addresses because every
/// object lives in the arena for the whole process; that also keeps the
/// object `Send`/`Sync` without an unsafe impl. A `0` entry in a freshly
/// created list/tuple means "not filled in yet".
#[derive(Clone, Debug)]
enum ShimKind {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// `c_text` is the NUL-terminated form used by `PyArg_ParseTuple("s")`
    /// and `PyUnicode_AsUTF8`; it is absent when the Python string contains
    /// an embedded NUL.
    Str {
        text: String,
        c_text: Option<CString>,
    },
    /// `data` is the bytes content; `c_data` is `data` plus a trailing NUL,
    /// matching CPython's guarantee for `PyBytes_AsString`.
    Bytes {
        data: Vec<u8>,
        c_data: Vec<u8>,
    },
    List(Vec<usize>),
    Tuple(Vec<usize>),
    Dict(Vec<(usize, usize)>),
    /// `name` is kept as a `CString` so `PyCapsule_GetName` can hand out a
    /// stable `const char *`. `destructor` is stored for API fidelity but is
    /// never invoked (documented slice limitation).
    Capsule {
        ptr: usize,
        name: Option<CString>,
        destructor: usize,
    },
    /// Exported `PyExc_<Name>` singleton. It is a data symbol for extensions
    /// (`PyErr_SetString` reads its name), never a Python value that crosses
    /// back to user code.
    ErrorType {
        name: String,
    },
    /// `NotImplemented`, the exported `_Py_NotImplementedStruct`.
    NotImplemented,
    /// `Ellipsis`, the exported `_Py_EllipsisObject`.
    Ellipsis,
    /// Heap-type handle created by `PyType_FromSpec`; see [`TypeHandle`].
    Type(TypeHandle),
    Module(u64),
    /// A `PyModuleDef` wrapped by `PyModuleDef_Init`, holding the
    /// extension-owned definition address (stored as `usize` so every shim
    /// object stays `Send`/`Sync`) until the loader creates the module and
    /// runs its exec slots.
    ModuleDef {
        def: usize,
    },
    Error,
}

/// Private `#[repr(C)]` object handed to extensions as an opaque `PyObject *`.
///
/// `pub(crate)` only because the C ABI entry points below mention it in their
/// signatures; nothing outside the runtime sees the type or its layout.
#[repr(C)]
pub(crate) struct ShimObject {
    refcnt: AtomicIsize,
    kind: ShimKind,
}

/// Shape of the `PyInit_<name>` entry point in a limited-API extension.
type ExtensionInit = unsafe extern "C" fn() -> *mut ShimObject;

// Process-lifetime object arena

#[allow(clippy::vec_box)] // the box keeps every PyObject address stable
fn arena_lock() -> MutexGuard<'static, Vec<Box<ShimObject>>> {
    #[allow(clippy::vec_box)]
    static ARENA: LazyLock<Mutex<Vec<Box<ShimObject>>>> = LazyLock::new(|| Mutex::new(Vec::new()));
    ARENA.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Allocates a shim object in the process-lifetime arena; see the
/// "No finalization" limitation above.
fn new_object(kind: ShimKind) -> *mut ShimObject {
    let pointer = Box::into_raw(Box::new(ShimObject {
        refcnt: AtomicIsize::new(1),
        kind,
    }));
    // SAFETY: `pointer` was produced by `Box::into_raw` in this function and
    // has not been freed; reconstituting the `Box` exactly once transfers
    // ownership to the arena, which keeps the allocation alive until exit.
    arena_lock().push(unsafe { Box::from_raw(pointer) });
    pointer
}

/// Exported constant objects (`Py_None`, `Py_True`, ...).
///
/// Each symbol is a real shim object, not a pointer variable: an extension
/// that writes `&_Py_NoneStruct` (the pre-3.13 spelling of `Py_None`) hands
/// back an object this host can convert, and `Py_GetConstantBorrowed` returns
/// the same address, exactly as CPython guarantees. The refcount starts at one
/// and is never used to free anything (see the "No finalization" limitation).
macro_rules! constant_objects {
    ($($symbol:ident => $kind:expr),+ $(,)?) => {
        $(
            /// Exported CPython constant singleton.
            #[no_mangle]
            pub static mut $symbol: ShimObject = ShimObject {
                refcnt: AtomicIsize::new(1),
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

/// Interned constants without an exported data symbol (`0`, `1`, `""`, `b""`,
/// `()`), created once so `Py_GetConstantBorrowed` stays identity-stable.
fn interned_constants() -> &'static [usize; 5] {
    static INTERNED: LazyLock<[usize; 5]> = LazyLock::new(|| {
        [
            new_object(ShimKind::Int(0)) as usize,
            new_object(ShimKind::Int(1)) as usize,
            new_object(ShimKind::Str {
                text: String::new(),
                c_text: Some(CString::default()),
            }) as usize,
            new_object(ShimKind::Bytes {
                data: Vec::new(),
                c_data: vec![0],
            }) as usize,
            new_object(ShimKind::Tuple(Vec::new())) as usize,
        ]
    });
    &INTERNED
}

/// Resolves a `Py_GetConstant` identifier, or `None` when it is unknown.
fn constant_object(constant_id: c_uint) -> Option<*mut ShimObject> {
    let pointer = match constant_id {
        PY_CONSTANT_NONE => ptr::addr_of_mut!(_Py_NoneStruct),
        PY_CONSTANT_FALSE => ptr::addr_of_mut!(_Py_FalseStruct),
        PY_CONSTANT_TRUE => ptr::addr_of_mut!(_Py_TrueStruct),
        PY_CONSTANT_ELLIPSIS => ptr::addr_of_mut!(_Py_EllipsisObject),
        PY_CONSTANT_NOT_IMPLEMENTED => ptr::addr_of_mut!(_Py_NotImplementedStruct),
        PY_CONSTANT_ZERO => interned_constant(0),
        PY_CONSTANT_ONE => interned_constant(1),
        PY_CONSTANT_EMPTY_STR => interned_constant(2),
        PY_CONSTANT_EMPTY_BYTES => interned_constant(3),
        PY_CONSTANT_EMPTY_TUPLE => interned_constant(4),
        _ => return None,
    };
    Some(pointer)
}

/// Address of one entry of [`interned_constants`].
fn interned_constant(index: usize) -> *mut ShimObject {
    interned_constants()[index] as *mut ShimObject
}

/// `Py_GetConstant`: returns a new reference to a Python constant.
///
/// Unknown identifiers set a `TypeError` and return NULL, matching CPython's
/// contract that a NULL return always comes with an exception.
#[no_mangle]
pub extern "C" fn Py_GetConstant(constant_id: c_uint) -> *mut ShimObject {
    let Some(object) = constant_object(constant_id) else {
        set_error(format!(
            "TypeError: Py_GetConstant: unknown constant id {constant_id}"
        ));
        return ptr::null_mut();
    };
    // A new reference: balance the decref the caller owes. Shim objects are
    // never freed, so this is bookkeeping rather than lifetime management.
    // SAFETY: every constant lives in the arena or in a static for the whole
    // process, so its atomic refcount is addressable.
    let _ = unsafe { (*object).refcnt.fetch_add(1, Ordering::Relaxed) };
    object
}

/// `Py_GetConstantBorrowed`: returns a borrowed reference to a Python constant.
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

/// `Py_Is`: identity comparison, as CPython's macro spells it.
#[no_mangle]
pub extern "C" fn Py_Is(left: *mut ShimObject, right: *mut ShimObject) -> c_int {
    if left == right {
        1
    } else {
        0
    }
}

/// `Py_IsNone`: true for the `None` singleton.
#[no_mangle]
pub extern "C" fn Py_IsNone(object: *mut ShimObject) -> c_int {
    if object == ptr::addr_of_mut!(_Py_NoneStruct) {
        1
    } else {
        0
    }
}

/// `Py_IsTrue`: true for the `True` singleton.
#[no_mangle]
pub extern "C" fn Py_IsTrue(object: *mut ShimObject) -> c_int {
    if object == ptr::addr_of_mut!(_Py_TrueStruct) {
        1
    } else {
        0
    }
}

/// `Py_IsFalse`: true for the `False` singleton.
#[no_mangle]
pub extern "C" fn Py_IsFalse(object: *mut ShimObject) -> c_int {
    if object == ptr::addr_of_mut!(_Py_FalseStruct) {
        1
    } else {
        0
    }
}

/// Declares the `PyExc_<Name>` data symbols exposed to extensions.
///
/// Extensions reference these as `PyObject *` globals (for example
/// `PyErr_SetString(PyExc_ValueError, ...)`), so each symbol must be exported
/// and populated before any extension initializer runs. They are not Python
/// values: only the error setters inspect them, to derive the exception
/// category. The list mirrors CPython 3.14's `pyerrors.h`.
macro_rules! exception_type_symbols {
    ($($symbol:ident => $name:literal),+ $(,)?) => {
        $(
            /// Exported CPython exception-type singleton.
            #[no_mangle]
            pub static mut $symbol: *mut ShimObject = ptr::null_mut();
        )+

        /// Populates every exported exception singleton, exactly once.
        fn initialize_exception_types() {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| unsafe {
                $(
                    $symbol = new_object(ShimKind::ErrorType {
                        name: $name.to_string(),
                    });
                )+
            });
        }

        /// References every singleton so the staticlib linker keeps the data.
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

/// Maps an exported exception singleton to the loader's message prefix.
///
/// The prefix is the CPython class name; [`crate::modules::extension`] maps it
/// back to the Hydro exception tag, including the base classes. A NULL or
/// non-exception object reports as `RuntimeError`.
fn exception_prefix(exception: *mut ShimObject) -> &'static str {
    match kind_ref(exception) {
        // `name` lives in the process-lifetime arena, so the borrow really is
        // `'static`.
        Some(ShimKind::ErrorType { name }) => name.as_str(),
        _ => "RuntimeError",
    }
}

/// Turns a contract value into a shim object.
///
/// The immutable singletons (`None`, `True`, `False`, `NotImplemented`,
/// `Ellipsis`) are the exported `_Py_*Struct` objects themselves, so an
/// extension's identity checks (`arg == Py_None`, `Py_IsTrue(arg)`) behave as
/// they do under CPython. Every other value gets a fresh arena object.
fn value_object(value: ShimValue) -> *mut ShimObject {
    let kind = match value {
        ShimValue::None => return ptr::addr_of_mut!(_Py_NoneStruct),
        ShimValue::Bool(flag) => {
            return if flag {
                ptr::addr_of_mut!(_Py_TrueStruct)
            } else {
                ptr::addr_of_mut!(_Py_FalseStruct)
            };
        }
        ShimValue::NotImplemented => return ptr::addr_of_mut!(_Py_NotImplementedStruct),
        ShimValue::Ellipsis => return ptr::addr_of_mut!(_Py_EllipsisObject),
        ShimValue::Int(number) => ShimKind::Int(number),
        ShimValue::Float(number) => ShimKind::Float(number),
        ShimValue::Str(text) => {
            // A `CString` keeps the `const char *` handed out by
            // `PyArg_ParseTuple("s")` valid for the process lifetime.
            let c_text = CString::new(text.as_bytes()).ok();
            ShimKind::Str { text, c_text }
        }
        ShimValue::Bytes(data) => {
            // CPython promises that `PyBytes_AsString` returns a pointer to a
            // NUL-terminated buffer, so `c_data` carries the extra sentinel.
            let mut c_data = data.clone();
            c_data.push(0);
            ShimKind::Bytes { data, c_data }
        }
        ShimValue::List(items) => ShimKind::List(
            items
                .into_iter()
                .map(|item| value_object(item) as usize)
                .collect(),
        ),
        ShimValue::Tuple(items) => ShimKind::Tuple(
            items
                .into_iter()
                .map(|item| value_object(item) as usize)
                .collect(),
        ),
        ShimValue::Dict(pairs) => ShimKind::Dict(
            pairs
                .into_iter()
                .map(|(key, value)| (value_object(key) as usize, value_object(value) as usize))
                .collect(),
        ),
        ShimValue::Capsule { ptr, name } => ShimKind::Capsule {
            ptr,
            // Capsule names are C strings; an embedded NUL (possible in a
            // Hydro string) drops the name, matching the NULL-name case.
            name: name.and_then(|name| CString::new(name).ok()),
            destructor: 0,
        },
    };
    new_object(kind)
}

/// Builds the positional argument tuple passed to a `METH_VARARGS` method.
fn tuple_object(arguments: &[ShimValue]) -> *mut ShimObject {
    let items = arguments
        .iter()
        .cloned()
        .map(|value| value_object(value) as usize)
        .collect();
    new_object(ShimKind::Tuple(items))
}

/// Converts an extension's return object to the contract value.
fn object_to_value(object: *mut ShimObject) -> Result<ShimValue, String> {
    object_to_value_at(object, 0, &mut Vec::new())
}

/// Recursive body of [`object_to_value`].
///
/// `depth` bounds container nesting; `path` holds the addresses of the
/// containers currently being converted so an extension-built cycle is
/// reported instead of overflowing the stack. It is a `Vec` (at most
/// [`MAX_CONVERSION_DEPTH`] entries) so scalar results allocate nothing.
fn object_to_value_at(
    object: *mut ShimObject,
    depth: usize,
    path: &mut Vec<usize>,
) -> Result<ShimValue, String> {
    if object.is_null() {
        return Err("cannot convert a NULL object".to_string());
    }
    // SAFETY: every shim object lives in the arena for the process lifetime.
    match unsafe { &(*object).kind } {
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
        ShimKind::Module(_) => {
            Err("extension returned a module object, which is not supported".to_string())
        }
        ShimKind::ModuleDef { .. } => {
            Err("extension returned a module definition object, which is not supported".to_string())
        }
        ShimKind::Capsule { ptr, name, .. } => Ok(ShimValue::Capsule {
            ptr: *ptr,
            name: name
                .as_ref()
                .map(|name| name.to_string_lossy().into_owned()),
        }),
        ShimKind::ErrorType { name } => Err(format!(
            "extension returned the exception type '{name}', which is not supported"
        )),
        ShimKind::NotImplemented => Ok(ShimValue::NotImplemented),
        ShimKind::Ellipsis => Ok(ShimValue::Ellipsis),
        ShimKind::Type(handle) => Err(format!(
            "extension returned the type object '{}', which cannot be converted to a Hydro value",
            handle.name
        )),
        ShimKind::Error => Err("extension returned an error object".to_string()),
    }
}

/// Records one container on the conversion path after checking its depth.
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

/// Renders a shim object for `PyErr_SetObject`, which stores the exception
/// value rather than just a message.
///
/// Only the types the shim can represent are rendered; the result mirrors
/// Python's `repr()` for scalars and `str()` for strings, which is what
/// CPython shows for the common `PyErr_SetObject(Error, "text")` form.
/// Container nesting is bounded, so an extension-built cycle cannot recurse
/// forever.
fn shim_repr(object: *mut ShimObject) -> String {
    shim_repr_at(object, 0)
}

/// Recursive body of [`shim_repr`].
fn shim_repr_at(object: *mut ShimObject, depth: usize) -> String {
    if object.is_null() {
        return "<NULL>".to_string();
    }
    if depth >= MAX_CONVERSION_DEPTH {
        return "...".to_string();
    }
    match kind_ref(object) {
        Some(ShimKind::None) => "None".to_string(),
        Some(ShimKind::Bool(flag)) => (if *flag { "True" } else { "False" }).to_string(),
        Some(ShimKind::Int(number)) => number.to_string(),
        Some(ShimKind::Float(number)) => format!("{number:?}"),
        Some(ShimKind::Str { text, .. }) => format!("'{}'", text.escape_debug()),
        Some(ShimKind::Bytes { data, .. }) => {
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
        Some(ShimKind::List(items)) => {
            let items: Vec<String> = items
                .iter()
                .map(|item| shim_repr_at(*item as *mut ShimObject, depth + 1))
                .collect();
            format!("[{}]", items.join(", "))
        }
        Some(ShimKind::Tuple(items)) => {
            let items: Vec<String> = items
                .iter()
                .map(|item| shim_repr_at(*item as *mut ShimObject, depth + 1))
                .collect();
            match items.len() {
                1 => format!("({},)", items[0]),
                _ => format!("({})", items.join(", ")),
            }
        }
        Some(ShimKind::Dict(pairs)) => {
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
        Some(ShimKind::Capsule { name, .. }) => match name {
            Some(name) => format!("<capsule object \"{}\">", name.to_string_lossy()),
            None => "<capsule object NULL>".to_string(),
        },
        Some(ShimKind::ErrorType { name }) => format!("<class '{name}'>"),
        Some(ShimKind::NotImplemented) => "NotImplemented".to_string(),
        Some(ShimKind::Ellipsis) => "Ellipsis".to_string(),
        Some(ShimKind::Type(handle)) => format!("<class '{}'>", handle.name),
        Some(ShimKind::Module(_)) => "<module>".to_string(),
        Some(ShimKind::ModuleDef { .. }) => "<module definition>".to_string(),
        Some(ShimKind::Error) => "<error>".to_string(),
        None => "<NULL>".to_string(),
    }
}

// Thread-local error state

std::thread_local! {
    /// Pending error set by an extension or by a failing conversion. This is
    /// the extension-host analogue of CPython's thread state; it is separate
    /// from the Hydro runtime's own pending exception.
    static CALL_ERROR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn set_error(message: impl Into<String>) {
    CALL_ERROR.with(|slot| *slot.borrow_mut() = Some(message.into()));
}

/// Records an argument-conversion failure as a `TypeError`.
///
/// CPython raises `TypeError` for every `PyArg_Parse*` failure, so the
/// `"TypeError: "` prefix is what [`crate::modules::extension`] maps back to
/// the matching Hydro tag.
fn set_type_error(message: impl Into<String>) {
    set_error(format!("TypeError: {}", message.into()));
}

/// Records a conversion failure reported by `PyArg_ParseTuple`/`parse.c`.
///
/// CPython raises `TypeError` for argument-format failures; the
/// `TypeError: ` prefix lets [`crate::modules::extension`] map the stored
/// message to the right pending exception.
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

fn error_pending() -> bool {
    CALL_ERROR.with(|slot| slot.borrow().is_some())
}

fn take_error() -> Option<String> {
    CALL_ERROR.with(|slot| slot.borrow_mut().take())
}

// Conversion helpers for the C entry points

/// Reads a NUL-terminated C string, replacing invalid UTF-8 lossily.
fn read_c_string(pointer: *const c_char) -> String {
    if pointer.is_null() {
        return String::new();
    }
    // SAFETY: callers only pass NUL-terminated strings (CPython C API contract).
    unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned()
}

fn as_int(object: *mut ShimObject) -> Result<i64, String> {
    if object.is_null() {
        return Err("cannot convert a NULL object to an integer".to_string());
    }
    // SAFETY: every shim object lives in the arena for the process lifetime.
    match unsafe { &(*object).kind } {
        ShimKind::Int(number) => Ok(*number),
        ShimKind::Bool(flag) => Ok(*flag as i64),
        _ => Err("an integer is required".to_string()),
    }
}

fn as_float(object: *mut ShimObject) -> Result<f64, String> {
    if object.is_null() {
        return Err("cannot convert a NULL object to a float".to_string());
    }
    // SAFETY: every shim object lives in the arena for the process lifetime.
    match unsafe { &(*object).kind } {
        ShimKind::Float(number) => Ok(*number),
        ShimKind::Int(number) => Ok(*number as f64),
        ShimKind::Bool(flag) => Ok(if *flag { 1.0 } else { 0.0 }),
        _ => Err("a float is required".to_string()),
    }
}

fn as_predicate(object: *mut ShimObject) -> Result<bool, String> {
    if object.is_null() {
        return Err("cannot convert a NULL object to a predicate".to_string());
    }
    // SAFETY: every shim object lives in the arena for the process lifetime.
    Ok(match unsafe { &(*object).kind } {
        ShimKind::None => false,
        ShimKind::Bool(flag) => *flag,
        ShimKind::Int(number) => *number != 0,
        ShimKind::Float(number) => *number != 0.0,
        ShimKind::Str { text, .. } => !text.is_empty(),
        ShimKind::Bytes { data, .. } => !data.is_empty(),
        ShimKind::List(items) => !items.is_empty(),
        ShimKind::Tuple(items) => !items.is_empty(),
        ShimKind::Dict(pairs) => !pairs.is_empty(),
        ShimKind::Capsule { .. } => true,
        ShimKind::ErrorType { .. } => true,
        ShimKind::NotImplemented => true,
        ShimKind::Ellipsis => true,
        ShimKind::Type(_) => true,
        ShimKind::Module(_) | ShimKind::ModuleDef { .. } | ShimKind::Error => true,
    })
}

/// Borrows the kind of a non-NULL shim object.
fn kind_ref(object: *mut ShimObject) -> Option<&'static ShimKind> {
    if object.is_null() {
        return None;
    }
    // SAFETY: every shim object lives in the arena for the process lifetime.
    Some(unsafe { &(*object).kind })
}

/// Borrows a list's item slots, rejecting NULL and non-list objects.
fn list_slots(object: *mut ShimObject) -> Result<&'static [usize], String> {
    match kind_ref(object) {
        Some(ShimKind::List(items)) => Ok(items.as_slice()),
        Some(_) => Err("TypeError: expected a list object".to_string()),
        None => Err("TypeError: expected a list object, got NULL".to_string()),
    }
}

/// Mutably borrows a list's item slots.
fn list_slots_mut(object: *mut ShimObject) -> Result<&'static mut Vec<usize>, String> {
    if object.is_null() {
        return Err("TypeError: expected a list object, got NULL".to_string());
    }
    // SAFETY: every shim object lives in the arena for the process lifetime.
    match unsafe { &mut (*object).kind } {
        ShimKind::List(items) => Ok(items),
        _ => Err("TypeError: expected a list object".to_string()),
    }
}

/// Borrows a tuple's item slots, rejecting NULL and non-tuple objects.
fn tuple_slots(object: *mut ShimObject) -> Result<&'static [usize], String> {
    match kind_ref(object) {
        Some(ShimKind::Tuple(items)) => Ok(items.as_slice()),
        Some(_) => Err("TypeError: expected a tuple object".to_string()),
        None => Err("TypeError: expected a tuple object, got NULL".to_string()),
    }
}

/// Mutably borrows a tuple's item slots.
fn tuple_slots_mut(object: *mut ShimObject) -> Result<&'static mut Vec<usize>, String> {
    if object.is_null() {
        return Err("TypeError: expected a tuple object, got NULL".to_string());
    }
    // SAFETY: every shim object lives in the arena for the process lifetime.
    match unsafe { &mut (*object).kind } {
        ShimKind::Tuple(items) => Ok(items),
        _ => Err("TypeError: expected a tuple object".to_string()),
    }
}

/// Borrows a dict's entry slots, rejecting NULL and non-dict objects.
fn dict_slots(object: *mut ShimObject) -> Result<&'static [(usize, usize)], String> {
    match kind_ref(object) {
        Some(ShimKind::Dict(pairs)) => Ok(pairs.as_slice()),
        Some(_) => Err("TypeError: expected a dict object".to_string()),
        None => Err("TypeError: expected a dict object, got NULL".to_string()),
    }
}

/// Mutably borrows a dict's entry slots.
fn dict_slots_mut(object: *mut ShimObject) -> Result<&'static mut Vec<(usize, usize)>, String> {
    if object.is_null() {
        return Err("TypeError: expected a dict object, got NULL".to_string());
    }
    // SAFETY: every shim object lives in the arena for the process lifetime.
    match unsafe { &mut (*object).kind } {
        ShimKind::Dict(pairs) => Ok(pairs),
        _ => Err("TypeError: expected a dict object".to_string()),
    }
}

/// Borrows a bytes object's content, rejecting NULL and non-bytes objects.
fn bytes_data(object: *mut ShimObject) -> Result<&'static [u8], String> {
    match kind_ref(object) {
        Some(ShimKind::Bytes { data, .. }) => Ok(data.as_slice()),
        Some(_) => Err("TypeError: expected a bytes object".to_string()),
        None => Err("TypeError: expected a bytes object, got NULL".to_string()),
    }
}

/// Borrows a bytes object's NUL-terminated buffer.
fn bytes_c_data(object: *mut ShimObject) -> Result<&'static [u8], String> {
    match kind_ref(object) {
        Some(ShimKind::Bytes { c_data, .. }) => Ok(c_data.as_slice()),
        Some(_) => Err("TypeError: expected a bytes object".to_string()),
        None => Err("TypeError: expected a bytes object, got NULL".to_string()),
    }
}

/// Structural equality for dict keys: pointer identity first, then the scalar
/// types the shim can represent (numerics compare across int/float/bool).
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
            (*a as i64) == *b
        }
        (ShimKind::Bool(a), ShimKind::Float(b)) | (ShimKind::Float(b), ShimKind::Bool(a)) => {
            (if *a { 1.0 } else { 0.0 }) == *b
        }
        (ShimKind::Int(a), ShimKind::Float(b)) | (ShimKind::Float(b), ShimKind::Int(a)) => {
            (*a as f64) == *b
        }
        (ShimKind::Str { text: a, .. }, ShimKind::Str { text: b, .. }) => a == b,
        (ShimKind::Bytes { data: a, .. }, ShimKind::Bytes { data: b, .. }) => a == b,
        _ => false,
    }
}

/// Finds the entry storing `key` (structural equality) in a dict's slots.
fn dict_position(pairs: &[(usize, usize)], key: *mut ShimObject) -> Option<usize> {
    pairs
        .iter()
        .position(|&(existing, _)| key_equal(existing as *mut ShimObject, key))
}

/// Stores a key/value pair, replacing an equal existing key.
fn dict_set(
    dict: *mut ShimObject,
    key: *mut ShimObject,
    value: *mut ShimObject,
) -> Result<(), String> {
    let position = dict_position(dict_slots(dict)?, key);
    let pairs = dict_slots_mut(dict)?;
    match position {
        Some(index) => {
            pairs[index].1 = value as usize;
            Ok(())
        }
        None => {
            if pairs.try_reserve(1).is_err() {
                return Err("MemoryError: cannot grow dict".to_string());
            }
            pairs.push((key as usize, value as usize));
            Ok(())
        }
    }
}

/// Returns the NUL-terminated text stored in a string object.
///
/// The pointer stays valid because the owning object lives in the arena for
/// the process lifetime.
fn as_c_string(object: *mut ShimObject) -> Result<*const c_char, String> {
    if object.is_null() {
        return Err("cannot read a string from a NULL object".to_string());
    }
    // SAFETY: every shim object lives in the arena for the process lifetime;
    // the returned pointer refers to its persisted `CString`.
    match unsafe { &(*object).kind } {
        ShimKind::Str {
            c_text: Some(text), ..
        } => Ok(text.as_ptr()),
        ShimKind::Str { c_text: None, .. } => {
            Err("string contains an embedded null character".to_string())
        }
        _ => Err("a string is required".to_string()),
    }
}

/// Stores one converted argument through the pointer a `va_arg` supplied.
fn store_argument<T>(out: *mut T, value: T, format_character: &str) -> c_int {
    if out.is_null() {
        set_type_error(format!("NULL output pointer for '{format_character}'"));
        return 0;
    }
    // SAFETY: the caller of `PyArg_ParseTuple` passed a pointer to a writable
    // `T` for exactly this format character, as the CPython API requires.
    unsafe { out.write(value) };
    1
}

// Module registry

/// Parsed, owned copy of one `PyMethodDef` entry.
#[derive(Clone, Debug)]
struct MethodSpec {
    name: String,
    flags: c_int,
    function: usize,
}

/// One registered extension module. The `dlopen` handle is retained for the
/// process lifetime; the module is never unloaded.
struct ModuleEntry {
    name: String,
    /// Raw `dlopen` handle; never closed. Kept for bookkeeping and future
    /// introspection.
    #[allow(dead_code)]
    handle: usize,
    /// Address of the shim object returned by `PyModule_Create2`; modules are
    /// arena-allocated and never freed.
    module_object: usize,
    /// Values and callables share one namespace so `PyModule_AddStringConstant`
    /// can overwrite a method name, exactly like a real module dictionary.
    attrs: HashMap<String, ShimAttr>,
    methods: Vec<MethodSpec>,
}

struct Registry {
    next_id: u64,
    modules: HashMap<u64, ModuleEntry>,
    /// Successfully loaded modules keyed by shared-object path. A path is
    /// loaded once per process, so multi-phase exec slots run exactly once.
    by_path: HashMap<String, u64>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            // Module ids start at 1; 0 is never a valid module.
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
///
/// `owner` describes the table's owner (`"extension module 'foo'"` or
/// `"type 'foo'"`) for diagnostics. A NULL table reads as no methods.
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
        // SAFETY: `methods` points at a NULL-terminated array of
        // `PyMethodDef`, so the current slot is readable until a NULL name.
        let method = unsafe { &*cursor };
        if method.ml_name.is_null() {
            break;
        }
        if entries.len() >= MAX_METHODS {
            return Err(format!("{owner} defines more than {MAX_METHODS} methods"));
        }
        // SAFETY: `ml_name` is a NUL-terminated C string.
        let method_name = unsafe { CStr::from_ptr(method.ml_name) }
            .to_string_lossy()
            .into_owned();
        if method.ml_meth.is_null() {
            return Err(format!(
                "{owner} method '{method_name}' has a NULL implementation"
            ));
        }
        entries.push(MethodSpec {
            name: method_name,
            flags: method.ml_flags,
            function: method.ml_meth as usize,
        });
        // SAFETY: advancing one entry inside the NULL-terminated table.
        cursor = unsafe { cursor.add(1) };
    }
    Ok(entries)
}

/// Reads the module name and method table out of a `PyModuleDef`.
fn read_module_definition(
    definition: *mut PyModuleDefMirror,
) -> Result<(String, Vec<MethodSpec>), String> {
    if definition.is_null() {
        return Err("extension module definition is NULL".to_string());
    }
    // SAFETY: the extension owns a live `PyModuleDef` for at least the
    // duration of its `PyInit_*` call; the mirror matches CPython 3.14 x86_64.
    let definition = unsafe { &*definition };
    if definition.m_name.is_null() {
        return Err("extension module definition has no name".to_string());
    }
    // SAFETY: `m_name` is a NUL-terminated C string by the CPython contract.
    let name = unsafe { CStr::from_ptr(definition.m_name) }
        .to_string_lossy()
        .into_owned();
    let methods = read_method_table(definition.m_methods, &format!("extension module '{name}'"))?;
    Ok((name, methods))
}

/// Parses `definition`, allocates a module shim and registers it.
///
/// Shared by `PyModule_Create2` (single-phase) and the multi-phase loader so
/// both forms produce identical attribute and method behaviour.
fn create_module_from_definition(
    definition: *mut PyModuleDefMirror,
) -> Result<(u64, *mut ShimObject), String> {
    let (name, methods) = read_module_definition(definition)?;
    let id = allocate_module_id();
    let module_object = new_object(ShimKind::Module(id));

    let mut attrs = HashMap::with_capacity(methods.len());
    for (index, method) in methods.iter().enumerate() {
        // `MAX_METHODS` keeps this conversion trivially in range.
        let method_index = index as u32;
        attrs.insert(
            method.name.clone(),
            ShimAttr::Callable(ShimCallable {
                module: id,
                method: method_index,
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

/// Reads a definition's module name for diagnostics, tolerating a missing
/// name (which `read_module_definition` rejects before slots run).
fn definition_name(definition: &PyModuleDefMirror) -> String {
    if definition.m_name.is_null() {
        "<unnamed>".to_string()
    } else {
        read_c_string(definition.m_name)
    }
}

/// Signature of a `Py_mod_exec` slot callback.
type ModuleExec = unsafe extern "C" fn(*mut ShimObject) -> c_int;

/// Runs the `Py_mod_exec` callbacks declared by a multi-phase definition.
///
/// The NULL-terminated `m_slots` array is walked at most [`MAX_MODULE_SLOTS`]
/// times. Policy-only slots are validated rather than silently ignored, and
/// `Py_mod_create` is outside this host's model, so both are reported as
/// errors. A failing callback aborts the load with its pending error.
fn run_module_slots(
    definition: *mut PyModuleDefMirror,
    module: *mut ShimObject,
) -> Result<(), String> {
    if definition.is_null() {
        return Err("PyModuleDef_Init: module definition is NULL".to_string());
    }
    // SAFETY: the extension owns a live `PyModuleDef` for at least the
    // duration of its initialisation; the mirror matches CPython 3.14 x86_64.
    let definition = unsafe { &*definition };
    if definition.m_slots.is_null() {
        return Ok(());
    }
    let name = definition_name(definition);
    let mut cursor = definition.m_slots;
    for index in 0..MAX_MODULE_SLOTS {
        // SAFETY: `m_slots` points at a NULL-terminated array of
        // `PyModuleDef_Slot`, so the current entry is readable.
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
                // SAFETY: a `Py_mod_exec` value is `int (*)(PyObject *)`.
                let callback: ModuleExec = unsafe { std::mem::transmute(slot.value) };
                clear_error();
                // SAFETY: `module` is a live module shim and the extension
                // owns the callback for the process lifetime.
                let status = unsafe { callback(module) };
                if status != 0 {
                    return Err(take_error().unwrap_or_else(|| {
                        format!(
                            "Py_mod_exec callback {index} of module definition '{name}' failed with status {status}"
                        )
                    }));
                }
                // A successful callback that left an error behind is an
                // extension bug; do not leak it into later shim calls.
                clear_error();
            }
            PY_MOD_MULTIPLE_INTERPRETERS => {
                let value = slot.value as isize;
                if !(0..=PY_MOD_MULTIPLE_INTERPRETERS_PER_INTERPRETER_GIL_SUPPORTED as isize)
                    .contains(&value)
                {
                    return Err(format!(
                        "module definition '{name}' has an invalid Py_mod_multiple_interpreters value {value}"
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
        "module definition '{name}' has more than {MAX_MODULE_SLOTS} slots without a zero terminator"
    ))
}

/// Validates that `object` is a shim module object and returns its id.
fn module_id(object: *mut ShimObject) -> Result<u64, String> {
    if object.is_null() {
        return Err("module object is NULL".to_string());
    }
    // SAFETY: every shim object lives in the arena for the process lifetime.
    match unsafe { &(*object).kind } {
        ShimKind::Module(id) => Ok(*id),
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

// Loader contract

/// Loads an extension shared object and runs its `PyInit_<init_name>`.
///
/// `init_name` is the bare module name (`foo`, not `PyInit_foo`). The
/// `dlopen` handle is kept open for the process lifetime, and successful loads
/// are cached by path so later attribute accesses reuse the same module.
/// Returns the module id used by [`get_attr`] and [`call_method`].
///
/// A single-phase initializer returns a module object from
/// `PyModule_Create2`; a multi-phase one returns a definition wrapped by
/// [`PyModuleDef_Init`], in which case the module is created here and its
/// `Py_mod_exec` slots run before the load is reported as successful.
pub(crate) fn load_extension(path: &str, init_name: &str) -> Result<u64, String> {
    if let Some(module) = registry_lock().by_path.get(path).copied() {
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

    // SAFETY: `handle` is a live handle; `symbol` is NUL-terminated.
    let entry_point = unsafe { libc::dlsym(handle, symbol.as_ptr()) };
    if entry_point.is_null() {
        let error = dl_error();
        // SAFETY: no initializer ran, so closing the handle cannot invalidate
        // state the process might still use.
        unsafe { libc::dlclose(handle) };
        return Err(error);
    }

    // SAFETY: `PyInit_*` in a limited-API extension has the CPython module
    // initialization signature (no arguments, returns `PyObject *`).
    let init: ExtensionInit = unsafe { std::mem::transmute(entry_point) };
    // SAFETY: calling the initializer with no arguments is exactly what
    // CPython does. It runs on this thread without the registry lock held, so
    // its calls back into `PyModule_Create2`/`PyModule_AddStringConstant` can
    // take the lock.
    let result = unsafe { init() };
    if result.is_null() {
        let error = take_error().unwrap_or_else(|| format!("PyInit_{init_name} returned NULL"));
        // SAFETY: the initializer failed and handed nothing back; the module
        // cannot be called later.
        unsafe { libc::dlclose(handle) };
        return Err(error);
    }
    // A non-NULL return with a stale error is an extension bug; CPython
    // discards it, and keeping it would misattribute a later slot failure.
    clear_error();

    // SAFETY: `result` points into the arena and stays alive for the process.
    let kind = unsafe { (*result).kind.clone() };
    let id = match kind {
        ShimKind::Module(id) => id,
        ShimKind::ModuleDef { def } => {
            let definition = def as *mut PyModuleDefMirror;
            let (id, module_object) = match create_module_from_definition(definition) {
                Ok(created) => created,
                Err(error) => {
                    // SAFETY: no module was registered, so nothing can call
                    // back into the extension through the registry.
                    unsafe { libc::dlclose(handle) };
                    return Err(error);
                }
            };
            if let Err(error) = run_module_slots(definition, module_object) {
                // SAFETY: the partially initialised module is discarded and
                // never becomes reachable through the registry.
                unsafe { libc::dlclose(handle) };
                let _ = registry_lock().modules.remove(&id);
                return Err(error);
            }
            id
        }
        _ => {
            // SAFETY: no shim module was produced; nothing can call into the
            // extension through the registry.
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

/// Extracts the pending `dlerror` message for the current thread.
fn dl_error() -> String {
    // SAFETY: `dlerror` returns either NULL or a NUL-terminated message owned
    // by the dynamic loader.
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
///
/// The registry lock is released before the extension runs; no GIL is
/// emulated (see the module documentation).
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

    if spec.function == 0 {
        return Err(format!("{}() has a NULL implementation", spec.name));
    }
    let flags = spec.flags;
    if (flags & !METHOD_FLAGS) != 0 {
        return Err(format!(
            "{}() uses unsupported METH flags {flags:#x}",
            spec.name
        ));
    }

    let has_varargs = (flags & METH_VARARGS) != 0;
    let has_keywords = (flags & METH_KEYWORDS) != 0;
    let has_noargs = (flags & METH_NOARGS) != 0;
    let has_o = (flags & METH_O) != 0;
    let has_fastcall = (flags & METH_FASTCALL) != 0;

    // SAFETY: `function` came from a `PyMethodDef.ml_meth` slot; the branch
    // below selects the signature implied by its METH_* flags.
    let address = spec.function as *const c_void;

    if has_fastcall {
        // `METH_FASTCALL` shares the `METH_KEYWORDS` bit, so it is dispatched
        // before the METH_VARARGS forms and never treated as one.
        if (flags & !(METH_FASTCALL | METH_KEYWORDS)) != 0 {
            return Err(format!(
                "{}() combines METH_FASTCALL with unsupported METH flags {flags:#x}",
                spec.name
            ));
        }
        // The vector lives on this stack frame; the extension must not retain
        // it, exactly as CPython's vectorcall contract states. For a zero-arity
        // call this is a dangling (but non-NULL and aligned) pointer that the
        // callee must not dereference, which mirrors CPython's `nargs == 0`
        // contract.
        let objects: Vec<*mut ShimObject> = args.iter().cloned().map(value_object).collect();
        let pointer = objects.as_ptr();
        let count = objects.len() as isize;
        clear_error();
        if has_keywords {
            // SAFETY: METH_FASTCALL | METH_KEYWORDS is
            // `_PyCFunctionFastWithKeywords`: (self, args, nargs, kwnames).
            let function: unsafe extern "C" fn(
                *mut ShimObject,
                *const *mut ShimObject,
                isize,
                *mut ShimObject,
            ) -> *mut ShimObject = unsafe { std::mem::transmute(address) };
            // SAFETY: the argument objects live in the arena; `kwnames` is
            // NULL because the Hydro bridge supplies no keyword arguments.
            let result = unsafe { function(module_object, pointer, count, ptr::null_mut()) };
            return convert_result(result);
        }
        // SAFETY: METH_FASTCALL is `_PyCFunctionFast`:
        // (self, args, nargs).
        let function: unsafe extern "C" fn(
            *mut ShimObject,
            *const *mut ShimObject,
            isize,
        ) -> *mut ShimObject = unsafe { std::mem::transmute(address) };
        // SAFETY: the argument objects live in the arena for the call.
        let result = unsafe { function(module_object, pointer, count) };
        return convert_result(result);
    }

    if has_noargs && !has_varargs && !has_keywords && !has_o {
        if !args.is_empty() {
            return Err(format!("{}() takes no arguments", spec.name));
        }
        // SAFETY: METH_NOARGS is a two-argument PyCFunction receiving NULL in
        // place of the argument object.
        let function: unsafe extern "C" fn(*mut ShimObject, *mut ShimObject) -> *mut ShimObject =
            unsafe { std::mem::transmute(address) };
        clear_error();
        // SAFETY: `module_object` is the shim module and `NULL` is the
        // documented METH_NOARGS argument.
        let result = unsafe { function(module_object, ptr::null_mut()) };
        return convert_result(result);
    }

    if has_o && !has_varargs && !has_keywords && !has_noargs {
        if args.len() != 1 {
            return Err(format!("{}() takes exactly one argument", spec.name));
        }
        let argument = value_object(args[0].clone());
        // SAFETY: METH_O is a PyCFunction whose second argument is the single
        // positional argument object.
        let function: unsafe extern "C" fn(*mut ShimObject, *mut ShimObject) -> *mut ShimObject =
            unsafe { std::mem::transmute(address) };
        clear_error();
        // SAFETY: both objects live in the arena for the process lifetime.
        let result = unsafe { function(module_object, argument) };
        return convert_result(result);
    }

    if has_varargs && !has_noargs && !has_o {
        let tuple = tuple_object(args);
        clear_error();
        if has_keywords {
            // SAFETY: METH_VARARGS | METH_KEYWORDS is the three-argument form
            // (self, args, kwargs); no keyword dictionary is provided.
            let function: unsafe extern "C" fn(
                *mut ShimObject,
                *mut ShimObject,
                *mut ShimObject,
            ) -> *mut ShimObject = unsafe { std::mem::transmute(address) };
            // SAFETY: the tuple and module live in the arena; kwargs is NULL
            // as CPython allows.
            let result = unsafe { function(module_object, tuple, ptr::null_mut()) };
            return convert_result(result);
        }
        // SAFETY: METH_VARARGS is a PyCFunction receiving (self, tuple).
        let function: unsafe extern "C" fn(*mut ShimObject, *mut ShimObject) -> *mut ShimObject =
            unsafe { std::mem::transmute(address) };
        // SAFETY: both objects live in the arena for the process lifetime.
        let result = unsafe { function(module_object, tuple) };
        return convert_result(result);
    }

    Err(format!(
        "{}() uses unsupported METH flags {flags:#x}",
        spec.name
    ))
}

/// Converts the extension's return object, honouring the pending error on NULL.
fn convert_result(result: *mut ShimObject) -> Result<ShimValue, String> {
    if result.is_null() {
        return Err(take_error().unwrap_or_else(|| "extension function failed".to_string()));
    }
    // CPython tolerates a non-NULL return with an error set by reporting a
    // SystemError; this host simply discards the stale error.
    clear_error();
    object_to_value(result)
}

// Exported CPython limited-API surface

/// `PyModule_Create2`: registers a module parsed from a `PyModuleDef`.
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
///
/// CPython returns the definition itself cast to `PyObject *`. This host
/// records the pointer in an arena shim so the loader can tell the multi-phase
/// result apart from a module object before running the definition's slots.
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

/// `PyModule_AddStringConstant`: adds (or overwrites) a string attribute.
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

/// `PyModule_AddObject`: adds (or overwrites) an attribute from an object.
///
/// Type handles created by `PyType_FromSpec` are stored as handles so the
/// extension bridge can construct instances; every other object is converted
/// to a contract value.
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
    let attribute = match kind_ref(value) {
        Some(ShimKind::Type(_)) => ShimAttr::Type(value as usize),
        _ => match object_to_value(value) {
            Ok(value) => ShimAttr::Value(value),
            Err(error) => {
                set_error(error);
                return -1;
            }
        },
    };
    match add_attribute(module, name, attribute) {
        Ok(()) => 0,
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

// Type handles

/// Borrows the handle of a type object, rejecting NULL and non-type objects.
fn type_handle(object: *mut ShimObject) -> Result<&'static TypeHandle, String> {
    match kind_ref(object) {
        Some(ShimKind::Type(handle)) => Ok(handle),
        Some(_) => Err("TypeError: expected a type object".to_string()),
        None => Err("TypeError: expected a type object, got NULL".to_string()),
    }
}

/// Names a `PyType_Slot` identifier for the unsupported-slot diagnostic.
///
/// The numbers come from CPython 3.14's `typeslots.h`, which fixes them as part
/// of the stable ABI.
fn type_slot_name(slot: c_int) -> &'static str {
    match slot {
        1..=46 => "buffer/number/sequence/mapping protocol slot",
        47 => "Py_tp_alloc",
        48 => "Py_tp_base",
        49 => "Py_tp_bases",
        50 => "Py_tp_call",
        51 => "Py_tp_clear",
        52 => "Py_tp_dealloc",
        53 => "Py_tp_del",
        54 => "Py_tp_descr_get",
        55 => "Py_tp_descr_set",
        57 => "Py_tp_getattr",
        58 => "Py_tp_getattro",
        59 => "Py_tp_hash",
        60 => "Py_tp_init",
        61 => "Py_tp_is_gc",
        62 => "Py_tp_iter",
        63 => "Py_tp_iternext",
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

/// Parses a `PyType_Spec` into a [`TypeHandle`].
///
/// Only the represented slots are accepted: `Py_tp_new` (construction),
/// `Py_tp_methods` (method table) and `Py_tp_doc` (documentation string).
/// Every other slot is a tagged error, because this host has no CPython type
/// layout to fill in.
fn read_type_spec(spec: *mut PyTypeSpecMirror) -> Result<TypeHandle, String> {
    if spec.is_null() {
        return Err("TypeError: PyType_FromSpec: type spec is NULL".to_string());
    }
    // SAFETY: the extension owns a live `PyType_Spec` for the duration of the
    // call; the mirror matches CPython 3.14 x86_64.
    let spec = unsafe { &*spec };
    if spec.name.is_null() {
        return Err("TypeError: PyType_FromSpec: type spec has no name".to_string());
    }
    // SAFETY: `name` is a NUL-terminated C string by the CPython contract.
    let name = unsafe { CStr::from_ptr(spec.name) }
        .to_string_lossy()
        .into_owned();
    if name.is_empty() {
        return Err("TypeError: PyType_FromSpec: type spec name is empty".to_string());
    }
    if spec.basicsize < 0 {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' has a negative basicsize {}",
            spec.basicsize
        ));
    }
    if spec.itemsize < 0 {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' has a negative itemsize {}",
            spec.itemsize
        ));
    }
    let unsupported_flags = spec.flags & !SUPPORTED_TYPE_FLAGS;
    if unsupported_flags != 0 {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' requests unsupported flags {unsupported_flags:#x} \
             (subtyping, GC and managed layouts are not supported)"
        ));
    }
    if spec.slots.is_null() {
        return Err(format!(
            "TypeError: PyType_FromSpec: type '{name}' has no slot table"
        ));
    }

    let short_name = match name.rsplit_once('.') {
        Some((_, short)) => short.to_string(),
        None => name.clone(),
    };
    let mut handle = TypeHandle {
        name: name.clone(),
        short_name,
        basicsize: spec.basicsize as isize,
        itemsize: spec.itemsize as isize,
        flags: spec.flags,
        doc: None,
        doc_pointer: 0,
        methods: Vec::new(),
        methods_pointer: 0,
        new_function: 0,
    };

    let mut cursor = spec.slots;
    let mut terminated = false;
    for _ in 0..MAX_TYPE_SLOTS {
        // SAFETY: `slots` points at a NULL-terminated array of `PyType_Slot`,
        // so the current entry is readable until the zero terminator.
        let slot = unsafe { &*cursor };
        if slot.slot == 0 {
            terminated = true;
            break;
        }
        match slot.slot {
            PY_TP_METHODS => {
                if slot.pfunc.is_null() {
                    return Err(format!(
                        "TypeError: PyType_FromSpec: type '{name}' has a NULL Py_tp_methods table"
                    ));
                }
                handle.methods = read_method_table(
                    slot.pfunc as *mut PyMethodDefMirror,
                    &format!("type '{name}'"),
                )?;
                handle.methods_pointer = slot.pfunc as usize;
            }
            PY_TP_NEW => {
                if slot.pfunc.is_null() {
                    return Err(format!(
                        "TypeError: PyType_FromSpec: type '{name}' has a NULL Py_tp_new"
                    ));
                }
                handle.new_function = slot.pfunc as usize;
            }
            PY_TP_DOC => {
                if !slot.pfunc.is_null() {
                    handle.doc = Some(read_c_string(slot.pfunc as *const c_char));
                    handle.doc_pointer = slot.pfunc as usize;
                }
            }
            other => {
                return Err(format!(
                    "TypeError: PyType_FromSpec: type '{name}' uses unsupported type slot {other} \
                     ({}); this host only represents Py_tp_new, Py_tp_methods and Py_tp_doc",
                    type_slot_name(other)
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
    Ok(handle)
}

/// `PyType_FromSpec`: creates an opaque type handle from a `PyType_Spec`.
#[no_mangle]
pub extern "C" fn PyType_FromSpec(spec: *mut PyTypeSpecMirror) -> *mut ShimObject {
    match read_type_spec(spec) {
        Ok(handle) => new_object(ShimKind::Type(handle)),
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyType_FromSpecWithBases`: `PyType_FromSpec` without base types.
///
/// Base classes are not represented, so only a NULL or empty bases tuple is
/// accepted; anything else is a tagged error rather than a silent mis-emulation.
#[no_mangle]
pub extern "C" fn PyType_FromSpecWithBases(
    spec: *mut PyTypeSpecMirror,
    bases: *mut ShimObject,
) -> *mut ShimObject {
    match kind_ref(bases) {
        Some(ShimKind::Tuple(items)) if items.is_empty() => PyType_FromSpec(spec),
        Some(ShimKind::Tuple(_)) => {
            set_error("TypeError: PyType_FromSpecWithBases: base types are not supported");
            ptr::null_mut()
        }
        _ => {
            // Base objects (not a tuple) are equally outside the model.
            if bases.is_null() {
                PyType_FromSpec(spec)
            } else {
                set_error("TypeError: PyType_FromSpecWithBases: base types are not supported");
                ptr::null_mut()
            }
        }
    }
}

/// `PyType_GetName`: returns a new reference to the type's short name.
#[no_mangle]
pub extern "C" fn PyType_GetName(object: *mut ShimObject) -> *mut ShimObject {
    match type_handle(object) {
        Ok(handle) => new_object(ShimKind::Str {
            text: handle.short_name.clone(),
            // `short_name` came from a C string, so it has no embedded NUL.
            c_text: CString::new(handle.short_name.as_str()).ok(),
        }),
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyType_GetQualName`: returns a new reference to the qualified type name.
#[no_mangle]
pub extern "C" fn PyType_GetQualName(object: *mut ShimObject) -> *mut ShimObject {
    match type_handle(object) {
        Ok(handle) => new_object(ShimKind::Str {
            text: handle.name.clone(),
            c_text: CString::new(handle.name.as_str()).ok(),
        }),
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyType_GetFlags`: returns the flags the spec requested.
#[no_mangle]
pub extern "C" fn PyType_GetFlags(object: *mut ShimObject) -> c_ulong {
    match type_handle(object) {
        Ok(handle) => handle.flags as c_ulong,
        Err(error) => {
            set_error(error);
            0
        }
    }
}

/// `PyType_GetSlot`: returns the recorded slot function pointer.
///
/// A slot this handle does not carry yields NULL without setting an error, as
/// CPython documents; a non-type object sets a `TypeError`.
#[no_mangle]
pub extern "C" fn PyType_GetSlot(object: *mut ShimObject, slot: c_int) -> *mut c_void {
    let handle = match type_handle(object) {
        Ok(handle) => handle,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let pointer = match slot {
        PY_TP_NEW => handle.new_function,
        PY_TP_METHODS => handle.methods_pointer,
        PY_TP_DOC => handle.doc_pointer,
        _ => 0,
    };
    pointer as *mut c_void
}

/// Calls a type handle, which is what `extension.call(path, "Type", ...)`
/// does for a module attribute created by `PyType_FromSpec`.
///
/// Construction invokes the spec's `Py_tp_new` with `(type, args_tuple, NULL)`.
/// A type without `Py_tp_new` cannot be instantiated, so that is a tagged
/// `TypeError` rather than a silent `None`.
pub(crate) fn call_type_object(address: usize, args: &[ShimValue]) -> Result<ShimValue, String> {
    let object = address as *mut ShimObject;
    let handle = type_handle(object)?;
    if !handle.has_new() {
        return Err(format!(
            "TypeError: cannot create '{}' instances: the type spec has no Py_tp_new",
            handle.name
        ));
    }
    let function = handle.new_function;
    let tuple = tuple_object(args);
    clear_error();
    // SAFETY: `new_function` came from a `Py_tp_new` slot, whose CPython
    // signature is `PyObject *(*)(PyTypeObject *, PyObject *, PyObject *)`.
    let new_function: unsafe extern "C" fn(
        *mut ShimObject,
        *mut ShimObject,
        *mut ShimObject,
    ) -> *mut ShimObject = unsafe { std::mem::transmute(function as *const c_void) };
    // SAFETY: the type object and the argument tuple live in the arena for the
    // process lifetime, and NULL keyword arguments are allowed by CPython.
    let result = unsafe { new_function(object, tuple, ptr::null_mut()) };
    convert_result(result)
}

/// Short display name of a type handle, for diagnostics in the extension
/// bridge.
pub(crate) fn type_display_name(address: usize) -> String {
    match type_handle(address as *mut ShimObject) {
        Ok(handle) => handle.name.clone(),
        Err(_) => "<unknown type>".to_string(),
    }
}

/// `PyLong_FromLong`: creates an integer shim object.
#[no_mangle]
pub extern "C" fn PyLong_FromLong(value: c_long) -> *mut ShimObject {
    new_object(ShimKind::Int(value))
}

/// `PyLong_AsLong`: extracts an integer, reporting failure with `-1`.
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

/// Converts a `Py_ssize_t` size argument, rejecting negative values.
fn non_negative_size(size: isize, what: &str) -> Result<usize, String> {
    usize::try_from(size).map_err(|_| format!("ValueError: negative {what} size"))
}

/// Converts a container length to a `Py_ssize_t`.
fn py_ssize_of(len: usize) -> isize {
    isize::try_from(len).unwrap_or(isize::MAX)
}

/// `PyUnicode_FromString`: creates a string shim object.
#[no_mangle]
pub extern "C" fn PyUnicode_FromString(value: *const c_char) -> *mut ShimObject {
    if value.is_null() {
        set_error("TypeError: PyUnicode_FromString requires a non-NULL string");
        return ptr::null_mut();
    }
    value_object(ShimValue::Str(read_c_string(value)))
}

/// `PyUnicode_AsUTF8`: returns the string's NUL-terminated UTF-8 buffer.
///
/// As in CPython the pointer is valid for as long as the string object lives;
/// arena objects live for the whole process. A string with an embedded NUL is
/// rejected because the shim keeps a `CString` cache.
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

/// `PyFloat_FromDouble`: creates a float shim object.
#[no_mangle]
pub extern "C" fn PyFloat_FromDouble(value: f64) -> *mut ShimObject {
    new_object(ShimKind::Float(value))
}

/// `PyFloat_AsDouble`: extracts a double, reporting failure with `-1.0`.
///
/// `-1.0` is also a valid value; callers distinguish the cases with
/// `PyErr_Occurred`, exactly like CPython.
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

/// `PyBytes_FromStringAndSize`: copies `length` bytes into a new bytes object.
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
    let mut data = Vec::new();
    if data.try_reserve_exact(length).is_err() {
        set_error("MemoryError: cannot allocate bytes");
        return ptr::null_mut();
    }
    if length != 0 {
        // SAFETY: the caller promises `value` points at `length` readable
        // bytes; a NULL buffer was rejected above.
        let source = unsafe { std::slice::from_raw_parts(value as *const u8, length) };
        data.extend_from_slice(source);
    }
    let mut c_data = data.clone();
    c_data.push(0);
    new_object(ShimKind::Bytes { data, c_data })
}

/// `PyBytes_AsString`: returns the bytes' NUL-terminated buffer.
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

/// `PyBytes_Size`: number of content bytes, or `-1` for a non-bytes object.
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

/// Borrows a capsule's pointer, name and stored destructor.
///
/// No error is set here; each entry point maps the failure to its own message.
fn capsule_fields(
    object: *mut ShimObject,
) -> Result<(usize, Option<&'static CString>, usize), String> {
    if object.is_null() {
        return Err("TypeError: a capsule object is required, got NULL".to_string());
    }
    // SAFETY: every shim object lives in the arena for the process lifetime.
    match unsafe { &(*object).kind } {
        ShimKind::Capsule {
            ptr,
            name,
            destructor,
        } => Ok((*ptr, name.as_ref(), *destructor)),
        _ => Err("TypeError: a capsule object is required".to_string()),
    }
}

/// `PyCapsule_New`: creates a capsule around an extension-owned pointer.
///
/// The destructor is stored but never called: arena objects are not
/// finalized in this slice.
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
        // `read_c_string` stops at the first NUL, so a `CString` cannot fail
        // here.
        CString::new(read_c_string(name)).ok()
    };
    new_object(ShimKind::Capsule {
        ptr: pointer as usize,
        name,
        destructor: destructor as usize,
    })
}

/// `PyCapsule_GetPointer`: returns the stored pointer after validating the
/// capsule and its name.
///
/// Both the argument name and the stored name must be NULL, or both must be
/// present and equal; anything else reports a `ValueError`.
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

/// `PyCapsule_GetName`: borrowed capsule name, or NULL for an unnamed capsule.
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

/// `PyCapsule_IsValid`: 1 when the capsule is valid and its name matches.
///
/// A NULL `name` only matches an unnamed capsule. Unlike the getters this
/// predicate never sets a pending error.
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

/// `PyCapsule_SetPointer`: replaces the stored pointer; NULL is rejected.
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
    // SAFETY: every shim object lives in the arena for the process lifetime.
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

/// `PyObject_CheckBuffer`: 1 for a shim bytes object, 0 otherwise.
///
/// This slice implements only a read-only, contiguous bytes buffer.
#[no_mangle]
pub extern "C" fn PyObject_CheckBuffer(object: *mut ShimObject) -> c_int {
    matches!(kind_ref(object), Some(ShimKind::Bytes { .. })) as c_int
}

/// `PyObject_GetBuffer`: fills a `Py_buffer` view over a shim bytes object.
///
/// Only bytes are supported. The view is one-dimensional, contiguous and
/// read-only (`format = "B"`, `itemsize = 1`); shape/strides/suboffsets are
/// NULL because `ndim == 1` needs none of them.
#[no_mangle]
pub extern "C" fn PyObject_GetBuffer(
    object: *mut ShimObject,
    view: *mut Py_buffer,
    flags: c_int,
) -> c_int {
    if view.is_null() {
        set_error("TypeError: PyObject_GetBuffer: view is NULL");
        return -1;
    }
    if flags & PYBUF_WRITABLE != 0 {
        set_error("BufferError: bytes buffers are read-only");
        return -1;
    }
    let data = match bytes_data(object) {
        Ok(data) => data,
        Err(error) => {
            set_error(error);
            return -1;
        }
    };
    // `len` is a `Py_ssize_t`; a bytes object longer than that cannot be
    // represented, and `py_ssize_of` saturates instead of wrapping.
    let length = py_ssize_of(data.len());
    // SAFETY: `view` was checked for NULL and points at caller-owned storage
    // of at least `sizeof(Py_buffer)` bytes, as required by the C API.
    unsafe {
        view.write(Py_buffer {
            // The bytes payload lives in the arena for the process lifetime,
            // so the pointer stays valid until `PyBuffer_Release` at the
            // earliest. Casting away const is sound because `readonly` is 1
            // and this host never writes through the view.
            buf: data.as_ptr() as *mut c_void,
            obj: object,
            len: length,
            itemsize: 1,
            readonly: 1,
            ndim: 1,
            // `c"B"` is a static, NUL-terminated format string.
            format: c"B".as_ptr() as *mut c_char,
            shape: ptr::null_mut(),
            strides: ptr::null_mut(),
            suboffsets: ptr::null_mut(),
            internal: ptr::null_mut(),
        });
    }
    0
}

/// `PyBuffer_Release`: drops a view obtained from [`PyObject_GetBuffer`].
///
/// The bytes payload is arena-owned, so releasing only needs to clear the
/// caller's fields; nothing is freed and no error can be pending.
#[no_mangle]
pub extern "C" fn PyBuffer_Release(view: *mut Py_buffer) {
    if view.is_null() {
        return;
    }
    // SAFETY: `view` is non-NULL and points at the caller's live `Py_buffer`.
    unsafe {
        view.write(Py_buffer {
            buf: ptr::null_mut(),
            obj: ptr::null_mut(),
            len: 0,
            itemsize: 0,
            readonly: 0,
            ndim: 0,
            format: ptr::null_mut(),
            shape: ptr::null_mut(),
            strides: ptr::null_mut(),
            suboffsets: ptr::null_mut(),
            internal: ptr::null_mut(),
        });
    }
}

/// `PyList_New`: creates a list whose slots start out uninitialized.
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

/// `PyList_Size`: number of items, or `-1` for a non-list object.
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

/// `PyList_GetItem`: borrowed item reference, or NULL when out of range.
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

/// `PyList_SetItem`: stores `item` at `index`, stealing its reference.
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
            *slot = item as usize;
            0
        }
        None => {
            set_error("IndexError: list assignment index out of range");
            -1
        }
    }
}

/// `PyList_Append`: appends one item, growing the list.
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
    items.push(item as usize);
    0
}

/// `PyTuple_New`: creates a tuple whose slots start out uninitialized.
#[no_mangle]
pub extern "C" fn PyTuple_New(size: isize) -> *mut ShimObject {
    let count = match non_negative_size(size, "tuple") {
        Ok(count) => count,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let mut items = Vec::new();
    if items.try_reserve_exact(count).is_err() {
        set_error("MemoryError: cannot allocate tuple");
        return ptr::null_mut();
    }
    items.resize(count, 0);
    new_object(ShimKind::Tuple(items))
}

/// `PyTuple_Size`: number of items, or `-1` for a non-tuple object.
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

/// `PyTuple_GetItem`: borrowed item reference, or NULL when out of range.
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

/// `PyTuple_SetItem`: stores `item` at `index`, stealing its reference.
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
            *slot = item as usize;
            0
        }
        None => {
            set_error("IndexError: tuple assignment index out of range");
            -1
        }
    }
}

/// `PyDict_New`: creates an empty dict.
#[no_mangle]
pub extern "C" fn PyDict_New() -> *mut ShimObject {
    new_object(ShimKind::Dict(Vec::new()))
}

/// `PyDict_SetItem`: inserts or overwrites `key` -> `value`.
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

/// `PyDict_SetItemString`: inserts or overwrites a string key.
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
    let key = value_object(ShimValue::Str(read_c_string(key)));
    PyDict_SetItem(object, key, value)
}

/// `PyDict_GetItem`: borrowed value for `key`, or NULL when absent.
///
/// A missing key returns NULL *without* setting an error, matching CPython;
/// a NULL/wrong-typed dict sets a `TypeError` and returns NULL.
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

/// `PyDict_GetItemString`: borrowed value for a string key, or NULL.
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

/// `Py_IncRef`: increments a shim object's reference count.
#[no_mangle]
pub extern "C" fn Py_IncRef(object: *mut ShimObject) {
    if object.is_null() {
        return;
    }
    // SAFETY: shim objects live in the arena for the process lifetime; the refcount is accessible.
    unsafe { (*object).refcnt.fetch_add(1, Ordering::Relaxed) };
}

/// `Py_DecRef`: decrements a shim object's reference count. The object is
/// never freed (see "No finalization" above), so stale pointers are harmless.
#[no_mangle]
pub extern "C" fn Py_DecRef(object: *mut ShimObject) {
    if object.is_null() {
        return;
    }
    // SAFETY: shim objects live in the arena for the process lifetime; the refcount is accessible.
    let _ = unsafe { (*object).refcnt.fetch_sub(1, Ordering::Relaxed) };
}

/// `_Py_IncRef`: the non-NULL form used by `Py_INCREF` in limited-API
/// extensions built against CPython 3.12 and newer.
///
/// Those headers implement the `Py_INCREF` macro as a call to this symbol (or
/// to `Py_IncRef` for older limited-API versions), so it must be exported just
/// like the public name.
#[no_mangle]
pub extern "C" fn _Py_IncRef(object: *mut ShimObject) {
    if object.is_null() {
        return;
    }
    // SAFETY: shim objects live in the arena for the process lifetime; the refcount is accessible.
    unsafe { (*object).refcnt.fetch_add(1, Ordering::Relaxed) };
}

/// `_Py_DecRef`: the non-NULL form used by `Py_DECREF` in limited-API
/// extensions built against CPython 3.12 and newer.
///
/// Same slice limitation as [`Py_DecRef`]: the count drops, memory stays.
#[no_mangle]
pub extern "C" fn _Py_DecRef(object: *mut ShimObject) {
    if object.is_null() {
        return;
    }
    // SAFETY: shim objects live in the arena for the process lifetime; the refcount is accessible.
    let _ = unsafe { (*object).refcnt.fetch_sub(1, Ordering::Relaxed) };
}

/// `PyErr_Occurred`: returns a non-NULL sentinel while an error is pending.
#[no_mangle]
pub extern "C" fn PyErr_Occurred() -> *mut ShimObject {
    if !error_pending() {
        return ptr::null_mut();
    }
    static SENTINEL: LazyLock<usize> = LazyLock::new(|| new_object(ShimKind::Error) as usize);
    *SENTINEL as *mut ShimObject
}

/// `PyErr_SetString`: records the message of a pending error, tagged with the
/// exception category taken from the `PyExc_*` singleton.
///
/// The loader classifier (`modules::extension::raised_error`) turns the
/// `"<Type>: "` prefix into the matching Hydro exception tag. A NULL or
/// unknown exception object falls back to `RuntimeError`.
#[no_mangle]
pub extern "C" fn PyErr_SetString(exception: *mut ShimObject, message: *const c_char) {
    let text = if message.is_null() {
        "extension error".to_string()
    } else {
        read_c_string(message)
    };
    set_error(format!("{}: {text}", exception_prefix(exception)));
}

/// `PyErr_SetNone`: records a pending error of `exception` without a message.
#[no_mangle]
pub extern "C" fn PyErr_SetNone(exception: *mut ShimObject) {
    set_error(format!("{}: ", exception_prefix(exception)));
}

/// `PyErr_SetObject`: records `value` as the description of the pending error.
///
/// A string value is used verbatim (Python's `str(exc)` for a single string
/// argument); every other shim value is rendered with [`shim_repr`].
#[no_mangle]
pub extern "C" fn PyErr_SetObject(exception: *mut ShimObject, value: *mut ShimObject) {
    let text = match kind_ref(value) {
        Some(ShimKind::Str { text, .. }) => text.clone(),
        _ => shim_repr(value),
    };
    set_error(format!("{}: {text}", exception_prefix(exception)));
}

/// `PyErr_NoMemory`: records a `MemoryError` and always returns NULL.
#[no_mangle]
pub extern "C" fn PyErr_NoMemory() -> *mut ShimObject {
    set_error("MemoryError: out of memory");
    ptr::null_mut()
}

/// `PyErr_BadArgument`: records a `TypeError` and always returns 0.
#[no_mangle]
pub extern "C" fn PyErr_BadArgument() -> c_int {
    set_error("TypeError: bad argument to internal function");
    0
}

/// `PyErr_BadInternalCall`: records a `SystemError`.
#[no_mangle]
pub extern "C" fn PyErr_BadInternalCall() {
    set_error("SystemError: bad argument to internal function");
}

/// `PyErr_SetFromErrno`: records the current `errno` description.
///
/// A NULL exception means `OSError`, as CPython documents for this entry
/// point.
#[no_mangle]
pub extern "C" fn PyErr_SetFromErrno(exception: *mut ShimObject) -> *mut ShimObject {
    let prefix = if exception.is_null() {
        "OSError"
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

/// `PyErr_Clear`: drops the pending error.
#[no_mangle]
pub extern "C" fn PyErr_Clear() {
    clear_error();
}

// Accessors used by `parse.c`

/// Number of positional arguments in a tuple object (0 for non-tuples).
#[no_mangle]
pub extern "C" fn hydro_capi_arg_count(args: *mut ShimObject) -> usize {
    if args.is_null() {
        return 0;
    }
    // SAFETY: every shim object lives in the arena for the process lifetime.
    match unsafe { &(*args).kind } {
        ShimKind::Tuple(items) => items.len(),
        _ => 0,
    }
}

/// Positional argument at `index`, or NULL when out of range.
#[no_mangle]
pub extern "C" fn hydro_capi_arg(args: *mut ShimObject, index: usize) -> *mut ShimObject {
    if args.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: every shim object lives in the arena for the process lifetime.
    match unsafe { &(*args).kind } {
        ShimKind::Tuple(items) => items
            .get(index)
            .map(|item| *item as *mut ShimObject)
            .unwrap_or(ptr::null_mut()),
        _ => ptr::null_mut(),
    }
}

/// `1` when `keywords` is NULL or a shim dict, `0` for any other object.
///
/// `parse.c` rejects a non-dict keyword object, the way CPython asserts it,
/// instead of silently treating it as "no keywords".
#[no_mangle]
pub extern "C" fn hydro_capi_kw_is_dict(keywords: *mut ShimObject) -> c_int {
    if keywords.is_null() {
        return 1;
    }
    if matches!(kind_ref(keywords), Some(ShimKind::Dict(_))) {
        1
    } else {
        0
    }
}

/// Number of entries in a keyword dict (0 for NULL or non-dicts).
#[no_mangle]
pub extern "C" fn hydro_capi_kw_count(keywords: *mut ShimObject) -> usize {
    dict_slots(keywords).map(|pairs| pairs.len()).unwrap_or(0)
}

/// Key of keyword entry `index`, or NULL when it is not a string.
///
/// `parse.c` treats a NULL key as "keywords must be strings", matching
/// CPython's `PyArg_ParseTupleAndKeywords` contract.
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

/// `"n"` conversion: `Py_ssize_t` (repr-compatible with Rust `isize`).
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

/// `"s"` conversion: NUL-terminated `const char *`.
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

/// `"O"` conversion: the shim object pointer itself.
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

// Link anchor

extern "C" {
    /// Defined in `parse.c`; declared here so [`anchor`] can reference it and
    /// force the C object into the final link. The arguments are declared as
    /// `c_void` because the Rust-side types are deliberately not FFI-safe.
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
}

/// References every exported entry point so the linker keeps the definitions.
///
/// `hydro_runtime_init` calls this once. Without the reference the staticlib
/// linker would drop the otherwise unreachable object files, leaving
/// `dlopen`ed extensions with unresolved `Py*` symbols.
pub(crate) fn anchor() {
    // Data symbols must already exist when an extension initializer evaluates
    // `PyExc_*` or `Py_None`, and their addresses must be referenced so the
    // staticlib linker does not drop them.
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
        PyArg_ParseTuple as *const (),
        PyArg_ParseTupleAndKeywords as *const (),
        PyErr_Format as *const (),
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

    #[test]
    fn list_roundtrip_through_the_c_api() {
        let list = PyList_New(0);
        assert!(!list.is_null());
        assert_eq!(PyList_Append(list, PyLong_FromLong(7)), 0);
        assert_eq!(PyList_Size(list), 1);

        match object_to_value(list) {
            Ok(ShimValue::List(items)) => assert!(matches!(items.as_slice(), [ShimValue::Int(7)])),
            other => panic!("unexpected conversion result: {other:?}"),
        }
    }

    #[test]
    fn uninitialized_container_items_are_rejected() {
        let message = object_to_value(PyList_New(2)).expect_err("unfilled list");
        assert!(message.contains("uninitialized"), "{message}");

        let message = object_to_value(PyTuple_New(1)).expect_err("unfilled tuple");
        assert!(message.contains("uninitialized"), "{message}");
    }

    #[test]
    fn cyclic_container_results_are_rejected() {
        let list = PyList_New(1);
        assert_eq!(PyList_SetItem(list, 0, list), 0);
        let message = object_to_value(list).expect_err("cyclic list");
        assert!(message.contains("cyclic"), "{message}");
    }

    #[test]
    fn dict_set_and_get_use_structural_keys() {
        clear_error();
        let dict = PyDict_New();
        let first = PyUnicode_FromString(c"key".as_ptr());
        let second = PyUnicode_FromString(c"key".as_ptr());

        assert_eq!(PyDict_SetItem(dict, first, PyLong_FromLong(4)), 0);
        assert_eq!(
            PyDict_SetItem(dict, second, PyLong_FromLong(5)),
            0,
            "replace"
        );
        assert_eq!(dict_len_for_test(dict), 1);

        let value = PyDict_GetItem(dict, first);
        assert!(!value.is_null());
        assert_eq!(PyLong_AsLong(value), 5);

        let value = PyDict_GetItemString(dict, c"key".as_ptr());
        assert!(!value.is_null());
        assert_eq!(PyLong_AsLong(value), 5);

        assert!(PyDict_GetItemString(dict, c"missing".as_ptr()).is_null());
        assert!(!error_pending(), "a missing key must not set an error");
    }

    #[test]
    fn bytes_c_api_keeps_a_trailing_nul() {
        let bytes = PyBytes_FromStringAndSize(b"hi\0x".as_ptr() as *const c_char, 2);
        assert!(!bytes.is_null());
        assert_eq!(PyBytes_Size(bytes), 2);
        let pointer = PyBytes_AsString(bytes);
        assert!(!pointer.is_null());
        // SAFETY: `pointer` addresses the shim's `c_data`, which is
        // `data.len() + 1` bytes long.
        unsafe {
            assert_eq!(*pointer.add(0), b'h' as c_char);
            assert_eq!(*pointer.add(1), b'i' as c_char);
            assert_eq!(*pointer.add(2), 0);
        }
    }

    #[test]
    fn capsule_roundtrip_preserves_pointer_and_name() {
        clear_error();
        static VALUE: i64 = 1234;
        static OTHER: i64 = 99;
        let pointer = &VALUE as *const i64 as *mut c_void;
        let capsule = PyCapsule_New(pointer, c"tea.demo".as_ptr(), ptr::null_mut());
        assert!(!capsule.is_null());
        assert!(!error_pending());

        // The value round trip carries the same pointer and name.
        match object_to_value(capsule) {
            Ok(ShimValue::Capsule {
                ptr: value,
                name: Some(name),
            }) => {
                assert_eq!(value, pointer as usize);
                assert_eq!(name, "tea.demo");
            }
            other => panic!("unexpected capsule conversion: {other:?}"),
        }

        // The pointer and name are retrievable through the C API.
        assert_eq!(PyCapsule_GetPointer(capsule, c"tea.demo".as_ptr()), pointer);
        assert!(!error_pending());
        // SAFETY: the capsule owns a `CString` that lives in the arena.
        let name = PyCapsule_GetName(capsule);
        assert_eq!(unsafe { CStr::from_ptr(name) }, c"tea.demo");
        assert_eq!(PyCapsule_IsValid(capsule, c"tea.demo".as_ptr()), 1);
        assert_eq!(PyCapsule_IsValid(capsule, c"other".as_ptr()), 0);
        assert_eq!(PyCapsule_IsValid(capsule, ptr::null()), 0);
        assert!(!error_pending(), "IsValid must not leave an error behind");

        // Name mismatches, unnamed lookups and NULL capsules are errors.
        assert!(PyCapsule_GetPointer(capsule, c"other".as_ptr()).is_null());
        assert!(error_pending());
        clear_error();
        assert!(PyCapsule_GetPointer(capsule, ptr::null()).is_null());
        assert!(error_pending());
        clear_error();
        assert!(PyCapsule_GetPointer(ptr::null_mut(), c"tea.demo".as_ptr()).is_null());
        assert!(error_pending());
        clear_error();
        assert!(PyCapsule_New(ptr::null_mut(), c"tea.demo".as_ptr(), ptr::null_mut()).is_null());
        assert!(error_pending());
        clear_error();

        // SetPointer replaces the payload but keeps the name.
        let replacement = &OTHER as *const i64 as *mut c_void;
        assert_eq!(PyCapsule_SetPointer(capsule, replacement), 0);
        assert_eq!(
            PyCapsule_GetPointer(capsule, c"tea.demo".as_ptr()),
            replacement
        );
        assert_eq!(PyCapsule_SetPointer(capsule, ptr::null_mut()), -1);
        assert!(error_pending());
        clear_error();
    }

    #[test]
    fn unnamed_capsules_report_a_null_name() {
        clear_error();
        static VALUE: i64 = 7;
        let pointer = &VALUE as *const i64 as *mut c_void;
        let capsule = PyCapsule_New(pointer, ptr::null(), ptr::null_mut());
        assert!(!capsule.is_null());
        assert!(PyCapsule_GetName(capsule).is_null());
        assert_eq!(PyCapsule_GetPointer(capsule, ptr::null()), pointer);
        assert_eq!(PyCapsule_IsValid(capsule, ptr::null()), 1);
        assert_eq!(PyCapsule_IsValid(capsule, c"named".as_ptr()), 0);
        match object_to_value(capsule) {
            Ok(ShimValue::Capsule {
                ptr: value,
                name: None,
            }) => {
                assert_eq!(value, pointer as usize);
            }
            other => panic!("unexpected capsule conversion: {other:?}"),
        }
    }

    #[test]
    fn bytes_buffer_fields_match_cpython() {
        clear_error();
        let bytes = PyBytes_FromStringAndSize(b"hello".as_ptr() as *const c_char, 5);
        assert!(!bytes.is_null());
        assert_eq!(PyObject_CheckBuffer(bytes), 1);
        assert_eq!(PyObject_CheckBuffer(PyLong_FromLong(1)), 0);

        // SAFETY: a zeroed `Py_buffer` is a valid bit pattern for every field
        // (raw pointers and integers); `GetBuffer` overwrites it completely.
        let mut view: Py_buffer = unsafe { std::mem::zeroed() };
        assert_eq!(PyObject_GetBuffer(bytes, &mut view, 0), 0);
        assert!(!error_pending());
        assert_eq!(view.obj, bytes);
        assert_eq!(view.len, 5);
        assert_eq!(view.itemsize, 1);
        assert_eq!(view.readonly, 1);
        assert_eq!(view.ndim, 1);
        assert!(view.shape.is_null());
        assert!(view.strides.is_null());
        assert!(view.suboffsets.is_null());
        assert!(view.internal.is_null());
        assert!(!view.format.is_null());
        // SAFETY: `format` points at the static `c"B"` literal.
        assert_eq!(unsafe { CStr::from_ptr(view.format) }, c"B");
        // SAFETY: `buf` addresses the bytes payload (5 readable bytes).
        let copied = unsafe { std::slice::from_raw_parts(view.buf as *const u8, 5) };
        assert_eq!(copied, b"hello");

        PyBuffer_Release(&mut view);
        assert!(view.buf.is_null());
        assert!(view.obj.is_null());
        assert_eq!(view.len, 0);
        assert_eq!(view.itemsize, 0);
        assert_eq!(view.readonly, 0);
        assert_eq!(view.ndim, 0);
        assert!(view.format.is_null());
    }

    #[test]
    fn buffer_requests_reject_unsupported_objects() {
        clear_error();
        let mut view: Py_buffer = unsafe { std::mem::zeroed() };
        assert_eq!(PyObject_GetBuffer(PyLong_FromLong(3), &mut view, 0), -1);
        assert!(error_pending());
        clear_error();
        assert_eq!(PyObject_GetBuffer(ptr::null_mut(), &mut view, 0), -1);
        assert!(error_pending());
        clear_error();
        let bytes = PyBytes_FromStringAndSize(b"x".as_ptr() as *const c_char, 1);
        assert_eq!(PyObject_GetBuffer(bytes, ptr::null_mut(), 0), -1);
        assert!(error_pending());
        clear_error();
        assert_eq!(PyObject_GetBuffer(bytes, &mut view, PYBUF_WRITABLE), -1);
        assert!(error_pending());
        clear_error();
        // Releasing a NULL view is a no-op.
        PyBuffer_Release(ptr::null_mut());
    }

    #[test]
    fn exported_exception_singletons_are_initialized_once() {
        anchor();
        let symbols = unsafe {
            [
                PyExc_BaseException,
                PyExc_Exception,
                PyExc_TypeError,
                PyExc_ValueError,
                PyExc_RuntimeError,
                PyExc_AttributeError,
                PyExc_IndexError,
                PyExc_KeyError,
                PyExc_OverflowError,
                PyExc_ZeroDivisionError,
                PyExc_StopIteration,
                PyExc_OSError,
                PyExc_AssertionError,
                PyExc_BaseExceptionGroup,
                PyExc_FileNotFoundError,
                PyExc_GeneratorExit,
                PyExc_ImportError,
                PyExc_KeyboardInterrupt,
                PyExc_LookupError,
                PyExc_MemoryError,
                PyExc_ModuleNotFoundError,
                PyExc_NameError,
                PyExc_NotImplementedError,
                PyExc_StopAsyncIteration,
                PyExc_SyntaxError,
                PyExc_SystemError,
                PyExc_SystemExit,
                PyExc_UnicodeDecodeError,
                PyExc_UserWarning,
            ]
        };
        for symbol in symbols {
            assert!(!symbol.is_null());
        }
        // A second anchor pass must not recreate the singletons.
        let first = unsafe { PyExc_TypeError };
        anchor();
        assert_eq!(unsafe { PyExc_TypeError }, first);
    }

    #[test]
    fn pyerr_setstring_prefixes_the_exception_name() {
        anchor();
        clear_error();
        PyErr_SetString(unsafe { PyExc_ValueError }, c"bad value".as_ptr());
        assert_eq!(take_error().as_deref(), Some("ValueError: bad value"));
        PyErr_SetString(unsafe { PyExc_KeyError }, c"missing".as_ptr());
        assert_eq!(take_error().as_deref(), Some("KeyError: missing"));
        PyErr_SetString(unsafe { PyExc_OSError }, c"io".as_ptr());
        assert_eq!(take_error().as_deref(), Some("OSError: io"));
        // The base classes keep their own names; the loader maps them to the
        // matching Hydro tags.
        PyErr_SetString(unsafe { PyExc_BaseException }, c"base".as_ptr());
        assert_eq!(take_error().as_deref(), Some("BaseException: base"));
        PyErr_SetString(unsafe { PyExc_Exception }, c"generic".as_ptr());
        assert_eq!(take_error().as_deref(), Some("Exception: generic"));
        PyErr_SetString(unsafe { PyExc_FileNotFoundError }, c"gone".as_ptr());
        assert_eq!(take_error().as_deref(), Some("FileNotFoundError: gone"));
        PyErr_SetString(unsafe { PyExc_UnicodeDecodeError }, c"codec".as_ptr());
        assert_eq!(take_error().as_deref(), Some("UnicodeDecodeError: codec"));
        // NULL, non-exception objects fall back to the base runtime error.
        PyErr_SetString(ptr::null_mut(), c"boom".as_ptr());
        assert_eq!(take_error().as_deref(), Some("RuntimeError: boom"));
        PyErr_SetString(PyLong_FromLong(1), c"odd".as_ptr());
        assert_eq!(take_error().as_deref(), Some("RuntimeError: odd"));
        assert!(!error_pending());
    }

    #[test]
    fn error_setters_record_their_messages() {
        anchor();
        clear_error();
        PyErr_SetNone(unsafe { PyExc_StopIteration });
        assert_eq!(take_error().as_deref(), Some("StopIteration: "));

        PyErr_SetObject(
            unsafe { PyExc_ValueError },
            value_object(ShimValue::Str("bad".to_string())),
        );
        assert_eq!(take_error().as_deref(), Some("ValueError: bad"));

        PyErr_SetObject(unsafe { PyExc_TypeError }, PyLong_FromLong(3));
        assert_eq!(take_error().as_deref(), Some("TypeError: 3"));

        PyErr_SetObject(
            unsafe { PyExc_KeyError },
            value_object(ShimValue::List(vec![ShimValue::Int(1)])),
        );
        assert_eq!(take_error().as_deref(), Some("KeyError: [1]"));

        assert!(PyErr_NoMemory().is_null());
        assert_eq!(take_error().as_deref(), Some("MemoryError: out of memory"));

        assert_eq!(PyErr_BadArgument(), 0);
        assert_eq!(
            take_error().as_deref(),
            Some("TypeError: bad argument to internal function")
        );

        PyErr_BadInternalCall();
        assert_eq!(
            take_error().as_deref(),
            Some("SystemError: bad argument to internal function")
        );

        // `PyErr_SetFromErrno` reports the current errno description under
        // OSError when no exception object is given.
        assert!(PyErr_SetFromErrno(ptr::null_mut()).is_null());
        let message = take_error().expect("errno error");
        assert!(message.starts_with("OSError: "), "{message}");
        assert!(!error_pending());
    }

    #[test]
    fn pyerr_format_renders_c99_conversions() {
        anchor();
        clear_error();
        // SAFETY: the format string and its arguments match the C99 contract.
        let result = unsafe {
            PyErr_Format(
                PyExc_ValueError as *mut c_void,
                c"%s has %d items".as_ptr(),
                c"probe".as_ptr(),
                3 as c_int,
            )
        };
        assert!(result.is_null());
        assert_eq!(
            take_error().as_deref(),
            Some("ValueError: probe has 3 items")
        );

        // CPython's object conversions are rejected instead of being handed to
        // `vsnprintf` (which has no meaning for them).
        let result = unsafe { PyErr_Format(PyExc_TypeError as *mut c_void, c"%R".as_ptr()) };
        assert!(result.is_null());
        let message = take_error().expect("unsupported conversion");
        assert!(message.contains("not supported"), "{message}");
        assert!(!error_pending());
    }

    #[test]
    fn constant_singletons_are_shared_with_the_data_symbols() {
        anchor();
        clear_error();
        let none = Py_GetConstantBorrowed(PY_CONSTANT_NONE);
        let true_ = Py_GetConstantBorrowed(PY_CONSTANT_TRUE);
        let false_ = Py_GetConstantBorrowed(PY_CONSTANT_FALSE);
        let ellipsis = Py_GetConstantBorrowed(PY_CONSTANT_ELLIPSIS);
        let not_implemented = Py_GetConstantBorrowed(PY_CONSTANT_NOT_IMPLEMENTED);
        assert_eq!(none, ptr::addr_of_mut!(_Py_NoneStruct));
        assert_eq!(true_, ptr::addr_of_mut!(_Py_TrueStruct));
        assert_eq!(false_, ptr::addr_of_mut!(_Py_FalseStruct));
        assert_eq!(not_implemented, ptr::addr_of_mut!(_Py_NotImplementedStruct));
        assert_eq!(ellipsis, ptr::addr_of_mut!(_Py_EllipsisObject));
        assert!(!error_pending());

        // The borrowed objects convert like the values they represent.
        assert!(matches!(object_to_value(none), Ok(ShimValue::None)));
        assert!(matches!(object_to_value(true_), Ok(ShimValue::Bool(true))));
        assert!(matches!(
            object_to_value(false_),
            Ok(ShimValue::Bool(false))
        ));
        assert!(matches!(
            object_to_value(not_implemented),
            Ok(ShimValue::NotImplemented)
        ));
        assert!(matches!(object_to_value(ellipsis), Ok(ShimValue::Ellipsis)));

        // Identity helpers match CPython's spelling.
        assert_eq!(Py_IsNone(none), 1);
        assert_eq!(Py_IsNone(true_), 0);
        assert_eq!(Py_IsTrue(true_), 1);
        assert_eq!(Py_IsFalse(false_), 1);
        assert_eq!(Py_Is(none, none), 1);
        assert_eq!(Py_Is(none, true_), 0);

        // Arguments converted for an extension are the same singletons, so
        // `arg == Py_None` inside the extension works.
        assert_eq!(value_object(ShimValue::None), none);
        assert_eq!(value_object(ShimValue::Bool(true)), true_);
        assert_eq!(value_object(ShimValue::Bool(false)), false_);
        assert_eq!(value_object(ShimValue::NotImplemented), not_implemented);
        assert_eq!(Py_IsNone(value_object(ShimValue::None)), 1);
        assert_eq!(Py_IsTrue(value_object(ShimValue::Bool(true))), 1);
    }

    #[test]
    fn py_get_constant_returns_new_and_borrowed_references() {
        anchor();
        // `NotImplemented` is only fetched by this test, so the reference
        // count assertions cannot race with a sibling test.
        let singleton = Py_GetConstantBorrowed(PY_CONSTANT_NOT_IMPLEMENTED);
        // SAFETY: the constant lives in a static for the whole process.
        let before = unsafe { (*singleton).refcnt.load(Ordering::Relaxed) };
        let borrowed = Py_GetConstantBorrowed(PY_CONSTANT_NOT_IMPLEMENTED);
        assert_eq!(borrowed, singleton);
        assert_eq!(
            unsafe { (*singleton).refcnt.load(Ordering::Relaxed) },
            before,
            "a borrowed reference must not change the count"
        );

        let fresh = Py_GetConstant(PY_CONSTANT_NOT_IMPLEMENTED);
        assert_eq!(fresh, singleton, "both spellings are the same singleton");
        assert_eq!(
            unsafe { (*singleton).refcnt.load(Ordering::Relaxed) },
            before + 1,
            "a new reference increments the count"
        );

        // The interned constants are identity-stable too.
        let zero = Py_GetConstantBorrowed(PY_CONSTANT_ZERO);
        assert_eq!(Py_GetConstantBorrowed(PY_CONSTANT_ZERO), zero);
        assert_eq!(PyLong_AsLong(zero), 0);
        assert_eq!(PyLong_AsLong(Py_GetConstantBorrowed(PY_CONSTANT_ONE)), 1);
        assert!(matches!(
            object_to_value(Py_GetConstantBorrowed(PY_CONSTANT_EMPTY_STR)),
            Ok(ShimValue::Str(text)) if text.is_empty()
        ));
        assert!(matches!(
            object_to_value(Py_GetConstantBorrowed(PY_CONSTANT_EMPTY_BYTES)),
            Ok(ShimValue::Bytes(bytes)) if bytes.is_empty()
        ));
        assert!(matches!(
            object_to_value(Py_GetConstantBorrowed(PY_CONSTANT_EMPTY_TUPLE)),
            Ok(ShimValue::Tuple(items)) if items.is_empty()
        ));

        // Unknown identifiers are tagged errors, not silent NULLs.
        clear_error();
        assert!(Py_GetConstant(PY_CONSTANT_EMPTY_TUPLE + 1).is_null());
        let message = take_error().expect("unknown constant");
        assert!(message.starts_with("TypeError: "), "{message}");
        assert!(message.contains("unknown constant id"), "{message}");
        assert!(Py_GetConstantBorrowed(PY_CONSTANT_EMPTY_TUPLE + 1).is_null());
        assert!(take_error().is_some());
    }

    #[test]
    fn c_entry_points_and_constant_symbols_are_linked() {
        // Nothing here links libpython: `parse.c`'s entry points and the
        // exported data symbols resolve inside the runtime. A missing
        // definition would only surface later as an extension `dlopen`
        // failure, so the addresses are asserted directly.
        let c_entry_points: [*const (); 3] = [
            PyArg_ParseTuple as *const (),
            PyArg_ParseTupleAndKeywords as *const (),
            PyErr_Format as *const (),
        ];
        assert!(c_entry_points.iter().all(|entry| !entry.is_null()));
        let data_symbols = [
            ptr::addr_of_mut!(_Py_NoneStruct) as *const (),
            ptr::addr_of_mut!(_Py_TrueStruct) as *const (),
            ptr::addr_of_mut!(_Py_FalseStruct) as *const (),
            ptr::addr_of_mut!(_Py_NotImplementedStruct) as *const (),
            ptr::addr_of_mut!(_Py_EllipsisObject) as *const (),
        ];
        assert!(data_symbols.iter().all(|symbol| !symbol.is_null()));

        anchor();
        assert!(!unsafe { PyExc_ValueError }.is_null());
        assert!(!unsafe { PyExc_FileNotFoundError }.is_null());
        assert!(!unsafe { PyExc_KeyboardInterrupt }.is_null());
    }

    /// Builds a shim keyword dict from name/value pairs.
    fn keyword_dict(pairs: &[(&str, ShimValue)]) -> *mut ShimObject {
        let dict = PyDict_New();
        for (name, value) in pairs {
            let key = value_object(ShimValue::Str((*name).to_string()));
            let item = value_object(value.clone());
            assert_eq!(PyDict_SetItem(dict, key, item), 0);
        }
        dict
    }

    #[test]
    fn keyword_parser_matches_positional_and_named_arguments() {
        anchor();
        clear_error();
        let names = [c"a".as_ptr(), c"b".as_ptr(), ptr::null()];
        let format = c"ii:add";

        // Both arguments positional.
        let args = tuple_object(&[ShimValue::Int(1), ShimValue::Int(2)]);
        let mut first: c_int = 0;
        let mut second: c_int = 0;
        // SAFETY: `names` is NULL-terminated and one entry per format unit; the
        // two output pointers match the "ii" conversions.
        let status = unsafe {
            PyArg_ParseTupleAndKeywords(
                args as *mut c_void,
                ptr::null_mut(),
                format.as_ptr(),
                names.as_ptr(),
                &mut first as *mut c_int,
                &mut second as *mut c_int,
            )
        };
        assert_eq!(status, 1, "{:?}", take_error());
        assert_eq!((first, second), (1, 2));

        // Second argument by name, first positionally.
        let kwargs = keyword_dict(&[("b", ShimValue::Int(5))]);
        let args = tuple_object(&[ShimValue::Int(3)]);
        first = 0;
        second = 0;
        // SAFETY: as above; `kwargs` is a shim dict.
        let status = unsafe {
            PyArg_ParseTupleAndKeywords(
                args as *mut c_void,
                kwargs as *mut c_void,
                format.as_ptr(),
                names.as_ptr(),
                &mut first as *mut c_int,
                &mut second as *mut c_int,
            )
        };
        assert_eq!(status, 1, "{:?}", take_error());
        assert_eq!((first, second), (3, 5));

        // The same parameter by position and by name: rejected as a duplicate
        // (the total still fits, so this is not an arity error).
        let kwargs = keyword_dict(&[("a", ShimValue::Int(7))]);
        first = 0;
        second = 0;
        // SAFETY: as above.
        let status = unsafe {
            PyArg_ParseTupleAndKeywords(
                args as *mut c_void,
                kwargs as *mut c_void,
                format.as_ptr(),
                names.as_ptr(),
                &mut first as *mut c_int,
                &mut second as *mut c_int,
            )
        };
        assert_eq!(status, 0);
        let message = take_error().expect("duplicate argument");
        assert!(message.contains("given by name and position"), "{message}");
        assert!(!error_pending());
    }

    #[test]
    fn keyword_parser_supports_optional_and_keyword_only_units() {
        anchor();
        clear_error();
        let names = [c"a".as_ptr(), c"b".as_ptr(), c"c".as_ptr(), ptr::null()];

        // "ii|i": the third argument is optional.
        let args = tuple_object(&[ShimValue::Int(1), ShimValue::Int(2)]);
        let mut first: c_int = 0;
        let mut second: c_int = 0;
        let mut third: c_int = -1;
        // SAFETY: the format "ii|i" needs three int pointers.
        let status = unsafe {
            PyArg_ParseTupleAndKeywords(
                args as *mut c_void,
                ptr::null_mut(),
                c"ii|i:add".as_ptr(),
                names.as_ptr(),
                &mut first as *mut c_int,
                &mut second as *mut c_int,
                &mut third as *mut c_int,
            )
        };
        assert_eq!(status, 1, "{:?}", take_error());
        assert_eq!(
            (first, second, third),
            (1, 2, -1),
            "missing optional untouched"
        );

        // The optional unit can also arrive by name.
        let kwargs = keyword_dict(&[("c", ShimValue::Int(9))]);
        third = -1;
        // SAFETY: as above.
        let status = unsafe {
            PyArg_ParseTupleAndKeywords(
                args as *mut c_void,
                kwargs as *mut c_void,
                c"ii|i:add".as_ptr(),
                names.as_ptr(),
                &mut first as *mut c_int,
                &mut second as *mut c_int,
                &mut third as *mut c_int,
            )
        };
        assert_eq!(status, 1, "{:?}", take_error());
        assert_eq!(third, 9);

        // "i$i": the second unit is keyword-only, so two positionals fail.
        let short_names = [c"a".as_ptr(), c"b".as_ptr(), ptr::null()];
        let args = tuple_object(&[ShimValue::Int(1), ShimValue::Int(2)]);
        let status = unsafe {
            PyArg_ParseTupleAndKeywords(
                args as *mut c_void,
                ptr::null_mut(),
                c"i$i:add".as_ptr(),
                short_names.as_ptr(),
                &mut first as *mut c_int,
                &mut second as *mut c_int,
            )
        };
        assert_eq!(status, 0);
        let message = take_error().expect("keyword-only violation");
        assert!(message.contains("takes at most 1 positional"), "{message}");
        assert!(!error_pending());
    }

    #[test]
    fn keyword_parser_rejects_bad_arity_and_names() {
        anchor();
        let names = [c"a".as_ptr(), c"b".as_ptr(), ptr::null()];
        let format = c"ii:add";
        let mut first: c_int = 0;
        let mut second: c_int = 0;

        // Missing required argument.
        clear_error();
        let args = tuple_object(&[]);
        // SAFETY: the format "ii" needs two int pointers.
        let status = unsafe {
            PyArg_ParseTupleAndKeywords(
                args as *mut c_void,
                ptr::null_mut(),
                format.as_ptr(),
                names.as_ptr(),
                &mut first as *mut c_int,
                &mut second as *mut c_int,
            )
        };
        assert_eq!(status, 0);
        let message = take_error().expect("missing argument");
        assert!(
            message.contains("missing required argument 'a'"),
            "{message}"
        );

        // CPython reports a missing required argument from the conversion
        // loop before it scans for unknown keywords, so a stray keyword does
        // not hide the missing '@a'.
        let kwargs = keyword_dict(&[("z", ShimValue::Int(1))]);
        let args = tuple_object(&[]);
        let status = unsafe {
            PyArg_ParseTupleAndKeywords(
                args as *mut c_void,
                kwargs as *mut c_void,
                format.as_ptr(),
                names.as_ptr(),
                &mut first as *mut c_int,
                &mut second as *mut c_int,
            )
        };
        assert_eq!(status, 0);
        let message = take_error().expect("missing argument wins");
        assert!(
            message.contains("missing required argument 'a'"),
            "{message}"
        );

        // Unknown keyword: the required arguments must be satisfied first, and
        // the optional third unit leaves room for the stray keyword without
        // tripping the arity check.
        let optional_names = [c"a".as_ptr(), c"b".as_ptr(), c"c".as_ptr(), ptr::null()];
        let kwargs = keyword_dict(&[("z", ShimValue::Int(1))]);
        let args = tuple_object(&[ShimValue::Int(1), ShimValue::Int(2)]);
        let mut third: c_int = 0;
        // SAFETY: the format "ii|i" needs three int pointers.
        let status = unsafe {
            PyArg_ParseTupleAndKeywords(
                args as *mut c_void,
                kwargs as *mut c_void,
                c"ii|i:add".as_ptr(),
                optional_names.as_ptr(),
                &mut first as *mut c_int,
                &mut second as *mut c_int,
                &mut third as *mut c_int,
            )
        };
        assert_eq!(status, 0);
        let message = take_error().expect("unexpected keyword");
        assert!(
            message.contains("unexpected keyword argument 'z'"),
            "{message}"
        );

        // Arity: more arguments than units.
        let params = tuple_object(&[ShimValue::Int(1), ShimValue::Int(2), ShimValue::Int(3)]);
        let status = unsafe {
            PyArg_ParseTupleAndKeywords(
                params as *mut c_void,
                ptr::null_mut(),
                format.as_ptr(),
                names.as_ptr(),
                &mut first as *mut c_int,
                &mut second as *mut c_int,
            )
        };
        assert_eq!(status, 0);
        let message = take_error().expect("too many arguments");
        assert!(message.contains("takes at most 2 arguments"), "{message}");

        // Keyword name array shorter than the format: an extension bug.
        let args = tuple_object(&[ShimValue::Int(1), ShimValue::Int(2)]);
        let short_names = [c"a".as_ptr(), ptr::null()];
        let status = unsafe {
            PyArg_ParseTupleAndKeywords(
                args as *mut c_void,
                ptr::null_mut(),
                format.as_ptr(),
                short_names.as_ptr(),
                &mut first as *mut c_int,
                &mut second as *mut c_int,
            )
        };
        assert_eq!(status, 0);
        let message = take_error().expect("mismatched keyword names");
        assert!(message.starts_with("SystemError: "), "{message}");
        assert!(
            message.contains("keyword names for 2 format units"),
            "{message}"
        );

        // A NULL keyword array is rejected instead of dereferenced.
        let status = unsafe {
            PyArg_ParseTupleAndKeywords(
                args as *mut c_void,
                ptr::null_mut(),
                format.as_ptr(),
                ptr::null(),
                &mut first as *mut c_int,
                &mut second as *mut c_int,
            )
        };
        assert_eq!(status, 0);
        assert!(take_error().unwrap().contains("keyword name array is NULL"));

        // A non-dict keyword object is rejected rather than treated as empty.
        let status = unsafe {
            PyArg_ParseTupleAndKeywords(
                args as *mut c_void,
                PyLong_FromLong(1) as *mut c_void,
                format.as_ptr(),
                names.as_ptr(),
                &mut first as *mut c_int,
                &mut second as *mut c_int,
            )
        };
        assert_eq!(status, 0);
        assert!(take_error().unwrap().contains("keywords must be a dict"));
        assert!(!error_pending());
    }

    #[test]
    fn keyword_parser_handles_positional_only_parameters() {
        anchor();
        clear_error();
        // An empty name marks a positional-only parameter (PEP 570).
        let names = [c"".as_ptr(), c"b".as_ptr(), ptr::null()];
        let kwargs = keyword_dict(&[("b", ShimValue::Int(4))]);
        let args = tuple_object(&[ShimValue::Int(1)]);
        let mut first: c_int = 0;
        let mut second: c_int = 0;
        // SAFETY: the format "ii" needs two int pointers.
        let status = unsafe {
            PyArg_ParseTupleAndKeywords(
                args as *mut c_void,
                kwargs as *mut c_void,
                c"ii:split".as_ptr(),
                names.as_ptr(),
                &mut first as *mut c_int,
                &mut second as *mut c_int,
            )
        };
        assert_eq!(status, 1, "{:?}", take_error());
        assert_eq!((first, second), (1, 4));
        assert!(!error_pending());
    }

    #[test]
    fn fastcall_methods_receive_the_argument_vector() {
        extern "C" fn fast_sum(
            _module: *mut ShimObject,
            args: *const *mut ShimObject,
            nargs: isize,
        ) -> *mut ShimObject {
            let mut total = 0i64;
            for index in 0..nargs.max(0) as usize {
                // SAFETY: `nargs` elements were passed by the caller.
                let item = unsafe { *args.add(index) };
                if let Some(ShimKind::Int(number)) = kind_ref(item) {
                    total += *number;
                }
            }
            new_object(ShimKind::Int(total))
        }
        extern "C" fn fast_with_keywords(
            _module: *mut ShimObject,
            args: *const *mut ShimObject,
            nargs: isize,
            kwnames: *mut ShimObject,
        ) -> *mut ShimObject {
            let count = nargs.max(0);
            let names: i64 = if kwnames.is_null() { 0 } else { 1 };
            // Encode both facts so the test can see the NULL keyword tuple.
            let first = if count > 0 {
                // SAFETY: `count` elements were passed by the caller.
                match kind_ref(unsafe { *args }) {
                    Some(ShimKind::Int(number)) => *number,
                    _ => 0,
                }
            } else {
                0
            };
            new_object(ShimKind::Int(first * 10 + (count as i64) * 100 + names))
        }

        clear_error();
        let id = allocate_module_id();
        let module_object = new_object(ShimKind::Module(id));
        let methods = vec![
            MethodSpec {
                name: "fast_sum".to_string(),
                flags: METH_FASTCALL,
                function: fast_sum as *const () as usize,
            },
            MethodSpec {
                name: "fast_keywords".to_string(),
                flags: METH_FASTCALL | METH_KEYWORDS,
                function: fast_with_keywords as *const () as usize,
            },
            MethodSpec {
                name: "fast_conflict".to_string(),
                flags: METH_FASTCALL | METH_VARARGS,
                function: fast_sum as *const () as usize,
            },
        ];
        registry_lock().modules.insert(
            id,
            ModuleEntry {
                name: "fastcall_probe".to_string(),
                handle: 0,
                module_object: module_object as usize,
                attrs: HashMap::new(),
                methods,
            },
        );

        let result =
            call_method(id, 0, &[ShimValue::Int(2), ShimValue::Int(3)]).expect("fastcall method");
        assert!(matches!(result, ShimValue::Int(5)));

        let result = call_method(id, 1, &[ShimValue::Int(4)]).expect("fastcall keywords");
        // 4 * 10 + 1 * 100 + no keyword tuple.
        assert!(matches!(result, ShimValue::Int(140)), "{result:?}");

        let message = call_method(id, 2, &[]).expect_err("conflicting flags");
        assert!(message.contains("METH_FASTCALL"), "{message}");

        let _ = registry_lock().modules.remove(&id);
        assert!(!error_pending());
    }

    /// Builds an `extern "C"` method that returns its first argument.
    extern "C" fn type_new(
        _type: *mut ShimObject,
        args: *mut ShimObject,
        _kwds: *mut ShimObject,
    ) -> *mut ShimObject {
        if hydro_capi_arg_count(args) == 0 {
            return PyLong_FromLong(-1);
        }
        hydro_capi_arg(args, 0)
    }

    /// Builds an `extern "C"` no-argument method for a type's method table.
    extern "C" fn type_method(_self: *mut ShimObject, _args: *mut ShimObject) -> *mut ShimObject {
        PyLong_FromLong(7)
    }

    #[test]
    fn type_spec_records_the_name_and_supported_slots() {
        anchor();
        clear_error();
        let doc = c"probe type";
        let mut methods = [
            PyMethodDefMirror {
                ml_name: c"seven".as_ptr(),
                ml_meth: type_method as *mut c_void,
                ml_flags: METH_NOARGS,
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
                slot: PY_TP_METHODS,
                pfunc: methods.as_mut_ptr() as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: PY_TP_NEW,
                pfunc: type_new as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: PY_TP_DOC,
                pfunc: doc.as_ptr() as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: ptr::null_mut(),
            },
        ];
        let mut spec = PyTypeSpecMirror {
            name: c"probe.Thing".as_ptr(),
            basicsize: 16,
            itemsize: 0,
            flags: 0,
            slots: slots.as_mut_ptr(),
        };
        let type_object = PyType_FromSpec(&mut spec);
        assert!(!type_object.is_null(), "{:?}", take_error());

        // The type's short and qualified names are exposed as new references.
        match object_to_value(PyType_GetName(type_object)) {
            Ok(ShimValue::Str(text)) => assert_eq!(text, "Thing"),
            other => panic!("unexpected name value: {other:?}"),
        }
        match object_to_value(PyType_GetQualName(type_object)) {
            Ok(ShimValue::Str(text)) => assert_eq!(text, "probe.Thing"),
            other => panic!("unexpected qualified name: {other:?}"),
        }
        assert_eq!(PyType_GetFlags(type_object), 0);
        assert_eq!(
            PyType_GetSlot(type_object, PY_TP_NEW),
            type_new as *mut c_void
        );
        assert_eq!(
            PyType_GetSlot(type_object, PY_TP_METHODS),
            methods.as_mut_ptr() as *mut c_void
        );
        assert_eq!(
            PyType_GetSlot(type_object, PY_TP_DOC),
            doc.as_ptr() as *mut c_void
        );
        // An unset slot is NULL without an error, as CPython documents.
        assert!(PyType_GetSlot(type_object, 60).is_null());
        assert!(!error_pending());

        // Construction goes through the handle's Py_tp_new.
        let result = call_type_object(type_object as usize, &[ShimValue::Int(42)])
            .expect("type construction");
        assert!(matches!(result, ShimValue::Int(42)));

        // `PyType_FromSpecWithBases` accepts a NULL/empty base tuple only.
        assert!(!PyType_FromSpecWithBases(&mut spec, ptr::null_mut()).is_null());
        let empty_bases = PyTuple_New(0);
        assert!(!PyType_FromSpecWithBases(&mut spec, empty_bases).is_null());
        let bases = tuple_object(&[ShimValue::Int(1)]);
        assert!(PyType_FromSpecWithBases(&mut spec, bases).is_null());
        let message = take_error().expect("base types");
        assert!(
            message.contains("base types are not supported"),
            "{message}"
        );

        // Non-type objects are rejected by every type accessor.
        let number = PyLong_FromLong(1);
        assert!(PyType_GetName(number).is_null());
        assert!(take_error().unwrap().contains("expected a type object"));
        assert!(PyType_GetSlot(ptr::null_mut(), PY_TP_NEW).is_null());
        assert!(take_error().unwrap().contains("expected a type object"));
        assert_eq!(PyType_GetFlags(ptr::null_mut()), 0);
        assert!(take_error().is_some());
    }

    #[test]
    fn type_spec_rejects_unsupported_slots_and_flags() {
        anchor();
        clear_error();
        let mut init_slot = [
            PyTypeSlotMirror {
                // Py_tp_init: an instance initialiser has no representation.
                slot: 60,
                pfunc: type_new as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: ptr::null_mut(),
            },
        ];
        let mut spec = PyTypeSpecMirror {
            name: c"probe.Init".as_ptr(),
            basicsize: 16,
            itemsize: 0,
            flags: 0,
            slots: init_slot.as_mut_ptr(),
        };
        assert!(PyType_FromSpec(&mut spec).is_null());
        let message = take_error().expect("unsupported slot");
        assert!(message.starts_with("TypeError: "), "{message}");
        assert!(message.contains("unsupported type slot 60"), "{message}");
        assert!(message.contains("Py_tp_init"), "{message}");

        // Subtyping is not represented, so Py_TPFLAGS_BASETYPE is rejected.
        spec.slots = std::ptr::null_mut();
        spec.flags = 1 << 10;
        assert!(PyType_FromSpec(&mut spec).is_null());
        let message = take_error().expect("unsupported flags");
        assert!(message.contains("unsupported flags"), "{message}");

        // A spec without a slot table is an error rather than an empty type.
        spec.flags = 0;
        assert!(PyType_FromSpec(&mut spec).is_null());
        assert!(take_error().unwrap().contains("no slot table"));
        assert!(PyType_FromSpec(ptr::null_mut()).is_null());
        assert!(take_error().unwrap().contains("type spec is NULL"));
        assert!(!error_pending());
    }

    #[test]
    fn type_handles_become_callable_module_attributes() {
        anchor();
        clear_error();
        let mut slots = [
            PyTypeSlotMirror {
                slot: PY_TP_NEW,
                pfunc: type_new as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: ptr::null_mut(),
            },
        ];
        let mut spec = PyTypeSpecMirror {
            name: c"probe.Handle".as_ptr(),
            basicsize: 16,
            itemsize: 0,
            flags: 0,
            slots: slots.as_mut_ptr(),
        };
        let type_object = PyType_FromSpec(&mut spec);
        assert!(!type_object.is_null(), "{:?}", take_error());

        let mut definition = empty_definition();
        let (module_id, module_object) =
            create_module_from_definition(&mut definition).expect("module");
        assert_eq!(
            PyModule_AddObject(module_object, c"Handle".as_ptr(), type_object),
            0
        );
        match get_attr(module_id, "Handle").expect("type attribute") {
            ShimAttr::Type(address) => assert_eq!(address, type_object as usize),
            other => panic!("unexpected attribute: {other:?}"),
        }
        let _ = registry_lock().modules.remove(&module_id);

        // A type without Py_tp_new cannot be constructed.
        let mut bare_slots = [
            PyTypeSlotMirror {
                slot: PY_TP_DOC,
                pfunc: c"bare".as_ptr() as *mut c_void,
            },
            PyTypeSlotMirror {
                slot: 0,
                pfunc: ptr::null_mut(),
            },
        ];
        spec.name = c"probe.Bare".as_ptr();
        spec.slots = bare_slots.as_mut_ptr();
        let bare = PyType_FromSpec(&mut spec);
        assert!(!bare.is_null());
        let message = call_type_object(bare as usize, &[]).expect_err("no tp_new");
        assert!(message.starts_with("TypeError: "), "{message}");
        assert!(message.contains("cannot create 'probe.Bare'"), "{message}");
        assert!(!error_pending());
    }

    #[test]
    fn null_and_negative_arguments_report_errors() {
        clear_error();
        assert!(PyList_New(-1).is_null());
        assert!(error_pending());
        clear_error();
        assert_eq!(PyList_Size(ptr::null_mut()), -1);
        assert!(error_pending());
        clear_error();
        assert!(PyList_GetItem(ptr::null_mut(), 0).is_null());
        assert!(error_pending());
        clear_error();
        assert_eq!(PyTuple_Size(ptr::null_mut()), -1);
        assert!(error_pending());
        clear_error();
        assert!(PyBytes_FromStringAndSize(ptr::null(), 1).is_null());
        assert!(error_pending());
        clear_error();
        assert_eq!(
            PyDict_SetItem(ptr::null_mut(), ptr::null_mut(), ptr::null_mut()),
            -1
        );
        assert!(error_pending());
        clear_error();
    }

    /// Counts the entries of a shim dict without exposing an exported size
    /// function (CPython's `PyDict_Size` is not part of this slice).
    fn dict_len_for_test(dict: *mut ShimObject) -> usize {
        dict_slots(dict).map(|pairs| pairs.len()).unwrap_or(0)
    }

    /// Builds a minimal live `PyModuleDef` for slot parsing tests.
    fn empty_definition() -> PyModuleDefMirror {
        PyModuleDefMirror {
            ob_refcnt: 1,
            ob_type: ptr::null_mut(),
            m_init: ptr::null_mut(),
            m_index: 0,
            m_copy: ptr::null_mut(),
            m_name: c"slot_test_module".as_ptr(),
            m_doc: ptr::null(),
            m_size: 0,
            m_methods: ptr::null_mut(),
            m_slots: ptr::null_mut(),
            m_traverse: ptr::null_mut(),
            m_clear: ptr::null_mut(),
            m_free: ptr::null_mut(),
        }
    }

    #[test]
    fn module_def_init_wraps_a_definition() {
        clear_error();
        let mut definition = empty_definition();
        let object = PyModuleDef_Init(&mut definition);
        assert!(!object.is_null());
        assert!(!error_pending());
        match kind_ref(object) {
            Some(ShimKind::ModuleDef { def }) => {
                assert_eq!(*def, &mut definition as *mut PyModuleDefMirror as usize);
            }
            other => panic!("unexpected module definition object: {other:?}"),
        }
        let message = object_to_value(object).expect_err("definitions are not values");
        assert!(message.contains("module definition"), "{message}");

        clear_error();
        assert!(PyModuleDef_Init(ptr::null_mut()).is_null());
        assert!(error_pending());
        clear_error();
    }

    #[test]
    fn module_slots_run_until_the_zero_terminator() {
        use std::sync::atomic::AtomicUsize;
        clear_error();
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        extern "C" fn exec(_module: *mut ShimObject) -> c_int {
            CALLS.fetch_add(1, Ordering::Relaxed);
            0
        }
        CALLS.store(0, Ordering::Relaxed);
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
                value: ptr::null_mut(),
            },
            // Entries after the terminator are never read.
            PyModuleDefSlotMirror {
                slot: PY_MOD_EXEC,
                value: exec as *mut c_void,
            },
        ];
        let mut definition = empty_definition();
        definition.m_slots = slots.as_mut_ptr();
        let (id, module) = create_module_from_definition(&mut definition).expect("module");
        run_module_slots(&mut definition, module).expect("exec slots");
        assert_eq!(
            CALLS.load(Ordering::Relaxed),
            2,
            "the zero slot terminates the walk"
        );
        assert!(!error_pending());
        let _ = registry_lock().modules.remove(&id);
    }

    #[test]
    fn module_policy_slots_are_validated() {
        let mut accepted = [
            PyModuleDefSlotMirror {
                slot: PY_MOD_MULTIPLE_INTERPRETERS,
                value: ptr::without_provenance_mut(2),
            },
            PyModuleDefSlotMirror {
                slot: PY_MOD_GIL,
                value: ptr::without_provenance_mut(1),
            },
            PyModuleDefSlotMirror {
                slot: 0,
                value: ptr::null_mut(),
            },
        ];
        let mut definition = empty_definition();
        definition.m_slots = accepted.as_mut_ptr();
        run_module_slots(&mut definition, ptr::null_mut()).expect("accepted policy slots");

        let mut interpreters = [
            PyModuleDefSlotMirror {
                slot: PY_MOD_MULTIPLE_INTERPRETERS,
                value: ptr::without_provenance_mut(3),
            },
            PyModuleDefSlotMirror {
                slot: 0,
                value: ptr::null_mut(),
            },
        ];
        definition.m_slots = interpreters.as_mut_ptr();
        let message = run_module_slots(&mut definition, ptr::null_mut())
            .expect_err("invalid multiple-interpreters value");
        assert!(
            message.contains("Py_mod_multiple_interpreters"),
            "{message}"
        );

        let mut gil = [
            PyModuleDefSlotMirror {
                slot: PY_MOD_GIL,
                value: ptr::without_provenance_mut(2),
            },
            PyModuleDefSlotMirror {
                slot: 0,
                value: ptr::null_mut(),
            },
        ];
        definition.m_slots = gil.as_mut_ptr();
        let message =
            run_module_slots(&mut definition, ptr::null_mut()).expect_err("invalid GIL value");
        assert!(message.contains("Py_mod_gil"), "{message}");
    }

    #[test]
    fn unsupported_module_slots_are_rejected() {
        let mut create = [
            PyModuleDefSlotMirror {
                slot: PY_MOD_CREATE,
                value: ptr::null_mut(),
            },
            PyModuleDefSlotMirror {
                slot: 0,
                value: ptr::null_mut(),
            },
        ];
        let mut definition = empty_definition();
        definition.m_slots = create.as_mut_ptr();
        let message =
            run_module_slots(&mut definition, ptr::null_mut()).expect_err("Py_mod_create");
        assert!(message.contains("Py_mod_create"), "{message}");

        let mut unknown = [
            PyModuleDefSlotMirror {
                slot: 99,
                value: ptr::null_mut(),
            },
            PyModuleDefSlotMirror {
                slot: 0,
                value: ptr::null_mut(),
            },
        ];
        definition.m_slots = unknown.as_mut_ptr();
        let message = run_module_slots(&mut definition, ptr::null_mut()).expect_err("unknown slot");
        assert!(message.contains("unsupported Py_mod slot 99"), "{message}");
    }

    #[test]
    fn module_slot_exec_failures_are_reported() {
        clear_error();
        extern "C" fn status_only(_module: *mut ShimObject) -> c_int {
            1
        }
        extern "C" fn with_error(_module: *mut ShimObject) -> c_int {
            hydro_capi_set_error(c"exec failed".as_ptr());
            1
        }
        let mut slots = [
            PyModuleDefSlotMirror {
                slot: PY_MOD_EXEC,
                value: status_only as *mut c_void,
            },
            PyModuleDefSlotMirror {
                slot: 0,
                value: ptr::null_mut(),
            },
        ];
        let mut definition = empty_definition();
        definition.m_slots = slots.as_mut_ptr();
        let message = run_module_slots(&mut definition, ptr::null_mut()).expect_err("status");
        assert!(message.contains("failed with status 1"), "{message}");

        slots[0].value = with_error as *mut c_void;
        let message = run_module_slots(&mut definition, ptr::null_mut()).expect_err("error");
        assert_eq!(message, "exec failed");

        slots[0].value = ptr::null_mut();
        let message =
            run_module_slots(&mut definition, ptr::null_mut()).expect_err("NULL callback");
        assert!(message.contains("NULL Py_mod_exec"), "{message}");
        assert!(slots[0].value.is_null());
    }

    #[test]
    fn module_slot_iteration_is_bounded() {
        let mut slots = vec![
            PyModuleDefSlotMirror {
                slot: PY_MOD_GIL,
                value: ptr::null_mut(),
            };
            MAX_MODULE_SLOTS
        ];
        let mut definition = empty_definition();
        definition.m_slots = slots.as_mut_ptr();
        let message =
            run_module_slots(&mut definition, ptr::null_mut()).expect_err("unterminated table");
        assert!(message.contains("more than"), "{message}");
    }
}
