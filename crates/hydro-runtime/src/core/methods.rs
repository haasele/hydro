//! Dynamic method dispatch for built-in types (ARCHITECTURE §3.2).
//!
//! `hydro_method` resolves a method by receiver kind and name;
//! [`call_kw`] handles the keyword variant used by `hydro_method_kw`. Every
//! entry point is total: malformed name buffers, missing argument buffers and
//! every user-level misuse report a pending exception and return `None`; the
//! runtime never panics on user input.
//!
//! Details that differ from CPython are documented here: `dict.keys()`,
//! `dict.values()` and `dict.items()` return live views (`dict_keys`,
//! `dict_values`, `dict_items`) that observe later dict mutations, so
//! `len(view)`, iteration, membership, set operators and `repr` always reflect
//! the current contents. `dict_items` iteration yields `(key, value)` tuples.
//! `bytearray` and `memoryview` expose CPython's mutable sequence and buffer
//! surfaces; unsupported buffer formats report a tagged error rather than a
//! placeholder.
//!
//! Class instances follow Python's descriptor precedence (data descriptor,
//! instance dictionary, non-data descriptor/method), class namespaces accept
//! runtime assignment, and `__slots__` restricts instance attributes.
//!
//! Generator objects expose `send`, `throw` and `close`. `throw` accepts an
//! exception *instance* only (Hydro has no exception class objects) and supports
//! just the single-argument form.
//!
//! `int` is a builtin function value at runtime rather than a class object, so
//! its class-level methods are dispatched from the same entry points:
//! `int.from_bytes(data, byteorder='big', *, signed=False)` on the type value
//! (also callable unbound) and `int.to_bytes(length=1, byteorder='big', *,
//! signed=False)`/`int.bit_length()` as instance methods. They follow CPython's
//! `byteorder`, sign-extension and overflow rules; `from_bytes` keeps arbitrary
//! precision through the heap `BigInt` path.

use std::io::{BufRead as _, IsTerminal, Read as _, Write as _};
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::builtins;
use crate::container;
use crate::exc::{
    self, ATTRIBUTE_ERROR, INDEX_ERROR, KEY_ERROR, OS_ERROR, OVERFLOW_ERROR, RUNTIME_ERROR,
    TYPE_ERROR, VALUE_ERROR,
};
use crate::modules;
use crate::ops;
use crate::pool;
use crate::process;
use crate::value::{self, NONE};
use hydro_ir::{MethodKind, ParamKind};
use num_bigint::{BigInt, Sign};

/// Dispatches `obj.<name>(args...)`.
pub(crate) fn call(
    obj: i64,
    name: *const u8,
    name_len: usize,
    argc: usize,
    argv: *const i64,
) -> i64 {
    let Some(method) = decode_name(name, name_len) else {
        return attribute_error(obj, "");
    };
    // Template values expose only `Interpolation.convert()`.
    if let Some(result) = crate::core::template::call_method(obj, &method, argc, argv) {
        return result;
    }
    if let Some(module) = value::as_module(obj) {
        let function = modules::module_get_by_name(module.name(), &method);
        if exc::pending() {
            return NONE;
        }
        return builtins::call(function, argc, argv);
    }
    if method == "$super_init" {
        let args = if argc == 0 {
            Vec::new()
        } else if argv.is_null() {
            return type_error(
                "super().__init__() received arguments without an argument buffer".to_string(),
            );
        } else {
            // SAFETY: the ABI requires `argv..argv+argc` to be readable.
            unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec()
        };
        return super_init(obj, &args, &[]);
    }
    // `self.$super_new([class])` materialises a `super` proxy for the runtime
    // (zero-argument `super()` cannot name its class without frame context).
    if method == "$super_new" {
        return super_proxy(obj, argc, argv);
    }
    // Instances (including user-defined exception instances) bind `self`
    // while collecting the arguments, so they skip the generic copy below
    // (one allocation instead of two).
    if value::instance_class_value(obj).is_some() {
        return instance_method(obj, &method, argc, argv);
    }
    // `Class.method(...)` resolves through the class namespace; the binding
    // kind decides whether the class or no receiver is prepended.
    if value::as_class(obj).is_some() {
        return class_member_method(obj, &method, argc, argv);
    }
    let Some(args) = copy_args(argc, argv) else {
        return type_error(format!(
            "{}.{}() received {} argument(s) without an argument buffer",
            value::type_name(obj),
            method,
            argc
        ));
    };
    // argparse group/subparsers handles are namespaces with native methods.
    if crate::modules::argparse::is_handle(obj) {
        return crate::modules::argparse::handle_method(obj, &method, &args);
    }
    // Generators expose `send`, `throw` and `close`; async generators expose
    // the `__aiter__`/`__anext__`/`asend`/`athrow`/`aclose` protocol; tasks
    // (coroutine fibers) expose `cancel` and the result probes.
    if let Some(kind) = value::as_fiber(obj).map(|fiber| fiber.kind) {
        return match kind {
            crate::fiber::FiberKind::Generator => generator_method(obj, &method, &args),
            crate::fiber::FiberKind::AsyncGenerator => async_generator_method(obj, &method, &args),
            crate::fiber::FiberKind::Coroutine => coroutine_method(obj, &method, &args),
        };
    }
    if let Some(s) = value::as_str(obj) {
        // Copy the receiver before any allocation: a reference into a heap
        // object must not be read after an allocation that may collect.
        return str_method(obj, s.to_string(), &method, &args);
    }
    // Checked before `as_bytes`, which also accepts a `bytearray`.
    if value::as_bytearray(obj).is_some() {
        return bytearray_method(obj, &method, &args);
    }
    if value::as_memoryview(obj).is_some() {
        return memoryview_method(obj, &method, &args);
    }
    if value::as_dict_view(obj).is_some() {
        return dict_view_method(obj, &method, &args);
    }
    if value::as_super(obj).is_some() {
        return super_method(obj, &method, &args);
    }
    if value::as_descriptor(obj).is_some() {
        return descriptor_value_method(obj, &method, &args);
    }
    if let Some(bytes) = value::as_bytes(obj) {
        // Same rule as for strings: copy the payload out first.
        return bytes_method(obj, bytes.to_vec(), &method, &args);
    }
    if value::as_list(obj).is_some() {
        return list_method(obj, &method, &args);
    }
    if value::as_dict(obj).is_some() {
        return dict_method(obj, &method, &args);
    }
    if value::as_tuple(obj).is_some() {
        return tuple_method(obj, &method, &args);
    }
    if value::as_set(obj).is_some() {
        return set_method(obj, &method, &args);
    }
    if value::as_lock(obj).is_some() {
        return lock_method(obj, &method, &args);
    }
    if value::as_thread(obj).is_some() {
        return thread_method(obj, &method, &args);
    }
    if value::as_event(obj).is_some() {
        return event_method(obj, &method, &args);
    }
    if value::as_queue(obj).is_some() {
        return queue_method(obj, &method, &args);
    }
    if value::as_process(obj).is_some() {
        return process_method(obj, &method, &args);
    }
    if value::as_mp_queue(obj).is_some() {
        return mp_queue_method(obj, &method, &args);
    }
    if value::as_pool(obj).is_some() {
        return pool_method(obj, &method, &args);
    }
    if value::as_csv_writer(obj).is_some() {
        return crate::csv::writer_method(obj, &method, &args);
    }
    if value::as_file(obj).is_some() {
        return file_method(obj, &method, &args);
    }
    if value::as_std_stream(obj).is_some() {
        return std_stream_method(obj, &method, &args);
    }
    if value::as_random(obj).is_some() {
        return modules::random_method(obj, &method, &args);
    }
    if value::as_path(obj).is_some() {
        return path_method(obj, &method, &args);
    }
    if value::as_arg_parser(obj).is_some() {
        return crate::modules::argparse::method(obj, &method, &args);
    }
    if value::as_defaultdict(obj).is_some() {
        return crate::modules::collections::method(obj, &method, &args);
    }
    if value::as_pattern(obj).is_some() {
        return crate::modules::re::pattern_method(obj, &method, &args);
    }
    if value::as_match(obj).is_some() {
        return crate::modules::re::match_method(obj, &method, &args);
    }
    if value::as_socket(obj).is_some() {
        return crate::modules::socket::method(obj, &method, &args);
    }
    // The `int` builtin type value is a function object at runtime, so
    // `int.from_bytes(...)` arrives here with the type value as the receiver.
    if is_builtin_int_type(obj) {
        return int_class_method(obj, &method, &args, &[]);
    }
    if is_integer_value(obj) {
        return int_method(obj, &method, &args, &[]);
    }
    attribute_error(obj, &method)
}

/// Implements the lowered `super().__init__(...)` form.
///
/// Python resolves the initializer on the class after the one that defines the
/// current method. Without runtime frame context the runtime can only start
/// after the receiver's class, which is the same position for the common
/// `__init__` case and, with the C3 linearization, now follows the full MRO
/// (so diamond hierarchies reach the shared ancestor exactly once).
fn super_init(obj: i64, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let Some(class) = value::instance_class_value(obj) else {
        return type_error("super(): __self__ is not a class instance".to_string());
    };
    let Some(entry) = value::class_lookup_after(class, class, "__init__") else {
        return NONE;
    };
    if let value::ClassEntry::Method(_, function) = &entry {
        if let Some(function) = value::as_function(*function) {
            if let value::FunctionBody::Native { module, name } = &function.body {
                if *module == "threading" && *name == "_threadbase_init" {
                    return NONE;
                }
            }
        }
    }
    call_bound_entry(obj, class, entry, "__init__", args, keywords)
}

/// `self.$super_new([class])`: materialises a `super` proxy.
///
/// Zero-argument `super()` cannot name its defining class without frame
/// context, so the proxy starts after the receiver's class. An explicit class
/// argument is validated against the receiver's linearization.
fn super_proxy(obj: i64, argc: usize, argv: *const i64) -> i64 {
    // `__self__` may be an instance (ordinary method) or a class object
    // (`classmethod` calling `super()`), matching CPython's second argument.
    let start = match value::instance_class_value(obj) {
        Some(class) => class,
        None if value::as_class(obj).is_some() => obj,
        None => return type_error("super(): no arguments".to_string()),
    };
    let mut class = start;
    if argc > 0 {
        if argv.is_null() || argc > isize::MAX as usize / std::mem::size_of::<i64>() {
            return type_error("super() received arguments without an argument buffer".to_string());
        }
        // SAFETY: ABI contract: `argv` points at `argc` readable values.
        let args = unsafe { std::slice::from_raw_parts(argv, argc) };
        if args.len() != 1 {
            return type_error("super() takes at most 1 argument here".to_string());
        }
        let explicit = args[0];
        if value::as_class(explicit).is_none() {
            return type_error(format!(
                "super() argument 1 must be a type, not '{}'",
                value::type_name(explicit)
            ));
        }
        if !value::class_mro(start).contains(&explicit) {
            return type_error(
                "super(type, obj): obj must be an instance or subtype of type".to_string(),
            );
        }
        class = explicit;
    }
    value::super_new(class, obj)
}

/// Calls `super(...).name(...)` through the proxy's MRO position.
fn super_method(proxy: i64, method: &str, args: &[i64]) -> i64 {
    let Some(state) = value::as_super(proxy) else {
        return attribute_error(proxy, method);
    };
    let owner = if let Some(class) = value::instance_class_value(state.object) {
        class
    } else if value::as_class(state.object).is_some() {
        state.object
    } else {
        return attribute_error(proxy, method);
    };
    let Some(entry) = value::class_lookup_after(owner, state.class, method) else {
        return attribute_error(proxy, method);
    };
    call_bound_entry(state.object, owner, entry, method, args, &[])
}

/// Reads `super(...).name` through the proxy's MRO position.
fn super_getattr(proxy: i64, attr: &str) -> Option<i64> {
    let state = value::as_super(proxy)?;
    let owner = if let Some(class) = value::instance_class_value(state.object) {
        class
    } else if value::as_class(state.object).is_some() {
        state.object
    } else {
        return None;
    };
    let entry = value::class_lookup_after(owner, state.class, attr)?;
    match entry {
        value::ClassEntry::Method(MethodKind::Property, function) => {
            let args = [state.object];
            Some(builtins::call(function, 1, args.as_ptr()))
        }
        value::ClassEntry::Method(_, _) => {
            Some(value::bound_method_new(state.object, attr.to_string()))
        }
        value::ClassEntry::Attr(value) => {
            if let Some(result) = descriptor_get(value, state.object, owner) {
                return Some(result);
            }
            if value::as_function(value).is_some() {
                return Some(value::bound_method_new(state.object, attr.to_string()));
            }
            Some(value)
        }
    }
}

/// Calls one class-namespace entry with Python's receiver binding.
fn call_bound_entry(
    receiver: i64,
    owner: i64,
    entry: value::ClassEntry,
    method: &str,
    args: &[i64],
    keywords: &[(String, i64)],
) -> i64 {
    let (kind, function) = match entry {
        value::ClassEntry::Attr(value) => {
            // A descriptor instance resolves through `__get__` first.
            if let Some(result) = descriptor_get(value, receiver, owner) {
                if exc::pending() {
                    return NONE;
                }
                return builtins::call(result, args.len(), args.as_ptr());
            }
            if value::as_function(value).is_some() {
                (MethodKind::Normal, value)
            } else {
                return builtins::call(value, args.len(), args.as_ptr());
            }
        }
        value::ClassEntry::Method(kind, function) => (kind, function),
    };
    let bound = match kind {
        MethodKind::Normal | MethodKind::Property => Some(receiver),
        MethodKind::Classmethod => Some(owner),
        MethodKind::Staticmethod => None,
    };
    if !value::function_signature(function).is_empty() && value::function_full_entry(function) != 0
    {
        return call_user_method_kw(
            bound.unwrap_or(receiver),
            owner,
            kind,
            function,
            method,
            args,
            &KeywordArgs {
                items: keywords.to_vec(),
            },
        );
    }
    // Callables without parameter metadata cannot bind keywords; reject like
    // the other native call paths instead of silently dropping them.
    if let Some((name, _)) = keywords.first() {
        return type_error(format!(
            "{method}() got an unexpected keyword argument '{name}'"
        ));
    }
    let mut call_args = Vec::with_capacity(args.len() + 1);
    call_args.extend(bound);
    call_args.extend_from_slice(args);
    builtins::call(function, call_args.len(), call_args.as_ptr())
}

fn instance_method(obj: i64, method: &str, argc: usize, argv: *const i64) -> i64 {
    if crate::modules::asyncio::is_task_group(obj) {
        return crate::modules::asyncio::task_group_method(obj, method, argc, argv);
    }
    if argc > 0 && (argv.is_null() || argc > isize::MAX as usize / std::mem::size_of::<i64>()) {
        return type_error(format!(
            "{}.{}() received {} argument(s) without an argument buffer",
            value::type_name_of(obj),
            method,
            argc
        ));
    }
    let Some(class) = value::instance_class_value(obj) else {
        return attribute_error(obj, method);
    };
    let entry = value::class_lookup(class, method);
    // Python's precedence: a data descriptor wins over the instance
    // dictionary, which wins over a normal method / non-data descriptor.
    if let Some(entry) = &entry {
        if let Some(result) = data_descriptor_get(obj, class, entry) {
            if exc::pending() {
                return NONE;
            }
            return builtins::call(result, argc, argv);
        }
        if !is_data_descriptor(entry) {
            if let Some(attribute) = value::instance_attr(obj, method) {
                return builtins::call(attribute, argc, argv);
            }
        }
    } else if let Some(attribute) = value::instance_attr(obj, method) {
        return builtins::call(attribute, argc, argv);
    }
    let Some(entry) = entry else {
        return attribute_error(obj, method);
    };
    let args = if argc == 0 {
        Vec::new()
    } else {
        // SAFETY: checked above; caller contract.
        unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec()
    };
    call_bound_entry(obj, class, entry, method, &args, &[])
}

/// Whether a class-namespace entry is a data descriptor.
///
/// Properties are data descriptors; an object whose class implements
/// `__set__` or `__delete__` is one too. Plain functions and other values are
/// non-data descriptors (they have `__get__` at most).
fn is_data_descriptor(entry: &value::ClassEntry) -> bool {
    match entry {
        value::ClassEntry::Method(MethodKind::Property, _) => true,
        value::ClassEntry::Method(_, _) => false,
        value::ClassEntry::Attr(value) => {
            if let Some(state) = value::as_descriptor(*value) {
                // Only `property` is a data descriptor; `classmethod` and
                // `staticmethod` are non-data descriptors.
                return state.kind == value::DescriptorKind::Property;
            }
            descriptor_method(*value, "__set__").is_some()
                || descriptor_method(*value, "__delete__").is_some()
        }
    }
}

/// Reads a data descriptor stored in a class namespace.
///
/// Returns `None` when the entry is not a data descriptor (the instance
/// dictionary then wins) and `Some(NONE)` when it is one without `__get__`
/// (CPython raises `AttributeError` for that shape).
fn data_descriptor_get(instance: i64, owner: i64, entry: &value::ClassEntry) -> Option<i64> {
    match entry {
        value::ClassEntry::Method(MethodKind::Property, function) => {
            // A property getter takes exactly the instance: the descriptor
            // call binds the receiver and passes no extra arguments.
            let result = call_descriptor(
                owner,
                MethodKind::Normal,
                *function,
                "property",
                instance,
                &[],
            );
            Some(result)
        }
        value::ClassEntry::Method(_, _) => None,
        value::ClassEntry::Attr(value) => {
            if !is_data_descriptor(entry) {
                return None;
            }
            // A data descriptor with only `__set__`/`__delete__` has no
            // getter: reading falls back to the instance dictionary.
            descriptor_get(*value, instance, owner)
        }
    }
}

/// Returns the class and resolved method of `name` on the class of the
/// descriptor object `value`.
fn descriptor_method(value: i64, name: &str) -> Option<(i64, MethodKind, i64)> {
    let class = value::as_instance(value)?.class;
    match value::class_lookup(class, name)? {
        value::ClassEntry::Method(kind, function) => Some((class, kind, function)),
        value::ClassEntry::Attr(function) if value::as_function(function).is_some() => {
            Some((class, MethodKind::Normal, function))
        }
        value::ClassEntry::Attr(_) => None,
    }
}

/// Binds a built-in descriptor value for an instance/class access.
///
/// `classmethod` produces a partial applying the owner class, `staticmethod`
/// returns its target unchanged, and `property` evaluates its getter for an
/// instance (class access returns the property object itself).
fn bind_builtin_descriptor(
    descriptor: i64,
    state: &value::DescriptorState,
    instance: i64,
    owner: i64,
) -> i64 {
    match state.kind {
        value::DescriptorKind::Classmethod => value::partial_new(state.target, vec![owner]),
        value::DescriptorKind::Staticmethod => state.target,
        value::DescriptorKind::Property => {
            if instance == NONE {
                return descriptor;
            }
            call_receiver_callable(state.target, instance, owner, "property", &[])
        }
    }
}

/// Calls a callable with `receiver` bound as its first positional argument.
///
/// Generated functions bind through the keyword binder (so defaults and
/// keyword-only slots apply); native callables and classes receive the
/// receiver directly.
fn call_receiver_callable(
    target: i64,
    receiver: i64,
    owner: i64,
    method: &str,
    args: &[i64],
) -> i64 {
    if !value::function_signature(target).is_empty() && value::function_full_entry(target) != 0 {
        return call_user_method_kw(
            receiver,
            owner,
            MethodKind::Normal,
            target,
            method,
            args,
            &KeywordArgs { items: Vec::new() },
        );
    }
    let mut call_args = Vec::with_capacity(args.len() + 1);
    call_args.push(receiver);
    call_args.extend_from_slice(args);
    builtins::call(target, call_args.len(), call_args.as_ptr())
}

/// Methods of a built-in descriptor value: `property.getter/setter/deleter`.
fn descriptor_value_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    let Some(state) = value::as_descriptor(obj) else {
        return attribute_error(obj, method);
    };
    if state.kind != value::DescriptorKind::Property {
        return attribute_error(obj, method);
    }
    match method {
        "getter" | "setter" | "deleter" => {
            if let Some(err) = check_arity("property", method, args.len(), 1, 1) {
                return err;
            }
            match method {
                "getter" => value::property_new(args[0], state.setter, state.deleter),
                "setter" => value::property_new(state.target, args[0], state.deleter),
                _ => value::property_new(state.target, state.setter, args[0]),
            }
        }
        _ => attribute_error(obj, method),
    }
}

/// Calls `value.__get__(instance, owner)` when the object implements the
/// descriptor protocol. `None` means "not a descriptor".
fn descriptor_get(value: i64, instance: i64, owner: i64) -> Option<i64> {
    // Built-in `classmethod`/`staticmethod`/`property` values have their own
    // binding rules.
    if let Some(state) = value::as_descriptor(value) {
        return Some(bind_builtin_descriptor(value, state, instance, owner));
    }
    let (class, kind, function) = descriptor_method(value, "__get__")?;
    let args = [instance, owner];
    Some(call_descriptor(
        class, kind, function, "__get__", value, &args,
    ))
}

/// Calls `value.__set__(instance, new_value)` for a data descriptor.
fn descriptor_set(value: i64, instance: i64, _owner: i64, new_value: i64) -> Option<i64> {
    let (class, kind, function) = descriptor_method(value, "__set__")?;
    let args = [instance, new_value];
    Some(call_descriptor(
        class, kind, function, "__set__", value, &args,
    ))
}

/// Calls `value.__delete__(instance)` for a data descriptor.
fn descriptor_delete(value: i64, instance: i64, _owner: i64) -> Option<i64> {
    let (class, kind, function) = descriptor_method(value, "__delete__")?;
    let args = [instance];
    Some(call_descriptor(
        class,
        kind,
        function,
        "__delete__",
        value,
        &args,
    ))
}

/// Shared descriptor call: user methods bind through the keyword binder,
/// native/property functions through a positional call.
fn call_descriptor(
    class: i64,
    kind: MethodKind,
    function: i64,
    name: &str,
    receiver: i64,
    args: &[i64],
) -> i64 {
    if !value::function_signature(function).is_empty() && value::function_full_entry(function) != 0
    {
        return call_user_method_kw(
            receiver,
            class,
            kind,
            function,
            name,
            args,
            &KeywordArgs { items: Vec::new() },
        );
    }
    let bound = match kind {
        MethodKind::Normal | MethodKind::Property => Some(receiver),
        MethodKind::Classmethod => Some(class),
        MethodKind::Staticmethod => None,
    };
    let mut call_args = Vec::with_capacity(args.len() + 1);
    call_args.extend(bound);
    call_args.extend_from_slice(args);
    builtins::call(function, call_args.len(), call_args.as_ptr())
}

/// Resolves `class.attr` through the class's metaclass.
///
/// Used only after the class namespace misses (CPython's normal lookup order).
/// Returns `None` when the class has no explicit metaclass or the metaclass
/// does not define `attr`. A metaclass property runs with the class as its
/// receiver, so `Number.__abstractmethods__` works; every other entry becomes
/// a bound-method value that [`class_member_method`] rebinds through
/// [`call_bound_entry`].
fn metaclass_attribute(class: i64, attr: &str) -> Option<i64> {
    let metaclass = value::class_metaclass(class);
    if metaclass == NONE {
        return None;
    }
    match value::class_lookup(metaclass, attr)? {
        value::ClassEntry::Attr(item) => {
            if let Some(result) = descriptor_get(item, NONE, class) {
                return Some(result);
            }
            Some(item)
        }
        value::ClassEntry::Method(MethodKind::Property, function) => {
            let args = [class];
            Some(builtins::call(function, 1, args.as_ptr()))
        }
        value::ClassEntry::Method(_, _) => Some(value::bound_method_new(class, attr.to_string())),
    }
}

/// Calls `Class.method(...)` and `Class.attr`-style methods on a class value.
///
/// Ordinary methods are unbound (the caller supplies `self`), classmethods
/// bind the class and staticmethods bind nothing. Properties are data
/// attributes of instances, not callables on the class, so they report an
/// `AttributeError` here. A name missing from the class namespace is resolved
/// on the metaclass, where the class itself is the receiver.
fn class_member_method(class: i64, method: &str, argc: usize, argv: *const i64) -> i64 {
    if method == "__new__"
        && value::class_lookup(class, "__new__").is_none()
        && !metaclass_defines_new(class)
    {
        // `Cls.__new__(Cls)` allocates without running `__init__` when the
        // class (and its metaclass) define no custom `__new__`.
        return value::instance_new(class);
    }
    if argc > 0 && (argv.is_null() || argc > isize::MAX as usize / std::mem::size_of::<i64>()) {
        return type_error(format!(
            "{}.{}() received {} argument(s) without an argument buffer",
            value::type_name_of(class),
            method,
            argc
        ));
    }
    let (entry, via_metaclass) = match value::class_lookup(class, method) {
        Some(entry) => (entry, false),
        None => {
            let metaclass = value::class_metaclass(class);
            if metaclass == NONE {
                return attribute_error(class, method);
            }
            match value::class_lookup(metaclass, method) {
                Some(entry) => (entry, true),
                None => return attribute_error(class, method),
            }
        }
    };
    if via_metaclass {
        // A property is a data attribute of the class, not something callable
        // through `Cls.prop()`.
        if matches!(&entry, value::ClassEntry::Method(MethodKind::Property, _)) {
            return attribute_error(class, method);
        }
        let metaclass = value::class_metaclass(class);
        let args = if argc == 0 {
            Vec::new()
        } else {
            // SAFETY: argument buffer validated above.
            unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec()
        };
        return call_bound_entry(class, metaclass, entry, method, &args, &[]);
    }
    let (kind, function) = match entry {
        value::ClassEntry::Attr(value) => return builtins::call(value, argc, argv),
        value::ClassEntry::Method(MethodKind::Property, _) => {
            return attribute_error(class, method)
        }
        value::ClassEntry::Method(kind, function) => (kind, function),
    };
    if matches!(kind, MethodKind::Classmethod | MethodKind::Staticmethod)
        && !value::function_signature(function).is_empty()
        && value::function_full_entry(function) != 0
    {
        let args = if argc == 0 {
            Vec::new()
        } else {
            // SAFETY: checked above; caller contract.
            unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec()
        };
        return call_user_method(class, class, kind, function, method, &args, &[]);
    }
    let receiver = (kind == MethodKind::Classmethod).then_some(class);
    let mut call_args = Vec::with_capacity(argc + 1);
    call_args.extend(receiver);
    if argc > 0 {
        // SAFETY: ABI contract: `argv` points at `argc` readable values; length checked above.
        let args = unsafe { std::slice::from_raw_parts(argv, argc) };
        call_args.extend_from_slice(args);
    }
    builtins::call(function, call_args.len(), call_args.as_ptr())
}

/// Whether the class's metaclass defines `__new__`.
fn metaclass_defines_new(class: i64) -> bool {
    let metaclass = value::class_metaclass(class);
    metaclass != NONE && value::class_lookup(metaclass, "__new__").is_some()
}

/// Keyword parameter names for native class methods that bind keywords.
///
/// `timeit.Timer` methods, the `ssl.SSLContext` methods and the
/// `select`/`selectors` objects declare names; generated user methods have no
/// parameter metadata, so every other instance method rejects keywords.
/// `self` is implicit and excluded from the returned names.
fn native_instance_params(obj: i64, method: &str) -> Option<(&'static [&'static str], usize)> {
    let state = value::as_instance(obj)?;
    let function = value::class_method(state.class, method)?;
    let value::FunctionBody::Native { module, name } = &value::as_function(function)?.body else {
        return None;
    };
    if *module == "timeit" {
        return crate::modules::timeit::method_keyword_params(name).map(|params| (params, 0));
    }
    match (*module, *name) {
        ("ssl", "_ssl_context_wrap_socket") => Some((&["sock", "server_hostname"], 1)),
        ("ssl", "_ssl_context_load_verify_locations") => Some((&["cafile", "capath", "cadata"], 0)),
        ("ssl", "_ssl_context_load_cert_chain") => Some((&["certfile", "keyfile", "password"], 1)),
        ("selectors", "_selector_register") => Some((&["fileobj", "events", "data"], 2)),
        ("selectors", "_selector_unregister") => Some((&["fileobj"], 1)),
        ("selectors", "_selector_modify") => Some((&["fileobj", "events", "data"], 2)),
        ("selectors", "_selector_select") => Some((&["timeout"], 0)),
        ("selectors", "_selector_get_key") => Some((&["fileobj"], 1)),
        ("select", "_poll_register") => Some((&["fd", "eventmask"], 1)),
        ("select", "_poll_unregister") => Some((&["fd"], 1)),
        ("select", "_poll_modify") => Some((&["fd", "eventmask"], 2)),
        ("select", "_poll_poll") => Some((&["timeout"], 0)),
        _ => None,
    }
}

