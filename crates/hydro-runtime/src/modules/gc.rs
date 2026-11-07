//! `gc`: the collector's introspection surface.
//!
//! The runtime heap is one generation of conservative mark-and-sweep, so the
//! truthful mapping of the CPython API is:
//!
//! * `collect()` forces a full collection and returns the number of freed
//!   objects;
//! * `isenabled()` is always `True` (automatic collection has no switch) and
//!   `enable()` is an idempotent no-op returning `None`;
//! * `get_stats()` returns a one-element list of dictionaries with the
//!   collector's real cumulative counters (`collections`, `collected`,
//!   `uncollectable` is always 0 because the collector runs no finalizers and
//!   therefore has no uncollectable objects);
//! * `get_count()` reports the completed-collection counter in the first
//!   generation slot; the other two slots are 0 because no generational
//!   split exists;
//! * `get_threshold()` reports the effective threshold from
//!   `HYDRO_GC_THRESHOLD` (or the built-in default) in the first slot;
//! * `is_tracked(obj)` reports CPython's classification: immediates and
//!   immutable scalar payloads are untracked, containers and other mutable
//!   heap objects are tracked;
//! * `garbage` is the module-level list of uncollectable objects. The
//!   collector runs no finalizers, so nothing is ever uncollectable and the
//!   list stays empty; it is one persistent list per run so identity,
//!   `len()` and mutation behave like CPython's list.
//!
//! Explicitly unsupported, with a tagged `NotImplementedError` instead of a
//! silent no-op:
//!
//! * `disable()` — automatic collection is unconditional in this collector;
//! * `set_threshold()` — the threshold is fixed at heap construction;
//! * `freeze()` / `unfreeze()` — there is no permanent generation;
//! * `get_objects()` — module code cannot enumerate raw heap objects;
//! * `get_referrers()` — finding referring containers requires the same
//!   enumeration.
//!
//! These names are declared in the central spec so callers receive the
//! documented error rather than an `AttributeError`. A later collector
//! revision with enable/threshold setters can replace these bodies.

use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, NOT_IMPLEMENTED_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::ops;
use crate::value::{self, NONE};

fn unsupported(function: &str) -> i64 {
    exc::raise_with_message(
        NOT_IMPLEMENTED_ERROR,
        &format!(
            "gc.{function}() is not supported: Hydro's collector has a single unconditional \
             generation and exposes no switch for it"
        ),
    )
}

/// Validates the optional `generation` argument (0, 1 or 2, like CPython).
fn check_generation(args: &[i64]) -> Result<(), i64> {
    match args.first().copied() {
        None | Some(NONE) => Ok(()),
        Some(value) => match value::as_integer(value) {
            Some(0..=2) => Ok(()),
            Some(_) => Err(exc::raise_with_message(
                VALUE_ERROR,
                "generation must be one of 0, 1 or 2",
            )),
            None => Err(exc::raise_with_message(
                TYPE_ERROR,
                "generation must be an integer",
            )),
        },
    }
}

/// `gc.collect(generation=2)`: a full collection. The generation argument is
/// accepted and validated, then ignored because every collection is full.
fn collect(args: &[i64]) -> i64 {
    if let Err(error) = check_generation(args) {
        return error;
    }
    ops::int_result(heap::collect())
}

/// `gc.get_stats()`: one dictionary of cumulative counters for the single
/// generation.
fn get_stats() -> i64 {
    let stats = heap::gc_stats();
    let start = heap::root_values(&[]);
    let mut pairs = Vec::with_capacity(3);
    for (name, count) in [
        ("collections", stats.collections as i64),
        ("collected", stats.freed_total as i64),
        ("uncollectable", 0),
    ] {
        let key = value::str_alloc(name.to_string());
        heap::push_scoped_root(key);
        let number = ops::int_result(count);
        heap::push_scoped_root(number);
        pairs.push((key, number));
    }
    let dict = value::dict_new(pairs);
    heap::push_scoped_root(dict);
    let result = value::list_new(vec![dict]);
    heap::release_roots(start);
    result
}

/// Registry key of the persistent `gc.garbage` list.
const GARBAGE_KEY: &str = "gc.garbage";

/// `gc.garbage`: the list of objects the collector found uncollectable.
///
/// Hydro's collector runs no finalizers and therefore has no uncollectable
/// objects, so the list is empty. It is kept in the runtime registry so every
/// read observes the same list object (`.append`/`.pop`/`.remove` work on it)
/// until the program's globals are reset.
pub(crate) fn garbage_list() -> i64 {
    let start = heap::root_values(&[]);
    let list = match globals::registry_get(GARBAGE_KEY) {
        Some(cached) => cached,
        None => {
            let list = value::list_new(Vec::new());
            heap::push_scoped_root(list);
            // The registry roots the value before publishing it.
            globals::registry_set(GARBAGE_KEY, list);
            list
        }
    };
    heap::release_roots(start);
    list
}

/// Materialises a `gc` constant (only `garbage` exists).
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "garbage" => garbage_list(),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'gc' has no attribute '{name}'"),
        ),
    }
}

