//! End-to-end tests for Python-compatible exception handling: the
//! `Exception`/`BaseException` hierarchies, tuple handlers and explicit
//! exception chaining via `raise ... from ...`.
//!
//! Traceback frames are simplified in PyX, so these tests assert the printed
//! messages and their order rather than byte-for-byte CPython output.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// CPython's separator between an explicit cause and the exception it causes.
const DIRECT_CAUSE: &str = "The above exception was the direct cause of the following exception:";

/// CPython's separator between an implicitly handled exception and the
/// exception raised while handling it.
const DURING_HANDLING: &str = "During handling of the above exception, another exception occurred:";

fn write_source(test_name: &str, source: &str) -> PathBuf {
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pyx-exceptions-{}-{test_name}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("failed to create temp dir");
    let path = dir.join("main.py");
    std::fs::write(&path, source).expect("failed to write source");
    path
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pyx"))
        .args(args)
        .output()
        .expect("failed to spawn pyx")
}

fn run_source(test_name: &str, source: &str) -> (i32, String, String) {
    let path = write_source(test_name, source);
    let output = run(&["run", path.to_str().expect("UTF-8 temp path")]);
    let _ = std::fs::remove_dir_all(path.parent().expect("temp parent"));
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Asserts that `needle` occurs in `haystack` at or after byte `from`.
fn assert_after(haystack: &str, needle: &str, from: usize) -> usize {
    let rest = haystack
        .get(from..)
        .unwrap_or_else(|| panic!("offset {from} is not a byte boundary in {haystack:?}"));
    let at = rest
        .find(needle)
        .unwrap_or_else(|| panic!("missing {needle:?} in {haystack:?}"));
    from + at
}

#[test]
fn builtin_exception_classes_are_first_class_values() {
    let source = "\
print(ValueError.__name__)
print(ValueError)
print(ValueError is ValueError)
def pick():
    return ValueError
print(pick() is ValueError)
print(BaseException.__name__, Exception.__name__, AssertionError.__name__, GeneratorExit.__name__)
";
    let (code, stdout, stderr) = run_source("exception_class_values", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(
        stdout,
        "ValueError\n<class 'ValueError'>\nTrue\nTrue\n\
         BaseException Exception AssertionError GeneratorExit\n"
    );
}

#[test]
fn user_classes_inherit_builtin_exception_classes() {
    let source = "\
class E(Exception):
    def describe(self):
        return 'E:' + self.__class__.__name__
class F(E):
    pass
print(E.__name__, F.__name__)
print(E)
e = E()
f = F()
print(isinstance(e, E), isinstance(e, Exception), isinstance(e, F))
print(e.describe())
f.extra = 7
print(f.extra)
class G(ValueError):
    pass
print(G.__name__)
";
    let (code, stdout, stderr) = run_source("exception_class_inheritance", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "E F\n<class 'E'>\nTrue True False\nE:E\n7\nG\n");
}

#[test]
fn except_exception_catches_standard_errors() {
    let source = "\
def fail(kind):
    if kind == 1:
        raise ValueError('bad value')
    if kind == 2:
        raise TypeError('bad type')
    if kind == 3:
        raise RuntimeError('bad runtime')

for kind in [1, 2, 3]:
    try:
        fail(kind)
    except Exception:
        print('caught')
";
    let (code, stdout, stderr) = run_source("exception_catches", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "caught\ncaught\ncaught\n");
}

#[test]
fn except_exception_does_not_catch_keyboard_interrupt() {
    let source = "\
try:
    raise KeyboardInterrupt('stop')
except Exception:
    print('caught')
print('unreachable')
";
    let (code, stdout, stderr) = run_source("exception_keyboard", source);
    assert_eq!(code, 1, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(!stdout.contains("caught"), "stdout:\n{stdout}");
    assert!(
        stderr.contains("KeyboardInterrupt: stop"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn except_base_exception_catches_keyboard_interrupt() {
    let source = "\
try:
    raise KeyboardInterrupt('stop')
except BaseException as e:
    print(e)
print('after')
";
    let (code, stdout, stderr) = run_source("base_exception", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "stop\nafter\n");
}

#[test]
fn tuple_handler_catches_both_types_with_the_message() {
    let source = "\
for kind in [1, 2]:
    try:
        if kind == 1:
            raise ValueError('value bad')
        else:
            raise TypeError('type bad')
    except (ValueError, TypeError) as e:
        print(e)
";
    let (code, stdout, stderr) = run_source("tuple_catches", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "value bad\ntype bad\n");
}

#[test]
fn tuple_handler_does_not_catch_unmatched_exceptions() {
    let source = "\
try:
    raise RuntimeError('boom')
except (ValueError, TypeError):
    print('caught')
print('unreachable')
";
    let (code, stdout, stderr) = run_source("tuple_unmatched", source);
    assert_eq!(code, 1, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(!stdout.contains("caught"), "stdout:\n{stdout}");
    assert!(stderr.contains("RuntimeError: boom"), "stderr:\n{stderr}");
}

#[test]
fn earlier_exception_handler_wins_over_a_later_concrete_one() {
    let source = "\
try:
    raise ValueError('v')
except Exception:
    print('first')
except ValueError:
    print('second')
";
    let (code, stdout, stderr) = run_source("handler_order", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "first\n");
}

#[test]
fn raise_from_prints_the_cause_then_the_raised_exception() {
    let source = "\
def inner():
    raise ValueError('inner boom')

def outer():
    try:
        inner()
    except ValueError as e:
        raise RuntimeError('outer boom') from e

outer()
";
    let (code, _, stderr) = run_source("raise_from", source);
    assert_eq!(code, 1, "stderr:\n{stderr}");
    let inner = assert_after(&stderr, "ValueError: inner boom", 0);
    let marker = assert_after(&stderr, DIRECT_CAUSE, inner);
    assert_after(&stderr, "RuntimeError: outer boom", marker);
}

#[test]
fn raise_from_none_suppresses_the_original_cause() {
    let source = "\
def inner():
    raise ValueError('inner boom')

def outer():
    try:
        inner()
    except ValueError:
        raise RuntimeError('outer boom') from None

outer()
";
    let (code, _, stderr) = run_source("raise_from_none", source);
    assert_eq!(code, 1, "stderr:\n{stderr}");
    assert!(
        !stderr.contains("inner boom"),
        "the suppressed cause must not be printed:\n{stderr}"
    );
    assert!(!stderr.contains(DIRECT_CAUSE), "stderr:\n{stderr}");
    assert!(
        stderr.contains("RuntimeError: outer boom"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn division_error_in_handler_chains_to_the_new_exception() {
    let source = "\
try:
    1 / 0
except ZeroDivisionError:
    raise ValueError('x')
";
    let (code, _, stderr) = run_source("division_context", source);
    assert_eq!(code, 1, "stderr:\n{stderr}");
    let division = assert_after(&stderr, "ZeroDivisionError", 0);
    let marker = assert_after(&stderr, DURING_HANDLING, division);
    assert_after(&stderr, "ValueError: x", marker);
}

#[test]
fn nested_handlers_chain_implicit_context_deepest_first() {
    let source = "\
def inner():
    raise ValueError('inner boom')

def outer():
    try:
        inner()
    except ValueError:
        try:
            raise TypeError('middle boom')
        except TypeError:
            raise RuntimeError('outer boom')

outer()
";
    let (code, _, stderr) = run_source("implicit_nested", source);
    assert_eq!(code, 1, "stderr:\n{stderr}");
    let inner = assert_after(&stderr, "ValueError: inner boom", 0);
    let middle = assert_after(&stderr, DURING_HANDLING, inner);
    let middle_frame = assert_after(&stderr, "TypeError: middle boom", middle);
    let outer_frame = assert_after(&stderr, DURING_HANDLING, middle_frame);
    assert_after(&stderr, "RuntimeError: outer boom", outer_frame);
    assert_eq!(
        stderr.matches(DURING_HANDLING).count(),
        2,
        "both implicit links must be rendered:\n{stderr}"
    );
    assert!(!stderr.contains(DIRECT_CAUSE), "stderr:\n{stderr}");
}

#[test]
fn explicit_cause_wins_over_implicit_context_in_a_handler() {
    let source = "\
def inner():
    raise ValueError('inner boom')

def outer():
    try:
        inner()
    except ValueError as e:
        raise RuntimeError('outer boom') from e

outer()
";
    let (code, _, stderr) = run_source("explicit_over_context", source);
    assert_eq!(code, 1, "stderr:\n{stderr}");
    let inner = assert_after(&stderr, "ValueError: inner boom", 0);
    let marker = assert_after(&stderr, DIRECT_CAUSE, inner);
    assert_after(&stderr, "RuntimeError: outer boom", marker);
    assert!(
        !stderr.contains(DURING_HANDLING),
        "explicit cause must suppress implicit context:\n{stderr}"
    );
}

#[test]
fn raise_from_none_suppresses_implicit_context_in_a_handler() {
    let source = "\
try:
    raise ValueError('inner boom')
except ValueError:
    raise RuntimeError('outer boom') from None
";
    let (code, _, stderr) = run_source("from_none_context", source);
    assert_eq!(code, 1, "stderr:\n{stderr}");
    assert!(!stderr.contains("inner boom"), "stderr:\n{stderr}");
    assert!(!stderr.contains(DURING_HANDLING), "stderr:\n{stderr}");
    assert!(!stderr.contains(DIRECT_CAUSE), "stderr:\n{stderr}");
    assert!(
        stderr.contains("RuntimeError: outer boom"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn bare_re_raise_in_a_handler_does_not_chain_to_itself() {
    let source = "\
def fail():
    try:
        raise ValueError('once')
    except ValueError:
        raise

fail()
";
    let (code, _, stderr) = run_source("handler_reraise", source);
    assert_eq!(code, 1, "stderr:\n{stderr}");
    assert_eq!(
        stderr.matches("ValueError: once").count(),
        1,
        "a bare re-raise must print the exception once:\n{stderr}"
    );
    assert!(!stderr.contains(DURING_HANDLING), "stderr:\n{stderr}");
}

#[test]
fn handler_context_does_not_leak_after_normal_break_or_return() {
    let source = "\
def normal():
    try:
        raise ValueError('first')
    except ValueError:
        pass

def via_break():
    while True:
        try:
            raise ValueError('second')
        except ValueError:
            break

def via_return():
    try:
        raise ValueError('third')
    except ValueError:
        return

def raised_after_return():
    via_return()
    raise TypeError('after return')

normal()
via_break()
raised_after_return()
";
    let (code, _, stderr) = run_source("no_context_leak", source);
    assert_eq!(code, 1, "stderr:\n{stderr}");
    assert!(
        !stderr.contains("ValueError"),
        "handled exceptions must not leak into later raises:\n{stderr}"
    );
    assert!(!stderr.contains(DURING_HANDLING), "stderr:\n{stderr}");
    assert!(
        stderr.contains("TypeError: after return"),
        "stderr:\n{stderr}"
    );
}
