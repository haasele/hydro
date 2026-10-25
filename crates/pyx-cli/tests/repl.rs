//! End-to-end tests for the persistent interpreter surfaces: `pyx repl`,
//! `pyx -c` and `pyx -i`.
//!
//! Every test feeds standard input through a pipe and closes it, so the
//! session always sees EOF. The child gets a bounded wait: a session that
//! never exits fails the test instead of hanging the suite.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(test_name: &str) -> TestDir {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pyx-repl-{}-{test_name}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TestDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, source).expect("write file");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Runs `pyx` with `stdin` piped in, bounded to 60 seconds.
fn run(args: &[&str], stdin: &str, cwd: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pyx"));
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let mut child = command.spawn().expect("failed to spawn pyx");
    {
        let mut pipe = child.stdin.take().expect("child stdin");
        pipe.write_all(stdin.as_bytes()).expect("write stdin");
        // Dropping the pipe closes it, so the session reads EOF.
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if child.try_wait().expect("wait for pyx").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("pyx {args:?} did not exit within 60 seconds");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().expect("collect output")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "pyx failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        stdout(output),
        stderr(output)
    );
}

#[test]
fn repl_echoes_the_trailing_expression() {
    let output = run(&["repl"], "1 + 1\n", None);
    assert_success(&output);
    assert_eq!(stdout(&output), "2\n");
    // Prompts and the banner are only printed for a terminal stdin.
    assert!(
        !stderr(&output).contains(">>>"),
        "unexpected prompt on stderr: {}",
        stderr(&output)
    );
}

#[test]
fn repl_does_not_echo_assignments() {
    let output = run(&["repl"], "x = 41\nx\n", None);
    assert_success(&output);
    assert_eq!(stdout(&output), "41\n");
}

#[test]
fn repl_stores_echoed_values_in_underscore() {
    let output = run(&["repl"], "2 * 3\n_\n", None);
    assert_success(&output);
    assert_eq!(stdout(&output), "6\n6\n");
}

#[test]
fn repl_keeps_state_across_inputs() {
    let output = run(&["repl"], "x = 1\nx += 1\nx\n", None);
    assert_success(&output);
    assert_eq!(stdout(&output), "2\n");
}

#[test]
fn repl_buffers_an_indented_block_until_a_blank_line() {
    let source = "def add(a, b):\n    total = a + b\n    return total\n\nadd(2, 3)\n";
    let output = run(&["repl"], source, None);
    assert_success(&output);
    assert_eq!(stdout(&output), "5\n");
}

#[test]
fn repl_reports_an_exception_and_continues() {
    let output = run(&["repl"], "1 / 0\n1 + 1\n", None);
    assert_success(&output);
    assert_eq!(stdout(&output), "2\n");
    assert!(
        stderr(&output).contains("ZeroDivisionError"),
        "missing traceback on stderr: {}",
        stderr(&output)
    );
}

#[test]
fn repl_keeps_import_bindings_usable() {
    let output = run(&["repl"], "import math\nmath.sqrt(9)\n", None);
    assert_success(&output);
    assert_eq!(stdout(&output), "3.0\n");
}

#[test]
fn repl_initializes_an_imported_module_once() {
    let dir = TestDir::new("imports");
    dir.write("mod.py", "print(\"init\")\n\n\ndef f():\n    return 7\n");
    let input = "from mod import f\nf()\nfrom mod import f\nf()\n";
    let output = run(&["repl"], input, Some(dir.path()));
    assert_success(&output);
    assert_eq!(stdout(&output), "init\n7\n7\n");
}

#[test]
fn repl_no_echo_suppresses_expression_values() {
    let output = run(&["repl", "--no-echo"], "1 + 1\nprint(3)\n", None);
    assert_success(&output);
    assert_eq!(stdout(&output), "3\n");
}

#[test]
fn command_flag_runs_code_and_sets_argv_zero() {
    let output = run(
        &[
            "-c",
            "import sys\nprint(sys.argv[0])\nprint(sys.argv[1])",
            "hello",
        ],
        "",
        None,
    );
    assert_success(&output);
    assert_eq!(stdout(&output), "-c\nhello\n");
}

#[test]
fn command_flag_propagates_an_uncaught_exception() {
    let output = run(&["-c", "raise ValueError('boom')"], "", None);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("ValueError"),
        "missing traceback on stderr: {}",
        stderr(&output)
    );
}

#[test]
fn interactive_flag_runs_a_file_then_the_session() {
    let dir = TestDir::new("interactive-file");
    let script = dir.write("script.py", "x = 20\nprint(x + 1)\n");
    let output = run(
        &["-i", script.to_str().expect("UTF-8 path")],
        "x + 1\nprint(x)\n",
        Some(dir.path()),
    );
    assert_success(&output);
    assert_eq!(stdout(&output), "21\n21\n20\n");
}

#[test]
fn interactive_flag_runs_command_code_then_the_session() {
    let output = run(&["-i", "-c", "x = 5"], "x * 2\n", None);
    assert_success(&output);
    assert_eq!(stdout(&output), "10\n");
}

#[test]
fn repl_reports_a_syntax_error_immediately() {
    // An unmatched closing bracket is a genuine error, not an incomplete
    // fragment, so it is reported without waiting for more input.
    let output = run(&["repl"], "x = )\n1 + 1\n", None);
    assert_success(&output);
    assert!(
        stderr(&output).contains("error"),
        "missing syntax error on stderr: {}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "2\n");
}
