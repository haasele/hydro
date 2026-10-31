//! `_struct`: pack and unpack C-style binary records.
//!
//! Supports the format characters `x b B h H i I l L q Q f d ? s` with the
//! byte-order/alignment prefixes `@ = < > !`, repeat counts and native
//! alignment for `@`. Sizes follow CPython's `struct` module: standard sizes
//! (4-byte `l`/`L`) for `= < > !`, C sizes and alignment for `@`.
//!
//! `struct.py` in the standard library is `from _struct import *`, so the
//! module exposes the same names: `pack`, `pack_into`, `unpack`,
//! `unpack_from`, `iter_unpack`, `calcsize`, `_clearcache`, the `Struct`
//! class and the `error` exception class.
//!
//! Parsing and packing never panic: every malformed format, wrong value type,
//! out-of-range integer, short buffer or truncated `s` field raises
//! `_struct.error` (a subclass of `Exception`) and returns `None`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use hydro_ir::MethodKind;
use num_traits::ToPrimitive;

use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, EXCEPTION, TYPE_ERROR};
use crate::value::{self, NONE};

/// Maximum repeat count accepted for one format item; larger counts would
/// only produce absurd buffer sizes.
const MAX_COUNT: usize = 1 << 30;

/// Largest total struct size accepted. A larger result is reported as
/// `struct.error` instead of attempting an absurd allocation.
const MAX_STRUCT_SIZE: usize = 1 << 26;

/// Number of parsed formats kept in the per-thread cache.
const FORMAT_CACHE_LIMIT: usize = 64;

/// Byte order used to encode multi-byte fields.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    Little,
    Big,
}

fn native_order() -> Order {
    if cfg!(target_endian = "little") {
        Order::Little
    } else {
        Order::Big
    }
}

/// One parsed format item, with its resolved byte offset.
struct Item {
    code: u8,
    count: usize,
    offset: usize,
}

/// A fully parsed format string.
struct Parsed {
    items: Vec<Item>,
    size: usize,
    order: Order,
    /// True for the standard-size prefixes (`= < > !`).
    standard: bool,
}

