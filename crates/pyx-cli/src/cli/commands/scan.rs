//! Recursive file discovery shared by `test`, `fmt` and `lint`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::CliError;

/// Directory names that are never descended into during discovery.
const SKIPPED_DIRS: [&str; 3] = ["target", "__pycache__", "node_modules"];

/// Collects the files named by `paths`.
///
/// Explicit file arguments are always returned. Directories are walked
/// recursively, keeping files whose name satisfies `matches_name`. Hidden
/// entries and [`SKIPPED_DIRS`] are ignored. The result is sorted and free of
/// duplicates so command output and execution order are deterministic.
pub(crate) fn collect_files(
    paths: &[PathBuf],
    matches_name: impl Fn(&str) -> bool,
) -> Result<Vec<PathBuf>, CliError> {
    let mut files = Vec::new();
    for path in paths {
        let metadata = fs::metadata(path).map_err(|error| {
            CliError::new(
                1,
                format!("pyx: cannot access '{}': {error}", path.display()),
            )
        })?;
        if metadata.is_dir() {
            walk(path, &matches_name, &mut files)?;
        } else {
            files.push(path.clone());
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn walk(
    directory: &Path,
    matches_name: &impl Fn(&str) -> bool,
    files: &mut Vec<PathBuf>,
) -> Result<(), CliError> {
    let entries = fs::read_dir(directory).map_err(|error| {
        CliError::new(
            1,
            format!(
                "pyx: cannot read directory '{}': {error}",
                directory.display()
            ),
        )
    })?;

    let mut children = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            CliError::new(
                1,
                format!(
                    "pyx: cannot read directory '{}': {error}",
                    directory.display()
                ),
            )
        })?;
        children.push(entry.path());
    }
    children.sort();

    for child in children {
        let Some(name) = child
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        // Symlinks are skipped: following them could escape the tree or loop.
        let file_type = match fs::symlink_metadata(&child) {
            Ok(metadata) => metadata.file_type(),
            Err(_) => continue,
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if SKIPPED_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(&child, matches_name, files)?;
        } else if file_type.is_file() && matches_name(&name) {
            files.push(child);
        }
    }
    Ok(())
}

/// Whether `name` looks like a test module: `test_*.py` or `*_test.py`.
pub(crate) fn is_test_file(name: &str) -> bool {
    name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
}

/// Whether `name` is a Python source file.
pub(crate) fn is_python_file(name: &str) -> bool {
    name.ends_with(".py")
}
