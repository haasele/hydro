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

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use hydro_ir::MethodKind;

use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, IMPORT_ERROR, NOT_IMPLEMENTED_ERROR, OS_ERROR, TYPE_ERROR,
};
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
const INSTALL: &str = "_install";
const PATH_FINDER_FIND_SPEC: &str = "_path_finder_find_spec";
const PATH_FINDER_INVALIDATE: &str = "_path_finder_invalidate_caches";
const FILE_FINDER_INIT: &str = "_file_finder_init";
const FILE_FINDER_PATH_HOOK: &str = "_file_finder_path_hook";
const FILE_FINDER_HOOK: &str = "_file_finder_hook";
const FILE_FINDER_FIND_SPEC: &str = "_file_finder_find_spec";
const FILE_FINDER_INVALIDATE: &str = "_file_finder_invalidate_caches";
const LOADER_BASICS_IS_PACKAGE: &str = "_loader_basics_is_package";
const SOURCE_GET_SOURCE: &str = "_source_get_source";
const SOURCE_GET_CODE: &str = "_source_get_code";
const SOURCE_SOURCE_TO_CODE: &str = "_source_source_to_code";
const SOURCE_PATH_STATS: &str = "_source_path_stats";
const SOURCE_PATH_MTIME: &str = "_source_path_mtime";
const SOURCE_SET_DATA: &str = "_source_set_data";
const FILE_LOADER_GET_DATA: &str = "_file_loader_get_data";
const EXTENSION_IS_PACKAGE: &str = "_extension_is_package";
const NAMESPACE_LOADER_CREATE_MODULE: &str = "_namespace_loader_create_module";
const NAMESPACE_LOADER_IS_PACKAGE: &str = "_namespace_loader_is_package";
const NAMESPACE_LOADER_GET_CODE: &str = "_namespace_loader_get_code";
const NAMESPACE_LOADER_GET_SOURCE: &str = "_namespace_loader_get_source";
const NAMESPACE_LOADER_EXEC_MODULE: &str = "_namespace_loader_exec_module";
const WINDOWS_REGISTRY_FIND_SPEC: &str = "_windows_registry_finder_find_spec";
const ZIPIMPORTER_INIT: &str = "_zipimporter_init";
const ZIPIMPORTER_FIND_SPEC: &str = "_zipimporter_find_spec";
const ZIPIMPORTER_IS_PACKAGE: &str = "_zipimporter_is_package";
const ZIPIMPORTER_GET_FILENAME: &str = "_zipimporter_get_filename";
const ZIPIMPORTER_GET_SOURCE: &str = "_zipimporter_get_source";
const ZIPIMPORTER_GET_CODE: &str = "_zipimporter_get_code";
const ZIPIMPORTER_GET_DATA: &str = "_zipimporter_get_data";
const ZIPIMPORTER_LOAD_MODULE: &str = "_zipimporter_load_module";
const ZIPIMPORTER_INVALIDATE: &str = "_zipimporter_invalidate_caches";
const ZIPIMPORTER_GET_FILES: &str = "_zipimporter_get_files";

// `importlib.abc` class methods. Their function values carry the
// `importlib.machinery` module id so the central dispatch reaches them; the
// `importlib.abc` module spec therefore only declares constants.
const LOADER_CREATE_MODULE: &str = "_loader_create_module";
const LOADER_LOAD_MODULE: &str = "_loader_load_module";
const META_PATH_FINDER_INVALIDATE: &str = "_meta_path_finder_invalidate_caches";
const PATH_ENTRY_FINDER_INVALIDATE: &str = "_path_entry_finder_invalidate_caches";
const FINDER_FIND_MODULE: &str = "_finder_find_module";
const RESOURCE_LOADER_GET_DATA: &str = "_resource_loader_get_data";
const INSPECT_LOADER_IS_PACKAGE: &str = "_inspect_loader_is_package";
const INSPECT_LOADER_GET_SOURCE: &str = "_inspect_loader_get_source";
const INSPECT_LOADER_GET_CODE: &str = "_inspect_loader_get_code";
const INSPECT_LOADER_SOURCE_TO_CODE: &str = "_inspect_loader_source_to_code";
const EXECUTION_LOADER_GET_FILENAME: &str = "_execution_loader_get_filename";
const EXECUTION_LOADER_GET_CODE: &str = "_execution_loader_get_code";
const RESOURCE_READER_OPEN_RESOURCE: &str = "_resource_reader_open_resource";
const RESOURCE_READER_RESOURCE_PATH: &str = "_resource_reader_resource_path";
const RESOURCE_READER_IS_RESOURCE: &str = "_resource_reader_is_resource";
const RESOURCE_READER_CONTENTS: &str = "_resource_reader_contents";
const TRAVERSABLE_RESOURCES_FILES: &str = "_traversable_resources_files";

/// Registry key of the bootstrap module `_install()` was called with.
const BOOTSTRAP_KEY: &str = "importlib.machinery._bootstrap";

/// Registry key of `zipimport._zip_directory_cache` (shared with the
/// `zipimport` alias).
const ZIP_DIRECTORY_CACHE: &str = "zipimport._zip_directory_cache";

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
        INSTALL => install(args),
        PATH_FINDER_FIND_SPEC => path_finder_find_spec(args),
        PATH_FINDER_INVALIDATE | FILE_FINDER_INVALIDATE => NONE,
        FILE_FINDER_INIT => file_finder_init(args),
        FILE_FINDER_PATH_HOOK => file_finder_path_hook(args),
        FILE_FINDER_HOOK => file_finder_hook(args),
        FILE_FINDER_FIND_SPEC => file_finder_find_spec(args),
        LOADER_BASICS_IS_PACKAGE => loader_basics_is_package(args),
        SOURCE_GET_SOURCE => source_get_source(args),
        SOURCE_GET_CODE => source_get_code(args),
        SOURCE_SOURCE_TO_CODE => source_to_code(args),
        SOURCE_PATH_STATS => source_path_stats(args),
        SOURCE_PATH_MTIME => source_path_mtime(args),
        SOURCE_SET_DATA => source_set_data(args),
        FILE_LOADER_GET_DATA => file_loader_get_data(args),
        EXTENSION_IS_PACKAGE => extension_is_package(args),
        NAMESPACE_LOADER_EXEC_MODULE => namespace_exec_module(),
        NAMESPACE_LOADER_CREATE_MODULE
        | NAMESPACE_LOADER_GET_CODE
        | NAMESPACE_LOADER_GET_SOURCE
        | WINDOWS_REGISTRY_FIND_SPEC => NONE,
        NAMESPACE_LOADER_IS_PACKAGE => value::bool_value(true),
        ZIPIMPORTER_INIT => zipimporter_init(args),
        ZIPIMPORTER_FIND_SPEC => zipimporter_find_spec(args),
        ZIPIMPORTER_IS_PACKAGE => zipimporter_is_package(args),
        ZIPIMPORTER_GET_FILENAME => zipimporter_get_filename(args),
        ZIPIMPORTER_GET_SOURCE => zipimporter_get_source(args),
        ZIPIMPORTER_GET_CODE => zipimporter_get_code(args),
        ZIPIMPORTER_GET_DATA => zipimporter_get_data(args),
        ZIPIMPORTER_LOAD_MODULE => zipimporter_load_module(args),
        ZIPIMPORTER_INVALIDATE => zipimporter_invalidate(args),
        ZIPIMPORTER_GET_FILES => zipimporter_get_files(args),
        LOADER_CREATE_MODULE => NONE,
        LOADER_LOAD_MODULE => loader_load_module(args),
        META_PATH_FINDER_INVALIDATE | PATH_ENTRY_FINDER_INVALIDATE => NONE,
        FINDER_FIND_MODULE => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "Finder.find_module() is not implemented; define find_spec()",
        ),
        RESOURCE_LOADER_GET_DATA => exc::raise_with_message(
            OS_ERROR,
            "ResourceLoader.get_data() is not implemented by this loader",
        ),
        INSPECT_LOADER_IS_PACKAGE => import_error("InspectLoader.is_package()"),
        INSPECT_LOADER_GET_SOURCE => import_error("InspectLoader.get_source()"),
        INSPECT_LOADER_GET_CODE => inspect_get_code(args),
        INSPECT_LOADER_SOURCE_TO_CODE => source_to_code(args),
        EXECUTION_LOADER_GET_FILENAME => import_error("ExecutionLoader.get_filename()"),
        EXECUTION_LOADER_GET_CODE => execution_get_code(args),
        RESOURCE_READER_OPEN_RESOURCE
        | RESOURCE_READER_RESOURCE_PATH
        | RESOURCE_READER_IS_RESOURCE
        | RESOURCE_READER_CONTENTS
        | TRAVERSABLE_RESOURCES_FILES => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "the resource reader interface is abstract; implement it in the reader",
        ),
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
        // The external path finders and loaders. `PathFinder`/`FileFinder`
        // are real finders over Hydro's compiled-module table and the
        // filesystem; `SourcelessFileLoader` and `AppleFrameworkLoader`
        // complete the loader classes CPython's frozen module exports.
        "PathFinder" => path_finder_class(),
        "FileFinder" => file_finder_class(),
        "SourcelessFileLoader" => sourceless_loader_class(),
        "AppleFrameworkLoader" => apple_framework_loader_class(),
        // `NamespaceLoader`: the loader of PEP 420 namespace packages.
        "NamespaceLoader" => namespace_loader_class(),
        // The abstract file-backed loader bases `importlib.abc` mixes in.
        "FileLoader" => file_loader_class(),
        "SourceLoader" => source_loader_base_class(),
        "_LoaderBasics" => loader_basics_class(),
        // `WindowsRegistryFinder` exists on every platform in CPython's
        // `importlib.machinery`; on POSIX its `find_spec` finds nothing.
        "WindowsRegistryFinder" => windows_registry_finder_class(),
        // `zipimport.zipimporter` and its error type. The `zipimport` alias
        // resolves these through this table.
        "zipimporter" => zipimporter_class(),
        "ZipImportError" => zip_import_error_class(),
        // `zipimport._zip_directory_cache`: Hydro imports no zip archives, so
        // the mapping is a real, permanently empty dict (`pkgutil` reads it
        // only for an existing `zipimporter`, which Hydro never creates).
        "_zip_directory_cache" => zip_directory_cache(),
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
    let bases = if base == NONE { Vec::new() } else { vec![base] };
    cached_class_multi(key, name, bases, methods)
}

