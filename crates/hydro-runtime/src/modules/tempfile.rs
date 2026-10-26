//! `tempfile`: temporary directory helpers.
//!
//! Only the benchmark-critical subset is implemented: `gettempdir()`,
//! `mkdtemp()` and the `TemporaryDirectory` context manager. Unique names are
//! derived from the process id and a process-global counter, which is enough
//! to keep concurrent callers (and parallel tests) apart without pulling in a
//! random-number dependency.
//!
//! `TemporaryDirectory` is a native class instance: the constructor allocates
//! the directory and stores its path in the `name` attribute, while
//! `__enter__`/`__exit__`/`cleanup` are native methods. Cleanup removes the
//! tree recursively and treats an already-missing path as success. The
//! `name` attribute is kept after cleanup, so callers can still inspect the
//! removed path.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

use crate::exc::{self, ATTRIBUTE_ERROR, OS_ERROR, TYPE_ERROR};
use crate::value::{self, NONE};

/// Source of unique name components within one process.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Resolved temporary directory, computed once like CPython.
static TEMP_DIR: LazyLock<String> = LazyLock::new(|| {
    for key in ["TMPDIR", "TEMP", "TMP"] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() && Path::new(&value).is_dir() {
                return value;
            }
        }
    }
    if cfg!(windows) {
        if let Ok(profile) = std::env::var("USERPROFILE") {
            if Path::new(&profile).is_dir() {
                return profile;
            }
        }
        ".".to_string()
    } else if Path::new("/tmp").is_dir() {
        "/tmp".to_string()
    } else if Path::new("/var/tmp").is_dir() {
        "/var/tmp".to_string()
    } else {
        ".".to_string()
    }
});

/// `tempfile.gettempdir()`.
fn gettempdir() -> i64 {
    value::str_alloc(TEMP_DIR.as_str().to_string())
}

/// Builds a fresh unique name component (`<pid><counter>`).
fn unique_component() -> String {
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}{count}", std::process::id())
}