thread_local! {
    /// Parsed formats keyed by their source text. `_clearcache` empties it.
    static FORMAT_CACHE: RefCell<HashMap<String, Rc<Parsed>>> =
        RefCell::new(HashMap::new());
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

/// Raises `_struct.error` with `message` and returns `None`.
fn struct_error(message: &str) -> i64 {
    let message_value = value::str_alloc(message.to_string());
    let class = error_class();
    let instance = crate::builtins::call(class, 1, [message_value].as_ptr());
    if exc::pending() {
        return NONE;
    }
    exc::raise_value(instance)
}

/// The cached `_struct.error` class: a subclass of `Exception`, so
/// `except struct.error:` and `except Exception:` both catch it.
fn error_class() -> i64 {
    if let Some(class) = globals::registry_get("_struct.error") {
        return class;
    }
    let class = value::class_new(
        "error".to_string(),
        exc::exception_class(EXCEPTION),
        Vec::new(),
    );
    globals::registry_set("_struct.error", class);
    class
}

/// One native method value belonging to the `Struct` class.
fn method(name: &'static str) -> (String, i64, MethodKind) {
    (
        name.rsplit('.').next().unwrap_or(name).to_string(),
        value::native_function_new("_struct", name, usize::MAX),
        MethodKind::Normal,
    )
}

/// The cached `_struct.Struct` class. Instances carry `format` and `size`
/// attributes; every operation reparses the format through [`parsed_format`],
/// so the instance state is fully observably Python data.
fn struct_class() -> i64 {
    if let Some(class) = globals::registry_get("_struct.Struct") {
        return class;
    }
    let methods = vec![
        method("Struct.__init__"),
        method("Struct.pack"),
        method("Struct.pack_into"),
        method("Struct.unpack"),
        method("Struct.unpack_from"),
        method("Struct.iter_unpack"),
    ];
    let class = value::class_new_full("Struct".to_string(), NONE, methods, Vec::new());
    globals::registry_set("_struct.Struct", class);
    class
}

/// `_struct.error` as a class value.
pub(crate) fn error_value() -> i64 {
    error_class()
}

/// Class values exported by the module (`Struct`, `error`).
///
/// `mod.rs` calls this before the function/constant tables so
/// `struct.Struct` and `struct.error` resolve to real class objects.
pub(crate) fn class_value(module: &str, name: &str) -> Option<i64> {
    match (module, name) {
        ("_struct" | "struct", "Struct") => Some(struct_class()),
        ("_struct" | "struct", "error") => Some(error_class()),
        _ => None,
    }
}

fn code_size(code: u8, standard: bool) -> usize {
    match code {
        b'x' | b'b' | b'B' | b'?' | b's' => 1,
        b'h' | b'H' => 2,
        b'i' | b'I' | b'f' => 4,
        b'l' | b'L' => {
            if standard {
                4
            } else {
                std::mem::size_of::<libc::c_long>()
            }
        }
        b'q' | b'Q' | b'd' => 8,
        _ => 0,
    }
}

fn align_up(offset: usize, align: usize) -> Option<usize> {
    if align <= 1 {
        return Some(offset);
    }
    let mask = align - 1;
    offset.checked_add(mask).map(|value| value & !mask)
}

/// Parses `format` into items with offsets and the total size.
fn parse_format(format: &str) -> Result<Parsed, String> {
    if format.len() > 1_000_000 {
        return Err("struct format is too long".to_string());
    }
    let mut chars = format.chars();
    let prefix = format.as_bytes().first().copied();
    let (order, standard) = match prefix {
        Some(b'@') => (native_order(), false),
        Some(b'=') => (native_order(), true),
        Some(b'<') => (Order::Little, true),
        Some(b'>') | Some(b'!') => (Order::Big, true),
        _ => (native_order(), false),
    };
    if matches!(prefix, Some(b'@' | b'=' | b'<' | b'>' | b'!')) {
        chars.next();
    }
    let mut items = Vec::new();
    let mut offset = 0usize;
    let mut digits: Option<usize> = None;
    for ch in chars {
        if ch.is_ascii_digit() {
            let digit = (ch as u8 - b'0') as usize;
            let current = digits.unwrap_or(0);
            digits = Some(
                current
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(digit))
                    .filter(|value| *value <= MAX_COUNT)
                    .ok_or_else(|| "struct format repeat count is too large".to_string())?,
            );
            continue;
        }
        let code = ch as u32;
        if code > u32::from(u8::MAX) {
            return Err("bad char in struct format".to_string());
        }
        let code = code as u8;
        let size = code_size(code, standard);
        if size == 0 {
            return Err("bad char in struct format".to_string());
        }
        let count = digits.take().unwrap_or(1);
        let align = if standard || code == b'x' || code == b's' {
            1
        } else {
            size
        };
        offset = align_up(offset, align).ok_or_else(|| "struct format is too large".to_string())?;
        let end = count
            .checked_mul(size)
            .and_then(|length| offset.checked_add(length))
            .ok_or_else(|| "struct format is too large".to_string())?;
        items.push(Item {
            code,
            count,
            offset,
        });
        if end > MAX_STRUCT_SIZE {
            return Err("total struct size too long".to_string());
        }
        offset = end;
    }
    if digits.is_some() {
        return Err("bad char in struct format".to_string());
    }
    Ok(Parsed {
        items,
        size: offset,
        order,
        standard,
    })
}

/// Parses a format through the per-thread cache.
fn parsed_format(format: &str) -> Result<Rc<Parsed>, String> {
    if let Some(parsed) = FORMAT_CACHE.with(|cache| cache.borrow().get(format).cloned()) {
        return Ok(parsed);
    }
    let parsed = Rc::new(parse_format(format)?);
    FORMAT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= FORMAT_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(format.to_string(), Rc::clone(&parsed));
    });
    Ok(parsed)
}

