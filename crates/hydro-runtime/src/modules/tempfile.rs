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
//! subclass it. `TemporaryFile` creates an already-unlinked file, and
//! `SpooledTemporaryFile` keeps the data in memory (a `BytesIO`/`StringIO`)
//! until it outgrows `max_size` or needs a real descriptor.
//!
//! The internal helpers CPython's own module exposes are implemented too:
//! `_infer_return_type`, `_candidate_tempdir_list`, `_get_default_tempdir`,
//! `_get_candidate_names`, `_RandomNameSequence`, `_mkstemp_inner`,
//! `gettempprefix[b]`, `gettempdirb`, and the `TMP_MAX`/`template`/`tempdir`/
//! `_bin_openflags`/`_text_openflags` data. `bytes` arguments make the call
//! return `bytes`, exactly like `_sanitize_params`; a program assignment to
//! `tempfile.tempdir` is honoured by every entry point.
//!
//! `TemporaryDirectory` is a native class instance: the constructor allocates
//! the directory and stores its path in the `name` attribute, while
//! `__enter__`/`__exit__`/`cleanup` are native methods. Cleanup removes the
//! tree recursively and treats an already-missing path as success. Like
//! CPython's `_rmtree`/`_resetperms` pair it restores owner access to a
//! directory whose mode forbids listing or unlinking, but only inside the
//! tree being removed: the containing directory's mode is never changed.
//! Directory symlinks are unlinked rather than followed. The `name` attribute
//! is kept after cleanup, so callers can still inspect the removed path.
//!
//! `NamedTemporaryFile` wraps a real file object (created by the runtime's
//! `io.open`) in the native `NamedTemporaryFile` class. The wrapper exposes
//! `file`, `name`, `mode`, `delete`, `delete_on_close`, `encoding`, `errors`
//! and `newlines`, implements `close()`/`__enter__`/`__exit__`, and deletes
//! the file on close when `delete` and `delete_on_close` are both true (or on
//! context-manager exit when only `delete` is true).

use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use hydro_ir::{MethodKind, ParamKind};

use crate::core::{globals, iter, methods};
use crate::exc::{
    self, ATTRIBUTE_ERROR, FILE_EXISTS_ERROR, FILE_NOT_FOUND_ERROR, IS_A_DIRECTORY_ERROR,
    NOT_A_DIRECTORY_ERROR, OS_ERROR, PERMISSION_ERROR, RESOURCE_WARNING, STOP_ITERATION,
    TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::value::{self, ParamMeta, FALSE, NONE, TRUE};

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

/// `tempfile.gettempdir()`: the `str` view of `tempfile.tempdir`.
///
/// A program assignment wins (decoded to `str` when it is bytes); otherwise
/// the resolved default is cached so repeated reads share identity, exactly
/// like CPython's `global tempdir` caching.
fn gettempdir() -> i64 {
    if let Some(dir) = tempdir_override() {
        if dir != NONE {
            if value::as_str(dir).is_some() {
                return dir;
            }
            if let Some(bytes) = value::as_bytes(dir) {
                return value::str_alloc(String::from_utf8_lossy(bytes).into_owned());
            }
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "tempfile.tempdir must be str or bytes, not {}",
                    value::type_name(dir)
                ),
            );
        }
    }
    // `_gettempdir()` resolves the default once and stores it back into
    // `tempfile.tempdir`; an explicit `tempdir = None` forces a new probe.
    let resolved = get_default_tempdir(&[]);
    if exc::pending() {
        return NONE;
    }
    let text = match value::as_str(resolved) {
        Some(text) => value::str_alloc(text.to_string()),
        None => match value::as_bytes(resolved) {
            Some(bytes) => value::str_alloc(String::from_utf8_lossy(bytes).into_owned()),
            None => return NONE,
        },
    };
    let roots = heap::root_values(&[text]);
    globals::set_attribute_override("tempfile", "tempdir", text);
    heap::release_roots(roots);
    text
}

/// `tempfile.gettempdirb()`: the `bytes` view of `tempfile.tempdir`.
fn gettempdirb() -> i64 {
    if let Some(dir) = tempdir_override() {
        if dir != NONE {
            if value::as_bytes(dir).is_some() {
                return dir;
            }
            if let Some(text) = value::as_str(dir) {
                return value::bytes_new(text.as_bytes().to_vec());
            }
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "tempfile.tempdir must be str or bytes, not {}",
                    value::type_name(dir)
                ),
            );
        }
    }
    let text = gettempdir();
    if exc::pending() {
        return NONE;
    }
    let text = value::as_str(text).unwrap_or_default();
    value::bytes_new(text.as_bytes().to_vec())
}

/// Creates a directory named `prefix + <random> + suffix` below `dir`.
fn create_temp_dir(prefix: &str, suffix: &str, dir: Option<&str>) -> Result<PathBuf, TempError> {
    let base = base_dir(dir.map(str::to_string)).map_err(TempError::Raised)?;
    exclusive_temp_dir(&base, prefix, suffix)
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

/// Reads an optional path argument; `None`/missing select the default. A
/// value of another type reports `TypeError`.
fn optional_path(
    args: &[i64],
    index: usize,
    _function: &str,
    _name: &str,
) -> Result<Option<String>, i64> {
    match args.get(index).copied() {
        None | Some(NONE) => Ok(None),
        Some(arg) => path_argument(arg).map(|argument| Some(argument.text)),
    }
}

/// The built-in exception class CPython raises for an errno.
fn os_error_tag(error: &io::Error) -> u32 {
    #[cfg(unix)]
    match error.raw_os_error() {
        Some(libc::ENOENT) => FILE_NOT_FOUND_ERROR,
        Some(libc::EEXIST) => FILE_EXISTS_ERROR,
        Some(libc::EACCES | libc::EPERM) => PERMISSION_ERROR,
        Some(libc::ENOTDIR) => NOT_A_DIRECTORY_ERROR,
        Some(libc::EISDIR) => IS_A_DIRECTORY_ERROR,
        _ => OS_ERROR,
    }
    #[cfg(not(unix))]
    match error.kind() {
        io::ErrorKind::NotFound => FILE_NOT_FOUND_ERROR,
        io::ErrorKind::AlreadyExists => FILE_EXISTS_ERROR,
        io::ErrorKind::PermissionDenied => PERMISSION_ERROR,
        io::ErrorKind::NotADirectory => NOT_A_DIRECTORY_ERROR,
        io::ErrorKind::IsADirectory => IS_A_DIRECTORY_ERROR,
        _ => OS_ERROR,
    }
}

/// Raises the `OSError` subclass CPython uses for a filesystem failure on
/// `path`, in the `[Errno N] message: 'path'` form.
fn io_error(error: io::Error, path: &Path) -> i64 {
    let path = path.to_string_lossy();
    let text = error.to_string();
    let tag = os_error_tag(&error);
    match error.raw_os_error() {
        Some(code) => {
            let detail = text.split(" (os error").next().unwrap_or(text.as_str());
            exc::raise_with_message(tag, &format!("[Errno {code}] {detail}: '{path}'"))
        }
        None => exc::raise_with_message(tag, &format!("{text}: '{path}'")),
    }
}

// -------------------------------------------------------------------------
// Internal helpers mirrored from `Lib/tempfile.py`
// -------------------------------------------------------------------------

/// `tempfile.template`: the prefix `mkstemp`/`mkdtemp` use by default.
const TEMPLATE: &str = "tmp";

/// `tempfile.TMP_MAX`: how many candidate names an API tries before giving up.
const TMP_MAX: i64 = 20;

/// `_text_openflags`: `O_RDWR | O_CREAT | O_EXCL | O_NOFOLLOW`.
#[cfg(unix)]
const fn text_openflags() -> i64 {
    (libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW) as i64
}

#[cfg(not(unix))]
const fn text_openflags() -> i64 {
    0x2 | 0x100 | 0x400
}

/// `_bin_openflags`: the text flags plus the platform's `O_BINARY`.
#[cfg(unix)]
const fn bin_openflags() -> i64 {
    text_openflags()
}

#[cfg(not(unix))]
const fn bin_openflags() -> i64 {
    text_openflags() | 0x8000
}

/// Characters `_RandomNameSequence` draws from (CPython's alphabet).
const NAME_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";

/// State of the name generator. `_RandomNameSequence` reseeds when the
/// process id changes, so a forked child does not repeat its parent's names.
static NAME_STATE: LazyLock<std::sync::Mutex<(u32, u64)>> = LazyLock::new(|| {
    let seed = name_seed(std::process::id());
    std::sync::Mutex::new((std::process::id(), seed | 1))
});

fn name_seed(pid: u32) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    (u64::from(pid) << 32) ^ nanos ^ (NAME_ALPHABET.as_ptr() as usize as u64)
}

/// One eight-character random file-name component (`[a-z0-9_-]{8}`).
fn random_name() -> String {
    let pid = std::process::id();
    let mut state = NAME_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.0 != pid {
        state.0 = pid;
        state.1 = name_seed(pid) | 1;
    }
    let mut x = state.1;
    let mut name = String::with_capacity(8);
    for _ in 0..8 {
        // xorshift64: enough mixing for a name nobody can guess cheaply, with
        // no dependency beyond the standard library.
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        name.push(NAME_ALPHABET[(x % NAME_ALPHABET.len() as u64) as usize] as char);
    }
    state.1 = x;
    name
}

/// The caller's string type for a `tempfile` call (`_infer_return_type`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReturnKind {
    Text,
    Bytes,
}

fn bytes_type_value() -> i64 {
    value::type_class(value::bytes_new(Vec::new()))
}

fn str_type_value() -> i64 {
    value::type_class(value::str_alloc(String::new()))
}

fn return_kind_value(kind: ReturnKind) -> i64 {
    match kind {
        ReturnKind::Text => str_type_value(),
        ReturnKind::Bytes => bytes_type_value(),
    }
}

/// A path argument resolved to text plus the string type it arrived as.
#[derive(Clone)]
struct PathArgument {
    text: String,
    kind: ReturnKind,
}

