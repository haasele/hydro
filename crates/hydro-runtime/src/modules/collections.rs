//! `collections.defaultdict`.
//!
//! The container keeps insertion-ordered `(key, value)` pairs like `dict`,
//! plus a `default_factory` value. Missing-key subscripting invokes the
//! factory once, inserts the result and returns it. A `None` factory raises
//! `KeyError`, matching CPython.

use std::cmp::Ordering;

use crate::container;
use crate::core::globals;
use crate::core::iter;
use crate::core::methods;
use crate::core::ops;
use crate::exc::{self, ATTRIBUTE_ERROR, INDEX_ERROR, KEY_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::value::{self, FunctionBody, ParamMeta, FALSE, NONE};
use hydro_ir::{MethodKind, ParamKind};

/// State of a `collections.defaultdict`.
pub(crate) struct DefaultDictState {
    pub(crate) factory: i64,
    pub(crate) pairs: Vec<(i64, i64)>,
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn attribute_error(name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("'defaultdict' object has no attribute '{name}'"),
    )
}

/// Collects `source` into insertion-ordered pairs, last value winning for a
/// duplicate key, exactly like `dict(source)`.
///
/// The pairs live only in Rust memory that the collector does not scan, so the
/// caller takes a root marker first and keeps it until
/// [`value::defaultdict_new`] makes the result own every value. This function
/// roots each pair it appends.
fn defaultdict_collect(source: i64) -> Result<Vec<(i64, i64)>, i64> {
    if let Some(pairs) = value::as_dict(source) {
        return Ok(pairs.to_vec());
    }
    if let Some((pairs, _)) = value::as_defaultdict(source) {
        return Ok(pairs.to_vec());
    }
    let iterator = iter::iter_of(source);
    if exc::pending() {
        return Err(NONE);
    }
    let mut pairs: Vec<(i64, i64)> = Vec::new();
    let mut index = 0usize;
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return Err(NONE);
        }
        if status == 0 {
            break;
        }
        let Some(items) = value::as_list(item).or_else(|| value::as_tuple(item)) else {
            return Err(type_error(&format!(
                "cannot convert dictionary update sequence element #{index} to a sequence"
            )));
        };
        if items.len() != 2 {
            return Err(exc::raise_with_message(
                VALUE_ERROR,
                &format!(
                    "dictionary update sequence element #{index} has length {}; 2 is required",
                    items.len()
                ),
            ));
        }
        let (key, entry) = (items[0], items[1]);
        if let Some(name) = container::unhashable_name(key) {
            return Err(type_error(&container::unhashable_message(name)));
        }
        match container::dict_find(&pairs, key) {
            Some(position) => pairs[position].1 = entry,
            None => {
                heap::root_values(&[key, entry]);
                pairs.push((key, entry));
            }
        }
        index += 1;
    }
    Ok(pairs)
}

/// `defaultdict(default_factory=None, /, initial=(), **kwargs)`: the factory
/// must be callable or `None`, the optional second positional argument is a
/// mapping or an iterable of key/value pairs, and every keyword is a data key
/// (the factory name is positional-only in CPython).
fn defaultdict_construct(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let factory = args.first().copied().unwrap_or(NONE);
    if factory != NONE && value::as_function(factory).is_none() {
        return type_error(&format!(
            "defaultdict() first argument must be callable or None, not {}",
            value::type_name(factory)
        ));
    }
    if args.len() > 2 {
        return type_error(&format!(
            "dict expected at most 1 argument, got {}",
            args.len() - 1
        ));
    }
    let mark = heap::root_values(std::slice::from_ref(&factory));
    let mut pairs = match args.get(1).copied() {
        Some(source) => match defaultdict_collect(source) {
            Ok(pairs) => pairs,
            Err(error) => {
                heap::release_roots(mark);
                return error;
            }
        },
        None => Vec::new(),
    };
    for (name, value) in keywords {
        let key = value::str_alloc(name.clone());
        heap::root_values(&[key, *value]);
        match container::dict_find(&pairs, key) {
            Some(position) => pairs[position].1 = *value,
            None => pairs.push((key, *value)),
        }
    }
    let result = value::defaultdict_new(factory, pairs);
    heap::release_roots(mark);
    result
}

pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    if let Some(method) = function.strip_prefix("OrderedDict.") {
        return ordereddict_method(method, args);
    }
    if let Some(method) = function.strip_prefix("Counter.") {
        return counter_method(method, args);
    }
    match function {
        "OrderedDict" => ordereddict_new(args, &[]),
        "Counter" => counter_new(args, &[]),
        "defaultdict" => defaultdict_construct(args, &[]),
        "namedtuple" => namedtuple(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'collections' has no attribute '{function}'"),
        ),
    }
}

/// Keyword-aware dispatch: `defaultdict(list, a=1)` stores the keyword names
/// as keys, `OrderedDict(**items)` keeps the keyword names as keys and
/// `Counter(**kwds)` counts them. No other `collections` constructor accepts
/// dynamic keywords.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match function {
        "defaultdict" => defaultdict_construct(args, keywords),
        "OrderedDict" => ordereddict_new(args, keywords),
        "Counter" => counter_new(args, keywords),
        _ => type_error(&format!(
            "collections.{function}() does not accept keyword arguments"
        )),
    }
}

/// Identity-stable `defaultdict` native function shared by `collections` and
/// `_collections`.
///
/// `module_get_named` normally allocates a fresh native function per access;
/// `collections.defaultdict is _collections.defaultdict` is observable, so the
/// alias must be one value. The cache lives in the globals registry, which
/// roots it for the collector.
pub(crate) fn defaultdict_function() -> i64 {
    if let Some(cached) = globals::registry_get("collections.defaultdict") {
        return cached;
    }
    let function = value::native_function_new("collections", "defaultdict", 1);
    globals::registry_set("collections.defaultdict", function);
    function
}

/// Produces the value stored for a missing key. The caller checks that the
/// factory is not `None`.
fn factory_value(factory: i64) -> i64 {
    let Some(function) = value::as_function(factory) else {
        return type_error("default_factory is not callable");
    };
    if let FunctionBody::Builtin(builtin) = &function.body {
        match builtin {
            hydro_ir::Builtin::Int => return value::encode_int(0).unwrap_or(NONE),
            hydro_ir::Builtin::Float => return value::float_new(0.0),
            hydro_ir::Builtin::Str => return value::str_alloc(String::new()),
            hydro_ir::Builtin::List => return value::list_new(Vec::new()),
            hydro_ir::Builtin::Dict => return value::dict_new(Vec::new()),
            hydro_ir::Builtin::Set => return value::set_new(Vec::new()),
            _ => {}
        }
    }
    crate::builtins::call(factory, 0, std::ptr::null())
}

/// `dd[key]`: returns the stored value, or inserts and returns the default.
pub(crate) fn getitem(dd: i64, key: i64) -> i64 {
    if let Some(name) = container::unhashable_name(key) {
        return type_error(&container::unhashable_message(name));
    }
    let (factory, existing) = {
        let Some((pairs, factory)) = value::as_defaultdict(dd) else {
            return type_error("'defaultdict' object is not subscriptable");
        };
        (
            factory,
            container::dict_find(pairs, key).map(|position| pairs[position].1),
        )
    };
    if let Some(existing) = existing {
        return existing;
    }
    if factory == NONE {
        return exc::raise_with_message(KEY_ERROR, &value::repr(key));
    }
    let default = factory_value(factory);
    if exc::pending() {
        return NONE;
    }
    // Re-check after the factory call: the factory may allocate and collect.
    if let Some(mut state) = value::defaultdict_state_mut(dd) {
        match container::dict_find(&state.pairs, key) {
            Some(position) => return state.pairs[position].1,
            None => {
                container::dict_insert_barrier(dd, &mut state.pairs, key, default);
            }
        }
    }
    default
}

/// `dd[key] = value`.
pub(crate) fn setitem(dd: i64, key: i64, new_value: i64) -> i64 {
    if let Some(name) = container::unhashable_name(key) {
        return type_error(&container::unhashable_message(name));
    }
    if let Some(mut state) = value::defaultdict_state_mut(dd) {
        container::dict_insert_barrier(dd, &mut state.pairs, key, new_value);
    }
    NONE
}

/// `del dd[key]`: removes the entry. A missing key raises `KeyError` with the
/// key's `repr` and unhashable keys raise `TypeError`.
pub(crate) fn delitem(dd: i64, key: i64) -> i64 {
    if let Some(name) = container::unhashable_name(key) {
        return type_error(&container::unhashable_message(name));
    }
    let mut state = match value::defaultdict_state_mut(dd) {
        Some(state) => state,
        None => return type_error("'defaultdict' object does not support item deletion"),
    };
    match container::dict_find(&state.pairs, key) {
        Some(position) => {
            state.pairs.remove(position);
            NONE
        }
        None => {
            drop(state);
            exc::raise_with_message(KEY_ERROR, &value::repr(key))
        }
    }
}

fn check_arity(method: &str, argc: usize, min: usize, max: usize) -> Option<i64> {
    if argc >= min && argc <= max {
        return None;
    }
    let message = if min == max {
        format!("defaultdict.{method}() takes {min} argument(s) ({argc} given)")
    } else {
        format!("defaultdict.{method}() takes {min} to {max} arguments ({argc} given)")
    };
    Some(type_error(&message))
}

fn check_hashable(key: i64) -> Option<i64> {
    container::unhashable_name(key).map(|name| type_error(&container::unhashable_message(name)))
}

/// Builds the `keys()`/`values()`/`items()` result from a pair snapshot.
fn view_list(method: &str, pairs: Vec<(i64, i64)>) -> i64 {
    match method {
        "items" => {
            let result = value::list_new(Vec::new());
            for (key, item) in pairs {
                let pair = value::tuple_new(vec![key, item]);
                if let Some(mut items) = value::list_items_mut(result) {
                    items.push(pair);
                    crate::heap::write_barrier_edge(result, pair);
                }
            }
            result
        }
        "keys" => value::list_new(pairs.into_iter().map(|(key, _)| key).collect()),
        _ => value::list_new(pairs.into_iter().map(|(_, item)| item).collect()),
    }
}

