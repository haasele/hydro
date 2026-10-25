//! `pyx pytest`: discover pytest-style test files, run each one in an
//! isolated child process and aggregate the results.
//!
//! A child (`pyx __pytest <file>`) compiles the file with its `conftest.py`
//! ancestors, runs the module body and then calls
//! [`pyx_runtime::run_pytest_with`]. The parent never shares a process with a
//! test module, so a crash, `os._exit` or leaked global state cannot abort the
//! run or leak into the next file.
//!
//! The child writes a tab-separated machine report to stdout
//! (`TEST`/`SUMMARY`/`ERROR` lines); the parent parses it and owns the final
//! human/JSON report and exit code (0 pass, 1 failure, 2 collection/internal,
//! 5 no tests).

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use super::scan::{collect_files, is_test_file};
use crate::cli::args::Verbosity;
use crate::cli::diag;
use crate::cli::{CliError, CliResult};

/// Exit code used when no tests were collected.
const NO_TESTS: u8 = 5;
/// Exit code for collection and internal failures.
const INTERNAL_ERROR: u8 = 2;

struct Options {
    paths: Vec<PathBuf>,
    quiet: bool,
    exit_first: bool,
    maxfail: Option<usize>,
    keyword: Option<String>,
    json: bool,
}

fn parse_options(args: &[OsString], command: &str) -> Result<Options, CliError> {
    let mut options = Options {
        paths: Vec::new(),
        quiet: false,
        exit_first: false,
        maxfail: None,
        keyword: None,
        json: false,
    };
    let mut positional_only = false;
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        let text = argument.to_string_lossy();
        index += 1;
        if !positional_only && text == "--" {
            positional_only = true;
            continue;
        }
        if positional_only || text == "-" || !text.starts_with('-') {
            options.paths.push(PathBuf::from(argument.as_os_str()));
            continue;
        }
        match text.as_ref() {
            "-q" | "--quiet" => options.quiet = true,
            "-x" | "--exitfirst" => options.exit_first = true,
            "--json" => options.json = true,
            "--color" => {
                return Err(CliError::usage(format!(
                    "pyx: {command}: option '--color' requires a value (auto, always or never)"
                )))
            }
            "--no-color" | "--color=auto" | "--color=always" | "--color=never" => {}
            "-k" => {
                let Some(value) = args.get(index) else {
                    return Err(CliError::usage(format!(
                        "pyx: {command}: option '-k' requires an expression"
                    )));
                };
                options.keyword = Some(value.to_string_lossy().into_owned());
                index += 1;
            }
            "-v" | "--verbose" => {}
            _ => {
                if let Some(value) = text.strip_prefix("-k=") {
                    options.keyword = Some(value.to_string());
                } else if let Some(value) = text.strip_prefix("--maxfail=") {
                    let count = value.parse::<usize>().map_err(|_| {
                        CliError::usage(format!(
                            "pyx: {command}: invalid value for '--maxfail': '{value}'"
                        ))
                    })?;
                    if count == 0 {
                        return Err(CliError::usage(format!(
                            "pyx: {command}: '--maxfail' must be at least 1"
                        )));
                    }
                    options.maxfail = Some(count);
                } else if text == "--maxfail" {
                    let Some(value) = args.get(index) else {
                        return Err(CliError::usage(format!(
                            "pyx: {command}: option '--maxfail' requires a number"
                        )));
                    };
                    let value = value.to_string_lossy();
                    let count = value.parse::<usize>().map_err(|_| {
                        CliError::usage(format!(
                            "pyx: {command}: invalid value for '--maxfail': '{value}'"
                        ))
                    })?;
                    if count == 0 {
                        return Err(CliError::usage(format!(
                            "pyx: {command}: '--maxfail' must be at least 1"
                        )));
                    }
                    options.maxfail = Some(count);
                    index += 1;
                } else {
                    return Err(CliError::usage(format!(
                        "pyx: {command}: unsupported option '{text}'"
                    )));
                }
            }
        }
    }
    if options.paths.is_empty() {
        options.paths.push(PathBuf::from("."));
    }
    if options.exit_first {
        options.maxfail = Some(1);
    }
    Ok(options)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Pass,
    Fail,
    Skip,
    Xfail,
    Xpass,
}

impl Outcome {
    fn parse(text: &str) -> Option<Outcome> {
        Some(match text {
            "pass" => Outcome::Pass,
            "fail" => Outcome::Fail,
            "skip" => Outcome::Skip,
            "xfail" => Outcome::Xfail,
            "xpass" => Outcome::Xpass,
            _ => return None,
        })
    }

    fn symbol(self) -> char {
        match self {
            Outcome::Pass => '.',
            Outcome::Fail => 'F',
            Outcome::Skip => 's',
            Outcome::Xfail => 'x',
            Outcome::Xpass => 'X',
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Fail => "fail",
            Outcome::Skip => "skip",
            Outcome::Xfail => "xfail",
            Outcome::Xpass => "xpass",
        }
    }
}

