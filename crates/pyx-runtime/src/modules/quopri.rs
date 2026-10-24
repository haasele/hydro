//! `quopri`: quoted-printable encoding and decoding.
//!
//! The byte-level behaviour mirrors CPython's `binascii.b2a_qp`/`a2b_qp`
//! as used by `Lib/quopri.py` (`istext=True`):
//!
//! - lines are limited to 76 columns and broken with a `=\n` soft break;
//! - `\n` (and `\r\n`) stay literal, but any space or tab directly before the
//!   line ending is encoded so transports cannot strip it;
//! - `quotetabs` encodes every space and tab, not just trailing ones;
//! - `header` encodes spaces as `_` and encodes a literal `_` in turn;
//! - a lone `.` at the start of a line is encoded (`=2E`).
//!
//! Decoding drops soft line breaks, expands `=XX`, keeps a `=` that does not
//! introduce a valid escape, and maps `_` back to a space in header mode.

use std::borrow::Cow;

use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::methods;
use crate::value::{self, NONE};

/// RFC 2045 line-length limit, matching `binascii.MAXLINESIZE`.
const MAX_LINE: usize = 76;

const HEX: &[u8; 16] = b"0123456789ABCDEF";

fn push_hex(out: &mut Vec<u8>, byte: u8) {
    out.push(HEX[usize::from(byte >> 4)]);
    out.push(HEX[usize::from(byte & 0x0f)]);
}

fn is_hex(byte: u8) -> bool {
    byte.is_ascii_hexdigit()
}

fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'A'..=b'F' => byte - b'A' + 10,
        _ => byte - b'a' + 10,
    }
}

/// `binascii.b2a_qp(data, quotetabs, istext=True, header)`.
///
/// CPython first sizes the output and then fills it; a single pass produces
/// the same bytes because the two passes make identical decisions.
fn b2a_qp(data: &[u8], quotetabs: bool, header: bool) -> Vec<u8> {
    // CPython normalises every line ending to the style of the first one.
    let crlf = data
        .iter()
        .position(|&byte| byte == b'\n')
        .is_some_and(|at| at > 0 && data[at - 1] == b'\r');
    let mut out = Vec::with_capacity(data.len() + data.len() / MAX_LINE * 3 + 3);
    let mut line_len = 0usize;
    let mut at = 0usize;
    while at < data.len() {
        let byte = data[at];
        let quote = byte > 126
            || byte == b'='
            || (header && byte == b'_')
            || (byte == b'.'
                && line_len == 0
                && (at + 1 == data.len()
                    || data[at + 1] == b'\n'
                    || data[at + 1] == b'\r'
                    || data[at + 1] == 0))
            || ((byte == b'\t' || byte == b' ') && at + 1 == data.len())
            || (byte < 33
                && byte != b'\r'
                && byte != b'\n'
                && (quotetabs || (byte != b'\t' && byte != b' ')));
        if quote {
            if line_len + 3 >= MAX_LINE {
                out.push(b'=');
                if crlf {
                    out.push(b'\r');
                }
                out.push(b'\n');
                line_len = 0;
            }
            out.push(b'=');
            push_hex(&mut out, byte);
            at += 1;
            line_len += 3;
        } else if byte == b'\n' || (at + 1 < data.len() && byte == b'\r' && data[at + 1] == b'\n') {
            line_len = 0;
            // RFC 1521: whitespace at the end of a line is always encoded.
            if let Some(&last) = out.last() {
                if last == b' ' || last == b'\t' {
                    out.pop();
                    out.push(b'=');
                    push_hex(&mut out, last);
                }
            }
            if crlf {
                out.push(b'\r');
            }
            out.push(b'\n');
            at += if byte == b'\r' { 2 } else { 1 };
        } else {
            if at + 1 != data.len() && data[at + 1] != b'\n' && line_len + 1 >= MAX_LINE {
                out.push(b'=');
                if crlf {
                    out.push(b'\r');
                }
                out.push(b'\n');
                line_len = 0;
            }
            line_len += 1;
            if header && byte == b' ' {
                out.push(b'_');
            } else {
                out.push(byte);
            }
            at += 1;
        }
    }
    out
}

/// `binascii.a2b_qp(data, header)`.
fn a2b_qp(data: &[u8], header: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut at = 0usize;
    while at < data.len() {
        if data[at] == b'=' {
            at += 1;
            if at >= data.len() {
                break;
            }
            let byte = data[at];
            if byte == b'\n' || byte == b'\r' {
                // Soft line break: skip through the end of the physical line.
                if byte != b'\n' {
                    while at < data.len() && data[at] != b'\n' {
                        at += 1;
                    }
                }
                if at < data.len() {
                    at += 1;
                }
            } else if byte == b'=' {
                // A broken `==` escape: CPython emits the second `=`.
                out.push(b'=');
                at += 1;
            } else if at + 1 < data.len() && is_hex(byte) && is_hex(data[at + 1]) {
                out.push((hex_value(byte) << 4) | hex_value(data[at + 1]));
                at += 2;
            } else {
                out.push(b'=');
            }
            continue;
        }
        if header && data[at] == b'_' {
            out.push(b' ');
        } else {
            out.push(data[at]);
        }
        at += 1;
    }
    out
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

/// Bytes-only argument, matching `b2a_qp`.
fn encoded_input(argument: i64) -> Result<&'static [u8], i64> {
    match value::as_bytes(argument) {
        Some(bytes) => Ok(bytes),
        None => Err(type_error(&format!(
            "a bytes-like object is required, not '{}'",
            value::type_name(argument)
        ))),
    }
}

