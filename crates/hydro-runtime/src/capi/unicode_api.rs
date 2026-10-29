//! CPython 3.14 Unicode/bytes entry points missing from the ABI inventory.
//!
//! This group covers construction (`PyUnicode_FromOrdinal`,
//! `PyUnicode_FromWideChar`, `PyUnicode_DecodeUTF8`,
//! `PyUnicode_DecodeLocale`), storage mutation (`PyUnicode_CopyCharacters`,
//! `PyUnicode_Resize`), searching (`PyUnicode_Find`, `PyUnicode_FindChar`,
//! `PyUnicode_Equal`), splitting/joining (`PyUnicode_Split`,
//! `PyUnicode_Join`, `PyBytes_Join`) and locale encoding
//! (`PyUnicode_EncodeLocale`).
//!
//! Signatures, return values and error behaviour mirror CPython 3.14 as
//! declared by `unicodeobject.h`, `cpython/unicodeobject.h` and
//! `cpython/bytesobject.h`. Everything is expressed on top of the shim's
//! existing representation: a string is a Rust `String` plus, for strings
//! created through `PyUnicode_New` (and for strings that hold lone
//! surrogates), the writable UCS buffer that `PyUnicode_DATA` exposes.
//!
//! Consequences of that representation, in line with the rest of the shim:
//!
//! * `PyUnicode_New` storage is resizable here, so `PyUnicode_Resize` can grow
//!   and shrink it in place. A string that is not uniquely referenced is
//!   replaced by a copy, exactly as `unicode_resize()` does.
//! * `RawText` records the code unit width but not whether `PyUnicode_New` was
//!   asked for an ASCII (`maxchar < 128`) or latin1 buffer, so
//!   `PyUnicode_CopyCharacters` treats width-1 `PyUnicode_New` destinations as
//!   latin1 and keeps CPython's content-derived distinction for
//!   `String`-backed strings.
//! * Byte offsets reported by the UTF-8 decoders are byte positions; search
//!   and split indices count code points (code units for `PyUnicode_New`
//!   storage), as in CPython.
//! * Unpaired surrogates survive in the UCS storage written by
//!   `PyUnicode_FromWideChar`, `PyUnicode_DecodeUTF8` (`surrogateescape` /
//!   `surrogatepass`) and `PyUnicode_FromOrdinal`, but reading such a string
//!   back as text yields U+FFFD, the shim-wide documented limitation.
//! * The process locale encoding is UTF-8, which is what
//!   `PyUnicode_DecodeLocale`/`PyUnicode_EncodeLocale` use. Their error
//!   handlers are limited to the two the wide-character locale codecs accept
//!   (`strict` and `surrogateescape`); anything else raises
//!   `ValueError: unsupported error handler`.
//!
//! Nothing here panics: every failure path returns NULL (or `-1`/`-2`) with a
//! tagged Python error set, and every owned reference is released exactly once.

#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uint};
use std::ptr;
use std::sync::atomic::Ordering;

use super::*;

// ---------------------------------------------------------------------------
// String storage helpers
// ---------------------------------------------------------------------------

/// A string's code points together with the UCS kind of its storage.
struct StringView {
    /// Code unit width of the object's storage (1, 2 or 4 bytes).
    width: u8,
    /// The string's code points, in CPython's order.
    units: Vec<u32>,
    /// True when `units` came from a `PyUnicode_New`-style UCS buffer rather
    /// than from the decoded `String` payload.
    raw: bool,
}

/// Reads the code points of a `PyUnicode_New` UCS buffer.
fn raw_units(raw: &RawText) -> Result<Vec<u32>, ()> {
    let mut units = Vec::new();
    if units
        .try_reserve_exact(raw.data.len() / raw.width.max(1) as usize)
        .is_err()
    {
        set_error("MemoryError: cannot allocate the decoded string");
        return Err(());
    }
    match raw.width {
        1 => units.extend(raw.data.iter().map(|&byte| byte as u32)),
        2 => {
            for chunk in raw.data.as_chunks::<2>().0 {
                units.push(u16::from_le_bytes([chunk[0], chunk[1]]) as u32);
            }
        }
        4 => {
            for chunk in raw.data.as_chunks::<4>().0 {
                units.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
        }
        _ => {
            set_error("SystemError: invalid unicode storage kind");
            return Err(());
        }
    }
    Ok(units)
}

/// Code unit width CPython would use for `units` (`PyUnicode_New`'s choice).
fn width_for_units(units: &[u32]) -> u8 {
    let max = units.iter().copied().max().unwrap_or(0);
    if max < 0x100 {
        1
    } else if max < 0x10000 {
        2
    } else {
        4
    }
}

/// CPython's `unicode_kind_name()` for compact strings.
fn kind_name(width: u8, units: &[u32]) -> &'static str {
    match width {
        1 => {
            if units.iter().all(|&unit| unit < 0x80) {
                "ascii"
            } else {
                "latin1"
            }
        }
        2 => "UCS2",
        _ => "UCS4",
    }
}

/// `unicode_kind_name()` derived from a destination's maximum code point.
fn kind_name_from_maxchar(width: u8, maxchar: u32) -> &'static str {
    match width {
        1 => {
            if maxchar < 0x80 {
                "ascii"
            } else {
                "latin1"
            }
        }
        2 => "UCS2",
        _ => "UCS4",
    }
}

/// Largest code point `PyUnicode_CopyCharacters` may write into `view`.
///
/// `String`-backed strings reproduce CPython's content-derived ASCII/latin1
/// split (`PyUnicode_MAX_CHAR_VALUE`). `PyUnicode_New` storage only records its
/// code unit width, so a width-1 buffer is treated as latin1: the declared
/// maximum character is not part of the shim payload, and assuming latin1 keeps
/// the documented "`PyUnicode_New(size, 0xff)` then copy latin1 data" pattern
/// working.
fn destination_maxchar(view: &StringView) -> u32 {
    match view.width {
        1 if view.raw => 0xFF,
        1 if view.units.iter().all(|&unit| unit < 0x80) => 0x7F,
        1 => 0xFF,
        2 => 0xFFFF,
        _ => 0x10FFFF,
    }
}

/// Borrows a string's code points; `Ok(None)` means "not a string".
///
/// Memory pressure is reported through `set_error` and surfaces as `Err(())`.
fn string_view(object: *mut ShimObject) -> Result<Option<StringView>, ()> {
    let kind = match kind_ref(object) {
        Some(kind) => kind,
        None => return Ok(None),
    };
    match kind {
        ShimKind::Str { raw: Some(raw), .. } => {
            let width = raw.width;
            Ok(Some(StringView {
                width,
                units: raw_units(raw)?,
                raw: true,
            }))
        }
        ShimKind::Str { text, .. } => {
            let mut units = Vec::new();
            if units.try_reserve_exact(text.len()).is_err() {
                set_error("MemoryError: cannot allocate the decoded string");
                return Err(());
            }
            units.extend(text.chars().map(|character| character as u32));
            Ok(Some(StringView {
                width: width_for_units(&units),
                units,
                raw: false,
            }))
        }
        _ => Ok(None),
    }
}

/// [`string_view`] for callers that require a string, raising CPython's
/// `ensure_unicode` `TypeError` otherwise.
fn require_string_view(object: *mut ShimObject) -> Option<StringView> {
    match string_view(object) {
        Ok(Some(view)) => Some(view),
        Ok(None) => {
            set_error(format!(
                "TypeError: must be str, not {}",
                object_type_name(object)
            ));
            None
        }
        Err(()) => None,
    }
}