/// [`cached_class`] with an explicit base list, so the abstract classes mirror
/// CPython's multiple inheritance (`FileLoader(FileLoader, ResourceLoader,
/// ExecutionLoader)`) and `issubclass()` checks follow the real C3 MRO.
fn cached_class_multi(
    key: &str,
    name: &str,
    bases: Vec<i64>,
    methods: Vec<(String, i64, MethodKind)>,
) -> i64 {
    cached_class_multi_attrs(key, name, bases, methods, Vec::new())
}

/// [`cached_class_multi`] with class-level attributes (CPython's `_ORIGIN`
/// marker on the built-in and frozen importers).
fn cached_class_multi_attrs(
    key: &str,
    name: &str,
    bases: Vec<i64>,
    methods: Vec<(String, i64, MethodKind)>,
    attrs: Vec<(String, i64)>,
) -> i64 {
    if let Some(class) = globals::registry_get(key) {
        return class;
    }
    let method_values: Vec<i64> = methods.iter().map(|(_, value, _)| *value).collect();
    let mut rooted = method_values.clone();
    rooted.extend_from_slice(&bases);
    rooted.extend(attrs.iter().map(|(_, value)| *value));
    let roots = crate::heap::root_values(&rooted);
    let class = value::class_new_multi(name.to_string(), bases, NONE, methods, attrs);
    let class_roots = crate::heap::root_values(&[class]);
    globals::registry_set(key, class);
    crate::heap::release_roots(class_roots);
    crate::heap::release_roots(roots);
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

/// `importlib.abc.Loader` (exposed here so `importlib.abc` can share the
/// value): `create_module()` defers to default module creation and
/// `load_module()` reports the module the runtime compiled ahead of time.
///
/// CPython deliberately omits `exec_module()` from this class so
/// `hasattr(loader, 'exec_module')` stays a capability probe.
pub(crate) fn loader_class() -> i64 {
    cached_class(
        "importlib.abc.Loader",
        "Loader",
        NONE,
        vec![
            (
                "create_module".to_string(),
                native(LOADER_CREATE_MODULE, 2),
                MethodKind::Normal,
            ),
            (
                "load_module".to_string(),
                native(LOADER_LOAD_MODULE, 2),
                MethodKind::Normal,
            ),
        ],
    )
}

/// `importlib.abc.MetaPathFinder`: an entry on `sys.meta_path`.
///
/// `find_spec()` is absent on purpose (CPython omits it for the same
/// `hasattr` reason); `invalidate_caches()` is the optional no-op hook.
pub(crate) fn meta_path_finder_class() -> i64 {
    cached_class(
        "importlib.abc.MetaPathFinder",
        "MetaPathFinder",
        NONE,
        vec![(
            "invalidate_caches".to_string(),
            native(META_PATH_FINDER_INVALIDATE, 1),
            MethodKind::Normal,
        )],
    )
}

/// `importlib.abc.PathEntryFinder`: a finder for one `sys.path` entry.
pub(crate) fn path_entry_finder_class() -> i64 {
    cached_class(
        "importlib.abc.PathEntryFinder",
        "PathEntryFinder",
        NONE,
        vec![(
            "invalidate_caches".to_string(),
            native(PATH_ENTRY_FINDER_INVALIDATE, 1),
            MethodKind::Normal,
        )],
    )
}

/// `importlib.abc.Finder`: the pre-PEP 451 finder interface. Hydro's finders
/// implement `find_spec()`, so `find_module()` only reports
/// `NotImplementedError`, exactly like CPython's modern ABC.
pub(crate) fn finder_class() -> i64 {
    cached_class(
        "importlib.abc.Finder",
        "Finder",
        NONE,
        vec![(
            "find_module".to_string(),
            native(FINDER_FIND_MODULE, 3),
            MethodKind::Normal,
        )],
    )
}

/// `importlib.abc.ResourceLoader(Loader)`.
pub(crate) fn resource_loader_class() -> i64 {
    cached_class(
        "importlib.abc.ResourceLoader",
        "ResourceLoader",
        loader_class(),
        vec![(
            "get_data".to_string(),
            native(RESOURCE_LOADER_GET_DATA, 2),
            MethodKind::Normal,
        )],
    )
}

/// `importlib.abc.InspectLoader(Loader)`.
pub(crate) fn inspect_loader_class() -> i64 {
    cached_class(
        "importlib.abc.InspectLoader",
        "InspectLoader",
        loader_class(),
        vec![
            (
                "is_package".to_string(),
                native(INSPECT_LOADER_IS_PACKAGE, 2),
                MethodKind::Normal,
            ),
            (
                "get_source".to_string(),
                native(INSPECT_LOADER_GET_SOURCE, 2),
                MethodKind::Normal,
            ),
            (
                "get_code".to_string(),
                native(INSPECT_LOADER_GET_CODE, 2),
                MethodKind::Normal,
            ),
            (
                "source_to_code".to_string(),
                native(INSPECT_LOADER_SOURCE_TO_CODE, 2),
                MethodKind::Staticmethod,
            ),
            // `_bootstrap_external._LoaderBasics` supplies this on the ABCs
            // that mix it in; the runtime value is the shared
            // "already executed" implementation.
            (
                "exec_module".to_string(),
                native(SOURCE_EXEC_MODULE, 2),
                MethodKind::Normal,
            ),
        ],
    )
}

/// `importlib.abc.ExecutionLoader(InspectLoader)`.
pub(crate) fn execution_loader_class() -> i64 {
    cached_class(
        "importlib.abc.ExecutionLoader",
        "ExecutionLoader",
        inspect_loader_class(),
        vec![
            (
                "get_filename".to_string(),
                native(EXECUTION_LOADER_GET_FILENAME, 2),
                MethodKind::Normal,
            ),
            (
                "get_code".to_string(),
                native(EXECUTION_LOADER_GET_CODE, 2),
                MethodKind::Normal,
            ),
        ],
    )
}

/// `_bootstrap_external._LoaderBasics`: the concrete defaults shared by
/// `SourceLoader` and `SourcelessFileLoader`.
pub(crate) fn loader_basics_class() -> i64 {
    cached_class(
        "importlib.machinery._LoaderBasics",
        "_LoaderBasics",
        NONE,
        vec![
            (
                "is_package".to_string(),
                native(LOADER_BASICS_IS_PACKAGE, 2),
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
            (
                "load_module".to_string(),
                native(LOADER_LOAD_MODULE, 2),
                MethodKind::Normal,
            ),
        ],
    )
}

/// `_bootstrap_external.FileLoader`: the file-backed loader base
/// (`__init__`, `get_filename`, `get_data`, `get_resource_reader`).
pub(crate) fn file_loader_class() -> i64 {
    cached_class(
        "importlib.machinery.FileLoader",
        "FileLoader",
        NONE,
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
                "get_data".to_string(),
                native(FILE_LOADER_GET_DATA, 2),
                MethodKind::Normal,
            ),
            (
                "is_package".to_string(),
                native(LOADER_BASICS_IS_PACKAGE, 2),
                MethodKind::Normal,
            ),
        ],
    )
}

/// `_bootstrap_external.SourceLoader`: the abstract source/bytecode loader.
///
/// `get_source()` reads and decodes the file for real; `get_code()` reports
/// the missing runtime compiler rather than inventing a code object.
pub(crate) fn source_loader_base_class() -> i64 {
    cached_class(
        "importlib.machinery.SourceLoader",
        "SourceLoader",
        loader_basics_class(),
        vec![
            (
                "get_source".to_string(),
                native(SOURCE_GET_SOURCE, 2),
                MethodKind::Normal,
            ),
            (
                "get_code".to_string(),
                native(SOURCE_GET_CODE, 2),
                MethodKind::Normal,
            ),
            (
                "source_to_code".to_string(),
                native(SOURCE_SOURCE_TO_CODE, 4),
                MethodKind::Normal,
            ),
            (
                "path_stats".to_string(),
                native(SOURCE_PATH_STATS, 2),
                MethodKind::Normal,
            ),
            (
                "path_mtime".to_string(),
                native(SOURCE_PATH_MTIME, 2),
                MethodKind::Normal,
            ),
            (
                "set_data".to_string(),
                native(SOURCE_SET_DATA, 4),
                MethodKind::Normal,
            ),
        ],
    )
}

