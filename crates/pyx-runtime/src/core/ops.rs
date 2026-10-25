//! Python operator semantics for tagged values (ARCHITECTURE §3.2).
//!
//! All fallible operations report errors as a pending exception and return a
//! dummy `None` value; the codegen polls `pyx_exc_pending()` afterwards.

use std::cmp::Ordering;

use crate::container;
use crate::exc::{self, OVERFLOW_ERROR, TYPE_ERROR, ZERO_DIVISION_ERROR};
use crate::value::{self, Num};

// `pyx_binop` op codes.
pub(crate) const OP_ADD: u32 = 0;
pub(crate) const OP_SUB: u32 = 1;
pub(crate) const OP_MUL: u32 = 2;
pub(crate) const OP_DIV: u32 = 3;
pub(crate) const OP_FLOOR_DIV: u32 = 4;
pub(crate) const OP_MOD: u32 = 5;
pub(crate) const OP_POW: u32 = 6;
pub(crate) const OP_EQ: u32 = 7;
pub(crate) const OP_NE: u32 = 8;
pub(crate) const OP_LT: u32 = 9;
pub(crate) const OP_LE: u32 = 10;
pub(crate) const OP_GT: u32 = 11;
pub(crate) const OP_GE: u32 = 12;
pub(crate) const OP_IN: u32 = 13;
pub(crate) const OP_NOT_IN: u32 = 14;
pub(crate) const OP_IS: u32 = 15;
pub(crate) const OP_IS_NOT: u32 = 16;
pub(crate) const OP_BIT_AND: u32 = 17;
pub(crate) const OP_BIT_OR: u32 = 18;
pub(crate) const OP_BIT_XOR: u32 = 19;
pub(crate) const OP_SHL: u32 = 20;
pub(crate) const OP_SHR: u32 = 21;

// `pyx_unop` op codes.
pub(crate) const OP_NEG: u32 = 0;
pub(crate) const OP_POS: u32 = 1;
pub(crate) const OP_NOT: u32 = 2;
pub(crate) const OP_INVERT: u32 = 3;

/// `2^63` as `f64`; used for exact int/float comparisons near the i64 bounds.
const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;

fn overflow() -> i64 {
    exc::raise_with_message(OVERFLOW_ERROR, "integer overflow")
}

fn zero_div(message: &str) -> i64 {
    exc::raise_with_message(ZERO_DIVISION_ERROR, message)
}

fn arithmetic_type_error(op: &str, a: i64, b: i64) -> i64 {
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "unsupported operand type(s) for {}: '{}' and '{}'",
            op,
            value::type_name(a),
            value::type_name(b)
        ),
    )
}

/// Encodes a Python integer result, raising `OverflowError` if it no longer
/// fits into a tagged immediate.
pub(crate) fn int_result(n: i64) -> i64 {
    match value::encode_int(n) {
        Some(v) => v,
        None => overflow(),
    }
}

fn to_f64(n: Num) -> f64 {
    match n {
        Num::Int(i) => i as f64,
        Num::Float(f) => f,
    }
}

// -------------------------------------------------------------------------
// Binary operators
// -------------------------------------------------------------------------

pub(crate) fn binop(op: u32, a: i64, b: i64) -> i64 {
    // `str % args` is Python's printf-style formatting, not a numeric
    // operation. The template is copied out before any allocation.
    if op == OP_MOD {
        if let Some(template) = value::as_str(a) {
            let template = template.to_string();
            return match crate::format::percent_format(&template, b) {
                Ok(text) => value::str_alloc(text),
                Err(error) => exc::raise_with_message(error.tag, &error.message),
            };
        }
    }
    // `Path / str` and `Path / Path` join paths instead of dividing; the
    // numeric operators below never apply to path objects.
    if op == OP_DIV && value::as_path(a).is_some() {
        return path_join_op(a, b);
    }
    match op {
        OP_ADD => add(a, b),
        OP_SUB => sub(a, b),
        OP_MUL => mul(a, b),
        OP_DIV => div(a, b),
        OP_FLOOR_DIV => floor_div(a, b),
        OP_MOD => modulo(a, b),
        OP_POW => pow(a, b),
        OP_EQ | OP_NE | OP_LT | OP_LE | OP_GT | OP_GE => compare(op, a, b),
        OP_IN => contains(a, b, false),
        OP_NOT_IN => contains(a, b, true),
        // Identity compares the tagged representation: `None` and the
        // booleans are singletons, small integers are unboxed immediates, and
        // heap values compare by pointer.
        OP_IS => value::bool_value(a == b),
        OP_IS_NOT => value::bool_value(a != b),
        OP_BIT_AND => bitwise(op, a, b),
        OP_BIT_OR => bitwise(op, a, b),
        OP_BIT_XOR => bitwise(op, a, b),
        OP_SHL => shift(a, b, true),
        OP_SHR => shift(a, b, false),
        _ => exc::raise_with_message(TYPE_ERROR, "unknown binary operator"),
    }
}

/// `&`, `|`, `^` on integers and booleans.
///
/// Two booleans keep the boolean result type (Python's `bool` is an `int`
/// subtype); every other integer combination yields an `int`. Floating-point,
/// string and container operands raise `TypeError` like CPython.
fn bitwise(op: u32, a: i64, b: i64) -> i64 {
    let symbol = match op {
        OP_BIT_AND => "&",
        OP_BIT_OR => "|",
        _ => "^",
    };
    let (Some(x), Some(y)) = (value::as_integer(a), value::as_integer(b)) else {
        return arithmetic_type_error(symbol, a, b);
    };
    let result = match op {
        OP_BIT_AND => x & y,
        OP_BIT_OR => x | y,
        _ => x ^ y,
    };
    // `True` and `False` are singleton immediates; two booleans produce a
    // boolean, any mixed pair produces an int.
    if matches!(a, value::TRUE | value::FALSE) && matches!(b, value::TRUE | value::FALSE) {
        return value::bool_value(result != 0);
    }
    int_result(result)
}

