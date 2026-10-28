//! Descriptor and exception entry points used by the pinned extension wheels.
//!
//! This module adds the CPython 3.14 C-API functions that the abi benchmark
//! reports as missing:
//!
//! - [`PyArg_ValidateKeywordArguments`] validates a keyword dictionary;
//! - [`PyClassMethod_New`] and [`PyStaticMethod_New`] wrap a callable;
//! - [`PyDescr_NewClassMethod`] builds a class-method descriptor for a
//!   `PyMethodDef`;
//! - [`PyErr_NewException`] creates a real exception class and links it into
//!   the host's exception hierarchy.
//!
//! Class-method and static-method objects are the same tagged payload the host
//! already uses for bound methods: the wrapped callable occupies the
//! `function` slot and the descriptor-owner slot stays empty. Their exported
//! types are real (`PyClassMethod_Type`, `PyStaticMethod_Type`,
//! `PyClassMethodDescr_Type`), so `Py_TYPE` and the host's attribute lookup
//! agree on what the object is. The three types get their `tp_getattro`,
//! `tp_repr` and `tp_descr_get` slots installed once, which is what makes
//! `__func__`, `__wrapped__`, `__get__`, `__name__`, `__qualname__` and
//! `__doc__` work through `PyObject_GetAttr*` and keeps the descriptor
//! protocol reachable through `PyType_GetSlot`.
//!
//! A class-method descriptor stores its owning type and its `PyMethodDef` (the
//! latter inside a private capsule handle) in the same method payload. It is
//! callable like CPython's `classmethod_descriptor` and binds callers to the
//! class on `__get__`.
//!
//! Every entry point follows CPython's ownership contract: constructors return
//! a new reference, attribute lookup returns a new reference, and failures set
//! a Python exception and return `NULL` (or 0/-1 where the API says so). No
//! path unwraps or panics on extension-supplied data.

#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(private_interfaces)]

use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::Once;

use super::types::{self, TypeObject as PyTypeObject};
use super::*;

/// `METH_METHOD` from CPython's `methodobject.h`: the implementation also
/// receives the class that defined the method.
const METH_METHOD: c_int = 0x0200;

// ---------------------------------------------------------------------------
// Keyword validation
// ---------------------------------------------------------------------------

/// `PyArg_ValidateKeywordArguments`.
///
/// `kwargs` must be a dictionary whose keys are all strings. A non-dictionary
/// argument is a bad internal call (the `SystemError` CPython raises), and a
/// non-string key raises `TypeError("keywords must be strings")`.
#[no_mangle]
pub unsafe extern "C" fn PyArg_ValidateKeywordArguments(kwargs: *mut ShimObject) -> c_int {
    let pairs = match kind_ref(kwargs) {
        Some(ShimKind::Dict(pairs)) => pairs,
        _ => {
            bad_internal_call();
            return 0;
        }
    };
    for &(key, _) in pairs {
        if key == 0 || !matches!(kind_ref(key as *mut ShimObject), Some(ShimKind::Str { .. })) {
            set_error("TypeError: keywords must be strings");
            return 0;
        }
    }
    1
}

// ---------------------------------------------------------------------------
// Type-slot hooks
// ---------------------------------------------------------------------------

