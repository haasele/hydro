//! `abc`: abstract base classes (PEP 3119) on top of Hydro's class model.
//!
//! [`ABCMeta`] is a real class whose base is the cached native `type` class, so
//! `class Number(metaclass=ABCMeta)`, `class Meta(ABCMeta)` and
//! `class Point(ABC)` all create classes the ordinary class machinery can use
//! (`__mro__`, `super()`, `isinstance`/`issubclass` through the base chain).
//!
//! The metaclass implements the two hooks the runtime can honour truthfully:
//!
//! * `register(subclass)` records a virtual-subclass edge. The edge is
//!   consulted by the builtin `isinstance`/`issubclass` paths, so a
//!   registration really changes later checks. Registering a real subclass is
//!   a no-op that returns it; a duplicate registration is idempotent; neither
//!   class values nor the runtime's native type spellings (`int`, `float`, …)
//!   are rejected. Nothing is recorded when the target is not a class, so
//!   `register` never reports a success that has no effect.
//! * `__instancecheck__`/`__subclasscheck__` delegate to the same checks, and
//!   `__abstractmethods__` reports the abstract names computed from the class
//!   namespace and the MRO.
//!
//! What is deliberately not emulated, because the runtime cannot enforce it:
//!
//! * metaclass `__instancecheck__` overrides written in Python are not called
//!   by the `isinstance` builtin (only real and registered edges are
//!   consulted);
//! * instantiating a class with abstract methods is not blocked (the frontend
//!   does not enforce abstractness), but `__abstractmethods__` and
//!   `abstractmethod` are real: they record and report the declared abstract
//!   names instead of pretending enforcement happened.
//!
//! The registry lives in the process-wide globals side store under
//! `\0hydro.registry`, which roots every registered class for the rest of the
//! heap generation and is cleared by `hydro_runtime_init`. A registration is
//! therefore visible to every thread, matching the process-wide scope of the
//! runtime's module globals; class objects themselves are shared by the heap.

use std::sync::atomic::{AtomicUsize, Ordering};

use hydro_ir::{Builtin, MethodKind};

use crate::builtins;
use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, NOT_IMPLEMENTED_ERROR, TYPE_ERROR};
use crate::heap;
use crate::ops;
use crate::value::{self, DescriptorKind, FunctionBody, NONE};

/// Cached `ABCMeta` class.
const METACLASS_KEY: &str = "abc.ABCMeta";
/// Cached `ABC` helper class.
const ABC_KEY: &str = "abc.ABC";
/// Flat `[abc, subclass, abc, subclass, …]` list of virtual-subclass edges.
const VIRTUAL_KEY: &str = "abc.virtual";
/// Function values marked by `abstractmethod`.
const ABSTRACT_KEY: &str = "abc.abstract";
/// `get_cache_token()` counter, bumped whenever a registration is recorded.
const TOKEN_KEY: &str = "abc.token";

/// Number of recorded virtual edges across the whole process.
///
/// `isinstance`/`issubclass` consult it before touching the registry, so the
/// common case (no registrations) costs one relaxed load and no hash lookup.
static VIRTUAL_EDGES: AtomicUsize = AtomicUsize::new(0);

/// Materialises the module constant `name`.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "ABCMeta" => metaclass_class(),
        "ABC" => abc_class(),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'abc' has no attribute '{name}'"),
        ),
    }
}

/// Dispatches a native `abc` function. Arity is enforced by the central spec.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "abstractmethod" => abstractmethod(args),
        "get_cache_token" => ops::int_result(cache_token()),
        // Recomputing is unnecessary: `__abstractmethods__` is derived from the
        // live class namespace on every read, so the class is already current.
        "update_abstractmethods" => args.first().copied().unwrap_or(NONE),
        "abstractclassmethod" | "abstractstaticmethod" | "abstractproperty" => {
            exc::raise_with_message(
                NOT_IMPLEMENTED_ERROR,
                &format!(
                    "abc.{function}() is not supported: use @classmethod, @staticmethod or \
                     @property together with @abstractmethod"
                ),
            )
        }
        "ABCMeta.register" => register(args),
        "ABCMeta.__instancecheck__" => instance_check(args),
        "ABCMeta.__subclasscheck__" => subclass_check(args),
        "ABCMeta.__abstractmethods__" => abstract_methods_getter(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'abc' has no attribute '{function}'"),
        ),
    }
}