/// Runtime keyword binding for a generated user method, also used by class
/// instantiation to bind constructor arguments through the same path.
///
/// The source signature is stored on the function value (all slots including
/// the receiver). Positional arguments fill positional slots, a `*args` slot
/// captures the remainder, keyword arguments fill positional/keyword-only
/// slots by name, and unknown names go to `**kwargs`. Missing slots use the
/// literal default or raise `TypeError`. The resulting full slot vector is
/// forwarded through the generated uniform full-argv trampoline.
pub(crate) fn call_user_method(
    instance: i64,
    class: i64,
    kind: MethodKind,
    function: i64,
    method: &str,
    args: &[i64],
    keywords: &[(String, i64)],
) -> i64 {
    let keywords = KeywordArgs {
        items: keywords.to_vec(),
    };
    call_user_method_kw(instance, class, kind, function, method, args, &keywords)
}

fn call_user_method_kw(
    instance: i64,
    class: i64,
    kind: MethodKind,
    function: i64,
    method: &str,
    args: &[i64],
    keywords: &KeywordArgs,
) -> i64 {
    let signature = value::function_signature(function);
    let entry = value::function_full_entry(function);
    let receiver_name = value::class_display_name(class)
        .unwrap_or("object")
        .to_string();
    let receiver_count = usize::from(kind != MethodKind::Staticmethod);
    let mut slots: Vec<Option<i64>> = vec![None; signature.len()];
    let receiver = match kind {
        MethodKind::Normal | MethodKind::Property => Some(instance),
        MethodKind::Classmethod => Some(class),
        MethodKind::Staticmethod => None,
    };
    if let Some(receiver) = receiver {
        if !slots.is_empty() {
            // Normal methods bind the instance; classmethods bind the class.
            slots[0] = Some(receiver);
        }
    }

    let mut next_positional = 0usize;
    let mut vararg_items: Vec<i64> = Vec::new();
    for (index, param) in signature.iter().enumerate().skip(receiver_count) {
        match param.kind {
            // Positional-only parameters bind exactly like positional ones;
            // only keyword matching treats them differently.
            ParamKind::Positional | ParamKind::PosOnly => {
                if next_positional < args.len() {
                    slots[index] = Some(args[next_positional]);
                    next_positional += 1;
                }
            }
            ParamKind::VarArg => {
                vararg_items.extend_from_slice(&args[next_positional..]);
                next_positional = args.len();
            }
            ParamKind::KwOnly | ParamKind::KwArg => {}
        }
    }
    if next_positional < args.len() {
        return type_error(format!(
            "{receiver_name}.{method}() takes at most {} positional argument(s) ({} given)",
            signature.len().saturating_sub(receiver_count),
            args.len()
        ));
    }

    let mut extra_keywords: Vec<(String, i64)> = Vec::new();
    for (name, argument) in &keywords.items {
        let mut matched = false;
        for (index, param) in signature.iter().enumerate().skip(receiver_count) {
            if param.kind != ParamKind::Positional && param.kind != ParamKind::KwOnly {
                continue;
            }
            if param.name == *name {
                if slots[index].is_some() {
                    return type_error(format!(
                        "{receiver_name}.{method}() got multiple values for argument '{name}'"
                    ));
                }
                slots[index] = Some(*argument);
                matched = true;
                break;
            }
        }
        if !matched {
            if signature.iter().any(|param| param.kind == ParamKind::KwArg) {
                extra_keywords.push((name.clone(), *argument));
            } else {
                return type_error(format!(
                    "{receiver_name}.{method}() got an unexpected keyword argument '{name}'"
                ));
            }
        }
    }

    let vararg_index = signature
        .iter()
        .position(|param| param.kind == ParamKind::VarArg);
    if let Some(index) = vararg_index {
        slots[index] = Some(value::list_new(vararg_items));
    }
    let kwargs_index = signature
        .iter()
        .position(|param| param.kind == ParamKind::KwArg);
    if let Some(index) = kwargs_index {
        let pairs = extra_keywords
            .iter()
            .map(|(name, value)| (value::str_alloc(name.clone()), *value))
            .collect();
        slots[index] = Some(value::dict_new(pairs));
    }

    for (index, param) in signature.iter().enumerate().skip(receiver_count) {
        if slots[index].is_some() {
            continue;
        }
        match param.default {
            Some(default) => slots[index] = Some(default),
            None => {
                return type_error(format!(
                    "{receiver_name}.{method}() missing required argument '{}'",
                    param.name
                ))
            }
        }
    }

    let full: Vec<i64> = slots.into_iter().map(|slot| slot.unwrap_or(NONE)).collect();
    if let Some(func) = value::as_function(function) {
        if let value::FunctionBody::Host { token, captures } = &func.body {
            return crate::host::call_slots(*token, captures, &full, func.flags, &func.name);
        }
    }
    if let Some(func) = value::as_function(function) {
        if func.flags & (value::FUNCTION_FLAG_GENERATOR | value::FUNCTION_FLAG_ASYNC) != 0 {
            if signature.iter().all(|param| param.kind.is_positional()) {
                return builtins::call(function, full.len(), full.as_ptr());
            }
            return type_error(format!(
                "{receiver_name}.{method}() cannot bind keywords for a generator"
            ));
        }
    }
    crate::core::builtins::call_full_entry(entry, &full)
}

/// Keyword arguments decoded from the `[name, value, ...]` buffer passed by
/// generated code. Names are tagged strings.
pub(crate) struct KeywordArgs {
    pub(crate) items: Vec<(String, i64)>,
}

impl KeywordArgs {
    fn from_abi(kwargc: usize, kw_argv: *const i64) -> Result<KeywordArgs, i64> {
        if kwargc == 0 {
            return Ok(KeywordArgs { items: Vec::new() });
        }
        if kw_argv.is_null() || kwargc > isize::MAX as usize / (2 * std::mem::size_of::<i64>()) {
            return Err(type_error(
                "keyword arguments were passed without an argument buffer".to_string(),
            ));
        }
        // SAFETY: ABI contract: `kw_argv` points at `2 * kwargc` readable values; length checked.
        let pairs = unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) };
        let mut items = Vec::with_capacity(kwargc);
        for pair in pairs.as_chunks::<2>().0 {
            let Some(name) = value::as_str(pair[0]) else {
                return Err(type_error(format!(
                    "keyword argument name must be str, not {}",
                    value::type_name(pair[0])
                )));
            };
            items.push((name.to_string(), pair[1]));
        }
        Ok(KeywordArgs { items })
    }

    fn first_name(&self) -> Option<&str> {
        self.items.first().map(|(name, _)| name.as_str())
    }

    /// Reports the first keyword as unsupported for a method that has no
    /// keyword binding.
    fn reject(&self, receiver: &str, method: &str) -> i64 {
        match self.first_name() {
            Some(name) => type_error(format!(
                "{receiver}.{method}() got an unexpected keyword argument '{name}'"
            )),
            None => type_error(format!(
                "{receiver}.{method}() does not support keyword arguments"
            )),
        }
    }

    /// Binds the keywords to the named positional parameters of a method and
    /// rewrites `positional` into the full argument vector. Duplicate and
    /// unknown names are rejected, as are missing required parameters.
    fn bind(
        &self,
        receiver: &str,
        method: &str,
        params: &[&str],
        min_args: usize,
        positional: &mut Vec<i64>,
    ) -> Result<(), i64> {
        if positional.len() > params.len() {
            return Err(type_error(format!(
                "{receiver}.{method}() takes at most {} positional argument(s) ({} given)",
                params.len(),
                positional.len()
            )));
        }
        let mut supplied: Vec<Option<i64>> = positional.iter().copied().map(Some).collect();
        supplied.resize(params.len(), None);
        for (name, argument) in &self.items {
            let Some(index) = params.iter().position(|param| *param == name.as_str()) else {
                return Err(type_error(format!(
                    "{receiver}.{method}() got an unexpected keyword argument '{name}'"
                )));
            };
            if supplied[index].is_some() {
                return Err(type_error(format!(
                    "{receiver}.{method}() got multiple values for argument '{name}'"
                )));
            }
            supplied[index] = Some(*argument);
        }
        for (index, slot) in supplied.iter().enumerate() {
            if slot.is_none() && index < min_args {
                return Err(type_error(format!(
                    "{receiver}.{method}() missing required argument: '{}'",
                    params[index]
                )));
            }
        }
        // Omitted trailing slots stay unset, matching the direct-call ABI
        // where trailing parameters are simply absent. Explicitly passed
        // `None` values (Some(NONE)) are preserved.
        while supplied.len() > min_args && supplied.last().is_some_and(|slot| slot.is_none()) {
            supplied.pop();
        }
        positional.clear();
        for slot in supplied {
            positional.push(slot.unwrap_or(NONE));
        }
        Ok(())
    }
}

/// Dispatches `obj.<name>(args..., name=value...)`.
///
/// Calls without keywords delegate to [`call`] unchanged. Calls with keywords
/// bind them to the named positional parameters of the supported native
/// methods; unsupported receivers and method names report a `TypeError`
/// instead of silently ignoring the keywords.
pub(crate) fn call_kw(
    obj: i64,
    name: *const u8,
    name_len: usize,
    argc: usize,
    argv: *const i64,
    kwargc: usize,
    kw_argv: *const i64,
) -> i64 {
    if kwargc == 0 {
        return call(obj, name, name_len, argc, argv);
    }
    let Some(method) = decode_name(name, name_len) else {
        return attribute_error(obj, "");
    };
    let Some(mut args) = copy_args(argc, argv) else {
        return type_error(format!(
            "{}.{}() received {} argument(s) without an argument buffer",
            value::type_name(obj),
            method,
            argc
        ));
    };
    let keywords = match KeywordArgs::from_abi(kwargc, kw_argv) {
        Ok(keywords) => keywords,
        Err(err) => return err,
    };
    if let Some(module) = value::as_module(obj) {
        let module_name = module.name();
        return modules::module_call_kw(
            module_name.as_ptr(),
            module_name.len(),
            method.as_ptr(),
            method.len(),
            argc,
            argv,
            kwargc,
            kw_argv,
        );
    }
    if method == "$super_init" {
        return super_init(obj, &args, &keywords.items);
    }
    if method == "$super_new" {
        // The lowering passes the defining class positionally; keywords are
        // never part of the `super()` protocol.
        return type_error("$super_new() takes no keyword arguments".to_string());
    }
    // `super().name(...)` keyword calls resolve through the proxy's MRO
    // position, exactly like the positional path.
    if let Some(state) = value::as_super(obj) {
        let owner = if let Some(class) = value::instance_class_value(state.object) {
            class
        } else if value::as_class(state.object).is_some() {
            state.object
        } else {
            return attribute_error(obj, &method);
        };
        let Some(entry) = value::class_lookup_after(owner, state.class, &method) else {
            return attribute_error(obj, &method);
        };
        return call_bound_entry(state.object, owner, entry, &method, &args, &keywords.items);
    }
    if crate::modules::argparse::is_handle(obj) {
        return crate::modules::argparse::handle_method_kw(obj, &method, &mut args, &keywords);
    }
    if value::as_list(obj).is_some() {
        let (params, min_args): (&[&str], usize) = match method.as_str() {
            "sort" => (&["key", "reverse"], 0),
            _ => return keywords.reject("list", &method),
        };
        if let Err(err) = keywords.bind("list", &method, params, min_args, &mut args) {
            return err;
        }
        return list_method(obj, &method, args.as_slice());
    }
    if let Some(s) = value::as_str(obj) {
        return str_method_kw(obj, s.to_string(), &method, &mut args, &keywords);
    }
    if value::as_path(obj).is_some() {
        return path_method_kw(obj, &method, &mut args, &keywords);
    }
    if value::as_arg_parser(obj).is_some() {
        return crate::modules::argparse::method_kw(obj, &method, &mut args, &keywords);
    }
    if value::as_class(obj).is_some() {
        if let Some(value::ClassEntry::Method(kind, function)) = value::class_lookup(obj, &method) {
            if matches!(kind, MethodKind::Classmethod | MethodKind::Staticmethod)
                && !value::function_signature(function).is_empty()
                && value::function_full_entry(function) != 0
            {
                return call_user_method(obj, obj, kind, function, &method, &args, &keywords.items);
            }
        }
        return keywords.reject("class", &method);
    }
    if value::as_instance(obj).is_some() {
        // Generated user methods carry full parameter metadata and a uniform
        // full-argv trampoline. Bind all parameter kinds (including
        // keyword-only slots and **kwargs) before falling back to the native
        // class tables.
        if let Some(state) = value::as_instance(obj) {
            if let Some(value::ClassEntry::Method(kind, function)) =
                value::class_lookup(state.class, &method)
            {
                // Native methods such as `ExitStack.callback` forward the
                // call-site keywords to the callable they register; their
                // spec declares dynamic keyword support, so dispatch them
                // with the receiver in the positional list.
                if let Some(func) = value::as_function(function) {
                    if let value::FunctionBody::Native { module, name } = &func.body {
                        if crate::modules::native_accepts_keywords(module, name) {
                            let mut call_args = Vec::with_capacity(args.len() + 1);
                            call_args.push(obj);
                            call_args.extend_from_slice(&args);
                            return crate::modules::call_native_kw(
                                module,
                                name,
                                call_args.len(),
                                call_args.as_ptr(),
                                kwargc,
                                kw_argv,
                            );
                        }
                    }
                }
                if kind != MethodKind::Property
                    && !value::function_signature(function).is_empty()
                    && value::function_full_entry(function) != 0
                {
                    return call_user_method_kw(
                        obj,
                        state.class,
                        kind,
                        function,
                        &method,
                        &args,
                        &keywords,
                    );
                }
            }
        }
        // Native class methods with declared parameter names bind keywords;
        // generated user methods have no names in this ABI.
        if let Some((params, min_args)) = native_instance_params(obj, &method) {
            let receiver = value::instance_class_name(obj).unwrap_or("instance");
            if let Err(err) = keywords.bind(receiver, &method, params, min_args, &mut args) {
                return err;
            }
            return instance_method(obj, &method, args.len(), args.as_ptr());
        }
        // Function values carry no parameter names in this ABI, so user
        // methods only support positional arguments.
        return keywords.reject(&value::type_name_of(obj), &method);
    }
    // `int.from_bytes` binds `byteorder` and the keyword-only `signed`;
    // `to_bytes`/`bit_length` do the same for integer receivers.
    if is_builtin_int_type(obj) {
        return int_class_method(obj, &method, &args, &keywords.items);
    }
    if is_integer_value(obj) {
        return int_method(obj, &method, &args, &keywords.items);
    }
    // Native objects whose methods take no keyword parameters (and every
    // other receiver kind) reject the keywords explicitly.
    keywords.reject(value::type_name(obj), &method)
}

/// `obj.name(args..., *star, name=value..., **star_map)`.
///
/// Normalizes the positional and keyword operands exactly like
/// [`crate::core::builtins::call_star_kw`], then dispatches through [`call_kw`]
/// so every receiver kind (class, instance, native object) keeps its existing
/// keyword rules.
#[allow(clippy::too_many_arguments)]
pub(crate) fn method_star_kw(
    receiver: i64,
    name_ptr: *const u8,
    name_len: usize,
    argc: usize,
    argv: *const i64,
    star: i64,
    kwargc: usize,
    kw_argv: *const i64,
    star_map: i64,
) -> i64 {
    if argc > 0 && argv.is_null() {
        return type_error(format!(
            "method call received {argc} argument(s) without an argument buffer"
        ));
    }
    if argc > isize::MAX as usize / std::mem::size_of::<i64>()
        || kwargc > isize::MAX as usize / (2 * std::mem::size_of::<i64>())
    {
        return type_error("method call has too many arguments".to_string());
    }
    let Some(method) = decode_name(name_ptr, name_len) else {
        return attribute_error(receiver, "");
    };
    // SAFETY: ABI contract: `argv`/`kw_argv` point at readable buffers whose
    // lengths were validated above.
    let mut args: Vec<i64> = if argc == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec()
    };
    if star != NONE {
        match builtins::star_sequence(star) {
            Ok(items) => args.extend_from_slice(&items),
            Err(err) => return err,
        }
    }
    let mut keywords: Vec<(String, i64)> = Vec::with_capacity(kwargc);
    if kwargc > 0 {
        if kw_argv.is_null() {
            return type_error(
                "keyword arguments were passed without an argument buffer".to_string(),
            );
        }
        // SAFETY: ABI contract: `kw_argv` holds `2 * kwargc` readable values.
        let pairs = unsafe { std::slice::from_raw_parts(kw_argv, kwargc * 2) };
        for pair in pairs.as_chunks::<2>().0 {
            let Some(keyword) = value::as_str(pair[0]) else {
                return type_error("keyword name must be str".to_string());
            };
            keywords.push((keyword.to_string(), pair[1]));
        }
    }
    if star_map != NONE {
        let Some(mapping) = value::as_dict(star_map) else {
            return type_error(format!(
                "argument after ** must be a mapping, not '{}'",
                value::type_name(star_map)
            ));
        };
        let entries: Vec<(i64, i64)> = mapping.to_vec();
        for (key, item) in entries {
            let Some(keyword) = value::as_str(key) else {
                return type_error("keywords must be strings".to_string());
            };
            if keywords
                .iter()
                .any(|(existing, _)| existing.as_str() == keyword)
            {
                return type_error(format!(
                    "{method}() got multiple values for keyword argument '{keyword}'"
                ));
            }
            keywords.push((keyword.to_string(), item));
        }
    }
    let mut roots_source: Vec<i64> = args.clone();
    roots_source.extend(keywords.iter().map(|(_, item)| *item));
    let roots = crate::heap::root_values(&roots_source);
    let mut pairs: Vec<i64> = Vec::with_capacity(keywords.len() * 2);
    for (keyword, item) in &keywords {
        pairs.push(value::str_alloc(keyword.clone()));
        pairs.push(*item);
    }
    let result = call_kw(
        receiver,
        name_ptr,
        name_len,
        args.len(),
        args.as_ptr(),
        keywords.len(),
        pairs.as_ptr(),
    );
    crate::heap::release_roots(roots);
    result
}

/// Keyword dispatch for string methods: `split`, `replace`, `startswith` and
/// `endswith` bind their named parameters; every other method rejects them.
fn str_method_kw(
    obj: i64,
    s: String,
    method: &str,
    args: &mut Vec<i64>,
    keywords: &KeywordArgs,
) -> i64 {
    if method == "format" {
        return match crate::format::call_format_kw(&s, args, &keywords.items) {
            Ok(text) => value::str_alloc(text),
            Err(error) => exc::raise_with_message(error.tag, &error.message),
        };
    }
    let (params, min_args): (&[&str], usize) = match method {
        "split" | "rsplit" => (&["sep", "maxsplit"], 0),
        "splitlines" => (&["keepends"], 0),
        "replace" => (&["old", "new", "count"], 2),
        "startswith" => (&["prefix"], 1),
        "endswith" => (&["suffix"], 1),
        _ => return keywords.reject("str", method),
    };
    if let Err(err) = keywords.bind("str", method, params, min_args, args) {
        return err;
    }
    str_method(obj, s, method, args.as_slice())
}

/// Keyword dispatch for `pathlib.Path` methods. `joinpath(*parts)` takes no
/// keyword parameters and is therefore rejected like any other unsupported
/// method.
fn path_method_kw(obj: i64, method: &str, args: &mut Vec<i64>, keywords: &KeywordArgs) -> i64 {
    let (params, min_args): (&[&str], usize) = match method {
        "mkdir" => (&["parents", "exist_ok"], 0),
        "open" => (&["mode", "encoding", "newline", "errors"], 0),
        "write_text" => (&["text", "encoding"], 1),
        "read_text" => (&["encoding"], 0),
        "write_bytes" => (&["data"], 1),
        "glob" => (&["pattern"], 1),
        _ => return keywords.reject("Path", method),
    };
    if let Err(err) = keywords.bind("Path", method, params, min_args, args) {
        return err;
    }
    path_method(obj, method, args.as_slice())
}

/// Dispatches `obj.<name>` for data attributes of native objects, class
/// instances, class objects and built-in exception objects.
///
/// Method attributes stay callable through [`call`] and remain an
/// `AttributeError` when read without a call; a `@property` is a data
/// attribute and calls its getter. Class instances resolve their own attribute
/// table first, then `__class__`, properties and the frozen class attributes.
/// Class objects expose their frozen attributes and `__name__`. Exception
/// objects expose `args`, `__class__`, `__cause__`, `__context__`,
/// `__suppress_context__` and `__traceback__` through
/// [`crate::exc::exception_attr`]. Unknown names and receivers without
/// attributes report a pending `AttributeError` and return `None`.
pub(crate) fn getattr(obj: i64, name: *const u8, name_len: usize) -> i64 {
    let Some(attr) = decode_name(name, name_len) else {
        return attribute_error(obj, "");
    };
    // PEP 750 template values expose `strings`, `interpolations`, `values`
    // and the interpolation fields directly.
    if let Some(attribute) = crate::core::template::getattr(obj, &attr) {
        return attribute;
    }
    if let Some(module) = value::as_module(obj) {
        return crate::modules::module_get_by_name(module.name(), &attr);
    }
    // Function metadata: the generated code stores the bare definition name,
    // so `f.__name__`/`f.__qualname__` report it and `__doc__` stays `None`
    // until a program assigns one. `__module__`/`__annotations__` are absent
    // because Hydro keeps no module provenance or annotation payload; every
    // other name falls back to the function's live `__dict__`.
    if let Some(function) = value::as_function(obj) {
        // The `int` builtin type value exposes its class-level methods as
        // callable attributes, like CPython's type objects.
        if matches!(
            &function.body,
            value::FunctionBody::Builtin(hydro_ir::Builtin::Int)
        ) && matches!(attr.as_str(), "from_bytes" | "to_bytes" | "bit_length")
        {
            return value::bound_method_new(obj, attr);
        }
        if let value::FunctionBody::Partial {
            function: target,
            args,
            keywords,
        } = &function.body
        {
            // CPython's `functools.partial` exposes the wrapped callable, the
            // bound positional arguments and the bound keywords.
            match attr.as_str() {
                "func" => return *target,
                "args" => return value::tuple_new(args.clone()),
                "keywords" => {
                    let mark = crate::heap::root_values(&[]);
                    let pairs: Vec<(i64, i64)> = keywords
                        .iter()
                        .map(|(name, item)| (value::str_alloc(name.clone()), *item))
                        .collect();
                    let result = value::dict_new(pairs);
                    crate::heap::release_roots(mark);
                    return result;
                }
                _ => {}
            }
        }
        return match attr.as_str() {
            "__name__" => value::str_alloc(value::function_name(obj)),
            "__qualname__" => value::str_alloc(value::function_qualname(obj)),
            "__doc__" => value::function_doc(obj),
            "__module__" => match value::function_module(obj) {
                Some(module) => module,
                None => attribute_error(obj, &attr),
            },
            "__annotations__" => match value::function_annotations(obj) {
                Some(annotations) => annotations,
                None => attribute_error(obj, &attr),
            },
            "__dict__" => {
                if value::function_has_attributes(obj) {
                    value::function_dict(obj).unwrap_or(NONE)
                } else {
                    attribute_error(obj, &attr)
                }
            }
            _ => match value::function_attribute(obj, &attr) {
                Some(attribute) => attribute,
                None => attribute_error(obj, &attr),
            },
        };
    }
    // Built-in exception objects expose `args`, `__class__`, `__cause__`,
    // `__context__`, `__suppress_context__` and `__traceback__`. The check is
    // before the instance/class branches because exception objects are not
    // class instances.
    if let Some(attribute) = exc::exception_attr(obj, &attr) {
        return attribute;
    }
    if value::as_path(obj).is_some() {
        return path_attr(obj, &attr);
    }
    if value::as_file(obj).is_some() {
        return file_attr(obj, &attr);
    }
    if value::as_list(obj).is_some() {
        if matches!(
            attr.as_str(),
            "append"
                | "extend"
                | "insert"
                | "remove"
                | "pop"
                | "clear"
                | "reverse"
                | "copy"
                | "count"
                | "index"
                | "sort"
        ) {
            return value::bound_method_new(obj, attr);
        }
        return attribute_error(obj, &attr);
    }
    if value::as_dict(obj).is_some() {
        if matches!(
            attr.as_str(),
            "get"
                | "keys"
                | "values"
                | "items"
                | "pop"
                | "update"
                | "setdefault"
                | "clear"
                | "copy"
                | "__getitem__"
                | "__setitem__"
                | "__contains__"
                | "__len__"
        ) {
            return value::bound_method_new(obj, attr);
        }
        return attribute_error(obj, &attr);
    }
    if value::as_set(obj).is_some() {
        // Native bound method values cover read-only set operations used as
        // callbacks (`_SINGLE_CELLS.issuperset` in rich).
        if matches!(attr.as_str(), "issubset" | "issuperset") {
            return value::bound_method_new(obj, attr);
        }
        return attribute_error(obj, &attr);
    }
    if value::as_thread(obj).is_some() {
        return thread_attr(obj, &attr);
    }
    if value::as_process(obj).is_some() {
        return process_attr(obj, &attr);
    }
    if value::as_namespace(obj).is_some() {
        let Some(pairs) = value::as_namespace(obj) else {
            return attribute_error(obj, &attr);
        };
        if let Some((_, value)) = pairs.iter().find(|(name, _)| name.as_str() == attr) {
            return *value;
        }
        return attribute_error(obj, &attr);
    }
    if value::as_arg_parser(obj).is_some() {
        if let Some(result) = crate::modules::argparse::parser_attr(obj, &attr) {
            return result;
        }
        return attribute_error(obj, &attr);
    }
    if value::as_defaultdict(obj).is_some() {
        if let Some(result) = crate::modules::collections::getattr(obj, &attr) {
            return result;
        }
        return attribute_error(obj, &attr);
    }
    if value::as_pattern(obj).is_some() {
        if matches!(
            attr.as_str(),
            "search" | "match" | "fullmatch" | "findall" | "finditer" | "sub" | "split"
        ) {
            return value::bound_method_new(obj, attr);
        }
        if let Some(result) = crate::modules::re::pattern_attr(obj, &attr) {
            return result;
        }
        return attribute_error(obj, &attr);
    }
    if value::as_match(obj).is_some() {
        if matches!(
            attr.as_str(),
            "group" | "groups" | "groupdict" | "start" | "end" | "span"
        ) {
            return value::bound_method_new(obj, attr);
        }
        return attribute_error(obj, &attr);
    }
    if let Some(class) = value::instance_class_value(obj) {
        if attr == "__dict__" {
            if !value::class_has_dict(class) {
                return attribute_error(obj, &attr);
            }
            return match value::instance_dict(obj) {
                Some(mapping) => mapping,
                None => attribute_error(obj, &attr),
            };
        }
        // `__class__` is a data descriptor on every Python object, so it wins
        // over an instance attribute of the same name.
        if attr == "__class__" {
            return class;
        }
        let entry = value::class_lookup(class, &attr);
        // 1. A data descriptor wins over the instance dictionary.
        if let Some(entry) = entry.as_ref().filter(|entry| is_data_descriptor(entry)) {
            if let Some(result) = data_descriptor_get(obj, class, entry) {
                return result;
            }
        }
        // 2. The instance dictionary.
        if let Some(attribute) = value::instance_attr(obj, &attr) {
            return attribute;
        }
        // 3. Class attributes, non-data descriptors and methods.
        let Some(entry) = entry else {
            return attribute_error(obj, &attr);
        };
        return match entry {
            // A non-data descriptor resolves `__get__(instance, owner)`.
            value::ClassEntry::Attr(item) => {
                if let Some(result) = descriptor_get(item, obj, class) {
                    return result;
                }
                if value::as_function(item).is_some() {
                    // A function assigned to the class binds the receiver,
                    // exactly like a class-body method.
                    return value::bound_method_new(obj, attr);
                }
                item
            }
            value::ClassEntry::Method(MethodKind::Property, function) => {
                let args = [obj];
                builtins::call(function, 1, args.as_ptr())
            }
            value::ClassEntry::Method(_, _) => value::bound_method_new(obj, attr),
        };
    }
    if let Some(class) = value::as_class(obj) {
        // Every class object knows its Python-visible name; its attributes
        // were frozen when the class statement ran and later assignments live
        // in the overlay.
        match attr.as_str() {
            "__name__" | "__qualname__" => return value::str_alloc(class.name.clone()),
            "__bases__" => return value::tuple_new(class.bases.clone()),
            "__base__" => return class.bases.first().copied().unwrap_or(NONE),
            "__mro__" => return value::tuple_new(value::class_mro(obj)),
            "__class__" => return value::type_class(obj),
            "__dict__" => {
                let pairs = value::class_namespace(obj);
                let dict = value::dict_new(Vec::new());
                let root = crate::heap::root_values(&[dict]);
                for (name, item) in pairs {
                    let key = value::str_alloc(name);
                    container::dict_set(dict, key, item);
                }
                crate::heap::release_roots(root);
                return dict;
            }
            _ => {}
        }
        if attr == "__new__" && value::class_lookup(obj, "__new__").is_none() {
            return value::bound_method_new(obj, attr);
        }
        match value::class_lookup(obj, &attr) {
            Some(value::ClassEntry::Attr(item)) => {
                // A descriptor stored as a class attribute receives
                // `(None, owner)` when read from the class.
                if let Some(result) = descriptor_get(item, NONE, obj) {
                    return result;
                }
                return item;
            }
            Some(value::ClassEntry::Method(MethodKind::Property, _)) => {}
            Some(value::ClassEntry::Method(_, _)) => {
                return value::bound_method_new(obj, attr);
            }
            None => {}
        }
        // A name missing from the class namespace falls back to the
        // metaclass, as in CPython: this is how `ABCMeta.register`,
        // `__instancecheck__`, `__subclasscheck__` and `__abstractmethods__`
        // reach every class created with that metaclass.
        if let Some(metaclass) = metaclass_attribute(obj, &attr) {
            return metaclass;
        }
        return attribute_error(obj, &attr);
    }
    if value::as_super(obj).is_some() {
        if let Some(result) = super_getattr(obj, &attr) {
            return result;
        }
        return attribute_error(obj, &attr);
    }
    if let Some(state) = value::as_descriptor(obj) {
        // `property` exposes `getter`/`setter`/`deleter` returning new
        // property objects; the other descriptor kinds have no methods.
        return match attr.as_str() {
            "getter" | "setter" | "deleter" if state.kind == value::DescriptorKind::Property => {
                value::bound_method_new(obj, attr)
            }
            _ => attribute_error(obj, &attr),
        };
    }
    if value::as_bytearray(obj).is_some() {
        if matches!(
            attr.as_str(),
            "append"
                | "extend"
                | "insert"
                | "pop"
                | "remove"
                | "reverse"
                | "clear"
                | "copy"
                | "decode"
                | "hex"
                | "count"
                | "find"
                | "replace"
                | "split"
                | "strip"
                | "lstrip"
                | "rstrip"
                | "join"
                | "upper"
                | "lower"
                | "startswith"
                | "endswith"
                | "__len__"
                | "__getitem__"
                | "__setitem__"
                | "__delitem__"
                | "__contains__"
        ) {
            return value::bound_method_new(obj, attr);
        }
        return attribute_error(obj, &attr);
    }
    if let Some(view) = value::as_memoryview(obj) {
        if view.released {
            return exc::raise_with_message(exc::VALUE_ERROR, value::MEMORYVIEW_RELEASED);
        }
        return match attr.as_str() {
            "format" => value::str_alloc(view.format.to_string()),
            "itemsize" => ops::int_result(view.itemsize as i64),
            "ndim" => ops::int_result(1),
            "shape" => value::tuple_new(vec![ops::int_result(view.len() as i64)]),
            "strides" => value::tuple_new(vec![ops::int_result(view.stride as i64)]),
            "readonly" => value::bool_value(view.readonly),
            "nbytes" => ops::int_result(view.nbytes() as i64),
            "contiguous" => value::TRUE,
            "obj" => view.buffer,
            "tobytes" | "tolist" | "release" | "cast" | "__len__" | "__getitem__"
            | "__setitem__" | "__contains__" => value::bound_method_new(obj, attr),
            _ => attribute_error(obj, &attr),
        };
    }
    if let Some(view) = value::as_dict_view(obj) {
        return match attr.as_str() {
            "mapping" => view.dict,
            "__len__" | "__iter__" | "__contains__" | "isdisjoint" => {
                value::bound_method_new(obj, attr)
            }
            _ => attribute_error(obj, &attr),
        };
    }
    // Integer values expose their bytes-conversion and bit-length methods as
    // bound-method values (`f = (255).to_bytes`).
    if is_integer_value(obj) && matches!(attr.as_str(), "to_bytes" | "bit_length") {
        return value::bound_method_new(obj, attr);
    }
    attribute_error(obj, &attr)
}

