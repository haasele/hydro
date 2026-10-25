//! `collections.defaultdict`.
//!
//! The container keeps insertion-ordered `(key, value)` pairs like `dict`,
//! plus a `default_factory` value. Missing-key subscripting invokes the
//! factory once, inserts the result and returns it. A `None` factory raises
//! `KeyError`, matching CPython.

use crate::container;
use crate::exc::{self, ATTRIBUTE_ERROR, KEY_ERROR, TYPE_ERROR};
use crate::value::{self, FunctionBody, NONE};

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

pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "defaultdict" => {
            let factory = args.first().copied().unwrap_or(NONE);
            if factory != NONE && value::as_function(factory).is_none() {
                return type_error(&format!(
                    "defaultdict() first argument must be callable or None, not {}",
                    value::type_name(factory)
                ));
            }
            value::defaultdict_new(factory, Vec::new())
        }
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'collections' has no attribute '{function}'"),
        ),
    }
}

/// Produces the value stored for a missing key. The caller checks that the
/// factory is not `None`.
fn factory_value(factory: i64) -> i64 {
    let Some(function) = value::as_function(factory) else {
        return type_error("default_factory is not callable");
    };
    if let FunctionBody::Builtin(builtin) = &function.body {
        match builtin {
            pyx_ir::Builtin::Int => return value::encode_int(0).unwrap_or(NONE),
            pyx_ir::Builtin::Float => return value::float_new(0.0),
            pyx_ir::Builtin::Str => return value::str_alloc(String::new()),
            pyx_ir::Builtin::List => return value::list_new(Vec::new()),
            pyx_ir::Builtin::Dict => return value::dict_new(Vec::new()),
            pyx_ir::Builtin::Set => return value::set_new(Vec::new()),
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