// -------------------------------------------------------------------------
// Virtual-subclass registry (read by the builtin isinstance/issubclass)
// -------------------------------------------------------------------------

/// Whether `subclass` is a registered virtual subclass of `ancestor`.
///
/// Mirrors `ABCMeta.__subclasscheck__` for the parts the runtime models: a
/// class is a virtual subclass when it (or a real ancestor) was registered
/// with `ancestor` or with a real subclass of `ancestor` (CPython checks the
/// registries of every subclass as well). Both arguments must be classes;
/// native type spellings are handled by [`is_virtual_instance`].
pub(crate) fn is_virtual_subclass(subclass: i64, ancestor: i64) -> bool {
    if VIRTUAL_EDGES.load(Ordering::Relaxed) == 0 {
        return false;
    }
    if value::as_class(subclass).is_none() || value::as_class(ancestor).is_none() {
        return false;
    }
    let Some(edges) = virtual_edges() else {
        return false;
    };
    edges.as_chunks::<2>().0.iter().any(|&[abc, target]| {
        registered_under(abc, ancestor) && value::is_subclass_of(subclass, target)
    })
}

/// Whether `value` is an instance of `ancestor` through a registered edge.
///
/// Class values are never instances here (their `isinstance` relation is the
/// metaclass one, handled by the ordinary builtin path). Every other value is
/// tested by its real class and, for native scalars registered through their
/// builtin spelling (`Number.register(int)`), by its static type name.
pub(crate) fn is_virtual_instance(value: i64, ancestor: i64) -> bool {
    if VIRTUAL_EDGES.load(Ordering::Relaxed) == 0 {
        return false;
    }
    if value::as_class(ancestor).is_none() || value::as_class(value).is_some() {
        return false;
    }
    let class = value::type_class(value);
    if value::as_class(class).is_some() && is_virtual_subclass(class, ancestor) {
        return true;
    }
    let Some(edges) = virtual_edges() else {
        return false;
    };
    let actual = value::type_name(value);
    edges.as_chunks::<2>().0.iter().any(|&[abc, target]| {
        if !registered_under(abc, ancestor) {
            return false;
        }
        match native_type_target(target) {
            Some(name) => actual == name || (actual == "bool" && name == "int"),
            None => value::is_subclass_of(value, target),
        }
    })
}

/// Whether an edge recorded for `abc` counts for `ancestor`: the edge's ABC is
/// `ancestor` or one of its real subclasses.
fn registered_under(abc: i64, ancestor: i64) -> bool {
    abc == ancestor || value::is_subclass_of(abc, ancestor)
}

/// The live edge list, if any registration happened.
fn virtual_edges() -> Option<&'static [i64]> {
    value::as_list(globals::registry_get(VIRTUAL_KEY)?)
}

fn abstract_markers() -> Option<&'static [i64]> {
    value::as_list(globals::registry_get(ABSTRACT_KEY)?)
}

fn bump_token() {
    let next = cache_token().wrapping_add(1);
    globals::registry_set(TOKEN_KEY, ops::int_result(next));
}

fn cache_token() -> i64 {
    globals::registry_get(TOKEN_KEY)
        .and_then(value::as_integer)
        .unwrap_or(0)
}

