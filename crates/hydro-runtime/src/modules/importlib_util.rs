//! `importlib.util`: spec discovery, file specs, module creation and
//! `LazyLoader`.
//!
//! Discovery is filesystem based, layered over Hydro's native module table:
//! a name in `hydro_ir::stdlib` is a built-in module, an installed package is
//! found exactly where CPython would find it on the live `sys.path` list
//! (`HYDRO_SITE_PACKAGES`, the `VIRTUAL_ENV`/`CONDA_PREFIX` site directories,
//! `PYTHONPATH`, `HYDRO_STDLIB` and the pure-Python modules vendored in Hydro's
//! `stdlib/` seed that list), and a module compiled into the program resolves
//! through the runtime registry.
//!
//! A dotted name is resolved the way `_find_and_load` does it: the parent
//! package's live `__path__` (which `pkgutil.extend_path`-style code may have
//! extended) is the finder path, and only the last dotted component is looked
//! up there. A directory portion without `__init__.py` contributes a PEP 420
//! namespace package, collected across every path entry exactly like
//! `PathFinder._get_spec`.
//!
//! Hydro compiles Python ahead of time, so `module_from_spec` returns the
//! already-initialized module when one exists and otherwise creates a real
//! module-like namespace carrying the documented metadata attributes. A
//! discovered source module's body is then executed through the runtime
//! `compile`/`exec` evaluator (`SourceFileLoader.exec_module`), so its
//! bindings are real; a module compiled into the program is never re-run.

use std::path::{Path, PathBuf};

use hydro_ir::MethodKind;

use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, IMPORT_ERROR, MODULE_NOT_FOUND_ERROR, SYNTAX_ERROR, TYPE_ERROR,
    UNICODE_DECODE_ERROR, VALUE_ERROR,
};
use crate::value::{self, NONE};

use super::importlib_machinery as machinery;

const MODULE: &str = "importlib.util";

const LAZY_INIT: &str = "_lazy_init";
const LAZY_CREATE_MODULE: &str = "_lazy_create_module";
const LAZY_EXEC_MODULE: &str = "_lazy_exec_module";
const CACHE_FROM_SOURCE: &str = "cache_from_source";
const SOURCE_FROM_CACHE: &str = "source_from_cache";
const SOURCE_HASH: &str = "source_hash";

/// Dispatches a native `importlib.util` function. Arity is enforced by the
/// central spec before this point.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "find_spec" => find_spec(args),
        "_find_spec_from_path" => find_spec_from_path(args),
        "spec_from_file_location" => spec_from_file_location(args),
        "spec_from_loader" => spec_from_loader(args),
        "module_from_spec" => module_from_spec(args),
        "resolve_name" => resolve_name(args),
        "decode_source" => decode_source(args),
        CACHE_FROM_SOURCE => cache_from_source(args),
        SOURCE_FROM_CACHE => source_from_cache(args),
        SOURCE_HASH => source_hash(args),
        LAZY_INIT => lazy_init(args),
        LAZY_CREATE_MODULE => lazy_create_module(args),
        LAZY_EXEC_MODULE => lazy_exec_module(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '{MODULE}' has no attribute '{function}'"),
        ),
    }
}

/// CPython's `PYC_MAGIC_NUMBER` for the emulated interpreter version.
///
/// `_imp.pyc_magic_number_token` is `PYC_MAGIC_NUMBER | '\r' << 16 | '\n' <<
/// 24`; `importlib._bootstrap_external` derives `MAGIC_NUMBER` with
/// `_imp.pyc_magic_number_token.to_bytes(4, 'little')`, so the exported bytes
/// are `2B 0E 0D 0A` for 3627 (`0x0E2B`).
///
/// The runtime's `_imp` module does not expose the token yet, so this constant
/// is the single definition: when `_imp.pyc_magic_number_token` is added it
/// must re-export this value (and [`MAGIC_NUMBER_BYTES`] must not be restated
/// elsewhere).
pub(crate) const PYC_MAGIC_NUMBER: u32 = 3627;

/// `_imp.pyc_magic_number_token`: the magic number with the `\r\n` terminator
/// in the high bytes, the key `_imp.source_hash` hashes sources with.
pub(crate) const PYC_MAGIC_NUMBER_TOKEN: u32 =
    PYC_MAGIC_NUMBER | ((b'\r' as u32) << 16) | ((b'\n' as u32) << 24);

/// `importlib.util.MAGIC_NUMBER`, in the byte order `.pyc` headers use.
pub(crate) const MAGIC_NUMBER_BYTES: [u8; 4] = PYC_MAGIC_NUMBER_TOKEN.to_le_bytes();

/// Materialises a `importlib.util` module constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "LazyLoader" => lazy_loader_class(),
        "MAGIC_NUMBER" => magic_number(),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '{MODULE}' has no attribute '{name}'"),
        ),
    }
}

/// The cached `MAGIC_NUMBER` bytes object.
fn magic_number() -> i64 {
    const KEY: &str = "importlib.util.MAGIC_NUMBER";
    if let Some(cached) = globals::registry_get(KEY) {
        return cached;
    }
    let bytes = value::bytes_new(MAGIC_NUMBER_BYTES.to_vec());
    let roots = crate::heap::root_values(&[bytes]);
    globals::registry_set(KEY, bytes);
    crate::heap::release_roots(roots);
    bytes
}

/// `cache_from_source(path, debug_override=None, *, optimization=None)`.
///
/// The `__pycache__` location, cache tag (`cpython-314`) and optional
/// `.opt-<level>` infix are CPython's; Hydro reports optimization level 0 for
/// `sys.flags.optimize`, so the default name is the unoptimized `.pyc`.
fn cache_from_source(args: &[i64]) -> i64 {
    let path_arg = args.first().copied().unwrap_or(NONE);
    let Some(path) = value::as_str(path_arg) else {
        if value::as_bytes(path_arg).is_some() {
            // CPython's byte-path path fails the same way (`str.partition`
            // on a bytes filename).
            return exc::raise_with_message(
                TYPE_ERROR,
                "argument should be integer or bytes-like object, not 'str'",
            );
        }
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "expected str, bytes or os.PathLike object, not {}",
                value::type_name(path_arg)
            ),
        );
    };
    let debug_override = args.get(1).copied().unwrap_or(NONE);
    let mut optimization = args.get(2).copied().unwrap_or(NONE);
    if debug_override != NONE {
        if optimization != NONE {
            return exc::raise_with_message(
                TYPE_ERROR,
                "debug_override or optimization must be set to None",
            );
        }
        optimization = if value::truthy(debug_override) {
            value::str_alloc(String::new())
        } else {
            crate::ops::int_result(1)
        };
    }
    // `sys.flags.optimize` is 0, so `None` means the unoptimized tag.
    let optimization = if optimization == NONE {
        String::new()
    } else if let Some(text) = value::as_str(optimization) {
        text.to_string()
    } else {
        value::to_string(optimization)
    };
    let mut tag = hydro_ir::PYTHON_ABI_TAG.to_string();
    if !optimization.is_empty() {
        if !optimization.chars().all(char::is_alphanumeric) {
            return exc::raise_with_message(
                VALUE_ERROR,
                &format!("'{optimization}' is not alphanumeric"),
            );
        }
        tag.push_str(".opt-");
        tag.push_str(&optimization);
    }
    let (head, tail) = split_tail(path);
    let (base, sep, rest) = match tail.rfind('.') {
        Some(index) => (&tail[..index], ".", &tail[index + 1..]),
        None => ("", "", tail),
    };
    let stem = if base.is_empty() { rest } else { base };
    let filename = format!("{stem}{sep}{tag}{}", BYTECODE_SUFFIX);
    let cached = if head.is_empty() {
        Path::new("__pycache__").join(filename)
    } else {
        Path::new(head).join("__pycache__").join(filename)
    };
    value::str_alloc(cached.to_string_lossy().into_owned())
}

/// The first bytecode suffix (`BYTECODE_SUFFIXES[0]`) without allocating the
/// module's list.
const BYTECODE_SUFFIX: &str = ".pyc";

