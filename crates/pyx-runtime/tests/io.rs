//! Integration tests for `io.open` and the file object (`read`, `write`,
//! `readline(s)`, iteration, `seek`/`tell`, `close` and `OSError`).
//!
//! Every test uses its own temp file and removes it on drop; the runtime is
//! driven only through the public ABI.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use pyx_runtime::abi::*;

fn text(value: &str) -> i64 {
    pyx_str_new(value.as_ptr(), value.len())
}

fn bytes(value: &[u8]) -> i64 {
    pyx_bytes_new(value.as_ptr(), value.len())
}

/// Asserts that `actual` is a `bytes` object with exactly `expected` content.
fn assert_bytes(actual: i64, expected: &[u8]) {
    assert_eq!(pyx_exc_pending(), 0, "unexpected pending exception");
    assert!(
        same(actual, bytes(expected)),
        "bytes mismatch, got {:?}",
        pyx_to_str(actual)
    );
}

fn call(module: &str, function: &str, args: &[i64]) -> i64 {
    pyx_module_call(
        module.as_ptr(),
        module.len(),
        function.as_ptr(),
        function.len(),
        args.len(),
        args.as_ptr(),
    )
}

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    pyx_method(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

fn same(a: i64, b: i64) -> bool {
    call("operator", "eq", &[a, b]) == pyx_true()
}

fn assert_str(actual: i64, expected: &str) {
    assert_eq!(pyx_exc_pending(), 0, "unexpected pending exception");
    assert!(
        same(actual, text(expected)),
        "value mismatch, got {:?}",
        pyx_to_str(actual)
    );
}

/// Compares a pending exception message without asserting that no exception
/// is pending (the message is read while the exception is still pending).
fn assert_error_text(actual: i64, expected: &str) {
    assert!(
        same(actual, text(expected)),
        "message mismatch, got {:?}",
        pyx_to_str(actual)
    );
}

fn assert_int(value: i64, expected: i64) {
    assert_eq!(pyx_to_int(value), expected << 3);
}

fn error_tag() -> i64 {
    assert_eq!(pyx_exc_pending(), 1, "expected a pending exception");
    pyx_exc_tag()
}

fn error_message() -> i64 {
    pyx_to_str(pyx_exc_value())
}

fn clear_error() {
    pyx_exc_clear();
}

/// A unique temp path removed when the test ends.
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(tag: &str) -> TempFile {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pyx_io_{}_{}_{}.txt",
            std::process::id(),
            tag,
            count
        ));
        let _ = std::fs::remove_file(&path);
        TempFile { path }
    }

    fn value(&self) -> i64 {
        text(self.path.to_str().expect("temp path is UTF-8"))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn open(path: i64, mode: &str) -> i64 {
    let file = call("io", "open", &[path, text(mode)]);
    assert_eq!(pyx_exc_pending(), 0, "open failed");
    file
}

fn close(file: i64) {
    method(file, "close", &[]);
    assert_eq!(pyx_exc_pending(), 0, "close failed");
}

#[test]
fn write_read_roundtrip() {
    pyx_runtime_init();
    let temp = TempFile::new("roundtrip");

    let file = open(temp.value(), "w");
    assert_str(
        pyx_to_str(file),
        &format!(
            "<file name='{}' mode='w' encoding='UTF-8'>",
            temp.path.display()
        ),
    );
    let written = method(file, "write", &[text("hello\nwörld\n")]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_int(written, 12);
    assert_eq!(method(file, "closed", &[]), pyx_false());
    close(file);
    assert_eq!(method(file, "closed", &[]), pyx_true());

    let file = open(temp.value(), "r");
    let content = method(file, "read", &[]);
    assert_str(content, "hello\nwörld\n");
    // A second read reaches end of file and yields "".
    assert_str(method(file, "read", &[]), "");
    assert_int(method(file, "tell", &[]), 12);
    close(file);
}

#[test]
fn readline_readlines_and_limits() {
    pyx_runtime_init();
    let temp = TempFile::new("lines");

    let file = open(temp.value(), "w");
    method(
        file,
        "writelines",
        &[pyx_list_new(
            3,
            [text("one\n"), text("two\n"), text("three")].as_ptr(),
        )],
    );
    assert_eq!(pyx_exc_pending(), 0);
    close(file);

    let file = open(temp.value(), "r");
    assert_str(method(file, "readline", &[]), "one\n");
    assert_str(method(file, "readline", &[pyx_int(2)]), "tw");
    assert_str(method(file, "readline", &[]), "o\n");
    assert_str(method(file, "readline", &[pyx_int(0)]), "");
    assert_str(method(file, "readline", &[pyx_int(-1)]), "three");
    // At end of file `readline()` returns "".
    assert_str(method(file, "readline", &[]), "");
    clear_error();

    // Reopen for `readlines`.
    close(file);
    let file = open(temp.value(), "r");
    let lines = method(file, "readlines", &[]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_int(pyx_len(lines), 3);
    assert_str(pyx_subscript(lines, pyx_int(2)), "three");
    close(file);
}

#[test]
fn file_iteration_yields_lines() {
    pyx_runtime_init();
    let temp = TempFile::new("iter");

    let file = open(temp.value(), "w");
    method(file, "write", &[text("a\nb\nc")]);
    close(file);

    let file = open(temp.value(), "r");
    let iter = pyx_iter_new(file);
    assert_eq!(pyx_exc_pending(), 0);
    let mut lines = Vec::new();
    loop {
        let mut out = 0i64;
        if pyx_iter_next(iter, &mut out) == 0 {
            break;
        }
        lines.push(out);
    }
    assert_eq!(pyx_exc_pending(), 0);
    assert_eq!(lines.len(), 3);
    assert_str(lines[0], "a\n");
    assert_str(lines[1], "b\n");
    assert_str(lines[2], "c");
    close(file);
}

#[test]
fn close_is_idempotent_and_blocks_operations() {
    pyx_runtime_init();
    let temp = TempFile::new("close");

    let file = open(temp.value(), "w");
    method(file, "write", &[text("x")]);
    close(file);
    // Second close is a no-op.
    method(file, "close", &[]);
    assert_eq!(pyx_exc_pending(), 0);

    let read = method(file, "read", &[]);
    assert_eq!(read, pyx_none());
    assert_eq!(error_tag(), 0);
    assert_error_text(error_message(), "I/O operation on closed file");
    clear_error();

    method(file, "write", &[text("y")]);
    assert_eq!(error_tag(), 0);
    assert_error_text(error_message(), "I/O operation on closed file");
    clear_error();

    method(file, "tell", &[]);
    assert_eq!(error_tag(), 0);
    clear_error();

    method(file, "flush", &[]);
    assert_eq!(error_tag(), 0);
    clear_error();
}

#[test]
fn context_manager_protocol_returns_self_and_closes() {
    pyx_runtime_init();
    let temp = TempFile::new("context");

    let file = open(temp.value(), "w");
    let entered = method(file, "__enter__", &[]);
    assert_eq!(pyx_exc_pending(), 0, "__enter__ must not raise");
    assert!(same(entered, file), "__enter__ returns the file itself");
    assert_eq!(method(file, "closed", &[]), pyx_false());

    let none = [pyx_none(), pyx_none(), pyx_none()];
    assert_eq!(method(file, "__exit__", &none), pyx_none());
    assert_eq!(pyx_exc_pending(), 0, "__exit__ must not raise");
    assert_eq!(method(file, "closed", &[]), pyx_true());

    // The protocol is idempotent, like `close()`.
    assert_eq!(method(file, "__exit__", &none), pyx_none());
    assert_eq!(pyx_exc_pending(), 0);

    // `__exit__` takes exactly the three exception arguments.
    method(file, "__exit__", &[]);
    assert_eq!(error_tag(), 1, "__exit__ arity errors are TypeErrors");
    clear_error();
}

#[test]
fn missing_file_raises_oserror() {
    pyx_runtime_init();
    let temp = TempFile::new("missing");
    let file = call("io", "open", &[temp.value(), text("r")]);
    assert_eq!(file, pyx_none());
    assert_eq!(error_tag(), 10);
    assert_error_text(
        error_message(),
        &format!(
            "No such file or directory (os error 2): '{}'",
            temp.path.display()
        ),
    );
    clear_error();
}

#[test]
fn seek_and_tell_count_characters() {
    pyx_runtime_init();
    let temp = TempFile::new("seek");

    let file = open(temp.value(), "w");
    method(file, "write", &[text("αβγ\nδε")]);
    close(file);

    let file = open(temp.value(), "r");
    assert_str(method(file, "read", &[pyx_int(2)]), "αβ");
    assert_int(method(file, "tell", &[]), 2);
    method(file, "seek", &[pyx_int(0)]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_int(method(file, "tell", &[]), 0);
    assert_str(method(file, "read", &[]), "αβγ\nδε");
    assert_int(method(file, "tell", &[]), 6);

    // Relative and end-relative seeks use character offsets.
    method(file, "seek", &[pyx_int(-2), pyx_int(2)]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_int(method(file, "tell", &[]), 4);
    assert_str(method(file, "read", &[]), "δε");

    method(file, "seek", &[pyx_int(0), pyx_int(1)]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_int(method(file, "tell", &[]), 6);

    sink(method(file, "seek", &[pyx_int(-10)]));
    assert_eq!(error_tag(), 10);
    clear_error();
    close(file);
}

#[test]
fn append_mode_and_unicode_write() {
    pyx_runtime_init();
    let temp = TempFile::new("append");

    let file = open(temp.value(), "w");
    method(file, "write", &[text("start\n")]);
    close(file);

    let file = open(temp.value(), "a");
    let written = method(file, "write", &[text("é\n")]);
    assert_eq!(pyx_exc_pending(), 0);
    // Write returns characters, not bytes.
    assert_int(written, 2);
    assert_int(method(file, "tell", &[]), 2);
    method(file, "flush", &[]);
    assert_eq!(pyx_exc_pending(), 0);
    close(file);

    let file = open(temp.value(), "r");
    assert_str(method(file, "read", &[]), "start\né\n");
    close(file);
}

#[test]
fn binary_write_and_read_roundtrip() {
    pyx_runtime_init();
    let temp = TempFile::new("binary-roundtrip");

    let file = open(temp.value(), "wb");
    assert_str(
        pyx_to_str(file),
        &format!("<file name='{}' mode='wb'>", temp.path.display()),
    );
    let payload = [0u8, b'A', b'\n'];
    let written = method(file, "write", &[bytes(&payload)]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_int(written, 3);
    close(file);
    assert_eq!(std::fs::read(&temp.path).expect("read raw file"), payload);

    let file = open(temp.value(), "rb");
    assert_str(
        pyx_to_str(file),
        &format!("<file name='{}' mode='rb'>", temp.path.display()),
    );
    assert_bytes(method(file, "read", &[]), &payload);
    // A second read reaches end of file and yields b"".
    assert_bytes(method(file, "read", &[]), b"");
    assert_int(method(file, "tell", &[]), 3);
    close(file);
}

#[test]
fn binary_readline_readlines_and_iteration() {
    pyx_runtime_init();
    let temp = TempFile::new("binary-lines");
    let payload = b"\xffA\nB\nC";

    let file = open(temp.value(), "wb");
    method(file, "write", &[bytes(payload)]);
    assert_eq!(pyx_exc_pending(), 0);
    close(file);

    let file = open(temp.value(), "rb");
    assert_bytes(method(file, "readline", &[]), b"\xffA\n");
    assert_int(method(file, "tell", &[]), 3);
    // The limit counts bytes, not characters.
    assert_bytes(method(file, "readline", &[pyx_int(1)]), b"B");
    assert_bytes(method(file, "readline", &[pyx_int(0)]), b"");
    assert_bytes(method(file, "readline", &[pyx_int(-1)]), b"\n");
    assert_bytes(method(file, "readline", &[]), b"C");
    // At end of file `readline()` returns b"".
    assert_bytes(method(file, "readline", &[]), b"");
    close(file);

    // Reopen for `readlines`.
    let file = open(temp.value(), "rb");
    let lines = method(file, "readlines", &[]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_int(pyx_len(lines), 3);
    assert_bytes(pyx_subscript(lines, pyx_int(0)), b"\xffA\n");
    assert_bytes(pyx_subscript(lines, pyx_int(2)), b"C");
    close(file);

    // Iteration yields lines as `bytes`, including the trailing newline.
    let file = open(temp.value(), "rb");
    let iter = pyx_iter_new(file);
    assert_eq!(pyx_exc_pending(), 0);
    let mut lines = Vec::new();
    loop {
        let mut out = 0i64;
        if pyx_iter_next(iter, &mut out) == 0 {
            break;
        }
        lines.push(out);
    }
    assert_eq!(pyx_exc_pending(), 0);
    assert_eq!(lines.len(), 3);
    assert!(same(lines[0], bytes(b"\xffA\n")));
    assert!(same(lines[1], bytes(b"B\n")));
    assert!(same(lines[2], bytes(b"C")));
    close(file);
}

#[test]
fn binary_seek_and_tell_count_bytes() {
    pyx_runtime_init();
    let temp = TempFile::new("binary-seek");

    let file = open(temp.value(), "w");
    method(file, "write", &[text("αβγ\nδε")]);
    close(file);

    // Text mode keeps character positions and character-sized reads.
    let file = open(temp.value(), "r");
    assert_str(method(file, "read", &[pyx_int(2)]), "αβ");
    assert_int(method(file, "tell", &[]), 2);
    method(file, "seek", &[pyx_int(-2), pyx_int(2)]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_int(method(file, "tell", &[]), 4);
    assert_str(method(file, "read", &[]), "δε");
    close(file);

    // Binary mode counts the UTF-8 bytes instead.
    let file = open(temp.value(), "rb");
    let first_two = method(file, "read", &[pyx_int(2)]);
    assert_bytes(first_two, "α".as_bytes());
    assert_int(method(file, "tell", &[]), 2);
    method(file, "seek", &[pyx_int(0)]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_bytes(method(file, "read", &[]), "αβγ\nδε".as_bytes());
    assert_int(method(file, "tell", &[]), 11);
    method(file, "seek", &[pyx_int(-2), pyx_int(2)]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_int(method(file, "tell", &[]), 9);
    // Byte offsets are not character offsets: byte 9 is the start of the
    // final two-byte UTF-8 character ε.
    assert_bytes(method(file, "read", &[]), "ε".as_bytes());
    close(file);
}

#[test]
fn binary_mode_rejects_text_options() {
    pyx_runtime_init();
    let temp = TempFile::new("binary-options");

    call("io", "open", &[temp.value(), text("rb"), text("utf-8")]);
    assert_eq!(error_tag(), 0);
    assert_error_text(
        error_message(),
        "binary mode doesn't take an encoding argument",
    );
    clear_error();

    call(
        "io",
        "open",
        &[temp.value(), text("rb"), pyx_none(), text("\n")],
    );
    assert_eq!(error_tag(), 0);
    assert_error_text(
        error_message(),
        "binary mode doesn't take a newline argument",
    );
    clear_error();

    call(
        "io",
        "open",
        &[
            temp.value(),
            text("rb"),
            pyx_none(),
            pyx_none(),
            text("strict"),
        ],
    );
    assert_eq!(error_tag(), 0);
    assert_error_text(
        error_message(),
        "binary mode doesn't take an errors argument",
    );
    clear_error();

    // `b` and `t` are mutually exclusive and each may appear only once.
    for mode in ["bt", "tb", "bb", "tt"] {
        call("io", "open", &[temp.value(), text(mode)]);
        assert_eq!(error_tag(), 0);
        assert_error_text(error_message(), &format!("invalid mode: '{mode}'"));
        clear_error();
    }

    call("io", "open", &[temp.value(), text("r+b")]);
    assert_eq!(error_tag(), 0);
    assert_error_text(error_message(), "read/write mode is not supported");
    clear_error();

    call("io", "open", &[temp.value(), text("q")]);
    assert_eq!(error_tag(), 0);
    assert_error_text(error_message(), "invalid mode: 'q'");
    clear_error();
}

#[test]
fn exclusive_binary_and_text_modes() {
    pyx_runtime_init();
    let temp = TempFile::new("exclusive-binary");

    let file = open(temp.value(), "xb");
    method(file, "write", &[bytes(b"\x01")]);
    assert_eq!(pyx_exc_pending(), 0);
    close(file);

    let file = open(temp.value(), "rb");
    assert_bytes(method(file, "read", &[]), b"\x01");
    close(file);

    // `x` refuses an existing path, in binary and text mode alike.
    call("io", "open", &[temp.value(), text("xb")]);
    assert_eq!(error_tag(), 10);
    clear_error();

    let text_temp = TempFile::new("exclusive-text");
    let file = open(text_temp.value(), "xt");
    method(file, "write", &[text("x")]);
    close(file);
    let file = open(text_temp.value(), "rt");
    assert_str(method(file, "read", &[]), "x");
    close(file);
}

#[test]
fn text_mode_aliases_and_write_type_checks() {
    pyx_runtime_init();
    let temp = TempFile::new("aliases");

    // `t` and the default are text mode; `x`/`a` work in binary mode too.
    let file = open(temp.value(), "wt");
    method(file, "write", &[text("text\n")]);
    assert_eq!(pyx_exc_pending(), 0);
    close(file);

    let file = open(temp.value(), "rt");
    assert_str(method(file, "read", &[]), "text\n");
    close(file);

    let file = open(temp.value(), "at");
    method(file, "write", &[text("more\n")]);
    close(file);

    let file = open(temp.value(), "rt");
    assert_str(method(file, "read", &[]), "text\nmore\n");
    close(file);

    // `write` requires bytes in binary mode and str in text mode.
    let file = open(temp.value(), "ab");
    method(file, "write", &[text("nope")]);
    assert_eq!(error_tag(), 1);
    clear_error();
    method(
        file,
        "writelines",
        &[pyx_list_new(1, [text("nope")].as_ptr())],
    );
    assert_eq!(error_tag(), 1);
    clear_error();
    method(
        file,
        "writelines",
        &[pyx_list_new(2, [bytes(b"a"), bytes(b"b\n")].as_ptr())],
    );
    assert_eq!(pyx_exc_pending(), 0);
    close(file);

    let file = open(temp.value(), "r");
    method(file, "write", &[bytes(b"nope")]);
    assert_eq!(error_tag(), 1);
    clear_error();
    close(file);
}

#[test]
fn csv_and_json_reject_binary_files() {
    pyx_runtime_init();
    let temp = TempFile::new("binary-csv-json");
    std::fs::write(&temp.path, "a,b\n1,2\n").expect("write fixture");

    let file = open(temp.value(), "rb");
    let reader = call("csv", "reader", &[file]);
    assert_eq!(reader, pyx_none());
    assert_eq!(error_tag(), 0);
    assert_error_text(error_message(), "cannot read binary file as text");
    clear_error();
    close(file);

    let file = open(temp.value(), "rb");
    let loaded = call("json", "load", &[file]);
    assert_eq!(loaded, pyx_none());
    assert_eq!(error_tag(), 0);
    assert_error_text(error_message(), "cannot read binary file as text");
    clear_error();
    close(file);

    // A binary file is not a text target for `csv.writer`/`json.dump`.
    let file = open(temp.value(), "ab");
    let writer = call("csv", "writer", &[file]);
    assert_eq!(pyx_exc_pending(), 0);
    method(
        writer,
        "writerow",
        &[pyx_list_new(2, [text("c"), text("d")].as_ptr())],
    );
    assert_eq!(error_tag(), 0);
    assert_error_text(error_message(), "cannot write text to binary file");
    clear_error();

    let value = call("json", "loads", &[text("{\"a\": 1}")]);
    assert_eq!(pyx_exc_pending(), 0);
    call("json", "dump", &[value, file]);
    assert_eq!(error_tag(), 0);
    assert_error_text(error_message(), "cannot write text to binary file");
    clear_error();
    close(file);

    // Writing bytes to a text file is equally rejected.
    let file = open(temp.value(), "r");
    method(file, "write", &[bytes(b"x")]);
    assert_eq!(error_tag(), 1);
    clear_error();
    close(file);
}

#[test]
fn keyword_slots_apply_defaults_and_validate_encoding() {
    pyx_runtime_init();
    let temp = TempFile::new("slots");

    let file = open(temp.value(), "w");
    method(file, "write", &[text("slots")]);
    close(file);

    // `open(path, mode, encoding=None, newline=None, errors=None)`, the shape
    // the frontend produces for keyword calls.
    let file = call(
        "io",
        "open",
        &[
            temp.value(),
            pyx_none(),
            text("utf-8"),
            pyx_none(),
            pyx_none(),
        ],
    );
    assert_eq!(pyx_exc_pending(), 0);
    assert_eq!(pyx_truthy(file), 1);
    assert_str(method(file, "read", &[]), "slots");
    close(file);

    let file = call(
        "io",
        "open",
        &[temp.value(), text("w"), pyx_none(), pyx_none(), pyx_none()],
    );
    assert_eq!(pyx_exc_pending(), 0);
    close(file);

    call("io", "open", &[temp.value(), text("r"), text("latin-1")]);
    assert_eq!(error_tag(), 0);
    assert_error_text(error_message(), "unsupported encoding: 'latin-1'");
    clear_error();
}

#[test]
fn write_errors_and_type_checks() {
    pyx_runtime_init();
    let temp = TempFile::new("errors");

    let file = open(temp.value(), "w");
    method(file, "write", &[text("ok")]);
    assert_eq!(pyx_exc_pending(), 0);

    // Reading a write-only file is an OSError.
    method(file, "read", &[]);
    assert_eq!(error_tag(), 10);
    assert_error_text(error_message(), "file not open for reading");
    clear_error();

    // `write` requires a string.
    method(file, "write", &[pyx_int(1)]);
    assert_eq!(error_tag(), 1);
    clear_error();

    // `writelines` requires an iterable of strings.
    method(
        file,
        "writelines",
        &[pyx_list_new(1, [pyx_int(1)].as_ptr())],
    );
    assert_eq!(error_tag(), 1);
    clear_error();

    close(file);

    let file = open(temp.value(), "r");
    method(file, "write", &[text("nope")]);
    assert_eq!(error_tag(), 10);
    assert_error_text(error_message(), "file not open for writing");
    clear_error();
    close(file);
}

#[test]
fn open_function_value_is_callable() {
    pyx_runtime_init();
    let temp = TempFile::new("function");

    let (module, name) = ("io", "open");
    let native_open = pyx_module_get(module.as_ptr(), module.len(), name.as_ptr(), name.len());
    assert_eq!(pyx_exc_pending(), 0);
    assert_eq!(pyx_truthy(native_open), 1);

    let args = [temp.value(), text("w")];
    let file = pyx_call(native_open, args.len(), args.as_ptr());
    assert_eq!(pyx_exc_pending(), 0);
    method(file, "write", &[text("via function")]);
    close(file);

    let args = [temp.value(), text("r")];
    let file = pyx_call(native_open, args.len(), args.as_ptr());
    assert_eq!(pyx_exc_pending(), 0);
    assert_str(method(file, "read", &[]), "via function");
    close(file);
}

#[test]
fn os_remove_and_rename_use_oserror() {
    pyx_runtime_init();
    let temp = TempFile::new("rename-src");
    let renamed = std::env::temp_dir().join(format!("pyx_io_{}_renamed.txt", std::process::id()));
    let _ = std::fs::remove_file(&renamed);

    let file = open(temp.value(), "w");
    method(file, "write", &[text("rename")]);
    close(file);

    let renamed_value = text(renamed.to_str().expect("temp path is UTF-8"));
    assert_eq!(
        call("os", "rename", &[temp.value(), renamed_value]),
        pyx_none()
    );
    assert_eq!(pyx_exc_pending(), 0);
    assert!(!temp.path.exists());
    assert!(renamed.exists());

    let file = open(renamed_value, "r");
    assert_str(method(file, "read", &[]), "rename");
    close(file);

    assert_eq!(call("os", "remove", &[renamed_value]), pyx_none());
    assert_eq!(pyx_exc_pending(), 0);
    assert!(!renamed.exists());

    call("os", "remove", &[renamed_value]);
    assert_eq!(error_tag(), 10);
    clear_error();
}

/// Consumes a value so a call that is expected to fail does not trip the
/// unused-result lint.
fn sink(value: i64) -> i64 {
    value
}
