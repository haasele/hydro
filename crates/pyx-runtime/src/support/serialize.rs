//! PyX-owned binary serialization used by `pickle` and `multiprocessing.Queue`.
//!
//! This is *not* CPython's pickle format. It is a small, bounds-checked
//! encoding of the runtime's values with a magic header and a version byte:
//!
//! ```text
//! "PYX1" <version: u8> <value>
//! ```
//!
//! Supported values: `None`, `True`/`False`, tagged integers, floats,
//! strings, bytes, lists, tuples, dicts, sets, ranges and generated function
//! values.
//! Function values are restored from their wrapper address, so they are only
//! meaningful in the same process (or an address-space copy created by
//! `fork`).
//!
//! The `spawn` mode of [`encode`]/[`decode`] targets a fresh process instead:
//! functions carry their name and arity and are resolved through the runtime
//! registry ([`crate::spawn`]), and `multiprocessing.Queue` objects carry
//! their inherited descriptors. Native functions and other native objects
//! stay unsupported in both modes.
//!
//! Encoding rejects cycles (the traversal is depth-limited) and unsupported
//! types; decoding never panics on malformed input and reports
//! [`PickleError::Corrupt`] instead. Containers are built incrementally so
//! partially decoded values stay reachable for the conservative collector.

use crate::value::{self, FunctionBody, NONE};

/// Magic bytes at the start of every encoded payload.
const MAGIC: &[u8; 4] = b"PYX1";
/// Current format version.
const VERSION: u8 = 1;
/// Maximum container nesting accepted by both directions.
const MAX_DEPTH: usize = 128;
/// Upper bound for a single string/function-name length.
const MAX_NAME: usize = u32::MAX as usize;

// Value tags.
const TAG_NONE: u8 = 0;
const TAG_FALSE: u8 = 1;
const TAG_TRUE: u8 = 2;
const TAG_INT: u8 = 3;
const TAG_FLOAT: u8 = 4;
const TAG_STR: u8 = 5;
const TAG_LIST: u8 = 6;
const TAG_TUPLE: u8 = 7;
const TAG_DICT: u8 = 8;
const TAG_SET: u8 = 9;
const TAG_RANGE: u8 = 10;
const TAG_FUNCTION: u8 = 11;
/// Function by name (spawn mode only).
const TAG_FUNCTION_NAME: u8 = 12;
/// `multiprocessing.Queue` descriptors (spawn mode only).
const TAG_MP_QUEUE: u8 = 13;
/// Immutable bytes payload (`u32` length + raw bytes).
const TAG_BYTES: u8 = 14;

/// Reason a value could not be serialized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PickleError {
    /// The value contains a type the format cannot represent.
    Unsupported(&'static str),
    /// Container nesting exceeded [`MAX_DEPTH`] (or a cyclic structure was
    /// encountered).
    TooDeep,
    /// The input is malformed or truncated.
    Corrupt,
}

impl PickleError {
    /// Message used by `pickle.dumps`.
    pub(crate) fn dumps_message(self) -> String {
        match self {
            PickleError::Unsupported(ty) => format!("cannot pickle '{ty}' object"),
            PickleError::TooDeep => "maximum pickle recursion depth exceeded".to_string(),
            PickleError::Corrupt => "invalid pickle data".to_string(),
        }
    }

    /// Message used by `pickle.loads` and queue decoding.
    pub(crate) fn loads_message(self) -> String {
        match self {
            PickleError::Unsupported(ty) => format!("cannot unpickle '{ty}' object"),
            PickleError::TooDeep => "maximum pickle recursion depth exceeded".to_string(),
            PickleError::Corrupt => "invalid pickle data".to_string(),
        }
    }
}

// -------------------------------------------------------------------------
// Encoding
// -------------------------------------------------------------------------

