//! Builtin functions: `print`, `len`, `str`, `int`, `float`, `abs`, `range`,
//! `list`, `dict`, `sum`, `enumerate`, `zip` (ARCHITECTURE §3.2).

use std::io::Write as _;
use std::num::IntErrorKind;

use pyx_ir::Builtin;

use crate::container;
use crate::exc::{self, OVERFLOW_ERROR, RUNTIME_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::iter;
use crate::modules;
use crate::ops;
use crate::value::{self, FunctionBody, NONE};

/// `2^63` as `f64`, the first float outside the `i64` range.
const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;

/// Renders `argv` like Python's `print`: `str()` per argument, single spaces.
fn render_args(argv: &[i64]) -> String {
    let mut out = String::new();
    for (index, value) in argv.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(&value::to_string(*value));
    }
    out
}

/// `print(*argv)`: space-separated `str()` output plus a newline, flushed.
/// Returns `None`.
pub(crate) fn print(argc: usize, argv: *const i64) -> i64 {
    let args: &[i64] = if argc == 0 || argv.is_null() {
        &[]
    } else {
        // SAFETY: the ABI contract requires `argv` to point at `argc` readable
        // tagged values.
        unsafe { std::slice::from_raw_parts(argv, argc) }
    };
    let mut text = render_args(args);
    text.push('\n');
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
    NONE
}

/// `str(v)` as a fresh string object.
pub(crate) fn to_str(v: i64) -> i64 {
    value::str_alloc(value::to_string(v))
}

/// `len(v)`: strings count Unicode scalar values, lists/tuples their elements
/// and dicts their keys.
pub(crate) fn len(v: i64) -> i64 {
    if let Some(s) = value::as_str(v) {
        return ops::int_result(s.chars().count() as i64);
    }
    if let Some(items) = value::as_list(v) {
        return ops::int_result(items.len() as i64);
    }
    if let Some(bytes) = value::as_bytes(v) {
        return ops::int_result(bytes.len() as i64);
    }
    if let Some(pairs) = value::as_dict(v) {
        return ops::int_result(pairs.len() as i64);
    }
    if let Some((pairs, _)) = value::as_defaultdict(v) {
        return ops::int_result(pairs.len() as i64);
    }
    if let Some(items) = value::as_tuple(v) {
        return ops::int_result(items.len() as i64);
    }
    if let Some(items) = value::as_set(v) {
        return ops::int_result(items.len() as i64);
    }
    if let Some((start, stop, step)) = value::as_range(v) {
        return ops::int_result(value::range_len(start, stop, step));
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!("object of type '{}' has no len()", value::type_name(v)),
    )
}

/// `int(v)`.
pub(crate) fn to_int(v: i64) -> i64 {
    if let Some(n) = value::as_integer(v) {
        return ops::int_result(n);
    }
    if let Some(f) = value::as_float(v) {
        if f.is_nan() {
            return exc::raise_with_message(VALUE_ERROR, "cannot convert float NaN to integer");
        }
        if f.is_infinite() {
            return exc::raise_with_message(
                VALUE_ERROR,
                "cannot convert float infinity to integer",
            );
        }
        if !(-TWO_POW_63..TWO_POW_63).contains(&f) {
            return exc::raise_with_message(OVERFLOW_ERROR, "integer overflow");
        }
        // `int()` truncates toward zero.
        return ops::int_result(f.trunc() as i64);
    }
    if let Some(s) = value::as_str(v) {
        return match s.trim().parse::<i64>() {
            Ok(n) => ops::int_result(n),
            Err(err) => match err.kind() {
                IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => {
                    exc::raise_with_message(OVERFLOW_ERROR, "integer overflow")
                }
                _ => exc::raise_with_message(
                    VALUE_ERROR,
                    &format!("invalid literal for int() with base 10: '{}'", s),
                ),
            },
        };
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "int() argument must be a string, a bytes-like object or a real number, not '{}'",
            value::type_name(v)
        ),
    )
}

/// `float(v)`.
pub(crate) fn to_float(v: i64) -> i64 {
    if let Some(n) = value::as_integer(v) {
        return value::float_new(n as f64);
    }
    if let Some(f) = value::as_float(v) {
        return value::float_new(f);
    }
    if let Some(s) = value::as_str(v) {
        return match s.trim().parse::<f64>() {
            Ok(f) => value::float_new(f),
            Err(_) => exc::raise_with_message(
                VALUE_ERROR,
                &format!("could not convert string to float: '{}'", s),
            ),
        };
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "float() argument must be a string or a real number, not '{}'",
            value::type_name(v)
        ),
    )
}

/// `abs(v)`.
pub(crate) fn abs(v: i64) -> i64 {
    if let Some(n) = value::as_integer(v) {
        return match n.checked_abs() {
            Some(a) => ops::int_result(a),
            None => exc::raise_with_message(OVERFLOW_ERROR, "integer overflow"),
        };
    }
    if let Some(f) = value::as_float(v) {
        return value::float_new(f.abs());
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!("bad operand type for abs(): '{}'", value::type_name(v)),
    )
}

