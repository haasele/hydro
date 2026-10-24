//! Mapping of the first command-line argument to a subcommand.

use std::ffi::OsString;

use super::commands::{
    cmd_add, cmd_build, cmd_check, cmd_clean, cmd_doctor, cmd_fmt, cmd_help, cmd_init, cmd_lint,
    cmd_lock, cmd_new, cmd_remove, cmd_run, cmd_spawn, cmd_sync, cmd_test, cmd_version,
};
use super::{CliError, CliResult};

pub(crate) fn dispatch(args: &[OsString]) -> CliResult {
    let Some(command) = args.first() else {
        return Err(CliError::usage("pyx: missing command"));
    };
    let command = command.to_string_lossy();
    match command.as_ref() {
        "new" => cmd_new(&args[1..]),
        "init" => cmd_init(&args[1..]),
        "check" => cmd_check(&args[1..]),
        "run" => cmd_run(&args[1..]),
        "build" => cmd_build(&args[1..]),
        "test" => cmd_test(&args[1..]),
        "fmt" => cmd_fmt(&args[1..]),
        "lint" => cmd_lint(&args[1..]),
        "clean" => cmd_clean(&args[1..]),
        "add" => cmd_add(&args[1..]),
        "remove" => cmd_remove(&args[1..]),
        "sync" => cmd_sync(&args[1..]),
        "lock" => cmd_lock(&args[1..]),
        // Internal entry point for `multiprocessing` spawn children. Not
        // listed in the help output.
        "__spawn" => cmd_spawn(&args[1..]),
        "doctor" => cmd_doctor(&args[1..]),
        "help" | "--help" | "-h" => cmd_help(&args[1..]),
        "--version" | "-V" => cmd_version(&args[1..]),
        _ => Err(CliError::usage(format!("pyx: unknown command '{command}'"))),
    }
}
