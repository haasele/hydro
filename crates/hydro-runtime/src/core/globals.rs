//! Module-level globals.
//!
//! Hydro compiles each module body straight-line and keeps no Python module
//! object for user modules, so `global`/module-scope names need a side store
//! that survives across functions. The store is thread-local, keyed by
//! `module + "\0" + name`, mirroring CPython's per-module dictionaries.
//!
//! Values are owned by Rust memory the collector cannot scan, so every stored
//! value is also installed as a thread root. Conservative retention is enough
//! for this slice: replaced values stay rooted until the runtime resets.
//!
//! Module attribute assignments (`module.name = value`) need a per-module
//! shadow of the native spec, which lives in a reserved pseudo-module of the
//! same map (see [`MODULE_ATTR_MODULE`]); [`reset`] clears it with the rest of
//! a program run.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};

use crate::exc::{self, NAME_ERROR};
use crate::heap;
use crate::value::NONE;

thread_local! {
    /// Module globals of the current thread.
    static GLOBALS: RefCell<HashMap<String, i64>> = RefCell::new(HashMap::new());

    /// Whether any module attribute override was stored on this thread.
    ///
    /// Attribute resolution runs on every module read and direct module call;
    /// the flag keeps the common (no overrides) case to one thread-local
    /// boolean instead of a store lookup.
    static HAS_ATTRIBUTE_OVERRIDES: Cell<bool> = const { Cell::new(false) };
}

/// Pseudo-module that owns runtime cache entries kept in the globals store.
///
/// The name starts with NUL, which cannot occur in a Python module name or
/// identifier, so [`module_names`] never reports it and no user module can
/// collide with it. Values stored here are rooted and cleared exactly like
/// ordinary module globals, which is what makes [`reset`] invalidate the
/// module-registry caches without a separate ABI entry point.
const REGISTRY_MODULE: &str = "\0hydro.registry";

/// Pseudo-module that owns module attribute overrides.
///
/// A native module's attributes come from the central spec, not from a mutable
/// namespace, so `module.name = value` needs a side table that takes
/// precedence over the spec default. The reserved name starts with NUL for the
/// same reason as [`REGISTRY_MODULE`]: no Python module can collide with it.
const MODULE_ATTR_MODULE: &str = "\0hydro.module.attrs";

/// Builds the store key. The NUL separator cannot occur in a Python
/// identifier, so different `(module, name)` pairs never collide.
fn key(module: &str, name: &str) -> String {
    let mut key = String::with_capacity(module.len() + 1 + name.len());
    key.push_str(module);
    key.push('\0');
    key.push_str(name);
    key
}

/// `module.name = value`: inserts or replaces the global.
pub(crate) fn set(module: &str, name: &str, value: i64) {
    GLOBALS.with(|globals| {
        globals.borrow_mut().insert(key(module, name), value);
    });
    // The map lives in Rust memory the collector cannot trace; keep the value
    // alive for as long as the entry (and, conservatively, beyond it).
    heap::add_thread_root(value);
}

/// Reads `module.name` from the store only, without raising; used by runtime
/// import internals and by [`sync_view`], which must not observe view-only
/// bindings. Public reads use [`get`]/[`lookup`], which also consult the
/// cached `globals()` view.
pub(crate) fn try_get(module: &str, name: &str) -> Option<i64> {
    GLOBALS.with(|globals| globals.borrow().get(&key(module, name)).copied())
}

/// Reads `module.name`, consulting the store and then the cached `globals()`
/// view (see [`lookup`]). A missing entry raises CPython's `NameError`
/// (`name '<name>' is not defined`) and returns `None`.
pub(crate) fn get(module: &str, name: &str) -> i64 {
    match lookup(module, name) {
        Some(value) => value,
        None => exc::raise_with_message(NAME_ERROR, &format!("name '{name}' is not defined")),
    }
}

/// Forgets every stored global (a fresh program run).
///
/// The map also carries the runtime module-registry caches (see
/// [`REGISTRY_MODULE`]); clearing it therefore invalidates `sys.modules`,
/// `sys.path` and every other cached module value as well.
pub(crate) fn reset() {
    GLOBALS.with(|globals| globals.borrow_mut().clear());
    HAS_ATTRIBUTE_OVERRIDES.with(|flag| flag.set(false));
}