/// `a2b_qp` also accepts ASCII `str` input.
fn decoded_input(argument: i64) -> Result<Cow<'static, [u8]>, i64> {
    if let Some(bytes) = value::as_bytes(argument) {
        return Ok(Cow::Borrowed(bytes));
    }
    if let Some(text) = value::as_str(argument) {
        if !text.is_ascii() {
            return Err(exc::raise_with_message(
                VALUE_ERROR,
                "string argument should contain only ASCII characters",
            ));
        }
        return Ok(Cow::Owned(text.as_bytes().to_vec()));
    }
    Err(type_error(&format!(
        "argument should be bytes, buffer or ASCII string, not '{}'",
        value::type_name(argument)
    )))
}

fn flag(argument: Option<&i64>) -> bool {
    argument.is_some_and(|&value| value::truthy(value))
}

/// `encodestring(data, quotetabs=False, header=False)`.
fn encodestring(args: &[i64]) -> i64 {
    let Some(&data) = args.first() else {
        return type_error("encodestring() missing required argument 'data'");
    };
    let data = match encoded_input(data) {
        Ok(data) => data,
        Err(error) => return error,
    };
    let quotetabs = flag(args.get(1));
    let header = flag(args.get(2));
    value::bytes_new(b2a_qp(data, quotetabs, header))
}

/// `decodestring(data, header=False)`.
fn decodestring(args: &[i64]) -> i64 {
    let Some(&data) = args.first() else {
        return type_error("decodestring() missing required argument 'data'");
    };
    let data = match decoded_input(data) {
        Ok(data) => data,
        Err(error) => return error,
    };
    let header = flag(args.get(1));
    value::bytes_new(a2b_qp(&data, header))
}

