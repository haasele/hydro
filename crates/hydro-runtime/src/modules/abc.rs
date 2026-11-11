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
//!   a no-op that returns it; a duplicate registration is idempotent; class
//!   values, the runtime's native type spellings (`int`, `float`, …) and
//!   extension class handles (NumPy's scalar types, which reach Hydro as
//!   opaque C handles) are all accepted. Nothing is recorded when the target
//!   is not a class, so `register` never reports a success that has no
//!   effect. Every native `collections.abc` class is an `ABCMeta` instance, so
//!   `Mapping.register` resolves here through the ordinary metaclass lookup and
//!   records into the same registry.
//! * `__instancecheck__`/`__subclasscheck__` delegate to the same checks, and
//!   `__abstractmethods__` reports the abstract names computed from the class
//!   namespace and the MRO.
//! * constructing an instance whose effective abstract methods are non-empty
//!   raises CPython's `TypeError` before allocation. The shared class-call hook
//!   in `core::methods` runs [`check_instantiable`] for both the positional and
//!   the keyword constructor paths, so overriding every abstract name makes the
//!   class constructible again and a partial override stays rejected.
//!
//! What is deliberately not emulated, because the runtime cannot enforce it:
//!
//! * metaclass `__instancecheck__` overrides written in Python are not called
//!   by the `isinstance` builtin (only real and registered edges are
//!   consulted).
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
use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR};
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
        "abstractclassmethod" => abstract_descriptor(DescriptorKind::Classmethod, args),
        "abstractstaticmethod" => abstract_descriptor(DescriptorKind::Staticmethod, args),
        "abstractproperty" => abstract_property(args),
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
/// registries of every subclass as well). After the registry misses, an ABC's
/// `__subclasshook__` decides structurally, exactly like CPython's
/// `_abc_subclasscheck`. Both arguments must be classes — Hydro class values
/// or extension class handles; native type spellings are handled by
/// [`is_virtual_instance`].
pub(crate) fn is_virtual_subclass(subclass: i64, ancestor: i64) -> bool {
    let subclass_is_class =
        value::as_class(subclass).is_some() || extension_class_target(subclass).is_some();
    if !subclass_is_class || value::as_class(ancestor).is_none() {
        return false;
    }
    if VIRTUAL_EDGES.load(Ordering::Relaxed) != 0 {
        if let Some(edges) = virtual_edges() {
            if edges.as_chunks::<2>().0.iter().any(|&[abc, target]| {
                registered_under(abc, ancestor) && edge_matches_subclass(subclass, target)
            }) {
                return true;
            }
        }
    }
    structural_subclass(subclass, ancestor)
}

/// `ABCMeta.__subclasscheck__`'s final step: an ABC's classmethod
/// `__subclasshook__(subclass)` decides when the registry has no edge.
///
/// CPython invokes the hook only for classes created through `ABCMeta`.
/// `True` means structurally compatible and `NotImplemented`/falsey means no
/// answer; a hook that raises leaves its exception pending for the caller.
/// A guard prevents a hook that re-enters `issubclass` for the same ancestors
/// from recursing without bound.
fn structural_subclass(subclass: i64, ancestor: i64) -> bool {
    if !is_abc_class(ancestor) || exc::pending() {
        return false;
    }
    static DEPTH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    if DEPTH.load(Ordering::Relaxed) != 0 {
        return false;
    }
    let Some(entry) = value::class_lookup(ancestor, "__subclasshook__") else {
        return false;
    };
    let (kind, function) = match entry {
        value::ClassEntry::Method(kind, function) => (kind, function),
        // A function stored as a class attribute is a descriptor too; it
        // binds the class exactly like a normal method.
        value::ClassEntry::Attr(item) if value::as_function(item).is_some() => {
            (MethodKind::Normal, item)
        }
        value::ClassEntry::Attr(_) => return false,
    };
    // CPython reads the hook through the descriptor protocol: a classmethod
    // or plain function binds the ABC as its first argument, a staticmethod
    // does not.
    let args: [i64; 2] = [ancestor, subclass];
    let argc = if kind == MethodKind::Staticmethod {
        1
    } else {
        2
    };
    let argv = if argc == 1 { &args[1] } else { &args[0] };
    DEPTH.store(1, Ordering::Relaxed);
    let result = builtins::call(function, argc, argv as *const i64);
    DEPTH.store(0, Ordering::Relaxed);
    if exc::pending() || result == value::NOT_IMPLEMENTED {
        return false;
    }
    value::truthy(result)
}

