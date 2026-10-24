//! `pyx help` and `pyx --version`.

use std::env;
use std::ffi::OsString;
use std::process::ExitCode;

use crate::cli::{usage_text, CliError, CliResult};

pub(crate) fn cmd_help(args: &[OsString]) -> CliResult {
    if let Some(extra) = args.first() {
        return Err(CliError::usage(format!(
            "pyx: help: unexpected argument '{}'",
            extra.to_string_lossy()
        )));
    }
    print!("{}", usage_text());
    Ok(ExitCode::SUCCESS)
}

pub(crate) fn cmd_version(args: &[OsString]) -> CliResult {
    if let Some(extra) = args.first() {
        return Err(CliError::usage(format!(
            "pyx: version: unexpected argument '{}'",
            extra.to_string_lossy()
        )));
    }
    println!("pyx {}", env!("CARGO_PKG_VERSION"));
    Ok(ExitCode::SUCCESS)
}