/// Installs the descriptor hooks on the three exported descriptor types.
///
/// The host's builtin type table owns these types; their `tp_getattro`,
/// `tp_repr`, `tp_descr_get` and (for class-method descriptors) `tp_call` slots
/// are empty until the first object of the corresponding kind is created.
/// Installation is idempotent and happens before the object escapes, so a
/// `PyObject_GetAttr*` on the result always sees the richer protocol.
fn install_descriptor_hooks() {
    static HOOKS: Once = Once::new();
    HOOKS.call_once(|| {
        types::ensure_builtin_types();
        // SAFETY: the type table is process-lifetime storage, and the slots are
        // written once before any object of these types is visible.
        unsafe {
            let classmethod = ptr::addr_of_mut!(types::CLASSMETHOD_TYPE);
            (*classmethod).tp_getattro = descriptor_getattro as *const () as usize;
            (*classmethod).tp_repr = descriptor_repr as *const () as usize;
            (*classmethod).tp_descr_get = classmethod_descr_get as *const () as usize;

            let staticmethod = ptr::addr_of_mut!(types::STATICMETHOD_TYPE);
            (*staticmethod).tp_getattro = descriptor_getattro as *const () as usize;
            (*staticmethod).tp_repr = descriptor_repr as *const () as usize;
            (*staticmethod).tp_descr_get = staticmethod_descr_get as *const () as usize;

            let descriptor = ptr::addr_of_mut!(types::CLASSMETHOD_DESCR_TYPE);
            (*descriptor).tp_getattro = descriptor_getattro as *const () as usize;
            (*descriptor).tp_repr = descriptor_repr as *const () as usize;
            (*descriptor).tp_descr_get = classmethod_descr_get as *const () as usize;
            (*descriptor).tp_call = classmethod_descr_call as *const () as usize;
        }
    });
}

/// Names the exported descriptor type of `object`, if it is one of ours.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DescriptorKind {
    ClassMethod,
    StaticMethod,
    ClassMethodDescriptor,
}

fn descriptor_kind(object: *mut ShimObject) -> Option<DescriptorKind> {
    let type_object = type_of(object);
    if type_object.is_null() {
        return None;
    }
    // SAFETY: every type pointer returned by `type_of` points at a live type.
    if type_object == ptr::addr_of_mut!(types::CLASSMETHOD_TYPE) {
        Some(DescriptorKind::ClassMethod)
    } else if type_object == ptr::addr_of_mut!(types::STATICMETHOD_TYPE) {
        Some(DescriptorKind::StaticMethod)
    } else if type_object == ptr::addr_of_mut!(types::CLASSMETHOD_DESCR_TYPE) {
        Some(DescriptorKind::ClassMethodDescriptor)
    } else {
        None
    }
}

/// The wrapped callable of a class-method or static-method object.
fn wrapped_callable(object: *mut ShimObject) -> Option<*mut ShimObject> {
    match kind_ref(object) {
        Some(ShimKind::Method { function, .. }) if *function != 0 => {
            Some(*function as *mut ShimObject)
        }
        _ => None,
    }
}

/// The owning type and `PyMethodDef` of a class-method descriptor.
fn classmethod_descriptor_parts(
    object: *mut ShimObject,
) -> Option<(*mut PyTypeObject, *mut PyMethodDefMirror)> {
    let function = match kind_ref(object) {
        Some(ShimKind::Method {
            function,
            self_object,
        }) if *self_object != 0 && *function != 0 => *function as *mut ShimObject,
        _ => return None,
    };
    let method = match kind_ref(function) {
        Some(ShimKind::Capsule { ptr, .. }) if *ptr != 0 => *ptr as *mut PyMethodDefMirror,
        _ => return None,
    };
    let self_object = match kind_ref(object) {
        Some(ShimKind::Method { self_object, .. }) => *self_object,
        _ => return None,
    };
    Some((self_object as *mut PyTypeObject, method))
}

/// Short Python name of a `PyMethodDef`.
fn method_name(method: *mut PyMethodDefMirror) -> Option<String> {
    if method.is_null() {
        return None;
    }
    // SAFETY: the extension owns the live definition for its lifetime.
    let name = unsafe { (*method).ml_name };
    (!name.is_null()).then(|| read_c_string(name))
}

// ---------------------------------------------------------------------------
// Descriptor protocol
// ---------------------------------------------------------------------------

/// Builds the `__get__` callable for a descriptor object.
fn dunder_get(object: *mut ShimObject) -> *mut ShimObject {
    // SAFETY: the definition is a process-lifetime table and `object` is a new
    // reference owned by the returned function.
    PyCFunction_NewEx(ptr::addr_of_mut!(DUNDER_GET_DEF), object, ptr::null_mut())
}

