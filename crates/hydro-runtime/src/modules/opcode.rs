//! `_opcode`: truthful introspection constants.
//!
//! Hydro does not compile to CPython bytecode and never specializes
//! instructions, so both specialization flags are the documented "disabled"
//! value `0`. No other `_opcode` name is exposed: `_opcode.stack_effect`,
//! `opmap`, `HAVE_ARGUMENT`, ... are absent and therefore raise
//! `AttributeError` instead of returning invented data.

use crate::ops;

pub(crate) const ENABLE_SPECIALIZATION: i64 = 0;
pub(crate) const ENABLE_SPECIALIZATION_FT: i64 = 0;

pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "ENABLE_SPECIALIZATION" => ops::int_result(ENABLE_SPECIALIZATION),
        "ENABLE_SPECIALIZATION_FT" => ops::int_result(ENABLE_SPECIALIZATION_FT),
        _ => crate::exc::raise_with_message(
            crate::exc::ATTRIBUTE_ERROR,
            &format!("module '_opcode' has no attribute '{name}'"),
        ),
    }
}
