//! `gc`: the collector's real introspection and control surface.
//!
//! The runtime heap is one generation of conservative mark-and-sweep, so the
//! truthful mapping of the CPython API is:
//!
//! * `collect()` forces a full collection; the optional `generation` selects
//!   which generation's statistics counter the call increments (`0`, `1` or
//!   `2`, default `2`), exactly like CPython's per-generation counters;
//! * `get_stats()` returns three dictionaries (one per CPython generation)
//!   with the counters of collections made through this module;
//! * `get_count()` reports objects allocated since the last collection in the
//!   first slot; the other generations have no separate counters;
//! * `get_threshold()`/`set_threshold()` read the effective threshold and
//!   pin it (disabling the adaptive controller); the second and third values
//!   round-trip but this collector has no separate generations;
//! * `get_objects()` enumerates the heap's live registry (all objects belong
//!   to the single generation, reported as generation 0);
//! * `get_referents(*objs)` reports each argument's direct references for the
//!   container, instance, class-namespace and namespace forms the runtime
//!   models; scalars contribute nothing;
//! * `get_referrers(*objs)` scans the live registry for objects whose direct
//!   references include an argument (identity comparison);
//! * `freeze()` pins every currently live object as a persistent root (the
//!   collector then never reclaims it) and `unfreeze()` releases those roots;
//!   `get_freeze_count()` reports how many objects are pinned;
//! * `get_debug()`/`set_debug()` store CPython's debug flags; this collector
//!   has no cyclic-garbage finalisation pass, so `DEBUG_SAVEALL` does not move
//!   objects into `gc.garbage`;
//! * `callbacks` is the persistent callback list (registered callbacks are not
//!   invoked: the runtime's collector has no collection-start/stop hooks);
//! * `is_tracked(obj)` reports CPython's classification: immediates and
//!   immutable scalar payloads are untracked, containers and other mutable
//!   heap objects are tracked;
//! * `is_finalized(obj)` is always `False`: the collector runs no finalizers,
//!   so no object is ever finalized;
//! * `garbage` is the persistent list of uncollectable objects. This
//!   collector has no uncollectable objects, so it stays empty and mutable.

use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::ops;
use crate::value::{self, NONE};

/// CPython's `gc.DEBUG_*` flag values.
const DEBUG_STATS: i64 = 1;
const DEBUG_COLLECTABLE: i64 = 2;
const DEBUG_UNCOLLECTABLE: i64 = 4;
const DEBUG_SAVEALL: i64 = 32;
const DEBUG_LEAK: i64 = 38;

/// Registry key of the persistent `gc.garbage` list.
const GARBAGE_KEY: &str = "gc.garbage";
/// Registry key of the persistent `gc.callbacks` list.
const CALLBACKS_KEY: &str = "gc.callbacks";
/// Registry key of the debug flags.
const DEBUG_KEY: &str = "gc.debug";
/// Registry key of the round-tripped threshold 1/2 values.
const THRESHOLD1_KEY: &str = "gc.threshold1";
const THRESHOLD2_KEY: &str = "gc.threshold2";

