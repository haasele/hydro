//! `annotationlib`: introspection helpers for Hydro's eagerly evaluated
//! annotations.
//!
//! Hydro has no lazy annotation scope (PEP 649). The frontend captures every
//! class/module/function annotation into an ordinary `__annotations__`
//! dictionary at definition time, so the runtime already holds evaluated
//! values. `annotationlib` exposes the CPython 3.14 surface pydantic uses on
//! top of that storage:
//!
//! * `get_annotations(obj, *, globals=None, locals=None, eval_str=False,
//!   format=Format.VALUE)`;
//! * `call_annotate_function(annotate, format, *, owner=None)` and
//!   `call_evaluate_function(evaluate, format, *, owner=None)`;
//! * `get_annotate_from_class_namespace(namespace)`;
//! * `type_repr(value)` and `annotations_to_string(annotations)`;
//! * the `Format` constants (`VALUE`, `VALUE_WITH_FAKE_GLOBALS`,
//!   `FORWARDREF`, `STRING`).
//!
//! Because the stored values are already evaluated, `VALUE` and `FORWARDREF`
//! return the same mapping (a fresh dictionary, like CPython) and `STRING`
//! renders each value with [`type_repr`]. A class whose namespace contains an
//! `__annotate__` callable is supported too: the callable runs and its result
//! is filtered through the requested format.
//!
//! There is no runtime compiler, so `eval_str=True` cannot evaluate a string
//! annotation. String values are returned unchanged, which is exactly what
//! pydantic's `eval_type` expects for `ForwardRef`-style annotations.

use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::methods;
use crate::value::{self, NONE};

/// `Format` member values. The enum is exposed as a class whose attributes
/// are these integers, matching `enum.IntEnum` member values.
const FORMAT_VALUE: i64 = 1;
const FORMAT_VALUE_WITH_FAKE_GLOBALS: i64 = 2;
const FORMAT_FORWARDREF: i64 = 3;
const FORMAT_STRING: i64 = 4;

fn int(n: i64) -> i64 {
    value::encode_int(n).unwrap_or(NONE)
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn missing_attribute(name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module 'annotationlib' has no attribute '{name}'"),
    )
}

/// `module.name` for the read-only members of the module.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "Format" => format_class(),
        "VALUE" => int(FORMAT_VALUE),
        "VALUE_WITH_FAKE_GLOBALS" => int(FORMAT_VALUE_WITH_FAKE_GLOBALS),
        "FORWARDREF" => int(FORMAT_FORWARDREF),
        "STRING" => int(FORMAT_STRING),
        "ForwardRef" => crate::modules::typing::constant("ForwardRef"),
        "__all__" => value::list_new(
            [
                "Format",
                "ForwardRef",
                "call_annotate_function",
                "call_evaluate_function",
                "get_annotate_from_class_namespace",
                "get_annotations",
                "annotations_to_string",
                "type_repr",
            ]
            .iter()
            .map(|name| value::str_alloc((*name).to_string()))
            .collect(),
        ),
        _ => missing_attribute(name),
    }
}

/// The `Format` class value: an empty class whose attributes are the integer
/// member values, so `Format.FORWARDREF` compares equal to the integer `3`
/// and `int(format)` works.
///
/// The class is cached in the globals registry together with the heap
/// generation (the [`crate::modules::inspect`] pattern): `hydro_runtime_init`
/// frees the heap, so a value from an earlier run must never be handed out
/// again.
const FORMAT_CACHE_KEY: &str = "annotationlib.format";
const FORMAT_CACHE_GENERATION: &str = "annotationlib.format.generation";

fn format_class() -> i64 {
    let generation = int(i64::try_from(heap::generation()).unwrap_or(i64::MAX));
    if globals::registry_get(FORMAT_CACHE_GENERATION) == Some(generation) {
        if let Some(class) = globals::registry_get(FORMAT_CACHE_KEY) {
            return class;
        }
    }
    let class = value::class_new("Format".to_string(), NONE, Vec::new());
    let roots = heap::root_values(&[class]);
    for (name, member) in [
        ("VALUE", FORMAT_VALUE),
        ("VALUE_WITH_FAKE_GLOBALS", FORMAT_VALUE_WITH_FAKE_GLOBALS),
        ("FORWARDREF", FORMAT_FORWARDREF),
        ("STRING", FORMAT_STRING),
    ] {
        value::class_set_attr(class, name.to_string(), int(member));
    }
    globals::registry_set(FORMAT_CACHE_GENERATION, generation);
    globals::registry_set(FORMAT_CACHE_KEY, class);
    heap::release_roots(roots);
    class
}