/// `METH_VARARGS` entry point behind `descr.__get__(obj[, type])`.
unsafe extern "C" fn dunder_get_entry(
    object: *mut ShimObject,
    args: *mut ShimObject,
) -> *mut ShimObject {
    let argc = PyTuple_Size(args);
    if argc < 0 {
        return ptr::null_mut();
    }
    if !(1..=2).contains(&argc) {
        set_error(format!(
            "TypeError: descriptor '__get__' requires 1 or 2 arguments (got {argc})"
        ));
        return ptr::null_mut();
    }
    let instance = PyTuple_GetItem(args, 0);
    if instance.is_null() {
        return ptr::null_mut();
    }
    let owner = if argc == 2 {
        let owner = PyTuple_GetItem(args, 1);
        if owner.is_null() {
            return ptr::null_mut();
        }
        owner
    } else {
        ptr::null_mut()
    };
    bind_descriptor(object, instance, owner)
}

/// Shared `tp_descr_get` implementation for class-method objects.
unsafe extern "C" fn classmethod_descr_get(
    object: *mut ShimObject,
    instance: *mut ShimObject,
    owner: *mut ShimObject,
) -> *mut ShimObject {
    bind_descriptor(object, instance, owner)
}

/// Shared `tp_descr_get` implementation for static-method objects.
unsafe extern "C" fn staticmethod_descr_get(
    object: *mut ShimObject,
    _instance: *mut ShimObject,
    _owner: *mut ShimObject,
) -> *mut ShimObject {
    match wrapped_callable(object) {
        Some(callable) => {
            inc_ref(callable);
            callable
        }
        None => {
            set_error("RuntimeError: uninitialized staticmethod object");
            ptr::null_mut()
        }
    }
}

/// Dispatches `__get__` for any of the three descriptor objects.
///
/// `instance` is the object the descriptor was found on (or `None`), and
/// `owner` is the explicit type argument, possibly `NULL` when the caller
/// supplied only one argument.
fn bind_descriptor(
    object: *mut ShimObject,
    instance: *mut ShimObject,
    owner: *mut ShimObject,
) -> *mut ShimObject {
    match descriptor_kind(object) {
        Some(DescriptorKind::ClassMethod) => {
            let Some(callable) = wrapped_callable(object) else {
                set_error("RuntimeError: uninitialized classmethod object");
                return ptr::null_mut();
            };
            let owner = if !owner.is_null() {
                owner
            } else if !instance.is_null() {
                type_of(instance) as *mut ShimObject
            } else {
                set_error("TypeError: classmethod descriptor needs either an object or a type");
                return ptr::null_mut();
            };
            PyMethod_New(callable, owner)
        }
        Some(DescriptorKind::StaticMethod) => unsafe {
            staticmethod_descr_get(object, instance, owner)
        },
        Some(DescriptorKind::ClassMethodDescriptor) => {
            bind_classmethod_descriptor(object, instance, owner)
        }
        None => {
            set_error("SystemError: object is not a descriptor");
            ptr::null_mut()
        }
    }
}