/// Resolves a `str`, `bytes`, native `pathlib.Path` or arbitrary
/// `os.PathLike` (`__fspath__`) argument. `Err` carries the pending exception
/// value, or a freshly raised `TypeError`.
fn path_argument(raw: i64) -> Result<PathArgument, i64> {
    if let Some(text) = value::as_str(raw) {
        return Ok(PathArgument {
            text: text.to_string(),
            kind: ReturnKind::Text,
        });
    }
    if let Some(bytes) = value::as_bytes(raw) {
        return Ok(PathArgument {
            text: String::from_utf8_lossy(bytes).into_owned(),
            kind: ReturnKind::Bytes,
        });
    }
    if let Some(path) = value::as_path(raw) {
        return Ok(PathArgument {
            text: path.path().to_string(),
            kind: ReturnKind::Text,
        });
    }
    if let Some(instance) = value::as_instance(raw) {
        if value::class_lookup(instance.class, "__fspath__").is_some() {
            let roots = heap::root_values(&[raw]);
            let result = methods::call(raw, b"__fspath__".as_ptr(), 10, 0, std::ptr::null());
            if exc::pending() {
                heap::release_roots(roots);
                return Err(NONE);
            }
            let result_roots = heap::root_values(&[result]);
            let parsed = if let Some(text) = value::as_str(result) {
                Ok(PathArgument {
                    text: text.to_string(),
                    kind: ReturnKind::Text,
                })
            } else if let Some(path) = value::as_path(result) {
                Ok(PathArgument {
                    text: path.path().to_string(),
                    kind: ReturnKind::Text,
                })
            } else if let Some(bytes) = value::as_bytes(result) {
                Ok(PathArgument {
                    text: String::from_utf8_lossy(bytes).into_owned(),
                    kind: ReturnKind::Bytes,
                })
            } else {
                Err(exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "expected __fspath__() to return str or bytes, not {}",
                        value::type_name(result)
                    ),
                ))
            };
            heap::release_roots(result_roots);
            heap::release_roots(roots);
            return parsed;
        }
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "expected str, bytes or os.PathLike object, not {}",
            value::type_name(raw)
        ),
    ))
}

/// Classifies one `_infer_return_type` argument. `Err` carries a pending
/// exception (a `__fspath__` call failed).
fn infer_arg_kind(arg: i64) -> Result<Option<ReturnKind>, i64> {
    if arg == NONE {
        return Ok(None);
    }
    if value::as_bytes(arg).is_some() {
        return Ok(Some(ReturnKind::Bytes));
    }
    if value::as_str(arg).is_some() || value::as_path(arg).is_some() {
        return Ok(Some(ReturnKind::Text));
    }
    // `os.PathLike`: `__fspath__()` supplies the path whose type decides.
    if value::as_instance(arg)
        .and_then(|instance| value::class_lookup(instance.class, "__fspath__"))
        .is_some()
    {
        return path_argument(arg).map(|argument| Some(argument.kind));
    }
    // Not path-like: CPython treats every non-bytes object as text.
    Ok(Some(ReturnKind::Text))
}

/// `_infer_return_type(*args)`: the `str`/`bytes` type a call must return,
/// raising `TypeError` when bytes and non-bytes components are mixed.
fn infer_return_kind(args: &[i64]) -> Result<ReturnKind, i64> {
    let mut kind: Option<ReturnKind> = None;
    for &arg in args {
        let Some(next) = infer_arg_kind(arg)? else {
            continue;
        };
        match kind {
            None => kind = Some(next),
            Some(existing) if existing != next => {
                return Err(exc::raise_with_message(
                    TYPE_ERROR,
                    "Can't mix bytes and non-bytes in path components.",
                ));
            }
            Some(_) => {}
        }
    }
    Ok(kind.unwrap_or_else(|| {
        if current_tempdir_is_bytes() {
            ReturnKind::Bytes
        } else {
            ReturnKind::Text
        }
    }))
}

/// `tempfile.tempdir` as the program set it (`None`/str/bytes).
fn tempdir_override() -> Option<i64> {
    globals::attribute_override("tempfile", "tempdir")
}

fn current_tempdir_is_bytes() -> bool {
    tempdir_override().and_then(value::as_bytes).is_some()
}

/// The directory the default `dir` argument resolves to.
fn current_tempdir_text() -> Result<String, i64> {
    if let Some(dir) = tempdir_override() {
        if dir != NONE {
            if let Some(text) = value::as_str(dir) {
                return Ok(text.to_string());
            }
            if let Some(bytes) = value::as_bytes(dir) {
                return Ok(String::from_utf8_lossy(bytes).into_owned());
            }
            return Err(exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "tempfile.tempdir must be str or bytes, not {}",
                    value::type_name(dir)
                ),
            ));
        }
    }
    let text = gettempdir();
    if exc::pending() {
        return Err(NONE);
    }
    Ok(value::as_str(text).unwrap_or_default().to_string())
}

/// A path argument in either string type (`str`, `bytes` or
/// `pathlib.Path`); arbitrary `os.PathLike` instances are resolved by
/// [`path_argument`].
fn temp_name_text(arg: i64) -> Option<String> {
    if let Some(text) = value::as_str(arg) {
        return Some(text.to_string());
    }
    if let Some(bytes) = value::as_bytes(arg) {
        return Some(String::from_utf8_lossy(bytes).into_owned());
    }
    value::as_path(arg).map(|path| path.path().to_string())
}

/// Reads an optional path argument accepting `str`, `bytes` and `os.PathLike`.
fn optional_name(
    args: &[i64],
    index: usize,
    _function: &str,
    _name: &str,
) -> Result<Option<String>, i64> {
    match args.get(index).copied() {
        None | Some(NONE) => Ok(None),
        Some(arg) => path_argument(arg).map(|argument| Some(argument.text)),
    }
}

/// Lexical `os.path.abspath`: joins against the working directory and folds
/// `.`/`..` without touching the filesystem.
fn absolute(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    normalize(&joined)
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The base directory of a call: an explicit `dir` or the resolved tempdir.
fn base_dir(dir: Option<String>) -> Result<PathBuf, i64> {
    match dir {
        Some(dir) => Ok(absolute(Path::new(&dir))),
        None => Ok(absolute(Path::new(&current_tempdir_text()?))),
    }
}

fn name_value(kind: ReturnKind, path: &Path) -> i64 {
    let text = path.to_string_lossy().into_owned();
    match kind {
        ReturnKind::Text => value::str_alloc(text),
        ReturnKind::Bytes => value::bytes_new(text.into_bytes()),
    }
}

/// A filesystem failure together with the pending exception of a Python hook
/// (`_get_candidate_names`/`_candidate_tempdir_list`), so callers can report
/// whichever occurred instead of masking it.
#[derive(Debug)]
enum TempError {
    /// The failing operation plus the entry it was applied to, so the raised
    /// `OSError` names the candidate path CPython names, not the base
    /// directory the candidate was derived from.
    Io {
        error: io::Error,
        path: PathBuf,
    },
    /// Every candidate supplied by `_get_candidate_names` was unusable and no
    /// `TMP_MAX` attempt succeeded. Nothing was created.
    Exhausted(&'static str),
    Raised(i64),
}

/// Raises the error represented by `error`.
///
/// Exhaustion becomes CPython's `FileExistsError(EEXIST, ...)` with no
/// filename component: no path was ever created, so naming the base
/// directory (as an `io_error` would) would be misleading.
fn raise_temp_error(error: TempError) -> i64 {
    match error {
        TempError::Io { error, path } => io_error(error, &path),
        TempError::Exhausted(message) => {
            exc::raise_with_message(FILE_EXISTS_ERROR, &format!("[Errno 17] {message}"))
        }
        TempError::Raised(raised) => raised,
    }
}

/// Advances a Python iterator once: a native iterator (list/tuple/range/file
/// iterators), a generator, or an instance with `__next__`.
///
/// `StopIteration` becomes `Ok(None)`; any other exception is returned as
/// `Err` with the pending exception left in place.
fn next_iterator_item(iterator: i64) -> Result<Option<i64>, i64> {
    let generator = value::as_fiber(iterator)
        .map(|fiber| fiber.kind == crate::fiber::FiberKind::Generator)
        .unwrap_or(false);
    if value::as_iter(iterator).is_some() || generator {
        let mut out = NONE;
        if iter::iter_next(iterator, &mut out) == 1 {
            return Ok(Some(out));
        }
        if exc::pending() {
            return Err(NONE);
        }
        return Ok(None);
    }
    if let Some(instance) = value::as_instance(iterator) {
        if value::class_lookup(instance.class, "__next__").is_some() {
            let roots = heap::root_values(&[iterator]);
            let item = methods::call(iterator, b"__next__".as_ptr(), 9, 0, std::ptr::null());
            if exc::pending() {
                let tag = exc::pending_tag();
                heap::release_roots(roots);
                if tag == STOP_ITERATION as i64 {
                    exc::clear();
                    return Ok(None);
                }
                return Err(NONE);
            }
            heap::release_roots(roots);
            return Ok(Some(item));
        }
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!("'{}' object is not an iterator", value::type_name(iterator)),
    ))
}

/// The candidate names one API call consumes, in order.
///
/// A program assignment to `_get_candidate_names` (the test suite replaces it
/// with a deterministic iterator) is honoured exactly like CPython's call to
/// the hook; otherwise the process-wide random sequence supplies them. At most
/// `TMP_MAX` names are taken: an iterator that ends earlier finishes the batch,
/// and the caller then reports exhaustion without leaving a partial path.
fn candidate_names_batch() -> Result<Vec<String>, i64> {
    let Some(hook) = globals::attribute_override("tempfile", "_get_candidate_names") else {
        return Ok((0..TMP_MAX).map(|_| random_name()).collect());
    };
    let hook_roots = heap::root_values(&[hook]);
    let sequence = crate::core::builtins::call(hook, 0, std::ptr::null());
    if exc::pending() {
        heap::release_roots(hook_roots);
        return Err(NONE);
    }
    let sequence_roots = heap::root_values(&[sequence]);
    let iterator = iter::iter_new(sequence);
    if exc::pending() {
        heap::release_roots(sequence_roots);
        heap::release_roots(hook_roots);
        return Err(NONE);
    }
    let iterator_roots = heap::root_values(&[iterator]);
    let mut names = Vec::with_capacity(TMP_MAX as usize);
    while names.len() < TMP_MAX as usize {
        let item = match next_iterator_item(iterator) {
            Ok(Some(item)) => item,
            Ok(None) => break,
            Err(raised) => {
                heap::release_roots(iterator_roots);
                heap::release_roots(sequence_roots);
                heap::release_roots(hook_roots);
                return Err(raised);
            }
        };
        let item_roots = heap::root_values(&[item]);
        if let Some(text) = value::as_str(item) {
            names.push(text.to_string());
            heap::release_roots(item_roots);
            continue;
        }
        heap::release_roots(item_roots);
        heap::release_roots(iterator_roots);
        heap::release_roots(sequence_roots);
        heap::release_roots(hook_roots);
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "_get_candidate_names() yielded {}, not str",
                value::type_name(item)
            ),
        ));
    }
    heap::release_roots(iterator_roots);
    heap::release_roots(sequence_roots);
    heap::release_roots(hook_roots);
    Ok(names)
}

