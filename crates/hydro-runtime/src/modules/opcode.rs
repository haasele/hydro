//! `_opcode`: truthful introspection for a bytecode-less runtime.
//!
//! Hydro does not compile to CPython bytecode and never specializes
//! instructions, so the module exposes the introspection surface CPython's
//! pure-Python `opcode` module imports, with only truthful answers:
//!
//! * the predicate helpers (`has_arg`, `has_const`, `has_name`, `has_jump`,
//!   `has_free`, `has_local`, `has_exc`, `is_valid`) report `False` for every
//!   opcode: no instruction of this runtime has an argument, constant, name,
//!   jump target, free variable, local variable or exception table entry;
//! * `stack_effect` and `get_executor` raise `NotImplementedError` instead of
//!   inventing a stack depth or an executor object for code that is never
//!   executed as CPython bytecode; `get_specialization_stats` reports that
//!   specialization is unavailable for the same reason;
//! * the intrinsic/specialization metadata helpers return the empty
//!   collections this runtime actually has (no intrinsics, no specialized
//!   opcodes, no special-method mapping, no number-operation table);
//! * `ENABLE_SPECIALIZATION`/`ENABLE_SPECIALIZATION_FT` are the documented
//!   "disabled" value `0`.
//!
//! `opmap`/`opname` are not defined here: in CPython 3.14 they live in
//! `_opcode_metadata`, and this module has no opcode table of its own to
//! report.

use crate::exc::{self, ATTRIBUTE_ERROR, NOT_IMPLEMENTED_ERROR};
use crate::ops;
use crate::value;

pub(crate) const ENABLE_SPECIALIZATION: i64 = 0;
pub(crate) const ENABLE_SPECIALIZATION_FT: i64 = 0;

pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "ENABLE_SPECIALIZATION" => ops::int_result(ENABLE_SPECIALIZATION),
        "ENABLE_SPECIALIZATION_FT" => ops::int_result(ENABLE_SPECIALIZATION_FT),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '_opcode' has no attribute '{name}'"),
        ),
    }
}

/// Dispatches a native `_opcode` function. Arity is enforced by the spec.
pub(crate) fn call(function: &str, _args: &[i64]) -> i64 {
    match function {
        "has_arg" | "has_const" | "has_name" | "has_jump" | "has_free" | "has_local"
        | "has_exc" | "is_valid" => value::FALSE,
        "stack_effect" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "stack_effect() is not available: Hydro does not execute CPython bytecode",
        ),
        "get_executor" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "get_executor() is not available: Hydro code has no CPython executors",
        ),
        "get_specialization_stats" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "get_specialization_stats() is not available: this runtime never specializes \
                 instructions",
        ),
        "get_intrinsic1_descs" | "get_intrinsic2_descs" | "get_nb_ops" => {
            value::list_new(Vec::new())
        }
        "get_special_method_names" => value::dict_new(Vec::new()),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '_opcode' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{exc, value};

    #[test]
    fn specialization_flags_are_disabled() {
        exc::init();
        assert_eq!(constant("ENABLE_SPECIALIZATION"), ops::int_result(0));
        assert_eq!(constant("ENABLE_SPECIALIZATION_FT"), ops::int_result(0));
        assert!(!exc::pending());
    }

    #[test]
    fn opcode_predicates_are_false() {
        exc::init();
        for name in [
            "has_arg",
            "has_const",
            "has_name",
            "has_jump",
            "has_free",
            "has_local",
            "has_exc",
            "is_valid",
        ] {
            assert_eq!(call(name, &[ops::int_result(1)]), value::FALSE, "{name}");
        }
        assert!(!exc::pending());
    }

    #[test]
    fn unsupported_operations_raise_not_implemented() {
        exc::init();
        for name in ["stack_effect", "get_executor", "get_specialization_stats"] {
            call(name, &[ops::int_result(1), ops::int_result(0)]);
            assert_eq!(
                exc::pending_tag(),
                exc::NOT_IMPLEMENTED_ERROR as i64,
                "{name}"
            );
            exc::clear();
        }
    }

    #[test]
    fn metadata_helpers_report_empty_tables() {
        exc::init();
        for name in ["get_intrinsic1_descs", "get_intrinsic2_descs", "get_nb_ops"] {
            let list = call(name, &[]);
            assert!(
                value::as_list(list).is_some_and(|items| items.is_empty()),
                "{name}"
            );
        }
        let names = call("get_special_method_names", &[]);
        assert!(value::as_dict(names).is_some_and(|pairs| pairs.is_empty()));
        assert!(!exc::pending());
    }

    #[test]
    fn unknown_names_raise_attribute_error() {
        exc::init();
        call("missing", &[]);
        assert_eq!(exc::pending_tag(), exc::ATTRIBUTE_ERROR as i64);
        exc::clear();
        constant("missing");
        assert_eq!(exc::pending_tag(), exc::ATTRIBUTE_ERROR as i64);
        exc::clear();
    }
}
