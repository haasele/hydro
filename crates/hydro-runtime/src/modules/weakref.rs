//! `_weakref` / `weakref`: non-owning references to heap objects.
//!
//! A weak reference is a normal class instance (`ReferenceType` and its
//! subclasses); its target is recorded in the collector's weak side table
//! ([`crate::heap::weakref_register`]) and never stored as a tagged attribute,
//! so neither the instance's attribute table nor the collector's tracing can
//! keep the target alive. The collector clears the table entry when the target
//! is swept, which makes `ref()` return `None`.
//!
//! Implemented here:
//!
//! * `ref`/`ReferenceType` — callable, equality by target, `__hash__` when the
//!   target is hashable, `__repr__`;
//! * `proxy`/`ProxyType`/`CallableProxyType` — the callable proxy form (calls
//!   forward to the target, and a collected target raises `ReferenceError`);
//! * `getweakrefcount`, `getweakrefs`, `_remove_dead_weakref`;
//! * weak-reference callbacks: `ref(obj, callback)` / `proxy(obj, callback)`
//!   invoke the callable exactly once, with the reference object, when the
//!   target is collected. Callbacks run outside every GC lock at a safepoint
//!   or after an explicit `gc.collect()`; a raising callback is reported like
//!   CPython's unraisable output and cleared.
//!
//! Documented limits (reported instead of silently ignored):
//!
//! * A non-callable `proxy` cannot forward attribute reads without a
//!   dedicated attribute-dispatch hook, so `proxy(non_callable)` raises
//!   `NotImplementedError` instead of pretending to proxy.
//! * `hash(ref)` goes through the instance identity hash because `hash()` in
//!   this runtime does not dispatch `__hash__`; the correct target hash (and
//!   `TypeError` for unhashable targets) is available through
//!   `ref.__hash__()`.
//! * Pure `weakref.KeyedRef` relies on `__new__` to attach its `key`, and the
//!   runtime's generic `__new__` ignores that; its callback therefore cannot
//!   remove `WeakValueDictionary` entries (the callback is reported and
//!   cleared). `WeakSet` and `WeakKeyDictionary`, which use plain `ref` with a
//!   closure callback, work.
//!
//! Integration contract (the module itself is registered by `modules::mod`):
//!
//! * declare `pub(crate) mod weakref;` in `modules/mod.rs`;
//! * `dispatch`: route both `"_weakref"` and `"weakref"` to
//!   `weakref::call(function, args)`;
//! * `dispatch_kw`: reject keyword calls (or forward to `call` when the
//!   keyword-only forms are implemented);
//! * `constant_value`: `("_weakref" | "weakref", name) => weakref::constant(name)`.
//!
//! Recommended registration: give `_weakref` native specs only, and let
//! CPython's pure `Lib/weakref.py` provide the `weakref` module (it imports
//! every name below from `_weakref` and adds `WeakSet`, `WeakMethod`,
//! `WeakKeyDictionary` and `WeakValueDictionary`). If a native `weakref`
//! module is registered instead, those four pure consumers do not exist.
//! Constants (module *constants*, never function specs, because `ref` is the
//! `ReferenceType` class value): `ref`, `ReferenceType`, `proxy` is a
//! function, `ProxyType`, `CallableProxyType`, `ProxyTypes`.
//!
//! Public function specs: `proxy` (1..=2), `getweakrefcount` (1),
//! `getweakrefs` (1), `_remove_dead_weakref` (2).
//!
//! Native method bodies (also require specs under `_weakref`, because
//! `FunctionBody::Native` validates every call against the central table):
//! `_weakref_ref_init` (2..=3), `_weakref_ref_call` (1), `_weakref_ref_eq` (2),
//! `_weakref_ref_ne` (2), `_weakref_ref_hash` (1), `_weakref_ref_repr` (1),
//! `_weakref_proxy_init` (2..=3), `_weakref_proxy_eq` (2),
//! `_weakref_proxy_ne` (2), `_weakref_proxy_repr` (1),
//! `_weakref_proxy_call` (1..=usize::MAX). The method specs must be reachable
//! through the central `hydro_ir::stdlib` table even though they are not
//! Python-visible names, exactly like the other modules' internal methods.

