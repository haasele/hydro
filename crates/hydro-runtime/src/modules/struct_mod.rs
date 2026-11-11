//! `_struct`: pack and unpack C-style binary records.
//!
//! The format grammar and the conversion routines mirror CPython 3.14's
//! `Modules/_struct.c`: the format characters
//! `x c b B ? h H i I l L q Q n N e f d F D s p P` with the
//! byte-order/alignment prefixes `@ = < > !`, repeat counts, whitespace
//! between items and native alignment for `@`.
//!
//! * `@` (default) uses native sizes and native alignment.
//! * `= < > !` use the standard sizes with no padding; `n`, `N` and `P` are
//!   native-only and therefore rejected by `calcsize("=n")` exactly like
//!   CPython's `getentry` ("bad char in struct format").
//! * `s`/`p` are counted in bytes, `x` skips padding bytes, `c` is a
//!   one-byte `bytes` object, `?` packs `PyObject_IsTrue`, `e` is IEEE 754
//!   binary16, `F`/`D` are single/double C complex and `P` is `void *`.
//!
//! `struct.py` is `from _struct import *`, so this module exposes the same
//! names: `pack`, `pack_into`, `unpack`, `unpack_from`, `iter_unpack`,
//! `calcsize`, `_clearcache`, `Struct` and `error`.
//!
//! Parsing and packing never panic: every malformed format, wrong value type,
//! out-of-range integer, short buffer or truncated field raises `_struct.error`
//! (a subclass of `Exception`) and returns `None`. Float overflow propagates
//! the `OverflowError` CPython raises.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use hydro_ir::{MethodKind, ParamKind};
use num_bigint::BigInt;
use num_traits::{ToPrimitive, Zero};

use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, BUFFER_ERROR, EXCEPTION, INDEX_ERROR, MEMORY_ERROR, OVERFLOW_ERROR,
    RUNTIME_ERROR, TYPE_ERROR, UNICODE_ENCODE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::ops;
use crate::value::{self, NONE};

/// Native C `long` width, as CPython's `l`/`L` codes use it.
const NATIVE_LONG: usize = std::mem::size_of::<libc::c_long>();
/// Native `Py_ssize_t`/`size_t` width (`n`/`N`).
const NATIVE_SSIZE: usize = std::mem::size_of::<isize>();
/// Native `void *` width (`P`).
const NATIVE_POINTER: usize = std::mem::size_of::<*const u8>();
/// Largest buffer a single `pack` will allocate. A format may describe a
/// larger record (CPython reports its size), but building it would only
/// exhaust memory, so the allocation reports `MemoryError`.
const MAX_PACK_ALLOCATION: usize = 1 << 30;

/// Largest record a format may describe. CPython caps at `PY_SSIZE_T_MAX`;
/// every integer the runtime can return without promoting to a big integer is
/// `value::MAX_INT`, so a larger `calcsize` could not be represented (and
/// `sys.maxsize + 1` counts in `test_struct` are expected to overflow).
const MAX_STRUCT_SIZE: i128 = value::MAX_INT as i128;

/// Registry key of the cached `_struct.Struct` class.
const STRUCT_CLASS_KEY: &str = "_struct.Struct";

/// Registry key of the cached `unpack_iterator` class returned by
/// `Struct.iter_unpack`.
const UNPACK_ITERATOR_KEY: &str = "_struct.unpack_iterator";

/// Number of parsed formats kept in the per-thread cache (CPython's
/// `MAXCACHE`).
const FORMAT_CACHE_LIMIT: usize = 100;

/// Byte order used to encode multi-byte fields.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    Little,
    Big,
}

/// Size/alignment table selected by the format prefix.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Table {
    /// `@` and the prefix-less form: native sizes, native alignment.
    Native,
    /// `= < > !`: standard sizes, no alignment.
    Standard,
}

fn native_order() -> Order {
    if cfg!(target_endian = "little") {
        Order::Little
    } else {
        Order::Big
    }
}

/// One `formatdef` entry: byte size, alignment and signedness.
#[derive(Clone, Copy)]
struct Entry {
    size: usize,
    align: usize,
    signed: bool,
}

const fn native_entry(code: u8) -> Option<Entry> {
    let (size, align, signed) = match code {
        b'x' | b'c' | b's' | b'p' => (1, 0, false),
        b'b' => (1, 1, true),
        b'B' => (1, 1, false),
        b'?' => (1, 1, false),
        b'h' => (2, 2, true),
        b'H' => (2, 2, false),
        b'e' => (2, 2, false),
        b'i' => (4, 4, true),
        b'I' => (4, 4, false),
        b'f' => (4, 4, false),
        b'l' => (NATIVE_LONG, NATIVE_LONG, true),
        b'L' => (NATIVE_LONG, NATIVE_LONG, false),
        b'q' => (8, 8, true),
        b'Q' => (8, 8, false),
        b'd' => (8, 8, false),
        b'n' => (NATIVE_SSIZE, NATIVE_SSIZE, true),
        b'N' => (NATIVE_SSIZE, NATIVE_SSIZE, false),
        b'P' => (NATIVE_POINTER, NATIVE_POINTER, false),
        // `F` is a pair of floats (alignment of `float`); `D` a pair of
        // doubles (alignment of `double`).
        b'F' => (8, 4, false),
        b'D' => (16, 8, false),
        _ => return None,
    };
    Some(Entry {
        size,
        align,
        signed,
    })
}

const fn standard_entry(code: u8) -> Option<Entry> {
    let (size, signed) = match code {
        b'x' | b'c' | b's' | b'p' => (1, false),
        b'b' => (1, true),
        b'B' => (1, false),
        b'?' => (1, false),
        b'h' => (2, true),
        b'H' => (2, false),
        b'e' => (2, false),
        b'i' | b'l' => (4, true),
        b'I' | b'L' => (4, false),
        b'q' => (8, true),
        b'Q' => (8, false),
        b'f' => (4, false),
        b'd' => (8, false),
        b'F' => (8, false),
        b'D' => (16, false),
        // `n`, `N` and `P` are absent from CPython's standard tables.
        _ => return None,
    };
    Some(Entry {
        size,
        align: 0,
        signed,
    })
}

fn entry(code: u8, table: Table) -> Option<Entry> {
    match table {
        Table::Native => native_entry(code),
        Table::Standard => standard_entry(code),
    }
}

/// One parsed format item, with its resolved byte offset.
struct Item {
    code: u8,
    /// Repeat count for ordinary items; byte length for `s`/`p`.
    count: usize,
    offset: usize,
}