/// The first source suffix (`SOURCE_SUFFIXES[0]`).
const SOURCE_SUFFIX: &str = ".py";

/// `source_from_cache(path)`: the source path a PEP 3147/488 cache path was
/// compiled from.
///
/// The `.pyc` file does not need to exist. The path must follow the cache
/// layout — a bottom-level `__pycache__` directory holding a filename with two
/// or three dots (`<stem>.<tag>.pyc`, optionally `<stem>.<tag>.opt-<level>.pyc`)
/// — and anything else raises the `ValueError` CPython reports. `sys.pycache_
/// prefix` and `sys.implementation.cache_tag` are always available here, so
/// the `NotImplementedError` branches cannot occur.
fn source_from_cache(args: &[i64]) -> i64 {
    let path_arg = args.first().copied().unwrap_or(NONE);
    let Some(path) = value::as_str(path_arg) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "expected str, bytes or os.PathLike object, not {}",
                value::type_name(path_arg)
            ),
        );
    };
    let (head, filename) = split_tail(path);
    let (pycache_dir, pycache) = match head.rfind('/') {
        Some(index) => (&head[..index], &head[index + 1..]),
        None => ("", head),
    };
    if pycache != "__pycache__" {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!("__pycache__ not bottom-level directory in '{path}'"),
        );
    }
    let dot_count = filename.bytes().filter(|&byte| byte == b'.').count();
    if dot_count != 2 && dot_count != 3 {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!("expected only 2 or 3 dots in '{filename}'"),
        );
    }
    if dot_count == 3 {
        // `<stem>.<tag>.<optimization>.pyc`: the optimization component is
        // `opt-<level>`, always the second-to-last dotted field.
        let mut fields = filename.rsplitn(3, '.');
        fields.next(); // ".pyc"
        let optimization = fields.next().unwrap_or("");
        let Some(level) = optimization.strip_prefix("opt-") else {
            return exc::raise_with_message(
                VALUE_ERROR,
                "optimization portion of filename does not start with 'opt-'",
            );
        };
        if level.is_empty() || !level.chars().all(char::is_alphanumeric) {
            return exc::raise_with_message(
                VALUE_ERROR,
                &format!("optimization level '{optimization}' is not an alphanumeric value"),
            );
        }
    }
    let stem = filename.split('.').next().unwrap_or("");
    let source = if pycache_dir.is_empty() {
        format!("{stem}{SOURCE_SUFFIX}")
    } else {
        format!("{pycache_dir}/{stem}{SOURCE_SUFFIX}")
    };
    value::str_alloc(source)
}

/// `os.path.split` for a `str` path: the last component and its directory.
fn split_tail(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(index) => (&path[..index], &path[index + 1..]),
        None => ("", path),
    }
}

/// `source_hash(source_bytes)`: CPython's keyed SipHash-1-3 of the source,
/// keyed with `_imp.pyc_magic_number_token`, as the eight bytes hash-based
/// `.pyc` files store.
fn source_hash(args: &[i64]) -> i64 {
    let source_arg = args.first().copied().unwrap_or(NONE);
    let Some(source) = value::as_bytes(source_arg) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "a bytes-like object is required, not '{}'",
                value::type_name(source_arg)
            ),
        );
    };
    let hash = siphash13(u64::from(PYC_MAGIC_NUMBER_TOKEN), 0, source);
    value::bytes_new(hash.to_le_bytes().to_vec())
}

/// CPython's `_Py_KeyedHash` (SipHash-1-3) with an arbitrary 64-bit key,
/// little-endian: the digest `_imp.source_hash` returns.
pub(crate) fn keyed_source_hash(key: u64, source: &[u8]) -> [u8; 8] {
    siphash13(key, 0, source).to_le_bytes()
}

/// CPython's `_Py_KeyedHash` (SipHash-1-3), used by `_imp.source_hash`.
// `as_chunks` needs Rust 1.88; the workspace MSRV is 1.85.
#[allow(clippy::chunks_exact_to_as_chunks)]
fn siphash13(k0: u64, k1: u64, source: &[u8]) -> u64 {
    let mut b = (source.len() as u64) << 56;
    let mut v0 = k0 ^ 0x736f_6d65_7073_6575;
    let mut v1 = k1 ^ 0x646f_7261_6e64_6f6d;
    let mut v2 = k0 ^ 0x6c79_6765_6e65_7261;
    let mut v3 = k1 ^ 0x7465_6462_7974_6573;
    let mut chunks = source.chunks_exact(8);
    for chunk in &mut chunks {
        let mut block = [0u8; 8];
        block.copy_from_slice(chunk);
        let mi = u64::from_le_bytes(block);
        v3 ^= mi;
        single_round(&mut v0, &mut v1, &mut v2, &mut v3);
        v0 ^= mi;
    }
    let remainder = chunks.remainder();
    let mut tail = [0u8; 8];
    tail[..remainder.len()].copy_from_slice(remainder);
    b |= u64::from_le_bytes(tail);
    v3 ^= b;
    single_round(&mut v0, &mut v1, &mut v2, &mut v3);
    v0 ^= b;
    v2 ^= 0xff;
    for _ in 0..3 {
        single_round(&mut v0, &mut v1, &mut v2, &mut v3);
    }
    (v0 ^ v1) ^ (v2 ^ v3)
}

/// One SipHash round pair (`SINGLE_ROUND` in CPython's `pyhash.c`).
fn single_round(v0: &mut u64, v1: &mut u64, v2: &mut u64, v3: &mut u64) {
    half_round(v0, v1, v2, v3, 13, 16);
    half_round(v2, v1, v0, v3, 17, 21);
}

/// One `HALF_ROUND` in CPython's `pyhash.c`.
fn half_round(a: &mut u64, b: &mut u64, c: &mut u64, d: &mut u64, s: u32, t: u32) {
    *a = (*a).wrapping_add(*b);
    *c = (*c).wrapping_add(*d);
    *b = (*b).rotate_left(s) ^ *a;
    *d = (*d).rotate_left(t) ^ *c;
    *a = (*a).rotate_left(32);
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
        search_locations: native_search_locations(name),
        loader: LoaderKind::Builtin,
    })
}

/// Search locations of a native package: `Some(empty)` when the central
/// table declares submodules, so `ModuleSpec.parent` reports the package
/// itself (matching `module.__package__` for native modules).
fn native_search_locations(name: &str) -> Option<Vec<String>> {
    let prefix = format!("{name}.");
    hydro_ir::stdlib::MODULES
        .iter()
        .any(|spec| spec.name.starts_with(&prefix))
        .then(Vec::new)
}

fn found_spec(found: &FoundSpec) -> i64 {
    spec_instance(found)
}

/// Resolves `name` to a module spec without consulting the caller's package
/// context. `None` means no module exists on the search path.
pub(crate) fn resolve_module(name: &str) -> Option<FoundSpec> {
    resolve_module_with_path(name, None)
}