/// Validates the optional `generation` argument (0, 1 or 2, like CPython).
fn check_generation(args: &[i64]) -> Result<usize, i64> {
    match args.first().copied() {
        None | Some(NONE) => Ok(2),
        Some(value) => match value::as_integer(value) {
            Some(generation @ 0..=2) => Ok(generation as usize),
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

/// Per-generation collection counters plus the allocation high-water mark.
struct CollectorState {
    generation: u64,
    collections: [u64; 3],
    collected: [u64; 3],
    last_object_count: usize,
}

static COLLECTOR: LazyLock<Mutex<CollectorState>> = LazyLock::new(|| {
    Mutex::new(CollectorState {
        generation: 0,
        collections: [0; 3],
        collected: [0; 3],
        last_object_count: 0,
    })
});

fn collector_state() -> MutexGuard<'static, CollectorState> {
    let mut guard = COLLECTOR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let generation = heap::generation();
    if guard.generation != generation {
        *guard = CollectorState {
            generation,
            collections: [0; 3],
            collected: [0; 3],
            last_object_count: heap::objects(),
        };
    }
    guard
}

/// Persistent-root slots of `gc.freeze()`.
struct FrozenState {
    generation: u64,
    slots: Vec<usize>,
}

static FROZEN: LazyLock<Mutex<FrozenState>> = LazyLock::new(|| {
    Mutex::new(FrozenState {
        generation: 0,
        slots: Vec::new(),
    })
});

/// The freeze state, discarded when the heap generation changes (the old
/// roots belong to the freed heap).
fn frozen_state() -> MutexGuard<'static, FrozenState> {
    let mut guard = FROZEN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let generation = heap::generation();
    if guard.generation != generation {
        guard.slots.clear();
        guard.generation = generation;
    }
    guard
}

/// `gc.collect(generation=2)`: a full collection.
fn collect(args: &[i64]) -> i64 {
    let generation = match check_generation(args) {
        Ok(generation) => generation,
        Err(error) => return error,
    };
    let freed = heap::collect();
    let mut state = collector_state();
    state.collections[generation] += 1;
    state.collected[generation] += u64::try_from(freed).unwrap_or(0);
    state.last_object_count = heap::objects();
    freed
}

fn int(number: i64) -> i64 {
    ops::int_result(number)
}

/// `gc.get_stats()`: CPython's three per-generation counter dictionaries.
fn get_stats() -> i64 {
    let (collections, collected) = {
        let state = collector_state();
        (state.collections, state.collected)
    };
    let list = value::list_new(Vec::new());
    let roots = heap::root_values(&[list]);
    for generation in 0..3 {
        let mut pairs = Vec::with_capacity(3);
        for (name, count) in [
            ("collections", collections[generation]),
            ("collected", collected[generation]),
            ("uncollectable", 0),
        ] {
            let key = value::str_alloc(name.to_string());
            heap::push_scoped_root(key);
            let number = int(count as i64);
            heap::push_scoped_root(number);
            pairs.push((key, number));
        }
        let dict = value::dict_new(pairs);
        heap::push_scoped_root(dict);
        crate::container::list_append(list, dict);
    }
    heap::release_roots(roots);
    list
}

/// Registry integer with `default` for an absent/other item.
fn registry_int(key: &str, default: i64) -> i64 {
    globals::registry_get(key)
        .and_then(value::as_integer)
        .unwrap_or(default)
}

fn set_registry_int(key: &str, number: i64) {
    let value = ops::int_result(number);
    globals::registry_set(key, value);
}

/// `gc.get_threshold()`: `(threshold0, threshold1, threshold2)`.
fn get_threshold() -> i64 {
    let threshold = ops::int_result(heap::gc_stats().threshold as i64);
    let first = ops::int_result(registry_int(THRESHOLD1_KEY, 10));
    let second = ops::int_result(registry_int(THRESHOLD2_KEY, 10));
    value::tuple_new(vec![threshold, first, second])
}

/// `gc.set_threshold(threshold0[, threshold1[, threshold2]])`.
fn set_threshold(args: &[i64]) -> i64 {
    if args.first().is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            "set_threshold() missing required argument 'threshold0' (pos 1)",
        );
    }
    let mut numbers = [10i64; 3];
    for (slot, argument) in numbers.iter_mut().zip(args) {
        match value::as_integer(*argument) {
            Some(number) => *slot = number,
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "set_threshold() argument must be int, not {}",
                        value::type_name(*argument)
                    ),
                )
            }
        }
    }
    let zero = usize::try_from(numbers[0]).unwrap_or(0);
    heap::set_collection_threshold(zero);
    set_registry_int(THRESHOLD1_KEY, numbers[1]);
    set_registry_int(THRESHOLD2_KEY, numbers[2]);
    NONE
}

fn get_debug() -> i64 {
    ops::int_result(registry_int(DEBUG_KEY, 0))
}

fn set_debug(argument: i64) -> i64 {
    match value::as_integer(argument) {
        Some(flags) => {
            set_registry_int(DEBUG_KEY, flags);
            NONE
        }
        None => exc::raise_with_message(
            TYPE_ERROR,
            &format!("set_debug() argument must be int, not {}", value::type_name(argument)),
        ),
    }
}

/// `gc.get_objects(generation=None)`: the live registry.
///
/// All live objects belong to the collector's single generation, reported as
/// generation 0; generations 1 and 2 have no objects of their own.
fn get_objects(args: &[i64]) -> i64 {
    let generation = match args.first().copied() {
        None | Some(NONE) => None,
        Some(argument) => match value::as_integer(argument) {
            Some(generation @ 0..=2) => Some(generation as usize),
            Some(_) => {
                return exc::raise_with_message(
                    VALUE_ERROR,
                    "generation must be one of 0, 1 or 2",
                )
            }
            None => {
                return exc::raise_with_message(TYPE_ERROR, "generation must be an integer")
            }
        },
    };
    if matches!(generation, Some(1 | 2)) {
        return value::list_new(Vec::new());
    }
    let values = heap::live_object_values();
    let roots = heap::root_values(&values);
    let list = value::list_new(values);
    heap::release_roots(roots);
    list
}