/// Positional-and-keyword dispatch for `annotationlib.<function>(...)`.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    call_kw(function, args, &[])
}

pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match function {
        "get_annotations" => get_annotations(args, keywords),
        "call_annotate_function" => call_annotate(args, keywords),
        "call_evaluate_function" => call_annotate(args, keywords),
        "get_annotate_from_class_namespace" => get_annotate_from_namespace(args),
        "type_repr" => type_repr_call(args),
        "annotations_to_string" => annotations_to_string_call(args),
        _ => missing_attribute(function),
    }
}

fn keyword(keywords: &[(String, i64)], name: &str) -> Option<i64> {
    keywords
        .iter()
        .find_map(|(key, value)| (key == name).then_some(*value))
}

/// Reads an attribute, swallowing `AttributeError` (the "does not exist"
/// signal used by the introspection helpers).
fn try_getattr(obj: i64, name: &str) -> Option<i64> {
    let result = methods::getattr(obj, name.as_ptr(), name.len());
    if exc::pending() {
        let _ = exc::clear();
        return None;
    }
    Some(result)
}

fn annotations_of(obj: i64) -> Option<i64> {
    if value::as_class(obj).is_some() {
        return value::class_annotations(obj);
    }
    if value::as_function(obj).is_some() {
        // A bound method forwards to its underlying function through
        // `getattr`; a plain function reads its own metadata slot.
        return value::function_annotations(obj).or_else(|| try_getattr(obj, "__annotations__"));
    }
    if let Some(module) = value::as_module(obj) {
        return globals::try_get(module.name(), "__annotations__");
    }
    try_getattr(obj, "__annotations__")
}

fn is_callable(obj: i64) -> bool {
    value::as_function(obj).is_some()
        || value::as_class(obj).is_some()
        || value::as_instance(obj).is_some() && try_getattr(obj, "__call__").is_some()
}

fn format_member(argument: i64) -> Option<i64> {
    value::as_integer(argument).filter(|member| (1..=4).contains(member))
}

/// `get_annotations(obj, *, globals=None, locals=None, eval_str=False,
/// format=Format.VALUE)`.
fn get_annotations(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let Some(&obj) = args.first() else {
        return type_error("get_annotations() missing required argument 'obj' (pos 1)");
    };
    let mut format = int(FORMAT_VALUE);
    let mut eval_str = false;
    for (name, argument) in keywords {
        match name.as_str() {
            "format" => format = *argument,
            "eval_str" => eval_str = value::truthy(*argument),
            // `globals`/`locals` only matter for `eval_str`; the stored
            // annotations are already evaluated, so they are accepted and
            // ignored.
            "globals" | "locals" => {
                if *argument != NONE && value::as_dict(*argument).is_none() {
                    return type_error(&format!(
                        "get_annotations() argument '{name}' must be a dict or None"
                    ));
                }
            }
            _ => {
                return type_error(&format!(
                    "get_annotations() got an unexpected keyword argument '{name}'"
                ))
            }
        }
    }
    let Some(format) = format_member(format) else {
        return value_error(&format!("Unsupported format {}", value::repr(format)));
    };
    if format == FORMAT_VALUE_WITH_FAKE_GLOBALS {
        return value_error("The VALUE_WITH_FAKE_GLOBALS format is for internal use only");
    }
    if eval_str && format != FORMAT_VALUE {
        return value_error("eval_str=True is only supported with format=Format.VALUE");
    }

    // A class may install a PEP 649 `__annotate__` callable. Use it for the
    // STRING format (source text is only available from the callable) and as
    // the fallback when the class has no stored annotations yet.
    let annotate = try_getattr(obj, "__annotate__");
    if let Some(annotate) = annotate {
        if format == FORMAT_STRING {
            return call_annotate_value(annotate, format);
        }
        if value::as_class(obj).is_some() {
            if let Some(annotations) = value::class_annotations(obj) {
                if !value::as_dict(annotations).is_some_and(|pairs| pairs.is_empty()) {
                    return finish_annotations(annotations, format);
                }
            }
            return call_annotate_value(annotate, format);
        }
    }

    let Some(annotations) = annotations_of(obj) else {
        if value::as_class(obj).is_some() || is_callable(obj) {
            return value::dict_new(Vec::new());
        }
        return type_error(&format!("{} does not have annotations", value::repr(obj)));
    };
    finish_annotations(annotations, format)
}

