//! `_string`: the parser behind `string.Formatter`.
//!
//! `formatter_parser` and `formatter_field_name_split` follow CPython 3.14's
//! `formatter_unicode.c` state machine exactly:
//!
//! - `{{`/`}}` are literal braces, a lone brace is a `ValueError`; each
//!   escaped brace terminates the current literal chunk, matching CPython's
//!   iterator (so `a{{b` yields `a{` and then `b`);
//! - field names may contain `[...]` sections (where `:`/`!`/`}` are data,
//!   but `{` is still rejected outside brackets);
//! - a conversion is one code point after `!` and must be followed by `:`, `}`
//!   or the end of the field;
//! - format specs are scanned with brace nesting only (no recursive
//!   expansion), so `{a:{b:{c}}}` reports the spec literally.
//!
//! Return values use the runtime's ordinary objects: `formatter_parser`
//! returns a list iterator over 4-tuples and `formatter_field_name_split`
//! returns `(first, rest)` where `rest` is a list iterator over
//! `(is_attribute, value)` tuples.
//!
//! Integral indices in field names are recognized with ASCII digits. CPython
//! also accepts other Unicode decimal digits; this runtime has no Unicode
//! decimal-value table, so such digits stay strings (the field still resolves
//! through the attribute/key path).

use crate::exc::{self, TYPE_ERROR, VALUE_ERROR};
use crate::ops;
use crate::value::{self, NONE};

pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "formatter_parser" => formatter_parser(args),
        "formatter_field_name_split" => formatter_field_name_split(args),
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module '_string' has no attribute '{function}'"),
        ),
    }
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

/// `(literal, field_name, format_spec, conversion)`.
fn field_tuple(
    literal: String,
    field: Option<&str>,
    spec: Option<&str>,
    conversion: Option<char>,
) -> i64 {
    let literal = value::str_alloc(literal);
    let field = match field {
        Some(field) => value::str_alloc(field.to_string()),
        None => NONE,
    };
    let spec = match spec {
        Some(spec) => value::str_alloc(spec.to_string()),
        None => NONE,
    };
    let conversion = match conversion {
        Some(conversion) => value::str_alloc(conversion.to_string()),
        None => NONE,
    };
    value::tuple_new(vec![literal, field, spec, conversion])
}

fn formatter_parser(args: &[i64]) -> i64 {
    let Some(source) = value::as_str(args[0]) else {
        return type_error(&format!(
            "formatter_parser() argument must be str, not {}",
            value::type_name(args[0])
        ));
    };
    let bytes = source.as_bytes();
    let length = bytes.len();
    let mut position = 0usize;
    let mut literal = String::new();
    // The list owns every yielded tuple; tuples are appended as they are
    // created so the collector can always trace them (the local Vec buffer is
    // invisible to the conservative stack scan).
    let fields = value::list_new(Vec::new());
    loop {
        // Scan literal text up to the next markup field. `{{` and `}}` are
        // copied as single braces; a lone `}` is an error here. CPython ends
        // the literal chunk right after each escaped brace, so `a{{b` yields
        // `a{` and `b` separately.
        let mut markup = false;
        let mut escaped = false;
        while position < length {
            match bytes[position] {
                b'}' => {
                    if position + 1 < length && bytes[position + 1] == b'}' {
                        literal.push('}');
                        position += 2;
                        escaped = true;
                        break;
                    } else {
                        return value_error("Single '}' encountered in format string");
                    }
                }
                b'{' => {
                    if position + 1 < length && bytes[position + 1] == b'{' {
                        literal.push('{');
                        position += 2;
                        escaped = true;
                        break;
                    } else {
                        markup = true;
                        break;
                    }
                }
                _ => {
                    let character = source[position..].chars().next().unwrap_or('?');
                    literal.push(character);
                    position += character.len_utf8();
                }
            }
        }
        if escaped {
            push_field(
                fields,
                field_tuple(std::mem::take(&mut literal), None, None, None),
            );
            continue;
        }
        if !markup {
            if !literal.is_empty() {
                push_field(
                    fields,
                    field_tuple(std::mem::take(&mut literal), None, None, None),
                );
            }
            break;
        }

        // Consume the opening brace.
        position += 1;
        if position >= length {
            return value_error("Single '{' encountered in format string");
        }
        let field_start = position;
        let mut bracket_depth = 0usize;
        loop {
            if position >= length {
                return value_error("expected '}' before end of string");
            }
            let byte = bytes[position];
            if bracket_depth == 0 {
                if byte == b'}' || byte == b':' || byte == b'!' {
                    break;
                }
                if byte == b'{' {
                    return value_error("unexpected '{' in field name");
                }
            }
            if byte == b'[' {
                bracket_depth += 1;
            } else if byte == b']' && bracket_depth > 0 {
                bracket_depth -= 1;
            }
            position += 1;
        }
        let field_name = &source[field_start..position];
        let mut conversion = None;
        let mut format_spec = "";
        // `scan_format_spec` consumes the closing brace, while a plain field
        // end or `!conv}` leaves it for the check after this block.
        let mut closed = false;
        if bytes[position] == b'!' {
            position += 1;
            if position >= length {
                return value_error("end of string while looking for conversion specifier");
            }
            let character = source[position..].chars().next().unwrap_or('!');
            conversion = Some(character);
            position += character.len_utf8();
            if position >= length {
                return value_error("unmatched '{' in format spec");
            }
            if bytes[position] == b'}' {
                // Empty format spec, closing brace handled below.
            } else if bytes[position] == b':' {
                position += 1;
                format_spec = match scan_format_spec(source, &mut position) {
                    Ok(spec) => spec,
                    Err(error) => return error,
                };
                closed = true;
            } else {
                return value_error("expected ':' after conversion specifier");
            }
        } else if bytes[position] == b':' {
            position += 1;
            format_spec = match scan_format_spec(source, &mut position) {
                Ok(spec) => spec,
                Err(error) => return error,
            };
            closed = true;
        }
        if !closed {
            if position >= length || bytes[position] != b'}' {
                return value_error("unmatched '{' in format spec");
            }
            position += 1;
        }
        push_field(
            fields,
            field_tuple(
                std::mem::take(&mut literal),
                Some(field_name),
                Some(format_spec),
                conversion,
            ),
        );
    }
    value::iter_new(fields)
}