/// `importlib.abc.FileLoader(_bootstrap_external.FileLoader, ResourceLoader,
/// ExecutionLoader)`: the file loader with the documented ABC surface.
pub(crate) fn abc_file_loader_class() -> i64 {
    cached_class_multi(
        "importlib.abc.FileLoader",
        "FileLoader",
        vec![
            file_loader_class(),
            resource_loader_class(),
            execution_loader_class(),
        ],
        Vec::new(),
    )
}

/// `importlib.abc.SourceLoader(_bootstrap_external.SourceLoader,
/// ResourceLoader, ExecutionLoader)`.
pub(crate) fn abc_source_loader_class() -> i64 {
    cached_class_multi(
        "importlib.abc.SourceLoader",
        "SourceLoader",
        vec![
            source_loader_base_class(),
            resource_loader_class(),
            execution_loader_class(),
        ],
        Vec::new(),
    )
}

/// `importlib.abc.ResourceReader`: the legacy resource-reader ABC (kept under
/// `importlib.resources.abc` in CPython 3.14; the same class value is exposed
/// under the historical `importlib.abc` spelling).
pub(crate) fn resource_reader_class() -> i64 {
    cached_class(
        "importlib.abc.ResourceReader",
        "ResourceReader",
        NONE,
        vec![
            (
                "open_resource".to_string(),
                native(RESOURCE_READER_OPEN_RESOURCE, 2),
                MethodKind::Normal,
            ),
            (
                "resource_path".to_string(),
                native(RESOURCE_READER_RESOURCE_PATH, 2),
                MethodKind::Normal,
            ),
            (
                "is_resource".to_string(),
                native(RESOURCE_READER_IS_RESOURCE, 2),
                MethodKind::Normal,
            ),
            (
                "contents".to_string(),
                native(RESOURCE_READER_CONTENTS, 1),
                MethodKind::Normal,
            ),
        ],
    )
}

/// `importlib.abc.TraversableResources(ResourceReader)`.
pub(crate) fn traversable_resources_class() -> i64 {
    cached_class(
        "importlib.abc.TraversableResources",
        "TraversableResources",
        resource_reader_class(),
        vec![(
            "files".to_string(),
            native(TRAVERSABLE_RESOURCES_FILES, 1),
            MethodKind::Normal,
        )],
    )
}

/// `importlib.machinery.SourceFileLoader(name, path)`.
///
/// CPython's class is `SourceFileLoader(FileLoader, SourceLoader)`; the ABC
/// value is used as the base so `issubclass(SourceFileLoader,
/// importlib.abc.FileLoader)` holds with the real C3 linearization.
pub(crate) fn source_loader_class() -> i64 {
    cached_class_multi(
        "importlib.machinery.SourceFileLoader",
        "SourceFileLoader",
        vec![abc_file_loader_class(), abc_source_loader_class()],
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
        abc_file_loader_class(),
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
            (
                "is_package".to_string(),
                native(EXTENSION_IS_PACKAGE, 2),
                MethodKind::Normal,
            ),
            (
                "get_code".to_string(),
                native(NAMESPACE_LOADER_GET_CODE, 2),
                MethodKind::Normal,
            ),
            (
                "get_source".to_string(),
                native(NAMESPACE_LOADER_GET_SOURCE, 2),
                MethodKind::Normal,
            ),
        ],
    )
}

/// `importlib.machinery.BuiltinImporter`: classmethods for native modules.
pub(crate) fn builtin_importer_class() -> i64 {
    cached_class_multi_attrs(
        "importlib.machinery.BuiltinImporter",
        "BuiltinImporter",
        vec![meta_path_finder_class(), inspect_loader_class()],
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
        // `spec_from_loader` reads `_ORIGIN`, exactly like CPython.
        vec![("_ORIGIN".to_string(), value::str_alloc("built-in".to_string()))],
    )
}

/// `importlib.machinery.FrozenImporter`: Hydro ships no frozen modules, so
/// discovery returns `None` and `create_module` reports the exact reason.
pub(crate) fn frozen_importer_class() -> i64 {
    cached_class_multi_attrs(
        "importlib.machinery.FrozenImporter",
        "FrozenImporter",
        vec![meta_path_finder_class(), inspect_loader_class()],
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
        vec![("_ORIGIN".to_string(), value::str_alloc("frozen".to_string()))],
    )
}

/// `importlib.machinery.PathFinder`: the `sys.meta_path` finder for path
/// entries. Hydro resolves installed files and compiled modules through
/// [`importlib_util`](super::importlib_util), so the finder reports those
/// specs instead of maintaining a separate path-importer cache.
pub(crate) fn path_finder_class() -> i64 {
    cached_class(
        "importlib.machinery.PathFinder",
        "PathFinder",
        meta_path_finder_class(),
        vec![
            (
                "find_spec".to_string(),
                native(PATH_FINDER_FIND_SPEC, 4),
                MethodKind::Classmethod,
            ),
            (
                "invalidate_caches".to_string(),
                native(PATH_FINDER_INVALIDATE, 1),
                MethodKind::Classmethod,
            ),
        ],
    )
}

/// `importlib.machinery.FileFinder`: a directory finder.
///
/// The instance stores its directory in `path`; `find_spec` searches that
/// directory with [`importlib_util`](super::importlib_util)'s filesystem
/// discovery. `path_hook` returns the hook function CPython installs into
/// `sys.path_hooks`; Hydro has no runtime `sys.path_hooks` list (imports are
/// resolved ahead of time), so the hook is returned for compatibility and
/// works when called directly.
pub(crate) fn file_finder_class() -> i64 {
    cached_class(
        "importlib.machinery.FileFinder",
        "FileFinder",
        path_entry_finder_class(),
        vec![
            (
                "__init__".to_string(),
                native(FILE_FINDER_INIT, usize::MAX),
                MethodKind::Normal,
            ),
            (
                "path_hook".to_string(),
                native(FILE_FINDER_PATH_HOOK, usize::MAX),
                MethodKind::Classmethod,
            ),
            (
                "find_spec".to_string(),
                native(FILE_FINDER_FIND_SPEC, 3),
                MethodKind::Normal,
            ),
            (
                "invalidate_caches".to_string(),
                native(FILE_FINDER_INVALIDATE, 1),
                MethodKind::Classmethod,
            ),
        ],
    )
}

/// `importlib.machinery.SourcelessFileLoader(name, path)`: the loader for a
/// `.pyc` file that has no source.
///
/// Hydro cannot execute bytecode at runtime; a module compiled into the
/// program is already initialized, so `exec_module` accepts that module and
/// reports a precise `ImportError` for any other file.
pub(crate) fn sourceless_loader_class() -> i64 {
    cached_class(
        "importlib.machinery.SourcelessFileLoader",
        "SourcelessFileLoader",
        abc_file_loader_class(),
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
            (
                "get_source".to_string(),
                native(NAMESPACE_LOADER_GET_SOURCE, 2),
                MethodKind::Normal,
            ),
            (
                "get_code".to_string(),
                native(SOURCE_GET_CODE, 2),
                MethodKind::Normal,
            ),
        ],
    )
}

