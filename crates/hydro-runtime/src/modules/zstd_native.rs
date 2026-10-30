//! `_zstd`: the compiled Zstandard codec behind `compression.zstd`.
//!
//! The streams are the real system `libzstd` (linked through `-lzstd`): every
//! frame this module emits is a normal Zstandard (RFC 8878) frame, and every
//! frame it reads is decoded by the library, including frames produced by the
//! CLI or another language binding. Nothing is reimplemented and no
//! compressed data is synthesised.
//!
//! ## Surface (CPython 3.14's `Modules/_zstd`)
//!
//! * `ZstdCompressor(level=None, options=None, zstd_dict=None)` with
//!   `compress(data, mode=CONTINUE)`, `flush(mode=FLUSH_FRAME)`,
//!   `set_pledged_input_size(size)`, the `last_mode` member and the
//!   `CONTINUE`/`FLUSH_BLOCK`/`FLUSH_FRAME` class attributes.
//!   `level` and `options` are mutually exclusive, exactly like the C module.
//! * `ZstdDecompressor(zstd_dict=None, options=None)` with
//!   `decompress(data, max_length=-1)` and the `eof`/`needs_input`/
//!   `unused_data` members. `decompress` after the frame ended raises
//!   `EOFError`, and `max_length >= 0` limits one call's output while leaving
//!   the remainder buffered in the decoder.
//! * `ZstdDict(dict_content, /, *, is_raw=False)` with the `dict_content` and
//!   `dict_id` members and the `as_digested_dict`/`as_undigested_dict`/
//!   `as_prefix` `(dict, mode)` pairs accepted by both codecs.
//! * `ZstdError` (an `Exception` subclass), `get_frame_size`,
//!   `get_frame_info`, `train_dict`, `finalize_dict`, `get_param_bounds`,
//!   `set_parameter_types`, `zstd_version`, `zstd_version_number`, every
//!   `ZSTD_*` parameter/strategy constant and `ZSTD_DStreamOutSize`.
//!
//! The pure-Python `compression.zstd` package sits on top: its `compress`,
//! `decompress`, `ZstdFile`/`open` and `FrameInfo` helpers are that module's
//! code driving these classes and functions.
//!
//! ## Native state
//!
//! Codec instances carry an integer handle in a private attribute; the actual
//! `ZSTD_CCtx`/`ZSTD_DCtx` lives in a process-wide registry guarded by a
//! mutex. `ZSTD_freeCCtx`/`ZSTD_freeDCtx` run when a registry entry is
//! dropped, so a finished decompressor frees its context before `eof` is
//! reported. Hydro has no finaliser hook for native instances, so an
//! abandoned compressor keeps its allocation until process exit (the same
//! policy as `zlib`/`_bz2`/`_lzma` here).
//!
//! ## Failure policy
//!
//! Bad argument types raise `TypeError`, rejected options raise `ValueError`,
//! and every failure reported by libzstd raises `ZstdError` with CPython's
//! wording. No input can panic.

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_uint, c_ulonglong, c_void};
use std::sync::{LazyLock, Mutex, MutexGuard};

use hydro_ir::MethodKind;

use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, EOF_ERROR, EXCEPTION, MEMORY_ERROR, RUNTIME_ERROR, TYPE_ERROR,
    VALUE_ERROR,
};
use crate::heap;
use crate::value::{self, NONE};

const MODULE: &str = "_zstd";

/// Hidden instance attributes. The leading NUL keeps them out of the normal
/// attribute namespace, like the payload attributes of other native classes.
const CODEC_ATTR: &str = "\u{0}zstd.codec";
const DICT_CONTENT_ATTR: &str = "\u{0}zstd.dict.content";
const DICT_ID_ATTR: &str = "\u{0}zstd.dict.id";

// -------------------------------------------------------------------------
// Constants (values from `zstd.h`, libzstd 1.5.x)
// -------------------------------------------------------------------------

const ZSTD_CLEVEL_DEFAULT: i64 = 3;

const ZSTD_C_COMPRESSION_LEVEL: c_int = 100;
const ZSTD_C_WINDOW_LOG: c_int = 101;
const ZSTD_C_HASH_LOG: c_int = 102;
const ZSTD_C_CHAIN_LOG: c_int = 103;
const ZSTD_C_SEARCH_LOG: c_int = 104;
const ZSTD_C_MIN_MATCH: c_int = 105;
const ZSTD_C_TARGET_LENGTH: c_int = 106;
const ZSTD_C_STRATEGY: c_int = 107;
const ZSTD_C_ENABLE_LDM: c_int = 160;
const ZSTD_C_LDM_HASH_LOG: c_int = 161;
const ZSTD_C_LDM_MIN_MATCH: c_int = 162;
const ZSTD_C_LDM_BUCKET_SIZE_LOG: c_int = 163;
const ZSTD_C_LDM_HASH_RATE_LOG: c_int = 164;
const ZSTD_C_CONTENT_SIZE_FLAG: c_int = 200;
const ZSTD_C_CHECKSUM_FLAG: c_int = 201;
const ZSTD_C_DICT_ID_FLAG: c_int = 202;
const ZSTD_C_NB_WORKERS: c_int = 400;
const ZSTD_C_JOB_SIZE: c_int = 401;
const ZSTD_C_OVERLAP_LOG: c_int = 402;
const ZSTD_D_WINDOW_LOG_MAX: c_int = 100;

const ZSTD_STRATEGY_FAST: i64 = 1;
const ZSTD_STRATEGY_DFAST: i64 = 2;
const ZSTD_STRATEGY_GREEDY: i64 = 3;
const ZSTD_STRATEGY_LAZY: i64 = 4;
const ZSTD_STRATEGY_LAZY2: i64 = 5;
const ZSTD_STRATEGY_BTLAZY2: i64 = 6;
const ZSTD_STRATEGY_BTOPT: i64 = 7;
const ZSTD_STRATEGY_BTULTRA: i64 = 8;
const ZSTD_STRATEGY_BTULTRA2: i64 = 9;

/// `ZSTD_EndDirective`.
const MODE_CONTINUE: c_int = 0;
const MODE_FLUSH_BLOCK: c_int = 1;
const MODE_FLUSH_FRAME: c_int = 2;

/// `ZSTD_dictContentType_e` / `ZSTD_dictLoadMethod_e`.
const DCT_AUTO: c_int = 0;
const DCT_RAW_CONTENT: c_int = 1;
const DCT_FULL_DICT: c_int = 2;
const DLM_BY_COPY: c_int = 0;

const ZSTD_CONTENTSIZE_UNKNOWN: u64 = u64::MAX;
const ZSTD_CONTENTSIZE_ERROR: u64 = u64::MAX - 1;

// -------------------------------------------------------------------------
// libzstd FFI
// -------------------------------------------------------------------------

/// `ZSTD_inBuffer` / `ZSTD_outBuffer` (`zstd.h`).
#[repr(C)]
struct InBuffer {
    src: *const c_void,
    size: usize,
    pos: usize,
}

#[repr(C)]
struct OutBuffer {
    dst: *mut c_void,
    size: usize,
    pos: usize,
}

/// `ZSTD_bounds`.
#[repr(C)]
struct Bounds {
    error: usize,
    lower: c_int,
    upper: c_int,
}

/// `ZDICT_params_t`.
#[repr(C)]
struct ZdictParams {
    compression_level: c_int,
    notification_level: c_uint,
    dict_id: c_uint,
}

#[link(name = "zstd")]
extern "C" {
    fn ZSTD_versionString() -> *const c_char;
    fn ZSTD_versionNumber() -> c_uint;
    fn ZSTD_isError(code: usize) -> c_uint;
    fn ZSTD_getErrorName(code: usize) -> *const c_char;

    fn ZSTD_createCCtx() -> *mut c_void;
    fn ZSTD_freeCCtx(cctx: *mut c_void) -> usize;
    fn ZSTD_CCtx_setParameter(cctx: *mut c_void, param: c_int, value: c_int) -> usize;
    fn ZSTD_CCtx_setPledgedSrcSize(cctx: *mut c_void, size: c_ulonglong) -> usize;
    fn ZSTD_CCtx_loadDictionary(cctx: *mut c_void, dict: *const c_void, size: usize) -> usize;
    fn ZSTD_CCtx_loadDictionary_advanced(
        cctx: *mut c_void,
        dict: *const c_void,
        size: usize,
        load_method: c_int,
        content_type: c_int,
    ) -> usize;
    fn ZSTD_CCtx_refPrefix_advanced(
        cctx: *mut c_void,
        prefix: *const c_void,
        size: usize,
        content_type: c_int,
    ) -> usize;
    fn ZSTD_compressStream2(
        cctx: *mut c_void,
        output: *mut OutBuffer,
        input: *mut InBuffer,
        end_op: c_int,
    ) -> usize;
    fn ZSTD_CStreamOutSize() -> usize;

    fn ZSTD_createDCtx() -> *mut c_void;
    fn ZSTD_freeDCtx(dctx: *mut c_void) -> usize;
    fn ZSTD_DCtx_setParameter(dctx: *mut c_void, param: c_int, value: c_int) -> usize;
    fn ZSTD_DCtx_loadDictionary(dctx: *mut c_void, dict: *const c_void, size: usize) -> usize;
    fn ZSTD_DCtx_loadDictionary_advanced(
        dctx: *mut c_void,
        dict: *const c_void,
        size: usize,
        load_method: c_int,
        content_type: c_int,
    ) -> usize;
    fn ZSTD_DCtx_refPrefix_advanced(
        dctx: *mut c_void,
        prefix: *const c_void,
        size: usize,
        content_type: c_int,
    ) -> usize;
    fn ZSTD_decompressStream(
        dctx: *mut c_void,
        output: *mut OutBuffer,
        input: *mut InBuffer,
    ) -> usize;
    fn ZSTD_DStreamOutSize() -> usize;

    fn ZSTD_getFrameContentSize(src: *const c_void, size: usize) -> c_ulonglong;
    fn ZSTD_getDictID_fromFrame(src: *const c_void, size: usize) -> c_uint;
    fn ZSTD_getDictID_fromDict(src: *const c_void, size: usize) -> c_uint;
    fn ZSTD_findFrameCompressedSize(src: *const c_void, size: usize) -> usize;

    fn ZSTD_cParam_getBounds(param: c_int) -> Bounds;
    fn ZSTD_dParam_getBounds(param: c_int) -> Bounds;

    fn ZDICT_trainFromBuffer(
        dict: *mut c_void,
        dict_capacity: usize,
        samples: *const c_void,
        sample_sizes: *const usize,
        sample_count: c_uint,
    ) -> usize;
    fn ZDICT_finalizeDictionary(
        dict: *mut c_void,
        dict_capacity: usize,
        custom: *const c_void,
        custom_size: usize,
        samples: *const c_void,
        sample_sizes: *const usize,
        sample_count: c_uint,
        params: ZdictParams,
    ) -> usize;
    fn ZDICT_isError(code: usize) -> c_uint;
    fn ZDICT_getErrorName(code: usize) -> *const c_char;
}