/// `left << right` and `left >> right` on integers (and booleans).
///
/// A negative shift count raises `ValueError`. Left shifts that leave the
/// runtime's tagged-integer range raise `OverflowError`; right shifts
/// saturate like Python's arithmetic shift for arbitrarily large counts.
fn shift(a: i64, b: i64, left: bool) -> i64 {
    let symbol = if left { "<<" } else { ">>" };
    let (Some(base), Some(count)) = (value::as_integer(a), value::as_integer(b)) else {
        return arithmetic_type_error(symbol, a, b);
    };
    if count < 0 {
        return exc::raise_with_message(exc::VALUE_ERROR, "negative shift count");
    }
    if left {
        let shifted = if count >= 64 {
            if base == 0 {
                0
            } else {
                return overflow();
            }
        } else {
            let amount = count as u32;
            match base.checked_shl(amount) {
                // The reverse shift catches bits shifted out of the i64.
                Some(shifted) if (shifted >> amount) == base => shifted,
                _ => return overflow(),
            }
        };
        int_result(shifted)
    } else {
        let shifted = if count >= 64 {
            if base < 0 {
                -1
            } else {
                0
            }
        } else {
            base >> count
        };
        int_result(shifted)
    }
}

/// `Path / other`: joins the right operand, which must be a `str` or a
/// `Path`.
fn path_join_op(a: i64, b: i64) -> i64 {
    let Some(base) = value::as_path(a) else {
        return arithmetic_type_error("/", a, b);
    };
    // Copy both payloads before allocating: heap references must not be read
    // after an allocation that may collect.
    let base = base.path().to_string();
    let Some(part) = value::as_path_like(b) else {
        return arithmetic_type_error("/", a, b);
    };
    let part = part.to_string();
    value::path_new(value::join_paths(&base, &part))
}

/// `left in right` (and `left not in right`), Python semantics.
fn contains(left: i64, right: i64, negate: bool) -> i64 {
    let found = if let Some(items) = value::as_list(right) {
        container::set_contains(items, left)
    } else if let Some(items) = value::as_tuple(right) {
        container::set_contains(items, left)
    } else if let Some(pairs) = value::as_dict(right) {
        if let Some(name) = container::unhashable_name(left) {
            return exc::raise_with_message(TYPE_ERROR, &container::unhashable_message(name));
        }
        container::dict_find(pairs, left).is_some()
    } else if let Some((pairs, _)) = value::as_defaultdict(right) {
        if let Some(name) = container::unhashable_name(left) {
            return exc::raise_with_message(TYPE_ERROR, &container::unhashable_message(name));
        }
        container::dict_find(pairs, left).is_some()
    } else if let Some(s) = value::as_str(right) {
        let Some(needle) = value::as_str(left) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "'in <string>' requires string as left operand, not {}",
                    value::type_name(left)
                ),
            );
        };
        // Byte-wise substring search is equivalent to scalar-value search for
        // valid UTF-8 (a match always starts on a character boundary).
        s.contains(needle)
    } else if let Some(haystack) = value::as_bytes(right) {
        if let Some(needle) = value::as_bytes(left) {
            if needle.is_empty() {
                true
            } else {
                haystack
                    .windows(needle.len())
                    .any(|window| window == needle)
            }
        } else if let Some(byte) = value::as_integer(left) {
            // `int in bytes` tests for a single byte value.
            (0..=255).contains(&byte) && haystack.contains(&(byte as u8))
        } else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "a bytes-like object is required, not '{}'",
                    value::type_name(left)
                ),
            );
        }
    } else if let Some(items) = value::as_set(right) {
        if let Some(name) = container::unhashable_name(left) {
            return exc::raise_with_message(TYPE_ERROR, &container::unhashable_message(name));
        }
        container::set_contains(items, left)
    } else if let Some((start, stop, step)) = value::as_range(right) {
        range_contains(start, stop, step, left)
    } else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "argument of type '{}' is not iterable",
                value::type_name(right)
            ),
        );
    };
    value::bool_value(found != negate)
}

/// `needle in range(start, stop, step)` with Python semantics: only integral
/// numbers can be members, everything else is simply absent.
fn range_contains(start: i64, stop: i64, step: i64, needle: i64) -> bool {
    if step == 0 {
        return false;
    }
    let candidate = match value::num_of(needle) {
        Some(Num::Int(n)) => n,
        Some(Num::Float(f)) => {
            if !f.is_finite() || f.fract() != 0.0 {
                return false;
            }
            let n = f as i64;
            if n as f64 != f {
                return false;
            }
            n
        }
        None => return false,
    };
    let (start, stop, candidate) = (start as i128, stop as i128, candidate as i128);
    let in_bounds = if step > 0 {
        candidate >= start && candidate < stop
    } else {
        candidate <= start && candidate > stop
    };
    in_bounds && (candidate - start) % (step as i128) == 0
}

fn add(a: i64, b: i64) -> i64 {
    if let (Some(x), Some(y)) = (value::num_of(a), value::num_of(b)) {
        return match (x, y) {
            (Num::Int(p), Num::Int(q)) => match p.checked_add(q) {
                Some(n) => int_result(n),
                None => overflow(),
            },
            (x, y) => value::float_new(to_f64(x) + to_f64(y)),
        };
    }
    if let (Some(s), Some(t)) = (value::as_str(a), value::as_str(b)) {
        return match s.len().checked_add(t.len()) {
            Some(n) if n <= isize::MAX as usize => {
                let mut out = String::with_capacity(n);
                out.push_str(s);
                out.push_str(t);
                value::str_alloc(out)
            }
            _ => overflow(),
        };
    }
    if let (Some(xs), Some(ys)) = (value::as_bytes(a), value::as_bytes(b)) {
        // `bytes + bytes` concatenates into a fresh bytes object.
        return match xs.len().checked_add(ys.len()) {
            Some(n) if n <= isize::MAX as usize => {
                let mut out = Vec::with_capacity(n);
                out.extend_from_slice(xs);
                out.extend_from_slice(ys);
                value::bytes_new(out)
            }
            _ => overflow(),
        };
    }
    if let (Some(xs), Some(ys)) = (value::as_tuple(a), value::as_tuple(b)) {
        // `tuple + tuple` concatenates into a fresh tuple.
        match xs.len().checked_add(ys.len()) {
            Some(n) if n <= isize::MAX as usize / std::mem::size_of::<i64>() => {
                let mut items = Vec::with_capacity(n);
                items.extend_from_slice(xs);
                items.extend_from_slice(ys);
                return value::tuple_new(items);
            }
            _ => return overflow(),
        }
    }
    if let (Some(xs), Some(ys)) = (value::as_list(a), value::as_list(b)) {
        // `list + list` concatenates into a fresh list.
        match xs.len().checked_add(ys.len()) {
            Some(n) if n <= isize::MAX as usize / std::mem::size_of::<i64>() => {
                let mut items = Vec::with_capacity(n);
                items.extend_from_slice(xs);
                items.extend_from_slice(ys);
                return value::list_new(items);
            }
            _ => return overflow(),
        }
    }
    arithmetic_type_error("+", a, b)
}

