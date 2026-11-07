//! `tempfile`: temporary files and directories.
//!
//! The implemented subset is `gettempdir()`, `mkdtemp()`, `mkstemp()`,
//! `mktemp()`, `TemporaryDirectory` and `NamedTemporaryFile`. Unique names
//! are derived from the process id and a process-global counter, which is
//! enough to keep concurrent callers (and parallel tests) apart without
//! pulling in a random-number dependency.
//!
//! `mktemp()` is the deprecated name-only helper: it returns a name that does
//! not exist yet without creating the file. `__file__` reports the Python
//! source path of the module, which the stdlib tests use to locate the
//! interpreter's library directory.
//!
//! `_TemporaryFileWrapper` mirrors CPython's wrapper class: it stores the
//! wrapped `file`, the `name` it was created under and the `delete` /
//! `delete_on_close` policy, delegates every other attribute to the file
//! (`__getattr__`), and deletes the file on `close()` when the policy asks
//! for it. It is a real class value, so `urllib.response.addbase` can
//! subclass it.
//!
//! `TemporaryDirectory` is a native class instance: the constructor allocates
//! the directory and stores its path in the `name` attribute, while
//! `__enter__`/`__exit__`/`cleanup` are native methods. Cleanup removes the
//! tree recursively and treats an already-missing path as success. The
//! `name` attribute is kept after cleanup, so callers can still inspect the
//! removed path.
//!
//! `NamedTemporaryFile` wraps a real file object (created by the runtime's
//! `io.open`) in the native `NamedTemporaryFile` class. The wrapper exposes
//! `file`, `name`, `mode`, `delete`, `delete_on_close`, `encoding`, `errors`
//! and `newlines`, implements `close()`/`__enter__`/`__exit__`, and deletes
//! the file on close when `delete` and `delete_on_close` are both true (or on
//! context-manager exit when only `delete` is true).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

use hydro_ir::{MethodKind, ParamKind};

use crate::core::{globals, iter, methods};
use crate::exc::{self, ATTRIBUTE_ERROR, OS_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::value::{self, ParamMeta, FALSE, NONE, TRUE};

/// Source of unique name components within one process.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Attempts `mktemp()` makes before reporting exhaustion, matching CPython's
/// `TMP_MAX`.
const MKTEMP_ATTEMPTS: usize = 10_000;

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

/// `tempfile.mkstemp(suffix=None, prefix=None, dir=None, text=False)`: creates
/// an exclusively-opened temporary file and returns `(fd, path)`.
fn mkstemp(args: &[i64]) -> i64 {
    let suffix = match optional_path(args, 0, "mkstemp", "suffix") {
        Ok(value) => value.unwrap_or_default(),
        Err(error) => return error,
    };
    let prefix = match optional_path(args, 1, "mkstemp", "prefix") {
        Ok(value) => value.unwrap_or_else(|| "tmp".to_string()),
        Err(error) => return error,
    };
    let dir = match optional_path(args, 2, "mkstemp", "dir") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let base = match dir.as_deref() {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(TEMP_DIR.as_str()),
    };
    for _ in 0..100 {
        let candidate = base.join(format!("{prefix}{}{suffix}", unique_component()));
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => {
                #[cfg(unix)]
                {
                    use std::os::unix::io::IntoRawFd;
                    let fd = file.into_raw_fd();
                    let path = value::str_alloc(candidate.to_string_lossy().into_owned());
                    return value::tuple_new(vec![
                        value::encode_int(i64::from(fd)).expect("fd fits"),
                        path,
                    ]);
                }
                #[cfg(not(unix))]
                {
                    drop(file);
                    let path = value::str_alloc(candidate.to_string_lossy().into_owned());
                    return value::tuple_new(vec![NONE, path]);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return io_error(error, &base),
        }
    }
    io_error(
        io::Error::new(io::ErrorKind::AlreadyExists, "too many temporary files"),
        &base,
    )
}

/// `tempfile.mktemp(suffix='', prefix='tmp', dir=None)`: the deprecated
/// name-only helper.
///
/// The name is chosen from the same process-wide counter `mkstemp` uses and
/// is checked against the filesystem (`symlink_metadata` also reports a
/// dangling link, so an uncreatable name is never returned), but the file is
/// left for the caller to create — which is exactly why CPython deprecates
/// the function.
fn mktemp(args: &[i64]) -> i64 {
    let suffix = match optional_path(args, 0, "mktemp", "suffix") {
        Ok(value) => value.unwrap_or_default(),
        Err(error) => return error,
    };
    let prefix = match optional_path(args, 1, "mktemp", "prefix") {
        Ok(value) => value.unwrap_or_else(|| "tmp".to_string()),
        Err(error) => return error,
    };
    let dir = match optional_path(args, 2, "mktemp", "dir") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let base = match dir.as_deref() {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(TEMP_DIR.as_str()),
    };
    for _ in 0..MKTEMP_ATTEMPTS {
        let candidate = base.join(format!("{prefix}{}{suffix}", unique_component()));
        if fs::symlink_metadata(&candidate).is_err() {
            return value::str_alloc(candidate.to_string_lossy().into_owned());
        }
    }
    // CPython raises `FileExistsError(EEXIST, "No usable temporary filename
    // found")`; Hydro's exception registry has the `OSError` spelling.
    exc::raise_with_message(OS_ERROR, "[Errno 17] No usable temporary filename found")
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

// -------------------------------------------------------------------------
// NamedTemporaryFile
// -------------------------------------------------------------------------

/// Creates an empty file named `prefix + unique + suffix` below `dir`.
///
/// `create_new` guarantees the exclusive creation `mkstemp` performs; a
/// collision retries with the next counter value. The caller owns the file and
/// must unlink it (directly or through the wrapper) if opening fails.
fn create_temp_file(prefix: &str, suffix: &str, dir: Option<&str>) -> io::Result<PathBuf> {
    let base = match dir {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(TEMP_DIR.as_str()),
    };
    for _ in 0..100 {
        let candidate = base.join(format!("{prefix}{}{suffix}", unique_component()));
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => {
                drop(file);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&candidate, fs::Permissions::from_mode(0o600));
                }
                return Ok(candidate);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique temporary file",
    ))
}

/// Validates `buffering` and returns the value to forward to `io.open`.
/// The native file object is not separately buffered, so only the range/type
/// checks are observable.
fn buffering_arg(args: &[i64]) -> Result<i64, i64> {
    match args.get(1).copied() {
        None | Some(NONE) => Ok(NONE),
        Some(argument) => match value::as_integer(argument) {
            Some(size) if size >= -1 => Ok(argument),
            Some(_) => Err(exc::raise_with_message(
                VALUE_ERROR,
                "invalid buffering size",
            )),
            None => Err(exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "NamedTemporaryFile() argument 'buffering' must be int, not '{}'",
                    value::type_name(argument)
                ),
            )),
        },
    }
}

