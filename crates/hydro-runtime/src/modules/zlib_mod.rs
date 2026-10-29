//! `zlib`: DEFLATE compression, checksums and the incremental codec objects.
//!
//! The codecs are the real system `libz` streams (linked through `-lz`), not a
//! reimplementation: `compress`/`decompress` and the `compressobj`/
//! `decompressobj` objects produce exactly the bytes CPython's `zlib` module
//! produces for the same parameters. The module mirrors CPython 3.14's
//! `Modules/zlibmodule.c` argument handling:
//!
//! * `compress(data, level=-1, wbits=MAX_WBITS)` asks `deflateInit2` and maps
//!   a rejected option to `zlib.error("Bad compression level")`, like the C
//!   module; `decompress(data, wbits=MAX_WBITS, bufsize=DEF_BUF_SIZE)` leaves
//!   window validation to `inflateInit2`, so its message comes from zlib.
//! * `compressobj(level=-1, method=DEFLATED, wbits=MAX_WBITS,
//!   memLevel=DEF_MEM_LEVEL, strategy=Z_DEFAULT_STRATEGY)` and
//!   `decompressobj(wbits=MAX_WBITS, zdict=b'')` reject unsupported options
//!   with `ValueError("Invalid initialization option")`.
//! * `Compress.compress/flush/copy` and `Decompress.decompress/flush/copy`
//!   stream through a live `z_stream`; `Decompress` keeps `unused_data`,
//!   `unconsumed_tail` and `eof` up to date after every call, exactly like
//!   the C extension.
//!
//! ## Native state
//!
//! Instances carry an integer handle in a private attribute; the actual
//! `z_stream` lives in a process-wide registry guarded by a mutex.
//! `deflateCopy` and `inflateCopy` back `copy()`. A stream is released when
//! its `Compress`/`Decompress` object is finished (`flush(Z_FINISH)`) or when
//! the process exits: Hydro has no finaliser hook for native instances, so a
//! program that abandons a codec object without finishing it keeps the
//! underlying zlib allocation until exit. The registry is not keyed to the
//! heap generation because a reset in one thread must never release a live
//! stream owned by another.
//!
//! ## Failure policy
//!
//! Bad argument types raise `TypeError`, unsupported options raise
//! `ValueError`, and every failure reported by libz raises `zlib.error`
//! (a subclass of `Exception`) with CPython's
//! `Error -N while <action>: <detail>` phrasing. No input can panic.

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_uint, c_ulong, c_void};
use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, EXCEPTION, MEMORY_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::value::{self, NONE};

// -------------------------------------------------------------------------
// zlib constants (values from `zlib.h`)
// -------------------------------------------------------------------------

const Z_NO_FLUSH: c_int = 0;
const Z_PARTIAL_FLUSH: c_int = 1;
const Z_SYNC_FLUSH: c_int = 2;
const Z_FULL_FLUSH: c_int = 3;
const Z_FINISH: c_int = 4;
const Z_BLOCK: c_int = 5;
const Z_TREES: c_int = 6;

const Z_OK: c_int = 0;
const Z_STREAM_END: c_int = 1;
const Z_NEED_DICT: c_int = 2;
const Z_ERRNO: c_int = -1;
const Z_STREAM_ERROR: c_int = -2;
const Z_DATA_ERROR: c_int = -3;
const Z_MEM_ERROR: c_int = -4;
const Z_BUF_ERROR: c_int = -5;
const Z_VERSION_ERROR: c_int = -6;

const Z_NO_COMPRESSION: i64 = 0;
const Z_BEST_SPEED: i64 = 1;
const Z_BEST_COMPRESSION: i64 = 9;
const Z_DEFAULT_COMPRESSION: i64 = -1;
const DEFLATED: i64 = 8;
const DEF_MEM_LEVEL: i64 = 8;
const MAX_WBITS: i64 = 15;
const DEF_BUF_SIZE: usize = 16_384;

/// Output chunk produced per `deflate`/`inflate` iteration.
const OUT_CHUNK: usize = 65_536;

/// Private instance attribute holding the registry handle.
const STREAM_ATTR: &str = "_zlib_stream";

/// libz's default compression strategy (`Z_DEFAULT_STRATEGY`).
const Z_DEFAULT_STRATEGY: c_int = 0;

// -------------------------------------------------------------------------
// libz FFI
// -------------------------------------------------------------------------

type AllocFunc = unsafe extern "C" fn(*mut c_void, c_uint, c_uint) -> *mut c_void;
type FreeFunc = unsafe extern "C" fn(*mut c_void, *mut c_void);

/// Mirrors `struct z_stream_s` from `zlib.h`. `uLong` is `unsigned long`,
/// which is `c_ulong` on every platform libz supports.
#[repr(C)]
struct ZStream {
    next_in: *const u8,
    avail_in: c_uint,
    total_in: c_ulong,
    next_out: *mut u8,
    avail_out: c_uint,
    total_out: c_ulong,
    msg: *const c_char,
    state: *mut c_void,
    zalloc: Option<AllocFunc>,
    zfree: Option<FreeFunc>,
    opaque: *mut c_void,
    data_type: c_int,
    adler: c_ulong,
    reserved: c_ulong,
}

#[link(name = "z")]
extern "C" {
    fn zlibVersion() -> *const c_char;
    fn deflateInit2_(
        strm: *mut ZStream,
        level: c_int,
        method: c_int,
        window_bits: c_int,
        mem_level: c_int,
        strategy: c_int,
        version: *const c_char,
        stream_size: c_int,
    ) -> c_int;
    fn deflate(strm: *mut ZStream, flush: c_int) -> c_int;
    fn deflateEnd(strm: *mut ZStream) -> c_int;
    fn deflateCopy(dest: *mut ZStream, source: *mut ZStream) -> c_int;
    fn inflateInit2_(
        strm: *mut ZStream,
        window_bits: c_int,
        version: *const c_char,
        stream_size: c_int,
    ) -> c_int;
    fn inflate(strm: *mut ZStream, flush: c_int) -> c_int;
    fn inflateEnd(strm: *mut ZStream) -> c_int;
    fn inflateCopy(dest: *mut ZStream, source: *mut ZStream) -> c_int;
    fn inflateSetDictionary(strm: *mut ZStream, dictionary: *const u8, length: c_uint) -> c_int;
    #[link_name = "crc32"]
    fn z_crc32(crc: c_ulong, buf: *const u8, len: c_uint) -> c_ulong;
    #[link_name = "adler32"]
    fn z_adler32(adler: c_ulong, buf: *const u8, len: c_uint) -> c_ulong;
}

fn zstream_size() -> c_int {
    std::mem::size_of::<ZStream>() as c_int
}

fn version_pointer() -> *const c_char {
    // SAFETY: `zlibVersion` returns a pointer to a static NUL-terminated string.
    unsafe { zlibVersion() }
}

fn new_stream() -> Box<ZStream> {
    // SAFETY: a zeroed `z_stream` is the documented "uninitialised" state;
    // a null `zalloc`/`zfree` selects zlib's default allocator.
    Box::new(unsafe { std::mem::zeroed() })
}