/// A fully parsed format string.
struct Parsed {
    items: Vec<Item>,
    size: usize,
    /// Number of values consumed/produced (`s_len`).
    len: usize,
    order: Order,
    table: Table,
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
    let roots = heap::root_values(std::slice::from_ref(&message_value));
    let class = error_class();
    let instance = crate::builtins::call(class, 1, &message_value as *const i64);
    heap::release_roots(roots);
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
fn method(name: &'static str, arity: usize) -> (String, i64, MethodKind) {
    (
        name.rsplit('.').next().unwrap_or(name).to_string(),
        value::native_function_new("_struct", name, arity),
        MethodKind::Normal,
    )
}

/// A `self` parameter descriptor for native methods.
fn self_parameter() -> value::ParamMeta {
    value::ParamMeta {
        name: "self".to_string(),
        kind: ParamKind::PosOnly,
        default: None,
    }
}

/// Builds a native method with a declared signature and full-argv trampoline.
///
/// `unpack_from`, `__repr__` and `__sizeof__` have no central `_struct` spec
/// entry (they are not module functions) and one of them accepts keywords, so
/// every caller that binds them runs the trampoline directly through the
/// shared method binder instead of the module dispatcher.
fn method_entry(
    name: &'static str,
    entry: extern "C" fn(*const i64) -> i64,
    signature: Vec<value::ParamMeta>,
) -> i64 {
    let function = value::native_function_new("_struct", name, usize::MAX);
    value::function_set_signature(function, signature);
    value::function_set_full_entry(function, entry as *const () as usize);
    function
}

/// The cached `_struct.Struct` class. Instances carry `format` and `size`
/// attributes; every operation reparses the format through [`parsed_format`],
/// so the instance state is fully observably Python data.
///
/// `format` and `size` are read-only properties, exactly like CPython's
/// getset descriptors: a half-initialized instance (`Struct.__new__`) reports
/// `size == -1` and raises `RuntimeError` for `format`.
fn struct_class() -> i64 {
    if let Some(class) = globals::registry_get(STRUCT_CLASS_KEY) {
        return class;
    }
    let unpack_from = method_entry(
        "Struct.unpack_from",
        struct_unpack_from_entry,
        vec![
            self_parameter(),
            value::ParamMeta {
                name: "buffer".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
            value::ParamMeta {
                name: "offset".to_string(),
                kind: ParamKind::Positional,
                default: Some(int_result(0)),
            },
        ],
    );
    let repr = method_entry("Struct.__repr__", struct_repr_entry, vec![self_parameter()]);
    let sizeof = method_entry(
        "Struct.__sizeof__",
        struct_sizeof_entry,
        vec![self_parameter()],
    );
    let format_getter = method_entry("Struct.format", struct_format_entry, vec![self_parameter()]);
    let size_getter = method_entry("Struct.size", struct_size_entry, vec![self_parameter()]);
    let init = method("Struct.__init__", usize::MAX);
    let pack = method("Struct.pack", usize::MAX);
    let pack_into = method("Struct.pack_into", usize::MAX);
    let unpack = method("Struct.unpack", 2);
    let iter_unpack = method("Struct.iter_unpack", 2);
    let values = [
        unpack_from,
        repr,
        sizeof,
        format_getter,
        size_getter,
        init.1,
        pack.1,
        pack_into.1,
        unpack.1,
        iter_unpack.1,
    ];
    let roots = heap::root_values(&values);
    let format_property = value::property_new(format_getter, NONE, NONE);
    let property_roots = heap::root_values(std::slice::from_ref(&format_property));
    let size_property = value::property_new(size_getter, NONE, NONE);
    heap::push_scoped_root(size_property);
    let methods = vec![
        init,
        pack,
        pack_into,
        unpack,
        ("unpack_from".to_string(), unpack_from, MethodKind::Normal),
        iter_unpack,
        ("__repr__".to_string(), repr, MethodKind::Normal),
        ("__sizeof__".to_string(), sizeof, MethodKind::Normal),
    ];
    let attrs = vec![
        ("format".to_string(), format_property),
        ("size".to_string(), size_property),
    ];
    let class = value::class_new_full("Struct".to_string(), NONE, methods, attrs);
    globals::registry_set(STRUCT_CLASS_KEY, class);
    heap::release_roots(property_roots);
    heap::release_roots(roots);
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

fn is_format_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Parses `format` into items with offsets and the total size, mirroring
/// `prepare_s` in `Modules/_struct.c`.
fn parse_format(format: &[u8]) -> Result<Parsed, String> {
    if format.len() > 1_000_000 {
        return Err("total struct size too long".to_string());
    }
    if format.contains(&0) {
        return Err("embedded null character".to_string());
    }
    let (order, table, mut index) = match format.first() {
        Some(b'<') => (Order::Little, Table::Standard, 1),
        Some(b'>') | Some(b'!') => (Order::Big, Table::Standard, 1),
        Some(b'=') => (native_order(), Table::Standard, 1),
        Some(b'@') => (native_order(), Table::Native, 1),
        _ => (native_order(), Table::Native, 0),
    };
    const OVERFLOW: &str = "total struct size too long";
    // `prepare_s` does the arithmetic in `Py_ssize_t`, so a format that fits in
    // `usize` but not in the runtime's integer range is still an overflow.
    let mut items = Vec::new();
    let mut size: i128 = 0;
    let mut length: i128 = 0;
    let mut digits: Option<i128> = None;
    while index < format.len() {
        let byte = format[index];
        index += 1;
        if is_format_space(byte) {
            continue;
        }
        if byte.is_ascii_digit() {
            let digit = i128::from(byte - b'0');
            digits = Some(
                digits
                    .unwrap_or(0)
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(digit))
                    .filter(|value| *value <= MAX_STRUCT_SIZE)
                    .ok_or_else(|| OVERFLOW.to_string())?,
            );
            continue;
        }
        let code = byte;
        let Some(entry) = entry(code, table) else {
            return Err("bad char in struct format".to_string());
        };
        let count = digits.take().unwrap_or(1);
        let aligned = align_up_i128(size, entry.align);
        let end = match code {
            b's' | b'p' => {
                length = length.checked_add(1).ok_or_else(|| OVERFLOW.to_string())?;
                let end = aligned
                    .checked_add(count)
                    .ok_or_else(|| OVERFLOW.to_string())?;
                items.push(Item {
                    code,
                    count: count as usize,
                    offset: usize::try_from(aligned).unwrap_or(usize::MAX),
                });
                end
            }
            b'x' => aligned
                .checked_add(count)
                .ok_or_else(|| OVERFLOW.to_string())?,
            _ => {
                length = length
                    .checked_add(count)
                    .ok_or_else(|| OVERFLOW.to_string())?;
                let bytes = count
                    .checked_mul(entry.size as i128)
                    .ok_or_else(|| OVERFLOW.to_string())?;
                let end = aligned
                    .checked_add(bytes)
                    .ok_or_else(|| OVERFLOW.to_string())?;
                if count > 0 {
                    items.push(Item {
                        code,
                        count: count as usize,
                        offset: usize::try_from(aligned).unwrap_or(usize::MAX),
                    });
                }
                end
            }
        };
        if end > MAX_STRUCT_SIZE || length > MAX_STRUCT_SIZE {
            return Err(OVERFLOW.to_string());
        }
        size = end;
    }
    if digits.is_some() {
        return Err("repeat count given without format specifier".to_string());
    }
    Ok(Parsed {
        items,
        size: size as usize,
        len: length as usize,
        order,
        table,
    })
}

fn align_up_i128(offset: i128, align: usize) -> i128 {
    if align <= 1 {
        return offset;
    }
    let mask = align as i128 - 1;
    (offset + mask) & !mask
}

/// Parses a format through the per-thread cache.
fn parsed_format(format: &str) -> Result<Rc<Parsed>, String> {
    if let Some(parsed) = FORMAT_CACHE.with(|cache| cache.borrow().get(format).cloned()) {
        return Ok(parsed);
    }
    let parsed = Rc::new(parse_format(format.as_bytes())?);
    FORMAT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= FORMAT_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(format.to_string(), Rc::clone(&parsed));
    });
    Ok(parsed)
}