/// Creates an exclusively-opened `prefix + <candidate> + suffix` file in
/// `base`.
fn exclusive_temp_file(
    base: &Path,
    prefix: &str,
    suffix: &str,
) -> Result<(fs::File, PathBuf), TempError> {
    for name in candidate_names_batch().map_err(TempError::Raised)? {
        let candidate = base.join(format!("{prefix}{name}{suffix}"));
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(&candidate) {
            Ok(file) => return Ok((file, candidate)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(TempError::Io {
                    error,
                    path: candidate,
                })
            }
        }
    }
    Err(TempError::Exhausted("No usable temporary file name found"))
}

/// Creates a `prefix + <candidate> + suffix` directory in `base`.
fn exclusive_temp_dir(base: &Path, prefix: &str, suffix: &str) -> Result<PathBuf, TempError> {
    for name in candidate_names_batch().map_err(TempError::Raised)? {
        let candidate = base.join(format!("{prefix}{name}{suffix}"));
        match fs::create_dir(&candidate) {
            Ok(()) => {
                restrict_permissions(&candidate);
                return Ok(candidate);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(TempError::Io {
                    error,
                    path: candidate,
                })
            }
        }
    }
    Err(TempError::Exhausted(
        "No usable temporary directory name found",
    ))
}

/// `tempfile.mkdtemp(suffix=None, prefix=None, dir=None)`.
fn mkdtemp(args: &[i64]) -> i64 {
    let suffix = match optional_name(args, 0, "mkdtemp", "suffix") {
        Ok(value) => value.unwrap_or_default(),
        Err(error) => return error,
    };
    let prefix = match optional_name(args, 1, "mkdtemp", "prefix") {
        Ok(value) => value.unwrap_or_else(|| TEMPLATE.to_string()),
        Err(error) => return error,
    };
    let dir = match optional_name(args, 2, "mkdtemp", "dir") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let kind = match infer_return_kind(&args[..args.len().min(3)]) {
        Ok(kind) => kind,
        Err(error) => return error,
    };
    let base = match base_dir(dir) {
        Ok(base) => base,
        Err(error) => return error,
    };
    match exclusive_temp_dir(&base, &prefix, &suffix) {
        Ok(path) => name_value(kind, &path),
        Err(error) => raise_temp_error(error),
    }
}

/// `tempfile.mkstemp(suffix=None, prefix=None, dir=None, text=False)`: creates
/// an exclusively-opened temporary file and returns `(fd, path)`.
fn mkstemp(args: &[i64]) -> i64 {
    let suffix = match optional_name(args, 0, "mkstemp", "suffix") {
        Ok(value) => value.unwrap_or_default(),
        Err(error) => return error,
    };
    let prefix = match optional_name(args, 1, "mkstemp", "prefix") {
        Ok(value) => value.unwrap_or_else(|| TEMPLATE.to_string()),
        Err(error) => return error,
    };
    let dir = match optional_name(args, 2, "mkstemp", "dir") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let kind = match infer_return_kind(&args[..args.len().min(3)]) {
        Ok(kind) => kind,
        Err(error) => return error,
    };
    let base = match base_dir(dir) {
        Ok(base) => base,
        Err(error) => return error,
    };
    match exclusive_temp_file(&base, &prefix, &suffix) {
        Ok((file, path)) => {
            #[cfg(unix)]
            {
                use std::os::unix::io::IntoRawFd;
                let fd = i64::from(file.into_raw_fd());
                value::tuple_new(vec![
                    value::encode_int(fd).expect("fd fits"),
                    name_value(kind, &path),
                ])
            }
            #[cfg(not(unix))]
            {
                drop(file);
                value::tuple_new(vec![NONE, name_value(kind, &path)])
            }
        }
        Err(error) => raise_temp_error(error),
    }
}

/// Reads a mandatory-`str` optional argument of `mktemp`, whose signature does
/// not accept bytes or `os.PathLike` for the prefix/suffix components.
fn mktemp_text_arg(args: &[i64], index: usize, name: &str) -> Result<Option<String>, i64> {
    match args.get(index).copied() {
        None | Some(NONE) => Ok(None),
        Some(arg) => match value::as_str(arg) {
            Some(text) => Ok(Some(text.to_string())),
            None => Err(exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "mktemp() argument '{name}' must be str, not {}",
                    value::type_name(arg)
                ),
            )),
        },
    }
}

/// `tempfile.mktemp(suffix='', prefix='tmp', dir=None)`: the deprecated
/// name-only helper.
///
/// The name is chosen from the same random sequence `mkstemp` uses and is
/// checked against the filesystem (`symlink_metadata` also reports a dangling
/// link, so an uncreatable name is never returned), but the file is left for
/// the caller to create — which is exactly why CPython deprecates the
/// function. Unlike `mkstemp`, the result is not made absolute: CPython joins
/// the caller's `dir` verbatim.
fn mktemp(args: &[i64]) -> i64 {
    let suffix = match mktemp_text_arg(args, 0, "suffix") {
        Ok(value) => value.unwrap_or_default(),
        Err(error) => return error,
    };
    let prefix = match mktemp_text_arg(args, 1, "prefix") {
        Ok(value) => value.unwrap_or_else(|| TEMPLATE.to_string()),
        Err(error) => return error,
    };
    let dir = match args.get(2).copied() {
        None | Some(NONE) => match current_tempdir_text() {
            Ok(dir) => dir,
            Err(error) => return error,
        },
        Some(arg) => match path_argument(arg) {
            Ok(argument) if argument.kind == ReturnKind::Text => argument.text,
            Ok(_) => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "Can't mix bytes and non-bytes in path components.",
                )
            }
            Err(error) => return error,
        },
    };
    let names = match candidate_names_batch() {
        Ok(names) => names,
        Err(error) => return error,
    };
    let directory = Path::new(&dir);
    for name in names {
        let candidate = directory.join(format!("{prefix}{name}{suffix}"));
        if fs::symlink_metadata(&candidate).is_err() {
            return value::str_alloc(candidate.to_string_lossy().into_owned());
        }
    }
    exc::raise_with_message(
        FILE_EXISTS_ERROR,
        "[Errno 17] No usable temporary filename found",
    )
}

/// `_infer_return_type(*args)`: the `str`/`bytes` type object the callers must
/// return, raising `TypeError` when the components mix the two.
fn infer_return_type(args: &[i64]) -> i64 {
    match infer_return_kind(args) {
        Ok(kind) => return_kind_value(kind),
        Err(error) => error,
    }
}

/// `_RandomNameSequence`: an iterator of eight-character names.
fn random_name_sequence_class() -> i64 {
    const KEY: &str = "tempfile._RandomNameSequence.class";
    if let Some(class) = globals::registry_get(KEY) {
        return class;
    }
    let methods = vec![
        (
            "__iter__".to_string(),
            wrapper_method(random_name_iter_entry, 1, "__iter__"),
            MethodKind::Normal,
        ),
        (
            "__next__".to_string(),
            wrapper_method(random_name_next_entry, 1, "__next__"),
            MethodKind::Normal,
        ),
    ];
    let attrs = vec![(
        "characters".to_string(),
        value::str_alloc(String::from_utf8_lossy(NAME_ALPHABET).into_owned()),
    )];
    let mut live = Vec::with_capacity(methods.len() + 1);
    live.extend(methods.iter().map(|(_, function, _)| *function));
    live.extend(attrs.iter().map(|(_, item)| *item));
    let roots = heap::root_values(&live);
    let class = value::class_new_full("_RandomNameSequence".to_string(), NONE, methods, attrs);
    let class_roots = heap::root_values(&[class]);
    globals::registry_set(KEY, class);
    heap::release_roots(class_roots);
    heap::release_roots(roots);
    class
}