/// `importlib.machinery.AppleFrameworkLoader`: on every non-Apple target this
/// is the extension loader under its Apple-framework spelling, matching
/// CPython's Linux surface.
pub(crate) fn apple_framework_loader_class() -> i64 {
    cached_class(
        "importlib.machinery.AppleFrameworkLoader",
        "AppleFrameworkLoader",
        extension_loader_class(),
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

/// `importlib.machinery.NamespaceLoader`: the PEP 420 loader.
pub(crate) fn namespace_loader_class() -> i64 {
    cached_class(
        "importlib.machinery.NamespaceLoader",
        "NamespaceLoader",
        inspect_loader_class(),
        vec![
            (
                "create_module".to_string(),
                native(NAMESPACE_LOADER_CREATE_MODULE, 2),
                MethodKind::Normal,
            ),
            (
                "exec_module".to_string(),
                native(NAMESPACE_LOADER_EXEC_MODULE, 2),
                MethodKind::Normal,
            ),
            (
                "is_package".to_string(),
                native(NAMESPACE_LOADER_IS_PACKAGE, 2),
                MethodKind::Normal,
            ),
            (
                "get_code".to_string(),
                native(NAMESPACE_LOADER_GET_CODE, 2),
                MethodKind::Normal,
            ),
            (
                "get_source".to_string(),
                native(NAMESPACE_LOADER_GET_SOURCE, 2),
                MethodKind::Normal,
            ),
        ],
    )
}

/// `importlib.machinery.WindowsRegistryFinder`: present on every platform;
/// on POSIX its `find_spec()` finds nothing, exactly like CPython's.
pub(crate) fn windows_registry_finder_class() -> i64 {
    cached_class(
        "importlib.machinery.WindowsRegistryFinder",
        "WindowsRegistryFinder",
        meta_path_finder_class(),
        vec![(
            "find_spec".to_string(),
            native(WINDOWS_REGISTRY_FIND_SPEC, 4),
            MethodKind::Classmethod,
        )],
    )
}

/// `zipimport.ZipImportError`, an `ImportError` subclass.
pub(crate) fn zip_import_error_class() -> i64 {
    cached_class(
        "zipimport.ZipImportError",
        "ZipImportError",
        exc::exception_class(IMPORT_ERROR),
        Vec::new(),
    )
}

/// `zipimport.zipimporter(archivepath)`.
///
/// The implementation reads the archive's central directory for real, so
/// `_get_files()`, `find_spec()`, `is_package()`, `get_filename()` and
/// `get_source()` report the archive's actual contents. Executing a module
/// from the archive still needs a runtime compiler, which Hydro does not have;
/// `get_code()`/`load_module()` report that instead of inventing a module.
pub(crate) fn zipimporter_class() -> i64 {
    cached_class(
        "zipimport.zipimporter",
        "zipimporter",
        inspect_loader_class(),
        vec![
            (
                "__init__".to_string(),
                native(ZIPIMPORTER_INIT, usize::MAX),
                MethodKind::Normal,
            ),
            (
                "find_spec".to_string(),
                native(ZIPIMPORTER_FIND_SPEC, 3),
                MethodKind::Normal,
            ),
            (
                "is_package".to_string(),
                native(ZIPIMPORTER_IS_PACKAGE, 2),
                MethodKind::Normal,
            ),
            (
                "get_filename".to_string(),
                native(ZIPIMPORTER_GET_FILENAME, 2),
                MethodKind::Normal,
            ),
            (
                "get_source".to_string(),
                native(ZIPIMPORTER_GET_SOURCE, 2),
                MethodKind::Normal,
            ),
            (
                "get_code".to_string(),
                native(ZIPIMPORTER_GET_CODE, 2),
                MethodKind::Normal,
            ),
            (
                "get_data".to_string(),
                native(ZIPIMPORTER_GET_DATA, 2),
                MethodKind::Normal,
            ),
            (
                "load_module".to_string(),
                native(ZIPIMPORTER_LOAD_MODULE, 2),
                MethodKind::Normal,
            ),
            (
                "exec_module".to_string(),
                native(SOURCE_EXEC_MODULE, 2),
                MethodKind::Normal,
            ),
            (
                "invalidate_caches".to_string(),
                native(ZIPIMPORTER_INVALIDATE, 1),
                MethodKind::Normal,
            ),
            (
                "_get_files".to_string(),
                native(ZIPIMPORTER_GET_FILES, 1),
                MethodKind::Normal,
            ),
        ],
    )
}

/// Materialises an `importlib.abc` module constant. The classes live in this
/// module so the concrete finders and loaders share their values, which keeps
/// `issubclass()` checks true across both spellings.
pub(crate) fn abc_constant(name: &str) -> i64 {
    match name {
        "Loader" => loader_class(),
        "MetaPathFinder" => meta_path_finder_class(),
        "PathEntryFinder" => path_entry_finder_class(),
        "Finder" => finder_class(),
        "ResourceLoader" => resource_loader_class(),
        "InspectLoader" => inspect_loader_class(),
        "ExecutionLoader" => execution_loader_class(),
        "FileLoader" => abc_file_loader_class(),
        "SourceLoader" => abc_source_loader_class(),
        "ResourceReader" => resource_reader_class(),
        "TraversableResources" => traversable_resources_class(),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'importlib.abc' has no attribute '{name}'"),
        ),
    }
}

// -------------------------------------------------------------------------
// Path finders
// -------------------------------------------------------------------------

/// `_install(_bootstrap_module)`: CPython's hook that registers the
/// path-based import components.
///
/// Hydro's importer is compiled in: native modules, modules compiled into the
/// program and files under `sys.path` are discovered by
/// [`importlib_util`](super::importlib_util) without a mutable
/// `sys.path_hooks` list. The truthful equivalent of the CPython call is
/// therefore to record the bootstrap module and ensure the real
/// [`PathFinder`] is on `sys.meta_path`; no file finder is installed because
/// there is no runtime path-importer cache to populate. Repeated calls are
/// idempotent.
fn install(args: &[i64]) -> i64 {
    let Some(bootstrap) = args.first().copied() else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "_install() missing 1 required positional argument: '_bootstrap_module'",
        );
    };
    globals::registry_set(BOOTSTRAP_KEY, bootstrap);
    let meta_path = super::module_get_by_name("sys", "meta_path");
    if exc::pending() {
        return NONE;
    }
    let finder = path_finder_class();
    let Some(items) = value::as_list(meta_path) else {
        return NONE;
    };
    if items.contains(&finder) {
        return NONE;
    }
    let roots = crate::heap::root_values(&[meta_path, finder]);
    crate::container::list_append(meta_path, finder);
    crate::heap::release_roots(roots);
    NONE
}

/// The `path` argument of `PathFinder.find_spec` as directories, or `None`
/// for `sys.path`.
fn finder_path(argument: i64) -> Option<Vec<String>> {
    if argument == NONE {
        return None;
    }
    let items = value::as_list(argument)?;
    Some(
        items
            .iter()
            .filter_map(|item| value::as_str(*item).map(str::to_string))
            .collect(),
    )
}

/// `PathFinder.find_spec(fullname, path=None, target=None)`, with the class
/// receiver in `args[0]`.
fn path_finder_find_spec(args: &[i64]) -> i64 {
    let Some(name) = args.get(1).and_then(|arg| value::as_str(*arg)) else {
        return exc::raise_with_message(TYPE_ERROR, "find_spec() argument 'name' must be str");
    };
    if name.is_empty() || name.split('.').any(str::is_empty) {
        return NONE;
    }
    if hydro_ir::stdlib::module(name).is_some() {
        return super::importlib_util::native_spec(name);
    }
    let path = finder_path(args.get(2).copied().unwrap_or(NONE));
    match super::importlib_util::resolve_module_with_path(name, path.as_deref()) {
        Some(found) => super::importlib_util::spec_instance(&found),
        None => NONE,
    }
}

/// `FileFinder(path, *loader_details)`.
fn file_finder_init(args: &[i64]) -> i64 {
    let receiver = args[0];
    let Some(path) = args.get(1).and_then(|arg| value::as_str(*arg)) else {
        return exc::raise_with_message(TYPE_ERROR, "file_finder path must be a str, not 'int'");
    };
    value::instance_setattr(
        receiver,
        "path".to_string(),
        value::str_alloc(path.to_string()),
    );
    NONE
}

/// `FileFinder.path_hook(*loader_details)`: the hook CPython appends to
/// `sys.path_hooks`. The loader details only choose which loaders a finder
/// knows; Hydro's discovery picks the loader from the file suffix, so the
/// returned hook accepts the directory alone.
fn file_finder_path_hook(_args: &[i64]) -> i64 {
    native(FILE_FINDER_HOOK, 1)
}

/// The function returned by `FileFinder.path_hook`: `hook(path)`.
fn file_finder_hook(args: &[i64]) -> i64 {
    let Some(path) = args.first().and_then(|arg| value::as_str(*arg)) else {
        return exc::raise_with_message(TYPE_ERROR, "path hook argument must be str, not 'int'");
    };
    if !Path::new(path).is_dir() {
        return exc::raise_with_message(IMPORT_ERROR, "only directories are supported");
    }
    let finder = value::instance_new(file_finder_class());
    let roots = crate::heap::root_values(&[finder]);
    value::instance_setattr(
        finder,
        "path".to_string(),
        value::str_alloc(path.to_string()),
    );
    crate::heap::release_roots(roots);
    finder
}

/// `FileFinder.find_spec(fullname, target=None)`, with the instance receiver
/// in `args[0]`.
fn file_finder_find_spec(args: &[i64]) -> i64 {
    let Some(name) = args.get(1).and_then(|arg| value::as_str(*arg)) else {
        return exc::raise_with_message(TYPE_ERROR, "find_spec() argument 'name' must be str");
    };
    let Some(path) = loader_field(args[0], "path") else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "find_spec() receiver has no str 'path' attribute",
        );
    };
    match super::importlib_util::filesystem_spec(name, &[path]) {
        Some(found) => super::importlib_util::spec_instance(&found),
        None => NONE,
    }
}

