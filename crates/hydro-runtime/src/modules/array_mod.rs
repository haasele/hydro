//! `array`: compact arrays of machine values.
//!
//! `array.array(typecode[, initializer])` is a real native class: its
//! instances carry a hidden `bytearray` payload of native-endian items plus
//! the type code, so `append`/`insert`/`pop`/`byteswap`/`frombytes` work on
//! the packed representation exactly like CPython's C implementation.
//!
//! Supported type codes are `b B u h H i I l L f d`; `l`/`L` follow the
//! platform `long` size and `u` the platform `wchar_t` size, like CPython.
//! Every conversion mirrors CPython 3.14's `arraymodule.c`, including the
//! per-code overflow messages, the `frombytes` item-size check and the
//! slice-assignment rules. No input can panic: invalid values report a
//! pending exception and return `None`.
//!
//! The class value is cached in the globals registry, so
//! `isinstance(a, array.array)` and subclass checks see one class per
//! runtime.

use hydro_ir::MethodKind;
use num_bigint::{BigInt, Sign};
use num_traits::ToPrimitive;

use crate::container;
use crate::core::globals;
use crate::core::iter;
use crate::exc::{
    self, ATTRIBUTE_ERROR, INDEX_ERROR, OVERFLOW_ERROR, RUNTIME_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::ops;
use crate::value::{self, NONE};

const MODULE: &str = "array";
/// Registry key of the cached `array.array` class.
const CLASS_KEY: &str = "array.array.class";
/// Hidden instance attribute holding the packed payload (`bytearray`).
const DATA_ATTR: &str = "\u{0}array.data";
/// Hidden instance attribute holding the one-character type code (`str`).
const CODE_ATTR: &str = "\u{0}array.code";

/// Platform `long` size, as CPython's `l`/`L` codes use it.
const LONG_SIZE: usize = std::mem::size_of::<std::os::raw::c_long>();
const ULONG_SIZE: usize = std::mem::size_of::<std::os::raw::c_ulong>();
/// Platform `wchar_t` size, as CPython's `u` code uses it.
const WCHAR_SIZE: usize = if cfg!(windows) { 2 } else { 4 };

/// Description of one supported type code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Descr {
    code: char,
    itemsize: usize,
}

const fn descr(code: char) -> Option<Descr> {
    let itemsize = match code {
        'b' | 'B' => 1,
        'u' => WCHAR_SIZE,
        'h' | 'H' => 2,
        'i' | 'I' => 4,
        'l' => LONG_SIZE,
        'L' => ULONG_SIZE,
        'f' => 4,
        'd' => 8,
        _ => return None,
    };
    Some(Descr { code, itemsize })
}

/// The module-level `array.typecodes` string.
const TYPECODES: &str = "bBuhHiIlLfd";

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn overflow_error(message: &str) -> i64 {
    exc::raise_with_message(OVERFLOW_ERROR, message)
}

fn attribute_error(name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module '{MODULE}' has no attribute '{name}'"),
    )
}

// -------------------------------------------------------------------------
// Class construction
// -------------------------------------------------------------------------

/// Cached `array.array` class value.
pub(crate) fn array_class() -> i64 {
    if let Some(cached) = globals::registry_get(CLASS_KEY) {
        return cached;
    }
    // Method name, native dispatch name, and maximum arity (including `self`).
    let specs: [(&str, &'static str, usize); 20] = [
        ("__init__", "array.__init__", 3),
        ("append", "array.append", 2),
        ("extend", "array.extend", 2),
        ("insert", "array.insert", 3),
        ("pop", "array.pop", 2),
        ("remove", "array.remove", 2),
        ("count", "array.count", 2),
        ("index", "array.index", 4),
        ("reverse", "array.reverse", 1),
        ("tobytes", "array.tobytes", 1),
        ("frombytes", "array.frombytes", 2),
        ("tolist", "array.tolist", 1),
        ("byteswap", "array.byteswap", 1),
        ("__getitem__", "array.__getitem__", 2),
        ("__setitem__", "array.__setitem__", 3),
        ("__len__", "array.__len__", 1),
        ("__iter__", "array.__iter__", 1),
        ("__contains__", "array.__contains__", 2),
        ("__repr__", "array.__repr__", 1),
        ("__eq__", "array.__eq__", 2),
    ];
    let mut methods = Vec::with_capacity(specs.len());
    let mut values = Vec::with_capacity(specs.len());
    for (name, native, arity) in specs {
        let function = value::native_function_new(MODULE, native, arity);
        values.push(function);
        methods.push((name.to_string(), function, MethodKind::Normal));
    }
    let method_roots = heap::root_values(&values);
    let class = value::class_new_full("array".to_string(), NONE, methods, Vec::new());
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(CLASS_KEY, class);
    heap::release_roots(class_roots);
    heap::release_roots(method_roots);
    class
}

/// Module constants (`array` class and `typecodes`).
pub(crate) fn constant(name: &str) -> Option<i64> {
    match name {
        "array" => Some(array_class()),
        "typecodes" => Some(value::str_alloc(TYPECODES.to_string())),
        _ => None,
    }
}

// -------------------------------------------------------------------------
// Instance storage
// -------------------------------------------------------------------------

/// Type code of an array instance.
fn code_of(obj: i64) -> Option<Descr> {
    let stored = value::instance_attr(obj, CODE_ATTR)?;
    let text = value::as_str(stored)?;
    let mut chars = text.chars();
    let code = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    descr(code)
}

/// Payload value (`bytearray`) of an array instance.
fn data_of(obj: i64) -> Option<i64> {
    value::instance_attr(obj, DATA_ATTR)
}

/// Borrows the packed payload of an array instance.
fn raw_of(obj: i64) -> Option<&'static [u8]> {
    value::as_bytearray(data_of(obj)?).map(|bytes| bytes.as_slice())
}

/// Number of items currently stored.
fn items_len(obj: i64, descr: Descr) -> usize {
    raw_of(obj).map_or(0, |raw| raw.len() / descr.itemsize)
}

/// Snapshot of the packed payload, used when user code may run.
fn raw_snapshot(obj: i64) -> Vec<u8> {
    raw_of(obj).map(<[u8]>::to_vec).unwrap_or_default()
}

/// Appends raw item bytes to the payload.
fn push_raw(obj: i64, bytes: &[u8]) -> bool {
    let Some(data) = data_of(obj) else {
        return false;
    };
    match value::bytearray_bytes_mut(data) {
        Some(target) => {
            target.extend_from_slice(bytes);
            true
        }
        None => false,
    }
}

/// Replaces the whole payload.
fn store_raw(obj: i64, raw: Vec<u8>) {
    let data = value::bytearray_new(raw);
    value::instance_setattr(obj, DATA_ATTR.to_string(), data);
}

/// Writes one item at `index`.
fn write_item(obj: i64, descr: Descr, index: usize, bytes: &[u8]) -> bool {
    let Some(data) = data_of(obj) else {
        return false;
    };
    let Some(target) = value::bytearray_bytes_mut(data) else {
        return false;
    };
    let start = index * descr.itemsize;
    let Some(end) = start.checked_add(descr.itemsize) else {
        return false;
    };
    if end > target.len() {
        return false;
    }
    target[start..end].copy_from_slice(&bytes[..descr.itemsize]);
    true
}