extern "C" fn random_name_iter_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self`; the iterator is its own
    // iterator (`__iter__` returns self).
    unsafe { *argv }
}

extern "C" fn random_name_next_entry(_argv: *const i64) -> i64 {
    value::str_alloc(random_name())
}

/// `_get_candidate_names()`: the process-wide `_RandomNameSequence`.
fn candidate_names() -> i64 {
    const KEY: &str = "tempfile._get_candidate_names.instance";
    if let Some(sequence) = globals::registry_get(KEY) {
        return sequence;
    }
    let class = random_name_sequence_class();
    let instance = value::instance_new(class);
    let roots = heap::root_values(&[instance]);
    globals::registry_set(KEY, instance);
    heap::release_roots(roots);
    instance
}

/// `_candidate_tempdir_list()`: the directories `_get_default_tempdir` probes.
fn default_candidate_strings() -> Vec<String> {
    let mut dirs = Vec::new();
    for key in ["TMPDIR", "TEMP", "TMP"] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                dirs.push(value);
            }
        }
    }
    if cfg!(windows) {
        if let Ok(profile) = std::env::var("USERPROFILE") {
            dirs.push(format!("{profile}\\AppData\\Local\\Temp"));
        }
        dirs.push("c:\\temp".to_string());
        dirs.push("c:\\tmp".to_string());
    } else {
        dirs.push("/tmp".to_string());
        dirs.push("/var/tmp".to_string());
        dirs.push("/usr/tmp".to_string());
    }
    match std::env::current_dir() {
        Ok(dir) => dirs.push(dir.to_string_lossy().into_owned()),
        Err(_) => dirs.push(".".to_string()),
    }
    dirs
}

fn candidate_tempdir_list() -> i64 {
    let dirs = default_candidate_strings();
    let items: Vec<i64> = dirs
        .iter()
        .map(|dir| value::str_alloc(dir.clone()))
        .collect();
    let roots = heap::root_values(&items);
    let list = value::list_new(items);
    heap::release_roots(roots);
    list
}

/// The string entries of a list/tuple of path-like values.
fn names_from_sequence(sequence: i64) -> Vec<String> {
    let items: Vec<i64> = if let Some(list) = value::as_list(sequence) {
        list.to_vec()
    } else if let Some(tuple) = value::as_tuple(sequence) {
        tuple.to_vec()
    } else {
        Vec::new()
    };
    items.into_iter().filter_map(temp_name_text).collect()
}

/// Probes `dir` by creating and removing a unique file, exactly like
/// `_get_default_tempdir`. `Ok(false)` means every candidate collided.
fn probe_tempdir(dir: &Path) -> io::Result<bool> {
    for _ in 0..TMP_MAX as usize {
        let candidate = dir.join(random_name());
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(mut file) => {
                let written = file.write_all(b"blat");
                drop(file);
                let _ = fs::remove_file(&candidate);
                return written.map(|()| true);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

/// `_get_default_tempdir(dirlist=None)`.
fn get_default_tempdir(args: &[i64]) -> i64 {
    let names = match args.first().copied() {
        Some(dirlist) if dirlist != NONE => names_from_sequence(dirlist),
        _ => match globals::attribute_override("tempfile", "_candidate_tempdir_list") {
            // The test suite replaces the candidate list to force its own
            // directory through this exact hook.
            Some(hook) => {
                let result = crate::core::builtins::call(hook, 0, std::ptr::null());
                if exc::pending() {
                    return NONE;
                }
                names_from_sequence(result)
            }
            None => default_candidate_strings(),
        },
    };
    for name in names {
        let dir = if name == "." {
            PathBuf::from(".")
        } else {
            absolute(Path::new(&name))
        };
        match probe_tempdir(&dir) {
            Ok(true) => return value::str_alloc(dir.to_string_lossy().into_owned()),
            Ok(false) | Err(_) => continue,
        }
    }
    exc::raise_with_message(
        FILE_NOT_FOUND_ERROR,
        "[Errno 2] No usable temporary directory found",
    )
}

/// `_mkstemp_inner(dir, pre, suf, flags, output_type)`.
fn mkstemp_inner(args: &[i64]) -> i64 {
    let dir = match optional_name(args, 0, "_mkstemp_inner", "dir") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let prefix = match optional_name(args, 1, "_mkstemp_inner", "pre") {
        Ok(value) => value.unwrap_or_default(),
        Err(error) => return error,
    };
    let suffix = match optional_name(args, 2, "_mkstemp_inner", "suf") {
        Ok(value) => value.unwrap_or_default(),
        Err(error) => return error,
    };
    // The descriptor is always opened binary; `flags` only changes the mode
    // bits the caller asked for, which the runtime fixes at 0600.
    let kind = match args.get(4).copied() {
        Some(output) if output == bytes_type_value() => ReturnKind::Bytes,
        _ => ReturnKind::Text,
    };
    let base = match base_dir(dir) {
        Ok(base) => base,
        Err(error) => return error,
    };
    match exclusive_temp_file(&base, &prefix, &suffix) {
        Ok((file, path)) => {
            #[cfg(unix)]
            {
                use std::os::unix::io::IntoRawFd;
                let fd = i64::from(file.into_raw_fd());
                value::tuple_new(vec![
                    value::encode_int(fd).expect("fd fits"),
                    name_value(kind, &path),
                ])
            }
            #[cfg(not(unix))]
            {
                drop(file);
                value::tuple_new(vec![NONE, name_value(kind, &path)])
            }
        }
        Err(error) => raise_temp_error(error),
    }
}

/// `tempfile.TemporaryFile(mode='w+b', buffering=-1, encoding=None,
/// newline=None, suffix=None, prefix=None, dir=None, *, errors=None)`.
///
/// The file is created, opened and immediately unlinked: the descriptor stays
/// usable while no directory entry remains, which is the POSIX behaviour of
/// CPython's implementation.
fn new_temporary_file(args: &[i64]) -> i64 {
    let mode = match args.first().copied() {
        None | Some(NONE) => "w+b".to_string(),
        Some(argument) => match value::as_str(argument) {
            Some(text) => text.to_string(),
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "TemporaryFile() argument 'mode' must be str, not '{}'",
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
    let encoding = match optional_text(args, 2, "TemporaryFile", "encoding") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let newline = match optional_text(args, 3, "TemporaryFile", "newline") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let suffix = match optional_name(args, 4, "TemporaryFile", "suffix") {
        Ok(value) => value.unwrap_or_default(),
        Err(error) => return error,
    };
    let prefix = match optional_name(args, 5, "TemporaryFile", "prefix") {
        Ok(value) => value.unwrap_or_else(|| TEMPLATE.to_string()),
        Err(error) => return error,
    };
    let dir = match optional_name(args, 6, "TemporaryFile", "dir") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let errors = match optional_text(args, 7, "TemporaryFile", "errors") {
        Ok(value) => value,
        Err(error) => return error,
    };
    if let Err(error) = validate_text_encoding(&mode, encoding) {
        return error;
    }
    let created = create_temp_file(&prefix, &suffix, dir.clone().as_deref());
    let path = match created {
        Ok(path) => path,
        Err(error) => return raise_temp_error(error),
    };
    let path_text = path.to_string_lossy().into_owned();
    let open_args = [
        value::str_alloc(path_text.clone()),
        value::str_alloc(mode),
        buffering,
        encoding,
        errors,
        newline,
        NONE,
        NONE,
    ];
    let file = crate::modules::io_open(&open_args);
    if exc::pending() {
        let _ = unlink_if_present(&path_text);
        return file;
    }
    // The open descriptor keeps the contents alive after the name is gone.
    let _ = unlink_if_present(&path_text);
    file
}

/// The `chmod` half of CPython's `_resetperms`: give the owner access back.
///
/// Symlinks are left alone (`_dont_follow_symlinks`), so recovering from a
/// permission error never changes the mode of the directory a link points at.
fn reset_permissions(path: &Path) {
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(true)
    {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Gives the owner access back to `path` and to every ancestor up to and
/// including `root`. The directory that contains `root` is never changed.
fn reset_path_and_ancestors(path: &Path, root: &Path) {
    reset_permissions(path);
    let mut current = path.parent();
    while let Some(parent) = current {
        if !parent.starts_with(root) {
            break;
        }
        reset_permissions(parent);
        current = parent.parent();
    }
}

/// Removes `path` recursively, recovering from a mode bit the caller removed.
///
/// `TemporaryDirectory.cleanup()` must remove a tree whose directories and
/// files were chmodded to any mode (the test suite walks all eight
/// permission triples), while never touching the mode of the directory that
/// contains the temporary directory. CPython reaches that through
/// `_rmtree_use_dir_fd` and its `onexc` handler, which resets the offending
/// directory's permissions and retries; this is the same policy without
/// directory descriptors: reset only paths inside `root` and retry.
fn remove_tree(path: &Path) -> io::Result<()> {
    remove_tree_at(path, path)
}

/// Lists `path`, retrying once after restoring owner access when a mode bit
/// denies reading it.
///
/// `Ok(None)` reports that the entry vanished or stopped being a directory
/// between the caller's metadata check and this call; the caller re-examines
/// it (and unlinks it when it became a file or a symlink) instead of treating
/// the race as a failure.
fn directory_entries(path: &Path, root: &Path) -> io::Result<Option<Vec<PathBuf>>> {
    for attempt in 0..2 {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotADirectory => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied && attempt == 0 => {
                // A directory with no read/execute bit cannot even be listed.
                reset_path_and_ancestors(path, root);
                continue;
            }
            Err(error) => return Err(error),
        };
        let mut paths = Vec::new();
        let mut denied = None;
        for entry in entries {
            match entry {
                Ok(entry) => paths.push(entry.path()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    denied = Some(error);
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        match denied {
            Some(_) if attempt == 0 => {
                // Restart the listing after restoring access so the full set
                // is returned, not the partially collected prefix.
                reset_path_and_ancestors(path, root);
                continue;
            }
            Some(error) => return Err(error),
            None => return Ok(Some(paths)),
        }
    }
    unreachable!("directory_entries retries once")
}

/// Recursive worker of [`remove_tree`]; `root` never has its parent chmodded.
///
/// A symlink found here is always unlinked, never traversed, even when it
/// points at a directory; only `symlink_metadata` decides what an entry is.
fn remove_tree_at(path: &Path, root: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            // The entry is unreachable because an ancestor lost its execute
            // bit; restore access inside the tree and retry.
            reset_path_and_ancestors(path, root);
            match fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return remove_file_at(path, root);
    }
    let Some(entries) = directory_entries(path, root)? else {
        // The entry was replaced by a file or a symlink, or removed, while it
        // was being listed: handle it as the non-directory it is now.
        return remove_file_at(path, root);
    };
    for entry in entries {
        remove_tree_at(&entry, root)?;
    }
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
            // Replaced by a file or a symlink after the listing; unlink the
            // replacement instead of failing the whole cleanup.
            match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            // Removing the entry needs write access to its parent. Only
            // directories inside the tree are reset: the parent of `root`
            // belongs to the caller and keeps its mode.
            reset_path_and_ancestors(path, root);
            match fs::remove_dir(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
                    remove_file_at(path, root)
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

/// Unlinks one file (or symlink) entry, resetting a parent inside `root`
/// when the directory lost its write bit, then retrying once.
fn remove_file_at(path: &Path, root: &Path) -> io::Result<()> {
    match remove_file_entry(path, root) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            reset_path_and_ancestors(path, root);
            remove_file_entry(path, root)
        }
        result => result,
    }
}

/// Single `remove_file` attempt used by [`remove_file_at`].
///
/// `IsADirectory` means the entry changed into a directory after the caller's
/// metadata check, so a real directory is removed recursively. A directory
/// symlink (Windows reports those as `IsADirectory`) is removed with
/// `remove_dir`, which unlinks the link itself and never touches its target.
fn remove_file_entry(path: &Path, root: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::IsADirectory => {
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => fs::remove_dir(path),
                Ok(metadata) if metadata.file_type().is_dir() => remove_tree_at(path, root),
                Ok(_) => fs::remove_file(path),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
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
    let kind = match infer_return_kind(&args[..args.len().min(3)]) {
        Ok(kind) => kind,
        Err(error) => return error,
    };
    let created = create_temp_dir(&prefix, &suffix, dir.clone().as_deref());
    let path = match created {
        Ok(path) => path,
        Err(error) => return raise_temp_error(error),
    };
    let ignore_cleanup_errors = bool_arg(args, 3, false);
    let delete = bool_arg(args, 4, true);
    let instance = value::instance_new(temporary_directory_class());
    let name = name_value(kind, &path);
    let roots = heap::root_values(&[name, instance]);
    value::instance_setattr(instance, "name".to_string(), name);
    value::instance_setattr(
        instance,
        "_ignore_cleanup_errors".to_string(),
        if ignore_cleanup_errors { TRUE } else { FALSE },
    );
    value::instance_setattr(
        instance,
        "_delete".to_string(),
        if delete { TRUE } else { FALSE },
    );
    heap::release_roots(roots);
    instance
}

/// Cleans up the directory named by the instance's `name` attribute.
///
/// An explicit `cleanup()` runs even for `delete=False`; the `_delete` flag
/// only suppresses the automatic cleanup in `__exit__`, matching CPython's
/// finalizer detach.
fn cleanup(instance: i64) -> i64 {
    let Some(name) = value::instance_attr(instance, "name") else {
        return NONE;
    };
    if name == NONE {
        return NONE;
    }
    let path = match path_argument(name) {
        Ok(argument) => argument.text,
        Err(error) => return error,
    };
    match remove_tree(Path::new(&path)) {
        Ok(()) => NONE,
        Err(error) => {
            if value::instance_attr(instance, "_ignore_cleanup_errors") == Some(TRUE) {
                NONE
            } else {
                io_error(error, Path::new(&path))
            }
        }
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
fn create_temp_file(prefix: &str, suffix: &str, dir: Option<&str>) -> Result<PathBuf, TempError> {
    let base = base_dir(dir.map(str::to_string)).map_err(TempError::Raised)?;
    let (file, path) = exclusive_temp_file(&base, prefix, suffix)?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(path)
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

/// Validates an explicit text-mode `encoding` before the temporary file is
/// created. CPython's `TextIOWrapper` resolves the name through the codec
/// registry and raises `LookupError` for an unknown one, leaving no file
/// behind.
fn validate_text_encoding(mode: &str, encoding: i64) -> Result<(), i64> {
    if mode.contains('b') || encoding == NONE {
        return Ok(());
    }
    let Some(name) = value::as_str(encoding) else {
        return Ok(());
    };
    let name = value::str_alloc(name.to_string());
    let roots = heap::root_values(&[name]);
    let _ = crate::modules::codecs::call("lookup", &[name]);
    let pending = exc::pending();
    heap::release_roots(roots);
    if pending {
        Err(NONE)
    } else {
        Ok(())
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

/// Whether a `NamedTemporaryFile`/`_TemporaryFileWrapper` is closed.
///
/// The wrapped file is authoritative (`closed` is delegated to it in
/// CPython); the wrapper's own flag covers file-like objects that are not
/// runtime file values.
fn wrapper_closed(instance: i64) -> bool {
    if let Some(file) = value::instance_attr(instance, "file") {
        if let Some(file) = value::as_file(file) {
            return file.closed();
        }
    }
    flag(instance, "closed")
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
    if let Err(error) = validate_text_encoding(&mode, encoding) {
        return error;
    }
    let path_start = 4.min(args.len());
    let path_end = 7.min(args.len());
    let kind = match infer_return_kind(&args[path_start..path_end]) {
        Ok(kind) => kind,
        Err(error) => return error,
    };

    let path = match create_temp_file(&prefix, &suffix, dir.clone().as_deref()) {
        Ok(path) => path,
        Err(error) => return raise_temp_error(error),
    };
    let name_attr = name_value(kind, &path);
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
    let name_value = name_attr;
    let mode_value = value::str_alloc(mode);
    let encoding_value = if binary || encoding == NONE {
        if binary {
            NONE
        } else {
            value::str_alloc("utf-8".to_string())
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

// -------------------------------------------------------------------------
// SpooledTemporaryFile
// -------------------------------------------------------------------------

/// Data/cursor attributes of the runtime's in-memory `io` streams. The
/// spooled file rewrites them directly because those streams do not expose
/// `truncate`.
const SPOOLED_IO_DATA: &str = "_hydro_io_data";
const SPOOLED_IO_POS: &str = "_hydro_io_pos";

/// `os.linesep` the text-mode `newlines` property reports.
const SPOOLED_LINESEP: &str = if cfg!(windows) { "\r\n" } else { "\n" };

/// Methods delegated to the current backing file. They are bound on the
/// instance so compiled calls reach the live `_file` even after it rolls over.
const SPOOLED_DELEGATED: [&str; 11] = [
    "read",
    "readline",
    "readlines",
    "seek",
    "tell",
    "flush",
    "isatty",
    "readable",
    "writable",
    "seekable",
    "detach",
];

fn spooled_attr(instance: i64, name: &str) -> i64 {
    value::instance_attr(instance, name).unwrap_or(NONE)
}

fn spooled_binary(instance: i64) -> bool {
    spooled_attr(instance, "_binary") == TRUE
}

fn spooled_file(instance: i64) -> Option<i64> {
    value::instance_attr(instance, "_file")
}

fn spooled_text_attr(instance: i64, name: &str) -> Option<String> {
    value::instance_attr(instance, name)
        .and_then(value::as_str)
        .map(str::to_string)
}

fn spooled_max_size(instance: i64) -> i64 {
    value::as_integer(spooled_attr(instance, "_max_size")).unwrap_or(0)
}

/// Binds the delegated stream methods of `file` on the instance.
fn spooled_bind_file(instance: i64, file: i64) {
    for name in SPOOLED_DELEGATED {
        let method = value::bound_method_new(file, name.to_string());
        let roots = heap::root_values(&[method]);
        value::instance_setattr(instance, name.to_string(), method);
        heap::release_roots(roots);
    }
}

/// Rolls the in-memory buffer over into a real temporary file, copying the
/// contents and restoring the cursor position like CPython's `rollover()`.
fn spooled_rollover(instance: i64) -> i64 {
    if spooled_attr(instance, "_rolled") == TRUE {
        return NONE;
    }
    let Some(old) = spooled_file(instance) else {
        return NONE;
    };
    let roots = heap::root_values(&[instance, old]);
    let pos = methods::call(old, b"tell".as_ptr(), 4, 0, std::ptr::null());
    if exc::pending() {
        heap::release_roots(roots);
        return NONE;
    }
    let content = methods::call(old, b"getvalue".as_ptr(), 8, 0, std::ptr::null());
    if exc::pending() {
        heap::release_roots(roots);
        return NONE;
    }
    let content_roots = heap::root_values(&[content, pos]);
    let binary = spooled_binary(instance);
    let prefix = spooled_text_attr(instance, "_prefix").unwrap_or_else(|| TEMPLATE.to_string());
    let suffix = spooled_text_attr(instance, "_suffix").unwrap_or_default();
    let dir = spooled_text_attr(instance, "_dir");
    let created = create_temp_file(&prefix, &suffix, dir.as_deref());
    match created {
        Ok(path) => {
            let path_text = path.to_string_lossy().into_owned();
            let mode = if binary {
                "w+b".to_string()
            } else {
                spooled_text_attr(instance, "_mode").unwrap_or_else(|| "w+".to_string())
            };
            // `io.open` rejects text-only options in binary mode.
            let encoding = if binary {
                NONE
            } else {
                spooled_attr(instance, "_encoding")
            };
            let errors = if binary {
                NONE
            } else {
                spooled_attr(instance, "_errors")
            };
            let newline = if binary {
                NONE
            } else {
                spooled_attr(instance, "_newline")
            };
            let open_args = [
                value::str_alloc(path_text.clone()),
                value::str_alloc(mode),
                NONE,
                encoding,
                errors,
                newline,
                NONE,
                NONE,
            ];
            let file = crate::modules::io_open(&open_args);
            if exc::pending() {
                let _ = unlink_if_present(&path_text);
            } else {
                let file_roots = heap::root_values(&[file]);
                let write_args = [content];
                let _ = methods::call(file, b"write".as_ptr(), 5, 1, write_args.as_ptr());
                if !exc::pending() {
                    // The whole old buffer was written, so the new file's
                    // cursor already sits at its end. Calling seek() is only
                    // needed when the old cursor pointed inside the buffer
                    // (and a write-only descriptor rejects it).
                    let at_end = match value::as_integer(pos) {
                        Some(pos) if binary => {
                            value::as_bytes(content).is_some_and(|bytes| bytes.len() as i64 == pos)
                        }
                        Some(pos) => value::as_str(content)
                            .is_some_and(|text| text.chars().count() as i64 == pos),
                        None => false,
                    };
                    if !at_end {
                        let seek_args = [pos];
                        let _ = methods::call(file, b"seek".as_ptr(), 4, 1, seek_args.as_ptr());
                    }
                }
                if exc::pending() {
                    let _ = unlink_if_present(&path_text);
                } else {
                    let _ = methods::call(old, b"close".as_ptr(), 5, 0, std::ptr::null());
                    let name = value::str_alloc(path_text);
                    let name_roots = heap::root_values(&[name]);
                    value::instance_setattr(instance, "_file".to_string(), file);
                    value::instance_setattr(instance, "_name".to_string(), name);
                    value::instance_setattr(instance, "_rolled".to_string(), TRUE);
                    value::instance_setattr(instance, "_fileno".to_string(), NONE);
                    spooled_bind_file(instance, file);
                    heap::release_roots(name_roots);
                }
                heap::release_roots(file_roots);
            }
        }
        Err(error) => {
            let _ = raise_temp_error(error);
        }
    }
    heap::release_roots(content_roots);
    heap::release_roots(roots);
    if exc::pending() {
        NONE
    } else {
        instance
    }
}

/// `_check`: rolls over once the cursor passes `max_size`.
fn spooled_check(instance: i64) -> i64 {
    if spooled_attr(instance, "_rolled") == TRUE {
        return NONE;
    }
    let max_size = spooled_max_size(instance);
    if max_size <= 0 {
        return NONE;
    }
    let Some(file) = spooled_file(instance) else {
        return NONE;
    };
    let roots = heap::root_values(&[instance, file]);
    let tell = methods::call(file, b"tell".as_ptr(), 4, 0, std::ptr::null());
    let over = !exc::pending() && value::as_integer(tell).is_some_and(|pos| pos > max_size);
    heap::release_roots(roots);
    if over {
        spooled_rollover(instance)
    } else {
        NONE
    }
}

/// `SpooledTemporaryFile.__init__`: allocates the in-memory buffer.
extern "C" fn spooled_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the signature binder passes the ten initializer slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 10) };
    let instance = args[0];
    let roots = heap::root_values(args);
    let slot_text = |slot: i64| -> Option<String> {
        if slot == NONE {
            None
        } else {
            value::as_str(slot).map(str::to_string)
        }
    };
    let mode = slot_text(args[2]).unwrap_or_else(|| "w+b".to_string());
    let binary = mode.contains('b');
    let max_size = match args[1] {
        slot if slot == NONE => 0,
        slot => match value::as_integer(slot) {
            Some(size) => size,
            None => {
                heap::release_roots(roots);
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "SpooledTemporaryFile() argument 'max_size' must be int, not '{}'",
                        value::type_name(slot)
                    ),
                );
            }
        },
    };
    // `buffering` is validated exactly like `NamedTemporaryFile`.
    let buffering = args[3];
    if buffering != NONE {
        match value::as_integer(buffering) {
            Some(size) if size >= -1 => {}
            Some(_) => {
                heap::release_roots(roots);
                return exc::raise_with_message(VALUE_ERROR, "invalid buffering size");
            }
            None => {
                heap::release_roots(roots);
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "SpooledTemporaryFile() argument 'buffering' must be int, not '{}'",
                        value::type_name(buffering)
                    ),
                );
            }
        }
    }
    let encoding_slot = args[4];
    let errors_slot = args[9];
    let newline = args[5];
    for (position, name) in [(4usize, "encoding"), (5, "newline"), (9, "errors")] {
        let slot = args[position];
        if slot != NONE && value::as_str(slot).is_none() {
            heap::release_roots(roots);
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "SpooledTemporaryFile() argument '{name}' must be str or None, not '{}'",
                    value::type_name(slot)
                ),
            );
        }
    }
    if let Err(error) = validate_text_encoding(&mode, encoding_slot) {
        heap::release_roots(roots);
        return error;
    }
    let suffix = slot_text(args[6]).unwrap_or_default();
    let prefix = slot_text(args[7]).unwrap_or_else(|| TEMPLATE.to_string());
    let dir = slot_text(args[8]);

    let file = if binary {
        crate::modules::call_native("io", "BytesIO", 0, std::ptr::null())
    } else {
        crate::modules::call_native("io", "StringIO", 0, std::ptr::null())
    };
    if exc::pending() {
        heap::release_roots(roots);
        return NONE;
    }
    let file_roots = heap::root_values(&[file]);
    let encoding = if encoding_slot == NONE {
        value::str_alloc("utf-8".to_string())
    } else {
        encoding_slot
    };
    let errors = if errors_slot == NONE {
        value::str_alloc("strict".to_string())
    } else {
        errors_slot
    };
    let mode_value = value::str_alloc(mode);
    let name_roots = heap::root_values(&[encoding, errors, mode_value]);
    let max_value = value::encode_int(max_size).unwrap_or(NONE);
    value::instance_setattr(instance, "_file".to_string(), file);
    value::instance_setattr(instance, "_rolled".to_string(), FALSE);
    value::instance_setattr(instance, "_closed".to_string(), FALSE);
    value::instance_setattr(instance, "_max_size".to_string(), max_value);
    value::instance_setattr(instance, "_mode".to_string(), mode_value);
    value::instance_setattr(
        instance,
        "_binary".to_string(),
        if binary { TRUE } else { FALSE },
    );
    value::instance_setattr(instance, "_encoding".to_string(), encoding);
    value::instance_setattr(instance, "_errors".to_string(), errors);
    value::instance_setattr(instance, "_newline".to_string(), newline);
    value::instance_setattr(instance, "_name".to_string(), NONE);
    value::instance_setattr(instance, "_fileno".to_string(), NONE);
    value::instance_setattr(instance, "_prefix".to_string(), value::str_alloc(prefix));
    value::instance_setattr(instance, "_suffix".to_string(), value::str_alloc(suffix));
    value::instance_setattr(
        instance,
        "_dir".to_string(),
        match dir {
            Some(dir) => value::str_alloc(dir),
            None => NONE,
        },
    );
    spooled_bind_file(instance, file);
    heap::release_roots(name_roots);
    heap::release_roots(file_roots);
    heap::release_roots(roots);
    NONE
}

/// `SpooledTemporaryFile.write(s)`: writes, then rolls over when needed.
extern "C" fn spooled_write_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `(self, data)`.
    let instance = unsafe { *argv };
    let data = unsafe { *argv.add(1) };
    let Some(file) = spooled_file(instance) else {
        return exc::raise_with_message(VALUE_ERROR, "I/O operation on closed file.");
    };
    let roots = heap::root_values(&[instance, file, data]);
    let args = [data];
    let result = methods::call(file, b"write".as_ptr(), 5, 1, args.as_ptr());
    if !exc::pending() {
        let _ = spooled_check(instance);
    }
    let outcome = if exc::pending() { NONE } else { result };
    heap::release_roots(roots);
    outcome
}

/// `SpooledTemporaryFile.writelines(iterable)`: writes item by item so the
/// rollover happens between iterations, exactly like CPython.
extern "C" fn spooled_writelines_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `(self, iterable)`.
    let instance = unsafe { *argv };
    let iterable = unsafe { *argv.add(1) };
    let roots = heap::root_values(&[instance, iterable]);
    let iterator = iter::iter_new(iterable);
    if exc::pending() {
        heap::release_roots(roots);
        return NONE;
    }
    let iterator_roots = heap::root_values(&[iterator]);
    loop {
        let mut item = NONE;
        if iter::iter_next(iterator, &mut item) == 0 {
            break;
        }
        if exc::pending() {
            break;
        }
        let item_roots = heap::root_values(&[item]);
        let Some(file) = spooled_file(instance) else {
            heap::release_roots(item_roots);
            break;
        };
        let file_roots = heap::root_values(&[file]);
        let args = [item];
        let _ = methods::call(file, b"write".as_ptr(), 5, 1, args.as_ptr());
        if !exc::pending() {
            let _ = spooled_check(instance);
        }
        heap::release_roots(file_roots);
        heap::release_roots(item_roots);
        if exc::pending() {
            break;
        }
    }
    heap::release_roots(iterator_roots);
    heap::release_roots(roots);
    NONE
}

/// `SpooledTemporaryFile.truncate(size=None)`.
extern "C" fn spooled_truncate_entry(argv: *const i64) -> i64 {
    // SAFETY: the signature binder passes `(self, size)` with the default.
    let instance = unsafe { *argv };
    let size = unsafe { *argv.add(1) };
    let roots = heap::root_values(&[instance, size]);
    let result = spooled_truncate(instance, size);
    heap::release_roots(roots);
    result
}

fn spooled_truncate(instance: i64, size: i64) -> i64 {
    let Some(file) = spooled_file(instance) else {
        return exc::raise_with_message(VALUE_ERROR, "I/O operation on closed file.");
    };
    let rolled = spooled_attr(instance, "_rolled") == TRUE;
    let current = if size == NONE {
        let roots = heap::root_values(&[instance, file]);
        let tell = methods::call(file, b"tell".as_ptr(), 4, 0, std::ptr::null());
        heap::release_roots(roots);
        if exc::pending() {
            return NONE;
        }
        value::as_integer(tell).unwrap_or(0)
    } else {
        match value::as_integer(size) {
            Some(size) => size,
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "an integer is required (got type {})",
                        value::type_name(size)
                    ),
                )
            }
        }
    };
    if current < 0 {
        return exc::raise_with_message(VALUE_ERROR, "negative size value");
    }
    if rolled {
        let roots = heap::root_values(&[instance, file]);
        let _ = methods::call(file, b"flush".as_ptr(), 5, 0, std::ptr::null());
        heap::release_roots(roots);
        if exc::pending() {
            return NONE;
        }
        let Some(path) = spooled_text_attr(instance, "_name") else {
            return NONE;
        };
        let target = u64::try_from(current).unwrap_or(0);
        return match std::fs::OpenOptions::new().write(true).open(&path) {
            Ok(handle) => match handle.set_len(target) {
                Ok(()) => value::encode_int(current).unwrap_or(NONE),
                Err(error) => io_error(error, Path::new(&path)),
            },
            Err(error) => io_error(error, Path::new(&path)),
        };
    }
    let max_size = spooled_max_size(instance);
    if max_size > 0 && current > max_size {
        let _ = spooled_rollover(instance);
        if exc::pending() {
            return NONE;
        }
        return spooled_truncate(instance, size);
    }
    let binary = spooled_binary(instance);
    let data = value::instance_attr(file, SPOOLED_IO_DATA).unwrap_or(NONE);
    let roots = heap::root_values(&[instance, file, data]);
    let target = usize::try_from(current).unwrap_or(0);
    if binary {
        let mut bytes = value::as_bytes(data)
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        bytes.resize(target, 0);
        let replacement = value::bytes_new(bytes);
        let replacement_roots = heap::root_values(&[replacement]);
        value::instance_setattr(file, SPOOLED_IO_DATA.to_string(), replacement);
        heap::release_roots(replacement_roots);
    } else {
        let text = value::as_str(data).unwrap_or_default().to_string();
        let mut chars: Vec<char> = text.chars().collect();
        chars.resize(target, '\0');
        let replacement = value::str_alloc(chars.into_iter().collect());
        let replacement_roots = heap::root_values(&[replacement]);
        value::instance_setattr(file, SPOOLED_IO_DATA.to_string(), replacement);
        heap::release_roots(replacement_roots);
    }
    let position = value::instance_attr(file, SPOOLED_IO_POS)
        .and_then(value::as_integer)
        .unwrap_or(0);
    if position > current {
        let clamped = value::encode_int(current).unwrap_or(NONE);
        let clamped_roots = heap::root_values(&[clamped]);
        value::instance_setattr(file, SPOOLED_IO_POS.to_string(), clamped);
        heap::release_roots(clamped_roots);
    }
    heap::release_roots(roots);
    value::encode_int(current).unwrap_or(NONE)
}

/// `SpooledTemporaryFile.close()`: closes the backing file, releases any
/// descriptor `fileno()` handed out, and removes the rolled-over file.
extern "C" fn spooled_close_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self`.
    let instance = unsafe { *argv };
    let roots = heap::root_values(&[instance]);
    let result = spooled_close(instance);
    heap::release_roots(roots);
    result
}

fn spooled_close(instance: i64) -> i64 {
    if spooled_attr(instance, "_closed") == TRUE {
        return NONE;
    }
    if let Some(file) = spooled_file(instance) {
        let roots = heap::root_values(&[file]);
        let _ = methods::call(file, b"close".as_ptr(), 5, 0, std::ptr::null());
        heap::release_roots(roots);
    }
    if let Some(fd) = value::as_integer(spooled_attr(instance, "_fileno")) {
        #[cfg(unix)]
        {
            // SAFETY: the descriptor was opened by `spooled_open_fileno`.
            unsafe {
                libc::close(fd as libc::c_int);
            }
        }
        #[cfg(not(unix))]
        let _ = fd;
    }
    if let Some(path) = spooled_text_attr(instance, "_name") {
        let _ = unlink_if_present(&path);
    }
    value::instance_setattr(instance, "_closed".to_string(), TRUE);
    NONE
}

/// Opens a second descriptor on the rolled-over path for `fileno()`.
#[cfg(unix)]
fn spooled_open_fileno(path: &str) -> i64 {
    let Ok(c_path) = std::ffi::CString::new(path) else {
        return -1;
    };
    // SAFETY: `c_path` is NUL-terminated and the call does not retain it.
    i64::from(unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR) })
}

#[cfg(not(unix))]
fn spooled_open_fileno(_path: &str) -> i64 {
    -1
}

/// `SpooledTemporaryFile.fileno()`: rolls over and returns a real descriptor.
extern "C" fn spooled_fileno_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self`.
    let instance = unsafe { *argv };
    let roots = heap::root_values(&[instance]);
    let result = spooled_fileno(instance);
    heap::release_roots(roots);
    result
}

