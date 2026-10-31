//! Builtin functions: `print`, `len`, `str`, `int`, `float`, `abs`, `range`,
//! `list`, `dict`, `sum`, `enumerate`, `zip`, `ord`, `chr`, `hex`, `bin`,
//! `oct`, `divmod`, `callable` (ARCHITECTURE §3.2).

use std::io::Write as _;

use hydro_ir::{stdlib, BinOp, Builtin, MethodKind, ParamKind, UnOp};
use num_bigint::BigInt;

use crate::container;
use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, IMPORT_ERROR, NAME_ERROR, NOT_IMPLEMENTED_ERROR, OS_ERROR,
    OVERFLOW_ERROR, RUNTIME_ERROR, SYNTAX_ERROR, TYPE_ERROR, VALUE_ERROR, ZERO_DIVISION_ERROR,
};
use crate::format;
use crate::iter;
use crate::methods;
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
        // SAFETY: ABI contract: `argv` points at `argc` readable tagged values.
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

/// `print(*args, sep=..., end=..., file=..., flush=...)`.
///
/// `sep`/`end` must be `str` or `None` (the defaults); `file` defaults to
/// stdout and may be any file-like object reachable through its `write`
/// method; `flush` calls `flush()` on the target after writing. Unknown
/// keywords and invalid values report a tagged `TypeError` before any output
/// is produced.
pub(crate) fn print_with_keywords(
    argc: usize,
    argv: *const i64,
    keywords: &[(String, i64)],
) -> i64 {
    let mut sep = String::from(" ");
    let mut end = String::from("\n");
    let mut file = NONE;
    let mut flush = false;
    for (name, argument) in keywords {
        match name.as_str() {
            "sep" | "end" => {
                let slot = if name == "sep" { &mut sep } else { &mut end };
                if *argument == NONE {
                    *slot = if name == "sep" {
                        String::from(" ")
                    } else {
                        String::from("\n")
                    };
                    continue;
                }
                let Some(text) = value::as_str(*argument) else {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!(
                            "{name} must be None or a string, not {}",
                            value::type_name(*argument)
                        ),
                    );
                };
                *slot = text.to_string();
            }
            "file" => file = *argument,
            "flush" => flush = value::truthy(*argument),
            _ => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("print() got an unexpected keyword argument '{name}'"),
                )
            }
        }
    }
    let args: &[i64] = if argc == 0 || argv.is_null() {
        &[]
    } else {
        // SAFETY: ABI contract: `argv` points at `argc` readable tagged values.
        unsafe { std::slice::from_raw_parts(argv, argc) }
    };
    let mut text = String::new();
    for (index, argument) in args.iter().enumerate() {
        if index > 0 {
            text.push_str(&sep);
        }
        text.push_str(&value::to_string(*argument));
    }
    text.push_str(&end);
    if file == NONE {
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        let _ = stdout.write_all(text.as_bytes());
        // The existing positional `print` always flushes; keep that behaviour
        // (and honour an explicit `flush=True`).
        let _ = stdout.flush();
        return NONE;
    }
    let written = value::str_alloc(text);
    let arguments = [written];
    let _ = crate::methods::call(
        file,
        b"write".as_ptr(),
        5,
        arguments.len(),
        arguments.as_ptr(),
    );
    if exc::pending() {
        return NONE;
    }
    if flush {
        let _ = crate::methods::call(file, b"flush".as_ptr(), 5, 0, std::ptr::null());
        if exc::pending() {
            return NONE;
        }
    }
    NONE
}

/// `str(v)` as a fresh string object.
pub(crate) fn to_str(v: i64) -> i64 {
    value::str_alloc(value::to_string(v))
}

/// `str(bytes_like, encoding[, errors])`: decodes through `_codecs.decode`.
///
/// The object is checked before the codec arguments, matching CPython:
/// a `str` object reports `decoding str is not supported` and any other
/// non-bytes value reports `decoding to str: need a bytes-like object`.
/// `bytes`/`bytearray` are passed through to the codec, a `memoryview` is
/// materialized as `bytes` first. Encoding lookup and decode failures come
/// from the codec, so unknown names stay `LookupError` and malformed input
/// raises `UnicodeDecodeError` from the chosen error handler.
fn str_decode(args: &[i64]) -> i64 {
    if value::as_str(args[0]).is_some() {
        return exc::raise_with_message(TYPE_ERROR, "decoding str is not supported");
    }
    let mut source = args[0];
    // A `bytearray` payload must not be aliased across the decode: a custom
    // error handler runs Python code that can resize it. Copy to `bytes`.
    if let Some(data) = value::as_bytearray(source) {
        source = value::bytes_new(data.clone());
    } else if value::as_bytes(source).is_none() {
        let Some(view) = value::as_memoryview(source) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "decoding to str: need a bytes-like object, {} found",
                    value::type_name(source)
                ),
            );
        };
        match value::memoryview_bytes(view) {
            Ok(bytes) => source = value::bytes_new(bytes),
            Err(message) => return exc::raise_with_message(VALUE_ERROR, message),
        }
    }
    let mut call_args = vec![source];
    for (name, argument) in [
        ("encoding", args.get(1).copied()),
        ("errors", args.get(2).copied()),
    ] {
        let Some(argument) = argument else {
            continue;
        };
        if value::as_str(argument).is_none() {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "str() argument '{name}' must be str, not {}",
                    value::type_name(argument)
                ),
            );
        }
        call_args.push(argument);
    }
    // The decoded object (and the codec arguments) must survive a custom
    // error handler that runs Python code and may trigger a collection.
    let roots = crate::heap::root_values(&call_args);
    let result = modules::codecs::call("decode", &call_args);
    crate::heap::release_roots(roots);
    result
}

/// `repr(v)` as a fresh string object.
pub(crate) fn to_repr(v: i64) -> i64 {
    value::str_alloc(value::repr(v))
}

/// `ascii(v)`: `repr(v)` with every non-ASCII character escaped.
///
/// The escapes match CPython's `ascii()`: `\xNN` for code points up to
/// `0xff`, `\uNNNN` up to `0xffff` and `\U000NNNNN` above.
pub(crate) fn to_ascii(v: i64) -> i64 {
    value::str_alloc(ascii_escape(&value::repr(v)))
}

/// Escapes the non-ASCII characters of an already rendered `repr` string.
fn ascii_escape(repr: &str) -> String {
    let mut out = String::with_capacity(repr.len() + 8);
    for character in repr.chars() {
        let code = character as u32;
        if code < 0x80 {
            out.push(character);
        } else if code <= 0xff {
            out.push_str(&format!("\\x{code:02x}"));
        } else if code <= 0xffff {
            out.push_str(&format!("\\u{code:04x}"));
        } else {
            out.push_str(&format!("\\U{code:08x}"));
        }
    }
    out
}

/// `hash(v)`: the runtime's dict-key hash mapped into a non-negative tagged
/// integer.
///
/// Equal values hash equally (`1`, `1.0` and `True` share a hash) and
/// unhashable values raise `TypeError`, matching their use as dict keys. The
/// value is deterministic per process but implementation-defined and not
/// reproducible against CPython.
pub(crate) fn hash(v: i64) -> i64 {
    if let Some(name) = container::unhashable_name(v) {
        return exc::raise_with_message(TYPE_ERROR, &container::unhashable_message(name));
    }
    // Drop the low 4 bits so the 64-bit hash always fits the 61-bit tagged
    // integer as a non-negative value.
    ops::int_result((container::key_hash(v) >> 4) as i64)
}

/// `id(v)`: the value's identity as an integer.
///
/// Heap values report their runtime address with the tag bits masked off;
/// tagged immediates (`int`, `bool`, `None`, ...) are mapped injectively away
/// from the heap tag. This is the same encoding `builtins.id` uses, so both
/// spellings return the same number for a value and the number never collides
/// between an immediate and a heap object.
pub(crate) fn id_value(v: i64) -> i64 {
    if value::is_heap(v) {
        return ops::int_result(v & !value::TAG_MASK);
    }
    ops::int_result((v as u64 ^ 0x9e37_79b9_7f4a_7c17u64) as i64)
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
    if let Some(view) = value::as_memoryview(v) {
        return ops::int_result(view.len() as i64);
    }
    if let Some(view) = value::as_dict_view(v) {
        return ops::int_result(container::dict_view_len(view) as i64);
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
    if let Some(instance) = value::as_instance(v) {
        if let Some(value::ClassEntry::Method(MethodKind::Normal, function)) =
            value::class_lookup(instance.class, "__len__")
        {
            let arguments = [v];
            let result = call(function, 1, arguments.as_ptr());
            if exc::pending() {
                return NONE;
            }
            if value::as_integer(result).is_some() {
                return result;
            }
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "'__len__' should return an integer, not {}",
                    value::type_name(result)
                ),
            );
        }
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!("object of type '{}' has no len()", value::type_name(v)),
    )
}

/// `ord(c)`: the Unicode scalar value of a one-character string.
///
/// A string with any other length (including the empty string) raises
/// `TypeError`, as does a non-string argument.
pub(crate) fn ord_value(v: i64) -> i64 {
    let Some(s) = value::as_str(v) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "ord() expected string of length 1, but {} found",
                value::type_name(v)
            ),
        );
    };
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) => ops::int_result(ch as i64),
        _ => {
            let len = s.chars().count();
            exc::raise_with_message(
                TYPE_ERROR,
                &format!("ord() expected a character, but string of length {len} found"),
            )
        }
    }
}

/// `chr(i)`: the one-character string for a Unicode scalar value.
///
/// `i` must be an integer in `0..=0x10FFFF`; surrogate code points are
/// rejected because Hydro strings are UTF-8 and cannot represent them. A
/// non-integer argument raises `TypeError`, a rejected code point
/// `ValueError`.
pub(crate) fn chr_value(v: i64) -> i64 {
    let Some(code) = value::as_integer(v) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "chr() argument must be an int, not '{}'",
                value::type_name(v)
            ),
        );
    };
    if !(0..=0x10FFFF).contains(&code) {
        return exc::raise_with_message(VALUE_ERROR, "chr() arg not in range(0x110000)");
    }
    match char::from_u32(code as u32) {
        Some(ch) => value::str_alloc(ch.to_string()),
        None => exc::raise_with_message(VALUE_ERROR, "chr() arg cannot be a surrogate code point"),
    }
}

/// Shared implementation of `hex`, `bin` and `oct`: the `radix` spelling of
/// `v` with `prefix` and a leading `-` for negative values.
///
/// Exact integers (tagged `int`, `bool` and heap `BigInt`) produce their full
/// digit sequence; every other type raises the same `TypeError` CPython
/// reports for a missing `__index__`.
fn radix_literal(v: i64, radix: u32, prefix: &str) -> i64 {
    let Some(number) = value::to_bigint(v) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "'{}' object cannot be interpreted as an integer",
                value::type_name_of(v)
            ),
        );
    };
    let sign = if number.sign() == num_bigint::Sign::Minus {
        "-"
    } else {
        ""
    };
    let digits = number.magnitude().to_str_radix(radix);
    value::str_alloc(format!("{sign}{prefix}{digits}"))
}

/// `hex(v)`: `0x`-prefixed lowercase hexadecimal.
pub(crate) fn hex_value(v: i64) -> i64 {
    radix_literal(v, 16, "0x")
}

/// `bin(v)`: `0b`-prefixed binary.
pub(crate) fn bin_value(v: i64) -> i64 {
    radix_literal(v, 2, "0b")
}

/// `oct(v)`: `0o`-prefixed octal.
pub(crate) fn oct_value(v: i64) -> i64 {
    radix_literal(v, 8, "0o")
}

/// Tries `v.__int__()` / `v.__index__()` for the `int()` conversion protocol.
///
/// The special method is looked up the way CPython looks it up: through the
/// receiver's class for instances, through the metaclass for class objects,
/// and through the extension's attribute surface for opaque extension values.
/// A missing method clears the `AttributeError` and returns `None`; any other
/// exception propagates. A result that is not an exact integer reports the
/// protocol's `TypeError` (CPython still *accepts* `bool` there, deprecating
/// it, so that case converts silently).
fn int_special_method(v: i64, name: &str) -> Option<i64> {
    let result = methods::call(v, name.as_ptr(), name.len(), 0, std::ptr::null());
    if exc::pending() {
        if exc::pending_tag() == ATTRIBUTE_ERROR as i64 {
            exc::clear();
            return None;
        }
        return Some(NONE);
    }
    if let Some(n) = value::as_integer(result) {
        return Some(ops::int_result(n));
    }
    if let Some(n) = value::as_bigint(result) {
        return Some(value::bigint_new(n.clone()));
    }
    Some(exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "{name} returned non-int (type {})",
            value::type_name_of(result)
        ),
    ))
}

/// `int(v)`.
pub(crate) fn to_int(v: i64) -> i64 {
    if let Some(n) = value::as_integer(v) {
        return ops::int_result(n);
    }
    // Integers outside `i64` keep their exact value.
    if let Some(n) = value::as_bigint(v) {
        return value::bigint_new(n.clone());
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
            // Large finite floats convert exactly instead of overflowing.
            return match value::bigint_from_f64_trunc(f) {
                Some(n) => value::bigint_new(n),
                None => exc::raise_with_message(OVERFLOW_ERROR, "integer overflow"),
            };
        }
        // `int()` truncates toward zero.
        return ops::int_result(f.trunc() as i64);
    }
    if let Some(s) = value::as_str(v) {
        return match value::parse_decimal_int(s.trim()) {
            Some(n) => value::bigint_new(n),
            None => exc::raise_with_message(
                VALUE_ERROR,
                &format!("invalid literal for int() with base 10: '{s}'"),
            ),
        };
    }
    // CPython's conversion protocol: `__int__` first, then `__index__`, on
    // instances, class objects (via the metaclass) and extension values.
    if let Some(result) = int_special_method(v, "__int__") {
        return result;
    }
    if let Some(result) = int_special_method(v, "__index__") {
        return result;
    }
    // CPython names the class of an instance but reports `type` for a class
    // object that defines no conversion protocol.
    let type_name = if value::as_class(v).is_some() {
        "type".to_string()
    } else {
        value::type_name_of(v)
    };
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "int() argument must be a string, a bytes-like object or a real number, not '{}'",
            type_name
        ),
    )
}

/// `int(string, base)` for an explicit base (2..36, or 0 with prefixes).
fn to_int_base(value_arg: i64, base_arg: i64) -> i64 {
    if base_arg == NONE {
        return to_int(value_arg);
    }
    let Some(base) = value::as_integer(base_arg) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "int() base must be an integer, not '{}'",
                value::type_name(base_arg)
            ),
        );
    };
    if base == 10 {
        // An explicit base only accepts strings (CPython:
        // "int() can't convert non-string with explicit base"); in particular
        // it never runs the `__int__`/`__index__` protocol.
        if value::as_str(value_arg).is_some() {
            return to_int(value_arg);
        }
        return exc::raise_with_message(
            TYPE_ERROR,
            "int() can't convert non-string with explicit base",
        );
    }
    if !(base == 0 || (2..=36).contains(&base)) {
        return exc::raise_with_message(VALUE_ERROR, "int() base must be >= 2 and <= 36, or 0");
    }
    let Some(text) = value::as_str(value_arg) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "int() can't convert non-string with explicit base",
        );
    };
    let text = text.trim();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (radix, digits) = if base == 0 {
        if let Some(rest) = digits
            .strip_prefix("0x")
            .or_else(|| digits.strip_prefix("0X"))
        {
            (16, rest)
        } else if let Some(rest) = digits
            .strip_prefix("0o")
            .or_else(|| digits.strip_prefix("0O"))
        {
            (8, rest)
        } else if let Some(rest) = digits
            .strip_prefix("0b")
            .or_else(|| digits.strip_prefix("0B"))
        {
            (2, rest)
        } else if digits.len() > 1 && digits.starts_with('0') {
            (8, &digits[1..])
        } else {
            (10, digits)
        }
    } else {
        (base as u32, digits)
    };
    if digits.is_empty() {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!("invalid literal for int() with base {base}: '{text}'"),
        );
    }
    for ch in digits.chars() {
        if ch.to_digit(radix).is_none() {
            return exc::raise_with_message(
                VALUE_ERROR,
                &format!("invalid literal for int() with base {base}: '{text}'"),
            );
        }
    }
    // Accumulate with arbitrary precision: an explicit base no longer turns a
    // large literal into `OverflowError`.
    match value::bigint_from_radix(digits, radix, negative) {
        Some(n) => value::bigint_new(n),
        None => exc::raise_with_message(
            VALUE_ERROR,
            &format!("invalid literal for int() with base {base}: '{text}'"),
        ),
    }
}

/// `float(v)`.
pub(crate) fn to_float(v: i64) -> i64 {
    if let Some(n) = value::as_integer(v) {
        return value::float_new(n as f64);
    }
    if let Some(n) = value::as_bigint(v) {
        return match value::bigint_to_f64(n) {
            Some(f) => value::float_new(f),
            None => exc::raise_with_message(OVERFLOW_ERROR, "int too large to convert to float"),
        };
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
            None => value::bigint_new(n.unsigned_abs().into()),
        };
    }
    if let Some(n) = value::as_bigint(v) {
        // The magnitude of a BigInt is already the absolute value.
        return value::bigint_new(n.magnitude().clone().into());
    }
    if let Some(f) = value::as_float(v) {
        return value::float_new(f.abs());
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!("bad operand type for abs(): '{}'", value::type_name(v)),
    )
}

/// `divmod(a, b)`: Python's floored quotient and remainder as a 2-tuple.
///
/// Integers use floor division (the remainder takes the divisor's sign and
/// division towards negative infinity). Floats follow CPython's `fmod`
/// adjustment, so `divmod(-7.5, 2.0) == (-4.0, 0.5)`. A zero divisor raises
/// `ZeroDivisionError` and a non-numeric operand raises `TypeError`.
pub(crate) fn divmod(a: i64, b: i64) -> i64 {
    if value::is_bigint(a) || value::is_bigint(b) {
        return bigint_divmod(a, b);
    }
    match (value::num_of(a), value::num_of(b)) {
        (Some(value::Num::Int(p)), Some(value::Num::Int(q))) => {
            if q == 0 {
                return exc::raise_with_message(ZERO_DIVISION_ERROR, "division by zero");
            }
            let (Some(mut quotient), Some(mut remainder)) = (p.checked_div(q), p.checked_rem(q))
            else {
                return exc::raise_with_message(OVERFLOW_ERROR, "integer overflow");
            };
            if remainder != 0 && ((remainder < 0) != (q < 0)) {
                remainder += q;
                quotient -= 1;
            }
            value::tuple_new(vec![ops::int_result(quotient), ops::int_result(remainder)])
        }
        (Some(x), Some(y)) => {
            let dividend = match x {
                value::Num::Int(n) => n as f64,
                value::Num::Float(f) => f,
            };
            let divisor = match y {
                value::Num::Int(n) => n as f64,
                value::Num::Float(f) => f,
            };
            float_divmod(dividend, divisor)
        }
        _ => exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "unsupported operand type(s) for divmod(): '{}' and '{}'",
                value::type_name(a),
                value::type_name(b)
            ),
        ),
    }
}

/// `divmod` when at least one operand is a heap BigInt: exact integer
/// division when both operands are integers, float semantics when either is a
/// float.
fn bigint_divmod(a: i64, b: i64) -> i64 {
    if let (Some(left), Some(right)) = (value::to_bigint(a), value::to_bigint(b)) {
        if right.sign() == num_bigint::Sign::NoSign {
            return exc::raise_with_message(ZERO_DIVISION_ERROR, "division by zero");
        }
        let (quotient, remainder) = ops::integer_floor_divmod(&left, &right);
        return value::tuple_new(vec![
            value::bigint_new(quotient),
            value::bigint_new(remainder),
        ]);
    }
    let convert = |operand: i64| -> Option<f64> {
        match value::to_bigint(operand) {
            Some(n) => value::bigint_to_f64(&n),
            None => value::as_float(operand),
        }
    };
    let (Some(dividend), Some(divisor)) = (convert(a), convert(b)) else {
        return exc::raise_with_message(OVERFLOW_ERROR, "int too large to convert to float");
    };
    float_divmod(dividend, divisor)
}

/// Floating-point `divmod`: the remainder takes the divisor's sign and the
/// floored quotient keeps the dividend's.
fn float_divmod(dividend: f64, divisor: f64) -> i64 {
    if divisor == 0.0 {
        return exc::raise_with_message(ZERO_DIVISION_ERROR, "division by zero");
    }
    let mut modulo = dividend % divisor;
    if modulo != 0.0 {
        // The remainder takes the divisor's sign.
        if (divisor < 0.0) != (modulo < 0.0) {
            modulo += divisor;
        }
    } else {
        // Preserve the divisor's sign on an exact zero, as CPython does
        // (`divmod(4.0, 2.0) == (2.0, 0.0)`).
        modulo = 0.0_f64.copysign(divisor);
    }
    // Derive the floored quotient from the adjusted remainder so the identity
    // `dividend == quotient * divisor + modulo` holds.
    let mut quotient = if modulo != 0.0 {
        (dividend - modulo) / divisor
    } else {
        dividend / divisor
    };
    if quotient != 0.0 {
        quotient = quotient.copysign(dividend / divisor);
    }
    value::tuple_new(vec![value::float_new(quotient), value::float_new(modulo)])
}

/// `callable(v)`: whether `v` can be called like a function.
///
/// This mirrors the values [`call`] accepts: functions (including builtins,
/// native module functions, getters and partials), classes and extension
/// functions.
pub(crate) fn callable(v: i64) -> i64 {
    value::bool_value(
        value::as_function(v).is_some()
            || value::as_class(v).is_some()
            || value::as_ext_function(v).is_some()
            || modules::extension::ext_object_is_callable(v),
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
pub(crate) const MAX_BYTES_ALLOC: usize = 64 * 1024 * 1024;

/// `bytes(...)`: the constructor behind [`crate::abi::hydro_bytes_build`].
///
/// Signatures handled here:
/// - `bytes()` -> `b""`
/// - `bytes(bytes)` -> copy
/// - `bytes(int)` -> that many zero bytes
/// - `bytes(list|tuple|set)` -> one integer per element (`0..=255`)
/// - `bytes(str, encoding[, errors])` -> encoded bytes through `_codecs`
///
/// Every other form reports a pending exception and returns `None`.
pub(crate) fn bytes_build(argc: usize, argv: *const i64) -> i64 {
    if argc > 3 {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("bytes() takes at most 3 arguments ({argc} given)"),
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
    // SAFETY: ABI contract: `argv` points at `argc` readable values; length checked above.
    let args = unsafe { std::slice::from_raw_parts(argv, argc) };
    if argc >= 2 {
        if value::as_str(args[0]).is_none() {
            return exc::raise_with_message(TYPE_ERROR, "encoding without a string argument");
        }
        if value::as_str(args[1]).is_none() {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("encoding must be str, not {}", value::type_name(args[1])),
            );
        }
        if argc == 3 && value::as_str(args[2]).is_none() {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("errors must be str, not {}", value::type_name(args[2])),
            );
        }
        // The codec module owns encoding names, aliases and error handlers.
        let roots = crate::heap::root_values(args);
        let result = modules::codecs::call("encode", args);
        crate::heap::release_roots(roots);
        return result;
    }
    if let Some(bytes) = value::as_bytes(args[0]) {
        return value::bytes_from_slice(bytes);
    }
    // `bytes(memoryview)` copies the visible bytes (contiguous by
    // construction in this runtime).
    if let Some(view) = value::as_memoryview(args[0]) {
        return match value::memoryview_bytes(view) {
            Ok(bytes) => value::bytes_new(bytes),
            Err(message) => exc::raise_with_message(exc::VALUE_ERROR, message),
        };
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
    // `bytes(range(...))` is the common iterable form; materialise it
    // directly so the builtin never depends on the generic iterator for this
    // shape (and so huge ranges are rejected before any allocation).
    if let Some(range) = value::as_range(args[0]) {
        return bytes_from_range(range);
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
                        value::type_name_of(item)
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
    bytes_from_iterable(args[0])
}

/// `bytes(range(start, stop, step))`: one byte per element.
///
/// The element count is computed in `i128` so an extreme range is rejected by
/// the allocation limit instead of overflowing; every value must be in
/// `0..=255`, like CPython.
fn bytes_from_range((start, stop, step): (i64, i64, i64)) -> i64 {
    let (start, stop, step) = (i128::from(start), i128::from(stop), i128::from(step));
    let count = if step > 0 {
        if start >= stop {
            0
        } else {
            (stop - start + step - 1) / step
        }
    } else if step < 0 {
        if start <= stop {
            0
        } else {
            (start - stop + (-step) - 1) / (-step)
        }
    } else {
        return exc::raise_with_message(VALUE_ERROR, "range() arg 3 must not be zero");
    };
    let count = usize::try_from(count).unwrap_or(usize::MAX);
    if count > MAX_BYTES_ALLOC {
        return exc::raise_with_message(
            VALUE_ERROR,
            "bytes length exceeds the runtime allocation limit",
        );
    }
    let mut out = Vec::with_capacity(count);
    let mut current = start;
    for _ in 0..count {
        if !(0..=255).contains(&current) {
            return exc::raise_with_message(VALUE_ERROR, "bytes must be in range(0, 256)");
        }
        out.push(current as u8);
        current += step;
    }
    value::bytes_new(out)
}

/// `bytes(iterable)`: consumes any other iterable (generators, ranges, dict
/// key views, ...) one integer at a time.
///
/// Each element must support the integer conversion; a `0..=255` range check
/// reports CPython's `ValueError`. A value that is not iterable reports the
/// constructor's `TypeError` rather than the iterator protocol's diagnostic,
/// so `bytes(object())` matches CPython's wording.
fn bytes_from_iterable(iterable: i64) -> i64 {
    let iterator = iter::iter_new(iterable);
    if exc::pending() {
        // Not iterable: replace `'X' object is not iterable` with the
        // constructor's message.
        exc::clear();
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "cannot convert '{}' object to bytes",
                value::type_name_of(iterable)
            ),
        );
    }
    let mut out = Vec::new();
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        let Some(byte) = value::as_integer(item) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "'{}' object cannot be interpreted as an integer",
                    value::type_name_of(item)
                ),
            );
        };
        if !(0..=255).contains(&byte) {
            return exc::raise_with_message(VALUE_ERROR, "bytes must be in range(0, 256)");
        }
        if out.len() >= MAX_BYTES_ALLOC {
            return exc::raise_with_message(
                VALUE_ERROR,
                "bytes length exceeds the runtime allocation limit",
            );
        }
        out.push(byte as u8);
    }
    value::bytes_new(out)
}

/// `bytes.fromhex(string)`: decodes pairs of hexadecimal digits.
///
/// CPython accepts `str` and any bytes-like operand (ASCII only in either
/// case), skips ASCII whitespace anywhere, and only allows whitespace between
/// pairs: a whitespace character standing between the two digits of a pair is
/// reported as a non-hexadecimal character at its own position. The error
/// wording and positions match CPython's `bytes.fromhex`/`bytearray.fromhex`.
pub(crate) fn bytes_fromhex(argument: i64) -> i64 {
    // The operand is a sequence of code units: Unicode scalar values for a
    // `str`, bytes for a bytes-like value. Positions in error messages count
    // those units, not bytes of the UTF-8 encoding.
    let units: Vec<u32> = if let Some(text) = value::as_str(argument) {
        text.chars().map(|ch| ch as u32).collect()
    } else if let Some(bytes) = value::as_bytes(argument) {
        bytes.iter().map(|byte| u32::from(*byte)).collect()
    } else if let Some(bytes) = value::as_bytearray(argument) {
        bytes.iter().map(|byte| u32::from(*byte)).collect()
    } else if let Some(view) = value::as_memoryview(argument) {
        match value::memoryview_bytes(view) {
            Ok(bytes) => bytes.into_iter().map(u32::from).collect(),
            Err(message) => return exc::raise_with_message(VALUE_ERROR, message),
        }
    } else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "fromhex() argument must be str or bytes-like, not {}",
                value::type_name(argument)
            ),
        );
    };
    let mut out = Vec::with_capacity(units.len() / 2);
    let mut high: Option<u8> = None;
    for (position, unit) in units.into_iter().enumerate() {
        if matches!(unit, 0x20 | 0x09 | 0x0a | 0x0b | 0x0c | 0x0d) {
            if high.is_some() {
                return exc::raise_with_message(
                    VALUE_ERROR,
                    &format!(
                        "non-hexadecimal number found in fromhex() arg at position {position}"
                    ),
                );
            }
            continue;
        }
        let Some(digit) = hex_digit(unit) else {
            return exc::raise_with_message(
                VALUE_ERROR,
                &format!("non-hexadecimal number found in fromhex() arg at position {position}"),
            );
        };
        match high.take() {
            Some(first) => out.push((first << 4) | digit),
            None => high = Some(digit),
        }
    }
    if high.is_some() {
        return exc::raise_with_message(
            VALUE_ERROR,
            "fromhex() arg must contain an even number of hexadecimal digits",
        );
    }
    value::bytes_new(out)
}

/// Value of one hexadecimal digit code unit, lower- or upper-case.
fn hex_digit(unit: u32) -> Option<u8> {
    match unit {
        0x30..=0x39 => Some((unit - 0x30) as u8),
        0x41..=0x46 => Some((unit - 0x41 + 10) as u8),
        0x61..=0x66 => Some((unit - 0x61 + 10) as u8),
        _ => None,
    }
}

/// `float.fromhex(string)`: parses CPython's hexadecimal floating-point
/// spelling.
///
/// The operand must be a `str`; the grammar is `[sign] [0x] hexdigits
/// [. hexdigits] [p [sign] decimal-digits]` with at least one hexadecimal
/// digit, plus the case-insensitive `inf`/`infinity`/`nan` spellings.
/// Underscores are not accepted (unlike integer hex literals). The sign of a
/// NaN is ignored, matching CPython.
pub(crate) fn float_fromhex(argument: i64) -> i64 {
    let Some(text) = value::as_str(argument) else {
        return exc::raise_with_message(TYPE_ERROR, "bad argument type for built-in operation");
    };
    match parse_hex_float(text) {
        Some(number) => value::float_new(number),
        None => exc::raise_with_message(VALUE_ERROR, "invalid hexadecimal floating-point string"),
    }
}

/// Parses one hexadecimal floating-point string; `None` when it is malformed.
fn parse_hex_float(text: &str) -> Option<f64> {
    let text = text.trim();
    let (negative, body) = match text.as_bytes().first() {
        Some(&b'-') => (true, &text[1..]),
        Some(&b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let lower = body.to_ascii_lowercase();
    if lower == "inf" || lower == "infinity" {
        return Some(if negative {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    if lower == "nan" {
        return Some(f64::NAN);
    }
    let body = body
        .strip_prefix("0x")
        .or_else(|| body.strip_prefix("0X"))
        .unwrap_or(body);
    let mut pieces = body.splitn(2, ['p', 'P']);
    let mantissa = pieces.next().unwrap_or("");
    let exponent = match pieces.next() {
        Some(digits) => parse_hex_exponent(digits)?,
        None => 0,
    };
    let mut value = 0f64;
    let mut exact: u128 = 0;
    let mut exact_overflow = false;
    let mut fraction_digits = 0i32;
    let mut digits = 0usize;
    let mut seen_dot = false;
    for ch in mantissa.chars() {
        if ch == '.' {
            if seen_dot {
                return None;
            }
            seen_dot = true;
            continue;
        }
        let digit = ch.to_digit(16)?;
        digits += 1;
        value = value * 16.0 + f64::from(digit);
        if !exact_overflow {
            match exact
                .checked_mul(16)
                .and_then(|n| n.checked_add(u128::from(digit)))
            {
                Some(next) => exact = next,
                None => exact_overflow = true,
            }
        }
        if seen_dot {
            fraction_digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    // The accumulated digits already include the fractional ones; shift the
    // binary point back by four bits per fractional hexadecimal digit.
    let value = if exact_overflow { value } else { exact as f64 };
    let scale = i64::from(exponent) - 4 * i64::from(fraction_digits);
    let result = if scale > i64::from(i32::MAX) {
        f64::INFINITY
    } else if scale < i64::from(i32::MIN) {
        0.0
    } else {
        value * 2f64.powi(scale as i32)
    };
    Some(if negative { -result } else { result })
}

/// Parses the decimal exponent of a hexadecimal float (`p3`, `P-2`).
fn parse_hex_exponent(digits: &str) -> Option<i32> {
    let (negative, digits) = match digits.as_bytes().first() {
        Some(&b'-') => (true, &digits[1..]),
        Some(&b'+') => (false, &digits[1..]),
        _ => (false, digits),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value: i64 = 0;
    for ch in digits.chars() {
        let digit = ch.to_digit(10)?;
        value = value.saturating_mul(10).saturating_add(i64::from(digit));
    }
    let value = if negative { -value } else { value };
    Some(value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
}

/// `bytearray(...)`: the mutable byte-sequence constructor.
///
/// The 0/1/2/3-argument forms share [`bytes_build`] and wrap its result in a
/// fresh `bytearray`, so `bytearray(str, encoding[, errors])` uses the same
/// codecs and error handlers as `bytes`. A failing conversion keeps the
/// pending exception and returns `None`.
pub(crate) fn bytearray_build(argc: usize, argv: *const i64) -> i64 {
    if argc > 3 {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("bytearray() takes at most 3 arguments ({argc} given)"),
        );
    }
    if argc > 0 && argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            "bytearray() received arguments without an argument buffer",
        );
    }
    let converted = bytes_build(argc, argv);
    if exc::pending() {
        return converted;
    }
    // `bytes_build` yields a fresh `bytes` object for every accepted form; the
    // bytes are copied into the mutable payload before it can be collected.
    match value::as_bytes(converted) {
        Some(bytes) => value::bytearray_new(bytes.to_vec()),
        None => converted,
    }
}

/// `memoryview(value)`: a live buffer view over `bytes`/`bytearray`.
pub(crate) fn memoryview_build(argc: usize, argv: *const i64) -> i64 {
    if argc != 1 {
        return builtin_arity_error("memoryview", "exactly one", argc);
    }
    if argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            "memoryview() received 1 argument without an argument buffer",
        );
    }
    // SAFETY: ABI contract: one readable tagged value.
    let argument = unsafe { *argv };
    value::memoryview_abi(argument)
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

/// `tuple(iterable)`: materialises the elements in iteration order.
///
/// A list is copied into a fresh tuple; other iterables are consumed through
/// the iterator protocol, so generators work. The intermediate list roots the
/// consumed values while the tuple is allocated.
pub(crate) fn tuple_from(iterable: i64) -> i64 {
    let list = list_from(iterable);
    if exc::pending() {
        return NONE;
    }
    let items = match value::as_list(list) {
        Some(items) => items.to_vec(),
        None => Vec::new(),
    };
    value::tuple_new(items)
}

/// Returns the elements of a list/tuple pair candidate.
fn pair_items(v: i64) -> Option<&'static [i64]> {
    value::as_list(v).or_else(|| value::as_tuple(v))
}

/// `dict([source], **kwargs)`: builds the mapping from an optional dict or
/// iterable of pairs and then layers the keyword arguments on top. Keyword
/// names are inserted as `str` keys, so a name that already came from the
/// source overwrites its value (the key keeps its original position), like
/// CPython's `dict({'a': 1}, a=2)`.
pub(crate) fn dict_build(source: Option<i64>, keywords: &[(String, i64)]) -> i64 {
    let result = match source {
        Some(source) => dict_from(source),
        None => value::dict_new(Vec::new()),
    };
    if exc::pending() || keywords.is_empty() {
        return result;
    }
    // The key strings are allocated before the graph lock is taken, and the
    // source values live in Rust memory the collector cannot trace, so root
    // the result and every value across those allocations.
    let mut roots_source = Vec::with_capacity(keywords.len() + 1);
    roots_source.push(result);
    roots_source.extend(keywords.iter().map(|(_, item)| *item));
    let roots = crate::heap::root_values(&roots_source);
    let keys: Vec<i64> = keywords
        .iter()
        .map(|(name, _)| value::str_alloc(name.clone()))
        .collect();
    if let Some(mut pairs) = value::dict_pairs_mut(result) {
        for (key, (_, item)) in keys.iter().zip(keywords.iter()) {
            container::dict_insert_barrier(result, &mut pairs, *key, *item);
        }
    }
    crate::heap::release_roots(roots);
    result
}

/// `dict(iterable)`: copies a dict or consumes an iterable of two-element
/// lists/tuples. Keys must be hashable; duplicate keys keep their first
/// position and the last value. The result itself roots every pair inserted
/// so far.
pub(crate) fn dict_from(iterable: i64) -> i64 {
    // Any mapping source (`dict`, `defaultdict`, a native `dict` subclass
    // instance) copies its pairs directly; everything else is an iterable of
    // two-item sequences.
    if let Some(pairs) = value::mapping_pairs(iterable) {
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
    // A generator fiber is driven directly; wrapping it in an iterator object
    // would re-check the fiber kind and output pointer on every yield.
    if value::as_fiber(iterable)
        .map(|fiber| fiber.kind == crate::fiber::FiberKind::Generator)
        .unwrap_or(false)
    {
        return sum_generator(iterable, start);
    }
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

/// `sum` over a generator fiber, without the iterator wrapper.
///
/// Mirrors [`iter::iter_next`]'s generator path: a finished fiber ends the
/// loop (re-raising its captured exception first), a yield supplies the item.
/// The fiber is resumed with `None`, matching iteration `send` semantics.
fn sum_generator(generator: i64, start: i64) -> i64 {
    // Resolve the fiber payload once. Re-looking-up the tagged object for
    // `done`, `error` and the yielded value costs three kind checks per item
    // on the hottest generator loop.
    let Some(ptr) = value::fiber_raw(generator) else {
        return NONE;
    };
    let mut accumulator = start;
    loop {
        // SAFETY: `generator` stays rooted for this call, so the fiber payload
        // cannot be collected or moved.
        unsafe {
            if (*ptr).done {
                if (*ptr).error != NONE {
                    crate::fiber::raise_error(generator);
                    return NONE;
                }
                return accumulator;
            }
            crate::fiber::resume_raw(ptr, NONE);
            if (*ptr).error != NONE {
                crate::fiber::raise_error(generator);
                return NONE;
            }
            if (*ptr).done {
                return accumulator;
            }
            accumulator = ops::binop(ops::OP_ADD, accumulator, (*ptr).yielded);
        }
        if exc::pending() {
            return NONE;
        }
    }
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
    // SAFETY: ABI contract: `argv` points at `argc` readable values; length checked above.
    unsafe { std::slice::from_raw_parts(argv, argc) }
}

/// `zip(*iterables)`: materialises tuples up to the shortest source.
///
/// Sources and their iterators are kept in a registered list (`roots`), and
/// each row is collected in a registered list before the tuple is allocated,
/// so freshly created string elements stay reachable across collections.
pub(crate) fn zip(argc: usize, argv: *const i64) -> i64 {
    zip_strict(argc, argv, false)
}

/// `zip(*iterables, strict=...)`: [`zip`] plus Python's length check.
///
/// With `strict`, the first source that runs out must be the only one with
/// elements left: an earlier source reports itself as shorter, and otherwise
/// every later source is probed for an extra element, matching CPython's
/// `zip_next` diagnostics (and its `ValueError` tags).
pub(crate) fn zip_strict(argc: usize, argv: *const i64, strict: bool) -> i64 {
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
        let mut ended_at = 0;
        for index in 0..argc {
            let Some(iterator) =
                value::as_list(roots).and_then(|items| items.get(argc + index).copied())
            else {
                ended = true;
                ended_at = index;
                break;
            };
            let mut item = NONE;
            let status = iter::iter_next(iterator, &mut item);
            if exc::pending() {
                return NONE;
            }
            if status == 0 {
                ended = true;
                ended_at = index;
                break;
            }
            if let Some(mut items) = value::list_items_mut(row) {
                items.push(item);
                crate::heap::write_barrier_edge(row, item);
            }
        }
        if ended {
            if strict && !zip_strict_end(roots, argc, ended_at) {
                return NONE;
            }
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

/// The strict `zip` end-of-row check (CPython's `zip_next` `check:` block).
///
/// `ended` is the position of the first source that ran out for this row.
/// A source that ran out after an earlier one produced an element is reported
/// as shorter; only when the first source ended are the remaining ones probed,
/// and an extra element there reports that source as longer. Returns `false`
/// with a pending `ValueError` on a mismatch, `true` when all sources ended
/// together.
fn zip_strict_end(roots: i64, argc: usize, ended: usize) -> bool {
    if ended > 0 {
        let plural = if ended == 1 { " " } else { "s 1-" };
        exc::raise_with_message(
            VALUE_ERROR,
            &format!(
                "zip() argument {} is shorter than argument{plural}{ended}",
                ended + 1
            ),
        );
        return false;
    }
    for index in 1..argc {
        let Some(iterator) =
            value::as_list(roots).and_then(|items| items.get(argc + index).copied())
        else {
            break;
        };
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return false;
        }
        if status != 0 {
            let plural = if index == 1 { " " } else { "s 1-" };
            exc::raise_with_message(
                VALUE_ERROR,
                &format!(
                    "zip() argument {} is longer than argument{plural}{index}",
                    index + 1
                ),
            );
            return false;
        }
    }
    true
}

// First-class functions

/// `super(type, object)`: the explicit two-argument super proxy.
///
/// `object` may be an instance (ordinary method) or a class (`classmethod`
/// calling `super(cls, cls)`); the proxy starts the MRO lookup after `type`,
/// exactly like the receiver form the frontend lowers for `super()`. `None`
/// is not accepted: Hydro has no unbound-super protocol surface, so a bad
/// second argument reports CPython's message.
fn super_two_arg(class: i64, object: i64) -> i64 {
    if value::as_class(class).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "super() argument 1 must be a type, not '{}'",
                value::type_name(class)
            ),
        );
    }
    let start = match value::instance_class_value(object) {
        Some(start) => start,
        None if value::as_class(object).is_some() => object,
        None => {
            return exc::raise_with_message(
                TYPE_ERROR,
                "super(type, obj): obj must be an instance or subtype of type",
            )
        }
    };
    if !value::class_mro(start).contains(&class) {
        return exc::raise_with_message(
            TYPE_ERROR,
            "super(type, obj): obj must be an instance or subtype of type",
        );
    }
    value::super_new(class, object)
}

/// `super(type)`: the unbound super object.
///
/// Attribute access on an unbound proxy has no receiver, so it raises
/// `AttributeError`; the value itself only exists for signature parity.
fn super_unbound(class: i64) -> i64 {
    if value::as_class(class).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "super() argument 1 must be a type, not '{}'",
                value::type_name(class)
            ),
        );
    }
    value::super_new(class, NONE)
}

/// `fn(args...)`: calls a function value.
///
/// Wrapper functions have the C signature `extern "C" fn(*const i64) -> i64`
/// and receive a pointer to `argc` tagged arguments (NULL for zero arguments).
/// Generated code reports errors through the thread-local pending exception
/// itself; its return value is passed through unchanged. Builtin values are
/// dispatched by [`call_builtin`], native module functions by
/// [`crate::modules`] with the same arity and argument-buffer checks.
pub(crate) fn call(function: i64, argc: usize, argv: *const i64) -> i64 {
    // Opaque extension values are extension callables (types construct through
    // their own `tp_new`, instances through `__call__`).
    if value::as_ext_object(function).is_some() {
        return modules::extension::ext_object_call(function, argc, argv);
    }
    if value::as_class(function).is_some() {
        // A Hydro class deriving from an extension type has no C-layout
        // instance of its own; construction defers to the extension base's
        // `tp_new`/vectorcall.
        if let Some(base) = value::extension_class_base(function) {
            return modules::extension::ext_object_call(base, argc, argv);
        }
        return instantiate(function, argc, argv);
    }
    if let Some(state) = value::as_ext_function(function) {
        return modules::extension::call_value(state, argc, argv);
    }
    if let Some(instance) = value::as_instance(function) {
        if value::class_lookup(instance.class, "__call__").is_some() {
            return methods::call(function, b"__call__".as_ptr(), "__call__".len(), argc, argv);
        }
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
            // A dynamic signature stores the minimum positional arity and
            // lets the generated wrapper bind defaults, `*args` and
            // `**kwargs`; an ordinary wrapper takes exactly `arity`
            // arguments.
            let varargs = func.flags & value::FUNCTION_FLAG_VARARGS != 0;
            if varargs {
                // A default that only exists when the function value was
                // created (`def f(x=expr)`) cannot be embedded in the wrapper:
                // the attached signature carries the evaluated value. Route
                // through the shared binder so that value is applied instead
                // of the wrapper's compile-time `None` fallback.
                if func.flags & value::FUNCTION_FLAG_DYNAMIC_DEFAULTS != 0
                    && func.full_entry != 0
                    && !func.signature.is_empty()
                {
                    return call_kw(function, argc, argv, 0, std::ptr::null());
                }
                if argc < arity {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!("function takes at least {arity} arguments ({argc} given)"),
                    );
                }
            } else if arity != argc {
                // Indirect calls (imported aliases, values) bypass the
                // frontend's static default filling. When full metadata is
                // available, let the runtime binder fill omitted defaults
                // and report missing/extra positional arguments precisely.
                if func.full_entry != 0 && !func.signature.is_empty() {
                    return call_kw(function, argc, argv, 0, std::ptr::null());
                }
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("function takes {arity} arguments ({argc} given)"),
                );
            }
            if *entry == 0 {
                return exc::raise_with_message(RUNTIME_ERROR, "function has no code");
            }
            // The wrapper's argument buffer for a dynamic signature starts
            // with the tagged total count (captures + user arguments), so the
            // wrapper can tell how many positional arguments the caller
            // supplied.
            let total = captures.len() + argc;
            let count_value = if varargs {
                match value::encode_int(total as i64) {
                    Some(count) => Some(count),
                    None => {
                        return exc::raise_with_message(
                            RUNTIME_ERROR,
                            "too many arguments for the call frame",
                        )
                    }
                }
            } else {
                None
            };
            // A generator/coroutine call does not run the body: it creates a
            // lazy fiber object. The wrapper receives captures followed by
            // the user arguments on its first resume.
            if func.flags & (value::FUNCTION_FLAG_ASYNC | value::FUNCTION_FLAG_GENERATOR) != 0 {
                let mut args: Vec<i64> = Vec::with_capacity(usize::from(varargs) + total);
                if let Some(count) = count_value {
                    args.push(count);
                }
                args.extend_from_slice(captures);
                if argc > 0 {
                    // SAFETY: checked above for NULL; caller contract.
                    let rest = unsafe { std::slice::from_raw_parts(argv, argc) };
                    args.extend_from_slice(rest);
                }
                let name = func.name.clone();
                let entry = *entry;
                let roots = crate::heap::root_values(&args);
                let created = if func.flags & value::FUNCTION_FLAG_ASYNC != 0 {
                    if func.flags & value::FUNCTION_FLAG_GENERATOR != 0 {
                        value::fiber_async_generator_new(entry, name, args)
                    } else {
                        value::fiber_coroutine_new(entry, name, args)
                    }
                } else {
                    value::fiber_generator_new(entry, name, args)
                };
                crate::heap::release_roots(roots);
                return match created {
                    Some(fiber) => fiber,
                    None => exc::raise_with_message(
                        RUNTIME_ERROR,
                        "cannot allocate a generator/coroutine stack",
                    ),
                };
            }
            // The generated wrapper takes captures followed by user
            // arguments. Avoid a copy for ordinary functions.
            let mut buffer: Vec<i64> = Vec::new();
            let argv = if !varargs && captures.is_empty() {
                if argc == 0 {
                    std::ptr::null()
                } else {
                    argv
                }
            } else {
                let prefix = usize::from(varargs);
                buffer.reserve(prefix + total);
                if let Some(count) = count_value {
                    buffer.push(count);
                }
                buffer.extend_from_slice(captures);
                if argc > 0 {
                    // SAFETY: caller passed a valid buffer of `argc` values; NULL checked above.
                    let args = unsafe { std::slice::from_raw_parts(argv, argc) };
                    buffer.extend_from_slice(args);
                }
                buffer.as_ptr()
            };
            // SAFETY: `entry` is a generated entry point with this exact signature
            // (ARCHITECTURE §3/§5); the callee only reads the buffer up to the checked arity.
            let entry: unsafe extern "C" fn(*const i64) -> i64 =
                unsafe { std::mem::transmute(*entry) };
            // SAFETY: the generated wrapper is called in-process with its emitted
            // calling convention; the copied buffer is rooted while the callee may allocate.
            let roots = crate::heap::root_values(&buffer);
            let result = unsafe { entry(argv) };
            crate::heap::release_roots(roots);
            result
        }
        FunctionBody::Host { token, captures } => {
            // Interpreter functions carry full parameter metadata. Positional
            // calls with a complete, plain signature dispatch directly; any
            // omitted default, keyword-only/vararg slot or dynamic signature
            // goes through the shared runtime binder first.
            let expected = func.signature.len();
            let varargs = func.flags & value::FUNCTION_FLAG_VARARGS != 0;
            if expected != 0 && (varargs || argc != expected) {
                return call_kw(function, argc, argv, 0, std::ptr::null());
            }
            if expected == 0 && argc != 0 {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("{}() takes no arguments ({argc} given)", func.name),
                );
            }
            let args: &[i64] = if argc == 0 {
                &[]
            } else {
                // SAFETY: NULL with argc > 0 was rejected above; the caller
                // guarantees a readable buffer of `argc` tagged values.
                unsafe { std::slice::from_raw_parts(argv, argc) }
            };
            crate::host::call_slots(*token, captures, args, func.flags, &func.name)
        }
        FunctionBody::Native { module, name } => modules::call_native(module, name, argc, argv),
        FunctionBody::Hna { module, index } => {
            crate::native::call_function(*module, *index, argc, argv)
        }
        FunctionBody::BoundMethod { receiver, method } => {
            crate::core::methods::call(*receiver, method.as_ptr(), method.len(), argc, argv)
        }
        FunctionBody::Builtin(builtin) => call_builtin(*builtin, argc, argv),
        FunctionBody::AttributeGetter { names } => attribute_getter_call(names, argc, argv),
        FunctionBody::ItemGetter { keys } => item_getter_call(keys, argc, argv),
        FunctionBody::MethodCaller { name } => {
            if argc == 0 || argv.is_null() {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "methodcaller() takes at least one argument (0 given)",
                );
            }
            methods::call(
                unsafe { *argv },
                name.as_ptr(),
                name.len(),
                argc - 1,
                unsafe { argv.add(1) },
            )
        }
        FunctionBody::Partial {
            function: inner,
            args: bound,
            keywords,
        } => {
            if argc > 0 && argv.is_null() {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "partial call received arguments without an argument buffer",
                );
            }
            let mut call_args = Vec::with_capacity(bound.len() + argc);
            call_args.extend_from_slice(bound);
            if argc > 0 {
                // SAFETY: NULL checked above; the ABI contract makes `argv..argv+argc` readable.
                let supplied = unsafe { std::slice::from_raw_parts(argv, argc) };
                call_args.extend_from_slice(supplied);
            }
            let roots = crate::heap::root_values(&call_args);
            let result = if keywords.is_empty() {
                call(*inner, call_args.len(), call_args.as_ptr())
            } else {
                // The bound keyword values move into the target's positional
                // slots in parameter order.
                call_with_keywords(*inner, call_args, keywords)
            };
            crate::heap::release_roots(roots);
            result
        }
        FunctionBody::Identity => {
            if argc != 1 || argv.is_null() {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("{}() takes exactly one argument", func.name),
                );
            }
            // SAFETY: exactly one argument was checked above.
            unsafe { *argv }
        }
        FunctionBody::Exec {
            check,
            globals,
            locals,
            ..
        } => exec_check_call(&func.name, check, *globals, *locals, argc, argv),
    }
}

/// Runs one function compiled by the [`exec_builtin`] subset.
///
/// Exactly one positional argument is accepted; the call has no keyword or
/// default slots. Names in the check expression resolve in `globals` first and
/// then `locals` (the bounded exec semantics), and a missing name raises the
/// same `NameError` an ordinary function would.
fn exec_check_call(
    function_name: &str,
    check: &value::ExecCheck,
    globals_ns: i64,
    locals_ns: i64,
    argc: usize,
    argv: *const i64,
) -> i64 {
    let expected = if matches!(check, value::ExecCheck::RaiseFrom) {
        2
    } else {
        1
    };
    if argc != expected || argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{function_name}() takes exactly {expected} positional argument(s) ({argc} given)"
            ),
        );
    }
    // SAFETY: exactly `expected` arguments were checked above.
    let argument = unsafe { *argv };
    match check {
        value::ExecCheck::RaiseFrom => {
            // SAFETY: two positional parameters were checked above.
            let cause = unsafe { *argv.add(1) };
            exc::raise_value_from(argument, cause)
        }
        value::ExecCheck::AlwaysFalse => value::FALSE,
        value::ExecCheck::Identity { name } => {
            let Some(target) = exec_lookup(globals_ns, locals_ns, name) else {
                return exec_name_error(name);
            };
            ops::binop(BinOp::Is.code(), argument, target)
        }
        value::ExecCheck::IdentityEither { first, second } => {
            let Some(target) = exec_lookup(globals_ns, locals_ns, first) else {
                return exec_name_error(first);
            };
            if ops::binop(BinOp::Is.code(), argument, target) == value::TRUE {
                return value::TRUE;
            }
            let Some(target) = exec_lookup(globals_ns, locals_ns, second) else {
                return exec_name_error(second);
            };
            ops::binop(BinOp::Is.code(), argument, target)
        }
        value::ExecCheck::IsInstance { name } => {
            let Some(target) = exec_lookup(globals_ns, locals_ns, name) else {
                return exec_name_error(name);
            };
            let roots = crate::heap::root_values(&[argument, target]);
            let result = isinstance(argument, target);
            crate::heap::release_roots(roots);
            result
        }
    }
}

/// Looks a name up in the mappings an exec-compiled function captured.
fn exec_lookup(globals_ns: i64, locals_ns: i64, name: &str) -> Option<i64> {
    namespace_get(globals_ns, name).or_else(|| {
        if locals_ns == globals_ns {
            None
        } else {
            namespace_get(locals_ns, name)
        }
    })
}

fn exec_name_error(name: &str) -> i64 {
    exc::raise_with_message(NAME_ERROR, &format!("name '{name}' is not defined"))
}

/// Calls `target` with `args` plus keyword pairs bound by parameter name.
///
/// Used by `functools.partial`: only a generated wrapper with a plain
/// positional signature carries its parameter names (codegen attaches them
/// through `hydro_function_params`). Every keyword must name a parameter that is
/// not already filled by a positional argument; a hole before a named
/// parameter would need the target's defaults, which are applied by the
/// wrapper, so it is reported as a missing argument. A callable without
/// parameter names (native function, builtin, getter or a dynamic signature)
/// raises `TypeError` instead of dropping the keywords.
fn call_with_keywords(target: i64, args: Vec<i64>, keywords: &[(String, i64)]) -> i64 {
    if let Some(func) = value::as_function(target) {
        if let FunctionBody::BoundMethod { receiver, method } = &func.body {
            let mut pairs: Vec<i64> = Vec::with_capacity(keywords.len() * 2);
            for (name, value) in keywords {
                pairs.push(value::str_alloc(name.clone()));
                pairs.push(*value);
            }
            return methods::call_kw(
                *receiver,
                method.as_ptr(),
                method.len(),
                args.len(),
                args.as_ptr(),
                keywords.len(),
                pairs.as_ptr(),
            );
        }
        if !value::function_signature(target).is_empty() && value::function_full_entry(target) != 0
        {
            let mut pairs: Vec<i64> = Vec::with_capacity(keywords.len() * 2);
            for (name, value) in keywords {
                pairs.push(value::str_alloc(name.clone()));
                pairs.push(*value);
            }
            return call_kw(
                target,
                args.len(),
                args.as_ptr(),
                keywords.len(),
                pairs.as_ptr(),
            );
        }
    }
    let Some(func) = value::as_function(target) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("'{}' object is not callable", value::type_name_of(target)),
        );
    };
    let name = func.name.clone();
    let varargs = func.flags & value::FUNCTION_FLAG_VARARGS != 0;
    if varargs || !matches!(&func.body, FunctionBody::Wrapper { .. }) {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("{name}() does not accept keyword arguments"),
        );
    }
    let params = value::function_params(target);
    // Positional-only parameters never receive a keyword value; they always
    // form the leading run of the positional names.
    let posonly = value::function_posonly_count(target);
    let mut slots: Vec<Option<i64>> = vec![None; params.len()];
    for (index, &arg) in args.iter().enumerate() {
        let Some(slot) = slots.get_mut(index) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "{name}() takes {} positional arguments ({} given)",
                    params.len(),
                    args.len()
                ),
            );
        };
        *slot = Some(arg);
    }
    for (keyword, value) in keywords {
        let Some(index) = params
            .iter()
            .skip(posonly)
            .position(|param| param == keyword)
            .map(|offset| offset + posonly)
        else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("{name}() got an unexpected keyword argument '{keyword}'"),
            );
        };
        if slots[index].is_some() {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("{name}() got multiple values for argument '{keyword}'"),
            );
        }
        slots[index] = Some(*value);
    }
    let mut bound: Vec<i64> = Vec::with_capacity(slots.len());
    for (index, slot) in slots.into_iter().enumerate() {
        match slot {
            Some(value) => bound.push(value),
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "{name}() missing required positional argument '{}'",
                        params[index]
                    ),
                )
            }
        }
    }
    call(target, bound.len(), bound.as_ptr())
}

/// Materialises the operand of a starred call into positional values.
///
/// Lists and tuples are copied directly. Every other iterable is consumed
/// through the iterator ABI: the collected values live in a temporary Hydro list
/// so a collection during generator resume or `next` cannot lose them.
pub(crate) fn star_sequence(star: i64) -> Result<Vec<i64>, i64> {
    if let Some(items) = value::as_list(star).or_else(|| value::as_tuple(star)) {
        return Ok(items.to_vec());
    }
    let iterator = iter::iter_new(star);
    if exc::pending() {
        return Err(NONE);
    }
    let collected = value::list_new(Vec::new());
    loop {
        let mut item = NONE;
        let more = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return Err(NONE);
        }
        if more == 0 {
            break;
        }
        container::list_append(collected, item);
        if exc::pending() {
            return Err(NONE);
        }
    }
    Ok(value::as_list(collected)
        .map(|items| items.to_vec())
        .unwrap_or_default())
}

/// `fn(args..., *star)`: calls a function value with one starred positional
/// operand.
///
/// The elements of `star`, any iterable, are appended to the `argc` positional
/// operands in `argv` before dispatching through [`call`]. Non-iterables raise
/// the iterator `TypeError`; a NULL `argv` with a non-zero
/// `argc` is rejected like [`call`] does.
pub(crate) fn call_star(function: i64, argc: usize, argv: *const i64, star: i64) -> i64 {
    if argc > 0 && argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("function call received {argc} argument(s) without an argument buffer"),
        );
    }
    if argc > isize::MAX as usize / std::mem::size_of::<i64>() {
        return exc::raise_with_message(TYPE_ERROR, "function call has too many arguments");
    }
    // Copy the flattened elements before allocating the combined buffer: the
    // source list/tuple stays a live argument (and therefore a GC root) while
    // the call runs.
    let items = match star_sequence(star) {
        Ok(items) => items,
        Err(err) => return err,
    };
    let Some(total) = argc.checked_add(items.len()) else {
        return exc::raise_with_message(TYPE_ERROR, "function call has too many arguments");
    };
    // SAFETY: ABI contract: `argv` points at `argc` readable values; length checked above.
    let mut args: Vec<i64> = Vec::with_capacity(total);
    if argc > 0 {
        unsafe {
            args.extend_from_slice(std::slice::from_raw_parts(argv, argc));
        }
    }
    args.extend_from_slice(&items);
    let roots = crate::heap::root_values(&args);
    let result = call(function, args.len(), args.as_ptr());
    crate::heap::release_roots(roots);
    result
}

/// `fn(args..., *star, name=value..., **star_map)`: the general indirect call.
///
/// `star`/`star_map` are `NONE` when the caller used no `*`/`**` operand.
/// Positional operands are flattened first (the `argv` prefix, then `star`),
/// named keywords are merged with the `star_map` entries in mapping order, and
/// the result dispatches through [`call_kw`] exactly like a literal keyword
/// call. Duplicate keyword names and non-string mapping keys report CPython's
/// `TypeError`; the flattened values stay rooted while user code runs.
pub(crate) fn call_star_kw(
    function: i64,
    argc: usize,
    argv: *const i64,
    star: i64,
    kwargc: usize,
    kw_argv: *const i64,
    star_map: i64,
) -> i64 {
    if argc > 0 && argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("function call received {argc} argument(s) without an argument buffer"),
        );
    }
    if argc > isize::MAX as usize / std::mem::size_of::<i64>()
        || kwargc > isize::MAX as usize / (2 * std::mem::size_of::<i64>())
    {
        return exc::raise_with_message(TYPE_ERROR, "function call has too many arguments");
    }
    let name = value::as_function(function)
        .map(|func| func.name.clone())
        .unwrap_or_else(|| "function".to_string());
    // SAFETY: ABI contract: `argv`/`kw_argv` point at readable buffers whose
    // lengths were validated above.
    let mut args: Vec<i64> = if argc == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec()
    };
    if star != NONE {
        match star_sequence(star) {
            Ok(items) => args.extend_from_slice(&items),
            Err(err) => return err,
        }
    }
    let mut keywords: Vec<(String, i64)> = Vec::with_capacity(kwargc);
    if kwargc > 0 {
        if kw_argv.is_null() {
            return exc::raise_with_message(
                TYPE_ERROR,
                "keyword arguments were passed without an argument buffer",
            );
        }
        // SAFETY: ABI contract: `kw_argv` holds `2 * kwargc` readable values.
        let pairs = unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) };
        for pair in pairs.as_chunks::<2>().0 {
            let Some(keyword) = value::as_str(pair[0]) else {
                return exc::raise_with_message(TYPE_ERROR, "keyword name must be str");
            };
            keywords.push((keyword.to_string(), pair[1]));
        }
    }
    if star_map != NONE {
        let Some(mapping) = value::as_dict(star_map) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "argument after ** must be a mapping, not '{}'",
                    value::type_name(star_map)
                ),
            );
        };
        // Copy before allocating: the slice borrows the dict.
        let entries: Vec<(i64, i64)> = mapping.to_vec();
        for (key, item) in entries {
            let Some(keyword) = value::as_str(key) else {
                return exc::raise_with_message(TYPE_ERROR, "keywords must be strings");
            };
            if keywords
                .iter()
                .any(|(existing, _)| existing.as_str() == keyword)
            {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("{name}() got multiple values for keyword argument '{keyword}'"),
                );
            }
            keywords.push((keyword.to_string(), item));
        }
    }
    // Publish every flattened positional and keyword value as a root: the
    // vectors are native memory the collector cannot scan, and the keyword
    // name allocations below can collect.
    let mut roots_source: Vec<i64> = args.clone();
    roots_source.extend(keywords.iter().map(|(_, item)| *item));
    let roots = crate::heap::root_values(&roots_source);
    let mut pairs: Vec<i64> = Vec::with_capacity(keywords.len() * 2);
    for (keyword, item) in &keywords {
        pairs.push(value::str_alloc(keyword.clone()));
        pairs.push(*item);
    }
    let result = call_kw(
        function,
        args.len(),
        args.as_ptr(),
        keywords.len(),
        pairs.as_ptr(),
    );
    crate::heap::release_roots(roots);
    result
}

/// `fn(args..., name=value...)`: runtime keyword binding for a function value.
///
/// Generated functions carry their full parameter metadata and a uniform
/// full-argv trampoline. Builtins with named parameters (`print`, `zip`,
/// `max`/`min`, `eval`/`exec` and `__import__`) bind their keywords here;
/// every other native/builtin value validates its own positional arity and
/// rejects keywords.
pub(crate) fn call_kw(
    function: i64,
    argc: usize,
    argv: *const i64,
    kwargc: usize,
    kw_argv: *const i64,
) -> i64 {
    if argc > 0 && argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("function call received {argc} argument(s) without an argument buffer"),
        );
    }
    if kwargc == 0 {
        // A generated function may still use its metadata for omitted
        // defaults; only native/builtin values fall back to the positional
        // call here.
        if value::function_signature(function).is_empty()
            || value::function_full_entry(function) == 0
        {
            return call(function, argc, argv);
        }
    }
    if kwargc > 0
        && (kw_argv.is_null() || kwargc > isize::MAX as usize / (2 * std::mem::size_of::<i64>()))
    {
        return exc::raise_with_message(
            TYPE_ERROR,
            "keyword arguments were passed without an argument buffer",
        );
    }
    if value::as_ext_object(function).is_some() {
        return modules::extension::ext_object_call_kw(function, argc, argv, kwargc, kw_argv);
    }
    if value::as_class(function).is_some() {
        let class = function;
        if let Some(base) = value::extension_class_base(class) {
            return modules::extension::ext_object_call_kw(base, argc, argv, kwargc, kw_argv);
        }
        if crate::modules::typing::is_typeddict_class(class) {
            let args = if argc == 0 {
                Vec::new()
            } else {
                // SAFETY: checked above; caller contract.
                unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec()
            };
            let pairs = unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) };
            let mut keywords = Vec::with_capacity(kwargc);
            for pair in pairs.as_chunks::<2>().0 {
                let Some(keyword) = value::as_str(pair[0]) else {
                    return exc::raise_with_message(TYPE_ERROR, "keyword name must be str");
                };
                keywords.push((keyword.to_string(), pair[1]));
            }
            return typed_dict_construct(&args, &keywords);
        }
        let args = if argc == 0 {
            Vec::new()
        } else {
            // SAFETY: checked above; caller contract.
            unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec()
        };
        let pairs = unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) };
        let mut keywords = Vec::with_capacity(kwargc);
        for pair in pairs.as_chunks::<2>().0 {
            let Some(keyword) = value::as_str(pair[0]) else {
                return exc::raise_with_message(TYPE_ERROR, "keyword name must be str");
            };
            keywords.push((keyword.to_string(), pair[1]));
        }
        if let Some(tag) = exc::exception_tag_of_class(class) {
            return instantiate_exception_kw(tag, &args, &keywords);
        }
        // `functools.partial` is a real class value (its type checks and PEP
        // 604 union membership need one), so calling the value must still
        // build the partial function object rather than a blank instance.
        if value::is_partial_class(class) {
            let Some((&target, bound)) = args.split_first() else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "type 'functools.partial' takes at least one argument",
                );
            };
            return value::partial_new_with_keywords(target, bound.to_vec(), keywords);
        }
        // `datetime.date(...)`, `datetime.timedelta(days=...)`, ... bind their
        // declared parameters by name. The name check keeps the registry
        // lookups off the hot path for ordinary user classes.
        if matches!(
            value::class_display_name(class),
            Some("date" | "time" | "datetime" | "timedelta")
        ) {
            if let Some(result) = crate::modules::datetime::instantiate_kw(class, &args, &keywords)
            {
                return result;
            }
        }
        // `socket.SocketType`/`socket.socket` is the cached class `type()`
        // reports for sockets; its constructor is the socket factory, with
        // `family`/`type`/`proto` bound by name.
        if value::class_display_name(class) == Some("socket") && value::socket_class() == class {
            return crate::modules::socket::call_kw("socket", "socket", &args, keywords);
        }
        // The cached `builtins.frozendict` class builds an immutable mapping
        // instance (its keywords are data entries) instead of a blank native
        // instance.
        if value::class_display_name(class) == Some("frozendict")
            && globals::registry_get("builtins.frozendict") == Some(class)
        {
            return crate::modules::builtins::frozendict_instantiate(&args, &keywords);
        }
        // `collections.defaultdict`'s module value is a class (it carries the
        // `__repr__` that `pprint` keys its dispatch table by), so calling the
        // value must still run the real constructor.
        if value::class_display_name(class) == Some("defaultdict")
            && globals::registry_get("collections.defaultdict.class") == Some(class)
        {
            return crate::modules::collections::call_kw("defaultdict", &args, &keywords);
        }
        // `codecs.CodecInfo(...)` builds the native codec-registry entry.
        if value::class_display_name(class) == Some("CodecInfo")
            && globals::registry_get("codecs.CodecInfo.class") == Some(class)
        {
            return crate::modules::codecs_codecinfo_new(&args, &keywords);
        }
        // The `pathlib` pure-path classes construct the same native `Path`
        // value as `pathlib.Path`; the helper validates the identity and the
        // platform-specific `PosixPath`/`WindowsPath` rules.
        if matches!(
            value::class_display_name(class),
            Some("PurePath" | "PosixPath" | "WindowsPath" | "PurePosixPath" | "PureWindowsPath")
        ) {
            if let Some(result) = modules::pathlib_instantiate(class, &args, &keywords) {
                return result;
            }
        }
        // `type.__call__` runs a compiled `__new__` before the default
        // allocation and `__init__` (pure-Python `_pydatetime` classes build
        // themselves there).
        if let Some(result) = call_user_new(class, &args, &keywords) {
            return result;
        }
        let instance = value::instance_new(class);
        let Some((kind, init)) = value::class_member(class, "__init__") else {
            if !args.is_empty() || !keywords.is_empty() {
                let name = value::class_display_name(class)
                    .unwrap_or("class")
                    .to_string();
                if let Some((keyword, _)) = keywords.first() {
                    // A built-in exception class keeps its constructor contract
                    // even when it reaches the generic class path (for example
                    // after its tag cache entry was dropped): name the class
                    // and the offending keyword instead of the generic
                    // "class takes no arguments". User exception classes with
                    // no `__init__` inherit `BaseException`'s keyword refusal.
                    let is_exception = value::exception_tag_of_derived_class(class).is_some()
                        || hydro_ir::ExcKind::from_class_name(&name).is_some()
                        || hydro_ir::ExcKind::from_name(&name).is_some();
                    let message = if is_exception {
                        format!("{name}() got an unexpected keyword argument '{keyword}'")
                    } else {
                        format!("{name}() takes no keyword arguments")
                    };
                    return exc::raise_with_message(TYPE_ERROR, &message);
                }
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("{name}() takes no arguments"),
                );
            }
            return instance;
        };
        if value::function_signature(init).is_empty() || value::function_full_entry(init) == 0 {
            return exc::raise_with_message(
                TYPE_ERROR,
                "class constructor does not support keyword arguments",
            );
        }
        let _ =
            methods::call_user_method(instance, class, kind, init, "__init__", &args, &keywords);
        return if exc::pending() { NONE } else { instance };
    }
    // An instance with a `__call__` accepts keywords through the method
    // binder, exactly like its positional call path. This is how a
    // `functools.singledispatch` dispatcher forwards `**kwargs` to the
    // selected implementation.
    if let Some(instance) = value::as_instance(function) {
        if value::class_lookup(instance.class, "__call__").is_some() {
            return methods::call_kw(
                function,
                b"__call__".as_ptr(),
                "__call__".len(),
                argc,
                argv,
                kwargc,
                kw_argv,
            );
        }
    }
    if let Some(func) = value::as_function(function) {
        if let FunctionBody::Hna { module, index } = &func.body {
            return crate::native::call_function_kw(*module, *index, argc, argv, kwargc, kw_argv);
        }
        if let FunctionBody::BoundMethod { receiver, method } = &func.body {
            return methods::call_kw(
                *receiver,
                method.as_ptr(),
                method.len(),
                argc,
                argv,
                kwargc,
                kw_argv,
            );
        }
        // Native functions whose spec declares dynamic keywords
        // (`contextlib`'s `contextmanager` wrapper) forward the names to the
        // callee through the module dispatcher instead of rejecting them.
        if let FunctionBody::Native { module, name } = &func.body {
            if crate::modules::native_accepts_keywords(module, name) {
                return crate::modules::call_native_kw(module, name, argc, argv, kwargc, kw_argv);
            }
        }
        // `zip(*iterables, strict=...)`: the only keyword `zip` accepts. The
        // flag is evaluated like CPython's `PyObject_IsTrue`, so any object
        // works; a duplicate or unknown keyword reports `TypeError` without
        // dropping it.
        if matches!(&func.body, FunctionBody::Builtin(Builtin::Zip)) {
            let mut strict = false;
            let mut seen = false;
            if kwargc > 0 {
                if kw_argv.is_null() {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        "keyword arguments were passed without an argument buffer",
                    );
                }
                // SAFETY: `kw_argv` holds `2 * kwargc` readable values.
                let pairs = unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) };
                for pair in pairs.as_chunks::<2>().0 {
                    let Some(keyword) = value::as_str(pair[0]) else {
                        return exc::raise_with_message(TYPE_ERROR, "keyword name must be str");
                    };
                    if keyword != "strict" {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            &format!("zip() got an unexpected keyword argument '{keyword}'"),
                        );
                    }
                    if seen {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            "zip() got multiple values for keyword argument 'strict'",
                        );
                    }
                    strict = value::truthy(pair[1]);
                    seen = true;
                }
            }
            return zip_strict(argc, argv, strict);
        }
        // `print(*args, sep=..., end=..., file=..., flush=...)` is one of the
        // builtins with runtime keyword parameters.
        if matches!(&func.body, FunctionBody::Builtin(Builtin::Print)) {
            let mut keywords: Vec<(String, i64)> = Vec::with_capacity(kwargc);
            if kwargc > 0 {
                if kw_argv.is_null() {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        "keyword arguments were passed without an argument buffer",
                    );
                }
                // SAFETY: `kw_argv` holds `2 * kwargc` readable values.
                let pairs = unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) };
                for pair in pairs.as_chunks::<2>().0 {
                    let Some(keyword) = value::as_str(pair[0]) else {
                        return exc::raise_with_message(TYPE_ERROR, "keyword name must be str");
                    };
                    keywords.push((keyword.to_string(), pair[1]));
                }
            }
            return print_with_keywords(argc, argv, &keywords);
        }
        // `__import__(name, globals=None, locals=None, fromlist=(), level=0)`:
        // CPython 3.14 accepts every parameter by name. Merge the positional
        // prefix with the keyword pairs into the five-slot positional vector
        // the module entry point reads, padding omitted slots with `None`.
        // Unknown and duplicate names are rejected instead of being dropped.
        if matches!(&func.body, FunctionBody::Builtin(Builtin::Import)) {
            const PARAMS: [&str; 5] = ["name", "globals", "locals", "fromlist", "level"];
            if argc > PARAMS.len() {
                return builtin_arity_error("__import__", "1 to 5", argc);
            }
            let mut slots: Vec<Option<i64>> = vec![None; PARAMS.len()];
            if argc > 0 {
                // SAFETY: NULL with argc > 0 was rejected at entry.
                let positional = unsafe { std::slice::from_raw_parts(argv, argc) };
                for (index, argument) in positional.iter().enumerate() {
                    slots[index] = Some(*argument);
                }
            }
            if kwargc > 0 {
                if kw_argv.is_null() {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        "keyword arguments were passed without an argument buffer",
                    );
                }
                // SAFETY: `kw_argv` holds `2 * kwargc` readable values.
                let pairs = unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) };
                for pair in pairs.as_chunks::<2>().0 {
                    let Some(keyword) = value::as_str(pair[0]) else {
                        return exc::raise_with_message(TYPE_ERROR, "keyword name must be str");
                    };
                    let Some(index) = PARAMS.iter().position(|param| *param == keyword) else {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            &format!("__import__() got an unexpected keyword argument '{keyword}'"),
                        );
                    };
                    if slots[index].is_some() {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            &format!("__import__() got multiple values for argument '{keyword}'"),
                        );
                    }
                    slots[index] = Some(pair[1]);
                }
            }
            if slots[0].is_none() {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "__import__() missing required argument 'name' (pos 1)",
                );
            }
            let bound: Vec<i64> = slots.into_iter().map(|slot| slot.unwrap_or(NONE)).collect();
            let roots = crate::heap::root_values(&bound);
            let result = crate::modules::builtins::import_from_core(&bound);
            crate::heap::release_roots(roots);
            return result;
        }
        // Partial application forwards keywords to the wrapped callable:
        // `classmethod(f)` binds the class positionally and must still accept
        // `C.f(k=1)`; the same path serves `functools.partial`.
        if let FunctionBody::Partial {
            function: inner,
            args: bound_args,
            keywords: bound_keywords,
        } = &func.body
        {
            let inner = *inner;
            let mut merged_args: Vec<i64> = bound_args.clone();
            if argc > 0 {
                // SAFETY: NULL checked at entry; `argv..argv+argc` is readable.
                let supplied = unsafe { std::slice::from_raw_parts(argv, argc) };
                merged_args.extend_from_slice(supplied);
            }
            let mut merged_keywords: Vec<(String, i64)> = bound_keywords.clone();
            if kwargc > 0 {
                // SAFETY: `kw_argv` holds `2 * kwargc` readable values.
                let pairs = unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) };
                for pair in pairs.as_chunks::<2>().0 {
                    let Some(keyword) = value::as_str(pair[0]) else {
                        return exc::raise_with_message(TYPE_ERROR, "keyword name must be str");
                    };
                    if merged_keywords
                        .iter()
                        .any(|(existing, _)| existing.as_str() == keyword)
                    {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            &format!(
                                "{}() got multiple values for keyword argument '{keyword}'",
                                func.name
                            ),
                        );
                    }
                    merged_keywords.push((keyword.to_string(), pair[1]));
                }
            }
            let mut roots_source = merged_args.clone();
            roots_source.extend(merged_keywords.iter().map(|(_, item)| *item));
            let roots = crate::heap::root_values(&roots_source);
            let mut pairs: Vec<i64> = Vec::with_capacity(merged_keywords.len() * 2);
            for (keyword, item) in &merged_keywords {
                pairs.push(value::str_alloc(keyword.clone()));
                pairs.push(*item);
            }
            let result = call_kw(
                inner,
                merged_args.len(),
                merged_args.as_ptr(),
                merged_keywords.len(),
                pairs.as_ptr(),
            );
            crate::heap::release_roots(roots);
            return result;
        }
    }
    let Some(func) = value::as_function(function) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("'{}' object is not callable", value::type_name_of(function)),
        );
    };
    let name = func.name.clone();
    // `eval`/`exec` are builtins with named parameters. Bind `source`,
    // `globals` and `locals` keywords into the positional slots their runtime
    // entry points expect; every other builtin rejects keywords (matching its
    // direct call lowering).
    if let FunctionBody::Builtin(builtin) = &func.body {
        // `type(name, bases, namespace, **kwds)`: CPython forwards the
        // class-header keywords to the metaclass resolved from the bases
        // (`EnumType.__new__(..., boundary=..., _simple=...)`). `type(x, **kw)`
        // has no keyword parameters and is rejected.
        if matches!(builtin, Builtin::Type) {
            let mut keywords: Vec<(String, i64)> = Vec::with_capacity(kwargc);
            if kwargc > 0 {
                if kw_argv.is_null() {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        "keyword arguments were passed without an argument buffer",
                    );
                }
                // SAFETY: `kw_argv` holds `2 * kwargc` readable values.
                let pairs = unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) };
                for pair in pairs.as_chunks::<2>().0 {
                    let Some(keyword) = value::as_str(pair[0]) else {
                        return exc::raise_with_message(TYPE_ERROR, "keyword name must be str");
                    };
                    if keywords
                        .iter()
                        .any(|(existing, _)| existing.as_str() == keyword)
                    {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            &format!("type() got multiple values for keyword argument '{keyword}'"),
                        );
                    }
                    keywords.push((keyword.to_string(), pair[1]));
                }
            }
            let roots_source: Vec<i64> = keywords.iter().map(|(_, item)| *item).collect();
            let roots = crate::heap::root_values(&roots_source);
            let result = type_build_kw(argc, argv, &keywords);
            crate::heap::release_roots(roots);
            return result;
        }
        // `dict([source], **kwargs)`: CPython builds the mapping from the
        // optional positional source and then layers the keyword arguments on
        // top, with later keys winning. The keyword names are not declared
        // parameters, so the binder collects them all.
        if matches!(builtin, Builtin::Dict) {
            if argc > 1 {
                return builtin_arity_error("dict", "at most 1", argc);
            }
            let source = if argc == 0 {
                None
            } else {
                // SAFETY: nil with argc > 0 was rejected by the caller.
                Some(unsafe { *argv })
            };
            let mut keywords: Vec<(String, i64)> = Vec::with_capacity(kwargc);
            if kwargc > 0 {
                if kw_argv.is_null() {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        "keyword arguments were passed without an argument buffer",
                    );
                }
                // SAFETY: `kw_argv` holds `2 * kwargc` readable values.
                let pairs = unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) };
                for pair in pairs.as_chunks::<2>().0 {
                    let Some(keyword) = value::as_str(pair[0]) else {
                        return exc::raise_with_message(TYPE_ERROR, "keywords must be strings");
                    };
                    keywords.push((keyword.to_string(), pair[1]));
                }
            }
            return dict_build(source, &keywords);
        }
        if matches!(builtin, Builtin::Eval | Builtin::Exec) {
            let args = if argc == 0 {
                Vec::new()
            } else {
                // SAFETY: nil with argc > 0 was rejected by the caller.
                unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec()
            };
            if args.len() > 3 {
                return builtin_arity_error(builtin.name(), "1 to 3", args.len());
            }
            let mut slots: Vec<Option<i64>> = vec![None; 3];
            for (index, argument) in args.into_iter().enumerate() {
                slots[index] = Some(argument);
            }
            let pairs = if kwargc == 0 {
                &[][..]
            } else {
                // SAFETY: the caller validated the buffer and its length.
                unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) }
            };
            let mut provided = slots
                .iter()
                .rposition(Option::is_some)
                .map_or(0, |index| index + 1);
            for pair in pairs.as_chunks::<2>().0 {
                let Some(keyword) = value::as_str(pair[0]) else {
                    return exc::raise_with_message(TYPE_ERROR, "keyword name must be str");
                };
                let index = match keyword {
                    "source" => 0,
                    "globals" => 1,
                    "locals" => 2,
                    _ => {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            &format!(
                                "{}() got an unexpected keyword argument '{keyword}'",
                                builtin.name()
                            ),
                        )
                    }
                };
                if slots[index].is_some() {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!(
                            "{}() got multiple values for argument '{keyword}'",
                            builtin.name()
                        ),
                    );
                }
                slots[index] = Some(pair[1]);
                provided = provided.max(index + 1);
            }
            let bound: Vec<i64> = slots
                .into_iter()
                .take(provided)
                .map(|slot| slot.unwrap_or(NONE))
                .collect();
            return call_builtin(*builtin, bound.len(), bound.as_ptr());
        }
        // `int(x, base=10)`: `base` is the only keyword CPython accepts.
        if matches!(builtin, Builtin::Int) {
            let pairs = if kwargc == 0 {
                &[][..]
            } else {
                if kw_argv.is_null() {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        "keyword arguments were passed without an argument buffer",
                    );
                }
                // SAFETY: the caller validated the buffer and its length.
                unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) }
            };
            let mut base: Option<i64> = None;
            for pair in pairs.as_chunks::<2>().0 {
                let Some(keyword) = value::as_str(pair[0]) else {
                    return exc::raise_with_message(TYPE_ERROR, "keyword name must be str");
                };
                if keyword != "base" {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!("int() got an unexpected keyword argument '{keyword}'"),
                    );
                }
                if base.is_some() {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        "int() got multiple values for argument 'base'",
                    );
                }
                base = Some(pair[1]);
            }
            if argc == 0 {
                return exc::raise_with_message(TYPE_ERROR, "int() missing string argument");
            }
            if argc > 2 {
                return builtin_arity_error("int", "at most 2", argc);
            }
            if argc == 2 && base.is_some() {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "int() got multiple values for argument 'base'",
                );
            }
            let mut bound = Vec::with_capacity(2);
            // SAFETY: nil with argc > 0 was rejected by the caller.
            bound.push(unsafe { *argv });
            if let Some(base) = base {
                bound.push(base);
            }
            return call_builtin(*builtin, bound.len(), bound.as_ptr());
        }
        // `max`/`min` bind `key=` and the keyword-only `default=` here; the
        // direct lowering path appends the key slot instead.
        if matches!(builtin, Builtin::Max | Builtin::Min) {
            return extremum_keywords(*builtin, argc, argv, kwargc, kw_argv);
        }
        // `str(object='', encoding=..., errors=...)`: every parameter is
        // accepted by name. An omitted object yields `""` even when
        // `encoding`/`errors` are given (matching CPython), but those
        // arguments are still parsed as `str`.
        if matches!(builtin, Builtin::Str) {
            const PARAMS: [&str; 3] = ["object", "encoding", "errors"];
            if argc > PARAMS.len() {
                return builtin_arity_error("str", "at most 3", argc);
            }
            let mut slots: [Option<i64>; 3] = [None; 3];
            if argc > 0 {
                // SAFETY: a NULL buffer with argc > 0 was rejected at entry.
                let positional = unsafe { std::slice::from_raw_parts(argv, argc) };
                for (index, argument) in positional.iter().enumerate() {
                    slots[index] = Some(*argument);
                }
            }
            if kwargc > 0 {
                if kw_argv.is_null() {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        "keyword arguments were passed without an argument buffer",
                    );
                }
                // SAFETY: `kw_argv` holds `2 * kwargc` readable values.
                let pairs = unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) };
                for pair in pairs.as_chunks::<2>().0 {
                    let Some(keyword) = value::as_str(pair[0]) else {
                        return exc::raise_with_message(TYPE_ERROR, "keyword name must be str");
                    };
                    let Some(index) = PARAMS.iter().position(|param| *param == keyword) else {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            &format!("str() got an unexpected keyword argument '{keyword}'"),
                        );
                    };
                    if slots[index].is_some() {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            &format!("str() got multiple values for argument '{keyword}'"),
                        );
                    }
                    slots[index] = Some(pair[1]);
                }
            }
            let Some(object) = slots[0] else {
                for (index, name) in [(1usize, "encoding"), (2, "errors")] {
                    let Some(argument) = slots[index] else {
                        continue;
                    };
                    if value::as_str(argument).is_none() {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            &format!(
                                "str() argument '{name}' must be str, not {}",
                                value::type_name(argument)
                            ),
                        );
                    }
                }
                return value::str_alloc(String::new());
            };
            if slots[1].is_none() && slots[2].is_none() {
                return call_builtin(Builtin::Str, 1, &object);
            }
            if slots[1].is_none() {
                // `errors=` without `encoding=` decodes as UTF-8.
                slots[1] = Some(value::str_alloc("utf-8".to_string()));
            }
            let bound: Vec<i64> = slots.into_iter().flatten().collect();
            let roots = crate::heap::root_values(&bound);
            let result = call_builtin(Builtin::Str, bound.len(), bound.as_ptr());
            crate::heap::release_roots(roots);
            return result;
        }
    }
    let signature = value::function_signature(function);
    let entry = value::function_full_entry(function);
    if signature.is_empty() {
        // Closures have no full signature metadata, but their user-visible
        // positional names are stored separately. Bind keyword arguments to
        // those positions and let the wrapper prepend captured values.
        let names = value::function_params(function);
        if !names.is_empty() {
            if argc > 0 && argv.is_null() {
                return exc::raise_with_message(TYPE_ERROR, "missing argument buffer");
            }
            let mut slots: Vec<Option<i64>> = vec![None; names.len()];
            let args = if argc == 0 {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec()
            };
            if args.len() > names.len() {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("{name}() takes at most {} arguments", names.len()),
                );
            }
            for (index, value) in args.into_iter().enumerate() {
                slots[index] = Some(value);
            }
            let pairs = if kwargc == 0 {
                &[][..]
            } else {
                unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) }
            };
            for pair in pairs.as_chunks::<2>().0 {
                let Some(keyword) = value::as_str(pair[0]) else {
                    return exc::raise_with_message(TYPE_ERROR, "keyword name must be str");
                };
                let Some(index) = names.iter().position(|candidate| candidate == keyword) else {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!("{name}() got an unexpected keyword argument '{keyword}'"),
                    );
                };
                if slots[index].is_some() {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!("{name}() got multiple values for argument '{keyword}'"),
                    );
                }
                slots[index] = Some(pair[1]);
            }
            let mut bound = Vec::with_capacity(names.len());
            for (index, slot) in slots.into_iter().enumerate() {
                match slot {
                    Some(value) => bound.push(value),
                    None => {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            &format!("{name}() missing required argument '{}'", names[index]),
                        )
                    }
                }
            }
            return call(function, bound.len(), bound.as_ptr());
        }
    }
    if signature.is_empty() || entry == 0 {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("{name}() does not accept keyword arguments"),
        );
    }
    let args = if argc == 0 {
        Vec::new()
    } else {
        // SAFETY: checked above; caller contract.
        unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec()
    };
    let pairs = if kwargc == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) }
    };
    let mut keywords: Vec<(String, i64)> = Vec::with_capacity(kwargc);
    for pair in pairs.as_chunks::<2>().0 {
        let Some(keyword) = value::as_str(pair[0]) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "keyword argument name must be str, not '{}'",
                    value::type_name(pair[0])
                ),
            );
        };
        keywords.push((keyword.to_string(), pair[1]));
    }

    let mut slots: Vec<Option<i64>> = vec![None; signature.len()];
    let mut next_positional = 0usize;
    let mut vararg_items: Vec<i64> = Vec::new();
    for (index, param) in signature.iter().enumerate() {
        match param.kind {
            // Positional-only parameters fill from the positional stream
            // exactly like ordinary positional parameters.
            ParamKind::Positional | ParamKind::PosOnly => {
                if next_positional < args.len() {
                    slots[index] = Some(args[next_positional]);
                    next_positional += 1;
                }
            }
            ParamKind::VarArg => {
                vararg_items.extend_from_slice(&args[next_positional..]);
                next_positional = args.len();
            }
            ParamKind::KwOnly | ParamKind::KwArg => {}
        }
    }
    if next_positional < args.len() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{name}() takes at most {} positional argument(s) ({} given)",
                signature
                    .iter()
                    .filter(|param| param.kind.is_positional())
                    .count(),
                args.len()
            ),
        );
    }

    let mut extra_keywords: Vec<(String, i64)> = Vec::new();
    for (keyword, argument) in keywords {
        let mut matched = false;
        for (index, param) in signature.iter().enumerate() {
            if param.kind != ParamKind::Positional && param.kind != ParamKind::KwOnly {
                continue;
            }
            if param.name == keyword {
                if slots[index].is_some() {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!("{name}() got multiple values for argument '{keyword}'"),
                    );
                }
                slots[index] = Some(argument);
                matched = true;
                break;
            }
        }
        if !matched {
            if signature.iter().any(|param| param.kind == ParamKind::KwArg) {
                extra_keywords.push((keyword, argument));
            } else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("{name}() got an unexpected keyword argument '{keyword}'"),
                );
            }
        }
    }

    if let Some(index) = signature
        .iter()
        .position(|param| param.kind == ParamKind::VarArg)
    {
        slots[index] = Some(value::list_new(vararg_items));
    }
    if let Some(index) = signature
        .iter()
        .position(|param| param.kind == ParamKind::KwArg)
    {
        let pairs = extra_keywords
            .iter()
            .map(|(keyword, value)| (value::str_alloc(keyword.clone()), *value))
            .collect();
        slots[index] = Some(value::dict_new(pairs));
    }
    for (index, param) in signature.iter().enumerate() {
        if slots[index].is_some() {
            continue;
        }
        match param.default {
            Some(default) => slots[index] = Some(default),
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("{name}() missing required argument '{}'", param.name),
                )
            }
        }
    }
    let full: Vec<i64> = slots.into_iter().map(|slot| slot.unwrap_or(NONE)).collect();
    if let FunctionBody::Host { token, captures } = &func.body {
        // The shared binder produced one value per user-visible parameter;
        // the interpreter copies captures and these slots into its frame.
        return crate::host::call_slots(*token, captures, &full, func.flags, &name);
    }
    if func.flags & (value::FUNCTION_FLAG_ASYNC | value::FUNCTION_FLAG_GENERATOR) != 0 {
        // Generator/coroutine calls must create a suspended fiber instead of
        // running the concrete entry. A plain positional signature goes
        // through the regular wrapper with the fully bound slot vector; a
        // keyword-bearing signature cannot, because the generated prologue
        // re-binds user positional arguments and rejects the keyword slots
        // (`function takes at most N positional arguments`). Such calls resume
        // through the uniform full-entry trampoline with the complete vector.
        if !signature.iter().all(|param| param.kind.is_positional()) && entry != 0 {
            let roots = crate::heap::root_values(&full);
            let created = if func.flags & value::FUNCTION_FLAG_ASYNC != 0 {
                if func.flags & value::FUNCTION_FLAG_GENERATOR != 0 {
                    value::fiber_async_generator_new(entry, name.clone(), full)
                } else {
                    value::fiber_coroutine_new(entry, name.clone(), full)
                }
            } else {
                value::fiber_generator_new(entry, name.clone(), full)
            };
            crate::heap::release_roots(roots);
            return match created {
                Some(fiber) => fiber,
                None => exc::raise_with_message(
                    RUNTIME_ERROR,
                    "cannot allocate a generator/coroutine stack",
                ),
            };
        }
        return call(function, full.len(), full.as_ptr());
    }
    call_full_entry(entry, &full)
}

/// Calls a generated full-argv trampoline while keeping every argument value
/// visible to the collector. Small vectors are copied to a stack array;
/// unusually large signatures root the array for the duration of the call.
pub(crate) fn call_full_entry(entry: usize, args: &[i64]) -> i64 {
    // SAFETY: `entry` is the generated trampoline for this exact argument count.
    let trampoline: extern "C" fn(*const i64) -> i64 =
        unsafe { std::mem::transmute::<usize, extern "C" fn(*const i64) -> i64>(entry) };
    const STACK_ARGS: usize = 32;
    if args.len() <= STACK_ARGS {
        let mut buffer = [NONE; STACK_ARGS];
        buffer[..args.len()].copy_from_slice(args);
        return trampoline(buffer.as_ptr());
    }
    let roots = crate::heap::root_values(args);
    let result = trampoline(args.as_ptr());
    crate::heap::release_roots(roots);
    result
}

/// Resolves a name bound by an import that was deferred at compile time.
///
/// The loader links every module that is part of the program, so a deferred
/// import is served from the runtime module registry (`sys.modules`), never by
/// compiling at runtime:
///
/// - a module alias returns the module value;
/// - `from pkg import sub` returns the registered submodule when it was
///   preloaded, otherwise an attribute of `pkg` (a function, class or
///   constant), and finally the module registry again for dotted attribute
///   chains (`from pkg import sub.value`);
/// - a compiled extension named by `path` materialises its module value, and
///   `from ext import member` reads the extension attribute lazily;
/// - a module that is not registered raises `ModuleNotFoundError` carrying the
///   exact name, while a member an existing module does not provide raises
///   `ImportError` (`cannot import name '<member>' from '<module>'`), so
///   `except ImportError` handlers run with CPython's error taxonomy. A
///   parent that never published `__name__` is still initializing, so the
///   missing name is reported as the dotted submodule it names (NumPy probes
///   `numpy._core` this way).
///
/// The returned `None` is only the pending-exception placeholder for
/// generated code.
pub(crate) fn deferred_import(module: &str, member: Option<&str>, path: Option<&str>) -> i64 {
    if let Some(path) = path {
        // A compiled extension: the module value is materialised lazily (and
        // cached under its import name), so an attribute read loads the shared
        // object through the extension ABI exactly like `ext.member`.
        return match member {
            Some(member) => modules::extension::import_member(module, path, member),
            None => {
                if crate::native::is_hna_path(path) {
                    crate::native::extension_module_value(module, path)
                } else {
                    modules::extension_module_value(module, path)
                }
            }
        };
    }
    let Some(member) = member else {
        return match modules::registry_lookup(module) {
            Some(value) => value,
            None => module_not_found(module),
        };
    };
    // Walk the member chain one component at a time: each leading component
    // that names a registered module becomes the new receiver, so
    // `from pkg import sub` and `from pkg import sub.value` share one path.
    let mut current = module.to_string();
    let mut remaining = member;
    loop {
        let (head, tail) = match remaining.split_once('.') {
            Some((head, tail)) => (head, Some(tail)),
            None => (remaining, None),
        };
        let full = format!("{current}.{head}");
        if let Some(value) = modules::registry_lookup(&full) {
            match tail {
                Some(tail) => {
                    current = full;
                    remaining = tail;
                    continue;
                }
                None => return value,
            }
        }
        if let Some(value) = globals::try_get(&current, head) {
            return match tail {
                Some(tail) => attribute_chain(value, tail),
                None => value,
            };
        }
        // The base module exists: a name it does not provide is a failed
        // `from ... import`, not a missing module. Native modules resolve
        // their members on demand from the central spec, exactly like
        // `getattr(module, name)`.
        if modules::registry_lookup(&current).is_some() || stdlib::module(&current).is_some() {
            let value = modules::module_get_by_name(&current, head);
            if exc::pending() {
                // `module_get_by_name` reports the missing attribute as
                // `AttributeError`; a failed from-import is an `ImportError`.
                if exc::pending_tag() == i64::from(ATTRIBUTE_ERROR) {
                    exc::clear();
                    // A module that never published `__name__` is not fully
                    // initialized: the name was really a submodule import
                    // (NumPy probes `numpy._core` before its package is
                    // loaded), so report the dotted module that is missing.
                    if !is_initialized_module(&current) {
                        return module_not_found(&full);
                    }
                    return cannot_import_name(head, &current);
                }
                return NONE;
            }
            return match tail {
                Some(tail) => attribute_chain(value, tail),
                None => value,
            };
        }
        return module_not_found(&current);
    }
}

/// Whether `module` is an initialized module.
///
/// A native module has a central spec. A compiled unit publishes its
/// `__name__` into the runtime global table when it initializes, so a module
/// that only has incidental globals is still initializing (or never loaded)
/// and its namespace is not authoritative for import resolution.
fn is_initialized_module(module: &str) -> bool {
    stdlib::module(module).is_some() || globals::try_get(module, "__name__").is_some()
}

/// Raises `ImportError: cannot import name '<member>' from '<module>'`.
///
/// This is the error CPython reports for `from <module> import <member>` when
/// the module exists but does not provide the name, so a `try`/`except
/// ImportError` fallback runs. The `name` keyword stores the module name, like
/// the interpreter's own from-import failure.
pub(crate) fn cannot_import_name(member: &str, module: &str) -> i64 {
    let message = format!("cannot import name '{member}' from '{module}'");
    let missing = value::str_alloc(module.to_string());
    let roots = crate::heap::root_values(&[missing]);
    let exception = exc::exception_value_with_keywords(
        IMPORT_ERROR,
        &message,
        vec![message.clone()],
        vec![("name".to_string(), missing)],
    );
    crate::heap::release_roots(roots);
    exc::raise_value(exception)
}

/// Follows a dotted attribute path from `value`, propagating the pending
/// `AttributeError` of a missing member.
fn attribute_chain(value: i64, path: &str) -> i64 {
    let mut value = value;
    for name in path.split('.') {
        value = methods::getattr(value, name.as_ptr(), name.len());
        if exc::pending() {
            return NONE;
        }
    }
    value
}

/// Raises `ModuleNotFoundError` naming `name` exactly, like CPython's importer.
///
/// The `name` keyword is stored on the exception object so runtime consumers
/// can inspect the missing module; the message keeps the standard
/// `No module named '<name>'` spelling.
pub(crate) fn module_not_found(name: &str) -> i64 {
    let message = format!("No module named '{name}'");
    let missing = value::str_alloc(name.to_string());
    let roots = crate::heap::root_values(&[missing]);
    let exception = exc::exception_value_with_keywords(
        exc::MODULE_NOT_FOUND_ERROR,
        &message,
        vec![message.clone()],
        vec![("name".to_string(), missing)],
    );
    crate::heap::release_roots(roots);
    exc::raise_value(exception)
}

/// Applies an `operator.attrgetter` value to its single argument.
///
/// One stored name yields the attribute value; several yield a tuple in
/// declaration order. A missing attribute leaves the `AttributeError` raised by
/// [`methods::getattr`] pending.
fn attribute_getter_call(names: &[String], argc: usize, argv: *const i64) -> i64 {
    if argc != 1 || argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("attrgetter() takes exactly one argument ({argc} given)"),
        );
    }
    // SAFETY: exactly one argument was checked above.
    let object = unsafe { *argv };
    if names.len() == 1 {
        let name = &names[0];
        return methods::getattr(object, name.as_ptr(), name.len());
    }
    // Root the results in a list while later attribute reads allocate, then
    // copy the snapshot into the tuple CPython returns.
    let collected = value::list_new(Vec::new());
    for name in names {
        let item = methods::getattr(object, name.as_ptr(), name.len());
        if exc::pending() {
            return NONE;
        }
        push_item(collected, item);
    }
    let items = value::as_list(collected)
        .map(|items| items.to_vec())
        .unwrap_or_default();
    value::tuple_new(items)
}

/// Applies an `operator.itemgetter` value to its single argument.
///
/// One stored key yields the subscripted value; several yield a tuple. Keys are
/// traced by the function body, so they stay alive across the call.
fn item_getter_call(keys: &[i64], argc: usize, argv: *const i64) -> i64 {
    if argc != 1 || argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("itemgetter() takes exactly one argument ({argc} given)"),
        );
    }
    // SAFETY: exactly one argument was checked above.
    let object = unsafe { *argv };
    if keys.len() == 1 {
        return container::subscript(object, keys[0]);
    }
    let collected = value::list_new(Vec::new());
    for &key in keys {
        let item = container::subscript(object, key);
        if exc::pending() {
            return NONE;
        }
        push_item(collected, item);
    }
    let items = value::as_list(collected)
        .map(|items| items.to_vec())
        .unwrap_or_default();
    value::tuple_new(items)
}

/// Appends `item` to the list rooted at `list`, recording the GC edge.
fn push_item(list: i64, item: i64) {
    if let Some(mut items) = value::list_items_mut(list) {
        items.push(item);
        crate::heap::write_barrier_edge(list, item);
    }
}

/// Instantiates a class: allocates the instance and runs `__init__` from the
/// class chain with the instance bound as `self`.
///
/// `__init__`'s return value is ignored (Python requires `None`). Without
/// `__init__` any positional argument is a `TypeError`; otherwise a raised
/// exception propagates and no instance is returned.
/// Instantiates a built-in exception class: `ValueError('x')` creates a
/// plain exception object with the tag's class name, message and `args`
/// tuple. Extra positional arguments are rejected (Hydro stores one message,
/// not arbitrary argument objects).
fn instantiate_exception(tag: u32, argc: usize, argv: *const i64) -> i64 {
    let args: &[i64] = if argc > 0 && !argv.is_null() {
        if argc > 4 && exc::is_os_error_tag(tag) {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "{}() takes at most 4 arguments ({} given)",
                    exc::tag_name(tag),
                    argc
                ),
            );
        }
        if argc > 1 && !exc::is_os_error_tag(tag) {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "{}() takes at most 1 argument ({} given)",
                    exc::tag_name(tag),
                    argc
                ),
            );
        }
        // SAFETY: ABI contract: `argv` points at `argc` readable tagged values.
        unsafe { std::slice::from_raw_parts(argv, argc) }
    } else {
        &[]
    };
    instantiate_exception_kw(tag, args, &[])
}

/// Instantiates a built-in exception class with keyword arguments:
/// `AttributeError(msg, name=None)`, `NameError(msg, name=...)`, and the
/// `SyntaxError` family's source-position keywords.
///
/// The keyword set accepted by each class comes from
/// [`exc::exception_keyword_names`]. An unsupported name raises `TypeError`
/// naming the class and keyword instead of being silently dropped, matching
/// CPython's constructor.
fn instantiate_exception_kw(tag: u32, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if args.len() > 1 {
        if exc::is_os_error_tag(tag) && args.len() <= 4 {
            return os_error_value(tag, args, keywords);
        }
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{}() takes at most 1 argument ({} given)",
                exc::tag_name(tag),
                args.len()
            ),
        );
    }
    let allowed = exc::exception_keyword_names(tag);
    for (name, _) in keywords {
        if !allowed.contains(&name.as_str()) {
            let message = if allowed.is_empty() {
                format!("{}() takes no keyword arguments", exc::tag_name(tag))
            } else {
                format!(
                    "{}() got an unexpected keyword argument '{}'",
                    exc::tag_name(tag),
                    name
                )
            };
            return exc::raise_with_message(TYPE_ERROR, &message);
        }
    }
    let (message, rendered_args) = match args.first() {
        Some(&argument) => {
            let text = value::to_string(argument);
            // `KeyError` renders its argument with `repr()` in
            // `str(exception)` while `args` keeps the argument itself (as
            // text).
            let message = if tag == exc::KEY_ERROR {
                value::repr(argument)
            } else {
                text.clone()
            };
            (message, vec![text])
        }
        None => (String::new(), Vec::new()),
    };
    exc::exception_value_with_keywords(tag, &message, rendered_args, keywords.to_vec())
}

/// Builds `OSError(errno, strerror[, filename])` with CPython's rendered
/// message (`[Errno 2] No such file: 'x'`). The raw arguments remain in
/// `args`; the message is what `str(exception)` and tracebacks show.
fn os_error_value(tag: u32, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let rendered: Vec<String> = args
        .iter()
        .map(|argument| value::to_string(*argument))
        .collect();
    let message = match (value::as_integer(args[0]), value::as_str(args[1])) {
        (Some(errno), Some(text)) => {
            let mut message = format!("[Errno {errno}] {text}");
            if let Some(filename) = args.get(2).copied().filter(|value| *value != NONE) {
                message.push_str(": ");
                message.push_str(&value::repr(filename));
            }
            message
        }
        _ => rendered.first().cloned().unwrap_or_default(),
    };
    exc::exception_value_with_keywords(tag, &message, rendered, keywords.to_vec())
}

/// Builds the native payload for a user-defined exception class.
///
/// Unlike the built-in exception constructors this accepts any argument count
/// (matching a Python class deriving from `BaseException`): every rendered
/// argument becomes an `args` element and the first one is the message.
fn derived_exception_payload(tag: u32, argc: usize, argv: *const i64) -> i64 {
    if argc == 0 || argv.is_null() {
        return exc::exception_value(tag, "", Vec::new());
    }
    if argc > isize::MAX as usize / std::mem::size_of::<i64>() {
        return exc::raise_with_message(
            TYPE_ERROR,
            "exception instantiation has too many arguments",
        );
    }
    // SAFETY: ABI contract: `argv` points at `argc` readable values.
    let values = unsafe { std::slice::from_raw_parts(argv, argc) };
    let rendered: Vec<String> = values
        .iter()
        .map(|argument| value::to_string(*argument))
        .collect();
    let message = rendered.first().cloned().unwrap_or_default();
    exc::exception_value(tag, &message, rendered)
}

/// Constructs a TypedDict class call. TypedDict classes are annotation-only
/// classes, but CPython's runtime constructor is `dict`: positional mappings
/// are copied and keyword arguments become entries.
fn typed_dict_construct(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if args.len() > 1 {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "TypedDict() takes at most 1 positional argument ({} given)",
                args.len()
            ),
        );
    }
    let result = if let Some(&mapping) = args.first() {
        if mapping == NONE {
            value::dict_new(Vec::new())
        } else {
            dict_from(mapping)
        }
    } else {
        value::dict_new(Vec::new())
    };
    if exc::pending() {
        return NONE;
    }
    let roots = crate::heap::root_values(&[result]);
    for (name, value) in keywords {
        let key = value::str_alloc(name.clone());
        crate::container::dict_set(result, key, *value);
    }
    crate::heap::release_roots(roots);
    result
}

/// Native constructor that supplies a scalar subclass instance's payload.
fn scalar_constructor(kind: &str) -> Option<Builtin> {
    Some(match kind {
        "bool" => Builtin::Bool,
        "int" => Builtin::Int,
        "float" => Builtin::Float,
        "str" => Builtin::Str,
        "bytes" => Builtin::Bytes,
        _ => return None,
    })
}

/// Allocates `class(args...)` when `class` derives from a native scalar type.
///
/// Returns `None` when `class` is not a scalar subclass. The exact native
/// class (`type(1)(3)`, `type('')(b'x')`) returns the plain scalar value;
/// a user subclass gets an instance carrying the scalar payload in a reserved
/// attribute, which [`crate::value::scalar_payload`] and every scalar view
/// unwrap.
pub(crate) fn scalar_instance_new(class: i64, argc: usize, argv: *const i64) -> Option<i64> {
    let (kind, native) = value::native_scalar_base(class)?;
    let builtin = scalar_constructor(kind)?;
    // `float()` accepts no argument (`float.__new__(cls)` is `0.0`), while
    // `Builtin::Float` itself takes exactly one.
    let payload = if kind == "float" && argc == 0 {
        value::float_new(0.0)
    } else {
        call_builtin(builtin, argc, argv)
    };
    if exc::pending() {
        return Some(NONE);
    }
    if class == native {
        return Some(payload);
    }
    let roots = crate::heap::root_values(std::slice::from_ref(&payload));
    let instance = value::instance_new(class);
    value::instance_set_scalar_payload(instance, payload);
    crate::heap::release_roots(roots);
    Some(instance)
}

/// `int.__new__(cls, ...)` and the native `__scalar_new__` slot.
///
/// `argv[0]` is the class; the remaining arguments are the constructor's.
/// The bound form (`int.__new__`) reaches this through [`crate::methods::call`].
pub(crate) fn scalar_new_from_args(argc: usize, argv: *const i64) -> i64 {
    if argc == 0 || argv.is_null() {
        return exc::raise_with_message(TYPE_ERROR, "scalar __new__() missing the class argument");
    }
    // SAFETY: checked above for NULL; caller contract.
    let args = unsafe { std::slice::from_raw_parts(argv, argc) };
    let class = args[0];
    if value::as_class(class).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "scalar __new__() argument 1 must be a type, not '{}'",
                value::type_name(class)
            ),
        );
    }
    match scalar_instance_new(class, argc - 1, args[1..].as_ptr()) {
        Some(result) => result,
        None => value::instance_new(class),
    }
}

/// Runs a compiled `__new__` override for a class call.
///
/// CPython's `type.__call__` invokes `cls.__new__(cls, *args, **kwargs)`
/// before `__init__`. A class that overrides `__new__` (the pure-Python
/// `_pydatetime` classes, for example) allocates and initialises its own
/// instance, so a missing `__init__` is not an arity error. Native hooks
/// (`object.__new__`, the scalar `__scalar_new__`) carry no compiled signature
/// and return `None` so the default allocation path runs. When `__new__`
/// returns an instance of `class`, its `__init__` runs with the same
/// arguments; any other result is returned unchanged, as in CPython.
fn call_user_new(class: i64, args: &[i64], keywords: &[(String, i64)]) -> Option<i64> {
    let (_kind, new) = value::class_member(class, "__new__")?;
    if value::function_signature(new).is_empty() || value::function_full_entry(new) == 0 {
        return None;
    }
    // `__new__` is implicitly a static method: the class is the first
    // argument even when the body does not decorate it.
    let result = methods::call_user_method(
        class,
        class,
        MethodKind::Normal,
        new,
        "__new__",
        args,
        keywords,
    );
    if exc::pending() {
        return Some(NONE);
    }
    if instance_of(result, class) {
        if let Some((kind, init)) = value::class_member(class, "__init__") {
            if !value::function_signature(init).is_empty() && value::function_full_entry(init) != 0
            {
                let _ = methods::call_user_method(
                    result, class, kind, init, "__init__", args, keywords,
                );
                if exc::pending() {
                    return Some(NONE);
                }
            }
        }
    }
    Some(result)
}

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
    if crate::modules::typing::is_typeddict_class(class) {
        let args: &[i64] = if argc == 0 {
            &[]
        } else {
            // SAFETY: checked above for NULL; caller contract.
            unsafe { std::slice::from_raw_parts(argv, argc) }
        };
        return typed_dict_construct(args, &[]);
    }
    // The universal base class has CPython's zero-argument constructor.
    if value::is_object_class(class) {
        if argc == 0 {
            return value::instance_new(class);
        }
        return exc::raise_with_message(TYPE_ERROR, "object() takes no arguments");
    }
    // `types.CellType` is a plain native class with one optional initial
    // value. Its cached class value is callable through the normal class
    // path, so route it to the cell constructor before generic instantiation
    // rejects the argument.
    if globals::registry_get("types.CellType") == Some(class) {
        let args: &[i64] = if argc == 0 {
            &[]
        } else {
            // SAFETY: argc > 0 implies a non-null, readable buffer; the check
            // above validated the count.
            unsafe { std::slice::from_raw_parts(argv, argc) }
        };
        return modules::types::call("CellType", args);
    }
    // `complex` and `slice` are native instances of cached classes; calling
    // the class value must run their constructors instead of allocating a
    // blank instance. The name check keeps the registry lookup off the hot
    // path for ordinary user classes.
    let native_name = value::class_display_name(class);
    // `functools.partial`'s module value is a class so type checks and unions
    // work; calling it constructs the real partial function object.
    if value::is_partial_class(class) {
        let args: &[i64] = if argc == 0 {
            &[]
        } else {
            // SAFETY: argc > 0 implies a non-null, readable buffer.
            unsafe { std::slice::from_raw_parts(argv, argc) }
        };
        let Some((&target, bound)) = args.split_first() else {
            return exc::raise_with_message(
                TYPE_ERROR,
                "type 'functools.partial' takes at least one argument",
            );
        };
        return value::partial_new(target, bound.to_vec());
    }
    if native_name == Some("complex") && globals::registry_get("builtins.complex") == Some(class) {
        let args: &[i64] = if argc == 0 {
            &[]
        } else {
            // SAFETY: argc > 0 implies a non-null, readable buffer.
            unsafe { std::slice::from_raw_parts(argv, argc) }
        };
        return complex_new(args);
    }
    if native_name == Some("slice") && globals::registry_get("builtins.slice") == Some(class) {
        let args: &[i64] = if argc == 0 {
            &[]
        } else {
            // SAFETY: argc > 0 implies a non-null, readable buffer.
            unsafe { std::slice::from_raw_parts(argv, argc) }
        };
        return slice_new(args);
    }
    // `builtins.frozendict` is a cached native class whose constructor builds
    // an immutable mapping instance instead of allocating a blank instance.
    if native_name == Some("frozendict")
        && globals::registry_get("builtins.frozendict") == Some(class)
    {
        let args: &[i64] = if argc == 0 {
            &[]
        } else {
            // SAFETY: argc > 0 implies a non-null, readable buffer.
            unsafe { std::slice::from_raw_parts(argv, argc) }
        };
        return modules::builtins::frozendict_instantiate(args, &[]);
    }
    // The `collections.defaultdict` module value is a class for `pprint`'s
    // dispatch keys; calling it runs the real constructor.
    if native_name == Some("defaultdict")
        && globals::registry_get("collections.defaultdict.class") == Some(class)
    {
        let args: &[i64] = if argc == 0 {
            &[]
        } else {
            // SAFETY: argc > 0 implies a non-null, readable buffer.
            unsafe { std::slice::from_raw_parts(argv, argc) }
        };
        return modules::collections::call("defaultdict", args);
    }
    // `codecs.CodecInfo(...)` builds the native codec-registry entry.
    if native_name == Some("CodecInfo")
        && globals::registry_get("codecs.CodecInfo.class") == Some(class)
    {
        let args: &[i64] = if argc == 0 {
            &[]
        } else {
            // SAFETY: argc > 0 implies a non-null, readable buffer.
            unsafe { std::slice::from_raw_parts(argv, argc) }
        };
        return modules::codecs_codecinfo_new(args, &[]);
    }
    // The `pathlib` pure-path classes construct the same native `Path` value
    // as `pathlib.Path`; the helper validates the identity and the
    // platform-specific `PosixPath`/`WindowsPath` rules.
    if matches!(
        native_name,
        Some("PurePath" | "PosixPath" | "WindowsPath" | "PurePosixPath" | "PureWindowsPath")
    ) {
        let args: &[i64] = if argc == 0 {
            &[]
        } else {
            // SAFETY: argc > 0 implies a non-null, readable buffer.
            unsafe { std::slice::from_raw_parts(argv, argc) }
        };
        if let Some(result) = modules::pathlib_instantiate(class, args, &[]) {
            return result;
        }
    }
    // `socket.SocketType`/`socket.socket` is the cached class `type()` reports
    // for sockets; calling it runs the socket factory instead of allocating a
    // blank instance.
    if native_name == Some("socket") && value::socket_class() == class {
        let args: &[i64] = if argc == 0 {
            &[]
        } else {
            // SAFETY: argc > 0 implies a non-null, readable buffer.
            unsafe { std::slice::from_raw_parts(argv, argc) }
        };
        return crate::modules::socket::call("socket", "socket", args);
    }
    // The `datetime` value classes are cached native classes whose instances
    // carry the date/time fields as attributes. Calling the class value runs
    // the CPython constructor (range validation included) instead of
    // allocating a blank instance. The name check keeps the registry lookups
    // off the hot path for ordinary user classes.
    if matches!(
        native_name,
        Some("date" | "time" | "datetime" | "timedelta")
    ) {
        let args: &[i64] = if argc == 0 {
            &[]
        } else {
            // SAFETY: argc > 0 implies a non-null, readable buffer.
            unsafe { std::slice::from_raw_parts(argv, argc) }
        };
        if let Some(result) = crate::modules::datetime::instantiate(class, args) {
            return result;
        }
    }
    // CPython's `re.Pattern`/`re.Match` are abstract: they exist for
    // `isinstance` and class identity, and cannot be instantiated.
    if (native_name == Some("Pattern") && value::pattern_class() == class)
        || (native_name == Some("Match") && value::match_class() == class)
    {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "cannot create 're.{}' instances",
                native_name.unwrap_or("Pattern")
            ),
        );
    }
    if let Some(tag) = exc::exception_tag_of_class(class) {
        return instantiate_exception(tag, argc, argv);
    }
    // A user class derived from a native exception class instantiates as a
    // real exception payload that still remembers its Python class, so
    // `raise MyError('x')`, `type(e) is MyError`, `isinstance(e, MyError)` and
    // the native `except ValueError:` ancestry all hold.
    if let Some(tag) = value::exception_tag_of_derived_class(class) {
        // User exception classes accept any number of constructor arguments;
        // the native payload records the rendered `args` and the first one as
        // the message, while a user `__init__` (below) sees the values.
        let plain = derived_exception_payload(tag, argc, argv);
        if exc::pending() {
            return NONE;
        }
        let Some(data) = value::exception_clone(plain) else {
            return plain;
        };
        let instance = value::exc_instance_new(class, data);
        // A user `__init__` runs on the exception instance with `self` bound,
        // so `class E(Exception): def __init__(self, extra): self.extra = extra`
        // keeps working alongside the exception payload.
        if let Some((kind, init)) = value::class_member(class, "__init__") {
            if !value::function_signature(init).is_empty() && value::function_full_entry(init) != 0
            {
                let args = if argc == 0 {
                    Vec::new()
                } else {
                    // SAFETY: checked above; caller contract.
                    unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec()
                };
                let _ =
                    methods::call_user_method(instance, class, kind, init, "__init__", &args, &[]);
                return if exc::pending() { NONE } else { instance };
            }
        }
        return instance;
    }
    // Copy the class name before allocating: a reference into a heap object
    // must not be read after an allocation that may collect.
    let name = value::class_display_name(class)
        .unwrap_or("type")
        .to_string();
    // `type.__call__` runs a compiled `__new__` before the default allocation
    // and `__init__`; `_pydatetime`'s classes build themselves there.
    let call_args: &[i64] = if argc == 0 {
        &[]
    } else {
        // SAFETY: checked above for NULL; caller contract.
        unsafe { std::slice::from_raw_parts(argv, argc) }
    };
    if let Some(result) = call_user_new(class, call_args, &[]) {
        return result;
    }
    // A subclass of a native scalar allocates the wrapped scalar as its
    // payload; the exact native class returns the plain builtin value.
    let instance = match scalar_instance_new(class, argc, argv) {
        Some(result) => {
            if exc::pending() {
                return NONE;
            }
            if value::as_instance(result).is_none() {
                return result;
            }
            result
        }
        None => value::instance_new(class),
    };
    let Some((kind, init)) = value::class_member(class, "__init__") else {
        if argc > 0 {
            return exc::raise_with_message(TYPE_ERROR, &format!("{name}() takes no arguments"));
        }
        return instance;
    };
    // `object.__init__` ignores extra arguments when the scalar payload has
    // already consumed them, matching CPython's behaviour for builtin
    // subclasses (`class N(int): pass; N(3)`).
    if value::function_native_name(init) == Some("__object_init__")
        && value::scalar_payload(instance).is_some()
    {
        return instance;
    }
    if !value::function_signature(init).is_empty() && value::function_full_entry(init) != 0 {
        let call_args = if argc == 0 {
            Vec::new()
        } else {
            // SAFETY: checked above; caller contract.
            unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec()
        };
        let _ = methods::call_user_method(instance, class, kind, init, "__init__", &call_args, &[]);
        return if exc::pending() { NONE } else { instance };
    }
    let mut args = Vec::with_capacity(argc + 1);
    match kind {
        MethodKind::Normal | MethodKind::Property => args.push(instance),
        MethodKind::Classmethod => args.push(class),
        MethodKind::Staticmethod => {}
    }
    if argc > 0 {
        // SAFETY: ABI contract: `argv` points at `argc` readable values; non-NULL since `argc > 0`.
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

/// `type(name, bases, namespace)`: dynamic class creation.
///
/// The bases are validated and linearized with C3, `__slots__` is honored, and
/// the metaclass resolved from the bases runs its `__new__`/`__init__` hooks
/// (see [`value::class_new_multi`]). Functions in the namespace become normal
/// methods; every other value is a class attribute.
fn type_three_arg(name: i64, bases: i64, namespace: i64) -> i64 {
    type_new_impl(NONE, name, bases, namespace, true, &[])
}

/// `type(name, bases, namespace, **kwds)`: CPython forwards the class-header
/// keywords to the metaclass selected from `bases`, so
/// `EnumType.__new__(..., boundary=..., _simple=...)` sees them.
fn type_three_arg_kw(name: i64, bases: i64, namespace: i64, keywords: &[(String, i64)]) -> i64 {
    type_new_impl(NONE, name, bases, namespace, true, keywords)
}

/// Keyword form of the three-argument `type(...)` call (`hydro_call_kw` on the
/// builtin value). CPython's `type(x, **kwds)` has no keyword parameters.
fn type_build_kw(argc: usize, argv: *const i64, keywords: &[(String, i64)]) -> i64 {
    let args: &[i64] = if argc == 0 {
        &[]
    } else {
        // SAFETY: ABI contract: `argv` points at `argc` readable values.
        unsafe { std::slice::from_raw_parts(argv, argc) }
    };
    type_build_keywords(args, keywords)
}

/// Keyword form of `type(...)` over a decoded argument slice, shared by the
/// builtin value path and the `builtins` module dispatcher.
pub(crate) fn type_build_keywords(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if args.len() == 1 {
        return exc::raise_with_message(TYPE_ERROR, "type() takes no keyword arguments");
    }
    if args.len() != 3 {
        return builtin_arity_error("type", "1 or 3", args.len());
    }
    type_three_arg_kw(args[0], args[1], args[2], keywords)
}

/// `type.__new__(metaclass, name, bases, namespace)`: the underlying class
/// allocator with an explicit metaclass, the C-level operation metaclasses
/// call from their `__new__`.
///
/// Unlike `type(name, bases, namespace)`, this does not run the metaclass
/// `__new__`/`__init__` hooks: the caller is that hook, so re-entering it
/// would recurse. The metaclass must be a class (or the `type` builtin, which
/// denotes the native `type` class).
pub(crate) fn type_new(metaclass: i64, name: i64, bases: i64, namespace: i64) -> i64 {
    type_new_impl(metaclass, name, bases, namespace, false, &[])
}

fn type_new_impl(
    metaclass: i64,
    name: i64,
    bases: i64,
    namespace: i64,
    run_hooks: bool,
    keywords: &[(String, i64)],
) -> i64 {
    // `type.__new__` names itself in argument diagnostics; the builtin
    // three-argument constructor keeps `type()`.
    let context = if run_hooks {
        "type()"
    } else {
        "type.__new__()"
    };
    let Some(name_text) = value::as_str(name) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{context} argument 1 must be str, not {}",
                value::type_name(name)
            ),
        );
    };
    let name_text = name_text.to_string();
    let metaclass = value::normalize_class_base(metaclass);
    if metaclass != NONE {
        if value::as_class(metaclass).is_none() {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "type.__new__(X): X is not a type object ({})",
                    value::type_name(metaclass)
                ),
            );
        }
        // `type.__new__` requires the metatype to derive from `type`;
        // `str`, `dict` and other builtin classes are types, but not
        // metaclasses.
        if !value::is_subclass_of(metaclass, value::native_type_class()) {
            let metaclass_name = value::class_display_name(metaclass).unwrap_or("type");
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "type.__new__({metaclass_name}): {metaclass_name} is not a subtype of type"
                ),
            );
        }
    }
    let Some(base_values) = value::as_tuple(bases) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{context} argument 2 must be tuple, not {}",
                value::type_name(bases)
            ),
        );
    };
    let base_values: Vec<i64> = base_values
        .iter()
        .copied()
        .map(value::normalize_class_base)
        .collect();
    for &base in &base_values {
        if value::as_class(base).is_none() && base != NONE {
            // CPython folds a non-class base into the metaclass resolution:
            // no metaclass can be derived from it.
            return exc::raise_with_message(
                TYPE_ERROR,
                "metaclass conflict: the metaclass of a derived class must be a \
                 (non-strict) subclass of the metaclasses of all its bases",
            );
        }
    }
    // `None` in the bases list is a no-op, as in class bodies.
    let base_values: Vec<i64> = base_values
        .into_iter()
        .filter(|&base| base != NONE)
        .collect();
    let Some(entries) = value::as_dict(namespace) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{context} argument 3 must be dict, not {}",
                value::type_name(namespace)
            ),
        );
    };
    let entries = entries.to_vec();
    let mut methods: Vec<(String, i64, MethodKind)> = Vec::with_capacity(entries.len());
    let mut attrs: Vec<(String, i64)> = Vec::with_capacity(entries.len());
    for (key, item) in entries {
        let Some(attr) = value::as_str(key) else {
            return exc::raise_with_message(TYPE_ERROR, "type() namespace keys must be strings");
        };
        if value::as_function(item).is_some() {
            methods.push((attr.to_string(), item, MethodKind::Normal));
        } else {
            attrs.push((attr.to_string(), item));
        }
    }
    if run_hooks {
        value::class_new_multi_kw(name_text, base_values, metaclass, methods, attrs, keywords)
    } else {
        value::class_new_multi_without_hooks(name_text, base_values, metaclass, methods, attrs)
    }
}

/// `globals()`: the live namespace mapping of the module codegen named.
///
/// The frontend appends the owning module's dotted name as a hidden trailing
/// argument, so a direct `globals()` never guesses a frame. Calls through a
/// first-class `globals` value carry no module and fail loudly instead.
fn globals_builtin(module_arg: i64) -> i64 {
    let Some(module) = value::as_str(module_arg) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "globals() requires the owning module name supplied by codegen",
        );
    };
    globals::view(module)
}

/// `vars(object)`: the object's namespace mapping.
fn vars_value(object: i64) -> i64 {
    if let Some(module) = value::as_module(object) {
        let name = module.name().to_string();
        return modules::module_dict(&name);
    }
    if value::as_instance(object).is_some() {
        return match value::instance_dict(object) {
            Some(mapping) => mapping,
            None => exc::raise_with_message(TYPE_ERROR, "instance has no __dict__"),
        };
    }
    if value::as_class(object).is_some() {
        // Hydro class namespaces are immutable after creation, so `vars` is a
        // snapshot, exactly like `Cls.__dict__`.
        let pairs = value::class_namespace(object);
        let namespace = value::dict_new(Vec::new());
        let roots = crate::heap::root_values(&[namespace]);
        for (name, item) in pairs {
            insert_namespace_pair(namespace, &name, item);
        }
        crate::heap::release_roots(roots);
        return namespace;
    }
    if value::function_has_attributes(object) {
        // `vars(f)` is the function's live `__dict__`, like CPython.
        return value::function_dict(object).unwrap_or_else(|| value::dict_new(Vec::new()));
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "vars() argument must have __dict__ attribute, not '{}'",
            value::type_name(object)
        ),
    )
}

/// Inserts one string-keyed pair into `namespace`, rooting the fresh key until
/// the dict owns it.
fn insert_namespace_pair(namespace: i64, name: &str, item: i64) {
    let key = value::str_alloc(name.to_string());
    let roots = crate::heap::root_values(&[key, item]);
    container::dict_set(namespace, key, item);
    crate::heap::release_roots(roots);
}

/// `dir(object)`: sorted attribute names of a module, class or instance.
fn dir_value(object: i64) -> i64 {
    let mut names: Vec<String> = Vec::new();
    if let Some(state) = value::as_instance(object) {
        names.push("__class__".to_string());
        names.push("__dict__".to_string());
        {
            let attrs = state.lock_attrs();
            names.extend(attrs.iter().map(|(name, _)| name.clone()));
        }
        collect_class_names_dir(state.class, &mut names);
    } else if value::as_class(object).is_some() {
        names.push("__name__".to_string());
        names.push("__class__".to_string());
        names.push("__dict__".to_string());
        names.extend(
            value::class_namespace(object)
                .into_iter()
                .map(|(name, _)| name),
        );
        collect_class_names_dir(object, &mut names);
    } else if let Some(module) = value::as_module(object) {
        let module_name = module.name().to_string();
        if module_name == "errno" {
            return exc::raise_with_message(
                NOT_IMPLEMENTED_ERROR,
                "dir() cannot enumerate 'errno': its names are resolved on demand",
            );
        }
        names.push("__name__".to_string());
        names.push("__dict__".to_string());
        if let Some(exported) = modules::module_attr_names(module) {
            // Compiled extension module: `dir` enumerates the exact table
            // `getattr`/`hasattr` read.
            match exported {
                Ok(exported) => names.extend(exported),
                Err(message) => return modules::extension::raised_error(&message),
            }
        } else {
            match stdlib::module(&module_name) {
                Some(spec) => {
                    names.extend(spec.functions.iter().map(|f| f.name.to_string()));
                    names.extend(spec.constants.iter().map(|c| c.name.to_string()));
                    if module_name == "builtins" {
                        names.push("eval".to_string());
                        names.push("exec".to_string());
                    }
                }
                None => {
                    // Compiled user module: a PEP 562 `__dir__` wins over the
                    // namespace enumeration, exactly like CPython's `dir()`.
                    if let Some(result) = modules::module_dir_value(&module_name) {
                        return result;
                    }
                    // Otherwise the globals side store plus any names bound
                    // dynamically through its `globals()` view.
                    names.extend(globals::names(&module_name));
                    names.extend(globals::view_names(&module_name));
                }
            }
        }
        // Program assignments are attributes too, even when the name is not in
        // the native spec or the globals store (deletion removes them again).
        names.extend(
            globals::attribute_overrides(&module_name)
                .into_iter()
                .map(|(name, _)| name),
        );
    } else if let Some(function) = value::as_function(object) {
        names.extend(["__name__", "__qualname__", "__doc__"].map(str::to_string));
        if value::function_has_attributes(object) {
            names.push("__dict__".to_string());
            names.extend(value::function_attribute_names(object));
        }
        if value::function_module(object).is_some() {
            names.push("__module__".to_string());
        }
        if value::function_annotations(object).is_some() {
            names.push("__annotations__".to_string());
        }
        if matches!(&function.body, FunctionBody::Partial { .. }) {
            names.extend(["func", "args", "keywords"].map(str::to_string));
        }
    } else {
        return exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            &format!(
                "dir() is not supported for '{}' objects in Hydro",
                value::type_name(object)
            ),
        );
    }
    names.sort_unstable();
    names.dedup();
    let result = value::list_new(Vec::new());
    for name in names {
        let text = value::str_alloc(name);
        let roots = crate::heap::root_values(&[text]);
        container::list_append(result, text);
        crate::heap::release_roots(roots);
    }
    result
}

/// Appends the own-namespace names of `class` and its base chain.
fn collect_class_names_dir(class: i64, names: &mut Vec<String>) {
    let mut current = class;
    for _ in 0..1024 {
        let Some(state) = value::as_class(current) else {
            return;
        };
        names.extend(
            value::class_namespace(current)
                .into_iter()
                .map(|(name, _)| name),
        );
        if state.base == NONE {
            return;
        }
        current = state.base;
    }
}

/// Shortens a source string for an error message (char-boundary safe).
fn source_snippet(source: &str) -> String {
    const LIMIT: usize = 60;
    if source.chars().count() <= LIMIT {
        return source.to_string();
    }
    let mut snippet: String = source.chars().take(LIMIT).collect();
    snippet.push('…');
    snippet
}

/// Reads `name` from a namespace dict, without raising for a miss.
fn namespace_get(namespace: i64, name: &str) -> Option<i64> {
    let entries = value::as_dict(namespace)?;
    entries
        .iter()
        .find_map(|(key, item)| (value::as_str(*key) == Some(name)).then_some(*item))
}

/// `eval(source[, globals[, locals]])`.
///
/// Hydro has no runtime Python compiler. A recursive-descent evaluator covers
/// the forms annotation libraries actually evaluate: literals, names,
/// attribute reads, subscripts, unary/binary arithmetic, shifts, bitwise
/// operators, comparisons and boolean operators. Names resolve in `locals`,
/// then `globals`, then `builtins`; a miss is a real `NameError`. Call,
/// comprehension and lambda forms raise a tagged `NotImplementedError`
/// instead of guessing.
fn eval_builtin(args: &[i64]) -> i64 {
    let (source, global_ns, local_ns) = match args {
        [source] => (*source, NONE, NONE),
        [source, globals] => (*source, *globals, NONE),
        [source, globals, locals] => (*source, *globals, *locals),
        _ => return builtin_arity_error("eval", "1 to 3", args.len()),
    };
    let Some(text) = value::as_str(source) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "eval() arg 1 must be a string, bytes or code object, not '{}'",
                value::type_name(source)
            ),
        );
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return exc::raise_with_message(SYNTAX_ERROR, "invalid syntax");
    }
    // Namespaces must be real dicts before anything is evaluated.
    for namespace in [global_ns, local_ns] {
        if namespace != NONE && value::as_dict(namespace).is_none() {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "eval() namespace must be a dict, not '{}'",
                    value::type_name(namespace)
                ),
            );
        }
    }
    let trimmed_value = value::str_alloc(trimmed.to_string());
    let literal = modules::ast::call("literal_eval", &[trimmed_value]);
    if !exc::pending() {
        return literal;
    }
    // `literal_eval` reports non-literal text as `ValueError`; that only means
    // "not a literal", not a user-visible failure.
    if exc::pending_tag() != VALUE_ERROR as i64 {
        return NONE;
    }
    exc::clear();

    let mut parser = EvalParser::new(trimmed, global_ns, local_ns);
    match parser.parse_document() {
        Ok(value) => value,
        Err(EvalError::Pending) => NONE,
        Err(EvalError::Name(name)) => {
            exc::raise_with_message(NAME_ERROR, &format!("name '{name}' is not defined"))
        }
        Err(EvalError::Syntax) => exc::raise_with_message(SYNTAX_ERROR, "invalid syntax"),
        Err(EvalError::Unsupported) => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            &format!(
                "eval() cannot compile this expression at runtime in Hydro: '{}' \
                 (only literals, names, attributes, subscripts and arithmetic \
                 expressions are evaluated; whole programs are compiled ahead of time)",
                source_snippet(trimmed)
            ),
        ),
    }
}

/// Failure of the runtime expression evaluator.
enum EvalError {
    /// A runtime operation raised; the pending exception is already set.
    Pending,
    Name(String),
    Syntax,
    Unsupported,
}

type EvalResult = Result<i64, EvalError>;

/// Recursive-descent evaluator for the expression subset `eval` supports.
///
/// The grammar covers the forms annotation libraries actually evaluate:
/// literals, names, attribute reads, subscripts, unary/binary arithmetic,
/// shifts, bitwise operators, comparisons and short-circuiting boolean
/// operators. A call, comprehension or lambda is reported as unsupported
/// (`NotImplementedError`) instead of being guessed.
struct EvalParser {
    chars: Vec<char>,
    position: usize,
    global_ns: i64,
    local_ns: i64,
}

impl EvalParser {
    fn new(source: &str, global_ns: i64, local_ns: i64) -> EvalParser {
        EvalParser {
            chars: source.chars().collect(),
            position: 0,
            global_ns,
            local_ns,
        }
    }

    fn parse_document(&mut self) -> EvalResult {
        let value = self.parse_or()?;
        self.skip_whitespace();
        if self.position != self.chars.len() {
            return Err(EvalError::Syntax);
        }
        Ok(value)
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.position + offset).copied()
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.position += 1;
        }
    }

    /// Consumes `symbol` after optional whitespace, rewinding on failure.
    fn match_symbol(&mut self, symbol: &str) -> bool {
        self.skip_whitespace();
        let start = self.position;
        for expected in symbol.chars() {
            if self.peek() != Some(expected) {
                self.position = start;
                return false;
            }
            self.position += 1;
        }
        true
    }

    /// Consumes a keyword only when it ends at an identifier boundary.
    fn eat_keyword(&mut self, keyword: &str) -> bool {
        self.skip_whitespace();
        let start = self.position;
        for expected in keyword.chars() {
            if self.peek() != Some(expected) {
                self.position = start;
                return false;
            }
            self.position += 1;
        }
        if self
            .peek()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
        {
            self.position = start;
            return false;
        }
        true
    }

    fn match_phrase(&mut self, phrase: &[&str]) -> bool {
        self.skip_whitespace();
        let start = self.position;
        for word in phrase {
            if !self.eat_keyword(word) {
                self.position = start;
                return false;
            }
        }
        true
    }

    /// True when the next two non-space characters are `**` (so the
    /// multiplication level must not consume a single `*`).
    fn peek_double_star(&mut self) -> bool {
        self.skip_whitespace();
        self.peek() == Some('*') && self.peek_at(1) == Some('*')
    }

    fn parse_or(&mut self) -> EvalResult {
        let mut left = self.parse_and()?;
        while self.eat_keyword("or") {
            let roots = crate::heap::root_values(&[left]);
            let right = match self.parse_and() {
                Ok(value) => value,
                Err(error) => {
                    crate::heap::release_roots(roots);
                    return Err(error);
                }
            };
            let selected = if value::truthy(left) { left } else { right };
            crate::heap::release_roots(roots);
            left = selected;
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> EvalResult {
        let mut left = self.parse_not()?;
        while self.eat_keyword("and") {
            let roots = crate::heap::root_values(&[left]);
            let right = match self.parse_not() {
                Ok(value) => value,
                Err(error) => {
                    crate::heap::release_roots(roots);
                    return Err(error);
                }
            };
            let selected = if value::truthy(left) { right } else { left };
            crate::heap::release_roots(roots);
            left = selected;
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> EvalResult {
        if self.eat_keyword("not") {
            let operand = self.parse_not()?;
            return self.apply_unary(UnOp::Not, operand);
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> EvalResult {
        let mut left = self.parse_bitor()?;
        let mut compared = false;
        loop {
            let op = if self.match_phrase(&["is", "not"]) {
                Some(BinOp::IsNot)
            } else if self.eat_keyword("is") {
                Some(BinOp::Is)
            } else if self.match_phrase(&["not", "in"]) {
                Some(BinOp::NotIn)
            } else if self.eat_keyword("in") {
                Some(BinOp::In)
            } else if self.match_symbol("==") {
                Some(BinOp::Eq)
            } else if self.match_symbol("!=") {
                Some(BinOp::Ne)
            } else if self.match_symbol("<=") {
                Some(BinOp::Le)
            } else if self.match_symbol(">=") {
                Some(BinOp::Ge)
            } else if self.match_symbol("<") {
                Some(BinOp::Lt)
            } else if self.match_symbol(">") {
                Some(BinOp::Gt)
            } else {
                None
            };
            let Some(op) = op else { break };
            let roots = crate::heap::root_values(&[left]);
            let right = match self.parse_bitor() {
                Ok(value) => value,
                Err(error) => {
                    crate::heap::release_roots(roots);
                    return Err(error);
                }
            };
            let result = ops::binop(op.code(), left, right);
            crate::heap::release_roots(roots);
            if exc::pending() {
                return Err(EvalError::Pending);
            }
            compared = true;
            // Chained comparisons compare the previous right operand next.
            // Python evaluates until the first false result.
            left = right;
            if !value::truthy(result) {
                return Ok(value::FALSE);
            }
        }
        if compared {
            Ok(value::TRUE)
        } else {
            Ok(left)
        }
    }

    fn parse_bitor(&mut self) -> EvalResult {
        let mut left = self.parse_bitxor()?;
        while self.match_symbol("|") {
            let roots = crate::heap::root_values(&[left]);
            let right = match self.parse_bitxor() {
                Ok(value) => value,
                Err(error) => {
                    crate::heap::release_roots(roots);
                    return Err(error);
                }
            };
            let result = ops::binop(BinOp::BitOr.code(), left, right);
            crate::heap::release_roots(roots);
            if exc::pending() {
                return Err(EvalError::Pending);
            }
            left = result;
        }
        Ok(left)
    }

    fn parse_bitxor(&mut self) -> EvalResult {
        let mut left = self.parse_bitand()?;
        while self.match_symbol("^") {
            let roots = crate::heap::root_values(&[left]);
            let right = match self.parse_bitand() {
                Ok(value) => value,
                Err(error) => {
                    crate::heap::release_roots(roots);
                    return Err(error);
                }
            };
            let result = ops::binop(BinOp::BitXor.code(), left, right);
            crate::heap::release_roots(roots);
            if exc::pending() {
                return Err(EvalError::Pending);
            }
            left = result;
        }
        Ok(left)
    }

    fn parse_bitand(&mut self) -> EvalResult {
        let mut left = self.parse_shift()?;
        while self.match_symbol("&") {
            let roots = crate::heap::root_values(&[left]);
            let right = match self.parse_shift() {
                Ok(value) => value,
                Err(error) => {
                    crate::heap::release_roots(roots);
                    return Err(error);
                }
            };
            let result = ops::binop(BinOp::BitAnd.code(), left, right);
            crate::heap::release_roots(roots);
            if exc::pending() {
                return Err(EvalError::Pending);
            }
            left = result;
        }
        Ok(left)
    }

    fn parse_shift(&mut self) -> EvalResult {
        let mut left = self.parse_additive()?;
        loop {
            let op = if self.match_symbol("<<") {
                Some(BinOp::Shl)
            } else if self.match_symbol(">>") {
                Some(BinOp::Shr)
            } else {
                None
            };
            let Some(op) = op else { break };
            let roots = crate::heap::root_values(&[left]);
            let right = match self.parse_additive() {
                Ok(value) => value,
                Err(error) => {
                    crate::heap::release_roots(roots);
                    return Err(error);
                }
            };
            let result = ops::binop(op.code(), left, right);
            crate::heap::release_roots(roots);
            if exc::pending() {
                return Err(EvalError::Pending);
            }
            left = result;
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> EvalResult {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = if self.match_symbol("+") {
                Some(BinOp::Add)
            } else if self.match_symbol("-") {
                Some(BinOp::Sub)
            } else {
                None
            };
            let Some(op) = op else { break };
            let roots = crate::heap::root_values(&[left]);
            let right = match self.parse_multiplicative() {
                Ok(value) => value,
                Err(error) => {
                    crate::heap::release_roots(roots);
                    return Err(error);
                }
            };
            let result = ops::binop(op.code(), left, right);
            crate::heap::release_roots(roots);
            if exc::pending() {
                return Err(EvalError::Pending);
            }
            left = result;
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> EvalResult {
        let mut left = self.parse_factor()?;
        loop {
            if self.peek_double_star() {
                break;
            }
            let op = if self.match_symbol("//") {
                Some(BinOp::FloorDiv)
            } else if self.match_symbol("*") {
                Some(BinOp::Mul)
            } else if self.match_symbol("/") {
                Some(BinOp::Div)
            } else if self.match_symbol("%") {
                Some(BinOp::Mod)
            } else {
                None
            };
            let Some(op) = op else { break };
            let roots = crate::heap::root_values(&[left]);
            let right = match self.parse_factor() {
                Ok(value) => value,
                Err(error) => {
                    crate::heap::release_roots(roots);
                    return Err(error);
                }
            };
            let result = ops::binop(op.code(), left, right);
            crate::heap::release_roots(roots);
            if exc::pending() {
                return Err(EvalError::Pending);
            }
            left = result;
        }
        Ok(left)
    }

    fn parse_factor(&mut self) -> EvalResult {
        if self.match_symbol("+") {
            let operand = self.parse_factor()?;
            return self.apply_unary(UnOp::Pos, operand);
        }
        if self.match_symbol("-") {
            let operand = self.parse_factor()?;
            return self.apply_unary(UnOp::Neg, operand);
        }
        if self.match_symbol("~") {
            let operand = self.parse_factor()?;
            return self.apply_unary(UnOp::Invert, operand);
        }
        self.parse_power()
    }

    fn parse_power(&mut self) -> EvalResult {
        let base = self.parse_postfix()?;
        if self.match_symbol("**") {
            let roots = crate::heap::root_values(&[base]);
            let exponent = match self.parse_factor() {
                Ok(value) => value,
                Err(error) => {
                    crate::heap::release_roots(roots);
                    return Err(error);
                }
            };
            let result = ops::binop(BinOp::Pow.code(), base, exponent);
            crate::heap::release_roots(roots);
            if exc::pending() {
                return Err(EvalError::Pending);
            }
            return Ok(result);
        }
        Ok(base)
    }

    fn parse_postfix(&mut self) -> EvalResult {
        let mut value = self.parse_atom()?;
        loop {
            if self.match_symbol(".") {
                let Some(name) = self.parse_identifier() else {
                    return Err(EvalError::Syntax);
                };
                let roots = crate::heap::root_values(&[value]);
                let attribute = methods::getattr(value, name.as_ptr(), name.len());
                crate::heap::release_roots(roots);
                if exc::pending() {
                    return Err(EvalError::Pending);
                }
                value = attribute;
                continue;
            }
            if self.match_symbol("[") {
                let roots = crate::heap::root_values(&[value]);
                let index = match self.parse_or() {
                    Ok(index) => index,
                    Err(error) => {
                        crate::heap::release_roots(roots);
                        return Err(error);
                    }
                };
                if !self.match_symbol("]") {
                    crate::heap::release_roots(roots);
                    return Err(EvalError::Syntax);
                }
                let roots = crate::heap::root_values(&[index]);
                let item = container::subscript(value, index);
                crate::heap::release_roots(roots);
                if exc::pending() {
                    return Err(EvalError::Pending);
                }
                value = item;
                continue;
            }
            if self.match_symbol("(") {
                // A runtime call would need the full call protocol; annotation
                // evaluation does not rely on it, so report it as unsupported
                // instead of executing only part of the expression.
                return Err(EvalError::Unsupported);
            }
            break;
        }
        Ok(value)
    }

    fn parse_atom(&mut self) -> EvalResult {
        self.skip_whitespace();
        match self.peek() {
            None => Err(EvalError::Syntax),
            Some('(') => self.parse_parenthesized(),
            Some('[') => self.parse_list(),
            Some('{') => self.parse_braces(),
            Some('"' | '\'') => self.parse_string(),
            Some(ch) if ch.is_ascii_digit() || ch == '.' => self.parse_number(),
            Some(ch) if ch == '_' || ch.is_alphabetic() => self.parse_name_atom(),
            Some(_) => Err(EvalError::Syntax),
        }
    }

    fn parse_parenthesized(&mut self) -> EvalResult {
        self.match_symbol("(");
        if self.match_symbol(")") {
            return Ok(value::tuple_new(Vec::new()));
        }
        let first = self.parse_or()?;
        if !self.match_symbol(",") {
            if !self.match_symbol(")") {
                return Err(EvalError::Syntax);
            }
            return Ok(first);
        }
        let roots = crate::heap::root_values(&[first]);
        let mut items = vec![first];
        loop {
            if self.match_symbol(")") {
                break;
            }
            let item = match self.parse_or() {
                Ok(item) => item,
                Err(error) => {
                    crate::heap::release_roots(roots);
                    return Err(error);
                }
            };
            crate::heap::root_values(&[item]);
            items.push(item);
            if self.match_symbol(",") {
                continue;
            }
            if !self.match_symbol(")") {
                crate::heap::release_roots(roots);
                return Err(EvalError::Syntax);
            }
            break;
        }
        let tuple = value::tuple_new(items);
        crate::heap::release_roots(roots);
        Ok(tuple)
    }

    fn parse_list(&mut self) -> EvalResult {
        self.match_symbol("[");
        let mark = crate::heap::root_values(&[]);
        let mut items = Vec::new();
        loop {
            if self.match_symbol("]") {
                break;
            }
            let item = match self.parse_or() {
                Ok(item) => item,
                Err(error) => {
                    crate::heap::release_roots(mark);
                    return Err(error);
                }
            };
            crate::heap::root_values(&[item]);
            items.push(item);
            if self.match_symbol(",") {
                continue;
            }
            if !self.match_symbol("]") {
                crate::heap::release_roots(mark);
                return Err(EvalError::Syntax);
            }
            break;
        }
        let list = value::list_new(items);
        crate::heap::release_roots(mark);
        Ok(list)
    }

    fn parse_braces(&mut self) -> EvalResult {
        self.match_symbol("{");
        if self.match_symbol("}") {
            return Ok(value::dict_new(Vec::new()));
        }
        let mark = crate::heap::root_values(&[]);
        let first = match self.parse_or() {
            Ok(value) => value,
            Err(error) => {
                crate::heap::release_roots(mark);
                return Err(error);
            }
        };
        // Park the first element while the rest of the literal is parsed.
        crate::heap::root_values(&[first]);
        if self.match_symbol(":") {
            let mut pairs = Vec::new();
            let value = match self.parse_or() {
                Ok(value) => value,
                Err(error) => {
                    crate::heap::release_roots(mark);
                    return Err(error);
                }
            };
            crate::heap::root_values(&[value]);
            pairs.push((first, value));
            loop {
                if self.match_symbol("}") {
                    break;
                }
                if !self.match_symbol(",") {
                    crate::heap::release_roots(mark);
                    return Err(EvalError::Syntax);
                }
                if self.match_symbol("}") {
                    break;
                }
                let key = match self.parse_or() {
                    Ok(key) => key,
                    Err(error) => {
                        crate::heap::release_roots(mark);
                        return Err(error);
                    }
                };
                if !self.match_symbol(":") {
                    crate::heap::release_roots(mark);
                    return Err(EvalError::Syntax);
                }
                let value = match self.parse_or() {
                    Ok(value) => value,
                    Err(error) => {
                        crate::heap::release_roots(mark);
                        return Err(error);
                    }
                };
                crate::heap::root_values(&[key, value]);
                pairs.push((key, value));
            }
            let dict = value::dict_new(pairs);
            crate::heap::release_roots(mark);
            return Ok(dict);
        }
        let mut items = vec![first];
        loop {
            if self.match_symbol("}") {
                break;
            }
            if !self.match_symbol(",") {
                crate::heap::release_roots(mark);
                return Err(EvalError::Syntax);
            }
            if self.match_symbol("}") {
                break;
            }
            let item = match self.parse_or() {
                Ok(item) => item,
                Err(error) => {
                    crate::heap::release_roots(mark);
                    return Err(error);
                }
            };
            crate::heap::root_values(&[item]);
            items.push(item);
        }
        let set = value::set_new(items);
        crate::heap::release_roots(mark);
        Ok(set)
    }

    fn parse_string(&mut self) -> EvalResult {
        self.skip_whitespace();
        let start = self.position;
        let Some(quote) = self.peek() else {
            return Err(EvalError::Syntax);
        };
        self.position += 1;
        loop {
            match self.peek() {
                None => return Err(EvalError::Syntax),
                Some('\\') => {
                    self.position += 1;
                    if self.peek().is_none() {
                        return Err(EvalError::Syntax);
                    }
                    self.position += 1;
                }
                Some(ch) if ch == quote => {
                    self.position += 1;
                    break;
                }
                Some(_) => self.position += 1,
            }
        }
        let literal: String = self.chars[start..self.position].iter().collect();
        let source = value::str_alloc(literal);
        let roots = crate::heap::root_values(&[source]);
        let parsed = modules::ast::call("literal_eval", &[source]);
        let failed = exc::pending();
        if failed {
            exc::clear();
        }
        crate::heap::release_roots(roots);
        if failed {
            return Err(EvalError::Syntax);
        }
        Ok(parsed)
    }

    fn parse_number(&mut self) -> EvalResult {
        self.skip_whitespace();
        let start = self.position;
        let hexadecimal = self.peek() == Some('0') && matches!(self.peek_at(1), Some('x' | 'X'))
            || self.peek() == Some('0') && matches!(self.peek_at(1), Some('o' | 'O'))
            || self.peek() == Some('0') && matches!(self.peek_at(1), Some('b' | 'B'));
        if hexadecimal {
            self.position += 2;
            while self
                .peek()
                .is_some_and(|ch| ch.is_ascii_hexdigit() || ch == '_')
            {
                self.position += 1;
            }
        } else {
            let mut previous = '\0';
            while let Some(ch) = self.peek() {
                let digit = ch.is_ascii_digit() || ch == '_' || ch == '.';
                let exponent = matches!(ch, 'e' | 'E');
                let exponent_sign = matches!(ch, '+' | '-') && matches!(previous, 'e' | 'E');
                let radix_marker =
                    matches!(ch, 'x' | 'X' | 'o' | 'O' | 'b' | 'B') && previous == '0';
                if !(digit || exponent || exponent_sign || radix_marker) {
                    break;
                }
                previous = ch;
                self.position += 1;
            }
        }
        let text: String = self.chars[start..self.position]
            .iter()
            .filter(|ch| **ch != '_')
            .collect();
        if text.is_empty() {
            return Err(EvalError::Syntax);
        }
        let lower = text.to_ascii_lowercase();
        if let Some(digits) = lower.strip_prefix("0x") {
            return parse_integer(digits, 16);
        }
        if let Some(digits) = lower.strip_prefix("0o") {
            return parse_integer(digits, 8);
        }
        if let Some(digits) = lower.strip_prefix("0b") {
            return parse_integer(digits, 2);
        }
        if text.contains('.') || text.contains('e') || text.contains('E') {
            return text
                .parse::<f64>()
                .map(value::float_new)
                .map_err(|_| EvalError::Syntax);
        }
        if let Ok(integer) = text.parse::<i64>() {
            return Ok(ops::int_result(integer));
        }
        match BigInt::parse_bytes(text.as_bytes(), 10) {
            Some(integer) => Ok(value::bigint_new(integer)),
            None => Err(EvalError::Syntax),
        }
    }

    fn parse_name_atom(&mut self) -> EvalResult {
        let Some(name) = self.parse_identifier() else {
            return Err(EvalError::Syntax);
        };
        match name.as_str() {
            "True" => Ok(value::TRUE),
            "False" => Ok(value::FALSE),
            "None" => Ok(NONE),
            "and" | "or" | "not" | "in" | "is" => Err(EvalError::Syntax),
            _ => self.lookup_name(&name),
        }
    }

    fn parse_identifier(&mut self) -> Option<String> {
        self.skip_whitespace();
        let start = self.position;
        let first = self.peek()?;
        if first != '_' && !first.is_alphabetic() {
            return None;
        }
        self.position += 1;
        while self
            .peek()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
        {
            self.position += 1;
        }
        Some(self.chars[start..self.position].iter().collect())
    }

    fn lookup_name(&mut self, name: &str) -> EvalResult {
        for namespace in [self.local_ns, self.global_ns] {
            if namespace == NONE {
                continue;
            }
            if let Some(value) = namespace_get(namespace, name) {
                return Ok(value);
            }
        }
        let value = modules::module_get_by_name("builtins", name);
        if exc::pending() {
            if exc::pending_tag() == exc::ATTRIBUTE_ERROR as i64 {
                exc::clear();
                return Err(EvalError::Name(name.to_string()));
            }
            return Err(EvalError::Pending);
        }
        Ok(value)
    }

    fn apply_unary(&mut self, op: UnOp, operand: i64) -> EvalResult {
        let roots = crate::heap::root_values(&[operand]);
        let result = ops::unop(op.code(), operand);
        crate::heap::release_roots(roots);
        if exc::pending() {
            return Err(EvalError::Pending);
        }
        Ok(result)
    }
}

/// Parses a non-decimal integer literal, promoting out-of-range values to the
/// runtime `BigInt` kind instead of failing.
fn parse_integer(digits: &str, radix: u32) -> EvalResult {
    if let Ok(integer) = i64::from_str_radix(digits, radix) {
        return Ok(ops::int_result(integer));
    }
    match BigInt::parse_bytes(digits.as_bytes(), radix) {
        Some(integer) => Ok(value::bigint_new(integer)),
        None => Err(EvalError::Syntax),
    }
}

/// Parsed form of the single generated function definition `exec` accepts.
struct ExecSource {
    name: String,
    param: String,
    /// Second positional-only parameter for the `raise_from` shape.
    second: Option<String>,
    check: value::ExecCheck,
}

/// `exec(source[, globals[, locals]])`.
///
/// Hydro has no runtime statement compiler. The one generated source shape the
/// standard library builds at runtime is the pair of factories in
/// `typing_inspection`: a single function definition whose body returns an
/// identity or `isinstance` check against a cached typing object. That shape is
/// compiled into a [`FunctionBody::Exec`] value and installed in the locals
/// mapping (or in globals when locals is omitted). Every other statement
/// raises a `NotImplementedError` naming the rejected construct instead of
/// silently doing nothing.
fn exec_builtin(args: &[i64]) -> i64 {
    if args.is_empty() || args.len() > 3 {
        return builtin_arity_error("exec", "1 to 3", args.len());
    }
    let Some(source) = value::as_str(args[0]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "exec() arg 1 must be a str, not '{}'",
                value::type_name(args[0])
            ),
        );
    };
    let provided_globals = args.get(1).copied();
    let provided_locals = args.get(2).copied();
    // Namespaces must be real dicts before anything is compiled.
    for (label, namespace) in [("globals", provided_globals), ("locals", provided_locals)] {
        if let Some(namespace) = namespace {
            if namespace != NONE && value::as_dict(namespace).is_none() {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "exec() {label} must be a dict, not '{}'",
                        value::type_name(namespace)
                    ),
                );
            }
        }
    }
    let parsed = match parse_exec_source(source) {
        Ok(parsed) => parsed,
        Err(construct) => {
            return exc::raise_with_message(
                NOT_IMPLEMENTED_ERROR,
                &format!(
                    "exec() cannot compile Python source at runtime in Hydro: {construct} \
                     (source: '{}'); supported source is a single function definition \
                     returning `PARAM is NAME`, `PARAM is NAME or PARAM is NAME`, \
                     `isinstance(PARAM, NAME)` or `False`",
                    source_snippet(source)
                ),
            )
        }
    };
    let globals_ns = match provided_globals {
        None | Some(NONE) => value::dict_new(Vec::new()),
        Some(namespace) => namespace,
    };
    let locals_ns = match provided_locals {
        None | Some(NONE) => globals_ns,
        Some(namespace) => namespace,
    };
    let roots = crate::heap::root_values(&[globals_ns, locals_ns]);
    let key = value::str_alloc(parsed.name.clone());
    let key_root = crate::heap::root_values(&[key]);
    let function = value::exec_function_new(
        parsed.name,
        parsed.param,
        parsed.second,
        parsed.check,
        globals_ns,
        locals_ns,
    );
    let live = crate::heap::root_values(&[key, function]);
    container::dict_set(locals_ns, key, function);
    crate::heap::release_roots(live);
    crate::heap::release_roots(key_root);
    crate::heap::release_roots(roots);
    NONE
}

/// Parses the bounded `exec` grammar.
///
/// Accepted shape, ignoring surrounding whitespace:
///
/// ```text
/// def NAME(PARAM: ANNOTATION, /) -> ANNOTATION:
///     return EXPR
/// ```
///
/// where `EXPR` is one of `PARAM is NAME`, `PARAM is NAME or PARAM is NAME`,
/// `isinstance(PARAM, NAME)` or `False`. Annotations may name anything; they
/// are validated only enough to find the end of the header. Every rejection
/// returns a description of the unsupported construct.
fn parse_exec_source(source: &str) -> Result<ExecSource, String> {
    let text = source.trim();
    if text.is_empty() {
        return Err("an empty source string".to_string());
    }
    let rest = text
        .strip_prefix("def")
        .ok_or_else(|| "a source that is not a single 'def' statement".to_string())?;
    if !rest.starts_with(char::is_whitespace) {
        return Err("a source that is not a single 'def' statement".to_string());
    }
    let rest = rest.trim_start();
    let name_len = rest
        .find(|ch: char| !(ch == '_' || ch.is_alphanumeric()))
        .unwrap_or(rest.len());
    let name = &rest[..name_len];
    if !is_exec_identifier(name) {
        return Err("a function definition without a valid name".to_string());
    }
    let after_name = rest[name_len..].trim_start();
    let Some(after_open) = after_name.strip_prefix('(') else {
        return Err("a function header without a parameter list".to_string());
    };
    let Some(close) = exec_closing_paren(after_open) else {
        return Err("a function header without a closing ')'".to_string());
    };
    let after_params = after_open[close + 1..].trim_start();
    let colon = exec_header_colon(after_params)?;
    let body = after_params[colon + 1..].trim();
    if body.is_empty() {
        return Err("a function definition without a body".to_string());
    }
    // `six.raise_from` (installed through `exec` on Python 3) is the one
    // multi-statement shape the standard library generates. Recognise the
    // exact `raise value from from_value` statement and keep the rest of the
    // bounded grammar unchanged.
    let normalized_body = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized_body.contains("raise value from from_value") {
        let parameters = after_open[..close].trim();
        let mut parts = parameters.split(',').map(str::trim);
        let (Some(value), Some(cause), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err("a function body with a raise statement".to_string());
        };
        if !is_exec_identifier(value) || !is_exec_identifier(cause) {
            return Err("a function body with a raise statement".to_string());
        }
        return Ok(ExecSource {
            name: name.to_string(),
            param: value.to_string(),
            second: Some(cause.to_string()),
            check: value::ExecCheck::RaiseFrom,
        });
    }
    let param = parse_exec_parameter(&after_open[..close])?;
    if body.chars().any(|ch| matches!(ch, '\n' | '\r')) {
        return Err("a function body with more than one statement".to_string());
    }
    let Some(expression) = body.strip_prefix("return") else {
        return Err("a function body that is not a single 'return' statement".to_string());
    };
    if !expression.starts_with(' ') && !expression.starts_with('\t') {
        return Err("a function body that is not a single 'return' statement".to_string());
    }
    let expression = expression.trim();
    if expression.is_empty() {
        return Err("a 'return' statement without an expression".to_string());
    }
    if expression.contains(';') {
        return Err("a function body with more than one statement".to_string());
    }
    let check = parse_exec_check(expression, &param)?;
    Ok(ExecSource {
        name: name.to_string(),
        param,
        second: None,
        check,
    })
}

/// Parses `PARAM: ANNOTATION, /` (the comma and `/` are mandatory).
fn parse_exec_parameter(text: &str) -> Result<String, String> {
    let text = text.trim();
    let Some(before_slash) = text.strip_suffix('/') else {
        return Err("a parameter list without the positional-only '/' marker".to_string());
    };
    let before_slash = before_slash.trim_end();
    let Some(parameter) = before_slash.strip_suffix(',') else {
        return Err("a positional-only parameter without the separating comma".to_string());
    };
    let parameter = parameter.trim_end();
    if parameter.is_empty() {
        return Err("a function definition without a parameter".to_string());
    }
    if exec_find_top_level(parameter, ',').is_some() {
        return Err("a function definition with more than one parameter".to_string());
    }
    let (name, annotation) = match parameter.split_once(':') {
        Some((name, annotation)) => (name.trim(), Some(annotation.trim())),
        None => (parameter, None),
    };
    if !is_exec_identifier(name) {
        return Err("a parameter that is not a single positional-only name".to_string());
    }
    if annotation.is_some_and(str::is_empty) {
        return Err("a parameter with an empty annotation".to_string());
    }
    Ok(name.to_string())
}

/// Parses the return expression forms `typing_inspection` generates.
fn parse_exec_check(expression: &str, param: &str) -> Result<value::ExecCheck, String> {
    let normalized = expression.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized == "False" {
        return Ok(value::ExecCheck::AlwaysFalse);
    }
    if let Some(arguments) = normalized
        .strip_prefix("isinstance(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let Some((value_text, target_text)) = arguments.split_once(',') else {
            return Err(format!("the return expression '{normalized}'"));
        };
        let value_text = value_text.trim();
        let target_text = target_text.trim();
        if value_text != param
            || !is_exec_identifier(target_text)
            || exec_find_top_level(target_text, ',').is_some()
        {
            return Err(format!("the return expression '{normalized}'"));
        }
        return Ok(value::ExecCheck::IsInstance {
            name: target_text.to_string(),
        });
    }
    let parts: Vec<&str> = normalized.split(" or ").collect();
    match parts.as_slice() {
        [single] => Ok(value::ExecCheck::Identity {
            name: parse_exec_identity(single, param, &normalized)?,
        }),
        [first, second] => Ok(value::ExecCheck::IdentityEither {
            first: parse_exec_identity(first, param, &normalized)?,
            second: parse_exec_identity(second, param, &normalized)?,
        }),
        _ => Err(format!("the return expression '{normalized}'")),
    }
}

/// Parses `PARAM is NAME` and returns `NAME`.
fn parse_exec_identity(text: &str, param: &str, expression: &str) -> Result<String, String> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    match parts.as_slice() {
        [left, "is", right] if *left == param && is_exec_identifier(right) => {
            Ok((*right).to_string())
        }
        _ => Err(format!("the return expression '{expression}'")),
    }
}

/// Whether `text` is a Python identifier: a letter or `_` followed by
/// letters, digits or `_`.
fn is_exec_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first != '_' && !first.is_alphabetic() {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_alphanumeric())
}

/// Index of the `)` that closes the parameter list, ignoring parentheses and
/// brackets inside annotations (including those inside string literals).
fn exec_closing_paren(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' | '[' | '{' => depth += 1,
            ')' if depth == 0 => return Some(index),
            ')' => depth -= 1,
            ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    None
}

/// Finds the first `target` at bracket depth zero, outside string literals.
fn exec_find_top_level(text: &str, target: char) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ if ch == target && depth == 0 => return Some(index),
            _ => {}
        }
    }
    None
}

/// Index of the `:` that ends the function header.
fn exec_header_colon(text: &str) -> Result<usize, String> {
    let has_newline = |text: &str| text.chars().any(|ch| matches!(ch, '\n' | '\r'));
    match exec_find_top_level(text, ':') {
        Some(index) => {
            if has_newline(&text[..index]) {
                Err("a function header spanning more than one line".to_string())
            } else {
                Ok(index)
            }
        }
        None => {
            if has_newline(text) {
                Err("a function header spanning more than one line".to_string())
            } else {
                Err("a function header without a trailing ':'".to_string())
            }
        }
    }
}

/// `compile(...)`: no bytecode compiler exists, so fail loudly.
fn compile_builtin(args: &[i64]) -> i64 {
    if args.len() < 3 || args.len() > 6 {
        return builtin_arity_error("compile", "3 to 6", args.len());
    }
    let snippet = value::as_str(args[0])
        .map(source_snippet)
        .unwrap_or_else(|| "<non-string source>".to_string());
    exc::raise_with_message(
        NOT_IMPLEMENTED_ERROR,
        &format!(
            "compile() is not supported: Hydro consumes Python source ahead of time \
             and has no bytecode compiler (source: '{}')",
            snippet
        ),
    )
}

/// Internal: build a class from a runtime namespace dict (see
/// [`Builtin::ClassFromNamespace`]).
///
/// Function values become normal methods (so `Cls.method` is unbound and
/// `instance.method()` binds `self`); every other namespace entry — including
/// `classmethod`/`staticmethod`/`property` descriptor objects — becomes a class
/// attribute, which the attribute protocols already bind correctly.
fn class_from_namespace(name: i64, bases: i64, metaclass: i64, namespace: i64) -> i64 {
    let Some(name_text) = value::as_str(name) else {
        return exc::raise_with_message(TYPE_ERROR, "class name must be a str");
    };
    let name_text = name_text.to_string();
    let Some(base_values) = value::as_tuple(bases) else {
        return exc::raise_with_message(TYPE_ERROR, "class bases must be a tuple");
    };
    let base_values: Vec<i64> = base_values
        .iter()
        .copied()
        .filter(|base| *base != NONE)
        .collect();
    let Some(entries) = value::as_dict(namespace) else {
        return exc::raise_with_message(TYPE_ERROR, "class namespace must be a dict");
    };
    let entries = entries.to_vec();
    let mut methods: Vec<(String, i64, MethodKind)> = Vec::new();
    let mut attrs: Vec<(String, i64)> = Vec::new();
    for (key, item) in entries {
        let Some(attr) = value::as_str(key) else {
            return exc::raise_with_message(TYPE_ERROR, "class namespace keys must be strings");
        };
        if value::as_function(item).is_some() {
            methods.push((attr.to_string(), item, MethodKind::Normal));
        } else {
            attrs.push((attr.to_string(), item));
        }
    }
    value::class_new_multi(name_text, base_values, metaclass, methods, attrs)
}

/// Internal: `globals()[name] = value` writes the binding into the module's
/// runtime global store (see [`Builtin::GlobalSetDynamic`]).
fn dynamic_global_set(module: i64, name: i64, item: i64) -> i64 {
    let (Some(module), Some(name)) = (value::as_str(module), value::as_str(name)) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "globals() item assignment requires a string key",
        );
    };
    globals::set(module, name, item);
    NONE
}

/// Shared `getattr(obj, name)` lookup used by the 2- and 3-argument forms.
fn getattr_builtin(object: i64, name: i64) -> i64 {
    let Some(name) = value::as_str(name) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "attribute name must be string, not '{}'",
                value::type_name(name)
            ),
        );
    };
    // `methods::getattr` dispatches module values (including compiled
    // extensions), the exception attribute surface (`args`, `__cause__`, ...)
    // and every other receiver through a single lookup path.
    methods::getattr(object, name.as_ptr(), name.len())
}

fn builtin_arity_error(name: &str, expected: &str, argc: usize) -> i64 {
    exc::raise_with_message(
        TYPE_ERROR,
        &format!("{name}() takes {expected} argument(s) ({argc} given)"),
    )
}

/// Calls a builtin referenced as a first-class value (`f = int; f("42")`).
///
/// Arity and semantics match the direct builtin calls lowered by codegen:
/// `print` is variadic, `str` takes zero to three
/// (the `str(bytes, encoding[, errors])` decoding form), `repr`/`int`/`float`/`abs`/`len` take one
/// `set`/`list`/`dict` accept zero or one, `sum`/`enumerate` one or two,
/// `range` one to three, `bytes` zero to two, `map`/`filter` exactly two,
/// `iter` one or two, `next` one or two, `max`/`min` at least one (their
/// final argument is the key slot), `sorted` one to three
/// `(iterable, key, reverse)`, `round` one or two, `format` two (or
/// three for the internal conversion flag), `isinstance` and `issubclass`
/// exactly two, `all` exactly one, `ord`/`chr`/`hex`/`bin`/`oct` exactly one,
/// `divmod` exactly two, `callable` exactly one, `id` exactly one, the
/// internal dict merge exactly two and `__import__` one to five (the five-slot
/// vector `name, globals, locals, fromlist, level`).
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
    // SAFETY: ABI contract: `argv` points at `argc` readable values; length checked above.
    let args: &[i64] = if argc == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(argv, argc) }
    };
    match builtin {
        Builtin::Print => print(argc, argv),
        Builtin::Bytes => bytes_build(argc, argv),
        Builtin::ByteArray => bytearray_build(argc, argv),
        Builtin::MemoryView => memoryview_build(argc, argv),
        Builtin::Classmethod => match args {
            [target] => value::classmethod_new(*target),
            _ => builtin_arity_error("classmethod", "exactly one", argc),
        },
        Builtin::Staticmethod => match args {
            [target] => value::staticmethod_new(*target),
            _ => builtin_arity_error("staticmethod", "exactly one", argc),
        },
        // `property([fget[, fset[, fdel[, doc]]]])`. A blank `property()` is a
        // real descriptor whose slots are `None` and which accepts attribute
        // assignment afterwards (`enum` builds redirect descriptors that way).
        // The doc argument is accepted and not stored (Hydro function values
        // carry no docstring).
        Builtin::Property => match args {
            [] => value::property_new(NONE, NONE, NONE),
            [getter] => value::property_new(*getter, NONE, NONE),
            [getter, setter] => value::property_new(*getter, *setter, NONE),
            [getter, setter, deleter] => value::property_new(*getter, *setter, *deleter),
            [getter, setter, deleter, _doc] => value::property_new(*getter, *setter, *deleter),
            _ => builtin_arity_error("property", "0 to 4", argc),
        },
        Builtin::Zip => zip(argc, argv),
        Builtin::Set => match args {
            [] => container::set_new(0, std::ptr::null()),
            [iterable] => container::set_from(*iterable),
            _ => builtin_arity_error("set", "0 or 1", argc),
        },
        // Hydro has a single set representation; `frozenset` builds the same
        // deduplicated value as `set`.
        Builtin::Frozenset => match args {
            [] => container::set_new(0, std::ptr::null()),
            [iterable] => container::set_from(*iterable),
            _ => builtin_arity_error("frozenset", "0 or 1", argc),
        },
        Builtin::Getattr => match args {
            [object, name] => getattr_builtin(*object, *name),
            [object, name, default] => {
                let result = getattr_builtin(*object, *name);
                if exc::pending() && exc::pending_tag() == exc::ATTRIBUTE_ERROR as i64 {
                    exc::clear();
                    *default
                } else {
                    result
                }
            }
            _ => builtin_arity_error("getattr", "2 or 3", argc),
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
        Builtin::Tuple => match args {
            [] => container::tuple_new(0, std::ptr::null()),
            [iterable] => tuple_from(*iterable),
            _ => builtin_arity_error("tuple", "0 or 1", argc),
        },
        Builtin::Hasattr => match args {
            [object, name] => match value::as_str(*name) {
                Some(name) => methods::hasattr(*object, name.as_ptr(), name.len()),
                None => exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "attribute name must be string, not '{}'",
                        value::type_name(*name)
                    ),
                ),
            },
            _ => builtin_arity_error("hasattr", "exactly 2", argc),
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
            [function, iterables @ ..] if !iterables.is_empty() => map_many(*function, iterables),
            _ => builtin_arity_error("map", "at least 2", argc),
        },
        Builtin::Filter => match args {
            [function, iterable] => filter(*function, *iterable),
            _ => builtin_arity_error("filter", "exactly 2", argc),
        },
        Builtin::Iter => match args {
            [obj] => iter::iter_of(*obj),
            [callable, sentinel] => iter::iter_callable(*callable, *sentinel),
            _ => builtin_arity_error("iter", "1 or 2", argc),
        },
        Builtin::Next => match args {
            [obj] => iter::next_value(*obj, NONE, false),
            [obj, default] => iter::next_value(*obj, *default, true),
            _ => builtin_arity_error("next", "1 or 2", argc),
        },
        Builtin::Len => match args {
            [value] => len(*value),
            _ => builtin_arity_error("len", "exactly 1", argc),
        },
        Builtin::Str => match args {
            [] => value::str_alloc(String::new()),
            [value] => to_str(*value),
            [value, encoding] => str_decode(&[*value, *encoding]),
            [value, encoding, errors] => str_decode(&[*value, *encoding, *errors]),
            _ => builtin_arity_error("str", "at most 3", argc),
        },
        Builtin::Repr => match args {
            [value] => to_repr(*value),
            _ => builtin_arity_error("repr", "exactly 1", argc),
        },
        Builtin::Ascii => match args {
            [value] => to_ascii(*value),
            _ => builtin_arity_error("ascii", "exactly 1", argc),
        },
        Builtin::Hash => match args {
            [value] => hash(*value),
            _ => builtin_arity_error("hash", "exactly 1", argc),
        },
        Builtin::Id => match args {
            [value] => id_value(*value),
            _ => builtin_arity_error("id", "exactly 1", argc),
        },
        Builtin::Int => match args {
            [] => ops::int_result(0),
            [value] => to_int(*value),
            [value, base] => to_int_base(*value, *base),
            _ => builtin_arity_error("int", "at most 2", argc),
        },
        Builtin::Float => match args {
            [value] => to_float(*value),
            _ => builtin_arity_error("float", "exactly 1", argc),
        },
        Builtin::Abs => match args {
            [value] => abs(*value),
            _ => builtin_arity_error("abs", "exactly 1", argc),
        },
        Builtin::Bool => match args {
            [] => value::FALSE,
            [value] => value::bool_value(value::truthy(*value)),
            _ => builtin_arity_error("bool", "at most 1", argc),
        },
        Builtin::Type => match args {
            [value] => value::type_class(*value),
            [name, bases, namespace] => type_three_arg(*name, *bases, *namespace),
            _ => builtin_arity_error("type", "1 or 3", argc),
        },
        Builtin::Super => match args {
            [class, object] => super_two_arg(*class, *object),
            [class] => super_unbound(*class),
            [] => exc::raise_with_message(RUNTIME_ERROR, "super(): no arguments"),
            _ => builtin_arity_error("super", "at most 2", argc),
        },
        Builtin::Reversed => match args {
            [value] => reversed(*value),
            _ => builtin_arity_error("reversed", "exactly 1", argc),
        },
        Builtin::Import => {
            if args.len() > 5 {
                return builtin_arity_error("__import__", "1 to 5", argc);
            }
            crate::modules::builtins::import_from_core(args)
        }
        Builtin::Input => match args {
            [] => input_value(None),
            [prompt] => input_value(Some(*prompt)),
            _ => builtin_arity_error("input", "at most 1", argc),
        },
        Builtin::GetIpython => match args {
            [] => get_ipython_value(),
            _ => builtin_arity_error("get_ipython", "exactly 0", argc),
        },
        Builtin::Max => max(argc, argv),
        Builtin::Min => min(argc, argv),
        Builtin::Sorted => sorted(argc, argv),
        Builtin::Round => match args {
            [value] => round_value(*value, NONE),
            [value, ndigits] => round_value(*value, *ndigits),
            _ => builtin_arity_error("round", "1 or 2", argc),
        },
        Builtin::Format => match args {
            // `format(value)` uses the empty format specification; CPython's
            // signature is `format(value, format_spec='')`.
            [value] => format_builtin(*value, crate::value::str_from_bytes(b""), 0),
            [value, spec] => format_builtin(*value, *spec, 0),
            [value, spec, conversion] => match value::as_integer(*conversion) {
                Some(flag @ 0..=3) => format_builtin(*value, *spec, flag as u8),
                _ => exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "format() conversion must be an integer flag, not '{}'",
                        value::type_name(*conversion)
                    ),
                ),
            },
            _ => builtin_arity_error("format", "2 or 3", argc),
        },
        Builtin::Isinstance => match args {
            [object, target] => isinstance(*object, *target),
            _ => builtin_arity_error("isinstance", "exactly 2", argc),
        },
        Builtin::Issubclass => match args {
            [subclass, class] => issubclass(*subclass, *class),
            _ => builtin_arity_error("issubclass", "exactly 2", argc),
        },
        Builtin::All => match args {
            [iterable] => all(*iterable),
            _ => builtin_arity_error("all", "exactly 1", argc),
        },
        Builtin::AnyValue => match args {
            [iterable] => any(*iterable),
            _ => builtin_arity_error("any", "exactly 1", argc),
        },
        Builtin::Ord => match args {
            [value] => ord_value(*value),
            _ => builtin_arity_error("ord", "exactly 1", argc),
        },
        Builtin::Chr => match args {
            [value] => chr_value(*value),
            _ => builtin_arity_error("chr", "exactly 1", argc),
        },
        Builtin::Hex => match args {
            [value] => hex_value(*value),
            _ => builtin_arity_error("hex", "exactly 1", argc),
        },
        Builtin::Bin => match args {
            [value] => bin_value(*value),
            _ => builtin_arity_error("bin", "exactly 1", argc),
        },
        Builtin::Oct => match args {
            [value] => oct_value(*value),
            _ => builtin_arity_error("oct", "exactly 1", argc),
        },
        Builtin::Divmod => match args {
            [a, b] => divmod(*a, *b),
            _ => builtin_arity_error("divmod", "exactly 2", argc),
        },
        Builtin::Callable => match args {
            [value] => callable(*value),
            _ => builtin_arity_error("callable", "exactly 1", argc),
        },
        Builtin::DictMerge => match args {
            [left, right] => dict_merge(*left, *right),
            _ => builtin_arity_error("dict merge", "exactly 2", argc),
        },
        // `globals()` carries the module name as its hidden trailing argument
        // (see the frontend call lowering). A first-class `globals` value has
        // no module context, so it fails loudly instead of returning a fake
        // empty namespace.
        Builtin::Globals => match args {
            [module] => globals_builtin(*module),
            [] => exc::raise_with_message(
                RUNTIME_ERROR,
                "globals() has no module context when called through a first-class value; \
                 call globals() directly",
            ),
            _ => builtin_arity_error("globals", "no", argc),
        },
        // Direct `locals()` is materialised by codegen from the live local
        // slots. The runtime entry point is only reachable through a function
        // value, which has no frame.
        Builtin::Locals => match args {
            [] => exc::raise_with_message(
                RUNTIME_ERROR,
                "locals() has no frame context when called through a first-class value; \
                 call locals() directly",
            ),
            _ => builtin_arity_error("locals", "no", argc),
        },
        Builtin::Dir => match args {
            [object] => dir_value(*object),
            [] => exc::raise_with_message(
                RUNTIME_ERROR,
                "dir() without arguments has no frame context when called through a \
                 first-class value; call dir() directly",
            ),
            _ => builtin_arity_error("dir", "at most 1", argc),
        },
        // `vars()` without an argument is `locals()`; codegen handles that
        // shape directly.
        Builtin::Vars => match args {
            [object] => vars_value(*object),
            [] => exc::raise_with_message(
                RUNTIME_ERROR,
                "vars() without arguments has no frame context when called through a \
                 first-class value; call vars() directly",
            ),
            _ => builtin_arity_error("vars", "at most 1", argc),
        },
        Builtin::Eval => eval_builtin(args),
        Builtin::Exec => exec_builtin(args),
        Builtin::Compile => compile_builtin(args),
        Builtin::ClassFromNamespace => match args {
            [name, bases, metaclass, namespace] => {
                class_from_namespace(*name, *bases, *metaclass, *namespace)
            }
            _ => builtin_arity_error("internal class construction", "exactly 4", argc),
        },
        Builtin::GlobalSetDynamic => match args {
            [module, name, item] => dynamic_global_set(*module, *name, *item),
            _ => builtin_arity_error("internal global store", "exactly 3", argc),
        },
    }
}

/// `input([prompt])`: writes the prompt to stdout without a newline, flushes,
/// reads one line from stdin and returns it as a `str` without the trailing
/// line terminator.
///
/// A non-`str` prompt raises `TypeError`; end of file raises `RuntimeError`
/// (CPython's `EOFError`); an I/O failure raises `OSError`.
fn input_value(prompt: Option<i64>) -> i64 {
    if let Some(prompt) = prompt {
        let Some(text) = value::as_str(prompt) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "input(): argument must be str, not '{}'",
                    value::type_name(prompt)
                ),
            );
        };
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
    }
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) => exc::raise_with_message(RUNTIME_ERROR, "EOF when reading a line"),
        Ok(_) => value::str_alloc(strip_input_newline(line)),
        Err(error) => exc::raise_with_message(OS_ERROR, &format!("input() failed: {error}")),
    }
}

/// Removes one trailing `\n` and the `\r` of a `\r\n` terminator from an
/// input line.
fn strip_input_newline(mut line: String) -> String {
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    line
}

/// `get_ipython()`: the probe libraries such as rich use to detect a Jupyter
/// kernel.
///
/// Hydro has no IPython, so the call returns a fresh instance whose class is
/// named `TerminalInteractiveShell`. `get_ipython().__class__.__name__` is
/// therefore `"TerminalInteractiveShell"` (neither `"ZMQInteractiveShell"` nor
/// a Colab shell), and every string rendering is a plain Hydro repr without
/// `google.colab` or `ZMQInteractiveShell`.
fn get_ipython_value() -> i64 {
    // The class stays reachable from the instance; the conservative stack scan
    // keeps it alive while the instance is allocated.
    let class = value::class_new("TerminalInteractiveShell".to_string(), NONE, Vec::new());
    value::instance_new(class)
}

/// `map(function, iterable, *iterables)`: materialises a list with one call
/// per tuple of source elements. With several iterables the mapping stops at
/// the shortest one, matching CPython. The result list, iterators and the
/// per-call argument vector are live roots while callbacks run, so a
/// collection inside the callback cannot free them.
pub(crate) fn map(function: i64, iterable: i64) -> i64 {
    map_many(function, &[iterable])
}

/// Multi-iterable body of [`map`].
pub(crate) fn map_many(function: i64, iterables: &[i64]) -> i64 {
    if value::as_function(function).is_none() {
        return exc::raise_with_message(TYPE_ERROR, "map() argument 1 must be callable");
    }
    let mut iterators = Vec::with_capacity(iterables.len());
    for &iterable in iterables {
        let iterator = iter::iter_new(iterable);
        if exc::pending() {
            return NONE;
        }
        iterators.push(iterator);
    }
    let result = value::list_new(Vec::new());
    let roots = crate::heap::root_values(&[result]);
    loop {
        let mut arguments = Vec::with_capacity(iterators.len());
        for &iterator in &iterators {
            let mut item = NONE;
            let status = iter::iter_next(iterator, &mut item);
            if exc::pending() {
                crate::heap::release_roots(roots);
                return NONE;
            }
            if status == 0 {
                crate::heap::release_roots(roots);
                return result;
            }
            arguments.push(item);
        }
        let argument_roots = crate::heap::root_values(&arguments);
        let mapped = call(function, arguments.len(), arguments.as_ptr());
        crate::heap::release_roots(argument_roots);
        if exc::pending() {
            crate::heap::release_roots(roots);
            return NONE;
        }
        if let Some(mut items) = value::list_items_mut(result) {
            items.push(mapped);
            crate::heap::write_barrier_edge(result, mapped);
        }
    }
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

// `all` and dict unpacking

/// `any(iterable)`: `True` when at least one element is truthy.
/// An empty iterable is `False`; iteration errors propagate.
pub(crate) fn any(iterable: i64) -> i64 {
    let iterator = iter::iter_new(iterable);
    if exc::pending() {
        return NONE;
    }
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            return value::bool_value(false);
        }
        if value::truthy(item) {
            return value::bool_value(true);
        }
    }
}

/// `all(iterable)`: `True` when every element is truthy, `False` otherwise.
/// An empty iterable is `True`, like CPython.
pub(crate) fn all(iterable: i64) -> i64 {
    let iterator = iter::iter_new(iterable);
    if exc::pending() {
        return NONE;
    }
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            return value::bool_value(true);
        }
        if !value::truthy(item) {
            return value::bool_value(false);
        }
    }
}

/// `{**left, **right}`: a fresh dict with `left`'s entries first, then
/// `right`'s. Duplicate keys keep their first position and take the right
/// value (Python's display and `dict.update` semantics).
///
/// Non-dict operands raise `TypeError`. The result is allocated empty and
/// filled through [`container::dict_insert_barrier`], so every inserted key
/// and value gets its GC edge.
pub(crate) fn dict_merge(left: i64, right: i64) -> i64 {
    let Some(left_pairs) = value::as_dict(left) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "dict unpacking requires a mapping, not '{}'",
                value::type_name(left)
            ),
        );
    };
    let Some(right_pairs) = value::as_dict(right) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "dict unpacking requires a mapping, not '{}'",
                value::type_name(right)
            ),
        );
    };
    // Copy both payloads before allocating: a reference into a heap object
    // must not be read after an allocation that may collect.
    let left_pairs = left_pairs.to_vec();
    let right_pairs = right_pairs.to_vec();
    let result = value::dict_new(Vec::new());
    if let Some(mut pairs) = value::dict_pairs_mut(result) {
        for (key, item) in left_pairs {
            container::dict_insert_barrier(result, &mut pairs, key, item);
        }
        for (key, item) in right_pairs {
            container::dict_insert_barrier(result, &mut pairs, key, item);
        }
    }
    result
}

// `max` / `min` / `round` / `format` / `isinstance`

/// `max(*args)`: the largest candidate, or the largest element of a single
/// iterable. The final argument is always the key slot: `None` or a callable.
///
/// Direct lowering always appends the key slot; calls through a first-class
/// `max` value pass the user arguments unchanged. A trailing argument that is
/// `None` or callable is therefore treated as the key, which covers both
/// shapes.
pub(crate) fn max(argc: usize, argv: *const i64) -> i64 {
    extremum("max", argc, argv, true)
}

/// `min(*args)`: the counterpart of [`max`].
pub(crate) fn min(argc: usize, argv: *const i64) -> i64 {
    extremum("min", argc, argv, false)
}

/// Shared `max`/`min` implementation. Ties keep the first element seen, like
/// CPython.
fn extremum(name: &str, argc: usize, argv: *const i64, want_max: bool) -> i64 {
    if argc > 0 && argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("{name}() received {argc} argument(s) without an argument buffer"),
        );
    }
    if argc > isize::MAX as usize / std::mem::size_of::<i64>() {
        return builtin_arity_error(name, "a supported number of", argc);
    }
    // SAFETY: ABI contract: `argv` points at `argc` readable values; length checked above.
    let args: &[i64] = if argc == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(argv, argc) }
    };

    let keyed =
        argc >= 2 && (args[argc - 1] == NONE || value::as_function(args[argc - 1]).is_some());
    let (candidates, key) = if keyed {
        (&args[..argc - 1], args[argc - 1])
    } else {
        (args, NONE)
    };
    if key != NONE && value::as_function(key).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("'{}' object is not callable", value::type_name(key)),
        );
    }
    if candidates.is_empty() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("{name} expected at least 1 argument, got 0"),
        );
    }
    extremum_values(name, candidates, key, None, want_max)
}

/// Shared `max`/`min` evaluation over already-split candidates.
///
/// `default` is the `default=` keyword (keyword-only, single iterable form);
/// it is returned when the iterable is empty instead of raising `ValueError`.
fn extremum_values(
    name: &str,
    candidates: &[i64],
    key: i64,
    default: Option<i64>,
    want_max: bool,
) -> i64 {
    let mut best = NONE;
    let mut best_key = NONE;
    let mut found = false;

    // One candidate is an iterable; two or more are compared directly.
    let iterable = candidates.len() == 1;
    let mut iterator = NONE;
    if iterable {
        iterator = iter::iter_new(candidates[0]);
        if exc::pending() {
            return NONE;
        }
    }

    let mut index = 0usize;
    loop {
        let mut item = NONE;
        if iterable {
            let status = iter::iter_next(iterator, &mut item);
            if exc::pending() {
                return NONE;
            }
            if status == 0 {
                break;
            }
        } else {
            item = candidates[index];
            index += 1;
        }

        let item_key = if key == NONE {
            item
        } else {
            let produced = call(key, 1, &item);
            if exc::pending() {
                return NONE;
            }
            produced
        };

        if !found {
            best = item;
            best_key = item_key;
            found = true;
        } else {
            let op = if want_max { ops::OP_GT } else { ops::OP_LT };
            let verdict = ops::binop(op, item_key, best_key);
            if exc::pending() {
                return NONE;
            }
            if value::truthy(verdict) {
                best = item;
                best_key = item_key;
            }
        }

        if !iterable && index == candidates.len() {
            break;
        }
    }

    if !found {
        if let Some(default) = default {
            return default;
        }
        return exc::raise_with_message(VALUE_ERROR, &format!("{name}() arg is an empty sequence"));
    }
    best
}

/// Binds `max`/`min` keyword arguments (`key=`, `default=`) for a call
/// through a first-class builtin value (or the frontend's keyword lowering of
/// the single-iterable `default=` form).
///
/// CPython restricts `default` to the single-positional iterable form; the
/// positional arguments are otherwise treated like [`extremum`].
fn extremum_keywords(
    builtin: Builtin,
    argc: usize,
    argv: *const i64,
    kwargc: usize,
    kw_argv: *const i64,
) -> i64 {
    let name = builtin.name();
    if argc > 0 && argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("{name}() received {argc} argument(s) without an argument buffer"),
        );
    }
    let args: Vec<i64> = if argc == 0 {
        Vec::new()
    } else {
        // SAFETY: ABI contract: `argv..argv+argc` is readable; checked above.
        unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec()
    };
    let mut key = NONE;
    let mut key_seen = false;
    let mut default: Option<i64> = None;
    if kwargc > 0 {
        if kw_argv.is_null() || kwargc > isize::MAX as usize / (2 * std::mem::size_of::<i64>()) {
            return exc::raise_with_message(
                TYPE_ERROR,
                "keyword arguments were passed without an argument buffer",
            );
        }
        // SAFETY: ABI contract: `kw_argv` holds `2 * kwargc` readable values.
        let pairs = unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) };
        for pair in pairs.as_chunks::<2>().0 {
            let Some(keyword) = value::as_str(pair[0]) else {
                return exc::raise_with_message(TYPE_ERROR, "keyword name must be str");
            };
            match keyword {
                "key" => {
                    if key_seen {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            &format!("{name}() got multiple values for keyword argument 'key'"),
                        );
                    }
                    key_seen = true;
                    key = pair[1];
                }
                "default" => {
                    if default.is_some() {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            &format!("{name}() got multiple values for keyword argument 'default'"),
                        );
                    }
                    default = Some(pair[1]);
                }
                _ => {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!("{name}() got an unexpected keyword argument '{keyword}'"),
                    )
                }
            }
        }
    }
    if default.is_some() && args.len() > 1 {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("Cannot specify a default for {name}() with multiple positional arguments"),
        );
    }
    if args.is_empty() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("{name} expected at least 1 argument, got 0"),
        );
    }
    extremum_values(name, &args, key, default, builtin == Builtin::Max)
}

/// `reversed(iterable)`: materialise and reverse a sequence (the rich/stdlib
/// surface only needs a list result). Non-sequences are iterated to a list.
fn reversed(value: i64) -> i64 {
    let mut items: Vec<i64> = if let Some(items) = value::as_list(value) {
        items.to_vec()
    } else if let Some(items) = value::as_tuple(value) {
        items.to_vec()
    } else if let Some(text) = value::as_str(value) {
        text.chars()
            .map(|ch| value::str_alloc(ch.to_string()))
            .collect()
    } else if let Some(bytes) = value::as_bytes(value) {
        bytes
            .iter()
            .map(|byte| ops::int_result(i64::from(*byte)))
            .collect()
    } else if let Some((start, stop, step)) = value::as_range(value) {
        let mut items = Vec::new();
        let mut current = start;
        if step > 0 {
            while current < stop {
                items.push(ops::int_result(current));
                current = match current.checked_add(step) {
                    Some(next) => next,
                    None => break,
                };
            }
        } else if step < 0 {
            while current > stop {
                items.push(ops::int_result(current));
                current = match current.checked_add(step) {
                    Some(next) => next,
                    None => break,
                };
            }
        }
        items
    } else {
        let iterator = crate::iter::iter_new(value);
        if exc::pending() {
            return iterator;
        }
        let mut items = Vec::new();
        loop {
            let mut item = NONE;
            if crate::iter::iter_next(iterator, &mut item) == 0 {
                break;
            }
            if exc::pending() {
                return item;
            }
            items.push(item);
        }
        items
    };
    items.reverse();
    value::list_new(items)
}

/// `sorted(iterable, key=None, reverse=None)`: a new list with the elements
/// in ascending order, or descending when `reverse` is truthy.
///
/// Direct lowering always supplies the `key` and `reverse` slots (`None`
/// when omitted); a first-class `sorted` value forwards the user arguments,
/// so one to three arguments are accepted. Ordering uses Python's `<` and the
/// sort is stable, so equal keys keep their source order in either direction.
/// Keys are evaluated once per element before any comparison.
pub(crate) fn sorted(argc: usize, argv: *const i64) -> i64 {
    if argc > 0 && argv.is_null() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("sorted() received {argc} argument(s) without an argument buffer"),
        );
    }
    if argc > isize::MAX as usize / std::mem::size_of::<i64>() {
        return builtin_arity_error("sorted", "a supported number of", argc);
    }
    // SAFETY: ABI contract: `argv` points at `argc` readable values; length checked above.
    let args: &[i64] = if argc == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(argv, argc) }
    };
    let (iterable, key, reverse) = match args {
        [iterable] => (*iterable, NONE, NONE),
        [iterable, key] => (*iterable, *key, NONE),
        [iterable, key, reverse] => (*iterable, *key, *reverse),
        _ => return builtin_arity_error("sorted", "1 to 3", argc),
    };
    if key != NONE
        && value::as_function(key).is_none()
        && value::as_class(key).is_none()
        && value::as_ext_function(key).is_none()
        && !modules::extension::ext_object_is_callable(key)
    {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("'{}' object is not callable", value::type_name(key)),
        );
    }

    // Materialise the source into a rooted list: the key calls below and the
    // comparisons allocate, so every element must stay reachable.
    let items = value::list_new(Vec::new());
    let iterator = iter::iter_new(iterable);
    if exc::pending() {
        return NONE;
    }
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        if let Some(mut stored) = value::list_items_mut(items) {
            stored.push(item);
            crate::heap::write_barrier_edge(items, item);
        }
    }
    let count = value::as_list(items).map_or(0, |stored| stored.len());

    // Evaluate the key exactly once per element, in source order.
    let keys = if key == NONE {
        items
    } else {
        let keys = value::list_new(Vec::new());
        for index in 0..count {
            let item = value::as_list(items).and_then(|stored| stored.get(index).copied());
            let Some(item) = item else {
                break;
            };
            let produced = call(key, 1, &item);
            if exc::pending() {
                return NONE;
            }
            if let Some(mut stored) = value::list_items_mut(keys) {
                stored.push(produced);
                crate::heap::write_barrier_edge(keys, produced);
            }
        }
        keys
    };

    let descending = value::truthy(reverse);
    let order = match merge_sort_indices((0..count).collect(), keys, descending) {
        Ok(order) => order,
        Err(error) => return error,
    };

    let result = value::list_new(Vec::new());
    for index in order {
        let item = value::as_list(items).and_then(|stored| stored.get(index).copied());
        let Some(item) = item else {
            continue;
        };
        if let Some(mut stored) = value::list_items_mut(result) {
            stored.push(item);
            crate::heap::write_barrier_edge(result, item);
        }
    }
    result
}

/// Reads one key from the rooted keys list.
fn sort_key(keys: i64, index: usize) -> i64 {
    value::as_list(keys)
        .and_then(|stored| stored.get(index).copied())
        .unwrap_or(NONE)
}

/// Stable merge sort of element indices ordered by their keys.
///
/// The keys stay rooted in `keys`, so comparisons may allocate freely. A
/// pending comparison error stops the sort and returns the pending value.
fn merge_sort_indices(
    mut indices: Vec<usize>,
    keys: i64,
    descending: bool,
) -> Result<Vec<usize>, i64> {
    let len = indices.len();
    if len <= 1 {
        return Ok(indices);
    }
    let right_half = indices.split_off(len / 2);
    let left = merge_sort_indices(indices, keys, descending)?;
    let right = merge_sort_indices(right_half, keys, descending)?;
    let mut merged = Vec::with_capacity(len);
    let (mut i, mut j) = (0usize, 0usize);
    while i < left.len() && j < right.len() {
        // Only a strict `<` moves the right element ahead of the left one, so
        // equal keys keep their source order in both directions.
        let (a, b) = if descending {
            (sort_key(keys, left[i]), sort_key(keys, right[j]))
        } else {
            (sort_key(keys, right[j]), sort_key(keys, left[i]))
        };
        let verdict = ops::binop(ops::OP_LT, a, b);
        if exc::pending() {
            return Err(verdict);
        }
        if value::truthy(verdict) {
            merged.push(right[j]);
            j += 1;
        } else {
            merged.push(left[i]);
            i += 1;
        }
    }
    merged.extend_from_slice(&left[i..]);
    merged.extend_from_slice(&right[j..]);
    Ok(merged)
}

/// `round(value[, ndigits])`. `ndigits` is `NONE` for the one-argument form.
pub(crate) fn round_value(value: i64, ndigits: i64) -> i64 {
    let digits = if ndigits == NONE {
        None
    } else {
        match value::as_integer(ndigits) {
            Some(digits) => Some(digits),
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "'{}' object cannot be interpreted as an integer",
                        value::type_name(ndigits)
                    ),
                );
            }
        }
    };

    if let Some(n) = value::as_integer(value) {
        return match digits {
            None => ops::int_result(n),
            Some(digits) if digits >= 0 => ops::int_result(n),
            Some(digits) => round_int_digits(n, digits),
        };
    }
    if let Some(f) = value::as_float(value) {
        return round_float(f, digits);
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "type {} doesn't define __round__ method",
            value::type_name(value)
        ),
    )
}

/// Rounds an integer to a negative number of decimal digits with CPython's
/// round-half-to-even rule.
fn round_int_digits(n: i64, digits: i64) -> i64 {
    // Every tagged integer is far below 5 * 10^18, so 19 places already
    // rounds to zero.
    if digits.unsigned_abs() >= 19 {
        return ops::int_result(0);
    }
    let place = 10i128.pow(digits.unsigned_abs() as u32);
    let value = n as i128;
    let quotient = value / place;
    let remainder = value % place;
    let twice = remainder.abs() * 2;
    let round_up = twice > place || (twice == place && quotient % 2 != 0);
    let rounded = if round_up {
        quotient + if value < 0 { -1 } else { 1 }
    } else {
        quotient
    };
    ops::int_result((rounded * place) as i64)
}

/// Rounds a float. Without `ndigits` the result is an `int` (ties to even);
/// with `ndigits` it stays a float. The fixed-precision formatter rounds the
/// exact decimal expansion half-to-even, so `round(2.675, 2) == 2.67`.
fn round_float(x: f64, digits: Option<i64>) -> i64 {
    let Some(digits) = digits else {
        if x.is_nan() {
            return exc::raise_with_message(VALUE_ERROR, "cannot convert float NaN to integer");
        }
        if x.is_infinite() {
            return exc::raise_with_message(
                VALUE_ERROR,
                "cannot convert float infinity to integer",
            );
        }
        if !(-TWO_POW_63..TWO_POW_63).contains(&x) {
            return exc::raise_with_message(OVERFLOW_ERROR, "integer overflow");
        }
        return ops::int_result(x.round_ties_even() as i64);
    };

    if x.is_nan() || x.is_infinite() {
        return value::float_new(x);
    }
    if digits > 323 {
        return value::float_new(x);
    }
    if digits < -323 {
        // Preserve the sign of zero, as CPython does.
        return value::float_new(x * 0.0);
    }
    let rounded = if digits >= 0 {
        format!("{:.*}", digits as usize, x)
            .parse::<f64>()
            .unwrap_or(x)
    } else {
        let factor = 10f64.powi(-(digits as i32));
        (x / factor).round_ties_even() * factor
    };
    value::float_new(rounded)
}

/// `format(value, spec)`: Python's built-in formatting with no conversion.
/// `conversion` is the f-string flag (`0` none, `1` `!s`, `2` `!r`, `3` `!a`).
pub(crate) fn format_builtin(value: i64, spec: i64, conversion: u8) -> i64 {
    // A native scalar subclass instance formats as the wrapped builtin value.
    let value = value::scalar_payload(value).unwrap_or(value);
    let Some(spec_text) = value::as_str(spec) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "format() argument 2 must be str, not '{}'",
                value::type_name(spec)
            ),
        );
    };
    match format::format_value(value, spec_text, conversion) {
        Ok(text) => value::str_alloc(text),
        Err(error) => exc::raise_with_message(error.tag, &error.message),
    }
}

/// `isinstance(value, target)`: `target` is a native type (an `int`, `float`,
/// `str`, ... builtin value), `None` for `NoneType`, a class, or a tuple of
/// those. Anything else is a `TypeError`.
///
/// Built-in exception classes participate in the CPython hierarchy:
/// `isinstance(ValueError(), Exception)` and
/// `isinstance(FileNotFoundError(), OSError)` are true, and every Hydro value
/// (including exception objects) is an instance of the cached `object` class.
pub(crate) fn isinstance(value: i64, target: i64) -> i64 {
    match type_check(value, target) {
        Some(result) => value::bool_value(result),
        None => exc::raise_with_message(
            TYPE_ERROR,
            "isinstance() arg 2 must be a type or tuple of types",
        ),
    }
}

/// `issubclass(subclass, class)`: both arguments name classes (or `class` is
/// a tuple of classes).
///
/// The check follows the CPython hierarchy for exception classes
/// (`issubclass(SyntaxError, Exception)`), walks the base chain of
/// user-defined classes, and treats every class as a subclass of the cached
/// `object` class. Non-class arguments raise `TypeError`, matching CPython.
pub(crate) fn issubclass(subclass: i64, class: i64) -> i64 {
    match subclass_check(subclass, class) {
        Ok(result) => value::bool_value(result),
        Err(SubclassError::Subclass) => {
            exc::raise_with_message(TYPE_ERROR, "issubclass() arg 1 must be a class")
        }
        Err(SubclassError::Class) => exc::raise_with_message(
            TYPE_ERROR,
            "issubclass() arg 2 must be a class or tuple of classes",
        ),
    }
}

/// Which `issubclass` argument was not a class.
enum SubclassError {
    Subclass,
    Class,
}

/// Tri-state check for `issubclass`, with the offending argument on failure.
fn subclass_check(subclass: i64, class: i64) -> Result<bool, SubclassError> {
    // The builtin type spellings (`int`, `list`, `type`, ...) are function
    // values that also name the cached class of the same type; both spellings
    // are class operands here.
    let subclass = value::builtin_type_class(subclass).unwrap_or(subclass);
    let class = value::builtin_type_class(class).unwrap_or(class);
    if let Some(types) = value::as_tuple(class) {
        if value::as_class(subclass).is_none() {
            return Err(SubclassError::Subclass);
        }
        for &candidate in types {
            if value::as_class(candidate).is_none()
                && value::builtin_type_class(candidate).is_none()
            {
                return Err(SubclassError::Class);
            }
            if subclass_check(subclass, candidate)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if value::as_class(subclass).is_none() {
        // Hydro spells native types (`bytes`, `memoryview`, …) as builtin
        // function values rather than class objects. `Buffer` accepts those
        // spellings, matching `issubclass(bytes, Buffer)` in CPython.
        if value::as_class(class).is_some()
            && crate::modules::collections_abc::is_buffer_class(class)
            && crate::modules::collections_abc::is_native_buffer_type(subclass)
        {
            return Ok(true);
        }
        return Err(SubclassError::Subclass);
    }
    if value::is_object_class(class) {
        // Every class is a subclass of `object`.
        return Ok(true);
    }
    if value::as_class(class).is_none() {
        return Err(SubclassError::Class);
    }
    if subclass == class {
        return Ok(true);
    }
    // Exception classes: the tag hierarchy knows about the classes without a
    // dedicated class object for every parent edge (and models
    // `ExceptionGroup`'s double base).
    if let (Some(sub_tag), Some(class_tag)) = (
        exc::exception_tag_of_class(subclass),
        exc::exception_tag_of_class(class),
    ) {
        return Ok(exc::tag_is_subclass(sub_tag, class_tag));
    }
    // User classes: follow the C3 linearization, then check every native
    // exception ancestor for the tag relationship (so `class E(ValueError)` is
    // a subclass of `Exception`).
    if value::class_mro(subclass).contains(&class) {
        return Ok(true);
    }
    // `collections.abc.Buffer`'s structural hook: a class declaring
    // `__buffer__` (or a native exporter) is a subclass without an explicit
    // edge, matching CPython's `__subclasshook__`. A registered virtual
    // subclass that declares no `__buffer__` still falls through below.
    if crate::modules::collections_abc::is_buffer_class(class)
        && crate::modules::collections_abc::buffer_subclass_supported(subclass)
    {
        return Ok(true);
    }
    // Concrete classes registered through `ABCMeta.register` are virtual
    // subclasses here (the registry is empty unless an ABC was used).
    if crate::modules::abc::is_virtual_subclass(subclass, class) {
        return Ok(true);
    }
    if let Some(class_tag) = exc::exception_tag_of_class(class) {
        if let Some(sub_tag) = value::exception_tag_of_derived_class(subclass) {
            return Ok(exc::tag_is_subclass(sub_tag, class_tag));
        }
    }
    Ok(false)
}

/// Tri-state check: `Some(true/false)` for a recognized type object, `None`
/// when `target` is not a type. Tuples are checked element by element.
fn type_check(value: i64, target: i64) -> Option<bool> {
    if let Some(types) = value::as_tuple(target) {
        for &candidate in types {
            match type_check(value, candidate) {
                Some(true) => return Some(true),
                Some(false) => {}
                None => return None,
            }
        }
        return Some(false);
    }
    // `None` itself names `NoneType`.
    if target == NONE {
        return Some(value == NONE);
    }
    // An extension type object participates through the extension's own
    // type hierarchy (`PyObject_IsInstance`).
    if value::as_ext_object(target).is_some() {
        let result = modules::extension::ext_object_isinstance(value, target);
        if exc::pending() {
            return Some(false);
        }
        return Some(result == value::bool_value(true));
    }
    if value::as_class(target).is_some() {
        return Some(instance_of(value, target));
    }
    if let Some(function) = value::as_function(target) {
        match &function.body {
            FunctionBody::Builtin(builtin) => {
                if *builtin == Builtin::Tuple {
                    return Some(instance_of(
                        value,
                        value::type_class(value::tuple_new(Vec::new())),
                    ));
                }
                return named_type_check(value, builtin.name());
            }
            // `types.FunctionType` and `types.UnionType` are declared as
            // module functions so `types.FunctionType(...)` compiles; the
            // module value is therefore the native function object, not the
            // class. Resolve it to the same membership test the class form
            // uses.
            FunctionBody::Native {
                module: "types",
                name,
            } => return crate::modules::types::type_check_target(value, name),
            _ => {}
        }
    }
    None
}

/// Whether `class` ultimately derives from the runtime `typing.Protocol`
/// placeholder. Such classes use structural checks in `isinstance`.
fn is_protocol_class(class: i64) -> bool {
    let mut current = class;
    for _ in 0..64 {
        let Some(state) = value::as_class(current) else {
            return false;
        };
        if state.name == "Protocol" {
            return true;
        }
        if state.base == NONE {
            return false;
        }
        current = state.base;
    }
    false
}

/// Structural protocol check: every method declared by the protocol must be
/// present on the value. Native scalar values never satisfy a rich protocol.
fn protocol_instance_of(value: i64, class: i64) -> bool {
    let Some(protocol) = value::as_class(class) else {
        return false;
    };
    if protocol.methods.is_empty() {
        return true;
    }
    let Some(instance) = value::as_instance(value) else {
        return false;
    };
    protocol.methods.iter().all(|(name, _, _)| {
        value::class_lookup(instance.class, name).is_some()
            || value::instance_attr(value, name).is_some()
    })
}

/// Matches `value` against a native type name (`int`, `bool`, `str`, ...).
fn named_type_check(value: i64, name: &str) -> Option<bool> {
    Some(match name {
        // `bool` is a subclass of `int` in Python. A native scalar subclass
        // instance is checked through its payload (`class N(int)`), so
        // `isinstance(N(3), int)` holds for the builtin spelling too.
        "int" => value::as_integer(value).is_some() || value::is_bigint(value),
        "bool" => {
            value::type_name(value) == "bool"
                || value::scalar_payload(value)
                    .is_some_and(|payload| payload == value::TRUE || payload == value::FALSE)
        }
        "float" => value::as_float(value).is_some(),
        "str" => value::as_str(value).is_some(),
        // `bytearray` is bytes-like but not a `bytes` instance.
        "bytes" => value::as_bytearray(value).is_none() && value::as_bytes(value).is_some(),
        "bytearray" => value::as_bytearray(value).is_some(),
        "memoryview" => value::as_memoryview(value).is_some(),
        "dict_keys" | "dict_values" | "dict_items" => value::as_dict_view(value)
            .map(|view| view.kind.type_name() == name)
            .unwrap_or(false),
        "list" => value::as_list(value).is_some(),
        "dict" => {
            // A native `dict` subclass instance is a dict too; `mapping_pairs`
            // resolves the real mapping storage (and a plain dict or a
            // defaultdict) without creating anything.
            value::mapping_pairs(value).is_some()
                || crate::modules::collections::is_counter_instance(value)
        }
        "tuple" => {
            value::as_tuple(value).is_some()
                || crate::modules::collections::is_namedtuple_instance(value)
        }
        "set" | "frozenset" => value::as_set(value).is_some(),
        "NoneType" | "None" => value == NONE,
        // `re.Pattern`/`re.Match` are the cached classes `type()` reports for
        // compiled patterns and match objects, so `isinstance(p, re.Pattern)`
        // and `isinstance(m, re.Match)` hold without a class instance.
        "Pattern" => value::as_pattern(value).is_some(),
        "Match" => value::as_match(value).is_some(),
        // `socket.SocketType` is the cached class `type()` reports for a
        // socket object, so `isinstance(s, socket.SocketType)` holds.
        "socket" => value::as_socket(value).is_some(),
        // `types.FunctionType`: every function value (`def`, lambda,
        // builtin/bound/native) reports the cached `function` type name, but a
        // `functools.partial` is an instance of `functools.partial`.
        "function" => value::as_function(value)
            .is_some_and(|function| !matches!(&function.body, FunctionBody::Partial { .. })),
        // `functools.partial` is stored as a function value; its cached class
        // is the value the module name resolves to, so both the class and the
        // `type(p)` name reach this arm.
        "partial" => value::as_function(value)
            .is_some_and(|function| matches!(&function.body, FunctionBody::Partial { .. })),
        // Native descriptor/module/iterator classes are spellings of builtin
        // function values rather than class objects; their runtime type name
        // is still the CPython class name.
        "staticmethod" | "classmethod" | "property" | "module" | "method" | "generator"
        | "frame" | "code" => value::type_name(value) == name,
        "object" => true,
        // A class, or the builtin type spelling source code names it by, is an
        // instance of `type`.
        "type" => value::as_class(value).is_some() || value::builtin_type_class(value).is_some(),
        _ => return None,
    })
}

/// Whether `value` is an instance of `class` (or one of its subclasses).
///
/// Besides the base chain of class instances this resolves the two runtime
/// class families:
/// - built-in exception classes use the tag hierarchy, so
///   `isinstance(ValueError('x'), Exception)` holds without materialising a
///   class object per parent edge;
/// - the cached native classes created by `type()` for primitives match their
///   value kind (`isinstance(3, type(3))`), `object` matches everything and
///   `type` matches every class value.
fn instance_of(value: i64, class: i64) -> bool {
    if value::is_object_class(class) {
        return true;
    }
    // Instances (and native scalars registered through their builtin spelling)
    // of a class recorded with `ABCMeta.register` are virtual instances. The
    // registry is empty unless an ABC was used, so this is one atomic load in
    // the common case.
    if crate::modules::abc::is_virtual_instance(value, class) {
        return true;
    }
    // `collections.abc.Buffer`'s structural hook: native buffer exporters and
    // instances whose class declares `__buffer__` match without an edge.
    if crate::modules::collections_abc::is_buffer_class(class)
        && crate::modules::collections_abc::buffer_instance_supported(value)
    {
        return true;
    }
    // Classes are instances of their metaclass (the cached `type` class when no
    // explicit metaclass was given), so `isinstance(C, Meta)` works for a
    // `type` subclass.
    if value::as_class(value).is_some() {
        let metaclass = value::type_class(value);
        if metaclass == class {
            return true;
        }
        if value::as_class(class).is_some() && value::is_subclass_of(metaclass, class) {
            return true;
        }
    }
    if value::is_type_class(class) {
        return value::as_class(value).is_some() || value::builtin_type_class(value).is_some();
    }
    // A `functools.partial` value is stored as a function object, but its
    // type is the cached `partial` class (see `value::type_class`).
    if value::is_partial_class(class) {
        return value::as_function(value)
            .is_some_and(|function| matches!(&function.body, FunctionBody::Partial { .. }));
    }
    if is_protocol_class(class) {
        return protocol_instance_of(value, class);
    }
    if value::class_display_name(class) == Some("tuple")
        && crate::modules::collections::is_namedtuple_instance(value)
    {
        return true;
    }
    // A user-defined exception instance keeps its Python class identity; its
    // native payload adds the built-in exception ancestry.
    if let Some(exception) = value::as_exc_instance(value) {
        if value::class_mro(exception.class).contains(&class) {
            return true;
        }
        return match exc::exception_tag_of_class(class) {
            Some(handler_tag) => exc::tag_is_subclass(exception.data.tag, handler_tag),
            None => false,
        };
    }
    // Built-in exception objects carry their tag, not a class instance.
    if let Some(data) = value::exception_data(value) {
        return match exc::exception_tag_of_class(class) {
            Some(handler_tag) => exc::tag_is_subclass(data.tag, handler_tag),
            None => false,
        };
    }
    if let Some(instance) = value::as_instance(value) {
        if value::class_mro(instance.class).contains(&class) {
            return true;
        }
        // A user class derived from a native exception class inherits its
        // whole CPython ancestry (`class E(ValueError)` is an `Exception`).
        if let Some(class_tag) = exc::exception_tag_of_class(class) {
            if let Some(current_tag) = value::exception_tag_of_derived_class(instance.class) {
                return exc::tag_is_subclass(current_tag, class_tag);
            }
        }
        return false;
    }
    // File objects are native streams, not class instances. Their class
    // identity follows the stream state (raw/buffered/text), so `isinstance`
    // walks the real `io` hierarchy instead of guessing from the mode string.
    if let Some(file) = value::as_file(value) {
        let actual = crate::modules::io_file_class(file.class());
        return value::class_mro(actual).contains(&class);
    }
    // An opaque extension handle is an instance of every Hydro class whose
    // base chain contains the extension type it was constructed from.
    if value::is_ext_object(value) && modules::extension::ext_object_is_hydro_subclass(value, class)
    {
        return true;
    }
    // Non-instance values match only the cached native primitive classes
    // (`type(x)`), which `named_type_check` resolves by name.
    if value::is_cached_type_class(class) {
        if let Some(name) = value::class_display_name(class) {
            if let Some(result) = named_type_check(value, name) {
                return result;
            }
        }
    }
    false
}

// -------------------------------------------------------------------------
// `complex` and `slice` values
// -------------------------------------------------------------------------
//
// Hydro has no dedicated complex/slice object kinds, so both are native
// instances of cached classes. That keeps `type(x)`, `isinstance(x, ...)` and
// class-identity comparisons working, and the classes are callable through the
// ordinary class path (routed below, in [`instantiate`]).

/// Allocates one native method value. `entry` receives the bound arguments
/// (`self` first) and its exact arity is checked by [`call`].
fn native_method(entry: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
    value::function_new(entry as *const () as usize, arity, name.to_string())
}

/// The cached `complex` class used by `type(complex value)` and
/// `isinstance(x, complex)`.
pub(crate) fn complex_class() -> i64 {
    if let Some(class) = globals::registry_get("builtins.complex") {
        return class;
    }
    let class = value::class_new(
        "complex".to_string(),
        NONE,
        vec![
            (
                "__repr__".to_string(),
                native_method(complex_repr_method, 1, "__repr__"),
            ),
            (
                "conjugate".to_string(),
                native_method(complex_conjugate, 1, "conjugate"),
            ),
            (
                "__eq__".to_string(),
                native_method(complex_eq_method, 2, "__eq__"),
            ),
        ],
    );
    globals::registry_set("builtins.complex", class);
    class
}

/// The cached `slice` class.
pub(crate) fn slice_class() -> i64 {
    if let Some(class) = globals::registry_get("builtins.slice") {
        return class;
    }
    let class = value::class_new(
        "slice".to_string(),
        NONE,
        vec![
            (
                "__repr__".to_string(),
                native_method(slice_repr_method, 1, "__repr__"),
            ),
            (
                "indices".to_string(),
                native_method(slice_indices_method, 2, "indices"),
            ),
            (
                "__eq__".to_string(),
                native_method(slice_eq_method, 2, "__eq__"),
            ),
        ],
    );
    globals::registry_set("builtins.slice", class);
    class
}

pub(crate) fn is_complex_instance(v: i64) -> bool {
    // The ordinary numeric path must not build a registry key and take the
    // globals RwLock for a tagged int. Only real instances can match.
    let Some(class) = value::instance_class_value(v) else {
        return false;
    };
    globals::registry_get("builtins.complex") == Some(class)
}

fn is_slice_instance(v: i64) -> bool {
    let Some(class) = value::instance_class_value(v) else {
        return false;
    };
    globals::registry_get("builtins.slice") == Some(class)
}

/// Numeric view of `v` for complex construction (`bool`, `int`, `bigint` and
/// `float`).
fn numeric_f64(v: i64) -> Option<f64> {
    if let Some(f) = value::as_float(v) {
        return Some(f);
    }
    if let Some(i) = value::as_integer(v) {
        return Some(i as f64);
    }
    value::as_bigint(v).and_then(value::bigint_to_f64)
}

/// `(real, imag)` of a complex value or any numeric value.
pub(crate) fn complex_components(v: i64) -> Option<(f64, f64)> {
    if is_complex_instance(v) {
        let real = value::instance_attr(v, "real").and_then(value::as_float);
        let imag = value::instance_attr(v, "imag").and_then(value::as_float);
        return Some((real.unwrap_or(0.0), imag.unwrap_or(0.0)));
    }
    numeric_f64(v).map(|real| (real, 0.0))
}

/// Allocates one complex value carrying `real`/`imag` attributes.
pub(crate) fn build_complex(real: f64, imag: f64) -> i64 {
    let class = complex_class();
    let instance = value::instance_new(class);
    let real_value = value::float_new(real);
    let imag_value = value::float_new(imag);
    let roots = crate::heap::root_values(&[instance, real_value, imag_value]);
    value::instance_setattr(instance, "real".to_string(), real_value);
    value::instance_setattr(instance, "imag".to_string(), imag_value);
    crate::heap::release_roots(roots);
    instance
}

/// Parses the imaginary suffix of a complex literal.
fn parse_imaginary_part(text: &str) -> Option<f64> {
    match text {
        "" | "+" => Some(1.0),
        "-" => Some(-1.0),
        _ => text.parse::<f64>().ok(),
    }
}

/// Parses CPython's `complex(str)` grammar: an optional surrounding pair of
/// parentheses, then either a real number or `<real><sign><imag>j`. Spaces
/// inside the expression are rejected, like CPython.
fn parse_complex_text(text: &str) -> Option<(f64, f64)> {
    let trimmed = text.trim();
    let body = if trimmed.starts_with('(') && trimmed.ends_with(')') {
        trimmed[1..trimmed.len() - 1].trim()
    } else {
        trimmed
    };
    if body.is_empty() {
        return None;
    }
    let bytes = body.as_bytes();
    if bytes
        .last()
        .is_some_and(|last| *last == b'j' || *last == b'J')
    {
        let stem = &body[..body.len() - 1];
        // Locate the last top-level sign, skipping exponent signs (`1e-3j`).
        let mut split = None;
        let mut previous = '\0';
        for (index, ch) in stem.char_indices() {
            if (ch == '+' || ch == '-') && index > 0 && previous != 'e' && previous != 'E' {
                split = Some((index, ch));
            }
            previous = ch;
        }
        let (real, imag) = match split {
            Some((index, sign)) => {
                let real = stem[..index].parse::<f64>().ok()?;
                let sign_text = if sign == '+' { "+" } else { "-" };
                let rest = &stem[index + 1..];
                (real, format!("{sign_text}{rest}").parse::<f64>().ok()?)
            }
            None => (0.0, parse_imaginary_part(stem)?),
        };
        return Some((real, imag));
    }
    body.parse::<f64>().ok().map(|real| (real, 0.0))
}

/// `complex(real=0, imag=0)` / `complex(str)`.
pub(crate) fn complex_new(args: &[i64]) -> i64 {
    match args {
        [] => build_complex(0.0, 0.0),
        [single] => {
            if let Some(text) = value::as_str(*single) {
                return match parse_complex_text(text) {
                    Some((real, imag)) => build_complex(real, imag),
                    None => {
                        exc::raise_with_message(VALUE_ERROR, "complex() arg is a malformed string")
                    }
                };
            }
            if is_complex_instance(*single) {
                let (real, imag) = complex_components(*single).unwrap_or((0.0, 0.0));
                return build_complex(real, imag);
            }
            match numeric_f64(*single) {
                Some(real) => build_complex(real, 0.0),
                None => exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "complex() first argument must be a string or a number, not '{}'",
                        value::type_name_of(*single)
                    ),
                ),
            }
        }
        [real, imag] => {
            let Some(real) = numeric_f64(*real) else {
                return complex_argument_error(*real, "real");
            };
            let Some(imag) = numeric_f64(*imag) else {
                return complex_argument_error(*imag, "imag");
            };
            build_complex(real, imag)
        }
        _ => exc::raise_with_message(TYPE_ERROR, "complex() takes at most 2 arguments"),
    }
}

fn complex_argument_error(v: i64, which: &str) -> i64 {
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "complex() {which} part must be a number, not '{}'",
            value::type_name_of(v)
        ),
    )
}

/// Formats one component the way `complex.__repr__` does (integral values
/// lose the trailing `.0`).
fn complex_part(v: f64) -> String {
    let text = value::format_float(v);
    if v.is_finite() && v.fract() == 0.0 {
        text.strip_suffix(".0").map(str::to_string).unwrap_or(text)
    } else {
        text
    }
}

fn complex_repr_text(real: f64, imag: f64) -> String {
    let imag_text = complex_part(imag.abs());
    if real == 0.0 && !real.is_sign_negative() {
        return format!(
            "{}{}j",
            if imag.is_sign_negative() { "-" } else { "" },
            imag_text
        );
    }
    format!(
        "({}{}{}j)",
        complex_part(real),
        if imag.is_sign_negative() { "-" } else { "+" },
        imag_text
    )
}

extern "C" fn complex_repr_method(argv: *const i64) -> i64 {
    // SAFETY: bound method call passes `self` first.
    let this = unsafe { *argv };
    let (real, imag) = complex_components(this).unwrap_or((0.0, 0.0));
    value::str_alloc(complex_repr_text(real, imag))
}

extern "C" fn complex_conjugate(argv: *const i64) -> i64 {
    // SAFETY: bound method call passes `self` first.
    let this = unsafe { *argv };
    let (real, imag) = complex_components(this).unwrap_or((0.0, 0.0));
    build_complex(real, -imag)
}

extern "C" fn complex_eq_method(argv: *const i64) -> i64 {
    // SAFETY: `__eq__` receives `self` and the other operand.
    let left = unsafe { *argv };
    let right = unsafe { *argv.add(1) };
    match (complex_components(left), complex_components(right)) {
        (Some((lr, li)), Some((rr, ri))) => value::bool_value(lr == rr && li == ri),
        _ => value::NOT_IMPLEMENTED,
    }
}

/// Allocates one slice value. Missing bounds are stored as `None`, matching
/// CPython's attribute values.
fn build_slice(start: i64, stop: i64, step: i64) -> i64 {
    let class = slice_class();
    let instance = value::instance_new(class);
    let roots = crate::heap::root_values(&[instance, start, stop, step]);
    value::instance_setattr(instance, "start".to_string(), start);
    value::instance_setattr(instance, "stop".to_string(), stop);
    value::instance_setattr(instance, "step".to_string(), step);
    crate::heap::release_roots(roots);
    instance
}

/// `slice(stop)` / `slice(start, stop[, step])`.
pub(crate) fn slice_new(args: &[i64]) -> i64 {
    match args {
        [stop] => build_slice(NONE, *stop, NONE),
        [start, stop] => build_slice(*start, *stop, NONE),
        [start, stop, step] => build_slice(*start, *stop, *step),
        [] => exc::raise_with_message(TYPE_ERROR, "slice expected at least 1 argument, got 0"),
        _ => exc::raise_with_message(TYPE_ERROR, "slice() takes at most 3 arguments"),
    }
}

extern "C" fn slice_repr_method(argv: *const i64) -> i64 {
    // SAFETY: bound method call passes `self` first.
    let this = unsafe { *argv };
    let start = value::instance_attr(this, "start").unwrap_or(NONE);
    let stop = value::instance_attr(this, "stop").unwrap_or(NONE);
    let step = value::instance_attr(this, "step").unwrap_or(NONE);
    let roots = crate::heap::root_values(&[start, stop, step]);
    let rendered = format!(
        "slice({}, {}, {})",
        value::repr(start),
        value::repr(stop),
        value::repr(step)
    );
    crate::heap::release_roots(roots);
    value::str_alloc(rendered)
}

/// One `slice.indices` operand: `None` stays unset, integers are decoded,
/// everything else raises the CPython `TypeError`.
fn slice_bound_argument(v: i64) -> Result<Option<i64>, i64> {
    if v == NONE {
        return Ok(None);
    }
    match value::as_integer(v) {
        Some(n) => Ok(Some(n)),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            "slice indices must be integers or None or have an __index__ method",
        )),
    }
}

extern "C" fn slice_indices_method(argv: *const i64) -> i64 {
    // SAFETY: bound method call passes `self` and `len`.
    let this = unsafe { *argv };
    let length_value = unsafe { *argv.add(1) };
    let Some(length) = value::as_integer(length_value) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "'{}' object cannot be interpreted as an integer",
                value::type_name_of(length_value)
            ),
        );
    };
    let start = match slice_bound_argument(value::instance_attr(this, "start").unwrap_or(NONE)) {
        Ok(bound) => bound,
        Err(error) => return error,
    };
    let stop = match slice_bound_argument(value::instance_attr(this, "stop").unwrap_or(NONE)) {
        Ok(bound) => bound,
        Err(error) => return error,
    };
    let step = match slice_bound_argument(value::instance_attr(this, "step").unwrap_or(NONE)) {
        Ok(bound) => bound,
        Err(error) => return error,
    };
    let step = step.unwrap_or(1) as i128;
    if step == 0 {
        return exc::raise_with_message(VALUE_ERROR, "slice step cannot be zero");
    }
    let len = length as i128;
    let start = match start {
        None => {
            if step < 0 {
                len - 1
            } else {
                0
            }
        }
        Some(start) => {
            let start = start as i128;
            if start < 0 {
                (start + len).max(if step < 0 { -1 } else { 0 })
            } else {
                start.min(if step < 0 { len - 1 } else { len })
            }
        }
    };
    let stop = match stop {
        None => {
            if step < 0 {
                -1
            } else {
                len
            }
        }
        Some(stop) => {
            let stop = stop as i128;
            if stop < 0 {
                (stop + len).max(-1)
            } else {
                stop.min(len)
            }
        }
    };
    let clamp = |n: i128| i64::try_from(n).unwrap_or(if n < 0 { i64::MIN } else { i64::MAX });
    let values = [
        ops::int_result(clamp(start)),
        ops::int_result(clamp(stop)),
        ops::int_result(clamp(step)),
    ];
    value::tuple_new(values.to_vec())
}

extern "C" fn slice_eq_method(argv: *const i64) -> i64 {
    // SAFETY: `__eq__` receives `self` and the other operand.
    let left = unsafe { *argv };
    let right = unsafe { *argv.add(1) };
    if !is_slice_instance(right) {
        return value::NOT_IMPLEMENTED;
    }
    for field in ["start", "stop", "step"] {
        let l = value::instance_attr(left, field).unwrap_or(NONE);
        let r = value::instance_attr(right, field).unwrap_or(NONE);
        if !container::equals(l, r) {
            return value::FALSE;
        }
    }
    value::TRUE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{encode_int, FALSE, NONE, TRUE};
    fn int(n: i64) -> i64 {
        encode_int(n).expect("representable")
    }

    fn as_int(v: i64) -> i64 {
        value::as_integer(v).expect("int result")
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

    /// Stand-in for a generated dynamic wrapper: reads the tagged total count
    /// from `argv[0]` and returns the argument values as a tuple.
    extern "C" fn varargs_probe(argv: *const i64) -> i64 {
        let total = unsafe { *argv } >> 3;
        let mut values = Vec::with_capacity(total as usize);
        for index in 0..total as usize {
            values.push(unsafe { *argv.add(1 + index) });
        }
        value::tuple_new(values)
    }

    #[test]
    fn varargs_calls_pass_a_tagged_count_and_the_arguments() {
        exc::init();
        let function = value::function_new_with_flags(
            varargs_probe as *const () as usize,
            0,
            "probe".to_string(),
            value::FUNCTION_FLAG_VARARGS,
        );
        let args = [int(7), int(8)];
        let result = call(function, 2, args.as_ptr());
        assert!(!exc::pending(), "unexpected exception");
        let items = value::as_tuple(result).expect("probe returns a tuple");
        assert_eq!(items, &[int(7), int(8)]);

        // A dynamic signature stores the minimum arity.
        let required = value::function_new_with_flags(
            varargs_probe as *const () as usize,
            2,
            "required".to_string(),
            value::FUNCTION_FLAG_VARARGS,
        );
        let _ = call(required, 1, args.as_ptr());
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn varargs_closures_prefix_the_captured_values() {
        exc::init();
        let closure = value::closure_new_with_flags(
            varargs_probe as *const () as usize,
            0,
            "closure".to_string(),
            vec![int(5)],
            value::FUNCTION_FLAG_VARARGS,
        );
        let args = [int(9)];
        let result = call(closure, 1, args.as_ptr());
        assert!(!exc::pending(), "unexpected exception");
        let items = value::as_tuple(result).expect("probe returns a tuple");
        assert_eq!(items, &[int(5), int(9)]);
        exc::clear();
    }

    #[test]
    fn repr_uses_python_representation() {
        exc::init();
        let quoted = builtin_call(Builtin::Repr, &[string("a'b")]);
        assert_eq!(value::as_str(quoted), Some("\"a'b\""));
        let list = value::list_new(vec![int(1), string("a")]);
        let rendered_list = builtin_call(Builtin::Repr, &[list]);
        assert_eq!(value::as_str(rendered_list), Some("[1, 'a']"));
        assert_eq!(
            value::as_str(builtin_call(Builtin::Repr, &[int(5)])),
            Some("5")
        );
        assert!(!exc::pending());

        assert_eq!(builtin_call(Builtin::Repr, &[]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "repr() takes exactly 1 argument(s) (0 given)"
        );
    }

    #[test]
    fn starred_calls_flatten_lists_and_tuples() {
        exc::init();
        let function = value::function_new_with_flags(
            varargs_probe as *const () as usize,
            0,
            "probe".to_string(),
            value::FUNCTION_FLAG_VARARGS,
        );
        let star = value::tuple_new(vec![int(7), int(8)]);
        let leading = [int(1)];
        let result = call_star(function, 1, leading.as_ptr(), star);
        assert!(!exc::pending(), "unexpected exception");
        let items = value::as_tuple(result).expect("probe returns a tuple");
        assert_eq!(items, &[int(1), int(7), int(8)]);

        // A list flattens the same way, with no leading operands.
        let star = value::list_new(vec![int(3)]);
        let result = call_star(function, 0, std::ptr::null(), star);
        assert!(!exc::pending(), "unexpected exception");
        let items = value::as_tuple(result).expect("probe returns a tuple");
        assert_eq!(items, &[int(3)]);

        // Builtin values receive the flattened positional arguments too.
        let abs = value::builtin_function_new(Builtin::Abs);
        let star = value::tuple_new(vec![int(-5)]);
        let result = call_star(abs, 0, std::ptr::null(), star);
        assert_eq!(value::as_integer(result), Some(5));
        assert!(!exc::pending());

        // Any other starred value raises TypeError.
        assert_eq!(call_star(function, 0, std::ptr::null(), int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(pending_message(), "'int' object is not iterable");
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
    fn dict_subclass_instances_are_dicts() {
        exc::init();
        // `class D(dict): pass`
        let derived =
            value::class_new_full("D".to_string(), value::dict_class(), Vec::new(), Vec::new());
        let instance = value::instance_new(derived);
        let storage = value::native_dict_data(instance, true).expect("mapping storage");
        container::dict_set(storage, string("a"), int(1));
        assert!(!exc::pending());

        // `isinstance(d, dict)` follows the native class ancestry, and the
        // class itself still matches.
        let dict_type = value::builtin_function_new(Builtin::Dict);
        assert_eq!(isinstance(instance, dict_type), TRUE);
        assert_eq!(isinstance(instance, derived), TRUE);
        // An instance of an unrelated class is not a dict.
        let other = value::instance_new(value::class_new("Other".to_string(), NONE, Vec::new()));
        assert_eq!(isinstance(other, dict_type), FALSE);
        assert!(!exc::pending());

        // `dict(d)` copies the mapping instead of iterating its keys, and the
        // copy is a plain dict.
        let copied = dict_from(instance);
        assert!(!exc::pending());
        assert_eq!(rendered(copied), "{'a': 1}");
        assert!(value::as_instance(copied).is_none());
        // `d` itself is untouched.
        assert_eq!(
            value::mapping_pairs(instance).map(|pairs| pairs.len()),
            Some(1)
        );
    }

    #[test]
    fn dict_build_layers_keywords_over_the_source() {
        exc::init();
        let built = dict_build(
            None,
            &[("a".to_string(), int(1)), ("b".to_string(), int(2))],
        );
        assert!(!exc::pending());
        assert_eq!(rendered(built), "{'a': 1, 'b': 2}");

        let source = dict_from(value::list_new(vec![value::tuple_new(vec![
            string("a"),
            int(1),
        ])]));
        assert_eq!(rendered(source), "{'a': 1}");

        // A keyword with the source's key overwrites the value in place; a new
        // name appends.
        let merged = dict_build(
            Some(source),
            &[("a".to_string(), int(9)), ("c".to_string(), int(3))],
        );
        assert_eq!(rendered(merged), "{'a': 9, 'c': 3}");

        // A plain copy keeps the source untouched.
        let copied = dict_build(Some(source), &[]);
        assert_eq!(rendered(copied), "{'a': 1}");
        assert_eq!(rendered(source), "{'a': 1}");

        // A malformed source still reports the iterable diagnostics.
        let bad = dict_build(Some(int(5)), &[("a".to_string(), int(1))]);
        assert_eq!(bad, NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::init();
    }

    #[test]
    fn dict_keyword_calls_merge_arguments() {
        exc::init();
        let dict = value::builtin_function_new(Builtin::Dict);
        let keywords = [string("a"), int(1), string("b"), int(2)];
        let empty = call_kw(dict, 0, std::ptr::null(), 2, keywords.as_ptr());
        assert!(!exc::pending());
        assert_eq!(rendered(empty), "{'a': 1, 'b': 2}");

        let source = dict_from(value::list_new(vec![value::tuple_new(vec![
            string("a"),
            int(7),
        ])]));
        let merged = call_kw(dict, 1, &source, 2, keywords.as_ptr());
        assert_eq!(rendered(merged), "{'a': 1, 'b': 2}");

        // At most one positional source is accepted.
        let two = [source, source];
        assert_eq!(call_kw(dict, 2, two.as_ptr(), 0, std::ptr::null()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
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

    fn generator(entry: extern "C" fn(*const i64) -> i64) -> i64 {
        exc::init();
        value::fiber_generator_new(entry as usize, "gen".to_string(), Vec::new())
            .expect("generator stack")
    }

    extern "C" fn yields_one_to_three(_argv: *const i64) -> i64 {
        for n in 1..=3 {
            crate::fiber::yield_value(int(n));
        }
        NONE
    }

    extern "C" fn yields_nothing(_argv: *const i64) -> i64 {
        NONE
    }

    extern "C" fn yields_one_then_raises(_argv: *const i64) -> i64 {
        crate::fiber::yield_value(int(1));
        exc::raise_with_message(VALUE_ERROR, "boom");
        NONE
    }

    extern "C" fn yields_text(_argv: *const i64) -> i64 {
        crate::fiber::yield_value(string("a"));
        NONE
    }

    #[test]
    fn sum_of_generators() {
        exc::init();
        assert_eq!(
            value::as_integer(sum(generator(yields_one_to_three), int(0))),
            Some(6)
        );
        assert_eq!(
            value::as_integer(sum(generator(yields_one_to_three), int(10))),
            Some(16)
        );
        assert_eq!(
            value::as_float(sum(generator(yields_one_to_three), value::float_new(0.5))),
            Some(6.5)
        );
        assert_eq!(
            value::as_integer(sum(generator(yields_nothing), int(7))),
            Some(7)
        );
        assert!(!exc::pending());

        // An exhausted generator keeps the start value and raises nothing.
        let exhausted = generator(yields_one_to_three);
        assert_eq!(value::as_integer(sum(exhausted, int(0))), Some(6));
        assert_eq!(value::as_integer(sum(exhausted, int(5))), Some(5));
        assert!(!exc::pending());
    }

    #[test]
    fn sum_of_generator_propagates_errors() {
        exc::init();
        let failing = generator(yields_one_then_raises);
        assert_eq!(sum(failing, int(0)), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(pending_message(), "boom");
        exc::init();

        // The captured exception re-raises on later drives, like iteration.
        assert_eq!(sum(failing, int(0)), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::init();

        assert_eq!(sum(generator(yields_text), int(0)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "unsupported operand type(s) for +: 'int' and 'str'"
        );
        exc::init();
    }

    #[test]
    fn generator_collection_is_unchanged() {
        exc::init();
        assert_eq!(
            integers(list_from(generator(yields_one_to_three))),
            vec![1, 2, 3]
        );
        assert!(!exc::pending());
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
    fn zip_strict_reports_length_mismatches() {
        exc::init();
        // `strict=False` keeps the truncating behaviour.
        let longer = value::list_new(vec![int(1), int(2)]);
        let shorter = value::list_new(vec![int(3)]);
        let sources = [longer, shorter];
        assert_eq!(rendered(zip_strict(2, sources.as_ptr(), false)), "[(1, 3)]");
        assert!(!exc::pending());

        // A later source that ended first is reported as shorter.
        assert_eq!(zip_strict(2, sources.as_ptr(), true), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "zip() argument 2 is shorter than argument 1"
        );
        exc::init();

        // The first source ended while a later one still yields: that one is
        // longer (CPython's `check:` block probes the remaining iterators).
        let first = value::list_new(vec![int(1)]);
        let second = value::list_new(vec![int(2), int(3)]);
        let sources = [first, second];
        assert_eq!(zip_strict(2, sources.as_ptr(), true), NONE);
        assert_eq!(
            pending_message(),
            "zip() argument 2 is longer than argument 1"
        );
        exc::init();

        // Three sources use CPython's plural spelling.
        let a = value::list_new(vec![int(1), int(2), int(3)]);
        let b = value::list_new(vec![int(4), int(5), int(6)]);
        let c = value::list_new(vec![int(7), int(8)]);
        let sources = [a, b, c];
        assert_eq!(zip_strict(3, sources.as_ptr(), true), NONE);
        assert_eq!(
            pending_message(),
            "zip() argument 3 is shorter than arguments 1-2"
        );
        exc::init();

        let a = value::list_new(vec![int(1)]);
        let b = value::list_new(vec![int(2)]);
        let c = value::list_new(vec![int(3), int(4)]);
        let sources = [a, b, c];
        assert_eq!(zip_strict(3, sources.as_ptr(), true), NONE);
        assert_eq!(
            pending_message(),
            "zip() argument 3 is longer than arguments 1-2"
        );
        exc::init();

        // Equal lengths and zero sources succeed under `strict=True`.
        let left = value::list_new(vec![int(1), int(2)]);
        let right = value::list_new(vec![int(3), int(4)]);
        let sources = [left, right];
        assert_eq!(
            rendered(zip_strict(2, sources.as_ptr(), true)),
            "[(1, 3), (2, 4)]"
        );
        assert!(!exc::pending());
        assert_eq!(rendered(zip_strict(0, std::ptr::null(), true)), "[]");
        assert!(!exc::pending());
    }

    #[test]
    fn zip_strict_keyword_binding() {
        exc::init();
        let zip_fn = value::builtin_function_new(Builtin::Zip);
        let left = value::list_new(vec![int(1), int(2)]);
        let right = value::list_new(vec![int(3)]);
        let args = [left, right];

        let pairs = [string("strict"), TRUE];
        assert_eq!(call_kw(zip_fn, 2, args.as_ptr(), 1, pairs.as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::init();

        let pairs = [string("strict"), FALSE];
        assert_eq!(
            rendered(call_kw(zip_fn, 2, args.as_ptr(), 1, pairs.as_ptr())),
            "[(1, 3)]"
        );
        assert!(!exc::pending());

        // Any truthy object enables the check, like `PyObject_IsTrue`.
        let pairs = [string("strict"), int(1)];
        assert_eq!(call_kw(zip_fn, 2, args.as_ptr(), 1, pairs.as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::init();

        let pairs = [string("mode"), TRUE];
        assert_eq!(call_kw(zip_fn, 2, args.as_ptr(), 1, pairs.as_ptr()), NONE);
        assert_eq!(
            pending_message(),
            "zip() got an unexpected keyword argument 'mode'"
        );
        exc::init();

        // No keyword keeps the plain positional path.
        assert_eq!(
            rendered(call_kw(zip_fn, 2, args.as_ptr(), 0, std::ptr::null())),
            "[(1, 3)]"
        );
        assert!(!exc::pending());
    }

    #[test]
    fn super_two_argument_builds_a_proxy() {
        exc::init();
        let object = value::object_class();
        let base = value::class_new("Base".to_string(), object, Vec::new());
        let derived = value::class_new("Derived".to_string(), base, Vec::new());
        let instance = value::instance_new(derived);

        let proxy = builtin_call(Builtin::Super, &[base, instance]);
        assert!(!exc::pending());
        let state = value::as_super(proxy).expect("super proxy");
        assert_eq!(state.class, base);
        assert_eq!(state.object, instance);

        // The second argument may be a class (the `classmethod` form).
        let proxy = builtin_call(Builtin::Super, &[base, derived]);
        assert!(!exc::pending());
        let state = value::as_super(proxy).expect("super proxy");
        assert_eq!(state.class, base);
        assert_eq!(state.object, derived);

        // A class outside the MRO is rejected like CPython.
        let unrelated = value::class_new("Unrelated".to_string(), object, Vec::new());
        assert_eq!(builtin_call(Builtin::Super, &[unrelated, instance]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "super(type, obj): obj must be an instance or subtype of type"
        );
        exc::init();

        // The first argument must be a class.
        assert_eq!(builtin_call(Builtin::Super, &[int(1), instance]), NONE);
        assert_eq!(
            pending_message(),
            "super() argument 1 must be a type, not 'int'"
        );
        exc::init();

        // `super(type)` stays unbound: a proxy without a receiver.
        let proxy = builtin_call(Builtin::Super, &[derived]);
        assert!(!exc::pending());
        let state = value::as_super(proxy).expect("unbound super proxy");
        assert_eq!(state.class, derived);
        assert_eq!(state.object, NONE);

        // A zero-argument call only reaches the runtime through a
        // first-class value; it reports CPython's RuntimeError.
        assert_eq!(builtin_call(Builtin::Super, &[]), NONE);
        assert_eq!(exc::pending_tag(), RUNTIME_ERROR as i64);
        exc::init();
    }

    #[test]
    fn iter_and_next_as_first_class_values() {
        exc::init();
        let iter_fn = value::builtin_function_new(Builtin::Iter);
        let next_fn = value::builtin_function_new(Builtin::Next);

        let source = [value::list_new(vec![int(4), int(5)])];
        let iterator = call(iter_fn, 1, source.as_ptr());
        assert!(!exc::pending());
        assert_eq!(value::type_name(iterator), "iterator");

        assert_eq!(call(next_fn, 1, [iterator].as_ptr()), int(4));
        let args = [iterator, int(-1)];
        assert_eq!(call(next_fn, 2, args.as_ptr()), int(5));
        // Exhausted: the first-class default suppresses StopIteration.
        assert_eq!(call(next_fn, 2, args.as_ptr()), int(-1));
        assert_eq!(call(next_fn, 1, [iterator].as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), exc::STOP_ITERATION as i64);

        exc::init();
        assert_eq!(call(iter_fn, 0, std::ptr::null()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "iter() takes 1 or 2 argument(s) (0 given)"
        );
    }

    static ITER_SOURCE_CALLS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

    /// Zero-argument callable yielding `0`, `1`, then `None` (the sentinel).
    extern "C" fn counted_callable(_argv: *const i64) -> i64 {
        let call = ITER_SOURCE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if call < 2 {
            int(call)
        } else {
            NONE
        }
    }

    #[test]
    fn iter_callable_as_first_class_values() {
        exc::init();
        ITER_SOURCE_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
        let source = value::function_new(
            counted_callable as *const () as usize,
            0,
            "source".to_string(),
        );
        let iter_fn = value::builtin_function_new(Builtin::Iter);
        let next_fn = value::builtin_function_new(Builtin::Next);

        let args = [source, NONE];
        let iterator = call(iter_fn, 2, args.as_ptr());
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(value::type_name(iterator), "iterator");

        assert_eq!(call(next_fn, 1, [iterator].as_ptr()), int(0));
        assert_eq!(call(next_fn, 1, [iterator].as_ptr()), int(1));
        assert_eq!(call(next_fn, 1, [iterator].as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), exc::STOP_ITERATION as i64);
        // Three calls produced the values and the sentinel; nothing ran again.
        assert_eq!(
            ITER_SOURCE_CALLS.load(std::sync::atomic::Ordering::SeqCst),
            3
        );
        exc::clear();
        assert_eq!(call(next_fn, 2, [iterator, int(-1)].as_ptr()), int(-1));
        assert_eq!(
            ITER_SOURCE_CALLS.load(std::sync::atomic::Ordering::SeqCst),
            3
        );

        // The callable is validated eagerly.
        exc::init();
        let bogus = [int(1), NONE];
        assert_eq!(call(iter_fn, 2, bogus.as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(pending_message(), "iter(v, w): v must be callable");

        exc::init();
        let too_many = [int(1), int(2), int(3)];
        assert_eq!(call(iter_fn, 3, too_many.as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "iter() takes 1 or 2 argument(s) (3 given)"
        );
    }

    fn builtin_call(builtin: Builtin, args: &[i64]) -> i64 {
        call_builtin(builtin, args.len(), args.as_ptr())
    }

    fn set_integers(v: i64) -> Vec<i64> {
        let mut items: Vec<i64> = value::as_set(v)
            .expect("set result")
            .iter()
            .map(|&item| value::as_integer(item).expect("integer element"))
            .collect();
        items.sort_unstable();
        items
    }

    #[test]
    fn frozenset_builds_a_set() {
        exc::init();
        let empty = builtin_call(Builtin::Frozenset, &[]);
        assert_eq!(value::type_name(empty), "set");
        assert_eq!(rendered(empty), "set()");
        assert!(!value::truthy(empty));

        let source = value::list_new(vec![int(3), int(1), int(3)]);
        let built = builtin_call(Builtin::Frozenset, &[source]);
        assert_eq!(set_integers(built), vec![1, 3]);

        let duplicated = builtin_call(Builtin::Frozenset, &[built]);
        assert_eq!(set_integers(duplicated), vec![1, 3]);

        // A range is consumed through the iterator protocol.
        let from_range = builtin_call(Builtin::Frozenset, &[value::range_new(0, 5, 2)]);
        assert_eq!(set_integers(from_range), vec![0, 2, 4]);
        assert!(!exc::pending());

        assert_eq!(builtin_call(Builtin::Frozenset, &[source, source]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "frozenset() takes 0 or 1 argument(s) (2 given)"
        );
    }

    #[test]
    fn getattr_dispatches_module_values_and_objects() {
        exc::init();
        let math = value::module_value("math".to_string());
        let pi = builtin_call(Builtin::Getattr, &[math, string("pi")]);
        assert_eq!(value::as_float(pi), Some(std::f64::consts::PI));
        assert!(!exc::pending());

        assert_eq!(
            builtin_call(Builtin::Getattr, &[math, string("missing")]),
            NONE
        );
        assert_eq!(exc::pending_tag(), exc::ATTRIBUTE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "module 'math' has no attribute 'missing'"
        );

        // The three-argument form returns the default for a missing module
        // attribute instead of propagating the AttributeError.
        exc::init();
        assert_eq!(
            builtin_call(Builtin::Getattr, &[math, string("missing"), int(7)]),
            int(7)
        );
        assert!(!exc::pending());

        exc::init();
        let namespace = value::namespace_new(vec![("answer".to_string(), int(42))]);
        assert_eq!(
            builtin_call(Builtin::Getattr, &[namespace, string("answer")]),
            int(42)
        );
        assert!(!exc::pending());

        exc::init();
        assert_eq!(builtin_call(Builtin::Getattr, &[math, int(1)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "attribute name must be string, not 'int'"
        );

        exc::init();
        assert_eq!(builtin_call(Builtin::Getattr, &[math]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "getattr() takes 2 or 3 argument(s) (1 given)"
        );
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
        let ordered = sorted(3, [source, NONE, TRUE].as_ptr());

        for _ in 0..128 {
            let _ = string("garbage");
        }
        crate::heap::collect();

        assert_eq!(rendered(enumerated), "[(0, 'a'), (1, 'b')]");
        assert_eq!(rendered(zipped), "[('a',), ('b',)]");
        assert_eq!(integers(listed), vec![0, 1, 2, 3]);
        assert_eq!(rendered(ordered), "['b', 'a']");
        assert_eq!(value::as_integer(summed), Some(6));
        assert!(!exc::pending());
    }

    #[test]
    fn max_min_over_candidates_and_iterables() {
        exc::init();
        let variadic = [int(1), int(5), int(3), NONE];
        assert_eq!(
            value::as_integer(max(variadic.len(), variadic.as_ptr())),
            Some(5)
        );
        assert_eq!(
            value::as_integer(min(variadic.len(), variadic.as_ptr())),
            Some(1)
        );

        // First-class calls do not append a key slot.
        let bare = [int(1), int(5), int(3)];
        assert_eq!(value::as_integer(max(bare.len(), bare.as_ptr())), Some(5));
        assert_eq!(value::as_integer(min(bare.len(), bare.as_ptr())), Some(1));

        // A single candidate is iterated.
        let list = value::list_new(vec![int(4), int(9), int(2)]);
        let single = [list, NONE];
        assert_eq!(value::as_integer(max(2, single.as_ptr())), Some(9));
        assert_eq!(value::as_integer(min(2, single.as_ptr())), Some(2));

        let tuple = value::tuple_new(vec![int(-1), int(-7)]);
        let single_tuple = [tuple, NONE];
        assert_eq!(value::as_integer(max(2, single_tuple.as_ptr())), Some(-1));

        // Ties keep the first candidate.
        let ties = [int(2), int(2), int(1), NONE];
        let tied = value::list_new(vec![int(2), int(2), int(1)]);
        let first = max(3, [int(2), int(2), NONE].as_ptr());
        assert_eq!(value::as_integer(first), Some(2));
        assert_eq!(value::as_integer(max(4, ties.as_ptr())), Some(2));
        assert_eq!(value::as_integer(len(tied)), Some(3));

        // Empty iterables raise ValueError.
        let empty = value::list_new(Vec::new());
        let empty_args = [empty, NONE];
        assert_eq!(max(2, empty_args.as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(pending_message(), "max() arg is an empty sequence");
        exc::init();
        assert_eq!(min(2, empty_args.as_ptr()), NONE);
        assert_eq!(pending_message(), "min() arg is an empty sequence");

        // Non-iterable single candidates are reported by the iterator.
        exc::init();
        let scalar = [int(1), NONE];
        assert_eq!(max(2, scalar.as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    extern "C" fn negating_key(argv: *const i64) -> i64 {
        // SAFETY: `call` passes a pointer to one live tagged argument.
        let item = unsafe { *argv };
        let n = value::as_integer(item).expect("integer argument");
        ops::int_result(-n)
    }

    #[test]
    fn max_min_apply_the_key_function() {
        exc::init();
        let list = value::list_new(vec![int(1), int(5), int(3)]);
        let key = value::function_new(negating_key as *const () as usize, 1, "key".to_string());
        let args = [list, key];
        assert_eq!(value::as_integer(max(2, args.as_ptr())), Some(1));
        assert_eq!(value::as_integer(min(2, args.as_ptr())), Some(5));

        // The key also applies to variadic candidates.
        let variadic = [int(1), int(5), key];
        assert_eq!(value::as_integer(max(3, variadic.as_ptr())), Some(1));
        assert_eq!(value::as_integer(min(3, variadic.as_ptr())), Some(5));
    }

    #[test]
    fn sorted_orders_numbers_and_strings() {
        exc::init();
        let numbers = value::list_new(vec![int(3), int(1), int(2)]);
        let result = sorted(3, [numbers, NONE, NONE].as_ptr());
        assert_eq!(integers(result), vec![1, 2, 3]);
        // A fresh list is returned; the source keeps its order.
        assert_eq!(integers(numbers), vec![3, 1, 2]);
        assert_ne!(result, numbers);

        // One-argument (first-class) calls work too, and any iterable is
        // accepted.
        let words = value::list_new(vec![string("pear"), string("apple"), string("fig")]);
        let result = sorted(1, [words].as_ptr());
        assert_eq!(rendered(result), "['apple', 'fig', 'pear']");

        let countdown = ranged(3, 0, -1);
        assert_eq!(integers(sorted(1, [countdown].as_ptr())), vec![1, 2, 3]);

        let empty = value::list_new(Vec::new());
        assert_eq!(rendered(sorted(3, [empty, NONE, NONE].as_ptr())), "[]");
    }

    extern "C" fn length_key(argv: *const i64) -> i64 {
        // SAFETY: `call` passes a pointer to one live tagged argument.
        let item = unsafe { *argv };
        len(item)
    }

    #[test]
    fn sorted_applies_the_key_once_and_honours_reverse() {
        exc::init();
        let list = value::list_new(vec![int(1), int(5), int(3)]);
        let key = value::function_new(negating_key as *const () as usize, 1, "key".to_string());
        let ascending = sorted(3, [list, key, NONE].as_ptr());
        assert_eq!(integers(ascending), vec![5, 3, 1]);
        let descending = sorted(3, [list, key, TRUE].as_ptr());
        assert_eq!(integers(descending), vec![1, 3, 5]);
    }

    #[test]
    fn sorted_is_stable_in_both_directions() {
        exc::init();
        // Keys are string lengths: "a"/"d" tie at 1 and "bb"/"cc" tie at 2.
        let items = value::list_new(vec![string("bb"), string("a"), string("cc"), string("d")]);
        let key = value::function_new(length_key as *const () as usize, 1, "key".to_string());
        let ascending = sorted(3, [items, key, NONE].as_ptr());
        assert_eq!(rendered(ascending), "['a', 'd', 'bb', 'cc']");
        // `reverse=True` reverses the order while equal keys keep their
        // source order.
        let descending = sorted(3, [items, key, TRUE].as_ptr());
        assert_eq!(rendered(descending), "['bb', 'cc', 'a', 'd']");
    }

    #[test]
    fn sorted_reports_non_iterables_and_bad_keys() {
        exc::init();
        assert_eq!(sorted(3, [int(1), NONE, NONE].as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);

        exc::init();
        let list = value::list_new(vec![int(1)]);
        assert_eq!(sorted(3, [list, int(2), NONE].as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(pending_message(), "'int' object is not callable");

        // Comparisons use Python's `<`, so mismatched element types raise.
        exc::init();
        let mixed = value::list_new(vec![int(1), string("a")]);
        assert_eq!(sorted(3, [mixed, NONE, NONE].as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);

        exc::init();
        assert_eq!(builtin_call(Builtin::Sorted, &[]), NONE);
        assert_eq!(
            pending_message(),
            "sorted() takes 1 to 3 argument(s) (0 given)"
        );
    }

    #[test]
    fn round_uses_python_ties_to_even() {
        exc::init();
        assert_eq!(value::as_integer(round_value(int(5), NONE)), Some(5));
        assert_eq!(value::as_integer(round_value(TRUE, NONE)), Some(1));
        assert_eq!(value::as_integer(round_value(int(15), int(-1))), Some(20));
        assert_eq!(value::as_integer(round_value(int(25), int(-1))), Some(20));
        assert_eq!(value::as_integer(round_value(int(-25), int(-1))), Some(-20));
        assert_eq!(value::as_integer(round_value(int(5), int(-1))), Some(0));
        assert_eq!(
            value::as_integer(round_value(int(1234), int(-2))),
            Some(1200)
        );

        // Floats without ndigits become ints, ties to even.
        assert_eq!(
            value::as_integer(round_value(value::float_new(2.5), NONE)),
            Some(2)
        );
        assert_eq!(
            value::as_integer(round_value(value::float_new(3.5), NONE)),
            Some(4)
        );
        assert_eq!(
            value::as_integer(round_value(value::float_new(-2.5), NONE)),
            Some(-2)
        );

        // With ndigits the result stays a float, rounded on the exact decimal
        // expansion of the float.
        assert_eq!(
            value::as_float(round_value(value::float_new(2.675), int(2))),
            Some(2.67)
        );
        assert_eq!(
            value::as_float(round_value(value::float_new(0.125), int(2))),
            Some(0.12)
        );
        assert_eq!(
            value::as_float(round_value(value::float_new(2.5), int(0))),
            Some(2.0)
        );
        assert_eq!(
            value::as_float(round_value(value::float_new(2.5), int(5))),
            Some(2.5)
        );

        assert_eq!(round_value(value::float_new(f64::NAN), NONE), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::init();
        assert_eq!(round_value(string("x"), NONE), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "type str doesn't define __round__ method"
        );
        exc::init();
        assert_eq!(round_value(int(1), string("2")), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn format_builtin_uses_the_shared_formatter() {
        exc::init();
        // `format(value)` defaults `format_spec` to the empty string.
        assert_eq!(
            value::as_str(builtin_call(Builtin::Format, &[int(7)])),
            Some("7")
        );
        assert_eq!(
            value::as_str(builtin_call(Builtin::Format, &[string("x")])),
            Some("x")
        );
        assert!(!exc::pending());
        assert_eq!(
            value::as_str(format_builtin(int(255), string("#x"), 0)),
            Some("0xff")
        );
        assert_eq!(
            value::as_str(format_builtin(int(42), string("04d"), 0)),
            Some("0042")
        );
        assert_eq!(
            value::as_str(format_builtin(value::float_new(3.14159), string(".2f"), 0)),
            Some("3.14")
        );
        assert_eq!(
            value::as_str(format_builtin(string("x"), string(">3"), 0)),
            Some("  x")
        );
        assert!(!exc::pending());

        assert_eq!(format_builtin(int(1), int(1), 0), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::init();
        assert_eq!(format_builtin(value::float_new(1.5), string("d"), 0), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
    }

    #[test]
    fn isinstance_native_types_and_tuples() {
        exc::init();
        let int_type = value::builtin_function_new(Builtin::Int);
        let float_type = value::builtin_function_new(Builtin::Float);
        let str_type = value::builtin_function_new(Builtin::Str);
        let list_type = value::builtin_function_new(Builtin::List);
        let dict_type = value::builtin_function_new(Builtin::Dict);
        let set_type = value::builtin_function_new(Builtin::Set);
        let bytes_type = value::builtin_function_new(Builtin::Bytes);

        assert_eq!(isinstance(int(3), int_type), TRUE);
        // Python's bool is an int subclass.
        assert_eq!(isinstance(TRUE, int_type), TRUE);
        assert_eq!(isinstance(string("x"), int_type), FALSE);
        assert_eq!(isinstance(value::float_new(1.0), float_type), TRUE);
        assert_eq!(isinstance(int(1), float_type), FALSE);
        assert_eq!(isinstance(string("x"), str_type), TRUE);
        assert_eq!(isinstance(value::list_new(Vec::new()), list_type), TRUE);
        assert_eq!(isinstance(value::dict_new(Vec::new()), dict_type), TRUE);
        assert_eq!(
            isinstance(container::set_new(0, std::ptr::null()), set_type),
            TRUE
        );
        assert_eq!(isinstance(value::bytes_new(vec![1, 2]), bytes_type), TRUE);

        // `None` names NoneType.
        assert_eq!(isinstance(NONE, NONE), TRUE);
        assert_eq!(isinstance(int(1), NONE), FALSE);

        let union = value::tuple_new(vec![float_type, str_type]);
        assert_eq!(isinstance(string("x"), union), TRUE);
        assert_eq!(isinstance(value::float_new(0.5), union), TRUE);
        assert_eq!(isinstance(int(1), union), FALSE);

        assert_eq!(isinstance(int(1), int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "isinstance() arg 2 must be a type or tuple of types"
        );
        exc::init();
        assert_eq!(
            isinstance(string("x"), value::tuple_new(vec![int_type, int(2)])),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    fn isinstance_walks_class_bases() {
        exc::init();
        let base = value::class_new("Animal".to_string(), NONE, Vec::new());
        let derived = value::class_new("Dog".to_string(), base, Vec::new());
        let sibling = value::class_new("Cat".to_string(), base, Vec::new());
        let dog = value::instance_new(derived);
        let cat = value::instance_new(sibling);

        assert_eq!(isinstance(dog, derived), TRUE);
        assert_eq!(isinstance(dog, base), TRUE);
        assert_eq!(isinstance(cat, derived), FALSE);
        assert_eq!(isinstance(cat, base), TRUE);
        assert!(!exc::pending());
    }

    #[test]
    fn isinstance_uses_the_exception_hierarchy() {
        exc::init();
        let value_error = exc::exception_class(exc::VALUE_ERROR);
        let exception = exc::exception_class(exc::EXCEPTION);
        let base_exception = exc::exception_class(exc::BASE_EXCEPTION);
        let arithmetic = exc::exception_class(exc::ARITHMETIC_ERROR);
        let zero_division = exc::exception_class(exc::ZERO_DIVISION_ERROR);
        let overflow = exc::exception_class(exc::OVERFLOW_ERROR);
        let lookup = exc::exception_class(exc::LOOKUP_ERROR);
        let key_error = exc::exception_class(exc::KEY_ERROR);
        let import_error = exc::exception_class(exc::IMPORT_ERROR);
        let missing_module = exc::exception_class(exc::MODULE_NOT_FOUND_ERROR);
        let os_error = exc::exception_class(exc::OS_ERROR);
        let file_not_found = exc::exception_class(exc::FILE_NOT_FOUND_ERROR);

        let raised = exc::exception_value(exc::VALUE_ERROR, "boom", vec!["boom".to_string()]);
        assert_eq!(isinstance(raised, value_error), TRUE);
        assert_eq!(isinstance(raised, exception), TRUE);
        assert_eq!(isinstance(raised, base_exception), TRUE);
        assert_eq!(isinstance(raised, arithmetic), FALSE);
        assert_eq!(isinstance(raised, os_error), FALSE);

        // ArithmeticError catches its two concrete children.
        let zero = exc::exception_value(exc::ZERO_DIVISION_ERROR, "", Vec::new());
        assert_eq!(isinstance(zero, arithmetic), TRUE);
        assert_eq!(isinstance(zero, zero_division), TRUE);
        assert_eq!(isinstance(zero, overflow), FALSE);
        assert_eq!(isinstance(zero, exception), TRUE);
        let overflowed = exc::exception_value(exc::OVERFLOW_ERROR, "", Vec::new());
        assert_eq!(isinstance(overflowed, arithmetic), TRUE);

        // LookupError catches IndexError and KeyError.
        let key = exc::exception_value(exc::KEY_ERROR, "'k'", vec!["k".to_string()]);
        assert_eq!(isinstance(key, lookup), TRUE);
        assert_eq!(isinstance(key, key_error), TRUE);

        // ImportError catches ModuleNotFoundError.
        let missing = exc::exception_value(exc::MODULE_NOT_FOUND_ERROR, "", Vec::new());
        assert_eq!(isinstance(missing, import_error), TRUE);
        assert_eq!(isinstance(missing, missing_module), TRUE);
        assert_eq!(isinstance(missing, exception), TRUE);

        // OSError catches FileNotFoundError.
        let missing_file = exc::exception_value(exc::FILE_NOT_FOUND_ERROR, "", Vec::new());
        assert_eq!(isinstance(missing_file, os_error), TRUE);
        assert_eq!(isinstance(missing_file, file_not_found), TRUE);
        assert_eq!(isinstance(missing_file, exception), TRUE);
        assert_eq!(isinstance(missing_file, value_error), FALSE);

        // A user class derived from a native exception class inherits the
        // whole CPython ancestry for its instances.
        let user = value::class_new("MyError".to_string(), value_error, Vec::new());
        let instance = value::instance_new(user);
        assert_eq!(isinstance(instance, user), TRUE);
        assert_eq!(isinstance(instance, value_error), TRUE);
        assert_eq!(isinstance(instance, exception), TRUE);
        assert_eq!(isinstance(instance, base_exception), TRUE);
        assert_eq!(isinstance(instance, arithmetic), FALSE);
        assert!(!exc::pending());
        exc::init();
    }

    #[test]
    fn isinstance_matches_object_and_type_classes() {
        exc::init();
        let object = value::object_class();
        let class = value::class_new("C".to_string(), NONE, Vec::new());
        let raised = exc::exception_value(exc::VALUE_ERROR, "", Vec::new());

        // `object` is the root of the hierarchy: everything is an instance.
        assert_eq!(isinstance(int(1), object), TRUE);
        assert_eq!(isinstance(TRUE, object), TRUE);
        assert_eq!(isinstance(NONE, object), TRUE);
        assert_eq!(isinstance(string("x"), object), TRUE);
        assert_eq!(isinstance(raised, object), TRUE);
        assert_eq!(isinstance(class, object), TRUE);

        // `type(x)` of a class value is the cached `type` class.
        let type_class = value::type_class(class);
        assert_eq!(isinstance(class, type_class), TRUE);
        assert_eq!(isinstance(int(1), type_class), FALSE);
        assert_eq!(isinstance(raised, type_class), FALSE);

        // `type(x)` of a primitive matches exactly that value kind.
        let int_class = value::type_class(int(3));
        assert_eq!(isinstance(int(3), int_class), TRUE);
        assert_eq!(isinstance(string("x"), int_class), FALSE);
        let none_class = value::type_class(NONE);
        assert_eq!(isinstance(NONE, none_class), TRUE);
        assert_eq!(isinstance(int(1), none_class), FALSE);
        assert!(!exc::pending());
        exc::init();
    }

    #[test]
    fn builtin_type_spellings_act_as_class_operands() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        let type_builtin = value::builtin_function_new(Builtin::Type);
        let int_builtin = value::builtin_function_new(Builtin::Int);
        let list_builtin = value::builtin_function_new(Builtin::List);

        // `isinstance(type, type)`; classes and their builtin spellings are
        // instances of `type`, plain values are not.
        assert_eq!(isinstance(type_builtin, type_builtin), TRUE);
        assert_eq!(isinstance(int_builtin, type_builtin), TRUE);
        assert_eq!(isinstance(value::type_class(int(1)), type_builtin), TRUE);
        assert_eq!(isinstance(int(1), type_builtin), FALSE);
        assert!(!exc::pending());

        // `issubclass(int, type)` is False (int is an *instance* of type) and
        // no longer raises; class operands compare by the class they name.
        assert_eq!(issubclass(int_builtin, type_builtin), FALSE);
        assert_eq!(issubclass(type_builtin, type_builtin), TRUE);
        assert_eq!(issubclass(int_builtin, int_builtin), TRUE);
        assert_eq!(issubclass(int_builtin, value::object_class()), TRUE);
        assert_eq!(issubclass(int_builtin, list_builtin), FALSE);
        // A tuple target accepts the builtin spellings too.
        assert_eq!(
            issubclass(
                int_builtin,
                value::tuple_new(vec![list_builtin, int_builtin])
            ),
            TRUE
        );
        assert!(!exc::pending());

        // `copy`'s dispatch compares `type(x)` against the builtin spelling:
        // a cached scalar/container class equals its builtin function value.
        let int_class = value::type_class(int(1));
        let list_class = value::type_class(value::list_new(Vec::new()));
        assert!(crate::container::equals(int_class, int_builtin));
        assert!(crate::container::equals(list_class, list_builtin));
        assert!(!crate::container::equals(int_class, list_builtin));

        // The class exposes the unbound `copy` slot `copy.copy` calls as
        // `cls.copy(x)`.
        let items = value::list_new(vec![int(1), int(2)]);
        let copy = methods::getattr(list_class, b"copy".as_ptr(), "copy".len());
        let result = crate::builtins::call(copy, 1, [items].as_ptr());
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(
            value::as_list(result).map(|items| items.to_vec()),
            Some(vec![int(1), int(2)])
        );
        assert_ne!(result, items);
        // A tuple's copy is the tuple itself.
        let pair = value::tuple_new(vec![int(3)]);
        let tuple_copy = methods::getattr(value::type_class(pair), b"copy".as_ptr(), "copy".len());
        assert_eq!(crate::builtins::call(tuple_copy, 1, [pair].as_ptr()), pair);
        assert!(!exc::pending());
    }

    #[test]
    fn functools_partial_is_a_class_for_type_checks_and_unions() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        let target = value::builtin_function_new(Builtin::Str);
        let partial = value::partial_new(target, Vec::new());
        let partial_class = value::partial_class();

        assert!(value::is_partial_class(value::type_class(partial)));
        assert_eq!(value::type_name(partial), "partial");
        assert_eq!(isinstance(partial, partial_class), TRUE);
        assert_eq!(isinstance(target, partial_class), FALSE);
        assert_eq!(isinstance(partial, target), FALSE);
        assert_eq!(issubclass(partial_class, value::object_class()), TRUE);

        // Calling the class still builds a real partial, which stays callable.
        let made = crate::builtins::call(partial_class, 1, [target].as_ptr());
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(isinstance(made, partial_class), TRUE);
        let bound = crate::builtins::call(made, 1, [int(4)].as_ptr());
        assert_eq!(value::as_str(bound), Some("4"));

        // `FunctionType | partial` composes as a PEP 604 type union.
        let function_class = crate::modules::types::function_type_class();
        assert_eq!(issubclass(partial_class, function_class), FALSE);
        let union = crate::ops::binop(crate::ops::OP_BIT_OR, function_class, partial_class);
        assert!(!exc::pending(), "{}", pending_message());
        assert!(crate::modules::types::is_union(union));
        let members = value::instance_attr(union, "__args__")
            .and_then(value::as_tuple)
            .expect("union members")
            .to_vec();
        assert_eq!(members, vec![function_class, partial_class]);
        assert!(!exc::pending());
    }

    #[test]
    fn object_protocol_slots_resolve_past_a_plain_class() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        // `typing_extensions.sentinel` declares no base, so the class records
        // an empty linearization; `super()` inside it must still reach the
        // `object` attribute slots.
        let class = value::class_new("Sentinel".to_string(), NONE, Vec::new());
        for name in ["__setattr__", "__getattribute__", "__delattr__"] {
            assert!(
                matches!(
                    value::class_lookup_after(class, class, name),
                    Some(value::ClassEntry::Method(MethodKind::Normal, _))
                ),
                "{name} must resolve through super()"
            );
            assert_eq!(
                value::class_lookup_owner(class, name),
                Some(value::object_class()),
                "{name} is owned by object"
            );
        }
        // `super()` asks for the next attribute in the real linearization,
        // which ends at `object`; `class_lookup` keeps the construction
        // fallback narrow, so a missing `__init__` still means "no
        // initializer" for ordinary instance construction.
        assert!(matches!(
            value::class_lookup_after(class, class, "__init__"),
            Some(value::ClassEntry::Method(MethodKind::Normal, _))
        ));
        assert_eq!(
            value::class_lookup_owner(class, "__init__"),
            None,
            "the construction path must not synthesize object.__init__"
        );
        assert!(value::class_lookup(class, "__init__").is_none());

        // The `sentinel.__init__` body (`self._name = name` through the
        // overridden `__setattr__`) forwards to `super().__setattr__`, which
        // must perform the default store.
        let instance = value::instance_new(class);
        let proxy = value::super_new(class, instance);
        let name = string("_name");
        let stored = string("x");
        let args = [name, stored];
        assert_eq!(
            crate::methods::call(proxy, b"__setattr__".as_ptr(), 11, 2, args.as_ptr()),
            NONE
        );
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(
            value::instance_attr(instance, "_name").map(value::to_string),
            Some("x".to_string())
        );
        // Reading the stored attribute back through the proxy's slot.
        let name = string("_name");
        let read = crate::methods::call(proxy, b"__getattribute__".as_ptr(), 16, 1, &name);
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(value::to_string(read), "x");
        // And deletion.
        let name = string("_name");
        assert_eq!(
            crate::methods::call(proxy, b"__delattr__".as_ptr(), 11, 1, &name),
            NONE
        );
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(value::instance_attr(instance, "_name"), None);
    }

    #[test]
    fn issubclass_uses_the_exception_hierarchy() {
        exc::init();
        let syntax = exc::exception_class(exc::SYNTAX_ERROR);
        let indentation = exc::exception_class(exc::INDENTATION_ERROR);
        let exception = exc::exception_class(exc::EXCEPTION);
        let base_exception = exc::exception_class(exc::BASE_EXCEPTION);
        let arithmetic = exc::exception_class(exc::ARITHMETIC_ERROR);
        let zero = exc::exception_class(exc::ZERO_DIVISION_ERROR);
        let os_error = exc::exception_class(exc::OS_ERROR);
        let file_not_found = exc::exception_class(exc::FILE_NOT_FOUND_ERROR);

        assert_eq!(issubclass(syntax, exception), TRUE);
        assert_eq!(issubclass(syntax, base_exception), TRUE);
        assert_eq!(issubclass(indentation, syntax), TRUE);
        assert_eq!(issubclass(syntax, indentation), FALSE);
        assert_eq!(issubclass(zero, arithmetic), TRUE);
        assert_eq!(issubclass(arithmetic, zero), FALSE);
        assert_eq!(issubclass(file_not_found, os_error), TRUE);
        assert_eq!(issubclass(os_error, file_not_found), FALSE);
        assert_eq!(issubclass(zero, file_not_found), FALSE);
        // A tuple of types succeeds when any candidate matches.
        assert_eq!(
            issubclass(zero, value::tuple_new(vec![syntax, arithmetic])),
            TRUE
        );
        assert_eq!(
            issubclass(zero, value::tuple_new(vec![syntax, os_error])),
            FALSE
        );

        // User classes: the base chain plus the native tag ancestry.
        let user = value::class_new("MyError".to_string(), zero, Vec::new());
        assert_eq!(issubclass(user, zero), TRUE);
        assert_eq!(issubclass(user, arithmetic), TRUE);
        assert_eq!(issubclass(user, exception), TRUE);
        assert_eq!(issubclass(user, base_exception), TRUE);
        assert_eq!(issubclass(user, syntax), FALSE);

        // Every class is a subclass of `object`.
        let object = value::object_class();
        assert_eq!(issubclass(user, object), TRUE);
        assert_eq!(issubclass(syntax, object), TRUE);
        assert_eq!(issubclass(value::type_class(int(1)), object), TRUE);
        assert!(!exc::pending());

        // Non-class arguments report the CPython TypeError messages.
        assert_eq!(issubclass(int(1), exception), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(pending_message(), "issubclass() arg 1 must be a class");
        exc::init();

        assert_eq!(issubclass(syntax, int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "issubclass() arg 2 must be a class or tuple of classes"
        );
        exc::init();

        assert_eq!(issubclass(syntax, value::tuple_new(vec![int(1)])), NONE);
        assert_eq!(
            pending_message(),
            "issubclass() arg 2 must be a class or tuple of classes"
        );
        exc::init();
    }

    #[test]
    fn getattr_reads_exception_attributes() {
        exc::init();
        let raised = exc::exception_value(exc::VALUE_ERROR, "boom", vec!["boom".to_string()]);
        assert_eq!(
            rendered(getattr_builtin(raised, string("args"))),
            "('boom',)"
        );
        assert_eq!(getattr_builtin(raised, string("__cause__")), NONE);
        assert_eq!(getattr_builtin(raised, string("__context__")), NONE);
        assert_eq!(
            rendered(getattr_builtin(raised, string("__suppress_context__"))),
            "False"
        );
        assert_eq!(getattr_builtin(raised, string("__traceback__")), NONE);
        assert_eq!(
            getattr_builtin(raised, string("__class__")),
            exc::exception_class(exc::VALUE_ERROR)
        );
        assert!(!exc::pending());

        // Unknown attributes still report AttributeError.
        assert_eq!(getattr_builtin(raised, string("missing")), NONE);
        assert_eq!(exc::pending_tag(), exc::ATTRIBUTE_ERROR as i64);
        exc::init();
    }

    #[test]
    fn exception_class_calls_store_args() {
        exc::init();
        let value_error = exc::exception_class(exc::VALUE_ERROR);
        let one = [string("boom")];

        let raised = call(value_error, 1, one.as_ptr());
        assert!(!exc::pending());
        assert_eq!(
            rendered(getattr_builtin(raised, string("args"))),
            "('boom',)"
        );

        // `ValueError()` has an empty args tuple.
        let plain = call(value_error, 0, std::ptr::null());
        assert!(!exc::pending());
        assert_eq!(rendered(getattr_builtin(plain, string("args"))), "()");

        // `KeyError('boom')` keeps the raw argument in `args` and renders it
        // with `repr()` as the message.
        let key_error = exc::exception_class(exc::KEY_ERROR);
        let keyed = call(key_error, 1, one.as_ptr());
        assert_eq!(rendered(keyed), "'boom'");
        assert_eq!(
            rendered(getattr_builtin(keyed, string("args"))),
            "('boom',)"
        );

        // Multiple arguments are rejected explicitly rather than truncated.
        let two = [string("a"), string("b")];
        assert_eq!(call(value_error, 2, two.as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "ValueError() takes at most 1 argument (2 given)"
        );
        exc::init();
    }

    #[test]
    fn exception_keyword_constructors_store_attributes() {
        // Every `exc::init()` frees the previous run's heap and clears the
        // exception-class cache, so classes and strings are reacquired after
        // each one instead of being reused.
        exc::init();
        let attribute_error = exc::exception_class(exc::ATTRIBUTE_ERROR);
        let message = [string("boom")];
        let name = string("missing");

        // `AttributeError('boom', name='missing')`: the keyword is stored and
        // exposed as `.name`; `.obj` defaults to None.
        let pairs = [string("name"), name];
        let raised = call_kw(attribute_error, 1, message.as_ptr(), 1, pairs.as_ptr());
        assert!(!exc::pending());
        assert_eq!(getattr_builtin(raised, string("name")), name);
        assert_eq!(getattr_builtin(raised, string("obj")), NONE);
        assert_eq!(rendered(raised), "boom");
        assert_eq!(
            rendered(getattr_builtin(raised, string("args"))),
            "('boom',)"
        );

        // `NameError` accepts `name`.
        let name_error = exc::exception_class(exc::NAME_ERROR);
        let named = call_kw(name_error, 1, message.as_ptr(), 1, pairs.as_ptr());
        assert!(!exc::pending());
        assert_eq!(getattr_builtin(named, string("name")), name);

        // `NameError` rejects `obj`, naming the class and the keyword.
        exc::init();
        let name_error = exc::exception_class(exc::NAME_ERROR);
        let message = [string("boom")];
        let pairs = [string("obj"), string("missing")];
        assert_eq!(
            call_kw(name_error, 1, message.as_ptr(), 1, pairs.as_ptr()),
            NONE
        );
        assert_eq!(exc::pending_tag(), exc::TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "NameError() got an unexpected keyword argument 'obj'"
        );

        // Classes without keyword parameters reject every keyword.
        exc::init();
        let value_error = exc::exception_class(exc::VALUE_ERROR);
        let message = [string("boom")];
        let pairs = [string("name"), string("missing")];
        assert_eq!(
            call_kw(value_error, 1, message.as_ptr(), 1, pairs.as_ptr()),
            NONE
        );
        assert_eq!(pending_message(), "ValueError() takes no keyword arguments");

        exc::init();
        let attribute_error = exc::exception_class(exc::ATTRIBUTE_ERROR);
        let message = [string("boom")];
        let pairs = [string("foo"), int(1)];
        assert_eq!(
            call_kw(attribute_error, 1, message.as_ptr(), 1, pairs.as_ptr()),
            NONE
        );
        assert_eq!(
            pending_message(),
            "AttributeError() got an unexpected keyword argument 'foo'"
        );
        exc::init();
    }

    #[test]
    fn every_builtin_exception_rejects_unknown_keywords_by_name() {
        // The constructor path must be the per-class one for every built-in
        // exception, never the generic class fallback that reported
        // "class takes no arguments".
        for tag in [
            exc::VALUE_ERROR,
            exc::TYPE_ERROR,
            exc::ATTRIBUTE_ERROR,
            exc::NAME_ERROR,
            exc::OS_ERROR,
            exc::KEY_ERROR,
            exc::RUNTIME_ERROR,
            exc::SYNTAX_ERROR,
            exc::INDENTATION_ERROR,
            exc::TAB_ERROR,
            exc::STOP_ITERATION,
            exc::ASSERTION_ERROR,
            exc::IMPORT_ERROR,
            exc::MEMORY_ERROR,
        ] {
            exc::init();
            let class = exc::exception_class(tag);
            let message = [string("boom")];
            let pairs = [string("not_a_keyword"), int(1)];
            assert_eq!(
                call_kw(class, 1, message.as_ptr(), 1, pairs.as_ptr()),
                NONE,
                "{} accepted an unknown keyword",
                exc::tag_name(tag)
            );
            assert_eq!(exc::pending_tag(), exc::TYPE_ERROR as i64);
            let rendered = pending_message();
            assert!(
                rendered.starts_with(&format!("{}()", exc::tag_name(tag))),
                "{} rejection must name the class: {rendered}",
                exc::tag_name(tag)
            );
            assert!(
                !rendered.contains("class takes no arguments"),
                "{} used the generic class fallback: {rendered}",
                exc::tag_name(tag)
            );
        }
        exc::init();
    }

    extern "C" fn matmul_forty_one(_argv: *const i64) -> i64 {
        int(41)
    }

    extern "C" fn matmul_seven(_argv: *const i64) -> i64 {
        int(7)
    }

    extern "C" fn matmul_in_place(_argv: *const i64) -> i64 {
        int(99)
    }

    #[test]
    fn matmul_dispatches_user_dunders_and_scalars() {
        exc::init();
        // Scalar operands multiply.
        assert_eq!(
            value::as_integer(ops::binop(ops::OP_MATMUL, int(6), int(7))),
            Some(42)
        );
        assert_eq!(
            value::as_float(ops::binop(ops::OP_MATMUL, value::float_new(1.5), int(2))),
            Some(3.0)
        );

        // `__matmul__` on the left operand.
        let left = value::function_new(
            matmul_forty_one as *const () as usize,
            2,
            "__matmul__".to_string(),
        );
        let left_class = value::class_new(
            "Left".to_string(),
            NONE,
            vec![("__matmul__".to_string(), left)],
        );
        let left_instance = value::instance_new(left_class);
        assert_eq!(
            value::as_integer(ops::binop(ops::OP_MATMUL, left_instance, int(1))),
            Some(41)
        );

        // `__rmatmul__` on the right operand, reflected form.
        let right = value::function_new(
            matmul_seven as *const () as usize,
            2,
            "__rmatmul__".to_string(),
        );
        let right_class = value::class_new(
            "Right".to_string(),
            NONE,
            vec![("__rmatmul__".to_string(), right)],
        );
        let right_instance = value::instance_new(right_class);
        assert_eq!(
            value::as_integer(ops::binop(ops::OP_MATMUL, int(1), right_instance)),
            Some(7)
        );

        // `@=` prefers `__imatmul__`; `@` uses `__matmul__`.
        let in_place = value::function_new(
            matmul_in_place as *const () as usize,
            2,
            "__imatmul__".to_string(),
        );
        let both_class = value::class_new(
            "Both".to_string(),
            NONE,
            vec![
                ("__matmul__".to_string(), left),
                ("__imatmul__".to_string(), in_place),
            ],
        );
        let both = value::instance_new(both_class);
        assert_eq!(
            value::as_integer(ops::binop(ops::OP_MATMUL_INPLACE, both, both)),
            Some(99)
        );
        assert_eq!(
            value::as_integer(ops::binop(ops::OP_MATMUL, both, both)),
            Some(41)
        );
        assert!(!exc::pending());
    }

    #[test]
    fn matmul_without_a_protocol_raises_type_error() {
        exc::init();
        assert_eq!(ops::binop(ops::OP_MATMUL, string("a"), string("b")), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "unsupported operand type(s) for @: 'str' and 'str'"
        );

        exc::init();
        assert_eq!(
            ops::binop(ops::OP_MATMUL_INPLACE, string("a"), string("b")),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "unsupported operand type(s) for @=: 'str' and 'str'"
        );
        exc::init();
    }

    #[test]
    fn all_uses_truthiness_and_accepts_empty_iterables() {
        exc::init();
        assert_eq!(all(value::list_new(vec![int(1), string("x"), TRUE])), TRUE);
        assert_eq!(all(value::list_new(vec![int(1), NONE])), FALSE);
        assert_eq!(all(value::list_new(Vec::new())), TRUE);
        assert_eq!(all(value::set_new(Vec::new())), TRUE);
        assert!(!exc::pending());

        assert_eq!(all(int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    fn dict_merge_preserves_order_and_overwrites() {
        exc::init();
        let left = value::dict_new(vec![(string("a"), int(1)), (string("b"), int(2))]);
        let right = value::dict_new(vec![(string("b"), int(3)), (string("c"), int(4))]);
        let merged = dict_merge(left, right);
        assert_eq!(rendered(merged), "{'a': 1, 'b': 3, 'c': 4}");
        // The operands are never mutated.
        assert_eq!(rendered(left), "{'a': 1, 'b': 2}");
        assert_eq!(rendered(right), "{'b': 3, 'c': 4}");
        assert!(!exc::pending());

        assert_eq!(dict_merge(value::list_new(Vec::new()), right), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "dict unpacking requires a mapping, not 'list'"
        );
        exc::init();
        assert_eq!(dict_merge(left, int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    fn attribute_getter_reads_one_or_many_attributes() {
        exc::init();
        let namespace = value::namespace_new(vec![
            ("start".to_string(), int(3)),
            ("end".to_string(), int(9)),
        ]);
        let single = value::attribute_getter_new(vec!["end".to_string()]);
        let argument = namespace;
        assert_eq!(call(single, 1, &argument), int(9));

        let multiple = value::attribute_getter_new(vec!["start".to_string(), "end".to_string()]);
        let pair = call(multiple, 1, &argument);
        let items = value::as_tuple(pair).expect("tuple result");
        assert_eq!(items.len(), 2);
        assert_eq!(value::as_integer(items[0]), Some(3));
        assert_eq!(value::as_integer(items[1]), Some(9));
        assert!(!exc::pending());
    }

    #[test]
    fn attribute_getter_reports_missing_attributes() {
        exc::init();
        let namespace = value::namespace_new(Vec::new());
        let getter = value::attribute_getter_new(vec!["missing".to_string()]);
        assert_eq!(call(getter, 1, &namespace), NONE);
        assert_eq!(exc::pending_tag(), exc::ATTRIBUTE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn item_getter_subscripts_one_or_many_keys() {
        exc::init();
        let list = value::list_new(vec![int(10), int(20)]);
        let single = value::item_getter_new(vec![int(1)]);
        assert_eq!(call(single, 1, &list), int(20));

        let multiple = value::item_getter_new(vec![int(0), int(1)]);
        let pair = call(multiple, 1, &list);
        let items = value::as_tuple(pair).expect("tuple result");
        assert_eq!(value::as_integer(items[0]), Some(10));
        assert_eq!(value::as_integer(items[1]), Some(20));
        assert!(!exc::pending());
    }

    #[test]
    fn partial_binds_leading_arguments() {
        exc::init();
        let target = value::builtin_function_new(Builtin::Str);
        let partial = value::partial_new(target, vec![int(7)]);
        assert_eq!(value::as_str(call(partial, 0, std::ptr::null())), Some("7"));
        assert!(!exc::pending());
    }

    #[test]
    fn identity_returns_its_argument() {
        exc::init();
        let decorator = value::identity_function_new("decorator");
        let argument = string("payload");
        assert_eq!(call(decorator, 1, &argument), argument);
        assert!(!exc::pending());
    }

    #[test]
    fn tuple_from_materialises_iterables() {
        exc::init();
        let list = value::list_new(vec![int(1), int(2)]);
        assert_eq!(
            value::as_tuple(tuple_from(list)).map(|items| items.to_vec()),
            Some(vec![int(1), int(2)])
        );
        let tuple = value::tuple_new(vec![int(3)]);
        assert_eq!(
            value::as_tuple(tuple_from(tuple)).map(|items| items.to_vec()),
            Some(vec![int(3)])
        );
        // Strings iterate by character; other iterables become a fresh tuple.
        let chars = tuple_from(string("ab"));
        assert_eq!(value::as_tuple(chars).map(|items| items.len()), Some(2));
        assert!(!exc::pending());

        assert_eq!(tuple_from(int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(pending_message(), "'int' object is not iterable");
        exc::init();
    }

    #[test]
    fn hasattr_swallows_only_attribute_errors() {
        exc::init();
        let math = value::module_value("math".to_string());
        assert_eq!(builtin_call(Builtin::Hasattr, &[math, string("pi")]), TRUE);
        assert_eq!(
            builtin_call(Builtin::Hasattr, &[math, string("missing")]),
            FALSE
        );
        assert!(!exc::pending());

        // A non-string name is a TypeError, not `False`.
        assert_eq!(builtin_call(Builtin::Hasattr, &[math, int(1)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "attribute name must be string, not 'int'"
        );
        exc::init();
        assert_eq!(builtin_call(Builtin::Hasattr, &[math]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::init();
    }

    /// Stand-in for a generated wrapper taking two positional parameters.
    extern "C" fn pair_probe(argv: *const i64) -> i64 {
        // SAFETY: the runtime only enters a wrapper after checking its arity.
        let (left, right) = unsafe { (*argv, *argv.add(1)) };
        value::tuple_new(vec![left, right])
    }

    #[test]
    fn partial_applies_keywords_by_parameter_name() {
        exc::init();
        let target = value::function_new(pair_probe as *const () as usize, 2, "pair".to_string());
        value::function_set_params(target, vec!["left".to_string(), "right".to_string()]);
        let partial = value::partial_new_with_keywords(
            target,
            Vec::new(),
            vec![("right".to_string(), int(9))],
        );
        let left = int(1);
        let result = call(partial, 1, &left);
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(rendered(result), "(1, 9)");

        // A hole before the named parameter cannot be filled without the
        // target's defaults, so it is reported instead of passing `None`.
        let hole = value::partial_new_with_keywords(
            target,
            Vec::new(),
            vec![("right".to_string(), int(9))],
        );
        assert_eq!(call(hole, 0, std::ptr::null()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "pair() missing required positional argument 'left'"
        );
        exc::init();

        // Unknown names are never dropped.
        let unknown = value::partial_new_with_keywords(
            target,
            Vec::new(),
            vec![("missing".to_string(), int(1))],
        );
        let both = [int(1), int(2)];
        assert_eq!(call(unknown, 2, both.as_ptr()), NONE);
        assert_eq!(
            pending_message(),
            "pair() got an unexpected keyword argument 'missing'"
        );
        exc::init();

        // A callable without parameter names rejects the keywords.
        let native = value::builtin_function_new(Builtin::Str);
        let rejected = value::partial_new_with_keywords(
            native,
            Vec::new(),
            vec![("name".to_string(), int(1))],
        );
        assert_eq!(call(rejected, 0, std::ptr::null()), NONE);
        assert_eq!(pending_message(), "str() does not accept keyword arguments");
        exc::init();
    }

    #[test]
    fn hash_matches_equal_values_and_rejects_unhashables() {
        exc::init();
        let one = builtin_call(Builtin::Hash, &[int(1)]);
        assert_eq!(one, builtin_call(Builtin::Hash, &[value::float_new(1.0)]));
        assert_eq!(one, builtin_call(Builtin::Hash, &[TRUE]));
        assert_eq!(
            builtin_call(
                Builtin::Hash,
                &[value::tuple_new(vec![int(1), string("x")])]
            ),
            builtin_call(
                Builtin::Hash,
                &[value::tuple_new(vec![int(1), string("x")])]
            )
        );
        assert!(value::as_integer(one).is_some_and(|hash| hash >= 0));
        assert!(!exc::pending());

        exc::init();
        assert_eq!(
            builtin_call(Builtin::Hash, &[value::list_new(Vec::new())]),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(pending_message(), "unhashable type: 'list'");

        exc::init();
        assert_eq!(
            builtin_call(Builtin::Hash, &[value::dict_new(Vec::new())]),
            NONE
        );
        assert_eq!(pending_message(), "unhashable type: 'dict'");

        // A tuple containing an unhashable element is unhashable too.
        exc::init();
        assert_eq!(
            builtin_call(
                Builtin::Hash,
                &[value::tuple_new(vec![value::list_new(Vec::new())])]
            ),
            NONE
        );
        assert_eq!(pending_message(), "unhashable type: 'list'");

        exc::init();
        assert_eq!(builtin_call(Builtin::Hash, &[]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    fn id_is_stable_and_matches_the_builtins_module() {
        // Identity values are integers; compare them by value because an
        // out-of-range immediate id is a heap `BigInt`.
        let same_id = |left: i64, right: i64| ops::binop(BinOp::Eq.code(), left, right) == TRUE;
        exc::init();
        let immediate = int(7);
        let immediate_id = builtin_call(Builtin::Id, &[immediate]);
        assert!(same_id(
            immediate_id,
            builtin_call(Builtin::Id, &[immediate])
        ));
        // Immediates are mapped injectively, so distinct values differ.
        assert!(!same_id(immediate_id, builtin_call(Builtin::Id, &[int(8)])));
        assert!(!exc::pending());

        // Heap values keep one identity for their lifetime and never share it
        // with a different object.
        let first = value::list_new(Vec::new());
        let second = value::list_new(Vec::new());
        let roots = crate::heap::root_values(&[first, second]);
        let first_id = builtin_call(Builtin::Id, &[first]);
        assert!(same_id(first_id, builtin_call(Builtin::Id, &[first])));
        assert!(!same_id(first_id, builtin_call(Builtin::Id, &[second])));
        // The `builtins.id` spelling returns the same number.
        assert_eq!(first_id, crate::modules::builtins::call("id", &[first]));
        crate::heap::release_roots(roots);
        assert!(!exc::pending());

        // Arity is enforced.
        exc::init();
        assert_eq!(builtin_call(Builtin::Id, &[]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    fn ord_returns_the_scalar_value() {
        exc::init();
        assert_eq!(
            value::as_integer(builtin_call(Builtin::Ord, &[string("A")])),
            Some(65)
        );
        assert_eq!(
            value::as_integer(builtin_call(Builtin::Ord, &[string("é")])),
            Some(233)
        );
        assert_eq!(
            value::as_integer(builtin_call(Builtin::Ord, &[string("😀")])),
            Some(0x1F600)
        );
        assert!(!exc::pending());

        exc::init();
        assert_eq!(builtin_call(Builtin::Ord, &[string("ab")]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "ord() expected a character, but string of length 2 found"
        );

        exc::init();
        assert_eq!(builtin_call(Builtin::Ord, &[string("")]), NONE);
        assert_eq!(
            pending_message(),
            "ord() expected a character, but string of length 0 found"
        );

        exc::init();
        assert_eq!(builtin_call(Builtin::Ord, &[int(65)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "ord() expected string of length 1, but int found"
        );
    }

    #[test]
    fn chr_returns_a_one_character_string() {
        exc::init();
        for (code, expected) in [(0, "\u{0}"), (65, "A"), (233, "é"), (0x1F600, "😀")] {
            let text = builtin_call(Builtin::Chr, &[int(code)]);
            assert!(!exc::pending());
            assert_eq!(value::as_str(text), Some(expected));
        }

        exc::init();
        assert_eq!(builtin_call(Builtin::Chr, &[int(-1)]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(pending_message(), "chr() arg not in range(0x110000)");

        exc::init();
        assert_eq!(builtin_call(Builtin::Chr, &[int(0x110000)]), NONE);
        assert_eq!(pending_message(), "chr() arg not in range(0x110000)");

        exc::init();
        assert_eq!(builtin_call(Builtin::Chr, &[int(0xD800)]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "chr() arg cannot be a surrogate code point"
        );

        exc::init();
        assert_eq!(builtin_call(Builtin::Chr, &[string("A")]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "chr() argument must be an int, not 'str'"
        );
    }

    #[test]
    fn ord_and_chr_round_trip() {
        exc::init();
        for text in ["A", "é", "😀"] {
            let ordinal = builtin_call(Builtin::Ord, &[string(text)]);
            let code = value::as_integer(ordinal).expect("ord returns an int");
            let round_tripped = builtin_call(Builtin::Chr, &[int(code)]);
            assert!(!exc::pending());
            assert_eq!(value::as_str(round_tripped), Some(text));
        }
    }

    #[test]
    fn divmod_returns_floored_quotient_and_remainder() {
        exc::init();
        for ((a, b), (quotient, remainder)) in [
            ((7, 3), (2, 1)),
            ((7, -3), (-3, -2)),
            ((-7, 3), (-3, 2)),
            ((-7, -3), (2, -1)),
            ((6, 3), (2, 0)),
        ] {
            let result = builtin_call(Builtin::Divmod, &[int(a), int(b)]);
            assert!(!exc::pending(), "divmod({a}, {b}) raised");
            let items = value::as_tuple(result).expect("divmod returns a tuple");
            assert_eq!(items.len(), 2);
            assert_eq!(value::as_integer(items[0]), Some(quotient));
            assert_eq!(value::as_integer(items[1]), Some(remainder));
        }

        // Floats keep CPython's fmod adjustment and produce floats.
        let result = builtin_call(Builtin::Divmod, &[value::float_new(-7.5), int(2)]);
        assert!(!exc::pending());
        let items = value::as_tuple(result).expect("divmod returns a tuple");
        assert_eq!(value::as_float(items[0]), Some(-4.0));
        assert_eq!(value::as_float(items[1]), Some(0.5));

        // A zero divisor is ZeroDivisionError for both numeric kinds.
        assert_eq!(builtin_call(Builtin::Divmod, &[int(1), int(0)]), NONE);
        assert_eq!(exc::pending_tag(), ZERO_DIVISION_ERROR as i64);
        assert_eq!(pending_message(), "division by zero");
        exc::clear();
        assert_eq!(
            builtin_call(Builtin::Divmod, &[value::float_new(1.0), int(0)]),
            NONE
        );
        assert_eq!(exc::pending_tag(), ZERO_DIVISION_ERROR as i64);
        exc::clear();

        // Non-numeric operands use Python's divmod() message.
        assert_eq!(builtin_call(Builtin::Divmod, &[string("a"), int(2)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "unsupported operand type(s) for divmod(): 'str' and 'int'"
        );

        exc::clear();
        assert_eq!(builtin_call(Builtin::Divmod, &[int(1)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "divmod() takes exactly 2 argument(s) (1 given)"
        );
    }

    #[test]
    fn callable_recognises_function_values_and_classes() {
        exc::init();
        let function = value::builtin_function_new(Builtin::Len);
        assert_eq!(builtin_call(Builtin::Callable, &[function]), value::TRUE);
        let class = value::class_new("Thing".to_string(), NONE, Vec::new());
        assert_eq!(builtin_call(Builtin::Callable, &[class]), value::TRUE);

        for value in [int(1), string("text"), NONE, value::list_new(Vec::new())] {
            assert_eq!(builtin_call(Builtin::Callable, &[value]), value::FALSE);
            assert!(!exc::pending());
        }

        assert_eq!(builtin_call(Builtin::Callable, &[]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "callable() takes exactly 1 argument(s) (0 given)"
        );
    }

    #[test]
    fn input_lines_drop_the_trailing_newline() {
        assert_eq!(strip_input_newline("value\n".to_string()), "value");
        assert_eq!(strip_input_newline("value\r\n".to_string()), "value");
        assert_eq!(strip_input_newline("value".to_string()), "value");
        assert_eq!(strip_input_newline("".to_string()), "");
        assert_eq!(strip_input_newline("\n".to_string()), "");
    }

    #[test]
    fn input_rejects_a_non_string_prompt() {
        exc::init();
        assert_eq!(input_value(Some(int(1))), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "input(): argument must be str, not 'int'"
        );
        // Arity is enforced by the builtin dispatcher.
        assert_eq!(
            builtin_call(Builtin::Input, &[string("a"), string("b")]),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    fn get_ipython_reports_a_terminal_shell() {
        exc::init();
        let shell = builtin_call(Builtin::GetIpython, &[]);
        assert!(!exc::pending());
        let class = crate::methods::getattr(shell, "__class__".as_ptr(), 9);
        assert!(!exc::pending());
        let name = crate::methods::getattr(class, "__name__".as_ptr(), 8);
        assert_eq!(rendered(name), "TerminalInteractiveShell");
        let text = rendered(shell);
        assert!(!text.contains("ZMQInteractiveShell"), "{text}");
        assert!(!text.contains("google.colab"), "{text}");
        // The probe takes no arguments.
        assert_eq!(builtin_call(Builtin::GetIpython, &[int(1)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    /// Arbitrary-precision literal through the normalizing constructor.
    fn big(text: &str) -> i64 {
        value::bigint_new(text.parse::<num_bigint::BigInt>().expect("big literal"))
    }

    #[test]
    fn bigint_conversions_preserve_exact_values() {
        exc::init();
        // `int(BigInt)` and `int(str)` keep the exact value.
        let huge = big("-123456789012345678901234567890");
        assert_eq!(rendered(to_int(huge)), "-123456789012345678901234567890");

        let from_text = to_int(string("123456789012345678901234567890"));
        assert!(value::is_bigint(from_text));
        assert_eq!(rendered(from_text), "123456789012345678901234567890");

        // An explicit base stays exact as well.
        let base16 = builtin_call(Builtin::Int, &[string("ffffffffffffffffffffffff"), int(16)]);
        assert_eq!(rendered(base16), "79228162514264337593543950335");

        // `int(float)` truncates large finite floats exactly.
        assert_eq!(
            rendered(to_int(value::float_new(1e30))),
            "1000000000000000019884624838656"
        );
        // `int()`/`bool()`/`float()` agree with the tagged fast path.
        assert_eq!(value::as_integer(to_int(TRUE)), Some(1));
        assert_eq!(
            value::as_float(to_float(big("100000000000000000000"))),
            Some(1e20)
        );
        assert_eq!(value::bool_value(value::truthy(big("1"))), TRUE);
        assert_eq!(value::bool_value(value::truthy(big("-1"))), TRUE);
        assert!(!exc::pending());

        // Floats that cannot hold the integer raise OverflowError.
        let enormous = big("-100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000");
        assert_eq!(to_float(enormous), NONE);
        assert_eq!(exc::pending_tag(), OVERFLOW_ERROR as i64);
        assert_eq!(pending_message(), "int too large to convert to float");
    }

    #[test]
    fn bigint_abs_divmod_and_hash() {
        exc::init();
        let negative = big("-123456789012345678901234567890");
        assert_eq!(rendered(abs(negative)), "123456789012345678901234567890");

        // divmod uses Python's floored quotient/remainder and stays exact.
        let result = builtin_call(Builtin::Divmod, &[negative, int(3)]);
        assert!(!exc::pending());
        let items = value::as_tuple(result).expect("divmod returns a tuple");
        assert_eq!(rendered(items[0]), "-41152263004115226300411522630");
        assert_eq!(rendered(items[1]), "0");

        let two_100 = big("1267650600228229401496703205376");
        let result = builtin_call(Builtin::Divmod, &[two_100, int(3)]);
        assert!(!exc::pending());
        let items = value::as_tuple(result).expect("divmod returns a tuple");
        assert_eq!(rendered(items[0]), "422550200076076467165567735125");
        assert_eq!(rendered(items[1]), "1");

        // Equal heap BigInts are interchangeable dict/set keys.
        let key = big("123456789012345678901234567890");
        let same = big("123456789012345678901234567890");
        let dict = value::dict_new(vec![(key, int(1)), (same, int(2))]);
        assert_eq!(value::as_dict(dict).map(|pairs| pairs.len()), Some(1));
        assert!(container::dict_find(value::as_dict(dict).expect("dict"), same).is_some());
        assert!(container::dict_find(value::as_dict(dict).expect("dict"), key).is_some());
        assert!(!container::dict_find(
            value::as_dict(dict).expect("dict"),
            big("123456789012345678901234567891")
        )
        .is_some());
        let set = value::set_new(vec![key]);
        assert!(container::set_contains(
            value::as_set(set).expect("set"),
            same
        ));

        // `hash()` agrees for equal values and both fit tagged integers.
        let heap_hash = hash(key);
        assert!(value::is_int(heap_hash));
        assert_eq!(heap_hash, hash(same));
        assert_eq!(rendered(hash(int(7))), rendered(hash(big("7"))));
        assert!(!exc::pending());
    }

    thread_local! {
        static SINK: std::cell::RefCell<(String, usize)> =
            const { std::cell::RefCell::new((String::new(), 0)) };
    }

    extern "C" fn sink_write(args: *const i64) -> i64 {
        // SAFETY: called with `(self, text)`.
        let values = unsafe { std::slice::from_raw_parts(args, 2) };
        SINK.with(|sink| sink.borrow_mut().0.push_str(&value::to_string(values[1])));
        NONE
    }

    extern "C" fn sink_flush(_args: *const i64) -> i64 {
        SINK.with(|sink| sink.borrow_mut().1 += 1);
        NONE
    }

    fn sink_instance() -> i64 {
        let class = value::class_new_full(
            "Sink".to_string(),
            NONE,
            vec![
                (
                    "write".to_string(),
                    value::function_new(sink_write as *const () as usize, 2, "write".to_string()),
                    MethodKind::Normal,
                ),
                (
                    "flush".to_string(),
                    value::function_new(sink_flush as *const () as usize, 1, "flush".to_string()),
                    MethodKind::Normal,
                ),
            ],
            Vec::new(),
        );
        value::instance_new(class)
    }

    #[test]
    fn print_keywords_write_to_file_with_sep_end_and_flush() {
        crate::exc::init();
        SINK.with(|sink| *sink.borrow_mut() = (String::new(), 0));
        let sink = sink_instance();
        let print_function = value::builtin_function_new(Builtin::Print);
        let args = [int(1), string("x")];

        // sep/end/file/flush are honoured; the call returns None.
        let keywords = [
            string("sep"),
            string("-"),
            string("end"),
            string("!"),
            string("file"),
            sink,
            string("flush"),
            TRUE,
        ];
        let result = call_kw(
            print_function,
            args.len(),
            args.as_ptr(),
            4,
            keywords.as_ptr(),
        );
        assert_eq!(result, NONE);
        assert!(!crate::exc::pending());
        SINK.with(|sink| {
            let state = sink.borrow();
            assert_eq!(state.0, "1-x!");
            assert_eq!(state.1, 1);
        });

        // `sep=None`/`end=None` mean the defaults.
        SINK.with(|sink| *sink.borrow_mut() = (String::new(), 0));
        let keywords = [
            string("sep"),
            NONE,
            string("end"),
            NONE,
            string("file"),
            sink,
        ];
        let _ = call_kw(
            print_function,
            args.len(),
            args.as_ptr(),
            3,
            keywords.as_ptr(),
        );
        assert!(!crate::exc::pending());
        SINK.with(|sink| assert_eq!(sink.borrow().0, "1 x\n"));

        // Non-string sep is a tagged TypeError, before any output.
        crate::exc::clear();
        SINK.with(|sink| *sink.borrow_mut() = (String::new(), 0));
        let keywords = [string("sep"), int(1), string("file"), sink];
        let _ = call_kw(
            print_function,
            args.len(),
            args.as_ptr(),
            2,
            keywords.as_ptr(),
        );
        assert_eq!(crate::exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        SINK.with(|sink| assert_eq!(sink.borrow().0, ""));
        crate::exc::clear();

        // Unknown keyword is a tagged TypeError.
        let keywords = [string("bogus"), int(1)];
        let _ = call_kw(
            print_function,
            args.len(),
            args.as_ptr(),
            1,
            keywords.as_ptr(),
        );
        assert_eq!(crate::exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        crate::exc::clear();
    }

    #[test]
    fn globals_view_reads_the_store_and_publishes_dynamic_writes() {
        // The global store is process-wide: hold the runtime-state lock while
        // resetting it and asserting on this module's entries.
        let _heap_guard = crate::heap::heap_reset_test_guard();
        globals::reset();
        exc::init();
        globals::set("mod", "counter", int(7));
        let view = builtin_call(Builtin::Globals, &[string("mod")]);
        assert!(!crate::exc::pending());
        assert_eq!(namespace_get(view, "counter"), Some(int(7)));
        assert_eq!(view, builtin_call(Builtin::Globals, &[string("mod")]));

        // A write through the view reaches the runtime global store on the
        // next `globals()` call, so static reads and `getattr` observe it.
        container::dict_set(view, string("extra"), int(3));
        let _ = builtin_call(Builtin::Globals, &[string("mod")]);
        assert_eq!(globals::try_get("mod", "extra"), Some(int(3)));

        // The frontend's `globals()[key] = value` shape stores directly.
        assert_eq!(
            builtin_call(
                Builtin::GlobalSetDynamic,
                &[string("mod"), string("dyn"), int(5)],
            ),
            NONE
        );
        assert_eq!(globals::try_get("mod", "dyn"), Some(int(5)));

        // A first-class `globals` value has no module context.
        let _ = builtin_call(Builtin::Globals, &[]);
        assert_eq!(crate::exc::pending_tag(), RUNTIME_ERROR as i64);
        crate::exc::clear();
    }

    #[test]
    fn vars_and_dir_expose_real_namespaces() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        exc::init();
        globals::reset();
        let class = value::class_new_full(
            "C".to_string(),
            NONE,
            vec![(
                "method".to_string(),
                value::builtin_function_new(Builtin::Len),
                MethodKind::Normal,
            )],
            vec![("attr".to_string(), int(4))],
        );
        let instance = value::instance_new(class);
        value::instance_setattr(instance, "x".to_string(), int(1));

        let mapping = builtin_call(Builtin::Vars, &[instance]);
        assert_eq!(namespace_get(mapping, "x"), Some(int(1)));

        let names = builtin_call(Builtin::Dir, &[class]);
        let names = value::as_list(names).expect("dir() returns a list");
        assert!(names
            .iter()
            .any(|name| value::as_str(*name) == Some("attr")));
        assert!(names
            .iter()
            .any(|name| value::as_str(*name) == Some("method")));

        // `builtins.__dict__` carries the actual builtin values, including
        // the ones without a central spec entry.
        let dict = modules::module_dict("builtins");
        assert!(namespace_get(dict, "len").is_some());
        assert!(namespace_get(dict, "eval").is_some());
        assert!(namespace_get(dict, "exec").is_some());
        assert_eq!(namespace_get(dict, "None"), Some(NONE));
    }

    #[test]
    fn eval_supports_literals_and_names_and_fails_loudly() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        exc::init();
        globals::reset();
        assert_eq!(
            rendered(builtin_call(Builtin::Eval, &[string("[1, 2]")])),
            "[1, 2]"
        );
        let function = builtin_call(Builtin::Eval, &[string("len")]);
        assert_eq!(
            value::as_function(function).map(|function| function.name.as_str()),
            Some("len")
        );
        // A name the builtins do not define is a real `NameError` (probe code
        // such as pydantic's `in_ipython` relies on it).
        let _ = builtin_call(Builtin::Eval, &[string("__IPYTHON__")]);
        assert_eq!(exc::pending_tag(), NAME_ERROR as i64);
        exc::clear();
        // Arithmetic, attributes, subscripts and caller namespaces are
        // evaluated without a compiler.
        assert_eq!(
            value::as_integer(builtin_call(Builtin::Eval, &[string("1 + 2 * 3")])),
            Some(7)
        );
        let namespace = value::dict_new(vec![(string("value"), int(5))]);
        assert_eq!(
            value::as_integer(builtin_call(
                Builtin::Eval,
                &[string("value + 1"), namespace],
            )),
            Some(6)
        );
        assert_eq!(
            value::as_integer(builtin_call(
                Builtin::Eval,
                &[string("{'k': 2}['k']"), NONE, NONE],
            )),
            Some(2)
        );
        // A call requires the full call protocol, which the runtime
        // expression evaluator does not implement; it says so honestly.
        let _ = builtin_call(Builtin::Eval, &[string("len([1])")]);
        assert_eq!(exc::pending_tag(), NOT_IMPLEMENTED_ERROR as i64);
        exc::clear();
        let _ = builtin_call(Builtin::Exec, &[string("x = 1")]);
        assert_eq!(exc::pending_tag(), NOT_IMPLEMENTED_ERROR as i64);
        exc::clear();
        let _ = builtin_call(
            Builtin::Compile,
            &[string("x = 1"), string("<test>"), string("exec")],
        );
        assert_eq!(exc::pending_tag(), NOT_IMPLEMENTED_ERROR as i64);
        exc::clear();
    }

    /// Runs the bounded `exec` compiler and returns the locals namespace.
    fn install_exec(source: &str, globals: i64, locals: i64) -> i64 {
        let _ = builtin_call(Builtin::Exec, &[string(source), globals, locals]);
        assert!(!exc::pending(), "{}", pending_message());
        locals
    }

    fn installed_function(namespace: i64, name: &str) -> i64 {
        namespace_get(namespace, name).expect("exec installed the function")
    }

    fn call_one(function: i64, argument: i64) -> i64 {
        call(function, 1, [argument].as_ptr())
    }

    #[test]
    fn exec_compiles_the_typing_inspection_check_forms() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        globals::reset();
        let target = value::list_new(vec![int(1)]);
        let other = value::list_new(vec![int(2)]);
        let third = value::list_new(vec![int(3)]);

        let globals = value::dict_new(vec![(string("_obj"), target)]);
        let locals = value::dict_new(Vec::new());
        install_exec(
            "def is_obj(obj: Any, /) -> bool:\n    return obj is _obj",
            globals,
            locals,
        );
        let function = installed_function(locals, "is_obj");
        assert_eq!(value::function_name(function), "is_obj");
        assert_eq!(value::function_posonly_count(function), 1);
        assert_eq!(call_one(function, target), TRUE);
        assert_eq!(call_one(function, other), FALSE);

        // Either identity check, as generated when `typing` and
        // `typing_extensions` expose different objects.
        let globals = value::dict_new(vec![(string("_t_obj"), target), (string("_te_obj"), other)]);
        let locals = value::dict_new(Vec::new());
        install_exec(
            "def is_either(obj: Any, /) -> bool:\n    return obj is _t_obj or obj is _te_obj",
            globals,
            locals,
        );
        let function = installed_function(locals, "is_either");
        assert_eq!(call_one(function, target), TRUE);
        assert_eq!(call_one(function, other), TRUE);
        assert_eq!(call_one(function, third), FALSE);

        // `isinstance` against one cached type and against a tuple of types.
        let int_type = value::type_class(int(1));
        let str_type = value::type_class(string("x"));
        let globals = value::dict_new(vec![(string("_obj"), int_type)]);
        let locals = value::dict_new(Vec::new());
        install_exec(
            "\ndef is_int(obj: Any, /) -> 'TypeIs[int]':\n    return isinstance(obj, _obj)\n",
            globals,
            locals,
        );
        let function = installed_function(locals, "is_int");
        assert_eq!(call_one(function, int(3)), TRUE);
        assert_eq!(call_one(function, string("3")), FALSE);

        let globals = value::dict_new(vec![(
            string("_objs"),
            value::tuple_new(vec![int_type, str_type]),
        )]);
        let locals = value::dict_new(Vec::new());
        install_exec(
            "def is_int_or_str(obj: Any, /) -> bool:\n    return isinstance(obj, _objs)",
            globals,
            locals,
        );
        let function = installed_function(locals, "is_int_or_str");
        assert_eq!(call_one(function, int(3)), TRUE);
        assert_eq!(call_one(function, string("3")), TRUE);
        assert_eq!(call_one(function, value::list_new(Vec::new())), FALSE);

        // The fallback form when neither module exposes the member.
        let globals = value::dict_new(Vec::new());
        let locals = value::dict_new(Vec::new());
        install_exec(
            "def never(obj: Any, /) -> bool:\n    return False",
            globals,
            locals,
        );
        let function = installed_function(locals, "never");
        assert_eq!(call_one(function, target), FALSE);
    }

    #[test]
    fn exec_resolves_names_from_globals_and_locals_and_reports_errors() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        globals::reset();
        let target = value::list_new(vec![int(1)]);

        // The name lives only in the explicit locals mapping.
        let globals = value::dict_new(Vec::new());
        let locals = value::dict_new(vec![(string("_obj"), target)]);
        install_exec(
            "def from_locals(obj: Any, /) -> bool:\n    return obj is _obj",
            globals,
            locals,
        );
        let function = installed_function(locals, "from_locals");
        assert_eq!(call_one(function, target), TRUE);

        // The name lives only in globals.
        let globals = value::dict_new(vec![(string("_obj"), target)]);
        let locals = value::dict_new(Vec::new());
        install_exec(
            "def from_globals(obj: Any, /) -> bool:\n    return obj is _obj",
            globals,
            locals,
        );
        let function = installed_function(locals, "from_globals");
        assert_eq!(call_one(function, target), TRUE);

        // With locals omitted the definition is installed in globals.
        let globals = value::dict_new(vec![(string("_obj"), target)]);
        let _ = builtin_call(
            Builtin::Exec,
            &[
                string("def in_globals(obj: Any, /) -> bool:\n    return obj is _obj"),
                globals,
            ],
        );
        assert!(!exc::pending(), "{}", pending_message());
        let function = installed_function(globals, "in_globals");
        assert_eq!(call_one(function, target), TRUE);

        // A missing name is a real NameError when the function runs.
        let globals = value::dict_new(Vec::new());
        let locals = value::dict_new(Vec::new());
        install_exec(
            "def missing(obj: Any, /) -> bool:\n    return obj is _missing",
            globals,
            locals,
        );
        let function = installed_function(locals, "missing");
        assert_eq!(call_one(function, target), NONE);
        assert_eq!(exc::pending_tag(), NAME_ERROR as i64);
        assert_eq!(pending_message(), "name '_missing' is not defined");
        exc::clear();

        // Arity and keyword calls are rejected, not silently accepted.
        assert_eq!(call(function, 0, std::ptr::null()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        let two = [target, target];
        assert_eq!(call(function, 2, two.as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        let keyword = [string("obj"), target];
        assert_eq!(
            call_kw(function, 0, std::ptr::null(), 1, keyword.as_ptr()),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        // Non-dict namespaces and non-string source are type errors.
        let _ = builtin_call(
            Builtin::Exec,
            &[string("def f(obj, /): return False"), int(1)],
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        let _ = builtin_call(Builtin::Exec, &[int(1)]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn exec_function_is_a_real_function_and_rejects_unsupported_source() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        globals::reset();
        let globals = value::dict_new(vec![(string("_obj"), value::list_new(vec![int(1)]))]);
        let locals = value::dict_new(Vec::new());
        install_exec(
            "def is_obj(obj: Any, /) -> bool:\n    return obj is _obj",
            globals,
            locals,
        );
        let function = installed_function(locals, "is_obj");

        // `type(f)` is the runtime's cached `function` class, and the function
        // accepts the writable metadata `typing_inspection` assigns.
        let class = builtin_call(Builtin::Type, &[function]);
        assert!(value::as_class(class).is_some());
        assert_eq!(value::class_display_name(class), Some("function"));
        assert!(value::function_setattr(function, "__doc__", string("doc")));
        assert_eq!(rendered(value::function_doc(function)), "doc");
        assert_eq!(
            value::function_signature(function)
                .iter()
                .map(|param| param.kind)
                .collect::<Vec<_>>(),
            vec![ParamKind::PosOnly]
        );

        for source in [
            "x = 1",
            "def f(obj, /):\n    pass",
            "def f(obj, /):\n    return obj.attr",
            "def f(obj, /):\n    return obj is _obj\n    x = 1",
            "def f(obj):\n    return obj is _obj",
            "def f(obj, other, /):\n    return obj is _obj",
        ] {
            let _ = builtin_call(
                Builtin::Exec,
                &[
                    string(source),
                    value::dict_new(Vec::new()),
                    value::dict_new(Vec::new()),
                ],
            );
            assert_eq!(
                exc::pending_tag(),
                NOT_IMPLEMENTED_ERROR as i64,
                "source should be rejected: {source}"
            );
            let message = pending_message();
            assert!(
                message.contains("exec() cannot compile Python source at runtime in Hydro"),
                "{message}"
            );
            exc::clear();
        }
    }

    /// Builds a function whose only reference to the globals mapping is the
    /// function's own GC edge.
    #[inline(never)]
    fn build_exec_gc_function() -> i64 {
        let target = value::list_new(vec![int(1)]);
        let globals = value::dict_new(vec![(string("_obj"), target)]);
        let locals = value::dict_new(Vec::new());
        let source = string("def gc_check(obj: Any, /) -> bool:\n    return obj is _obj");
        let _ = builtin_call(Builtin::Exec, &[source, globals, locals]);
        assert!(!exc::pending(), "{}", pending_message());
        namespace_get(locals, "gc_check").expect("installed")
    }

    /// Overwrites stale tagged values left by [`build_exec_gc_function`] so
    /// the conservative scan cannot keep the mapping alive by accident.
    #[inline(never)]
    fn scrub_exec_stack() {
        let mut buffer = [0u8; 64 * 1024];
        std::hint::black_box(&mut buffer);
        buffer[0] = 0;
        std::hint::black_box(&buffer);
    }

    #[test]
    fn exec_function_keeps_its_globals_mapping_alive() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        globals::reset();
        let function = build_exec_gc_function();
        let roots = crate::heap::root_values(&[function]);
        scrub_exec_stack();
        for _ in 0..128 {
            let _ = string("garbage");
        }
        crate::heap::collect();

        let func = value::as_function(function).expect("function value");
        let (globals_ns, locals_ns) = match &func.body {
            FunctionBody::Exec {
                globals, locals, ..
            } => (*globals, *locals),
            _ => panic!("expected an exec-compiled function"),
        };
        let target = namespace_get(globals_ns, "_obj").expect("globals entry survived");
        assert_eq!(namespace_get(locals_ns, "gc_check"), Some(function));
        assert_eq!(call_one(function, target), TRUE);
        assert!(!exc::pending(), "{}", pending_message());
        let other = value::list_new(vec![int(2)]);
        assert_eq!(call_one(function, other), FALSE);
        crate::heap::release_roots(roots);
    }

    #[test]
    fn class_from_namespace_binds_functions_as_normal_methods() {
        exc::init();
        let namespace = value::dict_new(Vec::new());
        container::dict_set(namespace, string("answer"), int(42));
        container::dict_set(
            namespace,
            string("size"),
            value::builtin_function_new(Builtin::Len),
        );
        let class = builtin_call(
            Builtin::ClassFromNamespace,
            &[string("K"), value::tuple_new(Vec::new()), NONE, namespace],
        );
        assert!(!crate::exc::pending());
        assert!(matches!(
            value::class_lookup(class, "answer"),
            Some(value::ClassEntry::Attr(value)) if value == int(42)
        ));
        assert!(matches!(
            value::class_lookup(class, "size"),
            Some(value::ClassEntry::Method(MethodKind::Normal, _))
        ));
    }

    #[test]
    fn deferred_import_resolves_registered_modules_by_exact_name() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        globals::reset();
        exc::init();
        // A compiled module is registered by the `__name__` global its
        // initializer stores; submodules are ordinary dotted entries.
        globals::set("pkg", "__name__", string("pkg"));
        globals::set("pkg.sub", "__name__", string("pkg.sub"));
        globals::set("pkg.sub", "value", int(7));

        // Module alias (`import pkg.sub as s`).
        let module = deferred_import("pkg.sub", None, None);
        assert!(!exc::pending());
        assert_eq!(value::as_module(module).map(|m| m.name()), Some("pkg.sub"));

        // `from pkg import sub` returns the preloaded submodule, not an
        // attribute of the package.
        let submodule = deferred_import("pkg", Some("sub"), None);
        assert!(!exc::pending());
        assert_eq!(
            value::as_module(submodule).map(|m| m.name()),
            Some("pkg.sub")
        );

        // `from pkg.sub import value` returns the module attribute.
        let attribute = deferred_import("pkg.sub", Some("value"), None);
        assert!(!exc::pending());
        assert_eq!(value::as_integer(attribute), Some(7));

        // A member the package does not provide is a failed from-import: the
        // module exists, so the error is `ImportError`, exactly like CPython.
        exc::clear();
        assert_eq!(deferred_import("pkg", Some("missing"), None), NONE);
        assert_eq!(exc::pending_tag(), exc::IMPORT_ERROR as i64);
        assert_eq!(pending_message(), "cannot import name 'missing' from 'pkg'");

        // A missing module alias reports its own exact name.
        exc::clear();
        assert_eq!(deferred_import("nowhere", None, None), NONE);
        assert_eq!(exc::pending_tag(), exc::MODULE_NOT_FOUND_ERROR as i64);
        assert_eq!(pending_message(), "No module named 'nowhere'");

        globals::reset();
        exc::init();
    }

    #[test]
    fn deferred_import_reports_missing_native_members_as_import_errors() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        globals::reset();
        exc::init();

        // `from _socket import NOPE`: the native module exists, so a missing
        // member is an `ImportError` that an `except ImportError` handler
        // catches, not an `AttributeError`.
        assert_eq!(deferred_import("_socket", Some("NOPE"), None), NONE);
        assert_eq!(exc::pending_tag(), exc::IMPORT_ERROR as i64);
        assert_eq!(
            pending_message(),
            "cannot import name 'NOPE' from '_socket'"
        );
        exc::clear();

        // A member the spec provides still resolves to its runtime value.
        let function = deferred_import("_socket", Some("gethostname"), None);
        assert!(!exc::pending());
        assert!(value::as_function(function).is_some());

        globals::reset();
        exc::init();
    }

    #[test]
    fn deferred_import_of_an_extension_module_value_keeps_its_path() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        globals::reset();
        exc::init();

        // Without a loadable shared object, the module value itself is still
        // materialised lazily under its import name.
        let module = deferred_import("ext", None, Some("/nonexistent/ext.abi3.so"));
        assert!(!exc::pending());
        assert_eq!(value::as_module(module).map(|m| m.name()), Some("ext"));
        exc::clear();

        // `from ext import member` must load the shared object; a failure is
        // an `ImportError` (not the `RuntimeError` a plain attribute read
        // reports), so an optional-import handler catches it.
        assert_eq!(
            deferred_import("ext", Some("NOPE"), Some("/nonexistent/ext.abi3.so")),
            NONE
        );
        assert_eq!(exc::pending_tag(), exc::IMPORT_ERROR as i64);

        globals::reset();
        exc::init();
    }

    #[test]
    fn cannot_import_name_reports_the_cpython_message() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        globals::reset();
        exc::init();

        assert_eq!(cannot_import_name("NOPE", "_socket"), NONE);
        assert_eq!(exc::pending_tag(), exc::IMPORT_ERROR as i64);
        assert_eq!(
            pending_message(),
            "cannot import name 'NOPE' from '_socket'"
        );

        globals::reset();
        exc::init();
    }

    extern "C" fn int_probe_seven(_argv: *const i64) -> i64 {
        value::encode_int(7).expect("representable")
    }

    extern "C" fn int_probe_nine(_argv: *const i64) -> i64 {
        value::encode_int(9).expect("representable")
    }

    extern "C" fn int_probe_float(_argv: *const i64) -> i64 {
        value::float_new(2.5)
    }

    extern "C" fn int_probe_true(_argv: *const i64) -> i64 {
        TRUE
    }

    fn int_method_class(name: &str, methods: &[(&str, extern "C" fn(*const i64) -> i64)]) -> i64 {
        let entries = methods
            .iter()
            .map(|(method, probe)| {
                (
                    method.to_string(),
                    value::function_new(*probe as *const () as usize, 1, method.to_string()),
                )
            })
            .collect();
        value::class_new(name.to_string(), NONE, entries)
    }

    #[test]
    fn int_uses_the_int_and_index_protocols() {
        exc::init();
        // `__int__` is preferred and its result is the integer.
        let ipv4_like = int_method_class("IPv4Like", &[("__int__", int_probe_seven)]);
        let instance = value::instance_new(ipv4_like);
        assert_eq!(as_int(builtin_call(Builtin::Int, &[instance])), 7);

        // `__index__` is the fallback when `__int__` is absent.
        let indexed = int_method_class("Indexed", &[("__index__", int_probe_nine)]);
        let instance = value::instance_new(indexed);
        assert_eq!(as_int(builtin_call(Builtin::Int, &[instance])), 9);

        // `__int__` wins over `__index__`.
        let both = int_method_class(
            "Both",
            &[("__int__", int_probe_seven), ("__index__", int_probe_nine)],
        );
        assert_eq!(
            as_int(builtin_call(Builtin::Int, &[value::instance_new(both)])),
            7
        );

        // A non-int result is the protocol's TypeError.
        let bad = int_method_class("BadInt", &[("__int__", int_probe_float)]);
        assert_eq!(
            builtin_call(Builtin::Int, &[value::instance_new(bad)]),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(pending_message(), "__int__ returned non-int (type float)");
        exc::clear();

        // CPython still accepts `bool` (deprecated); convert silently.
        let boolean = int_method_class("BoolInt", &[("__int__", int_probe_true)]);
        assert_eq!(
            as_int(builtin_call(Builtin::Int, &[value::instance_new(boolean)])),
            1
        );

        // No protocol: the generic message names the class.
        let plain = value::instance_new(value::class_new("Plain".to_string(), NONE, Vec::new()));
        assert_eq!(builtin_call(Builtin::Int, &[plain]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert!(pending_message().ends_with("not 'Plain'"));
        exc::clear();

        // A class object converts through its metaclass's `__int__`.
        let meta = int_method_class("Meta", &[("__int__", int_probe_seven)]);
        let class = value::class_new_multi(
            "MetaInstance".to_string(),
            Vec::new(),
            meta,
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(as_int(builtin_call(Builtin::Int, &[class])), 7);

        // `int("10", base=2)`: the only keyword CPython accepts.
        let int_fn = value::builtin_function_new(Builtin::Int);
        let mut words = [string("base"), int(2)];
        assert_eq!(
            as_int(call_kw(
                int_fn,
                1,
                [string("10")].as_ptr(),
                1,
                words.as_mut_ptr()
            )),
            2
        );
        // `int(base=2)` has no string operand.
        assert_eq!(
            call_kw(int_fn, 0, std::ptr::null(), 1, words.as_mut_ptr()),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(pending_message(), "int() missing string argument");
        exc::clear();
        // An unexpected keyword is rejected by name.
        let mut words = [string("encoding"), string("utf-8")];
        assert_eq!(
            call_kw(int_fn, 1, [string("10")].as_ptr(), 1, words.as_mut_ptr()),
            NONE
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "int() got an unexpected keyword argument 'encoding'"
        );
        exc::clear();

        // An explicit base never uses the protocol.
        let instance = value::instance_new(ipv4_like);
        assert_eq!(builtin_call(Builtin::Int, &[instance, int(10)]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "int() can't convert non-string with explicit base"
        );
    }

    #[test]
    fn bytes_accepts_general_iterables() {
        exc::init();
        // A range is not a list/tuple/set, so it exercises the iterator path.
        let from_range = builtin_call(Builtin::Bytes, &[value::range_new(65, 68, 1)]);
        assert_eq!(value::as_bytes(from_range), Some(&b"ABC"[..]));
        // A stepped range follows the range values, not the index:
        // 65, 67, 69 -> b"ACE".
        let stepped = builtin_call(Builtin::Bytes, &[value::range_new(65, 70, 2)]);
        assert_eq!(value::as_bytes(stepped), Some(&b"ACE"[..]));
        // An element above 255 is CPython's ValueError.
        assert_eq!(
            builtin_call(Builtin::Bytes, &[value::range_new(250, 258, 1)]),
            NONE
        );
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(pending_message(), "bytes must be in range(0, 256)");
        exc::clear();

        // Mapping iteration yields keys, exactly like CPython; the key must
        // be integer-convertible (`bytes({'A': 1})` is a TypeError there).
        let mapping = value::dict_new(vec![(int(65), int(1))]);
        let from_dict = builtin_call(Builtin::Bytes, &[mapping]);
        assert_eq!(value::as_bytes(from_dict), Some(&b"A"[..]));

        // An element that is not index-convertible reports its own type.
        let bad = value::list_new(vec![string("x")]);
        assert_eq!(builtin_call(Builtin::Bytes, &[bad]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "'str' object cannot be interpreted as an integer"
        );
        exc::clear();

        // A non-iterable reports the constructor's message, not the iterator's.
        let obj = value::instance_new(value::class_new(
            "NotIterable".to_string(),
            NONE,
            Vec::new(),
        ));
        assert_eq!(builtin_call(Builtin::Bytes, &[obj]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "cannot convert 'NotIterable' object to bytes"
        );
    }

    #[test]
    fn bytes_fromhex_matches_cpython() {
        exc::init();
        assert_eq!(value::as_bytes(bytes_fromhex(string(""))), Some(&b""[..]));
        assert_eq!(
            value::as_bytes(bytes_fromhex(string(" 00\t0a\n0B\r0c\x0b"))),
            Some(&[0x00, 0x0a, 0x0b, 0x0c][..])
        );
        // Bytes-like operands use byte positions in error messages.
        let bytes_operand = value::bytes_new(b"00 01".to_vec());
        assert_eq!(
            value::as_bytes(bytes_fromhex(bytes_operand)),
            Some(&[0x00, 0x01][..])
        );
        let buffer = value::bytearray_new(b"fF".to_vec());
        assert_eq!(value::as_bytes(bytes_fromhex(buffer)), Some(&[0xff][..]));

        // Whitespace inside a pair is the whitespace character's error.
        assert_eq!(bytes_fromhex(string("0 0")), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "non-hexadecimal number found in fromhex() arg at position 1"
        );
        exc::clear();

        assert_eq!(bytes_fromhex(string("000")), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "fromhex() arg must contain an even number of hexadecimal digits"
        );
        exc::clear();

        // A non-ASCII character is reported at its own index.
        assert_eq!(bytes_fromhex(string("00\u{a0}01")), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "non-hexadecimal number found in fromhex() arg at position 2"
        );
        exc::clear();

        assert_eq!(bytes_fromhex(int(3)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "fromhex() argument must be str or bytes-like, not int"
        );
    }

    #[test]
    fn float_fromhex_matches_cpython() {
        exc::init();
        // Hex digits, an optional `0x`, and the binary exponent.
        assert_eq!(
            value::as_float(float_fromhex(string("0x1.a934f0979a371p-2"))),
            Some(0.4152410118609203)
        );
        // Without `0x`, digits are still hexadecimal: `1.5` is 1 + 5/16 and
        // `1e3` is 0x1e3.
        assert_eq!(value::as_float(float_fromhex(string("1.5"))), Some(1.3125));
        assert_eq!(value::as_float(float_fromhex(string("1e3"))), Some(483.0));
        assert_eq!(
            value::as_float(float_fromhex(string("  +0x1p3  "))),
            Some(8.0)
        );
        assert_eq!(
            value::as_float(float_fromhex(string("-inf"))),
            Some(f64::NEG_INFINITY)
        );
        assert!(value::as_float(float_fromhex(string("nan"))).is_some_and(f64::is_nan));

        for source in ["", "  ", "0x", ".", "0x1p", "0x1p+", "1__0", "1 2", "nanx"] {
            assert_eq!(float_fromhex(string(source)), NONE, "{source:?}");
            assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
            assert_eq!(
                pending_message(),
                "invalid hexadecimal floating-point string"
            );
            exc::clear();
        }
        // Only `str` is accepted.
        assert_eq!(float_fromhex(int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "bad argument type for built-in operation"
        );
    }

    /// A native scalar subclass allocates an instance that carries the wrapped
    /// scalar payload: `class N(int): pass; N(3)`.
    #[test]
    fn scalar_subclass_constructs_and_behaves_like_its_builtin() {
        crate::exc::init();
        let int_class = value::native_class("int");
        let class = value::class_new_multi(
            "N".to_string(),
            vec![int_class],
            NONE,
            Vec::new(),
            Vec::new(),
        );
        let args = [int(3)];
        let instance = call(class, 1, args.as_ptr());
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(
            value::as_instance(instance).map(|state| state.class),
            Some(class)
        );
        assert_eq!(value::as_integer(instance), Some(3));
        assert_eq!(value::type_class(instance), class);
        assert_eq!(value::repr(instance), "3");
        assert_eq!(value::to_string(instance), "3");
        assert_eq!(
            value::as_integer(builtin_call(Builtin::Int, &[instance])),
            Some(3)
        );
        assert!(crate::container::equals(instance, int(3)));
        assert_eq!(hash(instance), hash(int(3)));
        // Operator dispatch must not fall back to `object.__eq__` identity.
        assert_eq!(
            crate::ops::binop(crate::ops::OP_EQ, instance, int(3)),
            value::TRUE
        );
        assert_eq!(
            crate::ops::binop(crate::ops::OP_NE, instance, int(3)),
            value::FALSE
        );
        assert_eq!(
            value::as_integer(crate::ops::binop(crate::ops::OP_ADD, instance, int(1))),
            Some(4)
        );
        assert!(value::truthy(builtin_call(
            Builtin::Isinstance,
            &[instance, int_class]
        )));
        // The source spelling `isinstance(x, int)` uses the builtin value.
        assert!(value::truthy(builtin_call(
            Builtin::Isinstance,
            &[instance, value::builtin_function_new(Builtin::Int)]
        )));
        assert!(value::truthy(instance));
        assert_eq!(
            value::as_str(format_builtin(instance, string("04d"), 0)),
            Some("0003")
        );

        // A subclass with no own `__init__` inherits `object.__init__`, which
        // must ignore the arguments the scalar constructor already consumed.
        assert!(!exc::pending());

        // Invalid construction keeps the scalar constructor's error.
        let too_many = [int(1), int(2), int(3)];
        assert_eq!(call(class, 3, too_many.as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert!(pending_message().contains("int()"));
        exc::clear();

        // A plain class keeps rejecting constructor arguments.
        let plain = value::class_new("Plain".to_string(), NONE, Vec::new());
        assert_eq!(call(plain, 1, args.as_ptr()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
    }

    /// `class S(str)` and `class B(bytes)` expose the wrapped value through the
    /// scalar views, rendering, equality and inherited methods.
    #[test]
    fn scalar_subclasses_wrap_str_and_bytes() {
        crate::exc::init();
        let str_class = value::native_class("str");
        let sclass = value::class_new_multi(
            "S".to_string(),
            vec![str_class],
            NONE,
            Vec::new(),
            Vec::new(),
        );
        let text = [string("a")];
        let instance = call(sclass, 1, text.as_ptr());
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(value::as_str(instance), Some("a"));
        assert_eq!(value::repr(instance), "'a'");
        assert_eq!(value::to_string(instance), "a");
        assert!(crate::container::equals(instance, string("a")));
        assert_eq!(hash(instance), hash(string("a")));
        assert!(value::truthy(builtin_call(
            Builtin::Isinstance,
            &[instance, str_class]
        )));
        let upper = crate::methods::call(instance, b"upper".as_ptr(), 5, 0, std::ptr::null());
        assert_eq!(value::as_str(upper), Some("A"));
        assert!(!exc::pending());

        let bytes_class = value::native_class("bytes");
        let bclass = value::class_new_multi(
            "B".to_string(),
            vec![bytes_class],
            NONE,
            Vec::new(),
            Vec::new(),
        );
        let data = [value::bytes_new(b"xy".to_vec())];
        let instance = call(bclass, 1, data.as_ptr());
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(value::as_bytes(instance), Some(&b"xy"[..]));
        assert_eq!(value::repr(instance), "b'xy'");
        assert!(value::truthy(builtin_call(
            Builtin::Isinstance,
            &[instance, bytes_class]
        )));

        let float_class = value::native_class("float");
        let fclass = value::class_new_multi(
            "F".to_string(),
            vec![float_class],
            NONE,
            Vec::new(),
            Vec::new(),
        );
        let zero = call(fclass, 0, std::ptr::null());
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(value::as_float(zero), Some(0.0));
        let arg = [string("1.5")];
        let instance = call(fclass, 1, arg.as_ptr());
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(value::as_float(instance), Some(1.5));
        assert_eq!(value::repr(instance), "1.5");
        assert!(value::truthy(builtin_call(
            Builtin::Isinstance,
            &[instance, float_class]
        )));

        let bool_class = value::native_class("bool");
        let bclass = value::class_new_multi(
            "Bool".to_string(),
            vec![bool_class],
            NONE,
            Vec::new(),
            Vec::new(),
        );
        let arg = [int(0)];
        let instance = call(bclass, 1, arg.as_ptr());
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(value::as_integer(instance), Some(0));
        assert_eq!(value::repr(instance), "False");
        assert!(!value::truthy(instance));
        assert!(value::truthy(builtin_call(
            Builtin::Isinstance,
            &[instance, bool_class]
        )));
        assert!(value::truthy(builtin_call(
            Builtin::Isinstance,
            &[instance, value::builtin_function_new(Builtin::Bool)]
        )));
    }

    /// `int.__new__(cls, value)` (the path `enum.py` uses to build IntEnum
    /// members) allocates the subclass instance instead of returning the
    /// class or a plain int.
    #[test]
    fn scalar_new_allocates_a_subclass_instance() {
        crate::exc::init();
        let int_class = value::native_class("int");
        let class = value::class_new_multi(
            "E".to_string(),
            vec![int_class],
            NONE,
            Vec::new(),
            Vec::new(),
        );
        let args = [class, int(1)];
        let member = crate::modules::call_native("builtins", "__scalar_new__", 2, args.as_ptr());
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(
            value::as_instance(member).map(|state| state.class),
            Some(class)
        );
        assert_eq!(value::as_integer(member), Some(1));
        assert_eq!(value::repr(member), "1");

        // The bound spelling `int.__new__(cls, value)` takes the same path.
        let int_fn = value::builtin_function_new(Builtin::Int);
        let new_fn = crate::methods::getattr(int_fn, b"__new__".as_ptr(), 7);
        assert!(!exc::pending(), "{}", pending_message());
        let args = [class, int(2)];
        let member = call(new_fn, 2, args.as_ptr());
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(
            value::as_instance(member).map(|state| state.class),
            Some(class)
        );
        assert_eq!(value::as_integer(member), Some(2));

        // The exact native class still yields the plain builtin value.
        let args = [int_class, int(3)];
        let plain = crate::modules::call_native("builtins", "__scalar_new__", 2, args.as_ptr());
        assert!(value::is_int(plain));
        assert_eq!(value::as_integer(plain), Some(3));
    }
}