/// Allocates a fresh empty array instance for `descr`.
fn new_instance(descr: Descr) -> i64 {
    let class = array_class();
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    let instance = value::instance_new(class);
    let roots = heap::root_values(std::slice::from_ref(&instance));
    let data = value::bytearray_new(Vec::new());
    value::instance_setattr(instance, DATA_ATTR.to_string(), data);
    let code = value::str_alloc(descr.code.to_string());
    value::instance_setattr(instance, CODE_ATTR.to_string(), code);
    // CPython exposes `typecode` and `itemsize` as read-only members; the
    // runtime has no data descriptor hook here, so they are plain attributes
    // while the hidden copies above stay authoritative.
    let typecode = value::str_alloc(descr.code.to_string());
    value::instance_setattr(instance, "typecode".to_string(), typecode);
    let itemsize = ops::int_result(descr.itemsize as i64);
    value::instance_setattr(instance, "itemsize".to_string(), itemsize);
    heap::release_roots(roots);
    heap::release_roots(class_roots);
    instance
}

// -------------------------------------------------------------------------
// Item conversion
// -------------------------------------------------------------------------

/// Builds the native-endian bytes of a signed integer of `size` bytes.
fn signed_bytes(value: i64, size: usize) -> [u8; 8] {
    let mut out = [0u8; 8];
    match size {
        1 => out[..1].copy_from_slice(&(value as i8).to_ne_bytes()),
        2 => out[..2].copy_from_slice(&(value as i16).to_ne_bytes()),
        4 => out[..4].copy_from_slice(&(value as i32).to_ne_bytes()),
        _ => out.copy_from_slice(&value.to_ne_bytes()),
    }
    out
}

/// Builds the native-endian bytes of an unsigned integer of `size` bytes.
fn unsigned_bytes(value: u64, size: usize) -> [u8; 8] {
    let mut out = [0u8; 8];
    match size {
        1 => out[..1].copy_from_slice(&(value as u8).to_ne_bytes()),
        2 => out[..2].copy_from_slice(&(value as u16).to_ne_bytes()),
        4 => out[..4].copy_from_slice(&(value as u32).to_ne_bytes()),
        _ => out.copy_from_slice(&value.to_ne_bytes()),
    }
    out
}

/// Reads a native-endian signed integer of `size` bytes.
fn read_signed(raw: &[u8], size: usize) -> Option<i64> {
    match size {
        1 => raw.first().map(|byte| *byte as i8 as i64),
        2 => raw
            .get(..2)?
            .try_into()
            .ok()
            .map(|bytes: [u8; 2]| i16::from_ne_bytes(bytes) as i64),
        4 => raw
            .get(..4)?
            .try_into()
            .ok()
            .map(|bytes: [u8; 4]| i32::from_ne_bytes(bytes) as i64),
        _ => raw
            .get(..8)?
            .try_into()
            .ok()
            .map(|bytes: [u8; 8]| i64::from_ne_bytes(bytes)),
    }
}

/// Reads a native-endian unsigned integer of `size` bytes.
fn read_unsigned(raw: &[u8], size: usize) -> Option<u64> {
    match size {
        1 => raw.first().map(|byte| *byte as u64),
        2 => raw
            .get(..2)?
            .try_into()
            .ok()
            .map(|bytes: [u8; 2]| u16::from_ne_bytes(bytes) as u64),
        4 => raw
            .get(..4)?
            .try_into()
            .ok()
            .map(|bytes: [u8; 4]| u32::from_ne_bytes(bytes) as u64),
        _ => raw
            .get(..8)?
            .try_into()
            .ok()
            .map(|bytes: [u8; 8]| u64::from_ne_bytes(bytes)),
    }
}

/// `PyFloat_AsDouble` for one array item: ints, bools, big ints and floats.
fn float_item(v: i64) -> Option<f64> {
    if let Some(f) = value::as_float(v) {
        return Some(f);
    }
    if let Some(i) = value::as_integer(v) {
        return Some(i as f64);
    }
    if let Some(n) = value::as_bigint(v) {
        return match value::bigint_to_f64(n) {
            Some(f) => Some(f),
            None => {
                overflow_error("int too large to convert to float");
                None
            }
        };
    }
    type_error(&format!(
        "must be real number, not {}",
        value::type_name_of(v)
    ));
    None
}

/// Encodes one Python value as native item bytes (`descr.itemsize` valid).
fn encode_item(descr: Descr, v: i64) -> Option<[u8; 8]> {
    match descr.code {
        'f' => {
            let x = float_item(v)?;
            Some(unsigned_bytes((x as f32).to_bits() as u64, 4))
        }
        'd' => {
            let x = float_item(v)?;
            Some(x.to_ne_bytes())
        }
        'u' => {
            let Some(text) = value::as_str(v) else {
                type_error(&format!(
                    "array item must be a unicode character, not {}",
                    value::type_name_of(v)
                ));
                return None;
            };
            let mut chars = text.chars();
            let Some(ch) = chars.next() else {
                type_error("array item must be a unicode character, not a string of length 0");
                return None;
            };
            if chars.next().is_some() {
                type_error(&format!(
                    "array item must be a unicode character, not a string of length {}",
                    text.chars().count()
                ));
                return None;
            }
            Some(unsigned_bytes(ch as u32 as u64, descr.itemsize))
        }
        _ => {
            let Some(n) = value::to_bigint(v) else {
                type_error(&format!(
                    "'{}' object cannot be interpreted as an integer",
                    value::type_name_of(v)
                ));
                return None;
            };
            encode_integer(descr.code, &n)
        }
    }
}

/// Integer item conversion, matching CPython's per-code `PyArg_Parse`
/// formatters and range checks.
fn encode_integer(code: char, n: &BigInt) -> Option<[u8; 8]> {
    if matches!(code, 'I' | 'L') {
        let Ok(x) = u64::try_from(n) else {
            if n.sign() == Sign::Minus {
                overflow_error("can't convert negative value to unsigned int");
            } else {
                overflow_error("Python int too large to convert to C unsigned long");
            }
            return None;
        };
        if code == 'I' && x > u32::MAX as u64 {
            overflow_error("unsigned int is greater than maximum");
            return None;
        }
        let size = if code == 'I' { 4 } else { ULONG_SIZE };
        return Some(unsigned_bytes(x, size));
    }
    let Ok(x) = i64::try_from(n) else {
        overflow_error("Python int too large to convert to C long");
        return None;
    };
    match code {
        // `b` parses through the `h` (short) formatter, then checks the
        // signed-char range.
        'b' => {
            if x > i16::MAX as i64 {
                overflow_error("signed short integer is greater than maximum");
                return None;
            }
            if x < i16::MIN as i64 {
                overflow_error("signed short integer is less than minimum");
                return None;
            }
            if x > i8::MAX as i64 {
                overflow_error("signed char is greater than maximum");
                return None;
            }
            if x < i8::MIN as i64 {
                overflow_error("signed char is less than minimum");
                return None;
            }
            Some(signed_bytes(x, 1))
        }
        'B' => {
            if x < 0 {
                overflow_error("unsigned byte integer is less than minimum");
                return None;
            }
            if x > u8::MAX as i64 {
                overflow_error("unsigned byte integer is greater than maximum");
                return None;
            }
            Some(unsigned_bytes(x as u64, 1))
        }
        'h' => {
            if x > i16::MAX as i64 {
                overflow_error("signed short integer is greater than maximum");
                return None;
            }
            if x < i16::MIN as i64 {
                overflow_error("signed short integer is less than minimum");
                return None;
            }
            Some(signed_bytes(x, 2))
        }
        // `H` parses through the `i` formatter first, then checks the
        // unsigned-short range.
        'H' => {
            if x > i32::MAX as i64 {
                overflow_error("signed integer is greater than maximum");
                return None;
            }
            if x < i32::MIN as i64 {
                overflow_error("signed integer is less than minimum");
                return None;
            }
            if x < 0 {
                overflow_error("unsigned short is less than minimum");
                return None;
            }
            if x > u16::MAX as i64 {
                overflow_error("unsigned short is greater than maximum");
                return None;
            }
            Some(unsigned_bytes(x as u64, 2))
        }
        'i' => {
            if x > i32::MAX as i64 {
                overflow_error("signed integer is greater than maximum");
                return None;
            }
            if x < i32::MIN as i64 {
                overflow_error("signed integer is less than minimum");
                return None;
            }
            Some(signed_bytes(x, 4))
        }
        'l' => Some(signed_bytes(x, LONG_SIZE)),
        _ => None,
    }
}

