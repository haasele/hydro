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

#[test]
fn yield_from_delegates_a_plain_iterable() {
    let source = "\
def gen():
    yield from [1, 2]
    yield 3

print(list(gen()))
";
    let (code, stdout, stderr) = run_source("yield_from_list", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "[1, 2, 3]\n");
}

#[test]
fn yield_from_evaluates_to_the_delegate_return_value() {
    let source = "\
def sub():
    yield 1
    return 42

def outer():
    x = yield from sub()
    print(\"sub returned\", x)
    yield x

g = outer()
print(next(g))
print(next(g))
";
    let (code, stdout, stderr) = run_source("yield_from_result", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "1\nsub returned 42\n42\n");
}

#[test]
fn yield_from_forwards_send_values() {
    let source = "\
def sub():
    x = yield 1
    yield x * 2

def outer():
    yield from sub()

g = outer()
print(next(g))
print(g.send(5))
try:
    next(g)
except StopIteration:
    print(\"done\")
";
    let (code, stdout, stderr) = run_source("yield_from_send", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "1\n10\ndone\n");
}

#[test]
fn close_closes_the_delegated_generator_first() {
    let source = "\
def sub():
    try:
        yield 1
        yield 2
    finally:
        print(\"sub cleanup\")

def outer():
    try:
        yield from sub()
    finally:
        print(\"outer cleanup\")

g = outer()
print(next(g))
print(g.close())
";
    let (code, stdout, stderr) = run_source("yield_from_close", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "1\nsub cleanup\nouter cleanup\nNone\n");
}

#[test]
fn throw_is_forwarded_into_the_delegated_generator() {
    let source = "\
def sub():
    try:
        yield 1
    except ValueError as e:
        print(\"sub caught\", e)
        yield 2

def outer():
    yield from sub()

g = outer()
print(next(g))

try:
    raise ValueError(\"boom\")
except ValueError as e:
    print(g.throw(e))
";
    let (code, stdout, stderr) = run_source("yield_from_throw", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "1\nsub caught boom\n2\n");
}

#[test]
fn send_to_a_plain_iterator_delegate_is_an_attribute_error() {
    let source = "\
def gen():
    yield from [1, 2]

g = gen()
print(next(g))
try:
    g.send(5)
except AttributeError as e:
    print(e)
";
    let (code, stdout, stderr) = run_source("yield_from_send_attr", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "1\n'iterator' object has no attribute 'send'\n");
}

#[test]
fn delegating_to_the_running_generator_is_a_runtime_error() {
    let source = "\
def gen(holder):
    yield from holder[0]

holder = []
g = gen(holder)
holder.append(g)
try:
    next(g)
except RuntimeError as e:
    print(e)
";
    let (code, stdout, stderr) = run_source("yield_from_self", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "generator already executing\n");
}

#[test]
fn a_delegate_that_yields_during_close_raises_runtime_error() {
    let source = "\
def sub():
    try:
        yield 1
    finally:
        yield 2

def outer():
    yield from sub()

g = outer()
print(next(g))
try:
    g.close()
except RuntimeError as e:
    print(e)
";
    let (code, stdout, stderr) = run_source("yield_from_close_yield", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "1\ngenerator ignored GeneratorExit\n");
}

// -------------------------------------------------------------------------
// Lazy generator expressions
// -------------------------------------------------------------------------

/// True when the AOT runtime archive was built next to the test binary.
fn runtime_lib_present() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    exe.ancestors()
        .any(|dir| dir.join("libpyx_runtime.a").is_file())
}

/// `(exit code, stdout, stderr)` of a finished child process.
fn output_parts(output: Output) -> (i32, String, String) {
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn run_executable(path: &Path) -> (i32, String, String) {
    let output = Command::new(path)
        .output()
        .expect("failed to spawn built executable");
    output_parts(output)
}

#[test]
fn generator_expression_runs_only_as_items_are_requested() {
    let source = "\
class Counter:
    def __init__(self):
        self.calls = 0
    def tick(self, value):
        self.calls += 1
        return value * 2

counter = Counter()
gen = (counter.tick(v) for v in [1, 2, 3])
print(counter.calls)
print(next(gen))
print(counter.calls)
print(next(gen))
print(counter.calls)
print(list(gen))
print(counter.calls)
";
    let (code, stdout, stderr) = run_source("genexp_lazy", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "0\n2\n1\n4\n2\n[6]\n3\n");
}

#[test]
fn generator_expression_is_single_pass_and_exhausts() {
    let source = "\
gen = (v for v in [1, 2])
print(list(gen))
print(list(gen))
total = 0
for v in gen:
    total += v
print(total)
for v in gen:
    total += v
print(total)
try:
    next(gen)
except StopIteration:
    print('stop')
";
    let (code, stdout, stderr) = run_source("genexp_single_pass", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "[1, 2]\n[]\n0\n0\nstop\n");
}

#[test]
fn generator_expression_binds_tuple_targets_and_sees_later_mutations() {
    let source = "\
def rows(data, offset):
    gen = ((i + offset, j * offset) for i, j in data if i < j if j > 2)
    return list(gen)

def pairs():
    return list((i, j) for i in range(3) for j in range(i) if j % 2 == 0)

def total():
    return sum(x for x in range(4))

data = [(1, 3), (2, 2), (1, 5)]
print(rows(data, 10))
print(pairs())
print(total())

values = [1, 2]
grow = (v for v in values)
values.append(3)
print(list(grow))
";
    let (code, stdout, stderr) = run_source("genexp_clauses", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(
        stdout,
        "[(11, 30), (11, 50)]\n[(1, 0), (2, 0)]\n6\n[1, 2, 3]\n"
    );
}

#[test]
fn generator_expression_resumes_after_partial_consumption() {
    let source = "\
def scan():
    gen = (v * v for v in range(10))
    head = next(gen)
    total = 0
    for v in gen:
        if v > 20:
            break
        total += v
    return head, total

print(scan())
print(sum(v for v in range(5)))
print(list(v for v in range(3)))

rest = (v for v in range(5))
for v in rest:
    if v == 1:
        break
print(next(rest))
";
    let (code, stdout, stderr) = run_source("genexp_partial", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "(0, 30)\n10\n[0, 1, 2]\n2\n");
}

#[test]
fn generator_expression_propagates_exceptions_on_resume() {
    let source = "\
def bad(v):
    if v == 2:
        raise ValueError('boom from genexp')
    return v

gen = (bad(v) for v in [1, 2, 3])
print(next(gen))
try:
    for v in gen:
        print(v)
except ValueError:
    print('caught ValueError')
";
    let (code, stdout, stderr) = run_source("genexp_exceptions", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout, "1\ncaught ValueError\n");
}

#[test]
fn generator_expression_rejects_a_foreign_walrus_target() {
    let source = "gen = ((y := v) for v in [1, 2])\n";
    let (code, _, stderr) = run_source("genexp_walrus", source);
    assert_eq!(code, 1, "stderr:\n{stderr}");
    assert!(
        stderr.contains("assignment expressions inside a generator expression"),
        "missing walrus diagnostic, got:\n{stderr}"
    );
}

#[test]
fn generator_expressions_run_under_aot() {
    if !runtime_lib_present() {
        eprintln!(
            "skipping generator_expressions_run_under_aot: libpyx_runtime.a not found next \
             to the test profile (AOT is covered by scripts/verify.sh)"
        );
        return;
    }
    let source = "\
class Counter:
    def __init__(self):
        self.calls = 0
    def tick(self, value):
        self.calls += 1
        return value

counter = Counter()
gen = (counter.tick(v) * 10 for v in [1, 2, 3] if v != 2)
print(next(gen))
print(list(gen))
print(counter.calls)
print(sum(v for v in range(4)))
";
    let program = write_source("genexp_aot", source);
    let binary = program.with_file_name("genexp_bin");
    let program_arg = program.to_string_lossy().into_owned();
    let binary_arg = binary.to_string_lossy().into_owned();
    let (code, stdout, stderr) = output_parts(run(&["build", &program_arg, "-o", &binary_arg]));
    assert_eq!(
        code, 0,
        "pyx build failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let (code, stdout, stderr) = run_executable(&binary);
    cleanup(&program);
    assert_eq!(code, 0, "AOT binary failed: {stderr}");
    assert_eq!(stdout, "10\n[30]\n2\n6\n");
}

// -------------------------------------------------------------------------
// Async generators, tasks and TaskGroup
// -------------------------------------------------------------------------

#[test]
fn builtin_exception_classes_construct_and_type_identity() {
    let source = "\
error = ValueError('made')
print(type(error) is ValueError)
print(type(OSError('x')).__name__)
print(str(error))

try:
    raise ValueError('caught')
except ValueError:
    print('caught ValueError')

async def athrown():
    try:
        yield 1
    except ValueError:
        yield 99

import asyncio

async def main():
    gen = athrown()
    print(await gen.__anext__())
    print(await gen.athrow(ValueError('thrown')))

asyncio.run(main())
";
    let (code, stdout, stderr) = run_source("exc_identity", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "True\nOSError\nmade\ncaught ValueError\n1\n99\n");
}

#[test]
fn async_generator_protocol_methods_work() {
    let source = "\
import asyncio

async def sent():
    received = yield 1
    yield received

async def closed():
    try:
        yield 1
    finally:
        print('closed')

async def handled():
    try:
        yield 1
    except ValueError:
        yield 99

async def main():
    g = sent()
    print(await g.__anext__())
    print(await g.asend(5))
    try:
        await g.__anext__()
    except StopAsyncIteration:
        print('done')

    c = closed()
    await c.__anext__()
    await c.aclose()

    h = handled()
    await h.__anext__()
    print(await h.athrow(ValueError('x')))

asyncio.run(main())
";
    let (code, stdout, stderr) = run_source("async_protocol", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "1\n5\ndone\nclosed\n99\n");
}

#[test]
fn task_group_overlaps_tasks_in_creation_order() {
    let source = "\
import asyncio
from asyncio import TaskGroup

seen = []

async def work(label, delay):
    seen.append(label)
    await asyncio.sleep(delay)
    seen.append(label + '2')
    return label

async def main():
    async with TaskGroup() as tg:
        tg.create_task(work('a', 0))
        tg.create_task(work('b', 0))
    return seen

print(asyncio.run(main()))
";
    let (code, stdout, stderr) = run_source("task_group_overlap", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "['a', 'b', 'a2', 'b2']\n");
}

#[test]
fn task_group_cancels_siblings_and_propagates_the_error() {
    let source = "\
import asyncio

seen = []

async def slow():
    try:
        await asyncio.sleep(5)
    finally:
        seen.append('cancelled')

async def boom():
    raise ValueError('boom')

async def main():
    try:
        async with asyncio.TaskGroup() as tg:
            tg.create_task(slow())
            tg.create_task(boom())
    except ValueError:
        return 'caught'
    return 'missed'

print(asyncio.run(main()))
print(seen)
";
    let (code, stdout, stderr) = run_source("task_group_error", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "caught\n['cancelled']\n");
}

#[test]
fn task_cancel_and_current_task_are_available() {
    let source = "\
import asyncio

async def child():
    return 1

async def main():
    task = asyncio.create_task(child())
    task.cancel()
    print(task.cancelled())
    print(asyncio.current_task() is not None)
    return 0

print(asyncio.run(main()))
";
    let (code, stdout, stderr) = run_source("task_cancel", source);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stderr.is_empty(), "unexpected stderr:\n{stderr}");
    assert_eq!(stdout, "True\nTrue\n0\n");
}

#[test]
fn reentrant_run_reports_a_runtime_error() {
    let source = "\
import asyncio

async def inner():
    return 1

async def main():
    return asyncio.run(inner())

asyncio.run(main())
";
    let (code, _, stderr) = run_source("reentrant_run", source);
    assert_eq!(code, 1, "stderr:\n{stderr}");
    assert!(
        stderr.contains("cannot be called from a running event loop"),
        "missing reentrant-run diagnostic, got:\n{stderr}"
    );
}

fn example_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
}

fn run_example(name: &str) -> (i32, String, String) {
    let path = example_file(name);
    let output = run(&["run", path.to_str().expect("UTF-8 example path")]);
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn expected_example(name: &str) -> String {
    std::fs::read_to_string(example_file(&format!("expected/{name}")))
        .expect("expected output file")
}

#[test]
fn async_examples_match_cpython_expected_output() {
    for name in ["generators_async.txt", "async_generators.txt"] {
        let source = name.replace(".txt", ".py");
        let (code, stdout, stderr) = run_example(&source);
        assert_eq!(code, 0, "{source} stderr:\n{stderr}");
        assert_eq!(stdout, expected_example(name), "{source} output");
    }
}

#[test]
fn async_examples_run_under_aot() {
    if !runtime_lib_present() {
        eprintln!("skipping async_examples_run_under_aot: libpyx_runtime.a not found");
        return;
    }
    for name in ["generators_async.txt", "async_generators.txt"] {
        let source = name.replace(".txt", ".py");
        let program = example_file(&source);
        let binary = std::env::temp_dir().join(format!(
            "pyx-async-aot-{}-{}",
            std::process::id(),
            source.replace('.', "_")
        ));
        let program_arg = program.to_string_lossy().into_owned();
        let binary_arg = binary.to_string_lossy().into_owned();
        let (code, stdout, stderr) = output_parts(run(&["build", &program_arg, "-o", &binary_arg]));
        assert_eq!(
            code, 0,
            "pyx build {source} failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        let (code, stdout, stderr) = run_executable(&binary);
        let _ = std::fs::remove_file(&binary);
        assert_eq!(code, 0, "{source} AOT failed: {stderr}");
        assert_eq!(stdout, expected_example(name), "{source} AOT output");
    }
}
