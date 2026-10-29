//! `base64`: RFC 4648 Base16/Base32/Base64 codecs.
//!
//! The implementation mirrors CPython 3.14's `Lib/base64.py` on top of the
//! native `binascii` core: Base64 accepts an alternative alphabet via
//! `altchars`, `b64decode(validate=True)` enables the strict `a2b_base64`
//! checks, `encodebytes` wraps at 76 columns with a trailing newline per
//! chunk, and Base32 follows the Python-level padding algorithm (including
//! `casefold` and the RFC 4648 §2.4 `map01` mapping). Inputs and results are
//! `bytes`; every failure raises `ValueError` and no input can panic.

use super::binascii::{self, argument, bytes_argument, flag_argument};
use crate::exc::{self, ATTRIBUTE_ERROR, VALUE_ERROR};
use crate::value::{self, NONE};

/// Uppercase Base32 alphabet.
const B32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Bytes per Base64 output line in `encodebytes` (76 columns / 4 * 3).
const MAXBINSIZE: usize = 57;

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

/// Reads an optional two-byte alternative alphabet.
fn altchars(value_arg: i64) -> Result<Option<[u8; 2]>, i64> {
    if value_arg == NONE {
        return Ok(None);
    }
    let bytes = bytes_argument(value_arg, "altchars")?;
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

/// `urlsafe_b64encode(s)`.
fn urlsafe_b64encode(args: &[i64]) -> i64 {
    let data = match bytes_argument(argument(args, 0), "s") {
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
    let data = match bytes_argument(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let alternative = match altchars(argument(args, 1)) {
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
    let mut data = match bytes_argument(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let alternative = match altchars(argument(args, 1)) {
        Ok(alternative) => alternative,
        Err(error) => return error,
    };
    let validate = flag_argument(argument(args, 2), false);
    if let Some(alternative) = alternative {
        translate_altchars(&mut data, alternative, false);
    }
    match binascii::base64_decode(&data, validate) {
        Ok(decoded) => value::bytes_new(decoded),
        Err(message) => value_error(&message),
    }
}

/// `urlsafe_b64decode(s)`: `-`/`_` are translated before decoding.
fn urlsafe_b64decode(args: &[i64]) -> i64 {
    let mut data = match bytes_argument(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    // CPython 3.15 adds `padded`; pydantic uses `padded=True` in a version
    // branch that is unreachable on the 3.14 target, but the call still has
    // to type-check. Accepting the keyword here keeps that forward-compatible
    // call valid and restores padding only when requested.
    if flag_argument(argument(args, 1), false) {
        while data.len() % 4 != 0 {
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
        Err(message) => value_error(&message),
    }
}

/// `encodebytes(s)`: 57-byte chunks, each encoded as a 76-column line.
fn encodebytes(args: &[i64]) -> i64 {
    let data = match bytes_argument(argument(args, 0), "s") {
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

/// `b16encode(s)`: uppercase hexadecimal.
fn b16encode(args: &[i64]) -> i64 {
    let data = match bytes_argument(argument(args, 0), "s") {
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
    let mut data = match bytes_argument(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    if flag_argument(argument(args, 1), false) {
        data.make_ascii_uppercase();
    }
    if data.iter().any(|byte| !is_base16(*byte)) {
        return value_error("Non-base16 digit found");
    }
    match binascii::hex_decode(&data) {
        Ok(decoded) => value::bytes_new(decoded),
        Err(message) => value_error(message),
    }
}

fn is_base16(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'A'..=b'F')
}

/// `b32encode(s)`.
fn b32encode(args: &[i64]) -> i64 {
    let mut data = match bytes_argument(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
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
            encoded.push(B32_ALPHABET[((acc >> shift) & 0x1f) as usize]);
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
    value::bytes_new(encoded)
}

/// Value of one uppercase Base32 character.
fn b32_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'2'..=b'7' => Some(byte - b'2' + 26),
        _ => None,
    }
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

/// `b32decode(s, casefold=False, map01=None)`.
fn b32decode(args: &[i64]) -> i64 {
    let mut data = match bytes_argument(argument(args, 0), "s") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let casefold = flag_argument(argument(args, 1), false);
    let map01 = argument(args, 2);
    if !data.len().is_multiple_of(8) {
        return value_error("Incorrect padding");
    }
    if map01 != NONE {
        let mapping = match bytes_argument(map01, "map01") {
            Ok(mapping) => mapping,
            Err(error) => return error,
        };
        let [replacement] = mapping.as_slice() else {
            return value_error("map01 must be a bytes-like object of length 1");
        };
        for byte in data.iter_mut() {
            match *byte {
                b'0' => *byte = b'O',
                b'1' => *byte = *replacement,
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
            let Some(digit) = b32_value(byte) else {
                return value_error("Non-base32 digit found");
            };
            acc = (acc << 5) + u64::from(digit);
        }
        decoded.extend_from_slice(&to_be5(acc));
    }
    if !matches!(padchars, 0 | 1 | 3 | 4 | 6) {
        return value_error("Incorrect padding");
    }
    if padchars != 0 && !decoded.is_empty() {
        acc <<= 5 * padchars as u32;
        let last = to_be5(acc);
        // 6 pads -> 1 byte, 4 -> 2, 3 -> 3, 1 -> 4.
        let leftover = (43 - 5 * padchars) / 8;
        decoded.truncate(decoded.len() - 5);
        decoded.extend_from_slice(&last[..leftover]);
    }
    value::bytes_new(decoded)
}

/// Dispatches `module.function(...)` calls for `base64`.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "b64encode" | "standard_b64encode" | "encodestring" => b64encode(args),
        "b64decode" | "standard_b64decode" | "decodestring" => b64decode(args),
        "urlsafe_b64encode" => urlsafe_b64encode(args),
        "urlsafe_b64decode" => urlsafe_b64decode(args),
        "encodebytes" => encodebytes(args),
        "decodebytes" => b64decode(args),
        "b16encode" => b16encode(args),
        "b16decode" => b16decode(args),
        "b32encode" => b32encode(args),
        "b32decode" => b32decode(args),
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
    fn binary_payloads_round_trip() {
        exc::init();
        let payload: Vec<u8> = (0..=255).collect();
        let encoded = call("b64encode", &[bytes(&payload)]);
        assert_eq!(output(call("b64decode", &[encoded])), payload.as_slice());
        let encoded = call("b32encode", &[bytes(&payload)]);
        assert_eq!(output(call("b32decode", &[encoded])), payload.as_slice());
        let encoded = call("b16encode", &[bytes(&payload)]);
        assert_eq!(output(call("b16decode", &[encoded])), payload.as_slice());
        assert!(!exc::pending());
    }

    #[test]
    fn unknown_function_is_an_attribute_error() {
        exc::init();
        call("a85encode", &[]);
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        exc::clear();
    }
}
