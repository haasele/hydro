//! `_collections_abc` / `collections.abc`: the abstract base class objects.
//!
//! Each name is a real class value created once per heap generation and
//! exposed by both module spellings, so `collections.abc.Mapping is
//! _collections_abc.Mapping` holds within a thread. The classes mirror
//! CPython's single-inheritance projection of the ABC graph: the runtime
//! class model stores one base, so a class whose CPython definition has
//! several bases (`Collection(Sized, Iterable, Container)`) uses the
//! semantically primary one (`Iterable`) and documents the approximation.
//!
//! What works through the normal runtime paths:
//!
//! * `class MyMapping(Mapping): ...` — the class statement takes a native
//!   class value as its base, so `isinstance(instance, Mapping)` and
//!   `issubclass(MyMapping, Mapping)` are true through the ordinary base
//!   chain;
//! * `Mapping.register(WeakSet)` — every ABC is an `abc.ABCMeta` instance
//!   (as in CPython), so `register` is inherited from the metaclass and
//!   records a real virtual-subclass edge in [`crate::modules::abc`]. The
//!   builtin `isinstance`/`issubclass` consult that edge, so a registration
//!   changes later checks; the private `_abc_register` spelling answers
//!   through the same entry point;
//! * the class objects themselves (`Mapping.__name__`, identity, importing).
//!
//! What is deliberately not emulated: the structural `__subclasshook__`
//! bodies of the non-`Buffer` ABCs. The runtime cannot run a Python-level
//! hook implementation, so `isinstance({}, Mapping)` is only `True` after an
//! explicit `Mapping.register(dict)`; it is never inferred structurally.
//!
//! `Buffer` (PEP 688) is special in its hook, not its registration: it is an
//! `ABCMeta` class like every other ABC, it declares the abstract
//! `__buffer__(flags)` hook (`__abstractmethods__` reports `__buffer__`), and
//! it additionally carries `__subclasshook__`, whose structural answer the
//! native `isinstance`/`issubclass` paths reproduce: native exporters
//! (`bytes`, `bytearray`, `memoryview`) and classes declaring `__buffer__`
//! match without a registered edge.
//!
//! The class cache lives in the globals side store under
//! `\0hydro.registry`, which roots every cached class and is cleared by
//! `hydro_runtime_init`, so no class value can survive its heap.

use hydro_ir::MethodKind;

use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, NOT_IMPLEMENTED_ERROR, TYPE_ERROR};
use crate::heap;
use crate::modules::abc;
use crate::value::{self, FunctionBody, NONE};

/// Class name and (optional) single base, in declaration order. CPython's
/// multi-base classes keep the primary base; see the module docs.
const CLASSES: &[(&str, Option<&str>)] = &[
    ("Hashable", None),
    ("Awaitable", None),
    ("Coroutine", Some("Awaitable")),
    ("AsyncIterable", None),
    ("AsyncIterator", Some("AsyncIterable")),
    ("AsyncGenerator", Some("AsyncIterator")),
    ("Iterable", None),
    ("Iterator", Some("Iterable")),
    ("Generator", Some("Iterator")),
    ("Reversible", Some("Iterable")),
    ("Sized", None),
    ("Container", None),
    ("Callable", None),
    ("Collection", Some("Iterable")),
    ("Set", Some("Collection")),
    ("MutableSet", Some("Set")),
    ("Mapping", Some("Collection")),
    ("MutableMapping", Some("Mapping")),
    ("Sequence", Some("Collection")),
    ("MutableSequence", Some("Sequence")),
    ("MappingView", Some("Sized")),
    ("KeysView", Some("MappingView")),
    ("ItemsView", Some("MappingView")),
    ("ValuesView", Some("MappingView")),
    // PEP 688. Like every name above this one is an `ABCMeta` instance;
    // `Buffer` additionally declares `__buffer__` and `__subclasshook__`: see
    // [`buffer_class`].
    ("Buffer", None),
];