/// [`resolve_module`] with an explicit finder path.
///
/// `None` searches the live `sys.path`, resolving a dotted name's parent
/// package first so its `__path__` is the finder path — exactly what
/// `importlib._bootstrap._find_and_load` does before calling `_find_spec`.
/// `Some(dirs)` replaces the finder path, exactly like `PathFinder`: the last
/// dotted component is looked up inside `dirs` (the parent package's
/// `__path__`), never re-prefixed with the whole dotted name. Built-in modules
/// and modules compiled into the program are still found through an explicit
/// path, because their finders ignore it.
pub(crate) fn resolve_module_with_path(name: &str, path: Option<&[String]>) -> Option<FoundSpec> {
    if name.is_empty() || name.split('.').any(str::is_empty) {
        return None;
    }
    if name == "__main__" {
        // CPython always has a `__main__` module: the entry program. Materialise
        // its module value so the registry branch below describes it even when
        // the program has not bound a global yet.
        super::module_value_by_name("__main__");
    }
    if hydro_ir::stdlib::module(name).is_some() {
        return Some(FoundSpec {
            name: name.to_string(),
            origin: None,
            search_locations: native_search_locations(name),
            loader: LoaderKind::Builtin,
        });
    }
    let (parent, tail) = match name.rsplit_once('.') {
        Some((parent, tail)) => (Some(parent), tail),
        None => (None, name),
    };
    // A module compiled into the program but not installed as a file (for
    // example a local module outside `sys.path`). Its path is taken from the
    // `__file__` global when the module bound one; a package's live `__path__`
    // (possibly extended by `pkgutil.extend_path`) describes namespace packages.
    // The registry is name-keyed and its modules have no finder path, so an
    // explicit `path` does not hide them (`PathFinder` promises the same for
    // frozen modules).
    if super::registry_lookup(name).is_some() {
        return Some(registry_module_spec(name));
    }
    // An explicit finder path is already the parent package's `__path__` (or a
    // replacement for `sys.path`): CPython's `FileFinder` searches the last
    // dotted component there, so `pkg.mod` looks for `<dir>/mod.py`.
    if let Some(dirs) = path {
        return filesystem_leaf_in(name, tail, dirs);
    }
    // CPython imports the parent package before searching for a submodule and
    // passes `parent.__path__` to `PathFinder`. The live attribute is
    // authoritative: `__path__` may have been extended after import.
    if let Some(parent) = parent {
        let parent_dirs = module_path_dirs(parent);
        if let Some(dirs) = &parent_dirs {
            if let Some(found) = filesystem_leaf_in(name, tail, dirs) {
                return Some(found);
            }
        }
        let resolved_parent = if parent_dirs.is_none() {
            resolve_module_with_path(parent, None)
        } else {
            None
        };
        // A parent with a real `__path__` that lacks the leaf has no such
        // submodule. A parent with no path at all (`None`) is a plain module,
        // which CPython reports as "not a package". An *empty* path marks a
        // native package such as `unittest`: the central table has no
        // directory for it, but a source tree on `sys.path` still holds
        // `unittest/mock.py`, so the dotted-name walk below must run.
        let fallback = match (&parent_dirs, &resolved_parent) {
            (Some(dirs), _) => dirs.is_empty(),
            (None, Some(found_parent)) => match found_parent.search_locations.as_deref() {
                Some(dirs) => dirs.is_empty() && found_parent.loader == LoaderKind::Builtin,
                None => false,
            },
            // The parent is unknown to every finder, but a PEP 420 layout can
            // still hold the full dotted path.
            (None, None) => true,
        };
        if !fallback {
            return None;
        }
    }
    let search = live_search_path();
    if let Some(found) = filesystem_module_in(name, &search) {
        return Some(found);
    }
    None
}

/// The `FoundSpec` of a module compiled into the program, from its globals.
fn registry_module_spec(name: &str) -> FoundSpec {
    let origin = globals::lookup(name, "__file__")
        .and_then(value::as_str)
        .map(machinery::absolute);
    let search_locations = module_path_dirs(name).or_else(|| {
        origin.as_deref().and_then(|path| {
            is_package_init(Path::new(path))
                .then(|| Path::new(path).parent())
                .flatten()
                .map(|dir| vec![dir.to_string_lossy().into_owned()])
        })
    });
    let loader = if origin.is_some() {
        LoaderKind::Source
    } else {
        LoaderKind::None
    };
    FoundSpec {
        name: name.to_string(),
        origin,
        search_locations,
        loader,
    }
}

/// `sys.path` as the live search roots.
///
/// CPython's `PathFinder` reads the mutable `sys.path` list on every import;
/// Hydro derives the initial list from the environment but must observe later
/// writes (`sys.path.append(...)`), so the module attribute wins once it
/// exists. The environment-derived list is the fallback when the `sys` surface
/// is unavailable.
fn live_search_path() -> Vec<String> {
    let path = super::module_get_by_name("sys", "path");
    if exc::pending() {
        exc::clear();
        return super::search_path();
    }
    if let Some(items) = value::as_list(path) {
        return items
            .iter()
            .filter_map(|item| value::as_str(*item).map(str::to_string))
            .collect();
    }
    super::search_path()
}

/// A package's live `__path__` directories.
///
/// A compiled package keeps `__path__` in the runtime global store (so it
/// survives a `sys.modules` pop); a namespace package materialised by
/// `module_from_spec` carries it in its namespace dict. Native packages have
/// no directory of their own and report none.
fn module_path_dirs(name: &str) -> Option<Vec<String>> {
    if let Some(module) = super::registry_lookup(name) {
        if module != NONE {
            if let Some(dirs) = module_path_dirs_value(module) {
                return Some(dirs);
            }
        }
    }
    path_dirs(globals::lookup(name, "__path__")?)
}

/// `getattr(module, name)` for a module-like value: a module value, a
/// namespace created by `module_from_spec`, or a `types.ModuleType` instance.
///
/// `None` means the attribute is absent; an attribute whose value is `None`
/// is reported as `Some(NONE)`. A failure other than `AttributeError` stays
/// pending for the caller.
fn module_attribute(module: i64, name: &str) -> Option<i64> {
    if module == NONE {
        return None;
    }
    if let Some(pairs) = value::as_namespace(module) {
        return pairs
            .iter()
            .find(|(key, _)| key.as_str() == name)
            .map(|(_, item)| *item);
    }
    if let Some(item) = value::instance_attr(module, name) {
        return Some(item);
    }
    let state = value::as_module(module)?;
    let item = super::module_attr(state, name);
    if exc::pending() {
        if exc::pending_tag() == i64::from(ATTRIBUTE_ERROR) {
            exc::clear();
        }
        return None;
    }
    Some(item)
}

/// `module.__path__` of a module-like value, or `None` when the value is not
/// a package.
///
/// A native package is one whose submodules the central table declares; it has
/// no directory of its own, so its path is the empty marker (`NONE`) while the
/// attribute still counts as present.
pub(crate) fn module_path_value(module: i64) -> Option<i64> {
    if let Some(path) = module_attribute(module, "__path__") {
        return Some(path);
    }
    let state = value::as_module(module)?;
    native_search_locations(state.name()).map(|_| NONE)
}

/// `module.__path__` as owned directory strings; `None` when the value is not
/// a package. A package whose path is empty or not a list of strings reports
/// an empty directory list.
pub(crate) fn module_path_dirs_value(module: i64) -> Option<Vec<String>> {
    match module_path_value(module)? {
        NONE => Some(Vec::new()),
        path => Some(path_dirs(path).unwrap_or_default()),
    }
}

/// Whether a module-like value is a package (`module.__path__` is present).
pub(crate) fn module_is_package(module: i64) -> bool {
    module_path_value(module).is_some()
}

/// Raises CPython's `ModuleNotFoundError` with an arbitrary message and the
/// missing module's name attached as the `name` keyword.
pub(crate) fn raise_module_not_found(message: &str, name: &str) -> i64 {
    let missing = value::str_alloc(name.to_string());
    let roots = crate::heap::root_values(&[missing]);
    let exception = exc::exception_value_with_keywords(
        MODULE_NOT_FOUND_ERROR,
        message,
        vec![message.to_string()],
        vec![("name".to_string(), missing)],
    );
    crate::heap::release_roots(roots);
    exc::raise_value(exception)
}

/// `__path__` as owned directory strings (a list or tuple of `str`).
fn path_dirs(path: i64) -> Option<Vec<String>> {
    let items = value::as_list(path).or_else(|| value::as_tuple(path))?;
    Some(
        items
            .iter()
            .filter_map(|item| value::as_str(*item).map(str::to_string))
            .collect(),
    )
}

