//! Builtin functions: `print`, `len`, `str`, `int`, `float`, `abs`, `range`,
//! `list`, `dict`, `sum`, `enumerate`, `zip`, `ord`, `chr`, `divmod`,
//! `callable` (ARCHITECTURE §3.2).

use std::io::Write as _;

use pyx_ir::{Builtin, MethodKind, ParamKind};

use crate::container;
use crate::exc::{
    self, OS_ERROR, OVERFLOW_ERROR, RUNTIME_ERROR, TYPE_ERROR, VALUE_ERROR, ZERO_DIVISION_ERROR,
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

/// `str(v)` as a fresh string object.
pub(crate) fn to_str(v: i64) -> i64 {
    value::str_alloc(value::to_string(v))
}

/// `repr(v)` as a fresh string object.
pub(crate) fn to_repr(v: i64) -> i64 {
    value::str_alloc(value::repr(v))
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
/// rejected because PyX strings are UTF-8 and cannot represent them. A
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
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "int() argument must be a string, a bytes-like object or a real number, not '{}'",
            value::type_name(v)
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
        return to_int(value_arg);
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
            || value::as_ext_function(v).is_some(),
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
    // SAFETY: ABI contract: `argv` points at `argc` readable values; length checked above.
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
    let mut accumulator = start;
    loop {
        match crate::fiber::resume_value(generator) {
            // Yielded: the suspension point stored the item in the fiber.
            Some(false) => {
                accumulator = ops::binop(
                    ops::OP_ADD,
                    accumulator,
                    crate::fiber::yielded_value(generator),
                );
                if exc::pending() {
                    return NONE;
                }
            }
            // Finished, now or before this call.
            Some(true) => {
                if crate::fiber::has_error(generator) {
                    crate::fiber::raise_error(generator);
                    return NONE;
                }
                return accumulator;
            }
            // Unreachable: the caller checked that this is a generator fiber.
            None => return NONE,
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

// First-class functions

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
                    value::fiber_coroutine_new(entry, name, args)
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
        FunctionBody::Native { module, name } => modules::call_native(module, name, argc, argv),
        FunctionBody::BoundMethod { receiver, method } => {
            crate::core::methods::call(*receiver, method.as_ptr(), method.len(), argc, argv)
        }
        FunctionBody::Builtin(builtin) => call_builtin(*builtin, argc, argv),
        FunctionBody::AttributeGetter { names } => attribute_getter_call(names, argc, argv),
        FunctionBody::ItemGetter { keys } => item_getter_call(keys, argc, argv),
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
    }
}

/// Calls `target` with `args` plus keyword pairs bound by parameter name.
///
/// Used by `functools.partial`: only a generated wrapper with a plain
/// positional signature carries its parameter names (codegen attaches them
/// through `pyx_function_params`). Every keyword must name a parameter that is
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
        let Some(index) = params.iter().position(|param| param == keyword) else {
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
/// through the iterator ABI: the collected values live in a temporary PyX list
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

/// `fn(args..., name=value...)`: runtime keyword binding for a function value.
///
/// Generated functions carry their full parameter metadata and a uniform
/// full-argv trampoline. Native/builtin values validate their own positional
/// arity and reject keywords here.
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
    if value::as_class(function).is_some() {
        let class = function;
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
        let instance = value::instance_new(class);
        let Some((kind, init)) = value::class_member(class, "__init__") else {
            if !args.is_empty() || !keywords.is_empty() {
                return exc::raise_with_message(TYPE_ERROR, "class takes no arguments");
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
    if let Some(func) = value::as_function(function) {
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
    }
    let Some(func) = value::as_function(function) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("'{}' object is not callable", value::type_name_of(function)),
        );
    };
    let name = func.name.clone();
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
            ParamKind::Positional => {
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
                    .filter(|param| param.kind == ParamKind::Positional)
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
    if func.flags & (value::FUNCTION_FLAG_ASYNC | value::FUNCTION_FLAG_GENERATOR) != 0 {
        // Generator/coroutine calls must create a suspended fiber instead of
        // running the concrete entry. Plain positional signatures can go
        // through the regular wrapper with the fully bound slot vector.
        if signature
            .iter()
            .all(|param| param.kind == ParamKind::Positional)
        {
            return call(function, full.len(), full.as_ptr());
        }
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("{name}() does not accept keyword arguments"),
        );
    }
    call_full_entry(entry, &full)
}

/// Calls a generated full-argv trampoline while keeping every argument value
/// visible to the conservative collector. Small vectors are copied to a stack
/// array; unusually large signatures fall back to permanent thread roots.
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
    for &argument in args {
        crate::heap::add_thread_root(argument);
    }
    trampoline(args.as_ptr())
}

/// Resolves a name bound by a function-local import.
///
/// PyX links imports at compile time, so a deferred import that is reached at
/// runtime cannot be executed. This reports the unresolved module (and member)
/// and always raises `RuntimeError`; the returned `None` is only the pending
/// exception placeholder for generated code.
pub(crate) fn deferred_import(module: &str, member: Option<&str>) -> i64 {
    // A nested import that names a value of an already-initialized module can
    // be served from the mirrored module global table. The loader links every
    // module that is part of the program, so no dynamic loading is needed.
    if let Some(member) = member {
        if let Some(value) = crate::core::globals::try_get(module, member) {
            return value;
        }
    }
    let message = match member {
        Some(member) => format!("dynamic import of '{module}.{member}' is not supported"),
        None => format!("dynamic import of '{module}' is not supported"),
    };
    exc::raise_with_message(RUNTIME_ERROR, &message)
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
    let Some((kind, init)) = value::class_member(class, "__init__") else {
        if argc > 0 {
            return exc::raise_with_message(TYPE_ERROR, &format!("{name}() takes no arguments"));
        }
        return instance;
    };
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
    if let Some(module) = value::as_module(object) {
        modules::module_get_by_name(module.name(), name)
    } else {
        methods::getattr(object, name.as_ptr(), name.len())
    }
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
/// `print` is variadic, `str`/`repr`/`int`/`float`/`abs`/`len` take one
/// argument,
/// `set`/`list`/`dict` accept zero or one, `sum`/`enumerate` one or two,
/// `range` one to three, `bytes` zero to two, `map`/`filter` exactly two,
/// `iter` exactly one, `next` one or two, `max`/`min` at least one (their
/// final argument is the key slot), `sorted` one to three
/// `(iterable, key, reverse)`, `round` one or two, `format` two (or
/// three for the internal conversion flag), `isinstance` exactly two,
/// `all` exactly one, `ord`/`chr` exactly one, `divmod` exactly two,
/// `callable` exactly one and the internal dict merge exactly two.
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
        Builtin::Zip => zip(argc, argv),
        Builtin::Set => match args {
            [] => container::set_new(0, std::ptr::null()),
            [iterable] => container::set_from(*iterable),
            _ => builtin_arity_error("set", "0 or 1", argc),
        },
        // PyX has a single set representation; `frozenset` builds the same
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
            [function, iterable] => map(*function, *iterable),
            _ => builtin_arity_error("map", "exactly 2", argc),
        },
        Builtin::Filter => match args {
            [function, iterable] => filter(*function, *iterable),
            _ => builtin_arity_error("filter", "exactly 2", argc),
        },
        Builtin::Iter => match args {
            [obj] => iter::iter_of(*obj),
            _ => builtin_arity_error("iter", "exactly 1", argc),
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
            [value] => to_str(*value),
            _ => builtin_arity_error("str", "exactly 1", argc),
        },
        Builtin::Repr => match args {
            [value] => to_repr(*value),
            _ => builtin_arity_error("repr", "exactly 1", argc),
        },
        Builtin::Hash => match args {
            [value] => hash(*value),
            _ => builtin_arity_error("hash", "exactly 1", argc),
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
            _ => builtin_arity_error("type", "exactly 1", argc),
        },
        Builtin::Reversed => match args {
            [value] => reversed(*value),
            _ => builtin_arity_error("reversed", "exactly 1", argc),
        },
        Builtin::Import => exc::raise_with_message(
            RUNTIME_ERROR,
            "dynamic imports are not supported at runtime",
        ),
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
/// PyX has no IPython, so the call returns a fresh instance whose class is
/// named `TerminalInteractiveShell`. `get_ipython().__class__.__name__` is
/// therefore `"TerminalInteractiveShell"` (neither `"ZMQInteractiveShell"` nor
/// a Colab shell), and every string rendering is a plain PyX repr without
/// `google.colab` or `ZMQInteractiveShell`.
fn get_ipython_value() -> i64 {
    // The class stays reachable from the instance; the conservative stack scan
    // keeps it alive while the instance is allocated.
    let class = value::class_new("TerminalInteractiveShell".to_string(), NONE, Vec::new());
    value::instance_new(class)
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
        return exc::raise_with_message(VALUE_ERROR, &format!("{name}() arg is an empty sequence"));
    }
    best
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
pub(crate) fn isinstance(value: i64, target: i64) -> i64 {
    match type_check(value, target) {
        Some(result) => value::bool_value(result),
        None => exc::raise_with_message(
            TYPE_ERROR,
            "isinstance() arg 2 must be a type or tuple of types",
        ),
    }
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
    if value::as_class(target).is_some() {
        return Some(instance_of(value, target));
    }
    if let Some(function) = value::as_function(target) {
        if let FunctionBody::Builtin(builtin) = &function.body {
            return named_type_check(value, builtin.name());
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
        // `bool` is a subclass of `int` in Python.
        "int" => {
            value::is_int(value) || value::is_bigint(value) || value::type_name(value) == "bool"
        }
        "bool" => value::type_name(value) == "bool",
        "float" => value::as_float(value).is_some(),
        "str" => value::as_str(value).is_some(),
        "bytes" => value::as_bytes(value).is_some(),
        "list" => value::as_list(value).is_some(),
        "dict" => value::as_dict(value).is_some() || value::as_defaultdict(value).is_some(),
        "tuple" => value::as_tuple(value).is_some(),
        "set" | "frozenset" => value::as_set(value).is_some(),
        "NoneType" | "None" => value == NONE,
        "object" => true,
        "type" => value::as_class(value).is_some(),
        _ => return None,
    })
}

/// Whether `value` is an instance of `class` (or one of its subclasses).
fn instance_of(value: i64, class: i64) -> bool {
    if is_protocol_class(class) {
        return protocol_instance_of(value, class);
    }
    let Some(instance) = value::as_instance(value) else {
        return false;
    };
    let mut current = instance.class;
    for _ in 0..1024 {
        if current == class {
            return true;
        }
        let Some(base_state) = value::as_class(current) else {
            // `instance.class` is always a class; defensive only.
            return current == class;
        };
        if base_state.base == NONE {
            return false;
        }
        current = base_state.base;
    }
    false
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
            "iter() takes exactly 1 argument(s) (0 given)"
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
}
