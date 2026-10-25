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

fn run_source_stressed(test_name: &str, source: &str) -> (i32, String, String) {
    let path = write_source(test_name, source);
    let output = Command::new(env!("CARGO_BIN_EXE_pyx"))
        .args(["run", path.to_str().expect("UTF-8 temp path")])
        .env("PYX_GC_STRESS", "1")
        .output()
        .expect("failed to spawn pyx");
    let _ = std::fs::remove_dir_all(path.parent().expect("temp parent"));
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Whether the AOT runtime archive is available next to the test profile.
fn runtime_lib_present() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    exe.ancestors()
        .any(|dir| dir.join("libpyx_runtime.a").is_file())
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
fn captured_mutations_are_shared_with_the_enclosing_function() {
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
    assert_eq!(stdout, "2\n");
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
fn nonlocal_mutations_are_visible_inside_and_outside() {
    let source = "\
def outer():
    x = 0
    def bump():
        nonlocal x
        x += 1
        return x
    first = bump()
    x = 10
    return first, x, bump()

a, b, c = outer()
print(a, b, c)
";
    let (code, stdout, stderr) = run_source("nonlocal_shared", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "1 10 11\n");
}

#[test]
fn sibling_closures_share_one_cell() {
    let source = "\
def pair():
    x = 0
    def inc():
        nonlocal x
        x += 1
    def get():
        return x
    return inc, get

inc, get = pair()
inc()
inc()
print(get())
";
    let (code, stdout, stderr) = run_source("shared_cell", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "2\n");
}

#[test]
fn loop_mutations_update_the_outer_cell() {
    let source = "\
def outer():
    x = 0
    def get():
        return x
    for _ in range(3):
        x += 1
    return get()

print(outer())
";
    let (code, stdout, stderr) = run_source("loop_shared_cell", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "3\n");
}

#[test]
fn nonlocal_mutation_from_a_loop_updates_the_outer_binding() {
    let source = "\
def outer():
    x = 0
    def bump():
        nonlocal x
        for _ in range(3):
            x += 1
    bump()
    return x

print(outer())
";
    let (code, stdout, stderr) = run_source("nonlocal_loop", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "3\n");
}

#[test]
fn nonlocal_rebinding_replaces_the_enclosing_binding() {
    let source = "\
def outer():
    x = 1
    def set_value(v):
        nonlocal x
        x = v
    def get():
        return x
    return set_value, get

set_value, get = outer()
print(get())
set_value(7)
print(get())
";
    let (code, stdout, stderr) = run_source("nonlocal_rebind", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "1\n7\n");
}

#[test]
fn loop_closures_get_a_distinct_cell_per_iteration() {
    let source = "\
def make_all():
    fns = []
    for i in range(3):
        i += 1
        def inc():
            nonlocal i
            i += 1
            return i
        fns.append(inc)
    return fns

a, b, c = make_all()
print(a())
print(b())
print(c())
";
    let (code, stdout, stderr) = run_source("loop_cells", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    // A shared cell would print 4, 5, 6: every closure would see the last
    // iteration's value. Distinct per-iteration cells keep 2, 3, 4.
    assert_eq!(stdout, "2\n3\n4\n");
}

#[test]
fn missing_nonlocal_binding_is_a_compile_diagnostic() {
    let path = write_source(
        "missing_nonlocal",
        "\
def outer():
    def inner():
        nonlocal absent
        absent = 1
    return inner
",
    );
    let output = run(&["check", path.to_str().expect("UTF-8 temp path")]);
    let _ = std::fs::remove_dir_all(path.parent().expect("temp parent"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr:\n{stderr}");
    assert!(
        stderr.contains("no binding for nonlocal 'absent' found"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn cells_survive_gc_stress() {
    let source = "\
def make():
    text = \"abc\"
    def get():
        return text
    text = text + \"d\"
    return get

fns = []
for _ in range(20):
    fns.append(make())
print(len(fns[19]()))
";
    let (code, stdout, stderr) = run_source_stressed("gc_stress_cells", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "4\n");
}

#[test]
fn nonlocal_closures_run_in_aot_builds() {
    if !runtime_lib_present() {
        eprintln!(
            "skipping nonlocal_closures_run_in_aot_builds: libpyx_runtime.a not found next to \
             the test profile"
        );
        return;
    }
    let dir = write_source(
        "aot_nonlocal",
        "\
def pair():
    x = 0
    def inc():
        nonlocal x
        x += 1
    def get():
        return x
    return inc, get

inc, get = pair()
inc()
inc()
print(get())
",
    );
    let dir = dir.parent().expect("temp parent").to_path_buf();
    let binary = dir.join("closure_cells");
    let binary_arg = binary.to_string_lossy().into_owned();
    let source = dir.join("main.py");
    let (code, stdout, stderr) = {
        let output = run(&[
            "build",
            source.to_str().expect("UTF-8 temp path"),
            "-o",
            &binary_arg,
        ]);
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    assert_eq!(code, 0, "stdout:\n{stdout}\nstderr:\n{stderr}");
    let output = Command::new(&binary).output().expect("run AOT binary");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "2\n");
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