/// `ABCMeta.register(cls, subclass)`: records a virtual subclass.
fn register(args: &[i64]) -> i64 {
    let (Some(&abc), Some(&subclass)) = (args.first(), args.get(1)) else {
        return exc::raise_with_message(TYPE_ERROR, "register() takes 2 arguments");
    };
    if value::as_class(abc).is_none() || !is_abc_class(abc) {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "Cannot register a virtual subclass of '{}': it is not an ABC",
                value::type_name_of(abc)
            ),
        );
    }
    let native_target = native_type_target(subclass);
    if native_target.is_none() && value::as_class(subclass).is_none() {
        return exc::raise_with_message(TYPE_ERROR, "Can only register classes");
    }
    // Registering an existing real subclass changes nothing.
    if value::as_class(subclass).is_some() && value::is_subclass_of(subclass, abc) {
        return subclass;
    }
    if !has_virtual_edge(abc, subclass) {
        add_virtual_edge(abc, subclass);
    }
    subclass
}

/// Whether `class` was created by `ABCMeta` or a subclass of it.
///
/// The ancestry is checked by name (like the runtime's `Protocol` detection)
/// so a class created against another thread's cached `ABCMeta` object is
/// still recognised: the class cache is thread-local, the class objects are
/// not.
fn is_abc_class(class: i64) -> bool {
    let metaclass = value::class_metaclass(class);
    if metaclass == NONE {
        return false;
    }
    value::class_mro(metaclass)
        .iter()
        .any(|&ancestor| value::class_display_name(ancestor) == Some("ABCMeta"))
}

fn has_virtual_edge(abc: i64, subclass: i64) -> bool {
    virtual_edges().is_some_and(|edges| {
        edges
            .as_chunks::<2>()
            .0
            .iter()
            .any(|&[owner, target]| owner == abc && target == subclass)
    })
}

fn add_virtual_edge(abc: i64, subclass: i64) {
    let list = match globals::registry_get(VIRTUAL_KEY) {
        Some(list) => list,
        None => {
            let list = value::list_new(Vec::new());
            // Rooted by the registry (it installs a thread root) before the
            // first append.
            globals::registry_set(VIRTUAL_KEY, list);
            list
        }
    };
    let roots = heap::root_values(&[abc, subclass]);
    if let Some(mut items) = value::list_items_mut(list) {
        items.push(abc);
        items.push(subclass);
    }
    heap::release_roots(roots);
    VIRTUAL_EDGES.fetch_add(1, Ordering::Relaxed);
    bump_token();
}

/// The builtin spelling of a native type, when `v` is one.
///
/// Hydro represents `int`, `float`, … as builtin function values rather than
/// class objects; registering them records the static type name so
/// `isinstance(1, Number)` works like it does against CPython's `int` class.
fn native_type_target(v: i64) -> Option<&'static str> {
    let function = value::as_function(v)?;
    let FunctionBody::Builtin(builtin) = &function.body else {
        return None;
    };
    match builtin.name() {
        "int" | "float" | "complex" | "bool" | "str" | "bytes" | "bytearray" | "list" | "dict"
        | "tuple" | "set" | "frozenset" | "memoryview" => Some(builtin.name()),
        _ => None,
    }
}

// -------------------------------------------------------------------------
// Abstract-method marking
// -------------------------------------------------------------------------

/// `abstractmethod(func)`: records the declaration and returns the function
/// unchanged, like CPython's decorator.
fn abstractmethod(args: &[i64]) -> i64 {
    let Some(&function) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "abstractmethod() takes 1 argument");
    };
    if !is_abstract(function) {
        add_abstract(function);
    }
    function
}

/// Marks a runtime-native function as abstract.
///
/// Python code spells this as `@abstractmethod`; native methods installed on
/// an `ABCMeta` class (such as `collections.abc.Buffer.__buffer__`) are marked
/// through this entry point so `__abstractmethods__` reports them.
pub(crate) fn mark_abstract(function: i64) {
    if !is_abstract(function) {
        add_abstract(function);
    }
}

fn is_abstract(v: i64) -> bool {
    abstract_markers().is_some_and(|markers| markers.contains(&v))
}