/// The finder path CPython passes for `name`: its parent package's `__path__`.
///
/// A native package has an empty `__path__` (the central table names its
/// submodules but no directory exists). Passing that empty list to a finder
/// would hide the dotted source tree on `sys.path` (`unittest/mock.py`), so
/// the live search roots are used instead, exactly as if the parent had no
/// path entry of its own.
pub(crate) fn parent_finder_path(name: &str) -> Option<Vec<String>> {
    let (parent, _) = name.rsplit_once('.')?;
    let dirs = module_path_dirs(parent).or_else(|| {
        resolve_module_with_path(parent, None).and_then(|found| found.search_locations)
    })?;
    (!dirs.is_empty()).then_some(dirs)
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

/// Filesystem-only discovery for `importlib.machinery.FileFinder` and the
/// `FileFinder` path hook: native modules and modules that exist only in the
/// runtime registry are not found through a directory finder.
///
/// A `FileFinder` owns one directory and looks up the last dotted component
/// there (`FileFinder.find_spec(fullname)` uses `fullname.rpartition('.')[2]`),
/// so the whole dotted name is never used as a path.
pub(crate) fn filesystem_spec(name: &str, dirs: &[String]) -> Option<FoundSpec> {
    if name.is_empty() || name.split('.').any(str::is_empty) {
        return None;
    }
    let tail = name.rsplit_once('.').map_or(name, |(_, tail)| tail);
    filesystem_leaf_in(name, tail, dirs)
}

/// Looks for the single component `leaf` of `name` inside `dirs` (a parent
/// package's `__path__`), reusing the same file layout rules as
/// [`filesystem_module_in`].
///
/// A bare directory portion is a PEP 420 namespace-package candidate: every
/// portion is collected and returned once no later `dir` holds a regular
/// package or module, exactly like `PathFinder._get_spec`.
fn filesystem_leaf_in(name: &str, leaf: &str, dirs: &[String]) -> Option<FoundSpec> {
    if leaf.is_empty() {
        return None;
    }
    let mut namespaces: Vec<String> = Vec::new();
    for dir in dirs {
        let base = Path::new(dir).join(leaf);
        let init = base.join("__init__.py");
        if init.is_file() {
            return Some(FoundSpec {
                name: name.to_string(),
                origin: Some(machinery::absolute(&init.to_string_lossy())),
                search_locations: Some(vec![base.to_string_lossy().into_owned()]),
                loader: LoaderKind::Source,
            });
        }
        let source = base.with_extension("py");
        if source.is_file() {
            return Some(FoundSpec {
                name: name.to_string(),
                origin: Some(machinery::absolute(&source.to_string_lossy())),
                search_locations: None,
                loader: LoaderKind::Source,
            });
        }
        if let Some(path) = extension_file(&base) {
            return Some(FoundSpec {
                name: name.to_string(),
                origin: Some(machinery::absolute(&path.to_string_lossy())),
                search_locations: None,
                loader: LoaderKind::Extension,
            });
        }
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

/// Looks for `name` in `search`, which is the documented `sys.path` order for
/// the default resolution path.
fn filesystem_module_in(name: &str, search: &[String]) -> Option<FoundSpec> {
    if name.is_empty() || name.split('.').any(str::is_empty) {
        return None;
    }
    let parts: Vec<&str> = name.split('.').collect();
    let mut namespaces: Vec<String> = Vec::new();
    for dir in search {
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
///
/// CPython resolves a relative name, returns the stored `__spec__` for a name
/// already in `sys.modules`, and otherwise imports the parent package — the
/// parent's initializer may publish the `__path__` the finders need — before
/// walking `sys.meta_path` with that path. A parent that cannot be imported
/// raises the parent's `ModuleNotFoundError`; a parent that is not a package
/// raises `ModuleNotFoundError` naming the requested module.
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
    // `sys.modules` wins: a loaded module's `__spec__` is authoritative.
    match spec_from_sys_modules(&name) {
        Ok(Some(spec)) => return spec,
        Ok(None) => {}
        Err(raised) => return raised,
    }
    let mut path = None;
    if let Some((parent, _)) = name.rsplit_once('.') {
        // `__import__(parent, fromlist=['__path__'])`: importing the parent is
        // an observable step (`test_find_submodule` checks `sys.modules`).
        let imported = import_parent(parent);
        if exc::pending() {
            return NONE;
        }
        if imported == NONE {
            return crate::builtins::module_not_found(&name);
        }
        let Some(dirs) = module_path_dirs_value(imported) else {
            return raise_module_not_found(
                &format!(
                    "__path__ attribute not found on '{parent}' while trying to find '{name}'"
                ),
                &name,
            );
        };
        // A native package reports an empty path (no directory exists); the
        // live search roots keep a dotted source tree reachable.
        path = (!dirs.is_empty()).then_some(dirs);
    }
    meta_path_find_spec(&name, path.as_deref()).unwrap_or(NONE)
}

/// Imports `name` as `find_spec`'s parent step, returning its module value.
///
/// The result is the same object `sys.modules[name]` holds; a raised import
/// error stays pending for the caller.
fn import_parent(name: &str) -> i64 {
    let name_value = value::str_alloc(name.to_string());
    let roots = crate::heap::root_values(&[name_value]);
    let module = super::importlib::import_module(name_value, NONE);
    crate::heap::release_roots(roots);
    module
}

/// Whether `value` is a `types.ModuleType` (or subclass) instance.
fn is_module_type_instance(module: i64) -> bool {
    value::instance_class_value(module)
        .is_some_and(|class| value::is_subclass_of(class, super::types::module_type_class()))
}

/// `importlib.util._find_spec_from_path(name, path=None, target=None)`.
///
/// CPython's helper is `_find_spec(name, path)` plus the `sys.modules`
/// pre-check; `path` is forwarded to the finders and `target` is only a hint
/// for reload-aware finders (Hydro's finders need neither). Relative names are
/// not resolved here, matching `_bootstrap._find_spec`.
fn find_spec_from_path(args: &[i64]) -> i64 {
    let name = match str_arg(args, 0, "_find_spec_from_path", "name") {
        Ok(name) => name,
        Err(raised) => return raised,
    };
    let path = match path_arg(args.get(1).copied().unwrap_or(NONE)) {
        Ok(path) => path,
        Err(raised) => return raised,
    };
    if name.is_empty() || name.split('.').any(str::is_empty) {
        return NONE;
    }
    // The util helper checks `sys.modules` itself and treats a missing or
    // `None` `__spec__` as an error. Any other loaded module returns its
    // stored spec; only an unloaded name reaches the finders.
    match spec_from_sys_modules(&name) {
        Ok(Some(spec)) => return spec,
        Ok(None) => {}
        Err(raised) => return raised,
    }
    meta_path_find_spec(&name, path.as_deref()).unwrap_or(NONE)
}

/// What a `sys.meta_path` walk produced.
enum MetaMatch {
    /// Hydro's own finder resolved the module.
    Found(FoundSpec),
    /// A program-defined finder returned this spec.
    Finder(i64),
}

/// Walks `sys.meta_path` in order, returning the first match for `name`.
///
/// Hydro's finders are implemented natively but keep their place in the list:
/// `BuiltinImporter` answers for native modules, `FrozenImporter` reports no
/// frozen module (Hydro bundles none) and `PathFinder` searches `path` (or the
/// live `sys.path` when `None`). Program-defined finders are called as
/// `find_spec(name, path, None)` exactly where the list puts them, so a finder
/// appended after the default trio cannot shadow a native module and a finder
/// prepended before it can. When the list has no `PathFinder` — Hydro installs
/// one lazily through `importlib.machinery._install` — the path search keeps
/// CPython's default position, right after the builtin finders.
fn meta_path_match(name: &str, path: Option<&[String]>) -> Option<MetaMatch> {
    let meta_path = super::module_get_by_name("sys", "meta_path");
    if exc::pending() {
        exc::clear();
        return None;
    }
    let finders = value::as_list(meta_path)?;
    let builtin = machinery::builtin_importer_class();
    let frozen = machinery::frozen_importer_class();
    let path_finder = machinery::path_finder_class();
    let has_path_finder = finders.contains(&path_finder);
    let mut path_roots = None;
    let path_value = match path {
        Some(dirs) => {
            let list = value::list_new(Vec::new());
            let roots = crate::heap::root_values(&[list]);
            for dir in dirs {
                let item = value::str_alloc(dir.clone());
                let item_roots = crate::heap::root_values(&[item]);
                crate::container::list_append(list, item);
                crate::heap::release_roots(item_roots);
            }
            path_roots = Some(roots);
            list
        }
        None => NONE,
    };
    let name_value = value::str_alloc(name.to_string());
    let roots = crate::heap::root_values(&[meta_path, name_value, path_value]);
    if let Some(path_roots) = path_roots {
        crate::heap::release_roots(path_roots);
    }
    let mut searched_path = false;
    let mut saw_hydro_finder = false;
    let mut result = None;
    for finder in finders {
        if *finder == builtin {
            saw_hydro_finder = true;
            if hydro_ir::stdlib::module(name).is_some() {
                result = Some(MetaMatch::Found(FoundSpec {
                    name: name.to_string(),
                    origin: None,
                    search_locations: native_search_locations(name),
                    loader: LoaderKind::Builtin,
                }));
                break;
            }
        } else if *finder == frozen {
            // Hydro bundles no frozen module data, so the finder reports
            // nothing; a name that also exists as source under `sys.path` is
            // found by the path search.
            saw_hydro_finder = true;
        } else if *finder == path_finder {
            saw_hydro_finder = true;
            searched_path = true;
            if let Some(found) = resolve_module_with_path(name, path) {
                result = Some(MetaMatch::Found(found));
                break;
            }
        } else {
            // The implicit `PathFinder` keeps the position of CPython's
            // default entry: after the builtin finders, before appended ones.
            if !has_path_finder && saw_hydro_finder && !searched_path {
                searched_path = true;
                if let Some(found) = resolve_module_with_path(name, path) {
                    result = Some(MetaMatch::Found(found));
                    break;
                }
            }
            let find_spec =
                crate::core::methods::getattr(*finder, "find_spec".as_ptr(), "find_spec".len());
            if exc::pending() {
                exc::clear();
                continue;
            }
            let spec = crate::builtins::call(find_spec, 3, [name_value, path_value, NONE].as_ptr());
            if exc::pending() {
                crate::heap::release_roots(roots);
                return None;
            }
            if spec != NONE {
                result = Some(MetaMatch::Finder(spec));
                break;
            }
        }
    }
    if result.is_none() && !searched_path && saw_hydro_finder {
        if let Some(found) = resolve_module_with_path(name, path) {
            result = Some(MetaMatch::Found(found));
        }
    }
    crate::heap::release_roots(roots);
    result
}

/// The spec `sys.meta_path` reports for `name`.
pub(crate) fn meta_path_find_spec(name: &str, path: Option<&[String]>) -> Option<i64> {
    match meta_path_match(name, path)? {
        MetaMatch::Found(found) => Some(spec_instance(&found)),
        MetaMatch::Finder(spec) => Some(spec),
    }
}

/// Imports `name` through `sys.meta_path`.
///
/// A module Hydro's own finders discovered is materialised (native modules,
/// source/namespace modules on the search roots); a spec a program-defined
/// finder returned is loaded through its loader, executing the loader's
/// `exec_module` exactly like CPython's `_load`.
pub(crate) fn meta_path_import(name: &str, path: Option<&[String]>) -> Option<i64> {
    match meta_path_match(name, path)? {
        MetaMatch::Found(found) => Some(materialize_found(&found)),
        MetaMatch::Finder(spec) => Some(super::importlib::load_spec(name, spec)),
    }
}

/// The authoritative `__spec__` of a module already in `sys.modules`.
///
/// `Ok(None)` means the name is not loaded, or the loaded module value is not
/// authoritative (an extension module value with no published spec) and the
/// finders should describe it. `Ok(Some(NONE))` means
/// `sys.modules[name] is None` (CPython returns `None` from `find_spec`);
/// a module namespace with a missing or `None` `__spec__` raises the same
/// `ValueError` CPython's `importlib.util` helpers do.
fn spec_from_sys_modules(name: &str) -> Result<Option<i64>, i64> {
    let Some(module) = super::registry_lookup(name) else {
        return Ok(None);
    };
    if module == NONE {
        return Ok(Some(NONE));
    }
    // A module-like value answers through its live `__spec__` attribute:
    // native module values synthesize their spec, and a `types.ModuleType`
    // instance or a namespace published by a program-defined finder stores
    // the spec CPython would read.
    if let Some(spec) = module_attribute(module, "__spec__") {
        if spec == NONE {
            return Err(exc::raise_with_message(
                VALUE_ERROR,
                &format!("{name}.__spec__ is None"),
            ));
        }
        return Ok(Some(spec));
    }
    if exc::pending() {
        // Another failure from the attribute surface is the outcome.
        return Err(NONE);
    }
    // A value with no `__spec__` at all: CPython's util helpers raise for
    // module objects and namespaces. An extension module value owns its own
    // attribute table and is not authoritative, so the finders still describe
    // the file.
    if let Some(state) = value::as_module(module) {
        return if state.path().is_some() {
            Ok(None)
        } else {
            Err(exc::raise_with_message(
                VALUE_ERROR,
                &format!("{name}.__spec__ is not set"),
            ))
        };
    }
    if value::as_namespace(module).is_some() || is_module_type_instance(module) {
        return Err(exc::raise_with_message(
            VALUE_ERROR,
            &format!("{name}.__spec__ is not set"),
        ));
    }
    Ok(None)
}

/// Decodes the `path` argument of `_find_spec_from_path`.
///
/// `None` selects `sys.path`; a list/tuple of `str` replaces it. Anything else
/// reports the same `TypeError` the bootstrap finders produce for an invalid
/// path sequence.
fn path_arg(argument: i64) -> Result<Option<Vec<String>>, i64> {
    if argument == NONE {
        return Ok(None);
    }
    let Some(items) = value::as_list(argument).or_else(|| value::as_tuple(argument)) else {
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "_find_spec_from_path() argument 'path' must be a list or tuple of str, not '{}'",
                value::type_name(argument)
            ),
        ));
    };
    let mut path = Vec::with_capacity(items.len());
    for &item in items {
        match value::as_str(item) {
            Some(text) => path.push(text.to_string()),
            None => {
                return Err(exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "_find_spec_from_path() argument 'path' must contain only str, not '{}'",
                        value::type_name(item)
                    ),
                ))
            }
        }
    }
    Ok(Some(path))
}

