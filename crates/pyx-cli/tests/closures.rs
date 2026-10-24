//! End-to-end tests for nested-function closures: source -> frontend ->
//! codegen -> runtime.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn write_source(test_name: &str, source: &str) -> PathBuf {
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pyx-closures-{}-{test_name}-{unique}",
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

const MAKE_ADD: &str = "\
def make(x):
    def add(y):
        return x + y
    return add
";

#[test]
fn nested_function_captures_a_parameter() {
    let source = format!("{MAKE_ADD}\nf = make(2)\nprint(f(3))\n");
    let (code, stdout, stderr) = run_source("basic", &source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "5\n");
}

#[test]
fn independent_closures_keep_their_own_captures() {
    let source =
        format!("{MAKE_ADD}\na = make(2)\nb = make(10)\nprint(a(3))\nprint(b(3))\nprint(a(4))\n");
    let (code, stdout, stderr) = run_source("independent", &source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "5\n13\n6\n");
}

#[test]
fn captures_are_values_taken_at_definition_time() {
    let source = "\
def outer():
    x = 1
    def get():
        return x
    x = 2
    return get

f = outer()
print(f())
";
    let (code, stdout, stderr) = run_source("value_semantics", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "1\n");
}

#[test]
fn closure_in_a_loop_captures_the_current_value() {
    let source = "\
def make_all():
    fns = []
    for i in range(3):
        def get():
            return i
        fns.append(get)
    return fns

fns = make_all()
print(fns[0]())
print(fns[1]())
print(fns[2]())
";
    let (code, stdout, stderr) = run_source("loop", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "0\n1\n2\n");
}

#[test]
fn captured_heap_values_survive_collection() {
    let source = "\
def make():
    text = \"abc\"
    def get():
        return text
    return get

f = make()
print(len(f()))
";
    let (code, stdout, stderr) = run_source("gc", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "3\n");
}

#[test]
fn recursive_nested_functions_are_rejected_with_a_diagnostic() {
    let path = write_source(
        "recursive",
        "\
def outer():
    def inner():
        return inner()
    return inner

f = outer()
",
    );
    let output = run(&["check", path.to_str().expect("UTF-8 temp path")]);
    let _ = std::fs::remove_dir_all(path.parent().expect("temp parent"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr:\n{stderr}");
    assert!(
        stderr.contains("recursive nested functions are not supported"),
        "stderr:\n{stderr}"
    );
}
