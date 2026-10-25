//! `typing`: importable placeholders for the typing names used by the
//! supported corpus.
//!
//! Type annotations are consumed by the frontend and the runtime never
//! introspects these values, so generic aliases and special forms are `None`
//! placeholders. `TYPE_CHECKING` is `False`, which keeps guarded imports from
//! running. The three names that commonly appear as a class base
//! (`Generic`, `NamedTuple`, `Protocol`) are empty native classes so a class
//! definition can subclass them; no tuple or protocol semantics are attached.
//!
//! Every access to a class constant allocates a fresh class, matching
//! CPython's stable identity semantics closely enough for subclassing while
//! avoiding a static root.

use crate::exc::{self, ATTRIBUTE_ERROR};
use crate::value::{self, FALSE, NONE};

/// Constants materialised as empty native classes (usable as class bases).
const CLASSES: &[&str] = &["Generic", "NamedTuple", "Protocol"];

/// Constants materialised as `None`: annotations are not evaluated at runtime.
const PLACEHOLDERS: &[&str] = &[
    "Any",
    "AnyStr",
    "BinaryIO",
    "Callable",
    "ClassVar",
    "Collection",
    "ContextManager",
    "DefaultDict",
    "Deque",
    "Dict",
    "Final",
    "ForwardRef",
    "Generator",
    "IO",
    "Iterable",
    "Iterator",
    "List",
    "Literal",
    "Mapping",
    "Match",
    "NoReturn",
    "Optional",
    "Pattern",
    "Sequence",
    "Set",
    "TextIO",
    "Tuple",
    "Type",
    "Union",
];

/// Materialises the module constant `name`.
pub(crate) fn constant(name: &str) -> i64 {
    if name == "TYPE_CHECKING" {
        return FALSE;
    }
    if CLASSES.contains(&name) {
        return value::class_new(name.to_string(), NONE, Vec::new());
    }
    if PLACEHOLDERS.contains(&name) {
        return NONE;
    }
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module 'typing' has no attribute '{name}'"),
    )
}

/// Dispatches a native `typing` function. Arity is enforced by the central
/// spec before this point.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        // CPython's signature is `cast(typ, val)`; the value argument wins.
        "cast" => args.get(1).copied().unwrap_or(NONE),
        "runtime_checkable" | "overload" => args.first().copied().unwrap_or(NONE),
        // Type variables are annotations only; the frontend ignores the value.
        "TypeVar" => NONE,
        // `NewType("Name", int)` is called like the underlying type in the
        // corpus, so the underlying type is the usable stand-in.
        "NewType" => args.get(1).copied().unwrap_or(NONE),
        "get_type_hints" => value::dict_new(Vec::new()),
        "get_args" => value::tuple_new(Vec::new()),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'typing' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{as_class, as_dict, as_tuple, TRUE};

    fn text(s: &str) -> i64 {
        value::str_alloc(s.to_string())
    }

    #[test]
    fn type_checking_is_false() {
        exc::init();
        assert_eq!(constant("TYPE_CHECKING"), FALSE);
        assert_ne!(constant("TYPE_CHECKING"), TRUE);
        assert!(!exc::pending());
    }

    #[test]
    fn generic_aliases_are_none_placeholders() {
        exc::init();
        for name in PLACEHOLDERS {
            assert_eq!(constant(name), NONE, "{name}");
        }
        assert!(!exc::pending());
    }

    #[test]
    fn class_placeholders_are_native_classes() {
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
    fn cast_returns_the_value_argument() {
        exc::init();
        let typ = constant("Any");
        let value = text("payload");
        assert_eq!(call("cast", &[typ, value]), value);
        // The type argument is ignored even when it is a nested alias.
        assert_eq!(call("cast", &[NONE, NONE]), NONE);
        assert!(!exc::pending());
    }

    #[test]
    fn decorators_are_identity_and_typevar_is_none() {
        exc::init();
        let function = text("f");
        assert_eq!(call("runtime_checkable", &[function]), function);
        assert_eq!(call("overload", &[function]), function);
        assert_eq!(call("TypeVar", &[text("T")]), NONE);
        assert_eq!(call("TypeVar", &[text("T"), text("int")]), NONE);
        // `NewType` stands in for the underlying type so calls work.
        assert_eq!(call("NewType", &[text("TaskID"), function]), function);
        assert!(!exc::pending());
    }

    #[test]
    fn introspection_helpers_return_empty_containers() {
        exc::init();
        let hints = call("get_type_hints", &[text("f")]);
        assert_eq!(as_dict(hints).expect("dict").len(), 0);
        let empty = call("get_type_hints", &[]);
        assert_eq!(as_dict(empty).expect("dict").len(), 0);
        let args = call("get_args", &[NONE]);
        assert_eq!(as_tuple(args).expect("tuple").len(), 0);
        assert!(!exc::pending());
    }

    #[test]
    fn unknown_function_raises_attribute_error() {
        exc::init();
        call("missing", &[]);
        assert!(exc::pending());
        exc::clear();
    }
}