/// `zError` names for statuses libz does not describe through `strm.msg`.
fn z_error_name(status: c_int) -> &'static str {
    match status {
        Z_OK => "",
        Z_STREAM_END => "stream end",
        Z_NEED_DICT => "need dictionary",
        Z_ERRNO => "file error",
        Z_STREAM_ERROR => "stream error",
        Z_DATA_ERROR => "data error",
        Z_MEM_ERROR => "out of memory",
        Z_BUF_ERROR => "buffer error",
        Z_VERSION_ERROR => "version error",
        _ => "unknown error",
    }
}

/// A libz failure: the raw status plus the message libz stored on the stream.
struct Failure {
    status: c_int,
    message: String,
}

impl Failure {
    fn from_stream(stream: &ZStream, status: c_int) -> Failure {
        let message = if stream.msg.is_null() {
            String::new()
        } else {
            // SAFETY: zlib sets `msg` to a NUL-terminated string it owns for
            // the lifetime of the stream.
            unsafe { CStr::from_ptr(stream.msg) }
                .to_string_lossy()
                .into_owned()
        };
        Failure { status, message }
    }

    fn truncated() -> Failure {
        Failure {
            status: Z_BUF_ERROR,
            message: "incomplete or truncated stream".to_string(),
        }
    }

    fn detail(&self) -> &str {
        if self.message.is_empty() {
            z_error_name(self.status)
        } else {
            self.message.as_str()
        }
    }

    /// CPython's `Error -N while <action>: <detail>` text.
    fn render(&self, action: &str) -> String {
        format!("Error {} while {}: {}", self.status, action, self.detail())
    }
}

/// One-shot inflate failure: initialisation errors are reported separately so
/// the caller can use CPython's "preparing to decompress data" phrasing.
enum InflateFailure {
    Init(Failure),
    Decode(Failure),
}

// -------------------------------------------------------------------------
// Exceptions and small argument helpers
// -------------------------------------------------------------------------

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

/// Reads a positional or keyword slot; omitted optional slots arrive as `None`.
fn argument(args: &[i64], index: usize) -> i64 {
    args.get(index).copied().unwrap_or(NONE)
}

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

fn bytes_argument(value_arg: i64, name: &str) -> Result<Vec<u8>, i64> {
    match value::as_bytes(value_arg) {
        Some(bytes) => Ok(bytes.to_vec()),
        None => Err(type_error(&format!(
            "argument '{name}' must be a bytes-like object, not '{}'",
            value::type_name(value_arg)
        ))),
    }
}

fn encode_int(n: i64) -> i64 {
    value::encode_int(n).unwrap_or(NONE)
}

/// Cached native class lookup through the runtime registry.
fn cached_class(key: &str, build: impl FnOnce() -> i64) -> i64 {
    if let Some(class) = globals::registry_get(key) {
        return class;
    }
    let class = build();
    globals::registry_set(key, class);
    class
}

fn compress_class() -> i64 {
    cached_class("zlib.Compress", || {
        let methods = vec![
            (
                "compress".to_string(),
                value::native_function_new("zlib", "_zlib_compress", usize::MAX),
            ),
            (
                "flush".to_string(),
                value::native_function_new("zlib", "_zlib_flush", usize::MAX),
            ),
            (
                "copy".to_string(),
                value::native_function_new("zlib", "_zlib_copy", usize::MAX),
            ),
        ];
        value::class_new("Compress".to_string(), NONE, methods)
    })
}

/// Methods shared by `zlib.Decompress` (the `decompressobj()` type) and the
/// `_ZlibDecompressor` type gzip instantiates with `wbits=-MAX_WBITS`.
///
/// The `__init__` method carries a real signature (`wbits=MAX_WBITS,
/// zdict=b''`) so keyword binding works; `decompressobj()` allocates the same
/// state directly without going through it.
fn decompress_methods() -> Vec<(String, i64)> {
    let init = value::native_function_new("zlib", "_zlib_decompress_init", usize::MAX);
    value::function_set_signature(
        init,
        vec![
            value::ParamMeta {
                name: "self".to_string(),
                kind: hydro_ir::ParamKind::Positional,
                default: None,
            },
            value::ParamMeta {
                name: "wbits".to_string(),
                kind: hydro_ir::ParamKind::Positional,
                default: Some(encode_int(MAX_WBITS)),
            },
            value::ParamMeta {
                name: "zdict".to_string(),
                kind: hydro_ir::ParamKind::Positional,
                default: Some(NONE),
            },
        ],
    );
    value::function_set_full_entry(init, decompress_init_entry as *const () as usize);
    vec![
        ("__init__".to_string(), init),
        (
            "decompress".to_string(),
            value::native_function_new("zlib", "_zlib_decompress", usize::MAX),
        ),
        (
            "flush".to_string(),
            value::native_function_new("zlib", "_zlib_decompress_flush", usize::MAX),
        ),
        (
            "copy".to_string(),
            value::native_function_new("zlib", "_zlib_decompress_copy", usize::MAX),
        ),
    ]
}

/// `type(decompressobj())`: CPython names this class `zlib.Decompress`.
fn decompress_class() -> i64 {
    cached_class("zlib.Decompress", || {
        value::class_new("Decompress".to_string(), NONE, decompress_methods())
    })
}

/// `zlib._ZlibDecompressor`: the type gzip's `DecompressReader` uses as its
/// `_decomp_factory` (`zlib._ZlibDecompressor(wbits=-MAX_WBITS)`).
fn private_decompressor_class() -> i64 {
    cached_class("zlib._ZlibDecompressor", || {
        value::class_new("_ZlibDecompressor".to_string(), NONE, decompress_methods())
    })
}

/// `zlib._ZlibCompressor`: the mirror type for incremental compression.
fn private_compressor_class() -> i64 {
    cached_class("zlib._ZlibCompressor", || {
        value::class_new(
            "_ZlibCompressor".to_string(),
            NONE,
            vec![
                (
                    "compress".to_string(),
                    value::native_function_new("zlib", "_zlib_compress", usize::MAX),
                ),
                (
                    "flush".to_string(),
                    value::native_function_new("zlib", "_zlib_flush", usize::MAX),
                ),
                (
                    "copy".to_string(),
                    value::native_function_new("zlib", "_zlib_copy", usize::MAX),
                ),
            ],
        )
    })
}

/// `zlib.error`: a subclass of `Exception`, so `except zlib.error` and
/// `except Exception` both catch it.
fn error_class() -> i64 {
    cached_class("zlib.error", || {
        value::class_new(
            "error".to_string(),
            exc::exception_class(EXCEPTION),
            Vec::new(),
        )
    })
}

/// Raises `zlib.error(message)` and returns the `None` dummy value.
fn zlib_error(message: &str) -> i64 {
    let message_value = value::str_alloc(message.to_string());
    let class = error_class();
    let instance = crate::builtins::call(class, 1, [message_value].as_ptr());
    if exc::pending() {
        return NONE;
    }
    exc::raise_value(instance)
}

fn stream_error(failure: &Failure, action: &str) -> i64 {
    zlib_error(&failure.render(action))
}

// -------------------------------------------------------------------------
// Stream registry
// -------------------------------------------------------------------------

struct DeflateStream {
    stream: Box<ZStream>,
    finished: bool,
}

impl Drop for DeflateStream {
    fn drop(&mut self) {
        if !self.stream.state.is_null() {
            // SAFETY: `state` is a live deflate stream owned by this value.
            unsafe { deflateEnd(&mut *self.stream) };
            self.stream.state = std::ptr::null_mut();
        }
    }
}

