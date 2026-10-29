//! `_codecs`: the built-in codecs and the codec/error-handler registries.
//!
//! Hydro's strings are UTF-8, and the runtime implements three codecs
//! natively: `utf-8`, `ascii` and `latin-1` (ISO-8859-1). Latin-1 maps every
//! byte to the code point of the same value and therefore never fails to
//! decode; ASCII rejects bytes above `0x7f` and both single-byte codecs
//! reject code points they cannot represent. Every other encoding name fails
//! with `LookupError` before any conversion happens; it is never silently
//! treated as UTF-8.
//!
//! The codec registry keeps callable search functions registered with
//! `register()` and consults them from `lookup()` after the built-in names
//! (and their aliases) are answered. Registered search functions must return
//! a 4-tuple (encode, decode, incremental encoder, incremental decoder), like
//! CPython's `CodecInfo` tuple. The built-in entries are the stateless
//! `utf_8_*`/`ascii_*`/`latin_1_*` functions; `encode`/`decode` themselves
//! take the encoding name and route to the same engines.
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
use crate::exc::{
    self, LOOKUP_ERROR, TYPE_ERROR, UNICODE_DECODE_ERROR, UNICODE_ENCODE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::ops;
use crate::value::{self, NONE};

thread_local! {
    /// Codec search functions, in registration order.
    static SEARCH_FUNCTIONS: RefCell<Vec<i64>> = const { RefCell::new(Vec::new()) };
    /// Named error handlers; later registrations replace earlier ones.
    static ERROR_HANDLERS: RefCell<Vec<(String, i64)>> = const { RefCell::new(Vec::new()) };
}

/// How malformed input is treated.
enum ErrorMode {
    Strict,
    Ignore,
    Replace,
    Custom(i64),
}

/// The codecs the runtime implements natively.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codec {
    Utf8,
    Ascii,
    Latin1,
}

impl Codec {
    /// The spelling CPython uses in error messages and registry keys.
    fn name(self) -> &'static str {
        match self {
            Codec::Utf8 => "utf-8",
            Codec::Ascii => "ascii",
            Codec::Latin1 => "latin-1",
        }
    }

    /// Highest code point the codec can represent.
    fn limit(self) -> u32 {
        match self {
            Codec::Utf8 => 0x10_FFFF,
            Codec::Ascii => 0x7f,
            Codec::Latin1 => 0xff,
        }
    }
}

/// Resolves an encoding name, including the CPython aliases, to a built-in
/// codec. Names are matched the way CPython normalizes them: surrounding
/// whitespace is ignored, case is folded and `_` is equivalent to `-`.
fn codec_for_name(name: &str) -> Option<Codec> {
    if name.contains('\0') {
        return None;
    }
    let normalized = name.trim_matches(|c: char| c.is_ascii_whitespace());
    let normalized = normalized.to_ascii_lowercase().replace('_', "-");
    match normalized.as_str() {
        "utf-8" | "utf8" | "u8" | "cp65001" => Some(Codec::Utf8),
        "ascii" | "us-ascii" | "us" | "646" => Some(Codec::Ascii),
        "latin-1" | "latin1" | "latin" | "iso-8859-1" | "iso8859-1" | "iso-latin-1" | "l1"
        | "cp819" | "8859" | "819" => Some(Codec::Latin1),
        _ => None,
    }
}

pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "encode" => encode(args),
        "decode" => decode(args),
        "utf_8_encode" => utf_8_encode(args),
        "utf_8_decode" => utf_8_decode(args),
        "ascii_encode" => ascii_encode(args),
        "ascii_decode" => ascii_decode(args),
        "latin_1_encode" => latin_1_encode(args),
        "latin_1_decode" => latin_1_decode(args),
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

/// Resolves the optional `encoding` argument (index 1) to a built-in codec.
fn encoding_argument(args: &[i64], function: &str) -> Result<Codec, i64> {
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
    codec_for_name(raw).ok_or_else(|| lookup_error_message(&format!("unknown encoding: {raw}")))
}