fn add_abstract(function: i64) {
    let list = match globals::registry_get(ABSTRACT_KEY) {
        Some(list) => list,
        None => {
            let list = value::list_new(Vec::new());
            globals::registry_set(ABSTRACT_KEY, list);
            list
        }
    };
    let roots = heap::root_values(&[function]);
    if let Some(mut items) = value::list_items_mut(list) {
        items.push(function);
    }
    heap::release_roots(roots);
}

/// `ABCMeta.__abstractmethods__`: the effective abstract names of a class.
///
/// The class namespace is walked from the most derived class outwards and the
/// first definition of a name decides, so overriding an abstract method with a
/// concrete one removes it and re-declaring it abstract keeps it. Descriptors
/// report abstractness of their wrapped callable, matching CPython's
/// `__isabstractmethod__` propagation.
fn abstract_methods_getter(args: &[i64]) -> i64 {
    let Some(&class) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "__abstractmethods__ requires a class");
    };
    if value::as_class(class).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "__abstractmethods__ requires a class, not '{}'",
                value::type_name_of(class)
            ),
        );
    }
    let mut decided: Vec<(String, bool)> = Vec::new();
    for ancestor in value::class_mro(class) {
        for (name, item) in value::class_namespace(ancestor) {
            if decided.iter().any(|(existing, _)| existing == &name) {
                continue;
            }
            decided.push((name, value_is_abstract(item)));
        }
    }
    let names: Vec<i64> = decided
        .into_iter()
        .filter(|(_, is_abstract)| *is_abstract)
        .map(|(name, _)| value::str_alloc(name))
        .collect();
    let roots = heap::root_values(&names);
    let set = value::set_new(names);
    heap::release_roots(roots);
    set
}

fn value_is_abstract(item: i64) -> bool {
    if is_abstract(item) {
        return true;
    }
    let Some(descriptor) = value::as_descriptor(item) else {
        return false;
    };
    match descriptor.kind {
        DescriptorKind::Property => {
            is_abstract(descriptor.fget())
                || (descriptor.fset() != NONE && is_abstract(descriptor.fset()))
                || (descriptor.fdel() != NONE && is_abstract(descriptor.fdel()))
        }
        DescriptorKind::Classmethod | DescriptorKind::Staticmethod => {
            is_abstract(descriptor.target)
        }
        // The `type.__annotations__` getset descriptor wraps no function.
        DescriptorKind::TypeAnnotations => false,
    }
}

// -------------------------------------------------------------------------
// Metaclass hooks
// -------------------------------------------------------------------------

/// `ABCMeta.__instancecheck__(cls, instance)`: the same answer the builtin
/// gives, including real inheritance and virtual registrations.
fn instance_check(args: &[i64]) -> i64 {
    let (Some(&class), Some(&instance)) = (args.first(), args.get(1)) else {
        return exc::raise_with_message(TYPE_ERROR, "__instancecheck__() takes 2 arguments");
    };
    builtins::isinstance(instance, class)
}

/// `ABCMeta.__subclasscheck__(cls, subclass)`.
fn subclass_check(args: &[i64]) -> i64 {
    let (Some(&class), Some(&subclass)) = (args.first(), args.get(1)) else {
        return exc::raise_with_message(TYPE_ERROR, "__subclasscheck__() takes 2 arguments");
    };
    builtins::issubclass(subclass, class)
}

// -------------------------------------------------------------------------
// Class objects
// -------------------------------------------------------------------------

