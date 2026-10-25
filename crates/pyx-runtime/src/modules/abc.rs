//! `abc`: the abstract base class placeholder and `abstractmethod`.
//!
//! The frontend does not enforce abstractness yet, so `ABC` is an empty
//! native class that merely serves as a base, and `abstractmethod` is the
//! identity decorator (CPython returns the function unchanged, setting a flag
//! on it).

use crate::exc::{self, ATTRIBUTE_ERROR};
use crate::value::{self, NONE};

/// Materialises the module constant `name`.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "ABC" => value::class_new("ABC".to_string(), NONE, Vec::new()),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'abc' has no attribute '{name}'"),
        ),
    }
}

/// Dispatches a native `abc` function. Arity is enforced by the central spec.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "abstractmethod" => args.first().copied().unwrap_or(NONE),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'abc' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{as_class, str_alloc};

    #[test]
    fn abc_is_an_empty_native_class() {
        exc::init();
        let class = as_class(constant("ABC")).expect("class value");
        assert_eq!(class.name, "ABC");
        assert_eq!(class.base, NONE);
        assert!(class.methods.is_empty());
        assert!(!exc::pending());
    }

    #[test]
    fn abstractmethod_is_the_identity_decorator() {
        exc::init();
        let function = str_alloc("render".to_string());
        assert_eq!(call("abstractmethod", &[function]), function);
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