fn zstd_is_error(code: usize) -> bool {
    // SAFETY: `ZSTD_isError` only inspects the status value.
    unsafe { ZSTD_isError(code) != 0 }
}

fn zstd_status_name(code: usize) -> String {
    // SAFETY: libzstd returns a static NUL-terminated error name.
    let pointer = unsafe { ZSTD_getErrorName(code) };
    if pointer.is_null() {
        format!("zstd error {code}")
    } else {
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    }
}

fn zdict_status_name(code: usize) -> String {
    // SAFETY: libzstd returns a static NUL-terminated error name.
    let pointer = unsafe { ZDICT_getErrorName(code) };
    if pointer.is_null() {
        format!("zdict error {code}")
    } else {
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    }
}

// -------------------------------------------------------------------------
// Errors and argument helpers
// -------------------------------------------------------------------------

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn memory_error() -> i64 {
    exc::raise_with_message(MEMORY_ERROR, "Unable to allocate a zstd context")
}

fn missing_attribute(module: &str, name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module '{module}' has no attribute '{name}'"),
    )
}

fn cached_class(key: &str, build: impl FnOnce() -> i64) -> i64 {
    if let Some(class) = globals::registry_get(key) {
        return class;
    }
    let class = build();
    globals::registry_set(key, class);
    class
}

/// `_zstd.ZstdError`: a subclass of `Exception`, so `except ZstdError` and
/// `except Exception` both catch it.
fn error_class() -> i64 {
    cached_class("_zstd.ZstdError", || {
        value::class_new(
            "ZstdError".to_string(),
            exc::exception_class(EXCEPTION),
            Vec::new(),
        )
    })
}

/// Raises `ZstdError(message)` and returns the `None` dummy value.
fn zstd_error(message: &str) -> i64 {
    let message_value = value::str_alloc(message.to_string());
    let class = error_class();
    let instance = crate::builtins::call(class, 1, [message_value].as_ptr());
    if exc::pending() {
        return NONE;
    }
    exc::raise_value(instance)
}

/// Reads a positional slot; omitted optional slots arrive as `None`.
fn argument(args: &[i64], index: usize) -> i64 {
    args.get(index).copied().unwrap_or(NONE)
}

fn int_value(value_arg: i64) -> Result<i64, i64> {
    value::as_integer(value_arg).ok_or_else(|| {
        type_error(&format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(value_arg)
        ))
    })
}

/// Any bytes-like object (`bytes`, `bytearray`, `memoryview`).
fn bytes_like(value_arg: i64) -> Result<Vec<u8>, i64> {
    if let Some(bytes) = value::as_bytes(value_arg) {
        return Ok(bytes.to_vec());
    }
    if let Some(bytes) = value::as_bytearray(value_arg) {
        return Ok(bytes.clone());
    }
    if let Some(view) = value::as_memoryview(value_arg) {
        return value::memoryview_bytes(view)
            .map_err(|message| type_error(&format!("a bytes-like object is required: {message}")));
    }
    Err(type_error(&format!(
        "a bytes-like object is required, not '{}'",
        value::type_name(value_arg)
    )))
}

/// The `train_dict`/`finalize_dict` sample buffers are exact `bytes`.
fn exact_bytes(value_arg: i64, function: &str, position: usize) -> Result<Vec<u8>, i64> {
    value::as_bytes(value_arg)
        .map(|bytes| bytes.to_vec())
        .ok_or_else(|| {
            type_error(&format!(
                "{function}() argument {position} must be bytes, not {}",
                value::type_name(value_arg)
            ))
        })
}

fn int_constant(n: i64) -> i64 {
    value::encode_int(n).unwrap_or(NONE)
}

fn parameter_name(is_compress: bool, parameter: c_int) -> &'static str {
    if is_compress {
        match parameter {
            ZSTD_C_COMPRESSION_LEVEL => "compression_level",
            ZSTD_C_WINDOW_LOG => "window_log",
            ZSTD_C_HASH_LOG => "hash_log",
            ZSTD_C_CHAIN_LOG => "chain_log",
            ZSTD_C_SEARCH_LOG => "search_log",
            ZSTD_C_MIN_MATCH => "min_match",
            ZSTD_C_TARGET_LENGTH => "target_length",
            ZSTD_C_STRATEGY => "strategy",
            ZSTD_C_ENABLE_LDM => "enable_long_distance_matching",
            ZSTD_C_LDM_HASH_LOG => "ldm_hash_log",
            ZSTD_C_LDM_MIN_MATCH => "ldm_min_match",
            ZSTD_C_LDM_BUCKET_SIZE_LOG => "ldm_bucket_size_log",
            ZSTD_C_LDM_HASH_RATE_LOG => "ldm_hash_rate_log",
            ZSTD_C_CONTENT_SIZE_FLAG => "content_size_flag",
            ZSTD_C_CHECKSUM_FLAG => "checksum_flag",
            ZSTD_C_DICT_ID_FLAG => "dict_id_flag",
            ZSTD_C_NB_WORKERS => "nb_workers",
            ZSTD_C_JOB_SIZE => "job_size",
            ZSTD_C_OVERLAP_LOG => "overlap_log",
            _ => "unknown parameter",
        }
    } else {
        match parameter {
            ZSTD_D_WINDOW_LOG_MAX => "window_log_max",
            _ => "unknown parameter",
        }
    }
}

fn parameter_bounds(is_compress: bool, parameter: c_int) -> Result<(c_int, c_int), i64> {
    let bounds = if is_compress {
        // SAFETY: `ZSTD_cParam_getBounds` only reads the enum value.
        unsafe { ZSTD_cParam_getBounds(parameter) }
    } else {
        // SAFETY: `ZSTD_dParam_getBounds` only reads the enum value.
        unsafe { ZSTD_dParam_getBounds(parameter) }
    };
    if bounds.error != 0 {
        return Err(value_error(&format!(
            "invalid {} parameter 'unknown parameter (key {parameter})'",
            if is_compress {
                "compression"
            } else {
                "decompression"
            }
        )));
    }
    Ok((bounds.lower, bounds.upper))
}

fn range_error(
    is_compress: bool,
    parameter: c_int,
    value_arg: i64,
    lower: c_int,
    upper: c_int,
) -> i64 {
    if is_compress && parameter == ZSTD_C_COMPRESSION_LEVEL {
        return value_error(&format!(
            "illegal compression level {value_arg}; the valid range is [{lower}, {upper}]"
        ));
    }
    value_error(&format!(
        "{} parameter '{}' received an illegal value {value_arg}; the valid range is [{lower}, {upper}]",
        if is_compress {
            "compression"
        } else {
            "decompression"
        },
        parameter_name(is_compress, parameter)
    ))
}

/// Applies one `options` pair, mirroring CPython's `set_parameter` helper.
fn set_parameter(
    is_compress: bool,
    context: *mut c_void,
    key: i64,
    value_arg: i64,
) -> Result<(), i64> {
    let Ok(parameter) = c_int::try_from(key) else {
        // PyLong_AsInt failure surfaces as OverflowError in CPython.
        return Err(exc::raise_with_message(
            exc::OVERFLOW_ERROR,
            "signed integer is greater than maximum",
        ));
    };
    let Ok(value_int) = c_int::try_from(value_arg) else {
        return Err(exc::raise_with_message(
            exc::OVERFLOW_ERROR,
            "signed integer is greater than maximum",
        ));
    };
    let bounds = parameter_bounds(is_compress, parameter)?;
    let (lower, upper) = bounds;
    // libzstd silently clamps several parameters (flags, level); only the
    // compression level is range-checked here, exactly like CPython. Every
    // other parameter reports a rejection through `setParameter`.
    if is_compress
        && parameter == ZSTD_C_COMPRESSION_LEVEL
        && (value_int < lower || value_int > upper)
    {
        return Err(range_error(is_compress, parameter, value_arg, lower, upper));
    }
    // SAFETY: the context is live and owned by the caller; `setParameter`
    // copies the value into the context.
    let status = unsafe {
        if is_compress {
            ZSTD_CCtx_setParameter(context, parameter, value_int)
        } else {
            ZSTD_DCtx_setParameter(context, parameter, value_int)
        }
    };
    if zstd_is_error(status) {
        return Err(range_error(is_compress, parameter, value_arg, lower, upper));
    }
    Ok(())
}

fn options_pairs(class_name: &str, options_arg: i64) -> Result<Vec<(i64, i64)>, i64> {
    let Some(pairs) = value::as_dict(options_arg) else {
        return Err(type_error(&format!(
            "{class_name}() argument 'options' must be dict, not {}",
            value::type_name(options_arg)
        )));
    };
    let mut parsed = Vec::with_capacity(pairs.len());
    for &(key, value_arg) in pairs {
        let key = int_value(key)?;
        let value_arg = int_value(value_arg)?;
        parsed.push((key, value_arg));
    }
    Ok(parsed)
}

// -------------------------------------------------------------------------
// Codec registry
// -------------------------------------------------------------------------

struct CompressorState {
    context: *mut c_void,
    last_mode: c_int,
    /// Buffer referenced by `ZSTD_CCtx_refPrefix_advanced`; the vector's heap
    /// allocation stays fixed when the state moves into the registry.
    prefix: Vec<u8>,
}

impl Drop for CompressorState {
    fn drop(&mut self) {
        if !self.context.is_null() {
            // SAFETY: `context` is the live `ZSTD_CCtx` owned by this value.
            unsafe { ZSTD_freeCCtx(self.context) };
            self.context = std::ptr::null_mut();
        }
    }
}

struct DecompressorState {
    context: *mut c_void,
    eof: bool,
    needs_input: bool,
    unused: Vec<u8>,
    prefix: Vec<u8>,
    /// Input bytes the decoder did not consume before `max_length` stopped a
    /// call; CPython keeps them inside the object for the next `decompress`.
    pending: Vec<u8>,
}

impl Drop for DecompressorState {
    fn drop(&mut self) {
        if !self.context.is_null() {
            // SAFETY: `context` is the live `ZSTD_DCtx` owned by this value.
            unsafe { ZSTD_freeDCtx(self.context) };
            self.context = std::ptr::null_mut();
        }
    }
}

enum Codec {
    Compressor(CompressorState),
    Decompressor(DecompressorState),
}

// SAFETY: a codec is only touched while the registry mutex is held, so its raw
// libzstd pointers never cross threads unsynchronised.
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

fn register(codec: Codec) -> u64 {
    let mut registry = registry();
    let handle = registry.next_handle;
    registry.next_handle = handle.wrapping_add(1).max(1);
    registry.codecs.insert(handle, codec);
    handle
}

