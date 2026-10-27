//! Pure-Python `functools` internals used by `inspect`, plus the real
//! `partialmethod` descriptor and `Placeholder` singleton.
//!
//! `update_wrapper`/`wraps` are implemented over the runtime's function
//! values: `update_wrapper(wrapper, wrapped)` copies the metadata slots
//! `wrapped` actually exposes (skipping absent ones, exactly like CPython),
//! updates the requested mapping attributes, and records
//! `wrapper.__wrapped__`. `wraps(wrapped)` returns the decorator that applies
//! it. Function values therefore carry the writable attribute surface CPython
//! gives them: `__name__`, `__qualname__`, `__doc__`, an assigned
//! `__module__`/`__annotations__` and a live `__dict__`.
//!
//! `inspect._has_code_flag` unwraps wrapper descriptors before reading
//! `__code__.co_flags`:
//!
//! ```python
//! f = functools._unwrap_partialmethod(f)
//! ...
//! f = functools._unwrap_partial(f)
//! ```
//!
//! This module implements those helpers over the runtime's real callable
//! values (`FunctionBody::Partial` chains and class-instance descriptors).
//! `_singledispatchmethod_get` is also a class value at runtime because
//! `inspect.isroutine` uses it as an `isinstance` target.
//!
//! `partialmethod` is a native class implementing CPython's descriptor
//! protocol: `__get__` binds the instance (or returns an unbound callable)
//! and merges stored arguments with the arguments of the call, filling
//! `Placeholder` holes exactly like `functools.partialmethod` does. The
//! `Placeholder` singleton is an instance of `_PlaceholderType`.
//!
//! `cached_property` is a native class too, and it caches for real: `__get__`
//! calls the wrapped getter once per instance and stores the result in the
//! instance's `__dict__` under the attribute name, so every later read is a
//! plain instance-attribute hit. The class namespace has no creation hook
//! (see [`cached_property_name`]), so the descriptor learns its attribute name
//! from a `__set_name__` call when one happens and otherwise recovers the same
//! name from the owner class namespace on the first access.
//!
//! Limits, reported instead of fabricated:
//! - `singledispatchmethod` itself is not implemented, so the wrapper stores
//!   the descriptor and the `(obj, cls)` pair but cannot delegate dispatch.
//! - `functools.partial` objects do not fill `Placeholder` holes themselves
//!   (the partial call path lives in the value layer); `partialmethod`
//!   forwards its own placeholders through the merged argument list.

use hydro_ir::ParamKind;

use crate::core::value::{self, ClassEntry, FunctionBody, NONE};
use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR};
use crate::heap;
use hydro_ir::MethodKind;

/// CPython's `functools.WRAPPER_ASSIGNMENTS`.
const WRAPPER_ASSIGNMENTS: [&str; 5] = [
    "__module__",
    "__name__",
    "__qualname__",
    "__doc__",
    "__annotations__",
];

/// CPython's `functools.WRAPPER_UPDATES`.
const WRAPPER_UPDATES: [&str; 1] = ["__dict__"];

/// A `(name, ...)` tuple of freshly allocated strings, kept rooted while the
/// tuple allocation may trigger a collection.
fn string_tuple(names: &[&str]) -> i64 {
    let items: Vec<i64> = names
        .iter()
        .map(|name| value::str_alloc((*name).to_string()))
        .collect();
    let roots = heap::root_values(&items);
    let tuple = value::tuple_new(items);
    heap::release_roots(roots);
    tuple
}

/// `functools.WRAPPER_ASSIGNMENTS` as a value tuple.
pub(crate) fn default_assigned() -> i64 {
    string_tuple(&WRAPPER_ASSIGNMENTS)
}

/// `functools.WRAPPER_UPDATES` as a value tuple.
pub(crate) fn default_updated() -> i64 {
    string_tuple(&WRAPPER_UPDATES)
}

/// Resolves an optional `assigned`/`updated` argument.
///
/// Codegen pads an omitted keyword slot with `None` (`wraps(f, updated=())`
/// lowers to `wraps(f, None, ())`), so `None` stands for "use the default
/// list". An explicit `None` from source cannot be distinguished from the
/// padding and takes the same path, which is the convention every native
/// module function with optional parameters follows.
fn wrapper_argument(argument: Option<i64>, default: fn() -> i64) -> i64 {
    match argument {
        Some(item) if item != NONE => item,
        _ => default(),
    }
}

/// Outcome of reading one attribute for `update_wrapper`: a missing attribute
/// leaves the pending `AttributeError` in place so the caller can decide
/// whether CPython skips it (`assigned`) or propagates it (`updated`).
enum AttributeRead {
    Found(i64),
    Missing,
    Failed,
}

fn read_attribute(obj: i64, name: &str) -> AttributeRead {
    let result = crate::core::methods::getattr(obj, name.as_ptr(), name.len());
    if !exc::pending() {
        return AttributeRead::Found(result);
    }
    if exc::pending_tag() == ATTRIBUTE_ERROR as i64 {
        return AttributeRead::Missing;
    }
    AttributeRead::Failed
}

fn write_attribute(obj: i64, name: &str, item: i64) -> bool {
    crate::core::methods::setattr(obj, name.as_ptr(), name.len(), item);
    !exc::pending()
}

/// Materialises an `assigned`/`updated` argument as attribute names.
///
/// CPython iterates the argument; Hydro accepts the tuple/list forms the
/// standard library and ordinary call sites produce and reports a `TypeError`
/// for anything else instead of guessing.
fn attribute_names(items: i64, parameter: &str) -> Result<Vec<String>, ()> {
    let Some(entries) = value::as_tuple(items).or_else(|| value::as_list(items)) else {
        exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "update_wrapper() argument '{parameter}' must be a sequence of attribute names, not '{}'",
                value::type_name(items)
            ),
        );
        return Err(());
    };
    let mut names = Vec::with_capacity(entries.len());
    for &entry in entries {
        match value::as_str(entry) {
            Some(name) => names.push(name.to_string()),
            None => {
                exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "attribute name must be string, not '{}'",
                        value::type_name(entry)
                    ),
                );
                return Err(());
            }
        }
    }
    Ok(names)
}

/// `functools.update_wrapper(wrapper, wrapped, assigned=WRAPPER_ASSIGNMENTS,
/// updated=WRAPPER_UPDATES)`.
///
/// Follows CPython's algorithm exactly: assigned attributes that `wrapped`
/// exposes are copied onto `wrapper`, missing ones are skipped (an absent
/// `__dict__`/`__annotations__` therefore never fabricates a value), the
/// `updated` attributes are updated from `wrapped` (defaulting to an empty
/// mapping when absent), and `wrapper.__wrapped__` is set to `wrapped`.
fn update_wrapper(wrapper: i64, wrapped: i64, assigned: Option<i64>, updated: Option<i64>) -> i64 {
    let roots = heap::root_values(&[wrapper, wrapped]);
    let assigned = wrapper_argument(assigned, default_assigned);
    // The assigned default may allocate, and the updated default allocates
    // again; keep the first list rooted while the second is materialised.
    let assigned_roots = heap::root_values(&[assigned]);
    let updated = wrapper_argument(updated, default_updated);
    let inner = heap::root_values(&[assigned, updated]);
    heap::release_roots(assigned_roots);
    let result = apply_wrapper_metadata(wrapper, wrapped, assigned, updated);
    heap::release_roots(inner);
    heap::release_roots(roots);
    result
}

fn apply_wrapper_metadata(wrapper: i64, wrapped: i64, assigned: i64, updated: i64) -> i64 {
    let Ok(assigned_names) = attribute_names(assigned, "assigned") else {
        return NONE;
    };
    let Ok(updated_names) = attribute_names(updated, "updated") else {
        return NONE;
    };
    for name in assigned_names {
        match read_attribute(wrapped, &name) {
            AttributeRead::Found(item) => {
                // `getattr` may have allocated the value just read, so keep it
                // rooted while `setattr` allocates the wrapper's storage.
                let roots = heap::root_values(&[item]);
                let written = write_attribute(wrapper, &name, item);
                heap::release_roots(roots);
                if !written {
                    return NONE;
                }
            }
            // CPython ignores an assigned attribute `wrapped` does not have.
            AttributeRead::Missing => {
                exc::clear();
            }
            AttributeRead::Failed => return NONE,
        }
    }
    for name in updated_names {
        // `getattr(wrapper, attr)` is unguarded in CPython, so a wrapper
        // without the mapping propagates the AttributeError.
        let target = match read_attribute(wrapper, &name) {
            AttributeRead::Found(item) => item,
            AttributeRead::Missing | AttributeRead::Failed => return NONE,
        };
        let roots = heap::root_values(&[target]);
        let source = match read_attribute(wrapped, &name) {
            AttributeRead::Found(item) => item,
            AttributeRead::Missing => {
                exc::clear();
                value::dict_new(Vec::new())
            }
            AttributeRead::Failed => {
                heap::release_roots(roots);
                return NONE;
            }
        };
        let argv = [source];
        let edges = heap::root_values(&[source]);
        crate::core::methods::call(
            target,
            b"update".as_ptr(),
            "update".len(),
            argv.len(),
            argv.as_ptr(),
        );
        heap::release_roots(edges);
        heap::release_roots(roots);
        if exc::pending() {
            return NONE;
        }
    }
    if !write_attribute(wrapper, "__wrapped__", wrapped) {
        return NONE;
    }
    wrapper
}

