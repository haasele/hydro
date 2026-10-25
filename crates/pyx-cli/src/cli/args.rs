//! Parsing of the flags shared by the subcommands.

use std::ffi::OsString;
use std::path::PathBuf;

use super::diag;
use super::CliError;

/// Amount of non-error status the CLI prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verbosity {
    /// Suppress non-error status output.
    Quiet,
    Normal,
    /// Print phase timings, cache decisions and extra command detail.
    Verbose,
}

impl Verbosity {
    pub(crate) fn is_quiet(self) -> bool {
        self == Verbosity::Quiet
    }

    pub(crate) fn is_verbose(self) -> bool {
        self == Verbosity::Verbose
    }
}

/// Consume leading global `--verbose`/`--quiet` flags.
pub(crate) fn parse_global(args: &[OsString]) -> Result<(Verbosity, Vec<OsString>), CliError> {
    let mut verbosity = Verbosity::Normal;
    let mut index = 0;
    while index < args.len() {
        let text = args[index].to_string_lossy();
        match apply_verbosity(text.as_ref(), &mut verbosity, "pyx") {
            Some(result) => {
                result?;
                index += 1;
            }
            None => break,
        }
    }
    Ok((verbosity, args[index..].to_vec()))
}

/// Applies a `--verbose`/`--quiet` flag and reports whether it was one.
pub(crate) fn apply_verbosity(
    text: &str,
    verbosity: &mut Verbosity,
    command: &str,
) -> Option<Result<(), CliError>> {
    let requested = match text {
        "--verbose" => Verbosity::Verbose,
        "--quiet" => Verbosity::Quiet,
        _ => return None,
    };
    Some(match *verbosity {
        Verbosity::Normal => {
            *verbosity = requested;
            Ok(())
        }
        current if current == requested => Ok(()),
        _ => Err(CliError::usage(if command == "pyx" {
            "pyx: '--verbose' and '--quiet' cannot be used together".to_string()
        } else {
            format!("pyx: {command}: '--verbose' and '--quiet' cannot be used together")
        })),
    })
}

/// Applies a diagnostic-related flag and reports whether it was one.
///
/// Returns `None` for unrelated arguments; unsupported values are usage errors.
pub(crate) fn apply_diag_flag(
    text: &str,
    options: &mut diag::Options,
    command: &str,
) -> Option<Result<(), CliError>> {
    if let Some(result) = apply_verbosity(text, &mut options.verbosity, command) {
        return Some(result);
    }
    match text {
        "--json" => {
            options.json = true;
            Some(Ok(()))
        }
        "--no-color" => {
            options.color = diag::ColorMode::Never;
            Some(Ok(()))
        }
        "--color" => Some(Err(CliError::usage(format!(
            "pyx: {command}: option '--color' requires a value (auto, always or never)"
        )))),
        _ => {
            let value = text.strip_prefix("--color=")?;
            Some(match value {
                "auto" => {
                    options.color = diag::ColorMode::Auto;
                    Ok(())
                }
                "always" => {
                    options.color = diag::ColorMode::Always;
                    Ok(())
                }
                "never" => {
                    options.color = diag::ColorMode::Never;
                    Ok(())
                }
                invalid => Err(CliError::usage(format!(
                    "pyx: {command}: invalid value for '--color': '{invalid}' \
                     (expected auto, always or never)"
                ))),
            })
        }
    }
}

/// Parse the arguments of commands that take exactly one source file and no
/// program arguments: `check`, `gc` and the hidden `__spawn`; `--` is allowed.
pub(crate) fn parse_single_source(
    args: &[OsString],
    command: &str,
    verbosity: Verbosity,
) -> Result<(PathBuf, diag::Options), CliError> {
    let mut file: Option<PathBuf> = None;
    let mut positional_only = false;
    let mut options = diag::Options {
        verbosity,
        ..diag::Options::default()
    };
    for arg in args {
        let text = arg.to_string_lossy();
        if !positional_only && text == "--" {
            positional_only = true;
            continue;
        }
        if !positional_only && text.starts_with('-') && text != "-" {
            if let Some(result) = apply_diag_flag(text.as_ref(), &mut options, command) {
                result?;
                continue;
            }
            return Err(CliError::usage(format!(
                "pyx: {command}: unsupported option '{text}'"
            )));
        }
        if file.is_some() {
            return Err(CliError::usage(format!(
                "pyx: {command}: unexpected argument '{text}'"
            )));
        }
        file = Some(PathBuf::from(arg.as_os_str()));
    }
    let file =
        file.ok_or_else(|| CliError::usage(format!("pyx: {command}: missing source file")))?;
    Ok((file, options))
}

