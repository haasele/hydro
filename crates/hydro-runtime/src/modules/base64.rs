//! `base64`: RFC 4648 Base16/Base32/Base64/Base32hex codecs, RFC 1924 Base85,
//! Adobe Ascii85, ZeroMQ Base85 and the legacy file helpers.
//!
//! The implementation mirrors CPython 3.14's `Lib/base64.py`: Base64 accepts
//! an alternative alphabet via `altchars`, `b64decode(validate=True)` enables
//! the strict `a2b_base64` checks, `encodebytes` wraps at 76 columns with a
//! trailing newline per chunk, Base32 follows the Python-level padding
//! algorithm (including `casefold` and the RFC 4648 §2.4 `map01` mapping), and
//! the Base85 family reproduces CPython's zero/space compression, Adobe
//! framing, padding and error reporting. Decoders accept ASCII `str` and any
//! buffer object (`bytes`, `bytearray`, `memoryview` or `array`); encoders
//! accept buffers only. Conversion failures raise `binascii.Error` for the
//! Base16/32/64 decoders and `ValueError` for the Base85 family, exactly like
//! CPython. No input can panic.

use super::array_mod;
use super::binascii::{self, argument, flag_argument};
use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value::{self, NONE};

/// Uppercase Base32 alphabet (RFC 4648 §6).
const B32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Base32hex alphabet (RFC 4648 §7): digits then `A`-`V`.
const B32HEX_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHIJKLMNOPQRSTUV";

/// Base85 alphabet shared by `b85encode`/`b85decode` (RFC 1924).
const B85_ALPHABET: &[u8; 85] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";