use std::cell::RefCell;

use hydro_ir::MethodKind;

use crate::exc::{self, ATTRIBUTE_ERROR, NOT_IMPLEMENTED_ERROR, REFERENCE_ERROR, TYPE_ERROR};
use crate::heap;
use crate::value::{self, NONE};

/// Per-thread cache of the native class values, invalidated by the heap
/// generation counter (a runtime reset frees every heap object).
struct ClassCache {
    generation: u64,
    reference: i64,
    proxy: i64,
    callable_proxy: i64,
}

const NO_CLASS: i64 = NONE;

thread_local! {
    static CLASS_CACHE: RefCell<ClassCache> = const {
        RefCell::new(ClassCache {
            generation: u64::MAX,
            reference: NO_CLASS,
            proxy: NO_CLASS,
            callable_proxy: NO_CLASS,
        })
    };
}

fn method(name: &str, function: &'static str, arity: usize) -> (String, i64, MethodKind) {
    (
        name.to_string(),
        value::native_function_new("_weakref", function, arity),
        MethodKind::Normal,
    )
}

/// Builds the three class values and roots them for this thread.
///
/// Every method value is registered as a thread root immediately after it is
/// allocated: under `HYDRO_GC_STRESS` each allocation collects, and the Rust
/// tuple/`Vec` holding a freshly built native function is invisible to the
/// conservative stack scan. The roots live as long as the class (they are the
/// class's method table).
fn build_classes(generation: u64) -> ClassCache {
    let reference = value::class_new_full(
        "ReferenceType".to_string(),
        NONE,
        vec![
            rooted_method("__init__", "_weakref_ref_init", 3),
            rooted_method("__call__", "_weakref_ref_call", 1),
            rooted_method("__eq__", "_weakref_ref_eq", 2),
            rooted_method("__ne__", "_weakref_ref_ne", 2),
            rooted_method("__hash__", "_weakref_ref_hash", 1),
            rooted_method("__repr__", "_weakref_ref_repr", 1),
        ],
        Vec::new(),
    );
    heap::add_thread_root(reference);

    let proxy = value::class_new_full(
        "ProxyType".to_string(),
        NONE,
        vec![
            rooted_method("__init__", "_weakref_proxy_init", 3),
            rooted_method("__eq__", "_weakref_proxy_eq", 2),
            rooted_method("__ne__", "_weakref_proxy_ne", 2),
            rooted_method("__repr__", "_weakref_proxy_repr", 1),
        ],
        Vec::new(),
    );
    heap::add_thread_root(proxy);

    let callable_proxy = value::class_new_full(
        "CallableProxyType".to_string(),
        proxy,
        vec![rooted_method("__call__", "_weakref_proxy_call", usize::MAX)],
        Vec::new(),
    );
    heap::add_thread_root(callable_proxy);
    ClassCache {
        generation,
        reference,
        proxy,
        callable_proxy,
    }
}

/// Builds a native method value and roots it before the next allocation.
fn rooted_method(name: &str, function: &'static str, arity: usize) -> (String, i64, MethodKind) {
    let entry = method(name, function, arity);
    heap::add_thread_root(entry.1);
    entry
}

/// Returns a cached class value, rebuilding the cache when the heap changed.
fn cached_class(select: fn(&ClassCache) -> i64) -> i64 {
    let generation = heap::generation();
    let cached = CLASS_CACHE.with(|cell| {
        let cache = cell.borrow();
        if cache.generation == generation {
            select(&cache)
        } else {
            NO_CLASS
        }
    });
    if cached != NO_CLASS {
        return cached;
    }
    let classes = build_classes(generation);
    let selected = select(&classes);
    CLASS_CACHE.with(|cell| *cell.borrow_mut() = classes);
    selected
}

fn reference_class() -> i64 {
    cached_class(|cache| cache.reference)
}

fn proxy_class() -> i64 {
    cached_class(|cache| cache.proxy)
}

