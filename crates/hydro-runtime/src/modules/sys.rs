//! Frame introspection for the `sys` module.
//!
//! `sys._getframe(depth)` returns a real frame object backed by the call
//! records the runtime pushes around every generated (and interpreted)
//! activation. `f_locals` reads the activation's registered GC slots while it
//! runs, so it reflects generated-code locals at safepoints; after the frame
//! returns, the object keeps the snapshot taken when the activation ended.
//!
//! No frame is invented: a depth beyond the live stack raises CPython's
//! `ValueError: call stack is not deep enough`, and a depth argument that is
//! not an integer raises its `TypeError`/`OverflowError`.

use crate::core::exc;
use crate::core::value;
use crate::exc::{OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR};

/// `sys._getframe(depth=0)`.
pub(crate) fn getframe(args: &[i64]) -> i64 {
    let depth = match args.first().copied() {
        Some(value) => match frame_depth(value) {
            Ok(depth) => depth,
            Err(raised) => return raised,
        },
        None => 0,
    };
    if depth < 0 {
        return exc::raise_with_message(VALUE_ERROR, "call stack is not deep enough");
    }
    exc::getframe(depth as usize)
}

/// `sys._getframemodulename(depth=0)`.
///
/// Returns the module key of the requested frame (the same key a frame's
/// `f_globals` maps), or `None` when the stack is shallower than `depth`,
/// exactly like CPython's C implementation.
pub(crate) fn getframemodulename(args: &[i64]) -> i64 {
    let depth = match args.first().copied() {
        Some(value) => match frame_depth(value) {
            Ok(depth) => depth,
            Err(raised) => return raised,
        },
        None => 0,
    };
    if depth < 0 {
        return exc::raise_with_message(VALUE_ERROR, "call stack is not deep enough");
    }
    match exc::frame_module_at(depth as usize) {
        Some(module) => value::str_alloc(module),
        None => value::NONE,
    }
}

/// `inspect.currentframe()`: the innermost running Hydro activation.
pub(crate) fn current_frame() -> i64 {
    exc::current_frame()
}

/// Validates the optional `depth` argument of the frame APIs.
///
/// CPython converts the value to a C `int`: an integer that does not fit is an
/// `OverflowError`, any other type a `TypeError` naming its type. Negative
/// depths are accepted here and rejected by the callers as CPython does.
fn frame_depth(depth: i64) -> Result<i64, i64> {
    if let Some(depth) = value::as_integer(depth) {
        return Ok(depth);
    }
    if value::to_bigint(depth).is_some() {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            "Python int too large to convert to C int",
        ));
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(depth)
        ),
    ))
}
