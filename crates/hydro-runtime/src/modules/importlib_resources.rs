//! `importlib.resources`: resource access through a filesystem `Traversable`.
//!
//! `files(package)` resolves the package exactly like `importlib.util`
//! (`sys.path` discovery plus the runtime module registry) and returns a
//! native object whose methods operate on the real directory: `joinpath`,
//! `read_text`, `read_bytes`, `iterdir`, `is_file`, `is_dir`, `name`,
//! `__fspath__` and `open`.
//!
//! This is a filesystem-backed implementation. Zip archives and other
//! importers are not supported by Hydro's loader, so `files()` reports the
//! package location it actually found instead of fabricating a container.

use std::path::{Path, PathBuf};

use hydro_ir::MethodKind;

use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, LOOKUP_ERROR, OS_ERROR, TYPE_ERROR, UNICODE_DECODE_ERROR, VALUE_ERROR,
};
use crate::value::{self, NONE};

use super::importlib_util as util;

const MODULE: &str = "importlib.resources";

const TRAVERSABLE_INIT: &str = "_traversable_init";
const TRAVERSABLE_JOINPATH: &str = "_traversable_joinpath";
const TRAVERSABLE_READ_TEXT: &str = "_traversable_read_text";
const TRAVERSABLE_READ_BYTES: &str = "_traversable_read_bytes";
const TRAVERSABLE_ITERDIR: &str = "_traversable_iterdir";
const TRAVERSABLE_IS_FILE: &str = "_traversable_is_file";
const TRAVERSABLE_IS_DIR: &str = "_traversable_is_dir";
const TRAVERSABLE_NAME: &str = "_traversable_name";
const TRAVERSABLE_FSPATH: &str = "_traversable_fspath";
const TRAVERSABLE_OPEN: &str = "_traversable_open";

/// Dispatches a native `importlib.resources` function. Arity is enforced by
/// the central spec before this point.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "files" => files(args),
        TRAVERSABLE_INIT => traversable_init(args),
        TRAVERSABLE_JOINPATH => joinpath(args),
        TRAVERSABLE_READ_TEXT => read_text(args),
        TRAVERSABLE_READ_BYTES => read_bytes(args),
        TRAVERSABLE_ITERDIR => iterdir(args),
        TRAVERSABLE_IS_FILE => is_file(args),
        TRAVERSABLE_IS_DIR => is_dir(args),
        TRAVERSABLE_NAME => name(args),
        TRAVERSABLE_FSPATH => fspath(args),
        TRAVERSABLE_OPEN => open(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '{MODULE}' has no attribute '{function}'"),
        ),
    }
}

/// Materialises a `importlib.resources` module constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "Traversable" => traversable_class(),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '{MODULE}' has no attribute '{name}'"),
        ),
    }
}

fn traversable_class() -> i64 {
    const KEY: &str = "importlib.resources.Traversable";
    if let Some(class) = globals::registry_get(KEY) {
        return class;
    }
    let methods = vec![
        (
            "__init__".to_string(),
            native(TRAVERSABLE_INIT, 2),
            MethodKind::Normal,
        ),
        (
            "joinpath".to_string(),
            native(TRAVERSABLE_JOINPATH, usize::MAX),
            MethodKind::Normal,
        ),
        (
            "read_text".to_string(),
            native(TRAVERSABLE_READ_TEXT, 3),
            MethodKind::Normal,
        ),
        (
            "read_bytes".to_string(),
            native(TRAVERSABLE_READ_BYTES, 1),
            MethodKind::Normal,
        ),
        (
            "iterdir".to_string(),
            native(TRAVERSABLE_ITERDIR, 1),
            MethodKind::Normal,
        ),
        (
            "is_file".to_string(),
            native(TRAVERSABLE_IS_FILE, 1),
            MethodKind::Normal,
        ),
        (
            "is_dir".to_string(),
            native(TRAVERSABLE_IS_DIR, 1),
            MethodKind::Normal,
        ),
        (
            "__fspath__".to_string(),
            native(TRAVERSABLE_FSPATH, 1),
            MethodKind::Normal,
        ),
        (
            "open".to_string(),
            native(TRAVERSABLE_OPEN, 5),
            MethodKind::Normal,
        ),
    ];
    let method_values: Vec<i64> = methods.iter().map(|(_, value, _)| *value).collect();
    let method_roots = crate::heap::root_values(&method_values);
    // `name` is a property, like CPython's `Traversable.name` (reading it
    // yields the final path component; it is not a callable).
    let attrs = vec![(
        "name".to_string(),
        value::property_new(native(TRAVERSABLE_NAME, 1), NONE, NONE),
    )];
    let attr_roots =
        crate::heap::root_values(&attrs.iter().map(|(_, value)| *value).collect::<Vec<_>>());
    let class = value::class_new_full("Traversable".to_string(), NONE, methods, attrs);
    let class_roots = crate::heap::root_values(&[class]);
    globals::registry_set(KEY, class);
    crate::heap::release_roots(class_roots);
    crate::heap::release_roots(attr_roots);
    crate::heap::release_roots(method_roots);
    class
}