/// `zipimport._zip_directory_cache`: archive path -> `{member: info}` mapping.
///
/// The mapping is a real cached dict so identity and mutation behave like
/// CPython's module global: `zipimporter()` fills it with the archive's
/// central directory, `invalidate_caches()` drops one archive's entry and
/// `pkgutil` reads the member names.
fn zip_directory_cache() -> i64 {
    if let Some(cached) = globals::registry_get(ZIP_DIRECTORY_CACHE) {
        return cached;
    }
    let cache = value::dict_new(Vec::new());
    let roots = crate::heap::root_values(&[cache]);
    globals::registry_set(ZIP_DIRECTORY_CACHE, cache);
    crate::heap::release_roots(roots);
    cache
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

/// `spec.submodule_search_locations` as the live list value, or `None` when
/// the spec is not a package.
///
/// `module_from_spec` binds this exact object as the module's `__path__`, so
/// `module.__path__ is spec.submodule_search_locations` holds and extending
/// one is visible through the other, exactly like CPython.
pub(crate) fn spec_search_locations_value(spec: i64) -> Option<i64> {
    let raw = value::instance_attr(spec, "submodule_search_locations")?;
    value::as_list(raw)?;
    Some(raw)
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

/// `NamespaceLoader.exec_module(module)`: a PEP 420 namespace package has no
/// body, so the loader does nothing (CPython's implementation is `pass`).
fn namespace_exec_module() -> i64 {
    NONE
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

// -------------------------------------------------------------------------
// Shared loader method implementations
// -------------------------------------------------------------------------

/// `getattr(receiver, name)`, clearing a missing attribute to `NONE`.
fn lookup_method(receiver: i64, name: &str) -> i64 {
    let found = crate::core::methods::getattr(receiver, name.as_ptr(), name.len());
    if exc::pending() {
        if exc::pending_tag() == i64::from(ATTRIBUTE_ERROR) {
            exc::clear();
        }
        return NONE;
    }
    found
}

/// Calls `receiver.name(*args)`.
fn call_method(receiver: i64, name: &str, args: &[i64]) -> i64 {
    let function = lookup_method(receiver, name);
    if exc::pending() {
        return NONE;
    }
    if function == NONE {
        return exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!(
                "'{}' object has no attribute '{name}'",
                value::type_name(receiver)
            ),
        );
    }
    crate::builtins::call(function, args.len(), args.as_ptr())
}

fn import_error(method: &str) -> i64 {
    exc::raise_with_message(
        IMPORT_ERROR,
        &format!("{method} is not implemented by this loader"),
    )
}

/// `Loader.load_module(fullname)`: the module body already ran, so the
/// registry entry is the result; anything else needs a runtime compiler.
fn loader_load_module(args: &[i64]) -> i64 {
    let receiver = args[0];
    let Some(fullname) = value::as_str(args[1]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "load_module() argument 'fullname' must be str, not '{}'",
                value::type_name(args[1])
            ),
        );
    };
    if let Some(module) = super::registry_lookup(fullname) {
        return module;
    }
    if lookup_method(receiver, "exec_module") == NONE {
        return exc::raise_with_message(
            IMPORT_ERROR,
            &format!("cannot load module {fullname:?}: loader defines no exec_module()"),
        );
    }
    exc::raise_with_message(
        IMPORT_ERROR,
        &format!(
            "cannot load module {fullname:?}: Hydro executes module bodies ahead of time \
             and has no runtime Python compiler"
        ),
    )
}

/// `self.get_filename(fullname)` as an owned path.
fn method_filename(loader: i64, fullname: i64) -> Result<String, i64> {
    let found = call_method(loader, "get_filename", &[fullname]);
    if exc::pending() {
        return Err(found);
    }
    match value::as_str(found) {
        Some(text) => Ok(text.to_string()),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "get_filename() must return str, not '{}'",
                value::type_name(found)
            ),
        )),
    }
}

/// `_LoaderBasics.is_package(fullname)`: the filename base is `__init__`.
fn loader_basics_is_package(args: &[i64]) -> i64 {
    let loader = args[0];
    let Some(fullname) = value::as_str(args[1]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "is_package() argument 'fullname' must be str, not '{}'",
                value::type_name(args[1])
            ),
        );
    };
    let filename = match method_filename(loader, args[1]) {
        Ok(filename) => filename,
        Err(raised) => return raised,
    };
    let base = Path::new(&filename)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name))
        .unwrap_or("");
    let tail = fullname.rsplit('.').next().unwrap_or(fullname);
    value::bool_value(base == "__init__" && tail != "__init__")
}

/// `SourceLoader.get_source(fullname)`: the decoded source file.
fn source_get_source(args: &[i64]) -> i64 {
    let loader = args[0];
    let path = match method_filename(loader, args[1]) {
        Ok(path) => path,
        Err(raised) => return raised,
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => {
            return exc::raise_with_message(
                IMPORT_ERROR,
                "source not available through get_data()",
            )
        }
    };
    let source = value::bytes_new(bytes);
    super::importlib_util::call("decode_source", &[source])
}

/// `SourceLoader.get_code(fullname)`: reading succeeds, compiling does not.
fn source_get_code(args: &[i64]) -> i64 {
    let loader = args[0];
    let path = match method_filename(loader, args[1]) {
        Ok(path) => path,
        Err(raised) => return raised,
    };
    if let Err(error) = std::fs::read(&path) {
        return exc::raise_with_message(
            IMPORT_ERROR,
            &format!("source not available through get_data(): {error}"),
        );
    }
    exc::raise_with_message(
        NOT_IMPLEMENTED_ERROR,
        &format!(
            "get_code() cannot produce a code object for '{path}': Hydro consumes \
             Python source ahead of time and has no runtime bytecode compiler"
        ),
    )
}

/// `SourceLoader.source_to_code(data, path='<string>')`.
fn source_to_code(args: &[i64]) -> i64 {
    let path = args.get(1).copied().and_then(value::as_str).unwrap_or("<string>");
    exc::raise_with_message(
        NOT_IMPLEMENTED_ERROR,
        &format!(
            "source_to_code() cannot compile Python source at runtime in Hydro \
             (path: '{path}'): module bodies are compiled ahead of time"
        ),
    )
}

/// `InspectLoader.get_code(fullname)`: `get_source()` then `source_to_code()`.
fn inspect_get_code(args: &[i64]) -> i64 {
    let receiver = args[0];
    let fullname = args[1];
    let source = call_method(receiver, "get_source", &[fullname]);
    if exc::pending() {
        return NONE;
    }
    if source == NONE {
        return NONE;
    }
    call_method(receiver, "source_to_code", &[source])
}

/// `ExecutionLoader.get_code(fullname)`: pass the filename when available.
fn execution_get_code(args: &[i64]) -> i64 {
    let receiver = args[0];
    let fullname = args[1];
    let source = call_method(receiver, "get_source", &[fullname]);
    if exc::pending() {
        return NONE;
    }
    if source == NONE {
        return NONE;
    }
    let path = call_method(receiver, "get_filename", &[fullname]);
    if exc::pending() {
        if exc::pending_tag() == i64::from(IMPORT_ERROR) {
            exc::clear();
            return call_method(receiver, "source_to_code", &[source]);
        }
        return NONE;
    }
    call_method(receiver, "source_to_code", &[source, path])
}

fn stat_error(path: &str, error: &std::io::Error) -> i64 {
    exc::raise_with_message(OS_ERROR, &format!("{error}: '{path}'"))
}

/// `SourceLoader.path_stats(path)`.
fn source_path_stats(args: &[i64]) -> i64 {
    let Some(path) = args.get(1).copied().and_then(value::as_str) else {
        return exc::raise_with_message(TYPE_ERROR, "path_stats() argument must be str");
    };
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let mtime = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_secs_f64())
                .unwrap_or(0.0);
            let key_mtime = value::str_alloc("mtime".to_string());
            let key_size = value::str_alloc("size".to_string());
            let roots = crate::heap::root_values(&[key_mtime, key_size]);
            let dict = value::dict_new(vec![
                (key_mtime, value::float_new(mtime)),
                (key_size, crate::ops::int_result(metadata.len() as i64)),
            ]);
            crate::heap::release_roots(roots);
            dict
        }
        Err(error) => stat_error(path, &error),
    }
}

/// `SourceLoader.path_mtime(path)`: the integer modification time.
fn source_path_mtime(args: &[i64]) -> i64 {
    let Some(path) = args.get(1).copied().and_then(value::as_str) else {
        return exc::raise_with_message(TYPE_ERROR, "path_mtime() argument must be str");
    };
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let seconds = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0);
            crate::ops::int_result(seconds as i64)
        }
        Err(error) => stat_error(path, &error),
    }
}

/// `SourceLoader.set_data(path, data, *, _mode=0o666)`: write bytecode.
///
/// CPython silently ignores write failures (a read-only installation simply
/// does not cache bytecode); this keeps that behaviour.
fn source_set_data(args: &[i64]) -> i64 {
    let Some(path) = args.get(1).copied().and_then(value::as_str) else {
        return exc::raise_with_message(TYPE_ERROR, "set_data() argument 'path' must be str");
    };
    let Some(data) = args.get(2).copied().and_then(value::as_bytes) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "set_data() argument 'data' must be bytes, not '{}'",
                value::type_name(args.get(2).copied().unwrap_or(NONE))
            ),
        );
    };
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    let _ = std::fs::write(path, data);
    NONE
}

/// `FileLoader.get_data(path)`.
fn file_loader_get_data(args: &[i64]) -> i64 {
    let Some(path) = args.get(1).copied().and_then(value::as_str) else {
        return exc::raise_with_message(TYPE_ERROR, "get_data() argument must be str");
    };
    match std::fs::read(path) {
        Ok(bytes) => value::bytes_new(bytes),
        Err(error) => stat_error(path, &error),
    }
}

/// `ExtensionFileLoader.is_package(fullname)`.
fn extension_is_package(args: &[i64]) -> i64 {
    let Some(path) = value::instance_attr(args[0], "path").and_then(value::as_str) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "is_package() receiver has no str 'path' attribute",
        );
    };
    let Some(filename) = Path::new(path).file_name().and_then(|name| name.to_str()) else {
        return value::bool_value(false);
    };
    let suffixes = constant("EXTENSION_SUFFIXES");
    let matches = value::as_list(suffixes).is_some_and(|suffixes| {
        suffixes.iter().any(|suffix| {
            value::as_str(*suffix)
                .is_some_and(|suffix| filename == format!("__init__{suffix}"))
        })
    });
    if exc::pending() {
        return NONE;
    }
    value::bool_value(matches)
}