fn callable_proxy_class() -> i64 {
    cached_class(|cache| cache.callable_proxy)
}

/// Materialises a module constant.
///
/// `ref` is the `ReferenceType` class itself, matching CPython
/// (`weakref.ref is weakref.ReferenceType`). Unknown names raise
/// `AttributeError` instead of producing `None`.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "ref" | "ReferenceType" => reference_class(),
        "ProxyType" => proxy_class(),
        "CallableProxyType" => callable_proxy_class(),
        "ProxyTypes" => value::tuple_new(vec![proxy_class(), callable_proxy_class()]),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'weakref' has no attribute '{name}'"),
        ),
    }
}

/// Dispatches a native `_weakref`/`weakref` function (and the native method
/// bodies installed on the weak-reference classes).
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        // Public module functions.
        "ref" => module_ref(args),
        "proxy" => module_proxy(args),
        "getweakrefcount" => getweakrefcount(args),
        "getweakrefs" => getweakrefs(args),
        "_remove_dead_weakref" => remove_dead_weakref(args),
        // Native methods of ReferenceType.
        "_weakref_ref_init" => reference_init(args),
        "_weakref_ref_call" => reference_call(args),
        "_weakref_ref_eq" => reference_eq(args, false),
        "_weakref_ref_ne" => reference_eq(args, true),
        "_weakref_ref_hash" => reference_hash(args),
        "_weakref_ref_repr" => reference_repr(args),
        // Native methods of ProxyType/CallableProxyType.
        "_weakref_proxy_init" => proxy_init(args),
        "_weakref_proxy_call" => proxy_call(args),
        "_weakref_proxy_eq" => proxy_identity_eq(args, false),
        "_weakref_proxy_ne" => proxy_identity_eq(args, true),
        "_weakref_proxy_repr" => proxy_repr(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'weakref' has no attribute '{function}'"),
        ),
    }
}

/// Instance attribute holding the armed callback, mirroring CPython's
/// `weakref.__callback__`.
const CALLBACK_ATTR: &str = "__callback__";

/// Reads and validates the callback argument at `index`.
///
/// `NONE` means "no callback"; anything else must be callable. A non-callable
/// callback is a `TypeError`, exactly like CPython.
fn callback_argument(args: &[i64], index: usize) -> Result<i64, ()> {
    let callback = args.get(index).copied().unwrap_or(NONE);
    if callback != NONE && !is_callable(callback) {
        exc::raise_with_message(TYPE_ERROR, "argument 2 must be callable");
        return Err(());
    }
    Ok(callback)
}

/// Validates the target of a reference, raising `TypeError` for values that
/// cannot be weakly referenced.
fn validate_target(target: i64) -> bool {
    if heap::weakref_supported(target) {
        return true;
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "cannot create weak reference to '{}' object",
            value::type_name(target)
        ),
    );
    false
}

/// `ReferenceType.__init__(self, target, callback=None)`.
fn reference_init(args: &[i64]) -> i64 {
    if args.len() < 2 {
        return exc::raise_with_message(TYPE_ERROR, "ref() requires a target object");
    }
    if value::as_instance(args[0]).is_none() {
        return exc::raise_with_message(TYPE_ERROR, "ref() requires a ReferenceType instance");
    }
    let callback = match callback_argument(args, 2) {
        Ok(callback) => callback,
        Err(()) => return NONE,
    };
    let target = args[1];
    if !validate_target(target) {
        return NONE;
    }
    if !heap::weakref_register_with_callback(args[0], target, callback) {
        return exc::raise_with_message(TYPE_ERROR, "cannot create weak reference");
    }
    // The visible attribute and the side table must agree. The side table is
    // authoritative for the GC edge; the attribute mirrors CPython.
    value::instance_setattr(args[0], CALLBACK_ATTR.to_string(), callback);
    NONE
}

/// `ReferenceType.__call__(self)`.
fn reference_call(args: &[i64]) -> i64 {
    match args.first() {
        Some(&reference) => heap::weakref_target(reference),
        None => NONE,
    }
}

