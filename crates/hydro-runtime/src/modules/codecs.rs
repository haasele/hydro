//! `_codecs`: the UTF-8 codec and the codec/error-handler registries.
//!
//! Hydro's strings are UTF-8, so UTF-8 is the only codec with a native
//! implementation. Non-UTF-8 encoding names fail with `LookupError` before any
//! conversion happens; they are never silently treated as UTF-8.
//!
//! The codec registry keeps callable search functions registered with
//! `register()` and consults them from `lookup()`; only UTF-8 is built in.
//! Registered search functions must return a 4-tuple (encode, decode,
//! incremental encoder, incremental decoder), like CPython's `CodecInfo` tuple.
//! `encode`/`decode` themselves implement UTF-8 only: a registered codec can
//! be looked up, but conversions through it are not routed here.
//!
//! The error-handler registry stores callables. `strict`, `ignore` and
//! `replace` are built in (and can be overridden by `register_error`, as in
//! CPython). During decoding, a registered handler is called with a
//! `UnicodeDecodeError` object and must return `(replacement_str, new_pos)`.
//! Hydro's exception objects expose `args` rather than `.object/.start/.end`,
//! so the error object carries `(encoding, object_repr, start, end, reason)`
//! as its args tuple.
//!
//! `escape_encode`/`escape_decode` implement CPython's `unicode_escape`
//! subset used by the `codecs` module.

use std::cell::RefCell;

use crate::builtins as core_builtins;
use crate::core::globals;
use crate::exc::{self, LOOKUP_ERROR, TYPE_ERROR, UNICODE_DECODE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::ops;
use crate::value::{self, NONE};

thread_local! {
    /// Codec search functions, in registration order.
    static SEARCH_FUNCTIONS: RefCell<Vec<i64>> = const { RefCell::new(Vec::new()) };
    /// Named error handlers; later registrations replace earlier ones.
    static ERROR_HANDLERS: RefCell<Vec<(String, i64)>> = const { RefCell::new(Vec::new()) };
}

/// How malformed UTF-8 input is treated.
enum ErrorMode {
    Strict,
    Ignore,
    Replace,
    Custom(i64),
}

pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "encode" => encode(args),
        "decode" => decode(args),
        "utf_8_encode" => utf_8_encode(args),
        "utf_8_decode" => utf_8_decode(args),
        "escape_encode" => escape_encode(args),
        "escape_decode" => escape_decode(args),
        "lookup" => lookup(args),
        "register" => register(args),
        "unregister" => unregister(args),
        "register_error" => register_error(args),
        "lookup_error" => lookup_error(args),
        "strict_errors" => strict_errors(args),
        "ignore_errors" => ignore_errors(args),
        "replace_errors" => replace_errors(args),
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module '_codecs' has no attribute '{function}'"),
        ),
    }
}

// Argument helpers

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn text_argument(function: &str, slot: &str, value: i64) -> Result<&'static str, i64> {
    match value::as_str(value) {
        Some(text) => Ok(text),
        None => Err(type_error(&format!(
            "{function}() argument '{slot}' must be str, not {}",
            value::type_name(value)
        ))),
    }
}

fn bytes_argument(_function: &str, value: i64) -> Result<&'static [u8], i64> {
    match value::as_bytes(value) {
        Some(data) => Ok(data),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "a bytes-like object is required, not '{}'",
                value::type_name(value)
            ),
        )),
    }
}

/// Resolves the optional `encoding` argument (index 1) to the canonical name.
fn encoding_argument(args: &[i64], function: &str) -> Result<&'static str, i64> {
    let raw = match args.get(1).copied() {
        None => "utf-8",
        Some(value) => text_argument(function, "encoding", value)?,
    };
    if raw.contains('\0') {
        return Err(exc::raise_with_message(
            VALUE_ERROR,
            "embedded null character",
        ));
    }
    let normalized = raw.trim_matches(|c: char| c.is_ascii_whitespace());
    let normalized = normalized.to_ascii_lowercase().replace('_', "-");
    match normalized.as_str() {
        "utf-8" | "utf8" | "u8" | "cp65001" => Ok("utf-8"),
        _ => Err(lookup_error_message(&format!("unknown encoding: {raw}"))),
    }
}

