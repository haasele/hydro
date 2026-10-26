//! `decimal` / `_decimal`: an exact decimal type for object-dtype workloads.
//!
//! Values are stored exactly as a signed integer coefficient and a base-10
//! exponent (standard `decimal.Decimal` representation), using arbitrary
//! precision integers. Construction from `str`, `int`, `bool`, `float` (exact
//! binary expansion) and `(sign, digits, exponent)` tuples, comparisons,
//! `+`/`-`/`*`/`/` through the method forms (`add`, `subtract`, `multiply`,
//! `divide` and the dunder entry points), `quantize`, and the predicates
//! `is_nan`/`is_finite`/`is_infinite`/`is_zero`/`is_signed` are implemented.
//!
//! # Scope
//!
//! This is deliberately a subset, not the full `decimal` specification:
//!
//! - `Context` holds `prec`/`rounding`/`Emin`/`Emax` and is shared through
//!   `getcontext()`/`setcontext()`/`localcontext()`; it governs division and
//!   `quantize` rounding. Addition and multiplication return exact results
//!   without context rounding.
//! - Signals other than the raised `InvalidOperation`/`DivisionByZero`/
//!   `Overflow` conditions are not trapped, and `flags` is not updated.
//! - The binary arithmetic operators `-`, `*`, `/` are not dispatched for
//!   user classes by the current runtime (only `+` is); use the method forms.
//!   `Decimal` still defines `__sub__`, `__mul__` and `__truediv__` so direct
//!   calls and future dispatcher support work.
//! - `sqrt`, transcendental methods, `from_float` and pickling are absent;
//!   unknown attributes raise `AttributeError`.

use std::cmp::Ordering;

use num_bigint::{BigInt, Sign};

use crate::exc::{
    self, ARITHMETIC_ERROR, ATTRIBUTE_ERROR, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR,
    ZERO_DIVISION_ERROR,
};
use crate::value::{self, NONE};

const MODULE: &str = "decimal";

// Native entry points.
const INIT: &str = "_decimal_init";
const STR: &str = "_decimal_str";
const REPR: &str = "_decimal_repr";
const AS_TUPLE: &str = "_decimal_as_tuple";
const EQ: &str = "_decimal_eq";
const NE: &str = "_decimal_ne";
const LT: &str = "_decimal_lt";
const LE: &str = "_decimal_le";
const GT: &str = "_decimal_gt";
const GE: &str = "_decimal_ge";
const ADD: &str = "_decimal_add";
const RADD: &str = "_decimal_radd";
const SUB: &str = "_decimal_subtract";
const MUL: &str = "_decimal_multiply";
const DIV: &str = "_decimal_divide";
const NEG: &str = "_decimal_neg";
const POS: &str = "_decimal_pos";
const ABS: &str = "_decimal_abs";
const QUANTIZE: &str = "_decimal_quantize";
const COMPARE: &str = "_decimal_compare";
const COPY_ABS: &str = "_decimal_copy_abs";
const COPY_NEGATE: &str = "_decimal_copy_negate";
const COPY_SIGN: &str = "_decimal_copy_sign";
const ADJUSTED: &str = "_decimal_adjusted";
const IS_NAN: &str = "_decimal_is_nan";
const IS_QNAN: &str = "_decimal_is_qnan";
const IS_SNAN: &str = "_decimal_is_snan";
const IS_FINITE: &str = "_decimal_is_finite";
const IS_INFINITE: &str = "_decimal_is_infinite";
const IS_ZERO: &str = "_decimal_is_zero";
const IS_SIGNED: &str = "_decimal_is_signed";
const TO_INTEGRAL: &str = "_decimal_to_integral_value";

// Context entry points.
const CONTEXT_INIT: &str = "_context_init";
const CONTEXT_ENTER: &str = "_context_enter";
const CONTEXT_EXIT: &str = "_context_exit";
const CONTEXT_COPY: &str = "_context_copy";
const LOCALCONTEXT_INIT: &str = "_localcontext_init";

/// Largest exponent difference an exact operation will materialise.
const MAX_SCALE: i64 = 100_000;

/// Default context precision (CPython's 28).
const DEFAULT_PREC: i64 = 28;

/// Special values.
const FINITE: i64 = 0;
const NAN: i64 = 1;
const SNAN: i64 = 2;
const INF: i64 = 3;