/// `spec_from_loader(name, loader, *, origin=None, is_package=None)`.
///
/// Mirrors CPython's `_bootstrap.spec_from_loader`: `loader._ORIGIN` supplies
/// the origin for built-in and frozen loaders, a file loader supplies it
/// through `get_filename()` — which also makes the spec's `has_location` true
/// and populates `submodule_search_locations` from the file's directory for a
/// package — and `loader.is_package()` supplies the package flag unless the
/// caller passed it. A loader that reports neither (a plain object, a
/// `LazyLoader`) yields a module spec with `origin=None` and no
/// `submodule_search_locations`, exactly like CPython.
fn spec_from_loader(args: &[i64]) -> i64 {
    let name = match str_arg(args, 0, "spec_from_loader", "name") {
        Ok(name) => name,
        Err(raised) => return raised,
    };
    let loader = args.get(1).copied().unwrap_or(NONE);
    let origin_arg = args.get(2).copied().unwrap_or(NONE);
    let is_package_arg = args.get(3).copied().unwrap_or(NONE);

    let name_value = value::str_alloc(name.clone());
    let roots = crate::heap::root_values(&[loader, origin_arg, name_value]);
    let mut origin = origin_arg;
    // CPython consults `loader._ORIGIN` only for an *omitted* origin; an
    // explicit empty string is kept.
    if origin_arg == NONE {
        origin = loader_constant(loader, "_ORIGIN");
    }
    if exc::pending() {
        crate::heap::release_roots(roots);
        return NONE;
    }
    let mut is_package = if is_package_arg == NONE {
        None
    } else {
        Some(value::truthy(is_package_arg))
    };
    let origin_text = value::as_str(origin)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let file_loader = loader != NONE && loader_member(loader, "get_filename") != NONE;
    if exc::pending() {
        crate::heap::release_roots(roots);
        return NONE;
    }
    if origin_text.is_some() || !file_loader {
        // `loader._ORIGIN` (or an explicit origin), or a loader without
        // `get_filename()`: a plain `ModuleSpec`, no filesystem location.
        let mut is_package = is_package;
        if is_package.is_none() {
            is_package = loader_package_flag(loader, name_value);
        }
        if exc::pending() {
            crate::heap::release_roots(roots);
            return NONE;
        }
        crate::heap::release_roots(roots);
        let locations = (is_package == Some(true)).then(Vec::<String>::new);
        return machinery::new_spec(&name, loader, origin_text, locations, false);
    }
    // A file loader names its own file. `get_filename()` raising `ImportError`
    // leaves CPython's `<unknown>` placeholder in place; every other failure
    // propagates.
    let mut location = "<unknown>".to_string();
    if let Some(found) = loader_method(loader, "get_filename", &[name_value]) {
        if exc::pending() {
            if exc::pending_tag() != i64::from(IMPORT_ERROR) {
                crate::heap::release_roots(roots);
                return NONE;
            }
            exc::clear();
        } else if let Some(path) = value::as_str(found) {
            location = path.to_string();
        }
    } else if exc::pending() {
        if exc::pending_tag() != i64::from(IMPORT_ERROR) {
            crate::heap::release_roots(roots);
            return NONE;
        }
        exc::clear();
    }
    if is_package.is_none() {
        is_package = loader_package_flag(loader, name_value);
        if exc::pending() {
            crate::heap::release_roots(roots);
            return NONE;
        }
    }
    let mut locations = None;
    if is_package == Some(true) {
        let mut dirs = Vec::new();
        if location != "<unknown>" {
            if let Some(dir) = Path::new(&location).parent() {
                let dir = dir.to_string_lossy().into_owned();
                if !dir.is_empty() {
                    dirs.push(dir);
                }
            }
        }
        locations = Some(dirs);
    }
    crate::heap::release_roots(roots);
    machinery::new_spec(&name, loader, Some(location), locations, true)
}