/// Decodes one item from its native bytes.
fn decode_item(descr: Descr, raw: &[u8]) -> i64 {
    match descr.code {
        'b' => match raw.first() {
            Some(byte) => ops::int_result(*byte as i8 as i64),
            None => NONE,
        },
        'B' => match raw.first() {
            Some(byte) => ops::int_result(*byte as i64),
            None => NONE,
        },
        'u' => match read_unsigned(raw, descr.itemsize) {
            Some(code) => match u32::try_from(code).ok().and_then(char::from_u32) {
                Some(ch) => value::str_alloc(ch.to_string()),
                None => value_error(&format!(
                    "character U+{code:X} is not in range [U+0000; U+10ffff]"
                )),
            },
            None => NONE,
        },
        'h' => read_signed(raw, 2).map_or(NONE, ops::int_result),
        'H' => read_unsigned(raw, 2).map_or(NONE, |n| ops::int_result(n as i64)),
        'i' => read_signed(raw, 4).map_or(NONE, ops::int_result),
        'I' => read_unsigned(raw, 4).map_or(NONE, |n| ops::int_result(n as i64)),
        'l' => read_signed(raw, LONG_SIZE).map_or(NONE, ops::int_result),
        'L' => match read_unsigned(raw, ULONG_SIZE) {
            Some(n) if n <= i64::MAX as u64 => ops::int_result(n as i64),
            Some(n) => value::bigint_new(BigInt::from(n)),
            None => NONE,
        },
        'f' => match read_unsigned(raw, 4) {
            Some(bits) => value::float_new(f32::from_bits(bits as u32) as f64),
            None => NONE,
        },
        'd' => match raw.get(..8).and_then(|bytes| bytes.try_into().ok()) {
            Some(bytes) => value::float_new(f64::from_ne_bytes(bytes)),
            None => NONE,
        },
        _ => NONE,
    }
}

// -------------------------------------------------------------------------
// Integer arguments and slices
// -------------------------------------------------------------------------

/// `Py_ssize_t` argument (`pop`, `insert`).
fn ssize_arg(v: i64) -> Result<i64, i64> {
    let Some(n) = value::to_bigint(v) else {
        return Err(type_error(&format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name_of(v)
        )));
    };
    n.to_i64()
        .ok_or_else(|| overflow_error("Python int too large to convert to C ssize_t"))
}

/// `index()` bound argument: integers saturate like `slice_index`.
fn slice_index_arg(v: i64) -> Result<i64, i64> {
    let Some(n) = value::to_bigint(v) else {
        return Err(type_error(
            "slice indices must be integers or have an __index__ method",
        ));
    };
    Ok(n.to_i64().unwrap_or(if n.sign() == Sign::Minus {
        i64::MIN
    } else {
        i64::MAX
    }))
}

/// Subscript index (`a[i]`): non-integers get the array-specific message and
/// out-of-range integers the CPython `IndexError`.
fn subscript_index(v: i64) -> Result<i64, i64> {
    let Some(n) = value::to_bigint(v) else {
        return Err(type_error("array indices must be integers"));
    };
    n.to_i64().ok_or_else(|| {
        exc::raise_with_message(INDEX_ERROR, "cannot fit 'int' into an index-sized integer")
    })
}

/// One bound of a `slice` object: `Ok(None)` for `None`.
fn slice_bound(v: i64) -> Result<Option<i64>, i64> {
    if v == NONE {
        return Ok(None);
    }
    let Some(n) = value::to_bigint(v) else {
        return Err(type_error(
            "slice indices must be integers or None or have an __index__ method",
        ));
    };
    Ok(Some(n.to_i64().unwrap_or(if n.sign() == Sign::Minus {
        i64::MIN
    } else {
        i64::MAX
    })))
}

/// Whether `v` is a `slice` instance (the runtime's builtin class).
fn is_slice(v: i64) -> bool {
    value::instance_class_value(v) == Some(crate::builtins::slice_class())
}

/// `(start, stop, step)` of a `slice` value, validated like `PySlice_Unpack`.
fn slice_parts(slice: i64) -> Result<(Option<i64>, Option<i64>, i64), i64> {
    let start = slice_bound(value::instance_attr(slice, "start").unwrap_or(NONE))?;
    let stop = slice_bound(value::instance_attr(slice, "stop").unwrap_or(NONE))?;
    let step = slice_bound(value::instance_attr(slice, "step").unwrap_or(NONE))?;
    let step = match step {
        None => 1,
        Some(0) => return Err(value_error("slice step cannot be zero")),
        Some(step) => step,
    };
    Ok((start, stop, step))
}

/// `PySlice_AdjustIndices`: clamp the bounds and return the visited positions.
fn slice_positions(start: Option<i64>, stop: Option<i64>, step: i64, len: usize) -> Vec<usize> {
    let len = len as i64;
    let (lower, upper) = if step > 0 { (0, len) } else { (-1, len - 1) };
    let clamp = |value: i64| {
        if value < 0 {
            let adjusted = value.saturating_add(len);
            adjusted.max(lower)
        } else {
            value.min(upper)
        }
    };
    let start = match start {
        None => {
            if step < 0 {
                upper
            } else {
                lower
            }
        }
        Some(value) => clamp(value),
    };
    let stop = match stop {
        None => {
            if step < 0 {
                lower
            } else {
                upper
            }
        }
        Some(value) => clamp(value),
    };
    let mut positions = Vec::new();
    let mut index = start;
    if step > 0 {
        while index < stop {
            if index < 0 {
                break;
            }
            positions.push(index as usize);
            index = match index.checked_add(step) {
                Some(next) => next,
                None => break,
            };
        }
    } else {
        while index > stop {
            if index < 0 {
                break;
            }
            positions.push(index as usize);
            index = match index.checked_add(step) {
                Some(next) => next,
                None => break,
            };
        }
    }
    positions
}

// -------------------------------------------------------------------------
// Construction
// -------------------------------------------------------------------------

/// Parses the `typecode` argument: a one-character string.
fn typecode_arg(v: i64) -> Result<char, i64> {
    let Some(text) = value::as_str(v) else {
        return Err(type_error(&format!(
            "array() argument 1 must be a unicode character, not {}",
            value::type_name_of(v)
        )));
    };
    let mut chars = text.chars();
    let Some(code) = chars.next() else {
        return Err(type_error(
            "array() argument 1 must be a unicode character, not a string of length 0",
        ));
    };
    if chars.next().is_some() {
        return Err(type_error(&format!(
            "array() argument 1 must be a unicode character, not a string of length {}",
            text.chars().count()
        )));
    }
    Ok(code)
}

fn bad_typecode() -> i64 {
    value_error("bad typecode (must be b, B, u, h, H, i, I, l, L, f or d)")
}

