//! Differential tests: `pyx run` (JIT) and `pyx run --interpret` must agree
//! on stdout, stderr and the exit code for the same source file.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn write_source(test_name: &str, source: &str) -> PathBuf {
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pyx-interpret-{}-{test_name}-{unique}",
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

fn cleanup(path: &Path) {
    let _ = std::fs::remove_dir_all(path.parent().expect("temp parent"));
}

fn compare(file: &Path, extra: &[&str]) {
    let file = file.to_str().expect("UTF-8 path");
    let mut jit_args = vec!["run", file];
    jit_args.extend_from_slice(extra);
    let mut interpret_args = vec!["run", "--interpret", file];
    interpret_args.extend_from_slice(extra);
    let jit = run(&jit_args);
    let interpreted = run(&interpret_args);
    assert_eq!(
        jit.status.code(),
        interpreted.status.code(),
        "exit codes differ for {file}; JIT stderr:\n{}\ninterpreter stderr:\n{}",
        String::from_utf8_lossy(&jit.stderr),
        String::from_utf8_lossy(&interpreted.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&jit.stdout),
        String::from_utf8_lossy(&interpreted.stdout),
        "stdout differs for {file}"
    );
    assert_eq!(
        String::from_utf8_lossy(&jit.stderr),
        String::from_utf8_lossy(&interpreted.stderr),
        "stderr differs for {file}"
    );
}

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
}

#[test]
fn examples_match_the_jit() {
    for name in [
        "classes.py",
        "closures.py",
        "finally_reraise.py",
        "generators_async.py",
        "slices.py",
        "formatting.py",
        "sets.py",
        "tuples.py",
        "comprehensions.py",
        "exceptions.py",
    ] {
        compare(&example(name), &[]);
    }
}

#[test]
fn containers_and_formatting_match() {
    let source = "\
xs = [1, 2, 3]
xs.append(4)
print(xs)
d = {\"b\": 2, \"a\": 1}
d[\"c\"] = 3
print(d)
print(sorted(d.keys()))
print({1, 2, 3})
print((1, \"a\", 2.5))
print(f\"value={xs[1]:>3} name={'x'}\")
print(\"%s-%d\" % (\"a\", 7))
for k in d:
    print(k, d[k])
";
    let path = write_source("containers", source);
    compare(&path, &[]);
    cleanup(&path);
}

#[test]
fn uncaught_exceptions_match() {
    let source = "\
def explode(value):
    if value > 1:
        raise ValueError(\"boom\")
    return value

print(explode(1))
print(explode(2))
";
    let path = write_source("uncaught", source);
    compare(&path, &[]);
    cleanup(&path);
}

#[test]
fn program_arguments_match() {
    let source = "\
import sys
print(len(sys.argv))
print(sys.argv[1])
print(sys.argv[2])
";
    let path = write_source("argv", source);
    compare(&path, &["alpha", "beta"]);
    cleanup(&path);
}

#[test]
fn interpret_flag_is_not_forwarded_to_the_program() {
    let source = "\
import sys
print(len(sys.argv))
";
    let path = write_source("flag", source);
    compare(&path, &[]);
    cleanup(&path);
}