fn finish_annotations(annotations: i64, format: i64) -> i64 {
    if value::as_dict(annotations).is_none() {
        return value_error(&format!("{} is not a dict", value::repr(annotations)));
    }
    if format == FORMAT_STRING {
        return stringified(annotations);
    }
    // VALUE and FORWARDREF report the evaluated mapping; the result is a
    // fresh dictionary, exactly like CPython's `dict(ann)`.
    let roots = heap::root_values(&[annotations]);
    let result = value::dict_new(value::as_dict(annotations).unwrap_or(&[]).to_vec());
    heap::release_roots(roots);
    result
}

/// A fresh dictionary whose values are the STRING-format rendering.
fn stringified(annotations: i64) -> i64 {
    let Some(pairs) = value::as_dict(annotations) else {
        return value::dict_new(Vec::new());
    };
    let roots = heap::root_values(&[annotations]);
    let mut rendered = Vec::with_capacity(pairs.len());
    for (key, item) in pairs {
        let text = if value::as_str(*item).is_some() {
            *item
        } else {
            value::str_alloc(render_annotation(*item))
        };
        rendered.push((*key, text));
    }
    let result = value::dict_new(rendered);
    heap::release_roots(roots);
    result
}

/// `call_annotate_function(annotate, format, *, owner=None)` (and its
/// `call_evaluate_function` alias).
fn call_annotate(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let Some(&annotate) = args.first() else {
        return type_error("call_annotate_function() missing required argument 'annotate' (pos 1)");
    };
    let format = args
        .get(1)
        .copied()
        .or_else(|| keyword(keywords, "format"))
        .unwrap_or_else(|| int(FORMAT_VALUE));
    let Some(format) = format_member(format) else {
        return value_error(&format!("Unsupported format {}", value::repr(format)));
    };
    for (name, _) in keywords {
        if !matches!(name.as_str(), "format" | "owner" | "_is_evaluate") {
            return type_error(&format!(
                "call_annotate_function() got an unexpected keyword argument '{name}'"
            ));
        }
    }
    call_annotate_value(annotate, format)
}

fn call_annotate_value(annotate: i64, format: i64) -> i64 {
    if format == FORMAT_VALUE_WITH_FAKE_GLOBALS {
        return value_error("The VALUE_WITH_FAKE_GLOBALS format is for internal use only");
    }
    if value::as_function(annotate).is_none() {
        return type_error(&format!(
            "{} is not an __annotate__ function",
            value::repr(annotate)
        ));
    }
    // Compiler-generated `__annotate__` functions take one `format` argument;
    // the zero-argument thunk form is accepted as well.
    let arity = value::as_function(annotate).map(|function| function.arity);
    let argv = [int(format)];
    let result = match arity {
        Some(0) => crate::builtins::call(annotate, 0, argv.as_ptr()),
        _ => crate::builtins::call(annotate, 1, argv.as_ptr()),
    };
    if exc::pending() {
        return result;
    }
    if value::as_dict(result).is_none() {
        return value_error("__annotate__ returned a non-dict");
    }
    finish_annotations(result, format)
}

/// Evaluates a class's PEP 649 `__annotate__` callable, if present.
///
/// `value::class_annotations` consults this before creating its lazy empty
/// mapping, so `Class.__annotations__` observes the deferred annotations that
/// the frontend emits for class bodies.
pub(crate) fn evaluate_class_annotate(class: i64) -> Option<i64> {
    let annotate = match value::class_lookup(class, "__annotate__") {
        Some(value::ClassEntry::Attr(annotate)) => annotate,
        Some(value::ClassEntry::Method(_, annotate)) => annotate,
        None => return None,
    };
    let result = call_annotate_value(annotate, FORMAT_VALUE);
    if crate::exc::pending() {
        return None;
    }
    if value::as_dict(result).is_some() {
        Some(result)
    } else {
        None
    }
}

/// `get_annotate_from_class_namespace(obj)`.
fn get_annotate_from_namespace(args: &[i64]) -> i64 {
    let Some(&namespace) = args.first() else {
        return type_error(
            "get_annotate_from_class_namespace() missing required argument 'obj' (pos 1)",
        );
    };
    let Some(pairs) = value::as_dict(namespace) else {
        return type_error("get_annotate_from_class_namespace() argument must be a dict");
    };
    pairs
        .iter()
        .find_map(|(key, item)| (value::as_str(*key) == Some("__annotate__")).then_some(*item))
        .or_else(|| {
            pairs.iter().find_map(|(key, item)| {
                (value::as_str(*key) == Some("__annotate_func__")).then_some(*item)
            })
        })
        .unwrap_or(NONE)
}

