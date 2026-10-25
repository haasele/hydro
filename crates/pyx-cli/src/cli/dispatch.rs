//! Mapping of the first command-line argument to a subcommand.

use std::ffi::OsString;

use super::args;
use super::commands::{
    cmd_add, cmd_build, cmd_check, cmd_clean, cmd_completions, cmd_doctor, cmd_fmt, cmd_gc,
    cmd_help, cmd_init, cmd_lint, cmd_lock, cmd_new, cmd_remove, cmd_run, cmd_spawn, cmd_sync,
    cmd_test, cmd_version,
};
use super::{CliError, CliResult};

pub(crate) fn dispatch(args: &[OsString]) -> CliResult {
    let (verbosity, args) = args::parse_global(args)?;
    let Some(command) = args.first() else {
        return Err(CliError::usage("pyx: missing command"));
    };
    let command = command.to_string_lossy();
    match command.as_ref() {
        "new" => cmd_new(&args[1..], verbosity),
        "init" => cmd_init(&args[1..], verbosity),
        "check" => cmd_check(&args[1..], verbosity),
        "run" => cmd_run(&args[1..], verbosity),
        "gc" => cmd_gc(&args[1..], verbosity),
        "build" => cmd_build(&args[1..], verbosity),
        "test" => cmd_test(&args[1..], verbosity),
        "fmt" => cmd_fmt(&args[1..], verbosity),
        "lint" => cmd_lint(&args[1..], verbosity),
        "clean" => cmd_clean(&args[1..], verbosity),
        "add" => cmd_add(&args[1..]),
        "remove" => cmd_remove(&args[1..]),
        "sync" => cmd_sync(&args[1..]),
        "lock" => cmd_lock(&args[1..]),
        // Internal entry point for `multiprocessing` spawn children. Not
        // listed in the help output.
        "__spawn" => cmd_spawn(&args[1..], verbosity),
        "doctor" => cmd_doctor(&args[1..], verbosity),
        "completions" => cmd_completions(&args[1..]),
        "help" | "--help" | "-h" => cmd_help(&args[1..]),
        "--version" | "-V" => cmd_version(&args[1..], verbosity),
        _ => Err(CliError::usage(format!("pyx: unknown command '{command}'"))),
    }
}
