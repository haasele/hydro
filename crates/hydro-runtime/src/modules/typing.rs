#![allow(clippy::needless_borrow, clippy::question_mark)]
//! `typing`: importable placeholders for the typing names used by the
//! supported corpus.
//!
//! Type annotations are consumed by the frontend and the runtime never
//! introspects these values. Generic aliases and special forms are empty
//! native classes: classes are subscriptable in Hydro, so runtime-evaluated
//! aliases such as `CellSpan = Tuple[int, int, int]` produce a value instead
//! of failing on `None[...]`. No generic substitution or `isinstance`
//! semantics are attached; annotations themselves are not evaluated.
//! `TYPE_CHECKING` is `False`, which keeps guarded imports from running.
//!
//! Most class constants allocate a fresh class per access, matching CPython's
//! stable identity semantics closely enough for subclassing. `ForwardRef` is
//! the exception: it is a generation-cached class because `isinstance` checks
//! and construction must share one identity.
//!
//! Two runtime helpers from CPython 3.14's `typing` are implemented for
//! `annotationlib`:
//!
//! * `_deprecation_warning_for_no_type_params_passed(name)` emits the same
//!   `DeprecationWarning` text CPython uses;
//! * `evaluate_forward_ref(ref, ...)` evaluates a `ForwardRef` whose argument
//!   is a plain (dotted) name against the supplied `locals`/`globals`. Full
//!   expression evaluation needs a runtime compiler Hydro does not have, so a
//!   non-name expression raises a tagged `NotImplementedError` when the helper
//!   is actually called (attribute lookup always succeeds).

use std::sync::{LazyLock, Mutex};

use hydro_ir::ParamKind;

use crate::container;
use crate::exc::{
    self, ATTRIBUTE_ERROR, DEPRECATION_WARNING, NAME_ERROR, NOT_IMPLEMENTED_ERROR, TYPE_ERROR,
};
use crate::heap;
use crate::methods;
use crate::value::{self, FALSE, NONE, TRUE};

/// Generation-tagged cache for the native `ForwardRef` class.
struct ForwardRefState {
    generation: u64,
    class: i64,
}

static FORWARD_REF: LazyLock<Mutex<Option<ForwardRefState>>> = LazyLock::new(|| Mutex::new(None));

/// `typing.ForwardRef` as a real class value.
///
/// The class is what `isinstance(x, typing.ForwardRef)` needs; the module
/// lookup returns it for the function spec that makes `typing.ForwardRef(arg)`
/// compile. Construction is served by [`new_forward_ref`] and by the native
/// `__init__` used for indirect calls.
pub(crate) fn class_value(module: &str, name: &str) -> Option<i64> {
    if module == "typing" && name == "ForwardRef" {
        return Some(forward_ref_class());
    }
    None
}

fn forward_ref_class() -> i64 {
    let generation = heap::generation();
    {
        let guard = FORWARD_REF
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(state) = guard.as_ref() {
            if state.generation == generation {
                heap::add_thread_root(state.class);
                return state.class;
            }
        }
    }
    let mut signature = Vec::with_capacity(7);
    signature.push(value::ParamMeta {
        name: "self".to_string(),
        kind: ParamKind::Positional,
        default: None,
    });
    for (name, default) in [
        ("arg", None),
        ("module", Some(NONE)),
        ("owner", Some(NONE)),
        ("is_argument", Some(TRUE)),
        ("is_class", Some(FALSE)),
    ] {
        signature.push(value::ParamMeta {
            name: name.to_string(),
            kind: ParamKind::Positional,
            default,
        });
    }
    signature.push(value::ParamMeta {
        name: "kwargs".to_string(),
        kind: ParamKind::KwArg,
        default: Some(NONE),
    });
    let init = value::native_function_new("typing", "_forward_ref_init", usize::MAX);
    value::function_set_signature(init, signature);
    value::function_set_full_entry(init, init_forward_ref as *const () as usize);
    let class = value::class_new(
        "ForwardRef".to_string(),
        NONE,
        vec![("__init__".to_string(), init)],
    );
    heap::add_thread_root(class);
    *FORWARD_REF
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some(ForwardRefState { generation, class });
    class
}

/// Native `ForwardRef.__init__(self, arg, module=None, owner=None,
/// is_argument=True, is_class=False, **kwargs)`.
extern "C" fn init_forward_ref(argv: *const i64) -> i64 {
    // SAFETY: the class-call binder passes one slot per declared parameter.
    let instance = unsafe { *argv };
    let arg = unsafe { *argv.add(1) };
    let module = unsafe { *argv.add(2) };
    let owner = unsafe { *argv.add(3) };
    let is_argument = unsafe { *argv.add(4) };
    let is_class = unsafe { *argv.add(5) };
    populate_forward_ref(instance, arg, module, owner, is_argument, is_class)
}

