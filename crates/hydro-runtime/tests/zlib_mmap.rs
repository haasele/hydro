//! Integration tests for the `zlib` and `mmap` native modules.
//!
//! The tests drive the public runtime ABI exactly like generated code. They
//! assume the registration entries described in the modules' reports:
//!
//! * `zlib` and `mmap` are added to `stdlib::MODULES` with the function,
//!   constant and internal method-body specs listed there, `zlib_mod::call`
//!   and `mmap_mod::call` are wired into `modules::module_call`, and the
//!   `constant` arms are wired into `constant_value`.
//! * `modules::module_get_named` consults `zlib_mod::class_value` and
//!   `mmap_mod::class_value` before the function table so `zlib.error` and
//!   `mmap.mmap` resolve to class values.
//!
//! Slice objects are built as `slice`-class instances through the class ABI:
//! Hydro's container slice path (`hydro_slice`) has no instance dispatch, so
//! `m[a:b]` is exercised through the `__getitem__`/`__setitem__` methods that
//! `mmap.mmap` installs, which is the same code path `container::subscript`
//! takes for an integer index.

use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use hydro_runtime::abi::*;

const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;
const OVERFLOW_ERROR: i64 = 4;
const INDEX_ERROR: i64 = 6;
const ATTRIBUTE_ERROR: i64 = 9;
const EXCEPTION: i64 = 12;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn int(n: i64) -> i64 {
    n << 3
}

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn bytes(value: &[u8]) -> i64 {
    hydro_bytes_new(value.as_ptr(), value.len())
}

fn module(module: &str, function: &str, args: &[i64]) -> i64 {
    hydro_module_call(
        module.as_ptr(),
        module.len(),
        function.as_ptr(),
        function.len(),
        args.len(),
        args.as_ptr(),
    )
}

fn module_get(module: &str, name: &str) -> i64 {
    hydro_module_get(module.as_ptr(), module.len(), name.as_ptr(), name.len())
}

fn call(function: i64, args: &[i64]) -> i64 {
    hydro_call(function, args.len(), args.as_ptr())
}

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    hydro_method(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

fn getattr(obj: i64, name: &str) -> i64 {
    hydro_getattr(obj, name.as_ptr(), name.len())
}

fn eq(a: i64, b: i64) -> bool {
    hydro_binop(7, a, b) == hydro_true()
}

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(hydro_to_int(actual), hydro_to_int(int(expected)));
}

fn assert_bytes(actual: i64, expected: &[u8]) {
    assert_no_error();
    assert!(eq(actual, bytes(expected)), "bytes mismatch");
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert!(eq(actual, text(expected)), "string mismatch");
}

fn assert_true(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_true(), "expected True");
}

fn assert_false(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_false(), "expected False");
}

fn expect_error(tag: i64) {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    assert_eq!(hydro_exc_tag(), tag, "wrong exception tag");
    hydro_exc_clear();
}

/// Builds a `slice`-class instance; `mmap.mmap` recognises it by class name.
fn slice(start: i64, stop: i64, step: i64) -> i64 {
    let class = hydro_class_new(
        b"slice".as_ptr(),
        b"slice".len(),
        hydro_none(),
        0,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        0,
        std::ptr::null(),
        std::ptr::null(),
    );
    let instance = hydro_instance_new(class);
    hydro_setattr(instance, b"start".as_ptr(), b"start".len(), start);
    hydro_setattr(instance, b"stop".as_ptr(), b"stop".len(), stop);
    hydro_setattr(instance, b"step".as_ptr(), b"step".len(), step);
    instance
}

/// A temporary file with a live read/write descriptor.
struct TempFile {
    path: PathBuf,
    file: std::fs::File,
}

