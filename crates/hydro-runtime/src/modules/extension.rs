//! Bridge to CPython limited-API (`abi3`) extension modules.
//!
//! The frontend resolves an extension `.so` to its absolute path and codegen
//! calls [`call`] / [`get`] with the shared-object path and the Python
//! attribute name. All CPython interaction lives in `crate::capi`; this module
//! only converts between tagged Hydro values and [`ShimValue`]s and allocates
//! first-class function values for callables.
//!
//! Only the `ShimValue` types cross the boundary: scalars, bytes, strings,
//! capsules, `NotImplemented`, lists/tuples/dicts/sets of them and opaque
//! [`ShimValue::Object`] handles. Containers are copied recursively in both
//! directions (depth-limited to [`MAX_CONVERSION_DEPTH`]); a cyclic Hydro
//! container raises a `TypeError` instead of recursing. Capsules cross as an
//! opaque pointer plus name, so passing one back to the extension returns the
//! original pointer.
//!
//! Extension-owned instances and type objects cross as `KIND_EXT_OBJECT`
//! values that own one reference to the original handle. They keep their
//! identity (`is`), extension `repr`/`==`/truth, attributes, methods and
//! constructors; passing one back yields the same pointer. This is what makes
//! real extension APIs (NumPy arrays, pydantic-core sentinels and types)
//! usable instead of rejected.
//!
//! The reverse direction crosses Hydro classes, instances, functions and
//! modules as rooted handles ([`capi::hydro_shim`]). The handle owns a
//! persistent GC root, so the collector keeps the Hydro value alive while the
//! extension holds it; `PyObject_GetAttr*`, `PyObject_Call*`,
//! `PyObject_Repr`/`Str`, truth, `len`, `hash` and the type checks dispatch to
//! Hydro, and passing the handle back reproduces the identical Hydro value.
//! Scalars and containers are still copied. A value that has no handle
//! representation raises instead of fabricating an object.
//!
//! All fallible paths report a pending exception and return the `None` dummy
//! value; no user input can panic.

use std::collections::HashMap;
use std::ffi::CString;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use crate::capi::{self, OwnedShimObject, ShimAttr, ShimValue, MAX_CONVERSION_DEPTH};
use crate::exc::{
    self, ARITHMETIC_ERROR, ASSERTION_ERROR, ATTRIBUTE_ERROR, BASE_EXCEPTION, BASE_EXCEPTION_GROUP,
    BLOCKING_IO_ERROR, BROKEN_PIPE_ERROR, BUFFER_ERROR, BYTES_WARNING, CHILD_PROCESS_ERROR,
    CONNECTION_ABORTED_ERROR, CONNECTION_ERROR, CONNECTION_REFUSED_ERROR, CONNECTION_RESET_ERROR,
    DEPRECATION_WARNING, EOF_ERROR, EXCEPTION, FILE_EXISTS_ERROR, FILE_NOT_FOUND_ERROR,
    FLOATING_POINT_ERROR, FUTURE_WARNING, GENERATOR_EXIT, IMPORT_ERROR, IMPORT_WARNING,
    INDENTATION_ERROR, INDEX_ERROR, INTERRUPTED_ERROR, IS_A_DIRECTORY_ERROR, KEYBOARD_INTERRUPT,
    KEY_ERROR, LOOKUP_ERROR, MEMORY_ERROR, MODULE_NOT_FOUND_ERROR, NAME_ERROR,
    NOT_A_DIRECTORY_ERROR, NOT_IMPLEMENTED_ERROR, OS_ERROR, OVERFLOW_ERROR,
    PENDING_DEPRECATION_WARNING, PERMISSION_ERROR, PROCESS_LOOKUP_ERROR, RECURSION_ERROR,
    REFERENCE_ERROR, RESOURCE_WARNING, RUNTIME_ERROR, RUNTIME_WARNING, STOP_ASYNC_ITERATION,
    STOP_ITERATION, SYNTAX_ERROR, SYNTAX_WARNING, SYSTEM_ERROR, SYSTEM_EXIT, TAB_ERROR,
    TIMEOUT_ERROR, TYPE_ERROR, UNICODE_DECODE_ERROR, UNICODE_ENCODE_ERROR, UNICODE_ERROR,
    UNICODE_TRANSLATE_ERROR, UNICODE_WARNING, USER_WARNING, VALUE_ERROR, WARNING,
    ZERO_DIVISION_ERROR,
};
use crate::value::{self, ExtFunctionState, NONE};

/// Maximum length accepted for an ABI name buffer (`isize::MAX` keeps the
/// slice representable).
const MAX_NAME_LEN: usize = isize::MAX as usize;

/// Maximum argument count whose byte length still fits in `isize`.
const MAX_ARGS: usize = MAX_NAME_LEN / std::mem::size_of::<i64>();

/// Borrows a UTF-8 name from the ABI buffer without allocating. `NULL`, empty,
/// over-long and non-UTF-8 buffers are rejected.
fn name_str<'a>(ptr: *const u8, len: usize) -> Option<&'a str> {
    if ptr.is_null() || len == 0 || len > MAX_NAME_LEN {
        return None;
    }
    // SAFETY: the ABI contract requires `ptr..ptr+len` to be readable; the
    // length check above keeps the slice representable.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    std::str::from_utf8(bytes).ok()
}

/// Borrows the tagged argument buffer after validating it.
fn argument_slice<'a>(argc: usize, argv: *const i64) -> Result<&'a [i64], &'static str> {
    if argc == 0 {
        return Ok(&[]);
    }
    if argv.is_null() {
        return Err("extension call received arguments without an argument buffer");
    }
    if argc > MAX_ARGS {
        return Err("extension call has too many arguments");
    }
    // SAFETY: the ABI contract requires `argv` to point at `argc` readable
    // tagged values; the count check above keeps the slice representable.
    Ok(unsafe { std::slice::from_raw_parts(argv, argc) })
}

/// Derives the CPython initialisation module name from a shared-object path.
///
/// CPython names the entry point `PyInit_<name>`, where `<name>` is the first
/// component of the file stem. Both `foo.abi3.so` and
/// `foo.cpython-314-x86_64-linux-gnu.so` therefore yield `foo`.
pub(crate) fn init_name(path: &str) -> String {
    let stem = Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(path);
    match stem.split('.').next() {
        Some(first) if !first.is_empty() => first.to_string(),
        _ => stem.to_string(),
    }
}

/// Converts one tagged Hydro value to a shim value.
///
/// Supported types are `None`, `bool`, `int`, `float`, `str`, `bytes`,
/// `list`, `tuple`, `dict`, capsules, opaque extension handles and Hydro
/// classes, instances, functions and modules. Containers are copied
/// recursively; nesting is bounded by [`MAX_CONVERSION_DEPTH`] and a cycle
/// raises a `TypeError`. A handle is passed through with an additional
/// reference, preserving its identity. A Hydro class/instance/function/module
/// crosses as a rooted handle ([`capi::hydro_shim`]) that dispatches
/// attribute access, calls, `repr`/`str`, truth and comparison back to Hydro.
pub(crate) fn value_to_shim(value_arg: i64) -> Result<ShimValue, String> {
    value_to_shim_at(value_arg, 0, &mut Vec::new())
}

