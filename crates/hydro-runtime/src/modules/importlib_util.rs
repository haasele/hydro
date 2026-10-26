//! `importlib.util`: spec discovery, file specs, module creation and
//! `LazyLoader`.
//!
//! Discovery is filesystem based, layered over Hydro's native module table:
//! a name in `hydro_ir::stdlib` is a built-in module, an installed package is
//! found exactly where CPython would find it on `sys.path` (`HYDRO_SITE_PACKAGES`,
//! the `VIRTUAL_ENV`/`CONDA_PREFIX` site directories, `PYTHONPATH`,
//! `HYDRO_STDLIB`), and a module compiled into the program resolves through
//! the runtime registry.
//!
//! Hydro compiles Python ahead of time, so `module_from_spec` returns the
//! already-initialized module when one exists and otherwise creates a real
//! module-like namespace carrying the documented metadata attributes. It never
//! pretends the source executed: `loader.exec_module` on such a module raises
//! a precise `ImportError` instead.

use std::path::{Path, PathBuf};

use hydro_ir::MethodKind;

use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, IMPORT_ERROR, SYNTAX_ERROR, TYPE_ERROR, UNICODE_DECODE_ERROR,
    VALUE_ERROR,
};
use crate::value::{self, NONE};

use super::importlib_machinery as machinery;

const MODULE: &str = "importlib.util";

const LAZY_INIT: &str = "_lazy_init";
const LAZY_CREATE_MODULE: &str = "_lazy_create_module";
const LAZY_EXEC_MODULE: &str = "_lazy_exec_module";

/// Dispatches a native `importlib.util` function. Arity is enforced by the
/// central spec before this point.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "find_spec" => find_spec(args),
        "spec_from_file_location" => spec_from_file_location(args),
        "module_from_spec" => module_from_spec(args),
        "resolve_name" => resolve_name(args),
        "decode_source" => decode_source(args),
        LAZY_INIT => lazy_init(args),
        LAZY_CREATE_MODULE => lazy_create_module(args),
        LAZY_EXEC_MODULE => lazy_exec_module(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '{MODULE}' has no attribute '{function}'"),
        ),
    }
}

/// Materialises a `importlib.util` module constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "LazyLoader" => lazy_loader_class(),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '{MODULE}' has no attribute '{name}'"),
        ),
    }
}

// -------------------------------------------------------------------------
// Discovery
// -------------------------------------------------------------------------

/// The loader class a discovered module uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoaderKind {
    /// Native (built-in) module: `BuiltinImporter`.
    Builtin,
    /// Python source file: `SourceFileLoader`.
    Source,
    /// Compiled extension: `ExtensionFileLoader`.
    Extension,
    /// No loader (namespace packages, or a module only known to the registry).
    None,
}

/// A discovered module: everything a `ModuleSpec` needs.
#[derive(Clone, Debug)]
pub(crate) struct FoundSpec {
    pub name: String,
    pub origin: Option<String>,
    pub search_locations: Option<Vec<String>>,
    pub loader: LoaderKind,
}

/// Builds the `ModuleSpec` instance for a discovered module.
pub(crate) fn spec_instance(found: &FoundSpec) -> i64 {
    let (loader, has_location) = match found.loader {
        LoaderKind::Builtin => (machinery::builtin_importer_class(), false),
        LoaderKind::Source => match found.origin.as_deref() {
            Some(path) => (machinery::source_loader(&found.name, path), true),
            None => (NONE, false),
        },
        LoaderKind::Extension => match found.origin.as_deref() {
            Some(path) => (machinery::extension_loader(&found.name, path), true),
            None => (NONE, false),
        },
        LoaderKind::None => (NONE, false),
    };
    machinery::new_spec(
        &found.name,
        loader,
        found.origin.clone(),
        found.search_locations.clone(),
        has_location,
    )
}

/// Spec of a native module (any entry in the central module table).
pub(crate) fn native_spec(name: &str) -> i64 {
    found_spec(&FoundSpec {
        name: name.to_string(),
        origin: None,
        search_locations: None,
        loader: LoaderKind::Builtin,
    })
}

fn found_spec(found: &FoundSpec) -> i64 {
    spec_instance(found)
}