fn handle_of(instance: i64) -> Result<u64, i64> {
    match value::instance_attr(instance, CODEC_ATTR).and_then(value::as_integer) {
        Some(handle) if handle > 0 => Ok(handle as u64),
        _ => Err(exc::raise_with_message(
            RUNTIME_ERROR,
            "inconsistent codec state",
        )),
    }
}

fn attach_handle(instance: i64, handle: u64) {
    value::instance_setattr(
        instance,
        CODEC_ATTR.to_string(),
        int_constant(handle as i64),
    );
}

fn store_compressor_attributes(instance: i64, last_mode: c_int) {
    value::instance_setattr(
        instance,
        "last_mode".to_string(),
        int_constant(last_mode as i64),
    );
}

fn store_decompressor_attributes(instance: i64, eof: bool, needs_input: bool, unused: Vec<u8>) {
    let unused = value::bytes_new(unused);
    let roots = heap::root_values(&[unused]);
    value::instance_setattr(instance, "eof".to_string(), value::bool_value(eof));
    value::instance_setattr(
        instance,
        "needs_input".to_string(),
        value::bool_value(needs_input),
    );
    value::instance_setattr(instance, "unused_data".to_string(), unused);
    heap::release_roots(roots);
}

// -------------------------------------------------------------------------
// Dictionary arguments
// -------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum DictMode {
    /// `ZSTD_dct_auto`: full dictionary when it starts with the magic.
    Auto,
    /// `ZSTD_dct_fullDict`: the content must be a complete dictionary.
    Digested,
    /// CPython's `as_undigested_dict`: load through `ZSTD_dct_auto` and let
    /// the library digest the content (no pre-digested header claim).
    Undigested,
    /// `ZSTD_CCtx_refPrefix_advanced` semantics.
    Prefix,
}

/// Returns the raw dictionary bytes stored on a `ZstdDict` instance.
fn dict_content_of(value_arg: i64) -> Option<Vec<u8>> {
    value::instance_attr(value_arg, DICT_CONTENT_ATTR)
        .and_then(|content| value::as_bytes(content).map(|bytes| bytes.to_vec()))
}

/// Binds `zstd_dict`, accepting a `ZstdDict` or one of the
/// `(ZstdDict, mode)` pairs the `as_*_dict`/`as_prefix` members return.
fn dict_argument(value_arg: i64) -> Result<Option<(Vec<u8>, DictMode)>, i64> {
    if value_arg == NONE {
        return Ok(None);
    }
    if let Some(items) = value::as_tuple(value_arg) {
        if items.len() == 2 {
            if let (Some(content), Some(mode)) =
                (dict_content_of(items[0]), value::as_integer(items[1]))
            {
                let mode = match mode {
                    0 => DictMode::Digested,
                    1 => DictMode::Undigested,
                    2 => DictMode::Prefix,
                    _ => {
                        return Err(type_error(
                            "zstd_dict argument should be a ZstdDict object.",
                        ))
                    }
                };
                return Ok(Some((content, mode)));
            }
        }
    } else if let Some(content) = dict_content_of(value_arg) {
        return Ok(Some((content, DictMode::Auto)));
    }
    Err(type_error(
        "zstd_dict argument should be a ZstdDict object.",
    ))
}

// -------------------------------------------------------------------------
// `ZstdCompressor`
// -------------------------------------------------------------------------

fn param(name: &str, kind: hydro_ir::ParamKind, default: Option<i64>) -> value::ParamMeta {
    value::ParamMeta {
        name: name.to_string(),
        kind,
        default,
    }
}

fn self_param() -> value::ParamMeta {
    param("self", hydro_ir::ParamKind::PosOnly, None)
}

fn keyword_param(name: &str, default: Option<i64>) -> value::ParamMeta {
    param(name, hydro_ir::ParamKind::Positional, default)
}

/// A native method with full parameter metadata, so the runtime binder fills
/// defaults and accepts keywords.
fn method_function(
    name: &'static str,
    signature: Vec<value::ParamMeta>,
    entry: extern "C" fn(*const i64) -> i64,
) -> i64 {
    let function = value::native_function_new(MODULE, name, usize::MAX);
    value::function_set_signature(function, signature);
    value::function_set_full_entry(function, entry as *const () as usize);
    function
}

fn compressor_class() -> i64 {
    cached_class("_zstd.ZstdCompressor", || {
        let mut init_signature = vec![self_param()];
        init_signature.push(keyword_param("level", Some(NONE)));
        init_signature.push(keyword_param("options", Some(NONE)));
        init_signature.push(keyword_param("zstd_dict", Some(NONE)));
        let init = method_function(
            "_zstd_compressor_init",
            init_signature,
            compressor_init_entry,
        );
        let compress = method_function(
            "_zstd_compress",
            vec![
                self_param(),
                param("data", hydro_ir::ParamKind::Positional, None),
                keyword_param("mode", Some(int_constant(MODE_CONTINUE as i64))),
            ],
            compressor_compress_entry,
        );
        let flush = method_function(
            "_zstd_flush",
            vec![
                self_param(),
                keyword_param("mode", Some(int_constant(MODE_FLUSH_FRAME as i64))),
            ],
            compressor_flush_entry,
        );
        let pledge = method_function(
            "_zstd_set_pledged_input_size",
            vec![self_param(), keyword_param("size", None)],
            compressor_pledge_entry,
        );
        let methods = vec![
            ("__init__".to_string(), init, MethodKind::Normal),
            ("compress".to_string(), compress, MethodKind::Normal),
            ("flush".to_string(), flush, MethodKind::Normal),
            (
                "set_pledged_input_size".to_string(),
                pledge,
                MethodKind::Normal,
            ),
        ];
        let attrs = vec![
            ("CONTINUE".to_string(), int_constant(MODE_CONTINUE as i64)),
            (
                "FLUSH_BLOCK".to_string(),
                int_constant(MODE_FLUSH_BLOCK as i64),
            ),
            (
                "FLUSH_FRAME".to_string(),
                int_constant(MODE_FLUSH_FRAME as i64),
            ),
        ];
        value::class_new_full("ZstdCompressor".to_string(), NONE, methods, attrs)
    })
}

fn compressor_new(level: i64, options: i64, zstd_dict: i64) -> i64 {
    let instance = value::instance_new(compressor_class());
    match init_compressor(instance, level, options, zstd_dict) {
        Ok(()) => instance,
        Err(error) => error,
    }
}

fn init_compressor(
    instance: i64,
    level_arg: i64,
    options_arg: i64,
    dict_arg: i64,
) -> Result<(), i64> {
    let level = if level_arg == NONE {
        None
    } else {
        Some(
            value::as_integer(level_arg)
                .ok_or_else(|| type_error("invalid type for level, expected int"))?,
        )
    };
    if level.is_some() && options_arg != NONE {
        return Err(type_error("Only one of level or options should be used."));
    }

    // SAFETY: `ZSTD_createCCtx` has no preconditions.
    let context = unsafe { ZSTD_createCCtx() };
    if context.is_null() {
        return Err(memory_error());
    }

    let outcome = (|| -> Result<(), i64> {
        if let Some(level) = level {
            let (lower, upper) = parameter_bounds(true, ZSTD_C_COMPRESSION_LEVEL)?;
            if level < i64::from(lower) || level > i64::from(upper) {
                return Err(value_error(&format!(
                    "illegal compression level {level}; the valid range is [{lower}, {upper}]"
                )));
            }
            // SAFETY: the context is fresh and owned by this function.
            let status = unsafe {
                ZSTD_CCtx_setParameter(context, ZSTD_C_COMPRESSION_LEVEL, level as c_int)
            };
            if zstd_is_error(status) {
                return Err(zstd_error(&format!(
                    "Unable to set the compression level: {}",
                    zstd_status_name(status)
                )));
            }
        }
        if options_arg != NONE {
            for (key, value_arg) in options_pairs("ZstdCompressor", options_arg)? {
                set_parameter(true, context, key, value_arg)?;
            }
        }
        Ok(())
    })();
    if let Err(error) = outcome {
        // SAFETY: the context is still owned here and was not registered.
        unsafe { ZSTD_freeCCtx(context) };
        return Err(error);
    }

    let dict = match dict_argument(dict_arg) {
        Ok(dict) => dict,
        Err(error) => {
            // SAFETY: the context is still owned here and was not registered.
            unsafe { ZSTD_freeCCtx(context) };
            return Err(error);
        }
    };
    let mut state = CompressorState {
        context,
        last_mode: MODE_FLUSH_FRAME,
        prefix: Vec::new(),
    };
    if let Some((content, mode)) = dict {
        if mode == DictMode::Prefix {
            state.prefix = content;
            // SAFETY: the prefix buffer lives in `state.prefix`, whose heap
            // allocation outlives the context; libzstd copies the reference
            // only for the duration of the current frame.
            let status = unsafe {
                ZSTD_CCtx_refPrefix_advanced(
                    state.context,
                    state.prefix.as_ptr() as *const c_void,
                    state.prefix.len(),
                    DCT_RAW_CONTENT,
                )
            };
            if zstd_is_error(status) {
                // SAFETY: `state` still owns the context; drop frees it.
                return Err(zstd_error(&format!(
                    "Unable to load the Zstandard dictionary: {}",
                    zstd_status_name(status)
                )));
            }
        } else {
            let (content_type, advanced) = match mode {
                DictMode::Auto | DictMode::Undigested => (DCT_AUTO, false),
                DictMode::Digested => (DCT_FULL_DICT, true),
                DictMode::Prefix => unreachable!(),
            };
            // SAFETY: the context is live; `content` is a valid buffer.
            let status = unsafe {
                if advanced {
                    ZSTD_CCtx_loadDictionary_advanced(
                        state.context,
                        content.as_ptr() as *const c_void,
                        content.len(),
                        DLM_BY_COPY,
                        content_type,
                    )
                } else {
                    ZSTD_CCtx_loadDictionary(
                        state.context,
                        content.as_ptr() as *const c_void,
                        content.len(),
                    )
                }
            };
            if zstd_is_error(status) {
                return Err(zstd_error(&format!(
                    "Unable to load the Zstandard dictionary: {}",
                    zstd_status_name(status)
                )));
            }
        }
    }
    let handle = register(Codec::Compressor(state));
    attach_handle(instance, handle);
    store_compressor_attributes(instance, MODE_FLUSH_FRAME);
    Ok(())
}

