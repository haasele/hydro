//! PEP 750 template-string values (`Template` / `Interpolation`).
//!
//! A template is a first-class runtime value, never a `str`: it keeps the
//! literal parts and its interpolations separately, exactly like CPython's
//! `string.templatelib.Template`. Generated code builds one through
//! [`hydro_template_new`](crate::abi::hydro_template_new) after constructing
//! each interpolation through
//! [`hydro_interpolation_new`](crate::abi::hydro_interpolation_new).
//!
//! The value model exposes the same surface as CPython 3.14 without a fake
//! Python module:
//!
//! | value          | attributes                                                   |
//! |----------------|--------------------------------------------------------------|
//! | `Template`     | `strings`, `interpolations`, `values`                        |
//! | `Interpolation`| `value`, `expression`, `conversion`, `format_spec`, `self_documenting`, `convert()` |
//!
//! `str(template)` concatenates the literal parts with each interpolation's
//! formatted value; `repr()` matches CPython's constructor-style rendering.
//!
//! The ABI entry points never panic: malformed buffers and invariant
//! violations raise a Python exception and return `None`.

use std::os::raw::c_uchar;

use crate::value;
use crate::{exc, format};

/// Payload of a `Template` value ([`value::KIND_TEMPLATE`]).
pub(crate) struct TemplateState {
    /// Constant parts as tagged `str` values, always one more than
    /// `interpolations`.
    pub(crate) strings: Vec<i64>,
    /// Tagged `Interpolation` values in source order.
    pub(crate) interpolations: Vec<i64>,
}

/// Payload of an `Interpolation` value ([`value::KIND_INTERPOLATION`]).
pub(crate) struct InterpolationState {
    /// The evaluated expression value.
    pub(crate) value: i64,
    /// Source text of the expression exactly as written.
    pub(crate) expression: String,
    /// `0` none, `1` `!s`, `2` `!r`, `3` `!a`.
    pub(crate) conversion: u8,
    /// Tagged `str` with the evaluated format spec, or `None`.
    pub(crate) format_spec: i64,
    /// Whether the source used the `=` debug specifier.
    pub(crate) self_documenting: bool,
}

/// `str(interpolation)`: apply the conversion and format spec to the value.
pub(crate) fn interpolation_to_string(state: &InterpolationState) -> String {
    let spec = value::as_str(state.format_spec).unwrap_or("");
    match format::format_value(state.value, spec, state.conversion) {
        Ok(text) => text,
        // `str(interpolation)` applies the format spec, so an invalid spec
        // must raise instead of silently rendering the unformatted value.
        // The pending exception is observed by the caller's exception check.
        Err(error) => {
            exc::raise_with_message(error.tag, &error.message);
            String::new()
        }
    }
}

/// `str(template)`: concatenate literal parts and formatted interpolations.
pub(crate) fn template_to_string(state: &TemplateState) -> String {
    let mut out = String::new();
    for (index, interpolation) in state.interpolations.iter().enumerate() {
        if let Some(text) = state
            .strings
            .get(index)
            .and_then(|value| value::as_str(*value))
        {
            out.push_str(text);
        }
        match value::as_interpolation(*interpolation) {
            Some(interpolation) => out.push_str(&interpolation_to_string(interpolation)),
            // The constructor rejects non-interpolation entries; keep the
            // rendering total instead of panicking on a corrupt payload.
            None => out.push_str(&value::to_string(*interpolation)),
        }
    }
    if let Some(text) = state
        .strings
        .get(state.interpolations.len())
        .and_then(|value| value::as_str(*value))
    {
        out.push_str(text);
    }
    out
}

/// `repr(interpolation)`: `Interpolation('x', 1, None, None)`.
pub(crate) fn interpolation_repr(state: &InterpolationState) -> String {
    let conversion = match state.conversion {
        1 => "'s'".to_string(),
        2 => "'r'".to_string(),
        3 => "'a'".to_string(),
        _ => "None".to_string(),
    };
    let spec = if state.format_spec == value::NONE {
        "None".to_string()
    } else {
        value::quote_str(value::as_str(state.format_spec).unwrap_or(""))
    };
    format!(
        "Interpolation({}, {}, {}, {})",
        value::quote_str(&state.expression),
        value::repr(state.value),
        conversion,
        spec
    )
}