fn sub(a: i64, b: i64) -> i64 {
    if let (Some(x), Some(y)) = (value::num_of(a), value::num_of(b)) {
        return match (x, y) {
            (Num::Int(p), Num::Int(q)) => match p.checked_sub(q) {
                Some(n) => int_result(n),
                None => overflow(),
            },
            (x, y) => value::float_new(to_f64(x) - to_f64(y)),
        };
    }
    if value::as_set(a).is_some() || value::as_set(b).is_some() {
        if value::as_set(a).is_none() || value::as_set(b).is_none() {
            return arithmetic_type_error("-", a, b);
        }
        // Allocate first, then read both operands so they stay live roots
        // across the collection that the allocation may trigger.
        let result = value::set_new(Vec::new());
        let xs = value::as_set(a).unwrap_or(&[]);
        let ys = value::as_set(b).unwrap_or(&[]);
        for &item in xs {
            if !container::set_contains(ys, item) {
                container::insert_set_item(result, item);
            }
        }
        return result;
    }
    arithmetic_type_error("-", a, b)
}

fn mul(a: i64, b: i64) -> i64 {
    if let (Some(x), Some(y)) = (value::num_of(a), value::num_of(b)) {
        return match (x, y) {
            (Num::Int(p), Num::Int(q)) => match p.checked_mul(q) {
                Some(n) => int_result(n),
                None => overflow(),
            },
            (x, y) => value::float_new(to_f64(x) * to_f64(y)),
        };
    }
    // `str * int` / `int * str` (bool counts as 0/1).
    if let (Some(s), Some(n)) = (value::as_str(a), value::as_integer(b)) {
        return repeat_str(s, n);
    }
    if let (Some(n), Some(s)) = (value::as_integer(a), value::as_str(b)) {
        return repeat_str(s, n);
    }
    // `bytes * int` / `int * bytes`.
    if let (Some(bytes), Some(n)) = (value::as_bytes(a), value::as_integer(b)) {
        return repeat_bytes(bytes, n);
    }
    if let (Some(n), Some(bytes)) = (value::as_integer(a), value::as_bytes(b)) {
        return repeat_bytes(bytes, n);
    }
    // `list * int` / `int * list`.
    if let (Some(items), Some(n)) = (value::as_list(a), value::as_integer(b)) {
        return repeat_list(items, n);
    }
    if let (Some(n), Some(items)) = (value::as_integer(a), value::as_list(b)) {
        return repeat_list(items, n);
    }
    // `tuple * int` / `int * tuple`.
    if let (Some(items), Some(n)) = (value::as_tuple(a), value::as_integer(b)) {
        return repeat_tuple(items, n);
    }
    if let (Some(n), Some(items)) = (value::as_integer(a), value::as_tuple(b)) {
        return repeat_tuple(items, n);
    }
    arithmetic_type_error("*", a, b)
}

fn repeat_str(s: &str, n: i64) -> i64 {
    if n <= 0 || s.is_empty() {
        return value::str_alloc(String::new());
    }
    let count = n as usize;
    match s.len().checked_mul(count) {
        Some(total) if total <= isize::MAX as usize => {
            let mut out = String::with_capacity(total);
            for _ in 0..count {
                out.push_str(s);
            }
            value::str_alloc(out)
        }
        _ => overflow(),
    }
}

fn repeat_bytes(bytes: &[u8], n: i64) -> i64 {
    if n <= 0 || bytes.is_empty() {
        return value::bytes_new(Vec::new());
    }
    let count = n as usize;
    match bytes.len().checked_mul(count) {
        Some(total) if total <= isize::MAX as usize => {
            let mut out = Vec::with_capacity(total);
            for _ in 0..count {
                out.extend_from_slice(bytes);
            }
            value::bytes_new(out)
        }
        _ => overflow(),
    }
}

fn repeat_list(items: &[i64], n: i64) -> i64 {
    if n <= 0 || items.is_empty() {
        return value::list_new(Vec::new());
    }
    let count = n as usize;
    match items.len().checked_mul(count) {
        Some(total) if total <= isize::MAX as usize / std::mem::size_of::<i64>() => {
            let mut out = Vec::with_capacity(total);
            for _ in 0..count {
                out.extend_from_slice(items);
            }
            value::list_new(out)
        }
        _ => overflow(),
    }
}

fn repeat_tuple(items: &[i64], n: i64) -> i64 {
    if n <= 0 || items.is_empty() {
        return value::tuple_new(Vec::new());
    }
    let count = n as usize;
    match items.len().checked_mul(count) {
        Some(total) if total <= isize::MAX as usize / std::mem::size_of::<i64>() => {
            let mut out = Vec::with_capacity(total);
            for _ in 0..count {
                out.extend_from_slice(items);
            }
            value::tuple_new(out)
        }
        _ => overflow(),
    }
}

fn div(a: i64, b: i64) -> i64 {
    if let (Some(x), Some(y)) = (value::num_of(a), value::num_of(b)) {
        let denom = to_f64(y);
        if denom == 0.0 {
            return zero_div("division by zero");
        }
        return value::float_new(to_f64(x) / denom);
    }
    arithmetic_type_error("/", a, b)
}