/// `array.array(typecode[, initializer])`.
fn construct(args: &[i64]) -> i64 {
    match args.len() {
        0 => return type_error("array() takes at least 1 argument (0 given)"),
        1 | 2 => {}
        count => {
            return type_error(&format!(
                "array() takes at most 2 arguments ({count} given)"
            ))
        }
    }
    let code = match typecode_arg(args[0]) {
        Ok(code) => code,
        Err(error) => return error,
    };
    let initial = args.get(1).copied();
    if let Some(init) = initial {
        // A `str` initializer is only valid for the unicode code; the check
        // happens before the type code itself is validated.
        if code != 'u' && value::as_str(init).is_some() {
            return type_error(&format!(
                "cannot use a str to initialize an array with typecode '{code}'"
            ));
        }
    }
    // A non-fast-path initializer is converted to an iterator first, so its
    // iteration errors take precedence over a bad type code.
    let mut iterator = NONE;
    let mut fast = true;
    if let Some(init) = initial {
        fast = matches!(initial_kind(init, code), Initial::Fast);
        if !fast {
            iterator = iter::iter_of(init);
            if exc::pending() {
                return NONE;
            }
        }
    }
    let Some(descr) = descr(code) else {
        return bad_typecode();
    };
    let instance = new_instance(descr);
    let roots = heap::root_values(std::slice::from_ref(&instance));
    let result = match initial {
        None => NONE,
        Some(init) if fast => fill_fast(instance, descr, init),
        Some(_) => fill_iter(instance, descr, iterator),
    };
    heap::release_roots(roots);
    if exc::pending() {
        return NONE;
    }
    if result == NONE {
        return instance;
    }
    result
}

/// Classification of a constructor initializer.
enum Initial {
    Fast,
    Iterable,
}

/// CPython's fast-path test for `array(typecode, initializer)`.
fn initial_kind(init: i64, code: char) -> Initial {
    if value::as_list(init).is_some()
        || value::as_tuple(init).is_some()
        || value::as_bytes(init).is_some()
    {
        return Initial::Fast;
    }
    if code == 'u' && value::as_str(init).is_some() {
        return Initial::Fast;
    }
    // Arrays are consumed element by element without going through the
    // Python iterator protocol, so construction never needs the class's
    // `__iter__` native dispatch.
    if is_array(init) {
        return Initial::Fast;
    }
    Initial::Iterable
}

/// Whether `v` is an `array.array` instance (or subclass instance).
fn is_array(v: i64) -> bool {
    let Some(class) = value::instance_class_value(v) else {
        return false;
    };
    value::is_subclass_of(class, array_class())
}

/// Fills a fresh array from a fast-path initializer.
fn fill_fast(instance: i64, descr: Descr, init: i64) -> i64 {
    if let Some(items) = value::as_list(init) {
        return push_sequence(instance, descr, items);
    }
    if let Some(items) = value::as_tuple(init) {
        return push_sequence(instance, descr, items);
    }
    if let Some(bytes) = value::as_bytes(init) {
        return append_bytes(instance, descr, bytes);
    }
    if descr.code == 'u' {
        if let Some(text) = value::as_str(init) {
            for ch in text.chars() {
                let encoded = unsigned_bytes(ch as u32 as u64, descr.itemsize);
                if !push_raw(instance, &encoded[..descr.itemsize]) {
                    return type_error("array payload is unavailable");
                }
            }
            return NONE;
        }
    }
    if is_array(init) {
        let Some(other_descr) = code_of(init) else {
            return type_error("array payload is unavailable");
        };
        let Some(other) = raw_of(init) else {
            return type_error("array payload is unavailable");
        };
        if other_descr.code == descr.code {
            if !push_raw(instance, other) {
                return type_error("array payload is unavailable");
            }
            return NONE;
        }
        // A different type code is converted element by element, exactly as
        // iterating the source array would.
        let other = other.to_vec();
        for chunk in other.chunks(other_descr.itemsize) {
            let item = decode_item(other_descr, chunk);
            if exc::pending() {
                return NONE;
            }
            let Some(encoded) = encode_item(descr, item) else {
                return NONE;
            };
            if !push_raw(instance, &encoded[..descr.itemsize]) {
                return type_error("array payload is unavailable");
            }
        }
    }
    NONE
}

/// Converts each value of a list/tuple initializer.
fn push_sequence(instance: i64, descr: Descr, items: &[i64]) -> i64 {
    for item in items {
        let Some(encoded) = encode_item(descr, *item) else {
            return NONE;
        };
        if !push_raw(instance, &encoded[..descr.itemsize]) {
            return type_error("array payload is unavailable");
        }
    }
    NONE
}

/// Consumes an arbitrary iterable initializer.
fn fill_iter(instance: i64, descr: Descr, iterator: i64) -> i64 {
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            return NONE;
        }
        let Some(encoded) = encode_item(descr, item) else {
            return NONE;
        };
        if !push_raw(instance, &encoded[..descr.itemsize]) {
            return type_error("array payload is unavailable");
        }
    }
}

/// `frombytes` validation and append.
fn append_bytes(instance: i64, descr: Descr, bytes: &[u8]) -> i64 {
    if !bytes.len().is_multiple_of(descr.itemsize) {
        return value_error("bytes length not a multiple of item size");
    }
    if !push_raw(instance, bytes) {
        return type_error("array payload is unavailable");
    }
    NONE
}

// -------------------------------------------------------------------------
// Methods
// -------------------------------------------------------------------------

/// Receiver of a native method call.
fn receiver(args: &[i64], method: &str) -> Result<(i64, Descr), i64> {
    let Some(&obj) = args.first() else {
        return Err(type_error(&format!("{method}() missing self")));
    };
    match code_of(obj) {
        Some(descr) => Ok((obj, descr)),
        None => Err(type_error(&format!("{method}() receiver is not an array"))),
    }
}

fn len_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.__len__") else {
        return NONE;
    };
    ops::int_result(items_len(obj, descr) as i64)
}

fn getitem_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.__getitem__") else {
        return NONE;
    };
    let Some(&index) = args.get(1) else {
        return type_error("array.__getitem__() missing index");
    };
    if is_slice(index) {
        return slice_get(obj, descr, index);
    }
    let Ok(index) = subscript_index(index) else {
        return NONE;
    };
    let len = items_len(obj, descr) as i64;
    let position = if index < 0 {
        index.saturating_add(len)
    } else {
        index
    };
    if position < 0 || position >= len {
        return exc::raise_with_message(INDEX_ERROR, "array index out of range");
    }
    let Some(raw) = raw_of(obj) else {
        return type_error("array payload is unavailable");
    };
    let start = position as usize * descr.itemsize;
    match raw.get(start..start + descr.itemsize) {
        Some(item) => decode_item(descr, item),
        None => exc::raise_with_message(INDEX_ERROR, "array index out of range"),
    }
}

/// `a[start:stop:step]` as a fresh array of the same type code.
fn slice_get(obj: i64, descr: Descr, slice: i64) -> i64 {
    let (start, stop, step) = match slice_parts(slice) {
        Ok(parts) => parts,
        Err(error) => return error,
    };
    let Some(raw) = raw_of(obj) else {
        return type_error("array payload is unavailable");
    };
    let positions = slice_positions(start, stop, step, raw.len() / descr.itemsize);
    let mut out = Vec::with_capacity(positions.len() * descr.itemsize);
    for position in positions {
        let begin = position * descr.itemsize;
        out.extend_from_slice(&raw[begin..begin + descr.itemsize]);
    }
    let result = new_instance(descr);
    let roots = heap::root_values(std::slice::from_ref(&result));
    store_raw(result, out);
    heap::release_roots(roots);
    result
}

fn setitem_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.__setitem__") else {
        return NONE;
    };
    let (Some(&index), Some(&new_value)) = (args.get(1), args.get(2)) else {
        return type_error("array.__setitem__() missing arguments");
    };
    if is_slice(index) {
        return slice_set(obj, descr, index, new_value);
    }
    let Ok(index) = subscript_index(index) else {
        return NONE;
    };
    let len = items_len(obj, descr) as i64;
    let position = if index < 0 {
        index.saturating_add(len)
    } else {
        index
    };
    if position < 0 || position >= len {
        return exc::raise_with_message(INDEX_ERROR, "array assignment index out of range");
    }
    let Some(encoded) = encode_item(descr, new_value) else {
        return NONE;
    };
    if !write_item(obj, descr, position as usize, &encoded) {
        return type_error("array payload is unavailable");
    }
    NONE
}