struct InflateStream {
    stream: Box<ZStream>,
    eof: bool,
    unused: Vec<u8>,
    tail: Vec<u8>,
    zdict: Vec<u8>,
}

impl Drop for InflateStream {
    fn drop(&mut self) {
        if !self.stream.state.is_null() {
            // SAFETY: `state` is a live inflate stream owned by this value.
            unsafe { inflateEnd(&mut *self.stream) };
            self.stream.state = std::ptr::null_mut();
        }
    }
}

enum Stream {
    Deflate(DeflateStream),
    Inflate(InflateStream),
}

// SAFETY: a codec value is only touched while the registry mutex is held, so
// its raw zlib pointers never cross threads unsynchronised.
unsafe impl Send for Stream {}

struct Registry {
    next_handle: u64,
    streams: HashMap<u64, Stream>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| {
    Mutex::new(Registry {
        next_handle: 1,
        streams: HashMap::new(),
    })
});

/// Locks the stream registry.
fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Stores `stream` and returns its handle.
fn register(stream: Stream) -> u64 {
    let mut registry = registry();
    let handle = registry.next_handle;
    registry.next_handle = handle.wrapping_add(1).max(1);
    registry.streams.insert(handle, stream);
    handle
}

/// Reads the registry handle stored on a codec instance.
fn handle_of(instance: i64) -> Result<u64, i64> {
    match value::instance_attr(instance, STREAM_ATTR).and_then(value::as_integer) {
        Some(handle) if handle > 0 => Ok(handle as u64),
        _ => Err(zlib_error("Inconsistent stream state")),
    }
}

/// Attaches `handle` to a freshly created instance.
fn attach_handle(instance: i64, handle: u64) {
    value::instance_setattr(instance, STREAM_ATTR.to_string(), encode_int(handle as i64));
}

/// Copies the derived `Decompress` state so the registry lock can be released
/// before any heap object is allocated.
fn attribute_snapshot(state: &InflateStream) -> (Vec<u8>, Vec<u8>, bool) {
    (state.unused.clone(), state.tail.clone(), state.eof)
}

/// Stores the derived `Decompress` attributes. The first new string object is
/// rooted across the second allocation.
fn store_attributes(instance: i64, unused: Vec<u8>, tail: Vec<u8>, eof: bool) {
    // `_ZlibDecompressor.needs_input` is true while no unconsumed input is
    // buffered. Only instances constructed through that class carry the
    // attribute; `decompressobj()` instances keep CPython's attribute set.
    let needs_input = tail.is_empty();
    let unused = value::bytes_new(unused);
    let roots = heap::root_values(&[unused]);
    let tail = value::bytes_new(tail);
    value::instance_setattr(instance, "unused_data".to_string(), unused);
    value::instance_setattr(instance, "unconsumed_tail".to_string(), tail);
    value::instance_setattr(instance, "eof".to_string(), value::bool_value(eof));
    if value::instance_attr(instance, "needs_input").is_some() {
        value::instance_setattr(
            instance,
            "needs_input".to_string(),
            value::bool_value(needs_input),
        );
    }
    heap::release_roots(roots);
}

// -------------------------------------------------------------------------
// Deflate
// -------------------------------------------------------------------------

/// Feeds `input` to `stream` and appends every produced byte to `out`.
///
/// Returns zlib's status: `Z_STREAM_END` for a finished `Z_FINISH` flush,
/// `Z_OK`/`Z_BUF_ERROR` otherwise. The loop stops with a non-finishing flush
/// once the input is consumed and the output buffer was not filled.
fn deflate_pump(
    stream: &mut ZStream,
    mut input: &[u8],
    flush: c_int,
    out: &mut Vec<u8>,
) -> Result<c_int, Failure> {
    loop {
        if stream.avail_in == 0 && !input.is_empty() {
            let take = input.len().min(u32::MAX as usize);
            stream.next_in = input.as_ptr();
            stream.avail_in = take as c_uint;
            input = &input[take..];
        }
        let mut buffer = vec![0u8; OUT_CHUNK];
        stream.next_out = buffer.as_mut_ptr();
        stream.avail_out = OUT_CHUNK as c_uint;
        // SAFETY: `stream` is initialised; the input and output buffers stay
        // alive for the duration of the call.
        let status = unsafe { deflate(stream, flush) };
        let produced = OUT_CHUNK - stream.avail_out as usize;
        if produced > 0 {
            out.extend_from_slice(&buffer[..produced]);
        }
        match status {
            Z_STREAM_END => return Ok(status),
            Z_OK | Z_BUF_ERROR => {
                let input_pending = stream.avail_in > 0 || !input.is_empty();
                if flush == Z_FINISH {
                    // A finishing flush keeps running until `Z_STREAM_END`;
                    // no progress with output room left means the stream is
                    // wedged, so report the status to the caller.
                    if produced == 0 && !input_pending && stream.avail_out > 0 {
                        return Ok(status);
                    }
                } else if !input_pending && stream.avail_out > 0 {
                    return Ok(status);
                }
            }
            _ => return Err(Failure::from_stream(stream, status)),
        }
    }
}

/// Initialises a deflate stream; the caller owns the returned box.
fn init_deflate(
    level: c_int,
    wbits: c_int,
    mem_level: c_int,
    strategy: c_int,
    method: c_int,
) -> Result<Box<ZStream>, Failure> {
    let mut stream = new_stream();
    // SAFETY: fresh stream; the version string and structure size are the
    // documented libz handshake for the size-checked initialisers.
    let status = unsafe {
        deflateInit2_(
            &mut *stream,
            level,
            method,
            wbits,
            mem_level,
            strategy,
            version_pointer(),
            zstream_size(),
        )
    };
    if status != Z_OK {
        return Err(Failure::from_stream(&stream, status));
    }
    Ok(stream)
}

/// `zlib.compress` and the one-shot path of `Compress`.
fn deflate_all(input: &[u8], level: c_int, wbits: c_int) -> Result<Vec<u8>, Failure> {
    let mut stream = init_deflate(
        level,
        wbits,
        DEF_MEM_LEVEL as c_int,
        Z_DEFAULT_STRATEGY,
        DEFLATED as c_int,
    )?;
    let mut out = Vec::new();
    let status = deflate_pump(&mut stream, input, Z_FINISH, &mut out)?;
    // SAFETY: the stream was initialised above and is not used afterwards.
    unsafe { deflateEnd(&mut *stream) };
    if status != Z_STREAM_END {
        return Err(Failure {
            status,
            message: "incomplete stream".to_string(),
        });
    }
    Ok(out)
}

// -------------------------------------------------------------------------
// Inflate
// -------------------------------------------------------------------------

/// Initialises an inflate stream; the caller owns the returned box.
fn init_inflate(wbits: c_int) -> Result<Box<ZStream>, Failure> {
    let mut stream = new_stream();
    // SAFETY: fresh stream; standard libz initialisation handshake.
    let status = unsafe { inflateInit2_(&mut *stream, wbits, version_pointer(), zstream_size()) };
    if status != Z_OK {
        return Err(Failure::from_stream(&stream, status));
    }
    Ok(stream)
}

