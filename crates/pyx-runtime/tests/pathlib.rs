//! Integration tests for `pathlib.Path`: construction, path arithmetic, file
//! and directory helpers, iteration, globbing, error reporting and GC
//! survival.
//!
//! Every test drives the runtime through the public ABI and uses its own
//! temporary directory, removed on drop.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use pyx_runtime::abi::*;

fn text(value: &str) -> i64 {
    pyx_str_new(value.as_ptr(), value.len())
}

fn bytes(value: &[u8]) -> i64 {
    pyx_bytes_new(value.as_ptr(), value.len())
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

fn method_kw(obj: i64, name: &str, args: &[i64], kwargs: &[(&str, i64)]) -> i64 {
    let mut pairs = Vec::with_capacity(kwargs.len() * 2);
    for (key, value) in kwargs {
        pairs.push(text(key));
        pairs.push(*value);
    }
    pyx_method_kw(
        obj,
        name.as_ptr(),
        name.len(),
        args.len(),
        args.as_ptr(),
        kwargs.len(),
        pairs.as_ptr(),
    )
}

fn path(value: &str) -> i64 {
    let value = call("pathlib", "Path", &[text(value)]);
    assert_eq!(pyx_exc_pending(), 0, "Path() raised");
    value
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
        "value mismatch, got {:?}",
        pyx_to_str(actual)
    );
}