// -------------------------------------------------------------------------
// `zipimport.zipimporter`
// -------------------------------------------------------------------------

/// One central-directory entry.
#[derive(Clone, Debug)]
struct ZipEntry {
    name: String,
    compress_type: u16,
    crc: u32,
    compress_size: u64,
    file_size: u64,
    header_offset: u64,
    time: u16,
    date: u16,
}

fn u16_at(data: &[u8], offset: usize) -> Option<u16> {
    let slice = data.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    let slice = data.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn u64_at(data: &[u8], offset: usize) -> Option<u64> {
    let slice = data.get(offset..offset + 8)?;
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(slice);
    Some(u64::from_le_bytes(bytes))
}

/// Reads the central directory of `archive`.
///
/// Supports the ZIP32 layout and the ZIP64 end-of-central-directory record
/// (the sparse-ZIP64 fixtures in CPython's tests use it). Returning `Err`
/// means the file is not a usable zip archive, which `zipimporter()` reports
/// as `ZipImportError`.
fn read_central_directory(archive: &str) -> Result<Vec<ZipEntry>, String> {
    let mut file = std::fs::File::open(archive).map_err(|error| format!("{error}: {archive}"))?;
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    if length < 22 {
        return Err(format!("{archive} is not a zip file"));
    }
    let tail_len = length.min(22 + 0xFFFF);
    let mut tail = vec![0u8; tail_len as usize];
    file.seek(SeekFrom::Start(length - tail_len))
        .map_err(|error| error.to_string())?;
    file.read_exact(&mut tail)
        .map_err(|error| error.to_string())?;
    // Locate the end-of-central-directory signature, scanning backwards.
    let mut eocd = None;
    for index in (0..tail.len().saturating_sub(21)).rev() {
        if tail[index..index + 4] == [0x50, 0x4B, 0x05, 0x06] {
            eocd = Some(index);
            break;
        }
    }
    let Some(eocd) = eocd else {
        return Err(format!("{archive} is not a zip file"));
    };
    let mut count = u64::from(u16_at(&tail, eocd + 10).ok_or("truncated directory")?);
    let mut directory_size = u64::from(u32_at(&tail, eocd + 12).ok_or("truncated directory")?);
    let mut directory_offset = u64::from(u32_at(&tail, eocd + 16).ok_or("truncated directory")?);
    let eocd_file_offset = length - tail_len + eocd as u64;

    if count == 0xFFFF
        || directory_size == 0xFFFF_FFFF
        || directory_offset == 0xFFFF_FFFF
    {
        // ZIP64: the locator precedes the classic record.
        if eocd_file_offset >= 20 {
            let mut locator = [0u8; 20];
            file.seek(SeekFrom::Start(eocd_file_offset - 20))
                .map_err(|error| error.to_string())?;
            file.read_exact(&mut locator)
                .map_err(|error| error.to_string())?;
            if locator[0..4] == [0x50, 0x4B, 0x06, 0x07] {
                let zip64_offset = u64_at(&locator, 8).ok_or("truncated zip64 locator")?;
                let mut record = [0u8; 56];
                file.seek(SeekFrom::Start(zip64_offset))
                    .map_err(|error| error.to_string())?;
                file.read_exact(&mut record)
                    .map_err(|error| error.to_string())?;
                if record[0..4] != [0x50, 0x4B, 0x06, 0x06] {
                    return Err(format!("{archive} has a corrupt zip64 directory"));
                }
                count = u64_at(&record, 32).ok_or("truncated zip64 directory")?;
                directory_size = u64_at(&record, 40).ok_or("truncated zip64 directory")?;
                directory_offset = u64_at(&record, 48).ok_or("truncated zip64 directory")?;
            }
        }
    }

    let mut directory = vec![0u8; directory_size.min(length) as usize];
    file.seek(SeekFrom::Start(directory_offset))
        .map_err(|error| error.to_string())?;
    file.read_exact(&mut directory)
        .map_err(|error| error.to_string())?;

    let mut entries = Vec::new();
    let mut position = 0usize;
    for _ in 0..count {
        if directory.get(position..position + 4) != Some(&[0x50u8, 0x4B, 0x01, 0x02][..]) {
            break;
        }
        let compress_type = u16_at(&directory, position + 10).ok_or("truncated entry")?;
        let time = u16_at(&directory, position + 12).ok_or("truncated entry")?;
        let date = u16_at(&directory, position + 14).ok_or("truncated entry")?;
        let crc = u32_at(&directory, position + 16).ok_or("truncated entry")?;
        let mut compress_size = u64::from(u32_at(&directory, position + 20).ok_or("truncated entry")?);
        let mut file_size = u64::from(u32_at(&directory, position + 24).ok_or("truncated entry")?);
        let name_len = u16_at(&directory, position + 28).ok_or("truncated entry")? as usize;
        let extra_len = u16_at(&directory, position + 30).ok_or("truncated entry")? as usize;
        let comment_len = u16_at(&directory, position + 32).ok_or("truncated entry")? as usize;
        let mut header_offset =
            u64::from(u32_at(&directory, position + 42).ok_or("truncated entry")?);
        let name_start = position + 46;
        let name_end = name_start + name_len;
        let extra_end = name_end + extra_len;
        let Some(name_bytes) = directory.get(name_start..name_end) else {
            break;
        };
        // ZIP64 extra field: only the components that overflowed are present,
        // in the fixed order usize, csize, offset.
        let mut extra = directory.get(name_end..extra_end).unwrap_or(&[]);
        while extra.len() >= 4 {
            let id = u16_at(extra, 0).unwrap_or(0);
            let size = u16_at(extra, 2).unwrap_or(0) as usize;
            let Some(body) = extra.get(4..4 + size) else {
                break;
            };
            if id == 0x0001 {
                let mut cursor = 0usize;
                let mut take = |width: usize| -> Option<u64> {
                    let value = if width == 8 {
                        u64_at(body, cursor)
                    } else {
                        u32_at(body, cursor).map(u64::from)
                    };
                    cursor += width;
                    value
                };
                if file_size == 0xFFFF_FFFF {
                    file_size = take(8).unwrap_or(file_size);
                }
                if compress_size == 0xFFFF_FFFF {
                    compress_size = take(8).unwrap_or(compress_size);
                }
                if header_offset == 0xFFFF_FFFF {
                    header_offset = take(8).unwrap_or(header_offset);
                }
            }
            extra = extra.get(4 + size..).unwrap_or(&[]);
        }
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        entries.push(ZipEntry {
            name,
            compress_type,
            crc,
            compress_size,
            file_size,
            header_offset,
            time,
            date,
        });
        position = extra_end + comment_len;
    }
    Ok(entries)
}

/// Reads one member's uncompressed bytes, verifying its CRC.
fn read_zip_member(archive: &str, entry: &ZipEntry) -> Result<Vec<u8>, String> {
    let mut file = std::fs::File::open(archive).map_err(|error| format!("{error}: {archive}"))?;
    let mut header = [0u8; 30];
    file.seek(SeekFrom::Start(entry.header_offset))
        .map_err(|error| error.to_string())?;
    file.read_exact(&mut header)
        .map_err(|error| error.to_string())?;
    if header[0..4] != [0x50, 0x4B, 0x03, 0x04] {
        return Err(format!("{archive} has a corrupt local header"));
    }
    let name_len = u16_at(&header, 26).ok_or("truncated local header")? as u64;
    let extra_len = u16_at(&header, 28).ok_or("truncated local header")? as u64;
    let data_offset = entry.header_offset + 30 + name_len + extra_len;
    let mut compressed = vec![0u8; entry.compress_size as usize];
    file.seek(SeekFrom::Start(data_offset))
        .map_err(|error| error.to_string())?;
    file.read_exact(&mut compressed)
        .map_err(|error| error.to_string())?;
    let plain = match entry.compress_type {
        0 => compressed,
        8 => {
            let source = value::bytes_new(compressed);
            let wbits = crate::ops::int_result(-15);
            let roots = crate::heap::root_values(&[source]);
            let inflated = super::zlib_mod::call("decompress", &[source, wbits]);
            crate::heap::release_roots(roots);
            if exc::pending() {
                return Err("deflate stream is corrupt".to_string());
            }
            match value::as_bytes(inflated) {
                Some(bytes) => bytes.to_vec(),
                None => return Err("deflate stream is corrupt".to_string()),
            }
        }
        other => return Err(format!("compression type {other} is not supported")),
    };
    let crc_input = value::bytes_new(plain.clone());
    let roots = crate::heap::root_values(&[crc_input]);
    let crc = super::zlib_mod::call("crc32", &[crc_input]);
    crate::heap::release_roots(roots);
    if exc::pending() {
        return Err("cannot checksum entry".to_string());
    }
    if value::as_integer(crc).map(|crc| crc as u64 & 0xFFFF_FFFF) != Some(u64::from(entry.crc)) {
        return Err(format!("bad CRC-32 for file '{}'", entry.name));
    }
    Ok(plain)
}

/// Central-directory entries of `archive`, cached in
/// `zipimport._zip_directory_cache`.
fn zip_entries(archive: &str) -> Result<Vec<ZipEntry>, String> {
    if let Some(cached) = zip_cache(archive) {
        let entries: Vec<ZipEntry> = value::as_dict(cached)
            .map(|pairs| {
                pairs
                    .iter()
                    .filter_map(|(key, item)| {
                        let name = value::as_str(*key)?.to_string();
                        let fields = value::as_tuple(*item)?;
                        if fields.len() != 7 {
                            return None;
                        }
                        Some(ZipEntry {
                            name,
                            compress_type: value::as_integer(fields[0])? as u16,
                            compress_size: value::as_integer(fields[1])? as u64,
                            file_size: value::as_integer(fields[2])? as u64,
                            header_offset: value::as_integer(fields[3])? as u64,
                            time: value::as_integer(fields[4])? as u16,
                            date: value::as_integer(fields[5])? as u16,
                            crc: value::as_integer(fields[6])? as u32,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !entries.is_empty() {
            return Ok(entries);
        }
    }
    let entries = read_central_directory(archive)?;
    store_zip_cache(archive, &entries);
    Ok(entries)
}

fn zip_cache(archive: &str) -> Option<i64> {
    let cache = zip_directory_cache();
    value::as_dict(cache).and_then(|pairs| {
        pairs
            .iter()
            .find_map(|(key, item)| (value::as_str(*key) == Some(archive)).then_some(*item))
    })
}

fn store_zip_cache(archive: &str, entries: &[ZipEntry]) {
    let cache = zip_directory_cache();
    let key = value::str_alloc(archive.to_string());
    let dict = value::dict_new(Vec::new());
    let roots = crate::heap::root_values(&[cache, key, dict]);
    for entry in entries {
        let name = value::str_alloc(entry.name.clone());
        let name_roots = crate::heap::root_values(&[name]);
        let fields = vec![
            crate::ops::int_result(i64::from(entry.compress_type)),
            crate::ops::int_result(entry.compress_size as i64),
            crate::ops::int_result(entry.file_size as i64),
            crate::ops::int_result(entry.header_offset as i64),
            crate::ops::int_result(i64::from(entry.time)),
            crate::ops::int_result(i64::from(entry.date)),
            crate::ops::int_result(i64::from(entry.crc)),
        ];
        let tuple = value::tuple_new(fields);
        let tuple_roots = crate::heap::root_values(&[tuple]);
        crate::container::dict_set(dict, name, tuple);
        crate::heap::release_roots(tuple_roots);
        crate::heap::release_roots(name_roots);
    }
    if let Some(mut pairs) = value::dict_pairs_mut(cache) {
        let _ = pairs.remove(key);
    }
    crate::container::dict_set(cache, key, dict);
    crate::heap::release_roots(roots);
}

fn remove_zip_cache(archive: &str) -> bool {
    let cache = zip_directory_cache();
    let key = value::str_alloc(archive.to_string());
    let roots = crate::heap::root_values(&[key]);
    let removed = match value::dict_pairs_mut(cache) {
        Some(mut pairs) => pairs.remove(key).is_some(),
        None => false,
    };
    crate::heap::release_roots(roots);
    removed
}

/// Raises `zipimport.ZipImportError` with `message`.
fn raise_zip_error(message: String) -> i64 {
    let class = zip_import_error_class();
    let text = value::str_alloc(message);
    let roots = crate::heap::root_values(&[class, text]);
    let instance = crate::builtins::call(class, 1, &text as *const i64);
    crate::heap::release_roots(roots);
    if exc::pending() {
        return instance;
    }
    exc::raise_value(instance)
}

/// Splits `archivepath` into the archive file and the in-archive prefix.
///
/// CPython walks up the path until an existing file is found; the remainder is
/// the prefix (normalised to `/`-separated with a trailing separator).
fn find_archive(archivepath: &str) -> Result<(String, String), String> {
    if archivepath.is_empty() {
        return Err("archive path is empty".to_string());
    }
    let mut candidate = std::path::PathBuf::from(archivepath);
    loop {
        match std::fs::metadata(&candidate) {
            Ok(metadata) => {
                if metadata.is_dir() {
                    return Err(format!("{archivepath} is a directory"));
                }
                let archive = candidate.to_string_lossy().into_owned();
                let mut prefix = archivepath[archive.len()..].replace('\\', "/");
                while prefix.starts_with('/') {
                    prefix.remove(0);
                }
                if !prefix.is_empty() && !prefix.ends_with('/') {
                    prefix.push('/');
                }
                return Ok((archive, prefix));
            }
            Err(_) => match candidate.parent().map(|parent| parent.to_path_buf()) {
                Some(parent) if !parent.as_os_str().is_empty() => candidate = parent,
                _ => return Err(format!("{archivepath} is not a valid file")),
            },
        }
    }
}

fn zip_field(importer: i64, name: &str) -> Option<String> {
    value::instance_attr(importer, name)
        .and_then(value::as_str)
        .map(str::to_string)
}

/// The in-archive key for `fullname`: `prefix + dotted-name-as-path`.
fn zip_key(prefix: &str, fullname: &str) -> String {
    let mut key = prefix.to_string();
    let relative = fullname.trim_start_matches('/');
    if relative.contains('/') {
        key.push_str(relative);
    } else {
        key.push_str(&relative.replace('.', "/"));
    }
    key
}

fn zip_entry_named(archive: &str, name: &str) -> Result<Option<ZipEntry>, String> {
    Ok(zip_entries(archive)?.into_iter().find(|entry| entry.name == name))
}

/// `zipimporter(archivepath)`.
fn zipimporter_init(args: &[i64]) -> i64 {
    let receiver = args[0];
    let Some(archivepath) = args.get(1).and_then(|arg| value::as_str(*arg)) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "archivepath must be str, not '{}'",
                value::type_name(args.get(1).copied().unwrap_or(NONE))
            ),
        );
    };
    let (archive, prefix) = match find_archive(archivepath) {
        Ok(found) => found,
        Err(message) => return raise_zip_error(message),
    };
    if let Err(message) = read_central_directory(&archive) {
        return raise_zip_error(message);
    }
    // Populate the shared directory cache for this archive (a re-created
    // importer must observe the file as it is now).
    remove_zip_cache(&archive);
    if let Err(message) = zip_entries(&archive) {
        return raise_zip_error(message);
    }
    for (field, text) in [("archive", archive.clone()), ("prefix", prefix)] {
        value::instance_setattr(
            receiver,
            field.to_string(),
            value::str_alloc(text),
        );
    }
    NONE
}

fn zipimporter_get_files(args: &[i64]) -> i64 {
    let Some(archive) = zip_field(args[0], "archive") else {
        return exc::raise_with_message(TYPE_ERROR, "zipimporter has no 'archive' attribute");
    };
    match zip_entries(&archive) {
        Ok(entries) => {
            let names: Vec<i64> = entries
                .iter()
                .map(|entry| value::str_alloc(entry.name.clone()))
                .collect();
            let roots = crate::heap::root_values(&names);
            let list = value::list_new(names);
            crate::heap::release_roots(roots);
            list
        }
        Err(message) => raise_zip_error(message),
    }
}

fn zipimporter_invalidate(args: &[i64]) -> i64 {
    if let Some(archive) = zip_field(args[0], "archive") {
        remove_zip_cache(&archive);
    }
    NONE
}

/// The module file or package initializer of `fullname` inside the importer's
/// archive, as `(key, is_package)`.
fn zip_module_info(importer: i64, fullname: &str) -> Result<(String, bool), String> {
    let archive = zip_field(importer, "archive").ok_or("zipimporter has no 'archive'")?;
    let prefix = zip_field(importer, "prefix").unwrap_or_default();
    let key = zip_key(&prefix, fullname);
    let entries = zip_entries(&archive)?;
    let has = |name: &str| entries.iter().any(|entry| entry.name == name);
    if has(&format!("{key}/__init__.pyc")) {
        return Ok((format!("{key}/__init__.pyc"), true));
    }
    if has(&format!("{key}/__init__.py")) {
        return Ok((format!("{key}/__init__.py"), true));
    }
    if has(&format!("{key}.pyc")) {
        return Ok((format!("{key}.pyc"), false));
    }
    if has(&format!("{key}.py")) {
        return Ok((format!("{key}.py"), false));
    }
    Err(format!("can't find module '{fullname}' on path"))
}

fn zipimporter_find_spec(args: &[i64]) -> i64 {
    let importer = args[0];
    let Some(fullname) = value::as_str(args[1]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "find_spec() argument 'fullname' must be str, not '{}'",
                value::type_name(args[1])
            ),
        );
    };
    let archive = match zip_field(importer, "archive") {
        Some(archive) => archive,
        None => return exc::raise_with_message(TYPE_ERROR, "zipimporter has no archive"),
    };
    let (key, is_package) = match zip_module_info(importer, fullname) {
        Ok(info) => info,
        Err(_) => return NONE,
    };
    let origin = format!("{archive}/{key}");
    let search_locations = is_package.then(|| {
        let package_dir = key.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
        vec![format!("{archive}/{package_dir}")]
    });
    new_spec(
        fullname,
        importer,
        Some(origin),
        search_locations,
        true,
    )
}

fn zipimporter_is_package(args: &[i64]) -> i64 {
    let Some(fullname) = value::as_str(args[1]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "is_package() argument must be str, not '{}'",
                value::type_name(args[1])
            ),
        );
    };
    match zip_module_info(args[0], fullname) {
        Ok((_, is_package)) => value::bool_value(is_package),
        Err(message) => raise_zip_error(message),
    }
}