/// Validates an optional text argument and returns its tagged value (`None`
/// for an omitted/`None` slot).
fn optional_text(args: &[i64], index: usize, function: &str, name: &str) -> Result<i64, i64> {
    match args.get(index).copied() {
        None | Some(NONE) => Ok(NONE),
        Some(argument) => match value::as_str(argument) {
            Some(_) => Ok(argument),
            None => Err(exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "{function}() argument '{name}' must be str or None, not '{}'",
                    value::type_name(argument)
                ),
            )),
        },
    }
}

/// Reads an optional boolean flag; missing/`None` selects `default`.
fn bool_arg(args: &[i64], index: usize, default: bool) -> bool {
    match args.get(index).copied() {
        None | Some(NONE) => default,
        Some(argument) => value::truthy(argument),
    }
}

/// Builds the cached native `NamedTemporaryFile` wrapper class.
fn named_temporary_file_class() -> i64 {
    const KEY: &str = "tempfile.NamedTemporaryFile.class";
    if let Some(class) = globals::registry_get(KEY) {
        return class;
    }
    // Native class methods dispatch through `modules::call_native`, which
    // resolves the native name in the central spec. The `_tempdir_*` entries
    // are the only tempfile method names declared there, so the wrapper
    // reuses them and [`call`] branches on the wrapper class: close uses the
    // cleanup entry, `__enter__` the enter entry and `__exit__` the exit
    // entry. The stream methods are installed per instance as bound methods
    // of `.file` (see `install_stream_methods`), which needs no spec entry.
    const METHODS: [(&str, &str, usize); 3] = [
        ("close", "_tempdir_cleanup", 1),
        ("__enter__", "_tempdir_enter", 1),
        ("__exit__", "_tempdir_exit", 4),
    ];
    let mut functions = [NONE; METHODS.len()];
    for (slot, (_, native, arity)) in functions.iter_mut().zip(METHODS) {
        *slot = value::native_function_new("tempfile", native, arity);
    }
    let roots = heap::root_values(&functions);
    let methods = METHODS
        .iter()
        .zip(functions)
        .map(|((python_name, _, _), function)| ((*python_name).to_string(), function))
        .collect();
    let class = value::class_new("NamedTemporaryFile".to_string(), NONE, methods);
    let class_roots = heap::root_values(&[class]);
    globals::registry_set(KEY, class);
    heap::release_roots(class_roots);
    heap::release_roots(roots);
    class
}

/// Reads a wrapper flag (`_close_called`/`_cleanup_called`).
fn flag(instance: i64, name: &str) -> bool {
    value::instance_attr(instance, name) == Some(TRUE)
}

fn set_flag(instance: i64, name: &str, value: bool) {
    value::instance_setattr(instance, name.to_string(), if value { TRUE } else { FALSE });
}

/// Unlinks a temporary file, tolerating an already-missing path.
fn unlink_if_present(path: &str) -> Result<(), io::Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Closes the wrapped file object, if it is still open, and mirrors the new
/// `file.closed` state onto the wrapper's `.closed` attribute.
fn close_wrapped(instance: i64) {
    if let Some(file) = value::instance_attr(instance, "file") {
        if let Some(file) = value::as_file(file) {
            file.close();
        } else {
            // A file-like object (`io.StringIO`, a socket makefile wrapper, a
            // user class): close it through its own protocol, exactly like
            // CPython's `self.file.close()` in `_TemporaryFileCloser`.
            let roots = heap::root_values(&[instance, file]);
            let _ = methods::call(file, b"close".as_ptr(), 5, 0, std::ptr::null());
            heap::release_roots(roots);
        }
    }
    let roots = heap::root_values(&[instance]);
    value::instance_setattr(instance, "closed".to_string(), TRUE);
    heap::release_roots(roots);
}

