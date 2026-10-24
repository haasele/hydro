//! End-to-end tests for the first slice of the standard-library program
//! (`docs/TODO.md`, `P7`): every module in Batch A imported and exercised
//! through `pyx run`.
//!
//! The program covers `bisect`/`_bisect`, `heapq`/`_heapq`, `colorsys`,
//! `keyword`, `stat`/`_stat` and the `_operator`, `_functools`, `_io`
//! aliases. It depends only on the native implementations, never on a CPython
//! source tree.

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
            "pyx-stdlib-batch-a-{}-{test_name}-{unique}",
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

/// The Batch A program. `__DATA_FILE__` is replaced with a fresh temp path so
/// the `_io` round trip never touches a shared file.
const PROGRAM: &str = r#"import bisect
import _bisect
import heapq
import _heapq
import colorsys
import keyword
import stat
import _stat
import _operator
import _functools
import _io


def close(left, right):
    return abs(left - right) < 1e-9


def magnitude(value):
    return abs(value)


values = [1, 3, 5, 7]
bisect.insort(values, 4)
print(values)
print(bisect.bisect_left(values, 4), bisect.bisect_right(values, 4), bisect.bisect(values, 4))

by_size = [1, 3, -5, 9]
bisect.insort_right(by_size, -3, key=magnitude)
print(by_size)
bisect.insort_left(by_size, -4, key=magnitude)
print(by_size)
print(bisect.bisect_left(by_size, 3, key=magnitude), bisect.bisect_right(by_size, 3, key=magnitude))
print(_bisect.bisect_right([1, 2, 3], 2))
print(_bisect.bisect_left([1, 2, 2, 3], 2))

heap = []
heapq.heappush(heap, 5)
heapq.heappush(heap, 1)
heapq.heappush(heap, 3)
print(heap[0])
print(heapq.heappop(heap))
heapq.heapify(heap)
print(heap[0])
print(heapq.heappushpop(heap, 2))
print(heapq.heapreplace(heap, 10))
print(heap[0], len(heap))
print(heapq.nlargest(2, [5, 1, 4, 2, 3]))
print(heapq.nsmallest(3, [5, 1, 4, 2, 3]))
print(heapq.nlargest(2, [-5, 1, 4], key=magnitude))

other = [2, 4]
_heapq.heapify(other)
_heapq.heappush(other, 1)
print(_heapq.heappop(other), len(other))

hsv = colorsys.rgb_to_hsv(0.0, 1.0, 0.0)
print(close(hsv[0], 1.0 / 3.0) and close(hsv[1], 1.0) and close(hsv[2], 1.0))
rgb = colorsys.hsv_to_rgb(1.0 / 3.0, 1.0, 1.0)
print(close(rgb[0], 0.0) and close(rgb[1], 1.0) and close(rgb[2], 0.0))
hls = colorsys.rgb_to_hls(0.2, 0.4, 0.6)
back = colorsys.hls_to_rgb(hls[0], hls[1], hls[2])
print(close(back[0], 0.2) and close(back[1], 0.4) and close(back[2], 0.6))
yiq = colorsys.rgb_to_yiq(0.2, 0.4, 0.6)
back = colorsys.yiq_to_rgb(yiq[0], yiq[1], yiq[2])
print(close(back[0], 0.2) and close(back[1], 0.4) and close(back[2], 0.6))

names = keyword.kwlist
soft = keyword.softkwlist
print(len(names) > 0)
print("True" in names)
print("match" in soft)
print(keyword.iskeyword("True"))
print(keyword.iskeyword("TRUE"))
print(keyword.issoftkeyword("match"))
ordered = True
i = 0
while i + 1 < len(names):
    if names[i] > names[i + 1]:
        ordered = False
    i += 1
print(ordered)

print(stat.S_ISDIR(stat.S_IFDIR))
print(stat.S_ISREG(stat.S_IFDIR))
print(stat.filemode(0o40755))
print(stat.S_IMODE(0o40755))
print(_stat.S_ISREG(_stat.S_IFREG))
print(_stat.S_ISLNK(_stat.S_IFLNK))

print(_operator.add(2, 3), _operator.mul(2, 3))


def add(left, right):
    return left + right


print(_functools.reduce(add, [1, 2, 3, 4]))

handle = _io.open(__DATA_FILE__, "w", encoding="utf-8")
handle.write("batch a\n")
handle.close()
handle = _io.open(__DATA_FILE__, "r", encoding="utf-8")
print(handle.read().strip())
handle.close()
"#;

/// Stdout of [`PROGRAM`] with a fresh data file.
const EXPECTED_STDOUT: &str = "[1, 3, 4, 5, 7]\n\
     2 3 3\n\
     [1, 3, -3, -5, 9]\n\
     [1, 3, -3, -4, -5, 9]\n\
     1 3\n\
     2\n\
     1\n\
     1\n\
     1\n\
     3\n\
     2\n\
     3\n\
     5 2\n\
     [5, 4]\n\
     [1, 2, 3]\n\
     [-5, 4]\n\
     1 2\n\
     True\n\
     True\n\
     True\n\
     True\n\
     True\n\
     True\n\
     True\n\
     True\n\
     False\n\
     True\n\
     True\n\
     True\n\
     False\n\
     drwxr-xr-x\n\
     493\n\
     True\n\
     True\n\
     5 6\n\
     10\n\
     batch a\n";

#[test]
fn batch_a_modules_run_end_to_end() {
    let dir = TestDir::new("end-to-end");
    let data = dir.path.join("batch_a_data.txt");
    let source = PROGRAM.replace("__DATA_FILE__", &python_literal(&data));
    let program = dir.write("batch_a.py", &source);
    let (code, stdout, stderr) = run(&program);
    assert_eq!(code, 0, "batch A program must exit 0:\n{stderr}");
    assert_eq!(stdout, EXPECTED_STDOUT, "stderr:\n{stderr}");
    assert_eq!(
        std::fs::read_to_string(&data).expect("_io must write the data file"),
        "batch a\n",
        "unexpected data file contents"
    );
}

/// An out-of-range heap access must surface the native error instead of a
/// silent default.
const FAILING_PROGRAM: &str = r#"import heapq

heapq.heappop([])
"#;

#[test]
fn batch_a_modules_report_errors() {
    let dir = TestDir::new("error");
    let program = dir.write("bad_heap.py", FAILING_PROGRAM);
    let (code, _stdout, stderr) = run(&program);
    assert_ne!(
        code, 0,
        "popping an empty heap must exit non-zero; stderr:\n{stderr}"
    );
}