/// `repr(template)`: `Template(strings=(...), interpolations=(...))`.
pub(crate) fn template_repr(state: &TemplateState) -> String {
    let mut strings = String::new();
    for (index, text) in state.strings.iter().enumerate() {
        if index > 0 {
            strings.push_str(", ");
        }
        strings.push_str(&value::repr(*text));
    }
    if state.strings.len() == 1 {
        strings.push(',');
    }
    let mut interpolations = String::new();
    for (index, interpolation) in state.interpolations.iter().enumerate() {
        if index > 0 {
            interpolations.push_str(", ");
        }
        match value::as_interpolation(*interpolation) {
            Some(interpolation) => interpolations.push_str(&interpolation_repr(interpolation)),
            None => interpolations.push_str(&value::repr(*interpolation)),
        }
    }
    if state.interpolations.len() == 1 {
        interpolations.push(',');
    }
    format!("Template(strings=({strings}), interpolations=({interpolations}))")
}

/// Attribute lookup for `Template` and `Interpolation` values.
///
/// Returns `None` for any other value (or an unknown attribute) so the caller
/// can fall through to its normal `AttributeError` path.
pub(crate) fn getattr(obj: i64, attr: &str) -> Option<i64> {
    if let Some(state) = value::as_template(obj) {
        return match attr {
            "strings" => Some(value::tuple_new(state.strings.clone())),
            "interpolations" => Some(value::tuple_new(state.interpolations.clone())),
            "values" => Some(value::tuple_new(
                state
                    .interpolations
                    .iter()
                    .map(|interpolation| {
                        value::as_interpolation(*interpolation)
                            .map_or(value::NONE, |interpolation| interpolation.value)
                    })
                    .collect(),
            )),
            _ => None,
        };
    }
    let state = value::as_interpolation(obj)?;
    match attr {
        "value" => Some(state.value),
        "expression" => Some(value::str_alloc(state.expression.clone())),
        "conversion" => Some(match state.conversion {
            1 => value::str_alloc("s".to_string()),
            2 => value::str_alloc("r".to_string()),
            3 => value::str_alloc("a".to_string()),
            _ => value::NONE,
        }),
        "format_spec" => Some(state.format_spec),
        "self_documenting" => Some(value::bool_value(state.self_documenting)),
        "convert" => Some(value::bound_method_new(obj, attr.to_string())),
        _ => None,
    }
}

/// Method call on a template value. Only `Interpolation.convert()` exists.
///
/// Returns `None` when `obj` is not a template value so the caller can use
/// its normal method tables.
pub(crate) fn call_method(obj: i64, method: &str, argc: usize, _argv: *const i64) -> Option<i64> {
    if method != "convert" {
        return None;
    }
    let state = value::as_interpolation(obj)?;
    if argc != 0 {
        return Some(exc::raise_with_message(
            exc::TYPE_ERROR,
            "convert() takes no arguments",
        ));
    }
    Some(value::str_alloc(interpolation_to_string(state)))
}