/// Calls `obj.<name>(args...)` through the regular method dispatcher.
fn invoke(obj: i64, name: &str, args: &[i64]) -> i64 {
    methods::call(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

/// Reads the whole input file object, returning its bytes.
fn read_all(input: i64) -> Result<Vec<u8>, i64> {
    let data = invoke(input, "read", &[]);
    if exc::pending() {
        return Err(data);
    }
    match value::as_bytes(data) {
        Some(bytes) => Ok(bytes.to_vec()),
        None => Err(type_error(&format!(
            "a bytes-like object is required, not '{}'",
            value::type_name(data)
        ))),
    }
}

fn write_all(output: i64, data: Vec<u8>) -> i64 {
    let written = value::bytes_new(data);
    invoke(output, "write", &[written]);
    NONE
}

/// `encode(input, output, quotetabs, header=False)`.
fn encode_file(args: &[i64]) -> i64 {
    let (Some(&input), Some(&output), Some(&quotetabs)) = (args.first(), args.get(1), args.get(2))
    else {
        return type_error("encode() missing required argument");
    };
    let data = match read_all(input) {
        Ok(data) => data,
        Err(error) => return error,
    };
    write_all(
        output,
        b2a_qp(&data, value::truthy(quotetabs), flag(args.get(3))),
    )
}

/// `decode(input, output, header=False)`.
fn decode_file(args: &[i64]) -> i64 {
    let (Some(&input), Some(&output)) = (args.first(), args.get(1)) else {
        return type_error("decode() missing required argument");
    };
    // Unlike encoding, `a2b_qp` also accepts an ASCII text stream.
    let data = invoke(input, "read", &[]);
    if exc::pending() {
        return NONE;
    }
    let data = match decoded_input(data) {
        Ok(data) => data.into_owned(),
        Err(error) => return error,
    };
    write_all(output, a2b_qp(&data, flag(args.get(2))))
}

/// Dispatches `quopri.function(...)` calls.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "encodestring" => encodestring(args),
        "decodestring" => decodestring(args),
        "encode" => encode_file(args),
        "decode" => decode_file(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'quopri' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(data: &[u8]) -> i64 {
        value::bytes_new(data.to_vec())
    }

    fn result(value: i64) -> Vec<u8> {
        value::as_bytes(value).expect("bytes result").to_vec()
    }

    /// Builds the expected byte string for an assertion.
    fn expect(data: &[u8]) -> Vec<u8> {
        data.to_vec()
    }

    #[test]
    fn simple_round_trip() {
        exc::init();
        let encoded = call("encodestring", &[bytes(b"hello world")]);
        assert!(!exc::pending());
        assert_eq!(result(encoded), expect(b"hello world"));
        assert_eq!(
            result(call("decodestring", &[encoded])),
            expect(b"hello world")
        );
        assert!(!exc::pending());
    }

    #[test]
    fn encodes_edge_characters() {
        exc::init();
        assert_eq!(result(call("encodestring", &[bytes(b"=")])), expect(b"=3D"));
        assert_eq!(
            result(call("encodestring", &[bytes(b"\xff\x00")])),
            expect(b"=FF=00")
        );
        // Trailing whitespace is always encoded, interior tabs are not.
        assert_eq!(
            result(call("encodestring", &[bytes(b"a\tb c \nnext")])),
            expect(b"a\tb c=20\nnext")
        );
        assert_eq!(
            result(call("encodestring", &[bytes(b" \n")])),
            expect(b"=20\n")
        );
        assert_eq!(
            result(call("encodestring", &[bytes(b"  \n")])),
            expect(b" =20\n")
        );
        assert!(!exc::pending());
    }

    #[test]
    fn quotetabs_encodes_all_whitespace() {
        exc::init();
        let encoded = call("encodestring", &[bytes(b"hello world"), value::TRUE]);
        assert_eq!(result(encoded), expect(b"hello=20world"));
        assert_eq!(
            result(call("encodestring", &[bytes(b"a\tb"), value::TRUE])),
            expect(b"a=09b")
        );
        assert!(!exc::pending());
    }

    #[test]
    fn header_mode_uses_underscores() {
        exc::init();
        let encode = |data: &[u8]| call("encodestring", &[bytes(data), value::FALSE, value::TRUE]);
        assert_eq!(result(encode(b"hello world")), expect(b"hello_world"));
        assert_eq!(result(encode(b"a_b")), expect(b"a=5Fb"));
        assert_eq!(result(encode(b"a b")), expect(b"a_b"));
        assert_eq!(
            result(call("decodestring", &[bytes(b"hello_world"), value::TRUE])),
            expect(b"hello world")
        );
        // Without header mode the underscore stays literal.
        assert_eq!(
            result(call("decodestring", &[bytes(b"a_b")])),
            expect(b"a_b")
        );
        assert!(!exc::pending());
    }

    #[test]
    fn soft_breaks_at_seventy_six_columns() {
        exc::init();
        let data = vec![b'a'; 80];
        let encoded = result(call("encodestring", &[bytes(&data)]));
        assert_eq!(encoded.len(), 82);
        assert_eq!(encoded.get(75..77), Some(&b"=\n"[..]));
        assert_eq!(result(call("decodestring", &[bytes(&encoded)])), data);

        // A first line ending in CRLF makes every soft break CRLF too.
        let mut source = b"a\r\n".to_vec();
        source.extend_from_slice(&[b'b'; 80]);
        let encoded_crlf = result(call("encodestring", &[bytes(&source)]));
        assert!(encoded_crlf
            .windows(3)
            .any(|window| window == &b"=\r\n"[..]));
        assert_eq!(
            result(call("decodestring", &[bytes(&encoded_crlf)])),
            source
        );
        assert!(!exc::pending());
    }

    #[test]
    fn decode_handles_soft_breaks_and_bad_escapes() {
        exc::init();
        assert_eq!(
            result(call("decodestring", &[bytes(b"a=\nb")])),
            expect(b"ab")
        );
        assert_eq!(
            result(call("decodestring", &[bytes(b"a=\r\nb")])),
            expect(b"ab")
        );
        assert_eq!(
            result(call("decodestring", &[bytes(b"a=3db")])),
            expect(b"a=b")
        );
        // A `=` without a valid two-digit escape is preserved.
        assert_eq!(
            result(call("decodestring", &[bytes(b"x=ZZ")])),
            expect(b"x=ZZ")
        );
        assert_eq!(result(call("decodestring", &[bytes(b"x=")])), expect(b"x"));
        assert_eq!(
            result(call("decodestring", &[bytes(b"a==b")])),
            expect(b"a=b")
        );
        assert!(!exc::pending());
    }

    #[test]
    fn string_inputs_and_type_errors() {
        exc::init();
        let text = value::str_alloc("abc=20def".to_string());
        assert_eq!(result(call("decodestring", &[text])), expect(b"abc def"));
        assert!(!exc::pending());

        // Encoding requires bytes.
        assert_eq!(call("encodestring", &[text]), NONE);
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        // Non-ASCII strings cannot be decoded.
        let non_ascii = value::str_alloc("café".to_string());
        assert_eq!(call("decodestring", &[non_ascii]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();

        let integer = value::encode_int(3).expect("representable");
        assert_eq!(call("encodestring", &[integer]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        assert_eq!(call("unknown", &[]), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn file_pair_requires_a_readable_input() {
        exc::init();
        // A plain bytes object has no `read` method, so the dispatcher raises.
        assert_eq!(
            call("encode", &[bytes(b"x"), bytes(b""), value::FALSE]),
            NONE
        );
        assert!(exc::pending());
        exc::clear();
        assert_eq!(call("decode", &[bytes(b"x"), bytes(b"")]), NONE);
        assert!(exc::pending());
        exc::clear();
    }
}