fn spooled_fileno(instance: i64) -> i64 {
    let cached = spooled_attr(instance, "_fileno");
    if cached != NONE {
        return cached;
    }
    if spooled_attr(instance, "_rolled") != TRUE {
        let _ = spooled_rollover(instance);
        if exc::pending() {
            return NONE;
        }
    }
    let Some(path) = spooled_text_attr(instance, "_name") else {
        return exc::raise_with_message(OS_ERROR, "[Errno 9] Bad file descriptor");
    };
    let fd = spooled_open_fileno(&path);
    if fd < 0 {
        return io_error(io::Error::last_os_error(), Path::new(&path));
    }
    let encoded = value::encode_int(fd).unwrap_or(NONE);
    let roots = heap::root_values(&[encoded]);
    value::instance_setattr(instance, "_fileno".to_string(), encoded);
    heap::release_roots(roots);
    encoded
}

extern "C" fn spooled_enter_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self`.
    let instance = unsafe { *argv };
    if spooled_attr(instance, "_closed") == TRUE {
        return exc::raise_with_message(VALUE_ERROR, "Cannot enter context with closed file");
    }
    instance
}

extern "C" fn spooled_exit_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `(self, exc, value, tb)`.
    let instance = unsafe { *argv };
    let roots = heap::root_values(&[instance]);
    let result = spooled_close(instance);
    heap::release_roots(roots);
    result
}

extern "C" fn spooled_del_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self`.
    let instance = unsafe { *argv };
    let roots = heap::root_values(&[instance]);
    if spooled_attr(instance, "_closed") != TRUE {
        crate::modules::warnings::warn_category(
            "Unclosed file <tempfile.SpooledTemporaryFile>",
            RESOURCE_WARNING,
        );
    }
    let result = spooled_close(instance);
    heap::release_roots(roots);
    result
}

