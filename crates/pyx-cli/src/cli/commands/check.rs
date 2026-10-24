//! `pyx check`: parse and type-check a source file without running it.

use std::ffi::OsString;
use std::process::ExitCode;

use super::{read_source, unwrap_or_report};
use crate::cli::args::parse_single_source;
use crate::cli::CliResult;

pub(crate) fn cmd_check(args: &[OsString]) -> CliResult {
    let (file, options) = parse_single_source(args, "check")?;
    let source = read_source(&file)?;
    let _program = unwrap_or_report(
        pyx_frontend::check_file(&file),
        &source,
        &file,
        "check",
        &options,
    )?;
    Ok(ExitCode::SUCCESS)
}
