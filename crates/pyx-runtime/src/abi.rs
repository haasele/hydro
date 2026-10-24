use std::os::raw::c_uchar;

use crate::{builtins, container, exc, fiber, format, heap, iter, methods, modules, ops, value};

// ---------------------------------------------------------------------
// Constants (tagged representation, see ARCHITECTURE §3.1)
// ---------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn pyx_none() -> i64 {
    value::NONE
}

#[no_mangle]
pub extern "C" fn pyx_true() -> i64 {
    value::TRUE
}

#[no_mangle]
pub extern "C" fn pyx_false() -> i64 {
    value::FALSE
}

// ---------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn pyx_bool(b: i64) -> i64 {
    value::bool_value(value::truthy(b))
}

#[no_mangle]
pub extern "C" fn pyx_int(v: i64) -> i64 {
    // Identity for tagged immediates; tolerates raw literals from codegen.
    value::normalize_int(v)
}

#[no_mangle]
pub extern "C" fn pyx_str_new(ptr: *const c_uchar, len: usize) -> i64 {
    value::str_from_ptr(ptr, len)
}

/// `bytes` literal: copies `len` bytes from `ptr` into a bytes object.
#[no_mangle]
pub extern "C" fn pyx_bytes_new(ptr: *const c_uchar, len: usize) -> i64 {
    value::bytes_from_ptr(ptr, len)
}

/// `bytes(...)`: the constructor (0, 1 or 2 arguments).
#[no_mangle]
pub extern "C" fn pyx_bytes_build(argc: usize, argv: *const i64) -> i64 {
    builtins::bytes_build(argc, argv)
}

#[no_mangle]
pub extern "C" fn pyx_float_new(f: f64) -> i64 {
    value::float_new(f)
}

/// `list` literal: copies `argc` tagged items from `argv`.
#[no_mangle]
pub extern "C" fn pyx_list_new(argc: usize, argv: *const i64) -> i64 {
    container::list_new(argc, argv)
}

/// `tuple` literal: copies `argc` tagged items from `argv`.
#[no_mangle]
pub extern "C" fn pyx_tuple_new(argc: usize, argv: *const i64) -> i64 {
    container::tuple_new(argc, argv)
}

/// Validates `a, b, ... = value` for lists and tuples of exactly
/// `expected` elements. Returns `None` on success; raises `ValueError`
/// (length mismatch) or `TypeError` (non-sequence) otherwise.
#[no_mangle]
pub extern "C" fn pyx_unpack(value: i64, expected: usize) -> i64 {
    container::unpack(value, expected)
}

/// `dict` literal from parallel key/value buffers. Unhashable keys raise
/// `TypeError` and return `None`.
#[no_mangle]
pub extern "C" fn pyx_dict_new(argc: usize, keys: *const i64, values: *const i64) -> i64 {
    container::dict_new(argc, keys, values)
}

/// `set` literal: dedupes `argc` tagged items while preserving first
/// occurrence. Unhashable items raise `TypeError` and return `None`.
#[no_mangle]
pub extern "C" fn pyx_set_new(argc: usize, argv: *const i64) -> i64 {
    container::set_new(argc, argv)
}

/// `set(iterable)` for lists, tuples, sets, dicts and strings; other types
/// raise `TypeError` and return `None`.
#[no_mangle]
pub extern "C" fn pyx_set_from(iterable: i64) -> i64 {
    container::set_from(iterable)
}

/// `range(start, stop, step)`; tagged integer arguments, `step != 0`.
#[no_mangle]
pub extern "C" fn pyx_range_new(start: i64, stop: i64, step: i64) -> i64 {
    builtins::range_new(start, stop, step)
}

/// `list(iterable)`.
#[no_mangle]
pub extern "C" fn pyx_list_from(iterable: i64) -> i64 {
    builtins::list_from(iterable)
}

/// `dict(iterable)`.
#[no_mangle]
pub extern "C" fn pyx_dict_from(iterable: i64) -> i64 {
    builtins::dict_from(iterable)
}

/// `sum(iterable, start)`.
#[no_mangle]
pub extern "C" fn pyx_sum(iterable: i64, start: i64) -> i64 {
    builtins::sum(iterable, start)
}

/// `enumerate(iterable, start)`: list of `(index, value)` tuples.
#[no_mangle]
pub extern "C" fn pyx_enumerate(iterable: i64, start: i64) -> i64 {
    builtins::enumerate(iterable, start)
}

/// `zip(argc, argv)`: list of tuples up to the shortest source.
#[no_mangle]
pub extern "C" fn pyx_zip(argc: usize, argv: *const i64) -> i64 {
    builtins::zip(argc, argv)
}

// ---------------------------------------------------------------------
// First-class functions (Phase 2.9)
// ---------------------------------------------------------------------

/// Copies the function name passed by generated code. A missing or empty
/// buffer yields the fallback `"<function>"`.
fn function_name(ptr: *const c_uchar, len: usize) -> String {
    if ptr.is_null() || len == 0 || len > isize::MAX as usize {
        return "<function>".to_string();
    }
    // SAFETY: the ABI contract requires `ptr..ptr+len` to be readable;
    // the length check above keeps the slice representable.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let text = String::from_utf8_lossy(bytes).into_owned();
    if text.is_empty() {
        "<function>".to_string()
    } else {
        text
    }
}

/// Decodes a required UTF-8 name, returning `None` for a NULL/empty buffer or
/// an invalid length. Empty names are not valid class or attribute names.
fn required_name(ptr: *const c_uchar, len: usize) -> Option<String> {
    if ptr.is_null() || len == 0 || len > isize::MAX as usize {
        return None;
    }
    // SAFETY: the ABI contract requires `ptr..ptr+len` to be readable; the
    // length check above keeps the slice representable.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let text = String::from_utf8_lossy(bytes).into_owned();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Creates a class object: `class Name(Base):` with already-lowered methods.
///
/// `names_ptr`/`funcs_ptr` point at `method_count` tagged values: method names
/// (strings) and function values. `base` is `NONE` or a class object.
/// Duplicate method names keep their first position and the last function,
/// mirroring successive assignments in a class body. Malformed buffers, names
/// and non-function methods raise `TypeError` and return `None`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn pyx_class_new(
    name_ptr: *const c_uchar,
    name_len: usize,
    base: i64,
    method_count: usize,
    names_ptr: *const i64,
    funcs_ptr: *const i64,
) -> i64 {
    let Some(name) = required_name(name_ptr, name_len) else {
        return exc::raise_with_message(exc::TYPE_ERROR, "class name must be a non-empty string");
    };
    if base != value::NONE && value::as_class(base).is_none() {
        return exc::raise_with_message(
            exc::TYPE_ERROR,
            "class base must be a class object or None",
        );
    }
    let mut methods: Vec<(String, i64)> = Vec::new();
    if method_count > 0 {
        if names_ptr.is_null()
            || funcs_ptr.is_null()
            || method_count > isize::MAX as usize / std::mem::size_of::<(String, i64)>()
        {
            return exc::raise_with_message(
                exc::TYPE_ERROR,
                &format!(
                    "class definition received {method_count} method(s) without a method buffer"
                ),
            );
        }
        methods.reserve(method_count);
        // SAFETY: the ABI contract requires both buffers to hold
        // `method_count` readable tagged values; the length check above keeps
        // the slices representable.
        let names = unsafe { std::slice::from_raw_parts(names_ptr, method_count) };
        let funcs = unsafe { std::slice::from_raw_parts(funcs_ptr, method_count) };
        for (name_value, function) in names.iter().zip(funcs) {
            let Some(method_name) = value::as_str(*name_value) else {
                return exc::raise_with_message(
                    exc::TYPE_ERROR,
                    "class method name must be a string",
                );
            };
            if method_name.is_empty() {
                return exc::raise_with_message(
                    exc::TYPE_ERROR,
                    "class method name must not be empty",
                );
            }
            if value::as_function(*function).is_none() {
                return exc::raise_with_message(
                    exc::TYPE_ERROR,
                    &format!("class method '{method_name}' is not a function"),
                );
            }
            let position = methods
                .iter()
                .position(|(existing, _)| existing.as_str() == method_name);
            match position {
                Some(index) => methods[index].1 = *function,
                None => methods.push((method_name.to_string(), *function)),
            }
        }
    }
    value::class_new(name, base, methods)
}