extern "C" fn spooled_iter_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self`.
    let instance = unsafe { *argv };
    let Some(file) = spooled_file(instance) else {
        return NONE;
    };
    let roots = heap::root_values(&[instance, file]);
    let result = iter::iter_new(file);
    heap::release_roots(roots);
    result
}

extern "C" fn spooled_next_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self`.
    let instance = unsafe { *argv };
    let Some(file) = spooled_file(instance) else {
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

/// Placeholder for a delegated name: instance-bound methods serve the calls,
/// but the name must still exist in the class namespace for `dir()`.
extern "C" fn spooled_stub_entry(_argv: *const i64) -> i64 {
    exc::raise_with_message(ATTRIBUTE_ERROR, "SpooledTemporaryFile method unavailable")
}

extern "C" fn spooled_mode_entry(argv: *const i64) -> i64 {
    // SAFETY: a property getter receives the instance.
    let instance = unsafe { *argv };
    if spooled_attr(instance, "_rolled") == TRUE && spooled_binary(instance) {
        return value::str_alloc("rb+".to_string());
    }
    value::str_alloc(spooled_text_attr(instance, "_mode").unwrap_or_else(|| "w+b".to_string()))
}

extern "C" fn spooled_name_entry(argv: *const i64) -> i64 {
    // SAFETY: a property getter receives the instance.
    spooled_attr(unsafe { *argv }, "_name")
}

extern "C" fn spooled_closed_entry(argv: *const i64) -> i64 {
    // SAFETY: a property getter receives the instance.
    spooled_attr(unsafe { *argv }, "_closed")
}

/// `encoding`/`errors`: text mode only, matching the real file objects.
fn spooled_text_property(instance: i64, stored: &str, name: &str) -> i64 {
    if spooled_binary(instance) {
        return exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("'_io.BufferedRandom' object has no attribute '{name}'"),
        );
    }
    spooled_attr(instance, stored)
}