/// Whether `value` is an instance of `ancestor` through a registered edge.
///
/// Class values are never instances here (their `isinstance` relation is the
/// metaclass one, handled by the ordinary builtin path). Every other value is
/// tested by its real class and, for native scalars registered through their
/// builtin spelling (`Number.register(int)`), by its static type name.
/// Extension handles are tested with `PyObject_IsInstance`, so a registered C
/// base class covers the extension's own subclasses.
pub(crate) fn is_virtual_instance(value: i64, ancestor: i64) -> bool {
    if value::as_class(ancestor).is_none() || value::as_class(value).is_some() {
        return false;
    }
    let class = value::type_class(value);
    if value::as_class(class).is_some() && is_virtual_subclass(class, ancestor) {
        return true;
    }
    if VIRTUAL_EDGES.load(Ordering::Relaxed) == 0 {
        return false;
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
            None => edge_matches_instance(value, target),
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
    if native_target.is_none()
        && value::as_class(subclass).is_none()
        && extension_class_target(subclass).is_none()
    {
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

/// Native entry point for the `register` method inherited by every ABC.
///
/// `collections.abc` classes are `ABCMeta` instances, so `Mapping.register(X)`
/// resolves to `ABCMeta.register` and reaches [`register`] with
/// `[Mapping, X]`; the private `_collections_abc._abc_register` spelling is
/// answered by the same entry point. Both share the single edge registry above
/// instead of keeping a second one.
pub(crate) fn register_from_method(args: &[i64]) -> i64 {
    register(args)
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
            .any(|&[owner, target]| owner == abc && same_target(target, subclass))
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
// Extension class targets
// -------------------------------------------------------------------------

/// The extension type object behind `v`, when `v` is an extension class.
///
/// C extensions expose their classes to Hydro as opaque handles: NumPy's
/// scalar types (`numpy.integer`, `numpy.float64`, …) are `PyTypeObject`s that
/// arrive wrapped in [`value::ExtObjectData`]. Only a real type object is a
/// class here: the predicate is the host's `PyType_Check`, so an extension
/// *instance* (whose type is not a metaclass) is still rejected. The check is
/// generic; no module or type name is special-cased.
fn extension_class_target(v: i64) -> Option<*mut crate::capi::ShimObject> {
    let state = value::as_ext_object(v)?;
    crate::capi::object_is_type(state.object).then_some(state.object)
}

/// Whether two extension type pointers name the same type or a real subclass.
fn extension_subclass_of_pointer(
    derived: *mut crate::capi::ShimObject,
    target: *mut crate::capi::ShimObject,
) -> bool {
    derived == target || crate::capi::object_is_subclass(derived, target).unwrap_or(false)
}

/// Whether `subclass` is extension type `target` or derives from it.
///
/// An extension class answers through the extension's own type hierarchy
/// (`PyObject_IsSubclass`). A Hydro class that names an extension type among
/// its bases is handled by walking its linearization, mirroring the builtin
/// `issubclass` path in `builtins.rs`.
fn extension_subclass_of(subclass: i64, target: *mut crate::capi::ShimObject) -> bool {
    if let Some(state) = value::as_ext_object(subclass) {
        return extension_subclass_of_pointer(state.object, target);
    }
    if value::as_class(subclass).is_none() {
        return false;
    }
    value::class_mro(subclass).iter().any(|&ancestor| {
        let Some(base) = value::as_ext_object(ancestor) else {
            return false;
        };
        extension_subclass_of_pointer(base.object, target)
    })
}

/// Whether the extension type `derived` is a registered virtual subclass of
/// the Hydro ABC `ancestor`.
///
/// The C API receives extension classes as raw pointers, so this spelling
/// mirrors [`is_virtual_subclass`] without materialising a Hydro handle.
pub(crate) fn is_virtual_extension_subclass(
    derived: *mut crate::capi::ShimObject,
    ancestor: i64,
) -> bool {
    if VIRTUAL_EDGES.load(Ordering::Relaxed) == 0 || value::as_class(ancestor).is_none() {
        return false;
    }
    if !crate::capi::object_is_type(derived) {
        return false;
    }
    let Some(edges) = virtual_edges() else {
        return false;
    };
    edges.as_chunks::<2>().0.iter().any(|&[abc, target]| {
        registered_under(abc, ancestor)
            && extension_class_target(target)
                .is_some_and(|extension| extension_subclass_of_pointer(derived, extension))
    })
}

/// Whether `value` is an instance of extension type `target`.
///
/// An extension handle answers through `PyObject_IsInstance`, so a registered
/// base type covers the extension's own subclasses (NumPy's `numpy.integer`
/// covers `numpy.int64`). A Hydro value is tested through its class's
/// linearization, which may contain extension bases.
fn extension_instance_of(value: i64, target: *mut crate::capi::ShimObject) -> bool {
    if let Some(state) = value::as_ext_object(value) {
        return crate::capi::object_isinstance(state.object, target).unwrap_or(false);
    }
    extension_subclass_of(value::type_class(value), target)
}

/// Whether the edge target `target` covers the class `subclass`.
fn edge_matches_subclass(subclass: i64, target: i64) -> bool {
    match extension_class_target(target) {
        Some(extension) => extension_subclass_of(subclass, extension),
        None => value::is_subclass_of(subclass, target),
    }
}

/// Whether the edge target `target` covers the non-class value `value`.
fn edge_matches_instance(value: i64, target: i64) -> bool {
    match extension_class_target(target) {
        Some(extension) => extension_instance_of(value, extension),
        None => value::is_subclass_of(value, target),
    }
}

/// Whether two edge targets name the same class.
///
/// Extension handles are distinct heap values even when they wrap the same
/// `PyTypeObject`, so a second registration of the same C class must compare
/// the underlying type pointer instead of the handle.
fn same_target(left: i64, right: i64) -> bool {
    if left == right {
        return true;
    }
    match (value::as_ext_object(left), value::as_ext_object(right)) {
        (Some(left), Some(right)) => left.object == right.object,
        _ => false,
    }
}

// -------------------------------------------------------------------------
// Abstract-method marking
// -------------------------------------------------------------------------

/// The deprecated `abstractclassmethod`/`abstractstaticmethod` decorators:
/// wrap the callable exactly like `classmethod`/`staticmethod` and mark the
/// wrapped function abstract, so `__abstractmethods__` reports it. The
/// descriptor value also carries `__isabstractmethod__` for the wrappers whose
/// attribute table the runtime exposes.
fn abstract_descriptor(kind: DescriptorKind, args: &[i64]) -> i64 {
    let Some(&function) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "decorator takes exactly one argument");
    };
    mark_abstract(function);
    value::descriptor_new(kind, function, NONE, NONE)
}

/// `abstractproperty(fget=None, fset=None, fdel=None, doc=None)`: CPython's
/// deprecated abstract-property decorator. The property's getter is marked
/// abstract (so the class-level computation sees it) and the descriptor itself
/// reports `__isabstractmethod__`, matching the class attribute on CPython's
/// `abstractproperty` type.
fn abstract_property(args: &[i64]) -> i64 {
    let fget = args.first().copied().unwrap_or(NONE);
    let fset = args.get(1).copied().unwrap_or(NONE);
    let fdel = args.get(2).copied().unwrap_or(NONE);
    if fget != NONE {
        mark_abstract(fget);
    }
    let roots = heap::root_values(&[fget, fset, fdel]);
    let descriptor = value::descriptor_new(DescriptorKind::Property, fget, fset, fdel);
    if let Some(state) = value::as_descriptor(descriptor) {
        state.set_attr(descriptor, "__isabstractmethod__".to_string(), value::TRUE);
    }
    heap::release_roots(roots);
    descriptor
}

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
    let names: Vec<i64> = effective_abstract_names(class)
        .into_iter()
        .map(value::str_alloc)
        .collect();
    let roots = heap::root_values(&names);
    let set = value::set_new(names);
    heap::release_roots(roots);
    set
}

/// The effective abstract names of `class`, with the most derived definition
/// deciding each name.
///
/// Shared by `__abstractmethods__` and [`check_instantiable`], so the reported
/// set and the instantiation rule never disagree.
fn effective_abstract_names(class: i64) -> Vec<String> {
    let mut decided: Vec<(String, bool)> = Vec::new();
    for ancestor in value::class_mro(class) {
        for (name, item) in value::class_namespace(ancestor) {
            if decided.iter().any(|(existing, _)| existing == &name) {
                continue;
            }
            decided.push((name, value_is_abstract(item)));
        }
    }
    decided
        .into_iter()
        .filter(|(_, is_abstract)| *is_abstract)
        .map(|(name, _)| name)
        .collect()
}

/// Rejects an instance whose class still has effective abstract methods.
///
/// This is CPython's `object.__new__` rule: a class carrying a non-empty
/// `__abstractmethods__` cannot be instantiated, and the diagnostic sorts the
/// remaining names and quotes each one. Returns `None` when construction may
/// proceed; otherwise raises `TypeError` and returns `Some(NONE)` for the
/// constructor hook to propagate.
///
/// Only classes created through `ABCMeta` (or a subclass of it) are checked,
/// mirroring the `Py_TPFLAGS_IS_ABSTRACT` flag `_abc_init` sets on those
/// classes. Since [`effective_abstract_names`] is computed from the live
/// namespaces, replacing an inherited abstract method with a concrete one on
/// the subclass lifts the rejection without any extra bookkeeping.
pub(crate) fn check_instantiable(class: i64) -> Option<i64> {
    // No function has ever been declared abstract, so no class can report an
    // effective abstract name. This keeps the check off the hot construction
    // path for programs that never use `abc`.
    if abstract_markers().is_none_or(|markers| markers.is_empty()) {
        return None;
    }
    if !is_abc_class(class) {
        return None;
    }
    let mut names = effective_abstract_names(class);
    if names.is_empty() {
        return None;
    }
    // CPython joins the sorted names with "', '" and wraps the whole list in
    // single quotes, so a single name reads `'name'` and several read
    // `'a', 'b'`.
    names.sort_unstable();
    let quoted: Vec<String> = names.into_iter().map(|name| format!("'{name}'")).collect();
    let plural = if quoted.len() > 1 { "s" } else { "" };
    Some(exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "Can't instantiate abstract class {} without an implementation for \
             abstract method{plural} {}",
            value::class_display_name(class).unwrap_or("type"),
            quoted.join(", "),
        ),
    ))
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

    /// Builds a synthetic heap extension type, optionally derived from `base`.
    ///
    /// The caller owns the returned reference. `BASETYPE` is set so the type
    /// can serve as a base for the derived probe.
    fn extension_type(
        name: &std::ffi::CStr,
        base: Option<*mut crate::capi::ShimObject>,
    ) -> *mut crate::capi::ShimObject {
        let mut slots = [crate::capi::PyTypeSlotMirror {
            slot: 0,
            pfunc: std::ptr::null_mut(),
        }];
        let mut spec = crate::capi::PyTypeSpecMirror {
            name: name.as_ptr(),
            basicsize: 32,
            itemsize: 0,
            flags: crate::capi::PY_TPFLAGS_BASETYPE as u32,
            slots: slots.as_mut_ptr(),
        };
        let bases = match base {
            Some(base) => {
                let tuple = crate::capi::PyTuple_New(1);
                assert!(!tuple.is_null());
                crate::capi::Py_IncRef(base);
                assert_eq!(crate::capi::PyTuple_SetItem(tuple, 0, base), 0);
                tuple
            }
            None => std::ptr::null_mut(),
        };
        let type_object = crate::capi::PyType_FromSpecWithBases(&mut spec, bases);
        if !bases.is_null() {
            crate::capi::Py_DecRef(bases);
        }
        assert!(
            !type_object.is_null(),
            "extension type creation failed: {:?}",
            crate::capi::take_error()
        );
        type_object
    }

    /// Wraps an extension reference as a Hydro value (which owns the reference).
    fn extension_value(object: *mut crate::capi::ShimObject, type_name: &str) -> i64 {
        value::ext_object_new(object, type_name.to_string())
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
    fn register_accepts_extension_classes_and_matches_their_instances() {
        let _guard = guard();
        reset();
        let number = number_abc();

        // A synthetic heap extension type stands in for a NumPy scalar type
        // (`numpy.integer`): a real `PyTypeObject` behind an opaque handle.
        let base_type = extension_type(c"abcprobe.Base", None);
        let base = extension_value(base_type, "type");
        assert!(crate::capi::object_is_type(base_type));

        // Registering the C class records the virtual edge and returns the
        // argument unchanged, like the Hydro-class and builtin-spelling paths.
        assert_eq!(call("ABCMeta.register", &[number, base]), base);
        assert!(!exc::pending());
        assert!(is_virtual_subclass(base, number));
        // The C API spelling consults the same registry, so extension code
        // calling `PyObject_IsSubclass(numpy.integer, numbers.Integral)` sees
        // the registration.
        assert!(is_virtual_extension_subclass(base_type, number));
        let abc_handle = crate::capi::value_object(crate::capi::hydro_shim(number));
        assert_eq!(crate::capi::PyObject_IsSubclass(base_type, abc_handle), 1);
        crate::capi::Py_DecRef(abc_handle);
        assert_eq!(call("get_cache_token", &[]), ops::int_result(1));

        // Instances of the registered C class match through
        // `PyObject_IsInstance`.
        let instance_like = extension_value(
            crate::capi::_PyObject_New(base_type),
            "abcprobe.Base instance",
        );
        assert!(is_virtual_instance(instance_like, number));
        // The C API `isinstance` spelling routes through Hydro's registry too.
        let instance_handle = crate::capi::value_object(crate::capi::hydro_shim(number));
        assert_eq!(
            crate::capi::PyObject_IsInstance(
                value::ext_object_pointer(instance_like).expect("extension handle"),
                instance_handle,
            ),
            1
        );
        crate::capi::Py_DecRef(instance_handle);

        // The extension's own subclasses match too.
        let derived_type = extension_type(c"abcprobe.Derived", Some(base_type));
        let derived = extension_value(derived_type, "type");
        assert!(is_virtual_subclass(derived, number));
        let derived_instance = extension_value(
            crate::capi::_PyObject_New(derived_type),
            "abcprobe.Derived instance",
        );
        assert!(is_virtual_instance(derived_instance, number));

        // An unrelated extension type does not match.
        let other_type = extension_type(c"abcprobe.Other", None);
        let other_instance = extension_value(
            crate::capi::_PyObject_New(other_type),
            "abcprobe.Other instance",
        );
        assert!(!is_virtual_instance(other_instance, number));
        let other_class = extension_value(other_type, "type");
        assert!(!is_virtual_subclass(other_class, number));

        // A second handle to the same type is the same registration: it is
        // idempotent and does not bump the cache token.
        crate::capi::Py_IncRef(base_type);
        let duplicate = extension_value(base_type, "type");
        assert_eq!(call("ABCMeta.register", &[number, duplicate]), duplicate);
        assert_eq!(call("get_cache_token", &[]), ops::int_result(1));
    }

    #[test]
    fn register_rejects_extension_instances() {
        let _guard = guard();
        reset();
        let number = number_abc();
        let type_object = extension_type(c"abcprobe.Instance", None);
        // A C *instance* is an opaque handle whose type is not a metaclass;
        // `register` must reject it instead of recording an edge with no
        // effect.
        let instance =
            extension_value(crate::capi::_PyObject_New(type_object), "abcprobe.Instance");
        assert!(!crate::capi::object_is_type(
            value::ext_object_pointer(instance).expect("extension handle")
        ));
        call("ABCMeta.register", &[number, instance]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        assert!(!is_virtual_instance(instance, number));
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
    fn instantiation_requires_every_abstract_method() {
        let _guard = guard();
        reset();
        let metaclass = abc_metaclass();
        let abstract_render = value::native_function_new("abc", "abstractmethod", 1);
        let abstract_save = value::native_function_new("abc", "abstractmethod", 2);
        assert_eq!(call("abstractmethod", &[abstract_render]), abstract_render);
        assert_eq!(call("abstractmethod", &[abstract_save]), abstract_save);
        let base = class_new_multi(
            "Base".to_string(),
            Vec::new(),
            metaclass,
            vec![
                ("render".to_string(), abstract_render, MethodKind::Normal),
                ("save".to_string(), abstract_save, MethodKind::Normal),
            ],
            Vec::new(),
        );

        let pending_message = || {
            let value = exc::pending_value();
            value::exception_data(value)
                .map(|data| data.msg.clone())
                .unwrap_or_default()
        };

        // Both names remain: CPython's diagnostic sorts and quotes them. The
        // ordinary class-call hook must reject the instance, not just the
        // standalone checker.
        assert_eq!(crate::builtins::call(base, 0, std::ptr::null()), NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "Can't instantiate abstract class Base without an implementation for \
             abstract methods 'render', 'save'"
        );
        exc::clear();
        assert_eq!(check_instantiable(base), Some(NONE));
        exc::clear();

        // Overriding every abstract name makes the class constructible.
        let render = value::native_function_new("abc", "render", 1);
        let save = value::native_function_new("abc", "save", 2);
        let concrete = class_new_multi(
            "Concrete".to_string(),
            vec![base],
            NONE,
            vec![
                ("render".to_string(), render, MethodKind::Normal),
                ("save".to_string(), save, MethodKind::Normal),
            ],
            Vec::new(),
        );
        assert_eq!(check_instantiable(concrete), None);
        assert!(!exc::pending());
        let instance = crate::builtins::call(concrete, 0, std::ptr::null());
        assert!(!exc::pending());
        assert_eq!(value::instance_class_value(instance), Some(concrete));

        // A partial override keeps the remaining name abstract.
        let partial = class_new_multi(
            "Partial".to_string(),
            vec![base],
            NONE,
            vec![("render".to_string(), render, MethodKind::Normal)],
            Vec::new(),
        );
        assert_eq!(check_instantiable(partial), Some(NONE));
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "Can't instantiate abstract class Partial without an implementation for \
             abstract method 'save'"
        );
        exc::clear();
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