fn assert_path(actual: i64, expected: &str) {
    assert_no_error();
    assert!(
        same(pyx_to_str(actual), text(expected)),
        "path mismatch, got {:?}",
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

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(pyx_to_int(actual), expected << 3);
}

fn assert_bytes(actual: i64, expected: &[u8]) {
    assert_no_error();
    assert!(
        same(actual, bytes(expected)),
        "bytes mismatch, got {:?}",
        pyx_to_str(actual)
    );
}

fn error_tag() -> i64 {
    assert_eq!(pyx_exc_pending(), 1, "expected a pending exception");
    pyx_exc_tag()
}

fn error_message() -> i64 {
    pyx_to_str(pyx_exc_value())
}

/// Compares a pending exception message; the exception stays pending.
fn assert_error_text(actual: i64, expected: &str) {
    assert!(
        same(actual, text(expected)),
        "message mismatch, got {:?}",
        pyx_to_str(actual)
    );
}

fn clear_error() {
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
            "pyx_pathlib_{}_{}_{}",
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
fn constructor_defaults_cwd_home_and_repr() {
    pyx_runtime_init();

    assert_path(call("pathlib", "Path", &[]), ".");
    assert_path(call("pathlib", "Path", &[pyx_none()]), ".");
    assert_path(call("pathlib", "Path", &[text("a/b")]), "a/b");

    // A path is accepted as the constructor argument and copied.
    let original = path("x/y");
    assert_path(call("pathlib", "Path", &[original]), "x/y");

    // Wrong argument type.
    let bad = call("pathlib", "Path", &[pyx_int(1)]);
    assert_eq!(bad, pyx_none());
    assert_eq!(error_tag(), 1);
    clear_error();

    let cwd = call("pathlib", "cwd", &[]);
    let expected = std::env::current_dir().expect("current dir");
    assert_path(cwd, &expected.to_string_lossy());

    let home = call("pathlib", "home", &[]);
    let expected = std::env::var_os("HOME")
        .map(|home| home.to_string_lossy().into_owned())
        .filter(|home| !home.is_empty())
        .unwrap_or_else(|| ".".to_string());
    assert_path(home, &expected);

    // `str(Path)` is the path, `repr(Path)` names the concrete class.
    let value = path("a/b");
    assert_str(pyx_to_str(value), "a/b");
    let items = [value];
    let list = pyx_list_new(1, items.as_ptr());
    assert_str(
        pyx_to_str(list),
        if cfg!(windows) {
            "[WindowsPath('a/b')]"
        } else {
            "[PosixPath('a/b')]"
        },
    );

    // Attribute-like accessors are methods in this runtime.
    method(value, "nope", &[]);
    assert_eq!(error_tag(), 9);
    assert_error_text(error_message(), "'Path' object has no attribute 'nope'");
    clear_error();
}

#[test]
fn name_stem_suffix_parent_and_joinpath() {
    pyx_runtime_init();

    let archive = path("dir/archive.tar.gz");
    assert_str(method(archive, "name", &[]), "archive.tar.gz");
    assert_str(method(archive, "stem", &[]), "archive.tar");
    assert_str(method(archive, "suffix", &[]), ".gz");
    assert_path(method(archive, "parent", &[]), "dir");

    assert_str(method(path("a"), "name", &[]), "a");
    assert_path(method(path("a"), "parent", &[]), ".");
    assert_path(method(path("/a"), "parent", &[]), "/");
    assert_str(method(path("/a"), "name", &[]), "a");
    assert_str(method(path("/"), "name", &[]), "");
    assert_path(method(path("/"), "parent", &[]), "/");
    assert_str(method(path("."), "name", &[]), "");
    assert_path(method(path("."), "parent", &[]), ".");
    assert_str(method(path("a."), "suffix", &[]), "");
    assert_str(method(path("a."), "stem", &[]), "a.");
    assert_str(method(path(".bashrc"), "suffix", &[]), "");
    assert_str(method(path(".bashrc"), "stem", &[]), ".bashrc");

    let joined = method(path("a"), "joinpath", &[text("b"), text("c.txt")]);
    assert_path(joined, "a/b/c.txt");
    let joined = method(path("a"), "joinpath", &[path("b"), text("c")]);
    assert_path(joined, "a/b/c");
    let joined = method(path("a"), "joinpath", &[text("/abs")]);
    assert_path(joined, "/abs");
    let joined = method(path("."), "joinpath", &[text("a")]);
    assert_path(joined, "a");
    // No parts leaves the path unchanged.
    assert_path(method(path("a/b"), "joinpath", &[]), "a/b");

    method(path("a"), "joinpath", &[pyx_int(1)]);
    assert_eq!(error_tag(), 1);
    clear_error();

    // Arity is checked.
    method(archive, "name", &[text("x")]);
    assert_eq!(error_tag(), 1);
    clear_error();

    assert_str(method(path("a/b"), "as_posix", &[]), "a/b");
}

#[test]
fn slash_operator_joins_paths() {
    pyx_runtime_init();

    let joined = pyx_binop(3, path("a"), text("b.txt"));
    assert_path(joined, "a/b.txt");

    let joined = pyx_binop(3, path("a"), path("b"));
    assert_path(joined, "a/b");

    let joined = pyx_binop(3, path("a"), text("/abs"));
    assert_path(joined, "/abs");

    let joined = pyx_binop(3, path("."), text("a"));
    assert_path(joined, "a");

    let root = path("/");
    let joined = pyx_binop(3, root, text("etc"));
    assert_path(joined, "/etc");

    // Non-path operands are a TypeError and never an integer division.
    let bad = pyx_binop(3, path("a"), pyx_int(1));
    assert_eq!(bad, pyx_none());
    assert_eq!(error_tag(), 1);
    clear_error();

    // `str / Path` stays unsupported.
    pyx_binop(3, text("a"), path("b"));
    assert_eq!(error_tag(), 1);
    clear_error();
}

#[test]
fn read_write_text_and_bytes() {
    pyx_runtime_init();
    let dir = TempDir::new("text");
    let file = dir.path.join("note.txt");
    let target = path(file.to_str().expect("UTF-8"));

    assert_int(method(target, "write_text", &[text("héllo")]), 5);
    assert_str(method(target, "read_text", &[]), "héllo");
    assert_str(method(target, "read_text", &[pyx_none()]), "héllo");
    assert_str(method(target, "read_text", &[text("utf-8")]), "héllo");

    // Only UTF-8 is implemented.
    method(target, "read_text", &[text("latin-1")]);
    assert_eq!(error_tag(), 0);
    clear_error();

    // Wrong text argument type.
    method(target, "write_text", &[pyx_int(1)]);
    assert_eq!(error_tag(), 1);
    clear_error();

    let data = [0u8, 1, 255, 65];
    assert_int(method(target, "write_bytes", &[bytes(&data)]), 4);
    assert_bytes(method(target, "read_bytes", &[]), &data);
    assert_eq!(std::fs::read(&file).expect("read back"), data);

    method(target, "write_bytes", &[text("not bytes")]);
    assert_eq!(error_tag(), 1);
    clear_error();

    // Writing replaces the content.
    assert_int(method(target, "write_text", &[text("replaced")]), 8);
    assert_str(method(target, "read_text", &[]), "replaced");
}

#[test]
fn open_reads_and_writes_through_files() {
    pyx_runtime_init();
    let dir = TempDir::new("open");
    let target = path(dir.path.join("io.txt").to_str().expect("UTF-8"));

    let file = method(target, "open", &[text("w")]);
    assert_no_error();
    assert_int(method(file, "write", &[text("hi")]), 2);
    method(file, "close", &[]);

    // Default mode is "r".
    let file = method(target, "open", &[]);
    assert_str(method(file, "read", &[]), "hi");
    method(file, "close", &[]);

    // Binary mode round-trips raw bytes.
    let file = method(target, "open", &[text("wb")]);
    assert_no_error();
    assert_int(method(file, "write", &[bytes(&[0, 255])]), 2);
    method(file, "close", &[]);
    assert_bytes(method(target, "read_bytes", &[]), &[0, 255]);

    let file = method(target, "open", &[text("rb")]);
    assert_bytes(method(file, "read", &[]), &[0, 255]);
    method(file, "close", &[]);

    // A binary file rejects a text encoding.
    method(target, "open", &[text("rb"), text("utf-8")]);
    assert_eq!(error_tag(), 0);
    clear_error();
}

#[test]
fn exists_is_file_is_dir() {
    pyx_runtime_init();
    let dir = TempDir::new("exists");
    let target = path(dir.path.join("file.txt").to_str().expect("UTF-8"));
    let directory = path(&dir.text());

    assert_false(method(target, "exists", &[]));
    assert_false(method(target, "is_file", &[]));
    assert_false(method(target, "is_dir", &[]));

    assert_int(method(target, "write_text", &[text("x")]), 1);
    assert_true(method(target, "exists", &[]));
    assert_true(method(target, "is_file", &[]));
    assert_false(method(target, "is_dir", &[]));

    assert_true(method(directory, "exists", &[]));
    assert_true(method(directory, "is_dir", &[]));
    assert_false(method(directory, "is_file", &[]));

    assert_eq!(pyx_truthy(target), 1);
}

#[test]
fn mkdir_parents_and_exist_ok() {
    pyx_runtime_init();
    let dir = TempDir::new("mkdir");

    let nested = path(dir.path.join("a/b").to_str().expect("UTF-8"));
    // Without `parents` the missing "a" makes this fail.
    method(nested, "mkdir", &[]);
    assert_eq!(error_tag(), 10);
    clear_error();

    method(nested, "mkdir", &[pyx_true()]);
    assert_no_error();
    assert_true(method(nested, "is_dir", &[]));

    // `exist_ok=True` tolerates the existing directory.
    method(nested, "mkdir", &[pyx_true(), pyx_true()]);
    assert_no_error();

    // `exist_ok=False` reports the conflict.
    method(nested, "mkdir", &[pyx_true(), pyx_false()]);
    assert_eq!(error_tag(), 10);
    clear_error();

    // The single-level form also checks `exist_ok`.
    method(nested, "mkdir", &[]);
    assert_eq!(error_tag(), 10);
    clear_error();
    method(nested, "mkdir", &[pyx_false(), pyx_true()]);
    assert_no_error();

    let single = path(dir.path.join("single").to_str().expect("UTF-8"));
    method(single, "mkdir", &[]);
    assert_no_error();
    method(single, "mkdir", &[]);
    assert_eq!(error_tag(), 10);
    clear_error();

    // Wrong flag type.
    method(single, "mkdir", &[pyx_int(1)]);
    assert_eq!(error_tag(), 1);
    clear_error();
}

#[test]
fn keyword_arguments_bind_path_methods() {
    pyx_runtime_init();
    let dir = TempDir::new("kw");

    let nested = path(dir.path.join("a/b").to_str().expect("UTF-8"));
    method_kw(
        nested,
        "mkdir",
        &[],
        &[("parents", pyx_true()), ("exist_ok", pyx_true())],
    );
    assert_no_error();
    assert_true(method(nested, "is_dir", &[]));

    // `exist_ok=True` tolerates the existing directory.
    method_kw(nested, "mkdir", &[], &[("exist_ok", pyx_true())]);
    assert_no_error();

    let file = path(dir.path.join("kw.txt").to_str().expect("UTF-8"));
    let handle = method_kw(file, "open", &[], &[("mode", text("wb"))]);
    assert_no_error();
    assert_int(method(handle, "write", &[bytes(&[7, 8])]), 2);
    // File methods reject keyword arguments explicitly.
    method_kw(handle, "write", &[], &[("data", bytes(&[9]))]);
    assert_eq!(error_tag(), 1);
    clear_error();
    method(handle, "close", &[]);
    assert_bytes(method(file, "read_bytes", &[]), &[7, 8]);

    let written = method_kw(
        file,
        "write_text",
        &[text("hé")],
        &[("encoding", text("utf-8"))],
    );
    assert_int(written, 2);
    let read = method_kw(file, "read_text", &[], &[("encoding", text("utf-8"))]);
    assert_str(read, "hé");

    // Keyword and positional values for the same parameter conflict.
    method_kw(nested, "mkdir", &[pyx_true()], &[("parents", pyx_true())]);
    assert_eq!(error_tag(), 1);
    clear_error();

    // Unknown keywords and missing required parameters are runtime errors.
    method_kw(nested, "mkdir", &[], &[("nope", pyx_true())]);
    assert_eq!(error_tag(), 1);
    clear_error();
    method_kw(file, "write_text", &[], &[("encoding", text("utf-8"))]);
    assert_eq!(error_tag(), 1);
    clear_error();
}

#[test]
fn iterdir_and_glob_are_sorted() {
    pyx_runtime_init();
    let dir = TempDir::new("iter");

    for name in ["c.txt", "a.txt", "b.txt", "a.md"] {
        let target = path(dir.path.join(name).to_str().expect("UTF-8"));
        assert_int(method(target, "write_text", &[text("x")]), 1);
    }
    let sub = dir.path.join("sub");
    std::fs::create_dir(&sub).expect("create sub");
    let nested = path(sub.join("x.txt").to_str().expect("UTF-8"));
    assert_int(method(nested, "write_text", &[text("y")]), 1);

    let directory = path(&dir.text());
    let entries = method(directory, "iterdir", &[]);
    assert_no_error();
    let length = pyx_to_int(pyx_len(entries)) >> 3;
    assert_eq!(length, 5);
    let expected = ["a.md", "a.txt", "b.txt", "c.txt", "sub"];
    for (index, name) in expected.iter().enumerate() {
        let item = pyx_subscript(entries, pyx_int(index as i64));
        assert_path(item, dir.path.join(*name).to_str().expect("UTF-8"));
    }

    let matches = method(directory, "glob", &[text("*.txt")]);
    assert_no_error();
    let length = pyx_to_int(pyx_len(matches)) >> 3;
    assert_eq!(length, 3);
    for (index, name) in ["a.txt", "b.txt", "c.txt"].iter().enumerate() {
        let item = pyx_subscript(matches, pyx_int(index as i64));
        assert_path(item, dir.path.join(*name).to_str().expect("UTF-8"));
    }

    // `?` matches exactly one character; nested patterns join with `/`.
    let matches = method(directory, "glob", &[text("?.md")]);
    assert_no_error();
    assert_eq!(pyx_to_int(pyx_len(matches)) >> 3, 1);

    let matches = method(directory, "glob", &[text("sub/*.txt")]);
    assert_no_error();
    assert_eq!(pyx_to_int(pyx_len(matches)) >> 3, 1);
    let item = pyx_subscript(matches, pyx_int(0));
    assert_path(item, sub.join("x.txt").to_str().expect("UTF-8"));

    // No matches and a missing directory both yield an empty list.
    let matches = method(directory, "glob", &[text("*.nope")]);
    assert_no_error();
    assert_eq!(pyx_to_int(pyx_len(matches)), 0);
    let missing = path(dir.path.join("missing").to_str().expect("UTF-8"));
    let matches = method(missing, "glob", &[text("*")]);
    assert_no_error();
    assert_eq!(pyx_to_int(pyx_len(matches)), 0);

    // Wrong argument type.
    method(directory, "glob", &[pyx_int(1)]);
    assert_eq!(error_tag(), 1);
    clear_error();
}

#[test]
fn missing_paths_and_bad_encodings_raise() {
    pyx_runtime_init();
    let dir = TempDir::new("errors");
    let missing = path(dir.path.join("nope.txt").to_str().expect("UTF-8"));

    let value = method(missing, "read_text", &[]);
    assert_eq!(value, pyx_none());
    assert_eq!(error_tag(), 10);
    clear_error();

    let value = method(missing, "read_bytes", &[]);
    assert_eq!(value, pyx_none());
    assert_eq!(error_tag(), 10);
    clear_error();

    let existing = path(dir.path.join("file.txt").to_str().expect("UTF-8"));
    assert_int(method(existing, "write_text", &[text("x")]), 1);
    method(existing, "read_text", &[text("latin-1")]);
    assert_eq!(error_tag(), 0);
    clear_error();
    method(existing, "write_text", &[text("x"), text("latin-1")]);
    assert_eq!(error_tag(), 0);
    clear_error();

    // `iterdir` on a missing directory is an OSError as well.
    method(missing, "iterdir", &[]);
    assert_eq!(error_tag(), 10);
    clear_error();
}

#[test]
fn absolute_and_resolve() {
    pyx_runtime_init();
    let dir = TempDir::new("resolve");
    let cwd = std::env::current_dir().expect("current dir");
    let cwd_text = cwd.to_string_lossy().into_owned();

    let relative = method(path("some/dir"), "absolute", &[]);
    assert_path(relative, &format!("{cwd_text}/some/dir"));

    let absolute = method(path("/etc"), "absolute", &[]);
    assert_path(absolute, "/etc");

    let resolved = method(path(&dir.text()), "resolve", &[]);
    let canonical = std::fs::canonicalize(&dir.path).expect("canonicalize temp dir");
    assert_path(resolved, &canonical.to_string_lossy());

    // Missing paths fall back to the absolute form instead of failing.
    let missing = path(dir.path.join("missing/../x").to_str().expect("UTF-8"));
    let resolved = method(missing, "resolve", &[]);
    assert_path(
        resolved,
        &format!("{}/missing/../x", dir.path.to_str().expect("UTF-8")),
    );
}

#[test]
fn paths_survive_collection() {
    pyx_runtime_init();
    let dir = TempDir::new("gc");
    let target = path(dir.path.join("kept.txt").to_str().expect("UTF-8"));
    assert_int(method(target, "write_text", &[text("kept")]), 4);

    // Keep one path in a list and one in a dict, then allocate garbage and
    // force a collection.
    let items = [target];
    let list = pyx_list_new(1, items.as_ptr());
    let keys = [text("path")];
    let values = [target];
    let dict = pyx_dict_new(1, keys.as_ptr(), values.as_ptr());
    assert_no_error();

    for _ in 0..200 {
        let _ = pyx_str_new(b"garbage".as_ptr(), 7);
    }
    pyx_gc_collect();

    let from_list = pyx_subscript(list, pyx_int(0));
    assert_str(method(from_list, "read_text", &[]), "kept");
    let from_dict = pyx_subscript(dict, text("path"));
    assert_str(method(from_dict, "read_text", &[]), "kept");

    // A longer chain of joined copies survives as well.
    let mut chained = path(&dir.text());
    for _ in 0..20 {
        chained = method(chained, "joinpath", &[text("a")]);
    }
    pyx_gc_collect();
    assert_path(method(chained, "parent", &[]), &{
        let mut expected = dir.text();
        for _ in 0..19 {
            expected.push_str("/a");
        }
        expected
    });

    // The path string remains usable after the collection.
    assert_str(method(target, "read_text", &[]), "kept");
}
