//! `pyx lint`: run the frontend over every Python file of a tree.
//!
//! Every file is type-checked like `pyx check`; diagnostics are printed per
//! file in human mode and aggregated into one JSON array with `--json`.

use std::ffi::OsString;
use std::fs;
use std::process::ExitCode;

use super::scan::{collect_files, is_python_file};
use crate::cli::args::parse_scan;
use crate::cli::{diag, CliError, CliResult};

pub(crate) fn cmd_lint(args: &[OsString]) -> CliResult {
    let options = parse_scan(args, "lint", false)?;
    let files = collect_files(&options.paths, is_python_file)?;

    let mut collected: Vec<pyx_frontend::Diagnostic> = Vec::new();
    let mut failed_files = 0usize;
    let mut read_failures = 0usize;

    for file in &files {
        let source = match fs::read_to_string(file) {
            Ok(source) => source,
            Err(error) => {
                eprintln!("pyx: lint: cannot read '{}': {error}", file.display());
                read_failures += 1;
                continue;
            }
        };
        match pyx_frontend::check_file(file) {
            Ok(_) => {}
            Err(diagnostics) => {
                failed_files += 1;
                if options.diagnostics.json {
                    collected.extend(diagnostics);
                } else {
                    diag::report(
                        &diagnostics,
                        Some(&source),
                        file,
                        "lint",
                        &options.diagnostics,
                    );
                }
            }
        }
    }

    if options.diagnostics.json {
        eprintln!("{}", diag::render_json(&collected));
    } else if failed_files == 0 && read_failures == 0 {
        println!("pyx lint: {} file(s) checked, no errors", files.len());
    } else {
        eprintln!(
            "pyx lint: {} file(s) checked, {} file(s) with errors",
            files.len(),
            failed_files + read_failures
        );
    }

    if failed_files > 0 || read_failures > 0 {
        Err(CliError::silent(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}
