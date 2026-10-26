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
//!   `HYDRO_GC_THRESHOLD` (or the built-in default) in the first slot.
//!
//! Explicitly unsupported, with a tagged `NotImplementedError` instead of a
//! silent no-op:
//!
//! * `disable()` — automatic collection is unconditional in this collector;
//! * `set_threshold()` — the threshold is fixed at heap construction;
//! * `freeze()` / `unfreeze()` — there is no permanent generation;
//! * `get_objects()` — module code cannot enumerate raw heap objects.
//!
//! These names are declared in the central spec so callers receive the
//! documented error rather than an `AttributeError`. A later collector
//! revision with enable/threshold setters can replace these bodies.

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
        heap::add_thread_root(key);
        let number = ops::int_result(count);
        heap::add_thread_root(number);
        pairs.push((key, number));
    }
    let dict = value::dict_new(pairs);
    heap::add_thread_root(dict);
    let result = value::list_new(vec![dict]);
    heap::release_roots(start);
    result
}

/// Dispatches a native `gc` function. Arity is enforced by the central spec.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "collect" => collect(args),
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
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'gc' has no attribute '{function}'"),
        ),
    }
}