fn zipimporter_get_filename(args: &[i64]) -> i64 {
    let Some(fullname) = value::as_str(args[1]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "get_filename() argument must be str, not '{}'",
                value::type_name(args[1])
            ),
        );
    };
    let Some(archive) = zip_field(args[0], "archive") else {
        return exc::raise_with_message(TYPE_ERROR, "zipimporter has no archive");
    };
    match zip_module_info(args[0], fullname) {
        Ok((key, _)) => value::str_alloc(format!("{archive}/{key}")),
        Err(message) => raise_zip_error(message),
    }
}

fn zipimporter_get_source(args: &[i64]) -> i64 {
    let Some(fullname) = value::as_str(args[1]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "get_source() argument must be str, not '{}'",
                value::type_name(args[1])
            ),
        );
    };
    let Some(archive) = zip_field(args[0], "archive") else {
        return exc::raise_with_message(TYPE_ERROR, "zipimporter has no archive");
    };
    let (key, _) = match zip_module_info(args[0], fullname) {
        Ok(info) => info,
        Err(message) => return raise_zip_error(message),
    };
    // A source file may accompany the bytecode; only `.pyc` means "no source".
    let source_key = match key.strip_suffix(".pyc") {
        Some(stem) => format!("{stem}.py"),
        None => key.clone(),
    };
    let entry = match zip_entry_named(&archive, &source_key) {
        Ok(Some(entry)) => entry,
        Ok(None) => return NONE,
        Err(message) => return raise_zip_error(message),
    };
    match read_zip_member(&archive, &entry) {
        Ok(bytes) => {
            let source = value::bytes_new(bytes);
            super::importlib_util::call("decode_source", &[source])
        }
        Err(message) => raise_zip_error(message),
    }
}