/// `getattr(loader, name)` for a loader's optional class attribute, consuming
/// a missing attribute as `None`.
fn loader_constant(loader: i64, name: &str) -> i64 {
    let found = crate::core::methods::getattr(loader, name.as_ptr(), name.len());
    if exc::pending() {
        if exc::pending_tag() == i64::from(ATTRIBUTE_ERROR) {
            exc::clear();
        }
        return NONE;
    }
    found
}

/// `loader.is_package(name)` as a flag, or `None` when the loader has no
/// `is_package()` (or it raised `ImportError`, which CPython treats as an
/// undefined answer). Any other failure stays pending for the caller.
fn loader_package_flag(loader: i64, name_value: i64) -> Option<bool> {
    let found = loader_method(loader, "is_package", &[name_value])?;
    if exc::pending() {
        if exc::pending_tag() == i64::from(IMPORT_ERROR) {
            exc::clear();
            return None;
        }
        return None;
    }
    Some(found != NONE && value::truthy(found))
}

/// `getattr(receiver, name)` as a callable, or `None` when the attribute is
/// missing. A raised `AttributeError` is consumed; any other pending error is
/// left for the caller.
fn loader_method(receiver: i64, name: &str, args: &[i64]) -> Option<i64> {
    let function = crate::core::methods::getattr(receiver, name.as_ptr(), name.len());
    if exc::pending() {
        if exc::pending_tag() == i64::from(ATTRIBUTE_ERROR) {
            exc::clear();
        }
        return None;
    }
    if function == NONE {
        return None;
    }
    Some(crate::builtins::call(function, args.len(), args.as_ptr()))
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

    let name_value = value::str_alloc(name.clone());
    let roots = crate::heap::root_values(&[location_arg, loader_arg, search_arg, name_value]);

    // `location=None`: ask a file loader for its own filename, keeping
    // CPython's `<unknown>` placeholder when the loader has none.
    let location = if location_arg == NONE {
        let mut location = "<unknown>".to_string();
        if loader_arg != NONE {
            if let Some(found) = loader_method(loader_arg, "get_filename", &[name_value]) {
                if exc::pending() {
                    if exc::pending_tag() != i64::from(IMPORT_ERROR) {
                        crate::heap::release_roots(roots);
                        return NONE;
                    }
                    exc::clear();
                } else if let Some(path) = value::as_str(found) {
                    location = path.to_string();
                }
            } else if exc::pending() {
                if exc::pending_tag() != i64::from(IMPORT_ERROR) {
                    crate::heap::release_roots(roots);
                    return NONE;
                }
                exc::clear();
            }
        }
        location
    } else {
        match value::as_path_like(location_arg) {
            Some(text) => machinery::absolute(text),
            None => {
                crate::heap::release_roots(roots);
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "spec_from_file_location() argument 'location' must be str or \
                         os.PathLike, not '{}'",
                        value::type_name(location_arg)
                    ),
                );
            }
        }
    };

    let spec = machinery::new_spec(&name, NONE, Some(location.clone()), None, true);

    let loader = if loader_arg != NONE {
        loader_arg
    } else if location.ends_with(".py") {
        machinery::source_loader(&name, &location)
    } else if location.ends_with(".pyc") {
        machinery::sourceless_loader(&name, &location)
    } else if is_extension_path(Path::new(&location)) {
        machinery::extension_loader(&name, &location)
    } else {
        // CPython declines locations whose suffix no loader recognizes.
        crate::heap::release_roots(roots);
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
            crate::heap::release_roots(roots);
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "spec_from_file_location() argument 'submodule_search_locations' \
                     must be a list or None, not '{}'",
                    value::type_name(search_arg)
                ),
            );
        }
    } else {
        // Omitted: CPython checks the loader's own `is_package()` answer.
        let is_package = loader_package_flag(loader, name_value);
        if exc::pending() {
            crate::heap::release_roots(roots);
            return NONE;
        }
        if is_package == Some(true) {
            locations = Some(Vec::new());
        }
    }
    if let Some(mut dirs) = locations {
        if dirs.is_empty() && location != "<unknown>" {
            if let Some(dir) = Path::new(&location).parent() {
                dirs.push(dir.to_string_lossy().into_owned());
            }
        }
        machinery::set_package_locations(spec, dirs);
    }
    crate::heap::release_roots(roots);
    spec
}

/// `module_from_spec(spec)`.
fn module_from_spec(args: &[i64]) -> i64 {
    module_from_spec_value(args[0])
}

/// `module_from_spec(spec)`: the module object described by `spec`.
///
/// CPython's contract is followed exactly: the loader's `create_module()` is
/// asked first (a loader returning `None` requests the default module object),
/// a loader that defines `exec_module()` without `create_module()` raises
/// `ImportError`, and the documented metadata attributes are installed on the
/// resulting module. A loader-created module gets only the attributes it does
/// not already provide.
///
/// Hydro's module objects are native values, so the default module object is
/// a real namespace carrying `__name__`, `__loader__`, `__package__`,
/// `__spec__`, `__doc__` and, when the spec has them, `__file__`,
/// `__cached__` and `__path__`. `__path__` is the spec's own
/// `submodule_search_locations` list, so extending one is visible through the
/// other. A spec without a loader but with search locations gets CPython's
/// backward-compatibility `NamespaceLoader` (also published as `spec.loader`).
pub(crate) fn module_from_spec_value(spec: i64) -> i64 {
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
        if module != NONE {
            return module;
        }
    }
    let loader = value::instance_attr(spec, "loader").unwrap_or(NONE);
    let roots = crate::heap::root_values(&[spec, loader]);
    let create_module = if loader == NONE {
        NONE
    } else {
        loader_member(loader, "create_module")
    };
    let mut created = NONE;
    if create_module != NONE {
        created = crate::builtins::call(create_module, 1, &spec as *const i64);
        if exc::pending() {
            crate::heap::release_roots(roots);
            return created;
        }
    } else if loader != NONE && loader_member(loader, "exec_module") != NONE {
        crate::heap::release_roots(roots);
        return exc::raise_with_message(
            IMPORT_ERROR,
            "loaders that define exec_module() must also define create_module()",
        );
    }
    let created_roots = crate::heap::root_values(&[created]);
    let metadata = module_metadata(spec, &name);
    let metadata_roots =
        crate::heap::root_values(&metadata.iter().map(|(_, value)| *value).collect::<Vec<_>>());
    let module = if created != NONE {
        initialize_module(created, metadata);
        created
    } else {
        value::namespace_new(metadata)
    };
    crate::heap::release_roots(metadata_roots);
    crate::heap::release_roots(created_roots);
    crate::heap::release_roots(roots);
    module
}