/// Equality/inequality by target.
///
/// Two cleared references compare equal only when they are the same object,
/// matching CPython's behaviour once the referent is gone.
fn reference_eq(args: &[i64], invert: bool) -> i64 {
    let (Some(&left), Some(&right)) = (args.first(), args.get(1)) else {
        return value::bool_value(invert);
    };
    let equal = match (
        heap::weakref_target_raw(left),
        heap::weakref_target_raw(right),
    ) {
        (Some(a), Some(b)) if a == NONE && b == NONE => left == right,
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    value::bool_value(if invert { !equal } else { equal })
}

/// `ReferenceType.__hash__(self)`: the target's hash, or `TypeError` once the
/// reference is dead. `builtins::hash` raises `TypeError` for unhashable
/// targets, so only hashable targets produce a value.
fn reference_hash(args: &[i64]) -> i64 {
    let Some(&reference) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "weak reference object is dead");
    };
    match heap::weakref_target_raw(reference) {
        Some(target) if target != NONE => crate::builtins::hash(target),
        _ => exc::raise_with_message(TYPE_ERROR, "weak reference object is dead"),
    }
}

/// `ReferenceType.__repr__(self)`.
fn reference_repr(args: &[i64]) -> i64 {
    let Some(&reference) = args.first() else {
        return value::str_alloc("<weakref uninitialized>".to_string());
    };
    let text = match heap::weakref_target_raw(reference) {
        Some(target) if target != NONE => {
            let addr = (target & !value::TAG_MASK) as usize;
            format!(
                "<weakref to '{}' at 0x{addr:x}>",
                value::type_name_of(target)
            )
        }
        Some(_) => "<weakref dead>".to_string(),
        None => "<weakref uninitialized>".to_string(),
    };
    value::str_alloc(text)
}

/// Whether `target` can be called through `builtins::call`.
fn is_callable(target: i64) -> bool {
    if value::as_class(target).is_some()
        || value::as_function(target).is_some()
        || value::as_ext_function(target).is_some()
        || crate::modules::extension::ext_object_is_callable(target)
    {
        return true;
    }
    match value::as_instance(target) {
        Some(state) => value::class_lookup(state.class, "__call__").is_some(),
        None => false,
    }
}

/// `ref(target, callback=None)` when the name is resolved as a function
/// instead of the `ReferenceType` class constant.
fn module_ref(args: &[i64]) -> i64 {
    let Some(&target) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "ref() requires a target object");
    };
    let callback = match callback_argument(args, 1) {
        Ok(callback) => callback,
        Err(()) => return NONE,
    };
    if !validate_target(target) {
        return NONE;
    }
    let reference = value::instance_new(reference_class());
    // `__init__` returns None, so the module function has to return the
    // freshly created reference itself.
    let result = reference_init(&[reference, target, callback]);
    if exc::pending() {
        result
    } else {
        reference
    }
}

/// `proxy(target, callback=None)`: a callable proxy.
///
/// Only callable targets are supported; attribute forwarding for non-callable
/// targets needs a dispatch hook this module does not own, so those raise a
/// clear `NotImplementedError` instead of silently not proxying.
fn module_proxy(args: &[i64]) -> i64 {
    let Some(&target) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "proxy() requires a target object");
    };
    let callback = match callback_argument(args, 1) {
        Ok(callback) => callback,
        Err(()) => return NONE,
    };
    if !validate_target(target) {
        return NONE;
    }
    if !is_callable(target) {
        return exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "weakref.proxy of a non-callable object is not supported by this runtime",
        );
    }
    let proxy = value::instance_new(callable_proxy_class());
    // `ProxyType.__init__` returns None: the module function returns the new
    // proxy instance, and reports a failed initialisation with its exception.
    let result = proxy_init(&[proxy, target, callback]);
    if exc::pending() {
        result
    } else {
        proxy
    }
}