/// Creates a function value from the generated wrapper entry point.
/// A `NULL` wrapper is an internal error (`RuntimeError`). `flags` bit 0
/// marks an `async def` wrapper (calls create a coroutine) and bit 1 a
/// generator wrapper (calls create a generator).
#[no_mangle]
pub extern "C" fn pyx_function_new(
    wrapper: *const c_uchar,
    arity: usize,
    name_ptr: *const c_uchar,
    name_len: usize,
    flags: u32,
) -> i64 {
    if wrapper.is_null() {
        return exc::raise_with_message(exc::RUNTIME_ERROR, "function wrapper is NULL");
    }
    let name = function_name(name_ptr, name_len);
    value::function_new_with_flags(wrapper as usize, arity, name, flags)
}

/// Creates a closure value from a generated wrapper entry point, the
/// user-visible arity and `capture_count` captured tagged values. A `NULL`
/// wrapper or a missing/oversized capture buffer is an internal error
/// (`RuntimeError`). `flags` has the same meaning as in [`pyx_function_new`].
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn pyx_closure_new(
    wrapper: *const c_uchar,
    arity: usize,
    name_ptr: *const c_uchar,
    name_len: usize,
    capture_count: usize,
    captures_ptr: *const i64,
    flags: u32,
) -> i64 {
    if wrapper.is_null() {
        return exc::raise_with_message(exc::RUNTIME_ERROR, "function wrapper is NULL");
    }
    if capture_count > 0 && (captures_ptr.is_null() || capture_count > isize::MAX as usize) {
        return exc::raise_with_message(exc::RUNTIME_ERROR, "closure capture buffer is invalid");
    }
    let captures = if capture_count == 0 {
        Vec::new()
    } else {
        // SAFETY: the ABI contract requires `captures_ptr..+capture_count`
        // to be readable tagged values; the length check above keeps the
        // slice representable.
        unsafe { std::slice::from_raw_parts(captures_ptr, capture_count) }.to_vec()
    };
    let name = function_name(name_ptr, name_len);
    value::closure_new_with_flags(wrapper as usize, arity, name, captures, flags)
}

/// Copies `argc` tagged arguments for a new fiber. `NULL` with `argc > 0` and
/// over-long counts are reported as an internal error.
fn copy_fiber_args(argc: usize, argv: *const i64) -> Option<Vec<i64>> {
    if argc == 0 {
        return Some(Vec::new());
    }
    if argv.is_null() || argc > isize::MAX as usize / std::mem::size_of::<i64>() {
        return None;
    }
    // SAFETY: the ABI contract requires `argv..argv+argc` to be readable; the
    // length check keeps the slice representable.
    Some(unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec())
}

/// Creates a lazy generator object from a generated wrapper entry point. The
/// body does not run until the first `pyx_iter_next`.
#[no_mangle]
pub extern "C" fn pyx_generator_new(entry: *const c_uchar, argc: usize, argv: *const i64) -> i64 {
    fiber_new(entry, argc, argv, fiber::FiberKind::Generator)
}

/// Creates a coroutine object from a generated wrapper entry point. The body
/// does not run until the first `pyx_await`.
#[no_mangle]
pub extern "C" fn pyx_coroutine_new(entry: *const c_uchar, argc: usize, argv: *const i64) -> i64 {
    fiber_new(entry, argc, argv, fiber::FiberKind::Coroutine)
}

fn fiber_new(entry: *const c_uchar, argc: usize, argv: *const i64, kind: fiber::FiberKind) -> i64 {
    if entry.is_null() {
        return exc::raise_with_message(exc::RUNTIME_ERROR, "fiber entry point is NULL");
    }
    let Some(args) = copy_fiber_args(argc, argv) else {
        return exc::raise_with_message(exc::RUNTIME_ERROR, "fiber argument buffer is invalid");
    };
    let created = match kind {
        fiber::FiberKind::Generator => {
            value::fiber_generator_new(entry as usize, String::new(), args)
        }
        fiber::FiberKind::Coroutine => {
            value::fiber_coroutine_new(entry as usize, String::new(), args)
        }
    };
    match created {
        Some(fiber) => fiber,
        None => exc::raise_with_message(
            exc::RUNTIME_ERROR,
            "cannot allocate a generator/coroutine stack",
        ),
    }
}

/// `await value`: drives a coroutine object to completion. Values that are
/// not fibers are returned unchanged.
#[no_mangle]
pub extern "C" fn pyx_await(value: i64) -> i64 {
    fiber::await_value(value)
}

/// `yield value`: suspends the running generator and returns the value sent
/// on resume (`None` for plain iteration). Outside a running generator this
/// raises `RuntimeError`.
#[no_mangle]
pub extern "C" fn pyx_yield(value: i64) -> i64 {
    fiber::yield_value(value)
}

/// `yield from value`: delegates iteration to `value`. Each delegated item is
/// yielded through the running generator; `send` values, `throw` exceptions
/// and `close` are forwarded to a delegated generator. Returns the delegate's
/// return value (`None` for a plain iterator).
#[no_mangle]
pub extern "C" fn pyx_yield_from(value: i64) -> i64 {
    fiber::yield_from_value(value)
}

/// Creates a function value for a builtin referenced as a first-class value.
/// The name is the Python-visible spelling (`int`, `map`, ...); unknown names
/// raise `AttributeError` and return `None`.
#[no_mangle]
pub extern "C" fn pyx_builtin_function(name_ptr: *const c_uchar, name_len: usize) -> i64 {
    let name = function_name(name_ptr, name_len);
    match pyx_ir::Builtin::from_name(&name) {
        Some(builtin) => value::builtin_function_new(builtin),
        None => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("unknown builtin function '{name}'"),
        ),
    }
}

/// `fn(argc, argv)`: indirect call of a function value.
#[no_mangle]
pub extern "C" fn pyx_call(function: i64, argc: usize, argv: *const i64) -> i64 {
    builtins::call(function, argc, argv)
}

/// `map(function, iterable)`: materialised list of results.
#[no_mangle]
pub extern "C" fn pyx_map(function: i64, iterable: i64) -> i64 {
    builtins::map(function, iterable)
}