fn zipimporter_get_code(args: &[i64]) -> i64 {
    let Some(fullname) = value::as_str(args[1]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "get_code() argument must be str, not '{}'",
                value::type_name(args[1])
            ),
        );
    };
    let Some(archive) = zip_field(args[0], "archive") else {
        return exc::raise_with_message(TYPE_ERROR, "zipimporter has no archive");
    };
    let (key, _) = match zip_module_info(args[0], fullname) {
        Ok(info) => info,
        Err(message) => return raise_zip_error(message),
    };
    // Validate the entry's data so a corrupt archive still fails truthfully.
    if let Ok(Some(entry)) = zip_entry_named(&archive, &key) {
        if let Err(message) = read_zip_member(&archive, &entry) {
            return raise_zip_error(message);
        }
    }
    exc::raise_with_message(
        NOT_IMPLEMENTED_ERROR,
        &format!(
            "get_code({fullname:?}) cannot unmarshal a code object: Hydro executes \
             modules ahead of time and has no runtime bytecode interpreter"
        ),
    )
}

fn zipimporter_get_data(args: &[i64]) -> i64 {
    let Some(path) = value::as_str(args[1]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "get_data() argument must be str, not '{}'",
                value::type_name(args[1])
            ),
        );
    };
    let Some(archive) = zip_field(args[0], "archive") else {
        return exc::raise_with_message(TYPE_ERROR, "zipimporter has no archive");
    };
    let key = match path.strip_prefix(archive.as_str()) {
        Some(rest) => rest.trim_start_matches('/').to_string(),
        None => path.to_string(),
    };
    match zip_entry_named(&archive, &key) {
        Ok(Some(entry)) => match read_zip_member(&archive, &entry) {
            Ok(bytes) => value::bytes_new(bytes),
            Err(message) => raise_zip_error(message),
        },
        Ok(None) => exc::raise_with_message(
            OS_ERROR,
            &format!("can't find {path:?} in {archive}"),
        ),
        Err(message) => raise_zip_error(message),
    }
}

fn zipimporter_load_module(args: &[i64]) -> i64 {
    let Some(fullname) = value::as_str(args[1]) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "load_module() argument must be str, not '{}'",
                value::type_name(args[1])
            ),
        );
    };
    if let Some(module) = super::registry_lookup(fullname) {
        return module;
    }
    raise_zip_error(format!(
        "cannot load module '{fullname}' from a zip archive: Hydro executes \
         modules ahead of time and has no runtime bytecode interpreter"
    ))
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

    #[test]
    fn install_registers_the_path_finder_and_bootstrap_module_once() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        crate::core::globals::reset();
        exc::init();
        let bootstrap = value::module_value("importlib._bootstrap".to_string());
        let roots = crate::heap::root_values(&[bootstrap]);

        assert_eq!(call("_install", &[bootstrap]), NONE);
        assert!(
            !exc::pending(),
            "{}",
            value::to_string(exc::pending_value())
        );
        assert_eq!(
            crate::core::globals::registry_get(BOOTSTRAP_KEY),
            Some(bootstrap)
        );

        let meta_path = crate::modules::module_get_by_name("sys", "meta_path");
        let finders = value::as_list(meta_path).expect("sys.meta_path is a list");
        let finder = path_finder_class();
        assert_eq!(
            finders.iter().filter(|item| **item == finder).count(),
            1,
            "PathFinder is installed exactly once"
        );
        let after_first = finders.len();

        // Repeated installs are idempotent.
        assert_eq!(call("_install", &[bootstrap]), NONE);
        assert_eq!(
            value::as_list(meta_path).map(|finders| finders.len()),
            Some(after_first)
        );
        assert!(!exc::pending());

        crate::heap::release_roots(roots);
        crate::core::globals::reset();
        exc::init();
    }

    #[test]
    fn install_requires_the_bootstrap_module() {
        exc::init();
        call("_install", &[]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::clear();
    }

    #[test]
    fn zip_directory_cache_is_a_real_empty_mapping() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        crate::core::globals::reset();
        exc::init();
        let cache = constant("_zip_directory_cache");
        assert_eq!(cache, constant("_zip_directory_cache"));
        let entries = value::as_dict(cache).expect("_zip_directory_cache is a dict");
        assert!(entries.is_empty());
        assert!(!exc::pending());

        // It is the live mapping, not a fresh copy per read.
        let key = value::str_alloc("/tmp/hydro.zip".to_string());
        let value = value::dict_new(Vec::new());
        let roots = crate::heap::root_values(&[key, value]);
        crate::container::dict_set(cache, key, value);
        crate::heap::release_roots(roots);
        assert_eq!(
            value::as_dict(constant("_zip_directory_cache")).map(|entries| entries.len()),
            Some(1)
        );

        crate::core::globals::reset();
        exc::init();
    }

    #[test]
    fn path_finder_finds_native_and_unknown_modules() {
        exc::init();
        let finder = path_finder_class();
        let spec = call(
            PATH_FINDER_FIND_SPEC,
            &[finder, value::str_alloc("math".to_string()), NONE, NONE],
        );
        assert!(
            !exc::pending(),
            "{}",
            value::to_string(exc::pending_value())
        );
        assert_eq!(spec_name(spec).as_deref(), Some("math"));
        assert_eq!(
            value::instance_attr(spec, "loader"),
            Some(builtin_importer_class())
        );

        exc::clear();
        let missing = call(
            PATH_FINDER_FIND_SPEC,
            &[
                finder,
                value::str_alloc("hydro_no_such_module_xyz".to_string()),
                NONE,
                NONE,
            ],
        );
        assert_eq!(missing, NONE);
        assert!(!exc::pending());
    }

    #[test]
    fn file_finder_hook_accepts_directories_only() {
        exc::init();
        let hook = call(FILE_FINDER_PATH_HOOK, &[file_finder_class()]);
        assert!(!exc::pending());
        assert_eq!(value::function_name(hook), "_file_finder_hook".to_string());

        let dir = std::env::temp_dir().to_string_lossy().into_owned();
        let finder = call(FILE_FINDER_HOOK, &[value::str_alloc(dir.clone())]);
        assert!(
            !exc::pending(),
            "{}",
            value::to_string(exc::pending_value())
        );
        assert_eq!(
            value::instance_attr(finder, "path").and_then(value::as_str),
            Some(dir.as_str())
        );
        // A FileFinder instance resolves a real file in its directory.
        let spec = call(
            FILE_FINDER_FIND_SPEC,
            &[finder, value::str_alloc("math".to_string()), NONE],
        );
        assert_eq!(spec, NONE, "native modules are not directory entries");

        exc::clear();
        call(
            FILE_FINDER_HOOK,
            &[value::str_alloc("/hydro_no_such_dir_xyz".to_string())],
        );
        assert_eq!(exc::pending_tag(), i64::from(IMPORT_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "only directories are supported"
        );
        exc::clear();
    }

    #[test]
    fn external_loader_classes_are_cached() {
        exc::init();
        for name in [
            "PathFinder",
            "FileFinder",
            "SourcelessFileLoader",
            "AppleFrameworkLoader",
        ] {
            let first = constant(name);
            assert_eq!(first, constant(name), "{name} is cached");
            assert!(value::as_class(first).is_some(), "{name} is a class");
            assert!(!exc::pending(), "{name}");
        }
    }
}