extern "C" fn compressor_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the class binder passes the signature's four slots.
    let instance = unsafe { *argv };
    let level = unsafe { *argv.add(1) };
    let options = unsafe { *argv.add(2) };
    let zstd_dict = unsafe { *argv.add(3) };
    match init_compressor(instance, level, options, zstd_dict) {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

/// Feeds `data` to the streaming compressor and collects every produced byte.
fn compress_pump(state: &mut CompressorState, data: &[u8], mode: c_int) -> Result<Vec<u8>, i64> {
    let mut out = Vec::new();
    // SAFETY: `ZSTD_CStreamOutSize` has no preconditions.
    let chunk = unsafe { ZSTD_CStreamOutSize() }.max(64);
    let mut position = 0usize;
    loop {
        let mut buffer = vec![0u8; chunk];
        let mut output = OutBuffer {
            dst: buffer.as_mut_ptr() as *mut c_void,
            size: buffer.len(),
            pos: 0,
        };
        let mut input = InBuffer {
            src: data.as_ptr() as *const c_void,
            size: data.len(),
            pos: position,
        };
        // SAFETY: the context and both buffers are live for the call.
        let status = unsafe { ZSTD_compressStream2(state.context, &mut output, &mut input, mode) };
        if zstd_is_error(status) {
            return Err(zstd_error(&format!(
                "Unable to compress Zstandard data: {}",
                zstd_status_name(status)
            )));
        }
        position = input.pos;
        out.extend_from_slice(&buffer[..output.pos]);
        if position < data.len() {
            continue;
        }
        // `ZSTD_e_continue` may keep data buffered; `flush`/`end` must run
        // until the library reports an empty internal buffer.
        if mode != MODE_CONTINUE && status != 0 {
            continue;
        }
        break;
    }
    Ok(out)
}

fn compressor_compress(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("compress() missing required argument 'self'");
    };
    let data_arg = argument(args, 1);
    if data_arg == NONE {
        return type_error("compress() missing required argument 'data' (pos 1)");
    }
    let mode = match argument(args, 2) {
        NONE => i64::from(MODE_CONTINUE),
        value_arg => match int_value(value_arg) {
            Ok(mode) => mode,
            Err(error) => return error,
        },
    };
    if !(MODE_CONTINUE as i64..=MODE_FLUSH_FRAME as i64).contains(&mode) {
        return value_error(
            "mode argument wrong value, it should be one of ZstdCompressor.CONTINUE, \
             ZstdCompressor.FLUSH_BLOCK, ZstdCompressor.FLUSH_FRAME.",
        );
    }
    let mode = mode as c_int;
    let data = match bytes_like(data_arg) {
        Ok(data) => data,
        Err(error) => return error,
    };
    let handle = match handle_of(instance) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let outcome = {
        let mut registry = registry();
        match registry.codecs.get_mut(&handle) {
            Some(Codec::Compressor(state)) => {
                let outcome = compress_pump(state, &data, mode);
                state.last_mode = mode;
                outcome.map(|out| (out, state.last_mode))
            }
            _ => Err(exc::raise_with_message(
                RUNTIME_ERROR,
                "inconsistent codec state",
            )),
        }
    };
    match outcome {
        Ok((out, last_mode)) => {
            store_compressor_attributes(instance, last_mode);
            value::bytes_new(out)
        }
        Err(error) => error,
    }
}

extern "C" fn compressor_compress_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes the signature's three slots.
    let instance = unsafe { *argv };
    let data = unsafe { *argv.add(1) };
    let mode = unsafe { *argv.add(2) };
    compressor_compress(&[instance, data, mode])
}

fn compressor_flush(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("flush() missing required argument 'self'");
    };
    let mode = match argument(args, 1) {
        NONE => i64::from(MODE_FLUSH_FRAME),
        value_arg => match int_value(value_arg) {
            Ok(mode) => mode,
            Err(error) => return error,
        },
    };
    if mode != i64::from(MODE_FLUSH_BLOCK) && mode != i64::from(MODE_FLUSH_FRAME) {
        return value_error(
            "mode argument wrong value, it should be ZstdCompressor.FLUSH_FRAME or \
             ZstdCompressor.FLUSH_BLOCK.",
        );
    }
    let mode = mode as c_int;
    let handle = match handle_of(instance) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let outcome = {
        let mut registry = registry();
        match registry.codecs.get_mut(&handle) {
            Some(Codec::Compressor(state)) => {
                let outcome = compress_pump(state, &[], mode);
                state.last_mode = mode;
                outcome.map(|out| (out, state.last_mode))
            }
            _ => Err(exc::raise_with_message(
                RUNTIME_ERROR,
                "inconsistent codec state",
            )),
        }
    };
    match outcome {
        Ok((out, last_mode)) => {
            store_compressor_attributes(instance, last_mode);
            value::bytes_new(out)
        }
        Err(error) => error,
    }
}

extern "C" fn compressor_flush_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes the signature's two slots.
    let instance = unsafe { *argv };
    let mode = unsafe { *argv.add(1) };
    compressor_flush(&[instance, mode])
}

fn compressor_set_pledged(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("set_pledged_input_size() missing required argument 'self'");
    };
    let size_arg = argument(args, 1);
    if size_arg == NONE {
        return type_error("set_pledged_input_size() missing required argument 'size' (pos 1)");
    }
    let Some(size) = value::as_integer(size_arg) else {
        return type_error("an integer is required");
    };
    if size <= 0 || size == i64::MAX {
        return value_error(
            "size argument should be a positive int less than 18446744073709551614",
        );
    }
    let handle = match handle_of(instance) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let mut registry = registry();
    let Some(Codec::Compressor(state)) = registry.codecs.get_mut(&handle) else {
        return exc::raise_with_message(RUNTIME_ERROR, "inconsistent codec state");
    };
    if state.last_mode != MODE_FLUSH_FRAME {
        return value_error(
            "set_pledged_input_size() method must be called when last_mode == FLUSH_FRAME",
        );
    }
    // SAFETY: the context is live and owned by the registry entry.
    let status = unsafe { ZSTD_CCtx_setPledgedSrcSize(state.context, size as c_ulonglong) };
    if zstd_is_error(status) {
        return zstd_error(&format!(
            "Unable to set the pledged input size: {}",
            zstd_status_name(status)
        ));
    }
    NONE
}

extern "C" fn compressor_pledge_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes the signature's two slots.
    let instance = unsafe { *argv };
    let size = unsafe { *argv.add(1) };
    compressor_set_pledged(&[instance, size])
}

// -------------------------------------------------------------------------
// `ZstdDecompressor`
// -------------------------------------------------------------------------

fn decompressor_class() -> i64 {
    cached_class("_zstd.ZstdDecompressor", || {
        let init = method_function(
            "_zstd_decompressor_init",
            vec![
                self_param(),
                keyword_param("zstd_dict", Some(NONE)),
                keyword_param("options", Some(NONE)),
            ],
            decompressor_init_entry,
        );
        let decompress = method_function(
            "_zstd_decompress",
            vec![
                self_param(),
                param("data", hydro_ir::ParamKind::Positional, None),
                keyword_param("max_length", Some(int_constant(-1))),
            ],
            decompressor_decompress_entry,
        );
        let methods = vec![
            ("__init__".to_string(), init, MethodKind::Normal),
            ("decompress".to_string(), decompress, MethodKind::Normal),
        ];
        value::class_new_full("ZstdDecompressor".to_string(), NONE, methods, Vec::new())
    })
}

fn decompressor_new(zstd_dict: i64, options: i64) -> i64 {
    let instance = value::instance_new(decompressor_class());
    match init_decompressor(instance, zstd_dict, options) {
        Ok(()) => instance,
        Err(error) => error,
    }
}

fn init_decompressor(instance: i64, dict_arg: i64, options_arg: i64) -> Result<(), i64> {
    // SAFETY: `ZSTD_createDCtx` has no preconditions.
    let context = unsafe { ZSTD_createDCtx() };
    if context.is_null() {
        return Err(memory_error());
    }
    if options_arg != NONE {
        let outcome = (|| -> Result<(), i64> {
            for (key, value_arg) in options_pairs("ZstdDecompressor", options_arg)? {
                set_parameter(false, context, key, value_arg)?;
            }
            Ok(())
        })();
        if let Err(error) = outcome {
            // SAFETY: the context is still owned here and was not registered.
            unsafe { ZSTD_freeDCtx(context) };
            return Err(error);
        }
    }
    let dict = match dict_argument(dict_arg) {
        Ok(dict) => dict,
        Err(error) => {
            // SAFETY: the context is still owned here and was not registered.
            unsafe { ZSTD_freeDCtx(context) };
            return Err(error);
        }
    };
    let mut state = DecompressorState {
        context,
        eof: false,
        needs_input: true,
        unused: Vec::new(),
        prefix: Vec::new(),
        pending: Vec::new(),
    };
    if let Some((content, mode)) = dict {
        if mode == DictMode::Prefix {
            state.prefix = content;
            // SAFETY: the prefix buffer lives in `state.prefix` and remains
            // valid while the decoder references it.
            let status = unsafe {
                ZSTD_DCtx_refPrefix_advanced(
                    state.context,
                    state.prefix.as_ptr() as *const c_void,
                    state.prefix.len(),
                    DCT_RAW_CONTENT,
                )
            };
            if zstd_is_error(status) {
                return Err(zstd_error(&format!(
                    "Unable to load the Zstandard dictionary: {}",
                    zstd_status_name(status)
                )));
            }
        } else {
            let (content_type, advanced) = match mode {
                DictMode::Auto | DictMode::Undigested => (DCT_AUTO, false),
                DictMode::Digested => (DCT_FULL_DICT, true),
                DictMode::Prefix => unreachable!(),
            };
            // SAFETY: the context is live; `content` is a valid buffer.
            let status = unsafe {
                if advanced {
                    ZSTD_DCtx_loadDictionary_advanced(
                        state.context,
                        content.as_ptr() as *const c_void,
                        content.len(),
                        DLM_BY_COPY,
                        content_type,
                    )
                } else {
                    ZSTD_DCtx_loadDictionary(
                        state.context,
                        content.as_ptr() as *const c_void,
                        content.len(),
                    )
                }
            };
            if zstd_is_error(status) {
                return Err(zstd_error(&format!(
                    "Unable to load the Zstandard dictionary: {}",
                    zstd_status_name(status)
                )));
            }
        }
    }
    let handle = register(Codec::Decompressor(state));
    attach_handle(instance, handle);
    store_decompressor_attributes(instance, false, true, Vec::new());
    Ok(())
}