fn encode(n: i64) -> i64 {
    value::encode_int(n).unwrap_or_else(|| exc::raise_with_message(OVERFLOW_ERROR, "overflow"))
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

/// Exact decimal value.
#[derive(Clone, Debug)]
struct Dec {
    sign: bool,
    /// Decimal digits without sign or leading zeros (`"0"` for zero).
    digits: String,
    exp: i32,
    special: i64,
}

impl Dec {
    fn zero(sign: bool, exp: i32) -> Self {
        Dec {
            sign,
            digits: "0".to_string(),
            exp,
            special: FINITE,
        }
    }

    fn nan(sign: bool) -> Self {
        Dec {
            sign,
            digits: String::new(),
            exp: 0,
            special: NAN,
        }
    }

    fn inf(sign: bool) -> Self {
        Dec {
            sign,
            digits: String::new(),
            exp: 0,
            special: INF,
        }
    }

    fn from_int(n: i64) -> Self {
        Self::from_bigint(BigInt::from(n), 0)
    }

    fn from_bigint(mut coeff: BigInt, exp: i32) -> Self {
        let sign = coeff.sign() == Sign::Minus;
        if sign {
            coeff = -coeff;
        }
        let mut digits = coeff.to_str_radix(10);
        let trimmed = digits.trim_start_matches('0');
        if trimmed.is_empty() {
            digits = "0".to_string();
        } else {
            digits = trimmed.to_string();
        }
        Dec {
            sign,
            digits,
            exp,
            special: FINITE,
        }
    }

    fn is_zero(&self) -> bool {
        self.special == FINITE && self.digits == "0"
    }

    fn is_nan(&self) -> bool {
        self.special == NAN || self.special == SNAN
    }

    fn is_infinite(&self) -> bool {
        self.special == INF
    }

    fn is_finite(&self) -> bool {
        self.special == FINITE
    }

    /// Magnitude as a `BigInt` (zero for non-finite values).
    fn magnitude(&self) -> BigInt {
        if self.special != FINITE {
            return BigInt::from(0);
        }
        BigInt::parse_bytes(self.digits.as_bytes(), 10).unwrap_or_else(|| BigInt::from(0))
    }

    fn coefficient_signed(&self) -> BigInt {
        let magnitude = self.magnitude();
        if self.sign {
            -magnitude
        } else {
            magnitude
        }
    }

    fn digit_count(&self) -> usize {
        self.digits.chars().count()
    }

    fn adjusted(&self) -> i64 {
        i64::from(self.exp) + self.digit_count() as i64 - 1
    }
}

fn pow10(exp: u32) -> BigInt {
    let mut result = BigInt::from(1);
    let mut base = BigInt::from(10);
    let mut remaining = exp;
    while remaining > 0 {
        if remaining & 1 == 1 {
            result *= &base;
        }
        remaining >>= 1;
        if remaining > 0 {
            base = &base * &base;
        }
    }
    result
}

fn digit_count(value: &BigInt) -> usize {
    value.to_str_radix(10).trim_start_matches('-').len()
}

/// Rounds `coeff * 10^exp` to `prec` significant digits.
///
/// `sticky` marks discarded non-zero digits beyond the integer remainder.
fn round_parts(
    coeff: BigInt,
    exp: i32,
    prec: usize,
    rounding: Rounding,
    sticky: bool,
) -> (BigInt, i32) {
    let prec = prec.max(1);
    if coeff.sign() == Sign::NoSign {
        return (coeff, exp);
    }
    let negative = coeff.sign() == Sign::Minus;
    let magnitude = if negative { -coeff } else { coeff };
    let digits = digits_of(&magnitude);
    if digits <= prec {
        return (if negative { -magnitude } else { magnitude }, exp);
    }
    let drop = digits - prec;
    let divisor = pow10(drop as u32);
    let quotient = &magnitude / &divisor;
    let remainder = &magnitude % &divisor;
    let increment = should_increment(&quotient, &remainder, &divisor, negative, rounding, sticky);
    let mut result = if increment { quotient + 1 } else { quotient };
    let mut exponent = exp.saturating_add(drop as i32);
    if digit_count(&result) > prec {
        result /= 10;
        exponent = exponent.saturating_add(1);
    }
    (if negative { -result } else { result }, exponent)
}

fn digits_of(value: &BigInt) -> usize {
    value.to_str_radix(10).trim_start_matches('-').len()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Rounding {
    Ceiling,
    Down,
    Floor,
    HalfDown,
    HalfEven,
    HalfUp,
    Up,
    ZeroFiveUp,
}

fn parse_rounding(name: &str) -> Rounding {
    match name {
        "ROUND_CEILING" => Rounding::Ceiling,
        "ROUND_DOWN" => Rounding::Down,
        "ROUND_FLOOR" => Rounding::Floor,
        "ROUND_HALF_DOWN" => Rounding::HalfDown,
        "ROUND_HALF_UP" => Rounding::HalfUp,
        "ROUND_UP" => Rounding::Up,
        "ROUND_05UP" => Rounding::ZeroFiveUp,
        _ => Rounding::HalfEven,
    }
}

fn should_increment(
    quotient: &BigInt,
    remainder: &BigInt,
    divisor: &BigInt,
    negative: bool,
    rounding: Rounding,
    sticky: bool,
) -> bool {
    let inexact = remainder.sign() != Sign::NoSign || sticky;
    if !inexact {
        return false;
    }
    match rounding {
        Rounding::Down => false,
        Rounding::Up => true,
        Rounding::Ceiling => !negative,
        Rounding::Floor => negative,
        Rounding::ZeroFiveUp => {
            let last = quotient
                .to_str_radix(10)
                .chars()
                .last()
                .and_then(|ch| ch.to_digit(10))
                .unwrap_or(0);
            last == 0 || last == 5
        }
        Rounding::HalfUp | Rounding::HalfDown | Rounding::HalfEven => {
            let twice = remainder * 2i32;
            match twice.cmp(divisor) {
                Ordering::Greater => true,
                Ordering::Less => false,
                // Exactly half: the sticky bit means "more than half".
                Ordering::Equal => {
                    if sticky {
                        true
                    } else {
                        match rounding {
                            Rounding::HalfUp => true,
                            Rounding::HalfDown => false,
                            _ => {
                                let last = quotient
                                    .to_str_radix(10)
                                    .chars()
                                    .last()
                                    .and_then(|ch| ch.to_digit(10))
                                    .unwrap_or(0);
                                last % 2 == 1
                            }
                        }
                    }
                }
            }
        }
    }
}

// -------------------------------------------------------------------------
// Parsing and formatting
// -------------------------------------------------------------------------

fn parse_decimal(text: &str) -> Result<Dec, i64> {
    let cleaned: String = text.chars().filter(|ch| *ch != '_').collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return Err(exc::raise_with_message(
            VALUE_ERROR,
            "ConversionSyntax: invalid decimal literal",
        ));
    }
    let (sign, rest) = match trimmed.as_bytes()[0] {
        b'-' => (true, &trimmed[1..]),
        b'+' => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let lowered = rest.to_ascii_lowercase();
    if lowered == "nan" {
        return Ok(Dec::nan(sign));
    }
    if lowered == "snan" {
        return Ok(Dec {
            sign,
            digits: String::new(),
            exp: 0,
            special: SNAN,
        });
    }
    if lowered == "inf" || lowered == "infinity" {
        return Ok(Dec::inf(sign));
    }
    // `[digits][.digits][(e|E)[sign]digits]`
    let (mantissa, exponent_text) = match rest.find(['e', 'E']) {
        Some(position) => (&rest[..position], Some(&rest[position + 1..])),
        None => (rest, None),
    };
    let mut explicit_exp: i64 = 0;
    if let Some(exponent_text) = exponent_text {
        if exponent_text.is_empty() {
            return Err(exc::raise_with_message(
                VALUE_ERROR,
                "ConversionSyntax: invalid decimal literal",
            ));
        }
        let parsed: i64 = exponent_text.parse().map_err(|_| {
            exc::raise_with_message(VALUE_ERROR, "ConversionSyntax: invalid decimal literal")
        })?;
        explicit_exp = parsed;
    }
    let (integer_part, fraction_part) = match mantissa.split_once('.') {
        Some((integer, fraction)) => (integer, fraction),
        None => (mantissa, ""),
    };
    if !integer_part.chars().all(|ch| ch.is_ascii_digit())
        || !fraction_part.chars().all(|ch| ch.is_ascii_digit())
        || (integer_part.is_empty() && fraction_part.is_empty())
    {
        return Err(exc::raise_with_message(
            VALUE_ERROR,
            "ConversionSyntax: invalid decimal literal",
        ));
    }
    let mut digits = String::with_capacity(integer_part.len() + fraction_part.len());
    digits.push_str(integer_part);
    digits.push_str(fraction_part);
    let exponent = explicit_exp
        .checked_sub(fraction_part.len() as i64)
        .ok_or_else(|| {
            exc::raise_with_message(OVERFLOW_ERROR, "decimal exponent is out of range")
        })?;
    let exponent = i32::try_from(exponent)
        .map_err(|_| exc::raise_with_message(OVERFLOW_ERROR, "decimal exponent is out of range"))?;
    let value = BigInt::parse_bytes(digits.as_bytes(), 10)
        .ok_or_else(|| exc::raise_with_message(VALUE_ERROR, "ConversionSyntax"))?;
    Ok(Dec::from_bigint(
        if sign { -value } else { value },
        exponent,
    ))
}

fn dec_from_float(f: f64) -> Dec {
    if f.is_nan() {
        return Dec::nan(f.is_sign_negative());
    }
    if f.is_infinite() {
        return Dec::inf(f.is_sign_negative());
    }
    if f == 0.0 {
        return Dec::zero(f.is_sign_negative(), 0);
    }
    let bits = f.to_bits();
    let sign = (bits >> 63) != 0;
    let exponent_bits = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1u64 << 52) - 1);
    let (mantissa, exponent) = if exponent_bits == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1u64 << 52), exponent_bits - 1023 - 52)
    };
    let mantissa = BigInt::from(mantissa);
    let (coeff, exp) = if exponent >= 0 {
        (mantissa << (exponent as usize), 0)
    } else {
        // m * 2^-k == m * 5^k / 10^k, so the coefficient is exact.
        let scale = (-exponent) as u32;
        (mantissa * BigInt::from(5).pow(scale), exponent)
    };
    let mut value = Dec::from_bigint(coeff, exp);
    value.sign = sign;
    value
}

