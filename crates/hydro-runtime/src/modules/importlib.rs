//! `importlib`: dynamic imports, reload and cache invalidation.
//!
//! Hydro resolves ordinary imports statically. `import_module` exists for code
//! that selects a module name at runtime: the runtime registry (`sys.modules`)
//! knows every native module from `hydro_ir::stdlib` and every loaded user
//! module discovered in the globals store, and import returns that exact
//! module value so identity matches `sys.modules[name]`.
//!
//! The dotted submodules live in their own native modules
//! ([`importlib_util`](super::importlib_util),
//! [`importlib_machinery`](super::importlib_machinery),
//! [`importlib_metadata`](super::importlib_metadata) and
//! [`importlib_resources`](super::importlib_resources)), so importing
//! `importlib.metadata` never compiles CPython's package files.
//!
//! `reload` re-imports native modules (the built-in loader's documented
//! no-op) and reports a precise `ImportError` for compiled Python modules,
//! which have no runtime execution engine. Names outside the registry fail
//! with `ModuleNotFoundError`. The only remaining placeholder is the generated
//! Unicode width tables (`rich._unicode_data.unicodeX`): their synthesized
//! `cell_table` reports a single default-width range for every codepoint
//! instead of failing the import. The real per-width tables are not bundled.

use crate::exc::{self, IMPORT_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value;

/// Placeholder modules that are still synthesized on demand.
const UNICODE_TABLE_PREFIX: &str = "rich._unicode_data";

/// Dispatches a native `importlib` function. Arity is enforced by the central
/// spec before this point.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "import_module" => import_module(
            args.first().copied().unwrap_or(value::NONE),
            args.get(1).copied().unwrap_or(value::NONE),
        ),
        "reload" => reload(args.first().copied().unwrap_or(value::NONE)),
        "invalidate_caches" => {
            super::importlib_metadata::invalidate_caches();
            value::NONE
        }
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module 'importlib' has no attribute '{function}'"),
        ),
    }
}

/// `importlib.reload(module)`.
///
/// CPython asks the module's loader to execute the module body again. Hydro
/// compiles module bodies ahead of time, so a pure-Python module cannot be
/// re-executed at runtime: re-running it would either be a silent no-op or a
/// partially reinitialized module, both worse than failing. Native modules are
/// the exception — their loader is the built-in importer, whose reload is a
/// documented no-op returning the same module.
fn reload(module_arg: i64) -> i64 {
    let Some(module) = value::as_module(module_arg) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "reload() argument must be a module, not '{}'",
                value::type_name(module_arg)
            ),
        );
    };
    let name = module.name().to_string();
    if super::registry_lookup(&name) != Some(module_arg) {
        return exc::raise_with_message(IMPORT_ERROR, &format!("module {name} not in sys.modules"));
    }
    if hydro_ir::stdlib::module(&name).is_some() {
        // Built-in loader: `exec_module` is a no-op, so reload returns the
        // same module object.
        return module_arg;
    }
    if name == UNICODE_TABLE_PREFIX || name.starts_with("rich._unicode_data.") {
        return unicode_table_placeholder();
    }
    exc::raise_with_message(
        IMPORT_ERROR,
        &format!(
            "cannot reload '{name}': Hydro compiles Python modules ahead of time \
             and has no runtime execution engine for module bodies"
        ),
    )
}