/// Recursive body of [`value_to_shim`].
///
/// `depth` counts enclosing containers; `path` holds the tagged values of the
/// containers on the current path so a self-referential Hydro list/dict is
/// reported instead of recursing until the stack overflows. It stays empty
/// (and therefore allocation-free) for scalar arguments.
fn value_to_shim_at(
    value_arg: i64,
    depth: usize,
    path: &mut Vec<i64>,
) -> Result<ShimValue, String> {
    if value_arg == NONE {
        return Ok(ShimValue::None);
    }
    if value_arg == value::TRUE {
        return Ok(ShimValue::Bool(true));
    }
    if value_arg == value::FALSE {
        return Ok(ShimValue::Bool(false));
    }
    if value_arg == value::NOT_IMPLEMENTED {
        return Ok(ShimValue::NotImplemented);
    }
    if value::is_int(value_arg) {
        return Ok(ShimValue::Int(value_arg >> 3));
    }
    if let Some(f) = value::as_float(value_arg) {
        return Ok(ShimValue::Float(f));
    }
    if let Some(s) = value::as_str(value_arg) {
        return Ok(ShimValue::Str(s.to_string()));
    }
    if let Some(bytes) = value::as_bytes(value_arg) {
        return Ok(ShimValue::Bytes(bytes.to_vec()));
    }
    if let Some(state) = value::as_ext_object(value_arg) {
        // Pass the original handle back with an additional reference: the
        // contract value owns that reference until it is consumed.
        return Ok(ShimValue::Object {
            object: OwnedShimObject::new(state.object),
            type_name: state.type_name.clone(),
        });
    }
    if let Some((ptr, name)) = value::as_capsule(value_arg) {
        return Ok(ShimValue::Capsule {
            ptr,
            name: name.map(str::to_string),
        });
    }
    if let Some(items) = value::as_list(value_arg) {
        return convert_sequence(value_arg, "list", depth, path, items, ShimValue::List);
    }
    if let Some(items) = value::as_tuple(value_arg) {
        return convert_sequence(value_arg, "tuple", depth, path, items, ShimValue::Tuple);
    }
    if let Some(pairs) = value::as_dict(value_arg) {
        begin_value_container(value_arg, "dict", depth, path)?;
        let mut converted = Vec::with_capacity(pairs.len());
        for &(key, item) in pairs {
            let key = value_to_shim_at(key, depth + 1, path)?;
            let item = value_to_shim_at(item, depth + 1, path)?;
            converted.push((key, item));
        }
        path.pop();
        return Ok(ShimValue::Dict(converted));
    }
    if let Some(items) = value::as_set(value_arg) {
        return convert_sequence(value_arg, "set", depth, path, items, ShimValue::Set);
    }
    // Classes, instances, functions and modules have no copied shim
    // representation. They cross as a rooted handle instead, so the extension
    // can inspect, call and store them and pass them back with their identity.
    if hydro_handle_supported(value_arg) {
        return Ok(capi::hydro_shim(value_arg));
    }
    Err(format!(
        "cannot pass '{}' object to a C extension function",
        value::type_name(value_arg)
    ))
}

/// Whether a Hydro value is representable as a rooted C handle.
///
/// Scalars and containers are copied by the arms above; the values listed
/// here have no copy representation and need a live handle so the extension
/// can observe their real identity, attributes and call semantics.
pub(crate) fn hydro_handle_supported(value_arg: i64) -> bool {
    value::as_class(value_arg).is_some()
        || value::as_instance(value_arg).is_some()
        || value::as_exc_instance(value_arg).is_some()
        || value::as_function(value_arg).is_some()
        || value::as_ext_function(value_arg).is_some()
        || value::as_module(value_arg).is_some()
        || value::as_namespace(value_arg).is_some()
}

/// Converts one Hydro sequence (list or tuple) into its shim counterpart.
fn convert_sequence(
    value_arg: i64,
    kind: &str,
    depth: usize,
    path: &mut Vec<i64>,
    items: &[i64],
    wrap: fn(Vec<ShimValue>) -> ShimValue,
) -> Result<ShimValue, String> {
    begin_value_container(value_arg, kind, depth, path)?;
    let mut converted = Vec::with_capacity(items.len());
    for &item in items {
        converted.push(value_to_shim_at(item, depth + 1, path)?);
    }
    path.pop();
    Ok(wrap(converted))
}

/// Checks depth and cycle state before descending into a Hydro container.
fn begin_value_container(
    value_arg: i64,
    kind: &str,
    depth: usize,
    path: &mut Vec<i64>,
) -> Result<(), String> {
    if depth >= MAX_CONVERSION_DEPTH {
        return Err(format!(
            "cannot pass a '{kind}' object nested more than {MAX_CONVERSION_DEPTH} levels \
             deep to a C extension function"
        ));
    }
    if path.contains(&value_arg) {
        return Err(format!(
            "cannot pass a cyclic '{kind}' object to a C extension function"
        ));
    }
    path.push(value_arg);
    Ok(())
}

/// Converts one shim value back to a tagged Hydro value.
///
/// Containers become freshly allocated Hydro lists/tuples/dicts; nothing is
/// shared with the extension-side object.
pub(crate) fn shim_to_value(shim: ShimValue) -> Result<i64, String> {
    shim_to_value_at(shim, 0)
}

/// Recursive body of [`shim_to_value`], bounded like the inbound direction.
fn shim_to_value_at(shim: ShimValue, depth: usize) -> Result<i64, String> {
    Ok(match shim {
        ShimValue::None => NONE,
        ShimValue::Bool(b) => value::bool_value(b),
        ShimValue::Int(n) => value::encode_int(n).ok_or_else(|| {
            format!("integer result {n} is outside the supported Hydro int range")
        })?,
        ShimValue::Float(f) => value::float_new(f),
        ShimValue::Str(s) => value::str_alloc(s),
        ShimValue::Bytes(bytes) => value::bytes_new(bytes),
        ShimValue::Capsule { ptr, name } => value::capsule_new(ptr, name),
        // Transfer the owned reference into the Hydro handle; the payload
        // releases it when the collector drops the object.
        ShimValue::Object { object, type_name } => {
            value::ext_object_new(object.into_raw(), type_name)
        }
        // The handle names the original Hydro value, so the same class,
        // instance, function or module comes back with its identity intact.
        // Its persistent root is released when the handle's last owner drops
        // (the caller roots the returned tagged value like any other result).
        ShimValue::Hydro(handle) => handle.value(),
        ShimValue::List(items) => {
            let items = shim_items_to_values(items, "list", depth)?;
            value::list_new(items)
        }
        ShimValue::Tuple(items) => {
            let items = shim_items_to_values(items, "tuple", depth)?;
            value::tuple_new(items)
        }
        ShimValue::Dict(pairs) => {
            if depth >= MAX_CONVERSION_DEPTH {
                return Err(format!(
                    "extension returned a 'dict' nested more than {MAX_CONVERSION_DEPTH} \
                     levels deep"
                ));
            }
            let mut converted = Vec::with_capacity(pairs.len());
            for (key, item) in pairs {
                let key = shim_to_value_at(key, depth + 1)?;
                let item = shim_to_value_at(item, depth + 1)?;
                converted.push((key, item));
            }
            value::dict_new(converted)
        }
        ShimValue::Set(items) => {
            let items = shim_items_to_values(items, "set", depth)?;
            // Reuse the set helpers so an item Hydro cannot hash raises
            // `TypeError` (a C set may hold a handle whose Hydro value is
            // unhashable) instead of corrupting the set invariant.
            let set = value::set_new(Vec::new());
            let roots = crate::heap::root_values(&[set]);
            for item in items {
                crate::container::set_add(set, item);
                if exc::pending() {
                    crate::heap::release_roots(roots);
                    return Err(capi::take_hydro_error(
                        "cannot convert an extension set item",
                    ));
                }
            }
            crate::heap::release_roots(roots);
            set
        }
        ShimValue::NotImplemented => value::NOT_IMPLEMENTED,
        ShimValue::Ellipsis => {
            return Err(
                "extension returned the Ellipsis singleton, which has no Hydro value".to_string(),
            )
        }
    })
}

