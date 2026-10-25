//! `pyx repl`, `pyx -c` and `pyx -i`: one persistent tree-walk interpreter
//! session.
//!
//! Unlike `pyx run` (JIT) and `pyx run --interpret` (one program, one run),
//! these commands keep the [`pyx_interp::Interp`] and the frontend's
//! `SessionCompiler` alive for the whole process. Each input is compiled as a
//! session fragment, executed in the same runtime namespace and leaves its
//! module-level bindings, functions and classes visible to the next input.
//!
//! * `pyx repl` reads standard input until EOF.
//! * `pyx -c <code>` runs `code` once and exits; `sys.argv[0]` is `-c`.
//! * `pyx -i [<file> | -c <code>]` runs the prelude (if any) and then enters
//!   the same session's REPL without resetting it.
//!
//! Auto-indented blocks are buffered while the frontend reports the fragment
//! as incomplete; a blank line forces compilation, EOF exits and a genuine
//! syntax error is reported immediately. Prompts and the banner go to stderr
//! and only appear when standard input is a terminal (`--quiet` and
//! `--no-banner` suppress them); program output and echoed expression values
//! go to stdout.

use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

use pyx_frontend::{is_incomplete, SessionCompiler, SessionFragment, SessionOptions};
use pyx_runtime::abi;

use crate::cli::args::{parse_repl_args, InterpreterRequest, Prelude, ReplOptions, Verbosity};
use crate::cli::diag;
use crate::cli::{CliError, CliResult};

/// How one fragment ended.
enum Run {
    /// The fragment completed; any echo was already printed.
    Ok,
    /// An uncaught exception was reported; carries the process exit code.
    Raised(u8),
    /// The fragment did not compile (diagnostics were reported).
    Failed,
}

/// One persistent REPL/interpreter session: the frontend's incremental
/// compiler and the interpreter that runs its fragments.
struct Session {
    compiler: SessionCompiler,
    interp: pyx_interp::Interp,
    options: ReplOptions,
}

impl Session {
    fn new(compiler: SessionOptions, options: ReplOptions) -> Session {
        // One runtime initialization per process: `pyx_runtime_init` resets
        // the heap and the module global table, so it must precede the first
        // fragment and must not run between fragments.
        abi::pyx_runtime_init();
        Session {
            compiler: SessionCompiler::new(compiler),
            interp: pyx_interp::Interp::new("__main__"),
            options,
        }
    }

    /// Compiles and runs `source` as one session fragment.
    fn run_source(&mut self, source: &str, echo: bool) -> Run {
        let fragment = match self.compiler.compile(source) {
            Ok(fragment) => fragment,
            Err(diagnostics) => {
                let filename = self.compiler.options().entry.filename();
                diag::report(
                    &diagnostics,
                    Some(source),
                    Path::new(&filename),
                    "compile",
                    &self.options.diag,
                );
                return Run::Failed;
            }
        };
        self.run_fragment(fragment, echo)
    }

    /// Reads and runs `path` as one session fragment.
    fn run_file(&mut self, path: &Path, echo: bool) -> Run {
        let fragment = match self.compiler.compile_file(path) {
            Ok(fragment) => fragment,
            Err(diagnostics) => {
                diag::report(&diagnostics, None, path, "compile", &self.options.diag);
                return Run::Failed;
            }
        };
        self.run_fragment(fragment, echo)
    }

    fn run_fragment(&mut self, fragment: SessionFragment, echo: bool) -> Run {
        let SessionFragment {
            program,
            initializers,
            bound_names,
            ..
        } = fragment;
        // Tracebacks of this fragment name its own source (`<stdin>`,
        // `<pyx -c>` or the script path).
        let source_name = program.source_name.clone();
        abi::pyx_set_file(source_name.as_ptr(), source_name.len());
        let outcome = self
            .interp
            .run_fragment(program, &initializers, &bound_names, echo);
        if !outcome.completed {
            // `pyx_uncaught` prints the traceback, clears the exception and
            // returns the exit code; the session stays usable.
            let code = abi::pyx_uncaught() as i32;
            return Run::Raised(u8::try_from(code).unwrap_or(1));
        }
        if let Some(value) = outcome.value {
            echo_repr(value);
        }
        Run::Ok
    }