fn floor_div(a: i64, b: i64) -> i64 {
    match (value::num_of(a), value::num_of(b)) {
        (Some(Num::Int(p)), Some(Num::Int(q))) => {
            if q == 0 {
                return zero_div("integer division or modulo by zero");
            }
            let (Some(quo), Some(rem)) = (p.checked_div(q), p.checked_rem(q)) else {
                return overflow();
            };
            // Python floors towards negative infinity; `Rust`'s `/` truncates.
            let floored = if rem != 0 && ((rem < 0) != (q < 0)) {
                quo.checked_sub(1)
            } else {
                Some(quo)
            };
            match floored {
                Some(n) => int_result(n),
                None => overflow(),
            }
        }
        (Some(x), Some(y)) => {
            let denom = to_f64(y);
            if denom == 0.0 {
                return zero_div("integer division or modulo by zero");
            }
            value::float_new((to_f64(x) / denom).floor())
        }
        _ => arithmetic_type_error("//", a, b),
    }
}

fn modulo(a: i64, b: i64) -> i64 {
    match (value::num_of(a), value::num_of(b)) {
        (Some(Num::Int(p)), Some(Num::Int(q))) => {
            if q == 0 {
                return zero_div("integer modulo by zero");
            }
            let Some(rem) = p.checked_rem(q) else {
                return overflow();
            };
            let adjusted = if rem != 0 && ((rem < 0) != (q < 0)) {
                rem.checked_add(q)
            } else {
                Some(rem)
            };
            match adjusted {
                Some(n) => int_result(n),
                None => overflow(),
            }
        }
        (Some(x), Some(y)) => {
            let denom = to_f64(y);
            if denom == 0.0 {
                return zero_div("integer modulo by zero");
            }
            let mut rem = to_f64(x) % denom;
            if rem != 0.0 && ((rem < 0.0) != (denom < 0.0)) {
                rem += denom;
            }
            value::float_new(rem)
        }
        _ => arithmetic_type_error("%", a, b),
    }
}

fn pow(a: i64, b: i64) -> i64 {
    match (value::num_of(a), value::num_of(b)) {
        (Some(Num::Int(base)), Some(Num::Int(exp))) => {
            if exp >= 0 {
                int_pow(base, exp as u64)
            } else {
                float_pow(base as f64, exp as f64)
            }
        }
        (Some(x), Some(y)) => float_pow(to_f64(x), to_f64(y)),
        _ => arithmetic_type_error("**", a, b),
    }
}

fn int_pow(base: i64, mut exp: u64) -> i64 {
    let mut acc: i64 = 1;
    let mut factor = base;
    while exp > 0 {
        if exp & 1 == 1 {
            match acc.checked_mul(factor) {
                Some(v) => acc = v,
                None => return overflow(),
            }
        }
        exp >>= 1;
        if exp > 0 {
            match factor.checked_mul(factor) {
                Some(v) => factor = v,
                None => return overflow(),
            }
        }
    }
    int_result(acc)
}

fn float_pow(base: f64, exp: f64) -> i64 {
    if base == 0.0 && exp < 0.0 {
        return zero_div("0.0 cannot be raised to a negative power");
    }
    value::float_new(base.powf(exp))
}

// -------------------------------------------------------------------------
// Comparisons
// -------------------------------------------------------------------------

fn compare(op: u32, a: i64, b: i64) -> i64 {
    if let (Some(x), Some(y)) = (value::num_of(a), value::num_of(b)) {
        return ordered_result(op, cmp_num(x, y));
    }
    if let (Some(s), Some(t)) = (value::as_str(a), value::as_str(b)) {
        // UTF-8 byte order equals Unicode scalar order.
        return ordered_result(op, Some(s.cmp(t)));
    }
    if let (Some(x), Some(y)) = (value::as_bytes(a), value::as_bytes(b)) {
        // Bytes compare lexicographically by unsigned byte value.
        return ordered_result(op, Some(x.cmp(y)));
    }
    // Tuples and lists compare lexicographically, with recursive element
    // ordering (Python's sequence comparison).
    if matches!(op, OP_LT | OP_LE | OP_GT | OP_GE) {
        if let (Some(x), Some(y)) = (value::as_tuple(a), value::as_tuple(b)) {
            return ordered_result(op, sequence_order(x, y));
        }
        if let (Some(x), Some(y)) = (value::as_list(a), value::as_list(b)) {
            return ordered_result(op, sequence_order(x, y));
        }
    }
    // Containers compare structurally (lists element-wise, dicts
    // order-insensitively); ordering is not defined for them in Phase 2.0.
    if matches!(op, OP_EQ | OP_NE) && (is_container(a) || is_container(b)) {
        let equal = container::equals(a, b);
        return value::bool_value(if op == OP_EQ { equal } else { !equal });
    }
    // Python's `==`/`!=` across different types is well-defined.
    match op {
        OP_EQ => return value::bool_value(a == b),
        OP_NE => return value::bool_value(a != b),
        _ => {}
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "'{}' not supported between instances of '{}' and '{}'",
            cmp_symbol(op),
            value::type_name(a),
            value::type_name(b)
        ),
    )
}

/// Lexicographic ordering of two Python sequences. Equality is recursive;
/// incomparable element types propagate the normal TypeError.
fn sequence_order(a: &[i64], b: &[i64]) -> Option<Ordering> {
    let common = a.len().min(b.len());
    for index in 0..common {
        if container::equals(a[index], b[index]) {
            continue;
        }
        let result = binop(OP_LT, a[index], b[index]);
        if exc::pending() {
            return None;
        }
        return if value::truthy(result) {
            Some(Ordering::Less)
        } else {
            Some(Ordering::Greater)
        };
    }
    Some(a.len().cmp(&b.len()))
}

fn is_container(v: i64) -> bool {
    value::as_list(v).is_some()
        || value::as_dict(v).is_some()
        || value::as_iter(v).is_some()
        || value::as_tuple(v).is_some()
        || value::as_set(v).is_some()
}

fn cmp_symbol(op: u32) -> &'static str {
    match op {
        OP_LT => "<",
        OP_LE => "<=",
        OP_GT => ">",
        OP_GE => ">=",
        OP_EQ => "==",
        _ => "!=",
    }
}

fn ordered_result(op: u32, ord: Option<Ordering>) -> i64 {
    let result = match op {
        OP_EQ => matches!(ord, Some(Ordering::Equal)),
        OP_NE => !matches!(ord, Some(Ordering::Equal)),
        OP_LT => matches!(ord, Some(Ordering::Less)),
        OP_LE => matches!(ord, Some(Ordering::Less | Ordering::Equal)),
        OP_GT => matches!(ord, Some(Ordering::Greater)),
        OP_GE => matches!(ord, Some(Ordering::Greater | Ordering::Equal)),
        _ => false,
    };
    value::bool_value(result)
}