/// Appends a tuple to the parser's result list with a collector edge.
fn push_field(fields: i64, item: i64) {
    if let Some(mut items) = value::list_items_mut(fields) {
        items.push(item);
        crate::heap::write_barrier_edge(fields, item);
    }
}

/// Scans a format spec up to the matching `}`, returning the literal spec and
/// advancing `position` past the closing brace.
///
/// Every `{` increases the nesting depth (there is no `{{` escape handling in
/// specs; CPython keeps the doubled braces as literal spec text).
fn scan_format_spec<'a>(source: &'a str, position: &mut usize) -> Result<&'a str, i64> {
    let bytes = source.as_bytes();
    let start = *position;
    let mut depth = 1usize;
    loop {
        if *position >= bytes.len() {
            return Err(value_error("unmatched '{' in format spec"));
        }
        match bytes[*position] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let spec = &source[start..*position];
                    *position += 1;
                    return Ok(spec);
                }
            }
            _ => {}
        }
        *position += 1;
    }
}

/// `formatter_field_name_split(field_name)` returns
/// `(first, rest_iterator)`.
fn formatter_field_name_split(args: &[i64]) -> i64 {
    let Some(source) = value::as_str(args[0]) else {
        return type_error(&format!(
            "formatter_field_name_split() argument must be str, not {}",
            value::type_name(args[0])
        ));
    };
    let bytes = source.as_bytes();
    let length = bytes.len();
    let mut position = 0usize;
    while position < length && bytes[position] != b'.' && bytes[position] != b'[' {
        position += 1;
    }
    let first_text = &source[..position];
    let first = if first_text.is_empty() {
        value::str_alloc(String::new())
    } else {
        match decimal_value(first_text) {
            Some(number) => number,
            None => value::str_alloc(first_text.to_string()),
        }
    };
    let rest = value::list_new(Vec::new());
    while position < length {
        match bytes[position] {
            b'.' => {
                position += 1;
                let start = position;
                while position < length && bytes[position] != b'.' && bytes[position] != b'[' {
                    position += 1;
                }
                if position == start {
                    return value_error("Empty attribute in format string");
                }
                let name = value::str_alloc(source[start..position].to_string());
                push_field(rest, value::tuple_new(vec![value::TRUE, name]));
            }
            b'[' => {
                position += 1;
                let start = position;
                while position < length && bytes[position] != b']' {
                    position += 1;
                }
                if position >= length {
                    return value_error("Missing ']' in format string");
                }
                let content = &source[start..position];
                if content.is_empty() {
                    return value_error("Empty attribute in format string");
                }
                let element = match decimal_value(content) {
                    Some(number) => number,
                    None => value::str_alloc(content.to_string()),
                };
                position += 1;
                push_field(rest, value::tuple_new(vec![value::FALSE, element]));
                if position < length && bytes[position] != b'.' && bytes[position] != b'[' {
                    return value_error("Only '.' or '[' may follow ']' in format field specifier");
                }
            }
            _ => {
                // The first-part scan stops only at `.`/`[`, so this is
                // unreachable; report it as malformed input rather than
                // panicking.
                return value_error("malformed format field name");
            }
        }
    }
    let rest = value::iter_new(rest);
    value::tuple_new(vec![first, rest])
}

/// Converts an all-ASCII-digit field-name component to an integer value.
fn decimal_value(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if text.len() <= 18 {
        match text.parse::<i64>() {
            Ok(number) => Some(ops::int_result(number)),
            Err(_) => None,
        }
    } else {
        let number = value::parse_decimal_int(text)?;
        Some(value::bigint_new(number))
    }
}