fn native(function: &'static str, arity: usize) -> i64 {
    value::native_function_new(MODULE, function, arity)
}

fn traversable(path: PathBuf) -> i64 {
    let instance = value::instance_new(traversable_class());
    value::instance_setattr(
        instance,
        "_path".to_string(),
        value::str_alloc(path.to_string_lossy().into_owned()),
    );
    instance
}

fn path_of(receiver: i64) -> Result<PathBuf, i64> {
    match value::instance_attr(receiver, "_path").and_then(value::as_str) {
        Some(path) => Ok(PathBuf::from(path)),
        None => Err(exc::raise_with_message(
            VALUE_ERROR,
            "Traversable instance is not bound to a path",
        )),
    }
}

/// `files(package)`.
fn files(args: &[i64]) -> i64 {
    let package = args[0];
    let name = if let Some(text) = value::as_str(package) {
        text.to_string()
    } else if let Some(module) = value::as_module(package) {
        module.name().to_string()
    } else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "files() argument must be a module or str, not '{}'",
                value::type_name(package)
            ),
        );
    };
    if name.is_empty() {
        return exc::raise_with_message(VALUE_ERROR, "Empty module name");
    }
    if util::resolve_module(&name).is_none() {
        return crate::builtins::module_not_found(&name);
    }
    let Some(location) = util::package_location(&name) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("'{name}' is not a package with a filesystem location"),
        );
    };
    traversable(location)
}

fn traversable_init(args: &[i64]) -> i64 {
    let receiver = args[0];
    let Some(path) = value::as_path_like(args[1]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "Traversable() argument must be str or os.PathLike, not '{}'",
                value::type_name(args[1])
            ),
        );
    };
    value::instance_setattr(
        receiver,
        "_path".to_string(),
        value::str_alloc(path.to_string()),
    );
    NONE
}

/// `joinpath(*descendants)`.
fn joinpath(args: &[i64]) -> i64 {
    let mut path = match path_of(args[0]) {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(raised) => return raised,
    };
    for argument in &args[1..] {
        let Some(part) = value::as_path_like(*argument) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "joinpath() argument must be str or os.PathLike, not '{}'",
                    value::type_name(*argument)
                ),
            );
        };
        path = value::join_paths(&path, part);
    }
    traversable(PathBuf::from(path))
}

/// Accepts the UTF-8 spellings Hydro's decoder understands; anything else is a
/// genuine `LookupError`, not a silently wrong decode.
fn check_encoding(encoding: Option<i64>) -> Result<bool, i64> {
    let Some(encoding) = encoding else {
        return Ok(false);
    };
    if encoding == NONE {
        return Ok(false);
    }
    let Some(text) = value::as_str(encoding) else {
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "read_text() argument 'encoding' must be str, not '{}'",
                value::type_name(encoding)
            ),
        ));
    };
    match text.to_ascii_lowercase().replace('_', "-").as_str() {
        "" | "utf-8" | "utf8" => Ok(false),
        "ascii" | "us-ascii" => Ok(true),
        other => Err(exc::raise_with_message(
            LOOKUP_ERROR,
            &format!("unknown encoding: {other}"),
        )),
    }
}