/// `filter(function_or_none, iterable)`: materialised filtered list.
#[no_mangle]
pub extern "C" fn pyx_filter(function: i64, iterable: i64) -> i64 {
    builtins::filter(function, iterable)
}

// ---------------------------------------------------------------------
// Builtin modules (Phase 2.10)
// ---------------------------------------------------------------------

/// `module.function(args...)` for a name from `pyx_ir::stdlib`.
///
/// Names are decoded as UTF-8, arity is validated against the central
/// spec, and unknown modules/members raise `AttributeError`. Returns the
/// result value or `None` with a pending exception.
#[no_mangle]
pub extern "C" fn pyx_module_call(
    module: *const c_uchar,
    module_len: usize,
    function: *const c_uchar,
    function_len: usize,
    argc: usize,
    argv: *const i64,
) -> i64 {
    modules::module_call(module, module_len, function, function_len, argc, argv)
}

/// `module.name`: a constant value or a native function object.
///
/// Native functions carry their arity from the central spec and are
/// callable through `pyx_call`/`pyx_map`/`pyx_filter`. Unknown names raise
/// `AttributeError` and return `None`.
#[no_mangle]
pub extern "C" fn pyx_module_get(
    module: *const c_uchar,
    module_len: usize,
    name: *const c_uchar,
    name_len: usize,
) -> i64 {
    modules::module_get(module, module_len, name, name_len)
}

// ---------------------------------------------------------------------
// CPython limited-API extensions (P4-04)
// ---------------------------------------------------------------------

/// `extension.function(args...)` for a loaded `abi3` extension module.
///
/// `path` is the shared-object path resolved by the frontend and `member` the
/// Python attribute name. Unknown modules/attributes raise `AttributeError`,
/// call failures propagate as pending exceptions. Returns the result value or
/// `None` with a pending exception.
#[no_mangle]
pub extern "C" fn pyx_extension_call(
    path: *const c_uchar,
    path_len: usize,
    member: *const c_uchar,
    member_len: usize,
    argc: usize,
    argv: *const i64,
) -> i64 {
    modules::extension::call(path, path_len, member, member_len, argc, argv)
}

/// `extension.member`: a scalar value or a callable function object.
///
/// Callables become `KIND_EXT_FUNCTION` values and remain callable through
/// `pyx_call`. Unknown names raise `AttributeError`; returns `None` with a
/// pending exception.
#[no_mangle]
pub extern "C" fn pyx_extension_get(
    path: *const c_uchar,
    path_len: usize,
    member: *const c_uchar,
    member_len: usize,
) -> i64 {
    modules::extension::get(path, path_len, member, member_len)
}

// ---------------------------------------------------------------------
// Comprehension builders
// ---------------------------------------------------------------------

/// Appends a list-comprehension element. Non-lists raise `TypeError`;
/// always returns `None` otherwise.
#[no_mangle]
pub extern "C" fn pyx_list_append(list: i64, value: i64) -> i64 {
    container::list_append(list, value)
}

/// Adds a set-comprehension element unless already present. Unhashable
/// values and non-sets raise `TypeError`; returns `None`.
#[no_mangle]
pub extern "C" fn pyx_set_add(set: i64, value: i64) -> i64 {
    container::set_add(set, value)
}

/// `dict[key] = value` for a dict-comprehension result: an existing key
/// keeps its first position and takes the last value. Unhashable keys and
/// non-dicts raise `TypeError`; returns `None`.
#[no_mangle]
pub extern "C" fn pyx_dict_set(dict: i64, key: i64, value: i64) -> i64 {
    container::dict_set(dict, key, value)
}

// ---------------------------------------------------------------------
// Indexing and iteration
// ---------------------------------------------------------------------

/// `object[index]` for lists, strings, dicts and tuples.
#[no_mangle]
pub extern "C" fn pyx_subscript(obj: i64, index: i64) -> i64 {
    container::subscript(obj, index)
}

/// `object[index] = value` for lists and dicts. Returns `None`; errors are
/// reported through the pending exception.
#[no_mangle]
pub extern "C" fn pyx_subscript_set(obj: i64, index: i64, new_value: i64) -> i64 {
    container::subscript_set(obj, index, new_value)
}

/// `object[start:stop:step]` for lists, strings and tuples. A tagged `None` in any
/// bound position means the argument was omitted. Errors are reported
/// through the pending exception and yield `None`.
#[no_mangle]
pub extern "C" fn pyx_slice(obj: i64, start: i64, stop: i64, step: i64) -> i64 {
    container::slice(obj, start, stop, step)
}

/// `obj.name` for a data attribute of a native value or class instance.
/// Unknown receivers and names raise `AttributeError`; the runtime never
/// aborts.
#[no_mangle]
pub extern "C" fn pyx_getattr(obj: i64, name: *const c_uchar, name_len: usize) -> i64 {
    methods::getattr(obj, name, name_len)
}

/// `obj.name = value` for class instances. Class namespaces are immutable;
/// assigning to a class raises `TypeError`, other receivers `AttributeError`.
/// The stored value is published with a GC insertion barrier; returns `None`
/// on success.
#[no_mangle]
pub extern "C" fn pyx_setattr(
    obj: i64,
    name: *const c_uchar,
    name_len: usize,
    new_value: i64,
) -> i64 {
    methods::setattr(obj, name, name_len, new_value)
}

/// Dynamic method call: `obj.<name>(args...)`. Dispatch is by receiver
/// kind; unknown methods raise `AttributeError`, never abort the process.
#[no_mangle]
pub extern "C" fn pyx_method(
    obj: i64,
    name: *const c_uchar,
    name_len: usize,
    argc: usize,
    argv: *const i64,
) -> i64 {
    methods::call(obj, name, name_len, argc, argv)
}

/// Dynamic method call with keyword arguments:
/// `obj.<name>(args..., name=value...)`. `kw_argv` holds `2 * kwargc` tagged
/// values in `[name, value, ...]` order. Without keywords this behaves like
/// [`pyx_method`].
#[no_mangle]
pub extern "C" fn pyx_method_kw(
    obj: i64,
    name: *const c_uchar,
    name_len: usize,
    argc: usize,
    argv: *const i64,
    kwargc: usize,
    kw_argv: *const i64,
) -> i64 {
    methods::call_kw(obj, name, name_len, argc, argv, kwargc, kw_argv)
}

/// Creates an iterator over a list, string, dict, tuple or generator object.
#[no_mangle]
pub extern "C" fn pyx_iter_new(obj: i64) -> i64 {
    iter::iter_new(obj)
}

/// Writes the next element to `out` and returns `1`, or returns `0` at the
/// end. `NULL` out-pointers are rejected without writing.
#[no_mangle]
pub extern "C" fn pyx_iter_next(iterator: i64, out: *mut i64) -> i64 {
    iter::iter_next(iterator, out)
}

/// `iter(obj)`: an existing iterator/generator is returned unchanged,
/// otherwise a new iterator is created. Non-iterables raise `TypeError`.
#[no_mangle]
pub extern "C" fn pyx_iter(obj: i64) -> i64 {
    iter::iter_of(obj)
}