/// Returns (creating on first use) the `ABCMeta` class object.
fn metaclass_class() -> i64 {
    if let Some(cached) = globals::registry_get(METACLASS_KEY) {
        return cached;
    }
    let register = value::native_function_new("abc", "ABCMeta.register", 2);
    let instance_check = value::native_function_new("abc", "ABCMeta.__instancecheck__", 2);
    let subclass_check = value::native_function_new("abc", "ABCMeta.__subclasscheck__", 2);
    let abstract_methods = value::native_function_new("abc", "ABCMeta.__abstractmethods__", 1);
    // As a base the bare `type` builtin denotes the cached native `type` class.
    let type_builtin = value::builtin_function_new(Builtin::Type);
    let roots = heap::root_values(&[
        register,
        instance_check,
        subclass_check,
        abstract_methods,
        type_builtin,
    ]);
    let methods = vec![
        ("register".to_string(), register, MethodKind::Normal),
        (
            "__instancecheck__".to_string(),
            instance_check,
            MethodKind::Normal,
        ),
        (
            "__subclasscheck__".to_string(),
            subclass_check,
            MethodKind::Normal,
        ),
        (
            "__abstractmethods__".to_string(),
            abstract_methods,
            MethodKind::Property,
        ),
    ];
    let class = value::class_new_multi(
        "ABCMeta".to_string(),
        vec![type_builtin],
        NONE,
        methods,
        Vec::new(),
    );
    heap::release_roots(roots);
    if !exc::pending() {
        globals::registry_set(METACLASS_KEY, class);
    }
    class
}

