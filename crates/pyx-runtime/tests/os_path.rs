//! Integration tests for the `os.path` module: joining, splitting, filesystem
//! predicates and path-like (`pathlib.Path`) arguments.
//!
//! The tests drive the public runtime ABI, exactly like generated code, and
//! use their own temporary directory (removed on drop).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use pyx_runtime::abi::*;

const TYPE_ERROR: i64 = 1;

fn text(value: &str) -> i64 {
    pyx_str_new(value.as_ptr(), value.len())
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

fn get(module: &str, name: &str) -> i64 {
    pyx_module_get(module.as_ptr(), module.len(), name.as_ptr(), name.len())
}

fn invoke(function: i64, args: &[i64]) -> i64 {
    pyx_call(function, args.len(), args.as_ptr())
}

fn item(tuple: i64, index: i64) -> i64 {
    pyx_subscript(tuple, pyx_int(index))
}

fn same(a: i64, b: i64) -> bool {
    call("operator", "eq", &[a, b]) == pyx_true()
}

fn assert_no_error() {
    assert_eq!(pyx_exc_pending(), 0, "unexpected pending exception");
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert!(
        same(actual, text(expected)),
        "string mismatch, got {:?}",
        pyx_to_str(actual)
    );
}

fn assert_true(actual: i64) {
    assert_no_error();
    assert_eq!(actual, pyx_true(), "expected True");
}

fn assert_false(actual: i64) {
    assert_no_error();
    assert_eq!(actual, pyx_false(), "expected False");
}

fn expect_type_error(actual: i64) {
    assert_eq!(actual, pyx_none(), "expected the None dummy value");
    assert_eq!(pyx_exc_pending(), 1, "expected a pending exception");
    assert_eq!(pyx_exc_tag(), TYPE_ERROR, "wrong exception tag");
    pyx_exc_clear();
}

/// A unique temporary directory removed when the test ends.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pyx_os_path_{}_{}_{}",
            std::process::id(),
            tag,
            count
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    fn text(&self) -> String {
        self.path.to_str().expect("temp path is UTF-8").to_string()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn join_concatenates_and_resets_on_absolute_segments() {
    pyx_runtime_init();

    assert_str(
        call("os.path", "join", &[text("a"), text("b")]),
        &format!("a{}b", std::path::MAIN_SEPARATOR),
    );
    let separator = std::path::MAIN_SEPARATOR;
    assert_str(
        call("os.path", "join", &[text("a"), text("b"), text("c")]),
        &format!("a{separator}b{separator}c"),
    );
    assert_str(
        call("os.path", "join", &[text("a/"), text("b")]),
        &format!("a{separator}b"),
    );
    assert_str(call("os.path", "join", &[text("a"), text("/b")]), "/b");
    assert_str(call("os.path", "join", &[]), "");

    // Function values route through the same implementation.
    let join = get("os.path", "join");
    assert_no_error();
    assert_str(
        invoke(join, &[text("a"), text("b")]),
        &format!("a{separator}b"),
    );

    expect_type_error(call("os.path", "join", &[text("a"), pyx_int(1)]));
}

#[test]
fn basename_dirname_and_split_match_python() {
    pyx_runtime_init();

    for (path, expected) in [("a/b", "b"), ("a/b/", ""), ("/", ""), ("plain", "plain")] {
        assert_str(call("os.path", "basename", &[text(path)]), expected);
    }
    for (path, expected) in [
        ("a/b", "a"),
        ("a/b/", "a/b"),
        ("/a", "/"),
        ("plain", ""),
        ("/", "/"),
    ] {
        assert_str(call("os.path", "dirname", &[text(path)]), expected);
    }

    let split = call("os.path", "split", &[text("a/b/c")]);
    assert_no_error();
    assert_str(item(split, 0), "a/b");
    assert_str(item(split, 1), "c");
}

#[test]
fn splitext_splits_the_final_extension() {
    pyx_runtime_init();

    for (path, root, extension) in [
        ("a.tar.gz", "a.tar", ".gz"),
        (".bashrc", ".bashrc", ""),
        ("a.", "a", "."),
        ("noext", "noext", ""),
        ("dir.name/file", "dir.name/file", ""),
    ] {
        let split = call("os.path", "splitext", &[text(path)]);
        assert_no_error();
        assert_str(item(split, 0), root);
        assert_str(item(split, 1), extension);
    }
}

#[test]
fn exists_isfile_isdir_and_path_like_arguments() {
    pyx_runtime_init();
    let dir = TempDir::new("predicates");
    let file = dir.path.join("file.txt");
    std::fs::write(&file, b"x").expect("write file");

    let file_text = file.to_str().expect("UTF-8 path");
    let dir_text = dir.text();
    let missing = dir.path.join("missing").to_string_lossy().into_owned();

    assert_true(call("os.path", "exists", &[text(file_text)]));
    assert_true(call("os.path", "isfile", &[text(file_text)]));
    assert_false(call("os.path", "isdir", &[text(file_text)]));
    assert_true(call("os.path", "isdir", &[text(&dir_text)]));
    assert_false(call("os.path", "isfile", &[text(&dir_text)]));
    assert_false(call("os.path", "exists", &[text(&missing)]));

    // A `pathlib.Path` is accepted wherever a path string is.
    let path = call("pathlib", "Path", &[text(file_text)]);
    assert_no_error();
    assert_true(call("os.path", "exists", &[path]));
    assert_true(call("os.path", "isfile", &[path]));
    assert_str(call("os.path", "basename", &[path]), "file.txt");

    expect_type_error(call("os.path", "exists", &[pyx_int(1)]));
    expect_type_error(call("os.path", "basename", &[pyx_none()]));
}

#[test]
fn abspath_normalises_relative_paths() {
    pyx_runtime_init();

    let cwd = std::env::current_dir().expect("current dir");
    assert_str(
        call("os.path", "abspath", &[text(".")]),
        &cwd.to_string_lossy(),
    );
    let expected = cwd.join("some").join("path");
    assert_str(
        call("os.path", "abspath", &[text("some/./nested/../path")]),
        &expected.to_string_lossy(),
    );
}
