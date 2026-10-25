//! `pyx fmt`: normalize trailing whitespace and the final newline.
//!
//! This is deliberately not an AST formatter: indentation, spacing inside
//! lines and line breaks are preserved. Only trailing whitespace is removed
//! and the file is terminated by exactly one `\n`. Empty files stay empty.

use std::ffi::OsString;
use std::fs;
use std::process::ExitCode;

use super::scan::{collect_files, is_python_file};
use crate::cli::args::{parse_scan, Verbosity};
use crate::cli::{CliError, CliResult};

pub(crate) fn cmd_fmt(args: &[OsString], verbosity: Verbosity) -> CliResult {
    let options = parse_scan(args, "fmt", true, verbosity)?;
    let files = collect_files(&options.paths, is_python_file)?;

    let mut changed = 0usize;
    let mut failures = 0usize;
    for file in &files {
        let source = match fs::read_to_string(file) {
            Ok(source) => source,
            Err(error) => {
                eprintln!("pyx: fmt: cannot read '{}': {error}", file.display());
                failures += 1;
                continue;
            }
        };
        let formatted = format_source(&source);
        if formatted == source {
            continue;
        }
        changed += 1;
        if options.check {
            println!("would reformat {}", file.display());
            continue;
        }
        if let Err(error) = fs::write(file, &formatted) {
            eprintln!("pyx: fmt: cannot write '{}': {error}", file.display());
            failures += 1;
            continue;
        }
        if !options.diagnostics.verbosity.is_quiet() {
            println!("formatted {}", file.display());
        }
    }

    if options.check && changed > 0 {
        eprintln!("pyx: fmt: {changed} file(s) need formatting");
    }
    if (changed > 0 && options.check) || failures > 0 {
        return Err(CliError::silent(1));
    }
    Ok(ExitCode::SUCCESS)
}

/// Removes trailing whitespace from every line and keeps exactly one final
/// newline. Empty input stays empty.
pub(crate) fn format_source(source: &str) -> String {
    if source.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(source.len() + 1);
    for line in source.lines() {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}
