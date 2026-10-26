//! `binascii`: conversions between binary data and ASCII encodings.
//!
//! The encodings follow CPython (3.14) `binascii`/`Modules/binascii.c`:
//! `hexlify` inserts an optional separator with right-anchored grouping for a
//! positive `bytes_per_sep` and left-anchored grouping for a negative value,
//! `unhexlify` rejects odd-length and non-hex input, and `a2b_base64` follows
//! the `strict_mode` state machine of the C module (including its padding
//! rules). The legacy `adler32` checksum, dropped from CPython 3.14, is kept
//! because `zlib.adler32` remains part of the compatibility target. The
//! module-level base64 helpers are shared with the `base64` module.
//!
//! All functions accept `bytes` only and no input can panic; every failure
//! reports a pending `ValueError`/`TypeError` and returns `None`.

use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value::{self, NONE};

/// Lowercase hex digits used by `hexlify`/`b2a_hex`.
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// The RFC 4648 standard base64 alphabet.
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

const BASE64_PAD: u8 = b'=';

/// Table for the reflected IEEE 802.3 polynomial used by `crc32`.
const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut crc = n as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[n] = crc;
        n += 1;
    }
    table
}

static CRC32_TABLE: [u32; 256] = crc32_table();

/// Reads a positional or keyword slot. Omitted optional slots arrive as
/// `None` (the frontend fills keyword gaps with `None`).
pub(crate) fn argument(args: &[i64], index: usize) -> i64 {
    args.get(index).copied().unwrap_or(NONE)
}

pub(crate) fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

/// Copies a `bytes` argument, so the buffer stays valid across allocations.
pub(crate) fn bytes_argument(value_arg: i64, name: &str) -> Result<Vec<u8>, i64> {
    match value::as_bytes(value_arg) {
        Some(bytes) => Ok(bytes.to_vec()),
        None => Err(type_error(&format!(
            "argument '{name}' must be a bytes-like object, not '{}'",
            value::type_name(value_arg)
        ))),
    }
}

/// Interprets a boolean flag slot: `None` selects the documented default.
pub(crate) fn flag_argument(value_arg: i64, default: bool) -> bool {
    if value_arg == NONE {
        default
    } else {
        value::truthy(value_arg)
    }
}

/// Integer slot; `None` selects `default`.
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

/// Lowercase hex encoding without separators.
pub(crate) fn hex_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() * 2);
    for &byte in data {
        out.push(HEX_DIGITS[(byte >> 4) as usize]);
        out.push(HEX_DIGITS[(byte & 0x0f) as usize]);
    }
    out
}

/// Hex encoding with `sep` inserted every `group` input bytes. Positive
/// groups are counted from the right (the first group may be short), negative
/// groups from the left, matching `_Py_strhex_bytes_with_sep`.
fn hex_encode_with_sep(data: &[u8], separator: Option<u8>, group: i64) -> Vec<u8> {
    let Some(separator) = separator else {
        return hex_encode(data);
    };
    if group == 0 {
        return hex_encode(data);
    }
    let mut out = Vec::with_capacity(data.len() * 2 + data.len() / 2);
    let len = data.len();
    for (index, &byte) in data.iter().enumerate() {
        out.push(HEX_DIGITS[(byte >> 4) as usize]);
        out.push(HEX_DIGITS[(byte & 0x0f) as usize]);
        if index + 1 == len {
            break;
        }
        let boundary = if group > 0 {
            // Right-anchored: the separator sits before each full group
            // counted from the end.
            let remaining = (len - 1 - index) as u64;
            remaining.is_multiple_of(group as u64)
        } else {
            // Left-anchored: every `-group` bytes.
            (index as u64 + 1).is_multiple_of(group.unsigned_abs())
        };
        if boundary {
            out.push(separator);
        }
    }
    out
}

/// Decodes hex digits (either case). Odd lengths are rejected before any
/// character is inspected, as in CPython.
pub(crate) fn hex_decode(data: &[u8]) -> Result<Vec<u8>, &'static str> {
    if !data.len().is_multiple_of(2) {
        return Err("Odd-length string");
    }
    let mut out = Vec::with_capacity(data.len() / 2);
    for pair in data.as_chunks::<2>().0 {
        let high = hex_digit(pair[0]).ok_or("Non-hexadecimal digit found")?;
        let low = hex_digit(pair[1]).ok_or("Non-hexadecimal digit found")?;
        out.push((high << 4) | low);
    }
    Ok(out)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Standard base64 encoding with `=` padding and no newline.
