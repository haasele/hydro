//! `_hydro_sidecar`: foreign-runtime sidecar transport (protocol v1).
//!
//! A sidecar is a child process that speaks a small length-prefixed binary
//! protocol over its standard input and output. The parent owns a capsule
//! handle backing a [`SidecarHandle`]; the child's stderr is inherited, so the
//! worker can log without corrupting the frame stream, and the worker is
//! expected to exit on EOF at a frame boundary.
//!
//! Every exchange prepares the request bytes as plain Rust data *before* the
//! thread parks for the collector, then parks once for the blocking write and
//! read through [`heap::blocking_enter`]/[`heap::blocking_leave`]. Replies are
//! decoded into Hydro values only after the thread unparks, and every
//! intermediate container is rooted immediately so strict collection cannot
//! reclaim a partially built response. No Hydro object is alive while the
//! thread is parked.
//!
//! Wire format (all integers little-endian):
//!
//! ```text
//! frame   := u32 payload_len | payload
//! payload := u8 version | u32 id | u8 kind | body
//! request := u32 method_len | method_utf8 | u32 argc | value*
//! ok      := value
//! error   := u8 exc_tag | u32 msg_len | msg_utf8
//! ```
//!
//! Value tags are `0` `None`, `1` `False`, `2` `True`, `3` `int` (`i64`),
//! `4` `float` (`f64` bits), `5` `str` (UTF-8), `6` `list`, `7` `dict`
//! (`(key, value)` pairs whose key tag is `3` or `5`), `8` `bytes` (a
//! `bytearray` projects to `bytes`) and `9` `tuple`. Any other tag is a
//! protocol violation.

use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use num_bigint::BigInt;

use crate::exc::{
    self, ATTRIBUTE_ERROR, BROKEN_PIPE_ERROR, CONNECTION_RESET_ERROR, OS_ERROR, OVERFLOW_ERROR,
    RUNTIME_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::value::{self, NONE};

/// Wire protocol version implemented by this module.
pub(crate) const PROTOCOL_VERSION: u8 = 1;

/// Maximum payload bytes in one frame (excluding the 4-byte length prefix).
pub(crate) const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Maximum container nesting accepted in either direction.
pub(crate) const MAX_DEPTH: usize = 64;

/// Maximum method-name bytes accepted by `call`.
pub(crate) const MAX_METHOD: usize = 256;

/// Maximum bytes accepted in a remote error message.
const MAX_ERROR: usize = 64 * 1024;

/// Highest exception tag the runtime knows (`TimeoutError`, ARCHITECTURE §3.2).
const MAX_EXCEPTION_TAG: u32 = 67;

const KIND_REQUEST: u8 = 1;
const KIND_OK: u8 = 2;
const KIND_ERROR: u8 = 3;

const TAG_NONE: u8 = 0;
const TAG_FALSE: u8 = 1;
const TAG_TRUE: u8 = 2;
const TAG_INT: u8 = 3;
const TAG_FLOAT: u8 = 4;
const TAG_STR: u8 = 5;
const TAG_LIST: u8 = 6;
const TAG_DICT: u8 = 7;
const TAG_BYTES: u8 = 8;
const TAG_TUPLE: u8 = 9;

/// Capsule name that tags a sidecar handle.
const CAPSULE_NAME: &str = "hydro.sidecar";

/// Grace period for the worker to exit (or die) on each termination step.
const TERMINATE_TIMEOUT: Duration = Duration::from_millis(500);

/// Poll interval while waiting for a dying worker.
const TERMINATE_POLL: Duration = Duration::from_millis(10);

/// Method name used for the spawn handshake.
const HELLO_METHOD: &str = "__hello__";

/// A capsule-backed sidecar worker.
struct SidecarHandle {
    /// Program re-launched after a crash or an explicit `restart`.
    program: String,
    /// Arguments passed to the program; the program is not run through a shell.
    arguments: Vec<String>,
    /// PID that created the handle; a forked child must not touch the pipes.
    owner_pid: u32,
    /// The worker pipe pair and the per-handle request counter.
    slot: Mutex<WorkerSlot>,
}

/// Mutable worker state, protected by [`SidecarHandle::slot`].
struct WorkerSlot {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    /// Next request id; `1` initially, because `0` is reserved for the handshake.
    next_id: u32,
    /// Set by `close`; a closed handle never respawns.
    closed: bool,
}

impl WorkerSlot {
    fn new() -> Self {
        WorkerSlot {
            child: None,
            stdin: None,
            stdout: None,
            next_id: 1,
            closed: false,
        }
    }
}

/// A failure with the exception tag to raise and the exact message.
#[derive(Debug)]
struct Failure {
    tag: u32,
    message: String,
}

impl Failure {
    fn raise(self) -> i64 {
        exc::raise_with_message(self.tag, &self.message)
    }
}

fn failure(tag: u32, message: String) -> Failure {
    Failure { tag, message }
}

fn type_failure(message: String) -> Failure {
    failure(TYPE_ERROR, message)
}

fn value_failure(message: &str) -> Failure {
    failure(VALUE_ERROR, message.to_string())
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn runtime_error(message: &str) -> i64 {
    exc::raise_with_message(RUNTIME_ERROR, message)
}

/// A protocol violation; always reported as `RuntimeError`.
fn protocol_error(reason: &str) -> String {
    format!("sidecar: protocol error: {reason}")
}

fn version_mismatch(found: u8) -> String {
    format!("sidecar: worker speaks protocol version {found}, expected {PROTOCOL_VERSION}")
}

fn frame_too_large() -> Failure {
    value_failure(&format!(
        "sidecar: request exceeds the {MAX_FRAME}-byte frame limit"
    ))
}

// -------------------------------------------------------------------------
// Capsule handle
// -------------------------------------------------------------------------

/// Borrows the handle behind a validated capsule.
fn handle_argument(argument: i64) -> Option<&'static SidecarHandle> {
    match value::as_capsule(argument) {
        Some((pointer, Some(name))) if name == CAPSULE_NAME && pointer != 0 => {
            // SAFETY: only `spawn` creates this capsule, and it stores the
            // pointer of an intentionally leaked (never freed) `SidecarHandle`.
            Some(unsafe { &*(pointer as *const SidecarHandle) })
        }
        _ => None,
    }
}

/// Reads the `handle` argument of a function, raising `TypeError` otherwise.
fn require_handle(args: &[i64], function: &str) -> Result<&'static SidecarHandle, i64> {
    let Some(argument) = args.first() else {
        return Err(type_error(&format!(
            "{function}() missing required argument: 'handle'"
        )));
    };
    match handle_argument(*argument) {
        Some(handle) => Ok(handle),
        None => Err(type_error(&format!(
            "{function}() argument 'handle' must be a sidecar handle capsule"
        ))),
    }
}

/// Rejects a handle inherited across `fork`. `close` handles this separately
/// because it must not signal the parent's worker.
fn check_process(handle: &SidecarHandle) -> Option<i64> {
    let current = std::process::id();
    if current == handle.owner_pid {
        None
    } else {
        Some(runtime_error(&format!(
            "sidecar: handle was created in process {} and cannot be used in process {}",
            handle.owner_pid, current
        )))
    }
}