/// Decodes a `range()` argument, raising the Python `TypeError` for
/// non-integers (`range_arg_type.py`).
fn integer_argument(v: i64) -> Option<i64> {
    match value::as_integer(v) {
        Some(n) => Some(n),
        None => {
            exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "'{}' object cannot be interpreted as an integer",
                    value::type_name(v)
                ),
            );
            None
        }
    }
}

/// `range(start, stop, step)`: builds a lazy range object. Arguments are
/// tagged integers; a zero step is a `ValueError`.
pub(crate) fn range_new(start: i64, stop: i64, step: i64) -> i64 {
    let Some(start) = integer_argument(start) else {
        return NONE;
    };
    let Some(stop) = integer_argument(stop) else {
        return NONE;
    };
    let Some(step) = integer_argument(step) else {
        return NONE;
    };
    if step == 0 {
        return exc::raise_with_message(VALUE_ERROR, "range() arg 3 must not be zero");
    }
    value::range_new(start, stop, step)
}

/// Upper bound for a `bytes` allocation requested by user input (zero-fill
/// and sequence conversion). Keeps `bytes(n)` from aborting the process on an
/// impossible allocation.
const MAX_BYTES_ALLOC: usize = 64 * 1024 * 1024;

/// Truthy for the UTF-8 encoding spellings the runtime accepts. The runtime
/// only implements UTF-8, so every other name is a decode/encode error.
pub(crate) fn is_utf8_encoding(name: &str) -> bool {
    name.eq_ignore_ascii_case("utf-8")
        || name.eq_ignore_ascii_case("utf8")
        || name.eq_ignore_ascii_case("utf_8")
        || name.eq_ignore_ascii_case("u8")
}

/// `bytes(...)`: the constructor behind [`crate::abi::pyx_bytes_build`].
///
/// Signatures handled here:
/// - `bytes()` -> `b""`
/// - `bytes(bytes)` -> copy
/// - `bytes(int)` -> that many zero bytes
/// - `bytes(list|tuple|set)` -> one integer per element (`0..=255`)
/// - `bytes(str, encoding)` -> UTF-8 bytes (only UTF-8 is supported)
///
/// Every other form reports a pending exception and returns `None`.
pub(crate) fn bytes_build(argc: usize, argv: *const i64) -> i64 {
    if argc > 2 {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("bytes() takes at most 2 arguments ({argc} given)"),
        );
    }
    if argc == 0 {
        return value::bytes_new(Vec::new());
    }
    if argv.is_null() || argc > isize::MAX as usize / std::mem::size_of::<i64>() {
        return exc::raise_with_message(
            TYPE_ERROR,
            "bytes() received arguments without an argument buffer",
        );
    }
    // SAFETY: the ABI contract requires `argv` to point at `argc` readable
    // tagged values; the length check above keeps the slice in range.
    let args = unsafe { std::slice::from_raw_parts(argv, argc) };
    if argc == 2 {
        let Some(text) = value::as_str(args[0]) else {
            return exc::raise_with_message(TYPE_ERROR, "encoding without a string argument");
        };
        let Some(encoding) = value::as_str(args[1]) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("encoding must be str, not {}", value::type_name(args[1])),
            );
        };
        if !is_utf8_encoding(encoding) {
            return exc::raise_with_message(VALUE_ERROR, &format!("unknown encoding: {encoding}"));
        }
        return value::bytes_new(text.as_bytes().to_vec());
    }
    if let Some(bytes) = value::as_bytes(args[0]) {
        return value::bytes_from_slice(bytes);
    }
    if let Some(count) = value::as_integer(args[0]) {
        if count < 0 {
            return exc::raise_with_message(VALUE_ERROR, "negative count");
        }
        let count = count as usize;
        if count > MAX_BYTES_ALLOC {
            return exc::raise_with_message(
                VALUE_ERROR,
                "bytes length exceeds the runtime allocation limit",
            );
        }
        return value::bytes_new(vec![0; count]);
    }
    let sequence = value::as_list(args[0])
        .or_else(|| value::as_tuple(args[0]))
        .or_else(|| value::as_set(args[0]));
    if let Some(items) = sequence {
        if items.len() > MAX_BYTES_ALLOC {
            return exc::raise_with_message(
                VALUE_ERROR,
                "bytes length exceeds the runtime allocation limit",
            );
        }
        let items = items.to_vec();
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let Some(byte) = value::as_integer(item) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "'{}' object cannot be interpreted as an integer",
                        value::type_name(item)
                    ),
                );
            };
            if !(0..=255).contains(&byte) {
                return exc::raise_with_message(VALUE_ERROR, "bytes must be in range(0, 256)");
            }
            out.push(byte as u8);
        }
        return value::bytes_new(out);
    }
    if value::as_str(args[0]).is_some() {
        return exc::raise_with_message(TYPE_ERROR, "string argument without an encoding");
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "cannot convert '{}' object to bytes",
            value::type_name(args[0])
        ),
    )
}