/// Registry key of the `Buffer` ABC (also its cache key).
const BUFFER_KEY: &str = "collections.abc.Buffer";

/// The primary base of `name`, if it declares one.
fn base_name(name: &str) -> Option<&'static str> {
    CLASSES
        .iter()
        .find(|(class, _)| *class == name)
        .and_then(|(_, base)| *base)
}

fn is_abc(name: &str) -> bool {
    CLASSES.iter().any(|(class, _)| *class == name)
}

/// Returns (creating on first use) the class object for `name`.
fn class_for(name: &str) -> i64 {
    if name == "Buffer" {
        return buffer_class();
    }
    let key = format!("collections.abc.{name}");
    if let Some(cached) = globals::registry_get(&key) {
        return cached;
    }
    let base = match base_name(name) {
        Some(base) => class_for(base),
        None => NONE,
    };
    // Every ABC is an `ABCMeta` instance, exactly like CPython: `register` is
    // not stored in the class namespace but inherited from the metaclass, so
    // `Mapping.register(X)` binds `Mapping` and records into the single
    // virtual-subclass registry in [`abc`] (`__abstractmethods__` is reported
    // the same way). The metaclass object itself is cached in the process-wide
    // registry, so no thread creates a second one.
    let metaclass = abc::constant("ABCMeta");
    let bases = if base == NONE { Vec::new() } else { vec![base] };
    // `bases` and `metaclass` are referenced only by this Rust frame until the
    // class owns them, so root them across the allocation.
    let roots = heap::root_values(&[metaclass, base]);
    let class = value::class_new_multi(name.to_string(), bases, metaclass, Vec::new(), Vec::new());
    heap::release_roots(roots);
    // Publish the class before any further allocation: the registry roots it
    // for the rest of the heap generation.
    globals::registry_set(&key, class);
    class
}

/// Returns (creating on first use) the `collections.abc.Buffer` class.
///
/// `Buffer` is an `ABCMeta` class like every other ABC (so `register` comes
/// from the metaclass), but its contract is also enforceable structurally: it
/// declares the abstract `__buffer__(flags)` hook and its `__subclasshook__`
/// answers structurally.
///
/// CPython implements the hook as `_check_methods(C, "__buffer__")`; the
/// runtime cannot run Python-level `__subclasshook__` implementations, so the
/// same rule is honoured natively by [`buffer_subclass_supported`] and the
/// builtin `isinstance`/`issubclass` paths. Native exporters (`bytes`,
/// `bytearray`, `memoryview`) satisfy it without a registered edge and without
/// carrying a Python-level `__buffer__` method.
fn buffer_class() -> i64 {
    if let Some(cached) = globals::registry_get(BUFFER_KEY) {
        return cached;
    }
    let base = abc::constant("ABC");
    let buffer = value::native_function_new("_collections_abc", "_abc_buffer", 2);
    let subclasshook =
        value::native_function_new("_collections_abc", "_abc_buffer_subclasshook", 2);
    // `Buffer.__buffer__` is `@abstractmethod`, so `__abstractmethods__`
    // reports it exactly as CPython does.
    abc::mark_abstract(buffer);
    // `__slots__ = ()`, as in CPython's definition.
    let slots = value::tuple_new(Vec::new());
    let roots = heap::root_values(&[base, buffer, subclasshook, slots]);
    let methods = vec![
        ("__buffer__".to_string(), buffer, MethodKind::Normal),
        (
            "__subclasshook__".to_string(),
            subclasshook,
            MethodKind::Classmethod,
        ),
    ];
    let attrs = vec![("__slots__".to_string(), slots)];
    let class = value::class_new_multi("Buffer".to_string(), vec![base], NONE, methods, attrs);
    heap::release_roots(roots);
    globals::registry_set(BUFFER_KEY, class);
    class
}