/// `functools.wraps(wrapped, assigned=WRAPPER_ASSIGNMENTS,
/// updated=WRAPPER_UPDATES)`: the decorator that applies `update_wrapper`
/// with the given (or default) assignment lists.
///
/// The decorator is a `functools.partial` of the internal `_wraps_apply`
/// helper. `functools.partial` prepends its bound arguments, so the helper
/// takes the decorator's single argument — the wrapper — last:
/// `_wraps_apply(wrapped, assigned, updated, wrapper)`. The partial therefore
/// binds `wrapped`, `assigned` and `updated` (the defaults when they were
/// omitted) and keeps every bound value as a traced GC edge.
pub(crate) fn wraps_decorator(wrapped: i64, assigned: Option<i64>, updated: Option<i64>) -> i64 {
    let apply = value::native_function_new("functools", "_wraps_apply", 4);
    let roots = heap::root_values(&[wrapped, apply]);
    let assigned = wrapper_argument(assigned, default_assigned);
    // Both lists must survive the allocations that follow; root each one as it
    // is produced so a collection cannot drop the first while the second is
    // being materialised.
    let assigned_roots = heap::root_values(&[assigned]);
    let updated = wrapper_argument(updated, default_updated);
    let bound = [wrapped, assigned, updated];
    let edges = heap::root_values(&bound);
    heap::release_roots(assigned_roots);
    let decorator = value::partial_new(apply, bound.to_vec());
    heap::release_roots(edges);
    heap::release_roots(roots);
    decorator
}

/// `functools._unwrap_partial(func)`: repeatedly replaces a
/// `functools.partial` with its `.func` target.
pub(crate) fn unwrap_partial(mut value: i64) -> i64 {
    loop {
        let Some(function) = value::as_function(value) else {
            return value;
        };
        match &function.body {
            FunctionBody::Partial { function, .. } => value = *function,
            _ => return value,
        }
    }
}

/// Reads an attribute from an instance dictionary or its class namespace.
///
/// Only plain data attributes are considered: invoking a descriptor here would
/// execute Python code during unwrapping, which `inspect` does not do either
/// (it reads `__partialmethod__`/`func` as data).
fn instance_attribute(obj: i64, name: &str) -> Option<i64> {
    let class = value::instance_class_value(obj)?;
    if let Some(attribute) = value::instance_attr(obj, name) {
        return Some(attribute);
    }
    match value::class_lookup(class, name) {
        Some(ClassEntry::Attr(attribute)) => Some(attribute),
        _ => None,
    }
}

/// A representable `partialmethod` descriptor: an instance of a class named
/// `partialmethod` with a stored `func` callable.
fn is_partialmethod_descriptor(obj: i64) -> bool {
    value::instance_class_name(obj) == Some("partialmethod")
        && instance_attribute(obj, "func").is_some()
}

/// `functools._unwrap_partialmethod(func)`.
///
/// Follows CPython's loop: unwrap `__partialmethod__` back-references, unwrap
/// the descriptor's `func`, then unwrap any `partial` chain. Value identity is
/// used as the termination condition, like the pure-Python implementation.
pub(crate) fn unwrap_partialmethod(mut value: i64) -> i64 {
    let mut previous = NONE;
    while value != previous {
        previous = value;
        while let Some(marker) = instance_attribute(value, "__partialmethod__") {
            if !is_partialmethod_descriptor(marker) || marker == value {
                break;
            }
            value = marker;
        }
        while is_partialmethod_descriptor(value) {
            let Some(target) = instance_attribute(value, "func") else {
                break;
            };
            if target == value {
                break;
            }
            value = target;
        }
        value = unwrap_partial(value);
    }
    value
}

/// The callable stored by a `singledispatchmethod`/`partialmethod` descriptor
/// (`descriptor.func`). Raises `AttributeError` when the attribute is absent.
fn descriptor_function(unbound: i64) -> i64 {
    if let Some(function) = instance_attribute(unbound, "func") {
        return function;
    }
    // A `partial` wrapper's target is the representable function behind it.
    if let Some(function) = value::as_function(unbound) {
        if matches!(&function.body, FunctionBody::Partial { .. }) {
            return unwrap_partial(unbound);
        }
    }
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!(
            "'{}' object has no attribute 'func'",
            value::type_name(unbound)
        ),
    )
}

/// The cached `functools._singledispatchmethod_get` class.
///
/// `inspect.isroutine` reads it as an `isinstance` target, so every access
/// must return one class value.
pub(crate) fn singledispatchmethod_get_class() -> i64 {
    if let Some(cached) =
        crate::core::globals::registry_get("functools._singledispatchmethod_get.class")
    {
        return cached;
    }
    let methods: Vec<(String, i64, MethodKind)> = vec![
        (
            "__init__".to_string(),
            value::native_function_new(
                "functools",
                "_singledispatchmethod_get.__init__",
                usize::MAX,
            ),
            MethodKind::Normal,
        ),
        (
            "__repr__".to_string(),
            value::native_function_new("functools", "_singledispatchmethod_get.__repr__", 1),
            MethodKind::Normal,
        ),
    ];
    let method_values: Vec<i64> = methods.iter().map(|(_, function, _)| *function).collect();
    let method_roots = heap::root_values(&method_values);
    let class = value::class_new_full(
        "_singledispatchmethod_get".to_string(),
        NONE,
        methods,
        Vec::new(),
    );
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    crate::core::globals::registry_set("functools._singledispatchmethod_get.class", class);
    heap::release_roots(class_roots);
    heap::release_roots(method_roots);
    class
}

/// Stores the descriptor and receiver on a `_singledispatchmethod_get`
/// instance. Returns the instance, or a pending `None` on error.
fn init_get(instance: i64, unbound: i64, obj: i64, cls: i64) -> i64 {
    let function = descriptor_function(unbound);
    if exc::pending() {
        return NONE;
    }
    let roots = heap::root_values(&[unbound, obj, cls, function]);
    value::instance_setattr(instance, "_unbound".to_string(), unbound);
    value::instance_setattr(instance, "_obj".to_string(), obj);
    value::instance_setattr(instance, "_cls".to_string(), cls);
    value::instance_setattr(instance, "_func".to_string(), function);
    heap::release_roots(roots);
    instance
}

/// Allocates a `_singledispatchmethod_get` wrapper for `(unbound, obj, cls)`.
fn new_get(unbound: i64, obj: i64, cls: i64) -> i64 {
    let class = singledispatchmethod_get_class();
    let roots = heap::root_values(&[unbound, obj, cls, class]);
    let instance = value::instance_new(class);
    let result = init_get(instance, unbound, obj, cls);
    heap::release_roots(roots);
    result
}

fn get_repr(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return value::str_alloc("<single dispatch method _singledispatchmethod_get>".to_string());
    };
    let name = value::instance_attr(instance, "_func")
        .and_then(|function| value::as_function(function).map(|f| f.name.clone()))
        .unwrap_or_else(|| "?".to_string());
    let obj = value::instance_attr(instance, "_obj").unwrap_or(NONE);
    if obj == NONE {
        value::str_alloc(format!("<single dispatch method {name}>"))
    } else {
        value::str_alloc(format!(
            "<bound single dispatch method {name} of {}>",
            value::repr(obj)
        ))
    }
}

/// Materialises the `functools` constants.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "Placeholder" => placeholder(),
        "_PlaceholderType" => placeholder_type_class(),
        // The default assignment lists `functools.update_wrapper` uses; both
        // are value tuples, so a caller can pass them back explicitly.
        "WRAPPER_ASSIGNMENTS" => default_assigned(),
        "WRAPPER_UPDATES" => default_updated(),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'functools' has no attribute '{name}'"),
        ),
    }
}

/// Cached `functools._PlaceholderType` class. The class is the identity of
/// the `Placeholder` singleton, so every access must return the same value.
pub(crate) fn placeholder_type_class() -> i64 {
    if let Some(cached) = crate::core::globals::registry_get("functools._PlaceholderType") {
        return cached;
    }
    let repr = method_entry(
        "functools",
        "_placeholder_repr",
        placeholder_repr_entry,
        vec![self_parameter()],
    );
    let class = value::class_new(
        "_PlaceholderType".to_string(),
        NONE,
        vec![("__repr__".to_string(), repr)],
    );
    crate::core::globals::registry_set("functools._PlaceholderType", class);
    class
}

/// The `functools.Placeholder` singleton.
pub(crate) fn placeholder() -> i64 {
    if let Some(cached) = crate::core::globals::registry_get("functools.Placeholder") {
        return cached;
    }
    let class = placeholder_type_class();
    let instance = value::instance_new(class);
    crate::core::globals::registry_set("functools.Placeholder", instance);
    instance
}

/// Whether `item` is any `_PlaceholderType` instance (the singleton or a
/// separately constructed one, which behaves the same way).
fn is_placeholder(item: i64) -> bool {
    value::instance_class_value(item) == Some(placeholder_type_class())
}

/// A `self` parameter descriptor for native methods.
fn self_parameter() -> value::ParamMeta {
    value::ParamMeta {
        name: "self".to_string(),
        kind: ParamKind::Positional,
        default: None,
    }
}

/// Builds a native method with a declared signature and full-argv trampoline.
///
/// These internal names have no entry in the central module spec: callers must
/// reach them through the shared signature binder (attribute dispatch, `super`,
/// `builtins::call_kw`), which runs the trampoline with the receiver bound.
/// A plain positional call instead resolves the body as a module function and
/// raises `AttributeError`.
fn method_entry(
    module: &'static str,
    name: &'static str,
    entry: extern "C" fn(*const i64) -> i64,
    signature: Vec<value::ParamMeta>,
) -> i64 {
    let function = value::native_function_new(module, name, usize::MAX);
    value::function_set_signature(function, signature);
    value::function_set_full_entry(function, entry as *const () as usize);
    function
}

extern "C" fn placeholder_repr_entry(_argv: *const i64) -> i64 {
    value::str_alloc("Placeholder".to_string())
}