/// Resolves `name` to a module spec without consulting the caller's package
/// context. `None` means no module exists on the search path.
pub(crate) fn resolve_module(name: &str) -> Option<FoundSpec> {
    if hydro_ir::stdlib::module(name).is_some() {
        return Some(FoundSpec {
            name: name.to_string(),
            origin: None,
            search_locations: None,
            loader: LoaderKind::Builtin,
        });
    }
    if let Some(found) = filesystem_module(name) {
        return Some(found);
    }
    // A module compiled into the program but not installed as a file (for
    // example a local module outside `sys.path`). Its path is taken from the
    // `__file__` global when the module bound one.
    if super::registry_lookup(name).is_some() {
        let origin = globals::lookup(name, "__file__")
            .and_then(value::as_str)
            .map(machinery::absolute);
        let package = origin
            .as_deref()
            .is_some_and(|path| is_package_init(Path::new(path)));
        let search_locations = if package {
            origin
                .as_deref()
                .and_then(|path| Path::new(path).parent())
                .map(|dir| vec![dir.to_string_lossy().into_owned()])
        } else {
            None
        };
        let loader = if origin.is_some() {
            LoaderKind::Source
        } else {
            LoaderKind::None
        };
        return Some(FoundSpec {
            name: name.to_string(),
            origin,
            search_locations,
            loader,
        });
    }
    None
}

/// The directory (or module file) of a package, used by
/// `importlib.resources.files`.
pub(crate) fn package_location(name: &str) -> Option<PathBuf> {
    let found = resolve_module(name)?;
    if let Some(dirs) = found.search_locations {
        return dirs.into_iter().next().map(PathBuf::from);
    }
    found.origin.map(PathBuf::from)
}

/// Candidate file suffixes for compiled extension modules.
fn is_extension_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.ends_with(".so") || name.ends_with(".pyd")
}

/// Whether a source path is a package initializer.
fn is_package_init(path: &Path) -> bool {
    path.file_name() == Some(std::ffi::OsStr::new("__init__.py"))
}

/// Looks for `name` in the documented `sys.path` order.
fn filesystem_module(name: &str) -> Option<FoundSpec> {
    if name.is_empty() || name.split('.').any(str::is_empty) {
        return None;
    }
    let parts: Vec<&str> = name.split('.').collect();
    let mut namespaces: Vec<String> = Vec::new();
    for dir in super::search_path() {
        let mut base = PathBuf::from(&dir);
        for part in &parts {
            base.push(part);
        }
        // A regular package: `<base>/__init__.py`.
        let init = base.join("__init__.py");
        if init.is_file() {
            let origin = machinery::absolute(&init.to_string_lossy());
            let package_dir = base.to_string_lossy().into_owned();
            return Some(FoundSpec {
                name: name.to_string(),
                origin: Some(origin),
                search_locations: Some(vec![package_dir]),
                loader: LoaderKind::Source,
            });
        }
        // A plain module: `<base>.py`.
        let source = base.with_extension("py");
        if source.is_file() {
            return Some(FoundSpec {
                name: name.to_string(),
                origin: Some(machinery::absolute(&source.to_string_lossy())),
                search_locations: None,
                loader: LoaderKind::Source,
            });
        }
        // A compiled extension: `<base>.so` or `<leaf>.<abi>.so`.
        if let Some(path) = extension_file(&base) {
            return Some(FoundSpec {
                name: name.to_string(),
                origin: Some(machinery::absolute(&path.to_string_lossy())),
                search_locations: None,
                loader: LoaderKind::Extension,
            });
        }
        // A namespace package candidate keeps looking for a regular module in
        // later search-path entries, exactly like CPython.
        if base.is_dir() {
            namespaces.push(base.to_string_lossy().into_owned());
        }
    }
    if namespaces.is_empty() {
        return None;
    }
    Some(FoundSpec {
        name: name.to_string(),
        origin: None,
        search_locations: Some(namespaces),
        loader: LoaderKind::None,
    })
}

/// Finds a compiled extension file for the module base path `base`.
fn extension_file(base: &Path) -> Option<PathBuf> {
    let parent = base.parent()?;
    let stem = base.file_name()?.to_str()?;
    let entries = std::fs::read_dir(parent).ok()?;
    let mut matches: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_extension_path(path))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name == format!("{stem}.so")
                        || name == format!("{stem}.pyd")
                        || name.starts_with(&format!("{stem}."))
                })
        })
        .collect();
    matches.sort();
    matches.into_iter().next()
}

// -------------------------------------------------------------------------
// Functions
// -------------------------------------------------------------------------