/// Converts a shim sequence's items, applying the shared depth bound.
fn shim_items_to_values(
    items: Vec<ShimValue>,
    kind: &str,
    depth: usize,
) -> Result<Vec<i64>, String> {
    if depth >= MAX_CONVERSION_DEPTH {
        return Err(format!(
            "extension returned a '{kind}' nested more than {MAX_CONVERSION_DEPTH} levels deep"
        ));
    }
    let mut converted = Vec::with_capacity(items.len());
    for item in items {
        converted.push(shim_to_value_at(item, depth + 1)?);
    }
    Ok(converted)
}

/// Python type name of a shim value, used in the not-callable error.
fn shim_type_name(shim: &ShimValue) -> String {
    match shim {
        ShimValue::None => "NoneType".to_string(),
        ShimValue::Bool(_) => "bool".to_string(),
        ShimValue::Int(_) => "int".to_string(),
        ShimValue::Float(_) => "float".to_string(),
        ShimValue::Str(_) => "str".to_string(),
        ShimValue::Bytes(_) => "bytes".to_string(),
        ShimValue::List(_) => "list".to_string(),
        ShimValue::Tuple(_) => "tuple".to_string(),
        ShimValue::Dict(_) => "dict".to_string(),
        ShimValue::Set(_) => "set".to_string(),
        ShimValue::Capsule { .. } => "PyCapsule".to_string(),
        ShimValue::Object { type_name, .. } => type_name.clone(),
        ShimValue::Hydro(handle) => handle.type_name().to_string(),
        ShimValue::NotImplemented => "NotImplementedType".to_string(),
        ShimValue::Ellipsis => "ellipsis".to_string(),
    }
}

/// Raises "'X' object is not callable" for a non-callable attribute.
fn not_callable(shim: &ShimValue) -> i64 {
    exc::raise_with_message(
        TYPE_ERROR,
        &format!("'{}' object is not callable", shim_type_name(shim)),
    )
}

/// Selects the exception type for a message returned by the shim.
///
/// The shim reports its own failure category in the text. The error setters in
/// `capi` emit exactly `"<Type>: <message>"` for the exported `PyExc_*`
/// singletons, so the prefix selects the matching Hydro tag; a message without
/// a known prefix is an internal load/call failure, which falls back to
/// `RuntimeError` (with a few historical heuristics for the loader's own
/// diagnostics).
pub(crate) fn raised_error(message: &str) -> i64 {
    let (tag, text) = match message.split_once(": ") {
        Some((name, rest)) => match exception_tag(name) {
            Some(tag) => (tag, rest),
            None => fallback_error(message),
        },
        None => fallback_error(message),
    };
    // An exception instance raised by the extension is preserved as the
    // payload: the frontend approximates its imported class with
    // `RuntimeError`, while `str`/`repr`/attributes/methods delegate to the
    // original object.
    if let Some(exception) = capi::take_error_exception(message) {
        let object = capi::exception_object_to_hydro(exception);
        return exc::raise_extension(object, exc::RUNTIME_ERROR, text);
    }
    exc::raise_with_message(tag, text)
}

/// Maps a CPython exception class name to its Hydro tag.
///
/// The names are the ones the exported `PyExc_*` symbols carry, plus the
/// classes the shim itself names in its own diagnostics. Aliases without a
/// dedicated Hydro tag resolve to the class they alias (`IOError`,
/// `EnvironmentError` and `WindowsError` are `OSError`); subclasses without a
/// tag resolve to their nearest tagged ancestor (`UnboundLocalError` is a
/// `NameError`, `EncodingWarning` is a `Warning`).
pub(crate) fn exception_tag(name: &str) -> Option<u32> {
    Some(match name {
        "ArithmeticError" => ARITHMETIC_ERROR,
        "AssertionError" => ASSERTION_ERROR,
        "AttributeError" => ATTRIBUTE_ERROR,
        "BaseException" => BASE_EXCEPTION,
        "BaseExceptionGroup" => BASE_EXCEPTION_GROUP,
        "BlockingIOError" => BLOCKING_IO_ERROR,
        "BrokenPipeError" => BROKEN_PIPE_ERROR,
        "BufferError" => BUFFER_ERROR,
        "BytesWarning" => BYTES_WARNING,
        "ChildProcessError" => CHILD_PROCESS_ERROR,
        "ConnectionAbortedError" => CONNECTION_ABORTED_ERROR,
        "ConnectionError" => CONNECTION_ERROR,
        "ConnectionRefusedError" => CONNECTION_REFUSED_ERROR,
        "ConnectionResetError" => CONNECTION_RESET_ERROR,
        "DeprecationWarning" => DEPRECATION_WARNING,
        // `EncodingWarning` subclasses `Warning`; it has no tag of its own.
        "EncodingWarning" => WARNING,
        "EnvironmentError" | "IOError" | "WindowsError" | "OSError" => OS_ERROR,
        "EOFError" => EOF_ERROR,
        "Exception" => EXCEPTION,
        "FileExistsError" => FILE_EXISTS_ERROR,
        "FileNotFoundError" => FILE_NOT_FOUND_ERROR,
        "FloatingPointError" => FLOATING_POINT_ERROR,
        "FutureWarning" => FUTURE_WARNING,
        "GeneratorExit" => GENERATOR_EXIT,
        "ImportError" => IMPORT_ERROR,
        "ImportWarning" => IMPORT_WARNING,
        "IndentationError" => INDENTATION_ERROR,
        "IndexError" => INDEX_ERROR,
        "InterruptedError" => INTERRUPTED_ERROR,
        "IsADirectoryError" => IS_A_DIRECTORY_ERROR,
        "KeyboardInterrupt" => KEYBOARD_INTERRUPT,
        "KeyError" => KEY_ERROR,
        "LookupError" => LOOKUP_ERROR,
        "MemoryError" => MEMORY_ERROR,
        "ModuleNotFoundError" => MODULE_NOT_FOUND_ERROR,
        "NameError" | "UnboundLocalError" => NAME_ERROR,
        "NotADirectoryError" => NOT_A_DIRECTORY_ERROR,
        "NotImplementedError" => NOT_IMPLEMENTED_ERROR,
        "OverflowError" => OVERFLOW_ERROR,
        "PendingDeprecationWarning" => PENDING_DEPRECATION_WARNING,
        "PermissionError" => PERMISSION_ERROR,
        "ProcessLookupError" => PROCESS_LOOKUP_ERROR,
        "RecursionError" => RECURSION_ERROR,
        "ReferenceError" => REFERENCE_ERROR,
        "ResourceWarning" => RESOURCE_WARNING,
        "RuntimeError" => RUNTIME_ERROR,
        "RuntimeWarning" => RUNTIME_WARNING,
        "StopAsyncIteration" => STOP_ASYNC_ITERATION,
        "StopIteration" => STOP_ITERATION,
        "SyntaxError" => SYNTAX_ERROR,
        "SyntaxWarning" => SYNTAX_WARNING,
        "SystemError" => SYSTEM_ERROR,
        "SystemExit" => SYSTEM_EXIT,
        "TabError" => TAB_ERROR,
        "TimeoutError" => TIMEOUT_ERROR,
        "TypeError" => TYPE_ERROR,
        "UnicodeDecodeError" => UNICODE_DECODE_ERROR,
        "UnicodeEncodeError" => UNICODE_ENCODE_ERROR,
        "UnicodeError" => UNICODE_ERROR,
        "UnicodeTranslateError" => UNICODE_TRANSLATE_ERROR,
        "UnicodeWarning" => UNICODE_WARNING,
        "UserWarning" => USER_WARNING,
        "ValueError" => VALUE_ERROR,
        "Warning" => WARNING,
        "ZeroDivisionError" => ZERO_DIVISION_ERROR,
        _ => return None,
    })
}

