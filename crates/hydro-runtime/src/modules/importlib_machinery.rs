//! `importlib.machinery`: loaders and `ModuleSpec`.
//!
//! Hydro resolves modules ahead of time: a compiled module's initializer has
//! already run by the time user code executes, so the loaders here report the
//! *loaded* module instead of re-executing source. Everything that would need
//! a runtime Python compiler (`SourceFileLoader.exec_module` on a module that
//! is not part of the program) fails with a precise `ImportError` rather than
//! fabricating an empty module.
//!
//! `ModuleSpec` and the loader objects are real native class instances, so
//! `isinstance`, attribute assignment (`spec.loader = ...`) and method binding
//! behave like CPython. Path finders (`BuiltinImporter`, `FrozenImporter`) are
//! classes with classmethods, matching `importlib.machinery`'s package
//! attributes.

use std::path::Path;

use hydro_ir::MethodKind;

use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, IMPORT_ERROR, TYPE_ERROR};
use crate::value::{self, NONE};

pub(crate) const MODULE: &str = "importlib.machinery";

const SPEC_INIT: &str = "_spec_init";
const MODULE_SPEC: &str = "_module_spec";
const SOURCE_INIT: &str = "_source_init";
const SOURCE_GET_FILENAME: &str = "_source_get_filename";
const SOURCE_CREATE_MODULE: &str = "_source_create_module";
const SOURCE_EXEC_MODULE: &str = "_source_exec_module";
const EXTENSION_INIT: &str = "_extension_init";
const EXTENSION_GET_FILENAME: &str = "_extension_get_filename";
const EXTENSION_CREATE_MODULE: &str = "_extension_create_module";
const EXTENSION_EXEC_MODULE: &str = "_extension_exec_module";
const BUILTIN_FIND_SPEC: &str = "_builtin_find_spec";
const BUILTIN_CREATE_MODULE: &str = "_builtin_create_module";
const BUILTIN_EXEC_MODULE: &str = "_builtin_exec_module";
const BUILTIN_INVALIDATE: &str = "_builtin_invalidate_caches";
const FROZEN_FIND_SPEC: &str = "_frozen_find_spec";
const FROZEN_CREATE_MODULE: &str = "_frozen_create_module";
const FROZEN_EXEC_MODULE: &str = "_frozen_exec_module";
const FROZEN_INVALIDATE: &str = "_frozen_invalidate_caches";

/// Dispatches a native `importlib.machinery` function. Arity is enforced by
/// the central spec before this point.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        SPEC_INIT => spec_init(args),
        MODULE_SPEC => module_spec(args),
        SOURCE_INIT => source_init(args),
        SOURCE_GET_FILENAME => get_filename(args),
        SOURCE_CREATE_MODULE => create_module(args),
        SOURCE_EXEC_MODULE => exec_module(args),
        EXTENSION_INIT => source_init(args),
        EXTENSION_GET_FILENAME => get_filename(args),
        EXTENSION_CREATE_MODULE => create_module(args),
        EXTENSION_EXEC_MODULE => exec_module(args),
        BUILTIN_FIND_SPEC | FROZEN_FIND_SPEC => importer_find_spec(function, args),
        BUILTIN_CREATE_MODULE => builtin_create_module(args),
        BUILTIN_EXEC_MODULE => builtin_exec_module(args),
        FROZEN_CREATE_MODULE => frozen_create_module(args),
        FROZEN_EXEC_MODULE => frozen_exec_module(args),
        BUILTIN_INVALIDATE | FROZEN_INVALIDATE => NONE,
        "all_suffixes" => all_suffixes(),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '{MODULE}' has no attribute '{function}'"),
        ),
    }
}

/// Materialises a `importlib.machinery` module constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "ModuleSpec" => module_spec_class(),
        "Loader" => loader_class(),
        "SourceFileLoader" => source_loader_class(),
        "ExtensionFileLoader" => extension_loader_class(),
        "BuiltinImporter" => builtin_importer_class(),
        "FrozenImporter" => frozen_importer_class(),
        // The module-suffix lists. They are cached, mutable lists like
        // CPython's module-level lists (`SOURCE_SUFFIXES.append(...)` sticks).
        "SOURCE_SUFFIXES" => {
            suffix_list("importlib.machinery.SOURCE_SUFFIXES", &[".py".to_string()])
        }
        "BYTECODE_SUFFIXES" => suffix_list(
            "importlib.machinery.BYTECODE_SUFFIXES",
            &[".pyc".to_string()],
        ),
        "EXTENSION_SUFFIXES" => extension_suffix_list(),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '{MODULE}' has no attribute '{name}'"),
        ),
    }
}

