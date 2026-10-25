//! End-to-end tests for the `pyx` CLI.
//!
//! These tests drive the built binary through `CARGO_BIN_EXE_pyx` and only use
//! the standard library. All source paths are absolute so the tests do not
//! depend on the process working directory.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

/// Returns the absolute, canonical path to the repository's `examples` dir.
fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .canonicalize()
        .expect("examples directory must exist")
}

/// Absolute path to an example program.
fn example(relative: &str) -> String {
    examples_dir().join(relative).to_string_lossy().into_owned()
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

fn output_parts(output: Output) -> (i32, String, String) {
    (
        exit_code(output.status),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Runs the `pyx` binary and returns `(exit code, stdout, stderr)`.
fn run(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_pyx"))
        .args(args)
        .output()
        .expect("failed to spawn pyx binary");
    output_parts(output)
}

/// Runs the `pyx` binary with additional environment variables set on the
/// child process, so GC tuning never leaks into other tests.
fn run_with_env(args: &[&str], envs: &[(&str, &str)]) -> (i32, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pyx"));
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.output().expect("failed to spawn pyx binary");
    output_parts(output)
}

/// Stdout produced by `examples/fib.py`.
const FIB_STDOUT: &str = "0\n1\n1\n2\n3\n5\n8\n13\n21\n34\n";

/// Stdout produced by `examples/collections.py`.
const COLLECTIONS_STDOUT: &str = "[10, 20, 30]\n\
     10 30 3\n\
     [10, 99, 30]\n\
     True [10, 99, 30, 40]\n\
     139\n\
     3\n\
     {'a': 10, 'b': 2, 'c': 3}\n\
     3 2\n\
     a\n\
     b\n\
     c\n\
     missing 'missing'\n\
     abc\n";

/// Stdout produced by `examples/finally.py`.
const FINALLY_STDOUT: &str = "body\n\
     finally\n\
     1\n\
     cleanup\n\
     7\n\
     inner-finally\n\
     outer-finally\n\
     3\n\
     ok\n\
     else\n\
     fin\n\
     inner\n\
     else-inner\n\
     fin-inner\n\
     loop 1\n\
     finally 1\n\
     finally 2\n\
     loop 3\n\
     finally 3\n\
     finally 4\n\
     after 4\n\
     propagate-finally\n\
     caught boom\n\
     try-else\n\
     caught-else from else\n\
     bare caught\n\
     after-break\n\
     second wins second\n";

/// Stdout produced by `examples/slices.py`.
const SLICES_STDOUT: &str = "bcd\n\
     abc\n\
     def\n\
     def\n\
     ace\n\
     fedcba\n\
     bcdef\n\
     ab\n\
     2\n\
     [1, 2, 3]\n\
     [0, 1]\n\
     [4, 5]\n\
     [4, 5]\n\
     [0, 2, 4]\n\
     [5, 4, 3, 2, 1, 0]\n\
     []\n\
     [1, 3]\n\
     [0]\n\
     [0, 1, 2, 3, 4, 5]\n\
     [0, 1, 2, 3, 4, 5]\n\
     zero slice step cannot be zero\n\
     él olléh\n\
     [0, 9, 8, 3, 4, 5]\n\
     [0, 7, 6, 5, 8, 3, 4, 5]\n\
     [0, 8, 3, 4, 5]\n\
     [4, 3, 4, 5]\n\
     [4, 3, 4, 8, 9]\n\
     [4, 3, 5, 4, 8, 9]\n\
     [0, 3, 0, 4, 0, 9]\n\
     [6, 5, 4, 3, 2, 1]\n\
     [6, 5, 4, 3, 2, 1]\n\
     [6, 'a', 'b', 4, 3, 2, 1]\n\
     eval value\n\
     eval start\n\
     eval stop\n\
     [0, 9, 2]\n\
     extended attempt to assign sequence of size 1 to extended slice of size 4\n\
     immutable 'str' object does not support item assignment\n";

/// Stdout produced by `examples/fstrings.py`.
const FSTRINGS_STDOUT: &str = "Hello, PyX!\n\
     7 + 7 = 14\n\
     pi=3.5\n\
     list=[1, 2], none=None, flag=True\n\
     nested\n\
     {literal} PyX\n\
     PyX\n\
     \n";

/// Stdout produced by `examples/methods.py`.
const METHODS_STDOUT: &str = "Hello World\n\
     HELLO WORLD\n\
     ['a', 'b', 'c']\n\
     ['a', 'b', 'c']\n\
     a-b-c\n\
     bonono\n\
     bonona\n\
     True True\n\
     2 2\n\
     True True True True\n\
     [1, 2, 3, 4, 5] 5\n\
     5 [1, 2, 3, 4]\n\
     [4, 3, 1, 0] 1 1\n\
     [4, 3, 1, 0] [4, 3, 1, 0, 99]\n\
     1 None 0\n\
     {'a': 1, 'b': 2, 'c': 3}\n\
     a\n\
     b\n\
     c\n\
     1\n\
     2\n\
     3\n\
     1 {'b': 2, 'c': 3}\n\
     -1\n\
     attr 'str' object has no attribute 'nope'\n\
     type list.append() takes exactly one argument (0 given)\n\
     index pop from empty list\n\
     key 'z'\n\
     join sequence item 0: expected str instance, int found\n";

/// Stdout produced by `examples/sets.py`.
const SETS_STDOUT: &str = "3 True False True\n\
     True True True True True\n\
     4 True\n\
     3 False\n\
     remove 9\n\
     3 4 False True\n\
     5 True True\n\
     0 False True\n\
     1 True\n\
     3 True False\n\
     12\n\
     True True\n\
     hash unhashable type: 'list'\n";

/// Stdout produced by `examples/tuples.py`.
const TUPLES_STDOUT: &str = "(1, 2, 3) 3 1 3\n\
     (2, 3) (3, 2, 1) (1,)\n\
     () (1,) True False\n\
     (1, 2, 3, 4) (1, 2, 1, 2, 1, 2) (0, 0)\n\
     6\n\
     1 2 3\n\
     4 5\n\
     a 1\n\
     b 2\n\
     tuple-key 3\n\
     1 1\n\
     True True\n\
     (1, 'a', None)\n\
     unpack too many values to unpack (expected 2)\n\
     unpack not enough values to unpack (expected 3, got 2)\n\
     index tuple index out of range\n\
     hash unhashable type: 'list'\n";

/// Stdout produced by `examples/builtins.py`.
const BUILTINS_STDOUT: &str = "range(0, 5) 5 [0, 1, 2, 3, 4] 10\n\
     range(0, 10, 2) range(5, 0, -2)\n\
     45 25 145\n\
     [2, 4, 6] [5, 3, 1]\n\
     ['a', 'b', 'c'] [1, 2] ['a', 'b']\n\
     3 True\n\
     {1: 'a', 2: 'b'}\n\
     {1: 'a', 2: 'b'}\n\
     0 a\n\
     1 b\n\
     10 a\n\
     11 b\n\
     1 x\n\
     2 y\n\
     [(1, 3), (2, 4)] [(0, 'x')]\n\
     6 3.5 7\n\
     True False False\n\
     0 1\n\
     zero range() arg 3 must not be zero\n\
     sum unsupported operand type(s) for +: 'int' and 'str'\n\
     65 233 A é 😀\n\
     z\n\
     ord ord() expected a character, but string of length 2 found\n\
     chr chr() arg not in range(0x110000)\n\
     chr chr() arg cannot be a surrogate code point\n\
     walrus 10\n\
     10\n\
     loop 1\n\
     loop 2\n\
     after 3\n\
     [2, 4, 6]\n\
     6\n\
     True 1\n\
     False 2\n\
     True 4\n\
     False 3\n";

/// Stdout produced by `examples/comprehensions.py`.
const COMPREHENSIONS_STDOUT: &str = "[0, 2, 4, 6, 8]\n\
     [0, 3, 6, 9]\n\
     [0, 1, 1, 2, 2, 3]\n\
     [[], [0], [0, 1]]\n\
     {0: 0, 1: 1, 2: 4, 3: 9}\n\
     3 True True True\n\
     45\n\
     0123\n\
     [1, 2]\n\
     {1: 'a', 2: 'b'}\n\
     ['b']\n\
     [3, 4, 5]\n\
     [0, 1, 2] 100\n\
     [1]\n\
     [(0, 1), (1, 0)]\n\
     [0, 1, 2]\n\
     [0, 1]\n";

/// Stdout produced by `examples/functions.py`.
const FUNCTIONS_STDOUT: &str = "5\n\
     [0, 2, 4, 6, 8]\n\
     [0, 2, 4, 6]\n\
     [1, 'x', [1]]\n\
     [1, 2, 3]\n\
     7\n\
     20\n\
     15 15\n\
     11 9\n\
     hi!\n";

/// Stdout produced by `examples/modules.py`.
const MODULES_STDOUT: &str = "3.0\n\
     4.0 True\n\
     2 3 -2 3.5\n\
     8.0 1.0 1.0\n\
     0.0 1.0\n\
     True True\n\
     5 20 ab\n\
     10\n\
     24\n\
     10\n\
     13\n\
     10\n\
     True True\n\
     True\n\
     True\n\
     True\n\
     True True\n";

/// Stdout produced by `examples/json_os.py`.
const JSON_OS_STDOUT: &str = "{\"name\": \"PyX\", \"n\": 3, \"ok\": true, \"items\": [1, 2.5, null], \"nested\": {\"x\": \"y\"}}\n\
     PyX 3 True 2.5 y\n\
     [1, 'a', True, None, 2.5]\n\
     \"a\\\"b\\nc\"\n\
     [1, 2.0, false, null]\n\
     2\n\
     {\"a\": [1, {\"b\": \"c\"}]}\n\
     é€\n\
     \"\\u00e9\\u20ac\"\n\
     True True True True\n\
     True\n\
     fallback\n\
     None\n\
     bad\n\
     dumps\n\
     loads\n";

/// Stdout produced by `examples/threads.py`.
const THREADS_STDOUT: &str = "2000 2000\n\
     threads done\n\
     False\n\
     True\n\
     True\n\
     False\n\
     False\n\
     True\n";

const THREADING_STDOUT: &str = "300\nTrue\n";
const EVENT_QUEUE_STDOUT: &str = "0 1\nTrue 8 False\n";
const RLOCK_STDOUT: &str = "True\nTrue\nFalse\n";
const PICKLE_STDOUT: &str = "True\n{'a': [1, 2, 3], 'b': ('x', True, None)}\n";
const MULTIPROCESSING_STDOUT: &str = "hello [1, 2, 3]\n0\nfork\n";
const SPAWN_STDOUT: &str = "42\n0\nspawn\n";
const POOL_STDOUT: &str = "[1, 4, 9, 16]\n42\n";
const FILES_STDOUT: &str = "alpha\nbeta\nalpha\nbeta\nmissing\n";
const PATHLIB_STDOUT: &str = "True a.txt .txt\nhello\nb'\\x00A'\n2\n1\na.txt\nb.bin\n";

const CLOSURES_STDOUT: &str = "5 13\n6\n";

const GENERATORS_ASYNC_STDOUT: &str = "[0, 1, 2, 3]\n8\n[1, 2]\n";

const CLASSES_STDOUT: &str = "4 4\n7\n5 10\nmissing\n";

const SOCKET_ECHO_STDOUT: &str = "b'ping' b'pong'\n";

const STDLIB_MODULES_STDOUT: &str =
    "5 True file.txt\n1\ndefaultdict(<class 'list'>, {'a': [1]})\n['1', '22']\nbx\n";

const STDLIB_UTILS_STDOUT: &str = "True\n5\nTrue\n1\nx\n3 True\n2.0\n2.0\nTrue\nPyX\n";

const BINARY_FILES_STDOUT: &str = "5\n0 10\nb'\\x00A\\n'\nb'B\\n'\n";

const BYTES_STDOUT: &str = "12\n72 10\nb'Hello'\n[72, 101, 108]\nHello, PyX!\nb'h\\xc3\\xa9'\n[b'a', b'b', b'c']\nb'a-b'\nTrue 616263\n1\nb'\\x00\\xff'\nb'AB' b'\\x00\\x00' b'x'\n";

const CSV_JSON_STDOUT: &str = "['name', 'qty', 'note']\n\
     ['a', '1', 'x,y']\n\
     ['b', '2', 'say \"hi\"']\n\
     a 1\n\
     b 2\n\
     [{'id': '1', 'value': 'x'}]\n\
     {'a': [1, 2], 'b': True}\n";

/// Stdout produced by `examples/threads_gc.py`.
const THREADS_GC_STDOUT: &str = "2000 4 True True\n";

/// Runs an already built executable.
fn run_executable(path: &Path) -> (i32, String, String) {
    let output = Command::new(path)
        .output()
        .expect("failed to spawn built executable");
    output_parts(output)
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// An isolated directory for a single test, removed on drop.
struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(test_name: &str) -> Self {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("pyx-e2e-{pid}-{test_name}-{unique}"));
        // Remove stale state from an interrupted previous run.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("failed to create test temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Whether the AOT runtime archive is available next to the test profile.
fn runtime_lib_present() -> bool {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(_) => return false,
    };
    exe.ancestors()
        .any(|dir| dir.join("libpyx_runtime.a").is_file())
}

#[test]
fn check_hello() {
    let hello = example("hello.py");
    let (code, stdout, stderr) = run(&["check", &hello]);

    assert_eq!(code, 0, "pyx check failed: {stderr}");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
}

#[test]
fn run_hello() {
    let hello = example("hello.py");
    let (code, stdout, stderr) = run(&["run", &hello]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(stdout, "Hello, PyX!\n");
}

#[test]
fn run_fib() {
    let fib = example("fib.py");
    let (code, stdout, stderr) = run(&["run", &fib]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(stdout, FIB_STDOUT);
}

#[test]
fn run_control() {
    let control = example("control.py");
    let (code, stdout, stderr) = run(&["run", &control]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(stdout, "14\n3\nlarge\n3\n2\n1\n");
}

#[test]
fn run_strings() {
    let strings = example("strings.py");
    let (code, stdout, stderr) = run(&["run", &strings]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(stdout, "PyX\n3\nn=42\n8\n5.0\nTrue\n0.5\n9\n");
}

#[test]
fn run_slices() {
    let slices = example("slices.py");
    let (code, stdout, stderr) = run(&["run", &slices]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, SLICES_STDOUT);
}

#[test]
fn run_fstrings() {
    let fstrings = example("fstrings.py");
    let (code, stdout, stderr) = run(&["run", &fstrings]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, FSTRINGS_STDOUT);
}

#[test]
fn run_slices_gc() {
    let slices = example("slices.py");
    let (code, stdout, stderr) = run_with_env(&["run", &slices], &[("PYX_GC_THRESHOLD", "8")]);

    assert_eq!(code, 0, "pyx run failed with GC threshold: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, SLICES_STDOUT);
}

#[test]
fn check_dynamic_format_spec() {
    let program = example("errors/format_spec.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 1, "dynamic format spec must exit with status 1");
    assert!(
        stdout.is_empty(),
        "no code may run after a frontend diagnostic"
    );
    assert!(
        stderr.contains("dynamic") && stderr.contains("format spec"),
        "missing dynamic format-spec diagnostic, got:\n{stderr}"
    );
}

#[test]
fn run_formatting() {
    let program = example("formatting.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "3.14\n0042\n'x'\n0xff\na 2.2\nx:3\n1.23 00042 %\n");
}

#[test]
fn check_dict_slice() {
    let program = example("errors/dict_slice.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 1, "slicing a dict must exit with status 1");
    assert!(stdout.is_empty(), "no output must precede the TypeError");
    assert!(
        stderr.contains("unhashable") && stderr.contains("slice"),
        "missing unhashable slice message, got:\n{stderr}"
    );
}

#[test]
fn check_augmented_slice_assign() {
    let program = example("errors/slice_assign.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(
        code, 1,
        "augmented slice assignment must exit with status 1"
    );
    assert!(
        stdout.is_empty(),
        "no code may run after a frontend diagnostic"
    );
    assert!(
        stderr.contains("augmented slice assignment"),
        "missing augmented slice assignment diagnostic, got:\n{stderr}"
    );
}

#[test]
fn run_collections() {
    let collections = example("collections.py");
    let (code, stdout, stderr) = run(&["run", &collections]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(
        stderr.is_empty(),
        "unexpected stderr from collections.py:\n{stderr}"
    );
    assert_eq!(stdout, COLLECTIONS_STDOUT);
}

#[test]
fn run_gc_threshold() {
    let stress = example("gc_stress.py");
    let (code, stdout, stderr) = run_with_env(&["run", &stress], &[("PYX_GC_THRESHOLD", "16")]);

    assert_eq!(code, 0, "pyx run failed with GC threshold: {stderr}");
    assert_eq!(stdout, "1600\n");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
}

#[test]
fn run_gc_stress_mode() {
    let stress = example("gc_stress.py");
    let (code, stdout, stderr) = run_with_env(&["run", &stress], &[("PYX_GC_STRESS", "1")]);

    assert_eq!(code, 0, "pyx run failed in GC stress mode: {stderr}");
    assert_eq!(stdout, "1600\n");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
}

#[test]
fn run_collections_gc_stress() {
    let collections = example("collections.py");
    let (code, stdout, stderr) = run_with_env(&["run", &collections], &[("PYX_GC_THRESHOLD", "8")]);

    assert_eq!(code, 0, "pyx run failed with GC threshold: {stderr}");
    assert!(
        stderr.is_empty(),
        "unexpected stderr from collections.py under GC stress:\n{stderr}"
    );
    assert_eq!(stdout, COLLECTIONS_STDOUT);
}

#[test]
fn run_fib_gc() {
    let fib = example("fib.py");
    let (code, stdout, stderr) = run_with_env(&["run", &fib], &[("PYX_GC_THRESHOLD", "4")]);

    assert_eq!(code, 0, "pyx run failed with GC threshold: {stderr}");
    assert_eq!(stdout, FIB_STDOUT);
}

#[test]
fn run_index_error() {
    let index_error = example("errors/index_error.py");
    let (code, stdout, stderr) = run(&["run", &index_error]);

    assert_eq!(code, 1, "uncaught IndexError must exit with status 1");
    assert!(stdout.is_empty(), "no output must precede the IndexError");
    assert!(
        stderr.contains("Traceback (most recent call last):"),
        "missing traceback header, got:\n{stderr}"
    );
    assert!(
        stderr.contains("IndexError: list index out of range"),
        "missing IndexError message, got:\n{stderr}"
    );
    assert!(
        stderr.contains("index_error.py") && stderr.contains("line 2"),
        "missing file/line info in traceback, got:\n{stderr}"
    );
}

#[test]
fn check_collections() {
    let collections = example("collections.py");
    let (code, stdout, stderr) = run(&["check", &collections]);

    assert_eq!(code, 0, "pyx check failed: {stderr}");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.is_empty(),
        "pyx check must not write to stderr:\n{stderr}"
    );
}

#[test]
fn run_exceptions_caught() {
    let exceptions = example("exceptions.py");
    let (code, stdout, stderr) = run(&["run", &exceptions]);

    assert_eq!(
        code, 1,
        "the final unhandled ValueError must exit with status 1"
    );
    assert_eq!(
        stdout,
        "5\ncaught: integer division or modulo by zero\n0\nraised ok\n"
    );

    assert!(
        stderr.contains("Traceback (most recent call last):"),
        "missing traceback header, got:\n{stderr}"
    );
    assert!(
        stderr.contains("ValueError: unhandled"),
        "missing uncaught ValueError message, got:\n{stderr}"
    );
    assert!(
        stderr.contains("examples/exceptions.py"),
        "missing source path in traceback, got:\n{stderr}"
    );
}

#[test]
fn run_reraise() {
    let reraise = example("reraise.py");
    let (code, stdout, stderr) = run(&["run", &reraise]);

    assert_eq!(code, 0, "caught re-raise must exit 0; stderr:\n{stderr}");
    assert_eq!(stdout, "caught original\nabcabc xyxy z\n");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
}

#[test]
fn run_finally() {
    let finally = example("finally.py");
    let (code, stdout, stderr) = run(&["run", &finally]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, FINALLY_STDOUT);
}

#[test]
fn run_finally_gc() {
    let finally = example("finally.py");
    let (code, stdout, stderr) = run_with_env(&["run", &finally], &[("PYX_GC_THRESHOLD", "8")]);

    assert_eq!(code, 0, "pyx run failed with GC threshold: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, FINALLY_STDOUT);
}

#[test]
fn run_finally_reraise() {
    let program = example("finally_reraise.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(
        stdout,
        "finally-reraise reraise-finally\nno-active No active exception to reraise\n"
    );
}

#[test]
fn run_return_after_try() {
    let program = example("return_after_try.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "5\n");
}

#[test]
fn run_key_error() {
    let program = example("key_error.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(stdout, "key 'k'\n");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
}

#[test]
fn run_uncaught() {
    let uncaught = example("errors/uncaught.py");
    let (code, stdout, stderr) = run(&["run", &uncaught]);

    assert_eq!(code, 1, "uncaught exception must exit with status 1");
    assert_eq!(stdout, "before\n");
    assert!(
        stderr.contains("Traceback"),
        "missing traceback, got:\n{stderr}"
    );
    assert!(
        stderr.contains("ZeroDivisionError: integer division or modulo by zero"),
        "missing ZeroDivisionError message, got:\n{stderr}"
    );
    assert!(
        stderr.contains("uncaught.py") && stderr.contains("line 2"),
        "missing file/line info in traceback, got:\n{stderr}"
    );
}

#[test]
fn check_type_error() {
    let type_error = example("errors/type_error.py");
    let (code, stdout, stderr) = run(&["check", &type_error]);

    assert_eq!(code, 1, "type error must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("type_error.py:"),
        "missing diagnostic location, got:\n{stderr}"
    );
    assert!(
        stderr.contains("int") && stderr.contains("str"),
        "diagnostic must mention the mismatched types, got:\n{stderr}"
    );
}

#[test]
fn check_syntax_error() {
    let syntax_error = example("errors/syntax_error.py");
    let (code, _stdout, stderr) = run(&["check", &syntax_error]);

    assert_eq!(code, 1, "syntax error must exit with status 1");
    assert!(
        stderr.contains("syntax_error.py:"),
        "missing diagnostic location, got:\n{stderr}"
    );
}

#[test]
fn run_type_error() {
    // The frontend must reject the program before anything is executed.
    let type_error = example("errors/type_error.py");
    let (code, stdout, stderr) = run(&["run", &type_error]);

    assert_eq!(code, 1, "type error must exit with status 1");
    assert!(
        stdout.is_empty(),
        "no code may run after a frontend diagnostic"
    );
    assert!(
        stderr.contains("type_error.py:") && stderr.contains("error"),
        "missing frontend diagnostic, got:\n{stderr}"
    );
}

#[test]
fn build_hello() {
    if !runtime_lib_present() {
        eprintln!(
            "skipping build_hello: libpyx_runtime.a not found next to the test profile \
             (AOT is covered by scripts/verify.sh, which builds it first)"
        );
        return;
    }

    let dir = TestDir::new("build_hello");
    let hello = example("hello.py");
    let binary = dir.path().join("hello");
    let binary_arg = binary.to_string_lossy().into_owned();

    let (code, stdout, stderr) = run(&["build", &hello, "-o", &binary_arg]);
    assert_eq!(
        code, 0,
        "pyx build failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    if binary.exists() {
        let (code, stdout, stderr) = run_executable(&binary);
        assert_eq!(code, 0, "AOT binary failed: {stderr}");
        assert_eq!(stdout, "Hello, PyX!\n");
    }
}

#[test]
fn build_missing_runtime() {
    let dir = TestDir::new("build_missing_runtime");
    let hello = example("hello.py");
    let binary = dir.path().join("hello");
    let binary_arg = binary.to_string_lossy().into_owned();

    let (code, _stdout, stderr) = run(&[
        "build",
        &hello,
        "-o",
        &binary_arg,
        "--runtime-lib",
        "/nonexistent",
    ]);

    assert_eq!(
        code, 2,
        "a missing runtime library must be a toolchain error"
    );
    assert!(
        stderr.contains("runtime")
            && (stderr.contains("not found")
                || stderr.contains("--runtime-lib")
                || stderr.contains("PYX_RUNTIME_LIB")),
        "missing actionable runtime hint, got:\n{stderr}"
    );
}

#[test]
fn run_methods() {
    let methods = example("methods.py");
    let (code, stdout, stderr) = run(&["run", &methods]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, METHODS_STDOUT);
}

#[test]
fn run_methods_gc() {
    let methods = example("methods.py");
    let (code, stdout, stderr) = run_with_env(&["run", &methods], &[("PYX_GC_THRESHOLD", "8")]);

    assert_eq!(code, 0, "pyx run failed with GC threshold: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, METHODS_STDOUT);
}

#[test]
fn check_unknown_method() {
    let program = example("errors/unknown_method.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "unknown method must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("has no method"),
        "missing unknown method diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_method_arity() {
    let program = example("errors/method_arity.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "method arity error must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("takes") && stderr.contains("argument"),
        "missing method arity diagnostic, got:\n{stderr}"
    );
}

#[test]
fn method_value_raises_attribute_error() {
    // A method read without a call lowers to a data-attribute access and is
    // rejected at runtime, mirroring CPython's AttributeError.
    let program = example("errors/attribute_value.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 1, "uncaught AttributeError must exit with status 1");
    assert!(stdout.is_empty(), "the failure happens before print");
    assert!(
        stderr.contains("AttributeError: 'str' object has no attribute 'upper'"),
        "missing AttributeError, got:\n{stderr}"
    );
}

#[test]
fn check_method_arg_type() {
    let program = example("errors/method_arg_type.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(
        code, 1,
        "method argument type error must exit with status 1"
    );
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("argument") && (stderr.contains("str") || stderr.contains("int")),
        "missing method argument type diagnostic, got:\n{stderr}"
    );
}

#[test]
fn run_tuples() {
    let tuples = example("tuples.py");
    let (code, stdout, stderr) = run(&["run", &tuples]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, TUPLES_STDOUT);
}

#[test]
fn run_tuples_gc() {
    let tuples = example("tuples.py");
    let (code, stdout, stderr) = run_with_env(&["run", &tuples], &[("PYX_GC_THRESHOLD", "8")]);

    assert_eq!(code, 0, "pyx run failed with GC threshold: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, TUPLES_STDOUT);
}

#[test]
fn check_tuple_star() {
    let program = example("errors/tuple_star.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "starred tuple unpacking must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("starred unpacking"),
        "missing starred unpacking diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_tuple_assign() {
    let program = example("errors/tuple_assign.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "tuple item assignment must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("tuple") && stderr.contains("item assignment"),
        "missing tuple item assignment diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_tuple_unhashable_key() {
    let program = example("errors/tuple_unhashable_key.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "unhashable tuple key must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("unhashable") && stderr.contains("list"),
        "missing unhashable key diagnostic, got:\n{stderr}"
    );
}

#[test]
fn run_sets() {
    let sets = example("sets.py");
    let (code, stdout, stderr) = run(&["run", &sets]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, SETS_STDOUT);
}

#[test]
fn run_sets_gc() {
    let sets = example("sets.py");
    let (code, stdout, stderr) = run_with_env(&["run", &sets], &[("PYX_GC_THRESHOLD", "8")]);

    assert_eq!(code, 0, "pyx run failed with GC threshold: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, SETS_STDOUT);
}

#[test]
fn run_builtins() {
    let builtins = example("builtins.py");
    let (code, stdout, stderr) = run(&["run", &builtins]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(
        stderr.is_empty(),
        "unexpected stderr from builtins.py:\n{stderr}"
    );
    assert_eq!(stdout, BUILTINS_STDOUT);
}

#[test]
fn run_builtins_gc() {
    let builtins = example("builtins.py");
    let (code, stdout, stderr) = run_with_env(&["run", &builtins], &[("PYX_GC_THRESHOLD", "8")]);

    assert_eq!(code, 0, "pyx run failed with GC threshold: {stderr}");
    assert!(
        stderr.is_empty(),
        "unexpected stderr from builtins.py under GC stress:\n{stderr}"
    );
    assert_eq!(stdout, BUILTINS_STDOUT);
}

#[test]
fn check_list_noniterable() {
    let program = example("errors/list_noniterable.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "list() of a non-iterable must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("iterable"),
        "missing non-iterable list diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_dict_noniterable() {
    let program = example("errors/dict_noniterable.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "dict() of a non-iterable must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("iterable"),
        "missing non-iterable dict diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_range_arg_type() {
    let program = example("errors/range_arg_type.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(
        code, 1,
        "range() with a str argument must exit with status 1"
    );
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("range") && stderr.contains("int"),
        "missing range argument type diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_sum_arg_type() {
    let program = example("errors/sum_arg_type.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "sum() with a str start must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("sum") && stderr.contains("argument"),
        "missing sum argument type diagnostic, got:\n{stderr}"
    );
}

#[test]
fn run_comprehensions() {
    let program = example("comprehensions.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(
        stderr.is_empty(),
        "unexpected stderr from comprehensions.py:\n{stderr}"
    );
    assert_eq!(stdout, COMPREHENSIONS_STDOUT);
}

#[test]
fn run_comprehensions_gc() {
    let program = example("comprehensions.py");
    let (code, stdout, stderr) = run_with_env(&["run", &program], &[("PYX_GC_THRESHOLD", "8")]);

    assert_eq!(code, 0, "pyx run failed with GC threshold: {stderr}");
    assert!(
        stderr.is_empty(),
        "unexpected stderr from comprehensions.py under GC stress:\n{stderr}"
    );
    assert_eq!(stdout, COMPREHENSIONS_STDOUT);
}

#[test]
fn check_comp_noniterable() {
    let program = example("errors/comp_noniterable.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(
        code, 1,
        "comprehension over a non-iterable must exit with status 1"
    );
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("not iterable"),
        "missing non-iterable comprehension diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_comp_unhashable() {
    let program = example("errors/comp_unhashable.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(
        code, 1,
        "unhashable set comprehension element must exit with status 1"
    );
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("unhashable") && stderr.contains("list"),
        "missing unhashable comprehension diagnostic, got:\n{stderr}"
    );
}

#[test]
fn run_range_step_zero() {
    let program = example("errors/range_step_zero.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 1, "zero range step must exit with status 1");
    assert!(stdout.is_empty(), "no output must precede the ValueError");
    assert!(
        stderr.contains("range() arg 3 must not be zero"),
        "missing zero range step error, got:\n{stderr}"
    );
}

#[test]
fn run_loop_else_semantics() {
    let dir = TestDir::new("loop-else");
    let program = dir.path().join("loop_else.py");
    std::fs::write(
        &program,
        concat!(
            // Normal exhaustion runs `else`.
            "for i in range(3):\n",
            "    print(i)\n",
            "else:\n",
            "    print('range done')\n",
            // `break` skips it.
            "for i in range(3):\n",
            "    if i == 1:\n",
            "        print('break', i)\n",
            "        break\n",
            "else:\n",
            "    print('range else')\n",
            // `continue` does not.
            "for i in range(3):\n",
            "    if i == 1:\n",
            "        continue\n",
            "    print('cont', i)\n",
            "else:\n",
            "    print('cont done')\n",
            // The runtime iterator form, including an empty iterable.
            "for x in [10, 20]:\n",
            "    print(x)\n",
            "else:\n",
            "    print('each done')\n",
            "for x in []:\n",
            "    print('never')\n",
            "else:\n",
            "    print('empty done')\n",
            // Nested loops: each `else` belongs to its own loop only.
            "for i in range(2):\n",
            "    for j in range(2):\n",
            "        if j == 1:\n",
            "            break\n",
            "    else:\n",
            "        print('inner else')\n",
            "    print('outer', i)\n",
            "    if i == 0:\n",
            "        continue\n",
            "else:\n",
            "    print('outer else')\n",
            // `break` in a loop `else` targets the enclosing loop, so the
            // outer `else` is skipped.
            "for i in range(3):\n",
            "    for j in range(2):\n",
            "        pass\n",
            "    else:\n",
            "        if i == 1:\n",
            "            break\n",
            "        print('inner else', i)\n",
            "else:\n",
            "    print('outer else skipped')\n",
            // `continue` in a loop `else` targets the enclosing loop too.
            "for i in range(3):\n",
            "    for j in range(2):\n",
            "        pass\n",
            "    else:\n",
            "        if i == 0:\n",
            "            continue\n",
            "        print('cont inner else', i)\n",
            // `while ... else`.
            "n = 0\n",
            "while n < 3:\n",
            "    n += 1\n",
            "    if n == 2:\n",
            "        continue\n",
            "else:\n",
            "    print('while done', n)\n",
            "while True:\n",
            "    print('once')\n",
            "    break\n",
            "else:\n",
            "    print('never')\n",
            "while False:\n",
            "    print('never')\n",
            "else:\n",
            "    print('false else')\n",
            // `break` through a `finally` inside the loop still skips `else`.
            "for i in range(3):\n",
            "    try:\n",
            "        if i == 1:\n",
            "            break\n",
            "    finally:\n",
            "        print('cleanup', i)\n",
            "else:\n",
            "    print('unreachable else')\n",
            "print('after break')\n",
            // An exception in the body skips `else` and still runs `finally`.
            "try:\n",
            "    for i in range(2):\n",
            "        try:\n",
            "            if i == 1:\n",
            "                raise ValueError('boom')\n",
            "        finally:\n",
            "            print('finally', i)\n",
            "    else:\n",
            "        print('for else after raise')\n",
            "except ValueError as exc:\n",
            "    print('caught', exc)\n",
        ),
    )
    .expect("failed to write temp program");
    let program = program.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(
        stdout,
        "0\n\
         1\n\
         2\n\
         range done\n\
         break 1\n\
         cont 0\n\
         cont 2\n\
         cont done\n\
         10\n\
         20\n\
         each done\n\
         empty done\n\
         outer 0\n\
         outer 1\n\
         outer else\n\
         inner else 0\n\
         cont inner else 1\n\
         cont inner else 2\n\
         while done 3\n\
         once\n\
         false else\n\
         cleanup 0\n\
         cleanup 1\n\
         after break\n\
         finally 0\n\
         finally 1\n\
         caught boom\n"
    );
}

#[test]
fn check_set_unhashable() {
    let program = example("errors/set_unhashable.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "unhashable set element must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("unhashable") && stderr.contains("list"),
        "missing unhashable set element diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_set_subscript() {
    let program = example("errors/set_subscript.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "set subscript must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("not subscriptable"),
        "missing set subscript diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_in_str_type() {
    let program = example("errors/in_str_type.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(
        code, 1,
        "int membership against str must exit with status 1"
    );
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("string") && stderr.contains("left operand"),
        "missing string membership diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_in_noniterable() {
    let program = example("errors/in_noniterable.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(
        code, 1,
        "membership against non-iterable must exit with status 1"
    );
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("not iterable"),
        "missing non-iterable membership diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_set_index_assign() {
    let program = example("errors/set_index_assign.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "set item assignment must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("does not support item assignment"),
        "missing set item assignment diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_renders_diagnostic() {
    let program = example("errors/type_error.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "type error must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("error[PYX0102]"),
        "missing diagnostic code, got:\n{stderr}"
    );
    assert!(
        stderr.contains("-->"),
        "missing location marker, got:\n{stderr}"
    );
    assert!(
        stderr.contains("1 |"),
        "missing source snippet line number, got:\n{stderr}"
    );
    assert!(
        stderr.contains('^'),
        "missing caret span marker, got:\n{stderr}"
    );
    assert!(
        stderr.contains("help:"),
        "missing help line, got:\n{stderr}"
    );
}

#[test]
fn check_json_diagnostics() {
    let program = example("errors/type_error.py");
    let (code, stdout, stderr) = run(&["check", "--json", &program]);

    assert_eq!(code, 1, "type error must exit with status 1");
    assert!(stdout.is_empty(), "diagnostics must go to stderr");
    assert!(
        stderr.trim_start().starts_with('['),
        "JSON diagnostics must be a top-level array, got:\n{stderr}"
    );
    assert!(
        stderr.contains("\"code\":\"PYX0102\""),
        "missing JSON diagnostic code, got:\n{stderr}"
    );
    assert!(
        stderr.contains("\"line\":1"),
        "missing JSON line number, got:\n{stderr}"
    );
    assert!(
        stderr.contains("\"file\":"),
        "missing JSON file field, got:\n{stderr}"
    );
    assert!(
        stderr.contains("\"end_col\":"),
        "missing JSON end_col field, got:\n{stderr}"
    );
    assert!(
        stderr.contains("\"help\":"),
        "missing JSON help field, got:\n{stderr}"
    );
}

#[test]
fn check_spans_multichar_expression() {
    let dir = TestDir::new("span-expression");
    let program = dir.path().join("span.py");
    std::fs::write(&program, "value = 5\nprint(value[0])\n").expect("failed to write program");
    let program = program.to_string_lossy().into_owned();

    let (code, stdout, stderr) = run(&["check", &program]);
    assert_eq!(code, 1, "subscript error must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("'int' object is not subscriptable"),
        "missing subscript diagnostic, got:\n{stderr}"
    );
    assert!(
        stderr.contains("^^^^^"),
        "expected the offending identifier span, got:\n{stderr}"
    );

    let (code, _, stderr) = run(&["check", "--json", &program]);
    assert_eq!(code, 1, "JSON diagnostics must exit with status 1");
    assert!(
        stderr.contains("\"col\":7") && stderr.contains("\"end_col\":12"),
        "expected a five-byte expression span at line 2, got:\n{stderr}"
    );
}

#[test]
fn check_json_syntax_code() {
    let program = example("errors/syntax_error.py");
    let (code, stdout, stderr) = run(&["check", "--json", &program]);

    assert_eq!(code, 1, "syntax error must exit with status 1");
    assert!(stdout.is_empty(), "diagnostics must go to stderr");
    assert!(
        stderr.contains("\"code\":\"PYX0001\""),
        "missing syntax error JSON code, got:\n{stderr}"
    );
}

#[test]
fn check_color_always() {
    let program = example("errors/type_error.py");
    let (code, stdout, stderr) = run(&["check", "--color=always", &program]);

    assert_eq!(code, 1, "type error must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("\u{1b}["),
        "expected ANSI escape sequences with --color=always, got:\n{stderr}"
    );
    assert!(
        stderr.contains("error["),
        "missing diagnostic header, got:\n{stderr}"
    );
}

#[test]
fn check_color_never() {
    let program = example("errors/type_error.py");
    let (code, stdout, stderr) = run(&["check", "--color=never", &program]);

    assert_eq!(code, 1, "type error must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        !stderr.contains("\u{1b}["),
        "unexpected ANSI escape sequences with --color=never, got:\n{stderr}"
    );
}

#[test]
fn check_success_quiet() {
    let hello = example("hello.py");
    let (code, stdout, stderr) = run(&["check", &hello]);

    assert_eq!(code, 0, "pyx check failed: {stderr}");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.is_empty(),
        "successful check must be quiet, got:\n{stderr}"
    );
}

#[test]
fn doctor_human() {
    let (code, stdout, stderr) = run(&["doctor"]);

    assert_eq!(code, 0, "pyx doctor failed: {stderr}");
    assert!(
        stdout.contains("pyx "),
        "missing tool/version line, got:\n{stdout}"
    );
    assert!(
        stdout.contains("runtime library:"),
        "missing runtime library report, got:\n{stdout}"
    );
    assert!(
        stdout.contains("C compiler:"),
        "missing C compiler report, got:\n{stdout}"
    );
}

#[test]
fn doctor_json() {
    let (code, stdout, stderr) = run(&["doctor", "--json"]);

    assert_eq!(code, 0, "pyx doctor --json failed: {stderr}");
    assert!(
        stdout.trim_start().starts_with('{'),
        "JSON doctor output must be an object, got:\n{stdout}"
    );
    assert!(
        stdout.contains("\"version\":"),
        "missing JSON version field, got:\n{stdout}"
    );
    assert!(
        stdout.contains("\"target\":"),
        "missing JSON target field, got:\n{stdout}"
    );
    assert!(
        stdout.contains("\"cc_found\":"),
        "missing JSON cc_found field, got:\n{stdout}"
    );
}

#[test]
fn unknown_name_help() {
    let dir = TestDir::new("unknown-name");
    let program = dir.path().join("unknown_name.py");
    std::fs::write(&program, "print(missing_name)\n").expect("failed to write temp program");
    let program = program.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "unknown name must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("PYX0101"),
        "missing unknown name diagnostic code, got:\n{stderr}"
    );
    // A close match may or may not be proposed for `missing_name`; at minimum
    // the stable unknown-name code must be reported.
}

#[test]
fn run_augmented_subscript_assignment() {
    let dir = TestDir::new("aug-subscript");
    let program = dir.path().join("aug_subscript.py");
    std::fs::write(
        &program,
        concat!(
            "d = {}\n",
            "d['x'] = 1\n",
            "d['x'] += 2\n",
            "print(d['x'])\n",
            "xs = [1]\n",
            "xs[0] *= 3\n",
            "print(xs[0])\n",
            "from collections import defaultdict\n",
            "counts = defaultdict(int)\n",
            "counts['y'] += 1\n",
            "counts['y'] += 1\n",
            "print(counts['y'])\n",
            "def f():\n",
            "    print('f')\n",
            "    return [10]\n",
            "def g():\n",
            "    print('g')\n",
            "    return 0\n",
            "f()[g()] += 5\n",
        ),
    )
    .expect("failed to write temp program");
    let program = program.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(stdout, "3\n3\n2\nf\ng\n");
}

#[test]
fn run_augmented_subscript_assignment_gc() {
    let dir = TestDir::new("aug-subscript-gc");
    let program = dir.path().join("aug_subscript_gc.py");
    std::fs::write(
        &program,
        concat!(
            "from collections import defaultdict\n",
            "d = defaultdict(int)\n",
            "i = 0\n",
            "while i < 200:\n",
            "    d[i] += 1\n",
            "    i += 1\n",
            "print(d[0], d[199])\n",
        ),
    )
    .expect("failed to write temp program");
    let program = program.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run_with_env(&["run", &program], &[("PYX_GC_THRESHOLD", "8")]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(stdout, "1 1\n");
}

#[test]
fn run_chained_assignment_gcd_divmod_callable_and_finditer() {
    let dir = TestDir::new("chained-assign");
    let program = dir.path().join("chained_assign.py");
    std::fs::write(
        &program,
        concat!(
            "def f():\n",
            "    a = b = 1\n",
            "    d = {}\n",
            "    xs = [0]\n",
            "    d['k'] = xs[0] = 7\n",
            "    return a, b, d['k'], xs[0]\n",
            "print(f())\n",
            "from math import gcd\n",
            "print(gcd(12, 8), gcd(12, 8, 6))\n",
            "print(divmod(-7, 3))\n",
            "print(callable(print), callable(1))\n",
            "import re\n",
            "for m in re.finditer('[0-9]+', 'a1b22'):\n",
            "    print(m.group(), m.start(), m.end())\n",
            "for m in re.finditer('A', 'aA', flags=re.IGNORECASE):\n",
            "    print(m.group(), m.start())\n",
        ),
    )
    .expect("failed to write temp program");
    let program = program.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(
        stdout,
        "(1, 1, 7, 7)\n4 2\n(-3, 2)\nTrue False\n1 1 2\n22 3 5\na 0\nA 1\n"
    );
}

#[test]
fn run_assert_statements() {
    let dir = TestDir::new("assert-statements");
    let program = dir.path().join("asserts.py");
    std::fs::write(
        &program,
        concat!(
            "def check(n):\n",
            "    assert n > 0, 'n must be positive'\n",
            "    return n\n",
            "print(check(2))\n",
            "try:\n",
            "    check(0)\n",
            "except AssertionError as exc:\n",
            "    print('assertion:', exc)\n",
            // `AssertionError` is an `Exception`: the generic handler catches
            // a bare failing assert too.
            "try:\n",
            "    assert 1 == 2\n",
            "except Exception:\n",
            "    print('exception caught')\n",
            // The message expression only runs when the test fails.
            "seen = []\n",
            "def message():\n",
            "    seen.append(1)\n",
            "    return 'lazy'\n",
            "assert True, message()\n",
            "print('lazy evaluations:', len(seen))\n",
            "try:\n",
            "    assert False, message()\n",
            "except AssertionError as exc:\n",
            "    print('message:', exc)\n",
            "print('lazy evaluations:', len(seen))\n",
        ),
    )
    .expect("failed to write temp program");
    let program = program.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(
        stdout,
        "2\n\
         assertion: n must be positive\n\
         exception caught\n\
         lazy evaluations: 0\n\
         message: lazy\n\
         lazy evaluations: 1\n"
    );
}

#[test]
fn run_re_flags_and_verbose_pattern() {
    let dir = TestDir::new("re-flags");
    let program = dir.path().join("re_flags.py");
    std::fs::write(
        &program,
        concat!(
            "import re\n",
            "RE_COLOR = re.compile(\n",
            "    r\"\"\"^\n",
            "\\#([0-9a-f]{6})$|\n",
            "color\\(([0-9]{1,3})\\)$|\n",
            "rgb\\(([\\d\\s,]+)\\)$\n",
            "\"\"\",\n",
            "    re.VERBOSE,\n",
            ")\n",
            "m = RE_COLOR.search('rgb(1, 22, 200)')\n",
            "print(m is not None, m.group(3))\n",
            "print(RE_COLOR.search('nope') is None)\n",
            "CASE = re.compile('abc', re.I)\n",
            "print(CASE.search('xABCx') is not None)\n",
        ),
    )
    .expect("failed to write temp program");
    let program = program.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(stdout, "True 1, 22, 200\nTrue\nTrue\n");
}

#[test]
fn run_functions() {
    let program = example("functions.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(
        stderr.is_empty(),
        "unexpected stderr from functions.py:\n{stderr}"
    );
    assert_eq!(stdout, FUNCTIONS_STDOUT);
}

#[test]
fn run_functions_gc() {
    let program = example("functions.py");
    let (code, stdout, stderr) = run_with_env(&["run", &program], &[("PYX_GC_THRESHOLD", "8")]);

    assert_eq!(code, 0, "pyx run failed with GC threshold: {stderr}");
    assert!(
        stderr.is_empty(),
        "unexpected stderr from functions.py under GC stress:\n{stderr}"
    );
    assert_eq!(stdout, FUNCTIONS_STDOUT);
}

#[test]
fn check_lambda_closure() {
    let program = example("errors/lambda_closure.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "capturing lambda must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("closures") || stderr.contains("not supported"),
        "missing closure diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_map_noncallable() {
    let program = example("errors/map_noncallable.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "map() of a non-callable must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("map") && stderr.contains("callable"),
        "missing non-callable map diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_filter_noncallable() {
    let program = example("errors/filter_noncallable.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(
        code, 1,
        "filter() of a non-callable must exit with status 1"
    );
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("filter") && stderr.contains("callable"),
        "missing non-callable filter diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_indirect_noncallable() {
    let program = example("errors/indirect_noncallable.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(
        code, 1,
        "calling a non-callable value must exit with status 1"
    );
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("not callable"),
        "missing not-callable diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_lambda_default() {
    let program = example("errors/lambda_default.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "lambda default value must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("default"),
        "missing default parameter diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_lambda_star() {
    let program = example("errors/lambda_star.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "lambda *args parameter must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("vararg") || stderr.contains("star"),
        "missing vararg diagnostic, got:\n{stderr}"
    );
}

#[test]
fn run_modules() {
    let program = example("modules.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(
        stderr.is_empty(),
        "unexpected stderr from modules.py:\n{stderr}"
    );
    assert_eq!(stdout, MODULES_STDOUT);
}

#[test]
fn run_modules_gc() {
    let program = example("modules.py");
    let (code, stdout, stderr) = run_with_env(&["run", &program], &[("PYX_GC_THRESHOLD", "8")]);

    assert_eq!(code, 0, "pyx run failed with GC threshold: {stderr}");
    assert!(
        stderr.is_empty(),
        "unexpected stderr from modules.py under GC stress:\n{stderr}"
    );
    assert_eq!(stdout, MODULES_STDOUT);
}

#[test]
fn check_import_unsupported() {
    let program = example("errors/import_unsupported.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "unsupported import must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("not found") || stderr.contains("not supported"),
        "missing unsupported import diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_module_member_unknown() {
    let program = example("errors/module_member_unknown.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "unknown module member must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("no attribute"),
        "missing unknown module member diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_import_in_function_is_deferred() {
    // Nested imports are no longer rejected: native modules bind normally and
    // user modules bind to a runtime error stub, so the program compiles.
    let program = example("errors/import_in_function.py");
    let (code, _stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 0, "function-local import must check:\n{stderr}");
}

#[test]
fn check_from_member_unknown() {
    let program = example("errors/from_member_unknown.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(
        code, 1,
        "unknown from-import member must exit with status 1"
    );
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("no attribute"),
        "missing unknown from-import member diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_module_value() {
    // Standard-library modules are first-class values (used by the official
    // `test_errno.py` through `getattr(errno, name)`).
    let program = example("errors/module_value.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 0, "stdlib module values must be accepted:\n{stderr}");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.is_empty(),
        "unexpected module value diagnostic:\n{stderr}"
    );
}

#[test]
fn check_module_call_noncallable() {
    let program = example("errors/module_call_noncallable.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(
        code, 1,
        "calling a non-callable module attribute must exit with status 1"
    );
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("not callable"),
        "missing not-callable diagnostic, got:\n{stderr}"
    );
}

#[test]
fn run_json_os() {
    let program = example("json_os.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(
        stderr.is_empty(),
        "unexpected stderr from json_os.py:\n{stderr}"
    );
    assert_eq!(stdout, JSON_OS_STDOUT);
}

#[test]
fn run_json_os_gc() {
    let program = example("json_os.py");
    let (code, stdout, stderr) = run_with_env(&["run", &program], &[("PYX_GC_THRESHOLD", "8")]);

    assert_eq!(code, 0, "pyx run failed with GC threshold: {stderr}");
    assert!(
        stderr.is_empty(),
        "unexpected stderr from json_os.py under GC stress:\n{stderr}"
    );
    assert_eq!(stdout, JSON_OS_STDOUT);
}

#[test]
fn run_threads() {
    let program = example("threads.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(
        stderr.is_empty(),
        "unexpected stderr from threads.py:\n{stderr}"
    );
    assert_eq!(stdout, THREADS_STDOUT);
}

#[test]
fn run_threads_gc() {
    let program = example("threads.py");
    let (code, stdout, stderr) = run_with_env(&["run", &program], &[("PYX_GC_THRESHOLD", "8")]);

    assert_eq!(code, 0, "pyx run failed with GC threshold: {stderr}");
    assert!(
        stderr.is_empty(),
        "unexpected stderr from threads.py under GC stress:\n{stderr}"
    );
    assert_eq!(stdout, THREADS_STDOUT);
}

#[test]
fn run_threads_gc_stress() {
    let program = example("threads_gc.py");
    let (code, stdout, stderr) = run_with_env(&["run", &program], &[("PYX_GC_THRESHOLD", "1")]);

    assert_eq!(code, 0, "pyx run failed with GC threshold 1: {stderr}");
    assert!(
        stderr.is_empty(),
        "unexpected stderr from threads_gc.py under GC stress:\n{stderr}"
    );
    assert_eq!(stdout, THREADS_GC_STDOUT);
}

#[test]
fn run_threads_gc_stress_default() {
    let program = example("threads_gc.py");
    let (code, stdout, stderr) = run_with_env(&["run", &program], &[]);

    assert_eq!(
        code, 0,
        "pyx run failed with default GC threshold: {stderr}"
    );
    assert!(
        stderr.is_empty(),
        "unexpected stderr from threads_gc.py with default GC threshold:\n{stderr}"
    );
    assert_eq!(stdout, THREADS_GC_STDOUT);
}

#[test]
fn run_threading() {
    let program = example("threading.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, THREADING_STDOUT);
}

#[test]
fn run_event_queue() {
    let program = example("event_queue.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, EVENT_QUEUE_STDOUT);
}

#[test]
fn run_rlock() {
    let program = example("rlock.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, RLOCK_STDOUT);
}

#[test]
fn run_pickle_roundtrip() {
    let program = example("pickle.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, PICKLE_STDOUT);
}

#[test]
fn run_pathlib() {
    let program = example("pathlib_example.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, PATHLIB_STDOUT);
}

#[test]
fn run_closures() {
    let program = example("closures.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, CLOSURES_STDOUT);
}

#[test]
fn run_generators_async() {
    let program = example("generators_async.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, GENERATORS_ASYNC_STDOUT);
}

#[test]
fn run_classes() {
    let program = example("classes.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, CLASSES_STDOUT);
}

#[test]
fn run_socket_echo() {
    let program = example("socket_echo.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, SOCKET_ECHO_STDOUT);
}

#[test]
fn run_stdlib_modules() {
    let program = example("stdlib_modules.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, STDLIB_MODULES_STDOUT);
}

#[test]
fn run_stdlib_utils() {
    let program = example("stdlib_utils.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, STDLIB_UTILS_STDOUT);
}

#[test]
fn run_binary_files() {
    let program = example("binary_files.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, BINARY_FILES_STDOUT);
}

#[test]
fn run_bytes() {
    let program = example("bytes.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, BYTES_STDOUT);
}

#[test]
fn run_csv_json() {
    let program = example("csv_json.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, CSV_JSON_STDOUT);
}

#[test]
fn run_files() {
    let program = example("files.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, FILES_STDOUT);
}

#[test]
fn run_spawn() {
    let program = example("spawn.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, SPAWN_STDOUT);
}

#[test]
fn run_pool() {
    let program = example("pool.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, POOL_STDOUT);
}

#[test]
fn run_pool_spawn() {
    let program = example("pool_spawn.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, POOL_STDOUT);
}

#[test]
fn run_spawn_gc_stress() {
    let program = example("spawn.py");
    let (code, stdout, stderr) = run_with_env(&["run", &program], &[("PYX_GC_THRESHOLD", "1")]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, SPAWN_STDOUT);
}

#[test]
fn run_multiprocessing() {
    let program = example("multiprocessing.py");
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, MULTIPROCESSING_STDOUT);
}

#[test]
fn run_threading_gc_stress() {
    let program = example("threading.py");
    let (code, stdout, stderr) = run_with_env(&["run", &program], &[("PYX_GC_THRESHOLD", "1")]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, THREADING_STDOUT);
}

#[test]
fn check_json_member_unknown() {
    let program = example("errors/json_member_unknown.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "unknown json member must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("no attribute"),
        "missing unknown json member diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_os_member_unknown() {
    let program = example("errors/os_member_unknown.py");
    let (code, stdout, stderr) = run(&["check", &program]);

    assert_eq!(code, 1, "unknown os member must exit with status 1");
    assert!(stdout.is_empty(), "pyx check must not write to stdout");
    assert!(
        stderr.contains("no attribute"),
        "missing unknown os member diagnostic, got:\n{stderr}"
    );
}

// --- Project scaffolding and tooling commands -----------------------------

/// Runs the `pyx` binary with `dir` as the working directory.
fn run_in(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_pyx"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn pyx binary");
    output_parts(output)
}

fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read '{}': {error}", path.display()))
}

#[test]
fn global_verbose_reports_run_timing() {
    let hello = example("hello.py");
    let (code, stdout, stderr) = run(&["--verbose", "run", &hello]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(stdout, "Hello, PyX!\n");
    assert!(
        stderr.contains("pyx: run: frontend"),
        "missing frontend timing, got:\n{stderr}"
    );
    assert!(
        stderr.contains("pyx: run: jit"),
        "missing JIT timing, got:\n{stderr}"
    );
    assert!(
        stderr.contains(" ms"),
        "missing timing unit, got:\n{stderr}"
    );
}

#[test]
fn global_quiet_suppresses_scaffold_status() {
    let dir = TestDir::new("quiet-scaffold");
    let project = dir.path().join("project");
    let project_arg = project.to_string_lossy().into_owned();

    let (code, stdout, stderr) = run(&["--quiet", "new", &project_arg]);

    assert_eq!(code, 0, "pyx new failed: {stderr}");
    assert!(
        stdout.is_empty(),
        "quiet new must not print status:\n{stdout}"
    );
    assert!(project.join("main.py").is_file());
}

#[test]
fn global_quiet_suppresses_clean_status() {
    let dir = TestDir::new("quiet-clean");
    std::fs::create_dir_all(dir.path().join(".pyx/cache")).expect("failed to create cache");

    let (code, stdout, stderr) = run_in(dir.path(), &["--quiet", "clean"]);

    assert_eq!(code, 0, "pyx clean failed: {stderr}");
    assert!(
        stdout.is_empty(),
        "quiet clean must not print status:\n{stdout}"
    );
    assert!(!dir.path().join(".pyx/cache").exists());
}

#[test]
fn verbose_and_quiet_conflict() {
    let hello = example("hello.py");
    for args in [
        vec!["--verbose", "--quiet", "run", hello.as_str()],
        vec!["run", "--verbose", "--quiet", hello.as_str()],
    ] {
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 2, "conflicting flags must be a usage error");
        assert!(stdout.is_empty(), "usage errors go to stderr");
        assert!(
            stderr.contains("'--verbose' and '--quiet' cannot be used together"),
            "missing conflict message, got:\n{stderr}"
        );
    }
}

#[test]
fn help_lists_project_commands() {
    let (code, stdout, stderr) = run(&["help"]);

    assert_eq!(code, 0, "pyx help failed: {stderr}");
    for needle in [
        "pyx new",
        "pyx init",
        "pyx test",
        "pyx fmt",
        "pyx lint",
        "pyx clean",
        "--check",
        "--no-cache",
    ] {
        assert!(
            stdout.contains(needle),
            "help output is missing '{needle}':\n{stdout}"
        );
    }
}

#[test]
fn new_creates_scaffold() {
    let dir = TestDir::new("new-scaffold");
    let project = dir.path().join("demo-project");
    let project_arg = project.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["new", &project_arg]);

    assert_eq!(code, 0, "pyx new failed: {stderr}");
    assert!(
        stdout.contains("created"),
        "pyx new must report created files:\n{stdout}"
    );
    assert_eq!(
        read_text(&project.join("main.py")),
        "print(\"Hello, PyX!\")\n"
    );

    let pyproject = read_text(&project.join("pyproject.toml"));
    assert!(pyproject.contains("name = \"demo-project\""), "{pyproject}");
    assert!(pyproject.contains("version = \"0.1.0\""), "{pyproject}");
    assert!(
        pyproject.contains("requires-python = \">=3.10\""),
        "{pyproject}"
    );

    let gitignore = read_text(&project.join(".gitignore"));
    assert!(gitignore.contains(".pyx/"), "{gitignore}");
    assert!(gitignore.contains("__pycache__/"), "{gitignore}");

    // The generated program must run out of the box.
    let main = project.join("main.py").to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", &main]);
    assert_eq!(code, 0, "generated main.py failed: {stderr}");
    assert_eq!(stdout, "Hello, PyX!\n");
}

#[test]
fn new_existing_directory_requires_force() {
    let dir = TestDir::new("new-existing");
    let project = dir.path().join("taken");
    std::fs::create_dir_all(&project).expect("failed to create project dir");
    std::fs::write(project.join("notes.txt"), "keep\n").expect("failed to write notes");
    let project_arg = project.to_string_lossy().into_owned();

    let (code, _stdout, stderr) = run(&["new", &project_arg]);
    assert_eq!(code, 1, "existing directory must exit with status 1");
    assert!(
        stderr.contains("already exists") && stderr.contains("--force"),
        "missing actionable existing-directory error, got:\n{stderr}"
    );
    assert!(
        !project.join("main.py").exists(),
        "a refused pyx new must not write files"
    );

    let (code, _stdout, stderr) = run(&["new", &project_arg, "--force"]);
    assert_eq!(code, 0, "pyx new --force failed: {stderr}");
    assert!(project.join("main.py").is_file());
    assert_eq!(read_text(&project.join("notes.txt")), "keep\n");
}

#[test]
fn init_scaffolds_current_directory() {
    let dir = TestDir::new("init-current");
    let (code, _stdout, stderr) = run_in(dir.path(), &["init"]);

    assert_eq!(code, 0, "pyx init failed: {stderr}");
    assert!(dir.path().join("main.py").is_file());
    assert!(dir.path().join("pyproject.toml").is_file());
    assert!(dir.path().join(".gitignore").is_file());

    // Existing non-empty files are never overwritten without --force.
    let custom = "[project]\nname = \"custom\"\n";
    std::fs::write(dir.path().join("pyproject.toml"), custom).expect("failed to write pyproject");
    let (code, _stdout, stderr) = run_in(dir.path(), &["init"]);
    assert_eq!(code, 1, "a conflicting init must exit with status 1");
    assert!(
        stderr.contains("--force"),
        "missing --force hint, got:\n{stderr}"
    );
    assert_eq!(read_text(&dir.path().join("pyproject.toml")), custom);

    let (code, _stdout, stderr) = run_in(dir.path(), &["init", "--force"]);
    assert_eq!(code, 0, "pyx init --force failed: {stderr}");
    assert!(read_text(&dir.path().join("pyproject.toml")).contains("version = \"0.1.0\""));
}

#[test]
fn test_command_reports_pass_and_fail() {
    let dir = TestDir::new("test-command");
    std::fs::write(dir.path().join("test_ok.py"), "print(\"ok\")\n")
        .expect("failed to write test_ok.py");
    let nested = dir.path().join("nested");
    std::fs::create_dir_all(&nested).expect("failed to create nested dir");
    std::fs::write(nested.join("case_test.py"), "print(\"nested\")\n")
        .expect("failed to write case_test.py");
    let failing = dir.path().join("test_bad.py");
    std::fs::write(&failing, "print(undefined_value)\n").expect("failed to write test_bad.py");

    let (code, stdout, stderr) = run_in(dir.path(), &["test"]);
    assert_eq!(code, 1, "a failing test must exit with status 1");
    assert!(
        stdout.contains("PASS") && stdout.contains("test_ok.py"),
        "missing passing test report:\n{stdout}"
    );
    assert!(
        stdout.contains("FAIL") && stdout.contains("test_bad.py"),
        "missing failing test report:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("2 passed, 1 failed, 3 total"),
        "missing summary, got:\n{stdout}"
    );
    assert!(
        stdout.contains("nested") && stdout.contains("ok"),
        "test program output must be forwarded, got:\n{stdout}"
    );
    assert!(
        stderr.contains("PYX0101"),
        "missing frontend diagnostic from the failing test, got:\n{stderr}"
    );

    std::fs::remove_file(&failing).expect("failed to remove failing test");
    let (code, stdout, stderr) = run_in(dir.path(), &["test"]);
    assert_eq!(code, 0, "all tests must pass: {stderr}");
    assert!(stdout.contains("2 passed, 0 failed, 2 total"), "{stdout}");

    let (code, stdout, stderr) = run_in(dir.path(), &["test", "--json"]);
    assert_eq!(code, 0, "pyx test --json failed: {stderr}");
    let json = stdout.trim();
    assert!(
        json.starts_with('[') && json.ends_with(']'),
        "test --json must print one array, got:\n{stdout}"
    );
    assert!(
        json.contains("\"status\":\"pass\""),
        "missing JSON pass status, got:\n{stdout}"
    );
}

#[test]
fn test_command_without_tests() {
    let dir = TestDir::new("test-empty");
    let (code, stdout, stderr) = run_in(dir.path(), &["test"]);

    assert_eq!(code, 0, "no tests must not be a failure: {stderr}");
    assert!(
        stdout.contains("no test files found"),
        "missing empty-run notice, got:\n{stdout}"
    );

    let (code, stdout, stderr) = run_in(dir.path(), &["test", "--json"]);
    assert_eq!(code, 0, "pyx test --json failed: {stderr}");
    assert_eq!(stdout.trim(), "[]");
}

#[test]
fn fmt_check_and_write() {
    let dir = TestDir::new("fmt");
    let file = dir.path().join("sample.py");
    let original = "print(\"hi\")   \n\n\n";
    std::fs::write(&file, original).expect("failed to write sample.py");
    let file_arg = file.to_string_lossy().into_owned();

    let (code, stdout, _stderr) = run(&["fmt", "--check", &file_arg]);
    assert_eq!(code, 1, "--check must exit with status 1 on changes");
    assert!(
        stdout.contains("sample.py"),
        "missing would-reformat report:\n{stdout}"
    );
    assert_eq!(read_text(&file), original, "--check must not write");

    let (code, stdout, stderr) = run(&["fmt", &file_arg]);
    assert_eq!(code, 0, "pyx fmt failed: {stderr}");
    assert!(stdout.contains("formatted"), "missing report:\n{stdout}");
    assert_eq!(read_text(&file), "print(\"hi\")\n");

    let (code, _stdout, stderr) = run(&["fmt", "--check", &file_arg]);
    assert_eq!(code, 0, "formatted file must pass --check: {stderr}");
}

#[test]
fn lint_checks_all_files() {
    let dir = TestDir::new("lint");
    std::fs::write(dir.path().join("good.py"), "print(\"ok\")\n").expect("failed to write good.py");
    let bad = dir.path().join("bad.py");
    std::fs::write(&bad, "print(missing_name)\n").expect("failed to write bad.py");

    let (code, stdout, stderr) = run_in(dir.path(), &["lint"]);
    assert_eq!(code, 1, "lint must exit with status 1 on diagnostics");
    assert!(
        stderr.contains("PYX0101") && stderr.contains("bad.py"),
        "missing lint diagnostic, got:\n{stderr}"
    );
    assert!(
        stdout.is_empty(),
        "lint must not write to stdout on failure"
    );

    std::fs::remove_file(&bad).expect("failed to remove bad.py");
    let (code, stdout, stderr) = run_in(dir.path(), &["lint"]);
    assert_eq!(code, 0, "clean lint failed: {stderr}");
    assert!(
        stdout.contains("no errors"),
        "missing clean summary, got:\n{stdout}"
    );

    std::fs::write(&bad, "print(missing_name)\n").expect("failed to rewrite bad.py");
    let (code, stdout, stderr) = run_in(dir.path(), &["lint", "--json"]);
    assert_eq!(code, 1, "lint --json must exit with status 1");
    assert!(stdout.is_empty(), "JSON diagnostics belong on stderr");
    let json = stderr.trim();
    assert!(
        json.starts_with('[') && json.ends_with(']'),
        "lint --json must print one array, got:\n{stderr}"
    );
    assert!(
        json.contains("\"code\":\"PYX0101\""),
        "missing diagnostic in JSON array, got:\n{stderr}"
    );
}

#[test]
fn clean_removes_generated_dirs() {
    let dir = TestDir::new("clean");
    std::fs::create_dir_all(dir.path().join(".pyx/cache")).expect("failed to create cache");
    std::fs::write(dir.path().join(".pyx/cache/entry.o"), b"x").expect("failed to write cache");
    std::fs::create_dir_all(dir.path().join("target/pyx-tmp")).expect("failed to create pyx-tmp");
    std::fs::write(dir.path().join("target/pyx-tmp/tmp.o"), b"x").expect("failed to write tmp");
    std::fs::create_dir_all(dir.path().join("target/debug")).expect("failed to create debug");
    std::fs::write(dir.path().join("target/debug/keep"), b"x").expect("failed to write keep");

    let (code, stdout, stderr) = run_in(dir.path(), &["clean", "--dry-run"]);
    assert_eq!(code, 0, "clean --dry-run failed: {stderr}");
    assert!(
        stdout.contains("would remove .pyx/cache")
            && stdout.contains("would remove target/pyx-tmp"),
        "missing dry-run report:\n{stdout}"
    );
    assert!(dir.path().join(".pyx/cache").exists());

    let (code, stdout, stderr) = run_in(dir.path(), &["clean"]);
    assert_eq!(code, 0, "pyx clean failed: {stderr}");
    assert!(stdout.contains("removed .pyx/cache"), "{stdout}");
    assert!(!dir.path().join(".pyx/cache").exists());
    assert!(!dir.path().join("target/pyx-tmp").exists());
    assert!(
        dir.path().join("target/debug/keep").exists(),
        "clean must not remove the rest of target/"
    );

    let (code, stdout, stderr) = run_in(dir.path(), &["clean"]);
    assert_eq!(code, 0, "clean must be idempotent: {stderr}");
    assert!(stdout.contains("nothing to remove"), "{stdout}");
}

#[test]
fn build_verbose_reports_timing_and_cache_decisions() {
    if !runtime_lib_present() {
        eprintln!(
            "skipping build_verbose_reports_timing_and_cache_decisions: \
             libpyx_runtime.a not found next to the test profile"
        );
        return;
    }

    let dir = TestDir::new("build-verbose");
    std::fs::write(dir.path().join("hello.py"), "print(\"Hello, PyX!\")\n")
        .expect("failed to write hello.py");

    let (code, stdout, stderr) = run_in(dir.path(), &["--verbose", "build", "hello.py"]);
    assert_eq!(code, 0, "verbose pyx build failed:\n{stdout}\n{stderr}");
    for needle in [
        "pyx: build: frontend",
        "pyx: build: cache key",
        "pyx: build: cache miss",
        "pyx: build: codegen",
        "pyx: build: link",
    ] {
        assert!(stderr.contains(needle), "missing '{needle}' in:\n{stderr}");
    }

    let (code, _, stderr) = run_in(dir.path(), &["build", "--verbose", "hello.py"]);
    assert_eq!(code, 0, "cached verbose build failed: {stderr}");
    assert!(
        stderr.contains("pyx: build: cache hit"),
        "second verbose build must report the cache hit, got:\n{stderr}"
    );
}

#[test]
fn build_cache_hit() {
    if !runtime_lib_present() {
        eprintln!("skipping build_cache_hit: libpyx_runtime.a not found next to the test profile");
        return;
    }

    let dir = TestDir::new("build-cache");
    std::fs::write(dir.path().join("hello.py"), "print(\"Hello, PyX!\")\n")
        .expect("failed to write hello.py");

    let (code, _stdout, stderr) = run_in(dir.path(), &["build", "hello.py"]);
    assert_eq!(code, 0, "pyx build failed: {stderr}");

    let cache = dir.path().join(".pyx/cache");
    let mut names: Vec<String> = std::fs::read_dir(&cache)
        .expect("build cache directory must exist")
        .map(|entry| {
            entry
                .expect("cache entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    assert_eq!(
        names.len(),
        2,
        "cache must contain exactly one object and one binary, got: {names:?}"
    );
    assert!(names.iter().any(|name| name.ends_with(".o")), "{names:?}");
    assert!(names.iter().any(|name| name.ends_with(".bin")), "{names:?}");

    // The second build must reuse the cache and still produce a working binary.
    let (code, _stdout, stderr) = run_in(dir.path(), &["build", "hello.py"]);
    assert_eq!(code, 0, "cached pyx build failed: {stderr}");
    let (code, stdout, stderr) = run_executable(&dir.path().join("hello"));
    assert_eq!(code, 0, "cached binary failed: {stderr}");
    assert_eq!(stdout, "Hello, PyX!\n");

    // --no-cache must neither read nor populate the cache.
    let fresh = TestDir::new("build-no-cache");
    std::fs::write(fresh.path().join("hello.py"), "print(\"Hello, PyX!\")\n")
        .expect("failed to write hello.py");
    let (code, _stdout, stderr) = run_in(fresh.path(), &["build", "--no-cache", "hello.py"]);
    assert_eq!(code, 0, "pyx build --no-cache failed: {stderr}");
    assert!(
        !fresh.path().join(".pyx").exists(),
        "--no-cache must not create .pyx/cache"
    );
}

#[test]
fn build_cache_invalidates_imported_module() {
    if !runtime_lib_present() {
        eprintln!(
            "skipping build_cache_invalidates_imported_module: \
             libpyx_runtime.a not found next to the test profile"
        );
        return;
    }

    let dir = TestDir::new("build-cache-module");
    let helper = dir.path().join("helper.py");
    std::fs::write(&helper, "def value():\n    return 1\n").expect("failed to write helper.py");
    std::fs::write(
        dir.path().join("main.py"),
        "from helper import value\nprint(value())\n",
    )
    .expect("failed to write main.py");

    let binary = dir.path().join("main");
    let (code, _stdout, stderr) = run_in(dir.path(), &["build", "main.py"]);
    assert_eq!(code, 0, "pyx build failed: {stderr}");
    let (code, stdout, stderr) = run_executable(&binary);
    assert_eq!(code, 0, "first build output failed: {stderr}");
    assert_eq!(stdout, "1\n");

    // Changing an imported module must invalidate the cached binary.
    std::fs::write(&helper, "def value():\n    return 2\n").expect("failed to rewrite helper.py");
    let (code, _stdout, stderr) = run_in(dir.path(), &["build", "main.py"]);
    assert_eq!(code, 0, "pyx build after helper change failed: {stderr}");
    let (code, stdout, stderr) = run_executable(&binary);
    assert_eq!(code, 0, "rebuilt output failed: {stderr}");
    assert_eq!(
        stdout, "2\n",
        "changing an imported module must invalidate the build cache"
    );

    // Unchanged sources must keep producing the same output.
    let (code, _stdout, stderr) = run_in(dir.path(), &["build", "main.py"]);
    assert_eq!(code, 0, "cached pyx build failed: {stderr}");
    let (code, stdout, stderr) = run_executable(&binary);
    assert_eq!(code, 0, "cached output failed: {stderr}");
    assert_eq!(stdout, "2\n");
}

// --- uv passthrough commands ----------------------------------------------

/// Runs the `pyx` binary in `dir` with extra environment variables.
fn run_in_with_env(dir: &Path, args: &[&str], envs: &[(&str, &OsStr)]) -> (i32, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pyx"));
    command.args(args).current_dir(dir);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.output().expect("failed to spawn pyx binary");
    output_parts(output)
}

/// A shell script standing in for `uv`, selected through `PYX_UV`.
///
/// Each invocation appends its working directory and arguments to
/// `$PYX_FAKE_UV_LOG` and exits with `$PYX_FAKE_UV_EXIT` (default 0).
struct FakeUv {
    program: PathBuf,
    log: PathBuf,
}

impl FakeUv {
    fn new(dir: &Path) -> Self {
        let program = dir.join("fake-uv");
        let log = dir.join("uv.log");
        std::fs::write(
            &program,
            "#!/bin/sh\n\
             {\n\
               printf 'cwd=%s\\n' \"$(pwd -P)\"\n\
               for arg in \"$@\"; do\n\
                 printf 'arg=%s\\n' \"$arg\"\n\
               done\n\
             } >> \"$PYX_FAKE_UV_LOG\"\n\
             exit \"${PYX_FAKE_UV_EXIT:-0}\"\n",
        )
        .expect("failed to write fake uv");
        let mut permissions = std::fs::metadata(&program)
            .expect("failed to stat fake uv")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&program, permissions).expect("failed to make fake uv executable");
        Self { program, log }
    }

    /// Runs `pyx <args>` in `dir` through the fake and returns the recorded log.
    fn run(&self, dir: &Path, args: &[&str], exit: &str) -> (i32, String, String, Vec<String>) {
        let _ = std::fs::remove_file(&self.log);
        let program = self.program.as_os_str();
        let log = self.log.as_os_str();
        let (code, stdout, stderr) = run_in_with_env(
            dir,
            args,
            &[
                ("PYX_UV", program),
                ("PYX_FAKE_UV_LOG", log),
                ("PYX_FAKE_UV_EXIT", OsStr::new(exit)),
            ],
        );
        let recorded = std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        (code, stdout, stderr, recorded)
    }
}

/// Creates a project directory containing a minimal `pyproject.toml`.
fn uv_project(dir: &Path) -> PathBuf {
    let project = dir.join("project");
    std::fs::create_dir_all(&project).expect("failed to create project directory");
    std::fs::write(
        project.join("pyproject.toml"),
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .expect("failed to write pyproject.toml");
    project
        .canonicalize()
        .expect("project directory must be canonicalizable")
}

#[test]
fn uv_commands_forward_arguments_in_cwd() {
    let dir = TestDir::new("uv-forward");
    let project = uv_project(dir.path());
    let fake = FakeUv::new(dir.path());
    let cwd = format!("cwd={}", project.display());

    let cases: [(&[&str], Vec<&str>); 4] = [
        (
            &["add", "requests", "flask>=3"],
            vec!["add", "requests", "flask>=3"],
        ),
        (
            &["remove", "--dev", "pytest"],
            vec!["remove", "--dev", "pytest"],
        ),
        (&["sync", "--frozen"], vec!["sync", "--frozen"]),
        (&["lock", "--upgrade"], vec!["lock", "--upgrade"]),
    ];

    for (args, forwarded) in cases {
        let (code, stdout, stderr, recorded) = fake.run(&project, args, "0");
        assert_eq!(code, 0, "pyx {args:?} failed: {stderr}");
        assert!(stdout.is_empty(), "pyx {args:?} must not write to stdout");
        assert!(stderr.is_empty(), "pyx {args:?} must not write to stderr");

        let mut expected = vec![cwd.clone()];
        expected.extend(forwarded.iter().map(|arg| format!("arg={arg}")));
        assert_eq!(
            recorded, expected,
            "pyx {args:?} must invoke uv with the arguments verbatim"
        );
    }
}

#[test]
fn uv_exit_status_is_returned() {
    let dir = TestDir::new("uv-status");
    let project = uv_project(dir.path());
    let fake = FakeUv::new(dir.path());

    for exit in ["1", "7"] {
        let (code, stdout, stderr, recorded) = fake.run(&project, &["sync"], exit);
        assert_eq!(
            code,
            exit.parse::<i32>().unwrap(),
            "uv status must pass through"
        );
        assert!(
            stdout.is_empty(),
            "pyx must not add stdout for a failing uv"
        );
        assert!(
            stderr.is_empty(),
            "pyx must not add stderr for a failing uv"
        );
        assert_eq!(recorded[0], format!("cwd={}", project.display()));
        assert_eq!(recorded[1], "arg=sync");
    }
}

#[test]
fn uv_missing_executable_is_a_setup_error() {
    let dir = TestDir::new("uv-missing");
    let project = uv_project(dir.path());
    let program = dir.path().join("does-not-exist/uv");

    let (code, stdout, stderr) = run_in_with_env(
        &project,
        &["add", "requests"],
        &[("PYX_UV", program.as_os_str())],
    );

    assert_eq!(code, 2, "a missing uv executable must exit with status 2");
    assert!(stdout.is_empty(), "the error belongs on stderr");
    assert!(
        stderr.contains("cannot run") && stderr.contains("PYX_UV"),
        "missing actionable uv error, got:\n{stderr}"
    );
}

#[test]
fn uv_commands_do_not_require_pyproject() {
    let dir = TestDir::new("uv-no-pyproject");
    let fake = FakeUv::new(dir.path());

    // Without a pyproject.toml, pyx still delegates: uv decides and reports.
    let (code, _stdout, _stderr, recorded) = fake.run(dir.path(), &["lock"], "1");

    assert_eq!(code, 1, "uv's own failure must be returned");
    assert_eq!(
        recorded,
        vec![
            format!(
                "cwd={}",
                dir.path()
                    .canonicalize()
                    .expect("canonical temp dir")
                    .display()
            ),
            "arg=lock".to_owned(),
        ],
        "pyx must not pre-check pyproject.toml"
    );
}

#[test]
fn help_lists_uv_commands() {
    let (code, stdout, stderr) = run(&["help"]);

    assert_eq!(code, 0, "pyx help failed: {stderr}");
    for needle in [
        "pyx add",
        "pyx remove",
        "pyx sync",
        "pyx lock",
        "PYX_UV",
        "uv commands",
    ] {
        assert!(
            stdout.contains(needle),
            "help output is missing '{needle}':\n{stdout}"
        );
    }
}

#[test]
fn run_del_ellipsis_and_console_probes() {
    let dir = TestDir::new("console-language");
    let program = dir.path().join("console_language.py");
    std::fs::write(
        &program,
        concat!(
            "values = [1, 2, 3, 4]\n",
            "del values[1]\n",
            "print(values)\n",
            "del values[1:3]\n",
            "print(values)\n",
            "del values[:]\n",
            "print(values)\n",
            "table = {'a': 1, 'b': 2}\n",
            "del table['a']\n",
            "print(table)\n",
            "print(...)\n",
            "def f():\n",
            "    x = 1\n",
            "    del x\n",
            "    return x\n",
            "print(f())\n",
            "class C:\n",
            "    def __init__(self):\n",
            "        self.value = 7\n",
            "c = C()\n",
            "del c.value\n",
            "print(hasattr(c, 'value'))\n",
            "try:\n",
            "    raise SystemExit\n",
            "except SystemExit:\n",
            "    print('caught')\n",
            "try:\n",
            "    get_ipython\n",
            "except NameError:\n",
            "    print('missing')\n",
            "print('probe')\n",
            "ipython = get_ipython()\n",
            "print(ipython.__class__.__name__)\n",
        ),
    )
    .expect("failed to write temp program");
    let program = program.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(
        stdout,
        "[1, 3, 4]\n[1]\n[]\n{'b': 2}\nNone\nNone\nFalse\ncaught\nprobe\nTerminalInteractiveShell\n"
    );
}

#[test]
fn typing_alias_subscripts_are_runtime_values() {
    let dir = TestDir::new("typing-alias");
    let program = dir.path().join("typing_alias.py");
    std::fs::write(
        &program,
        concat!(
            "from typing import Tuple, Optional\n",
            "Span = Tuple[int, int]\n",
            "MaybeSpan = Optional[Span]\n",
            "print(Span.__name__, MaybeSpan.__name__)\n",
        ),
    )
    .expect("failed to write temp program");
    let program = program.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(stdout, "Tuple Optional\n");
}

#[test]
fn bigint_literals_promote_and_format() {
    let dir = TestDir::new("bigint");
    let program = dir.path().join("main.py");
    std::fs::write(
        &program,
        concat!(
            "a = 2 ** 100\n",
            "print(a)\n",
            "print(a + 1, a - 1, a * 2)\n",
            "print(a // 3, a % 3, divmod(a, 7))\n",
            "print(-a, abs(-a))\n",
            "print(a > 10 ** 29, a == 2 ** 100)\n",
            "print(f\"{a:#x}\")\n",
            "print(int(\"123456789012345678901234567890\"))\n",
            "d = {a: 1}\n",
            "print(d[a], len(d))\n",
        ),
    )
    .expect("failed to write temp program");
    let program = program.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", &program]);
    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(
        stdout,
        "1267650600228229401496703205376\n\
         1267650600228229401496703205377 1267650600228229401496703205375 2535301200456458802993406410752\n\
         422550200076076467165567735125 1 (181092942889747057356671886482, 2)\n\
         -1267650600228229401496703205376 1267650600228229401496703205376\n\
         True True\n\
         0x10000000000000000000000000\n\
         123456789012345678901234567890\n\
         1 1\n"
    );
}

#[test]
fn thread_subclass_and_stdlib_helpers_run() {
    let dir = TestDir::new("thread-subclass");
    let source = dir.path().join("main.py");
    std::fs::write(
        &source,
        r#"import html
import json
import re
import threading

class T(threading.Thread):
    def __init__(self, n, daemon=False):
        super().__init__(daemon=daemon)
        self.n = n
        self.result = 0

    def run(self):
        self.result = self.n * 2

class Base:
    def __init__(self, value):
        self.value = value

class Derived(Base):
    def __init__(self):
        super().__init__(9)

t = T(21)
t.start()
t.join()
print(t.result, t.is_alive(), Derived().value, html.escape('<a&"'))
print(json.dumps({'a': 1}, skipkeys=True, check_circular=False, allow_nan=True))
print('{name:04d}'.format(name=7))
d = {'a': 1}
print(d.setdefault('b', 2), d)
m = re.compile(r'(x)').search('axb')
span = m.span
print(span(), m.group(1), m.groupdict())
"#,
    )
    .expect("failed to write temp program");
    let source = source.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", &source]);
    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(
        stdout,
        "42 False 9 &lt;a&amp;&quot;\n{\"a\": 1}\n0007\n2 {'a': 1, 'b': 2}\n(1, 2) x {}\n"
    );
}

#[test]
fn generator_expressions_are_single_pass_iterators() {
    let dir = TestDir::new("generator-expression");
    let program = dir.path().join("main.py");
    std::fs::write(
        &program,
        concat!(
            "def gen():\n",
            "    yield 'a'\n",
            "    yield 'b'\n",
            "print(next(x for x in [7, 8, 9]))\n",
            "print(''.join(gen()))\n",
            "print(''.join(x for x in ['c', 'd']))\n",
            "print([0, *gen(), 3])\n",
            "print(sum(x for x in range(4)))\n",
        ),
    )
    .expect("failed to write temp program");
    let program = program.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", &program]);
    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(stdout, "7\nab\ncd\n[0, 'a', 'b', 3]\n6\n");
}

#[test]
fn user_dunders_and_defaults_are_dispatched() {
    let dir = TestDir::new("user-dunders");
    let program = dir.path().join("dunders.py");
    std::fs::write(
        &program,
        concat!(
            "class C:\n",
            "    def __init__(self, x=3):\n",
            "        self.x = x\n",
            "    def __call__(self, y=1):\n",
            "        return self.x + y\n",
            "    def __len__(self):\n",
            "        return self.x\n",
            "    def __contains__(self, value):\n",
            "        return value == self.x\n",
            "    def __str__(self):\n",
            "        return 'C'\n",
            "    def __add__(self, other):\n",
            "        return self.x + other\n",
            "c = C()\n",
            "print(c(), len(c), 3 in c, str(c), c + 2)\n",
        ),
    )
    .expect("failed to write temp program");
    let program = program.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", &program]);

    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(stdout, "4 3 True C 5\n");
}

#[test]
fn uncaught_system_exit_and_input_eof_fail() {
    let dir = TestDir::new("console-errors");
    let system_exit = dir.path().join("system_exit.py");
    std::fs::write(&system_exit, "raise SystemExit\n").expect("failed to write temp program");
    let system_exit = system_exit.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", &system_exit]);
    assert_eq!(code, 0, "uncaught bare SystemExit exits 0");
    assert!(stdout.is_empty(), "unexpected stdout:\n{stdout}");
    assert!(
        stderr.is_empty(),
        "SystemExit must not print a traceback:\n{stderr}"
    );

    let system_exit_code = dir.path().join("system_exit_code.py");
    std::fs::write(&system_exit_code, "raise SystemExit(3)\n")
        .expect("failed to write temp program");
    let system_exit_code = system_exit_code.to_string_lossy().into_owned();
    let (code, _, stderr) = run(&["run", &system_exit_code]);
    assert_eq!(code, 3, "SystemExit(3) must use the stored code:\n{stderr}");
    assert!(stderr.is_empty(), "no traceback for SystemExit:\n{stderr}");

    let system_exit_message = dir.path().join("system_exit_message.py");
    std::fs::write(&system_exit_message, "import sys\nsys.exit('boom')\n")
        .expect("failed to write temp program");
    let system_exit_message = system_exit_message.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", &system_exit_message]);
    assert_eq!(code, 1, "sys.exit(str) must exit 1");
    assert!(stdout.is_empty(), "unexpected stdout:\n{stdout}");
    assert_eq!(stderr, "boom\n", "sys.exit(str) message goes to stderr");

    let eof = dir.path().join("input_eof.py");
    std::fs::write(&eof, "input('prompt: ')\n").expect("failed to write temp program");
    let eof = eof.to_string_lossy().into_owned();
    // `Command::output` gives the child a closed stdin, so `input()` reads EOF.
    let (code, stdout, stderr) = run(&["run", &eof]);
    assert_eq!(code, 1, "input() at EOF must exit nonzero");
    assert_eq!(
        stdout, "prompt: ",
        "the prompt is written without a newline"
    );
    assert!(
        stderr.contains("EOF when reading a line"),
        "missing EOF diagnostic:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// String accumulator (`s += ...` loop lowering)
// ---------------------------------------------------------------------------

/// The benchmark shape (`s += "x"` in a loop) plus the cases the frontend's
/// accumulator proof must keep semantically identical: an empty loop, a
/// `break` that keeps the partial value, two accumulators in one loop and a
/// function-local accumulator.
const STRING_ACCUMULATOR_SOURCE: &str = concat!(
    "n = 20000\n",
    "s = \"\"\n",
    "i = 0\n",
    "while i < n:\n",
    "    s += \"x\"\n",
    "    i += 1\n",
    "print(len(s))\n",
    "empty = \"seed\"\n",
    "j = 0\n",
    "while j < 0:\n",
    "    empty += \"y\"\n",
    "    j += 1\n",
    "print(empty)\n",
    "part = \"\"\n",
    "k = 0\n",
    "while True:\n",
    "    if k == 5:\n",
    "        break\n",
    "    part += \"ab\"\n",
    "    k += 1\n",
    "print(part)\n",
    "pair_a = \"\"\n",
    "pair_b = \"\"\n",
    "m = 0\n",
    "while m < 3:\n",
    "    pair_a += \"1\"\n",
    "    pair_b += \"2\"\n",
    "    m += 1\n",
    "print(pair_a, pair_b)\n",
    "def build(count):\n",
    "    out = \"\"\n",
    "    for index in range(count):\n",
    "        out += str(index)\n",
    "    return out\n",
    "text = build(10)\n",
    "print(len(text), text)\n",
);

const STRING_ACCUMULATOR_STDOUT: &str = "20000\nseed\nababababab\n111 222\n10 0123456789\n";

fn write_string_accumulator(dir: &TestDir) -> String {
    let program = dir.path().join("string_accumulator.py");
    std::fs::write(&program, STRING_ACCUMULATOR_SOURCE).expect("failed to write temp program");
    program.to_string_lossy().into_owned()
}

#[test]
fn run_string_accumulator() {
    let dir = TestDir::new("string-accumulator");
    let program = write_string_accumulator(&dir);
    let (code, stdout, stderr) = run(&["run", &program]);
    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, STRING_ACCUMULATOR_STDOUT);
}

#[test]
fn run_string_accumulator_gc_stress() {
    let dir = TestDir::new("string-accumulator-gc");
    let program = write_string_accumulator(&dir);
    for env in [("PYX_GC_STRESS", "1"), ("PYX_GC_THRESHOLD", "8")] {
        let (code, stdout, stderr) = run_with_env(&["run", &program], &[env]);
        assert_eq!(code, 0, "pyx run failed with {env:?}: {stderr}");
        assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
        assert_eq!(stdout, STRING_ACCUMULATOR_STDOUT);
    }
}

#[test]
fn run_string_accumulator_fallbacks() {
    // Aliasing before the loop and a conditional append must keep the generic
    // concatenation path and the exact Python values.
    let dir = TestDir::new("string-accumulator-fallback");
    let program = dir.path().join("fallback.py");
    std::fs::write(
        &program,
        concat!(
            "s = \"\"\n",
            "alias = s\n",
            "for i in range(3):\n",
            "    s += \"x\"\n",
            "print(s, len(alias))\n",
            "t = \"\"\n",
            "for i in range(5):\n",
            "    if i % 2 == 0:\n",
            "        t += str(i)\n",
            "print(t)\n",
        ),
    )
    .expect("failed to write temp program");
    let program = program.to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["run", &program]);
    assert_eq!(code, 0, "pyx run failed: {stderr}");
    assert_eq!(stdout, "xxx 0\n024\n");
}

#[test]
fn build_string_accumulator() {
    if !runtime_lib_present() {
        eprintln!(
            "skipping build_string_accumulator: libpyx_runtime.a not found next to the test \
             profile (AOT is covered by scripts/verify.sh, which builds it first)"
        );
        return;
    }

    let dir = TestDir::new("build_string_accumulator");
    let program = write_string_accumulator(&dir);
    let binary = dir.path().join("string_accumulator");
    let binary_arg = binary.to_string_lossy().into_owned();

    let (code, stdout, stderr) = run(&["build", &program, "-o", &binary_arg]);
    assert_eq!(
        code, 0,
        "pyx build failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let (code, stdout, stderr) = run_executable(&binary);
    assert_eq!(code, 0, "AOT binary failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, STRING_ACCUMULATOR_STDOUT);
}