/// Whether `instance` is a `NamedTemporaryFile` wrapper rather than a
/// `TemporaryDirectory`.
fn is_named_wrapper(instance: i64) -> bool {
    value::instance_class_name(instance) == Some("NamedTemporaryFile")
}

/// Installs the stream methods on a wrapper as bound methods of `.file`.
///
/// A compiled `wrapper.read(...)` lowers to a direct method call, which
/// resolves through the class table and the instance dictionary but not
/// through `__getattr__`; binding the real file methods on the instance gives
/// the same delegation CPython performs through
/// `_TemporaryFileWrapper.__getattr__` for those calls and keeps the receiver
/// (`file`) alive through the traced instance attribute table.
fn install_stream_methods(instance: i64, file: i64) {
    for name in [
        "read",
        "readline",
        "readlines",
        "write",
        "writelines",
        "flush",
        "seek",
        "tell",
    ] {
        let method = value::bound_method_new(file, name.to_string());
        let roots = heap::root_values(&[method]);
        value::instance_setattr(instance, name.to_string(), method);
        heap::release_roots(roots);
    }
}

/// `_TemporaryFileCloser.cleanup()`: close and unlink according to `delete`.
fn cleanup_named(instance: i64) -> i64 {
    if flag(instance, "_cleanup_called") {
        return NONE;
    }
    set_flag(instance, "_cleanup_called", true);
    if !flag(instance, "_close_called") {
        set_flag(instance, "_close_called", true);
        close_wrapped(instance);
    }
    if value::instance_attr(instance, "delete") == Some(TRUE) {
        let name = value::instance_attr(instance, "name").unwrap_or(NONE);
        let Some(path) = value::as_str(name) else {
            return exc::raise_with_message(TYPE_ERROR, "NamedTemporaryFile.name must be str");
        };
        let path = path.to_string();
        if let Err(error) = unlink_if_present(&path) {
            return io_error(error, Path::new(&path));
        }
    }
    NONE
}

/// `_TemporaryFileCloser.close()`: close now, unlink only when
/// `delete_on_close` is set.
fn close_named(instance: i64) -> i64 {
    if flag(instance, "_close_called") {
        return NONE;
    }
    set_flag(instance, "_close_called", true);
    close_wrapped(instance);
    if value::instance_attr(instance, "delete") == Some(TRUE)
        && value::instance_attr(instance, "delete_on_close") == Some(TRUE)
    {
        return cleanup_named(instance);
    }
    NONE
}

/// `tempfile.NamedTemporaryFile(...)`.
fn new_named_temporary_file(args: &[i64]) -> i64 {
    let mode = match args.first().copied() {
        None | Some(NONE) => "w+b".to_string(),
        Some(argument) => match value::as_str(argument) {
            Some(text) => text.to_string(),
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "NamedTemporaryFile() argument 'mode' must be str, not '{}'",
                        value::type_name(argument)
                    ),
                )
            }
        },
    };
    let buffering = match buffering_arg(args) {
        Ok(buffering) => buffering,
        Err(error) => return error,
    };
    let encoding = match optional_text(args, 2, "NamedTemporaryFile", "encoding") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let newline = match optional_text(args, 3, "NamedTemporaryFile", "newline") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let suffix = match optional_path(args, 4, "NamedTemporaryFile", "suffix") {
        Ok(value) => value.unwrap_or_default(),
        Err(error) => return error,
    };
    let prefix = match optional_path(args, 5, "NamedTemporaryFile", "prefix") {
        Ok(value) => value.unwrap_or_else(|| "tmp".to_string()),
        Err(error) => return error,
    };
    let dir = match optional_path(args, 6, "NamedTemporaryFile", "dir") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let delete = bool_arg(args, 7, true);
    let errors = match optional_text(args, 8, "NamedTemporaryFile", "errors") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let delete_on_close = bool_arg(args, 9, true);

    let path = match create_temp_file(&prefix, &suffix, dir.as_deref()) {
        Ok(path) => path,
        Err(error) => {
            let base = Path::new(dir.as_deref().unwrap_or(TEMP_DIR.as_str()));
            return io_error(error, base);
        }
    };
    let path_text = path.to_string_lossy().into_owned();
    // CPython's `io.open` layout: file, mode, buffering, encoding, errors,
    // newline, closefd, opener. `io.open` ignores closefd/opener, so they stay
    // `None`; the wrapper opens an existing path.
    let open_args = [
        value::str_alloc(path_text.clone()),
        value::str_alloc(mode.clone()),
        buffering,
        encoding,
        errors,
        newline,
        NONE,
        NONE,
    ];
    let file_value = crate::modules::io_open(&open_args);
    if exc::pending() {
        let _ = unlink_if_present(&path_text);
        return file_value;
    }
    // The file object must stay visible while the attribute values and the
    // instance are allocated.
    let file_roots = heap::root_values(&[file_value, encoding, errors]);

    let binary = mode.contains('b');
    let name_value = value::str_alloc(path_text);
    let mode_value = value::str_alloc(mode);
    let encoding_value = if binary || encoding == NONE {
        if binary {
            NONE
        } else {
            value::str_alloc("UTF-8".to_string())
        }
    } else {
        encoding
    };
    let errors_value = if binary || errors == NONE {
        if binary {
            NONE
        } else {
            value::str_alloc("strict".to_string())
        }
    } else {
        errors
    };

    let instance = value::instance_new(named_temporary_file_class());
    let roots = heap::root_values(&[
        name_value,
        mode_value,
        encoding_value,
        errors_value,
        instance,
    ]);
    value::instance_setattr(instance, "name".to_string(), name_value);
    value::instance_setattr(instance, "mode".to_string(), mode_value);
    value::instance_setattr(
        instance,
        "delete".to_string(),
        if delete { TRUE } else { FALSE },
    );
    value::instance_setattr(
        instance,
        "delete_on_close".to_string(),
        if delete_on_close { TRUE } else { FALSE },
    );
    value::instance_setattr(instance, "encoding".to_string(), encoding_value);
    value::instance_setattr(instance, "errors".to_string(), errors_value);
    value::instance_setattr(instance, "newlines".to_string(), NONE);
    value::instance_setattr(instance, "closed".to_string(), FALSE);
    value::instance_setattr(instance, "_close_called".to_string(), FALSE);
    value::instance_setattr(instance, "_cleanup_called".to_string(), FALSE);
    value::instance_setattr(instance, "file".to_string(), file_value);
    install_stream_methods(instance, file_value);
    heap::release_roots(roots);
    heap::release_roots(file_roots);
    instance
}

