//! Native `inspect` over Hydro's runtime representations.
//!
//! CPython implements `inspect` in Python on top of frame, code and traceback
//! objects. Hydro has none of those, but it does keep real metadata for
//! function values (parameter names, kinds and defaults), class namespaces and
//! method bindings, and native module specs. This module exposes CPython's
//! introspection API over that metadata:
//!
//! * predicates (`isfunction`, `ismethod`, `isbuiltin`, `isclass`, ...);
//! * docstring helpers (`cleandoc`, `getdoc`);
//! * `Signature`/`Parameter`/`BoundArguments` objects with real argument
//!   binding, formatting and `functools.partial`/`partialmethod` support;
//! * `getfullargspec`, `getcallargs`, `getmembers`, `getattr_static` and
//!   `unwrap`.
//!
//! ## Precise unsupported areas (reported, not fabricated)
//!
//! * **Code, frame and traceback objects do not exist.** `isframe` and
//!   `iscode` are therefore always `False`; `currentframe()` returns `None`
//!   because no frame object can be represented, and `stack()` returns an
//!   empty list because there are no representable frame contexts.
//!   `getclosurevars` raises `NotImplementedError`: free/global/builtin name
//!   references live in a code object the runtime does not reify.
//! * **Annotations are not stored on function values.** Every
//!   `Parameter.annotation` (and `FullArgSpec.annotations`) is
//!   `Parameter.empty`; `signature`'s `globals`/`locals`/`eval_str`/
//!   `annotation_format` parameters are accepted but have nothing to evaluate.
//! * **Function and class values carry no `__module__`, `__code__` or
//!   `__qualname__` provenance.** `getmodule` answers for modules, native
//!   module functions and builtins (the `builtins` module) and returns `None`
//!   for user functions/classes instead of guessing. `getfile` answers for
//!   modules through their `__file__` global and raises a precise `TypeError`
//!   for function/class values.
//! * **`mappingproxy` is not representable.** `Signature.parameters` returns a
//!   fresh `dict`: insertion order is preserved, but mutating it does not
//!   mutate the signature.
//! * **Native signatures carry no parameter names beyond the central spec.**
//!   `stdlib::FunctionSpec` records keyword names and arity. Named slots keep
//!   their names; unnamed slots are reported as positional-only `arg1`,
//!   `arg2`, ...; a native optional parameter whose concrete default is not in
//!   the spec reports `None` as its default (the native implementation still
//!   applies the real default). Builtins use a curated table with CPython's
//!   real signatures; builtins whose signature CPython does not expose raise
//!   `ValueError` with CPython's message.
//! * **`getattr_static` returns the raw function for methods and properties.**
//!   Hydro stores a class entry's binding kind plus the underlying function,
//!   not a reified descriptor object, so the function value is the closest
//!   uninvoked representation.

use std::cell::Cell;

use hydro_ir::{MethodKind, ParamKind};

use crate::container;
use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, INDEX_ERROR, NOT_IMPLEMENTED_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::iter;
use crate::modules::importlib_machinery;
use crate::value::{self, ClassEntry, DescriptorKind, FunctionBody, ParamMeta, FALSE, NONE, TRUE};

// -------------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------------

/// Out-of-stack roots for values that only live in Rust-side temporaries
/// while further Hydro allocations may collect the heap.
#[derive(Default)]
struct Roots(Vec<usize>);

impl Roots {
    fn new() -> Roots {
        Roots(Vec::new())
    }

    fn keep(&mut self, values: &[i64]) {
        self.0.push(heap::root_values(values));
    }
}

impl Drop for Roots {
    fn drop(&mut self) {
        for &start in self.0.iter().rev() {
            heap::release_roots(start);
        }
    }
}

fn text(content: &str) -> i64 {
    value::str_alloc(content.to_string())
}

fn integer(number: i64) -> i64 {
    value::encode_int(number).unwrap_or(NONE)
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn attribute_error(obj: i64, name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!(
            "'{}' object has no attribute '{}'",
            value::type_name_of(obj),
            name
        ),
    )
}

/// Clears a pending `AttributeError` raised while probing an optional
/// attribute; any other exception stays pending.
fn take_attribute_error() -> bool {
    if !exc::pending() {
        return false;
    }
    if exc::pending_tag() != i64::from(ATTRIBUTE_ERROR) {
        return false;
    }
    let _ = exc::clear();
    true
}

fn is_instance_of(value_in: i64, class: i64) -> bool {
    value::instance_class_value(value_in).is_some_and(|owner| value::is_subclass_of(owner, class))
}

fn function_body(obj: i64) -> Option<&'static FunctionBody> {
    value::as_function(obj).map(|function| &function.body)
}

fn function_name(obj: i64) -> Option<String> {
    value::as_function(obj).map(|function| function.name.clone())
}

fn parameter_field(parameter: i64, name: &str) -> i64 {
    value::instance_attr(parameter, name).unwrap_or(NONE)
}

fn parameter_text(parameter: i64, name: &str) -> String {
    value::as_str(parameter_field(parameter, name))
        .map(str::to_string)
        .unwrap_or_default()
}

/// Python keywords, for `Parameter` name validation.
const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

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
    KEYWORDS.contains(&name)
}

fn class_bases(class: i64) -> Vec<i64> {
    value::as_class(class)
        .map(|state| state.bases.clone())
        .unwrap_or_default()
}

// -------------------------------------------------------------------------
// No-argument/basic signatures: dicts, parameter values, kinds
// -------------------------------------------------------------------------

const KIND_DESCRIPTIONS: [(&str, &str); 5] = [
    ("POSITIONAL_ONLY", "positional-only"),
    ("POSITIONAL_OR_KEYWORD", "positional or keyword"),
    ("VAR_POSITIONAL", "variadic positional"),
    ("KEYWORD_ONLY", "keyword-only"),
    ("VAR_KEYWORD", "variadic keyword"),
];

const POSITIONAL_ONLY: i64 = 0;
const POSITIONAL_OR_KEYWORD: i64 = 1;
const VAR_POSITIONAL: i64 = 2;
const KEYWORD_ONLY: i64 = 3;
const VAR_KEYWORD: i64 = 4;

/// Runtime cache key used to invalidate every cached `inspect` class when the
/// heap is reinitialised (the values would otherwise dangle).
const CACHE_GENERATION_KEY: &str = "inspect.cache_generation";

fn cache_generation() -> i64 {
    integer(i64::try_from(heap::generation()).unwrap_or(i64::MAX))
}

/// Reads an `inspect` registry entry created for the current heap generation.
fn cached(key: &str) -> Option<i64> {
    if globals::registry_get(CACHE_GENERATION_KEY) != Some(cache_generation()) {
        return None;
    }
    globals::registry_get(key)
}

/// Stores an `inspect` registry entry and stamps the current heap generation.
fn cache(key: &str, item: i64) {
    globals::registry_set(CACHE_GENERATION_KEY, cache_generation());
    globals::registry_set(key, item);
}

thread_local! {
    /// Set while `parameter_kind_class` is populating its `inspect.kind.*`
    /// cache entries. Lazy `kind_member` initialization must not re-enter
    /// class construction while the members are still being created.
    static KIND_MEMBERS_INITIALIZING: Cell<bool> = const { Cell::new(false) };
}

/// Clears `KIND_MEMBERS_INITIALIZING` when construction finishes, including
/// on unwind.
struct KindMemberInitGuard;

impl KindMemberInitGuard {
    fn enter() -> KindMemberInitGuard {
        KIND_MEMBERS_INITIALIZING.with(|initializing| initializing.set(true));
        KindMemberInitGuard
    }
}

impl Drop for KindMemberInitGuard {
    fn drop(&mut self) {
        KIND_MEMBERS_INITIALIZING.with(|initializing| initializing.set(false));
    }
}

fn dict_pairs(dict: i64) -> Vec<(i64, i64)> {
    value::as_dict(dict)
        .map(|pairs| pairs.to_vec())
        .unwrap_or_default()
}

fn dict_copy(dict: i64) -> i64 {
    let result = value::dict_new(Vec::new());
    let mut guard = Roots::new();
    guard.keep(&[result]);
    for (key, item) in dict_pairs(dict) {
        container::dict_set(result, key, item);
    }
    result
}

fn dict_lookup_name(dict: i64, name: &str) -> Option<i64> {
    dict_pairs(dict)
        .into_iter()
        .find(|(key, _)| value::as_str(*key) == Some(name))
        .map(|(_, item)| item)
}

fn dict_insert_name(dict: i64, name: &str, item: i64) {
    let key = text(name);
    let mut guard = Roots::new();
    guard.keep(&[key, item]);
    container::dict_set(dict, key, item);
}

fn dict_insert_key(dict: i64, key: i64, item: i64) {
    let mut guard = Roots::new();
    guard.keep(&[key, item]);
    container::dict_set(dict, key, item);
}

pub(crate) fn class_value(module: &str, name: &str) -> Option<i64> {
    if module != "inspect" {
        return None;
    }
    Some(match name {
        "Signature" => signature_class(),
        "Parameter" => parameter_class(),
        "BoundArguments" => bound_arguments_class(),
        "FullArgSpec" => full_arg_spec_class(),
        "FrameInfo" => frame_info_class(),
        "ClosureVars" => closure_vars_class(),
        "ParameterKind" | "_ParameterKind" => parameter_kind_class(),
        _ => return None,
    })
}

/// `inspect.empty` / `Parameter.empty`: CPython's `_empty` marker class.
fn empty_class() -> i64 {
    if let Some(class) = cached("inspect._empty.class") {
        return class;
    }
    let class = value::class_new("_empty".to_string(), NONE, Vec::new());
    cache("inspect._empty.class", class);
    class
}

fn is_empty(value_in: i64) -> bool {
    value_in == empty_class()
}

fn kind_member(index: i64) -> Option<i64> {
    // Out-of-range indices never map to a kind, so they must not materialize
    // the class either.
    let in_range = usize::try_from(index)
        .ok()
        .is_some_and(|index| index < KIND_DESCRIPTIONS.len());
    if !in_range {
        return None;
    }
    let key = format!("inspect.kind.{index}");
    if cached(&key).is_none() && !KIND_MEMBERS_INITIALIZING.with(Cell::get) {
        parameter_kind_class();
    }
    cached(&key)
}

fn kind_index_of(value_in: i64) -> Option<i64> {
    if !is_instance_of(value_in, parameter_kind_class()) {
        return None;
    }
    value::instance_attr(value_in, "value").and_then(value::as_integer)
}

/// Resolves a user-supplied kind to its canonical index in `0..=4`.
///
/// Both the integer representation and a `_ParameterKind` member are accepted
/// (matching CPython's `kind in _ParameterKind` check). The member is only
/// materialized to validate the index; callers that need the stored value must
/// resolve it through [`kind_member`], which returns the kind instance.
fn normalize_kind(value_in: i64) -> Option<i64> {
    let index = match value::as_integer(value_in) {
        Some(index) => index,
        None => kind_index_of(value_in)?,
    };
    kind_member(index)?;
    Some(index)
}

fn kind_name(index: i64) -> &'static str {
    usize::try_from(index)
        .ok()
        .and_then(|index| KIND_DESCRIPTIONS.get(index))
        .map(|(name, _)| *name)
        .unwrap_or("POSITIONAL_ONLY")
}

fn kind_description(index: i64) -> &'static str {
    usize::try_from(index)
        .ok()
        .and_then(|index| KIND_DESCRIPTIONS.get(index))
        .map(|(_, description)| *description)
        .unwrap_or("positional-only")
}

fn kind_index_for(kind: ParamKind) -> i64 {
    match kind {
        ParamKind::PosOnly => POSITIONAL_ONLY,
        ParamKind::Positional => POSITIONAL_OR_KEYWORD,
        ParamKind::VarArg => VAR_POSITIONAL,
        ParamKind::KwOnly => KEYWORD_ONLY,
        ParamKind::KwArg => VAR_KEYWORD,
    }
}

// -------------------------------------------------------------------------
// `ParameterKind`
// -------------------------------------------------------------------------

fn parameter_kind_class() -> i64 {
    if let Some(class) = cached("inspect.ParameterKind.class") {
        return class;
    }
    // The class is cached before its members exist, so a re-entrant
    // `kind_member` must observe the guard instead of starting a second build.
    let _initializing = KindMemberInitGuard::enter();
    let mut roots = Roots::new();
    let methods: Vec<(String, i64, MethodKind)> = vec![
        (
            "__str__".to_string(),
            method_function(
                "_ParameterKind.__str__",
                vec![param("self", ParamKind::Positional, None)],
                kind_str_entry,
            ),
            MethodKind::Normal,
        ),
        (
            "__repr__".to_string(),
            method_function(
                "_ParameterKind.__repr__",
                vec![param("self", ParamKind::Positional, None)],
                kind_repr_entry,
            ),
            MethodKind::Normal,
        ),
        (
            "__eq__".to_string(),
            method_function(
                "_ParameterKind.__eq__",
                vec![
                    param("self", ParamKind::Positional, None),
                    param("other", ParamKind::Positional, None),
                ],
                kind_eq_entry,
            ),
            MethodKind::Normal,
        ),
        (
            "__hash__".to_string(),
            method_function(
                "_ParameterKind.__hash__",
                vec![param("self", ParamKind::Positional, None)],
                kind_hash_entry,
            ),
            MethodKind::Normal,
        ),
    ];
    let method_values: Vec<i64> = methods.iter().map(|(_, function, _)| *function).collect();
    roots.keep(&method_values);
    let class = value::class_new_full("_ParameterKind".to_string(), NONE, methods, Vec::new());
    cache("inspect.ParameterKind.class", class);
    for (index, (name, description)) in KIND_DESCRIPTIONS.iter().enumerate() {
        let index = index as i64;
        let name_value = text(name);
        let description_value = text(description);
        roots.keep(&[name_value, description_value]);
        let member = value::instance_new(class);
        roots.keep(&[member]);
        value::instance_setattr(member, "name".to_string(), name_value);
        value::instance_setattr(member, "value".to_string(), integer(index));
        value::instance_setattr(member, "_value_".to_string(), integer(index));
        value::instance_setattr(member, "description".to_string(), description_value);
        value::class_set_attr(class, name.to_string(), member);
        cache(&format!("inspect.kind.{index}"), member);
    }
    class
}

fn kind_index_of_instance(self_value: i64) -> i64 {
    kind_index_of(self_value).unwrap_or(POSITIONAL_OR_KEYWORD)
}

fn kind_str_raw(self_value: i64) -> i64 {
    text(kind_name(kind_index_of_instance(self_value)))
}

fn kind_repr_raw(self_value: i64) -> i64 {
    let index = kind_index_of_instance(self_value);
    text(&format!("<_ParameterKind.{}: {}>", kind_name(index), index))
}

fn kind_eq_raw(self_value: i64, other: i64) -> i64 {
    if self_value == other {
        return TRUE;
    }
    let index = kind_index_of_instance(self_value);
    if let Some(other_index) = kind_index_of(other) {
        return value::bool_value(other_index == index);
    }
    value::bool_value(value::as_integer(other).is_some_and(|other| other == index))
}

fn kind_hash_raw(self_value: i64) -> i64 {
    integer(kind_index_of_instance(self_value))
}

extern "C" fn kind_str_entry(argv: *const i64) -> i64 {
    // SAFETY: the class-call binder passes one slot per declared parameter.
    kind_str_raw(unsafe { *argv })
}

extern "C" fn kind_repr_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    kind_repr_raw(unsafe { *argv })
}

extern "C" fn kind_eq_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameters.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    kind_eq_raw(args[0], args[1])
}

extern "C" fn kind_hash_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    kind_hash_raw(unsafe { *argv })
}

// -------------------------------------------------------------------------
// `Parameter`
// -------------------------------------------------------------------------

/// One class method definition: `(attribute name, qualified name, signature,
/// native entry point, kind)`.
type MethodDefinition = (
    &'static str,
    &'static str,
    Vec<ParamMeta>,
    extern "C" fn(*const i64) -> i64,
    MethodKind,
);

fn param(name: &str, kind: ParamKind, default: Option<i64>) -> ParamMeta {
    ParamMeta {
        name: name.to_string(),
        kind,
        default,
    }
}

fn method_function(
    name: &'static str,
    signature: Vec<ParamMeta>,
    entry: extern "C" fn(*const i64) -> i64,
) -> i64 {
    let arity = signature.len();
    let function = value::native_function_new("inspect", name, arity);
    value::function_set_signature(function, signature);
    value::function_set_full_entry(function, entry as *const () as usize);
    function
}