/// `a[start:stop:step] = other` for an array right-hand side.
fn slice_set(obj: i64, descr: Descr, slice: i64, new_value: i64) -> i64 {
    if !is_array(new_value) {
        return type_error(&format!(
            "can only assign array (not \"{}\") to array slice",
            value::type_name_of(new_value)
        ));
    }
    let Some(other_descr) = code_of(new_value) else {
        return type_error("array payload is unavailable");
    };
    if other_descr.code != descr.code {
        return type_error("bad argument type for built-in operation");
    }
    let (start, stop, step) = match slice_parts(slice) {
        Ok(parts) => parts,
        Err(error) => return error,
    };
    let Some(raw) = raw_of(obj) else {
        return type_error("array payload is unavailable");
    };
    let mut raw = raw.to_vec();
    let other = raw_snapshot(new_value);
    let itemsize = descr.itemsize;
    let len = raw.len() / itemsize;
    let positions = slice_positions(start, stop, step, len);
    if step == 1 {
        // The insertion point is the first selected position; when the slice
        // is empty (`a[2:1] = ...`) it is the clamped `start` bound.
        let insert_at = match positions.first() {
            Some(position) => *position,
            None => {
                let len = len as i64;
                let start = start.unwrap_or(0);
                let clamped = if start < 0 {
                    (start.saturating_add(len)).max(0)
                } else {
                    start.min(len)
                };
                clamped as usize
            }
        };
        let remove_end = match positions.last() {
            Some(position) => position + 1,
            None => insert_at,
        };
        let mut out = Vec::with_capacity(raw.len() + other.len());
        out.extend_from_slice(&raw[..insert_at * itemsize]);
        out.extend_from_slice(&other);
        out.extend_from_slice(&raw[remove_end * itemsize..]);
        raw = out;
    } else if other.is_empty() {
        // Delete the selected positions. A negative step yields them in
        // descending order; the splice below needs ascending positions.
        let mut ordered = positions.clone();
        ordered.sort_unstable();
        let mut out = Vec::with_capacity(raw.len());
        let mut cursor = 0usize;
        for position in &ordered {
            let begin = *position * itemsize;
            out.extend_from_slice(&raw[cursor..begin]);
            cursor = begin + itemsize;
        }
        out.extend_from_slice(&raw[cursor..]);
        raw = out;
    } else {
        let needed = other.len() / itemsize;
        if needed != positions.len() {
            return value_error(&format!(
                "attempt to assign array of size {needed} to extended slice of size {}",
                positions.len()
            ));
        }
        for (slot, position) in positions.iter().enumerate() {
            let begin = *position * itemsize;
            raw[begin..begin + itemsize]
                .copy_from_slice(&other[slot * itemsize..slot * itemsize + itemsize]);
        }
    }
    store_raw(obj, raw);
    NONE
}

fn append_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.append") else {
        return NONE;
    };
    let Some(&new_value) = args.get(1) else {
        return type_error("append() takes exactly one argument (0 given)");
    };
    let Some(encoded) = encode_item(descr, new_value) else {
        return NONE;
    };
    if !push_raw(obj, &encoded[..descr.itemsize]) {
        return type_error("array payload is unavailable");
    }
    NONE
}

fn extend_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.extend") else {
        return NONE;
    };
    let Some(&source) = args.get(1) else {
        return type_error("extend() takes exactly one argument (0 given)");
    };
    if is_array(source) {
        let Some(other_descr) = code_of(source) else {
            return type_error("array payload is unavailable");
        };
        if other_descr.code != descr.code {
            return type_error("can only extend with array of same kind");
        }
        if source == obj {
            let raw = raw_snapshot(source);
            if !push_raw(obj, &raw) {
                return type_error("array payload is unavailable");
            }
            return NONE;
        }
        let Some(raw) = raw_of(source) else {
            return type_error("array payload is unavailable");
        };
        if !push_raw(obj, raw) {
            return type_error("array payload is unavailable");
        }
        return NONE;
    }
    let iterator = iter::iter_of(source);
    if exc::pending() {
        return NONE;
    }
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            return NONE;
        }
        let Some(encoded) = encode_item(descr, item) else {
            return NONE;
        };
        if !push_raw(obj, &encoded[..descr.itemsize]) {
            return type_error("array payload is unavailable");
        }
    }
}

fn insert_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.insert") else {
        return NONE;
    };
    let (Some(&where_arg), Some(&new_value)) = (args.get(1), args.get(2)) else {
        return type_error("insert() takes exactly 2 arguments");
    };
    let Ok(at) = ssize_arg(where_arg) else {
        return NONE;
    };
    // CPython validates the value before resizing.
    let Some(encoded) = encode_item(descr, new_value) else {
        return NONE;
    };
    let len = items_len(obj, descr) as i64;
    let position = if at < 0 {
        at.saturating_add(len).max(0)
    } else {
        at.min(len)
    } as usize;
    let Some(data) = data_of(obj) else {
        return type_error("array payload is unavailable");
    };
    let Some(target) = value::bytearray_bytes_mut(data) else {
        return type_error("array payload is unavailable");
    };
    let start = position * descr.itemsize;
    if start > target.len() {
        return type_error("array payload is unavailable");
    }
    target.splice(start..start, encoded[..descr.itemsize].iter().copied());
    NONE
}

fn pop_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.pop") else {
        return NONE;
    };
    let len = items_len(obj, descr) as i64;
    if len == 0 {
        return exc::raise_with_message(INDEX_ERROR, "pop from empty array");
    }
    let where_arg = args.get(1).copied().unwrap_or(NONE);
    let where_value = if where_arg == NONE {
        -1
    } else {
        match ssize_arg(where_arg) {
            Ok(value) => value,
            Err(error) => return error,
        }
    };
    let position = if where_value < 0 {
        where_value.saturating_add(len)
    } else {
        where_value
    };
    if position < 0 || position >= len {
        return exc::raise_with_message(INDEX_ERROR, "pop index out of range");
    }
    let start = position as usize * descr.itemsize;
    let item = {
        let Some(raw) = raw_of(obj) else {
            return type_error("array payload is unavailable");
        };
        match raw.get(start..start + descr.itemsize) {
            Some(bytes) => decode_item(descr, bytes),
            None => return exc::raise_with_message(INDEX_ERROR, "pop index out of range"),
        }
    };
    if exc::pending() {
        return NONE;
    }
    let Some(data) = data_of(obj) else {
        return type_error("array payload is unavailable");
    };
    let Some(target) = value::bytearray_bytes_mut(data) else {
        return type_error("array payload is unavailable");
    };
    target.drain(start..start + descr.itemsize);
    item
}

fn remove_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.remove") else {
        return NONE;
    };
    let Some(&target) = args.get(1) else {
        return type_error("remove() takes exactly one argument (0 given)");
    };
    let raw = raw_snapshot(obj);
    for (index, chunk) in raw.chunks(descr.itemsize).enumerate() {
        let item = decode_item(descr, chunk);
        if exc::pending() {
            return NONE;
        }
        if container::equals(item, target) {
            let Some(data) = data_of(obj) else {
                return type_error("array payload is unavailable");
            };
            let Some(bytes) = value::bytearray_bytes_mut(data) else {
                return type_error("array payload is unavailable");
            };
            let start = index * descr.itemsize;
            bytes.drain(start..start + descr.itemsize);
            return NONE;
        }
    }
    value_error("array.remove(x): x not in array")
}