// -------------------------------------------------------------------------
// _TemporaryFileWrapper
// -------------------------------------------------------------------------

/// Wraps a native entry point as a class-method value.
///
/// `_TemporaryFileWrapper`'s methods are not module-level `tempfile`
/// functions, so the central spec table cannot declare them; a raw wrapper
/// carries its own entry point, which the class method binder invokes
/// directly.
fn wrapper_method(entry: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
    value::function_new(entry as *const () as usize, arity, name.to_string())
}

/// The wrapped file object of a `_TemporaryFileWrapper` instance.
fn wrapped_file(instance: i64) -> Option<i64> {
    value::instance_attr(instance, "file")
}

/// `_TemporaryFileWrapper.__init__(self, file, name, delete=True,
/// delete_on_close=True)`: stores the wrapped file and the deletion policy.
///
/// The signature is real (not just an arity) because
/// `urllib.response.addbase.__init__` forwards `delete=False` as a keyword
/// through `super().__init__(...)`; the method binder resolves it here.
extern "C" fn wrapper_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes the five signature slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 5) };
    let (instance, file, name) = (args[0], args[1], args[2]);
    let roots = heap::root_values(args);
    value::instance_setattr(instance, "file".to_string(), file);
    value::instance_setattr(instance, "name".to_string(), name);
    value::instance_setattr(
        instance,
        "delete".to_string(),
        value::bool_value(value::truthy(args[3])),
    );
    value::instance_setattr(
        instance,
        "delete_on_close".to_string(),
        value::bool_value(value::truthy(args[4])),
    );
    value::instance_setattr(instance, "_close_called".to_string(), FALSE);
    value::instance_setattr(instance, "_cleanup_called".to_string(), FALSE);
    // Compiled method calls do not consult `__getattr__`, so the wrapped
    // file's stream methods are bound on the instance the same way
    // `NamedTemporaryFile` does it.
    install_stream_methods(instance, file);
    heap::release_roots(roots);
    NONE
}

/// `_TemporaryFileWrapper.__getattr__(name)`: delegates to the wrapped file
/// and caches non-numeric results, exactly like CPython's wrapper.
extern "C" fn wrapper_getattr_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `(self, name)`.
    let instance = unsafe { *argv };
    let name_value = unsafe { *argv.add(1) };
    let Some(name) = value::as_str(name_value) else {
        return exc::raise_with_message(TYPE_ERROR, "attribute name must be string");
    };
    let name = name.to_string();
    let Some(file) = wrapped_file(instance) else {
        return exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("'_TemporaryFileWrapper' object has no attribute '{name}'"),
        );
    };
    let roots = heap::root_values(&[instance, file]);
    let result = methods::getattr(file, name.as_ptr(), name.len());
    // CPython caches everything but integers (`closed` and friends must stay
    // live views of the file).
    if !exc::pending() && value::as_integer(result).is_none() {
        let result_roots = heap::root_values(&[result]);
        value::instance_setattr(instance, name, result);
        heap::release_roots(result_roots);
    }
    heap::release_roots(roots);
    result
}