fn parameter_class() -> i64 {
    if let Some(class) = cached("inspect.Parameter.class") {
        return class;
    }
    let empty = empty_class();
    let mut roots = Roots::new();
    roots.keep(&[empty]);
    let mut methods: Vec<(String, i64, MethodKind)> = Vec::new();
    let definitions: [MethodDefinition; 10] = [
        (
            "__init__",
            "Parameter.__init__",
            vec![
                param("self", ParamKind::Positional, None),
                param("name", ParamKind::Positional, None),
                param("kind", ParamKind::Positional, None),
                param("default", ParamKind::KwOnly, Some(empty)),
                param("annotation", ParamKind::KwOnly, Some(empty)),
            ],
            parameter_init_entry,
            MethodKind::Normal,
        ),
        (
            "replace",
            "Parameter.replace",
            vec![
                param("self", ParamKind::Positional, None),
                param("name", ParamKind::KwOnly, Some(empty)),
                param("kind", ParamKind::KwOnly, Some(empty)),
                param("annotation", ParamKind::KwOnly, Some(empty)),
                param("default", ParamKind::KwOnly, Some(empty)),
            ],
            parameter_replace_entry,
            MethodKind::Normal,
        ),
        (
            "__str__",
            "Parameter.__str__",
            vec![param("self", ParamKind::Positional, None)],
            parameter_str_entry,
            MethodKind::Normal,
        ),
        (
            "__repr__",
            "Parameter.__repr__",
            vec![param("self", ParamKind::Positional, None)],
            parameter_repr_entry,
            MethodKind::Normal,
        ),
        (
            "__eq__",
            "Parameter.__eq__",
            vec![
                param("self", ParamKind::Positional, None),
                param("other", ParamKind::Positional, None),
            ],
            parameter_eq_entry,
            MethodKind::Normal,
        ),
        (
            "__hash__",
            "Parameter.__hash__",
            vec![param("self", ParamKind::Positional, None)],
            parameter_hash_entry,
            MethodKind::Normal,
        ),
        (
            "name",
            "Parameter.name",
            vec![param("self", ParamKind::Positional, None)],
            parameter_name_entry,
            MethodKind::Property,
        ),
        (
            "default",
            "Parameter.default",
            vec![param("self", ParamKind::Positional, None)],
            parameter_default_entry,
            MethodKind::Property,
        ),
        (
            "annotation",
            "Parameter.annotation",
            vec![param("self", ParamKind::Positional, None)],
            parameter_annotation_entry,
            MethodKind::Property,
        ),
        (
            "kind",
            "Parameter.kind",
            vec![param("self", ParamKind::Positional, None)],
            parameter_kind_entry,
            MethodKind::Property,
        ),
    ];
    for (name, qualified, signature, entry, kind) in definitions {
        methods.push((
            name.to_string(),
            method_function(qualified, signature, entry),
            kind,
        ));
    }
    let method_values: Vec<i64> = methods.iter().map(|(_, function, _)| *function).collect();
    roots.keep(&method_values);
    let class = value::class_new_full("Parameter".to_string(), NONE, methods, Vec::new());
    cache("inspect.Parameter.class", class);
    let kind_class = parameter_kind_class();
    roots.keep(&[kind_class]);
    for (index, (name, _)) in KIND_DESCRIPTIONS.iter().enumerate() {
        if let Some(member) = kind_member(index as i64) {
            roots.keep(&[member]);
            value::class_set_attr(class, name.to_string(), member);
        }
    }
    value::class_set_attr(class, "empty".to_string(), empty);
    class
}

fn parameter_kind_index(parameter: i64) -> i64 {
    kind_index_of(parameter_field(parameter, "_kind")).unwrap_or(POSITIONAL_OR_KEYWORD)
}

fn make_parameter(name: &str, kind: i64, default: i64, annotation: i64) -> i64 {
    let class = parameter_class();
    let mut guard = Roots::new();
    guard.keep(&[default, annotation, class]);
    let kind_value = kind_member(kind).unwrap_or(NONE);
    let name_value = text(name);
    guard.keep(&[name_value, kind_value]);
    let parameter = value::instance_new(class);
    guard.keep(&[parameter]);
    value::instance_setattr(parameter, "_name".to_string(), name_value);
    value::instance_setattr(parameter, "_kind".to_string(), kind_value);
    value::instance_setattr(parameter, "_default".to_string(), default);
    value::instance_setattr(parameter, "_annotation".to_string(), annotation);
    parameter
}

fn parameter_init_impl(args: &[i64]) -> i64 {
    let instance = args[0];
    let name = args.get(1).copied().unwrap_or(NONE);
    let kind_value = args.get(2).copied().unwrap_or(NONE);
    let default = args.get(3).copied().unwrap_or_else(empty_class);
    let annotation = args.get(4).copied().unwrap_or_else(empty_class);
    let Some(mut index) = normalize_kind(kind_value) else {
        return value_error(&format!(
            "value {} is not a valid Parameter.kind",
            value::repr(kind_value)
        ));
    };
    if default != empty_class() && (index == VAR_POSITIONAL || index == VAR_KEYWORD) {
        return value_error(&format!(
            "{} parameters cannot have default values",
            kind_description(index)
        ));
    }
    if name == empty_class() {
        return value_error("name is a required attribute for Parameter");
    }
    let Some(mut name_text) = value::as_str(name).map(str::to_string) else {
        return type_error(&format!(
            "name must be a str, not a {}",
            value::type_name(name)
        ));
    };
    if name_text.starts_with('.') && name_text[1..].chars().all(|ch| ch.is_ascii_digit()) {
        if index != POSITIONAL_OR_KEYWORD {
            return value_error(&format!(
                "implicit arguments must be passed as positional or keyword arguments, not {}",
                kind_description(index)
            ));
        }
        index = POSITIONAL_ONLY;
        name_text = format!("implicit{}", &name_text[1..]);
    } else if name_text == ".format" {
        name_text = "format".to_string();
    }
    if (is_keyword(&name_text) && index != POSITIONAL_ONLY) || !is_identifier(&name_text) {
        return value_error(&format!("{name_text:?} is not a valid parameter name"));
    }
    // `normalize_kind` accepted this index and every branch above only narrows it
    // to another valid index, so the member always exists; the stored field is
    // the kind instance, never a bare integer.
    let kind_value = kind_member(index).unwrap_or(NONE);
    let mut guard = Roots::new();
    let name_value = text(&name_text);
    guard.keep(&[name_value, default, annotation, kind_value]);
    value::instance_setattr(instance, "_name".to_string(), name_value);
    value::instance_setattr(instance, "_kind".to_string(), kind_value);
    value::instance_setattr(instance, "_default".to_string(), default);
    value::instance_setattr(instance, "_annotation".to_string(), annotation);
    instance
}

extern "C" fn parameter_init_entry(argv: *const i64) -> i64 {
    // SAFETY: five declared parameter slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 5) };
    parameter_init_impl(args)
}

fn parameter_replace_impl(args: &[i64]) -> i64 {
    let parameter = args[0];
    let name = args.get(1).copied().unwrap_or_else(empty_class);
    let kind = args.get(2).copied().unwrap_or_else(empty_class);
    let annotation = args.get(3).copied().unwrap_or_else(empty_class);
    let default = args.get(4).copied().unwrap_or_else(empty_class);
    let name = if is_empty(name) {
        parameter_field(parameter, "_name")
    } else {
        name
    };
    let kind = if is_empty(kind) {
        parameter_field(parameter, "_kind")
    } else {
        kind
    };
    let annotation = if is_empty(annotation) {
        parameter_field(parameter, "_annotation")
    } else {
        annotation
    };
    let default = if is_empty(default) {
        parameter_field(parameter, "_default")
    } else {
        default
    };
    let class = parameter_class();
    let mut guard = Roots::new();
    guard.keep(&[name, kind, annotation, default, class]);
    let result = value::instance_new(class);
    guard.keep(&[result]);
    parameter_init_impl(&[result, name, kind, default, annotation])
}

extern "C" fn parameter_replace_entry(argv: *const i64) -> i64 {
    // SAFETY: five declared parameter slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 5) };
    parameter_replace_impl(args)
}

fn annotation_text(annotation: i64) -> Option<String> {
    if is_empty(annotation) {
        return None;
    }
    if let Some(name) = value::class_display_name(annotation) {
        return Some(name.to_string());
    }
    if let Some(function) = value::as_function(annotation) {
        return Some(function.name.clone());
    }
    if let Some(content) = value::as_str(annotation) {
        return Some(value::repr(text(content)));
    }
    Some(value::repr(annotation))
}

fn parameter_str_impl(parameter: i64, quote_annotations: bool) -> String {
    let mut formatted = parameter_text(parameter, "_name");
    let kind = parameter_kind_index(parameter);
    let annotation = parameter_field(parameter, "_annotation");
    let default = parameter_field(parameter, "_default");
    if !is_empty(annotation) {
        let rendered = if quote_annotations {
            annotation_text(annotation).unwrap_or_default()
        } else {
            value::as_str(annotation)
                .map(str::to_string)
                .unwrap_or_else(|| annotation_text(annotation).unwrap_or_default())
        };
        formatted = format!("{formatted}: {rendered}");
    }
    if !is_empty(default) {
        if !is_empty(annotation) {
            formatted = format!("{formatted} = {}", value::repr(default));
        } else {
            formatted = format!("{formatted}={}", value::repr(default));
        }
    }
    match kind {
        VAR_POSITIONAL => format!("*{formatted}"),
        VAR_KEYWORD => format!("**{formatted}"),
        _ => formatted,
    }
}

fn parameter_str_raw(parameter: i64) -> i64 {
    text(&parameter_str_impl(parameter, true))
}

extern "C" fn parameter_str_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    parameter_str_raw(unsafe { *argv })
}

fn parameter_repr_raw(parameter: i64) -> i64 {
    text(&format!(
        "<Parameter \"{}\">",
        parameter_str_impl(parameter, true)
    ))
}

extern "C" fn parameter_repr_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    parameter_repr_raw(unsafe { *argv })
}

fn parameter_eq_raw(left: i64, right: i64) -> i64 {
    if left == right {
        return TRUE;
    }
    if !is_instance_of(right, parameter_class()) {
        return FALSE;
    }
    let equal = parameter_field(left, "_name") == parameter_field(right, "_name")
        && parameter_field(left, "_kind") == parameter_field(right, "_kind")
        && container::equals(
            parameter_field(left, "_default"),
            parameter_field(right, "_default"),
        )
        && container::equals(
            parameter_field(left, "_annotation"),
            parameter_field(right, "_annotation"),
        );
    value::bool_value(equal)
}

extern "C" fn parameter_eq_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameters.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    parameter_eq_raw(args[0], args[1])
}

fn parameter_hash_raw(parameter: i64) -> i64 {
    crate::builtins::hash(text(&parameter_str_impl(parameter, true)))
}

extern "C" fn parameter_hash_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    parameter_hash_raw(unsafe { *argv })
}

extern "C" fn parameter_name_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    parameter_field(unsafe { *argv }, "_name")
}

extern "C" fn parameter_default_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    parameter_field(unsafe { *argv }, "_default")
}

extern "C" fn parameter_annotation_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    parameter_field(unsafe { *argv }, "_annotation")
}

extern "C" fn parameter_kind_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    parameter_field(unsafe { *argv }, "_kind")
}

// -------------------------------------------------------------------------
// `Signature`
// -------------------------------------------------------------------------

fn signature_class() -> i64 {
    if let Some(class) = cached("inspect.Signature.class") {
        return class;
    }
    let empty = empty_class();
    let mut roots = Roots::new();
    roots.keep(&[empty]);
    let definitions: [MethodDefinition; 10] = [
        (
            "__init__",
            "Signature.__init__",
            vec![
                param("self", ParamKind::Positional, None),
                param("parameters", ParamKind::Positional, Some(NONE)),
                param("return_annotation", ParamKind::KwOnly, Some(empty)),
            ],
            signature_init_entry,
            MethodKind::Normal,
        ),
        (
            "bind",
            "Signature.bind",
            vec![
                param("self", ParamKind::Positional, None),
                param("args", ParamKind::VarArg, None),
                param("kwargs", ParamKind::KwArg, None),
            ],
            signature_bind_entry,
            MethodKind::Normal,
        ),
        (
            "bind_partial",
            "Signature.bind_partial",
            vec![
                param("self", ParamKind::Positional, None),
                param("args", ParamKind::VarArg, None),
                param("kwargs", ParamKind::KwArg, None),
            ],
            signature_bind_partial_entry,
            MethodKind::Normal,
        ),
        (
            "replace",
            "Signature.replace",
            vec![
                param("self", ParamKind::Positional, None),
                param("parameters", ParamKind::KwOnly, Some(empty)),
                param("return_annotation", ParamKind::KwOnly, Some(empty)),
            ],
            signature_replace_entry,
            MethodKind::Normal,
        ),
        (
            "format",
            "Signature.format",
            vec![
                param("self", ParamKind::Positional, None),
                param("max_width", ParamKind::KwOnly, Some(NONE)),
                param("quote_annotation_strings", ParamKind::KwOnly, Some(TRUE)),
            ],
            signature_format_entry,
            MethodKind::Normal,
        ),
        (
            "__str__",
            "Signature.__str__",
            vec![param("self", ParamKind::Positional, None)],
            signature_str_entry,
            MethodKind::Normal,
        ),
        (
            "__repr__",
            "Signature.__repr__",
            vec![param("self", ParamKind::Positional, None)],
            signature_repr_entry,
            MethodKind::Normal,
        ),
        (
            "__eq__",
            "Signature.__eq__",
            vec![
                param("self", ParamKind::Positional, None),
                param("other", ParamKind::Positional, None),
            ],
            signature_eq_entry,
            MethodKind::Normal,
        ),
        (
            "parameters",
            "Signature.parameters",
            vec![param("self", ParamKind::Positional, None)],
            signature_parameters_entry,
            MethodKind::Property,
        ),
        (
            "return_annotation",
            "Signature.return_annotation",
            vec![param("self", ParamKind::Positional, None)],
            signature_return_annotation_entry,
            MethodKind::Property,
        ),
    ];
    let mut methods: Vec<(String, i64, MethodKind)> = Vec::with_capacity(definitions.len());
    for (name, qualified, signature, entry, kind) in definitions {
        methods.push((
            name.to_string(),
            method_function(qualified, signature, entry),
            kind,
        ));
    }
    let method_values: Vec<i64> = methods.iter().map(|(_, function, _)| *function).collect();
    roots.keep(&method_values);
    let class = value::class_new_full("Signature".to_string(), NONE, methods, Vec::new());
    cache("inspect.Signature.class", class);
    class
}

fn signature_parameters(signature: i64) -> i64 {
    parameter_field(signature, "_parameters")
}

fn signature_return_annotation(signature: i64) -> i64 {
    parameter_field(signature, "_return_annotation")
}

fn new_signature(parameters: i64, return_annotation: i64) -> i64 {
    let class = signature_class();
    let mut guard = Roots::new();
    guard.keep(&[parameters, return_annotation, class]);
    let signature = value::instance_new(class);
    guard.keep(&[signature]);
    value::instance_setattr(signature, "_parameters".to_string(), parameters);
    value::instance_setattr(
        signature,
        "_return_annotation".to_string(),
        return_annotation,
    );
    signature
}

fn empty_signature() -> i64 {
    let parameters = value::dict_new(Vec::new());
    let mut guard = Roots::new();
    guard.keep(&[parameters]);
    new_signature(parameters, empty_class())
}

fn signature_init_impl(args: &[i64]) -> i64 {
    let signature = args[0];
    let parameters = args.get(1).copied().unwrap_or(NONE);
    let return_annotation = args.get(2).copied().unwrap_or_else(empty_class);
    let stored = value::dict_new(Vec::new());
    let mut guard = Roots::new();
    guard.keep(&[stored, return_annotation]);
    if parameters != NONE {
        let values: Vec<i64> = if value::as_dict(parameters).is_some() {
            dict_pairs(parameters)
                .into_iter()
                .map(|(_, item)| item)
                .collect()
        } else {
            let iterator = iter::iter_of(parameters);
            if exc::pending() {
                return NONE;
            }
            let mut values = Vec::new();
            loop {
                let mut item = NONE;
                let status = iter::iter_next(iterator, &mut item);
                if exc::pending() {
                    return NONE;
                }
                if status == 0 {
                    break;
                }
                values.push(item);
            }
            values
        };
        for parameter in values {
            if !is_instance_of(parameter, parameter_class()) {
                return type_error(&format!("invalid parameter: {}", value::repr(parameter)));
            }
            let name = parameter_text(parameter, "_name");
            dict_insert_name(stored, &name, parameter);
        }
    }
    value::instance_setattr(signature, "_parameters".to_string(), stored);
    value::instance_setattr(
        signature,
        "_return_annotation".to_string(),
        return_annotation,
    );
    signature
}

extern "C" fn signature_init_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    signature_init_impl(args)
}

fn signature_replace_impl(args: &[i64]) -> i64 {
    let signature = args[0];
    let parameters = args.get(1).copied().unwrap_or_else(empty_class);
    let return_annotation = args.get(2).copied().unwrap_or_else(empty_class);
    let parameters = if is_empty(parameters) {
        signature_parameters(signature)
    } else {
        parameters
    };
    let return_annotation = if is_empty(return_annotation) {
        signature_return_annotation(signature)
    } else {
        return_annotation
    };
    let replacement = value::instance_new(signature_class());
    let mut guard = Roots::new();
    guard.keep(&[parameters, return_annotation, replacement]);
    signature_init_impl(&[replacement, parameters, return_annotation])
}

extern "C" fn signature_replace_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    signature_replace_impl(args)
}

