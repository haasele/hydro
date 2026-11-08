//! `__future__`: the compile-time feature records.
//!
//! `from __future__ import ...` statements are handled by the parser and never
//! re-exported at runtime. The module attributes are CPython's `_Feature`
//! instances (not booleans): `pydoc` and the CPython tests test them with
//! `isinstance(obj, __future__._Feature)` and read
//! `getOptionalRelease()`/`getMandatoryRelease()`. Each record is a real
//! instance of the `_Feature` class the parent module registers, with the
//! documented `optional`, `mandatory` and `compiler_flag` attributes, and it is
//! cached per heap generation so repeated reads observe one object (CPython
//! module attributes are singletons).
//!
//! `all_feature_names` and `__all__` return fresh lists on every access, like
//! the mutable module attributes. The `CO_*` compiler-flag constants carry
//! CPython's values.

use crate::container;
use crate::exc::{self, ATTRIBUTE_ERROR};
use crate::core::globals;
use crate::heap;
use crate::ops;
use crate::value::{self, NONE};

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

/// One `_Feature` declaration: release tuples and the compiler flag.
///
/// A release tuple is `(major, minor, micro, level, serial)`; `mandatory` is
/// `None` for `annotations`.
struct Feature {
    name: &'static str,
    optional: (i64, i64, i64, &'static str, i64),
    mandatory: Option<(i64, i64, i64, &'static str, i64)>,
    compiler_flag: i64,
}

const FEATURES: &[Feature] = &[
    Feature {
        name: "nested_scopes",
        optional: (2, 1, 0, "beta", 1),
        mandatory: Some((2, 2, 0, "alpha", 0)),
        compiler_flag: 0x0010,
    },
    Feature {
        name: "generators",
        optional: (2, 2, 0, "alpha", 1),
        mandatory: Some((2, 3, 0, "final", 0)),
        compiler_flag: 0,
    },
    Feature {
        name: "division",
        optional: (2, 2, 0, "alpha", 2),
        mandatory: Some((3, 0, 0, "alpha", 0)),
        compiler_flag: 0x20000,
    },
    Feature {
        name: "absolute_import",
        optional: (2, 5, 0, "alpha", 1),
        mandatory: Some((3, 0, 0, "alpha", 0)),
        compiler_flag: 0x40000,
    },
    Feature {
        name: "with_statement",
        optional: (2, 5, 0, "alpha", 1),
        mandatory: Some((2, 6, 0, "alpha", 0)),
        compiler_flag: 0x80000,
    },
    Feature {
        name: "print_function",
        optional: (2, 6, 0, "alpha", 2),
        mandatory: Some((3, 0, 0, "alpha", 0)),
        compiler_flag: 0x100000,
    },
    Feature {
        name: "unicode_literals",
        optional: (2, 6, 0, "alpha", 2),
        mandatory: Some((3, 0, 0, "alpha", 0)),
        compiler_flag: 0x200000,
    },
    Feature {
        name: "barry_as_FLUFL",
        optional: (3, 1, 0, "alpha", 2),
        mandatory: Some((4, 0, 0, "alpha", 0)),
        compiler_flag: 0x400000,
    },
    Feature {
        name: "generator_stop",
        optional: (3, 5, 0, "beta", 1),
        mandatory: Some((3, 7, 0, "alpha", 0)),
        compiler_flag: 0x800000,
    },
    Feature {
        name: "annotations",
        optional: (3, 7, 0, "beta", 1),
        mandatory: None,
        compiler_flag: 0x1000000,
    },
];

/// Names exported by `__future__.__all__`: `all_feature_names` plus every
/// feature name, matching `Lib/__future__.py`.
const DUNDER_ALL: &[&str] = &[
    "all_feature_names",
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

fn feature(name: &str) -> Option<&'static Feature> {
    FEATURES.iter().find(|entry| entry.name == name)
}

/// Builds one Python string list, rooting the list while it grows.
fn string_list(items: &[&str]) -> i64 {
    let list = value::list_new(Vec::with_capacity(items.len()));
    let roots = heap::root_values(&[list]);
    for &item in items {
        let text = value::str_alloc(item.to_string());
        container::list_append(list, text);
    }
    heap::release_roots(roots);
    list
}

/// `all_feature_names`: a fresh list on every access.
fn all_feature_names() -> i64 {
    string_list(ALL_FEATURE_NAMES)
}

fn cache_generation_key() -> &'static str {
    "__future__.generation"
}

fn cached_feature(name: &str) -> Option<i64> {
    let generation = ops::int_result(heap::generation() as i64);
    if globals::registry_get(cache_generation_key()) != Some(generation) {
        return None;
    }
    globals::registry_get(&format!("__future__.feature.{name}"))
}

fn cache_feature(name: &str, instance: i64) {
    globals::registry_set(cache_generation_key(), ops::int_result(heap::generation() as i64));
    globals::registry_set(&format!("__future__.feature.{name}"), instance);
}

fn version_tuple(release: (i64, i64, i64, &'static str, i64)) -> i64 {
    let major = ops::int_result(release.0);
    let minor = ops::int_result(release.1);
    let micro = ops::int_result(release.2);
    let level = value::str_alloc(release.3.to_string());
    let serial = ops::int_result(release.4);
    let roots = heap::root_values(&[level]);
    let tuple = value::tuple_new(vec![major, minor, micro, level, serial]);
    heap::release_roots(roots);
    tuple
}

/// The cached `_Feature` instance for `entry`.
fn feature_instance(entry: &'static Feature) -> i64 {
    if let Some(cached) = cached_feature(entry.name) {
        return cached;
    }
    // The class value is owned (and cached) by the parent module; reading it
    // through the parent keeps `isinstance(x, __future__._Feature)` true.
    let class = super::future_feature_class();
    if exc::pending() {
        return NONE;
    }
    let roots = heap::root_values(&[class]);
    let optional = version_tuple(entry.optional);
    heap::push_scoped_root(optional);
    let mandatory = match entry.mandatory {
        Some(release) => version_tuple(release),
        None => NONE,
    };
    heap::push_scoped_root(mandatory);
    let compiler_flag = ops::int_result(entry.compiler_flag);
    let instance = value::instance_new(class);
    heap::push_scoped_root(instance);
    value::instance_setattr(instance, "optional".to_string(), optional);
    value::instance_setattr(instance, "mandatory".to_string(), mandatory);
    value::instance_setattr(instance, "compiler_flag".to_string(), compiler_flag);
    cache_feature(entry.name, instance);
    heap::release_roots(roots);
    instance
}

/// Materialises the module constant `name`.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "all_feature_names" => all_feature_names(),
        "__all__" => string_list(DUNDER_ALL),
        _ => {
            if let Some(entry) = feature(name) {
                return feature_instance(entry);
            }
            let flag = match name {
                "CO_NESTED" => Some(0x0010),
                "CO_GENERATOR_ALLOWED" => Some(0),
                "CO_FUTURE_DIVISION" => Some(0x20000),
                "CO_FUTURE_ABSOLUTE_IMPORT" => Some(0x40000),
                "CO_FUTURE_WITH_STATEMENT" => Some(0x80000),
                "CO_FUTURE_PRINT_FUNCTION" => Some(0x100000),
                "CO_FUTURE_UNICODE_LITERALS" => Some(0x200000),
                "CO_FUTURE_BARRY_AS_BDFL" => Some(0x400000),
                "CO_FUTURE_GENERATOR_STOP" => Some(0x800000),
                "CO_FUTURE_ANNOTATIONS" => Some(0x1000000),
                _ => None,
            };
            match flag {
                Some(flag) => ops::int_result(flag),
                None => exc::raise_with_message(
                    ATTRIBUTE_ERROR,
                    &format!("module '__future__' has no attribute '{name}'"),
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: i64) -> Vec<&'static str> {
        value::as_list(list)
            .expect("list value")
            .iter()
            .map(|item| value::as_str(*item).expect("string element"))
            .collect()
    }

    fn class_name(instance: i64) -> String {
        value::instance_class_value(instance)
            .and_then(|class| value::as_class(class).map(|class| class.name.clone()))
            .unwrap_or_default()
    }

    #[test]
    fn features_are_feature_instances() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        for entry in FEATURES {
            let instance = constant(entry.name);
            assert!(!exc::pending(), "{}", entry.name);
            assert_eq!(class_name(instance), "_Feature", "{}", entry.name);
            let optional = value::instance_attr(instance, "optional").expect("optional");
            assert_eq!(
                value::as_tuple(optional).map(<[i64]>::len),
                Some(5),
                "{}",
                entry.name
            );
            let flag = value::instance_attr(instance, "compiler_flag").expect("flag");
            assert_eq!(
                value::as_integer(flag),
                Some(entry.compiler_flag),
                "{}",
                entry.name
            );
        }
    }

    #[test]
    fn feature_instances_are_stable_and_barry_is_disabled_value() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        let first = constant("annotations");
        let second = constant("annotations");
        assert_eq!(first, second, "feature records are module singletons");
        let mandatory = value::instance_attr(first, "mandatory").expect("mandatory");
        assert_eq!(mandatory, NONE);
        assert_eq!(
            value::as_integer(constant("CO_FUTURE_BARRY_AS_BDFL")),
            Some(0x400000)
        );
    }

    #[test]
    fn all_feature_names_and_dunder_all_are_fresh_lists() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        let first = constant("all_feature_names");
        let second = constant("all_feature_names");
        assert_ne!(first, second, "each lookup allocates a new list");
        assert_eq!(strings(first), ALL_FEATURE_NAMES);
        assert_eq!(strings(constant("__all__")), DUNDER_ALL);
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