/// `_TemporaryFileWrapper.__enter__()`: enters the wrapped file and returns
/// the wrapper, which is the object CPython's overridden context manager
/// binds to the `as` target.
extern "C" fn wrapper_enter_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self`.
    let instance = unsafe { *argv };
    let Some(file) = wrapped_file(instance) else {
        return NONE;
    };
    let roots = heap::root_values(&[instance, file]);
    let _ = methods::call(file, b"__enter__".as_ptr(), 9, 0, std::ptr::null());
    heap::release_roots(roots);
    if exc::pending() {
        return NONE;
    }
    instance
}

/// `_TemporaryFileWrapper.__exit__(exc, value, tb)`: exits the wrapped file,
/// then closes and deletes the temporary file according to `delete`.
extern "C" fn wrapper_exit_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `(self, exc, value, tb)`.
    let instance = unsafe { *argv };
    let exit_args = [unsafe { *argv.add(1) }, unsafe { *argv.add(2) }, unsafe {
        *argv.add(3)
    }];
    let Some(file) = wrapped_file(instance) else {
        return NONE;
    };
    let roots = heap::root_values(&[instance, file]);
    let result = methods::call(
        file,
        b"__exit__".as_ptr(),
        8,
        exit_args.len(),
        exit_args.as_ptr(),
    );
    if exc::pending() {
        heap::release_roots(roots);
        return NONE;
    }
    let result_roots = heap::root_values(&[result]);
    let _ = wrapper_cleanup(instance);
    // CPython returns what the wrapped file's `__exit__` returned; a cleanup
    // failure replaces it.
    let outcome = if exc::pending() { NONE } else { result };
    heap::release_roots(result_roots);
    heap::release_roots(roots);
    outcome
}

/// `_TemporaryFileWrapper.__iter__()`: iterates the wrapped file.
extern "C" fn wrapper_iter_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self`.
    let instance = unsafe { *argv };
    let Some(file) = wrapped_file(instance) else {
        return NONE;
    };
    let roots = heap::root_values(&[instance, file]);
    let result = iter::iter_new(file);
    heap::release_roots(roots);
    result
}

/// `_TemporaryFileWrapper.__next__()`: the next line of the wrapped file.
extern "C" fn wrapper_next_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self`.
    let instance = unsafe { *argv };
    let Some(file) = wrapped_file(instance) else {
        return NONE;
    };
    let roots = heap::root_values(&[instance, file]);
    let iterator = iter::iter_new(file);
    if exc::pending() {
        heap::release_roots(roots);
        return NONE;
    }
    let iterator_roots = heap::root_values(&[iterator]);
    let result = iter::next_value(iterator, NONE, false);
    heap::release_roots(iterator_roots);
    heap::release_roots(roots);
    result
}

/// `_TemporaryFileWrapper.closed`: the wrapped file's live closed flag.
extern "C" fn wrapper_closed_entry(argv: *const i64) -> i64 {
    // SAFETY: a property getter receives the instance.
    let instance = unsafe { *argv };
    let Some(file) = wrapped_file(instance) else {
        return exc::raise_with_message(
            ATTRIBUTE_ERROR,
            "'_TemporaryFileWrapper' object has no attribute 'file'",
        );
    };
    let roots = heap::root_values(&[file]);
    let result = methods::getattr(file, b"closed".as_ptr(), 6);
    heap::release_roots(roots);
    result
}

/// `_TemporaryFileWrapper.close()`: closes the file and, when both `delete`
/// and `delete_on_close` are set, unlinks it now.
extern "C" fn wrapper_close_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self`.
    let instance = unsafe { *argv };
    let roots = heap::root_values(&[instance]);
    let result = wrapper_close(instance);
    heap::release_roots(roots);
    result
}

/// Closes the wrapped file once; deletes it when `delete_on_close` allows it.
fn wrapper_close(instance: i64) -> i64 {
    if flag(instance, "_close_called") {
        return NONE;
    }
    set_flag(instance, "_close_called", true);
    close_wrapped(instance);
    if flag(instance, "delete") && flag(instance, "delete_on_close") {
        return wrapper_cleanup(instance);
    }
    NONE
}

/// `_TemporaryFileCloser.cleanup()`: close the file once and unlink it when
/// `delete` allows it, tolerating an already-missing path.
fn wrapper_cleanup(instance: i64) -> i64 {
    if flag(instance, "_cleanup_called") {
        return NONE;
    }
    set_flag(instance, "_cleanup_called", true);
    if !flag(instance, "_close_called") {
        set_flag(instance, "_close_called", true);
        close_wrapped(instance);
    }
    if !flag(instance, "delete") {
        return NONE;
    }
    let name = value::instance_attr(instance, "name").unwrap_or(NONE);
    let Some(path) = value::as_path_like(name) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "_TemporaryFileWrapper.name must be str or pathlib.Path, not {}",
                value::type_name(name)
            ),
        );
    };
    let path = path.to_string();
    match unlink_if_present(&path) {
        Ok(()) => NONE,
        Err(error) => io_error(error, Path::new(&path)),
    }
}

