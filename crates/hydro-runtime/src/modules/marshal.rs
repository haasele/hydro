//! `marshal`: CPython's internal object-serialization format.
//!
//! The byte stream follows CPython 3.14's `Python/marshal.c`: the same type
//! codes, little-endian 32/64-bit fields, 15-bit `int` "long" digits, binary
//! floats/complexes for versions above 1, short-ASCII strings for version 4
//! and up, and `FLAG_REF` shared references (which `loads` honours; `dumps`
//! writes every object inline, which is valid marshal data and round-trips).
//!
//! `dumps(value, version=4, *, allow_code=True)` validates `version` the same
//! way CPython does (`0..=5`) and reports `"allow_code" requires at least
//! marshalling version 4` for the incompatible combination. `loads` accepts
//! every type code the runtime can represent.
//!
//! Code objects are the one gap: Hydro's `CodeState` carries source metadata
//! rather than CPython bytecode, so marshalling a code object reports a
//! `ValueError` instead of inventing a payload the runtime cannot execute.
//! Data values (`None`, `bool`, `int`, `float`, `complex`, `bytes`, `str`,
//! `tuple`, `list`, `dict`, `set`/`frozenset` and `slice`) are fully
//! supported in both directions.

use num_bigint::{BigInt, BigUint, Sign};
use num_traits::{ToPrimitive, Zero};

use crate::container;
use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::ops;
use crate::value::{self, NONE};

/// CPython 3.14 marshal format version (`Doc/library/marshal.rst`).
pub(crate) const VERSION: i64 = 5;

/// `MAX_MARSHAL_STACK_DEPTH` from `Python/marshal.c`.
const MAX_DEPTH: usize = 2000;

/// `FLAG_REF`: the object is added to the reference table.
const FLAG_REF: u8 = 0x80;

const TYPE_NULL: u8 = b'0';
const TYPE_NONE: u8 = b'N';
const TYPE_FALSE: u8 = b'F';
const TYPE_TRUE: u8 = b'T';
const TYPE_STOPITER: u8 = b'S';
const TYPE_ELLIPSIS: u8 = b'.';
const TYPE_INT: u8 = b'i';
const TYPE_INT64: u8 = b'I';
const TYPE_FLOAT: u8 = b'f';
const TYPE_BINARY_FLOAT: u8 = b'g';
const TYPE_COMPLEX: u8 = b'x';
const TYPE_BINARY_COMPLEX: u8 = b'y';
const TYPE_LONG: u8 = b'l';
const TYPE_STRING: u8 = b's';
const TYPE_INTERNED: u8 = b't';
const TYPE_REF: u8 = b'r';
const TYPE_TUPLE: u8 = b'(';
const TYPE_LIST: u8 = b'[';
const TYPE_DICT: u8 = b'{';
const TYPE_CODE: u8 = b'c';
const TYPE_UNICODE: u8 = b'u';
const TYPE_SET: u8 = b'<';
const TYPE_FROZENSET: u8 = b'>';
const TYPE_ASCII: u8 = b'a';
const TYPE_ASCII_INTERNED: u8 = b'A';
const TYPE_SMALL_TUPLE: u8 = b')';
const TYPE_SHORT_ASCII: u8 = b'z';
const TYPE_SHORT_ASCII_INTERNED: u8 = b'Z';
const TYPE_SLICE: u8 = b':';

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn bad_data() -> i64 {
    value_error("bad marshal data (unknown type code)")
}

// -------------------------------------------------------------------------
// Encoder
// -------------------------------------------------------------------------

struct Encoder {
    out: Vec<u8>,
    version: i64,
}

impl Encoder {
    fn byte(&mut self, byte: u8) {
        self.out.push(byte);
    }

    fn short(&mut self, value: u32) {
        self.out.extend_from_slice(&(value as u16).to_le_bytes());
    }

