//! Object-level entry points: attribute queries, managed dictionaries, type
//! lookup, finalisation/GC state, weak references, thread locks, code and
//! frame objects, and the fatal-error hook.
//!
//! `capi::mod` registers this module; it is a separate child module so the
//! object-model surface stays reviewable next to the type system it drives.
//! Every entry point here reuses the shim internals of the parent module:
//! handles keep CPython's `PyObject_HEAD`, reference counts are real, and
//! attribute lookup goes through the same `attribute_lookup` /
//! `generic_attribute` path as `PyObject_GetAttr`.
//!
//! # Code, frame and descriptor objects
//!
//! This host had no code, frame or descriptor instances (see the
//! "Remaining limits" section of the parent module). They are needed as real
//! objects, because extension modules reach into them:
//!
//! * Cython's `__Pyx_AddTraceback` calls `PyCode_NewEmpty`, then `PyFrame_New`,
//!   writes `frame->f_lineno` (offset `0x28`) and calls `PyTraceBack_Here`.
//! * Cython's cached attribute lookup calls `_PyType_Lookup(type, name)`,
//!   reads `Py_TYPE(descriptor)->tp_descr_get` (offset `0x110`) and invokes it
//!   as `tp_descr_get(descriptor, self, type)`.
//!
//! Both patterns touch object layout directly, so the objects are allocated as
//! extension instances (`TypeRole::Instance`) of private static types whose
//! `tp_basicsize` covers a struct that mirrors CPython 3.14's layout for every
//! field an extension can read or write. Allocating through
//! `types::allocate_instance` means `Py_TYPE`, `Py_REFCNT`, `Py_DecRef` and
//! `tp_dealloc` behave exactly like every other extension instance, and the
//! private type's `tp_dealloc` (installed below) releases the owned children
//! and the storage.
//!
//! Frames created here are inert: they own references to their code, globals
//! and locals, they are not pushed on any interpreter stack, and no frame ever
//! executes. `PyThreadState_GetFrame` therefore returns `NULL`, which is
//! CPython's answer for "no current frame". There is still no traceback object
//! model (the parent module documents that `PyTraceBack_Print` accepts only
//! `None`), so `PyTraceBack_Here` validates its frame and reports success
//! without attaching anything; returning `-1` there would replace the pending
//! exception of the very error path that called it.
//!
//! # Instance dictionaries and weak references
//!
//! Handles have no instance dictionary: `generic_attribute`/
//! `generic_setattr` resolve members, get-sets and methods only, and
//! `PyType_FromSpec` rejects `Py_TPFLAGS_MANAGED_DICT`. The managed-dictionary
//! entry points are therefore consistent no-ops: there is never a dictionary
//! to visit, to clear or to point at. `_PyObject_GetDictPtr` returns `NULL`,
//! which is CPython's documented result for an object without a dictionary
//! slot (its callers fall back to the normal attribute path).
//!
//! No type in this host sets `tp_weaklistoffset` (nor the managed-weakref
//! flag, which `PyType_FromSpec` rejects), so every object is non-weakrefable.
//! `PyWeakref_NewRef` accordingly raises the `TypeError` CPython itself raises
//! for a non-weakrefable object instead of inventing a weak-reference object
//! whose referent death the host cannot observe.
//!
//! # Garbage collection and finalisation
//!
//! `PyGC_Enable`/`PyGC_Disable` record the requested state and return the
//! previous one, exactly like CPython, but the host never collects handle
//! cycles (they leak instead of dangling), so the switch has no other effect.
//!
//! Finalisation state is kept in a side table keyed by object address: this
//! host's instances have no CPython GC header to hold `_PyGC_FINALIZED`. The
//! deallocator in the parent module (`free_instance`) runs `tp_finalize`
//! before `tp_dealloc`, and `tp_dealloc` is the only caller of
//! `PyObject_CallFinalizerFromDealloc`, so a zero reference count there means
//! the finalizer already ran. Marking and returning `0` without running it
//! again keeps `__del__` single-shot. (The side table keeps the marker after
//! the object is freed; a later object that reuses the address could be
//! reported as finalised, which is the same address-keyed approximation the
//! parent module uses for its GC-tracked set. A finalizer that asks
//! `PyObject_GC_IsFinalized(self)` from inside `__del__` also sees `0`: the
//! parent's deallocator sets no marker of its own before running it.)

use std::collections::{HashMap, HashSet};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, Once, PoisonError};

use super::*;

/// Upper bound on the arguments read by `PyObject_CallMethodObjArgs`.
///
/// CPython has no bound: its varargs are NUL-terminated by the caller. The
/// bound only keeps a caller that forgot the terminator from scanning
/// arbitrary memory.
const MAX_CALL_METHOD_ARGS: usize = 1024;

// ---------------------------------------------------------------------------
// Private type objects for code, frame and descriptor instances
// ---------------------------------------------------------------------------

/// CPython 3.14 `PyCodeObject` prefix, followed by the host's own tail.
///
/// The prefix matches `Include/cpython/code.h` (`_PyCode_DEF`, non-free-
/// threaded build) so an extension that inspects a code object reads the
/// documented field. `co_code` lives in the Rust-only tail at the offset where
/// CPython starts its inline `co_code_adaptive` array: the bytecode is kept as
/// the bytes object the constructor received, because a fixed struct cannot
/// hold a variable-length instruction array. The public API reaches the
/// bytecode through `PyCode_GetCode`-style functions (not exported here);
/// reading it in place needs CPython's internal headers, which extension
/// modules cannot be built against.
#[repr(C)]
#[allow(dead_code)]
pub(crate) struct CodeObject {
    head: PyObjectHead,
    ob_size: AtomicIsize,
    co_consts: *mut ShimObject,
    co_names: *mut ShimObject,
    co_exceptiontable: *mut ShimObject,
    co_flags: c_int,
    co_argcount: c_int,
    co_posonlyargcount: c_int,
    co_kwonlyargcount: c_int,
    co_stacksize: c_int,
    co_firstlineno: c_int,
    co_nlocalsplus: c_int,
    co_framesize: c_int,
    co_nlocals: c_int,
    co_ncellvars: c_int,
    co_nfreevars: c_int,
    co_version: u32,
    co_localsplusnames: *mut ShimObject,
    co_localspluskinds: *mut ShimObject,
    co_filename: *mut ShimObject,
    co_name: *mut ShimObject,
    co_qualname: *mut ShimObject,
    co_linetable: *mut ShimObject,
    co_weakreflist: *mut ShimObject,
    co_executors: *mut c_void,
    _co_cached: *mut c_void,
    _co_instrumentation_version: usize,
    _co_monitoring: *mut c_void,
    _co_unique_id: isize,
    _co_firsttraceable: c_int,
    co_extra: *mut c_void,
    // Rust-only tail: nowhere for the adapter to put inline bytecode.
    co_code: *mut ShimObject,
}

/// CPython 3.14 `PyFrameObject` prefix, followed by the host's own tail.
///
/// Offsets are load-bearing: Cython's traceback helper stores `f_lineno`
/// (`0x28`) through the returned pointer. `_f_frame_data` is omitted because
/// this host never owns an interpreter frame; no frame executes here.
#[repr(C)]
#[allow(dead_code)]
pub(crate) struct FrameObject {
    head: PyObjectHead,
    f_back: *mut FrameObject,
    f_frame: *mut c_void,
    f_trace: *mut ShimObject,
    f_lineno: c_int,
    f_trace_lines: c_char,
    f_trace_opcodes: c_char,
    _reserved: [u8; 2],
    f_extra_locals: *mut ShimObject,
    f_locals_cache: *mut ShimObject,
    f_overwritten_fast_locals: *mut ShimObject,
    // Rust-only tail: the objects this frame keeps alive.
    code: *mut CodeObject,
    globals: *mut ShimObject,
    locals: *mut ShimObject,
}