/// Classifies a shim message that carries no recognisable exception prefix.
///
/// These are the loader's own diagnostics (`dlopen` failures, missing
/// attributes, arity checks), so the historical text heuristics decide between
/// `AttributeError`, `TypeError` and `RuntimeError`.
fn fallback_error(message: &str) -> (u32, &str) {
    if message.contains("has no attribute")
        || message.contains("attribute not found")
        || message.contains("AttributeError")
    {
        (ATTRIBUTE_ERROR, message)
    } else if message.contains("TypeError")
        || message.contains("not callable")
        || message.contains("takes ")
        || message.contains("cannot pass")
        || message.contains("cannot set")
    {
        (TYPE_ERROR, message)
    } else {
        (RUNTIME_ERROR, message)
    }
}

/// Converts a successful shim result, reporting conversion failures.
fn result_value(shim: ShimValue) -> i64 {
    match shim_to_value(shim) {
        Ok(value) => value,
        Err(message) => exc::raise_with_message(RUNTIME_ERROR, &message),
    }
}

/// Converts the tagged argument buffer, reporting the first unsupported
/// argument as a `TypeError`.
fn shim_arguments(argv: &[i64]) -> Result<Vec<ShimValue>, i64> {
    let mut args = Vec::with_capacity(argv.len());
    for &arg in argv {
        match value_to_shim(arg) {
            Ok(shim) => args.push(shim),
            Err(message) => return Err(exc::raise_with_message(TYPE_ERROR, &message)),
        }
    }
    Ok(args)
}

/// `extension.call(path, member, args...)`: calls a member of a loaded
/// extension module.
pub(crate) fn call(
    path_ptr: *const u8,
    path_len: usize,
    member_ptr: *const u8,
    member_len: usize,
    argc: usize,
    argv: *const i64,
) -> i64 {
    let Some(path) = name_str(path_ptr, path_len) else {
        return exc::raise_with_message(RUNTIME_ERROR, "invalid extension path buffer");
    };
    let Some(member) = name_str(member_ptr, member_len) else {
        return exc::raise_with_message(RUNTIME_ERROR, "invalid extension attribute buffer");
    };
    if crate::native::is_hna_path(path) {
        return crate::native::call_member(path, member, argc, argv);
    }
    let argv = match argument_slice(argc, argv) {
        Ok(args) => args,
        Err(message) => return exc::raise_with_message(TYPE_ERROR, message),
    };
    let module = match capi::load_extension(path, &init_name(path)) {
        Ok(module) => module,
        Err(message) => return raised_error(&message),
    };
    let attribute = match capi::get_attr(module, member) {
        Ok(attribute) => attribute,
        Err(message) => return raised_error(&message),
    };
    match attribute {
        ShimAttr::Callable(callable) => {
            match capi::call_method_hydro_args(callable.module, callable.method, argv) {
                Ok(value) => value,
                Err(message) => raised_error(&message),
            }
        }
        ShimAttr::Type(address) => {
            let args = match shim_arguments(argv) {
                Ok(args) => args,
                Err(value) => return value,
            };
            // Construction goes through the spec's `Py_tp_new`; a type without
            // one reports a `TypeError` from `capi`.
            match capi::call_type_object_hydro(address, &args) {
                Ok(value) => value,
                Err(message) => raised_error(&message),
            }
        }
        ShimAttr::Value(shim) => not_callable(&shim),
    }
}

/// `extension.get(path, member)`: materialises a module attribute as a Hydro
/// value or a callable function object.
pub(crate) fn get(
    path_ptr: *const u8,
    path_len: usize,
    member_ptr: *const u8,
    member_len: usize,
) -> i64 {
    let Some(path) = name_str(path_ptr, path_len) else {
        return exc::raise_with_message(RUNTIME_ERROR, "invalid extension path buffer");
    };
    let Some(member) = name_str(member_ptr, member_len) else {
        return exc::raise_with_message(RUNTIME_ERROR, "invalid extension attribute buffer");
    };
    if crate::native::is_hna_path(path) {
        return crate::native::get(path, member);
    }
    let module = match capi::load_extension(path, &init_name(path)) {
        Ok(module) => module,
        Err(message) => return raised_error(&message),
    };
    match capi::get_attr(module, member) {
        Ok(ShimAttr::Value(shim)) => result_value(shim),
        Ok(ShimAttr::Callable(callable)) => {
            value::ext_function_new(callable.module, callable.method, member.to_string())
        }
        // A type object is a first-class opaque value: it exposes
        // `__name__`/`__module__`, supports `isinstance` and is callable
        // through [`call_type_object`](capi::call_type_object).
        Ok(ShimAttr::Type(address)) => type_object_value(address),
        Err(message) => raised_error(&message),
    }
}

/// Python-visible attribute names exported by a compiled extension module.
pub(crate) fn module_attribute_names(path: &str) -> Result<Vec<String>, String> {
    if crate::native::is_hna_path(path) {
        return crate::native::attribute_names(None, path);
    }
    capi::load_extension(path, &init_name(path)).map(capi::module_attribute_names)
}

/// Materialises a module-owned type handle as an opaque Hydro value.
///
/// The registry keeps its own reference; the returned value owns one more.
fn type_object_value(address: usize) -> i64 {
    let object = address as *mut capi::ShimObject;
    capi::inc_ref(object);
    value::ext_object_new(object, capi::type_display_name(address))
}

/// `from <extension> import member`: a module attribute read for an import
/// statement.
///
/// Unlike [`get`], a missing member and an unloadable shared object raise
/// `ImportError` instead of `AttributeError`/`RuntimeError`, matching CPython's
/// from-import, so `try: from ext import name` / `except ImportError:` runs the
/// handler. `module` is the import name used in the error message.
pub(crate) fn import_member(module: &str, path: &str, member: &str) -> i64 {
    if crate::native::is_hna_path(path) {
        return crate::native::import_member(module, path, member);
    }
    let loaded = match capi::load_extension(path, &init_name(path)) {
        Ok(loaded) => loaded,
        Err(message) => return raised_import_error(&message),
    };
    match capi::get_attr(loaded, member) {
        Ok(ShimAttr::Value(shim)) => result_value(shim),
        Ok(ShimAttr::Callable(callable)) => {
            value::ext_function_new(callable.module, callable.method, member.to_string())
        }
        Ok(ShimAttr::Type(address)) => type_object_value(address),
        // The module exists but does not export the name: the from-import
        // failed, which is an `ImportError`, not an `AttributeError`.
        Err(_) => crate::builtins::cannot_import_name(member, module),
    }
}

/// Raises `ImportError` for a shared-object load failure reported by the shim.
///
/// A load error means the extension module as a whole is unavailable, exactly
/// the case CPython's importer reports as `ImportError`. A message that names a
/// known exception class keeps its text without the `Type: ` prefix.
fn raised_import_error(message: &str) -> i64 {
    let text = match message.split_once(": ") {
        Some((name, rest)) if exception_tag(name).is_some() => rest,
        _ => message,
    };
    exc::raise_with_message(IMPORT_ERROR, text)
}

