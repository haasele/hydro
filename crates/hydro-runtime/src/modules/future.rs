//! `__future__`: the compile-time feature flags as booleans.
//!
//! Python's `from __future__ import ...` statements are handled by the parser
//! and never re-exported at runtime, so the constants here exist to make the
//! module importable and to answer `__future__.annotations` style lookups. All
//! optional features that Python 3 enables by default are `True`;
//! `barry_as_FLUFL` is the one disabled flag.

use crate::exc::{self, ATTRIBUTE_ERROR};
use crate::value::{FALSE, TRUE};

/// Enabled-by-default features, matching CPython's `__future__.all_feature_names`.
const ENABLED: &[&str] = &[
    "annotations",
    "generator_stop",
    "division",
    "absolute_import",
    "print_function",
    "unicode_literals",
];

/// Materialises the module constant `name`.
pub(crate) fn constant(name: &str) -> i64 {
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
    fn barry_as_flufl_is_disabled() {
        exc::init();
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
