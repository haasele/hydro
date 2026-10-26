//! Integration tests for `csv.reader`/`csv.writer` and the dict variants,
//! driven through the public ABI and `io.open` file objects.
//!
//! Every test uses its own temp file and removes it on drop.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use hydro_runtime::abi::*;

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
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

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    hydro_method(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

fn same(a: i64, b: i64) -> bool {
    module("operator", "eq", &[a, b]) == hydro_true()
}

fn assert_str(actual: i64, expected: &str) {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
    assert!(
        same(actual, text(expected)),
        "value mismatch, got {:?}",
        hydro_to_str(actual)
    );
}

fn assert_int(value: i64, expected: i64) {
    assert_eq!(hydro_to_int(value), expected << 3);
}

const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;
const ATTRIBUTE_ERROR: i64 = 9;
const OS_ERROR: i64 = 10;

/// Compares a pending exception message without requiring a clear state.
fn assert_error_text(actual: i64, expected: &str) {
    assert!(
        same(actual, text(expected)),
        "message mismatch, got {:?}",
        hydro_to_str(actual)
    );
}

fn error_tag() -> i64 {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    hydro_exc_tag()
}

fn error_message() -> i64 {
    hydro_to_str(hydro_exc_value())
}

fn clear_error() {
    hydro_exc_clear();
}

fn list(items: &[i64]) -> i64 {
    hydro_list_new(items.len(), items.as_ptr())
}

fn dict(keys: &[i64], values: &[i64]) -> i64 {
    hydro_dict_new(keys.len(), keys.as_ptr(), values.as_ptr())
}

struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(tag: &str) -> TempFile {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hydro_csv_{}_{}_{}.txt",
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

    fn contents(&self) -> String {
        std::fs::read_to_string(&self.path).expect("temp file is readable")
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn open(path: i64, mode: &str) -> i64 {
    let file = module("io", "open", &[path, text(mode)]);
    assert_eq!(hydro_exc_pending(), 0, "open failed");
    file
}

fn close(file: i64) {
    method(file, "close", &[]);
    assert_eq!(hydro_exc_pending(), 0, "close failed");
}

/// Overwrites stale stack slots so the collector does not see old file tags.
#[inline(never)]
fn scrub_stack() {
    let mut buffer = [0u8; 64 * 1024];
    std::hint::black_box(&mut buffer);
    buffer[0] = 0;
    std::hint::black_box(&buffer);
}

#[test]
fn writer_reader_roundtrip_quoting() {
    hydro_runtime_init();
    let temp = TempFile::new("roundtrip");

    let file = open(temp.value(), "w");
    let writer = module("csv", "writer", &[file]);
    assert_eq!(hydro_exc_pending(), 0);
    assert_str(hydro_to_str(writer), "<csv.writer>");

    let row = list(&[
        text("a,b"),
        text("c\"d"),
        text("e\nf"),
        hydro_none(),
        text("plain"),
    ]);
    assert_eq!(method(writer, "writerow", &[row]), hydro_none());
    assert_eq!(hydro_exc_pending(), 0);

    let rows = list(&[list(&[hydro_int(1), hydro_float_new(2.5)])]);
    assert_eq!(method(writer, "writerows", &[rows]), hydro_none());
    assert_eq!(hydro_exc_pending(), 0);
    close(file);

    assert_eq!(
        temp.contents(),
        "\"a,b\",\"c\"\"d\",\"e\nf\",,plain\r\n1,2.5\r\n"
    );

    let file = open(temp.value(), "r");
    let reader = module("csv", "reader", &[file]);
    assert_eq!(hydro_exc_pending(), 0);
    assert_str(hydro_to_str(reader), "<csv.reader>");
    let parsed = hydro_list_from(reader);
    assert_eq!(hydro_exc_pending(), 0);
    assert_int(hydro_len(parsed), 2);

    let first = hydro_subscript(parsed, hydro_int(0));
    assert_int(hydro_len(first), 5);
    assert_str(hydro_subscript(first, hydro_int(0)), "a,b");
    assert_str(hydro_subscript(first, hydro_int(1)), "c\"d");
    assert_str(hydro_subscript(first, hydro_int(2)), "e\nf");
    assert_str(hydro_subscript(first, hydro_int(3)), "");
    assert_str(hydro_subscript(first, hydro_int(4)), "plain");

    let second = hydro_subscript(parsed, hydro_int(1));
    assert_int(hydro_len(second), 2);
    assert_str(hydro_subscript(second, hydro_int(0)), "1");
    assert_str(hydro_subscript(second, hydro_int(1)), "2.5");
    close(file);
}

#[test]
fn reader_iteration_builds_fresh_rows() {
    hydro_runtime_init();
    let temp = TempFile::new("iterate");
    std::fs::write(&temp.path, "a,b\n\nc\n").expect("write fixture");

    let file = open(temp.value(), "r");
    let reader = module("csv", "reader", &[file]);
    let iter = hydro_iter_new(reader);
    assert_eq!(hydro_exc_pending(), 0);

    let mut rows = Vec::new();
    loop {
        let mut out = 0i64;
        if hydro_iter_next(iter, &mut out) == 0 {
            break;
        }
        rows.push(out);
    }
    assert_eq!(hydro_exc_pending(), 0);
    assert_eq!(rows.len(), 3);
    assert_str(hydro_subscript(rows[0], hydro_int(1)), "b");
    assert_int(hydro_len(rows[1]), 0);
    assert_str(hydro_subscript(rows[2], hydro_int(0)), "c");
    close(file);
}

#[test]
fn dict_reader_header_and_missing_extra_fields() {
    hydro_runtime_init();
    let temp = TempFile::new("dictreader");
    std::fs::write(&temp.path, "name,age\nAlice,30\nBob\nCarol,40,extra\n").expect("write fixture");

    let file = open(temp.value(), "r");
    let reader = module("csv", "DictReader", &[file]);
    assert_eq!(hydro_exc_pending(), 0);
    assert_str(hydro_to_str(reader), "<csv.reader>");
    let rows = hydro_list_from(reader);
    assert_eq!(hydro_exc_pending(), 0);
    assert_int(hydro_len(rows), 3);

    let first = hydro_subscript(rows, hydro_int(0));
    assert_str(hydro_subscript(first, text("name")), "Alice");
    assert_str(hydro_subscript(first, text("age")), "30");

    // Missing trailing column: empty string, not None.
    let second = hydro_subscript(rows, hydro_int(1));
    assert_str(hydro_subscript(second, text("name")), "Bob");
    assert_str(hydro_subscript(second, text("age")), "");

    // Extra columns are ignored.
    let third = hydro_subscript(rows, hydro_int(2));
    assert_str(hydro_subscript(third, text("age")), "40");
    assert_int(hydro_len(third), 2);
    close(file);
}

#[test]
fn dict_writer_header_missing_and_extra_fields() {
    hydro_runtime_init();
    let temp = TempFile::new("dictwriter");

    let file = open(temp.value(), "w");
    let fieldnames = list(&[text("name"), text("age")]);
    let writer = module("csv", "DictWriter", &[file, fieldnames]);
    assert_eq!(hydro_exc_pending(), 0);
    assert_str(hydro_to_str(writer), "<csv.writer>");

    assert_eq!(method(writer, "writeheader", &[]), hydro_none());
    assert_eq!(hydro_exc_pending(), 0);

    // Dict insertion order differs from the fieldname order.
    let row = dict(&[text("age"), text("name")], &[text("30"), text("Alice")]);
    assert_eq!(method(writer, "writerow", &[row]), hydro_none());
    assert_eq!(hydro_exc_pending(), 0);

    // Missing value becomes an empty field.
    let partial = dict(&[text("name")], &[text("Bob")]);
    assert_eq!(method(writer, "writerow", &[partial]), hydro_none());
    assert_eq!(hydro_exc_pending(), 0);

    let rows = list(&[dict(
        &[text("name"), text("age")],
        &[text("Carol"), text("40")],
    )]);
    assert_eq!(method(writer, "writerows", &[rows]), hydro_none());
    assert_eq!(hydro_exc_pending(), 0);

    // A key outside fieldnames is rejected before anything is written.
    let extra = dict(&[text("name"), text("city")], &[text("Dan"), text("X")]);
    method(writer, "writerow", &[extra]);
    assert_eq!(error_tag(), VALUE_ERROR);
    assert_error_text(error_message(), "dict contains fields not in fieldnames");
    clear_error();

    // Non-dict rows and non-string fieldnames are type errors.
    method(writer, "writerow", &[list(&[text("x")])]);
    assert_eq!(error_tag(), TYPE_ERROR);
    clear_error();

    let bad_names = list(&[hydro_int(1)]);
    let failed = module("csv", "DictWriter", &[file, bad_names]);
    assert_eq!(failed, hydro_none());
    assert_eq!(error_tag(), TYPE_ERROR);
    clear_error();

    close(file);
    assert_eq!(
        temp.contents(),
        "name,age\r\nAlice,30\r\nBob,\r\nCarol,40\r\n"
    );
}

#[test]
fn malformed_quotes_raise_value_error() {
    hydro_runtime_init();
    let temp = TempFile::new("malformed");
    std::fs::write(&temp.path, "\"a,b").expect("write fixture");

    let file = open(temp.value(), "r");
    let reader = module("csv", "reader", &[file]);
    assert_eq!(reader, hydro_none());
    assert_eq!(error_tag(), VALUE_ERROR);
    assert_error_text(error_message(), "unexpected end of data");
    clear_error();
    close(file);
}

#[test]
fn writer_errors_and_closed_files() {
    hydro_runtime_init();
    let temp = TempFile::new("errors");

    // Reading file with a writer: the write fails with OSError.
    std::fs::write(&temp.path, "payload").expect("write fixture");
    let read_file = open(temp.value(), "r");
    let writer = module("csv", "writer", &[read_file]);
    assert_eq!(hydro_exc_pending(), 0);
    method(writer, "writerow", &[list(&[text("x")])]);
    assert_eq!(error_tag(), OS_ERROR);
    clear_error();
    close(read_file);

    // Closed target file: ValueError from FileState.
    method(writer, "writerow", &[list(&[text("x")])]);
    assert_eq!(error_tag(), VALUE_ERROR);
    assert_error_text(error_message(), "I/O operation on closed file");
    clear_error();

    // Non-file arguments are rejected at construction.
    let bad = module("csv", "writer", &[list(&[])]);
    assert_eq!(bad, hydro_none());
    assert_eq!(error_tag(), TYPE_ERROR);
    clear_error();

    let bad = module("csv", "reader", &[hydro_none()]);
    assert_eq!(bad, hydro_none());
    assert_eq!(error_tag(), TYPE_ERROR);
    clear_error();

    // Plain writers have no writeheader.
    let file = open(temp.value(), "w");
    let plain = module("csv", "writer", &[file]);
    method(plain, "writeheader", &[]);
    assert_eq!(error_tag(), ATTRIBUTE_ERROR);
    clear_error();
    // Unknown method.
    method(plain, "nope", &[]);
    assert_eq!(error_tag(), ATTRIBUTE_ERROR);
    clear_error();
    close(file);
}

#[test]
fn reader_closed_file_errors() {
    hydro_runtime_init();
    let temp = TempFile::new("readerclosed");

    let file = open(temp.value(), "w");
    method(file, "write", &[text("a,b\n")]);
    close(file);

    let reader = module("csv", "reader", &[file]);
    assert_eq!(reader, hydro_none());
    assert_eq!(error_tag(), VALUE_ERROR);
    assert_error_text(error_message(), "I/O operation on closed file");
    clear_error();

    // A write-mode file cannot be read.
    let file = open(temp.value(), "a");
    let reader = module("csv", "reader", &[file]);
    assert_eq!(reader, hydro_none());
    assert_eq!(error_tag(), OS_ERROR);
    clear_error();
    close(file);
}

#[inline(never)]
fn make_writer(path: i64) -> i64 {
    let file = open(path, "w");
    module("csv", "writer", &[file])
}

#[test]
fn collection_keeps_writer_file_alive() {
    hydro_runtime_init();
    let temp = TempFile::new("gcwriter");

    let writer = make_writer(temp.value());
    assert_eq!(hydro_exc_pending(), 0);

    scrub_stack();
    let _ = hydro_gc_collect();
    assert_eq!(hydro_exc_pending(), 0);

    // Only the writer's GC edge keeps the file object (and its handle) alive.
    let row = list(&[text("alive")]);
    assert_eq!(method(writer, "writerow", &[row]), hydro_none());
    assert_eq!(hydro_exc_pending(), 0);
    assert_eq!(temp.contents(), "alive\r\n");
}