/// Format argument: CPython accepts `str` (ASCII) or `bytes` for
/// `struct.calcsize`/`pack`/`unpack` and their `Struct` methods. A non-ASCII
/// format raises `UnicodeEncodeError`, exactly like `PyUnicode_AsASCIIString`.
fn format_argument(value_arg: i64, name: &str) -> Result<String, i64> {
    if let Some(text) = value::as_str(value_arg) {
        if text.is_ascii() {
            return Ok(text.to_string());
        }
        return Err(raise_non_ascii_format());
    }
    if let Some(bytes) = value::as_bytes(value_arg) {
        if !bytes.is_ascii() {
            return Err(raise_non_ascii_format());
        }
        return Ok(String::from_utf8_lossy(bytes).into_owned());
    }
    Err(type_error(&format!(
        "Struct() argument 1 must be a str or bytes object, not {name}"
    )))
}

fn raise_non_ascii_format() -> i64 {
    exc::raise_with_message(
        UNICODE_ENCODE_ERROR,
        "'ascii' codec can't encode character in struct format: ordinal not in range(128)",
    )
}

// -------------------------------------------------------------------------
// Argument conversion
// -------------------------------------------------------------------------

/// `get_pylong` in `Modules/_struct.c`: an exact integer, else the
/// `__index__` protocol, else `struct.error("required argument is not an
/// integer")`. A failing `__index__` keeps its own exception.
fn int_argument(value_arg: i64) -> Result<BigInt, i64> {
    if let Some(n) = value::as_integer(value_arg) {
        return Ok(BigInt::from(n));
    }
    if let Some(n) = value::as_bigint(value_arg) {
        return Ok(n.clone());
    }
    if let Some(result) = ops::instance_method_call(value_arg, "__index__", &[value_arg]) {
        if exc::pending() {
            return Err(NONE);
        }
        if let Some(n) = value::as_integer(result) {
            return Ok(BigInt::from(n));
        }
        if let Some(n) = value::as_bigint(result) {
            return Ok(n.clone());
        }
        return Err(type_error(&format!(
            "__index__ returned non-int (type {})",
            value::type_name_of(result)
        )));
    }
    Err(struct_error("required argument is not an integer"))
}

fn float_argument(value_arg: i64) -> Option<f64> {
    if let Some(f) = value::as_float(value_arg) {
        return Some(f);
    }
    if let Some(i) = value::as_integer(value_arg) {
        return Some(i as f64);
    }
    if let Some(n) = value::as_bigint(value_arg) {
        return value::bigint_to_f64(n);
    }
    if let Some(result) = ops::instance_method_call(value_arg, "__float__", &[value_arg]) {
        if exc::pending() {
            return None;
        }
        return value::as_float(result);
    }
    None
}

/// `PyComplex_AsCComplex`: numbers and complex values both convert.
fn complex_argument(value_arg: i64) -> Option<(f64, f64)> {
    crate::builtins::complex_components(value_arg)
}

/// `PyObject_IsTrue`: evaluates a user instance's `__bool__` (then `__len__`)
/// and leaves a raised exception pending, unlike the infallible
/// `value::truthy` used for internal dispatch decisions.
fn truthy_argument(value_arg: i64) -> Result<bool, i64> {
    if value::as_instance(value_arg).is_some() {
        if let Some(result) =
            crate::core::ops::instance_method_call(value_arg, "__bool__", &[value_arg])
        {
            if exc::pending() {
                return Err(NONE);
            }
            return match result {
                value::TRUE => Ok(true),
                value::FALSE => Ok(false),
                other => Err(type_error(&format!(
                    "__bool__ should return bool, returned {}",
                    value::type_name(other)
                ))),
            };
        }
        if let Some(result) =
            crate::core::ops::instance_method_call(value_arg, "__len__", &[value_arg])
        {
            if exc::pending() {
                return Err(NONE);
            }
            if let Some(length) = value::as_integer(result) {
                return Ok(length != 0);
            }
            if let Some(length) = value::as_bigint(result) {
                return Ok(!length.is_zero());
            }
            return Err(type_error(&format!(
                "'{}' object cannot be interpreted as an integer",
                value::type_name(result)
            )));
        }
    }
    Ok(value::truthy(value_arg))
}

fn int_result(n: i64) -> i64 {
    value::encode_int(n).unwrap_or_else(|| value::bigint_new(BigInt::from(n)))
}

fn unsigned_result(n: u64) -> i64 {
    match i64::try_from(n) {
        Ok(n) => int_result(n),
        Err(_) => value::bigint_new(BigInt::from(n)),
    }
}

/// Inclusive range of one integer format item, as `BigInt` bounds.
fn integer_range(_code: u8, entry: Entry) -> (BigInt, BigInt) {
    let bits = entry.size * 8;
    let modulus = BigInt::from(1u8) << bits;
    if entry.signed {
        let half = BigInt::from(1u8) << (bits - 1);
        (-half.clone(), half - BigInt::from(1u8))
    } else {
        (BigInt::zero(), modulus - BigInt::from(1u8))
    }
}