pub(crate) fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    match name {
        "get" => {
            if let Some(err) = check_arity(name, args.len(), 1, 2) {
                return err;
            }
            if let Some(err) = check_hashable(args[0]) {
                return err;
            }
            let Some((pairs, _)) = value::as_defaultdict(obj) else {
                return attribute_error(name);
            };
            match container::dict_find(pairs, args[0]) {
                Some(position) => pairs[position].1,
                None => args.get(1).copied().unwrap_or(NONE),
            }
        }
        "keys" | "values" | "items" => {
            if let Some(err) = check_arity(name, args.len(), 0, 0) {
                return err;
            }
            let pairs = value::as_defaultdict(obj)
                .map(|(pairs, _)| pairs.to_vec())
                .unwrap_or_default();
            view_list(name, pairs)
        }
        "pop" => {
            if let Some(err) = check_arity(name, args.len(), 1, 2) {
                return err;
            }
            if let Some(err) = check_hashable(args[0]) {
                return err;
            }
            let key = args[0];
            let mut state = match value::defaultdict_state_mut(obj) {
                Some(state) => state,
                None => return attribute_error(name),
            };
            match container::dict_find(&state.pairs, key) {
                Some(position) => state.pairs.remove(position).1,
                None => {
                    drop(state);
                    if args.len() == 2 {
                        args[1]
                    } else {
                        exc::raise_with_message(KEY_ERROR, &value::repr(key))
                    }
                }
            }
        }
        "update" => {
            if let Some(err) = check_arity(name, args.len(), 1, 1) {
                return err;
            }
            let pairs = if let Some((pairs, _)) = value::as_defaultdict(args[0]) {
                pairs.to_vec()
            } else if let Some(pairs) = value::as_dict(args[0]) {
                pairs.to_vec()
            } else {
                return type_error(&format!(
                    "'{}' object is not iterable",
                    value::type_name(args[0])
                ));
            };
            if let Some(mut state) = value::defaultdict_state_mut(obj) {
                for (key, item) in pairs {
                    container::dict_insert_barrier(obj, &mut state.pairs, key, item);
                }
            }
            NONE
        }
        "clear" => {
            if let Some(err) = check_arity(name, args.len(), 0, 0) {
                return err;
            }
            if let Some(mut state) = value::defaultdict_state_mut(obj) {
                state.pairs.clear();
            }
            NONE
        }
        "copy" => {
            if let Some(err) = check_arity(name, args.len(), 0, 0) {
                return err;
            }
            let (factory, pairs) = value::as_defaultdict(obj)
                .map(|(pairs, factory)| (factory, pairs.to_vec()))
                .unwrap_or((NONE, Vec::new()));
            value::defaultdict_new(factory, pairs)
        }
        _ => attribute_error(name),
    }
}

/// `default_factory` data attribute.
pub(crate) fn getattr(obj: i64, attr: &str) -> Option<i64> {
    if attr != "default_factory" {
        return None;
    }
    value::as_defaultdict(obj).map(|(_, factory)| factory)
}

// -------------------------------------------------------------------------
// `collections.OrderedDict`
// -------------------------------------------------------------------------

/// Instance attribute holding the insertion-ordered backing dict.
///
/// The key is not a valid Python identifier, so it cannot collide with a
/// method or a keyword passed to the constructor.
const ORDERED_DATA_ATTR: &str = "\u{0}ordered_data";

/// Identity-stable `collections.OrderedDict` class.
///
/// The class object is cached in the globals registry: `collections.OrderedDict
/// is _collections.OrderedDict`, `isinstance` and method binding all require one
/// class value per runtime.
pub(crate) fn ordereddict_class() -> i64 {
    if let Some(cached) = globals::registry_get("collections.OrderedDict.class") {
        return cached;
    }
    let mut methods: Vec<(String, i64, MethodKind)> = Vec::with_capacity(20);
    for (name, arity) in [
        ("__init__", usize::MAX),
        ("__getitem__", 2),
        ("__setitem__", 3),
        ("__delitem__", 2),
        ("__iter__", 1),
        ("__len__", 1),
        ("__contains__", 2),
        ("__eq__", 2),
        ("__repr__", 1),
        ("get", 3),
        ("keys", 1),
        ("values", 1),
        ("items", 1),
        ("move_to_end", 3),
        ("popitem", 2),
        ("pop", 3),
        ("update", 2),
        ("setdefault", 3),
        ("copy", 1),
        ("clear", 1),
    ] {
        let qualified = match name {
            "__init__" => "OrderedDict.__init__",
            "__getitem__" => "OrderedDict.__getitem__",
            "__setitem__" => "OrderedDict.__setitem__",
            "__delitem__" => "OrderedDict.__delitem__",
            "__iter__" => "OrderedDict.__iter__",
            "__len__" => "OrderedDict.__len__",
            "__contains__" => "OrderedDict.__contains__",
            "__eq__" => "OrderedDict.__eq__",
            "__repr__" => "OrderedDict.__repr__",
            "get" => "OrderedDict.get",
            "keys" => "OrderedDict.keys",
            "values" => "OrderedDict.values",
            "items" => "OrderedDict.items",
            "move_to_end" => "OrderedDict.move_to_end",
            "popitem" => "OrderedDict.popitem",
            "pop" => "OrderedDict.pop",
            "update" => "OrderedDict.update",
            "setdefault" => "OrderedDict.setdefault",
            "copy" => "OrderedDict.copy",
            "clear" => "OrderedDict.clear",
            _ => unreachable!(),
        };
        methods.push((
            name.to_string(),
            value::native_function_new("collections", qualified, arity),
            MethodKind::Normal,
        ));
    }
    // `fromkeys` is a classmethod: the binder prepends the owner class.
    methods.push((
        "fromkeys".to_string(),
        value::native_function_new("collections", "OrderedDict.fromkeys", 3),
        MethodKind::Classmethod,
    ));
    let method_values: Vec<i64> = methods.iter().map(|(_, function, _)| *function).collect();
    let method_roots = heap::root_values(&method_values);
    let class = value::class_new_full("OrderedDict".to_string(), NONE, methods, Vec::new());
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    globals::registry_set("collections.OrderedDict.class", class);
    heap::release_roots(class_roots);
    heap::release_roots(method_roots);
    class
}

/// Data dict of an `OrderedDict` (or subclass) instance.
fn ordereddict_data(obj: i64) -> Option<i64> {
    let class = value::instance_class_value(obj)?;
    if !value::is_subclass_of(class, ordereddict_class()) {
        return None;
    }
    value::instance_attr(obj, ORDERED_DATA_ATTR)
}

/// Snapshot of an `OrderedDict`'s `(key, value)` pairs in insertion order.
fn ordereddict_pairs(obj: i64) -> Vec<(i64, i64)> {
    ordereddict_data(obj)
        .and_then(value::as_dict)
        .map(|pairs| pairs.to_vec())
        .unwrap_or_default()
}

/// Replaces the backing dict of `obj`, preserving the pair order in `pairs`.
fn ordereddict_store(obj: i64, pairs: Vec<(i64, i64)>) {
    let data = value::dict_new(pairs);
    value::instance_setattr(obj, ORDERED_DATA_ATTR.to_string(), data);
}

/// Allocates an `OrderedDict` holding `pairs` in order.
fn ordereddict_from_pairs(pairs: Vec<(i64, i64)>) -> i64 {
    let data = value::dict_new(pairs);
    let roots = heap::root_values(std::slice::from_ref(&data));
    let instance = value::instance_new(ordereddict_class());
    value::instance_setattr(instance, ORDERED_DATA_ATTR.to_string(), data);
    heap::release_roots(roots);
    instance
}

/// `(key, value)` from a 2-item tuple/list, for the iterable constructor form.
fn ordereddict_pair(item: i64) -> Option<(i64, i64)> {
    let values = value::as_tuple(item).or_else(|| value::as_list(item))?;
    if values.len() != 2 {
        return None;
    }
    Some((values[0], values[1]))
}

/// Collects the pairs of a mapping or of an iterable of 2-item sequences.
///
/// Every key is checked for hashability, exactly like `dict` construction.
fn ordereddict_collect(source: i64) -> Result<Vec<(i64, i64)>, i64> {
    if let Some(data) = ordereddict_data(source) {
        let Some(pairs) = value::as_dict(data) else {
            return Ok(Vec::new());
        };
        return Ok(pairs.to_vec());
    }
    if let Some(pairs) = value::as_dict(source) {
        return Ok(pairs.to_vec());
    }
    if let Some((pairs, _)) = value::as_defaultdict(source) {
        return Ok(pairs.to_vec());
    }
    let iterator = iter::iter_of(source);
    if exc::pending() {
        return Err(NONE);
    }
    // The collected pairs live in Rust memory that the collector does not
    // scan; the caller takes a root marker before calling so it can release
    // them once the result owns the pairs.
    let mut pairs: Vec<(i64, i64)> = Vec::new();
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return Err(NONE);
        }
        if status == 0 {
            break;
        }
        let Some((key, value)) = ordereddict_pair(item) else {
            return Err(type_error(&format!(
                "cannot convert dictionary update sequence element #{} to a sequence",
                pairs.len()
            )));
        };
        if let Some(name) = container::unhashable_name(key) {
            return Err(type_error(&container::unhashable_message(name)));
        }
        heap::root_values(&[key, value]);
        pairs.push((key, value));
    }
    Ok(pairs)
}

/// `OrderedDict()` / `OrderedDict(iterable_or_mapping, **kwargs)`.
fn ordereddict_new(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let mark = heap::root_values(&[]);
    let mut pairs = if let Some(source) = args.first().copied() {
        match ordereddict_collect(source) {
            Ok(pairs) => pairs,
            Err(err) => {
                heap::release_roots(mark);
                return err;
            }
        }
    } else {
        Vec::new()
    };
    for (name, value) in keywords {
        let key = value::str_alloc(name.clone());
        heap::root_values(&[key, *value]);
        if let Some(position) = container::dict_find(&pairs, key) {
            pairs[position].1 = *value;
        } else {
            pairs.push((key, *value));
        }
    }
    let result = ordereddict_from_pairs(pairs);
    heap::release_roots(mark);
    result
}

/// Shared validation for the mutating methods: only `OrderedDict` instances
/// have this backing store.
fn ordereddict_receiver(method: &str, args: &[i64]) -> Result<(i64, i64), i64> {
    let Some(&obj) = args.first() else {
        return Err(type_error(&format!(
            "OrderedDict.{method}() missing required argument 'self'"
        )));
    };
    match ordereddict_data(obj) {
        Some(data) => Ok((obj, data)),
        None => Err(type_error(&format!(
            "descriptor '{method}' for 'OrderedDict' objects doesn't apply to a '{}' object",
            value::type_name(obj)
        ))),
    }
}

fn ordereddict_items(args: &[i64], name: &str) -> i64 {
    let Ok((_, data)) = ordereddict_receiver(name, args) else {
        return crate::exc::pending_value();
    };
    let pairs = value::as_dict(data)
        .map(|pairs| pairs.to_vec())
        .unwrap_or_default();
    view_list(name, pairs)
}