/// `hasattr(obj, name)`: whether the attribute exists.
///
/// It runs the regular attribute lookup and swallows only `AttributeError`;
/// any other failure (a property raising a different exception) propagates
/// with its own tag. A non-string name is rejected by the builtin itself.
pub(crate) fn hasattr(obj: i64, name: *const u8, name_len: usize) -> i64 {
    let _value = getattr(obj, name, name_len);
    if !exc::pending() {
        return value::TRUE;
    }
    if exc::pending_tag() == ATTRIBUTE_ERROR as i64 {
        exc::clear();
        return value::FALSE;
    }
    NONE
}

/// `obj.name = value` for class instances and classes.
///
/// Instance wiring follows Python's data-descriptor rule: a data descriptor
/// (property, or an object whose class implements `__set__`) receives the
/// assignment; otherwise the instance dictionary stores it, subject to
/// `__slots__` restrictions. Class assignment writes the runtime overlay, so
/// it never mutates the frozen class body.
pub(crate) fn setattr(obj: i64, name: *const u8, name_len: usize, new_value: i64) -> i64 {
    let Some(attr) = decode_name(name, name_len) else {
        return attribute_error(obj, "");
    };
    // Python-level functions and partial objects have CPython's writable
    // attribute surface: the `__name__`/`__qualname__`/`__doc__`/`__module__`/
    // `__annotations__` slots plus a `__dict__` of arbitrary attributes.
    // Native module functions and builtins are C-level callables and reject
    // assignment, exactly like CPython's `builtin_function_or_method`.
    if let Some(function) = value::as_function(obj) {
        // `functools.partial`'s `func`/`args`/`keywords` are read-only slots,
        // like CPython; every other name uses the function attribute surface.
        if matches!(&function.body, value::FunctionBody::Partial { .. })
            && matches!(attr.as_str(), "func" | "args" | "keywords")
        {
            return attribute_error(obj, &attr);
        }
        match attr.as_str() {
            "__name__" | "__qualname__" if value::as_str(new_value).is_none() => {
                return type_error(format!("{attr} must be set to a string object"));
            }
            "__dict__" if value::as_dict(new_value).is_none() => {
                return type_error(format!(
                    "__dict__ must be set to a dictionary, not a '{}'",
                    value::type_name(new_value)
                ));
            }
            _ => {}
        }
        if value::function_setattr(obj, &attr, new_value) {
            return NONE;
        }
        return attribute_error(obj, &attr);
    }
    if let Some(class) = value::instance_class_value(obj) {
        if attr == "__dict__" {
            if value::instance_set_dict(obj, new_value) {
                return NONE;
            }
            return type_error("__dict__ must be set to a dict".to_string());
        }
        if let Some(entry) = value::class_lookup(class, &attr).filter(is_data_descriptor) {
            match &entry {
                value::ClassEntry::Method(MethodKind::Property, _) => {
                    let name = value::instance_class_name(obj).unwrap_or("object");
                    return exc::raise_with_message(
                        ATTRIBUTE_ERROR,
                        &format!("property '{attr}' of '{name}' object has no setter"),
                    );
                }
                value::ClassEntry::Attr(descriptor) => {
                    if let Some(state) = value::as_descriptor(*descriptor) {
                        if state.kind == value::DescriptorKind::Property {
                            if state.setter == NONE {
                                let name = value::instance_class_name(obj).unwrap_or("object");
                                return exc::raise_with_message(
                                    ATTRIBUTE_ERROR,
                                    &format!("property '{attr}' of '{name}' object has no setter"),
                                );
                            }
                            return call_receiver_callable(
                                state.setter,
                                obj,
                                class,
                                "property",
                                &[new_value],
                            );
                        }
                    }
                    if let Some(result) = descriptor_set(*descriptor, obj, class, new_value) {
                        return result;
                    }
                    let name = value::instance_class_name(obj).unwrap_or("object");
                    return exc::raise_with_message(
                        ATTRIBUTE_ERROR,
                        &format!("can't set attribute '{attr}' of '{name}' object"),
                    );
                }
                value::ClassEntry::Method(_, _) => {}
            }
        }
        if !value::class_allows_attr(class, &attr) {
            let name = value::instance_class_name(obj).unwrap_or("object");
            return exc::raise_with_message(
                ATTRIBUTE_ERROR,
                &format!("'{name}' object has no attribute '{attr}'"),
            );
        }
        value::instance_setattr(obj, attr, new_value);
        return NONE;
    }
    if let Some(class) = value::as_class(obj) {
        if value::class_set_attr(obj, attr, new_value) {
            return NONE;
        }
        return type_error(format!("cannot set attributes of class '{}'", class.name));
    }
    attribute_error(obj, &attr)
}

/// `del obj.name` for class instances and classes.
///
/// A data descriptor with `__delete__` receives the deletion; otherwise the
/// instance attribute is removed. Class deletion tombstones the name (or drops
/// a runtime assignment). Every other receiver reports `AttributeError`.
pub(crate) fn delete_attr(obj: i64, name: *const u8, name_len: usize) -> i64 {
    let Some(attr) = decode_name(name, name_len) else {
        return attribute_error(obj, "");
    };
    if let Some(class) = value::instance_class_value(obj) {
        if let Some(value::ClassEntry::Attr(descriptor)) = value::class_lookup(class, &attr) {
            if let Some(state) = value::as_descriptor(descriptor) {
                if state.kind == value::DescriptorKind::Property && state.deleter != NONE {
                    return call_receiver_callable(state.deleter, obj, class, "property", &[]);
                }
            }
            if descriptor_method(descriptor, "__delete__").is_some() {
                if let Some(result) = descriptor_delete(descriptor, obj, class) {
                    return result;
                }
            }
        }
        if value::instance_remove_attr(obj, &attr).is_some() {
            return NONE;
        }
        return attribute_error(obj, &attr);
    }
    if let Some(class) = value::as_class(obj) {
        if value::class_delete_attr(obj, &attr) {
            return NONE;
        }
        return type_error(format!(
            "cannot delete attribute '{}' of class '{}'",
            attr, class.name
        ));
    }
    attribute_error(obj, &attr)
}

/// Data attributes of `pathlib.Path`.
fn path_attr(obj: i64, attr: &str) -> i64 {
    let Some(state) = value::as_path(obj) else {
        return attribute_error(obj, attr);
    };
    // Copy the receiver's path before any allocation: a reference into a heap
    // object must not be read after an allocation that may collect.
    let path = state.path().to_string();
    match attr {
        "name" | "stem" | "suffix" => {
            let name = path_name(&path);
            let text = match attr {
                "name" => name,
                "stem" => path_stem(&name),
                _ => path_suffix(&name),
            };
            value::str_alloc(text)
        }
        "parent" => value::path_new(path_parent(&path)),
        "path" => value::str_alloc(path),
        _ => attribute_error(obj, attr),
    }
}

/// Data attributes of an open file object.
fn file_attr(obj: i64, attr: &str) -> i64 {
    let Some(file) = value::as_file(obj) else {
        return attribute_error(obj, attr);
    };
    // Snapshot the state before allocating the result.
    let path = file.path().to_string();
    let mode = file.mode();
    let binary = file.binary();
    let closed = file.closed();
    match attr {
        "name" => value::str_alloc(path),
        "mode" => value::str_alloc(format!("{mode}{}", if binary { "b" } else { "" })),
        "closed" => value::bool_value(closed),
        "encoding" => {
            if binary {
                NONE
            } else {
                value::str_alloc("UTF-8".to_string())
            }
        }
        _ => attribute_error(obj, attr),
    }
}

/// Data attributes of a `threading.Thread`.
fn thread_attr(obj: i64, attr: &str) -> i64 {
    let Some(thread) = value::as_thread(obj) else {
        return attribute_error(obj, attr);
    };
    match attr {
        "name" => {
            let name = thread.name.clone();
            value::str_alloc(name)
        }
        "ident" => {
            let id = thread.id.load(Ordering::Acquire);
            if id == 0 {
                NONE
            } else {
                ops::int_result(id)
            }
        }
        // The runtime always spawns non-daemon threads.
        "daemon" => value::FALSE,
        _ => attribute_error(obj, attr),
    }
}

/// Data attributes of a `multiprocessing.Process`.
fn process_attr(obj: i64, attr: &str) -> i64 {
    let Some(process) = value::as_process(obj) else {
        return attribute_error(obj, attr);
    };
    match attr {
        "name" => {
            let name = process.name.clone();
            value::str_alloc(name)
        }
        "pid" => ops::int_result(process.pid.load(Ordering::Acquire)),
        "exitcode" => match process::process_exitcode(process) {
            Some(code) => ops::int_result(code),
            None => NONE,
        },
        // The runtime always spawns non-daemon processes.
        "daemon" => value::FALSE,
        _ => attribute_error(obj, attr),
    }
}

/// Decodes the method name. `None` covers `NULL`, empty and non-UTF-8 names,
/// all of which surface as an `AttributeError` on the receiver.
fn decode_name(ptr: *const u8, len: usize) -> Option<String> {
    if ptr.is_null() || len == 0 || len > isize::MAX as usize {
        return None;
    }
    // SAFETY: ABI contract: `ptr..ptr+len` is readable; length checked above.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

/// Copies the argument vector, rejecting `argc > 0` with a `NULL` buffer
/// instead of dereferencing it.
fn copy_args(argc: usize, argv: *const i64) -> Option<Vec<i64>> {
    if argc == 0 {
        return Some(Vec::new());
    }
    if argv.is_null() || argc > isize::MAX as usize / std::mem::size_of::<i64>() {
        return None;
    }
    // SAFETY: the ABI contract requires `argc` readable tagged values.
    Some(unsafe { std::slice::from_raw_parts(argv, argc) }.to_vec())
}

fn type_error(message: String) -> i64 {
    exc::raise_with_message(TYPE_ERROR, &message)
}

fn attribute_error(obj: i64, method: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!(
            "'{}' object has no attribute '{}'",
            value::type_name_of(obj),
            method
        ),
    )
}

/// Checks `argc` against `min..=max`.
///
/// The exact-arity form uses the receiver prefix (`list.append() takes
/// exactly one argument (0 given)`); the optional range form follows
/// CPython's unprefixed wording (`pop() takes at most 1 argument (2 given)`).
fn check_arity(recv: &str, method: &str, argc: usize, min: usize, max: usize) -> Option<i64> {
    if argc >= min && argc <= max {
        return None;
    }
    if min == max {
        let message = match min {
            0 => format!("{recv}.{method}() takes no arguments ({argc} given)"),
            1 => format!("{recv}.{method}() takes exactly one argument ({argc} given)"),
            n => format!("{recv}.{method}() takes exactly {n} arguments ({argc} given)"),
        };
        return Some(type_error(message));
    }
    if argc < min {
        Some(type_error(format!(
            "{method}() takes at least {min} {} ({argc} given)",
            plural(min)
        )))
    } else {
        Some(type_error(format!(
            "{method}() takes at most {max} {} ({argc} given)",
            plural(max)
        )))
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        "argument"
    } else {
        "arguments"
    }
}

/// Clones a string argument, reporting a `TypeError` naming the 1-based
/// argument position otherwise.
fn str_arg(method: &str, position: usize, v: i64) -> Result<String, i64> {
    match value::as_str(v) {
        Some(s) => Ok(s.to_string()),
        None => Err(type_error(format!(
            "{method}() argument {position} must be str, not {}",
            value::type_name(v)
        ))),
    }
}

/// Decodes an integer argument, rejecting non-integers like CPython.
fn int_arg(v: i64) -> Result<i64, i64> {
    value::as_integer(v).ok_or_else(|| {
        type_error(format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(v)
        ))
    })
}

// int

/// Whether `obj` is the `int` builtin *type* value.
///
/// Builtin types are plain builtin function values at runtime (`int` is the
/// `Builtin::Int` function), so `int.from_bytes(...)` reaches [`call`] with a
/// function receiver instead of a class object. Only `int` has a class-level
/// method table here; every other builtin function value keeps the plain
/// function attribute surface.
fn is_builtin_int_type(obj: i64) -> bool {
    let Some(function) = value::as_function(obj) else {
        return false;
    };
    matches!(
        &function.body,
        value::FunctionBody::Builtin(hydro_ir::Builtin::Int)
    )
}

/// Whether `obj` is an integer value (`int`, `bool` or a heap `BigInt`).
fn is_integer_value(obj: i64) -> bool {
    value::as_integer(obj).is_some() || value::is_bigint(obj)
}

/// CPython's accepted `byteorder` spellings.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ByteOrder {
    Big,
    Little,
}

/// Validates the `byteorder` argument shared by `int.from_bytes` and
/// `int.to_bytes`. A missing argument defaults to `'big'`; a non-string is a
/// `TypeError`, an unknown spelling a `ValueError`, exactly like CPython.
fn byte_order(method: &str, argument: Option<i64>) -> Result<ByteOrder, i64> {
    let Some(argument) = argument else {
        return Ok(ByteOrder::Big);
    };
    let Some(text) = value::as_str(argument) else {
        return Err(type_error(format!(
            "{method}() argument 'byteorder' must be str, not {}",
            value::type_name(argument)
        )));
    };
    match text {
        "big" => Ok(ByteOrder::Big),
        "little" => Ok(ByteOrder::Little),
        _ => Err(exc::raise_with_message(
            VALUE_ERROR,
            "byteorder must be either 'little' or 'big'",
        )),
    }
}

/// Binds the positional-or-keyword prefix (`params`) and the keyword-only
/// `signed` flag shared by `int.from_bytes` and `int.to_bytes`.
///
/// Returns the slots in parameter order (missing slots stay `None`) and the
/// truthiness of `signed`.
fn int_bytes_arguments(
    method: &str,
    params: &[&str],
    args: &[i64],
    keywords: &[(String, i64)],
) -> Result<(Vec<Option<i64>>, bool), i64> {
    if args.len() > params.len() {
        return Err(type_error(format!(
            "{method}() takes at most {} positional {} ({} given)",
            params.len(),
            plural(params.len()),
            args.len()
        )));
    }
    let mut slots = vec![None; params.len()];
    for (index, argument) in args.iter().enumerate() {
        slots[index] = Some(*argument);
    }
    let mut signed = None;
    for (name, argument) in keywords {
        if name.as_str() == "signed" {
            if signed.is_some() {
                return Err(type_error(format!(
                    "{method}() got multiple values for argument 'signed'"
                )));
            }
            signed = Some(*argument);
            continue;
        }
        let Some(index) = params.iter().position(|param| *param == name.as_str()) else {
            return Err(type_error(format!(
                "{method}() got an unexpected keyword argument '{name}'"
            )));
        };
        if slots[index].is_some() {
            return Err(type_error(format!(
                "{method}() got multiple values for argument '{name}'"
            )));
        }
        slots[index] = Some(*argument);
    }
    Ok((slots, signed.is_some_and(value::truthy)))
}

/// Copies the payload of a buffer-protocol argument (`bytes`, `bytearray` or
/// `memoryview`) for `int.from_bytes`.
fn buffer_bytes(object: i64) -> Result<Vec<u8>, i64> {
    if let Some(bytes) = value::as_bytes(object) {
        return Ok(bytes.to_vec());
    }
    if let Some(view) = value::as_memoryview(object) {
        return value::memoryview_bytes(view)
            .map_err(|message| exc::raise_with_message(VALUE_ERROR, message));
    }
    Err(type_error(format!(
        "cannot convert '{}' object to bytes",
        value::type_name(object)
    )))
}

/// `int.from_bytes(bytes, byteorder='big', *, signed=False)`.
///
/// The result keeps Python's arbitrary precision: the bytes are decoded as a
/// `BigInt` and allocated through [`value::bigint_new`], which tags the value
/// when it fits. `signed` selects two's-complement sign extension.
fn int_from_bytes(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let (slots, signed) =
        match int_bytes_arguments("from_bytes", &["bytes", "byteorder"], args, keywords) {
            Ok(bound) => bound,
            Err(err) => return err,
        };
    let Some(data) = slots[0] else {
        return type_error("from_bytes() missing required argument 'bytes' (pos 1)".to_string());
    };
    let order = match byte_order("from_bytes", slots[1]) {
        Ok(order) => order,
        Err(err) => return err,
    };
    let data = match buffer_bytes(data) {
        Ok(data) => data,
        Err(err) => return err,
    };
    let number = match (order, signed) {
        (ByteOrder::Big, false) => BigInt::from_bytes_be(Sign::Plus, &data),
        (ByteOrder::Little, false) => BigInt::from_bytes_le(Sign::Plus, &data),
        (ByteOrder::Big, true) => BigInt::from_signed_bytes_be(&data),
        (ByteOrder::Little, true) => BigInt::from_signed_bytes_le(&data),
    };
    value::bigint_new(number)
}

/// `int.to_bytes(length=1, byteorder='big', *, signed=False)`.
///
/// The magnitude is encoded big-endian, two's-complemented for negative
/// numbers and reversed for `'little'`. Values that do not fit `length` bytes
/// report `OverflowError`, matching CPython's range rules.
fn int_to_bytes(obj: i64, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let (slots, signed) =
        match int_bytes_arguments("to_bytes", &["length", "byteorder"], args, keywords) {
            Ok(bound) => bound,
            Err(err) => return err,
        };
    let length = match slots[0] {
        None => 1,
        Some(argument) => match value::to_bigint(argument) {
            Some(number) => match i64::try_from(&number) {
                Ok(length) => length,
                Err(_) => {
                    return exc::raise_with_message(
                        OVERFLOW_ERROR,
                        "Python int too large to convert to C ssize_t",
                    )
                }
            },
            None => {
                return type_error(format!(
                    "'{}' object cannot be interpreted as an integer",
                    value::type_name(argument)
                ))
            }
        },
    };
    if length < 0 {
        return exc::raise_with_message(VALUE_ERROR, "length argument must be non-negative");
    }
    if length > builtins::MAX_BYTES_ALLOC as i64 {
        return exc::raise_with_message(
            VALUE_ERROR,
            "bytes length exceeds the runtime allocation limit",
        );
    }
    let length = length as usize;
    let order = match byte_order("to_bytes", slots[1]) {
        Ok(order) => order,
        Err(err) => return err,
    };
    let Some(number) = value::to_bigint(obj) else {
        return type_error("descriptor 'to_bytes' requires an 'int' object".to_string());
    };
    let sign = number.sign();
    // `BigUint::to_bytes_be` pads zero to `[0]`; an empty magnitude is the
    // correct starting point for the zero value.
    let magnitude = if sign == Sign::NoSign {
        Vec::new()
    } else {
        number.magnitude().to_bytes_be()
    };
    let top = magnitude.first().copied().unwrap_or(0);
    // Zero fits every length, including zero.
    if sign != Sign::NoSign {
        if signed {
            let fits = if sign == Sign::Minus {
                // The most negative representable value is `-2^(8*length-1)`,
                // whose magnitude is `0x80` followed by zeros.
                magnitude.len() < length
                    || (magnitude.len() == length
                        && (top < 0x80
                            || (top == 0x80 && magnitude[1..].iter().all(|&byte| byte == 0))))
            } else {
                magnitude.len() < length || (magnitude.len() == length && top < 0x80)
            };
            if !fits {
                return exc::raise_with_message(OVERFLOW_ERROR, "int too big to convert");
            }
        } else if sign == Sign::Minus {
            return exc::raise_with_message(
                OVERFLOW_ERROR,
                "can't convert negative int to unsigned",
            );
        } else if magnitude.len() > length {
            return exc::raise_with_message(OVERFLOW_ERROR, "int too big to convert");
        }
    }
    let mut encoded = vec![0u8; length];
    let pad = length - magnitude.len();
    encoded[pad..].copy_from_slice(&magnitude);
    if sign == Sign::Minus {
        // Two's complement of the zero-padded magnitude: `0xFF` padding above
        // the magnitude becomes the sign extension.
        for byte in encoded.iter_mut() {
            *byte = !*byte;
        }
        let mut carry = true;
        for byte in encoded.iter_mut().rev() {
            if !carry {
                break;
            }
            let (sum, overflow) = byte.overflowing_add(1);
            *byte = sum;
            carry = overflow;
        }
    }
    if order == ByteOrder::Little {
        encoded.reverse();
    }
    value::bytes_new(encoded)
}

/// `(n).bit_length()`: the number of bits needed for the absolute value; `0`
/// for zero. Works for tagged integers, `bool` and heap `BigInt` values.
fn int_bit_length(obj: i64, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if let Some(err) = check_arity("int", "bit_length", args.len(), 0, 0) {
        return err;
    }
    if !keywords.is_empty() {
        return type_error("bit_length() takes no keyword arguments".to_string());
    }
    match value::to_bigint(obj) {
        Some(number) => ops::int_result(number.bits() as i64),
        None => type_error("descriptor 'bit_length' requires an 'int' object".to_string()),
    }
}

/// Class-level methods reached through the `int` builtin type value.
///
/// `from_bytes` is the classmethod CPython exposes on the type; `to_bytes` and
/// `bit_length` are additionally callable unbound (`int.to_bytes(n, ...)`), so
/// the first positional argument supplies the receiver.
fn int_class_method(obj: i64, method: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match method {
        "from_bytes" => int_from_bytes(args, keywords),
        "to_bytes" | "bit_length" => {
            let Some((receiver, rest)) = args.split_first() else {
                return type_error(format!("unbound method int.{method}() needs an argument"));
            };
            int_method(*receiver, method, rest, keywords)
        }
        _ => attribute_error(obj, method),
    }
}

/// Instance methods of integer values.
fn int_method(obj: i64, method: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match method {
        "to_bytes" => int_to_bytes(obj, args, keywords),
        "bit_length" => int_bit_length(obj, args, keywords),
        _ => attribute_error(obj, method),
    }
}

// str

/// Converts the `width` argument of the padding methods to a character count.
///
/// CPython converts through `Py_ssize_t`, so integers outside that range raise
/// `OverflowError` rather than `TypeError`.
fn padding_width(v: i64) -> Result<i64, i64> {
    if let Some(width) = value::as_integer(v) {
        return Ok(width);
    }
    match value::as_bigint(v) {
        Some(number) => i64::try_from(number).map_err(|_| {
            exc::raise_with_message(
                OVERFLOW_ERROR,
                "Python int too large to convert to C ssize_t",
            )
        }),
        None => Err(type_error(format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(v)
        ))),
    }
}

/// Resolves the optional `fillchar` argument of the padding methods.
///
/// CPython requires a single-character string; the default is a space.
fn padding_fill(argument: Option<i64>) -> Result<char, i64> {
    let Some(argument) = argument else {
        return Ok(' ');
    };
    let Some(text) = value::as_str(argument) else {
        return Err(type_error(format!(
            "The fill character must be a unicode character, not {}",
            value::type_name(argument)
        )));
    };
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(fill), None) => Ok(fill),
        _ => Err(type_error(
            "The fill character must be exactly one character long".to_string(),
        )),
    }
}