/// Locks the slot, parking for the collector if another thread holds it.
///
/// The mutex serialises frames on one pipe, so callers may hold it across the
/// parked I/O; it is not one of the runtime locks `blocking_enter` forbids.
fn lock_slot(handle: &SidecarHandle) -> MutexGuard<'_, WorkerSlot> {
    if let Ok(guard) = handle.slot.try_lock() {
        return guard;
    }
    heap::blocking_enter();
    let guard = handle
        .slot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    heap::blocking_leave();
    guard
}

// -------------------------------------------------------------------------
// Argument normalisation
// -------------------------------------------------------------------------

/// Copies a `str` or `bytes` value into an owned `String` (lossy for bytes).
fn text_of(argument: i64) -> Option<String> {
    if let Some(text) = value::as_str(argument) {
        return Some(text.to_string());
    }
    value::as_bytes(argument).map(|bytes| String::from_utf8_lossy(bytes).into_owned())
}

/// Validates the `program` operand.
fn program_text(argument: i64) -> Result<String, i64> {
    match text_of(argument) {
        Some(program) if !program.is_empty() => Ok(program),
        Some(_) => Err(value_error("sidecar: program must not be empty")),
        None => Err(type_error(&format!(
            "spawn() argument 'program' must be str, not '{}'",
            value::type_name(argument)
        ))),
    }
}

/// Validates the optional `args` operand: `None`, a single string, or a
/// sequence of strings.
fn arguments_list(argument: i64) -> Result<Vec<String>, i64> {
    if argument == NONE {
        return Ok(Vec::new());
    }
    if let Some(text) = text_of(argument) {
        return Ok(vec![text]);
    }
    let Some(items) = value::as_list(argument).or_else(|| value::as_tuple(argument)) else {
        return Err(type_error(&format!(
            "spawn() argument 'args' must be a str or a sequence of str, not '{}'",
            value::type_name(argument)
        )));
    };
    let mut arguments = Vec::with_capacity(items.len());
    for &item in items {
        let Some(text) = text_of(item) else {
            return Err(type_error(&format!(
                "spawn() argument 'args' items must be str, not '{}'",
                value::type_name(item)
            )));
        };
        arguments.push(text);
    }
    Ok(arguments)
}

// -------------------------------------------------------------------------
// Payload codec: encoding
// -------------------------------------------------------------------------

/// Encodes one request frame (length prefix included).
fn encode_request(id: u32, method: &str, values: &[i64]) -> Result<Vec<u8>, Failure> {
    if method.is_empty() {
        return Err(value_failure("sidecar: method name must not be empty"));
    }
    if method.len() > MAX_METHOD {
        return Err(value_failure(&format!(
            "sidecar: method name is longer than {MAX_METHOD} bytes"
        )));
    }
    let argc = u32::try_from(values.len()).map_err(|_| frame_too_large())?;
    let method_length = u32::try_from(method.len()).map_err(|_| frame_too_large())?;

    let mut payload = Vec::new();
    payload.push(PROTOCOL_VERSION);
    payload.extend_from_slice(&id.to_le_bytes());
    payload.push(KIND_REQUEST);
    payload.extend_from_slice(&method_length.to_le_bytes());
    payload.extend_from_slice(method.as_bytes());
    payload.extend_from_slice(&argc.to_le_bytes());

    let mut path = Vec::new();
    for &item in values {
        encode_value(&mut payload, item, &mut path, 0)?;
        if payload.len() > MAX_FRAME {
            return Err(frame_too_large());
        }
    }
    if payload.len() > MAX_FRAME {
        return Err(frame_too_large());
    }
    Ok(prefix_frame(payload))
}

/// Prefixes a payload with its little-endian 32-bit length.
fn prefix_frame(payload: Vec<u8>) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 4);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    frame
}

/// Encodes one value.
///
/// `path` holds the tagged ids of the containers currently being encoded, so a
/// value that reaches itself is reported instead of looping forever; `depth`
/// bounds container nesting.
fn encode_value(
    out: &mut Vec<u8>,
    item: i64,
    path: &mut Vec<i64>,
    depth: usize,
) -> Result<(), Failure> {
    if depth > MAX_DEPTH {
        return Err(value_failure(&format!(
            "sidecar: container nesting exceeds {MAX_DEPTH}"
        )));
    }
    if item == value::NONE {
        out.push(TAG_NONE);
        return Ok(());
    }
    if item == value::TRUE {
        out.push(TAG_TRUE);
        return Ok(());
    }
    if item == value::FALSE {
        out.push(TAG_FALSE);
        return Ok(());
    }
    if let Some(number) = value::as_integer(item) {
        out.push(TAG_INT);
        out.extend_from_slice(&number.to_le_bytes());
        return Ok(());
    }
    if value::is_bigint(item) {
        return Err(failure(
            OVERFLOW_ERROR,
            "sidecar: integer does not fit in 64 bits".to_string(),
        ));
    }
    if let Some(number) = value::as_float(item) {
        out.push(TAG_FLOAT);
        out.extend_from_slice(&number.to_bits().to_le_bytes());
        return Ok(());
    }
    if let Some(text) = value::as_str(item) {
        if text.len() > MAX_FRAME {
            return Err(frame_too_large());
        }
        out.push(TAG_STR);
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
        return Ok(());
    }
    if let Some(bytes) = value::as_bytes(item) {
        if bytes.len() > MAX_FRAME {
            return Err(frame_too_large());
        }
        out.push(TAG_BYTES);
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(bytes);
        return Ok(());
    }
    if let Some(items) = value::as_list(item) {
        return encode_sequence(out, TAG_LIST, item, items, path, depth);
    }
    if let Some(items) = value::as_tuple(item) {
        return encode_sequence(out, TAG_TUPLE, item, items, path, depth);
    }
    if let Some(pairs) = value::as_dict(item) {
        return encode_mapping(out, item, pairs, path, depth);
    }
    Err(type_failure(format!(
        "sidecar: cannot encode value of type '{}'",
        value::type_name(item)
    )))
}

/// Encodes a `list` or `tuple` payload.
fn encode_sequence(
    out: &mut Vec<u8>,
    tag: u8,
    container: i64,
    items: &[i64],
    path: &mut Vec<i64>,
    depth: usize,
) -> Result<(), Failure> {
    if path.contains(&container) {
        return Err(value_failure("sidecar: cannot encode a cyclic container"));
    }
    let count = u32::try_from(items.len()).map_err(|_| frame_too_large())?;
    path.push(container);
    out.push(tag);
    out.extend_from_slice(&count.to_le_bytes());
    for &item in items {
        encode_value(out, item, path, depth + 1)?;
        if out.len() > MAX_FRAME {
            return Err(frame_too_large());
        }
    }
    path.pop();
    Ok(())
}

/// Encodes a `dict` payload.
fn encode_mapping(
    out: &mut Vec<u8>,
    container: i64,
    pairs: &[(i64, i64)],
    path: &mut Vec<i64>,
    depth: usize,
) -> Result<(), Failure> {
    if path.contains(&container) {
        return Err(value_failure("sidecar: cannot encode a cyclic container"));
    }
    let count = u32::try_from(pairs.len()).map_err(|_| frame_too_large())?;
    path.push(container);
    out.push(TAG_DICT);
    out.extend_from_slice(&count.to_le_bytes());
    for &(key, item) in pairs {
        encode_dict_key(out, key)?;
        encode_value(out, item, path, depth + 1)?;
        if out.len() > MAX_FRAME {
            return Err(frame_too_large());
        }
    }
    path.pop();
    Ok(())
}

