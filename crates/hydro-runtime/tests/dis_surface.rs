//! Integration tests for the `dis` surface: `sys.byteorder`, `io.StringIO`,
//! `io.BytesIO` and runtime-evaluated (dynamic) format specs.
//!
//! `dis.Instruction.__str__` formats into an `io.StringIO`, `dis.dis` uses
//! `StringIO` as a context manager and cache argreprs use `sys.byteorder`.
//! Every value here is driven through the public ABI.

use hydro_runtime::abi::*;

static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

const NONE: i64 = 1;
const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;

fn int(n: i64) -> i64 {
    n << 3
}

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn bytes(value: &[u8]) -> i64 {
    hydro_bytes_new(value.as_ptr(), value.len())
}

fn call(module: &str, function: &str, args: &[i64]) -> i64 {
    hydro_module_call(
        module.as_ptr(),
        module.len(),
        function.as_ptr(),
        function.len(),
        args.len(),
        args.as_ptr(),
    )
}

fn get(module: &str, name: &str) -> i64 {
    hydro_module_get(module.as_ptr(), module.len(), name.as_ptr(), name.len())
}

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    hydro_method(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

fn getattr(obj: i64, name: &str) -> i64 {
    hydro_getattr(obj, name.as_ptr(), name.len())
}

fn same(left: i64, right: i64) -> bool {
    call("operator", "eq", &[left, right]) == hydro_true()
}

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert!(
        same(actual, text(expected)),
        "string mismatch: expected {expected:?}, got {:?}",
        hydro_to_str(actual)
    );
}

fn assert_bytes(actual: i64, expected: &[u8]) {
    assert_no_error();
    assert!(
        same(actual, bytes(expected)),
        "bytes mismatch, got {:?}",
        hydro_to_str(actual)
    );
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(hydro_to_int(actual), expected << 3);
}

fn assert_error(actual: i64, tag: i64, message: &str) {
    assert_eq!(actual, NONE, "a failing call must return None");
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    assert_eq!(hydro_exc_tag(), tag, "wrong exception tag");
    assert!(
        same(hydro_to_str(hydro_exc_value()), text(message)),
        "message mismatch: expected {message:?}, got {:?}",
        hydro_to_str(hydro_exc_value())
    );
    hydro_exc_clear();
}

#[test]
fn sys_byteorder_reports_the_target_endianness() {
    let _guard = test_guard();
    hydro_runtime_init();
    let byteorder = get("sys", "byteorder");
    assert_no_error();
    let expected = if cfg!(target_endian = "little") {
        "little"
    } else {
        "big"
    };
    assert_str(byteorder, expected);
}

#[test]
fn string_io_write_read_getvalue_seek_tell() {
    let _guard = test_guard();
    hydro_runtime_init();
    let stream = call("io", "StringIO", &[]);
    assert_no_error();

    assert_int(method(stream, "write", &[text("hello")]), 5);
    assert_int(method(stream, "tell", &[]), 5);
    assert_str(method(stream, "getvalue", &[]), "hello");

    // Reading starts at the current position (end of the data).
    assert_str(method(stream, "read", &[]), "");
    assert_int(method(stream, "seek", &[int(0)]), 0);
    assert_str(method(stream, "read", &[int(2)]), "he");
    assert_int(method(stream, "tell", &[]), 2);
    assert_str(method(stream, "read", &[]), "llo");

    // A write at a seek position overwrites in place and extends the stream.
    assert_int(method(stream, "seek", &[int(0)]), 0);
    assert_int(method(stream, "write", &[text("HELLO WORLD")]), 11);
    assert_int(method(stream, "tell", &[]), 11);
    assert_str(method(stream, "getvalue", &[]), "HELLO WORLD");

    // Seeking past the end pads with NUL on the next write, like CPython.
    assert_int(method(stream, "seek", &[int(13)]), 13);
    assert_int(method(stream, "write", &[text("!")]), 1);
    assert_str(method(stream, "getvalue", &[]), "HELLO WORLD\0\0!");

    // readline stops after the newline; size limits are honoured.
    let lines = call("io", "StringIO", &[text("a\nbc\n")]);
    assert_no_error();
    assert_str(method(lines, "readline", &[]), "a\n");
    assert_str(method(lines, "readline", &[]), "bc\n");
    assert_str(method(lines, "readline", &[]), "");
    assert_int(method(stream, "seek", &[int(-1), int(2)]), 13);
    assert_str(method(stream, "read", &[]), "!");

    // Non-string writes and negative absolute seeks raise the CPython errors.
    assert_error(
        method(stream, "write", &[int(1)]),
        TYPE_ERROR,
        "string argument expected, got 'int'",
    );
    assert_error(
        method(stream, "seek", &[int(-1)]),
        VALUE_ERROR,
        "negative seek position -1",
    );
    assert_str(method(stream, "getvalue", &[]), "HELLO WORLD\0\0!");
}

