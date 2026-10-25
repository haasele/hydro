//! `shutil`: filesystem copy/move helpers used by the benchmarks.
//!
//! Only the file-oriented subset is implemented: `which`,
//! `get_terminal_size`, `copyfile`, `copy`, `copy2`, `rmtree` and `move`.
//! Paths are strings or `pathlib.Path` objects; every other argument type is
//! rejected with `TypeError`, and filesystem failures surface as `OSError`
//! with Python's `[Errno N] message: 'path'` rendering.
//!
//! `copy2` preserves permissions (and file access/modification times where
//! the platform exposes them); `copy` copies contents and permissions.
//! Directory copies recurse and reproduce symlinks as symlinks. Special
//! files (sockets, fifos, devices) are rejected by the entry points and
//! skipped while walking a directory, matching `shutil.copytree`'s
//! `ignore_dangling_symlinks`-style tolerance for unsupported entries.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::exc::{self, ATTRIBUTE_ERROR, OS_ERROR, TYPE_ERROR};
use crate::value::{self, NONE};

/// Raises `OSError` with a literal message and returns the dummy value.
fn os_error_message(message: &str) -> i64 {
    exc::raise_with_message(OS_ERROR, message)
}

/// Raises `OSError` for a failed filesystem operation on `path`.
fn io_error(error: io::Error, path: &Path) -> i64 {
    let path = path.to_string_lossy();
    let text = error.to_string();
    match error.raw_os_error() {
        Some(code) => {
            let detail = text.split(" (os error").next().unwrap_or(text.as_str());
            exc::raise_with_message(OS_ERROR, &format!("[Errno {code}] {detail}: '{path}'"))
        }
        None => exc::raise_with_message(OS_ERROR, &format!("{text}: '{path}'")),
    }
}

/// Reads an optional path-like argument; `None`/missing select the default.
fn optional_path(
    args: &[i64],
    index: usize,
    function: &str,
    name: &str,
) -> Result<Option<String>, i64> {
    match args.get(index).copied() {
        None | Some(NONE) => Ok(None),
        Some(arg) => match value::as_path_like(arg) {
            Some(text) => Ok(Some(text.to_string())),
            None => Err(exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "{function}() argument '{name}' must be str or pathlib.Path, not {}",
                    value::type_name(arg)
                ),
            )),
        },
    }
}

/// Reads a required path-like argument.
fn required_path(args: &[i64], index: usize, function: &str, name: &str) -> Result<String, i64> {
    match optional_path(args, index, function, name)? {
        Some(text) => Ok(text),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!("{function}() missing required argument: '{name}'"),
        )),
    }
}

/// Reads an optional string argument (`which`'s `path` is not a path-like
/// value: it is the PATH list itself).
fn optional_text(
    args: &[i64],
    index: usize,
    function: &str,
    name: &str,
) -> Result<Option<String>, i64> {
    match args.get(index).copied() {
        None | Some(NONE) => Ok(None),
        Some(arg) => match value::as_str(arg) {
            Some(text) => Ok(Some(text.to_string())),
            None => Err(exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "{function}() argument '{name}' must be str, not {}",
                    value::type_name(arg)
                ),
            )),
        },
    }
}

/// Reads an optional integer argument.
fn optional_int(
    args: &[i64],
    index: usize,
    function: &str,
    name: &str,
) -> Result<Option<i64>, i64> {
    match args.get(index).copied() {
        None | Some(NONE) => Ok(None),
        Some(arg) => match value::as_integer(arg) {
            Some(number) => Ok(Some(number)),
            None => Err(exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "{function}() argument '{name}' must be int, not {}",
                    value::type_name(arg)
                ),
            )),
        },
    }
}

/// Reads an optional boolean flag (`None` selects `false`).
fn optional_flag(args: &[i64], index: usize) -> bool {
    match args.get(index).copied() {
        None | Some(NONE) => false,
        Some(arg) => value::truthy(arg),
    }
}

/// Encodes a non-negative terminal dimension.
fn encode_size(size: i64) -> i64 {
    value::encode_int(size).unwrap_or(NONE)
}