/// Numeric comparison with Python semantics. `None` means "unordered", which
/// happens for NaN: every comparison except `!=` is false.
pub(crate) fn cmp_num(a: Num, b: Num) -> Option<Ordering> {
    match (a, b) {
        (Num::Int(x), Num::Int(y)) => Some(x.cmp(&y)),
        (Num::Float(x), Num::Float(y)) => x.partial_cmp(&y),
        (Num::Int(x), Num::Float(y)) => cmp_int_float(x, y),
        (Num::Float(x), Num::Int(y)) => cmp_int_float(y, x).map(Ordering::reverse),
    }
}

/// Exact comparison of an `i64` with an `f64` (no lossy conversion of the
/// integer). Returns `None` only for NaN.
fn cmp_int_float(i: i64, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if f == f64::INFINITY {
        return Some(Ordering::Less);
    }
    if f == f64::NEG_INFINITY {
        return Some(Ordering::Greater);
    }
    // Out of i64 range: the float is strictly above/below every integer.
    if f >= TWO_POW_63 {
        return Some(Ordering::Less);
    }
    if f < -TWO_POW_63 {
        return Some(Ordering::Greater);
    }
    let truncated = f.trunc() as i64;
    match i.cmp(&truncated) {
        Ordering::Equal => {
            if f.fract() > 0.0 {
                Some(Ordering::Less)
            } else if f.fract() < 0.0 {
                Some(Ordering::Greater)
            } else {
                Some(Ordering::Equal)
            }
        }
        other => Some(other),
    }
}

// -------------------------------------------------------------------------
// Unary operators
// -------------------------------------------------------------------------

pub(crate) fn unop(op: u32, a: i64) -> i64 {
    match op {
        OP_NEG => {
            if let Some(n) = value::as_integer(a) {
                return match n.checked_neg() {
                    Some(v) => int_result(v),
                    None => overflow(),
                };
            }
            if let Some(f) = value::as_float(a) {
                return value::float_new(-f);
            }
            unary_type_error("-", a)
        }
        OP_POS => {
            if let Some(n) = value::as_integer(a) {
                return int_result(n);
            }
            if let Some(f) = value::as_float(a) {
                return value::float_new(f);
            }
            unary_type_error("+", a)
        }
        OP_NOT => value::bool_value(!value::truthy(a)),
        // `~x == -x - 1`; `as_integer` also accepts `bool`, matching Python.
        // Tagged ints stay far inside `i64`, so the complement cannot overflow.
        OP_INVERT => match value::as_integer(a) {
            Some(n) => int_result(!n),
            None => unary_type_error("~", a),
        },
        _ => exc::raise_with_message(TYPE_ERROR, "unknown unary operator"),
    }
}

