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
//! - **Values are limited** to the `ShimValue` variants plus tuples of shim
//!   objects. Returning anything else from an extension is reported as an
//!   error rather than converted.

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

// -------------------------------------------------------------------------
// CPython limited-API mirror structures
// -------------------------------------------------------------------------

/// `METH_*` bits from CPython's `methodobject.h` (limited-API subset).
const METH_VARARGS: c_int = 0x0001;
const METH_KEYWORDS: c_int = 0x0002;
const METH_NOARGS: c_int = 0x0004;
const METH_O: c_int = 0x0008;
const METHOD_FLAGS: c_int = METH_VARARGS | METH_KEYWORDS | METH_NOARGS | METH_O;

/// Upper bound on the entries read from a `PyMethodDef` table. CPython has no
/// declared limit; bounding it keeps a corrupt table from scanning forever.
const MAX_METHODS: usize = 4096;

/// Mirrors `PyModuleDef` from CPython 3.14 on x86_64, including
/// `PyModuleDef_Base` (`ob_refcnt`, `ob_type`, `m_init`, `m_index`,
/// `m_copy`) and the trailing name/doc/size/methods/slots/traverse/clear/free
/// fields. Only the name and method fields are inspected.
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
    m_slots: *mut c_void,
    m_traverse: *mut c_void,
    m_clear: *mut c_void,
    m_free: *mut c_void,
}

/// Mirrors `PyMethodDef`: `{ ml_name, ml_meth, ml_flags, ml_doc }`.
#[repr(C)]
#[allow(dead_code)]
pub(crate) struct PyMethodDefMirror {
    ml_name: *const c_char,
    ml_meth: *mut c_void,
    ml_flags: c_int,
    ml_doc: *const c_char,
}

// -------------------------------------------------------------------------
// Shim values and objects
// -------------------------------------------------------------------------

/// The crate-internal value contract shared with the extension loader.
#[derive(Clone, Debug)]
pub(crate) enum ShimValue {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
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

/// Internal representation of one shim object.
///
/// Tuple entries and module objects are stored as addresses because every
/// object lives in the arena for the whole process; that also keeps the
/// object `Send`/`Sync` without an unsafe impl.
#[derive(Clone, Debug)]
enum ShimKind {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// `c_text` is the NUL-terminated form used by `PyArg_ParseTuple("s")`;
    /// it is absent when the Python string contains an embedded NUL.
    Str {
        text: String,
        c_text: Option<CString>,
    },
    Bytes(Vec<u8>),
    Tuple(Vec<usize>),
    Module(u64),
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

// -------------------------------------------------------------------------
// Process-lifetime object arena
// -------------------------------------------------------------------------

#[allow(clippy::vec_box)] // the box keeps every PyObject address stable
fn arena_lock() -> MutexGuard<'static, Vec<Box<ShimObject>>> {
    #[allow(clippy::vec_box)]
    static ARENA: LazyLock<Mutex<Vec<Box<ShimObject>>>> = LazyLock::new(|| Mutex::new(Vec::new()));
    ARENA.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Allocates a shim object in the process-lifetime arena.
///
/// Objects are never freed: `Py_DecRef` only decrements the count. That is the
/// documented slice limitation which makes it impossible for an extension to
/// observe a dangling `PyObject *`.
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
        ShimValue::Bytes(bytes) => ShimKind::Bytes(bytes),
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
        ShimKind::Bytes(bytes) => Ok(ShimValue::Bytes(bytes.clone())),
        ShimKind::Tuple(_) => Err("extension returned a tuple, which is not supported".to_string()),
        ShimKind::Module(_) => {
            Err("extension returned a module object, which is not supported".to_string())
        }
        ShimKind::Error => Err("extension returned an error object".to_string()),
    }
}

// -------------------------------------------------------------------------
// Thread-local error state
// -------------------------------------------------------------------------

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

// -------------------------------------------------------------------------
// Conversion helpers for the C entry points
// -------------------------------------------------------------------------

/// Reads a NUL-terminated C string, replacing invalid UTF-8 lossily.
fn read_c_string(pointer: *const c_char) -> String {
    if pointer.is_null() {
        return String::new();
    }
    // SAFETY: callers only pass pointers to NUL-terminated strings (module
    // names, attribute names and values from the CPython C API contract).
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
        ShimKind::Bytes(bytes) => !bytes.is_empty(),
        ShimKind::Tuple(items) => !items.is_empty(),
        ShimKind::Module(_) | ShimKind::Error => true,
    })
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