/// Resolves the optional `errors` argument (index 2). `encode`/`decode`
/// require a string (CPython rejects `None` there).
fn errors_argument(args: &[i64], function: &str) -> Result<&'static str, i64> {
    match args.get(2).copied() {
        None => Ok("strict"),
        Some(value) => text_argument(function, "errors", value),
    }
}

/// Resolves an error-handler name to its behavior, consulting the registry
/// first so a registered handler can override a built-in name.
fn error_mode(name: &str) -> Result<ErrorMode, i64> {
    if let Some(handler) = registered_error(name) {
        return Ok(ErrorMode::Custom(handler));
    }
    match name {
        "strict" => Ok(ErrorMode::Strict),
        "ignore" => Ok(ErrorMode::Ignore),
        "replace" => Ok(ErrorMode::Replace),
        _ => Err(lookup_error_message(&format!(
            "unknown error handler name '{name}'"
        ))),
    }
}

fn lookup_error_message(message: &str) -> i64 {
    exc::raise_with_message(LOOKUP_ERROR, message)
}

fn registered_error(name: &str) -> Option<i64> {
    ERROR_HANDLERS.with(|handlers| {
        handlers
            .borrow()
            .iter()
            .rev()
            .find(|(registered, _)| registered.as_str() == name)
            .map(|(_, handler)| *handler)
    })
}

// encode / decode

fn encode(args: &[i64]) -> i64 {
    let text = match value::as_str(args[0]) {
        Some(text) => text,
        None => {
            return type_error(&format!(
                "utf_8_encode() argument 1 must be str, not {}",
                value::type_name(args[0])
            ))
        }
    };
    // Validate the encoding and handler even though UTF-8 encoding itself
    // cannot fail: callers rely on unknown names raising.
    let encoding = match encoding_argument(args, "encode") {
        Ok(encoding) => encoding,
        Err(error) => return error,
    };
    debug_assert_eq!(encoding, "utf-8");
    let errors = match errors_argument(args, "encode") {
        Ok(errors) => errors,
        Err(error) => return error,
    };
    if let Err(error) = error_mode(errors) {
        return error;
    }
    value::bytes_new(text.as_bytes().to_vec())
}