pub(crate) fn base64_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied();
        let third = chunk.get(2).copied();
        out.push(BASE64_ALPHABET[(first >> 2) as usize]);
        out.push(BASE64_ALPHABET[(((first & 0x03) << 4) | (second.unwrap_or(0) >> 4)) as usize]);
        match second {
            Some(second) => out.push(
                BASE64_ALPHABET[(((second & 0x0f) << 2) | (third.unwrap_or(0) >> 6)) as usize],
            ),
            None => out.push(BASE64_PAD),
        }
        match third {
            Some(third) => out.push(BASE64_ALPHABET[(third & 0x3f) as usize]),
            None => out.push(BASE64_PAD),
        }
    }
    out
}

/// Value of one standard-alphabet base64 character.
fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decodes standard base64. In `strict` mode only alphabet characters and
/// well-formed padding are accepted; otherwise non-alphabet bytes (including
/// whitespace) are ignored and excess padding is tolerated. Both modes
/// enforce CPython's padding rules, so an unpadded quantum is an error.
pub(crate) fn base64_decode(data: &[u8], strict: bool) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(data.len().div_ceil(4) * 3);
    let mut quad_pos = 0usize;
    let mut leftchar = 0u8;
    let mut pads = 0usize;
    for (index, &byte) in data.iter().enumerate() {
        if byte == BASE64_PAD {
            pads += 1;
            if quad_pos >= 2 && quad_pos + pads <= 4 {
                continue;
            }
            if !strict {
                continue;
            }
            if quad_pos == 1 {
                break;
            }
            return Err(if quad_pos == 0 && index == 0 {
                "Leading padding not allowed".to_string()
            } else {
                "Excess padding not allowed".to_string()
            });
        }
        let Some(digit) = base64_value(byte) else {
            if strict {
                return Err("Only base64 data is allowed".to_string());
            }
            continue;
        };
        if pads > 0 && strict {
            return Err(if quad_pos + pads == 4 {
                "Excess data after padding".to_string()
            } else {
                "Discontinuous padding not allowed".to_string()
            });
        }
        pads = 0;
        match quad_pos {
            0 => {
                quad_pos = 1;
                leftchar = digit;
            }
            1 => {
                quad_pos = 2;
                out.push((leftchar << 2) | (digit >> 4));
                leftchar = digit & 0x0f;
            }
            2 => {
                quad_pos = 3;
                out.push((leftchar << 4) | (digit >> 2));
                leftchar = digit & 0x03;
            }
            _ => {
                quad_pos = 0;
                out.push((leftchar << 6) | digit);
                leftchar = 0;
            }
        }
    }
    if quad_pos == 1 {
        let count = out.len() / 3 * 4 + 1;
        return Err(format!(
            "Invalid base64-encoded string: number of data characters ({count}) cannot be 1 \
             more than a multiple of 4"
        ));
    }
    if quad_pos != 0 && quad_pos + pads < 4 {
        return Err("Incorrect padding".to_string());
    }
    Ok(out)
}

/// A single-byte separator: `bytes` or a one-character `str`.
fn separator(value_arg: i64) -> Result<Option<u8>, i64> {
    if value_arg == NONE {
        return Ok(None);
    }
    if let Some(bytes) = value::as_bytes(value_arg) {
        return match bytes {
            [single] => Ok(Some(*single)),
            _ => Err(value_error("sep must be length 1.")),
        };
    }
    if let Some(text) = value::as_str(value_arg) {
        let bytes = text.as_bytes();
        return match bytes {
            [single] => Ok(Some(*single)),
            _ => Err(value_error("sep must be length 1.")),
        };
    }
    Err(type_error(&format!(
        "object of type '{}' has no len()",
        value::type_name(value_arg)
    )))
}

fn int_result(raw: i64) -> i64 {
    value::encode_int(raw).unwrap_or(NONE)
}

