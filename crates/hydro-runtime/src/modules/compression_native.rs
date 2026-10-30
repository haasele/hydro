//! `_bz2` and `_lzma`: the compiled low-level codecs behind the pure-Python
//! `bz2` and `lzma` modules.
//!
//! Both codecs are the real system libraries, linked through `-lbz2` and
//! `-llzma` by the `#[link]` attributes below: the bytes this module produces
//! are byte-for-byte what CPython 3.14.9's `_bz2`/`_lzma` produce for the same
//! parameters. Nothing is reimplemented and no stream is synthesised.
//!
//! ## Surface
//!
//! * `_bz2.BZ2Compressor(compresslevel=9)` with `compress(data)`/`flush()`;
//!   `_bz2.BZ2Decompressor()` with `decompress(data, max_length=-1)` and the
//!   `eof`/`unused_data`/`needs_input` members, plus libbzip2's error mapping
//!   (`OSError`, `EOFError`, `ValueError`, `RuntimeError`, `MemoryError`).
//! * `_lzma.LZMACompressor(format=FORMAT_XZ, check=-1, preset=None,
//!   filters=None)` and `_lzma.LZMADecompressor(format=FORMAT_AUTO,
//!   memlimit=None, filters=None)`, the `LZMAError` class, `is_check_supported`,
//!   `_encode_filter_properties`/`_decode_filter_properties`, and every
//!   `FORMAT_*`/`CHECK_*`/`FILTER_*`/`MF_*`/`MODE_*`/`PRESET_*` constant with
//!   CPython's argument validation and messages.
//!
//! The pure `bz2.py`/`lzma.py` modules (CPython's `Lib` tree) sit on top:
//! `BZ2File`/`LZMAFile` and the one-shot `compress`/`decompress` helpers are
//! their code, driving these classes through `compression._common._streams`.
//! Nothing here shadows those module names, so `bz2`/`lzma` must resolve to
//! the pure-Python sources.
//!
//! ## Native state
//!
//! Instances carry an integer handle in a private attribute; the actual
//! `bz_stream`/`lzma_stream` lives in a process-wide registry guarded by a
//! mutex. `lzma_end`/`BZ2_bz*End` run when a registry entry is dropped. Hydro
//! has no finaliser hook for native instances, so a compressor abandoned
//! without `flush()` keeps its library allocation until process exit, and a
//! stream is only released when its registry entry dies (process exit).
//!
//! ## Failure policy
//!
//! Bad argument types raise `TypeError`, rejected options raise `ValueError`,
//! libbzip2 failures raise `OSError`/`EOFError`/`RuntimeError` with CPython's
//! wording and liblzma failures raise `LZMAError`. No input can panic.

use std::collections::HashMap;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::ptr;
use std::sync::{LazyLock, Mutex, MutexGuard};

use num_bigint::BigInt;

use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, EOF_ERROR, EXCEPTION, MEMORY_ERROR, OS_ERROR, OVERFLOW_ERROR,
    RUNTIME_ERROR, SYSTEM_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::value::{self, NONE};

// -------------------------------------------------------------------------
// libbzip2 constants (`bzlib.h`)
// -------------------------------------------------------------------------

const BZ_RUN: c_int = 0;
const BZ_FINISH: c_int = 2;

const BZ_OK: c_int = 0;
const BZ_RUN_OK: c_int = 1;
const BZ_FLUSH_OK: c_int = 2;
const BZ_FINISH_OK: c_int = 3;
const BZ_STREAM_END: c_int = 4;
const BZ_SEQUENCE_ERROR: c_int = -1;
const BZ_PARAM_ERROR: c_int = -2;
const BZ_MEM_ERROR: c_int = -3;
const BZ_DATA_ERROR: c_int = -4;
const BZ_DATA_ERROR_MAGIC: c_int = -5;
const BZ_IO_ERROR: c_int = -6;
const BZ_UNEXPECTED_EOF: c_int = -7;
const BZ_CONFIG_ERROR: c_int = -9;

// -------------------------------------------------------------------------
// liblzma constants (`base.h`, `check.h`, `container.h`, `lzma12.h`,
// `filter.h`, `delta.h`, `bcj.h`)
// -------------------------------------------------------------------------

const LZMA_RUN: c_int = 0;
const LZMA_FINISH: c_int = 3;

const LZMA_OK: c_int = 0;
const LZMA_STREAM_END: c_int = 1;
const LZMA_NO_CHECK: c_int = 2;
const LZMA_UNSUPPORTED_CHECK: c_int = 3;
const LZMA_GET_CHECK: c_int = 4;
const LZMA_MEM_ERROR: c_int = 5;
const LZMA_MEMLIMIT_ERROR: c_int = 6;
const LZMA_FORMAT_ERROR: c_int = 7;
const LZMA_OPTIONS_ERROR: c_int = 8;
const LZMA_DATA_ERROR: c_int = 9;
const LZMA_BUF_ERROR: c_int = 10;
const LZMA_PROG_ERROR: c_int = 11;

const FORMAT_AUTO: i64 = 0;
const FORMAT_XZ: i64 = 1;
const FORMAT_ALONE: i64 = 2;
const FORMAT_RAW: i64 = 3;

const CHECK_NONE: c_int = 0;
const CHECK_CRC32: c_int = 1;
const CHECK_CRC64: c_int = 4;
const CHECK_SHA256: c_int = 10;
const CHECK_ID_MAX: c_int = 15;
/// `_lzma`'s `LZMA_CHECK_UNKNOWN`: no check has been decoded yet.
const CHECK_UNKNOWN: c_int = CHECK_ID_MAX + 1;

const FILTER_LZMA1: u64 = 0x4000_0000_0000_0001;
const FILTER_LZMA2: u64 = 0x21;
const FILTER_DELTA: u64 = 0x03;
const FILTER_X86: u64 = 0x04;
const FILTER_POWERPC: u64 = 0x05;
const FILTER_IA64: u64 = 0x06;
const FILTER_ARM: u64 = 0x07;
const FILTER_ARMTHUMB: u64 = 0x08;
const FILTER_SPARC: u64 = 0x09;

const MF_HC3: i64 = 0x03;
const MF_HC4: i64 = 0x04;
const MF_BT2: i64 = 0x12;
const MF_BT3: i64 = 0x13;
const MF_BT4: i64 = 0x14;
const MODE_FAST: i64 = 1;
const MODE_NORMAL: i64 = 2;
const PRESET_DEFAULT: u32 = 6;
const PRESET_EXTREME: i64 = 1_i64 << 31;

/// `LZMA_FILTERS_MAX`.
const FILTERS_MAX: usize = 4;
/// `LZMA_VLI_UNKNOWN`: the filter-chain terminator.
const LZMA_VLI_UNKNOWN: u64 = u64::MAX;
/// `LZMA_TELL_NO_CHECK | LZMA_TELL_ANY_CHECK`, the decoder flags CPython sets.
const DECODER_FLAGS: u32 = 0x01 | 0x04;

/// `lzma_delta_type` member: only `LZMA_DELTA_TYPE_BYTE` exists.
const DELTA_TYPE_BYTE: c_int = 0;

/// Output chunk produced per `bz*`/`lzma_code` iteration.
const OUT_CHUNK: usize = 65_536;

/// Private instance attribute holding the registry handle.
const STREAM_ATTR: &str = "_codec_stream";

// -------------------------------------------------------------------------
// libbzip2 FFI
// -------------------------------------------------------------------------

type BzAllocFunc = unsafe extern "C" fn(*mut c_void, c_int, c_int) -> *mut c_void;
type BzFreeFunc = unsafe extern "C" fn(*mut c_void, *mut c_void);

/// Mirrors `struct bz_stream` from `bzlib.h`.
#[repr(C)]
struct BzStream {
    next_in: *mut c_char,
    avail_in: c_uint,
    total_in_lo32: c_uint,
    total_in_hi32: c_uint,
    next_out: *mut c_char,
    avail_out: c_uint,
    total_out_lo32: c_uint,
    total_out_hi32: c_uint,
    state: *mut c_void,
    bzalloc: Option<BzAllocFunc>,
    bzfree: Option<BzFreeFunc>,
    opaque: *mut c_void,
}

#[link(name = "bz2")]
extern "C" {
    fn BZ2_bzCompressInit(
        strm: *mut BzStream,
        block_size_100k: c_int,
        verbosity: c_int,
        work_factor: c_int,
    ) -> c_int;
    fn BZ2_bzCompress(strm: *mut BzStream, action: c_int) -> c_int;
    fn BZ2_bzCompressEnd(strm: *mut BzStream) -> c_int;
    fn BZ2_bzDecompressInit(strm: *mut BzStream, verbosity: c_int, small: c_int) -> c_int;
    fn BZ2_bzDecompress(strm: *mut BzStream) -> c_int;
    fn BZ2_bzDecompressEnd(strm: *mut BzStream) -> c_int;
}

// -------------------------------------------------------------------------
// liblzma FFI
// -------------------------------------------------------------------------

/// Mirrors `struct lzma_filter` from `lzma/filter.h`. The value is a plain
/// descriptor (id + borrowed option pointer), so it copies freely; the option
/// object itself stays owned by `FilterChain`.
#[repr(C)]
#[derive(Clone, Copy)]
struct LzmaFilter {
    id: u64,
    options: *mut c_void,
}

/// Mirrors `lzma_options_lzma` from `lzma/lzma12.h` (liblzma 5.8). The
/// reserved tail keeps the size identical to the C struct, which is what
/// `lzma_lzma_preset` and every encoder expect.
#[repr(C)]
struct LzmaOptionsLzma {
    dict_size: u32,
    preset_dict: *const u8,
    preset_dict_size: u32,
    lc: u32,
    lp: u32,
    pb: u32,
    mode: c_int,
    nice_len: u32,
    mf: c_int,
    depth: u32,
    ext_flags: u32,
    ext_size_low: u32,
    ext_size_high: u32,
    reserved_int4: u32,
    reserved_int5: u32,
    reserved_int6: u32,
    reserved_int7: u32,
    reserved_int8: u32,
    reserved_enum1: c_int,
    reserved_enum2: c_int,
    reserved_enum3: c_int,
    reserved_enum4: c_int,
    reserved_ptr1: *mut c_void,
    reserved_ptr2: *mut c_void,
}

/// Mirrors `lzma_options_delta` from `lzma/delta.h`.
#[repr(C)]
struct LzmaOptionsDelta {
    type_: c_int,
    dist: u32,
    reserved_int1: u32,
    reserved_int2: u32,
    reserved_int3: u32,
    reserved_int4: u32,
    reserved_ptr1: *mut c_void,
    reserved_ptr2: *mut c_void,
}

/// Mirrors `lzma_options_bcj` from `lzma/bcj.h` (liblzma 5.8: one field).
#[repr(C)]
struct LzmaOptionsBcj {
    start_offset: u32,
}

/// Mirrors `lzma_stream` from `lzma/base.h`. A zeroed value is
/// `LZMA_STREAM_INIT`.
#[repr(C)]
struct LzmaStream {
    next_in: *const u8,
    avail_in: usize,
    total_in: u64,
    next_out: *mut u8,
    avail_out: usize,
    total_out: u64,
    allocator: *const c_void,
    internal: *mut c_void,
    reserved_ptr1: *mut c_void,
    reserved_ptr2: *mut c_void,
    reserved_ptr3: *mut c_void,
    reserved_ptr4: *mut c_void,
    seek_pos: u64,
    reserved_int2: u64,
    reserved_int3: usize,
    reserved_int4: usize,
    reserved_enum1: c_int,
    reserved_enum2: c_int,
}

