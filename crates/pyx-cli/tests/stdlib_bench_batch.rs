//! End-to-end tests for the benchmark-critical standard-library slice:
//! `subprocess`, `tempfile`, `shutil`, `textwrap` and `timeit`.
//!
//! The generated program exercises the exact APIs the benchmark scripts in
//! `benchmarks/` rely on, then a second program checks that a failing
//! `subprocess.run(..., check=True)` surfaces as a non-zero exit. A final
//! test starts `benchmarks/bench_pyx.py` under `pyx run` and checks its
//! argparse help.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// An isolated directory for a single test, removed on drop.
struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(test_name: &str) -> TestDir {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pyx-stdlib-bench-batch-{}-{test_name}-{unique}",
            std::process::id()
        ));
        // Remove stale state from an interrupted previous run.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TestDir { path }
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, source).expect("write program");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Renders `path` as a Python string literal for the generated program.
fn python_literal(path: &Path) -> String {
    let text = path.to_str().expect("UTF-8 temp path");
    let mut literal = String::with_capacity(text.len() + 2);
    literal.push('"');
    for ch in text.chars() {
        match ch {
            '\\' => literal.push_str("\\\\"),
            '"' => literal.push_str("\\\""),
            _ => literal.push(ch),
        }
    }
    literal.push('"');
    literal
}