/// Builds the cached native `_TemporaryFileWrapper` class.
///
/// A real class value (not a constructor function) so
/// `urllib.response.addbase` can subclass it and `isinstance` works. The
/// methods carry their own entry points because none of their names exist in
/// the central `tempfile` spec.
fn temporary_file_wrapper_class() -> i64 {
    const KEY: &str = "tempfile._TemporaryFileWrapper.class";
    if let Some(class) = globals::registry_get(KEY) {
        return class;
    }
    // `__init__` needs real parameter metadata: `super().__init__(fp, name,
    // delete=False)` in the urllib response classes binds `delete` by name.
    let init = value::native_function_new("tempfile", "_TemporaryFileWrapper.__init__", usize::MAX);
    value::function_set_signature(
        init,
        vec![
            ParamMeta {
                name: "self".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
            ParamMeta {
                name: "file".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
            ParamMeta {
                name: "name".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
            ParamMeta {
                name: "delete".to_string(),
                kind: ParamKind::Positional,
                default: Some(TRUE),
            },
            ParamMeta {
                name: "delete_on_close".to_string(),
                kind: ParamKind::Positional,
                default: Some(TRUE),
            },
        ],
    );
    value::function_set_full_entry(init, wrapper_init_entry as *const () as usize);

    let methods = vec![
        ("__init__".to_string(), init, MethodKind::Normal),
        (
            "__getattr__".to_string(),
            wrapper_method(wrapper_getattr_entry, 2, "__getattr__"),
            MethodKind::Normal,
        ),
        (
            "__enter__".to_string(),
            wrapper_method(wrapper_enter_entry, 1, "__enter__"),
            MethodKind::Normal,
        ),
        (
            "__exit__".to_string(),
            wrapper_method(wrapper_exit_entry, 4, "__exit__"),
            MethodKind::Normal,
        ),
        (
            "__iter__".to_string(),
            wrapper_method(wrapper_iter_entry, 1, "__iter__"),
            MethodKind::Normal,
        ),
        (
            "__next__".to_string(),
            wrapper_method(wrapper_next_entry, 1, "__next__"),
            MethodKind::Normal,
        ),
        (
            "close".to_string(),
            wrapper_method(wrapper_close_entry, 1, "close"),
            MethodKind::Normal,
        ),
    ];
    let closed = value::property_new(
        wrapper_method(wrapper_closed_entry, 1, "closed"),
        NONE,
        NONE,
    );
    let attrs = vec![("closed".to_string(), closed)];
    // The method and property values live in plain Rust memory while the
    // class is built, so root them before the class allocation can collect.
    let mut live = Vec::with_capacity(methods.len() + 1);
    live.push(init);
    live.extend(methods.iter().map(|(_, function, _)| *function));
    live.push(closed);
    let roots = heap::root_values(&live);
    let class = value::class_new_full("_TemporaryFileWrapper".to_string(), NONE, methods, attrs);
    let class_roots = heap::root_values(&[class]);
    globals::registry_set(KEY, class);
    heap::release_roots(class_roots);
    heap::release_roots(roots);
    class
}

/// Source path reported by `tempfile.__file__`.
///
/// Hydro implements `tempfile` natively, but CPython's module is a Python
/// module: the standard library tree backing this interpreter is where its
/// source lives, and the stdlib tests derive that directory from this value.
/// `HYDRO_STDLIB` names the tree when an installation supplies one; the
/// vendored tree is the fallback root. `"tempfile.py"` is the last resort for
/// a build with no visible tree at all.
fn module_file() -> String {
    let root = match std::env::var_os("HYDRO_STDLIB") {
        Some(stdlib) if !stdlib.is_empty() => PathBuf::from(stdlib),
        _ => match crate::modules::bundled_stdlib_root() {
            Some(root) => root.to_path_buf(),
            None => return "tempfile.py".to_string(),
        },
    };
    root.join("tempfile.py").to_string_lossy().into_owned()
}

/// Module attributes that are not functions: the module's source path and the
/// `_TemporaryFileWrapper` class.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "__file__" => value::str_alloc(module_file()),
        "_TemporaryFileWrapper" => temporary_file_wrapper_class(),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'tempfile' has no attribute '{name}'"),
        ),
    }
}