fn render(dec: &Dec) -> String {
    let sign = if dec.sign { "-" } else { "" };
    match dec.special {
        NAN => return format!("{sign}NaN"),
        SNAN => return format!("{sign}sNaN"),
        INF => return format!("{sign}Infinity"),
        _ => {}
    }
    let digits = if dec.digits.is_empty() {
        "0"
    } else {
        dec.digits.as_str()
    };
    let adjusted = dec.adjusted();
    if dec.exp > 0 || adjusted < -6 {
        let mut out = String::with_capacity(digits.len() + 8);
        out.push_str(sign);
        let mut chars = digits.chars();
        if let Some(first) = chars.next() {
            out.push(first);
        }
        let rest: String = chars.collect();
        if !rest.is_empty() {
            out.push('.');
            out.push_str(&rest);
        }
        out.push('E');
        if adjusted >= 0 {
            out.push('+');
        }
        out.push_str(&adjusted.to_string());
        return out;
    }
    if dec.exp == 0 {
        return format!("{sign}{digits}");
    }
    let fraction = (-dec.exp) as usize;
    if fraction < digits.len() {
        let split = digits.len() - fraction;
        format!("{sign}{}.{}", &digits[..split], &digits[split..])
    } else {
        format!("{sign}0.{}{}", "0".repeat(fraction - digits.len()), digits)
    }
}

// -------------------------------------------------------------------------
// Instances
// -------------------------------------------------------------------------