extern "C" fn spooled_encoding_entry(argv: *const i64) -> i64 {
    // SAFETY: a property getter receives the instance.
    spooled_text_property(unsafe { *argv }, "_encoding", "encoding")
}

extern "C" fn spooled_errors_entry(argv: *const i64) -> i64 {
    // SAFETY: a property getter receives the instance.
    spooled_text_property(unsafe { *argv }, "_errors", "errors")
}

extern "C" fn spooled_newlines_entry(argv: *const i64) -> i64 {
    // SAFETY: a property getter receives the instance.
    let instance = unsafe { *argv };
    if spooled_binary(instance) {
        return exc::raise_with_message(
            ATTRIBUTE_ERROR,
            "'_io.BufferedRandom' object has no attribute 'newlines'",
        );
    }
    value::str_alloc(SPOOLED_LINESEP.to_string())
}

/// Builds the cached `SpooledTemporaryFile` class.
///
/// Inherits `io.IOBase` so `isinstance(f, io.IOBase)` holds, and declares
/// every IOBase/BufferedIOBase/TextIOBase name so the class namespace matches
/// CPython's `dir()`.
fn spooled_temporary_file_class() -> i64 {
    const KEY: &str = "tempfile.SpooledTemporaryFile.class";
    if let Some(class) = globals::registry_get(KEY) {
        return class;
    }
    let init = value::native_function_new("tempfile", "SpooledTemporaryFile.__init__", usize::MAX);
    value::function_set_signature(
        init,
        vec![
            ParamMeta {
                name: "self".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
            ParamMeta {
                name: "max_size".to_string(),
                kind: ParamKind::Positional,
                default: Some(NONE),
            },
            ParamMeta {
                name: "mode".to_string(),
                kind: ParamKind::Positional,
                default: Some(NONE),
            },
            ParamMeta {
                name: "buffering".to_string(),
                kind: ParamKind::Positional,
                default: Some(NONE),
            },
            ParamMeta {
                name: "encoding".to_string(),
                kind: ParamKind::Positional,
                default: Some(NONE),
            },
            ParamMeta {
                name: "newline".to_string(),
                kind: ParamKind::Positional,
                default: Some(NONE),
            },
            ParamMeta {
                name: "suffix".to_string(),
                kind: ParamKind::Positional,
                default: Some(NONE),
            },
            ParamMeta {
                name: "prefix".to_string(),
                kind: ParamKind::Positional,
                default: Some(NONE),
            },
            ParamMeta {
                name: "dir".to_string(),
                kind: ParamKind::Positional,
                default: Some(NONE),
            },
            ParamMeta {
                name: "errors".to_string(),
                kind: ParamKind::Positional,
                default: Some(NONE),
            },
        ],
    );
    value::function_set_full_entry(init, spooled_init_entry as *const () as usize);

    let truncate = value::native_function_new("tempfile", "SpooledTemporaryFile.truncate", 2);
    value::function_set_signature(
        truncate,
        vec![
            ParamMeta {
                name: "self".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
            ParamMeta {
                name: "size".to_string(),
                kind: ParamKind::Positional,
                default: Some(NONE),
            },
        ],
    );
    value::function_set_full_entry(truncate, spooled_truncate_entry as *const () as usize);

    let mut methods = vec![
        ("__init__".to_string(), init, MethodKind::Normal),
        (
            "write".to_string(),
            wrapper_method(spooled_write_entry, 2, "write"),
            MethodKind::Normal,
        ),
        (
            "writelines".to_string(),
            wrapper_method(spooled_writelines_entry, 2, "writelines"),
            MethodKind::Normal,
        ),
        ("truncate".to_string(), truncate, MethodKind::Normal),
        (
            "close".to_string(),
            wrapper_method(spooled_close_entry, 1, "close"),
            MethodKind::Normal,
        ),
        (
            "fileno".to_string(),
            wrapper_method(spooled_fileno_entry, 1, "fileno"),
            MethodKind::Normal,
        ),
        (
            "__enter__".to_string(),
            wrapper_method(spooled_enter_entry, 1, "__enter__"),
            MethodKind::Normal,
        ),
        (
            "__exit__".to_string(),
            wrapper_method(spooled_exit_entry, 4, "__exit__"),
            MethodKind::Normal,
        ),
        (
            "__del__".to_string(),
            wrapper_method(spooled_del_entry, 1, "__del__"),
            MethodKind::Normal,
        ),
        (
            "__iter__".to_string(),
            wrapper_method(spooled_iter_entry, 1, "__iter__"),
            MethodKind::Normal,
        ),
        (
            "__next__".to_string(),
            wrapper_method(spooled_next_entry, 1, "__next__"),
            MethodKind::Normal,
        ),
    ];
    for name in [
        "read",
        "read1",
        "readinto",
        "readinto1",
        "readline",
        "readlines",
        "seek",
        "tell",
        "flush",
        "isatty",
        "readable",
        "writable",
        "seekable",
        "detach",
    ] {
        methods.push((
            name.to_string(),
            wrapper_method(spooled_stub_entry, 1, name),
            MethodKind::Normal,
        ));
    }
    let attrs = vec![
        (
            "mode".to_string(),
            value::property_new(wrapper_method(spooled_mode_entry, 1, "mode"), NONE, NONE),
        ),
        (
            "name".to_string(),
            value::property_new(wrapper_method(spooled_name_entry, 1, "name"), NONE, NONE),
        ),
        (
            "closed".to_string(),
            value::property_new(
                wrapper_method(spooled_closed_entry, 1, "closed"),
                NONE,
                NONE,
            ),
        ),
        (
            "encoding".to_string(),
            value::property_new(
                wrapper_method(spooled_encoding_entry, 1, "encoding"),
                NONE,
                NONE,
            ),
        ),
        (
            "errors".to_string(),
            value::property_new(
                wrapper_method(spooled_errors_entry, 1, "errors"),
                NONE,
                NONE,
            ),
        ),
        (
            "newlines".to_string(),
            value::property_new(
                wrapper_method(spooled_newlines_entry, 1, "newlines"),
                NONE,
                NONE,
            ),
        ),
    ];
    let base = crate::modules::module_get_by_name("io", "IOBase");
    let mut live = Vec::with_capacity(methods.len() + attrs.len() + 1);
    live.extend(methods.iter().map(|(_, function, _)| *function));
    live.extend(attrs.iter().map(|(_, item)| *item));
    live.push(base);
    let roots = heap::root_values(&live);
    let class = value::class_new_full("SpooledTemporaryFile".to_string(), base, methods, attrs);
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
        "_RandomNameSequence" => random_name_sequence_class(),
        "SpooledTemporaryFile" => spooled_temporary_file_class(),
        "TMP_MAX" => value::encode_int(TMP_MAX).expect("TMP_MAX fits"),
        "template" => value::str_alloc(TEMPLATE.to_string()),
        // `tempfile.tempdir` starts out `None`; a program assignment is
        // resolved by the module attribute override before this dispatch.
        "tempdir" => NONE,
        "_bin_openflags" => value::encode_int(bin_openflags()).expect("flags fit"),
        "_text_openflags" => value::encode_int(text_openflags()).expect("flags fit"),
        // Hydro always unlinks through the resolved path, and never uses
        // `O_TMPFILE`, so both capability flags are truthfully false.
        "_O_TMPFILE_WORKS" => FALSE,
        "_rmtree_use_dir_fd" => FALSE,
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
        "gettempdirb" => gettempdirb(),
        "gettempprefix" => value::str_alloc(TEMPLATE.to_string()),
        "gettempprefixb" => value::bytes_new(TEMPLATE.as_bytes().to_vec()),
        "mktemp" => mktemp(args),
        "mkdtemp" => mkdtemp(args),
        "mkstemp" => mkstemp(args),
        "_infer_return_type" => infer_return_type(args),
        "_candidate_tempdir_list" => candidate_tempdir_list(),
        "_get_default_tempdir" => get_default_tempdir(args),
        "_get_candidate_names" => candidate_names(),
        "_mkstemp_inner" => mkstemp_inner(args),
        "TemporaryFile" => new_temporary_file(args),
        "TemporaryDirectory" => new_temporary_directory(args),
        "NamedTemporaryFile" => new_named_temporary_file(args),
        "_tempdir_enter" => match args.first() {
            // A `NamedTemporaryFile` wrapper enters as itself; CPython's
            // `TemporaryDirectory.__enter__` instead returns the directory
            // name, so `with TemporaryDirectory() as d:` binds a `str`.
            // `_TemporaryFileWrapper.__enter__` calls `self.file.__enter__()`,
            // which rejects a closed file, so a second `with` after `close()`
            // raises `ValueError` rather than silently succeeding.
            Some(&instance) if is_named_wrapper(instance) => {
                if wrapper_closed(instance) {
                    exc::raise_with_message(VALUE_ERROR, "I/O operation on closed file.")
                } else {
                    instance
                }
            }
            Some(&instance) => value::instance_attr(instance, "name").unwrap_or(instance),
            None => exc::raise_with_message(TYPE_ERROR, "__enter__() missing self"),
        },
        "_tempdir_exit" => match args.first() {
            Some(&instance) if is_named_wrapper(instance) => {
                close_wrapped(instance);
                cleanup_named(instance)
            }
            Some(&instance) => {
                if value::instance_attr(instance, "_delete") == Some(FALSE) {
                    NONE
                } else {
                    cleanup(instance)
                }
            }
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

    /// `TemporaryDirectory.cleanup()` recovers from modes that forbid reading
    /// and unlinking (CPython's `_resetperms` retry), and it never changes the
    /// mode of the directory that contains the temporary directory.
    #[cfg(unix)]
    #[test]
    fn cleanup_recovers_from_read_only_modes_without_touching_the_parent() {
        use std::os::unix::fs::PermissionsExt;

        exc::init();
        let parent = create_temp_dir("hydro-parent-", "", None).expect("parent directory");
        let parent_mode = fs::metadata(&parent)
            .expect("parent metadata")
            .permissions()
            .mode()
            & 0o777;
        let args = [
            NONE,
            text("hydro-readonly-"),
            value::str_alloc(parent.to_string_lossy().into_owned()),
        ];
        let instance = call("TemporaryDirectory", &args);
        assert_clean(instance);
        let path = PathBuf::from(name_of(instance));
        let nested = path.join("nested");
        fs::create_dir(&nested).expect("nested directory");
        let child = nested.join("file.txt");
        fs::write(&child, b"payload").expect("nested file");

        // Every entry loses a different bit: the file cannot be opened, the
        // nested directory cannot be listed, the root cannot be written.
        fs::set_permissions(&child, fs::Permissions::from_mode(0o000)).expect("chmod file");
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o500)).expect("chmod nested");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).expect("chmod root");

        assert_eq!(call("_tempdir_cleanup", &[instance]), NONE);
        if exc::pending() {
            let message = value::to_string(exc::pending_value());
            exc::clear();
            panic!("cleanup raised: {message}");
        }
        assert!(!path.exists(), "read-only tree is removed");
        assert_eq!(
            fs::metadata(&parent)
                .expect("parent metadata")
                .permissions()
                .mode()
                & 0o777,
            parent_mode,
            "cleanup must not chmod the containing directory"
        );

        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).expect("restore parent");
        fs::remove_dir_all(&parent).expect("remove parent");
    }

    /// A directory symlink inside the temporary directory is unlinked, never
    /// followed, so the link target and its contents survive cleanup.
    #[cfg(unix)]
    #[test]
    fn cleanup_unlinks_directory_symlinks_without_following_them() {
        exc::init();
        let target = create_temp_dir("hydro-link-target-", "", None).expect("target directory");
        let target_file = target.join("payload.txt");
        fs::write(&target_file, b"keep me").expect("target file");

        let instance = call("TemporaryDirectory", &[]);
        assert_clean(instance);
        let path = PathBuf::from(name_of(instance));
        std::os::unix::fs::symlink(&target, path.join("link")).expect("directory symlink");

        assert_eq!(call("_tempdir_cleanup", &[instance]), NONE);
        assert!(!exc::pending());
        assert!(!path.exists());
        assert!(target_file.is_file(), "link target is untouched");

        fs::remove_dir_all(&target).expect("remove target");
    }

    /// Entering the context of an already-closed wrapper matches
    /// `file.__enter__()`: `ValueError`, not a second successful `with`.
    #[test]
    fn named_temporary_file_context_manager_rejects_a_closed_file() {
        exc::init();
        let instance = named_file("w+b", false, true);
        let name = value::as_str(attribute(instance, "name"))
            .expect("str name")
            .to_string();
        assert_eq!(method(instance, "close", &[]), NONE);
        assert!(!exc::pending());
        assert_eq!(method(instance, "__enter__", &[]), NONE);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        fs::remove_file(&name).expect("cleanup test file");
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
