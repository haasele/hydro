//! `pyx doctor`: report the toolchain and environment the CLI will use.
//!
//! The report answers the two questions that decide whether `run`/`build`
//! work: is a C compiler available, and can the runtime staticlib be found.

use std::env;
use std::ffi::OsString;
use std::process::{Command, ExitCode};

use super::build::runtime_lib_status;
use crate::cli::diag;
use crate::cli::CliError;

/// Maximum characters of the C compiler's version banner that are printed.
const VERSION_LIMIT: usize = 120;

pub(crate) fn cmd_doctor(args: &[OsString]) -> Result<ExitCode, CliError> {
    let mut json = false;

    for arg in args {
        let text = arg.to_string_lossy();
        match text.as_ref() {
            "--json" => json = true,
            // Color flags are accepted for symmetry with the compiler
            // commands; the report itself is plain text.
            "--no-color" => {}
            "--color" => {
                return Err(CliError::usage(
                    "pyx: doctor: option '--color' requires a value (auto, always or never)",
                ))
            }
            other => {
                if let Some(value) = other.strip_prefix("--color=") {
                    match value {
                        "auto" | "always" | "never" => {}
                        invalid => {
                            return Err(CliError::usage(format!(
                                "pyx: doctor: invalid value for '--color': '{invalid}' \
                                 (expected auto, always or never)"
                            )))
                        }
                    }
                } else {
                    return Err(CliError::usage(format!(
                        "pyx: doctor: unexpected argument '{}'",
                        text
                    )));
                }
            }
        }
    }

    let version = env!("CARGO_PKG_VERSION");
    let target = format!("{}-{}", env::consts::ARCH, env::consts::OS);
    let workspace = env::current_dir()
        .map(|directory| directory.display().to_string())
        .unwrap_or_else(|_| String::from("."));
    let (runtime_lib, runtime_lib_found) = runtime_lib_status();
    let runtime_lib = runtime_lib.display().to_string();
    let compiler = Compiler::detect();

    if json {
        println!(
            "{{\"version\":{},\"target\":{},\"runtime_lib\":{},\"runtime_lib_found\":{},\
             \"cc\":{},\"cc_found\":{},\"workspace\":{}}}",
            diag::json_string(version),
            diag::json_string(&target),
            diag::json_string(&runtime_lib),
            runtime_lib_found,
            diag::json_string(&compiler.command),
            compiler.found,
            diag::json_string(&workspace)
        );
    } else {
        println!("pyx {version} ({target})");
        println!(
            "runtime library: {runtime_lib} ({})",
            if runtime_lib_found {
                "found"
            } else {
                "missing"
            }
        );
        if !runtime_lib_found {
            println!(
                "hint: AOT builds need libpyx_runtime.a; build it with \
                 `cargo build -p pyx-runtime`"
            );
        }
        let version = compiler.version.as_deref().unwrap_or("not found");
        println!("C compiler: {} ({version})", compiler.command);
        println!("workspace: {workspace}");
    }

    // The JIT does not need the runtime staticlib, so availability of the C
    // compiler is the only hard requirement for a usable toolchain.
    if compiler.found {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

struct Compiler {
    command: String,
    version: Option<String>,
    found: bool,
}

impl Compiler {
    fn detect() -> Compiler {
        let command = env::var_os("CC")
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("cc"));

        let output = Command::new(&command).arg("--version").output();
        match output {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let first = stdout
                    .lines()
                    .chain(stderr.lines())
                    .map(str::trim)
                    .find(|line| !line.is_empty());
                match first {
                    Some(line) => Compiler {
                        command,
                        version: Some(truncate(line, VERSION_LIMIT)),
                        found: true,
                    },
                    None => Compiler {
                        command,
                        version: None,
                        found: output.status.success(),
                    },
                }
            }
            Err(_) => Compiler {
                command,
                version: None,
                found: false,
            },
        }
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut shortened: String = text.chars().take(limit.saturating_sub(3)).collect();
    shortened.push_str("...");
    shortened
}
