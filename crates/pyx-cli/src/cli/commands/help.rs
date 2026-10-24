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
    let mut verbose = false;
    for extra in args {
        match extra.to_str() {
            Some("--verbose") => verbose = true,
            _ => {
                return Err(CliError::usage(format!(
                    "pyx: version: unexpected argument '{}'",
                    extra.to_string_lossy()
                )))
            }
        }
    }

    let version = env!("CARGO_PKG_VERSION");
    println!("pyx {version}");
    if !verbose {
        return Ok(ExitCode::SUCCESS);
    }

    // The runtime is built from this workspace, so its package version is the
    // workspace version; there is no separate `pyx_runtime::VERSION` constant.
    println!("target: {}-{}", env::consts::ARCH, env::consts::OS);
    println!("backend: Cranelift");
    println!("runtime: workspace {version}");
    println!("python: subset of Python 3 (see README, \"Language and Stdlib Scope\")");
    Ok(ExitCode::SUCCESS)
}