#[link(name = "lzma")]
extern "C" {
    fn lzma_easy_encoder(strm: *mut LzmaStream, preset: u32, check: c_int) -> c_int;
    fn lzma_stream_encoder(
        strm: *mut LzmaStream,
        filters: *const LzmaFilter,
        check: c_int,
    ) -> c_int;
    fn lzma_alone_encoder(strm: *mut LzmaStream, options: *const LzmaOptionsLzma) -> c_int;
    fn lzma_raw_encoder(strm: *mut LzmaStream, filters: *const LzmaFilter) -> c_int;
    fn lzma_auto_decoder(strm: *mut LzmaStream, memlimit: u64, flags: u32) -> c_int;
    fn lzma_stream_decoder(strm: *mut LzmaStream, memlimit: u64, flags: u32) -> c_int;
    fn lzma_alone_decoder(strm: *mut LzmaStream, memlimit: u64) -> c_int;
    fn lzma_raw_decoder(strm: *mut LzmaStream, filters: *const LzmaFilter) -> c_int;
    fn lzma_code(strm: *mut LzmaStream, action: c_int) -> c_int;
    fn lzma_end(strm: *mut LzmaStream);
    fn lzma_get_check(strm: *const LzmaStream) -> c_int;
    fn lzma_check_is_supported(check: c_int) -> u8;
    fn lzma_lzma_preset(options: *mut LzmaOptionsLzma, preset: u32) -> u8;
    fn lzma_properties_size(size: *mut u32, filter: *const LzmaFilter) -> c_int;
    fn lzma_properties_encode(filter: *const LzmaFilter, props: *mut u8) -> c_int;
    fn lzma_properties_decode(
        filter: *mut LzmaFilter,
        allocator: *const c_void,
        props: *const u8,
        props_size: usize,
    ) -> c_int;
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

fn overflow_error(message: &str) -> i64 {
    exc::raise_with_message(OVERFLOW_ERROR, message)
}

fn os_error(message: &str) -> i64 {
    exc::raise_with_message(OS_ERROR, message)
}

fn eof_error(message: &str) -> i64 {
    exc::raise_with_message(EOF_ERROR, message)
}

fn memory_error(message: &str) -> i64 {
    exc::raise_with_message(MEMORY_ERROR, message)
}

/// An unrepresentable registry handle: only user tampering can produce it.
fn inconsistent_state() -> i64 {
    exc::raise_with_message(RUNTIME_ERROR, "inconsistent codec state")
}

/// Reads a positional slot; omitted optional slots arrive as `None`.
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

/// `check`-style C `int` conversion, with CPython's overflow wording.
fn c_int_argument(value_arg: i64, name: &str, default: i64) -> Result<c_int, i64> {
    let n = int_argument(value_arg, name, default)?;
    c_int::try_from(n).map_err(|_| {
        overflow_error(if n < 0 {
            "signed integer is less than minimum"
        } else {
            "signed integer is greater than maximum"
        })
    })
}

/// `uint32_t` conversion (`_PyLong_UInt32_Converter`).
fn u32_argument(value_arg: i64, name: &str) -> Result<u32, i64> {
    let Some(n) = value::as_integer(value_arg) else {
        return Err(type_error(&format!(
            "argument '{name}' must be an integer, not '{}'",
            value::type_name(value_arg)
        )));
    };
    if n < 0 {
        return Err(value_error("Cannot convert negative int"));
    }
    u32::try_from(n).map_err(|_| overflow_error("Python int too large for C unsigned int"))
}

/// `uint64_t` conversion (`_PyLong_UInt64_Converter`).
fn u64_argument(value_arg: i64, name: &str) -> Result<u64, i64> {
    let Some(n) = value::as_integer(value_arg) else {
        return Err(type_error(&format!(
            "argument '{name}' must be an integer, not '{}'",
            value::type_name(value_arg)
        )));
    };
    if n < 0 {
        return Err(value_error("Cannot convert negative int"));
    }
    Ok(n as u64)
}

/// Any bytes-like object (`bytes`, `bytearray`, `memoryview`).
fn bytes_argument(value_arg: i64, name: &str) -> Result<Vec<u8>, i64> {
    if let Some(bytes) = value::as_bytes(value_arg) {
        return Ok(bytes.to_vec());
    }
    if let Some(bytes) = value::as_bytearray(value_arg) {
        return Ok(bytes.clone());
    }
    if let Some(view) = value::as_memoryview(value_arg) {
        return value::memoryview_bytes(view).map_err(|_| {
            type_error(&format!(
                "argument '{name}' is not a contiguous byte buffer"
            ))
        });
    }
    Err(type_error(&format!(
        "argument '{name}' must be a bytes-like object, not '{}'",
        value::type_name(value_arg)
    )))
}

/// Encodes an integer constant, promoting to `BigInt` beyond the immediate
/// range (`FILTER_LZMA1` needs 63 bits).
fn int_constant(n: i64) -> i64 {
    value::encode_int(n).unwrap_or_else(|| value::bigint_new(BigInt::from(n)))
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

// -------------------------------------------------------------------------
// libbzip2 error mapping
// -------------------------------------------------------------------------

/// Maps a libbzip2 status to CPython's `_bz2` exception, raising on failure.
fn catch_bz2(status: c_int) -> Result<(), i64> {
    match status {
        BZ_OK | BZ_RUN_OK | BZ_FLUSH_OK | BZ_FINISH_OK | BZ_STREAM_END => Ok(()),
        BZ_CONFIG_ERROR => Err(exc::raise_with_message(
            SYSTEM_ERROR,
            "libbzip2 was not compiled correctly",
        )),
        BZ_PARAM_ERROR => Err(value_error(
            "Internal error - invalid parameters passed to libbzip2",
        )),
        BZ_MEM_ERROR => Err(memory_error("")),
        BZ_DATA_ERROR | BZ_DATA_ERROR_MAGIC => Err(os_error("Invalid data stream")),
        BZ_IO_ERROR => Err(os_error("Unknown I/O error")),
        BZ_UNEXPECTED_EOF => Err(eof_error(
            "Compressed file ended before the logical end-of-stream was detected",
        )),
        BZ_SEQUENCE_ERROR => Err(exc::raise_with_message(
            RUNTIME_ERROR,
            "Internal error - Invalid sequence of commands sent to libbzip2",
        )),
        other => Err(os_error(&format!(
            "Unrecognized error from libbzip2: {other}"
        ))),
    }
}

// -------------------------------------------------------------------------
// liblzma error mapping
// -------------------------------------------------------------------------

fn lzma_error_class() -> i64 {
    cached_class("_lzma.LZMAError", || {
        value::class_new(
            "LZMAError".to_string(),
            exc::exception_class(EXCEPTION),
            Vec::new(),
        )
    })
}

/// Raises `LZMAError(message)` and returns the `None` dummy value.
fn lzma_error(message: &str) -> i64 {
    let message_value = value::str_alloc(message.to_string());
    let class = lzma_error_class();
    let instance = crate::builtins::call(class, 1, [message_value].as_ptr());
    if exc::pending() {
        return NONE;
    }
    exc::raise_value(instance)
}

/// Maps a liblzma status to CPython's `_lzma` exception, raising on failure.
fn catch_lzma(status: c_int) -> Result<(), i64> {
    match status {
        LZMA_OK | LZMA_GET_CHECK | LZMA_NO_CHECK | LZMA_STREAM_END => Ok(()),
        LZMA_UNSUPPORTED_CHECK => Err(lzma_error("Unsupported integrity check")),
        LZMA_MEM_ERROR => Err(memory_error("")),
        LZMA_MEMLIMIT_ERROR => Err(lzma_error("Memory usage limit exceeded")),
        LZMA_FORMAT_ERROR => Err(lzma_error("Input format not supported by decoder")),
        LZMA_OPTIONS_ERROR => Err(lzma_error("Invalid or unsupported options")),
        LZMA_DATA_ERROR => Err(lzma_error("Corrupt input data")),
        LZMA_BUF_ERROR => Err(lzma_error("Insufficient buffer space")),
        LZMA_PROG_ERROR => Err(lzma_error("Internal error")),
        other => Err(lzma_error(&format!(
            "Unrecognized error from liblzma: {other}"
        ))),
    }
}

// -------------------------------------------------------------------------
// Stream registry
// -------------------------------------------------------------------------

struct Bz2CompressorState {
    stream: Box<BzStream>,
    flushed: bool,
}

impl Drop for Bz2CompressorState {
    fn drop(&mut self) {
        if !self.stream.state.is_null() {
            // SAFETY: `state` is a live compressor owned by this value.
            unsafe { BZ2_bzCompressEnd(&mut *self.stream) };
            self.stream.state = ptr::null_mut();
        }
    }
}

struct Bz2DecompressorState {
    stream: Box<BzStream>,
    eof: bool,
    needs_input: bool,
    failed: bool,
    unused: Vec<u8>,
    /// Input libbzip2 has not consumed yet (CPython's `input_buffer`).
    input: Vec<u8>,
}

impl Drop for Bz2DecompressorState {
    fn drop(&mut self) {
        if !self.stream.state.is_null() {
            // SAFETY: `state` is a live decompressor owned by this value.
            unsafe { BZ2_bzDecompressEnd(&mut *self.stream) };
            self.stream.state = ptr::null_mut();
        }
    }
}

struct LzmaCompressorState {
    stream: Box<LzmaStream>,
    flushed: bool,
}

impl Drop for LzmaCompressorState {
    fn drop(&mut self) {
        // SAFETY: `lzma_end` accepts any initialized `lzma_stream`; a zeroed
        // one has a null `internal` pointer and is a no-op.
        unsafe { lzma_end(&mut *self.stream) };
    }
}

struct LzmaDecompressorState {
    stream: Box<LzmaStream>,
    check: c_int,
    eof: bool,
    needs_input: bool,
    unused: Vec<u8>,
    /// Input `lzma_code` has not consumed yet.
    input: Vec<u8>,
}

impl Drop for LzmaDecompressorState {
    fn drop(&mut self) {
        // SAFETY: `lzma_end` accepts any initialized `lzma_stream`.
        unsafe { lzma_end(&mut *self.stream) };
    }
}

enum Codec {
    Bz2Compressor(Bz2CompressorState),
    Bz2Decompressor(Bz2DecompressorState),
    LzmaCompressor(LzmaCompressorState),
    LzmaDecompressor(LzmaDecompressorState),
}

// SAFETY: an entry is only touched while the registry mutex is held, so the
// raw library pointers never cross threads unsynchronised.
unsafe impl Send for Codec {}

struct Registry {
    next_handle: u64,
    codecs: HashMap<u64, Codec>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| {
    Mutex::new(Registry {
        next_handle: 1,
        codecs: HashMap::new(),
    })
});

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Stores `codec` and returns its handle.
fn register(codec: Codec) -> u64 {
    let mut registry = registry();
    let handle = registry.next_handle;
    registry.next_handle = handle.wrapping_add(1).max(1);
    registry.codecs.insert(handle, codec);
    handle
}

/// Reads the registry handle stored on a codec instance.
fn handle_of(instance: i64) -> Result<u64, i64> {
    match value::instance_attr(instance, STREAM_ATTR).and_then(value::as_integer) {
        Some(handle) if handle > 0 => Ok(handle as u64),
        _ => Err(inconsistent_state()),
    }
}

/// Attaches `handle` to a freshly created instance.
fn attach_handle(instance: i64, handle: u64) {
    value::instance_setattr(
        instance,
        STREAM_ATTR.to_string(),
        int_constant(handle as i64),
    );
}

// -------------------------------------------------------------------------
// Decompressor attributes
// -------------------------------------------------------------------------

/// Publishes `BZ2Decompressor`'s read-only members. The new bytes object is
/// rooted across the attribute stores, which may allocate.
fn store_bz2_attributes(instance: i64, eof: bool, unused: Vec<u8>, needs_input: bool) {
    let unused = value::bytes_new(unused);
    let roots = heap::root_values(&[unused]);
    value::instance_setattr(instance, "eof".to_string(), value::bool_value(eof));
    value::instance_setattr(instance, "unused_data".to_string(), unused);
    value::instance_setattr(
        instance,
        "needs_input".to_string(),
        value::bool_value(needs_input),
    );
    heap::release_roots(roots);
}

/// Publishes `LZMADecompressor`'s read-only members, including `check`.
fn store_lzma_attributes(
    instance: i64,
    check: c_int,
    eof: bool,
    unused: Vec<u8>,
    needs_input: bool,
) {
    let unused = value::bytes_new(unused);
    let roots = heap::root_values(&[unused]);
    value::instance_setattr(instance, "check".to_string(), int_constant(check as i64));
    value::instance_setattr(instance, "eof".to_string(), value::bool_value(eof));
    value::instance_setattr(instance, "unused_data".to_string(), unused);
    value::instance_setattr(
        instance,
        "needs_input".to_string(),
        value::bool_value(needs_input),
    );
    heap::release_roots(roots);
}

// -------------------------------------------------------------------------
// libbzip2 streams
// -------------------------------------------------------------------------

fn new_bz_stream() -> Box<BzStream> {
    // SAFETY: a zeroed `bz_stream` is the documented "uninitialised" state;
    // null `bzalloc`/`bzfree` select libbzip2's default allocator.
    Box::new(unsafe { std::mem::zeroed() })
}

fn new_lzma_stream() -> Box<LzmaStream> {
    // SAFETY: a zeroed `lzma_stream` is `LZMA_STREAM_INIT`.
    Box::new(unsafe { std::mem::zeroed() })
}

/// Feeds `input` to a compressor and appends every produced byte to `out`.
///
/// In `BZ_RUN` mode the loop stops once the input is consumed (libbzip2 keeps
/// the rest buffered, exactly like CPython). In `BZ_FINISH` mode it runs until
/// `BZ_STREAM_END`.
fn bz2_compress_pump(
    state: &mut Bz2CompressorState,
    mut input: &[u8],
    action: c_int,
    out: &mut Vec<u8>,
) -> Result<(), i64> {
    loop {
        if state.stream.avail_in == 0 && !input.is_empty() {
            let take = input.len().min(u32::MAX as usize);
            state.stream.next_in = input.as_ptr() as *mut c_char;
            state.stream.avail_in = take as c_uint;
            input = &input[take..];
        }
        if action == BZ_RUN && state.stream.avail_in == 0 {
            break;
        }
        let mut buffer = vec![0u8; OUT_CHUNK];
        state.stream.next_out = buffer.as_mut_ptr() as *mut c_char;
        state.stream.avail_out = OUT_CHUNK as c_uint;
        // SAFETY: the stream is initialised and both buffers are live for the
        // duration of the call.
        let status = unsafe { BZ2_bzCompress(&mut *state.stream, action) };
        let produced = OUT_CHUNK - state.stream.avail_out as usize;
        if produced > 0 {
            out.extend_from_slice(&buffer[..produced]);
        }
        catch_bz2(status)?;
        if action == BZ_FINISH && status == BZ_STREAM_END {
            break;
        }
        // Guarantee termination if libbzip2 ever stalls with output room.
        if produced == 0 && state.stream.avail_out > 0 && state.stream.avail_in == 0 {
            break;
        }
    }
    state.stream.next_in = ptr::null_mut();
    state.stream.avail_in = 0;
    Ok(())
}

/// Decompresses the buffered input, honouring `max_length`, and refreshes
/// `eof`/`needs_input`/`unused` exactly like `_bz2`'s `decompress()`.
fn bz2_decompress_pump(
    state: &mut Bz2DecompressorState,
    max_length: i64,
    out: &mut Vec<u8>,
) -> Result<(), i64> {
    let mut cursor = 0usize;
    loop {
        if max_length >= 0 && out.len() as i64 >= max_length {
            break;
        }
        if state.stream.avail_in == 0 && cursor < state.input.len() {
            let take = (state.input.len() - cursor).min(u32::MAX as usize);
            let start = state.input[cursor..].as_ptr() as *mut c_char;
            state.stream.next_in = start;
            state.stream.avail_in = take as c_uint;
            cursor += take;
        }
        let want = if max_length >= 0 {
            ((max_length - out.len() as i64) as usize).min(OUT_CHUNK)
        } else {
            OUT_CHUNK
        };
        if want == 0 {
            break;
        }
        let mut buffer = vec![0u8; want];
        state.stream.next_out = buffer.as_mut_ptr() as *mut c_char;
        state.stream.avail_out = want as c_uint;
        // SAFETY: the stream is initialised and both buffers are live.
        let status = unsafe { BZ2_bzDecompress(&mut *state.stream) };
        let produced = want - state.stream.avail_out as usize;
        if produced > 0 {
            out.extend_from_slice(&buffer[..produced]);
        }
        if status == BZ_STREAM_END {
            state.eof = true;
            break;
        }
        if let Err(error) = catch_bz2(status) {
            // A decompressor that saw an error cannot be fed again; libbzip2
            // may otherwise write out of bounds.
            state.needs_input = false;
            state.stream.avail_in = 0;
            state.stream.next_in = ptr::null_mut();
            state.failed = true;
            return Err(error);
        }
        if state.stream.avail_in == 0 && cursor >= state.input.len() {
            break;
        }
        if produced == 0 && state.stream.avail_out > 0 {
            break;
        }
    }
    let consumed = cursor.saturating_sub(state.stream.avail_in as usize);
    let leftover = state.input[consumed..].to_vec();
    state.stream.avail_in = 0;
    state.stream.next_in = ptr::null_mut();
    if state.eof {
        if !leftover.is_empty() {
            state.unused = leftover;
        }
        state.input.clear();
        state.needs_input = false;
    } else {
        state.needs_input = leftover.is_empty();
        state.input = leftover;
    }
    Ok(())
}

// -------------------------------------------------------------------------
// liblzma compressor / decompressor pumps
// -------------------------------------------------------------------------

fn lzma_compress_pump(
    state: &mut LzmaCompressorState,
    input: &[u8],
    action: c_int,
    out: &mut Vec<u8>,
) -> Result<(), i64> {
    state.stream.next_in = input.as_ptr();
    state.stream.avail_in = input.len();
    loop {
        let mut buffer = vec![0u8; OUT_CHUNK];
        state.stream.next_out = buffer.as_mut_ptr();
        state.stream.avail_out = OUT_CHUNK;
        // SAFETY: the stream is initialised and both buffers are live.
        let status = unsafe { lzma_code(&mut *state.stream, action) };
        let produced = OUT_CHUNK - state.stream.avail_out;
        if produced > 0 {
            out.extend_from_slice(&buffer[..produced]);
        }
        // An empty finishing flush may report a buffer error that CPython
        // treats as success.
        let status = if status == LZMA_BUF_ERROR && input.is_empty() && state.stream.avail_out > 0 {
            LZMA_OK
        } else {
            status
        };
        catch_lzma(status)?;
        if (action == LZMA_RUN && state.stream.avail_in == 0)
            || (action == LZMA_FINISH && status == LZMA_STREAM_END)
        {
            break;
        }
        // Guarantee termination if liblzma ever stalls with output room.
        if produced == 0 && state.stream.avail_out > 0 && state.stream.avail_in == 0 {
            break;
        }
    }
    state.stream.next_in = ptr::null();
    state.stream.avail_in = 0;
    Ok(())
}

fn lzma_decompress_pump(
    state: &mut LzmaDecompressorState,
    max_length: i64,
    out: &mut Vec<u8>,
) -> Result<(), i64> {
    let mut cursor = 0usize;
    loop {
        if max_length >= 0 && out.len() as i64 >= max_length {
            break;
        }
        if state.stream.avail_in == 0 && cursor < state.input.len() {
            // The pointer stays valid until `state.input` is replaced, which
            // only happens after liblzma no longer holds it.
            state.stream.next_in = state.input.as_ptr().wrapping_add(cursor);
            state.stream.avail_in = state.input.len() - cursor;
            cursor = state.input.len();
        }
        let want = if max_length >= 0 {
            ((max_length - out.len() as i64) as usize).min(OUT_CHUNK)
        } else {
            OUT_CHUNK
        };
        if want == 0 {
            break;
        }
        let mut buffer = vec![0u8; want];
        state.stream.next_out = buffer.as_mut_ptr();
        state.stream.avail_out = want;
        // SAFETY: the stream is initialised and both buffers are live.
        let status = unsafe { lzma_code(&mut *state.stream, LZMA_RUN) };
        let produced = want - state.stream.avail_out;
        if produced > 0 {
            out.extend_from_slice(&buffer[..produced]);
        }
        let status =
            if status == LZMA_BUF_ERROR && state.stream.avail_in == 0 && state.stream.avail_out > 0
            {
                LZMA_OK
            } else {
                status
            };
        catch_lzma(status)?;
        if status == LZMA_GET_CHECK || status == LZMA_NO_CHECK {
            // SAFETY: `lzma_get_check` only reads the decoder's own state.
            state.check = unsafe { lzma_get_check(&*state.stream) };
        }
        if status == LZMA_STREAM_END {
            state.eof = true;
            break;
        }
        if state.stream.avail_out == 0 {
            continue;
        }
        if state.stream.avail_in == 0 && cursor >= state.input.len() {
            break;
        }
        // `LZMA_GET_CHECK`/`LZMA_NO_CHECK` are header-only warnings: the next
        // call consumes the pending input, so they are not a stall.
        if produced == 0 && status != LZMA_GET_CHECK && status != LZMA_NO_CHECK {
            break;
        }
    }
    let consumed = cursor.saturating_sub(state.stream.avail_in);
    let leftover = state.input[consumed..].to_vec();
    state.stream.avail_in = 0;
    state.stream.next_in = ptr::null();
    let limited = max_length >= 0 && out.len() as i64 >= max_length;
    if state.eof {
        if !leftover.is_empty() {
            state.unused = leftover;
        }
        state.input.clear();
        state.needs_input = false;
    } else {
        state.needs_input = leftover.is_empty() && !limited;
        state.input = leftover;
    }
    Ok(())
}

// -------------------------------------------------------------------------
// Filter chain parsing (`_lzma`'s dict specs)
// -------------------------------------------------------------------------

/// Owns the option objects a `lzma_filter` chain points at until the encoder
/// or decoder has copied them.
struct FilterChain {
    filters: Vec<LzmaFilter>,
    options: Vec<FilterOptions>,
}

enum FilterOptions {
    Lzma(Box<LzmaOptionsLzma>),
    Delta(Box<LzmaOptionsDelta>),
    Bcj(Box<LzmaOptionsBcj>),
}

impl FilterOptions {
    /// Address of the option object for `lzma_filter.options`. The box keeps
    /// the allocation alive, so moving the `FilterOptions` does not move it.
    fn as_ptr(&self) -> *mut c_void {
        match self {
            FilterOptions::Lzma(options) => {
                options.as_ref() as *const LzmaOptionsLzma as *mut c_void
            }
            FilterOptions::Delta(options) => {
                options.as_ref() as *const LzmaOptionsDelta as *mut c_void
            }
            FilterOptions::Bcj(options) => options.as_ref() as *const LzmaOptionsBcj as *mut c_void,
        }
    }
}

fn dict_lookup(pairs: &[(i64, i64)], name: &str) -> Option<i64> {
    pairs
        .iter()
        .find(|(key, _)| value::as_str(*key) == Some(name))
        .map(|(_, item)| *item)
}

/// Parses one filter dict, mirroring `parse_filter_spec_*` in `_lzmamodule.c`.
fn parse_filter_spec(spec: i64) -> Result<FilterChain, i64> {
    let Some(pairs) = value::as_dict(spec) else {
        return Err(type_error(
            "Filter specifier must be a dict or dict-like object",
        ));
    };
    let Some(id_value) = dict_lookup(pairs, "id") else {
        return Err(value_error("Filter specifier must have an \"id\" entry"));
    };
    let id = u64_argument(id_value, "id")?;
    let mut chain = FilterChain {
        filters: Vec::new(),
        options: Vec::new(),
    };
    match id {
        FILTER_LZMA1 | FILTER_LZMA2 => {
            let options = parse_lzma_options(pairs)?;
            chain.options.push(FilterOptions::Lzma(options));
            let options_ptr = chain.options.last().expect("just pushed").as_ptr();
            chain.filters.push(LzmaFilter {
                id,
                options: options_ptr,
            });
        }
        FILTER_DELTA => {
            let mut options = Box::new(LzmaOptionsDelta {
                type_: DELTA_TYPE_BYTE,
                dist: 1,
                reserved_int1: 0,
                reserved_int2: 0,
                reserved_int3: 0,
                reserved_int4: 0,
                reserved_ptr1: ptr::null_mut(),
                reserved_ptr2: ptr::null_mut(),
            });
            if let Some(dist) = dict_lookup(pairs, "dist") {
                options.dist = u32_argument(dist, "dist")
                    .map_err(|_| value_error("Invalid filter specifier for delta filter"))?;
            }
            for (key, _) in pairs {
                match value::as_str(*key) {
                    Some("id" | "dist") => {}
                    _ => return Err(value_error("Invalid filter specifier for delta filter")),
                }
            }
            chain.options.push(FilterOptions::Delta(options));
            let options_ptr = chain.options.last().expect("just pushed").as_ptr();
            chain.filters.push(LzmaFilter {
                id,
                options: options_ptr,
            });
        }
        FILTER_X86 | FILTER_POWERPC | FILTER_IA64 | FILTER_ARM | FILTER_ARMTHUMB | FILTER_SPARC => {
            let mut options = Box::new(LzmaOptionsBcj { start_offset: 0 });
            if let Some(offset) = dict_lookup(pairs, "start_offset") {
                options.start_offset = u32_argument(offset, "start_offset")
                    .map_err(|_| value_error("Invalid filter specifier for BCJ filter"))?;
            }
            for (key, _) in pairs {
                match value::as_str(*key) {
                    Some("id" | "start_offset") => {}
                    _ => return Err(value_error("Invalid filter specifier for BCJ filter")),
                }
            }
            chain.options.push(FilterOptions::Bcj(options));
            let options_ptr = chain.options.last().expect("just pushed").as_ptr();
            chain.filters.push(LzmaFilter {
                id,
                options: options_ptr,
            });
        }
        other => return Err(value_error(&format!("Invalid filter ID: {other}"))),
    }
    Ok(chain)
}

/// Parses the LZMA1/LZMA2 option dict: preset defaults first, then overrides.
fn parse_lzma_options(pairs: &[(i64, i64)]) -> Result<Box<LzmaOptionsLzma>, i64> {
    let preset = match dict_lookup(pairs, "preset") {
        Some(value_arg) => u32_argument(value_arg, "preset")?,
        None => PRESET_DEFAULT,
    };
    let mut options = Box::new(unsafe {
        // SAFETY: `lzma_options_lzma` is plain data; zeroed means "unset" and
        // `lzma_lzma_preset` fills every meaningful field.
        std::mem::zeroed::<LzmaOptionsLzma>()
    });
    // SAFETY: `options` is a valid, exclusively borrowed option struct.
    if unsafe { lzma_lzma_preset(&mut *options, preset) } != 0 {
        return Err(lzma_error(&format!("Invalid compression preset: {preset}")));
    }
    for (key, item) in pairs {
        let Some(key) = value::as_str(*key) else {
            return Err(value_error("Invalid filter specifier for LZMA filter"));
        };
        let converted = match key {
            "id" | "preset" => continue,
            "dict_size" => u32_argument(*item, key).map(|n| options.dict_size = n),
            "lc" => u32_argument(*item, key).map(|n| options.lc = n),
            "lp" => u32_argument(*item, key).map(|n| options.lp = n),
            "pb" => u32_argument(*item, key).map(|n| options.pb = n),
            "mode" => c_int_argument(*item, key, 0).map(|n| options.mode = n),
            "nice_len" => u32_argument(*item, key).map(|n| options.nice_len = n),
            "mf" => c_int_argument(*item, key, 0).map(|n| options.mf = n),
            "depth" => u32_argument(*item, key).map(|n| options.depth = n),
            _ => return Err(value_error("Invalid filter specifier for LZMA filter")),
        };
        converted.map_err(|_| value_error("Invalid filter specifier for LZMA filter"))?;
    }
    Ok(options)
}

/// Parses a `filters=` argument (a list/tuple of dicts) into a chain.
fn parse_filter_chain(filters: i64) -> Result<FilterChain, i64> {
    let items: Vec<i64> = if let Some(list) = value::as_list(filters) {
        list.to_vec()
    } else if let Some(tuple) = value::as_tuple(filters) {
        tuple.to_vec()
    } else {
        return Err(type_error("filters must be a sequence of dicts"));
    };
    if items.len() > FILTERS_MAX {
        return Err(value_error(&format!(
            "Too many filters - liblzma supports a maximum of {FILTERS_MAX}"
        )));
    }
    let mut chain = FilterChain {
        filters: Vec::new(),
        options: Vec::new(),
    };
    for item in items {
        let mut parsed = parse_filter_spec(item)?;
        chain.filters.append(&mut parsed.filters);
        chain.options.append(&mut parsed.options);
    }
    Ok(chain)
}

/// Terminates a chain for liblzma and returns the first filter pointer.
fn terminated(chain: &mut FilterChain) -> *const LzmaFilter {
    chain.filters.push(LzmaFilter {
        id: LZMA_VLI_UNKNOWN,
        options: ptr::null_mut(),
    });
    chain.filters.as_ptr()
}

// -------------------------------------------------------------------------
// `_encode_filter_properties` / `_decode_filter_properties`
// -------------------------------------------------------------------------

/// Builds a dict from `(name, value)` pairs, rooting each freshly allocated
/// key (and any heap value) until the dict owns it. Rust-side `Vec` buffers are
/// invisible to the conservative collector, so this cannot be skipped.
fn dict_from_fields(fields: &[(&str, i64)]) -> i64 {
    let mut roots = Vec::with_capacity(fields.len());
    let mut pairs = Vec::with_capacity(fields.len());
    for (name, item) in fields {
        let key = value::str_alloc((*name).to_string());
        roots.push(heap::root_values(&[key, *item]));
        pairs.push((key, *item));
    }
    let dict = value::dict_new(pairs);
    for start in roots.into_iter().rev() {
        heap::release_roots(start);
    }
    dict
}

/// Builds the Python dict `_decode_filter_properties` returns.
fn build_filter_spec(filter: &LzmaFilter) -> Result<i64, i64> {
    let mut fields: Vec<(&str, i64)> = vec![("id", int_constant(filter.id as i64))];
    match filter.id {
        FILTER_LZMA1 => {
            // SAFETY: `lzma_properties_decode` allocated an
            // `lzma_options_lzma` for an LZMA1 filter.
            let options = unsafe { &*(filter.options as *const LzmaOptionsLzma) };
            fields.push(("lc", int_constant(options.lc as i64)));
            fields.push(("lp", int_constant(options.lp as i64)));
            fields.push(("pb", int_constant(options.pb as i64)));
            fields.push(("dict_size", int_constant(options.dict_size as i64)));
        }
        FILTER_LZMA2 => {
            // SAFETY: same allocation shape, only `dict_size` is meaningful.
            let options = unsafe { &*(filter.options as *const LzmaOptionsLzma) };
            fields.push(("dict_size", int_constant(options.dict_size as i64)));
        }
        FILTER_DELTA => {
            // SAFETY: an `lzma_options_delta` for a delta filter.
            let options = unsafe { &*(filter.options as *const LzmaOptionsDelta) };
            fields.push(("dist", int_constant(options.dist as i64)));
        }
        FILTER_X86 | FILTER_POWERPC | FILTER_IA64 | FILTER_ARM | FILTER_ARMTHUMB | FILTER_SPARC => {
            if !filter.options.is_null() {
                // SAFETY: an `lzma_options_bcj` for a BCJ filter.
                let options = unsafe { &*(filter.options as *const LzmaOptionsBcj) };
                fields.push(("start_offset", int_constant(options.start_offset as i64)));
            }
        }
        other => return Err(value_error(&format!("Invalid filter ID: {other}"))),
    }
    Ok(dict_from_fields(&fields))
}

fn encode_filter_properties(filter: i64) -> i64 {
    let chain = match parse_filter_spec(filter) {
        Ok(chain) => chain,
        Err(error) => return error,
    };
    let filter = chain.filters[0];
    let mut size: u32 = 0;
    // SAFETY: `filter` points at a live option object held by `chain`.
    if let Err(error) = catch_lzma(unsafe { lzma_properties_size(&mut size, &filter) }) {
        return error;
    }
    let mut buffer = vec![0u8; (size as usize).max(1)];
    // SAFETY: `buffer` has at least `size` writable bytes.
    if let Err(error) = catch_lzma(unsafe { lzma_properties_encode(&filter, buffer.as_mut_ptr()) })
    {
        return error;
    }
    buffer.truncate(size as usize);
    value::bytes_new(buffer)
}

fn decode_filter_properties(filter_id: i64, encoded_props: i64) -> i64 {
    let id = match u64_argument(filter_id, "filter_id") {
        Ok(id) => id,
        Err(error) => return error,
    };
    let props = match bytes_argument(encoded_props, "encoded_props") {
        Ok(props) => props,
        Err(error) => return error,
    };
    let mut filter = LzmaFilter {
        id,
        options: ptr::null_mut(),
    };
    // SAFETY: `props` is a live buffer; the allocator is liblzma's default.
    let status =
        unsafe { lzma_properties_decode(&mut filter, ptr::null(), props.as_ptr(), props.len()) };
    if let Err(error) = catch_lzma(status) {
        return error;
    }
    let spec = build_filter_spec(&filter);
    if !filter.options.is_null() {
        // SAFETY: `lzma_properties_decode` allocated this with the default
        // allocator, so the C library's `free` releases it.
        unsafe { libc::free(filter.options) };
    }
    match spec {
        Ok(spec) => spec,
        Err(error) => error,
    }
}

// -------------------------------------------------------------------------
// LZMA encoder/decoder construction
// -------------------------------------------------------------------------

/// Initialises an XZ/ALONE/RAW encoder into a fresh stream.
fn init_lzma_encoder(
    stream: &mut LzmaStream,
    format: i64,
    check: c_int,
    preset: u32,
    filters: i64,
) -> Result<(), i64> {
    match format {
        FORMAT_XZ => {
            let check = if check == -1 { CHECK_CRC64 } else { check };
            if filters == NONE {
                // SAFETY: `stream` is a zeroed `lzma_stream`.
                return catch_lzma(unsafe { lzma_easy_encoder(stream, preset, check) });
            }
            let mut chain = parse_filter_chain(filters)?;
            let pointer = terminated(&mut chain);
            // SAFETY: the chain and its option objects outlive the call, which
            // copies them into the encoder.
            let status = unsafe { lzma_stream_encoder(stream, pointer, check) };
            catch_lzma(status)
        }
        FORMAT_ALONE => {
            if filters != NONE {
                let chain = parse_filter_chain(filters)?;
                if chain.filters.len() != 1 || chain.filters[0].id != FILTER_LZMA1 {
                    return Err(value_error(
                        "Invalid filter chain for FORMAT_ALONE - must be a single LZMA1 filter",
                    ));
                }
                let options = chain.filters[0].options as *const LzmaOptionsLzma;
                // SAFETY: a single LZMA1 filter guarantees an LZMA option
                // object, alive for the duration of the call.
                let status = unsafe { lzma_alone_encoder(stream, options) };
                return catch_lzma(status);
            }
            let mut options = Box::new(unsafe {
                // SAFETY: plain data; the preset call fills every field.
                std::mem::zeroed::<LzmaOptionsLzma>()
            });
            // SAFETY: `options` is exclusively borrowed and valid.
            if unsafe { lzma_lzma_preset(&mut *options, preset) } != 0 {
                return Err(lzma_error(&format!("Invalid compression preset: {preset}")));
            }
            // SAFETY: `stream` is zeroed; the options outlive the call.
            let status = unsafe { lzma_alone_encoder(stream, &*options) };
            catch_lzma(status)
        }
        FORMAT_RAW => {
            if filters == NONE {
                return Err(value_error("Must specify filters for FORMAT_RAW"));
            }
            let mut chain = parse_filter_chain(filters)?;
            let pointer = terminated(&mut chain);
            // SAFETY: the chain and its option objects outlive the call.
            let status = unsafe { lzma_raw_encoder(stream, pointer) };
            catch_lzma(status)
        }
        other => Err(value_error(&format!("Invalid container format: {other}"))),
    }
}

/// Initialises an AUTO/XZ/ALONE/RAW decoder into a fresh stream.
fn init_lzma_decoder(
    stream: &mut LzmaStream,
    format: i64,
    memlimit: u64,
    filters: i64,
) -> Result<c_int, i64> {
    match format {
        FORMAT_AUTO => {
            // SAFETY: `stream` is a zeroed `lzma_stream`.
            catch_lzma(unsafe { lzma_auto_decoder(stream, memlimit, DECODER_FLAGS) })?;
            Ok(CHECK_UNKNOWN)
        }
        FORMAT_XZ => {
            // SAFETY: `stream` is a zeroed `lzma_stream`.
            catch_lzma(unsafe { lzma_stream_decoder(stream, memlimit, DECODER_FLAGS) })?;
            Ok(CHECK_UNKNOWN)
        }
        FORMAT_ALONE => {
            // SAFETY: `stream` is a zeroed `lzma_stream`.
            catch_lzma(unsafe { lzma_alone_decoder(stream, memlimit) })?;
            Ok(CHECK_NONE)
        }
        FORMAT_RAW => {
            if filters == NONE {
                return Err(value_error("Must specify filters for FORMAT_RAW"));
            }
            let mut chain = parse_filter_chain(filters)?;
            let pointer = terminated(&mut chain);
            // SAFETY: the chain and its option objects outlive the call.
            catch_lzma(unsafe { lzma_raw_decoder(stream, pointer) })?;
            Ok(CHECK_NONE)
        }
        other => Err(value_error(&format!("Invalid container format: {other}"))),
    }
}

// -------------------------------------------------------------------------
// `_bz2` classes
// -------------------------------------------------------------------------

fn bz2_compressor_class() -> i64 {
    cached_class("_bz2.BZ2Compressor", || {
        let init = value::native_function_new("_bz2", "_bz2_compressor_init", usize::MAX);
        value::function_set_signature(
            init,
            vec![
                value::ParamMeta {
                    name: "self".to_string(),
                    kind: hydro_ir::ParamKind::Positional,
                    default: None,
                },
                value::ParamMeta {
                    name: "compresslevel".to_string(),
                    kind: hydro_ir::ParamKind::Positional,
                    default: Some(int_constant(9)),
                },
            ],
        );
        value::function_set_full_entry(init, bz2_compressor_init_entry as *const () as usize);
        value::class_new(
            "BZ2Compressor".to_string(),
            NONE,
            vec![
                ("__init__".to_string(), init),
                (
                    "compress".to_string(),
                    value::native_function_new("_bz2", "_bz2_compress", usize::MAX),
                ),
                (
                    "flush".to_string(),
                    value::native_function_new("_bz2", "_bz2_flush", usize::MAX),
                ),
            ],
        )
    })
}

fn bz2_decompressor_class() -> i64 {
    cached_class("_bz2.BZ2Decompressor", || {
        let init = value::native_function_new("_bz2", "_bz2_decompressor_init", usize::MAX);
        value::function_set_signature(
            init,
            vec![value::ParamMeta {
                name: "self".to_string(),
                kind: hydro_ir::ParamKind::Positional,
                default: None,
            }],
        );
        value::function_set_full_entry(init, bz2_decompressor_init_entry as *const () as usize);
        value::class_new(
            "BZ2Decompressor".to_string(),
            NONE,
            vec![
                ("__init__".to_string(), init),
                (
                    "decompress".to_string(),
                    value::native_function_new("_bz2", "_bz2_decompress", usize::MAX),
                ),
            ],
        )
    })
}

fn init_bz2_compressor(instance: i64, compresslevel: i64) -> Result<(), i64> {
    let level = int_argument(compresslevel, "compresslevel", 9)?;
    if !(1..=9).contains(&level) {
        return Err(value_error("compresslevel must be between 1 and 9"));
    }
    let mut stream = new_bz_stream();
    // SAFETY: `stream` is a fresh zeroed `bz_stream`; null allocator hooks
    // select libbzip2's default memory handling.
    let status = unsafe { BZ2_bzCompressInit(&mut *stream, level as c_int, 0, 0) };
    catch_bz2(status)?;
    let handle = register(Codec::Bz2Compressor(Bz2CompressorState {
        stream,
        flushed: false,
    }));
    attach_handle(instance, handle);
    Ok(())
}

fn init_bz2_decompressor(instance: i64) -> Result<(), i64> {
    let mut stream = new_bz_stream();
    // SAFETY: `stream` is a fresh zeroed `bz_stream`.
    let status = unsafe { BZ2_bzDecompressInit(&mut *stream, 0, 0) };
    catch_bz2(status)?;
    let handle = register(Codec::Bz2Decompressor(Bz2DecompressorState {
        stream,
        eof: false,
        needs_input: true,
        failed: false,
        unused: Vec::new(),
        input: Vec::new(),
    }));
    attach_handle(instance, handle);
    store_bz2_attributes(instance, false, Vec::new(), true);
    Ok(())
}

fn bz2_compressor_new(compresslevel: i64) -> i64 {
    let instance = value::instance_new(bz2_compressor_class());
    match init_bz2_compressor(instance, compresslevel) {
        Ok(()) => instance,
        Err(error) => error,
    }
}

fn bz2_decompressor_new() -> i64 {
    let instance = value::instance_new(bz2_decompressor_class());
    match init_bz2_decompressor(instance) {
        Ok(()) => instance,
        Err(error) => error,
    }
}

/// `BZ2Compressor(compresslevel=9)`.
extern "C" fn bz2_compressor_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the class binder passes the signature's two slots.
    let instance = unsafe { *argv };
    let compresslevel = unsafe { *argv.add(1) };
    match init_bz2_compressor(instance, compresslevel) {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

/// `BZ2Decompressor()`.
extern "C" fn bz2_decompressor_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the class binder passes the signature's single slot.
    let instance = unsafe { *argv };
    match init_bz2_decompressor(instance) {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

// -------------------------------------------------------------------------
// `_lzma` classes
// -------------------------------------------------------------------------

fn lzma_compressor_class() -> i64 {
    cached_class("_lzma.LZMACompressor", || {
        let init = value::native_function_new("_lzma", "_lzma_compressor_init", usize::MAX);
        value::function_set_signature(
            init,
            vec![
                value::ParamMeta {
                    name: "self".to_string(),
                    kind: hydro_ir::ParamKind::Positional,
                    default: None,
                },
                value::ParamMeta {
                    name: "format".to_string(),
                    kind: hydro_ir::ParamKind::Positional,
                    default: Some(int_constant(FORMAT_XZ)),
                },
                value::ParamMeta {
                    name: "check".to_string(),
                    kind: hydro_ir::ParamKind::Positional,
                    default: Some(int_constant(-1)),
                },
                value::ParamMeta {
                    name: "preset".to_string(),
                    kind: hydro_ir::ParamKind::Positional,
                    default: Some(NONE),
                },
                value::ParamMeta {
                    name: "filters".to_string(),
                    kind: hydro_ir::ParamKind::Positional,
                    default: Some(NONE),
                },
            ],
        );
        value::function_set_full_entry(init, lzma_compressor_init_entry as *const () as usize);
        value::class_new(
            "LZMACompressor".to_string(),
            NONE,
            vec![
                ("__init__".to_string(), init),
                (
                    "compress".to_string(),
                    value::native_function_new("_lzma", "_lzma_compress", usize::MAX),
                ),
                (
                    "flush".to_string(),
                    value::native_function_new("_lzma", "_lzma_flush", usize::MAX),
                ),
            ],
        )
    })
}

fn lzma_decompressor_class() -> i64 {
    cached_class("_lzma.LZMADecompressor", || {
        let init = value::native_function_new("_lzma", "_lzma_decompressor_init", usize::MAX);
        value::function_set_signature(
            init,
            vec![
                value::ParamMeta {
                    name: "self".to_string(),
                    kind: hydro_ir::ParamKind::Positional,
                    default: None,
                },
                value::ParamMeta {
                    name: "format".to_string(),
                    kind: hydro_ir::ParamKind::Positional,
                    default: Some(int_constant(FORMAT_AUTO)),
                },
                value::ParamMeta {
                    name: "memlimit".to_string(),
                    kind: hydro_ir::ParamKind::Positional,
                    default: Some(NONE),
                },
                value::ParamMeta {
                    name: "filters".to_string(),
                    kind: hydro_ir::ParamKind::Positional,
                    default: Some(NONE),
                },
            ],
        );
        value::function_set_full_entry(init, lzma_decompressor_init_entry as *const () as usize);
        value::class_new(
            "LZMADecompressor".to_string(),
            NONE,
            vec![
                ("__init__".to_string(), init),
                (
                    "decompress".to_string(),
                    value::native_function_new("_lzma", "_lzma_decompress", usize::MAX),
                ),
            ],
        )
    })
}

fn init_lzma_compressor(
    instance: i64,
    format: i64,
    check: i64,
    preset: i64,
    filters: i64,
) -> Result<(), i64> {
    let format = int_argument(format, "format", FORMAT_XZ)?;
    let check = c_int_argument(check, "check", -1)?;
    if format != FORMAT_XZ && check != -1 && check != CHECK_NONE {
        return Err(value_error(
            "Integrity checks are only supported by FORMAT_XZ",
        ));
    }
    if preset != NONE && filters != NONE {
        return Err(value_error("Cannot specify both preset and filter chain"));
    }
    let preset = if preset == NONE {
        PRESET_DEFAULT
    } else {
        u32_argument(preset, "preset")?
    };
    let mut stream = new_lzma_stream();
    init_lzma_encoder(&mut stream, format, check, preset, filters)?;
    let handle = register(Codec::LzmaCompressor(LzmaCompressorState {
        stream,
        flushed: false,
    }));
    attach_handle(instance, handle);
    Ok(())
}

fn init_lzma_decompressor(
    instance: i64,
    format: i64,
    memlimit: i64,
    filters: i64,
) -> Result<(), i64> {
    let format = int_argument(format, "format", FORMAT_AUTO)?;
    let memlimit = if memlimit == NONE {
        u64::MAX
    } else {
        if format == FORMAT_RAW {
            return Err(value_error("Cannot specify memory limit with FORMAT_RAW"));
        }
        u64_argument(memlimit, "memlimit")?
    };
    if format == FORMAT_RAW && filters == NONE {
        return Err(value_error("Must specify filters for FORMAT_RAW"));
    }
    if format != FORMAT_RAW && filters != NONE {
        return Err(value_error("Cannot specify filters except with FORMAT_RAW"));
    }
    let mut stream = new_lzma_stream();
    let check = init_lzma_decoder(&mut stream, format, memlimit, filters)?;
    let handle = register(Codec::LzmaDecompressor(LzmaDecompressorState {
        stream,
        check,
        eof: false,
        needs_input: true,
        unused: Vec::new(),
        input: Vec::new(),
    }));
    attach_handle(instance, handle);
    store_lzma_attributes(instance, check, false, Vec::new(), true);
    Ok(())
}

fn lzma_compressor_new(format: i64, check: i64, preset: i64, filters: i64) -> i64 {
    let instance = value::instance_new(lzma_compressor_class());
    match init_lzma_compressor(instance, format, check, preset, filters) {
        Ok(()) => instance,
        Err(error) => error,
    }
}

fn lzma_decompressor_new(format: i64, memlimit: i64, filters: i64) -> i64 {
    let instance = value::instance_new(lzma_decompressor_class());
    match init_lzma_decompressor(instance, format, memlimit, filters) {
        Ok(()) => instance,
        Err(error) => error,
    }
}

/// `LZMACompressor(format=FORMAT_XZ, check=-1, preset=None, filters=None)`.
extern "C" fn lzma_compressor_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the class binder passes the signature's five slots.
    let instance = unsafe { *argv };
    let format = unsafe { *argv.add(1) };
    let check = unsafe { *argv.add(2) };
    let preset = unsafe { *argv.add(3) };
    let filters = unsafe { *argv.add(4) };
    match init_lzma_compressor(instance, format, check, preset, filters) {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

/// `LZMADecompressor(format=FORMAT_AUTO, memlimit=None, filters=None)`.
extern "C" fn lzma_decompressor_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the class binder passes the signature's four slots.
    let instance = unsafe { *argv };
    let format = unsafe { *argv.add(1) };
    let memlimit = unsafe { *argv.add(2) };
    let filters = unsafe { *argv.add(3) };
    match init_lzma_decompressor(instance, format, memlimit, filters) {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

// -------------------------------------------------------------------------
// Method bodies
// -------------------------------------------------------------------------

fn bz2_compress_method(args: &[i64]) -> i64 {
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
    let mut out = Vec::new();
    let outcome = match registry.codecs.get_mut(&handle) {
        Some(Codec::Bz2Compressor(state)) => {
            if state.flushed {
                Err(value_error("Compressor has been flushed"))
            } else {
                bz2_compress_pump(state, &data, BZ_RUN, &mut out)
            }
        }
        _ => Err(inconsistent_state()),
    };
    drop(registry);
    match outcome {
        Ok(()) => value::bytes_new(out),
        Err(error) => error,
    }
}

fn bz2_flush_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let handle = match handle_of(instance) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let mut registry = registry();
    let mut out = Vec::new();
    let outcome = match registry.codecs.get_mut(&handle) {
        Some(Codec::Bz2Compressor(state)) => {
            if state.flushed {
                Err(value_error("Repeated call to flush()"))
            } else {
                state.flushed = true;
                bz2_compress_pump(state, &[], BZ_FINISH, &mut out)
            }
        }
        _ => Err(inconsistent_state()),
    };
    drop(registry);
    match outcome {
        Ok(()) => value::bytes_new(out),
        Err(error) => error,
    }
}

fn bz2_decompress_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let data = match bytes_argument(argument(args, 1), "data") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let max_length = match int_argument(argument(args, 2), "max_length", -1) {
        Ok(max_length) => max_length,
        Err(error) => return error,
    };
    let handle = match handle_of(instance) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let (outcome, snapshot) = {
        let mut registry = registry();
        let mut out = Vec::new();
        match registry.codecs.get_mut(&handle) {
            Some(Codec::Bz2Decompressor(state)) => {
                let outcome = if state.eof {
                    Err(eof_error("End of stream already reached"))
                } else if state.failed {
                    Err(value_error(
                        "Decompressor is unusable after a previous error",
                    ))
                } else {
                    state.input.extend_from_slice(&data);
                    bz2_decompress_pump(state, max_length, &mut out)
                };
                let snapshot = (state.eof, state.unused.clone(), state.needs_input);
                (outcome, Some((out, snapshot)))
            }
            _ => (Err(inconsistent_state()), None),
        }
    };
    let (out, (eof, unused, needs_input)) = match snapshot {
        Some(snapshot) => snapshot,
        None => {
            return match outcome {
                Ok(()) => NONE,
                Err(error) => error,
            };
        }
    };
    store_bz2_attributes(instance, eof, unused, needs_input);
    match outcome {
        Ok(()) => value::bytes_new(out),
        Err(error) => error,
    }
}

fn lzma_compress_method(args: &[i64]) -> i64 {
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
    let mut out = Vec::new();
    let outcome = match registry.codecs.get_mut(&handle) {
        Some(Codec::LzmaCompressor(state)) => {
            if state.flushed {
                Err(value_error("Compressor has been flushed"))
            } else {
                lzma_compress_pump(state, &data, LZMA_RUN, &mut out)
            }
        }
        _ => Err(inconsistent_state()),
    };
    drop(registry);
    match outcome {
        Ok(()) => value::bytes_new(out),
        Err(error) => error,
    }
}

fn lzma_flush_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let handle = match handle_of(instance) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let mut registry = registry();
    let mut out = Vec::new();
    let outcome = match registry.codecs.get_mut(&handle) {
        Some(Codec::LzmaCompressor(state)) => {
            if state.flushed {
                Err(value_error("Repeated call to flush()"))
            } else {
                state.flushed = true;
                lzma_compress_pump(state, &[], LZMA_FINISH, &mut out)
            }
        }
        _ => Err(inconsistent_state()),
    };
    drop(registry);
    match outcome {
        Ok(()) => value::bytes_new(out),
        Err(error) => error,
    }
}

fn lzma_decompress_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let data = match bytes_argument(argument(args, 1), "data") {
        Ok(data) => data,
        Err(error) => return error,
    };
    let max_length = match int_argument(argument(args, 2), "max_length", -1) {
        Ok(max_length) => max_length,
        Err(error) => return error,
    };
    let handle = match handle_of(instance) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let (outcome, snapshot) = {
        let mut registry = registry();
        let mut out = Vec::new();
        match registry.codecs.get_mut(&handle) {
            Some(Codec::LzmaDecompressor(state)) => {
                let outcome = if state.eof {
                    Err(eof_error("Already at end of stream"))
                } else {
                    state.input.extend_from_slice(&data);
                    lzma_decompress_pump(state, max_length, &mut out)
                };
                let snapshot = (
                    state.check,
                    state.eof,
                    state.unused.clone(),
                    state.needs_input,
                );
                (outcome, Some((out, snapshot)))
            }
            _ => (Err(inconsistent_state()), None),
        }
    };
    let (out, (check, eof, unused, needs_input)) = match snapshot {
        Some(snapshot) => snapshot,
        None => {
            return match outcome {
                Ok(()) => NONE,
                Err(error) => error,
            };
        }
    };
    store_lzma_attributes(instance, check, eof, unused, needs_input);
    match outcome {
        Ok(()) => value::bytes_new(out),
        Err(error) => error,
    }
}

// -------------------------------------------------------------------------
// Module surface
// -------------------------------------------------------------------------

fn is_bz2(module: &str) -> bool {
    matches!(module, "_bz2" | "bz2")
}

fn is_lzma(module: &str) -> bool {
    matches!(module, "_lzma" | "lzma")
}

fn missing_attribute(module: &str, name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module '{module}' has no attribute '{name}'"),
    )
}

/// Class-valued module members. Attribute reads must return the class, not a
/// constructor function, so identity/`isinstance` behave like CPython.
pub(crate) fn class_value(module: &str, name: &str) -> Option<i64> {
    if is_bz2(module) {
        return match name {
            "BZ2Compressor" => Some(bz2_compressor_class()),
            "BZ2Decompressor" => Some(bz2_decompressor_class()),
            _ => None,
        };
    }
    if is_lzma(module) {
        return match name {
            "LZMACompressor" => Some(lzma_compressor_class()),
            "LZMADecompressor" => Some(lzma_decompressor_class()),
            "LZMAError" => Some(lzma_error_class()),
            _ => None,
        };
    }
    None
}

/// Materialises a module constant (or a class/function-valued member).
pub(crate) fn constant(module: &str, name: &str) -> i64 {
    if let Some(class) = class_value(module, name) {
        return class;
    }
    if !is_lzma(module) {
        return missing_attribute(module, name);
    }
    let numeric = match name {
        "FORMAT_AUTO" => Some(FORMAT_AUTO),
        "FORMAT_XZ" => Some(FORMAT_XZ),
        "FORMAT_ALONE" => Some(FORMAT_ALONE),
        "FORMAT_RAW" => Some(FORMAT_RAW),
        "CHECK_NONE" => Some(CHECK_NONE as i64),
        "CHECK_CRC32" => Some(CHECK_CRC32 as i64),
        "CHECK_CRC64" => Some(CHECK_CRC64 as i64),
        "CHECK_SHA256" => Some(CHECK_SHA256 as i64),
        "CHECK_ID_MAX" => Some(CHECK_ID_MAX as i64),
        "CHECK_UNKNOWN" => Some(CHECK_UNKNOWN as i64),
        "FILTER_LZMA1" => Some(FILTER_LZMA1 as i64),
        "FILTER_LZMA2" => Some(FILTER_LZMA2 as i64),
        "FILTER_DELTA" => Some(FILTER_DELTA as i64),
        "FILTER_X86" => Some(FILTER_X86 as i64),
        "FILTER_POWERPC" => Some(FILTER_POWERPC as i64),
        "FILTER_IA64" => Some(FILTER_IA64 as i64),
        "FILTER_ARM" => Some(FILTER_ARM as i64),
        "FILTER_ARMTHUMB" => Some(FILTER_ARMTHUMB as i64),
        "FILTER_SPARC" => Some(FILTER_SPARC as i64),
        "MF_HC3" => Some(MF_HC3),
        "MF_HC4" => Some(MF_HC4),
        "MF_BT2" => Some(MF_BT2),
        "MF_BT3" => Some(MF_BT3),
        "MF_BT4" => Some(MF_BT4),
        "MODE_FAST" => Some(MODE_FAST),
        "MODE_NORMAL" => Some(MODE_NORMAL),
        "PRESET_DEFAULT" => Some(PRESET_DEFAULT as i64),
        "PRESET_EXTREME" => Some(PRESET_EXTREME),
        _ => None,
    };
    if let Some(numeric) = numeric {
        return int_constant(numeric);
    }
    match name {
        "is_check_supported" => value::native_function_new("_lzma", "is_check_supported", 1),
        "_encode_filter_properties" => {
            value::native_function_new("_lzma", "_encode_filter_properties", 1)
        }
        "_decode_filter_properties" => {
            value::native_function_new("_lzma", "_decode_filter_properties", 2)
        }
        _ => missing_attribute(module, name),
    }
}

fn is_check_supported(value_arg: i64) -> i64 {
    let id = match c_int_argument(value_arg, "check_id", 0) {
        Ok(id) => id,
        Err(error) => return error,
    };
    // SAFETY: `lzma_check_is_supported` has no preconditions.
    value::bool_value(unsafe { lzma_check_is_supported(id) } != 0)
}

/// Dispatches `_bz2`/`_lzma` calls, including the native method bodies
/// installed on the codec classes.
pub(crate) fn call(module: &str, function: &str, args: &[i64]) -> i64 {
    let bare = function.rsplit('.').next().unwrap_or(function);
    if is_bz2(module) {
        return match bare {
            // The constructor decodes and validates the tagged slot itself
            // (`init_bz2_compressor` is also the class `__init__` entry), so
            // the raw argument must be forwarded untouched.
            "BZ2Compressor" => bz2_compressor_new(argument(args, 0)),
            "BZ2Decompressor" => bz2_decompressor_new(),
            "_bz2_compress" => bz2_compress_method(args),
            "_bz2_flush" => bz2_flush_method(args),
            "_bz2_decompress" => bz2_decompress_method(args),
            "_bz2_compressor_init" => {
                match init_bz2_compressor(argument(args, 0), argument(args, 1)) {
                    Ok(()) => NONE,
                    Err(error) => error,
                }
            }
            "_bz2_decompressor_init" => match init_bz2_decompressor(argument(args, 0)) {
                Ok(()) => NONE,
                Err(error) => error,
            },
            _ => missing_attribute(module, function),
        };
    }
    if is_lzma(module) {
        return match bare {
            "LZMACompressor" => lzma_compressor_new(
                argument(args, 0),
                argument(args, 1),
                argument(args, 2),
                argument(args, 3),
            ),
            "LZMADecompressor" => {
                lzma_decompressor_new(argument(args, 0), argument(args, 1), argument(args, 2))
            }
            "_lzma_compress" => lzma_compress_method(args),
            "_lzma_flush" => lzma_flush_method(args),
            "_lzma_decompress" => lzma_decompress_method(args),
            "_lzma_compressor_init" => match init_lzma_compressor(
                argument(args, 0),
                argument(args, 1),
                argument(args, 2),
                argument(args, 3),
                argument(args, 4),
            ) {
                Ok(()) => NONE,
                Err(error) => error,
            },
            "_lzma_decompressor_init" => match init_lzma_decompressor(
                argument(args, 0),
                argument(args, 1),
                argument(args, 2),
                argument(args, 3),
            ) {
                Ok(()) => NONE,
                Err(error) => error,
            },
            "is_check_supported" => is_check_supported(argument(args, 0)),
            "_encode_filter_properties" => encode_filter_properties(argument(args, 0)),
            "_decode_filter_properties" => {
                decode_filter_properties(argument(args, 0), argument(args, 1))
            }
            _ => missing_attribute(module, function),
        };
    }
    missing_attribute(module, function)
}

/// Binds keyword arguments into a positional slot layout. Slots before a named
/// one are padded with `None`, matching the compiler's keyword lowering.
fn bind_keywords(
    function: &str,
    args: &[i64],
    keywords: &[(String, i64)],
    names: &[&str],
) -> Result<Vec<i64>, i64> {
    let mut slots = args.to_vec();
    let mut provided = vec![true; args.len()];
    provided.resize(names.len(), false);
    for (name, item) in keywords {
        let Some(index) = names.iter().position(|candidate| candidate == name) else {
            return Err(type_error(&format!(
                "{function}() got an unexpected keyword argument '{name}'"
            )));
        };
        if provided[index] {
            return Err(type_error(&format!(
                "{function}() got multiple values for argument '{name}'"
            )));
        }
        if slots.len() < index {
            slots.resize(index, NONE);
        }
        slots.push(*item);
        provided[index] = true;
    }
    Ok(slots)
}

/// Keyword-aware dispatch for the constructors and `_lzma` helpers. Named
/// parameters normally arrive already bound by the shared binder; this is the
/// fallback for direct native calls.
pub(crate) fn call_kw(
    module: &str,
    function: &str,
    args: &[i64],
    keywords: &[(String, i64)],
) -> i64 {
    if keywords.is_empty() {
        return call(module, function, args);
    }
    let bare = function.rsplit('.').next().unwrap_or(function);
    let names: &[&str] = match bare {
        "BZ2Compressor" => &["compresslevel"],
        "LZMACompressor" => &["format", "check", "preset", "filters"],
        "LZMADecompressor" => &["format", "memlimit", "filters"],
        "is_check_supported" => &["check_id"],
        "_encode_filter_properties" => &["filter"],
        "_decode_filter_properties" => &["filter_id", "encoded_props"],
        _ => {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("{module}.{function}() does not accept keyword arguments"),
            )
        }
    };
    match bind_keywords(bare, args, keywords, names) {
        Ok(slots) => call(module, function, &slots),
        Err(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference vectors produced by CPython 3.14.9 (`bz2`/`lzma` on the
    // system libraries), so a mismatch means the stream is wrong, not merely
    // different.
    const BZ2_HELLO: &[u8] = &[
        0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0x44, 0xf7, 0x13, 0x78, 0x00,
        0x00, 0x01, 0x91, 0x80, 0x40, 0x00, 0x06, 0x44, 0x90, 0x80, 0x20, 0x00, 0x22, 0x03, 0x34,
        0x84, 0x30, 0x21, 0xb6, 0x81, 0x54, 0x27, 0x8b, 0xb9, 0x22, 0x9c, 0x28, 0x48, 0x22, 0x7b,
        0x89, 0xbc, 0x00,
    ];
    const XZ_HELLO: &[u8] = &[
        0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00, 0x04, 0xe6, 0xd6, 0xb4, 0x46, 0x02, 0x00, 0x21,
        0x01, 0x16, 0x00, 0x00, 0x00, 0x74, 0x2f, 0xe5, 0xa3, 0x01, 0x00, 0x0a, 0x68, 0x65, 0x6c,
        0x6c, 0x6f, 0x20, 0x77, 0x6f, 0x72, 0x6c, 0x64, 0x00, 0x00, 0xda, 0x52, 0x23, 0xef, 0xcd,
        0x7e, 0x03, 0x53, 0x00, 0x01, 0x23, 0x0b, 0xc2, 0x1b, 0xfd, 0x09, 0x1f, 0xb6, 0xf3, 0x7d,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x59, 0x5a,
    ];
    const ALONE_HELLO: &[u8] = &[
        0x5d, 0x00, 0x00, 0x80, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x34,
        0x19, 0x49, 0xee, 0x8d, 0xe9, 0x17, 0x89, 0x3a, 0x33, 0x60, 0x05, 0xf7, 0xcf, 0x64, 0xff,
        0xfb, 0x78, 0x20, 0x00,
    ];
    const RAW_HELLO: &[u8] = &[
        0x01, 0x00, 0x0a, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20, 0x77, 0x6f, 0x72, 0x6c, 0x64, 0x00,
    ];
    const RAW_CHAIN: &[u8] = &[
        0xe0, 0x01, 0xff, 0x00, 0x10, 0x5d, 0x00, 0x30, 0x98, 0x88, 0x98, 0x39, 0xd2, 0x32, 0x25,
        0x19, 0x3b, 0x04, 0x36, 0xe9, 0x75, 0xfc, 0x00, 0x00,
    ];
    const LZMA1_PROPS: &[u8] = &[0x5d, 0x00, 0x00, 0x80, 0x00];

    fn bytes(data: &[u8]) -> i64 {
        value::bytes_new(data.to_vec())
    }

    fn int(n: i64) -> i64 {
        int_constant(n)
    }

    fn number(value_arg: i64) -> i64 {
        value::as_integer(value_arg).expect("integer result")
    }

    fn data(value_arg: i64) -> Vec<u8> {
        value::as_bytes(value_arg).expect("bytes result").to_vec()
    }

    fn assert_bytes(value_arg: i64, expected: &[u8]) {
        assert_eq!(data(value_arg), expected, "unexpected bytes");
    }

    fn call(function: &str, args: &[i64]) -> i64 {
        super::call("_bz2", function, args)
    }

    fn lzma_call(function: &str, args: &[i64]) -> i64 {
        super::call("_lzma", function, args)
    }

    fn lzma_filters(items: &[(u64, &[(&str, i64)])]) -> i64 {
        let mut specs = Vec::new();
        let mut roots = Vec::new();
        for (id, options) in items {
            let mut fields = vec![("id", int_constant(*id as i64))];
            for (name, item) in *options {
                fields.push((*name, *item));
            }
            let spec = dict_from_fields(&fields);
            roots.push(heap::root_values(&[spec]));
            specs.push(spec);
        }
        let list = value::list_new(specs);
        for start in roots.into_iter().rev() {
            heap::release_roots(start);
        }
        list
    }

    fn bz2_compress_all(input: &[u8], level: i64) -> Vec<u8> {
        let compressor = call("BZ2Compressor", &[int(level)]);
        assert!(!exc::pending());
        let mut out = data(call("_bz2_compress", &[compressor, bytes(input)]));
        out.extend(data(call("_bz2_flush", &[compressor])));
        out
    }

    fn lzma_compress_all(
        input: &[u8],
        format: i64,
        check: i64,
        preset: i64,
        filters: i64,
    ) -> Vec<u8> {
        let compressor = lzma_call(
            "LZMACompressor",
            &[int(format), int(check), preset, filters],
        );
        assert!(!exc::pending());
        let mut out = data(lzma_call("_lzma_compress", &[compressor, bytes(input)]));
        out.extend(data(lzma_call("_lzma_flush", &[compressor])));
        out
    }

    #[test]
    fn bz2_round_trip_matches_the_reference_stream() {
        exc::init();
        let compressed = bz2_compress_all(b"hello world", 9);
        assert_eq!(compressed, BZ2_HELLO);
        assert!(!exc::pending());

        let decompressor = call("BZ2Decompressor", &[]);
        let plain = call("_bz2_decompress", &[decompressor, bytes(&compressed)]);
        assert_bytes(plain, b"hello world");
        assert_eq!(value::instance_attr(decompressor, "eof"), Some(value::TRUE));
        assert_eq!(
            value::instance_attr(decompressor, "needs_input"),
            Some(value::FALSE)
        );
        assert_eq!(
            data(value::instance_attr(decompressor, "unused_data").expect("unused_data")),
            Vec::<u8>::new()
        );
        assert!(!exc::pending());
    }

    #[test]
    fn bz2_streaming_compress_buffers_until_flush() {
        exc::init();
        let compressor = call("BZ2Compressor", &[int(9)]);
        let first = data(call("_bz2_compress", &[compressor, bytes(b"hello ")]));
        let second = data(call("_bz2_compress", &[compressor, bytes(b"world")]));
        let tail = data(call("_bz2_flush", &[compressor]));
        assert!(first.is_empty());
        assert!(second.is_empty());
        assert_eq!(tail, BZ2_HELLO);
        assert_eq!(bz2_compress_all(b"hello world", 9), BZ2_HELLO);
    }

    #[test]
    fn bz2_decompressor_tracks_limits_tail_and_eof() {
        exc::init();
        let decompressor = call("BZ2Decompressor", &[]);
        let first = call("_bz2_decompress", &[decompressor, bytes(BZ2_HELLO), int(4)]);
        assert_bytes(first, b"hell");
        assert_eq!(
            value::instance_attr(decompressor, "needs_input"),
            Some(value::FALSE)
        );
        assert_eq!(
            value::instance_attr(decompressor, "eof"),
            Some(value::FALSE)
        );
        let rest = call("_bz2_decompress", &[decompressor, bytes(b"")]);
        assert_bytes(rest, b"o world");
        assert_eq!(value::instance_attr(decompressor, "eof"), Some(value::TRUE));

        // Trailing bytes after the stream are preserved, never swallowed.
        let mut with_trailer = BZ2_HELLO.to_vec();
        with_trailer.extend_from_slice(b"TRAILER");
        let decompressor = call("BZ2Decompressor", &[]);
        let plain = call(
            "_bz2_decompress",
            &[decompressor, value::bytes_new(with_trailer)],
        );
        assert_bytes(plain, b"hello world");
        assert_bytes(
            value::instance_attr(decompressor, "unused_data").expect("unused_data"),
            b"TRAILER",
        );
        assert_eq!(
            value::instance_attr(decompressor, "needs_input"),
            Some(value::FALSE)
        );
    }

    #[test]
    fn bz2_errors_match_cpython() {
        exc::init();
        call("BZ2Compressor", &[int(0)]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();

        let compressor = call("BZ2Compressor", &[int(9)]);
        call("_bz2_flush", &[compressor]);
        assert!(!exc::pending());
        call("_bz2_flush", &[compressor]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        call("_bz2_compress", &[compressor, bytes(b"x")]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();

        let decompressor = call("BZ2Decompressor", &[]);
        call("_bz2_decompress", &[decompressor, bytes(b"junk")]);
        assert_eq!(exc::pending_tag(), OS_ERROR as i64);
        exc::clear();
        // Once libbzip2 failed the object cannot be reused.
        call("_bz2_decompress", &[decompressor, bytes(b"")]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();

        let decompressor = call("BZ2Decompressor", &[]);
        call("_bz2_decompress", &[decompressor, bytes(BZ2_HELLO)]);
        assert!(!exc::pending());
        call("_bz2_decompress", &[decompressor, bytes(b"")]);
        assert_eq!(exc::pending_tag(), EOF_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn bz2_classes_are_real_classes_and_construct_through_init() {
        exc::init();
        let class = class_value("_bz2", "BZ2Compressor").expect("BZ2Compressor class");
        assert!(value::as_class(class).is_some());
        assert_eq!(class, constant("_bz2", "BZ2Compressor"));
        let instance = crate::builtins::call(class, 1, [int(9)].as_ptr());
        assert!(!exc::pending());
        assert_eq!(value::type_name_of(instance), "BZ2Compressor");
        let compressed = call("_bz2_compress", &[instance, bytes(b"hello world")]);
        let tail = call("_bz2_flush", &[instance]);
        let mut whole = data(compressed);
        whole.extend(data(tail));
        assert_eq!(whole, BZ2_HELLO);

        let decompress_class = class_value("_bz2", "BZ2Decompressor").expect("class");
        let decompressor = crate::builtins::call(decompress_class, 0, ptr::null());
        assert_eq!(value::type_name_of(decompressor), "BZ2Decompressor");
        assert_bytes(
            call("_bz2_decompress", &[decompressor, bytes(BZ2_HELLO)]),
            b"hello world",
        );
    }

    #[test]
    fn lzma_xz_alone_and_raw_match_the_reference_streams() {
        exc::init();
        assert_eq!(
            lzma_compress_all(b"hello world", FORMAT_XZ, -1, NONE, NONE),
            XZ_HELLO
        );
        assert_eq!(
            lzma_compress_all(b"hello world", FORMAT_ALONE, -1, NONE, NONE),
            ALONE_HELLO
        );
        let lzma2 = lzma_filters(&[(FILTER_LZMA2, &[("preset", int(6))])]);
        assert_eq!(
            lzma_compress_all(b"hello world", FORMAT_RAW, -1, NONE, lzma2),
            RAW_HELLO
        );
        let chain = lzma_filters(&[
            (FILTER_X86, &[]),
            (FILTER_DELTA, &[("dist", int(4))]),
            (FILTER_LZMA2, &[("preset", int(6))]),
        ]);
        assert_eq!(
            lzma_compress_all(
                b"abcdefgh".repeat(64).as_slice(),
                FORMAT_RAW,
                -1,
                NONE,
                chain
            ),
            RAW_CHAIN
        );
        assert!(!exc::pending());
    }

    #[test]
    fn lzma_decompressors_read_every_reference_stream() {
        exc::init();
        let decompressor = lzma_call("LZMADecompressor", &[int(FORMAT_AUTO), NONE, NONE]);
        let plain = lzma_call("_lzma_decompress", &[decompressor, bytes(XZ_HELLO)]);
        assert_bytes(plain, b"hello world");
        assert_eq!(value::instance_attr(decompressor, "eof"), Some(value::TRUE));
        assert_eq!(
            value::instance_attr(decompressor, "check"),
            Some(int(CHECK_CRC64 as i64))
        );

        let decompressor = lzma_call("LZMADecompressor", &[int(FORMAT_ALONE), NONE, NONE]);
        assert_bytes(
            lzma_call("_lzma_decompress", &[decompressor, bytes(ALONE_HELLO)]),
            b"hello world",
        );

        let lzma2 = lzma_filters(&[(FILTER_LZMA2, &[("preset", int(6))])]);
        let decompressor = lzma_call("LZMADecompressor", &[int(FORMAT_RAW), NONE, lzma2]);
        assert_bytes(
            lzma_call("_lzma_decompress", &[decompressor, bytes(RAW_HELLO)]),
            b"hello world",
        );

        let chain = lzma_filters(&[
            (FILTER_X86, &[]),
            (FILTER_DELTA, &[("dist", int(4))]),
            (FILTER_LZMA2, &[("preset", int(6))]),
        ]);
        let decompressor = lzma_call("LZMADecompressor", &[int(FORMAT_RAW), NONE, chain]);
        assert_eq!(
            data(lzma_call(
                "_lzma_decompress",
                &[decompressor, bytes(RAW_CHAIN)]
            )),
            b"abcdefgh".repeat(64)
        );
        assert!(!exc::pending());
    }

    #[test]
    fn lzma_streaming_and_limit_semantics_match_cpython() {
        exc::init();
        let compressor = lzma_call(
            "LZMACompressor",
            &[int(FORMAT_XZ), int(CHECK_CRC64 as i64), int(6), NONE],
        );
        let first = data(lzma_call("_lzma_compress", &[compressor, bytes(b"hello ")]));
        let second = data(lzma_call("_lzma_compress", &[compressor, bytes(b"world")]));
        let tail = data(lzma_call("_lzma_flush", &[compressor]));
        let mut whole = first;
        whole.extend(second);
        whole.extend(tail);
        assert_eq!(whole, XZ_HELLO);

        let decompressor = lzma_call("LZMADecompressor", &[int(FORMAT_AUTO), NONE, NONE]);
        assert_eq!(
            value::instance_attr(decompressor, "check"),
            Some(int(CHECK_UNKNOWN as i64))
        );
        let partial = lzma_call("_lzma_decompress", &[decompressor, bytes(XZ_HELLO), int(4)]);
        assert_bytes(partial, b"hell");
        assert_eq!(
            value::instance_attr(decompressor, "needs_input"),
            Some(value::FALSE)
        );
        let rest = lzma_call("_lzma_decompress", &[decompressor, bytes(b"")]);
        assert_bytes(rest, b"o world");
        assert_eq!(value::instance_attr(decompressor, "eof"), Some(value::TRUE));

        let mut with_trailer = XZ_HELLO.to_vec();
        with_trailer.extend_from_slice(b"TRAILER");
        let decompressor = lzma_call("LZMADecompressor", &[int(FORMAT_AUTO), NONE, NONE]);
        assert_bytes(
            lzma_call(
                "_lzma_decompress",
                &[decompressor, value::bytes_new(with_trailer)],
            ),
            b"hello world",
        );
        assert_bytes(
            value::instance_attr(decompressor, "unused_data").expect("unused_data"),
            b"TRAILER",
        );
        assert_eq!(
            value::instance_attr(decompressor, "check"),
            Some(int(CHECK_CRC64 as i64))
        );
    }

    #[test]
    fn lzma_errors_match_cpython() {
        exc::init();
        // Flush/compress state machine.
        let compressor = lzma_call("LZMACompressor", &[NONE, NONE, NONE, NONE]);
        lzma_call("_lzma_flush", &[compressor]);
        assert!(!exc::pending());
        lzma_call("_lzma_flush", &[compressor]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        lzma_call("_lzma_compress", &[compressor, bytes(b"x")]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();

        // Constructor validation.
        lzma_call("LZMACompressor", &[int(9), NONE, NONE, NONE]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        lzma_call(
            "LZMACompressor",
            &[int(FORMAT_ALONE), int(CHECK_CRC32 as i64), NONE, NONE],
        );
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        lzma_call(
            "LZMACompressor",
            &[NONE, NONE, int(1), lzma_filters(&[(FILTER_LZMA2, &[])])],
        );
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        lzma_call("LZMACompressor", &[int(FORMAT_RAW), NONE, NONE, NONE]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();

        lzma_call("LZMADecompressor", &[int(FORMAT_RAW), NONE, NONE]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        lzma_call(
            "LZMADecompressor",
            &[int(FORMAT_XZ), NONE, lzma_filters(&[(FILTER_LZMA2, &[])])],
        );
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        lzma_call("LZMADecompressor", &[int(FORMAT_RAW), int(1024), NONE]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        lzma_call("LZMADecompressor", &[NONE, int(-1), NONE]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();

        // A corrupt stream raises LZMAError; a finished one raises EOFError.
        let decompressor = lzma_call("LZMADecompressor", &[int(FORMAT_AUTO), NONE, NONE]);
        lzma_call("_lzma_decompress", &[decompressor, bytes(b"junk")]);
        assert_eq!(exc::pending_tag(), EXCEPTION as i64);
        let pending = exc::pending_value();
        assert_eq!(
            value::as_exc_instance(pending).map(|state| state.class),
            Some(lzma_error_class())
        );
        exc::clear();

        let decompressor = lzma_call("LZMADecompressor", &[int(FORMAT_AUTO), NONE, NONE]);
        lzma_call("_lzma_decompress", &[decompressor, bytes(XZ_HELLO)]);
        assert!(!exc::pending());
        lzma_call("_lzma_decompress", &[decompressor, bytes(b"")]);
        assert_eq!(exc::pending_tag(), EOF_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn lzma_filter_error_messages_are_cpythons() {
        exc::init();
        lzma_call(
            "LZMACompressor",
            &[
                NONE,
                NONE,
                NONE,
                lzma_filters(&[(FILTER_LZMA2, &[("bogus", int(1))])]),
            ],
        );
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        lzma_call(
            "LZMACompressor",
            &[NONE, NONE, NONE, lzma_filters(&[(99, &[])])],
        );
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        // A filter spec without "id" and a non-dict element.
        let empty = value::dict_new(Vec::new());
        lzma_call(
            "LZMACompressor",
            &[NONE, NONE, NONE, value::list_new(vec![empty])],
        );
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        lzma_call(
            "LZMACompressor",
            &[NONE, NONE, NONE, value::list_new(vec![NONE])],
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        let too_many = lzma_filters(&[
            (FILTER_LZMA2, &[]),
            (FILTER_LZMA2, &[]),
            (FILTER_LZMA2, &[]),
            (FILTER_LZMA2, &[]),
            (FILTER_LZMA2, &[]),
        ]);
        lzma_call("LZMACompressor", &[NONE, NONE, NONE, too_many]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn filter_properties_round_trip() {
        exc::init();
        let lzma1 = lzma_filters(&[(FILTER_LZMA1, &[("preset", int(6))])]);
        let filter = value::as_list(lzma1).expect("list")[0];
        let encoded = lzma_call("_encode_filter_properties", &[filter]);
        assert_bytes(encoded, LZMA1_PROPS);
        let decoded = lzma_call(
            "_decode_filter_properties",
            &[int(FILTER_LZMA1 as i64), encoded],
        );
        assert!(!exc::pending());
        let pairs = value::as_dict(decoded).expect("dict");
        let field = |name: &str| dict_lookup(pairs, name).map(number);
        assert_eq!(field("id"), Some(FILTER_LZMA1 as i64));
        assert_eq!(field("lc"), Some(3));
        assert_eq!(field("lp"), Some(0));
        assert_eq!(field("pb"), Some(2));
        assert_eq!(field("dict_size"), Some(8 * 1024 * 1024));

        // LZMA2 encodes its dictionary size as a single byte.
        let lzma2 = lzma_filters(&[(FILTER_LZMA2, &[("preset", int(6))])]);
        let filter = value::as_list(lzma2).expect("list")[0];
        let encoded = lzma_call("_encode_filter_properties", &[filter]);
        assert_eq!(data(encoded), vec![0x16]);
        let decoded = lzma_call(
            "_decode_filter_properties",
            &[int(FILTER_LZMA2 as i64), encoded],
        );
        let pairs = value::as_dict(decoded).expect("dict");
        assert_eq!(
            dict_lookup(pairs, "dict_size").map(number),
            Some(8 * 1024 * 1024)
        );

        // Delta properties are the distance byte; BCJ encodes a start offset.
        let delta = lzma_filters(&[(FILTER_DELTA, &[("dist", int(4))])]);
        let filter = value::as_list(delta).expect("list")[0];
        let encoded = lzma_call("_encode_filter_properties", &[filter]);
        assert_eq!(data(encoded), vec![0x03]);
        let decoded = lzma_call(
            "_decode_filter_properties",
            &[int(FILTER_DELTA as i64), encoded],
        );
        let pairs = value::as_dict(decoded).expect("dict");
        assert_eq!(dict_lookup(pairs, "dist").map(number), Some(4));

        let bcj = lzma_filters(&[(FILTER_X86, &[("start_offset", int(4))])]);
        let filter = value::as_list(bcj).expect("list")[0];
        let encoded = lzma_call("_encode_filter_properties", &[filter]);
        assert_eq!(data(encoded), vec![0x04, 0x00, 0x00, 0x00]);
        let decoded = lzma_call(
            "_decode_filter_properties",
            &[int(FILTER_X86 as i64), encoded],
        );
        let pairs = value::as_dict(decoded).expect("dict");
        assert_eq!(dict_lookup(pairs, "start_offset").map(number), Some(4));

        // Unknown filter ids are rejected before liblzma sees them.
        lzma_call("_decode_filter_properties", &[int(99), bytes(b"")]);
        assert_eq!(exc::pending_tag(), EXCEPTION as i64);
        exc::clear();
        lzma_call("_decode_filter_properties", &[int(-1), bytes(b"")]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn lzma_constants_and_check_support() {
        exc::init();
        assert_eq!(number(constant("_lzma", "FORMAT_AUTO")), FORMAT_AUTO);
        assert_eq!(number(constant("_lzma", "FORMAT_XZ")), FORMAT_XZ);
        assert_eq!(number(constant("_lzma", "FORMAT_ALONE")), FORMAT_ALONE);
        assert_eq!(number(constant("_lzma", "FORMAT_RAW")), FORMAT_RAW);
        assert_eq!(number(constant("_lzma", "CHECK_CRC64")), 4);
        assert_eq!(number(constant("_lzma", "CHECK_UNKNOWN")), 16);
        assert_eq!(
            number(constant("_lzma", "FILTER_LZMA1")),
            FILTER_LZMA1 as i64
        );
        assert_eq!(number(constant("_lzma", "PRESET_EXTREME")), PRESET_EXTREME);
        assert_eq!(number(constant("_lzma", "MF_BT4")), MF_BT4);

        for supported in [CHECK_NONE, CHECK_CRC32, CHECK_CRC64, CHECK_SHA256] {
            assert_eq!(
                lzma_call("is_check_supported", &[int(supported as i64)]),
                value::TRUE
            );
        }
        assert_eq!(
            lzma_call("is_check_supported", &[int(CHECK_ID_MAX as i64)]),
            value::FALSE
        );
        assert!(!exc::pending());
    }

    #[test]
    fn lzma_classes_construct_through_init_with_keywords() {
        exc::init();
        let class = class_value("_lzma", "LZMACompressor").expect("class");
        assert!(value::as_class(class).is_some());
        assert_eq!(class, constant("_lzma", "LZMACompressor"));
        let instance = crate::builtins::call(class, 0, ptr::null());
        assert!(!exc::pending());
        assert_eq!(value::type_name_of(instance), "LZMACompressor");
        let compressed = lzma_call("_lzma_compress", &[instance, bytes(b"hello world")]);
        let tail = lzma_call("_lzma_flush", &[instance]);
        let mut whole = data(compressed);
        whole.extend(data(tail));
        assert_eq!(whole, XZ_HELLO);

        // The `_streams.DecompressReader` path builds the decompressor with
        // keyword arguments (`LZMADecompressor(format=..., filters=...)`).
        let decompress_class = class_value("_lzma", "LZMADecompressor").expect("class");
        let name = value::str_alloc("format".to_string());
        let argument = int_constant(FORMAT_ALONE);
        let keywords = [name, argument];
        let decompressor =
            crate::builtins::call_kw(decompress_class, 0, ptr::null(), 1, keywords.as_ptr());
        assert!(!exc::pending());
        assert_eq!(value::type_name_of(decompressor), "LZMADecompressor");
        assert_bytes(
            lzma_call("_lzma_decompress", &[decompressor, bytes(ALONE_HELLO)]),
            b"hello world",
        );

        assert_eq!(class_value("_lzma", "LZMAError"), Some(lzma_error_class()));
    }

    #[test]
    fn unknown_names_are_attribute_errors() {
        exc::init();
        call("nope", &[]);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        exc::clear();
        constant("_bz2", "nope");
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        exc::clear();
    }
}
