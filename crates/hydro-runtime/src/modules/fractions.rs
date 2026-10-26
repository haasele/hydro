//! `fractions.Fraction`: numeric ratios for `rich._ratio`.
//!
//! The runtime has no heap rational type, and the only consumer performs
//! ordinary arithmetic and comparisons, so `Fraction` evaluates to the exact
//! IEEE-754 quotient (`Fraction(1, 2)` is `0.5`). A zero denominator raises
//! `ZeroDivisionError` like CPython.
//!
//! Inputs accepted: integers, booleans, floats and the string spellings
//! `"n/d"`, `"n"` and decimal floats.

use crate::exc::{self, TYPE_ERROR, VALUE_ERROR, ZERO_DIVISION_ERROR};
use crate::value::{self, NONE};

/// Dispatches a native `fractions` function. Arity is enforced by the central
/// spec before this point.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "Fraction" => fraction(args),
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module 'fractions' has no attribute '{function}'"),
        ),
    }
}

/// `Fraction(numerator, denominator=1)`.
fn fraction(args: &[i64]) -> i64 {
    let numerator = match ratio_value("numerator", args[0]) {
        Ok(numerator) => numerator,
        Err(error) => return error,
    };
    let denominator = match args.get(1).copied().unwrap_or(NONE) {
        NONE => 1.0,
        value_arg => match ratio_value("denominator", value_arg) {
            Ok(denominator) => denominator,
            Err(error) => return error,
        },
    };
    if denominator == 0.0 {
        return exc::raise_with_message(
            ZERO_DIVISION_ERROR,
            "Fraction() denominator must not be zero",
        );
    }
    value::float_new(numerator / denominator)
}

/// Numeric view of a `Fraction` argument.
fn ratio_value(position: &str, value_arg: i64) -> Result<f64, i64> {
    if let Some(integer) = value::as_integer(value_arg) {
        return Ok(integer as f64);
    }
    if let Some(float) = value::as_float(value_arg) {
        return Ok(float);
    }
    if let Some(text) = value::as_str(value_arg) {
        return parse_ratio(text);
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "Fraction() {position} must be a number or a string, not {}",
            value::type_name(value_arg)
        ),
    ))
}

/// Parses `"n/d"`, `"n"` or a decimal float.
fn parse_ratio(text: &str) -> Result<f64, i64> {
    let text = text.trim();
    if let Some((numerator, denominator)) = text.split_once('/') {
        let numerator: f64 = numerator
            .trim()
            .parse()
            .map_err(|_| exc::raise_with_message(VALUE_ERROR, "Invalid literal for Fraction"))?;
        let denominator: f64 = denominator
            .trim()
            .parse()
            .map_err(|_| exc::raise_with_message(VALUE_ERROR, "Invalid literal for Fraction"))?;
        if denominator == 0.0 {
            return Err(exc::raise_with_message(
                ZERO_DIVISION_ERROR,
                "Fraction() denominator must not be zero",
            ));
        }
        return Ok(numerator / denominator);
    }
    text.parse::<f64>()
        .map_err(|_| exc::raise_with_message(VALUE_ERROR, "Invalid literal for Fraction"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{encode_int, float_new, str_alloc};

    fn int(n: i64) -> i64 {
        encode_int(n).expect("small integer")
    }

    fn text(s: &str) -> i64 {
        str_alloc(s.to_string())
    }

    #[test]
    fn fractions_evaluate_to_quotients() {
        exc::init();
        assert_eq!(
            value::as_float(call("Fraction", &[int(1), int(2)])),
            Some(0.5)
        );
        assert_eq!(value::as_float(call("Fraction", &[int(3)])), Some(3.0));
        assert_eq!(
            value::as_float(call("Fraction", &[float_new(0.25)])),
            Some(0.25)
        );
        assert_eq!(
            value::as_float(call("Fraction", &[text("1/4")])),
            Some(0.25)
        );
        assert!(!exc::pending());
    }

    #[test]
    fn zero_denominator_is_a_zero_division_error() {
        exc::init();
        assert_eq!(call("Fraction", &[int(1), int(0)]), NONE);
        assert!(exc::pending());
        exc::clear();
    }

    #[test]
    fn invalid_values_are_rejected() {
        exc::init();
        call("Fraction", &[text("not a number")]);
        assert!(exc::pending());
        exc::clear();
        call("Fraction", &[value::list_new(Vec::new())]);
        assert!(exc::pending());
        exc::clear();
    }
}