/// `all_suffixes()`: `SOURCE_SUFFIXES + BYTECODE_SUFFIXES +
/// EXTENSION_SUFFIXES`, as a fresh list.
pub(crate) fn all_suffixes() -> i64 {
    let mut suffixes: Vec<i64> = Vec::new();
    for name in ["SOURCE_SUFFIXES", "BYTECODE_SUFFIXES", "EXTENSION_SUFFIXES"] {
        let list = constant(name);
        if exc::pending() {
            return NONE;
        }
        if let Some(items) = value::as_list(list) {
            suffixes.extend_from_slice(items);
        }
    }
    let roots = crate::heap::root_values(&suffixes);
    let result = value::list_new(suffixes);
    crate::heap::release_roots(roots);
    result
}

/// A cached list of suffix strings under a registry key.
fn suffix_list(key: &str, suffixes: &[String]) -> i64 {
    if let Some(list) = globals::registry_get(key) {
        return list;
    }
    let items: Vec<i64> = suffixes
        .iter()
        .map(|suffix| value::str_alloc(suffix.clone()))
        .collect();
    let roots = crate::heap::root_values(&items);
    let list = value::list_new(items);
    let list_roots = crate::heap::root_values(&[list]);
    globals::registry_set(key, list);
    crate::heap::release_roots(list_roots);
    crate::heap::release_roots(roots);
    list
}

/// `EXTENSION_SUFFIXES`: the ABI-tagged suffix for this target followed by the
/// generic `abi3`/`so` names, matching what the extension loader accepts.
fn extension_suffix_list() -> i64 {
    #[cfg(target_os = "windows")]
    let suffixes: Vec<String> = vec![".pyd".to_string()];
    #[cfg(target_os = "macos")]
    let suffixes: Vec<String> = {
        let abi = format!(".{}-darwin.so", hydro_ir::PYTHON_ABI_TAG);
        vec![abi, ".abi3.so".to_string(), ".so".to_string()]
    };
    #[cfg(all(unix, target_os = "linux"))]
    let suffixes: Vec<String> = {
        let libc = if cfg!(target_env = "musl") {
            "linux-musl"
        } else {
            "linux-gnu"
        };
        let arch = std::env::consts::ARCH;
        let abi = format!(".{}-{arch}-{libc}.so", hydro_ir::PYTHON_ABI_TAG);
        vec![abi, ".abi3.so".to_string(), ".so".to_string()]
    };
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "linux")))]
    let suffixes: Vec<String> = vec![".abi3.so".to_string(), ".so".to_string()];
    suffix_list("importlib.machinery.EXTENSION_SUFFIXES", &suffixes)
}

// -------------------------------------------------------------------------
// Classes
// -------------------------------------------------------------------------