// -------------------------------------------------------------------------
// Module registry
// -------------------------------------------------------------------------

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
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            // Module ids start at 1; 0 is never a valid module.
            next_id: 1,
            modules: HashMap::new(),
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
        return Err("PyModule_Create2: module definition is NULL".to_string());
    }
    // SAFETY: the extension owns a live `PyModuleDef` for at least the
    // duration of its `PyInit_*` call; the mirror matches CPython 3.14 x86_64.
    let definition = unsafe { &*definition };
    if definition.m_name.is_null() {
        return Err("PyModule_Create2: module definition has no name".to_string());
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

// -------------------------------------------------------------------------
// Loader contract
// -------------------------------------------------------------------------

/// Loads an extension shared object and runs its `PyInit_<init_name>`.
///
/// `init_name` is the bare module name (`foo`, not `PyInit_foo`). The
/// `dlopen` handle is kept open for the process lifetime. Returns the module
/// id used by [`get_attr`] and [`call_method`].
pub(crate) fn load_extension(path: &str, init_name: &str) -> Result<u64, String> {
    let path = CString::new(path)
        .map_err(|_| "extension path contains an embedded null character".to_string())?;
    let symbol = CString::new(format!("PyInit_{init_name}"))
        .map_err(|_| "extension init name contains an embedded null character".to_string())?;

    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
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
    let module = unsafe { init() };
    if module.is_null() {
        let error = take_error().unwrap_or_else(|| format!("PyInit_{init_name} returned NULL"));
        // SAFETY: the initializer failed and handed nothing back; the module
        // cannot be called later.
        unsafe { libc::dlclose(handle) };
        return Err(error);
    }

    // SAFETY: `module` points into the arena and stays alive for the process.
    let kind = unsafe { (*module).kind.clone() };
    let ShimKind::Module(id) = kind else {
        // SAFETY: no shim module was produced; nothing can call into the
        // extension through the registry.
        unsafe { libc::dlclose(handle) };
        return Err(format!("PyInit_{init_name} did not create a module"));
    };

    let mut registry = registry_lock();
    let Some(entry) = registry.modules.get_mut(&id) else {
        return Err(format!("PyInit_{init_name} registered no module"));
    };
    entry.handle = handle as usize;
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

// -------------------------------------------------------------------------
// Exported CPython limited-API surface
// -------------------------------------------------------------------------

/// `PyModule_Create2`: registers a module parsed from a `PyModuleDef`.
#[no_mangle]
pub extern "C" fn PyModule_Create2(
    definition: *mut PyModuleDefMirror,
    _api_version: c_int,
) -> *mut ShimObject {
    let (name, methods) = match read_module_definition(definition) {
        Ok(parsed) => parsed,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };

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
    module_object
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

/// `Py_IncRef`: increments a shim object's reference count.
#[no_mangle]
pub extern "C" fn Py_IncRef(object: *mut ShimObject) {
    if object.is_null() {
        return;
    }
    // SAFETY: every non-NULL shim object lives in the arena for the process
    // lifetime, so the reference count is always accessible.
    unsafe { (*object).refcnt.fetch_add(1, Ordering::Relaxed) };
}

/// `Py_DecRef`: decrements a shim object's reference count.
///
/// Documented slice limitation: the object is never freed even when the count
/// reaches zero, which makes stale pointers harmless but retains memory.
#[no_mangle]
pub extern "C" fn Py_DecRef(object: *mut ShimObject) {
    if object.is_null() {
        return;
    }
    // SAFETY: every non-NULL shim object lives in the arena for the process
    // lifetime, so the reference count is always accessible.
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
    // SAFETY: every non-NULL shim object lives in the arena for the process
    // lifetime, so the reference count is always accessible.
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
    // SAFETY: every non-NULL shim object lives in the arena for the process
    // lifetime, so the reference count is always accessible.
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

/// `PyErr_SetString`: records the message of a pending error.
///
/// The exception type is accepted for signature compatibility and ignored;
/// this host only preserves the message.
#[no_mangle]
pub extern "C" fn PyErr_SetString(_exception: *mut ShimObject, message: *const c_char) {
    let text = if message.is_null() {
        "extension error".to_string()
    } else {
        read_c_string(message)
    };
    set_error(text);
}

/// `PyErr_Clear`: drops the pending error.
#[no_mangle]
pub extern "C" fn PyErr_Clear() {
    clear_error();
}

// -------------------------------------------------------------------------
// Accessors used by `parse.c`
// -------------------------------------------------------------------------

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

// -------------------------------------------------------------------------
// Link anchor
// -------------------------------------------------------------------------

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
    let symbols: [*const (); 24] = [
        PyModule_Create2 as *const (),
        PyModule_AddStringConstant as *const (),
        PyModule_AddObject as *const (),
        PyLong_FromLong as *const (),
        PyLong_AsLong as *const (),
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
