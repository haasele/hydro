//! `enum`: placeholder classes for the enum bases used by the corpus.
//!
//! Enum members and class bodies are a later compiler feature, so `Enum` and
//! `IntEnum` are empty native classes and `auto()` evaluates to `None`. Code
//! that only imports a base and subclasses it (`class Color(IntEnum)`)
//! therefore imports and defines cleanly; lookups such as `Color.RED` are
//! resolved by the ordinary class attribute rules once the class body runs.

use crate::exc::{self, ATTRIBUTE_ERROR};
use crate::value::{self, NONE};

/// Names a module constant as an empty native class.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "Enum" | "IntEnum" => value::class_new(name.to_string(), NONE, Vec::new()),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'enum' has no attribute '{name}'"),
        ),
    }
}

/// Dispatches a native `enum` function. Arity is enforced by the central spec.
pub(crate) fn call(function: &str, _args: &[i64]) -> i64 {
    match function {
        // Member numbering needs enum class construction, which the frontend
        // does not model yet; `None` keeps `auto()` importable.
        "auto" => NONE,
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'enum' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::as_class;

    #[test]
    fn enum_bases_are_empty_native_classes() {
        exc::init();
        for name in ["Enum", "IntEnum"] {
            let class = as_class(constant(name)).expect("class value");
            assert_eq!(class.name, name);
            assert_eq!(class.base, NONE);
            assert!(class.methods.is_empty());
        }
        assert!(!exc::pending());
    }

    #[test]
    fn auto_returns_none() {
        exc::init();
        assert_eq!(call("auto", &[]), NONE);
        assert!(!exc::pending());
    }

    #[test]
    fn unknown_names_raise_attribute_error() {
        exc::init();
        call("missing", &[]);
        assert!(exc::pending());
        exc::clear();
        constant("missing");
        assert!(exc::pending());
        exc::clear();
    }
}
