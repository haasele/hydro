//! `importlib.metadata`: installed-distribution metadata.
//!
//! Distributions are discovered from the `.dist-info` directories on
//! `sys.path` (`HYDRO_SITE_PACKAGES`, virtualenv/conda site-packages,
//! `PYTHONPATH`, `HYDRO_STDLIB`). Nothing is downloaded and no build backend
//! is consulted: `METADATA`, `entry_points.txt`, `RECORD` and
//! `direct_url.json` are read from disk.
//!
//! `Distribution` and `EntryPoint` are native class instances with real
//! values for `name`, `version`, `metadata`, `requires`, `entry_points`,
//! `files` and `origin`. A missing distribution raises the
//! `PackageNotFoundError` class this module exposes, so `except` clauses in
//! packages (numpy's and pydantic's probes) keep working.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

use hydro_ir::MethodKind;

use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, IMPORT_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value::{self, NONE};

const MODULE: &str = "importlib.metadata";

const DISTRIBUTION_READ_TEXT: &str = "_distribution_read_text";
const ENTRY_POINT_LOAD: &str = "_entry_point_load";
const PACKAGE_NOT_FOUND_STR: &str = "_package_not_found_str";

/// One parsed entry point.
#[derive(Clone, Debug)]
struct EntryPointData {
    group: String,
    name: String,
    value: String,
    module: String,
    attr: String,
    extras: Vec<String>,
}

/// One installed distribution.
#[derive(Clone, Debug)]
struct Dist {
    /// `METADATA`'s `Name` header (`pyproject.toml` normal form when absent,
    /// taken from the directory).
    name: String,
    /// `METADATA`'s `Version` header (or the directory suffix).
    version: String,
    /// Normalised name used for lookups (`-` separated, lower case).
    normalized: String,
    /// The `.dist-info` directory.
    path: PathBuf,
    /// Raw `METADATA` headers in file order.
    headers: Vec<(String, String)>,
    /// `Requires-Dist` values in file order.
    requires: Vec<String>,
    entry_points: Vec<EntryPointData>,
    /// `RECORD` paths (first CSV column), or `None` when the file is absent.
    files: Option<Vec<String>>,
}

type CacheEntry = (Vec<String>, Vec<Dist>);

static CACHE: LazyLock<Mutex<Option<CacheEntry>>> = LazyLock::new(|| Mutex::new(None));

fn cache() -> MutexGuard<'static, Option<CacheEntry>> {
    CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `importlib.invalidate_caches()` clears the discovered-distribution cache so
/// a package installed later in the process lifetime is observed.
pub(crate) fn invalidate_caches() {
    *cache() = None;
}

/// Discovered distributions for the current search path.
///
/// The cache records the paths it was built from: `sys.path` is environment
/// derived and tests (or a REPL session) may change it between calls, so an
/// entry from a different path list is ignored instead of returning stale
/// results.
fn distributions_cache() -> Vec<Dist> {
    let paths = super::search_path();
    if let Some((cached_paths, dists)) = cache().as_ref() {
        if *cached_paths == paths {
            return dists.clone();
        }
    }
    let found = collect_from_paths(&paths);
    *cache() = Some((paths, found.clone()));
    found
}

/// Dispatches a native `importlib.metadata` function. Arity is enforced by the
/// central spec before this point.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "version" => version(args),
        "metadata" => metadata(args),
        "requires" => requires(args),
        "distribution" => distribution(args),
        "distributions" => distributions_kw(&[]),
        "entry_points" => entry_points_kw(&[]),
        DISTRIBUTION_READ_TEXT => distribution_read_text(args),
        ENTRY_POINT_LOAD => entry_point_load(args),
        PACKAGE_NOT_FOUND_STR => package_not_found_str(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '{MODULE}' has no attribute '{function}'"),
        ),
    }
}