/// Whether `class` is the runtime's `collections.abc.Buffer` class.
///
/// The registry lookup performs no allocation: before `Buffer` is materialised
/// no value can name it, so a missing entry is a truthful "no".
pub(crate) fn is_buffer_class(class: i64) -> bool {
    globals::registry_get(BUFFER_KEY) == Some(class)
}

/// Whether `value` is a native buffer exporter (`bytes`, `bytearray`,
/// `memoryview`).
pub(crate) fn is_native_buffer_type(value: i64) -> bool {
    if let Some(function) = value::as_function(value) {
        if let FunctionBody::Builtin(builtin) = &function.body {
            return matches!(builtin.name(), "bytes" | "bytearray" | "memoryview");
        }
    }
    value::is_cached_type_class(value)
        && matches!(
            value::class_display_name(value),
            Some("bytes" | "bytearray" | "memoryview")
        )
}

/// `Buffer.__subclasshook__(C)` natively: `C` supports the buffer protocol
/// when it is a native exporter or declares `__buffer__` in its MRO.
pub(crate) fn buffer_subclass_supported(subclass: i64) -> bool {
    is_native_buffer_type(subclass) || value::class_lookup(subclass, "__buffer__").is_some()
}

/// `isinstance(value, Buffer)` for the structural (unregistered) case.
///
/// The virtual-subclass registry is consulted by the caller first; this covers
/// native exporters and user classes that declare `__buffer__` without
/// inheriting from `Buffer` (CPython's `__subclasshook__` behaviour).
pub(crate) fn buffer_instance_supported(value: i64) -> bool {
    if value::as_bytearray(value).is_some()
        || value::as_bytes(value).is_some()
        || value::as_memoryview(value).is_some()
    {
        return true;
    }
    let Some(instance) = value::as_instance(value) else {
        return false;
    };
    value::class_lookup(instance.class, "__buffer__").is_some()
}

/// `Buffer.__buffer__(self, flags)`: the abstract body, which raises like
/// CPython's `raise NotImplementedError`.
fn buffer_method(_args: &[i64]) -> i64 {
    exc::raise_with_message(
        NOT_IMPLEMENTED_ERROR,
        "Buffer.__buffer__ is abstract and cannot be called directly",
    )
}

/// `Buffer.__subclasshook__(cls, candidate)`: the native structural check.
fn buffer_subclasshook(args: &[i64]) -> i64 {
    let Some(&candidate) = args.get(1) else {
        return exc::raise_with_message(TYPE_ERROR, "Buffer.__subclasshook__() takes 2 arguments");
    };
    value::bool_value(buffer_subclass_supported(candidate))
}

/// Materialises a `collections.abc` / `_collections_abc` constant.
pub(crate) fn constant(name: &str) -> i64 {
    if is_abc(name) {
        return class_for(name);
    }
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module 'collections.abc' has no attribute '{name}'"),
    )
}