/// `shutil.which(cmd, mode=1, path=None)`.
fn which(args: &[i64]) -> i64 {
    let command = match optional_text(args, 0, "which", "cmd") {
        Ok(Some(command)) => command,
        Ok(None) => return NONE,
        Err(error) => return error,
    };
    let mode = match optional_int(args, 1, "which", "mode") {
        Ok(Some(mode)) => mode,
        Ok(None) => 1,
        Err(error) => return error,
    };
    if command.is_empty() {
        return NONE;
    }
    let path = match optional_text(args, 2, "which", "path") {
        Ok(path) => path,
        Err(error) => return error,
    };
    let has_separator = command.contains(std::path::MAIN_SEPARATOR)
        || (std::path::MAIN_SEPARATOR != '/' && command.contains('/'));
    if has_separator {
        let candidate = PathBuf::from(&command);
        return if access_ok(&candidate, mode) {
            value::str_alloc(command)
        } else {
            NONE
        };
    }
    let path = path.unwrap_or_else(|| env::var("PATH").unwrap_or_default());
    for directory in env::split_paths(&path) {
        let candidate = directory.join(&command);
        if access_ok(&candidate, mode) {
            return value::str_alloc(candidate.to_string_lossy().into_owned());
        }
    }
    NONE
}

/// `os.access`-style check: existence plus the requested mode bits.
fn access_ok(path: &Path, mode: i64) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    if mode == 0 {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bits = metadata.permissions().mode();
        if mode & 1 != 0 && bits & 0o111 == 0 {
            return false;
        }
        if mode & 2 != 0 && bits & 0o222 == 0 {
            return false;
        }
        if mode & 4 != 0 && bits & 0o444 == 0 {
            return false;
        }
        true
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        true
    }
}

/// Parses a `(columns, lines)` fallback pair.
fn fallback_pair(fallback: i64) -> Result<(i64, i64), i64> {
    let items = match value::as_tuple(fallback) {
        Some(items) => items,
        None => match value::as_list(fallback) {
            Some(items) => items,
            None => {
                return Err(exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "get_terminal_size() fallback must be a 2-tuple, not {}",
                        value::type_name(fallback)
                    ),
                ))
            }
        },
    };
    if items.len() != 2 {
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            "get_terminal_size() fallback must be a 2-tuple",
        ));
    }
    let mut sizes = [0i64; 2];
    for (slot, item) in sizes.iter_mut().zip(items) {
        let Some(number) = value::as_integer(*item) else {
            return Err(exc::raise_with_message(
                TYPE_ERROR,
                "get_terminal_size() fallback entries must be int",
            ));
        };
        if number < 0 {
            return Err(exc::raise_with_message(
                TYPE_ERROR,
                "get_terminal_size() fallback entries must be non-negative",
            ));
        }
        *slot = number;
    }
    Ok((sizes[0], sizes[1]))
}

/// Reads a positive dimension from the environment.
fn environment_size(name: &str) -> Option<i64> {
    let text = env::var(name).ok()?;
    let number = text.trim().parse::<i64>().ok()?;
    (number > 0).then_some(number)
}

/// `shutil.get_terminal_size(fallback=None)` returns a namespace with
/// `columns` and `lines`.
fn get_terminal_size(args: &[i64]) -> i64 {
    let fallback = match args.first().copied() {
        None | Some(NONE) => (80, 24),
        Some(arg) => match fallback_pair(arg) {
            Ok(pair) => pair,
            Err(error) => return error,
        },
    };
    let columns = environment_size("COLUMNS").unwrap_or(fallback.0);
    let lines = environment_size("LINES").unwrap_or(fallback.1);
    value::namespace_new(vec![
        ("columns".to_string(), encode_size(columns)),
        ("lines".to_string(), encode_size(lines)),
    ])
}

/// Copies a symlink itself.
#[cfg(unix)]
fn copy_symlink(from: &Path, to: &Path) -> io::Result<()> {
    let target = fs::read_link(from)?;
    let _ = fs::remove_file(to);
    std::os::unix::fs::symlink(target, to)
}

#[cfg(windows)]
fn copy_symlink(from: &Path, to: &Path) -> io::Result<()> {
    let target = fs::read_link(from)?;
    let _ = fs::remove_file(to);
    if fs::metadata(from)
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
    {
        std::os::windows::fs::symlink_dir(target, to)
    } else {
        std::os::windows::fs::symlink_file(target, to)
    }
}