fn count_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.count") else {
        return NONE;
    };
    let Some(&target) = args.get(1) else {
        return type_error("count() takes exactly one argument (0 given)");
    };
    let raw = raw_snapshot(obj);
    let mut count = 0i64;
    for chunk in raw.chunks(descr.itemsize) {
        let item = decode_item(descr, chunk);
        if exc::pending() {
            return NONE;
        }
        if container::equals(item, target) {
            count += 1;
        }
    }
    ops::int_result(count)
}

fn index_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.index") else {
        return NONE;
    };
    let Some(&target) = args.get(1) else {
        return type_error("index() missing required argument 'v'");
    };
    let len = items_len(obj, descr) as i64;
    let mut start = 0i64;
    if let Some(&arg) = args.get(2) {
        match slice_index_arg(arg) {
            Ok(value) => {
                start = if value < 0 {
                    value.saturating_add(len).max(0)
                } else {
                    value
                }
            }
            Err(error) => return error,
        }
    }
    let mut stop = i64::MAX;
    if let Some(&arg) = args.get(3) {
        match slice_index_arg(arg) {
            Ok(value) => {
                stop = if value < 0 {
                    value.saturating_add(len)
                } else {
                    value
                };
            }
            Err(error) => return error,
        }
    }
    let raw = raw_snapshot(obj);
    let mut position = start;
    while position < stop && position < len {
        let begin = position as usize * descr.itemsize;
        let item = decode_item(descr, &raw[begin..begin + descr.itemsize]);
        if exc::pending() {
            return NONE;
        }
        if container::equals(item, target) {
            return ops::int_result(position);
        }
        position += 1;
    }
    value_error("array.index(x): x not in array")
}

fn reverse_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.reverse") else {
        return NONE;
    };
    let Some(raw) = raw_of(obj) else {
        return type_error("array payload is unavailable");
    };
    let mut out = Vec::with_capacity(raw.len());
    for chunk in raw.chunks(descr.itemsize).rev() {
        out.extend_from_slice(chunk);
    }
    store_raw(obj, out);
    NONE
}

fn tobytes_method(args: &[i64]) -> i64 {
    let Ok((obj, _descr)) = receiver(args, "array.tobytes") else {
        return NONE;
    };
    match raw_of(obj) {
        Some(raw) => value::bytes_new(raw.to_vec()),
        None => type_error("array payload is unavailable"),
    }
}

fn frombytes_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.frombytes") else {
        return NONE;
    };
    let Some(&buffer) = args.get(1) else {
        return type_error("frombytes() takes exactly one argument (0 given)");
    };
    let Some(bytes) = value::as_bytes(buffer) else {
        // CPython's `Py_buffer` accepts any itemsize-1 buffer; the runtime's
        // memory views are byte-sized, so materialise their visible bytes.
        if let Some(view) = value::as_memoryview(buffer) {
            if view.released {
                return exc::raise_with_message(VALUE_ERROR, value::MEMORYVIEW_RELEASED);
            }
            let mut bytes = Vec::with_capacity(view.len());
            for index in 0..view.len() {
                match value::memoryview_get(view, index) {
                    Ok(item) => match value::as_integer(item) {
                        Some(byte) => bytes.push(byte as u8),
                        None => {
                            return type_error(&format!(
                                "a bytes-like object is required, not '{}'",
                                value::type_name_of(buffer)
                            ))
                        }
                    },
                    Err(message) => return container::memoryview_error(message),
                }
            }
            return append_bytes(obj, descr, &bytes);
        }
        return type_error(&format!(
            "a bytes-like object is required, not '{}'",
            value::type_name_of(buffer)
        ));
    };
    append_bytes(obj, descr, bytes)
}

fn tolist_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.tolist") else {
        return NONE;
    };
    tolist_snapshot(obj, descr)
}

/// Builds a list of decoded items.
fn tolist_snapshot(obj: i64, descr: Descr) -> i64 {
    let Some(raw) = raw_of(obj) else {
        return type_error("array payload is unavailable");
    };
    let list = value::list_new(Vec::new());
    let roots = heap::root_values(std::slice::from_ref(&list));
    for chunk in raw.chunks(descr.itemsize) {
        let item = decode_item(descr, chunk);
        if exc::pending() {
            heap::release_roots(roots);
            return NONE;
        }
        container::list_append(list, item);
        if exc::pending() {
            heap::release_roots(roots);
            return NONE;
        }
    }
    heap::release_roots(roots);
    list
}

fn byteswap_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.byteswap") else {
        return NONE;
    };
    if descr.itemsize == 1 {
        return NONE;
    }
    if !matches!(descr.itemsize, 2 | 4 | 8) {
        return exc::raise_with_message(
            RUNTIME_ERROR,
            "don't know how to byteswap this array type",
        );
    }
    let Some(data) = data_of(obj) else {
        return type_error("array payload is unavailable");
    };
    let Some(bytes) = value::bytearray_bytes_mut(data) else {
        return type_error("array payload is unavailable");
    };
    let size = descr.itemsize;
    for chunk in bytes.chunks_mut(size) {
        chunk.reverse();
    }
    NONE
}

fn iter_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.__iter__") else {
        return NONE;
    };
    let list = tolist_snapshot(obj, descr);
    if exc::pending() {
        return NONE;
    }
    let roots = heap::root_values(std::slice::from_ref(&list));
    let iterator = value::iter_new(list);
    heap::release_roots(roots);
    iterator
}

fn contains_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.__contains__") else {
        return NONE;
    };
    let Some(&target) = args.get(1) else {
        return type_error("__contains__() takes exactly one argument");
    };
    let raw = raw_snapshot(obj);
    for chunk in raw.chunks(descr.itemsize) {
        let item = decode_item(descr, chunk);
        if exc::pending() {
            return NONE;
        }
        if container::equals(item, target) {
            return value::TRUE;
        }
    }
    value::FALSE
}

fn repr_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.__repr__") else {
        return NONE;
    };
    let name = value::instance_class_value(obj)
        .and_then(value::class_display_name)
        .unwrap_or("array");
    let len = items_len(obj, descr);
    if len == 0 {
        return value::str_alloc(format!("{name}('{}')", descr.code));
    }
    if descr.code == 'u' {
        let Some(raw) = raw_of(obj) else {
            return type_error("array payload is unavailable");
        };
        let mut text = String::with_capacity(len);
        for chunk in raw.chunks(descr.itemsize) {
            let item = decode_item(descr, chunk);
            if exc::pending() {
                return NONE;
            }
            match value::as_str(item) {
                Some(ch) => text.push_str(ch),
                None => return type_error("array payload is unavailable"),
            }
        }
        let text = value::str_alloc(text);
        let roots = heap::root_values(std::slice::from_ref(&text));
        let rendered = format!("{name}('{}', {})", descr.code, value::repr(text));
        heap::release_roots(roots);
        return value::str_alloc(rendered);
    }
    let list = tolist_snapshot(obj, descr);
    if exc::pending() {
        return NONE;
    }
    let roots = heap::root_values(std::slice::from_ref(&list));
    let rendered = format!("{name}('{}', {})", descr.code, value::repr(list));
    heap::release_roots(roots);
    value::str_alloc(rendered)
}

fn eq_method(args: &[i64]) -> i64 {
    let Ok((obj, descr)) = receiver(args, "array.__eq__") else {
        return NONE;
    };
    let Some(&other) = args.get(1) else {
        return value::NOT_IMPLEMENTED;
    };
    if !is_array(other) {
        return value::NOT_IMPLEMENTED;
    }
    let Some(other_descr) = code_of(other) else {
        return value::NOT_IMPLEMENTED;
    };
    let (Some(left), Some(right)) = (raw_of(obj), raw_of(other)) else {
        return value::NOT_IMPLEMENTED;
    };
    let left_len = left.len() / descr.itemsize;
    let right_len = right.len() / other_descr.itemsize;
    if left_len != right_len {
        return value::FALSE;
    }
    for index in 0..left_len {
        let a = decode_item(
            descr,
            &left[index * descr.itemsize..(index + 1) * descr.itemsize],
        );
        if exc::pending() {
            return NONE;
        }
        let b = decode_item(
            other_descr,
            &right[index * other_descr.itemsize..(index + 1) * other_descr.itemsize],
        );
        if exc::pending() {
            return NONE;
        }
        if !container::equals(a, b) {
            return value::FALSE;
        }
    }
    value::TRUE
}