/// Allocates (once) a native class under a registry key.
fn cached_class(key: &str, name: &str, base: i64, methods: Vec<(String, i64, MethodKind)>) -> i64 {
    if let Some(class) = globals::registry_get(key) {
        return class;
    }
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

pub(crate) fn module_spec_class() -> i64 {
    cached_class(
        "importlib.machinery.ModuleSpec",
        "ModuleSpec",
        NONE,
        vec![(
            "__init__".to_string(),
            native(SPEC_INIT, 6),
            MethodKind::Normal,
        )],
    )
}

/// `importlib.machinery.Loader`: the loader base class.
pub(crate) fn loader_class() -> i64 {
    cached_class("importlib.machinery.Loader", "Loader", NONE, Vec::new())
}

/// `importlib.machinery.SourceFileLoader(name, path)`.
pub(crate) fn source_loader_class() -> i64 {
    cached_class(
        "importlib.machinery.SourceFileLoader",
        "SourceFileLoader",
        loader_class(),
        vec![
            (
                "__init__".to_string(),
                native(SOURCE_INIT, 3),
                MethodKind::Normal,
            ),
            (
                "get_filename".to_string(),
                native(SOURCE_GET_FILENAME, 2),
                MethodKind::Normal,
            ),
            (
                "create_module".to_string(),
                native(SOURCE_CREATE_MODULE, 2),
                MethodKind::Normal,
            ),
            (
                "exec_module".to_string(),
                native(SOURCE_EXEC_MODULE, 2),
                MethodKind::Normal,
            ),
        ],
    )
}

/// `importlib.machinery.ExtensionFileLoader(name, path)`.
pub(crate) fn extension_loader_class() -> i64 {
    cached_class(
        "importlib.machinery.ExtensionFileLoader",
        "ExtensionFileLoader",
        loader_class(),
        vec![
            (
                "__init__".to_string(),
                native(EXTENSION_INIT, 3),
                MethodKind::Normal,
            ),
            (
                "get_filename".to_string(),
                native(EXTENSION_GET_FILENAME, 2),
                MethodKind::Normal,
            ),
            (
                "create_module".to_string(),
                native(EXTENSION_CREATE_MODULE, 2),
                MethodKind::Normal,
            ),
            (
                "exec_module".to_string(),
                native(EXTENSION_EXEC_MODULE, 2),
                MethodKind::Normal,
            ),
        ],
    )
}

/// `importlib.machinery.BuiltinImporter`: classmethods for native modules.
pub(crate) fn builtin_importer_class() -> i64 {
    cached_class(
        "importlib.machinery.BuiltinImporter",
        "BuiltinImporter",
        NONE,
        vec![
            (
                "find_spec".to_string(),
                native(BUILTIN_FIND_SPEC, 3),
                MethodKind::Classmethod,
            ),
            (
                "create_module".to_string(),
                native(BUILTIN_CREATE_MODULE, 2),
                MethodKind::Classmethod,
            ),
            (
                "exec_module".to_string(),
                native(BUILTIN_EXEC_MODULE, 2),
                MethodKind::Classmethod,
            ),
            (
                "invalidate_caches".to_string(),
                native(BUILTIN_INVALIDATE, 1),
                MethodKind::Classmethod,
            ),
        ],
    )
}

/// `importlib.machinery.FrozenImporter`: Hydro ships no frozen modules, so
/// discovery returns `None` and `create_module` reports the exact reason.
pub(crate) fn frozen_importer_class() -> i64 {
    cached_class(
        "importlib.machinery.FrozenImporter",
        "FrozenImporter",
        NONE,
        vec![
            (
                "find_spec".to_string(),
                native(FROZEN_FIND_SPEC, 3),
                MethodKind::Classmethod,
            ),
            (
                "create_module".to_string(),
                native(FROZEN_CREATE_MODULE, 2),
                MethodKind::Classmethod,
            ),
            (
                "exec_module".to_string(),
                native(FROZEN_EXEC_MODULE, 2),
                MethodKind::Classmethod,
            ),
            (
                "invalidate_caches".to_string(),
                native(FROZEN_INVALIDATE, 1),
                MethodKind::Classmethod,
            ),
        ],
    )
}

// -------------------------------------------------------------------------
// Loader instances
// -------------------------------------------------------------------------

/// `SourceFileLoader(name, path)` instance.
pub(crate) fn source_loader(name: &str, path: &str) -> i64 {
    loader_instance(source_loader_class(), name, path)
}

/// `ExtensionFileLoader(name, path)` instance.
pub(crate) fn extension_loader(name: &str, path: &str) -> i64 {
    loader_instance(extension_loader_class(), name, path)
}

fn loader_instance(class: i64, name: &str, path: &str) -> i64 {
    let instance = value::instance_new(class);
    value::instance_setattr(
        instance,
        "name".to_string(),
        value::str_alloc(name.to_string()),
    );
    value::instance_setattr(
        instance,
        "path".to_string(),
        value::str_alloc(path.to_string()),
    );
    value::instance_setattr(
        instance,
        "fullname".to_string(),
        value::str_alloc(name.to_string()),
    );
    instance
}

// -------------------------------------------------------------------------
// ModuleSpec
// -------------------------------------------------------------------------

/// Builds a `ModuleSpec` instance with the documented attributes.
///
/// `origin` is the absolute file path for file-backed specs, `None` for
/// built-in modules. `search_locations` is `Some` exactly when the module is a
/// package. `has_location` mirrors CPython's `_set_fileattr`.
pub(crate) fn new_spec(
    name: &str,
    loader: i64,
    origin: Option<String>,
    search_locations: Option<Vec<String>>,
    has_location: bool,
) -> i64 {
    let spec = value::instance_new(module_spec_class());
    let roots = crate::heap::root_values(&[spec, loader]);
    set_spec_fields(
        spec,
        name,
        loader,
        origin.as_deref(),
        search_locations,
        has_location,
    );
    crate::heap::release_roots(roots);
    spec
}

fn set_spec_fields(
    spec: i64,
    name: &str,
    loader: i64,
    origin: Option<&str>,
    search_locations: Option<Vec<String>>,
    has_location: bool,
) {
    let is_package = search_locations.is_some();
    value::instance_setattr(spec, "name".to_string(), value::str_alloc(name.to_string()));
    value::instance_setattr(spec, "loader".to_string(), loader);
    value::instance_setattr(
        spec,
        "origin".to_string(),
        origin.map(str_value).unwrap_or(NONE),
    );
    let locations =
        search_locations.map(|dirs| value::list_new(dirs.into_iter().map(str_value).collect()));
    value::instance_setattr(
        spec,
        "submodule_search_locations".to_string(),
        locations.unwrap_or(NONE),
    );
    // CPython's `parent` is the package the module lives in; a package is its
    // own parent, and a top-level (non-package) module has no package.
    let parent = if is_package {
        name.to_string()
    } else {
        name.rsplit_once('.')
            .map(|(parent, _)| parent.to_string())
            .unwrap_or_default()
    };
    value::instance_setattr(spec, "parent".to_string(), value::str_alloc(parent));
    value::instance_setattr(
        spec,
        "has_location".to_string(),
        value::bool_value(has_location),
    );
    let cached = if has_location {
        origin.and_then(cached_from_origin)
    } else {
        None
    };
    value::instance_setattr(
        spec,
        "cached".to_string(),
        cached.map(str_value).unwrap_or(NONE),
    );
    value::instance_setattr(spec, "loader_state".to_string(), NONE);
    value::instance_setattr(
        spec,
        "_set_fileattr".to_string(),
        value::bool_value(has_location),
    );
}

fn str_value(text: impl Into<String>) -> i64 {
    value::str_alloc(text.into())
}

/// `__pycache__/<stem>.cpython-314.pyc` for a source file, `None` otherwise
/// (extension modules and built-ins have no cached bytecode).
fn cached_from_origin(origin: &str) -> Option<String> {
    let path = Path::new(origin);
    if path.extension().and_then(|ext| ext.to_str()) != Some("py") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    let dir = path.parent()?;
    Some(
        dir.join("__pycache__")
            .join(format!("{stem}.cpython-314.pyc"))
            .to_string_lossy()
            .into_owned(),
    )
}

/// `spec.name` for a spec instance.
pub(crate) fn spec_name(spec: i64) -> Option<String> {
    value::instance_attr(spec, "name")
        .and_then(value::as_str)
        .map(str::to_string)
}

/// `spec.origin` for a spec instance (absent, `None`, or a string).
pub(crate) fn spec_origin(spec: i64) -> Option<String> {
    value::instance_attr(spec, "origin")
        .and_then(value::as_str)
        .map(str::to_string)
}

/// `spec.submodule_search_locations` as owned directories.
pub(crate) fn spec_search_locations(spec: i64) -> Option<Vec<String>> {
    let raw = value::instance_attr(spec, "submodule_search_locations")?;
    let items = value::as_list(raw)?;
    Some(
        items
            .iter()
            .filter_map(|item| value::as_str(*item).map(str::to_string))
            .collect(),
    )
}

/// `spec.has_location` for a spec instance.
pub(crate) fn spec_has_location(spec: i64) -> bool {
    value::instance_attr(spec, "has_location").is_some_and(value::truthy)
}

/// Assigns `spec.loader` (used by `spec_from_file_location` when it picks a
/// loader itself).
pub(crate) fn set_spec_loader(spec: i64, loader: i64) {
    value::instance_setattr(spec, "loader".to_string(), loader);
}

/// Replaces `submodule_search_locations`, keeping `parent` in sync: a package
/// is its own parent, exactly like CPython's `ModuleSpec.parent`.
pub(crate) fn set_package_locations(spec: i64, dirs: Vec<String>) {
    let list = value::list_new(dirs.into_iter().map(str_value).collect());
    let roots = crate::heap::root_values(&[spec, list]);
    value::instance_setattr(spec, "submodule_search_locations".to_string(), list);
    let name = spec_name(spec).unwrap_or_default();
    value::instance_setattr(spec, "parent".to_string(), str_value(name));
    crate::heap::release_roots(roots);
}

/// `_module_spec(name, origin, is_package)`: the `ModuleSpec` a compiled unit
/// publishes as `__spec__`.
///
/// `origin` is the unit's `__file__` (or `None` when no path is known);
/// `is_package` marks a package `__init__.py`, whose search location is the
/// directory holding it. A file-backed module gets a real `SourceFileLoader`,
/// so `__loader__` is `__spec__.loader` and `spec.loader.path` names the
/// compiled source.
fn module_spec(args: &[i64]) -> i64 {
    let Some(name) = value::as_str(args[0]) else {
        return exc::raise_with_message(TYPE_ERROR, "_module_spec() argument 'name' must be str");
    };
    let origin = value::as_str(args[1]).map(str::to_string);
    let is_package = value::truthy(args[2]);
    let loader = match origin.as_deref() {
        Some(path) => source_loader(name, path),
        None => builtin_importer_class(),
    };
    let search_locations = if is_package {
        Some(
            origin
                .as_deref()
                .and_then(|path| Path::new(path).parent())
                .map(|dir| dir.to_string_lossy().into_owned())
                .into_iter()
                .collect(),
        )
    } else {
        None
    };
    let has_location = origin.is_some();
    new_spec(name, loader, origin, search_locations, has_location)
}

/// Whether `value` is a `ModuleSpec` (or a subclass) instance.
pub(crate) fn is_spec(value: i64) -> bool {
    let Some(class) = value::instance_class_value(value) else {
        return false;
    };
    value::is_subclass_of(class, module_spec_class())
}

// -------------------------------------------------------------------------
// Method implementations
// -------------------------------------------------------------------------

fn spec_init(args: &[i64]) -> i64 {
    let receiver = args[0];
    let Some(name) = value::as_str(args[1]) else {
        return exc::raise_with_message(TYPE_ERROR, "ModuleSpec() argument 'name' must be str");
    };
    let name = name.to_string();
    let loader = args.get(2).copied().unwrap_or(NONE);
    let origin = args.get(3).copied().unwrap_or(NONE);
    let loader_state = args.get(4).copied().unwrap_or(NONE);
    let is_package = args.get(5).copied().unwrap_or(NONE);
    let origin_text = value::as_str(origin).map(str::to_string);
    let has_location = origin_text
        .as_deref()
        .is_some_and(|text| text != "<unknown>");
    let search_locations = if value::truthy(is_package) {
        let parent = origin_text
            .as_deref()
            .and_then(|text| Path::new(text).parent())
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default();
        Some(if parent.is_empty() {
            Vec::new()
        } else {
            vec![parent]
        })
    } else {
        None
    };
    set_spec_fields(
        receiver,
        &name,
        loader,
        origin_text.as_deref(),
        search_locations,
        has_location,
    );
    value::instance_setattr(receiver, "loader_state".to_string(), loader_state);
    NONE
}

fn loader_field(loader: i64, name: &str) -> Option<String> {
    value::instance_attr(loader, name)
        .and_then(value::as_str)
        .map(str::to_string)
}

fn source_init(args: &[i64]) -> i64 {
    let receiver = args[0];
    let Some(name) = value::as_str(args[1]) else {
        return exc::raise_with_message(TYPE_ERROR, "loader name must be str, not 'int'");
    };
    let Some(path) = value::as_str(args[2]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "loader path must be str or os.PathLike, not 'int'",
        );
    };
    for (field, text) in [("name", name), ("fullname", name), ("path", path)] {
        value::instance_setattr(
            receiver,
            field.to_string(),
            value::str_alloc(text.to_string()),
        );
    }
    NONE
}