fn str_method(obj: i64, s: String, method: &str, args: &[i64]) -> i64 {
    match method {
        "upper" => {
            if let Some(err) = check_arity("str", method, args.len(), 0, 0) {
                return err;
            }
            value::str_alloc(s.to_uppercase())
        }
        "lower" => {
            if let Some(err) = check_arity("str", method, args.len(), 0, 0) {
                return err;
            }
            value::str_alloc(s.to_lowercase())
        }
        "strip" | "lstrip" | "rstrip" => {
            if let Some(err) = check_arity("str", method, args.len(), 0, 0) {
                return err;
            }
            let trimmed = match method {
                "strip" => s.trim(),
                "lstrip" => s.trim_start(),
                _ => s.trim_end(),
            };
            value::str_alloc(trimmed.to_string())
        }
        "rjust" | "ljust" | "center" => {
            if let Some(err) = check_arity("str", method, args.len(), 1, 2) {
                return err;
            }
            let width = match padding_width(args[0]) {
                Ok(width) => width,
                Err(err) => return err,
            };
            let fill = match padding_fill(args.get(1).copied()) {
                Ok(fill) => fill,
                Err(err) => return err,
            };
            // The padding methods count code points, so `width` is compared
            // against the character length rather than the byte length.
            let length = s.chars().count();
            let total = match usize::try_from(width) {
                Ok(total) if total > length => total,
                _ => return value::str_alloc(s),
            };
            let padding = total - length;
            let (left, right) = match method {
                "rjust" => (padding, 0),
                "ljust" => (0, padding),
                _ => {
                    // CPython places the extra fill character on the right
                    // when the width is odd; `marg & width & 1` encodes that.
                    let left = padding / 2 + (padding & total & 1);
                    (left, padding - left)
                }
            };
            let mut out = String::new();
            for _ in 0..left {
                out.push(fill);
            }
            out.push_str(&s);
            for _ in 0..right {
                out.push(fill);
            }
            value::str_alloc(out)
        }
        "zfill" => {
            if let Some(err) = check_arity("str", method, args.len(), 1, 1) {
                return err;
            }
            let width = match padding_width(args[0]) {
                Ok(width) => width,
                Err(err) => return err,
            };
            let length = s.chars().count();
            let padding = match usize::try_from(width) {
                Ok(width) if width > length => width - length,
                _ => return value::str_alloc(s),
            };
            // Zeros go after a leading sign, so `'-42'.zfill(5)` is `'-0042'`.
            let (sign, rest) = match s.chars().next() {
                Some(sign @ ('+' | '-')) => (Some(sign), &s[sign.len_utf8()..]),
                _ => (None, s.as_str()),
            };
            let mut out = String::new();
            if let Some(sign) = sign {
                out.push(sign);
            }
            for _ in 0..padding {
                out.push('0');
            }
            out.push_str(rest);
            value::str_alloc(out)
        }
        "split" => {
            if let Some(err) = check_arity("str", method, args.len(), 0, 2) {
                return err;
            }
            let maxsplit = match args.get(1).copied() {
                None => -1,
                Some(arg) => match int_arg(arg) {
                    Ok(n) => n,
                    Err(err) => return err,
                },
            };
            let parts: Vec<String> = if args.is_empty() || args[0] == NONE {
                // Python's whitespace split drops empty fields; an explicit
                // `None` separator selects the same form.
                split_whitespace_limit(&s, maxsplit)
            } else {
                let Some(sep) = value::as_str(args[0]) else {
                    return type_error(format!(
                        "must be str or None, not {}",
                        value::type_name(args[0])
                    ));
                };
                let sep = sep.to_string();
                if sep.is_empty() {
                    return exc::raise_with_message(VALUE_ERROR, "empty separator");
                }
                // Explicit separators keep empty fields, like CPython.
                if maxsplit < 0 {
                    s.split(sep.as_str()).map(str::to_owned).collect()
                } else {
                    let limit = usize::try_from(maxsplit)
                        .unwrap_or(usize::MAX - 1)
                        .saturating_add(1);
                    s.splitn(limit, sep.as_str()).map(str::to_owned).collect()
                }
            };
            str_list(parts)
        }
        "rsplit" => {
            if let Some(err) = check_arity("str", method, args.len(), 0, 2) {
                return err;
            }
            let maxsplit = match args.get(1).copied() {
                None => -1,
                Some(arg) => match int_arg(arg) {
                    Ok(n) => n,
                    Err(err) => return err,
                },
            };
            let parts: Vec<String> = if args.is_empty() || args[0] == NONE {
                split_whitespace_limit(&s, maxsplit)
            } else {
                let Some(sep) = value::as_str(args[0]) else {
                    return type_error(format!(
                        "must be str or None, not {}",
                        value::type_name(args[0])
                    ));
                };
                let sep = sep.to_string();
                if sep.is_empty() {
                    return exc::raise_with_message(VALUE_ERROR, "empty separator");
                }
                if maxsplit < 0 {
                    s.rsplit(sep.as_str()).map(str::to_owned).collect()
                } else {
                    let limit = usize::try_from(maxsplit)
                        .unwrap_or(usize::MAX - 1)
                        .saturating_add(1);
                    let mut parts: Vec<String> =
                        s.rsplitn(limit, sep.as_str()).map(str::to_owned).collect();
                    parts.reverse();
                    parts
                }
            };
            str_list(parts)
        }
        "join" => {
            if let Some(err) = check_arity("str", method, args.len(), 1, 1) {
                return err;
            }
            let items = match crate::core::builtins::star_sequence(args[0]) {
                Ok(items) => items,
                Err(_) => {
                    exc::clear();
                    return type_error("can only join an iterable".to_string());
                }
            };
            let mut out = String::new();
            for (index, item) in items.iter().enumerate() {
                let Some(part) = value::as_str(*item) else {
                    return type_error(format!(
                        "sequence item {}: expected str instance, {} found",
                        index,
                        value::type_name(*item)
                    ));
                };
                if index > 0 {
                    out.push_str(&s);
                }
                out.push_str(part);
            }
            value::str_alloc(out)
        }
        "partition" | "rpartition" => {
            if let Some(err) = check_arity("str", method, args.len(), 1, 1) {
                return err;
            }
            let Some(sep) = value::as_str(args[0]) else {
                return type_error(format!("must be str, not {}", value::type_name(args[0])));
            };
            if sep.is_empty() {
                return exc::raise_with_message(VALUE_ERROR, "empty separator");
            }
            let split = if method == "partition" {
                s.find(sep)
            } else {
                s.rfind(sep)
            };
            match split {
                Some(index) => value::tuple_new(vec![
                    value::str_alloc(s[..index].to_string()),
                    value::str_alloc(sep.to_string()),
                    value::str_alloc(s[index + sep.len()..].to_string()),
                ]),
                None if method == "partition" => value::tuple_new(vec![
                    value::str_alloc(s),
                    value::str_alloc(String::new()),
                    value::str_alloc(String::new()),
                ]),
                None => value::tuple_new(vec![
                    value::str_alloc(String::new()),
                    value::str_alloc(String::new()),
                    value::str_alloc(s),
                ]),
            }
        }
        "splitlines" => {
            if let Some(err) = check_arity("str", method, args.len(), 0, 1) {
                return err;
            }
            let keepends = args
                .first()
                .filter(|arg| **arg != NONE)
                .map(|arg| value::truthy(*arg))
                .unwrap_or(false);
            let result = value::list_new(Vec::new());
            let root = crate::heap::root_values(&[result]);
            let mut start = 0usize;
            let mut chars = s.char_indices().peekable();
            while let Some((index, ch)) = chars.next() {
                let mut width = ch.len_utf8();
                let is_break = matches!(
                    ch,
                    '\n' | '\r'
                        | '\x0b'
                        | '\x0c'
                        | '\x1c'
                        | '\x1d'
                        | '\x1e'
                        | '\u{85}'
                        | '\u{2028}'
                        | '\u{2029}'
                );
                if ch == '\r' {
                    if let Some(&(_, '\n')) = chars.peek() {
                        chars.next();
                        width += 1;
                    }
                }
                if is_break {
                    let end = if keepends { index + width } else { index };
                    let part = value::str_alloc(s[start..end].to_string());
                    container::list_append(result, part);
                    start = index + width;
                }
            }
            if start < s.len() {
                let part = value::str_alloc(s[start..].to_string());
                container::list_append(result, part);
            }
            crate::heap::release_roots(root);
            result
        }
        "translate" => {
            if let Some(err) = check_arity("str", method, args.len(), 1, 1) {
                return err;
            }
            if args[0] == NONE {
                return value::str_alloc(s);
            }
            let Some(table) = value::as_dict_data(args[0]) else {
                return type_error(format!(
                    "translate() argument must be a dict or None, not '{}'",
                    value::type_name(args[0])
                ));
            };
            let mut out = String::with_capacity(s.len());
            for ch in s.chars() {
                let Some(key) = value::encode_int(ch as i64) else {
                    out.push(ch);
                    continue;
                };
                if let Some(position) = table.find(key) {
                    let replacement = table.entries()[position].1;
                    if replacement != NONE {
                        let Some(text) = value::as_str(replacement) else {
                            return type_error(format!(
                                "character mapping must return integer, None or str, not '{}'",
                                value::type_name(replacement)
                            ));
                        };
                        out.push_str(text);
                    }
                } else {
                    out.push(ch);
                }
            }
            value::str_alloc(out)
        }
        "replace" => {
            if let Some(err) = check_arity("str", method, args.len(), 2, 3) {
                return err;
            }
            let old = match str_arg(method, 1, args[0]) {
                Ok(s) => s,
                Err(err) => return err,
            };
            let new = match str_arg(method, 2, args[1]) {
                Ok(s) => s,
                Err(err) => return err,
            };
            let count = if args.len() == 3 {
                match int_arg(args[2]) {
                    Ok(n) => n,
                    Err(err) => return err,
                }
            } else {
                -1
            };
            let replaced = if count < 0 {
                s.replace(old.as_str(), new.as_str())
            } else {
                s.replacen(old.as_str(), new.as_str(), count as usize)
            };
            value::str_alloc(replaced)
        }
        "startswith" | "endswith" => {
            if let Some(err) = check_arity("str", method, args.len(), 1, 1) {
                return err;
            }
            let pat = match str_arg(method, 1, args[0]) {
                Ok(s) => s,
                Err(err) => return err,
            };
            value::bool_value(if method == "startswith" {
                s.starts_with(pat.as_str())
            } else {
                s.ends_with(pat.as_str())
            })
        }
        "find" => {
            if let Some(err) = check_arity("str", method, args.len(), 1, 1) {
                return err;
            }
            let pat = match str_arg(method, 1, args[0]) {
                Ok(s) => s,
                Err(err) => return err,
            };
            let position = match s.find(pat.as_str()) {
                // `find` returns a character index, not a byte offset.
                Some(byte) => s[..byte].chars().count() as i64,
                None => -1,
            };
            ops::int_result(position)
        }
        "count" => {
            if let Some(err) = check_arity("str", method, args.len(), 1, 1) {
                return err;
            }
            let pat = match str_arg(method, 1, args[0]) {
                Ok(s) => s,
                Err(err) => return err,
            };
            let n = if pat.is_empty() {
                s.chars().count() + 1
            } else {
                s.matches(pat.as_str()).count()
            };
            ops::int_result(n as i64)
        }
        "encode" => {
            if let Some(err) = check_arity("str", method, args.len(), 0, 2) {
                return err;
            }
            // Route through the codec module so encoding names, aliases and
            // error handlers behave exactly like `str.encode` in CPython.
            let mut call_args = Vec::with_capacity(1 + args.len());
            call_args.push(obj);
            for (position, argument) in args.iter().enumerate() {
                if value::as_str(*argument).is_none() {
                    return type_error(format!(
                        "{method}() argument {} must be str, not {}",
                        position + 1,
                        value::type_name(*argument)
                    ));
                }
                call_args.push(*argument);
            }
            // A registered error handler runs Python code and may trigger a
            // collection, so the receiver and the codec arguments must
            // survive the call.
            let roots = crate::heap::root_values(&call_args);
            let result = crate::modules::codecs::call("encode", &call_args);
            crate::heap::release_roots(roots);
            result
        }
        "format" => match crate::format::call_format(&s, args) {
            Ok(text) => value::str_alloc(text),
            Err(error) => exc::raise_with_message(error.tag, &error.message),
        },
        "isdigit" | "isalpha" | "isalnum" | "isspace" => {
            if let Some(err) = check_arity("str", method, args.len(), 0, 0) {
                return err;
            }
            let matches = |f: fn(char) -> bool| !s.is_empty() && s.chars().all(f);
            value::bool_value(match method {
                "isdigit" => matches(char::is_numeric),
                "isalpha" => matches(char::is_alphabetic),
                "isalnum" => matches(char::is_alphanumeric),
                _ => matches(char::is_whitespace),
            })
        }
        "isascii" => {
            if let Some(err) = check_arity("str", method, args.len(), 0, 0) {
                return err;
            }
            // CPython semantics: every character is at most U+007F. The empty
            // string is ASCII.
            value::bool_value(s.is_ascii())
        }
        // CPython semantics: at least one cased character, and no character
        // of the opposite case. Uncased characters (digits, punctuation) are
        // ignored, so `"ABC1"` is upper and `"abc1"` is lower.
        "isupper" | "islower" => {
            if let Some(err) = check_arity("str", method, args.len(), 0, 0) {
                return err;
            }
            let upper = method == "isupper";
            let mut cased = false;
            let mut matched = true;
            for ch in s.chars() {
                let wrong_case = if upper {
                    ch.is_lowercase()
                } else {
                    ch.is_uppercase()
                };
                if wrong_case {
                    matched = false;
                    break;
                }
                if ch.is_uppercase() || ch.is_lowercase() {
                    cased = true;
                }
            }
            value::bool_value(matched && cased)
        }
        _ => attribute_error(obj, method),
    }
}

/// `str.split(None, maxsplit)` semantics: leading whitespace is skipped,
/// fields are separated by whitespace runs, and the remainder after
/// `maxsplit` splits keeps its trailing whitespace.
fn split_whitespace_limit(text: &str, maxsplit: i64) -> Vec<String> {
    if maxsplit < 0 {
        return text.split_whitespace().map(str::to_owned).collect();
    }
    let mut parts = Vec::new();
    let mut rest = text;
    for _ in 0..maxsplit {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            return parts;
        }
        let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        parts.push(trimmed[..end].to_string());
        rest = &trimmed[end..];
    }
    let remainder = rest.trim_start();
    if !remainder.is_empty() {
        parts.push(remainder.to_string());
    }
    parts
}

/// Builds a list of freshly allocated strings.
///
/// GC: the list is allocated first and held in a tagged local, so the
/// conservative collector can see it across the per-item allocations; items
/// are pushed into the registered list incrementally instead of collecting
/// them in an unregistered `Vec` that a collection could not trace.
fn str_list(parts: Vec<String>) -> i64 {
    let result = value::list_new(Vec::new());
    for part in parts {
        let item = value::str_alloc(part);
        if let Some(mut items) = value::list_items_mut(result) {
            items.push(item);
            crate::heap::write_barrier_edge(result, item);
        }
    }
    result
}

// bytes

/// Copies a `bytes` argument, reporting a `TypeError` naming the 1-based
/// argument position otherwise.
fn bytes_arg(method: &str, position: usize, v: i64) -> Result<Vec<u8>, i64> {
    match value::as_bytes(v) {
        Some(bytes) => Ok(bytes.to_vec()),
        None => Err(type_error(format!(
            "{method}() argument {position} must be bytes, not {}",
            value::type_name(v)
        ))),
    }
}

/// Builds a list of freshly allocated bytes objects, incrementally so the
/// conservative collector traces every element already appended.
fn bytes_list(parts: Vec<Vec<u8>>) -> i64 {
    let result = value::list_new(Vec::new());
    for part in parts {
        let item = value::bytes_new(part);
        if let Some(mut items) = value::list_items_mut(result) {
            items.push(item);
            crate::heap::write_barrier_edge(result, item);
        }
    }
    result
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn count_sub(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return haystack.len() + 1;
    }
    let mut count = 0;
    let mut pos = 0;
    while pos + needle.len() <= haystack.len() {
        if &haystack[pos..pos + needle.len()] == needle {
            count += 1;
            pos += needle.len();
        } else {
            pos += 1;
        }
    }
    count
}

fn hex_bytes(data: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(data.len() * 2);
    for &byte in data {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// `bytes.replace`: returns `None` when the result size cannot be
/// represented (the caller raises `OverflowError`).
fn replace_bytes(data: &[u8], old: &[u8], new: &[u8], count: i64) -> Option<Vec<u8>> {
    let limit = if count < 0 {
        usize::MAX
    } else {
        count as usize
    };
    let max_hits = if old.is_empty() {
        data.len().saturating_add(1)
    } else {
        data.len() / old.len() + 1
    };
    let hits = if limit == usize::MAX {
        max_hits
    } else {
        limit.min(max_hits)
    };
    let capacity = hits
        .checked_mul(new.len())
        .and_then(|extra| data.len().checked_add(extra))
        .filter(|total| *total <= isize::MAX as usize)?;
    if old.is_empty() {
        let mut out = Vec::with_capacity(capacity);
        let mut inserted = 0;
        for index in 0..=data.len() {
            if inserted < limit {
                out.extend_from_slice(new);
                inserted += 1;
            }
            if index < data.len() {
                out.push(data[index]);
            }
        }
        return Some(out);
    }
    let mut out = Vec::with_capacity(capacity);
    let mut pos = 0;
    let mut done = 0;
    while done < limit {
        let Some(offset) = find_sub(&data[pos..], old) else {
            break;
        };
        let at = pos + offset;
        out.extend_from_slice(&data[pos..at]);
        out.extend_from_slice(new);
        pos = at + old.len();
        done += 1;
    }
    out.extend_from_slice(&data[pos..]);
    Some(out)
}

/// Trims bytes from both ends (`left`/`right`) using either `chars` or ASCII
/// whitespace.
fn strip_bytes(data: &[u8], chars: Option<&[u8]>, left: bool, right: bool) -> Vec<u8> {
    let keep = |byte: u8| match chars {
        Some(set) => !set.contains(&byte),
        None => !matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c),
    };
    let mut start = 0;
    let mut end = data.len();
    if left {
        while start < end && !keep(data[start]) {
            start += 1;
        }
    }
    if right {
        while end > start && !keep(data[end - 1]) {
            end -= 1;
        }
    }
    data[start..end].to_vec()
}

fn bytes_method(obj: i64, data: Vec<u8>, method: &str, args: &[i64]) -> i64 {
    match method {
        "decode" => {
            if let Some(err) = check_arity("bytes", method, args.len(), 0, 2) {
                return err;
            }
            // Decode a private copy: a registered error handler can resize or
            // mutate the original bytearray while this call is in progress.
            let source = value::bytes_new(data);
            let mut call_args = Vec::with_capacity(1 + args.len());
            call_args.push(source);
            for (position, argument) in args.iter().enumerate() {
                if value::as_str(*argument).is_none() {
                    return type_error(format!(
                        "{method}() argument {} must be str, not {}",
                        position + 1,
                        value::type_name(*argument)
                    ));
                }
                call_args.push(*argument);
            }
            let roots = crate::heap::root_values(&call_args);
            let result = crate::modules::codecs::call("decode", &call_args);
            crate::heap::release_roots(roots);
            result
        }
        "hex" => {
            if let Some(err) = check_arity("bytes", method, args.len(), 0, 0) {
                return err;
            }
            value::str_alloc(hex_bytes(&data))
        }
        "count" => {
            if let Some(err) = check_arity("bytes", method, args.len(), 1, 1) {
                return err;
            }
            let needle = match bytes_arg(method, 1, args[0]) {
                Ok(bytes) => bytes,
                Err(err) => return err,
            };
            ops::int_result(count_sub(&data, &needle) as i64)
        }
        "find" => {
            if let Some(err) = check_arity("bytes", method, args.len(), 1, 1) {
                return err;
            }
            let needle = match bytes_arg(method, 1, args[0]) {
                Ok(bytes) => bytes,
                Err(err) => return err,
            };
            let position = match find_sub(&data, &needle) {
                Some(index) => index as i64,
                None => -1,
            };
            ops::int_result(position)
        }
        "startswith" | "endswith" | "replace" | "split" | "strip" | "lstrip" | "rstrip" => {
            match method {
                "startswith" | "endswith" => {
                    if let Some(err) = check_arity("bytes", method, args.len(), 1, 1) {
                        return err;
                    }
                    let pattern = match bytes_arg(method, 1, args[0]) {
                        Ok(bytes) => bytes,
                        Err(err) => return err,
                    };
                    let matched = if method == "startswith" {
                        data.starts_with(&pattern)
                    } else {
                        data.ends_with(&pattern)
                    };
                    value::bool_value(matched)
                }
                "replace" => {
                    if let Some(err) = check_arity("bytes", method, args.len(), 2, 3) {
                        return err;
                    }
                    let old = match bytes_arg(method, 1, args[0]) {
                        Ok(bytes) => bytes,
                        Err(err) => return err,
                    };
                    let new = match bytes_arg(method, 2, args[1]) {
                        Ok(bytes) => bytes,
                        Err(err) => return err,
                    };
                    let count = if args.len() == 3 {
                        match int_arg(args[2]) {
                            Ok(n) => n,
                            Err(err) => return err,
                        }
                    } else {
                        -1
                    };
                    match replace_bytes(&data, &old, &new, count) {
                        Some(out) => value::bytes_new(out),
                        None => exc::raise_with_message(OVERFLOW_ERROR, "integer overflow"),
                    }
                }
                "split" => {
                    if let Some(err) = check_arity("bytes", method, args.len(), 0, 1) {
                        return err;
                    }
                    let mut parts: Vec<Vec<u8>> = Vec::new();
                    if args.is_empty() || args[0] == NONE {
                        // Whitespace split drops empty fields.
                        let mut start = None;
                        for (index, &byte) in data.iter().enumerate() {
                            let whitespace =
                                matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
                            if whitespace {
                                if let Some(begin) = start.take() {
                                    parts.push(data[begin..index].to_vec());
                                }
                            } else if start.is_none() {
                                start = Some(index);
                            }
                        }
                        if let Some(begin) = start {
                            parts.push(data[begin..].to_vec());
                        }
                    } else {
                        let separator = match bytes_arg(method, 1, args[0]) {
                            Ok(bytes) => bytes,
                            Err(err) => return err,
                        };
                        if separator.is_empty() {
                            return exc::raise_with_message(VALUE_ERROR, "empty separator");
                        }
                        let mut pos = 0;
                        while let Some(offset) = find_sub(&data[pos..], &separator) {
                            let at = pos + offset;
                            parts.push(data[pos..at].to_vec());
                            pos = at + separator.len();
                        }
                        parts.push(data[pos..].to_vec());
                    }
                    bytes_list(parts)
                }
                _ => {
                    if let Some(err) = check_arity("bytes", method, args.len(), 0, 1) {
                        return err;
                    }
                    let chars = if args.is_empty() || args[0] == NONE {
                        None
                    } else {
                        match bytes_arg(method, 1, args[0]) {
                            Ok(bytes) => Some(bytes),
                            Err(err) => return err,
                        }
                    };
                    let (left, right) = match method {
                        "lstrip" => (true, false),
                        "rstrip" => (false, true),
                        _ => (true, true),
                    };
                    value::bytes_new(strip_bytes(&data, chars.as_deref(), left, right))
                }
            }
        }
        "join" => {
            if let Some(err) = check_arity("bytes", method, args.len(), 1, 1) {
                return err;
            }
            let items = match crate::core::builtins::star_sequence(args[0]) {
                Ok(items) => items,
                Err(_) => {
                    exc::clear();
                    return type_error("can only join an iterable".to_string());
                }
            };
            let mut total = 0usize;
            for (index, item) in items.iter().enumerate() {
                let Some(part) = value::as_bytes(*item) else {
                    return type_error(format!(
                        "sequence item {}: expected a bytes-like object, {} found",
                        index,
                        value::type_name(*item)
                    ));
                };
                if index > 0 {
                    total = match total.checked_add(data.len()) {
                        Some(n) => n,
                        None => return exc::raise_with_message(OVERFLOW_ERROR, "integer overflow"),
                    };
                }
                total = match total.checked_add(part.len()) {
                    Some(n) if n <= isize::MAX as usize => n,
                    _ => return exc::raise_with_message(OVERFLOW_ERROR, "integer overflow"),
                };
            }
            let mut out = Vec::with_capacity(total);
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.extend_from_slice(&data);
                }
                if let Some(part) = value::as_bytes(*item) {
                    out.extend_from_slice(part);
                }
            }
            value::bytes_new(out)
        }
        "upper" | "lower" => {
            if let Some(err) = check_arity("bytes", method, args.len(), 0, 0) {
                return err;
            }
            let out = if method == "upper" {
                data.to_ascii_uppercase()
            } else {
                data.to_ascii_lowercase()
            };
            value::bytes_new(out)
        }
        _ => attribute_error(obj, method),
    }
}

// bytearray

/// Methods of a mutable `bytearray`.
///
/// Mutations go through [`value::bytearray_bytes_mut`]; the payload is never
/// borrowed across an allocation. Read-only transformations reuse
/// [`bytes_method`] and wrap a bytes result back into a `bytearray`, matching
/// CPython (`bytearray.replace` returns a `bytearray`, `split` a list of
/// them).
fn bytearray_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    match method {
        "__len__" => {
            if let Some(err) = check_arity("bytearray", method, args.len(), 0, 0) {
                return err;
            }
            ops::int_result(value::as_bytearray(obj).map_or(0, |bytes| bytes.len() as i64))
        }
        "__getitem__" => {
            if let Some(err) = check_arity("bytearray", method, args.len(), 1, 1) {
                return err;
            }
            container::subscript(obj, args[0])
        }
        "__setitem__" => {
            if let Some(err) = check_arity("bytearray", method, args.len(), 2, 2) {
                return err;
            }
            container::subscript_set(obj, args[0], args[1])
        }
        "__delitem__" => {
            if let Some(err) = check_arity("bytearray", method, args.len(), 1, 1) {
                return err;
            }
            container::delete_subscript(obj, args[0])
        }
        "__contains__" => {
            if let Some(err) = check_arity("bytearray", method, args.len(), 1, 1) {
                return err;
            }
            bytearray_contains(obj, args[0])
        }
        "append" => {
            if let Some(err) = check_arity("bytearray", method, args.len(), 1, 1) {
                return err;
            }
            let Some(byte) = byte_item(args[0]) else {
                return bytearray_item_error(args[0]);
            };
            match value::bytearray_bytes_mut(obj) {
                Some(bytes) => {
                    bytes.push(byte);
                    NONE
                }
                None => attribute_error(obj, method),
            }
        }
        "extend" => {
            if let Some(err) = check_arity("bytearray", method, args.len(), 1, 1) {
                return err;
            }
            let items = match builtins::star_sequence(args[0]) {
                Ok(items) => items,
                Err(err) => return err,
            };
            let mut appended = Vec::with_capacity(items.len());
            for item in items {
                let Some(byte) = byte_item(item) else {
                    return bytearray_item_error(item);
                };
                appended.push(byte);
            }
            match value::bytearray_bytes_mut(obj) {
                Some(bytes) => {
                    bytes.extend_from_slice(&appended);
                    NONE
                }
                None => attribute_error(obj, method),
            }
        }
        "insert" => {
            if let Some(err) = check_arity("bytearray", method, args.len(), 2, 2) {
                return err;
            }
            let Ok(raw) = int_arg(args[0]) else {
                return type_error(format!(
                    "'{}' object cannot be interpreted as an integer",
                    value::type_name(args[0])
                ));
            };
            let Some(byte) = byte_item(args[1]) else {
                return bytearray_item_error(args[1]);
            };
            match value::bytearray_bytes_mut(obj) {
                Some(bytes) => {
                    let len = bytes.len() as i64;
                    let index = if raw < 0 { (raw + len).max(0) } else { raw };
                    let position = (index as usize).min(bytes.len());
                    bytes.insert(position, byte);
                    NONE
                }
                None => attribute_error(obj, method),
            }
        }
        "pop" => {
            if let Some(err) = check_arity("bytearray", method, args.len(), 0, 1) {
                return err;
            }
            let Some(len) = value::as_bytearray(obj).map(|bytes| bytes.len()) else {
                return attribute_error(obj, method);
            };
            if len == 0 {
                return exc::raise_with_message(INDEX_ERROR, "pop from empty bytearray");
            }
            let raw = if args.is_empty() {
                -1
            } else {
                match int_arg(args[0]) {
                    Ok(raw) => raw,
                    Err(_) => {
                        return type_error(format!(
                            "'{}' object cannot be interpreted as an integer",
                            value::type_name(args[0])
                        ))
                    }
                }
            };
            let Some(position) = container::normalize_index(raw, len) else {
                return exc::raise_with_message(INDEX_ERROR, "pop index out of range");
            };
            match value::bytearray_bytes_mut(obj) {
                Some(bytes) => ops::int_result(bytes.remove(position) as i64),
                None => attribute_error(obj, method),
            }
        }
        "remove" => {
            if let Some(err) = check_arity("bytearray", method, args.len(), 1, 1) {
                return err;
            }
            let Some(byte) = byte_item(args[0]) else {
                return bytearray_item_error(args[0]);
            };
            match value::bytearray_bytes_mut(obj) {
                Some(bytes) => match bytes.iter().position(|&existing| existing == byte) {
                    Some(position) => {
                        bytes.remove(position);
                        NONE
                    }
                    None => exc::raise_with_message(VALUE_ERROR, "value not found in bytearray"),
                },
                None => attribute_error(obj, method),
            }
        }
        "reverse" => {
            if let Some(err) = check_arity("bytearray", method, args.len(), 0, 0) {
                return err;
            }
            if let Some(bytes) = value::bytearray_bytes_mut(obj) {
                bytes.reverse();
            }
            NONE
        }
        "clear" => {
            if let Some(err) = check_arity("bytearray", method, args.len(), 0, 0) {
                return err;
            }
            if let Some(bytes) = value::bytearray_bytes_mut(obj) {
                bytes.clear();
            }
            NONE
        }
        "copy" => {
            if let Some(err) = check_arity("bytearray", method, args.len(), 0, 0) {
                return err;
            }
            let copy = value::as_bytearray(obj).cloned().unwrap_or_default();
            value::bytearray_new(copy)
        }
        // Read-only byte-string methods; transformations return a bytearray.
        "decode" | "hex" | "count" | "find" | "startswith" | "endswith" => {
            let data = value::as_bytearray(obj).cloned().unwrap_or_default();
            bytes_method(obj, data, method, args)
        }
        "replace" | "split" | "strip" | "lstrip" | "rstrip" | "join" | "upper" | "lower" => {
            let data = value::as_bytearray(obj).cloned().unwrap_or_default();
            let result = bytes_method(obj, data, method, args);
            if exc::pending() {
                return result;
            }
            bytes_result_as_bytearray(result)
        }
        _ => attribute_error(obj, method),
    }
}

