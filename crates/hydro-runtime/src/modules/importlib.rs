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

use super::importlib_machinery as machinery;
use super::importlib_util;

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
    // CPython always has `sys.modules["__main__"]`: the entry program. Hydro
    // compiles the entry module under that exact name, so expose its module
    // value even when the program has not bound any global yet.
    if name == "__main__" {
        return super::module_value_by_name("__main__");
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

/// Dispatch for the native `runpy` surface.
///
/// The runtime module table declares `runpy.run_module`; the entry point lives
/// here because it is defined in terms of `importlib`'s registry and spec
/// machinery. `mod.rs` forwards `runpy` calls to this function.
pub(crate) fn runpy_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "run_module" => run_module(args),
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module 'runpy' has no attribute '{function}'"),
        ),
    }
}

/// Keyword-aware dispatch for the native `runpy` surface.
pub(crate) fn runpy_call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if function != "run_module" {
        return exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module 'runpy' has no attribute '{function}'"),
        );
    }
    const PARAMETERS: [&str; 4] = ["mod_name", "init_globals", "run_name", "alter_sys"];
    let mut bound = args.to_vec();
    if bound.len() < PARAMETERS.len() {
        bound.resize(PARAMETERS.len(), value::NONE);
    }
    for (name, value) in keywords {
        let Some(slot) = PARAMETERS
            .iter()
            .position(|parameter| *parameter == name.as_str())
        else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("run_module() got an unexpected keyword argument '{name}'"),
            );
        };
        if slot < args.len() {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("run_module() got multiple values for argument '{name}'"),
            );
        }
        bound[slot] = *value;
    }
    run_module(&bound)
}

/// `runpy.run_module(mod_name, init_globals=None, run_name=None,
/// alter_sys=False)`.
///
/// Hydro compiles module bodies ahead of time, so a module that is part of the
/// program has already executed. `run_module` returns the documented result: a
/// fresh dict holding that module's namespace (side effects are not repeated)
/// with `__name__` set to `run_name`, the module's spec/loader/file metadata,
/// and `init_globals` merged in first. Native modules have no code object, so
/// they raise CPython's `ImportError: No code object available for <name>`;
/// names outside the program fail with `No module named <name>`, and an
/// installed source file that was not compiled into the program reports that
/// Hydro has no runtime Python compiler rather than pretending to run it.
///
/// Package names follow CPython and execute the package's `__main__` submodule;
/// `alter_sys` is accepted but has no observable effect because the module is
/// never inserted into `sys.modules`.
pub(crate) fn run_module(args: &[i64]) -> i64 {
    let module_arg = args.first().copied().unwrap_or(value::NONE);
    let Some(mod_name) = value::as_str(module_arg).map(str::to_string) else {
        // CPython's `_get_module_details` probes `mod_name.startswith`, so the
        // error for a non-string is the attribute error, not a `TypeError`.
        return exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!(
                "'{}' object has no attribute 'startswith'",
                value::type_name(module_arg)
            ),
        );
    };
    if mod_name.starts_with('.') {
        return exc::raise_with_message(IMPORT_ERROR, "Relative module names not supported");
    }
    let init_globals = args.get(1).copied().unwrap_or(value::NONE);
    // The entry program always exists as `sys.modules["__main__"]`.
    if mod_name == "__main__" {
        super::module_value_by_name("__main__");
    }
    let mod_name = match redirect_package(mod_name) {
        Ok(name) => name,
        Err(raised) => return raised,
    };
    let run_name = match args.get(2).copied() {
        Some(name) if name != value::NONE => name,
        _ => value::str_alloc(mod_name.clone()),
    };
    if hydro_ir::stdlib::module(&mod_name).is_some() {
        // Built-in/frozen modules have no Python source to run, exactly like
        // CPython's `BuiltinImporter`.
        return exc::raise_with_message(
            IMPORT_ERROR,
            &format!("No code object available for {mod_name}"),
        );
    }
    if super::registry_lookup(&mod_name).is_none() {
        if importlib_util::resolve_module(&mod_name).is_some() {
            return exc::raise_with_message(
                IMPORT_ERROR,
                &format!(
                    "cannot execute module '{mod_name}': Hydro has no runtime Python \
                     compiler; Python modules are compiled into the program ahead of time"
                ),
            );
        }
        return exc::raise_with_message(IMPORT_ERROR, &format!("No module named {mod_name}"));
    }
    // CPython merges `init_globals` only after the module's code is found, so
    // an invalid mapping surfaces only for an executable module.
    let init_pairs: Option<&'static [(i64, i64)]> = if init_globals == value::NONE {
        None
    } else if let Some(pairs) = value::as_dict(init_globals) {
        Some(pairs)
    } else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "'{}' object is not iterable",
                value::type_name(init_globals)
            ),
        );
    };
    module_globals_dict(&mod_name, run_name, init_pairs)
}