/// Two's-complement low 64 bits of `number`, used to fill the field bytes.
fn twos_complement(number: &BigInt) -> u64 {
    if number.sign() == num_bigint::Sign::Minus {
        let modulus = BigInt::from(1u8) << 64u32;
        ((number % &modulus) + &modulus)
            .to_u64()
            .unwrap_or_default()
    } else {
        number.to_u64().unwrap_or_default()
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

/// IEEE 754 binary16 encoding, matching `PyFloat_Pack2` including
/// round-to-nearest-even and the NaN payload rule. `Err` means overflow.
fn pack_half(x: f64) -> Result<u16, ()> {
    if x == 0.0 {
        return Ok(if x.is_sign_negative() { 0x8000 } else { 0 });
    }
    if x.is_infinite() {
        return Ok(if x < 0.0 { 0xfc00 } else { 0x7c00 });
    }
    if x.is_nan() {
        let sign = if x.is_sign_negative() { 0x8000u16 } else { 0 };
        let payload = ((x.to_bits() & 0x000f_ffc0_0000_0000) >> 42) as u16;
        let bits = if payload == 0 { 1 << 9 } else { payload };
        return Ok(sign | 0x7c00 | bits);
    }
    let sign = if x < 0.0 { 0x8000u16 } else { 0 };
    let magnitude = x.abs();
    let raw = magnitude.to_bits();
    let raw_exponent = ((raw >> 52) & 0x7ff) as i32;
    let mantissa = raw & ((1u64 << 52) - 1);
    let (mut e, mut f) = if raw_exponent == 0 {
        // A subnormal double is far below 2**-25: underflow to zero.
        (0i32, 0.0f64)
    } else {
        (
            raw_exponent - 1023,
            1.0 + (mantissa as f64) / ((1u64 << 52) as f64),
        )
    };
    if f != 0.0 {
        if e >= 16 {
            return Err(());
        } else if e < -25 {
            f = 0.0;
            e = 0;
        } else if e < -14 {
            f *= 2f64.powi(14 + e);
            e = 0;
        } else {
            e += 15;
            f -= 1.0;
        }
    }
    let scaled = f * 1024.0;
    let mut bits = scaled as u16;
    let fraction = scaled - f64::from(bits);
    if fraction > 0.5 || (fraction == 0.5 && bits % 2 == 1) {
        bits += 1;
        if bits == 1024 {
            bits = 0;
            e += 1;
            if e == 31 {
                return Err(());
            }
        }
    }
    Ok(sign | ((e as u16) << 10) | bits)
}

/// IEEE 754 binary16 decoding.
fn unpack_half(bits: u16) -> f64 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from((bits >> 10) & 0x1f);
    let mantissa = f64::from(bits & 0x03ff);
    if exponent == 0 {
        sign * mantissa * 2f64.powi(-24)
    } else if exponent == 31 {
        if mantissa == 0.0 {
            sign * f64::INFINITY
        } else {
            f64::NAN
        }
    } else {
        sign * (1.0 + mantissa / 1024.0) * 2f64.powi(exponent - 15)
    }
}

/// Packs one float item, propagating CPython's `OverflowError` texts.
fn pack_float(
    buffer: &mut [u8],
    offset: usize,
    code: u8,
    number: f64,
    order: Order,
) -> Result<(), i64> {
    match code {
        b'e' => match pack_half(number) {
            Ok(bits) => {
                write_uint(buffer, offset, 2, u64::from(bits), order);
                Ok(())
            }
            Err(()) => Err(exc::raise_with_message(
                OVERFLOW_ERROR,
                "float too large to pack with e format",
            )),
        },
        b'f' => {
            let narrowed = number as f32;
            if number.is_finite() && narrowed.is_infinite() {
                return Err(exc::raise_with_message(
                    OVERFLOW_ERROR,
                    "float too large to pack with f format",
                ));
            }
            write_uint(buffer, offset, 4, u64::from(narrowed.to_bits()), order);
            Ok(())
        }
        _ => {
            write_uint(buffer, offset, 8, number.to_bits(), order);
            Ok(())
        }
    }
}

/// Packs `values` into the buffer described by `parsed`.
fn pack_values(parsed: &Parsed, values: &[i64]) -> Result<Vec<u8>, i64> {
    if parsed.len != values.len() {
        return Err(struct_error(&format!(
            "pack expected {} items for packing (got {})",
            parsed.len,
            values.len()
        )));
    }
    if parsed.size > MAX_PACK_ALLOCATION {
        return Err(exc::raise_with_message(
            MEMORY_ERROR,
            "struct size is too large to allocate",
        ));
    }
    let mut buffer = vec![0u8; parsed.size];
    let mut next = 0usize;
    for item in &parsed.items {
        let Some(entry) = entry(item.code, parsed.table) else {
            return Err(struct_error("bad char in struct format"));
        };
        match item.code {
            b's' | b'p' => {
                let value_arg = values[next];
                next += 1;
                let Some(bytes) = value::as_bytes(value_arg) else {
                    return Err(struct_error(&format!(
                        "argument for '{}' must be a bytes object",
                        item.code as char
                    )));
                };
                let source = bytes.to_vec();
                let mut length = source.len();
                if item.code == b'p' {
                    if item.count == 0 {
                        length = 0;
                    } else if length > item.count - 1 {
                        length = item.count - 1;
                    }
                    if length > 0 {
                        buffer[item.offset + 1..item.offset + 1 + length]
                            .copy_from_slice(&source[..length]);
                        let prefix = length.min(255) as u8;
                        if prefix > 0 {
                            buffer[item.offset] = prefix;
                        }
                    }
                } else {
                    if length > item.count {
                        length = item.count;
                    }
                    if length > 0 {
                        buffer[item.offset..item.offset + length]
                            .copy_from_slice(&source[..length]);
                    }
                }
            }
            b'c' | b'?' => {
                for index in 0..item.count {
                    let offset = item.offset + index;
                    let value_arg = values[next];
                    next += 1;
                    if item.code == b'?' {
                        // `PyObject_IsTrue` propagates a raising `__bool__`.
                        let truth = truthy_argument(value_arg)?;
                        buffer[offset] = u8::from(truth);
                    } else {
                        match value::as_bytes(value_arg) {
                            Some([single]) => buffer[offset] = *single,
                            _ => {
                                return Err(struct_error(
                                    "char format requires a bytes object of length 1",
                                ))
                            }
                        }
                    }
                }
            }
            _ => {
                for index in 0..item.count {
                    let offset = item.offset + index * entry.size;
                    let value_arg = values[next];
                    next += 1;
                    match item.code {
                        b'e' | b'f' | b'd' => {
                            let Some(number) = float_argument(value_arg) else {
                                return Err(struct_error("required argument is not a float"));
                            };
                            pack_float(&mut buffer, offset, item.code, number, parsed.order)?;
                        }
                        b'F' | b'D' => {
                            let Some((real, imag)) = complex_argument(value_arg) else {
                                return Err(struct_error("required argument is not a complex"));
                            };
                            if item.code == b'F' {
                                write_uint(
                                    &mut buffer,
                                    offset,
                                    4,
                                    u64::from((real as f32).to_bits()),
                                    parsed.order,
                                );
                                write_uint(
                                    &mut buffer,
                                    offset + 4,
                                    4,
                                    u64::from((imag as f32).to_bits()),
                                    parsed.order,
                                );
                            } else {
                                write_uint(&mut buffer, offset, 8, real.to_bits(), parsed.order);
                                write_uint(
                                    &mut buffer,
                                    offset + 8,
                                    8,
                                    imag.to_bits(),
                                    parsed.order,
                                );
                            }
                        }
                        _ => {
                            let number = match int_argument(value_arg) {
                                Ok(number) => number,
                                Err(_) => return Err(NONE),
                            };
                            if item.code == b'P' {
                                // `PyLong_AsVoidPtr` accepts the signed long
                                // range and keeps the two's-complement bits.
                                let Some(raw) = number.to_i64() else {
                                    return Err(struct_error("int too large to convert"));
                                };
                                write_uint(
                                    &mut buffer,
                                    offset,
                                    entry.size,
                                    raw as u64,
                                    parsed.order,
                                );
                                continue;
                            }
                            let (minimum, maximum) = integer_range(item.code, entry);
                            if number < minimum || number > maximum {
                                return Err(range_error(item.code, entry));
                            }
                            let raw = twos_complement(&number);
                            write_uint(&mut buffer, offset, entry.size, raw, parsed.order);
                        }
                    }
                }
            }
        }
    }
    Ok(buffer)
}

/// `_range_error` in `Modules/_struct.c`.
fn range_error(code: u8, entry: Entry) -> i64 {
    let (minimum, maximum) = integer_range(code, entry);
    if entry.signed {
        struct_error(&format!(
            "'{}' format requires {minimum} <= number <= {maximum}",
            code as char
        ))
    } else {
        struct_error(&format!(
            "'{}' format requires 0 <= number <= {maximum}",
            code as char
        ))
    }
}

/// Unpacks one complete struct from `buffer` starting at `start`.
fn unpack_values(parsed: &Parsed, buffer: &[u8], start: usize) -> Vec<i64> {
    let mut values = Vec::new();
    for item in &parsed.items {
        let Some(entry) = entry(item.code, parsed.table) else {
            continue;
        };
        match item.code {
            b's' => {
                let begin = start + item.offset;
                values.push(value::bytes_new(buffer[begin..begin + item.count].to_vec()));
            }
            b'p' => {
                let begin = start + item.offset;
                if item.count == 0 {
                    values.push(value::bytes_new(Vec::new()));
                } else {
                    let declared = usize::from(buffer[begin]);
                    let length = declared.min(item.count - 1);
                    values.push(value::bytes_new(
                        buffer[begin + 1..begin + 1 + length].to_vec(),
                    ));
                }
            }
            b'c' => {
                let begin = start + item.offset;
                values.push(value::bytes_new(vec![buffer[begin]]));
            }
            _ => {
                for index in 0..item.count {
                    let offset = start + item.offset + index * entry.size;
                    match item.code {
                        b'?' => values.push(value::bool_value(buffer[offset] != 0)),
                        b'e' => {
                            let bits = read_uint(buffer, offset, 2, parsed.order) as u16;
                            values.push(value::float_new(unpack_half(bits)));
                        }
                        b'f' => {
                            let bits = read_uint(buffer, offset, 4, parsed.order) as u32;
                            values.push(value::float_new(f64::from(f32::from_bits(bits))));
                        }
                        b'd' => {
                            let bits = read_uint(buffer, offset, 8, parsed.order);
                            values.push(value::float_new(f64::from_bits(bits)));
                        }
                        b'F' => {
                            let real = read_uint(buffer, offset, 4, parsed.order) as u32;
                            let imag = read_uint(buffer, offset + 4, 4, parsed.order) as u32;
                            values.push(crate::builtins::build_complex(
                                f64::from(f32::from_bits(real)),
                                f64::from(f32::from_bits(imag)),
                            ));
                        }
                        b'D' => {
                            let real = read_uint(buffer, offset, 8, parsed.order);
                            let imag = read_uint(buffer, offset + 8, 8, parsed.order);
                            values.push(crate::builtins::build_complex(
                                f64::from_bits(real),
                                f64::from_bits(imag),
                            ));
                        }
                        _ => {
                            if entry.signed {
                                values.push(int_result(read_signed(
                                    buffer,
                                    offset,
                                    entry.size,
                                    parsed.order,
                                )));
                            } else {
                                let raw = read_uint(buffer, offset, entry.size, parsed.order);
                                values.push(unsigned_result(raw));
                            }
                        }
                    }
                }
            }
        }
    }
    values
}

fn tuple_of(values: Vec<i64>) -> i64 {
    value::tuple_new(values)
}

// -------------------------------------------------------------------------
// Buffer protocol
// -------------------------------------------------------------------------

/// Reads a read-only C-contiguous buffer (`PyBUF_SIMPLE`).
fn buffer_bytes(value_arg: i64) -> Result<Vec<u8>, i64> {
    if let Some(bytes) = value::as_bytes(value_arg) {
        return Ok(bytes.to_vec());
    }
    if let Some(view) = value::as_memoryview(value_arg) {
        if view.released {
            return Err(exc::raise_with_message(
                VALUE_ERROR,
                value::MEMORYVIEW_RELEASED,
            ));
        }
        if view.stride != 1 && view.len() > 1 {
            return Err(non_contiguous_buffer());
        }
        return match value::memoryview_bytes(view) {
            Ok(bytes) => Ok(bytes),
            Err(message) => Err(exc::raise_with_message(VALUE_ERROR, message)),
        };
    }
    if let Some(bytes) = crate::modules::array_mod::buffer_snapshot(value_arg) {
        return Ok(bytes);
    }
    Err(type_error(&format!(
        "a bytes-like object is required, not '{}'",
        value::type_name(value_arg)
    )))
}

fn non_contiguous_buffer() -> i64 {
    exc::raise_with_message(
        BUFFER_ERROR,
        "memoryview: underlying buffer is not C-contiguous",
    )
}

/// Writes `payload` into a writable C-contiguous buffer at `start`.
fn buffer_write(value_arg: i64, start: usize, payload: &[u8]) -> Result<(), i64> {
    if let Some(target) = value::bytearray_bytes_mut(value_arg) {
        if start + payload.len() > target.len() {
            return Err(struct_error("buffer is too small"));
        }
        target[start..start + payload.len()].copy_from_slice(payload);
        return Ok(());
    }
    if let Some(view) = value::as_memoryview(value_arg) {
        if view.released {
            return Err(exc::raise_with_message(
                VALUE_ERROR,
                value::MEMORYVIEW_RELEASED,
            ));
        }
        if view.readonly {
            return Err(type_error("memoryview: underlying buffer is read-only"));
        }
        if view.stride != 1 && view.len() > 1 {
            return Err(type_error(
                "memoryview: underlying buffer is not C-contiguous",
            ));
        }
        if start + payload.len() > view.len() {
            return Err(struct_error("buffer is too small"));
        }
        for (index, byte) in payload.iter().enumerate() {
            if let Err(message) = value::memoryview_set_byte(view, start + index, *byte) {
                return Err(exc::raise_with_message(VALUE_ERROR, message));
            }
        }
        return Ok(());
    }
    if crate::modules::array_mod::buffer_write(value_arg, start, payload) {
        return Ok(());
    }
    Err(type_error(&format!(
        "argument must be read-write bytes-like object, not {}",
        value::type_name(value_arg)
    )))
}

/// Whether `value_arg` accepts writes through the buffer protocol.
fn buffer_writable(value_arg: i64) -> bool {
    value::as_bytearray(value_arg).is_some()
        || value::as_memoryview(value_arg).is_some()
        || crate::modules::array_mod::is_array(value_arg)
}

/// `PyNumber_AsSsize_t(arg, PyExc_IndexError)`.
fn offset_argument(value_arg: i64) -> Result<i64, i64> {
    let Some(number) = value::to_bigint(value_arg) else {
        return Err(type_error(&format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(value_arg)
        )));
    };
    number.to_i64().ok_or_else(|| {
        exc::raise_with_message(INDEX_ERROR, "cannot fit 'int' into an index-sized integer")
    })
}