fn str_arg(args: &[i64], index: usize, function: &str, name: &str) -> Result<String, i64> {
    match args.get(index).copied() {
        Some(value) if value != NONE => match value::as_str(value) {
            Some(text) => Ok(text.to_string()),
            None => Err(exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "{function}() argument '{name}' must be str, not '{}'",
                    value::type_name(value)
                ),
            )),
        },
        _ => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!("{function}() missing required argument: '{name}'"),
        )),
    }
}

/// `find_spec(name, package=None)`.
fn find_spec(args: &[i64]) -> i64 {
    let name = match str_arg(args, 0, "find_spec", "name") {
        Ok(name) => name,
        Err(raised) => return raised,
    };
    let name = if name.starts_with('.') {
        match resolve_relative(&name, args.get(1).copied().unwrap_or(NONE)) {
            Ok(name) => name,
            Err(raised) => return raised,
        }
    } else {
        name
    };
    if name.is_empty() || name.split('.').any(str::is_empty) {
        return NONE;
    }
    // `sys.modules` wins, exactly like CPython: a loaded module's `__spec__`
    // is authoritative.
    if let Some(module) = super::registry_lookup(&name) {
        if let Some(pairs) = value::as_namespace(module) {
            if let Some((_, spec)) = pairs.iter().find(|(key, _)| key.as_str() == "__spec__") {
                if *spec == NONE {
                    return exc::raise_with_message(
                        VALUE_ERROR,
                        &format!("{name}.__spec__ is set to None"),
                    );
                }
                return *spec;
            }
        }
        if hydro_ir::stdlib::module(&name).is_some() {
            return native_spec(&name);
        }
    }
    match resolve_module(&name) {
        Some(found) => found_spec(&found),
        None => NONE,
    }
}

/// `spec_from_file_location(name, location=None, loader=None,
/// submodule_search_locations=...)`.
///
/// The frontend fills omitted slots with `None`, so `None` for
/// `submodule_search_locations` means "populate automatically" here; pass an
/// explicit empty list to request `submodule_search_locations == []`.
fn spec_from_file_location(args: &[i64]) -> i64 {
    let name = match str_arg(args, 0, "spec_from_file_location", "name") {
        Ok(name) => name,
        Err(raised) => return raised,
    };
    let location_arg = args.get(1).copied().unwrap_or(NONE);
    let loader_arg = args.get(2).copied().unwrap_or(NONE);
    let search_arg = args.get(3).copied().unwrap_or(NONE);

    let location = if location_arg == NONE {
        "<unknown>".to_string()
    } else {
        match value::as_path_like(location_arg) {
            Some(text) => machinery::absolute(text),
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "spec_from_file_location() argument 'location' must be str or \
                         os.PathLike, not '{}'",
                        value::type_name(location_arg)
                    ),
                )
            }
        }
    };

    let spec = machinery::new_spec(&name, NONE, Some(location.clone()), None, true);

    let loader = if loader_arg != NONE {
        loader_arg
    } else if location.ends_with(".py") {
        machinery::source_loader(&name, &location)
    } else if is_extension_path(Path::new(&location)) {
        machinery::extension_loader(&name, &location)
    } else {
        // CPython declines locations whose suffix no loader recognizes.
        return NONE;
    };
    machinery::set_spec_loader(spec, loader);

    let mut locations: Option<Vec<String>> = None;
    if search_arg != NONE {
        if let Some(items) = value::as_list(search_arg) {
            locations = Some(
                items
                    .iter()
                    .filter_map(|item| value::as_str(*item).map(str::to_string))
                    .collect(),
            );
        } else if let Some(text) = value::as_str(search_arg) {
            locations = Some(vec![text.to_string()]);
        } else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "spec_from_file_location() argument 'submodule_search_locations' \
                     must be a list or None, not '{}'",
                    value::type_name(search_arg)
                ),
            );
        }
    } else if is_package_init(Path::new(&location)) {
        let dir = Path::new(&location)
            .parent()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default();
        locations = Some(if dir.is_empty() {
            Vec::new()
        } else {
            vec![dir]
        });
    }
    if let Some(mut dirs) = locations {
        if dirs.is_empty() && location != "<unknown>" {
            if let Some(dir) = Path::new(&location).parent() {
                dirs.push(dir.to_string_lossy().into_owned());
            }
        }
        machinery::set_package_locations(spec, dirs);
    }
    spec
}