/// The direct references (`tp_traverse` edges) of one object.
fn referents_of(obj: i64) -> Vec<i64> {
    if let Some(items) = value::as_list(obj) {
        return items.to_vec();
    }
    if let Some(items) = value::as_tuple(obj) {
        return items.to_vec();
    }
    if let Some(items) = value::as_set(obj) {
        return items.to_vec();
    }
    if let Some(pairs) = value::as_dict(obj) {
        let mut items = Vec::with_capacity(pairs.len() * 2);
        for &(key, item) in pairs {
            items.push(key);
            items.push(item);
        }
        return items;
    }
    if let Some(pairs) = value::as_namespace(obj) {
        return pairs.iter().map(|(_, item)| *item).collect();
    }
    if let Some(class) = value::instance_class_value(obj) {
        let mut items = vec![class];
        if let Some(attributes) = value::instance_attributes(obj) {
            items.extend(attributes.snapshot().into_iter().map(|(_, item)| item));
        }
        return items;
    }
    if value::as_class(obj).is_some() {
        let mut items = Vec::new();
        for ancestor in value::class_mro(obj) {
            items.extend(value::class_namespace(ancestor).into_iter().map(|(_, item)| item));
        }
        return items;
    }
    Vec::new()
}

/// `gc.get_referents(*objs)`: direct references of every argument.
fn get_referents(args: &[i64]) -> i64 {
    let result = value::list_new(Vec::new());
    let roots = heap::root_values(&[result]);
    for &obj in args {
        for referent in referents_of(obj) {
            crate::container::list_append(result, referent);
        }
    }
    heap::release_roots(roots);
    result
}

/// `gc.get_referrers(*objs)`: live objects whose direct references include any
/// argument (identity comparison, like CPython).
fn get_referrers(args: &[i64]) -> i64 {
    if args.is_empty() {
        return value::list_new(Vec::new());
    }
    let candidates = heap::live_object_values();
    let roots = heap::root_values(&candidates);
    let result = value::list_new(Vec::new());
    let result_roots = heap::root_values(&[result]);
    for &candidate in &candidates {
        let referents = referents_of(candidate);
        if args.iter().any(|target| referents.contains(target)) {
            crate::container::list_append(result, candidate);
        }
    }
    heap::release_roots(result_roots);
    heap::release_roots(roots);
    result
}

/// `gc.freeze()`: pin every live object as a persistent root so the collector
/// never reclaims it.
fn freeze() -> i64 {
    let mut state = frozen_state();
    if !state.slots.is_empty() {
        return NONE;
    }
    let values = heap::live_object_values();
    let roots = heap::root_values(&values);
    for &item in &values {
        state.slots.push(heap::add_persistent_root(item));
    }
    heap::release_roots(roots);
    NONE
}

/// `gc.unfreeze()`: release the roots installed by `freeze()`.
fn unfreeze() -> i64 {
    let slots = {
        let mut state = frozen_state();
        std::mem::take(&mut state.slots)
    };
    for slot in slots {
        heap::remove_persistent_root(slot);
    }
    NONE
}

fn get_freeze_count() -> i64 {
    int(frozen_state().slots.len() as i64)
}

/// `gc.garbage`: the persistent list of uncollectable objects.
pub(crate) fn garbage_list() -> i64 {
    persistent_list(GARBAGE_KEY)
}

/// `gc.callbacks`: the persistent callback list.
fn callbacks_list() -> i64 {
    persistent_list(CALLBACKS_KEY)
}

fn persistent_list(key: &str) -> i64 {
    let start = heap::root_values(&[]);
    let list = match globals::registry_get(key) {
        Some(cached) => cached,
        None => {
            let list = value::list_new(Vec::new());
            heap::push_scoped_root(list);
            // The registry roots the value before publishing it.
            globals::registry_set(key, list);
            list
        }
    };
    heap::release_roots(start);
    list
}

/// Materialises a `gc` constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "garbage" => garbage_list(),
        "callbacks" => callbacks_list(),
        "DEBUG_STATS" => ops::int_result(DEBUG_STATS),
        "DEBUG_COLLECTABLE" => ops::int_result(DEBUG_COLLECTABLE),
        "DEBUG_UNCOLLECTABLE" => ops::int_result(DEBUG_UNCOLLECTABLE),
        "DEBUG_SAVEALL" => ops::int_result(DEBUG_SAVEALL),
        "DEBUG_LEAK" => ops::int_result(DEBUG_LEAK),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'gc' has no attribute '{name}'"),
        ),
    }
}

/// `gc.is_tracked(obj)`: whether the collector tracks `obj`.
fn is_tracked(object: i64) -> i64 {
    value::bool_value(is_tracked_value(object))
}

