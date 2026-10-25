//! Tests for `pyx completions` and the verbose `pyx --version` output.
//!
//! The binary is driven through `CARGO_BIN_EXE_pyx`, like the other CLI tests,
//! and only the standard library is used.

use std::process::{Command, Output};

/// Runs the `pyx` binary and returns `(exit code, stdout, stderr)`.
fn run(args: &[&str]) -> (i32, String, String) {
    let output: Output = Command::new(env!("CARGO_BIN_EXE_pyx"))
        .args(args)
        .output()
        .expect("failed to spawn pyx binary");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

const COMMANDS: &[&str] = &[
    "new",
    "init",
    "check",
    "run",
    "build",
    "test",
    "pytest",
    "fmt",
    "lint",
    "clean",
    "doctor",
    "gc",
    "repl",
    "add",
    "remove",
    "sync",
    "lock",
    "completions",
    "help",
];

const OPTIONS: &[&str] = &[
    "--json",
    "--color",
    "--no-color",
    "--verbose",
    "--quiet",
    "-o",
    "--output",
    "--runtime-lib",
    "--no-cache",
    "--force",
    "--check",
    "--dry-run",
    "--no-echo",
    "--no-banner",
];

fn assert_script_complete(shell: &str, script: &str) {
    for &command in COMMANDS {
        assert!(
            script.contains(command),
            "{shell} completion is missing command '{command}'"
        );
    }
    for &option in OPTIONS {
        assert!(
            script.contains(option),
            "{shell} completion is missing option '{option}'"
        );
    }
}

#[test]
fn completions_bash_script() {
    let (code, stdout, stderr) = run(&["completions", "bash"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("complete -F _pyx pyx"), "stdout: {stdout}");
    assert!(stdout.contains("compgen"), "stdout: {stdout}");
    assert_script_complete("bash", &stdout);
}

#[test]
fn completions_zsh_script() {
    let (code, stdout, stderr) = run(&["completions", "zsh"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.starts_with("#compdef pyx"), "stdout: {stdout}");
    assert!(stdout.contains("compdef _pyx pyx"), "stdout: {stdout}");
    assert_script_complete("zsh", &stdout);
}

#[test]
fn completions_fish_script() {
    let (code, stdout, stderr) = run(&["completions", "fish"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("complete -c pyx"), "stdout: {stdout}");
    assert_script_complete("fish", &stdout);
}

#[test]
fn completions_unknown_shell_is_a_usage_error() {
    let (code, stdout, stderr) = run(&["completions", "powershell"]);
    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout}");
    assert!(
        stderr.contains("unsupported shell 'powershell'"),
        "stderr: {stderr}"
    );
    for shell in ["bash", "zsh", "fish"] {
        assert!(stderr.contains(shell), "stderr: {stderr}");
    }
}

#[test]
fn completions_missing_shell_is_a_usage_error() {
    let (code, _, stderr) = run(&["completions"]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("missing shell"), "stderr: {stderr}");
    for shell in ["bash", "zsh", "fish"] {
        assert!(stderr.contains(shell), "stderr: {stderr}");
    }
}

#[test]
fn version_prints_one_line() {
    let (code, stdout, stderr) = run(&["--version"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        stdout,
        format!("pyx {}\n", env!("CARGO_PKG_VERSION")),
        "unexpected version output"
    );
    assert_eq!(stdout.lines().count(), 1);

    let (code, stdout, _) = run(&["-V"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.lines().count(), 1);
}

#[test]
fn version_verbose_reports_version_target_and_backend() {
    for args in [vec!["--version", "--verbose"], vec!["-V", "--verbose"]] {
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 0, "stderr: {stderr}");
        let version_line = format!("pyx {}", env!("CARGO_PKG_VERSION"));
        assert_eq!(stdout.lines().next(), Some(version_line.as_str()));
        assert!(
            stdout.contains(env!("CARGO_PKG_VERSION")),
            "stdout: {stdout}"
        );
        let target = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
        assert!(stdout.contains(&target), "stdout: {stdout}");
        assert!(stdout.contains("Cranelift"), "stdout: {stdout}");
        assert!(stdout.contains("runtime: workspace"), "stdout: {stdout}");
        assert!(
            stdout.contains("python: 3.14.9 compatibility target (ABI cpython-314)"),
            "stdout: {stdout}"
        );
    }
}

#[test]
fn version_rejects_unknown_options() {
    let (code, _, stderr) = run(&["--version", "--nope"]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(
        stderr.contains("unexpected argument '--nope'"),
        "stderr: {stderr}"
    );
}
