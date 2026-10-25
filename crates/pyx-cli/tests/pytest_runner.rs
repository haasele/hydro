//! End-to-end tests for `pyx pytest` and the hidden `pyx __pytest` child.
//!
//! Every test builds a temporary tree, runs the real binary and asserts on its
//! report and exit code. The same files also run under CPython's pytest when
//! it is installed; the assertions here describe PyX's own deterministic
//! report instead of depending on pytest's exact formatting.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(test_name: &str) -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pyx-pytest-{}-{test_name}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("failed to create test directory");
        Self { path }
    }

    fn write(&self, name: &str, source: &str) {
        fs::write(self.path.join(name), source).expect("failed to write test file");
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn run(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_pyx"))
        .args(args)
        .output()
        .expect("failed to spawn pyx");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn run_dir(dir: &TestDir, extra: &[&str]) -> (i32, String, String) {
    let path = dir.path().to_string_lossy().into_owned();
    let mut args = vec!["pytest", path.as_str()];
    args.extend_from_slice(extra);
    run(&args)
}

#[test]
fn plain_pass_and_fail_reports_source_assert() {
    let dir = TestDir::new("plain");
    dir.write(
        "test_plain.py",
        "def test_pass():\n    assert 1 + 1 == 2\n\ndef test_fail():\n    x = 1\n    assert x == 2\n",
    );
    let (code, stdout, stderr) = run_dir(&dir, &[]);
    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("1 passed"), "stdout: {stdout}");
    assert!(stdout.contains("1 failed"), "stdout: {stdout}");
    assert!(
        stdout.contains("assert x == 2"),
        "source text missing from failure: {stdout}"
    );
}

#[test]
fn raises_with_match_and_approx_compare() {
    let dir = TestDir::new("raises");
    dir.write(
        "test_raises.py",
        "import pytest\n\ndef test_raises():\n    with pytest.raises(ValueError, match=\"bad\"):\n        raise ValueError(\"bad value\")\n\ndef test_approx():\n    assert 0.1 + 0.2 == pytest.approx(0.3)\n\ndef test_approx_lists():\n    assert pytest.approx([0.3]) == [0.1 + 0.2]\n",
    );
    let (code, stdout, stderr) = run_dir(&dir, &[]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("3 passed"), "stdout: {stdout}");
}

#[test]
fn skip_xfail_and_parametrize_are_reported() {
    let dir = TestDir::new("marks");
    dir.write(
        "test_marks.py",
        "import pytest\n\n@pytest.mark.parametrize(\"x\", [1, 2, 3])\ndef test_square(x):\n    assert x * x > 0\n\n@pytest.mark.skip(reason=\"not ready\")\ndef test_skipped():\n    assert False\n\n@pytest.mark.xfail\ndef test_expected_failure():\n    assert False\n",
    );
    let (code, stdout, stderr) = run_dir(&dir, &[]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("3 passed"), "stdout: {stdout}");
    assert!(stdout.contains("1 skipped"), "stdout: {stdout}");
    assert!(stdout.contains("1 xfailed"), "stdout: {stdout}");
}

#[test]
fn class_test_methods_are_collected() {
    let dir = TestDir::new("class");
    dir.write(
        "test_class.py",
        "class TestGroup:\n    def test_method(self):\n        assert 1 == 1\n\n    def test_second(self):\n        assert True\n",
    );
    let (code, stdout, stderr) = run_dir(&dir, &[]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("2 passed"), "stdout: {stdout}");
}

#[test]
fn fixtures_conftest_and_yield_teardown() {
    let dir = TestDir::new("fixtures");
    dir.write(
        "conftest.py",
        "import pytest\n\n@pytest.fixture\ndef base():\n    return 41\n",
    );
    dir.write(
        "test_fixtures.py",
        "import pytest\n\n@pytest.fixture\ndef bump():\n    print(\"setup\")\n    yield 1\n    print(\"teardown\")\n\n@pytest.fixture(scope=\"module\")\ndef shared():\n    print(\"module-setup\")\n    yield 5\n    print(\"module-teardown\")\n\ndef test_uses(base, bump):\n    assert base + bump == 42\n\ndef test_shared_a(shared):\n    assert shared == 5\n\ndef test_shared_b(shared):\n    assert shared == 5\n",
    );
    let (code, stdout, stderr) = run_dir(&dir, &[]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("3 passed"), "stdout: {stdout}");
    assert!(
        stdout.contains("teardown"),
        "teardown did not run: {stdout}"
    );
    assert_eq!(
        stdout.matches("module-setup").count(),
        1,
        "module fixture ran more than once: {stdout}"
    );
    assert!(
        stdout.contains("module-teardown"),
        "module teardown did not run: {stdout}"
    );
}

#[test]
fn no_tests_exits_five() {
    let dir = TestDir::new("empty");
    let (code, stdout, stderr) = run_dir(&dir, &[]);
    assert_eq!(code, 5, "stdout: {stdout}\nstderr: {stderr}");
}

#[test]
fn json_shape_and_aggregate_counts() {
    let dir = TestDir::new("json");
    dir.write(
        "test_json.py",
        "import pytest\n\ndef test_pass():\n    assert True\n\n@pytest.mark.skip\ndef test_skip():\n    assert False\n\ndef test_fail():\n    assert False\n",
    );
    let (code, stdout, stderr) = run_dir(&dir, &["--json"]);
    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("\"exit_code\":1"), "stdout: {stdout}");
    assert!(stdout.contains("\"passed\":1"), "stdout: {stdout}");
    assert!(stdout.contains("\"failed\":1"), "stdout: {stdout}");
    assert!(stdout.contains("\"skipped\":1"), "stdout: {stdout}");
    assert!(stdout.contains("\"outcome\":\"pass\""), "stdout: {stdout}");
}

#[test]
fn one_broken_file_does_not_abort_the_run() {
    let dir = TestDir::new("isolation");
    dir.write("test_good.py", "def test_ok():\n    assert True\n");
    dir.write("test_bad.py", "def test_(:\n");
    let (code, stdout, stderr) = run_dir(&dir, &[]);
    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("1 passed"), "stdout: {stdout}");
}

#[test]
fn keyword_filter_and_exit_first() {
    let dir = TestDir::new("filter");
    dir.write(
        "test_filter.py",
        "def test_yes():\n    assert True\n\ndef test_no():\n    assert False\n",
    );
    let (code, stdout, stderr) = run_dir(&dir, &["-k", "yes"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("1 passed"), "stdout: {stdout}");
    assert!(!stdout.contains("test_no"), "stdout: {stdout}");

    let (code, stdout, _) = run_dir(&dir, &["-x"]);
    assert_eq!(code, 1, "stdout: {stdout}");
}

#[test]
fn pyx_test_remains_backward_compatible() {
    let dir = TestDir::new("compat");
    dir.write("test_plain.py", "print(\"ran\")\n");
    let path = dir.path().to_string_lossy().into_owned();
    let (code, stdout, stderr) = run(&["test", path.as_str()]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("PASS"), "stdout: {stdout}");
    assert!(stdout.contains("ran"), "stdout: {stdout}");
}
