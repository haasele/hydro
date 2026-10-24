//! Command-line handling for the `pyx` binary.
//!
//! Layout:
//! - [`args`] parses the flags shared by the subcommands,
//! - [`dispatch`] maps the first positional argument to a command,
//! - [`commands`] holds one module per subcommand,
//! - [`diag`] renders frontend diagnostics.

pub(crate) mod args;
pub(crate) mod commands;
pub(crate) mod diag;
pub(crate) mod dispatch;

use std::process::ExitCode;

/// A failure that terminates the CLI.
///
/// `message` is `None` when the command already printed its output (for
/// example a list of frontend diagnostics or the C compiler's stderr).
pub(crate) struct CliError {
    pub(crate) code: u8,
    pub(crate) message: Option<String>,
}

impl CliError {
    pub(crate) fn new(code: u8, message: impl Into<String>) -> CliError {
        CliError {
            code,
            message: Some(message.into()),
        }
    }

    pub(crate) fn silent(code: u8) -> CliError {
        CliError {
            code,
            message: None,
        }
    }

    pub(crate) fn usage(message: impl Into<String>) -> CliError {
        CliError::new(2, format!("{}\n\n{}", message.into(), usage_text()))
    }
}

pub(crate) type CliResult = Result<ExitCode, CliError>;

pub(crate) fn usage_text() -> String {
    format!(
        "pyx {}\n\n\
         usage:\n  \
         pyx new <path> [--force]           create a project directory with a minimal scaffold\n  \
         pyx init [path] [--force]          scaffold a project in an existing directory\n  \
         pyx check <file.py> [options]      parse and type-check only\n  \
         pyx run <file.py> [options]        compile with the JIT and run\n  \
         pyx build <file.py> [options]      compile and link a native executable\n  \
         pyx test [paths...] [options]      run test_*.py / *_test.py files with `pyx run`\n  \
         pyx fmt [paths...] [--check]       strip trailing whitespace and fix the final newline\n  \
         pyx lint [paths...] [options]      type-check every .py file in the given paths\n  \
         pyx clean [--dry-run]              remove .pyx/cache and target/pyx-tmp\n  \
         pyx add <spec...> [uv options]     add dependencies with uv\n  \
         pyx remove <spec...> [uv options]  remove dependencies with uv\n  \
         pyx sync [uv options]              sync the environment with uv\n  \
         pyx lock [uv options]              update the lockfile with uv\n  \
         pyx doctor [--json]                report toolchain and environment status\n  \
         pyx help | --help | -h             show this help\n  \
         pyx --version | -V                 show the version\n\n\
         new/init:\n  \
         a scaffold contains main.py, pyproject.toml and .gitignore; without --force\n  \
         existing non-empty files are never overwritten\n\n\
         test/fmt/lint:\n  \
         paths default to the current directory and are searched recursively\n  \
         (hidden entries, target, __pycache__ and node_modules are skipped);\n  \
         explicit files are always used as given\n  \
         a test passes when its program exits with status 0; there are no\n  \
         pytest-style assertions\n  \
         --check                  (fmt) report files that need formatting, do not write\n  \
         --json                   (test) print a JSON array of results on stdout\n  \
                                  (lint) print diagnostics as a JSON array on stderr\n  \
         --color=<WHEN>           colorize output: auto, always or never (default: auto)\n  \
         --no-color               disable colors (same as --color=never)\n\n\
         diagnostic options (check, run, build, test, lint):\n  \
         --json                   print diagnostics as a JSON array on stderr\n  \
         --color=<WHEN>           colorize diagnostics: auto, always or never (default: auto)\n  \
         --no-color               disable colors (same as --color=never)\n\n\
         build options:\n  \
         -o, --output <OUT>       output path (default: <file stem> in the current directory)\n  \
         --runtime-lib <PATH>     path to libpyx_runtime.a\n  \
         --no-cache               do not read or write the build cache (.pyx/cache)\n\n\
         uv commands (add, remove, sync, lock):\n  \
         all arguments are forwarded verbatim to uv in the current directory;\n  \
         pyx adds no options of its own and returns the exit status of uv\n\n\
         environment:\n  \
         CC                       C compiler used to link AOT binaries (default: cc)\n  \
         PYX_RUNTIME_LIB          path to libpyx_runtime.a\n  \
         PYX_UV                   uv executable used by add/remove/sync/lock (default: uv)\n  \
         NO_COLOR                 disable colored diagnostics when set\n\n\
         exit codes: 0 success, 1 user/program error, 2 internal/setup error\n",
        env!("CARGO_PKG_VERSION")
    )
}