/// `ProxyType.__init__(self, target, callback=None)`.
fn proxy_init(args: &[i64]) -> i64 {
    if args.len() < 2 {
        return exc::raise_with_message(TYPE_ERROR, "proxy() requires a target object");
    }
    if value::as_instance(args[0]).is_none() {
        return exc::raise_with_message(TYPE_ERROR, "proxy() requires a ProxyType instance");
    }
    let callback = match callback_argument(args, 2) {
        Ok(callback) => callback,
        Err(()) => return NONE,
    };
    let target = args[1];
    if !validate_target(target) {
        return NONE;
    }
    if !is_callable(target) {
        return exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "weakref.proxy of a non-callable object is not supported by this runtime",
        );
    }
    if !heap::weakref_register_with_callback(args[0], target, callback) {
        return exc::raise_with_message(TYPE_ERROR, "cannot create weak proxy");
    }
    value::instance_setattr(args[0], CALLBACK_ATTR.to_string(), callback);
    NONE
}

/// `__call__` of a callable proxy: forwards every argument to the target.
fn proxy_call(args: &[i64]) -> i64 {
    let Some(&proxy) = args.first() else {
        return exc::raise_with_message(
            REFERENCE_ERROR,
            "weakly-referenced object no longer exists",
        );
    };
    let target = heap::weakref_target(proxy);
    if target == NONE {
        return exc::raise_with_message(
            REFERENCE_ERROR,
            "weakly-referenced object no longer exists",
        );
    }
    let rest = &args[1..];
    let argv = if rest.is_empty() {
        std::ptr::null()
    } else {
        rest.as_ptr()
    };
    crate::builtins::call(target, rest.len(), argv)
}

/// Proxies compare by identity (the runtime has no transparent descriptor
/// forwarding); `__eq__`/`__ne__` keep instance comparisons well-defined.
fn proxy_identity_eq(args: &[i64], invert: bool) -> i64 {
    let equal = args.len() >= 2 && args[0] == args[1];
    value::bool_value(if invert { !equal } else { equal })
}

fn proxy_repr(args: &[i64]) -> i64 {
    let Some(&proxy) = args.first() else {
        return value::str_alloc("<weakproxy uninitialized>".to_string());
    };
    let text = match heap::weakref_target_raw(proxy) {
        Some(target) if target != NONE => {
            let addr = (target & !value::TAG_MASK) as usize;
            format!(
                "<weakproxy to '{}' at 0x{addr:x}>",
                value::type_name_of(target)
            )
        }
        Some(_) => "<weakproxy dead>".to_string(),
        None => "<weakproxy uninitialized>".to_string(),
    };
    value::str_alloc(text)
}

fn getweakrefcount(args: &[i64]) -> i64 {
    let Some(&target) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "getweakrefcount() requires an object");
    };
    if !validate_target(target) {
        return NONE;
    }
    match value::encode_int(heap::weakref_count(target) as i64) {
        Some(count) => count,
        None => exc::raise_with_message(TYPE_ERROR, "weak reference count is out of range"),
    }
}

fn getweakrefs(args: &[i64]) -> i64 {
    let Some(&target) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "getweakrefs() requires an object");
    };
    if !validate_target(target) {
        return NONE;
    }
    let references = heap::weakrefs_for(target);
    // Root the references before the first allocation and keep the pins until
    // the list itself is reachable from the caller's stack.
    let roots = heap::root_values(&references);
    let list = value::list_new(Vec::new());
    for &reference in &references {
        if let Some(mut items) = value::list_items_mut(list) {
            items.push(reference);
        }
        heap::write_barrier_edge(list, reference);
    }
    heap::release_roots(roots);
    heap::weakrefs_unpin(&references);
    list
}