/// Parse the arguments of `run`: one source file followed by program arguments.
///
/// Diagnostic flags are only recognized before the source file; the first
/// positional argument selects the file and every later token, including `--`,
/// is forwarded to the program verbatim. `run -- file.py a` works as well.
#[allow(dead_code)] // Exercised by the argument-parser unit tests; run uses parse_run_args.
pub(crate) fn parse_source_and_args(
    args: &[OsString],
    command: &str,
    verbosity: Verbosity,
) -> Result<(PathBuf, diag::Options, Vec<String>), CliError> {
    let (file, options, program_args, _) = parse_run_args(args, command, verbosity)?;
    Ok((file, options, program_args))
}

/// Like [`parse_source_and_args`], but also reports `--interpret`.
///
/// `--interpret` selects the IR tree-walk interpreter instead of the JIT; it
/// is only recognized before the source file, like the diagnostic flags.
pub(crate) fn parse_run_args(
    args: &[OsString],
    command: &str,
    verbosity: Verbosity,
) -> Result<(PathBuf, diag::Options, Vec<String>, bool), CliError> {
    let mut file: Option<PathBuf> = None;
    let mut program_args = Vec::new();
    let mut positional_only = false;
    let mut interpret = false;
    let mut options = diag::Options {
        verbosity,
        ..diag::Options::default()
    };
    for arg in args {
        if file.is_some() {
            program_args.push(arg.to_string_lossy().into_owned());
            continue;
        }
        let text = arg.to_string_lossy();
        if !positional_only && text == "--" {
            positional_only = true;
            continue;
        }
        if !positional_only && text == "--interpret" {
            interpret = true;
            continue;
        }
        if !positional_only && text.starts_with('-') && text != "-" {
            if let Some(result) = apply_diag_flag(text.as_ref(), &mut options, command) {
                result?;
                continue;
            }
            return Err(CliError::usage(format!(
                "pyx: {command}: unsupported option '{text}'"
            )));
        }
        file = Some(PathBuf::from(arg.as_os_str()));
    }
    let file =
        file.ok_or_else(|| CliError::usage(format!("pyx: {command}: missing source file")))?;
    Ok((file, options, program_args, interpret))
}

/// Options of `new`/`init`: one destination and the overwrite switch.
pub(crate) struct ProjectOptions {
    pub(crate) path: PathBuf,
    pub(crate) force: bool,
    pub(crate) verbosity: Verbosity,
}

/// Parse `new <path> [--force]` / `init [path] [--force]`.
///
/// `path_required` is true for `new`; `init` defaults to the current directory.
pub(crate) fn parse_project(
    args: &[OsString],
    command: &str,
    path_required: bool,
    verbosity: Verbosity,
) -> Result<ProjectOptions, CliError> {
    let mut path: Option<PathBuf> = None;
    let mut force = false;
    let mut verbosity = verbosity;
    let mut positional_only = false;

    for arg in args {
        let text = arg.to_string_lossy();
        if !positional_only && text == "--" {
            positional_only = true;
            continue;
        }
        if !positional_only && text.starts_with('-') && text != "-" {
            if let Some(result) = apply_verbosity(text.as_ref(), &mut verbosity, command) {
                result?;
                continue;
            }
            match text.as_ref() {
                "--force" => {
                    force = true;
                    continue;
                }
                other => {
                    return Err(CliError::usage(format!(
                        "pyx: {command}: unsupported option '{other}'"
                    )))
                }
            }
        }
        if path.is_some() {
            return Err(CliError::usage(format!(
                "pyx: {command}: unexpected argument '{text}'"
            )));
        }
        path = Some(PathBuf::from(arg.as_os_str()));
    }

    let path = match path {
        Some(path) => path,
        None if path_required => {
            return Err(CliError::usage(format!(
                "pyx: {command}: missing destination path"
            )))
        }
        None => PathBuf::from("."),
    };
    Ok(ProjectOptions {
        path,
        force,
        verbosity,
    })
}