/// Names of the user modules that own at least one stored global, sorted
/// lexicographically.
///
/// This is the discovery source for the runtime module registry: a compiled
/// user module has no native spec, so its entries in the globals store are the
/// evidence that it was loaded. The reserved registry pseudo-module is
/// excluded.
pub(crate) fn module_names() -> Vec<String> {
    GLOBALS.with(|globals| {
        let mut names = BTreeSet::new();
        for key in globals.borrow().keys() {
            if let Some((module, _)) = key.split_once('\0') {
                if !module.is_empty() {
                    names.insert(module.to_string());
                }
            }
        }
        names.into_iter().collect()
    })
}

/// Reads a runtime cache entry owned by [`REGISTRY_MODULE`].
pub(crate) fn registry_get(name: &str) -> Option<i64> {
    try_get(REGISTRY_MODULE, name)
}

/// Stores a runtime cache entry owned by [`REGISTRY_MODULE`].
pub(crate) fn registry_set(name: &str, value: i64) {
    set(REGISTRY_MODULE, name, value);
}

/// The stored attribute override of `module.name`, if the program assigned
/// one.
///
/// Overrides shadow the native spec default for reads, calls and constants;
/// [`attribute_overrides`] enumerates them for `vars(module)`/`dir(module)`.
pub(crate) fn attribute_override(module: &str, name: &str) -> Option<i64> {
    if !HAS_ATTRIBUTE_OVERRIDES.with(|flag| flag.get()) {
        return None;
    }
    try_get(MODULE_ATTR_MODULE, &key(module, name))
}

/// `module.name = value`: records the override, keeping `value` rooted for as
/// long as the entry lives.
pub(crate) fn set_attribute_override(module: &str, name: &str, value: i64) {
    HAS_ATTRIBUTE_OVERRIDES.with(|flag| flag.set(true));
    set(MODULE_ATTR_MODULE, &key(module, name), value);
}

/// `del module.name` for an overridden attribute: drops the override, which
/// makes the native spec default (or the absence of the name) visible again.
pub(crate) fn remove_attribute_override(module: &str, name: &str) -> bool {
    if !HAS_ATTRIBUTE_OVERRIDES.with(|flag| flag.get()) {
        return false;
    }
    remove(MODULE_ATTR_MODULE, &key(module, name))
}

/// Every `(name, value)` attribute override of `module`, sorted by name.
pub(crate) fn attribute_overrides(module: &str) -> Vec<(String, i64)> {
    if !HAS_ATTRIBUTE_OVERRIDES.with(|flag| flag.get()) {
        return Vec::new();
    }
    let module_prefix = format!("{module}\0");
    let store_prefix = format!("{MODULE_ATTR_MODULE}\0");
    GLOBALS.with(|globals| {
        let globals = globals.borrow();
        let mut entries: Vec<(String, i64)> = globals
            .iter()
            .filter_map(|(key, value)| {
                let name = key
                    .strip_prefix(store_prefix.as_str())?
                    .strip_prefix(module_prefix.as_str())?;
                Some((name.to_string(), *value))
            })
            .collect();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        entries
    })
}

/// Removes `module.name`, reporting whether the binding existed.
///
/// The cached `globals()` view is updated too: a live view must not keep
/// reporting a name the program deleted.
pub(crate) fn remove(module: &str, name: &str) -> bool {
    let removed = GLOBALS.with(|globals| globals.borrow_mut().remove(&key(module, name)).is_some());
    if removed {
        remove_from_view(module, name);
    }
    removed
}

/// Drops `name` from the cached `globals()` view of `module`, if present.
fn remove_from_view(module: &str, name: &str) {
    let Some(dict) = registry_get(&view_key(module)) else {
        return;
    };
    let present = crate::value::as_dict(dict)
        .map(|entries| {
            entries
                .iter()
                .any(|(key, _)| crate::value::as_str(*key) == Some(name))
        })
        .unwrap_or(false);
    if !present {
        return;
    }
    // Root the synthetic key before locking the dict: allocation may collect.
    let key = crate::value::str_alloc(name.to_string());
    let roots = heap::root_values(&[key]);
    if let Some(mut entries) = crate::value::dict_pairs_mut(dict) {
        let _ = entries.remove(key);
    }
    heap::release_roots(roots);
}