/// `module_from_spec(spec)`.
fn module_from_spec(args: &[i64]) -> i64 {
    let spec = args[0];
    if !machinery::is_spec(spec) {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "module_from_spec() argument must be a ModuleSpec instance, not '{}'",
                value::type_name(spec)
            ),
        );
    }
    let Some(name) = machinery::spec_name(spec) else {
        return exc::raise_with_message(
            VALUE_ERROR,
            "module_from_spec() requires a spec with a str name",
        );
    };
    // A compiled module already has a real module value; the loader's
    // `create_module()` would return exactly this object.
    if let Some(module) = super::registry_lookup(&name) {
        return module;
    }
    let origin = machinery::spec_origin(spec);
    let has_location = machinery::spec_has_location(spec);
    let locations = machinery::spec_search_locations(spec);
    let parent = value::instance_attr(spec, "parent")
        .and_then(value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    let loader = value::instance_attr(spec, "loader").unwrap_or(NONE);

    let mut pairs: Vec<(String, i64)> = Vec::with_capacity(8);
    pairs.push(("__name__".to_string(), value::str_alloc(name.clone())));
    pairs.push(("__loader__".to_string(), loader));
    pairs.push(("__package__".to_string(), value::str_alloc(parent)));
    pairs.push(("__spec__".to_string(), spec));
    pairs.push(("__doc__".to_string(), NONE));
    if has_location {
        if let Some(origin) = origin.as_deref() {
            pairs.push(("__file__".to_string(), value::str_alloc(origin.to_string())));
        }
        if let Some(cached) = value::instance_attr(spec, "cached") {
            pairs.push(("__cached__".to_string(), cached));
        }
    }
    if let Some(locations) = locations {
        let items = locations.into_iter().map(value::str_alloc).collect();
        pairs.push(("__path__".to_string(), value::list_new(items)));
    }
    value::namespace_new(pairs)
}

/// `resolve_name(name, package)`.
fn resolve_name(args: &[i64]) -> i64 {
    let name = match str_arg(args, 0, "resolve_name", "name") {
        Ok(name) => name,
        Err(raised) => return raised,
    };
    match resolve_relative(&name, args.get(1).copied().unwrap_or(NONE)) {
        Ok(resolved) => value::str_alloc(resolved),
        Err(raised) => raised,
    }
}

/// Resolves a relative name for `resolve_name`/`find_spec`, which report a
/// missing package as `ImportError` (unlike `import_module`'s `TypeError`).
fn resolve_relative(name: &str, package_arg: i64) -> Result<String, i64> {
    if !name.starts_with('.') {
        return Ok(name.to_string());
    }
    let package = value::as_str(package_arg).filter(|package| !package.is_empty());
    let Some(package) = package else {
        return Err(exc::raise_with_message(
            IMPORT_ERROR,
            &format!("no package specified for {name:?} (required for relative module names)"),
        ));
    };
    let dots = name.bytes().take_while(|byte| *byte == b'.').count();
    let rest = &name[dots..];
    let mut parts: Vec<&str> = package.split('.').filter(|part| !part.is_empty()).collect();
    let up = dots - 1;
    if up > parts.len() {
        return Err(exc::raise_with_message(
            IMPORT_ERROR,
            "attempted relative import beyond top-level package",
        ));
    }
    parts.truncate(parts.len() - up);
    if !rest.is_empty() {
        parts.push(rest);
    }
    Ok(parts.join("."))
}

// -------------------------------------------------------------------------
// Source decoding
// -------------------------------------------------------------------------

/// `importlib.util.decode_source(source_bytes)`: decode source code with
/// CPython's rules.
///
/// A UTF-8 BOM is stripped, the `coding[:=]` cookie on the first two lines
/// picks the codec (default UTF-8), universal newlines are translated and
/// unknown cookie codecs raise `SyntaxError` while invalid data raises
/// `UnicodeDecodeError`.
fn decode_source(args: &[i64]) -> i64 {
    let Some(source) = value::as_bytes(args[0]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "a bytes-like object is required, not '{}'",
                value::type_name(args[0])
            ),
        );
    };
    let (source, has_bom) = match source.strip_prefix(b"\xef\xbb\xbf") {
        Some(rest) => (rest, true),
        None => (source, false),
    };
    let encoding = match source_encoding(source) {
        Ok(encoding) => encoding,
        Err(raised) => return raised,
    };
    // `tokenize.detect_encoding` refuses a cookie that contradicts the BOM.
    if has_bom && encoding != "utf-8" {
        return exc::raise_with_message(SYNTAX_ERROR, "encoding problem: utf-8");
    }
    match decode_bytes(source, encoding) {
        Ok(text) => value::str_alloc(translate_newlines(text)),
        Err(raised) => raised,
    }
}