/// Storage shared by the three descriptor types: the address of the
/// `PyMethodDef`/`PyGetSetDef`/`PyMemberDef` entry it describes.
#[repr(C)]
pub(crate) struct DescriptorObject {
    head: PyObjectHead,
    definition: usize,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::offset_of!(CodeObject, co_consts) == 0x18);
    assert!(std::mem::offset_of!(CodeObject, co_flags) == 0x30);
    assert!(std::mem::offset_of!(CodeObject, co_firstlineno) == 0x44);
    assert!(std::mem::offset_of!(CodeObject, co_localsplusnames) == 0x60);
    assert!(std::mem::offset_of!(CodeObject, co_filename) == 0x70);
    assert!(std::mem::offset_of!(CodeObject, co_linetable) == 0x88);
    assert!(std::mem::offset_of!(CodeObject, co_extra) == 0xc8);
    assert!(std::mem::offset_of!(FrameObject, f_back) == 0x10);
    assert!(std::mem::offset_of!(FrameObject, f_frame) == 0x18);
    assert!(std::mem::offset_of!(FrameObject, f_trace) == 0x20);
    assert!(std::mem::offset_of!(FrameObject, f_lineno) == 0x28);
    assert!(std::mem::offset_of!(FrameObject, f_extra_locals) == 0x30);
    assert!(std::mem::offset_of!(FrameObject, code) == 0x48);
    assert!(std::mem::offset_of!(DescriptorObject, definition) == 0x10);
};

static mut CODE_TYPE: TypeObject = TypeObject::builtin(
    c"code".as_ptr(),
    TypeRole::Instance,
    PY_TPFLAGS_IMMUTABLETYPE,
);
static mut FRAME_TYPE: TypeObject = TypeObject::builtin(
    c"frame".as_ptr(),
    TypeRole::Instance,
    PY_TPFLAGS_IMMUTABLETYPE,
);
static mut METHOD_DESCRIPTOR_TYPE: TypeObject = TypeObject::builtin(
    c"method_descriptor".as_ptr(),
    TypeRole::Instance,
    PY_TPFLAGS_IMMUTABLETYPE,
);
static mut GETSET_DESCRIPTOR_TYPE: TypeObject = TypeObject::builtin(
    c"getset_descriptor".as_ptr(),
    TypeRole::Instance,
    PY_TPFLAGS_IMMUTABLETYPE,
);
static mut MEMBER_DESCRIPTOR_TYPE: TypeObject = TypeObject::builtin(
    c"member_descriptor".as_ptr(),
    TypeRole::Instance,
    PY_TPFLAGS_IMMUTABLETYPE,
);

/// Fills in the layouts and slots of the private types, once.
///
/// The private types are not part of the builtin table patched by
/// `ensure_builtin_types`, so they are linked to the metatype and given their
/// instance size and destructors here. `allocate_instance` completes the rest
/// of `PyType_Ready` on first use.
fn ensure_private_types() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        types::ensure_builtin_types();
        // SAFETY: the statics are process-lifetime type tables; this is the
        // single writer, and every reader runs after `call_once`.
        unsafe {
            let metatype = ptr::addr_of_mut!(types::TYPE_TYPE);
            let base = ptr::addr_of_mut!(types::OBJECT_TYPE);

            let code = ptr::addr_of_mut!(CODE_TYPE);
            (*code).ob_type = metatype;
            (*code).tp_base = base;
            (*code).tp_basicsize = std::mem::size_of::<CodeObject>() as isize;
            (*code).tp_dealloc = code_dealloc as *const () as usize;

            let frame = ptr::addr_of_mut!(FRAME_TYPE);
            (*frame).ob_type = metatype;
            (*frame).tp_base = base;
            (*frame).tp_basicsize = std::mem::size_of::<FrameObject>() as isize;
            (*frame).tp_dealloc = frame_dealloc as *const () as usize;

            let method_descriptor = ptr::addr_of_mut!(METHOD_DESCRIPTOR_TYPE);
            (*method_descriptor).ob_type = metatype;
            (*method_descriptor).tp_base = base;
            (*method_descriptor).tp_basicsize = std::mem::size_of::<DescriptorObject>() as isize;
            (*method_descriptor).tp_dealloc = descriptor_dealloc as *const () as usize;
            (*method_descriptor).tp_descr_get = method_descriptor_get as *const () as usize;

            let getset_descriptor = ptr::addr_of_mut!(GETSET_DESCRIPTOR_TYPE);
            (*getset_descriptor).ob_type = metatype;
            (*getset_descriptor).tp_base = base;
            (*getset_descriptor).tp_basicsize = std::mem::size_of::<DescriptorObject>() as isize;
            (*getset_descriptor).tp_dealloc = descriptor_dealloc as *const () as usize;
            (*getset_descriptor).tp_descr_get = getset_descriptor_get as *const () as usize;
            (*getset_descriptor).tp_descr_set = getset_descriptor_set as *const () as usize;

            let member_descriptor = ptr::addr_of_mut!(MEMBER_DESCRIPTOR_TYPE);
            (*member_descriptor).ob_type = metatype;
            (*member_descriptor).tp_base = base;
            (*member_descriptor).tp_basicsize = std::mem::size_of::<DescriptorObject>() as isize;
            (*member_descriptor).tp_dealloc = descriptor_dealloc as *const () as usize;
            (*member_descriptor).tp_descr_get = member_descriptor_get as *const () as usize;
            (*member_descriptor).tp_descr_set = member_descriptor_set as *const () as usize;
        }
    });
}

/// Releases a code object's owned children and its storage.
///
/// Runs as the private type's `tp_dealloc`, i.e. from `free_instance` after
/// the count reached zero. CPython's `code_dealloc` also clears the cached
/// `_co_cached`/`co_extra` state; this host never creates either.
unsafe extern "C" fn code_dealloc(object: *mut ShimObject) {
    if object.is_null() {
        return;
    }
    let code = object as *mut CodeObject;
    // SAFETY: `object` is a live code allocation whose reference count just
    // reached zero, so this thread owns it.
    unsafe {
        for child in [
            (*code).co_consts,
            (*code).co_names,
            (*code).co_exceptiontable,
            (*code).co_localsplusnames,
            (*code).co_localspluskinds,
            (*code).co_filename,
            (*code).co_name,
            (*code).co_qualname,
            (*code).co_linetable,
            (*code).co_code,
        ] {
            if !child.is_null() {
                dec_ref(child);
            }
        }
        libc::free(object as *mut c_void);
    }
}

/// Releases a frame object's owned children and its storage.
unsafe extern "C" fn frame_dealloc(object: *mut ShimObject) {
    if object.is_null() {
        return;
    }
    let frame = object as *mut FrameObject;
    // SAFETY: as in `code_dealloc`; the children are this frame's references.
    unsafe {
        for child in [
            (*frame).code as *mut ShimObject,
            (*frame).globals,
            (*frame).locals,
        ] {
            if !child.is_null() {
                dec_ref(child);
            }
        }
        libc::free(object as *mut c_void);
    }
}

/// Releases a descriptor object.
///
/// Descriptors are immortal (they are cached for the lifetime of the type they
/// describe), so this only exists to keep `tp_dealloc` well defined.
unsafe extern "C" fn descriptor_dealloc(object: *mut ShimObject) {
    if object.is_null() {
        return;
    }
    // SAFETY: the caller owned the allocation.
    unsafe { libc::free(object as *mut c_void) };
}

/// `tp_descr_get` of a method descriptor: binds the `PyMethodDef`.
unsafe extern "C" fn method_descriptor_get(
    descriptor: *mut ShimObject,
    object: *mut ShimObject,
    _type_object: *mut ShimObject,
) -> *mut ShimObject {
    if descriptor.is_null() {
        set_error("SystemError: method descriptor is NULL");
        return ptr::null_mut();
    }
    if object.is_null() {
        // Lookup on the class itself returns the descriptor, not a binding.
        inc_ref(descriptor);
        return descriptor;
    }
    // SAFETY: a method descriptor is a `DescriptorObject`.
    let definition = unsafe { (*(descriptor as *mut DescriptorObject)).definition };
    PyCFunction_NewEx(
        definition as *mut PyMethodDefMirror,
        object,
        ptr::null_mut(),
    )
}