fn get_filename(args: &[i64]) -> i64 {
    let loader = args[0];
    let path = loader_field(loader, "path").unwrap_or_default();
    if let Some(requested) = value::as_str(args[1]) {
        if let Some(name) = loader_field(loader, "name") {
            if requested != name {
                return exc::raise_with_message(
                    IMPORT_ERROR,
                    &format!("cannot get filename for {requested}: loader handles {name}"),
                );
            }
        }
    }
    str_value(path)
}

/// `loader.create_module(spec)`: return the already-loaded module, or `None`
/// so module_from_spec creates a fresh module object.
fn create_module(args: &[i64]) -> i64 {
    let spec = args[1];
    let Some(name) = spec_name(spec) else {
        return NONE;
    };
    super::registry_lookup(&name).unwrap_or(NONE)
}

/// Shared `exec_module` implementation: a module whose body is part of the
/// compiled program has already run, so nothing remains to execute. Any other
/// module cannot be executed at runtime.
pub(crate) fn exec_module(args: &[i64]) -> i64 {
    let module = args[1];
    exec_loaded_module(module)
}

pub(crate) fn exec_loaded_module(module: i64) -> i64 {
    let name = module_name(module);
    let Some(name) = name else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "exec_module() argument must be a module, not '{}'",
                value::type_name(module)
            ),
        );
    };
    if super::registry_lookup(&name).is_some() {
        // The compiled initializer already ran; executing again would be a
        // silent no-op, which is exactly what CPython's built-in loaders do.
        return NONE;
    }
    let origin = value::instance_attr(module, "__file__")
        .and_then(value::as_str)
        .map(str::to_string);
    let detail = match origin {
        Some(path) => format!("cannot execute module '{name}' from '{path}'"),
        None => format!("cannot execute module '{name}'"),
    };
    exc::raise_with_message(
        IMPORT_ERROR,
        &format!(
            "{detail}: Hydro has no runtime Python compiler; Python modules are \
             compiled into the program ahead of time"
        ),
    )
}

