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
//! The class cache lives in the globals side store under
//! `\0hydro.registry`, which roots every cached class and is cleared by
//! `hydro_runtime_init`, so no class value can survive its heap.

use hydro_ir::MethodKind;

use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, NOT_IMPLEMENTED_ERROR};
use crate::heap;
use crate::value::{self, NONE};

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
];

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
pub(crate) fn call(function: &str, _args: &[i64]) -> i64 {
    match function {
        "_abc_register" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "ABC.register() is not implemented: Hydro's isinstance/issubclass only consult \
             the class hierarchy, so a virtual subclass registration would have no effect",
        ),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'collections.abc' has no attribute '{function}'"),
        ),
    }
}