/// `list(iterable)`: materialises the runtime iteration order. The result is
/// allocated first and filled incrementally, so the collector traces every
/// element that has already been appended.
pub(crate) fn list_from(iterable: i64) -> i64 {
    let iterator = iter::iter_new(iterable);
    if exc::pending() {
        return NONE;
    }
    let result = value::list_new(Vec::new());
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        if let Some(mut items) = value::list_items_mut(result) {
            items.push(item);
            crate::heap::write_barrier_edge(result, item);
        }
    }
    result
}

/// Returns the elements of a list/tuple pair candidate.
fn pair_items(v: i64) -> Option<&'static [i64]> {
    value::as_list(v).or_else(|| value::as_tuple(v))
}

/// `dict(iterable)`: copies a dict or consumes an iterable of two-element
/// lists/tuples. Keys must be hashable; duplicate keys keep their first
/// position and the last value. The result itself roots every pair inserted
/// so far.
pub(crate) fn dict_from(iterable: i64) -> i64 {
    if let Some(pairs) = value::as_dict(iterable) {
        let result = value::dict_new(Vec::new());
        if let Some(mut dst) = value::dict_pairs_mut(result) {
            dst.extend_from_slice(pairs);
            for &(key, val) in pairs {
                crate::heap::write_barrier_edge(result, key);
                crate::heap::write_barrier_edge(result, val);
            }
        }
        return result;
    }
    let iterator = iter::iter_new(iterable);
    if exc::pending() {
        return NONE;
    }
    let result = value::dict_new(Vec::new());
    let mut index = 0usize;
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        let Some(items) = pair_items(item) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "cannot convert dictionary update sequence element #{} to a sequence",
                    index
                ),
            );
        };
        if items.len() != 2 {
            return exc::raise_with_message(
                VALUE_ERROR,
                &format!(
                    "dictionary update sequence element #{} has length {}; 2 is required",
                    index,
                    items.len()
                ),
            );
        }
        let (key, new_value) = (items[0], items[1]);
        if let Some(name) = container::unhashable_name(key) {
            return exc::raise_with_message(TYPE_ERROR, &container::unhashable_message(name));
        }
        if let Some(mut pairs) = value::dict_pairs_mut(result) {
            container::dict_insert_barrier(result, &mut pairs, key, new_value);
        }
        index += 1;
    }
    result
}

/// `sum(iterable, start)`: uses the runtime iterator protocol and `+`
/// semantics, so pending arithmetic errors propagate unchanged.
pub(crate) fn sum(iterable: i64, start: i64) -> i64 {
    let iterator = iter::iter_new(iterable);
    if exc::pending() {
        return NONE;
    }
    let mut accumulator = start;
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        accumulator = ops::binop(ops::OP_ADD, accumulator, item);
        if exc::pending() {
            return NONE;
        }
    }
    accumulator
}

/// `enumerate(iterable, start)`: a list of `(index, value)` tuples. The
/// result list is the root for the tuples built during the loop.
pub(crate) fn enumerate(iterable: i64, start: i64) -> i64 {
    let Some(mut index) = integer_argument(start) else {
        return NONE;
    };
    let iterator = iter::iter_new(iterable);
    if exc::pending() {
        return NONE;
    }
    let result = value::list_new(Vec::new());
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        let tagged_index = ops::int_result(index);
        if exc::pending() {
            return NONE;
        }
        let pair = value::tuple_new(vec![tagged_index, item]);
        if let Some(mut items) = value::list_items_mut(result) {
            items.push(pair);
            crate::heap::write_barrier_edge(result, pair);
        }
        index = match index.checked_add(1) {
            Some(next) => next,
            None => return exc::raise_with_message(OVERFLOW_ERROR, "integer overflow"),
        };
    }
    result
}

/// Copies `argc` tagged values from `argv` into a list, defensively.
fn list_buffer(argc: usize, argv: *const i64) -> &'static [i64] {
    if argc == 0 || argv.is_null() || argc > isize::MAX as usize / std::mem::size_of::<i64>() {
        return &[];
    }
    // SAFETY: the ABI contract requires `argv` to point at `argc` readable
    // tagged values; the length check above keeps the slice in range.
    unsafe { std::slice::from_raw_parts(argv, argc) }
}