/// Parse the arguments of a command that accepts a list of paths.
///
/// Positional paths default to the current directory. Diagnostic flags are
/// accepted so callers can forward them to the compiler; `--check` is only
/// accepted when `allow_check` is set.
pub(crate) struct ScanOptions {
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) diagnostics: diag::Options,
    pub(crate) check: bool,
}

pub(crate) fn parse_scan(
    args: &[OsString],
    command: &str,
    allow_check: bool,
    verbosity: Verbosity,
) -> Result<ScanOptions, CliError> {
    let mut paths = Vec::new();
    let mut diagnostics = diag::Options {
        verbosity,
        ..diag::Options::default()
    };
    let mut check = false;
    let mut positional_only = false;

    for arg in args {
        let text = arg.to_string_lossy();
        if !positional_only && text == "--" {
            positional_only = true;
            continue;
        }
        if !positional_only && text.starts_with('-') && text != "-" {
            if allow_check && text == "--check" {
                check = true;
                continue;
            }
            if let Some(result) = apply_diag_flag(text.as_ref(), &mut diagnostics, command) {
                result?;
                continue;
            }
            return Err(CliError::usage(format!(
                "pyx: {command}: unsupported option '{text}'"
            )));
        }
        paths.push(PathBuf::from(arg.as_os_str()));
    }

    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }
    Ok(ScanOptions {
        paths,
        diagnostics,
        check,
    })
}

/// Parse `clean [--dry-run]` and the global verbosity switches.
pub(crate) fn parse_clean(
    args: &[OsString],
    verbosity: Verbosity,
) -> Result<(bool, Verbosity), CliError> {
    let mut dry_run = false;
    let mut verbosity = verbosity;
    for arg in args {
        let text = arg.to_string_lossy();
        if let Some(result) = apply_verbosity(text.as_ref(), &mut verbosity, "clean") {
            result?;
            continue;
        }
        match text.as_ref() {
            "--dry-run" => dry_run = true,
            other => {
                return Err(CliError::usage(format!(
                    "pyx: clean: unsupported option '{other}'"
                )))
            }
        }
    }
    Ok((dry_run, verbosity))
}

pub(crate) struct BuildOptions {
    pub(crate) file: PathBuf,
    pub(crate) output: Option<PathBuf>,
    pub(crate) runtime_lib: Option<PathBuf>,
    pub(crate) no_cache: bool,
    pub(crate) diagnostics: diag::Options,
}

pub(crate) fn parse_build(
    args: &[OsString],
    verbosity: Verbosity,
) -> Result<BuildOptions, CliError> {
    let mut file: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut runtime_lib: Option<PathBuf> = None;
    let mut no_cache = false;
    let mut diagnostics = diag::Options {
        verbosity,
        ..diag::Options::default()
    };
    let mut positional_only = false;
    let mut index = 0;

    while index < args.len() {
        let text = args[index].to_string_lossy();
        if !positional_only && text == "--" {
            positional_only = true;
            index += 1;
            continue;
        }
        if !positional_only {
            match text.as_ref() {
                "--no-cache" => {
                    no_cache = true;
                    index += 1;
                    continue;
                }
                "-o" | "--output" => {
                    let value = args.get(index + 1).ok_or_else(|| {
                        CliError::usage(format!(
                            "pyx: build: option '{text}' requires an output path"
                        ))
                    })?;
                    output = Some(PathBuf::from(value.as_os_str()));
                    index += 2;
                    continue;
                }
                "--runtime-lib" => {
                    let value = args.get(index + 1).ok_or_else(|| {
                        CliError::usage("pyx: build: option '--runtime-lib' requires a path")
                    })?;
                    runtime_lib = Some(PathBuf::from(value.as_os_str()));
                    index += 2;
                    continue;
                }
                other if other.starts_with('-') && other != "-" => {
                    if let Some(result) = apply_diag_flag(other, &mut diagnostics, "build") {
                        result?;
                        index += 1;
                        continue;
                    }
                    return Err(CliError::usage(format!(
                        "pyx: build: unsupported option '{other}'"
                    )));
                }
                _ => {}
            }
        }
        if file.is_some() {
            return Err(CliError::usage(format!(
                "pyx: build: unexpected argument '{}'",
                args[index].to_string_lossy()
            )));
        }
        file = Some(PathBuf::from(args[index].as_os_str()));
        index += 1;
    }

    let file = file.ok_or_else(|| CliError::usage("pyx: build: missing source file"))?;
    Ok(BuildOptions {
        file,
        output,
        runtime_lib,
        no_cache,
        diagnostics,
    })
}