/// `gc.is_tracked(obj)`: whether the collector tracks `obj`.
///
/// Hydro's heap is uniformly swept, so the truthful answer mirrors CPython's
/// classification rather than a per-object flag: immediates (`None`, `bool`,
/// tagged `int`) and untracked scalars (`str`, `bytes`, `float`, `BigInt`)
/// are not tracked; containers, instances, classes, exceptions and other
/// mutable heap objects are.
fn is_tracked(object: i64) -> i64 {
    value::bool_value(is_tracked_value(object))
}

/// The classification behind [`is_tracked`], using the value predicates.
fn is_tracked_value(object: i64) -> bool {
    if !value::is_heap(object) {
        return false;
    }
    // Instance-like values (including exception instances and classes) are
    // always GC objects, even when they wrap a scalar payload.
    if value::as_instance(object).is_some()
        || value::as_exc_instance(object).is_some()
        || value::as_class(object).is_some()
    {
        return true;
    }
    // `bytearray` is mutable and tracked; `str`/`bytes`/`float`/`BigInt`
    // payloads are not.
    if value::as_bytearray(object).is_some() {
        return true;
    }
    value::as_str(object).is_none()
        && value::as_bytes(object).is_none()
        && value::as_float(object).is_none()
        && !value::is_bigint(object)
}

/// Dispatches a native `gc` function. Arity is enforced by the central spec.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "collect" => collect(args),
        "is_tracked" => is_tracked(args.first().copied().unwrap_or(NONE)),
        "isenabled" => value::TRUE,
        "enable" => NONE,
        "disable" => unsupported("disable"),
        "get_count" => {
            let stats = heap::gc_stats();
            let count = ops::int_result(stats.collections as i64);
            let zero = ops::int_result(0);
            value::tuple_new(vec![count, zero, zero])
        }
        "get_stats" => get_stats(),
        "get_threshold" => {
            let stats = heap::gc_stats();
            let threshold = ops::int_result(stats.threshold as i64);
            let zero = ops::int_result(0);
            value::tuple_new(vec![threshold, zero, zero])
        }
        "set_threshold" => unsupported("set_threshold"),
        "freeze" => unsupported("freeze"),
        "unfreeze" => unsupported("unfreeze"),
        "get_objects" => {
            if let Err(error) = check_generation(args) {
                return error;
            }
            unsupported("get_objects")
        }
        // Finding the containers that refer to an object needs a scan of every
        // heap object's outgoing edges. The collector keeps its object pools
        // private (only allocation, marking and sweeping are exposed), so the
        // runtime cannot enumerate containers and a partial answer from roots
        // alone would be wrong. Report the limitation instead of returning a
        // fabricated list.
        "get_referrers" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "gc.get_referrers() is not supported: the collector exposes no heap-object \
             enumeration, so referring containers cannot be found",
        ),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'gc' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_is_one_persistent_empty_list() {
        // Keep a concurrent `hydro_runtime_init` reset from clearing the
        // registry between the two reads.
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        let first = constant("garbage");
        assert!(!exc::pending());
        let second = constant("garbage");
        assert!(!exc::pending());
        assert_eq!(first, second, "gc.garbage must keep a single identity");
        assert_eq!(value::as_list(first).map(|items| items.len()), Some(0));

        // The list is mutable, like CPython's module-level `gc.garbage`;
        // put it back the way it was so repeated runs see the empty list.
        let item = ops::int_result(7);
        if let Some(mut items) = value::list_items_mut(first) {
            items.push(item);
        }
        assert_eq!(
            value::as_list(constant("garbage")).map(|items| items.len()),
            Some(1)
        );
        if let Some(mut items) = value::list_items_mut(first) {
            items.pop();
        }
        assert_eq!(
            value::as_list(constant("garbage")).map(|items| items.len()),
            Some(0)
        );
    }

    #[test]
    fn is_tracked_matches_cpython_classification() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        let tracked = [
            value::list_new(Vec::new()),
            value::dict_new(Vec::new()),
            value::tuple_new(Vec::new()),
            value::set_new(Vec::new()),
            value::bytearray_new(Vec::new()),
            value::instance_new(value::class_new("T".to_string(), NONE, Vec::new())),
        ];
        for object in tracked {
            assert_eq!(
                is_tracked(object),
                value::TRUE,
                "{object:#x} must be tracked"
            );
        }

        let untracked = [
            NONE,
            value::TRUE,
            ops::int_result(7),
            value::str_alloc("text".to_string()),
            value::bytes_new(b"bytes".to_vec()),
            value::float_new(1.5),
        ];
        for object in untracked {
            assert_eq!(
                is_tracked(object),
                value::FALSE,
                "{object:#x} must not be tracked"
            );
        }
        assert!(!exc::pending());
    }

    #[test]
    fn unknown_constants_raise_attribute_error() {
        exc::init();
        assert_eq!(constant("missing"), NONE);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        exc::clear();
    }
}