/// Dispatches a native `tempfile` function or method.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "gettempdir" => gettempdir(),
        "mktemp" => mktemp(args),
        "mkdtemp" => mkdtemp(args),
        "mkstemp" => mkstemp(args),
        "TemporaryDirectory" => new_temporary_directory(args),
        "NamedTemporaryFile" => new_named_temporary_file(args),
        "_tempdir_enter" => match args.first() {
            // A `NamedTemporaryFile` wrapper enters as itself; CPython's
            // `TemporaryDirectory.__enter__` instead returns the directory
            // name, so `with TemporaryDirectory() as d:` binds a `str`.
            Some(&instance) if is_named_wrapper(instance) => instance,
            Some(&instance) => value::instance_attr(instance, "name").unwrap_or(instance),
            None => exc::raise_with_message(TYPE_ERROR, "__enter__() missing self"),
        },
        "_tempdir_exit" => match args.first() {
            Some(&instance) if is_named_wrapper(instance) => {
                close_wrapped(instance);
                cleanup_named(instance)
            }
            Some(&instance) => cleanup(instance),
            None => exc::raise_with_message(TYPE_ERROR, "__exit__() missing self"),
        },
        "_tempdir_cleanup" => match args.first() {
            // The wrapper's `close()` binds this entry: close now, unlink
            // only when `delete_on_close` is set.
            Some(&instance) if is_named_wrapper(instance) => close_named(instance),
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

    fn attribute(instance: i64, name: &str) -> i64 {
        value::instance_attr(instance, name).expect("attribute")
    }

    fn method(instance: i64, name: &str, args: &[i64]) -> i64 {
        crate::core::methods::call(
            instance,
            name.as_ptr(),
            name.len(),
            args.len(),
            args.as_ptr(),
        )
    }

    fn named_file(mode: &str, delete: bool, delete_on_close: bool) -> i64 {
        let args = [
            text(mode),
            NONE,
            NONE,
            NONE,
            text("-suffix"),
            text("hydro-named-"),
            value::str_alloc(TEMP_DIR.as_str().to_string()),
            if delete { TRUE } else { FALSE },
            NONE,
            if delete_on_close { TRUE } else { FALSE },
        ];
        let instance = call("NamedTemporaryFile", &args);
        assert_clean(instance);
        instance
    }

    #[test]
    fn named_temporary_file_wraps_a_real_file_and_deletes_on_close() {
        exc::init();
        let instance = named_file("w+b", true, true);
        assert_eq!(
            value::instance_class_name(instance),
            Some("NamedTemporaryFile")
        );
        let file_value = attribute(instance, "file");
        assert!(value::as_file(file_value).is_some(), "real file object");
        let name = value::as_str(attribute(instance, "name"))
            .expect("str name")
            .to_string();
        assert!(Path::new(&name).is_file());
        assert_eq!(value::as_str(attribute(instance, "mode")), Some("w+b"));
        assert_eq!(attribute(instance, "delete"), TRUE);
        assert_eq!(attribute(instance, "delete_on_close"), TRUE);
        assert_eq!(attribute(instance, "encoding"), NONE);
        assert_eq!(attribute(instance, "errors"), NONE);
        assert_eq!(attribute(instance, "newlines"), NONE);

        // The `.file` attribute is the stream itself.
        value::as_file(file_value)
            .expect("file")
            .write_bytes(b"payload")
            .expect("write");
        assert_eq!(method(instance, "close", &[]), NONE);
        assert!(!exc::pending());
        assert!(!Path::new(&name).exists(), "delete_on_close unlinks");
        assert!(value::as_file(file_value).expect("file").closed());
        assert_eq!(
            crate::core::methods::getattr(instance, b"closed".as_ptr(), b"closed".len()),
            TRUE,
            ".closed tracks the wrapper close path"
        );
        // Closing again is a no-op.
        assert_eq!(method(instance, "close", &[]), NONE);
    }

    #[test]
    fn named_temporary_file_delete_on_close_false_unlinks_on_exit() {
        exc::init();
        let instance = named_file("wb", true, false);
        let name = value::as_str(attribute(instance, "name"))
            .expect("str name")
            .to_string();
        value::as_file(attribute(instance, "file"))
            .expect("file")
            .write_bytes(b"x")
            .expect("write");
        assert_eq!(method(instance, "close", &[]), NONE);
        assert!(!exc::pending());
        assert!(Path::new(&name).is_file(), "close keeps the file");
        assert_eq!(method(instance, "__exit__", &[NONE, NONE, NONE]), NONE);
        assert!(!exc::pending());
        assert!(!Path::new(&name).exists(), "exit unlinks");
    }

    #[test]
    fn named_temporary_file_delete_false_keeps_the_file() {
        exc::init();
        let instance = named_file("w+b", false, true);
        let name = value::as_str(attribute(instance, "name"))
            .expect("str name")
            .to_string();
        assert_eq!(attribute(instance, "delete"), FALSE);
        assert_eq!(method(instance, "__exit__", &[NONE, NONE, NONE]), NONE);
        assert!(!exc::pending());
        assert!(Path::new(&name).is_file(), "delete=False keeps the file");
        fs::remove_file(&name).expect("cleanup test file");
    }

    #[test]
    fn named_temporary_file_text_mode_exposes_text_attributes() {
        exc::init();
        let args = [text("w"), NONE, text("utf-8"), NONE];
        let instance = call("NamedTemporaryFile", &args);
        assert_clean(instance);
        let name = value::as_str(attribute(instance, "name"))
            .expect("str name")
            .to_string();
        assert_eq!(
            value::as_str(attribute(instance, "encoding")),
            Some("utf-8")
        );
        assert_eq!(value::as_str(attribute(instance, "errors")), Some("strict"));
        let written = method(instance, "write", &[text("text")]);
        assert_eq!(value::as_integer(written), Some(4));
        assert!(!exc::pending());
        assert_eq!(
            method(instance, "__enter__", &[]),
            instance,
            "__enter__ returns the wrapper"
        );
        assert_eq!(method(instance, "__exit__", &[NONE, NONE, NONE]), NONE);
        assert!(!exc::pending());
        assert!(!Path::new(&name).exists());
    }

    #[test]
    fn named_temporary_file_rejects_text_options_in_binary_mode() {
        exc::init();
        // Slots: mode, buffering, encoding, newline, suffix, prefix, dir,
        // delete, errors. `io.open` rejects every text-only option.
        let rejects = |args: &[i64], expected: &str| {
            assert_eq!(call("NamedTemporaryFile", args), NONE);
            assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
            let message = value::to_string(exc::pending_value());
            exc::clear();
            assert_eq!(message, expected);
        };
        rejects(
            &[text("w+b"), NONE, text("utf-8")],
            "binary mode doesn't take an encoding argument",
        );
        rejects(
            &[text("w+b"), NONE, NONE, text("\n")],
            "binary mode doesn't take a newline argument",
        );
        rejects(
            &[
                text("w+b"),
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                NONE,
                TRUE,
                text("strict"),
            ],
            "binary mode doesn't take an errors argument",
        );
    }

    #[test]
    fn unknown_function_reports_attribute_error() {
        assert_eq!(call("named_temporary_file", &[]), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn mktemp_returns_a_unique_name_without_creating_it() {
        let first = call("mktemp", &[]);
        let second = call("mktemp", &[]);
        assert_clean(first);
        assert_clean(second);
        let first = value::as_str(first).expect("str").to_string();
        let second = value::as_str(second).expect("str").to_string();
        assert_ne!(first, second);
        assert!(
            !Path::new(&first).exists(),
            "mktemp must not create the file"
        );
        assert!(!Path::new(&second).exists());

        let named = call("mktemp", &[text(".sock"), text("hydro-mktemp-")]);
        assert_clean(named);
        let named = value::as_str(named).expect("str").to_string();
        let file_name = Path::new(&named)
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        assert!(file_name.starts_with("hydro-mktemp-"));
        assert!(file_name.ends_with(".sock"));
        assert!(!Path::new(&named).exists());
    }

    #[test]
    fn module_file_reports_a_python_source_path() {
        let path = value::as_str(constant("__file__"))
            .expect("str")
            .to_string();
        assert!(path.ends_with("tempfile.py"), "{path}");
    }

    /// Builds a `_TemporaryFileWrapper` through the real class call path, so
    /// construction, `__init__` binding and the property are exercised.
    fn wrapper_instance(file: i64, name: &str, delete: bool, delete_on_close: bool) -> i64 {
        let class = temporary_file_wrapper_class();
        let args = [
            file,
            text(name),
            if delete { TRUE } else { FALSE },
            if delete_on_close { TRUE } else { FALSE },
        ];
        let instance = crate::core::builtins::call(class, args.len(), args.as_ptr());
        assert_clean(instance);
        instance
    }

    fn open_wrapped_file(path: &str, mode: &str) -> i64 {
        let args = [text(path), text(mode), NONE, NONE, NONE, NONE, NONE, NONE];
        let file = crate::modules::io_open(&args);
        assert_clean(file);
        file
    }

    fn temp_path(prefix: &str) -> String {
        create_temp_file(prefix, ".tmp", None)
            .expect("temporary file")
            .to_string_lossy()
            .into_owned()
    }

    fn closed_flag(instance: i64) -> i64 {
        crate::core::methods::getattr(instance, b"closed".as_ptr(), 6)
    }

    #[test]
    fn temporary_file_wrapper_deletes_on_close() {
        exc::init();
        let path = temp_path("hydro-wrapper-");
        let file = open_wrapped_file(&path, "w+b");
        let instance = wrapper_instance(file, &path, true, true);
        assert_eq!(
            value::instance_class_name(instance),
            Some("_TemporaryFileWrapper")
        );
        assert_eq!(
            value::as_str(attribute(instance, "name")),
            Some(path.as_str())
        );
        assert_eq!(attribute(instance, "delete"), TRUE);
        assert_eq!(attribute(instance, "delete_on_close"), TRUE);
        // `closed` is a live property over the wrapped file.
        assert_eq!(closed_flag(instance), FALSE);

        assert_eq!(method(instance, "close", &[]), NONE);
        assert!(!exc::pending());
        assert!(!Path::new(&path).exists(), "close deletes the file");
        assert!(value::as_file(file).expect("file").closed());
        assert_eq!(closed_flag(instance), TRUE);
        // Closing again is a no-op.
        assert_eq!(method(instance, "close", &[]), NONE);
        assert!(!exc::pending());
    }

    #[test]
    fn temporary_file_wrapper_delete_on_close_false_unlinks_on_exit() {
        exc::init();
        let path = temp_path("hydro-wrapper-exit-");
        let file = open_wrapped_file(&path, "w+b");
        let instance = wrapper_instance(file, &path, true, false);
        assert_eq!(method(instance, "close", &[]), NONE);
        assert!(!exc::pending());
        assert!(Path::new(&path).is_file(), "close keeps the file");
        assert_eq!(method(instance, "__exit__", &[NONE, NONE, NONE]), NONE);
        assert!(!exc::pending());
        assert!(!Path::new(&path).exists(), "exit deletes the file");
    }

    #[test]
    fn temporary_file_wrapper_delete_false_keeps_the_file() {
        exc::init();
        let path = temp_path("hydro-wrapper-keep-");
        let file = open_wrapped_file(&path, "w+b");
        let instance = wrapper_instance(file, &path, false, true);
        assert_eq!(attribute(instance, "delete"), FALSE);
        assert_eq!(method(instance, "__exit__", &[NONE, NONE, NONE]), NONE);
        assert!(!exc::pending());
        assert!(Path::new(&path).is_file());
        fs::remove_file(&path).expect("cleanup test file");
    }

    #[test]
    fn temporary_file_wrapper_delegates_attributes_and_methods() {
        exc::init();
        let path = temp_path("hydro-wrapper-delegate-");
        let file = open_wrapped_file(&path, "w");
        let instance = wrapper_instance(file, &path, false, true);

        // `name` is the wrapper's own attribute; `mode` falls through
        // `__getattr__` to the wrapped file and is cached on the instance.
        assert_eq!(
            value::as_str(attribute(instance, "name")),
            Some(path.as_str())
        );
        let mode = crate::core::methods::getattr(instance, b"mode".as_ptr(), 4);
        assert!(!exc::pending());
        assert!(value::as_str(mode).is_some());
        assert!(value::instance_attr(instance, "mode").is_some());

        // Stream methods are bound on the instance for compiled calls.
        let written = method(instance, "write", &[text("payload")]);
        assert_eq!(value::as_integer(written), Some(7));
        assert!(!exc::pending());
        assert_eq!(method(instance, "__exit__", &[NONE, NONE, NONE]), NONE);
        assert!(!exc::pending());
        fs::remove_file(&path).expect("cleanup test file");
    }
}