/// Resolves CPython's package rule for `run_module`: running a package name
/// executes `<package>.__main__` and fails if that submodule does not exist.
fn redirect_package(mod_name: String) -> Result<String, i64> {
    let Some(found) = importlib_util::resolve_module(&mod_name) else {
        return Ok(mod_name);
    };
    // A package is known by its search locations; a compiled package whose
    // `__file__` was never materialised still marks itself through
    // `__path__`/`__package__`.
    let is_package = found.search_locations.is_some()
        || crate::core::globals::lookup(&mod_name, "__path__").is_some()
        || crate::core::globals::lookup(&mod_name, "__package__").and_then(value::as_str)
            == Some(mod_name.as_str());
    if !is_package {
        return Ok(mod_name);
    }
    let inner = format!("{mod_name}.__main__");
    if super::registry_lookup(&inner).is_some()
        || hydro_ir::stdlib::module(&inner).is_some()
        || importlib_util::resolve_module(&inner).is_some()
    {
        Ok(inner)
    } else {
        Err(exc::raise_with_message(
            IMPORT_ERROR,
            &format!(
                "No module named {inner}; '{mod_name}' is a package and cannot be \
                 directly executed"
            ),
        ))
    }
}

/// The result dict of `run_module`: the module's namespace snapshot plus the
/// metadata CPython's `_run_code`/`_init_module_attrs` would set.
fn module_globals_dict(
    mod_name: &str,
    run_name: i64,
    init_globals: Option<&'static [(i64, i64)]>,
) -> i64 {
    let spec = crate::core::globals::lookup(mod_name, "__spec__")
        .filter(|value| machinery::is_spec(*value))
        .unwrap_or_else(|| match importlib_util::resolve_module(mod_name) {
            Some(found) => importlib_util::spec_instance(&found),
            None => machinery::new_spec(mod_name, value::NONE, None, None, false),
        });
    let loader = value::instance_attr(spec, "loader").unwrap_or(value::NONE);
    let parent = value::instance_attr(spec, "parent").unwrap_or(value::NONE);
    let origin = value::instance_attr(spec, "origin").unwrap_or(value::NONE);
    let cached = value::instance_attr(spec, "cached").unwrap_or(value::NONE);
    let roots = crate::heap::root_values(&[spec, loader, parent, origin, cached, run_name]);

    let mut pairs: Vec<(i64, i64)> = Vec::new();
    for (name, item) in crate::core::globals::entries(mod_name) {
        // The metadata attributes are overwritten below; CPython never derives
        // them from the body for `run_module`'s result.
        if matches!(
            name.as_str(),
            "__name__" | "__spec__" | "__loader__" | "__package__" | "__file__" | "__cached__"
        ) {
            continue;
        }
        pairs.push((value::str_alloc(name), item));
    }
    if let Some(init) = init_globals {
        pairs.extend_from_slice(init);
    }
    pairs.push((value::str_alloc("__name__".to_string()), run_name));
    pairs.push((value::str_alloc("__spec__".to_string()), spec));
    pairs.push((value::str_alloc("__loader__".to_string()), loader));
    pairs.push((value::str_alloc("__package__".to_string()), parent));
    if origin != value::NONE {
        pairs.push((value::str_alloc("__file__".to_string()), origin));
    }
    if cached != value::NONE {
        pairs.push((value::str_alloc("__cached__".to_string()), cached));
    }
    let result = value::dict_new(pairs);
    crate::heap::release_roots(roots);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exc::MODULE_NOT_FOUND_ERROR;
    use crate::value::{as_dict, as_module, as_namespace, encode_int, str_alloc};

    fn text(s: &str) -> i64 {
        str_alloc(s.to_string())
    }

    /// `dict[key]` for a string key, without raising.
    fn dict_field(dict: i64, name: &str) -> Option<i64> {
        as_dict(dict)?
            .iter()
            .find_map(|(key, value)| (value::as_str(*key) == Some(name)).then_some(*value))
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

    #[test]
    fn main_module_is_always_importable() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        crate::core::globals::reset();
        exc::init();
        // The entry program is compiled under `__main__`; expose its module
        // value even before it binds a global.
        let main = call("import_module", &[text("__main__")]);
        assert!(
            !exc::pending(),
            "{}",
            value::to_string(exc::pending_value())
        );
        assert_eq!(
            as_module(main).map(|module| module.name()),
            Some("__main__")
        );
        assert_eq!(crate::modules::registry_lookup("__main__"), Some(main));

        // Bindings of the entry module stay visible through the same value.
        crate::core::globals::set("__main__", "entry", encode_int(11).expect("small int"));
        let again = call("import_module", &[text("__main__")]);
        assert_eq!(again, main);
        assert_eq!(
            crate::modules::module_get_by_name("__main__", "entry"),
            encode_int(11).expect("small int")
        );

        crate::core::globals::reset();
        exc::init();
    }

    #[test]
    fn run_module_returns_the_compiled_module_namespace() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        crate::core::globals::reset();
        exc::init();
        crate::core::globals::set("rmod", "__name__", text("rmod"));
        crate::core::globals::set("rmod", "__file__", text("/tmp/hydro-rmod.py"));
        crate::core::globals::set("rmod", "__doc__", text("module doc"));
        crate::core::globals::set("rmod", "x", encode_int(7).expect("small int"));

        let result = run_module(&[text("rmod"), value::NONE, text("__main__")]);
        assert!(
            !exc::pending(),
            "{}",
            value::to_string(exc::pending_value())
        );
        assert!(as_dict(result).is_some(), "run_module returns a dict");
        assert_eq!(
            dict_field(result, "__name__").and_then(value::as_str),
            Some("__main__")
        );
        assert_eq!(
            dict_field(result, "x"),
            Some(encode_int(7).expect("small int"))
        );
        assert_eq!(
            dict_field(result, "__package__").and_then(value::as_str),
            Some("")
        );
        assert_eq!(
            dict_field(result, "__file__").and_then(value::as_str),
            Some("/tmp/hydro-rmod.py")
        );
        assert_eq!(
            dict_field(result, "__doc__").and_then(value::as_str),
            Some("module doc")
        );
        let spec = dict_field(result, "__spec__").expect("__spec__");
        assert_eq!(machinery::spec_name(spec).as_deref(), Some("rmod"));
        assert_eq!(
            dict_field(result, "__loader__"),
            value::instance_attr(spec, "loader")
        );

        // `run_name=None` defaults to the module name.
        let default = run_module(&[text("rmod")]);
        assert_eq!(
            dict_field(default, "__name__").and_then(value::as_str),
            Some("rmod")
        );

        // `init_globals` is merged first; the metadata attributes win.
        let init = value::dict_new(vec![
            (text("z"), encode_int(5).expect("small int")),
            (text("__name__"), text("ignored")),
        ]);
        let roots = crate::heap::root_values(&[init]);
        let merged = run_module(&[text("rmod"), init]);
        crate::heap::release_roots(roots);
        assert!(
            !exc::pending(),
            "{}",
            value::to_string(exc::pending_value())
        );
        assert_eq!(
            dict_field(merged, "z"),
            Some(encode_int(5).expect("small int"))
        );
        assert_eq!(
            dict_field(merged, "__name__").and_then(value::as_str),
            Some("rmod")
        );

        // `init_globals` must be a mapping once the module is executable.
        exc::clear();
        run_module(&[text("rmod"), encode_int(1).expect("small int")]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'int' object is not iterable"
        );

        crate::core::globals::reset();
        exc::init();
    }

    #[test]
    fn run_module_binds_keywords_like_the_native_signature() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        crate::core::globals::reset();
        exc::init();
        crate::core::globals::set("kwmod", "__name__", text("kwmod"));
        crate::core::globals::set("kwmod", "value", encode_int(3).expect("small int"));

        let result = runpy_call_kw(
            "run_module",
            &[text("kwmod")],
            &[
                ("run_name".to_string(), text("__main__")),
                ("alter_sys".to_string(), value::NONE),
            ],
        );
        assert!(
            !exc::pending(),
            "{}",
            value::to_string(exc::pending_value())
        );
        assert_eq!(
            dict_field(result, "__name__").and_then(value::as_str),
            Some("__main__")
        );
        assert_eq!(
            dict_field(result, "value"),
            Some(encode_int(3).expect("small int"))
        );

        exc::clear();
        runpy_call_kw(
            "run_module",
            &[text("kwmod")],
            &[("mod_name".to_string(), text("other"))],
        );
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        assert!(
            value::to_string(exc::pending_value()).contains("multiple values"),
            "{}",
            value::to_string(exc::pending_value())
        );

        exc::clear();
        runpy_call_kw(
            "run_module",
            &[text("kwmod")],
            &[("unknown".to_string(), value::NONE)],
        );
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));

        crate::core::globals::reset();
        exc::init();
    }

    #[test]
    fn run_module_reports_native_and_missing_modules_like_cpython() {
        exc::init();
        // A native module has no code object to execute.
        run_module(&[text("sys")]);
        assert_eq!(exc::pending_tag(), i64::from(IMPORT_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "No code object available for sys"
        );

        // An unknown name keeps CPython's exact message (no quotes).
        exc::clear();
        run_module(&[text("definitely_not_a_module")]);
        assert_eq!(exc::pending_tag(), i64::from(IMPORT_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "No module named definitely_not_a_module"
        );

        // Relative names, non-string names and non-mapping init globals.
        exc::clear();
        run_module(&[text(".sibling")]);
        assert_eq!(exc::pending_tag(), i64::from(IMPORT_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "Relative module names not supported"
        );

        exc::clear();
        run_module(&[encode_int(1).expect("small int")]);
        assert_eq!(exc::pending_tag(), i64::from(exc::ATTRIBUTE_ERROR));
        assert!(
            value::to_string(exc::pending_value()).contains("has no attribute 'startswith'"),
            "{}",
            value::to_string(exc::pending_value())
        );

        exc::clear();
        run_module(&[text("math"), encode_int(1).expect("small int")]);
        // The module check happens first: CPython never reaches `init_globals`
        // for a module with no code object.
        assert_eq!(exc::pending_tag(), i64::from(IMPORT_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "No code object available for math"
        );
        exc::clear();
    }

    #[test]
    fn run_module_redirects_packages_to_main() {
        let _heap_guard = crate::heap::heap_reset_test_guard();
        crate::core::globals::reset();
        exc::init();
        crate::core::globals::set("pkg", "__name__", text("pkg"));
        crate::core::globals::set(
            "pkg",
            "__file__",
            text("/tmp/hydro-pkg-test/pkg/__init__.py"),
        );
        crate::core::globals::set("pkg.__main__", "__name__", text("pkg.__main__"));
        crate::core::globals::set("pkg.__main__", "m", encode_int(9).expect("small int"));

        let main = run_module(&[text("pkg")]);
        assert!(
            !exc::pending(),
            "{}",
            value::to_string(exc::pending_value())
        );
        assert_eq!(
            dict_field(main, "__name__").and_then(value::as_str),
            Some("pkg.__main__")
        );
        assert_eq!(
            dict_field(main, "m"),
            Some(encode_int(9).expect("small int"))
        );
        assert_eq!(
            dict_field(main, "__package__").and_then(value::as_str),
            Some("pkg")
        );

        // A package without `__main__` reports CPython's exact message.
        crate::core::globals::set("solo", "__name__", text("solo"));
        crate::core::globals::set(
            "solo",
            "__file__",
            text("/tmp/hydro-pkg-test/solo/__init__.py"),
        );
        exc::clear();
        run_module(&[text("solo")]);
        assert_eq!(exc::pending_tag(), i64::from(IMPORT_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "No module named solo.__main__; 'solo' is a package and cannot be directly executed"
        );

        crate::core::globals::reset();
        exc::init();
    }
}
