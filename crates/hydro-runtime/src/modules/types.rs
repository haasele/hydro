//! `types`: native placeholder classes plus `SimpleNamespace`.
//!
//! Rich imports `FrameType`, `ModuleType`, `TracebackType`, `MappingProxyType`
//! and friends almost exclusively for annotations, `isinstance` checks and
//! dictionary keys. Each name therefore materialises as an empty native class
//! whose identity is fresh on every access, like the `typing` placeholders.
//! `FunctionType`, `CodeType`, `GenericAlias` and `NoneType` follow the same
//! pattern; no introspection API is attached to them.
//!
//! `CellType` is the exception: it is a single cached class whose instances
//! carry a `cell_contents` attribute, so `types.CellType(value)` and
//! `isinstance(obj, types.CellType)` behave like CPython's cell type.
//!
//! `SimpleNamespace(...)` accepts one optional mapping and returns the native
//! namespace object used by `argparse`, so attribute reads work immediately.
//! CPython spells the attributes as keyword arguments; the frontend has no
//! `**kwargs` lowering for native module functions yet, so the runtime takes
//! the equivalent mapping instead of rejecting the constructor outright.

use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR};
use crate::heap;
use crate::methods;
use crate::value::{self, NONE};

/// Names exposed as empty native classes.
const CLASSES: &[&str] = &[
    "CodeType",
    "FrameType",
    "FunctionType",
    "GenericAlias",
    "MappingProxyType",
    "ModuleType",
    "NoneType",
    "TracebackType",
];

/// Materialises the module constant `name`.
pub(crate) fn constant(name: &str) -> i64 {
    if name == "CellType" {
        return cell_type_class();
    }
    if CLASSES.contains(&name) {
        return value::class_new(name.to_string(), NONE, Vec::new());
    }
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module 'types' has no attribute '{name}'"),
    )
}

/// The cached `types.CellType` class.
///
/// A cell is a plain native instance carrying a `cell_contents` attribute.
/// The class is cached in the runtime registry so repeated accesses share
/// identity, which `isinstance(..., types.CellType)` compares exactly like
/// CPython's native cell type.
pub(crate) fn cell_type_class() -> i64 {
    if let Some(cached) = globals::registry_get("types.CellType") {
        return cached;
    }
    let class = value::class_new("CellType".to_string(), NONE, Vec::new());
    globals::registry_set("types.CellType", class);
    class
}

/// `types.CellType([value])`: a cell object holding `value` (empty when
/// omitted).
fn cell_type_new(args: &[i64]) -> i64 {
    if args.len() > 1 {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("CellType() takes at most 1 argument ({} given)", args.len()),
        );
    }
    let class = cell_type_class();
    let instance = value::instance_new(class);
    let Some(&contents) = args.first() else {
        return instance;
    };
    // `setattr` may allocate (attribute name/interning), so keep the value
    // reachable across it.
    let roots = heap::root_values(&[contents]);
    let _ = methods::setattr(
        instance,
        b"cell_contents".as_ptr(),
        "cell_contents".len(),
        contents,
    );
    heap::release_roots(roots);
    instance
}

/// Dispatches a native `types` function. Arity is enforced by the central spec.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "SimpleNamespace" => simple_namespace(args.first().copied().unwrap_or(NONE)),
        "CellType" => cell_type_new(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'types' has no attribute '{function}'"),
        ),
    }
}

/// `SimpleNamespace(mapping=None)`: copies the mapping into a namespace.
fn simple_namespace(mapping: i64) -> i64 {
    if mapping == NONE {
        return value::namespace_new(Vec::new());
    }
    let Some(pairs) = value::as_dict(mapping) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "SimpleNamespace() argument must be a mapping, not {}",
                value::type_name(mapping)
            ),
        );
    };
    // The snapshot keeps the borrow independent of the allocations performed
    // while decoding the attribute names; the values themselves stay reachable
    // through the mapping argument.
    let snapshot = pairs.to_vec();
    let mut attributes = Vec::with_capacity(snapshot.len());
    for (key, item) in snapshot {
        let Some(name) = value::as_str(key) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "SimpleNamespace() attribute names must be str, not {}",
                    value::type_name(key)
                ),
            );
        };
        attributes.push((name.to_string(), item));
    }
    value::namespace_new(attributes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{as_class, as_namespace, encode_int};

    fn text(s: &str) -> i64 {
        value::str_alloc(s.to_string())
    }

    #[test]
    fn placeholder_names_are_native_classes() {
        exc::init();
        for name in CLASSES {
            let class = as_class(constant(name)).expect("class value");
            assert_eq!(class.name, *name);
            assert_eq!(class.base, NONE);
            assert!(class.methods.is_empty());
        }
        assert!(!exc::pending());
    }

    #[test]
    fn simple_namespace_without_attributes_is_empty() {
        exc::init();
        let namespace = call("SimpleNamespace", &[]);
        assert_eq!(as_namespace(namespace).map(|pairs| pairs.len()), Some(0));
        assert!(!exc::pending());
    }

    #[test]
    fn simple_namespace_copies_mapping_attributes() {
        exc::init();
        let mapping = value::dict_new(vec![
            (text("name"), text("rich")),
            (text("width"), encode_int(80).unwrap()),
        ]);
        let namespace = call("SimpleNamespace", &[mapping]);
        assert!(!exc::pending());
        let pairs = as_namespace(namespace).expect("namespace");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "name");
        assert_eq!(value::as_str(pairs[0].1), Some("rich"));
        assert_eq!(pairs[1].0, "width");
        assert_eq!(value::as_integer(pairs[1].1), Some(80));
    }

    #[test]
    fn simple_namespace_rejects_non_mappings() {
        exc::init();
        call("SimpleNamespace", &[text("not a mapping")]);
        assert!(exc::pending());
        exc::clear();
    }

    #[test]
    fn unknown_names_raise_attribute_error() {
        exc::init();
        constant("Missing");
        assert!(exc::pending());
        exc::clear();
        call("missing", &[]);
        assert!(exc::pending());
        exc::clear();
    }

    #[test]
    fn cell_type_is_a_cached_class_with_contents() {
        exc::init();
        let class = cell_type_class();
        assert_eq!(class, constant("CellType"));
        assert_eq!(
            as_class(class).map(|class| class.name.as_str()),
            Some("CellType")
        );

        let value = encode_int(42).unwrap();
        let cell = call("CellType", &[value]);
        assert!(!exc::pending());
        assert_eq!(value::instance_class_value(cell), Some(class));
        let read = methods::getattr(cell, b"cell_contents".as_ptr(), "cell_contents".len());
        assert_eq!(value::as_integer(read), Some(42));
    }
}