    /// Reads standard input until EOF, executing one fragment per logical
    /// input. Exceptions and syntax errors do not end the session.
    fn repl(&mut self) -> CliResult {
        let stdin = std::io::stdin();
        let interactive = stdin.is_terminal();
        let prompt = interactive && !self.options.quiet;
        if interactive && self.options.banner && !self.options.quiet {
            eprintln!(
                "PyX {} persistent interpreter (EOF to exit)",
                env!("CARGO_PKG_VERSION")
            );
        }
        let echo = self.options.echo;
        let mut reader = stdin.lock();
        let mut buffer = String::new();
        loop {
            if prompt {
                let text = if buffer.trim().is_empty() {
                    ">>> "
                } else {
                    "... "
                };
                let mut stderr = std::io::stderr();
                let _ = stderr.write_all(text.as_bytes());
                let _ = stderr.flush();
            }
            let mut line = String::new();
            match reader.read_line(&mut line) {
                // EOF ends the session, discarding an unfinished fragment.
                Ok(0) => return Ok(ExitCode::SUCCESS),
                Ok(_) => {}
                Err(error) => {
                    return Err(CliError::new(
                        1,
                        format!("pyx: repl: cannot read standard input: {error}"),
                    ))
                }
            }
            buffer.push_str(&line);
            let blank = line.trim().is_empty();
            if blank && buffer.trim().is_empty() {
                buffer.clear();
                continue;
            }
            // An open block (`def`, `if`, ...) parses as a complete module
            // once it ends at EOF, so completeness alone is not enough: while
            // the last line is indented the suite may still grow. A blank line
            // is the explicit "compile now" signal; an unclosed bracket or
            // string keeps buffering even across a blank line.
            let incomplete = is_incomplete(&buffer, "<stdin>");
            if blank {
                if incomplete {
                    continue;
                }
            } else if incomplete || ends_inside_block(&buffer) {
                continue;
            }
            let source = std::mem::take(&mut buffer);
            if source.trim().is_empty() {
                continue;
            }
            let _ = self.run_source(&source, echo);
        }
    }
}

/// Whether the buffered input ends inside an indented suite.
///
/// The frontend parses a fragment as a module, so `def f():\n    return 1\n`
/// already looks complete. Interactive input keeps buffering until the user
/// types a blank line or a dedented line, which is what this predicate
/// detects from the last physical line.
fn ends_inside_block(buffer: &str) -> bool {
    match buffer.lines().last() {
        Some(line) => line.starts_with(' ') || line.starts_with('\t'),
        None => false,
    }
}

/// Prints `repr(value)` plus a newline on stdout.
///
/// The value is rooted in the session global table (`_`); the repr string is
/// used immediately, so no additional root frame is needed.
fn echo_repr(value: i64) {
    let repr = abi::pyx_to_repr(value);
    if abi::pyx_exc_pending() != 0 {
        abi::pyx_exc_clear();
        return;
    }
    abi::pyx_print(1, &repr as *const i64);
}

/// `pyx repl [--no-echo] [--no-banner] [--quiet]`.
pub(crate) fn cmd_repl(args: &[OsString], verbosity: Verbosity) -> CliResult {
    let options = parse_repl_args(args, "repl", verbosity)?;
    pyx_runtime::set_argv(String::new(), Vec::new());
    let mut session = Session::new(SessionOptions::stdin(), options);
    session.repl()
}

/// Top-level `pyx -c` and `pyx -i`.
pub(crate) fn cmd_interpreter(request: InterpreterRequest) -> CliResult {
    match request {
        InterpreterRequest::Command {
            code,
            program_args,
            options,
        } => {
            pyx_runtime::set_argv("-c".to_string(), program_args);
            let mut session = Session::new(SessionOptions::command(), options);
            match session.run_source(&code, false) {
                Run::Ok => Ok(ExitCode::SUCCESS),
                Run::Raised(code) => Ok(ExitCode::from(code)),
                Run::Failed => Ok(ExitCode::from(1)),
            }
        }
        InterpreterRequest::Interactive {
            prelude,
            program_args,
            options,
        } => {
            // The prelude's entry decides `sys.argv[0]` and the compiler
            // entry; the REPL then continues from the same compiler, so
            // `-i script.py` keeps the script's bindings and only compiles
            // later input as `<stdin>`.
            let (compiler, argv0) = match &prelude {
                Some(Prelude::File(path)) => (SessionOptions::stdin(), path.display().to_string()),
                Some(Prelude::Command(_)) => (SessionOptions::command(), "-c".to_string()),
                None => (SessionOptions::stdin(), String::new()),
            };
            pyx_runtime::set_argv(argv0, program_args);
            let mut session = Session::new(compiler, options);
            match prelude {
                Some(Prelude::File(path)) => {
                    if matches!(session.run_file(&path, false), Run::Failed) {
                        return Ok(ExitCode::from(1));
                    }
                }
                Some(Prelude::Command(code)) => {
                    if matches!(session.run_source(&code, false), Run::Failed) {
                        return Ok(ExitCode::from(1));
                    }
                }
                None => {}
            }
            session.repl()
        }
    }
}