/// Stores the CPython `ForwardRef` attributes on a fresh instance.
fn populate_forward_ref(
    instance: i64,
    arg: i64,
    module: i64,
    owner: i64,
    is_argument: i64,
    is_class: i64,
) -> i64 {
    let Some(text) = value::as_str(arg) else {
        return type_error(&format!(
            "Forward reference must be a string -- got {}",
            value::repr(arg)
        ));
    };
    let arg_value = value::str_alloc(text.to_string());
    let roots = heap::root_values(&[instance, arg_value, module, owner, is_argument, is_class]);
    for (name, item) in [
        ("__arg__", arg_value),
        ("__forward_arg__", arg_value),
        ("__forward_module__", module),
        ("__owner__", owner),
        ("__forward_is_argument__", is_argument),
        ("__forward_is_class__", is_class),
        ("__globals__", NONE),
        ("__extra_names__", NONE),
        ("__cell__", NONE),
        ("__code__", NONE),
        ("__ast_node__", NONE),
        ("__resolved_str_cache__", NONE),
    ] {
        value::instance_setattr(instance, name.to_string(), item);
    }
    heap::release_roots(roots);
    NONE
}

/// `typing.ForwardRef(arg, *, module=None, owner=None, is_argument=True,
/// is_class=False)` through the module-call path.
fn new_forward_ref(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let mut arg = args.first().copied();
    let mut module = args.get(1).copied().unwrap_or(NONE);
    let mut owner = args.get(2).copied().unwrap_or(NONE);
    let mut is_argument = args.get(3).copied().unwrap_or(TRUE);
    let mut is_class = args.get(4).copied().unwrap_or(FALSE);
    if args.len() > 5 {
        return type_error("ForwardRef() takes at most 5 positional arguments");
    }
    for (keyword, item) in keywords {
        let item = *item;
        match keyword.as_str() {
            "arg" => arg = Some(item),
            "module" => module = item,
            "owner" => owner = item,
            "is_argument" => is_argument = item,
            "is_class" => is_class = item,
            other => {
                return type_error(&format!(
                    "ForwardRef() got an unexpected keyword argument '{other}'"
                ))
            }
        }
    }
    let Some(arg) = arg else {
        return type_error("ForwardRef() missing required argument 'arg'");
    };
    for (name, item) in [("is_argument", is_argument), ("is_class", is_class)] {
        if item != TRUE && item != FALSE {
            return type_error(&format!("ForwardRef() {name} must be a bool"));
        }
    }
    let instance = value::instance_new(forward_ref_class());
    let result = populate_forward_ref(instance, arg, module, owner, is_argument, is_class);
    if exc::pending() {
        return result;
    }
    instance
}

/// Constants materialised as empty native classes: usable as class bases and
/// subscriptable when a library evaluates an alias at runtime.
const CLASSES: &[&str] = &[
    "Any",
    "AnyStr",
    "BinaryIO",
    "Callable",
    "ClassVar",
    "Collection",
    "ContextManager",
    "DefaultDict",
    "Deque",
    "Dict",
    "Final",
    "ForwardRef",
    "Generator",
    "Generic",
    "IO",
    "Iterable",
    "Iterator",
    "List",
    "Literal",
    "Mapping",
    "Match",
    "NamedTuple",
    "NoReturn",
    "Optional",
    "Pattern",
    "Protocol",
    "Sequence",
    "Set",
    "TextIO",
    "Tuple",
    "Type",
    "Union",
];

/// Materialises the module constant `name`.
pub(crate) fn constant(name: &str) -> i64 {
    if name == "TYPE_CHECKING" {
        return FALSE;
    }
    if name == "ForwardRef" {
        return forward_ref_class();
    }
    if CLASSES.contains(&name) {
        return value::class_new(name.to_string(), NONE, Vec::new());
    }
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module 'typing' has no attribute '{name}'"),
    )
}

/// Dispatches a native `typing` function. Arity is enforced by the central
/// spec before this point.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        // CPython's signature is `cast(typ, val)`; the value argument wins.
        "cast" => args.get(1).copied().unwrap_or(NONE),
        "runtime_checkable" | "overload" => args.first().copied().unwrap_or(NONE),
        // Type variables are annotations only; the frontend ignores the value.
        "TypeVar" => NONE,
        // `NewType("Name", int)` is called like the underlying type in the
        // corpus, so the underlying type is the usable stand-in.
        "NewType" => args.get(1).copied().unwrap_or(NONE),
        "get_type_hints" => value::dict_new(Vec::new()),
        "get_args" => value::tuple_new(Vec::new()),
        "ForwardRef" => new_forward_ref(args, &[]),
        "_deprecation_warning_for_no_type_params_passed" => no_type_params(args, &[]),
        "evaluate_forward_ref" => evaluate_forward_ref(args, &[]),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'typing' has no attribute '{function}'"),
        ),
    }
}