/// Implements the native `OrderedDict` method bodies.
fn ordereddict_method(name: &str, args: &[i64]) -> i64 {
    match name {
        "__init__" => {
            let Some(&obj) = args.first() else {
                return type_error("OrderedDict.__init__() missing self");
            };
            if ordereddict_data(obj).is_some() {
                // Re-initialisation replaces the contents, like dict.__init__.
                if let Some(source) = args.get(1).copied() {
                    return ordereddict_method("update", &[obj, source]);
                }
                return NONE;
            }
            let mark = heap::root_values(&[]);
            let pairs = match args.get(1).copied() {
                Some(source) => match ordereddict_collect(source) {
                    Ok(pairs) => pairs,
                    Err(err) => {
                        heap::release_roots(mark);
                        return err;
                    }
                },
                None => Vec::new(),
            };
            let data = value::dict_new(pairs);
            value::instance_setattr(obj, ORDERED_DATA_ATTR.to_string(), data);
            heap::release_roots(mark);
            NONE
        }
        "fromkeys" => {
            // `OrderedDict.fromkeys(iterable, value=None)`: the classmethod
            // binder passes the owner class first.
            let Some(&iterable) = args.get(1) else {
                return type_error("OrderedDict.fromkeys() missing required argument 'iterable'");
            };
            let default = args.get(2).copied().unwrap_or(NONE);
            let iterator = iter::iter_of(iterable);
            if exc::pending() {
                return NONE;
            }
            let mark = heap::root_values(&[]);
            let mut pairs: Vec<(i64, i64)> = Vec::new();
            loop {
                let mut key = NONE;
                let status = iter::iter_next(iterator, &mut key);
                if exc::pending() {
                    heap::release_roots(mark);
                    return NONE;
                }
                if status == 0 {
                    break;
                }
                if let Some(unhashable) = container::unhashable_name(key) {
                    heap::release_roots(mark);
                    return type_error(&container::unhashable_message(unhashable));
                }
                if container::dict_find(&pairs, key).is_none() {
                    heap::root_values(&[key, default]);
                    pairs.push((key, default));
                }
            }
            let result = ordereddict_from_pairs(pairs);
            heap::release_roots(mark);
            result
        }
        "__getitem__" | "__delitem__" | "pop" => {
            let Ok((obj, data)) = ordereddict_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let Some(&key) = args.get(1) else {
                return type_error(&format!("OrderedDict.{name}() missing required argument"));
            };
            if let Some(unhashable) = container::unhashable_name(key) {
                return type_error(&container::unhashable_message(unhashable));
            }
            let pairs = value::as_dict(data)
                .map(|pairs| pairs.to_vec())
                .unwrap_or_default();
            let Some(position) = container::dict_find(&pairs, key) else {
                if name == "pop" {
                    if let Some(&default) = args.get(2) {
                        return default;
                    }
                }
                return exc::raise_with_message(KEY_ERROR, &value::repr(key));
            };
            let (_, existing) = pairs[position];
            if name == "__getitem__" {
                return existing;
            }
            let mut remaining = pairs;
            remaining.remove(position);
            ordereddict_store(obj, remaining);
            if name == "pop" {
                existing
            } else {
                NONE
            }
        }
        "__setitem__" => {
            let Ok((_, data)) = ordereddict_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let (Some(&key), Some(&new_value)) = (args.get(1), args.get(2)) else {
                return type_error("OrderedDict.__setitem__() requires a key and a value");
            };
            if let Some(unhashable) = container::unhashable_name(key) {
                return type_error(&container::unhashable_message(unhashable));
            }
            if let Some(mut pairs) = value::dict_pairs_mut(data) {
                container::dict_insert_barrier(data, &mut *pairs, key, new_value);
            }
            NONE
        }
        "__iter__" => {
            let Ok((_, data)) = ordereddict_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let keys: Vec<i64> = value::as_dict(data)
                .map(|pairs| pairs.iter().map(|(key, _)| *key).collect())
                .unwrap_or_default();
            iter::iter_new(value::list_new(keys))
        }
        "__len__" => {
            let Ok((_, data)) = ordereddict_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            value::encode_int(value::as_dict(data).map_or(0, |pairs| pairs.len() as i64))
                .unwrap_or(NONE)
        }
        "__contains__" => {
            let Ok((_, data)) = ordereddict_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let Some(&key) = args.get(1) else {
                return value::bool_value(false);
            };
            if let Some(unhashable) = container::unhashable_name(key) {
                return type_error(&container::unhashable_message(unhashable));
            }
            let found = value::as_dict(data)
                .and_then(|pairs| container::dict_find(pairs, key))
                .is_some();
            value::bool_value(found)
        }
        "__eq__" => {
            let Some(&obj) = args.first() else {
                return value::NOT_IMPLEMENTED;
            };
            let Some(&other) = args.get(1) else {
                return value::NOT_IMPLEMENTED;
            };
            let Some(left_data) = ordereddict_data(obj) else {
                return value::NOT_IMPLEMENTED;
            };
            let Some(left) = value::as_dict(left_data) else {
                return value::NOT_IMPLEMENTED;
            };
            if let Some(right) = ordereddict_data(other).and_then(value::as_dict) {
                let equal = left.len() == right.len()
                    && left.iter().zip(right.iter()).all(|(&(lk, lv), &(rk, rv))| {
                        container::equals(lk, rk) && container::equals(lv, rv)
                    });
                return value::bool_value(equal);
            }
            if value::as_dict(other).is_some() || value::as_defaultdict(other).is_some() {
                return value::bool_value(container::equals(left_data, other));
            }
            value::NOT_IMPLEMENTED
        }
        "__repr__" => {
            let Ok((_, data)) = ordereddict_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let pairs = value::as_dict(data)
                .map(|pairs| pairs.to_vec())
                .unwrap_or_default();
            if pairs.is_empty() {
                return value::str_alloc("OrderedDict()".to_string());
            }
            let roots = heap::root_values(
                &pairs
                    .iter()
                    .flat_map(|(key, item)| [*key, *item])
                    .collect::<Vec<_>>(),
            );
            let mut rendered = Vec::with_capacity(pairs.len());
            for (key, item) in &pairs {
                rendered.push(format!("({}, {})", value::repr(*key), value::repr(*item)));
            }
            heap::release_roots(roots);
            value::str_alloc(format!("OrderedDict([{}])", rendered.join(", ")))
        }
        "get" => {
            let Ok((_, data)) = ordereddict_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let Some(&key) = args.get(1) else {
                return type_error("OrderedDict.get() missing required argument 'key'");
            };
            if let Some(unhashable) = container::unhashable_name(key) {
                return type_error(&container::unhashable_message(unhashable));
            }
            let default = args.get(2).copied().unwrap_or(NONE);
            match value::as_dict(data).and_then(|pairs| container::dict_find(pairs, key)) {
                Some(position) => value::as_dict(data).expect("checked")[position].1,
                None => default,
            }
        }
        "keys" | "values" | "items" => ordereddict_items(args, name),
        "move_to_end" => {
            let Ok((obj, _)) = ordereddict_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let Some(&key) = args.get(1) else {
                return type_error("OrderedDict.move_to_end() missing required argument 'key'");
            };
            if let Some(unhashable) = container::unhashable_name(key) {
                return type_error(&container::unhashable_message(unhashable));
            }
            let last = args.get(2).copied().is_none_or(value::truthy);
            let mut pairs = ordereddict_pairs(obj);
            let Some(position) = container::dict_find(&pairs, key) else {
                return exc::raise_with_message(KEY_ERROR, &value::repr(key));
            };
            let pair = pairs.remove(position);
            if last {
                pairs.push(pair);
            } else {
                pairs.insert(0, pair);
            }
            ordereddict_store(obj, pairs);
            NONE
        }
        "popitem" => {
            let Ok((obj, _)) = ordereddict_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let last = args.get(1).copied().is_none_or(value::truthy);
            let mut pairs = ordereddict_pairs(obj);
            if pairs.is_empty() {
                return exc::raise_with_message(KEY_ERROR, "dictionary is empty");
            }
            let (key, item) = if last {
                pairs.pop().expect("non-empty")
            } else {
                pairs.remove(0)
            };
            ordereddict_store(obj, pairs);
            value::tuple_new(vec![key, item])
        }
        "update" => {
            let Ok((_, data)) = ordereddict_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let Some(&source) = args.get(1) else {
                return NONE;
            };
            let mark = heap::root_values(&[]);
            let incoming = match ordereddict_collect(source) {
                Ok(pairs) => pairs,
                Err(err) => {
                    heap::release_roots(mark);
                    return err;
                }
            };
            if let Some(mut pairs) = value::dict_pairs_mut(data) {
                for (key, item) in incoming {
                    container::dict_insert_barrier(data, &mut *pairs, key, item);
                }
            }
            heap::release_roots(mark);
            NONE
        }
        "setdefault" => {
            let Ok((_, data)) = ordereddict_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let Some(&key) = args.get(1) else {
                return type_error("OrderedDict.setdefault() missing required argument 'key'");
            };
            if let Some(unhashable) = container::unhashable_name(key) {
                return type_error(&container::unhashable_message(unhashable));
            }
            let default = args.get(2).copied().unwrap_or(NONE);
            if let Some(pairs) = value::as_dict(data) {
                if let Some(position) = container::dict_find(pairs, key) {
                    return pairs[position].1;
                }
            }
            if let Some(mut pairs) = value::dict_pairs_mut(data) {
                container::dict_insert_barrier(data, &mut *pairs, key, default);
            }
            default
        }
        "copy" => {
            let Ok((obj, _)) = ordereddict_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            ordereddict_from_pairs(ordereddict_pairs(obj))
        }
        "clear" => {
            let Ok((_, data)) = ordereddict_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            if let Some(mut pairs) = value::dict_pairs_mut(data) {
                pairs.clear();
            }
            NONE
        }
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("'OrderedDict' object has no attribute '{name}'"),
        ),
    }
}

// -------------------------------------------------------------------------
// `collections.Counter`
// -------------------------------------------------------------------------

/// Instance attribute holding the counter's `(element, count)` dict.
const COUNTER_DATA_ATTR: &str = "\u{0}counter_data";

/// Identity-stable `collections.Counter` class.
///
/// `Counter` is a `dict` subclass: the class derives from the cached native
/// `dict` class, so `isinstance(Counter(), dict)` follows the dict ancestry.
/// The counts live in an instance-owned dict, which keeps insertion order and
/// key hashing identical to `dict`.
pub(crate) fn counter_class() -> i64 {
    if let Some(cached) = globals::registry_get("collections.Counter.class") {
        return cached;
    }
    let base = value::type_class(value::dict_new(Vec::new()));
    let mut methods: Vec<(String, i64, MethodKind)> = Vec::with_capacity(28);
    for (name, qualified, arity) in [
        ("__init__", "Counter.__init__", usize::MAX),
        ("__getitem__", "Counter.__getitem__", 2),
        ("__missing__", "Counter.__missing__", 2),
        ("__setitem__", "Counter.__setitem__", 3),
        ("__delitem__", "Counter.__delitem__", 2),
        ("__iter__", "Counter.__iter__", 1),
        ("__len__", "Counter.__len__", 1),
        ("__contains__", "Counter.__contains__", 2),
        ("__eq__", "Counter.__eq__", 2),
        ("__ne__", "Counter.__ne__", 2),
        ("__add__", "Counter.__add__", 2),
        ("__sub__", "Counter.__sub__", 2),
        ("__repr__", "Counter.__repr__", 1),
        ("__str__", "Counter.__str__", 1),
        ("get", "Counter.get", 3),
        ("keys", "Counter.keys", 1),
        ("values", "Counter.values", 1),
        ("items", "Counter.items", 1),
        ("update", "Counter.update", 2),
        ("subtract", "Counter.subtract", 2),
        ("elements", "Counter.elements", 1),
        ("most_common", "Counter.most_common", 2),
        ("total", "Counter.total", 1),
        ("copy", "Counter.copy", 1),
        ("clear", "Counter.clear", 1),
        ("pop", "Counter.pop", 3),
        ("popitem", "Counter.popitem", 1),
        ("setdefault", "Counter.setdefault", 3),
    ] {
        methods.push((
            name.to_string(),
            value::native_function_new("collections", qualified, arity),
            MethodKind::Normal,
        ));
    }
    let method_values: Vec<i64> = methods.iter().map(|(_, function, _)| *function).collect();
    let method_roots = heap::root_values(&method_values);
    let class = value::class_new_full("Counter".to_string(), base, methods, Vec::new());
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    globals::registry_set("collections.Counter.class", class);
    heap::release_roots(class_roots);
    heap::release_roots(method_roots);
    class
}