/// `zlib.decompress`: inflates one complete stream and ignores trailing data.
fn inflate_all(input: &[u8], wbits: c_int, hint: usize) -> Result<Vec<u8>, InflateFailure> {
    let mut stream = match init_inflate(wbits) {
        Ok(stream) => stream,
        Err(failure) => return Err(InflateFailure::Init(failure)),
    };
    let result = inflate_loop(&mut stream, input, hint, None);
    // SAFETY: `stream` was initialised above and is not used afterwards.
    unsafe { inflateEnd(&mut *stream) };
    result
}

fn inflate_loop(
    stream: &mut ZStream,
    input: &[u8],
    hint: usize,
    zdict: Option<&[u8]>,
) -> Result<Vec<u8>, InflateFailure> {
    let mut out = Vec::with_capacity(hint.min(1 << 26));
    let mut cursor = 0usize;
    let mut dictionary_set = false;
    loop {
        if stream.avail_in == 0 && cursor < input.len() {
            let take = (input.len() - cursor).min(u32::MAX as usize);
            // SAFETY: `cursor` is within `input`.
            stream.next_in = unsafe { input.as_ptr().add(cursor) };
            stream.avail_in = take as c_uint;
            cursor += take;
        }
        let mut buffer = vec![0u8; OUT_CHUNK];
        stream.next_out = buffer.as_mut_ptr();
        stream.avail_out = OUT_CHUNK as c_uint;
        // SAFETY: the stream is initialised and both buffers are live.
        let status = unsafe { inflate(stream, Z_NO_FLUSH) };
        let produced = OUT_CHUNK - stream.avail_out as usize;
        if produced > 0 {
            out.extend_from_slice(&buffer[..produced]);
        }
        match status {
            Z_STREAM_END => return Ok(out),
            Z_OK | Z_BUF_ERROR => {
                let exhausted = stream.avail_in == 0 && cursor >= input.len();
                if exhausted && produced == 0 {
                    return Err(InflateFailure::Decode(Failure::truncated()));
                }
            }
            Z_NEED_DICT => match zdict {
                Some(dictionary) if !dictionary_set => {
                    // SAFETY: the dictionary buffer outlives the call.
                    let result = unsafe {
                        inflateSetDictionary(
                            stream,
                            dictionary.as_ptr(),
                            dictionary.len() as c_uint,
                        )
                    };
                    if result != Z_OK {
                        return Err(InflateFailure::Decode(Failure::from_stream(stream, result)));
                    }
                    dictionary_set = true;
                }
                _ => {
                    return Err(InflateFailure::Decode(Failure::from_stream(stream, status)));
                }
            },
            _ => return Err(InflateFailure::Decode(Failure::from_stream(stream, status))),
        }
    }
}

// -------------------------------------------------------------------------
// Checksums
// -------------------------------------------------------------------------

fn crc32(args: &[i64]) -> i64 {
    let data = match bytes_argument(argument(args, 0), "data") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let initial = match int_argument(argument(args, 1), "value", 0) {
        Ok(initial) => initial,
        Err(error) => return error,
    };
    let mut crc = (initial as u64 & 0xFFFF_FFFF) as c_ulong;
    let mut rest: &[u8] = &data;
    while !rest.is_empty() {
        let take = rest.len().min(u32::MAX as usize);
        // SAFETY: `rest` is a live slice of `take` bytes.
        crc = unsafe { z_crc32(crc, rest.as_ptr(), take as c_uint) };
        rest = &rest[take..];
    }
    encode_int((crc & 0xFFFF_FFFF) as i64)
}

fn adler32(args: &[i64]) -> i64 {
    let data = match bytes_argument(argument(args, 0), "data") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let initial = match int_argument(argument(args, 1), "value", 1) {
        Ok(initial) => initial,
        Err(error) => return error,
    };
    let mut adler = (initial as u64 & 0xFFFF_FFFF) as c_ulong;
    let mut rest: &[u8] = &data;
    while !rest.is_empty() {
        let take = rest.len().min(u32::MAX as usize);
        // SAFETY: `rest` is a live slice of `take` bytes.
        adler = unsafe { z_adler32(adler, rest.as_ptr(), take as c_uint) };
        rest = &rest[take..];
    }
    encode_int((adler & 0xFFFF_FFFF) as i64)
}

// -------------------------------------------------------------------------
// Option validation
// -------------------------------------------------------------------------

/// Whether `mode` is one of the flush values `deflate`/`inflate` understand.
fn valid_flush_mode(mode: i64) -> bool {
    (Z_NO_FLUSH as i64..=Z_BLOCK as i64).contains(&mode)
}

// -------------------------------------------------------------------------
// Public module functions
// -------------------------------------------------------------------------

/// `zlib.compress(data, level=-1, wbits=MAX_WBITS)`.
fn compress(args: &[i64]) -> i64 {
    let data = match bytes_argument(argument(args, 0), "data") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let level = match int_argument(argument(args, 1), "level", Z_DEFAULT_COMPRESSION) {
        Ok(level) => level,
        Err(error) => return error,
    };
    let wbits = match int_argument(argument(args, 2), "wbits", MAX_WBITS) {
        Ok(wbits) => wbits,
        Err(error) => return error,
    };
    match deflate_all(&data, level as c_int, wbits as c_int) {
        Ok(compressed) => value::bytes_new(compressed),
        // `deflateInit2` rejects an unsupported level, window, memLevel or
        // strategy with `Z_STREAM_ERROR`, which CPython reports with this
        // exact message.
        Err(failure) => match failure.status {
            Z_STREAM_ERROR => zlib_error("Bad compression level"),
            Z_MEM_ERROR => {
                exc::raise_with_message(MEMORY_ERROR, "Out of memory while compressing data")
            }
            _ => stream_error(&failure, "compressing data"),
        },
    }
}

/// `zlib.decompress(data, wbits=MAX_WBITS, bufsize=DEF_BUF_SIZE)`.
fn decompress(args: &[i64]) -> i64 {
    let data = match bytes_argument(argument(args, 0), "data") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let wbits = match int_argument(argument(args, 1), "wbits", MAX_WBITS) {
        Ok(wbits) => wbits,
        Err(error) => return error,
    };
    let bufsize = match int_argument(argument(args, 2), "bufsize", DEF_BUF_SIZE as i64) {
        Ok(bufsize) => bufsize,
        Err(error) => return error,
    };
    if bufsize < 0 {
        return value_error("bufsize must be non-negative");
    }
    let hint = if bufsize == 0 { 1 } else { bufsize as usize };
    match inflate_all(&data, wbits as c_int, hint) {
        Ok(plain) => value::bytes_new(plain),
        Err(InflateFailure::Init(failure)) => {
            stream_error(&failure, "preparing to decompress data")
        }
        Err(InflateFailure::Decode(failure)) => stream_error(&failure, "decompressing data"),
    }
}

