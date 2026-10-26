//! `marshal`: an intentionally unsupported serialization format.
//!
//! Hydro has no CPython marshal byte stream: the format encodes CPython
//! bytecode and object layouts that the runtime does not share. `dump`,
//! `dumps`, `load` and `loads` therefore raise `NotImplementedError` with a
//! message naming the supported alternatives (`pickle`, `json`) instead of
//! fabricating a payload or silently accepting one.
//!
//! `marshal.version` reports the CPython 3.14 format version (5, the version
//! that added `slice` support); it is a truthful format *identifier* only,
//! not a promise that this runtime can read or write that format.

use crate::exc::{self, ATTRIBUTE_ERROR, NOT_IMPLEMENTED_ERROR};
use crate::ops;

/// CPython 3.14 marshal format version (`Doc/library/marshal.rst`,
/// `versionchanged:: 3.14`).
pub(crate) const VERSION: i64 = 5;

fn unsupported(function: &str) -> i64 {
    exc::raise_with_message(
        NOT_IMPLEMENTED_ERROR,
        &format!(
            "marshal.{function}() is not implemented: Hydro has no CPython marshal byte stream \
             (use pickle or json)"
        ),
    )
}

/// Dispatches a native `marshal` function. Arity is enforced by the central
/// spec; every conversion reports the missing format.
pub(crate) fn call(function: &str, _args: &[i64]) -> i64 {
    match function {
        "dump" | "dumps" | "load" | "loads" => unsupported(function),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'marshal' has no attribute '{function}'"),
        ),
    }
}

/// Materialises a `marshal` constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "version" => ops::int_result(VERSION),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'marshal' has no attribute '{name}'"),
        ),
    }
}
