//! `pyx clean`: remove generated build state.
//!
//! Only the project-local cache (`.pyx/cache`) and the temporary AOT directory
//! (`target/pyx-tmp`) are removed; the rest of `target/` is left alone. Missing
//! paths are not an error, so running the command twice is safe.

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use crate::cli::args::{parse_clean, Verbosity};
use crate::cli::{CliError, CliResult};

/// Paths relative to the working directory that `clean` owns.
const TARGETS: [&str; 2] = [".pyx/cache", "target/pyx-tmp"];

pub(crate) fn cmd_clean(args: &[OsString], verbosity: Verbosity) -> CliResult {
    let (dry_run, verbosity) = parse_clean(args, verbosity)?;
    let quiet = verbosity.is_quiet();
    let mut found = 0usize;

    for target in TARGETS {
        let path = Path::new(target);
        if !path.exists() {
            continue;
        }
        found += 1;
        if dry_run {
            if !quiet {
                println!("would remove {target}");
            }
            continue;
        }
        fs::remove_dir_all(path).map_err(|error| {
            CliError::new(1, format!("pyx: clean: cannot remove '{target}': {error}"))
        })?;
        if !quiet {
            println!("removed {target}");
        }
    }

    if found == 0 && !quiet {
        println!("pyx clean: nothing to remove");
    }
    Ok(ExitCode::SUCCESS)
}