// -------------------------------------------------------------------------
// Dispatch
// -------------------------------------------------------------------------

/// Dispatches one `array` module function or `array.array` method.
///
/// The qualified method names (`array.append`, ...) are the names carried by
/// the native function values in the class, so the central dispatch reaches
/// them exactly like other native class methods.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "array" => construct(args),
        "array.__init__" => {
            let Some(&obj) = args.first() else {
                return type_error("array.__init__() missing self");
            };
            let rest = &args[1..];
            let fresh = construct(rest);
            if exc::pending() {
                return NONE;
            }
            // Copy the freshly built payload into the allocated instance.
            let (Some(descr), Some(raw)) = (code_of(fresh), raw_of(fresh)) else {
                return type_error("array payload is unavailable");
            };
            let raw = raw.to_vec();
            value::instance_setattr(
                obj,
                CODE_ATTR.to_string(),
                value::str_alloc(descr.code.to_string()),
            );
            value::instance_setattr(
                obj,
                "typecode".to_string(),
                value::str_alloc(descr.code.to_string()),
            );
            value::instance_setattr(
                obj,
                "itemsize".to_string(),
                ops::int_result(descr.itemsize as i64),
            );
            store_raw(obj, raw);
            NONE
        }
        "array.append" => append_method(args),
        "array.extend" => extend_method(args),
        "array.insert" => insert_method(args),
        "array.pop" => pop_method(args),
        "array.remove" => remove_method(args),
        "array.count" => count_method(args),
        "array.index" => index_method(args),
        "array.reverse" => reverse_method(args),
        "array.tobytes" => tobytes_method(args),
        "array.frombytes" => frombytes_method(args),
        "array.tolist" => tolist_method(args),
        "array.byteswap" => byteswap_method(args),
        "array.__getitem__" => getitem_method(args),
        "array.__setitem__" => setitem_method(args),
        "array.__len__" => len_method(args),
        "array.__iter__" => iter_method(args),
        "array.__contains__" => contains_method(args),
        "array.__repr__" => repr_method(args),
        "array.__eq__" => eq_method(args),
        _ => attribute_error(function),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> i64 {
        // Tagged immediates are 61-bit; larger values (e.g. `i64::MAX`) become
        // heap BigInts, exactly like integer results elsewhere in the runtime.
        ops::int_result(n)
    }

    fn text(s: &str) -> i64 {
        value::str_alloc(s.to_string())
    }

    fn new_array(code: &str, initial: Option<i64>) -> i64 {
        let mut args = vec![text(code)];
        if let Some(initial) = initial {
            args.push(initial);
        }
        call("array", &args)
    }

    fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(obj);
        argv.extend_from_slice(args);
        call(&format!("array.{name}"), &argv)
    }

    fn list_of(v: i64) -> Vec<i64> {
        value::as_list(v).expect("list").to_vec()
    }

    fn ints(v: i64) -> Vec<i64> {
        list_of(v)
            .into_iter()
            .map(|item| value::as_integer(item).expect("int"))
            .collect()
    }

    fn text_of(v: i64) -> String {
        value::as_str(v).expect("str").to_string()
    }

    fn failed(tag: u32) -> bool {
        let pending = exc::pending_tag();
        exc::clear();
        pending == i64::from(tag)
    }

    /// Asserts the pending exception tag and returns its message, clearing it.
    fn take_error(tag: u32) -> String {
        assert_eq!(
            exc::pending_tag(),
            i64::from(tag),
            "unexpected pending exception"
        );
        let message = value::to_string(exc::pending_value());
        exc::clear();
        message
    }

    #[test]
    fn construct_and_read_items() {
        exc::init();
        let a = new_array("i", Some(value::list_new(vec![int(1), int(2), int(3)])));
        assert!(!exc::pending());
        assert_eq!(text_of(value::instance_attr(a, "typecode").unwrap()), "i");
        assert_eq!(
            value::as_integer(value::instance_attr(a, "itemsize").unwrap()),
            Some(4)
        );
        assert_eq!(value::as_integer(method(a, "__len__", &[])), Some(3));
        assert_eq!(
            value::as_integer(method(a, "__getitem__", &[int(0)])),
            Some(1)
        );
        assert_eq!(
            value::as_integer(method(a, "__getitem__", &[int(-1)])),
            Some(3)
        );
        assert_eq!(ints(method(a, "tolist", &[])), vec![1, 2, 3]);
        assert_eq!(text_of(method(a, "__repr__", &[])), "array('i', [1, 2, 3])");
        assert_eq!(
            text_of(method(new_array("d", None), "__repr__", &[])),
            "array('d')"
        );
    }

    #[test]
    fn initializer_forms_match_cpython() {
        exc::init();
        // bytes / bytearray use the packed representation.
        let from_bytes = new_array("i", Some(value::bytes_new(vec![1, 0, 0, 0, 2, 0, 0, 0])));
        assert_eq!(ints(method(from_bytes, "tolist", &[])), vec![1, 2]);
        // A mapping initializer iterates its keys.
        let mapping = value::dict_new(vec![(int(1), int(2)), (int(3), int(4))]);
        let from_map = new_array("i", Some(mapping));
        assert_eq!(ints(method(from_map, "tolist", &[])), vec![1, 3]);
        // A same-code array is copied; another code iterates (and rejects
        // floats for an integer code).
        let source = new_array("i", Some(value::list_new(vec![int(4), int(5)])));
        let copy = new_array("i", Some(source));
        assert_eq!(ints(method(copy, "tolist", &[])), vec![4, 5]);
        let floats = new_array("f", Some(value::list_new(vec![value::float_new(1.5)])));
        new_array("i", Some(floats));
        assert!(failed(TYPE_ERROR));
        // A `str` initializer is only valid for the unicode code.
        new_array("i", Some(text("ab")));
        assert!(failed(TYPE_ERROR));
        // Bad type code and non-iterable initializer.
        new_array("z", None);
        assert!(failed(VALUE_ERROR));
        new_array("i", Some(int(5)));
        assert!(failed(TYPE_ERROR));
    }

    #[test]
    fn integer_ranges_report_cpython_messages() {
        exc::init();
        let b = new_array("b", None);
        method(b, "append", &[int(300)]);
        assert_eq!(
            take_error(OVERFLOW_ERROR),
            "signed char is greater than maximum"
        );
        let byte = new_array("B", None);
        method(byte, "append", &[int(-1)]);
        assert_eq!(
            take_error(OVERFLOW_ERROR),
            "unsigned byte integer is less than minimum"
        );
        let wide = new_array("H", None);
        method(wide, "append", &[int(70_000)]);
        assert_eq!(
            take_error(OVERFLOW_ERROR),
            "unsigned short is greater than maximum"
        );
        method(wide, "append", &[value::float_new(1.5)]);
        assert_eq!(
            take_error(TYPE_ERROR),
            "'float' object cannot be interpreted as an integer"
        );
        let unsigned = new_array("I", None);
        method(unsigned, "append", &[int(-1)]);
        assert_eq!(
            take_error(OVERFLOW_ERROR),
            "can't convert negative value to unsigned int"
        );
        let long = new_array("l", None);
        // `i64::MAX` exceeds the tagged-integer range; pass an exact BigInt.
        let huge = value::bigint_new(BigInt::from(i64::MAX));
        method(long, "append", &[huge]);
        assert!(!exc::pending());
        assert_eq!(ints(method(long, "tolist", &[])), vec![i64::MAX]);
        let float = new_array("f", None);
        method(float, "append", &[text("x")]);
        assert_eq!(take_error(TYPE_ERROR), "must be real number, not str");
    }

    #[test]
    fn mutation_methods() {
        exc::init();
        let a = new_array("i", Some(value::list_new(vec![int(1), int(2)])));
        method(a, "append", &[int(3)]);
        method(a, "insert", &[int(0), int(0)]);
        method(a, "insert", &[int(100), int(9)]);
        method(a, "insert", &[int(-100), int(-1)]);
        assert_eq!(ints(method(a, "tolist", &[])), vec![-1, 0, 1, 2, 3, 9]);
        assert_eq!(value::as_integer(method(a, "pop", &[])), Some(9));
        assert_eq!(value::as_integer(method(a, "pop", &[int(0)])), Some(-1));
        method(a, "remove", &[int(1)]);
        assert_eq!(ints(method(a, "tolist", &[])), vec![0, 2, 3]);
        assert_eq!(value::as_integer(method(a, "count", &[int(3)])), Some(1));
        assert_eq!(value::as_integer(method(a, "index", &[int(2)])), Some(1));
        method(a, "index", &[int(99)]);
        assert!(failed(VALUE_ERROR));
        method(a, "reverse", &[]);
        assert_eq!(ints(method(a, "tolist", &[])), vec![3, 2, 0]);
        assert_eq!(method(a, "__contains__", &[int(2)]), value::TRUE);
        assert_eq!(method(a, "__contains__", &[int(7)]), value::FALSE);
        let empty = new_array("i", None);
        method(empty, "pop", &[]);
        assert!(failed(INDEX_ERROR));
    }

    #[test]
    fn bytes_round_trip_and_byteswap() {
        exc::init();
        let a = new_array("i", Some(value::list_new(vec![int(1), int(2)])));
        let packed = method(a, "tobytes", &[]);
        assert_eq!(
            value::as_bytes(packed).expect("bytes").to_vec(),
            vec![1, 0, 0, 0, 2, 0, 0, 0]
        );
        let restored = new_array("i", None);
        method(restored, "frombytes", &[packed]);
        assert_eq!(ints(method(restored, "tolist", &[])), vec![1, 2]);
        method(restored, "frombytes", &[value::bytes_new(vec![0])]);
        assert_eq!(
            take_error(VALUE_ERROR),
            "bytes length not a multiple of item size"
        );
        method(restored, "frombytes", &[text("x")]);
        assert!(failed(TYPE_ERROR));

        method(a, "byteswap", &[]);
        assert_eq!(
            value::as_bytes(method(a, "tobytes", &[]))
                .expect("bytes")
                .to_vec(),
            vec![0, 0, 0, 1, 0, 0, 0, 2]
        );
        let one = new_array("b", Some(value::list_new(vec![int(1)])));
        method(one, "byteswap", &[]);
        assert_eq!(ints(method(one, "tolist", &[])), vec![1]);
    }

    #[test]
    fn slices_read_write_and_delete() {
        exc::init();
        let a = new_array(
            "i",
            Some(value::list_new(vec![
                int(1),
                int(2),
                int(3),
                int(4),
                int(5),
            ])),
        );
        let forward = method(
            a,
            "__getitem__",
            &[crate::builtins::slice_new(&[int(1), int(3)])],
        );
        assert_eq!(ints(method(forward, "tolist", &[])), vec![2, 3]);
        let backward = method(
            a,
            "__getitem__",
            &[crate::builtins::slice_new(&[NONE, NONE, int(-1)])],
        );
        assert_eq!(ints(method(backward, "tolist", &[])), vec![5, 4, 3, 2, 1]);

        let replacement = new_array("i", Some(value::list_new(vec![int(9), int(9)])));
        method(
            a,
            "__setitem__",
            &[crate::builtins::slice_new(&[int(0), int(1)]), replacement],
        );
        assert_eq!(ints(method(a, "tolist", &[])), vec![9, 9, 2, 3, 4, 5]);
        // Extended slice assignment requires an exact size match.
        method(
            a,
            "__setitem__",
            &[
                crate::builtins::slice_new(&[NONE, NONE, int(2)]),
                new_array("i", Some(value::list_new(vec![int(7)]))),
            ],
        );
        assert!(failed(VALUE_ERROR));
        // Deleting an extended slice with an empty array.
        method(
            a,
            "__setitem__",
            &[
                crate::builtins::slice_new(&[NONE, NONE, int(2)]),
                new_array("i", None),
            ],
        );
        assert_eq!(ints(method(a, "tolist", &[])), vec![9, 3, 5]);
        // A non-array right-hand side is rejected.
        method(
            a,
            "__setitem__",
            &[
                crate::builtins::slice_new(&[int(0), int(1)]),
                value::list_new(vec![int(1)]),
            ],
        );
        assert!(failed(TYPE_ERROR));
    }

    #[test]
    fn unicode_code_uses_characters() {
        exc::init();
        let a = new_array("u", Some(text("abc")));
        assert_eq!(text_of(method(a, "__getitem__", &[int(1)])), "b");
        assert_eq!(text_of(method(a, "__repr__", &[])), "array('u', 'abc')");
        method(a, "append", &[text("d")]);
        let items = list_of(method(a, "tolist", &[]));
        let chars: Vec<String> = items.iter().map(|item| text_of(*item)).collect();
        assert_eq!(chars, vec!["a", "b", "c", "d"]);
        method(a, "append", &[text("de")]);
        assert!(failed(TYPE_ERROR));
        method(a, "append", &[int(1)]);
        assert!(failed(TYPE_ERROR));
    }

    #[test]
    fn extend_and_equality() {
        exc::init();
        let a = new_array("i", Some(value::list_new(vec![int(1)])));
        method(
            a,
            "extend",
            &[new_array("i", Some(value::list_new(vec![int(2), int(3)])))],
        );
        method(a, "extend", &[value::list_new(vec![int(4)])]);
        assert_eq!(ints(method(a, "tolist", &[])), vec![1, 2, 3, 4]);
        method(
            a,
            "extend",
            &[new_array(
                "f",
                Some(value::list_new(vec![value::float_new(1.0)])),
            )],
        );
        assert_eq!(
            take_error(TYPE_ERROR),
            "can only extend with array of same kind"
        );
        let same = new_array(
            "i",
            Some(value::list_new(vec![int(1), int(2), int(3), int(4)])),
        );
        assert_eq!(method(a, "__eq__", &[same]), value::TRUE);
        let floats = new_array(
            "f",
            Some(value::list_new(vec![
                value::float_new(1.0),
                value::float_new(2.0),
                value::float_new(3.0),
                value::float_new(4.0),
            ])),
        );
        assert_eq!(method(a, "__eq__", &[floats]), value::TRUE);
        assert_eq!(
            method(a, "__eq__", &[value::list_new(vec![])]),
            value::NOT_IMPLEMENTED
        );
        let iterator = method(a, "__iter__", &[]);
        let mut seen = Vec::new();
        loop {
            let mut item = NONE;
            if iter::iter_next(iterator, &mut item) == 0 {
                break;
            }
            seen.push(value::as_integer(item).expect("int"));
        }
        assert_eq!(seen, vec![1, 2, 3, 4]);
    }

    #[test]
    fn module_constants() {
        exc::init();
        assert_eq!(
            text_of(constant("typecodes").expect("typecodes")),
            TYPECODES
        );
        let class = constant("array").expect("class");
        assert_eq!(value::class_display_name(class), Some("array"));
        assert_eq!(constant("nope"), None);
    }
}