/// The codec selected by the source: the first two lines are scanned for a
/// coding cookie, everything else defaults to UTF-8.
fn source_encoding(source: &[u8]) -> Result<&'static str, i64> {
    let mut lines = source.split(|byte| *byte == b'\n');
    let first = lines.next().unwrap_or(b"");
    if !first.is_empty() {
        if let Some(encoding) = cookie_in_line(first)? {
            return Ok(encoding);
        }
        if !is_blank_line(first) {
            return Ok("utf-8");
        }
    }
    match lines.next() {
        Some(second) if !second.is_empty() => Ok(cookie_in_line(second)?.unwrap_or("utf-8")),
        _ => Ok("utf-8"),
    }
}

/// Whether `line` is blank or a comment, so the next line may carry the
/// cookie (CPython's `blank_re`).
fn is_blank_line(line: &[u8]) -> bool {
    let mut start = 0;
    while start < line.len() && matches!(line[start], b' ' | b'\t' | 0x0c) {
        start += 1;
    }
    let trimmed = &line[start..];
    matches!(
        trimmed.first(),
        None | Some(b'#') | Some(b'\r') | Some(b'\n')
    )
}

/// The codec named by a `coding[:=]` comment on `line`, if any.
///
/// Scanning is byte-oriented like CPython's `cookie_re`: the comment marker and
/// the codec name are ASCII, so a line carrying non-UTF-8 comment text can
/// still declare the file's encoding.
fn cookie_in_line(line: &[u8]) -> Result<Option<&'static str>, i64> {
    // `.` in the pattern does not cross a newline.
    let line = match line.iter().position(|byte| *byte == b'\n') {
        Some(end) => &line[..end],
        None => line,
    };
    let mut start = 0;
    while start < line.len() && matches!(line[start], b' ' | b'\t' | 0x0c) {
        start += 1;
    }
    let Some(comment) = line.get(start..).and_then(|rest| rest.strip_prefix(b"#")) else {
        return Ok(None);
    };
    let mut search = comment;
    while let Some(index) = find_coding(search) {
        let after = &search[index + "coding".len()..];
        if matches!(after.first(), Some(b':') | Some(b'=')) {
            let mut cursor = 1;
            while cursor < after.len() && matches!(after[cursor], b' ' | b'\t') {
                cursor += 1;
            }
            let name_start = cursor;
            while cursor < after.len()
                && (after[cursor].is_ascii_alphanumeric()
                    || matches!(after[cursor], b'-' | b'_' | b'.'))
            {
                cursor += 1;
            }
            if cursor > name_start {
                // Every accepted byte is ASCII, so the name always decodes.
                let name = std::str::from_utf8(&after[name_start..cursor]).unwrap_or("");
                let normalized = normalize_encoding(name);
                return match canonical_encoding(&normalized) {
                    Some(encoding) => Ok(Some(encoding)),
                    None => Err(exc::raise_with_message(
                        SYNTAX_ERROR,
                        &format!("unknown encoding: {normalized}"),
                    )),
                };
            }
        }
        search = after;
    }
    Ok(None)
}

/// Position of `coding` in `search`, the byte equivalent of the lazy `.*?`
/// search in CPython's cookie pattern.
fn find_coding(search: &[u8]) -> Option<usize> {
    search
        .windows("coding".len())
        .position(|window| window.starts_with(b"coding"))
}

/// CPython's `_get_normal_name`: the first twelve characters, lowercased with
/// underscores spelled as hyphens.
fn normalize_encoding(name: &str) -> String {
    name.chars()
        .take(12)
        .collect::<String>()
        .to_ascii_lowercase()
        .replace('_', "-")
}

/// Maps a normalized cookie name onto a codec this runtime decodes, following
/// CPython's UTF-8 and Latin-1 aliasing.
fn canonical_encoding(normalized: &str) -> Option<&'static str> {
    if normalized == "utf-8"
        || normalized == "utf8"
        || normalized == "u8"
        || normalized == "cp65001"
        || normalized.starts_with("utf-8-")
    {
        Some("utf-8")
    } else if normalized == "latin-1"
        || normalized == "latin1"
        || normalized.starts_with("latin-1-")
        || normalized == "iso-8859-1"
        || normalized == "iso8859-1"
        || normalized.starts_with("iso-8859-1-")
        || normalized == "iso-latin-1"
        || normalized.starts_with("iso-latin-1-")
        || normalized == "l1"
        || normalized == "cp819"
    {
        Some("iso-8859-1")
    } else if normalized == "ascii" || normalized == "us-ascii" {
        Some("ascii")
    } else {
        None
    }
}