/// Presentation and diagnostic options of the persistent interpreter
/// (`pyx repl`, `pyx -i`).
pub(crate) struct ReplOptions {
    /// Echo the value of a trailing top-level expression (`--no-echo`
    /// disables it).
    pub(crate) echo: bool,
    /// Print the version banner when the session is interactive
    /// (`--no-banner` disables it).
    pub(crate) banner: bool,
    /// Suppress the banner and prompts (`--quiet`, also a global flag).
    pub(crate) quiet: bool,
    pub(crate) diag: diag::Options,
}

impl ReplOptions {
    fn new(verbosity: Verbosity) -> ReplOptions {
        ReplOptions {
            echo: true,
            banner: true,
            quiet: verbosity.is_quiet(),
            diag: diag::Options {
                verbosity,
                ..diag::Options::default()
            },
        }
    }
}

/// Applies a REPL flag, including the diagnostic flags every command shares.
fn apply_repl_flag(
    text: &str,
    options: &mut ReplOptions,
    command: &str,
) -> Option<Result<(), CliError>> {
    match text {
        "--no-echo" => {
            options.echo = false;
            return Some(Ok(()));
        }
        "--no-banner" => {
            options.banner = false;
            return Some(Ok(()));
        }
        _ => {}
    }
    apply_diag_flag(text, &mut options.diag, command)
}

/// Parse `pyx repl [--no-echo] [--no-banner] [--quiet] [diagnostic options]`.
pub(crate) fn parse_repl_args(
    args: &[OsString],
    command: &str,
    verbosity: Verbosity,
) -> Result<ReplOptions, CliError> {
    let mut options = ReplOptions::new(verbosity);
    for arg in args {
        let text = arg.to_string_lossy();
        match apply_repl_flag(text.as_ref(), &mut options, command) {
            Some(result) => result?,
            None => {
                return Err(CliError::usage(format!(
                    "pyx: {command}: unexpected argument '{text}'"
                )))
            }
        }
    }
    options.quiet = options.diag.verbosity.is_quiet();
    Ok(options)
}

/// Source executed before an interactive session starts (`pyx -i ...`).
pub(crate) enum Prelude {
    /// `-c <code>`: compile and run `code` from `<pyx -c>`.
    Command(String),
    /// A script file, compiled and run before the REPL reads standard input.
    File(PathBuf),
}

/// A top-level `-c`/`-i` invocation, recognized before subcommand dispatch.
pub(crate) enum InterpreterRequest {
    /// `pyx -c <code> [args...]`: run `code` and exit. `sys.argv[0]` is
    /// `-c`.
    Command {
        code: String,
        program_args: Vec<String>,
        /// Diagnostics and echo/banner settings; echo is off for `-c`.
        options: ReplOptions,
    },
    /// `pyx -i [<file> | -c <code>] [args...]`: run the optional prelude and
    /// then enter the REPL with the same session.
    Interactive {
        prelude: Option<Prelude>,
        program_args: Vec<String>,
        options: ReplOptions,
    },
}

/// Recognizes `-c` and `-i` as the first argument after the global flags.
///
/// Returns `Ok(None)` for every other first argument so [`super::dispatch`]
/// can continue with the regular subcommands.
pub(crate) fn parse_interpreter_entry(
    args: &[OsString],
    verbosity: Verbosity,
) -> Result<Option<InterpreterRequest>, CliError> {
    let Some(first) = args.first() else {
        return Ok(None);
    };
    let first = first.to_string_lossy();
    match first.as_ref() {
        "-c" => {
            let code = args
                .get(1)
                .ok_or_else(|| CliError::usage("pyx: -c: missing code"))?;
            let mut options = ReplOptions::new(verbosity);
            // `-c` is not interactive: expression values are not echoed.
            options.echo = false;
            Ok(Some(InterpreterRequest::Command {
                code: code.to_string_lossy().into_owned(),
                program_args: program_args(&args[2..]),
                options,
            }))
        }
        "-i" => Ok(Some(parse_interactive(&args[1..], verbosity)?)),
        _ => Ok(None),
    }
}