/// Format argument: CPython accepts `str` or ASCII `bytes` for
/// `struct.calcsize`/`pack`/`unpack` and their `Struct` methods. Arbitrary
/// objects are rejected with the same `TypeError` CPython raises.
fn string_argument(value_arg: i64, name: &str) -> Result<String, i64> {
    if let Some(text) = value::as_str(value_arg) {
        return Ok(text.to_string());
    }
    if let Some(bytes) = value::as_bytes(value_arg) {
        return match std::str::from_utf8(bytes) {
            Ok(text) => Ok(text.to_string()),
            Err(_) => Err(type_error(&format!(
                "argument '{name}' must be an ASCII string, not bytes with non-ASCII data"
            ))),
        };
    }
    Err(type_error(&format!(
        "argument '{name}' must be str or bytes, not {}",
        value::type_name(value_arg)
    )))
}

/// Exact integer argument as `i128` so every struct range can be checked.
fn int_argument(value_arg: i64) -> Option<i128> {
    if let Some(n) = value::as_integer(value_arg) {
        return Some(i128::from(n));
    }
    value::as_bigint(value_arg).and_then(ToPrimitive::to_i128)
}

/// Numeric argument for `f`/`d` fields: ints (including BigInt) and floats.
fn float_argument(value_arg: i64) -> Option<f64> {
    if let Some(f) = value::as_float(value_arg) {
        return Some(f);
    }
    if let Some(n) = value::as_integer(value_arg) {
        return Some(n as f64);
    }
    value::as_bigint(value_arg).and_then(ToPrimitive::to_f64)
}

fn int_result(n: i64) -> i64 {
    value::encode_int(n).unwrap_or_else(|| value::bigint_new(num_bigint::BigInt::from(n)))
}

fn unsigned_result(n: u64) -> i64 {
    match i64::try_from(n) {
        Ok(n) => int_result(n),
        Err(_) => value::bigint_new(num_bigint::BigInt::from(n)),
    }
}

fn write_uint(buffer: &mut [u8], offset: usize, size: usize, value: u64, order: Order) {
    let bytes = match order {
        Order::Little => value.to_le_bytes(),
        Order::Big => value.to_be_bytes(),
    };
    let source = match order {
        Order::Little => &bytes[..size],
        Order::Big => &bytes[8 - size..],
    };
    buffer[offset..offset + size].copy_from_slice(source);
}

fn read_uint(buffer: &[u8], offset: usize, size: usize, order: Order) -> u64 {
    let mut bytes = [0u8; 8];
    match order {
        Order::Little => bytes[..size].copy_from_slice(&buffer[offset..offset + size]),
        Order::Big => bytes[8 - size..].copy_from_slice(&buffer[offset..offset + size]),
    }
    match order {
        Order::Little => u64::from_le_bytes(bytes),
        Order::Big => u64::from_be_bytes(bytes),
    }
}

fn read_signed(buffer: &[u8], offset: usize, size: usize, order: Order) -> i64 {
    let raw = read_uint(buffer, offset, size, order);
    if size == 8 {
        return raw as i64;
    }
    let shift = 64 - size * 8;
    ((raw << shift) as i64) >> shift
}