/// Calls a first-class extension function obtained from [`get`].
pub(crate) fn call_value(state: &ExtFunctionState, argc: usize, argv: *const i64) -> i64 {
    let argv = match argument_slice(argc, argv) {
        Ok(args) => args,
        Err(message) => return exc::raise_with_message(TYPE_ERROR, message),
    };
    match capi::call_method_hydro_args(state.module, state.method, argv) {
        Ok(value) => value,
        Err(message) => raised_error(&message),
    }
}

// ---------------------------------------------------------------------------
// Opaque extension handles
// ---------------------------------------------------------------------------

/// Validates a Hydro keyword buffer and keeps the values tagged.
///
/// The names are checked exactly like [`abi_keywords`], but the values stay
/// Hydro values so the call can track container identity across the bridge.
fn hydro_keywords(kwargc: usize, kw_argv: *const i64) -> Result<Vec<(String, i64)>, i64> {
    if kw_argv.is_null() || kwargc > isize::MAX as usize / (2 * std::mem::size_of::<i64>()) {
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            "keyword arguments were passed without an argument buffer",
        ));
    }
    // SAFETY: ABI contract checked above: `kw_argv` holds `2 * kwargc` values.
    let pairs = unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) };
    let mut keywords = Vec::with_capacity(kwargc);
    for pair in pairs.as_chunks::<2>().0 {
        let Some(name) = value::as_str(pair[0]) else {
            return Err(exc::raise_with_message(
                TYPE_ERROR,
                "keyword name must be str",
            ));
        };
        keywords.push((name.to_string(), pair[1]));
    }
    Ok(keywords)
}

/// Borrows the handle of an opaque extension value, or raises `TypeError`.
fn ext_handle(value_arg: i64) -> Option<*mut capi::ShimObject> {
    match value::as_ext_object(value_arg) {
        Some(state) => Some(state.object),
        None => {
            exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "expected an extension object, not '{}'",
                    value::type_name(value_arg)
                ),
            );
            None
        }
    }
}

/// `object.name` for an opaque extension value.
pub(crate) fn ext_object_getattr(value_arg: i64, name: &str) -> i64 {
    let Some(object) = ext_handle(value_arg) else {
        return NONE;
    };
    match capi::object_get_attr_hydro(object, name) {
        Ok(value) => value,
        Err(message) => raised_error(&message),
    }
}

/// Calls an opaque extension value (`Type(...)` or `instance(...)`).
pub(crate) fn ext_object_call(value_arg: i64, argc: usize, argv: *const i64) -> i64 {
    let Some(object) = ext_handle(value_arg) else {
        return NONE;
    };
    let argv = match argument_slice(argc, argv) {
        Ok(args) => args,
        Err(message) => return exc::raise_with_message(TYPE_ERROR, message),
    };
    // Vectorcall handles both heap-type construction and plain callables.
    match capi::object_call_hydro_args(object, argv, &[]) {
        Ok(value) => value,
        Err(message) => raised_error(&message),
    }
}

/// Calls `object.name(args...)` on an opaque extension value.
pub(crate) fn ext_object_method(value_arg: i64, name: &str, argc: usize, argv: *const i64) -> i64 {
    let Some(object) = ext_handle(value_arg) else {
        return NONE;
    };
    let argv = match argument_slice(argc, argv) {
        Ok(args) => args,
        Err(message) => return exc::raise_with_message(TYPE_ERROR, message),
    };
    match capi::object_call_method_hydro_args(object, name, argv, &[]) {
        Ok(value) => value,
        Err(message) => raised_error(&message),
    }
}

/// Calls `object.name(args..., name=value...)` on an opaque extension value.
pub(crate) fn ext_object_method_kw(
    value_arg: i64,
    name: &str,
    argc: usize,
    argv: *const i64,
    kwargc: usize,
    kw_argv: *const i64,
) -> i64 {
    let Some(object) = ext_handle(value_arg) else {
        return NONE;
    };
    let argv = match argument_slice(argc, argv) {
        Ok(args) => args,
        Err(message) => return exc::raise_with_message(TYPE_ERROR, message),
    };
    let keywords = match hydro_keywords(kwargc, kw_argv) {
        Ok(keywords) => keywords,
        Err(value) => return value,
    };
    match capi::object_call_method_hydro_args(object, name, argv, &keywords) {
        Ok(value) => value,
        Err(message) => raised_error(&message),
    }
}

/// Calls an opaque extension value with keyword arguments.
pub(crate) fn ext_object_call_kw(
    value_arg: i64,
    argc: usize,
    argv: *const i64,
    kwargc: usize,
    kw_argv: *const i64,
) -> i64 {
    let Some(object) = ext_handle(value_arg) else {
        return NONE;
    };
    let argv = match argument_slice(argc, argv) {
        Ok(args) => args,
        Err(message) => return exc::raise_with_message(TYPE_ERROR, message),
    };
    let keywords = match hydro_keywords(kwargc, kw_argv) {
        Ok(keywords) => keywords,
        Err(value) => return value,
    };
    match capi::object_call_hydro_args(object, argv, &keywords) {
        Ok(value) => value,
        Err(message) => raised_error(&message),
    }
}

/// `callable(value)` for an opaque extension handle.
pub(crate) fn ext_object_is_callable(value_arg: i64) -> bool {
    match value::as_ext_object(value_arg) {
        Some(state) => capi::object_callable(state.object),
        None => false,
    }
}

/// Metadata override for one opaque extension handle.
#[derive(Default, Clone)]
struct ExtMetadata {
    name: Option<String>,
    qualname: Option<String>,
    module: Option<String>,
    /// `Some(None)` means the attribute was explicitly set to `None`.
    doc: Option<Option<String>>,
}

/// Writable metadata slots of extension handles, keyed by handle address.
///
/// Type and method handles live in the extension's heap, so their writable
/// `__name__`/`__qualname__`/`__module__`/`__doc__` values are kept here and
/// consulted by the Hydro attribute path before the C attribute lookup.
static EXT_METADATA: LazyLock<Mutex<HashMap<usize, ExtMetadata>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn metadata_lock() -> std::sync::MutexGuard<'static, HashMap<usize, ExtMetadata>> {
    EXT_METADATA
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn text_value(text: String) -> i64 {
    value::str_alloc(text)
}

/// Reads a stored override for `name`, if the handle has one.
pub(crate) fn ext_object_metadata_getattr(value_arg: i64, name: &str) -> Option<i64> {
    let state = value::as_ext_object(value_arg)?;
    let address = state.object as usize;
    let guard = metadata_lock();
    let metadata = guard.get(&address)?;
    match name {
        "__name__" => metadata.name.as_ref().map(|text| text_value(text.clone())),
        "__qualname__" => metadata
            .qualname
            .as_ref()
            .map(|text| text_value(text.clone())),
        "__module__" => metadata
            .module
            .as_ref()
            .map(|text| text_value(text.clone())),
        "__doc__" => metadata.doc.as_ref().map(|doc| match doc {
            Some(text) => text_value(text.clone()),
            None => NONE,
        }),
        _ => None,
    }
}