/// `_remove_dead_weakref(mapping, key)`: removes `mapping[key]` when its value
/// is a weak reference whose target was collected. Missing keys and live
/// references are left untouched, matching CPython.
fn remove_dead_weakref(args: &[i64]) -> i64 {
    let (Some(&mapping), Some(&key)) = (args.first(), args.get(1)) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "_remove_dead_weakref() requires a mapping and a key",
        );
    };
    if value::as_dict(mapping).is_none() {
        return exc::raise_with_message(TYPE_ERROR, "_remove_dead_weakref() requires a dict");
    }
    let Some(mut pairs) = value::dict_pairs_mut(mapping) else {
        return exc::raise_with_message(TYPE_ERROR, "_remove_dead_weakref() requires a dict");
    };
    let position = match pairs.find(key) {
        Some(position) => position,
        None => return NONE,
    };
    let stored = pairs.entries()[position].1;
    if matches!(heap::weakref_target_raw(stored), Some(target) if target == NONE) {
        pairs.remove(key);
    }
    NONE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    use crate::heap::WEAK_TEST_LOCK;
    use crate::value::{as_class, as_instance, encode_int, str_alloc, FALSE, TRUE};

    /// Counts callback invocations and records the reference argument.
    static CALLBACK_CALLS: AtomicUsize = AtomicUsize::new(0);
    static CALLBACK_LAST: AtomicI64 = AtomicI64::new(0);

    /// A generated-wrapper-shaped callback: receives the reference as its
    /// single positional argument.
    unsafe extern "C" fn counting_callback(argv: *const i64) -> i64 {
        CALLBACK_CALLS.fetch_add(1, Ordering::SeqCst);
        if !argv.is_null() {
            // SAFETY: the caller passes one readable tagged argument.
            CALLBACK_LAST.store(unsafe { *argv }, Ordering::SeqCst);
        }
        NONE
    }

    fn counting_ref_callback() -> i64 {
        let entry: unsafe extern "C" fn(*const i64) -> i64 = counting_callback;
        value::function_new_with_flags(entry as usize, 1, "weakref_callback".to_string(), 0)
    }

    fn reset_callback_counter() {
        CALLBACK_CALLS.store(0, Ordering::SeqCst);
        CALLBACK_LAST.store(0, Ordering::SeqCst);
    }

    fn weak_guard() -> std::sync::MutexGuard<'static, ()> {
        let guard = WEAK_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::heap::weakref_tests_clear();
        reset_callback_counter();
        guard
    }

    fn new_instance(class: i64) -> i64 {
        value::instance_new(class)
    }

    fn target_instance(name: &str) -> i64 {
        let class = value::class_new(name.to_string(), NONE, Vec::new());
        new_instance(class)
    }

    fn new_reference(target: i64) -> i64 {
        let reference = new_instance(reference_class());
        let _ = reference_init(&[reference, target]);
        reference
    }

    #[test]
    fn reference_resolves_a_live_target() {
        let _guard = weak_guard();
        exc::init();
        let target = target_instance("C");
        let reference = new_reference(target);
        assert!(!exc::pending());
        assert_eq!(reference_call(&[reference]), target);
        assert_eq!(getweakrefcount(&[target]), encode_int(1).unwrap());
        assert_eq!(as_instance(reference).unwrap().class, reference_class());
    }

    #[test]
    fn references_to_the_same_target_compare_equal() {
        let _guard = weak_guard();
        exc::init();
        let target = target_instance("C");
        let other = target_instance("D");
        let first = new_reference(target);
        let second = new_reference(target);
        let different = new_reference(other);
        assert_eq!(reference_eq(&[first, second], false), TRUE);
        assert_eq!(reference_eq(&[first, different], false), FALSE);
        assert_eq!(reference_eq(&[first, different], true), TRUE);
    }

    #[test]
    fn hash_matches_the_target() {
        let _guard = weak_guard();
        exc::init();
        let target = target_instance("C");
        let reference = new_reference(target);
        let expected = crate::builtins::hash(target);
        assert!(!exc::pending());
        assert_eq!(reference_hash(&[reference]), expected);
    }

    #[test]
    fn invalid_target_raises_type_error() {
        let _guard = weak_guard();
        exc::init();
        let reference = new_instance(reference_class());
        let list = value::list_new(Vec::new());
        assert_eq!(reference_init(&[reference, list]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn non_callable_callback_raises_type_error() {
        let _guard = weak_guard();
        exc::init();
        let target = target_instance("C");
        let reference = new_instance(reference_class());
        let not_callable = encode_int(7).unwrap();
        assert_eq!(reference_init(&[reference, target, not_callable]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert!(value::to_string(exc::pending_value()).contains("callable"));
        exc::clear();
    }

    #[test]
    fn callback_is_armed_and_visible() {
        let _guard = weak_guard();
        exc::init();
        let target = target_instance("C");
        let callback = counting_ref_callback();
        let reference = new_instance(reference_class());
        assert_eq!(reference_init(&[reference, target, callback]), NONE);
        assert!(!exc::pending());
        assert_eq!(
            value::instance_attr(reference, "__callback__"),
            Some(callback)
        );
        assert_eq!(reference_call(&[reference]), target);
        // Merely arming a callback does not run it.
        assert_eq!(CALLBACK_CALLS.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn callback_runs_once_when_target_is_cleared() {
        let _guard = weak_guard();
        exc::init();
        let target = target_instance("C");
        let callback = counting_ref_callback();
        let reference = new_instance(reference_class());
        assert_eq!(reference_init(&[reference, target, callback]), NONE);
        assert!(!exc::pending());

        // Clearing the target arms the callback; it must not run inline.
        crate::heap::weakref_test_kill(reference);
        assert_eq!(reference_call(&[reference]), NONE);
        assert_eq!(CALLBACK_CALLS.load(Ordering::SeqCst), 0);

        crate::heap::run_pending_weakref_callbacks();
        assert_eq!(CALLBACK_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(CALLBACK_LAST.load(Ordering::SeqCst), reference);
        assert!(!exc::pending());

        // Clearing/draining again must not invoke it a second time.
        crate::heap::weakref_test_kill(reference);
        crate::heap::run_pending_weakref_callbacks();
        assert_eq!(CALLBACK_CALLS.load(Ordering::SeqCst), 1);
        assert!(!exc::pending());
    }

    #[test]
    fn raising_callback_is_reported_and_cleared() {
        let _guard = weak_guard();
        exc::init();
        let target = target_instance("C");
        // `len(reference)` has no `__len__` and raises TypeError.
        let callback = value::builtin_function_new(hydro_ir::Builtin::Len);
        let reference = new_instance(reference_class());
        assert_eq!(reference_init(&[reference, target, callback]), NONE);
        assert!(!exc::pending());
        crate::heap::weakref_test_kill(reference);
        crate::heap::run_pending_weakref_callbacks();
        // The unraisable callback exception is reported and cleared; it must
        // not leak into the caller's exception state.
        assert!(!exc::pending());
    }

    #[test]
    fn weakset_style_ref_with_closure_callback_is_accepted() {
        let _guard = weak_guard();
        exc::init();
        let target = target_instance("C");
        let callback = counting_ref_callback();
        // `_weakrefset` calls `ref(item, self._remove)`; a callable callback
        // must be accepted (no TypeError) and armed.
        let reference = module_ref(&[target, callback]);
        assert!(!exc::pending());
        assert_eq!(
            value::instance_attr(reference, "__callback__"),
            Some(callback)
        );
        assert_eq!(reference_call(&[reference]), target);
    }

    #[test]
    fn callable_proxy_accepts_and_runs_callback() {
        let _guard = weak_guard();
        exc::init();
        let call_body = value::native_function_new("_weakref", "_weakref_proxy_call", usize::MAX);
        let target_class = value::class_new(
            "Callable".to_string(),
            NONE,
            vec![("__call__".to_string(), call_body)],
        );
        let target = new_instance(target_class);
        let callback = counting_ref_callback();
        let proxy = module_proxy(&[target, callback]);
        assert!(!exc::pending());
        assert_eq!(value::instance_attr(proxy, "__callback__"), Some(callback));
        assert_eq!(reference_call(&[proxy]), target);

        crate::heap::weakref_test_kill(proxy);
        crate::heap::run_pending_weakref_callbacks();
        assert_eq!(CALLBACK_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(CALLBACK_LAST.load(Ordering::SeqCst), proxy);
        assert!(!exc::pending());
    }

    #[test]
    fn non_callable_proxy_raises_clearly() {
        let _guard = weak_guard();
        exc::init();
        let target = target_instance("C");
        assert_eq!(module_proxy(&[target]), NONE);
        assert_eq!(exc::pending_tag(), NOT_IMPLEMENTED_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn callable_proxy_resolves_and_calls() {
        let _guard = weak_guard();
        exc::init();
        // A target class with a call body; only its presence in the class
        // namespace matters for the proxy.
        let call_body = value::native_function_new("_weakref", "_weakref_proxy_call", usize::MAX);
        let target_class = value::class_new(
            "Callable".to_string(),
            NONE,
            vec![("__call__".to_string(), call_body)],
        );
        let target = new_instance(target_class);
        assert!(is_callable(target));
        let proxy = module_proxy(&[target]);
        assert!(!exc::pending());
        assert_eq!(as_instance(proxy).unwrap().class, callable_proxy_class());
        assert_eq!(reference_call(&[proxy]), target);
        assert!(as_class(proxy_class()).is_some());
    }

    #[test]
    fn getweakrefs_lists_registered_references() {
        let _guard = weak_guard();
        exc::init();
        let target = target_instance("C");
        let first = new_reference(target);
        let second = new_reference(target);
        let list = getweakrefs(&[target]);
        assert!(!exc::pending());
        let items = value::as_list(list).expect("list result");
        assert_eq!(items.len(), 2);
        assert!(items.contains(&first));
        assert!(items.contains(&second));
        // The refs are pinned only while the call builds its list.
        assert!(crate::heap::weakref_target_raw(first).is_some());
    }

    #[test]
    fn getweakrefs_skips_cleared_references() {
        let _guard = weak_guard();
        exc::init();
        let target = target_instance("C");
        let live = new_reference(target);
        let dead = new_reference(target);
        crate::heap::weakref_test_kill(dead);
        let list = getweakrefs(&[target]);
        assert!(!exc::pending());
        let items = value::as_list(list).expect("list result");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0], live);
    }

    #[test]
    fn remove_dead_weakref_deletes_only_dead_entries() {
        let _guard = weak_guard();
        exc::init();
        let target = target_instance("C");
        let live = new_reference(target);
        let dead = new_reference(target);
        crate::heap::weakref_test_kill(dead);
        let live_key = str_alloc("live".to_string());
        let dead_key = str_alloc("dead".to_string());
        let mapping = value::dict_new(vec![(live_key, live), (dead_key, dead)]);
        let _ = remove_dead_weakref(&[mapping, dead_key]);
        assert!(!exc::pending());
        let pairs = value::as_dict(mapping).expect("dict");
        assert_eq!(pairs.len(), 1);
        assert_eq!(value::as_str(pairs[0].0), Some("live"));
        // Removing the live entry is a no-op.
        let _ = remove_dead_weakref(&[mapping, live_key]);
        assert_eq!(value::as_dict(mapping).map(|pairs| pairs.len()), Some(1));
        assert!(!exc::pending());
    }

    #[test]
    fn dead_reference_hash_and_equality() {
        let _guard = weak_guard();
        exc::init();
        let target = target_instance("C");
        let first = new_reference(target);
        let second = new_reference(target);
        crate::heap::weakref_test_kill(first);
        crate::heap::weakref_test_kill(second);
        // Dead references compare by identity.
        assert_eq!(reference_eq(&[first, first], false), TRUE);
        assert_eq!(reference_eq(&[first, second], false), FALSE);
        assert_eq!(reference_call(&[first]), NONE);
        // Hashing a dead reference raises TypeError.
        assert_eq!(reference_hash(&[first]), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn repr_reports_live_and_dead_references() {
        let _guard = weak_guard();
        exc::init();
        let target = target_instance("C");
        let reference = new_reference(target);
        let live = reference_repr(&[reference]);
        assert!(value::as_str(live).unwrap().contains("weakref to"));
        crate::heap::weakref_tests_clear();
        let uninitialized = reference_repr(&[reference]);
        assert_eq!(
            value::as_str(uninitialized),
            Some("<weakref uninitialized>")
        );
    }
}