/// Encodes `value` (the body only, with magic and version prefix).
///
/// `spawn` selects the transport used by the `spawn` start method: functions
/// are written by name and `multiprocessing.Queue` descriptors are included.
/// The default pickle/queue mode rejects both native forms.
pub(crate) fn encode(value: i64, spawn: bool) -> Result<Vec<u8>, PickleError> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    encode_value(value, 0, spawn, &mut out)?;
    Ok(out)
}

fn write_len(out: &mut Vec<u8>, len: usize) -> Result<(), PickleError> {
    let Ok(len) = u32::try_from(len) else {
        return Err(PickleError::Unsupported("container"));
    };
    out.extend_from_slice(&len.to_le_bytes());
    Ok(())
}

fn encode_value(
    value: i64,
    depth: usize,
    spawn: bool,
    out: &mut Vec<u8>,
) -> Result<(), PickleError> {
    if value == NONE {
        out.push(TAG_NONE);
        return Ok(());
    }
    if value == value::FALSE {
        out.push(TAG_FALSE);
        return Ok(());
    }
    if value == value::TRUE {
        out.push(TAG_TRUE);
        return Ok(());
    }
    if let Some(number) = value::as_integer(value) {
        out.push(TAG_INT);
        out.extend_from_slice(&number.to_le_bytes());
        return Ok(());
    }
    if let Some(number) = value::as_float(value) {
        out.push(TAG_FLOAT);
        out.extend_from_slice(&number.to_bits().to_le_bytes());
        return Ok(());
    }
    if let Some(text) = value::as_str(value) {
        if text.len() > MAX_NAME {
            return Err(PickleError::Unsupported("str"));
        }
        out.push(TAG_STR);
        write_len(out, text.len())?;
        out.extend_from_slice(text.as_bytes());
        return Ok(());
    }
    if let Some(bytes) = value::as_bytes(value) {
        out.push(TAG_BYTES);
        write_len(out, bytes.len())?;
        out.extend_from_slice(bytes);
        return Ok(());
    }
    if depth >= MAX_DEPTH {
        return Err(PickleError::TooDeep);
    }
    if let Some(items) = value::as_list(value) {
        out.push(TAG_LIST);
        write_len(out, items.len())?;
        for &item in items {
            encode_value(item, depth + 1, spawn, out)?;
        }
        return Ok(());
    }
    if let Some(items) = value::as_tuple(value) {
        out.push(TAG_TUPLE);
        write_len(out, items.len())?;
        for &item in items {
            encode_value(item, depth + 1, spawn, out)?;
        }
        return Ok(());
    }
    if let Some(items) = value::as_set(value) {
        out.push(TAG_SET);
        write_len(out, items.len())?;
        for &item in items {
            encode_value(item, depth + 1, spawn, out)?;
        }
        return Ok(());
    }
    if let Some(pairs) = value::as_dict(value) {
        out.push(TAG_DICT);
        write_len(out, pairs.len())?;
        for &(key, item) in pairs {
            encode_value(key, depth + 1, spawn, out)?;
            encode_value(item, depth + 1, spawn, out)?;
        }
        return Ok(());
    }
    if let Some((start, stop, step)) = value::as_range(value) {
        out.push(TAG_RANGE);
        out.extend_from_slice(&start.to_le_bytes());
        out.extend_from_slice(&stop.to_le_bytes());
        out.extend_from_slice(&step.to_le_bytes());
        return Ok(());
    }
    if let Some(function) = value::as_function(value) {
        let wrapper = match &function.body {
            FunctionBody::Wrapper { entry, captures } => {
                if !captures.is_empty() {
                    return Err(PickleError::Unsupported("function"));
                }
                *entry
            }
            // Native module functions and builtins have no name entry in the
            // registry and no address that is stable across serialization
            // boundaries.
            FunctionBody::Native { .. } | FunctionBody::Builtin(_) => {
                return Err(PickleError::Unsupported("function"));
            }
            // Derived callables (getters, partials, identity decorators) have
            // no registry entry to reconstruct them from.
            FunctionBody::AttributeGetter { .. }
            | FunctionBody::ItemGetter { .. }
            | FunctionBody::Partial { .. }
            | FunctionBody::Identity => {
                return Err(PickleError::Unsupported("function"));
            }
        };
        let Ok(arity) = u32::try_from(function.arity) else {
            return Err(PickleError::Unsupported("function"));
        };
        let name = function.name.as_str();
        if name.len() > MAX_NAME {
            return Err(PickleError::Unsupported("function"));
        }
        if spawn {
            out.push(TAG_FUNCTION_NAME);
            write_len(out, name.len())?;
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&arity.to_le_bytes());
            return Ok(());
        }
        out.push(TAG_FUNCTION);
        out.extend_from_slice(&(wrapper as u64).to_le_bytes());
        out.extend_from_slice(&arity.to_le_bytes());
        write_len(out, name.len())?;
        out.extend_from_slice(name.as_bytes());
        return Ok(());
    }
    if spawn {
        if let Some(queue) = value::as_mp_queue(value) {
            out.push(TAG_MP_QUEUE);
            out.extend_from_slice(&queue.read_fd.to_le_bytes());
            out.extend_from_slice(&queue.write_fd.to_le_bytes());
            return Ok(());
        }
    }
    Err(PickleError::Unsupported(value::type_name(value)))
}