// -------------------------------------------------------------------------
// Operations
// -------------------------------------------------------------------------

/// Resolves the `Struct` instance's format, reporting CPython's
/// `RuntimeError` for an object whose `__init__` never ran.
fn instance_format(instance: i64, method: &str) -> Result<String, i64> {
    match value::instance_attr(instance, "format") {
        Some(format) => {
            if let Some(text) = value::as_str(format) {
                if text.is_ascii() {
                    return Ok(text.to_string());
                }
                return Err(raise_non_ascii_format());
            }
            Err(type_error(&format!("{method}() receiver has no format")))
        }
        None => Err(exc::raise_with_message(
            RUNTIME_ERROR,
            "Struct object is not initialized",
        )),
    }
}

fn parse_for(instance: i64, method: &str) -> Result<Rc<Parsed>, i64> {
    let format = instance_format(instance, method)?;
    match parsed_format(&format) {
        Ok(parsed) => Ok(parsed),
        Err(message) => Err(struct_error(&message)),
    }
}

fn pack_call(format_arg: i64, values: &[i64]) -> i64 {
    let Ok(format) = format_argument(format_arg, "format") else {
        return NONE;
    };
    let parsed = match parsed_format(&format) {
        Ok(parsed) => parsed,
        Err(message) => return struct_error(&message),
    };
    match pack_values(&parsed, values) {
        Ok(buffer) => value::bytes_new(buffer),
        Err(result) => result,
    }
}