/// The metadata attributes `module_from_spec` installs, in CPython's order.
fn module_metadata(spec: i64, name: &str) -> Vec<(String, i64)> {
    let mut loader = value::instance_attr(spec, "loader").unwrap_or(NONE);
    let search_locations = machinery::spec_search_locations_value(spec);
    let mut namespace_loader = false;
    if loader == NONE {
        if let Some(path) = search_locations {
            // CPython's backward-compatibility rule: a namespace spec reports
            // a `NamespaceLoader` whose `_path` is the search-location list.
            let created = value::instance_new(machinery::namespace_loader_class());
            let roots = crate::heap::root_values(&[created, path]);
            value::instance_setattr(created, "_path".to_string(), path);
            machinery::set_spec_loader(spec, created);
            crate::heap::release_roots(roots);
            loader = created;
            namespace_loader = true;
        }
    }
    let parent = value::instance_attr(spec, "parent")
        .and_then(value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    let has_location = machinery::spec_has_location(spec);
    let mut metadata: Vec<(String, i64)> = Vec::with_capacity(9);
    metadata.push(("__name__".to_string(), value::str_alloc(name.to_string())));
    metadata.push(("__loader__".to_string(), loader));
    metadata.push(("__package__".to_string(), value::str_alloc(parent)));
    metadata.push(("__spec__".to_string(), spec));
    metadata.push(("__doc__".to_string(), NONE));
    if has_location {
        if let Some(origin) = machinery::spec_origin(spec) {
            metadata.push(("__file__".to_string(), value::str_alloc(origin)));
        }
        if let Some(cached) = value::instance_attr(spec, "cached") {
            metadata.push(("__cached__".to_string(), cached));
        }
    } else if namespace_loader {
        metadata.push(("__file__".to_string(), NONE));
    }
    if let Some(path) = search_locations {
        metadata.push(("__path__".to_string(), path));
    }
    metadata
}

/// Installs `metadata` on a loader-created module, keeping any attribute the
/// loader already provided (CPython's `_init_module_attrs(override=False)`).
fn initialize_module(module: i64, metadata: Vec<(String, i64)>) {
    if let Some(state) = value::as_module(module) {
        let name = state.name().to_string();
        // A native module synthesizes `__name__`, `__loader__`, `__package__`
        // and `__spec__` from the central table; only the filesystem-facing
        // attributes (`__path__` of a native package) come from the spec.
        let native = hydro_ir::stdlib::module(&name).is_some();
        for (attribute, item) in metadata {
            if native
                && matches!(
                    attribute.as_str(),
                    "__name__" | "__loader__" | "__package__" | "__spec__"
                )
            {
                continue;
            }
            if globals::try_get(&name, &attribute).is_none() {
                globals::set(&name, &attribute, item);
            }
        }
        return;
    }
    if let Some(mut pairs) = value::namespace_pairs_mut(module) {
        let mut added = Vec::new();
        for (attribute, item) in metadata {
            if !pairs.pairs.iter().any(|(key, _)| key == &attribute) {
                pairs.pairs.push((attribute, item));
                added.push(item);
            }
        }
        drop(pairs);
        // The namespace now references the values; publish the collector's
        // write barrier exactly like every other namespace mutation.
        for item in added {
            crate::heap::write_barrier_edge(module, item);
        }
        return;
    }
    if value::instance_class_value(module).is_some() {
        for (attribute, item) in metadata {
            if value::instance_attr(module, &attribute).is_none() {
                value::instance_setattr(module, attribute, item);
            }
        }
    }
}

/// `getattr(loader, name)`, or `NONE` when the loader does not provide it.
fn loader_member(loader: i64, name: &str) -> i64 {
    let found = crate::core::methods::getattr(loader, name.as_ptr(), name.len());
    if exc::pending() {
        if exc::pending_tag() == i64::from(ATTRIBUTE_ERROR) {
            exc::clear();
        }
        return NONE;
    }
    found
}

/// Materialises the module `found` describes and publishes it in
/// `sys.modules`, returning the module object.
///
/// A unit compiled into the program keeps its namespace in the runtime global
/// store, so a fresh module value exposes it exactly like the original one.
/// Any other module (pure-Python source, namespace package) becomes a real
/// module namespace carrying the discovered spec's metadata; a discovered
/// source module's body is then executed through the runtime compiler so its
/// bindings are real, while a namespace package has no body to run.
pub(crate) fn materialize_found(found: &FoundSpec) -> i64 {
    let name = found.name.as_str();
    if found.loader == LoaderKind::Extension {
        if let Some(module) = crate::modules::extension::import_module(name) {
            return module;
        }
        let origin = found.origin.as_deref().unwrap_or(name);
        return exc::raise_with_message(
            IMPORT_ERROR,
            &format!("cannot load extension module '{name}' from '{origin}'"),
        );
    }
    if globals::try_get(name, "__name__").is_some() {
        return super::module_value_by_name(name);
    }
    let spec = spec_instance(found);
    let module = module_from_spec_value(spec);
    if exc::pending() {
        return module;
    }
    super::publish_module(name, module);
    // A source module discovered on the search roots has not run: execute its
    // body now through the runtime compiler. Namespace packages have no body
    // and compiled modules already ran (the loader reports the latter as a
    // no-op), so only the discovered source loader reaches this path.
    if found.loader == LoaderKind::Source {
        machinery::exec_loaded_module(module);
        if exc::pending() {
            return module;
        }
    }
    module
}

/// Materialises the module `name` resolves to, or `None` when no finder knows
/// it. A name already in `sys.modules` is returned as-is.
pub(crate) fn materialize_module(name: &str) -> Option<i64> {
    if let Some(module) = super::registry_lookup(name) {
        if module != NONE {
            return Some(module);
        }
    }
    resolve_module(name).map(|found| materialize_found(&found))
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
            &format!("no package specified for '{name}' (required for relative module names)"),
        ));
    };
    let dots = name.bytes().take_while(|byte| *byte == b'.').count();
    let rest = &name[dots..];
    let mut parts: Vec<&str> = package.split('.').filter(|part| !part.is_empty()).collect();
    let up = dots - 1;
    // `_resolve_name` removes `level - 1` components from the package; the
    // base package itself must survive.
    if up >= parts.len() {
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
    // CPython's `LazyLoader.__init__` rejects a loader without `exec_module()`
    // before storing it.
    if lazy_lookup(wrapped, "exec_module") == NONE {
        return exc::raise_with_message(
            TYPE_ERROR,
            "LazyLoader(loader) requires loader.exec_module()",
        );
    }
    value::instance_setattr(receiver, "loader".to_string(), wrapped);
    NONE
}

/// `LazyLoader.create_module(spec)`: the wrapped loader supplies the module.
fn lazy_create_module(args: &[i64]) -> i64 {
    let receiver = args[0];
    let spec = args[1];
    let loader = value::instance_attr(receiver, "loader").unwrap_or(NONE);
    if loader == NONE {
        return NONE;
    }
    if let Some(created) = lazy_call(loader, "create_module", &[spec]) {
        return created;
    }
    if exc::pending() {
        return NONE;
    }
    NONE
}

/// `LazyLoader.exec_module(module)`.
///
/// CPython defers the call until the module's first attribute access. Hydro
/// modules are already initialized when compiled in, so the load happens here:
/// the wrapped loader's `exec_module` runs (a real no-op for a compiled
/// module), and anything else reports the missing runtime compiler.
fn lazy_exec_module(args: &[i64]) -> i64 {
    let receiver = args[0];
    let module = args[1];
    let loader = value::instance_attr(receiver, "loader").unwrap_or(NONE);
    if loader != NONE {
        if let Some(result) = lazy_call(loader, "exec_module", &[module]) {
            return result;
        }
        if exc::pending() {
            return NONE;
        }
    }
    machinery::exec_loaded_module(module)
}