/// Packs `values` into the buffer described by `parsed`.
fn pack_values(parsed: &Parsed, values: &[i64]) -> Result<Vec<u8>, String> {
    let required: usize = parsed
        .items
        .iter()
        .map(|item| match item.code {
            b'x' => 0,
            b's' => 1,
            _ => item.count,
        })
        .sum();
    if required != values.len() {
        return Err(format!(
            "pack expected {required} items for packing (got {})",
            values.len()
        ));
    }
    let mut buffer = vec![0u8; parsed.size];
    let mut next = 0usize;
    for item in &parsed.items {
        if item.code == b'x' {
            continue;
        }
        let size = code_size(item.code, parsed.standard);
        if item.code == b's' {
            let value_arg = values[next];
            next += 1;
            let Some(bytes) = value::as_bytes(value_arg) else {
                return Err(format!(
                    "argument for 's' must be a bytes object, not {}",
                    value::type_name(value_arg)
                ));
            };
            if bytes.len() > item.count {
                return Err(format!(
                    "argument for 's' is too long: {} bytes for a {}-byte field",
                    bytes.len(),
                    item.count
                ));
            }
            // Trailing bytes stay zero, matching CPython's NUL padding.
            buffer[item.offset..item.offset + bytes.len()].copy_from_slice(bytes);
            continue;
        }
        for index in 0..item.count {
            let offset = item.offset + index * size;
            match item.code {
                b'?' => {
                    let value_arg = values[next];
                    next += 1;
                    buffer[offset] = u8::from(value::truthy(value_arg));
                }
                b'f' | b'd' => {
                    let value_arg = values[next];
                    next += 1;
                    let Some(number) = float_argument(value_arg) else {
                        return Err(format!(
                            "required argument is not a float, not {}",
                            value::type_name(value_arg)
                        ));
                    };
                    if item.code == b'f' {
                        let narrowed = number as f32;
                        if number.is_finite() && narrowed.is_infinite() {
                            return Err("float too large to pack with f format".to_string());
                        }
                        write_uint(
                            &mut buffer,
                            offset,
                            4,
                            u64::from(narrowed.to_bits()),
                            parsed.order,
                        );
                    } else {
                        write_uint(&mut buffer, offset, 8, number.to_bits(), parsed.order);
                    }
                }
                _ => {
                    let value_arg = values[next];
                    next += 1;
                    let Some(number) = int_argument(value_arg) else {
                        return Err("required argument is not an integer".to_string());
                    };
                    let (minimum, maximum) = integer_range(item.code, parsed.standard);
                    if number < minimum || number > maximum {
                        return Err(format!(
                            "'{}' format requires {minimum} <= number <= {maximum}",
                            item.code as char
                        ));
                    }
                    let raw = number as u128 as u64;
                    write_uint(&mut buffer, offset, size, raw, parsed.order);
                }
            }
        }
    }
    Ok(buffer)
}

/// Inclusive value range for an integer format character.
fn integer_range(code: u8, standard: bool) -> (i128, i128) {
    match code {
        b'b' => (-128, 127),
        b'B' => (0, 255),
        b'h' => (-32768, 32767),
        b'H' => (0, 65535),
        b'i' => (-2_147_483_648, 2_147_483_647),
        b'I' => (0, 4_294_967_295),
        b'l' | b'L' => {
            if standard {
                if code == b'l' {
                    (-2_147_483_648, 2_147_483_647)
                } else {
                    (0, 4_294_967_295)
                }
            } else if code == b'l' {
                (i128::from(i64::MIN), i128::from(i64::MAX))
            } else {
                (0, i128::from(u64::MAX))
            }
        }
        b'q' => (i128::from(i64::MIN), i128::from(i64::MAX)),
        b'Q' => (0, i128::from(u64::MAX)),
        _ => (0, 0),
    }
}