/// Implements `classmethod_get` for a `PyClassMethodDescr_Type` object.
fn bind_classmethod_descriptor(
    object: *mut ShimObject,
    instance: *mut ShimObject,
    owner: *mut ShimObject,
) -> *mut ShimObject {
    let Some((defining_type, method)) = classmethod_descriptor_parts(object) else {
        set_error("SystemError: malformed classmethod descriptor");
        return ptr::null_mut();
    };
    let name = method_name(method).unwrap_or_else(|| "?".to_string());
    let defining_name = types::type_name(defining_type);

    let owner = if !owner.is_null() {
        owner
    } else if !instance.is_null() {
        type_of(instance) as *mut ShimObject
    } else {
        set_error(format!(
            "TypeError: descriptor '{name}' for type '{defining_name}' needs either an object \
             or a type"
        ));
        return ptr::null_mut();
    };

    if !types::is_type_pointer(owner) {
        set_error(format!(
            "TypeError: descriptor '{name}' for type '{defining_name}' needs a type, not a \
             '{}' as arg 2",
            object_type_name(owner)
        ));
        return ptr::null_mut();
    }
    let owner_type = owner as *mut PyTypeObject;
    if !types::is_subtype(owner_type, defining_type) {
        set_error(format!(
            "TypeError: descriptor '{name}' requires a subtype of '{defining_name}' but \
             received '{}'",
            types::type_name(owner_type)
        ));
        return ptr::null_mut();
    }
    // SAFETY: the descriptor's `PyMethodDef` belongs to the loaded extension.
    let flags = unsafe { (*method).ml_flags };
    let class = if flags & METH_METHOD != 0 {
        defining_type
    } else {
        ptr::null_mut()
    };
    // The defining class is only passed for `METH_METHOD`; `PyCMethod_New`
    // validates that pair and reports a `SystemError` otherwise.
    PyCMethod_New(method, owner, ptr::null_mut(), class)
}

/// `tp_call` for a class-method descriptor: `descr(cls, ...)` binds `cls` and
/// then calls the underlying method.
unsafe extern "C" fn classmethod_descr_call(
    object: *mut ShimObject,
    args: *mut ShimObject,
    kwargs: *mut ShimObject,
) -> *mut ShimObject {
    let argc = PyTuple_Size(args);
    if argc < 0 {
        return ptr::null_mut();
    }
    if argc < 1 {
        let name = classmethod_descriptor_parts(object)
            .and_then(|(_, method)| method_name(method))
            .unwrap_or_else(|| "?".to_string());
        set_error(format!(
            "TypeError: descriptor '{name}' of 'classmethod_descriptor' object needs an argument"
        ));
        return ptr::null_mut();
    }
    let first = PyTuple_GetItem(args, 0);
    if first.is_null() {
        return ptr::null_mut();
    }
    let bound = bind_classmethod_descriptor(object, ptr::null_mut(), first);
    if bound.is_null() {
        return ptr::null_mut();
    }
    let items = match tuple_slots(args) {
        Ok(items) => items,
        Err(error) => {
            dec_ref(bound);
            set_error(error);
            return ptr::null_mut();
        }
    };
    // SAFETY: `items` has at least one element (`argc >= 1`).
    let rest = unsafe { items.as_ptr().add(1) }.cast::<*mut ShimObject>();
    let result = PyObject_VectorcallDict(bound, rest, items.len() - 1, kwargs);
    dec_ref(bound);
    result
}

// ---------------------------------------------------------------------------
// Attribute and repr slots
// ---------------------------------------------------------------------------

