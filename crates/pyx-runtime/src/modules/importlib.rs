//! `importlib.import_module`: dynamic imports for the native module table.
//!
//! PyX resolves ordinary imports statically. `import_module` exists for code
//! that selects a module name at runtime: a name registered in
//! `pyx_ir::stdlib` yields the native module value, so attribute access goes
//! through the usual dispatcher.
//!
//! Names outside that table yield a placeholder namespace whose `cell_table`
//! attribute is an empty dict. Rich's Unicode width tables are loaded this way
//! (`rich._unicode_data.unicodeX`); the empty table makes the cell-width lookup
//! fall back to the default width instead of failing the import. This is a
//! temporary shim: the real per-width tables are not bundled.

use crate::exc::{self, TYPE_ERROR, VALUE_ERROR};
use crate::value;

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
    if pyx_ir::stdlib::module(name).is_some() {
        return value::module_value(name.to_string());
    }
    // Placeholder for the generated Unicode tables (and any other module the
    // runtime cannot import natively).
    let cell_table = value::dict_new(Vec::new());
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
    fn unicode_tables_get_an_empty_cell_table() {
        exc::init();
        let loaded = call("import_module", &[text("rich._unicode_data.unicode15_0_0")]);
        let pairs = as_namespace(loaded).expect("placeholder namespace");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "cell_table");
        let table = value::as_dict(pairs[0].1).expect("dict cell_table");
        assert!(table.is_empty());
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