fn unpack_call(instance: i64, buffer_arg: i64) -> i64 {
    let parsed = match parse_for(instance, "unpack") {
        Ok(parsed) => parsed,
        Err(result) => return result,
    };
    let buffer = match buffer_bytes(buffer_arg) {
        Ok(buffer) => buffer,
        Err(result) => return result,
    };
    if buffer.len() != parsed.size {
        return struct_error(&format!(
            "unpack requires a buffer of {} bytes",
            parsed.size
        ));
    }
    tuple_of(unpack_values(&parsed, &buffer, 0))
}

fn unpack_from_call(instance: i64, buffer_arg: i64, offset: i64) -> i64 {
    let parsed = match parse_for(instance, "unpack_from") {
        Ok(parsed) => parsed,
        Err(result) => return result,
    };
    let buffer = match buffer_bytes(buffer_arg) {
        Ok(buffer) => buffer,
        Err(result) => return result,
    };
    let size = parsed.size as i64;
    let mut start = offset;
    if start < 0 {
        if start.saturating_add(size) > 0 {
            return struct_error(&format!(
                "not enough data to unpack {size} bytes at offset {start}"
            ));
        }
        if start + (buffer.len() as i64) < 0 {
            return struct_error(&format!(
                "offset {start} out of range for {}-byte buffer",
                buffer.len()
            ));
        }
        start += buffer.len() as i64;
    }
    let available = buffer.len() as i64 - start;
    if available < size {
        return struct_error(&format!(
            "unpack_from requires a buffer of at least {} bytes for unpacking {size} bytes at offset {start} (actual buffer size is {})",
            parsed.size as i128 + start as i128,
            buffer.len()
        ));
    }
    tuple_of(unpack_values(&parsed, &buffer, start as usize))
}

/// `Struct.unpack_from(buffer, offset=0, /)` through the method binder: the
/// declared `buffer`/`offset` names make the keyword spellings work.
extern "C" fn struct_unpack_from_entry(argv: *const i64) -> i64 {
    // SAFETY: the binder passes `self`, `buffer` and `offset` in that order.
    let instance = unsafe { *argv };
    let buffer_arg = unsafe { *argv.add(1) };
    let offset_arg = unsafe { *argv.add(2) };
    let offset = match offset_argument(offset_arg) {
        Ok(offset) => offset,
        Err(result) => return result,
    };
    unpack_from_call(instance, buffer_arg, offset)
}

/// `Struct.format` getter (`s_get_format`): raises `RuntimeError` until
/// `__init__` published a format.
extern "C" fn struct_format_entry(argv: *const i64) -> i64 {
    // SAFETY: the property binder passes the instance.
    let instance = unsafe { *argv };
    match instance_format(instance, "format") {
        Ok(format) => value::str_alloc(format),
        Err(result) => result,
    }
}

/// `Struct.size` getter (`s_get_size`): a half-initialized instance reports
/// `-1` instead of raising.
extern "C" fn struct_size_entry(argv: *const i64) -> i64 {
    // SAFETY: the property binder passes the instance.
    let instance = unsafe { *argv };
    match value::instance_attr(instance, "size") {
        Some(size) => size,
        None => int_result(-1),
    }
}

/// `Struct.__sizeof__` (`s_sizeof`): the base object plus one `formatcode`
/// slot per parsed item and the terminator. The exact count keeps the value
/// meaningful for `sys.getsizeof` parity even though the numbers are the
/// runtime's own object sizes.
extern "C" fn struct_sizeof_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes the instance.
    let instance = unsafe { *argv };
    let parsed = match parse_for(instance, "__sizeof__") {
        Ok(parsed) => parsed,
        Err(result) => return result,
    };
    let codes = parsed.items.len().saturating_add(1) as i64;
    int_result(32 + codes * 16)
}

fn pack_into_call(instance: i64, rest: &[i64]) -> i64 {
    let parsed = match parse_for(instance, "pack_into") {
        Ok(parsed) => parsed,
        Err(result) => return result,
    };
    match rest.len() {
        0 => return struct_error("pack_into expected buffer argument"),
        1 => return struct_error("pack_into expected offset argument"),
        _ => {}
    }
    let buffer_arg = rest[0];
    if !buffer_writable(buffer_arg) {
        return type_error(&format!(
            "argument must be read-write bytes-like object, not {}",
            value::type_name(buffer_arg)
        ));
    }
    let offset = match offset_argument(rest[1]) {
        Ok(offset) => offset,
        Err(result) => return result,
    };
    let values = &rest[2..];
    if parsed.len != values.len() {
        return struct_error(&format!(
            "pack_into expected {} items for packing (got {})",
            parsed.len,
            values.len()
        ));
    }
    let payload = match pack_values(&parsed, values) {
        Ok(payload) => payload,
        Err(result) => return result,
    };
    let length = match buffer_length(buffer_arg) {
        Some(length) => length,
        None => return type_error("argument must be read-write bytes-like object"),
    };
    let size = parsed.size as i64;
    let mut start = offset;
    if start < 0 {
        if start.saturating_add(size) > 0 {
            return struct_error(&format!("no space to pack {size} bytes at offset {start}"));
        }
        if start + (length as i64) < 0 {
            return struct_error(&format!(
                "offset {start} out of range for {length}-byte buffer"
            ));
        }
        start += length as i64;
    }
    let available = length as i64 - start;
    if available < size {
        return struct_error(&format!(
            "pack_into requires a buffer of at least {} bytes for packing {size} bytes at offset {start} (actual buffer size is {length})",
            parsed.size as i128 + start as i128
        ));
    }
    match buffer_write(buffer_arg, start as usize, &payload) {
        Ok(()) => NONE,
        Err(result) => result,
    }
}

fn buffer_length(value_arg: i64) -> Option<usize> {
    if let Some(bytes) = value::as_bytearray(value_arg) {
        return Some(bytes.len());
    }
    if let Some(view) = value::as_memoryview(value_arg) {
        return Some(view.len());
    }
    crate::modules::array_mod::buffer_length(value_arg)
}

/// The iterator class returned by `Struct.iter_unpack`, named
/// `unpack_iterator` like CPython's type and deriving from
/// `collections.abc.Iterator` so `isinstance`/`issubclass` checks succeed.
fn unpack_iterator_class() -> i64 {
    if let Some(class) = globals::registry_get(UNPACK_ITERATOR_KEY) {
        return class;
    }
    let base = crate::modules::collections_abc::constant("Iterator");
    let init = method_entry(
        "Struct.unpack_iterator.__init__",
        unpack_iterator_init_entry,
        vec![self_parameter()],
    );
    let iter = method_entry(
        "Struct.unpack_iterator.__iter__",
        unpack_iterator_iter_entry,
        vec![self_parameter()],
    );
    let next = method_entry(
        "Struct.unpack_iterator.__next__",
        unpack_iterator_next_entry,
        vec![self_parameter()],
    );
    let hint = method_entry(
        "Struct.unpack_iterator.__length_hint__",
        unpack_iterator_length_hint_entry,
        vec![self_parameter()],
    );
    let roots = heap::root_values(&[base, init, iter, next, hint]);
    let methods = vec![
        ("__init__".to_string(), init, MethodKind::Normal),
        ("__iter__".to_string(), iter, MethodKind::Normal),
        ("__next__".to_string(), next, MethodKind::Normal),
        ("__length_hint__".to_string(), hint, MethodKind::Normal),
    ];
    let class = value::class_new_multi(
        "unpack_iterator".to_string(),
        vec![base],
        NONE,
        methods,
        Vec::new(),
    );
    globals::registry_set(UNPACK_ITERATOR_KEY, class);
    heap::release_roots(roots);
    class
}