/// `zip(*iterables)`: materialises tuples up to the shortest source.
///
/// Sources and their iterators are kept in a registered list (`roots`), and
/// each row is collected in a registered list before the tuple is allocated,
/// so freshly created string elements stay reachable across collections.
pub(crate) fn zip(argc: usize, argv: *const i64) -> i64 {
    if argc == 0 {
        return value::list_new(Vec::new());
    }
    let sources = list_buffer(argc, argv);
    if sources.len() != argc {
        // Defensive: malformed buffer behaves like `zip()`.
        return value::list_new(Vec::new());
    }
    let roots = value::list_new(Vec::new());
    for &source in sources {
        if let Some(mut items) = value::list_items_mut(roots) {
            items.push(source);
            crate::heap::write_barrier_edge(roots, source);
        }
    }
    for index in 0..argc {
        let Some(source) = value::as_list(roots).and_then(|items| items.get(index).copied()) else {
            return value::list_new(Vec::new());
        };
        let iterator = iter::iter_new(source);
        if exc::pending() {
            return NONE;
        }
        if let Some(mut items) = value::list_items_mut(roots) {
            items.push(iterator);
            crate::heap::write_barrier_edge(roots, iterator);
        }
    }

    let result = value::list_new(Vec::new());
    loop {
        let row = value::list_new(Vec::new());
        let mut ended = false;
        for index in 0..argc {
            let Some(iterator) =
                value::as_list(roots).and_then(|items| items.get(argc + index).copied())
            else {
                ended = true;
                break;
            };
            let mut item = NONE;
            let status = iter::iter_next(iterator, &mut item);
            if exc::pending() {
                return NONE;
            }
            if status == 0 {
                ended = true;
                break;
            }
            if let Some(mut items) = value::list_items_mut(row) {
                items.push(item);
                crate::heap::write_barrier_edge(row, item);
            }
        }
        if ended {
            break;
        }
        let values = value::as_list(row)
            .map(|items| items.to_vec())
            .unwrap_or_default();
        let pair = value::tuple_new(values);
        if let Some(mut items) = value::list_items_mut(result) {
            items.push(pair);
            crate::heap::write_barrier_edge(result, pair);
        }
    }
    result
}

// -------------------------------------------------------------------------
// First-class functions (Phase 2.9)
// -------------------------------------------------------------------------

/// `fn(args...)`: calls a function value.
///
/// Wrapper functions have the C signature `extern "C" fn(*const i64) -> i64`
/// and receive a pointer to `argc` tagged arguments (NULL for zero arguments).
/// Generated code reports errors through the thread-local pending exception
/// itself; its return value is passed through unchanged. Builtin values are
/// dispatched by [`call_builtin`], native module functions by
/// [`crate::modules`] with the same arity and argument-buffer checks.
pub(crate) fn call(function: i64, argc: usize, argv: *const i64) -> i64 {
    if value::as_class(function).is_some() {
        return instantiate(function, argc, argv);
    }
    let Some(func) = value::as_function(function) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("'{}' object is not callable", value::type_name_of(function)),
        );
    };
    if argc > 0 && argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("function call received {argc} argument(s) without an argument buffer"),
        );
    }
    match &func.body {
        FunctionBody::Wrapper { entry, captures } => {
            let arity = func.arity;
            if arity != argc {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("function takes {arity} arguments ({argc} given)"),
                );
            }
            if *entry == 0 {
                return exc::raise_with_message(RUNTIME_ERROR, "function has no code");
            }
            // The generated wrapper takes captures followed by user
            // arguments. Avoid a copy for ordinary functions.
            let mut buffer: Vec<i64> = Vec::new();
            let argv = if captures.is_empty() {
                if argc == 0 {
                    std::ptr::null()
                } else {
                    argv
                }
            } else {
                buffer.reserve(captures.len() + argc);
                buffer.extend_from_slice(captures);
                if argc > 0 {
                    // SAFETY: the caller passed a valid buffer for `argc`
                    // tagged values; checked above for NULL.
                    let args = unsafe { std::slice::from_raw_parts(argv, argc) };
                    buffer.extend_from_slice(args);
                }
                buffer.as_ptr()
            };
            // SAFETY: `entry` was installed by `pyx_function_new` or
            // `pyx_closure_new` from a generated entry point with exactly this
            // signature (ARCHITECTURE §3/§5). The argument buffer is only
            // dereferenced by the callee when it has parameters, which were
            // checked against the wrapper's capture+user arity.
            let entry: unsafe extern "C" fn(*const i64) -> i64 =
                unsafe { std::mem::transmute(*entry) };
            // SAFETY: the generated wrapper is called in-process with the
            // calling convention it was emitted for.
            unsafe { entry(argv) }
        }
        FunctionBody::Native { module, name } => modules::call_native(module, name, argc, argv),
        FunctionBody::Builtin(builtin) => call_builtin(*builtin, argc, argv),
    }
}