fn program_args(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

/// Parses the arguments after `pyx -i`.
///
/// Flags come first, then either `-c <code>` or a script path; everything
/// after the code/script is a program argument. Without either, the session
/// starts empty.
fn parse_interactive(
    args: &[OsString],
    verbosity: Verbosity,
) -> Result<InterpreterRequest, CliError> {
    let mut options = ReplOptions::new(verbosity);
    let mut prelude = None;
    let mut program = Vec::new();
    let mut positional_only = false;
    let mut index = 0;
    while index < args.len() {
        let text = args[index].to_string_lossy();
        if !positional_only && text == "--" {
            positional_only = true;
            index += 1;
            continue;
        }
        if !positional_only && text == "-c" {
            let code = args
                .get(index + 1)
                .ok_or_else(|| CliError::usage("pyx: -i: option '-c' requires code"))?;
            prelude = Some(Prelude::Command(code.to_string_lossy().into_owned()));
            program = program_args(&args[index + 2..]);
            break;
        }
        if !positional_only && text.starts_with('-') && text != "-" {
            match apply_repl_flag(text.as_ref(), &mut options, "-i") {
                Some(result) => {
                    result?;
                    index += 1;
                    continue;
                }
                None => {
                    return Err(CliError::usage(format!(
                        "pyx: -i: unsupported option '{text}'"
                    )))
                }
            }
        }
        prelude = Some(Prelude::File(PathBuf::from(args[index].as_os_str())));
        program = program_args(&args[index + 1..]);
        break;
    }
    options.quiet = options.diag.verbosity.is_quiet();
    Ok(InterpreterRequest::Interactive {
        prelude,
        program_args: program,
        options,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_source_and_args, Verbosity};
    use std::path::PathBuf;

    fn args(values: &[&str]) -> Vec<std::ffi::OsString> {
        values.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn run_accepts_interpret_before_source() {
        let input = args(&["--interpret", "--color=never", "a.py", "--interpret"]);
        let (file, _options, program_args, interpret) =
            super::parse_run_args(&input, "run", Verbosity::Normal).unwrap();
        assert_eq!(file, PathBuf::from("a.py"));
        assert_eq!(program_args, ["--interpret"]);
        assert!(interpret);
    }

    #[test]
    fn run_forwards_arguments_after_source() {
        let (file, options, program_args) =
            parse_source_and_args(&args(&["a.py", "--x", "-h"]), "run", Verbosity::Normal).unwrap();
        assert_eq!(file, PathBuf::from("a.py"));
        assert!(!options.json);
        assert_eq!(program_args, ["--x", "-h"]);
    }

    #[test]
    fn run_accepts_separator_before_source() {
        let (file, _, program_args) =
            parse_source_and_args(&args(&["--", "a.py", "--x"]), "run", Verbosity::Normal).unwrap();
        assert_eq!(file, PathBuf::from("a.py"));
        assert_eq!(program_args, ["--x"]);
    }

    #[test]
    fn run_requires_source_file() {
        let input = args(&["--json"]);
        let error = parse_source_and_args(&input, "run", Verbosity::Normal).unwrap_err();
        assert_eq!(error.code, 2);
        assert!(error
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("missing source file"));
    }

    #[test]
    fn run_accepts_options_before_source() {
        let input = args(&["--json", "--color=never", "a.py", "--x"]);
        let (file, options, program_args) =
            parse_source_and_args(&input, "run", Verbosity::Normal).unwrap();
        assert_eq!(file, PathBuf::from("a.py"));
        assert!(options.json);
        assert_eq!(options.color, crate::cli::diag::ColorMode::Never);
        assert_eq!(program_args, ["--x"]);
    }

    #[test]
    fn run_rejects_unsupported_option_before_source() {
        let input = args(&["--bogus", "a.py"]);
        let error = parse_source_and_args(&input, "run", Verbosity::Normal).unwrap_err();
        assert_eq!(error.code, 2);
        assert!(error
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("unsupported option '--bogus'"));
    }
}