/// `tp_getattro` for the three descriptor types.
unsafe extern "C" fn descriptor_getattro(
    object: *mut ShimObject,
    name: *mut ShimObject,
) -> *mut ShimObject {
    let Some(name) = as_string(name) else {
        set_error("TypeError: attribute name must be a string");
        return ptr::null_mut();
    };
    match descriptor_attribute(object, &name) {
        Ok(value) => value,
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

fn descriptor_attribute(object: *mut ShimObject, name: &str) -> Result<*mut ShimObject, String> {
    let kind = match descriptor_kind(object) {
        Some(kind) => kind,
        None => {
            return Err(format!(
                "SystemError: '{}' is not a supported descriptor",
                object_type_name(object)
            ));
        }
    };
    match (kind, name) {
        (DescriptorKind::ClassMethod | DescriptorKind::StaticMethod, "__func__")
        | (DescriptorKind::ClassMethod | DescriptorKind::StaticMethod, "__wrapped__") => {
            let callable = wrapped_callable(object).ok_or_else(|| {
                format!(
                    "RuntimeError: uninitialized {} object",
                    descriptor_type_name(kind)
                )
            })?;
            inc_ref(callable);
            Ok(callable)
        }
        (DescriptorKind::ClassMethod | DescriptorKind::StaticMethod, "__get__") => {
            Ok(dunder_get(object))
        }
        (DescriptorKind::ClassMethod | DescriptorKind::StaticMethod, "__isabstractmethod__") => {
            let callable = wrapped_callable(object).ok_or_else(|| {
                format!(
                    "RuntimeError: uninitialized {} object",
                    descriptor_type_name(kind)
                )
            })?;
            let is_abstract = PyObject_GetAttrString(callable, c"__isabstractmethod__".as_ptr());
            if is_abstract.is_null() {
                clear_error();
                Ok(value_object(ShimValue::Bool(false)))
            } else {
                let truth = object_truthy(is_abstract).unwrap_or(false);
                dec_ref(is_abstract);
                Ok(value_object(ShimValue::Bool(truth)))
            }
        }
        (DescriptorKind::ClassMethodDescriptor, "__get__") => Ok(dunder_get(object)),
        (DescriptorKind::ClassMethodDescriptor, "__name__") => {
            let (_, method) = classmethod_descriptor_parts(object)
                .ok_or_else(|| "SystemError: malformed classmethod descriptor".to_string())?;
            let name = method_name(method)
                .ok_or_else(|| "SystemError: classmethod descriptor has no name".to_string())?;
            Ok(new_str(name))
        }
        (DescriptorKind::ClassMethodDescriptor, "__qualname__") => {
            let (defining_type, method) = classmethod_descriptor_parts(object)
                .ok_or_else(|| "SystemError: malformed classmethod descriptor".to_string())?;
            let name = method_name(method)
                .ok_or_else(|| "SystemError: classmethod descriptor has no name".to_string())?;
            Ok(new_str(format!(
                "{}.{}",
                types::type_name(defining_type),
                name
            )))
        }
        (DescriptorKind::ClassMethodDescriptor, "__objclass__") => {
            let (defining_type, _) = classmethod_descriptor_parts(object)
                .ok_or_else(|| "SystemError: malformed classmethod descriptor".to_string())?;
            inc_ref(defining_type as *mut ShimObject);
            Ok(defining_type as *mut ShimObject)
        }
        (DescriptorKind::ClassMethodDescriptor, "__doc__") => {
            let (_, method) = classmethod_descriptor_parts(object)
                .ok_or_else(|| "SystemError: malformed classmethod descriptor".to_string())?;
            // SAFETY: the definition is owned by the loaded extension.
            let doc = unsafe { (*method).ml_doc };
            if doc.is_null() {
                Ok(value_object(ShimValue::None))
            } else {
                Ok(new_str(read_c_string(doc)))
            }
        }
        (DescriptorKind::ClassMethodDescriptor, "__text_signature__") => {
            Ok(value_object(ShimValue::None))
        }
        _ => Err(format!(
            "AttributeError: '{}' object has no attribute '{name}'",
            descriptor_type_name(kind)
        )),
    }
}

fn descriptor_type_name(kind: DescriptorKind) -> &'static str {
    match kind {
        DescriptorKind::ClassMethod => "classmethod",
        DescriptorKind::StaticMethod => "staticmethod",
        DescriptorKind::ClassMethodDescriptor => "classmethod_descriptor",
    }
}

/// `tp_repr` for the three descriptor types.
unsafe extern "C" fn descriptor_repr(object: *mut ShimObject) -> *mut ShimObject {
    match descriptor_kind(object) {
        Some(DescriptorKind::ClassMethod) | Some(DescriptorKind::StaticMethod) => {
            let Some(callable) = wrapped_callable(object) else {
                return new_str("<uninitialized descriptor>".to_string());
            };
            let representation = PyObject_Repr(callable);
            if representation.is_null() {
                return ptr::null_mut();
            }
            let text = as_string(representation);
            dec_ref(representation);
            let Some(text) = text else {
                set_error("SystemError: callable repr is not a string");
                return ptr::null_mut();
            };
            let kind = descriptor_type_name(descriptor_kind(object).unwrap());
            new_str(format!("<{kind}({text})>"))
        }
        Some(DescriptorKind::ClassMethodDescriptor) => {
            let parts = classmethod_descriptor_parts(object);
            let (name, owner) = match parts {
                Some((defining_type, method)) => (
                    method_name(method).unwrap_or_else(|| "?".to_string()),
                    types::type_name(defining_type),
                ),
                None => ("?".to_string(), "<unknown>".to_string()),
            };
            new_str(format!("<method '{name}' of '{owner}' objects>"))
        }
        None => new_str("<descriptor>".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------------

/// Builds the method payload shared by class methods and static methods.
fn new_wrapped_method(
    callable: *mut ShimObject,
    type_object: *mut PyTypeObject,
) -> *mut ShimObject {
    inc_ref(callable);
    let object = new_object(ShimKind::Method {
        function: callable as usize,
        self_object: 0,
    });
    // SAFETY: the fresh handle owns its header and the static type outlives it.
    unsafe {
        (*object).head.ob_type = type_object;
    }
    object
}

/// `PyClassMethod_New`.
///
/// Returns a new class-method object wrapping `callable`; the result is not the
/// same object as `callable`. CPython does not require `callable` to be
/// callable here (the Python-level constructor does that check), but a NULL
/// callable is an internal error.
#[no_mangle]
pub unsafe extern "C" fn PyClassMethod_New(callable: *mut ShimObject) -> *mut ShimObject {
    install_descriptor_hooks();
    if callable.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    // SAFETY: the static type object has process lifetime.
    let type_object = ptr::addr_of_mut!(types::CLASSMETHOD_TYPE);
    new_wrapped_method(callable, type_object)
}

/// `PyStaticMethod_New`.
#[no_mangle]
pub unsafe extern "C" fn PyStaticMethod_New(callable: *mut ShimObject) -> *mut ShimObject {
    install_descriptor_hooks();
    if callable.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    // SAFETY: the static type object has process lifetime.
    let type_object = ptr::addr_of_mut!(types::STATICMETHOD_TYPE);
    new_wrapped_method(callable, type_object)
}

/// `PyDescr_NewClassMethod`.
///
/// Creates a `classmethod_descriptor` for `method` on `type_object`. The
/// descriptor owns a reference to the type; the `PyMethodDef` is owned by the
/// extension and is referenced, not copied, exactly as CPython does.
#[no_mangle]
pub unsafe extern "C" fn PyDescr_NewClassMethod(
    type_object: *mut PyTypeObject,
    method: *mut PyMethodDefMirror,
) -> *mut ShimObject {
    install_descriptor_hooks();
    if type_object.is_null() || method.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    if !types::is_type_pointer(type_object as *mut ShimObject) {
        set_error("TypeError: PyDescr_NewClassMethod: argument 1 must be a type");
        return ptr::null_mut();
    }
    // SAFETY: the caller passed a live definition; reading `ml_name` is the one
    // field CPython itself reads here.
    if unsafe { (*method).ml_name }.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    // The method pointer is parked in a capsule handle so the method payload's
    // release path decrements both children (capsule and type) normally.
    let method_handle = new_object(ShimKind::Capsule {
        ptr: method as usize,
        name: None,
        context: 0,
        destructor: 0,
    });
    inc_ref(type_object as *mut ShimObject);
    let object = new_object(ShimKind::Method {
        function: method_handle as usize,
        self_object: type_object as usize,
    });
    // SAFETY: the fresh handle owns its header and the static type outlives it.
    unsafe {
        (*object).head.ob_type = ptr::addr_of_mut!(types::CLASSMETHOD_DESCR_TYPE);
    }
    object
}

// ---------------------------------------------------------------------------
// Exceptions
// ---------------------------------------------------------------------------

/// `PyErr_NewException`.
///
/// `base == NULL` selects `Exception`. `dict` may be `NULL` or a dictionary;
/// CPython inserts `__module__` into a caller-supplied dictionary when it is
/// missing, and this host does the same. The new class is allocated through the
/// same exception-type path as the builtin `PyExc_*` classes, so
/// `PyErr_GivenExceptionMatches`, `is_subtype` and the rest of the exception
/// machinery see it as a member of the hierarchy.
///
/// CPython also accepts a tuple of bases; this host's type system has a single
/// `tp_base`, so a tuple is accepted only when it contains exactly one type.
#[no_mangle]
pub unsafe extern "C" fn PyErr_NewException(
    name: *const c_char,
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
    if module.is_empty() || short.is_empty() {
        set_error("SystemError: PyErr_NewException: name must be module.class");
        return ptr::null_mut();
    }
    if !dict.is_null() && !matches!(kind_ref(dict), Some(ShimKind::Dict(_))) {
        set_error("TypeError: PyErr_NewException: dict must be a dict");
        return ptr::null_mut();
    }
    let base = match resolve_exception_base(base) {
        Ok(base) => base,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    if !dict.is_null() {
        let key = new_str("__module__".to_string());
        let existing = PyDict_GetItem(dict, key);
        if existing.is_null() {
            let value = new_str(module.to_string());
            let status = PyDict_SetItem(dict, key, value);
            dec_ref(value);
            dec_ref(key);
            if status != 0 {
                return ptr::null_mut();
            }
        } else {
            dec_ref(key);
        }
    }
    // `PyErr_NewExceptionWithDoc` performs the allocation, links the base and
    // copies `__doc__` from the dictionary, so both entry points share one
    // exception registry.
    PyErr_NewExceptionWithDoc(name, ptr::null(), base, dict)
}

/// Resolves the `base` argument of `PyErr_NewException`.
///
/// `NULL` means `Exception`; a single type is used as-is; a one-element tuple
/// is unwrapped. Anything else is rejected with the error CPython's `type()`
/// call would report.
fn resolve_exception_base(base: *mut ShimObject) -> Result<*mut ShimObject, String> {
    if base.is_null() {
        return Ok(unsafe { PyExc_Exception });
    }
    let Some(ShimKind::Tuple(items)) = kind_ref(base) else {
        if types::is_type_pointer(base) {
            return Ok(base);
        }
        return Err(format!(
            "TypeError: PyErr_NewException: base must be an exception class, not '{}'",
            object_type_name(base)
        ));
    };
    if items.is_empty() {
        return Err("TypeError: PyErr_NewException: base tuple must not be empty".to_string());
    }
    for &item in items {
        let item = item as *mut ShimObject;
        if item.is_null() || !types::is_type_pointer(item) {
            return Err(format!(
                "TypeError: PyErr_NewException: bases must be types, not '{}'",
                object_type_name(item)
            ));
        }
    }
    if items.len() != 1 {
        return Err(
            "TypeError: PyErr_NewException: multiple base classes are not supported".to_string(),
        );
    }
    Ok(items[0] as *mut ShimObject)
}

// ---------------------------------------------------------------------------
// Static definitions
// ---------------------------------------------------------------------------

/// `__get__` entry used by all three descriptor objects. The implementation is
/// shared; the receiver's type selects the binding rule.
static mut DUNDER_GET_DEF: PyMethodDefMirror = PyMethodDefMirror {
    ml_name: c"__get__".as_ptr(),
    ml_meth: dunder_get_entry as *const () as *mut c_void,
    // `METH_VARARGS`: `(self, args)`; keyword arguments are rejected by the
    // host's C-function dispatcher, as in CPython.
    ml_flags: 0x0001,
    ml_doc: ptr::null(),
};