/// `tp_descr_get` of a get-set descriptor.
unsafe extern "C" fn getset_descriptor_get(
    descriptor: *mut ShimObject,
    object: *mut ShimObject,
    _type_object: *mut ShimObject,
) -> *mut ShimObject {
    if descriptor.is_null() {
        set_error("SystemError: getset descriptor is NULL");
        return ptr::null_mut();
    }
    if object.is_null() {
        inc_ref(descriptor);
        return descriptor;
    }
    // SAFETY: a get-set descriptor is a `DescriptorObject`.
    let definition = unsafe { (*(descriptor as *mut DescriptorObject)).definition };
    let definition = definition as *mut types::PyGetSetDefMirror;
    match types::getset_get(object, definition) {
        Ok(value) => value,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `tp_descr_set` of a get-set descriptor.
unsafe extern "C" fn getset_descriptor_set(
    descriptor: *mut ShimObject,
    object: *mut ShimObject,
    value: *mut ShimObject,
) -> c_int {
    if descriptor.is_null() || object.is_null() {
        set_error("SystemError: getset descriptor needs a descriptor and an instance");
        return -1;
    }
    // SAFETY: a get-set descriptor is a `DescriptorObject`.
    let definition = unsafe { (*(descriptor as *mut DescriptorObject)).definition };
    let definition = definition as *mut types::PyGetSetDefMirror;
    match types::getset_set(object, definition, value) {
        Ok(()) => 0,
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

/// `tp_descr_get` of a member descriptor.
unsafe extern "C" fn member_descriptor_get(
    descriptor: *mut ShimObject,
    object: *mut ShimObject,
    _type_object: *mut ShimObject,
) -> *mut ShimObject {
    if descriptor.is_null() {
        set_error("SystemError: member descriptor is NULL");
        return ptr::null_mut();
    }
    if object.is_null() {
        inc_ref(descriptor);
        return descriptor;
    }
    // SAFETY: a member descriptor is a `DescriptorObject`.
    let definition = unsafe { (*(descriptor as *mut DescriptorObject)).definition };
    let definition = definition as *mut types::PyMemberDefMirror;
    match types::member_get(object, definition) {
        Ok(value) => value,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `tp_descr_set` of a member descriptor.
unsafe extern "C" fn member_descriptor_set(
    descriptor: *mut ShimObject,
    object: *mut ShimObject,
    value: *mut ShimObject,
) -> c_int {
    if descriptor.is_null() || object.is_null() {
        set_error("SystemError: member descriptor needs a descriptor and an instance");
        return -1;
    }
    // SAFETY: a member descriptor is a `DescriptorObject`.
    let definition = unsafe { (*(descriptor as *mut DescriptorObject)).definition };
    let definition = definition as *mut types::PyMemberDefMirror;
    match types::member_set(object, definition, value) {
        Ok(()) => 0,
        Err(error) => {
            set_error(error);
            -1
        }
    }
}

/// Descriptors handed out by `_PyType_Lookup`.
///
/// `_PyType_Lookup` returns a borrowed reference, so the descriptor must
/// outlive the call: the entries in this table own their descriptor (which is
/// also immortal) for the process lifetime. Keys are the owning type's
/// address and the attribute name; a heap type that is freed and whose address
/// is reused can therefore hit a stale entry, exactly like the address-keyed
/// GC set in the parent module.
static DESCRIPTORS: LazyLock<Mutex<HashMap<(usize, String), usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn descriptor_cache() -> MutexGuard<'static, HashMap<(usize, String), usize>> {
    DESCRIPTORS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Builds a descriptor of `type_object` for the definition at `definition`.
fn new_descriptor(type_object: *mut TypeObject, definition: usize) -> *mut ShimObject {
    ensure_private_types();
    let descriptor = types::allocate_instance(type_object, 0);
    if descriptor.is_null() {
        // `allocate_instance` recorded the `MemoryError`.
        return ptr::null_mut();
    }
    // SAFETY: freshly allocated `DescriptorObject` storage.
    unsafe {
        (*(descriptor as *mut DescriptorObject)).definition = definition;
    }
    make_immortal(descriptor)
}

// ---------------------------------------------------------------------------
// Attribute queries
// ---------------------------------------------------------------------------

/// `PyObject_HasAttr`: `1` when the attribute exists, `0` otherwise.
///
/// Like CPython's, this entry point cannot report an error: a failing lookup
/// (including a non-string attribute name) is a missing attribute, and any
/// exception left behind by a custom `tp_getattro` is dropped so the caller's
/// error state is untouched.
#[no_mangle]
pub unsafe extern "C" fn PyObject_HasAttr(object: *mut ShimObject, name: *mut ShimObject) -> c_int {
    if object.is_null() || name.is_null() {
        return 0;
    }
    let Some(name) = as_string(name) else {
        return 0;
    };
    match attribute_lookup(object, &name) {
        Ok(value) => {
            dec_ref(value);
            1
        }
        Err(_) => {
            if error_pending() {
                let _ = take_error();
            }
            0
        }
    }
}

/// `PyObject_CallMethodObjArgs`: `getattr(object, name)(arg, ..., NULL)`.
///
/// The arguments are borrowed for the duration of the call; the result is a
/// new reference. The lookup uses the same path as `PyObject_GetAttr`, so a
/// method resolved through `tp_methods`, a member, a get-set or a custom
/// `tp_getattro` is called exactly as an extension would call it.
#[no_mangle]
pub unsafe extern "C" fn PyObject_CallMethodObjArgs(
    object: *mut ShimObject,
    name: *mut ShimObject,
    mut arguments: ...
) -> *mut ShimObject {
    if object.is_null() {
        set_error("SystemError: PyObject_CallMethodObjArgs: object is NULL");
        return ptr::null_mut();
    }
    if name.is_null() {
        set_error("SystemError: PyObject_CallMethodObjArgs: method name is NULL");
        return ptr::null_mut();
    }
    let Some(method_name) = as_string(name) else {
        set_error("TypeError: attribute name must be string");
        return ptr::null_mut();
    };
    let callable = match attribute_lookup(object, &method_name) {
        Ok(callable) => callable,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let mut values: Vec<*mut ShimObject> = Vec::new();
    loop {
        // SAFETY: the caller passes a NUL-terminated list of `PyObject *`,
        // exactly as CPython's varargs contract requires.
        let argument: *mut ShimObject = unsafe { arguments.next_arg() };
        if argument.is_null() {
            break;
        }
        if values.len() == MAX_CALL_METHOD_ARGS {
            dec_ref(callable);
            set_error("SystemError: PyObject_CallMethodObjArgs: too many arguments");
            return ptr::null_mut();
        }
        values.push(argument);
    }
    let result = hydro_capi_vectorcall(callable, values.as_ptr(), values.len());
    dec_ref(callable);
    result
}

// ---------------------------------------------------------------------------
// Managed (instance) dictionaries
// ---------------------------------------------------------------------------

/// `PyObject_VisitManagedDict`: visits the object's managed dictionary.
///
/// No object in this host has an instance dictionary (see the module docs),
/// so there is nothing to visit and the traversal reports success.
#[no_mangle]
pub unsafe extern "C" fn PyObject_VisitManagedDict(
    object: *mut ShimObject,
    _visit: Option<unsafe extern "C" fn(*mut ShimObject, *mut c_void) -> c_int>,
    _argument: *mut c_void,
) -> c_int {
    if object.is_null() {
        set_error("SystemError: PyObject_VisitManagedDict: object is NULL");
        return -1;
    }
    0
}

/// `PyObject_ClearManagedDict`: clears the object's managed dictionary.
///
/// There is never one to clear; `tp_clear` implementations that call this (as
/// Cython's generated code does) observe their objects consistently.
#[no_mangle]
pub unsafe extern "C" fn PyObject_ClearManagedDict(_object: *mut ShimObject) {}

/// `_PyObject_GetDictPtr`: the address of the object's `__dict__` slot.
///
/// CPython returns `NULL` for an object without a dictionary slot, which is
/// the case for every handle here: instance dictionaries are not implemented,
/// so no slot exists even for a type that declares `tp_dictoffset`. Callers
/// (Cython's `_PyObject_GetDictPtr`-based attribute fast path) fall back to the
/// ordinary attribute protocol.
#[no_mangle]
pub unsafe extern "C" fn _PyObject_GetDictPtr(object: *mut ShimObject) -> *mut *mut ShimObject {
    if object.is_null() {
        return ptr::null_mut();
    }
    ptr::null_mut()
}

// ---------------------------------------------------------------------------
// Type lookup
// ---------------------------------------------------------------------------

/// Borrows the value stored for `name` in a type's `tp_dict`, walking the MRO.
///
/// CPython stores every class attribute in the metatype-managed type
/// dictionary; this host's types keep methods, members and get-sets in their
/// own tables, but a type that does have a `tp_dict` (such as a heap type built
/// by a future `PyType_FromSpec` change) is consulted first, exactly as
/// CPython's `_PyType_Lookup` does.
fn type_dict_attribute(type_object: *mut TypeObject, name: &str) -> Option<*mut ShimObject> {
    for current in types::mro_of(type_object) {
        // SAFETY: every MRO entry is a live type object; `tp_dict` is either
        // NULL or a dictionary owned by that type, as in CPython.
        let dict = unsafe { (*current).tp_dict } as *mut ShimObject;
        if dict.is_null() {
            continue;
        }
        let Some(ShimKind::Dict(pairs)) = kind_ref(dict) else {
            continue;
        };
        for &(key, value) in pairs {
            if as_string(key as *mut ShimObject).as_deref() == Some(name) {
                return Some(value as *mut ShimObject);
            }
        }
    }
    None
}

/// `_PyType_Lookup`: borrow the descriptor for `name` from `type_object`'s MRO.
///
/// A value found in a type dictionary is returned as it is, borrowed. For an
/// attribute this host resolves through the type's tables (members, get-sets
/// and methods — the same order `generic_attribute` uses), a real descriptor
/// object is created whose `tp_descr_get`/`tp_descr_set` bind the way the
/// attribute protocol binds them. `NULL` without an error means the attribute
/// was not found, as in CPython.
#[no_mangle]
pub unsafe extern "C" fn _PyType_Lookup(
    type_object: *mut PyTypeObject,
    name: *mut ShimObject,
) -> *mut ShimObject {
    if type_object.is_null() || name.is_null() {
        set_error("SystemError: _PyType_Lookup: NULL argument");
        return ptr::null_mut();
    }
    let Some(name_text) = as_string(name) else {
        set_error("TypeError: attribute name must be string");
        return ptr::null_mut();
    };
    if !types::is_type_pointer(type_object as *mut ShimObject) {
        set_error("TypeError: _PyType_Lookup: argument is not a type");
        return ptr::null_mut();
    }
    if let Some(attribute) = type_dict_attribute(type_object, &name_text) {
        return attribute;
    }
    let key = (type_object as usize, name_text.clone());
    if let Some(cached) = descriptor_cache().get(&key).copied() {
        return cached as *mut ShimObject;
    }
    ensure_private_types();
    let description = if let Some(definition) = types::find_member(type_object, &name_text) {
        Some((
            ptr::addr_of_mut!(MEMBER_DESCRIPTOR_TYPE),
            definition as usize,
        ))
    } else if let Some(definition) = types::find_getset(type_object, &name_text) {
        Some((
            ptr::addr_of_mut!(GETSET_DESCRIPTOR_TYPE),
            definition as usize,
        ))
    } else {
        types::find_method(type_object, &name_text)
            .map(|spec| (ptr::addr_of_mut!(METHOD_DESCRIPTOR_TYPE), spec.definition))
    };
    let Some((descriptor_type, definition)) = description else {
        return ptr::null_mut();
    };
    let descriptor = new_descriptor(descriptor_type, definition);
    if descriptor.is_null() {
        // `new_descriptor` recorded the allocation failure.
        return ptr::null_mut();
    }
    descriptor_cache().insert(key, descriptor as usize);
    descriptor
}

// ---------------------------------------------------------------------------
// Garbage collector state and finalisation
// ---------------------------------------------------------------------------

/// Whether the garbage collector is enabled (`PyGC_Enable`/`PyGC_Disable`).
///
/// The host never collects handle cycles, so the flag records the caller's
/// request rather than changing behaviour.
static GC_ENABLED: AtomicBool = AtomicBool::new(true);

/// `PyGC_Enable`: enables the collector, returning the previous state.
#[no_mangle]
pub unsafe extern "C" fn PyGC_Enable() -> c_int {
    GC_ENABLED.swap(true, Ordering::SeqCst) as c_int
}

/// `PyGC_Disable`: disables the collector, returning the previous state.
#[no_mangle]
pub unsafe extern "C" fn PyGC_Disable() -> c_int {
    GC_ENABLED.swap(false, Ordering::SeqCst) as c_int
}

/// Addresses of objects whose finalizer has run.
///
/// See the module docs for why this is a side table and how it interacts with
/// the parent module's deallocator.
static FINALIZED: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

fn finalized_objects() -> MutexGuard<'static, HashSet<usize>> {
    FINALIZED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `PyObject_GC_IsFinalized`: whether the object's finalizer has run.
#[no_mangle]
pub unsafe extern "C" fn PyObject_GC_IsFinalized(object: *mut ShimObject) -> c_int {
    if object.is_null() {
        return 0;
    }
    finalized_objects().contains(&(object as usize)) as c_int
}

/// `PyObject_CallFinalizerFromDealloc`: run `tp_finalize` if it has not run.
///
/// CPython returns `-1` when the object was resurrected by its finalizer. This
/// host's deallocator runs `tp_finalize` itself, before `tp_dealloc` — and
/// `tp_dealloc` is the only caller of this entry point — so the finalizer has
/// already run when this arrives with a zero reference count. It is marked
/// finalised and `0` is returned without invoking it a second time, which
/// keeps `__del__` single-shot for the Cython `tp_dealloc` pattern that guards
/// the call with `PyObject_GC_IsFinalized`.
///
/// A non-zero reference count means the call did not come from a deallocator;
/// the finalizer is then run once under a temporary reference, like
/// `PyObject_CallFinalizer`.
#[no_mangle]
pub unsafe extern "C" fn PyObject_CallFinalizerFromDealloc(object: *mut ShimObject) -> c_int {
    if object.is_null() {
        set_error("SystemError: PyObject_CallFinalizerFromDealloc: object is NULL");
        return -1;
    }
    let type_object = type_of(object);
    if type_object.is_null() {
        set_error("SystemError: PyObject_CallFinalizerFromDealloc: object has no type");
        return -1;
    }
    // SAFETY: `type_object` is a live type object.
    let finalize = unsafe { (*type_object).tp_finalize };
    if finalize == 0 {
        return 0;
    }
    let already_finalized = {
        // The lock is released before the finalizer runs: extension code may
        // call back into `PyObject_GC_IsFinalized`.
        let mut finalized = finalized_objects();
        !finalized.insert(object as usize)
    };
    if already_finalized {
        // Never run the finalizer twice.
        return 0;
    }
    // SAFETY: every handle starts with the CPython header.
    let count = unsafe { (*object).head.ob_refcnt.load(Ordering::Relaxed) };
    if count <= 0 {
        // The deallocator already ran `tp_finalize` (see above).
        return 0;
    }
    let finalize: unsafe extern "C" fn(*mut ShimObject) =
        unsafe { std::mem::transmute(finalize as *const c_void) };
    inc_ref(object);
    let _gripper = GilGuard::new();
    // SAFETY: `tp_finalize` has CPython's `inquiry` shape and borrows the
    // object; the temporary reference keeps it alive across the call.
    unsafe { finalize(object) };
    dec_ref(object);
    0
}

/// `PyUnstable_Object_EnableDeferredRefcount`: opt an object into PEP 703
/// deferred reference counting.
///
/// The host's counts are always shared (atomic) counts and there is no
/// deferred-counting mode, so the request is ignored and `0` is returned — the
/// documented answer for a runtime that does not enable it. The function
/// cannot fail and sets no error.
#[no_mangle]
pub unsafe extern "C" fn PyUnstable_Object_EnableDeferredRefcount(
    _object: *mut ShimObject,
) -> c_int {
    0
}

// ---------------------------------------------------------------------------
// Weak references
// ---------------------------------------------------------------------------

/// `PyWeakref_NewRef`: create a weak reference to `object`.
///
/// Every type in this host has `tp_weaklistoffset == 0` and cannot request
/// `Py_TPFLAGS_MANAGED_WEAKREF` (a `PyType_Spec` requesting it is rejected), so
/// no object is weakrefable. CPython raises `TypeError` for exactly that case;
/// inventing a weak reference would hand the caller an object whose referent's
/// death this host cannot observe.
#[no_mangle]
pub unsafe extern "C" fn PyWeakref_NewRef(
    object: *mut ShimObject,
    callback: *mut ShimObject,
) -> *mut ShimObject {
    if object.is_null() {
        set_error("SystemError: PyWeakref_NewRef: object is NULL");
        return ptr::null_mut();
    }
    if !callback.is_null()
        && callback != ptr::addr_of_mut!(_Py_NoneStruct)
        && !object_callable(callback)
    {
        set_error("TypeError: weakref callback must be callable");
        return ptr::null_mut();
    }
    let type_object = type_of(object);
    if type_object.is_null() {
        set_error("TypeError: cannot create weak reference to a NULL-type object");
        return ptr::null_mut();
    }
    // SAFETY: live type object.
    let weaklist_offset = unsafe { (*type_object).tp_weaklistoffset };
    if weaklist_offset == 0 {
        set_error(format!(
            "TypeError: cannot create weak reference to '{}' object",
            types::type_name(type_object)
        ));
        return ptr::null_mut();
    }
    // A type that advertises a weak list would need the host to break the
    // reference when the referent dies; that hook does not exist here, so the
    // request is refused instead of returning a permanently-live reference.
    set_error("NotImplementedError: weak references are not supported by this Python host");
    ptr::null_mut()
}

/// `PyWeakref_GetRef`: retrieve the referent of a weak reference.
///
/// Returns `1` and a new reference when the referent is alive, `0` when it is
/// dead, and `-1` with an exception for anything that is not a weak
/// reference. No weak reference can exist in this host (above), so the only
/// reachable outcome for a real object is the `-1` diagnostic.
#[no_mangle]
pub unsafe extern "C" fn PyWeakref_GetRef(
    _reference: *mut ShimObject,
    object: *mut *mut ShimObject,
) -> c_int {
    if object.is_null() {
        set_error("SystemError: PyWeakref_GetRef: output pointer is NULL");
        return -1;
    }
    // SAFETY: the caller passes a writable `PyObject *`.
    unsafe { *object = ptr::null_mut() };
    // No object in this host is weakrefable (see `PyWeakref_NewRef`), so the
    // argument cannot name a weak reference whatever it points to.
    set_error("TypeError: PyWeakref_GetRef: argument is not a weak reference");
    -1
}

// ---------------------------------------------------------------------------
// Threads
// ---------------------------------------------------------------------------

/// A `PyThread_type_lock`.
///
/// Real locks are allocated so that a caller that hands one to
/// `PyThread_acquire_lock`/`PyThread_release_lock` in a host that exports them
/// gets mutual exclusion; this module only owns the allocate/free half of the
/// pair (the two symbols the pinned wheels reference).
struct ThreadLock {
    #[allow(dead_code)]
    mutex: Mutex<()>,
}

/// `PyThread_allocate_lock`.
#[no_mangle]
pub unsafe extern "C" fn PyThread_allocate_lock() -> *mut c_void {
    let lock = Box::new(ThreadLock {
        mutex: Mutex::new(()),
    });
    Box::into_raw(lock) as *mut c_void
}

/// `PyThread_free_lock`.
#[no_mangle]
pub unsafe extern "C" fn PyThread_free_lock(lock: *mut c_void) {
    if lock.is_null() {
        return;
    }
    // SAFETY: `lock` came from `PyThread_allocate_lock` and the caller gives up
    // ownership here, as CPython requires.
    drop(unsafe { Box::from_raw(lock as *mut ThreadLock) });
}

/// `PyThreadState_GetFrame`: the frame currently being executed by `tstate`.
///
/// Frames created by `PyFrame_New` are inert objects: nothing in this host
/// executes Python code, so no thread state has a current frame and `NULL` —
/// CPython's answer for "no frame" — is returned. The argument is accepted as
/// either a real token or `NULL`; it is not dereferenced.
#[no_mangle]
pub unsafe extern "C" fn PyThreadState_GetFrame(_state: *mut PyThreadState) -> *mut FrameObject {
    ptr::null_mut()
}

// ---------------------------------------------------------------------------
// Code objects
// ---------------------------------------------------------------------------

/// `CO_FAST_*` bits, one `co_localspluskinds` byte per locals-plus variable.
const CO_FAST_LOCAL: u8 = 0x20;
const CO_FAST_CELL: u8 = 0x40;
const CO_FAST_FREE: u8 = 0x80;

/// `CO_VARARGS`/`CO_VARKEYWORDS` from CPython 3.14's `code.h`.
const CO_VARARGS: c_int = 0x0004;
const CO_VARKEYWORDS: c_int = 0x0008;

/// `FRAME_SPECIALS_SIZE` for CPython 3.14 on x86_64:
/// `(sizeof(_PyInterpreterFrame) - 1) / sizeof(PyObject *)` = `(88 - 1) / 8`.
const FRAME_SPECIALS_SIZE: c_int = 10;

/// CPython 3.14's placeholder bytecode for `PyCode_NewEmpty`:
/// `RESUME RESUME_AT_FUNC_START LOAD_COMMON_CONSTANT CONSTANT_ASSERTIONERROR
/// RAISE_VARARGS 1`.
const CODE_NEW_EMPTY_BYTECODE: [u8; 6] = [128, 0, 81, 0, 104, 1];

/// CPython 3.14's location table for that placeholder: one entry covering
/// three code units, `PY_CODE_LOCATION_INFO_NO_COLUMNS`, line offset 0.
const CODE_NEW_EMPTY_LINETABLE: [u8; 2] = [0xea, 0];

/// `co_version` values start at one and never repeat, as in CPython.
static CODE_VERSION: AtomicU32 = AtomicU32::new(1);

/// What CPython's argument checks report besides a plain error message.
enum CodeCheckError {
    /// CPython answers with `PyErr_BadInternalCall()`.
    BadInternalCall,
    Message(&'static str),
}

/// CPython's `get_localsplus_counts`: plain locals, cells and frees.
fn localsplus_counts(kinds: &[u8]) -> (c_int, c_int, c_int) {
    let mut locals = 0;
    let mut cells = 0;
    let mut frees = 0;
    for &kind in kinds {
        if kind & CO_FAST_LOCAL != 0 {
            locals += 1;
            if kind & CO_FAST_CELL != 0 {
                cells += 1;
            }
        } else if kind & CO_FAST_CELL != 0 {
            cells += 1;
        } else if kind & CO_FAST_FREE != 0 {
            frees += 1;
        }
    }
    (locals, cells, frees)
}

/// True when `object` is a `tuple` (CPython's `PyTuple_Check`).
fn is_tuple(object: *mut ShimObject) -> bool {
    matches!(kind_ref(object), Some(ShimKind::Tuple(_)))
}

/// Reads one locals-plus name, which CPython requires to be a string.
fn localsplus_name(item: usize) -> Result<String, String> {
    as_string(item as *mut ShimObject)
        .ok_or_else(|| "TypeError: name tuples must contain only strings".to_string())
}

/// Builds `localsplusnames`/`localspluskinds` from the legacy varname tuples.
///
/// Mirrors CPython's `PyUnstable_Code_NewWithPosOnlyArgs`: every varname is a
/// `CO_FAST_LOCAL` slot, every freevar a `CO_FAST_FREE` slot, and a cellvar
/// that shadows an argument merges into that argument's slot with
/// `CO_FAST_CELL` added. Both returned objects are new references.
fn localsplus_from_tuples(
    varnames: *mut ShimObject,
    cellvars: *mut ShimObject,
    freevars: *mut ShimObject,
) -> Result<(*mut ShimObject, *mut ShimObject), String> {
    let varnames = tuple_slots(varnames)?;
    let cellvars = tuple_slots(cellvars)?;
    let freevars = tuple_slots(freevars)?;
    let mut names: Vec<usize> =
        Vec::with_capacity(varnames.len() + cellvars.len() + freevars.len());
    let mut kinds: Vec<u8> = Vec::with_capacity(names.capacity());
    let mut varname_texts: Vec<String> = Vec::with_capacity(varnames.len());
    for &item in varnames {
        let text = localsplus_name(item)?;
        inc_ref(item as *mut ShimObject);
        names.push(item);
        kinds.push(CO_FAST_LOCAL);
        varname_texts.push(text);
    }
    for &item in cellvars {
        let text = localsplus_name(item)?;
        if let Some(index) = varname_texts.iter().position(|existing| *existing == text) {
            // A cell that shadows an argument shares the argument's slot.
            kinds[index] |= CO_FAST_CELL;
            continue;
        }
        inc_ref(item as *mut ShimObject);
        names.push(item);
        kinds.push(CO_FAST_CELL);
    }
    for &item in freevars {
        localsplus_name(item)?;
        inc_ref(item as *mut ShimObject);
        names.push(item);
        kinds.push(CO_FAST_FREE);
    }
    let names_object = if names.is_empty() {
        empty_tuple()
    } else {
        new_object(ShimKind::Tuple(names))
    };
    Ok((names_object, new_bytes(kinds)))
}

/// Allocates and fills a code object; every object argument is borrowed and
/// retained with a new reference.
///
/// This is CPython's `_PyCode_New` for the fields this host can represent: the
/// bytecode itself is only retained (as `co_code` in the Rust tail) because
/// nothing executes it, but every count and pointer an extension may read is
/// computed exactly as CPython computes it.
#[allow(clippy::too_many_arguments)]
fn new_code_object(
    argcount: c_int,
    posonlyargcount: c_int,
    kwonlyargcount: c_int,
    flags: c_int,
    stacksize: c_int,
    firstlineno: c_int,
    code: *mut ShimObject,
    consts: *mut ShimObject,
    names: *mut ShimObject,
    localsplusnames: *mut ShimObject,
    localspluskinds: *mut ShimObject,
    filename: *mut ShimObject,
    name: *mut ShimObject,
    qualname: *mut ShimObject,
    linetable: *mut ShimObject,
    exceptiontable: *mut ShimObject,
) -> *mut ShimObject {
    ensure_private_types();
    let bytecode = match bytes_data(code) {
        Ok(bytecode) => bytecode,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let kinds = match bytes_data(localspluskinds) {
        Ok(kinds) => kinds,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let nlocalsplus = match c_int::try_from(kinds.len()) {
        Ok(count) => count,
        Err(_) => {
            set_error("SystemError: code: too many locals-plus variables");
            return ptr::null_mut();
        }
    };
    let (nlocals, ncellvars, nfreevars) = localsplus_counts(kinds);
    // CPython's `init_code`: a zero stack size becomes one, and the frame
    // estimate keeps room for the frame's special slots.
    let stacksize = if stacksize == 0 { 1 } else { stacksize };
    let code_object = types::allocate_instance(ptr::addr_of_mut!(CODE_TYPE), 0);
    if code_object.is_null() {
        return ptr::null_mut();
    }
    for child in [
        code,
        consts,
        names,
        localsplusnames,
        localspluskinds,
        filename,
        name,
        qualname,
        linetable,
        exceptiontable,
    ] {
        inc_ref(child);
    }
    // SAFETY: `code_object` is a freshly zeroed, owned `CodeObject`.
    unsafe {
        let storage = code_object as *mut CodeObject;
        // `Py_SIZE` of a code object is its number of code units.
        (*storage)
            .ob_size
            .store((bytecode.len() / 2) as isize, Ordering::Relaxed);
        (*storage).co_consts = consts;
        (*storage).co_names = names;
        (*storage).co_exceptiontable = exceptiontable;
        (*storage).co_flags = flags;
        (*storage).co_argcount = argcount;
        (*storage).co_posonlyargcount = posonlyargcount;
        (*storage).co_kwonlyargcount = kwonlyargcount;
        (*storage).co_stacksize = stacksize;
        (*storage).co_firstlineno = firstlineno;
        (*storage).co_nlocalsplus = nlocalsplus;
        (*storage).co_framesize = nlocalsplus
            .saturating_add(stacksize)
            .saturating_add(FRAME_SPECIALS_SIZE);
        (*storage).co_nlocals = nlocals;
        (*storage).co_ncellvars = ncellvars;
        (*storage).co_nfreevars = nfreevars;
        (*storage).co_version = CODE_VERSION.fetch_add(1, Ordering::Relaxed);
        (*storage).co_localsplusnames = localsplusnames;
        (*storage).co_localspluskinds = localspluskinds;
        (*storage).co_filename = filename;
        (*storage).co_name = name;
        (*storage).co_qualname = qualname;
        (*storage).co_linetable = linetable;
        (*storage).co_code = code;
    }
    code_object
}

/// `PyUnstable_Code_NewWithPosOnlyArgs`: build a code object.
///
/// The arguments and their validation follow CPython 3.14's
/// `PyUnstable_Code_NewWithPosOnlyArgs` exactly: the three name tuples are
/// merged into the code object's locals-plus tables, every object argument is
/// retained with a new reference (never stolen), and the same
/// `PyErr_BadInternalCall`, `OverflowError` and `ValueError` rejections are
/// reported. The code object is inert — this host executes no Python bytecode —
/// but its layout and derived counts are the ones CPython would compute, so
/// `PyFrame_New` and any extension inspecting the object see consistent data.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn PyUnstable_Code_NewWithPosOnlyArgs(
    argcount: c_int,
    posonlyargcount: c_int,
    kwonlyargcount: c_int,
    nlocals: c_int,
    stacksize: c_int,
    flags: c_int,
    code: *mut ShimObject,
    consts: *mut ShimObject,
    names: *mut ShimObject,
    varnames: *mut ShimObject,
    freevars: *mut ShimObject,
    cellvars: *mut ShimObject,
    filename: *mut ShimObject,
    name: *mut ShimObject,
    qualname: *mut ShimObject,
    firstlineno: c_int,
    linetable: *mut ShimObject,
    exceptiontable: *mut ShimObject,
) -> *mut CodeObject {
    // CPython rejects anything but tuples for the legacy name triples before
    // it looks at the rest.
    if !is_tuple(varnames) || !is_tuple(cellvars) || !is_tuple(freevars) {
        bad_internal_call();
        return ptr::null_mut();
    }
    let (localsplusnames, localspluskinds) =
        match localsplus_from_tuples(varnames, cellvars, freevars) {
            Ok(built) => built,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        };
    let checked = (|| -> Result<(), CodeCheckError> {
        // `_PyCode_Validate`'s argument checks.
        if argcount < posonlyargcount
            || posonlyargcount < 0
            || kwonlyargcount < 0
            || stacksize < 0
            || flags < 0
            || !matches!(kind_ref(code), Some(ShimKind::Bytes { .. }))
            || !is_tuple(consts)
            || !is_tuple(names)
            || !matches!(kind_ref(filename), Some(ShimKind::Str { .. }))
            || !matches!(kind_ref(name), Some(ShimKind::Str { .. }))
            || !matches!(kind_ref(qualname), Some(ShimKind::Str { .. }))
            || !matches!(kind_ref(linetable), Some(ShimKind::Bytes { .. }))
            || !matches!(kind_ref(exceptiontable), Some(ShimKind::Bytes { .. }))
        {
            return Err(CodeCheckError::BadInternalCall);
        }
        let bytecode = bytes_data(code).map_err(|_| CodeCheckError::BadInternalCall)?;
        if bytecode.len() > c_int::MAX as usize {
            return Err(CodeCheckError::Message(
                "OverflowError: code: co_code larger than INT_MAX",
            ));
        }
        if bytecode.len() % 2 != 0 || !(bytecode.as_ptr() as usize).is_multiple_of(2) {
            return Err(CodeCheckError::Message(
                "ValueError: code: co_code is malformed",
            ));
        }
        // `co_varnames` must cover the arguments.
        let kinds = bytes_data(localspluskinds).map_err(|_| CodeCheckError::BadInternalCall)?;
        let (plain_locals, _, _) = localsplus_counts(kinds);
        let nplainlocals = plain_locals
            .saturating_sub(argcount)
            .saturating_sub(kwonlyargcount)
            .saturating_sub(c_int::from(flags & CO_VARARGS != 0))
            .saturating_sub(c_int::from(flags & CO_VARKEYWORDS != 0));
        if nplainlocals < 0 {
            return Err(CodeCheckError::Message(
                "ValueError: code: co_varnames is too small",
            ));
        }
        let varname_count = match kind_ref(varnames) {
            Some(ShimKind::Tuple(items)) => items.len(),
            _ => 0,
        };
        if nlocals as isize != varname_count as isize {
            return Err(CodeCheckError::Message(
                "ValueError: code: co_nlocals != len(co_varnames)",
            ));
        }
        Ok(())
    })();
    if let Err(error) = checked {
        dec_ref(localsplusnames);
        dec_ref(localspluskinds);
        match error {
            CodeCheckError::BadInternalCall => bad_internal_call(),
            CodeCheckError::Message(message) => set_error(message),
        }
        return ptr::null_mut();
    }
    let code_object = new_code_object(
        argcount,
        posonlyargcount,
        kwonlyargcount,
        flags,
        stacksize,
        firstlineno,
        code,
        consts,
        names,
        localsplusnames,
        localspluskinds,
        filename,
        name,
        qualname,
        linetable,
        exceptiontable,
    );
    dec_ref(localsplusnames);
    dec_ref(localspluskinds);
    code_object as *mut CodeObject
}

/// `PyCode_NewEmpty`: a code object with no bytecode, for traceback frames.
///
/// This is the constructor Cython's traceback helper uses before `PyFrame_New`.
/// CPython 3.14 fills it with a six-byte assertion stub, a two-byte location
/// table, an empty constant/name table and stack size 1; the filename and
/// function name are decoded as UTF-8 from the caller's C strings and stored as
/// `co_filename`, `co_name` and `co_qualname`.
#[no_mangle]
pub unsafe extern "C" fn PyCode_NewEmpty(
    filename: *const c_char,
    funcname: *const c_char,
    firstlineno: c_int,
) -> *mut CodeObject {
    if filename.is_null() || funcname.is_null() {
        set_error("SystemError: PyCode_NewEmpty: NULL filename or function name");
        return ptr::null_mut();
    }
    let filename_object = new_str(read_c_string(filename));
    let funcname_object = new_str(read_c_string(funcname));
    // The empty tuple and empty bytes are immortal singletons in this host, so
    // they are passed without an extra reference.
    let empty_tuple_object = empty_tuple();
    let empty_bytes_object = new_bytes(Vec::new());
    let bytecode_object = new_bytes(CODE_NEW_EMPTY_BYTECODE.to_vec());
    let linetable_object = new_bytes(CODE_NEW_EMPTY_LINETABLE.to_vec());
    let code_object = new_code_object(
        0,
        0,
        0,
        0,
        1,
        firstlineno,
        bytecode_object,
        empty_tuple_object,
        empty_tuple_object,
        empty_tuple_object,
        empty_bytes_object,
        filename_object,
        funcname_object,
        funcname_object,
        linetable_object,
        empty_bytes_object,
    );
    dec_ref(filename_object);
    dec_ref(funcname_object);
    dec_ref(bytecode_object);
    dec_ref(linetable_object);
    code_object as *mut CodeObject
}

// ---------------------------------------------------------------------------
// Frames and tracebacks
// ---------------------------------------------------------------------------

/// `PyFrame_New`: build an inert frame object for `code`.
///
/// The frame owns references to its code, globals and locals. It is never
/// pushed on a frame stack and its `f_frame` stays `NULL`, because this host
/// has no interpreter frames; an extension that wants to inspect or annotate
/// the frame (Cython writes `f_lineno`) can do so safely.
///
/// `globals` must be a dictionary and `locals` may be `NULL` or a dictionary,
/// matching CPython's contract for the arguments it keeps.
#[no_mangle]
pub unsafe extern "C" fn PyFrame_New(
    state: *mut PyThreadState,
    code: *mut CodeObject,
    globals: *mut ShimObject,
    locals: *mut ShimObject,
) -> *mut FrameObject {
    if state.is_null() {
        set_error("SystemError: PyFrame_New: thread state is NULL");
        return ptr::null_mut();
    }
    if code.is_null() {
        set_error("SystemError: PyFrame_New: code is NULL");
        return ptr::null_mut();
    }
    ensure_private_types();
    let code_object = code as *mut ShimObject;
    if type_of(code_object) != ptr::addr_of_mut!(CODE_TYPE) {
        set_error("SystemError: PyFrame_New: code is not a code object");
        return ptr::null_mut();
    }
    if globals.is_null() || !matches!(kind_ref(globals), Some(ShimKind::Dict(_))) {
        set_error("SystemError: PyFrame_New: globals must be a dict");
        return ptr::null_mut();
    }
    if !locals.is_null() && !matches!(kind_ref(locals), Some(ShimKind::Dict(_))) {
        set_error("SystemError: PyFrame_New: locals must be a dict or NULL");
        return ptr::null_mut();
    }
    let frame = types::allocate_instance(ptr::addr_of_mut!(FRAME_TYPE), 0);
    if frame.is_null() {
        return ptr::null_mut();
    }
    inc_ref(code_object);
    inc_ref(globals);
    inc_ref(locals);
    // SAFETY: `frame` is a freshly zeroed, owned `FrameObject`.
    unsafe {
        let storage = frame as *mut FrameObject;
        (*storage).code = code;
        (*storage).globals = globals;
        (*storage).locals = locals;
        // CPython's `_PyFrame_New_NoTrack` default: per-line tracing on.
        (*storage).f_trace_lines = 1;
    }
    frame as *mut FrameObject
}

/// `PyTraceBack_Here`: add `frame` to the pending exception's traceback.
///
/// This host has no traceback object model at all: exceptions store a
/// `__traceback__` slot but `PyException_SetTraceback` only accepts `None`, and
/// `PyTraceBack_Print` accepts only `None`, because no frame ever executes.
/// There is therefore no chain to extend. The frame is still validated — a
/// caller that passes garbage gets an error — and `0` reports that nothing
/// could be attached. Failing here with `-1` would replace the pending
/// exception of the error path that called it with a spurious one.
#[no_mangle]
pub unsafe extern "C" fn PyTraceBack_Here(frame: *mut FrameObject) -> c_int {
    if frame.is_null() {
        set_error("SystemError: PyTraceBack_Here: frame is NULL");
        return -1;
    }
    let object = frame as *mut ShimObject;
    if type_of(object) != ptr::addr_of_mut!(FRAME_TYPE) {
        set_error("SystemError: PyTraceBack_Here: argument is not a frame object");
        return -1;
    }
    0
}

// ---------------------------------------------------------------------------
// Fatal errors
// ---------------------------------------------------------------------------

/// `_Py_FatalErrorFunc`: report a fatal error and abort the process.
///
/// The message is assembled from the two C strings (never from Python objects)
/// and written to standard error with `write(2)`, then the process aborts,
/// exactly as CPython's fatal-error handler does. `func` names the failing
/// entry point and is included when present.
#[no_mangle]
pub unsafe extern "C" fn _Py_FatalErrorFunc(func: *const c_char, message: *const c_char) -> ! {
    let mut text = String::from("Fatal Python error: ");
    text.push_str(&read_c_string(message));
    if !func.is_null() {
        text.push_str(" (in ");
        text.push_str(&read_c_string(func));
        text.push(')');
    }
    text.push('\n');
    // SAFETY: `write` to the standard-error descriptor cannot fault; the buffer
    // is valid for `text.len()` bytes and errors are deliberately ignored.
    unsafe {
        let buffer = text.as_ptr() as *const c_void;
        let mut written = 0usize;
        while written < text.len() {
            let result = libc::write(2, buffer.add(written), text.len() - written);
            if result <= 0 {
                break;
            }
            written += result as usize;
        }
    }
    libc::abort()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_attr_sees_type_attributes_and_suppresses_misses() {
        let type_object = ptr::addr_of_mut!(types::NONE_TYPE) as *mut ShimObject;
        let name = new_str("__name__".to_string());
        assert_eq!(unsafe { PyObject_HasAttr(type_object, name) }, 1);
        dec_ref(name);
        let absent = new_str("no_such_attribute".to_string());
        assert_eq!(unsafe { PyObject_HasAttr(type_object, absent) }, 0);
        dec_ref(absent);
        assert!(!error_pending());
    }

    #[test]
    fn type_lookup_reports_absent_attributes_without_an_error() {
        let type_object = ptr::addr_of_mut!(types::NONE_TYPE);
        let name = new_str("no_such_attribute".to_string());
        assert!(unsafe { _PyType_Lookup(type_object, name) }.is_null());
        assert!(!error_pending());
        dec_ref(name);
    }

    #[test]
    fn instance_dictionaries_are_absent() {
        let value = new_str("value".to_string());
        assert!(unsafe { _PyObject_GetDictPtr(value) }.is_null());
        dec_ref(value);
        assert!(unsafe { _PyObject_GetDictPtr(ptr::null_mut()) }.is_null());
    }

    #[test]
    fn managed_dictionaries_have_nothing_to_visit() {
        let value = new_str("value".to_string());
        assert_eq!(
            unsafe { PyObject_VisitManagedDict(value, None, ptr::null_mut()) },
            0
        );
        unsafe { PyObject_ClearManagedDict(value) };
        dec_ref(value);
        assert_eq!(
            unsafe { PyObject_VisitManagedDict(ptr::null_mut(), None, ptr::null_mut()) },
            -1
        );
        assert!(take_error().is_some());
    }

    #[test]
    fn call_method_obj_args_requires_the_attribute() {
        let value = new_str("value".to_string());
        let name = new_str("missing".to_string());
        let result =
            unsafe { PyObject_CallMethodObjArgs(value, name, ptr::null_mut::<ShimObject>()) };
        assert!(result.is_null());
        assert!(take_error().is_some());
        dec_ref(name);
        dec_ref(value);
    }

    #[test]
    fn weak_references_are_rejected_for_non_weakrefable_objects() {
        let value = new_str("value".to_string());
        assert!(unsafe { PyWeakref_NewRef(value, ptr::null_mut()) }.is_null());
        let error = take_error().unwrap_or_default();
        assert!(error.contains("cannot create weak reference"), "{error}");
        let mut referent: *mut ShimObject = ptr::null_mut();
        assert_eq!(unsafe { PyWeakref_GetRef(value, &mut referent) }, -1);
        assert!(referent.is_null());
        assert!(take_error().is_some());
        dec_ref(value);
    }

    #[test]
    fn gc_switch_reports_the_previous_state() {
        let _ = unsafe { PyGC_Enable() };
        assert_eq!(unsafe { PyGC_Disable() }, 1);
        assert_eq!(unsafe { PyGC_Disable() }, 0);
        assert_eq!(unsafe { PyGC_Enable() }, 0);
        assert_eq!(unsafe { PyGC_Enable() }, 1);
    }

    #[test]
    fn objects_without_a_finalizer_are_not_finalized() {
        let value = new_str("value".to_string());
        assert_eq!(unsafe { PyObject_CallFinalizerFromDealloc(value) }, 0);
        assert_eq!(unsafe { PyObject_GC_IsFinalized(value) }, 0);
        dec_ref(value);
    }

    #[test]
    fn deferred_refcount_requests_are_ignored() {
        let value = new_str("value".to_string());
        assert_eq!(
            unsafe { PyUnstable_Object_EnableDeferredRefcount(value) },
            0
        );
        dec_ref(value);
    }

    #[test]
    fn thread_locks_round_trip() {
        let lock = unsafe { PyThread_allocate_lock() };
        assert!(!lock.is_null());
        unsafe { PyThread_free_lock(lock) };
        unsafe { PyThread_free_lock(ptr::null_mut()) };
    }

    #[test]
    fn thread_state_has_no_current_frame() {
        assert!(unsafe { PyThreadState_GetFrame(ptr::null_mut()) }.is_null());
    }

    #[test]
    fn traceback_here_validates_its_frame() {
        assert_eq!(unsafe { PyTraceBack_Here(ptr::null_mut()) }, -1);
        assert!(take_error().is_some());
        let value = new_str("value".to_string());
        assert_eq!(unsafe { PyTraceBack_Here(value as *mut FrameObject) }, -1);
        assert!(take_error().is_some());
        dec_ref(value);
    }

    #[test]
    fn code_and_frame_objects_are_allocated_and_released() {
        let code = unsafe { PyCode_NewEmpty(c"demo.py".as_ptr(), c"demo".as_ptr(), 3) };
        assert!(!code.is_null());
        let globals = PyDict_New();
        assert!(!globals.is_null());
        let state = PyThreadState_Get();
        let frame = unsafe { PyFrame_New(state, code, globals, ptr::null_mut()) };
        assert!(!frame.is_null());
        assert_eq!(
            unsafe { PyFrame_New(state, code, ptr::null_mut(), ptr::null_mut()) },
            ptr::null_mut()
        );
        assert!(take_error().is_some());
        dec_ref(frame as *mut ShimObject);
        dec_ref(code as *mut ShimObject);
        dec_ref(globals);
    }

    #[test]
    fn code_construction_validates_its_arguments() {
        let empty_tuple = empty_tuple();
        let empty_bytes = new_bytes(Vec::new());
        let bytecode = new_bytes(CODE_NEW_EMPTY_BYTECODE.to_vec());
        let linetable = new_bytes(CODE_NEW_EMPTY_LINETABLE.to_vec());
        let filename = new_str("demo.py".to_string());
        let name = new_str("demo".to_string());
        let code = unsafe {
            PyUnstable_Code_NewWithPosOnlyArgs(
                0,
                0,
                0,
                0,
                1,
                0,
                bytecode,
                empty_tuple,
                empty_tuple,
                empty_tuple,
                empty_tuple,
                empty_tuple,
                filename,
                name,
                name,
                1,
                linetable,
                empty_bytes,
            )
        };
        assert!(!code.is_null());
        // SAFETY: `code` is a live code object.
        assert_eq!(unsafe { (*code).co_firstlineno }, 1);
        assert_eq!(unsafe { (*code).co_stacksize }, 1);
        assert_eq!(unsafe { (*code).co_nlocalsplus }, 0);
        assert_eq!(unsafe { (*code).co_nlocals }, 0);
        assert_eq!(
            unsafe { (*code).co_framesize },
            1 + FRAME_SPECIALS_SIZE,
            "empty code keeps one stack slot plus the frame specials"
        );
        assert_ne!(unsafe { (*code).co_version }, 0);
        // A cell that shadows a variable shares its locals-plus slot.
        let varnames = PyTuple_New(1);
        let cellvars = PyTuple_New(1);
        PyTuple_SetItem(varnames, 0, new_str("value".to_string()));
        PyTuple_SetItem(cellvars, 0, new_str("value".to_string()));
        let merged = unsafe {
            PyUnstable_Code_NewWithPosOnlyArgs(
                1,
                0,
                0,
                1,
                1,
                0,
                bytecode,
                empty_tuple,
                empty_tuple,
                varnames,
                empty_tuple,
                cellvars,
                filename,
                name,
                name,
                1,
                linetable,
                empty_bytes,
            )
        };
        assert!(!merged.is_null());
        // SAFETY: `merged` is a live code object.
        assert_eq!(unsafe { (*merged).co_nlocalsplus }, 1);
        assert_eq!(unsafe { (*merged).co_nlocals }, 1);
        assert_eq!(unsafe { (*merged).co_ncellvars }, 1);
        dec_ref(merged as *mut ShimObject);
        let rejected = unsafe {
            PyUnstable_Code_NewWithPosOnlyArgs(
                0,
                0,
                0,
                0,
                1,
                0,
                empty_tuple,
                empty_tuple,
                empty_tuple,
                empty_tuple,
                empty_tuple,
                empty_tuple,
                filename,
                name,
                name,
                1,
                empty_bytes,
                empty_bytes,
            )
        };
        assert!(rejected.is_null());
        assert!(take_error().is_some());
        dec_ref(varnames);
        dec_ref(cellvars);
        dec_ref(code as *mut ShimObject);
        dec_ref(filename);
        dec_ref(name);
    }
}