/// Decodes one `bytearray.append`/`extend`/`insert`/`remove` argument.
///
/// An integer must be in `0..=255`; a length-1 bytes-like value is accepted
/// like CPython's buffer-protocol path.
fn byte_item(v: i64) -> Option<u8> {
    if let Some(byte) = container::byte_value(v) {
        return Some(byte);
    }
    match value::as_bytes(v) {
        Some([single]) => Some(*single),
        _ => None,
    }
}

/// Reports the CPython error for an invalid `bytearray` item.
fn bytearray_item_error(v: i64) -> i64 {
    if value::as_integer(v).is_some() {
        exc::raise_with_message(VALUE_ERROR, "byte must be in range(0, 256)")
    } else if value::as_bytes(v).is_some() {
        type_error("bytearray item must be a byte or a bytes-like value".to_string())
    } else {
        type_error(format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(v)
        ))
    }
}

/// Wraps a bytes-method result back into `bytearray` (`split` returns a list
/// of bytearrays).
fn bytes_result_as_bytearray(result: i64) -> i64 {
    if let Some(bytes) = value::as_bytes(result) {
        return value::bytearray_new(bytes.to_vec());
    }
    if let Some(items) = value::as_list(result) {
        let source: Vec<i64> = items.to_vec();
        let output = value::list_new(Vec::new());
        for item in source {
            if let Some(bytes) = value::as_bytes(item) {
                let converted = value::bytearray_new(bytes.to_vec());
                container::list_append(output, converted);
            }
        }
        return output;
    }
    result
}

/// `x in bytearray`: an integer tests one byte, a bytes-like tests a
/// subsequence.
fn bytearray_contains(obj: i64, needle: i64) -> i64 {
    let Some(bytes) = value::as_bytearray(obj) else {
        return attribute_error(obj, "__contains__");
    };
    if let Some(single) = container::byte_value(needle) {
        return value::bool_value(bytes.contains(&single));
    }
    if let Some(pattern) = value::as_bytes(needle) {
        if pattern.is_empty() {
            return value::TRUE;
        }
        return value::bool_value(bytes.windows(pattern.len()).any(|window| window == pattern));
    }
    type_error(format!(
        "a bytes-like object is required, not '{}'",
        value::type_name(needle)
    ))
}

// memoryview

/// Methods and data attributes of a `memoryview`.
fn memoryview_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    let Some(view) = value::as_memoryview(obj) else {
        return attribute_error(obj, method);
    };
    if view.released {
        return exc::raise_with_message(VALUE_ERROR, value::MEMORYVIEW_RELEASED);
    }
    match method {
        "tobytes" => {
            if let Some(err) = check_arity("memoryview", method, args.len(), 0, 0) {
                return err;
            }
            match value::memoryview_bytes(view) {
                Ok(bytes) => value::bytes_new(bytes),
                Err(message) => container::memoryview_error(message),
            }
        }
        "tolist" => {
            if let Some(err) = check_arity("memoryview", method, args.len(), 0, 0) {
                return err;
            }
            let len = view.len();
            let output = value::list_new(Vec::new());
            for index in 0..len {
                match value::memoryview_get(view, index) {
                    Ok(item) => {
                        container::list_append(output, item);
                    }
                    Err(message) => return container::memoryview_error(message),
                }
            }
            output
        }
        "release" => {
            if let Some(err) = check_arity("memoryview", method, args.len(), 0, 0) {
                return err;
            }
            if let Some(state) = value::memoryview_mut(obj) {
                state.released = true;
                state.buffer = NONE;
                state.length = 0;
            }
            NONE
        }
        "cast" => {
            if let Some(err) = check_arity("memoryview", method, args.len(), 1, 1) {
                return err;
            }
            let format = match str_arg(method, 1, args[0]) {
                Ok(format) => format,
                Err(err) => return err,
            };
            let mut chars = format.chars();
            let code = match (chars.next(), chars.next()) {
                (Some(code), None) => code,
                _ => {
                    return exc::raise_with_message(
                        VALUE_ERROR,
                        "memoryview: destination format must be a single character",
                    )
                }
            };
            if !matches!(code, 'B' | 'b' | 'c') {
                return exc::raise_with_message(
                    crate::exc::NOT_IMPLEMENTED_ERROR,
                    &format!("memoryview: format '{code}' is not supported"),
                );
            }
            value::memoryview_with_window(view.buffer, view.offset, view.len(), view.stride, code)
        }
        "__len__" => {
            if let Some(err) = check_arity("memoryview", method, args.len(), 0, 0) {
                return err;
            }
            ops::int_result(view.len() as i64)
        }
        "__getitem__" => {
            if let Some(err) = check_arity("memoryview", method, args.len(), 1, 1) {
                return err;
            }
            container::subscript(obj, args[0])
        }
        "__setitem__" => {
            if let Some(err) = check_arity("memoryview", method, args.len(), 2, 2) {
                return err;
            }
            container::subscript_set(obj, args[0], args[1])
        }
        "__contains__" => {
            if let Some(err) = check_arity("memoryview", method, args.len(), 1, 1) {
                return err;
            }
            let needle = args[0];
            let len = view.len();
            for index in 0..len {
                match value::memoryview_get(view, index) {
                    Ok(item) => {
                        if container::equals(item, needle) {
                            return value::TRUE;
                        }
                    }
                    Err(message) => return container::memoryview_error(message),
                }
            }
            value::FALSE
        }
        _ => attribute_error(obj, method),
    }
}

// dict views

/// Methods of `dict_keys`/`dict_values`/`dict_items`.
fn dict_view_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    let Some(view) = value::as_dict_view(obj) else {
        return attribute_error(obj, method);
    };
    match method {
        "__len__" => {
            if let Some(err) = check_arity(view.kind.type_name(), method, args.len(), 0, 0) {
                return err;
            }
            ops::int_result(container::dict_view_len(view) as i64)
        }
        "__iter__" => {
            if let Some(err) = check_arity(view.kind.type_name(), method, args.len(), 0, 0) {
                return err;
            }
            value::iter_new(obj)
        }
        "__contains__" => {
            if let Some(err) = check_arity(view.kind.type_name(), method, args.len(), 1, 1) {
                return err;
            }
            if view.kind != value::DictViewKind::Values {
                if let Some(name) = container::unhashable_name(args[0]) {
                    return type_error(container::unhashable_message(name));
                }
            }
            value::bool_value(container::dict_view_contains(view, args[0]))
        }
        "isdisjoint" => {
            if let Some(err) = check_arity(view.kind.type_name(), method, args.len(), 1, 1) {
                return err;
            }
            let items = match builtins::star_sequence(args[0]) {
                Ok(items) => items,
                Err(err) => return err,
            };
            for item in items {
                if container::dict_view_contains(view, item) {
                    return value::FALSE;
                }
            }
            value::TRUE
        }
        _ => attribute_error(obj, method),
    }
}

// list

fn list_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    match method {
        "sort" => {
            if let Some(err) = check_arity("list", method, args.len(), 0, 2) {
                return err;
            }
            let key = args.first().copied().unwrap_or(NONE);
            let reverse = args.get(1).copied().unwrap_or(value::FALSE);
            let sorted = crate::builtins::sorted(3, [obj, key, reverse].as_ptr());
            if exc::pending() {
                return sorted;
            }
            let Some(source) = value::as_list(sorted).map(|items| items.to_vec()) else {
                return type_error("sort() internal result is not a list".to_string());
            };
            let Some(mut items) = value::list_items_mut(obj) else {
                return type_error("sort() receiver is not a list".to_string());
            };
            items.clear();
            for item in source {
                items.push(item);
                crate::heap::write_barrier_edge(obj, item);
            }
            NONE
        }
        "append" => {
            if let Some(err) = check_arity("list", method, args.len(), 1, 1) {
                return err;
            }
            if let Some(mut items) = value::list_items_mut(obj) {
                items.push(args[0]);
                crate::heap::write_barrier_edge(obj, args[0]);
            }
            NONE
        }
        "extend" => {
            if let Some(err) = check_arity("list", method, args.len(), 1, 1) {
                return err;
            }
            let source = args[0];
            // `xs.extend(xs)` must append the original snapshot once, not
            // iterate a list that is growing underneath the iterator.
            if source == obj {
                let snapshot = value::as_list(obj).unwrap_or(&[]).to_vec();
                if let Some(mut items) = value::list_items_mut(obj) {
                    for item in snapshot {
                        items.push(item);
                        crate::heap::write_barrier_edge(obj, item);
                    }
                }
                return NONE;
            }
            // Lists/tuples/sets/dicts keep their elements reachable while we
            // append; arbitrary iterables (generators in particular) may
            // produce a value that exists only in the loop local, so append it
            // immediately instead of collecting an unrooted Rust Vec.
            if let Some(items) = value::as_list(source)
                .or_else(|| value::as_tuple(source))
                .or_else(|| value::as_set(source))
            {
                let copied = items.to_vec();
                if let Some(mut target) = value::list_items_mut(obj) {
                    for item in copied {
                        target.push(item);
                        crate::heap::write_barrier_edge(obj, item);
                    }
                }
                return NONE;
            }
            if let Some(pairs) = value::as_dict(source) {
                let keys: Vec<i64> = pairs.iter().map(|(key, _)| *key).collect();
                if let Some(mut target) = value::list_items_mut(obj) {
                    for item in keys {
                        target.push(item);
                        crate::heap::write_barrier_edge(obj, item);
                    }
                }
                return NONE;
            }
            let iterator = crate::iter::iter_new(source);
            if exc::pending() {
                return NONE;
            }
            loop {
                let mut item = NONE;
                let produced = crate::iter::iter_next(iterator, &mut item);
                if exc::pending() {
                    return NONE;
                }
                if produced == 0 {
                    break;
                }
                if let Some(mut target) = value::list_items_mut(obj) {
                    target.push(item);
                    crate::heap::write_barrier_edge(obj, item);
                }
            }
            NONE
        }
        "insert" => {
            if let Some(err) = check_arity("list", method, args.len(), 2, 2) {
                return err;
            }
            let index = match int_arg(args[0]) {
                Ok(n) => n,
                Err(err) => return err,
            };
            let input = args[1];
            if let Some(mut items) = value::list_items_mut(obj) {
                let len = items.len() as i64;
                // Python clamps instead of rejecting out-of-range positions.
                let position = if index < 0 {
                    index.checked_add(len).unwrap_or(0).max(0)
                } else {
                    index.min(len)
                };
                items.insert(position as usize, input);
                crate::heap::write_barrier_edge(obj, input);
            }
            NONE
        }
        "remove" => {
            if let Some(err) = check_arity("list", method, args.len(), 1, 1) {
                return err;
            }
            let target = args[0];
            if let Some(mut items) = value::list_items_mut(obj) {
                match list_find(&items, target) {
                    Some(position) => {
                        items.remove(position);
                        NONE
                    }
                    None => {
                        // Drop the graph lock before allocating the exception.
                        drop(items);
                        exc::raise_with_message(VALUE_ERROR, "list.remove(x): x not in list")
                    }
                }
            } else {
                attribute_error(obj, method)
            }
        }
        "pop" => {
            if let Some(err) = check_arity("list", method, args.len(), 0, 1) {
                return err;
            }
            let index = if args.len() == 1 {
                match int_arg(args[0]) {
                    Ok(n) => Some(n),
                    Err(err) => return err,
                }
            } else {
                None
            };
            let mut items = match value::list_items_mut(obj) {
                Some(items) => items,
                None => return attribute_error(obj, method),
            };
            if items.is_empty() {
                // Drop the graph lock before allocating the exception.
                drop(items);
                return exc::raise_with_message(INDEX_ERROR, "pop from empty list");
            }
            let popped = match index {
                None => Some(items.pop().unwrap_or(NONE)),
                Some(raw) => container::normalize_index(raw, items.len())
                    .map(|position| items.remove(position)),
            };
            drop(items);
            match popped {
                Some(item) => item,
                None => exc::raise_with_message(INDEX_ERROR, "pop index out of range"),
            }
        }
        "clear" => {
            if let Some(err) = check_arity("list", method, args.len(), 0, 0) {
                return err;
            }
            if let Some(mut items) = value::list_items_mut(obj) {
                items.clear();
            }
            NONE
        }
        "reverse" => {
            if let Some(err) = check_arity("list", method, args.len(), 0, 0) {
                return err;
            }
            if let Some(mut items) = value::list_items_mut(obj) {
                items.reverse();
            }
            NONE
        }
        "copy" => {
            if let Some(err) = check_arity("list", method, args.len(), 0, 0) {
                return err;
            }
            let copied: Vec<i64> = value::as_list(obj).unwrap_or(&[]).to_vec();
            value::list_new(copied)
        }
        "count" => {
            if let Some(err) = check_arity("list", method, args.len(), 1, 1) {
                return err;
            }
            let target = args[0];
            let n = value::as_list(obj)
                .unwrap_or(&[])
                .iter()
                .filter(|&&item| container::equals(item, target))
                .count();
            ops::int_result(n as i64)
        }
        "index" => {
            if let Some(err) = check_arity("list", method, args.len(), 1, 1) {
                return err;
            }
            let target = args[0];
            match value::as_list(obj).and_then(|items| list_find(items, target)) {
                Some(position) => ops::int_result(position as i64),
                None => exc::raise_with_message(
                    VALUE_ERROR,
                    &format!("{} is not in list", value::repr(target)),
                ),
            }
        }
        _ => attribute_error(obj, method),
    }
}

/// First position of `target` under Python equality.
fn list_find(items: &[i64], target: i64) -> Option<usize> {
    items
        .iter()
        .position(|&item| container::equals(item, target))
}

// tuple

fn tuple_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    match method {
        "count" => {
            if let Some(err) = check_arity("tuple", method, args.len(), 1, 1) {
                return err;
            }
            let target = args[0];
            let n = value::as_tuple(obj)
                .unwrap_or(&[])
                .iter()
                .filter(|&&item| container::equals(item, target))
                .count();
            ops::int_result(n as i64)
        }
        "index" => {
            if let Some(err) = check_arity("tuple", method, args.len(), 1, 1) {
                return err;
            }
            let target = args[0];
            match value::as_tuple(obj).and_then(|items| list_find(items, target)) {
                Some(position) => ops::int_result(position as i64),
                None => exc::raise_with_message(VALUE_ERROR, "tuple.index(x): x not in tuple"),
            }
        }
        _ => attribute_error(obj, method),
    }
}

// set

/// `s.issuperset(other)` / `s.issubset(other)` for iterable operands.
fn set_relation(obj: i64, other: i64, superset: bool) -> i64 {
    let Some(items) = value::as_set(obj) else {
        return attribute_error(obj, "issuperset");
    };
    let receiver = items.to_vec();
    if superset {
        match iterable_elements(other) {
            Ok(elements) => {
                if elements
                    .into_iter()
                    .all(|item| container::set_contains(&receiver, item))
                {
                    value::TRUE
                } else {
                    value::FALSE
                }
            }
            Err(err) => err,
        }
    } else {
        let other_items = match value::as_set(other)
            .or_else(|| value::as_list(other))
            .or_else(|| value::as_tuple(other))
        {
            Some(items) => items.to_vec(),
            None => {
                return type_error(format!(
                    "'{}' object is not iterable",
                    value::type_name(other)
                ))
            }
        };
        if receiver
            .iter()
            .all(|item| container::set_contains(&other_items, *item))
        {
            value::TRUE
        } else {
            value::FALSE
        }
    }
}

/// Extracts iteration values for set relations. Characters of a string and
/// byte values are materialised one at a time; each temporary stays live on
/// the caller's stack while the membership check runs.
fn iterable_elements(value: i64) -> Result<Vec<i64>, i64> {
    if let Some(items) = value::as_list(value)
        .or_else(|| value::as_tuple(value))
        .or_else(|| value::as_set(value))
    {
        return Ok(items.to_vec());
    }
    if let Some(text) = value::as_str(value) {
        return Ok(text
            .chars()
            .map(|ch| value::str_alloc(ch.to_string()))
            .collect());
    }
    if let Some(bytes) = value::as_bytes(value) {
        return Ok(bytes
            .iter()
            .map(|byte| crate::ops::int_result(*byte as i64))
            .collect());
    }
    if let Some(pairs) = value::as_dict(value) {
        return Ok(pairs.iter().map(|(key, _)| *key).collect());
    }
    Err(type_error(format!(
        "'{}' object is not iterable",
        value::type_name(value)
    )))
}

fn set_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    match method {
        "add" => {
            if let Some(err) = check_arity("set", method, args.len(), 1, 1) {
                return err;
            }
            let item = args[0];
            if let Some(err) = check_hashable(item) {
                return err;
            }
            container::insert_set_item(obj, item);
            NONE
        }
        "remove" | "discard" => {
            if let Some(err) = check_arity("set", method, args.len(), 1, 1) {
                return err;
            }
            let item = args[0];
            if let Some(err) = check_hashable(item) {
                return err;
            }
            let position = value::as_set(obj).and_then(|items| container::set_find(items, item));
            match position {
                Some(index) => {
                    if let Some(mut items) = value::set_items_mut(obj) {
                        items.remove(index);
                    }
                    NONE
                }
                None if method == "discard" => NONE,
                // Python's `KeyError` renders the missing element's repr.
                None => exc::raise_with_message(KEY_ERROR, &value::repr(item)),
            }
        }
        "clear" => {
            if let Some(err) = check_arity("set", method, args.len(), 0, 0) {
                return err;
            }
            if let Some(mut items) = value::set_items_mut(obj) {
                items.clear();
            }
            NONE
        }
        "copy" => {
            if let Some(err) = check_arity("set", method, args.len(), 0, 0) {
                return err;
            }
            let result = value::set_new(Vec::new());
            // Read the receiver after allocating so it stays a root.
            if let Some(items) = value::as_set(obj) {
                for &item in items {
                    container::insert_set_item(result, item);
                }
            }
            result
        }
        "issuperset" | "issubset" => {
            if let Some(err) = check_arity("set", method, args.len(), 1, 1) {
                return err;
            }
            set_relation(obj, args[0], method == "issuperset")
        }
        "union" => {
            if let Some(err) = check_arity("set", method, args.len(), 1, 1) {
                return err;
            }
            let other = args[0];
            let supported = value::as_set(other).is_some()
                || value::as_list(other).is_some()
                || value::as_tuple(other).is_some();
            if !supported {
                return type_error(format!(
                    "'{}' object is not iterable",
                    value::type_name(other)
                ));
            }
            let invalid = if let Some(items) = value::as_set(other) {
                check_hashable_items(items)
            } else if let Some(items) = value::as_list(other) {
                check_hashable_items(items)
            } else {
                value::as_tuple(other).and_then(check_hashable_items)
            };
            if let Some(err) = invalid {
                return err;
            }
            // Allocate first; `obj` and `other` are read below and therefore
            // stay live roots across the allocation.
            let result = value::set_new(Vec::new());
            if let Some(items) = value::as_set(obj) {
                for &item in items {
                    container::insert_set_item(result, item);
                }
            }
            if let Some(items) = value::as_set(other) {
                for &item in items {
                    container::insert_set_item(result, item);
                }
            } else if let Some(items) = value::as_list(other) {
                for &item in items {
                    container::insert_set_item(result, item);
                }
            } else if let Some(items) = value::as_tuple(other) {
                for &item in items {
                    container::insert_set_item(result, item);
                }
            }
            result
        }
        _ => attribute_error(obj, method),
    }
}

// dict

fn dict_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    match method {
        "__getitem__" => {
            if let Some(err) = check_arity("dict", method, args.len(), 1, 1) {
                return err;
            }
            container::subscript(obj, args[0])
        }
        "__setitem__" => {
            if let Some(err) = check_arity("dict", method, args.len(), 2, 2) {
                return err;
            }
            container::subscript_set(obj, args[0], args[1])
        }
        "__delitem__" => {
            if let Some(err) = check_arity("dict", method, args.len(), 1, 1) {
                return err;
            }
            container::delete_subscript(obj, args[0])
        }
        "__contains__" => {
            if let Some(err) = check_arity("dict", method, args.len(), 1, 1) {
                return err;
            }
            if let Some(err) = check_hashable(args[0]) {
                return err;
            }
            let found = value::as_dict_data(obj).is_some_and(|dict| dict.find(args[0]).is_some());
            value::bool_value(found)
        }
        "__len__" => {
            if let Some(err) = check_arity("dict", method, args.len(), 0, 0) {
                return err;
            }
            crate::ops::int_result(value::as_dict(obj).map_or(0, |pairs| pairs.len() as i64))
        }
        "get" => {
            if let Some(err) = check_arity("dict", method, args.len(), 1, 2) {
                return err;
            }
            if let Some(err) = check_hashable(args[0]) {
                return err;
            }
            let found = value::as_dict_data(obj).and_then(|dict| {
                dict.find(args[0])
                    .map(|position| dict.entries()[position].1)
            });
            match found {
                Some(value) => value,
                None => {
                    if args.len() == 2 {
                        args[1]
                    } else {
                        NONE
                    }
                }
            }
        }
        "setdefault" => {
            if let Some(err) = check_arity("dict", method, args.len(), 1, 2) {
                return err;
            }
            if let Some(err) = check_hashable(args[0]) {
                return err;
            }
            let key = args[0];
            let default = args.get(1).copied().unwrap_or(NONE);
            if let Some(position) = value::as_dict_data(obj).and_then(|dict| dict.find(key)) {
                return value::as_dict_data(obj)
                    .map(|dict| dict.entries()[position].1)
                    .unwrap_or(default);
            }
            if let Some(mut pairs) = value::dict_pairs_mut(obj) {
                container::dict_insert_barrier(obj, &mut pairs, key, default);
            }
            default
        }
        "keys" | "values" | "items" => {
            if let Some(err) = check_arity("dict", method, args.len(), 0, 0) {
                return err;
            }
            // Live views: no copy, no snapshot. The view observes the dict's
            // current contents for len/iteration/membership/repr.
            let kind = match method {
                "keys" => value::DictViewKind::Keys,
                "values" => value::DictViewKind::Values,
                _ => value::DictViewKind::Items,
            };
            value::dict_view_new(obj, kind)
        }
        "pop" => {
            if let Some(err) = check_arity("dict", method, args.len(), 1, 2) {
                return err;
            }
            if let Some(err) = check_hashable(args[0]) {
                return err;
            }
            let key = args[0];
            let mut pairs = match value::dict_pairs_mut(obj) {
                Some(pairs) => pairs,
                None => return attribute_error(obj, method),
            };
            match pairs.remove(key) {
                Some((_, value)) => value,
                None => {
                    // Drop the graph lock before allocating the exception.
                    drop(pairs);
                    if args.len() == 2 {
                        args[1]
                    } else {
                        // Python's `KeyError` renders the missing key's repr.
                        exc::raise_with_message(KEY_ERROR, &value::repr(key))
                    }
                }
            }
        }
        "update" => {
            if let Some(err) = check_arity("dict", method, args.len(), 1, 1) {
                return err;
            }
            if let Some(source) = value::as_dict(args[0]) {
                // Copy before mutating so `d.update(d)` cannot observe a moving
                // pair vector; existing keys keep their position, new ones
                // append.
                let source: Vec<(i64, i64)> = source.to_vec();
                if let Some(mut pairs) = value::dict_pairs_mut(obj) {
                    for (key, val) in source {
                        container::dict_insert_barrier(obj, &mut pairs, key, val);
                    }
                }
                return NONE;
            }
            dict_update_from_pairs(obj, args[0])
        }
        "copy" => {
            if let Some(err) = check_arity("dict", method, args.len(), 0, 0) {
                return err;
            }
            let pairs = value::as_dict(obj).unwrap_or(&[]).to_vec();
            value::dict_new(pairs)
        }
        "clear" => {
            if let Some(err) = check_arity("dict", method, args.len(), 0, 0) {
                return err;
            }
            if let Some(mut pairs) = value::dict_pairs_mut(obj) {
                pairs.clear();
            }
            NONE
        }
        _ => attribute_error(obj, method),
    }
}

/// `d.update(iterable)`: copies an iterable of key/value pairs into `d`.
///
/// CPython accepts any iterable whose elements are two-item sequences, so each
/// element must be a list or tuple of exactly two values; other shapes report
/// the same `TypeError`/`ValueError` wording as `dict(iterable)`. Keys must be
/// hashable, and pairs are inserted as they are read, so an iterator that
/// yields from `d`'s own items observes the updates made so far.
fn dict_update_from_pairs(obj: i64, source: i64) -> i64 {
    let iterator = crate::iter::iter_new(source);
    if exc::pending() {
        return NONE;
    }
    let mut index = 0usize;
    loop {
        let mut item = NONE;
        let status = crate::iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return NONE;
        }
        if status == 0 {
            break;
        }
        let Some(items) = value::as_list(item).or_else(|| value::as_tuple(item)) else {
            return type_error(format!(
                "cannot convert dictionary update sequence element #{} to a sequence",
                index
            ));
        };
        if items.len() != 2 {
            return exc::raise_with_message(
                VALUE_ERROR,
                &format!(
                    "dictionary update sequence element #{} has length {}; 2 is required",
                    index,
                    items.len()
                ),
            );
        }
        let (key, val) = (items[0], items[1]);
        if let Some(name) = container::unhashable_name(key) {
            return type_error(container::unhashable_message(name));
        }
        if let Some(mut pairs) = value::dict_pairs_mut(obj) {
            container::dict_insert_barrier(obj, &mut pairs, key, val);
        }
        index += 1;
    }
    NONE
}

/// Rejects dict keys that Python considers unhashable (`list`, `dict`,
/// `iterator`, a bytes-like mutable/view value, or a tuple containing one).
fn check_hashable(key: i64) -> Option<i64> {
    container::unhashable_name(key).map(|name| type_error(container::unhashable_message(name)))
}

/// Returns the pending `TypeError` for the first unhashable element.
fn check_hashable_items(items: &[i64]) -> Option<i64> {
    items.iter().find_map(|&item| check_hashable(item))
}

// lock (`_thread.allocate_lock()`)

/// Methods of a `_thread` lock. The object owns a `Mutex<bool>`/`Condvar`
/// pair, so a blocking `acquire()` waits on the condition variable and never
/// holds the heap mutex. While waiting it is parked for the stop-the-world
/// collector (via `heap::blocking_enter`/`blocking_leave`), so its live
/// values stay scannable and the collector never waits on an unaccounted
/// thread.
fn lock_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    let Some(lock) = value::as_lock(obj) else {
        return attribute_error(obj, method);
    };
    match method {
        "__enter__" => {
            if let Some(err) = check_arity("lock", method, args.len(), 0, 0) {
                return err;
            }
            lock.acquire(true);
            obj
        }
        "__exit__" => {
            if let Some(err) = check_arity("lock", method, args.len(), 3, 3) {
                return err;
            }
            if !lock.release() {
                return exc::raise_with_message(RUNTIME_ERROR, "release unlocked lock");
            }
            NONE
        }
        "acquire" => {
            if let Some(err) = check_arity("lock", method, args.len(), 0, 1) {
                return err;
            }
            // CPython's default is blocking; any truthy value means blocking,
            // which is what `lock.acquire(False)` relies on.
            let blocking = args.first().copied().map(value::truthy).unwrap_or(true);
            value::bool_value(lock.acquire(blocking))
        }
        "release" => {
            if let Some(err) = check_arity("lock", method, args.len(), 0, 0) {
                return err;
            }
            if lock.release() {
                NONE
            } else {
                let message = if lock.is_reentrant() {
                    "cannot release un-acquired lock"
                } else {
                    "release unlocked lock"
                };
                exc::raise_with_message(RUNTIME_ERROR, message)
            }
        }
        "locked" => {
            if let Some(err) = check_arity("lock", method, args.len(), 0, 0) {
                return err;
            }
            value::bool_value(lock.locked())
        }
        _ => attribute_error(obj, method),
    }
}

