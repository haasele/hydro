//! Pure-Python `functools` internals used by `inspect`, plus the real
//! `partialmethod` descriptor and `Placeholder` singleton.
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
}
