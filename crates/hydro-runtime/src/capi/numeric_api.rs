//! Numeric conversion, `int()` string parsing, double formatting and
//! `memoryview`-from-memory entry points for the CPython 3.14 host surface.
//!
//! These are the numeric pieces of the limited API that extension modules call
//! directly rather than through `PyArg_ParseTuple`: the `PyLong` accessors
//! (`PyLong_AsDouble`, `PyLong_AsInt`, `PyLong_AsUnsignedLongLongMask`), the
//! constructors from `size_t` and from text (`PyLong_FromSize_t`,
//! `PyLong_FromString`), the internal copy hook (`_PyLong_Copy`), the shortest
//! double formatter (`PyOS_double_to_string`) and the zero-copy memoryview
//! constructor (`PyMemoryView_FromMemory`).
//!
//! Semantics follow CPython 3.14, including the error text for the supported
//! inputs:
//!
//! - `PyLong_FromString` parses bases 0 and 2..36 with CPython's prefix,
//!   underscore and trailing-whitespace rules, reports the first unconsumed
//!   byte through `pend`, and raises `ValueError` with the `invalid literal for
//!   int() with base N: '...'` message (the base is rewritten to 0 when the
//!   failure comes from a base-0 literal that is not zero). The offending
//!   literal is decoded lossily for that message, so a byte that is not valid
//!   UTF-8 appears as U+FFFD where CPython's decoder would fail the format
//!   with `UnicodeDecodeError` first;
//! - `PyLong_AsInt` distinguishes "not an integer" (`TypeError`) from the C
//!   `int` range (`OverflowError: Python int too large to convert to C int`);
//! - `PyOS_double_to_string` implements the `r`, `e`, `f` and `g` format codes
//!   (either case), the sign/add-dot-zero/alternate/no-negative-zero flags, the
//!   `Py_DTST_*` result tag and the `PyMem_Malloc` ownership contract: the
//!   caller releases the returned buffer with `PyMem_Free`;
//! - `PyMemoryView_FromMemory` produces the same view CPython's 3.14
//!   implementation builds (one-dimensional, `B` format, writable only for
//!   exactly `PyBUF_WRITE`) without taking ownership of the memory.
//!
//! Integers are 64-bit in this host (see `capi`'s module documentation), so a
//! value that would need a wider `PyLong` raises `OverflowError` instead of
//! being truncated. No entry point panics: failures set a Python error and
//! return the documented sentinel.

#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::Arc;

use super::*;

/// `Py_DTSF_SIGN`: prefix non-negative results with `+`.
const PY_DTSF_SIGN: c_int = 0x01;
/// `Py_DTSF_ADD_DOT_0`: add `.0` to integral results in non-exponential form.
const PY_DTSF_ADD_DOT_0: c_int = 0x02;
/// `Py_DTSF_ALT`: alternate formatting (`#` in `printf` terms).
const PY_DTSF_ALT: c_int = 0x04;
/// `Py_DTSF_NO_NEG_0`: coerce a negative-zero result to `0`.
const PY_DTSF_NO_NEG_0: c_int = 0x08;

/// `Py_DTST_FINITE`: the formatted value was finite.
const PY_DTST_FINITE: c_int = 0;
/// `Py_DTST_INFINITE`: the formatted value was an infinity.
const PY_DTST_INFINITE: c_int = 1;
/// `Py_DTST_NAN`: the formatted value was a NaN.
const PY_DTST_NAN: c_int = 2;

/// Largest accepted base for `PyLong_FromString`.
const MAX_BASE: c_int = 36;

/// Extracts the integer payload of a shim `int` (or `bool`, which is a subclass
/// in CPython), leaving the error state to the caller.
fn int_argument(object: *mut ShimObject) -> Option<i64> {
    match kind_ref(object) {
        Some(ShimKind::Int(value)) => Some(*value),
        Some(ShimKind::Bool(flag)) => Some(*flag as i64),
        _ => None,
    }
}

/// Reports CPython's `_PyNumber_Index` failure for an object that is not an
/// integer. Every numeric accessor here accepts the same domain: `int` and
/// `bool`.
fn set_index_type_error(object: *mut ShimObject) {
    set_error(format!(
        "TypeError: '{}' object cannot be interpreted as an integer",
        object_type_name(object)
    ));
}

// ---------------------------------------------------------------------------
// PyLong accessors
// ---------------------------------------------------------------------------

/// `PyLong_AsDouble`.
///
/// The shim's `int` is an `i64`, so every value is finite and representable as
/// a C `double`; CPython's `"int too large to convert to float"` branch is
/// therefore unreachable here. A non-integer raises `TypeError`, a NULL
/// argument `SystemError`, both returning `-1.0`.
#[no_mangle]
pub unsafe extern "C" fn PyLong_AsDouble(object: *mut ShimObject) -> f64 {
    if object.is_null() {
        bad_internal_call();
        return -1.0;
    }
    match int_argument(object) {
        Some(value) => value as f64,
        None => {
            // CPython's message for this function is not the `__index__` one.
            set_error("TypeError: an integer is required");
            -1.0
        }
    }
}

/// `PyLong_AsInt`.
///
/// Returns the value as a C `int`. A value outside `INT_MIN..=INT_MAX` sets
/// `OverflowError: Python int too large to convert to C int` and returns `-1`,
/// exactly as CPython 3.14 does.
#[no_mangle]
pub unsafe extern "C" fn PyLong_AsInt(object: *mut ShimObject) -> c_int {
    if object.is_null() {
        bad_internal_call();
        return -1;
    }
    let Some(value) = int_argument(object) else {
        set_index_type_error(object);
        return -1;
    };
    match c_int::try_from(value) {
        Ok(value) => value,
        Err(_) => {
            set_error("OverflowError: Python int too large to convert to C int");
            -1
        }
    }
}