/// Returns (creating on first use) the `ABC` helper class.
fn abc_class() -> i64 {
    if let Some(cached) = globals::registry_get(ABC_KEY) {
        return cached;
    }
    let metaclass = metaclass_class();
    if exc::pending() {
        return NONE;
    }
    // `__slots__ = ()`, as in CPython's helper.
    let slots = value::tuple_new(Vec::new());
    let roots = heap::root_values(&[metaclass, slots]);
    let attrs = vec![("__slots__".to_string(), slots)];
    let class = value::class_new_multi("ABC".to_string(), Vec::new(), metaclass, Vec::new(), attrs);
    heap::release_roots(roots);
    if !exc::pending() {
        globals::registry_set(ABC_KEY, class);
    }
    class
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{as_class, class_new, class_new_multi, str_alloc};

    /// The edge counter and the module-global store are process-wide;
    /// serialise the tests that reset or read them.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        crate::heap::heap_reset_test_guard()
    }

    fn reset() {
        exc::init();
        globals::reset();
        VIRTUAL_EDGES.store(0, Ordering::Relaxed);
    }

    fn abc_metaclass() -> i64 {
        let metaclass = constant("ABCMeta");
        assert!(!exc::pending());
        metaclass
    }

    fn number_abc() -> i64 {
        let metaclass = abc_metaclass();
        class_new_multi(
            "Number".to_string(),
            Vec::new(),
            metaclass,
            Vec::new(),
            Vec::new(),
        )
    }

    #[test]
    fn abc_meta_is_a_class_derived_from_type() {
        let _guard = guard();
        reset();
        let metaclass = abc_metaclass();
        let class = as_class(metaclass).expect("class value");
        assert_eq!(class.name, "ABCMeta");
        assert_eq!(class.methods.len(), 4);
        assert!(value::is_subclass_of(
            metaclass,
            value::type_class(metaclass)
        ));
        // `ABC` is a class whose metaclass is `ABCMeta`.
        let abc = as_class(constant("ABC")).expect("class value");
        assert_eq!(abc.name, "ABC");
        assert_eq!(abc.metaclass, metaclass);
        assert!(!exc::pending());
    }

    #[test]
    fn register_changes_subclass_and_instance_checks() {
        let _guard = guard();
        reset();
        let number = number_abc();
        let subclass = class_new("Real".to_string(), NONE, Vec::new());
        assert!(!exc::pending());
        assert!(!is_virtual_subclass(subclass, number));

        let registered = call("ABCMeta.register", &[number, subclass]);
        assert_eq!(registered, subclass);
        assert!(!exc::pending());
        assert!(is_virtual_subclass(subclass, number));

        // Instances of the registered class (and of its subclasses) match.
        let instance = value::instance_new(subclass);
        assert!(is_virtual_instance(instance, number));
        let derived = class_new("Derived".to_string(), subclass, Vec::new());
        let derived_instance = value::instance_new(derived);
        assert!(is_virtual_instance(derived_instance, number));

        // The token moves so `functools.singledispatch` dispatchers refresh.
        let token = call("get_cache_token", &[]);
        assert_eq!(token, ops::int_result(1));
        // Registering again is idempotent and does not bump the token.
        assert_eq!(call("ABCMeta.register", &[number, subclass]), subclass);
        assert_eq!(call("get_cache_token", &[]), ops::int_result(1));
    }

    #[test]
    fn register_of_a_native_type_spelling_matches_its_instances() {
        let _guard = guard();
        reset();
        let number = number_abc();
        let int_builtin = value::builtin_function_new(Builtin::Int);
        assert_eq!(
            call("ABCMeta.register", &[number, int_builtin]),
            int_builtin
        );
        // `1` and `True` (bool is a subclass of int) match, `"x"` does not.
        assert!(is_virtual_instance(ops::int_result(1), number));
        assert!(is_virtual_instance(value::TRUE, number));
        assert!(!is_virtual_instance(str_alloc("x".to_string()), number));
    }

    #[test]
    fn register_returns_the_argument_through_class_dispatch() {
        let _guard = guard();
        reset();
        let metaclass = abc_metaclass();
        let number = class_new_multi(
            "Number".to_string(),
            Vec::new(),
            metaclass,
            Vec::new(),
            Vec::new(),
        );
        // `Number.register(int)` goes through the metaclass fallback; the
        // native `register` must hand back the argument object unchanged, not
        // the class it was called on and not `None`.
        let int_builtin = value::builtin_function_new(Builtin::Int);
        let name = b"register";
        let argv = [int_builtin];
        let result = crate::methods::call(number, name.as_ptr(), name.len(), 1, argv.as_ptr());
        assert_eq!(result, int_builtin);
        assert!(!exc::pending());
        assert!(is_virtual_instance(crate::ops::int_result(1), number));
        assert!(is_virtual_instance(value::TRUE, number));
    }

    #[test]
    fn register_rejects_non_classes_and_non_abcs() {
        let _guard = guard();
        reset();
        let number = number_abc();
        let payload = str_alloc("payload".to_string());
        call("ABCMeta.register", &[number, payload]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        let plain = class_new("Plain".to_string(), NONE, Vec::new());
        call("ABCMeta.register", &[plain, payload]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn abstractmethod_records_and_abstract_attributes_are_reported() {
        let _guard = guard();
        reset();
        let metaclass = abc_metaclass();
        let function = value::native_function_new("abc", "abstractmethod", 1);
        assert_eq!(call("abstractmethod", &[function]), function);

        // A class declaring the abstract function reports its name; a subclass
        // that overrides it with a concrete function clears it again.
        let base = class_new_multi(
            "Base".to_string(),
            Vec::new(),
            metaclass,
            vec![("render".to_string(), function, MethodKind::Normal)],
            Vec::new(),
        );
        let concrete = value::native_function_new("abc", "abstractmethod", 1);
        let derived = class_new_multi(
            "Derived".to_string(),
            vec![base],
            NONE,
            vec![("render".to_string(), concrete, MethodKind::Normal)],
            Vec::new(),
        );
        let methods = |class| {
            value::as_set(abstract_methods_getter(&[class]))
                .expect("set value")
                .len()
        };
        assert_eq!(methods(base), 1);
        assert_eq!(methods(derived), 0);

        // `update_abstractmethods` returns the class unchanged.
        assert_eq!(call("update_abstractmethods", &[base]), base);
    }

    #[test]
    fn unknown_names_raise_attribute_error() {
        let _guard = guard();
        reset();
        call("missing", &[]);
        assert!(exc::pending());
        exc::clear();
        constant("missing");
        assert!(exc::pending());
        exc::clear();
    }
}