/// Keyword-aware dispatch for `distributions(**kwargs)` and
/// `entry_points(**kwargs)`.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let _ = args;
    match function {
        "distributions" => distributions_kw(keywords),
        "entry_points" => entry_points_kw(keywords),
        _ => exc::raise_with_message(
            TYPE_ERROR,
            &format!("{MODULE}.{function}() does not accept keyword arguments"),
        ),
    }
}

/// Materialises a `importlib.metadata` module constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "PackageNotFoundError" => package_not_found_class(),
        "Distribution" => distribution_class(),
        "EntryPoint" => entry_point_class(),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '{MODULE}' has no attribute '{name}'"),
        ),
    }
}

// -------------------------------------------------------------------------
// Discovery / parsing
// -------------------------------------------------------------------------

/// Normalised project name: lower case, runs of `-`, `_` and `.` collapse to
/// a single `-` (PEP 503).
fn normalize(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut separator = false;
    for character in name.chars() {
        if matches!(character, '-' | '_' | '.') {
            separator = true;
        } else {
            if separator && !normalized.is_empty() {
                normalized.push('-');
            }
            separator = false;
            normalized.extend(character.to_lowercase());
        }
    }
    normalized
}

/// Collects distributions from the given directories.
fn collect_from_paths(paths: &[String]) -> Vec<Dist> {
    let mut found = Vec::new();
    for dir in paths {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.ends_with(".dist-info") {
                continue;
            }
            if let Some(dist) = parse_dist_info(&path, name) {
                found.push(dist);
            }
        }
    }
    found.sort_by(|left, right| {
        left.normalized
            .cmp(&right.normalized)
            .then_with(|| left.version.cmp(&right.version))
    });
    found
}

/// Parses `METADATA`, `entry_points.txt`, `RECORD` and the directory name.
fn parse_dist_info(path: &Path, dir_name: &str) -> Option<Dist> {
    let stem = dir_name.strip_suffix(".dist-info").unwrap_or(dir_name);
    let (dir_name, dir_version) = match stem.rsplit_once('-') {
        Some((name, version)) => (name, version),
        None => (stem, ""),
    };
    let metadata_text = match std::fs::read_to_string(path.join("METADATA")) {
        Ok(text) => text,
        Err(_) => match std::fs::read_to_string(path.join("PKG-INFO")) {
            Ok(text) => text,
            // A directory without metadata is not a usable distribution; do
            // not invent a version for it.
            Err(_) => return None,
        },
    };
    let headers = parse_headers(&metadata_text);
    let header = |wanted: &str| {
        headers
            .iter()
            .find_map(|(name, value)| name.eq_ignore_ascii_case(wanted).then(|| value.clone()))
    };
    let name = header("Name").unwrap_or_else(|| dir_name.to_string());
    let version = header("Version").unwrap_or_else(|| dir_version.to_string());
    let requires: Vec<String> = headers
        .iter()
        .filter(|(header, _)| header.eq_ignore_ascii_case("Requires-Dist"))
        .map(|(_, value)| value.clone())
        .collect();
    let entry_points = std::fs::read_to_string(path.join("entry_points.txt"))
        .map(|text| parse_entry_points(&text))
        .unwrap_or_default();
    let files = std::fs::read_to_string(path.join("RECORD"))
        .ok()
        .map(|text| parse_record(&text));
    Some(Dist {
        normalized: normalize(&name),
        name,
        version,
        path: path.to_path_buf(),
        headers,
        requires,
        entry_points,
        files,
    })
}

/// Parses RFC 822-style headers up to the first blank line.
fn parse_headers(text: &str) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = headers.last_mut() {
                value.push('\n');
                value.push_str(line.trim_start());
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    headers
}

fn parse_record(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let path = line.split(',').next().unwrap_or("");
            (!path.is_empty()).then(|| path.to_string())
        })
        .collect()
}