/// Keyword-aware dispatch for the two `annotationlib` helpers.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match function {
        "ForwardRef" => new_forward_ref(args, keywords),
        "_deprecation_warning_for_no_type_params_passed" => no_type_params(args, keywords),
        "evaluate_forward_ref" => evaluate_forward_ref(args, keywords),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'typing' has no attribute '{function}'"),
        ),
    }
}

/// `typing._deprecation_warning_for_no_type_params_passed(funcname)`.
fn no_type_params(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let mut name = args.first().copied();
    let mut stacklevel = None;
    for (keyword, value) in keywords {
        let value = *value;
        match keyword.as_str() {
            "funcname" | "name" => name = Some(value),
            "stacklevel" => match value::as_integer(value) {
                Some(level) => stacklevel = Some(level),
                None => return type_error("stacklevel must be an int"),
            },
            other => {
                return type_error(&format!(
                    "_deprecation_warning_for_no_type_params_passed() got an unexpected keyword argument '{other}'"
                ))
            }
        }
    }
    let Some(name_value) = name else {
        return type_error(
            "_deprecation_warning_for_no_type_params_passed() missing required argument 'funcname'",
        );
    };
    let Some(name) = value::as_str(name_value) else {
        return type_error(
            "_deprecation_warning_for_no_type_params_passed() argument 'funcname' must be a string",
        );
    };
    // The location is not observable without a frame stack; keep the
    // parameter validated but document that only one call site exists.
    let _ = stacklevel;
    let name_repr = format!("'{}'", name.replace('\\', "\\\\").replace('\'', "\\'"));
    let message = format!(
        "Failing to pass a value to the 'type_params' parameter \
         of {name_repr} is deprecated, as it leads to incorrect behaviour \
         when calling {name} on a stringified annotation \
         that references a PEP 695 type parameter. \
         It will be disallowed in Python 3.15."
    );
    super::warnings::warn_category(&message, DEPRECATION_WARNING)
}

/// `typing.evaluate_forward_ref(forward_ref, *, globals, locals, ...)`.
///
/// `owner`, `type_params` and `_recursive_guard` are accepted for signature
/// compatibility; without a runtime compiler they do not affect evaluation.
fn evaluate_forward_ref(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if args.len() > 1 {
        return type_error(
            "evaluate_forward_ref() takes 1 positional argument but more were given",
        );
    }
    let mut forward_ref = args.first().copied();
    let mut globals = NONE;
    let mut locals = NONE;
    let mut format = NONE;
    for (keyword, value) in keywords {
        let value = *value;
        match keyword.as_str() {
            "forward_ref" => forward_ref = Some(value),
            "globals" => globals = value,
            "locals" => locals = value,
            "type_params" | "owner" | "_recursive_guard" => {}
            "format" => format = value,
            other => {
                return type_error(&format!(
                    "evaluate_forward_ref() got an unexpected keyword argument '{other}'"
                ))
            }
        }
    }
    for (name, value) in [("globals", globals), ("locals", locals)] {
        if value != NONE && value::as_dict(value).is_none() {
            return type_error(&format!(
                "evaluate_forward_ref() argument '{name}' must be a dict or None"
            ));
        }
    }
    let Some(forward_ref) = forward_ref else {
        return type_error("evaluate_forward_ref() missing required argument 'forward_ref'");
    };
    let Some(text) = forward_ref_text(forward_ref) else {
        return type_error(
            "evaluate_forward_ref() argument 'forward_ref' must be a ForwardRef instance",
        );
    };
    // `annotationlib.Format.STRING` is the IntEnum member with value 4.
    if value::as_integer(format) == Some(4) {
        return value::str_alloc(text.to_string());
    }
    let resolved = match resolve_name(&text, globals, locals) {
        Ok(value) => value,
        Err(error) => return error,
    };
    match resolved {
        Some(value) => value,
        None => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            &format!(
                "typing.evaluate_forward_ref() cannot evaluate the forward reference {text:?} in Hydro: \
                 only plain (dotted) names supported by the runtime can be resolved"
            ),
        ),
    }
}

/// Reads the textual argument of a `ForwardRef`-shaped value.
fn forward_ref_text(value: i64) -> Option<&'static str> {
    if value::instance_class_value(value).is_none() {
        return None;
    }
    for attribute in ["__forward_arg__", "__arg__"] {
        if let Some(argument) = value::instance_attr(value, attribute) {
            if argument != NONE {
                return value::as_str(argument);
            }
        }
    }
    None
}