/// Creates a directory named `prefix + unique + suffix` below `dir`.
fn create_temp_dir(prefix: &str, suffix: &str, dir: Option<&str>) -> io::Result<PathBuf> {
    let base = match dir {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(TEMP_DIR.as_str()),
    };
    for _ in 0..100 {
        let candidate = base.join(format!("{prefix}{}{suffix}", unique_component()));
        match fs::create_dir(&candidate) {
            Ok(()) => {
                restrict_permissions(&candidate);
                return Ok(candidate);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique temporary directory",
    ))
}

/// Tightens a freshly created temporary directory to owner-only access.
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Reads an optional `str`/`pathlib.Path` argument; `None`/missing select the
/// default. A value of another type reports `TypeError`.
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

/// Raises `OSError` for a filesystem failure on `path`.
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

/// `tempfile.mkdtemp(suffix=None, prefix=None, dir=None)`.
fn mkdtemp(args: &[i64]) -> i64 {
    let suffix = match optional_path(args, 0, "mkdtemp", "suffix") {
        Ok(value) => value.unwrap_or_default(),
        Err(error) => return error,
    };
    let prefix = match optional_path(args, 1, "mkdtemp", "prefix") {
        Ok(value) => value.unwrap_or_else(|| "tmp".to_string()),
        Err(error) => return error,
    };
    let dir = match optional_path(args, 2, "mkdtemp", "dir") {
        Ok(value) => value,
        Err(error) => return error,
    };
    match create_temp_dir(&prefix, &suffix, dir.as_deref()) {
        Ok(path) => value::str_alloc(path.to_string_lossy().into_owned()),
        Err(error) => {
            let base = Path::new(dir.as_deref().unwrap_or(TEMP_DIR.as_str()));
            io_error(error, base)
        }
    }
}

/// Removes `path` recursively, tolerating an already-missing path.
fn remove_tree(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_dir() {
        match fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    } else {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// Builds the native `TemporaryDirectory` class.
fn temporary_directory_class() -> i64 {
    const METHODS: [(&str, &str, usize); 3] = [
        ("__enter__", "_tempdir_enter", 1),
        ("__exit__", "_tempdir_exit", 4),
        ("cleanup", "_tempdir_cleanup", 1),
    ];
    let mut functions = [NONE; METHODS.len()];
    for (slot, (_, native, arity)) in functions.iter_mut().zip(METHODS) {
        *slot = value::native_function_new("tempfile", native, arity);
    }
    let methods = METHODS
        .iter()
        .zip(functions)
        .map(|((python_name, _, _), function)| ((*python_name).to_string(), function))
        .collect();
    value::class_new("TemporaryDirectory".to_string(), NONE, methods)
}

/// `tempfile.TemporaryDirectory(suffix=None, prefix=None, dir=None)`.
fn new_temporary_directory(args: &[i64]) -> i64 {
    let suffix = match optional_path(args, 0, "TemporaryDirectory", "suffix") {
        Ok(value) => value.unwrap_or_default(),
        Err(error) => return error,
    };
    let prefix = match optional_path(args, 1, "TemporaryDirectory", "prefix") {
        Ok(value) => value.unwrap_or_else(|| "tmp".to_string()),
        Err(error) => return error,
    };
    let dir = match optional_path(args, 2, "TemporaryDirectory", "dir") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let path = match create_temp_dir(&prefix, &suffix, dir.as_deref()) {
        Ok(path) => path,
        Err(error) => {
            let base = Path::new(dir.as_deref().unwrap_or(TEMP_DIR.as_str()));
            return io_error(error, base);
        }
    };
    let instance = value::instance_new(temporary_directory_class());
    value::instance_setattr(
        instance,
        "name".to_string(),
        value::str_alloc(path.to_string_lossy().into_owned()),
    );
    instance
}

/// Cleans up the directory named by the instance's `name` attribute.
fn cleanup(instance: i64) -> i64 {
    let Some(name) = value::instance_attr(instance, "name") else {
        return NONE;
    };
    if name == NONE {
        return NONE;
    }
    let Some(path) = value::as_path_like(name) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "TemporaryDirectory.name must be str or pathlib.Path, not {}",
                value::type_name(name)
            ),
        );
    };
    // Copy before touching the filesystem: the error path allocates.
    let path = path.to_string();
    match remove_tree(Path::new(&path)) {
        Ok(()) => NONE,
        Err(error) => io_error(error, Path::new(&path)),
    }
}