/// Resolves the optional `errors` argument (index 2). `encode`/`decode`
/// require a string (CPython rejects `None` there).
fn errors_argument(args: &[i64], function: &str) -> Result<&'static str, i64> {
    match args.get(2).copied() {
        None => Ok("strict"),
        Some(value) => text_argument(function, "errors", value),
    }
}

/// Resolves the `errors` argument of the stateless per-codec entry points,
/// which accept `None` as "strict" like CPython's `ascii_decode` family.
fn optional_errors_argument(
    args: &[i64],
    index: usize,
    function: &str,
) -> Result<&'static str, i64> {
    match args.get(index).copied() {
        None | Some(NONE) => Ok("strict"),
        Some(value) => match value::as_str(value) {
            Some(errors) => Ok(errors),
            None => Err(type_error(&format!(
                "{function}() argument {} must be str or None, not {}",
                index + 1,
                value::type_name(value)
            ))),
        },
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
    let Some(text) = value::as_str(args[0]) else {
        return type_error(&format!(
            "encode() argument 'obj' must be str, not {}",
            value::type_name(args[0])
        ));
    };
    // Validate the encoding and handler before converting: callers rely on
    // unknown names raising even when the input cannot fail.
    let codec = match encoding_argument(args, "encode") {
        Ok(codec) => codec,
        Err(error) => return error,
    };
    let errors = match errors_argument(args, "encode") {
        Ok(errors) => errors,
        Err(error) => return error,
    };
    let mode = match error_mode(errors) {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    match encode_text(text, codec, &mode) {
        Ok(bytes) => value::bytes_new(bytes),
        Err(error) => error,
    }
}

fn decode(args: &[i64]) -> i64 {
    let data = match bytes_argument("decode", args[0]) {
        Ok(data) => data,
        Err(error) => return error,
    };
    let codec = match encoding_argument(args, "decode") {
        Ok(codec) => codec,
        Err(error) => return error,
    };
    let errors = match errors_argument(args, "decode") {
        Ok(errors) => errors,
        Err(error) => return error,
    };
    let mode = match error_mode(errors) {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    match decode_bytes(data, codec, &mode) {
        Ok(text) => value::str_alloc(text),
        Err(error) => error,
    }
}

/// Encodes `text` with `codec`. UTF-8 cannot fail; the single-byte codecs
/// report the runs they cannot represent.
fn encode_text(text: &str, codec: Codec, mode: &ErrorMode) -> Result<Vec<u8>, i64> {
    match codec {
        Codec::Utf8 => Ok(text.as_bytes().to_vec()),
        Codec::Ascii | Codec::Latin1 => encode_single_byte(text, codec, mode),
    }
}

/// Decodes `data` with `codec`, applying `mode` to bytes the codec rejects.
fn decode_bytes(data: &[u8], codec: Codec, mode: &ErrorMode) -> Result<String, i64> {
    match codec {
        Codec::Utf8 => decode_utf8(data, mode, true).map(|(text, _)| text),
        Codec::Ascii => decode_ascii(data, mode),
        // Latin-1 is a total mapping: every byte is U+0000..U+00FF, so no
        // error handler can ever be consulted.
        Codec::Latin1 => Ok(data.iter().map(|&byte| byte as char).collect()),
    }
}

/// `ascii_encode(text, errors='strict')` -> `(bytes, length)`.
fn ascii_encode(args: &[i64]) -> i64 {
    single_byte_encode(args, Codec::Ascii, "ascii_encode")
}

/// `latin_1_encode(text, errors='strict')` -> `(bytes, length)`.
fn latin_1_encode(args: &[i64]) -> i64 {
    single_byte_encode(args, Codec::Latin1, "latin_1_encode")
}

fn single_byte_encode(args: &[i64], codec: Codec, function: &str) -> i64 {
    let Some(text) = value::as_str(args[0]) else {
        return type_error(&format!(
            "{function}() argument 1 must be str, not {}",
            value::type_name(args[0])
        ));
    };
    let errors = match optional_errors_argument(args, 1, function) {
        Ok(errors) => errors,
        Err(error) => return error,
    };
    let mode = match error_mode(errors) {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    let bytes = match encode_single_byte(text, codec, &mode) {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    let length = ops::int_result(text.chars().count() as i64);
    let encoded = value::bytes_new(bytes);
    value::tuple_new(vec![encoded, length])
}

/// `ascii_decode(data, errors='strict')` -> `(text, consumed)`.
fn ascii_decode(args: &[i64]) -> i64 {
    single_byte_decode(args, Codec::Ascii, "ascii_decode")
}

/// `latin_1_decode(data, errors='strict')` -> `(text, consumed)`.
fn latin_1_decode(args: &[i64]) -> i64 {
    single_byte_decode(args, Codec::Latin1, "latin_1_decode")
}

fn single_byte_decode(args: &[i64], codec: Codec, function: &str) -> i64 {
    let data = match bytes_argument(function, args[0]) {
        Ok(data) => data,
        Err(error) => return error,
    };
    let errors = match optional_errors_argument(args, 1, function) {
        Ok(errors) => errors,
        Err(error) => return error,
    };
    let mode = match error_mode(errors) {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    let text = match decode_bytes(data, codec, &mode) {
        Ok(text) => text,
        Err(error) => return error,
    };
    let consumed = ops::int_result(data.len() as i64);
    let text = value::str_alloc(text);
    value::tuple_new(vec![text, consumed])
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
    let errors = match optional_errors_argument(args, 1, "utf_8_decode") {
        Ok(errors) => errors,
        Err(error) => return error,
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
                                Codec::Utf8,
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
                                return Err(unicode_decode_error(
                                    Codec::Utf8,
                                    data,
                                    start,
                                    end,
                                    reason,
                                ));
                            }
                            ErrorMode::Ignore => {
                                offset = end;
                            }
                            ErrorMode::Replace => {
                                out.push('\u{FFFD}');
                                offset = end;
                            }
                            ErrorMode::Custom(handler) => {
                                let exception =
                                    decode_exception_value(Codec::Utf8, data, start, end, reason);
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

/// Strict/ignore/replace/custom-handler ASCII decoding.
///
/// CPython reports every rejected byte on its own, so the error span is
/// always one byte wide. Latin-1 never reaches this path.
fn decode_ascii(data: &[u8], mode: &ErrorMode) -> Result<String, i64> {
    const REASON: &str = "ordinal not in range(128)";
    let mut out = String::with_capacity(data.len());
    let mut offset = 0usize;
    while offset < data.len() {
        let byte = data[offset];
        if byte < 0x80 {
            out.push(byte as char);
            offset += 1;
            continue;
        }
        let start = offset;
        let end = start + 1;
        match mode {
            ErrorMode::Strict => {
                return Err(unicode_decode_error(Codec::Ascii, data, start, end, REASON));
            }
            ErrorMode::Ignore => {}
            ErrorMode::Replace => out.push('\u{FFFD}'),
            ErrorMode::Custom(handler) => {
                let exception = decode_exception_value(Codec::Ascii, data, start, end, REASON);
                let result = core_builtins::call(*handler, 1, [exception].as_ptr());
                if exc::pending() {
                    return Err(result);
                }
                let (replacement, position) = error_handler_result(result)?;
                if position <= start as i64 || position > data.len() as i64 {
                    return Err(exc::raise_with_message(
                        VALUE_ERROR,
                        &format!("error handler returned invalid position {position}"),
                    ));
                }
                out.push_str(replacement);
                offset = position as usize;
                continue;
            }
        }
        offset = end;
    }
    Ok(out)
}

/// Character-index iterator for the single-byte encoder. A custom error
/// handler may request an arbitrary resume position, so the encoder rebuilds
/// this iterator from that index.
fn characters_from(text: &str, index: usize) -> impl Iterator<Item = (usize, char)> + '_ {
    text.chars().enumerate().skip(index)
}

/// Encodes `text` with a single-byte codec.
///
/// Consecutive unrepresentable characters share one error span, matching
/// CPython's report. The strict/ignore/replace modes stream without an index
/// buffer; a custom handler can request an arbitrary resume position, so the
/// iterator is rebuilt from that character index.
fn encode_single_byte(text: &str, codec: Codec, mode: &ErrorMode) -> Result<Vec<u8>, i64> {
    debug_assert_ne!(codec, Codec::Utf8);
    let limit = codec.limit();
    let reason = format!("ordinal not in range({})", limit + 1);
    let character_count = match mode {
        ErrorMode::Custom(_) => text.chars().count(),
        _ => 0,
    };
    let mut out = Vec::with_capacity(text.len());
    let mut characters = characters_from(text, 0).peekable();
    while let Some((start, character)) = characters.next() {
        let code = character as u32;
        if code <= limit {
            out.push(code as u8);
            continue;
        }
        let mut end = start + 1;
        while let Some(&(index, next)) = characters.peek() {
            if (next as u32) <= limit {
                break;
            }
            end = index + 1;
            characters.next();
        }
        match mode {
            ErrorMode::Strict => {
                return Err(unicode_encode_error(codec, text, start, end, &reason));
            }
            ErrorMode::Ignore => {}
            ErrorMode::Replace => {
                // One '?' per unrepresentable character, like CPython.
                out.resize(out.len() + (end - start), b'?');
            }
            ErrorMode::Custom(handler) => {
                let exception = encode_exception_value(codec, text, start, end, &reason);
                let result = core_builtins::call(*handler, 1, [exception].as_ptr());
                if exc::pending() {
                    return Err(result);
                }
                let (replacement, position) = error_handler_result(result)?;
                if position <= start as i64 || position > character_count as i64 {
                    return Err(exc::raise_with_message(
                        VALUE_ERROR,
                        &format!("error handler returned invalid position {position}"),
                    ));
                }
                // The replacement is encoded with the same codec; a character
                // it cannot represent raises for the original span, like
                // CPython.
                for replacement_character in replacement.chars() {
                    let replacement_code = replacement_character as u32;
                    if replacement_code > limit {
                        return Err(unicode_encode_error(codec, text, start, end, &reason));
                    }
                    out.push(replacement_code as u8);
                }
                characters = characters_from(text, position as usize).peekable();
            }
        }
    }
    Ok(out)
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

fn unicode_decode_error(codec: Codec, data: &[u8], start: usize, _end: usize, reason: &str) -> i64 {
    let byte = data.get(start).copied().unwrap_or(0);
    exc::raise_with_message(
        UNICODE_DECODE_ERROR,
        &format!(
            "'{}' codec can't decode byte 0x{byte:02x} in position {start}: {reason}",
            codec.name()
        ),
    )
}

/// Builds a non-pending `UnicodeDecodeError` value for a custom handler.
///
/// The args tuple mirrors CPython's shape as closely as this runtime's
/// string-only exception arguments allow: `(encoding, object_repr, start,
/// end, reason)`.
fn decode_exception_value(
    codec: Codec,
    data: &[u8],
    start: usize,
    end: usize,
    reason: &str,
) -> i64 {
    let message = format!(
        "'{}' codec can't decode byte 0x{:02x} in position {start}: {reason}",
        codec.name(),
        data.get(start).copied().unwrap_or(0)
    );
    let args = vec![
        codec.name().to_string(),
        format!("b\"{}\"", String::from_utf8_lossy(data)),
        start.to_string(),
        end.to_string(),
        reason.to_string(),
    ];
    exc::exception_value(UNICODE_DECODE_ERROR, &message, args)
}

/// `UnicodeEncodeError` for the character run starting at `start`.
fn unicode_encode_error(codec: Codec, text: &str, start: usize, _end: usize, reason: &str) -> i64 {
    let character = text.chars().nth(start).unwrap_or('\u{0}');
    exc::raise_with_message(
        UNICODE_ENCODE_ERROR,
        &format!(
            "'{}' codec can't encode character '{}' in position {start}: {reason}",
            codec.name(),
            escaped_character(character)
        ),
    )
}

/// Builds a non-pending `UnicodeEncodeError` value for a custom handler, with
/// the same `(encoding, object, start, end, reason)` shape CPython uses.
fn encode_exception_value(codec: Codec, text: &str, start: usize, end: usize, reason: &str) -> i64 {
    let character = text.chars().nth(start).unwrap_or('\u{0}');
    let message = format!(
        "'{}' codec can't encode character '{}' in position {start}: {reason}",
        codec.name(),
        escaped_character(character)
    );
    let args = vec![
        codec.name().to_string(),
        text.to_string(),
        start.to_string(),
        end.to_string(),
        reason.to_string(),
    ];
    exc::exception_value(UNICODE_ENCODE_ERROR, &message, args)
}

/// Renders a code point the way CPython's `UnicodeEncodeError` message does:
/// `\xNN`, `\uNNNN` or `\UNNNNNNNN`.
fn escaped_character(character: char) -> String {
    let code = character as u32;
    if code <= 0xff {
        format!("\\x{code:02x}")
    } else if code <= 0xffff {
        format!("\\u{code:04x}")
    } else {
        format!("\\U{code:08x}")
    }
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

/// `lookup(name)`: the built-in codecs (and their aliases) are answered
/// first; other names consult the registered search functions.
fn lookup(args: &[i64]) -> i64 {
    let name = match text_argument("lookup", "encoding", args[0]) {
        Ok(name) => name,
        Err(error) => return error,
    };
    if let Some(codec) = codec_for_name(name) {
        return codec_info(codec);
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
        // A search function may return a plain 4-tuple or a
        // `codecs.CodecInfo` entry (what `encodings.search_function` builds).
        if value::instance_class_value(result) == Some(crate::modules::codecs_codecinfo_class()) {
            let encode = value::instance_attr(result, "encode").unwrap_or(NONE);
            let decode = value::instance_attr(result, "decode").unwrap_or(NONE);
            if !value::truthy(core_builtins::callable(encode))
                || !value::truthy(core_builtins::callable(decode))
            {
                return type_error("incompatible codecs in CodecInfo");
            }
            return result;
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

/// Cached `CodecInfo`-shaped tuple for a built-in codec:
/// `(encode, decode, None, None)`.
///
/// The encode/decode entries are the stateless per-codec functions, which
/// take `(input, errors='strict')`. The incremental codecs are absent
/// (`None`), which `CodecInfo` permits and which is honest: this runtime has
/// no incremental codec object.
fn codec_info(codec: Codec) -> i64 {
    let key = format!("_codecs.codec_info.{}", codec.name());
    if let Some(cached) = globals::registry_get(&key) {
        return cached;
    }
    let (encode_name, encode_arity, decode_name, decode_arity) = match codec {
        Codec::Utf8 => ("utf_8_encode", 1, "utf_8_decode", 3),
        Codec::Ascii => ("ascii_encode", 2, "ascii_decode", 2),
        Codec::Latin1 => ("latin_1_encode", 2, "latin_1_decode", 2),
    };
    let encode = value::native_function_new("_codecs", encode_name, encode_arity);
    let decode = value::native_function_new("_codecs", decode_name, decode_arity);
    let info = value::tuple_new(vec![encode, decode, NONE, NONE]);
    globals::registry_set(&key, info);
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

/// Extracts the `end` position from a `UnicodeDecodeError`/`UnicodeEncodeError`
/// created by this module (`args[3]`).
fn error_handler_end(exception: i64) -> Result<i64, i64> {
    let Some(data) = value::exception_data(exception) else {
        return Err(type_error(
            "error handler expects a UnicodeDecodeError or UnicodeEncodeError",
        ));
    };
    let Some(text) = data.args.get(3) else {
        return Err(type_error(
            "error handler expects a Unicode error with start/end args",
        ));
    };
    match text.parse::<i64>() {
        Ok(end) => Ok(end),
        Err(_) => Err(type_error(
            "error handler expects a Unicode error with start/end args",
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
    let end = match error_handler_end(args[0]) {
        Ok(end) => end,
        Err(error) => return error,
    };
    let replacement = value::str_alloc(String::new());
    let position = ops::int_result(end);
    value::tuple_new(vec![replacement, position])
}

fn replace_errors(args: &[i64]) -> i64 {
    let end = match error_handler_end(args[0]) {
        Ok(end) => end,
        Err(error) => return error,
    };
    // CPython uses U+FFFD for undecodable bytes and '?' for characters the
    // target codec cannot encode.
    let replacement_text = match value::exception_data(args[0]).map(|data| data.tag) {
        Some(UNICODE_DECODE_ERROR) => "\u{FFFD}",
        _ => "?",
    };
    let replacement = value::str_alloc(replacement_text.to_string());
    let position = ops::int_result(end);
    value::tuple_new(vec![replacement, position])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resets the process-wide runtime state and returns the lock that
    /// serialises this module's tests (the global store is shared by all
    /// threads, so resets must not interleave).
    fn init() -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::heap::heap_reset_test_guard();
        crate::core::globals::reset();
        exc::init();
        guard
    }

    fn text(s: &str) -> i64 {
        value::str_alloc(s.to_string())
    }

    fn bytes(data: &[u8]) -> i64 {
        value::bytes_new(data.to_vec())
    }

    fn str_of(v: i64) -> String {
        value::as_str(v).expect("str value").to_string()
    }

    fn bytes_of(v: i64) -> Vec<u8> {
        value::as_bytes(v).expect("bytes value").to_vec()
    }

    fn info_entry(name: &str, index: usize) -> i64 {
        let info = call("lookup", &[text(name)]);
        assert!(!exc::pending(), "lookup({name}) must not fail");
        let items = value::as_tuple(info).expect("CodecInfo-shaped tuple");
        assert_eq!(items.len(), 4, "{name} entry");
        items[index]
    }

    #[test]
    fn ascii_round_trips_through_names_and_aliases() {
        let _guard = init();
        let encoded = call("encode", &[text("abc"), text("ascii")]);
        assert_eq!(bytes_of(encoded), b"abc");
        let decoded = call("decode", &[bytes(b"abc"), text("ascii")]);
        assert_eq!(str_of(decoded), "abc");
        for alias in ["ascii", "us-ascii", "US_ASCII", "646"] {
            let encoded = call("encode", &[text("hi"), text(alias)]);
            assert!(!exc::pending(), "{alias}");
            assert_eq!(bytes_of(encoded), b"hi");
            let decoded = call("decode", &[bytes(b"hi"), text(alias)]);
            assert_eq!(str_of(decoded), "hi", "{alias}");
        }
    }

    #[test]
    fn latin_1_decodes_every_byte_and_answers_aliases() {
        let _guard = init();
        for alias in [
            "latin-1",
            "latin1",
            "latin_1",
            "iso-8859-1",
            "ISO_8859_1",
            "l1",
        ] {
            let decoded = call("decode", &[bytes(&[0xff, 0x80, 0x00]), text(alias)]);
            assert!(!exc::pending(), "{alias}");
            assert_eq!(str_of(decoded), "\u{ff}\u{80}\u{0}", "{alias}");
        }
        let decoded = call("decode", &[bytes(&[0xe9]), text("latin-1")]);
        assert_eq!(str_of(decoded), "é");
        let encoded = call("encode", &[text("é"), text("latin-1")]);
        assert_eq!(bytes_of(encoded), vec![0xe9]);
        // The error-handler name is validated even though Latin-1 decoding
        // cannot fail.
        call("decode", &[bytes(&[0xff]), text("latin-1"), text("bogus")]);
        assert_eq!(exc::pending_tag(), i64::from(LOOKUP_ERROR));
    }

    #[test]
    fn ascii_decode_strict_ignore_and_replace() {
        let _guard = init();
        call("decode", &[bytes(b"a\xffb"), text("ascii")]);
        assert_eq!(exc::pending_tag(), i64::from(UNICODE_DECODE_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'ascii' codec can't decode byte 0xff in position 1: ordinal not in range(128)"
        );
        exc::clear();
        let decoded = call("decode", &[bytes(b"a\xffb"), text("ascii"), text("ignore")]);
        assert_eq!(str_of(decoded), "ab");
        let decoded = call(
            "decode",
            &[bytes(b"a\xffb"), text("ascii"), text("replace")],
        );
        assert_eq!(str_of(decoded), "a\u{fffd}b");
    }

    #[test]
    fn unrepresentable_characters_raise_and_honour_error_modes() {
        let _guard = init();
        call("encode", &[text("aé"), text("ascii")]);
        assert_eq!(exc::pending_tag(), i64::from(UNICODE_ENCODE_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'ascii' codec can't encode character '\\xe9' in position 1: \
             ordinal not in range(128)"
        );
        exc::clear();
        assert_eq!(
            bytes_of(call(
                "encode",
                &[text("aéé"), text("ascii"), text("ignore")]
            )),
            b"a"
        );
        assert_eq!(
            bytes_of(call(
                "encode",
                &[text("aéé"), text("ascii"), text("replace")]
            )),
            b"a??"
        );

        call("encode", &[text("€"), text("latin-1")]);
        assert_eq!(exc::pending_tag(), i64::from(UNICODE_ENCODE_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'latin-1' codec can't encode character '\\u20ac' in position 0: \
             ordinal not in range(256)"
        );
        exc::clear();
        assert_eq!(
            bytes_of(call(
                "encode",
                &[text("€"), text("latin-1"), text("replace")]
            )),
            b"?"
        );
        assert_eq!(
            bytes_of(call(
                "encode",
                &[text("€"), text("latin-1"), text("ignore")]
            )),
            b""
        );
    }

    #[test]
    fn stateless_entries_report_input_lengths() {
        let _guard = init();
        let encoded = call("ascii_encode", &[text("aé"), text("replace")]);
        let items = value::as_tuple(encoded).expect("tuple");
        assert_eq!(bytes_of(items[0]), b"a?");
        assert_eq!(value::as_integer(items[1]), Some(2));

        let decoded = call("latin_1_decode", &[bytes(&[0xff])]);
        let items = value::as_tuple(decoded).expect("tuple");
        assert_eq!(str_of(items[0]), "ÿ");
        assert_eq!(value::as_integer(items[1]), Some(1));

        // `None` means "strict" for the stateless decoders.
        let decoded = call("ascii_decode", &[bytes(b"ok"), NONE]);
        let items = value::as_tuple(decoded).expect("tuple");
        assert_eq!(str_of(items[0]), "ok");
        assert_eq!(value::as_integer(items[1]), Some(2));
    }

    #[test]
    fn lookup_names_built_in_codecs_and_rejects_unknown_names() {
        let _guard = init();
        for (alias, encode_name, decode_name) in [
            ("utf-8", "utf_8_encode", "utf_8_decode"),
            ("ascii", "ascii_encode", "ascii_decode"),
            ("latin_1", "latin_1_encode", "latin_1_decode"),
            ("iso-8859-1", "latin_1_encode", "latin_1_decode"),
        ] {
            let encode = info_entry(alias, 0);
            assert_eq!(
                value::as_function(encode).map(|function| function.name.as_str()),
                Some(encode_name),
                "{alias}"
            );
            let decode = info_entry(alias, 1);
            assert_eq!(
                value::as_function(decode).map(|function| function.name.as_str()),
                Some(decode_name),
                "{alias}"
            );
            assert_eq!(info_entry(alias, 2), NONE, "{alias} incremental encoder");
            assert_eq!(info_entry(alias, 3), NONE, "{alias} incremental decoder");
        }
        call("lookup", &[text("cp1252")]);
        assert_eq!(exc::pending_tag(), i64::from(LOOKUP_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "unknown encoding: cp1252"
        );
    }

    #[test]
    fn registered_handlers_apply_to_encode_and_decode() {
        let _guard = init();
        let handler = call("lookup_error", &[text("ignore")]);
        assert!(!exc::pending());
        assert_eq!(call("register_error", &[text("mine"), handler]), NONE);
        assert_eq!(
            str_of(call(
                "decode",
                &[bytes(b"a\xffb"), text("ascii"), text("mine")]
            )),
            "ab"
        );
        assert_eq!(
            bytes_of(call("encode", &[text("aé"), text("ascii"), text("mine")])),
            b"a"
        );
    }
}