// threading.Thread / Event, queue.Queue, multiprocessing.Process / Queue

/// Parses an optional timeout: `None` stays `None`, numbers become a
/// non-negative duration.
fn timeout_argument(method: &str, v: i64) -> Result<Option<Duration>, i64> {
    if v == NONE {
        return Ok(None);
    }
    let seconds = match value::num_of(v) {
        Some(value::Num::Int(n)) => n as f64,
        Some(value::Num::Float(f)) => f,
        None => {
            return Err(type_error(format!(
                "{method}() timeout must be a number, not {}",
                value::type_name(v)
            )))
        }
    };
    if seconds.is_nan() || seconds < 0.0 {
        return Err(exc::raise_with_message(
            VALUE_ERROR,
            "'timeout' must be a non-negative number",
        ));
    }
    if !seconds.is_finite() || seconds >= MAX_TIMEOUT_SECONDS {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            "timeout is too large",
        ));
    }
    Ok(Some(Duration::from_secs_f64(seconds)))
}

/// Upper bound for a parsed timeout (~31 years). Keeps `Instant + Duration`
/// and `Duration::from_secs_f64` far away from their overflow limits.
const MAX_TIMEOUT_SECONDS: f64 = 1_000_000_000.0;

fn thread_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    let Some(thread) = value::as_thread(obj) else {
        return attribute_error(obj, method);
    };
    match method {
        "start" => {
            if let Some(err) = check_arity("Thread", method, args.len(), 0, 0) {
                return err;
            }
            if value::as_function(thread.target).is_none() {
                return type_error("thread target is not callable".to_string());
            }
            if thread.started.swap(true, Ordering::AcqRel) {
                return exc::raise_with_message(RUNTIME_ERROR, "threads can only be started once");
            }
            // Copy the owned inputs before spawning: the child registration
            // may switch the heap to shared mode, so no reference into the
            // Thread object is read across that call.
            let target = thread.target;
            let argv = thread.args.clone();
            let finished = thread.finished.clone();
            match modules::spawn_thread(target, argv, Some(finished)) {
                Ok(id) => {
                    // `obj` keeps the Thread object reachable; re-derive the
                    // reference instead of carrying one across the spawn.
                    if let Some(thread) = value::as_thread(obj) {
                        thread.id.store(id, Ordering::Release);
                    }
                    NONE
                }
                Err(message) => {
                    if let Some(thread) = value::as_thread(obj) {
                        thread.started.store(false, Ordering::Release);
                    }
                    exc::raise_with_message(RUNTIME_ERROR, &message)
                }
            }
        }
        "join" => {
            if let Some(err) = check_arity("Thread", method, args.len(), 0, 0) {
                return err;
            }
            if !thread.started.load(Ordering::Acquire) {
                return exc::raise_with_message(
                    RUNTIME_ERROR,
                    "cannot join thread before it is started",
                );
            }
            if thread.joined.load(Ordering::Acquire) {
                return NONE;
            }
            let id = thread.id.load(Ordering::Acquire);
            match modules::join_spawned(id) {
                Ok(()) => {
                    if let Some(thread) = value::as_thread(obj) {
                        thread.joined.store(true, Ordering::Release);
                    }
                    NONE
                }
                Err(message) => exc::raise_with_message(RUNTIME_ERROR, &message),
            }
        }
        "is_alive" => {
            if let Some(err) = check_arity("Thread", method, args.len(), 0, 0) {
                return err;
            }
            let alive = thread.started.load(Ordering::Acquire)
                && !thread.finished.load(Ordering::Acquire)
                && !thread.joined.load(Ordering::Acquire);
            value::bool_value(alive)
        }
        _ => attribute_error(obj, method),
    }
}

fn event_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    let Some(event) = value::as_event(obj) else {
        return attribute_error(obj, method);
    };
    match method {
        "set" | "clear" | "is_set" => {
            if let Some(err) = check_arity("Event", method, args.len(), 0, 0) {
                return err;
            }
            match method {
                "set" => {
                    event.set();
                    NONE
                }
                "clear" => {
                    event.clear();
                    NONE
                }
                _ => value::bool_value(event.is_set()),
            }
        }
        "wait" => {
            if let Some(err) = check_arity("Event", method, args.len(), 0, 1) {
                return err;
            }
            let timeout = match args.first().copied() {
                Some(arg) => match timeout_argument(method, arg) {
                    Ok(timeout) => timeout,
                    Err(err) => return err,
                },
                None => None,
            };
            value::bool_value(event.wait(timeout))
        }
        _ => attribute_error(obj, method),
    }
}

/// Maps a `queue.Queue` error to the CPython-visible `RuntimeError` message.
fn queue_error(error: value::QueueError) -> i64 {
    let message = match error {
        value::QueueError::Full => "queue.Full",
        value::QueueError::Empty => "queue.Empty",
    };
    exc::raise_with_message(RUNTIME_ERROR, message)
}

fn queue_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    let Some(queue) = value::as_queue(obj) else {
        return attribute_error(obj, method);
    };
    match method {
        "put" => {
            if let Some(err) = check_arity("Queue", method, args.len(), 1, 3) {
                return err;
            }
            let block = args.get(1).copied().map(value::truthy).unwrap_or(true);
            let timeout = match args.get(2).copied() {
                Some(arg) => match timeout_argument(method, arg) {
                    Ok(timeout) => timeout,
                    Err(err) => return err,
                },
                None => None,
            };
            match queue.put(args[0], block, timeout) {
                Ok(()) => NONE,
                Err(error) => queue_error(error),
            }
        }
        "put_nowait" => {
            if let Some(err) = check_arity("Queue", method, args.len(), 1, 1) {
                return err;
            }
            match queue.put(args[0], false, None) {
                Ok(()) => NONE,
                Err(error) => queue_error(error),
            }
        }
        "get" => {
            if let Some(err) = check_arity("Queue", method, args.len(), 0, 2) {
                return err;
            }
            let block = args.first().copied().map(value::truthy).unwrap_or(true);
            let timeout = match args.get(1).copied() {
                Some(arg) => match timeout_argument(method, arg) {
                    Ok(timeout) => timeout,
                    Err(err) => return err,
                },
                None => None,
            };
            match queue.get(block, timeout) {
                Ok(item) => item,
                Err(error) => queue_error(error),
            }
        }
        "get_nowait" => {
            if let Some(err) = check_arity("Queue", method, args.len(), 0, 0) {
                return err;
            }
            match queue.get(false, None) {
                Ok(item) => item,
                Err(error) => queue_error(error),
            }
        }
        "qsize" => {
            if let Some(err) = check_arity("Queue", method, args.len(), 0, 0) {
                return err;
            }
            ops::int_result(queue.qsize() as i64)
        }
        "empty" => {
            if let Some(err) = check_arity("Queue", method, args.len(), 0, 0) {
                return err;
            }
            value::bool_value(queue.empty())
        }
        "full" => {
            if let Some(err) = check_arity("Queue", method, args.len(), 0, 0) {
                return err;
            }
            value::bool_value(queue.full())
        }
        _ => attribute_error(obj, method),
    }
}

fn process_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    let Some(process) = value::as_process(obj) else {
        return attribute_error(obj, method);
    };
    match method {
        "start" => {
            if let Some(err) = check_arity("Process", method, args.len(), 0, 0) {
                return err;
            }
            if value::as_function(process.target).is_none() {
                return type_error("process target is not callable".to_string());
            }
            match process::start_process(process) {
                Ok(()) => NONE,
                Err(message) => exc::raise_with_message(RUNTIME_ERROR, &message),
            }
        }
        "join" => {
            if let Some(err) = check_arity("Process", method, args.len(), 0, 0) {
                return err;
            }
            if !process.started.load(Ordering::Acquire) {
                return exc::raise_with_message(
                    RUNTIME_ERROR,
                    "cannot join process before it is started",
                );
            }
            if !process.joined.load(Ordering::Acquire) {
                process::wait_for_process(process);
                if let Some(process) = value::as_process(obj) {
                    process.joined.store(true, Ordering::Release);
                }
            }
            NONE
        }
        "terminate" => {
            if let Some(err) = check_arity("Process", method, args.len(), 0, 0) {
                return err;
            }
            match process::terminate_process(process) {
                Ok(()) => NONE,
                Err(message) => exc::raise_with_message(RUNTIME_ERROR, &message),
            }
        }
        "is_alive" => {
            if let Some(err) = check_arity("Process", method, args.len(), 0, 0) {
                return err;
            }
            value::bool_value(process::process_alive(process))
        }
        "exitcode" => {
            if let Some(err) = check_arity("Process", method, args.len(), 0, 0) {
                return err;
            }
            match process::process_exitcode(process) {
                Some(code) => ops::int_result(code),
                None => NONE,
            }
        }
        _ => attribute_error(obj, method),
    }
}

/// Maps a cross-process queue error to a pending exception.
fn mp_error(putting: bool, error: process::MpError) -> i64 {
    use crate::process::MpError;
    match error {
        MpError::Full => exc::raise_with_message(RUNTIME_ERROR, "queue.Full"),
        MpError::Empty => exc::raise_with_message(RUNTIME_ERROR, "queue.Empty"),
        MpError::Closed => exc::raise_with_message(RUNTIME_ERROR, "queue is closed"),
        MpError::Io(message) => exc::raise_with_message(RUNTIME_ERROR, &message),
        MpError::Pickle(error) => {
            let message = if putting {
                error.dumps_message()
            } else {
                error.loads_message()
            };
            let tag = match error {
                crate::serialize::PickleError::Unsupported(_) => TYPE_ERROR,
                _ => VALUE_ERROR,
            };
            exc::raise_with_message(tag, &message)
        }
    }
}

fn mp_queue_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    let Some(queue) = value::as_mp_queue(obj) else {
        return attribute_error(obj, method);
    };
    match method {
        "put" => {
            if let Some(err) = check_arity("Queue", method, args.len(), 1, 3) {
                return err;
            }
            let block = args.get(1).copied().map(value::truthy).unwrap_or(true);
            let timeout = match args.get(2).copied() {
                Some(arg) => match timeout_argument(method, arg) {
                    Ok(timeout) => timeout,
                    Err(err) => return err,
                },
                None => None,
            };
            match process::queue_put(queue, args[0], block, timeout) {
                Ok(()) => NONE,
                Err(error) => mp_error(true, error),
            }
        }
        "put_nowait" => {
            if let Some(err) = check_arity("Queue", method, args.len(), 1, 1) {
                return err;
            }
            match process::queue_put(queue, args[0], false, None) {
                Ok(()) => NONE,
                Err(error) => mp_error(true, error),
            }
        }
        "get" => {
            if let Some(err) = check_arity("Queue", method, args.len(), 0, 2) {
                return err;
            }
            let block = args.first().copied().map(value::truthy).unwrap_or(true);
            let timeout = match args.get(1).copied() {
                Some(arg) => match timeout_argument(method, arg) {
                    Ok(timeout) => timeout,
                    Err(err) => return err,
                },
                None => None,
            };
            match process::queue_get(queue, block, timeout) {
                Ok(item) => item,
                Err(error) => mp_error(false, error),
            }
        }
        "get_nowait" => {
            if let Some(err) = check_arity("Queue", method, args.len(), 0, 0) {
                return err;
            }
            match process::queue_get(queue, false, None) {
                Ok(item) => item,
                Err(error) => mp_error(false, error),
            }
        }
        // `qsize`/`empty`/`full` need a cross-process item count, which the
        // metadata-only `maxsize` cannot provide: they are not exposed.
        _ => attribute_error(obj, method),
    }
}

fn pool_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    let result = match method {
        "__enter__" => {
            if let Some(err) = check_arity("Pool", method, args.len(), 0, 0) {
                return err;
            }
            return obj;
        }
        "__exit__" => {
            if let Some(err) = check_arity("Pool", method, args.len(), 3, 3) {
                return err;
            }
            // `with Pool(...)` tears the workers down without suppressing the
            // exception (if any).
            return match pool::terminate(obj) {
                Ok(()) => NONE,
                Err(message) => exc::raise_with_message(RUNTIME_ERROR, &message),
            };
        }
        "map" => {
            if let Some(err) = check_arity("Pool", method, args.len(), 2, 2) {
                return err;
            }
            pool::map(obj, args[0], args[1])
        }
        "apply" => {
            if let Some(err) = check_arity("Pool", method, args.len(), 1, 2) {
                return err;
            }
            let argv = args.get(1).copied().unwrap_or(NONE);
            pool::apply(obj, args[0], argv)
        }
        "close" => {
            if let Some(err) = check_arity("Pool", method, args.len(), 0, 0) {
                return err;
            }
            return match pool::close(obj) {
                Ok(()) => NONE,
                Err(message) => exc::raise_with_message(RUNTIME_ERROR, &message),
            };
        }
        "join" => {
            if let Some(err) = check_arity("Pool", method, args.len(), 0, 0) {
                return err;
            }
            return match pool::join(obj) {
                Ok(()) => NONE,
                Err(message) => exc::raise_with_message(RUNTIME_ERROR, &message),
            };
        }
        "terminate" => {
            if let Some(err) = check_arity("Pool", method, args.len(), 0, 0) {
                return err;
            }
            return match pool::terminate(obj) {
                Ok(()) => NONE,
                Err(message) => exc::raise_with_message(RUNTIME_ERROR, &message),
            };
        }
        _ => return attribute_error(obj, method),
    };
    match result {
        Ok(value) => value,
        Err(message) => exc::raise_with_message(RUNTIME_ERROR, &message),
    }
}

/// Methods of the native standard streams.
///
/// `write(text)` appends the string and flushes (so output interleaves with
/// `print`), returning the number of characters; `flush()` flushes; `isatty()`
/// reports whether the descriptor is a terminal; `fileno()` returns the
/// descriptor (0, 1 or 2). `readline()` and `read(n=-1)` read text from
/// `std::io::stdin` and exist only on `sys.stdin`; writing to `sys.stdin` and
/// reading from the output streams report `OSError`, like CPython's
/// `io.UnsupportedOperation`. Standard streams are never closed and expose no
/// other attributes.
fn std_stream_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    let Some(stream) = value::as_std_stream(obj) else {
        return attribute_error(obj, method);
    };
    match method {
        "write" => {
            if let Some(err) = check_arity(stream.name(), method, args.len(), 1, 1) {
                return err;
            }
            let Some(text) = value::as_str(args[0]) else {
                return type_error(format!(
                    "write() argument must be str, not {}",
                    value::type_name(args[0])
                ));
            };
            if stream.fd() == 0 {
                return exc::raise_with_message(OS_ERROR, "write: stream is not writable");
            }
            match write_std_stream(stream.fd(), text) {
                Ok(count) => ops::int_result(count as i64),
                Err(error) => exc::raise_with_message(OS_ERROR, &format!("write: {error}")),
            }
        }
        "flush" => {
            if let Some(err) = check_arity(stream.name(), method, args.len(), 0, 0) {
                return err;
            }
            match flush_std_stream(stream.fd()) {
                Ok(()) => NONE,
                Err(error) => exc::raise_with_message(OS_ERROR, &format!("flush: {error}")),
            }
        }
        "isatty" => {
            if let Some(err) = check_arity(stream.name(), method, args.len(), 0, 0) {
                return err;
            }
            value::bool_value(std_stream_is_tty(stream.fd()))
        }
        "fileno" => {
            if let Some(err) = check_arity(stream.name(), method, args.len(), 0, 0) {
                return err;
            }
            ops::int_result(i64::from(stream.fd()))
        }
        "readline" => {
            if let Some(err) = check_arity(stream.name(), method, args.len(), 0, 0) {
                return err;
            }
            if stream.fd() != 0 {
                return exc::raise_with_message(OS_ERROR, "readline: stream is not readable");
            }
            match read_std_stream_line() {
                Ok(text) => value::str_alloc(text),
                Err(error) => exc::raise_with_message(OS_ERROR, &format!("readline: {error}")),
            }
        }
        "read" => {
            if let Some(err) = check_arity(stream.name(), method, args.len(), 0, 1) {
                return err;
            }
            let size = match args.first() {
                None => -1,
                Some(arg) => match value::as_integer(*arg) {
                    Some(size) => size,
                    None => {
                        return type_error(format!(
                            "read() argument must be an integer, not {}",
                            value::type_name(*arg)
                        ))
                    }
                },
            };
            if stream.fd() != 0 {
                return exc::raise_with_message(OS_ERROR, "read: stream is not readable");
            }
            match read_std_stream(size) {
                Ok(text) => value::str_alloc(text),
                Err(error) => exc::raise_with_message(OS_ERROR, &format!("read: {error}")),
            }
        }
        _ => attribute_error(obj, method),
    }
}

/// Reads one line (including its newline, if any) from `std::io::stdin`.
fn read_std_stream_line() -> std::io::Result<String> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut bytes = Vec::new();
    input.read_until(b'\n', &mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Reads text from `std::io::stdin`.
///
/// A negative `size` reads everything up to EOF (Python's `read()` default);
/// a non-negative `size` bounds the number of bytes consumed. Invalid UTF-8 is
/// replaced rather than reported, so a size that splits a character cannot
/// fail.
fn read_std_stream(size: i64) -> std::io::Result<String> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut bytes = Vec::new();
    if size < 0 {
        input.read_to_end(&mut bytes)?;
    } else {
        input.take(size as u64).read_to_end(&mut bytes)?;
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Writes `text` to fd 1/2 and flushes it; returns the character count.
fn write_std_stream(fd: i32, text: &str) -> std::io::Result<usize> {
    if fd == 2 {
        let stderr = std::io::stderr();
        let mut out = stderr.lock();
        out.write_all(text.as_bytes())?;
        out.flush()?;
    } else {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        out.write_all(text.as_bytes())?;
        out.flush()?;
    }
    Ok(text.chars().count())
}

fn flush_std_stream(fd: i32) -> std::io::Result<()> {
    match fd {
        0 => Ok(()),
        2 => std::io::stderr().flush(),
        _ => std::io::stdout().flush(),
    }
}

fn std_stream_is_tty(fd: i32) -> bool {
    match fd {
        0 => std::io::stdin().is_terminal(),
        2 => std::io::stderr().is_terminal(),
        _ => std::io::stdout().is_terminal(),
    }
}

fn file_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    let Some(file) = value::as_file(obj) else {
        return attribute_error(obj, method);
    };
    match method {
        "__enter__" => {
            if let Some(err) = check_arity("file", method, args.len(), 0, 0) {
                return err;
            }
            obj
        }
        "__exit__" => {
            if let Some(err) = check_arity("file", method, args.len(), 3, 3) {
                return err;
            }
            // The context-manager protocol never suppresses exceptions, so
            // the three exception arguments are ignored.
            file.close();
            NONE
        }
        "close" => {
            if let Some(err) = check_arity("file", method, args.len(), 0, 0) {
                return err;
            }
            // Idempotent: closing a closed file is a no-op.
            file.close();
            NONE
        }
        "closed" => {
            if let Some(err) = check_arity("file", method, args.len(), 0, 0) {
                return err;
            }
            value::bool_value(file.closed())
        }
        "read" => {
            if let Some(err) = check_arity("file", method, args.len(), 0, 1) {
                return err;
            }
            let limit = match file_count(method, args.first().copied()) {
                Ok(limit) => limit,
                Err(err) => return err,
            };
            if file.binary() {
                match file.read_bytes(limit) {
                    Ok(data) => value::bytes_new(data),
                    Err(error) => value::raise_file_error(error),
                }
            } else {
                match file.read(limit) {
                    Ok(text) => value::str_alloc(text),
                    Err(error) => value::raise_file_error(error),
                }
            }
        }
        "readline" => {
            if let Some(err) = check_arity("file", method, args.len(), 0, 1) {
                return err;
            }
            let limit = match file_count(method, args.first().copied()) {
                Ok(limit) => limit,
                Err(err) => return err,
            };
            if file.binary() {
                match file.readline_bytes(limit) {
                    // `readline` returns b"" at end of file, unlike iteration.
                    Ok(line) => value::bytes_new(line.unwrap_or_default()),
                    Err(error) => value::raise_file_error(error),
                }
            } else {
                match file.readline(limit) {
                    // `readline` returns "" at end of file, unlike iteration.
                    Ok(line) => value::str_alloc(line.unwrap_or_default()),
                    Err(error) => value::raise_file_error(error),
                }
            }
        }
        "readlines" => {
            if let Some(err) = check_arity("file", method, args.len(), 0, 0) {
                return err;
            }
            // Build the result incrementally: the list must be reachable
            // before the per-line values are allocated.
            if file.binary() {
                let lines = match file.readlines_bytes() {
                    Ok(lines) => lines,
                    Err(error) => return value::raise_file_error(error),
                };
                let result = value::list_new(Vec::new());
                for line in lines {
                    let item = value::bytes_new(line);
                    container::list_append(result, item);
                    if exc::pending() {
                        return NONE;
                    }
                }
                result
            } else {
                let lines = match file.readlines() {
                    Ok(lines) => lines,
                    Err(error) => return value::raise_file_error(error),
                };
                let result = value::list_new(Vec::new());
                for line in lines {
                    let item = value::str_alloc(line);
                    container::list_append(result, item);
                    if exc::pending() {
                        return NONE;
                    }
                }
                result
            }
        }
        "write" => {
            if let Some(err) = check_arity("file", method, args.len(), 1, 1) {
                return err;
            }
            if file.binary() {
                let Some(data) = value::as_bytes(args[0]) else {
                    return type_error(format!(
                        "write() argument must be bytes, not {}",
                        value::type_name(args[0])
                    ));
                };
                match file.write_bytes(data) {
                    Ok(count) => ops::int_result(i64::try_from(count).unwrap_or(value::MAX_INT)),
                    Err(error) => value::raise_file_error(error),
                }
            } else {
                let text = match str_arg(method, 1, args[0]) {
                    Ok(text) => text,
                    Err(err) => return err,
                };
                match file.write(&text) {
                    Ok(count) => ops::int_result(i64::try_from(count).unwrap_or(value::MAX_INT)),
                    Err(error) => value::raise_file_error(error),
                }
            }
        }
        "writelines" => {
            if let Some(err) = check_arity("file", method, args.len(), 1, 1) {
                return err;
            }
            if file.binary() {
                let lines = match file_byte_lines(args[0]) {
                    Ok(lines) => lines,
                    Err(err) => return err,
                };
                for line in lines {
                    if let Err(error) = file.write_bytes(&line) {
                        return value::raise_file_error(error);
                    }
                }
            } else {
                let lines = match file_lines(args[0]) {
                    Ok(lines) => lines,
                    Err(err) => return err,
                };
                for line in lines {
                    if let Err(error) = file.write(&line) {
                        return value::raise_file_error(error);
                    }
                }
            }
            NONE
        }
        "flush" => {
            if let Some(err) = check_arity("file", method, args.len(), 0, 0) {
                return err;
            }
            match file.flush() {
                Ok(()) => NONE,
                Err(error) => value::raise_file_error(error),
            }
        }
        "tell" => {
            if let Some(err) = check_arity("file", method, args.len(), 0, 0) {
                return err;
            }
            match file.tell() {
                Ok(position) => ops::int_result(i64::try_from(position).unwrap_or(value::MAX_INT)),
                Err(error) => value::raise_file_error(error),
            }
        }
        "seek" => {
            if let Some(err) = check_arity("file", method, args.len(), 1, 2) {
                return err;
            }
            let offset = match int_arg(args[0]) {
                Ok(offset) => offset,
                Err(err) => return err,
            };
            let whence = match args.get(1).copied() {
                None | Some(NONE) => 0,
                Some(arg) => match int_arg(arg) {
                    Ok(whence) => whence,
                    Err(err) => return err,
                },
            };
            match file.seek(offset, whence) {
                Ok(()) => NONE,
                Err(error) => value::raise_file_error(error),
            }
        }
        _ => attribute_error(obj, method),
    }
}

/// Reads the optional count argument of `read`/`readline`: characters in text
/// mode, bytes in binary mode. `None` (or a negative integer) means "read all
/// / no limit".
fn file_count(method: &str, arg: Option<i64>) -> Result<Option<i64>, i64> {
    match arg {
        None | Some(NONE) => Ok(None),
        Some(v) => match value::as_integer(v) {
            Some(n) => Ok(Some(n)),
            None => Err(type_error(format!(
                "{method}() argument must be an integer, not {}",
                value::type_name(v)
            ))),
        },
    }
}

/// Materialises the argument of `writelines` as owned strings. Lists, tuples
/// and sets are the only iterables the runtime exposes generically; a `str`
/// yields its characters (each a one-character string), like CPython.
fn file_lines(v: i64) -> Result<Vec<String>, i64> {
    if let Some(text) = value::as_str(v) {
        return Ok(text.chars().map(|ch| ch.to_string()).collect());
    }
    let items = value::as_list(v)
        .or_else(|| value::as_tuple(v))
        .or_else(|| value::as_set(v));
    let Some(items) = items else {
        return Err(type_error(format!(
            "writelines() argument must be an iterable of strings, not {}",
            value::type_name(v)
        )));
    };
    let mut lines = Vec::with_capacity(items.len());
    for &item in items {
        let Some(text) = value::as_str(item) else {
            return Err(type_error(format!(
                "writelines() argument must be an iterable of strings, not {}",
                value::type_name(item)
            )));
        };
        lines.push(text.to_string());
    }
    Ok(lines)
}

/// Materialises the argument of `writelines` for a binary file. Lists, tuples
/// and sets of `bytes` are the supported iterables; strings and individual
/// `str` elements are rejected, like CPython.
fn file_byte_lines(v: i64) -> Result<Vec<Vec<u8>>, i64> {
    let items = value::as_list(v)
        .or_else(|| value::as_tuple(v))
        .or_else(|| value::as_set(v));
    let Some(items) = items else {
        return Err(type_error(format!(
            "writelines() argument must be an iterable of bytes, not {}",
            value::type_name(v)
        )));
    };
    let mut lines = Vec::with_capacity(items.len());
    for &item in items {
        let Some(data) = value::as_bytes(item) else {
            return Err(type_error(format!(
                "writelines() argument must be an iterable of bytes, not {}",
                value::type_name(item)
            )));
        };
        lines.push(data.to_vec());
    }
    Ok(lines)
}

// pathlib.Path

/// Dynamic `pathlib.Path` method dispatch.
///
/// The attribute-like accessors (`name`, `stem`, `suffix`, `parent`, `path`)
/// are also exposed as data attributes through [`getattr`]; the zero-argument
/// methods stay available for backwards compatibility.
fn path_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    let Some(state) = value::as_path(obj) else {
        return attribute_error(obj, method);
    };
    // Copy the receiver's path before any allocation: a reference into a heap
    // object must not be read after an allocation that may collect.
    let path = state.path().to_string();
    match method {
        "exists" | "is_file" | "is_dir" => {
            if let Some(err) = check_arity("Path", method, args.len(), 0, 0) {
                return err;
            }
            let target = fs_path(&path);
            let result = match method {
                "exists" => target.exists(),
                "is_file" => target.is_file(),
                _ => target.is_dir(),
            };
            value::bool_value(result)
        }
        "name" | "stem" | "suffix" => {
            if let Some(err) = check_arity("Path", method, args.len(), 0, 0) {
                return err;
            }
            let name = path_name(&path);
            let text = match method {
                "name" => name,
                "stem" => path_stem(&name),
                _ => path_suffix(&name),
            };
            value::str_alloc(text)
        }
        "parent" => {
            if let Some(err) = check_arity("Path", method, args.len(), 0, 0) {
                return err;
            }
            value::path_new(path_parent(&path))
        }
        "joinpath" => {
            let mut joined = path;
            for &arg in args {
                let Some(part) = value::as_path_like(arg) else {
                    return type_error(format!(
                        "joinpath() argument must be str or Path, not {}",
                        value::type_name(arg)
                    ));
                };
                // Copy before the next iteration can allocate.
                let part = part.to_string();
                joined = value::join_paths(&joined, &part);
            }
            value::path_new(joined)
        }
        "as_posix" => {
            if let Some(err) = check_arity("Path", method, args.len(), 0, 0) {
                return err;
            }
            let text = if cfg!(windows) {
                path.replace('\\', "/")
            } else {
                path
            };
            value::str_alloc(text)
        }
        "absolute" => {
            if let Some(err) = check_arity("Path", method, args.len(), 0, 0) {
                return err;
            }
            value::path_new(path_absolute(&path))
        }
        "resolve" => {
            if let Some(err) = check_arity("Path", method, args.len(), 0, 0) {
                return err;
            }
            // The file need not exist: fall back to the absolute form.
            match std::fs::canonicalize(fs_path(&path)) {
                Ok(resolved) => value::path_new(resolved.to_string_lossy().into_owned()),
                Err(_) => value::path_new(path_absolute(&path)),
            }
        }
        "read_text" => {
            if let Some(err) = check_arity("Path", method, args.len(), 0, 1) {
                return err;
            }
            if let Err(err) = path_encoding(method, args.first().copied()) {
                return err;
            }
            match std::fs::read_to_string(fs_path(&path)) {
                Ok(text) => value::str_alloc(text),
                Err(error) => path_os_error(&path, &error),
            }
        }
        "write_text" => {
            if let Some(err) = check_arity("Path", method, args.len(), 1, 2) {
                return err;
            }
            let text = match str_arg("write_text", 1, args[0]) {
                Ok(text) => text,
                Err(err) => return err,
            };
            if let Err(err) = path_encoding(method, args.get(1).copied()) {
                return err;
            }
            match std::fs::write(fs_path(&path), text.as_bytes()) {
                // `write_text` reports the number of characters written.
                Ok(()) => path_count(text.chars().count()),
                Err(error) => path_os_error(&path, &error),
            }
        }
        "read_bytes" => {
            if let Some(err) = check_arity("Path", method, args.len(), 0, 0) {
                return err;
            }
            match std::fs::read(fs_path(&path)) {
                Ok(data) => value::bytes_new(data),
                Err(error) => path_os_error(&path, &error),
            }
        }
        "write_bytes" => {
            if let Some(err) = check_arity("Path", method, args.len(), 1, 1) {
                return err;
            }
            let data = match bytes_arg("write_bytes", 1, args[0]) {
                Ok(data) => data,
                Err(err) => return err,
            };
            match std::fs::write(fs_path(&path), &data) {
                Ok(()) => path_count(data.len()),
                Err(error) => path_os_error(&path, &error),
            }
        }
        "open" => {
            if let Some(err) = check_arity("Path", method, args.len(), 0, 4) {
                return err;
            }
            // The slots follow `io.open`: file, mode, encoding, newline,
            // errors. Omitted trailing slots keep their defaults.
            let mut io_args = Vec::with_capacity(args.len() + 1);
            io_args.push(value::str_alloc(path));
            io_args.extend_from_slice(args);
            modules::io_open(&io_args)
        }
        "mkdir" => {
            if let Some(err) = check_arity("Path", method, args.len(), 0, 2) {
                return err;
            }
            let parents = match args.first().copied() {
                None | Some(NONE) => false,
                Some(arg) => match path_bool_arg(method, "parents", arg) {
                    Ok(value) => value,
                    Err(err) => return err,
                },
            };
            let exist_ok = match args.get(1).copied() {
                None | Some(NONE) => false,
                Some(arg) => match path_bool_arg(method, "exist_ok", arg) {
                    Ok(value) => value,
                    Err(err) => return err,
                },
            };
            let target = fs_path(&path);
            // `create_dir_all` never fails for an existing directory, so an
            // explicit `exist_ok=False` has to check first.
            if parents && !exist_ok && target.exists() {
                return exc::raise_with_message(
                    crate::exc::OS_ERROR,
                    &format!("File exists: '{path}'"),
                );
            }
            let result = if parents {
                std::fs::create_dir_all(target)
            } else {
                std::fs::create_dir(target)
            };
            match result {
                Ok(()) => NONE,
                Err(error) if exist_ok && error.kind() == std::io::ErrorKind::AlreadyExists => NONE,
                Err(error) => path_os_error(&path, &error),
            }
        }
        "iterdir" => {
            if let Some(err) = check_arity("Path", method, args.len(), 0, 0) {
                return err;
            }
            let entries = match std::fs::read_dir(fs_path(&path)) {
                Ok(entries) => entries,
                Err(error) => return path_os_error(&path, &error),
            };
            let mut names = Vec::new();
            for entry in entries {
                match entry {
                    Ok(entry) => match entry.file_name().to_str() {
                        Some(name) => names.push(name.to_string()),
                        None => continue,
                    },
                    Err(error) => return path_os_error(&path, &error),
                }
            }
            names.sort();
            let paths = names
                .iter()
                .map(|name| value::join_paths(&path, name))
                .collect();
            path_list(paths)
        }
        "glob" => {
            if let Some(err) = check_arity("Path", method, args.len(), 1, 1) {
                return err;
            }
            let pattern = match str_arg("glob", 1, args[0]) {
                Ok(pattern) => pattern,
                Err(err) => return err,
            };
            if pattern.is_empty() {
                return path_list(Vec::new());
            }
            // `"."` is transparent so results match CPython's relative form.
            let base = if path.is_empty() || path.as_str() == "." {
                std::path::PathBuf::new()
            } else {
                std::path::PathBuf::from(&path)
            };
            let mut found = Vec::new();
            glob_walk(&base, &pattern, &mut found);
            found.sort();
            found.dedup();
            path_list(found)
        }
        _ => attribute_error(obj, method),
    }
}

