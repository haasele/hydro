//! End-to-end tests for `unittest`-style test modules run through `pyx run`.
//!
//! The programs use only the standard library and mirror the official CPython
//! `Lib/test/test_errno.py` style: a `unittest.TestCase` subclass, module
//! attribute assertions and a `unittest.main()` entry point.

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
            "pyx-unittest-{}-{test_name}-{unique}",
            std::process::id()
        ));
        // Remove stale state from an interrupted previous run.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TestDir { path }
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, source).expect("write test program");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn run(path: &Path) -> (i32, String, String) {
    let output: Output = Command::new(env!("CARGO_BIN_EXE_pyx"))
        .args(["run", path.to_str().expect("utf-8 program path")])
        .output()
        .expect("failed to spawn pyx");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

const PASSING_PROGRAM: &str = r#"import errno
import unittest
class T(unittest.TestCase):
    def test_errno(self):
        self.assertIn(errno.ERANGE, errno.errorcode)
        self.assertHasAttr(errno, "EDOM")
        for code in frozenset(["EDOM", "ERANGE"]):
            self.assertHasAttr(errno, code)
        self.assertEqual(getattr(errno, "EDOM"), 33)
if __name__ == "__main__":
    unittest.main()
"#;

#[test]
fn unittest_runner_passes() {
    let dir = TestDir::new("pass");
    let program = dir.write("test_errno.py", PASSING_PROGRAM);
    let (code, _stdout, stderr) = run(&program);
    assert_eq!(code, 0, "unittest.main() must exit 0 on success:\n{stderr}");
    assert!(
        stderr.is_empty(),
        "a passing run must not write to stderr:\n{stderr}"
    );
}

const FAILING_PROGRAM: &str = r#"import unittest
class T(unittest.TestCase):
    def test_failure(self):
        self.assertEqual(1, 2)
if __name__ == "__main__":
    unittest.main()
"#;

#[test]
fn unittest_runner_reports_failures() {
    let dir = TestDir::new("fail");
    let program = dir.write("test_failure.py", FAILING_PROGRAM);
    let (code, _stdout, _stderr) = run(&program);
    assert_ne!(code, 0, "a failing assertion must exit non-zero");
}
