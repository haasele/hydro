//! `__future__`: the compile-time feature flags as booleans.
//!
//! Python's `from __future__ import ...` statements are handled by the parser
//! and never re-exported at runtime, so the constants here exist to make the
//! module importable and to answer `__future__.annotations` style lookups. All
//! optional features that Python 3 enables by default are `True`, and the
//! mandatory features (`nested_scopes`, `generators`, `with_statement`) are
//! also `True`; `barry_as_FLUFL` is the one disabled flag. `all_feature_names`
//! lists every feature name in CPython's declaration order.

use crate::container;
use crate::exc::{self, ATTRIBUTE_ERROR};
use crate::heap;
use crate::value::{self, FALSE, TRUE};

/// `all_feature_names`, in CPython's declaration order.
const ALL_FEATURE_NAMES: &[&str] = &[
    "nested_scopes",
    "generators",
    "division",
    "absolute_import",
    "with_statement",
    "print_function",
    "unicode_literals",
    "barry_as_FLUFL",
    "generator_stop",
    "annotations",
];

/// Feature booleans that are `True`.
///
/// The first block holds the optional features Python 3 enables by default.
/// `nested_scopes`, `generators` and `with_statement` are the mandatory
/// features whose `_Feature` objects have `mandatory != None`, so they are
/// true for every supported version as well.
const ENABLED: &[&str] = &[
    "annotations",
    "generator_stop",
    "division",
    "absolute_import",
    "print_function",
    "unicode_literals",
    "nested_scopes",
    "generators",
    "with_statement",
];

/// `__future__.all_feature_names`: a fresh list of every feature name.
///
/// The list is rooted before the strings are allocated and every appended
/// string is published with an insertion barrier, so no collection can see a
/// partially built list.
fn all_feature_names() -> i64 {
    let list = value::list_new(Vec::new());
    let roots = heap::root_values(&[list]);
    for name in ALL_FEATURE_NAMES {
        let item = value::str_from_bytes(name.as_bytes());
        container::list_append(list, item);
    }
    heap::release_roots(roots);
    list
}

/// Materialises the module constant `name`.
pub(crate) fn constant(name: &str) -> i64 {
    if name == "all_feature_names" {
        return all_feature_names();
    }
    if ENABLED.contains(&name) {
        return TRUE;
    }
    if name == "barry_as_FLUFL" {
        return FALSE;
    }
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module '__future__' has no attribute '{name}'"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_features_are_enabled() {
        exc::init();
        for name in ENABLED {
            assert_eq!(constant(name), TRUE, "{name}");
        }
        assert!(!exc::pending());
    }

    #[test]
    fn mandatory_features_are_enabled() {
        exc::init();
        // `_Feature(..., mandatory=...)` objects are truthy for every version
        // that already has the feature; Hydro models features as booleans.
        for name in ["nested_scopes", "generators", "with_statement"] {
            assert_eq!(constant(name), TRUE, "{name}");
        }
        assert!(!exc::pending());
    }

    #[test]
    fn barry_as_flufl_is_disabled() {
        exc::init();
        assert_eq!(constant("barry_as_FLUFL"), FALSE);
        assert!(!exc::pending());
    }

    #[test]
    fn all_feature_names_is_a_list_in_cpython_order() {
        exc::init();
        let names = constant("all_feature_names");
        assert!(!exc::pending());
        let items = value::as_list(names).expect("all_feature_names is a list");
        assert_eq!(items.len(), 10);
        let names: Vec<&str> = items
            .iter()
            .map(|item| value::as_str(*item).unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "nested_scopes",
                "generators",
                "division",
                "absolute_import",
                "with_statement",
                "print_function",
                "unicode_literals",
                "barry_as_FLUFL",
                "generator_stop",
                "annotations",
            ]
        );
    }

    #[test]
    fn all_feature_names_lists_every_feature_and_is_fresh() {
        exc::init();
        let first = constant("all_feature_names");
        let second = constant("all_feature_names");
        assert_ne!(first, second, "each lookup allocates a new list");
        for names in [first, second] {
            let items = value::as_list(names).expect("list");
            let has_barry = items
                .iter()
                .any(|item| value::as_str(*item) == Some("barry_as_FLUFL"));
            assert!(has_barry, "barry_as_FLUFL missing");
        }
        // `barry_as_FLUFL` is still the one disabled flag.
        assert_eq!(constant("barry_as_FLUFL"), FALSE);
        assert!(!exc::pending());
    }

    #[test]
    fn unknown_names_raise_attribute_error() {
        exc::init();
        constant("missing");
        assert!(exc::pending());
        exc::clear();
    }
}
