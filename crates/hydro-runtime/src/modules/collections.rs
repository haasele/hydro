//! `collections.defaultdict`.
//!
//! The container keeps insertion-ordered `(key, value)` pairs like `dict`,
//! plus a `default_factory` value. Missing-key subscripting invokes the
//! factory once, inserts the result and returns it. A `None` factory raises
//! `KeyError`, matching CPython.

use crate::container;
use crate::core::globals;
use crate::core::iter;
use crate::core::methods;
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
        "namedtuple" => namedtuple(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'collections' has no attribute '{function}'"),
        ),
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