/// `zlib.compressobj(level=-1, method=DEFLATED, wbits=MAX_WBITS,
/// memLevel=DEF_MEM_LEVEL, strategy=Z_DEFAULT_STRATEGY)`.
fn compressobj(args: &[i64]) -> i64 {
    let level = match int_argument(argument(args, 0), "level", Z_DEFAULT_COMPRESSION) {
        Ok(level) => level,
        Err(error) => return error,
    };
    let method = match int_argument(argument(args, 1), "method", DEFLATED) {
        Ok(method) => method,
        Err(error) => return error,
    };
    let wbits = match int_argument(argument(args, 2), "wbits", MAX_WBITS) {
        Ok(wbits) => wbits,
        Err(error) => return error,
    };
    let mem_level = match int_argument(argument(args, 3), "memLevel", DEF_MEM_LEVEL) {
        Ok(mem_level) => mem_level,
        Err(error) => return error,
    };
    let strategy = match int_argument(argument(args, 4), "strategy", Z_DEFAULT_STRATEGY as i64) {
        Ok(strategy) => strategy,
        Err(error) => return error,
    };
    let stream = match init_deflate(
        level as c_int,
        wbits as c_int,
        mem_level as c_int,
        strategy as c_int,
        method as c_int,
    ) {
        Ok(stream) => stream,
        Err(failure) => {
            return match failure.status {
                Z_STREAM_ERROR => value_error("Invalid initialization option"),
                Z_MEM_ERROR => exc::raise_with_message(
                    MEMORY_ERROR,
                    "Can't allocate memory for compression object",
                ),
                _ => stream_error(&failure, "creating compression object"),
            };
        }
    };
    let handle = register(Stream::Deflate(DeflateStream {
        stream,
        finished: false,
    }));
    let class = compress_class();
    let instance = value::instance_new(class);
    attach_handle(instance, handle);
    instance
}

/// `zlib.decompressobj(wbits=MAX_WBITS, zdict=b'')`.
fn decompressobj(args: &[i64]) -> i64 {
    let wbits = match int_argument(argument(args, 0), "wbits", MAX_WBITS) {
        Ok(wbits) => wbits,
        Err(error) => return error,
    };
    let zdict = match argument(args, 1) {
        NONE => Vec::new(),
        value_arg => match bytes_argument(value_arg, "zdict") {
            Ok(dictionary) => dictionary,
            Err(error) => return error,
        },
    };
    inflate_object(wbits, zdict, false)
}

/// Allocates a `Decompress` instance for `wbits`/`zdict`.
///
/// `incremental` selects the `_ZlibDecompressor` shape: gzip's
/// `DecompressReader` reads `needs_input`, which `zlib.decompressobj()`
/// instances do not expose in CPython, so it is only installed for that class.
fn inflate_object(wbits: i64, zdict: Vec<u8>, incremental: bool) -> i64 {
    let handle = match inflate_state(wbits, zdict) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let instance = value::instance_new(decompress_class());
    attach_handle(instance, handle);
    // Initial attribute values, matching a fresh `zlib.Decompress`.
    store_attributes(instance, Vec::new(), Vec::new(), false);
    if incremental {
        value::instance_setattr(instance, "needs_input".to_string(), value::TRUE);
    }
    instance
}

/// Initialises and registers an inflate stream for `wbits`, feeding `zdict` to
/// raw (`wbits < 0`) streams up front.
fn inflate_state(wbits: i64, zdict: Vec<u8>) -> Result<u64, i64> {
    let stream = match init_inflate(wbits as c_int) {
        Ok(stream) => stream,
        Err(failure) => {
            return Err(match failure.status {
                Z_STREAM_ERROR => value_error("Invalid initialization option"),
                Z_MEM_ERROR => exc::raise_with_message(
                    MEMORY_ERROR,
                    "Can't allocate memory for decompression object",
                ),
                _ => stream_error(&failure, "creating decompression object"),
            });
        }
    };
    let mut state = InflateStream {
        stream,
        eof: false,
        unused: Vec::new(),
        tail: Vec::new(),
        zdict,
    };
    // A raw stream may need its preset dictionary before the first call;
    // zlib-wrapped streams signal it with `Z_NEED_DICT` instead.
    if wbits < 0 && !state.zdict.is_empty() {
        // SAFETY: `state.stream` is initialised and `zdict` is live.
        let status = unsafe {
            inflateSetDictionary(
                &mut *state.stream,
                state.zdict.as_ptr(),
                state.zdict.len() as c_uint,
            )
        };
        if status != Z_OK {
            let failure = Failure::from_stream(&state.stream, status);
            return Err(stream_error(&failure, "preparing to decompress data"));
        }
    }
    Ok(register(Stream::Inflate(state)))
}

/// `_ZlibDecompressor.__init__(self, wbits=MAX_WBITS, zdict=b'')`.
extern "C" fn decompress_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes the signature's three slots.
    decompress_init(unsafe { *argv }, unsafe { *argv.add(1) }, unsafe {
        *argv.add(2)
    })
}

fn decompress_init(instance: i64, wbits: i64, zdict: i64) -> i64 {
    let Some(wbits) = value::as_integer(wbits) else {
        return type_error(&format!(
            "wbits must be an integer, not {}",
            value::type_name(wbits)
        ));
    };
    let zdict = match zdict {
        NONE => Vec::new(),
        value_arg => match bytes_argument(value_arg, "zdict") {
            Ok(dictionary) => dictionary,
            Err(error) => return error,
        },
    };
    let handle = match inflate_state(wbits, zdict) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    attach_handle(instance, handle);
    store_attributes(instance, Vec::new(), Vec::new(), false);
    value::instance_setattr(instance, "needs_input".to_string(), value::TRUE);
    NONE
}

// -------------------------------------------------------------------------
// Method bodies
// -------------------------------------------------------------------------

/// `Compress.compress(data)`.
fn compress_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let data = match bytes_argument(argument(args, 1), "data") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let handle = match handle_of(instance) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let mut registry = registry();
    let outcome = match registry.streams.get_mut(&handle) {
        Some(Stream::Deflate(state)) if !state.finished => {
            let mut out = Vec::new();
            match deflate_pump(&mut state.stream, &data, Z_NO_FLUSH, &mut out) {
                Ok(_) => Ok(out),
                Err(failure) => Err(failure),
            }
        }
        _ => Err(Failure {
            status: Z_STREAM_ERROR,
            message: "inconsistent stream state".to_string(),
        }),
    };
    drop(registry);
    match outcome {
        Ok(out) => value::bytes_new(out),
        Err(failure) => stream_error(&failure, "compressing data"),
    }
}

/// `Compress.flush([mode])`.
fn flush_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let mode = match int_argument(argument(args, 1), "mode", Z_FINISH as i64) {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    if mode == Z_NO_FLUSH as i64 {
        // CPython returns immediately: a no-op flush cannot produce bytes.
        return value::bytes_new(Vec::new());
    }
    let handle = match handle_of(instance) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let mut registry = registry();
    let outcome = match registry.streams.get_mut(&handle) {
        Some(Stream::Deflate(state)) if !state.finished => {
            if !valid_flush_mode(mode) {
                Err(Failure {
                    status: Z_STREAM_ERROR,
                    message: "inconsistent stream state".to_string(),
                })
            } else {
                let mut out = Vec::new();
                match deflate_pump(&mut state.stream, &[], mode as c_int, &mut out) {
                    Ok(status) => {
                        if mode as c_int == Z_FINISH && status != Z_STREAM_END {
                            Err(Failure {
                                status,
                                message: "incomplete stream".to_string(),
                            })
                        } else {
                            if mode as c_int == Z_FINISH {
                                state.finished = true;
                            }
                            Ok(out)
                        }
                    }
                    Err(failure) => Err(failure),
                }
            }
        }
        _ => Err(Failure {
            status: Z_STREAM_ERROR,
            message: "inconsistent stream state".to_string(),
        }),
    };
    drop(registry);
    match outcome {
        Ok(out) => value::bytes_new(out),
        Err(failure) => stream_error(&failure, "flushing"),
    }
}

