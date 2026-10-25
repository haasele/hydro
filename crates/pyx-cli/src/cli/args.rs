//! Parsing of the flags shared by the subcommands.

use std::ffi::OsString;
use std::path::PathBuf;

use super::diag;
use super::CliError;

/// Applies a diagnostic-related flag and reports whether it was one.
///
/// Returns `None` for unrelated arguments; unsupported values are usage errors.
pub(crate) fn apply_diag_flag(
    text: &str,
    options: &mut diag::Options,
    command: &str,
) -> Option<Result<(), CliError>> {
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
) -> Result<(PathBuf, diag::Options), CliError> {
    let mut file: Option<PathBuf> = None;
    let mut positional_only = false;
    let mut options = diag::Options::default();
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
pub(crate) fn parse_source_and_args(
    args: &[OsString],
    command: &str,
) -> Result<(PathBuf, diag::Options, Vec<String>), CliError> {
    let mut file: Option<PathBuf> = None;
    let mut program_args = Vec::new();
    let mut positional_only = false;
    let mut options = diag::Options::default();
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
    Ok((file, options, program_args))
}

/// Options of `new`/`init`: one destination and the overwrite switch.
pub(crate) struct ProjectOptions {
    pub(crate) path: PathBuf,
    pub(crate) force: bool,
}

/// Parse `new <path> [--force]` / `init [path] [--force]`.
///
/// `path_required` is true for `new`; `init` defaults to the current directory.
pub(crate) fn parse_project(
    args: &[OsString],
    command: &str,
    path_required: bool,
) -> Result<ProjectOptions, CliError> {
    let mut path: Option<PathBuf> = None;
    let mut force = false;
    let mut positional_only = false;

    for arg in args {
        let text = arg.to_string_lossy();
        if !positional_only && text == "--" {
            positional_only = true;
            continue;
        }
        if !positional_only && text.starts_with('-') && text != "-" {
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
    Ok(ProjectOptions { path, force })
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
) -> Result<ScanOptions, CliError> {
    let mut paths = Vec::new();
    let mut diagnostics = diag::Options::default();
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

/// Parse `clean [--dry-run]`.
pub(crate) fn parse_clean(args: &[OsString]) -> Result<bool, CliError> {
    let mut dry_run = false;
    for arg in args {
        let text = arg.to_string_lossy();
        match text.as_ref() {
            "--dry-run" => dry_run = true,
            other => {
                return Err(CliError::usage(format!(
                    "pyx: clean: unsupported option '{other}'"
                )))
            }
        }
    }
    Ok(dry_run)
}

pub(crate) struct BuildOptions {
    pub(crate) file: PathBuf,
    pub(crate) output: Option<PathBuf>,
    pub(crate) runtime_lib: Option<PathBuf>,
    pub(crate) no_cache: bool,
    pub(crate) diagnostics: diag::Options,
}

pub(crate) fn parse_build(args: &[OsString]) -> Result<BuildOptions, CliError> {
    let mut file: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut runtime_lib: Option<PathBuf> = None;
    let mut no_cache = false;
    let mut diagnostics = diag::Options::default();
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

#[cfg(test)]
mod tests {
    use super::parse_source_and_args;
    use std::path::PathBuf;

    fn args(values: &[&str]) -> Vec<std::ffi::OsString> {
        values.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn run_forwards_arguments_after_source() {
        let (file, options, program_args) =
            parse_source_and_args(&args(&["a.py", "--x", "-h"]), "run").unwrap();
        assert_eq!(file, PathBuf::from("a.py"));
        assert!(!options.json);
        assert_eq!(program_args, ["--x", "-h"]);
    }

    #[test]
    fn run_accepts_separator_before_source() {
        let (file, _, program_args) =
            parse_source_and_args(&args(&["--", "a.py", "--x"]), "run").unwrap();
        assert_eq!(file, PathBuf::from("a.py"));
        assert_eq!(program_args, ["--x"]);
    }

    #[test]
    fn run_requires_source_file() {
        let error = parse_source_and_args(&args(&["--json"]), "run").unwrap_err();
        assert_eq!(error.code, 2);
        assert!(error
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("missing source file"));
    }

    #[test]
    fn run_accepts_options_before_source() {
        let (file, options, program_args) =
            parse_source_and_args(&args(&["--json", "--color=never", "a.py", "--x"]), "run")
                .unwrap();
        assert_eq!(file, PathBuf::from("a.py"));
        assert!(options.json);
        assert_eq!(options.color, crate::cli::diag::ColorMode::Never);
        assert_eq!(program_args, ["--x"]);
    }

    #[test]
    fn run_rejects_unsupported_option_before_source() {
        let error = parse_source_and_args(&args(&["--bogus", "a.py"]), "run").unwrap_err();
        assert_eq!(error.code, 2);
        assert!(error
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("unsupported option '--bogus'"));
    }
}