/// `next(obj[, default])`: the next element of an iterator/generator.
/// `has_default` is `1` when the tagged `default` argument was supplied.
/// Exhaustion raises `StopIteration` without a default.
#[no_mangle]
pub extern "C" fn pyx_next(obj: i64, default: i64, has_default: i64) -> i64 {
    iter::next_value(obj, default, has_default != 0)
}

// ---------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn pyx_binop(op: u32, a: i64, b: i64) -> i64 {
    ops::binop(op, a, b)
}

#[no_mangle]
pub extern "C" fn pyx_unop(op: u32, a: i64) -> i64 {
    ops::unop(op, a)
}

/// Branch helper: returns `1` for truthy and `0` for falsy (C-style),
/// unlike `pyx_bool`, which returns the tagged `True`/`False` values.
#[no_mangle]
pub extern "C" fn pyx_truthy(v: i64) -> i64 {
    if value::truthy(v) {
        1
    } else {
        0
    }
}

// ---------------------------------------------------------------------
// Builtins
// ---------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn pyx_print(argc: usize, argv: *const i64) -> i64 {
    builtins::print(argc, argv)
}

#[no_mangle]
pub extern "C" fn pyx_len(v: i64) -> i64 {
    builtins::len(v)
}

#[no_mangle]
pub extern "C" fn pyx_to_str(v: i64) -> i64 {
    builtins::to_str(v)
}

#[no_mangle]
pub extern "C" fn pyx_to_int(v: i64) -> i64 {
    builtins::to_int(v)
}

#[no_mangle]
pub extern "C" fn pyx_to_float(v: i64) -> i64 {
    builtins::to_float(v)
}

#[no_mangle]
pub extern "C" fn pyx_abs(v: i64) -> i64 {
    builtins::abs(v)
}

/// Copies the static format spec passed by generated code. A missing buffer
/// is treated as the empty spec.
fn format_spec(ptr: *const c_uchar, len: usize) -> String {
    if ptr.is_null() || len == 0 || len > isize::MAX as usize {
        return String::new();
    }
    // SAFETY: the ABI contract requires `ptr..ptr+len` to be readable; the
    // length check above keeps the slice representable.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// Formats one f-string field: `value` with the static `spec` and a
/// conversion flag (`0` none, `1` `!s`, `2` `!r`, `3` `!a`).
///
/// Invalid specs raise `ValueError`; wrong operand types raise `TypeError`.
#[no_mangle]
pub extern "C" fn pyx_format_value(
    value: i64,
    spec: *const c_uchar,
    spec_len: usize,
    conversion: u8,
) -> i64 {
    let spec = format_spec(spec, spec_len);
    match format::format_value(value, &spec, conversion) {
        Ok(text) => value::str_alloc(text),
        Err(error) => exc::raise_with_message(error.tag, &error.message),
    }
}

// ---------------------------------------------------------------------
// Source location and frames
// ---------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn pyx_set_file(ptr: *const c_uchar, len: usize) {
    exc::set_file_ptr(ptr, len);
}

#[no_mangle]
pub extern "C" fn pyx_set_line(line: u32) {
    exc::set_line(line);
}

#[no_mangle]
pub extern "C" fn pyx_frame_enter(name: *const c_uchar, len: usize) {
    exc::frame_enter_ptr(name, len);
}

#[no_mangle]
pub extern "C" fn pyx_frame_leave() {
    exc::frame_leave();
}

// ---------------------------------------------------------------------
// Exceptions
// ---------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn pyx_exc_pending() -> i64 {
    if exc::pending() {
        1
    } else {
        0
    }
}

#[no_mangle]
pub extern "C" fn pyx_exc_tag() -> i64 {
    exc::pending_tag()
}

#[no_mangle]
pub extern "C" fn pyx_exc_value() -> i64 {
    exc::pending_value()
}

#[no_mangle]
pub extern "C" fn pyx_exc_clear() -> i64 {
    exc::clear()
}

#[no_mangle]
pub extern "C" fn pyx_raise(tag: u32, msg: i64) -> i64 {
    exc::raise(tag, msg)
}

#[no_mangle]
pub extern "C" fn pyx_raise_msg(tag: u32, ptr: *const c_uchar, len: usize) -> i64 {
    exc::raise_msg_ptr(tag, ptr, len)
}

#[no_mangle]
pub extern "C" fn pyx_raise_value(v: i64) -> i64 {
    exc::raise_value(v)
}

#[no_mangle]
pub extern "C" fn pyx_uncaught() -> i64 {
    exc::uncaught()
}

// ---------------------------------------------------------------------
// Garbage collector (ARCHITECTURE §7)
// ---------------------------------------------------------------------

/// Forces a conservative collection and returns the number of objects
/// freed by this call. In shared-heap mode this stops the other threads
/// at their safepoints; it returns `0` when another collector is already
/// running.
#[no_mangle]
pub extern "C" fn pyx_gc_collect() -> i64 {
    heap::collect()
}

/// Cooperative GC safepoint (Phase 2.13).
///
/// Generated code calls this at every function entry and loop backedge.
/// The fast path is a single atomic load; if a collection is requested,
/// the calling thread saves its context, parks and waits for the sweep to
/// finish. The runtime also calls it before every allocation and every
/// blocking operation, so native wrappers need not emit it themselves.
#[no_mangle]
pub extern "C" fn pyx_safepoint() {
    heap::safepoint()
}

/// Number of live heap objects registered for this thread, or for the
/// process once `_thread` has switched to the shared heap.
#[no_mangle]
pub extern "C" fn pyx_gc_objects() -> i64 {
    heap::objects() as i64
}

#[no_mangle]
pub extern "C" fn pyx_runtime_init() {
    // Order matters: resetting the exception state drops the pending
    // tagged value before the collector frees the objects it may point
    // at, so no dangling root survives into the next program run.
    //
    // Called once at program entry. Spawned `_thread` threads must never
    // call it: it frees the whole heap (including objects other threads
    // may still reach) and resets the shared-heap switch.
    crate::process::reset_start_method();
    exc::init();
    heap::reset();
    // Anchor the CPython shim so its symbols are linked into AOT binaries and
    // never dropped as unreachable (JIT links the same runtime object).
    crate::capi::anchor();
}

// ---------------------------------------------------------------------
// Spawn start method
// ---------------------------------------------------------------------

/// Registers a generated wrapper for the `spawn` start method.
///
/// Called by `pyx_main` after `pyx_runtime_init` and before the module
/// body, once per program function except `<module>`.
#[no_mangle]
pub extern "C" fn pyx_register_function(
    name_ptr: *const c_uchar,
    name_len: usize,
    wrapper: *const u8,
    arity: usize,
) {
    crate::spawn::register_from_abi(name_ptr, name_len, wrapper as usize, arity);
}