/// The counts dict of a `Counter` (or subclass) instance.
fn counter_data(obj: i64) -> Option<i64> {
    let class = value::instance_class_value(obj)?;
    if !value::is_subclass_of(class, counter_class()) {
        return None;
    }
    value::instance_attr(obj, COUNTER_DATA_ATTR)
}

/// Whether `v` is a `Counter` instance, for the `dict` `isinstance` check.
///
/// Uses the cached class (never creating it) so `isinstance(x, dict)` stays
/// cheap for values that have nothing to do with counters.
pub(crate) fn is_counter_instance(v: i64) -> bool {
    let Some(class) = value::instance_class_value(v) else {
        return false;
    };
    let Some(counter) = globals::registry_get("collections.Counter.class") else {
        return false;
    };
    value::is_subclass_of(class, counter)
}

/// Snapshot of a counter's `(element, count)` pairs in insertion order.
fn counter_pairs(obj: i64) -> Vec<(i64, i64)> {
    counter_data(obj)
        .and_then(value::as_dict)
        .map(|pairs| pairs.to_vec())
        .unwrap_or_default()
}

/// Replaces the counts dict, preserving the order of `pairs`.
fn counter_store(obj: i64, pairs: Vec<(i64, i64)>) {
    let data = value::dict_new(pairs);
    value::instance_setattr(obj, COUNTER_DATA_ATTR.to_string(), data);
}

/// Allocates a counter of `class` holding `pairs` in order.
fn counter_alloc(class: i64, pairs: Vec<(i64, i64)>) -> i64 {
    let data = value::dict_new(pairs);
    let roots = heap::root_values(std::slice::from_ref(&data));
    let instance = value::instance_new(class);
    value::instance_setattr(instance, COUNTER_DATA_ATTR.to_string(), data);
    heap::release_roots(roots);
    instance
}

/// Allocates a `Counter` (not a subclass) holding `pairs`.
fn counter_from_pairs(pairs: Vec<(i64, i64)>) -> i64 {
    counter_alloc(counter_class(), pairs)
}

fn counter_zero() -> i64 {
    value::encode_int(0).unwrap_or(NONE)
}

fn counter_find(pairs: &[(i64, i64)], key: i64) -> Option<i64> {
    container::dict_find(pairs, key).map(|position| pairs[position].1)
}

/// Count of `key`, with `0` for a missing key.
fn counter_count(obj: i64, key: i64) -> i64 {
    counter_data(obj)
        .and_then(value::as_dict)
        .and_then(|pairs| counter_find(pairs, key))
        .unwrap_or_else(counter_zero)
}

fn counter_insert(obj: i64, key: i64, count: i64) {
    let Some(data) = counter_data(obj) else {
        return;
    };
    if let Some(mut pairs) = value::dict_pairs_mut(data) {
        container::dict_insert_barrier(data, &mut *pairs, key, count);
    }
}

/// Adds `delta` to `key`'s count. `false` means an exception is pending.
fn counter_bump(obj: i64, key: i64, delta: i64) -> bool {
    let current = counter_count(obj, key);
    let next = ops::add(current, delta);
    if exc::pending() {
        return false;
    }
    counter_insert(obj, key, next);
    true
}

/// The `(key, value)` pairs of a mapping source, or `None` for an iterable.
fn counter_mapping(source: i64) -> Option<Vec<(i64, i64)>> {
    if value::instance_class_value(source).is_some() {
        if let Some(data) = counter_data(source) {
            return value::as_dict(data).map(|pairs| pairs.to_vec());
        }
        if let Some(data) = ordereddict_data(source) {
            return value::as_dict(data).map(|pairs| pairs.to_vec());
        }
    }
    if let Some(pairs) = value::as_dict(source) {
        return Some(pairs.to_vec());
    }
    value::as_defaultdict(source).map(|(pairs, _)| pairs.to_vec())
}

/// Merges a mapping's counts into `obj`: `update` adds, `subtract` removes.
fn counter_merge_mapping(obj: i64, pairs: &[(i64, i64)], subtract: bool) -> bool {
    for &(key, count) in pairs {
        let current = counter_count(obj, key);
        let next = if subtract {
            ops::sub(current, count)
        } else {
            ops::add(current, count)
        };
        if exc::pending() {
            return false;
        }
        counter_insert(obj, key, next);
    }
    true
}

/// `update`/`subtract` body. `false` means an exception is pending.
fn counter_update_counts(obj: i64, source: i64, subtract: bool) -> bool {
    if let Some(pairs) = counter_mapping(source) {
        return counter_merge_mapping(obj, &pairs, subtract);
    }
    let iterator = iter::iter_of(source);
    if exc::pending() {
        return false;
    }
    let delta = if subtract {
        value::encode_int(-1).unwrap_or(NONE)
    } else {
        value::encode_int(1).unwrap_or(NONE)
    };
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return false;
        }
        if status == 0 {
            break;
        }
        if let Some(name) = container::unhashable_name(item) {
            type_error(&container::unhashable_message(name));
            return false;
        }
        if !counter_bump(obj, item, delta) {
            return false;
        }
    }
    true
}

/// Whether count `v` is greater than zero; `None` means an exception.
fn counter_positive(v: i64) -> Option<bool> {
    if let Some(count) = value::as_integer(v) {
        return Some(count > 0);
    }
    let result = ops::binop(ops::OP_GT, v, counter_zero());
    if exc::pending() {
        return None;
    }
    Some(value::truthy(result))
}

/// Whether count `v` is less than zero; `None` means an exception.
fn counter_negative(v: i64) -> Option<bool> {
    if let Some(count) = value::as_integer(v) {
        return Some(count < 0);
    }
    let result = ops::binop(ops::OP_LT, v, counter_zero());
    if exc::pending() {
        return None;
    }
    Some(value::truthy(result))
}

/// Descending order of two counts (the order `most_common` wants).
fn counter_count_order(a: i64, b: i64) -> Option<Ordering> {
    if container::equals(a, b) {
        return Some(Ordering::Equal);
    }
    let result = ops::binop(ops::OP_GT, a, b);
    if exc::pending() {
        return None;
    }
    if value::truthy(result) {
        Some(Ordering::Less)
    } else {
        Some(Ordering::Greater)
    }
}

/// `Counter.__init__`: clears the counts and applies the optional source.
fn counter_init(args: &[i64]) -> i64 {
    let Some(&obj) = args.first() else {
        return type_error("Counter.__init__() missing self");
    };
    let Some(class) = value::instance_class_value(obj) else {
        return type_error("Counter.__init__() requires a Counter instance");
    };
    if !value::is_subclass_of(class, counter_class()) {
        return type_error(&format!(
            "descriptor '__init__' for 'Counter' objects doesn't apply to a '{}' object",
            value::type_name_of(obj)
        ));
    }
    counter_store(obj, Vec::new());
    if let Some(&source) = args.get(1) {
        if !counter_update_counts(obj, source, false) {
            return NONE;
        }
    }
    NONE
}

/// `Counter(iterable_or_mapping=None, /, **kwds)`.
fn counter_new(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let instance = counter_from_pairs(Vec::new());
    let roots = heap::root_values(std::slice::from_ref(&instance));
    if let Some(&source) = args.first() {
        if !counter_update_counts(instance, source, false) {
            heap::release_roots(roots);
            return NONE;
        }
    }
    if !keywords.is_empty() {
        let mut pairs: Vec<(i64, i64)> = Vec::with_capacity(keywords.len());
        let mut live: Vec<i64> = Vec::with_capacity(keywords.len() * 2);
        for (name, value) in keywords {
            let key = value::str_alloc(name.clone());
            live.push(key);
            live.push(*value);
            pairs.push((key, *value));
        }
        let keyword_roots = heap::root_values(&live);
        let merged = counter_merge_mapping(instance, &pairs, false);
        heap::release_roots(keyword_roots);
        if !merged {
            heap::release_roots(roots);
            return NONE;
        }
    }
    heap::release_roots(roots);
    instance
}

fn counter_receiver(method: &str, args: &[i64]) -> Result<(i64, i64), i64> {
    let Some(&obj) = args.first() else {
        return Err(type_error(&format!(
            "Counter.{method}() missing required argument 'self'"
        )));
    };
    match counter_data(obj) {
        Some(data) => Ok((obj, data)),
        None => Err(type_error(&format!(
            "descriptor '{method}' for 'Counter' objects doesn't apply to a '{}' object",
            value::type_name_of(obj)
        ))),
    }
}

fn counter_key(args: &[i64], method: &str) -> Result<i64, i64> {
    let Some(&key) = args.get(1) else {
        return Err(type_error(&format!(
            "Counter.{method}() missing required argument 'key'"
        )));
    };
    if let Some(err) = check_hashable(key) {
        return Err(err);
    }
    Ok(key)
}

/// `keys()`/`values()`/`items()` from the counts snapshot.
fn counter_items(args: &[i64], name: &str) -> i64 {
    let Ok((obj, _)) = counter_receiver(name, args) else {
        return crate::exc::pending_value();
    };
    view_list(name, counter_pairs(obj))
}

/// `c + other` / `c - other`: the combined positive counts of two counters.
fn counter_arith(obj: i64, other: i64, subtract: bool) -> i64 {
    if counter_data(other).is_none() {
        return value::NOT_IMPLEMENTED;
    }
    let mark = heap::root_values(&[]);
    let left = counter_pairs(obj);
    let right = counter_pairs(other);
    let mut live: Vec<i64> = Vec::with_capacity((left.len() + right.len()) * 2);
    for &(key, count) in left.iter().chain(right.iter()) {
        live.push(key);
        live.push(count);
    }
    heap::root_values(&live);
    let result = counter_from_pairs(Vec::new());
    heap::root_values(std::slice::from_ref(&result));
    let zero = counter_zero();
    for &(key, count) in &left {
        let existing = counter_find(&right, key).unwrap_or(zero);
        let combined = if subtract {
            ops::sub(count, existing)
        } else {
            ops::add(count, existing)
        };
        if exc::pending() {
            heap::release_roots(mark);
            return NONE;
        }
        if counter_positive(combined) == Some(true) {
            counter_insert(result, key, combined);
        }
        if exc::pending() {
            heap::release_roots(mark);
            return NONE;
        }
    }
    for &(key, count) in &right {
        if container::dict_find(&left, key).is_some() {
            continue;
        }
        let keep = if subtract {
            counter_negative(count)
        } else {
            counter_positive(count)
        };
        match keep {
            None => {
                heap::release_roots(mark);
                return NONE;
            }
            Some(true) => {
                // `other`'s negative counts turn into positive counts on
                // subtraction.
                let kept = if subtract {
                    let negated = ops::sub(zero, count);
                    if exc::pending() {
                        heap::release_roots(mark);
                        return NONE;
                    }
                    negated
                } else {
                    count
                };
                counter_insert(result, key, kept);
            }
            Some(false) => {}
        }
    }
    heap::release_roots(mark);
    result
}