#[test]
fn string_io_context_manager_and_closed_errors() {
    let _guard = test_guard();
    hydro_runtime_init();
    let stream = call("io", "StringIO", &[]);
    assert_no_error();
    let entered = method(stream, "__enter__", &[]);
    assert!(same(entered, stream), "__enter__ must return self");
    method(stream, "write", &[text("body")]);
    assert_eq!(getattr(stream, "closed"), hydro_false());
    assert_eq!(
        method(stream, "__exit__", &[NONE, NONE, NONE]),
        hydro_false()
    );
    assert_eq!(getattr(stream, "closed"), hydro_true());

    assert_error(
        method(stream, "getvalue", &[]),
        VALUE_ERROR,
        "I/O operation on closed file.",
    );
    assert_error(
        method(stream, "write", &[text("x")]),
        VALUE_ERROR,
        "I/O operation on closed file.",
    );

    // The class value is cached, so repeated lookups share identity and the
    // class itself can be called.
    let class = get("io", "StringIO");
    assert_eq!(class, get("io", "StringIO"), "class identity is cached");
    assert_no_error();
}

#[test]
fn bytes_io_roundtrip() {
    let _guard = test_guard();
    hydro_runtime_init();
    let stream = call("io", "BytesIO", &[bytes(b"abcdef")]);
    assert_no_error();
    assert_bytes(method(stream, "read", &[int(2)]), b"ab");
    assert_int(method(stream, "tell", &[]), 2);
    assert_int(method(stream, "seek", &[int(0), int(2)]), 6);
    assert_int(method(stream, "seek", &[int(0)]), 0);
    assert_bytes(method(stream, "read", &[]), b"abcdef");
    assert_bytes(method(stream, "getvalue", &[]), b"abcdef");

    assert_int(method(stream, "seek", &[int(3)]), 3);
    assert_int(method(stream, "write", &[bytes(b"XY")]), 2);
    assert_bytes(method(stream, "getvalue", &[]), b"abcXYf");

    let lines = call("io", "BytesIO", &[bytes(b"a\nb\n")]);
    assert_no_error();
    assert_bytes(method(lines, "readline", &[]), b"a\n");
    assert_bytes(method(lines, "readline", &[]), b"b\n");
    assert_bytes(method(lines, "readline", &[]), b"");

    assert_error(
        method(stream, "write", &[text("x")]),
        TYPE_ERROR,
        "a bytes-like object is required, not 'str'",
    );
}

#[test]
fn dynamic_format_specs_are_evaluated_by_the_runtime() {
    let _guard = test_guard();
    hydro_runtime_init();
    // The f-string lowering emits `format(value, spec, conversion)`; the
    // dynamic spec is a runtime string. This exercises the same 3-argument
    // builtin path.
    let format = hydro_builtin_function(b"format".as_ptr(), 6);
    assert_no_error();
    let call_format = |args: &[i64]| {
        hydro_call(
            format,
            args.len(),
            if args.is_empty() {
                std::ptr::null()
            } else {
                args.as_ptr()
            },
        )
    };

    assert_str(call_format(&[int(42), text(">4d"), int(0)]), "  42");
    assert_str(call_format(&[int(42), text("04d"), int(0)]), "0042");
    assert_str(
        call_format(&[hydro_float_new(std::f64::consts::PI), text(".2f"), int(0)]),
        "3.14",
    );
    assert_str(call_format(&[text("x"), text("^3"), int(0)]), " x ");
    assert_str(call_format(&[text("x"), text(">5"), int(2)]), "  'x'");
    // The spec is parsed at call time, so an invalid runtime spec raises.
    assert_error(
        call_format(&[int(1), text("q"), int(0)]),
        VALUE_ERROR,
        "Unknown format code 'q' for object of type 'int'",
    );
}