/// Encodes one dict key, restricted to `str` and non-bool `int`.
fn encode_dict_key(out: &mut Vec<u8>, key: i64) -> Result<(), Failure> {
    if key == value::TRUE || key == value::FALSE {
        return Err(type_failure(
            "sidecar: dict keys must be str or int, not 'bool'".to_string(),
        ));
    }
    if let Some(text) = value::as_str(key) {
        if text.len() > MAX_FRAME {
            return Err(frame_too_large());
        }
        out.push(TAG_STR);
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
        return Ok(());
    }
    if let Some(number) = value::as_integer(key) {
        out.push(TAG_INT);
        out.extend_from_slice(&number.to_le_bytes());
        return Ok(());
    }
    if value::is_bigint(key) {
        return Err(failure(
            OVERFLOW_ERROR,
            "sidecar: integer does not fit in 64 bits".to_string(),
        ));
    }
    Err(type_failure(format!(
        "sidecar: dict keys must be str or int, not '{}'",
        value::type_name(key)
    )))
}

// -------------------------------------------------------------------------
// Payload codec: decoding
// -------------------------------------------------------------------------

/// Bounds-checked cursor over a payload.
struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, position: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.position)
    }

    /// Takes `count` bytes, or reports a truncated payload.
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        if count > self.remaining() {
            return Err(protocol_error("truncated payload"));
        }
        let start = self.position;
        self.position += count;
        Ok(&self.bytes[start..self.position])
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn i64(&mut self) -> Result<i64, String> {
        let bytes = self.take(8)?;
        Ok(i64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    /// Reads a `u32`-length blob after checking it fits the remaining bytes.
    fn blob(&mut self) -> Result<&'a [u8], String> {
        let length = self.u32()? as usize;
        self.take(length)
    }
}

/// Roots a freshly allocated value immediately, so a strict collection between
/// two decode steps cannot reclaim a partially built result.
fn rooted(item: i64) -> i64 {
    heap::root_values(&[item]);
    item
}

/// A response frame decoded into either a value or a remote exception.
#[derive(Debug)]
enum Response {
    Value(i64),
    Remote { tag: u32, message: String },
}

/// Validates a frame payload length.
fn validate_frame_length(length: usize) -> Result<(), String> {
    if !(6..=MAX_FRAME).contains(&length) {
        return Err(protocol_error(&format!("invalid frame length {length}")));
    }
    Ok(())
}

/// Decodes one response payload, validating its version, id and kind.
fn decode_response(payload: &[u8], expected_id: u32) -> Result<Response, String> {
    if payload.len() < 6 {
        return Err(protocol_error("truncated payload"));
    }
    let mut reader = Reader::new(payload);
    let version = reader.u8()?;
    // The version is checked on every frame, not just the handshake, so a
    // mid-session downgrade cannot be ignored.
    if version != PROTOCOL_VERSION {
        return Err(version_mismatch(version));
    }
    let id = reader.u32()?;
    if id != expected_id {
        return Err(protocol_error(&format!(
            "response id {id} does not match request id {expected_id}"
        )));
    }
    let kind = reader.u8()?;
    match kind {
        KIND_OK => {
            let root_start = heap::root_values(&[]);
            let decoded = decode_value(&mut reader, 0);
            heap::release_roots(root_start);
            let item = decoded?;
            if reader.remaining() != 0 {
                return Err(protocol_error("trailing bytes after value"));
            }
            Ok(Response::Value(item))
        }
        KIND_ERROR => {
            let tag = u32::from(reader.u8()?);
            let length = reader.u32()? as usize;
            if length > MAX_ERROR {
                return Err(protocol_error(&format!(
                    "error message exceeds {MAX_ERROR} bytes"
                )));
            }
            let bytes = reader.take(length)?;
            let message = std::str::from_utf8(bytes)
                .map_err(|_| protocol_error("invalid UTF-8 in error message"))?
                .to_string();
            if reader.remaining() != 0 {
                return Err(protocol_error("trailing bytes after value"));
            }
            Ok(Response::Remote { tag, message })
        }
        other => Err(protocol_error(&format!("unexpected response kind {other}"))),
    }
}

/// Decodes one value, rooting every intermediate allocation.
fn decode_value(reader: &mut Reader<'_>, depth: usize) -> Result<i64, String> {
    if depth > MAX_DEPTH {
        return Err(protocol_error(&format!(
            "container nesting exceeds {MAX_DEPTH}"
        )));
    }
    let tag = reader.u8()?;
    match tag {
        TAG_NONE => Ok(value::NONE),
        TAG_FALSE => Ok(value::FALSE),
        TAG_TRUE => Ok(value::TRUE),
        TAG_INT => {
            let number = reader.i64()?;
            Ok(rooted(value::bigint_new(BigInt::from(number))))
        }
        TAG_FLOAT => {
            let bits = reader.u64()?;
            Ok(rooted(value::float_new(f64::from_bits(bits))))
        }
        TAG_STR => {
            let bytes = reader.blob()?;
            let text = std::str::from_utf8(bytes)
                .map_err(|_| protocol_error("invalid UTF-8 in str payload"))?;
            Ok(rooted(value::str_alloc(text.to_string())))
        }
        TAG_BYTES => {
            let bytes = reader.blob()?;
            Ok(rooted(value::bytes_new(bytes.to_vec())))
        }
        TAG_LIST | TAG_TUPLE => {
            let count = reader.u32()? as usize;
            // Every element carries at least its one-byte tag; reject an
            // oversized count before reserving.
            if count > reader.remaining() {
                return Err(protocol_error("truncated payload"));
            }
            let mut items = Vec::with_capacity(count.min(reader.remaining()));
            for _ in 0..count {
                items.push(decode_value(reader, depth + 1)?);
            }
            let container = if tag == TAG_LIST {
                value::list_new(items)
            } else {
                value::tuple_new(items)
            };
            Ok(rooted(container))
        }
        TAG_DICT => {
            let count = reader.u32()? as usize;
            // Each pair carries a key tag and a value tag at minimum.
            if count > reader.remaining() / 2 {
                return Err(protocol_error("truncated payload"));
            }
            let mut pairs = Vec::with_capacity(count.min(reader.remaining() / 2));
            for _ in 0..count {
                let key = decode_dict_key(reader)?;
                let item = decode_value(reader, depth + 1)?;
                pairs.push((key, item));
            }
            Ok(rooted(value::dict_new(pairs)))
        }
        other => Err(protocol_error(&format!("unknown value tag {other}"))),
    }
}