impl TempFile {
    fn new(tag: &str, contents: &[u8]) -> TempFile {
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hydro-zlib-mmap-{}-{tag}-{serial}",
            std::process::id()
        ));
        std::fs::write(&path, contents).expect("write temporary file");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open temporary file");
        TempFile { path, file }
    }

    fn fd(&self) -> i64 {
        i64::from(self.file.as_raw_fd())
    }

    fn contents(&self) -> Vec<u8> {
        std::fs::read(&self.path).expect("read temporary file")
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// -------------------------------------------------------------------------
// zlib
// -------------------------------------------------------------------------

#[test]
fn zlib_checksums_match_cpython() {
    hydro_runtime_init();
    assert_int(
        module("zlib", "crc32", &[bytes(b"hello world")]),
        222_957_957,
    );
    assert_int(
        module("zlib", "adler32", &[bytes(b"hello world")]),
        436_929_629,
    );
    assert_int(module("zlib", "crc32", &[bytes(b"")]), 0);
    assert_int(module("zlib", "adler32", &[bytes(b"")]), 1);
    // The optional second argument is the previous checksum.
    assert_int(
        module("zlib", "crc32", &[bytes(b"abc"), int(123)]),
        1_837_155_955,
    );
    assert_int(
        module("zlib", "adler32", &[bytes(b"abc"), int(123)]),
        62_587_297,
    );
    assert_int(module("zlib", "crc32", &[bytes(b"abc")]), 891_568_578);
    assert_int(module("zlib", "adler32", &[bytes(b"abc")]), 38_600_999);
}

#[test]
fn zlib_compress_produces_cpython_bytes() {
    hydro_runtime_init();
    let compressed = module("zlib", "compress", &[bytes(b"hello world")]);
    assert_bytes(
        compressed,
        &[
            120, 156, 203, 72, 205, 201, 201, 87, 40, 207, 47, 202, 73, 1, 0, 26, 11, 4, 93,
        ],
    );
    assert_bytes(
        module("zlib", "compress", &[bytes(b"hello world"), int(1)]),
        &[
            120, 1, 203, 72, 205, 201, 201, 87, 40, 207, 47, 202, 73, 1, 0, 26, 11, 4, 93,
        ],
    );
    assert_bytes(
        module(
            "zlib",
            "compress",
            &[bytes(b"hello world"), int(6), int(-15)],
        ),
        &[203, 72, 205, 201, 201, 87, 40, 207, 47, 202, 73, 1, 0],
    );
    assert_bytes(
        module("zlib", "compress", &[bytes(b""), int(-1)]),
        &[120, 156, 3, 0, 0, 0, 0, 1],
    );
}

#[test]
fn zlib_round_trips_every_window() {
    hydro_runtime_init();
    let payload = b"the quick brown fox jumps over the lazy dog";
    for (level, wbits, read_wbits) in [(6, 15, 15), (1, 15, 15), (9, -15, -15), (6, 31, 47)] {
        let compressed = module(
            "zlib",
            "compress",
            &[bytes(payload), int(level), int(wbits)],
        );
        assert_no_error();
        assert_bytes(
            module("zlib", "decompress", &[compressed, int(read_wbits)]),
            payload,
        );
    }
}

#[test]
fn zlib_errors_use_the_module_error_class() {
    hydro_runtime_init();
    // `zlib.error` subclasses Exception; the runtime reports tag `EXCEPTION`.
    module("zlib", "compress", &[bytes(b"x"), int(10)]);
    expect_error(EXCEPTION);
    module("zlib", "decompress", &[bytes(b"junk")]);
    expect_error(EXCEPTION);
    module("zlib", "decompress", &[bytes(b"")]);
    expect_error(EXCEPTION);
    // Invalid initialisation options are ValueErrors, like CPython.
    module("zlib", "compressobj", &[int(10)]);
    expect_error(VALUE_ERROR);
    module("zlib", "compressobj", &[int(6), int(8), int(99)]);
    expect_error(VALUE_ERROR);
    module("zlib", "decompressobj", &[int(199)]);
    expect_error(VALUE_ERROR);
    module("zlib", "nope", &[]);
    expect_error(ATTRIBUTE_ERROR);

    // The exception class is a real class value and instances match it.
    let error_class = module_get("zlib", "error");
    assert_no_error();
    let instance = call(error_class, &[text("boom")]);
    assert_no_error();
    assert_true(module("builtins", "isinstance", &[instance, error_class]));
}

#[test]
fn zlib_compressobj_streams_and_copies() {
    hydro_runtime_init();
    let object = module("zlib", "compressobj", &[int(6)]);
    assert_no_error();
    assert_bytes(method(object, "compress", &[bytes(b"hello ")]), &[120, 156]);
    assert_bytes(method(object, "compress", &[bytes(b"world")]), b"");
    assert_bytes(
        method(object, "flush", &[]),
        &[
            203, 72, 205, 201, 201, 87, 40, 207, 47, 202, 73, 1, 0, 26, 11, 4, 93,
        ],
    );

    // A finished stream reports CPython's inconsistent-state error.
    method(object, "compress", &[bytes(b"x")]);
    expect_error(EXCEPTION);

    // `copy()` resumes independently from the same state.
    let first = module("zlib", "compressobj", &[int(6)]);
    method(first, "compress", &[bytes(b"copy me")]);
    let second = method(first, "copy", &[]);
    assert_no_error();
    let from_second = method(second, "flush", &[]);
    let from_first = method(first, "flush", &[]);
    assert_no_error();
    assert!(eq(from_second, from_first));
}

#[test]
fn zlib_decompressobj_tracks_tail_and_eof() {
    hydro_runtime_init();
    let object = module("zlib", "decompressobj", &[]);
    let compressed = module("zlib", "compress", &[bytes(b"abcdef")]);
    assert_bytes(method(object, "decompress", &[compressed, int(4)]), b"abcd");
    assert_false(getattr(object, "eof"));
    let tail = getattr(object, "unconsumed_tail");
    assert_no_error();
    let tail_len = hydro_to_int(hydro_len(tail));
    assert!(
        tail_len > 0,
        "unconsumed_tail should hold the remaining input"
    );
    assert_bytes(method(object, "decompress", &[bytes(b"")]), b"ef");
    assert_true(getattr(object, "eof"));
    assert_bytes(getattr(object, "unconsumed_tail"), b"");

    // Data offered after the end of the stream is preserved as unused data.
    assert_bytes(method(object, "decompress", &[bytes(b"TRAILER")]), b"");
    assert_bytes(getattr(object, "unused_data"), b"TRAILER");
    assert_no_error();

    // `flush` is a no-op at end of stream but still validates its argument.
    assert_bytes(method(object, "flush", &[]), b"");
    method(object, "flush", &[int(0)]);
    expect_error(VALUE_ERROR);

    // A truncated stream returns the data seen so far; `flush` reports no
    // error and leaves `eof` false, matching CPython.
    let stream = module("zlib", "compress", &[bytes(b"hello world")]);
    let truncated = hydro_slice(stream, hydro_none(), int(-4), hydro_none());
    assert_no_error();
    let object = module("zlib", "decompressobj", &[]);
    assert_bytes(method(object, "decompress", &[truncated]), b"hello world");
    assert_bytes(method(object, "flush", &[]), b"");
    assert_false(getattr(object, "eof"));
}

#[test]
fn zlib_constants_and_versions() {
    hydro_runtime_init();
    assert_int(module_get("zlib", "MAX_WBITS"), 15);
    assert_int(module_get("zlib", "DEFLATED"), 8);
    assert_int(module_get("zlib", "DEF_MEM_LEVEL"), 8);
    assert_int(module_get("zlib", "Z_DEFAULT_COMPRESSION"), -1);
    assert_int(module_get("zlib", "Z_SYNC_FLUSH"), 2);
    assert_int(module_get("zlib", "Z_FINISH"), 4);
    let version = module_get("zlib", "ZLIB_VERSION");
    assert_no_error();
    assert!(hydro_to_int(hydro_len(version)) > 0);
    assert_no_error();
}

// -------------------------------------------------------------------------
// mmap
// -------------------------------------------------------------------------

#[test]
fn mmap_read_access_exposes_the_file() {
    hydro_runtime_init();
    let file = TempFile::new("read", b"hello world");
    let object = module("mmap", "mmap", &[int(file.fd()), int(0)]);
    assert_no_error();

    assert_int(method(object, "size", &[]), 11);
    assert_int(hydro_len(object), 11);
    assert_int(method(object, "tell", &[]), 0);
    assert_bytes(method(object, "read", &[int(5)]), b"hello");
    assert_int(method(object, "tell", &[]), 5);
    assert_int(method(object, "seek", &[int(6)]), 6);
    assert_bytes(method(object, "readline", &[]), b"world");
    assert_int(method(object, "seek", &[int(0)]), 0);
    assert_bytes(method(object, "read", &[int(-1)]), b"hello world");
    assert_bytes(method(object, "read", &[int(0)]), b"");

    assert_int(method(object, "seek", &[int(0)]), 0);
    assert_int(method(object, "find", &[bytes(b"world")]), 6);
    assert_int(method(object, "rfind", &[bytes(b"l")]), 9);
    assert_int(method(object, "find", &[bytes(b"absent")]), -1);

    assert_int(hydro_subscript(object, int(1)), 101);
    assert_int(hydro_subscript(object, int(-1)), 100);
    hydro_subscript(object, int(11));
    expect_error(INDEX_ERROR);

    // `repr` mirrors CPython's `<mmap.mmap ...>` format.
    assert_str(
        hydro_to_repr(object),
        "<mmap.mmap closed=False, access=ACCESS_DEFAULT, length=11, pos=0, offset=0>",
    );
    assert_false(getattr(object, "closed"));

    method(object, "close", &[]);
    assert_no_error();
    assert_true(getattr(object, "closed"));
    assert_str(hydro_to_repr(object), "<mmap.mmap closed=True>");
    method(object, "read", &[int(1)]);
    expect_error(VALUE_ERROR);
    // Closing twice is a no-op.
    method(object, "close", &[]);
    assert_no_error();
}

#[test]
fn mmap_write_access_updates_the_file() {
    hydro_runtime_init();
    let file = TempFile::new("write", b"hello world");
    let object = module(
        "mmap",
        "mmap",
        &[int(file.fd()), int(0), hydro_none(), hydro_none(), int(2)],
    );
    assert_no_error();
    assert_int(method(object, "write", &[bytes(b"HELLO")]), 5);
    assert_int(method(object, "seek", &[int(6)]), 6);
    assert_int(method(object, "write", &[bytes(b"WORLD")]), 5);
    method(object, "flush", &[]);
    assert_no_error();
    assert_eq!(file.contents(), b"HELLO WORLD".to_vec());

    // In-place moves may overlap.
    method(object, "move", &[int(0), int(6), int(5)]);
    method(object, "flush", &[]);
    assert_no_error();
    assert_eq!(file.contents(), b"WORLD WORLD".to_vec());

    // Growing the file remaps and extends the mapping.
    method(object, "resize", &[int(16)]);
    assert_no_error();
    assert_int(method(object, "size", &[]), 16);
    assert_int(method(object, "write", &[bytes(b"EXTRA")]), 5);
    method(object, "flush", &[]);
    assert_no_error();
    assert_eq!(file.contents(), b"WORLD WORLDEXTRA".to_vec());

    // Writing past the end is an error, not a silent truncation.
    method(object, "seek", &[int(14)]);
    method(object, "write", &[bytes(b"TOOLONG")]);
    expect_error(VALUE_ERROR);
    method(object, "close", &[]);
}

#[test]
fn mmap_access_copy_is_private() {
    hydro_runtime_init();
    let file = TempFile::new("copy", b"hello world");
    let object = module(
        "mmap",
        "mmap",
        &[int(file.fd()), int(0), hydro_none(), hydro_none(), int(3)],
    );
    assert_no_error();
    assert_int(method(object, "write", &[bytes(b"HELLO")]), 5);
    method(object, "flush", &[]);
    assert_no_error();
    assert_eq!(file.contents(), b"hello world".to_vec());
    method(object, "resize", &[int(4)]);
    expect_error(TYPE_ERROR);
    method(object, "close", &[]);
}

#[test]
fn mmap_readonly_mapping_rejects_writes() {
    hydro_runtime_init();
    let file = TempFile::new("readonly", b"hello");
    let object = module(
        "mmap",
        "mmap",
        &[int(file.fd()), int(0), hydro_none(), hydro_none(), int(1)],
    );
    assert_no_error();
    method(object, "write", &[bytes(b"x")]);
    expect_error(TYPE_ERROR);
    method(object, "write_byte", &[int(65)]);
    expect_error(TYPE_ERROR);
    method(object, "close", &[]);
}

#[test]
fn mmap_slices_read_and_write() {
    hydro_runtime_init();
    let private = module_get("mmap", "MAP_PRIVATE");
    let anonymous = module_get("mmap", "MAP_ANONYMOUS");
    assert_no_error();
    let flags = hydro_binop(18, private, anonymous);
    assert_no_error();
    let object = module("mmap", "mmap", &[int(-1), int(6), flags]);
    assert_no_error();
    assert_int(method(object, "write", &[bytes(b"abcdef")]), 6);

    assert_bytes(
        hydro_subscript(object, slice(hydro_none(), hydro_none(), hydro_none())),
        b"abcdef",
    );
    assert_bytes(
        hydro_subscript(object, slice(int(0), hydro_none(), int(2))),
        b"ace",
    );
    assert_bytes(
        hydro_subscript(object, slice(int(4), hydro_none(), int(-2))),
        b"eca",
    );
    assert_bytes(hydro_subscript(object, slice(int(4), int(2), int(1))), b"");

    // Step-1 slice assignment overwrites exactly the selected range.
    method(
        object,
        "__setitem__",
        &[slice(int(0), int(4), hydro_none()), bytes(b"XYZW")],
    );
    assert_no_error();
    assert_bytes(
        hydro_subscript(object, slice(int(0), int(6), hydro_none())),
        b"XYZWef",
    );

    // A slice whose length does not match the value is rejected.
    method(
        object,
        "__setitem__",
        &[slice(int(0), int(2), hydro_none()), bytes(b"Z")],
    );
    expect_error(INDEX_ERROR);
    method(object, "close", &[]);
}

#[test]
fn mmap_errors_match_cpython() {
    hydro_runtime_init();
    let file = TempFile::new("errors", b"abc");
    let fd = int(file.fd());

    // Invalid access mode.
    module(
        "mmap",
        "mmap",
        &[fd, int(0), hydro_none(), hydro_none(), int(5)],
    );
    expect_error(VALUE_ERROR);
    // Length greater than the file size.
    module("mmap", "mmap", &[fd, int(30)]);
    expect_error(VALUE_ERROR);
    // Negative length.
    module("mmap", "mmap", &[fd, int(-1)]);
    expect_error(OVERFLOW_ERROR);
    // Offset past the end of the file.
    module(
        "mmap",
        "mmap",
        &[
            fd,
            int(0),
            hydro_none(),
            hydro_none(),
            hydro_none(),
            int(16384),
        ],
    );
    expect_error(VALUE_ERROR);
    // Empty file with length 0.
    let empty = TempFile::new("empty", b"");
    module("mmap", "mmap", &[int(empty.fd()), int(0)]);
    expect_error(VALUE_ERROR);

    let object = module("mmap", "mmap", &[fd, int(0)]);
    assert_no_error();
    method(object, "seek", &[int(-1)]);
    expect_error(VALUE_ERROR);
    method(object, "seek", &[int(1), int(9)]);
    expect_error(VALUE_ERROR);
    method(object, "write_byte", &[int(300)]);
    expect_error(OVERFLOW_ERROR);
    method(object, "close", &[]);
}

#[test]
fn mmap_survives_closing_the_original_descriptor() {
    hydro_runtime_init();
    let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "hydro-zlib-mmap-{}-detach-{serial}",
        std::process::id()
    ));
    std::fs::write(&path, b"detached").expect("write temporary file");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open temporary file");
    let object = module("mmap", "mmap", &[int(i64::from(file.as_raw_fd())), int(0)]);
    assert_no_error();
    drop(file);
    assert_bytes(method(object, "read", &[int(8)]), b"detached");
    method(object, "close", &[]);
    assert_no_error();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn mmap_class_value_and_context_manager() {
    hydro_runtime_init();
    let file = TempFile::new("class", b"abc");
    let class = module_get("mmap", "mmap");
    assert_no_error();
    let object = call(class, &[int(file.fd()), int(0)]);
    assert_no_error();
    assert_true(module("builtins", "isinstance", &[object, class]));
    let entered = method(object, "__enter__", &[]);
    assert_no_error();
    assert_eq!(entered, object);
    method(
        object,
        "__exit__",
        &[hydro_none(), hydro_none(), hydro_none()],
    );
    assert_no_error();
    assert_true(getattr(object, "closed"));
}

#[test]
fn mmap_constants_are_exposed() {
    hydro_runtime_init();
    assert_int(module_get("mmap", "ACCESS_DEFAULT"), 0);
    assert_int(module_get("mmap", "ACCESS_READ"), 1);
    assert_int(module_get("mmap", "ACCESS_WRITE"), 2);
    assert_int(module_get("mmap", "ACCESS_COPY"), 3);
    let granularity = module_get("mmap", "ALLOCATIONGRANULARITY");
    assert_no_error();
    assert!(hydro_to_int(granularity) > 0);
    assert!(eq(module_get("mmap", "PAGESIZE"), granularity));
    assert_int(module_get("mmap", "MAP_SHARED"), 1);
}