/// `c == other`: multiset equality for counters, dict equality otherwise.
fn counter_equals(obj: i64, other: i64) -> i64 {
    let Some(data) = counter_data(obj) else {
        return value::NOT_IMPLEMENTED;
    };
    let Some(left) = value::as_dict(data) else {
        return value::NOT_IMPLEMENTED;
    };
    if let Some(right_data) = counter_data(other) {
        let Some(right) = value::as_dict(right_data) else {
            return value::bool_value(false);
        };
        // A missing key counts as zero, so `Counter({'a': 0}) == Counter()`.
        let equal = left.iter().all(|&(key, count)| {
            counter_find(right, key).is_none_or(|other| container::equals(count, other))
        }) && right.iter().all(|&(key, count)| {
            counter_find(left, key).is_none_or(|other| container::equals(count, other))
        });
        return value::bool_value(equal);
    }
    if value::as_dict(other).is_some() || value::as_defaultdict(other).is_some() {
        return value::bool_value(container::equals(data, other));
    }
    value::NOT_IMPLEMENTED
}

/// `repr(c)` / `str(c)`: `Counter({element: count, ...})`, like `dict`.
fn counter_repr(obj: i64) -> i64 {
    let pairs = counter_pairs(obj);
    let class_name = value::instance_class_name(obj)
        .unwrap_or("Counter")
        .to_string();
    if pairs.is_empty() {
        return value::str_alloc(format!("{class_name}()"));
    }
    let roots = heap::root_values(
        &pairs
            .iter()
            .flat_map(|&(key, count)| [key, count])
            .collect::<Vec<_>>(),
    );
    let rendered: Vec<String> = pairs
        .iter()
        .map(|&(key, count)| format!("{}: {}", value::repr(key), value::repr(count)))
        .collect();
    heap::release_roots(roots);
    value::str_alloc(format!("{class_name}({{{}}})", rendered.join(", ")))
}

/// `c.most_common(n=None)`: pairs ordered by descending count, ties in
/// insertion order.
fn counter_most_common(args: &[i64]) -> i64 {
    let Ok((obj, _)) = counter_receiver("most_common", args) else {
        return crate::exc::pending_value();
    };
    let requested = args.get(1).copied().unwrap_or(NONE);
    let limit = if requested == NONE {
        None
    } else {
        match value::as_integer(requested) {
            Some(count) => Some(count),
            None => {
                return type_error(&format!(
                    "'{}' object cannot be interpreted as an integer",
                    value::type_name(requested)
                ));
            }
        }
    };
    let mark = heap::root_values(&[]);
    let mut pairs = counter_pairs(obj);
    let live: Vec<i64> = pairs
        .iter()
        .flat_map(|&(key, count)| [key, count])
        .collect();
    heap::root_values(&live);
    let mut failure = false;
    pairs.sort_by(|a, b| {
        if failure {
            return Ordering::Equal;
        }
        match counter_count_order(a.1, b.1) {
            Some(order) => order,
            None => {
                failure = true;
                Ordering::Equal
            }
        }
    });
    if failure {
        heap::release_roots(mark);
        return NONE;
    }
    if let Some(limit) = limit {
        if limit <= 0 {
            pairs.clear();
        } else if (limit as usize) < pairs.len() {
            pairs.truncate(limit as usize);
        }
    }
    let result = value::list_new(Vec::new());
    heap::root_values(std::slice::from_ref(&result));
    for (key, count) in pairs {
        let pair = value::tuple_new(vec![key, count]);
        if let Some(mut items) = value::list_items_mut(result) {
            items.push(pair);
            crate::heap::write_barrier_edge(result, pair);
        }
    }
    heap::release_roots(mark);
    result
}

/// `c.elements()`: each element repeated by its positive count.
fn counter_elements(args: &[i64]) -> i64 {
    let Ok((obj, _)) = counter_receiver("elements", args) else {
        return crate::exc::pending_value();
    };
    let mark = heap::root_values(&[]);
    let pairs = counter_pairs(obj);
    let live: Vec<i64> = pairs
        .iter()
        .flat_map(|&(key, count)| [key, count])
        .collect();
    heap::root_values(&live);
    let items = value::list_new(Vec::new());
    heap::root_values(std::slice::from_ref(&items));
    for (key, count) in pairs {
        let Some(repeat) = value::as_integer(count) else {
            heap::release_roots(mark);
            return type_error(&format!(
                "'{}' object cannot be interpreted as an integer",
                value::type_name(count)
            ));
        };
        for _ in 0..repeat.max(0) {
            if let Some(mut stored) = value::list_items_mut(items) {
                stored.push(key);
                crate::heap::write_barrier_edge(items, key);
            }
        }
    }
    let iterator = iter::iter_new(items);
    heap::release_roots(mark);
    iterator
}

/// `c.total()`: the sum of the counts.
fn counter_total(args: &[i64]) -> i64 {
    let Ok((obj, _)) = counter_receiver("total", args) else {
        return crate::exc::pending_value();
    };
    let mut total = counter_zero();
    for (_, count) in counter_pairs(obj) {
        total = ops::add(total, count);
        if exc::pending() {
            return NONE;
        }
    }
    total
}

/// Implements the native `Counter` method bodies.
fn counter_method(name: &str, args: &[i64]) -> i64 {
    match name {
        "__init__" => counter_init(args),
        "__getitem__" => {
            let Ok((obj, _)) = counter_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let key = match counter_key(args, name) {
                Ok(key) => key,
                Err(err) => return err,
            };
            counter_data(obj)
                .and_then(value::as_dict)
                .and_then(|pairs| counter_find(pairs, key))
                .unwrap_or_else(counter_zero)
        }
        "__missing__" => {
            let Ok((_, _)) = counter_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            counter_zero()
        }
        "__setitem__" => {
            if counter_receiver(name, args).is_err() {
                return crate::exc::pending_value();
            }
            let (Some(&key), Some(&count)) = (args.get(1), args.get(2)) else {
                return type_error("Counter.__setitem__() requires a key and a count");
            };
            if let Some(err) = check_hashable(key) {
                return err;
            }
            counter_insert(args[0], key, count);
            NONE
        }
        "__delitem__" => {
            let Ok((obj, _)) = counter_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let key = match counter_key(args, name) {
                Ok(key) => key,
                Err(err) => return err,
            };
            let mut pairs = counter_pairs(obj);
            match container::dict_find(&pairs, key) {
                Some(position) => {
                    pairs.remove(position);
                    counter_store(obj, pairs);
                    NONE
                }
                None => exc::raise_with_message(KEY_ERROR, &value::repr(key)),
            }
        }
        "__iter__" => {
            let Ok((obj, _)) = counter_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let keys: Vec<i64> = counter_pairs(obj).into_iter().map(|(key, _)| key).collect();
            iter::iter_new(value::list_new(keys))
        }
        "__len__" => {
            let Ok((obj, _)) = counter_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            value::encode_int(counter_pairs(obj).len() as i64).unwrap_or(NONE)
        }
        "__contains__" => {
            let Ok((obj, _)) = counter_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let Some(&key) = args.get(1) else {
                return value::bool_value(false);
            };
            if let Some(err) = check_hashable(key) {
                return err;
            }
            let found = counter_data(obj)
                .and_then(value::as_dict)
                .and_then(|pairs| container::dict_find(pairs, key))
                .is_some();
            value::bool_value(found)
        }
        "__eq__" => {
            let (Some(&obj), Some(&other)) = (args.first(), args.get(1)) else {
                return value::NOT_IMPLEMENTED;
            };
            counter_equals(obj, other)
        }
        "__ne__" => {
            let (Some(&obj), Some(&other)) = (args.first(), args.get(1)) else {
                return value::NOT_IMPLEMENTED;
            };
            let equal = counter_equals(obj, other);
            if exc::pending() || equal == value::NOT_IMPLEMENTED {
                return equal;
            }
            value::bool_value(!value::truthy(equal))
        }
        "__add__" | "__sub__" => {
            let (Some(&obj), Some(&other)) = (args.first(), args.get(1)) else {
                return value::NOT_IMPLEMENTED;
            };
            counter_arith(obj, other, name == "__sub__")
        }
        "__repr__" | "__str__" => {
            let Ok((obj, _)) = counter_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            counter_repr(obj)
        }
        "get" => {
            let Ok((obj, _)) = counter_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let key = match counter_key(args, name) {
                Ok(key) => key,
                Err(err) => return err,
            };
            match counter_data(obj)
                .and_then(value::as_dict)
                .and_then(|pairs| counter_find(pairs, key))
            {
                Some(count) => count,
                None => args.get(2).copied().unwrap_or(NONE),
            }
        }
        "keys" | "values" | "items" => counter_items(args, name),
        "update" | "subtract" => {
            let Ok((obj, _)) = counter_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let Some(&source) = args.get(1) else {
                return NONE;
            };
            if !counter_update_counts(obj, source, name == "subtract") {
                return NONE;
            }
            NONE
        }
        "elements" => counter_elements(args),
        "most_common" => counter_most_common(args),
        "total" => counter_total(args),
        "copy" => {
            let Ok((obj, _)) = counter_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let class = value::instance_class_value(obj).unwrap_or_else(counter_class);
            let mark = heap::root_values(&[]);
            let pairs = counter_pairs(obj);
            let live: Vec<i64> = pairs
                .iter()
                .flat_map(|&(key, count)| [key, count])
                .collect();
            heap::root_values(&live);
            let result = counter_alloc(class, pairs);
            heap::release_roots(mark);
            result
        }
        "clear" => {
            if counter_receiver(name, args).is_err() {
                return crate::exc::pending_value();
            }
            counter_store(args[0], Vec::new());
            NONE
        }
        "pop" => {
            let Ok((obj, _)) = counter_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let key = match counter_key(args, name) {
                Ok(key) => key,
                Err(err) => return err,
            };
            let mut pairs = counter_pairs(obj);
            match container::dict_find(&pairs, key) {
                Some(position) => {
                    let (_, count) = pairs.remove(position);
                    counter_store(obj, pairs);
                    count
                }
                None => {
                    if args.len() > 2 {
                        args[2]
                    } else {
                        exc::raise_with_message(KEY_ERROR, &value::repr(key))
                    }
                }
            }
        }
        "popitem" => {
            let Ok((obj, _)) = counter_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let mut pairs = counter_pairs(obj);
            let Some((key, count)) = pairs.pop() else {
                return exc::raise_with_message(KEY_ERROR, "dictionary is empty");
            };
            let pair = value::tuple_new(vec![key, count]);
            counter_store(obj, pairs);
            pair
        }
        "setdefault" => {
            let Ok((obj, _)) = counter_receiver(name, args) else {
                return crate::exc::pending_value();
            };
            let key = match counter_key(args, name) {
                Ok(key) => key,
                Err(err) => return err,
            };
            if let Some(existing) = counter_data(obj)
                .and_then(value::as_dict)
                .and_then(|pairs| counter_find(pairs, key))
            {
                return existing;
            }
            let default = args.get(2).copied().unwrap_or(NONE);
            counter_insert(obj, key, default);
            default
        }
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("'Counter' object has no attribute '{name}'"),
        ),
    }
}

