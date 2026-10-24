//! End-to-end tests for real lazy generators and stackful coroutines driven
//! through the `pyx` binary (source -> frontend -> codegen -> runtime).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn write_source(test_name: &str, source: &str) -> PathBuf {
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pyx-fibers-{}-{test_name}-{unique}",
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

fn run_source(test_name: &str, source: &str) -> (i32, String, String) {
    let path = write_source(test_name, source);
    let output = run(&["run", path.to_str().expect("UTF-8 temp path")]);
    cleanup(&path);
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Like [`run_source`], but also reports the wall time of the child process.
fn run_source_timed(test_name: &str, source: &str) -> (i32, String, String, Duration) {
    let path = write_source(test_name, source);
    let started = Instant::now();
    let output = run(&["run", path.to_str().expect("UTF-8 temp path")]);
    let elapsed = started.elapsed();
    cleanup(&path);
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        elapsed,
    )
}

#[test]
fn generator_body_is_lazy_until_iteration() {
    let source = "\
def gen():
    print(\"body-start\")
    yield 1
    print(\"body-end\")

g = gen()
print(\"created\")
for x in g:
    print(x)
print(\"done\")
";
    let (code, stdout, stderr) = run_source("lazy", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "created\nbody-start\n1\nbody-end\ndone\n");
}

#[test]
fn generator_objects_keep_independent_state() {
    let source = "\
def gen():
    for i in range(3):
        yield i

first = gen()
second = gen()
print(list(first))
print(list(second))
print(list(first))
print(list(gen()))
";
    let (code, stdout, stderr) = run_source("independent", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    // A full pass over `first`, a full pass over its independent sibling
    // `second` with the same sequence, `first` exhausted, and a fresh object
    // still starting from the first element.
    assert_eq!(stdout, "[0, 1, 2]\n[0, 1, 2]\n[]\n[0, 1, 2]\n");
}

#[test]
fn return_inside_a_generator_ends_iteration() {
    let source = "\
def gen(n):
    i = 0
    while i < n:
        if i == 2:
            return
        yield i
        i += 1

print(list(gen(5)))
print(list(gen(1)))
";
    let (code, stdout, stderr) = run_source("return", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "[0, 1]\n[0]\n");
}

#[test]
fn exception_inside_a_generator_reaches_the_consumer() {
    let source = "\
def gen():
    yield 1
    raise ValueError(\"boom in generator\")

for value in gen():
    print(value)
";
    let (code, stdout, stderr) = run_source("raise", source);
    assert_eq!(code, 1, "uncaught generator exception must exit 1");
    assert_eq!(
        stdout, "1\n",
        "the value yielded before the raise must print"
    );
    assert!(
        stderr.contains("Traceback (most recent call last):"),
        "missing traceback, got:\n{stderr}"
    );
    assert!(
        stderr.contains("ValueError: boom in generator"),
        "missing propagated exception message, got:\n{stderr}"
    );
}

#[test]
fn generator_is_consumed_by_list_and_sum() {
    let source = "\
def gen(n):
    i = 0
    while i < n:
        yield i
        i += 1

print(list(gen(4)))
print(sum(gen(4)))
print(sum(gen(5), 10))
";
    let (code, stdout, stderr) = run_source("builtins", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "[0, 1, 2, 3]\n6\n20\n");
}

#[test]
fn gather_runs_coroutines_concurrently_in_input_order() {
    let source = "\
import asyncio
import time

async def sleeper(value):
    await asyncio.sleep(0.08)
    return value

async def main():
    started = time.perf_counter()
    results = await asyncio.gather(sleeper(1), sleeper(2))
    elapsed = time.perf_counter() - started
    print(results)
    print(elapsed < 0.16)
    return 0

print(asyncio.run(main()))
";
    let (code, stdout, stderr, wall) = run_source_timed("concurrent", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    // Two 80 ms sleeps overlap: a sequential run would need ~160 ms. The
    // in-program measurement reports whether the scheduler actually
    // overlapped them, and the process stays far below the bound below.
    assert_eq!(stdout, "[1, 2]\nTrue\n0\n");
    assert!(
        wall < Duration::from_millis(1500),
        "gather of two 80 ms sleeps took {wall:?} of wall time"
    );
}

#[test]
fn nested_and_indirect_awaits_return_the_final_value() {
    let source = "\
import asyncio

async def leaf():
    return 21

async def doubled():
    value = await leaf()
    return value * 2

async def main():
    direct = await doubled()
    alias = doubled
    indirect = await alias()
    return direct + indirect

print(asyncio.run(main()))
";
    let (code, stdout, stderr) = run_source("nested", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "84\n");
}

#[test]
fn gather_propagates_the_first_failure() {
    let source = "\
import asyncio

async def ok():
    await asyncio.sleep(0.02)
    return 1

async def boom():
    await asyncio.sleep(0.01)
    raise ValueError(\"gather boom\")

async def main():
    return await asyncio.gather(ok(), boom())

print(asyncio.run(main()))
";
    let (code, stdout, stderr) = run_source("gather_failure", source);
    assert_eq!(code, 1, "a failing gathered task must fail the process");
    assert!(
        stdout.is_empty(),
        "the print after the failing gather must not run, got:\n{stdout}"
    );
    assert!(
        stderr.contains("Traceback (most recent call last):"),
        "missing traceback, got:\n{stderr}"
    );
    assert!(
        stderr.contains("ValueError: gather boom"),
        "missing gather failure message, got:\n{stderr}"
    );
}

#[test]
fn await_outside_an_async_function_is_a_compile_error() {
    let source = "\
async def f():
    return 1

print(await f())
";
    let (code, stdout, stderr) = run_source("await_outside", source);
    assert_eq!(code, 1, "top-level await must be rejected with exit 1");
    assert!(stdout.is_empty(), "no program output on a compile error");
    assert!(
        stderr.contains("await") && stderr.contains("async"),
        "diagnostic must mention async, got:\n{stderr}"
    );
}

#[test]
fn send_passes_values_into_yield_expressions() {
    let source = "\
def echo():
    x = yield 1
    yield x

g = echo()
print(next(g))
print(g.send(5))
try:
    next(g)
except StopIteration:
    print(\"done\")
try:
    g.send(6)
except StopIteration:
    print(\"send-after-done\")
";
    let (code, stdout, stderr) = run_source("send", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "1\n5\ndone\nsend-after-done\n");
}

#[test]
fn send_non_none_to_a_just_started_generator_is_a_type_error() {
    let source = "\
def gen():
    x = yield 1
    yield x

g = gen()
try:
    g.send(5)
except TypeError as e:
    print(e)
print(next(g))
";
    let (code, stdout, stderr) = run_source("send_unstarted", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(
        stdout,
        "can't send non-None value to a just-started generator\n1\n"
    );
}

#[test]
fn close_runs_finally_blocks() {
    let source = "\
def gen():
    try:
        yield 1
        yield 2
    finally:
        print(\"cleanup\")

g = gen()
print(next(g))
print(g.close())
print(list(g))
";
    let (code, stdout, stderr) = run_source("close_finally", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "1\ncleanup\nNone\n[]\n");
}

#[test]
fn close_before_the_first_resume_does_not_run_the_body() {
    let source = "\
def gen():
    print(\"body\")
    yield 1

g = gen()
print(g.close())
print(list(g))
";
    let (code, stdout, stderr) = run_source("close_unstarted", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "None\n[]\n");
}

#[test]
fn yielding_during_close_raises_runtime_error() {
    let source = "\
def gen():
    try:
        yield 1
    finally:
        yield 2

g = gen()
print(next(g))
try:
    g.close()
except RuntimeError as e:
    print(e)
";
    let (code, stdout, stderr) = run_source("close_yield", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "1\ngenerator ignored GeneratorExit\n");
}

#[test]
fn throw_delivers_a_caught_exception_inside_the_generator() {
    let source = "\
def gen():
    try:
        yield 1
    except ValueError as e:
        print(\"caught\", e)
        yield 2

g = gen()
print(next(g))

try:
    raise ValueError(\"boom\")
except ValueError as e:
    print(g.throw(e))
";
    let (code, stdout, stderr) = run_source("throw", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "1\ncaught boom\n2\n");
}

#[test]
fn throw_rejects_non_exception_values() {
    let source = "\
def gen():
    yield 1

g = gen()
print(next(g))
try:
    g.throw(5)
except TypeError as e:
    print(e)
try:
    next(g)
except StopIteration:
    print(\"exhausted\")
";
    let (code, stdout, stderr) = run_source("throw_reject", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(
        stdout,
        "1\nthrow() argument must be an exception instance\nexhausted\n"
    );
}
