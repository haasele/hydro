//! `pyx test`: run every test module of a tree through `pyx run`.
//!
//! A test succeeds when the compiled program exits with status 0. There are no
//! pytest-style assertions: write plain PyX programs and let a crash or a
//! frontend diagnostic fail the test.

use std::env;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use super::scan::{collect_files, is_test_file};
use crate::cli::args::{parse_scan, ScanOptions};
use crate::cli::diag;
use crate::cli::{CliError, CliResult};

struct TestResult {
    file: PathBuf,
    code: i32,
    passed: bool,
    stdout: String,
    stderr: String,
}

pub(crate) fn cmd_test(args: &[OsString]) -> CliResult {
    let options = parse_scan(args, "test", false)?;
    let files = collect_files(&options.paths, is_test_file)?;

    if files.is_empty() {
        if options.diagnostics.json {
            println!("[]");
        } else {
            println!("pyx test: no test files found");
        }
        return Ok(ExitCode::SUCCESS);
    }

    let executable = env::current_exe().map_err(|error| {
        CliError::new(
            2,
            format!("pyx: test: cannot locate the pyx executable: {error}"),
        )
    })?;
    let mut results = Vec::with_capacity(files.len());
    for file in files {
        results.push(run_test(&executable, &file, &options));
    }

    if options.diagnostics.json {
        print_json(&results);
    } else {
        for result in &results {
            if result.passed {
                println!("PASS {}", result.file.display());
            } else {
                println!("FAIL {} (exit code {})", result.file.display(), result.code);
            }
            write_child_output(result);
        }
        let passed = results.iter().filter(|result| result.passed).count();
        println!(
            "\n{} passed, {} failed, {} total",
            passed,
            results.len() - passed,
            results.len()
        );
    }

    if results.iter().all(|result| result.passed) {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

/// Runs one test file in a child `pyx run` so program state never leaks
/// between tests and the child's own exit-code contract applies.
fn run_test(executable: &Path, file: &Path, options: &ScanOptions) -> TestResult {
    let mut command = Command::new(executable);
    command.arg("run").arg(file);
    if options.diagnostics.json {
        command.arg("--json");
    }
    match options.diagnostics.color {
        diag::ColorMode::Always => {
            command.arg("--color=always");
        }
        diag::ColorMode::Never => {
            command.arg("--no-color");
        }
        diag::ColorMode::Auto => {}
    }

    match command.output() {
        Ok(output) => TestResult {
            file: file.to_path_buf(),
            code: output.status.code().unwrap_or(-1),
            passed: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(error) => TestResult {
            file: file.to_path_buf(),
            code: -1,
            passed: false,
            stdout: String::new(),
            stderr: format!("pyx: test: cannot run '{}': {error}\n", file.display()),
        },
    }
}

fn write_child_output(result: &TestResult) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(result.stdout.as_bytes());
    if !result.stdout.is_empty() && !result.stdout.ends_with('\n') {
        let _ = stdout.write_all(b"\n");
    }
    let _ = stdout.flush();
    let mut stderr = std::io::stderr();
    let _ = stderr.write_all(result.stderr.as_bytes());
    let _ = stderr.flush();
}

fn print_json(results: &[TestResult]) {
    let mut out = String::from("[");
    for (index, result) in results.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"file\":{},\"status\":{},\"code\":{},\"stdout\":{},\"stderr\":{}}}",
            diag::json_string(&result.file.display().to_string()),
            diag::json_string(if result.passed { "pass" } else { "fail" }),
            result.code,
            diag::json_string(&result.stdout),
            diag::json_string(&result.stderr)
        ));
    }
    out.push(']');
    println!("{out}");
}
