//! Differential tests: `pyx run` versus CPython for the argparse subset.
//!
//! Every program pins `prog="demo"` so the usage line does not depend on the
//! temporary file name. The tests are skipped (with a note) when no `python3`
//! is available.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(name: &str) -> TestDir {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pyx-argparse-{}-{name}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TestDir { path }
    }

    fn write(&self, source: &str) -> PathBuf {
        let path = self.path.join("program.py");
        std::fs::write(&path, source).expect("write program");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn parts(output: Output) -> (i32, String, String) {
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Runs `pyx run <program> <args>`.
fn run_pyx(program: &Path, args: &[&str]) -> Option<(i32, String, String)> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pyx"));
    command.arg("run").arg(program).args(args);
    match command.output() {
        Ok(output) => Some(parts(output)),
        Err(error) => panic!("failed to spawn pyx: {error}"),
    }
}

/// Runs the same program under CPython; `None` means no interpreter.
fn run_python(program: &Path, args: &[&str]) -> Option<(i32, String, String)> {
    let output = Command::new("python3").arg(program).args(args).output();
    match output {
        Ok(output) => Some(parts(output)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipping argparse differential test: `python3` is not available");
            None
        }
        Err(error) => panic!("failed to spawn python3: {error}"),
    }
}

/// Runs the program under both engines and asserts identical results.
fn diff(name: &str, source: &str, args: &[&str]) {
    let dir = TestDir::new(name);
    let program = dir.write(source);
    let Some(expected) = run_python(&program, args) else {
        return;
    };
    let actual = run_pyx(&program, args).expect("pyx output");
    assert_eq!(
        actual, expected,
        "pyx differs from CPython for `{name}` with args {args:?}: pyx={actual:?} cpython={expected:?}"
    );
}

const HELP_PROGRAM: &str = r#"import argparse

parser = argparse.ArgumentParser(prog="demo", description="Demo description.")
parser.add_argument("input", help="input file")
parser.add_argument("--n", type=int, default=1, help="count")
parser.add_argument("-v", "--verbose", action="store_true", help="be loud")
parser.add_argument("--mode", choices=["a", "b"], default="a", help="mode")
parser.parse_args()
print("ran")
"#;

const VERSION_PROGRAM: &str = r#"import argparse

parser = argparse.ArgumentParser(prog="demo")
parser.add_argument("--version", action="version", version="%(prog)s 2.0")
parser.parse_args()
print("ran")
"#;

const ACTIONS_PROGRAM: &str = r#"import argparse

parser = argparse.ArgumentParser(prog="demo")
parser.add_argument("--name", default="anon")
parser.add_argument("--verbose", "-v", action="store_true")
parser.add_argument("--tag", action="append")
parser.add_argument("--mode", choices=["a", "b"], default="a")
parser.add_argument("--pair", nargs=2)
parser.add_argument("--opt", nargs="?", const="C", default="D")
parser.add_argument("--flag", action=argparse.BooleanOptionalAction)
parser.add_argument("files", nargs="*")
args = parser.parse_args()
print(args.name)
print(args.verbose)
print(args.tag)
print(args.mode)
print(args.pair)
print(args.opt)
print(args.flag)
print(args.files)
"#;

const SUBPARSERS_PROGRAM: &str = r#"import argparse

parser = argparse.ArgumentParser(prog="demo")
subparsers = parser.add_subparsers(dest="cmd", required=True)
alpha = subparsers.add_parser("alpha")
alpha.add_argument("--x", type=int, default=0)
beta = subparsers.add_parser("beta")
beta.add_argument("value")
args = parser.parse_args()
print(args.cmd)
if args.cmd == "alpha":
    print(args.x)
else:
    print(args.value)
"#;

const KNOWN_ARGS_PROGRAM: &str = r#"import argparse

parser = argparse.ArgumentParser(prog="demo")
parser.add_argument("--n")
namespace, extras = parser.parse_known_args()
print(namespace.n)
print(extras)
"#;

const REQUIRED_PROGRAM: &str = r#"import argparse

parser = argparse.ArgumentParser(prog="demo")
parser.add_argument("--n", required=True)
parser.parse_args()
"#;

const UNKNOWN_PROGRAM: &str = r#"import argparse

parser = argparse.ArgumentParser(prog="demo")
parser.add_argument("--n")
parser.parse_args()
"#;

const CHOICES_PROGRAM: &str = r#"import argparse

parser = argparse.ArgumentParser(prog="demo")
parser.add_argument("--mode", choices=["a", "b"])
parser.parse_args()
"#;

const MUTEX_PROGRAM: &str = r#"import argparse

parser = argparse.ArgumentParser(prog="demo")
parser.add_argument("--foo")
parser.add_argument("--bar")
group = parser.add_mutually_exclusive_group()
group.add_argument("--alpha")
group.add_argument("--beta")
parser.parse_args()
"#;

#[test]
fn help_matches_cpython() {
    diff("help", HELP_PROGRAM, &["--help"]);
    diff("help-short", HELP_PROGRAM, &["-h"]);
}

#[test]
fn version_matches_cpython() {
    diff("version", VERSION_PROGRAM, &["--version"]);
}

#[test]
fn action_values_match_cpython() {
    diff(
        "actions",
        ACTIONS_PROGRAM,
        &[
            "--name=x",
            "-v",
            "--tag",
            "one",
            "--tag=two",
            "--mode",
            "b",
            "--pair",
            "1",
            "2",
            "--no-flag",
            "f1",
            "f2",
        ],
    );
    diff("actions-defaults", ACTIONS_PROGRAM, &["--flag", "file.txt"]);
}

#[test]
fn subparsers_match_cpython() {
    diff(
        "subparsers-alpha",
        SUBPARSERS_PROGRAM,
        &["alpha", "--x", "5"],
    );
    diff("subparsers-beta", SUBPARSERS_PROGRAM, &["beta", "hello"]);
}

#[test]
fn parse_known_args_matches_cpython() {
    diff(
        "known-args",
        KNOWN_ARGS_PROGRAM,
        &["--n", "1", "--extra", "tail"],
    );
}

#[test]
fn error_paths_match_cpython() {
    diff("required", REQUIRED_PROGRAM, &[]);
    diff("required-help", REQUIRED_PROGRAM, &["--help"]);
    diff("unknown", UNKNOWN_PROGRAM, &["--bad"]);
    diff("choices", CHOICES_PROGRAM, &["--mode", "c"]);
    diff("mutex", MUTEX_PROGRAM, &["--alpha", "1", "--beta", "2"]);
}

#[test]
fn exit_codes_match_cpython() {
    diff("exit-int", "import sys\nsys.exit(3)\n", &[]);
    diff("exit-message", "import sys\nsys.exit('boom')\n", &[]);
    diff("exit-zero", "import sys\nsys.exit(0)\n", &[]);
    diff("exit-none", "import sys\nsys.exit()\n", &[]);
}

#[test]
fn caught_system_exit_matches_cpython() {
    let source = r#"import sys

try:
    sys.exit(4)
except SystemExit as exc:
    print("caught", exc)
print("after")
try:
    raise SystemExit("bye")
except SystemExit as exc:
    print("caught", exc)
"#;
    diff("caught", source, &[]);
}

#[test]
fn namespace_import_compiles_and_constructs() {
    let source = r#"from argparse import Namespace

namespace = Namespace(answer=42)
print(namespace.answer)
"#;
    diff("namespace", source, &[]);
}