fn unary_type_error(op: &str, a: i64) -> i64 {
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "bad operand type for unary {}: '{}'",
            op,
            value::type_name(a)
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{encode_int, FALSE, NONE, TRUE};

    fn int(n: i64) -> i64 {
        encode_int(n).expect("representable")
    }

    fn as_int(v: i64) -> i64 {
        value::as_integer(v).expect("integer result")
    }

    fn as_float(v: i64) -> f64 {
        value::as_float(v).expect("float result")
    }

    fn pending_message() -> String {
        value::to_string(exc::pending_value())
    }

    #[test]
    fn integer_arithmetic() {
        exc::init();
        assert_eq!(as_int(binop(OP_ADD, int(2), int(3))), 5);
        assert_eq!(as_int(binop(OP_SUB, int(2), int(3))), -1);
        assert_eq!(as_int(binop(OP_MUL, int(6), int(7))), 42);
        assert_eq!(as_int(binop(OP_POW, int(2), int(10))), 1024);
        assert_eq!(as_int(binop(OP_POW, int(0), int(0))), 1);
        assert_eq!(as_int(binop(OP_POW, int(-2), int(3))), -8);
        assert!(!exc::pending());
    }

    #[test]
    fn int_float_mixing() {
        exc::init();
        let sum = binop(OP_ADD, int(1), value::float_new(2.5));
        assert_eq!(as_float(sum), 3.5);
        assert_eq!(as_float(binop(OP_SUB, value::float_new(1.5), int(1))), 0.5);
    }

    #[test]
    fn true_division_is_float() {
        exc::init();
        let half = binop(OP_DIV, int(1), int(2));
        assert_eq!(as_float(half), 0.5);
        assert_eq!(value::to_string(half), "0.5");
    }

    #[test]
    fn floor_and_modulo_sign_rules() {
        exc::init();
        assert_eq!(as_int(binop(OP_FLOOR_DIV, int(-7), int(2))), -4);
        assert_eq!(as_int(binop(OP_FLOOR_DIV, int(7), int(-2))), -4);
        assert_eq!(as_int(binop(OP_FLOOR_DIV, int(7), int(2))), 3);
        assert_eq!(as_int(binop(OP_MOD, int(-7), int(2))), 1);
        assert_eq!(as_int(binop(OP_MOD, int(7), int(-2))), -1);
        assert_eq!(as_int(binop(OP_MOD, int(7), int(2))), 1);
        assert_eq!(as_int(binop(OP_MOD, int(-7), int(-2))), -1);
    }

    #[test]
    fn division_by_zero_messages() {
        exc::init();
        let v = binop(OP_DIV, int(1), int(0));
        assert_eq!(v, NONE);
        assert_eq!(exc::pending_tag(), ZERO_DIVISION_ERROR as i64);
        assert_eq!(pending_message(), "division by zero");

        exc::init();
        binop(OP_FLOOR_DIV, int(1), int(0));
        assert_eq!(pending_message(), "integer division or modulo by zero");

        exc::init();
        binop(OP_MOD, int(1), int(0));
        assert_eq!(pending_message(), "integer modulo by zero");
    }

    #[test]
    fn overflow_is_reported() {
        exc::init();
        let v = binop(OP_ADD, int(value::MAX_INT), int(1));
        assert_eq!(v, NONE);
        assert_eq!(exc::pending_tag(), OVERFLOW_ERROR as i64);
        assert_eq!(pending_message(), "integer overflow");
    }

    #[test]
    fn negative_power_is_float() {
        exc::init();
        let v = binop(OP_POW, int(2), int(-1));
        assert_eq!(as_float(v), 0.5);
    }

    #[test]
    fn string_operations() {
        exc::init();
        let a = value::str_from_bytes("ab".as_bytes());
        let b = value::str_from_bytes("cd".as_bytes());
        let joined = binop(OP_ADD, a, b);
        assert_eq!(value::as_str(joined), Some("abcd"));

        let repeated = binop(OP_MUL, value::str_from_bytes(b"ab"), int(3));
        assert_eq!(value::as_str(repeated), Some("ababab"));
        let repeated2 = binop(OP_MUL, int(3), value::str_from_bytes(b"ab"));
        assert_eq!(value::as_str(repeated2), Some("ababab"));
        let empty = binop(OP_MUL, value::str_from_bytes(b"ab"), int(-2));
        assert_eq!(value::as_str(empty), Some(""));
    }

    #[test]
    fn comparisons() {
        exc::init();
        assert_eq!(binop(OP_LT, int(1), int(2)), TRUE);
        assert_eq!(binop(OP_LT, int(2), int(2)), FALSE);
        assert_eq!(binop(OP_LE, int(2), int(2)), TRUE);
        assert_eq!(binop(OP_GT, int(3), int(2)), TRUE);
        assert_eq!(binop(OP_GE, int(2), int(3)), FALSE);
        assert_eq!(binop(OP_EQ, int(2), int(2)), TRUE);
        assert_eq!(binop(OP_NE, int(2), int(2)), FALSE);

        // Numeric equality across representations.
        assert_eq!(binop(OP_EQ, int(1), value::float_new(1.0)), TRUE);
        assert_eq!(binop(OP_EQ, int(1), value::float_new(1.5)), FALSE);

        let a = value::str_from_bytes(b"abc");
        let b = value::str_from_bytes(b"abd");
        assert_eq!(binop(OP_LT, a, b), TRUE);
        assert_eq!(binop(OP_EQ, a, value::str_from_bytes(b"abc")), TRUE);

        // Mismatched types: equality is false, ordering raises.
        assert_eq!(binop(OP_EQ, int(1), a), FALSE);
        assert_eq!(binop(OP_NE, int(1), a), TRUE);
        assert_eq!(binop(OP_EQ, NONE, NONE), TRUE);
        assert_eq!(binop(OP_EQ, NONE, int(0)), FALSE);
        assert!(!exc::pending());

        binop(OP_LT, int(1), a);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "'<' not supported between instances of 'int' and 'str'"
        );
    }

    #[test]
    fn identity_compares_tagged_values() {
        exc::init();
        assert_eq!(binop(OP_IS, NONE, NONE), TRUE);
        assert_eq!(binop(OP_IS, NONE, int(0)), FALSE);
        assert_eq!(binop(OP_IS_NOT, NONE, int(0)), TRUE);
        assert_eq!(binop(OP_IS, TRUE, TRUE), TRUE);
        assert_eq!(binop(OP_IS, TRUE, FALSE), FALSE);
        // Small integers are tagged immediates, so equal values are identical.
        assert_eq!(binop(OP_IS, int(7), int(7)), TRUE);
        // Heap values compare by pointer, not by content.
        let a = string("abc");
        let same = a;
        assert_eq!(binop(OP_IS, a, same), TRUE);
        assert_eq!(binop(OP_IS_NOT, a, string("abc")), TRUE);
        assert!(!exc::pending());
    }

    #[test]
    fn nan_comparisons_are_false() {
        exc::init();
        let nan = value::float_new(f64::NAN);
        assert_eq!(binop(OP_EQ, nan, nan), FALSE);
        assert_eq!(binop(OP_NE, nan, nan), TRUE);
        assert_eq!(binop(OP_LT, nan, value::float_new(1.0)), FALSE);
        assert!(!exc::pending());
    }

    #[test]
    fn booleans_are_numeric() {
        exc::init();
        assert_eq!(as_int(binop(OP_ADD, TRUE, TRUE)), 2);
        assert_eq!(binop(OP_EQ, TRUE, int(1)), TRUE);
        assert_eq!(as_int(unop(OP_NEG, TRUE)), -1);
    }

    #[test]
    fn container_operators() {
        exc::init();
        let a = value::list_new(vec![int(1), int(2)]);
        let b = value::list_new(vec![int(3)]);
        let joined = binop(OP_ADD, a, b);
        assert_eq!(value::as_list(joined), Some(&[int(1), int(2), int(3)][..]));
        // The operands are untouched (fresh list).
        assert_eq!(value::as_list(a), Some(&[int(1), int(2)][..]));
        assert!(!exc::pending());

        // Structural equality, including order-insensitive dicts.
        let same = value::list_new(vec![int(1), int(2)]);
        assert_eq!(binop(OP_EQ, a, same), TRUE);
        assert_eq!(binop(OP_NE, a, same), FALSE);
        assert_eq!(binop(OP_EQ, a, b), FALSE);

        let keys_a = [value::str_from_bytes(b"a"), value::str_from_bytes(b"b")];
        let values_a = [int(1), int(2)];
        let keys_b = [value::str_from_bytes(b"b"), value::str_from_bytes(b"a")];
        let values_b = [int(2), int(1)];
        let d1 = value::dict_new(vec![(keys_a[0], values_a[0]), (keys_a[1], values_a[1])]);
        let d2 = value::dict_new(vec![(keys_b[0], values_b[0]), (keys_b[1], values_b[1])]);
        assert_eq!(binop(OP_EQ, d1, d2), TRUE);
        assert_eq!(binop(OP_NE, d1, d2), FALSE);
        assert_eq!(binop(OP_EQ, d1, a), FALSE);

        // Lists compare lexicographically.
        assert_eq!(binop(OP_LT, a, b), TRUE);
        assert_eq!(binop(OP_GT, a, b), FALSE);
    }

    #[test]
    fn list_add_rejects_non_lists() {
        exc::init();
        let list = value::list_new(vec![int(1)]);
        let bad = binop(OP_ADD, list, int(2));
        assert_eq!(bad, NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "unsupported operand type(s) for +: 'list' and 'int'"
        );
    }

    #[test]
    fn tuple_operators() {
        exc::init();
        let a = value::tuple_new(vec![int(1), int(2)]);
        let b = value::tuple_new(vec![int(3)]);
        let joined = binop(OP_ADD, a, b);
        assert_eq!(value::as_tuple(joined), Some(&[int(1), int(2), int(3)][..]));
        assert!(!exc::pending());

        let repeated = binop(OP_MUL, a, int(2));
        assert_eq!(
            value::as_tuple(repeated),
            Some(&[int(1), int(2), int(1), int(2)][..])
        );
        let repeated2 = binop(OP_MUL, int(2), a);
        assert_eq!(
            value::as_tuple(repeated2),
            Some(&[int(1), int(2), int(1), int(2)][..])
        );
        assert_eq!(value::as_tuple(binop(OP_MUL, a, int(-2))), Some(&[][..]));
        assert_eq!(value::as_tuple(binop(OP_MUL, a, int(0))), Some(&[][..]));
        assert_eq!(
            value::as_tuple(binop(OP_MUL, value::tuple_new(Vec::new()), int(1000))),
            Some(&[][..])
        );
        assert!(!exc::pending());

        // Structural equality; tuples never equal lists; ordering raises.
        assert_eq!(
            binop(OP_EQ, a, value::tuple_new(vec![int(1), int(2)])),
            TRUE
        );
        assert_eq!(
            binop(OP_NE, a, value::tuple_new(vec![int(1), int(3)])),
            TRUE
        );
        assert_eq!(
            binop(OP_EQ, a, value::list_new(vec![int(1), int(2)])),
            FALSE
        );
        assert!(!exc::pending());
        // Tuples compare lexicographically.
        assert_eq!(binop(OP_LT, a, b), TRUE);
        assert_eq!(binop(OP_GT, a, b), FALSE);

        // Bad operand combinations keep the standard TypeError.
        exc::init();
        binop(OP_ADD, a, int(1));
        assert_eq!(
            pending_message(),
            "unsupported operand type(s) for +: 'tuple' and 'int'"
        );
    }

    #[test]
    fn tuple_repetition_overflow_is_reported() {
        exc::init();
        let a = value::tuple_new(vec![int(1), int(2)]);
        let v = binop(OP_MUL, a, int(value::MAX_INT));
        assert_eq!(v, NONE);
        assert_eq!(exc::pending_tag(), OVERFLOW_ERROR as i64);
    }

    #[test]
    fn unary_operators() {
        exc::init();
        assert_eq!(as_int(unop(OP_NEG, int(5))), -5);
        assert_eq!(as_int(unop(OP_POS, int(5))), 5);
        assert_eq!(as_float(unop(OP_NEG, value::float_new(2.5))), -2.5);
        assert_eq!(unop(OP_NOT, int(0)), TRUE);
        assert_eq!(unop(OP_NOT, int(3)), FALSE);
        assert_eq!(unop(OP_NOT, NONE), TRUE);
        assert_eq!(unop(OP_NOT, value::str_from_bytes(b"")), TRUE);
        assert_eq!(unop(OP_NOT, value::float_new(0.0)), TRUE);
        assert_eq!(unop(OP_NOT, value::float_new(0.5)), FALSE);

        unop(OP_NEG, value::str_from_bytes(b"x"));
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    fn string(s: &str) -> i64 {
        value::str_from_bytes(s.as_bytes())
    }

    fn list(items: &[i64]) -> i64 {
        value::list_new(items.to_vec())
    }

    fn tuple(items: &[i64]) -> i64 {
        value::tuple_new(items.to_vec())
    }

    fn set(items: &[i64]) -> i64 {
        value::set_new(items.to_vec())
    }

    fn dict(pairs: &[(i64, i64)]) -> i64 {
        value::dict_new(pairs.to_vec())
    }

    #[test]
    fn membership_over_all_iterable_types() {
        exc::init();
        let items = list(&[int(1), int(2), int(3)]);
        assert_eq!(binop(OP_IN, int(2), items), TRUE);
        assert_eq!(binop(OP_IN, int(9), items), FALSE);
        assert_eq!(binop(OP_NOT_IN, int(9), items), TRUE);
        assert_eq!(binop(OP_NOT_IN, int(2), items), FALSE);

        let pair = tuple(&[int(1), int(2)]);
        assert_eq!(binop(OP_IN, int(1), pair), TRUE);
        assert_eq!(binop(OP_NOT_IN, int(7), pair), TRUE);

        let text = string("abc");
        assert_eq!(binop(OP_IN, string("b"), text), TRUE);
        assert_eq!(binop(OP_IN, string("bc"), text), TRUE);
        assert_eq!(binop(OP_IN, string("z"), text), FALSE);
        assert_eq!(binop(OP_NOT_IN, string("z"), text), TRUE);

        let d = dict(&[(string("a"), int(1))]);
        assert_eq!(binop(OP_IN, string("a"), d), TRUE);
        assert_eq!(binop(OP_IN, string("b"), d), FALSE);

        let s = set(&[int(1), int(2)]);
        assert_eq!(binop(OP_IN, int(1), s), TRUE);
        assert_eq!(binop(OP_IN, int(3), s), FALSE);
        assert_eq!(binop(OP_NOT_IN, int(3), s), TRUE);
        assert!(!exc::pending());
    }

    #[test]
    fn membership_error_messages() {
        exc::init();
        assert_eq!(binop(OP_IN, int(1), string("abc")), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "'in <string>' requires string as left operand, not int"
        );

        exc::init();
        assert_eq!(binop(OP_IN, int(1), int(2)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(pending_message(), "argument of type 'int' is not iterable");

        // None is hashable, so this is a plain lookup and yields False.
        exc::init();
        assert_eq!(binop(OP_IN, NONE, set(&[int(1)])), FALSE);
        assert!(!exc::pending());

        let unhashable = list(&[int(1)]);
        assert_eq!(binop(OP_IN, unhashable, set(&[int(1)])), NONE);
        assert_eq!(pending_message(), "unhashable type: 'list'");

        exc::init();
        assert_eq!(binop(OP_IN, unhashable, dict(&[(int(1), int(2))])), NONE);
        assert_eq!(pending_message(), "unhashable type: 'list'");
    }

    #[test]
    fn range_membership() {
        exc::init();
        let ascending = value::range_new(0, 10, 1);
        let stepped = value::range_new(0, 10, 2);
        let descending = value::range_new(5, 0, -2);

        assert_eq!(binop(OP_IN, int(5), ascending), TRUE);
        assert_eq!(binop(OP_IN, int(10), ascending), FALSE);
        assert_eq!(binop(OP_IN, int(-1), ascending), FALSE);
        assert_eq!(binop(OP_IN, int(0), stepped), TRUE);
        assert_eq!(binop(OP_IN, int(2), stepped), TRUE);
        assert_eq!(binop(OP_IN, int(1), stepped), FALSE);
        assert_eq!(binop(OP_IN, int(5), descending), TRUE);
        assert_eq!(binop(OP_IN, int(3), descending), TRUE);
        assert_eq!(binop(OP_IN, int(4), descending), FALSE);
        assert_eq!(binop(OP_IN, TRUE, stepped), FALSE);
        assert_eq!(binop(OP_IN, int(1), value::range_new(0, 10, 1)), TRUE);
        assert_eq!(binop(OP_NOT_IN, int(10), ascending), TRUE);

        // Integral floats compare numerically; other values are absent.
        assert_eq!(binop(OP_IN, value::float_new(3.0), ascending), TRUE);
        assert_eq!(binop(OP_IN, value::float_new(3.5), ascending), FALSE);
        assert_eq!(binop(OP_IN, value::float_new(f64::NAN), ascending), FALSE);
        let a = value::str_from_bytes(b"a");
        assert_eq!(binop(OP_IN, a, ascending), FALSE);
        assert!(!exc::pending());

        // Ranges are not subscriptable (frontend rejects them statically).
        assert_eq!(crate::container::subscript(ascending, int(0)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(pending_message(), "'range' object is not subscriptable");
    }

    #[test]
    fn set_difference_and_unsupported_add() {
        exc::init();
        let a = set(&[int(1), int(2), int(3)]);
        let b = set(&[int(2), int(3), int(4)]);
        let diff = binop(OP_SUB, a, b);
        assert_eq!(value::as_set(diff), Some(&[int(1)][..]));

        assert_eq!(binop(OP_SUB, a, int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "unsupported operand type(s) for -: 'set' and 'int'"
        );

        exc::init();
        assert_eq!(binop(OP_ADD, a, b), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "unsupported operand type(s) for +: 'set' and 'set'"
        );
    }

    #[test]
    fn bitwise_integer_and_bool_semantics() {
        exc::init();
        assert_eq!(as_int(binop(OP_BIT_AND, int(6), int(3))), 2);
        assert_eq!(as_int(binop(OP_BIT_OR, int(6), int(3))), 7);
        assert_eq!(as_int(binop(OP_BIT_XOR, int(6), int(3))), 5);
        assert_eq!(as_int(binop(OP_SHL, int(1), int(4))), 16);
        assert_eq!(as_int(binop(OP_SHR, int(-8), int(1))), -4);
        assert!(!exc::pending());

        // Two booleans keep the boolean result; a mixed pair is an int.
        assert_eq!(binop(OP_BIT_AND, TRUE, TRUE), TRUE);
        assert_eq!(binop(OP_BIT_AND, TRUE, FALSE), FALSE);
        assert_eq!(binop(OP_BIT_OR, FALSE, FALSE), FALSE);
        assert_eq!(binop(OP_BIT_XOR, TRUE, TRUE), FALSE);
        assert_eq!(binop(OP_BIT_OR, TRUE, int(2)), int(3));
        assert!(!exc::pending());
    }

    #[test]
    fn bitwise_rejects_non_integers() {
        exc::init();
        let text = value::str_from_bytes(b"x");
        assert_eq!(binop(OP_BIT_AND, text, int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "unsupported operand type(s) for &: 'str' and 'int'"
        );

        exc::init();
        assert_eq!(binop(OP_BIT_OR, value::float_new(1.5), int(1)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "unsupported operand type(s) for |: 'float' and 'int'"
        );

        exc::init();
        assert_eq!(binop(OP_SHL, int(1), text), NONE);
        assert_eq!(
            pending_message(),
            "unsupported operand type(s) for <<: 'int' and 'str'"
        );
    }

    #[test]
    fn shift_counts_are_bounded() {
        exc::init();
        assert_eq!(binop(OP_SHL, int(1), int(-1)), NONE);
        assert_eq!(exc::pending_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(pending_message(), "negative shift count");

        exc::init();
        assert_eq!(binop(OP_SHR, int(1), int(-1)), NONE);
        assert_eq!(exc::pending_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(pending_message(), "negative shift count");

        exc::init();
        assert_eq!(binop(OP_SHL, int(value::MAX_INT), int(1)), NONE);
        assert_eq!(exc::pending_tag(), OVERFLOW_ERROR as i64);
        assert_eq!(pending_message(), "integer overflow");

        // Huge right shifts saturate like Python.
        exc::init();
        assert_eq!(as_int(binop(OP_SHR, int(-1), int(100))), -1);
        assert_eq!(as_int(binop(OP_SHR, int(1), int(100))), 0);
        assert!(!exc::pending());
    }

    #[test]
    fn invert_is_defined_for_ints_and_bools() {
        exc::init();
        assert_eq!(as_int(unop(OP_INVERT, int(5))), -6);
        assert_eq!(as_int(unop(OP_INVERT, int(0))), -1);
        assert_eq!(as_int(unop(OP_INVERT, int(-6))), 5);
        // `bool` is numerically an int.
        assert_eq!(as_int(unop(OP_INVERT, TRUE)), -2);
        assert_eq!(as_int(unop(OP_INVERT, FALSE)), -1);
        assert!(!exc::pending());

        exc::init();
        assert_eq!(unop(OP_INVERT, value::float_new(1.5)), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(pending_message(), "bad operand type for unary ~: 'float'");

        exc::init();
        assert_eq!(unop(OP_INVERT, value::str_from_bytes(b"x")), NONE);
        assert_eq!(pending_message(), "bad operand type for unary ~: 'str'");
    }
}