    fn long(&mut self, value: i32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    fn size(&mut self, value: usize) {
        self.long(value as i32);
    }

    fn float_bytes(&mut self, number: f64) {
        self.out.extend_from_slice(&number.to_bits().to_le_bytes());
    }

    fn float_text(&mut self, number: f64) {
        let text = value::format_float(number);
        self.short(text.len() as u32);
        self.out.extend_from_slice(text.as_bytes());
    }

    /// `w_PyLong`: 15-bit absolute-value digits, sign carried by the count.
    fn big_int(&mut self, number: &BigInt) {
        let (sign, magnitude) = number.clone().into_parts();
        let mut magnitude = magnitude;
        let mut digits: Vec<u16> = Vec::new();
        let base = BigUint::from(1u32 << 15);
        while !magnitude.is_zero() {
            let digit = (&magnitude % &base).to_u32().unwrap_or(0) as u16;
            digits.push(digit);
            magnitude /= &base;
        }
        let count = digits.len() as i32;
        self.long(if sign == Sign::Minus { -count } else { count });
        for digit in digits {
            self.short(u32::from(digit));
        }
    }

    fn value(&mut self, item: i64, depth: usize) -> Result<(), i64> {
        if depth > MAX_DEPTH {
            return Err(value_error("object too deeply nested to marshal"));
        }
        if item == NONE {
            self.byte(TYPE_NONE);
            return Ok(());
        }
        if item == value::TRUE {
            self.byte(TYPE_TRUE);
            return Ok(());
        }
        if item == value::FALSE {
            self.byte(TYPE_FALSE);
            return Ok(());
        }
        if let Some(number) = value::as_integer(item) {
            return self.integer(number);
        }
        if let Some(number) = value::as_bigint(item) {
            self.byte(TYPE_LONG);
            self.big_int(number);
            return Ok(());
        }
        if let Some(number) = value::as_float(item) {
            if self.version > 1 {
                self.byte(TYPE_BINARY_FLOAT);
                self.float_bytes(number);
            } else {
                self.byte(TYPE_FLOAT);
                self.float_text(number);
            }
            return Ok(());
        }
        if value::instance_class_value(item) == Some(crate::builtins::complex_class()) {
            let (real, imag) = crate::builtins::complex_components(item).unwrap_or((0.0, 0.0));
            if self.version > 1 {
                self.byte(TYPE_BINARY_COMPLEX);
                self.float_bytes(real);
                self.float_bytes(imag);
            } else {
                self.byte(TYPE_COMPLEX);
                self.float_text(real);
                self.float_text(imag);
            }
            return Ok(());
        }
        if let Some(bytes) = value::as_bytes(item) {
            // Only exact `bytes` objects marshal as TYPE_STRING; a
            // `bytearray` is unmarshallable in CPython.
            if value::as_bytearray(item).is_some() {
                return Err(value_error("unmarshallable object"));
            }
            self.byte(TYPE_STRING);
            self.size(bytes.len());
            self.out.extend_from_slice(bytes);
            return Ok(());
        }
        if let Some(text) = value::as_str(item) {
            let bytes = text.as_bytes();
            if self.version >= 4 && text.is_ascii() {
                if bytes.len() < 256 {
                    self.byte(TYPE_SHORT_ASCII);
                    self.byte(bytes.len() as u8);
                } else {
                    self.byte(TYPE_ASCII);
                    self.size(bytes.len());
                }
            } else {
                self.byte(TYPE_UNICODE);
                self.size(bytes.len());
            }
            self.out.extend_from_slice(bytes);
            return Ok(());
        }
        if let Some(items) = value::as_tuple(item) {
            self.tuple(items, depth)?;
            return Ok(());
        }
        if let Some(items) = value::as_list(item) {
            let items = items.to_vec();
            self.byte(TYPE_LIST);
            self.size(items.len());
            for element in items {
                self.value(element, depth + 1)?;
            }
            return Ok(());
        }
        if let Some(pairs) = value::as_dict(item) {
            let pairs = pairs.to_vec();
            self.byte(TYPE_DICT);
            for (key, element) in pairs {
                self.value(key, depth + 1)?;
                self.value(element, depth + 1)?;
            }
            self.byte(TYPE_NULL);
            return Ok(());
        }
        if let Some(items) = value::as_set(item) {
            let items = items.to_vec();
            self.byte(TYPE_SET);
            self.size(items.len());
            for element in items {
                self.value(element, depth + 1)?;
            }
            return Ok(());
        }
        if value::instance_class_value(item) == Some(crate::builtins::slice_class()) {
            let start = value::instance_attr(item, "start").unwrap_or(NONE);
            let stop = value::instance_attr(item, "stop").unwrap_or(NONE);
            let step = value::instance_attr(item, "step").unwrap_or(NONE);
            self.byte(TYPE_SLICE);
            self.value(start, depth + 1)?;
            self.value(stop, depth + 1)?;
            self.value(step, depth + 1)?;
            return Ok(());
        }
        if value::as_code(item).is_some() {
            // Hydro has no CPython bytecode to serialize; inventing a code
            // payload would produce something the runtime cannot execute.
            return Err(value_error(
                "marshal: code objects cannot be marshalled by this runtime",
            ));
        }
        Err(value_error("unmarshallable object"))
    }

    fn tuple(&mut self, items: &[i64], depth: usize) -> Result<(), i64> {
        let items = items.to_vec();
        if self.version >= 4 && items.len() < 256 {
            self.byte(TYPE_SMALL_TUPLE);
            self.byte(items.len() as u8);
        } else {
            self.byte(TYPE_TUPLE);
            self.size(items.len());
        }
        for element in items {
            self.value(element, depth + 1)?;
        }
        Ok(())
    }

    fn integer(&mut self, number: i64) -> Result<(), i64> {
        match i32::try_from(number) {
            Ok(small) => {
                self.byte(TYPE_INT);
                self.long(small);
            }
            Err(_) => {
                self.byte(TYPE_LONG);
                self.big_int(&BigInt::from(number));
            }
        }
        Ok(())
    }
}

/// `marshal.dumps(value, version=4, *, allow_code=True)`.
fn encode(value_arg: i64, version: i64) -> Result<Vec<u8>, i64> {
    let mut encoder = Encoder {
        out: Vec::new(),
        version,
    };
    encoder.value(value_arg, 0)?;
    Ok(encoder.out)
}

// -------------------------------------------------------------------------
// Decoder
// -------------------------------------------------------------------------

struct Decoder<'a> {
    data: &'a [u8],
    position: usize,
    refs: Vec<i64>,
}