/// Filesystem view of a path string; an empty path means the current
/// directory.
fn fs_path(path: &str) -> &std::path::Path {
    if path.is_empty() {
        std::path::Path::new(".")
    } else {
        std::path::Path::new(path)
    }
}

/// Encodes a write count as a Python integer.
fn path_count(count: usize) -> i64 {
    ops::int_result(i64::try_from(count).unwrap_or(value::MAX_INT))
}

/// Raises `OSError` naming the failing path.
fn path_os_error(path: &str, error: &std::io::Error) -> i64 {
    exc::raise_with_message(crate::exc::OS_ERROR, &format!("{error}: '{path}'"))
}

/// Validates the optional `encoding` argument of the text helpers. Only UTF-8
/// is implemented; `None` selects it.
fn path_encoding(method: &str, arg: Option<i64>) -> Result<(), i64> {
    let Some(argument) = arg else {
        return Ok(());
    };
    if argument == NONE {
        return Ok(());
    }
    let Some(text) = value::as_str(argument) else {
        return Err(type_error(format!(
            "{method}() argument 'encoding' must be str or None, not {}",
            value::type_name(argument)
        )));
    };
    let normalized = text.to_ascii_lowercase();
    if normalized != "utf-8" && normalized != "utf8" {
        return Err(exc::raise_with_message(
            VALUE_ERROR,
            &format!("unsupported encoding: '{text}'"),
        ));
    }
    Ok(())
}

/// Decodes an optional boolean argument (`mkdir`'s `parents`/`exist_ok`).
fn path_bool_arg(method: &str, name: &str, arg: i64) -> Result<bool, i64> {
    if arg == value::TRUE {
        return Ok(true);
    }
    if arg == value::FALSE {
        return Ok(false);
    }
    Err(type_error(format!(
        "{method}() argument '{name}' must be bool, not {}",
        value::type_name(arg)
    )))
}

/// Last component of `path` after pathlib's normalisation (empty separators
/// and `"."` components dropped), like `PurePath.name`.
fn path_name(path: &str) -> String {
    path.split('/')
        .rfind(|part| !part.is_empty() && *part != ".")
        .unwrap_or_default()
        .to_string()
}

/// `PurePath.suffix`: the extension including its dot, empty when the name has
/// no dot or only a leading/trailing one.
fn path_suffix(name: &str) -> String {
    match name.rfind('.') {
        Some(index) if index > 0 && index < name.len() - 1 => name[index..].to_string(),
        _ => String::new(),
    }
}

/// `PurePath.stem`: the name without its suffix.
fn path_stem(name: &str) -> String {
    match name.rfind('.') {
        Some(index) if index > 0 && index < name.len() - 1 => name[..index].to_string(),
        _ => name.to_string(),
    }
}

/// `PurePath.parent`: the path without its last component. The current
/// directory is its own parent and a root keeps its root.
fn path_parent(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    parts.pop();
    let joined = parts.join("/");
    if joined.is_empty() {
        if absolute {
            "/".to_string()
        } else {
            ".".to_string()
        }
    } else if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// Absolute form of `path`, joining the current directory for relative paths.
fn path_absolute(path: &str) -> String {
    let candidate = std::path::Path::new(path);
    if candidate.is_absolute() {
        return path.to_string();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(candidate).to_string_lossy().into_owned(),
        Err(_) => path.to_string(),
    }
}

/// Builds a list of freshly allocated `Path` objects incrementally so the
/// conservative collector traces every element already appended.
fn path_list(paths: Vec<String>) -> i64 {
    let result = value::list_new(Vec::new());
    for path in paths {
        let item = value::path_new(path);
        if let Some(mut items) = value::list_items_mut(result) {
            items.push(item);
            crate::heap::write_barrier_edge(result, item);
        }
    }
    result
}

/// Walks one `glob` pattern segment at a time. `*` matches any run within a
/// file name and `?` exactly one character; `**` is not special (it behaves
/// like `*`). Missing directories yield no matches.
fn glob_walk(current: &std::path::Path, pattern: &str, out: &mut Vec<String>) {
    if pattern.is_empty() {
        let text = current.to_string_lossy().into_owned();
        if !text.is_empty() && std::path::Path::new(&text).exists() {
            out.push(text);
        }
        return;
    }
    let (segment, rest) = match pattern.split_once('/') {
        Some((segment, rest)) => (segment, rest),
        None => (pattern, ""),
    };
    // Repeated separators and `"."` segments are transparent.
    if segment.is_empty() || segment == "." {
        glob_walk(current, rest, out);
        return;
    }
    if !segment.contains('*') && !segment.contains('?') {
        glob_walk(&current.join(segment), rest, out);
        return;
    }
    let dir = if current.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        current
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if glob_match(segment, name) {
            names.push(name.to_string());
        }
    }
    names.sort();
    for name in names {
        glob_walk(&current.join(&name), rest, out);
    }
}

/// Shell-style wildcard match used by `glob` (per file-name segment).
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    glob_match_chars(&pattern, &text)
}

fn glob_match_chars(pattern: &[char], text: &[char]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((&'*', rest)) => (0..=text.len()).any(|end| glob_match_chars(rest, &text[end..])),
        Some((&'?', rest)) => match text.split_first() {
            Some((_, tail)) => glob_match_chars(rest, tail),
            None => false,
        },
        Some((&expected, rest)) => match text.split_first() {
            Some((&ch, tail)) => ch == expected && glob_match_chars(rest, tail),
            None => false,
        },
    }
}

/// Methods of a generator object (`generator.send/throw/close`).
///
/// Only positional arguments are supported. `throw` accepts an exception
/// *instance* only: Hydro has no exception class objects, so the supported form
/// is `gen.throw(e)` where `e` is an exception object obtained from
/// `except ... as e` (or another runtime-provided exception value).
/// `throw` takes exactly one argument; the CPython 3-argument form (type,
/// value, traceback) is not available.
fn generator_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    match method {
        "send" => {
            if let Some(err) = check_arity("generator", method, args.len(), 1, 1) {
                return err;
            }
            crate::fiber::send_value(obj, args[0])
        }
        "throw" => {
            if let Some(err) = check_arity("generator", method, args.len(), 1, 1) {
                return err;
            }
            crate::fiber::throw_value(obj, args[0])
        }
        "close" => {
            if let Some(err) = check_arity("generator", method, args.len(), 0, 0) {
                return err;
            }
            crate::fiber::close_value(obj)
        }
        _ => attribute_error(obj, method),
    }
}

/// Methods of an async generator object.
fn async_generator_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    match method {
        "__aiter__" => {
            if let Some(err) = check_arity("async_generator", method, args.len(), 0, 0) {
                return err;
            }
            crate::fiber::async_generator_aiter(obj)
        }
        "__anext__" => {
            if let Some(err) = check_arity("async_generator", method, args.len(), 0, 0) {
                return err;
            }
            crate::fiber::async_generator_next(obj)
        }
        "asend" => {
            if let Some(err) = check_arity("async_generator", method, args.len(), 1, 1) {
                return err;
            }
            crate::fiber::asend_value(obj, args[0])
        }
        "athrow" => {
            if let Some(err) = check_arity("async_generator", method, args.len(), 1, 1) {
                return err;
            }
            crate::fiber::athrow_value(obj, args[0])
        }
        "aclose" => {
            if let Some(err) = check_arity("async_generator", method, args.len(), 0, 0) {
                return err;
            }
            crate::fiber::aclose_value(obj)
        }
        _ => attribute_error(obj, method),
    }
}

/// Methods of a task (a coroutine fiber scheduled by asyncio).
fn coroutine_method(obj: i64, method: &str, args: &[i64]) -> i64 {
    match method {
        "cancel" => {
            if let Some(err) = check_arity("Task", method, args.len(), 0, 1) {
                return err;
            }
            crate::fiber::cancel_value(obj)
        }
        "done" => {
            if let Some(err) = check_arity("Task", method, args.len(), 0, 0) {
                return err;
            }
            if crate::fiber::is_done(obj) {
                value::TRUE
            } else {
                value::FALSE
            }
        }
        "cancelled" => {
            if let Some(err) = check_arity("Task", method, args.len(), 0, 0) {
                return err;
            }
            if crate::fiber::is_cancelled(obj) {
                value::TRUE
            } else {
                value::FALSE
            }
        }
        "result" => {
            if let Some(err) = check_arity("Task", method, args.len(), 0, 0) {
                return err;
            }
            if !crate::fiber::is_done(obj) {
                return exc::raise_with_message(RUNTIME_ERROR, "task is not done");
            }
            crate::fiber::finish_value(obj)
        }
        "exception" => {
            if let Some(err) = check_arity("Task", method, args.len(), 0, 0) {
                return err;
            }
            if !crate::fiber::is_done(obj) {
                return exc::raise_with_message(RUNTIME_ERROR, "task is not done");
            }
            if crate::fiber::is_cancelled(obj) {
                return crate::fiber::raise_error(obj);
            }
            crate::fiber::exception_value(obj)
        }
        _ => attribute_error(obj, method),
    }
}

#[cfg(test)]
mod tests {
    use super::{call, delete_attr, getattr};
    use crate::container;
    use crate::value::{self, encode_int, FALSE, NONE, TRUE};
    use hydro_ir::MethodKind;

    fn int(n: i64) -> i64 {
        encode_int(n).expect("representable")
    }

    fn string(s: &str) -> i64 {
        value::str_from_bytes(s.as_bytes())
    }

    fn list(items: &[i64]) -> i64 {
        value::list_new(items.to_vec())
    }

    fn dict(pairs: &[(i64, i64)]) -> i64 {
        value::dict_new(pairs.to_vec())
    }

    fn tuple(items: &[i64]) -> i64 {
        value::tuple_new(items.to_vec())
    }

    fn set(items: &[i64]) -> i64 {
        value::set_new(items.to_vec())
    }

    fn reset() {
        // A reset frees every object in the process-wide heap, so it must not
        // overlap another test that is resetting (or a test that holds the
        // heap for its whole body, e.g. the stdlib surface test).
        let _heap_guard = crate::heap::heap_reset_test_guard();
        crate::exc::init();
        crate::heap::reset();
    }