#[cfg(not(any(unix, windows)))]
fn copy_symlink(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlinks are not supported on this platform",
    ))
}

/// Applies permissions and times from `metadata` to a regular file.
fn copy_metadata(metadata: &fs::Metadata, path: &Path) -> io::Result<()> {
    let accessed = metadata.accessed().ok();
    let modified = metadata.modified().ok();
    if accessed.is_some() || modified.is_some() {
        let mut times = fs::FileTimes::new();
        if let Some(accessed) = accessed {
            times = times.set_accessed(accessed);
        }
        if let Some(modified) = modified {
            times = times.set_modified(modified);
        }
        // A read-only handle is enough for `futimens`/`SetFileTime`; opening
        // for writing would fail when the source mode is read-only. Time
        // restoration is best-effort: an unreadable file still copies.
        if let Ok(file) = fs::File::open(path) {
            let _ = file.set_times(times);
        }
    }
    // Apply the mode last: a read-only source mode must not make the
    // destination unopenable for the timestamp update above.
    fs::set_permissions(path, metadata.permissions())
}

/// Whether both paths refer to the same existing file.
#[cfg(unix)]
fn same_file(from: &Path, to: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(from), fs::metadata(to)) {
        (Ok(from), Ok(to)) => from.dev() == to.dev() && from.ino() == to.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(from: &Path, to: &Path) -> bool {
    match (fs::canonicalize(from), fs::canonicalize(to)) {
        (Ok(from), Ok(to)) => from == to,
        _ => false,
    }
}

/// Rejects an existing destination that is not a regular file or directory
/// (a fifo/device/socket would block or fail inside `fs::copy`).
fn reject_special_destination(path: &Path) -> Option<i64> {
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() && !metadata.is_dir() => Some(os_error_message(
            &format!("cannot overwrite special file '{}'", path.to_string_lossy()),
        )),
        _ => None,
    }
}

/// Copies a regular file, optionally preserving its metadata.
fn copy_regular_file(from: &Path, to: &Path, preserve: bool) -> io::Result<()> {
    if same_file(from, to) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source and destination are the same file",
        ));
    }
    fs::copy(from, to)?;
    if preserve {
        if let Ok(metadata) = fs::metadata(from) {
            copy_metadata(&metadata, to)?;
        }
    }
    Ok(())
}

/// Whether `target` resolves inside `source` (guards a directory copy into
/// its own subtree, which would recurse forever).
fn is_inside(source: &Path, target: &Path) -> bool {
    let Ok(source) = fs::canonicalize(source) else {
        return false;
    };
    let resolved = match fs::canonicalize(target) {
        Ok(target) => target,
        Err(_) => {
            let Some(parent) = target.parent() else {
                return false;
            };
            let Ok(parent) = fs::canonicalize(parent) else {
                return false;
            };
            match target.file_name() {
                Some(name) => parent.join(name),
                None => return false,
            }
        }
    };
    resolved.starts_with(&source)
}

/// Recursively copies `from` into `to`.
fn copy_tree(from: &Path, to: &Path, preserve: bool) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            copy_symlink(&source, &target)?;
        } else if file_type.is_dir() {
            copy_tree(&source, &target, preserve)?;
        } else if file_type.is_file() {
            copy_regular_file(&source, &target, preserve)?;
        }
        // Sockets, fifos and device nodes are not reproducible by a plain
        // byte copy; skip them rather than aborting the whole tree.
    }
    // Apply the directory mode after populating it: a read-only source
    // directory must not block writing its children.
    if preserve {
        if let Ok(metadata) = fs::metadata(from) {
            let _ = fs::set_permissions(to, metadata.permissions());
        }
    }
    Ok(())
}