/// Decodes a dict key, restricted to `int` and `str`.
fn decode_dict_key(reader: &mut Reader<'_>) -> Result<i64, String> {
    match reader.u8()? {
        TAG_INT => {
            let number = reader.i64()?;
            Ok(rooted(value::bigint_new(BigInt::from(number))))
        }
        TAG_STR => {
            let bytes = reader.blob()?;
            let text = std::str::from_utf8(bytes)
                .map_err(|_| protocol_error("invalid UTF-8 in str payload"))?;
            Ok(rooted(value::str_alloc(text.to_string())))
        }
        other => Err(protocol_error(&format!(
            "dict key tag must be int or str, got {other}"
        ))),
    }
}

// -------------------------------------------------------------------------
// Process control
// -------------------------------------------------------------------------

/// Parks for the collector while writing the whole frame and reading the reply.
fn exchange(slot: &mut WorkerSlot, frame: &[u8]) -> Result<Vec<u8>, Failure> {
    write_frame(slot, frame)?;
    read_frame(slot)
}

/// Writes one frame; the caller already built the complete buffer.
fn write_frame(slot: &mut WorkerSlot, frame: &[u8]) -> Result<(), Failure> {
    let Some(stdin) = slot.stdin.as_mut() else {
        return Err(failure(
            BROKEN_PIPE_ERROR,
            "sidecar: worker closed its input".to_string(),
        ));
    };
    heap::blocking_enter();
    let result = stdin.write_all(frame);
    heap::blocking_leave();
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Err(failure(
            BROKEN_PIPE_ERROR,
            "sidecar: worker closed its input".to_string(),
        )),
        Err(error) => Err(failure(
            OS_ERROR,
            format!("sidecar: worker write failed: {error}"),
        )),
    }
}

/// Reads one frame. The length prefix is read while parked; the payload buffer
/// is allocated after unparking so the parked region performs no allocation.
fn read_frame(slot: &mut WorkerSlot) -> Result<Vec<u8>, Failure> {
    let mut header = [0u8; 4];
    read_exact_parked(slot, &mut header)?;
    let length = u32::from_le_bytes(header) as usize;
    validate_frame_length(length).map_err(|message| failure(RUNTIME_ERROR, message))?;
    let mut payload = vec![0u8; length];
    read_exact_parked(slot, &mut payload)?;
    Ok(payload)
}

/// One parked `read_exact`; classifies a transport failure.
fn read_exact_parked(slot: &mut WorkerSlot, buffer: &mut [u8]) -> Result<(), Failure> {
    heap::blocking_enter();
    let result = match slot.stdout.as_mut() {
        Some(stdout) => stdout.read_exact(buffer),
        None => Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "stdout is closed",
        )),
    };
    heap::blocking_leave();
    result.map_err(|error| read_failure(slot, error))
}

/// Classifies a failed read: a vanished worker is a connection reset, anything
/// else is an I/O error.
fn read_failure(slot: &mut WorkerSlot, error: io::Error) -> Failure {
    let connection_lost = matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe
    );
    if !connection_lost {
        return failure(OS_ERROR, format!("sidecar: worker read failed: {error}"));
    }
    let exited = match slot.child.as_mut().map(|child| child.try_wait()) {
        Some(Ok(Some(_))) => true,
        Some(Ok(None)) => false,
        Some(Err(_)) => true,
        None => true,
    };
    if exited {
        failure(
            CONNECTION_RESET_ERROR,
            "sidecar: worker closed its stdout unexpectedly".to_string(),
        )
    } else {
        failure(
            OS_ERROR,
            "sidecar: worker closed its stdout unexpectedly".to_string(),
        )
    }
}

/// Kills and reaps the worker; used on handshake and protocol failures.
fn kill_and_reap(child: &mut Child) {
    heap::blocking_enter();
    let _ = child.kill();
    let _ = child.wait();
    heap::blocking_leave();
}

/// Clears the slot without signalling the child (used on failure paths and
/// when the handle is inherited across `fork`).
fn clear_slot(slot: &mut WorkerSlot) {
    slot.stdin = None;
    slot.stdout = None;
    slot.child = None;
}

/// Kills and reaps the worker, then leaves the slot reusable.
fn reset_worker(slot: &mut WorkerSlot) {
    slot.stdin = None;
    slot.stdout = None;
    if let Some(mut child) = slot.child.take() {
        kill_and_reap(&mut child);
    }
}

/// Asks the worker to exit: close its stdin, wait 500 ms, send `SIGTERM`, wait
/// 500 ms, then `SIGKILL`. The whole wait is one parked region.
fn stop_worker(slot: &mut WorkerSlot) {
    // Closing stdin lets a conforming worker exit on EOF.
    slot.stdin = None;
    slot.stdout = None;
    let Some(mut child) = slot.child.take() else {
        return;
    };
    heap::blocking_enter();
    if !wait_for_exit(&mut child, TERMINATE_TIMEOUT) {
        let pid = child.id() as libc::pid_t;
        // SAFETY: `child` has not been reaped, so the kernel cannot have
        // recycled the pid; the signal can only reach this worker.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        if !wait_for_exit(&mut child, TERMINATE_TIMEOUT) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    heap::blocking_leave();
}

/// Polls `try_wait` until the child is reaped or the timeout expires.
fn wait_for_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            // The child cannot be inspected any more; treat it as gone.
            Err(_) => return true,
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(TERMINATE_POLL);
    }
}

// -------------------------------------------------------------------------
// Worker lifecycle
// -------------------------------------------------------------------------

/// Launches the program and performs the version handshake.
fn spawn_worker(handle: &SidecarHandle, slot: &mut WorkerSlot) -> Result<(), Failure> {
    let mut command = Command::new(&handle.program);
    command
        .args(&handle.arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Logs go to the terminal, never into the frame stream.
        .stderr(Stdio::inherit());

    heap::blocking_enter();
    let spawned = command.spawn();
    heap::blocking_leave();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => return Err(spawn_failure(&handle.program, &error)),
    };

    let pipes = (child.stdin.take(), child.stdout.take());
    let (Some(stdin), Some(stdout)) = pipes else {
        kill_and_reap(&mut child);
        return Err(failure(
            OS_ERROR,
            format!(
                "sidecar: spawn failed for '{}': pipe setup failed",
                handle.program
            ),
        ));
    };
    slot.child = Some(child);
    slot.stdin = Some(stdin);
    slot.stdout = Some(stdout);

    match handshake(slot) {
        Ok(()) => Ok(()),
        Err(failure) => {
            reset_worker(slot);
            Err(failure)
        }
    }
}

/// Formats a process-launch failure as `OSError`.
fn spawn_failure(program: &str, error: &io::Error) -> Failure {
    let detail = match error.raw_os_error() {
        Some(code) => format!("[Errno {code}] {error}"),
        None => error.to_string(),
    };
    failure(
        OS_ERROR,
        format!("sidecar: spawn failed for '{program}': {detail}"),
    )
}

