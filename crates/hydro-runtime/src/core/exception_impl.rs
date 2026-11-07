//! `BaseException` method bodies (CPython `Objects/exceptions.c`).
//!
//! [`crate::exc::exception_attr`] exposes these names as bound-method values,
//! and [`crate::exc::exception_method_call`] dispatches calls to them. The
//! payload itself lives in [`crate::exc::ExcData`]; this module only reads it
//! and, where the runtime stores one, updates the receiver's attribute table.
//!
//! The three CPython methods that mutate the exception are approximated the
//! way the payload allows:
//!
//! * `__setstate__` restores the receiver's instance attributes from a dict
//!   (the pickle state). Built-in exceptions have no attribute table in this
//!   runtime — their payload is immutable — so the state is still validated
//!   and snapshotted, but there is nothing to attach it to. The legacy tuple
//!   form (`self.args = state`) is accepted and validated for the same reason;
//!   [`super::exception_value_from_call`] stores `args` at construction, so
//!   the `__reduce__` form this runtime emits never needs it.
//! * `__reduce__`/`__reduce_ex__` return the reconstruction tuple
//!   `(type(self), self.args[, instance_dict])`, which is what `copy`,
//!   `copyreg` and `pickle` consume.
//! * `with_traceback` accepts only `None`: Hydro records the raise location
//!   in the payload and has no traceback object type.

use super::{exception_arity, ExcData, NONE, TYPE_ERROR};
use crate::value;

/// `BaseException.__setstate__(state)`.
///
/// A dict state restores the receiver's attributes; the pairs are snapshotted
/// before any attribute write so a key whose `__hash__`/`__eq__` mutates the
/// dict cannot invalidate the iteration (the CPython gh-97591 fix). A tuple
/// state is the legacy `self.args = state` form; it is validated and accepted.
///
/// Any other state reports `TypeError: state is not a dictionary`.
pub(super) fn setstate(object: i64, _data: &ExcData, args: &[i64]) -> i64 {
    if args.len() != 1 {
        return exception_arity("__setstate__", 1, args.len());
    }
    let state = args[0];
    let Some(pairs) = value::as_dict(state) else {
        if value::as_tuple(state).is_some() {
            // Legacy pickle form; see the module comment. The receiver keeps
            // the arguments it was constructed with.
            return NONE;
        }
        return super::raise_with_message(TYPE_ERROR, "state is not a dictionary");
    };
    // Take strong references (tagged values) before running any user code:
    // restoring an attribute can call back into Python and mutate the dict.
    let snapshot: Vec<(i64, i64)> = pairs.to_vec();
    for (key, new_value) in snapshot {
        let Some(name) = value::as_str(key) else {
            return super::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "attribute name must be string, not '{}'",
                    value::type_name(key)
                ),
            );
        };
        let name = name.to_string();
        value::instance_setattr(object, name, new_value);
    }
    NONE
}

/// `BaseException.__reduce__()`: `(type(self), self.args[, self.__dict__])`.
///
/// The third item is only present when the receiver has instance attributes,
/// matching CPython (an exception with an empty `__dict__` reduces to the
/// two-item form).
pub(super) fn reduce(object: i64, data: &ExcData) -> i64 {
    let args = super::args_tuple(data);
    let class = value::type_class(object);
    let roots = crate::heap::root_values(&[args, class]);
    let mut items = vec![class, args];
    if let Some(dictionary) = value::instance_dict(object) {
        let empty = value::as_dict(dictionary).is_none_or(|pairs| pairs.is_empty());
        if !empty {
            crate::heap::root_values(&[dictionary]);
            items.push(dictionary);
        }
    }
    let result = value::tuple_new(items);
    crate::heap::release_roots(roots);
    result
}

/// `BaseException.__reduce_ex__(protocol)`.
///
/// The protocol selects the pickle representation; Hydro has one
/// representation, so the argument only has to be present.
pub(super) fn reduce_ex(object: i64, data: &ExcData, args: &[i64]) -> i64 {
    if args.len() != 1 {
        return exception_arity("__reduce_ex__", 1, args.len());
    }
    reduce(object, data)
}

/// `BaseException.with_traceback(tb)`: returns the receiver.
///
/// Hydro has no traceback object type (`__traceback__` is always `None`, the
/// raise location lives in the payload), so `None` is the only accepted
/// argument.
pub(super) fn with_traceback(object: i64, args: &[i64]) -> i64 {
    if args.len() != 1 {
        return exception_arity("with_traceback", 1, args.len());
    }
    if args[0] == NONE {
        return object;
    }
    super::raise_with_message(
        TYPE_ERROR,
        "with_traceback() argument must be a traceback or None",
    )
}