fn signature_format_impl(signature: i64, max_width: i64, quote_annotations: bool) -> i64 {
    let parameters = signature_parameters(signature);
    let mut rendered: Vec<String> = Vec::new();
    let mut positional_only_separator = false;
    let mut keyword_only_separator = true;
    for (_, parameter) in dict_pairs(parameters) {
        if !is_instance_of(parameter, parameter_class()) {
            continue;
        }
        let kind = parameter_kind_index(parameter);
        let formatted = parameter_str_impl(parameter, quote_annotations);
        if kind == POSITIONAL_ONLY {
            positional_only_separator = true;
        } else if positional_only_separator {
            rendered.push("/".to_string());
            positional_only_separator = false;
        }
        if kind == VAR_POSITIONAL {
            keyword_only_separator = false;
        } else if kind == KEYWORD_ONLY && keyword_only_separator {
            rendered.push("*".to_string());
            keyword_only_separator = false;
        }
        rendered.push(formatted);
    }
    if positional_only_separator {
        rendered.push("/".to_string());
    }
    let mut formatted = format!("({})", rendered.join(", "));
    if let Some(width) = value::as_integer(max_width) {
        if width >= 0 && formatted.chars().count() > usize::try_from(width).unwrap_or(usize::MAX) {
            formatted = format!("(\n    {}\n)", rendered.join(",\n    "));
        }
    }
    let return_annotation = signature_return_annotation(signature);
    if !is_empty(return_annotation) {
        if let Some(annotation) = annotation_text(return_annotation) {
            formatted = format!("{formatted} -> {annotation}");
        }
    }
    text(&formatted)
}

fn signature_format_raw(signature: i64, max_width: i64, quote: bool) -> i64 {
    signature_format_impl(signature, max_width, quote)
}

extern "C" fn signature_format_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let max_width = args[1];
    let quote = if args[2] == NONE {
        true
    } else {
        value::truthy(args[2])
    };
    signature_format_raw(args[0], max_width, quote)
}

fn signature_str_raw(signature: i64) -> i64 {
    signature_format_raw(signature, NONE, true)
}

extern "C" fn signature_str_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    signature_str_raw(unsafe { *argv })
}

fn signature_repr_raw(signature: i64) -> i64 {
    let rendered = value::as_str(signature_str_raw(signature))
        .map(str::to_string)
        .unwrap_or_default();
    text(&format!("<Signature {rendered}>"))
}

extern "C" fn signature_repr_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    signature_repr_raw(unsafe { *argv })
}

fn signature_eq_raw(left: i64, right: i64) -> i64 {
    if left == right {
        return TRUE;
    }
    if !is_instance_of(right, signature_class()) {
        return FALSE;
    }
    let left_text = value::as_str(signature_str_raw(left))
        .map(str::to_string)
        .unwrap_or_default();
    let right_text = value::as_str(signature_str_raw(right))
        .map(str::to_string)
        .unwrap_or_default();
    value::bool_value(
        left_text == right_text
            && container::equals(
                signature_return_annotation(left),
                signature_return_annotation(right),
            ),
    )
}

extern "C" fn signature_eq_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameters.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    signature_eq_raw(args[0], args[1])
}

extern "C" fn signature_parameters_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    dict_copy(signature_parameters(unsafe { *argv }))
}

extern "C" fn signature_return_annotation_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    signature_return_annotation(unsafe { *argv })
}

// -------------------------------------------------------------------------
// Argument binding
// -------------------------------------------------------------------------

struct ParameterInfo {
    name: String,
    parameter: i64,
    kind: i64,
    default: i64,
    annotation: i64,
}

fn parameter_infos(signature: i64) -> Vec<ParameterInfo> {
    dict_pairs(signature_parameters(signature))
        .into_iter()
        .filter_map(|(key, parameter)| {
            let name = value::as_str(key).map(str::to_string)?;
            Some(ParameterInfo {
                name,
                parameter,
                kind: parameter_kind_index(parameter),
                default: parameter_field(parameter, "_default"),
                annotation: parameter_field(parameter, "_annotation"),
            })
        })
        .collect()
}

fn keyword_position(keywords: &[(String, i64)], name: &str) -> Option<usize> {
    keywords.iter().position(|(keyword, _)| keyword == name)
}

fn new_bound_arguments(signature: i64, arguments: i64) -> i64 {
    let class = bound_arguments_class();
    let mut guard = Roots::new();
    guard.keep(&[signature, arguments, class]);
    let instance = value::instance_new(class);
    guard.keep(&[instance]);
    value::instance_setattr(instance, "_signature".to_string(), signature);
    value::instance_setattr(instance, "arguments".to_string(), arguments);
    instance
}

/// CPython's `Signature._bind` over the runtime's `Signature` objects.
fn signature_bind_impl(
    signature: i64,
    positional: &[i64],
    keywords: &[(String, i64)],
    partial: bool,
) -> i64 {
    let parameters = parameter_infos(signature);
    let arguments = value::dict_new(Vec::new());
    let mut guard = Roots::new();
    guard.keep(&[arguments, signature]);
    let mut remaining: Vec<(String, i64)> = keywords.to_vec();
    let mut position = 0usize;
    let mut index = 0usize;
    loop {
        if position < positional.len() {
            let Some(info) = parameters.get(index) else {
                return type_error("too many positional arguments");
            };
            index += 1;
            match info.kind {
                VAR_KEYWORD | KEYWORD_ONLY => {
                    return type_error("too many positional arguments");
                }
                VAR_POSITIONAL => {
                    let rest: Vec<i64> = positional[position..].to_vec();
                    let tuple = value::tuple_new(rest);
                    guard.keep(&[tuple]);
                    dict_insert_name(arguments, &info.name, tuple);
                    break;
                }
                _ => {
                    if info.kind != POSITIONAL_ONLY
                        && keyword_position(&remaining, &info.name).is_some()
                    {
                        return type_error(&format!(
                            "multiple values for argument {:?}",
                            info.name
                        ));
                    }
                    let argument = positional[position];
                    position += 1;
                    dict_insert_name(arguments, &info.name, argument);
                }
            }
            continue;
        }
        let Some(info) = parameters.get(index) else {
            break;
        };
        if info.kind == VAR_POSITIONAL {
            break;
        }
        if keyword_position(&remaining, &info.name).is_some() {
            if info.kind == POSITIONAL_ONLY {
                if info.default == empty_class() {
                    return type_error(&format!(
                        "missing a required positional-only argument: {:?}",
                        info.name
                    ));
                }
                index += 1;
                continue;
            }
            break;
        }
        if info.kind == VAR_KEYWORD || info.default != empty_class() || partial {
            break;
        }
        let kind = if info.kind == KEYWORD_ONLY {
            " keyword-only"
        } else {
            ""
        };
        return type_error(&format!(
            "missing a required{kind} argument: {:?}",
            info.name
        ));
    }
    let mut kwargs_parameter: Option<String> = None;
    for info in &parameters[index.min(parameters.len())..] {
        if info.kind == VAR_KEYWORD {
            kwargs_parameter = Some(info.name.clone());
            continue;
        }
        if info.kind == VAR_POSITIONAL {
            continue;
        }
        if let Some(at) = keyword_position(&remaining, &info.name) {
            let (_, argument) = remaining.remove(at);
            dict_insert_name(arguments, &info.name, argument);
        } else if !partial && info.default == empty_class() {
            return type_error(&format!("missing a required argument: {:?}", info.name));
        }
    }
    if !remaining.is_empty() {
        if let Some(name) = kwargs_parameter {
            let extra = value::dict_new(Vec::new());
            guard.keep(&[extra]);
            for (keyword, argument) in &remaining {
                dict_insert_name(extra, keyword, *argument);
            }
            dict_insert_name(arguments, &name, extra);
        } else {
            let names: Vec<String> = parameters
                .iter()
                .filter(|info| {
                    info.kind == POSITIONAL_ONLY
                        && remaining.iter().any(|(name, _)| *name == info.name)
                })
                .map(|info| format!("{:?}", info.name))
                .collect();
            if !names.is_empty() {
                return type_error(&format!(
                    "got some positional-only arguments passed as keyword arguments: {}",
                    names.join(", ")
                ));
            }
            return type_error(&format!(
                "got an unexpected keyword argument {:?}",
                remaining[0].0
            ));
        }
    }
    new_bound_arguments(signature, arguments)
}

fn signature_bind_raw(args: &[i64], partial: bool) -> i64 {
    let signature = args[0];
    if args.len() == 3 && value::as_list(args[1]).is_some() && value::as_dict(args[2]).is_some() {
        let positional = value::as_list(args[1])
            .map(|items| items.to_vec())
            .unwrap_or_default();
        let keywords: Vec<(String, i64)> = dict_pairs(args[2])
            .into_iter()
            .filter_map(|(key, item)| value::as_str(key).map(|name| (name.to_string(), item)))
            .collect();
        return signature_bind_impl(signature, &positional, &keywords, partial);
    }
    signature_bind_impl(signature, args.get(1..).unwrap_or(&[]), &[], partial)
}

extern "C" fn signature_bind_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    signature_bind_raw(args, false)
}

extern "C" fn signature_bind_partial_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    signature_bind_raw(args, true)
}

// -------------------------------------------------------------------------
// `BoundArguments`
// -------------------------------------------------------------------------

fn bound_arguments_class() -> i64 {
    if let Some(class) = cached("inspect.BoundArguments.class") {
        return class;
    }
    let mut roots = Roots::new();
    let methods: Vec<(String, i64, MethodKind)> = vec![
        (
            "__init__".to_string(),
            method_function(
                "BoundArguments.__init__",
                vec![
                    param("self", ParamKind::Positional, None),
                    param("signature", ParamKind::Positional, None),
                    param("arguments", ParamKind::Positional, None),
                ],
                bound_arguments_init_entry,
            ),
            MethodKind::Normal,
        ),
        (
            "apply_defaults".to_string(),
            method_function(
                "BoundArguments.apply_defaults",
                vec![param("self", ParamKind::Positional, None)],
                bound_arguments_apply_defaults_entry,
            ),
            MethodKind::Normal,
        ),
        (
            "__repr__".to_string(),
            method_function(
                "BoundArguments.__repr__",
                vec![param("self", ParamKind::Positional, None)],
                bound_arguments_repr_entry,
            ),
            MethodKind::Normal,
        ),
        (
            "signature".to_string(),
            method_function(
                "BoundArguments.signature",
                vec![param("self", ParamKind::Positional, None)],
                bound_arguments_signature_entry,
            ),
            MethodKind::Property,
        ),
        (
            "args".to_string(),
            method_function(
                "BoundArguments.args",
                vec![param("self", ParamKind::Positional, None)],
                bound_arguments_args_entry,
            ),
            MethodKind::Property,
        ),
        (
            "kwargs".to_string(),
            method_function(
                "BoundArguments.kwargs",
                vec![param("self", ParamKind::Positional, None)],
                bound_arguments_kwargs_entry,
            ),
            MethodKind::Property,
        ),
    ];
    let method_values: Vec<i64> = methods.iter().map(|(_, function, _)| *function).collect();
    roots.keep(&method_values);
    let class = value::class_new_full("BoundArguments".to_string(), NONE, methods, Vec::new());
    cache("inspect.BoundArguments.class", class);
    class
}

fn bound_signature(bound: i64) -> i64 {
    parameter_field(bound, "_signature")
}

fn bound_arguments_dict(bound: i64) -> i64 {
    parameter_field(bound, "arguments")
}

fn bound_arguments_init_impl(args: &[i64]) -> i64 {
    let instance = args[0];
    let signature = args.get(1).copied().unwrap_or(NONE);
    let arguments = args.get(2).copied().unwrap_or(NONE);
    if value::as_dict(arguments).is_none() {
        return type_error("arguments must be a dict");
    }
    let mut guard = Roots::new();
    guard.keep(&[signature, arguments]);
    value::instance_setattr(instance, "_signature".to_string(), signature);
    value::instance_setattr(instance, "arguments".to_string(), arguments);
    instance
}

extern "C" fn bound_arguments_init_entry(argv: *const i64) -> i64 {
    // SAFETY: three declared parameter slots.
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    bound_arguments_init_impl(args)
}

fn bound_arguments_apply_defaults(bound: i64) -> i64 {
    let signature = bound_signature(bound);
    let arguments = bound_arguments_dict(bound);
    let parameters = parameter_infos(signature);
    let replacement = value::dict_new(Vec::new());
    let mut guard = Roots::new();
    guard.keep(&[replacement, arguments]);
    for info in parameters {
        let value_in = match dict_lookup_name(arguments, &info.name) {
            Some(argument) => argument,
            None => {
                if info.default != empty_class() {
                    info.default
                } else if info.kind == VAR_POSITIONAL {
                    value::tuple_new(Vec::new())
                } else if info.kind == VAR_KEYWORD {
                    value::dict_new(Vec::new())
                } else {
                    continue;
                }
            }
        };
        guard.keep(&[value_in]);
        dict_insert_name(replacement, &info.name, value_in);
    }
    value::instance_setattr(bound, "arguments".to_string(), replacement);
    NONE
}

extern "C" fn bound_arguments_apply_defaults_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    bound_arguments_apply_defaults(unsafe { *argv })
}

fn bound_arguments_args(bound: i64) -> i64 {
    let signature = bound_signature(bound);
    let arguments = bound_arguments_dict(bound);
    let mut values: Vec<i64> = Vec::new();
    let mut guard = Roots::new();
    for info in parameter_infos(signature) {
        if info.kind == VAR_KEYWORD || info.kind == KEYWORD_ONLY {
            break;
        }
        let Some(argument) = dict_lookup_name(arguments, &info.name) else {
            break;
        };
        if info.kind == VAR_POSITIONAL {
            if let Some(items) = value::as_tuple(argument) {
                for item in items {
                    guard.keep(&[*item]);
                    values.push(*item);
                }
            } else if let Some(items) = value::as_list(argument) {
                for item in items {
                    guard.keep(&[*item]);
                    values.push(*item);
                }
            }
        } else {
            guard.keep(&[argument]);
            values.push(argument);
        }
    }
    value::tuple_new(values)
}

extern "C" fn bound_arguments_args_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    bound_arguments_args(unsafe { *argv })
}

fn bound_arguments_kwargs(bound: i64) -> i64 {
    let signature = bound_signature(bound);
    let arguments = bound_arguments_dict(bound);
    let result = value::dict_new(Vec::new());
    let mut guard = Roots::new();
    guard.keep(&[result, arguments]);
    let mut started = false;
    for info in parameter_infos(signature) {
        if !started {
            if info.kind == VAR_KEYWORD || info.kind == KEYWORD_ONLY {
                started = true;
            } else if dict_lookup_name(arguments, &info.name).is_none() {
                started = true;
                continue;
            }
        }
        if !started {
            continue;
        }
        let Some(argument) = dict_lookup_name(arguments, &info.name) else {
            continue;
        };
        if info.kind == VAR_KEYWORD {
            for (key, item) in dict_pairs(argument) {
                dict_insert_key(result, key, item);
            }
        } else {
            dict_insert_name(result, &info.name, argument);
        }
    }
    result
}

extern "C" fn bound_arguments_kwargs_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    bound_arguments_kwargs(unsafe { *argv })
}

fn bound_arguments_repr(bound: i64) -> i64 {
    let rendered: Vec<String> = dict_pairs(bound_arguments_dict(bound))
        .into_iter()
        .filter_map(|(key, item)| {
            value::as_str(key).map(|name| format!("{name}={}", value::repr(item)))
        })
        .collect();
    text(&format!("<BoundArguments ({})>", rendered.join(", ")))
}

extern "C" fn bound_arguments_repr_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    bound_arguments_repr(unsafe { *argv })
}

extern "C" fn bound_arguments_signature_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    bound_signature(unsafe { *argv })
}

// -------------------------------------------------------------------------
// Namedtuple-like helper classes (`FullArgSpec`, `FrameInfo`, `ClosureVars`)
// -------------------------------------------------------------------------

const FULL_ARG_SPEC_FIELDS: [&str; 7] = [
    "args",
    "varargs",
    "varkw",
    "defaults",
    "kwonlyargs",
    "kwonlydefaults",
    "annotations",
];

const FRAME_INFO_FIELDS: [&str; 6] = [
    "frame",
    "filename",
    "lineno",
    "function",
    "code_context",
    "index",
];

const CLOSURE_VARS_FIELDS: [&str; 4] = ["nonlocals", "globals", "builtins", "unbound"];

