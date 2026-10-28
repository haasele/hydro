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
//! * the class objects themselves (`Mapping.__name__`, identity, importing).
//!
//! What is deliberately not emulated: virtual subclass registration and
//! structural `__subclasshook__` checks. The runtime `isinstance`/`issubclass`
//! paths only consult the class hierarchy, so registering a virtual subclass
//! would be a lie. Every ABC therefore carries a `register` method (as in
//! CPython) that raises a tagged `NotImplementedError` instead of pretending
//! the registration took effect, and `isinstance({}, Mapping)` is `False` —
//! exactly the behaviour of explicit inheritance-only semantics.
//!
//! `Buffer` is the exception (PEP 688): it is created as a real `abc.ABCMeta`
//! class (so `register` works, `__abstractmethods__` reports `__buffer__`, and
//! `__subclasshook__` is present), and the native `isinstance`/`issubclass`
//! paths reproduce CPython's structural hook: native exporters (`bytes`,
//! `bytearray`, `memoryview`) and classes declaring `__buffer__` match without
//! a registered edge.
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
    // PEP 688. Unlike the other names, `Buffer` is a real `ABCMeta` class:
    // see [`buffer_class`].
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
    // `register` exists on every ABC but reports that virtual subclass
    // registration is unavailable (`_abc_register` is the native body).
    let register = value::native_function_new("_collections_abc", "_abc_register", 1);
    // The function value lives only in Rust memory until the class owns it, so
    // root it across the class allocation.
    let roots = heap::root_values(&[register]);
    let methods = vec![("register".to_string(), register, MethodKind::Staticmethod)];
    let class = value::class_new_full(name.to_string(), base, methods, Vec::new());
    heap::release_roots(roots);
    // Publish the class before any further allocation: the registry roots it
    // for the rest of the heap generation.
    globals::registry_set(&key, class);
    class
}

/// Returns (creating on first use) the `collections.abc.Buffer` class.
///
/// Unlike the plain classes in [`CLASSES`], `Buffer` is a real `ABCMeta` class
/// because its contract is enforceable: it declares the abstract
/// `__buffer__(flags)` hook, participates in virtual-subclass registration
/// through the metaclass, and its `__subclasshook__` answers structurally.
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
        "_abc_register" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "ABC.register() is not implemented: Hydro's isinstance/issubclass only consult \
             the class hierarchy, so a virtual subclass registration would have no effect",
        ),
        "_abc_buffer" => buffer_method(args),
        "_abc_buffer_subclasshook" => buffer_subclasshook(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'collections.abc' has no attribute '{function}'"),
        ),
    }
}