/// Builds the `unpack_iterator` for `format` over a buffer snapshot.
fn new_unpack_iterator(format: &str, buffer: Vec<u8>) -> i64 {
    let class = unpack_iterator_class();
    let instance = value::instance_new(class);
    let roots = heap::root_values(std::slice::from_ref(&instance));
    value::instance_setattr(
        instance,
        "_format".to_string(),
        value::str_alloc(format.to_string()),
    );
    value::instance_setattr(instance, "_data".to_string(), value::bytes_new(buffer));
    value::instance_setattr(instance, "_position".to_string(), int_result(0));
    heap::release_roots(roots);
    instance
}

/// Iterator state: `(format, data, position)`, or the reason it is missing.
fn unpack_iterator_state(instance: i64) -> Result<(String, &'static [u8], usize), i64> {
    let Some(format) = value::instance_attr(instance, "_format").and_then(value::as_str) else {
        return Err(exc::raise_with_message(
            RUNTIME_ERROR,
            "unpack iterator is not initialized",
        ));
    };
    let Some(data) = value::instance_attr(instance, "_data").and_then(value::as_bytes) else {
        return Err(exc::raise_with_message(
            RUNTIME_ERROR,
            "unpack iterator is not initialized",
        ));
    };
    let position = value::instance_attr(instance, "_position")
        .and_then(value::as_integer)
        .unwrap_or(0)
        .max(0) as usize;
    Ok((format.to_string(), data, position))
}

/// `type(iter_unpack(...))()`: CPython's C type has no `tp_new`.
extern "C" fn unpack_iterator_init_entry(_argv: *const i64) -> i64 {
    type_error("cannot create 'unpack_iterator' instances")
}

/// `iter(self) is self`.
extern "C" fn unpack_iterator_iter_entry(argv: *const i64) -> i64 {
    // SAFETY: the binder passes the receiver first.
    unsafe { *argv }
}

/// `next(self)`: decodes one record and advances the byte cursor.
extern "C" fn unpack_iterator_next_entry(argv: *const i64) -> i64 {
    // SAFETY: the binder passes the receiver first.
    let instance = unsafe { *argv };
    let (format, data, position) = match unpack_iterator_state(instance) {
        Ok(state) => state,
        Err(result) => return result,
    };
    let parsed = match parsed_format(&format) {
        Ok(parsed) => parsed,
        Err(message) => return struct_error(&message),
    };
    if parsed.size == 0 || position + parsed.size > data.len() {
        return exc::raise_with_message(exc::STOP_ITERATION, "");
    }
    let row = tuple_of(unpack_values(&parsed, data, position));
    let next = (position + parsed.size) as i64;
    value::instance_setattr(instance, "_position".to_string(), int_result(next));
    row
}

/// `operator.length_hint` support: the number of records still to decode.
extern "C" fn unpack_iterator_length_hint_entry(argv: *const i64) -> i64 {
    // SAFETY: the binder passes the receiver first.
    let instance = unsafe { *argv };
    let (format, data, position) = match unpack_iterator_state(instance) {
        Ok(state) => state,
        Err(result) => return result,
    };
    let size = match parsed_format(&format) {
        Ok(parsed) => parsed.size,
        Err(message) => return struct_error(&message),
    };
    if size == 0 || position >= data.len() {
        return int_result(0);
    }
    int_result(((data.len() - position) / size) as i64)
}

fn iter_unpack_call(instance: i64, buffer_arg: i64) -> i64 {
    let format = match instance_format(instance, "iter_unpack") {
        Ok(format) => format,
        Err(result) => return result,
    };
    let parsed = match parsed_format(&format) {
        Ok(parsed) => parsed,
        Err(message) => return struct_error(&message),
    };
    if parsed.size == 0 {
        return struct_error("cannot iteratively unpack with a struct of length 0");
    }
    let buffer = match buffer_bytes(buffer_arg) {
        Ok(buffer) => buffer,
        Err(result) => return result,
    };
    if buffer.len() % parsed.size != 0 {
        return struct_error(&format!(
            "iterative unpacking requires a buffer of a multiple of {} bytes",
            parsed.size
        ));
    }
    new_unpack_iterator(&format, buffer)
}

/// `Struct.__init__`: parses `format` and publishes `format`/`size`. A parse
/// failure leaves the previous state untouched, like `Struct___init___impl`.
fn init_struct(instance: i64, format_arg: i64) -> i64 {
    let format = match format_argument(format_arg, &value::type_name_of(format_arg)) {
        Ok(format) => format,
        Err(result) => return result,
    };
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

fn struct_repr(instance: i64) -> i64 {
    let format = match instance_format(instance, "__repr__") {
        Ok(format) => format,
        Err(result) => return result,
    };
    // CPython renders `_PyType_Name(Py_TYPE(self))`, so a `Struct` subclass
    // keeps its own name.
    let name = value::instance_class_value(instance)
        .and_then(value::class_display_name)
        .unwrap_or("Struct");
    let text = format!("{name}({})", value::repr(value::str_alloc(format)));
    value::str_alloc(text)
}

/// `repr(s)` entry point: runs the shared renderer through the binder.
extern "C" fn struct_repr_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes the receiver first.
    struct_repr(unsafe { *argv })
}