/// `PyLong_AsUnsignedLongLongMask`.
///
/// Masks the value modulo `2^64`: negative values wrap through their two's
/// complement and out-of-range magnitudes keep only the low 64 bits. A
/// non-integer raises `TypeError` and a NULL argument `SystemError`, both
/// returning `(unsigned long long)-1`.
#[no_mangle]
pub unsafe extern "C" fn PyLong_AsUnsignedLongLongMask(object: *mut ShimObject) -> u64 {
    if object.is_null() {
        bad_internal_call();
        return u64::MAX;
    }
    match int_argument(object) {
        Some(value) => value as u64,
        None => {
            set_index_type_error(object);
            u64::MAX
        }
    }
}

/// `PyLong_FromSize_t`.
///
/// `size_t` is unsigned; a value above `i64::MAX` cannot be represented by this
/// host's 64-bit `PyLong`, so it raises `OverflowError` instead of being
/// truncated (the same rule as `PyLong_FromUnsignedLongLong`).
#[no_mangle]
pub unsafe extern "C" fn PyLong_FromSize_t(size: usize) -> *mut ShimObject {
    match i64::try_from(size) {
        Ok(value) => new_int(value),
        Err(_) => {
            set_error(format!(
                "OverflowError: integer {size} does not fit this host's 64-bit int model"
            ));
            ptr::null_mut()
        }
    }
}