/// Decodes `source` with a canonical codec name, raising `UnicodeDecodeError`
/// for data the codec rejects.
fn decode_bytes(source: &[u8], encoding: &str) -> Result<String, i64> {
    match encoding {
        "utf-8" => match std::str::from_utf8(source) {
            Ok(text) => Ok(text.to_string()),
            Err(error) => Err(utf8_decode_error(source, &error)),
        },
        // Both single-byte codecs map one byte to one code point.
        "iso-8859-1" => Ok(source.iter().map(|byte| char::from(*byte)).collect()),
        "ascii" => match source.iter().position(|byte| *byte >= 0x80) {
            None => Ok(source.iter().map(|byte| char::from(*byte)).collect()),
            Some(position) => Err(exc::raise_with_message(
                UNICODE_DECODE_ERROR,
                &format!(
                    "'ascii' codec can't decode byte 0x{:02x} in position {position}: ordinal not in range(128)",
                    source[position]
                ),
            )),
        },
        _ => Err(exc::raise_with_message(
            SYNTAX_ERROR,
            &format!("unknown encoding: {encoding}"),
        )),
    }
}

/// CPython-shaped `UnicodeDecodeError` for invalid UTF-8.
fn utf8_decode_error(source: &[u8], error: &std::str::Utf8Error) -> i64 {
    let position = error.valid_up_to();
    let byte = source.get(position).copied().unwrap_or(0);
    let reason = match error.error_len() {
        None => "unexpected end of data",
        Some(1) => "invalid start byte",
        Some(_) => "invalid continuation byte",
    };
    exc::raise_with_message(
        UNICODE_DECODE_ERROR,
        &format!("'utf-8' codec can't decode byte 0x{byte:02x} in position {position}: {reason}"),
    )
}

/// Universal newline translation, as `decode_source` performs through
/// `IncrementalNewlineDecoder(translate=True)`.
fn translate_newlines(text: String) -> String {
    if !text.contains('\r') {
        return text;
    }
    let mut translated = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            translated.push('\n');
        } else {
            translated.push(ch);
        }
    }
    translated
}

// -------------------------------------------------------------------------
// LazyLoader
// -------------------------------------------------------------------------

fn lazy_loader_class() -> i64 {
    const KEY: &str = "importlib.util.LazyLoader";
    if let Some(class) = globals::registry_get(KEY) {
        return class;
    }
    let methods = vec![
        (
            "__init__".to_string(),
            native(LAZY_INIT, 2),
            MethodKind::Normal,
        ),
        (
            "create_module".to_string(),
            native(LAZY_CREATE_MODULE, 2),
            MethodKind::Normal,
        ),
        (
            "exec_module".to_string(),
            native(LAZY_EXEC_MODULE, 2),
            MethodKind::Normal,
        ),
    ];
    let roots = crate::heap::root_values(
        &methods
            .iter()
            .map(|(_, value, _)| *value)
            .collect::<Vec<_>>(),
    );
    let class = value::class_new_full(
        "LazyLoader".to_string(),
        machinery::loader_class(),
        methods,
        Vec::new(),
    );
    let class_roots = crate::heap::root_values(&[class]);
    globals::registry_set(KEY, class);
    crate::heap::release_roots(class_roots);
    crate::heap::release_roots(roots);
    class
}

fn native(function: &'static str, arity: usize) -> i64 {
    value::native_function_new(MODULE, function, arity)
}

fn lazy_init(args: &[i64]) -> i64 {
    let receiver = args[0];
    let wrapped = args[1];
    if value::as_instance(wrapped).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "LazyLoader() argument must be a loader instance, not '{}'",
                value::type_name(wrapped)
            ),
        );
    }
    value::instance_setattr(receiver, "loader".to_string(), wrapped);
    NONE
}

fn lazy_create_module(args: &[i64]) -> i64 {
    let spec = args[1];
    machinery::spec_name(spec)
        .and_then(|name| super::registry_lookup(&name))
        .unwrap_or(NONE)
}

fn lazy_exec_module(args: &[i64]) -> i64 {
    machinery::exec_loaded_module(args[1])
}