// -------------------------------------------------------------------------
// Decoding
// -------------------------------------------------------------------------

/// Bounds-checked little-endian reader.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], PickleError> {
        let end = self.pos.checked_add(len).ok_or(PickleError::Corrupt)?;
        if end > self.data.len() {
            return Err(PickleError::Corrupt);
        }
        let bytes = &self.data[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, PickleError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, PickleError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, PickleError> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn i64(&mut self) -> Result<i64, PickleError> {
        Ok(self.u64()? as i64)
    }

    fn f64(&mut self) -> Result<f64, PickleError> {
        Ok(f64::from_bits(self.u64()?))
    }

    fn string(&mut self) -> Result<String, PickleError> {
        let len = self.u32()? as usize;
        if len > self.remaining() {
            return Err(PickleError::Corrupt);
        }
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| PickleError::Corrupt)
    }
}

/// Decodes a payload produced by [`encode`].
pub(crate) fn decode(data: &[u8], spawn: bool) -> Result<i64, PickleError> {
    let mut reader = Reader::new(data);
    let magic = reader.take(MAGIC.len())?;
    if magic != MAGIC.as_slice() {
        return Err(PickleError::Corrupt);
    }
    if reader.u8()? != VERSION {
        return Err(PickleError::Corrupt);
    }
    let value = decode_value(&mut reader, 0, spawn)?;
    if reader.remaining() != 0 {
        return Err(PickleError::Corrupt);
    }
    Ok(value)
}