/// `importlib.import_module(name, package=None)`.
///
/// A relative name (`".sub"`, `"..sibling"`) is resolved against `package`
/// first, exactly like CPython. The resolved dotted name is then looked up in
/// the runtime module registry, which contains every compiled user module
/// (including package submodules), every native module and every module value
/// materialized by an extension import. Names outside the registry raise
/// `ModuleNotFoundError` with the exact requested name.
fn import_module(name_arg: i64, package_arg: i64) -> i64 {
    let Some(name) = value::as_str(name_arg) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "import_module() argument must be str, not {}",
                value::type_name(name_arg)
            ),
        );
    };
    if name.is_empty() {
        return exc::raise_with_message(VALUE_ERROR, "Empty module name");
    }
    let name = match resolve_relative(name, package_arg) {
        Ok(name) => name,
        Err(raised) => return raised,
    };
    if name.split('.').any(|part| part.is_empty()) {
        return crate::builtins::module_not_found(&name);
    }
    if let Some(module) = super::registry_lookup(&name) {
        return module;
    }
    if name == UNICODE_TABLE_PREFIX || name.starts_with("rich._unicode_data.") {
        return unicode_table_placeholder();
    }
    // Unknown names never become a fake namespace: callers rely on the import
    // failing so their fallback/probe logic runs.
    crate::builtins::module_not_found(&name)
}

/// Resolves a relative module name against `package`.
///
/// One leading dot names the package itself, two dots its parent, and so on;
/// the remainder is appended as a dotted suffix. Absolute names pass through
/// unchanged. A relative name without a package raises `TypeError`, and
/// escaping the top-level package raises `ImportError`, matching CPython.
fn resolve_relative(name: &str, package_arg: i64) -> Result<String, i64> {
    if !name.starts_with('.') {
        return Ok(name.to_string());
    }
    let Some(package) = value::as_str(package_arg) else {
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            "package argument required for relative module names",
        ));
    };
    if package.is_empty() {
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            "package argument required for relative module names",
        ));
    }
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
    if parts.is_empty() {
        return Err(exc::raise_with_message(VALUE_ERROR, "Empty module name"));
    }
    Ok(parts.join("."))
}