/// Parses an `entry_points.txt` INI file.
fn parse_entry_points(text: &str) -> Vec<EntryPointData> {
    let mut entries = Vec::new();
    let mut group = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            group = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim().to_string();
        let value = value.trim().to_string();
        let (module, attr, extras) = parse_entry_point_value(&value);
        entries.push(EntryPointData {
            group: group.clone(),
            name,
            value,
            module,
            attr,
            extras,
        });
    }
    entries
}

/// Splits `pkg.mod:attr [extra1,extra2]` into its parts.
fn parse_entry_point_value(value: &str) -> (String, String, Vec<String>) {
    let (object, extras) = match value.split_once('[') {
        Some((object, rest)) => {
            let extras = rest
                .trim_end()
                .trim_end_matches(']')
                .split(',')
                .map(str::trim)
                .filter(|extra| !extra.is_empty())
                .map(str::to_string)
                .collect();
            (object.trim(), extras)
        }
        None => (value.trim(), Vec::new()),
    };
    let (module, attr) = match object.split_once(':') {
        Some((module, attr)) => (module.trim().to_string(), attr.trim().to_string()),
        None => (object.trim().to_string(), String::new()),
    };
    (module, attr, extras)
}

fn find_dist(name: &str) -> Option<Dist> {
    let wanted = normalize(name);
    distributions_cache()
        .into_iter()
        .find(|dist| dist.normalized == wanted)
}

fn not_found(name: &str) -> i64 {
    let class = package_not_found_class();
    let args = [value::str_alloc(name.to_string())];
    let instance = crate::builtins::call(class, args.len(), args.as_ptr());
    if exc::pending() {
        return instance;
    }
    // `raise PackageNotFoundError(name)` keeps the class identity; CPython's
    // custom `__str__` renders "No package metadata was found for <name>",
    // which the exception payload must carry for the traceback to be useful.
    let Some(data) = value::exception_data(instance) else {
        return exc::raise_value(instance);
    };
    let mut data = data.clone();
    data.name = "PackageNotFoundError";
    data.msg = format!("No package metadata was found for {name}");
    // Hydro's frontend approximates every non-builtin exception class in an
    // `except` clause with the `RuntimeError` tag (including the dotted
    // `except importlib.metadata.PackageNotFoundError` used by numpy). The
    // class itself still derives from `ModuleNotFoundError`, so `isinstance`
    // and `type(e)` are exact; only tag-based handler matching needs the
    // approximation.
    data.tag = exc::RUNTIME_ERROR;
    exc::raise_value(value::exception_with_data(instance, data))
}

// -------------------------------------------------------------------------
// Classes
// -------------------------------------------------------------------------

fn cached_class(key: &str, name: &str, base: i64, methods: Vec<(String, i64)>) -> i64 {
    if let Some(class) = globals::registry_get(key) {
        return class;
    }
    let methods: Vec<(String, i64, MethodKind)> = methods
        .into_iter()
        .map(|(name, function)| (name, function, MethodKind::Normal))
        .collect();
    let method_values: Vec<i64> = methods.iter().map(|(_, value, _)| *value).collect();
    let method_roots = crate::heap::root_values(&method_values);
    let class = value::class_new_full(name.to_string(), base, methods, Vec::new());
    let class_roots = crate::heap::root_values(&[class]);
    globals::registry_set(key, class);
    crate::heap::release_roots(class_roots);
    crate::heap::release_roots(method_roots);
    class
}

fn native(function: &'static str, arity: usize) -> i64 {
    value::native_function_new(MODULE, function, arity)
}

fn distribution_class() -> i64 {
    cached_class(
        "importlib.metadata.Distribution",
        "Distribution",
        NONE,
        vec![("read_text".to_string(), native(DISTRIBUTION_READ_TEXT, 2))],
    )
}

fn entry_point_class() -> i64 {
    cached_class(
        "importlib.metadata.EntryPoint",
        "EntryPoint",
        NONE,
        vec![("load".to_string(), native(ENTRY_POINT_LOAD, 1))],
    )
}