/// `hexlify(data, sep=None, bytes_per_sep=1)`; `b2a_hex` is an alias.
fn hexlify(args: &[i64]) -> i64 {
    let data = match bytes_argument(argument(args, 0), "data") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let separator = match separator(argument(args, 1)) {
        Ok(separator) => separator,
        Err(error) => return error,
    };
    let group = match int_argument(argument(args, 2), "bytes_per_sep", 1) {
        Ok(group) => group,
        Err(error) => return error,
    };
    // No separator means no grouping, whatever `bytes_per_sep` says.
    let encoded = hex_encode_with_sep(
        &data,
        separator,
        if separator.is_some() { group } else { 0 },
    );
    value::bytes_new(encoded)
}

/// `unhexlify(hexstr)`; `a2b_hex` is an alias.
fn unhexlify(args: &[i64]) -> i64 {
    let data = match bytes_argument(argument(args, 0), "hexstr") {
        Ok(data) => data,
        Err(error) => return error,
    };
    match hex_decode(&data) {
        Ok(decoded) => value::bytes_new(decoded),
        Err(message) => value_error(message),
    }
}

/// `b2a_base64(data, newline=True)`.
fn b2a_base64(args: &[i64]) -> i64 {
    let data = match bytes_argument(argument(args, 0), "data") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let newline = flag_argument(argument(args, 1), true);
    let mut encoded = base64_encode(&data);
    if newline {
        encoded.push(b'\n');
    }
    value::bytes_new(encoded)
}

/// `a2b_base64(data, strict_mode=False)`.
fn a2b_base64(args: &[i64]) -> i64 {
    let data = match bytes_argument(argument(args, 0), "data") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let strict = flag_argument(argument(args, 1), false);
    match base64_decode(&data, strict) {
        Ok(decoded) => value::bytes_new(decoded),
        Err(message) => value_error(&message),
    }
}

/// `crc32(data, value=0)`: the initial CRC is the previous result.
fn crc32(args: &[i64]) -> i64 {
    let data = match bytes_argument(argument(args, 0), "data") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let initial = match int_argument(argument(args, 1), "value", 0) {
        Ok(initial) => initial,
        Err(error) => return error,
    };
    let mut crc = (initial as u64 & 0xFFFF_FFFF) as u32 ^ 0xFFFF_FFFF;
    for &byte in &data {
        let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = CRC32_TABLE[index] ^ (crc >> 8);
    }
    int_result(i64::from(crc ^ 0xFFFF_FFFF))
}

/// `adler32(data, value=1)`: `value` is a previous Adler-32 checksum.
fn adler32(args: &[i64]) -> i64 {
    const MODULUS: u32 = 65521;
    let data = match bytes_argument(argument(args, 0), "data") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let initial = match int_argument(argument(args, 1), "value", 1) {
        Ok(initial) => initial as u64 & 0xFFFF_FFFF,
        Err(error) => return error,
    };
    let mut low = (initial & 0xFFFF) as u32;
    let mut high = ((initial >> 16) & 0xFFFF) as u32;
    for &byte in &data {
        low = (low + u32::from(byte)) % MODULUS;
        high = (high + low) % MODULUS;
    }
    int_result(i64::from((high << 16) | low))
}

