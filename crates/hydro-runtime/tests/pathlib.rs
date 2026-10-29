//! Integration tests for `pathlib.Path`: construction, path arithmetic, file
//! and directory helpers, iteration, globbing, error reporting and GC
//! survival.
//!
//! Every test drives the runtime through the public ABI and uses its own
//! temporary directory, removed on drop.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use hydro_runtime::abi::*;

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

/// Calls a first-class runtime value (for example a cached class constant)
/// through the normal call protocol instead of module dispatch.
fn call_value(callable: i64, args: &[i64]) -> i64 {
    hydro_call(
        callable,
        args.len(),
        if args.is_empty() {
            std::ptr::null()
        } else {
            args.as_ptr()
        },
    )
}

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    hydro_method(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

fn method_kw(obj: i64, name: &str, args: &[i64], kwargs: &[(&str, i64)]) -> i64 {
    let mut pairs = Vec::with_capacity(kwargs.len() * 2);
    for (key, value) in kwargs {
        pairs.push(text(key));
        pairs.push(*value);
    }
    hydro_method_kw(
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
    assert_eq!(hydro_exc_pending(), 0, "Path() raised");
    value
}

fn same(a: i64, b: i64) -> bool {
    call("operator", "eq", &[a, b]) == hydro_true()
}

const OP_IS: u32 = 15;

fn is_same(a: i64, b: i64) -> bool {
    hydro_binop(OP_IS, a, b) == hydro_true()
}

fn get(module: &str, name: &str) -> i64 {
    hydro_module_get(module.as_ptr(), module.len(), name.as_ptr(), name.len())
}

fn getattr(object: i64, name: &str) -> i64 {
    hydro_getattr(object, name.as_ptr(), name.len())
}

fn call_kw(callable: i64, args: &[i64], kwargs: &[(&str, i64)]) -> i64 {
    let mut pairs = Vec::with_capacity(kwargs.len() * 2);
    for (key, value_in) in kwargs {
        pairs.push(text(key));
        pairs.push(*value_in);
    }
    hydro_call_kw(
        callable,
        args.len(),
        if args.is_empty() {
            std::ptr::null()
        } else {
            args.as_ptr()
        },
        kwargs.len(),
        pairs.as_ptr(),
    )
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

fn assert_true(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_true(), "expected True");
}

fn assert_false(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_false(), "expected False");
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(hydro_to_int(actual), expected << 3);
}

fn assert_bytes(actual: i64, expected: &[u8]) {
    assert_no_error();
    assert!(
        same(actual, bytes(expected)),
        "bytes mismatch, got {:?}",
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

/// Compares a pending exception message; the exception stays pending.
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

/// A unique temporary directory removed when the test ends.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hydro_pathlib_{}_{}_{}",
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
    hydro_runtime_init();

    assert_path(call("pathlib", "Path", &[]), ".");
    assert_path(call("pathlib", "Path", &[hydro_none()]), ".");
    assert_path(call("pathlib", "Path", &[text("a/b")]), "a/b");

    // A path is accepted as the constructor argument and copied.
    let original = path("x/y");
    assert_path(call("pathlib", "Path", &[original]), "x/y");

    // Wrong argument type.
    let bad = call("pathlib", "Path", &[hydro_int(1)]);
    assert_eq!(bad, hydro_none());
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
    assert_str(hydro_to_str(value), "a/b");
    let items = [value];
    let list = hydro_list_new(1, items.as_ptr());
    assert_str(
        hydro_to_str(list),
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
    hydro_runtime_init();

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

    method(path("a"), "joinpath", &[hydro_int(1)]);
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
    hydro_runtime_init();

    let joined = hydro_binop(3, path("a"), text("b.txt"));
    assert_path(joined, "a/b.txt");

    let joined = hydro_binop(3, path("a"), path("b"));
    assert_path(joined, "a/b");

    let joined = hydro_binop(3, path("a"), text("/abs"));
    assert_path(joined, "/abs");

    let joined = hydro_binop(3, path("."), text("a"));
    assert_path(joined, "a");

    let root = path("/");
    let joined = hydro_binop(3, root, text("etc"));
    assert_path(joined, "/etc");

    // Non-path operands are a TypeError and never an integer division.
    let bad = hydro_binop(3, path("a"), hydro_int(1));
    assert_eq!(bad, hydro_none());
    assert_eq!(error_tag(), 1);
    clear_error();

    // `str / Path` stays unsupported.
    hydro_binop(3, text("a"), path("b"));
    assert_eq!(error_tag(), 1);
    clear_error();
}

#[test]
fn read_write_text_and_bytes() {
    hydro_runtime_init();
    let dir = TempDir::new("text");
    let file = dir.path.join("note.txt");
    let target = path(file.to_str().expect("UTF-8"));

    assert_int(method(target, "write_text", &[text("héllo")]), 5);
    assert_str(method(target, "read_text", &[]), "héllo");
    assert_str(method(target, "read_text", &[hydro_none()]), "héllo");
    assert_str(method(target, "read_text", &[text("utf-8")]), "héllo");

    // Only UTF-8 is implemented.
    method(target, "read_text", &[text("latin-1")]);
    assert_eq!(error_tag(), 0);
    clear_error();

    // Wrong text argument type.
    method(target, "write_text", &[hydro_int(1)]);
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
    hydro_runtime_init();
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
    hydro_runtime_init();
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

    assert_eq!(hydro_truthy(target), 1);
}

#[test]
fn mkdir_parents_and_exist_ok() {
    hydro_runtime_init();
    let dir = TempDir::new("mkdir");

    let nested = path(dir.path.join("a/b").to_str().expect("UTF-8"));
    // Without `parents` the missing "a" makes this fail.
    method(nested, "mkdir", &[]);
    assert_eq!(error_tag(), 10);
    clear_error();

    method(nested, "mkdir", &[hydro_true()]);
    assert_no_error();
    assert_true(method(nested, "is_dir", &[]));

    // `exist_ok=True` tolerates the existing directory.
    method(nested, "mkdir", &[hydro_true(), hydro_true()]);
    assert_no_error();

    // `exist_ok=False` reports the conflict.
    method(nested, "mkdir", &[hydro_true(), hydro_false()]);
    assert_eq!(error_tag(), 10);
    clear_error();

    // The single-level form also checks `exist_ok`.
    method(nested, "mkdir", &[]);
    assert_eq!(error_tag(), 10);
    clear_error();
    method(nested, "mkdir", &[hydro_false(), hydro_true()]);
    assert_no_error();

    let single = path(dir.path.join("single").to_str().expect("UTF-8"));
    method(single, "mkdir", &[]);
    assert_no_error();
    method(single, "mkdir", &[]);
    assert_eq!(error_tag(), 10);
    clear_error();

    // Wrong flag type.
    method(single, "mkdir", &[hydro_int(1)]);
    assert_eq!(error_tag(), 1);
    clear_error();
}

#[test]
fn keyword_arguments_bind_path_methods() {
    hydro_runtime_init();
    let dir = TempDir::new("kw");

    let nested = path(dir.path.join("a/b").to_str().expect("UTF-8"));
    method_kw(
        nested,
        "mkdir",
        &[],
        &[("parents", hydro_true()), ("exist_ok", hydro_true())],
    );
    assert_no_error();
    assert_true(method(nested, "is_dir", &[]));

    // `exist_ok=True` tolerates the existing directory.
    method_kw(nested, "mkdir", &[], &[("exist_ok", hydro_true())]);
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
    method_kw(
        nested,
        "mkdir",
        &[hydro_true()],
        &[("parents", hydro_true())],
    );
    assert_eq!(error_tag(), 1);
    clear_error();

    // Unknown keywords and missing required parameters are runtime errors.
    method_kw(nested, "mkdir", &[], &[("nope", hydro_true())]);
    assert_eq!(error_tag(), 1);
    clear_error();
    method_kw(file, "write_text", &[], &[("encoding", text("utf-8"))]);
    assert_eq!(error_tag(), 1);
    clear_error();
}

#[test]
fn iterdir_and_glob_are_sorted() {
    hydro_runtime_init();
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
    let length = hydro_to_int(hydro_len(entries)) >> 3;
    assert_eq!(length, 5);
    let expected = ["a.md", "a.txt", "b.txt", "c.txt", "sub"];
    for (index, name) in expected.iter().enumerate() {
        let item = hydro_subscript(entries, hydro_int(index as i64));
        assert_path(item, dir.path.join(*name).to_str().expect("UTF-8"));
    }

    let matches = method(directory, "glob", &[text("*.txt")]);
    assert_no_error();
    let length = hydro_to_int(hydro_len(matches)) >> 3;
    assert_eq!(length, 3);
    for (index, name) in ["a.txt", "b.txt", "c.txt"].iter().enumerate() {
        let item = hydro_subscript(matches, hydro_int(index as i64));
        assert_path(item, dir.path.join(*name).to_str().expect("UTF-8"));
    }

    // `?` matches exactly one character; nested patterns join with `/`.
    let matches = method(directory, "glob", &[text("?.md")]);
    assert_no_error();
    assert_eq!(hydro_to_int(hydro_len(matches)) >> 3, 1);

    let matches = method(directory, "glob", &[text("sub/*.txt")]);
    assert_no_error();
    assert_eq!(hydro_to_int(hydro_len(matches)) >> 3, 1);
    let item = hydro_subscript(matches, hydro_int(0));
    assert_path(item, sub.join("x.txt").to_str().expect("UTF-8"));

    // No matches and a missing directory both yield an empty list.
    let matches = method(directory, "glob", &[text("*.nope")]);
    assert_no_error();
    assert_eq!(hydro_to_int(hydro_len(matches)), 0);
    let missing = path(dir.path.join("missing").to_str().expect("UTF-8"));
    let matches = method(missing, "glob", &[text("*")]);
    assert_no_error();
    assert_eq!(hydro_to_int(hydro_len(matches)), 0);

    // Wrong argument type.
    method(directory, "glob", &[hydro_int(1)]);
    assert_eq!(error_tag(), 1);
    clear_error();
}

#[test]
fn missing_paths_and_bad_encodings_raise() {
    hydro_runtime_init();
    let dir = TempDir::new("errors");
    let missing = path(dir.path.join("nope.txt").to_str().expect("UTF-8"));

    let value = method(missing, "read_text", &[]);
    assert_eq!(value, hydro_none());
    assert_eq!(error_tag(), 10);
    clear_error();

    let value = method(missing, "read_bytes", &[]);
    assert_eq!(value, hydro_none());
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
    hydro_runtime_init();
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
    hydro_runtime_init();
    let dir = TempDir::new("gc");
    let target = path(dir.path.join("kept.txt").to_str().expect("UTF-8"));
    assert_int(method(target, "write_text", &[text("kept")]), 4);

    // Keep one path in a list and one in a dict, then allocate garbage and
    // force a collection.
    let items = [target];
    let list = hydro_list_new(1, items.as_ptr());
    let keys = [text("path")];
    let values = [target];
    let dict = hydro_dict_new(1, keys.as_ptr(), values.as_ptr());
    assert_no_error();

    for _ in 0..200 {
        let _ = hydro_str_new(b"garbage".as_ptr(), 7);
    }
    hydro_gc_collect();

    let from_list = hydro_subscript(list, hydro_int(0));
    assert_str(method(from_list, "read_text", &[]), "kept");
    let from_dict = hydro_subscript(dict, text("path"));
    assert_str(method(from_dict, "read_text", &[]), "kept");

    // A longer chain of joined copies survives as well.
    let mut chained = path(&dir.text());
    for _ in 0..20 {
        chained = method(chained, "joinpath", &[text("a")]);
    }
    hydro_gc_collect();
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

#[test]
fn pure_path_classes_construct_and_compare() {
    hydro_runtime_init();
    assert_no_error();
    // Pydantic 2.14 looks every class up by identity and uses it as a dict
    // key/annotation origin; repeated reads must return the cached class.
    for name in [
        "PurePath",
        "PosixPath",
        "WindowsPath",
        "PurePosixPath",
        "PureWindowsPath",
    ] {
        let class = get("pathlib", name);
        assert_no_error();
        assert!(
            is_same(class, get("pathlib", name)),
            "pathlib.{name} identity is not cached"
        );
        assert_str(getattr(class, "__name__"), name);
    }

    // `PurePath` shares the native path value factory with `Path`.
    let pure = get("pathlib", "PurePath");
    let first = call_value(pure, &[text("a/b")]);
    assert_no_error();
    assert_path(first, "a/b");
    assert_str(call("os", "fspath", &[first]), "a/b");
    assert_str(method(first, "__fspath__", &[]), "a/b");

    // Construction from another path copies it; equality is textual.
    let copy = call_value(pure, &[first]);
    assert_no_error();
    assert_path(copy, "a/b");
    assert!(same(first, copy));
    assert!(same(first, call_value(pure, &[text("a/b")])));
    assert!(!same(first, call_value(pure, &[text("a/c")])));
    assert!(!same(first, text("a/b")), "a path never equals a str");

    // Omitted/`None` arguments select ".", like `pathlib.Path`.
    assert_path(call_value(pure, &[]), ".");
    assert_path(call_value(pure, &[hydro_none()]), ".");

    // The concrete classes follow the host platform, matching CPython's
    // `cannot instantiate ... on your system` check.
    if cfg!(windows) {
        let _ = call_value(get("pathlib", "PosixPath"), &[text("a")]);
        assert_eq!(error_tag(), 32);
        assert_error_text(
            error_message(),
            "cannot instantiate 'PosixPath' on your system",
        );
        clear_error();
        assert_path(call_value(get("pathlib", "WindowsPath"), &[text("a")]), "a");
    } else {
        assert_path(call_value(get("pathlib", "PosixPath"), &[text("a")]), "a");
        let _ = call_value(get("pathlib", "WindowsPath"), &[text("a")]);
        assert_eq!(error_tag(), 32);
        assert_error_text(
            error_message(),
            "cannot instantiate 'WindowsPath' on your system",
        );
        clear_error();
    }

    // The pure classes are platform-independent. Windows text is preserved
    // verbatim: this runtime does not re-parse it (documented limitation).
    assert_path(
        call_value(get("pathlib", "PureWindowsPath"), &[text("C:\\x")]),
        "C:\\x",
    );
    assert_path(
        call_value(get("pathlib", "PurePosixPath"), &[text("a/b")]),
        "a/b",
    );

    // Constructor diagnostics: wrong type, too many segments, keywords.
    let _ = call_value(pure, &[hydro_int(1)]);
    assert_eq!(error_tag(), 1);
    assert_error_text(error_message(), "PurePath() argument must be str, not int");
    clear_error();
    let _ = call_value(pure, &[text("a"), text("b")]);
    assert_eq!(error_tag(), 1);
    assert_error_text(
        error_message(),
        "PurePath() expected at most 1 argument, got 2",
    );
    clear_error();
    let _ = call_kw(pure, &[], &[("path", text("a"))]);
    assert_eq!(error_tag(), 1);
    assert_error_text(error_message(), "PurePath() takes no keyword arguments");
    clear_error();
}

#[test]
fn pure_path_values_compare_nested_and_survive_collection() {
    hydro_runtime_init();
    let pure = get("pathlib", "PurePosixPath");
    let left = [call_value(pure, &[text("x/y")])];
    let right = [call_value(pure, &[text("x/y")])];
    assert_no_error();
    assert!(same(left[0], right[0]));

    // Nested in a tuple the same textual equality applies.
    let left_tuple = hydro_tuple_new(1, left.as_ptr());
    let right_tuple = hydro_tuple_new(1, right.as_ptr());
    assert!(same(left_tuple, right_tuple));

    // Paths held only by containers survive a collection.
    let values = [left[0]];
    let list = hydro_list_new(1, values.as_ptr());
    for _ in 0..200 {
        let _ = hydro_str_new(b"garbage".as_ptr(), 7);
    }
    hydro_gc_collect();
    let recovered = hydro_subscript(list, hydro_int(0));
    assert_path(recovered, "x/y");
    assert_str(method(recovered, "as_posix", &[]), "x/y");
}