// -------------------------------------------------------------------------
// `collections.namedtuple`
// -------------------------------------------------------------------------

/// Python keywords that cannot name a namedtuple field.
const NAMEDTUPLE_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// Whether `name` is a valid Python identifier in the ASCII/Unicode sense the
/// compiler accepts for class and field names.
fn is_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if first != '_' && !first.is_alphabetic() {
        return false;
    }
    characters.all(|character| character == '_' || character.is_alphanumeric())
}

fn is_keyword(name: &str) -> bool {
    NAMEDTUPLE_KEYWORDS.contains(&name)
}

/// Keeps `value` alive while the class under construction owns nothing yet.
///
/// The caller releases every root registered since its own marker once the
/// class exists. A no-op outside the runtime (no collector can run then).
fn keep(live: &mut Vec<i64>, value: i64) -> i64 {
    live.push(value);
    heap::root_values(std::slice::from_ref(&value));
    value
}

/// `_fields` of a namedtuple class, as the live string objects.
fn field_name_values(class: i64) -> Vec<i64> {
    match value::class_lookup(class, "_fields") {
        Some(value::ClassEntry::Attr(fields)) => value::as_tuple(fields)
            .map(|items| items.to_vec())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// `_fields` of a namedtuple class, in declaration order.
fn field_names(class: i64) -> Vec<String> {
    field_name_values(class)
        .iter()
        .filter_map(|&name| value::as_str(name).map(str::to_string))
        .collect()
}

/// Field values of a namedtuple instance in field order.
fn field_values(instance: i64) -> Vec<i64> {
    let Some(state) = value::as_instance(instance) else {
        return Vec::new();
    };
    field_names(state.class)
        .iter()
        .map(|name| value::instance_attr(instance, name).unwrap_or(NONE))
        .collect()
}

/// Whether `v` is an instance of a `collections.namedtuple` class.
///
/// The `_fields` class attribute is what distinguishes these classes from
/// `typing.NamedTuple` instances (which expose field values but no `_fields`).
pub(crate) fn is_namedtuple_instance(v: i64) -> bool {
    let Some(state) = value::as_instance(v) else {
        return false;
    };
    matches!(
        value::class_lookup(state.class, "_fields"),
        Some(value::ClassEntry::Attr(_))
    )
}

/// Values of a tuple or a namedtuple instance, for equality comparison.
fn sequence_values(v: i64) -> Option<Vec<i64>> {
    if is_namedtuple_instance(v) {
        return Some(field_values(v));
    }
    value::as_tuple(v).map(|items| items.to_vec())
}

/// Native method value with a fixed argument count.
fn native_method(entry: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
    value::function_new(entry as *const () as usize, arity, name.to_string())
}

/// The shared field-descriptor class: a `_tuplegetter`-shaped value whose
/// `__get__` returns the stored attribute.
///
/// Its instances are plain class instances so `_Instruction.opname.__doc__ =
/// ...` (a documented idiom in `dis`) can assign to the descriptor, exactly
/// like CPython's tuple getter objects.
fn field_descriptor_class() -> i64 {
    if let Some(class) = globals::registry_get("collections._tuplegetter") {
        return class;
    }
    let get = value::function_new(
        field_descriptor_get as *const () as usize,
        3,
        "__get__".to_string(),
    );
    let mark = heap::root_values(&[get]);
    let class = value::class_new(
        "_tuplegetter".to_string(),
        NONE,
        vec![("__get__".to_string(), get)],
    );
    heap::release_roots(mark);
    globals::registry_set("collections._tuplegetter", class);
    class
}

/// `descriptor.__get__(instance, owner)`: instance access reads the stored
/// field, class access returns the descriptor itself.
extern "C" fn field_descriptor_get(argv: *const i64) -> i64 {
    // SAFETY: the descriptor call ABI passes three readable slots.
    let descriptor = unsafe { *argv };
    let instance = unsafe { *argv.add(1) };
    if instance == NONE {
        return descriptor;
    }
    let Some(name) = value::instance_attr(descriptor, "_name").and_then(value::as_str) else {
        return NONE;
    };
    let name = name.to_string();
    methods::getattr(instance, name.as_ptr(), name.len())
}

/// `namedtuple(typename, field_names, *, rename=False, defaults=None,
/// module=None)`.
fn namedtuple(args: &[i64]) -> i64 {
    let mark = heap::root_values(&[]);
    let mut live: Vec<i64> = Vec::new();
    let result = build_namedtuple(args, &mut live);
    heap::release_roots(mark);
    result
}

fn build_namedtuple(args: &[i64], live: &mut Vec<i64>) -> i64 {
    let Some(typename_value) = args.first().copied() else {
        return type_error("namedtuple() missing required argument 'typename'");
    };
    let Some(typename) = value::as_str(typename_value) else {
        return type_error("namedtuple() argument 1 must be a string");
    };
    let typename = typename.to_string();
    if !is_identifier(&typename) {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!("Type names and field names must be valid identifiers: '{typename}'"),
        );
    }
    let Some(field_arg) = args.get(1).copied() else {
        return type_error("namedtuple() missing required argument 'field_names'");
    };
    let mut names: Vec<String> = if let Some(text) = value::as_str(field_arg) {
        text.replace(',', " ")
            .split_whitespace()
            .map(str::to_string)
            .collect()
    } else if let Some(items) = value::as_list(field_arg) {
        match string_list(items) {
            Ok(names) => names,
            Err(err) => return err,
        }
    } else if let Some(items) = value::as_tuple(field_arg) {
        match string_list(items) {
            Ok(names) => names,
            Err(err) => return err,
        }
    } else {
        return type_error(&format!(
            "'{}' object is not iterable",
            value::type_name(field_arg)
        ));
    };
    if names.is_empty() && value::as_str(field_arg).is_some_and(|text| !text.trim().is_empty()) {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!(
                "Type names and field names must be valid identifiers: '{}'",
                value::as_str(field_arg).unwrap_or("")
            ),
        );
    }

    let rename = args
        .get(2)
        .copied()
        .is_some_and(|arg| arg != NONE && value::truthy(arg));
    let mut seen: Vec<String> = Vec::with_capacity(names.len());
    for (index, name) in names.iter_mut().enumerate() {
        let valid = is_identifier(name) && !is_keyword(name) && !name.starts_with('_');
        if rename {
            if !valid || seen.iter().any(|existing| existing == name) {
                *name = format!("_{index}");
            }
            seen.push(name.clone());
            continue;
        }
        if !is_identifier(name) || is_keyword(name) {
            return exc::raise_with_message(
                VALUE_ERROR,
                &format!("Type names and field names must be valid identifiers: '{name}'"),
            );
        }
        if name.starts_with('_') {
            return exc::raise_with_message(
                VALUE_ERROR,
                &format!("Field names cannot start with an underscore: '{name}'"),
            );
        }
        if seen.iter().any(|existing| existing == name) {
            return exc::raise_with_message(
                VALUE_ERROR,
                &format!("Encountered duplicate field name: '{name}'"),
            );
        }
        seen.push(name.clone());
    }

    let mut default_values: Vec<i64> = Vec::new();
    let defaults_arg = args.get(3).copied().unwrap_or(NONE);
    if defaults_arg != NONE {
        match crate::core::builtins::star_sequence(defaults_arg) {
            Ok(values) => default_values = values,
            Err(err) => return err,
        }
    }
    if default_values.len() > names.len() {
        return type_error("Got more default values than field names");
    }

    // `_fields`: the ordered field-name tuple, shared with the descriptors and
    // `_asdict` so its string objects are the only name allocations.
    let mut field_items: Vec<i64> = Vec::with_capacity(names.len());
    for name in &names {
        field_items.push(keep(live, value::str_alloc(name.clone())));
    }
    let fields_tuple = keep(live, value::tuple_new(field_items.clone()));

    let descriptor_class = keep(live, field_descriptor_class());
    let mut attrs: Vec<(String, i64)> = Vec::with_capacity(names.len() + 4);
    attrs.push(("_fields".to_string(), fields_tuple));
    attrs.push(("__match_args__".to_string(), fields_tuple));
    for (index, (name, &name_value)) in names.iter().zip(field_items.iter()).enumerate() {
        let descriptor = keep(live, value::instance_new(descriptor_class));
        value::instance_setattr(descriptor, "_name".to_string(), name_value);
        let doc = keep(
            live,
            value::str_alloc(format!("Alias for field number {index}")),
        );
        value::instance_setattr(descriptor, "__doc__".to_string(), doc);
        attrs.push((name.clone(), descriptor));
    }
    let default_pairs: Vec<(i64, i64)> = field_items[names.len() - default_values.len()..]
        .iter()
        .copied()
        .zip(default_values.iter().copied())
        .collect();
    attrs.push((
        "_field_defaults".to_string(),
        keep(live, value::dict_new(default_pairs)),
    ));

    let mut signature: Vec<ParamMeta> = Vec::with_capacity(names.len() + 1);
    signature.push(ParamMeta {
        name: "self".to_string(),
        kind: ParamKind::Positional,
        default: None,
    });
    let default_start = names.len() - default_values.len();
    for (index, name) in names.iter().enumerate() {
        signature.push(ParamMeta {
            name: name.clone(),
            kind: ParamKind::Positional,
            default: (index >= default_start).then(|| default_values[index - default_start]),
        });
    }
    let init = keep(
        live,
        value::native_function_new("collections", "_namedtuple_init", usize::MAX),
    );
    value::function_set_signature(init, signature);
    value::function_set_full_entry(init, namedtuple_init as *const () as usize);

    let mut methods: Vec<(String, i64, MethodKind)> = vec![
        ("__init__".to_string(), init, MethodKind::Normal),
        (
            "__iter__".to_string(),
            keep(live, native_method(namedtuple_iter, 1, "__iter__")),
            MethodKind::Normal,
        ),
        (
            "__len__".to_string(),
            keep(live, native_method(namedtuple_len, 1, "__len__")),
            MethodKind::Normal,
        ),
        (
            "__getitem__".to_string(),
            keep(live, native_method(namedtuple_getitem, 2, "__getitem__")),
            MethodKind::Normal,
        ),
        (
            "__repr__".to_string(),
            keep(live, native_method(namedtuple_repr, 1, "__repr__")),
            MethodKind::Normal,
        ),
        (
            "__eq__".to_string(),
            keep(live, native_method(namedtuple_eq, 2, "__eq__")),
            MethodKind::Normal,
        ),
        (
            "_asdict".to_string(),
            keep(live, native_method(namedtuple_asdict, 1, "_asdict")),
            MethodKind::Normal,
        ),
    ];

    let make = keep(
        live,
        value::native_function_new("collections", "_namedtuple_make", usize::MAX),
    );
    value::function_set_signature(
        make,
        vec![
            ParamMeta {
                name: "cls".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
            ParamMeta {
                name: "iterable".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
        ],
    );
    value::function_set_full_entry(make, namedtuple_make as *const () as usize);
    methods.push(("_make".to_string(), make, MethodKind::Classmethod));

    let replace = keep(
        live,
        value::native_function_new("collections", "_namedtuple_replace", usize::MAX),
    );
    value::function_set_signature(
        replace,
        vec![
            ParamMeta {
                name: "self".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
            ParamMeta {
                name: "kwargs".to_string(),
                kind: ParamKind::KwArg,
                default: Some(NONE),
            },
        ],
    );
    value::function_set_full_entry(replace, namedtuple_replace as *const () as usize);
    methods.push(("_replace".to_string(), replace, MethodKind::Normal));

    let tuple_class = value::type_class(value::tuple_new(Vec::new()));
    value::class_new_full(typename, tuple_class, methods, attrs)
}

/// Field-name list from a list/tuple argument; every element must be a string.
fn string_list(items: &[i64]) -> Result<Vec<String>, i64> {
    let mut names = Vec::with_capacity(items.len());
    for &item in items {
        let Some(name) = value::as_str(item) else {
            return Err(type_error("Field names must be strings"));
        };
        names.push(name.to_string());
    }
    Ok(names)
}

/// `namedtuple.__init__`: stores each bound positional slot as a field
/// attribute. The binder has already filled defaults, so every slot exists.
extern "C" fn namedtuple_init(argv: *const i64) -> i64 {
    // SAFETY: the class-call binder passes one slot per declared parameter.
    let instance = unsafe { *argv };
    let Some(state) = value::as_instance(instance) else {
        return NONE;
    };
    let class = state.class;
    for (index, name) in field_names(class).into_iter().enumerate() {
        // SAFETY: the signature declares `self` plus one slot per field.
        let field = unsafe { *argv.add(index + 1) };
        value::instance_setattr(instance, name, field);
    }
    NONE
}

/// `x.__iter__()`: iterates the field values in declaration order.
extern "C" fn namedtuple_iter(argv: *const i64) -> i64 {
    // SAFETY: the method ABI passes the receiver in the first slot.
    let instance = unsafe { *argv };
    iter::iter_new(value::list_new(field_values(instance)))
}

/// `len(x)`: the number of fields.
extern "C" fn namedtuple_len(argv: *const i64) -> i64 {
    // SAFETY: the method ABI passes the receiver in the first slot.
    let instance = unsafe { *argv };
    value::encode_int(field_values(instance).len() as i64).unwrap_or(NONE)
}

/// `x[index]`: positional field access with Python's negative indices.
extern "C" fn namedtuple_getitem(argv: *const i64) -> i64 {
    // SAFETY: the method ABI passes the receiver and the index.
    let instance = unsafe { *argv };
    let index = unsafe { *argv.add(1) };
    let values = field_values(instance);
    let Some(raw) = value::as_integer(index) else {
        return type_error(&format!(
            "sequence indices must be integers or slices, not {}",
            value::type_name(index)
        ));
    };
    let len = values.len() as i64;
    let position = if raw < 0 { raw + len } else { raw };
    if position < 0 || position >= len {
        return exc::raise_with_message(INDEX_ERROR, "tuple index out of range");
    }
    values[position as usize]
}

/// `repr(x)`: `Name(field=value, ...)`, like a tuple subclass.
extern "C" fn namedtuple_repr(argv: *const i64) -> i64 {
    // SAFETY: the method ABI passes the receiver in the first slot.
    let instance = unsafe { *argv };
    let Some(class) = value::instance_class_value(instance) else {
        return NONE;
    };
    let name = value::class_display_name(class).unwrap_or("namedtuple");
    let names = field_names(class);
    let values = field_values(instance);
    let mut out = String::with_capacity(name.len() + 2 + names.len() * 12);
    out.push_str(name);
    out.push('(');
    for (index, (field, value)) in names.iter().zip(values.iter()).enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(field);
        out.push('=');
        out.push_str(&value::repr(*value));
    }
    out.push(')');
    value::str_alloc(out)
}

/// `x == y`: compares field values against another namedtuple of the same
/// shape or a plain tuple, like a tuple subclass.
extern "C" fn namedtuple_eq(argv: *const i64) -> i64 {
    // SAFETY: the method ABI passes the receiver and the other operand.
    let instance = unsafe { *argv };
    let other = unsafe { *argv.add(1) };
    let Some(left) = sequence_values(instance) else {
        return FALSE;
    };
    let Some(right) = sequence_values(other) else {
        return FALSE;
    };
    if left.len() != right.len() {
        return FALSE;
    }
    value::bool_value(
        left.iter()
            .zip(right.iter())
            .all(|(&a, &b)| container::equals(a, b)),
    )
}

/// `x._asdict()`: the field values as a `dict` in declaration order.
extern "C" fn namedtuple_asdict(argv: *const i64) -> i64 {
    // SAFETY: the method ABI passes the receiver in the first slot.
    let instance = unsafe { *argv };
    let Some(class) = value::instance_class_value(instance) else {
        return NONE;
    };
    let keys = field_name_values(class);
    let values = field_values(instance);
    value::dict_new(keys.into_iter().zip(values).collect())
}

/// `C._make(iterable)`: constructs an instance from an iterable of values.
extern "C" fn namedtuple_make(argv: *const i64) -> i64 {
    // SAFETY: the classmethod binder passes the class and the iterable.
    let class = unsafe { *argv };
    let iterable = unsafe { *argv.add(1) };
    let items = match crate::core::builtins::star_sequence(iterable) {
        Ok(items) => items,
        Err(err) => return err,
    };
    let expected = field_names(class).len();
    if items.len() != expected {
        return type_error(&format!(
            "Expected {expected} arguments, got {}",
            items.len()
        ));
    }
    let roots = heap::root_values(&items);
    let result = crate::core::builtins::call(class, items.len(), items.as_ptr());
    heap::release_roots(roots);
    result
}

/// `x._replace(**changes)`: a copy with the named fields replaced.
extern "C" fn namedtuple_replace(argv: *const i64) -> i64 {
    // SAFETY: the keyword binder passes the receiver and the `**kwargs` dict.
    let instance = unsafe { *argv };
    let changes = unsafe { *argv.add(1) };
    let Some(class) = value::instance_class_value(instance) else {
        return NONE;
    };
    let names = field_names(class);
    let mut values = field_values(instance);
    let Some(pairs) = value::as_dict(changes) else {
        return NONE;
    };
    let mut unexpected: Vec<String> = Vec::new();
    for &(key, item) in pairs {
        let Some(name) = value::as_str(key) else {
            return type_error("Field names must be strings");
        };
        match names.iter().position(|field| field == name) {
            Some(index) => values[index] = item,
            None => unexpected.push(name.to_string()),
        }
    }
    if !unexpected.is_empty() {
        let rendered: Vec<String> = unexpected.iter().map(|name| format!("'{name}'")).collect();
        return type_error(&format!(
            "Got unexpected field names: [{}]",
            rendered.join(", ")
        ));
    }
    let roots = heap::root_values(&values);
    let result = crate::core::builtins::call(class, values.len(), values.as_ptr());
    heap::release_roots(roots);
    result
}

#[cfg(test)]
mod ordereddict_tests {
    use super::*;

    fn int(n: i64) -> i64 {
        value::encode_int(n).expect("representable")
    }

    fn text(s: &str) -> i64 {
        value::str_alloc(s.to_string())
    }

    fn number(v: i64) -> i64 {
        value::as_integer(v).expect("integer")
    }

    fn string(v: i64) -> String {
        value::as_str(v).expect("string").to_string()
    }

    /// `OrderedDict([(key, value), ...])`.
    fn ordered(items: &[(&str, i64)]) -> i64 {
        let pairs: Vec<i64> = items
            .iter()
            .map(|(key, value)| value::tuple_new(vec![text(key), int(*value)]))
            .collect();
        call("OrderedDict", &[value::list_new(pairs)])
    }

    fn method(instance: i64, name: &str, args: &[i64]) -> i64 {
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(instance);
        argv.extend_from_slice(args);
        let function = format!("OrderedDict.{name}");
        call(&function, &argv)
    }

    fn keys(instance: i64) -> Vec<String> {
        let result = method(instance, "keys", &[]);
        value::as_list(result)
            .expect("keys list")
            .iter()
            .map(|&key| string(key))
            .collect()
    }

    fn pairs(instance: i64) -> Vec<(String, i64)> {
        let result = method(instance, "items", &[]);
        value::as_list(result)
            .expect("items list")
            .iter()
            .map(|&pair| {
                let tuple = value::as_tuple(pair).expect("pair tuple");
                (string(tuple[0]), number(tuple[1]))
            })
            .collect()
    }

    #[test]
    fn constructor_preserves_insertion_order() {
        exc::init();
        let od = ordered(&[("b", 2), ("a", 1), ("c", 3)]);
        assert!(!exc::pending());
        assert_eq!(keys(od), vec!["b", "a", "c"]);
        assert_eq!(
            pairs(od),
            vec![
                ("b".to_string(), 2),
                ("a".to_string(), 1),
                ("c".to_string(), 3)
            ]
        );
        assert_eq!(number(method(od, "__len__", &[])), 3);
        assert_eq!(number(method(od, "__getitem__", &[text("a")])), 1);
        assert_eq!(method(od, "__contains__", &[text("c")]), value::TRUE);
        assert_eq!(
            method(od, "__contains__", &[text("z")]),
            value::bool_value(false)
        );
    }

    #[test]
    fn move_to_end_and_popitem_follow_insertion_order() {
        exc::init();
        let od = ordered(&[("a", 1), ("b", 2), ("c", 3)]);
        method(od, "move_to_end", &[text("a")]);
        assert_eq!(keys(od), vec!["b", "c", "a"]);
        method(od, "move_to_end", &[text("a"), value::bool_value(false)]);
        assert_eq!(keys(od), vec!["a", "b", "c"]);

        let last = method(od, "popitem", &[]);
        let last = value::as_tuple(last).expect("popitem tuple");
        assert_eq!(string(last[0]), "c");
        assert_eq!(number(last[1]), 3);
        assert_eq!(keys(od), vec!["a", "b"]);

        let first = method(od, "popitem", &[value::bool_value(false)]);
        let first = value::as_tuple(first).expect("popitem tuple");
        assert_eq!(string(first[0]), "a");
        assert_eq!(keys(od), vec!["b"]);
    }

    #[test]
    fn update_setdefault_copy_clear_and_equality() {
        exc::init();
        let od = ordered(&[("a", 1), ("b", 2)]);
        method(od, "update", &[ordered(&[("b", 20), ("c", 3)])]);
        assert_eq!(
            pairs(od),
            vec![
                ("a".to_string(), 1),
                ("b".to_string(), 20),
                ("c".to_string(), 3)
            ]
        );
        assert_eq!(number(method(od, "setdefault", &[text("d"), int(4)])), 4);
        assert_eq!(number(method(od, "setdefault", &[text("a"), int(99)])), 1);
        assert_eq!(keys(od), vec!["a", "b", "c", "d"]);

        let copied = method(od, "copy", &[]);
        assert_eq!(pairs(copied), pairs(od));
        // OrderedDict equality is order-sensitive between two OrderedDicts.
        assert_eq!(method(od, "__eq__", &[copied]), value::TRUE);
        let reversed = ordered(&[("d", 4), ("c", 3), ("b", 20), ("a", 1)]);
        assert_eq!(method(od, "__eq__", &[reversed]), value::bool_value(false));
        // ... but order-insensitive against a plain dict with the same items.
        let plain = value::dict_new(vec![
            (text("d"), int(4)),
            (text("c"), int(3)),
            (text("b"), int(20)),
            (text("a"), int(1)),
        ]);
        assert_eq!(method(od, "__eq__", &[plain]), value::TRUE);

        method(od, "clear", &[]);
        assert!(keys(od).is_empty());

        let seeded = call(
            "OrderedDict.fromkeys",
            &[
                ordereddict_class(),
                value::list_new(vec![text("x"), text("y")]),
                int(7),
            ],
        );
        assert_eq!(
            pairs(seeded),
            vec![("x".to_string(), 7), ("y".to_string(), 7)]
        );
    }
}

#[cfg(test)]
mod counter_tests {
    use super::*;

    fn int(n: i64) -> i64 {
        value::encode_int(n).expect("representable")
    }

    fn text(s: &str) -> i64 {
        value::str_alloc(s.to_string())
    }

    fn number(v: i64) -> i64 {
        value::as_integer(v).expect("integer")
    }

    fn string(v: i64) -> String {
        value::as_str(v).expect("string").to_string()
    }

    fn words(items: &[&str]) -> i64 {
        value::list_new(items.iter().map(|item| text(item)).collect())
    }

    fn counter(items: &[&str]) -> i64 {
        call("Counter", &[words(items)])
    }

    fn method(instance: i64, name: &str, args: &[i64]) -> i64 {
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(instance);
        argv.extend_from_slice(args);
        call(&format!("Counter.{name}"), &argv)
    }

    fn pairs(instance: i64) -> Vec<(String, i64)> {
        let result = method(instance, "items", &[]);
        value::as_list(result)
            .expect("items list")
            .iter()
            .map(|&pair| {
                let tuple = value::as_tuple(pair).expect("pair tuple");
                (string(tuple[0]), number(tuple[1]))
            })
            .collect()
    }

    fn drain(iterator: i64) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            let mut item = NONE;
            if iter::iter_next(iterator, &mut item) == 0 {
                break;
            }
            assert!(!exc::pending());
            out.push(string(item));
        }
        out
    }

    #[test]
    fn constructor_counts_iterables_and_preserves_order() {
        exc::init();
        let c = counter(&["b", "a", "b", "c", "b"]);
        assert!(!exc::pending());
        assert_eq!(
            pairs(c),
            vec![
                ("b".to_string(), 3),
                ("a".to_string(), 1),
                ("c".to_string(), 1)
            ]
        );
        assert_eq!(number(method(c, "__len__", &[])), 3);
        assert_eq!(number(method(c, "__getitem__", &[text("b")])), 3);
        // A missing key reads as zero and is not inserted.
        assert_eq!(number(method(c, "__getitem__", &[text("z")])), 0);
        assert_eq!(pairs(c).len(), 3);
        assert_eq!(method(c, "__contains__", &[text("a")]), value::TRUE);
        assert_eq!(
            method(c, "__contains__", &[text("z")]),
            value::bool_value(false)
        );
        assert_eq!(method(c, "__missing__", &[text("z")]), int(0));
        assert_eq!(drain(method(c, "__iter__", &[])), vec!["b", "a", "c"]);
    }

    #[test]
    fn mapping_constructor_and_keywords_add_counts() {
        exc::init();
        let seed = value::dict_new(vec![(text("a"), int(2)), (text("b"), int(-1))]);
        let c = counter_new(&[seed], &[]);
        assert_eq!(pairs(c), vec![("a".to_string(), 2), ("b".to_string(), -1)]);

        let keywords = [("a".to_string(), int(1)), ("c".to_string(), int(4))];
        let combined = counter_new(&[words(&["a", "a"])], &keywords);
        assert!(!exc::pending());
        assert_eq!(
            pairs(combined),
            vec![("a".to_string(), 3), ("c".to_string(), 4)]
        );
    }

    #[test]
    fn item_assignment_update_subtract_and_total() {
        exc::init();
        let c = counter(&["a", "a", "b"]);
        method(c, "__setitem__", &[text("d"), int(5)]);
        assert_eq!(
            pairs(c),
            vec![
                ("a".to_string(), 2),
                ("b".to_string(), 1),
                ("d".to_string(), 5)
            ]
        );
        method(c, "__delitem__", &[text("d")]);
        assert_eq!(pairs(c).len(), 2);
        method(c, "__delitem__", &[text("missing")]);
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), KEY_ERROR as i64);
        exc::clear();

        method(c, "update", &[words(&["a", "c"])]);
        assert_eq!(
            pairs(c),
            vec![
                ("a".to_string(), 3),
                ("b".to_string(), 1),
                ("c".to_string(), 1)
            ]
        );
        method(c, "subtract", &[words(&["b", "b"])]);
        assert_eq!(
            pairs(c),
            vec![
                ("a".to_string(), 3),
                ("b".to_string(), -1),
                ("c".to_string(), 1)
            ]
        );
        assert_eq!(number(method(c, "total", &[])), 3);

        let empty = counter(&[]);
        assert_eq!(number(method(empty, "total", &[])), 0);
    }

    #[test]
    fn elements_and_most_common_follow_counts_and_insertion_order() {
        exc::init();
        let c = counter(&["b", "a", "b", "c", "b"]);
        assert_eq!(
            drain(method(c, "elements", &[])),
            vec!["b", "b", "b", "a", "c"]
        );

        let all = method(c, "most_common", &[]);
        let rendered: Vec<(String, i64)> = value::as_list(all)
            .expect("most_common list")
            .iter()
            .map(|&pair| {
                let tuple = value::as_tuple(pair).expect("pair tuple");
                (string(tuple[0]), number(tuple[1]))
            })
            .collect();
        assert_eq!(
            rendered,
            vec![
                ("b".to_string(), 3),
                ("a".to_string(), 1),
                ("c".to_string(), 1)
            ]
        );

        let top = method(c, "most_common", &[int(1)]);
        let top = value::as_list(top).expect("most_common list");
        assert_eq!(top.len(), 1);
        let pair = value::as_tuple(top[0]).expect("pair tuple");
        assert_eq!(string(pair[0]), "b");

        let zero = method(c, "most_common", &[int(0)]);
        assert!(value::as_list(zero).expect("list").is_empty());
        let negative = method(c, "most_common", &[int(-3)]);
        assert!(value::as_list(negative).expect("list").is_empty());
    }

    #[test]
    fn addition_and_subtraction_keep_positive_counts() {
        exc::init();
        let left = counter(&["a", "a", "b"]);
        let right = counter(&["a", "c"]);

        let sum = method(left, "__add__", &[right]);
        assert_eq!(
            pairs(sum),
            vec![
                ("a".to_string(), 3),
                ("b".to_string(), 1),
                ("c".to_string(), 1)
            ]
        );

        let difference = method(left, "__sub__", &[right]);
        assert_eq!(
            pairs(difference),
            vec![("a".to_string(), 1), ("b".to_string(), 1)]
        );

        let cancelled = method(left, "__sub__", &[counter(&["a", "a"])]);
        assert_eq!(pairs(cancelled), vec![("b".to_string(), 1)]);

        // Negative counts on the right become positive counts.
        let negative = counter_new(&[value::dict_new(vec![(text("z"), int(-2))])], &[]);
        let combined = method(counter(&["y"]), "__sub__", &[negative]);
        assert_eq!(
            pairs(combined),
            vec![("y".to_string(), 1), ("z".to_string(), 2)]
        );

        // A non-counter operand reports `NotImplemented`.
        assert_eq!(
            method(left, "__add__", &[value::list_new(Vec::new())]),
            value::NOT_IMPLEMENTED
        );
    }

    #[test]
    fn equality_uses_multiset_semantics_and_dict_fallback() {
        exc::init();
        let c = counter(&["a", "a", "b"]);
        assert_eq!(
            method(c, "__eq__", &[counter(&["b", "a", "a"])]),
            value::TRUE
        );
        assert_eq!(
            method(c, "__eq__", &[counter(&["a", "b"])]),
            value::bool_value(false)
        );
        // A zero count equals a missing key against another counter...
        let zeroed = counter_new(&[value::dict_new(vec![(text("a"), int(0))])], &[]);
        assert_eq!(method(zeroed, "__eq__", &[counter(&[])]), value::TRUE);
        // ... but not against a plain dict, which compares item for item.
        assert_eq!(
            method(zeroed, "__eq__", &[value::dict_new(Vec::new())]),
            value::bool_value(false)
        );
        assert_eq!(
            method(
                c,
                "__eq__",
                &[value::dict_new(vec![
                    (text("a"), int(2)),
                    (text("b"), int(1))
                ])]
            ),
            value::TRUE
        );
        assert_eq!(
            method(c, "__ne__", &[counter(&["a", "a", "b"])]),
            value::bool_value(false)
        );
        assert_eq!(method(c, "__eq__", &[int(1)]), value::NOT_IMPLEMENTED);
    }

    #[test]
    fn copy_repr_and_dict_membership() {
        exc::init();
        let c = counter(&["a", "a", "b"]);
        assert_eq!(
            value::repr(c),
            "Counter({'a': 2, 'b': 1})",
            "repr uses the counter representation"
        );
        assert_eq!(value::to_string(c), "Counter({'a': 2, 'b': 1})");
        assert_eq!(
            value::repr(counter(&[])),
            "Counter()",
            "empty counters render like dicts"
        );

        let copied = method(c, "copy", &[]);
        assert_eq!(pairs(copied), pairs(c));
        method(copied, "__setitem__", &[text("c"), int(9)]);
        assert_eq!(pairs(c).len(), 2, "copy must not share counts");

        method(copied, "clear", &[]);
        assert!(pairs(copied).is_empty());

        let key = text("a");
        assert_eq!(number(method(c, "get", &[key])), 2);
        assert_eq!(method(c, "get", &[text("z")]), NONE);
        assert_eq!(method(c, "get", &[text("z"), int(7)]), int(7));
        assert_eq!(method(c, "setdefault", &[text("z"), int(4)]), int(4));
        assert_eq!(number(method(c, "get", &[text("z")])), 4);
        let popped = method(c, "pop", &[text("z")]);
        assert_eq!(number(popped), 4);
        let popped_default = method(c, "pop", &[text("z"), int(0)]);
        assert_eq!(number(popped_default), 0);

        // `Counter` is a `dict` subclass.
        let dict_function = value::builtin_function_new(hydro_ir::Builtin::Dict);
        assert_eq!(
            crate::core::builtins::isinstance(c, dict_function),
            value::TRUE
        );
        assert!(value::is_subclass_of(
            counter_class(),
            value::type_class(value::dict_new(Vec::new()))
        ));
        assert_eq!(value::type_name_of(c), "Counter");
    }
}