fn decode_value(reader: &mut Reader<'_>, depth: usize, spawn: bool) -> Result<i64, PickleError> {
    match reader.u8()? {
        TAG_NONE => Ok(NONE),
        TAG_FALSE => Ok(value::FALSE),
        TAG_TRUE => Ok(value::TRUE),
        TAG_INT => {
            let number = reader.i64()?;
            value::encode_int(number).ok_or(PickleError::Corrupt)
        }
        TAG_FLOAT => Ok(value::float_new(reader.f64()?)),
        TAG_STR => {
            let text = reader.string()?;
            Ok(value::str_alloc(text))
        }
        TAG_BYTES => {
            let len = reader.u32()? as usize;
            if len > reader.remaining() {
                return Err(PickleError::Corrupt);
            }
            Ok(value::bytes_from_slice(reader.take(len)?))
        }
        TAG_LIST => {
            if depth >= MAX_DEPTH {
                return Err(PickleError::TooDeep);
            }
            let count = reader.u32()?;
            let list = value::list_new(Vec::new());
            for _ in 0..count {
                let item = decode_value(reader, depth + 1, spawn)?;
                if let Some(mut items) = value::list_items_mut(list) {
                    items.push(item);
                    crate::heap::write_barrier_edge(list, item);
                }
            }
            Ok(list)
        }
        TAG_TUPLE => {
            let temp = read_into_list(reader, depth, spawn)?;
            let items = value::as_list(temp)
                .map(|items| items.to_vec())
                .unwrap_or_default();
            Ok(value::tuple_new(items))
        }
        TAG_SET => {
            if depth >= MAX_DEPTH {
                return Err(PickleError::TooDeep);
            }
            let count = reader.u32()?;
            let set = value::set_new(Vec::new());
            for _ in 0..count {
                let item = decode_value(reader, depth + 1, spawn)?;
                if let Some(mut items) = value::set_items_mut(set) {
                    items.push(item);
                    crate::heap::write_barrier_edge(set, item);
                }
            }
            Ok(set)
        }
        TAG_DICT => {
            if depth >= MAX_DEPTH {
                return Err(PickleError::TooDeep);
            }
            let count = reader.u32()?;
            let dict = value::dict_new(Vec::new());
            for _ in 0..count {
                let key = decode_value(reader, depth + 1, spawn)?;
                let item = decode_value(reader, depth + 1, spawn)?;
                if let Some(mut pairs) = value::dict_pairs_mut(dict) {
                    crate::container::dict_insert_barrier(dict, &mut pairs, key, item);
                }
            }
            Ok(dict)
        }
        TAG_RANGE => {
            let start = reader.i64()?;
            let stop = reader.i64()?;
            let step = reader.i64()?;
            Ok(value::range_new(start, stop, step))
        }
        TAG_FUNCTION => {
            if spawn {
                // Wrapper addresses from another process are meaningless.
                return Err(PickleError::Corrupt);
            }
            let wrapper = reader.u64()? as usize;
            let arity = reader.u32()? as usize;
            let name = reader.string()?;
            if wrapper == 0 {
                return Err(PickleError::Corrupt);
            }
            Ok(value::function_new(wrapper, arity, name))
        }
        TAG_FUNCTION_NAME => {
            if !spawn {
                return Err(PickleError::Corrupt);
            }
            let name = reader.string()?;
            let arity = reader.u32()? as usize;
            let Some((wrapper, registered)) = crate::spawn::lookup(&name) else {
                return Err(PickleError::Unsupported("function"));
            };
            if wrapper == 0 || registered != arity {
                return Err(PickleError::Corrupt);
            }
            Ok(value::function_new(wrapper, arity, name))
        }
        TAG_MP_QUEUE => {
            if !spawn {
                return Err(PickleError::Corrupt);
            }
            let read_fd = i32::try_from(reader.u32()?).map_err(|_| PickleError::Corrupt)?;
            let write_fd = i32::try_from(reader.u32()?).map_err(|_| PickleError::Corrupt)?;
            if read_fd < 0 || write_fd < 0 {
                return Err(PickleError::Corrupt);
            }
            Ok(value::mp_queue_from_fds(read_fd, write_fd))
        }
        _ => Err(PickleError::Corrupt),
    }
}

fn read_into_list(reader: &mut Reader<'_>, depth: usize, spawn: bool) -> Result<i64, PickleError> {
    if depth >= MAX_DEPTH {
        return Err(PickleError::TooDeep);
    }
    let count = reader.u32()?;
    let list = value::list_new(Vec::new());
    for _ in 0..count {
        let item = decode_value(reader, depth + 1, spawn)?;
        if let Some(mut items) = value::list_items_mut(list) {
            items.push(item);
            crate::heap::write_barrier_edge(list, item);
        }
    }
    Ok(list)
}

// -------------------------------------------------------------------------
// Hex transport (`pickle.dumps` returns a hex string)
// -------------------------------------------------------------------------