fn namedtuple_methods(class_name: &'static str) -> Vec<(String, i64, MethodKind)> {
    let qualified = |suffix: &'static str| -> &'static str {
        match (class_name, suffix) {
            ("FullArgSpec", "__iter__") => "FullArgSpec.__iter__",
            ("FullArgSpec", "__len__") => "FullArgSpec.__len__",
            ("FullArgSpec", "__getitem__") => "FullArgSpec.__getitem__",
            ("FullArgSpec", "__repr__") => "FullArgSpec.__repr__",
            ("FrameInfo", "__iter__") => "FrameInfo.__iter__",
            ("FrameInfo", "__len__") => "FrameInfo.__len__",
            ("FrameInfo", "__getitem__") => "FrameInfo.__getitem__",
            ("FrameInfo", "__repr__") => "FrameInfo.__repr__",
            ("ClosureVars", "__iter__") => "ClosureVars.__iter__",
            ("ClosureVars", "__len__") => "ClosureVars.__len__",
            ("ClosureVars", "__getitem__") => "ClosureVars.__getitem__",
            _ => "ClosureVars.__repr__",
        }
    };
    let mut methods = Vec::new();
    for (name, entry, arity) in [
        (
            "__iter__",
            namedtuple_iter_entry as extern "C" fn(*const i64) -> i64,
            1usize,
        ),
        ("__len__", namedtuple_len_entry, 1),
        ("__getitem__", namedtuple_getitem_entry, 2),
        ("__repr__", namedtuple_repr_entry, 1),
    ] {
        let signature = if arity == 2 {
            vec![
                param("self", ParamKind::Positional, None),
                param("index", ParamKind::Positional, None),
            ]
        } else {
            vec![param("self", ParamKind::Positional, None)]
        };
        methods.push((
            name.to_string(),
            method_function(qualified(name), signature, entry),
            MethodKind::Normal,
        ));
    }
    methods
}

fn full_arg_spec_class() -> i64 {
    namedtuple_helper_class("FullArgSpec", &FULL_ARG_SPEC_FIELDS)
}

fn frame_info_class() -> i64 {
    namedtuple_helper_class("FrameInfo", &FRAME_INFO_FIELDS)
}

fn closure_vars_class() -> i64 {
    namedtuple_helper_class("ClosureVars", &CLOSURE_VARS_FIELDS)
}

fn namedtuple_helper_class(name: &'static str, fields: &[&'static str]) -> i64 {
    let key = format!("inspect.{name}.class");
    if let Some(class) = cached(&key) {
        return class;
    }
    let mut roots = Roots::new();
    let methods = namedtuple_methods(name);
    let method_values: Vec<i64> = methods.iter().map(|(_, function, _)| *function).collect();
    roots.keep(&method_values);
    let class = value::class_new_full(name.to_string(), NONE, methods, Vec::new());
    cache(&key, class);
    let field_values: Vec<i64> = fields.iter().map(|field| text(field)).collect();
    roots.keep(&field_values);
    value::class_set_attr(class, "_fields".to_string(), value::tuple_new(field_values));
    class
}

fn namedtuple_fields(obj: i64) -> &'static [&'static str] {
    let class = value::instance_class_value(obj).unwrap_or(NONE);
    if class == full_arg_spec_class() {
        &FULL_ARG_SPEC_FIELDS
    } else if class == frame_info_class() {
        &FRAME_INFO_FIELDS
    } else {
        &CLOSURE_VARS_FIELDS
    }
}

fn namedtuple_values_of(obj: i64) -> Vec<i64> {
    namedtuple_fields(obj)
        .iter()
        .map(|name| parameter_field(obj, name))
        .collect()
}

fn namedtuple_iter_raw(obj: i64) -> i64 {
    let list = value::list_new(namedtuple_values_of(obj));
    iter::iter_of(list)
}

extern "C" fn namedtuple_iter_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    namedtuple_iter_raw(unsafe { *argv })
}

extern "C" fn namedtuple_len_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    integer(namedtuple_values_of(unsafe { *argv }).len() as i64)
}

extern "C" fn namedtuple_getitem_entry(argv: *const i64) -> i64 {
    // SAFETY: two declared parameters.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    namedtuple_getitem(args[0], args[1])
}

fn namedtuple_getitem(obj: i64, index: i64) -> i64 {
    let values = namedtuple_values_of(obj);
    let Some(index) = value::as_integer(index) else {
        return type_error("namedtuple indices must be integers");
    };
    let length = values.len() as i64;
    let index = if index < 0 { index + length } else { index };
    if index < 0 || index >= length {
        return exc::raise_with_message(INDEX_ERROR, "tuple index out of range");
    }
    values[usize::try_from(index).unwrap_or(0)]
}

fn namedtuple_repr_raw(obj: i64) -> i64 {
    let class_name = value::instance_class_name(obj).unwrap_or("namedtuple");
    let rendered: Vec<String> = namedtuple_fields(obj)
        .iter()
        .zip(namedtuple_values_of(obj))
        .map(|(name, item)| format!("{name}={}", value::repr(item)))
        .collect();
    text(&format!("{class_name}({})", rendered.join(", ")))
}

extern "C" fn namedtuple_repr_entry(argv: *const i64) -> i64 {
    // SAFETY: one declared parameter.
    namedtuple_repr_raw(unsafe { *argv })
}

fn namedtuple_instance(class: i64, values: &[(&str, i64)]) -> i64 {
    let mut guard = Roots::new();
    guard.keep(&[class]);
    for &(_, item) in values {
        guard.keep(&[item]);
    }
    let instance = value::instance_new(class);
    guard.keep(&[instance]);
    for &(name, item) in values {
        value::instance_setattr(instance, name.to_string(), item);
    }
    instance
}

// -------------------------------------------------------------------------
// Predicates
// -------------------------------------------------------------------------

fn resolved_method(obj: i64) -> Option<(MethodKind, i64)> {
    let function = value::as_function(obj)?;
    let FunctionBody::BoundMethod { receiver, method } = &function.body else {
        return None;
    };
    let class = value::instance_class_value(*receiver).or_else(|| {
        if value::as_class(*receiver).is_some() {
            Some(*receiver)
        } else {
            None
        }
    })?;
    match value::class_lookup(class, method)? {
        ClassEntry::Method(kind, target) => Some((kind, target)),
        ClassEntry::Attr(item) if value::as_function(item).is_some() => {
            Some((MethodKind::Normal, item))
        }
        ClassEntry::Attr(_) => None,
    }
}

fn is_user_function(obj: i64) -> bool {
    matches!(
        function_body(obj),
        Some(FunctionBody::Wrapper { .. } | FunctionBody::Host { .. } | FunctionBody::Exec { .. })
    )
}

fn is_method(obj: i64) -> bool {
    matches!(function_body(obj), Some(FunctionBody::BoundMethod { .. }))
        && resolved_method(obj)
            .map(|(_, target)| is_user_function(target))
            .unwrap_or(false)
}

fn is_builtin(obj: i64) -> bool {
    match function_body(obj) {
        Some(FunctionBody::Native { .. } | FunctionBody::Builtin(_)) => true,
        // A bound method of a native receiver (`[].append`) has no class
        // namespace to resolve, but it is still a builtin method.
        Some(FunctionBody::BoundMethod { .. }) => resolved_method(obj)
            .map(|(_, target)| !is_user_function(target))
            .unwrap_or(true),
        _ => false,
    }
}

fn type_object_of(obj: i64) -> Option<i64> {
    value::instance_class_value(obj)
}

fn class_has_attribute(class: i64, name: &str) -> bool {
    value::class_lookup(class, name).is_some()
}

fn object_is_method_descriptor(obj: i64) -> bool {
    if value::as_class(obj).is_some() || is_method(obj) || is_user_function(obj) {
        return false;
    }
    if let Some(descriptor) = value::as_descriptor(obj) {
        return !matches!(
            descriptor.kind,
            DescriptorKind::Property | DescriptorKind::TypeAnnotations
        );
    }
    let Some(class) = type_object_of(obj) else {
        return false;
    };
    class_has_attribute(class, "__get__")
        && !class_has_attribute(class, "__set__")
        && !class_has_attribute(class, "__delete__")
}

fn object_is_data_descriptor(obj: i64) -> bool {
    if value::as_class(obj).is_some() || is_method(obj) || is_user_function(obj) {
        return false;
    }
    if let Some(descriptor) = value::as_descriptor(obj) {
        return matches!(
            descriptor.kind,
            DescriptorKind::Property | DescriptorKind::TypeAnnotations
        );
    }
    let Some(class) = type_object_of(obj) else {
        return false;
    };
    class_has_attribute(class, "__set__") || class_has_attribute(class, "__delete__")
}

fn flag_target(obj: i64) -> i64 {
    let mut target = crate::modules::functools::unwrap_partialmethod(obj);
    while let Some((_, function)) = resolved_method(target) {
        target = function;
    }
    crate::modules::functools::unwrap_partial(target)
}

fn function_flags(obj: i64) -> Option<u32> {
    value::as_function(obj).map(|function| function.flags)
}

fn is_generator_function(obj: i64) -> bool {
    let target = flag_target(obj);
    if !is_user_function(target) {
        return false;
    }
    function_flags(target).is_some_and(|flags| {
        flags & value::FUNCTION_FLAG_GENERATOR != 0 && flags & value::FUNCTION_FLAG_ASYNC == 0
    })
}

/// Attribute `inspect.markcoroutinefunction` writes on a callable whose code
/// it cannot rewrite. CPython sets `CO_COROUTINE` on `__code__`; Hydro keeps
/// no code object, so the marker lives in the function's attribute dictionary
/// and [`is_coroutine_function`] treats it like the flag.
const COROUTINE_FUNCTION_MARKER: &str = "__hydro_coroutine_function__";

fn is_coroutine_function(obj: i64) -> bool {
    let target = flag_target(obj);
    // The marker is authoritative: it is the code-free equivalent of
    // `CO_COROUTINE`.
    if value::function_attribute(target, COROUTINE_FUNCTION_MARKER).is_some_and(value::truthy) {
        return true;
    }
    if !is_user_function(target) {
        return false;
    }
    function_flags(target).is_some_and(|flags| {
        flags & value::FUNCTION_FLAG_ASYNC != 0 && flags & value::FUNCTION_FLAG_GENERATOR == 0
    })
}

/// `inspect.markcoroutinefunction(func)`: makes `iscoroutinefunction(func)`
/// report `True` and returns the callable.
///
/// CPython rewrites the function's code flags. Hydro function values have no
/// code object, so the equivalent marker is stored in the writable attribute
/// surface. As in CPython, a bound method marks its underlying function. A
/// callable without writable function metadata (a native/builtin function)
/// raises a precise `AttributeError` instead of silently succeeding.
fn mark_coroutine_function(obj: i64) -> i64 {
    // Bound methods and `functools.partial` wrappers are marked on the
    // underlying function, exactly like `iscoroutinefunction` looks through
    // them.
    let target = flag_target(obj);
    if value::as_function(target).is_none() || !value::function_has_attributes(target) {
        return exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!(
                "cannot mark {} as a coroutine function: the value has no writable \
                 function metadata",
                value::repr(obj)
            ),
        );
    }
    if !value::function_setattr(target, COROUTINE_FUNCTION_MARKER, TRUE) {
        return exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!(
                "cannot mark {} as a coroutine function: the value has no writable \
                 function metadata",
                value::repr(obj)
            ),
        );
    }
    target
}

fn is_async_generator_function(obj: i64) -> bool {
    let target = flag_target(obj);
    if !is_user_function(target) {
        return false;
    }
    function_flags(target).is_some_and(|flags| {
        flags & value::FUNCTION_FLAG_ASYNC != 0 && flags & value::FUNCTION_FLAG_GENERATOR != 0
    })
}

fn fiber_kind_of(obj: i64) -> Option<crate::fiber::FiberKind> {
    value::as_fiber(obj).map(|fiber| fiber.kind)
}

fn is_awaitable(obj: i64) -> bool {
    if fiber_kind_of(obj) == Some(crate::fiber::FiberKind::Coroutine) {
        return true;
    }
    value::instance_class_value(obj).is_some_and(|class| class_has_attribute(class, "__await__"))
}

fn is_abstract_class(obj: i64) -> bool {
    if value::as_class(obj).is_none() {
        return false;
    }
    let metaclass = value::class_metaclass(obj);
    let is_abc = metaclass != NONE
        && value::class_mro(metaclass)
            .iter()
            .any(|&ancestor| value::class_display_name(ancestor) == Some("ABCMeta"));
    if is_abc {
        let methods = crate::modules::abc::call("ABCMeta.__abstractmethods__", &[obj]);
        if exc::pending() {
            return false;
        }
        if value::as_set(methods).is_some_and(|names| !names.is_empty()) {
            return true;
        }
    }
    class_bases(obj).into_iter().any(is_abstract_class)
}

// -------------------------------------------------------------------------
// Signature construction
// -------------------------------------------------------------------------

fn signature_from_param_meta(function: i64, skip_first: bool) -> i64 {
    let parameters = value::dict_new(Vec::new());
    let mut guard = Roots::new();
    guard.keep(&[parameters, function]);
    for (index, meta) in value::function_signature(function).iter().enumerate() {
        if skip_first && index == 0 {
            continue;
        }
        let default = meta.default.unwrap_or_else(empty_class);
        let parameter = make_parameter(
            &meta.name,
            kind_index_for(meta.kind),
            default,
            empty_class(),
        );
        guard.keep(&[parameter]);
        dict_insert_name(parameters, &meta.name, parameter);
    }
    new_signature(parameters, empty_class())
}

fn native_spec_of(module: &str, name: &str) -> Option<&'static hydro_ir::stdlib::FunctionSpec> {
    if module == "inspect" {
        if let Some(spec) = function_spec(name) {
            return Some(spec);
        }
    }
    hydro_ir::stdlib::function(module, name).or_else(|| super::supplemental_function(module, name))
}

fn signature_from_native_spec(module: &str, name: &str, skip_first: bool) -> Result<i64, i64> {
    let Some(spec) = native_spec_of(module, name) else {
        return Err(type_error(&format!(
            "no signature found for builtin {module}.{name}"
        )));
    };
    let parameters = value::dict_new(Vec::new());
    let mut guard = Roots::new();
    guard.keep(&[parameters]);
    let positional_count = if spec.max_args == usize::MAX {
        spec.min_args
    } else {
        spec.max_args
    };
    for index in 0..positional_count {
        if skip_first && index == 0 {
            continue;
        }
        let declared = spec.params.get(index).copied();
        let parameter_name = match declared {
            Some(declared) => declared.to_string(),
            None => format!("arg{}", index + 1),
        };
        let kind = if declared.is_some() {
            POSITIONAL_OR_KEYWORD
        } else {
            POSITIONAL_ONLY
        };
        // The spec records arity, not default values; `None` keeps the
        // parameter optional and the native implementation applies the real
        // default.
        let default = if index >= spec.min_args {
            NONE
        } else {
            empty_class()
        };
        let parameter = make_parameter(&parameter_name, kind, default, empty_class());
        guard.keep(&[parameter]);
        dict_insert_name(parameters, &parameter_name, parameter);
    }
    if spec.max_args == usize::MAX {
        let parameter = make_parameter("args", VAR_POSITIONAL, empty_class(), empty_class());
        guard.keep(&[parameter]);
        dict_insert_name(parameters, "args", parameter);
    }
    if spec.dynamic_keywords {
        let parameter = make_parameter("kwargs", VAR_KEYWORD, empty_class(), empty_class());
        guard.keep(&[parameter]);
        dict_insert_name(parameters, "kwargs", parameter);
    }
    Ok(new_signature(parameters, empty_class()))
}