fn decode(args: &[i64]) -> i64 {
    let data = match bytes_argument("decode", args[0]) {
        Ok(data) => data,
        Err(error) => return error,
    };
    let encoding = match encoding_argument(args, "decode") {
        Ok(encoding) => encoding,
        Err(error) => return error,
    };
    debug_assert_eq!(encoding, "utf-8");
    let errors = match errors_argument(args, "decode") {
        Ok(errors) => errors,
        Err(error) => return error,
    };
    let mode = match error_mode(errors) {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    match decode_utf8(data, &mode, true) {
        Ok((text, _)) => value::str_alloc(text),
        Err(error) => error,
    }
}

fn utf_8_encode(args: &[i64]) -> i64 {
    let Some(text) = value::as_str(args[0]) else {
        return type_error(&format!(
            "utf_8_encode() argument 1 must be str, not {}",
            value::type_name(args[0])
        ));
    };
    let bytes = value::bytes_new(text.as_bytes().to_vec());
    let length = ops::int_result(text.chars().count() as i64);
    value::tuple_new(vec![bytes, length])
}

fn utf_8_decode(args: &[i64]) -> i64 {
    let data = match bytes_argument("utf_8_decode", args[0]) {
        Ok(data) => data,
        Err(error) => return error,
    };
    let errors = match args.get(1).copied() {
        None | Some(NONE) => "strict",
        Some(value) => match value::as_str(value) {
            Some(errors) => errors,
            None => {
                return type_error(&format!(
                    "utf_8_decode() argument 2 must be str or None, not {}",
                    value::type_name(value)
                ))
            }
        },
    };
    let mode = match error_mode(errors) {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    let final_ = args.get(2).copied().map(value::truthy).unwrap_or(false);
    match decode_utf8(data, &mode, final_) {
        Ok((text, consumed)) => {
            let text = value::str_alloc(text);
            let consumed = ops::int_result(consumed as i64);
            value::tuple_new(vec![text, consumed])
        }
        Err(error) => error,
    }
}

/// Strict/ignore/replace/custom-handler UTF-8 decoding.
///
/// Returns `(decoded_text, consumed_bytes)`. With `final == false` an
/// incomplete trailing sequence is left unconsumed (the incremental decoder
/// contract); otherwise it raises.
fn decode_utf8(data: &[u8], mode: &ErrorMode, final_: bool) -> Result<(String, usize), i64> {
    let mut out = String::with_capacity(data.len());
    let mut offset = 0usize;
    loop {
        if offset == data.len() {
            return Ok((out, offset));
        }
        match std::str::from_utf8(&data[offset..]) {
            Ok(text) => {
                out.push_str(text);
                return Ok((out, data.len()));
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    // `valid_up_to` bytes are valid by construction.
                    out.push_str(std::str::from_utf8(&data[offset..offset + valid]).unwrap_or(""));
                }
                let start = offset + valid;
                match error.error_len() {
                    None => {
                        if final_ {
                            return Err(unicode_decode_error(
                                data,
                                start,
                                data.len(),
                                "unexpected end of data",
                            ));
                        }
                        return Ok((out, start));
                    }
                    Some(length) => {
                        let end = start + length;
                        let reason = decode_reason(data[start]);
                        match mode {
                            ErrorMode::Strict => {
                                return Err(unicode_decode_error(data, start, end, reason));
                            }
                            ErrorMode::Ignore => {
                                offset = end;
                            }
                            ErrorMode::Replace => {
                                out.push('\u{FFFD}');
                                offset = end;
                            }
                            ErrorMode::Custom(handler) => {
                                let exception = decode_exception_value(data, start, end, reason);
                                let result = core_builtins::call(*handler, 1, [exception].as_ptr());
                                if exc::pending() {
                                    return Err(result);
                                }
                                let (replacement, position) = error_handler_result(result)?;
                                if position <= start as i64 || position > data.len() as i64 {
                                    return Err(exc::raise_with_message(
                                        VALUE_ERROR,
                                        &format!(
                                            "error handler returned invalid position {position}"
                                        ),
                                    ));
                                }
                                out.push_str(replacement);
                                offset = position as usize;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Classifies the first byte of an invalid sequence like CPython's UTF-8
/// decoder.
fn decode_reason(byte: u8) -> &'static str {
    if (0x80..=0xBF).contains(&byte) || !(0xC2..=0xF4).contains(&byte) {
        "invalid start byte"
    } else {
        "invalid continuation byte"
    }
}

fn unicode_decode_error(data: &[u8], start: usize, end: usize, reason: &str) -> i64 {
    let _ = end;
    let byte = data.get(start).copied().unwrap_or(0);
    exc::raise_with_message(
        UNICODE_DECODE_ERROR,
        &format!("'utf-8' codec can't decode byte 0x{byte:02x} in position {start}: {reason}"),
    )
}

/// Builds a non-pending `UnicodeDecodeError` value for a custom handler.
///
/// The args tuple mirrors CPython's shape as closely as this runtime's
/// string-only exception arguments allow: `(encoding, object_repr, start,
/// end, reason)`.
fn decode_exception_value(data: &[u8], start: usize, end: usize, reason: &str) -> i64 {
    let message = format!(
        "'utf-8' codec can't decode byte 0x{:02x} in position {start}: {reason}",
        data.get(start).copied().unwrap_or(0)
    );
    let args = vec![
        "utf-8".to_string(),
        format!("b\"{}\"", String::from_utf8_lossy(data)),
        start.to_string(),
        end.to_string(),
        reason.to_string(),
    ];
    exc::exception_value(UNICODE_DECODE_ERROR, &message, args)
}

/// Validates a handler result: a `(replacement: str, new_position: int)`
/// tuple.
fn error_handler_result(result: i64) -> Result<(&'static str, i64), i64> {
    let Some(items) = value::as_tuple(result) else {
        return Err(type_error(
            "error handler must return a tuple of (str, int)",
        ));
    };
    if items.len() != 2 {
        return Err(type_error(
            "error handler must return a tuple of (str, int)",
        ));
    }
    let Some(replacement) = value::as_str(items[0]) else {
        return Err(type_error("error handler replacement must be a str"));
    };
    let Some(position) = value::as_integer(items[1]) else {
        return Err(type_error("error handler position must be an int"));
    };
    Ok((replacement, position))
}

// escape codec

fn escape_encode(args: &[i64]) -> i64 {
    let Some(data) = value::as_bytes(args[0]) else {
        return type_error(&format!(
            "escape_encode() argument 1 must be bytes, not {}",
            value::type_name(args[0])
        ));
    };
    if let Some(errors) = args.get(1).copied() {
        if errors != NONE && value::as_str(errors).is_none() {
            return type_error(&format!(
                "escape_encode() argument 2 must be str or None, not {}",
                value::type_name(errors)
            ));
        }
    }
    let mut out = Vec::with_capacity(data.len() * 2);
    for &byte in data {
        match byte {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\'' => out.extend_from_slice(b"\\'"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            0x20..=0x7e => out.push(byte),
            _ => out.extend_from_slice(format!("\\x{byte:02x}").as_bytes()),
        }
    }
    let escaped = value::bytes_new(out);
    let length = ops::int_result(data.len() as i64);
    value::tuple_new(vec![escaped, length])
}

fn escape_decode(args: &[i64]) -> i64 {
    // CPython's legacy `escape_decode` also accepts a `str`; interpret it as
    // its UTF-8 bytes.
    let owned;
    let data: &[u8] = if let Some(data) = value::as_bytes(args[0]) {
        data
    } else if let Some(text) = value::as_str(args[0]) {
        owned = text.as_bytes().to_vec();
        &owned
    } else {
        return type_error(&format!(
            "escape_decode() argument 1 must be bytes, not {}",
            value::type_name(args[0])
        ));
    };
    let errors = match args.get(1).copied() {
        None | Some(NONE) => "strict",
        Some(value) => match value::as_str(value) {
            Some(errors) => errors,
            None => {
                return type_error(&format!(
                    "escape_decode() argument 2 must be str or None, not {}",
                    value::type_name(value)
                ))
            }
        },
    };
    if !matches!(errors, "strict" | "ignore" | "replace") {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!("decoding error; unknown error handling code: {errors}"),
        );
    }
    let strict = errors == "strict";
    let replace = errors == "replace";
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0usize;
    while i < data.len() {
        let byte = data[i];
        if byte != b'\\' {
            out.push(byte);
            i += 1;
            continue;
        }
        if i + 1 >= data.len() {
            return exc::raise_with_message(VALUE_ERROR, "Trailing \\ in string");
        }
        let escape = data[i + 1];
        match escape {
            b'\\' | b'\'' | b'"' => {
                out.push(escape);
                i += 2;
            }
            b'a' => {
                out.push(0x07);
                i += 2;
            }
            b'b' => {
                out.push(0x08);
                i += 2;
            }
            b'f' => {
                out.push(0x0c);
                i += 2;
            }
            b'n' => {
                out.push(b'\n');
                i += 2;
            }
            b'r' => {
                out.push(b'\r');
                i += 2;
            }
            b't' => {
                out.push(b'\t');
                i += 2;
            }
            b'v' => {
                out.push(0x0b);
                i += 2;
            }
            b'0'..=b'7' => {
                let mut value: u32 = 0;
                let mut digits = 0usize;
                let mut j = i + 1;
                while digits < 3 && j < data.len() && (b'0'..=b'7').contains(&data[j]) {
                    value = value * 8 + u32::from(data[j] - b'0');
                    digits += 1;
                    j += 1;
                }
                out.push((value & 0xff) as u8);
                i = j;
            }
            b'x' => {
                let digits_start = i + 2;
                let mut j = digits_start;
                while j < data.len() && j < digits_start + 2 && data[j].is_ascii_hexdigit() {
                    j += 1;
                }
                let digits = j - digits_start;
                if digits == 2 {
                    let hex = std::str::from_utf8(&data[digits_start..j]).unwrap_or("00");
                    out.push(u8::from_str_radix(hex, 16).unwrap_or(0));
                    i = j;
                } else if strict {
                    return exc::raise_with_message(
                        VALUE_ERROR,
                        &format!("invalid \\x escape at position {i}"),
                    );
                } else {
                    if replace {
                        out.push(b'?');
                    }
                    i = j;
                }
            }
            _ => {
                out.push(b'\\');
                out.push(escape);
                i += 2;
            }
        }
    }
    let decoded = value::bytes_new(out);
    let length = ops::int_result(data.len() as i64);
    value::tuple_new(vec![decoded, length])
}

// registries

/// `lookup(name)`: UTF-8 is built in; other names consult registered search
/// functions.
fn lookup(args: &[i64]) -> i64 {
    let name = match text_argument("lookup", "encoding", args[0]) {
        Ok(name) => name,
        Err(error) => return error,
    };
    if is_utf8_alias(name) {
        return codec_info();
    }
    let search_functions: Vec<i64> = SEARCH_FUNCTIONS.with(|functions| functions.borrow().clone());
    let name_value = value::str_alloc(name.to_string());
    for function in search_functions {
        let result = core_builtins::call(function, 1, [name_value].as_ptr());
        if exc::pending() {
            return result;
        }
        if result == NONE {
            continue;
        }
        let Some(items) = value::as_tuple(result) else {
            return type_error("codec search functions must return 4-tuples");
        };
        if items.len() != 4 {
            return type_error("codec search functions must return 4-tuples");
        }
        return result;
    }
    lookup_error_message(&format!("unknown encoding: {name}"))
}

fn is_utf8_alias(name: &str) -> bool {
    if name.contains('\0') {
        return false;
    }
    let normalized = name.trim_matches(|c: char| c.is_ascii_whitespace());
    let normalized = normalized.to_ascii_lowercase().replace('_', "-");
    matches!(normalized.as_str(), "utf-8" | "utf8" | "u8" | "cp65001")
}

/// Cached `CodecInfo`-shaped tuple for UTF-8: `(encode, decode, None, None)`.
///
/// The incremental codecs are absent (`None`), which `CodecInfo` permits and
/// which is honest: this runtime has no incremental decoder object.
fn codec_info() -> i64 {
    if let Some(cached) = globals::registry_get("_codecs.codec_info.utf-8") {
        return cached;
    }
    let encode = value::native_function_new("_codecs", "utf_8_encode", 1);
    let decode = value::native_function_new("_codecs", "utf_8_decode", 3);
    let info = value::tuple_new(vec![encode, decode, NONE, NONE]);
    globals::registry_set("_codecs.codec_info.utf-8", info);
    info
}

fn register(args: &[i64]) -> i64 {
    let function = args[0];
    if !value::truthy(core_builtins::callable(function)) {
        return type_error("argument must be callable");
    }
    heap::add_thread_root(function);
    SEARCH_FUNCTIONS.with(|functions| functions.borrow_mut().push(function));
    NONE
}

fn unregister(args: &[i64]) -> i64 {
    // CPython's unregister never diagnoses its argument; it silently removes
    // matching callables.
    let function = args[0];
    SEARCH_FUNCTIONS.with(|functions| {
        functions
            .borrow_mut()
            .retain(|registered| *registered != function);
    });
    NONE
}

fn register_error(args: &[i64]) -> i64 {
    let name = match value::as_str(args[0]) {
        Some(name) => name.to_string(),
        None => {
            return type_error(&format!(
                "register_error() argument 1 must be str, not {}",
                value::type_name(args[0])
            ))
        }
    };
    let handler = args[1];
    if !value::truthy(core_builtins::callable(handler)) {
        return type_error("handler must be callable");
    }
    heap::add_thread_root(handler);
    ERROR_HANDLERS.with(|handlers| {
        let mut handlers = handlers.borrow_mut();
        handlers.retain(|(registered, _)| registered.as_str() != name);
        handlers.push((name, handler));
    });
    NONE
}

fn lookup_error(args: &[i64]) -> i64 {
    let name = match value::as_str(args[0]) {
        Some(name) => name,
        None => {
            return type_error(&format!(
                "lookup_error() argument 1 must be str, not {}",
                value::type_name(args[0])
            ))
        }
    };
    if let Some(handler) = registered_error(name) {
        return handler;
    }
    if let Some(handler) = builtin_error_handler(name) {
        return handler;
    }
    lookup_error_message(&format!("unknown error handler name '{name}'"))
}

/// Native function value for a built-in error handler, cached for identity.
fn builtin_error_handler(name: &str) -> Option<i64> {
    let cache_key = match name {
        "strict" => "_codecs.strict_errors",
        "ignore" => "_codecs.ignore_errors",
        "replace" => "_codecs.replace_errors",
        _ => return None,
    };
    if let Some(cached) = globals::registry_get(cache_key) {
        return Some(cached);
    }
    let function = value::native_function_new("_codecs", leak_handler_name(name), 1);
    globals::registry_set(cache_key, function);
    Some(function)
}

/// `native_function_new` stores `&'static str` names. The three handler names
/// are fixed, so interning them once is enough.
fn leak_handler_name(name: &str) -> &'static str {
    match name {
        "strict" => "strict_errors",
        "ignore" => "ignore_errors",
        _ => "replace_errors",
    }
}

// Built-in error handlers

/// Extracts the `end` position from a `UnicodeDecodeError` created by this
/// module (`args[3]`).
fn decode_error_end(exception: i64) -> Result<i64, i64> {
    let Some(data) = value::exception_data(exception) else {
        return Err(type_error("error handler expects a UnicodeDecodeError"));
    };
    let Some(text) = data.args.get(3) else {
        return Err(type_error(
            "error handler expects a UnicodeDecodeError with start/end args",
        ));
    };
    match text.parse::<i64>() {
        Ok(end) => Ok(end),
        Err(_) => Err(type_error(
            "error handler expects a UnicodeDecodeError with start/end args",
        )),
    }
}

fn strict_errors(args: &[i64]) -> i64 {
    if value::exception_data(args[0]).is_none() {
        return type_error("strict_errors() argument must be an exception");
    }
    exc::raise_value(args[0])
}

fn ignore_errors(args: &[i64]) -> i64 {
    let end = match decode_error_end(args[0]) {
        Ok(end) => end,
        Err(error) => return error,
    };
    let replacement = value::str_alloc(String::new());
    let position = ops::int_result(end);
    value::tuple_new(vec![replacement, position])
}

fn replace_errors(args: &[i64]) -> i64 {
    let end = match decode_error_end(args[0]) {
        Ok(end) => end,
        Err(error) => return error,
    };
    let replacement = value::str_alloc("?".to_string());
    let position = ops::int_result(end);
    value::tuple_new(vec![replacement, position])
}