    fn invoke(obj: i64, name: &str, args: &[i64]) -> i64 {
        call(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
    }

    fn message() -> String {
        let v = crate::exc::pending_value();
        value::exception_data(v)
            .map(|d| d.msg.clone())
            .unwrap_or_default()
    }

    fn error_tag() -> i64 {
        crate::exc::pending_tag()
    }

    fn text(v: i64) -> String {
        value::as_str(v).expect("string result").to_string()
    }

    fn items(v: i64) -> Vec<i64> {
        value::as_list(v).expect("list result").to_vec()
    }

    fn rendered_list(v: i64) -> String {
        value::to_string(v)
    }

    #[test]
    fn case_mapping_and_whitespace() {
        reset();
        assert_eq!(
            text(invoke(string("Hello World"), "upper", &[])),
            "HELLO WORLD"
        );
        assert_eq!(
            text(invoke(string("Hello World"), "lower", &[])),
            "hello world"
        );
        // Full Unicode case mapping (ß -> SS).
        assert_eq!(text(invoke(string("straße"), "upper", &[])), "STRASSE");

        let padded = string("  \u{2003}Hi\u{2003}  ");
        assert_eq!(text(invoke(padded, "strip", &[])), "Hi");
        assert_eq!(text(invoke(padded, "lstrip", &[])), "Hi\u{2003}  ");
        assert_eq!(text(invoke(padded, "rstrip", &[])), "  \u{2003}Hi");
        assert!(!crate::exc::pending());
    }

    #[test]
    fn padding_methods() {
        reset();
        assert_eq!(text(invoke(string("ab"), "rjust", &[int(5)])), "   ab");
        assert_eq!(
            text(invoke(string("ab"), "rjust", &[int(5), string("-")])),
            "---ab"
        );
        assert_eq!(text(invoke(string("ab"), "ljust", &[int(5)])), "ab   ");
        assert_eq!(
            text(invoke(string("ab"), "ljust", &[int(5), string("-")])),
            "ab---"
        );
        // An odd amount of padding keeps the extra character on the right.
        assert_eq!(text(invoke(string("a"), "center", &[int(6)])), "  a   ");
        assert_eq!(
            text(invoke(string("ab"), "center", &[int(5), string("-")])),
            "--ab-"
        );
        assert_eq!(text(invoke(string("abc"), "center", &[int(3)])), "abc");
        // Width is a code-point count, and the fill character may be any
        // single character, including a non-ASCII one.
        assert_eq!(
            text(invoke(string("äöü"), "rjust", &[int(4), string("·")])),
            "·äöü"
        );
        assert_eq!(
            text(invoke(string("é"), "center", &[int(4), string("é")])),
            "éééé"
        );
        // A width no greater than the current length returns the string
        // unchanged, including a negative width.
        assert_eq!(text(invoke(string("abc"), "rjust", &[int(2)])), "abc");
        assert_eq!(text(invoke(string("abc"), "ljust", &[int(-1)])), "abc");
        assert_eq!(text(invoke(string(""), "center", &[int(0)])), "");

        assert_eq!(text(invoke(string("42"), "zfill", &[int(5)])), "00042");
        assert_eq!(text(invoke(string("-42"), "zfill", &[int(5)])), "-0042");
        assert_eq!(text(invoke(string("+42"), "zfill", &[int(5)])), "+0042");
        assert_eq!(text(invoke(string("-42"), "zfill", &[int(2)])), "-42");
        assert_eq!(text(invoke(string(""), "zfill", &[int(2)])), "00");
        assert!(!crate::exc::pending());
    }

    #[test]
    fn padding_errors() {
        reset();
        invoke(string("a"), "rjust", &[string("3")]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "'str' object cannot be interpreted as an integer"
        );

        reset();
        invoke(string("a"), "ljust", &[int(3), string("")]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "The fill character must be exactly one character long"
        );

        reset();
        invoke(string("a"), "center", &[int(3), string("xy")]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "The fill character must be exactly one character long"
        );

        reset();
        invoke(string("a"), "rjust", &[int(3), int(5)]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "The fill character must be a unicode character, not int"
        );

        reset();
        invoke(string("a"), "rjust", &[]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(message(), "rjust() takes at least 1 argument (0 given)");

        reset();
        invoke(string("a"), "center", &[int(1), string(" "), string(" ")]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(message(), "center() takes at most 2 arguments (3 given)");

        reset();
        invoke(string("a"), "zfill", &[int(2), string("0")]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "str.zfill() takes exactly one argument (2 given)"
        );
    }

    #[test]
    fn split_forms() {
        reset();
        assert_eq!(
            rendered_list(invoke(string("a b  c"), "split", &[])),
            "['a', 'b', 'c']"
        );
        assert_eq!(
            rendered_list(invoke(string("a,,b"), "split", &[string(",")])),
            "['a', '', 'b']"
        );
        assert_eq!(
            rendered_list(invoke(string(""), "split", &[string(",")])),
            "['']"
        );
        assert_eq!(rendered_list(invoke(string("   "), "split", &[])), "[]");

        let value = invoke(string("abc"), "split", &[string("")]);
        assert_eq!(value, NONE);
        assert_eq!(error_tag(), crate::exc::VALUE_ERROR as i64);
        assert_eq!(message(), "empty separator");

        reset();
        invoke(string("abc"), "split", &[int(1)]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(message(), "must be str or None, not int");
    }

    #[test]
    fn join_results_and_errors() {
        reset();
        let joined = invoke(
            string("-"),
            "join",
            &[list(&[string("a"), string("b"), string("c")])],
        );
        assert_eq!(text(joined), "a-b-c");
        assert_eq!(text(invoke(string(""), "join", &[list(&[])])), "");

        // Tuples are iterables too.
        assert_eq!(
            text(invoke(
                string("-"),
                "join",
                &[tuple(&[string("a"), string("b"), string("c")])]
            )),
            "a-b-c"
        );

        // Every other iterable is consumed through the iterator ABI.
        let iterator = crate::core::iter::iter_new(list(&[string("a"), string("b")]));
        assert_eq!(text(invoke(string("+"), "join", &[iterator])), "a+b");

        let generator = value::fiber_generator_new(
            yields_join_parts as *const () as usize,
            "join_parts".to_string(),
            Vec::new(),
        )
        .expect("generator");
        assert_eq!(text(invoke(string("/"), "join", &[generator])), "a/b");

        invoke(string("a"), "join", &[list(&[int(1), int(2)])]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "sequence item 0: expected str instance, int found"
        );

        reset();
        invoke(string("a"), "join", &[tuple(&[string("a"), int(1)])]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "sequence item 1: expected str instance, int found"
        );

        reset();
        let generator = value::fiber_generator_new(
            yields_join_int as *const () as usize,
            "join_int".to_string(),
            Vec::new(),
        )
        .expect("generator");
        invoke(string("a"), "join", &[generator]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "sequence item 0: expected str instance, int found"
        );

        reset();
        invoke(string("a"), "join", &[int(1)]);
        assert_eq!(message(), "can only join an iterable");
    }

    /// Generator body yielding the string parts `a`, `b` for the join tests.
    extern "C" fn yields_join_parts(_argv: *const i64) -> i64 {
        crate::fiber::yield_value(string("a"));
        crate::fiber::yield_value(string("b"));
        NONE
    }

    /// Generator body yielding a non-string item, rejected by `str.join`.
    extern "C" fn yields_join_int(_argv: *const i64) -> i64 {
        crate::fiber::yield_value(int(1));
        NONE
    }

    #[test]
    fn replace_counts() {
        reset();
        let all = invoke(string("banana"), "replace", &[string("a"), string("o")]);
        assert_eq!(text(all), "bonono");
        let two = invoke(
            string("banana"),
            "replace",
            &[string("a"), string("o"), int(2)],
        );
        assert_eq!(text(two), "bonona");
        let none = invoke(
            string("banana"),
            "replace",
            &[string("a"), string("o"), int(0)],
        );
        assert_eq!(text(none), "banana");
        let negative = invoke(
            string("banana"),
            "replace",
            &[string("a"), string("o"), int(-3)],
        );
        assert_eq!(text(negative), "bonono");
        // Empty needle inserts between characters, like CPython.
        assert_eq!(
            text(invoke(string("abc"), "replace", &[string(""), string("-")])),
            "-a-b-c-"
        );

        invoke(string("x"), "replace", &[int(1), string("y")]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(message(), "replace() argument 1 must be str, not int");
    }

    #[test]
    fn find_and_count() {
        reset();
        let hay = string("banana");
        assert_eq!(invoke(hay, "find", &[string("na")]), int(2));
        assert_eq!(invoke(hay, "find", &[string("zz")]), int(-1));
        assert_eq!(invoke(hay, "find", &[string("")]), int(0));
        assert_eq!(invoke(hay, "count", &[string("na")]), int(2));
        assert_eq!(invoke(hay, "count", &[string("")]), int(7));

        // Character (not byte) index for non-ASCII text.
        assert_eq!(invoke(string("äöüx"), "find", &[string("x")]), int(3));
        assert_eq!(invoke(string("ää"), "count", &[string("ä")]), int(2));
    }

    #[test]
    fn prefix_suffix_and_predicates() {
        reset();
        assert_eq!(invoke(string("hello"), "startswith", &[string("he")]), TRUE);
        assert_eq!(
            invoke(string("hello"), "startswith", &[string("lo")]),
            FALSE
        );
        assert_eq!(invoke(string("hello"), "endswith", &[string("lo")]), TRUE);
        assert_eq!(invoke(string("hello"), "endswith", &[string("he")]), FALSE);

        assert_eq!(invoke(string("123"), "isdigit", &[]), TRUE);
        assert_eq!(invoke(string("²"), "isdigit", &[]), TRUE);
        assert_eq!(invoke(string("abc"), "isalpha", &[]), TRUE);
        assert_eq!(invoke(string("a1"), "isalnum", &[]), TRUE);
        assert_eq!(invoke(string("a1!"), "isalnum", &[]), FALSE);
        assert_eq!(invoke(string(" \t"), "isspace", &[]), TRUE);
        // The empty string is false for every predicate.
        for method in ["isdigit", "isalpha", "isalnum", "isspace"] {
            assert_eq!(invoke(string(""), method, &[]), FALSE);
        }
    }

    #[test]
    fn isascii_predicate() {
        reset();
        assert_eq!(invoke(string("hello"), "isascii", &[]), TRUE);
        assert_eq!(invoke(string("abc123!~ \t\r\n"), "isascii", &[]), TRUE);
        // DEL (U+007F) is still ASCII.
        assert_eq!(invoke(string("\u{7f}"), "isascii", &[]), TRUE);
        // Latin-1, general Unicode, and astral characters are not.
        assert_eq!(invoke(string("é"), "isascii", &[]), FALSE);
        assert_eq!(invoke(string("abcé"), "isascii", &[]), FALSE);
        assert_eq!(invoke(string("αβγ"), "isascii", &[]), FALSE);
        assert_eq!(invoke(string("😀"), "isascii", &[]), FALSE);
        // The empty string is ASCII.
        assert_eq!(invoke(string(""), "isascii", &[]), TRUE);
    }

    #[test]
    fn upper_lower_predicates() {
        reset();
        assert_eq!(invoke(string("EDOM"), "isupper", &[]), TRUE);
        assert_eq!(invoke(string("EPERM2"), "isupper", &[]), TRUE);
        assert_eq!(invoke(string("ÄÖÜ"), "isupper", &[]), TRUE);
        assert_eq!(invoke(string("Error"), "isupper", &[]), FALSE);
        assert_eq!(invoke(string("123"), "isupper", &[]), FALSE);
        assert_eq!(invoke(string("__dict__"), "isupper", &[]), FALSE);
        assert_eq!(invoke(string(""), "isupper", &[]), FALSE);

        assert_eq!(invoke(string("errorcode"), "islower", &[]), TRUE);
        assert_eq!(invoke(string("abc2!"), "islower", &[]), TRUE);
        assert_eq!(invoke(string("αβγ"), "islower", &[]), TRUE);
        assert_eq!(invoke(string("aBc"), "islower", &[]), FALSE);
        assert_eq!(invoke(string("123"), "islower", &[]), FALSE);
        assert_eq!(invoke(string(""), "islower", &[]), FALSE);
        assert!(!crate::exc::pending());
    }

    #[test]
    fn list_append_extend_insert() {
        reset();
        let xs = list(&[int(1), int(2)]);
        invoke(xs, "append", &[int(3)]);
        assert_eq!(items(xs), vec![int(1), int(2), int(3)]);

        invoke(xs, "extend", &[list(&[int(4), int(5)])]);
        assert_eq!(items(xs), vec![int(1), int(2), int(3), int(4), int(5)]);

        // Self-extend sees the original contents only.
        let ys = list(&[int(1), int(2)]);
        invoke(ys, "extend", &[ys]);
        assert_eq!(items(ys), vec![int(1), int(2), int(1), int(2)]);

        invoke(xs, "insert", &[int(0), int(0)]);
        assert_eq!(items(xs)[0], int(0));
        // Negative and oversized indices clamp.
        invoke(xs, "insert", &[int(-100), int(9)]);
        assert_eq!(items(xs)[0], int(9));
        invoke(xs, "insert", &[int(100), int(7)]);
        assert_eq!(*items(xs).last().unwrap(), int(7));
        invoke(xs, "insert", &[int(-1), int(8)]);
        assert_eq!(items(xs)[items(xs).len() - 2], int(8));
    }

    #[test]
    fn list_remove_pop_and_errors() {
        reset();
        let xs = list(&[int(1), int(2), int(3)]);
        invoke(xs, "remove", &[int(2)]);
        assert_eq!(items(xs), vec![int(1), int(3)]);

        invoke(xs, "remove", &[int(42)]);
        assert_eq!(error_tag(), crate::exc::VALUE_ERROR as i64);
        assert_eq!(message(), "list.remove(x): x not in list");

        crate::exc::clear();
        assert_eq!(invoke(xs, "pop", &[]), int(3));
        assert_eq!(items(xs), vec![int(1)]);
        let ys = list(&[int(1), int(2), int(3)]);
        assert_eq!(invoke(ys, "pop", &[int(-1)]), int(3));
        assert_eq!(invoke(ys, "pop", &[int(0)]), int(1));

        crate::exc::clear();
        invoke(list(&[]), "pop", &[]);
        assert_eq!(error_tag(), crate::exc::INDEX_ERROR as i64);
        assert_eq!(message(), "pop from empty list");

        crate::exc::clear();
        invoke(list(&[int(1)]), "pop", &[int(5)]);
        assert_eq!(error_tag(), crate::exc::INDEX_ERROR as i64);
        assert_eq!(message(), "pop index out of range");
    }

    #[test]
    fn list_clear_reverse_copy_count_index() {
        reset();
        let xs = list(&[int(4), int(3), int(1), int(0)]);
        invoke(xs, "reverse", &[]);
        assert_eq!(items(xs), vec![int(0), int(1), int(3), int(4)]);
        assert_eq!(invoke(xs, "count", &[int(1)]), int(1));
        assert_eq!(invoke(xs, "count", &[int(9)]), int(0));
        assert_eq!(invoke(xs, "index", &[int(3)]), int(2));

        let copy = invoke(xs, "copy", &[]);
        assert_eq!(items(copy), items(xs));
        invoke(copy, "append", &[int(99)]);
        assert_ne!(items(copy), items(xs));

        invoke(xs, "clear", &[]);
        assert_eq!(items(xs), Vec::<i64>::new());

        reset();
        invoke(list(&[int(1)]), "index", &[string("x")]);
        assert_eq!(error_tag(), crate::exc::VALUE_ERROR as i64);
        assert_eq!(message(), "'x' is not in list");
    }

    #[test]
    fn dict_get_keys_values_and_pop() {
        reset();
        let d = dict(&[(string("a"), int(1)), (string("b"), int(2))]);
        assert_eq!(invoke(d, "get", &[string("a")]), int(1));
        assert_eq!(invoke(d, "get", &[string("z")]), NONE);
        assert_eq!(invoke(d, "get", &[string("z"), int(0)]), int(0));
        // Lookups must not mutate, and the views report their own type.
        let keys = invoke(d, "keys", &[]);
        let values = invoke(d, "values", &[]);
        assert_eq!(value::type_name(keys), "dict_keys");
        assert_eq!(value::type_name(values), "dict_values");
        assert_eq!(rendered_list(keys), "dict_keys(['a', 'b'])");
        assert_eq!(rendered_list(values), "dict_values([1, 2])");

        assert_eq!(invoke(d, "pop", &[string("a")]), int(1));
        assert_eq!(rendered_list(d), "{'b': 2}");
        assert_eq!(invoke(d, "pop", &[string("z"), int(-1)]), int(-1));

        crate::exc::clear();
        invoke(d, "pop", &[string("z")]);
        assert_eq!(error_tag(), crate::exc::KEY_ERROR as i64);
        assert_eq!(message(), "'z'");

        crate::exc::clear();
        // The view is live: it observes dict mutations and is not a list.
        invoke(d, "__setitem__", &[string("c"), int(3)]);
        assert_eq!(rendered_list(keys), "dict_keys(['b', 'c'])");
        assert_eq!(rendered_list(values), "dict_values([2, 3])");
        invoke(keys, "append", &[string("z")]);
        assert_eq!(error_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
        crate::exc::clear();
        // Membership and length go through the view.
        assert_eq!(invoke(keys, "__contains__", &[string("b")]), TRUE);
        assert_eq!(invoke(keys, "__len__", &[]), int(2));
        assert!(!crate::exc::pending());
    }

    #[test]
    fn dict_items_view_is_live_and_yields_tuples() {
        reset();
        let d = dict(&[(string("a"), int(1)), (string("b"), int(2))]);
        let pairs = invoke(d, "items", &[]);
        assert_eq!(value::type_name(pairs), "dict_items");
        assert_eq!(rendered_list(pairs), "dict_items([('a', 1), ('b', 2)])");
        // Iteration materialises fresh two-element tuples in insertion order.
        let entries = items(crate::builtins::list_from(pairs));
        assert_eq!(entries.len(), 2);
        assert_eq!(value::to_string(entries[0]), "('a', 1)");
        assert_eq!(value::type_name(entries[0]), "tuple");
        // Deleting a key is visible through the live view.
        invoke(d, "pop", &[string("a")]);
        assert_eq!(rendered_list(pairs), "dict_items([('b', 2)])");
        assert!(!crate::exc::pending());
    }

    #[test]
    fn tuple_count_and_index() {
        reset();
        let t = tuple(&[int(1), int(2), int(1)]);
        assert_eq!(invoke(t, "count", &[int(1)]), int(2));
        assert_eq!(invoke(t, "count", &[int(9)]), int(0));
        assert_eq!(invoke(t, "index", &[int(2)]), int(1));
        assert_eq!(invoke(t, "index", &[int(1)]), int(0));
        assert!(!crate::exc::pending());

        invoke(t, "index", &[int(9)]);
        assert_eq!(error_tag(), crate::exc::VALUE_ERROR as i64);
        assert_eq!(message(), "tuple.index(x): x not in tuple");

        crate::exc::clear();
        invoke(t, "count", &[]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "tuple.count() takes exactly one argument (0 given)"
        );

        crate::exc::clear();
        invoke(t, "append", &[int(1)]);
        assert_eq!(error_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "'tuple' object has no attribute 'append'");
    }

    #[test]
    fn dict_update_and_clear() {
        reset();
        let d = dict(&[(string("a"), int(1))]);
        invoke(
            d,
            "update",
            &[dict(&[(string("b"), int(2)), (string("a"), int(3))])],
        );
        // Existing key keeps its position, value is overwritten.
        assert_eq!(rendered_list(d), "{'a': 3, 'b': 2}");

        let self_update = dict(&[(string("x"), int(1)), (string("y"), int(2))]);
        invoke(self_update, "update", &[self_update]);
        assert_eq!(rendered_list(self_update), "{'x': 1, 'y': 2}");

        invoke(d, "clear", &[]);
        assert_eq!(rendered_list(d), "{}");
    }

    #[test]
    fn dict_update_accepts_an_iterable_of_pairs() {
        reset();
        let d = dict(&[(string("a"), int(1))]);
        // A list of pairs behaves like a mapping: existing keys keep their
        // position and take the new value, new keys append.
        invoke(
            d,
            "update",
            &[list(&[
                tuple(&[string("b"), int(2)]),
                tuple(&[string("a"), int(3)]),
            ])],
        );
        assert_eq!(rendered_list(d), "{'a': 3, 'b': 2}");
        assert!(!crate::exc::pending());

        // Pairs nested in a tuple iterable are accepted too.
        invoke(d, "update", &[tuple(&[tuple(&[string("c"), int(4)])])]);
        assert_eq!(rendered_list(d), "{'a': 3, 'b': 2, 'c': 4}");

        reset();
        invoke(dict(&[]), "update", &[list(&[int(1)])]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "cannot convert dictionary update sequence element #0 to a sequence"
        );

        reset();
        invoke(
            dict(&[]),
            "update",
            &[list(&[tuple(&[int(1), int(2), int(3)])])],
        );
        assert_eq!(error_tag(), crate::exc::VALUE_ERROR as i64);
        assert_eq!(
            message(),
            "dictionary update sequence element #0 has length 3; 2 is required"
        );

        reset();
        invoke(dict(&[]), "update", &[list(&[tuple(&[list(&[]), int(1)])])]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(message(), "unhashable type: 'list'");

        reset();
        invoke(dict(&[]), "update", &[int(1)]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(message(), "'int' object is not iterable");
    }

    #[test]
    fn dict_unhashable_keys_are_rejected() {
        reset();
        invoke(dict(&[]), "get", &[list(&[])]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(message(), "unhashable type: 'list'");

        reset();
        invoke(dict(&[]), "pop", &[dict(&[])]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(message(), "unhashable type: 'dict'");
    }

    #[test]
    fn unknown_methods_raise_attribute_error() {
        reset();
        invoke(string("x"), "nope", &[]);
        assert_eq!(error_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "'str' object has no attribute 'nope'");

        reset();
        invoke(int(1), "append", &[]);
        assert_eq!(error_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "'int' object has no attribute 'append'");
    }

    #[test]
    fn arity_errors_match_cpython_text() {
        reset();
        invoke(string("x"), "upper", &[int(1)]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(message(), "str.upper() takes no arguments (1 given)");

        reset();
        invoke(list(&[]), "append", &[]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "list.append() takes exactly one argument (0 given)"
        );

        reset();
        invoke(list(&[]), "pop", &[int(0), int(1)]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(message(), "pop() takes at most 1 argument (2 given)");

        reset();
        invoke(string("x"), "replace", &[string("a")]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(message(), "replace() takes at least 2 arguments (1 given)");

        reset();
        invoke(list(&[]), "insert", &[int(0)]);
        assert_eq!(
            message(),
            "list.insert() takes exactly 2 arguments (1 given)"
        );
    }

    #[test]
    fn set_methods_mutate_and_copy() {
        reset();
        let s = set(&[int(1), int(2)]);
        invoke(s, "add", &[int(2)]);
        invoke(s, "add", &[int(3)]);
        assert_eq!(value::as_set(s), Some(&[int(1), int(2), int(3)][..]));
        assert_eq!(error_tag(), -1);

        invoke(s, "remove", &[int(2)]);
        assert_eq!(value::as_set(s), Some(&[int(1), int(3)][..]));
        invoke(s, "discard", &[int(9)]);
        invoke(s, "discard", &[int(3)]);
        assert_eq!(value::as_set(s), Some(&[int(1)][..]));
        assert!(!crate::exc::pending());

        // Missing remove raises KeyError with the element's repr.
        invoke(s, "remove", &[int(9)]);
        assert_eq!(error_tag(), crate::exc::KEY_ERROR as i64);
        assert_eq!(message(), "9");
        crate::exc::clear();

        invoke(s, "remove", &[list(&[int(1)])]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(message(), "unhashable type: 'list'");
        crate::exc::clear();

        // copy() is independent.
        let copy = invoke(s, "copy", &[]);
        assert_ne!(copy, s);
        invoke(copy, "add", &[int(5)]);
        assert_eq!(value::as_set(copy).map(|items| items.len()), Some(2));
        assert_eq!(value::as_set(s).map(|items| items.len()), Some(1));

        invoke(copy, "clear", &[]);
        assert!(value::as_set(copy).map(|items| items.is_empty()) == Some(true));
        assert_eq!(value::as_set(s).map(|items| items.len()), Some(1));
        assert!(!crate::exc::pending());
    }

    #[test]
    fn set_union_accepts_set_list_and_tuple() {
        reset();
        let base = set(&[int(1), int(2)]);
        let union = invoke(base, "union", &[set(&[int(2), int(3)])]);
        assert_eq!(value::as_set(union), Some(&[int(1), int(2), int(3)][..]));

        let from_list = invoke(base, "union", &[list(&[int(3), int(4), int(3)])]);
        assert_eq!(
            value::as_set(from_list),
            Some(&[int(1), int(2), int(3), int(4)][..])
        );
        // The receiver is unchanged and the result is a fresh set.
        assert_eq!(value::as_set(base), Some(&[int(1), int(2)][..]));
        assert_ne!(union, base);

        let from_tuple = invoke(base, "union", &[tuple(&[int(5)])]);
        assert!(container::set_contains(
            value::as_set(from_tuple).unwrap_or(&[]),
            int(5)
        ));

        crate::exc::clear();
        invoke(base, "union", &[list(&[list(&[int(1)])])]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(message(), "unhashable type: 'list'");

        crate::exc::clear();
        invoke(base, "union", &[int(1)]);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(message(), "'int' object is not iterable");

        crate::exc::clear();
        invoke(base, "bogus", &[]);
        assert_eq!(error_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "'set' object has no attribute 'bogus'");
    }

    #[test]
    fn malformed_names_and_buffers_do_not_panic() {
        reset();
        let obj = string("x");
        // Empty name.
        invoke(obj, "", &[]);
        assert_eq!(error_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "'str' object has no attribute ''");

        // Invalid UTF-8 name.
        crate::exc::clear();
        let bytes = [0xffu8, 0xfe];
        call(obj, bytes.as_ptr(), bytes.len(), 0, std::ptr::null());
        assert_eq!(error_tag(), crate::exc::ATTRIBUTE_ERROR as i64);

        // argc > 0 with a NULL argument buffer is a TypeError, not a crash.
        crate::exc::clear();
        call(obj, b"upper".as_ptr(), "upper".len(), 1, std::ptr::null());
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert!(message().contains("argument buffer"));

        // NULL name pointer.
        crate::exc::clear();
        call(obj, std::ptr::null(), 3, 0, std::ptr::null());
        assert_eq!(error_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
    }

    /// Test getter/caller: returns its first (and only) argument, which lets a
    /// test observe exactly which receiver a method bound.
    extern "C" fn first_argument(args: *const i64) -> i64 {
        // SAFETY: wrappers created with arity 1 are called with one argument.
        unsafe { *args }
    }

    fn method(name: &str, arity: usize, kind: MethodKind) -> (String, i64, MethodKind) {
        (
            name.to_string(),
            value::function_new(
                first_argument as *const () as usize,
                arity,
                name.to_string(),
            ),
            kind,
        )
    }

    #[test]
    fn class_methods_bind_their_receiver_by_kind() {
        reset();
        let class = value::class_new_full(
            "C".to_string(),
            NONE,
            vec![
                method("get", 1, MethodKind::Normal),
                method("make", 1, MethodKind::Classmethod),
                method("helper", 1, MethodKind::Staticmethod),
            ],
            vec![("limit".to_string(), int(5))],
        );
        let instance = value::instance_new(class);

        // Calling through an instance binds `self`, the class or nothing.
        assert_eq!(invoke(instance, "get", &[]), instance);
        assert_eq!(invoke(instance, "make", &[]), class);
        assert_eq!(invoke(instance, "helper", &[int(9)]), int(9));
        assert!(!crate::exc::pending());

        // Calling through the class leaves ordinary methods unbound, binds the
        // class for classmethods and nothing for staticmethods.
        assert_eq!(invoke(class, "make", &[]), class);
        assert_eq!(invoke(class, "helper", &[int(3)]), int(3));
        assert_eq!(invoke(class, "get", &[instance]), instance);
        assert!(!crate::exc::pending());

        crate::exc::clear();
        invoke(instance, "missing", &[]);
        assert_eq!(error_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
    }

    #[test]
    fn properties_and_class_attributes_resolve_through_getattr() {
        reset();
        let class = value::class_new_full(
            "C".to_string(),
            NONE,
            vec![method("value", 1, MethodKind::Property)],
            vec![
                ("limit".to_string(), int(5)),
                ("__slots__".to_string(), list(&[])),
            ],
        );
        let instance = value::instance_new(class);

        // A property getter receives the instance.
        assert_eq!(getattr(instance, b"value".as_ptr(), 5), instance);
        // Class attributes are visible on instances as a fallback.
        assert_eq!(getattr(instance, b"limit".as_ptr(), 5), int(5));
        assert_eq!(getattr(instance, b"__class__".as_ptr(), 9), class);
        assert_eq!(getattr(class, b"limit".as_ptr(), 5), int(5));
        assert_eq!(text(getattr(class, b"__name__".as_ptr(), 8)), "C");
        assert!(!crate::exc::pending());

        // Runtime class assignment writes the overlay and is visible to
        // existing instances.
        crate::exc::clear();
        assert_eq!(super::setattr(class, b"limit".as_ptr(), 5, int(6)), NONE);
        assert!(!crate::exc::pending());
        assert_eq!(getattr(class, b"limit".as_ptr(), 5), int(6));
        assert_eq!(getattr(instance, b"limit".as_ptr(), 5), int(6));
    }

    #[test]
    fn exception_attributes_resolve_through_getattr_and_hasattr() {
        reset();
        let raised =
            crate::exc::exception_value(crate::exc::VALUE_ERROR, "boom", vec!["boom".to_string()]);

        let args = getattr(raised, b"args".as_ptr(), 4);
        assert_eq!(value::to_string(args), "('boom',)");
        // Unset `__cause__`/`__context__` are `None`; there is no traceback
        // object, so `__traceback__` is `None` too.
        assert_eq!(getattr(raised, b"__cause__".as_ptr(), 9), NONE);
        assert_eq!(getattr(raised, b"__context__".as_ptr(), 11), NONE);
        assert_eq!(getattr(raised, b"__traceback__".as_ptr(), 13), NONE);
        assert_eq!(getattr(raised, b"__suppress_context__".as_ptr(), 20), FALSE);
        assert_eq!(
            getattr(raised, b"__class__".as_ptr(), 9),
            crate::exc::exception_class(crate::exc::VALUE_ERROR)
        );
        assert!(!crate::exc::pending());

        // `hasattr` reports the surface without leaking an AttributeError.
        assert_eq!(super::hasattr(raised, b"args".as_ptr(), 4), TRUE);
        assert_eq!(super::hasattr(raised, b"__cause__".as_ptr(), 9), TRUE);
        assert_eq!(super::hasattr(raised, b"missing".as_ptr(), 7), FALSE);
        assert!(!crate::exc::pending());

        // Reading an unknown name still raises AttributeError.
        assert_eq!(getattr(raised, b"missing".as_ptr(), 7), NONE);
        assert_eq!(error_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
        crate::exc::init();
    }

    #[test]
    fn std_stream_methods_write_flush_and_report_a_tty() {
        reset();
        let stdout = value::std_stream_new(1);
        assert_eq!(invoke(stdout, "write", &[string("")]), int(0));
        assert_eq!(invoke(stdout, "flush", &[]), NONE);
        let tty = invoke(stdout, "isatty", &[]);
        assert!(tty == TRUE || tty == FALSE);
        assert!(!crate::exc::pending());

        // The stderr stream shares the same method surface.
        let stderr = value::std_stream_new(2);
        assert_eq!(invoke(stderr, "write", &[string("")]), int(0));
        assert_eq!(invoke(stderr, "flush", &[]), NONE);

        // Non-string writes and closed-stream operations are rejected.
        reset();
        let stdout = value::std_stream_new(1);
        assert_eq!(invoke(stdout, "write", &[int(1)]), NONE);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(message(), "write() argument must be str, not int");

        reset();
        let stdout = value::std_stream_new(1);
        assert_eq!(invoke(stdout, "close", &[]), NONE);
        assert_eq!(error_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
    }

    #[test]
    fn std_stream_fileno_and_read_surface() {
        // `fileno` reports the standard descriptor; stdin flushes as a no-op
        // and reports a (possibly false) tty without consuming input.
        reset();
        let stdin = value::std_stream_new(0);
        assert_eq!(invoke(stdin, "fileno", &[]), int(0));
        assert_eq!(invoke(stdin, "flush", &[]), NONE);
        let tty = invoke(stdin, "isatty", &[]);
        assert!(tty == TRUE || tty == FALSE);
        assert!(!crate::exc::pending());

        // Each error case starts from a fresh heap so the stream handle stays
        // valid.
        reset();
        assert_eq!(invoke(value::std_stream_new(1), "fileno", &[]), int(1));
        reset();
        assert_eq!(invoke(value::std_stream_new(2), "fileno", &[]), int(2));

        // Writing to stdin is rejected like CPython's `UnsupportedOperation`.
        reset();
        let stdin = value::std_stream_new(0);
        assert_eq!(invoke(stdin, "write", &[string("")]), NONE);
        assert_eq!(error_tag(), crate::exc::OS_ERROR as i64);
        assert_eq!(message(), "write: stream is not writable");

        // Reading is not attempted against the interactive stdin; the output
        // streams reject it with `OSError` like `io.UnsupportedOperation`.
        reset();
        let stdout = value::std_stream_new(1);
        assert_eq!(invoke(stdout, "readline", &[]), NONE);
        assert_eq!(error_tag(), crate::exc::OS_ERROR as i64);
        assert_eq!(message(), "readline: stream is not readable");

        reset();
        let stdout = value::std_stream_new(2);
        assert_eq!(invoke(stdout, "read", &[]), NONE);
        assert_eq!(error_tag(), crate::exc::OS_ERROR as i64);
        assert_eq!(message(), "read: stream is not readable");

        reset();
        let stdout = value::std_stream_new(1);
        assert_eq!(invoke(stdout, "read", &[int(-1)]), NONE);
        assert_eq!(error_tag(), crate::exc::OS_ERROR as i64);

        // A non-integer size is a `TypeError`, checked before readability.
        reset();
        let stdout = value::std_stream_new(1);
        assert_eq!(invoke(stdout, "read", &[string("x")]), NONE);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(message(), "read() argument must be an integer, not str");

        // Arity is enforced per method.
        reset();
        let stdin = value::std_stream_new(0);
        assert_eq!(invoke(stdin, "fileno", &[int(1)]), NONE);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        reset();
        let stdin = value::std_stream_new(0);
        assert_eq!(invoke(stdin, "readline", &[int(1)]), NONE);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        reset();
        let stdin = value::std_stream_new(0);
        assert_eq!(invoke(stdin, "read", &[int(1), int(2)]), NONE);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
    }

    #[test]
    fn delete_attr_removes_instance_and_class_attributes() {
        reset();
        let class = value::class_new("Thing".to_string(), NONE, Vec::new());
        let instance = value::instance_new(class);
        value::instance_setattr(instance, "value".to_string(), int(7));
        assert_eq!(getattr(instance, "value".as_ptr(), 5), int(7));
        assert!(!crate::exc::pending());

        assert_eq!(delete_attr(instance, "value".as_ptr(), 5), NONE);
        assert!(!crate::exc::pending());
        assert_eq!(getattr(instance, "value".as_ptr(), 5), NONE);
        assert_eq!(error_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
        assert_eq!(message(), "'Thing' object has no attribute 'value'");
        crate::exc::clear();

        // Deleting the same attribute twice reports the missing name.
        assert_eq!(delete_attr(instance, "value".as_ptr(), 5), NONE);
        assert_eq!(error_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
        crate::exc::clear();

        // Class namespaces accept runtime assignment, visible to instances.
        assert_eq!(super::setattr(class, "value".as_ptr(), 5, int(9)), NONE);
        assert!(!crate::exc::pending());
        assert_eq!(getattr(class, "value".as_ptr(), 5), int(9));
        assert_eq!(getattr(instance, "value".as_ptr(), 5), int(9));
        assert_eq!(delete_attr(class, "value".as_ptr(), 5), NONE);
        assert!(!crate::exc::pending());
        assert_eq!(getattr(instance, "value".as_ptr(), 5), NONE);
        assert_eq!(error_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
        crate::exc::clear();

        // Deleting a name that the class never had raises TypeError.
        assert_eq!(delete_attr(class, "value".as_ptr(), 5), NONE);
        assert_eq!(error_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(
            message(),
            "cannot delete attribute 'value' of class 'Thing'"
        );
        crate::exc::clear();

        // Non-instance receivers report AttributeError.
        assert_eq!(delete_attr(int(1), "value".as_ptr(), 5), NONE);
        assert_eq!(error_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
    }

    #[test]
    fn builtin_descriptor_values_bind_like_python() {
        reset();
        let entry = |name: &str, arity: usize| {
            value::function_new(
                first_argument as *const () as usize,
                arity,
                name.to_string(),
            )
        };
        let wrapped_class = value::class_new("Alias".to_string(), NONE, Vec::new());
        let cm_target = entry("cm", 1);
        let sm_target = entry("sm", 1);
        let pr_get = entry("pr", 1);
        let prw_get = entry("prw", 1);
        let prw_set = entry("prw_set", 2);
        assert_eq!(
            value::type_name(value::classmethod_new(sm_target)),
            "classmethod"
        );
        assert_eq!(
            value::type_name(value::staticmethod_new(sm_target)),
            "staticmethod"
        );
        assert_eq!(
            value::to_string(value::property_new(pr_get, NONE, NONE)),
            "<property object>"
        );
        let class = value::class_new_full(
            "Descriptors".to_string(),
            NONE,
            Vec::new(),
            vec![
                ("cm".to_string(), value::classmethod_new(cm_target)),
                ("sm".to_string(), value::staticmethod_new(sm_target)),
                ("pr".to_string(), value::property_new(pr_get, NONE, NONE)),
                (
                    "prw".to_string(),
                    value::property_new(prw_get, prw_set, NONE),
                ),
                (
                    "__class_getitem__".to_string(),
                    value::classmethod_new(wrapped_class),
                ),
            ],
        );
        let instance = value::instance_new(class);

        // classmethod binds the owner class, through the instance and the class.
        let bound = getattr(instance, b"cm".as_ptr(), 2);
        assert_eq!(crate::builtins::call(bound, 0, std::ptr::null()), class);
        let class_bound = getattr(class, b"cm".as_ptr(), 2);
        assert_eq!(
            crate::builtins::call(class_bound, 0, std::ptr::null()),
            class
        );

        // staticmethod returns its target unchanged on both accesses.
        assert_eq!(getattr(instance, b"sm".as_ptr(), 2), sm_target);
        assert_eq!(getattr(class, b"sm".as_ptr(), 2), sm_target);

        // property getter receives the instance; class access exposes the
        // property object itself.
        assert_eq!(getattr(instance, b"pr".as_ptr(), 2), instance);
        assert_eq!(
            value::to_string(getattr(class, b"pr".as_ptr(), 2)),
            "<property object>"
        );
        super::setattr(instance, b"pr".as_ptr(), 2, int(1));
        assert_eq!(error_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
        crate::exc::clear();

        // A property with a setter forwards the assigned value.
        assert_eq!(
            super::setattr(instance, b"prw".as_ptr(), 3, int(7)),
            instance
        );
        assert!(!crate::exc::pending());
        assert_eq!(getattr(instance, b"prw".as_ptr(), 3), instance);

        // `.getter/.setter/.deleter` derive new property objects.
        let pr_object = getattr(class, b"pr".as_ptr(), 2);
        let getter_arg = [pr_get];
        let derived_getter = call(pr_object, b"getter".as_ptr(), 6, 1, getter_arg.as_ptr());
        assert_eq!(value::type_name(derived_getter), "property");
        let setter_arg = [prw_set];
        let derived_setter = call(pr_object, b"setter".as_ptr(), 6, 1, setter_arg.as_ptr());
        assert_eq!(value::type_name(derived_setter), "property");
        assert!(value::class_set_attr(
            class,
            "derived".to_string(),
            derived_setter
        ));
        assert_eq!(
            super::setattr(instance, b"derived".as_ptr(), 7, int(3)),
            instance
        );
        assert!(!crate::exc::pending());

        // `classmethod(SomeClass)` (the `__class_getitem__ = classmethod(...)`
        // shape) binds the owner class as the first forwarded argument.
        let alias = getattr(
            instance,
            b"__class_getitem__".as_ptr(),
            b"__class_getitem__".len(),
        );
        match value::as_function(alias).map(|function| &function.body) {
            Some(value::FunctionBody::Partial { function, args, .. }) => {
                assert_eq!(*function, wrapped_class);
                assert_eq!(args.as_slice(), &[class]);
            }
            _ => panic!("classmethod(class) must produce a class-bound partial"),
        }
        // The binding helper relies on `partial_new` accepting a class target:
        // assert that invariant directly so a future special case cannot route
        // class targets away from the partial representation.
        let direct = value::partial_new(wrapped_class, vec![class]);
        match value::as_function(direct).map(|function| &function.body) {
            Some(value::FunctionBody::Partial { function, args, .. }) => {
                assert_eq!(*function, wrapped_class);
                assert_eq!(args.as_slice(), &[class]);
            }
            _ => panic!("partial_new must keep a class target as a partial"),
        }
        assert!(!crate::exc::pending());
    }
}