/// Dispatches one native call. `args` already contains `self` for the
/// `Struct.*` method entries.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "pack" => pack_call(
            args.first().copied().unwrap_or(NONE),
            &args[1.min(args.len())..],
        ),
        "Struct.pack" => {
            let Some(instance) = args.first().copied() else {
                return type_error("pack() missing self");
            };
            let Ok(parsed) = parse_for(instance, "pack") else {
                return NONE;
            };
            let values = &args[1..];
            if parsed.len != values.len() {
                return struct_error(&format!(
                    "pack expected {} items for packing (got {})",
                    parsed.len,
                    values.len()
                ));
            }
            match pack_values(&parsed, values) {
                Ok(buffer) => value::bytes_new(buffer),
                Err(result) => result,
            }
        }
        "pack_into" => {
            let Some(buffer_arg) = args.get(1).copied() else {
                return struct_error("pack_into expected buffer argument");
            };
            let Some(offset_arg) = args.get(2).copied() else {
                return struct_error("pack_into expected offset argument");
            };
            let Ok(format) = format_argument(args.first().copied().unwrap_or(NONE), "format")
            else {
                return NONE;
            };
            let parsed = match parsed_format(&format) {
                Ok(parsed) => parsed,
                Err(message) => return struct_error(&message),
            };
            // Repackage into the `Struct.pack_into` shape (buffer, offset,
            // values) so one implementation covers both spellings.
            let mut rest = Vec::with_capacity(args.len());
            rest.push(buffer_arg);
            rest.push(offset_arg);
            rest.extend_from_slice(&args[3..]);
            pack_into_with(&parsed, &rest)
        }
        "Struct.pack_into" => {
            let Some(instance) = args.first().copied() else {
                return type_error("pack_into() missing self");
            };
            pack_into_call(instance, &args[1..])
        }
        "unpack" => {
            let Some(buffer_arg) = args.get(1).copied() else {
                return type_error("unpack() missing buffer");
            };
            let Ok(format) = format_argument(args.first().copied().unwrap_or(NONE), "format")
            else {
                return NONE;
            };
            let parsed = match parsed_format(&format) {
                Ok(parsed) => parsed,
                Err(message) => return struct_error(&message),
            };
            let buffer = match buffer_bytes(buffer_arg) {
                Ok(buffer) => buffer,
                Err(result) => return result,
            };
            if buffer.len() != parsed.size {
                return struct_error(&format!(
                    "unpack requires a buffer of {} bytes",
                    parsed.size
                ));
            }
            tuple_of(unpack_values(&parsed, &buffer, 0))
        }
        "Struct.unpack" => {
            let Some(instance) = args.first().copied() else {
                return type_error("unpack() missing self");
            };
            let Some(buffer_arg) = args.get(1).copied() else {
                return type_error("unpack() missing buffer");
            };
            unpack_call(instance, buffer_arg)
        }
        "unpack_from" => {
            let Some(buffer_arg) = args.get(1).copied() else {
                return type_error("unpack_from() missing buffer");
            };
            let Ok(format) = format_argument(args.first().copied().unwrap_or(NONE), "format")
            else {
                return NONE;
            };
            let parsed = match parsed_format(&format) {
                Ok(parsed) => parsed,
                Err(message) => return struct_error(&message),
            };
            let offset = match args.get(2).copied() {
                Some(offset_arg) => match offset_argument(offset_arg) {
                    Ok(offset) => offset,
                    Err(result) => return result,
                },
                None => 0,
            };
            let buffer = match buffer_bytes(buffer_arg) {
                Ok(buffer) => buffer,
                Err(result) => return result,
            };
            unpack_from_with(&parsed, &buffer, offset)
        }
        "Struct.unpack_from" => {
            let Some(instance) = args.first().copied() else {
                return type_error("unpack_from() missing self");
            };
            let Some(buffer_arg) = args.get(1).copied() else {
                return type_error("unpack_from() missing buffer");
            };
            let offset = match args.get(2).copied() {
                Some(offset_arg) => match offset_argument(offset_arg) {
                    Ok(offset) => offset,
                    Err(result) => return result,
                },
                None => 0,
            };
            unpack_from_call(instance, buffer_arg, offset)
        }
        "iter_unpack" => {
            let Some(buffer_arg) = args.get(1).copied() else {
                return type_error("iter_unpack() missing buffer");
            };
            let Ok(format) = format_argument(args.first().copied().unwrap_or(NONE), "format")
            else {
                return NONE;
            };
            let parsed = match parsed_format(&format) {
                Ok(parsed) => parsed,
                Err(message) => return struct_error(&message),
            };
            iter_unpack_with(&format, &parsed, buffer_arg)
        }
        "Struct.iter_unpack" => {
            let Some(instance) = args.first().copied() else {
                return type_error("iter_unpack() missing self");
            };
            let Some(buffer_arg) = args.get(1).copied() else {
                return type_error("iter_unpack() missing buffer");
            };
            iter_unpack_call(instance, buffer_arg)
        }
        "Struct.__init__" => {
            let Some(instance) = args.first().copied() else {
                return type_error("Struct() missing self");
            };
            let Some(format_arg) = args.get(1).copied() else {
                return type_error("Struct() missing required argument 'format' (pos 1)");
            };
            init_struct(instance, format_arg);
            NONE
        }
        "Struct.__repr__" => {
            let Some(instance) = args.first().copied() else {
                return type_error("__repr__() missing self");
            };
            struct_repr(instance)
        }
        "Struct" => create_struct(args.first().copied().unwrap_or(NONE)),
        "calcsize" => {
            let Ok(format) = format_argument(args.first().copied().unwrap_or(NONE), "format")
            else {
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

/// Shared `pack_into` body for the module function with an already-parsed
/// format.
fn pack_into_with(parsed: &Parsed, rest: &[i64]) -> i64 {
    match rest.len() {
        0 => return struct_error("pack_into expected buffer argument"),
        1 => return struct_error("pack_into expected offset argument"),
        _ => {}
    }
    let buffer_arg = rest[0];
    if !buffer_writable(buffer_arg) {
        return type_error(&format!(
            "argument must be read-write bytes-like object, not {}",
            value::type_name(buffer_arg)
        ));
    }
    let offset = match offset_argument(rest[1]) {
        Ok(offset) => offset,
        Err(result) => return result,
    };
    let values = &rest[2..];
    if parsed.len != values.len() {
        return struct_error(&format!(
            "pack_into expected {} items for packing (got {})",
            parsed.len,
            values.len()
        ));
    }
    let payload = match pack_values(parsed, values) {
        Ok(payload) => payload,
        Err(result) => return result,
    };
    let length = match buffer_length(buffer_arg) {
        Some(length) => length,
        None => return type_error("argument must be read-write bytes-like object"),
    };
    let size = parsed.size as i64;
    let mut start = offset;
    if start < 0 {
        if start.saturating_add(size) > 0 {
            return struct_error(&format!("no space to pack {size} bytes at offset {start}"));
        }
        if start + (length as i64) < 0 {
            return struct_error(&format!(
                "offset {start} out of range for {length}-byte buffer"
            ));
        }
        start += length as i64;
    }
    let available = length as i64 - start;
    if available < size {
        return struct_error(&format!(
            "pack_into requires a buffer of at least {} bytes for packing {size} bytes at offset {start} (actual buffer size is {length})",
            parsed.size as i128 + start as i128
        ));
    }
    match buffer_write(buffer_arg, start as usize, &payload) {
        Ok(()) => NONE,
        Err(result) => result,
    }
}

fn unpack_from_with(parsed: &Parsed, buffer: &[u8], offset: i64) -> i64 {
    let size = parsed.size as i64;
    let mut start = offset;
    if start < 0 {
        if start.saturating_add(size) > 0 {
            return struct_error(&format!(
                "not enough data to unpack {size} bytes at offset {start}"
            ));
        }
        if start + (buffer.len() as i64) < 0 {
            return struct_error(&format!(
                "offset {start} out of range for {}-byte buffer",
                buffer.len()
            ));
        }
        start += buffer.len() as i64;
    }
    let available = buffer.len() as i64 - start;
    if available < size {
        return struct_error(&format!(
            "unpack_from requires a buffer of at least {} bytes for unpacking {size} bytes at offset {start} (actual buffer size is {})",
            parsed.size as i128 + start as i128,
            buffer.len()
        ));
    }
    tuple_of(unpack_values(parsed, buffer, start as usize))
}

fn iter_unpack_with(format: &str, parsed: &Parsed, buffer_arg: i64) -> i64 {
    if parsed.size == 0 {
        return struct_error("cannot iteratively unpack with a struct of length 0");
    }
    let buffer = match buffer_bytes(buffer_arg) {
        Ok(buffer) => buffer,
        Err(result) => return result,
    };
    if buffer.len() % parsed.size != 0 {
        return struct_error(&format!(
            "iterative unpacking requires a buffer of a multiple of {} bytes",
            parsed.size
        ));
    }
    new_unpack_iterator(format, buffer)
}