/// `getattr(receiver, name)`, clearing a missing attribute to `NONE`.
fn lazy_lookup(receiver: i64, name: &str) -> i64 {
    let found = crate::core::methods::getattr(receiver, name.as_ptr(), name.len());
    if exc::pending() {
        if exc::pending_tag() == i64::from(ATTRIBUTE_ERROR) {
            exc::clear();
        }
        return NONE;
    }
    found
}

/// Calls `receiver.name(*args)`, or `None` when the method is missing.
fn lazy_call(receiver: i64, name: &str, args: &[i64]) -> Option<i64> {
    let function = lazy_lookup(receiver, name);
    if exc::pending() {
        return None;
    }
    if function == NONE {
        return None;
    }
    Some(crate::builtins::call(function, args.len(), args.as_ptr()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn text(item: &str) -> i64 {
        value::str_alloc(item.to_string())
    }

    #[test]
    fn magic_number_matches_cpython_314_bytes() {
        exc::init();
        let first = constant("MAGIC_NUMBER");
        let second = constant("MAGIC_NUMBER");
        assert!(!exc::pending());
        assert_eq!(first, second, "MAGIC_NUMBER identity must be stable");
        let bytes = value::as_bytes(first).expect("bytes");
        // `_imp.pyc_magic_number_token.to_bytes(4, 'little')` for 3627.
        assert_eq!(bytes.to_vec(), vec![0x2B, 0x0E, 0x0D, 0x0A]);
    }

    #[test]
    fn cache_from_source_matches_cpython_layout() {
        exc::init();
        let cases = [
            ("/tmp/a.py", "/tmp/__pycache__/a.cpython-314.pyc"),
            ("a.py", "__pycache__/a.cpython-314.pyc"),
            (
                "/tmp/pkg/__init__.py",
                "/tmp/pkg/__pycache__/__init__.cpython-314.pyc",
            ),
            ("/tmp/noext", "/tmp/__pycache__/noextcpython-314.pyc"),
            ("/tmp/a.pyc", "/tmp/__pycache__/a.cpython-314.pyc"),
        ];
        for (path, expected) in cases {
            let cached = call(CACHE_FROM_SOURCE, &[text(path)]);
            assert!(
                !exc::pending(),
                "{path}: {}",
                value::to_string(exc::pending_value())
            );
            assert_eq!(value::as_str(cached), Some(expected), "{path}");
        }

        // An explicit optimization level becomes the `.opt-<level>` infix.
        let optimized = call(CACHE_FROM_SOURCE, &[text("/tmp/a.py"), NONE, text("2")]);
        assert_eq!(
            value::as_str(optimized),
            Some("/tmp/__pycache__/a.cpython-314.opt-2.pyc")
        );
        exc::clear();
        call(CACHE_FROM_SOURCE, &[text("/tmp/a.py"), NONE, text("x-y")]);
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'x-y' is not alphanumeric"
        );

        // A non-path argument reports CPython's `os.fspath` error.
        exc::clear();
        call(CACHE_FROM_SOURCE, &[crate::ops::int_result(1)]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::clear();
    }

    #[test]
    fn source_from_cache_inverts_cpython_layout() {
        exc::init();
        let cases = [
            ("/tmp/__pycache__/a.cpython-314.pyc", Some("/tmp/a.py")),
            ("__pycache__/a.cpython-314.pyc", Some("a.py")),
            (
                "/tmp/pkg/__pycache__/__init__.cpython-314.opt-2.pyc",
                Some("/tmp/pkg/__init__.py"),
            ),
            // A `.pyc` directly outside `__pycache__` is not a cache path.
            ("/tmp/a.pyc", None),
            // Wrong number of dots.
            ("/tmp/__pycache__/a.pyc", None),
            ("/tmp/__pycache__/a.b.c.d.pyc", None),
            // The optimization field must be `opt-<alnum>`.
            (
                "/tmp/__pycache__/a.cpython-314.opt-1.pyc",
                Some("/tmp/a.py"),
            ),
            ("/tmp/__pycache__/a.cpython-314.opt-x-y.pyc", None),
            ("/tmp/__pycache__/a.cpython-314.opt.pyc", None),
            ("/tmp/__pycache__/a.cpython-314.foo-1.pyc", None),
        ];
        for (path, expected) in cases {
            exc::clear();
            let source = call(SOURCE_FROM_CACHE, &[text(path)]);
            match expected {
                Some(expected) => {
                    assert!(
                        !exc::pending(),
                        "{path}: {}",
                        value::to_string(exc::pending_value())
                    );
                    assert_eq!(value::as_str(source), Some(expected), "{path}");
                }
                None => {
                    assert_eq!(
                        exc::pending_tag(),
                        i64::from(VALUE_ERROR),
                        "{path} must be rejected"
                    );
                }
            }
        }
        exc::clear();

        // A non-string path is a `TypeError` like CPython's `os.fspath`.
        let result = call(SOURCE_FROM_CACHE, &[crate::ops::int_result(1)]);
        assert_eq!(result, NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::clear();
    }

    #[test]
    fn source_hash_matches_cpython_siphash13() {
        exc::init();
        // Oracle values from `_imp.source_hash(_imp.pyc_magic_number_token, b)`.
        let cases: [(&[u8], [u8; 8]); 4] = [
            (b"", [0x61, 0xf5, 0xb7, 0xcb, 0x65, 0x3a, 0x6a, 0xa4]),
            (b"a", [0x97, 0x13, 0xde, 0xd0, 0xc3, 0x32, 0x43, 0x73]),
            (b"abc", [0x6e, 0x46, 0x1b, 0xf6, 0xa5, 0xe9, 0x7b, 0x4a]),
            (
                b"def f():\n    return 1\n",
                [0x34, 0x85, 0x3d, 0x99, 0x3c, 0xed, 0x1a, 0x39],
            ),
        ];
        for (source, expected) in cases {
            let digest = call(SOURCE_HASH, &[value::bytes_new(source.to_vec())]);
            assert!(
                !exc::pending(),
                "{}",
                value::to_string(exc::pending_value())
            );
            let bytes = value::as_bytes(digest).expect("bytes digest");
            assert_eq!(bytes, &expected[..], "{source:?}");
        }

        // Only bytes-like sources are accepted.
        exc::clear();
        call(SOURCE_HASH, &[text("abc")]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "a bytes-like object is required, not 'str'"
        );
        exc::clear();
    }

    #[test]
    fn find_spec_describes_the_entry_module() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        crate::core::globals::reset();
        exc::init();
        let spec = call("find_spec", &[text("__main__")]);
        assert!(
            !exc::pending(),
            "{}",
            value::to_string(exc::pending_value())
        );
        assert!(machinery::is_spec(spec));
        assert_eq!(machinery::spec_name(spec).as_deref(), Some("__main__"));
        crate::core::globals::reset();
        exc::init();
    }

    #[test]
    fn find_spec_from_path_searches_only_the_given_path() {
        exc::init();
        let dir = std::env::temp_dir().join(format!(
            "hydro-importlib-util-{}-{}",
            std::process::id(),
            "find-spec"
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        let module = dir.join("hydro_probe_module.py");
        fs::write(&module, b"value = 1\n").expect("write module");

        let path = value::list_new(vec![text(&dir.to_string_lossy())]);
        let args = [text("hydro_probe_module"), path];
        let spec = call("_find_spec_from_path", &args);
        assert!(!exc::pending());
        assert!(machinery::is_spec(spec));
        assert_eq!(
            machinery::spec_name(spec).as_deref(),
            Some("hydro_probe_module")
        );

        // A missing module on the explicit path stays `None`.
        let missing_args = [text("hydro_probe_absent"), value::list_new(Vec::new())];
        assert_eq!(call("_find_spec_from_path", &missing_args), NONE);
        assert!(!exc::pending());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_spec_from_path_rejects_invalid_path_sequences() {
        exc::init();
        let args = [
            text("hydro_probe_module"),
            value::encode_int(1).expect("small int"),
        ];
        assert_eq!(call("_find_spec_from_path", &args), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
    }
}