extern "C" fn decompressor_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the class binder passes the signature's three slots.
    let instance = unsafe { *argv };
    let zstd_dict = unsafe { *argv.add(1) };
    let options = unsafe { *argv.add(2) };
    match init_decompressor(instance, zstd_dict, options) {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

/// Runs one `decompress(data, max_length)` call. Unconsumed input is kept in
/// the state so a later call (even with `data=b""`) resumes the frame, exactly
/// like CPython's decoder object.
fn decompress_pump(
    state: &mut DecompressorState,
    data: &[u8],
    max_length: i64,
) -> Result<(Vec<u8>, bool, bool, Vec<u8>), i64> {
    if state.eof {
        return Err(exc::raise_with_message(
            EOF_ERROR,
            "Already at the end of a Zstandard frame.",
        ));
    }
    let limited = max_length >= 0;
    if limited && max_length == 0 {
        // A zero-length request consumes nothing; the bytes stay buffered.
        state.pending.extend_from_slice(data);
        state.needs_input = false;
        return Ok((
            Vec::new(),
            state.eof,
            state.needs_input,
            state.unused.clone(),
        ));
    }

    let mut input_buffer = Vec::with_capacity(state.pending.len() + data.len());
    input_buffer.extend_from_slice(&state.pending);
    input_buffer.extend_from_slice(data);
    state.pending.clear();

    let mut out = Vec::new();
    let mut position = 0usize;
    // SAFETY: `ZSTD_DStreamOutSize` has no preconditions.
    let chunk = unsafe { ZSTD_DStreamOutSize() }.max(64);
    loop {
        let want = if limited {
            let remaining = (max_length as usize).saturating_sub(out.len());
            if remaining == 0 {
                break;
            }
            remaining.min(chunk)
        } else {
            chunk
        };
        let mut buffer = vec![0u8; want];
        let mut output = OutBuffer {
            dst: buffer.as_mut_ptr() as *mut c_void,
            size: buffer.len(),
            pos: 0,
        };
        let mut input = InBuffer {
            src: input_buffer.as_ptr() as *const c_void,
            size: input_buffer.len(),
            pos: position,
        };
        // SAFETY: the context and both buffers are live for the call.
        let status = unsafe { ZSTD_decompressStream(state.context, &mut output, &mut input) };
        if zstd_is_error(status) {
            return Err(zstd_error(&format!(
                "Unable to decompress Zstandard data: {}",
                zstd_status_name(status)
            )));
        }
        position = input.pos;
        out.extend_from_slice(&buffer[..output.pos]);
        if status == 0 {
            // The frame ended. Anything left in the buffer belongs to the
            // next frame and is reported through `unused_data`.
            state.eof = true;
            state.needs_input = false;
            state.unused = input_buffer[position..].to_vec();
            state.pending.clear();
            // The decoder can no longer be used; release it now.
            if !state.context.is_null() {
                // SAFETY: the context is live and owned by this state; the
                // pointer is cleared so `Drop` does not free it twice.
                unsafe { ZSTD_freeDCtx(state.context) };
                state.context = std::ptr::null_mut();
            }
            break;
        }
        if output.pos < output.size {
            // The decoder wants more input to make progress.
            state.needs_input = position == input_buffer.len();
            break;
        }
        if limited && out.len() >= max_length as usize {
            state.needs_input = false;
            break;
        }
    }
    if !state.eof {
        state.pending = input_buffer[position..].to_vec();
        if !state.pending.is_empty() {
            state.needs_input = false;
        }
    }
    Ok((out, state.eof, state.needs_input, state.unused.clone()))
}

fn decompressor_decompress(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("decompress() missing required argument 'self'");
    };
    let data_arg = argument(args, 1);
    if data_arg == NONE {
        return type_error("decompress() missing required argument 'data' (pos 1)");
    }
    let max_length = match argument(args, 2) {
        NONE => -1,
        value_arg => match int_value(value_arg) {
            Ok(max_length) => max_length,
            Err(error) => return error,
        },
    };
    let data = match bytes_like(data_arg) {
        Ok(data) => data,
        Err(error) => return error,
    };
    let handle = match handle_of(instance) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let outcome = {
        let mut registry = registry();
        match registry.codecs.get_mut(&handle) {
            Some(Codec::Decompressor(state)) => decompress_pump(state, &data, max_length),
            _ => Err(exc::raise_with_message(
                RUNTIME_ERROR,
                "inconsistent codec state",
            )),
        }
    };
    match outcome {
        Ok((out, eof, needs_input, unused)) => {
            store_decompressor_attributes(instance, eof, needs_input, unused);
            value::bytes_new(out)
        }
        Err(error) => error,
    }
}

extern "C" fn decompressor_decompress_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes the signature's three slots.
    let instance = unsafe { *argv };
    let data = unsafe { *argv.add(1) };
    let max_length = unsafe { *argv.add(2) };
    decompressor_decompress(&[instance, data, max_length])
}

// -------------------------------------------------------------------------
// `ZstdDict`
// -------------------------------------------------------------------------

fn dict_class() -> i64 {
    cached_class("_zstd.ZstdDict", || {
        let init = method_function(
            "_zstd_dict_init",
            vec![
                self_param(),
                param("dict_content", hydro_ir::ParamKind::PosOnly, None),
                param("is_raw", hydro_ir::ParamKind::KwOnly, Some(value::FALSE)),
            ],
            dict_init_entry,
        );
        let repr = value::native_function_new(MODULE, "_zstd_dict_repr", 1);
        let methods = vec![
            ("__init__".to_string(), init, MethodKind::Normal),
            ("__repr__".to_string(), repr, MethodKind::Normal),
        ];
        let attrs = vec![
            (
                "dict_content".to_string(),
                value::property_new(
                    value::native_function_new(MODULE, "_zstd_dict_content", 1),
                    NONE,
                    NONE,
                ),
            ),
            (
                "dict_id".to_string(),
                value::property_new(
                    value::native_function_new(MODULE, "_zstd_dict_id", 1),
                    NONE,
                    NONE,
                ),
            ),
            (
                "as_digested_dict".to_string(),
                value::property_new(
                    value::native_function_new(MODULE, "_zstd_dict_digested", 1),
                    NONE,
                    NONE,
                ),
            ),
            (
                "as_undigested_dict".to_string(),
                value::property_new(
                    value::native_function_new(MODULE, "_zstd_dict_undigested", 1),
                    NONE,
                    NONE,
                ),
            ),
            (
                "as_prefix".to_string(),
                value::property_new(
                    value::native_function_new(MODULE, "_zstd_dict_prefix", 1),
                    NONE,
                    NONE,
                ),
            ),
        ];
        value::class_new_full("ZstdDict".to_string(), NONE, methods, attrs)
    })
}

fn store_dict_attributes(instance: i64, content: Vec<u8>, dict_id: u32) {
    let content = value::bytes_new(content);
    let roots = heap::root_values(&[content]);
    value::instance_setattr(instance, DICT_CONTENT_ATTR.to_string(), content);
    value::instance_setattr(
        instance,
        DICT_ID_ATTR.to_string(),
        int_constant(i64::from(dict_id)),
    );
    heap::release_roots(roots);
}

fn dict_new(content: Vec<u8>, is_raw: bool) -> Result<i64, i64> {
    if content.len() < 8 {
        return Err(value_error(
            "Zstandard dictionary content too short (must have at least eight bytes)",
        ));
    }
    // SAFETY: the buffer is valid for `content.len()` bytes.
    let dict_id =
        unsafe { ZSTD_getDictID_fromDict(content.as_ptr() as *const c_void, content.len()) };
    if !is_raw && dict_id == 0 {
        return Err(value_error("invalid Zstandard dictionary"));
    }
    let instance = value::instance_new(dict_class());
    store_dict_attributes(instance, content, dict_id);
    Ok(instance)
}

fn init_dict(instance: i64, content_arg: i64, is_raw: i64) -> Result<(), i64> {
    if content_arg == NONE {
        return Err(type_error(
            "ZstdDict() missing required argument 'dict_content' (pos 1)",
        ));
    }
    let content = bytes_like(content_arg)?;
    let is_raw = is_raw != NONE && value::truthy(is_raw);
    if content.len() < 8 {
        return Err(value_error(
            "Zstandard dictionary content too short (must have at least eight bytes)",
        ));
    }
    // SAFETY: the buffer is valid for `content.len()` bytes.
    let dict_id =
        unsafe { ZSTD_getDictID_fromDict(content.as_ptr() as *const c_void, content.len()) };
    if !is_raw && dict_id == 0 {
        return Err(value_error("invalid Zstandard dictionary"));
    }
    store_dict_attributes(instance, content, dict_id);
    Ok(())
}

extern "C" fn dict_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the class binder passes the signature's three slots.
    let instance = unsafe { *argv };
    let content = unsafe { *argv.add(1) };
    let is_raw = unsafe { *argv.add(2) };
    match init_dict(instance, content, is_raw) {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

fn dict_content(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("dict_content descriptor missing 'self'");
    };
    value::instance_attr(instance, DICT_CONTENT_ATTR).unwrap_or(NONE)
}

fn dict_id(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("dict_id descriptor missing 'self'");
    };
    value::instance_attr(instance, DICT_ID_ATTR).unwrap_or(NONE)
}

fn dict_view(args: &[i64], mode: i64) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("dictionary view descriptor missing 'self'");
    };
    let roots = heap::root_values(&[instance]);
    let mode = int_constant(mode);
    let result = value::tuple_new(vec![instance, mode]);
    heap::release_roots(roots);
    result
}

fn dict_repr(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("__repr__() missing required argument 'self'");
    };
    let dict_id = value::instance_attr(instance, DICT_ID_ATTR)
        .and_then(value::as_integer)
        .unwrap_or(0);
    let size = value::instance_attr(instance, DICT_CONTENT_ATTR)
        .and_then(value::as_bytes)
        .map(<[u8]>::len)
        .unwrap_or(0);
    value::str_alloc(format!("<ZstdDict dict_id={dict_id} dict_size={size}>"))
}

// -------------------------------------------------------------------------
// Frame inspection, dictionaries and parameters
// -------------------------------------------------------------------------

fn get_frame_size(args: &[i64]) -> i64 {
    let data_arg = argument(args, 0);
    if data_arg == NONE {
        return type_error("get_frame_size() missing required argument 'frame_buffer' (pos 1)");
    }
    let data = match bytes_like(data_arg) {
        Ok(data) => data,
        Err(error) => return error,
    };
    // SAFETY: the buffer is valid for `data.len()` bytes.
    let status =
        unsafe { ZSTD_findFrameCompressedSize(data.as_ptr() as *const c_void, data.len()) };
    if zstd_is_error(status) {
        return zstd_error(&format!(
            "Error when finding the compressed size of a Zstandard frame. Ensure the \
             frame_buffer argument starts from the beginning of a frame, and its length is \
             not less than this complete frame. Zstd error message: {}",
            zstd_status_name(status)
        ));
    }
    int_constant(status as i64)
}

fn get_frame_info(args: &[i64]) -> i64 {
    let data_arg = argument(args, 0);
    if data_arg == NONE {
        return type_error("get_frame_info() missing required argument 'frame_buffer' (pos 1)");
    }
    let data = match bytes_like(data_arg) {
        Ok(data) => data,
        Err(error) => return error,
    };
    // SAFETY: the buffer is valid for `data.len()` bytes.
    let size = unsafe { ZSTD_getFrameContentSize(data.as_ptr() as *const c_void, data.len()) };
    if size == ZSTD_CONTENTSIZE_ERROR {
        return zstd_error(
            "Error when getting information from the header of a Zstandard frame. Ensure the \
             frame_buffer argument starts from the beginning of a frame, and its length is not \
             less than the frame header (6~18 bytes).",
        );
    }
    // SAFETY: the buffer is valid for `data.len()` bytes.
    let dict_id = unsafe { ZSTD_getDictID_fromFrame(data.as_ptr() as *const c_void, data.len()) };
    let size = if size == ZSTD_CONTENTSIZE_UNKNOWN {
        NONE
    } else {
        int_constant(size as i64)
    };
    let roots = heap::root_values(&[size]);
    let result = value::tuple_new(vec![size, int_constant(i64::from(dict_id))]);
    heap::release_roots(roots);
    result
}