/// Unpacks one complete struct from `buffer` starting at `start`.
fn unpack_values(parsed: &Parsed, buffer: &[u8], start: usize) -> Vec<i64> {
    let mut values = Vec::new();
    for item in &parsed.items {
        if item.code == b'x' {
            continue;
        }
        if item.code == b's' {
            let begin = start + item.offset;
            let end = begin + item.count;
            values.push(value::bytes_new(buffer[begin..end].to_vec()));
            continue;
        }
        let size = code_size(item.code, parsed.standard);
        for index in 0..item.count {
            let offset = start + item.offset + index * size;
            match item.code {
                b'?' => values.push(value::bool_value(buffer[offset] != 0)),
                b'f' => {
                    let bits = read_uint(buffer, offset, 4, parsed.order) as u32;
                    values.push(value::float_new(f64::from(f32::from_bits(bits))));
                }
                b'd' => {
                    let bits = read_uint(buffer, offset, 8, parsed.order);
                    values.push(value::float_new(f64::from_bits(bits)));
                }
                b'b' | b'h' | b'i' | b'l' | b'q' => {
                    values.push(int_result(read_signed(buffer, offset, size, parsed.order)));
                }
                b'B' | b'H' | b'I' | b'L' | b'Q' => {
                    let raw = read_uint(buffer, offset, size, parsed.order);
                    values.push(unsigned_result(raw));
                }
                _ => {}
            }
        }
    }
    values
}

fn tuple_of(values: Vec<i64>) -> i64 {
    value::tuple_new(values)
}

fn buffer_bytes(value_arg: i64) -> Result<Vec<u8>, i64> {
    match value::as_bytes(value_arg) {
        Some(bytes) => Ok(bytes.to_vec()),
        None => Err(type_error(&format!(
            "a bytes-like object is required, not '{}'",
            value::type_name(value_arg)
        ))),
    }
}

fn offset_argument(value_arg: i64) -> Result<i64, i64> {
    match value::as_integer(value_arg) {
        Some(offset) => Ok(offset),
        None => Err(type_error("required argument is not an integer")),
    }
}

/// Resolves a possibly negative `unpack_from` offset.
fn resolve_read_offset(offset: i64, length: usize) -> Result<usize, String> {
    if offset < 0 {
        let resolved = length as i128 + offset as i128;
        if resolved < 0 {
            return Err(format!(
                "offset {offset} out of range for {length}-byte buffer"
            ));
        }
        Ok(resolved as usize)
    } else {
        Ok(offset as usize)
    }
}

fn pack_into_buffer(parsed: &Parsed, buffer_arg: i64, offset: i64, values: &[i64]) -> i64 {
    if value::as_bytearray(buffer_arg).is_none() {
        return type_error(&format!(
            "argument must be read-write bytes-like object, not {}",
            value::type_name(buffer_arg)
        ));
    }
    let payload = match pack_values(parsed, values) {
        Ok(payload) => payload,
        Err(message) => return struct_error(&message),
    };
    let length = value::as_bytearray(buffer_arg).map_or(0, Vec::len);
    let start = if offset < 0 {
        let resolved = length as i128 + offset as i128;
        if resolved < 0 {
            return struct_error(&format!(
                "offset {offset} out of range for {length}-byte buffer"
            ));
        }
        resolved as usize
    } else {
        offset as usize
    };
    let end = start.saturating_add(payload.len());
    if end > length {
        return struct_error(&format!(
            "pack_into requires a buffer of at least {end} bytes for packing {} bytes at offset {start} (actual buffer size is {length})",
            payload.len()
        ));
    }
    let Some(target) = value::bytearray_bytes_mut(buffer_arg) else {
        return type_error("argument must be read-write bytes-like object");
    };
    target[start..end].copy_from_slice(&payload);
    NONE
}

/// `Struct.__init__`: parses `format` and publishes `format`/`size`.
fn init_struct(instance: i64, format_arg: i64) -> i64 {
    let Ok(format) = string_argument(format_arg, "format") else {
        return NONE;
    };
    if exc::pending() {
        return NONE;
    }
    let parsed = match parsed_format(&format) {
        Ok(parsed) => parsed,
        Err(message) => return struct_error(&message),
    };
    value::instance_setattr(instance, "format".to_string(), value::str_alloc(format));
    value::instance_setattr(instance, "size".to_string(), int_result(parsed.size as i64));
    instance
}

fn create_struct(format_arg: i64) -> i64 {
    let class = struct_class();
    let instance = value::instance_new(class);
    let result = init_struct(instance, format_arg);
    if exc::pending() {
        NONE
    } else {
        result
    }
}