/// `type_repr(value)`: the STRING-format rendering of one annotation.
fn type_repr_call(args: &[i64]) -> i64 {
    let Some(&annotation) = args.first() else {
        return type_error("type_repr() missing required argument 'value' (pos 1)");
    };
    value::str_alloc(render_annotation(annotation))
}

/// `annotations_to_string(annotations)`.
fn annotations_to_string_call(args: &[i64]) -> i64 {
    let Some(&annotations) = args.first() else {
        return type_error("annotations_to_string() missing required argument (pos 1)");
    };
    if value::as_dict(annotations).is_none() {
        return type_error("annotations_to_string() argument must be a dict");
    }
    stringified(annotations)
}

/// Renders one annotation value the way CPython's `type_repr` does: classes
/// and functions use `module.qualname`, everything else uses `repr`.
fn render_annotation(annotation: i64) -> String {
    if let Some(text) = value::as_str(annotation) {
        return text.to_string();
    }
    if value::as_class(annotation).is_some() {
        let name = value::class_display_name(annotation)
            .unwrap_or("<anonymous>")
            .to_string();
        return qualify(annotation, name);
    }
    if value::as_function(annotation).is_some() {
        let name = value::function_qualname(annotation);
        let name = if name.is_empty() {
            value::function_name(annotation)
        } else {
            name
        };
        return qualify_function(annotation, name);
    }
    value::repr(annotation)
}

fn module_of(obj: i64) -> Option<String> {
    try_getattr(obj, "__module__")
        .and_then(value::as_str)
        .map(str::to_string)
        .filter(|module| !module.is_empty())
}

fn qualify(obj: i64, name: String) -> String {
    match module_of(obj) {
        Some(module) if module != "builtins" => format!("{module}.{name}"),
        _ => name,
    }
}

