//! Python operator semantics for tagged values (ARCHITECTURE §3.2).
//!
//! All fallible operations report errors as a pending exception and return a
//! dummy `None` value; the codegen polls `hydro_exc_pending()` afterwards.

use std::cmp::Ordering;

use num_bigint::{BigInt, Sign};
use num_traits::ToPrimitive;

use crate::container;
use crate::core::builtins;
use crate::exc::{self, OVERFLOW_ERROR, TYPE_ERROR, ZERO_DIVISION_ERROR};
use crate::value::{self, Num};
use hydro_ir::MethodKind;

// `hydro_binop` op codes.
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
/// `@`: `__matmul__`/`__rmatmul__` dispatch with a numeric scalar fallback.
pub(crate) const OP_MATMUL: u32 = 22;
/// `@=`: like [`OP_MATMUL`] but tries `__imatmul__` first.
pub(crate) const OP_MATMUL_INPLACE: u32 = 23;

// `hydro_unop` op codes.
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

/// Encodes a Python integer result: a tagged immediate when it fits, otherwise
/// a heap `BigInt` (promotion instead of `OverflowError`).
pub(crate) fn int_result(n: i64) -> i64 {
    match value::encode_int(n) {
        Some(v) => v,
        None => value::bigint_new(BigInt::from(n)),
    }
}

/// Raised when an exact `BigInt` operand cannot be represented as a float and
/// the operation (mixed arithmetic or true division) would need one.
fn float_overflow() -> i64 {
    exc::raise_with_message(OVERFLOW_ERROR, "int too large to convert to float")
}

/// Practical size cap for promoted results (`pow`, left shift): 8 Mibit
/// (~1 MiB of magnitude). Larger results raise `OverflowError` instead of
/// exhausting memory, mirroring CPython's `int`-size guards.
const MAX_BIGINT_BITS: u64 = 1 << 23;
/// Practical exponent cap for `BigInt ** BigInt` when the base is not one of
/// `0`, `1`, `-1`; larger exponents raise `OverflowError`.
const MAX_POW_EXP: u32 = 1 << 20;
/// Extra quotient bits kept by true division of out-of-`f64` integers.
const DIV_SCALE: u32 = 54;

/// Numeric operand of the exact-integer paths. Tagged ints, booleans and heap
/// BigInts become `Int`; heap floats stay `Float`.
enum BigOperand {
    Int(BigInt),
    Float(f64),
}

/// Numeric view used only by the BigInt-aware paths: unlike [`value::num_of`]
/// it keeps integers that do not fit `i64`.
fn big_operand(v: i64) -> Option<BigOperand> {
    if let Some(n) = value::to_bigint(v) {
        return Some(BigOperand::Int(n));
    }
    value::as_float(v).map(BigOperand::Float)
}

/// True when either operand needs the arbitrary-precision path.
fn needs_bigint(a: i64, b: i64) -> bool {
    value::is_bigint(a) || value::is_bigint(b)
}

fn to_f64(n: Num) -> f64 {
    match n {
        Num::Int(i) => i as f64,
        Num::Float(f) => f,
    }
}

/// `(real, imag)` pair for a complex arithmetic operand pair, or `None` when
/// neither operand is a complex value and the ordinary numeric path applies.
fn complex_operands(a: i64, b: i64) -> Option<((f64, f64), (f64, f64))> {
    if !builtins::is_complex_instance(a) && !builtins::is_complex_instance(b) {
        return None;
    }
    Some((
        builtins::complex_components(a)?,
        builtins::complex_components(b)?,
    ))
}

/// `a / b` for complex operands, using CPython's `_Py_c_quot` (Smith's
/// algorithm), which avoids the intermediate overflow of `br² + bi²`.
fn complex_divide(a: i64, b: i64) -> Option<i64> {
    let ((ar, ai), (br, bi)) = complex_operands(a, b)?;
    if br == 0.0 && bi == 0.0 {
        return Some(zero_div("complex division by zero"));
    }
    let result = if br.abs() >= bi.abs() {
        let ratio = bi / br;
        let denom = br + bi * ratio;
        builtins::build_complex((ar + ai * ratio) / denom, (ai - ar * ratio) / denom)
    } else {
        let ratio = br / bi;
        let denom = bi + br * ratio;
        builtins::build_complex((ar * ratio + ai) / denom, (ai * ratio - ar) / denom)
    };
    Some(result)
}