/// Placeholder for the generated Unicode width tables.
///
/// The table exposes the `CellTable` fields as a namespace; one full-width
/// range keeps the binary search in `rich.cells` valid and reports width 1
/// for every codepoint.
fn unicode_table_placeholder() -> i64 {
    let range = value::tuple_new(vec![
        crate::ops::int_result(0),
        crate::ops::int_result(0x10FFFF),
        crate::ops::int_result(1),
    ]);
    let cell_table = value::namespace_new(vec![
        (
            "unicode_version".to_string(),
            value::str_alloc("0.0.0".to_string()),
        ),
        ("widths".to_string(), value::list_new(vec![range])),
        ("narrow_to_wide".to_string(), value::set_new(Vec::new())),
    ]);
    value::namespace_new(vec![("cell_table".to_string(), cell_table)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exc::MODULE_NOT_FOUND_ERROR;
    use crate::value::{as_module, as_namespace, encode_int, str_alloc};

    fn text(s: &str) -> i64 {
        str_alloc(s.to_string())
    }

    #[test]
    fn registered_modules_are_returned_as_module_values() {
        exc::init();
        let math = call("import_module", &[text("math")]);
        let module = as_module(math).expect("module value");
        assert_eq!(module.name(), "math");
        assert!(!exc::pending());
    }

    #[test]
    fn registered_modules_share_sys_modules_identity() {
        exc::init();
        let first = call("import_module", &[text("sys")]);
        let second = call("import_module", &[text("sys")]);
        assert_eq!(first, second);
        assert_eq!(
            Some(first),
            crate::modules::registry_lookup("sys"),
            "import_module must return the sys.modules entry"
        );
        assert_eq!(as_module(first).map(|module| module.name()), Some("sys"));
        assert!(!exc::pending());
    }

    #[test]
    fn unknown_modules_raise_module_not_found() {
        exc::init();
        let missing = call("import_module", &[text("definitely_not_a_module")]);
        assert_eq!(
            exc::pending_tag(),
            i64::from(MODULE_NOT_FOUND_ERROR),
            "unknown imports must fail instead of returning a namespace"
        );
        assert_eq!(
            value::to_string(exc::pending_value()),
            "No module named 'definitely_not_a_module'"
        );
        assert!(as_namespace(missing).is_none());
        assert!(as_module(missing).is_none());

        exc::init();
        call("import_module", &[text("rich.not_a_width_table")]);
        assert_eq!(exc::pending_tag(), i64::from(MODULE_NOT_FOUND_ERROR));
    }

    #[test]
    fn dotted_user_submodules_and_relative_names_resolve() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        crate::core::globals::reset();
        exc::init();
        // Compiled modules register themselves through the `__name__` global
        // their initializer stores, dotted submodules included.
        crate::core::globals::set("pkg", "__name__", text("pkg"));
        crate::core::globals::set("pkg.sub", "__name__", text("pkg.sub"));
        crate::core::globals::set("pkg.sub", "value", encode_int(7).expect("small integer"));

        let submodule = call("import_module", &[text("pkg.sub")]);
        assert!(!exc::pending());
        assert_eq!(
            as_module(submodule).map(|module| module.name()),
            Some("pkg.sub")
        );

        // A relative name resolves against the explicit package first.
        let relative = call("import_module", &[text(".sub"), text("pkg")]);
        assert!(!exc::pending());
        assert_eq!(relative, submodule, "relative import must use the registry");

        // A missing dotted submodule reports the exact requested name.
        exc::clear();
        call("import_module", &[text("pkg.missing")]);
        assert_eq!(exc::pending_tag(), i64::from(MODULE_NOT_FOUND_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "No module named 'pkg.missing'"
        );

        // A relative name without a package is a `TypeError`, like CPython.
        exc::clear();
        call("import_module", &[text(".sub")]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));

        crate::core::globals::reset();
        exc::init();
    }

    #[test]
    fn unicode_tables_get_an_empty_cell_table() {
        exc::init();
        let loaded = call("import_module", &[text("rich._unicode_data.unicode15_0_0")]);
        let pairs = as_namespace(loaded).expect("placeholder namespace");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "cell_table");
        let table = as_namespace(pairs[0].1).expect("cell table namespace");
        assert_eq!(table.len(), 3);
        assert!(!exc::pending());
    }

    #[test]
    fn invalid_names_are_rejected() {
        exc::init();
        call("import_module", &[encode_int(1).expect("small integer")]);
        assert!(exc::pending());
        exc::clear();
        call("import_module", &[text("")]);
        assert!(exc::pending());
        exc::clear();
    }

    #[test]
    fn native_submodules_and_cache_invalidation_are_reachable() {
        exc::init();
        for name in [
            "importlib.util",
            "importlib.machinery",
            "importlib.metadata",
            "importlib.resources",
        ] {
            let module = call("import_module", &[text(name)]);
            assert_eq!(
                as_module(module).map(|module| module.name()),
                Some(name),
                "{name} must resolve to the native module"
            );
            assert!(!exc::pending(), "{name}");
        }
        // Reloading a built-in module is the documented no-op.
        let math = call("import_module", &[text("math")]);
        assert_eq!(call("reload", &[math]), math);
        assert_eq!(call("invalidate_caches", &[]), value::NONE);
        assert!(!exc::pending());
    }

    #[test]
    fn reloading_a_compiled_module_reports_the_missing_executor() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        crate::core::globals::reset();
        exc::init();
        crate::core::globals::set("pkg.reloadable", "__name__", text("pkg.reloadable"));
        let module = call("import_module", &[text("pkg.reloadable")]);
        exc::clear();
        call("reload", &[module]);
        assert_eq!(exc::pending_tag(), i64::from(IMPORT_ERROR));
        assert!(
            value::to_string(exc::pending_value()).contains("no runtime execution engine"),
            "{}",
            value::to_string(exc::pending_value())
        );

        // A module-like namespace is not a module value.
        exc::clear();
        let namespace = value::namespace_new(vec![("__name__".to_string(), text("ghost"))]);
        call("reload", &[namespace]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));

        crate::core::globals::reset();
        exc::init();
    }
}
