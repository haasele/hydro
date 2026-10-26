//! `importlib.import_module`: dynamic imports backed by the module registry.
//!
//! Hydro resolves ordinary imports statically. `import_module` exists for code
//! that selects a module name at runtime: the runtime registry (`sys.modules`)
//! knows every native module from `hydro_ir::stdlib` and every loaded user
//! module discovered in the globals store, and import returns that exact
//! module value so identity matches `sys.modules[name]`.
//!
//! Names outside the registry fail with `ModuleNotFoundError`. The only
//! remaining placeholder is the generated Unicode width tables
//! (`rich._unicode_data.unicodeX`): their synthesized `cell_table` reports a
//! single default-width range for every codepoint instead of failing the
//! import. The real per-width tables are not bundled.

use crate::exc::{self, MODULE_NOT_FOUND_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value;

/// Placeholder modules that are still synthesized on demand.
const UNICODE_TABLE_PREFIX: &str = "rich._unicode_data";

/// Dispatches a native `importlib` function. Arity is enforced by the central
/// spec before this point.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "import_module" => import_module(args[0]),
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module 'importlib' has no attribute '{function}'"),
        ),
    }
}

/// `importlib.import_module(name, package=None)`.
fn import_module(name_arg: i64) -> i64 {
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
    if name.split('.').any(|part| part.is_empty()) {
        return exc::raise_with_message(
            MODULE_NOT_FOUND_ERROR,
            &format!("No module named '{name}'"),
        );
    }
    if let Some(module) = super::registry_lookup(name) {
        return module;
    }
    if name == UNICODE_TABLE_PREFIX || name.starts_with("rich._unicode_data.") {
        return unicode_table_placeholder();
    }
    // Unknown names never become a fake namespace: callers rely on the import
    // failing so their fallback/probe logic runs.
    exc::raise_with_message(MODULE_NOT_FOUND_ERROR, &format!("No module named '{name}'"))
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
}