/// Name of a module value or of a module-like namespace created by
/// `module_from_spec`.
pub(crate) fn module_name(module: i64) -> Option<String> {
    if let Some(state) = value::as_module(module) {
        return Some(state.name().to_string());
    }
    let pairs = value::as_namespace(module)?;
    for (key, item) in pairs {
        if key.as_str() == "__name__" {
            return value::as_str(*item).map(str::to_string);
        }
    }
    None
}

fn importer_find_spec(function: &str, args: &[i64]) -> i64 {
    let Some(name) = value::as_str(args[1]) else {
        return exc::raise_with_message(TYPE_ERROR, "find_spec() argument 'name' must be str");
    };
    if function == FROZEN_FIND_SPEC {
        // Hydro bundles no frozen modules. Returning `None` is the truthful
        // answer; pretending a frozen `importlib._bootstrap` existed would
        // hand callers a module that cannot be executed.
        return NONE;
    }
    if hydro_ir::stdlib::module(name).is_none() {
        return NONE;
    }
    super::importlib_util::native_spec(name)
}

fn builtin_create_module(args: &[i64]) -> i64 {
    let spec = args[1];
    let Some(name) = spec_name(spec) else {
        return NONE;
    };
    if hydro_ir::stdlib::module(&name).is_none() {
        return exc::raise_with_message(
            IMPORT_ERROR,
            &format!("'{name}' is not a built-in module"),
        );
    }
    super::registry_lookup(&name).unwrap_or_else(|| value::module_value(name))
}

