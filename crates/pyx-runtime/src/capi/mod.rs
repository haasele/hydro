//! Minimal CPython limited-API host surface used to load stable-ABI
//! (`abi3`) extension modules.
//!
//! PyX does not link `libpython`. When an extension is loaded with `dlopen`
//! its undefined `Py*` symbols resolve against the definitions exported from
//! this module; the `pyx` executable is linked with `--export-dynamic` so
//! those definitions also land in the dynamic symbol table. Only the small
//! subset declared here exists — an extension that calls anything else fails
//! to load instead of being silently mis-emulated.
//!
//! [`anchor`] must be called once from `pyx_runtime_init`: it references every
//! exported entry point so the linker keeps their object files in the final
//! static library instead of discarding them as unreachable.
//!
//! `parse.c` supplies the one variadic entry point, `PyArg_ParseTuple`, which
//! cannot be written with stable Rust's C variadics. It forwards every
//! conversion to the `pyx_capi_*` accessors below.
//!
//! Slice limitations, deliberately documented:
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
//!   back into fresh PyX containers; nothing is shared across the boundary.
//!   Returning any other value from an extension is reported as an error
//!   rather than converted.
//! - **Capsules** carry an opaque `void *` and an optional name. The
//!   destructor is stored but never invoked, because arena objects are never
//!   finalized; an extension that relies on capsule finalization is
//!   unsupported.
//! - **Exception types** are exported as the usual `PyExc_*` data symbols so
//!   extension code can pass them to `PyErr_SetString`. Their names select
//!   the exception category the loader reports; `BaseException`/`Exception`
//!   fall back to `RuntimeError`.
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

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_long, c_void};
use std::ptr;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

// CPython limited-API mirror structures

/// `METH_*` bits from CPython's `methodobject.h` (limited-API subset).
const METH_VARARGS: c_int = 0x0001;
const METH_KEYWORDS: c_int = 0x0002;
const METH_NOARGS: c_int = 0x0004;
const METH_O: c_int = 0x0008;
const METHOD_FLAGS: c_int = METH_VARARGS | METH_KEYWORDS | METH_NOARGS | METH_O;

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
/// Container variants are owned copies: passing a PyX list to an extension
/// copies it, and a container returned by an extension is converted into a
/// fresh PyX container. Nothing is shared across the boundary.
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

/// Declares the `PyExc_<Name>` data symbols exposed to extensions.
///
/// Extensions reference these as `PyObject *` globals (for example
/// `PyErr_SetString(PyExc_ValueError, ...)`), so each symbol must be exported
/// and populated before any extension initializer runs. They are not Python
/// values: only [`PyErr_SetString`] inspects them, to derive the exception
/// category.
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
    PyExc_BaseException => "BaseException",
    PyExc_Exception => "Exception",
    PyExc_TypeError => "TypeError",
    PyExc_ValueError => "ValueError",
    PyExc_RuntimeError => "RuntimeError",
    PyExc_AttributeError => "AttributeError",
    PyExc_IndexError => "IndexError",
    PyExc_KeyError => "KeyError",
    PyExc_OverflowError => "OverflowError",
    PyExc_ZeroDivisionError => "ZeroDivisionError",
    PyExc_StopIteration => "StopIteration",
    PyExc_OSError => "OSError",
}

/// Maps an exported exception singleton to the loader's message prefix.
///
/// `BaseException`, `Exception`, NULL and any unknown object have no dedicated
/// PyX tag and therefore fall back to `RuntimeError`.
fn exception_prefix(exception: *mut ShimObject) -> &'static str {
    match kind_ref(exception) {
        Some(ShimKind::ErrorType { name }) => match name.as_str() {
            "TypeError" => "TypeError",
            "ValueError" => "ValueError",
            "RuntimeError" => "RuntimeError",
            "AttributeError" => "AttributeError",
            "IndexError" => "IndexError",
            "KeyError" => "KeyError",
            "OverflowError" => "OverflowError",
            "ZeroDivisionError" => "ZeroDivisionError",
            "StopIteration" => "StopIteration",
            "OSError" => "OSError",
            _ => "RuntimeError",
        },
        _ => "RuntimeError",
    }
}