#[derive(Clone, Copy)]
enum BuiltinDefault {
    Empty,
    NoValue,
    Int(i64),
    Str(&'static str),
    Bool(bool),
    EmptyTuple,
}

struct BuiltinParam {
    name: &'static str,
    kind: ParamKind,
    default: BuiltinDefault,
}

enum BuiltinSignature {
    Params(Vec<BuiltinParam>),
    Unavailable,
}

const fn bp(name: &'static str, kind: ParamKind) -> BuiltinParam {
    BuiltinParam {
        name,
        kind,
        default: BuiltinDefault::Empty,
    }
}

const fn bpd(name: &'static str, kind: ParamKind, default: BuiltinDefault) -> BuiltinParam {
    BuiltinParam {
        name,
        kind,
        default,
    }
}

/// CPython's real signatures for builtins with a text signature.
fn builtin_signature(name: &str) -> BuiltinSignature {
    use BuiltinDefault::*;
    use ParamKind::*;
    match name {
        "len" => BuiltinSignature::Params(vec![bp("obj", PosOnly)]),
        "print" => BuiltinSignature::Params(vec![
            bp("args", VarArg),
            bpd("sep", KwOnly, Str(" ")),
            bpd("end", KwOnly, Str("\n")),
            bpd("file", KwOnly, NoValue),
            bpd("flush", KwOnly, Bool(false)),
        ]),
        "float" => BuiltinSignature::Params(vec![bpd("x", PosOnly, Int(0))]),
        "bool" => BuiltinSignature::Params(vec![bpd("object", PosOnly, Bool(false))]),
        "list" | "tuple" | "set" | "frozenset" => {
            BuiltinSignature::Params(vec![bpd("iterable", PosOnly, EmptyTuple)])
        }
        "sorted" => BuiltinSignature::Params(vec![
            bp("iterable", PosOnly),
            bpd("key", KwOnly, NoValue),
            bpd("reverse", KwOnly, Bool(false)),
        ]),
        "sum" => {
            BuiltinSignature::Params(vec![bp("iterable", PosOnly), bpd("start", PosOnly, Int(0))])
        }
        "abs" => BuiltinSignature::Params(vec![bp("x", PosOnly)]),
        "repr" => BuiltinSignature::Params(vec![bp("obj", PosOnly)]),
        "id" => BuiltinSignature::Params(vec![bp("obj", PosOnly)]),
        "hash" => BuiltinSignature::Params(vec![bp("obj", PosOnly)]),
        "hasattr" => BuiltinSignature::Params(vec![bp("obj", PosOnly), bp("name", PosOnly)]),
        "isinstance" => {
            BuiltinSignature::Params(vec![bp("obj", PosOnly), bp("class_or_tuple", PosOnly)])
        }
        "issubclass" => {
            BuiltinSignature::Params(vec![bp("cls", PosOnly), bp("class_or_tuple", PosOnly)])
        }
        "open" => BuiltinSignature::Params(vec![
            bp("file", Positional),
            bpd("mode", KwOnly, Str("r")),
            bpd("buffering", KwOnly, Int(-1)),
            bpd("encoding", KwOnly, NoValue),
            bpd("errors", KwOnly, NoValue),
            bpd("newline", KwOnly, NoValue),
            bpd("closefd", KwOnly, Bool(true)),
            bpd("opener", KwOnly, NoValue),
        ]),
        "format" => BuiltinSignature::Params(vec![
            bp("value", PosOnly),
            bpd("format_spec", PosOnly, Str("")),
        ]),
        "ord" => BuiltinSignature::Params(vec![bp("character", PosOnly)]),
        "chr" => BuiltinSignature::Params(vec![bp("i", PosOnly)]),
        "any" => BuiltinSignature::Params(vec![bp("iterable", PosOnly)]),
        "all" => BuiltinSignature::Params(vec![bp("iterable", PosOnly)]),
        "enumerate" => BuiltinSignature::Params(vec![
            bp("iterable", Positional),
            bpd("start", Positional, Int(0)),
        ]),
        "zip" => BuiltinSignature::Params(vec![
            bp("iterables", VarArg),
            bpd("strict", KwOnly, Bool(false)),
        ]),
        "map" => BuiltinSignature::Params(vec![
            bp("function", PosOnly),
            bp("iterable", PosOnly),
            bp("iterables", VarArg),
            bpd("strict", KwOnly, Bool(false)),
        ]),
        "filter" => {
            BuiltinSignature::Params(vec![bp("function", PosOnly), bp("iterable", PosOnly)])
        }
        "pow" => BuiltinSignature::Params(vec![
            bp("base", Positional),
            bp("exp", Positional),
            bpd("mod", Positional, NoValue),
        ]),
        "divmod" => BuiltinSignature::Params(vec![bp("x", PosOnly), bp("y", PosOnly)]),
        "round" => BuiltinSignature::Params(vec![
            bp("number", Positional),
            bpd("ndigits", Positional, NoValue),
        ]),
        "memoryview" => BuiltinSignature::Params(vec![bp("object", Positional)]),
        "object" => BuiltinSignature::Params(Vec::new()),
        "staticmethod" | "classmethod" => BuiltinSignature::Params(vec![bp("function", PosOnly)]),
        "property" => BuiltinSignature::Params(vec![
            bpd("fget", Positional, NoValue),
            bpd("fset", Positional, NoValue),
            bpd("fdel", Positional, NoValue),
            bpd("doc", Positional, NoValue),
        ]),
        "globals" | "locals" => BuiltinSignature::Params(Vec::new()),
        _ => BuiltinSignature::Unavailable,
    }
}

fn builtin_default(default: BuiltinDefault) -> i64 {
    match default {
        BuiltinDefault::Empty => empty_class(),
        BuiltinDefault::NoValue => NONE,
        BuiltinDefault::Int(number) => integer(number),
        BuiltinDefault::Str(content) => text(content),
        BuiltinDefault::Bool(flag) => value::bool_value(flag),
        BuiltinDefault::EmptyTuple => value::tuple_new(Vec::new()),
    }
}

fn signature_from_builtin(function: i64) -> Result<i64, i64> {
    let Some(name) = function_name(function) else {
        return Err(type_error("not a callable object"));
    };
    match builtin_signature(&name) {
        BuiltinSignature::Params(params) => {
            let parameters = value::dict_new(Vec::new());
            let mut guard = Roots::new();
            guard.keep(&[parameters]);
            for info in params {
                let default = builtin_default(info.default);
                guard.keep(&[default]);
                let parameter =
                    make_parameter(info.name, kind_index_for(info.kind), default, empty_class());
                guard.keep(&[parameter]);
                dict_insert_name(parameters, info.name, parameter);
            }
            Ok(new_signature(parameters, empty_class()))
        }
        BuiltinSignature::Unavailable => {
            let types = [
                "str",
                "int",
                "dict",
                "range",
                "type",
                "slice",
                "bytes",
                "bytearray",
                "super",
            ];
            if types.contains(&name.as_str()) {
                Err(value_error(&format!(
                    "no signature found for builtin type <class '{name}'>"
                )))
            } else {
                Err(value_error(&format!(
                    "no signature found for builtin <built-in function {name}>"
                )))
            }
        }
    }
}

/// Applies a `functools.partial` binding to a signature, following CPython's
/// `_signature_get_partial`.
fn partial_signature(signature: i64, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let mut guard = Roots::new();
    guard.keep(&[signature]);
    let bound = signature_bind_impl(signature, args, keywords, true);
    if exc::pending() {
        let _ = exc::clear();
        return value_error("partial object has incorrect arguments");
    }
    guard.keep(&[bound]);
    let arguments = bound_arguments_dict(bound);
    let infos = parameter_infos(signature);
    let mut result: Vec<(String, i64)> = infos
        .iter()
        .map(|info| (info.name.clone(), info.parameter))
        .collect();
    let mut transform_to_keyword_only = false;
    for info in &infos {
        let bound_value = dict_lookup_name(arguments, &info.name);
        let mut replaced: Option<i64> = None;
        let mut remove = false;
        if let Some(argument) = bound_value {
            match info.kind {
                POSITIONAL_ONLY => remove = true,
                POSITIONAL_OR_KEYWORD => {
                    if keywords.iter().any(|(name, _)| *name == info.name) {
                        transform_to_keyword_only = true;
                        let parameter =
                            make_parameter(&info.name, info.kind, argument, info.annotation);
                        guard.keep(&[parameter]);
                        replaced = Some(parameter);
                    } else {
                        remove = true;
                    }
                }
                KEYWORD_ONLY => {
                    let parameter =
                        make_parameter(&info.name, info.kind, argument, info.annotation);
                    guard.keep(&[parameter]);
                    replaced = Some(parameter);
                }
                _ => {}
            }
        }
        if let Some(parameter) = replaced {
            if let Some(slot) = result.iter_mut().find(|(name, _)| *name == info.name) {
                slot.1 = parameter;
            }
        }
        if remove {
            result.retain(|(name, _)| *name != info.name);
            continue;
        }
        if transform_to_keyword_only {
            match info.kind {
                POSITIONAL_OR_KEYWORD => {
                    let parameter = result
                        .iter()
                        .find(|(name, _)| *name == info.name)
                        .map(|(_, parameter)| *parameter);
                    if let Some(parameter) = parameter {
                        let replacement = make_parameter(
                            &info.name,
                            KEYWORD_ONLY,
                            parameter_field(parameter, "_default"),
                            parameter_field(parameter, "_annotation"),
                        );
                        guard.keep(&[replacement]);
                        result.retain(|(name, _)| *name != info.name);
                        result.push((info.name.clone(), replacement));
                    }
                }
                KEYWORD_ONLY | VAR_KEYWORD => {
                    if let Some(position) = result.iter().position(|(name, _)| *name == info.name) {
                        let entry = result.remove(position);
                        result.push(entry);
                    }
                }
                VAR_POSITIONAL => {
                    result.retain(|(name, _)| *name != info.name);
                }
                _ => {}
            }
        }
    }
    let parameters = value::dict_new(Vec::new());
    guard.keep(&[parameters]);
    for (name, parameter) in &result {
        guard.keep(std::slice::from_ref(parameter));
        dict_insert_name(parameters, name, *parameter);
    }
    new_signature(parameters, signature_return_annotation(signature))
}

fn partial_method_signature(instance: i64) -> Option<i64> {
    if value::instance_class_name(instance) != Some("partialmethod") {
        return None;
    }
    let function = static_attribute(instance, "func")?;
    let signature = signature_of_function(function, false)?;
    if exc::pending() {
        return None;
    }
    let mut args = vec![NONE];
    if let Some(stored) = static_attribute(instance, "args") {
        if let Some(items) = value::as_tuple(stored).or_else(|| value::as_list(stored)) {
            args.extend_from_slice(items);
        }
    }
    let keywords: Vec<(String, i64)> = static_attribute(instance, "keywords")
        .map(|dict| {
            dict_pairs(dict)
                .into_iter()
                .filter_map(|(key, item)| value::as_str(key).map(|name| (name.to_string(), item)))
                .collect()
        })
        .unwrap_or_default();
    let transformed = partial_signature(signature, &args, &keywords);
    if exc::pending() {
        return None;
    }
    let mut guard = Roots::new();
    guard.keep(&[signature, transformed]);
    let Some(first) = parameter_infos(signature).into_iter().next() else {
        return Some(transformed);
    };
    if first.kind == VAR_POSITIONAL {
        return Some(transformed);
    }
    let existing = signature_parameters(transformed);
    guard.keep(&[existing, first.parameter]);
    let parameters = value::dict_new(Vec::new());
    guard.keep(&[parameters]);
    dict_insert_name(parameters, &first.name, first.parameter);
    for (key, item) in dict_pairs(existing) {
        dict_insert_key(parameters, key, item);
    }
    Some(new_signature(
        parameters,
        signature_return_annotation(transformed),
    ))
}

fn signature_of_function(function: i64, skip_first: bool) -> Option<i64> {
    if !value::function_signature(function).is_empty() {
        return Some(signature_from_param_meta(function, skip_first));
    }
    match function_body(function) {
        Some(FunctionBody::Native { module, name }) => {
            signature_from_native_spec(module, name, skip_first).ok()
        }
        Some(FunctionBody::Builtin(_)) => signature_from_builtin(function).ok(),
        Some(FunctionBody::Partial {
            function: target,
            args,
            keywords,
        }) => {
            let target_signature = signature_of_function(*target, false)?;
            if exc::pending() {
                return None;
            }
            Some(partial_signature(target_signature, args, keywords))
        }
        Some(FunctionBody::BoundMethod { .. }) => {
            let (kind, target) = resolved_method(function)?;
            signature_of_function(target, kind != MethodKind::Staticmethod)
        }
        Some(FunctionBody::Wrapper { .. } | FunctionBody::Host { .. }) => {
            let name = function_name(function).unwrap_or_else(|| "<function>".to_string());
            type_error(&format!(
                "no signature found for function {name:?}: the runtime carries no \
                 parameter metadata for this callable"
            ));
            None
        }
        _ => {
            let name = function_name(function).unwrap_or_else(|| "<callable>".to_string());
            value_error(&format!(
                "no signature found for builtin <built-in function {name}>"
            ));
            None
        }
    }
}

fn signature_of_class(class: i64) -> i64 {
    if value::class_display_name(class) == Some("object") {
        return empty_signature();
    }
    for name in ["__init__", "__new__"] {
        let Some((kind, function)) = class_callable(class, name) else {
            continue;
        };
        if let Some(signature) = signature_of_function(function, kind != MethodKind::Staticmethod) {
            return signature;
        }
        if exc::pending() {
            return NONE;
        }
    }
    // A class without an explicit initializer accepts any arguments.
    let parameters = value::dict_new(Vec::new());
    let mut guard = Roots::new();
    guard.keep(&[parameters]);
    let varargs = make_parameter("args", VAR_POSITIONAL, empty_class(), empty_class());
    let kwargs = make_parameter("kwargs", VAR_KEYWORD, empty_class(), empty_class());
    guard.keep(&[varargs, kwargs]);
    dict_insert_name(parameters, "args", varargs);
    dict_insert_name(parameters, "kwargs", kwargs);
    new_signature(parameters, empty_class())
}

fn class_callable(class: i64, name: &str) -> Option<(MethodKind, i64)> {
    match value::class_lookup(class, name)? {
        ClassEntry::Method(kind, function) => Some((kind, function)),
        ClassEntry::Attr(item) if value::as_function(item).is_some() => {
            Some((MethodKind::Normal, item))
        }
        ClassEntry::Attr(_) => None,
    }
}

fn has_static_attribute(obj: i64, name: &str) -> bool {
    static_attribute(obj, name).is_some()
}

fn signature_of_callable(obj: i64, follow_wrapped: bool) -> i64 {
    if let Some(signature) = static_attribute(obj, "__signature__") {
        if signature != NONE {
            if !is_instance_of(signature, signature_class()) {
                return type_error(&format!(
                    "unexpected object {} in __signature__ attribute",
                    value::repr(signature)
                ));
            }
            return signature;
        }
    }
    let obj = if follow_wrapped {
        unwrap_impl(obj)
    } else {
        obj
    };
    if exc::pending() {
        return NONE;
    }
    if let Some(signature) = static_attribute(obj, "__signature__") {
        if signature != NONE {
            if !is_instance_of(signature, signature_class()) {
                return type_error(&format!(
                    "unexpected object {} in __signature__ attribute",
                    value::repr(signature)
                ));
            }
            return signature;
        }
    }
    if value::as_function(obj).is_some() {
        if let Some(signature) = signature_of_function(obj, is_method(obj)) {
            return signature;
        }
        if exc::pending() {
            return NONE;
        }
        let name = function_name(obj).unwrap_or_else(|| "<callable>".to_string());
        return value_error(&format!(
            "no signature found for builtin <built-in function {name}>"
        ));
    }
    if value::as_class(obj).is_some() {
        return signature_of_class(obj);
    }
    if let Some(signature) = partial_method_signature(obj) {
        return signature;
    }
    if exc::pending() {
        return NONE;
    }
    if let Some(class) = value::instance_class_value(obj) {
        if let Some((kind, function)) = class_callable(class, "__call__") {
            if let Some(signature) =
                signature_of_function(function, kind != MethodKind::Staticmethod)
            {
                return signature;
            }
            if exc::pending() {
                return NONE;
            }
        }
    }
    type_error(&format!("{} is not a callable object", value::repr(obj)))
}

// -------------------------------------------------------------------------
// Static attribute access
// -------------------------------------------------------------------------

fn class_dict(class: i64) -> i64 {
    let dict = value::dict_new(Vec::new());
    let mut guard = Roots::new();
    guard.keep(&[dict]);
    for (name, item) in value::class_namespace(class) {
        guard.keep(&[item]);
        dict_insert_name(dict, &name, item);
    }
    dict
}

fn function_static_attribute(obj: i64, name: &str) -> Option<i64> {
    let function = value::as_function(obj)?;
    if let FunctionBody::Partial {
        function: target,
        args,
        keywords,
    } = &function.body
    {
        match name {
            "func" => return Some(*target),
            "args" => return Some(value::tuple_new(args.clone())),
            "keywords" => {
                let pairs: Vec<(i64, i64)> = keywords
                    .iter()
                    .map(|(key, item)| (text(key), *item))
                    .collect();
                return Some(value::dict_new(pairs));
            }
            _ => {}
        }
    }
    match name {
        "__name__" => Some(value::str_alloc(value::function_name(obj))),
        "__qualname__" => Some(value::str_alloc(value::function_qualname(obj))),
        "__doc__" => Some(value::function_doc(obj)),
        "__module__" => value::function_module(obj),
        "__annotations__" => value::function_annotations(obj),
        "__dict__" => {
            if value::function_has_attributes(obj) {
                value::function_dict(obj)
            } else {
                None
            }
        }
        _ => value::function_attribute(obj, name),
    }
}

fn module_static_attribute(module: &str, name: &str) -> Option<i64> {
    let value_in = super::module_get_by_name(module, name);
    if exc::pending() {
        let _ = take_attribute_error();
        return None;
    }
    Some(value_in)
}

fn static_attribute(obj: i64, name: &str) -> Option<i64> {
    if let Some(module) = value::as_module(obj) {
        if name == "__dict__" {
            return Some(super::module_dict(module.name()));
        }
        return module_static_attribute(module.name(), name);
    }
    if value::as_function(obj).is_some() {
        return function_static_attribute(obj, name);
    }
    if let Some(class) = value::as_class(obj) {
        match name {
            "__name__" | "__qualname__" => return value::class_display_name(obj).map(text),
            "__bases__" => return Some(value::tuple_new(class_bases(obj))),
            "__base__" => return Some(class_bases(obj).first().copied().unwrap_or(NONE)),
            "__mro__" => return Some(value::tuple_new(value::class_mro(obj))),
            "__class__" => return Some(value::type_class(obj)),
            "__dict__" => return Some(class_dict(obj)),
            _ => {}
        }
        let _ = class;
        if name == "__new__" && value::class_lookup(obj, "__new__").is_none() {
            return Some(value::bound_method_new(obj, name.to_string()));
        }
        return match value::class_lookup(obj, name)? {
            ClassEntry::Attr(item) => Some(item),
            ClassEntry::Method(_, function) => Some(function),
        };
    }
    if let Some(class) = value::instance_class_value(obj) {
        match name {
            "__class__" => return Some(class),
            "__dict__" => return value::instance_dict(obj),
            _ => {}
        }
        if let Some(attribute) = value::instance_attr(obj, name) {
            return Some(attribute);
        }
        return match value::class_lookup(class, name)? {
            ClassEntry::Attr(item) => Some(item),
            ClassEntry::Method(_, function) => Some(function),
        };
    }
    None
}

// -------------------------------------------------------------------------
// Module-level functions
// -------------------------------------------------------------------------

fn expand_tabs(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut column = 0usize;
    for character in input.chars() {
        match character {
            '\t' => {
                let spaces = 8 - (column % 8);
                for _ in 0..spaces {
                    output.push(' ');
                }
                column += spaces;
            }
            '\n' | '\r' => {
                output.push(character);
                column = 0;
            }
            _ => {
                output.push(character);
                column += 1;
            }
        }
    }
    output
}

fn cleandoc_impl(doc: i64) -> i64 {
    let Some(content) = value::as_str(doc) else {
        return type_error(&format!(
            "cleandoc() argument must be str, not '{}'",
            value::type_name(doc)
        ));
    };
    let expanded = expand_tabs(content);
    let mut lines: Vec<String> = expanded.split('\n').map(str::to_string).collect();
    let mut margin = usize::MAX;
    for line in lines.iter().skip(1) {
        let content = line.trim_start_matches(' ').chars().count();
        if content > 0 {
            let indent = line.chars().count() - content;
            margin = margin.min(indent);
        }
    }
    if let Some(first) = lines.first_mut() {
        *first = first.trim_start_matches(' ').to_string();
    }
    if margin != usize::MAX {
        for line in lines.iter_mut().skip(1) {
            *line = line.chars().skip(margin).collect();
        }
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    while lines.first().is_some_and(String::is_empty) {
        lines.remove(0);
    }
    text(&lines.join("\n"))
}

fn find_class_doc(obj: i64) -> i64 {
    let Some(class) = value::as_class(obj)
        .map(|_| obj)
        .or_else(|| value::instance_class_value(obj))
    else {
        return NONE;
    };
    match value::class_lookup(class, "__doc__") {
        Some(ClassEntry::Attr(item)) => item,
        _ => NONE,
    }
}

fn getdoc_impl(obj: i64) -> i64 {
    let doc = static_attribute(obj, "__doc__").unwrap_or(NONE);
    let doc = if doc == NONE {
        find_class_doc(obj)
    } else {
        doc
    };
    if value::as_str(doc).is_none() {
        return NONE;
    }
    cleandoc_impl(doc)
}

fn module_file(module: &str) -> Option<String> {
    globals::lookup(module, "__file__")
        .and_then(value::as_str)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
}

fn getfile_impl(obj: i64) -> i64 {
    if let Some(module) = value::as_module(obj) {
        return match module_file(module.name()) {
            Some(path) => text(&path),
            None => type_error(&format!(
                "<module '{}' (built-in)> is a built-in module",
                module.name()
            )),
        };
    }
    if value::as_function(obj).is_some() {
        return type_error(
            "cannot determine the source file of a Hydro function value: \
             function values carry no code object",
        );
    }
    if value::as_class(obj).is_some() {
        return type_error(
            "cannot determine the source file of a Hydro class value: \
             class values carry no module metadata",
        );
    }
    type_error(&format!(
        "module, class, method, function, traceback, frame, or code object was expected, got {}",
        value::type_name(obj)
    ))
}

fn getsourcefile_impl(obj: i64) -> i64 {
    let file = getfile_impl(obj);
    if exc::pending() {
        return NONE;
    }
    let Some(mut path) = value::as_str(file).map(str::to_string) else {
        return NONE;
    };
    if path.ends_with(".pyc") || path.ends_with(".pyo") {
        path.truncate(path.len() - 1);
    } else if [".so", ".pyd", ".dylib", ".dll"]
        .iter()
        .any(|suffix| path.ends_with(suffix))
    {
        return NONE;
    }
    if std::path::Path::new(&path).exists() {
        text(&path)
    } else {
        NONE
    }
}

/// `inspect.getmodulename(path)`: the final path component with a recognized
/// import suffix removed, or `None` when no source/bytecode/extension suffix
/// matches. This is CPython's `basename` + `importlib.machinery.all_suffixes`
/// scan.
fn getmodulename_impl(path: i64) -> i64 {
    let Some(text) = value::as_str(path) else {
        if value::as_bytes(path).is_some() {
            // `bytes.endswith(str)` is a `TypeError` in CPython's helper.
            return type_error("endswith first arg must be bytes or a tuple of bytes, not str");
        }
        return type_error(&format!(
            "expected str, bytes or os.PathLike object, not {}",
            value::type_name(path)
        ));
    };
    // `os.path.basename` on POSIX: everything after the last separator.
    let name = text.rsplit('/').next().unwrap_or("");
    let suffixes = importlib_machinery::all_suffixes();
    if exc::pending() {
        return NONE;
    }
    let Some(items) = value::as_list(suffixes) else {
        return NONE;
    };
    for suffix in items {
        let Some(suffix) = value::as_str(*suffix) else {
            continue;
        };
        if !suffix.is_empty() && name.ends_with(suffix) {
            return value::str_alloc(name[..name.len() - suffix.len()].to_string());
        }
    }
    NONE
}

fn getmodule_impl(obj: i64) -> i64 {
    if let Some(module) = value::as_module(obj) {
        return value::module_value(module.name().to_string());
    }
    if let Some(function) = value::as_function(obj) {
        match &function.body {
            FunctionBody::Native { module, .. } => {
                return value::module_value((*module).to_string())
            }
            FunctionBody::Builtin(_) => return value::module_value("builtins".to_string()),
            FunctionBody::Partial { function, .. } => return getmodule_impl(*function),
            FunctionBody::BoundMethod { .. } => {
                if let Some((_, target)) = resolved_method(obj) {
                    return getmodule_impl(target);
                }
            }
            _ => {}
        }
    }
    NONE
}

fn unwrap_impl(mut obj: i64) -> i64 {
    let mut visited: Vec<i64> = Vec::new();
    loop {
        if visited.contains(&obj) {
            return obj;
        }
        visited.push(obj);
        if has_static_attribute(obj, "__signature__") {
            return obj;
        }
        match static_attribute(obj, "__wrapped__") {
            Some(next) if next != NONE => obj = next,
            _ => return obj,
        }
    }
}

fn unwrap_with_stop(obj: i64, stop: i64) -> i64 {
    let mut current = obj;
    let mut visited: Vec<i64> = Vec::new();
    loop {
        if visited.contains(&current) {
            return current;
        }
        visited.push(current);
        if stop != NONE {
            let keep = crate::builtins::call(stop, 1, [current].as_ptr());
            if exc::pending() {
                return NONE;
            }
            if value::truthy(keep) {
                return current;
            }
        }
        match static_attribute(current, "__wrapped__") {
            Some(next) if next != NONE => current = next,
            _ => return current,
        }
    }
}

fn getmembers_impl(obj: i64, predicate: i64) -> i64 {
    let mut names: Vec<String> = Vec::new();
    if let Some(module) = value::as_module(obj) {
        names.extend(
            [
                "__name__",
                "__doc__",
                "__dict__",
                "__package__",
                "__spec__",
                "__loader__",
            ]
            .map(str::to_string),
        );
        if let Some(spec) = hydro_ir::stdlib::module(module.name()) {
            names.extend(
                spec.functions
                    .iter()
                    .map(|function| function.name.to_string()),
            );
            names.extend(
                spec.constants
                    .iter()
                    .map(|constant| constant.name.to_string()),
            );
        }
        names.extend(globals::names(module.name()));
    } else if value::as_class(obj).is_some() {
        names.extend(
            [
                "__name__",
                "__qualname__",
                "__bases__",
                "__base__",
                "__mro__",
                "__class__",
                "__dict__",
            ]
            .map(str::to_string),
        );
        for ancestor in value::class_mro(obj) {
            names.extend(
                value::class_namespace(ancestor)
                    .into_iter()
                    .map(|(name, _)| name),
            );
        }
    } else if let Some(class) = value::instance_class_value(obj) {
        names.extend(["__class__".to_string(), "__dict__".to_string()]);
        if let Some(attributes) = value::instance_attributes(obj) {
            names.extend(attributes.snapshot().into_iter().map(|(name, _)| name));
        }
        for ancestor in value::class_mro(class) {
            names.extend(
                value::class_namespace(ancestor)
                    .into_iter()
                    .map(|(name, _)| name),
            );
        }
    } else if value::as_function(obj).is_some() {
        names.extend(["__name__", "__qualname__", "__doc__"].map(str::to_string));
        if value::function_has_attributes(obj) {
            names.push("__dict__".to_string());
            names.extend(value::function_attribute_names(obj));
        }
        if value::function_module(obj).is_some() {
            names.push("__module__".to_string());
        }
        if value::function_annotations(obj).is_some() {
            names.push("__annotations__".to_string());
        }
        if matches!(function_body(obj), Some(FunctionBody::Partial { .. })) {
            names.extend(["func", "args", "keywords"].map(str::to_string));
        }
    }
    names.sort();
    names.dedup();
    let result = value::list_new(Vec::new());
    let mut guard = Roots::new();
    guard.keep(&[result]);
    for name in names {
        let attribute = match static_attribute(obj, &name) {
            Some(attribute) => attribute,
            None => {
                if exc::pending() {
                    return NONE;
                }
                continue;
            }
        };
        guard.keep(&[attribute]);
        if predicate != NONE {
            let keep = crate::builtins::call(predicate, 1, [attribute].as_ptr());
            if exc::pending() {
                return NONE;
            }
            if !value::truthy(keep) {
                continue;
            }
        }
        let name_value = text(&name);
        guard.keep(&[name_value]);
        let pair = value::tuple_new(vec![name_value, attribute]);
        guard.keep(&[pair]);
        container::list_append(result, pair);
    }
    result
}

/// The `Parameter.empty` sentinel belonging to `parameter`'s own class.
///
/// CPython compares `parameter.default is Parameter.empty`, so the right
/// reference is the sentinel of the class that created the instance. The
/// module-level sentinel is invalidated when the heap generation changes, and
/// rebuilding it would otherwise make the stored (older) sentinel of an
/// existing parameter look like a real default value.
fn parameter_empty(parameter: i64) -> i64 {
    value::instance_class_value(parameter)
        .and_then(|class| match value::class_lookup(class, "empty") {
            Some(ClassEntry::Attr(empty)) => Some(empty),
            _ => None,
        })
        .unwrap_or_else(empty_class)
}

fn parameter_has_default(parameter: i64) -> bool {
    parameter_field(parameter, "_default") != parameter_empty(parameter)
}

fn getfullargspec_impl(obj: i64) -> i64 {
    // CPython's `getfullargspec` reports the implicit receiver of a bound
    // method, so resolve it to the underlying function.
    let obj = resolved_method(obj)
        .map(|(_, target)| target)
        .unwrap_or(obj);
    let signature = if value::as_class(obj).is_some() {
        signature_of_class(obj)
    } else if value::as_function(obj).is_some() {
        match signature_of_function(obj, false) {
            Some(signature) => signature,
            None => {
                if exc::pending() {
                    return NONE;
                }
                return type_error(&format!("{} is not a callable object", value::repr(obj)));
            }
        }
    } else {
        return type_error(&format!("{} is not a callable object", value::repr(obj)));
    };
    if exc::pending() {
        return NONE;
    }
    let mut guard = Roots::new();
    guard.keep(&[signature]);
    let infos = parameter_infos(signature);
    let mut args: Vec<i64> = Vec::new();
    let mut positional: Vec<&ParameterInfo> = Vec::new();
    let mut defaults: Vec<i64> = Vec::new();
    let mut kwonlyargs: Vec<i64> = Vec::new();
    let mut kwonlydefaults: Vec<(i64, i64)> = Vec::new();
    let mut varargs = NONE;
    let mut varkw = NONE;
    for info in &infos {
        match info.kind {
            POSITIONAL_ONLY | POSITIONAL_OR_KEYWORD => {
                let name = text(&info.name);
                guard.keep(&[name]);
                args.push(name);
                positional.push(info);
            }
            VAR_POSITIONAL => {
                varargs = text(&info.name);
                guard.keep(&[varargs]);
            }
            KEYWORD_ONLY => {
                let name = text(&info.name);
                guard.keep(&[name]);
                kwonlyargs.push(name);
                if parameter_has_default(info.parameter) {
                    let key = text(&info.name);
                    guard.keep(&[key, info.default]);
                    kwonlydefaults.push((key, info.default));
                }
            }
            VAR_KEYWORD => {
                varkw = text(&info.name);
                guard.keep(&[varkw]);
            }
            _ => {}
        }
    }
    // `defaults` holds the values of the trailing positional parameters and
    // never the keyword-only ones (those live in `kwonlydefaults`).
    let mut first_default = positional.len();
    while first_default > 0 && parameter_has_default(positional[first_default - 1].parameter) {
        first_default -= 1;
    }
    for info in &positional[first_default..] {
        guard.keep(&[info.default]);
        defaults.push(info.default);
    }
    let args_list = value::list_new(args);
    guard.keep(&[args_list]);
    let kwonly_list = value::list_new(kwonlyargs);
    guard.keep(&[kwonly_list]);
    let defaults_tuple = value::tuple_new(defaults);
    guard.keep(&[defaults_tuple]);
    let kwonlydefaults_value = if kwonlydefaults.is_empty() {
        NONE
    } else {
        value::dict_new(kwonlydefaults)
    };
    guard.keep(&[kwonlydefaults_value]);
    let annotations = value::dict_new(Vec::new());
    guard.keep(&[annotations, varargs, varkw]);
    namedtuple_instance(
        full_arg_spec_class(),
        &[
            ("args", args_list),
            ("varargs", varargs),
            ("varkw", varkw),
            ("defaults", defaults_tuple),
            ("kwonlyargs", kwonly_list),
            ("kwonlydefaults", kwonlydefaults_value),
            ("annotations", annotations),
        ],
    )
}

fn getcallargs_impl(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let Some(&function) = args.first() else {
        return type_error("getcallargs() missing required argument 'func'");
    };
    let signature = signature_of_callable(function, true);
    if exc::pending() {
        return NONE;
    }
    let bound = signature_bind_impl(signature, args.get(1..).unwrap_or(&[]), keywords, false);
    if exc::pending() {
        return NONE;
    }
    bound_arguments_apply_defaults(bound);
    if exc::pending() {
        return NONE;
    }
    bound_arguments_dict(bound)
}

// -------------------------------------------------------------------------
// Public dispatch
// -------------------------------------------------------------------------

pub(crate) fn constant(name: &str) -> i64 {
    let code_flag = match name {
        "CO_OPTIMIZED" => Some(0x1),
        "CO_NEWLOCALS" => Some(0x2),
        "CO_VARARGS" => Some(0x4),
        "CO_VARKEYWORDS" => Some(0x8),
        "CO_NESTED" => Some(0x10),
        "CO_GENERATOR" => Some(0x20),
        "CO_NOFREE" => Some(0x40),
        "CO_COROUTINE" => Some(0x80),
        "CO_ITERABLE_COROUTINE" => Some(0x100),
        "CO_ASYNC_GENERATOR" => Some(0x200),
        "CO_HAS_DOCSTRING" => Some(0x4000000),
        "CO_METHOD" => Some(0x8000000),
        _ => None,
    };
    if let Some(flag) = code_flag {
        return integer(flag);
    }
    match name {
        "empty" | "_empty" => empty_class(),
        "_POSITIONAL_ONLY" => kind_member(POSITIONAL_ONLY).unwrap_or(NONE),
        "_POSITIONAL_OR_KEYWORD" => kind_member(POSITIONAL_OR_KEYWORD).unwrap_or(NONE),
        "_VAR_POSITIONAL" => kind_member(VAR_POSITIONAL).unwrap_or(NONE),
        "_KEYWORD_ONLY" => kind_member(KEYWORD_ONLY).unwrap_or(NONE),
        "_VAR_KEYWORD" => kind_member(VAR_KEYWORD).unwrap_or(NONE),
        _ => class_value("inspect", name).unwrap_or_else(|| {
            exc::raise_with_message(
                ATTRIBUTE_ERROR,
                &format!("module 'inspect' has no attribute '{name}'"),
            )
        }),
    }
}

pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    // Arguments normally arrive from a rooted generated frame; rooting them
    // here also covers direct runtime tests and keeps them alive across the
    // allocations performed while building signatures/return values.
    let mut guard = Roots::new();
    guard.keep(args);
    match function {
        "ismodule" => value::bool_value(value::as_module(args[0]).is_some()),
        "isclass" => value::bool_value(value::as_class(args[0]).is_some()),
        "isfunction" => value::bool_value(is_user_function(args[0])),
        "ismethod" => value::bool_value(is_method(args[0])),
        "isbuiltin" => value::bool_value(is_builtin(args[0])),
        "isroutine" => value::bool_value(
            is_method(args[0])
                || is_user_function(args[0])
                || is_builtin(args[0])
                || object_is_method_descriptor(args[0]),
        ),
        "isgeneratorfunction" => value::bool_value(is_generator_function(args[0])),
        "iscoroutinefunction" => value::bool_value(is_coroutine_function(args[0])),
        "markcoroutinefunction" => mark_coroutine_function(args[0]),
        "isasyncgenfunction" => value::bool_value(is_async_generator_function(args[0])),
        "isgenerator" => {
            value::bool_value(fiber_kind_of(args[0]) == Some(crate::fiber::FiberKind::Generator))
        }
        "iscoroutine" => {
            value::bool_value(fiber_kind_of(args[0]) == Some(crate::fiber::FiberKind::Coroutine))
        }
        "isasyncgen" => value::bool_value(
            fiber_kind_of(args[0]) == Some(crate::fiber::FiberKind::AsyncGenerator),
        ),
        "isawaitable" => value::bool_value(is_awaitable(args[0])),
        // Hydro has no frame, code or traceback objects.
        "isframe" | "iscode" => FALSE,
        "ismethoddescriptor" => value::bool_value(object_is_method_descriptor(args[0])),
        "isdatadescriptor" => value::bool_value(object_is_data_descriptor(args[0])),
        "ismemberdescriptor" | "isgetsetdescriptor" => FALSE,
        "isabstract" => value::bool_value(is_abstract_class(args[0])),
        "cleandoc" => cleandoc_impl(args[0]),
        "getdoc" => getdoc_impl(args[0]),
        "getmodule" => getmodule_impl(args[0]),
        "getmodulename" => getmodulename_impl(args.first().copied().unwrap_or(NONE)),
        "getfile" => getfile_impl(args[0]),
        "getsourcefile" => getsourcefile_impl(args[0]),
        "findsource" | "getsourcelines" => value::tuple_new(vec![
            value::list_new(Vec::new()),
            value::encode_int(0).unwrap_or(NONE),
        ]),
        "getblock" => args.first().copied().unwrap_or(NONE),
        "unwrap" => {
            let stop = args.get(1).copied().unwrap_or(NONE);
            unwrap_with_stop(args[0], stop)
        }
        "signature" => {
            let follow_wrapped = match args.get(1).copied() {
                Some(flag) if flag != NONE => value::truthy(flag),
                _ => true,
            };
            signature_of_callable(args[0], follow_wrapped)
        }
        "getfullargspec" => getfullargspec_impl(args[0]),
        "getcallargs" => getcallargs_impl(args, &[]),
        "getmembers" => getmembers_impl(args[0], args.get(1).copied().unwrap_or(NONE)),
        "getattr_static" => {
            let name = value::as_str(args[1]).unwrap_or("");
            match static_attribute(args[0], name) {
                Some(item) => item,
                None => {
                    if exc::pending() {
                        return NONE;
                    }
                    match args.get(2).copied() {
                        Some(default) if !is_empty(default) => default,
                        _ => attribute_error(args[0], name),
                    }
                }
            }
        }
        "currentframe" => crate::modules::sys::current_frame(),
        "stack" => value::list_new(Vec::new()),
        "getclosurevars" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "getclosurevars() is not supported: Hydro function values carry no code \
             object, so free, global and builtin name references cannot be enumerated",
        ),
        // Native methods reached through the raw native-call path (for example
        // `str()`/`repr()` invoke the dunder with just the receiver).
        "_ParameterKind.__str__" => kind_str_raw(args[0]),
        "_ParameterKind.__repr__" => kind_repr_raw(args[0]),
        "_ParameterKind.__eq__" => kind_eq_raw(args[0], args[1]),
        "_ParameterKind.__hash__" => kind_hash_raw(args[0]),
        "Parameter.__init__" => parameter_init_impl(args),
        "Parameter.replace" => parameter_replace_impl(args),
        "Parameter.__str__" => parameter_str_raw(args[0]),
        "Parameter.__repr__" => parameter_repr_raw(args[0]),
        "Parameter.__eq__" => parameter_eq_raw(args[0], args[1]),
        "Parameter.__hash__" => parameter_hash_raw(args[0]),
        "Parameter.name" => parameter_field(args[0], "_name"),
        "Parameter.default" => parameter_field(args[0], "_default"),
        "Parameter.annotation" => parameter_field(args[0], "_annotation"),
        "Parameter.kind" => parameter_field(args[0], "_kind"),
        "Signature.__init__" => signature_init_impl(args),
        "Signature.bind" => signature_bind_raw(args, false),
        "Signature.bind_partial" => signature_bind_raw(args, true),
        "Signature.replace" => signature_replace_impl(args),
        "Signature.format" => signature_format_raw(
            args[0],
            args.get(1).copied().unwrap_or(NONE),
            args.get(2).copied().map(value::truthy).unwrap_or(true),
        ),
        "Signature.__str__" => signature_str_raw(args[0]),
        "Signature.__repr__" => signature_repr_raw(args[0]),
        "Signature.__eq__" => signature_eq_raw(args[0], args[1]),
        "Signature.parameters" => dict_copy(signature_parameters(args[0])),
        "Signature.return_annotation" => signature_return_annotation(args[0]),
        "BoundArguments.__init__" => bound_arguments_init_impl(args),
        "BoundArguments.apply_defaults" => bound_arguments_apply_defaults(args[0]),
        "BoundArguments.__repr__" => bound_arguments_repr(args[0]),
        "BoundArguments.signature" => bound_signature(args[0]),
        "BoundArguments.args" => bound_arguments_args(args[0]),
        "BoundArguments.kwargs" => bound_arguments_kwargs(args[0]),
        "FullArgSpec.__iter__" | "FrameInfo.__iter__" | "ClosureVars.__iter__" => {
            namedtuple_iter_raw(args[0])
        }
        "FullArgSpec.__len__" | "FrameInfo.__len__" | "ClosureVars.__len__" => {
            integer(namedtuple_values_of(args[0]).len() as i64)
        }
        "FullArgSpec.__getitem__" | "FrameInfo.__getitem__" | "ClosureVars.__getitem__" => {
            namedtuple_getitem(args[0], args[1])
        }
        "FullArgSpec.__repr__" | "FrameInfo.__repr__" | "ClosureVars.__repr__" => {
            namedtuple_repr_raw(args[0])
        }
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'inspect' has no attribute '{function}'"),
        ),
    }
}

pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let mut guard = Roots::new();
    guard.keep(args);
    for (_, item) in keywords {
        guard.keep(std::slice::from_ref(item));
    }
    if function == "getcallargs" {
        return getcallargs_impl(args, keywords);
    }
    type_error(&format!(
        "inspect.{function}() does not accept keyword arguments"
    ))
}

// -------------------------------------------------------------------------
// Native method specs (resolved by the raw native-call path)
// -------------------------------------------------------------------------

const fn method_spec(name: &'static str, min: usize, max: usize) -> hydro_ir::stdlib::FunctionSpec {
    hydro_ir::stdlib::FunctionSpec {
        name,
        min_args: min,
        max_args: max,
        ret: hydro_ir::Type::Any,
        params: &[],
        dynamic_keywords: false,
    }
}

static METHOD_SPECS: &[hydro_ir::stdlib::FunctionSpec] = &[
    method_spec("_ParameterKind.__str__", 1, 1),
    method_spec("_ParameterKind.__repr__", 1, 1),
    method_spec("_ParameterKind.__eq__", 2, 2),
    method_spec("_ParameterKind.__hash__", 1, 1),
    method_spec("Parameter.__init__", 3, 5),
    method_spec("Parameter.replace", 1, 5),
    method_spec("Parameter.__str__", 1, 1),
    method_spec("Parameter.__repr__", 1, 1),
    method_spec("Parameter.__eq__", 2, 2),
    method_spec("Parameter.__hash__", 1, 1),
    method_spec("Parameter.name", 1, 1),
    method_spec("Parameter.default", 1, 1),
    method_spec("Parameter.annotation", 1, 1),
    method_spec("Parameter.kind", 1, 1),
    method_spec("Signature.__init__", 2, 3),
    method_spec("Signature.bind", 1, usize::MAX),
    method_spec("Signature.bind_partial", 1, usize::MAX),
    method_spec("Signature.replace", 1, 3),
    method_spec("Signature.format", 1, 3),
    method_spec("Signature.__str__", 1, 1),
    method_spec("Signature.__repr__", 1, 1),
    method_spec("Signature.__eq__", 2, 2),
    method_spec("Signature.parameters", 1, 1),
    method_spec("Signature.return_annotation", 1, 1),
    method_spec("BoundArguments.__init__", 3, 3),
    method_spec("BoundArguments.apply_defaults", 1, 1),
    method_spec("BoundArguments.__repr__", 1, 1),
    method_spec("BoundArguments.signature", 1, 1),
    method_spec("BoundArguments.args", 1, 1),
    method_spec("BoundArguments.kwargs", 1, 1),
    method_spec("FullArgSpec.__iter__", 1, 1),
    method_spec("FullArgSpec.__len__", 1, 1),
    method_spec("FullArgSpec.__getitem__", 2, 2),
    method_spec("FullArgSpec.__repr__", 1, 1),
    method_spec("FrameInfo.__iter__", 1, 1),
    method_spec("FrameInfo.__len__", 1, 1),
    method_spec("FrameInfo.__getitem__", 2, 2),
    method_spec("FrameInfo.__repr__", 1, 1),
    method_spec("ClosureVars.__iter__", 1, 1),
    method_spec("ClosureVars.__len__", 1, 1),
    method_spec("ClosureVars.__getitem__", 2, 2),
    method_spec("ClosureVars.__repr__", 1, 1),
];