/// Runs a spawned child's target without executing the module body.
///
/// Returns `-1` when `PYX_SPAWN_PAYLOAD` is absent (normal program start),
/// otherwise the child exit code (`0` success, `1` error).
#[no_mangle]
pub extern "C" fn pyx_spawn_child() -> i32 {
    crate::spawn::spawn_child()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_and_constructors() {
        pyx_runtime_init();
        assert_eq!(pyx_none(), 1);
        assert_eq!(pyx_false(), 9);
        assert_eq!(pyx_true(), 17);
        assert_eq!(pyx_bool(pyx_none()), pyx_false());
        assert_eq!(pyx_bool(pyx_int(0)), pyx_false());
        assert_eq!(pyx_bool(pyx_int(3)), pyx_true());
        assert_eq!(pyx_int(pyx_int(-4)), pyx_int(-4));

        let s = pyx_str_new(b"hi".as_ptr(), 2);
        assert_eq!(value::as_str(s), Some("hi"));
        assert_eq!(value::as_float(pyx_float_new(1.5)), Some(1.5));
    }

    #[test]
    fn operator_entry_points() {
        pyx_runtime_init();
        let two = pyx_int(2);
        let ten = pyx_int(10);
        let result = pyx_binop(ops::OP_POW, two, ten);
        assert_eq!(value::as_integer(result), Some(1024));

        let half = pyx_binop(ops::OP_DIV, pyx_int(1), two);
        assert_eq!(value::as_float(half), Some(0.5));

        assert_eq!(pyx_truthy(pyx_none()), 0);
        assert_eq!(pyx_truthy(pyx_str_new(b"x".as_ptr(), 1)), 1);
    }

    #[test]
    fn container_entry_points() {
        pyx_runtime_init();
        let items = [pyx_int(1), pyx_int(2)];
        let list = pyx_list_new(items.len(), items.as_ptr());
        assert_eq!(pyx_len(list), pyx_int(2));
        assert_eq!(pyx_subscript(list, pyx_int(-1)), pyx_int(2));
        assert_eq!(pyx_subscript_set(list, pyx_int(0), pyx_int(7)), pyx_none());
        assert_eq!(pyx_subscript(list, pyx_int(0)), pyx_int(7));
        assert_eq!(pyx_exc_pending(), 0);

        let keys = [pyx_str_new(b"k".as_ptr(), 1)];
        let values = [pyx_int(9)];
        let dict = pyx_dict_new(1, keys.as_ptr(), values.as_ptr());
        assert_eq!(pyx_subscript(dict, keys[0]), pyx_int(9));
        assert_eq!(pyx_len(dict), pyx_int(1));
        assert_eq!(pyx_exc_pending(), 0);

        let iter = pyx_iter_new(list);
        let mut out = 0i64;
        assert_eq!(pyx_iter_next(iter, &mut out), 1);
        assert_eq!(out, pyx_int(7));
        assert_eq!(pyx_iter_next(iter, &mut out), 1);
        assert_eq!(out, pyx_int(2));
        assert_eq!(pyx_iter_next(iter, &mut out), 0);
        assert_eq!(pyx_exc_pending(), 0);

        assert_eq!(pyx_iter_next(pyx_int(1), &mut out), 0);
        assert_eq!(pyx_exc_pending(), 1);
        pyx_exc_clear();
    }

    #[test]
    fn tuple_entry_points() {
        pyx_runtime_init();
        let items = [pyx_int(1), pyx_int(2)];
        let t = pyx_tuple_new(items.len(), items.as_ptr());
        assert_eq!(value::type_name(t), "tuple");
        assert_eq!(pyx_len(t), pyx_int(2));
        assert_eq!(pyx_subscript(t, pyx_int(-1)), pyx_int(2));
        assert_eq!(
            value::to_string(pyx_slice(t, pyx_int(1), pyx_none(), pyx_none())),
            "(2,)"
        );

        let mut out = 0i64;
        let iter = pyx_iter_new(t);
        assert_eq!(pyx_iter_next(iter, &mut out), 1);
        assert_eq!(out, pyx_int(1));
        assert_eq!(pyx_iter_next(iter, &mut out), 1);
        assert_eq!(out, pyx_int(2));
        assert_eq!(pyx_iter_next(iter, &mut out), 0);

        assert_eq!(pyx_unpack(t, 2), pyx_none());
        assert_eq!(pyx_exc_pending(), 0);

        assert_eq!(pyx_unpack(t, 1), pyx_none());
        assert_eq!(pyx_exc_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "too many values to unpack (expected 1)"
        );
        pyx_exc_clear();

        // A `NULL` buffer never panics and yields the empty tuple.
        let empty = pyx_tuple_new(1, std::ptr::null());
        assert_eq!(value::to_string(empty), "()");
        assert_eq!(pyx_exc_pending(), 0);
    }

    fn make_list(items: &[i64]) -> i64 {
        let tagged: Vec<i64> = items.iter().map(|n| pyx_int(*n)).collect();
        pyx_list_new(tagged.len(), tagged.as_ptr())
    }

    fn list_ints(v: i64) -> Vec<i64> {
        value::as_list(v)
            .expect("list value")
            .iter()
            .map(|item| value::as_integer(*item).expect("integer item"))
            .collect()
    }

    #[test]
    fn slice_entry_points() {
        pyx_runtime_init();

        let list = make_list(&[0, 1, 2, 3, 4]);
        assert_eq!(
            list_ints(pyx_slice(list, pyx_int(1), pyx_int(4), pyx_none())),
            vec![1, 2, 3]
        );
        assert_eq!(
            list_ints(pyx_slice(list, pyx_none(), pyx_none(), pyx_int(-1))),
            vec![4, 3, 2, 1, 0]
        );
        assert_eq!(
            list_ints(pyx_slice(list, pyx_none(), pyx_none(), pyx_int(2))),
            vec![0, 2, 4]
        );
        // Negative bounds.
        assert_eq!(
            list_ints(pyx_slice(list, pyx_int(-3), pyx_int(-1), pyx_none())),
            vec![2, 3]
        );
        // Clamping on both sides.
        assert_eq!(
            list_ints(pyx_slice(list, pyx_int(-100), pyx_int(100), pyx_int(2))),
            vec![0, 2, 4]
        );
        assert_eq!(
            list_ints(pyx_slice(list, pyx_int(100), pyx_none(), pyx_int(-2))),
            vec![4, 2, 0]
        );
        // Empty results.
        assert!(list_ints(pyx_slice(list, pyx_int(10), pyx_none(), pyx_none())).is_empty());
        assert!(list_ints(pyx_slice(list, pyx_none(), pyx_int(-100), pyx_none())).is_empty());
        assert!(list_ints(pyx_slice(list, pyx_int(2), pyx_int(2), pyx_none())).is_empty());
        assert!(list_ints(pyx_slice(list, pyx_int(0), pyx_int(0), pyx_int(-1))).is_empty());
        // Booleans are integers.
        assert_eq!(
            list_ints(pyx_slice(list, pyx_true(), pyx_int(3), pyx_none())),
            vec![1, 2]
        );
        assert_eq!(pyx_exc_pending(), 0);

        // String slices use Unicode scalar indices.
        let text = "aé😀z";
        let s = pyx_str_new(text.as_ptr(), text.len());
        assert_eq!(
            value::as_str(pyx_slice(s, pyx_int(1), pyx_int(3), pyx_none())),
            Some("é😀")
        );
        assert_eq!(
            value::as_str(pyx_slice(s, pyx_none(), pyx_none(), pyx_int(-1))),
            Some("z😀éa")
        );
        assert_eq!(
            value::as_str(pyx_slice(s, pyx_none(), pyx_none(), pyx_int(2))),
            Some("a😀")
        );
        assert_eq!(
            value::as_str(pyx_slice(s, pyx_int(3), pyx_none(), pyx_none())),
            Some("z")
        );
        assert_eq!(
            value::as_str(pyx_slice(s, pyx_int(9), pyx_none(), pyx_none())),
            Some("")
        );
        assert_eq!(pyx_exc_pending(), 0);
    }

    #[test]
    fn slice_results_are_independent() {
        pyx_runtime_init();

        let source = make_list(&[1, 2, 3]);
        let sliced = pyx_slice(source, pyx_none(), pyx_none(), pyx_none());
        assert_ne!(source, sliced);

        // Mutating the source must not affect the slice, and vice versa.
        pyx_subscript_set(source, pyx_int(0), pyx_int(9));
        assert_eq!(list_ints(sliced), vec![1, 2, 3]);
        pyx_subscript_set(sliced, pyx_int(1), pyx_int(8));
        assert_eq!(list_ints(source), vec![9, 2, 3]);
        assert_eq!(pyx_exc_pending(), 0);
    }

    #[test]
    fn slice_errors() {
        pyx_runtime_init();
        let list = make_list(&[1, 2, 3]);

        assert_eq!(
            pyx_slice(list, pyx_none(), pyx_none(), pyx_int(0)),
            pyx_none()
        );
        assert_eq!(pyx_exc_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "slice step cannot be zero"
        );
        pyx_exc_clear();

        assert_eq!(
            pyx_slice(list, pyx_float_new(1.0), pyx_none(), pyx_none()),
            pyx_none()
        );
        assert_eq!(pyx_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "slice indices must be integers or None or have an __index__ method"
        );
        pyx_exc_clear();

        let keys = [pyx_str_new(b"k".as_ptr(), 1)];
        let values = [pyx_int(1)];
        let dict = pyx_dict_new(1, keys.as_ptr(), values.as_ptr());
        assert_eq!(
            pyx_slice(dict, pyx_none(), pyx_none(), pyx_none()),
            pyx_none()
        );
        assert_eq!(pyx_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "unhashable type: 'slice'"
        );
        pyx_exc_clear();

        assert_eq!(
            pyx_slice(pyx_int(5), pyx_none(), pyx_none(), pyx_none()),
            pyx_none()
        );
        assert_eq!(pyx_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "'int' object is not subscriptable"
        );
        pyx_exc_clear();
        assert_eq!(pyx_exc_pending(), 0);
    }

    #[test]
    fn gc_entry_points() {
        pyx_runtime_init();
        assert_eq!(pyx_gc_objects(), 0);

        let s = pyx_str_new(b"gc".as_ptr(), 2);
        let f = pyx_float_new(1.0);
        // At least the most recent allocation is live. Conservative
        // scanning may keep older garbage, so no exact count.
        assert!(pyx_gc_objects() >= 1);

        let before = pyx_gc_objects();
        let freed = pyx_gc_collect();
        assert!(freed >= 0 && freed <= before);
        // `s` and `f` are used after the forced collection, so they are
        // roots and must have survived.
        assert_eq!(value::as_str(s), Some("gc"));
        assert_eq!(value::as_float(f), Some(1.0));
        assert!(pyx_gc_objects() >= 2);

        pyx_runtime_init();
        assert_eq!(pyx_gc_objects(), 0);
    }

    #[test]
    fn set_entry_points() {
        pyx_runtime_init();
        let items = [pyx_int(1), pyx_int(2), pyx_int(2), pyx_int(3)];
        let s = pyx_set_new(items.len(), items.as_ptr());
        assert_eq!(value::type_name(s), "set");
        assert_eq!(pyx_len(s), pyx_int(3));
        assert_eq!(pyx_binop(ops::OP_IN, pyx_int(2), s), pyx_true());
        assert_eq!(pyx_binop(ops::OP_NOT_IN, pyx_int(9), s), pyx_true());

        // set(iterable) from list, string and dict.
        let list = make_list(&[3, 1, 3, 2]);
        let from_list = pyx_set_from(list);
        assert_eq!(value::as_set(from_list).map(|i| i.len()), Some(3));

        let text = "aba";
        let from_str = pyx_set_from(pyx_str_new(text.as_ptr(), text.len()));
        assert_eq!(value::as_set(from_str).map(|i| i.len()), Some(2));

        let keys = [pyx_str_new(b"k".as_ptr(), 1), pyx_str_new(b"j".as_ptr(), 1)];
        let values = [pyx_int(1), pyx_int(2)];
        let dict = pyx_dict_new(2, keys.as_ptr(), values.as_ptr());
        let from_dict = pyx_set_from(dict);
        let from_dict = value::as_set(from_dict).expect("set");
        assert_eq!(from_dict.len(), 2);

        assert_eq!(pyx_set_from(pyx_int(3)), pyx_none());
        assert_eq!(pyx_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "'int' object is not iterable"
        );
        pyx_exc_clear();
    }

    #[test]
    fn gc_traces_set_elements() {
        pyx_runtime_init();
        let alpha = pyx_str_new(b"alpha".as_ptr(), 5);
        let beta = pyx_str_new(b"beta".as_ptr(), 4);
        let items = [alpha, beta];
        let s = pyx_set_new(items.len(), items.as_ptr());
        assert_eq!(value::as_set(s).map(|i| i.len()), Some(2));

        // Allocate unrelated garbage so the collector has sweeping work
        // while the set (and its string elements) are live.
        for _ in 0..64 {
            let _ = pyx_str_new(b"garbage".as_ptr(), 7);
        }
        pyx_gc_collect();

        let elements = value::as_set(s).expect("set survived collection");
        assert_eq!(elements.len(), 2);
        assert_eq!(value::as_str(elements[0]), Some("alpha"));
        assert_eq!(value::as_str(elements[1]), Some("beta"));
        assert_eq!(value::as_str(alpha), Some("alpha"));
        assert_eq!(value::as_str(beta), Some("beta"));
    }

    #[test]
    fn builtins_entry_points() {
        pyx_runtime_init();
        let r = pyx_range_new(pyx_int(0), pyx_int(5), pyx_int(1));
        assert_eq!(value::type_name(r), "range");
        assert_eq!(value::to_string(r), "range(0, 5)");
        assert_eq!(pyx_len(r), pyx_int(5));

        let mut out = 0i64;
        let iter = pyx_iter_new(r);
        for expected in [0, 1, 2, 3, 4] {
            assert_eq!(pyx_iter_next(iter, &mut out), 1);
            assert_eq!(out, pyx_int(expected));
        }
        assert_eq!(pyx_iter_next(iter, &mut out), 0);
        assert_eq!(pyx_exc_pending(), 0);

        assert_eq!(value::to_string(pyx_list_from(r)), "[0, 1, 2, 3, 4]");
        assert_eq!(pyx_sum(r, pyx_int(0)), pyx_int(10));
        assert_eq!(pyx_sum(r, pyx_int(100)), pyx_int(110));

        let source = make_list(&[1, 2]);
        let enumerated = pyx_enumerate(source, pyx_int(10));
        assert_eq!(value::to_string(enumerated), "[(10, 1), (11, 2)]");

        let right = make_list(&[3, 4]);
        let sources = [source, right];
        let zipped = pyx_zip(sources.len(), sources.as_ptr());
        assert_eq!(value::to_string(zipped), "[(1, 3), (2, 4)]");
        assert_eq!(pyx_exc_pending(), 0);

        let key = pyx_str_new(b"a".as_ptr(), 1);
        let pair1 = pyx_tuple_new(2, [key, pyx_int(1)].as_ptr());
        let pair2 = pyx_tuple_new(2, [pyx_int(2), value::str_from_bytes(b"b")].as_ptr());
        let pair_items = [pair1, pair2];
        let pair_list = pyx_list_new(pair_items.len(), pair_items.as_ptr());
        assert_eq!(
            value::to_string(pyx_dict_from(pair_list)),
            "{'a': 1, 2: 'b'}"
        );
        assert_eq!(pyx_exc_pending(), 0);

        assert_eq!(
            pyx_range_new(pyx_int(0), pyx_int(5), pyx_int(0)),
            pyx_none()
        );
        assert_eq!(pyx_exc_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "range() arg 3 must not be zero"
        );
        pyx_exc_clear();

        assert_eq!(pyx_list_from(pyx_int(1)), pyx_none());
        assert_eq!(pyx_exc_tag(), exc::TYPE_ERROR as i64);
        pyx_exc_clear();
        assert_eq!(pyx_dict_from(pyx_int(1)), pyx_none());
        assert_eq!(pyx_exc_tag(), exc::TYPE_ERROR as i64);
        pyx_exc_clear();
    }

    #[test]
    fn builtins_survive_gc() {
        pyx_runtime_init();
        let r = pyx_range_new(pyx_int(0), pyx_int(4), pyx_int(1));
        let listed = pyx_list_from(r);
        let summed = pyx_sum(r, pyx_int(0));
        let source_items = [pyx_str_new(b"a".as_ptr(), 1)];
        let source = pyx_list_new(source_items.len(), source_items.as_ptr());
        let enumerated = pyx_enumerate(source, pyx_int(0));
        let sources = [source];
        let zipped = pyx_zip(1, sources.as_ptr());

        for _ in 0..64 {
            let _ = pyx_str_new(b"garbage".as_ptr(), 7);
        }
        pyx_gc_collect();

        assert_eq!(value::to_string(listed), "[0, 1, 2, 3]");
        assert_eq!(summed, pyx_int(6));
        assert_eq!(value::to_string(enumerated), "[(0, 'a')]");
        assert_eq!(value::to_string(zipped), "[('a',)]");
        assert_eq!(pyx_exc_pending(), 0);
    }

    // ---- first-class functions (Phase 2.9) -------------------------

    extern "C" fn add_wrapper(argv: *const i64) -> i64 {
        // SAFETY: the caller guarantees two readable arguments.
        let args = unsafe { std::slice::from_raw_parts(argv, 2) };
        pyx_binop(ops::OP_ADD, args[0], args[1])
    }

    extern "C" fn double_wrapper(argv: *const i64) -> i64 {
        // SAFETY: `map` always passes exactly one argument.
        let arg = unsafe { *argv };
        pyx_binop(ops::OP_MUL, arg, pyx_int(2))
    }

    extern "C" fn is_even_wrapper(argv: *const i64) -> i64 {
        // SAFETY: `filter` always passes exactly one argument.
        let arg = unsafe { *argv };
        let remainder = pyx_binop(ops::OP_MOD, arg, pyx_int(2));
        pyx_binop(ops::OP_EQ, remainder, pyx_int(0))
    }

    extern "C" fn zero_wrapper(_argv: *const i64) -> i64 {
        pyx_int(42)
    }

    extern "C" fn fail_wrapper(_argv: *const i64) -> i64 {
        pyx_raise_msg(exc::TYPE_ERROR, b"boom".as_ptr(), 4)
    }

    fn function(wrapper: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
        pyx_function_new(wrapper as *const u8, arity, name.as_ptr(), name.len(), 0)
    }

    #[test]
    fn function_values_and_indirect_calls() {
        pyx_runtime_init();
        let add = function(add_wrapper, 2, "add");
        assert_eq!(value::type_name(add), "function");
        assert!(value::truthy(add));
        assert_eq!(value::to_string(add), "<function add>");
        assert_eq!(value::repr(add), "<function add>");

        let args = [pyx_int(2), pyx_int(3)];
        assert_eq!(pyx_call(add, 2, args.as_ptr()), pyx_int(5));
        assert_eq!(pyx_exc_pending(), 0);

        // A zero-argument call passes `NULL` to the wrapper.
        let zero = pyx_function_new(zero_wrapper as *const u8, 0, b"zero".as_ptr(), 4, 0);
        assert_eq!(pyx_call(zero, 0, std::ptr::null()), pyx_int(42));
        assert_eq!(pyx_exc_pending(), 0);

        // Missing name buffers fall back to `<function>`.
        let unnamed = pyx_function_new(add_wrapper as *const u8, 2, std::ptr::null(), 0, 0);
        assert_eq!(value::to_string(unnamed), "<function <function>>");

        // Functions are ordinary heap values in containers.
        let suite = pyx_list_new(2, [add, zero].as_ptr());
        assert_eq!(
            value::type_name(pyx_subscript(suite, pyx_int(1))),
            "function"
        );
        assert_eq!(
            pyx_call(pyx_subscript(suite, pyx_int(0)), 2, args.as_ptr()),
            pyx_int(5)
        );
        assert_eq!(pyx_len(suite), pyx_int(2));
        assert_eq!(pyx_exc_pending(), 0);
    }

    #[test]
    fn function_call_errors() {
        pyx_runtime_init();
        let add = function(add_wrapper, 2, "add");
        let args = [pyx_int(1)];

        assert_eq!(pyx_call(add, 1, args.as_ptr()), pyx_none());
        assert_eq!(pyx_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "function takes 2 arguments (1 given)"
        );
        pyx_exc_clear();

        assert_eq!(pyx_call(add, 2, std::ptr::null()), pyx_none());
        assert_eq!(pyx_exc_tag(), exc::TYPE_ERROR as i64);
        pyx_exc_clear();

        assert_eq!(pyx_call(pyx_int(7), 0, std::ptr::null()), pyx_none());
        assert_eq!(pyx_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "'int' object is not callable"
        );
        pyx_exc_clear();

        assert_eq!(pyx_call(pyx_none(), 0, std::ptr::null()), pyx_none());
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "'NoneType' object is not callable"
        );
        pyx_exc_clear();

        // A null wrapper is rejected at construction.
        assert_eq!(
            pyx_function_new(std::ptr::null(), 0, b"bad".as_ptr(), 3, 0),
            pyx_none()
        );
        assert_eq!(pyx_exc_tag(), exc::RUNTIME_ERROR as i64);
        pyx_exc_clear();

        // Function values reject the existing container operations.
        assert_eq!(pyx_len(add), pyx_none());
        assert_eq!(pyx_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "object of type 'function' has no len()"
        );
        pyx_exc_clear();

        assert_eq!(pyx_subscript(add, pyx_int(0)), pyx_none());
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "'function' object is not subscriptable"
        );
        pyx_exc_clear();

        assert_eq!(
            pyx_slice(add, pyx_none(), pyx_none(), pyx_none()),
            pyx_none()
        );
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "'function' object is not subscriptable"
        );
        pyx_exc_clear();
    }

    #[test]
    fn map_and_filter_values() {
        pyx_runtime_init();
        let double = function(double_wrapper, 1, "<lambda>");
        let even = function(is_even_wrapper, 1, "<lambda>");
        let r5 = pyx_range_new(pyx_int(0), pyx_int(5), pyx_int(1));
        let r8 = pyx_range_new(
            pyx_int(0),
            value::encode_int(8).expect("tagged 8"),
            pyx_int(1),
        );

        assert_eq!(value::to_string(pyx_map(double, r5)), "[0, 2, 4, 6, 8]");
        assert_eq!(value::to_string(pyx_filter(even, r8)), "[0, 2, 4, 6]");
        assert_eq!(pyx_exc_pending(), 0);

        // filter(None, ...) uses item truthiness.
        let zero = pyx_int(0);
        let one = pyx_int(1);
        let empty_string = pyx_str_new(b"".as_ptr(), 0);
        let x = pyx_str_new(b"x".as_ptr(), 1);
        let empty_list = pyx_list_new(0, std::ptr::null());
        let one_list = pyx_list_new(1, [one].as_ptr());
        let source = pyx_list_new(
            6,
            [zero, one, empty_string, x, empty_list, one_list].as_ptr(),
        );
        let filtered = pyx_filter(pyx_none(), source);
        assert_eq!(value::to_string(filtered), "[1, 'x', [1]]");
        assert_eq!(pyx_exc_pending(), 0);

        // map accepts any iterable, e.g. a list.
        let listed = pyx_map(
            double,
            pyx_list_new(3, [pyx_int(1), pyx_int(2), pyx_int(3)].as_ptr()),
        );
        assert_eq!(value::to_string(listed), "[2, 4, 6]");
        assert_eq!(pyx_exc_pending(), 0);
    }

    #[test]
    fn map_and_filter_errors() {
        pyx_runtime_init();
        let source = make_list(&[1, 2, 3]);
        let double = function(double_wrapper, 1, "<lambda>");

        assert_eq!(pyx_map(pyx_int(1), source), pyx_none());
        assert_eq!(pyx_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "map() argument 1 must be callable"
        );
        pyx_exc_clear();

        assert_eq!(pyx_filter(pyx_int(1), source), pyx_none());
        assert_eq!(pyx_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "filter() argument 1 must be callable or None"
        );
        pyx_exc_clear();

        assert_eq!(pyx_map(double, pyx_int(3)), pyx_none());
        assert_eq!(pyx_exc_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "'int' object is not iterable"
        );
        pyx_exc_clear();

        assert_eq!(pyx_filter(double, pyx_int(3)), pyx_none());
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "'int' object is not iterable"
        );
        pyx_exc_clear();

        // Arity mismatches surface through the callback path.
        let binary = function(add_wrapper, 2, "add");
        assert_eq!(pyx_map(binary, source), pyx_none());
        assert_eq!(
            value::to_string(pyx_exc_value()),
            "function takes 2 arguments (1 given)"
        );
        pyx_exc_clear();
    }

    #[test]
    fn function_exceptions_propagate() {
        pyx_runtime_init();
        let fail = function(fail_wrapper, 1, "fail");
        let source = make_list(&[1, 2]);

        assert_eq!(pyx_call(fail, 1, [pyx_int(1)].as_ptr()), pyx_none());
        assert_eq!(pyx_exc_pending(), 1);
        assert_eq!(value::to_string(pyx_exc_value()), "boom");
        pyx_exc_clear();

        assert_eq!(pyx_map(fail, source), pyx_none());
        assert_eq!(pyx_exc_pending(), 1);
        assert_eq!(value::to_string(pyx_exc_value()), "boom");
        pyx_exc_clear();

        assert_eq!(pyx_filter(fail, source), pyx_none());
        assert_eq!(pyx_exc_pending(), 1);
        assert_eq!(value::to_string(pyx_exc_value()), "boom");
        pyx_exc_clear();
    }

    #[test]
    fn functions_survive_gc() {
        pyx_runtime_init();
        let add = function(add_wrapper, 2, "add");
        let double = function(double_wrapper, 1, "<lambda>");

        // Function values inside a list and a dict, plus the wrappers
        // themselves allocated as garbage, exercise the collector.
        let items = [add, double];
        let funcs = pyx_list_new(items.len(), items.as_ptr());
        let keys = [pyx_str_new(b"add".as_ptr(), 3)];
        let values = [add];
        let table = pyx_dict_new(1, keys.as_ptr(), values.as_ptr());

        for _ in 0..256 {
            let _ = function(double_wrapper, 1, "garbage");
        }
        pyx_gc_collect();

        let first = pyx_subscript(funcs, pyx_int(0));
        assert_eq!(value::type_name(first), "function");
        assert_eq!(
            pyx_call(first, 2, [pyx_int(20), pyx_int(30)].as_ptr()),
            pyx_int(50)
        );
        let second = pyx_subscript(funcs, pyx_int(1));
        assert_eq!(
            value::to_string(pyx_map(
                second,
                pyx_range_new(pyx_int(0), pyx_int(3), pyx_int(1))
            )),
            "[0, 2, 4]"
        );
        assert_eq!(value::type_name(pyx_subscript(table, keys[0])), "function");

        // Iteration yields the function values unchanged.
        let mut out = 0i64;
        let iter = pyx_iter_new(funcs);
        assert_eq!(pyx_iter_next(iter, &mut out), 1);
        assert_eq!(value::type_name(out), "function");
        assert_eq!(pyx_iter_next(iter, &mut out), 1);
        assert_eq!(value::type_name(out), "function");
        assert_eq!(pyx_iter_next(iter, &mut out), 0);
        assert_eq!(pyx_exc_pending(), 0);
    }

    #[test]
    fn exception_entry_points() {
        pyx_runtime_init();
        assert_eq!(pyx_exc_pending(), 0);
        assert_eq!(pyx_exc_tag(), -1);

        let msg = pyx_str_new(b"boom".as_ptr(), 4);
        assert_eq!(pyx_raise(exc::VALUE_ERROR, msg), pyx_none());
        assert_eq!(pyx_exc_pending(), 1);
        assert_eq!(pyx_exc_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(value::to_string(pyx_exc_value()), "boom");
        assert_eq!(pyx_exc_clear(), 0);
        assert_eq!(pyx_exc_pending(), 0);

        pyx_raise_msg(exc::ZERO_DIVISION_ERROR, b"custom".as_ptr(), 6);
        assert_eq!(value::to_string(pyx_exc_value()), "custom");

        pyx_set_file(b"test.py".as_ptr(), 7);
        pyx_set_line(7);
        pyx_frame_enter(b"main".as_ptr(), 4);
        pyx_raise(exc::ZERO_DIVISION_ERROR, pyx_none());
        assert_eq!(value::to_string(pyx_exc_value()), "division by zero");
        let data = value::exception_data(pyx_exc_value()).expect("exception");
        assert_eq!(data.file, "test.py");
        assert_eq!(data.line, 7);
        assert_eq!(data.frame, "main");

        assert_eq!(pyx_uncaught(), 1);
        assert_eq!(pyx_exc_pending(), 0);
        assert_eq!(pyx_uncaught(), 0);
    }
}