/// `a ** b` when either operand is complex. Mirrors CPython: an exponent with
/// no imaginary part and a moderate integral value takes the exact repeated
/// squaring path; everything else is `exp(log(a) * b)` in polar form.
fn complex_pow(a: i64, b: i64) -> Option<i64> {
    let ((base_real, base_imag), (exp_real, exp_imag)) = complex_operands(a, b)?;
    if exp_imag == 0.0 && exp_real == exp_real.trunc() && exp_real.abs() <= 100.0 {
        let negative = exp_real < 0.0;
        let mut exponent = exp_real.abs() as u64;
        let mut result_real = 1.0f64;
        let mut result_imag = 0.0f64;
        let mut factor_real = base_real;
        let mut factor_imag = base_imag;
        while exponent > 0 {
            if exponent & 1 == 1 {
                let real = result_real * factor_real - result_imag * factor_imag;
                let imag = result_real * factor_imag + result_imag * factor_real;
                result_real = real;
                result_imag = imag;
            }
            exponent >>= 1;
            if exponent > 0 {
                let real = factor_real * factor_real - factor_imag * factor_imag;
                let imag = 2.0 * factor_real * factor_imag;
                factor_real = real;
                factor_imag = imag;
            }
        }
        if negative {
            let denominator = result_real * result_real + result_imag * result_imag;
            if denominator == 0.0 {
                return Some(zero_div("0.0 to a negative or complex power"));
            }
            return Some(builtins::build_complex(
                result_real / denominator,
                -result_imag / denominator,
            ));
        }
        return Some(builtins::build_complex(result_real, result_imag));
    }
    let modulus = base_real.hypot(base_imag);
    if modulus == 0.0 {
        if exp_real == 0.0 && exp_imag == 0.0 {
            return Some(builtins::build_complex(1.0, 0.0));
        }
        if exp_real < 0.0 || exp_imag != 0.0 {
            return Some(zero_div("0.0 to a negative or complex power"));
        }
        return Some(builtins::build_complex(0.0, 0.0));
    }
    let log_modulus = modulus.ln();
    let angle = base_imag.atan2(base_real);
    let magnitude = (log_modulus * exp_real - angle * exp_imag).exp();
    let phase = log_modulus * exp_imag + angle * exp_real;
    Some(builtins::build_complex(
        magnitude * phase.cos(),
        magnitude * phase.sin(),
    ))
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
    // PEP 604 type unions: `A | B` for class values, `None` and existing
    // union values (`types.UnionType`). Non-type operands fall through to the
    // native bitwise path and report its usual `TypeError`.
    if op == OP_BIT_OR {
        if let Some(result) = crate::modules::types::union_binary_or(a, b) {
            return result;
        }
    }
    // Set operators over sets and live `dict` views (`dict_keys`/`dict_items`).
    if matches!(op, OP_BIT_AND | OP_BIT_OR | OP_BIT_XOR | OP_SUB)
        && container::is_set_like(a)
        && container::is_set_like(b)
    {
        let set_op = match op {
            OP_BIT_AND => container::SetOp::And,
            OP_BIT_OR => container::SetOp::Or,
            OP_SUB => container::SetOp::Sub,
            _ => container::SetOp::Xor,
        };
        if let Some(result) = container::set_operation(set_op, a, b) {
            return result;
        }
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
        OP_MATMUL => matmul(a, b, false),
        OP_MATMUL_INPLACE => matmul(a, b, true),
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
    if needs_bigint(a, b) {
        return big_bitwise(op, a, b);
    }
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
/// tagged-integer range are promoted to a `BigInt` (larger than the practical
/// size cap raises `OverflowError`); right shifts saturate like Python's
/// arithmetic shift for arbitrarily large counts.
fn shift(a: i64, b: i64, left: bool) -> i64 {
    let symbol = if left { "<<" } else { ">>" };
    if needs_bigint(a, b) {
        return big_shift(a, b, left);
    }
    let (Some(base), Some(count)) = (value::as_integer(a), value::as_integer(b)) else {
        return arithmetic_type_error(symbol, a, b);
    };
    if count < 0 {
        return exc::raise_with_message(exc::VALUE_ERROR, "negative shift count");
    }
    if left {
        // Results beyond the tagged range are promoted to a BigInt instead of
        // raising; the BigInt path also enforces the practical size cap.
        let shifted = if count >= 64 {
            return big_shift(a, b, true);
        } else {
            let amount = count as u32;
            match base.checked_shl(amount) {
                // The reverse shift catches bits shifted out of the i64.
                Some(shifted) if (shifted >> amount) == base => shifted,
                _ => return big_shift(a, b, true),
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
    } else if let Some(view) = value::as_memoryview(right) {
        if view.released {
            return exc::raise_with_message(exc::VALUE_ERROR, value::MEMORYVIEW_RELEASED);
        }
        let mut found = false;
        for index in 0..view.len() {
            match value::memoryview_get(view, index) {
                Ok(item) => {
                    if container::equals(item, left) {
                        found = true;
                        break;
                    }
                }
                Err(message) => return container::memoryview_error(message),
            }
        }
        found
    } else if let Some(view) = value::as_dict_view(right) {
        // Keys and items require a hashable operand, like a dict lookup.
        if view.kind != value::DictViewKind::Values {
            if let Some(name) = container::unhashable_name(left) {
                return exc::raise_with_message(TYPE_ERROR, &container::unhashable_message(name));
            }
        }
        container::dict_view_contains(view, left)
    } else if let Some(items) = value::as_set(right) {
        if let Some(name) = container::unhashable_name(left) {
            return exc::raise_with_message(TYPE_ERROR, &container::unhashable_message(name));
        }
        container::set_contains(items, left)
    } else if let Some((start, stop, step)) = value::as_range(right) {
        range_contains(start, stop, step, left)
    } else if value::as_instance(right).is_some() {
        match instance_method_call(right, "__contains__", &[right, left]) {
            Some(result) => {
                if exc::pending() {
                    return result;
                }
                return value::bool_value(value::truthy(result) != negate);
            }
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "argument of type '{}' is not iterable",
                        value::type_name(right)
                    ),
                )
            }
        }
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

/// Calls `a.__op__(b)` (or `b.__rop__(a)` when the left operand has no
/// implementation) for user-class operands. Returns `None` when neither side
/// defines the dunder, so the native operator path can continue.
fn instance_dunder_binop(a: i64, b: i64, op: &str, reflected: &str) -> Option<i64> {
    if let Some(result) = instance_method_call(a, op, &[a, b]) {
        if !exc::pending() && result == value::NOT_IMPLEMENTED {
            // Fall through to the reflected method / native path.
        } else {
            return Some(result);
        }
    }
    if let Some(result) = instance_method_call(b, reflected, &[b, a]) {
        return Some(result);
    }
    None
}

/// Calls a normal user instance method with already-evaluated arguments.
pub(crate) fn instance_method_call(instance: i64, name: &str, args: &[i64]) -> Option<i64> {
    let state = value::as_instance(instance)?;
    let value::ClassEntry::Method(MethodKind::Normal, function) =
        value::class_lookup(state.class, name)?
    else {
        return None;
    };
    Some(builtins::call(function, args.len(), args.as_ptr()))
}

/// `a + b` with full Python semantics (`__add__`/`__radd__`, big integers,
/// numeric promotion, container concatenation). The string accumulator's
/// generic path reuses it so `s += value` behaves exactly like `s + value`.
pub(crate) fn add(a: i64, b: i64) -> i64 {
    if let Some(result) = instance_dunder_binop(a, b, "__add__", "__radd__") {
        // Both dunders declined: Python's operator protocol raises instead of
        // leaking the `NotImplemented` singleton to user code.
        return if result == value::NOT_IMPLEMENTED && !exc::pending() {
            arithmetic_type_error("+", a, b)
        } else {
            result
        };
    }
    if let Some(((ar, ai), (br, bi))) = complex_operands(a, b) {
        return builtins::build_complex(ar + br, ai + bi);
    }
    if needs_bigint(a, b) {
        return big_add(a, b);
    }
    if let (Some(x), Some(y)) = (value::num_of(a), value::num_of(b)) {
        return match (x, y) {
            (Num::Int(p), Num::Int(q)) => match p.checked_add(q) {
                Some(n) => int_result(n),
                None => big_add(a, b),
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
    // `bytearray + bytes-like` keeps the mutable result type; `bytes +
    // bytearray` stays `bytes` (the generic branch below).
    if value::as_bytearray(a).is_some() {
        if let (Some(xs), Some(ys)) = (value::as_bytes(a), value::as_bytes(b)) {
            return match xs.len().checked_add(ys.len()) {
                Some(n) if n <= isize::MAX as usize => {
                    let mut out = Vec::with_capacity(n);
                    out.extend_from_slice(xs);
                    out.extend_from_slice(ys);
                    value::bytearray_new(out)
                }
                _ => overflow(),
            };
        }
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

/// `a - b` with full Python semantics (`__sub__`/`__rsub__`, big integers,
/// numeric promotion). Mirrors [`add`].
pub(crate) fn sub(a: i64, b: i64) -> i64 {
    if let Some(result) = instance_dunder_binop(a, b, "__sub__", "__rsub__") {
        return if result == value::NOT_IMPLEMENTED && !exc::pending() {
            arithmetic_type_error("-", a, b)
        } else {
            result
        };
    }
    if let Some(((ar, ai), (br, bi))) = complex_operands(a, b) {
        return builtins::build_complex(ar - br, ai - bi);
    }
    if needs_bigint(a, b) {
        return big_sub(a, b);
    }
    if let (Some(x), Some(y)) = (value::num_of(a), value::num_of(b)) {
        return match (x, y) {
            (Num::Int(p), Num::Int(q)) => match p.checked_sub(q) {
                Some(n) => int_result(n),
                None => big_sub(a, b),
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

/// Sequence values whose `* int` repetition needs an index-sized count.
fn is_repeatable(v: i64) -> bool {
    value::as_str(v).is_some()
        || value::as_bytes(v).is_some()
        || value::as_list(v).is_some()
        || value::as_tuple(v).is_some()
}

fn mul(a: i64, b: i64) -> i64 {
    if let Some(((ar, ai), (br, bi))) = complex_operands(a, b) {
        return builtins::build_complex(ar * br - ai * bi, ar * bi + ai * br);
    }
    if needs_bigint(a, b) {
        if let Some(result) = big_mul(a, b) {
            return result;
        }
        // A sequence paired with a BigInt count cannot be repeated: the count
        // does not fit an index-sized integer.
        if is_repeatable(a) || is_repeatable(b) {
            return overflow();
        }
        return arithmetic_type_error("*", a, b);
    }
    if let (Some(x), Some(y)) = (value::num_of(a), value::num_of(b)) {
        return match (x, y) {
            (Num::Int(p), Num::Int(q)) => match p.checked_mul(q) {
                Some(n) => int_result(n),
                None => big_mul(a, b).unwrap_or_else(overflow),
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
    // `bytearray * int` / `int * bytearray` (checked before the bytes-like
    // form so the result type stays mutable).
    if let (Some(bytes), Some(n)) = (value::as_bytearray(a), value::as_integer(b)) {
        return repeat_bytearray(bytes, n);
    }
    if let (Some(n), Some(bytes)) = (value::as_integer(a), value::as_bytearray(b)) {
        return repeat_bytearray(bytes, n);
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

/// `a @ b` / `a @= b`.
///
/// The operator is a protocol hook first: a user `__matmul__` on the left
/// operand, then `__rmatmul__` on the right one. `@=` additionally tries the
/// left operand's `__imatmul__` before those reflected forms, like CPython.
/// Scalar operands (int/float/bool/BigInt) fall back to multiplication so
/// numpy-style scalar products work; every other operand pair raises the
/// standard tagged `TypeError`.
fn matmul(a: i64, b: i64, inplace: bool) -> i64 {
    if inplace {
        if let Some(result) = instance_method_call(a, "__imatmul__", &[a, b]) {
            if exc::pending() || result != value::NOT_IMPLEMENTED {
                return result;
            }
        }
    }
    if let Some(result) = instance_dunder_binop(a, b, "__matmul__", "__rmatmul__") {
        return result;
    }
    let numeric = |v: i64| value::num_of(v).is_some() || value::is_bigint(v);
    if numeric(a) && numeric(b) {
        return mul(a, b);
    }
    arithmetic_type_error(if inplace { "@=" } else { "@" }, a, b)
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

/// `bytearray * int` / `int * bytearray`: the repetition stays mutable.
fn repeat_bytearray(bytes: &[u8], n: i64) -> i64 {
    if n <= 0 || bytes.is_empty() {
        return value::bytearray_new(Vec::new());
    }
    let count = n as usize;
    match bytes.len().checked_mul(count) {
        Some(total) if total <= isize::MAX as usize => {
            let mut out = Vec::with_capacity(total);
            for _ in 0..count {
                out.extend_from_slice(bytes);
            }
            value::bytearray_new(out)
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
    if let Some(result) = complex_divide(a, b) {
        return result;
    }
    if needs_bigint(a, b) {
        return big_div(a, b);
    }
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
    if needs_bigint(a, b) {
        return big_floor_div(a, b);
    }
    match (value::num_of(a), value::num_of(b)) {
        (Some(Num::Int(p)), Some(Num::Int(q))) => {
            if q == 0 {
                return zero_div("integer division or modulo by zero");
            }
            let (Some(quo), Some(rem)) = (p.checked_div(q), p.checked_rem(q)) else {
                // `i64::MIN / -1` is the one truncated case that does not fit;
                // the exact value is representable as a BigInt.
                return big_floor_div(a, b);
            };
            // Python floors towards negative infinity; `Rust`'s `/` truncates.
            let floored = if rem != 0 && ((rem < 0) != (q < 0)) {
                match quo.checked_sub(1) {
                    Some(n) => n,
                    None => return big_floor_div(a, b),
                }
            } else {
                quo
            };
            int_result(floored)
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
    if needs_bigint(a, b) {
        return big_modulo(a, b);
    }
    match (value::num_of(a), value::num_of(b)) {
        (Some(Num::Int(p)), Some(Num::Int(q))) => {
            if q == 0 {
                return zero_div("integer modulo by zero");
            }
            let Some(rem) = p.checked_rem(q) else {
                // `i64::MIN % -1` is zero although the primitive overflows.
                return big_modulo(a, b);
            };
            let adjusted = if rem != 0 && ((rem < 0) != (q < 0)) {
                match rem.checked_add(q) {
                    Some(n) => n,
                    None => return big_modulo(a, b),
                }
            } else {
                rem
            };
            int_result(adjusted)
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
    if let Some(result) = complex_pow(a, b) {
        return result;
    }
    if needs_bigint(a, b) {
        return big_pow(a, b);
    }
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

fn int_pow(base: i64, exp: u64) -> i64 {
    // The exponent is shifted inside the loop; promotion needs the original.
    let original_exp = BigInt::from(exp);
    let mut exp = exp;
    let mut acc: i64 = 1;
    let mut factor = base;
    while exp > 0 {
        if exp & 1 == 1 {
            match acc.checked_mul(factor) {
                Some(v) => acc = v,
                None => return big_int_pow(BigInt::from(base), original_exp.clone()),
            }
        }
        exp >>= 1;
        if exp > 0 {
            match factor.checked_mul(factor) {
                Some(v) => factor = v,
                // The squared factor is a factor of the final result, so the
                // exact product no longer fits the tagged range.
                None => return big_int_pow(BigInt::from(base), original_exp.clone()),
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
// Arbitrary-precision integer operations
// -------------------------------------------------------------------------

/// Converts a numeric operand to `f64`; `None` when an integer operand is too
/// large (the caller raises `OverflowError`, like CPython).
fn big_operand_to_f64(operand: &BigOperand) -> Option<f64> {
    match operand {
        BigOperand::Int(n) => value::bigint_to_f64(n),
        BigOperand::Float(f) => Some(*f),
    }
}

/// Truncating integer division and remainder (Rust/`C` semantics: the
/// quotient rounds toward zero and the remainder takes the dividend's sign).
fn big_trunc_div_rem(left: &BigInt, right: &BigInt) -> (BigInt, BigInt) {
    (left / right, left % right)
}

/// Floored quotient and remainder for exact integers (Python's `divmod`
/// sign rules): the quotient rounds toward negative infinity and the
/// remainder takes the divisor's sign.
pub(crate) fn integer_floor_divmod(left: &BigInt, right: &BigInt) -> (BigInt, BigInt) {
    let (quotient, remainder) = big_trunc_div_rem(left, right);
    if remainder.sign() != Sign::NoSign && remainder.sign() != right.sign() {
        (quotient - BigInt::from(1), remainder + right)
    } else {
        (quotient, remainder)
    }
}

fn big_add(a: i64, b: i64) -> i64 {
    match (big_operand(a), big_operand(b)) {
        (Some(BigOperand::Int(x)), Some(BigOperand::Int(y))) => value::bigint_new(x + y),
        (Some(BigOperand::Int(x)), Some(BigOperand::Float(y))) => match value::bigint_to_f64(&x) {
            Some(f) => value::float_new(f + y),
            None => float_overflow(),
        },
        (Some(BigOperand::Float(x)), Some(BigOperand::Int(y))) => match value::bigint_to_f64(&y) {
            Some(f) => value::float_new(x + f),
            None => float_overflow(),
        },
        (Some(BigOperand::Float(x)), Some(BigOperand::Float(y))) => value::float_new(x + y),
        _ => arithmetic_type_error("+", a, b),
    }
}

fn big_sub(a: i64, b: i64) -> i64 {
    match (big_operand(a), big_operand(b)) {
        (Some(BigOperand::Int(x)), Some(BigOperand::Int(y))) => value::bigint_new(x - y),
        (Some(BigOperand::Int(x)), Some(BigOperand::Float(y))) => match value::bigint_to_f64(&x) {
            Some(f) => value::float_new(f - y),
            None => float_overflow(),
        },
        (Some(BigOperand::Float(x)), Some(BigOperand::Int(y))) => match value::bigint_to_f64(&y) {
            Some(f) => value::float_new(x - f),
            None => float_overflow(),
        },
        (Some(BigOperand::Float(x)), Some(BigOperand::Float(y))) => value::float_new(x - y),
        _ => arithmetic_type_error("-", a, b),
    }
}

/// Exact product, or `None` when `a`/`b` is not numeric (the caller decides
/// between the sequence-repetition error and `TypeError`).
fn big_mul(a: i64, b: i64) -> Option<i64> {
    Some(match (big_operand(a)?, big_operand(b)?) {
        (BigOperand::Int(x), BigOperand::Int(y)) => value::bigint_new(x * y),
        (BigOperand::Int(x), BigOperand::Float(y)) => match value::bigint_to_f64(&x) {
            Some(f) => value::float_new(f * y),
            None => float_overflow(),
        },
        (BigOperand::Float(x), BigOperand::Int(y)) => match value::bigint_to_f64(&y) {
            Some(f) => value::float_new(x * f),
            None => float_overflow(),
        },
        (BigOperand::Float(x), BigOperand::Float(y)) => value::float_new(x * y),
    })
}

fn big_div(a: i64, b: i64) -> i64 {
    match (big_operand(a), big_operand(b)) {
        (Some(BigOperand::Int(x)), Some(BigOperand::Int(y))) => {
            if y.sign() == Sign::NoSign {
                return zero_div("division by zero");
            }
            let denominator = value::bigint_to_f64(&y).unwrap_or_else(|| {
                if y.sign() == Sign::Minus {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                }
            });
            let Some(numerator) = value::bigint_to_f64(&x) else {
                // Both operands may exceed the `f64` range while their ratio
                // does not (`10**400 / 10**400 == 1.0`). Scale the exact
                // quotient by `2^54` so the result keeps full precision.
                let scaled = (&x << DIV_SCALE) / &y;
                let Some(quotient) = value::bigint_to_f64(&scaled) else {
                    return float_overflow();
                };
                let scale = 2f64.powi(DIV_SCALE as i32);
                return value::float_new(quotient / scale);
            };
            // A denominator too large for `f64` still divides exactly to a
            // signed zero, so it becomes an infinity instead of an error.
            value::float_new(numerator / denominator)
        }
        (Some(BigOperand::Int(x)), Some(BigOperand::Float(y))) => {
            if y == 0.0 {
                return zero_div("division by zero");
            }
            match value::bigint_to_f64(&x) {
                Some(f) => value::float_new(f / y),
                None => float_overflow(),
            }
        }
        (Some(BigOperand::Float(x)), Some(BigOperand::Int(y))) => {
            if y.sign() == Sign::NoSign {
                return zero_div("division by zero");
            }
            let denominator = value::bigint_to_f64(&y).unwrap_or_else(|| {
                if y.sign() == Sign::Minus {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                }
            });
            value::float_new(x / denominator)
        }
        (Some(BigOperand::Float(x)), Some(BigOperand::Float(y))) => {
            if y == 0.0 {
                return zero_div("division by zero");
            }
            value::float_new(x / y)
        }
        _ => arithmetic_type_error("/", a, b),
    }
}

fn big_floor_div(a: i64, b: i64) -> i64 {
    match (big_operand(a), big_operand(b)) {
        (Some(BigOperand::Int(p)), Some(BigOperand::Int(q))) => {
            if q.sign() == Sign::NoSign {
                return zero_div("integer division or modulo by zero");
            }
            let (quotient, _) = integer_floor_divmod(&p, &q);
            value::bigint_new(quotient)
        }
        (Some(x), Some(y)) => {
            let Some(denominator) = big_operand_to_f64(&y) else {
                return float_overflow();
            };
            if denominator == 0.0 {
                return zero_div("integer division or modulo by zero");
            }
            let Some(numerator) = big_operand_to_f64(&x) else {
                return float_overflow();
            };
            value::float_new((numerator / denominator).floor())
        }
        _ => arithmetic_type_error("//", a, b),
    }
}

fn big_modulo(a: i64, b: i64) -> i64 {
    match (big_operand(a), big_operand(b)) {
        (Some(BigOperand::Int(p)), Some(BigOperand::Int(q))) => {
            if q.sign() == Sign::NoSign {
                return zero_div("integer modulo by zero");
            }
            let (_, remainder) = integer_floor_divmod(&p, &q);
            value::bigint_new(remainder)
        }
        (Some(x), Some(y)) => {
            let Some(denominator) = big_operand_to_f64(&y) else {
                return float_overflow();
            };
            if denominator == 0.0 {
                return zero_div("integer modulo by zero");
            }
            let Some(numerator) = big_operand_to_f64(&x) else {
                return float_overflow();
            };
            let mut remainder = numerator % denominator;
            if remainder != 0.0 && ((remainder < 0.0) != (denominator < 0.0)) {
                remainder += denominator;
            }
            value::float_new(remainder)
        }
        _ => arithmetic_type_error("%", a, b),
    }
}

fn big_bitwise(op: u32, a: i64, b: i64) -> i64 {
    let symbol = match op {
        OP_BIT_AND => "&",
        OP_BIT_OR => "|",
        _ => "^",
    };
    match (big_operand(a), big_operand(b)) {
        (Some(BigOperand::Int(x)), Some(BigOperand::Int(y))) => {
            let result = match op {
                OP_BIT_AND => x & y,
                OP_BIT_OR => x | y,
                _ => x ^ y,
            };
            value::bigint_new(result)
        }
        _ => arithmetic_type_error(symbol, a, b),
    }
}

fn big_shift(a: i64, b: i64, left: bool) -> i64 {
    let symbol = if left { "<<" } else { ">>" };
    let (Some(base), Some(count)) = (value::to_bigint(a), value::to_bigint(b)) else {
        return arithmetic_type_error(symbol, a, b);
    };
    if count.sign() == Sign::Minus {
        return exc::raise_with_message(exc::VALUE_ERROR, "negative shift count");
    }
    if left {
        if base.sign() == Sign::NoSign {
            return int_result(0);
        }
        let Some(shift) = count.to_usize() else {
            return overflow();
        };
        // Cap promoted results instead of exhausting memory; CPython raises
        // `OverflowError`/`MemoryError` for shifts of this size as well.
        if base.bits().saturating_add(shift as u64) > MAX_BIGINT_BITS {
            return overflow();
        }
        value::bigint_new(base << shift)
    } else {
        // Right shifts saturate like Python's arithmetic shift when the count
        // is enormous.
        let Some(shift) = count.to_usize() else {
            return int_result(if base.sign() == Sign::Minus { -1 } else { 0 });
        };
        value::bigint_new(base >> shift)
    }
}

fn big_pow(a: i64, b: i64) -> i64 {
    match (big_operand(a), big_operand(b)) {
        (Some(BigOperand::Int(base)), Some(BigOperand::Int(exp))) => big_int_pow(base, exp),
        (Some(x), Some(y)) => {
            let Some(base) = big_operand_to_f64(&x) else {
                return float_overflow();
            };
            let Some(exp) = big_operand_to_f64(&y) else {
                return float_overflow();
            };
            float_pow(base, exp)
        }
        _ => arithmetic_type_error("**", a, b),
    }
}

/// `base ** exp` for exact integers, with Python's rules: `n ** 0 == 1`,
/// negative exponents produce a float and huge exponents raise
/// `OverflowError` unless the base makes the result trivial.
fn big_int_pow(base: BigInt, exp: BigInt) -> i64 {
    if exp.sign() == Sign::NoSign {
        return int_result(1);
    }
    if exp.sign() == Sign::Minus {
        let Some(base) = value::bigint_to_f64(&base) else {
            return float_overflow();
        };
        let exp = value::bigint_to_f64(&exp).unwrap_or(f64::NEG_INFINITY);
        return float_pow(base, exp);
    }
    if base.sign() == Sign::NoSign {
        return int_result(0);
    }
    let one = BigInt::from(1);
    if base == one {
        return int_result(1);
    }
    if base == -&one {
        let odd = (&exp & &one) == one;
        return int_result(if odd { -1 } else { 1 });
    }
    let Some(exp) = exp.to_u32() else {
        return overflow();
    };
    if exp > MAX_POW_EXP || (base.bits() as u128) * (exp as u128) > MAX_BIGINT_BITS as u128 {
        return overflow();
    }
    value::bigint_new(big_pow_small(base, exp))
}

/// Exponentiation by squaring over `BigInt`.
fn big_pow_small(mut base: BigInt, mut exp: u32) -> BigInt {
    let mut acc = BigInt::from(1);
    while exp > 0 {
        if exp & 1 == 1 {
            acc *= &base;
        }
        exp >>= 1;
        if exp > 0 {
            base = &base * &base;
        }
    }
    acc
}

/// Exact numeric ordering for operands that may exceed `i64`; `None` means
/// unordered (NaN).
fn cmp_big(a: BigOperand, b: BigOperand) -> Option<Ordering> {
    match (a, b) {
        (BigOperand::Int(x), BigOperand::Int(y)) => Some(x.cmp(&y)),
        (BigOperand::Float(x), BigOperand::Float(y)) => x.partial_cmp(&y),
        (BigOperand::Int(x), BigOperand::Float(y)) => value::bigint_cmp_f64(&x, y),
        (BigOperand::Float(x), BigOperand::Int(y)) => {
            value::bigint_cmp_f64(&y, x).map(Ordering::reverse)
        }
    }
}

// -------------------------------------------------------------------------
// Comparisons
// -------------------------------------------------------------------------

/// User-class rich comparisons. `__eq__`/`__ne__` also fall back to the
/// reflected method, matching Python's budgeted method lookup for this slice.
fn instance_comparison(op: u32, a: i64, b: i64) -> Option<i64> {
    let (method, reflected, invert) = match op {
        OP_EQ => ("__eq__", "__eq__", false),
        OP_NE => ("__eq__", "__eq__", true),
        OP_LT => ("__lt__", "__gt__", false),
        OP_LE => ("__le__", "__ge__", false),
        OP_GT => ("__gt__", "__lt__", false),
        OP_GE => ("__ge__", "__le__", false),
        _ => return None,
    };
    let result = instance_method_call(a, method, &[a, b])
        .or_else(|| instance_method_call(b, reflected, &[b, a]))?;
    if exc::pending() {
        return Some(result);
    }
    if result == value::NOT_IMPLEMENTED {
        return None;
    }
    if invert {
        Some(value::bool_value(!value::truthy(result)))
    } else {
        Some(value::bool_value(value::truthy(result)))
    }
}

fn compare(op: u32, a: i64, b: i64) -> i64 {
    if let Some(result) = instance_comparison(op, a, b) {
        return result;
    }
    if needs_bigint(a, b) {
        if let (Some(x), Some(y)) = (big_operand(a), big_operand(b)) {
            return ordered_result(op, cmp_big(x, y));
        }
    }
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
    // Live views compare structurally too (`memoryview` against bytes-like
    // values, `dict_keys`/`dict_items` as sets and `dict_values` by identity).
    if matches!(op, OP_EQ | OP_NE)
        && (value::as_memoryview(a).is_some()
            || value::as_memoryview(b).is_some()
            || value::as_dict_view(a).is_some()
            || value::as_dict_view(b).is_some())
    {
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
            if builtins::is_complex_instance(a) {
                if let Some((real, imag)) = builtins::complex_components(a) {
                    return builtins::build_complex(-real, -imag);
                }
            }
            if let Some(n) = value::as_bigint(a) {
                return value::bigint_new(-n);
            }
            if let Some(n) = value::as_integer(a) {
                return match n.checked_neg() {
                    Some(v) => int_result(v),
                    // `i64::MIN` negated is only representable as a BigInt.
                    None => value::bigint_new(-BigInt::from(n)),
                };
            }
            if let Some(f) = value::as_float(a) {
                return value::float_new(-f);
            }
            unary_type_error("-", a)
        }
        OP_POS => {
            if builtins::is_complex_instance(a) {
                return a;
            }
            if let Some(n) = value::as_bigint(a) {
                return value::bigint_new(n.clone());
            }
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
        OP_INVERT => {
            if let Some(n) = value::as_bigint(a) {
                return value::bigint_new(-n - BigInt::from(1));
            }
            match value::as_integer(a) {
                Some(n) => int_result(!n),
                None => unary_type_error("~", a),
            }
        }
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
    fn tagged_overflow_promotes_to_bigint() {
        exc::init();
        let sum = binop(OP_ADD, int(value::MAX_INT), int(1));
        assert!(!exc::pending());
        assert!(value::is_bigint(sum));
        assert_eq!(value::as_integer(sum), Some(value::MAX_INT + 1));
        assert_eq!(value::type_name(sum), "int");
        assert_eq!(value::to_string(sum), "1152921504606846976");

        // Small results keep the tagged fast path.
        let small = binop(OP_ADD, int(2), int(3));
        assert!(value::is_int(small));
        assert_eq!(value::as_integer(small), Some(5));

        // Products that leave the tag range promote as well.
        let product = binop(OP_MUL, int(value::MAX_INT), int(value::MAX_INT));
        assert!(value::is_bigint(product));
        assert_eq!(
            value::to_string(product),
            "1329227995784915870597964051066650625"
        );
        assert!(!exc::pending());
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

        // Ranges index by position like CPython: `r[i] == r.start + i*r.step`.
        assert_eq!(crate::container::subscript(ascending, int(0)), int(0));
        assert_eq!(crate::container::subscript(ascending, int(-1)), int(9));
        assert_eq!(crate::container::subscript(descending, int(-1)), int(1));
        assert!(!exc::pending());
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

        // Leaving the tagged range promotes instead of raising.
        exc::init();
        let shifted = binop(OP_SHL, int(value::MAX_INT), int(1));
        assert!(!exc::pending());
        assert!(value::is_bigint(shifted));
        assert_eq!(value::to_string(shifted), "2305843009213693950");

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

    /// Arbitrary-precision literal through the normalizing constructor.
    fn big(text: &str) -> i64 {
        value::bigint_new(text.parse::<BigInt>().expect("big literal"))
    }

    #[test]
    fn bigint_arithmetic_is_exact() {
        exc::init();
        let two_100 = binop(OP_POW, int(2), int(100));
        assert!(value::is_bigint(two_100));
        assert_eq!(value::to_string(two_100), "1267650600228229401496703205376");

        let two_200 = binop(OP_MUL, two_100, two_100);
        assert_eq!(
            value::to_string(two_200),
            "1606938044258990275541962092341162602522202993782792835301376"
        );

        let sum = binop(OP_ADD, two_200, two_100);
        assert_eq!(
            value::to_string(binop(OP_SUB, sum, two_100)),
            value::to_string(two_200)
        );
        assert!(!exc::pending());

        // Negative operands and subtraction across zero stay exact.
        let negative = binop(OP_SUB, int(0), two_100);
        assert_eq!(
            value::as_bigint(negative),
            Some(
                &"-1267650600228229401496703205376"
                    .parse::<BigInt>()
                    .expect("literal")
            )
        );
        assert_eq!(
            value::to_string(unop(OP_NEG, negative)),
            value::to_string(two_100)
        );
        assert_eq!(
            value::to_string(unop(OP_POS, negative)),
            value::to_string(negative)
        );
        assert!(!exc::pending());

        // BigInt mixed with a tagged int.
        let mixed = binop(OP_ADD, big("100000000000000000000"), int(1));
        assert_eq!(value::to_string(mixed), "100000000000000000001");
        assert!(!exc::pending());
    }

    #[test]
    fn bigint_floor_division_and_modulo() {
        exc::init();
        let negative = big("-1000000000000000000000000000004");
        assert_eq!(
            value::to_string(binop(OP_FLOOR_DIV, negative, int(3))),
            "-333333333333333333333333333335"
        );
        assert_eq!(value::to_string(binop(OP_MOD, negative, int(3))), "1");
        assert_eq!(
            value::to_string(binop(OP_FLOOR_DIV, negative, int(-3))),
            "333333333333333333333333333334"
        );
        assert_eq!(value::to_string(binop(OP_MOD, negative, int(-3))), "-2");

        let positive = big("1000000000000000000000000000000");
        assert_eq!(
            value::to_string(binop(OP_FLOOR_DIV, positive, int(3))),
            "333333333333333333333333333333"
        );
        assert_eq!(value::to_string(binop(OP_MOD, positive, int(3))), "1");
        assert!(!exc::pending());

        // `i64::MIN`-style tagged edge promotes: MIN_INT // -1 == 2^60.
        let promoted = binop(OP_FLOOR_DIV, int(value::MIN_INT), int(-1));
        assert!(value::is_bigint(promoted));
        assert_eq!(value::to_string(promoted), "1152921504606846976");
        assert_eq!(
            value::as_integer(binop(OP_MOD, int(value::MIN_INT), int(-1))),
            Some(0)
        );
        assert!(!exc::pending());

        exc::init();
        assert_eq!(binop(OP_FLOOR_DIV, positive, int(0)), NONE);
        assert_eq!(pending_message(), "integer division or modulo by zero");
        exc::init();
        assert_eq!(binop(OP_MOD, positive, int(0)), NONE);
        assert_eq!(pending_message(), "integer modulo by zero");
    }

    #[test]
    fn bigint_bitwise_shifts_and_pow_limits() {
        exc::init();
        let two_100 = binop(OP_SHL, int(1), int(100));
        assert_eq!(value::to_string(two_100), "1267650600228229401496703205376");
        assert_eq!(value::to_string(binop(OP_SHR, two_100, int(99))), "2");
        assert_eq!(value::to_string(binop(OP_BIT_AND, two_100, int(0xff))), "0");
        assert_eq!(
            value::to_string(binop(OP_BIT_OR, two_100, int(1))),
            "1267650600228229401496703205377"
        );
        assert_eq!(
            value::to_string(binop(OP_BIT_XOR, two_100, int(1))),
            "1267650600228229401496703205377"
        );
        assert_eq!(
            value::to_string(unop(OP_INVERT, two_100)),
            "-1267650600228229401496703205377"
        );

        // Negative operands use two's-complement bit patterns.
        let negative = big("-1267650600228229401496703205376");
        assert_eq!(
            value::to_string(binop(OP_BIT_AND, negative, int(0xff))),
            "0"
        );
        assert_eq!(value::to_string(binop(OP_SHR, negative, int(100))), "-1");
        assert_eq!(
            value::to_string(binop(OP_BIT_AND, negative, int(-1))),
            "-1267650600228229401496703205376"
        );
        assert!(!exc::pending());

        // Negative shift counts raise even with a BigInt base.
        exc::init();
        assert_eq!(binop(OP_SHR, negative, int(-1)), NONE);
        assert_eq!(exc::pending_tag(), exc::VALUE_ERROR as i64);
        assert_eq!(pending_message(), "negative shift count");

        // Impractical exponents and shifts raise instead of exhausting memory.
        exc::init();
        assert_eq!(binop(OP_POW, int(2), big("100000000000000000000")), NONE);
        assert_eq!(exc::pending_tag(), OVERFLOW_ERROR as i64);
        assert_eq!(pending_message(), "integer overflow");
        exc::init();
        assert_eq!(binop(OP_SHL, int(1), big("100000000000000000000")), NONE);
        assert_eq!(exc::pending_tag(), OVERFLOW_ERROR as i64);
        exc::clear();

        // Trivial bases stay cheap for any non-negative exponent.
        assert_eq!(
            as_int(binop(OP_POW, int(1), big("100000000000000000000"))),
            1
        );
        assert_eq!(
            as_int(binop(OP_POW, int(0), big("100000000000000000000"))),
            0
        );
        assert_eq!(
            as_int(binop(OP_POW, int(-1), big("100000000000000000001"))),
            -1
        );
        assert_eq!(
            as_int(binop(OP_POW, int(-1), big("100000000000000000000"))),
            1
        );
        assert!(!exc::pending());

        // Negative exponents keep the float result; an exponent too large for
        // `f64` underflows to zero exactly like CPython.
        assert_eq!(as_float(binop(OP_POW, int(2), int(-1))), 0.5);
        assert_eq!(
            as_float(binop(OP_POW, int(2), big("-100000000000000000000"))),
            0.0
        );
        assert!(!exc::pending());
    }

    #[test]
    fn bigint_comparisons_are_exact() {
        exc::init();
        let two_100 = binop(OP_SHL, int(1), int(100));
        let same = binop(OP_SHL, int(1), int(100));
        assert_eq!(binop(OP_EQ, two_100, same), TRUE);
        assert_eq!(binop(OP_NE, two_100, same), FALSE);
        assert_eq!(binop(OP_LT, two_100, same), FALSE);
        assert_eq!(binop(OP_LE, two_100, same), TRUE);

        assert_eq!(binop(OP_GT, two_100, int(value::MAX_INT)), TRUE);
        assert_eq!(binop(OP_LT, int(value::MAX_INT), two_100), TRUE);
        assert_eq!(
            binop(OP_GT, two_100, big("1267650600228229401496703205375")),
            TRUE
        );

        // Comparing against a float is exact, not lossy.
        assert_eq!(
            binop(OP_EQ, two_100, value::float_new(2f64.powi(100))),
            TRUE
        );
        assert_eq!(binop(OP_GT, two_100, value::float_new(2f64.powi(99))), TRUE);
        assert_eq!(binop(OP_LT, two_100, value::float_new(f64::INFINITY)), TRUE);
        assert_eq!(binop(OP_EQ, two_100, value::float_new(f64::NAN)), FALSE);
        assert!(!exc::pending());

        // Mismatched types keep Python's equality/ordering split.
        let text = value::str_from_bytes(b"x");
        assert_eq!(binop(OP_EQ, two_100, text), FALSE);
        exc::init();
        assert_eq!(binop(OP_LT, two_100, text), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
    }

    #[test]
    fn bigint_float_mixing_and_overflow() {
        exc::init();
        let two_100 = binop(OP_SHL, int(1), int(100));
        assert_eq!(
            as_float(binop(OP_MUL, two_100, value::float_new(0.5))),
            2f64.powi(99)
        );
        assert_eq!(as_float(binop(OP_DIV, two_100, int(2))), 2f64.powi(99));
        assert_eq!(
            as_float(binop(OP_ADD, value::float_new(0.5), two_100)),
            2f64.powi(100) + 0.5
        );
        assert!(!exc::pending());

        // An integer beyond the f64 range cannot mix with floats.
        exc::init();
        let enormous = binop(OP_SHL, int(1), int(2000));
        assert_eq!(binop(OP_ADD, enormous, value::float_new(1.0)), NONE);
        assert_eq!(exc::pending_tag(), OVERFLOW_ERROR as i64);
        assert_eq!(pending_message(), "int too large to convert to float");
    }
}