/// `del module.name`: removes the binding and returns the ABI dummy value.
/// A missing binding raises `NameError`, like CPython's `del` of an undefined
/// module name. An attribute override stored for the same name is dropped too,
/// so module-scope `del` cannot leave a stale shadow behind.
pub(crate) fn delete_value(module: &str, name: &str) -> i64 {
    let removed = remove(module, name);
    let overridden = remove_attribute_override(module, name);
    if removed || overridden {
        NONE
    } else {
        exc::raise_with_message(NAME_ERROR, &format!("name '{name}' is not defined"))
    }
}

/// Stores `value` and returns the ABI dummy value (`None`).
///
/// A module attribute override already stored for the name is refreshed so
/// `module.name` cannot keep reporting a stale value after module-scope code
/// rebinds the same global.
pub(crate) fn set_value(module: &str, name: &str, value: i64) -> i64 {
    set(module, name, value);
    if attribute_override(module, name).is_some() {
        set_attribute_override(module, name, value);
    }
    NONE
}

/// Every `(name, value)` binding of `module`, sorted by name.
///
/// The runtime owns no per-module dict object, so introspection
/// (`globals()`, `vars(module)`, `dir(module)`) reads this side store. The
/// reserved registry pseudo-module is a different module name, so its cache
/// entries never appear here.
pub(crate) fn entries(module: &str) -> Vec<(String, i64)> {
    let prefix = format!("{module}\0");
    GLOBALS.with(|globals| {
        let globals = globals.borrow();
        let mut entries: Vec<(String, i64)> = globals
            .iter()
            .filter_map(|(key, value)| {
                key.strip_prefix(prefix.as_str())
                    .map(|name| (name.to_string(), *value))
            })
            .collect();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        entries
    })
}

/// Binding names of `module`, sorted; see [`entries`].
pub(crate) fn names(module: &str) -> Vec<String> {
    entries(module).into_iter().map(|(name, _)| name).collect()
}

/// Registry key of the live `globals()` dict cached for `module`.
fn view_key(module: &str) -> String {
    format!("globals.view:{module}")
}

/// The live `globals()` mapping of `module`, created on first use.
///
/// One `dict` is cached per module (CPython exposes a stable module
/// `__dict__`), so `globals() is globals()` and item writes through the same
/// view accumulate. Every call refreshes the cached dict from the runtime
/// global store first, then publishes keys that only exist in the view back
/// into the store: `g = globals(); g["x"] = v` is therefore visible to later
/// static reads and `getattr(module, "x")`.
pub(crate) fn view(module: &str) -> i64 {
    if let Some(cached) = registry_get(&view_key(module)) {
        sync_view(module, cached);
        return cached;
    }
    // Publish the empty dict before filling it: the registry entry is a GC
    // root, so every key and value inserted below stays reachable.
    let dict = crate::value::dict_new(Vec::new());
    registry_set(&view_key(module), dict);
    sync_view(module, dict);
    dict
}

/// Refreshes `dict` from the global store, then stores view-only keys.
fn sync_view(module: &str, dict: i64) {
    for (name, value) in entries(module) {
        let key = crate::value::str_alloc(name);
        let roots = heap::root_values(&[key, value]);
        crate::container::dict_set(dict, key, value);
        heap::release_roots(roots);
    }
    let Some(entries) = crate::value::as_dict(dict) else {
        return;
    };
    // Snapshot before mutating the side store: `set` installs a root and must
    // not run while the dict slice is borrowed through a re-entrant path.
    let pending: Vec<(String, i64)> = entries
        .iter()
        .filter_map(|(key, value)| {
            let name = crate::value::as_str(*key)?;
            try_get(module, name)
                .is_none()
                .then(|| (name.to_string(), *value))
        })
        .collect();
    for (name, value) in pending {
        set(module, &name, value);
    }
}

/// Reads `module.name` from the store, falling back to the cached
/// `globals()` view for names a program bound with `globals()[name] = value`.
pub(crate) fn lookup(module: &str, name: &str) -> Option<i64> {
    if let Some(value) = try_get(module, name) {
        return Some(value);
    }
    let cached = registry_get(&view_key(module))?;
    crate::value::as_dict(cached)?
        .iter()
        .find_map(|(key, value)| (crate::value::as_str(*key) == Some(name)).then_some(*value))
}