/// Stores one of the writable metadata slots of an opaque handle.
///
/// Returns `None` when `name` is not a metadata slot (so the caller falls back
/// to `PyObject_SetAttr`), `Some(NONE)` on success and `Some(value)` with a
/// pending exception on failure.
pub(crate) fn ext_object_metadata_setattr(
    value_arg: i64,
    name: &str,
    new_value: i64,
) -> Option<i64> {
    let state = value::as_ext_object(value_arg)?;
    let slot = match name {
        "__name__" | "__qualname__" | "__module__" | "__doc__" => name,
        _ => return None,
    };
    let address = state.object as usize;
    let mut guard = metadata_lock();
    let metadata = guard.entry(address).or_default();
    if slot == "__doc__" && new_value == NONE {
        metadata.doc = Some(None);
        return Some(NONE);
    }
    let Some(text) = value::as_str(new_value) else {
        return Some(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{slot} must be a string, not '{}'",
                value::type_name(new_value)
            ),
        ));
    };
    let text = text.to_string();
    match slot {
        "__name__" => metadata.name = Some(text),
        "__qualname__" => metadata.qualname = Some(text),
        "__module__" => metadata.module = Some(text),
        "__doc__" => metadata.doc = Some(Some(text)),
        _ => unreachable!(),
    }
    Some(NONE)
}

/// Attribute read for a first-class extension function.
pub(crate) fn ext_function_getattr(value_arg: i64, name: &str) -> Option<i64> {
    let state = value::as_ext_function(value_arg)?;
    match name {
        "__name__" => Some(text_value(
            state
                .metadata
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .name
                .clone()
                .unwrap_or_else(|| state.name.clone()),
        )),
        "__qualname__" => {
            let metadata = state
                .metadata
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Some(text_value(
                metadata
                    .qualname
                    .clone()
                    .or_else(|| metadata.name.clone())
                    .unwrap_or_else(|| state.name.clone()),
            ))
        }
        "__module__" => state
            .metadata
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .module
            .clone()
            .map(text_value),
        "__doc__" => Some(
            match state
                .metadata
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .doc
                .clone()
            {
                Some(Some(text)) => text_value(text),
                Some(None) | None => NONE,
            },
        ),
        _ => None,
    }
}

/// Attribute write for a first-class extension function.
///
/// Returns `None` for non-metadata names; extension functions have no writable
/// arbitrary attributes in this shim.
pub(crate) fn ext_function_setattr(value_arg: i64, name: &str, new_value: i64) -> Option<i64> {
    let state = value::as_ext_function(value_arg)?;
    let slot = match name {
        "__name__" | "__qualname__" | "__module__" | "__doc__" => name,
        _ => return None,
    };
    let mut metadata = state
        .metadata
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot == "__doc__" && new_value == NONE {
        metadata.doc = Some(None);
        return Some(NONE);
    }
    let Some(text) = value::as_str(new_value) else {
        return Some(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{slot} must be a string, not '{}'",
                value::type_name(new_value)
            ),
        ));
    };
    let text = text.to_string();
    match slot {
        "__name__" => metadata.name = Some(text),
        "__qualname__" => metadata.qualname = Some(text),
        "__module__" => metadata.module = Some(text),
        "__doc__" => metadata.doc = Some(Some(text)),
        _ => unreachable!(),
    }
    Some(NONE)
}

/// Sets a non-metadata attribute on an opaque extension handle.
pub(crate) fn ext_object_setattr(value_arg: i64, name: &str, new_value: i64) -> i64 {
    let Some(object) = ext_handle(value_arg) else {
        return NONE;
    };
    let Ok(shim) = value_to_shim(new_value) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "cannot set '{}' on an extension object with '{}'",
                name,
                value::type_name(new_value)
            ),
        );
    };
    let object_value = capi::value_object(shim);
    let Ok(c_name) = CString::new(name) else {
        capi::dec_ref(object_value);
        return exc::raise_with_message(TYPE_ERROR, "attribute name contains a NUL");
    };
    let status = capi::PyObject_SetAttrString(object, c_name.as_ptr(), object_value);
    capi::dec_ref(object_value);
    if status == 0 {
        NONE
    } else {
        raised_error(
            &capi::take_error().unwrap_or_else(|| format!("AttributeError: cannot set '{name}'")),
        )
    }
}

/// Whether `value` (an opaque extension handle) is an instance of a Hydro
/// class that derives from an extension type.
pub(crate) fn ext_object_is_hydro_subclass(value_arg: i64, class: i64) -> bool {
    let Some(state) = value::as_ext_object(value_arg) else {
        return false;
    };
    for ancestor in value::class_mro(class) {
        let Some(base) = value::as_ext_object(ancestor) else {
            continue;
        };
        if !capi::object_is_type(base.object) {
            continue;
        }
        if matches!(capi::object_isinstance(state.object, base.object), Ok(true)) {
            return true;
        }
    }
    false
}

