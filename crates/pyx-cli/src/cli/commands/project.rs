//! `pyx new` and `pyx init`: create a minimal PyX project scaffold.
//!
//! Overwrite policy: without `--force`, any destination that already exists and
//! is non-empty is a conflict and nothing is written. Missing or empty files
//! are always (re)created. With `--force`, existing files are overwritten.
//! A directory at any scaffold path is always an error.

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use crate::cli::args::{parse_project, ProjectOptions};
use crate::cli::{CliError, CliResult};

const MAIN_PY: &str = "print(\"Hello, PyX!\")\n";
const GITIGNORE: &str = ".pyx/\n__pycache__/\n";

pub(crate) fn cmd_new(args: &[OsString]) -> CliResult {
    let options = parse_project(args, "new", true)?;
    if options.path.exists() {
        if !options.path.is_dir() {
            return Err(CliError::new(
                1,
                format!(
                    "pyx: new: '{}' already exists and is not a directory",
                    options.path.display()
                ),
            ));
        }
        if !options.force {
            return Err(CliError::new(
                1,
                format!(
                    "pyx: new: directory '{}' already exists \
                     (use --force to scaffold into it)",
                    options.path.display()
                ),
            ));
        }
    }
    fs::create_dir_all(&options.path).map_err(|error| {
        CliError::new(
            1,
            format!(
                "pyx: new: cannot create directory '{}': {error}",
                options.path.display()
            ),
        )
    })?;
    scaffold(&options.path, options.force)
}

pub(crate) fn cmd_init(args: &[OsString]) -> CliResult {
    let options: ProjectOptions = parse_project(args, "init", false)?;
    if !options.path.is_dir() {
        return Err(CliError::new(
            1,
            format!(
                "pyx: init: '{}' is not an existing directory",
                options.path.display()
            ),
        ));
    }
    scaffold(&options.path, options.force)
}

fn scaffold(directory: &Path, force: bool) -> CliResult {
    let name = project_name(directory);
    let files = [
        ("main.py", MAIN_PY.to_string()),
        ("pyproject.toml", pyproject(&name)),
        (".gitignore", GITIGNORE.to_string()),
    ];

    // Check every destination first so a refusal leaves the directory untouched.
    let mut conflicts = Vec::new();
    for (file, _) in &files {
        let path = directory.join(file);
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_dir() => conflicts.push(path),
            Ok(metadata) if !force && metadata.len() > 0 => conflicts.push(path),
            _ => {}
        }
    }
    if !conflicts.is_empty() {
        let mut list = String::new();
        for (index, path) in conflicts.iter().enumerate() {
            if index > 0 {
                list.push_str(", ");
            }
            list.push_str(&path.display().to_string());
        }
        return Err(CliError::new(
            1,
            format!(
                "pyx: refusing to overwrite existing file(s): {list} \
                 (use --force to overwrite)"
            ),
        ));
    }

    for (file, contents) in &files {
        let path = directory.join(file);
        fs::write(&path, contents).map_err(|error| {
            CliError::new(
                1,
                format!("pyx: cannot write '{}': {error}", path.display()),
            )
        })?;
        println!("created {}", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

/// Derives a PEP 503-style project name from the directory name.
fn project_name(directory: &Path) -> String {
    let raw = directory
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .or_else(|| {
            fs::canonicalize(directory).ok().and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
        })
        .unwrap_or_default();

    // Collapse every run of non-alphanumeric characters into one dash.
    let mut clean = String::new();
    let mut separated = false;
    for character in raw.chars() {
        if character.is_ascii_alphanumeric() {
            if separated && !clean.is_empty() {
                clean.push('-');
            }
            clean.push(character.to_ascii_lowercase());
            separated = false;
        } else {
            separated = true;
        }
    }
    while clean.ends_with('-') {
        clean.pop();
    }
    if clean.is_empty() {
        String::from("pyx-project")
    } else {
        clean
    }
}

fn pyproject(name: &str) -> String {
    format!("[project]\nname = \"{name}\"\nversion = \"0.1.0\"\nrequires-python = \">=3.10\"\n")
}