impl<'a> Decoder<'a> {
    fn read(&mut self, count: usize) -> Result<&'a [u8], i64> {
        let end = self
            .position
            .checked_add(count)
            .ok_or_else(|| value_error("bad marshal data (string size out of range)"))?;
        if end > self.data.len() {
            // `r_string` reports a short `loads()` buffer as `EOFError`;
            // `marshal.loads()`-truncated data is the documented trigger.
            return Err(exc::raise_with_message(
                exc::EOF_ERROR,
                "marshal data too short",
            ));
        }
        let slice = &self.data[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8, i64> {
        Ok(self.read(1)?[0])
    }

    fn short(&mut self) -> Result<u16, i64> {
        let bytes = self.read(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn long(&mut self) -> Result<i32, i64> {
        let bytes = self.read(4)?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn size(&mut self) -> Result<usize, i64> {
        let size = self.long()?;
        usize::try_from(size)
            .map_err(|_| value_error("bad marshal data (object size out of range)"))
    }

    fn float(&mut self) -> Result<f64, i64> {
        let bytes = self.read(8)?;
        let mut raw = [0u8; 8];
        raw.copy_from_slice(bytes);
        Ok(f64::from_bits(u64::from_le_bytes(raw)))
    }

    fn float_text(&mut self) -> Result<f64, i64> {
        let length = usize::from(self.short()?);
        let bytes = self.read(length)?;
        let text =
            std::str::from_utf8(bytes).map_err(|_| value_error("bad marshal data (float text)"))?;
        text.parse::<f64>()
            .map_err(|_| value_error("bad marshal data (float text)"))
    }

    fn big_int(&mut self) -> Result<i64, i64> {
        let count = self.long()?;
        let negative = count < 0;
        let digits = count.unsigned_abs() as usize;
        if digits > 0x1000_0000 {
            return Err(value_error("bad marshal data (long size out of range)"));
        }
        let mut magnitude = BigUint::zero();
        for _ in 0..digits {
            let digit = self.short()?;
            magnitude = (magnitude << 15u32) + BigUint::from(u32::from(digit));
        }
        let number =
            BigInt::from_biguint(if negative { Sign::Minus } else { Sign::Plus }, magnitude);
        Ok(match number.to_i64() {
            Some(small) => ops::int_result(small),
            None => value::bigint_new(number),
        })
    }

    fn value(&mut self, depth: usize) -> Result<i64, i64> {
        if depth > MAX_DEPTH {
            return Err(value_error("object too deeply nested to unmarshal"));
        }
        let code = self.byte()?;
        let flagged = code & FLAG_REF != 0;
        let code = code & !FLAG_REF;
        // Lists register themselves before their items are decoded so a
        // recursive payload can reference them; every other type registers
        // after construction.
        let mut registered = false;
        let result = match code {
            TYPE_NULL => return Err(value_error("bad marshal data (NULL object)")),
            TYPE_NONE => NONE,
            TYPE_FALSE => value::FALSE,
            TYPE_TRUE => value::TRUE,
            TYPE_STOPITER => {
                return Err(value_error(
                    "marshal: StopIteration cannot be represented by this runtime",
                ))
            }
            TYPE_ELLIPSIS => {
                return Err(value_error(
                    "marshal: Ellipsis cannot be represented by this runtime",
                ))
            }
            TYPE_INT => {
                let number = self.long()?;
                ops::int_result(i64::from(number))
            }
            TYPE_INT64 => {
                let bytes = self.read(8)?;
                let mut raw = [0u8; 8];
                raw.copy_from_slice(bytes);
                let number = i64::from_le_bytes(raw);
                ops::int_result(number)
            }
            TYPE_LONG => self.big_int()?,
            TYPE_FLOAT => value::float_new(self.float_text()?),
            TYPE_BINARY_FLOAT => value::float_new(self.float()?),
            TYPE_COMPLEX | TYPE_BINARY_COMPLEX => {
                let (real, imag) = if code == TYPE_COMPLEX {
                    (self.float_text()?, self.float_text()?)
                } else {
                    (self.float()?, self.float()?)
                };
                crate::builtins::build_complex(real, imag)
            }
            TYPE_STRING => {
                let length = self.size()?;
                let bytes = self.read(length)?.to_vec();
                value::bytes_new(bytes)
            }
            TYPE_ASCII | TYPE_ASCII_INTERNED | TYPE_UNICODE | TYPE_INTERNED => {
                let length = self.size()?;
                let bytes = self.read(length)?;
                match std::str::from_utf8(bytes) {
                    Ok(text) => value::str_alloc(text.to_string()),
                    Err(_) => return Err(value_error("bad marshal data (string is not UTF-8)")),
                }
            }
            TYPE_SHORT_ASCII | TYPE_SHORT_ASCII_INTERNED => {
                let length = usize::from(self.byte()?);
                let bytes = self.read(length)?;
                match std::str::from_utf8(bytes) {
                    Ok(text) => value::str_alloc(text.to_string()),
                    Err(_) => return Err(value_error("bad marshal data (string is not UTF-8)")),
                }
            }
            TYPE_TUPLE | TYPE_SMALL_TUPLE => {
                let length = if code == TYPE_SMALL_TUPLE {
                    usize::from(self.byte()?)
                } else {
                    self.size()?
                };
                if length > self.data.len() {
                    return Err(value_error("bad marshal data (tuple size out of range)"));
                }
                let mut items = Vec::with_capacity(length);
                for _ in 0..length {
                    items.push(self.value(depth + 1)?);
                }
                value::tuple_new(items)
            }
            TYPE_LIST => {
                let length = self.size()?;
                let list = value::list_new(Vec::new());
                let roots = heap::root_values(std::slice::from_ref(&list));
                if flagged {
                    self.refs.push(list);
                    registered = true;
                }
                for _ in 0..length {
                    let item = self.value(depth + 1)?;
                    if exc::pending() {
                        heap::release_roots(roots);
                        return Ok(NONE);
                    }
                    container::list_append(list, item);
                }
                heap::release_roots(roots);
                list
            }
            TYPE_DICT => {
                let mut pairs = Vec::new();
                loop {
                    // The terminator is a bare TYPE_NULL, not a value.
                    if self.peek_is_null() {
                        self.position += 1;
                        break;
                    }
                    let key = self.value(depth + 1)?;
                    let element = self.value(depth + 1)?;
                    pairs.push((key, element));
                }
                value::dict_new(pairs)
            }
            TYPE_SET | TYPE_FROZENSET => {
                let length = self.size()?;
                let mut items: Vec<i64> = Vec::with_capacity(length);
                for _ in 0..length {
                    let item = self.value(depth + 1)?;
                    if !container::set_contains(&items, item) {
                        items.push(item);
                    }
                }
                value::set_new(items)
            }
            TYPE_SLICE => {
                let mut parts = [NONE; 3];
                for slot in &mut parts {
                    *slot = self.value(depth + 1)?;
                }
                let class = crate::builtins::slice_class();
                let instance = value::instance_new(class);
                let roots = heap::root_values(std::slice::from_ref(&instance));
                value::instance_setattr(instance, "start".to_string(), parts[0]);
                value::instance_setattr(instance, "stop".to_string(), parts[1]);
                value::instance_setattr(instance, "step".to_string(), parts[2]);
                heap::release_roots(roots);
                instance
            }
            TYPE_REF => {
                let index = self.long()?;
                let index = usize::try_from(index)
                    .map_err(|_| value_error("bad marshal data (invalid reference)"))?;
                match self.refs.get(index) {
                    Some(item) => *item,
                    None => return Err(value_error("bad marshal data (invalid reference)")),
                }
            }
            TYPE_CODE => {
                return Err(value_error(
                    "marshal: code objects cannot be loaded by this runtime",
                ))
            }
            _ => return Err(bad_data()),
        };
        if flagged && !registered && code != TYPE_REF {
            self.refs.push(result);
        }
        Ok(result)
    }

    /// Whether the next byte is `TYPE_NULL` (dict terminator).
    fn peek_is_null(&self) -> bool {
        self.data.get(self.position) == Some(&TYPE_NULL)
    }
}

// -------------------------------------------------------------------------
// Module surface
// -------------------------------------------------------------------------

/// Version check shared by `dump`/`dumps`.
fn check_version(version: i64, allow_code: bool) -> Result<(), i64> {
    if !(0..=VERSION).contains(&version) {
        return Err(value_error("bad marshal version"));
    }
    if !allow_code && version < 4 {
        return Err(value_error(
            "\"allow_code\" requires at least marshalling version 4",
        ));
    }
    Ok(())
}

fn dumps(args: &[i64]) -> i64 {
    let Some(&value_arg) = args.first() else {
        return type_error("dumps() takes at least 1 argument (0 given)");
    };
    let version = match args.get(1).copied() {
        Some(version_arg) if version_arg != NONE => match value::as_integer(version_arg) {
            Some(version) => version,
            None => return type_error("an integer is required"),
        },
        _ => 4,
    };
    let allow_code = match args.get(2).copied() {
        Some(flag) if flag != NONE => value::truthy(flag),
        _ => true,
    };
    if let Err(error) = check_version(version, allow_code) {
        return error;
    }
    match encode(value_arg, version) {
        Ok(bytes) => value::bytes_new(bytes),
        Err(error) => error,
    }
}

fn dump(args: &[i64]) -> i64 {
    let Some(&value_arg) = args.first() else {
        return type_error("dump() takes at least 2 arguments (0 given)");
    };
    let Some(&file) = args.get(1) else {
        return type_error("dump() takes at least 2 arguments (1 given)");
    };
    let version = match args.get(2).copied() {
        Some(version_arg) if version_arg != NONE => match value::as_integer(version_arg) {
            Some(version) => version,
            None => return type_error("an integer is required"),
        },
        _ => 4,
    };
    let allow_code = match args.get(3).copied() {
        Some(flag) if flag != NONE => value::truthy(flag),
        _ => true,
    };
    if let Err(error) = check_version(version, allow_code) {
        return error;
    }
    let encoded = match encode(value_arg, version) {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    let bytes = value::bytes_new(encoded);
    let roots = heap::root_values(std::slice::from_ref(&bytes));
    let write = b"write";
    let _ = crate::methods::call(file, write.as_ptr(), write.len(), 1, &bytes as *const i64);
    heap::release_roots(roots);
    if exc::pending() {
        return NONE;
    }
    NONE
}

fn loads(args: &[i64]) -> i64 {
    let Some(&data_arg) = args.first() else {
        return type_error("loads() takes exactly 1 argument (0 given)");
    };
    let Some(data) = value::as_bytes(data_arg).map(<[u8]>::to_vec) else {
        return type_error(&format!(
            "a bytes-like object is required, not '{}'",
            value::type_name(data_arg)
        ));
    };
    let mut decoder = Decoder {
        data: &data,
        position: 0,
        refs: Vec::new(),
    };
    match decoder.value(0) {
        Ok(result) => result,
        Err(error) => error,
    }
}

fn load(args: &[i64]) -> i64 {
    let Some(&file) = args.first() else {
        return type_error("load() takes exactly 1 argument (0 given)");
    };
    let read = b"read";
    let contents = crate::methods::call(file, read.as_ptr(), read.len(), 0, std::ptr::null());
    if exc::pending() {
        return NONE;
    }
    let Some(data) = value::as_bytes(contents).map(<[u8]>::to_vec) else {
        return type_error("read() didn't return bytes");
    };
    let mut decoder = Decoder {
        data: &data,
        position: 0,
        refs: Vec::new(),
    };
    match decoder.value(0) {
        Ok(result) => result,
        Err(error) => error,
    }
}

/// Dispatches a native `marshal` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "dump" => dump(args),
        "dumps" => dumps(args),
        "load" => load(args),
        "loads" => loads(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'marshal' has no attribute '{function}'"),
        ),
    }
}

/// Materialises a `marshal` constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "version" => ops::int_result(VERSION),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'marshal' has no attribute '{name}'"),
        ),
    }
}