/// Splits `(samples_bytes, samples_sizes)` into the flat buffer and the
/// `size_t` array `ZDICT_*` expects, validating the tuple contract.
fn sample_buffers(
    function: &str,
    samples_arg: i64,
    sizes_arg: i64,
    samples_position: usize,
    sizes_position: usize,
) -> Result<(Vec<u8>, Vec<usize>), i64> {
    if samples_arg == NONE {
        return Err(type_error(&format!(
            "{function}() missing required argument 'samples_bytes' (pos {samples_position})"
        )));
    }
    let samples = exact_bytes(samples_arg, function, samples_position)?;
    if sizes_arg == NONE {
        return Err(type_error(&format!(
            "{function}() missing required argument 'samples_sizes' (pos {sizes_position})"
        )));
    }
    let Some(items) = value::as_tuple(sizes_arg) else {
        return Err(type_error(&format!(
            "{function}() argument {sizes_position} must be tuple, not {}",
            value::type_name(sizes_arg)
        )));
    };
    let mut sizes = Vec::with_capacity(items.len());
    let mut total: i128 = 0;
    for &item in items {
        let Some(size) = value::as_integer(item) else {
            return Err(type_error("an integer is required"));
        };
        total += i128::from(size);
        if size < 0 || total > samples.len() as i128 {
            return Err(value_error(
                "The samples size tuple doesn't match the concatenation's size.",
            ));
        }
        sizes.push(size as usize);
    }
    if total != samples.len() as i128 {
        return Err(value_error(
            "The samples size tuple doesn't match the concatenation's size.",
        ));
    }
    Ok((samples, sizes))
}

fn train_dict(args: &[i64]) -> i64 {
    let (samples, sizes) =
        match sample_buffers("train_dict", argument(args, 0), argument(args, 1), 1, 2) {
            Ok(buffers) => buffers,
            Err(error) => return error,
        };
    let dict_size_arg = argument(args, 2);
    if dict_size_arg == NONE {
        return type_error("train_dict() missing required argument 'dict_size' (pos 3)");
    }
    let dict_size = match int_value(dict_size_arg) {
        Ok(size) => size,
        Err(error) => return error,
    };
    if dict_size <= 0 {
        return value_error("dict_size argument should be positive number.");
    }
    let mut dict = vec![0u8; dict_size as usize];
    // SAFETY: both buffers are valid for the lengths passed; `sizes` has one
    // entry per sample.
    let status = unsafe {
        ZDICT_trainFromBuffer(
            dict.as_mut_ptr() as *mut c_void,
            dict.len(),
            samples.as_ptr() as *const c_void,
            sizes.as_ptr(),
            sizes.len() as c_uint,
        )
    };
    // SAFETY: `ZDICT_isError` only inspects the status value.
    if unsafe { ZDICT_isError(status) } != 0 {
        return zstd_error(&format!(
            "Unable to train the Zstandard dictionary: {}",
            zdict_status_name(status)
        ));
    }
    dict.truncate(status);
    value::bytes_new(dict)
}

fn finalize_dict(args: &[i64]) -> i64 {
    let custom_arg = argument(args, 0);
    if custom_arg == NONE {
        return type_error("finalize_dict() missing required argument 'custom_dict_bytes' (pos 1)");
    }
    let custom = match exact_bytes(custom_arg, "finalize_dict", 1) {
        Ok(custom) => custom,
        Err(error) => return error,
    };
    let (samples, sizes) =
        match sample_buffers("finalize_dict", argument(args, 1), argument(args, 2), 2, 3) {
            Ok(buffers) => buffers,
            Err(error) => return error,
        };
    let dict_size_arg = argument(args, 3);
    if dict_size_arg == NONE {
        return type_error("finalize_dict() missing required argument 'dict_size' (pos 4)");
    }
    let dict_size = match int_value(dict_size_arg) {
        Ok(size) => size,
        Err(error) => return error,
    };
    if dict_size <= 0 {
        return value_error("dict_size argument should be positive number.");
    }
    let level_arg = argument(args, 4);
    if level_arg == NONE {
        return type_error("finalize_dict() missing required argument 'compression_level' (pos 5)");
    }
    let level = match int_value(level_arg) {
        Ok(level) => level,
        Err(error) => return error,
    };
    let Ok(level) = c_int::try_from(level) else {
        return value_error("compression_level argument should be int in range");
    };
    let mut dict = vec![0u8; dict_size as usize];
    let params = ZdictParams {
        compression_level: level,
        notification_level: 0,
        dict_id: 0,
    };
    // SAFETY: all buffers are valid for the lengths passed.
    let status = unsafe {
        ZDICT_finalizeDictionary(
            dict.as_mut_ptr() as *mut c_void,
            dict.len(),
            custom.as_ptr() as *const c_void,
            custom.len(),
            samples.as_ptr() as *const c_void,
            sizes.as_ptr(),
            sizes.len() as c_uint,
            params,
        )
    };
    // SAFETY: `ZDICT_isError` only inspects the status value.
    if unsafe { ZDICT_isError(status) } != 0 {
        return zstd_error(&format!(
            "Unable to finalize the Zstandard dictionary: {}",
            zdict_status_name(status)
        ));
    }
    dict.truncate(status);
    value::bytes_new(dict)
}

fn get_param_bounds(args: &[i64]) -> i64 {
    let parameter_arg = argument(args, 0);
    if parameter_arg == NONE {
        return type_error("get_param_bounds() missing required argument 'parameter' (pos 1)");
    }
    let parameter = match int_value(parameter_arg) {
        Ok(parameter) => parameter,
        Err(error) => return error,
    };
    let is_compress_arg = argument(args, 1);
    if is_compress_arg == NONE {
        return type_error("get_param_bounds() missing required argument 'is_compress' (pos 2)");
    }
    let is_compress = value::truthy(is_compress_arg);
    let Ok(parameter) = c_int::try_from(parameter) else {
        return zstd_error(&format!(
            "Unable to get zstd {} parameter bounds: {}",
            if is_compress {
                "compression"
            } else {
                "decompression"
            },
            "Unsupported parameter"
        ));
    };
    let bounds = if is_compress {
        // SAFETY: `ZSTD_cParam_getBounds` only reads the enum value.
        unsafe { ZSTD_cParam_getBounds(parameter) }
    } else {
        // SAFETY: `ZSTD_dParam_getBounds` only reads the enum value.
        unsafe { ZSTD_dParam_getBounds(parameter) }
    };
    if bounds.error != 0 || zstd_is_error(bounds.error) {
        return zstd_error(&format!(
            "Unable to get zstd {} parameter bounds: {}",
            if is_compress {
                "compression"
            } else {
                "decompression"
            },
            zstd_status_name(bounds.error)
        ));
    }
    let lower = int_constant(i64::from(bounds.lower));
    let roots = heap::root_values(&[lower]);
    let result = value::tuple_new(vec![lower, int_constant(i64::from(bounds.upper))]);
    heap::release_roots(roots);
    result
}

fn set_parameter_types(args: &[i64]) -> i64 {
    if args.len() != 2 {
        return type_error(&format!(
            "set_parameter_types() takes exactly 2 arguments ({} given)",
            args.len()
        ));
    }
    // CPython stores the two IntEnum types to render parameter names in error
    // messages; this module renders the same names from its static table, so
    // the registration only validates the arity.
    NONE
}

// -------------------------------------------------------------------------
// Module surface
// -------------------------------------------------------------------------

pub(crate) fn class_value(module: &str, name: &str) -> Option<i64> {
    if module != MODULE {
        return None;
    }
    match name {
        "ZstdCompressor" => Some(compressor_class()),
        "ZstdDecompressor" => Some(decompressor_class()),
        "ZstdDict" => Some(dict_class()),
        "ZstdError" => Some(error_class()),
        _ => None,
    }
}

pub(crate) fn constant(name: &str) -> i64 {
    if let Some(class) = class_value(MODULE, name) {
        return class;
    }
    let numeric = match name {
        "ZSTD_CLEVEL_DEFAULT" => Some(ZSTD_CLEVEL_DEFAULT),
        "ZSTD_DStreamOutSize" => {
            // SAFETY: `ZSTD_DStreamOutSize` has no preconditions.
            Some(unsafe { ZSTD_DStreamOutSize() } as i64)
        }
        "ZSTD_c_compressionLevel" => Some(i64::from(ZSTD_C_COMPRESSION_LEVEL)),
        "ZSTD_c_windowLog" => Some(i64::from(ZSTD_C_WINDOW_LOG)),
        "ZSTD_c_hashLog" => Some(i64::from(ZSTD_C_HASH_LOG)),
        "ZSTD_c_chainLog" => Some(i64::from(ZSTD_C_CHAIN_LOG)),
        "ZSTD_c_searchLog" => Some(i64::from(ZSTD_C_SEARCH_LOG)),
        "ZSTD_c_minMatch" => Some(i64::from(ZSTD_C_MIN_MATCH)),
        "ZSTD_c_targetLength" => Some(i64::from(ZSTD_C_TARGET_LENGTH)),
        "ZSTD_c_strategy" => Some(i64::from(ZSTD_C_STRATEGY)),
        "ZSTD_c_enableLongDistanceMatching" => Some(i64::from(ZSTD_C_ENABLE_LDM)),
        "ZSTD_c_ldmHashLog" => Some(i64::from(ZSTD_C_LDM_HASH_LOG)),
        "ZSTD_c_ldmMinMatch" => Some(i64::from(ZSTD_C_LDM_MIN_MATCH)),
        "ZSTD_c_ldmBucketSizeLog" => Some(i64::from(ZSTD_C_LDM_BUCKET_SIZE_LOG)),
        "ZSTD_c_ldmHashRateLog" => Some(i64::from(ZSTD_C_LDM_HASH_RATE_LOG)),
        "ZSTD_c_contentSizeFlag" => Some(i64::from(ZSTD_C_CONTENT_SIZE_FLAG)),
        "ZSTD_c_checksumFlag" => Some(i64::from(ZSTD_C_CHECKSUM_FLAG)),
        "ZSTD_c_dictIDFlag" => Some(i64::from(ZSTD_C_DICT_ID_FLAG)),
        "ZSTD_c_nbWorkers" => Some(i64::from(ZSTD_C_NB_WORKERS)),
        "ZSTD_c_jobSize" => Some(i64::from(ZSTD_C_JOB_SIZE)),
        "ZSTD_c_overlapLog" => Some(i64::from(ZSTD_C_OVERLAP_LOG)),
        "ZSTD_d_windowLogMax" => Some(i64::from(ZSTD_D_WINDOW_LOG_MAX)),
        "ZSTD_fast" => Some(ZSTD_STRATEGY_FAST),
        "ZSTD_dfast" => Some(ZSTD_STRATEGY_DFAST),
        "ZSTD_greedy" => Some(ZSTD_STRATEGY_GREEDY),
        "ZSTD_lazy" => Some(ZSTD_STRATEGY_LAZY),
        "ZSTD_lazy2" => Some(ZSTD_STRATEGY_LAZY2),
        "ZSTD_btlazy2" => Some(ZSTD_STRATEGY_BTLAZY2),
        "ZSTD_btopt" => Some(ZSTD_STRATEGY_BTOPT),
        "ZSTD_btultra" => Some(ZSTD_STRATEGY_BTULTRA),
        "ZSTD_btultra2" => Some(ZSTD_STRATEGY_BTULTRA2),
        _ => None,
    };
    if let Some(numeric) = numeric {
        return int_constant(numeric);
    }
    match name {
        "zstd_version" => {
            // SAFETY: libzstd returns a static NUL-terminated version string.
            let pointer = unsafe { ZSTD_versionString() };
            let version = if pointer.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(pointer) }
                    .to_string_lossy()
                    .into_owned()
            };
            value::str_alloc(version)
        }
        "zstd_version_number" => {
            // SAFETY: `ZSTD_versionNumber` has no preconditions.
            int_constant(i64::from(unsafe { ZSTD_versionNumber() }))
        }
        "get_frame_size" => value::native_function_new(MODULE, "get_frame_size", usize::MAX),
        "get_frame_info" => value::native_function_new(MODULE, "get_frame_info", usize::MAX),
        "train_dict" => value::native_function_new(MODULE, "train_dict", usize::MAX),
        "finalize_dict" => value::native_function_new(MODULE, "finalize_dict", usize::MAX),
        "get_param_bounds" => value::native_function_new(MODULE, "get_param_bounds", usize::MAX),
        "set_parameter_types" => {
            value::native_function_new(MODULE, "set_parameter_types", usize::MAX)
        }
        _ => missing_attribute(MODULE, name),
    }
}