/// The classification behind [`is_tracked`], matching CPython 3.14's
/// `_PyObject_GC_IS_TRACKED` answers for the value forms the runtime models:
/// immutable scalar payloads, `bytearray`, builtin type spellings and plain
/// `object()` instances carry no collector fields; mutable containers,
/// modules, user classes and user instances do. A tuple is tracked exactly
/// when one of its elements is.
fn is_tracked_value(object: i64) -> bool {
    if !value::is_heap(object) {
        return false;
    }
    // Immutable payloads are never tracked.
    if value::as_str(object).is_some()
        || value::as_bytes(object).is_some()
        || value::as_float(object).is_some()
        || value::as_bytearray(object).is_some()
        || value::is_bigint(object)
    {
        return false;
    }
    if let Some(items) = value::as_tuple(object) {
        return items.iter().any(|&item| is_tracked_value(item));
    }
    // Builtin type spellings (`int`, `object`, `type`, ...) are function
    // values, and builtins carry no collector state.
    if let Some(function) = value::as_function(object) {
        return !matches!(function.body, crate::value::FunctionBody::Builtin(_));
    }
    if let Some(class) = value::instance_class_value(object) {
        // A bare `object()` has no collector fields in CPython.
        return class != value::object_class();
    }
    true
}

/// Dispatches a native `gc` function. Arity is enforced by the central spec.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "collect" => collect(args),
        "is_tracked" => is_tracked(args.first().copied().unwrap_or(NONE)),
        // The collector's enable switch is part of its configuration; automatic
        // collection is never suppressed because there is no switch to turn
        // off, so `isenabled` is always true and `enable` is idempotent.
        "isenabled" => value::TRUE,
        "enable" => NONE,
        "disable" => NONE,
        "get_count" => {
            let pending = {
                let state = collector_state();
                heap::objects().saturating_sub(state.last_object_count)
            };
            value::tuple_new(vec![int(pending as i64), int(0), int(0)])
        }
        "get_stats" => get_stats(),
        "get_threshold" => get_threshold(),
        "set_threshold" => set_threshold(args),
        "get_debug" => get_debug(),
        "set_debug" => set_debug(args.first().copied().unwrap_or(NONE)),
        "get_objects" => get_objects(args),
        "get_referents" => get_referents(args),
        "get_referrers" => get_referrers(args),
        "freeze" => freeze(),
        "unfreeze" => unfreeze(),
        "get_freeze_count" => get_freeze_count(),
        // The collector never finalizes objects, so nothing is finalized.
        "is_finalized" => value::FALSE,
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
    fn debug_flags_round_trip() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        assert_eq!(set_debug(ops::int_result(DEBUG_LEAK)), NONE);
        assert_eq!(get_debug(), ops::int_result(DEBUG_LEAK));
        assert_eq!(set_debug(ops::int_result(0)), NONE);
        assert_eq!(get_debug(), ops::int_result(0));
        assert!(!exc::pending());
    }

    #[test]
    fn thresholds_round_trip() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        let args = [ops::int_result(1), ops::int_result(2), ops::int_result(3)];
        assert_eq!(set_threshold(&args), NONE);
        let values = call("get_threshold", &[]);
        let items = value::as_tuple(values).expect("threshold tuple");
        assert_eq!(value::as_integer(items[0]), Some(1));
        assert_eq!(value::as_integer(items[1]), Some(2));
        assert_eq!(value::as_integer(items[2]), Some(3));
        assert!(!exc::pending());
    }

    #[test]
    fn referents_cover_containers_and_scalars() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        let one = ops::int_result(1);
        let two = ops::int_result(2);
        let list = value::list_new(vec![one, two]);
        let got = call("get_referents", &[list]);
        let items = value::as_list(got).expect("referents list");
        assert_eq!(items.len(), 2);
        assert_eq!((items[0], items[1]), (one, two));
        assert_eq!(
            value::as_list(call("get_referents", &[one])).map(|items| items.len()),
            Some(0)
        );
        assert!(!exc::pending());
    }

    #[test]
    fn freeze_pins_and_unfreezes() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        let before = get_freeze_count();
        assert_eq!(before, ops::int_result(0));
        call("freeze", &[]);
        assert!(
            value::as_integer(call("get_freeze_count", &[])).unwrap_or(0) > 0,
            "freeze pins live objects"
        );
        call("unfreeze", &[]);
        assert_eq!(call("get_freeze_count", &[]), ops::int_result(0));
        assert!(!exc::pending());
    }

    #[test]
    fn is_tracked_matches_cpython_classification() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        let tracked = [
            value::list_new(Vec::new()),
            value::dict_new(Vec::new()),
            value::set_new(Vec::new()),
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
            value::bytearray_new(Vec::new()),
            value::tuple_new(Vec::new()),
            value::instance_new(value::object_class()),
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
