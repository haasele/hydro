//! Integration tests for `json.dump`/`json.load` on `io.open` file objects.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use hydro_runtime::abi::*;

const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;
const OS_ERROR: i64 = 10;

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

struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(tag: &str) -> TempFile {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hydro_json_io_{}_{}_{}.txt",
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

#[test]
fn dump_load_roundtrip_with_unicode() {
    hydro_runtime_init();
    let temp = TempFile::new("roundtrip");

    let source = "{\"name\": \"w\u{00f6}rld\", \"n\": 3, \"flag\": true, \
                  \"none\": null, \"list\": [1, 2.5]}";
    let original = module("json", "loads", &[text(source)]);
    assert_eq!(hydro_exc_pending(), 0);
    let expected = module("json", "dumps", &[original]);
    assert_eq!(hydro_exc_pending(), 0);

    let file = open(temp.value(), "w");
    let status = module("json", "dump", &[original, file]);
    assert_eq!(status, hydro_none());
    assert_eq!(hydro_exc_pending(), 0);
    close(file);
    let contents = temp.contents();
    assert!(same(expected, text(&contents)), "dump content mismatch");
    // `ensure_ascii=True`: the umlaut is escaped on disk.
    assert!(contents.contains("w\\u00f6rld"));

    let file = open(temp.value(), "r");
    let loaded = module("json", "load", &[file]);
    assert_eq!(hydro_exc_pending(), 0);
    let dumped = module("json", "dumps", &[loaded]);
    assert_eq!(hydro_exc_pending(), 0);
    assert!(same(dumped, expected), "roundtrip mismatch");
    close(file);
}

#[test]
fn load_supports_astral_escapes() {
    hydro_runtime_init();
    let temp = TempFile::new("astral");
    std::fs::write(&temp.path, "\"\\ud83d\\ude00 \\u00e9\"").expect("write fixture");

    let file = open(temp.value(), "r");
    let loaded = module("json", "load", &[file]);
    assert_eq!(hydro_exc_pending(), 0);
    assert_str(loaded, "\u{1f600} \u{00e9}");
    close(file);
}

#[test]
fn invalid_json_raises_value_error() {
    hydro_runtime_init();
    let temp = TempFile::new("invalid");
    std::fs::write(&temp.path, "{\"a\": }").expect("write fixture");

    let file = open(temp.value(), "r");
    let loaded = module("json", "load", &[file]);
    assert_eq!(loaded, hydro_none());
    assert_eq!(error_tag(), VALUE_ERROR);
    assert_error_text(error_message(), "Expecting value");
    clear_error();
    close(file);
}

#[test]
fn file_errors_are_reported() {
    hydro_runtime_init();
    let temp = TempFile::new("errors");
    std::fs::write(&temp.path, "{\"a\": 1}").expect("write fixture");

    // Closed file: ValueError from the shared FileState.
    let file = open(temp.value(), "r");
    close(file);
    let loaded = module("json", "load", &[file]);
    assert_eq!(loaded, hydro_none());
    assert_eq!(error_tag(), VALUE_ERROR);
    assert_error_text(error_message(), "I/O operation on closed file");
    clear_error();

    let file = open(temp.value(), "w");
    close(file);
    let status = module("json", "dump", &[hydro_none(), file]);
    assert_eq!(status, hydro_none());
    assert_eq!(error_tag(), VALUE_ERROR);
    assert_error_text(error_message(), "I/O operation on closed file");
    clear_error();

    // Wrong mode: read for load needs a read-mode file, dump needs write.
    let file = open(temp.value(), "w");
    let loaded = module("json", "load", &[file]);
    assert_eq!(loaded, hydro_none());
    assert_eq!(error_tag(), OS_ERROR);
    clear_error();
    close(file);

    let file = open(temp.value(), "r");
    let status = module("json", "dump", &[hydro_none(), file]);
    assert_eq!(status, hydro_none());
    assert_eq!(error_tag(), OS_ERROR);
    clear_error();
    close(file);

    // Non-file arguments are type errors.
    let bad = module("json", "load", &[hydro_int(1)]);
    assert_eq!(bad, hydro_none());
    assert_eq!(error_tag(), TYPE_ERROR);
    clear_error();

    let bad = module("json", "dump", &[hydro_none(), hydro_none()]);
    assert_eq!(bad, hydro_none());
    assert_eq!(error_tag(), TYPE_ERROR);
    clear_error();
}

#[test]
fn serialisation_error_writes_nothing() {
    hydro_runtime_init();
    let temp = TempFile::new("unserialisable");
    let file = open(temp.value(), "w");

    // Sets are not JSON serialisable; the file must stay untouched.
    let item = hydro_int(1);
    let set = hydro_set_new(1, &item as *const i64);
    let status = module("json", "dump", &[set, file]);
    assert_eq!(status, hydro_none());
    assert_eq!(error_tag(), TYPE_ERROR);
    clear_error();
    close(file);
    assert_eq!(temp.contents(), "");
}

#[test]
fn load_reads_only_the_remaining_file_content() {
    hydro_runtime_init();
    let temp = TempFile::new("remaining");
    std::fs::write(&temp.path, "[1, 2]").expect("write fixture");

    let file = open(temp.value(), "r");
    let loaded = module("json", "load", &[file]);
    assert_eq!(hydro_exc_pending(), 0);
    let dumped = module("json", "dumps", &[loaded]);
    assert_eq!(hydro_exc_pending(), 0);
    assert!(same(dumped, text("[1, 2]")), "load mismatch");
    // The cursor is at the end, so a second load sees an empty document.
    let second = module("json", "load", &[file]);
    assert_eq!(second, hydro_none());
    assert_eq!(error_tag(), VALUE_ERROR);
    assert_error_text(error_message(), "Expecting value");
    clear_error();
    close(file);
}