/// Dispatches a native `_collections_abc` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        // `ABCMeta.register` body. `X.register(Y)` on a class reaches it
        // through the metaclass with `[X, Y]`; this private spelling forwards
        // to the same registry instead of keeping a second one.
        "_abc_register" => abc::register_from_method(args),
        "_abc_buffer" => buffer_method(args),
        "_abc_buffer_subclasshook" => buffer_subclasshook(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'collections.abc' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins;
    use crate::value::class_new;

    /// The class cache, the abstract markers and the virtual-edge list all
    /// live in the process-wide globals side store; serialise the tests that
    /// reset or read them.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        crate::heap::heap_reset_test_guard()
    }

    fn reset() {
        exc::init();
        globals::reset();
    }

    /// Calls `abc_class.register(subclass)` exactly as the frontend emits a
    /// method call on a class value.
    fn register(abc_class: i64, subclass: i64) -> i64 {
        let name = b"register";
        let argv = [subclass];
        crate::methods::call(abc_class, name.as_ptr(), name.len(), 1, argv.as_ptr())
    }

    #[test]
    fn every_abc_is_an_abcmeta_class() {
        let _guard = guard();
        reset();
        for name in [
            "Hashable",
            "Iterable",
            "Mapping",
            "MutableSet",
            "Sequence",
            "Buffer",
        ] {
            let class = constant(name);
            assert!(!exc::pending(), "{name}");
            let metaclass = value::class_metaclass(class);
            assert_ne!(metaclass, NONE, "{name} has no explicit metaclass");
            assert_eq!(
                value::class_display_name(metaclass),
                Some("ABCMeta"),
                "{name}'s metaclass is not ABCMeta"
            );
            // `register` lives on the metaclass, as in CPython, and is
            // reachable through ordinary class lookup.
            assert!(
                value::class_lookup(metaclass, "register").is_some(),
                "{name} cannot reach ABCMeta.register"
            );
            // The ABC's own namespace does not shadow it.
            assert!(
                !value::class_namespace(class)
                    .iter()
                    .any(|(entry, _)| entry.as_str() == "register"),
                "{name} stores its own register"
            );
        }
    }

    #[test]
    fn mutable_set_registration_makes_weakset_an_instance() {
        let _guard = guard();
        reset();
        let mutable_set = constant("MutableSet");
        // `weakref.WeakSet` is a plain class registered from Python.
        let weak_set = class_new("WeakSet".to_string(), NONE, Vec::new());
        assert!(!exc::pending());
        assert_eq!(
            builtins::issubclass(weak_set, mutable_set),
            value::FALSE,
            "an unregistered class must not be a virtual subclass"
        );

        assert_eq!(register(mutable_set, weak_set), weak_set);
        assert!(!exc::pending());

        assert_eq!(builtins::issubclass(weak_set, mutable_set), value::TRUE);
        assert_eq!(
            builtins::isinstance(value::instance_new(weak_set), mutable_set),
            value::TRUE
        );
        // Subclasses of the registered class inherit the virtual edge.
        let derived = class_new("DerivedWeakSet".to_string(), weak_set, Vec::new());
        assert_eq!(builtins::issubclass(derived, mutable_set), value::TRUE);
        assert_eq!(
            builtins::isinstance(value::instance_new(derived), mutable_set),
            value::TRUE
        );
        // The edge was recorded on `MutableSet`, so its real ancestor `Set`
        // answers too (CPython walks the registries of every subclass).
        assert_eq!(builtins::issubclass(weak_set, constant("Set")), value::TRUE);

        // Registration bumps `get_cache_token()` once; repeating it is
        // idempotent and leaves the token alone.
        let token = abc::call("get_cache_token", &[]);
        assert_eq!(token, crate::ops::int_result(1));
        assert_eq!(register(mutable_set, weak_set), weak_set);
        assert_eq!(abc::call("get_cache_token", &[]), token);
        assert!(!exc::pending());
    }

    #[test]
    fn mapping_registration_covers_the_dict_and_copy_scenarios() {
        let _guard = guard();
        reset();
        let mapping = constant("Mapping");
        let empty = value::dict_new(Vec::new());
        assert!(!exc::pending());
        // The runtime never infers structure, so `{}` needs an explicit edge.
        assert_eq!(builtins::isinstance(empty, mapping), value::FALSE);

        let dict_builtin = value::builtin_function_new(hydro_ir::Builtin::Dict);
        assert_eq!(register(mapping, dict_builtin), dict_builtin);
        assert!(!exc::pending());
        assert_eq!(builtins::isinstance(empty, mapping), value::TRUE);
        // An already-registered virtual subclass keeps matching: `copy.copy`
        // re-checks the same values after dispatch.
        assert_eq!(builtins::isinstance(empty, mapping), value::TRUE);

        // `MutableMapping` is a real subclass of `Mapping`, so it keeps its
        // own registry: registering there answers for `Mapping` too but not
        // the other way round.
        let mutable_mapping = constant("MutableMapping");
        let custom = class_new("CustomDict".to_string(), NONE, Vec::new());
        assert_eq!(register(mutable_mapping, custom), custom);
        assert_eq!(builtins::issubclass(custom, mapping), value::TRUE);
        assert_eq!(builtins::issubclass(custom, mutable_mapping), value::TRUE);
        assert_eq!(
            builtins::isinstance(value::instance_new(custom), mapping),
            value::TRUE
        );
        // `dict` was registered with `Mapping` only.
        assert_eq!(builtins::isinstance(empty, mutable_mapping), value::FALSE);
        assert!(!exc::pending());
    }

    #[test]
    fn sequence_registration_of_native_types_matches_instances() {
        let _guard = guard();
        reset();
        let sequence = constant("Sequence");
        let tuple_builtin = value::builtin_function_new(hydro_ir::Builtin::Tuple);
        assert_eq!(register(sequence, tuple_builtin), tuple_builtin);
        assert!(!exc::pending());
        let empty = value::tuple_new(Vec::new());
        assert_eq!(builtins::isinstance(empty, sequence), value::TRUE);
        // Only the registered native spelling matches; `str` is not
        // registered here even though CPython's `_collections_abc` does.
        assert_eq!(
            builtins::isinstance(value::str_alloc("x".to_string()), sequence),
            value::FALSE
        );
        assert!(!exc::pending());
    }

    #[test]
    fn register_returns_its_argument_and_handles_self_and_duplicates() {
        let _guard = guard();
        reset();
        let mapping = constant("Mapping");
        // Self registration is a real-subclass case: no edge, same result.
        assert_eq!(register(mapping, mapping), mapping);
        assert!(!exc::pending());
        assert_eq!(abc::call("get_cache_token", &[]), crate::ops::int_result(0));

        // The registered object is returned unchanged (CPython's
        // `ABCMeta.register` contract), never `None` and never the ABC.
        let subclass = class_new("Bag".to_string(), NONE, Vec::new());
        assert_eq!(register(mapping, subclass), subclass);
        assert_eq!(abc::call("get_cache_token", &[]), crate::ops::int_result(1));
        assert_eq!(register(mapping, subclass), subclass);
        assert_eq!(abc::call("get_cache_token", &[]), crate::ops::int_result(1));
    }

    #[test]
    fn register_rejects_non_classes_with_type_error() {
        let _guard = guard();
        reset();
        let mapping = constant("Mapping");
        let payload = value::str_alloc("payload".to_string());
        register(mapping, payload);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        // Registering against a non-ABC class keeps `ABCMeta.register`'s
        // error, not a silent success.
        let plain = class_new("Plain".to_string(), NONE, Vec::new());
        let subclass = class_new("Target".to_string(), NONE, Vec::new());
        abc::call("ABCMeta.register", &[plain, subclass]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        assert_eq!(abc::call("get_cache_token", &[]), crate::ops::int_result(0));
    }

    #[test]
    fn buffer_registration_and_structural_hook_still_work() {
        let _guard = guard();
        reset();
        let buffer = constant("Buffer");
        // Structural match without a registration (`__subclasshook__`).
        let bytes_builtin = value::builtin_function_new(hydro_ir::Builtin::Bytes);
        assert_eq!(builtins::issubclass(bytes_builtin, buffer), value::TRUE);
        assert_eq!(
            builtins::isinstance(value::bytes_new(Vec::new()), buffer),
            value::TRUE
        );
        // A plain class only matches once registered.
        let plain = class_new("Plain".to_string(), NONE, Vec::new());
        assert_eq!(builtins::issubclass(plain, buffer), value::FALSE);
        assert_eq!(register(buffer, plain), plain);
        assert_eq!(builtins::issubclass(plain, buffer), value::TRUE);
        assert!(!exc::pending());
    }

    #[test]
    fn unknown_constants_raise_attribute_error() {
        let _guard = guard();
        reset();
        constant("Missing");
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
    }
}