/// Dispatches `_zstd` calls, including the native method bodies installed on
/// the codec classes and the descriptor getters.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    let bare = function.rsplit('.').next().unwrap_or(function);
    match bare {
        "ZstdCompressor" => compressor_new(argument(args, 0), argument(args, 1), argument(args, 2)),
        "ZstdDecompressor" => decompressor_new(argument(args, 0), argument(args, 1)),
        "ZstdDict" => {
            let content = argument(args, 0);
            if content == NONE {
                return type_error("ZstdDict() missing required argument 'dict_content' (pos 1)");
            }
            let content = match bytes_like(content) {
                Ok(content) => content,
                Err(error) => return error,
            };
            let is_raw = argument(args, 1) != NONE && value::truthy(argument(args, 1));
            match dict_new(content, is_raw) {
                Ok(instance) => instance,
                Err(error) => error,
            }
        }
        "_zstd_compressor_init" => {
            let instance = argument(args, 0);
            match init_compressor(
                instance,
                argument(args, 1),
                argument(args, 2),
                argument(args, 3),
            ) {
                Ok(()) => NONE,
                Err(error) => error,
            }
        }
        "_zstd_compress" => compressor_compress(args),
        "_zstd_flush" => compressor_flush(args),
        "_zstd_set_pledged_input_size" => compressor_set_pledged(args),
        "_zstd_decompressor_init" => {
            let instance = argument(args, 0);
            match init_decompressor(instance, argument(args, 1), argument(args, 2)) {
                Ok(()) => NONE,
                Err(error) => error,
            }
        }
        "_zstd_decompress" => decompressor_decompress(args),
        "_zstd_dict_init" => {
            match init_dict(argument(args, 0), argument(args, 1), argument(args, 2)) {
                Ok(()) => NONE,
                Err(error) => error,
            }
        }
        "_zstd_dict_content" => dict_content(args),
        "_zstd_dict_id" => dict_id(args),
        "_zstd_dict_digested" => dict_view(args, 0),
        "_zstd_dict_undigested" => dict_view(args, 1),
        "_zstd_dict_prefix" => dict_view(args, 2),
        "_zstd_dict_repr" => dict_repr(args),
        "get_frame_size" => get_frame_size(args),
        "get_frame_info" => get_frame_info(args),
        "train_dict" => train_dict(args),
        "finalize_dict" => finalize_dict(args),
        "get_param_bounds" => get_param_bounds(args),
        "set_parameter_types" => set_parameter_types(args),
        _ => missing_attribute(MODULE, function),
    }
}

/// Binds keyword arguments into the positional slot layout for the module
/// functions. The classes' methods have full signatures and arrive through
/// the runtime binder instead.
fn bind_keywords(
    function: &str,
    args: &[i64],
    keywords: &[(String, i64)],
    names: &[&str],
) -> Result<Vec<i64>, i64> {
    let mut slots = args.to_vec();
    let mut provided: Vec<bool> = vec![true; args.len()];
    provided.resize(names.len(), false);
    for (name, item) in keywords {
        let Some(index) = names.iter().position(|candidate| candidate == name) else {
            return Err(type_error(&format!(
                "{function}() got an unexpected keyword argument '{name}'"
            )));
        };
        if provided.get(index).copied().unwrap_or(true) {
            return Err(type_error(&format!(
                "{function}() got multiple values for argument '{name}'"
            )));
        }
        if slots.len() <= index {
            slots.resize(index + 1, NONE);
        }
        slots[index] = *item;
        provided[index] = true;
    }
    Ok(slots)
}

pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if keywords.is_empty() {
        return call(function, args);
    }
    let bare = function.rsplit('.').next().unwrap_or(function);
    let names: &[&str] = match bare {
        "get_frame_size" | "get_frame_info" => &["frame_buffer"],
        "get_param_bounds" => &["parameter", "is_compress"],
        "ZstdCompressor" => &["level", "options", "zstd_dict"],
        "ZstdDecompressor" => &["zstd_dict", "options"],
        "ZstdDict" => &["dict_content", "is_raw"],
        _ => {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("{MODULE}.{function}() takes no keyword arguments"),
            );
        }
    };
    match bind_keywords(bare, args, keywords, names) {
        Ok(slots) => call(bare, &slots),
        Err(error) => error,
    }
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(data: &[u8]) -> i64 {
        value::bytes_new(data.to_vec())
    }

    fn text(data: &str) -> i64 {
        value::str_alloc(data.to_string())
    }

    fn as_int(value_arg: i64) -> i64 {
        value::as_integer(value_arg).expect("int result")
    }

    fn data_of(value_arg: i64) -> Vec<u8> {
        value::as_bytes(value_arg).expect("bytes result").to_vec()
    }

    fn clear_error() -> u32 {
        let tag = exc::pending_tag() as u32;
        exc::clear();
        tag
    }

    fn compressor(level: i64) -> i64 {
        let instance = call("ZstdCompressor", &[level, NONE, NONE]);
        assert!(!exc::pending(), "compressor construction raised");
        instance
    }

    fn decompressor() -> i64 {
        let instance = call("ZstdDecompressor", &[NONE, NONE]);
        assert!(!exc::pending(), "decompressor construction raised");
        instance
    }

    fn compress_frame(comp: i64, payload: &[u8]) -> Vec<u8> {
        let out = call(
            "_zstd_compress",
            &[comp, bytes(payload), int_constant(MODE_FLUSH_FRAME as i64)],
        );
        assert!(!exc::pending(), "compress raised");
        data_of(out)
    }

    fn decompress_all(dec: i64, frame: &[u8]) -> Vec<u8> {
        let out = call("_zstd_decompress", &[dec, bytes(frame), int_constant(-1)]);
        assert!(!exc::pending(), "decompress raised");
        data_of(out)
    }

    fn trained_dictionary() -> Vec<u8> {
        let samples: Vec<Vec<u8>> = (0..120)
            .map(|index| format!("hello world sample number {index}").into_bytes())
            .collect();
        let concatenated: Vec<u8> = samples.iter().flatten().copied().collect();
        let sizes: Vec<i64> = samples.iter().map(|sample| sample.len() as i64).collect();
        let sizes_tuple = value::tuple_new(sizes.iter().copied().map(int_constant).collect());
        let result = call(
            "train_dict",
            &[bytes(&concatenated), sizes_tuple, int_constant(256)],
        );
        assert!(!exc::pending(), "train_dict raised");
        data_of(result)
    }

    #[test]
    fn constants_match_libzstd() {
        assert_eq!(as_int(constant("ZSTD_CLEVEL_DEFAULT")), 3);
        assert_eq!(as_int(constant("ZSTD_c_compressionLevel")), 100);
        assert_eq!(as_int(constant("ZSTD_c_windowLog")), 101);
        assert_eq!(as_int(constant("ZSTD_c_strategy")), 107);
        assert_eq!(as_int(constant("ZSTD_c_enableLongDistanceMatching")), 160);
        assert_eq!(as_int(constant("ZSTD_c_contentSizeFlag")), 200);
        assert_eq!(as_int(constant("ZSTD_c_checksumFlag")), 201);
        assert_eq!(as_int(constant("ZSTD_c_nbWorkers")), 400);
        assert_eq!(as_int(constant("ZSTD_c_overlapLog")), 402);
        assert_eq!(as_int(constant("ZSTD_d_windowLogMax")), 100);
        assert_eq!(as_int(constant("ZSTD_fast")), 1);
        assert_eq!(as_int(constant("ZSTD_btultra2")), 9);
        assert_eq!(as_int(constant("ZSTD_DStreamOutSize")), 131_072);
        let version = value::as_str(constant("zstd_version")).unwrap();
        assert!(
            version.starts_with(|c: char| c.is_ascii_digit()),
            "{version}"
        );
        let number = as_int(constant("zstd_version_number"));
        assert_eq!(
            format!(
                "{}.{}.{}",
                number / 10_000,
                (number / 100) % 100,
                number % 100
            ),
            version
        );
    }

    #[test]
    fn one_shot_frame_round_trip() {
        exc::init();
        // `ZSTD_compress` itself is exercised through the streaming class;
        // both paths produce frames this module then decodes.
        let comp = compressor(NONE);
        let mut payload = Vec::new();
        for index in 0..20_000u32 {
            payload.extend_from_slice(format!("record {index}: {}\n", index * 7).as_bytes());
        }
        let frame = compress_frame(comp, &payload);
        assert!(frame.len() < payload.len(), "frame should compress text");

        assert_eq!(
            as_int(call("get_frame_size", &[bytes(&frame)])),
            frame.len() as i64
        );
        let info = call("get_frame_info", &[bytes(&frame)]);
        let (size, dict_id) = (
            value::as_tuple(info).unwrap()[0],
            value::as_tuple(info).unwrap()[1],
        );
        assert_eq!(as_int(size), payload.len() as i64);
        assert_eq!(as_int(dict_id), 0);

        let dec = decompressor();
        assert_eq!(&decompress_all(dec, &frame)[..], &payload[..]);
        assert!(value::truthy(value::instance_attr(dec, "eof").unwrap()));
        assert!(!value::truthy(
            value::instance_attr(dec, "needs_input").unwrap()
        ));
        assert_eq!(
            data_of(value::instance_attr(dec, "unused_data").unwrap()).len(),
            0
        );
    }

    #[test]
    fn streaming_modes_and_max_length() {
        exc::init();
        let comp = compressor(NONE);
        // CONTINUE keeps data buffered, FLUSH_BLOCK emits a decodable block,
        // FLUSH_FRAME ends the frame.
        let first = call(
            "_zstd_compress",
            &[comp, bytes(b"alpha"), int_constant(MODE_CONTINUE as i64)],
        );
        assert!(!exc::pending());
        let second = call(
            "_zstd_compress",
            &[comp, bytes(b"beta"), int_constant(MODE_FLUSH_BLOCK as i64)],
        );
        assert!(!exc::pending());
        let tail = call(
            "_zstd_flush",
            &[comp, int_constant(MODE_FLUSH_FRAME as i64)],
        );
        assert!(!exc::pending());
        assert_eq!(
            as_int(value::instance_attr(comp, "last_mode").unwrap()),
            MODE_FLUSH_FRAME as i64
        );
        let mut frame = data_of(first);
        frame.extend_from_slice(&data_of(second));
        frame.extend_from_slice(&data_of(tail));

        let dec = decompressor();
        let out = call("_zstd_decompress", &[dec, bytes(&frame), int_constant(4)]);
        assert!(!exc::pending());
        assert_eq!(&data_of(out)[..], b"alph");
        assert!(!value::truthy(value::instance_attr(dec, "eof").unwrap()));
        assert!(!value::truthy(
            value::instance_attr(dec, "needs_input").unwrap()
        ));
        let rest = call("_zstd_decompress", &[dec, bytes(b""), int_constant(-1)]);
        assert!(!exc::pending());
        assert_eq!(&data_of(rest)[..], b"abeta");
        assert!(value::truthy(value::instance_attr(dec, "eof").unwrap()));

        // A second frame in the same buffer is reported through unused_data.
        let frame = compress_frame(compressor(1), b"payload");
        let two_frames = [frame.clone(), frame.clone()].concat();
        let dec = decompressor();
        let out = call(
            "_zstd_decompress",
            &[dec, bytes(&two_frames), int_constant(-1)],
        );
        assert!(!exc::pending());
        assert_eq!(&data_of(out)[..], b"payload");
        let unused = data_of(value::instance_attr(dec, "unused_data").unwrap());
        assert_eq!(unused.len(), two_frames.len() - frame.len());

        // Calling decompress after the frame ended raises EOFError.
        let bad = call("_zstd_decompress", &[dec, bytes(b""), int_constant(-1)]);
        assert_eq!(clear_error(), EOF_ERROR);
        assert_eq!(bad, NONE);
    }

    #[test]
    fn dictionary_round_trip_and_views() {
        exc::init();
        let raw = trained_dictionary();
        let dict = call("ZstdDict", &[bytes(&raw), NONE]);
        assert!(!exc::pending(), "ZstdDict raised");
        let dict_id = as_int(value::instance_attr(dict, DICT_ID_ATTR).unwrap());
        assert_ne!(dict_id, 0);
        assert_eq!(
            value::as_str(call("_zstd_dict_repr", &[dict])).unwrap(),
            format!("<ZstdDict dict_id={dict_id} dict_size={}>", raw.len())
        );

        let payload = b"hello world sample number 42";
        let comp = call("ZstdCompressor", &[NONE, NONE, dict]);
        assert!(!exc::pending());
        let frame = compress_frame(comp, payload);
        let info = call("get_frame_info", &[bytes(&frame)]);
        assert_eq!(as_int(value::as_tuple(info).unwrap()[1]), dict_id);

        let dec = call("ZstdDecompressor", &[dict, NONE]);
        assert!(!exc::pending());
        assert_eq!(&decompress_all(dec, &frame)[..], &payload[..]);

        // The three views pair the dictionary with its load mode; each one is
        // accepted by both codecs.
        for (view_getter, expected_mode) in [
            ("_zstd_dict_digested", 0i64),
            ("_zstd_dict_undigested", 1),
            ("_zstd_dict_prefix", 2),
        ] {
            let pair = call(view_getter, &[dict]);
            let items = value::as_tuple(pair).unwrap();
            assert_eq!(items[0], dict);
            assert_eq!(as_int(items[1]), expected_mode);
            let comp = call("ZstdCompressor", &[NONE, NONE, pair]);
            assert!(!exc::pending(), "{view_getter} compressor");
            let frame = compress_frame(comp, payload);
            let info = call("get_frame_info", &[bytes(&frame)]);
            let frame_dict_id = as_int(value::as_tuple(info).unwrap()[1]);
            if expected_mode == 2 {
                // A prefix is not recorded in the frame header.
                assert_eq!(frame_dict_id, 0, "{view_getter}");
            } else {
                assert_eq!(frame_dict_id, dict_id, "{view_getter}");
            }
            let dec = call("ZstdDecompressor", &[pair, NONE]);
            assert!(!exc::pending(), "{view_getter} decompressor");
            assert_eq!(
                &decompress_all(dec, &frame)[..],
                &payload[..],
                "{view_getter}"
            );
        }
    }

    #[test]
    fn training_and_finalize_validation() {
        exc::init();
        let raw = trained_dictionary();
        assert_eq!(raw.len(), 256);
        // A raw dictionary needs at least eight bytes.
        let short = call("ZstdDict", &[bytes(b"abc"), value::TRUE]);
        assert_eq!(clear_error(), VALUE_ERROR);
        assert_eq!(short, NONE);
        // A non-raw dictionary must carry the format header.
        let invalid = call("ZstdDict", &[bytes(b"abcdefgh"), NONE]);
        assert_eq!(clear_error(), VALUE_ERROR);
        assert_eq!(invalid, NONE);
        // Mismatched sample sizes are rejected before libzstd runs.
        let sizes = value::tuple_new(vec![int_constant(3)]);
        let bad = call("train_dict", &[bytes(b"abcdef"), sizes, int_constant(64)]);
        assert_eq!(clear_error(), VALUE_ERROR);
        assert_eq!(bad, NONE);
        let empty = call("ZstdDict", &[bytes(&raw), NONE]);
        assert!(!exc::pending());

        let custom = data_of(value::instance_attr(empty, DICT_CONTENT_ATTR).unwrap());
        let samples: Vec<Vec<u8>> = (0..120)
            .map(|index| format!("hello world sample number {index}").into_bytes())
            .collect();
        let concatenated: Vec<u8> = samples.iter().flatten().copied().collect();
        let sizes = value::tuple_new(
            samples
                .iter()
                .map(|sample| int_constant(sample.len() as i64))
                .collect(),
        );
        let finalized = call(
            "finalize_dict",
            &[
                bytes(&custom),
                bytes(&concatenated),
                sizes,
                int_constant(256),
                int_constant(3),
            ],
        );
        assert!(!exc::pending(), "finalize_dict raised");
        let finalized = data_of(finalized);
        let dict = call("ZstdDict", &[bytes(&finalized), NONE]);
        assert!(!exc::pending());
        assert_ne!(as_int(value::instance_attr(dict, DICT_ID_ATTR).unwrap()), 0);
    }

    #[test]
    fn argument_and_error_surface() {
        exc::init();
        // level and options are mutually exclusive.
        let bad = call(
            "ZstdCompressor",
            &[int_constant(1), value::dict_new(Vec::new()), NONE],
        );
        assert_eq!(clear_error(), TYPE_ERROR);
        assert_eq!(bad, NONE);
        // Out-of-range level.
        let bad = call("ZstdCompressor", &[int_constant(100), NONE, NONE]);
        assert_eq!(clear_error(), VALUE_ERROR);
        assert_eq!(bad, NONE);
        // Unknown compression parameter.
        let options = value::dict_new(vec![(int_constant(9_999), int_constant(1))]);
        let bad = call("ZstdCompressor", &[NONE, options, NONE]);
        assert_eq!(clear_error(), VALUE_ERROR);
        assert_eq!(bad, NONE);
        // Decompression parameter bounds.
        let options = value::dict_new(vec![(
            int_constant(i64::from(ZSTD_D_WINDOW_LOG_MAX)),
            int_constant(1),
        )]);
        let bad = call("ZstdDecompressor", &[NONE, options]);
        assert_eq!(clear_error(), VALUE_ERROR);
        assert_eq!(bad, NONE);
        // Bad compression mode.
        let comp = compressor(NONE);
        let bad = call("_zstd_compress", &[comp, bytes(b"x"), int_constant(7)]);
        assert_eq!(clear_error(), VALUE_ERROR);
        assert_eq!(bad, NONE);
        // flush only accepts BLOCK/FRAME.
        let bad = call("_zstd_flush", &[comp, int_constant(MODE_CONTINUE as i64)]);
        assert_eq!(clear_error(), VALUE_ERROR);
        assert_eq!(bad, NONE);
        // Pledging is only legal right after a frame boundary.
        call(
            "_zstd_compress",
            &[comp, bytes(b"x"), int_constant(MODE_CONTINUE as i64)],
        );
        let bad = call("_zstd_set_pledged_input_size", &[comp, int_constant(4)]);
        assert_eq!(clear_error(), VALUE_ERROR);
        assert_eq!(bad, NONE);
        // Invalid frame bytes raise ZstdError.
        let bad = call("get_frame_size", &[bytes(b"not a frame")]);
        assert_eq!(clear_error(), EXCEPTION);
        assert_eq!(bad, NONE);
        let bad = call("get_frame_info", &[bytes(b"no")]);
        assert_eq!(clear_error(), EXCEPTION);
        assert_eq!(bad, NONE);
        // Parameter bounds for a known and an unknown parameter.
        let bounds = call(
            "get_param_bounds",
            &[
                int_constant(i64::from(ZSTD_C_COMPRESSION_LEVEL)),
                value::TRUE,
            ],
        );
        assert!(!exc::pending());
        let items = value::as_tuple(bounds).unwrap();
        assert_eq!(as_int(items[0]), -131_072);
        assert_eq!(as_int(items[1]), 22);
        let bad = call("get_param_bounds", &[int_constant(9_999), value::TRUE]);
        assert_eq!(clear_error(), EXCEPTION);
        assert_eq!(bad, NONE);
        // `set_parameter_types` is the import-time registration hook.
        assert_eq!(call("set_parameter_types", &[text("a"), text("b")]), NONE);
        assert!(!exc::pending());
    }
}