/// Copies `len` bytes at `ptr` into a UTF-8 string.
///
/// Returns `None` for a null buffer with a nonzero length or an impossible
/// length; the ABI reports that as `TypeError`.
fn decode_text(ptr: *const c_uchar, len: usize) -> Option<String> {
    if len == 0 {
        return Some(String::new());
    }
    if ptr.is_null() || len > isize::MAX as usize {
        return None;
    }
    // SAFETY: the ABI contract requires `ptr..ptr+len` to be readable.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Copies `len` tagged values at `ptr`.
fn copy_values(ptr: *const i64, len: usize) -> Option<Vec<i64>> {
    if len == 0 {
        return Some(Vec::new());
    }
    if ptr.is_null() || len > isize::MAX as usize / std::mem::size_of::<i64>() {
        return None;
    }
    // SAFETY: the ABI contract requires `ptr..ptr+len` to be readable tagged
    // values; they are rooted by the caller while this helper runs.
    Some(unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec())
}

/// `hydro_interpolation_new`: builds one interpolation object.
pub(crate) fn interpolation_new(
    value: i64,
    expression: *const c_uchar,
    expression_len: usize,
    conversion: u8,
    format_spec: i64,
    self_documenting: u8,
) -> i64 {
    if conversion > 3 {
        return exc::raise_with_message(exc::VALUE_ERROR, "invalid conversion specifier");
    }
    if format_spec != value::NONE && value::as_str(format_spec).is_none() {
        return exc::raise_with_message(exc::TYPE_ERROR, "format_spec must be a str or None");
    }
    let Some(expression) = decode_text(expression, expression_len) else {
        return exc::raise_with_message(exc::TYPE_ERROR, "invalid interpolation expression buffer");
    };
    value::interpolation_alloc(
        value,
        expression,
        conversion,
        format_spec,
        self_documenting != 0,
    )
}

/// `hydro_template_new`: builds a template from its literal parts and
/// interpolations.
pub(crate) fn template_new(
    strings_len: usize,
    strings: *const i64,
    interpolations_len: usize,
    interpolations: *const i64,
) -> i64 {
    let Some(string_values) = copy_values(strings, strings_len) else {
        return exc::raise_with_message(exc::TYPE_ERROR, "invalid template strings buffer");
    };
    let Some(interpolation_values) = copy_values(interpolations, interpolations_len) else {
        return exc::raise_with_message(exc::TYPE_ERROR, "invalid template interpolations buffer");
    };
    if string_values.len() != interpolation_values.len().saturating_add(1) {
        return exc::raise_with_message(
            exc::VALUE_ERROR,
            "template strings must be one longer than interpolations",
        );
    }
    if string_values
        .iter()
        .any(|string| value::as_str(*string).is_none())
    {
        return exc::raise_with_message(exc::TYPE_ERROR, "template strings must be str values");
    }
    if interpolation_values
        .iter()
        .any(|interpolation| value::as_interpolation(*interpolation).is_none())
    {
        return exc::raise_with_message(
            exc::TYPE_ERROR,
            "template interpolations must be Interpolation values",
        );
    }
    value::template_alloc(string_values, interpolation_values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interpolation(value: i64, expression: &str, conversion: u8, spec: i64) -> i64 {
        interpolation_new(
            value,
            expression.as_ptr(),
            expression.len(),
            conversion,
            spec,
            0,
        )
    }

    fn template(parts: &[&str], interpolations: &[i64]) -> i64 {
        let strings: Vec<i64> = parts
            .iter()
            .map(|part| value::str_alloc((*part).to_string()))
            .collect();
        let result = template_new(
            strings.len(),
            strings.as_ptr(),
            interpolations.len(),
            interpolations.as_ptr(),
        );
        assert!(!exc::pending(), "template construction failed");
        result
    }

    #[test]
    fn str_and_repr_follow_cpython() {
        let spec = value::str_alloc("04d".to_string());
        let interp = interpolation(value::normalize_int(42), "x", 0, spec);
        let template = template(&["value: ", ""], &[interp]);
        assert_eq!(value::to_string(template), "value: 0042");
        assert_eq!(
            value::repr(template),
            "Template(strings=('value: ', ''), interpolations=(Interpolation('x', 42, None, '04d'),))"
        );
        assert_eq!(value::repr(interp), "Interpolation('x', 42, None, '04d')");
    }

    #[test]
    fn attributes_expose_strings_and_values() {
        let interp = interpolation(value::normalize_int(7), "n", 2, value::NONE);
        let template = template(&["a", "b"], &[interp]);
        let strings = getattr(template, "strings").expect("strings");
        let strings = value::as_tuple(strings).expect("tuple");
        assert_eq!(strings.len(), 2);
        assert_eq!(value::as_str(strings[0]), Some("a"));
        assert_eq!(value::as_str(strings[1]), Some("b"));
        let values = getattr(template, "values").expect("values");
        assert_eq!(
            value::as_tuple(values).expect("tuple").to_vec(),
            vec![value::normalize_int(7)]
        );
        let conversion = getattr(interp, "conversion").expect("conversion");
        assert_eq!(value::as_str(conversion), Some("r"));
        let expression = getattr(interp, "expression").expect("expression");
        assert_eq!(value::as_str(expression), Some("n"));
    }

    #[test]
    fn convert_applies_conversion() {
        let interp = interpolation(value::str_alloc("hi".to_string()), "x", 2, value::NONE);
        let converted = call_method(interp, "convert", 0, std::ptr::null()).expect("method");
        assert_eq!(value::as_str(converted), Some("'hi'"));
    }

    #[test]
    fn invalid_buffers_raise_without_panicking() {
        let result = template_new(0, std::ptr::null(), 0, std::ptr::null());
        assert_eq!(result, value::NONE);
        assert!(exc::pending());
        exc::clear();
        let result = interpolation_new(value::NONE, std::ptr::null(), 4, 0, value::NONE, 0);
        assert_eq!(result, value::NONE);
        assert!(exc::pending());
        exc::clear();
    }
}
