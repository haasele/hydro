//! `pyx` CLI: check, run (JIT) and build (AOT) Python sources.
//!
//! Exit codes: `0` success, `1` user/program error, `2` internal/setup error.
//! The CLI never panics on user input; every failure is reported on stderr.
//!
//! This root module only installs the panic boundary and maps errors to exit
//! codes; [`cli`] owns argument parsing, dispatch and the commands.

mod cli;

use std::env;
use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();

    // A panic here is a compiler bug, not a user error. Catching it keeps the
    // exit code contract (2 = internal error) and lets us print an actionable
    // ICE report; the regular panic hook still fires so RUST_BACKTRACE works.
    let dispatched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cli::dispatch::dispatch(&args)
    }));

    match dispatched {
        Ok(Ok(code)) => code,
        Ok(Err(error)) => {
            if let Some(message) = error.message {
                eprintln!("{message}");
            }
            ExitCode::from(error.code)
        }
        Err(_) => {
            eprintln!("pyx: internal compiler error (ICE)");
            eprintln!(
                "version {} target {}-{}",
                env!("CARGO_PKG_VERSION"),
                env::consts::ARCH,
                env::consts::OS
            );
            eprintln!("please report this with the command you ran and RUST_BACKTRACE=1 output");
            ExitCode::from(2)
        }
    }
}