/// `importlib.metadata.PackageNotFoundError`, a `ModuleNotFoundError`
/// subclass, like CPython.
fn package_not_found_class() -> i64 {
    const KEY: &str = "importlib.metadata.PackageNotFoundError";
    if let Some(class) = globals::registry_get(KEY) {
        return class;
    }
    let base = exc::exception_class(exc::MODULE_NOT_FOUND_ERROR);
    let methods = vec![(
        "__str__".to_string(),
        native(PACKAGE_NOT_FOUND_STR, 1),
        MethodKind::Normal,
    )];
    let method_values: Vec<i64> = methods.iter().map(|(_, value, _)| *value).collect();
    let method_roots = crate::heap::root_values(&method_values);
    let class = value::class_new_full(
        "PackageNotFoundError".to_string(),
        base,
        methods,
        Vec::new(),
    );
    let class_roots = crate::heap::root_values(&[class]);
    globals::registry_set(KEY, class);
    crate::heap::release_roots(class_roots);
    crate::heap::release_roots(method_roots);
    class
}

// -------------------------------------------------------------------------
// Distribution objects
// -------------------------------------------------------------------------

fn strings(values: impl IntoIterator<Item = String>) -> i64 {
    value::list_new(values.into_iter().map(value::str_alloc).collect())
}

fn metadata_dict(dist: &Dist) -> i64 {
    // `email.message.Message` keeps repeated headers; a dict has one slot per
    // name, so a repeated header keeps its last occurrence.
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (name, value) in &dist.headers {
        match pairs
            .iter_mut()
            .find(|(key, _)| key.as_str() == name.as_str())
        {
            Some((_, slot)) => *slot = value.clone(),
            None => pairs.push((name.clone(), value.clone())),
        }
    }
    let pairs: Vec<(i64, i64)> = pairs
        .into_iter()
        .map(|(name, value)| (value::str_alloc(name), value::str_alloc(value)))
        .collect();
    value::dict_new(pairs)
}

/// `dist-info/direct_url.json` as nested namespaces, or `None`.
fn origin_value(dist: &Dist) -> i64 {
    let Ok(text) = std::fs::read_to_string(dist.path.join("direct_url.json")) else {
        return NONE;
    };
    let parsed = super::json_call("loads", &[value::str_alloc(text)]);
    if exc::pending() {
        // A malformed file is a real value in CPython's JSON reader only when
        // it parses; report the parse failure rather than pretending absence.
        return NONE;
    }
    json_to_namespace(parsed)
}

/// Converts a JSON object value into a `SimpleNamespace`-like namespace so
/// `origin.dir_info.editable` works; arrays and scalars keep their JSON form.
fn json_to_namespace(value: i64) -> i64 {
    if let Some(pairs) = value::as_dict(value) {
        let converted: Vec<(String, i64)> = pairs
            .iter()
            .filter_map(|(key, item)| {
                value::as_str(*key).map(|key| (key.to_string(), json_to_namespace(*item)))
            })
            .collect();
        return value::namespace_new(converted);
    }
    if let Some(items) = value::as_list(value) {
        let converted = items.iter().map(|item| json_to_namespace(*item)).collect();
        return value::list_new(converted);
    }
    value
}

fn distribution_value(dist: &Dist) -> i64 {
    let instance = value::instance_new(distribution_class());
    let roots = crate::heap::root_values(&[instance]);
    value::instance_setattr(
        instance,
        "_path".to_string(),
        value::str_alloc(dist.path.to_string_lossy().into_owned()),
    );
    value::instance_setattr(
        instance,
        "name".to_string(),
        value::str_alloc(dist.name.clone()),
    );
    value::instance_setattr(
        instance,
        "version".to_string(),
        value::str_alloc(dist.version.clone()),
    );
    value::instance_setattr(instance, "metadata".to_string(), metadata_dict(dist));
    value::instance_setattr(
        instance,
        "requires".to_string(),
        if dist.requires.is_empty() {
            NONE
        } else {
            strings(dist.requires.clone())
        },
    );
    let entries: Vec<i64> = dist.entry_points.iter().map(entry_point_value).collect();
    value::instance_setattr(
        instance,
        "entry_points".to_string(),
        value::list_new(entries),
    );
    value::instance_setattr(
        instance,
        "files".to_string(),
        dist.files
            .as_ref()
            .map(|files| strings(files.clone()))
            .unwrap_or(NONE),
    );
    value::instance_setattr(instance, "origin".to_string(), origin_value(dist));
    crate::heap::release_roots(roots);
    instance
}