/// Names currently held by the cached `globals()` view of `module` (empty
/// when no view was materialised yet).
pub(crate) fn view_names(module: &str) -> Vec<String> {
    let Some(dict) = registry_get(&view_key(module)) else {
        return Vec::new();
    };
    let Some(entries) = crate::value::as_dict(dict) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|(key, _)| crate::value::as_str(*key).map(str::to_string))
        .collect()
}

/// Publishes `name = value` into the cached `globals()` view of `module`, if a
/// view was already materialised.
///
/// `module.name = value` stores the binding in the side store first; this keeps
/// a live `globals()` mapping in step without waiting for the next call.
pub(crate) fn set_in_view(module: &str, name: &str, value: i64) {
    let Some(dict) = registry_get(&view_key(module)) else {
        return;
    };
    let key = crate::value::str_alloc(name.to_string());
    let roots = heap::root_values(&[key, value]);
    crate::container::dict_set(dict, key, value);
    heap::release_roots(roots);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container;
    use crate::value::{self, encode_int};

    fn int(n: i64) -> i64 {
        encode_int(n).expect("representable")
    }

    fn pending_message() -> String {
        value::to_string(exc::pending_value())
    }

    #[test]
    fn set_and_get_roundtrip() {
        reset();
        exc::init();
        assert_eq!(set_value("app", "counter", int(7)), NONE);
        assert_eq!(get("app", "counter"), int(7));
        assert!(!exc::pending());

        // Replacing keeps the latest value.
        set("app", "counter", int(8));
        assert_eq!(get("app", "counter"), int(8));
    }

    #[test]
    fn missing_global_raises_name_error() {
        reset();
        exc::init();
        assert_eq!(get("app", "missing"), NONE);
        assert_eq!(exc::pending_tag(), NAME_ERROR as i64);
        assert_eq!(pending_message(), "name 'missing' is not defined");
    }

    #[test]
    fn get_falls_back_to_globals_view() {
        reset();
        exc::init();
        // `g = globals(); g["x"] = 1` writes into the cached view without a
        // further `globals()` call, so the side store has no entry yet.
        let g = view("mod");
        let key = value::str_alloc("x".to_string());
        let roots = heap::root_values(&[g, key]);
        container::dict_set(g, key, int(1));
        heap::release_roots(roots);
        assert_eq!(try_get("mod", "x"), None);
        assert_eq!(get("mod", "x"), int(1));
        assert!(!exc::pending());

        // Names absent from both the store and the view still raise.
        assert_eq!(get("mod", "absent"), NONE);
        assert_eq!(exc::pending_tag(), NAME_ERROR as i64);
        assert_eq!(pending_message(), "name 'absent' is not defined");
    }

    #[test]
    fn modules_are_separate_namespaces() {
        reset();
        exc::init();
        set("first", "value", int(1));
        set("second", "value", int(2));
        assert_eq!(get("first", "value"), int(1));
        assert_eq!(get("second", "value"), int(2));
        // A name only resolves in its own module.
        assert_eq!(get("third", "value"), NONE);
        assert_eq!(exc::pending_tag(), NAME_ERROR as i64);

        exc::init();
        reset();
        assert_eq!(get("first", "value"), NONE);
        assert_eq!(exc::pending_tag(), NAME_ERROR as i64);
    }

    #[test]
    fn module_names_are_sorted_and_exclude_registry_entries() {
        reset();
        set("zebra", "value", int(1));
        set("alpha", "value", int(2));
        set("alpha", "other", int(3));
        registry_set("sys.modules", int(4));
        assert_eq!(
            module_names(),
            vec!["alpha".to_string(), "zebra".to_string()]
        );

        reset();
        assert!(module_names().is_empty());
        assert_eq!(registry_get("sys.modules"), None);
    }

    #[test]
    fn reset_invalidates_registry_entries() {
        reset();
        registry_set("sys.path", int(11));
        assert_eq!(registry_get("sys.path"), Some(int(11)));
        reset();
        assert_eq!(registry_get("sys.path"), None);
    }
}