/// Instantiates a class: allocates the instance and runs `__init__` from the
/// class chain with the instance bound as `self`.
///
/// `__init__`'s return value is ignored (Python requires `None`). Without
/// `__init__` any positional argument is a `TypeError`; otherwise a raised
/// exception propagates and no instance is returned.
fn instantiate(class: i64, argc: usize, argv: *const i64) -> i64 {
    if argc > 0 && argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("class instantiation received {argc} argument(s) without an argument buffer"),
        );
    }
    if argc > isize::MAX as usize / std::mem::size_of::<i64>() {
        return exc::raise_with_message(TYPE_ERROR, "class instantiation has too many arguments");
    }
    // Copy the class name before allocating: a reference into a heap object
    // must not be read after an allocation that may collect.
    let name = value::class_display_name(class)
        .unwrap_or("type")
        .to_string();
    let instance = value::instance_new(class);
    let Some(init) = value::class_method(class, "__init__") else {
        if argc > 0 {
            return exc::raise_with_message(TYPE_ERROR, &format!("{name}() takes no arguments"));
        }
        return instance;
    };
    let mut args = Vec::with_capacity(argc + 1);
    args.push(instance);
    if argc > 0 {
        // SAFETY: the ABI contract requires `argv` to point at `argc` readable
        // tagged values; `argv` is non-NULL because `argc > 0`.
        let rest = unsafe { std::slice::from_raw_parts(argv, argc) };
        args.extend_from_slice(rest);
    }
    let _ = call(init, args.len(), args.as_ptr());
    if exc::pending() {
        // The failing `__init__` already reported the exception; hide the
        // half-initialised instance from the caller.
        return NONE;
    }
    instance
}

/// Reports a builtin arity mismatch as `TypeError`, mirroring the message
/// generated code produces for direct builtin calls.
fn builtin_arity_error(name: &str, expected: &str, argc: usize) -> i64 {
    exc::raise_with_message(
        TYPE_ERROR,
        &format!("{name}() takes {expected} argument(s) ({argc} given)"),
    )
}

/// Calls a builtin referenced as a first-class value (`f = int; f("42")`).
///
/// Arity and semantics match the direct builtin calls lowered by codegen:
/// `print` is variadic, `str`/`int`/`float`/`abs`/`len` take one argument,
/// `set`/`list`/`dict` accept zero or one, `sum`/`enumerate` one or two,
/// `range` one to three, `bytes` zero to two, and `map`/`filter` exactly two.
pub(crate) fn call_builtin(builtin: Builtin, argc: usize, argv: *const i64) -> i64 {
    if argc > 0 && argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{}() received {argc} argument(s) without an argument buffer",
                builtin.name()
            ),
        );
    }
    if argc > isize::MAX as usize / std::mem::size_of::<i64>() {
        return builtin_arity_error(builtin.name(), "a supported number of", argc);
    }
    // SAFETY: the ABI contract requires `argv` to point at `argc` readable
    // tagged values; the length checks above keep the slice in range.
    let args: &[i64] = if argc == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(argv, argc) }
    };
    match builtin {
        Builtin::Print => print(argc, argv),
        Builtin::Bytes => bytes_build(argc, argv),
        Builtin::Zip => zip(argc, argv),
        Builtin::Set => match args {
            [] => container::set_new(0, std::ptr::null()),
            [iterable] => container::set_from(*iterable),
            _ => builtin_arity_error("set", "0 or 1", argc),
        },
        Builtin::Range => match args {
            [stop] => range_new(ops::int_result(0), *stop, ops::int_result(1)),
            [start, stop] => range_new(*start, *stop, ops::int_result(1)),
            [start, stop, step] => range_new(*start, *stop, *step),
            _ => builtin_arity_error("range", "1 to 3", argc),
        },
        Builtin::List => match args {
            [] => container::list_new(0, std::ptr::null()),
            [iterable] => list_from(*iterable),
            _ => builtin_arity_error("list", "0 or 1", argc),
        },
        Builtin::Dict => match args {
            [] => value::dict_new(Vec::new()),
            [iterable] => dict_from(*iterable),
            _ => builtin_arity_error("dict", "0 or 1", argc),
        },
        Builtin::Sum => match args {
            [iterable] => sum(*iterable, ops::int_result(0)),
            [iterable, start] => sum(*iterable, *start),
            _ => builtin_arity_error("sum", "1 or 2", argc),
        },
        Builtin::Enumerate => match args {
            [iterable] => enumerate(*iterable, ops::int_result(0)),
            [iterable, start] => enumerate(*iterable, *start),
            _ => builtin_arity_error("enumerate", "1 or 2", argc),
        },
        Builtin::Map => match args {
            [function, iterable] => map(*function, *iterable),
            _ => builtin_arity_error("map", "exactly 2", argc),
        },
        Builtin::Filter => match args {
            [function, iterable] => filter(*function, *iterable),
            _ => builtin_arity_error("filter", "exactly 2", argc),
        },
        Builtin::Len => match args {
            [value] => len(*value),
            _ => builtin_arity_error("len", "exactly 1", argc),
        },
        Builtin::Str => match args {
            [value] => to_str(*value),
            _ => builtin_arity_error("str", "exactly 1", argc),
        },
        Builtin::Int => match args {
            [value] => to_int(*value),
            _ => builtin_arity_error("int", "exactly 1", argc),
        },
        Builtin::Float => match args {
            [value] => to_float(*value),
            _ => builtin_arity_error("float", "exactly 1", argc),
        },
        Builtin::Abs => match args {
            [value] => abs(*value),
            _ => builtin_arity_error("abs", "exactly 1", argc),
        },
        Builtin::Format => exc::raise_with_message(
            RUNTIME_ERROR,
            "format() is an internal f-string helper and is not callable",
        ),
    }
}