struct TestCaseResult {
    name: String,
    outcome: Outcome,
    message: String,
}

struct FileResult {
    file: PathBuf,
    tests: Vec<TestCaseResult>,
    errors: Vec<String>,
    /// Program stdout that was not part of the machine report.
    output: Vec<String>,
    /// Set when the child could not be started or exited unexpectedly.
    launch_error: Option<String>,
}

pub(crate) fn cmd_pytest(args: &[OsString], verbosity: Verbosity) -> CliResult {
    let options = parse_options(args, "pytest")?;
    let files = collect_files(&options.paths, is_test_file)?;
    let quiet = options.quiet || verbosity.is_quiet();
    if files.is_empty() {
        if options.json {
            println!(
                "{{\"exit_code\":5,\"files\":[],\"passed\":0,\"failed\":0,\"skipped\":0,\
                 \"xfailed\":0,\"xpassed\":0,\"errors\":0}}"
            );
        } else if !quiet {
            println!("pyx pytest: no test files found");
        }
        return Ok(ExitCode::from(NO_TESTS));
    }

    let executable = env::current_exe().map_err(|error| {
        CliError::new(
            INTERNAL_ERROR,
            format!("pyx: pytest: cannot locate the pyx executable: {error}"),
        )
    })?;

    let mut results: Vec<FileResult> = Vec::with_capacity(files.len());
    let mut failures = 0usize;
    for file in files {
        let remaining = options
            .maxfail
            .map(|limit| limit.saturating_sub(failures).max(1));
        let result = run_file(&executable, &file, &options, remaining);
        failures += result
            .tests
            .iter()
            .filter(|test| test.outcome == Outcome::Fail)
            .count()
            + result.errors.len();
        let stop = options.maxfail.is_some_and(|limit| failures >= limit);
        results.push(result);
        if stop {
            break;
        }
    }

    let totals = totals(&results);
    let exit = if totals.errors > 0 {
        INTERNAL_ERROR
    } else if totals.failed > 0 {
        1
    } else if totals.passed == 0
        && totals.skipped == 0
        && totals.xfailed == 0
        && totals.xpassed == 0
    {
        NO_TESTS
    } else {
        0
    };

    if options.json {
        print_json(&results, exit);
    } else {
        print_report(&results, &totals, quiet);
    }
    Ok(ExitCode::from(exit))
}

fn run_file(
    executable: &Path,
    file: &Path,
    options: &Options,
    maxfail: Option<usize>,
) -> FileResult {
    let mut command = Command::new(executable);
    command.arg("__pytest").arg(file);
    if let Some(keyword) = &options.keyword {
        command.arg("-k").arg(keyword);
    }
    if let Some(maxfail) = maxfail {
        command.arg(format!("--maxfail={maxfail}"));
    }
    command.env("PYX_PYTEST", "1");
    command.env("PYX_PYTEST_MACHINE", "1");

    match command.output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let mut result = FileResult {
                file: file.to_path_buf(),
                tests: Vec::new(),
                errors: Vec::new(),
                output: Vec::new(),
                launch_error: None,
            };
            for line in stdout.lines() {
                let mut fields = line.splitn(4, '\t');
                match fields.next() {
                    Some("TEST") => {
                        let outcome = fields.next().and_then(Outcome::parse);
                        let name = fields.next().map(unescape);
                        let message = fields.next().map(unescape).unwrap_or_default();
                        if let (Some(outcome), Some(name)) = (outcome, name) {
                            result.tests.push(TestCaseResult {
                                name,
                                outcome,
                                message,
                            });
                        }
                    }
                    Some("ERROR") => {
                        if let Some(message) = fields.next() {
                            result.errors.push(unescape(message));
                        }
                    }
                    Some("SUMMARY") | Some("") => {}
                    _ => result.output.push(line.to_string()),
                }
            }
            if !output.status.success()
                && output.status.code() != Some(5)
                && result.tests.is_empty()
                && result.errors.is_empty()
            {
                let message = if stderr.trim().is_empty() {
                    format!(
                        "test module exited with status {}",
                        output.status.code().unwrap_or(-1)
                    )
                } else {
                    stderr.trim().to_string()
                };
                result.errors.push(message);
            }
            result
        }
        Err(error) => FileResult {
            file: file.to_path_buf(),
            tests: Vec::new(),
            errors: Vec::new(),
            output: Vec::new(),
            launch_error: Some(format!("cannot run '{}': {error}", file.display())),
        },
    }
}

struct Totals {
    passed: usize,
    failed: usize,
    skipped: usize,
    xfailed: usize,
    xpassed: usize,
    errors: usize,
}

fn totals(results: &[FileResult]) -> Totals {
    let mut totals = Totals {
        passed: 0,
        failed: 0,
        skipped: 0,
        xfailed: 0,
        xpassed: 0,
        errors: 0,
    };
    for result in results {
        totals.errors += result.errors.len();
        if result.launch_error.is_some() {
            totals.errors += 1;
        }
        for test in &result.tests {
            match test.outcome {
                Outcome::Pass => totals.passed += 1,
                Outcome::Fail => totals.failed += 1,
                Outcome::Skip => totals.skipped += 1,
                Outcome::Xfail => totals.xfailed += 1,
                Outcome::Xpass => totals.xpassed += 1,
            }
        }
    }
    totals
}