fn decimal_class() -> i64 {
    if let Some(class) = crate::core::globals::registry_get("decimal.Decimal") {
        return class;
    }
    let methods = vec![
        (
            "__init__".to_string(),
            native(INIT, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__str__".to_string(),
            native(STR, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__repr__".to_string(),
            native(REPR, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__eq__".to_string(),
            native(EQ, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__ne__".to_string(),
            native(NE, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__lt__".to_string(),
            native(LT, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__le__".to_string(),
            native(LE, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__gt__".to_string(),
            native(GT, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__ge__".to_string(),
            native(GE, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__add__".to_string(),
            native(ADD, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__radd__".to_string(),
            native(RADD, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__sub__".to_string(),
            native(SUB, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__mul__".to_string(),
            native(MUL, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__truediv__".to_string(),
            native(DIV, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__neg__".to_string(),
            native(NEG, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__pos__".to_string(),
            native(POS, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__abs__".to_string(),
            native(ABS, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "add".to_string(),
            native(ADD, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "subtract".to_string(),
            native(SUB, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "multiply".to_string(),
            native(MUL, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "divide".to_string(),
            native(DIV, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "quantize".to_string(),
            native(QUANTIZE, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "compare".to_string(),
            native(COMPARE, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "copy_abs".to_string(),
            native(COPY_ABS, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "copy_negate".to_string(),
            native(COPY_NEGATE, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "copy_sign".to_string(),
            native(COPY_SIGN, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "adjusted".to_string(),
            native(ADJUSTED, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "is_nan".to_string(),
            native(IS_NAN, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "is_qnan".to_string(),
            native(IS_QNAN, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "is_snan".to_string(),
            native(IS_SNAN, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "is_finite".to_string(),
            native(IS_FINITE, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "is_infinite".to_string(),
            native(IS_INFINITE, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "is_zero".to_string(),
            native(IS_ZERO, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "is_signed".to_string(),
            native(IS_SIGNED, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "to_integral_value".to_string(),
            native(TO_INTEGRAL, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "as_tuple".to_string(),
            native(AS_TUPLE, 1),
            hydro_ir::MethodKind::Normal,
        ),
    ];
    let class = value::class_new_full("Decimal".to_string(), NONE, methods, Vec::new());
    crate::core::globals::registry_set("decimal.Decimal", class);
    class
}

fn native(name: &'static str, arity: usize) -> i64 {
    value::native_function_new(MODULE, name, arity)
}

fn set_attr(instance: i64, name: &str, v: i64) {
    value::instance_setattr(instance, name.to_string(), v);
}

fn write_dec(instance: i64, dec: &Dec) {
    set_attr(instance, "_dec_sign", value::bool_value(dec.sign));
    set_attr(
        instance,
        "_dec_digits",
        value::str_alloc(dec.digits.clone()),
    );
    set_attr(instance, "_dec_exp", encode(i64::from(dec.exp)));
    set_attr(instance, "_dec_special", encode(dec.special));
}

/// Decodes a Decimal instance. A non-Decimal value returns the silent
/// `Err(NONE)` sentinel (no pending exception), so callers can probe operands
/// without leaking errors; `parse_operand` reports the TypeError.
fn read_dec(instance: i64) -> Result<Dec, i64> {
    if value::as_instance(instance).is_none() {
        return Err(NONE);
    }
    let Some(sign) = value::instance_attr(instance, "_dec_sign") else {
        return Err(NONE);
    };
    let sign = value::truthy(sign);
    let Some(digits) = value::instance_attr(instance, "_dec_digits").and_then(value::as_str) else {
        return Err(NONE);
    };
    let digits = digits.to_string();
    let Some(exp) = value::instance_attr(instance, "_dec_exp").and_then(value::as_integer) else {
        return Err(NONE);
    };
    let exp = i32::try_from(exp)
        .map_err(|_| exc::raise_with_message(OVERFLOW_ERROR, "decimal exponent is out of range"))?;
    let special = value::instance_attr(instance, "_dec_special")
        .and_then(value::as_integer)
        .unwrap_or(FINITE);
    Ok(Dec {
        sign,
        digits,
        exp,
        special,
    })
}

fn dec_instance(dec: &Dec) -> i64 {
    let class = decimal_class();
    let instance = value::instance_new(class);
    write_dec(instance, dec);
    instance
}

/// Converts an arithmetic operand to `Dec`; `None` for unsupported types.
fn operand_dec(v: i64) -> Option<Dec> {
    if let Ok(dec) = read_dec(v) {
        return Some(dec);
    }
    if let Some(integer) = value::as_integer(v) {
        return Some(Dec::from_int(integer));
    }
    if let Some(float) = value::as_float(v) {
        return Some(dec_from_float(float));
    }
    None
}

fn operand_or_error(v: i64) -> Result<Dec, i64> {
    operand_dec(v).ok_or_else(|| {
        type_error(&format!(
            "unsupported operand type: {}",
            value::type_name(v)
        ))
    })
}

// -------------------------------------------------------------------------
// Context
// -------------------------------------------------------------------------

fn context_class() -> i64 {
    if let Some(class) = crate::core::globals::registry_get("decimal.Context") {
        return class;
    }
    let methods = vec![
        (
            "__init__".to_string(),
            native(CONTEXT_INIT, 9),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__enter__".to_string(),
            native(CONTEXT_ENTER, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__exit__".to_string(),
            native(CONTEXT_EXIT, 4),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "copy".to_string(),
            native(CONTEXT_COPY, 1),
            hydro_ir::MethodKind::Normal,
        ),
    ];
    let class = value::class_new_full("Context".to_string(), NONE, methods, Vec::new());
    crate::core::globals::registry_set("decimal.Context", class);
    class
}

fn localcontext_class() -> i64 {
    if let Some(class) = crate::core::globals::registry_get("decimal.LocalContext") {
        return class;
    }
    let methods = vec![
        (
            "__init__".to_string(),
            native(LOCALCONTEXT_INIT, 2),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__enter__".to_string(),
            native(CONTEXT_ENTER, 1),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "__exit__".to_string(),
            native(CONTEXT_EXIT, 4),
            hydro_ir::MethodKind::Normal,
        ),
    ];
    let class = value::class_new_full("_ContextManager".to_string(), NONE, methods, Vec::new());
    crate::core::globals::registry_set("decimal.LocalContext", class);
    class
}

/// Installs the default attributes every context carries.
fn write_default_context(instance: i64) {
    set_attr(instance, "prec", encode(DEFAULT_PREC));
    set_attr(
        instance,
        "rounding",
        value::str_alloc("ROUND_HALF_EVEN".to_string()),
    );
    set_attr(instance, "Emin", encode(-999_999));
    set_attr(instance, "Emax", encode(999_999));
    set_attr(instance, "capitals", encode(1));
    set_attr(instance, "clamp", encode(0));
    set_attr(instance, "flags", value::dict_new(Vec::new()));
    set_attr(instance, "traps", value::dict_new(Vec::new()));
}

fn active_context() -> i64 {
    if let Some(context) = crate::core::globals::registry_get("decimal.active_context") {
        return context;
    }
    let class = context_class();
    let context = value::instance_new(class);
    write_default_context(context);
    crate::core::globals::registry_set("decimal.active_context", context);
    context
}

fn context_prec() -> usize {
    let context = active_context();
    value::instance_attr(context, "prec")
        .and_then(value::as_integer)
        .filter(|prec| *prec >= 1)
        .map(|prec| prec as usize)
        .unwrap_or(DEFAULT_PREC as usize)
}

fn context_rounding() -> Rounding {
    let context = active_context();
    let name = value::instance_attr(context, "rounding")
        .and_then(value::as_str)
        .unwrap_or("ROUND_HALF_EVEN");
    parse_rounding(name)
}

fn context_init(args: &[i64]) -> i64 {
    let instance = args[0];
    write_default_context(instance);
    let names = [
        "prec", "rounding", "Emin", "Emax", "capitals", "clamp", "flags", "traps",
    ];
    for (index, &raw) in args[1..].iter().enumerate() {
        if raw == NONE {
            continue;
        }
        let Some(name) = names.get(index) else {
            return type_error("too many Context() arguments");
        };
        set_attr(instance, name, raw);
    }
    NONE
}

fn localcontext_init(args: &[i64]) -> i64 {
    set_attr(
        args[0],
        "_local_context",
        args.get(1).copied().unwrap_or(NONE),
    );
    NONE
}

fn context_snapshot(context: i64) -> Vec<(&'static str, i64)> {
    const NAMES: [&str; 8] = [
        "prec", "rounding", "Emin", "Emax", "capitals", "clamp", "flags", "traps",
    ];
    NAMES
        .iter()
        .filter_map(|&name| value::instance_attr(context, name).map(|item| (name, item)))
        .collect()
}

fn copy_context_from(source: i64, target: i64) {
    if value::as_instance(source).is_none() {
        return;
    }
    for (name, item) in context_snapshot(source) {
        set_attr(target, name, item);
    }
}

fn context_enter(args: &[i64]) -> i64 {
    let manager = args[0];
    let active = active_context();
    let saved = context_snapshot(active);
    for (index, (name, item)) in saved.iter().enumerate() {
        set_attr(manager, &format!("_saved_{index}"), *item);
        set_attr(
            manager,
            &format!("_saved_name_{index}"),
            value::str_alloc(name.to_string()),
        );
    }
    set_attr(manager, "_saved_count", encode(saved.len() as i64));
    set_attr(manager, "_saved_active", value::TRUE);
    if let Some(context) = value::instance_attr(manager, "_local_context") {
        if context != NONE {
            copy_context_from(context, active);
        }
    }
    active
}

fn context_exit(args: &[i64]) -> i64 {
    let manager = args[0];
    let Some(count) = value::instance_attr(manager, "_saved_count").and_then(value::as_integer)
    else {
        return NONE;
    };
    let active = active_context();
    for index in 0..count.max(0) as usize {
        let name = value::instance_attr(manager, &format!("_saved_name_{index}"))
            .and_then(value::as_str)
            .map(str::to_string);
        let item = value::instance_attr(manager, &format!("_saved_{index}"));
        if let (Some(name), Some(item)) = (name, item) {
            set_attr(active, &name, item);
        }
    }
    NONE
}

fn context_copy(args: &[i64]) -> i64 {
    let class = context_class();
    let copy = value::instance_new(class);
    write_default_context(copy);
    copy_context_from(args[0], copy);
    copy
}

fn getcontext(_args: &[i64]) -> i64 {
    active_context()
}

fn setcontext(args: &[i64]) -> i64 {
    if value::as_instance(args[0]).is_none() {
        return type_error("setcontext() requires a Context instance");
    }
    copy_context_from(args[0], active_context());
    NONE
}

fn localcontext(args: &[i64]) -> i64 {
    let class = localcontext_class();
    let manager = value::instance_new(class);
    let context = args.first().copied().unwrap_or(NONE);
    set_attr(manager, "_local_context", context);
    manager
}

// -------------------------------------------------------------------------
// Construction
// -------------------------------------------------------------------------

fn decimal_init(args: &[i64]) -> i64 {
    let instance = args[0];
    let value_arg = args.get(1).copied().unwrap_or(NONE);
    let dec = match value_arg {
        NONE => Dec::from_int(0),
        raw => match parse_operand(raw) {
            Ok(dec) => dec,
            Err(error) => return error,
        },
    };
    write_dec(instance, &dec);
    NONE
}

fn parse_operand(raw: i64) -> Result<Dec, i64> {
    if let Ok(dec) = read_dec(raw) {
        return Ok(dec);
    }
    if let Some(text) = value::as_str(raw) {
        return parse_decimal(text);
    }
    if let Some(integer) = value::as_integer(raw) {
        return Ok(Dec::from_int(integer));
    }
    if let Some(float) = value::as_float(raw) {
        return Ok(dec_from_float(float));
    }
    if let Some(parts) = value::as_tuple(raw) {
        return parse_tuple(parts);
    }
    if let Some(parts) = value::as_list(raw) {
        return parse_tuple(parts);
    }
    Err(type_error(&format!(
        "conversion from {} to Decimal is not supported",
        value::type_name(raw)
    )))
}

fn parse_tuple(parts: &[i64]) -> Result<Dec, i64> {
    if parts.len() != 3 {
        return Err(type_error("Decimal tuple must have 3 elements"));
    }
    let sign = match value::as_integer(parts[0]) {
        Some(0) => false,
        Some(1) => true,
        _ => return Err(exc::raise_with_message(VALUE_ERROR, "Invalid tuple sign")),
    };
    let items = as_items(parts[1])
        .ok_or_else(|| exc::raise_with_message(VALUE_ERROR, "Invalid tuple digits"))?;
    let mut digits = String::with_capacity(items.len());
    for item in items {
        let Some(digit) = value::as_integer(item) else {
            return Err(exc::raise_with_message(VALUE_ERROR, "Invalid tuple digits"));
        };
        if !(0..=9).contains(&digit) {
            return Err(exc::raise_with_message(VALUE_ERROR, "Invalid tuple digits"));
        }
        digits.push(char::from_digit(digit as u32, 10).unwrap_or('0'));
    }
    let exponent = value::as_integer(parts[2])
        .ok_or_else(|| exc::raise_with_message(VALUE_ERROR, "Invalid tuple exponent"))?;
    let exponent = i32::try_from(exponent)
        .map_err(|_| exc::raise_with_message(OVERFLOW_ERROR, "decimal exponent is out of range"))?;
    let coefficient = BigInt::parse_bytes(digits.as_bytes(), 10).unwrap_or_else(|| BigInt::from(0));
    Ok(Dec::from_bigint(
        if sign { -coefficient } else { coefficient },
        exponent,
    ))
}

fn as_items(v: i64) -> Option<Vec<i64>> {
    if let Some(items) = value::as_tuple(v) {
        return Some(items.to_vec());
    }
    value::as_list(v).map(<[i64]>::to_vec)
}

// -------------------------------------------------------------------------
// Comparison
// -------------------------------------------------------------------------

fn cmp_abs(a: &Dec, b: &Dec) -> Ordering {
    if a.is_zero() && b.is_zero() {
        return Ordering::Equal;
    }
    let order_a = a.adjusted();
    let order_b = b.adjusted();
    if order_a != order_b {
        return order_a.cmp(&order_b);
    }
    let width = a.digits.len().max(b.digits.len());
    let mut left = a.digits.clone();
    let mut right = b.digits.clone();
    left.extend(std::iter::repeat_n('0', width - left.len()));
    right.extend(std::iter::repeat_n('0', width - right.len()));
    left.cmp(&right)
}

/// Numeric comparison; `None` when either operand is NaN or infinite-complex.
fn cmp_dec(a: &Dec, b: &Dec) -> Option<Ordering> {
    if a.is_nan() || b.is_nan() {
        return None;
    }
    match (a.is_infinite(), b.is_infinite()) {
        (true, true) => {
            return Some(if a.sign == b.sign {
                Ordering::Equal
            } else if a.sign {
                Ordering::Less
            } else {
                Ordering::Greater
            })
        }
        (true, false) => {
            return Some(if a.sign {
                Ordering::Less
            } else {
                Ordering::Greater
            })
        }
        (false, true) => {
            return Some(if b.sign {
                Ordering::Greater
            } else {
                Ordering::Less
            })
        }
        (false, false) => {}
    }
    if a.is_zero() && b.is_zero() {
        return Some(Ordering::Equal);
    }
    if a.sign != b.sign {
        return Some(if a.sign {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let magnitude = cmp_abs(a, b);
    Some(if a.sign {
        magnitude.reverse()
    } else {
        magnitude
    })
}

fn comparison(args: &[i64], allow_unconvertible: Option<bool>) -> i64 {
    let left = match read_dec(args[0]) {
        Ok(dec) => dec,
        Err(error) => return error,
    };
    let right = match operand_dec(args[1]) {
        Some(dec) => dec,
        None => {
            return match allow_unconvertible {
                Some(equal_if_unconvertible) => value::bool_value(equal_if_unconvertible),
                None => type_error(&format!(
                    "unsupported comparison operand: {}",
                    value::type_name(args[1])
                )),
            }
        }
    };
    match cmp_dec(&left, &right) {
        None => value::FALSE,
        Some(order) => value::bool_value(order == Ordering::Equal),
    }
}

fn eq_args(args: &[i64]) -> i64 {
    comparison(args, Some(false))
}

fn ne_args(args: &[i64]) -> i64 {
    let result = comparison(args, Some(false));
    if exc::pending() {
        return result;
    }
    value::bool_value(!value::truthy(result))
}

fn ordered(args: &[i64], expected: fn(Ordering) -> bool) -> i64 {
    let left = match read_dec(args[0]) {
        Ok(dec) => dec,
        Err(error) => return error,
    };
    let right = match operand_dec(args[1]) {
        Some(dec) => dec,
        None => {
            return type_error(&format!(
                "unsupported comparison operand: {}",
                value::type_name(args[1])
            ))
        }
    };
    match cmp_dec(&left, &right) {
        None => exc::raise_with_message(ARITHMETIC_ERROR, "comparison involving NaN is undefined"),
        Some(order) => value::bool_value(expected(order)),
    }
}

// -------------------------------------------------------------------------
// Arithmetic
// -------------------------------------------------------------------------

fn add_dec(a: &Dec, b: &Dec) -> Result<Dec, i64> {
    if a.is_nan() || b.is_nan() {
        return Ok(Dec::nan(a.sign || b.sign));
    }
    if a.is_infinite() || b.is_infinite() {
        if a.is_infinite() && b.is_infinite() && a.sign != b.sign {
            return Err(exc::raise_with_message(
                ARITHMETIC_ERROR,
                "InvalidOperation: Infinity - Infinity",
            ));
        }
        return Ok(Dec::inf(if a.is_infinite() { a.sign } else { b.sign }));
    }
    let common = a.exp.min(b.exp);
    let left = scale_to(&a.coefficient_signed(), a.exp, common)?;
    let right = scale_to(&b.coefficient_signed(), b.exp, common)?;
    let sum = left + right;
    let sign = if sum.sign() == Sign::NoSign {
        a.sign && b.sign
    } else {
        sum.sign() == Sign::Minus
    };
    let (coeff, exp) = round_parts(sum, common, context_prec(), context_rounding(), false);
    let mut result = Dec::from_bigint(coeff, exp);
    if result.is_zero() {
        result.sign = sign;
    }
    Ok(result)
}

fn scale_to(coeff: &BigInt, exp: i32, target: i32) -> Result<BigInt, i64> {
    if exp == target {
        return Ok(coeff.clone());
    }
    let difference = i64::from(exp) - i64::from(target);
    if difference > MAX_SCALE {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            "decimal exponent difference is too large",
        ));
    }
    let scale = pow10(difference.max(0) as u32);
    Ok(coeff * scale)
}

fn mul_dec(a: &Dec, b: &Dec) -> Result<Dec, i64> {
    if a.is_nan() || b.is_nan() {
        return Ok(Dec::nan(a.sign || b.sign));
    }
    let sign = a.sign != b.sign;
    if a.is_infinite() || b.is_infinite() {
        if a.is_zero() || b.is_zero() {
            return Err(exc::raise_with_message(
                ARITHMETIC_ERROR,
                "InvalidOperation: 0 * Infinity",
            ));
        }
        return Ok(Dec::inf(sign));
    }
    let exponent = i64::from(a.exp) + i64::from(b.exp);
    let exponent = i32::try_from(exponent)
        .map_err(|_| exc::raise_with_message(OVERFLOW_ERROR, "decimal exponent is out of range"))?;
    let product = a.coefficient_signed() * b.coefficient_signed();
    let (coeff, exp) = round_parts(product, exponent, context_prec(), context_rounding(), false);
    let mut result = Dec::from_bigint(coeff, exp);
    if result.is_zero() {
        result.sign = sign;
    }
    Ok(result)
}

fn div_dec(a: &Dec, b: &Dec) -> Result<Dec, i64> {
    if a.is_nan() || b.is_nan() {
        return Ok(Dec::nan(a.sign || b.sign));
    }
    let sign = a.sign != b.sign;
    if a.is_infinite() {
        if b.is_infinite() {
            return Err(exc::raise_with_message(
                ARITHMETIC_ERROR,
                "InvalidOperation: Infinity / Infinity",
            ));
        }
        return Ok(Dec::inf(sign));
    }
    if b.is_infinite() {
        return Ok(Dec::zero(sign, 0));
    }
    if b.is_zero() {
        if a.is_zero() {
            return Err(exc::raise_with_message(
                ARITHMETIC_ERROR,
                "InvalidOperation: 0 / 0",
            ));
        }
        return Err(exc::raise_with_message(
            ZERO_DIVISION_ERROR,
            "DivisionByZero: division by zero",
        ));
    }
    if a.is_zero() {
        let exponent = i64::from(a.exp) - i64::from(b.exp);
        let exponent = i32::try_from(exponent).unwrap_or(0);
        return Ok(Dec::zero(sign, exponent));
    }
    let prec = context_prec();
    let rounding = context_rounding();
    let numerator = a.magnitude();
    let denominator = b.magnitude();
    let digits_n = digit_count(&numerator) as i64;
    let digits_d = digit_count(&denominator) as i64;
    let shift = prec as i64 + 2 + digits_d - digits_n;
    let base_exp = i64::from(a.exp) - i64::from(b.exp);
    if shift > MAX_SCALE || -shift > MAX_SCALE {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            "decimal exponent difference is too large",
        ));
    }
    let (quotient, remainder, exponent) = if shift >= 0 {
        let scaled = &numerator * pow10(shift as u32);
        let quotient = &scaled / &denominator;
        let remainder = &scaled % &denominator;
        (quotient, remainder, base_exp - shift)
    } else {
        let scaled = &denominator * pow10((-shift) as u32);
        let quotient = &numerator / &scaled;
        let remainder = &numerator % &scaled;
        (quotient, remainder, base_exp - shift)
    };
    let exponent = i32::try_from(exponent)
        .map_err(|_| exc::raise_with_message(OVERFLOW_ERROR, "decimal exponent is out of range"))?;
    let inexact = remainder.sign() != Sign::NoSign;
    let (coeff, exp) = round_parts(quotient, exponent, prec, rounding, inexact);
    let mut magnitude = if coeff.sign() == Sign::Minus {
        -coeff
    } else {
        coeff
    };
    let mut exp = exp;
    // CPython's division reduces exact results to the smallest exponent that
    // still represents the value, then pads back to the ideal exponent
    // (`e1 - e2`) when that is smaller.
    let ten = BigInt::from(10);
    let mut guard = 0usize;
    while (&magnitude % &ten).sign() == Sign::NoSign && guard < MAX_SCALE as usize {
        magnitude /= &ten;
        exp = exp.saturating_add(1);
        guard += 1;
    }
    if exp as i64 > base_exp {
        let exact = {
            let delta = i64::from(exp) - base_exp;
            if delta > MAX_SCALE {
                false
            } else {
                let left = &magnitude * &denominator * pow10(delta as u32);
                left == numerator
            }
        };
        if exact {
            if let Ok(ideal) = i32::try_from(base_exp) {
                magnitude *= pow10((exp - ideal) as u32);
                exp = ideal;
            }
        }
    }
    let mut result = Dec::from_bigint(if sign { -magnitude } else { magnitude }, exp);
    result.sign = sign;
    Ok(result)
}

fn add_args(args: &[i64]) -> i64 {
    arithmetic(args, add_dec)
}

fn radd_args(args: &[i64]) -> i64 {
    // `__radd__(self, other)` has the receiver on the right.
    let left = match operand_or_error(args[1]) {
        Ok(dec) => dec,
        Err(error) => return error,
    };
    let right = match read_dec(args[0]) {
        Ok(dec) => dec,
        Err(error) => return error,
    };
    match add_dec(&left, &right) {
        Ok(dec) => dec_instance(&dec),
        Err(error) => error,
    }
}

fn arithmetic(args: &[i64], operation: fn(&Dec, &Dec) -> Result<Dec, i64>) -> i64 {
    let left = match read_dec(args[0]) {
        Ok(dec) => dec,
        Err(error) => return error,
    };
    let right = match operand_or_error(args[1]) {
        Ok(dec) => dec,
        Err(error) => return error,
    };
    match operation(&left, &right) {
        Ok(dec) => dec_instance(&dec),
        Err(error) => error,
    }
}

fn unary(args: &[i64], operation: fn(&Dec) -> Dec) -> i64 {
    let dec = match read_dec(args[0]) {
        Ok(dec) => dec,
        Err(error) => return error,
    };
    dec_instance(&operation(&dec))
}

fn negate(dec: &Dec) -> Dec {
    let mut result = dec.clone();
    result.sign = !result.sign;
    result
}

fn absolute(dec: &Dec) -> Dec {
    let mut result = dec.clone();
    result.sign = false;
    result
}

fn quantize(args: &[i64]) -> i64 {
    let value = match read_dec(args[0]) {
        Ok(dec) => dec,
        Err(error) => return error,
    };
    let target = match operand_or_error(args[1]) {
        Ok(dec) => dec,
        Err(error) => return error,
    };
    if !value.is_finite() || !target.is_finite() {
        return exc::raise_with_message(
            ARITHMETIC_ERROR,
            "InvalidOperation: quantize with a non-finite operand",
        );
    }
    let target_exp = target.exp;
    // `shift` is how many decimal places the result gains: a positive shift
    // pads with zeros, a negative shift rounds the dropped digits away.
    let shift = i64::from(value.exp) - i64::from(target_exp);
    let mut coefficient = if shift >= 0 {
        if shift > MAX_SCALE {
            return exc::raise_with_message(OVERFLOW_ERROR, "quantize exponent is too large");
        }
        value.magnitude() * pow10(shift as u32)
    } else {
        let scale = -shift;
        if scale > MAX_SCALE {
            return exc::raise_with_message(OVERFLOW_ERROR, "quantize exponent is too large");
        }
        let divisor = pow10(scale as u32);
        let magnitude = value.magnitude();
        let quotient = &magnitude / &divisor;
        let remainder = &magnitude % &divisor;
        let increment = should_increment(
            &quotient,
            &remainder,
            &divisor,
            value.sign,
            context_rounding(),
            false,
        );
        if increment {
            quotient + 1
        } else {
            quotient
        }
    };
    let exp = target_exp;
    if value.sign {
        coefficient = -coefficient;
    }
    let dec = Dec::from_bigint(coefficient, exp);
    if dec.digit_count() > context_prec() {
        return exc::raise_with_message(
            ARITHMETIC_ERROR,
            "InvalidOperation: quantize result has too many digits",
        );
    }
    dec_instance(&dec)
}

fn compare_method(args: &[i64]) -> i64 {
    let left = match read_dec(args[0]) {
        Ok(dec) => dec,
        Err(error) => return error,
    };
    let right = match operand_or_error(args[1]) {
        Ok(dec) => dec,
        Err(error) => return error,
    };
    match cmp_dec(&left, &right) {
        None => dec_instance(&Dec::nan(false)),
        Some(order) => {
            let result = match order {
                Ordering::Less => Dec::from_int(-1),
                Ordering::Equal => Dec::from_int(0),
                Ordering::Greater => Dec::from_int(1),
            };
            dec_instance(&result)
        }
    }
}

fn copy_sign(args: &[i64]) -> i64 {
    let mut value = match read_dec(args[0]) {
        Ok(dec) => dec,
        Err(error) => return error,
    };
    let other = match operand_or_error(args[1]) {
        Ok(dec) => dec,
        Err(error) => return error,
    };
    value.sign = other.sign;
    dec_instance(&value)
}

fn to_integral(args: &[i64]) -> i64 {
    let value = match read_dec(args[0]) {
        Ok(dec) => dec,
        Err(error) => return error,
    };
    if !value.is_finite() {
        return dec_instance(&value);
    }
    // Round the coefficient to exponent 0 by removing the decimal exponent.
    let shift = i64::from(value.exp);
    if shift >= 0 {
        return dec_instance(&value);
    }
    if -shift > MAX_SCALE {
        return exc::raise_with_message(OVERFLOW_ERROR, "exponent is too large");
    }
    let divisor = pow10((-shift) as u32);
    let magnitude = value.magnitude();
    let quotient = &magnitude / &divisor;
    let remainder = &magnitude % &divisor;
    let increment = should_increment(
        &quotient,
        &remainder,
        &divisor,
        value.sign,
        context_rounding(),
        false,
    );
    let mut result = if increment { quotient + 1 } else { quotient };
    if value.sign {
        result = -result;
    }
    dec_instance(&Dec::from_bigint(result, 0))
}

// -------------------------------------------------------------------------
// Formatting entry points
// -------------------------------------------------------------------------

fn decimal_str(args: &[i64]) -> i64 {
    match read_dec(args[0]) {
        Ok(dec) => value::str_alloc(render(&dec)),
        Err(error) => error,
    }
}

fn decimal_repr(args: &[i64]) -> i64 {
    match read_dec(args[0]) {
        Ok(dec) => value::str_alloc(format!("Decimal('{}')", render(&dec))),
        Err(error) => error,
    }
}

fn as_tuple(args: &[i64]) -> i64 {
    let dec = match read_dec(args[0]) {
        Ok(dec) => dec,
        Err(error) => return error,
    };
    let sign = encode(i64::from(dec.sign));
    let digits: Vec<i64> = if dec.special == FINITE {
        dec.digits
            .chars()
            .map(|ch| encode(i64::from(ch.to_digit(10).unwrap_or(0))))
            .collect()
    } else {
        Vec::new()
    };
    let exponent = match dec.special {
        NAN => value::str_alloc("n".to_string()),
        SNAN => value::str_alloc("N".to_string()),
        INF => value::str_alloc("F".to_string()),
        _ => encode(i64::from(dec.exp)),
    };
    value::tuple_new(vec![sign, value::tuple_new(digits), exponent])
}

fn predicate(args: &[i64], check: fn(&Dec) -> bool) -> i64 {
    match read_dec(args[0]) {
        Ok(dec) => value::bool_value(check(&dec)),
        Err(error) => error,
    }
}

fn adjusted(args: &[i64]) -> i64 {
    match read_dec(args[0]) {
        Ok(dec) if dec.is_finite() => encode(dec.adjusted()),
        Ok(_) => exc::raise_with_message(
            ARITHMETIC_ERROR,
            "adjusted() is undefined for non-finite values",
        ),
        Err(error) => error,
    }
}

// -------------------------------------------------------------------------
// Dispatch
// -------------------------------------------------------------------------

/// Dispatches a native `decimal`/`_decimal` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        INIT => decimal_init(args),
        STR => decimal_str(args),
        REPR => decimal_repr(args),
        AS_TUPLE => as_tuple(args),
        EQ => eq_args(args),
        NE => ne_args(args),
        LT => ordered(args, |order| order == Ordering::Less),
        LE => ordered(args, |order| order != Ordering::Greater),
        GT => ordered(args, |order| order == Ordering::Greater),
        GE => ordered(args, |order| order != Ordering::Less),
        ADD => add_args(args),
        RADD => radd_args(args),
        SUB => arithmetic(args, |a, b| add_dec(a, &negate(b))),
        MUL => arithmetic(args, mul_dec),
        DIV => arithmetic(args, div_dec),
        NEG => unary(args, negate),
        POS => unary(args, |dec| dec.clone()),
        ABS => unary(args, absolute),
        QUANTIZE => quantize(args),
        COMPARE => compare_method(args),
        COPY_ABS => unary(args, absolute),
        COPY_NEGATE => unary(args, negate),
        COPY_SIGN => copy_sign(args),
        ADJUSTED => adjusted(args),
        IS_NAN => predicate(args, |dec| dec.is_nan()),
        IS_QNAN => predicate(args, |dec| dec.special == NAN),
        IS_SNAN => predicate(args, |dec| dec.special == SNAN),
        IS_FINITE => predicate(args, |dec| dec.is_finite()),
        IS_INFINITE => predicate(args, |dec| dec.is_infinite()),
        IS_ZERO => predicate(args, |dec| dec.is_zero()),
        IS_SIGNED => predicate(args, |dec| dec.sign),
        TO_INTEGRAL => to_integral(args),
        "getcontext" => getcontext(args),
        "setcontext" => setcontext(args),
        "localcontext" => localcontext(args),
        CONTEXT_INIT => context_init(args),
        CONTEXT_ENTER => context_enter(args),
        CONTEXT_EXIT => context_exit(args),
        CONTEXT_COPY => context_copy(args),
        LOCALCONTEXT_INIT => localcontext_init(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'decimal' has no attribute '{function}'"),
        ),
    }
}

fn exception_class(name: &str, base_tag: u32) -> i64 {
    let key = format!("decimal.exception.{name}");
    if let Some(class) = crate::core::globals::registry_get(&key) {
        return class;
    }
    let base = exc::exception_class(base_tag);
    let class = value::class_new(name.to_string(), base, Vec::new());
    crate::core::globals::registry_set(&key, class);
    class
}

/// Derives a `decimal` exception from another exposed `decimal` exception.
fn derived_exception(name: &str, base: i64) -> i64 {
    let key = format!("decimal.exception.{name}");
    if let Some(class) = crate::core::globals::registry_get(&key) {
        return class;
    }
    let class = value::class_new(name.to_string(), base, Vec::new());
    crate::core::globals::registry_set(&key, class);
    class
}

/// Materialises a `decimal` module constant.
pub(crate) fn constant(name: &str) -> i64 {
    let text = |s: &str| value::str_alloc(s.to_string());
    match name {
        "Decimal" => decimal_class(),
        "Context" => context_class(),
        "DefaultContext" => {
            if let Some(context) = crate::core::globals::registry_get("decimal.default_context") {
                return context;
            }
            let class = context_class();
            let context = value::instance_new(class);
            write_default_context(context);
            crate::core::globals::registry_set("decimal.default_context", context);
            context
        }
        "DecimalException" => exception_class("DecimalException", ARITHMETIC_ERROR),
        "InvalidOperation" => exception_class("InvalidOperation", ARITHMETIC_ERROR),
        "DivisionByZero" => exception_class("DivisionByZero", ZERO_DIVISION_ERROR),
        "Overflow" => exception_class("Overflow", OVERFLOW_ERROR),
        "Underflow" => exception_class("Underflow", ARITHMETIC_ERROR),
        "Inexact" => exception_class("Inexact", ARITHMETIC_ERROR),
        "Rounded" => exception_class("Rounded", ARITHMETIC_ERROR),
        "Subnormal" => exception_class("Subnormal", ARITHMETIC_ERROR),
        "Clamped" => exception_class("Clamped", ARITHMETIC_ERROR),
        "ConversionSyntax" => derived_exception(
            "ConversionSyntax",
            exception_class("InvalidOperation", ARITHMETIC_ERROR),
        ),
        "ROUND_CEILING" => text("ROUND_CEILING"),
        "ROUND_DOWN" => text("ROUND_DOWN"),
        "ROUND_FLOOR" => text("ROUND_FLOOR"),
        "ROUND_HALF_DOWN" => text("ROUND_HALF_DOWN"),
        "ROUND_HALF_EVEN" => text("ROUND_HALF_EVEN"),
        "ROUND_HALF_UP" => text("ROUND_HALF_UP"),
        "ROUND_UP" => text("ROUND_UP"),
        "ROUND_05UP" => text("ROUND_05UP"),
        "MAX_PREC" => encode(999_999_999_999_999_999),
        "MAX_EMAX" => encode(999_999_999_999_999_999),
        "MIN_EMIN" => encode(-999_999_999_999_999_999),
        "MIN_ETINY" => encode(-1_999_999_999_999_999_997),
        "__libmpdec_version__" => text("4.0.0"),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'decimal' has no attribute '{name}'"),
        ),
    }
}