fn builtin_exec_module(args: &[i64]) -> i64 {
    let module = args[1];
    match module_name(module) {
        Some(name) if hydro_ir::stdlib::module(&name).is_some() => NONE,
        Some(name) => {
            exc::raise_with_message(IMPORT_ERROR, &format!("'{name}' is not a built-in module"))
        }
        None => exc::raise_with_message(TYPE_ERROR, "exec_module() argument must be a module"),
    }
}

fn frozen_create_module(args: &[i64]) -> i64 {
    let name = spec_name(args[1]).unwrap_or_else(|| "<unknown>".to_string());
    exc::raise_with_message(IMPORT_ERROR, &format!("no frozen module named '{name}'"))
}

fn frozen_exec_module(args: &[i64]) -> i64 {
    match module_name(args[1]) {
        Some(name) => {
            exc::raise_with_message(IMPORT_ERROR, &format!("no frozen module named '{name}'"))
        }
        None => exc::raise_with_message(TYPE_ERROR, "exec_module() argument must be a module"),
    }
}

// -------------------------------------------------------------------------
// Path helpers shared with `importlib.util`
// -------------------------------------------------------------------------

/// Absolute form of a path without requiring it to exist.
pub(crate) fn absolute(path: &str) -> String {
    let path = Path::new(path);
    if path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }
    match std::env::current_dir() {
        Ok(dir) => dir.join(path).to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(v: i64) -> Vec<String> {
        value::as_list(v)
            .expect("list")
            .iter()
            .filter_map(|item| value::as_str(*item).map(str::to_string))
            .collect()
    }

    #[test]
    fn suffix_lists_are_cached_and_mutable() {
        exc::init();
        assert_eq!(items(constant("SOURCE_SUFFIXES")), vec![".py"]);
        assert_eq!(items(constant("BYTECODE_SUFFIXES")), vec![".pyc"]);
        assert_eq!(constant("SOURCE_SUFFIXES"), constant("SOURCE_SUFFIXES"));

        let extensions = items(constant("EXTENSION_SUFFIXES"));
        assert!(!extensions.is_empty());
        assert!(
            extensions.iter().any(|suffix| suffix.ends_with(".so")) || cfg!(windows),
            "{extensions:?}"
        );

        // The cached list is the live module list: appending to it is visible
        // on the next lookup, like CPython's mutable module list.
        let source = constant("SOURCE_SUFFIXES");
        let suffix = value::str_alloc(".pyi".to_string());
        crate::container::list_append(source, suffix);
        assert!(items(constant("SOURCE_SUFFIXES")).contains(&".pyi".to_string()));
        assert!(!exc::pending());
    }

    #[test]
    fn compiled_module_specs_describe_the_unit() {
        exc::init();
        let source = "/tmp/hydro/pkg/mod.py";
        let spec = call(
            "_module_spec",
            &[
                value::str_alloc("pkg.mod".to_string()),
                value::str_alloc(source.to_string()),
                value::bool_value(false),
            ],
        );
        assert!(!exc::pending());
        assert_eq!(spec_name(spec).as_deref(), Some("pkg.mod"));
        assert_eq!(spec_origin(spec).as_deref(), Some(source));
        assert!(spec_has_location(spec));
        assert_eq!(spec_search_locations(spec), None);
        // A submodule lives in its parent package.
        assert_eq!(
            value::instance_attr(spec, "parent").and_then(value::as_str),
            Some("pkg")
        );
        // The loader is the real source loader for the compiled file, so
        // `__loader__ is __spec__.loader` and its path names the source.
        let loader = value::instance_attr(spec, "loader").expect("loader");
        assert_eq!(
            value::class_display_name(value::instance_class_value(loader).expect("loader class")),
            Some("SourceFileLoader")
        );
        assert_eq!(
            value::instance_attr(loader, "path").and_then(value::as_str),
            Some(source)
        );

        // A package is its own parent and publishes its directory.
        let pkg_source = "/tmp/hydro/pkg/__init__.py";
        let pkg = call(
            "_module_spec",
            &[
                value::str_alloc("pkg".to_string()),
                value::str_alloc(pkg_source.to_string()),
                value::bool_value(true),
            ],
        );
        assert!(!exc::pending());
        assert_eq!(
            value::instance_attr(pkg, "parent").and_then(value::as_str),
            Some("pkg")
        );
        assert_eq!(
            spec_search_locations(pkg),
            Some(vec!["/tmp/hydro/pkg".to_string()])
        );

        // A top-level module has no parent package.
        let top = call(
            "_module_spec",
            &[
                value::str_alloc("top".to_string()),
                value::str_alloc("/tmp/hydro/top.py".to_string()),
                value::bool_value(false),
            ],
        );
        assert!(!exc::pending());
        assert_eq!(
            value::instance_attr(top, "parent").and_then(value::as_str),
            Some("")
        );
    }

    #[test]
    fn all_suffixes_concatenates_the_three_lists() {
        exc::init();
        let combined = all_suffixes();
        let suffixes = items(combined);
        assert!(suffixes.contains(&".py".to_string()));
        assert!(suffixes.contains(&".pyc".to_string()));
        assert!(suffixes.len() >= 4);
        // A fresh list per call, not the cached source list.
        let second = call("all_suffixes", &[]);
        assert_ne!(combined, second);
        assert_eq!(items(second), suffixes);
        assert!(!exc::pending());
    }
}