pub(crate) fn function_spec(name: &str) -> Option<&'static hydro_ir::stdlib::FunctionSpec> {
    METHOD_SPECS.iter().find(|spec| spec.name == name)
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::methods;

    fn number(value_in: i64) -> i64 {
        value::as_integer(value_in).expect("integer")
    }

    fn string(value_in: i64) -> String {
        value::as_str(value_in).expect("string").to_string()
    }

    fn attr(obj: i64, name: &str) -> i64 {
        let result = methods::getattr(obj, name.as_ptr(), name.len());
        assert!(!exc::pending(), "attribute {name} raised");
        result
    }

    fn user_function(name: &str, params: Vec<ParamMeta>, flags: u32) -> i64 {
        let arity = params
            .iter()
            .filter(|param| param.kind.is_positional())
            .count();
        let function = value::function_new_with_flags(1, arity, name.to_string(), flags);
        value::function_set_signature(function, params);
        function
    }

    #[test]
    fn getmodulename_strips_import_suffixes() {
        exc::init();
        let cases = [
            ("/a/b/mod.py", Some("mod")),
            ("mod.py", Some("mod")),
            ("/a/b/mod.pyc", Some("mod")),
            ("dir/mod.abi3.so", Some("mod")),
            ("mod.txt", None),
            ("", None),
            ("dir/", None),
        ];
        for (path, expected) in cases {
            let argument = value::str_alloc(path.to_string());
            let result = call("getmodulename", &[argument]);
            assert!(!exc::pending(), "{path} raised");
            assert_eq!(value::as_str(result), expected, "{path}");
        }
        // The result is a plain `str`, and a real extension suffix (`.so`) is
        // recognized too.
        assert_eq!(
            value::as_str(call(
                "getmodulename",
                &[value::str_alloc("x.so".to_string())]
            )),
            Some("x")
        );
        exc::init();
        let _ = call("getmodulename", &[value::encode_int(3).unwrap_or(NONE)]);
        assert_eq!(exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        assert!(value::to_string(exc::pending_value())
            .contains("expected str, bytes or os.PathLike object, not int"));
        exc::clear();
    }

    fn signature_of(obj: i64) -> i64 {
        let result = call("signature", &[obj]);
        assert!(!exc::pending(), "signature() raised");
        result
    }

    fn parameter_names(signature: i64) -> Vec<String> {
        dict_pairs(attr(signature, "parameters"))
            .into_iter()
            .map(|(key, _)| string(key))
            .collect()
    }

    fn parameter_at(signature: i64, name: &str) -> i64 {
        dict_lookup_name(attr(signature, "parameters"), name).expect("parameter")
    }

    #[test]
    fn cleandoc_dedents_and_trims() {
        exc::init();
        assert_eq!(
            string(call("cleandoc", &[text("  hello\n    world\n\n")])),
            "hello\nworld"
        );
        assert_eq!(
            string(call("cleandoc", &[text("\n\n  a\n\tb\n")])),
            "a\n      b"
        );
        let error = call("cleandoc", &[integer(1)]);
        assert_eq!(error, NONE);
        assert!(exc::pending());
        let _ = exc::clear();
    }

    #[test]
    fn user_function_signature_has_kinds_and_defaults() {
        exc::init();
        let function = user_function(
            "f",
            vec![
                param("a", ParamKind::PosOnly, None),
                param("b", ParamKind::Positional, Some(integer(2))),
                param("args", ParamKind::VarArg, None),
                param("c", ParamKind::KwOnly, Some(integer(5))),
                param("kw", ParamKind::KwArg, None),
            ],
            0,
        );
        let signature = signature_of(function);
        assert_eq!(
            parameter_names(signature),
            vec!["a", "b", "args", "c", "kw"]
        );
        let a = parameter_at(signature, "a");
        assert_eq!(string(attr(a, "name")), "a");
        assert_eq!(number(attr(attr(a, "kind"), "value")), POSITIONAL_ONLY);
        assert_eq!(attr(a, "default"), empty_class());
        let b = parameter_at(signature, "b");
        assert_eq!(number(attr(b, "default")), 2);
        let c = parameter_at(signature, "c");
        assert_eq!(number(attr(attr(c, "kind"), "value")), KEYWORD_ONLY);
        assert_eq!(number(attr(c, "default")), 5);
        assert_eq!(value::to_string(signature), "(a, /, b=2, *args, c=5, **kw)");
    }

    #[test]
    fn parameter_constructor_validates_and_exposes_fields() {
        exc::init();
        let kind = kind_member(POSITIONAL_OR_KEYWORD).expect("kind");
        // CPython keeps `default` and `annotation` keyword-only:
        // `Parameter(name, kind, *, default=..., annotation=...)`.
        let parameter = crate::builtins::call_kw(
            parameter_class(),
            2,
            [text("value"), kind].as_ptr(),
            2,
            [
                text("default"),
                integer(3),
                text("annotation"),
                empty_class(),
            ]
            .as_ptr(),
        );
        assert!(!exc::pending());
        assert_eq!(string(attr(parameter, "name")), "value");
        assert_eq!(attr(parameter, "kind"), kind);
        assert_eq!(number(attr(parameter, "default")), 3);
        assert_eq!(attr(parameter, "annotation"), empty_class());

        let too_many = crate::builtins::call(
            parameter_class(),
            4,
            [text("value"), kind, integer(3), empty_class()].as_ptr(),
        );
        assert_eq!(too_many, NONE);
        assert!(exc::pending());
        let _ = exc::clear();

        let invalid = crate::builtins::call(parameter_class(), 2, [text("1bad"), kind].as_ptr());
        assert_eq!(invalid, NONE);
        assert!(exc::pending());
        let _ = exc::clear();

        let renamed = methods::call(parameter, b"replace".as_ptr(), 7, 0, std::ptr::null());
        assert!(!exc::pending());
        assert_eq!(string(attr(renamed, "name")), "value");
    }

    #[test]
    fn parameter_constructor_stores_the_kind_member() {
        exc::init();
        // `Parameter("x", Parameter.KEYWORD_ONLY).kind is Parameter.KEYWORD_ONLY`
        let keyword_only = kind_member(KEYWORD_ONLY).expect("kind");
        let parameter =
            crate::builtins::call(parameter_class(), 2, [text("x"), keyword_only].as_ptr());
        assert!(!exc::pending());
        let kind = attr(parameter, "kind");
        assert!(is_instance_of(kind, parameter_kind_class()));
        assert_eq!(kind, keyword_only);
        assert_eq!(number(attr(kind, "value")), KEYWORD_ONLY);
        // The module-level constant and the stored field are the same member.
        assert_eq!(attr(parameter_class(), "KEYWORD_ONLY"), kind);
        // `parameter_kind_index` reads the stored member, not a raw integer.
        assert_eq!(parameter_kind_index(parameter), KEYWORD_ONLY);

        // Integer kinds are coerced to their member, as CPython does.
        let from_integer = crate::builtins::call(
            parameter_class(),
            2,
            [text("y"), integer(VAR_POSITIONAL)].as_ptr(),
        );
        assert!(!exc::pending());
        assert_eq!(
            attr(from_integer, "kind"),
            kind_member(VAR_POSITIONAL).expect("kind")
        );

        // Out-of-range integers are still rejected.
        let invalid = crate::builtins::call(
            parameter_class(),
            2,
            [text("z"), integer(KIND_DESCRIPTIONS.len() as i64)].as_ptr(),
        );
        assert_eq!(invalid, NONE);
        assert!(exc::pending());
        let _ = exc::clear();

        // Variadic kinds reject defaults; this check only works when the
        // constructor compares against the right kind index.
        let with_default = crate::builtins::call_kw(
            parameter_class(),
            2,
            [text("args"), kind_member(VAR_KEYWORD).expect("kind")].as_ptr(),
            2,
            [text("default"), integer(1)].as_ptr(),
        );
        assert_eq!(with_default, NONE);
        assert!(exc::pending());
        let _ = exc::clear();

        // `replace` re-runs the constructor and keeps the stored kind a member.
        let replaced = methods::call(from_integer, b"replace".as_ptr(), 7, 0, std::ptr::null());
        assert!(!exc::pending());
        assert_eq!(
            attr(replaced, "kind"),
            kind_member(VAR_POSITIONAL).expect("kind")
        );
        assert_eq!(parameter_kind_index(replaced), VAR_POSITIONAL);
    }

    #[test]
    fn module_kind_constants_resolve_kind_members() {
        exc::init();
        // Never reset the heap here (or anywhere else in a unit test): the
        // heap, its cached classes and the `inspect` registry are shared with
        // every concurrently running test, so a process-global reset would
        // invalidate class pointers another test is still using.

        // Reading the module attribute must materialize the kind class and its
        // member instead of returning `None`; both are created lazily on first
        // use and then shared for the lifetime of the heap.
        let kind = constant("_POSITIONAL_OR_KEYWORD");
        assert_ne!(kind, NONE);
        assert!(is_instance_of(kind, parameter_kind_class()));
        assert_eq!(string(attr(kind, "name")), "POSITIONAL_OR_KEYWORD");
        assert_eq!(number(attr(kind, "value")), POSITIONAL_OR_KEYWORD);
        assert_eq!(string(attr(kind, "description")), "positional or keyword");

        for (index, (name, description)) in KIND_DESCRIPTIONS.iter().enumerate() {
            let member = constant(&format!("_{name}"));
            assert!(is_instance_of(member, parameter_kind_class()), "{name}");
            assert_eq!(number(attr(member, "value")), index as i64, "{name}");
            assert_eq!(string(attr(member, "description")), *description, "{name}");
            // The lazy accessor, the module constant and the class attribute
            // all resolve to the same cached member.
            assert_eq!(kind_member(index as i64), Some(member), "{name}");
            assert_eq!(attr(parameter_kind_class(), name), member, "{name}");
            assert_eq!(constant(&format!("_{name}")), member, "{name}");
        }

        // Out-of-range indices stay absent rather than materializing anything.
        assert_eq!(kind_member(-1), None);
        assert_eq!(kind_member(KIND_DESCRIPTIONS.len() as i64), None);
    }

    #[test]
    fn signature_bind_maps_arguments_and_defaults() {
        exc::init();
        let function = user_function(
            "f",
            vec![
                param("a", ParamKind::Positional, None),
                param("b", ParamKind::Positional, None),
                param("c", ParamKind::Positional, Some(integer(3))),
            ],
            0,
        );
        let signature = signature_of(function);
        let bound = methods::call(
            signature,
            b"bind".as_ptr(),
            4,
            2,
            [integer(1), integer(2)].as_ptr(),
        );
        assert!(!exc::pending());
        let arguments = attr(bound, "arguments");
        assert_eq!(number(dict_lookup_name(arguments, "a").expect("a")), 1);
        assert_eq!(number(dict_lookup_name(arguments, "b").expect("b")), 2);
        assert!(dict_lookup_name(arguments, "c").is_none());

        let defaults = methods::call(bound, b"apply_defaults".as_ptr(), 14, 0, std::ptr::null());
        assert!(!exc::pending());
        assert_eq!(defaults, NONE);
        assert_eq!(
            number(dict_lookup_name(attr(bound, "arguments"), "c").expect("c")),
            3
        );
    }

    #[test]
    fn class_signature_uses_init_and_strips_self() {
        exc::init();
        let init = user_function(
            "__init__",
            vec![
                param("self", ParamKind::Positional, None),
                param("x", ParamKind::Positional, None),
                param("y", ParamKind::Positional, Some(integer(7))),
            ],
            0,
        );
        let mut guard = Roots::new();
        guard.keep(&[init]);
        let class = value::class_new_full(
            "C".to_string(),
            NONE,
            vec![("__init__".to_string(), init, MethodKind::Normal)],
            Vec::new(),
        );
        let signature = signature_of(class);
        assert_eq!(parameter_names(signature), vec!["x", "y"]);
        assert_eq!(value::to_string(signature), "(x, y=7)");
    }

    #[test]
    fn native_function_and_builtin_signatures() {
        exc::init();
        let sqrt = value::native_function_new("math", "sqrt", 1);
        let signature = signature_of(sqrt);
        assert_eq!(parameter_names(signature), vec!["arg1"]);
        assert_eq!(
            number(attr(attr(parameter_at(signature, "arg1"), "kind"), "value")),
            POSITIONAL_ONLY
        );

        let len = value::builtin_function_new(hydro_ir::Builtin::Len);
        let signature = signature_of(len);
        assert_eq!(parameter_names(signature), vec!["obj"]);
        assert_eq!(
            number(attr(attr(parameter_at(signature, "obj"), "kind"), "value")),
            POSITIONAL_ONLY
        );

        let int = value::builtin_function_new(hydro_ir::Builtin::Int);
        let error = call("signature", &[int]);
        assert_eq!(error, NONE);
        assert!(exc::pending());
        let _ = exc::clear();
    }

    #[test]
    fn partial_signature_removes_bound_arguments() {
        exc::init();
        let function = user_function(
            "f",
            vec![
                param("a", ParamKind::Positional, None),
                param("b", ParamKind::Positional, None),
                param("c", ParamKind::Positional, Some(integer(3))),
            ],
            0,
        );
        let partial = value::partial_new(function, vec![integer(1)]);
        let signature = signature_of(partial);
        assert_eq!(parameter_names(signature), vec!["b", "c"]);
        assert_eq!(number(attr(parameter_at(signature, "c"), "default")), 3);

        let keyword_partial = value::partial_new_with_keywords(
            function,
            Vec::new(),
            vec![("a".to_string(), integer(4))],
        );
        let signature = signature_of(keyword_partial);
        assert_eq!(parameter_names(signature), vec!["a", "b", "c"]);
        assert_eq!(
            number(attr(attr(parameter_at(signature, "a"), "kind"), "value")),
            KEYWORD_ONLY
        );
        assert_eq!(number(attr(parameter_at(signature, "a"), "default")), 4);
    }

    #[test]
    fn getfullargspec_reports_namedtuple_fields() {
        exc::init();
        let function = user_function(
            "f",
            vec![
                param("a", ParamKind::PosOnly, None),
                param("b", ParamKind::Positional, Some(integer(2))),
                param("args", ParamKind::VarArg, None),
                param("c", ParamKind::KwOnly, Some(integer(5))),
                param("kw", ParamKind::KwArg, None),
            ],
            0,
        );
        let specification = call("getfullargspec", &[function]);
        assert!(!exc::pending());
        let args: Vec<String> = value::as_list(attr(specification, "args"))
            .expect("args list")
            .iter()
            .map(|item| string(*item))
            .collect();
        assert_eq!(args, vec!["a", "b"]);
        assert_eq!(string(attr(specification, "varargs")), "args");
        assert_eq!(string(attr(specification, "varkw")), "kw");
        let defaults = value::as_tuple(attr(specification, "defaults")).expect("defaults");
        assert_eq!(defaults.len(), 1);
        assert_eq!(number(defaults[0]), 2);
        let kwonly: Vec<String> = value::as_list(attr(specification, "kwonlyargs"))
            .expect("kwonlyargs")
            .iter()
            .map(|item| string(*item))
            .collect();
        assert_eq!(kwonly, vec!["c"]);
        let kwdefaults = attr(specification, "kwonlydefaults");
        assert_eq!(number(dict_lookup_name(kwdefaults, "c").expect("c")), 5);
        let length = methods::call(specification, b"__len__".as_ptr(), 7, 0, std::ptr::null());
        assert_eq!(number(length), 7);
        assert_eq!(
            namedtuple_getitem(specification, integer(1)),
            attr(specification, "varargs")
        );
        assert_eq!(
            namedtuple_getitem(specification, integer(0)),
            attr(specification, "args")
        );
    }

    #[test]
    fn getfullargspec_collects_trailing_positional_defaults() {
        exc::init();
        let function = user_function(
            "f",
            vec![
                param("a", ParamKind::Positional, None),
                param("b", ParamKind::Positional, Some(integer(2))),
                param("c", ParamKind::Positional, Some(integer(3))),
                param("d", ParamKind::KwOnly, Some(integer(4))),
            ],
            0,
        );
        let specification = call("getfullargspec", &[function]);
        assert!(!exc::pending());
        let defaults = value::as_tuple(attr(specification, "defaults")).expect("defaults");
        assert_eq!(defaults.len(), 2);
        assert_eq!(number(defaults[0]), 2);
        assert_eq!(number(defaults[1]), 3);
        // The keyword-only default belongs to `kwonlydefaults`, never to
        // `defaults`.
        let kwdefaults = attr(specification, "kwonlydefaults");
        assert_eq!(number(dict_lookup_name(kwdefaults, "d").expect("d")), 4);
        assert!(!defaults.iter().any(|value_in| number(*value_in) == 4));
    }

    #[test]
    fn getfullargspec_reports_empty_defaults_without_positional_defaults() {
        exc::init();
        let function = user_function(
            "f",
            vec![
                param("a", ParamKind::PosOnly, None),
                param("b", ParamKind::Positional, None),
            ],
            0,
        );
        let specification = call("getfullargspec", &[function]);
        assert!(!exc::pending());
        let args: Vec<String> = value::as_list(attr(specification, "args"))
            .expect("args list")
            .iter()
            .map(|item| string(*item))
            .collect();
        assert_eq!(args, vec!["a", "b"]);
        let defaults = value::as_tuple(attr(specification, "defaults")).expect("defaults");
        assert_eq!(defaults.len(), 0);
        assert_eq!(attr(specification, "kwonlydefaults"), NONE);
    }

    #[test]
    fn getcallargs_applies_defaults() {
        exc::init();
        let function = user_function(
            "f",
            vec![
                param("a", ParamKind::Positional, None),
                param("b", ParamKind::Positional, Some(integer(9))),
            ],
            0,
        );
        let mapping = call("getcallargs", &[function, integer(1)]);
        assert!(!exc::pending());
        assert_eq!(number(dict_lookup_name(mapping, "a").expect("a")), 1);
        assert_eq!(number(dict_lookup_name(mapping, "b").expect("b")), 9);
    }

    #[test]
    fn predicates_cover_runtime_callables() {
        exc::init();
        let function = user_function("f", vec![param("x", ParamKind::Positional, None)], 0);
        assert_eq!(call("isfunction", &[function]), TRUE);
        assert_eq!(call("ismethod", &[function]), FALSE);
        let generator = user_function(
            "g",
            vec![param("x", ParamKind::Positional, None)],
            value::FUNCTION_FLAG_GENERATOR,
        );
        assert_eq!(call("isgeneratorfunction", &[generator]), TRUE);
        let coroutine = user_function(
            "c",
            vec![param("x", ParamKind::Positional, None)],
            value::FUNCTION_FLAG_ASYNC,
        );
        assert_eq!(call("iscoroutinefunction", &[coroutine]), TRUE);
        let async_generator = user_function(
            "ag",
            vec![param("x", ParamKind::Positional, None)],
            value::FUNCTION_FLAG_ASYNC | value::FUNCTION_FLAG_GENERATOR,
        );
        assert_eq!(call("isasyncgenfunction", &[async_generator]), TRUE);
        assert_eq!(call("iscoroutinefunction", &[async_generator]), FALSE);
        let len = value::builtin_function_new(hydro_ir::Builtin::Len);
        assert_eq!(call("isbuiltin", &[len]), TRUE);
        assert_eq!(call("isroutine", &[len]), TRUE);
        assert_eq!(call("isclass", &[parameter_class()]), TRUE);
        assert_eq!(call("isclass", &[function]), FALSE);
        assert_eq!(call("isframe", &[function]), FALSE);
        assert_eq!(call("iscode", &[function]), FALSE);
        // Without a running Hydro activation there is no frame to report;
        // `currentframe` raises the same `ValueError` as `sys._getframe(0)`.
        let _ = call("currentframe", &[]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        let _ = exc::clear();
        assert_eq!(
            value::as_list(call("stack", &[])).map(|frames| frames.len()),
            Some(0)
        );
    }

    #[test]
    fn markcoroutinefunction_marks_without_a_code_object() {
        exc::init();
        let function = user_function("f", vec![param("x", ParamKind::Positional, None)], 0);
        assert_eq!(call("iscoroutinefunction", &[function]), FALSE);
        // The marker is set on the callable and returned as-is.
        assert_eq!(call("markcoroutinefunction", &[function]), function);
        assert_eq!(call("iscoroutinefunction", &[function]), TRUE);
        assert!(!exc::pending());

        // A bound method marks its underlying function, like CPython.
        let class = value::class_new("C".to_string(), NONE, vec![("m".to_string(), function)]);
        let instance = value::instance_new(class);
        let bound = methods::getattr(instance, "m".as_ptr(), 1);
        assert!(!exc::pending());
        assert_eq!(call("markcoroutinefunction", &[bound]), function);
        assert_eq!(call("iscoroutinefunction", &[bound]), TRUE);

        // Native callables have no writable function metadata: the failure is
        // explicit instead of a silent no-op.
        exc::init();
        let len = value::builtin_function_new(hydro_ir::Builtin::Len);
        call("markcoroutinefunction", &[len]);
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn getdoc_cleans_class_docstring() {
        exc::init();
        let class = value::class_new_full(
            "Doc".to_string(),
            NONE,
            Vec::new(),
            vec![("__doc__".to_string(), text("  Hello\n    world\n"))],
        );
        assert_eq!(string(call("getdoc", &[class])), "Hello\nworld");
        let function = user_function("f", Vec::new(), 0);
        assert_eq!(call("getdoc", &[function]), NONE);
    }

    #[test]
    fn getmembers_filters_and_sorts() {
        exc::init();
        let method = user_function(
            "method",
            vec![param("self", ParamKind::Positional, None)],
            0,
        );
        let mut guard = Roots::new();
        guard.keep(&[method]);
        let class = value::class_new_full(
            "Members".to_string(),
            NONE,
            vec![("method".to_string(), method, MethodKind::Normal)],
            vec![("constant".to_string(), integer(4))],
        );
        let members = call("getmembers", &[class]);
        assert!(!exc::pending());
        let names: Vec<String> = value::as_list(members)
            .expect("members")
            .iter()
            .map(|pair| string(value::as_tuple(*pair).expect("pair")[0]))
            .collect();
        assert!(names.contains(&"constant".to_string()));
        assert!(names.contains(&"method".to_string()));
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);

        let functions = call(
            "getmembers",
            &[
                class,
                value::native_function_new("inspect", "isfunction", 1),
            ],
        );
        assert!(!exc::pending());
        let function_names: Vec<String> = value::as_list(functions)
            .expect("members")
            .iter()
            .map(|pair| string(value::as_tuple(*pair).expect("pair")[0]))
            .collect();
        assert!(function_names.contains(&"method".to_string()));
        assert!(!function_names.contains(&"constant".to_string()));
    }

    #[test]
    fn getattr_static_does_not_invoke_descriptors() {
        exc::init();
        let init = user_function(
            "__init__",
            vec![param("self", ParamKind::Positional, None)],
            0,
        );
        let getter = user_function(
            "getter",
            vec![param("self", ParamKind::Positional, None)],
            0,
        );
        let mut guard = Roots::new();
        guard.keep(&[init, getter]);
        let class = value::class_new_full(
            "Static".to_string(),
            NONE,
            vec![
                ("__init__".to_string(), init, MethodKind::Normal),
                ("value".to_string(), getter, MethodKind::Property),
            ],
            Vec::new(),
        );
        let instance = value::instance_new(class);
        guard.keep(&[instance]);
        let raw = call("getattr_static", &[instance, text("value")]);
        assert!(!exc::pending());
        assert_eq!(raw, getter);
        let missing = call("getattr_static", &[instance, text("absent"), integer(11)]);
        assert_eq!(number(missing), 11);
        let error = call("getattr_static", &[instance, text("absent")]);
        assert_eq!(error, NONE);
        assert!(exc::pending());
        let _ = exc::clear();
    }

    #[test]
    fn getfile_and_getmodule_use_module_metadata() {
        exc::init();
        globals::set(
            "inspect_test_module",
            "__file__",
            text("/tmp/inspect_test.py"),
        );
        let module = value::module_value("inspect_test_module".to_string());
        assert_eq!(string(call("getfile", &[module])), "/tmp/inspect_test.py");
        assert_eq!(
            value::as_module(call("getmodule", &[module])).map(|state| state.name().to_string()),
            Some("inspect_test_module".to_string())
        );
        let cleandoc = value::native_function_new("inspect", "cleandoc", 1);
        assert_eq!(
            value::as_module(call("getmodule", &[cleandoc])).map(|state| state.name().to_string()),
            Some("inspect".to_string())
        );
    }
}
