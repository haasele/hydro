//! Integration tests for `hydro_getattr`: data attributes of `pathlib.Path`,
//! file objects, `threading.Thread` and `multiprocessing.Process`, plus the
//! `AttributeError` path for unknown names and receivers.
//!
//! Every test drives the runtime through the public ABI and uses its own
//! temporary directory, removed on drop.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use hydro_runtime::abi::*;

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
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

fn getattr(obj: i64, name: &str) -> i64 {
    hydro_getattr(obj, name.as_ptr(), name.len())
}

fn path(value: &str) -> i64 {
    let value = call("pathlib", "Path", &[text(value)]);
    assert_eq!(hydro_exc_pending(), 0, "Path() raised");
    value
}

fn function(wrapper: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
    hydro_function_new(wrapper as *const u8, arity, name.as_ptr(), name.len(), 0)
}

extern "C" fn worker(_argv: *const i64) -> i64 {
    0
}

fn same(a: i64, b: i64) -> bool {
    call("operator", "eq", &[a, b]) == hydro_true()
}

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert!(
        same(actual, text(expected)),
        "value mismatch, got {:?}",
        hydro_to_str(actual)
    );
}

fn assert_path(actual: i64, expected: &str) {
    assert_no_error();
    assert!(
        same(hydro_to_str(actual), text(expected)),
        "path mismatch, got {:?}",
        hydro_to_str(actual)
    );
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(hydro_to_int(actual), expected << 3);
}

fn error_tag() -> i64 {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    hydro_exc_tag()
}

fn error_message() -> i64 {
    hydro_to_str(hydro_exc_value())
}

fn assert_error_text(actual: i64, expected: &str) {
    assert!(
        same(actual, text(expected)),
        "message mismatch, got {:?}",
        hydro_to_str(actual)
    );
}

fn clear_error() {
    hydro_exc_clear();
}

/// A unique temp directory removed when the test ends.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hydro_attrs_{}_{}_{}",
            std::process::id(),
            tag,
            count
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn open(path: i64, mode: &str) -> i64 {
    let file = call("io", "open", &[path, text(mode)]);
    assert_no_error();
    file
}

#[test]
fn path_data_attributes() {
    hydro_runtime_init();

    let archive = path("dir/archive.tar.gz");
    assert_str(getattr(archive, "name"), "archive.tar.gz");
    assert_str(getattr(archive, "stem"), "archive.tar");
    assert_str(getattr(archive, "suffix"), ".gz");
    assert_path(getattr(archive, "parent"), "dir");
    assert_str(getattr(archive, "path"), "dir/archive.tar.gz");

    assert_str(getattr(path("a"), "name"), "a");
    assert_path(getattr(path("a"), "parent"), ".");
    assert_path(getattr(path("/a"), "parent"), "/");
    assert_str(getattr(path("/"), "name"), "");
    assert_str(getattr(path("README"), "suffix"), "");
}

#[test]
fn file_data_attributes() {
    hydro_runtime_init();
    let dir = TempDir::new("file");
    let target = dir.path.join("attrs.txt");
    let target_text = target.to_str().expect("UTF-8").to_string();

    let file = open(text(&target_text), "w");
    assert_str(getattr(file, "name"), &target_text);
    assert_str(getattr(file, "mode"), "w");
    assert_eq!(getattr(file, "closed"), hydro_false());
    assert_str(getattr(file, "encoding"), "UTF-8");
    assert_no_error();
    hydro_method(file, b"close".as_ptr(), 5, 0, std::ptr::null());
    assert_no_error();

    // Closing the file flips `closed` but keeps the metadata readable.
    let reopened = open(text(&target_text), "r");
    assert_str(getattr(reopened, "mode"), "r");
    assert_eq!(getattr(reopened, "closed"), hydro_false());
    hydro_method(reopened, b"close".as_ptr(), 5, 0, std::ptr::null());
    assert_no_error();
    assert_eq!(getattr(reopened, "closed"), hydro_true());
    assert_str(getattr(reopened, "encoding"), "UTF-8");

    // Binary files report a `b` mode and no encoding.
    let binary = open(text(&target_text), "rb");
    assert_str(getattr(binary, "mode"), "rb");
    assert_eq!(getattr(binary, "encoding"), hydro_none());
    assert_eq!(getattr(binary, "closed"), hydro_false());
    assert_no_error();
}

#[test]
fn thread_data_attributes() {
    hydro_runtime_init();
    let thread = call(
        "threading",
        "Thread",
        &[
            function(worker, 0, "worker"),
            hydro_list_new(0, std::ptr::null()),
            text("worker-thread"),
        ],
    );
    assert_no_error();

    assert_str(getattr(thread, "name"), "worker-thread");
    // Not started yet: no ident.
    assert_eq!(getattr(thread, "ident"), hydro_none());
    assert_eq!(getattr(thread, "daemon"), hydro_false());
    assert_no_error();
}

#[test]
fn process_data_attributes() {
    hydro_runtime_init();
    let process = call(
        "multiprocessing",
        "Process",
        &[
            function(worker, 0, "worker"),
            hydro_list_new(0, std::ptr::null()),
            text("worker-process"),
        ],
    );
    assert_no_error();

    assert_str(getattr(process, "name"), "worker-process");
    // Not started yet: pid 0 and no exit code.
    assert_int(getattr(process, "pid"), 0);
    assert_eq!(getattr(process, "exitcode"), hydro_none());
    assert_eq!(getattr(process, "daemon"), hydro_false());
    assert_no_error();
}

#[test]
fn unknown_attributes_raise_attribute_error() {
    hydro_runtime_init();

    // A method name read without a call is not a data attribute.
    getattr(path("a"), "exists");
    assert_eq!(error_tag(), 9);
    assert_error_text(error_message(), "'Path' object has no attribute 'exists'");
    clear_error();

    getattr(path("a"), "nope");
    assert_eq!(error_tag(), 9);
    clear_error();

    // Receivers without attributes (and non-objects) report the same error.
    getattr(hydro_int(1), "name");
    assert_eq!(error_tag(), 9);
    assert_error_text(error_message(), "'int' object has no attribute 'name'");
    clear_error();

    getattr(call("random", "Random", &[]), "name");
    assert_eq!(error_tag(), 9);
    clear_error();
}

#[test]
fn attributes_survive_collection() {
    hydro_runtime_init();

    let target = path("dir/kept.tar.gz");
    let items = [target, getattr(target, "parent")];
    let list = hydro_list_new(items.len(), items.as_ptr());
    assert_no_error();

    for _ in 0..200 {
        let _ = hydro_str_new(b"garbage".as_ptr(), 7);
    }
    hydro_gc_collect();

    let kept = hydro_subscript(list, hydro_int(0));
    assert_str(getattr(kept, "name"), "kept.tar.gz");
    assert_str(getattr(kept, "suffix"), ".gz");
    assert_path(getattr(kept, "parent"), "dir");
    let parent = hydro_subscript(list, hydro_int(1));
    assert_path(getattr(parent, "parent"), ".");
    assert_no_error();
}
