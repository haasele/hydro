//! `pyx run` (JIT) and the hidden `pyx __spawn` entry point.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use super::{exit_code, read_source, unwrap_or_report};
use crate::cli::args::parse_single_source;
use crate::cli::diag;
use crate::cli::{CliError, CliResult};

pub(crate) fn cmd_run(args: &[OsString]) -> CliResult {
    let (file, options) = parse_single_source(args, "run")?;
    compile_and_run_jit(&file, &options)
}

/// Hidden entry point used by `multiprocessing` spawn children.
///
/// The parent records the payload path in `PYX_SPAWN_PAYLOAD`; `pyx_main`
/// registers the program functions and consumes the payload before any
/// module body would run.
pub(crate) fn cmd_spawn(args: &[OsString]) -> CliResult {
    if env::var_os("PYX_SPAWN_PAYLOAD")
        .filter(|value| !value.is_empty())
        .is_none()
    {
        return Err(CliError::new(
            1,
            "pyx: __spawn: PYX_SPAWN_PAYLOAD is not set",
        ));
    }
    let (file, options) = parse_single_source(args, "__spawn")?;
    compile_and_run_jit(&file, &options)
}

/// Compiles and JIT-executes one source file, recording its canonical path so
/// a spawned child can recompile the same program.
fn compile_and_run_jit(file: &Path, options: &diag::Options) -> CliResult {
    let source = read_source(file)?;
    let program = unwrap_or_report(
        pyx_frontend::check_file(file),
        &source,
        file,
        "compile",
        options,
    )?;
    // A relative path would break when the child runs from another
    // directory; canonicalization also matches the source location used by
    // diagnostics.
    let canonical = fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    pyx_runtime::set_jit_source(canonical);
    let code = pyx_codegen::run_jit(&program)
        .map_err(|error| CliError::new(2, format!("pyx: internal codegen error: {error}")))?;
    Ok(ExitCode::from(exit_code(code)))
}