/// `_PyLong_Copy`.
///
/// Returns a new reference with the same value. CPython returns one of the
/// interned small integers for `-5..=256`; [`new_int`] applies the identical
/// caching, so the returned pointer matches the input for those values and is a
/// fresh object otherwise.
#[no_mangle]
pub unsafe extern "C" fn _PyLong_Copy(source: *mut ShimObject) -> *mut ShimObject {
    if source.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    match int_argument(source) {
        Some(value) => new_int(value),
        None => {
            // CPython asserts a `PyLongObject`; the shim reports the mismatch.
            set_error(format!(
                "TypeError: expect int, got {}",
                object_type_name(source)
            ));
            ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// PyLong_FromString
// ---------------------------------------------------------------------------

/// CPython's `Py_ISSPACE` on an ASCII byte (`\v` and `\f` included, unlike
/// Rust's `is_ascii_whitespace`).
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Entry of CPython's `_PyLong_DigitValue` table: the digit's value, or 255 for
/// any byte that is not an ASCII alphanumeric digit.
fn digit_value(byte: u8) -> c_int {
    match byte {
        b'0'..=b'9' => (byte - b'0') as c_int,
        b'a'..=b'z' => (byte - b'a') as c_int + 10,
        b'A'..=b'Z' => (byte - b'A') as c_int + 10,
        _ => 255,
    }
}

/// Renders `bytes` the way `repr()` renders a Python `str`.
///
/// `PyLong_FromString` formats the offending prefix with `%R`; only the
/// quoting/escaping of the first 200 bytes participates in the message.
fn python_repr(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if character == quote => {
                out.push('\\');
                out.push(quote);
            }
            character if (character as u32) < 0x20 || (character as u32) == 0x7f => {
                out.push_str(&format!("\\x{:02x}", character as u32));
            }
            character => out.push(character),
        }
    }
    out.push(quote);
    out
}

/// Reports `PyLong_FromString`'s parse failure: publishes `pend`, raises
/// `ValueError` with CPython's message and returns NULL.
///
/// # Safety
///
/// `text` must be the NUL-terminated string the caller passed in and
/// `position` must not exceed its length.
unsafe fn from_string_error(
    original: &[u8],
    base: c_int,
    position: usize,
    pend: *mut *mut c_char,
    text: *const c_char,
) -> *mut ShimObject {
    if !pend.is_null() {
        unsafe { *pend = (text as *mut c_char).add(position) };
    }
    let length = original.len().min(200);
    set_error(format!(
        "ValueError: invalid literal for int() with base {base}: {}",
        python_repr(&original[..length])
    ));
    ptr::null_mut()
}

/// `PyLong_FromString`: parses an integer from a NUL-terminated string.
///
/// Matches CPython 3.14's parser: `base == 0` selects the base from a `0x`/
/// `0o`/`0b` prefix (and rejects other leading-zero literals unless the value
/// is zero); bases 2..36 use an optional matching prefix. Single underscores
/// may separate digits (and may follow a prefix); whitespace is allowed before
/// the sign and after the digits. On failure the returned pointer is NULL and
/// `*pend`, when `pend` is non-NULL, names the offending byte at the same
/// offset CPython reports. Values that do not fit this host's 64-bit integer
/// raise `OverflowError`.
#[no_mangle]
pub unsafe extern "C" fn PyLong_FromString(
    text: *const c_char,
    pend: *mut *mut c_char,
    base: c_int,
) -> *mut ShimObject {
    if text.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    if (base != 0 && base < 2) || base > MAX_BASE {
        set_error("ValueError: int() arg 2 must be >= 2 and <= 36");
        return ptr::null_mut();
    }
    let original = unsafe { CStr::from_ptr(text) }.to_bytes();
    let total = original.len();
    let byte_at = |index: usize| original.get(index).copied().unwrap_or(0);

    // CPython also skips whitespace before the sign and accepts one sign only.
    let mut position = 0usize;
    while position < total && is_space(byte_at(position)) {
        position += 1;
    }
    let mut negative = false;
    if byte_at(position) == b'+' {
        position += 1;
    } else if byte_at(position) == b'-' {
        position += 1;
        negative = true;
    }

    let mut base = base;
    let mut error_if_nonzero = false;
    if base == 0 {
        if byte_at(position) != b'0' {
            base = 10;
        } else if matches!(byte_at(position + 1), b'x' | b'X') {
            base = 16;
        } else if matches!(byte_at(position + 1), b'o' | b'O') {
            base = 8;
        } else if matches!(byte_at(position + 1), b'b' | b'B') {
            base = 2;
        } else {
            // A C-style octal literal. It is legal only when the value is zero.
            error_if_nonzero = true;
            base = 10;
        }
    }
    // Strip the prefix (and the one underscore CPython allows right after it).
    if byte_at(position) == b'0'
        && ((base == 16 && matches!(byte_at(position + 1), b'x' | b'X'))
            || (base == 8 && matches!(byte_at(position + 1), b'o' | b'O'))
            || (base == 2 && matches!(byte_at(position + 1), b'b' | b'B')))
    {
        position += 2;
        if byte_at(position) == b'_' {
            position += 1;
        }
    }

    // Validate the digit string, tracking where CPython stops on each rule.
    let start = position;
    if byte_at(start) == b'_' {
        return unsafe { from_string_error(original, base, start, pend, text) };
    }
    let mut cursor = start;
    let mut previous_underscore = false;
    while digit_value(byte_at(cursor)) < base || byte_at(cursor) == b'_' {
        if byte_at(cursor) == b'_' && previous_underscore {
            // The second of two consecutive underscores is the error position
            // minus one, matching `*str = p - 1` in CPython.
            return unsafe { from_string_error(original, base, cursor - 1, pend, text) };
        }
        previous_underscore = byte_at(cursor) == b'_';
        cursor += 1;
    }
    if previous_underscore {
        return unsafe { from_string_error(original, base, cursor - 1, pend, text) };
    }
    let end = cursor;
    if start == end {
        return unsafe { from_string_error(original, base, cursor, pend, text) };
    }
    // Only trailing whitespace may follow the digits.
    while byte_at(cursor) != 0 && is_space(byte_at(cursor)) {
        cursor += 1;
    }
    if byte_at(cursor) != 0 {
        return unsafe { from_string_error(original, base, cursor, pend, text) };
    }

    // Parse the validated digits. The accumulator saturates so that a string
    // with millions of digits stays linear; only the overflow flag matters
    // past `i64::MAX + 1`.
    let limit = (i64::MAX as u128) + 1;
    let mut magnitude: u128 = 0;
    let mut overflow = false;
    #[allow(clippy::needless_range_loop)]
    for index in start..end {
        let byte = original[index];
        if byte == b'_' {
            continue;
        }
        let digit = digit_value(byte) as u128;
        if !overflow {
            magnitude = magnitude * base as u128 + digit;
            if magnitude > limit {
                overflow = true;
            }
        }
    }

    // `error_if_nonzero` rewrites the base to 0 for its message, and a nonzero
    // magnitude then fails before the sign is applied.
    if error_if_nonzero {
        base = 0;
        if magnitude != 0 {
            return unsafe { from_string_error(original, base, cursor, pend, text) };
        }
    }

    // The accumulator saturates at 2^63 (`i64::MAX + 1`), which is exactly
    // representable for a negative value and one too large otherwise.
    let bound = if negative {
        (i64::MAX as u128) + 1
    } else {
        i64::MAX as u128
    };
    if overflow || magnitude > bound {
        if !pend.is_null() {
            unsafe { *pend = (text as *mut c_char).add(cursor) };
        }
        set_error("OverflowError: integer does not fit this host's 64-bit int model");
        return ptr::null_mut();
    }

    // `magnitude` is at most `i64::MAX + 1`; the cast through `i128` represents
    // `i64::MIN` exactly.
    let value = if negative {
        -(magnitude as i128) as i64
    } else {
        magnitude as i64
    };
    if !pend.is_null() {
        unsafe { *pend = (text as *mut c_char).add(cursor) };
    }
    new_int(value)
}

// ---------------------------------------------------------------------------
// PyOS_double_to_string
// ---------------------------------------------------------------------------

/// Shortest round-trip digits and decimal-point position, as
/// `_Py_dg_dtoa(mode=0)` returns them.
fn shortest_digits(value: f64) -> (String, i64) {
    // Rust renders the shortest round-trip representation with a single
    // leading digit, e.g. `1.005e2`; CPython's dtoa yields the same digits.
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .unwrap_or((scientific.as_str(), "0"));
    let exponent: i64 = exponent.parse().unwrap_or(0);
    let mut digits: String = mantissa
        .chars()
        .filter(|character| *character != '.')
        .collect();
    trim_trailing_zeros(&mut digits);
    (digits, exponent + 1)
}

/// `ndigits` significant digits and decimal-point position, as
/// `_Py_dg_dtoa(mode=2)` returns them.
fn significant_digits(value: f64, ndigits: i64) -> (String, i64) {
    // dtoa's mode 2 asks for at least one digit; `0` and negatives round to a
    // single digit, which is what CPython's 'e'/'g' paths produce for them.
    let decimals = (ndigits.max(1) - 1) as usize;
    let scientific = format!("{:.*e}", decimals, value);
    let (mantissa, exponent) = scientific
        .split_once('e')
        .unwrap_or((scientific.as_str(), "0"));
    let exponent: i64 = exponent.parse().unwrap_or(0);
    let mut digits: String = mantissa
        .chars()
        .filter(|character| *character != '.')
        .collect();
    trim_trailing_zeros(&mut digits);
    (digits, exponent + 1)
}

/// Rounded-to-`decimals` digits of the value, with the trailing zeros that
/// `_Py_dg_dtoa(mode=3)` strips removed, and the decimal-point position.
fn fixed_digits(value: f64, decimals: i64) -> (String, i64) {
    let decimals = decimals.max(0) as usize;
    let fixed = format!("{:.*}", decimals, value);
    let (integer, fraction) = fixed.split_once('.').unwrap_or((fixed.as_str(), ""));
    let mut combined = String::with_capacity(integer.len() + fraction.len());
    combined.push_str(integer);
    combined.push_str(fraction);
    if combined.bytes().all(|byte| byte == b'0') {
        // Gay's dtoa reports a zero result as the single digit 0 with the
        // decimal point after it.
        return ("0".to_string(), 1);
    }
    let mut decpt = integer.len() as i64;
    let trimmed = combined.trim_start_matches('0');
    decpt -= (combined.len() - trimmed.len()) as i64;
    let mut digits = trimmed.to_string();
    trim_trailing_zeros(&mut digits);
    (digits, decpt)
}

/// Removes trailing zeros while keeping at least one digit.
fn trim_trailing_zeros(digits: &mut String) {
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
}

/// Appends `count` zero characters, reporting allocation failure instead of
/// panicking.
fn push_zeros(output: &mut String, count: i64) -> bool {
    if count <= 0 {
        return true;
    }
    let Ok(count) = usize::try_from(count) else {
        return false;
    };
    if output.try_reserve(count).is_err() {
        return false;
    }
    for _ in 0..count {
        output.push('0');
    }
    true
}

/// Formats `value` exactly as CPython's `format_float_short` does.
///
/// `format_code` must already be one of `r`, `e`, `f`, `g` (either case) and
/// `precision` must carry CPython's per-code adjustment. Returns the rendered
/// text and the `Py_DTST_*` tag, or `Err(())` when an output buffer allocation
/// failed.
fn format_float_short(
    value: f64,
    format_code: u8,
    precision: i64,
    flags: c_int,
) -> Result<(String, c_int), ()> {
    let uppercase = matches!(format_code, b'E' | b'F' | b'G');
    let code = format_code.to_ascii_lowercase();
    let always_add_sign = flags & PY_DTSF_SIGN != 0;
    let add_dot_zero = flags & PY_DTSF_ADD_DOT_0 != 0;
    let use_alt = flags & PY_DTSF_ALT != 0;
    let no_negative_zero = flags & PY_DTSF_NO_NEG_0 != 0;
    let mut sign = value.is_sign_negative();

    if value.is_nan() {
        // CPython ignores the sign of a NaN; only the explicit sign flag shows.
        let mut output = String::new();
        if always_add_sign {
            output.push('+');
        }
        output.push_str(if uppercase { "NAN" } else { "nan" });
        return Ok((output, PY_DTST_NAN));
    }
    if value.is_infinite() {
        let mut output = String::new();
        if sign {
            output.push('-');
        } else if always_add_sign {
            output.push('+');
        }
        output.push_str(if uppercase { "INF" } else { "inf" });
        return Ok((output, PY_DTST_INFINITE));
    }

    let magnitude = value.abs();
    let (digits, mut decpt) = match code {
        b'r' => shortest_digits(magnitude),
        b'e' | b'g' => significant_digits(magnitude, precision),
        b'f' => fixed_digits(magnitude, precision),
        _ => return Err(()),
    };
    let digits_length = digits.len() as i64;

    if no_negative_zero && sign && digits == "0" {
        sign = false;
    }

    let mut use_exponent = false;
    let mut end;
    match code {
        b'e' => {
            use_exponent = true;
            end = precision;
        }
        b'f' => {
            end = decpt + precision;
        }
        b'g' => {
            let threshold = if add_dot_zero {
                precision - 1
            } else {
                precision
            };
            if decpt <= -4 || decpt > threshold {
                use_exponent = true;
            }
            end = if use_alt { precision } else { digits_length };
        }
        // 'r': switch to exponential at 1e16 / below 1e-4.
        _ => {
            if decpt <= -4 || decpt > 16 {
                use_exponent = true;
            }
            end = digits_length;
        }
    }

    let mut exponent = 0i64;
    if use_exponent {
        exponent = decpt - 1;
        decpt = 1;
    }
    let start = if decpt <= 0 { decpt - 1 } else { 0 };
    if end <= decpt {
        end = if !use_exponent && add_dot_zero {
            decpt + 1
        } else {
            decpt
        };
    }

    let mut output = String::new();
    if sign {
        output.push('-');
    } else if always_add_sign {
        output.push('+');
    }
    if decpt <= 0 {
        if !push_zeros(&mut output, decpt - start) {
            return Err(());
        }
        output.push('.');
        if !push_zeros(&mut output, -decpt) {
            return Err(());
        }
    } else if start < 0 && !push_zeros(&mut output, -start) {
        return Err(());
    }

    if decpt > 0 && decpt <= digits_length {
        let split = decpt as usize;
        output.push_str(&digits[..split]);
        output.push('.');
        output.push_str(&digits[split..]);
    } else {
        output.push_str(&digits);
    }

    if digits_length < decpt {
        if !push_zeros(&mut output, decpt - digits_length) {
            return Err(());
        }
        output.push('.');
        if !push_zeros(&mut output, end - decpt) {
            return Err(());
        }
    } else if !push_zeros(&mut output, end - digits_length) {
        return Err(());
    }

    // CPython deletes a trailing decimal point unless alternate formatting is
    // requested.
    if output.ends_with('.') && !use_alt {
        output.pop();
    }
    if use_exponent {
        output.push(if uppercase { 'E' } else { 'e' });
        output.push(if exponent < 0 { '-' } else { '+' });
        let magnitude = exponent.unsigned_abs();
        if magnitude < 10 {
            output.push('0');
        }
        output.push_str(&magnitude.to_string());
    }
    Ok((output, PY_DTST_FINITE))
}

/// `PyOS_double_to_string`: formats `value` with a `printf`-style code.
///
/// `format_code` is `'r'` (shortest `repr` form, `precision` must be 0),
/// `'e'`/`'E'`, `'f'`/`'F'` or `'g'`/`'G'`; an unsupported code — or `'r'` with
/// a nonzero precision — raises `SystemError` and returns NULL. `flags` is any
/// combination of `Py_DTSF_SIGN`, `Py_DTSF_ADD_DOT_0`, `Py_DTSF_ALT` and
/// `Py_DTSF_NO_NEG_0`. The result is a `PyMem_Malloc` allocation the caller
/// releases with `PyMem_Free`; `*type_out`, when non-NULL, receives one of the
/// `Py_DTST_*` tags.
///
/// Negative precisions are outside the documented contract (CPython's dtoa
/// reads a negative digit count out of bounds). They are handled
/// deterministically: `'e'`/`'E'` and `'g'`/`'G'` round to one significant
/// digit, exactly as CPython's own `'g'`/`'e'` paths do, and `'f'`/`'F'` clamp
/// to zero decimals instead of reproducing CPython's undefined output.
#[no_mangle]
pub unsafe extern "C" fn PyOS_double_to_string(
    value: f64,
    format_code: c_char,
    precision: c_int,
    flags: c_int,
    type_out: *mut c_int,
) -> *mut c_char {
    let code = format_code as u8;
    let mut precision = precision as i64;
    match code {
        // `e` formats one digit before the point, so the significant-digit
        // count is one more than the requested number of decimals.
        b'e' | b'E' => precision += 1,
        b'f' | b'F' => {}
        b'g' | b'G' => {
            // Precision 0 makes no sense for 'g'; CPython interprets it as 1.
            if precision == 0 {
                precision = 1;
            }
        }
        b'r' => {
            if precision != 0 {
                bad_internal_call();
                return ptr::null_mut();
            }
        }
        _ => {
            bad_internal_call();
            return ptr::null_mut();
        }
    }

    let (text, tag) = match format_float_short(value, code, precision, flags) {
        Ok(result) => result,
        Err(()) => {
            PyErr_NoMemory();
            return ptr::null_mut();
        }
    };
    let length = text.len();
    let allocation = PyMem_Malloc(length + 1) as *mut u8;
    if allocation.is_null() {
        PyErr_NoMemory();
        return ptr::null_mut();
    }
    // SAFETY: `allocation` has `length + 1` writable bytes.
    unsafe {
        ptr::copy_nonoverlapping(text.as_ptr(), allocation, length);
        *allocation.add(length) = 0;
    }
    if !type_out.is_null() {
        // SAFETY: the caller passed a writable `int` slot.
        unsafe { *type_out = tag };
    }
    allocation as *mut c_char
}

// ---------------------------------------------------------------------------
// PyMemoryView_FromMemory
// ---------------------------------------------------------------------------

/// `PyMemoryView_FromMemory`: a memoryview over caller-owned memory.
///
/// The view is one-dimensional with format `B` and never takes ownership of
/// `memory`. CPython 3.14 makes the view writable only when `flags` is exactly
/// `PyBUF_WRITE` (any other value, including combinations, leaves it
/// read-only), and that rule is reproduced here. A negative size or a NULL
/// pointer with a nonzero size raises `BufferError` and returns NULL; an empty
/// view may use a NULL pointer.
#[no_mangle]
#[allow(clippy::arc_with_non_send_sync)]
pub unsafe extern "C" fn PyMemoryView_FromMemory(
    memory: *mut c_char,
    size: isize,
    flags: c_int,
) -> *mut ShimObject {
    if size < 0 {
        set_error("BufferError: negative buffer length");
        return ptr::null_mut();
    }
    if memory.is_null() && size != 0 {
        set_error("BufferError: NULL buffer with a non-zero length");
        return ptr::null_mut();
    }
    let readonly = if flags == buffer::PYBUF_WRITE { 0 } else { 1 };
    let mut view = buffer::Py_buffer {
        buf: memory as *mut c_void,
        obj: ptr::null_mut(),
        len: size,
        itemsize: 1,
        readonly,
        ndim: 1,
        format: c"B".as_ptr() as *mut c_char,
        shape: ptr::null_mut(),
        strides: ptr::null_mut(),
        suboffsets: ptr::null_mut(),
        internal: ptr::null_mut(),
    };
    // `PyBuffer_FillInfo(PYBUF_FULL_RO)` points `shape`/`strides` at the
    // view's own `len`/`itemsize`; record those addresses before the buffer is
    // moved into the `Arc`, then let `MemoryViewBuffer` relocate them.
    let source = ptr::addr_of!(view);
    view.shape = ptr::addr_of_mut!(view.len);
    view.strides = ptr::addr_of_mut!(view.itemsize);
    let storage = Arc::new(MemoryViewBuffer::new(view, source));
    storage.relocate_self_references();
    new_object(ShimKind::MemoryView { buffer: storage })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capi::buffer;
    use crate::capi::{
        take_error, PyFloat_FromDouble, PyLong_AsLong, PyLong_FromLong, PyMem_Free, Py_DecRef,
    };
    use std::ffi::CStr;

    /// Calls `PyLong_FromString` and reports the integer, pending error and the
    /// offset `pend` names (usize::MAX when `pend` was never assigned).
    fn parse(text: &str, base: c_int) -> (i64, Option<String>, usize) {
        let c_text = std::ffi::CString::new(text).expect("no embedded NUL");
        let mut pend: *mut c_char = std::ptr::null_mut();
        let object = unsafe { PyLong_FromString(c_text.as_ptr(), &mut pend, base) };
        let error = take_error();
        let offset = if pend.is_null() {
            usize::MAX
        } else {
            (pend as usize) - (c_text.as_ptr() as usize)
        };
        let value = if object.is_null() {
            0
        } else {
            let value = PyLong_AsLong(object) as i64;
            take_error();
            Py_DecRef(object);
            value
        };
        (value, error, offset)
    }

    #[test]
    fn from_string_parses_prefixes_underscores_and_whitespace() {
        assert_eq!(parse("123", 10).0, 123);
        assert_eq!(parse(" 12 ", 10).0, 12);
        assert_eq!(parse("+5", 10).0, 5);
        assert_eq!(parse("-5", 10).0, -5);
        assert_eq!(parse("1_000", 10).0, 1000);
        assert_eq!(parse("0x1f", 0).0, 31);
        assert_eq!(parse("-0X1F", 0).0, -31);
        assert_eq!(parse("0b_101", 0).0, 5);
        assert_eq!(parse("0o17", 0).0, 15);
        assert_eq!(parse("0x1f", 16).0, 31);
        assert_eq!(parse("z", 36).0, 35);
        assert_eq!(parse("Z", 36).0, 35);
        assert_eq!(parse("zz", 36).0, 35 * 36 + 35);
        assert_eq!(parse("0", 0).0, 0);
        assert_eq!(parse("000", 0).0, 0);
        assert_eq!(parse("0x_1f", 0).0, 31);
    }

    #[test]
    fn from_string_reports_cpython_error_positions() {
        let (value, error, offset) = parse("123abc", 10);
        assert_eq!(value, 0);
        assert_eq!(
            error.as_deref(),
            Some("ValueError: invalid literal for int() with base 10: '123abc'")
        );
        assert_eq!(offset, 3);

        let (_, error, offset) = parse("abc", 10);
        assert_eq!(
            error.as_deref(),
            Some("ValueError: invalid literal for int() with base 10: 'abc'")
        );
        assert_eq!(offset, 0);

        let (_, error, offset) = parse("", 10);
        assert_eq!(
            error.as_deref(),
            Some("ValueError: invalid literal for int() with base 10: ''")
        );
        assert_eq!(offset, 0);

        let (_, error, offset) = parse("017", 0);
        assert_eq!(
            error.as_deref(),
            Some("ValueError: invalid literal for int() with base 0: '017'")
        );
        assert_eq!(offset, 3);

        let (_, error, offset) = parse("0x", 0);
        assert_eq!(
            error.as_deref(),
            Some("ValueError: invalid literal for int() with base 16: '0x'")
        );
        assert_eq!(offset, 2);

        let (_, error, offset) = parse("1__0", 10);
        assert_eq!(
            error.as_deref(),
            Some("ValueError: invalid literal for int() with base 10: '1__0'")
        );
        assert_eq!(offset, 1);

        let (_, error, offset) = parse("1_", 10);
        assert_eq!(
            error.as_deref(),
            Some("ValueError: invalid literal for int() with base 10: '1_'")
        );
        assert_eq!(offset, 1);

        // A bad base is rejected before `pend` is touched.
        let (_, error, offset) = parse("12", 1);
        assert_eq!(
            error.as_deref(),
            Some("ValueError: int() arg 2 must be >= 2 and <= 36")
        );
        assert_eq!(offset, usize::MAX);
        let (_, error, _) = parse("12", 37);
        assert_eq!(
            error.as_deref(),
            Some("ValueError: int() arg 2 must be >= 2 and <= 36")
        );

        // repr() quoting: a double quote is used when the text has a quote.
        let (_, error, _) = parse("a'b", 10);
        assert_eq!(
            error.as_deref(),
            Some("ValueError: invalid literal for int() with base 10: \"a'b\"")
        );
    }

    #[test]
    fn from_string_reports_overflow_for_wide_values() {
        let (_, error, _) = parse("99999999999999999999", 10);
        assert_eq!(
            error.as_deref(),
            Some("OverflowError: integer does not fit this host's 64-bit int model")
        );
        // `i64::MIN` is still representable.
        assert_eq!(parse("-9223372036854775808", 10).0, i64::MIN);
        assert_eq!(parse("9223372036854775807", 10).0, i64::MAX);
    }

    #[test]
    fn long_accessors_follow_cpython_errors() {
        let value = PyLong_FromLong(2_147_483_648);
        assert_eq!(unsafe { PyLong_AsInt(value) }, -1);
        assert_eq!(
            take_error().as_deref(),
            Some("OverflowError: Python int too large to convert to C int")
        );
        Py_DecRef(value);

        let value = PyLong_FromLong(42);
        assert_eq!(unsafe { PyLong_AsInt(value) }, 42);
        assert!(take_error().is_none());
        assert_eq!(unsafe { PyLong_AsDouble(value) }, 42.0);
        assert_eq!(unsafe { PyLong_AsUnsignedLongLongMask(value) }, 42);
        Py_DecRef(value);

        let negative = PyLong_FromLong(-1);
        assert_eq!(unsafe { PyLong_AsUnsignedLongLongMask(negative) }, u64::MAX);
        Py_DecRef(negative);

        let float = PyFloat_FromDouble(1.5);
        assert_eq!(unsafe { PyLong_AsDouble(float) }, -1.0);
        assert_eq!(
            take_error().as_deref(),
            Some("TypeError: an integer is required")
        );
        assert_eq!(unsafe { PyLong_AsInt(float) }, -1);
        assert_eq!(
            take_error().as_deref(),
            Some("TypeError: 'float' object cannot be interpreted as an integer")
        );
        assert_eq!(unsafe { PyLong_AsUnsignedLongLongMask(float) }, u64::MAX);
        assert_eq!(
            take_error().as_deref(),
            Some("TypeError: 'float' object cannot be interpreted as an integer")
        );
        Py_DecRef(float);

        assert_eq!(unsafe { PyLong_AsInt(std::ptr::null_mut()) }, -1);
        assert_eq!(
            take_error().as_deref(),
            Some("SystemError: bad argument to internal function")
        );
        assert_eq!(
            unsafe { PyLong_AsUnsignedLongLongMask(std::ptr::null_mut()) },
            u64::MAX
        );
        take_error();
        assert_eq!(unsafe { PyLong_AsDouble(std::ptr::null_mut()) }, -1.0);
        take_error();
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn from_size_t_and_copy_match_the_int_model() {
        let object = unsafe { PyLong_FromSize_t(42) };
        assert_eq!(PyLong_AsLong(object), 42);
        Py_DecRef(object);

        assert!(unsafe { PyLong_FromSize_t(usize::MAX) }.is_null());
        assert_eq!(
            take_error().as_deref(),
            Some("OverflowError: integer 18446744073709551615 does not fit this host's 64-bit int model")
        );

        let small = PyLong_FromLong(7);
        let copy = unsafe { _PyLong_Copy(small) };
        assert_eq!(copy, small, "small ints are interned, as in CPython");
        Py_DecRef(copy);
        Py_DecRef(small);

        let large = PyLong_FromLong(1000);
        let copy = unsafe { _PyLong_Copy(large) };
        assert_ne!(copy, large);
        assert_eq!(PyLong_AsLong(copy), 1000);
        Py_DecRef(copy);
        Py_DecRef(large);
    }

    /// Formats through `PyOS_double_to_string`, copying and freeing the result.
    fn double_to_string(
        value: f64,
        code: u8,
        precision: c_int,
        flags: c_int,
    ) -> (Option<String>, c_int, Option<String>) {
        let mut tag = -1;
        let pointer =
            unsafe { PyOS_double_to_string(value, code as c_char, precision, flags, &mut tag) };
        let error = take_error();
        if pointer.is_null() {
            return (None, tag, error);
        }
        let text = unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .expect("ASCII output")
            .to_string();
        PyMem_Free(pointer as *mut c_void);
        (Some(text), tag, error)
    }

    #[test]
    fn double_to_string_covers_every_format_code() {
        // 'r' without `Py_DTSF_ADD_DOT_0` does not decorate an integral value;
        // CPython's `repr()` passes that flag itself.
        assert_eq!(double_to_string(0.0, b'r', 0, 0).0.as_deref(), Some("0"));
        assert_eq!(double_to_string(-0.0, b'r', 0, 0).0.as_deref(), Some("-0"));
        assert_eq!(double_to_string(1.0, b'r', 0, 0).0.as_deref(), Some("1"));
        assert_eq!(double_to_string(1.5, b'r', 0, 0).0.as_deref(), Some("1.5"));
        assert_eq!(
            double_to_string(1e15, b'r', 0, 0).0.as_deref(),
            Some("1000000000000000")
        );
        assert_eq!(
            double_to_string(1.0, b'r', 0, PY_DTSF_ADD_DOT_0)
                .0
                .as_deref(),
            Some("1.0")
        );
        assert_eq!(
            double_to_string(100.0, b'r', 0, PY_DTSF_ADD_DOT_0)
                .0
                .as_deref(),
            Some("100.0")
        );
        assert_eq!(
            double_to_string(1e15, b'r', 0, PY_DTSF_ADD_DOT_0)
                .0
                .as_deref(),
            Some("1000000000000000.0")
        );
        assert_eq!(
            double_to_string(1e16, b'r', 0, 0).0.as_deref(),
            Some("1e+16")
        );
        assert_eq!(
            double_to_string(1e-5, b'r', 0, 0).0.as_deref(),
            Some("1e-05")
        );

        assert_eq!(
            double_to_string(1.5, b'e', 2, 0).0.as_deref(),
            Some("1.50e+00")
        );
        assert_eq!(
            double_to_string(1.5, b'E', 3, PY_DTSF_SIGN).0.as_deref(),
            Some("+1.500E+00")
        );
        assert_eq!(
            double_to_string(1.5, b'f', 3, 0).0.as_deref(),
            Some("1.500")
        );
        assert_eq!(double_to_string(1.5, b'f', 0, 0).0.as_deref(), Some("2"));
        assert_eq!(double_to_string(1.5, b'F', 1, 0).0.as_deref(), Some("1.5"));
        assert_eq!(double_to_string(1.5, b'g', 0, 0).0.as_deref(), Some("2"));
        assert_eq!(double_to_string(1.5, b'g', 3, 0).0.as_deref(), Some("1.5"));
        assert_eq!(
            double_to_string(1.5, b'g', 3, PY_DTSF_ALT).0.as_deref(),
            Some("1.50")
        );
        assert_eq!(
            double_to_string(1.0, b'g', 3, PY_DTSF_ADD_DOT_0)
                .0
                .as_deref(),
            Some("1.0")
        );
        assert_eq!(
            double_to_string(123.456, b'G', 2, 0).0.as_deref(),
            Some("1.2E+02")
        );

        // Flags: sign, negative-zero suppression.
        assert_eq!(
            double_to_string(1.0, b'r', 0, PY_DTSF_SIGN).0.as_deref(),
            Some("+1")
        );
        assert_eq!(
            double_to_string(1.0, b'r', 0, PY_DTSF_SIGN | PY_DTSF_ADD_DOT_0)
                .0
                .as_deref(),
            Some("+1.0")
        );
        assert_eq!(
            double_to_string(-0.0, b'r', 0, PY_DTSF_NO_NEG_0)
                .0
                .as_deref(),
            Some("0")
        );
        assert_eq!(
            double_to_string(-0.0, b'r', 0, PY_DTSF_NO_NEG_0 | PY_DTSF_ADD_DOT_0)
                .0
                .as_deref(),
            Some("0.0")
        );
        assert_eq!(
            double_to_string(-0.0, b'r', 0, PY_DTSF_NO_NEG_0).1,
            PY_DTST_FINITE
        );

        // Infinities and NaNs carry the right tag and case.
        let (text, tag, _) = double_to_string(f64::INFINITY, b'r', 0, 0);
        assert_eq!(text.as_deref(), Some("inf"));
        assert_eq!(tag, PY_DTST_INFINITE);
        let (text, _, _) = double_to_string(f64::NEG_INFINITY, b'r', 0, 0);
        assert_eq!(text.as_deref(), Some("-inf"));
        let (text, _, _) = double_to_string(f64::INFINITY, b'F', 0, 0);
        assert_eq!(text.as_deref(), Some("INF"));
        let (text, tag, _) = double_to_string(f64::NAN, b'r', 0, 0);
        assert_eq!(text.as_deref(), Some("nan"));
        assert_eq!(tag, PY_DTST_NAN);
        let (text, _, _) = double_to_string(f64::NAN, b'r', 0, PY_DTSF_SIGN);
        assert_eq!(text.as_deref(), Some("+nan"));

        // Invalid requests raise `SystemError` and return NULL.
        let (text, _, error) = double_to_string(1.5, b'x', 0, 0);
        assert!(text.is_none());
        assert_eq!(
            error.as_deref(),
            Some("SystemError: bad argument to internal function")
        );
        let (text, _, error) = double_to_string(1.5, b'r', 1, 0);
        assert!(text.is_none());
        assert_eq!(
            error.as_deref(),
            Some("SystemError: bad argument to internal function")
        );
    }

    #[test]
    fn memoryview_from_memory_wraps_without_owning() {
        let mut storage = [1u8, 2, 3, 4];
        let writable = unsafe {
            PyMemoryView_FromMemory(
                storage.as_mut_ptr() as *mut c_char,
                storage.len() as isize,
                buffer::PYBUF_WRITE,
            )
        };
        assert!(!writable.is_null());
        let mut view: buffer::Py_buffer = unsafe { std::mem::zeroed() };
        assert_eq!(
            buffer::PyObject_GetBuffer(writable, &mut view, buffer::PYBUF_SIMPLE),
            0
        );
        assert_eq!(view.len, 4);
        assert_eq!(view.itemsize, 1);
        assert_eq!(view.readonly, 0);
        buffer::PyBuffer_Release(&mut view);
        Py_DecRef(writable);

        let readonly = unsafe {
            PyMemoryView_FromMemory(storage.as_mut_ptr() as *mut c_char, 4, buffer::PYBUF_READ)
        };
        assert!(!readonly.is_null());
        let mut view: buffer::Py_buffer = unsafe { std::mem::zeroed() };
        assert_eq!(
            buffer::PyObject_GetBuffer(readonly, &mut view, buffer::PYBUF_SIMPLE),
            0
        );
        assert_eq!(view.readonly, 1);
        buffer::PyBuffer_Release(&mut view);
        Py_DecRef(readonly);

        // Combinations other than exactly `PyBUF_WRITE` stay read-only.
        let combined = unsafe {
            PyMemoryView_FromMemory(
                storage.as_mut_ptr() as *mut c_char,
                4,
                buffer::PYBUF_WRITE | buffer::PYBUF_READ,
            )
        };
        assert!(!combined.is_null());
        let mut view: buffer::Py_buffer = unsafe { std::mem::zeroed() };
        buffer::PyObject_GetBuffer(combined, &mut view, buffer::PYBUF_SIMPLE);
        assert_eq!(view.readonly, 1);
        buffer::PyBuffer_Release(&mut view);
        Py_DecRef(combined);
    }

    #[test]
    fn memoryview_from_memory_rejects_invalid_arguments() {
        let mut storage = [0u8; 1];
        let view = unsafe {
            PyMemoryView_FromMemory(storage.as_mut_ptr() as *mut c_char, -1, buffer::PYBUF_READ)
        };
        assert!(view.is_null());
        assert_eq!(
            take_error().as_deref(),
            Some("BufferError: negative buffer length")
        );

        let view = unsafe { PyMemoryView_FromMemory(std::ptr::null_mut(), 4, buffer::PYBUF_READ) };
        assert!(view.is_null());
        assert_eq!(
            take_error().as_deref(),
            Some("BufferError: NULL buffer with a non-zero length")
        );

        // An empty view may use a NULL pointer.
        let view = unsafe { PyMemoryView_FromMemory(std::ptr::null_mut(), 0, buffer::PYBUF_READ) };
        assert!(!view.is_null());
        Py_DecRef(view);
    }
}