/// Dispatches a native `tempfile` function or method.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "gettempdir" => gettempdir(),
        "mkdtemp" => mkdtemp(args),
        "TemporaryDirectory" => new_temporary_directory(args),
        "_tempdir_enter" => match args.first() {
            // CPython's `__enter__` returns the directory name, so
            // `with TemporaryDirectory() as d:` binds `d` to a `str`.
            Some(&instance) => value::instance_attr(instance, "name").unwrap_or(instance),
            None => exc::raise_with_message(TYPE_ERROR, "__enter__() missing self"),
        },
        "_tempdir_exit" => match args.first() {
            Some(&instance) => cleanup(instance),
            None => exc::raise_with_message(TYPE_ERROR, "__exit__() missing self"),
        },
        "_tempdir_cleanup" => match args.first() {
            Some(&instance) => cleanup(instance),
            None => exc::raise_with_message(TYPE_ERROR, "cleanup() missing self"),
        },
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'tempfile' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(item: &str) -> i64 {
        value::str_alloc(item.to_string())
    }

    fn name_of(instance: i64) -> String {
        let name = value::instance_attr(instance, "name").expect("name attribute");
        value::as_str(name).expect("name is str").to_string()
    }

    fn assert_clean(test_result: i64) {
        if exc::pending() {
            let message = value::to_string(exc::pending_value());
            exc::clear();
            panic!("unexpected exception: {message}");
        }
        assert_ne!(test_result, NONE);
    }

    #[test]
    fn gettempdir_returns_an_existing_directory() {
        let result = call("gettempdir", &[]);
        let directory = value::as_str(result).expect("str").to_string();
        assert!(!directory.is_empty());
        assert!(Path::new(&directory).is_dir());
    }

    #[test]
    fn mkdtemp_applies_prefix_and_suffix() {
        let prefix = format!("hydro-prefix-{}-", std::process::id());
        let args = [
            text("-suffix"),
            text(&prefix),
            value::str_alloc(TEMP_DIR.as_str().to_string()),
        ];
        let result = call("mkdtemp", &args);
        assert_clean(result);
        let path = value::as_str(result).expect("str").to_string();
        let file_name = Path::new(&path).file_name().unwrap().to_string_lossy();
        assert!(file_name.starts_with(prefix.as_str()));
        assert!(file_name.ends_with("-suffix"));
        assert!(Path::new(&path).is_dir());
        fs::remove_dir_all(&path).expect("remove test directory");
    }

    #[test]
    fn mkdtemp_uses_defaults_and_unique_names() {
        let first = call("mkdtemp", &[]);
        let second = call("mkdtemp", &[]);
        assert_clean(first);
        assert_clean(second);
        let first = value::as_str(first).unwrap().to_string();
        let second = value::as_str(second).unwrap().to_string();
        assert_ne!(first, second);
        assert!(Path::new(&first).is_dir());
        assert!(Path::new(&second).is_dir());
        fs::remove_dir_all(&first).expect("remove first");
        fs::remove_dir_all(&second).expect("remove second");
    }

    #[test]
    fn mkdtemp_rejects_non_string_arguments() {
        let result = call("mkdtemp", &[value::encode_int(1).expect("small int")]);
        assert_eq!(result, NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn temporary_directory_context_manager_removes_the_tree() {
        let instance = call("TemporaryDirectory", &[]);
        assert_clean(instance);
        assert!(value::as_instance(instance).is_some());
        let path = name_of(instance);
        assert!(Path::new(&path).is_dir());
        let nested = Path::new(&path).join("nested");
        fs::create_dir(&nested).expect("nested directory");
        fs::write(nested.join("file.txt"), b"payload").expect("nested file");

        let entered = call("_tempdir_enter", &[instance]);
        assert_eq!(value::as_str(entered), Some(path.as_str()));
        let exit_args = [instance, NONE, NONE, NONE];
        assert_eq!(call("_tempdir_exit", &exit_args), NONE);
        assert!(!exc::pending());
        assert!(!Path::new(&path).exists());

        // A second cleanup tolerates the missing path.
        assert_eq!(call("_tempdir_cleanup", &[instance]), NONE);
        assert!(!exc::pending());
    }

    #[test]
    fn temporary_directory_accepts_prefix_and_suffix() {
        let prefix = format!("hydro-td-{}-", std::process::id());
        let args = [
            text("-end"),
            text(&prefix),
            value::str_alloc(TEMP_DIR.as_str().to_string()),
        ];
        let instance = call("TemporaryDirectory", &args);
        assert_clean(instance);
        let path = name_of(instance);
        let file_name = Path::new(&path).file_name().unwrap().to_string_lossy();
        assert!(file_name.starts_with(prefix.as_str()));
        assert!(file_name.ends_with("-end"));
        assert_eq!(call("_tempdir_cleanup", &[instance]), NONE);
        assert!(!Path::new(&path).exists());
    }

    #[test]
    fn cleanup_removes_a_directory_that_became_a_file() {
        let instance = call("TemporaryDirectory", &[]);
        assert_clean(instance);
        let path = name_of(instance);
        fs::remove_dir(&path).expect("remove directory");
        fs::write(&path, b"replacement").expect("write file");
        assert_eq!(call("_tempdir_cleanup", &[instance]), NONE);
        assert!(!exc::pending());
        assert!(!Path::new(&path).exists());
    }

    #[test]
    fn unknown_function_reports_attribute_error() {
        assert_eq!(call("named_temporary_file", &[]), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        exc::clear();
    }
}