/// `isinstance(value, extension_type)`.
pub(crate) fn ext_object_isinstance(value_arg: i64, type_arg: i64) -> i64 {
    let Some(state) = value::as_ext_object(type_arg) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "isinstance() arg 2 must be a type or tuple of types",
        );
    };
    if !capi::object_is_type(state.object) {
        return exc::raise_with_message(
            TYPE_ERROR,
            "isinstance() arg 2 must be a type or tuple of types",
        );
    }
    let instance = if let Some(value) = value::as_ext_object(value_arg) {
        value.object
    } else {
        let shim = match value_to_shim(value_arg) {
            Ok(shim) => shim,
            Err(message) => return exc::raise_with_message(TYPE_ERROR, &message),
        };
        capi::value_object(shim)
    };
    let owns_instance = value::as_ext_object(value_arg).is_none();
    let result = capi::object_isinstance(instance, state.object);
    if owns_instance {
        capi::dec_ref(instance);
    }
    match result {
        Ok(equal) => value::bool_value(equal),
        Err(message) => raised_error(&message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serialize::{self, PickleError};

    #[test]
    fn init_name_uses_first_stem_segment() {
        assert_eq!(
            init_name("/tmp/hydro_capi_probe.abi3.so"),
            "hydro_capi_probe"
        );
        assert_eq!(
            init_name("hydro_capi_probe.cpython-314-x86_64-linux-gnu.so"),
            "hydro_capi_probe"
        );
        assert_eq!(init_name("mod.so"), "mod");
        assert_eq!(init_name("/tmp/a.b/mod.abi3.so"), "mod");
    }

    fn roundtrip(shim: ShimValue) -> ShimValue {
        let tagged = shim_to_value(shim).expect("shim value converts");
        value_to_shim(tagged).expect("tagged value converts back")
    }

    #[test]
    fn scalar_roundtrips() {
        assert!(matches!(roundtrip(ShimValue::None), ShimValue::None));
        assert!(matches!(
            roundtrip(ShimValue::Bool(true)),
            ShimValue::Bool(true)
        ));
        assert!(matches!(
            roundtrip(ShimValue::Bool(false)),
            ShimValue::Bool(false)
        ));
        assert!(matches!(roundtrip(ShimValue::Int(42)), ShimValue::Int(42)));
        assert!(matches!(roundtrip(ShimValue::Int(-7)), ShimValue::Int(-7)));
        match roundtrip(ShimValue::Float(1.5)) {
            ShimValue::Float(f) => assert_eq!(f, 1.5),
            _ => panic!("float roundtrip changed type"),
        }
        match roundtrip(ShimValue::Str("hello".to_string())) {
            ShimValue::Str(s) => assert_eq!(s, "hello"),
            _ => panic!("str roundtrip changed type"),
        }
        match roundtrip(ShimValue::Bytes(vec![0, 1, 255])) {
            ShimValue::Bytes(bytes) => assert_eq!(bytes, vec![0, 1, 255]),
            _ => panic!("bytes roundtrip changed type"),
        }
    }

    #[test]
    fn singleton_constants_cross_the_boundary() {
        // `NotImplemented` is a real Hydro value, so it round trips.
        assert!(matches!(
            roundtrip(ShimValue::NotImplemented),
            ShimValue::NotImplemented
        ));
        assert!(matches!(
            value_to_shim(value::NOT_IMPLEMENTED),
            Ok(ShimValue::NotImplemented)
        ));
        assert_eq!(
            shim_type_name(&ShimValue::NotImplemented),
            "NotImplementedType"
        );

        // `Ellipsis` has no Hydro representation: it is a tagged error rather
        // than an invented value.
        let message = shim_to_value(ShimValue::Ellipsis).expect_err("Ellipsis is unsupported");
        assert!(message.contains("Ellipsis"), "{message}");
        assert_eq!(shim_type_name(&ShimValue::Ellipsis), "ellipsis");
    }

    #[test]
    fn tagged_scalars_convert_to_shim() {
        assert!(matches!(value_to_shim(value::NONE), Ok(ShimValue::None)));
        assert!(matches!(
            value_to_shim(value::TRUE),
            Ok(ShimValue::Bool(true))
        ));
        assert!(matches!(
            value_to_shim(value::FALSE),
            Ok(ShimValue::Bool(false))
        ));
        assert!(matches!(
            value_to_shim(value::encode_int(-3).unwrap()),
            Ok(ShimValue::Int(-3))
        ));
    }

    #[test]
    fn containers_roundtrip() {
        let list = value::list_new(vec![
            value::encode_int(1).unwrap(),
            value::encode_int(2).unwrap(),
        ]);
        let nested = value::tuple_new(vec![value::str_alloc("x".to_string()), list]);
        let dict = value::dict_new(vec![(value::str_alloc("k".to_string()), nested)]);

        let shim = value_to_shim(dict).expect("dict converts to the shim");
        let tagged = shim_to_value(shim).expect("dict converts back");

        let pairs = value::as_dict(tagged).expect("tagged value is a dict");
        assert_eq!(pairs.len(), 1);
        assert_eq!(value::as_str(pairs[0].0), Some("k"));
        let items = value::as_tuple(pairs[0].1).expect("tuple value");
        assert_eq!(value::as_str(items[0]), Some("x"));
        let items = value::as_list(items[1]).expect("list value");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], value::encode_int(1).unwrap());
        assert_eq!(items[1], value::encode_int(2).unwrap());
    }

    #[test]
    fn cyclic_arguments_report_type() {
        let list = value::list_new(Vec::new());
        value::list_items_mut(list)
            .expect("list payload")
            .push(list);

        let message = value_to_shim(list).expect_err("cycle is rejected");
        assert!(message.contains("cyclic"), "{message}");
        assert!(message.contains("'list'"), "{message}");
    }

    #[test]
    fn inbound_conversion_depth_is_bounded() {
        let mut nested = value::list_new(Vec::new());
        for _ in 0..=MAX_CONVERSION_DEPTH {
            nested = value::list_new(vec![nested]);
        }

        let message = value_to_shim(nested).expect_err("deep nesting is rejected");
        assert!(message.contains("more than"), "{message}");
    }

    #[test]
    fn outbound_conversion_depth_is_bounded() {
        let mut nested = ShimValue::List(Vec::new());
        for _ in 0..=MAX_CONVERSION_DEPTH {
            nested = ShimValue::List(vec![nested]);
        }

        let message = shim_to_value(nested).expect_err("deep result is rejected");
        assert!(message.contains("more than"), "{message}");
    }

    #[test]
    fn unsupported_arguments_report_type() {
        // Iterators have no C handle representation; they are rejected
        // instead of crossing as a fake object. Extension functions, by
        // contrast, are real callables and cross as Hydro handles.
        let iterator = value::iter_new(value::list_new(Vec::new()));
        let message = value_to_shim(iterator).expect_err("iterators are unsupported");
        assert!(message.contains("'iterator'"), "{message}");

        let function = value::ext_function_new(1, 2, "tag".to_string());
        match value_to_shim(function) {
            Ok(ShimValue::Hydro(handle)) => assert_eq!(handle.value(), function),
            Ok(other) => panic!("extension functions must cross as a Hydro handle: {other:?}"),
            Err(message) => panic!("extension functions are callable handles: {message}"),
        }
    }

    #[test]
    fn non_callable_attribute_raises_type_error() {
        exc::init();
        let result = not_callable(&ShimValue::Int(3));
        assert_eq!(result, NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        let exception = value::exception_data(exc::pending_value()).expect("pending exception");
        assert_eq!(exception.msg, "'int' object is not callable");
        exc::init();
    }

    #[test]
    fn c_api_exception_prefixes_select_hydro_tags() {
        let cases = [
            ("ValueError: bad", i64::from(VALUE_ERROR)),
            ("TypeError: bad", i64::from(TYPE_ERROR)),
            ("ZeroDivisionError: bad", i64::from(ZERO_DIVISION_ERROR)),
            ("RuntimeError: bad", i64::from(RUNTIME_ERROR)),
            ("OverflowError: bad", i64::from(OVERFLOW_ERROR)),
            ("IndexError: bad", i64::from(INDEX_ERROR)),
            ("KeyError: bad", i64::from(KEY_ERROR)),
            ("StopIteration: done", i64::from(STOP_ITERATION)),
            ("AttributeError: bad", i64::from(ATTRIBUTE_ERROR)),
            ("OSError: bad", i64::from(OS_ERROR)),
            ("BaseException: bad", i64::from(BASE_EXCEPTION)),
            ("Exception: bad", i64::from(EXCEPTION)),
            ("ArithmeticError: bad", i64::from(ARITHMETIC_ERROR)),
            ("AssertionError: bad", i64::from(ASSERTION_ERROR)),
            ("BaseExceptionGroup: bad", i64::from(BASE_EXCEPTION_GROUP)),
            ("BufferError: bad", i64::from(BUFFER_ERROR)),
            ("EOFError: bad", i64::from(EOF_ERROR)),
            ("FileExistsError: bad", i64::from(FILE_EXISTS_ERROR)),
            ("FileNotFoundError: bad", i64::from(FILE_NOT_FOUND_ERROR)),
            ("FloatingPointError: bad", i64::from(FLOATING_POINT_ERROR)),
            ("GeneratorExit: bad", i64::from(GENERATOR_EXIT)),
            ("ImportError: bad", i64::from(IMPORT_ERROR)),
            ("IndentationError: bad", i64::from(INDENTATION_ERROR)),
            ("KeyboardInterrupt: bad", i64::from(KEYBOARD_INTERRUPT)),
            ("LookupError: bad", i64::from(LOOKUP_ERROR)),
            ("MemoryError: bad", i64::from(MEMORY_ERROR)),
            (
                "ModuleNotFoundError: bad",
                i64::from(MODULE_NOT_FOUND_ERROR),
            ),
            ("NameError: bad", i64::from(NAME_ERROR)),
            ("NotImplementedError: bad", i64::from(NOT_IMPLEMENTED_ERROR)),
            ("RecursionError: bad", i64::from(RECURSION_ERROR)),
            ("ReferenceError: bad", i64::from(REFERENCE_ERROR)),
            ("StopAsyncIteration: done", i64::from(STOP_ASYNC_ITERATION)),
            ("SyntaxError: bad", i64::from(SYNTAX_ERROR)),
            ("SystemError: bad", i64::from(SYSTEM_ERROR)),
            ("SystemExit: bad", i64::from(SYSTEM_EXIT)),
            ("TabError: bad", i64::from(TAB_ERROR)),
            ("TimeoutError: bad", i64::from(TIMEOUT_ERROR)),
            ("UnboundLocalError: bad", i64::from(NAME_ERROR)),
            ("UnicodeDecodeError: bad", i64::from(UNICODE_DECODE_ERROR)),
            ("UnicodeEncodeError: bad", i64::from(UNICODE_ENCODE_ERROR)),
            (
                "UnicodeTranslateError: bad",
                i64::from(UNICODE_TRANSLATE_ERROR),
            ),
            ("UnicodeError: bad", i64::from(UNICODE_ERROR)),
            ("UserWarning: bad", i64::from(USER_WARNING)),
            ("Warning: bad", i64::from(WARNING)),
            // Aliases without a dedicated tag resolve to their class.
            ("IOError: bad", i64::from(OS_ERROR)),
            ("EnvironmentError: bad", i64::from(OS_ERROR)),
            ("WindowsError: bad", i64::from(OS_ERROR)),
            // Warnings without a tag resolve to `Warning`.
            ("EncodingWarning: bad", i64::from(WARNING)),
        ];
        exc::init();
        for (message, tag) in cases {
            raised_error(message);
            assert_eq!(exc::pending_tag(), tag, "{message}");
            exc::init();
        }

        raised_error("ValueError: bad value");
        let exception = value::exception_data(exc::pending_value()).expect("pending exception");
        assert_eq!(exception.msg, "bad value");
        exc::init();
    }

    #[test]
    fn missing_attribute_message_selects_attribute_error() {
        exc::init();
        raised_error("module 'probe' has no attribute 'missing'");
        assert_eq!(exc::pending_tag(), i64::from(ATTRIBUTE_ERROR));
        exc::init();
        raised_error("dlopen failed: file not found");
        assert_eq!(exc::pending_tag(), i64::from(RUNTIME_ERROR));
        exc::init();
        raised_error("TypeError: bad argument");
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        let exception = value::exception_data(exc::pending_value()).expect("pending exception");
        assert_eq!(exception.msg, "bad argument");
        exc::init();
        raised_error("add() takes exactly one argument");
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();
        raised_error("PyInit_probe returned NULL");
        assert_eq!(exc::pending_tag(), i64::from(RUNTIME_ERROR));
        exc::init();
    }

    #[test]
    fn extension_function_rendering() {
        let function = value::ext_function_new(7, 3, "add".to_string());
        assert_eq!(value::type_name(function), "builtin_function_or_method");
        assert_eq!(value::to_string(function), "<built-in function add>");
        assert_eq!(value::repr(function), "<built-in function add>");
        assert!(value::truthy(function));
        assert!(value::as_ext_function(function).is_some());
    }

    #[test]
    fn capsule_roundtrips_with_pointer_and_name() {
        let tagged = value::capsule_new(0xDEAD_BEEF, Some("tea.demo".to_string()));
        let shim = value_to_shim(tagged).expect("capsule converts to the shim");
        match &shim {
            ShimValue::Capsule { ptr, name } => {
                assert_eq!(*ptr, 0xDEAD_BEEF);
                assert_eq!(name.as_deref(), Some("tea.demo"));
            }
            other => panic!("unexpected shim value: {other:?}"),
        }

        let back = shim_to_value(shim).expect("capsule converts back");
        assert_eq!(
            value::as_capsule(back),
            Some((0xDEAD_BEEF, Some("tea.demo")))
        );

        let unnamed = value::capsule_new(0x1234, None);
        let shim = value_to_shim(unnamed).expect("unnamed capsule converts");
        assert!(matches!(
            shim,
            ShimValue::Capsule {
                ptr: 0x1234,
                name: None
            }
        ));
        let back = shim_to_value(shim).expect("unnamed capsule converts back");
        assert_eq!(value::as_capsule(back), Some((0x1234, None)));
    }

    #[test]
    fn nested_sets_roundtrip_through_the_contract() {
        let shim = ShimValue::Dict(vec![
            (
                ShimValue::Str("fields".to_string()),
                ShimValue::Set(vec![
                    ShimValue::Str("a".to_string()),
                    ShimValue::Str("b".to_string()),
                ]),
            ),
            (
                ShimValue::Str("nested".to_string()),
                ShimValue::List(vec![ShimValue::Set(vec![ShimValue::Int(1)])]),
            ),
        ]);
        let tagged = shim_to_value(shim.clone()).expect("sets convert to Hydro");
        let pairs = value::as_dict(tagged).expect("the root stays a dict");
        let fields = pairs
            .iter()
            .find(|(key, _)| value::as_str(*key) == Some("fields"))
            .map(|&(_, item)| item)
            .expect("fields entry");
        assert_eq!(
            value::as_set(fields)
                .expect("the nested value is a Hydro set")
                .len(),
            2
        );
        let nested = pairs
            .iter()
            .find(|(key, _)| value::as_str(*key) == Some("nested"))
            .map(|&(_, item)| item)
            .expect("nested entry");
        let nested_items = value::as_list(nested).expect("the nested list stays a list");
        assert!(value::as_set(nested_items[0]).is_some());

        let back = value_to_shim(tagged).expect("sets convert back to the contract");
        let ShimValue::Dict(pairs) = back else {
            panic!("the root stays a dict");
        };
        let fields = pairs
            .iter()
            .find(|(key, _)| matches!(key, ShimValue::Str(name) if name.as_str() == "fields"))
            .map(|(_, item)| item)
            .expect("fields entry");
        let ShimValue::Set(items) = fields else {
            panic!("the nested value becomes ShimValue::Set, not an opaque handle");
        };
        assert_eq!(items.len(), 2);
        let nested = pairs
            .iter()
            .find(|(key, _)| matches!(key, ShimValue::Str(name) if name.as_str() == "nested"))
            .map(|(_, item)| item)
            .expect("nested entry");
        let ShimValue::List(items) = nested else {
            panic!("the nested list stays a list");
        };
        assert!(matches!(&items[0], ShimValue::Set(inner) if inner.len() == 1));
    }

    #[test]
    fn opaque_handles_roundtrip_with_owned_references() {
        // A handle created with one reference becomes the Hydro value's own
        // reference; reading it back adds exactly one contract reference.
        let object = capi::new_object(capi::ShimKind::Int(5));
        let shim = ShimValue::Object {
            object: OwnedShimObject::new(object),
            type_name: "probe.Thing".to_string(),
        };
        assert_eq!(capi::Py_REFCNT(object), 2);
        capi::dec_ref(object);

        let tagged = shim_to_value(shim).expect("object converts to Hydro");
        let state = value::as_ext_object(tagged).expect("Hydro ext object");
        assert_eq!(state.type_name, "probe.Thing");
        let pointer = state.object;
        assert_eq!(capi::Py_REFCNT(pointer), 1);
        assert!(value::is_ext_object(tagged));
        assert_eq!(value::type_name_of(tagged), "probe.Thing");

        let back = value_to_shim(tagged).expect("Hydro object converts back");
        assert_eq!(capi::Py_REFCNT(pointer), 2);
        match back {
            ShimValue::Object { object, .. } => assert_eq!(object.as_ptr(), pointer),
            other => panic!("unexpected shim value: {other:?}"),
        }
        assert_eq!(capi::Py_REFCNT(pointer), 1);
    }

    #[test]
    fn serialization_rejects_capsules() {
        let capsule = value::capsule_new(0x1234, Some("tea.demo".to_string()));
        assert_eq!(
            serialize::encode(capsule, false),
            Err(PickleError::Unsupported("PyCapsule"))
        );
    }

    #[test]
    fn serialization_rejects_extension_functions() {
        let function = value::ext_function_new(1, 2, "tag".to_string());
        assert_eq!(
            serialize::encode(function, false),
            Err(PickleError::Unsupported("builtin_function_or_method"))
        );
    }
}