/// Dispatches `module.function(...)` calls for `binascii`.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "hexlify" | "b2a_hex" => hexlify(args),
        "unhexlify" | "a2b_hex" => unhexlify(args),
        "b2a_base64" => b2a_base64(args),
        "a2b_base64" => a2b_base64(args),
        "crc32" => crc32(args),
        "adler32" => adler32(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'binascii' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(data: &[u8]) -> i64 {
        value::bytes_new(data.to_vec())
    }

    fn int(n: i64) -> i64 {
        value::encode_int(n).expect("representable integer")
    }

    fn output(tagged: i64) -> &'static [u8] {
        value::as_bytes(tagged).expect("bytes result")
    }

    fn number(tagged: i64) -> i64 {
        value::as_integer(tagged).expect("integer result")
    }

    #[test]
    fn hexlify_matches_cpython() {
        exc::init();
        let data = bytes(b"tea");
        assert_eq!(output(call("hexlify", &[data])), b"746561");
        assert_eq!(output(call("b2a_hex", &[data])), b"746561");
        assert_eq!(output(call("hexlify", &[bytes(b"")])), b"");
        assert_eq!(output(call("hexlify", &[bytes(&[0x00, 0xff])])), b"00ff");
        assert!(!exc::pending());
    }

    #[test]
    fn hexlify_grouping() {
        exc::init();
        let abcd = bytes(b"abcd");
        let dash = bytes(b"-");
        // Default group size is one.
        assert_eq!(output(call("hexlify", &[abcd, dash])), b"61-62-63-64");
        // Positive groups are anchored on the right.
        assert_eq!(output(call("hexlify", &[abcd, dash, int(3)])), b"61-626364");
        assert_eq!(
            output(call("hexlify", &[bytes(&[0, 0, 0, 0]), dash, int(3)])),
            b"00-000000"
        );
        // Negative groups are anchored on the left.
        assert_eq!(
            output(call("hexlify", &[abcd, dash, int(-3)])),
            b"616263-64"
        );
        // Zero disables grouping, and `None` never groups.
        assert_eq!(output(call("hexlify", &[abcd, dash, int(0)])), b"61626364");
        assert_eq!(output(call("hexlify", &[abcd, NONE])), b"61626364");
        assert_eq!(output(call("hexlify", &[abcd, NONE, int(2)])), b"61626364");
        assert!(!exc::pending());
    }

    #[test]
    fn unhexlify_round_trips_and_rejects_bad_input() {
        exc::init();
        assert_eq!(output(call("unhexlify", &[bytes(b"746561")])), b"tea");
        assert_eq!(output(call("a2b_hex", &[bytes(b"746561")])), b"tea");
        assert_eq!(output(call("unhexlify", &[bytes(b"")])), b"");
        assert_eq!(
            output(call("unhexlify", &[bytes(b"0001020304ff")])),
            &[0u8, 1, 2, 3, 4, 0xff]
        );

        call("unhexlify", &[bytes(b"abc")]);
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();

        call("unhexlify", &[bytes(b"zz")]);
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn base64_line_encoding() {
        exc::init();
        assert_eq!(
            output(call("b2a_base64", &[bytes(b"hello")])),
            b"aGVsbG8=\n"
        );
        assert_eq!(
            output(call("b2a_base64", &[bytes(b"hello"), value::FALSE])),
            b"aGVsbG8="
        );
        assert_eq!(output(call("b2a_base64", &[bytes(b"")])), b"\n");
        assert_eq!(
            output(call("b2a_base64", &[bytes(b"ab"), value::FALSE])),
            b"YWI="
        );
        assert_eq!(
            output(call("b2a_base64", &[bytes(b"abc"), value::FALSE])),
            b"YWJj"
        );
        assert!(!exc::pending());
    }

    #[test]
    fn base64_decoding_is_lenient_unless_strict() {
        exc::init();
        assert_eq!(
            output(call("a2b_base64", &[bytes(b"aGVsbG8=\n")])),
            b"hello"
        );
        assert_eq!(output(call("a2b_base64", &[bytes(b"Y Q = = ")])), b"a");
        assert_eq!(output(call("a2b_base64", &[bytes(b"!!!!")])), b"");
        assert_eq!(output(call("a2b_base64", &[bytes(b"")])), b"");

        call("a2b_base64", &[bytes(b"YQ")]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();

        call("a2b_base64", &[bytes(b"Y Q = = "), value::TRUE]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();

        call("a2b_base64", &[bytes(b"YQ==AAAA"), value::TRUE]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn checksums_match_cpython() {
        exc::init();
        assert_eq!(number(call("crc32", &[bytes(b"")])), 0);
        assert_eq!(number(call("crc32", &[bytes(b"abc")])), 891_568_578);
        assert_eq!(number(call("crc32", &[bytes(b"tea")])), 2_391_201_714);
        assert_eq!(
            number(call("crc32", &[bytes(b"abc"), int(123)])),
            1_837_155_955
        );
        assert_eq!(
            number(call("crc32", &[bytes(b"abc"), int(-1)])),
            899_311_407
        );

        assert_eq!(number(call("adler32", &[bytes(b"")])), 1);
        assert_eq!(number(call("adler32", &[bytes(b"abc")])), 38_600_999);
        assert_eq!(number(call("adler32", &[bytes(b"tea")])), 42_598_715);
        assert_eq!(
            number(call("adler32", &[bytes(b"abc"), int(123)])),
            62_587_297
        );
        assert!(!exc::pending());
    }

    #[test]
    fn unknown_function_is_an_attribute_error() {
        exc::init();
        call("a2b_uu", &[]);
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        exc::clear();
    }
}