/// `map(function, iterable)`: materialises a list with one call per source
/// element. The result list and the iterator are live roots while callbacks
/// run, so a collection inside the callback cannot free them.
pub(crate) fn map(function: i64, iterable: i64) -> i64 {
    if value::as_function(function).is_none() {
        return exc::raise_with_message(TYPE_ERROR, "map() argument 1 must be callable");
    }
    let iterator = iter::iter_new(iterable);
    if exc::pending() {
        return NONE;
    }
    let result = value::list_new(Vec::new());
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        // `&item` points at this stack slot; the collector scans the native
        // stack, so the argument stays reachable for the whole callback.
        let mapped = call(function, 1, &item);
        if exc::pending() {
            return NONE;
        }
        if let Some(mut items) = value::list_items_mut(result) {
            items.push(mapped);
            crate::heap::write_barrier_edge(result, mapped);
        }
    }
    result
}

/// `filter(function_or_none, iterable)`: materialises the elements for which
/// the predicate is truthy (or the element itself when `None` is passed).
pub(crate) fn filter(function: i64, iterable: i64) -> i64 {
    let predicate = if function == NONE {
        None
    } else if value::as_function(function).is_some() {
        Some(function)
    } else {
        return exc::raise_with_message(TYPE_ERROR, "filter() argument 1 must be callable or None");
    };
    let iterator = iter::iter_new(iterable);
    if exc::pending() {
        return NONE;
    }
    let result = value::list_new(Vec::new());
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        let keep = match predicate {
            None => value::truthy(item),
            Some(function) => {
                let verdict = call(function, 1, &item);
                if exc::pending() {
                    return NONE;
                }
                value::truthy(verdict)
            }
        };
        if keep {
            if let Some(mut items) = value::list_items_mut(result) {
                items.push(item);
                crate::heap::write_barrier_edge(result, item);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{encode_int, FALSE, NONE, TRUE};
    fn int(n: i64) -> i64 {
        encode_int(n).expect("representable")
    }

    fn string(s: &str) -> i64 {
        value::str_from_bytes(s.as_bytes())
    }

    fn rendered(v: i64) -> String {
        value::to_string(v)
    }

    fn pending_message() -> String {
        rendered(exc::pending_value())
    }

    #[test]
    fn print_rendering() {
        let args = [int(1), string("x"), TRUE, NONE, value::float_new(1.0)];
        assert_eq!(render_args(&args), "1 x True None 1.0");
        assert_eq!(render_args(&[]), "");
    }

    #[test]
    fn len_counts_scalar_values() {
        exc::init();
        assert_eq!(value::as_integer(len(string("héllo"))), Some(5));
        assert_eq!(value::as_integer(len(string(""))), Some(0));
        assert_eq!(value::as_integer(len(string("日本語"))), Some(3));
        assert_eq!(len(int(5)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    fn len_counts_tuples() {
        exc::init();
        assert_eq!(
            value::as_integer(len(value::tuple_new(vec![int(1), int(2)]))),
            Some(2)
        );
        assert_eq!(
            value::as_integer(len(value::tuple_new(Vec::new()))),
            Some(0)
        );
        let t = value::tuple_new(vec![int(1)]);
        assert_eq!(to_int(t), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::init();
        assert_eq!(to_float(t), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::init();
        assert_eq!(abs(t), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    fn int_conversions() {
        exc::init();
        assert_eq!(value::as_integer(to_int(int(7))), Some(7));
        assert_eq!(value::as_integer(to_int(TRUE)), Some(1));
        assert_eq!(value::as_integer(to_int(FALSE)), Some(0));
        assert_eq!(value::as_integer(to_int(value::float_new(-2.9))), Some(-2));
        assert_eq!(value::as_integer(to_int(string(" 42 "))), Some(42));
        assert_eq!(value::as_integer(to_int(string("-7"))), Some(-7));

        let bad = to_int(string("4x"));
        assert_eq!(bad, NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(
            rendered(exc::pending_value()),
            "invalid literal for int() with base 10: '4x'"
        );

        exc::init();
        assert_eq!(to_int(value::float_new(f64::NAN)), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::init();
        assert_eq!(to_int(NONE), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    fn float_conversions() {
        exc::init();
        assert_eq!(
            value::as_float(to_float(int(3))).map(value::format_float),
            Some("3.0".to_string())
        );
        assert_eq!(value::as_float(to_float(string("0.5"))), Some(0.5));
        assert_eq!(value::as_float(to_float(string("1e3"))), Some(1000.0));
        assert_eq!(value::as_float(to_float(TRUE)), Some(1.0));

        let bad = to_float(string("abc"));
        assert_eq!(bad, NONE);
        assert_eq!(
            rendered(exc::pending_value()),
            "could not convert string to float: 'abc'"
        );
    }

    #[test]
    fn to_str_results() {
        assert_eq!(value::as_str(to_str(int(-5))), Some("-5"));
        assert_eq!(value::as_str(to_str(NONE)), Some("None"));
        assert_eq!(value::as_str(to_str(TRUE)), Some("True"));
        assert_eq!(value::as_str(to_str(value::float_new(1e20))), Some("1e+20"));
        let s = string("hi");
        assert_eq!(value::as_str(to_str(s)), Some("hi"));
    }

    #[test]
    fn abs_values() {
        exc::init();
        assert_eq!(value::as_integer(abs(int(-5))), Some(5));
        assert_eq!(value::as_integer(abs(int(5))), Some(5));
        assert_eq!(value::as_float(abs(value::float_new(-2.5))), Some(2.5));
        assert_eq!(value::as_integer(abs(FALSE)), Some(0));

        let v = abs(string("x"));
        assert_eq!(v, NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    fn integers(v: i64) -> Vec<i64> {
        value::as_list(v)
            .expect("list")
            .iter()
            .map(|&item| value::as_integer(item).expect("integer element"))
            .collect()
    }

    fn ranged(start: i64, stop: i64, step: i64) -> i64 {
        range_new(int(start), int(stop), int(step))
    }

    #[test]
    fn range_representation_and_length() {
        exc::init();
        let r = ranged(0, 5, 1);
        assert_eq!(value::type_name(r), "range");
        assert_eq!(rendered(r), "range(0, 5)");
        assert_eq!(value::as_integer(len(r)), Some(5));
        assert!(value::truthy(r));

        assert_eq!(rendered(ranged(0, 10, 2)), "range(0, 10, 2)");
        assert_eq!(rendered(ranged(5, 0, -2)), "range(5, 0, -2)");
        assert_eq!(value::as_integer(len(ranged(5, 0, -2))), Some(3));
        assert_eq!(value::as_integer(len(ranged(0, 0, 1))), Some(0));
        assert_eq!(value::as_integer(len(ranged(5, 0, 1))), Some(0));
        assert_eq!(value::as_integer(len(ranged(0, 5, -1))), Some(0));
        assert!(!value::truthy(ranged(0, 0, 1)));
        assert!(!exc::pending());
    }

    #[test]
    fn range_iteration_and_errors() {
        exc::init();

        fn collect(source: i64) -> Vec<i64> {
            let iterator = iter::iter_new(source);
            let mut out = NONE;
            let mut values = Vec::new();
            while iter::iter_next(iterator, &mut out) == 1 {
                values.push(value::as_integer(out).expect("integer yield"));
            }
            values
        }

        assert_eq!(collect(ranged(0, 5, 1)), vec![0, 1, 2, 3, 4]);
        assert_eq!(collect(ranged(5, 0, -2)), vec![5, 3, 1]);
        assert_eq!(collect(ranged(0, 10, 2)), vec![0, 2, 4, 6, 8]);
        assert_eq!(collect(ranged(0, 0, 1)), Vec::<i64>::new());
        assert!(!exc::pending());

        assert_eq!(range_new(int(0), int(5), int(0)), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(pending_message(), "range() arg 3 must not be zero");
        exc::init();

        assert_eq!(range_new(string("x"), int(5), int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "'str' object cannot be interpreted as an integer"
        );
        exc::init();

        assert_eq!(range_new(int(0), value::float_new(2.0), int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::init();
    }

    #[test]
    fn list_from_sources() {
        exc::init();
        let source = value::list_new(vec![int(1), int(2)]);
        assert_eq!(integers(list_from(source)), vec![1, 2]);

        let tuple = value::tuple_new(vec![int(3), int(4)]);
        assert_eq!(integers(list_from(tuple)), vec![3, 4]);

        let set = value::set_new(vec![int(5), int(6)]);
        assert_eq!(integers(list_from(set)), vec![5, 6]);

        assert_eq!(integers(list_from(ranged(2, 8, 2))), vec![2, 4, 6]);
        assert_eq!(integers(list_from(ranged(5, 0, -2))), vec![5, 3, 1]);

        let text = list_from(string("abc"));
        let chars: Vec<&str> = value::as_list(text)
            .expect("list")
            .iter()
            .map(|&item| value::as_str(item).expect("char"))
            .collect();
        assert_eq!(chars, vec!["a", "b", "c"]);

        let dict = value::dict_new(vec![(string("a"), int(1)), (string("b"), int(2))]);
        let keys = list_from(dict);
        let keys: Vec<&str> = value::as_list(keys)
            .expect("list")
            .iter()
            .map(|&item| value::as_str(item).expect("key"))
            .collect();
        assert_eq!(keys, vec!["a", "b"]);
        assert!(!exc::pending());

        assert_eq!(list_from(int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(pending_message(), "'int' object is not iterable");
        exc::init();
    }

    #[test]
    fn dict_from_pairs_and_copy() {
        exc::init();
        let pairs = value::list_new(vec![
            value::tuple_new(vec![int(1), string("a")]),
            value::list_new(vec![int(2), string("b")]),
        ]);
        let built = dict_from(pairs);
        assert_eq!(rendered(built), "{1: 'a', 2: 'b'}");

        // Duplicate keys keep the first position and the last value.
        let duplicates = value::list_new(vec![
            value::tuple_new(vec![int(1), string("a")]),
            value::tuple_new(vec![int(2), string("b")]),
            value::tuple_new(vec![int(1), string("c")]),
        ]);
        assert_eq!(rendered(dict_from(duplicates)), "{1: 'c', 2: 'b'}");

        let copied = dict_from(built);
        assert_eq!(rendered(copied), "{1: 'a', 2: 'b'}");
        assert!(!exc::pending());

        let short = value::list_new(vec![value::tuple_new(vec![int(1)])]);
        assert_eq!(dict_from(short), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "dictionary update sequence element #0 has length 1; 2 is required"
        );
        exc::init();

        let scalar = value::list_new(vec![int(1)]);
        assert_eq!(dict_from(scalar), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "cannot convert dictionary update sequence element #0 to a sequence"
        );
        exc::init();

        let unhashable = value::list_new(vec![value::tuple_new(vec![
            value::list_new(Vec::new()),
            int(1),
        ])]);
        assert_eq!(dict_from(unhashable), NONE);
        assert_eq!(pending_message(), "unhashable type: 'list'");
        exc::init();

        assert_eq!(dict_from(int(5)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(pending_message(), "'int' object is not iterable");
        exc::init();
    }

    #[test]
    fn sum_values_and_errors() {
        exc::init();
        assert_eq!(value::as_integer(sum(ranged(0, 5, 1), int(0))), Some(10));
        assert_eq!(value::as_integer(sum(ranged(1, 10, 2), int(0))), Some(25));
        assert_eq!(
            value::as_integer(sum(ranged(0, 10, 1), int(100))),
            Some(145)
        );

        let floats = value::list_new(vec![value::float_new(1.5), int(2)]);
        assert_eq!(value::as_float(sum(floats, int(0))), Some(3.5));
        assert_eq!(
            value::as_integer(sum(value::list_new(Vec::new()), int(7))),
            Some(7)
        );

        let texts = value::list_new(vec![string("a"), string("b")]);
        assert_eq!(sum(texts, int(0)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "unsupported operand type(s) for +: 'int' and 'str'"
        );
        exc::init();

        assert_eq!(sum(int(3), int(0)), NONE);
        assert_eq!(pending_message(), "'int' object is not iterable");
        exc::init();
    }

    #[test]
    fn enumerate_and_zip_values() {
        exc::init();
        let source = value::list_new(vec![string("a"), string("b")]);
        assert_eq!(rendered(enumerate(source, int(0))), "[(0, 'a'), (1, 'b')]");
        assert_eq!(
            rendered(enumerate(source, int(10))),
            "[(10, 'a'), (11, 'b')]"
        );
        assert!(!exc::pending());

        let left = value::list_new(vec![int(1), int(2), int(3)]);
        let right = value::list_new(vec![string("x"), string("y")]);
        let sources = [left, right];
        assert_eq!(rendered(zip(2, sources.as_ptr())), "[(1, 'x'), (2, 'y')]");
        assert_eq!(integers(zip(0, std::ptr::null())), Vec::<i64>::new());

        let single = [value::list_new(vec![int(7), int(8)])];
        assert_eq!(rendered(zip(1, single.as_ptr())), "[(7,), (8,)]");

        let invalid = [int(3)];
        assert_eq!(zip(1, invalid.as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(pending_message(), "'int' object is not iterable");
        exc::init();
    }

    #[test]
    fn builtins_survive_collection() {
        exc::init();
        let source = value::list_new(vec![string("a"), string("b")]);
        let enumerated = enumerate(source, int(0));
        let sources = [source];
        let zipped = zip(1, sources.as_ptr());
        let listed = list_from(ranged(0, 4, 1));
        let summed = sum(ranged(0, 4, 1), int(0));

        for _ in 0..128 {
            let _ = string("garbage");
        }
        crate::heap::collect();

        assert_eq!(rendered(enumerated), "[(0, 'a'), (1, 'b')]");
        assert_eq!(rendered(zipped), "[('a',), ('b',)]");
        assert_eq!(integers(listed), vec![0, 1, 2, 3]);
        assert_eq!(value::as_integer(summed), Some(6));
        assert!(!exc::pending());
    }
}