/// Turns a contract value into a fresh shim object.
fn value_object(value: ShimValue) -> *mut ShimObject {
    let kind = match value {
        ShimValue::None => ShimKind::None,
        ShimValue::Bool(flag) => ShimKind::Bool(flag),
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
            // PyX string) drops the name, matching the NULL-name case.
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

// Thread-local error state

std::thread_local! {
    /// Pending error set by an extension or by a failing conversion. This is
    /// the extension-host analogue of CPython's thread state; it is separate
    /// from the PyX runtime's own pending exception.
    static CALL_ERROR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn set_error(message: impl Into<String>) {
    CALL_ERROR.with(|slot| *slot.borrow_mut() = Some(message.into()));
}

/// Records a conversion failure reported by `PyArg_ParseTuple`/`parse.c`.
///
/// CPython raises `TypeError` for argument-format failures; the
/// `TypeError: ` prefix lets [`crate::modules::extension`] map the stored
/// message to the right pending exception.
#[no_mangle]
pub extern "C" fn pyx_capi_set_type_error(message: *const c_char) {
    if message.is_null() {
        set_error("TypeError: PyArg_ParseTuple failed");
        return;
    }
    // SAFETY: `parse.c` passes a valid NUL-terminated string literal.
    let text = unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned();
    set_error(format!("TypeError: {text}"));
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
        set_error(format!(
            "PyArg_ParseTuple: NULL output pointer for '{format_character}'"
        ));
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

    let mut methods = Vec::new();
    if !definition.m_methods.is_null() {
        let mut cursor = definition.m_methods;
        loop {
            // SAFETY: `m_methods` points at a NULL-terminated array of
            // `PyMethodDef`, so the current slot is readable until a NULL name.
            let method = unsafe { &*cursor };
            if method.ml_name.is_null() {
                break;
            }
            if methods.len() >= MAX_METHODS {
                return Err(format!(
                    "extension module '{name}' defines more than {MAX_METHODS} methods"
                ));
            }
            // SAFETY: `ml_name` is a NUL-terminated C string.
            let method_name = unsafe { CStr::from_ptr(method.ml_name) }
                .to_string_lossy()
                .into_owned();
            if method.ml_meth.is_null() {
                return Err(format!("method '{method_name}' has a NULL implementation"));
            }
            methods.push(MethodSpec {
                name: method_name,
                flags: method.ml_flags,
                function: method.ml_meth as usize,
            });
            // SAFETY: advancing one entry inside the NULL-terminated table.
            cursor = unsafe { cursor.add(1) };
        }
    }
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

    // SAFETY: `function` came from a `PyMethodDef.ml_meth` slot; the branch
    // below selects the signature implied by its METH_* flags.
    let address = spec.function as *const c_void;

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
    match object_to_value(value)
        .and_then(|value| add_attribute(module, name, ShimAttr::Value(value)))
    {
        Ok(()) => 0,
        Err(error) => {
            set_error(error);
            -1
        }
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
/// `"<Type>: "` prefix into the matching PyX exception tag. A NULL or
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

/// `PyErr_Clear`: drops the pending error.
#[no_mangle]
pub extern "C" fn PyErr_Clear() {
    clear_error();
}

// Accessors used by `parse.c`

/// Number of positional arguments in a tuple object (0 for non-tuples).
#[no_mangle]
pub extern "C" fn pyx_capi_arg_count(args: *mut ShimObject) -> usize {
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
pub extern "C" fn pyx_capi_arg(args: *mut ShimObject, index: usize) -> *mut ShimObject {
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

/// Records an error constructed on the C side of `PyArg_ParseTuple`.
#[no_mangle]
pub extern "C" fn pyx_capi_set_error(message: *const c_char) {
    if message.is_null() {
        set_error("extension reported an error");
    } else {
        set_error(read_c_string(message));
    }
}

/// `"i"` conversion: C `int`.
#[no_mangle]
pub extern "C" fn pyx_capi_parse_i(object: *mut ShimObject, out: *mut c_int) -> c_int {
    match as_int(object).and_then(|value| {
        i32::try_from(value).map_err(|_| "integer out of range for C int".to_string())
    }) {
        Ok(value) => store_argument(out, value, "i"),
        Err(error) => {
            set_error(error);
            0
        }
    }
}

/// `"l"` conversion: C `long`.
#[no_mangle]
pub extern "C" fn pyx_capi_parse_l(object: *mut ShimObject, out: *mut c_long) -> c_int {
    match as_int(object).and_then(|value| {
        c_long::try_from(value).map_err(|_| "integer out of range for C long".to_string())
    }) {
        Ok(value) => store_argument(out, value, "l"),
        Err(error) => {
            set_error(error);
            0
        }
    }
}

/// `"n"` conversion: `Py_ssize_t` (repr-compatible with Rust `isize`).
#[no_mangle]
pub extern "C" fn pyx_capi_parse_n(object: *mut ShimObject, out: *mut isize) -> c_int {
    match as_int(object)
        .and_then(|value| isize::try_from(value).map_err(|_| "integer out of range".to_string()))
    {
        Ok(value) => store_argument(out, value, "n"),
        Err(error) => {
            set_error(error);
            0
        }
    }
}

/// `"d"` conversion: C `double`.
#[no_mangle]
pub extern "C" fn pyx_capi_parse_d(object: *mut ShimObject, out: *mut f64) -> c_int {
    match as_float(object) {
        Ok(value) => store_argument(out, value, "d"),
        Err(error) => {
            set_error(error);
            0
        }
    }
}

/// `"s"` conversion: NUL-terminated `const char *`.
#[no_mangle]
pub extern "C" fn pyx_capi_parse_s(object: *mut ShimObject, out: *mut *const c_char) -> c_int {
    match as_c_string(object) {
        Ok(value) => store_argument(out, value, "s"),
        Err(error) => {
            set_error(error);
            0
        }
    }
}

/// `"O"` conversion: the shim object pointer itself.
#[no_mangle]
pub extern "C" fn pyx_capi_parse_o(object: *mut ShimObject, out: *mut *mut ShimObject) -> c_int {
    if object.is_null() {
        set_error("cannot convert a NULL object");
        return 0;
    }
    store_argument(out, object, "O")
}

/// `"p"` conversion: C `int` truth value.
#[no_mangle]
pub extern "C" fn pyx_capi_parse_p(object: *mut ShimObject, out: *mut c_int) -> c_int {
    match as_predicate(object) {
        Ok(value) => store_argument(out, value as c_int, "p"),
        Err(error) => {
            set_error(error);
            0
        }
    }
}

// Link anchor

extern "C" {
    /// Defined in `parse.c`; declared here so [`anchor`] can reference it and
    /// force the C object into the final link. The argument is declared as
    /// `c_void` because the Rust-side enum is deliberately not FFI-safe.
    fn PyArg_ParseTuple(args: *mut core::ffi::c_void, format: *const c_char, ...) -> c_int;
}

/// References every exported entry point so the linker keeps the definitions.
///
/// `pyx_runtime_init` calls this once. Without the reference the staticlib
/// linker would drop the otherwise unreachable object files, leaving
/// `dlopen`ed extensions with unresolved `Py*` symbols.
pub(crate) fn anchor() {
    // Data symbols must already be non-NULL when an extension initializer
    // evaluates `PyExc_*`, and the addresses must be referenced so the
    // staticlib linker does not drop them.
    initialize_exception_types();
    anchor_exception_types();
    let symbols: [*const (); 54] = [
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
        PyErr_Clear as *const (),
        PyArg_ParseTuple as *const (),
        pyx_capi_parse_i as *const (),
        pyx_capi_parse_l as *const (),
        pyx_capi_parse_n as *const (),
        pyx_capi_parse_d as *const (),
        pyx_capi_parse_s as *const (),
        pyx_capi_parse_o as *const (),
        pyx_capi_parse_p as *const (),
        pyx_capi_arg_count as *const (),
        pyx_capi_arg as *const (),
        pyx_capi_set_error as *const (),
        pyx_capi_set_type_error as *const (),
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
        let capsule = PyCapsule_New(pointer, c"pyx.demo".as_ptr(), ptr::null_mut());
        assert!(!capsule.is_null());
        assert!(!error_pending());

        // The value round trip carries the same pointer and name.
        match object_to_value(capsule) {
            Ok(ShimValue::Capsule {
                ptr: value,
                name: Some(name),
            }) => {
                assert_eq!(value, pointer as usize);
                assert_eq!(name, "pyx.demo");
            }
            other => panic!("unexpected capsule conversion: {other:?}"),
        }

        // The pointer and name are retrievable through the C API.
        assert_eq!(PyCapsule_GetPointer(capsule, c"pyx.demo".as_ptr()), pointer);
        assert!(!error_pending());
        // SAFETY: the capsule owns a `CString` that lives in the arena.
        let name = PyCapsule_GetName(capsule);
        assert_eq!(unsafe { CStr::from_ptr(name) }, c"pyx.demo");
        assert_eq!(PyCapsule_IsValid(capsule, c"pyx.demo".as_ptr()), 1);
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
        assert!(PyCapsule_GetPointer(ptr::null_mut(), c"pyx.demo".as_ptr()).is_null());
        assert!(error_pending());
        clear_error();
        assert!(PyCapsule_New(ptr::null_mut(), c"pyx.demo".as_ptr(), ptr::null_mut()).is_null());
        assert!(error_pending());
        clear_error();

        // SetPointer replaces the payload but keeps the name.
        let replacement = &OTHER as *const i64 as *mut c_void;
        assert_eq!(PyCapsule_SetPointer(capsule, replacement), 0);
        assert_eq!(
            PyCapsule_GetPointer(capsule, c"pyx.demo".as_ptr()),
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
        // NULL, non-exception objects and the tag-less base types fall back.
        PyErr_SetString(ptr::null_mut(), c"boom".as_ptr());
        assert_eq!(take_error().as_deref(), Some("RuntimeError: boom"));
        PyErr_SetString(unsafe { PyExc_BaseException }, c"base".as_ptr());
        assert_eq!(take_error().as_deref(), Some("RuntimeError: base"));
        PyErr_SetString(unsafe { PyExc_Exception }, c"generic".as_ptr());
        assert_eq!(take_error().as_deref(), Some("RuntimeError: generic"));
        PyErr_SetString(PyLong_FromLong(1), c"odd".as_ptr());
        assert_eq!(take_error().as_deref(), Some("RuntimeError: odd"));
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
            pyx_capi_set_error(c"exec failed".as_ptr());
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