extern "C" fn partialmethod_repr_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self` first.
    partialmethod_repr(unsafe { *argv })
}

extern "C" fn partialmethod_isabstractmethod_entry(argv: *const i64) -> i64 {
    // SAFETY: the property binder passes `self` first.
    partialmethod_isabstractmethod(unsafe { *argv })
}

/// The cached `functools._partialmethod_call` class: the callable produced by
/// `partialmethod.__get__` when the wrapped callable has no descriptor
/// protocol of its own. It stores the descriptor and, for instance access,
/// the bound receiver.
fn partialmethod_call_class() -> i64 {
    if let Some(cached) = crate::core::globals::registry_get("functools._partialmethod_call") {
        return cached;
    }
    let signature = vec![
        value::ParamMeta {
            name: "self".to_string(),
            kind: ParamKind::Positional,
            default: None,
        },
        value::ParamMeta {
            name: "args".to_string(),
            kind: ParamKind::VarArg,
            default: None,
        },
        value::ParamMeta {
            name: "kwargs".to_string(),
            kind: ParamKind::KwArg,
            default: Some(NONE),
        },
    ];
    let call = value::native_function_new("functools", "_partialmethod_call", usize::MAX);
    value::function_set_signature(call, signature);
    value::function_set_full_entry(call, partialmethod_call_entry as *const () as usize);
    let class = value::class_new(
        "_partialmethod_call".to_string(),
        NONE,
        vec![("__call__".to_string(), call)],
    );
    crate::core::globals::registry_set("functools._partialmethod_call", class);
    class
}

/// The cached `functools.partialmethod` class.
pub(crate) fn partialmethod_class() -> i64 {
    if let Some(cached) = crate::core::globals::registry_get("functools.partialmethod") {
        return cached;
    }
    let init_signature = vec![
        value::ParamMeta {
            name: "self".to_string(),
            kind: ParamKind::Positional,
            default: None,
        },
        value::ParamMeta {
            name: "func".to_string(),
            kind: ParamKind::Positional,
            default: None,
        },
        value::ParamMeta {
            name: "args".to_string(),
            kind: ParamKind::VarArg,
            default: None,
        },
        value::ParamMeta {
            name: "kwargs".to_string(),
            kind: ParamKind::KwArg,
            default: Some(NONE),
        },
    ];
    let init = value::native_function_new("functools", "_partialmethod_init", usize::MAX);
    value::function_set_signature(init, init_signature);
    value::function_set_full_entry(init, partialmethod_init_entry as *const () as usize);
    let repr = method_entry(
        "functools",
        "_partialmethod_repr",
        partialmethod_repr_entry,
        vec![self_parameter()],
    );
    let get = method_entry(
        "functools",
        "_partialmethod_get",
        partialmethod_get_entry,
        vec![
            self_parameter(),
            value::ParamMeta {
                name: "obj".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
            value::ParamMeta {
                name: "cls".to_string(),
                kind: ParamKind::Positional,
                default: Some(NONE),
            },
        ],
    );
    let is_abstract_method = method_entry(
        "functools",
        "_partialmethod_isabstractmethod",
        partialmethod_isabstractmethod_entry,
        vec![self_parameter()],
    );
    let class = value::class_new_full(
        "partialmethod".to_string(),
        NONE,
        vec![
            ("__init__".to_string(), init, MethodKind::Normal),
            ("__repr__".to_string(), repr, MethodKind::Normal),
            ("__get__".to_string(), get, MethodKind::Normal),
            (
                "__isabstractmethod__".to_string(),
                is_abstract_method,
                MethodKind::Property,
            ),
        ],
        Vec::new(),
    );
    crate::core::globals::registry_set("functools.partialmethod", class);
    class
}

/// `functools.partialmethod(func, /, *args, **keywords)` through the module
/// call path.
pub(crate) fn new_partialmethod(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let Some(func) = args.first().copied() else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "partialmethod() missing required argument 'func'",
        );
    };
    build_partialmethod(func, &args[1..], keywords)
}

/// `functools.partialmethod.__init__` through the class-call path. The binder
/// passes `(self, func, *args, **kwargs)`.
extern "C" fn partialmethod_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the class-call binder passes one slot per declared parameter.
    let instance = unsafe { *argv };
    let func = unsafe { *argv.add(1) };
    let args_value = unsafe { *argv.add(2) };
    let kwargs_value = unsafe { *argv.add(3) };
    let args: Vec<i64> = value::as_list(args_value)
        .map(|items| items.to_vec())
        .unwrap_or_default();
    let keywords: Vec<(String, i64)> = value::as_dict(kwargs_value)
        .map(|pairs| {
            pairs
                .iter()
                .filter_map(|(key, item)| value::as_str(*key).map(|name| (name.to_string(), *item)))
                .collect()
        })
        .unwrap_or_default();
    let roots = heap::root_values(&[instance, func]);
    let result = initialize_partialmethod(instance, func, &args, &keywords);
    heap::release_roots(roots);
    result
}

/// Validates the arguments and stores CPython's `partialmethod` state.
fn build_partialmethod(func: i64, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let class = partialmethod_class();
    let roots = heap::root_values(&[func, class]);
    let instance = value::instance_new(class);
    let instance_roots = heap::root_values(&[instance]);
    let result = initialize_partialmethod(instance, func, args, keywords);
    heap::release_roots(instance_roots);
    heap::release_roots(roots);
    result
}

/// Stores `func`/`args`/`keywords` plus the placeholder bookkeeping on
/// `instance`, flattening a nested `partialmethod` like CPython's
/// `_partial_new`. Returns the instance, or `None` with a pending exception.
fn initialize_partialmethod(
    instance: i64,
    func: i64,
    args: &[i64],
    keywords: &[(String, i64)],
) -> i64 {
    let callable_or_descriptor =
        value::truthy(crate::core::builtins::callable(func)) || has_descriptor_get(func);
    if !callable_or_descriptor {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "the first argument {} must be a callable or a descriptor",
                value::repr(func)
            ),
        );
    }
    if let Some(last) = args.last() {
        if is_placeholder(*last) {
            return exc::raise_with_message(TYPE_ERROR, "trailing Placeholders are not allowed");
        }
    }
    for (_, item) in keywords {
        if is_placeholder(*item) {
            return exc::raise_with_message(
                TYPE_ERROR,
                "Placeholder cannot be passed as a keyword argument",
            );
        }
    }
    // Flatten a partialmethod of the same class, like CPython's `_partial_new`.
    let mut stored_func = func;
    let mut stored_args: Vec<i64> = args.to_vec();
    let mut stored_keywords: Vec<(String, i64)> = keywords.to_vec();
    if value::instance_class_value(func) == Some(partialmethod_class()) {
        let previous_args = attribute_tuple(func, "args").unwrap_or_default();
        let previous_keywords = attribute_pairs(func, "keywords").unwrap_or_default();
        let phcount = attribute_count(func, "_phcount");
        let mut merged = previous_args.clone();
        if !args.is_empty() {
            merged.extend_from_slice(args);
            if phcount > 0 {
                if args.len() < phcount {
                    for _ in 0..(phcount - args.len()) {
                        merged.push(placeholder());
                    }
                }
                let order = merger_order(&previous_args);
                merged = apply_order(&order, &merged);
                if args.len() > phcount {
                    merged.extend_from_slice(&args[phcount..]);
                }
            }
        }
        if merged.last().is_some_and(|item| is_placeholder(*item)) {
            return exc::raise_with_message(TYPE_ERROR, "trailing Placeholders are not allowed");
        }
        let mut combined = previous_keywords;
        for (name, item) in keywords {
            combined.retain(|(existing, _)| existing != name);
            combined.push((name.clone(), *item));
        }
        stored_func = attribute(func, "func").unwrap_or(NONE);
        stored_args = merged;
        stored_keywords = combined;
    }
    // Recompute the placeholder bookkeeping for the flattened argument list.
    let count = stored_args
        .iter()
        .filter(|item| is_placeholder(**item))
        .count();
    let merger = if count == 0 {
        NONE
    } else {
        let keys: Vec<i64> = merger_order(&stored_args)
            .iter()
            .map(|index| value::encode_int(*index as i64).unwrap_or(NONE))
            .collect();
        value::item_getter_new(keys)
    };
    let args_tuple = value::tuple_new(stored_args.clone());
    let keyword_dict = value::dict_new(
        stored_keywords
            .iter()
            .map(|(name, item)| (value::str_alloc(name.clone()), *item))
            .collect(),
    );
    let count_value = value::encode_int(count as i64).unwrap_or(NONE);
    let roots = heap::root_values(&[instance, stored_func, args_tuple, keyword_dict, merger]);
    for (name, item) in [
        ("func", stored_func),
        ("args", args_tuple),
        ("keywords", keyword_dict),
        ("_phcount", count_value),
        ("_merger", merger),
    ] {
        value::instance_setattr(instance, name.to_string(), item);
    }
    heap::release_roots(roots);
    instance
}

/// The itemgetter key order `_partial_prepare_merger` computes: stored
/// placeholders consume the call arguments that follow the stored ones.
fn merger_order(stored: &[i64]) -> Vec<usize> {
    let mut order = Vec::with_capacity(stored.len());
    let mut next = stored.len();
    for (index, item) in stored.iter().enumerate() {
        if is_placeholder(*item) {
            order.push(next);
            next += 1;
        } else {
            order.push(index);
        }
    }
    order
}

fn apply_order(order: &[usize], values: &[i64]) -> Vec<i64> {
    let mut out = Vec::with_capacity(order.len());
    for index in order {
        match values.get(*index) {
            Some(item) => out.push(*item),
            None => return out,
        }
    }
    out
}

/// Reads a stored value attribute without invoking descriptors.
fn attribute(obj: i64, name: &str) -> Option<i64> {
    value::instance_attr(obj, name)
}

fn attribute_tuple(obj: i64, name: &str) -> Option<Vec<i64>> {
    match attribute(obj, name) {
        Some(value) => {
            let items = value::as_tuple(value)?;
            Some(items.to_vec())
        }
        None => None,
    }
}

fn attribute_pairs(obj: i64, name: &str) -> Option<Vec<(String, i64)>> {
    let value = attribute(obj, name)?;
    let pairs = value::as_dict(value)?;
    Some(
        pairs
            .iter()
            .filter_map(|(key, item)| value::as_str(*key).map(|name| (name.to_string(), *item)))
            .collect(),
    )
}

fn attribute_count(obj: i64, name: &str) -> usize {
    attribute(obj, name)
        .and_then(value::as_integer)
        .and_then(|count| usize::try_from(count).ok())
        .unwrap_or(0)
}

/// Whether `obj` itself implements the descriptor protocol (`__get__`).
fn has_descriptor_get(obj: i64) -> bool {
    if value::as_descriptor(obj).is_some() {
        return true;
    }
    let Some(class) = value::instance_class_value(obj) else {
        return false;
    };
    value::class_lookup(class, "__get__").is_some()
}

/// `obj.__get__(instance, owner)` for objects with a descriptor protocol.
/// Returns `None` when `obj` has none.
fn descriptor_protocol_get(obj: i64, instance: i64, owner: i64) -> Option<i64> {
    if let Some(state) = value::as_descriptor(obj) {
        return Some(match state.kind {
            value::DescriptorKind::Classmethod => value::partial_new(state.target, vec![owner]),
            value::DescriptorKind::Staticmethod => state.target,
            value::DescriptorKind::Property => {
                if instance == NONE {
                    obj
                } else {
                    let mut call_args = vec![instance];
                    crate::core::builtins::call(state.target, 1, call_args.as_mut_ptr())
                }
            }
        });
    }
    let class = value::instance_class_value(obj)?;
    value::class_lookup(class, "__get__")?;
    let getter = value::bound_method_new(obj, "__get__".to_string());
    let args = [instance, owner];
    Some(crate::core::builtins::call(
        getter,
        args.len(),
        args.as_ptr(),
    ))
}

/// `partialmethod.__get__(self, obj, cls)`.
extern "C" fn partialmethod_get_entry(argv: *const i64) -> i64 {
    let this = unsafe { *argv };
    let obj = unsafe { *argv.add(1) };
    let cls = unsafe { *argv.add(2) };
    let Some(func) = attribute(this, "func") else {
        return exc::raise_with_message(ATTRIBUTE_ERROR, "partialmethod has no 'func'");
    };
    if let Some(new_func) = descriptor_protocol_get(func, obj, cls) {
        if new_func != func {
            let args = attribute_tuple(this, "args").unwrap_or_default();
            let keywords = attribute_pairs(this, "keywords").unwrap_or_default();
            return value::partial_new_with_keywords(new_func, args, keywords);
        }
    }
    // The wrapped callable is not a descriptor: build CPython's unbound
    // `_method` callable, bound to `obj` unless this is a class access.
    let class = partialmethod_call_class();
    let roots = heap::root_values(&[this, class]);
    let callable = value::instance_new(class);
    let bound = if obj == NONE { NONE } else { obj };
    value::instance_setattr(callable, "_descriptor".to_string(), this);
    value::instance_setattr(callable, "_bound".to_string(), bound);
    value::instance_setattr(callable, "__partialmethod__".to_string(), this);
    heap::release_roots(roots);
    value::bound_method_new(callable, "__call__".to_string())
}

/// `_partialmethod_call.__call__(self, *args, **kwargs)`: merges the stored
/// arguments (filling `Placeholder` holes) and calls the wrapped callable.
extern "C" fn partialmethod_call_entry(argv: *const i64) -> i64 {
    let this = unsafe { *argv };
    let call_args_value = unsafe { *argv.add(1) };
    let call_kwargs_value = unsafe { *argv.add(2) };
    let call_args: Vec<i64> = value::as_list(call_args_value)
        .map(|items| items.to_vec())
        .unwrap_or_default();
    let call_keywords: Vec<(String, i64)> = value::as_dict(call_kwargs_value)
        .map(|pairs| {
            pairs
                .iter()
                .filter_map(|(key, item)| value::as_str(*key).map(|name| (name.to_string(), *item)))
                .collect()
        })
        .unwrap_or_default();
    let Some(descriptor) = attribute(this, "_descriptor") else {
        return exc::raise_with_message(TYPE_ERROR, "partialmethod callable has no descriptor");
    };
    let Some(func) = attribute(descriptor, "func") else {
        return exc::raise_with_message(TYPE_ERROR, "partialmethod has no 'func'");
    };
    let stored = attribute_tuple(descriptor, "args").unwrap_or_default();
    let mut keywords = attribute_pairs(descriptor, "keywords").unwrap_or_default();
    for (name, item) in call_keywords {
        keywords.retain(|(existing, _)| existing != &name);
        keywords.push((name, item));
    }
    let bound = attribute(this, "_bound").unwrap_or(NONE);
    let (receiver, mut tail) = if bound == NONE {
        if call_args.is_empty() {
            return exc::raise_with_message(
                TYPE_ERROR,
                "partialmethod callable missing required argument 'cls_or_self'",
            );
        }
        (call_args[0], call_args[1..].to_vec())
    } else {
        (bound, call_args)
    };
    let (stored_args, remaining) = merge_placeholders(&stored, &tail);
    if exc::pending() {
        return NONE;
    }
    tail = remaining;
    let mut final_args = Vec::with_capacity(1 + stored_args.len() + tail.len());
    final_args.push(receiver);
    final_args.extend_from_slice(&stored_args);
    final_args.extend_from_slice(&tail);
    let roots = heap::root_values(&final_args);
    let result = call_with_keywords(func, &final_args, &keywords);
    heap::release_roots(roots);
    result
}

/// Fills the `Placeholder` holes of `stored` from `call_args`, returning the
/// merged stored arguments and the unconsumed call arguments.
fn merge_placeholders(stored: &[i64], call_args: &[i64]) -> (Vec<i64>, Vec<i64>) {
    let phcount = stored.iter().filter(|item| is_placeholder(**item)).count();
    if phcount == 0 {
        return (stored.to_vec(), call_args.to_vec());
    }
    if call_args.len() < phcount {
        exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "missing positional arguments in 'partialmethod' call; expected at least \
                 {phcount}, got {}",
                call_args.len()
            ),
        );
        return (Vec::new(), Vec::new());
    }
    let mut merged = Vec::with_capacity(stored.len());
    let mut next = 0usize;
    for item in stored {
        if is_placeholder(*item) {
            merged.push(call_args[next]);
            next += 1;
        } else {
            merged.push(*item);
        }
    }
    (merged, call_args[phcount..].to_vec())
}

/// Calls `target` with positional arguments and keyword pairs.
fn call_with_keywords(target: i64, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if keywords.is_empty() {
        return crate::core::builtins::call(target, args.len(), args.as_ptr());
    }
    let mut pairs: Vec<i64> = Vec::with_capacity(keywords.len() * 2);
    for (name, item) in keywords {
        pairs.push(value::str_alloc(name.clone()));
        pairs.push(*item);
    }
    crate::core::builtins::call_kw(
        target,
        args.len(),
        args.as_ptr(),
        keywords.len(),
        pairs.as_ptr(),
    )
}

/// `partialmethod.__repr__`.
fn partialmethod_repr(instance: i64) -> i64 {
    let func = attribute(instance, "func").unwrap_or(NONE);
    let args = attribute_tuple(instance, "args").unwrap_or_default();
    let keywords = attribute_pairs(instance, "keywords").unwrap_or_default();
    let mut rendered = String::from("functools.partialmethod(");
    rendered.push_str(&value::repr(func));
    for item in &args {
        rendered.push_str(", ");
        rendered.push_str(&value::repr(*item));
    }
    for (name, item) in &keywords {
        rendered.push_str(", ");
        rendered.push_str(name);
        rendered.push('=');
        rendered.push_str(&value::repr(*item));
    }
    rendered.push(')');
    value::str_alloc(rendered)
}

/// `partialmethod.__isabstractmethod__`: forwards the wrapped callable's flag.
fn partialmethod_isabstractmethod(instance: i64) -> i64 {
    let func = attribute(instance, "func").unwrap_or(NONE);
    if func == NONE {
        return value::FALSE;
    }
    let flag = crate::core::methods::getattr(
        func,
        b"__isabstractmethod__".as_ptr(),
        "__isabstractmethod__".len(),
    );
    if exc::pending() {
        exc::clear();
        return value::FALSE;
    }
    value::bool_value(value::truthy(flag))
}

// cached_property

/// The cached `functools.cached_property` class.
///
/// The state lives in instance attributes, exactly like the pure-Python
/// class: `func` is the wrapped getter, `attrname` the attribute name learned
/// from `__set_name__` (or recovered by [`cached_property_name`]), and
/// `__doc__`/`__module__`/`__name__` mirror the getter's metadata.
pub(crate) fn cached_property_class() -> i64 {
    if let Some(cached) = crate::core::globals::registry_get("functools.cached_property") {
        return cached;
    }
    let init = method_entry(
        "functools",
        "_cached_property_init",
        cached_property_init_entry,
        vec![
            self_parameter(),
            value::ParamMeta {
                name: "func".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
        ],
    );
    let get = method_entry(
        "functools",
        "_cached_property_get",
        cached_property_get_entry,
        vec![
            self_parameter(),
            value::ParamMeta {
                name: "instance".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
            value::ParamMeta {
                name: "owner".to_string(),
                kind: ParamKind::Positional,
                default: Some(NONE),
            },
        ],
    );
    let set_name = method_entry(
        "functools",
        "_cached_property_set_name",
        cached_property_set_name_entry,
        vec![
            self_parameter(),
            value::ParamMeta {
                name: "owner".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
            value::ParamMeta {
                name: "name".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
        ],
    );
    let class = value::class_new_full(
        "cached_property".to_string(),
        NONE,
        vec![
            ("__init__".to_string(), init, MethodKind::Normal),
            ("__get__".to_string(), get, MethodKind::Normal),
            ("__set_name__".to_string(), set_name, MethodKind::Normal),
        ],
        Vec::new(),
    );
    crate::core::globals::registry_set("functools.cached_property", class);
    class
}

/// `functools.cached_property(func)` through the module call path.
///
/// `CPython`'s `__init__(self, func)` also accepts `func` by keyword, so the
/// module entry binds the same name.
pub(crate) fn new_cached_property(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if args.len() > 1 {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "cached_property.__init__() takes 2 positional arguments ({} given)",
                1 + args.len()
            ),
        );
    }
    let mut func = args.first().copied();
    for (name, item) in keywords {
        if name != "func" {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("cached_property.__init__() got an unexpected keyword argument '{name}'"),
            );
        }
        if func.is_some() {
            return exc::raise_with_message(
                TYPE_ERROR,
                "cached_property.__init__() got multiple values for argument 'func'",
            );
        }
        func = Some(*item);
    }
    let Some(func) = func else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "cached_property.__init__() missing 1 required positional argument: 'func'",
        );
    };
    build_cached_property(func)
}

/// Allocates a `cached_property` descriptor for `func`.
fn build_cached_property(func: i64) -> i64 {
    let class = cached_property_class();
    let roots = heap::root_values(&[func, class]);
    let instance = value::instance_new(class);
    let instance_roots = heap::root_values(&[instance]);
    let result = initialize_cached_property(instance, func);
    heap::release_roots(instance_roots);
    heap::release_roots(roots);
    result
}

/// Stores CPython's `cached_property` state on `instance`.
fn initialize_cached_property(instance: i64, func: i64) -> i64 {
    let roots = heap::root_values(&[instance, func]);
    value::instance_setattr(instance, "func".to_string(), func);
    // `None` until `__set_name__` (or the owner-namespace lookup) names it.
    value::instance_setattr(instance, "attrname".to_string(), NONE);
    for name in ["__doc__", "__module__", "__name__"] {
        let metadata = cached_property_metadata(func, name);
        value::instance_setattr(instance, name.to_string(), metadata);
    }
    heap::release_roots(roots);
    instance
}

/// The getter metadata CPython copies in `cached_property.__init__`.
///
/// Hydro function values keep `__name__`/`__doc__`/`__module__` in their
/// metadata slots; other callables carry none of them, so the attribute is
/// `None` instead of raising from `__init__`.
fn cached_property_metadata(func: i64, name: &str) -> i64 {
    if value::as_function(func).is_none() {
        return NONE;
    }
    match name {
        "__doc__" => value::function_doc(func),
        "__module__" => value::function_module(func).unwrap_or(NONE),
        _ => value::str_alloc(value::function_name(func)),
    }
}

/// `cached_property.__init__(self, func)` through the class-call path.
extern "C" fn cached_property_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the class-call binder passes `(self, func)`.
    let instance = unsafe { *argv };
    let func = unsafe { *argv.add(1) };
    let roots = heap::root_values(&[instance, func]);
    let result = initialize_cached_property(instance, func);
    heap::release_roots(roots);
    result
}

/// `cached_property.__get__(self, instance, owner=None)`.
extern "C" fn cached_property_get_entry(argv: *const i64) -> i64 {
    // SAFETY: the descriptor binder passes `(self, instance, owner)`.
    let this = unsafe { *argv };
    let instance = unsafe { *argv.add(1) };
    let owner = unsafe { *argv.add(2) };
    cached_property_get(this, instance, owner)
}

fn cached_property_get(this: i64, instance: i64, owner: i64) -> i64 {
    // Class access returns the descriptor itself, exactly like CPython.
    if instance == NONE {
        return this;
    }
    let name = match cached_property_name(this, owner) {
        Ok(name) => name,
        Err(error) => return error,
    };
    let Some(class) = value::instance_class_value(instance) else {
        return cached_property_no_dict(instance, &name);
    };
    if !value::class_has_dict(class) {
        return cached_property_no_dict(instance, &name);
    }
    // The cache is the instance namespace: a hit is an ordinary instance
    // attribute, which is also why later reads never reach `__get__`.
    if let Some(cached) = value::instance_attr(instance, &name) {
        return cached;
    }
    let Some(func) = value::instance_attr(this, "func") else {
        return exc::raise_with_message(TYPE_ERROR, "cached_property instance has no 'func'");
    };
    let roots = heap::root_values(&[this, instance, func]);
    let computed = crate::core::builtins::call(func, 1, &instance);
    if !exc::pending() {
        let computed_roots = heap::root_values(&[computed]);
        value::instance_setattr(instance, name, computed);
        heap::release_roots(computed_roots);
    }
    heap::release_roots(roots);
    computed
}

/// CPython's error when the instance cannot hold the cache.
fn cached_property_no_dict(instance: i64, name: &str) -> i64 {
    exc::raise_with_message(
        TYPE_ERROR,
        &format!(
            "No '__dict__' attribute on '{}' instance to cache '{}' property.",
            value::type_name_of(instance),
            name
        ),
    )
}

/// The attribute name of the descriptor on `owner`.
///
/// `__set_name__` records the name when it runs. Hydro's class namespace is
/// built and frozen by `class_new_*` without a creation hook, so the hook
/// never runs for a class statement; the first `__get__` therefore recovers
/// the name from the owner's frozen namespace: the entry whose value is this
/// very descriptor. That is the name `type.__new__` would have passed to
/// `__set_name__`, so the cache key and the double-assignment `TypeError` stay
/// CPython's. Only the namespace frozen at class creation is scanned —
/// assigning a descriptor to a class afterwards does not run `__set_name__` in
/// CPython either, and stays the "without calling __set_name__" `TypeError`
/// here. The resolved name is written back to `attrname`, so the scan happens
/// once per descriptor.
fn cached_property_name(this: i64, owner: i64) -> Result<String, i64> {
    if let Some(name) = value::instance_attr(this, "attrname").and_then(value::as_str) {
        return Ok(name.to_string());
    }
    let mut names: Vec<String> = Vec::new();
    for class in value::class_mro(owner) {
        let Some(state) = value::as_class(class) else {
            continue;
        };
        for (name, item) in &state.attrs {
            if *item == this && !names.iter().any(|existing| existing == name) {
                names.push(name.clone());
            }
        }
    }
    match names.len() {
        0 => Err(exc::raise_with_message(
            TYPE_ERROR,
            "Cannot use cached_property instance without calling __set_name__ on it.",
        )),
        1 => {
            let name = names.remove(0);
            let roots = heap::root_values(&[this]);
            value::instance_setattr(this, "attrname".to_string(), value::str_alloc(name.clone()));
            heap::release_roots(roots);
            Ok(name)
        }
        // CPython rejects the second `__set_name__` instead of guessing.
        _ => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "Cannot assign the same cached_property to two different names ('{}' and '{}').",
                names[0], names[1]
            ),
        )),
    }
}

/// `cached_property.__set_name__(self, owner, name)`.
extern "C" fn cached_property_set_name_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `(self, owner, name)`.
    let this = unsafe { *argv };
    let name = unsafe { *argv.add(2) };
    cached_property_set_name(this, name)
}

fn cached_property_set_name(this: i64, name: i64) -> i64 {
    let Some(name_text) = value::as_str(name) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "cached_property attribute name must be a string, not {}",
                value::type_name(name)
            ),
        );
    };
    match value::instance_attr(this, "attrname").and_then(value::as_str) {
        None => {
            let roots = heap::root_values(&[this, name]);
            value::instance_setattr(this, "attrname".to_string(), name);
            heap::release_roots(roots);
            NONE
        }
        Some(existing) if existing == name_text => NONE,
        Some(existing) => exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "Cannot assign the same cached_property to two different names ('{existing}' and \
                 '{name_text}')."
            ),
        ),
    }
}

/// Dispatches the `functools` internals, the `partialmethod` class methods and
/// the native methods of `_singledispatchmethod_get`. Returns `None` for every
/// other function.
pub(crate) fn call(function: &str, args: &[i64]) -> Option<i64> {
    if let Some(method) = function.strip_prefix("_singledispatchmethod_get.") {
        return Some(match method {
            "__init__" => match args {
                [instance, unbound, obj, cls] => init_get(*instance, *unbound, *obj, *cls),
                _ => exc::raise_with_message(
                    TYPE_ERROR,
                    "_singledispatchmethod_get.__init__() takes 4 arguments",
                ),
            },
            "__repr__" => get_repr(args),
            _ => exc::raise_with_message(
                ATTRIBUTE_ERROR,
                &format!("'_singledispatchmethod_get' object has no attribute '{method}'"),
            ),
        });
    }
    Some(match function {
        "update_wrapper" => match args {
            [wrapper, wrapped] => update_wrapper(*wrapper, *wrapped, None, None),
            [wrapper, wrapped, assigned] => {
                update_wrapper(*wrapper, *wrapped, Some(*assigned), None)
            }
            [wrapper, wrapped, assigned, updated] => {
                update_wrapper(*wrapper, *wrapped, Some(*assigned), Some(*updated))
            }
            _ => exc::raise_with_message(TYPE_ERROR, "update_wrapper() takes 2 to 4 arguments"),
        },
        // `functools.wraps(...)` returns a partial of this helper that binds
        // `wrapped`, `assigned` and `updated`; the decorator's own argument —
        // the wrapper — is appended by the partial call, so it arrives last.
        "_wraps_apply" => match args {
            [wrapped, assigned, updated, wrapper] => {
                update_wrapper(*wrapper, *wrapped, Some(*assigned), Some(*updated))
            }
            _ => exc::raise_with_message(TYPE_ERROR, "_wraps_apply() takes 4 arguments"),
        },
        "_unwrap_partial" => unwrap_partial(args[0]),
        "_unwrap_partialmethod" => unwrap_partialmethod(args[0]),
        "_singledispatchmethod_get" => match args {
            [unbound, obj, cls] => new_get(*unbound, *obj, *cls),
            // `inspect` only needs the class value for `isinstance`; a direct
            // one-argument call resolves the underlying function, which is the
            // descriptor's `func`.
            [unbound] => descriptor_function(*unbound),
            _ => exc::raise_with_message(
                TYPE_ERROR,
                "_singledispatchmethod_get() takes 1 or 3 arguments",
            ),
        },
        "partialmethod" => new_partialmethod(args, &[]),
        "_partialmethod_init" => match args {
            [instance, func, args_value, kwargs_value] => {
                let rest: Vec<i64> = value::as_list(*args_value)
                    .map(|items| items.to_vec())
                    .unwrap_or_default();
                let keywords: Vec<(String, i64)> = value::as_dict(*kwargs_value)
                    .map(|pairs| {
                        pairs
                            .iter()
                            .filter_map(|(key, item)| {
                                value::as_str(*key).map(|name| (name.to_string(), *item))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                initialize_partialmethod(*instance, *func, &rest, &keywords)
            }
            _ => exc::raise_with_message(TYPE_ERROR, "_partialmethod_init() takes 4 arguments"),
        },
        "_partialmethod_repr" => partialmethod_repr(args[0]),
        "_partialmethod_get" => match args {
            [this, obj, cls] => {
                let slot_args = [*this, *obj, *cls];
                partialmethod_get_entry(slot_args.as_ptr())
            }
            _ => exc::raise_with_message(TYPE_ERROR, "__get__() takes 3 arguments"),
        },
        "_partialmethod_isabstractmethod" => partialmethod_isabstractmethod(args[0]),
        "_placeholder_repr" => value::str_alloc("Placeholder".to_string()),
        "cached_property" => new_cached_property(args, &[]),
        "_cached_property_init" => match args {
            [instance, func] => initialize_cached_property(*instance, *func),
            _ => exc::raise_with_message(TYPE_ERROR, "_cached_property_init() takes 2 arguments"),
        },
        "_cached_property_get" => match args {
            [this, obj, cls] => cached_property_get(*this, *obj, *cls),
            _ => exc::raise_with_message(TYPE_ERROR, "__get__() takes 3 arguments"),
        },
        "_cached_property_set_name" => match args {
            [this, _owner, name] => cached_property_set_name(*this, *name),
            _ => exc::raise_with_message(TYPE_ERROR, "__set_name__() takes 3 arguments"),
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> i64 {
        value::encode_int(n).expect("representable")
    }

    fn target() -> i64 {
        value::native_function_new("functools", "reduce", 3)
    }

    fn standin_partialmethod_class() -> i64 {
        value::class_new("partialmethod".to_string(), NONE, Vec::new())
    }

    fn descriptor(function: i64) -> i64 {
        let instance = value::instance_new(standin_partialmethod_class());
        value::instance_setattr(instance, "func".to_string(), function);
        instance
    }

    #[test]
    fn unwrap_partial_follows_the_whole_chain() {
        exc::init();
        let target = target();
        let once = value::partial_new(target, vec![int(1)]);
        let twice = value::partial_new(once, vec![int(2)]);
        assert_eq!(unwrap_partial(twice), target);
        assert_eq!(unwrap_partial(once), target);
        // A non-partial callable is returned unchanged.
        assert_eq!(unwrap_partial(target), target);
        assert!(!exc::pending());
    }

    #[test]
    fn unwrap_partialmethod_unwraps_representable_descriptors() {
        exc::init();
        let target = target();
        assert_eq!(unwrap_partialmethod(target), target);

        let partialmethod = descriptor(target);
        assert_eq!(unwrap_partialmethod(partialmethod), target);

        // The descriptor's `func` may itself be a partial chain.
        let nested = descriptor(value::partial_new(target, vec![int(9)]));
        assert_eq!(unwrap_partialmethod(nested), target);

        // A `__partialmethod__` back-reference is followed, like CPython.
        let marker = descriptor(target);
        let method = value::instance_new(partialmethod_class());
        value::instance_setattr(method, "func".to_string(), marker);
        value::instance_setattr(method, "__partialmethod__".to_string(), marker);
        assert_eq!(unwrap_partialmethod(method), target);
        assert!(!exc::pending());
    }

    #[test]
    fn singledispatchmethod_get_resolves_function_and_class() {
        exc::init();
        let target = target();
        let descriptor = descriptor(target);

        // One-argument form resolves the underlying function.
        assert_eq!(
            call("_singledispatchmethod_get", &[descriptor]).expect("helper"),
            target
        );

        // Three-argument form builds the descriptor wrapper instance.
        let instance =
            call("_singledispatchmethod_get", &[descriptor, NONE, NONE]).expect("helper");
        assert!(value::as_instance(instance).is_some());
        assert_eq!(
            value::instance_class_value(instance),
            Some(singledispatchmethod_get_class())
        );
        assert_eq!(value::instance_attr(instance, "_func"), Some(target));
        let rendered = call("_singledispatchmethod_get.__repr__", &[instance]).expect("repr");
        assert!(value::as_str(rendered)
            .expect("str")
            .contains("single dispatch method"));

        // A missing `func` attribute is an AttributeError, not a fake value.
        assert!(call("_singledispatchmethod_get", &[int(3)]).is_some());
        assert!(exc::pending());
        exc::init();
    }

    /// A wrapper callable reading `count` positional values into a tuple.
    fn capture_function(count: usize) -> i64 {
        extern "C" fn capture1(argv: *const i64) -> i64 {
            value::tuple_new(vec![unsafe { *argv }])
        }
        extern "C" fn capture2(argv: *const i64) -> i64 {
            value::tuple_new(vec![unsafe { *argv }, unsafe { *argv.add(1) }])
        }
        extern "C" fn capture3(argv: *const i64) -> i64 {
            value::tuple_new(vec![unsafe { *argv }, unsafe { *argv.add(1) }, unsafe {
                *argv.add(2)
            }])
        }
        extern "C" fn capture4(argv: *const i64) -> i64 {
            value::tuple_new(vec![
                unsafe { *argv },
                unsafe { *argv.add(1) },
                unsafe { *argv.add(2) },
                unsafe { *argv.add(3) },
            ])
        }
        let (entry, arity): (usize, usize) = match count {
            1 => (capture1 as *const () as usize, 1),
            2 => (capture2 as *const () as usize, 2),
            3 => (capture3 as *const () as usize, 3),
            4 => (capture4 as *const () as usize, 4),
            _ => panic!("unsupported capture arity"),
        };
        value::function_new(entry, arity, "f".to_string())
    }

    fn tuple_items(v: i64) -> Vec<i64> {
        value::as_tuple(v).expect("tuple").to_vec()
    }

    #[test]
    fn placeholder_is_a_singleton_of_its_type() {
        exc::init();
        let first = placeholder();
        assert_eq!(first, placeholder(), "singleton identity");
        assert_eq!(
            value::instance_class_value(first),
            Some(placeholder_type_class())
        );
        assert_eq!(
            value::as_str(call("_placeholder_repr", &[first]).expect("repr")),
            Some("Placeholder")
        );
        // `repr(Placeholder)` goes through `_PlaceholderType.__repr__`, which
        // declares a `self` parameter and a full-entry trampoline but has no
        // module spec: the class-method binder must run it directly.
        assert_eq!(value::repr(first), "Placeholder");
        assert!(!exc::pending(), "repr(Placeholder) raised");
        assert_eq!(constant("Placeholder"), first);
        assert_eq!(constant("_PlaceholderType"), placeholder_type_class());
        assert!(!exc::pending());
    }

    #[test]
    fn partialmethod_records_arguments_and_renders() {
        exc::init();
        let target = target();
        let pm = new_partialmethod(&[target, int(1), int(2)], &[]);
        assert_eq!(value::instance_class_value(pm), Some(partialmethod_class()));
        assert_eq!(value::instance_attr(pm, "func"), Some(target));
        assert_eq!(
            tuple_items(value::instance_attr(pm, "args").expect("args")),
            vec![int(1), int(2)]
        );
        assert_eq!(value::instance_attr(pm, "_phcount"), Some(int(0)));
        let rendered =
            value::as_str(call("_partialmethod_repr", &[pm]).expect("repr")).expect("str");
        assert!(
            rendered.starts_with("functools.partialmethod("),
            "{rendered}"
        );
        assert!(
            rendered.contains('1') && rendered.contains('2'),
            "{rendered}"
        );

        // Trailing placeholders and Placeholder keywords are rejected.
        exc::init();
        new_partialmethod(&[target, placeholder()], &[]);
        assert!(exc::pending());
        assert!(value::to_string(exc::pending_value()).contains("trailing Placeholders"));
        exc::init();
        new_partialmethod(&[target], &[("x".to_string(), placeholder())]);
        assert!(exc::pending());
        assert!(value::to_string(exc::pending_value()).contains("Placeholder cannot be passed"));
        exc::clear();
    }

    #[test]
    fn partialmethod_get_binds_and_merges_placeholders() {
        exc::init();
        let target = capture_function(3);
        let pm = new_partialmethod(&[target, int(10)], &[]);
        let instance = value::instance_new(standin_partialmethod_class());
        let bound = call("_partialmethod_get", &[pm, instance, NONE]).expect("__get__");
        assert!(!exc::pending());
        let result = crate::core::builtins::call(bound, 1, [int(20)].as_ptr());
        assert!(!exc::pending());
        let items = tuple_items(result);
        assert_eq!(items, vec![instance, int(10), int(20)]);

        // Placeholder holes consume the call's leading arguments.
        exc::init();
        let target = capture_function(4);
        let pm = new_partialmethod(&[target, placeholder(), int(2)], &[]);
        let bound = call("_partialmethod_get", &[pm, instance, NONE]).expect("__get__");
        let args = [int(7), int(9)];
        let result = crate::core::builtins::call(bound, args.len(), args.as_ptr());
        assert!(!exc::pending());
        assert_eq!(tuple_items(result), vec![instance, int(7), int(2), int(9)]);

        // Too few call arguments for the placeholders is a TypeError.
        exc::init();
        let bound = call("_partialmethod_get", &[pm, instance, NONE]).expect("__get__");
        crate::core::builtins::call(bound, 0, std::ptr::null());
        assert!(exc::pending());
        assert!(value::to_string(exc::pending_value()).contains("missing positional arguments"));
        exc::clear();
    }

    #[test]
    fn partialmethod_class_access_requires_a_receiver() {
        exc::init();
        let target = capture_function(3);
        let pm = new_partialmethod(&[target, int(1), int(2)], &[]);
        // Class access returns the unbound callable: the first call argument
        // becomes the receiver.
        let unbound = call("_partialmethod_get", &[pm, NONE, NONE]).expect("__get__");
        let receiver = value::instance_new(standin_partialmethod_class());
        let result = crate::core::builtins::call(unbound, 1, [receiver].as_ptr());
        assert!(!exc::pending());
        assert_eq!(tuple_items(result), vec![receiver, int(1), int(2)]);

        exc::init();
        crate::core::builtins::call(unbound, 0, std::ptr::null());
        assert!(exc::pending());
        assert!(value::to_string(exc::pending_value()).contains("cls_or_self"));
        exc::clear();
    }

    #[test]
    fn nested_partialmethods_flatten_like_cpython() {
        exc::init();
        let target = target();
        let inner = new_partialmethod(&[target, int(1)], &[("a".to_string(), int(5))]);
        let outer = new_partialmethod(&[inner, int(2)], &[("b".to_string(), int(6))]);
        assert_eq!(value::instance_attr(outer, "func"), Some(target));
        assert_eq!(
            tuple_items(value::instance_attr(outer, "args").expect("args")),
            vec![int(1), int(2)]
        );
        let keywords = value::instance_attr(outer, "keywords").expect("keywords");
        let pairs = value::as_dict(keywords).expect("dict");
        assert_eq!(pairs.len(), 2);
        assert!(pairs
            .iter()
            .any(|(key, item)| value::as_str(*key) == Some("a") && *item == int(5)));
        assert!(pairs
            .iter()
            .any(|(key, item)| value::as_str(*key) == Some("b") && *item == int(6)));
        assert!(!exc::pending());
    }

    #[test]
    fn partialmethods_reject_non_callables() {
        exc::init();
        new_partialmethod(&[int(3)], &[]);
        assert!(exc::pending());
        assert!(value::to_string(exc::pending_value()).contains("callable or a descriptor"));
        exc::clear();
    }

    fn text(content: &str) -> i64 {
        value::str_alloc(content.to_string())
    }

    fn attribute(obj: i64, name: &str) -> i64 {
        let result = crate::core::methods::getattr(obj, name.as_ptr(), name.len());
        assert!(!exc::pending(), "reading {name} raised");
        result
    }

    fn set_attribute(obj: i64, name: &str, item: i64) {
        crate::core::methods::setattr(obj, name.as_ptr(), name.len(), item);
        assert!(!exc::pending(), "setting {name} raised");
    }

    fn name_tuple(names: &[&str]) -> i64 {
        let items: Vec<i64> = names.iter().map(|name| text(name)).collect();
        let roots = heap::root_values(&items);
        let tuple = value::tuple_new(items);
        heap::release_roots(roots);
        tuple
    }

    #[test]
    fn update_wrapper_copies_metadata_and_records_wrapped() {
        exc::init();
        let wrapped = value::identity_function_new("wrapped");
        let wrapper = value::identity_function_new("wrapper");
        set_attribute(wrapped, "__doc__", text("wrapped docs"));
        set_attribute(wrapped, "__module__", text("pkg.mod"));
        set_attribute(wrapped, "__annotations__", value::dict_new(Vec::new()));
        set_attribute(wrapped, "custom", int(7));

        assert_eq!(call("update_wrapper", &[wrapper, wrapped]), Some(wrapper));
        assert!(!exc::pending());

        assert_eq!(
            value::as_str(attribute(wrapper, "__name__")),
            Some("wrapped")
        );
        assert_eq!(
            value::as_str(attribute(wrapper, "__qualname__")),
            Some("wrapped")
        );
        assert_eq!(
            value::as_str(attribute(wrapper, "__doc__")),
            Some("wrapped docs")
        );
        assert_eq!(
            value::as_str(attribute(wrapper, "__module__")),
            Some("pkg.mod")
        );
        assert!(value::as_dict(attribute(wrapper, "__annotations__")).is_some());
        // The wrapped function's `__dict__` entries move to the wrapper; the
        // `__wrapped__` back-reference lives there too.
        assert_eq!(attribute(wrapper, "custom"), int(7));
        let dictionary = value::as_dict(attribute(wrapper, "__dict__"))
            .expect("wrapper __dict__")
            .to_vec();
        assert_eq!(dictionary.len(), 2);
        assert!(dictionary
            .iter()
            .any(|(key, _)| value::as_str(*key) == Some("custom")));
        assert!(dictionary
            .iter()
            .any(|(key, _)| value::as_str(*key) == Some("__wrapped__")));
        // `__wrapped__` is the back-reference `inspect.unwrap` follows.
        assert_eq!(attribute(wrapper, "__wrapped__"), wrapped);
        // The wrapped function itself is untouched.
        assert_eq!(
            value::as_str(attribute(wrapped, "__name__")),
            Some("wrapped")
        );
    }

    #[test]
    fn update_wrapper_skips_metadata_the_wrapped_callable_lacks() {
        exc::init();
        let wrapped = value::identity_function_new("bare");
        let wrapper = value::identity_function_new("wrapper");
        // Hydro retains no module or annotation payload on function values, so
        // those slots are absent and `update_wrapper` must skip them instead of
        // fabricating a value; the wrapper keeps its own assignment.
        set_attribute(wrapper, "__module__", text("own.module"));

        call("update_wrapper", &[wrapper, wrapped]).expect("update_wrapper");
        assert!(!exc::pending());
        assert_eq!(
            value::as_str(attribute(wrapper, "__module__")),
            Some("own.module")
        );

        // A slot the wrapped function never exposes stays absent.
        crate::core::methods::getattr(
            wrapper,
            b"__annotations__".as_ptr(),
            "__annotations__".len(),
        );
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        exc::clear();
        // `__doc__` exists on every function and is `None` for the bare one.
        assert_eq!(attribute(wrapper, "__doc__"), NONE);
        assert_eq!(attribute(wrapper, "__wrapped__"), wrapped);
    }

    #[test]
    fn wraps_decorator_applies_update_wrapper_and_wraps_partials() {
        exc::init();
        let wrapped = value::identity_function_new("wrapped");
        set_attribute(wrapped, "__doc__", text("docs"));

        let decorator = wraps_decorator(wrapped, None, None);
        // The decorator is a partial of the internal apply helper, so the
        // wrapped callable is a traced edge of the decorator value.
        let apply = unwrap_partial(decorator);
        assert_eq!(
            value::as_function(apply).map(|function| function.name.as_str()),
            Some("_wraps_apply")
        );

        let wrapper = value::identity_function_new("wrapper");
        let argv = [wrapper];
        let applied = crate::core::builtins::call(decorator, 1, argv.as_ptr());
        assert!(!exc::pending());
        assert_eq!(applied, wrapper);
        assert_eq!(
            value::as_str(attribute(wrapper, "__name__")),
            Some("wrapped")
        );
        assert_eq!(value::as_str(attribute(wrapper, "__doc__")), Some("docs"));
        assert_eq!(attribute(wrapper, "__wrapped__"), wrapped);

        // A `functools.partial` is a valid wrapper: its own attributes stay
        // readable and the metadata is attached to it.
        let function = target();
        let partial = value::partial_new(function, vec![int(1)]);
        let argv = [partial];
        assert_eq!(
            crate::core::builtins::call(decorator, 1, argv.as_ptr()),
            partial
        );
        assert!(!exc::pending());
        assert_eq!(attribute(partial, "__wrapped__"), wrapped);
        assert_eq!(attribute(partial, "func"), function);
        assert_eq!(
            value::as_tuple(attribute(partial, "args")).map(|items| items.to_vec()),
            Some(vec![int(1)])
        );
    }

    #[test]
    fn update_wrapper_honours_custom_assigned_and_updated() {
        exc::init();
        let wrapped = value::identity_function_new("wrapped");
        let wrapper = value::identity_function_new("wrapper");
        set_attribute(wrapped, "__doc__", text("wrapped docs"));
        set_attribute(wrapper, "__doc__", text("wrapper docs"));
        let wrapped_extra = value::dict_new(vec![(text("a"), int(1))]);
        let wrapper_extra = value::dict_new(vec![(text("b"), int(2))]);
        set_attribute(wrapped, "extra", wrapped_extra);
        set_attribute(wrapper, "extra", wrapper_extra);

        let assigned = name_tuple(&["__name__"]);
        let updated = name_tuple(&["extra"]);
        call("update_wrapper", &[wrapper, wrapped, assigned, updated]).expect("update_wrapper");
        assert!(!exc::pending());

        // Only the listed assigned slot is copied.
        assert_eq!(
            value::as_str(attribute(wrapper, "__name__")),
            Some("wrapped")
        );
        assert_eq!(
            value::as_str(attribute(wrapper, "__doc__")),
            Some("wrapper docs")
        );
        // The listed updated mapping merges the wrapped entries into the
        // wrapper's own mapping.
        let extra = attribute(wrapper, "extra");
        assert_eq!(extra, wrapper_extra);
        let pairs = value::as_dict(extra).expect("dict");
        assert_eq!(pairs.len(), 2);
        assert_eq!(
            pairs
                .iter()
                .find_map(|(key, item)| (value::as_str(*key) == Some("a")).then_some(*item)),
            Some(int(1))
        );
        assert_eq!(attribute(wrapper, "__wrapped__"), wrapped);
        // The `__dict__` update is skipped when it is not requested.
        assert!(value::function_attribute(wrapper, "__wrapped__").is_some());
        assert_eq!(value::function_attribute(wrapper, "unrelated"), None);

        // An empty `updated` skips the dictionary update entirely.
        let second = value::identity_function_new("second");
        let source = value::identity_function_new("source");
        set_attribute(source, "custom", int(3));
        call(
            "update_wrapper",
            &[second, source, name_tuple(&["__name__"]), name_tuple(&[])],
        )
        .expect("update_wrapper");
        assert!(!exc::pending());
        assert_eq!(value::as_str(attribute(second, "__name__")), Some("source"));
        assert_eq!(value::function_attribute(second, "custom"), None);

        // `wraps` forwards the same custom lists through its decorator.
        let via_wraps = value::identity_function_new("via wraps");
        let own_extra = value::dict_new(vec![(text("own"), int(4))]);
        set_attribute(via_wraps, "extra", own_extra);
        let argv = [wrapped, name_tuple(&["__name__"]), name_tuple(&[])];
        let decorator =
            crate::modules::call_native("functools", "wraps", argv.len(), argv.as_ptr());
        assert!(!exc::pending());
        // The decorator binds `wrapped`/`assigned`/`updated` on a partial of
        // the apply helper, so applying it only supplies the wrapper.
        assert_eq!(
            value::as_function(unwrap_partial(decorator)).map(|function| function.name.as_str()),
            Some("_wraps_apply")
        );
        let argv = [via_wraps];
        assert_eq!(
            crate::core::builtins::call(decorator, 1, argv.as_ptr()),
            via_wraps
        );
        assert!(!exc::pending());
        assert_eq!(
            value::as_str(attribute(via_wraps, "__name__")),
            Some("wrapped")
        );
        assert_eq!(value::as_str(attribute(via_wraps, "__doc__")), None);
        // The empty `updated` list leaves the wrapper's own mapping alone.
        assert_eq!(attribute(via_wraps, "extra"), own_extra);
        assert_eq!(
            value::as_dict(attribute(via_wraps, "extra")).map(|pairs| pairs.len()),
            Some(1)
        );
        assert_eq!(attribute(via_wraps, "__wrapped__"), wrapped);

        // Codegen pads an omitted keyword slot with `None`, the compiled form
        // of `wraps(f, updated=("extra",))`; the default `assigned` list still
        // applies while the custom update list is forwarded.
        let merged = value::identity_function_new("merged");
        let merged_extra = value::dict_new(vec![(text("own"), int(5))]);
        set_attribute(merged, "extra", merged_extra);
        let argv = [wrapped, NONE, name_tuple(&["extra"])];
        let decorator =
            crate::modules::call_native("functools", "wraps", argv.len(), argv.as_ptr());
        assert!(!exc::pending());
        let argv = [merged];
        assert_eq!(
            crate::core::builtins::call(decorator, 1, argv.as_ptr()),
            merged
        );
        assert!(!exc::pending());
        assert_eq!(
            value::as_str(attribute(merged, "__doc__")),
            Some("wrapped docs")
        );
        assert_eq!(
            value::as_str(attribute(merged, "__name__")),
            Some("wrapped")
        );
        // The custom update runs in place on the wrapper's own mapping.
        assert_eq!(attribute(merged, "extra"), merged_extra);
        assert_eq!(
            value::as_dict(merged_extra).map(|pairs| pairs.len()),
            Some(2)
        );
        assert_eq!(attribute(merged, "__wrapped__"), wrapped);
        assert!(!exc::pending());
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts how many times the `cached_property` getter ran.
    static CACHED_PROPERTY_CALLS: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn cached_property_getter_entry(_argv: *const i64) -> i64 {
        let calls = CACHED_PROPERTY_CALLS.fetch_add(1, Ordering::Relaxed);
        int(calls as i64)
    }

    /// A getter reporting how many times it has been called. Resets the
    /// counter, so a test observes only its own calls.
    fn counting_getter() -> i64 {
        CACHED_PROPERTY_CALLS.store(0, Ordering::Relaxed);
        value::function_new(
            cached_property_getter_entry as *const () as usize,
            1,
            "value".to_string(),
        )
    }

    /// `class Holder: value = <descriptor>`.
    fn holder_class(descriptor: i64) -> i64 {
        value::class_new_full(
            "Holder".to_string(),
            NONE,
            Vec::new(),
            vec![("value".to_string(), descriptor)],
        )
    }

    #[test]
    fn cached_property_caches_per_instance_in_the_instance_namespace() {
        exc::init();
        let descriptor = build_cached_property(counting_getter());
        assert_eq!(
            value::instance_class_name(descriptor),
            Some("cached_property")
        );
        assert_eq!(value::instance_attr(descriptor, "attrname"), Some(NONE));

        let class = holder_class(descriptor);
        // Class access returns the descriptor itself and never runs the
        // getter.
        assert_eq!(attribute(class, "value"), descriptor);
        assert_eq!(CACHED_PROPERTY_CALLS.load(Ordering::Relaxed), 0);

        let instance = value::instance_new(class);
        let first = attribute(instance, "value");
        assert_eq!(value::as_integer(first), Some(0));
        assert_eq!(CACHED_PROPERTY_CALLS.load(Ordering::Relaxed), 1);
        // The cache is an ordinary instance attribute under the attribute
        // name, so a later read never reaches `__get__`.
        assert_eq!(value::instance_attr(instance, "value"), Some(first));
        assert_eq!(attribute(instance, "value"), first);
        assert_eq!(CACHED_PROPERTY_CALLS.load(Ordering::Relaxed), 1);
        // The descriptor records the recovered name, like `__set_name__`.
        assert_eq!(
            value::instance_attr(descriptor, "attrname").and_then(value::as_str),
            Some("value")
        );
        // A second instance computes its own value.
        let other = value::instance_new(class);
        assert_eq!(value::as_integer(attribute(other, "value")), Some(1));
        assert_eq!(CACHED_PROPERTY_CALLS.load(Ordering::Relaxed), 2);
        assert!(!exc::pending());
    }

    #[test]
    fn cached_property_set_name_records_and_rejects_a_second_name() {
        exc::init();
        let descriptor = build_cached_property(counting_getter());
        assert_eq!(
            call(
                "_cached_property_set_name",
                &[descriptor, NONE, text("value")]
            ),
            Some(NONE)
        );
        assert_eq!(
            value::instance_attr(descriptor, "attrname").and_then(value::as_str),
            Some("value")
        );
        // Repeating the same name is idempotent.
        assert_eq!(
            call(
                "_cached_property_set_name",
                &[descriptor, NONE, text("value")]
            ),
            Some(NONE)
        );
        assert!(!exc::pending());
        // A second, different name is CPython's TypeError.
        call(
            "_cached_property_set_name",
            &[descriptor, NONE, text("other")],
        );
        assert!(exc::pending());
        assert!(
            value::to_string(exc::pending_value()).contains("two different names"),
            "{}",
            value::to_string(exc::pending_value())
        );
        exc::clear();
        // A non-string name cannot be recorded.
        call("_cached_property_set_name", &[descriptor, NONE, int(3)]);
        assert!(exc::pending());
        assert!(value::to_string(exc::pending_value()).contains("must be a string"));
        exc::clear();
    }

    #[test]
    fn cached_property_reports_the_cpython_errors() {
        // A descriptor unreachable from the owner namespace has no name.
        exc::init();
        let descriptor = build_cached_property(counting_getter());
        let class = value::class_new("Holder".to_string(), NONE, Vec::new());
        let instance = value::instance_new(class);
        cached_property_get(descriptor, instance, class);
        assert!(exc::pending());
        assert!(
            value::to_string(exc::pending_value()).contains("without calling __set_name__"),
            "{}",
            value::to_string(exc::pending_value())
        );
        exc::clear();

        // Two attribute names for the same descriptor are rejected, like
        // CPython's second `__set_name__` call.
        let descriptor = build_cached_property(counting_getter());
        let class = value::class_new_full(
            "Holder".to_string(),
            NONE,
            Vec::new(),
            vec![
                ("first".to_string(), descriptor),
                ("second".to_string(), descriptor),
            ],
        );
        let instance = value::instance_new(class);
        cached_property_get(descriptor, instance, class);
        assert!(exc::pending());
        assert!(value::to_string(exc::pending_value()).contains("two different names"));
        exc::clear();

        // A slots-only class has no instance `__dict__` to cache into.
        let descriptor = build_cached_property(counting_getter());
        let class = value::class_new_full(
            "Slotted".to_string(),
            NONE,
            Vec::new(),
            vec![
                ("__slots__".to_string(), text("value")),
                ("value".to_string(), descriptor),
            ],
        );
        let instance = value::instance_new(class);
        crate::core::methods::getattr(instance, b"value".as_ptr(), 5);
        assert!(exc::pending());
        assert_eq!(
            value::to_string(exc::pending_value()),
            "No '__dict__' attribute on 'Slotted' instance to cache 'value' property."
        );
        exc::clear();
        assert_eq!(CACHED_PROPERTY_CALLS.load(Ordering::Relaxed), 0);
        assert!(!exc::pending());
    }

    #[test]
    fn cached_property_constructor_matches_cpython_metadata() {
        exc::init();
        let getter = value::identity_function_new("value");
        set_attribute(getter, "__doc__", text("doc"));
        set_attribute(getter, "__module__", text("pkg.mod"));
        let descriptor = call("cached_property", &[getter]).expect("cached_property");
        assert_eq!(
            value::instance_class_name(descriptor),
            Some("cached_property")
        );
        assert_eq!(value::instance_attr(descriptor, "func"), Some(getter));
        assert_eq!(value::as_str(attribute(descriptor, "__doc__")), Some("doc"));
        assert_eq!(
            value::as_str(attribute(descriptor, "__module__")),
            Some("pkg.mod")
        );
        assert_eq!(
            value::as_str(attribute(descriptor, "__name__")),
            Some("value")
        );
        assert_eq!(value::instance_attr(descriptor, "attrname"), Some(NONE));
        assert!(!exc::pending());

        // The keyword form binds the same slot.
        let keyword = new_cached_property(&[], &[("func".to_string(), getter)]);
        assert_eq!(value::instance_attr(keyword, "func"), Some(getter));
        assert!(!exc::pending());

        // A missing getter and unexpected/duplicate keywords are TypeErrors
        // with CPython's `__init__` messages.
        call("cached_property", &[]);
        assert!(exc::pending());
        assert!(
            value::to_string(exc::pending_value())
                .contains("missing 1 required positional argument: 'func'"),
            "{}",
            value::to_string(exc::pending_value())
        );
        exc::clear();
        new_cached_property(&[getter], &[("func".to_string(), getter)]);
        assert!(exc::pending());
        assert!(value::to_string(exc::pending_value()).contains("multiple values"));
        exc::clear();
        new_cached_property(&[getter], &[("other".to_string(), getter)]);
        assert!(exc::pending());
        assert!(value::to_string(exc::pending_value()).contains("unexpected keyword argument"));
        exc::clear();
    }

    #[test]
    fn wrapper_constants_match_cpython_defaults() {
        exc::init();
        let assigned: Vec<String> = tuple_items(constant("WRAPPER_ASSIGNMENTS"))
            .into_iter()
            .map(|item| value::as_str(item).expect("string").to_string())
            .collect();
        assert_eq!(
            assigned,
            vec![
                "__module__",
                "__name__",
                "__qualname__",
                "__doc__",
                "__annotations__"
            ]
        );
        assert_eq!(
            tuple_items(constant("WRAPPER_UPDATES"))
                .into_iter()
                .map(|item| value::as_str(item).expect("string").to_string())
                .collect::<Vec<String>>(),
            vec!["__dict__"]
        );
        assert!(!exc::pending());
    }
}