fn qualify_function(function: i64, name: String) -> String {
    match value::function_module(function)
        .and_then(value::as_str)
        .map(str::to_string)
    {
        Some(module) if module != "builtins" => format!("{module}.{name}"),
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{as_dict, as_str, dict_new, encode_int, str_alloc};

    fn text(value_in: &str) -> i64 {
        str_alloc(value_in.to_string())
    }

    fn integer(value_in: i64) -> i64 {
        encode_int(value_in).expect("small integer")
    }

    fn pair(dict: i64, name: &str) -> Option<i64> {
        as_dict(dict)?
            .iter()
            .find_map(|(key, item)| (as_str(*key) == Some(name)).then_some(*item))
    }

    fn class_with_annotations(entries: &[(&str, i64)]) -> i64 {
        let pairs: Vec<(i64, i64)> = entries
            .iter()
            .map(|(name, item)| (text(name), *item))
            .collect();
        let annotations = dict_new(pairs);
        let roots = heap::root_values(&[annotations]);
        let class = value::class_new("Model".to_string(), NONE, Vec::new());
        let class_roots = heap::root_values(&[class]);
        value::class_set_attr(class, "__annotations__".to_string(), annotations);
        heap::release_roots(class_roots);
        heap::release_roots(roots);
        class
    }

    #[test]
    fn format_members_are_the_intenum_values() {
        exc::init();
        assert_eq!(value::as_integer(constant("VALUE")), Some(1));
        assert_eq!(value::as_integer(constant("FORWARDREF")), Some(3));
        assert_eq!(value::as_integer(constant("STRING")), Some(4));
        let format = constant("Format");
        assert!(value::as_class(format).is_some());
        let member = methods::getattr(format, "FORWARDREF".as_ptr(), 10);
        assert!(
            !exc::pending(),
            "{}",
            value::to_string(exc::pending_value())
        );
        assert_eq!(value::as_integer(member), Some(3));
    }

    #[test]
    fn get_annotations_returns_a_fresh_copy_of_class_annotations() {
        exc::init();
        let string_type = value::class_new("str".to_string(), NONE, Vec::new());
        let model = class_with_annotations(&[("name", string_type), ("age", integer(0))]);
        let first = get_annotations(&[model], &[]);
        assert!(!exc::pending());
        assert_eq!(pair(first, "name"), Some(string_type));
        assert_eq!(pair(first, "age"), Some(integer(0)));
        let second = get_annotations(&[model], &[]);
        assert_ne!(first, second, "each call returns a fresh dictionary");
    }

    #[test]
    fn class_annotations_are_per_class_and_not_inherited() {
        exc::init();
        let string_type = value::class_new("str".to_string(), NONE, Vec::new());
        let base = class_with_annotations(&[("name", string_type)]);
        let roots = heap::root_values(&[base]);
        let child = value::class_new("Child".to_string(), base, Vec::new());
        heap::release_roots(roots);
        let annotations = get_annotations(&[child], &[]);
        assert!(!exc::pending());
        assert!(
            value::as_dict(annotations).is_some_and(|pairs| pairs.is_empty()),
            "a subclass without its own annotations must report an empty mapping"
        );
        let inherited = get_annotations(&[base], &[]);
        assert_eq!(pair(inherited, "name"), Some(string_type));
    }

    #[test]
    fn forwardref_format_keeps_evaluated_values() {
        exc::init();
        let string_type = value::class_new("str".to_string(), NONE, Vec::new());
        let model = class_with_annotations(&[("name", string_type)]);
        let result = get_annotations(
            &[model],
            &[("format".to_string(), integer(FORMAT_FORWARDREF))],
        );
        assert!(!exc::pending());
        assert_eq!(pair(result, "name"), Some(string_type));
    }

    #[test]
    fn string_format_renders_annotation_values() {
        exc::init();
        let string_type = value::class_new("str".to_string(), NONE, Vec::new());
        let model = class_with_annotations(&[
            ("name", string_type),
            ("literal", text("AlreadyAForwardRef")),
        ]);
        let result = get_annotations(&[model], &[("format".to_string(), integer(FORMAT_STRING))]);
        assert!(!exc::pending());
        assert_eq!(
            pair(result, "name").and_then(as_str),
            Some("str"),
            "a builtin class renders by its plain name"
        );
        assert_eq!(
            pair(result, "literal").and_then(as_str),
            Some("AlreadyAForwardRef"),
            "string annotations pass through unchanged"
        );
    }

    #[test]
    fn unsupported_and_fake_global_formats_raise_value_error() {
        exc::init();
        let model = class_with_annotations(&[]);
        let _ = get_annotations(&[model], &[("format".to_string(), integer(9))]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        let _ = exc::clear();
        let _ = get_annotations(
            &[model],
            &[(
                "format".to_string(),
                integer(FORMAT_VALUE_WITH_FAKE_GLOBALS),
            )],
        );
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        let _ = exc::clear();
    }

    extern "C" fn annotate_wrapper(argv: *const i64) -> i64 {
        // SAFETY: `call_annotate` passes exactly one format argument.
        let _format = unsafe { *argv };
        dict_new(vec![(text("x"), integer(1))])
    }

    #[test]
    fn call_annotate_function_invokes_a_generated_annotate_function() {
        exc::init();
        let annotate = value::function_new_with_flags(
            annotate_wrapper as *const () as usize,
            1,
            "annotate".to_string(),
            0,
        );
        let roots = heap::root_values(&[annotate]);
        let result = call_annotate(&[annotate, integer(FORMAT_VALUE)], &[]);
        heap::release_roots(roots);
        assert!(!exc::pending());
        assert_eq!(pair(result, "x"), Some(integer(1)));
    }

    #[test]
    fn call_annotate_function_rejects_a_non_callable() {
        exc::init();
        let _ = call_annotate(&[text("not a function"), integer(FORMAT_VALUE)], &[]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        let _ = exc::clear();
    }

    #[test]
    fn namespace_helper_reads_both_spellings() {
        exc::init();
        let annotate = value::native_function_new("annotationlib", "annotate", 1);
        let namespace = dict_new(vec![(text("__annotate__"), annotate)]);
        let roots = heap::root_values(&[annotate, namespace]);
        assert_eq!(get_annotate_from_namespace(&[namespace]), annotate);
        let legacy = dict_new(vec![(text("__annotate_func__"), annotate)]);
        assert_eq!(get_annotate_from_namespace(&[legacy]), annotate);
        heap::release_roots(roots);
    }

    #[test]
    fn annotations_to_string_returns_a_fresh_dict() {
        exc::init();
        let source = dict_new(vec![(text("x"), text("Forward")), (text("y"), integer(2))]);
        let roots = heap::root_values(&[source]);
        let rendered = annotations_to_string_call(&[source]);
        heap::release_roots(roots);
        assert!(!exc::pending());
        assert_eq!(pair(rendered, "x").and_then(as_str), Some("Forward"));
        assert_eq!(pair(rendered, "y").and_then(as_str), Some("2"));
    }
}