fn run(path: &Path) -> (i32, String, String) {
    let output: Output = Command::new(env!("CARGO_BIN_EXE_pyx"))
        .args(["run", path.to_str().expect("UTF-8 program path")])
        .output()
        .expect("failed to spawn pyx");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The benchmark-batch program. `__BASE__` is replaced with a fresh temp
/// directory so `tempfile` never writes into shared state; the directory is
/// removed by [`TestDir`] even if the program fails midway.
const PROGRAM: &str = r#"import os
import shutil
import subprocess
import tempfile
import textwrap
import timeit

BASE = __BASE__

result = subprocess.run(
    ["/bin/sh", "-c", "printf hello"],
    capture_output=True,
    text=True,
)
print(result.stdout, result.returncode)
print(result.stdout == "hello", result.returncode == 0)

both = subprocess.run(
    ["/bin/sh", "-c", "printf one; printf two >&2"],
    capture_output=True,
    text=True,
)
print(both.stdout, both.stderr)

merged = subprocess.run(
    ["/bin/sh", "-c", "printf out; printf err >&2"],
    stdout=subprocess.PIPE,
    stderr=subprocess.STDOUT,
    text=True,
)
print(merged.stdout)

print(subprocess.PIPE, subprocess.STDOUT, subprocess.DEVNULL)
print(subprocess.run(["/bin/echo", "hidden"], stdout=subprocess.DEVNULL).returncode)

print(subprocess.check_output(["/bin/echo", "hi"], text=True).strip())
print(subprocess.check_output(["/bin/sh", "-c", "printf raw"]))

child = subprocess.Popen(["/bin/sh", "-c", "exit 0"])
print(child.wait())
print(child.returncode)
print(child.pid > 0)

failed = subprocess.run(["/bin/sh", "-c", "exit 3"])
print(failed.returncode)

print(len(tempfile.gettempdir()) > 0)
print(os.path.isdir(tempfile.gettempdir()))

with tempfile.TemporaryDirectory(prefix="pyx-bench-", dir=BASE) as tmp:
    note = os.path.join(tmp, "note.txt")
    handle = open(note, "w")
    handle.write("temp data")
    handle.close()
    handle = open(note, "r")
    print(handle.read())
    handle.close()
    print(os.path.isdir(tmp), tmp.find("pyx-bench-") >= 0)
print(os.path.exists(tmp))

scratch = tempfile.mkdtemp(prefix="pyx-bench-", dir=BASE)
print(os.path.isdir(scratch))

print(len(shutil.which("sh")) > 0)
print(shutil.which("pyx-no-such-command") == None)

src = os.path.join(scratch, "src.txt")
dst = os.path.join(scratch, "dst.txt")
handle = open(src, "w")
handle.write("copy me")
handle.close()
shutil.copyfile(src, dst)
handle = open(dst, "r")
print(handle.read())
handle.close()

print(shutil.get_terminal_size().columns > 0, shutil.get_terminal_size((120, 40)).columns > 0)

shutil.rmtree(scratch)
print(os.path.exists(scratch))

dedented = textwrap.dedent("    alpha\n      beta\n    gamma\n")
print(dedented == "alpha\n  beta\ngamma\n")
print(dedented)

print(textwrap.fill("one two three four five", 9))
print(textwrap.wrap("alpha beta gamma delta", 11))
print(textwrap.shorten("hello world this is a long sentence", 20))
print(textwrap.shorten("alpha beta gamma delta", 15, placeholder="..."))

def work():
    return sum(range(16))


timer = timeit.Timer(work)
print(timer.timeit(3) >= 0.0)
print(len(timer.repeat(repeat=2, number=2)) == 2)
print(len(timer.repeat(repeat=2, number=2)) == 2)
print(timeit.default_timer() > 0.0)
def one():
    return 1 + 1


number, seconds = timeit.Timer(one).autorange()
print(number > 0, seconds >= 0.0)
def two():
    return 2 * 2


print(len(timeit.repeat(two, number=2, repeat=3)) == 3)
"#;

/// Stdout of [`PROGRAM`], captured from CPython for the deterministic parts.
const EXPECTED_STDOUT: &str = concat!(
    "hello 0\n",
    "True True\n",
    "one two\n",
    "outerr\n",
    "-1 -2 -3\n",
    "0\n",
    "hi\n",
    "b'raw'\n",
    "0\n",
    "0\n",
    "True\n",
    "3\n",
    "True\n",
    "True\n",
    "temp data\n",
    "True True\n",
    "False\n",
    "True\n",
    "True\n",
    "True\n",
    "copy me\n",
    "True True\n",
    "False\n",
    "True\n",
    "alpha\n",
    "  beta\n",
    "gamma\n",
    "\n",
    "one two\n",
    "three\n",
    "four five\n",
    "['alpha beta', 'gamma delta']\n",
    "hello world [...]\n",
    "alpha beta...\n",
    "True\n",
    "True\n",
    "True\n",
    "True\n",
    "True True\n",
    "True\n",
);

#[test]
fn bench_batch_modules_run_end_to_end() {
    let dir = TestDir::new("end-to-end");
    let source = PROGRAM.replace("__BASE__", &python_literal(&dir.path));
    let program = dir.write("bench_batch.py", &source);
    let (code, stdout, stderr) = run(&program);
    assert_eq!(code, 0, "bench batch program must exit 0:\n{stderr}");
    assert_eq!(stdout, EXPECTED_STDOUT, "stderr:\n{stderr}");
}

/// A non-zero child plus `check=True` must raise instead of returning the
/// namespace, which makes the interpreter exit non-zero.
const FAILING_PROGRAM: &str = r#"import subprocess

result = subprocess.run(
    ["/bin/sh", "-c", "printf bad; exit 7"],
    capture_output=True,
    text=True,
    check=True,
)
print(result.returncode)
"#;

#[test]
fn bench_batch_subprocess_check_raises() {
    let dir = TestDir::new("check");
    let program = dir.write("bad_subprocess.py", FAILING_PROGRAM);
    let (code, stdout, stderr) = run(&program);
    assert_ne!(
        code, 0,
        "check=True with exit status 7 must exit non-zero; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// `pyx run` forwards arguments after the source file to `sys.argv`.
#[test]
fn run_forwards_program_arguments_to_sys_argv() {
    let dir = TestDir::new("argv");
    let source = "import sys\nprint(len(sys.argv))\nprint(sys.argv[1])\nprint(sys.argv[2])\n";
    let program = dir.write("argv_probe.py", source);

    let output: Output = Command::new(env!("CARGO_BIN_EXE_pyx"))
        .arg("run")
        .arg(&program)
        .arg("--flag")
        .arg("value")
        .output()
        .expect("failed to spawn pyx");
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        code, 0,
        "run with program arguments must exit 0:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(stdout, "3\n--flag\nvalue\n", "stderr:\n{stderr}");
}