/// `Compress.copy()`.
fn copy_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let handle = match handle_of(instance) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let class = value::instance_class_value(instance).unwrap_or_else(compress_class);
    let mut registry = registry();
    let outcome = match registry.streams.get_mut(&handle) {
        Some(Stream::Deflate(state)) if !state.finished => {
            let mut copy = new_stream();
            // SAFETY: both streams are valid; `deflateCopy` duplicates the
            // internal state owned by `state`.
            let status = unsafe { deflateCopy(&mut *copy, &mut *state.stream) };
            if status != Z_OK {
                Err(Failure::from_stream(&state.stream, status))
            } else {
                Ok(Stream::Deflate(DeflateStream {
                    stream: copy,
                    finished: false,
                }))
            }
        }
        _ => Err(Failure {
            status: Z_STREAM_ERROR,
            message: "inconsistent stream state".to_string(),
        }),
    };
    drop(registry);
    match outcome {
        Ok(stream) => {
            let handle = register(stream);
            let instance = value::instance_new(class);
            attach_handle(instance, handle);
            instance
        }
        Err(failure) => stream_error(&failure, "copying"),
    }
}

/// `Decompress.decompress(data, max_length=0)`.
fn decompress_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let data = match bytes_argument(argument(args, 1), "data") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let max_length = match int_argument(argument(args, 2), "max_length", 0) {
        Ok(max_length) => max_length,
        Err(error) => return error,
    };
    if max_length < 0 {
        return value_error("max_length must be greater than or equal to zero");
    }
    let handle = match handle_of(instance) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let (outcome, snapshot) = {
        let mut registry = registry();
        let outcome = match registry.streams.get_mut(&handle) {
            Some(Stream::Inflate(state)) => decompress_into(state, &data, max_length as usize),
            _ => Err(Failure {
                status: Z_STREAM_ERROR,
                message: "inconsistent stream state".to_string(),
            }),
        };
        let snapshot = match registry.streams.get(&handle) {
            Some(Stream::Inflate(state)) => Some(attribute_snapshot(state)),
            _ => None,
        };
        (outcome, snapshot)
    };
    if let Some((unused, tail, eof)) = snapshot {
        store_attributes(instance, unused, tail, eof);
    }
    match outcome {
        Ok(out) => value::bytes_new(out),
        Err(failure) => stream_error(&failure, "decompressing data"),
    }
}

/// Streaming inflate with CPython's `max_length`/`unconsumed_tail` semantics.
fn decompress_into(
    state: &mut InflateStream,
    data: &[u8],
    max_length: usize,
) -> Result<Vec<u8>, Failure> {
    // Data arriving after end-of-stream is preserved as `unused_data`.
    if state.eof {
        state.unused.extend_from_slice(data);
        return Ok(Vec::new());
    }
    let mut input = std::mem::take(&mut state.tail);
    input.extend_from_slice(data);
    let mut out = Vec::new();
    let mut cursor = 0usize;
    let mut dictionary_set = false;
    loop {
        if max_length > 0 && out.len() >= max_length {
            break;
        }
        if state.stream.avail_in == 0 && cursor < input.len() {
            let take = (input.len() - cursor).min(u32::MAX as usize);
            // SAFETY: `cursor` is within `input`.
            state.stream.next_in = unsafe { input.as_ptr().add(cursor) };
            state.stream.avail_in = take as c_uint;
            cursor += take;
        }
        let want = if max_length > 0 {
            (max_length - out.len()).min(OUT_CHUNK)
        } else {
            OUT_CHUNK
        };
        let mut buffer = vec![0u8; want];
        state.stream.next_out = buffer.as_mut_ptr();
        state.stream.avail_out = want as c_uint;
        let before_in = state.stream.avail_in;
        // SAFETY: the stream is initialised and both buffers are live.
        let status = unsafe { inflate(&mut *state.stream, Z_NO_FLUSH) };
        let produced = want - state.stream.avail_out as usize;
        if produced > 0 {
            out.extend_from_slice(&buffer[..produced]);
        }
        match status {
            Z_STREAM_END => {
                state.eof = true;
                break;
            }
            Z_OK | Z_BUF_ERROR => {
                let consumed = before_in.saturating_sub(state.stream.avail_in);
                if consumed == 0 && produced == 0 {
                    // No more input: return what is available and wait for it.
                    break;
                }
            }
            Z_NEED_DICT => {
                if dictionary_set || state.zdict.is_empty() {
                    return Err(Failure::from_stream(&state.stream, status));
                }
                // SAFETY: the dictionary buffer outlives the call.
                let result = unsafe {
                    inflateSetDictionary(
                        &mut *state.stream,
                        state.zdict.as_ptr(),
                        state.zdict.len() as c_uint,
                    )
                };
                if result != Z_OK {
                    return Err(Failure::from_stream(&state.stream, result));
                }
                dictionary_set = true;
            }
            _ => return Err(Failure::from_stream(&state.stream, status)),
        }
    }
    let consumed_total = cursor - state.stream.avail_in as usize;
    let leftover = input[consumed_total..].to_vec();
    state.stream.avail_in = 0;
    state.stream.next_in = std::ptr::null();
    if state.eof {
        state.unused.extend_from_slice(&leftover);
        state.tail.clear();
    } else {
        state.tail = leftover;
    }
    Ok(out)
}

/// `Decompress.flush([length])`.
fn decompress_flush(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let length = match int_argument(argument(args, 1), "length", DEF_BUF_SIZE as i64) {
        Ok(length) => length,
        Err(error) => return error,
    };
    if length <= 0 {
        return value_error("length must be greater than zero");
    }
    let handle = match handle_of(instance) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let (outcome, snapshot) = {
        let mut registry = registry();
        let outcome = match registry.streams.get_mut(&handle) {
            Some(Stream::Inflate(state)) => flush_inflate(state, length as usize),
            _ => Err(Failure {
                status: Z_STREAM_ERROR,
                message: "inconsistent stream state".to_string(),
            }),
        };
        let snapshot = match registry.streams.get(&handle) {
            Some(Stream::Inflate(state)) => Some(attribute_snapshot(state)),
            _ => None,
        };
        (outcome, snapshot)
    };
    if let Some((unused, tail, eof)) = snapshot {
        store_attributes(instance, unused, tail, eof);
    }
    match outcome {
        Ok(out) => value::bytes_new(out),
        Err(failure) => stream_error(&failure, "flushing"),
    }
}