/// Dispatches one native call. `args` already contains `self` for the
/// `Struct.*` method entries.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "pack" | "Struct.pack" => {
            let (format_arg, values) = if function == "pack" {
                (
                    args.first().copied().unwrap_or(NONE),
                    &args[1.min(args.len())..],
                )
            } else {
                let Some(instance) = args.first().copied() else {
                    return type_error("Struct.pack() missing self");
                };
                let format = match value::instance_attr(instance, "format") {
                    Some(format) => format,
                    None => return type_error("Struct instance is missing its format"),
                };
                (format, &args[1.min(args.len())..])
            };
            let Ok(format) = string_argument(format_arg, "format") else {
                return NONE;
            };
            let parsed = match parsed_format(&format) {
                Ok(parsed) => parsed,
                Err(message) => return struct_error(&message),
            };
            match pack_values(&parsed, values) {
                Ok(buffer) => value::bytes_new(buffer),
                Err(message) => struct_error(&message),
            }
        }
        "pack_into" | "Struct.pack_into" => {
            let (format_arg, rest) = if function == "pack_into" {
                (
                    args.first().copied().unwrap_or(NONE),
                    &args[1.min(args.len())..],
                )
            } else {
                let Some(instance) = args.first().copied() else {
                    return type_error("Struct.pack_into() missing self");
                };
                let format = match value::instance_attr(instance, "format") {
                    Some(format) => format,
                    None => return type_error("Struct instance is missing its format"),
                };
                (format, &args[1.min(args.len())..])
            };
            let Ok(format) = string_argument(format_arg, "format") else {
                return NONE;
            };
            let parsed = match parsed_format(&format) {
                Ok(parsed) => parsed,
                Err(message) => return struct_error(&message),
            };
            let Some(buffer_arg) = rest.first().copied() else {
                return type_error("pack_into() missing buffer");
            };
            let Some(offset_arg) = rest.get(1).copied() else {
                return type_error("pack_into() missing offset");
            };
            let offset = match offset_argument(offset_arg) {
                Ok(offset) => offset,
                Err(error) => return error,
            };
            let values = if rest.len() > 2 { &rest[2..] } else { &[] };
            pack_into_buffer(&parsed, buffer_arg, offset, values)
        }
        "unpack" | "Struct.unpack" => {
            let (format_arg, rest) = if function == "unpack" {
                (
                    args.first().copied().unwrap_or(NONE),
                    &args[1.min(args.len())..],
                )
            } else {
                let Some(instance) = args.first().copied() else {
                    return type_error("Struct.unpack() missing self");
                };
                let format = match value::instance_attr(instance, "format") {
                    Some(format) => format,
                    None => return type_error("Struct instance is missing its format"),
                };
                (format, &args[1.min(args.len())..])
            };
            let Ok(format) = string_argument(format_arg, "format") else {
                return NONE;
            };
            let parsed = match parsed_format(&format) {
                Ok(parsed) => parsed,
                Err(message) => return struct_error(&message),
            };
            let Some(buffer_arg) = rest.first().copied() else {
                return type_error("unpack() missing buffer");
            };
            let buffer = match buffer_bytes(buffer_arg) {
                Ok(buffer) => buffer,
                Err(error) => return error,
            };
            if buffer.len() != parsed.size {
                return struct_error(&format!(
                    "unpack requires a buffer of {} bytes",
                    parsed.size
                ));
            }
            tuple_of(unpack_values(&parsed, &buffer, 0))
        }
        "unpack_from" | "Struct.unpack_from" => {
            let (format_arg, rest) = if function == "unpack_from" {
                (
                    args.first().copied().unwrap_or(NONE),
                    &args[1.min(args.len())..],
                )
            } else {
                let Some(instance) = args.first().copied() else {
                    return type_error("Struct.unpack_from() missing self");
                };
                let format = match value::instance_attr(instance, "format") {
                    Some(format) => format,
                    None => return type_error("Struct instance is missing its format"),
                };
                (format, &args[1.min(args.len())..])
            };
            let Ok(format) = string_argument(format_arg, "format") else {
                return NONE;
            };
            let parsed = match parsed_format(&format) {
                Ok(parsed) => parsed,
                Err(message) => return struct_error(&message),
            };
            let Some(buffer_arg) = rest.first().copied() else {
                return type_error("unpack_from() missing buffer");
            };
            let offset = match rest.get(1).copied() {
                Some(offset_arg) => match offset_argument(offset_arg) {
                    Ok(offset) => offset,
                    Err(error) => return error,
                },
                None => 0,
            };
            let buffer = match buffer_bytes(buffer_arg) {
                Ok(buffer) => buffer,
                Err(error) => return error,
            };
            let start = match resolve_read_offset(offset, buffer.len()) {
                Ok(start) => start,
                Err(message) => return struct_error(&message),
            };
            let end = start.saturating_add(parsed.size);
            if end > buffer.len() {
                return struct_error(&format!(
                    "unpack_from requires a buffer of at least {end} bytes for unpacking {} bytes at offset {start} (actual buffer size is {})",
                    parsed.size,
                    buffer.len()
                ));
            }
            tuple_of(unpack_values(&parsed, &buffer, start))
        }
        "iter_unpack" | "Struct.iter_unpack" => {
            let (format_arg, rest) = if function == "iter_unpack" {
                (
                    args.first().copied().unwrap_or(NONE),
                    &args[1.min(args.len())..],
                )
            } else {
                let Some(instance) = args.first().copied() else {
                    return type_error("Struct.iter_unpack() missing self");
                };
                let format = match value::instance_attr(instance, "format") {
                    Some(format) => format,
                    None => return type_error("Struct instance is missing its format"),
                };
                (format, &args[1.min(args.len())..])
            };
            let Ok(format) = string_argument(format_arg, "format") else {
                return NONE;
            };
            let parsed = match parsed_format(&format) {
                Ok(parsed) => parsed,
                Err(message) => return struct_error(&message),
            };
            let Some(buffer_arg) = rest.first().copied() else {
                return type_error("iter_unpack() missing buffer");
            };
            let buffer = match buffer_bytes(buffer_arg) {
                Ok(buffer) => buffer,
                Err(error) => return error,
            };
            if parsed.size == 0 {
                return struct_error("cannot iteratively unpack with a struct of length 0");
            }
            if buffer.len() % parsed.size != 0 {
                return struct_error(&format!(
                    "iterative unpacking requires a buffer of a multiple of {} bytes",
                    parsed.size
                ));
            }
            let rows = buffer.len() / parsed.size;
            let mut items = Vec::with_capacity(rows);
            for row in 0..rows {
                items.push(tuple_of(unpack_values(&parsed, &buffer, row * parsed.size)));
            }
            crate::iter::iter_new(value::list_new(items))
        }
        "Struct.__init__" => {
            let Some(instance) = args.first().copied() else {
                return type_error("Struct() missing self");
            };
            let Some(format_arg) = args.get(1).copied() else {
                return type_error("Struct() missing format");
            };
            init_struct(instance, format_arg);
            NONE
        }
        "Struct" => {
            let format_arg = args.first().copied().unwrap_or(NONE);
            create_struct(format_arg)
        }
        "calcsize" => {
            let Some(format_arg) = args.first().copied() else {
                return type_error("calcsize() missing format");
            };
            let Ok(format) = string_argument(format_arg, "format") else {
                return NONE;
            };
            match parsed_format(&format) {
                Ok(parsed) => int_result(parsed.size as i64),
                Err(message) => struct_error(&message),
            }
        }
        "_clearcache" => {
            FORMAT_CACHE.with(|cache| cache.borrow_mut().clear());
            NONE
        }
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '_struct' has no attribute '{function}'"),
        ),
    }
}