/// Decodes code points into the shim's text payload, replacing lone
/// surrogates the same way `refresh_raw_unicode` does.
fn text_from_units(units: &[u32]) -> Result<String, String> {
    let mut capacity = 0usize;
    for &unit in units {
        let length = if (0xD800..=0xDFFF).contains(&unit) {
            3
        } else if unit < 0x80 {
            1
        } else if unit < 0x800 {
            2
        } else if unit < 0x10000 {
            3
        } else if unit <= 0x10FFFF {
            4
        } else {
            return Err("SystemError: invalid code point in unicode storage".to_string());
        };
        capacity = capacity
            .checked_add(length)
            .ok_or_else(|| "MemoryError: string is too large".to_string())?;
    }
    let mut text = String::new();
    if text.try_reserve_exact(capacity).is_err() {
        return Err("MemoryError: cannot allocate the string".to_string());
    }
    for &unit in units {
        text.push(char::from_u32(unit).unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    Ok(text)
}

/// Writes one code unit into plain UCS storage.
fn push_unit(data: &mut Vec<u8>, width: u8, code: u32) {
    if width == 1 {
        data.push(code as u8);
    } else if width == 2 {
        data.extend_from_slice(&(code as u16).to_le_bytes());
    } else {
        data.extend_from_slice(&code.to_le_bytes());
    }
}

/// Overwrites one code unit in existing UCS storage.
fn write_unit(data: &mut [u8], width: u8, index: usize, code: u32) {
    let offset = index * width as usize;
    if width == 1 {
        data[offset] = code as u8;
    } else if width == 2 {
        data[offset..offset + 2].copy_from_slice(&(code as u16).to_le_bytes());
    } else {
        data[offset..offset + 4].copy_from_slice(&code.to_le_bytes());
    }
}

/// Builds a string with explicit UCS storage, preserving `width`.
///
/// Returns NULL with a Python error on allocation failure.
fn new_raw_unicode(width: u8, units: &[u32]) -> *mut ShimObject {
    if units.is_empty() {
        return interned().empty_str as *mut ShimObject;
    }
    let text = match text_from_units(units) {
        Ok(text) => text,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    let byte_length = match units.len().checked_mul(width as usize) {
        Some(length) => length,
        None => {
            set_error("MemoryError: string is too large");
            return ptr::null_mut();
        }
    };
    let mut data = Vec::new();
    if data.try_reserve_exact(byte_length).is_err() {
        set_error("MemoryError: cannot allocate the string buffer");
        return ptr::null_mut();
    }
    for &unit in units {
        push_unit(&mut data, width, unit);
    }
    let c_text = CString::new(text.as_bytes()).ok();
    new_object(ShimKind::Str {
        text,
        c_text,
        raw: Some(RawText { width, data }),
    })
}

/// Builds a string from code points, choosing the storage CPython would use.
///
/// Surrogate code points are kept in UCS storage; ordinary text takes the
/// `String`-backed representation.
fn new_unicode_from_units(units: &[u32]) -> *mut ShimObject {
    if units.is_empty() {
        return interned().empty_str as *mut ShimObject;
    }
    if units.iter().any(|&unit| (0xD800..=0xDFFF).contains(&unit)) {
        return new_raw_unicode(width_for_units(units), units);
    }
    match text_from_units(units) {
        Ok(text) => new_str(text),
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// Builds a `bytes` object from an owned buffer without copying it twice.
fn new_bytes_from_vec(data: Vec<u8>) -> *mut ShimObject {
    if data.is_empty() {
        return interned().empty_bytes as *mut ShimObject;
    }
    let mut c_data = Vec::new();
    if c_data.try_reserve_exact(data.len() + 1).is_err() {
        set_error("MemoryError: cannot allocate the bytes buffer");
        return ptr::null_mut();
    }
    c_data.extend_from_slice(&data);
    c_data.push(0);
    new_object(ShimKind::Bytes { data, c_data })
}

/// True when the object carries a reference count of exactly one, CPython's
/// precondition for mutating string storage in place.
fn unicode_is_modifiable(object: *mut ShimObject) -> bool {
    if object.is_null() {
        return false;
    }
    // SAFETY: every handle starts with the CPython header.
    unsafe { (*object).head.ob_refcnt.load(Ordering::Relaxed) == 1 }
}

// ---------------------------------------------------------------------------
// UTF-8 decoding
// ---------------------------------------------------------------------------

/// Error handlers understood by `PyUnicode_DecodeUTF8`.
enum Utf8Handler {
    Strict,
    Ignore,
    Replace,
    SurrogateEscape,
    SurrogatePass,
    /// `backslashreplace`: each byte of the failing range becomes `\xNN`.
    BackslashReplace,
    /// A handler the codec machinery rejects for decoding. Like CPython, the
    /// failure is only reported when a decode error actually occurs.
    DecodeUnsupported,
    /// An unregistered handler name: CPython only looks it up once a decode
    /// error actually occurs, so the name is kept for that moment.
    Unknown(String),
}

/// Resolves the `errors` argument of `PyUnicode_DecodeUTF8`.
fn utf8_handler(errors: *const c_char) -> Utf8Handler {
    if errors.is_null() {
        return Utf8Handler::Strict;
    }
    match read_c_string(errors).as_str() {
        "strict" => Utf8Handler::Strict,
        "ignore" => Utf8Handler::Ignore,
        "replace" => Utf8Handler::Replace,
        "surrogateescape" => Utf8Handler::SurrogateEscape,
        "surrogatepass" => Utf8Handler::SurrogatePass,
        "backslashreplace" => Utf8Handler::BackslashReplace,
        "xmlcharrefreplace" | "namereplace" => Utf8Handler::DecodeUnsupported,
        other => Utf8Handler::Unknown(other.to_string()),
    }
}

/// Lowercase hexadecimal digit used by `backslashreplace`.
fn hex_digit(value: u8) -> u32 {
    if value < 10 {
        b'0' as u32 + value as u32
    } else {
        b'a' as u32 + (value - 10) as u32
    }
}

/// Resolves the `errors` argument shared by the locale codecs.
///
/// The wide-character locale codecs accept only `strict` (the default) and
/// `surrogateescape`; every other spelling raises `ValueError`, including
/// handlers CPython's codec machinery would otherwise know.
fn locale_handler(errors: *const c_char) -> Result<Utf8Handler, String> {
    if errors.is_null() {
        return Ok(Utf8Handler::Strict);
    }
    match read_c_string(errors).as_str() {
        "strict" => Ok(Utf8Handler::Strict),
        "surrogateescape" => Ok(Utf8Handler::SurrogateEscape),
        _ => Err("ValueError: unsupported error handler".to_string()),
    }
}

/// Decodes one UTF-8 sequence.
///
/// On failure returns the byte length CPython reports for the offending
/// sequence together with the reason string of its `UnicodeDecodeError`.
fn decode_utf8_char(
    data: &[u8],
    position: usize,
    allow_surrogates: bool,
) -> Result<(u32, usize), (usize, &'static str)> {
    let first = data[position];
    if first < 0x80 {
        return Ok((first as u32, 1));
    }
    let (continuations, second_min, second_max) = match first {
        0xC2..=0xDF => (1u8, 0x80u8, 0xBFu8),
        0xE0 => (2, 0xA0, 0xBF),
        0xED if allow_surrogates => (2, 0x80, 0xBF),
        0xED => (2, 0x80, 0x9F),
        0xE1..=0xEF => (2, 0x80, 0xBF),
        0xF0 => (3, 0x90, 0xBF),
        0xF4 => (3, 0x80, 0x8F),
        0xF1..=0xF3 => (3, 0x80, 0xBF),
        _ => return Err((1, "invalid start byte")),
    };
    let available = data.len() - position - 1;
    if available == 0 {
        return Err((data.len() - position, "unexpected end of data"));
    }
    let second = data[position + 1];
    if second < second_min || second > second_max {
        return Err((1, "invalid continuation byte"));
    }
    for index in 2..=continuations as usize {
        if index > available {
            return Err((data.len() - position, "unexpected end of data"));
        }
        let byte = data[position + index];
        if !(0x80..=0xBF).contains(&byte) {
            // CPython reports the valid prefix of the sequence as the error
            // range, so `\xf0\x9f(` fails over positions 0-1.
            return Err((index, "invalid continuation byte"));
        }
    }
    let mut code = match first {
        0xC2..=0xDF => (first & 0x1F) as u32,
        0xE0..=0xEF => (first & 0x0F) as u32,
        _ => (first & 0x07) as u32,
    };
    for index in 1..=continuations as usize {
        code = (code << 6) | (data[position + index] & 0x3F) as u32;
    }
    Ok((code, continuations as usize + 1))
}

/// Decodes UTF-8 bytes with `handler`.
///
/// `locale` selects the wide-character locale decoder's behaviour: every
/// malformed sequence is reported one byte at a time and its diagnostics use
/// the `locale` codec name and reason. Otherwise CPython's UTF-8 codec rules
/// apply, where a truncated sequence consumes all of its remaining bytes.
fn decode_utf8_units(data: &[u8], handler: &Utf8Handler, locale: bool) -> Result<Vec<u32>, String> {
    let codec = if locale { "locale" } else { "utf-8" };
    let mut units = Vec::new();
    if units.try_reserve_exact(data.len()).is_err() {
        return Err("MemoryError: cannot allocate the decoded string".to_string());
    }
    let allow_surrogates = matches!(handler, Utf8Handler::SurrogatePass);
    let mut position = 0usize;
    while position < data.len() {
        match decode_utf8_char(data, position, allow_surrogates) {
            Ok((code, consumed)) => {
                units.push(code);
                position += consumed;
            }
            Err((consumed, reason)) => {
                let (consumed, reason) = if locale {
                    (1, "decoding error")
                } else {
                    (consumed, reason)
                };
                match handler {
                    Utf8Handler::Ignore => position += consumed,
                    Utf8Handler::Replace => {
                        units.push(char::REPLACEMENT_CHARACTER as u32);
                        position += consumed;
                    }
                    Utf8Handler::SurrogateEscape => {
                        for offset in 0..consumed {
                            units.push(0xDC00 + data[position + offset] as u32);
                        }
                        position += consumed;
                    }
                    Utf8Handler::BackslashReplace => {
                        for offset in 0..consumed {
                            let byte = data[position + offset];
                            units.push(b'\\' as u32);
                            units.push(b'x' as u32);
                            units.push(hex_digit(byte >> 4));
                            units.push(hex_digit(byte & 0x0F));
                        }
                        position += consumed;
                    }
                    Utf8Handler::DecodeUnsupported => {
                        return Err(
                            "TypeError: don't know how to handle UnicodeDecodeError in error \
                             callback"
                                .to_string(),
                        )
                    }
                    Utf8Handler::Strict | Utf8Handler::SurrogatePass => {
                        let message = if consumed == 1 {
                            format!(
                                "UnicodeDecodeError: '{codec}' codec can't decode byte \
                                 0x{:02x} in position {position}: {reason}",
                                data[position]
                            )
                        } else {
                            format!(
                                "UnicodeDecodeError: '{codec}' codec can't decode bytes in \
                                 position {position}-{}: {reason}",
                                position + consumed - 1
                            )
                        };
                        return Err(message);
                    }
                    Utf8Handler::Unknown(name) => {
                        return Err(format!("LookupError: unknown error handler name '{name}'"))
                    }
                }
            }
        }
    }
    Ok(units)
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

/// `PyUnicode_FromOrdinal`: the one-character string for `character`.
#[no_mangle]
pub unsafe extern "C" fn PyUnicode_FromOrdinal(character: c_int) -> *mut ShimObject {
    if !(0..=0x10FFFF).contains(&character) {
        set_error("ValueError: chr() arg not in range(0x110000)");
        return ptr::null_mut();
    }
    new_unicode_from_units(&[character as u32])
}

/// `PyUnicode_FromWideChar`: decodes a `wchar_t` buffer.
///
/// `wchar_t` is a signed 32-bit value on this target; `size == -1` scans for
/// the terminating NUL. Code points above U+10FFFF (including negative
/// `wchar_t` values) raise `ValueError`, as in CPython.
#[no_mangle]
pub unsafe extern "C" fn PyUnicode_FromWideChar(value: *const i32, size: isize) -> *mut ShimObject {
    if value.is_null() {
        if size == 0 {
            return interned().empty_str as *mut ShimObject;
        }
        bad_internal_call();
        return ptr::null_mut();
    }
    let length = if size == -1 {
        let mut length = 0usize;
        loop {
            // SAFETY: the caller supplies a NUL-terminated wide string.
            if unsafe { *value.add(length) } == 0 {
                break;
            }
            length += 1;
        }
        length
    } else if size < 0 {
        // CPython reaches this state only through `wcslen()`; refusing to scan
        // an invalid negative length keeps the shim from walking memory.
        bad_internal_call();
        return ptr::null_mut();
    } else {
        match usize::try_from(size) {
            Ok(length) => length,
            Err(_) => {
                bad_internal_call();
                return ptr::null_mut();
            }
        }
    };
    if length == 0 {
        return interned().empty_str as *mut ShimObject;
    }
    let mut units = Vec::new();
    if units.try_reserve_exact(length).is_err() {
        set_error("MemoryError: cannot allocate the decoded string");
        return ptr::null_mut();
    }
    for index in 0..length {
        // SAFETY: the caller supplies `length` readable wide characters.
        let code = unsafe { *value.add(index) } as u32;
        if code > 0x10FFFF {
            set_error(format!(
                "ValueError: character U+{code:x} is not in range [U+0000; U+10ffff]"
            ));
            return ptr::null_mut();
        }
        units.push(code);
    }
    new_unicode_from_units(&units)
}

/// `PyUnicode_DecodeUTF8`: decodes UTF-8 bytes with `errors` handling.
#[no_mangle]
pub unsafe extern "C" fn PyUnicode_DecodeUTF8(
    string: *const c_char,
    length: isize,
    errors: *const c_char,
) -> *mut ShimObject {
    if length == 0 {
        return interned().empty_str as *mut ShimObject;
    }
    if length < 0 || (length as usize) > isize::MAX as usize - 88 {
        // CPython reports a negative (or absurdly large) size as a failed
        // allocation because it casts the size to `size_t` first.
        set_error("MemoryError: cannot allocate the decoded string");
        return ptr::null_mut();
    }
    if string.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    // SAFETY: the caller supplies `length` readable bytes.
    let data = unsafe { std::slice::from_raw_parts(string as *const u8, length as usize) };
    let handler = utf8_handler(errors);
    match decode_utf8_units(data, &handler, false) {
        Ok(units) => new_unicode_from_units(&units),
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyUnicode_DecodeLocale`: decodes a NUL-terminated locale-encoded string.
///
/// The shim's locale encoding is UTF-8. The error handler is validated before
/// the input is decoded (CPython's locale decoder rejects unknown handlers even
/// for empty input), and only `strict`/`surrogateescape` are accepted.
#[no_mangle]
pub unsafe extern "C" fn PyUnicode_DecodeLocale(
    string: *const c_char,
    errors: *const c_char,
) -> *mut ShimObject {
    if string.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    let handler = match locale_handler(errors) {
        Ok(handler) => handler,
        Err(error) => {
            set_error(error);
            return ptr::null_mut();
        }
    };
    // SAFETY: `string` is NUL-terminated by the C API contract.
    let data = unsafe { CStr::from_ptr(string) }.to_bytes();
    if data.is_empty() {
        return interned().empty_str as *mut ShimObject;
    }
    match decode_utf8_units(data, &handler, true) {
        Ok(units) => new_unicode_from_units(&units),
        Err(error) => {
            set_error(error);
            ptr::null_mut()
        }
    }
}

/// `PyUnicode_EncodeLocale`: encodes a string with the locale encoding.
///
/// The shim's locale encoding is UTF-8, which can encode every scalar value
/// the shim can hold, so the two accepted handlers (`strict` and
/// `surrogateescape`) produce identical bytes. Strings holding embedded NULs
/// are rejected, matching the wide-character encoder CPython routes through.
#[no_mangle]
pub unsafe extern "C" fn PyUnicode_EncodeLocale(
    unicode: *mut ShimObject,
    errors: *const c_char,
) -> *mut ShimObject {
    let text = match kind_ref(unicode) {
        Some(ShimKind::Str { text, .. }) => text,
        _ => {
            set_error("TypeError: bad argument type for built-in operation");
            return ptr::null_mut();
        }
    };
    if text.contains('\0') {
        set_error("ValueError: embedded null character");
        return ptr::null_mut();
    }
    if let Err(error) = locale_handler(errors) {
        set_error(error);
        return ptr::null_mut();
    }
    let mut data = Vec::new();
    if data.try_reserve_exact(text.len()).is_err() {
        set_error("MemoryError: cannot allocate the encoded bytes");
        return ptr::null_mut();
    }
    data.extend_from_slice(text.as_bytes());
    new_bytes_from_vec(data)
}

// ---------------------------------------------------------------------------
// Storage mutation
// ---------------------------------------------------------------------------

/// `PyUnicode_CopyCharacters`: copies code points between two strings.
///
/// Returns the number of copied code points, or `-1` with an exception set.
/// The destination must be uniquely referenced, and every copied code point
/// must fit the destination's UCS kind. A failure part-way through keeps the
/// characters copied before the offending one, as `_copy_characters()` does.
#[no_mangle]
pub unsafe extern "C" fn PyUnicode_CopyCharacters(
    to: *mut ShimObject,
    to_start: isize,
    from: *mut ShimObject,
    from_start: isize,
    how_many: isize,
) -> isize {
    let from_view = match string_view(from) {
        Ok(Some(view)) => view,
        Ok(None) => {
            bad_internal_call();
            return -1;
        }
        Err(()) => return -1,
    };
    let to_view = match string_view(to) {
        Ok(Some(view)) => view,
        Ok(None) => {
            bad_internal_call();
            return -1;
        }
        Err(()) => return -1,
    };

    let from_length = from_view.units.len() as isize;
    let to_length = to_view.units.len() as isize;
    if from_start < 0 || from_start > from_length {
        set_error("IndexError: string index out of range");
        return -1;
    }
    if to_start < 0 || to_start > to_length {
        set_error("IndexError: string index out of range");
        return -1;
    }
    if how_many < 0 {
        set_error("SystemError: how_many cannot be negative");
        return -1;
    }
    let how_many = how_many.min(from_length - from_start) as usize;
    let write_start = to_start as usize;
    if how_many > to_view.units.len() - write_start {
        set_error(format!(
            "SystemError: Cannot write {how_many} characters at {write_start} in a string of \
             {} characters",
            to_view.units.len()
        ));
        return -1;
    }
    if how_many == 0 {
        return 0;
    }
    if !unicode_is_modifiable(to) {
        set_error("SystemError: Cannot modify a string currently used");
        return -1;
    }

    let to_width = to_view.width;
    let to_maxchar = destination_maxchar(&to_view);
    if !to_view.raw {
        // Materialise the UCS storage the copy writes into. The layout is the
        // one `PyUnicode_DATA` would have exposed for this string.
        let byte_length = match to_view.units.len().checked_mul(to_width as usize) {
            Some(length) => length,
            None => {
                set_error("MemoryError: string is too large");
                return -1;
            }
        };
        let mut data = Vec::new();
        if data.try_reserve_exact(byte_length).is_err() {
            set_error("MemoryError: cannot allocate the string buffer");
            return -1;
        }
        for &unit in &to_view.units {
            push_unit(&mut data, to_width, unit);
        }
        // SAFETY: the payload was validated as a string above; the GIL
        // serialises access to the storage.
        unsafe {
            match kind_mut(to) {
                ShimKind::Str { raw, .. } => {
                    *raw = Some(RawText {
                        width: to_width,
                        data,
                    });
                }
                _ => {
                    set_error("SystemError: PyUnicode_CopyCharacters requires a str destination");
                    return -1;
                }
            }
        }
    }

    let mut overflowed = false;
    // SAFETY: the destination is a string with `PyUnicode_New` storage and the
    // bounds were checked against its code points.
    unsafe {
        let raw = match kind_mut(to) {
            ShimKind::Str { raw: Some(raw), .. } => raw,
            _ => {
                set_error("SystemError: PyUnicode_CopyCharacters requires writable string storage");
                return -1;
            }
        };
        for offset in 0..how_many {
            let code = from_view.units[from_start as usize + offset];
            if code > to_maxchar {
                overflowed = true;
                break;
            }
            write_unit(&mut raw.data, raw.width, write_start + offset, code);
        }
    }
    // Refresh the text payload and drop any UTF-8 view cached for the old
    // contents.
    // SAFETY: `to` is a live shim object.
    unsafe { refresh_raw_unicode(to) };
    drop_buffer_cache_entry(to as usize);
    if overflowed {
        set_error(format!(
            "SystemError: Cannot copy {} characters into a string of {} characters",
            kind_name(from_view.width, &from_view.units),
            kind_name_from_maxchar(to_width, to_maxchar)
        ));
        return -1;
    }
    how_many as isize
}

/// `PyUnicode_Resize`: resizes a string in place, replacing it when shared.
///
/// New code points are zero-filled, the same initialisation `PyUnicode_New`
/// performs in this shim. A string whose reference count is not one is
/// replaced by a copy of the requested length, exactly like
/// `unicode_resize()`; `*unicode` is updated and the old reference released.
#[no_mangle]
pub unsafe extern "C" fn PyUnicode_Resize(unicode: *mut *mut ShimObject, length: isize) -> c_int {
    if unicode.is_null() {
        bad_internal_call();
        return -1;
    }
    // SAFETY: the caller passes a writable `PyObject *` slot.
    let object = unsafe { *unicode };
    if object.is_null() || length < 0 {
        bad_internal_call();
        return -1;
    }
    let view = match string_view(object) {
        Ok(Some(view)) => view,
        Ok(None) => {
            bad_internal_call();
            return -1;
        }
        Err(()) => return -1,
    };
    let StringView { width, units, raw } = view;
    let Ok(new_length) = usize::try_from(length) else {
        bad_internal_call();
        return -1;
    };
    let old_length = units.len();
    if old_length == new_length {
        return 0;
    }
    if new_length == 0 {
        // CPython substitutes the empty singleton for an empty result.
        // SAFETY: validated slot; the caller's reference is released.
        unsafe { *unicode = interned().empty_str as *mut ShimObject };
        dec_ref(object);
        return 0;
    }

    if !unicode_is_modifiable(object) {
        let prefix = old_length.min(new_length);
        let mut new_units = Vec::new();
        if new_units.try_reserve_exact(new_length).is_err() {
            set_error("MemoryError: cannot allocate the resized string");
            return -1;
        }
        new_units.extend_from_slice(&units[..prefix]);
        new_units.resize(new_length, 0);
        let replacement = new_raw_unicode(width, &new_units);
        // (A shared string is replaced by a copy of the requested length.)
        if replacement.is_null() {
            return -1;
        }
        // SAFETY: validated slot; the caller's reference is released exactly
        // once, as `Py_SETREF` does.
        unsafe { *unicode = replacement };
        dec_ref(object);
        return 0;
    }

    let byte_length = match new_length.checked_mul(width as usize) {
        Some(byte_length) => byte_length,
        None => {
            set_error("MemoryError: string is too large");
            return -1;
        }
    };
    if raw {
        // Grow or shrink the existing UCS buffer without disturbing the
        // object's identity.
        let resized = unsafe {
            match kind_mut(object) {
                ShimKind::Str { raw: Some(raw), .. } if raw.width == width => {
                    if new_length > old_length {
                        let additional = byte_length.saturating_sub(raw.data.len());
                        if raw.data.try_reserve(additional).is_err() {
                            Some(false)
                        } else {
                            raw.data.resize(byte_length, 0);
                            Some(true)
                        }
                    } else {
                        raw.data.truncate(byte_length);
                        Some(true)
                    }
                }
                _ => None,
            }
        };
        match resized {
            Some(true) => {
                // SAFETY: `object` is a live shim object.
                unsafe { refresh_raw_unicode(object) };
                drop_buffer_cache_entry(object as usize);
                return 0;
            }
            Some(false) => {
                set_error("MemoryError: cannot grow the string buffer");
                return -1;
            }
            None => {}
        }
    }

    // A `String`-backed string: build the UCS storage for the new length.
    let mut new_units = units;
    if new_length > old_length {
        if new_units
            .try_reserve_exact(new_length - old_length)
            .is_err()
        {
            set_error("MemoryError: cannot allocate the resized string");
            return -1;
        }
        new_units.resize(new_length, 0);
    } else {
        new_units.truncate(new_length);
    }
    let mut data = Vec::new();
    if data.try_reserve_exact(byte_length).is_err() {
        set_error("MemoryError: cannot allocate the string buffer");
        return -1;
    }
    for &unit in &new_units {
        push_unit(&mut data, width, unit);
    }
    // SAFETY: the payload was validated as a string above.
    unsafe {
        if let ShimKind::Str { raw, .. } = kind_mut(object) {
            *raw = Some(RawText { width, data });
        }
    }
    // SAFETY: `object` is a live shim object.
    unsafe { refresh_raw_unicode(object) };
    drop_buffer_cache_entry(object as usize);
    0
}

// ---------------------------------------------------------------------------
// Comparison and search
// ---------------------------------------------------------------------------

/// `PyUnicode_Equal`: 1 when both arguments are equal strings, 0 when they
/// differ and `-1` with a `TypeError` when either is not a string.
#[no_mangle]
pub unsafe extern "C" fn PyUnicode_Equal(str1: *mut ShimObject, str2: *mut ShimObject) -> c_int {
    if !matches!(kind_ref(str1), Some(ShimKind::Str { .. })) {
        set_error(format!(
            "TypeError: first argument must be str, not {}",
            object_type_name(str1)
        ));
        return -1;
    }
    if !matches!(kind_ref(str2), Some(ShimKind::Str { .. })) {
        set_error(format!(
            "TypeError: second argument must be str, not {}",
            object_type_name(str2)
        ));
        return -1;
    }
    let left = require_string_view(str1);
    let Some(left) = left else { return -1 };
    let right = require_string_view(str2);
    let Some(right) = right else { return -1 };
    (left.units == right.units) as c_int
}

/// Clamps a `start`/`end` pair the way CPython's `ADJUST_INDICES` does.
fn adjust_find_indices(length: usize, start: isize, end: isize) -> (isize, isize) {
    let length = length as isize;
    let mut start = start;
    let mut end = end;
    if end > length {
        end = length;
    } else if end < 0 {
        end += length;
        if end < 0 {
            end = 0;
        }
    }
    if start < 0 {
        start += length;
        if start < 0 {
            start = 0;
        }
    }
    (start, end)
}

/// `PyUnicode_Find`: locates `substr` in `str[start:end]`.
///
/// Returns the match's code point index, `-1` when it is absent and `-2` with
/// an exception set when an argument is not a string, matching
/// `PyUnicode_Find`'s documented `-2` error return. `direction > 0` searches
/// forward; zero and negative directions search backward.
#[no_mangle]
pub unsafe extern "C" fn PyUnicode_Find(
    string: *mut ShimObject,
    substr: *mut ShimObject,
    start: isize,
    end: isize,
    direction: c_int,
) -> isize {
    let haystack = match require_string_view(string) {
        Some(view) => view.units,
        None => return -2,
    };
    let needle = match require_string_view(substr) {
        Some(view) => view.units,
        None => return -2,
    };
    let (start, end) = adjust_find_indices(haystack.len(), start, end);
    if end - start < needle.len() as isize {
        return -1;
    }
    let start = start as usize;
    let end = end as usize;
    if needle.is_empty() {
        return if direction > 0 {
            start as isize
        } else {
            end as isize
        };
    }
    let last = end - needle.len();
    let found = if direction > 0 {
        (start..=last).find(|&position| haystack[position..position + needle.len()] == needle[..])
    } else {
        (start..=last)
            .rev()
            .find(|&position| haystack[position..position + needle.len()] == needle[..])
    };
    found.map_or(-1, |position| position as isize)
}

/// `PyUnicode_FindChar`: locates a code point in `str[start:end]`.
///
/// Returns the index, `-1` when not found; a non-string argument raises
/// `TypeError` and also returns `-1` (CPython requires a string here).
#[no_mangle]
pub unsafe extern "C" fn PyUnicode_FindChar(
    string: *mut ShimObject,
    character: c_uint,
    start: isize,
    end: isize,
    direction: c_int,
) -> isize {
    let haystack = match require_string_view(string) {
        Some(view) => view.units,
        None => return -1,
    };
    let (start, end) = adjust_find_indices(haystack.len(), start, end);
    if end - start < 1 {
        return -1;
    }
    let window = &haystack[start as usize..end as usize];
    let found = if direction > 0 {
        window.iter().position(|&code| code == character)
    } else {
        window.iter().rposition(|&code| code == character)
    };
    found.map_or(-1, |position| (start as usize + position) as isize)
}

// ---------------------------------------------------------------------------
// Splitting and joining
// ---------------------------------------------------------------------------

/// CPython's `Py_UNICODE_ISSPACE` table.
fn cp_isspace(code: u32) -> bool {
    matches!(
        code,
        0x09..=0x0D
            | 0x1C..=0x1F
            | 0x20
            | 0x85
            | 0xA0
            | 0x1680
            | 0x2000..=0x200A
            | 0x2028
            | 0x2029
            | 0x202F
            | 0x205F
            | 0x3000
    )
}

/// First occurrence of `needle` in `haystack` at or after `from`.
fn find_subslice(haystack: &[u32], needle: &[u32], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() || needle.len() > haystack.len() - from {
        return None;
    }
    (from..=haystack.len() - needle.len())
        .find(|&position| haystack[position..position + needle.len()] == needle[..])
}

/// Wraps `object` in a fresh list as its only element, transferring a new
/// reference.
fn list_with_single(object: *mut ShimObject) -> *mut ShimObject {
    inc_ref(object);
    new_object(ShimKind::List(vec![object as usize]))
}

/// Appends one split field, reporting a failed growth as `MemoryError`.
fn push_split_field(fields: &mut Vec<(usize, usize)>, field: (usize, usize)) -> bool {
    if fields.try_reserve(1).is_err() {
        set_error("MemoryError: cannot allocate the split list");
        return false;
    }
    fields.push(field);
    true
}

/// `PyUnicode_Split`: splits on `sep`, or on runs of whitespace when `sep` is
/// NULL. `maxsplit < 0` means "no limit".
#[no_mangle]
pub unsafe extern "C" fn PyUnicode_Split(
    s: *mut ShimObject,
    sep: *mut ShimObject,
    maxsplit: isize,
) -> *mut ShimObject {
    let units = match require_string_view(s) {
        Some(view) => view.units,
        None => return ptr::null_mut(),
    };
    let separator = if sep.is_null() {
        None
    } else {
        match require_string_view(sep) {
            Some(view) => Some(view.units),
            None => return ptr::null_mut(),
        }
    };

    let explicit_separator = !sep.is_null();
    let mut fields: Vec<(usize, usize)> = Vec::new();
    match separator {
        None => {
            let limit = if maxsplit < 0 {
                ((units.len() as isize - 1) / 2 + 1) as usize
            } else {
                maxsplit as usize
            };
            let length = units.len();
            let mut i = 0usize;
            let mut remaining = limit;
            while remaining > 0 {
                remaining -= 1;
                while i < length && cp_isspace(units[i]) {
                    i += 1;
                }
                if i == length {
                    break;
                }
                let token_start = i;
                i += 1;
                while i < length && !cp_isspace(units[i]) {
                    i += 1;
                }
                if token_start == 0 && i == length {
                    // No whitespace at all: CPython returns the string itself.
                    return list_with_single(s);
                }
                if !push_split_field(&mut fields, (token_start, i)) {
                    return ptr::null_mut();
                }
            }
            if i < length {
                while i < length && cp_isspace(units[i]) {
                    i += 1;
                }
                if i != length && !push_split_field(&mut fields, (i, length)) {
                    return ptr::null_mut();
                }
            }
        }
        Some(separator) => {
            if separator.is_empty() {
                set_error("ValueError: empty separator");
                return ptr::null_mut();
            }
            let limit = if maxsplit < 0 {
                usize::MAX
            } else {
                maxsplit as usize
            };
            let length = units.len();
            let mut start = 0usize;
            let mut splits = 0usize;
            while splits < limit {
                let Some(position) = find_subslice(&units, &separator, start) else {
                    break;
                };
                if !push_split_field(&mut fields, (start, position)) {
                    return ptr::null_mut();
                }
                start = position + separator.len();
                splits += 1;
            }
            if !push_split_field(&mut fields, (start, length)) {
                return ptr::null_mut();
            }
        }
    }

    if explicit_separator && fields.len() == 1 && fields[0] == (0, units.len()) {
        // Nothing was split: CPython returns the original string in the list.
        // (The whitespace path only reuses the original from inside its token
        // loop; with `maxsplit=0` it copies instead.)
        return list_with_single(s);
    }
    let mut items: Vec<usize> = Vec::new();
    if items.try_reserve_exact(fields.len()).is_err() {
        set_error("MemoryError: cannot allocate the split list");
        return ptr::null_mut();
    }
    for (start, end) in fields {
        let piece = new_unicode_from_units(&units[start..end]);
        if piece.is_null() {
            for item in items {
                dec_ref(item as *mut ShimObject);
            }
            return ptr::null_mut();
        }
        items.push(piece as usize);
    }
    new_object(ShimKind::List(items))
}

/// `PyUnicode_Join`: concatenates the strings in `seq` with `separator`.
///
/// A NULL separator falls back to a single space (CPython's behaviour for the
/// C API), empty sequences produce the empty string and a one-element sequence
/// returns that element itself with a new reference.
#[no_mangle]
pub unsafe extern "C" fn PyUnicode_Join(
    separator: *mut ShimObject,
    seq: *mut ShimObject,
) -> *mut ShimObject {
    if seq.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    let fseq = PySequence_Fast(seq, c"can only join an iterable".as_ptr());
    if fseq.is_null() {
        return ptr::null_mut();
    }
    let result = unsafe { join_unicode_sequence(separator, fseq) };
    dec_ref(fseq);
    result
}

/// Joining logic behind [`PyUnicode_Join`]; `fseq` is a borrowed list/tuple.
unsafe fn join_unicode_sequence(
    separator: *mut ShimObject,
    fseq: *mut ShimObject,
) -> *mut ShimObject {
    let items: &'static [usize] = match kind_ref(fseq) {
        Some(ShimKind::List(items)) => items.as_slice(),
        Some(ShimKind::Tuple(items)) => items.as_slice(),
        _ => {
            set_error("SystemError: PyUnicode_Join requires a sequence");
            return ptr::null_mut();
        }
    };
    if items.is_empty() {
        return interned().empty_str as *mut ShimObject;
    }
    if items.len() == 1 {
        let item = items[0] as *mut ShimObject;
        if matches!(kind_ref(item), Some(ShimKind::Str { .. })) {
            inc_ref(item);
            return item;
        }
        // CPython reports the item before it ever looks at the separator.
        set_error(format!(
            "TypeError: sequence item 0: expected str instance, {} found",
            object_type_name(item)
        ));
        return ptr::null_mut();
    }
    let (separator_text, separator_chars): (&'static str, usize) = if separator.is_null() {
        (" ", 1)
    } else {
        match kind_ref(separator) {
            Some(ShimKind::Str { text, .. }) => (text.as_str(), text.chars().count()),
            _ => {
                set_error(format!(
                    "TypeError: separator: expected str instance, {} found",
                    object_type_name(separator)
                ));
                return ptr::null_mut();
            }
        }
    };

    let mut total_chars = 0usize;
    let mut total_bytes = 0usize;
    for (index, &item) in items.iter().enumerate() {
        let text = match kind_ref(item as *mut ShimObject) {
            Some(ShimKind::Str { text, .. }) => text.as_str(),
            _ => {
                set_error(format!(
                    "TypeError: sequence item {index}: expected str instance, {} found",
                    object_type_name(item as *mut ShimObject)
                ));
                return ptr::null_mut();
            }
        };
        if index != 0 {
            let Some(chars) = total_chars.checked_add(separator_chars) else {
                set_error("OverflowError: join() result is too long for a Python string");
                return ptr::null_mut();
            };
            let Some(bytes) = total_bytes.checked_add(separator_text.len()) else {
                set_error("OverflowError: join() result is too long for a Python string");
                return ptr::null_mut();
            };
            total_chars = chars;
            total_bytes = bytes;
        }
        let Some(chars) = total_chars.checked_add(text.chars().count()) else {
            set_error("OverflowError: join() result is too long for a Python string");
            return ptr::null_mut();
        };
        let Some(bytes) = total_bytes.checked_add(text.len()) else {
            set_error("OverflowError: join() result is too long for a Python string");
            return ptr::null_mut();
        };
        if chars > isize::MAX as usize {
            set_error("OverflowError: join() result is too long for a Python string");
            return ptr::null_mut();
        }
        total_chars = chars;
        total_bytes = bytes;
    }

    let mut joined = String::new();
    if joined.try_reserve_exact(total_bytes).is_err() {
        set_error("MemoryError: cannot allocate the joined string");
        return ptr::null_mut();
    }
    for (index, &item) in items.iter().enumerate() {
        if index != 0 {
            joined.push_str(separator_text);
        }
        if let Some(ShimKind::Str { text, .. }) = kind_ref(item as *mut ShimObject) {
            joined.push_str(text);
        }
    }
    new_str(joined)
}

/// `PyBytes_Join`: concatenates bytes-like objects with a bytes separator.
///
/// Items may be any object exporting the buffer protocol (`bytes`,
/// `bytearray` and `memoryview` in this shim). References are taken on a
/// snapshot of the sequence so an exporter running during buffer acquisition
/// cannot invalidate the handles being read.
#[no_mangle]
pub unsafe extern "C" fn PyBytes_Join(
    sep: *mut ShimObject,
    iterable: *mut ShimObject,
) -> *mut ShimObject {
    if sep.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    if !matches!(kind_ref(sep), Some(ShimKind::Bytes { .. })) {
        set_error(format!(
            "TypeError: sep: expected bytes, got {}",
            object_type_name(sep)
        ));
        return ptr::null_mut();
    }
    if iterable.is_null() {
        bad_internal_call();
        return ptr::null_mut();
    }
    let fseq = PySequence_Fast(iterable, c"can only join an iterable".as_ptr());
    if fseq.is_null() {
        return ptr::null_mut();
    }
    let result = unsafe { join_bytes_sequence(sep, fseq) };
    dec_ref(fseq);
    result
}

/// Owned handles snapshot that releases its references when dropped.
struct HandleSnapshot(Vec<usize>);

impl Drop for HandleSnapshot {
    fn drop(&mut self) {
        for &item in &self.0 {
            dec_ref(item as *mut ShimObject);
        }
    }
}

/// Appends one piece (and, when it is not the first, the separator) while
/// enforcing CPython's `PY_SSIZE_T_MAX` result limit.
fn append_joined_bytes(
    separator: &[u8],
    out: &mut Vec<u8>,
    total: &mut usize,
    index: usize,
    piece: &[u8],
) -> Result<(), ()> {
    let mut addition = piece.len();
    if index != 0 {
        addition = match addition.checked_add(separator.len()) {
            Some(addition) => addition,
            None => {
                set_error("OverflowError: join() result is too long");
                return Err(());
            }
        };
    }
    let new_total = match total.checked_add(addition) {
        Some(new_total) if new_total <= isize::MAX as usize => new_total,
        _ => {
            set_error("OverflowError: join() result is too long");
            return Err(());
        }
    };
    if out.try_reserve(addition).is_err() {
        set_error("MemoryError: cannot allocate the joined bytes");
        return Err(());
    }
    if index != 0 {
        out.extend_from_slice(separator);
    }
    out.extend_from_slice(piece);
    *total = new_total;
    Ok(())
}

/// Joining logic behind [`PyBytes_Join`]; `fseq` is a borrowed list/tuple.
unsafe fn join_bytes_sequence(sep: *mut ShimObject, fseq: *mut ShimObject) -> *mut ShimObject {
    let separator: &'static [u8] = match kind_ref(sep) {
        Some(ShimKind::Bytes { data, .. }) => data.as_slice(),
        _ => {
            set_error(format!(
                "TypeError: sep: expected bytes, got {}",
                object_type_name(sep)
            ));
            return ptr::null_mut();
        }
    };
    let items: &'static [usize] = match kind_ref(fseq) {
        Some(ShimKind::List(items)) => items.as_slice(),
        Some(ShimKind::Tuple(items)) => items.as_slice(),
        _ => {
            set_error("SystemError: PyBytes_Join requires a sequence");
            return ptr::null_mut();
        }
    };
    let mut handles: Vec<usize> = Vec::new();
    if handles.try_reserve_exact(items.len()).is_err() {
        set_error("MemoryError: cannot allocate the join sequence");
        return ptr::null_mut();
    }
    handles.extend_from_slice(items);
    if handles.is_empty() {
        return interned().empty_bytes as *mut ShimObject;
    }
    if handles.len() == 1 {
        let item = handles[0] as *mut ShimObject;
        if matches!(kind_ref(item), Some(ShimKind::Bytes { .. })) {
            inc_ref(item);
            return item;
        }
    }
    // Take a reference on every handle: a buffer exporter may mutate the
    // sequence (and drop its last reference to an item) while we work.
    let mut snapshot_handles: Vec<usize> = Vec::new();
    if snapshot_handles.try_reserve_exact(handles.len()).is_err() {
        set_error("MemoryError: cannot allocate the join sequence");
        return ptr::null_mut();
    }
    for &item in &handles {
        inc_ref(item as *mut ShimObject);
        snapshot_handles.push(item);
    }
    let snapshot = HandleSnapshot(snapshot_handles);

    let mut out: Vec<u8> = Vec::new();
    let mut total = 0usize;
    for (index, &item) in snapshot.0.iter().enumerate() {
        let item = item as *mut ShimObject;
        match kind_ref(item) {
            Some(ShimKind::Bytes { data, .. }) | Some(ShimKind::ByteArray { data, .. }) => {
                if append_joined_bytes(separator, &mut out, &mut total, index, data).is_err() {
                    return ptr::null_mut();
                }
            }
            _ => {
                let mut view: Py_buffer = unsafe { std::mem::zeroed() };
                if PyObject_GetBuffer(item, &mut view, PYBUF_SIMPLE) != 0 {
                    let _ = take_error();
                    set_error(format!(
                        "TypeError: sequence item {index}: expected a bytes-like object, {} \
                         found",
                        object_type_name(item)
                    ));
                    return ptr::null_mut();
                }
                let piece: &[u8] = if view.len > 0 && !view.buf.is_null() {
                    // SAFETY: the acquired buffer describes `view.len` readable
                    // bytes.
                    unsafe { std::slice::from_raw_parts(view.buf as *const u8, view.len as usize) }
                } else {
                    &[]
                };
                let status = append_joined_bytes(separator, &mut out, &mut total, index, piece);
                PyBuffer_Release(&mut view);
                if status.is_err() {
                    return ptr::null_mut();
                }
            }
        }
    }
    let current_length = match kind_ref(fseq) {
        Some(ShimKind::List(items)) => items.len(),
        Some(ShimKind::Tuple(items)) => items.len(),
        _ => snapshot.0.len(),
    };
    if current_length != snapshot.0.len() {
        set_error("RuntimeError: sequence changed size during iteration");
        return ptr::null_mut();
    }
    new_bytes_from_vec(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(object: *mut ShimObject) -> String {
        match kind_ref(object) {
            Some(ShimKind::Str { text, .. }) => text.clone(),
            _ => panic!("object is not a string: {:?}", take_error()),
        }
    }

    fn bytes_of(object: *mut ShimObject) -> Vec<u8> {
        match kind_ref(object) {
            Some(ShimKind::Bytes { data, .. }) => data.clone(),
            _ => panic!("object is not bytes: {:?}", take_error()),
        }
    }

    fn units_of(object: *mut ShimObject) -> Vec<u32> {
        match string_view(object) {
            Ok(Some(view)) => view.units,
            _ => panic!("object is not a string"),
        }
    }

    fn list_items(object: *mut ShimObject) -> Vec<*mut ShimObject> {
        match kind_ref(object) {
            Some(ShimKind::List(items)) => {
                items.iter().map(|&item| item as *mut ShimObject).collect()
            }
            _ => panic!("object is not a list"),
        }
    }

    #[test]
    fn decode_utf8_paths() {
        clear_error();
        let with_nul = [b'a', 0, b'b'];
        let decoded =
            unsafe { PyUnicode_DecodeUTF8(with_nul.as_ptr() as *const c_char, 3, ptr::null()) };
        assert!(!decoded.is_null(), "{:?}", take_error());
        assert_eq!(text_of(decoded), "a\0b");
        dec_ref(decoded);

        // Empty input returns the empty string before any handler lookup.
        let empty = unsafe { PyUnicode_DecodeUTF8(ptr::null(), 0, c"nope".as_ptr()) };
        assert!(!empty.is_null(), "{:?}", take_error());
        assert_eq!(text_of(empty), "");

        let invalid = [0xffu8];
        let bad =
            unsafe { PyUnicode_DecodeUTF8(invalid.as_ptr() as *const c_char, 1, ptr::null()) };
        assert!(bad.is_null());
        assert!(take_error()
            .unwrap_or_default()
            .starts_with("UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff"));

        let truncated_input = [0xf0u8, 0x9f];
        let truncated = unsafe {
            PyUnicode_DecodeUTF8(truncated_input.as_ptr() as *const c_char, 2, ptr::null())
        };
        assert!(truncated.is_null());
        assert!(take_error()
            .unwrap_or_default()
            .contains("can't decode bytes in position 0-1: unexpected end of data"));

        let replaced = unsafe {
            PyUnicode_DecodeUTF8(
                truncated_input.as_ptr() as *const c_char,
                2,
                c"replace".as_ptr(),
            )
        };
        assert!(!replaced.is_null(), "{:?}", take_error());
        assert_eq!(text_of(replaced), "\u{FFFD}");
        dec_ref(replaced);

        let escaped_byte = unsafe {
            PyUnicode_DecodeUTF8(
                invalid.as_ptr() as *const c_char,
                1,
                c"backslashreplace".as_ptr(),
            )
        };
        assert!(!escaped_byte.is_null(), "{:?}", take_error());
        assert_eq!(text_of(escaped_byte), "\\xff");
        dec_ref(escaped_byte);

        let escaped_input = [b'a', 0xff, b'b'];
        let escaped = unsafe {
            PyUnicode_DecodeUTF8(
                escaped_input.as_ptr() as *const c_char,
                3,
                c"surrogateescape".as_ptr(),
            )
        };
        assert!(!escaped.is_null(), "{:?}", take_error());
        assert_eq!(units_of(escaped), vec![0x61, 0xDCFF, 0x62]);
        dec_ref(escaped);

        let unknown =
            unsafe { PyUnicode_DecodeUTF8(invalid.as_ptr() as *const c_char, 1, c"nope".as_ptr()) };
        assert!(unknown.is_null());
        assert!(take_error()
            .unwrap_or_default()
            .starts_with("LookupError: unknown error handler name 'nope'"));

        // A valid input never resolves the handler, exactly like CPython.
        let valid_input = [b'h', 0xc3, 0xa9];
        let valid = unsafe {
            PyUnicode_DecodeUTF8(valid_input.as_ptr() as *const c_char, 3, c"nope".as_ptr())
        };
        assert!(!valid.is_null(), "{:?}", take_error());
        assert_eq!(text_of(valid), "hé");
        dec_ref(valid);

        let negative = unsafe { PyUnicode_DecodeUTF8(c"abc".as_ptr(), -1, ptr::null()) };
        assert!(negative.is_null());
        assert!(take_error().unwrap_or_default().starts_with("MemoryError"));
    }

    #[test]
    fn locale_codecs() {
        clear_error();
        let accented = [b'h', 0xc3, 0xa9];
        let decoded =
            unsafe { PyUnicode_DecodeLocale(accented.as_ptr() as *const c_char, ptr::null()) };
        assert!(!decoded.is_null(), "{:?}", take_error());
        assert_eq!(text_of(decoded), "hé");
        dec_ref(decoded);

        let escaped_input = [b'a', 0xff, b'b'];
        let escaped = unsafe {
            PyUnicode_DecodeLocale(
                escaped_input.as_ptr() as *const c_char,
                c"surrogateescape".as_ptr(),
            )
        };
        assert!(!escaped.is_null(), "{:?}", take_error());
        assert_eq!(units_of(escaped), vec![0x61, 0xDCFF, 0x62]);
        dec_ref(escaped);

        let bad_handler = unsafe { PyUnicode_DecodeLocale(c"".as_ptr(), c"nope".as_ptr()) };
        assert!(bad_handler.is_null());
        assert_eq!(
            take_error().unwrap_or_default(),
            "ValueError: unsupported error handler"
        );

        let text = new_str("hé".to_string());
        let encoded = unsafe { PyUnicode_EncodeLocale(text, ptr::null()) };
        assert!(!encoded.is_null(), "{:?}", take_error());
        assert_eq!(bytes_of(encoded), b"h\xc3\xa9".to_vec());
        dec_ref(encoded);
        dec_ref(text);

        let embedded = new_str("a\0b".to_string());
        let rejected = unsafe { PyUnicode_EncodeLocale(embedded, ptr::null()) };
        assert!(rejected.is_null());
        assert_eq!(
            take_error().unwrap_or_default(),
            "ValueError: embedded null character"
        );
        let not_text = new_int(1);
        let rejected = unsafe { PyUnicode_EncodeLocale(not_text, ptr::null()) };
        assert!(rejected.is_null());
        assert!(take_error().unwrap_or_default().starts_with("TypeError"));
        dec_ref(embedded);
        dec_ref(not_text);

        let empty = new_str(String::new());
        let encoded = unsafe { PyUnicode_EncodeLocale(empty, c"surrogateescape".as_ptr()) };
        assert!(!encoded.is_null(), "{:?}", take_error());
        assert!(bytes_of(encoded).is_empty());
        dec_ref(encoded);
    }

    #[test]
    fn ordinal_and_wide_characters() {
        clear_error();
        let ascii = unsafe { PyUnicode_FromOrdinal('A' as c_int) };
        assert!(!ascii.is_null(), "{:?}", take_error());
        assert_eq!(text_of(ascii), "A");
        dec_ref(ascii);

        let surrogate = unsafe { PyUnicode_FromOrdinal(0xD800) };
        assert!(!surrogate.is_null(), "{:?}", take_error());
        assert_eq!(units_of(surrogate), vec![0xD800]);
        assert!(matches!(
            kind_ref(surrogate),
            Some(ShimKind::Str {
                raw: Some(RawText { width: 2, .. }),
                ..
            })
        ));
        assert_eq!(text_of(surrogate), "\u{FFFD}");
        dec_ref(surrogate);

        assert!(unsafe { PyUnicode_FromOrdinal(-1) }.is_null());
        assert_eq!(
            take_error().unwrap_or_default(),
            "ValueError: chr() arg not in range(0x110000)"
        );

        let mut wide = [0x61i32, 0x6C, 0x100];
        let text = unsafe { PyUnicode_FromWideChar(wide.as_mut_ptr(), 3) };
        assert!(!text.is_null(), "{:?}", take_error());
        assert_eq!(text_of(text), "alĀ");
        dec_ref(text);

        let mut terminated = [0x61i32, 0x62, 0];
        let text = unsafe { PyUnicode_FromWideChar(terminated.as_mut_ptr(), -1) };
        assert!(!text.is_null(), "{:?}", take_error());
        assert_eq!(text_of(text), "ab");
        dec_ref(text);

        let empty = unsafe { PyUnicode_FromWideChar(ptr::null(), 0) };
        assert!(!empty.is_null(), "{:?}", take_error());
        assert_eq!(text_of(empty), "");

        assert!(unsafe { PyUnicode_FromWideChar(ptr::null(), 3) }.is_null());
        assert!(take_error().unwrap_or_default().starts_with("SystemError"));

        let mut invalid = [0x110000i32];
        assert!(unsafe { PyUnicode_FromWideChar(invalid.as_mut_ptr(), 1) }.is_null());
        assert_eq!(
            take_error().unwrap_or_default(),
            "ValueError: character U+110000 is not in range [U+0000; U+10ffff]"
        );
    }

    #[test]
    fn searching_and_equality() {
        clear_error();
        let haystack = new_str("abcabc".to_string());
        let needle = new_str("abc".to_string());
        assert_eq!(unsafe { PyUnicode_Find(haystack, needle, 0, 6, 1) }, 0);
        assert_eq!(unsafe { PyUnicode_Find(haystack, needle, 0, 6, -1) }, 3);
        // Direction zero searches backward, like CPython's `findchar`.
        assert_eq!(unsafe { PyUnicode_Find(haystack, needle, 0, 6, 0) }, 3);
        assert_eq!(unsafe { PyUnicode_Find(haystack, needle, 100, 200, 1) }, -1);
        let empty = new_str(String::new());
        assert_eq!(unsafe { PyUnicode_Find(haystack, empty, 1, 2, 1) }, 1);
        assert_eq!(unsafe { PyUnicode_Find(haystack, empty, 1, 2, -1) }, 2);
        let number = new_int(3);
        assert_eq!(unsafe { PyUnicode_Find(haystack, number, 0, 6, 1) }, -2);
        assert!(take_error().unwrap_or_default().starts_with("TypeError"));

        assert_eq!(
            unsafe { PyUnicode_FindChar(haystack, 'a' as c_uint, 0, 6, 1) },
            0
        );
        assert_eq!(
            unsafe { PyUnicode_FindChar(haystack, 'a' as c_uint, 0, 6, -1) },
            3
        );
        assert_eq!(
            unsafe { PyUnicode_FindChar(haystack, 'z' as c_uint, 0, 6, 1) },
            -1
        );

        assert_eq!(unsafe { PyUnicode_Equal(haystack, haystack) }, 1);
        assert_eq!(unsafe { PyUnicode_Equal(haystack, needle) }, 0);
        assert_eq!(unsafe { PyUnicode_Equal(number, needle) }, -1);
        assert!(take_error()
            .unwrap_or_default()
            .starts_with("TypeError: first argument must be str"));

        for object in [haystack, needle, empty, number] {
            dec_ref(object);
        }
    }

    #[test]
    fn split_paths() {
        clear_error();
        let text = new_str("a  b ".to_string());
        let parts = unsafe { PyUnicode_Split(text, ptr::null_mut(), -1) };
        assert!(!parts.is_null(), "{:?}", take_error());
        let items = list_items(parts);
        assert_eq!(items.len(), 2);
        assert_eq!(text_of(items[0]), "a");
        assert_eq!(text_of(items[1]), "b");
        dec_ref(parts);

        let parts = unsafe { PyUnicode_Split(text, ptr::null_mut(), 1) };
        assert!(!parts.is_null(), "{:?}", take_error());
        let items = list_items(parts);
        assert_eq!(items.len(), 2);
        assert_eq!(text_of(items[1]), "b ");
        dec_ref(parts);

        let parts = unsafe { PyUnicode_Split(text, ptr::null_mut(), 0) };
        assert!(!parts.is_null(), "{:?}", take_error());
        let items = list_items(parts);
        assert_eq!(items.len(), 1);
        assert_eq!(text_of(items[0]), "a  b ");
        assert_ne!(items[0], text, "maxsplit 0 copies, as CPython does");
        dec_ref(parts);

        let comma = new_str(",".to_string());
        let csv = new_str("a,b,".to_string());
        let parts = unsafe { PyUnicode_Split(csv, comma, -1) };
        assert!(!parts.is_null(), "{:?}", take_error());
        let items = list_items(parts);
        assert_eq!(items.len(), 3);
        assert_eq!(text_of(items[2]), "");
        dec_ref(parts);

        let empty_sep = new_str(String::new());
        let rejected = unsafe { PyUnicode_Split(csv, empty_sep, -1) };
        assert!(rejected.is_null());
        assert_eq!(
            take_error().unwrap_or_default(),
            "ValueError: empty separator"
        );

        for object in [text, comma, csv, empty_sep] {
            dec_ref(object);
        }
    }

    #[test]
    fn join_paths() {
        clear_error();
        let sequence = new_object(ShimKind::List(vec![
            new_str("a".to_string()) as usize,
            new_str("b".to_string()) as usize,
        ]));
        let separator = new_str("-".to_string());
        let joined = unsafe { PyUnicode_Join(separator, sequence) };
        assert!(!joined.is_null(), "{:?}", take_error());
        assert_eq!(text_of(joined), "a-b");
        dec_ref(joined);

        let joined = unsafe { PyUnicode_Join(ptr::null_mut(), sequence) };
        assert!(!joined.is_null(), "{:?}", take_error());
        assert_eq!(text_of(joined), "a b");
        dec_ref(joined);

        // A one-element sequence returns that element itself.
        let single = new_str("solo".to_string());
        let one = new_object(ShimKind::List(vec![single as usize]));
        let joined = unsafe { PyUnicode_Join(separator, one) };
        assert_eq!(joined, single, "singleton joins return the item itself");
        dec_ref(joined);
        dec_ref(one);

        // A non-string element is reported before the separator is checked.
        let number = new_int(7);
        let mixed = new_object(ShimKind::List(vec![
            new_str("a".to_string()) as usize,
            number as usize,
        ]));
        inc_ref(number);
        let rejected = unsafe { PyUnicode_Join(separator, mixed) };
        assert!(rejected.is_null());
        assert_eq!(
            take_error().unwrap_or_default(),
            "TypeError: sequence item 1: expected str instance, int found"
        );
        dec_ref(mixed);

        // Empty sequences produce the empty string even with a bad separator.
        let empty = new_object(ShimKind::List(Vec::new()));
        let joined = unsafe { PyUnicode_Join(number, empty) };
        assert!(!joined.is_null(), "{:?}", take_error());
        assert_eq!(text_of(joined), "");
        dec_ref(joined);
        dec_ref(empty);
        dec_ref(number);

        dec_ref(sequence);
        dec_ref(separator);
    }

    #[test]
    fn bytes_join_paths() {
        clear_error();
        let sequence = new_object(ShimKind::List(vec![
            new_bytes(b"a".to_vec()) as usize,
            new_object(ShimKind::ByteArray {
                data: b"b".to_vec(),
                c_data: b"b\0".to_vec(),
            }) as usize,
        ]));
        let separator = new_bytes(b"-".to_vec());
        let joined = unsafe { PyBytes_Join(separator, sequence) };
        assert!(!joined.is_null(), "{:?}", take_error());
        assert_eq!(bytes_of(joined), b"a-b".to_vec());
        dec_ref(joined);

        let text = new_str("x".to_string());
        let bad = unsafe { PyBytes_Join(text, sequence) };
        assert!(bad.is_null());
        assert_eq!(
            take_error().unwrap_or_default(),
            "TypeError: sep: expected bytes, got str"
        );

        let words = new_object(ShimKind::List(vec![
            new_bytes(b"c".to_vec()) as usize,
            text as usize,
        ]));
        inc_ref(text);
        let rejected = unsafe { PyBytes_Join(separator, words) };
        assert!(rejected.is_null());
        assert_eq!(
            take_error().unwrap_or_default(),
            "TypeError: sequence item 1: expected a bytes-like object, str found"
        );
        dec_ref(words);
        dec_ref(text);

        let empty = new_object(ShimKind::List(Vec::new()));
        let joined = unsafe { PyBytes_Join(separator, empty) };
        assert!(!joined.is_null(), "{:?}", take_error());
        assert!(bytes_of(joined).is_empty());
        dec_ref(joined);

        dec_ref(empty);
        dec_ref(sequence);
        dec_ref(separator);
    }

    #[test]
    fn copy_characters_and_resize() {
        clear_error();
        let source = new_str("ab".to_string());
        let target = PyUnicode_New(3, 0xFF);
        assert!(!target.is_null(), "{:?}", take_error());
        assert_eq!(
            unsafe { PyUnicode_CopyCharacters(target, 0, source, 0, 2) },
            2
        );
        assert_eq!(text_of(target), "ab\0");

        // In-place resize of the `PyUnicode_New` buffer keeps the object.
        let mut slot = target;
        assert_eq!(unsafe { PyUnicode_Resize(&mut slot, 5) }, 0);
        assert_eq!(slot, target);
        assert_eq!(text_of(target), "ab\0\0\0");
        let mut slot = target;
        assert_eq!(unsafe { PyUnicode_Resize(&mut slot, 1) }, 0);
        assert_eq!(text_of(target), "a");

        // Copying a code point that does not fit the destination kind fails.
        let wide = new_str("€".to_string());
        let ascii_target = PyUnicode_New(2, 0x7F);
        assert_eq!(
            unsafe { PyUnicode_CopyCharacters(ascii_target, 0, wide, 0, 1) },
            -1
        );
        let message = take_error().unwrap_or_default();
        assert!(message.contains("Cannot copy UCS2 characters"), "{message}");
        dec_ref(ascii_target);
        dec_ref(wide);

        // A `PyUnicode_New` latin1 buffer accepts latin1 code points; a
        // `String`-backed ASCII string does not.
        let latin1 = new_str("é".to_string());
        let latin_target = PyUnicode_New(2, 0xFF);
        assert_eq!(
            unsafe { PyUnicode_CopyCharacters(latin_target, 0, latin1, 0, 1) },
            1
        );
        assert_eq!(text_of(latin_target), "é\0");
        dec_ref(latin_target);
        let ascii_text = new_str("ab".to_string());
        assert_eq!(
            unsafe { PyUnicode_CopyCharacters(ascii_text, 0, latin1, 0, 1) },
            -1
        );
        assert_eq!(
            take_error().unwrap_or_default(),
            "SystemError: Cannot copy latin1 characters into a string of ascii characters"
        );
        dec_ref(ascii_text);
        dec_ref(latin1);

        // A shared string is replaced by a copy of the requested length.
        let shared = new_str("abc".to_string());
        inc_ref(shared);
        let mut slot = shared;
        assert_eq!(unsafe { PyUnicode_Resize(&mut slot, 5) }, 0);
        assert_ne!(slot, shared);
        assert_eq!(text_of(slot), "abc\0\0");
        dec_ref(shared);
        dec_ref(slot);

        // Bad arguments never panic.
        let mut not_text = new_int(1);
        assert_eq!(unsafe { PyUnicode_Resize(&mut not_text, 2) }, -1);
        assert!(take_error().unwrap_or_default().starts_with("SystemError"));
        let mut text = new_str("abc".to_string());
        assert_eq!(unsafe { PyUnicode_Resize(&mut text, -1) }, -1);
        assert!(take_error().unwrap_or_default().starts_with("SystemError"));
        assert_eq!(
            unsafe { PyUnicode_CopyCharacters(text, 0, source, 0, -1) },
            -1
        );
        assert_eq!(
            take_error().unwrap_or_default(),
            "SystemError: how_many cannot be negative"
        );
        assert_eq!(
            unsafe { PyUnicode_CopyCharacters(text, 9, source, 0, 1) },
            -1
        );
        assert_eq!(
            take_error().unwrap_or_default(),
            "IndexError: string index out of range"
        );
        // A zero-length copy on a shared string is allowed.
        let shared_text = new_str("shared".to_string());
        inc_ref(shared_text);
        assert_eq!(
            unsafe { PyUnicode_CopyCharacters(shared_text, 0, source, 0, 0) },
            0
        );
        assert_eq!(
            unsafe { PyUnicode_CopyCharacters(shared_text, 0, source, 0, 1) },
            -1
        );
        assert_eq!(
            take_error().unwrap_or_default(),
            "SystemError: Cannot modify a string currently used"
        );
        dec_ref(shared_text);
        dec_ref(shared_text);

        dec_ref(source);
        dec_ref(target);
        dec_ref(not_text);
        dec_ref(text);
    }
}