/// ZeroMQ Base85 alphabet.
const Z85_ALPHABET: &[u8; 85] =
    b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ.-:+=^!/*?&<>()[]{}@%$#";

/// The characters Ascii85 uses: `!` through `u`.
const A85_FIRST: u8 = b'!';
const A85_LAST: u8 = b'u';

/// Adobe Ascii85 framing markers.
const A85_START: &[u8] = b"<~";
const A85_END: &[u8] = b"~>";

/// Bytes per Base64 output line in `encodebytes` (76 columns / 4 * 3).
const MAXBINSIZE: usize = 57;

/// Column width of a Base64 line, excluding the newline.
const MAXLINESIZE: usize = 76;

/// The names `Lib/base64.py` exports.
const BASE64_ALL: &[&str] = &[
    "encode",
    "decode",
    "encodebytes",
    "decodebytes",
    "b64encode",
    "b64decode",
    "b32encode",
    "b32decode",
    "b32hexencode",
    "b32hexdecode",
    "b16encode",
    "b16decode",
    "b85encode",
    "b85decode",
    "a85encode",
    "a85decode",
    "z85encode",
    "z85decode",
    "standard_b64encode",
    "standard_b64decode",
    "urlsafe_b64encode",
    "urlsafe_b64decode",
];

/// Hidden `array` instance slots (`array_mod`'s storage names).
const ARRAY_DATA_ATTR: &str = "\u{0}array.data";
const ARRAY_CODE_ATTR: &str = "\u{0}array.code";

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

/// Raises `binascii.Error(message)`, the class the Base16/32/64 decoders use.
fn binascii_error(message: &str) -> i64 {
    let class = binascii::error_class();
    let argument = value::str_alloc(message.to_string());
    let roots = crate::heap::root_values(std::slice::from_ref(&argument));
    let instance = crate::builtins::call(class, 1, &argument as *const i64);
    crate::heap::release_roots(roots);
    if exc::pending() {
        return instance;
    }
    exc::raise_value(instance)
}

/// Raw payload of an `array.array` instance, if `value_arg` is one.
fn array_payload(value_arg: i64) -> Option<Vec<u8>> {
    if value::instance_class_value(value_arg)? != array_mod::array_class() {
        return None;
    }
    let data = value::instance_attr(value_arg, ARRAY_DATA_ATTR)?;
    value::as_bytes(data).map(|bytes| bytes.to_vec())
}

/// The typecode of an `array.array` instance, if `value_arg` is one.
fn array_typecode(value_arg: i64) -> Option<&'static str> {
    if value::instance_class_value(value_arg)? != array_mod::array_class() {
        return None;
    }
    let code = value::instance_attr(value_arg, ARRAY_CODE_ATTR)?;
    value::as_str(code)
}

/// Copies any buffer object CPython's modern API accepts.
fn buffer_bytes(value_arg: i64, name: &str) -> Result<Vec<u8>, i64> {
    if let Some(bytes) = value::as_bytes(value_arg) {
        return Ok(bytes.to_vec());
    }
    if let Some(view) = value::as_memoryview(value_arg) {
        return value::memoryview_bytes(view).map_err(value_error);
    }
    if let Some(bytes) = array_payload(value_arg) {
        return Ok(bytes);
    }
    Err(type_error(&format!(
        "argument '{name}' must be a bytes-like object, not '{}'",
        value::type_name(value_arg)
    )))
}

/// CPython's `_bytes_from_decode_data`: ASCII `str`, or any buffer object.
fn decode_data(value_arg: i64, name: &str) -> Result<Vec<u8>, i64> {
    if let Some(text) = value::as_str(value_arg) {
        if !text.is_ascii() {
            return Err(value_error(
                "string argument should contain only ASCII characters",
            ));
        }
        return Ok(text.as_bytes().to_vec());
    }
    buffer_bytes(value_arg, name)
}

/// `_input_type_check`: `encodebytes`/`decodebytes` require single-byte,
/// one-dimensional buffers (so `str`, lists and multi-byte formats fail).
fn strict_byte_input(value_arg: i64, _name: &str) -> Result<Vec<u8>, i64> {
    if let Some(bytes) = value::as_bytes(value_arg) {
        return Ok(bytes.to_vec());
    }
    if let Some(view) = value::as_memoryview(value_arg) {
        if !matches!(view.format, 'c' | 'b' | 'B') {
            return Err(type_error(&format!(
                "expected single byte elements, not '{}' from {}",
                view.format,
                value::type_name(value_arg)
            )));
        }
        return value::memoryview_bytes(view).map_err(value_error);
    }
    if let Some(bytes) = array_payload(value_arg) {
        if matches!(array_typecode(value_arg), Some("b" | "B" | "c")) {
            return Ok(bytes);
        }
    }
    Err(type_error(&format!(
        "expected bytes-like object, not {}",
        value::type_name(value_arg)
    )))
}

/// Integer slot with a default.
fn int_argument(value_arg: i64, name: &str, default: i64) -> Result<i64, i64> {
    if value_arg == NONE {
        return Ok(default);
    }
    value::as_integer(value_arg).ok_or_else(|| {
        type_error(&format!(
            "argument '{name}' must be an integer, not '{}'",
            value::type_name(value_arg)
        ))
    })
}

/// Reads an optional two-byte alternative alphabet (buffers only).
fn altchars_encode(value_arg: i64) -> Result<Option<[u8; 2]>, i64> {
    if value_arg == NONE {
        return Ok(None);
    }
    let bytes = buffer_bytes(value_arg, "altchars")?;
    match bytes.as_slice() {
        [first, second] => Ok(Some([*first, *second])),
        _ => Err(value_error(
            "altchars must be a bytes-like object of length 2",
        )),
    }
}

/// Reads an optional two-byte alternative alphabet for decoding, where CPython
/// also accepts an ASCII `str`.
fn altchars_decode(value_arg: i64) -> Result<Option<[u8; 2]>, i64> {
    if value_arg == NONE {
        return Ok(None);
    }
    let bytes = decode_data(value_arg, "altchars")?;
    match bytes.as_slice() {
        [first, second] => Ok(Some([*first, *second])),
        _ => Err(value_error(
            "altchars must be a bytes-like object of length 2",
        )),
    }
}

/// Replaces the standard `+`/`/` with the alternative pair, or (when
/// decoding) the other way around.
fn translate_altchars(data: &mut [u8], altchars: [u8; 2], encode: bool) {
    for byte in data.iter_mut() {
        if encode {
            match *byte {
                b'+' => *byte = altchars[0],
                b'/' => *byte = altchars[1],
                _ => {}
            }
        } else if *byte == altchars[0] {
            *byte = b'+';
        } else if *byte == altchars[1] {
            *byte = b'/';
        }
    }
}

// -------------------------------------------------------------------------
// Base64
// -------------------------------------------------------------------------

/// `urlsafe_b64encode(s)`.
fn urlsafe_b64encode(args: &[i64]) -> i64 {
    let data = match buffer_bytes(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let mut encoded = binascii::base64_encode(&data);
    for byte in encoded.iter_mut() {
        match *byte {
            b'+' => *byte = b'-',
            b'/' => *byte = b'_',
            _ => {}
        }
    }
    value::bytes_new(encoded)
}

/// `b64encode(s, altchars=None)`.
fn b64encode(args: &[i64]) -> i64 {
    let data = match buffer_bytes(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let alternative = match altchars_encode(argument(args, 1)) {
        Ok(alternative) => alternative,
        Err(error) => return error,
    };
    let mut encoded = binascii::base64_encode(&data);
    if let Some(alternative) = alternative {
        translate_altchars(&mut encoded, alternative, true);
    }
    value::bytes_new(encoded)
}

/// `b64decode(s, altchars=None, validate=False)`.
fn b64decode(args: &[i64]) -> i64 {
    let mut data = match decode_data(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let alternative = match altchars_decode(argument(args, 1)) {
        Ok(alternative) => alternative,
        Err(error) => return error,
    };
    let validate = flag_argument(argument(args, 2), false);
    if let Some(alternative) = alternative {
        translate_altchars(&mut data, alternative, false);
    }
    match binascii::base64_decode(&data, validate) {
        Ok(decoded) => value::bytes_new(decoded),
        Err(message) => binascii_error(&message),
    }
}

/// `urlsafe_b64decode(s)`: `-`/`_` are translated before decoding.
fn urlsafe_b64decode(args: &[i64]) -> i64 {
    let mut data = match decode_data(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    // CPython 3.15 adds `padded`; pydantic uses `padded=True` in a version
    // branch that is unreachable on the 3.14 target, but the call still has
    // to type-check. Accepting the keyword here keeps that forward-compatible
    // call valid and restores padding only when requested.
    if flag_argument(argument(args, 1), false) {
        while !data.len().is_multiple_of(4) {
            data.push(b'=');
        }
    }
    for byte in data.iter_mut() {
        match *byte {
            b'-' => *byte = b'+',
            b'_' => *byte = b'/',
            _ => {}
        }
    }
    match binascii::base64_decode(&data, false) {
        Ok(decoded) => value::bytes_new(decoded),
        Err(message) => binascii_error(&message),
    }
}

// -------------------------------------------------------------------------
// Multi-line helpers and file interface
// -------------------------------------------------------------------------

/// `encodebytes(s)`: 57-byte chunks, each encoded as a 76-column line.
fn encodebytes(args: &[i64]) -> i64 {
    let data = match strict_byte_input(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    if data.is_empty() {
        return value::bytes_new(Vec::new());
    }
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4 + data.len() / MAXBINSIZE + 2);
    for chunk in data.chunks(MAXBINSIZE) {
        out.extend_from_slice(&binascii::base64_encode(chunk));
        out.push(b'\n');
    }
    value::bytes_new(out)
}

/// `decodebytes(s)`: strict byte input, then lenient Base64 decoding.
fn decodebytes(args: &[i64]) -> i64 {
    let data = match strict_byte_input(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    match binascii::base64_decode(&data, false) {
        Ok(decoded) => value::bytes_new(decoded),
        Err(message) => binascii_error(&message),
    }
}

/// Calls an object's method by name (the file helpers' `read`/`write`).
fn call_method(obj: i64, name: &str, args: &[i64]) -> i64 {
    crate::methods::call(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

/// `encode(input, output)`: copy Base64 lines between binary files.
fn encode_file(args: &[i64]) -> i64 {
    let input = argument(args, 0);
    let output = argument(args, 1);
    if input == NONE || output == NONE {
        return type_error("encode() requires an input and an output file");
    }
    loop {
        let chunk = call_method(input, "read", &[crate::ops::int_result(MAXBINSIZE as i64)]);
        if exc::pending() {
            return NONE;
        }
        let Some(data) = value::as_bytes(chunk).map(|bytes| bytes.to_vec()) else {
            return type_error(&format!(
                "expected bytes-like object, not '{}'",
                value::type_name(chunk)
            ));
        };
        if data.is_empty() {
            return NONE;
        }
        let mut data = data;
        while data.len() < MAXBINSIZE {
            let want = (MAXBINSIZE - data.len()) as i64;
            let more_value = call_method(input, "read", &[crate::ops::int_result(want)]);
            if exc::pending() {
                return NONE;
            }
            let Some(more) = value::as_bytes(more_value) else {
                return type_error(&format!(
                    "expected bytes-like object, not '{}'",
                    value::type_name(more_value)
                ));
            };
            if more.is_empty() {
                break;
            }
            data.extend_from_slice(more);
        }
        let mut line = binascii::base64_encode(&data);
        line.push(b'\n');
        call_method(output, "write", &[value::bytes_new(line)]);
        if exc::pending() {
            return NONE;
        }
    }
}

/// `decode(input, output)`: decode one Base64 line at a time.
fn decode_file(args: &[i64]) -> i64 {
    let input = argument(args, 0);
    let output = argument(args, 1);
    if input == NONE || output == NONE {
        return type_error("decode() requires an input and an output file");
    }
    loop {
        let line = call_method(input, "readline", &[]);
        if exc::pending() {
            return NONE;
        }
        let Some(bytes) = value::as_bytes(line) else {
            return type_error(&format!(
                "expected bytes-like object, not '{}'",
                value::type_name(line)
            ));
        };
        if bytes.is_empty() {
            return NONE;
        }
        let decoded = match binascii::base64_decode(bytes, false) {
            Ok(decoded) => decoded,
            Err(message) => return binascii_error(&message),
        };
        call_method(output, "write", &[value::bytes_new(decoded)]);
        if exc::pending() {
            return NONE;
        }
    }
}

// -------------------------------------------------------------------------
// Base16
// -------------------------------------------------------------------------

/// `b16encode(s)`: uppercase hexadecimal.
fn b16encode(args: &[i64]) -> i64 {
    let data = match buffer_bytes(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let encoded = binascii::hex_encode(&data)
        .into_iter()
        .map(|byte| byte.to_ascii_uppercase())
        .collect();
    value::bytes_new(encoded)
}

/// `b16decode(s, casefold=False)`.
fn b16decode(args: &[i64]) -> i64 {
    let mut data = match decode_data(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    if flag_argument(argument(args, 1), false) {
        data.make_ascii_uppercase();
    }
    if data.iter().any(|byte| !is_base16(*byte)) {
        return binascii_error("Non-base16 digit found");
    }
    match binascii::hex_decode(&data) {
        Ok(decoded) => value::bytes_new(decoded),
        Err(message) => binascii_error(message),
    }
}

fn is_base16(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'A'..=b'F')
}

// -------------------------------------------------------------------------
// Base32 / Base32hex
// -------------------------------------------------------------------------

/// `_b32encode`: five-byte quanta, zero-padded, `=`-padded output.
fn b32_encode_with(alphabet: &[u8; 32], data: &[u8]) -> Vec<u8> {
    let mut data = data.to_vec();
    let leftover = data.len() % 5;
    if leftover != 0 {
        data.resize(data.len() + (5 - leftover), 0);
    }
    let mut encoded = Vec::with_capacity(data.len() / 5 * 8);
    for chunk in data.chunks(5) {
        let mut acc = 0u64;
        for &byte in chunk {
            acc = (acc << 8) | u64::from(byte);
        }
        for shift in [35u32, 30, 25, 20, 15, 10, 5, 0] {
            encoded.push(alphabet[((acc >> shift) & 0x1f) as usize]);
        }
    }
    let padding = match leftover {
        1 => 6,
        2 => 4,
        3 => 3,
        4 => 1,
        _ => 0,
    };
    if padding != 0 {
        let start = encoded.len() - padding;
        encoded[start..].fill(b'=');
    }
    encoded
}

/// Big-endian five-byte rendering of a 40-bit accumulator.
fn to_be5(acc: u64) -> [u8; 5] {
    [
        (acc >> 32) as u8,
        (acc >> 24) as u8,
        (acc >> 16) as u8,
        (acc >> 8) as u8,
        acc as u8,
    ]
}

/// Value of one character of `alphabet` (0-31).
fn b32_value(alphabet: &[u8; 32], byte: u8) -> Option<u8> {
    alphabet
        .iter()
        .position(|&candidate| candidate == byte)
        .map(|index| index as u8)
}

/// `_b32decode`: eight-character quanta with the RFC 4648 §2.4 `map01` rule.
fn b32_decode_with(
    alphabet: &[u8; 32],
    data: &[u8],
    casefold: bool,
    map01: Option<u8>,
) -> Result<Vec<u8>, String> {
    let mut data = data.to_vec();
    if !data.len().is_multiple_of(8) {
        return Err("Incorrect padding".to_string());
    }
    if let Some(replacement) = map01 {
        for byte in data.iter_mut() {
            match *byte {
                b'0' => *byte = b'O',
                b'1' => *byte = replacement,
                _ => {}
            }
        }
    }
    if casefold {
        data.make_ascii_uppercase();
    }
    let total = data.len();
    let mut body_len = total;
    while body_len > 0 && data[body_len - 1] == b'=' {
        body_len -= 1;
    }
    let padchars = total - body_len;
    let mut decoded = Vec::with_capacity(body_len / 8 * 5);
    let mut acc = 0u64;
    for chunk in data[..body_len].chunks(8) {
        acc = 0;
        for &byte in chunk {
            let Some(digit) = b32_value(alphabet, byte) else {
                return Err("Non-base32 digit found".to_string());
            };
            acc = (acc << 5) + u64::from(digit);
        }
        decoded.extend_from_slice(&to_be5(acc));
    }
    if !matches!(padchars, 0 | 1 | 3 | 4 | 6) {
        return Err("Incorrect padding".to_string());
    }
    if padchars != 0 && !decoded.is_empty() {
        acc <<= 5 * padchars as u32;
        let last = to_be5(acc);
        // 6 pads -> 1 byte, 4 -> 2, 3 -> 3, 1 -> 4.
        let leftover = (43 - 5 * padchars) / 8;
        decoded.truncate(decoded.len() - 5);
        decoded.extend_from_slice(&last[..leftover]);
    }
    Ok(decoded)
}

/// `b32encode(s)`.
fn b32encode(args: &[i64]) -> i64 {
    let data = match buffer_bytes(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    value::bytes_new(b32_encode_with(B32_ALPHABET, &data))
}

/// `b32decode(s, casefold=False, map01=None)`.
fn b32decode(args: &[i64]) -> i64 {
    let data = match decode_data(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let casefold = flag_argument(argument(args, 1), false);
    let map01 = match argument(args, 2) {
        NONE => None,
        map01 => match decode_data(map01, "map01") {
            Ok(mapping) => match mapping.as_slice() {
                [replacement] => Some(*replacement),
                _ => return value_error("map01 must be a bytes-like object of length 1"),
            },
            Err(error) => return error,
        },
    };
    match b32_decode_with(B32_ALPHABET, &data, casefold, map01) {
        Ok(decoded) => value::bytes_new(decoded),
        Err(message) => binascii_error(&message),
    }
}

/// `b32hexencode(s)`.
fn b32hexencode(args: &[i64]) -> i64 {
    let data = match buffer_bytes(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    value::bytes_new(b32_encode_with(B32HEX_ALPHABET, &data))
}

/// `b32hexdecode(s, casefold=False)`: no `map01` support.
fn b32hexdecode(args: &[i64]) -> i64 {
    let data = match decode_data(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let casefold = flag_argument(argument(args, 1), false);
    match b32_decode_with(B32HEX_ALPHABET, &data, casefold, None) {
        Ok(decoded) => value::bytes_new(decoded),
        Err(message) => binascii_error(&message),
    }
}

// -------------------------------------------------------------------------
// Base85 family
// -------------------------------------------------------------------------

/// `_85encode`: pack four-byte words into five base-85 digits, applying the
/// zero (`z`) and space (`y`) short sequences and trimming padded output.
fn encode_85(
    data: &[u8],
    alphabet: &[u8; 85],
    foldnuls: bool,
    foldspaces: bool,
    pad: bool,
) -> Vec<u8> {
    let mut data = data.to_vec();
    let padding = (4 - data.len() % 4) % 4;
    data.resize(data.len() + padding, 0);
    let mut out = Vec::with_capacity(data.len() / 4 * 5);
    let mut last_is_z = false;
    for chunk in data.chunks(4) {
        let word = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        last_is_z = false;
        if foldnuls && word == 0 {
            out.push(b'z');
            last_is_z = true;
        } else if foldspaces && word == 0x2020_2020 {
            out.push(b'y');
        } else {
            let mut digits = [0u8; 5];
            let mut remaining = word;
            for slot in digits.iter_mut().rev() {
                *slot = alphabet[(remaining % 85) as usize];
                remaining /= 85;
            }
            out.extend_from_slice(&digits);
        }
    }
    if padding != 0 && !pad {
        // A final zero word compressed to `z` expands back to five leading
        // digits before the padded characters are dropped.
        if last_is_z {
            out.truncate(out.len() - 1);
            out.extend_from_slice(&[alphabet[0]; 5]);
        }
        let keep = out.len().saturating_sub(padding);
        out.truncate(keep);
    }
    out
}

/// `_85decode` for the Base85 alphabet: pad with `~` (84) then trim.
fn b85_decode_bytes(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut data = data.to_vec();
    let padding = (5 - data.len() % 5) % 5;
    data.extend(std::iter::repeat_n(b'~', padding));
    let mut out = Vec::with_capacity(data.len() / 5 * 4);
    for (index, chunk) in data.chunks(5).enumerate() {
        let mut acc = 0u64;
        for (offset, &byte) in chunk.iter().enumerate() {
            let Some(digit) = b32_value_generic(B85_ALPHABET, byte) else {
                return Err(format!(
                    "bad base85 character at position {}",
                    index * 5 + offset
                ));
            };
            acc = acc * 85 + u64::from(digit);
        }
        if acc > u64::from(u32::MAX) {
            return Err(format!(
                "base85 overflow in hunk starting at byte {}",
                index * 5
            ));
        }
        out.extend_from_slice(&(acc as u32).to_be_bytes());
    }
    if padding != 0 {
        let keep = out.len().saturating_sub(padding);
        out.truncate(keep);
    }
    Ok(out)
}

/// Index of `byte` in an 85-character alphabet.
fn b32_value_generic(alphabet: &[u8; 85], byte: u8) -> Option<u8> {
    alphabet
        .iter()
        .position(|&candidate| candidate == byte)
        .map(|index| index as u8)
}

/// `b85encode(b, pad=False)`.
fn b85encode(args: &[i64]) -> i64 {
    let data = match buffer_bytes(argument(args, 0), "b") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let pad = flag_argument(argument(args, 1), false);
    value::bytes_new(encode_85(&data, B85_ALPHABET, false, false, pad))
}

/// `b85decode(b)`.
fn b85decode(args: &[i64]) -> i64 {
    let data = match decode_data(argument(args, 0), "b") {
        Ok(data) => data,
        Err(error) => return error,
    };
    match b85_decode_bytes(&data) {
        Ok(decoded) => value::bytes_new(decoded),
        Err(message) => value_error(&message),
    }
}

/// `z85encode(s)`: Base85 followed by the ZeroMQ alphabet translation.
fn z85encode(args: &[i64]) -> i64 {
    let data = match buffer_bytes(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let mut encoded = encode_85(&data, B85_ALPHABET, false, false, false);
    for byte in encoded.iter_mut() {
        if let Some(index) = b32_value_generic(B85_ALPHABET, *byte) {
            *byte = Z85_ALPHABET[index as usize];
        }
    }
    value::bytes_new(encoded)
}

/// `z85decode(s)`: translate the ZeroMQ alphabet back to Base85.
fn z85decode(args: &[i64]) -> i64 {
    let mut data = match decode_data(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    for byte in data.iter_mut() {
        if let Some(index) = b32_value_generic(Z85_ALPHABET, *byte) {
            *byte = B85_ALPHABET[index as usize];
        } else if matches!(*byte, b';' | b'_' | b'`' | b'|' | b'~') {
            // Base85-valid characters that ZeroMQ's alphabet does not have
            // must not decode as their Base85 values.
            *byte = 0;
        }
    }
    match b85_decode_bytes(&data) {
        Ok(decoded) => value::bytes_new(decoded),
        Err(message) => value_error(&message.replace("base85", "z85")),
    }
}

/// `a85encode(b, *, foldspaces=False, wrapcol=0, pad=False, adobe=False)`.
fn a85encode(args: &[i64]) -> i64 {
    let data = match buffer_bytes(argument(args, 0), "b") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let foldspaces = flag_argument(argument(args, 1), false);
    let wrapcol = match int_argument(argument(args, 2), "wrapcol", 0) {
        Ok(wrapcol) => wrapcol,
        Err(error) => return error,
    };
    let pad = flag_argument(argument(args, 3), false);
    let adobe = flag_argument(argument(args, 4), false);
    let mut result = encode_85(&data, A85_ALPHABET, true, foldspaces, pad);
    if adobe {
        let mut framed = Vec::with_capacity(result.len() + 4);
        framed.extend_from_slice(A85_START);
        framed.extend_from_slice(&result);
        result = framed;
    }
    if wrapcol != 0 {
        let width = std::cmp::max(if adobe { 2usize } else { 1 }, wrapcol.max(0) as usize);
        let mut chunks: Vec<&[u8]> = result.chunks(width).collect();
        if adobe && chunks.last().is_none_or(|last| last.len() + 2 > width) {
            chunks.push(&[]);
        }
        let mut wrapped = Vec::with_capacity(result.len() + chunks.len());
        for (index, chunk) in chunks.iter().enumerate() {
            if index != 0 {
                wrapped.push(b'\n');
            }
            wrapped.extend_from_slice(chunk);
        }
        result = wrapped;
    }
    if adobe {
        result.extend_from_slice(A85_END);
    }
    value::bytes_new(result)
}

/// `a85decode(b, *, foldspaces=False, adobe=False, ignorechars=b' \t\n\r\v')`.
fn a85decode(args: &[i64]) -> i64 {
    let mut data = match decode_data(argument(args, 0), "b") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let foldspaces = flag_argument(argument(args, 1), false);
    let adobe = flag_argument(argument(args, 2), false);
    // An omitted `ignorechars` uses CPython's whitespace default; an explicit
    // `None` is a `TypeError` (the runtime pads omitted trailing slots, so the
    // argument count distinguishes the two).
    let ignorechars = if args.len() <= 3 {
        b" \t\n\r\x0b".to_vec()
    } else {
        match buffer_bytes(argument(args, 3), "ignorechars") {
            Ok(ignorechars) => ignorechars,
            Err(error) => return error,
        }
    };
    if adobe {
        if !data.ends_with(A85_END) {
            return value_error("Ascii85 encoded byte sequences must end with b'~>'");
        }
        if data.starts_with(A85_START) {
            data.truncate(data.len() - A85_END.len());
            data.drain(..A85_START.len());
        } else {
            data.truncate(data.len() - A85_END.len());
        }
    }
    // Four `u` characters flush a trailing partial group; the extra decoded
    // bytes are removed below.
    data.extend_from_slice(b"uuuu");
    let mut decoded: Vec<u8> = Vec::new();
    let mut current: Vec<u8> = Vec::with_capacity(5);
    for &byte in &data {
        if (A85_FIRST..=A85_LAST).contains(&byte) {
            current.push(byte);
            if current.len() == 5 {
                let mut acc = 0u64;
                for &digit in &current {
                    acc = acc * 85 + u64::from(digit - A85_FIRST);
                }
                if acc > u64::from(u32::MAX) {
                    return value_error("Ascii85 overflow");
                }
                decoded.extend_from_slice(&(acc as u32).to_be_bytes());
                current.clear();
            }
        } else if byte == b'z' {
            if !current.is_empty() {
                return value_error("z inside Ascii85 5-tuple");
            }
            decoded.extend_from_slice(b"\0\0\0\0");
        } else if foldspaces && byte == b'y' {
            if !current.is_empty() {
                return value_error("y inside Ascii85 5-tuple");
            }
            decoded.extend_from_slice(b"    ");
        } else if ignorechars.contains(&byte) {
            continue;
        } else {
            return value_error(&format!("Non-Ascii85 digit found: {}", byte as char));
        }
    }
    let padding = 4 - current.len();
    if padding != 0 {
        let keep = decoded.len().saturating_sub(padding);
        decoded.truncate(keep);
    }
    value::bytes_new(decoded)
}

/// `a85encode`'s character set: `!` through `u`.
const A85_ALPHABET: &[u8; 85] =
    b"!\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstu";

// -------------------------------------------------------------------------
// Module surface
// -------------------------------------------------------------------------

/// Module constants: the alphabets CPython's `Lib/base64.py` keeps at module
/// scope plus the line-size limits and `__all__`.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "_b32alphabet" => value::bytes_new(B32_ALPHABET.to_vec()),
        "_b32hexalphabet" => value::bytes_new(B32HEX_ALPHABET.to_vec()),
        "_b85alphabet" => value::bytes_new(B85_ALPHABET.to_vec()),
        "_z85alphabet" => value::bytes_new(Z85_ALPHABET.to_vec()),
        "_A85START" => value::bytes_new(A85_START.to_vec()),
        "_A85END" => value::bytes_new(A85_END.to_vec()),
        "MAXLINESIZE" => crate::ops::int_result(MAXLINESIZE as i64),
        "MAXBINSIZE" => crate::ops::int_result(MAXBINSIZE as i64),
        "__all__" => {
            let names = value::list_new(Vec::with_capacity(BASE64_ALL.len()));
            for &name in BASE64_ALL {
                let item = value::str_alloc(name.to_string());
                crate::container::list_append(names, item);
            }
            names
        }
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'base64' has no attribute '{name}'"),
        ),
    }
}

/// Dispatches `module.function(...)` calls for `base64`.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "b64encode" | "standard_b64encode" | "encodestring" => b64encode(args),
        "b64decode" | "standard_b64decode" | "decodestring" => b64decode(args),
        "urlsafe_b64encode" => urlsafe_b64encode(args),
        "urlsafe_b64decode" => urlsafe_b64decode(args),
        "encodebytes" => encodebytes(args),
        "decodebytes" => decodebytes(args),
        "encode" => encode_file(args),
        "decode" => decode_file(args),
        "b16encode" => b16encode(args),
        "b16decode" => b16decode(args),
        "b32encode" => b32encode(args),
        "b32decode" => b32decode(args),
        "b32hexencode" => b32hexencode(args),
        "b32hexdecode" => b32hexdecode(args),
        "b85encode" => b85encode(args),
        "b85decode" => b85decode(args),
        "a85encode" => a85encode(args),
        "a85decode" => a85decode(args),
        "z85encode" => z85encode(args),
        "z85decode" => z85decode(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'base64' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(data: &[u8]) -> i64 {
        value::bytes_new(data.to_vec())
    }

    fn output(tagged: i64) -> &'static [u8] {
        value::as_bytes(tagged).expect("bytes result")
    }

    fn fails(_tagged: i64) -> bool {
        let pending = exc::pending();
        if pending {
            assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
            exc::clear();
        }
        pending
    }

    #[test]
    fn base64_round_trips() {
        exc::init();
        assert_eq!(output(call("b64encode", &[bytes(b"hello")])), b"aGVsbG8=");
        assert_eq!(output(call("b64decode", &[bytes(b"aGVsbG8=")])), b"hello");
        assert_eq!(output(call("b64encode", &[bytes(b"")])), b"");
        assert_eq!(output(call("b64decode", &[bytes(b"")])), b"");
        assert_eq!(
            output(call("standard_b64encode", &[bytes(b"tea!")])),
            b"dGVhIQ=="
        );
        assert_eq!(
            output(call("standard_b64decode", &[bytes(b"dGVhIQ==")])),
            b"tea!"
        );
        assert_eq!(
            output(call("encodestring", &[bytes(b"hello")])),
            b"aGVsbG8="
        );
        assert_eq!(
            output(call("decodestring", &[bytes(b"aGVsbG8=")])),
            b"hello"
        );
        // Decoders accept ASCII strings, like `_bytes_from_decode_data`.
        let text = value::str_alloc("aGVsbG8=".to_string());
        assert_eq!(output(call("b64decode", &[text])), b"hello");
        assert!(!exc::pending());
    }

    #[test]
    fn alternative_alphabets() {
        exc::init();
        assert_eq!(
            output(call("b64encode", &[bytes(b"\xfb\xff"), bytes(b"-_")])),
            b"-_8="
        );
        assert_eq!(
            output(call("b64decode", &[bytes(b"-_8="), bytes(b"-_")])),
            b"\xfb\xff"
        );
        // Without `altchars`, `-`/`_` are not part of the standard alphabet.
        assert!(fails(call("b64decode", &[bytes(b"-_8=")])));
        // `+` and `/` remain valid alongside the alternatives.
        assert_eq!(
            output(call("b64decode", &[bytes(b"+/8="), bytes(b"-_")])),
            b"\xfb\xff"
        );
        // `altchars` may be an ASCII string when decoding.
        let altchars = value::str_alloc("-_".to_string());
        assert_eq!(
            output(call("b64decode", &[bytes(b"-_8="), altchars])),
            b"\xfb\xff"
        );
        assert_eq!(
            output(call("urlsafe_b64encode", &[bytes(b"\xfb\xff")])),
            b"-_8="
        );
        assert_eq!(
            output(call("urlsafe_b64decode", &[bytes(b"-_8=")])),
            b"\xfb\xff"
        );
        assert!(!exc::pending());
    }

    #[test]
    fn decode_validation_and_padding() {
        exc::init();
        assert_eq!(output(call("b64decode", &[bytes(b"aG Vs bG8=")])), b"hello");
        assert!(fails(call(
            "b64decode",
            &[bytes(b"aG Vs bG8="), NONE, value::TRUE]
        )));
        assert!(fails(call("b64decode", &[bytes(b"YQ")])));
        assert!(fails(call("b64decode", &[bytes(b"YQ==AAAA")])));
        assert!(fails(call(
            "b64decode",
            &[bytes(b"YQ==="), NONE, value::TRUE]
        )));
        // Non-alphabet characters are discarded unless `validate` is set.
        assert_eq!(output(call("b64decode", &[bytes(b"YW]3=")])), b"am");
        assert!(fails(call(
            "b64decode",
            &[bytes(b"YW]3="), NONE, value::TRUE]
        )));
        assert!(!exc::pending());
    }

    #[test]
    fn multi_line_helpers() {
        exc::init();
        let encoded = call("encodebytes", &[bytes(&[b'a'; 60])]);
        // 57 bytes per line -> 76 characters, then the 3-byte remainder.
        let mut expected = b"YWFh".repeat(19);
        expected.push(b'\n');
        expected.extend_from_slice(b"YWFh\n");
        assert_eq!(output(encoded), expected.as_slice());
        assert_eq!(output(call("decodebytes", &[encoded])), &[b'a'; 60]);
        assert_eq!(output(call("encodebytes", &[bytes(b"")])), b"");
        assert_eq!(
            output(call("decodebytes", &[bytes(b"aGVsbG8Kd29ybGQK\n")])),
            b"hello\nworld\n"
        );
        // `encodebytes` rejects text input.
        let text = value::str_alloc("abc".to_string());
        call("encodebytes", &[text]);
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        assert!(!exc::pending());
    }

    #[test]
    fn base16_codec() {
        exc::init();
        assert_eq!(output(call("b16encode", &[bytes(b"tea")])), b"746561");
        assert_eq!(output(call("b16decode", &[bytes(b"746561")])), b"tea");
        assert_eq!(
            output(call("b16decode", &[bytes(b"ABCDEF")])),
            &[0xabu8, 0xcd, 0xef]
        );
        assert_eq!(output(call("b16decode", &[bytes(b"")])), b"");
        assert!(fails(call("b16decode", &[bytes(b"abcdef")])));
        assert_eq!(
            output(call("b16decode", &[bytes(b"abcdef"), value::TRUE])),
            &[0xabu8, 0xcd, 0xef]
        );
        assert!(fails(call("b16decode", &[bytes(b"74656")])));
        assert!(!exc::pending());
    }

    #[test]
    fn base32_codec() {
        exc::init();
        assert_eq!(output(call("b32encode", &[bytes(b"tea")])), b"ORSWC===");
        assert_eq!(output(call("b32decode", &[bytes(b"ORSWC===")])), b"tea");
        assert_eq!(output(call("b32encode", &[bytes(b"")])), b"");
        assert_eq!(output(call("b32decode", &[bytes(b"")])), b"");
        assert_eq!(output(call("b32encode", &[bytes(b"f")])), b"MY======");
        assert_eq!(output(call("b32encode", &[bytes(b"fo")])), b"MZXQ====");
        assert_eq!(output(call("b32encode", &[bytes(b"foo")])), b"MZXW6===");
        assert_eq!(
            output(call("b32encode", &[bytes(b"foobar")])),
            b"MZXW6YTBOI======"
        );
        assert_eq!(
            output(call("b32decode", &[bytes(b"MZXW6YTBOI======")])),
            b"foobar"
        );
        assert!(fails(call("b32decode", &[bytes(b"mzxw6===")])));
        assert_eq!(
            output(call("b32decode", &[bytes(b"mzxw6==="), value::TRUE])),
            b"foo"
        );
        assert_eq!(
            output(call("b32decode", &[bytes(b"01======"), NONE, bytes(b"L")])),
            b"r"
        );
        assert!(fails(call("b32decode", &[bytes(b"MZXW6")])));
        assert!(fails(call("b32decode", &[bytes(b"MZXW6==?")])));
        assert!(!exc::pending());
    }

    #[test]
    fn base32hex_codec() {
        exc::init();
        assert_eq!(output(call("b32hexencode", &[bytes(b"")])), b"");
        assert_eq!(output(call("b32hexencode", &[bytes(b"\x00")])), b"00======");
        assert_eq!(
            output(call("b32hexencode", &[bytes(b"abcde")])),
            b"C5H66P35"
        );
        assert_eq!(
            output(call("b32hexencode", &[bytes(b"foobar")])),
            b"CPNMUOJ1E8======"
        );
        assert_eq!(
            output(call("b32hexdecode", &[bytes(b"CPNMUOJ1E8======")])),
            b"foobar"
        );
        // Lowercase requires `casefold`.
        assert!(fails(call("b32hexdecode", &[bytes(b"c5h66p35")])));
        assert_eq!(
            output(call("b32hexdecode", &[bytes(b"c5h66p35"), value::TRUE])),
            b"abcde"
        );
        assert!(fails(call("b32hexdecode", &[bytes(b"c5h66p3=")])));
        assert!(!exc::pending());
    }

    #[test]
    fn base85_codec() {
        exc::init();
        assert_eq!(
            output(call("b85encode", &[bytes(b"www.python.org")])),
            b"cXxL#aCvlSZ*DGca%T"
        );
        assert_eq!(
            output(call("b85decode", &[bytes(b"cXxL#aCvlSZ*DGca%T")])),
            b"www.python.org"
        );
        assert_eq!(
            output(call("b85encode", &[bytes(b"x"), value::TRUE])),
            b"cmMzZ"
        );
        assert_eq!(output(call("b85decode", &[bytes(b"cmMzZ")])), b"x\0\0\0");
        assert_eq!(
            output(call("b85encode", &[bytes(b"zero compression\0\0\0\0")])),
            b"dS!BNAY*TBaB^jHb7^mG00000"
        );
        // Illegal characters and overflowing hunks are `ValueError`s.
        assert!(fails(call("b85decode", &[bytes(b"0000\"")])));
        assert!(fails(call("b85decode", &[bytes(b"|")])));
        assert!(!exc::pending());
    }

    #[test]
    fn ascii85_codec() {
        exc::init();
        assert_eq!(
            output(call("a85encode", &[bytes(b"www.python.org")])),
            b"GB\\6`E-ZP=Df.1GEb>"
        );
        assert_eq!(
            output(call("a85decode", &[bytes(b"GB\\6`E-ZP=Df.1GEb>")])),
            b"www.python.org"
        );
        // Adobe framing and wrapping.
        assert_eq!(
            output(call(
                "a85encode",
                &[
                    bytes(b"www.python.org"),
                    NONE,
                    value::FALSE,
                    value::FALSE,
                    value::TRUE
                ]
            )),
            b"<~GB\\6`E-ZP=Df.1GEb>~>"
        );
        assert_eq!(
            output(call(
                "a85decode",
                &[bytes(b"<~GB\\6`E-ZP=Df.1GEb>~>"), NONE, value::TRUE]
            )),
            b"www.python.org"
        );
        // Space folding and zero compression.
        assert_eq!(
            output(call("a85encode", &[bytes(b" "), NONE, NONE, NONE, NONE])),
            b"+9"
        );
        assert_eq!(
            output(call("a85decode", &[bytes(b"aaaaay"), value::TRUE])),
            b"\xc9\x80\x0b@    "
        );
        assert_eq!(output(call("a85decode", &[bytes(b"z")])), b"\0\0\0\0");
        assert!(fails(call("a85decode", &[bytes(b"abcx")])));
        assert!(fails(call("a85decode", &[bytes(b"s")])));
        assert!(!exc::pending());
    }

    #[test]
    fn z85_codec() {
        exc::init();
        assert_eq!(
            output(call("z85encode", &[bytes(b"www.python.org")])),
            b"CxXl-AcVLsz/dgCA+t"
        );
        assert_eq!(
            output(call("z85decode", &[bytes(b"CxXl-AcVLsz/dgCA+t")])),
            b"www.python.org"
        );
        assert_eq!(output(call("z85encode", &[bytes(b"\xff")])), b"@@");
        assert_eq!(
            output(call("z85decode", &[bytes(b"%nSc0")])),
            b"\xff\xff\xff\xff"
        );
        // ZeroMQ-only characters are not part of the alphabet.
        assert!(fails(call("z85decode", &[bytes(b"0000;")])));
        assert!(!exc::pending());
    }

    #[test]
    fn constants_match_cpython() {
        exc::init();
        assert_eq!(
            output(constant("_b32alphabet")),
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
        );
        assert_eq!(
            output(constant("_b32hexalphabet")),
            b"0123456789ABCDEFGHIJKLMNOPQRSTUV"
        );
        assert_eq!(output(constant("_A85START")), b"<~");
        assert_eq!(output(constant("_A85END")), b"~>");
        assert_eq!(
            value::as_integer(constant("MAXLINESIZE")),
            Some(MAXLINESIZE as i64)
        );
        assert_eq!(
            value::as_integer(constant("MAXBINSIZE")),
            Some(MAXBINSIZE as i64)
        );
        assert!(!exc::pending());
    }

    #[test]
    fn binary_payloads_round_trip() {
        exc::init();
        let payload: Vec<u8> = (0..=255).collect();
        let encoded = call("b64encode", &[bytes(&payload)]);
        assert_eq!(output(call("b64decode", &[encoded])), payload.as_slice());
        let encoded = call("b32encode", &[bytes(&payload)]);
        assert_eq!(output(call("b32decode", &[encoded])), payload.as_slice());
        let encoded = call("b32hexencode", &[bytes(&payload)]);
        assert_eq!(output(call("b32hexdecode", &[encoded])), payload.as_slice());
        let encoded = call("b16encode", &[bytes(&payload)]);
        assert_eq!(output(call("b16decode", &[encoded])), payload.as_slice());
        let encoded = call("b85encode", &[bytes(&payload)]);
        assert_eq!(output(call("b85decode", &[encoded])), payload.as_slice());
        let encoded = call("a85encode", &[bytes(&payload)]);
        assert_eq!(output(call("a85decode", &[encoded])), payload.as_slice());
        let encoded = call("z85encode", &[bytes(&payload)]);
        assert_eq!(output(call("z85decode", &[encoded])), payload.as_slice());
        assert!(!exc::pending());
    }

    #[test]
    fn unknown_function_is_an_attribute_error() {
        exc::init();
        call("not_a_codec", &[]);
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        exc::clear();
    }
}