/// `read_text(encoding=None, errors=None)`.
fn read_text(args: &[i64]) -> i64 {
    let path = match path_of(args[0]) {
        Ok(path) => path,
        Err(raised) => return raised,
    };
    let ascii = match check_encoding(args.get(1).copied()) {
        Ok(ascii) => ascii,
        Err(raised) => return raised,
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => return read_error(&path, &error),
    };
    if ascii && bytes.iter().any(|byte| *byte >= 0x80) {
        return exc::raise_with_message(
            UNICODE_DECODE_ERROR,
            &format!(
                "'ascii' codec can't decode byte in '{}'",
                path.to_string_lossy()
            ),
        );
    }
    match String::from_utf8(bytes) {
        Ok(text) => value::str_alloc(text),
        Err(error) => exc::raise_with_message(
            UNICODE_DECODE_ERROR,
            &format!(
                "'utf-8' codec can't decode byte 0x{:02x} in '{}'",
                error.as_bytes().first().copied().unwrap_or(0),
                path.to_string_lossy()
            ),
        ),
    }
}

fn read_error(path: &Path, error: &std::io::Error) -> i64 {
    let tag = match error.kind() {
        std::io::ErrorKind::NotFound => exc::FILE_NOT_FOUND_ERROR,
        std::io::ErrorKind::PermissionDenied => exc::PERMISSION_ERROR,
        std::io::ErrorKind::IsADirectory => exc::IS_A_DIRECTORY_ERROR,
        _ => OS_ERROR,
    };
    exc::raise_with_message(tag, &format!("{}: '{}'", error, path.to_string_lossy()))
}

fn read_bytes(args: &[i64]) -> i64 {
    let path = match path_of(args[0]) {
        Ok(path) => path,
        Err(raised) => return raised,
    };
    match std::fs::read(&path) {
        Ok(bytes) => value::bytes_new(bytes),
        Err(error) => read_error(&path, &error),
    }
}

/// `iterdir()` yields a list of `Traversable` values. A list is iterable in
/// Hydro, so `for child in package.iterdir()` works; CPython returns a lazy
/// iterator, which has no separate representation here.
fn iterdir(args: &[i64]) -> i64 {
    let path = match path_of(args[0]) {
        Ok(path) => path,
        Err(raised) => return raised,
    };
    let entries = match std::fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(error) => return read_error(&path, &error),
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    children.sort();
    value::list_new(children.into_iter().map(traversable).collect())
}

fn is_file(args: &[i64]) -> i64 {
    match path_of(args[0]) {
        Ok(path) => value::bool_value(path.is_file()),
        Err(raised) => raised,
    }
}

fn is_dir(args: &[i64]) -> i64 {
    match path_of(args[0]) {
        Ok(path) => value::bool_value(path.is_dir()),
        Err(raised) => raised,
    }
}

fn name(args: &[i64]) -> i64 {
    match path_of(args[0]) {
        Ok(path) => value::str_alloc(
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
        Err(raised) => raised,
    }
}

fn fspath(args: &[i64]) -> i64 {
    match path_of(args[0]) {
        Ok(path) => value::str_alloc(path.to_string_lossy().into_owned()),
        Err(raised) => raised,
    }
}

/// `open(mode='r', encoding=None, errors=None, newline=None)`.
///
/// Keyword arguments are bound by the positional slots the frontend fills;
/// the receiver is implicit. The native file object comes from the same
/// `io.open` implementation as `open()`.
fn open(args: &[i64]) -> i64 {
    let path = match path_of(args[0]) {
        Ok(path) => path,
        Err(raised) => return raised,
    };
    let mut io_args = Vec::with_capacity(args.len());
    io_args.push(value::str_alloc(path.to_string_lossy().into_owned()));
    io_args.extend_from_slice(&args[1..]);
    super::io_open(&io_args)
}
