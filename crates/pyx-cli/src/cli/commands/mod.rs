//! One module per `pyx` subcommand, plus the helpers they share.

mod build;
mod check;
mod clean;
mod doctor;
mod fmt;
mod help;
mod lint;
mod project;
mod run;
mod scan;
mod test;
mod uv;

pub(crate) use self::build::cmd_build;
pub(crate) use self::check::cmd_check;
pub(crate) use self::clean::cmd_clean;
pub(crate) use self::doctor::cmd_doctor;
pub(crate) use self::fmt::cmd_fmt;
pub(crate) use self::help::{cmd_help, cmd_version};
pub(crate) use self::lint::cmd_lint;
pub(crate) use self::project::{cmd_init, cmd_new};
pub(crate) use self::run::{cmd_run, cmd_spawn};
pub(crate) use self::test::cmd_test;
pub(crate) use self::uv::{cmd_add, cmd_lock, cmd_remove, cmd_sync};

use std::fs;
use std::path::Path;

use super::diag;
use super::CliError;

/// Print frontend diagnostics and turn them into an exit code 1 failure.
fn unwrap_or_report<T>(
    result: Result<T, Vec<pyx_frontend::Diagnostic>>,
    source: &str,
    file: &Path,
    verb: &str,
    options: &diag::Options,
) -> Result<T, CliError> {
    match result {
        Ok(value) => Ok(value),
        Err(diagnostics) => {
            let code = diag::report(&diagnostics, Some(source), file, verb, options);
            Err(CliError::silent(code))
        }
    }
}

fn read_source(path: &Path) -> Result<String, CliError> {
    fs::read_to_string(path).map_err(|error| {
        CliError::new(1, format!("pyx: cannot read '{}': {error}", path.display()))
    })
}

fn exit_code(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(1)
}