fn print_report(results: &[FileResult], totals: &Totals, quiet: bool) {
    if quiet {
        for result in results {
            for test in &result.tests {
                print!("{}", test.outcome.symbol());
            }
            if result.launch_error.is_some() || !result.errors.is_empty() {
                print!("E");
            }
        }
        println!();
    } else {
        for result in results {
            print!("{} ", result.file.display());
            for test in &result.tests {
                print!("{}", test.outcome.symbol());
            }
            if result.launch_error.is_some() || !result.errors.is_empty() {
                print!("E");
            }
            println!();
        }
    }

    for result in results {
        if !quiet {
            for line in &result.output {
                println!("{line}");
            }
        }
        for test in &result.tests {
            match test.outcome {
                Outcome::Fail => println!(
                    "FAILED {}::{} - {}",
                    result.file.display(),
                    test.name,
                    test.message
                ),
                Outcome::Xpass => {
                    println!("XPASS {}::{}", result.file.display(), test.name)
                }
                _ => {}
            }
        }
        if let Some(error) = &result.launch_error {
            println!("ERROR {} - {}", result.file.display(), error);
        }
        for error in &result.errors {
            println!("ERROR {} - {}", result.file.display(), error);
        }
    }

    println!();
    println!(
        "{} passed, {} failed, {} skipped, {} xfailed, {} xpassed, {} errors",
        totals.passed, totals.failed, totals.skipped, totals.xfailed, totals.xpassed, totals.errors
    );
}

fn print_json(results: &[FileResult], exit: u8) {
    let totals = totals(results);
    let mut out = String::new();
    out.push('{');
    out.push_str(&format!("\"exit_code\":{exit},\"files\":["));
    for (index, result) in results.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str("{\"file\":");
        out.push_str(&diag::json_string(&result.file.display().to_string()));
        out.push_str(",\"tests\":[");
        for (test_index, test) in result.tests.iter().enumerate() {
            if test_index > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"name\":{},\"outcome\":{}, \"message\":{}}}",
                diag::json_string(&test.name),
                diag::json_string(test.outcome.as_str()),
                diag::json_string(&test.message)
            ));
        }
        out.push_str("],\"errors\":[");
        for (error_index, error) in result.errors.iter().enumerate() {
            if error_index > 0 {
                out.push(',');
            }
            out.push_str(&diag::json_string(error));
        }
        if let Some(error) = &result.launch_error {
            if !result.errors.is_empty() {
                out.push(',');
            }
            out.push_str(&diag::json_string(error));
        }
        out.push_str("]}");
    }
    out.push_str(&format!(
        "],\"passed\":{},\"failed\":{},\"skipped\":{},\"xfailed\":{},\"xpassed\":{},\
         \"errors\":{}}}",
        totals.passed, totals.failed, totals.skipped, totals.xfailed, totals.xpassed, totals.errors
    ));
    println!("{out}");
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Hidden child entry point: run one test file and print its machine report.
pub(crate) fn cmd_pytest_child(args: &[OsString], verbosity: Verbosity) -> CliResult {
    let options = parse_options(args, "__pytest")?;
    let Some(file) = options.paths.first().cloned() else {
        return Err(CliError::usage("pyx: __pytest: missing test file"));
    };
    if options.paths.len() > 1 {
        return Err(CliError::usage(
            "pyx: __pytest: accepts exactly one test file",
        ));
    }

    env::set_var("PYX_PYTEST", "1");
    let source = fs::read_to_string(&file).map_err(|error| {
        CliError::new(
            1,
            format!("pyx: __pytest: cannot read '{}': {error}", file.display()),
        )
    })?;
    let diagnostic_options = diag::Options {
        verbosity,
        ..diag::Options::default()
    };
    let program = match pyx_frontend::check_pytest_file(&file) {
        Ok(program) => program,
        Err(compile_diagnostics) => {
            let code = diag::report(
                &compile_diagnostics,
                Some(&source),
                &file,
                "compile",
                &diagnostic_options,
            );
            return Err(CliError::silent(code));
        }
    };
    let canonical = fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
    pyx_runtime::set_jit_source(canonical);
    pyx_runtime::set_argv(file.display().to_string(), Vec::new());
    let code = pyx_codegen::run_jit(&program).map_err(|error| {
        CliError::new(
            INTERNAL_ERROR,
            format!("pyx: __pytest: internal codegen error: {error}"),
        )
    })?;
    if code != 0 {
        if env::var_os("PYX_PYTEST_MACHINE").is_some() {
            println!("ERROR\tmodule body failed with status {code}");
        }
        return Ok(ExitCode::from(INTERNAL_ERROR));
    }
    let code = pyx_runtime::run_pytest_with(options.keyword.as_deref(), options.maxfail);
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
}