fn entry_point_value(data: &EntryPointData) -> i64 {
    let instance = value::instance_new(entry_point_class());
    let roots = crate::heap::root_values(&[instance]);
    for (field, text) in [
        ("name", data.name.as_str()),
        ("value", data.value.as_str()),
        ("group", data.group.as_str()),
        ("module", data.module.as_str()),
        ("attr", data.attr.as_str()),
    ] {
        value::instance_setattr(
            instance,
            field.to_string(),
            value::str_alloc(text.to_string()),
        );
    }
    value::instance_setattr(instance, "extras".to_string(), strings(data.extras.clone()));
    crate::heap::release_roots(roots);
    instance
}

fn distribution_read_text(args: &[i64]) -> i64 {
    let receiver = args[0];
    let Some(filename) = value::as_str(args[1]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "Distribution.read_text() argument must be str",
        );
    };
    let Some(path) = value::instance_attr(receiver, "_path").and_then(value::as_str) else {
        return exc::raise_with_message(
            VALUE_ERROR,
            "Distribution instance is not bound to a .dist-info directory",
        );
    };
    if Path::new(filename).is_absolute() || filename.split(['/', '\\']).any(|part| part == "..") {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!("Distribution.read_text() path escapes the distribution: {filename}"),
        );
    }
    match std::fs::read_to_string(Path::new(path).join(filename)) {
        Ok(text) => value::str_alloc(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => NONE,
        Err(error) => exc::raise_with_message(
            crate::exc::OS_ERROR,
            &format!("cannot read '{filename}' from '{path}': {error}"),
        ),
    }
}

fn entry_point_load(args: &[i64]) -> i64 {
    let receiver = args[0];
    let attr = |field: &str| {
        value::instance_attr(receiver, field)
            .and_then(value::as_str)
            .map(str::to_string)
    };
    let Some(module_name) = attr("module") else {
        return exc::raise_with_message(
            VALUE_ERROR,
            "EntryPoint has no 'module'; the value is malformed",
        );
    };
    if module_name.is_empty() {
        return exc::raise_with_message(IMPORT_ERROR, "EntryPoint value names no module");
    }
    let mut object =
        super::importlib::call("import_module", &[value::str_alloc(module_name.clone())]);
    if exc::pending() {
        return NONE;
    }
    let Some(attr_path) = attr("attr").filter(|path| !path.is_empty()) else {
        return object;
    };
    for part in attr_path.split('.') {
        object = crate::core::methods::getattr(object, part.as_ptr(), part.len());
        if exc::pending() {
            return NONE;
        }
    }
    object
}

fn package_not_found_str(args: &[i64]) -> i64 {
    // `__str__` receives the exception instance; its `args` tuple holds the
    // distribution name, matching CPython's implementation.
    let instance = args[0];
    let name = exc::exception_attr(instance, "args")
        .and_then(value::as_tuple)
        .and_then(|items| items.first().copied())
        .and_then(value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| value::to_string(instance));
    value::str_alloc(format!("No package metadata was found for {name}"))
}

// -------------------------------------------------------------------------
// Module-level functions
// -------------------------------------------------------------------------

fn name_argument(args: &[i64], function: &str) -> Result<String, i64> {
    match args.first().copied() {
        Some(value) if value != NONE => match value::as_str(value) {
            Some(name) if !name.is_empty() => Ok(name.to_string()),
            Some(_) => Err(exc::raise_with_message(
                VALUE_ERROR,
                "A distribution name is required.",
            )),
            None => Err(exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "{function}() argument 'distribution_name' must be str, not '{}'",
                    value::type_name(value)
                ),
            )),
        },
        _ => Err(exc::raise_with_message(
            VALUE_ERROR,
            "A distribution name is required.",
        )),
    }
}