/// Removes `path` recursively, treating a missing path as success.
fn remove_path(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let result = if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Resolves the final destination for `src` copied/moved to `dst`: an
/// existing directory receives the source's base name.
fn destination_target(src: &Path, dst: &Path) -> Result<PathBuf, i64> {
    match fs::metadata(dst) {
        Ok(metadata) if metadata.is_dir() => {
            let Some(name) = src.file_name() else {
                return Err(os_error_message(&format!(
                    "invalid source path '{}'",
                    src.to_string_lossy()
                )));
            };
            Ok(dst.join(name))
        }
        Ok(_) => {
            if fs::metadata(src).map(|meta| meta.is_dir()).unwrap_or(false) {
                Err(os_error_message(&format!(
                    "cannot overwrite non-directory '{}' with directory '{}'",
                    dst.to_string_lossy(),
                    src.to_string_lossy()
                )))
            } else {
                Ok(dst.to_path_buf())
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(dst.to_path_buf()),
        Err(error) => Err(io_error(error, dst)),
    }
}

/// `shutil.copyfile(src, dst)`.
fn copyfile(args: &[i64]) -> i64 {
    let source = match required_path(args, 0, "copyfile", "src") {
        Ok(path) => path,
        Err(error) => return error,
    };
    let destination = match required_path(args, 1, "copyfile", "dst") {
        Ok(path) => path,
        Err(error) => return error,
    };
    let source_path = Path::new(&source);
    let destination_path = Path::new(&destination);
    match fs::metadata(source_path) {
        Ok(metadata) if metadata.is_dir() => {
            return os_error_message(&format!(
                "[Errno 21] Is a directory: '{}'",
                source_path.to_string_lossy()
            ))
        }
        Ok(metadata) if !metadata.is_file() => {
            return os_error_message(&format!(
                "cannot copy special file '{}'",
                source_path.to_string_lossy()
            ))
        }
        Ok(_) => {}
        Err(error) => return io_error(error, source_path),
    }
    if let Some(error) = reject_special_destination(destination_path) {
        return error;
    }
    match copy_regular_file(source_path, destination_path, false) {
        Ok(()) => value::str_alloc(destination),
        Err(error) => io_error(error, destination_path),
    }
}

/// Shared implementation of `copy` (`preserve == false`) and `copy2`.
fn copy_entry(args: &[i64], function: &str, preserve: bool) -> i64 {
    let source = match required_path(args, 0, function, "src") {
        Ok(path) => path,
        Err(error) => return error,
    };
    let destination = match required_path(args, 1, function, "dst") {
        Ok(path) => path,
        Err(error) => return error,
    };
    let source_path = Path::new(&source);
    let destination_path = Path::new(&destination);
    let metadata = match fs::metadata(source_path) {
        Ok(metadata) => metadata,
        Err(error) => return io_error(error, source_path),
    };
    if !metadata.is_file() && !metadata.is_dir() {
        return os_error_message(&format!(
            "cannot copy special file '{}'",
            source_path.to_string_lossy()
        ));
    }
    let target = match destination_target(source_path, destination_path) {
        Ok(target) => target,
        Err(error) => return error,
    };
    if let Some(error) = reject_special_destination(&target) {
        return error;
    }
    let result = if metadata.is_dir() {
        if is_inside(source_path, &target) {
            return os_error_message(&format!(
                "cannot copy directory '{}' into itself '{}'",
                source_path.to_string_lossy(),
                target.to_string_lossy()
            ));
        }
        copy_tree(source_path, &target, preserve)
    } else {
        copy_regular_file(source_path, &target, preserve)
    };
    match result {
        Ok(()) => value::str_alloc(target.to_string_lossy().into_owned()),
        Err(error) => io_error(error, &target),
    }
}

/// `shutil.rmtree(path, ignore_errors=False)`.
fn rmtree(args: &[i64]) -> i64 {
    let path = match required_path(args, 0, "rmtree", "path") {
        Ok(path) => path,
        Err(error) => return error,
    };
    let ignore_errors = optional_flag(args, 1);
    let path = Path::new(&path);
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return if ignore_errors {
                NONE
            } else {
                io_error(error, path)
            }
        }
        Err(error) => return io_error(error, path),
    };
    if !metadata.is_dir() {
        if ignore_errors {
            return NONE;
        }
        return os_error_message(&format!(
            "[Errno 20] Not a directory: '{}'",
            path.to_string_lossy()
        ));
    }
    match fs::remove_dir_all(path) {
        Ok(()) => NONE,
        Err(_) if ignore_errors => NONE,
        Err(error) => io_error(error, path),
    }
}

/// Cross-device rename failures fall back to copy + delete.
#[cfg(unix)]
fn is_cross_device(error: &io::Error) -> bool {
    error.raw_os_error() == Some(18) // EXDEV
}

#[cfg(windows)]
fn is_cross_device(error: &io::Error) -> bool {
    error.raw_os_error() == Some(17) // ERROR_NOT_SAME_DEVICE
}

#[cfg(not(any(unix, windows)))]
fn is_cross_device(_error: &io::Error) -> bool {
    false
}

/// `shutil.move(src, dst)`.
fn move_entry(args: &[i64]) -> i64 {
    let source = match required_path(args, 0, "move", "src") {
        Ok(path) => path,
        Err(error) => return error,
    };
    let destination = match required_path(args, 1, "move", "dst") {
        Ok(path) => path,
        Err(error) => return error,
    };
    let source_path = Path::new(&source);
    let destination_path = Path::new(&destination);
    let metadata = match fs::symlink_metadata(source_path) {
        Ok(metadata) => metadata,
        Err(error) => return io_error(error, source_path),
    };
    if !metadata.is_file() && !metadata.is_dir() && !metadata.file_type().is_symlink() {
        return os_error_message(&format!(
            "cannot move special file '{}'",
            source_path.to_string_lossy()
        ));
    }
    let target = match destination_target(source_path, destination_path) {
        Ok(target) => target,
        Err(error) => return error,
    };
    match fs::rename(source_path, &target) {
        Ok(()) => return value::str_alloc(target.to_string_lossy().into_owned()),
        Err(error) if !is_cross_device(&error) => return io_error(error, source_path),
        Err(_) => {}
    }
    // Different filesystems: copy the tree/file (with metadata) and remove
    // the source once the copy succeeded.
    let copied = if metadata.is_dir() {
        copy_tree(source_path, &target, true)
    } else if metadata.file_type().is_symlink() {
        copy_symlink(source_path, &target)
    } else {
        copy_regular_file(source_path, &target, true)
    };
    if let Err(error) = copied {
        let _ = remove_path(&target);
        return io_error(error, &target);
    }
    match remove_path(source_path) {
        Ok(()) => value::str_alloc(target.to_string_lossy().into_owned()),
        Err(error) => io_error(error, source_path),
    }
}

/// Dispatches a native `shutil` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "which" => which(args),
        "get_terminal_size" => get_terminal_size(args),
        "copyfile" => copyfile(args),
        "copy" => copy_entry(args, "copy", false),
        "copy2" => copy_entry(args, "copy2", true),
        "rmtree" => rmtree(args),
        "move" => move_entry(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'shutil' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    use super::*;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn text(item: &str) -> i64 {
        value::str_alloc(item.to_string())
    }

    fn temp_root(tag: &str) -> PathBuf {
        let count = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("pyx-shutil-{}-{tag}-{count}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test root");
        path
    }

    fn error_tag(result: i64) -> i64 {
        assert_eq!(result, NONE);
        assert!(exc::pending(), "expected an exception");
        let tag = exc::pending_tag();
        exc::clear();
        tag
    }

    #[cfg(unix)]
    #[test]
    fn which_honours_the_executable_bit() {
        let root = temp_root("which");
        let tool = root.join("tool");
        fs::write(&tool, b"#!/bin/sh\n").expect("write tool");
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).expect("chmod");

        let path_arg = text(&root.to_string_lossy());
        let result = call("which", &[text("tool"), encode_size(1), path_arg]);
        let expected = tool.to_string_lossy().into_owned();
        assert_eq!(value::as_str(result), Some(expected.as_str()));

        fs::set_permissions(&tool, fs::Permissions::from_mode(0o644)).expect("chmod");
        let path_arg = text(&root.to_string_lossy());
        let result = call("which", &[text("tool"), encode_size(1), path_arg]);
        assert_eq!(result, NONE);
        assert!(!exc::pending());

        // Mode 0 only requires existence.
        let path_arg = text(&root.to_string_lossy());
        let result = call("which", &[text("tool"), encode_size(0), path_arg]);
        assert_eq!(value::as_str(result), Some(expected.as_str()));
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn which_checks_direct_paths_and_missing_commands() {
        let root = temp_root("which-direct");
        let tool = root.join("direct-tool");
        fs::write(&tool, b"payload").expect("write tool");
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).expect("chmod");
        let direct = tool.to_string_lossy().into_owned();
        let result = call("which", &[text(&direct)]);
        assert_eq!(value::as_str(result), Some(direct.as_str()));

        let path_arg = text(&root.to_string_lossy());
        let result = call("which", &[text("absent-tool"), encode_size(1), path_arg]);
        assert_eq!(result, NONE);
        assert!(!exc::pending());
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn terminal_size_uses_environment_then_fallback() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous_columns = env::var("COLUMNS").ok();
        let previous_lines = env::var("LINES").ok();
        env::remove_var("COLUMNS");
        env::remove_var("LINES");

        let fallback = value::tuple_new(vec![encode_size(111), encode_size(42)]);
        let result = call("get_terminal_size", &[fallback]);
        let pairs = value::as_namespace(result).expect("terminal size namespace");
        let columns = pairs
            .iter()
            .find(|(name, _)| name.as_str() == "columns")
            .map(|(_, value)| *value)
            .expect("columns");
        let lines = pairs
            .iter()
            .find(|(name, _)| name.as_str() == "lines")
            .map(|(_, value)| *value)
            .expect("lines");
        assert_eq!(value::as_integer(columns), Some(111));
        assert_eq!(value::as_integer(lines), Some(42));

        env::set_var("COLUMNS", "132");
        env::set_var("LINES", "43");
        let result = call("get_terminal_size", &[]);
        let pairs = value::as_namespace(result).expect("terminal size namespace");
        let columns = pairs
            .iter()
            .find(|(name, _)| name.as_str() == "columns")
            .map(|(_, value)| *value)
            .expect("columns");
        let lines = pairs
            .iter()
            .find(|(name, _)| name.as_str() == "lines")
            .map(|(_, value)| *value)
            .expect("lines");
        assert_eq!(value::as_integer(columns), Some(132));
        assert_eq!(value::as_integer(lines), Some(43));

        match previous_columns {
            Some(columns) => env::set_var("COLUMNS", columns),
            None => env::remove_var("COLUMNS"),
        }
        match previous_lines {
            Some(lines) => env::set_var("LINES", lines),
            None => env::remove_var("LINES"),
        }
    }

    #[test]
    fn terminal_size_rejects_a_bad_fallback() {
        let result = call(
            "get_terminal_size",
            &[value::tuple_new(vec![encode_size(1)])],
        );
        assert_eq!(error_tag(result), TYPE_ERROR as i64);
    }

    #[test]
    fn copyfile_copies_contents_and_rejects_directories() {
        let root = temp_root("copyfile");
        let source = root.join("source.txt");
        let destination = root.join("destination.txt");
        fs::write(&source, b"hello").expect("write source");
        let source_arg = text(&source.to_string_lossy());
        let destination_arg = text(&destination.to_string_lossy());
        let result = call("copyfile", &[source_arg, destination_arg]);
        assert!(value::as_str(result).is_some());
        assert_eq!(fs::read(&destination).expect("read copy"), b"hello");

        let directory = root.join("subdir");
        fs::create_dir(&directory).expect("create subdir");
        let directory_arg = text(&directory.to_string_lossy());
        let target_arg = text(&root.join("out.txt").to_string_lossy());
        let result = call("copyfile", &[directory_arg, target_arg]);
        assert_eq!(error_tag(result), OS_ERROR as i64);

        // Copying a file onto itself must not truncate it.
        let source_arg = text(&source.to_string_lossy());
        let target_arg = text(&source.to_string_lossy());
        assert_eq!(
            error_tag(call("copyfile", &[source_arg, target_arg])),
            OS_ERROR as i64
        );
        assert_eq!(fs::read(&source).expect("read source"), b"hello");
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn copy_directory_into_existing_directory() {
        let root = temp_root("copy-dir");
        let source = root.join("source");
        fs::create_dir(&source).expect("create source");
        fs::write(source.join("file.txt"), b"payload").expect("write file");
        fs::create_dir(source.join("nested")).expect("create nested");
        fs::write(source.join("nested").join("deep.txt"), b"deep").expect("write deep");
        let destination = root.join("destination");
        fs::create_dir(&destination).expect("create destination");

        let source_arg = text(&source.to_string_lossy());
        let destination_arg = text(&destination.to_string_lossy());
        let result = call("copy", &[source_arg, destination_arg]);
        assert!(!exc::pending(), "copy raised");
        let target = destination.join("source");
        let expected = target.to_string_lossy().into_owned();
        assert_eq!(value::as_str(result), Some(expected.as_str()));
        assert_eq!(
            fs::read(target.join("file.txt")).expect("read copy"),
            b"payload"
        );
        assert_eq!(
            fs::read(target.join("nested").join("deep.txt")).expect("read deep"),
            b"deep"
        );

        // Copying a directory into its own subtree must fail instead of
        // recursing forever.
        let inside = source.join("inner");
        let source_arg = text(&source.to_string_lossy());
        let inside_arg = text(&inside.to_string_lossy());
        assert_eq!(
            error_tag(call("copy", &[source_arg, inside_arg])),
            OS_ERROR as i64
        );
        assert!(!inside.exists());
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn copy2_preserves_permissions() {
        let root = temp_root("copy2");
        let source = root.join("mode.txt");
        fs::write(&source, b"mode").expect("write source");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).expect("chmod");
        let destination = root.join("copied.txt");
        let source_arg = text(&source.to_string_lossy());
        let destination_arg = text(&destination.to_string_lossy());
        let result = call("copy2", &[source_arg, destination_arg]);
        assert!(!exc::pending(), "copy2 raised");
        assert_ne!(result, NONE);
        let mode = fs::metadata(&destination)
            .expect("copied metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o640);
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn rmtree_removes_trees_and_rejects_files() {
        let root = temp_root("rmtree");
        let tree = root.join("tree");
        fs::create_dir_all(tree.join("a").join("b")).expect("create tree");
        fs::write(tree.join("a").join("b").join("leaf.txt"), b"leaf").expect("write leaf");
        let tree_arg = text(&tree.to_string_lossy());
        assert_eq!(call("rmtree", &[tree_arg]), NONE);
        assert!(!tree.exists());

        let file = root.join("file.txt");
        fs::write(&file, b"data").expect("write file");
        let file_arg = text(&file.to_string_lossy());
        assert_eq!(error_tag(call("rmtree", &[file_arg])), OS_ERROR as i64);

        let file_arg = text(&file.to_string_lossy());
        let true_arg = value::TRUE;
        assert_eq!(call("rmtree", &[file_arg, true_arg]), NONE);
        assert!(!exc::pending());
        assert!(file.exists());

        let missing = root.join("missing");
        let missing_arg = text(&missing.to_string_lossy());
        assert_eq!(error_tag(call("rmtree", &[missing_arg])), OS_ERROR as i64);
        let missing_arg = text(&missing.to_string_lossy());
        assert_eq!(call("rmtree", &[missing_arg, value::TRUE]), NONE);
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn move_renames_files_and_directories() {
        let root = temp_root("move");
        let source = root.join("source.txt");
        fs::write(&source, b"moving").expect("write source");
        let destination = root.join("destination.txt");
        let source_arg = text(&source.to_string_lossy());
        let destination_arg = text(&destination.to_string_lossy());
        let result = call("move", &[source_arg, destination_arg]);
        assert!(!exc::pending(), "move raised");
        assert_eq!(
            value::as_str(result),
            Some(destination.to_string_lossy().into_owned().as_str())
        );
        assert!(!source.exists());
        assert_eq!(fs::read(&destination).expect("read moved"), b"moving");

        let directory = root.join("directory");
        fs::create_dir(&directory).expect("create directory");
        fs::write(directory.join("inner.txt"), b"inner").expect("write inner");
        let holding = root.join("holding");
        fs::create_dir(&holding).expect("create holding");
        let directory_arg = text(&directory.to_string_lossy());
        let holding_arg = text(&holding.to_string_lossy());
        let _ = call("move", &[directory_arg, holding_arg]);
        assert!(!exc::pending(), "move directory raised");
        assert!(!directory.exists());
        assert_eq!(
            fs::read(holding.join("directory").join("inner.txt")).expect("read moved inner"),
            b"inner"
        );
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn path_arguments_reject_other_types() {
        let result = call("rmtree", &[value::encode_int(1).expect("small int")]);
        assert_eq!(error_tag(result), TYPE_ERROR as i64);
    }

    #[test]
    fn unknown_function_reports_attribute_error() {
        let result = call("copytree", &[]);
        assert_eq!(error_tag(result), ATTRIBUTE_ERROR as i64);
    }
}