fn flush_inflate(state: &mut InflateStream, hint: usize) -> Result<Vec<u8>, Failure> {
    if state.eof {
        return Ok(Vec::new());
    }
    // `flush` also drains the input a limited `decompress` call left behind.
    let input = std::mem::take(&mut state.tail);
    let mut out = Vec::with_capacity(hint.min(1 << 26));
    let mut cursor = 0usize;
    let mut dictionary_set = false;
    let mut reached_end = false;
    loop {
        if state.stream.avail_in == 0 && cursor < input.len() {
            let take = (input.len() - cursor).min(u32::MAX as usize);
            // SAFETY: `cursor` is within `input`.
            state.stream.next_in = unsafe { input.as_ptr().add(cursor) };
            state.stream.avail_in = take as c_uint;
            cursor += take;
        }
        // Feed the leftover input first, then finish the stream.
        let finishing = state.stream.avail_in == 0 && cursor >= input.len();
        let flush = if finishing { Z_FINISH } else { Z_NO_FLUSH };
        let mut buffer = vec![0u8; OUT_CHUNK];
        state.stream.next_out = buffer.as_mut_ptr();
        state.stream.avail_out = OUT_CHUNK as c_uint;
        let before_in = state.stream.avail_in;
        // SAFETY: the stream is initialised and both buffers are live.
        let status = unsafe { inflate(&mut *state.stream, flush) };
        let produced = OUT_CHUNK - state.stream.avail_out as usize;
        if produced > 0 {
            out.extend_from_slice(&buffer[..produced]);
        }
        match status {
            Z_STREAM_END => {
                reached_end = true;
                break;
            }
            Z_OK | Z_BUF_ERROR => {
                let consumed = before_in.saturating_sub(state.stream.avail_in);
                // CPython returns the data produced so far when the stream
                // ends without `Z_STREAM_END`; truncation is not an error
                // here. A stalled call with output room also stops the loop
                // instead of spinning.
                if state.stream.avail_out > 0 && produced == 0 && (finishing || consumed == 0) {
                    break;
                }
            }
            Z_NEED_DICT => {
                if dictionary_set || state.zdict.is_empty() {
                    return Err(Failure::from_stream(&state.stream, status));
                }
                // SAFETY: the dictionary buffer outlives the call.
                let result = unsafe {
                    inflateSetDictionary(
                        &mut *state.stream,
                        state.zdict.as_ptr(),
                        state.zdict.len() as c_uint,
                    )
                };
                if result != Z_OK {
                    return Err(Failure::from_stream(&state.stream, result));
                }
                dictionary_set = true;
            }
            _ => return Err(Failure::from_stream(&state.stream, status)),
        }
    }
    let consumed_total = cursor - state.stream.avail_in as usize;
    let leftover = input[consumed_total..].to_vec();
    state.stream.avail_in = 0;
    state.stream.next_in = std::ptr::null();
    if reached_end {
        state.unused.extend_from_slice(&leftover);
        state.tail.clear();
        state.eof = true;
    } else {
        state.tail = leftover;
    }
    Ok(out)
}

/// `Decompress.copy()`.
fn decompress_copy(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let handle = match handle_of(instance) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let class = value::instance_class_value(instance).unwrap_or_else(decompress_class);
    let mut registry = registry();
    let outcome = match registry.streams.get_mut(&handle) {
        Some(Stream::Inflate(state)) => {
            let mut copy = new_stream();
            // SAFETY: both streams are valid; `inflateCopy` duplicates the
            // internal state owned by `state`.
            let status = unsafe { inflateCopy(&mut *copy, &mut *state.stream) };
            if status != Z_OK {
                Err(Failure::from_stream(&state.stream, status))
            } else {
                Ok(Stream::Inflate(InflateStream {
                    stream: copy,
                    eof: state.eof,
                    unused: state.unused.clone(),
                    tail: state.tail.clone(),
                    zdict: state.zdict.clone(),
                }))
            }
        }
        _ => Err(Failure {
            status: Z_STREAM_ERROR,
            message: "inconsistent stream state".to_string(),
        }),
    };
    drop(registry);
    match outcome {
        Ok(stream) => {
            let snapshot = match &stream {
                Stream::Inflate(state) => Some(attribute_snapshot(state)),
                Stream::Deflate(_) => None,
            };
            let handle = register(stream);
            let instance = value::instance_new(class);
            attach_handle(instance, handle);
            if let Some((unused, tail, eof)) = snapshot {
                store_attributes(instance, unused, tail, eof);
            }
            instance
        }
        Err(failure) => stream_error(&failure, "copying"),
    }
}

// -------------------------------------------------------------------------
// Module surface
// -------------------------------------------------------------------------

/// `zlib.error` as a class value.
pub(crate) fn class_value(module: &str, name: &str) -> Option<i64> {
    match (module, name) {
        ("zlib", "error") => Some(error_class()),
        _ => None,
    }
}

/// Materialises a module constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "error" => error_class(),
        "_ZlibDecompressor" => private_decompressor_class(),
        "_ZlibCompressor" => private_compressor_class(),
        "Z_NO_COMPRESSION" => encode_int(Z_NO_COMPRESSION),
        "Z_BEST_SPEED" => encode_int(Z_BEST_SPEED),
        "Z_BEST_COMPRESSION" => encode_int(Z_BEST_COMPRESSION),
        "Z_DEFAULT_COMPRESSION" => encode_int(Z_DEFAULT_COMPRESSION),
        "DEFLATED" => encode_int(DEFLATED),
        "DEF_MEM_LEVEL" => encode_int(DEF_MEM_LEVEL),
        "MAX_WBITS" => encode_int(MAX_WBITS),
        "Z_DEFAULT_STRATEGY" => encode_int(i64::from(Z_DEFAULT_STRATEGY)),
        "Z_FILTERED" => encode_int(1),
        "Z_HUFFMAN_ONLY" => encode_int(2),
        "Z_RLE" => encode_int(3),
        "Z_FIXED" => encode_int(4),
        "Z_NO_FLUSH" => encode_int(i64::from(Z_NO_FLUSH)),
        "Z_PARTIAL_FLUSH" => encode_int(i64::from(Z_PARTIAL_FLUSH)),
        "Z_SYNC_FLUSH" => encode_int(i64::from(Z_SYNC_FLUSH)),
        "Z_FULL_FLUSH" => encode_int(i64::from(Z_FULL_FLUSH)),
        "Z_FINISH" => encode_int(i64::from(Z_FINISH)),
        "Z_BLOCK" => encode_int(i64::from(Z_BLOCK)),
        "Z_TREES" => encode_int(i64::from(Z_TREES)),
        "Z_OK" => encode_int(i64::from(Z_OK)),
        "Z_STREAM_END" => encode_int(i64::from(Z_STREAM_END)),
        "Z_NEED_DICT" => encode_int(i64::from(Z_NEED_DICT)),
        "Z_ERRNO" => encode_int(i64::from(Z_ERRNO)),
        "Z_STREAM_ERROR" => encode_int(i64::from(Z_STREAM_ERROR)),
        "Z_DATA_ERROR" => encode_int(i64::from(Z_DATA_ERROR)),
        "Z_MEM_ERROR" => encode_int(i64::from(Z_MEM_ERROR)),
        "Z_BUF_ERROR" => encode_int(i64::from(Z_BUF_ERROR)),
        "Z_VERSION_ERROR" => encode_int(i64::from(Z_VERSION_ERROR)),
        "ZLIB_VERSION" | "ZLIB_RUNTIME_VERSION" => {
            let version = if version_pointer().is_null() {
                String::new()
            } else {
                // SAFETY: libz returns a static NUL-terminated version string.
                unsafe { CStr::from_ptr(version_pointer()) }
                    .to_string_lossy()
                    .into_owned()
            };
            value::str_alloc(version)
        }
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'zlib' has no attribute '{name}'"),
        ),
    }
}

