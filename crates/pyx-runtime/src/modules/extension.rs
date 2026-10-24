//! Bridge to CPython limited-API (`abi3`) extension modules.
//!
//! The frontend resolves an extension `.so` to its absolute path and codegen
//! calls [`call`] / [`get`] with the shared-object path and the Python
//! attribute name. All CPython interaction lives in `crate::capi`; this module
//! only converts between tagged PyX values and [`ShimValue`]s and allocates
//! first-class function values for callables.
//!
//! Only scalars cross the boundary in this slice. Containers and user
//! objects raise a `TypeError` instead of being silently mangled.
//!
//! All fallible paths report a pending exception and return the `None` dummy
//! value; no user input can panic.

use std::path::Path;

use crate::capi::{self, ShimAttr, ShimValue};
use crate::exc::{self, ATTRIBUTE_ERROR, RUNTIME_ERROR, TYPE_ERROR};
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

/// Converts one tagged PyX value to a shim value.
///
/// Supported types are `None`, `bool`, `int`, `float`, `str` and `bytes`.
/// Everything else reports the unsupported type by name.
pub(crate) fn value_to_shim(value_arg: i64) -> Result<ShimValue, String> {
    if value_arg == NONE {
        return Ok(ShimValue::None);
    }
    if value_arg == value::TRUE {
        return Ok(ShimValue::Bool(true));
    }
    if value_arg == value::FALSE {
        return Ok(ShimValue::Bool(false));
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
    Err(format!(
        "cannot pass '{}' object to a C extension function",
        value::type_name(value_arg)
    ))
}

/// Converts one shim value back to a tagged PyX value.
pub(crate) fn shim_to_value(shim: ShimValue) -> Result<i64, String> {
    Ok(match shim {
        ShimValue::None => NONE,
        ShimValue::Bool(b) => value::bool_value(b),
        ShimValue::Int(n) => value::encode_int(n)
            .ok_or_else(|| format!("integer result {n} is outside the supported PyX int range"))?,
        ShimValue::Float(f) => value::float_new(f),
        ShimValue::Str(s) => value::str_alloc(s),
        ShimValue::Bytes(bytes) => value::bytes_new(bytes),
    })
}

/// Python type name of a shim value, used in the not-callable error.
fn shim_type_name(shim: &ShimValue) -> &'static str {
    match shim {
        ShimValue::None => "NoneType",
        ShimValue::Bool(_) => "bool",
        ShimValue::Int(_) => "int",
        ShimValue::Float(_) => "float",
        ShimValue::Str(_) => "str",
        ShimValue::Bytes(_) => "bytes",
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
/// The shim reports its own failure category in the text (for example
/// `AttributeError: module 'x' has no attribute 'y'`); anything else is an
/// internal load/call failure and therefore a `RuntimeError`.
fn raised_error(message: &str) -> i64 {
    let (tag, text) = if let Some(rest) = message.strip_prefix("AttributeError: ") {
        (ATTRIBUTE_ERROR, rest)
    } else if let Some(rest) = message.strip_prefix("TypeError: ") {
        (TYPE_ERROR, rest)
    } else if let Some(rest) = message.strip_prefix("RuntimeError: ") {
        (RUNTIME_ERROR, rest)
    } else if message.contains("has no attribute")
        || message.contains("attribute not found")
        || message.contains("AttributeError")
    {
        (ATTRIBUTE_ERROR, message)
    } else if message.contains("TypeError")
        || message.contains("not callable")
        || message.contains("takes ")
    {
        (TYPE_ERROR, message)
    } else {
        (RUNTIME_ERROR, message)
    };
    exc::raise_with_message(tag, text)
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
            let args = match shim_arguments(argv) {
                Ok(args) => args,
                Err(value) => return value,
            };
            match capi::call_method(callable.module, callable.method, &args) {
                Ok(result) => result_value(result),
                Err(message) => raised_error(&message),
            }
        }
        ShimAttr::Value(shim) => not_callable(&shim),
    }
}

/// `extension.get(path, member)`: materialises a module attribute as a PyX
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
    let module = match capi::load_extension(path, &init_name(path)) {
        Ok(module) => module,
        Err(message) => return raised_error(&message),
    };
    match capi::get_attr(module, member) {
        Ok(ShimAttr::Value(shim)) => result_value(shim),
        Ok(ShimAttr::Callable(callable)) => {
            value::ext_function_new(callable.module, callable.method, member.to_string())
        }
        Err(message) => raised_error(&message),
    }
}

/// Calls a first-class extension function obtained from [`get`].
pub(crate) fn call_value(state: &ExtFunctionState, argc: usize, argv: *const i64) -> i64 {
    let argv = match argument_slice(argc, argv) {
        Ok(args) => args,
        Err(message) => return exc::raise_with_message(TYPE_ERROR, message),
    };
    let args = match shim_arguments(argv) {
        Ok(args) => args,
        Err(value) => return value,
    };
    match capi::call_method(state.module, state.method, &args) {
        Ok(result) => result_value(result),
        Err(message) => raised_error(&message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serialize::{self, PickleError};

    #[test]
    fn init_name_uses_first_stem_segment() {
        assert_eq!(init_name("/tmp/pyx_capi_probe.abi3.so"), "pyx_capi_probe");
        assert_eq!(
            init_name("pyx_capi_probe.cpython-314-x86_64-linux-gnu.so"),
            "pyx_capi_probe"
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
    fn unsupported_arguments_report_type() {
        let list = value::list_new(Vec::new());
        match value_to_shim(list) {
            Err(message) => assert!(message.contains("'list'"), "{message}"),
            Ok(_) => panic!("lists are unsupported"),
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
    fn serialization_rejects_extension_functions() {
        let function = value::ext_function_new(1, 2, "tag".to_string());
        assert_eq!(
            serialize::encode(function, false),
            Err(PickleError::Unsupported("builtin_function_or_method"))
        );
    }
}