/// Resolves a plain or dotted name through `locals`, then `globals`.
///
/// `Ok(None)` means the spelling is not statically resolvable (the caller
/// reports the tagged `NotImplementedError`), while `Err` carries a raised
/// Python exception such as `NameError`.
fn resolve_name(text: &str, globals: i64, locals: i64) -> Result<Option<i64>, i64> {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.is_empty() || parts.iter().any(|part| !is_identifier(part)) {
        return Ok(None);
    }
    let mut current = match lookup_name(parts[0], locals).or_else(|| lookup_name(parts[0], globals))
    {
        Some(value) => value,
        None => {
            return Err(exc::raise_with_message(
                NAME_ERROR,
                &format!("name '{}' is not defined", parts[0]),
            ))
        }
    };
    for part in &parts[1..] {
        current = methods::getattr(current, part.as_ptr(), part.len());
        if exc::pending() {
            return Err(current);
        }
    }
    Ok(Some(current))
}

fn lookup_name(name: &str, namespace: i64) -> Option<i64> {
    let entries = value::as_dict(namespace)?;
    let key = value::str_alloc(name.to_string());
    let position = container::dict_find(entries, key)?;
    Some(entries[position].1)
}

/// ASCII identifier check for annotation names (Unicode identifiers are
/// resolved by the frontend, not this fallback helper).
fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{as_class, as_dict, as_tuple, TRUE};

    fn text(s: &str) -> i64 {
        value::str_alloc(s.to_string())
    }

    #[test]
    fn type_checking_is_false() {
        exc::init();
        assert_eq!(constant("TYPE_CHECKING"), FALSE);
        assert_ne!(constant("TYPE_CHECKING"), TRUE);
        assert!(!exc::pending());
    }

    #[test]
    fn generic_aliases_are_subscriptable_classes() {
        exc::init();
        for name in CLASSES {
            if *name == "ForwardRef" {
                continue;
            }
            let class = as_class(constant(name)).expect("class value");
            assert_eq!(class.name, *name);
            assert!(!exc::pending());
        }
    }

    #[test]
    fn class_placeholders_are_native_classes() {
        exc::init();
        for name in CLASSES {
            if *name == "ForwardRef" {
                continue;
            }
            let class = as_class(constant(name)).expect("class value");
            assert_eq!(class.name, *name);
            assert_eq!(class.base, NONE);
            assert!(class.methods.is_empty());
        }
        assert!(!exc::pending());
    }

    #[test]
    fn generic_aliases_and_classes_are_subscriptable() {
        exc::init();
        let index = crate::core::value::str_alloc("int".to_string());
        let alias = constant("Tuple");
        assert_eq!(crate::core::container::subscript(alias, index), alias);
        let class = value::class_new("Plain".to_string(), NONE, Vec::new());
        assert_eq!(crate::core::container::subscript(class, index), class);
        assert!(!exc::pending());
    }

    #[test]
    fn cast_returns_the_value_argument() {
        exc::init();
        let typ = constant("Any");
        let value = text("payload");
        assert_eq!(call("cast", &[typ, value]), value);
        // The type argument is ignored even when it is a nested alias.
        assert_eq!(call("cast", &[NONE, NONE]), NONE);
        assert!(!exc::pending());
    }

    #[test]
    fn decorators_are_identity_and_typevar_is_none() {
        exc::init();
        let function = text("f");
        assert_eq!(call("runtime_checkable", &[function]), function);
        assert_eq!(call("overload", &[function]), function);
        assert_eq!(call("TypeVar", &[text("T")]), NONE);
        assert_eq!(call("TypeVar", &[text("T"), text("int")]), NONE);
        // `NewType` stands in for the underlying type so calls work.
        assert_eq!(call("NewType", &[text("TaskID"), function]), function);
        assert!(!exc::pending());
    }

    #[test]
    fn introspection_helpers_return_empty_containers() {
        exc::init();
        let hints = call("get_type_hints", &[text("f")]);
        assert_eq!(as_dict(hints).expect("dict").len(), 0);
        let empty = call("get_type_hints", &[]);
        assert_eq!(as_dict(empty).expect("dict").len(), 0);
        let args = call("get_args", &[NONE]);
        assert_eq!(as_tuple(args).expect("tuple").len(), 0);
        assert!(!exc::pending());
    }

    #[test]
    fn unknown_function_raises_attribute_error() {
        exc::init();
        call("missing", &[]);
        assert!(exc::pending());
        exc::clear();
    }
}