/// Dispatches `zlib.*` calls, including the native method bodies installed on
/// the `Compress` and `Decompress` classes.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "compress" => compress(args),
        "decompress" => decompress(args),
        "crc32" => crc32(args),
        "adler32" => adler32(args),
        "compressobj" => compressobj(args),
        "decompressobj" => decompressobj(args),
        // `Compress`
        "_zlib_compress" => compress_method(args),
        "_zlib_flush" => flush_method(args),
        "_zlib_copy" => copy_method(args),
        // `Decompress`
        "_zlib_decompress" => decompress_method(args),
        "_zlib_decompress_flush" => decompress_flush(args),
        "_zlib_decompress_copy" => decompress_copy(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'zlib' has no attribute '{function}'"),
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

    fn number(value_arg: i64) -> i64 {
        value::as_integer(value_arg).expect("integer result")
    }

    fn data(value_arg: i64) -> Vec<u8> {
        value::as_bytes(value_arg).expect("bytes result").to_vec()
    }

    fn call(function: &str, args: &[i64]) -> i64 {
        super::call(function, args)
    }

    #[test]
    fn checksums_match_the_reference() {
        exc::init();
        assert_eq!(number(call("crc32", &[bytes(b"hello world")])), 222_957_957);
        assert_eq!(
            number(call("adler32", &[bytes(b"hello world")])),
            436_929_629
        );
        assert_eq!(number(call("crc32", &[bytes(b"")])), 0);
        assert_eq!(number(call("adler32", &[bytes(b"")])), 1);
        assert!(!exc::pending());
    }

    #[test]
    fn compress_and_decompress_round_trip() {
        exc::init();
        let compressed = call("compress", &[bytes(b"hello world")]);
        assert_eq!(
            data(compressed),
            vec![120, 156, 203, 72, 205, 201, 201, 87, 40, 207, 47, 202, 73, 1, 0, 26, 11, 4, 93]
        );
        let plain = call("decompress", &[compressed]);
        assert_eq!(data(plain), b"hello world".to_vec());
        assert!(!exc::pending());
    }

    #[test]
    fn bad_parameters_are_reported() {
        exc::init();
        call("compress", &[bytes(b"x"), int(10)]);
        assert!(exc::pending());
        exc::clear();

        call("compressobj", &[int(10)]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();

        call("compressobj", &[int(6), int(8), int(99)]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();

        call("decompress", &[bytes(b"junk")]);
        assert!(exc::pending());
        exc::clear();

        call("decompress", &[bytes(b"")]);
        assert!(exc::pending());
        exc::clear();
    }

    #[test]
    fn compress_object_streams_and_copies() {
        exc::init();
        let obj = call("compressobj", &[int(6)]);
        assert!(!exc::pending());
        let first = call("_zlib_compress", &[obj, bytes(b"hello ")]);
        let second = call("_zlib_compress", &[obj, bytes(b"world")]);
        let tail = call("_zlib_flush", &[obj]);
        assert!(!exc::pending());
        assert_eq!(data(first), vec![120, 156]);
        assert!(data(second).is_empty());
        let mut whole = data(first);
        whole.extend(data(second));
        whole.extend(data(tail));
        assert_eq!(data(call("compress", &[bytes(b"hello world")])), whole);

        // A finished stream refuses further compression.
        call("_zlib_compress", &[obj, bytes(b"x")]);
        assert!(exc::pending());
        exc::clear();

        // `copy()` continues independently.
        let original = call("compressobj", &[int(6)]);
        let copy = call("_zlib_copy", &[original]);
        assert!(!exc::pending());
        let from_copy = call("_zlib_flush", &[copy]);
        let from_original = call("_zlib_flush", &[original]);
        assert_eq!(data(from_copy), data(from_original));
    }

    #[test]
    fn decompress_object_tracks_tail_and_eof() {
        exc::init();
        let obj = call("decompressobj", &[]);
        let compressed = call("compress", &[bytes(b"abcdef")]);
        let first = call("_zlib_decompress", &[obj, compressed, int(4)]);
        assert_eq!(data(first), b"abcd".to_vec());
        let tail = value::instance_attr(obj, "unconsumed_tail").expect("tail attribute");
        assert!(!data(tail).is_empty());
        let rest = call("_zlib_decompress", &[obj, bytes(b"")]);
        assert_eq!(data(rest), b"ef".to_vec());
        assert_eq!(value::instance_attr(obj, "eof"), Some(value::TRUE));

        let obj = call("decompressobj", &[]);
        let stream = call("compress", &[bytes(b"hi")]);
        let mut with_trailer = data(stream);
        with_trailer.extend_from_slice(b"TRAILER");
        let plain = call("_zlib_decompress", &[obj, value::bytes_new(with_trailer)]);
        assert_eq!(data(plain), b"hi".to_vec());
        let unused = value::instance_attr(obj, "unused_data").expect("unused attribute");
        assert_eq!(data(unused), b"TRAILER".to_vec());
    }

    #[test]
    fn raw_and_gzip_windows_work() {
        exc::init();
        let raw = call("compress", &[bytes(b"hello world"), int(6), int(-15)]);
        assert!(!exc::pending());
        assert_eq!(
            data(call("decompress", &[raw, int(-15)])),
            b"hello world".to_vec()
        );
        let gzip = call("compress", &[bytes(b"hello world"), int(6), int(31)]);
        assert!(!exc::pending());
        assert_eq!(
            data(call("decompress", &[gzip, int(47)])),
            b"hello world".to_vec()
        );
    }

    #[test]
    fn private_decompressor_class_constructs_and_streams() {
        exc::init();
        // `zlib._ZlibDecompressor` is a real class value.
        let class = constant("_ZlibDecompressor");
        assert_eq!(
            value::as_class(class)
                .map(|state| state.name.clone())
                .as_deref(),
            Some("_ZlibDecompressor")
        );
        assert!(value::as_class(constant("_ZlibCompressor")).is_some());
        assert_eq!(class, constant("_ZlibDecompressor"));

        // Build a raw-deflate payload, then instantiate the class the way
        // gzip does: `_ZlibDecompressor(wbits=-MAX_WBITS)`.
        let encoder = call("compressobj", &[int(6), int(8), int(-15)]);
        let body = call("_zlib_compress", &[encoder, bytes(b"hello world")]);
        let tail = call("_zlib_flush", &[encoder]);
        let mut payload = data(body);
        payload.extend_from_slice(&data(tail));
        assert!(!exc::pending());

        let instance = crate::builtins::call(class, 1, [int(-15)].as_ptr());
        assert!(!exc::pending());
        assert_eq!(value::type_name_of(instance), "_ZlibDecompressor");
        assert_eq!(
            value::instance_attr(instance, "needs_input"),
            Some(value::TRUE)
        );

        // A limited first call reports `needs_input = False`; the buffered
        // remainder is drained with an empty input, like gzip's reader.
        let first = call(
            "_zlib_decompress",
            &[instance, value::bytes_new(payload.clone()), int(5)],
        );
        assert_eq!(data(first), b"hello".to_vec());
        assert_eq!(
            value::instance_attr(instance, "needs_input"),
            Some(value::FALSE)
        );
        let second = call("_zlib_decompress", &[instance, bytes(b"")]);
        assert_eq!(data(second), b" world".to_vec());
        assert_eq!(
            value::instance_attr(instance, "needs_input"),
            Some(value::TRUE)
        );
        assert_eq!(value::instance_attr(instance, "eof"), Some(value::TRUE));
    }

    #[test]
    fn unknown_function_is_an_attribute_error() {
        exc::init();
        call("nope", &[]);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        exc::clear();
    }
}