/// Lowercase hex encoding.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Decodes a hex string; `None` for an odd length or a non-hex character.
pub(crate) fn from_hex(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let high = hex_digit(pair[0])?;
        let low = hex_digit(pair[1])?;
        out.push((high << 4) | low);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(value: i64) {
        let bytes = encode(value, false).expect("encodable");
        let decoded = decode(&bytes, false).expect("valid payload");
        assert_eq!(crate::value::repr(decoded), crate::value::repr(value));
    }

    #[test]
    fn scalars_and_containers_roundtrip() {
        crate::exc::init();
        roundtrip(NONE);
        roundtrip(value::TRUE);
        roundtrip(value::encode_int(-42).unwrap());
        roundtrip(value::float_new(1.5));
        roundtrip(value::str_alloc("héllo".to_string()));
        roundtrip(value::range_new(1, 9, 2));
        let nested = value::dict_new(vec![
            (
                value::str_alloc("k".into()),
                value::list_new(vec![value::TRUE, value::encode_int(7).unwrap()]),
            ),
            (value::encode_int(1).unwrap(), value::tuple_new(Vec::new())),
        ]);
        roundtrip(nested);
    }

    #[test]
    fn malformed_payloads_are_rejected() {
        assert_eq!(decode(b"nope", false), Err(PickleError::Corrupt));
        assert_eq!(decode(b"PYX1\x01", false), Err(PickleError::Corrupt));
        assert_eq!(
            decode(b"PYX1\x01\x05\xff\xff\xff\xff", false),
            Err(PickleError::Corrupt)
        );
        assert_eq!(
            decode(b"PYX1\x01\x03\x01", false),
            Err(PickleError::Corrupt)
        );
    }

    #[test]
    fn spawn_function_is_encoded_by_name() {
        let _guard = crate::spawn::TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::spawn::reset();
        let function = value::function_new(0x1234, 2, "worker".to_string());
        let bytes = encode(function, true).expect("encodable");
        // Address-based decoders must not accept a name-only function.
        assert_eq!(decode(&bytes, false), Err(PickleError::Corrupt));

        crate::spawn::register("worker", 0x5678, 2);
        let decoded = decode(&bytes, true).expect("registered function");
        let decoded = value::as_function(decoded).expect("function value");
        match &decoded.body {
            crate::value::FunctionBody::Wrapper { entry, .. } => assert_eq!(*entry, 0x5678),
            crate::value::FunctionBody::Native { .. } | crate::value::FunctionBody::Builtin(_) => {
                panic!("expected a wrapper")
            }
            crate::value::FunctionBody::AttributeGetter { .. }
            | crate::value::FunctionBody::ItemGetter { .. }
            | crate::value::FunctionBody::Partial { .. }
            | crate::value::FunctionBody::Identity => panic!("expected a wrapper"),
        }
        assert_eq!(decoded.arity, 2);
        assert_eq!(decoded.name, "worker");
        crate::spawn::reset();
    }

    #[test]
    fn spawn_unregistered_function_is_rejected() {
        let _guard = crate::spawn::TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::spawn::reset();
        let function = value::function_new(0x1234, 0, "missing".to_string());
        let bytes = encode(function, true).expect("encodable");
        assert_eq!(
            decode(&bytes, true),
            Err(PickleError::Unsupported("function"))
        );
    }

    #[test]
    fn spawn_mode_carries_queue_descriptors() {
        let queue = crate::process::new_queue(5).expect("socketpair");
        let original = value::as_mp_queue(queue).expect("queue value");
        let bytes = encode(queue, true).expect("encodable");
        let decoded = decode(&bytes, true).expect("valid payload");
        let decoded = value::as_mp_queue(decoded).expect("queue value");
        assert_eq!(decoded.read_fd, original.read_fd);
        assert_eq!(decoded.write_fd, original.write_fd);
        assert_eq!(decoded.maxsize, 0);
        // Normal pickle mode still rejects native queue objects.
        assert_eq!(encode(queue, false), Err(PickleError::Unsupported("Queue")));
    }

    #[test]
    fn hex_transport() {
        let bytes = b"\x00\x0f\xa5\xff";
        let hex = to_hex(bytes);
        assert_eq!(hex, "000fa5ff");
        assert_eq!(from_hex(&hex).as_deref(), Some(bytes.as_slice()));
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
    }
}
