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

    match classify(dispatched) {
        Outcome::Exit(code) => code,
        Outcome::Failure { code, message } => {
            if let Some(message) = message {
                eprintln!("{message}");
            }
            ExitCode::from(code)
        }
    }
}

/// A dispatch outcome independent of process I/O, so the panic mapping is
/// testable without a production panic trigger.
enum Outcome {
    Exit(ExitCode),
    Failure { code: u8, message: Option<String> },
}

fn classify(dispatched: std::thread::Result<cli::CliResult>) -> Outcome {
    match dispatched {
        Ok(Ok(code)) => Outcome::Exit(code),
        Ok(Err(error)) => Outcome::Failure {
            code: error.code,
            message: error.message,
        },
        Err(_) => Outcome::Failure {
            code: 2,
            message: Some(ice_banner()),
        },
    }
}

/// The report printed for an internal compiler error; it never includes the
/// panic payload.
fn ice_banner() -> String {
    format!(
        "pyx: internal compiler error (ICE)\n\
         version {} target {}-{}\n\
         please report this with the command you ran and RUST_BACKTRACE=1 output",
        env!("CARGO_PKG_VERSION"),
        env::consts::ARCH,
        env::consts::OS
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_maps_to_an_ice_without_the_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("secret panic payload");
        let outcome = classify(Err(payload));
        let Outcome::Failure { code, message } = outcome else {
            panic!("a panic must be classified as a failure");
        };
        assert_eq!(code, 2);
        let message = message.expect("an ICE report");
        assert!(message.contains("internal compiler error (ICE)"));
        assert!(message.contains("target "));
        assert!(message.contains("RUST_BACKTRACE=1"));
        assert!(!message.contains("secret panic payload"));
    }

    #[test]
    fn dispatch_errors_keep_their_exit_code_and_message() {
        let error = cli::CliError::new(2, "pyx: boom");
        let outcome = classify(Ok(Err(error)));
        let Outcome::Failure { code, message } = outcome else {
            panic!("an error must be classified as a failure");
        };
        assert_eq!(code, 2);
        assert_eq!(message.as_deref(), Some("pyx: boom"));
    }
}