/// Sends `__hello__` with the protocol version and validates the reply.
fn handshake(slot: &mut WorkerSlot) -> Result<(), Failure> {
    let version = value::encode_int(i64::from(PROTOCOL_VERSION)).unwrap_or(NONE);
    let frame = encode_request(0, HELLO_METHOD, &[version])?;
    let payload = exchange(slot, &frame).map_err(|error| {
        failure(
            RUNTIME_ERROR,
            format!("sidecar: handshake failed: {}", error.message),
        )
    })?;
    match decode_response(&payload, 0) {
        Ok(Response::Value(result)) => {
            if value::as_str(result).is_some() {
                Ok(())
            } else {
                Err(failure(
                    RUNTIME_ERROR,
                    "sidecar: handshake result must be str".to_string(),
                ))
            }
        }
        Ok(Response::Remote { tag, message }) => Err(failure(
            RUNTIME_ERROR,
            format!(
                "sidecar: handshake failed: worker raised {}: {message}",
                exc::tag_name(tag)
            ),
        )),
        Err(message) => Err(failure(RUNTIME_ERROR, message)),
    }
}

/// Ensures a live worker, respawning only while no request byte was written.
fn ensure_worker(handle: &SidecarHandle, slot: &mut WorkerSlot) -> Result<(), Failure> {
    if slot.closed {
        return Err(failure(
            RUNTIME_ERROR,
            "sidecar: handle is closed".to_string(),
        ));
    }
    let status = slot.child.as_mut().map(|child| child.try_wait());
    match status {
        Some(Ok(None)) => return Ok(()),
        Some(Ok(Some(_))) => clear_slot(slot),
        Some(Err(_)) => reset_worker(slot),
        None => {
            slot.stdin = None;
            slot.stdout = None;
        }
    }
    spawn_worker(handle, slot)
}

/// Advances the request id, keeping `0` reserved for the handshake.
fn next_request_id(current: u32) -> u32 {
    let next = current.wrapping_add(1);
    if next == 0 {
        1
    } else {
        next
    }
}

/// Maps a remote error tag to the corresponding exception.
fn raise_remote(tag: u32, message: &str) -> i64 {
    if tag > MAX_EXCEPTION_TAG {
        return runtime_error(&format!(
            "sidecar: worker reported unknown exception tag {tag}: {message}"
        ));
    }
    exc::raise_with_message(tag, message)
}

// -------------------------------------------------------------------------
// Module functions
// -------------------------------------------------------------------------

/// `_hydro_sidecar.spawn(program, args=None) -> capsule`.
fn spawn(args: &[i64]) -> i64 {
    let Some(program_argument) = args.first() else {
        return type_error("spawn() missing required argument: 'program'");
    };
    let program = match program_text(*program_argument) {
        Ok(program) => program,
        Err(error) => return error,
    };
    let arguments = match arguments_list(args.get(1).copied().unwrap_or(NONE)) {
        Ok(arguments) => arguments,
        Err(error) => return error,
    };

    let handle = Box::new(SidecarHandle {
        program,
        arguments,
        owner_pid: std::process::id(),
        slot: Mutex::new(WorkerSlot::new()),
    });
    let started = {
        let mut slot = handle
            .slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        spawn_worker(&handle, &mut slot)
    };
    if let Err(error) = started {
        return error.raise();
    }

    // Capsules have no destructor: the leaked handle lives for the process
    // lifetime unless the program calls `close`.
    let pointer = Box::into_raw(handle) as usize;
    value::capsule_new(pointer, Some(CAPSULE_NAME.to_string()))
}

/// `_hydro_sidecar.call(handle, method, *args) -> Any`.
fn method_call(args: &[i64]) -> i64 {
    if args.len() < 2 {
        return type_error("call() missing required arguments: 'handle' and 'method'");
    }
    let Some(handle) = handle_argument(args[0]) else {
        return type_error("call() argument 'handle' must be a sidecar handle capsule");
    };
    if let Some(error) = check_process(handle) {
        return error;
    }
    let Some(method) = value::as_str(args[1]) else {
        return type_error("call() argument 'method' must be str");
    };
    if method.is_empty() {
        return value_error("sidecar: method name must not be empty");
    }
    if method.len() > MAX_METHOD {
        return value_error(&format!(
            "sidecar: method name is longer than {MAX_METHOD} bytes"
        ));
    }
    let values = &args[2..];

    // One request in flight per handle: the slot mutex serialises frames.
    let (id, payload) = {
        let mut slot = lock_slot(handle);
        if let Err(error) = ensure_worker(handle, &mut slot) {
            return error.raise();
        }
        let id = slot.next_id;
        slot.next_id = next_request_id(id);
        // Encode before parking: the parked region only moves bytes.
        let frame = match encode_request(id, method, values) {
            Ok(frame) => frame,
            Err(error) => return error.raise(),
        };
        match exchange(&mut slot, &frame) {
            Ok(payload) => (id, payload),
            Err(error) => {
                // The request may have reached the worker; never retry it.
                // Reset the slot so the next call starts from a clean pipe.
                reset_worker(&mut slot);
                return error.raise();
            }
        }
    };

    // Decoding allocates Hydro values, so it runs outside the parked region.
    match decode_response(&payload, id) {
        Ok(Response::Value(item)) => item,
        Ok(Response::Remote { tag, message }) => raise_remote(tag, &message),
        Err(message) => runtime_error(&message),
    }
}