fn version(args: &[i64]) -> i64 {
    let name = match name_argument(args, "version") {
        Ok(name) => name,
        Err(raised) => return raised,
    };
    match find_dist(&name) {
        Some(dist) => value::str_alloc(dist.version),
        None => not_found(&name),
    }
}

fn metadata(args: &[i64]) -> i64 {
    let name = match name_argument(args, "metadata") {
        Ok(name) => name,
        Err(raised) => return raised,
    };
    match find_dist(&name) {
        Some(dist) => metadata_dict(&dist),
        None => not_found(&name),
    }
}

fn requires(args: &[i64]) -> i64 {
    let name = match name_argument(args, "requires") {
        Ok(name) => name,
        Err(raised) => return raised,
    };
    match find_dist(&name) {
        Some(dist) if !dist.requires.is_empty() => strings(dist.requires),
        Some(_) => NONE,
        None => not_found(&name),
    }
}

fn distribution(args: &[i64]) -> i64 {
    let name = match name_argument(args, "distribution") {
        Ok(name) => name,
        Err(raised) => return raised,
    };
    match find_dist(&name) {
        Some(dist) => distribution_value(&dist),
        None => not_found(&name),
    }
}

fn keyword_text(keywords: &[(String, i64)], name: &str) -> Result<Option<String>, i64> {
    let Some((_, value)) = keywords.iter().find(|(key, _)| key.as_str() == name) else {
        return Ok(None);
    };
    match value::as_str(*value) {
        Some(text) => Ok(Some(text.to_string())),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "argument '{name}' must be str, not '{}'",
                value::type_name(*value)
            ),
        )),
    }
}

fn distributions_kw(keywords: &[(String, i64)]) -> i64 {
    let name = match keyword_text(keywords, "name") {
        Ok(name) => name,
        Err(raised) => return raised,
    };
    let path = match keyword_text(keywords, "path") {
        Ok(path) => path,
        Err(raised) => return raised,
    };
    if let Some(unknown) = keywords
        .iter()
        .find(|(key, _)| !matches!(key.as_str(), "name" | "path"))
    {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "distributions() got an unexpected keyword argument '{}'",
                unknown.0
            ),
        );
    }
    let dists = match path {
        Some(path) => collect_from_paths(&[path]),
        None => distributions_cache(),
    };
    let selected: Vec<i64> = dists
        .iter()
        .filter(|dist| {
            name.as_ref()
                .is_none_or(|name| dist.normalized == normalize(name))
        })
        .map(distribution_value)
        .collect();
    value::list_new(selected)
}

fn entry_points_kw(keywords: &[(String, i64)]) -> i64 {
    let mut filters: Vec<(String, String)> = Vec::new();
    for (key, value) in keywords {
        if !matches!(key.as_str(), "group" | "name" | "value" | "module" | "attr") {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("entry_points() got an unexpected keyword argument '{key}'"),
            );
        }
        let Some(text) = value::as_str(*value) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("entry_points() argument '{key}' must be str"),
            );
        };
        filters.push((key.to_string(), text.to_string()));
    }
    let mut selected = Vec::new();
    for dist in distributions_cache() {
        for entry in &dist.entry_points {
            let matches = filters.iter().all(|(key, expected)| {
                let actual = match key.as_str() {
                    "group" => entry.group.as_str(),
                    "name" => entry.name.as_str(),
                    "value" => entry.value.as_str(),
                    "module" => entry.module.as_str(),
                    _ => entry.attr.as_str(),
                };
                actual == expected
            });
            if matches {
                selected.push(entry_point_value(entry));
            }
        }
    }
    value::list_new(selected)
}