/// `_hydro_sidecar.restart(handle) -> None`.
fn restart(args: &[i64]) -> i64 {
    let handle = match require_handle(args, "restart") {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    if let Some(error) = check_process(handle) {
        return error;
    }
    let mut slot = lock_slot(handle);
    if slot.closed {
        return runtime_error("sidecar: handle is closed");
    }
    stop_worker(&mut slot);
    match spawn_worker(handle, &mut slot) {
        Ok(()) => NONE,
        // The slot stays empty; the next call may retry the launch.
        Err(error) => error.raise(),
    }
}

/// `_hydro_sidecar.close(handle) -> None`.
fn close(args: &[i64]) -> i64 {
    let handle = match require_handle(args, "close") {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    if std::process::id() != handle.owner_pid {
        // The worker belongs to the parent: drop the local pipe handles
        // without signalling, so two processes cannot kill each other's
        // worker. `try_lock` never waits: after `fork` the inherited mutex may
        // still look locked to a thread that does not exist in this process.
        if let Ok(mut slot) = handle.slot.try_lock() {
            slot.closed = true;
            clear_slot(&mut slot);
        }
        return NONE;
    }
    let mut slot = lock_slot(handle);
    slot.closed = true;
    stop_worker(&mut slot);
    NONE
}

/// `_hydro_sidecar.alive(handle) -> bool`.
fn alive(args: &[i64]) -> i64 {
    let handle = match require_handle(args, "alive") {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    if let Some(error) = check_process(handle) {
        return error;
    }
    let mut slot = lock_slot(handle);
    if slot.closed {
        return value::FALSE;
    }
    match slot.child.as_mut().map(|child| child.try_wait()) {
        Some(Ok(None)) => value::TRUE,
        Some(Ok(Some(_))) => {
            // `try_wait` already reaped the child.
            clear_slot(&mut slot);
            value::FALSE
        }
        Some(Err(_)) => {
            reset_worker(&mut slot);
            value::FALSE
        }
        None => value::FALSE,
    }
}

/// Dispatches `_hydro_sidecar.*` calls.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "spawn" => spawn(args),
        "call" => method_call(args),
        "restart" => restart(args),
        "close" => close(args),
        "alive" => alive(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '_hydro_sidecar' has no attribute '{function}'"),
        ),
    }
}

/// Module constant lookup: `PROTOCOL_VERSION` and `MAX_FRAME`.
pub(crate) fn constant(name: &str) -> Option<i64> {
    match name {
        "PROTOCOL_VERSION" => value::encode_int(i64::from(PROTOCOL_VERSION)),
        "MAX_FRAME" => value::encode_int(MAX_FRAME as i64),
        _ => None,
    }
}

// -------------------------------------------------------------------------
// Test helpers for the codec (request side; the host only sends requests)
// -------------------------------------------------------------------------

/// Decodes a request payload; mirrors [`decode_response`] for the direction the
/// host only produces. Test-only: the runtime never parses requests.
#[cfg(test)]
fn decode_request(payload: &[u8]) -> Result<(u32, String, Vec<i64>), String> {
    if payload.len() < 6 {
        return Err(protocol_error("truncated payload"));
    }
    let mut reader = Reader::new(payload);
    let version = reader.u8()?;
    if version != PROTOCOL_VERSION {
        return Err(version_mismatch(version));
    }
    let id = reader.u32()?;
    let kind = reader.u8()?;
    if kind != KIND_REQUEST {
        return Err(protocol_error(&format!(
            "expected request kind, got {kind}"
        )));
    }
    let method_length = reader.u32()? as usize;
    if method_length > MAX_METHOD {
        return Err(protocol_error(&format!(
            "method name is longer than {MAX_METHOD} bytes"
        )));
    }
    let method = std::str::from_utf8(reader.take(method_length)?)
        .map_err(|_| protocol_error("invalid UTF-8 in method"))?
        .to_string();
    let argc = reader.u32()? as usize;
    if argc > reader.remaining() {
        return Err(protocol_error("truncated payload"));
    }

    let root_start = heap::root_values(&[]);
    let mut values = Vec::with_capacity(argc.min(reader.remaining()));
    let mut failure = None;
    for _ in 0..argc {
        match decode_value(&mut reader, 0) {
            Ok(item) => values.push(item),
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    heap::release_roots(root_start);
    if let Some(error) = failure {
        return Err(error);
    }
    if reader.remaining() != 0 {
        return Err(protocol_error("trailing bytes after value"));
    }
    Ok((id, method, values))
}

/// Builds an `ok` response payload.
#[cfg(test)]
fn encode_response_ok(id: u32, item: i64) -> Vec<u8> {
    let mut payload = vec![PROTOCOL_VERSION];
    payload.extend_from_slice(&id.to_le_bytes());
    payload.push(KIND_OK);
    let mut path = Vec::new();
    encode_value(&mut payload, item, &mut path, 0).expect("test value must encode");
    payload
}

/// Builds an `error` response payload.
#[cfg(test)]
fn encode_response_error(id: u32, tag: u32, message: &str) -> Vec<u8> {
    let mut payload = vec![PROTOCOL_VERSION];
    payload.extend_from_slice(&id.to_le_bytes());
    payload.push(KIND_ERROR);
    payload.push(tag as u8);
    payload.extend_from_slice(&(message.len() as u32).to_le_bytes());
    payload.extend_from_slice(message.as_bytes());
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container;

    /// Serialises tests that touch the process-global exception state.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_guard() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A tagged integer that fits the immediate range.
    fn int(number: i64) -> i64 {
        value::encode_int(number).expect("test integer must fit the tagged range")
    }

    fn pending_message() -> String {
        value::exception_data(exc::pending_value())
            .and_then(|data| data.args.first().cloned())
            .unwrap_or_else(|| value::repr(exc::pending_value()))
    }

    fn assert_no_exception() {
        assert!(
            !exc::pending(),
            "unexpected exception: {}",
            pending_message()
        );
    }

    #[test]
    fn codec_roundtrips_every_value_tag() {
        let _guard = test_guard();
        exc::init();

        let nested_list = value::list_new(vec![int(1), value::str_alloc("x".to_string())]);
        let dict = value::dict_new(vec![
            (value::str_alloc("k".to_string()), int(7)),
            (int(3), value::str_alloc("v".to_string())),
        ]);
        let values = vec![
            NONE,
            value::TRUE,
            value::FALSE,
            int(42),
            value::bigint_new(BigInt::from(i64::MAX)),
            value::float_new(1.5),
            value::str_alloc("héllo".to_string()),
            value::bytes_new(vec![0, 1, 255]),
            nested_list,
            value::tuple_new(vec![value::FALSE, value::str_alloc("t".to_string())]),
            dict,
        ];
        let roots = heap::root_values(&values);

        let frame = encode_request(9, "echo", &values).expect("request must encode");
        let payload = &frame[4..];
        assert_eq!(
            u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize,
            payload.len()
        );

        let (id, method, decoded) = decode_request(payload).expect("request must decode");
        assert_eq!(id, 9);
        assert_eq!(method, "echo");
        assert_eq!(decoded.len(), values.len());
        assert_eq!(value::as_str(decoded[6]), Some("héllo"));
        assert_eq!(value::as_bytes(decoded[7]), Some(&[0u8, 1, 255][..]));
        assert_eq!(value::as_integer(decoded[3]), Some(42));
        assert_eq!(value::as_float(decoded[5]), Some(1.5));

        let decoded_roots = heap::root_values(&decoded);
        let reencoded = encode_request(9, "echo", &decoded).expect("decoded values must encode");
        heap::release_roots(decoded_roots);
        assert_eq!(frame, reencoded, "codec must be canonical");

        heap::release_roots(roots);
    }

    #[test]
    fn codec_rejects_oversized_nesting() {
        let _guard = test_guard();
        exc::init();

        let mut nested = int(1);
        for _ in 0..=MAX_DEPTH + 1 {
            heap::root_values(&[nested]);
            nested = value::list_new(vec![nested]);
        }
        let error = encode_request(1, "echo", &[nested]).expect_err("nesting must be rejected");
        assert_eq!(error.tag, VALUE_ERROR);
        assert!(
            error.message.contains("nesting exceeds"),
            "{}",
            error.message
        );
    }

    #[test]
    fn codec_rejects_cyclic_containers() {
        let _guard = test_guard();
        exc::init();

        let list = value::list_new(vec![NONE]);
        heap::root_values(&[list]);
        let _ = container::list_append(list, list);

        let error = encode_request(1, "echo", &[list]).expect_err("cycle must be rejected");
        assert_eq!(error.tag, VALUE_ERROR);
        assert!(error.message.contains("cyclic"), "{}", error.message);
    }

    #[test]
    fn codec_rejects_unsupported_values_and_wide_integers() {
        let _guard = test_guard();
        exc::init();

        let capsule = value::capsule_new(1, Some("fixture".to_string()));
        let error = encode_request(1, "echo", &[capsule]).expect_err("capsule must be rejected");
        assert_eq!(error.tag, TYPE_ERROR);
        assert!(error.message.contains("PyCapsule"), "{}", error.message);

        let wide = value::bigint_new(BigInt::from(1i64) << 80usize);
        let error = encode_request(1, "echo", &[wide]).expect_err("wide int must be rejected");
        assert_eq!(error.tag, OVERFLOW_ERROR);
        assert!(
            error.message.contains("does not fit in 64 bits"),
            "{}",
            error.message
        );

        let dict = value::dict_new(vec![(value::TRUE, int(1))]);
        let error = encode_request(1, "echo", &[dict]).expect_err("bool key must be rejected");
        assert_eq!(error.tag, TYPE_ERROR);
        assert!(error.message.contains("bool"), "{}", error.message);
    }

    #[test]
    fn frame_length_limits_are_enforced() {
        assert!(validate_frame_length(0).is_err());
        assert!(validate_frame_length(5).is_err());
        assert!(validate_frame_length(6).is_ok());
        assert!(validate_frame_length(MAX_FRAME).is_ok());
        assert!(validate_frame_length(MAX_FRAME + 1).is_err());
        assert_eq!(
            validate_frame_length(0).expect_err("zero length"),
            "sidecar: protocol error: invalid frame length 0"
        );
    }

    #[test]
    fn response_validation_rejects_bad_headers() {
        let _guard = test_guard();
        exc::init();

        let payload = encode_response_ok(7, int(5));
        match decode_response(&payload, 7) {
            Ok(Response::Value(item)) => assert_eq!(value::as_integer(item), Some(5)),
            _ => panic!("expected a value response"),
        }

        assert!(decode_response(&payload, 8)
            .expect_err("id mismatch")
            .contains("does not match request id"));

        let mut wrong_version = payload.clone();
        wrong_version[0] = 2;
        assert_eq!(
            decode_response(&wrong_version, 7).expect_err("version mismatch"),
            "sidecar: worker speaks protocol version 2, expected 1"
        );

        let mut wrong_kind = payload.clone();
        wrong_kind[5] = 9;
        assert!(decode_response(&wrong_kind, 7)
            .expect_err("kind mismatch")
            .contains("unexpected response kind 9"));

        let mut trailing = payload.clone();
        trailing.push(0);
        assert!(decode_response(&trailing, 7)
            .expect_err("trailing bytes")
            .contains("trailing bytes after value"));

        assert!(decode_response(&payload[..5], 7).is_err());

        let mut unknown = vec![PROTOCOL_VERSION];
        unknown.extend_from_slice(&1u32.to_le_bytes());
        unknown.push(KIND_OK);
        unknown.push(250);
        assert!(decode_response(&unknown, 1)
            .expect_err("unknown tag")
            .contains("unknown value tag 250"));
    }

    #[test]
    fn handshake_frame_matches_the_wire_spec() {
        let _guard = test_guard();
        exc::init();

        let version = int(i64::from(PROTOCOL_VERSION));
        let frame = encode_request(0, "__hello__", &[version]).expect("handshake encodes");

        let mut expected = 32u32.to_le_bytes().to_vec();
        expected.extend_from_slice(&[1, 0, 0, 0, 0, 1]); // version, id 0, kind request
        expected.extend_from_slice(&9u32.to_le_bytes());
        expected.extend_from_slice(b"__hello__");
        expected.extend_from_slice(&1u32.to_le_bytes()); // argc
        expected.push(TAG_INT);
        expected.extend_from_slice(&1i64.to_le_bytes());
        assert_eq!(frame, expected);
    }

    #[test]
    fn decode_rejects_oversized_nesting() {
        let _guard = test_guard();
        exc::init();

        let mut payload = vec![PROTOCOL_VERSION];
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(KIND_OK);
        for _ in 0..=MAX_DEPTH + 1 {
            payload.push(TAG_LIST);
            payload.extend_from_slice(&1u32.to_le_bytes());
        }
        payload.push(TAG_NONE);
        let message = decode_response(&payload, 1).expect_err("nesting must be rejected");
        assert!(message.contains("nesting exceeds"), "{message}");
    }

    #[test]
    fn remote_errors_map_onto_runtime_tags() {
        let _guard = test_guard();
        exc::init();

        assert_eq!(raise_remote(0, "boom"), NONE);
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        assert_eq!(pending_message(), "boom");
        exc::clear();

        let payload = encode_response_error(4, 3, "remote failure");
        match decode_response(&payload, 4) {
            Ok(Response::Remote { tag, message }) => {
                assert_eq!(tag, RUNTIME_ERROR);
                assert_eq!(message, "remote failure");
            }
            _ => panic!("expected a remote error"),
        }

        assert_eq!(raise_remote(200, "wild"), NONE);
        assert_eq!(exc::pending_tag(), i64::from(RUNTIME_ERROR));
        let message = pending_message();
        assert!(message.contains("unknown exception tag 200"), "{message}");
        assert!(message.contains("wild"), "{message}");
        exc::clear();
    }

    #[test]
    fn remote_error_messages_are_capped() {
        let _guard = test_guard();
        exc::init();

        let mut payload = vec![PROTOCOL_VERSION];
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(KIND_ERROR);
        payload.push(3);
        payload.extend_from_slice(&((MAX_ERROR + 1) as u32).to_le_bytes());
        assert!(decode_response(&payload, 1)
            .expect_err("oversized message")
            .contains("error message exceeds"));
    }

    // -- Worker integration (skipped when python3 is unavailable) ----------

    /// A minimal protocol-v1 worker: handshake, echo, typed error, and the
    /// malformed replies the host must reject.
    const PY_WORKER: &str = r#"
import sys, time, struct
IN = sys.stdin.buffer
OUT = sys.stdout.buffer

def read_header():
    data = b''
    while len(data) < 4:
        chunk = IN.read(4 - len(data))
        if not chunk:
            return None
        data += chunk
    return data

def read_exact(n):
    data = b''
    while len(data) < n:
        chunk = IN.read(n - len(data))
        if not chunk:
            sys.exit(0)
        data += chunk
    return data

class Cursor:
    def __init__(self, data):
        self.data = data
        self.pos = 0
    def take(self, n):
        if self.pos + n > len(self.data):
            sys.exit(2)
        out = self.data[self.pos:self.pos + n]
        self.pos += n
        return out
    def u8(self):
        return self.take(1)[0]
    def u32(self):
        return int.from_bytes(self.take(4), 'little')
    def value(self):
        tag = self.u8()
        if tag == 0:
            return None
        if tag == 1:
            return False
        if tag == 2:
            return True
        if tag == 3:
            return int.from_bytes(self.take(8), 'little', signed=True)
        if tag == 4:
            return struct.unpack('<d', self.take(8))[0]
        if tag == 5:
            return self.take(self.u32()).decode('utf-8')
        if tag == 6:
            return [self.value() for _ in range(self.u32())]
        if tag == 7:
            n = self.u32()
            return {self.value(): self.value() for _ in range(n)}
        if tag == 8:
            return bytes(self.take(self.u32()))
        if tag == 9:
            return tuple(self.value() for _ in range(self.u32()))
        sys.exit(3)

def encode(value):
    out = bytearray()
    def put(v):
        if v is None:
            out.append(0)
        elif v is False:
            out.append(1)
        elif v is True:
            out.append(2)
        elif isinstance(v, int):
            out.append(3)
            out.extend(int(v).to_bytes(8, 'little', signed=True))
        elif isinstance(v, float):
            out.append(4)
            out.extend(struct.pack('<d', v))
        elif isinstance(v, str):
            raw = v.encode('utf-8')
            out.append(5)
            out.extend(len(raw).to_bytes(4, 'little'))
            out.extend(raw)
        elif isinstance(v, list):
            out.append(6)
            out.extend(len(v).to_bytes(4, 'little'))
            for item in v:
                put(item)
        elif isinstance(v, dict):
            out.append(7)
            out.extend(len(v).to_bytes(4, 'little'))
            for key, item in v.items():
                put(key)
                put(item)
        elif isinstance(v, bytes):
            out.append(8)
            out.extend(len(v).to_bytes(4, 'little'))
            out.extend(v)
        elif isinstance(v, tuple):
            out.append(9)
            out.extend(len(v).to_bytes(4, 'little'))
            for item in v:
                put(item)
        else:
            sys.exit(4)
    put(value)
    return bytes(out)

def write_payload(request_id, kind, body, version=1):
    payload = bytes([version]) + request_id.to_bytes(4, 'little') + bytes([kind]) + body
    OUT.write(len(payload).to_bytes(4, 'little'))
    OUT.write(payload)
    OUT.flush()

def reply(request_id, value):
    write_payload(request_id, 2, encode(value))

def fail(request_id, tag, message):
    raw = message.encode('utf-8')
    write_payload(request_id, 3, bytes([tag]) + len(raw).to_bytes(4, 'little') + raw)

while True:
    header = read_header()
    if header is None:
        sys.exit(0)
    payload = read_exact(int.from_bytes(header, 'little'))
    cursor = Cursor(payload)
    cursor.u8()
    request_id = cursor.u32()
    cursor.u8()
    method = cursor.take(cursor.u32()).decode('utf-8')
    args = [cursor.value() for _ in range(cursor.u32())]
    if method == '__hello__':
        reply(request_id, 'py-worker')
    elif method == 'echo':
        reply(request_id, args[0] if args else None)
    elif method == 'fail':
        fail(request_id, 3, 'boom')
    elif method == 'badversion':
        write_payload(request_id, 2, encode('x'), version=2)
    elif method == 'badid':
        write_payload(request_id + 1, 2, encode('x'))
    elif method == 'trailing':
        write_payload(request_id, 2, encode('x') + b'\x00')
    elif method == 'oversized':
        OUT.write((16 * 1024 * 1024 + 1).to_bytes(4, 'little'))
        OUT.flush()
        time.sleep(5)
    elif method == 'exit':
        sys.exit(1)
    else:
        fail(request_id, 9, 'unknown method ' + method)
"#;

    fn python_available() -> bool {
        Command::new("python3")
            .arg("-c")
            .arg("import sys")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn spawn_python(script: &str) -> i64 {
        let arguments = value::list_new(vec![
            value::str_alloc("-c".to_string()),
            value::str_alloc(script.to_string()),
        ]);
        let args = [value::str_alloc("python3".to_string()), arguments];
        spawn(&args)
    }

    fn echo(handle: i64, text: &str) -> i64 {
        call(
            "call",
            &[
                handle,
                value::str_alloc("echo".to_string()),
                value::str_alloc(text.to_string()),
            ],
        )
    }

    #[test]
    fn worker_handshake_echo_restart_and_close() {
        let _guard = test_guard();
        exc::init();
        if !python_available() {
            eprintln!("skipping sidecar worker test: python3 is not available");
            return;
        }

        let handle = spawn_python(PY_WORKER);
        assert_no_exception();

        let result = echo(handle, "hello");
        assert_no_exception();
        assert_eq!(value::as_str(result), Some("hello"));

        assert_eq!(alive(&[handle]), value::TRUE);
        assert_no_exception();

        assert_eq!(restart(&[handle]), NONE);
        assert_no_exception();

        let result = echo(handle, "after restart");
        assert_no_exception();
        assert_eq!(value::as_str(result), Some("after restart"));

        // A remote error keeps the worker usable.
        let failed = call("call", &[handle, value::str_alloc("fail".to_string())]);
        assert_eq!(failed, NONE);
        assert_eq!(exc::pending_tag(), i64::from(RUNTIME_ERROR));
        assert_eq!(pending_message(), "boom");
        exc::clear();
        let result = echo(handle, "still here");
        assert_no_exception();
        assert_eq!(value::as_str(result), Some("still here"));

        assert_eq!(close(&[handle]), NONE);
        assert_eq!(close(&[handle]), NONE);
        assert_eq!(alive(&[handle]), value::FALSE);
        let _ = echo(handle, "closed");
        assert_eq!(exc::pending_tag(), i64::from(RUNTIME_ERROR));
        assert!(pending_message().contains("handle is closed"));
        exc::clear();
    }

    #[test]
    fn worker_protocol_violations_reset_the_slot() {
        let _guard = test_guard();
        exc::init();
        if !python_available() {
            eprintln!("skipping sidecar worker test: python3 is not available");
            return;
        }

        let handle = spawn_python(PY_WORKER);
        assert_no_exception();

        let cases: [(&str, &str); 4] = [
            ("badversion", "protocol version 2, expected 1"),
            ("badid", "does not match request id"),
            ("trailing", "trailing bytes after value"),
            ("oversized", "invalid frame length"),
        ];
        for (method, expected) in cases {
            let _ = call("call", &[handle, value::str_alloc(method.to_string())]);
            assert_eq!(exc::pending_tag(), i64::from(RUNTIME_ERROR), "{method}");
            let message = pending_message();
            assert!(message.contains(expected), "{method}: {message}");
            exc::clear();

            // The slot was reset; the next call respawns and works.
            let result = echo(handle, "recovered");
            assert_no_exception();
            assert_eq!(value::as_str(result), Some("recovered"), "{method}");
        }

        assert_eq!(close(&[handle]), NONE);
    }

    #[test]
    fn worker_crash_is_reported_and_recovered() {
        let _guard = test_guard();
        exc::init();
        if !python_available() {
            eprintln!("skipping sidecar worker test: python3 is not available");
            return;
        }

        let handle = spawn_python(PY_WORKER);
        assert_no_exception();

        let _ = call("call", &[handle, value::str_alloc("exit".to_string())]);
        assert!(exc::pending(), "a vanished worker must raise");
        exc::clear();

        let result = echo(handle, "respawned");
        assert_no_exception();
        assert_eq!(value::as_str(result), Some("respawned"));

        assert_eq!(close(&[handle]), NONE);
    }

    #[test]
    fn spawn_and_handshake_failures_raise_without_leaking_a_handle() {
        let _guard = test_guard();
        exc::init();
        if !python_available() {
            eprintln!("skipping sidecar worker test: python3 is not available");
            return;
        }

        let missing = [value::str_alloc(
            "/nonexistent/hydro-sidecar-program".to_string(),
        )];
        assert_eq!(spawn(&missing), NONE);
        assert_eq!(exc::pending_tag(), i64::from(OS_ERROR));
        assert!(pending_message().contains("spawn failed"));
        exc::clear();

        // A worker that exits before answering the handshake.
        let silent = spawn_python("import sys\nsys.exit(0)\n");
        assert_eq!(silent, NONE);
        assert_eq!(exc::pending_tag(), i64::from(RUNTIME_ERROR));
        assert!(pending_message().contains("handshake failed"));
        exc::clear();
    }
}
