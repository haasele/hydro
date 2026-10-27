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
    if module != "typing" {
        return None;
    }
    match name {
        "ForwardRef" => Some(forward_ref_class()),
        // `ParamSpec` and `TypeVarTuple` are declared as module functions so
        // calls lower as module calls, but the attribute value itself is the
        // cached class (`isinstance`, `from typing import ParamSpec` and
        // subclassing all need it).
        "ParamSpec" | "TypeVarTuple" => Some(typevar_like_class(name)),
        _ => None,
    }
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
    "Annotated",
    "Any",
    "AnyStr",
    "AsyncContextManager",
    "AsyncGenerator",
    "AsyncIterable",
    "AsyncIterator",
    "Awaitable",
    "BinaryIO",
    "Callable",
    "ChainMap",
    "ClassVar",
    "Collection",
    "Concatenate",
    "ContextManager",
    "Coroutine",
    "Counter",
    "DefaultDict",
    "Deque",
    "Dict",
    "Doc",
    "Final",
    "ForwardRef",
    "FrozenSet",
    "Generator",
    "Generic",
    "IO",
    "Iterable",
    "Iterator",
    "List",
    "Literal",
    "LiteralString",
    "Mapping",
    "Match",
    "NamedTuple",
    "Never",
    "NoReturn",
    "NotRequired",
    "Optional",
    "OrderedDict",
    "ParamSpecArgs",
    "ParamSpecKwargs",
    "Pattern",
    "Protocol",
    "ReadOnly",
    "Required",
    "Self",
    "Sequence",
    "Set",
    "SupportsAbs",
    "SupportsBytes",
    "SupportsComplex",
    "SupportsFloat",
    "SupportsIndex",
    "SupportsInt",
    "SupportsRound",
    "Text",
    "TextIO",
    "Tuple",
    "Type",
    "TypeAlias",
    "TypeAliasType",
    "TypeForm",
    "TypeGuard",
    "TypeIs",
    "TypedDict",
    "Union",
    "Unpack",
];

/// CPython `typing` internals that `typing_extensions` subclasses or checks
/// with `isinstance`. These are cached (identity-stable) classes; they carry
/// no runtime behaviour of their own, exactly like the placeholders above.
const CACHED_CLASSES: &[&str] = &[
    "_AnnotatedAlias",
    "_BaseGenericAlias",
    "_ConcatenateGenericAlias",
    "_Final",
    "_GenericAlias",
    "_NotIterable",
    "_ProtocolMeta",
    "_SpecialForm",
    "_SpecialFrom",
    "_SpecialGenericAlias",
    "_TypeVarLikeMeta",
    "_TypedDictMeta",
    "_UnionGenericAlias",
    "_UnionGenericAliasMeta",
];

/// `typing.EXCLUDED_ATTRIBUTES`: names `inspect.getmembers` must ignore on a
/// protocol class (CPython 3.14's `_TYPING_INTERNALS | _SPECIAL_NAMES |
/// {'_MutableMapping__marker'}`).
const EXCLUDED_ATTRIBUTES: &[&str] = &[
    "__annotate__",
    "__annotate_func__",
    "__annotations__",
    "__annotations_cache__",
    "__abstractmethods__",
    "__class_getitem__",
    "__dict__",
    "__doc__",
    "__firstlineno__",
    "__init__",
    "__match_args__",
    "__module__",
    "__new__",
    "__non_callable_proto_members__",
    "__orig_bases__",
    "__orig_class__",
    "__parameters__",
    "__protocol_attrs__",
    "__slots__",
    "__static_attributes__",
    "__subclasshook__",
    "__type_params__",
    "__weakref__",
    "_MutableMapping__marker",
    "_is_protocol",
    "_is_runtime_protocol",
];

/// `typing._prohibited`: attributes `NamedTuple` class syntax rejects.
const PROHIBITED_ATTRIBUTES: &[&str] = &[
    "__getnewargs__",
    "__init__",
    "__new__",
    "__slots__",
    "_asdict",
    "_field_defaults",
    "_fields",
    "_make",
    "_replace",
    "_source",
];

/// `typing._ASSERT_NEVER_REPR_MAX_LENGTH`.
const ASSERT_NEVER_REPR_MAX_LENGTH: i64 = 100;

/// An identity-stable empty class cached under `typing.<name>`.
fn cached_class(name: &str) -> i64 {
    let key = format!("typing.{name}");
    if let Some(cached) = crate::core::globals::registry_get(&key) {
        return cached;
    }
    let class = if name == "_ConcatenateGenericAlias" {
        concatenate_generic_alias_class()
    } else {
        value::class_new(name.to_string(), NONE, Vec::new())
    };
    crate::core::globals::registry_set(&key, class);
    class
}

/// `typing._ConcatenateGenericAlias`: a real alias value, not an inert
/// placeholder.
///
/// CPython's class inherits `_GenericAlias`, whose constructor records the
/// origin and the argument tuple. Some `typing_extensions` releases also pass
/// `_typevar_types=(...)` and `_paramspec_tvars=True` as keyword-only
/// arguments; the runtime accepts and stores them so such construction
/// succeeds instead of raising `TypeError`. `__parameters__` is derived from
/// the arguments through the same collector used by
/// `_collect_type_vars`.
fn concatenate_generic_alias_class() -> i64 {
    let signature = vec![
        value::ParamMeta {
            name: "self".to_string(),
            kind: ParamKind::Positional,
            default: None,
        },
        value::ParamMeta {
            name: "origin".to_string(),
            kind: ParamKind::Positional,
            default: None,
        },
        value::ParamMeta {
            name: "args".to_string(),
            kind: ParamKind::Positional,
            default: None,
        },
        value::ParamMeta {
            name: "kwargs".to_string(),
            kind: ParamKind::KwArg,
            default: Some(NONE),
        },
    ];
    let init = value::native_function_new("typing", "_concatenate_generic_alias_init", usize::MAX);
    value::function_set_signature(init, signature);
    value::function_set_full_entry(
        init,
        concatenate_generic_alias_init_entry as *const () as usize,
    );
    value::class_new(
        "_ConcatenateGenericAlias".to_string(),
        NONE,
        vec![("__init__".to_string(), init)],
    )
}

/// Constructor body for [`concatenate_generic_alias_class`].
extern "C" fn concatenate_generic_alias_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the class-call binder passes `self`, `origin`, `args`, then the
    // `**kwargs` dict in signature order.
    let instance = unsafe { *argv };
    let origin = unsafe { *argv.add(1) };
    let args = unsafe { *argv.add(2) };
    let kwargs = unsafe { *argv.add(3) };
    let args_tuple = if let Some(items) = value::as_tuple(args) {
        value::tuple_new(items.to_vec())
    } else if let Some(items) = value::as_list(args) {
        value::tuple_new(items.to_vec())
    } else {
        value::tuple_new(vec![args])
    };
    let mut typevar_types = NONE;
    let mut paramspec_tvars = FALSE;
    if let Some(pairs) = value::as_dict(kwargs) {
        for (key, item) in pairs {
            let Some(keyword) = value::as_str(*key) else {
                return type_error("keyword names must be str");
            };
            match keyword {
                "_typevar_types" => typevar_types = *item,
                "_paramspec_tvars" => {
                    paramspec_tvars = value::bool_value(value::truthy(*item));
                }
                other => {
                    return type_error(&format!(
                        "_ConcatenateGenericAlias() got an unexpected keyword argument '{other}'"
                    ))
                }
            }
        }
    }
    let roots = heap::root_values(&[origin, args_tuple, typevar_types]);
    let parameters = collect_type_vars(args_tuple);
    let parameter_roots = heap::root_values(&[parameters]);
    value::instance_setattr(instance, "__origin__".to_string(), origin);
    value::instance_setattr(instance, "__args__".to_string(), args_tuple);
    value::instance_setattr(instance, "_typevar_types".to_string(), typevar_types);
    value::instance_setattr(instance, "_paramspec_tvars".to_string(), paramspec_tvars);
    value::instance_setattr(instance, "__parameters__".to_string(), parameters);
    heap::release_roots(parameter_roots);
    heap::release_roots(roots);
    NONE
}

/// A `frozenset`-shaped constant. Hydro has one set value type, so the
/// constant is a set built once and cached (membership/iteration are what
/// `typing_extensions` uses it for).
fn string_set(key: &str, names: &[&str]) -> i64 {
    let key = format!("typing.{key}");
    if let Some(cached) = crate::core::globals::registry_get(&key) {
        return cached;
    }
    let items: Vec<i64> = names
        .iter()
        .map(|name| value::str_alloc((*name).to_string()))
        .collect();
    let roots = heap::root_values(&items);
    let set = value::set_new(items);
    let set_roots = heap::root_values(&[set]);
    crate::core::globals::registry_set(&key, set);
    heap::release_roots(set_roots);
    heap::release_roots(roots);
    set
}

/// The cached `ParamSpec`/`TypeVarTuple` class. Both CPython classes store a
/// name plus keyword-only variance/default flags; the runtime keeps the same
/// attributes so annotations and `repr` see a usable placeholder.
fn typevar_like_class(name: &str) -> i64 {
    let key = format!("typing.{name}");
    if let Some(cached) = crate::core::globals::registry_get(&key) {
        return cached;
    }
    let signature = vec![
        value::ParamMeta {
            name: "self".to_string(),
            kind: ParamKind::Positional,
            default: None,
        },
        value::ParamMeta {
            name: "name".to_string(),
            kind: ParamKind::Positional,
            default: None,
        },
        value::ParamMeta {
            name: "kwargs".to_string(),
            kind: ParamKind::KwArg,
            default: Some(NONE),
        },
    ];
    let init = value::native_function_new("typing", "_typevar_like_init", usize::MAX);
    value::function_set_signature(init, signature);
    value::function_set_full_entry(init, typevar_like_init_entry as *const () as usize);
    let repr = value::native_function_new("typing", "_typevar_like_repr", usize::MAX);
    value::function_set_signature(
        repr,
        vec![value::ParamMeta {
            name: "self".to_string(),
            kind: ParamKind::Positional,
            default: None,
        }],
    );
    value::function_set_full_entry(repr, typevar_like_repr_entry as *const () as usize);
    let class = value::class_new(
        name.to_string(),
        NONE,
        vec![
            ("__init__".to_string(), init),
            ("__repr__".to_string(), repr),
        ],
    );
    crate::core::globals::registry_set(&key, class);
    class
}

/// `ParamSpec(name, *, bound=None, covariant=False, contravariant=False,
/// infer_variance=False, default=...)` / `TypeVarTuple(name, *, default=...)`.
extern "C" fn typevar_like_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the class-call binder passes `self`, `name`, then `**kwargs`.
    let instance = unsafe { *argv };
    let name = unsafe { *argv.add(1) };
    let kwargs = unsafe { *argv.add(2) };
    let Some(name_text) = value::as_str(name) else {
        return type_error("type parameter name must be a str");
    };
    let mut attributes: Vec<(String, i64)> = vec![
        (
            "__name__".to_string(),
            value::str_alloc(name_text.to_string()),
        ),
        ("__bound__".to_string(), NONE),
        ("__covariant__".to_string(), FALSE),
        ("__contravariant__".to_string(), FALSE),
        ("__infer_variance__".to_string(), FALSE),
        ("__default__".to_string(), NONE),
    ];
    if let Some(pairs) = value::as_dict(kwargs) {
        for (key, item) in pairs {
            let Some(keyword) = value::as_str(*key) else {
                return type_error("keyword names must be str");
            };
            match keyword {
                "bound" => attributes[1].1 = *item,
                "covariant" => attributes[2].1 = value::bool_value(value::truthy(*item)),
                "contravariant" => attributes[3].1 = value::bool_value(value::truthy(*item)),
                "infer_variance" => attributes[4].1 = value::bool_value(value::truthy(*item)),
                "default" => attributes[5].1 = *item,
                other => {
                    return type_error(&format!(
                        "{}() got an unexpected keyword argument '{other}'",
                        value::instance_class_name(instance).unwrap_or("type parameter")
                    ))
                }
            }
        }
    }
    let values: Vec<i64> = attributes.iter().map(|(_, item)| *item).collect();
    let roots = heap::root_values(&values);
    for (name, item) in &attributes {
        value::instance_setattr(instance, name.clone(), *item);
    }
    heap::release_roots(roots);
    NONE
}

/// `repr(ParamSpec('P')) == '~P'`.
fn typevar_like_repr(instance: i64) -> i64 {
    let name = value::instance_attr(instance, "__name__")
        .and_then(value::as_str)
        .unwrap_or("?");
    value::str_alloc(format!("~{name}"))
}

extern "C" fn typevar_like_repr_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self` first.
    typevar_like_repr(unsafe { *argv })
}

/// Materialises the module constant `name`.
pub(crate) fn constant(name: &str) -> i64 {
    if name == "TYPE_CHECKING" {
        return FALSE;
    }
    if name == "ForwardRef" {
        return forward_ref_class();
    }
    if name == "ParamSpec" || name == "TypeVarTuple" {
        return typevar_like_class(name);
    }
    if CLASSES.contains(&name) {
        return value::class_new(name.to_string(), NONE, Vec::new());
    }
    if CACHED_CLASSES.contains(&name) {
        return cached_class(name);
    }
    match name {
        "EXCLUDED_ATTRIBUTES" => string_set("EXCLUDED_ATTRIBUTES", EXCLUDED_ATTRIBUTES),
        "_prohibited" => string_set("_prohibited", PROHIBITED_ATTRIBUTES),
        "_ASSERT_NEVER_REPR_MAX_LENGTH" => {
            value::encode_int(ASSERT_NEVER_REPR_MAX_LENGTH).unwrap_or(NONE)
        }
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'typing' has no attribute '{name}'"),
        ),
    }
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
        "get_type_hints" => get_type_hints(args, &[]),
        "_get_defaults" => get_defaults(args.first().copied().unwrap_or(NONE)),
        "get_args" => alias_args(args.first().copied().unwrap_or(NONE)),
        "get_origin" => alias_origin(args.first().copied().unwrap_or(NONE)),
        // `@final` is a marker in this runtime: annotations are not enforced.
        "final" => args.first().copied().unwrap_or(NONE),
        "dataclass_transform" => {
            value::identity_function_new("typing.dataclass_transform.<locals>.decorator")
        }
        "ForwardRef" => new_forward_ref(args, &[]),
        "_deprecation_warning_for_no_type_params_passed" => no_type_params(args, &[]),
        "evaluate_forward_ref" => evaluate_forward_ref(args, &[]),
        "ParamSpec" | "TypeVarTuple" => new_typevar_like(function, args, &[]),
        "_tp_cache" => tp_cache(args, &[]),
        "_type_convert" => type_convert(args, &[]),
        "_type_check" => type_check(args, &[]),
        "_type_repr" => type_repr(args.first().copied().unwrap_or(NONE)),
        "_eval_type" => eval_type(args, &[]),
        "_is_param_expr" => value::bool_value(is_param_expr(args.first().copied().unwrap_or(NONE))),
        "_deduplicate" => deduplicate(args, &[]),
        "_generic_class_getitem" => generic_class_getitem(args),
        "_collect_type_vars" => collect_type_vars(args.first().copied().unwrap_or(NONE)),
        "_collect_parameters" => collect_type_vars(args.first().copied().unwrap_or(NONE)),
        "_check_generic" => NONE,
        "_overload_dummy" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "You should not call an overloaded function. A series of @overload-decorated \
             functions outside a stub module should always be followed by an implementation \
             that is not @overload-ed.",
        ),
        "_typevar_like_repr" => typevar_like_repr(args.first().copied().unwrap_or(NONE)),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'typing' has no attribute '{function}'"),
        ),
    }
}

/// Keyword-aware dispatch.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match function {
        "ForwardRef" => new_forward_ref(args, keywords),
        "_deprecation_warning_for_no_type_params_passed" => no_type_params(args, keywords),
        "evaluate_forward_ref" => evaluate_forward_ref(args, keywords),
        "ParamSpec" | "TypeVarTuple" => new_typevar_like(function, args, keywords),
        "_tp_cache" => tp_cache(args, keywords),
        "_type_convert" => type_convert(args, keywords),
        "_type_check" => type_check(args, keywords),
        "_eval_type" => eval_type(args, keywords),
        "_deduplicate" => deduplicate(args, keywords),
        "dataclass_transform" => {
            value::identity_function_new("typing.dataclass_transform.<locals>.decorator")
        }
        "get_type_hints" => get_type_hints(args, keywords),
        "_get_defaults" => get_defaults(args.first().copied().unwrap_or(NONE)),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'typing' has no attribute '{function}'"),
        ),
    }
}

/// Binds positional arguments plus keyword pairs for a fixed parameter list.
///
/// Returns `Err` with the pending `None` when a name is unknown/duplicated or
/// an argument is missing. `required` bounds how many leading parameters must
/// be present.
fn bind_parameters(
    function: &str,
    params: &[&str],
    required: usize,
    args: &[i64],
    keywords: &[(String, i64)],
) -> Result<Vec<Option<i64>>, i64> {
    if args.len() > params.len() {
        return Err(type_error(&format!(
            "{function}() takes at most {} arguments ({} given)",
            params.len(),
            args.len()
        )));
    }
    let mut slots: Vec<Option<i64>> = vec![None; params.len()];
    for (index, argument) in args.iter().enumerate() {
        slots[index] = Some(*argument);
    }
    for (name, item) in keywords {
        let Some(index) = params.iter().position(|param| param == name) else {
            return Err(type_error(&format!(
                "{function}() got an unexpected keyword argument '{name}'"
            )));
        };
        if slots[index].is_some() {
            return Err(type_error(&format!(
                "{function}() got multiple values for argument '{name}'"
            )));
        }
        slots[index] = Some(*item);
    }
    for (index, name) in params.iter().enumerate().take(required) {
        if slots[index].is_none() {
            return Err(type_error(&format!(
                "{function}() missing required argument '{name}'"
            )));
        }
    }
    Ok(slots)
}

/// `typing.ParamSpec(name, *, ...)` / `typing.TypeVarTuple(name, *, ...)`.
fn new_typevar_like(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let mut name = args.first().copied();
    if args.len() > 1 {
        return type_error(&format!(
            "{function}() takes 1 positional argument but {} were given",
            args.len()
        ));
    }
    let mut extra: Vec<(i64, i64)> = Vec::new();
    for (keyword, item) in keywords {
        if keyword == "name" {
            if name.is_some() {
                return type_error(&format!(
                    "{function}() got multiple values for argument 'name'"
                ));
            }
            name = Some(*item);
        } else {
            extra.push((value::str_alloc(keyword.clone()), *item));
        }
    }
    let Some(name) = name else {
        return type_error(&format!("{function}() missing required argument 'name'"));
    };
    let class = typevar_like_class(function);
    let kwargs = value::dict_new(extra);
    let instance = value::instance_new(class);
    let slots = [instance, name, kwargs];
    let roots = heap::root_values(&slots);
    let result = typevar_like_init_entry(slots.as_ptr());
    heap::release_roots(roots);
    if exc::pending() {
        return result;
    }
    instance
}

/// `typing._tp_cache(func=None, /, *, typed=False)`: identity decorator used
/// by the special-form classes.
fn tp_cache(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if args.len() > 1 {
        return type_error("_tp_cache() takes at most 1 argument");
    }
    for (name, _) in keywords {
        if name != "typed" {
            return type_error(&format!(
                "_tp_cache() got an unexpected keyword argument '{name}'"
            ));
        }
    }
    match args.first() {
        Some(function) => *function,
        None => value::identity_function_new("typing._tp_cache.<locals>.decorator"),
    }
}

/// `typing._type_convert(arg, module=None, *, allow_special_forms=False,
/// owner=None)`.
fn type_convert(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let params = ["arg", "module", "allow_special_forms", "owner"];
    let Ok(slots) = bind_parameters("_type_convert", &params, 1, args, keywords) else {
        return NONE;
    };
    convert_annotation(
        slots[0].unwrap_or(NONE),
        slots[1].unwrap_or(NONE),
        value::bool_value(value::truthy(slots[2].unwrap_or(NONE))),
    )
}

/// `typing._type_check(arg, msg, is_argument=True, module=None, *,
/// allow_special_forms=False, owner=None)`: converts `None`/strings like
/// CPython and otherwise returns the value unchanged (Hydro stores
/// annotations without evaluating them).
fn type_check(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let params = [
        "arg",
        "msg",
        "is_argument",
        "module",
        "allow_special_forms",
        "owner",
    ];
    let Ok(slots) = bind_parameters("_type_check", &params, 2, args, keywords) else {
        return NONE;
    };
    convert_annotation(
        slots[0].unwrap_or(NONE),
        slots[3].unwrap_or(NONE),
        value::bool_value(value::truthy(slots[4].unwrap_or(NONE))),
    )
}

/// `None` becomes `NoneType` and a string becomes a `ForwardRef`, matching
/// CPython's `_type_convert`.
fn convert_annotation(arg: i64, module: i64, allow_special_forms: i64) -> i64 {
    if arg == NONE {
        return crate::modules::types::constant("NoneType");
    }
    if value::as_str(arg).is_some() {
        let is_class = value::bool_value(value::truthy(allow_special_forms));
        return new_forward_ref(&[arg, module, NONE, TRUE, is_class], &[]);
    }
    arg
}

/// `typing._type_repr(obj)`: shortened rendering for types and functions.
fn type_repr(obj: i64) -> i64 {
    if let Some(name) = value::class_display_name(obj) {
        return value::str_alloc(name.to_string());
    }
    if let Some(function) = value::as_function(obj) {
        return value::str_alloc(function.name.clone());
    }
    value::str_alloc(value::repr(obj))
}

/// `typing.get_type_hints(obj, globalns=None, localns=None,
/// include_extras=False, *, format=None)`.
///
/// Hydro stores annotations only when a program (or a compatibility module)
/// assigns a function/class/module `__annotations__` mapping; the compiler
/// consumes annotations itself and keeps no payload. The helper therefore
/// reports the annotations that actually exist: for classes it merges the
/// MRO base-first (later definitions win, like CPython), for functions it
/// reads the assigned slot, and for modules it reads the module namespace.
/// String annotations are resolved through the supplied scopes when one is
/// given; without scope metadata the string is returned unchanged rather than
/// evaluated against a fabricated namespace. `include_extras` and `format`
/// are accepted for signature compatibility: Hydro does not model
/// `Annotated` stripping or annotationlib formats.
fn get_type_hints(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let params = ["obj", "globalns", "localns", "include_extras", "format"];
    let Ok(slots) = bind_parameters("get_type_hints", &params, 0, args, keywords) else {
        return NONE;
    };
    // Historical runtime behaviour (and the central spec's minimum): a call
    // with no object reports "no annotations" instead of inventing one.
    let Some(obj) = slots[0] else {
        return value::dict_new(Vec::new());
    };
    let globalns = slots[1].unwrap_or(NONE);
    let localns = slots[2].unwrap_or(NONE);
    let hints = value::dict_new(Vec::new());
    let roots = heap::root_values(&[hints, obj]);
    let mut merged: Vec<(String, i64)> = Vec::new();
    if let Some(state) = value::as_class(obj) {
        let mut chain = state.mro.clone();
        chain.reverse();
        chain.push(obj);
        for base in chain {
            if let Some(value::ClassEntry::Attr(annotations)) =
                value::class_lookup(base, "__annotations__")
            {
                if let Some(pairs) = value::as_dict(annotations) {
                    for (name, annotation) in pairs {
                        if let Some(name) = value::as_str(*name) {
                            merged.push((name.to_string(), *annotation));
                        }
                    }
                }
            }
        }
    } else if value::as_function(obj).is_some() {
        if let Some(annotations) = value::function_annotations(function_metadata_target(obj)) {
            if let Some(pairs) = value::as_dict(annotations) {
                for (name, annotation) in pairs {
                    if let Some(name) = value::as_str(*name) {
                        merged.push((name.to_string(), *annotation));
                    }
                }
            }
        }
    } else if let Some(module) = value::as_module(obj) {
        if let Some(annotations) = crate::core::globals::lookup(module.name(), "__annotations__") {
            if let Some(pairs) = value::as_dict(annotations) {
                for (name, annotation) in pairs {
                    if let Some(name) = value::as_str(*name) {
                        merged.push((name.to_string(), *annotation));
                    }
                }
            }
        }
    } else {
        let rendered = value::repr(obj);
        heap::release_roots(roots);
        return type_error(&format!("{rendered} does not have annotations"));
    }
    for (name, annotation) in merged {
        let resolved = resolve_hint(annotation, globalns, localns);
        if exc::pending() {
            heap::release_roots(roots);
            return NONE;
        }
        let key = value::str_alloc(name);
        let entry_roots = heap::root_values(&[key, resolved]);
        container::dict_set(hints, key, resolved);
        heap::release_roots(entry_roots);
    }
    heap::release_roots(roots);
    hints
}

/// Resolves one annotation value against the scopes `get_type_hints` was
/// given. CPython uses a single dict for both scopes when only one is passed.
fn resolve_hint(annotation: i64, globalns: i64, localns: i64) -> i64 {
    let Some(text) = value::as_str(annotation) else {
        return annotation;
    };
    if globalns == NONE && localns == NONE {
        // No scope metadata: a string annotation cannot be evaluated without a
        // compiler, so it is reported as written instead of guessed.
        return annotation;
    }
    let (globals, locals) = if globalns == NONE {
        (localns, localns)
    } else if localns == NONE {
        (globalns, globalns)
    } else {
        (globalns, localns)
    };
    match resolve_name(text, globals, locals) {
        Ok(Some(resolved)) => resolved,
        Ok(None) => annotation,
        Err(raised) => raised,
    }
}

/// Resolves a bound method to the function whose metadata backs its
/// `__code__`/`__defaults__`/`__annotations__` in CPython.
fn function_metadata_target(obj: i64) -> i64 {
    let Some(function) = value::as_function(obj) else {
        return obj;
    };
    let value::FunctionBody::BoundMethod { receiver, method } = &function.body else {
        return obj;
    };
    let class = value::instance_class_value(*receiver)
        .or_else(|| value::as_class(*receiver).map(|_| *receiver));
    let Some(class) = class else {
        return obj;
    };
    match value::class_lookup(class, method) {
        Some(value::ClassEntry::Method(_, target)) => target,
        Some(value::ClassEntry::Attr(target)) if value::as_function(target).is_some() => target,
        _ => obj,
    }
}

/// `typing._get_defaults(func)`: the function's default arguments by name.
///
/// CPython builds this from `__defaults__` and `__kwdefaults__`; Hydro keeps
/// the same information in every function value's parameter metadata, so
/// parameter names with a recorded default (positional, positional-only and
/// keyword-only alike) become the mapping. Callables without metadata report
/// an empty mapping instead of failing.
fn get_defaults(obj: i64) -> i64 {
    let obj = function_metadata_target(obj);
    let dict = value::dict_new(Vec::new());
    let roots = heap::root_values(&[dict, obj]);
    for param in value::function_signature(obj) {
        if !matches!(
            param.kind,
            ParamKind::Positional | ParamKind::PosOnly | ParamKind::KwOnly
        ) {
            continue;
        }
        let Some(default) = param.default else {
            continue;
        };
        let key = value::str_alloc(param.name.clone());
        let entry_roots = heap::root_values(&[key, default]);
        container::dict_set(dict, key, default);
        heap::release_roots(entry_roots);
    }
    heap::release_roots(roots);
    dict
}

/// `typing._eval_type(t, globalns, localns, ...)`: evaluates a `ForwardRef`
/// through the supplied namespaces; other annotations are returned unchanged.
fn eval_type(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let params = [
        "t",
        "globalns",
        "localns",
        "type_params",
        "recursive_guard",
        "format",
        "owner",
        "parent_fwdref",
        "prefer_fwd_module",
    ];
    let Ok(slots) = bind_parameters("_eval_type", &params, 1, args, keywords) else {
        return NONE;
    };
    let target = slots[0].unwrap_or(NONE);
    let Some(text) = forward_ref_text(target) else {
        return target;
    };
    let globals = slots[1].unwrap_or(NONE);
    let locals = slots[2].unwrap_or(NONE);
    match resolve_name(&text, globals, locals) {
        Ok(Some(value)) => value,
        Ok(None) => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            &format!(
                "typing._eval_type() cannot evaluate the forward reference {text:?} in Hydro: \
                 only plain (dotted) names supported by the runtime can be resolved"
            ),
        ),
        Err(error) => error,
    }
}

/// `typing._is_param_expr(arg)`.
fn is_param_expr(arg: i64) -> bool {
    if value::as_tuple(arg).is_some() || value::as_list(arg).is_some() {
        return true;
    }
    if value::instance_class_value(arg) == Some(typevar_like_class("ParamSpec")) {
        return true;
    }
    if value::instance_class_value(arg) == Some(cached_class("_ConcatenateGenericAlias")) {
        return true;
    }
    // `...` (the `Ellipsis` singleton) is also a parameter expression.
    let ellipsis = crate::modules::module_get_by_name("builtins", "Ellipsis");
    if !exc::pending() && arg == ellipsis {
        return true;
    }
    if exc::pending() {
        exc::clear();
    }
    false
}

/// `typing._deduplicate(params, *, unhashable_fallback=False)`:
/// `dict.fromkeys(params)` preserving first-occurrence order.
fn deduplicate(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    for (name, _) in keywords {
        if name != "unhashable_fallback" {
            return type_error(&format!(
                "_deduplicate() got an unexpected keyword argument '{name}'"
            ));
        }
    }
    if args.len() > 1 {
        return type_error("_deduplicate() takes 1 argument");
    }
    let params = args.first().copied().unwrap_or(NONE);
    let items: Vec<i64> = if let Some(items) = value::as_tuple(params) {
        items.to_vec()
    } else if let Some(items) = value::as_list(params) {
        items.to_vec()
    } else {
        return type_error(&format!(
            "'{}' object is not iterable",
            value::type_name(params)
        ));
    };
    let dict = value::dict_new(Vec::new());
    let roots = heap::root_values(&[dict]);
    let empty: &[(i64, i64)] = &[];
    for item in items {
        let entries = value::as_dict(dict).unwrap_or(empty);
        if container::dict_find(entries, item).is_none() {
            container::dict_set(dict, item, NONE);
            if exc::pending() {
                break;
            }
        }
    }
    heap::release_roots(roots);
    dict
}

/// `typing._generic_class_getitem(cls, args)`: builds a `_GenericAlias`
/// instance carrying `__origin__`/`__args__`.
fn generic_class_getitem(args: &[i64]) -> i64 {
    let cls = args.first().copied().unwrap_or(NONE);
    let parameters = args.get(1).copied().unwrap_or(NONE);
    let items: Vec<i64> = match value::as_tuple(parameters) {
        Some(items) => items.to_vec(),
        None => vec![parameters],
    };
    let class = cached_class("_GenericAlias");
    let origins = [cls, value::tuple_new(items), class];
    let roots = heap::root_values(&origins);
    let instance = value::instance_new(class);
    let args_tuple = origins[1];
    value::instance_setattr(instance, "__origin__".to_string(), cls);
    value::instance_setattr(instance, "__args__".to_string(), args_tuple);
    heap::release_roots(roots);
    instance
}

/// `typing._collect_type_vars`/`_collect_parameters`: flat collection of
/// `ParamSpec`/`TypeVarTuple` members, recursing into tuples and lists.
fn collect_type_vars(value_arg: i64) -> i64 {
    let mut collected: Vec<i64> = Vec::new();
    collect_type_vars_into(value_arg, &mut collected, 0);
    value::tuple_new(collected)
}

fn collect_type_vars_into(value_arg: i64, collected: &mut Vec<i64>, depth: usize) {
    if depth > 32 {
        return;
    }
    if let Some(items) = value::as_tuple(value_arg) {
        for item in items {
            collect_type_vars_into(*item, collected, depth + 1);
        }
        return;
    }
    if let Some(items) = value::as_list(value_arg) {
        for item in items {
            collect_type_vars_into(*item, collected, depth + 1);
        }
        return;
    }
    let class = value::instance_class_value(value_arg);
    if (class == Some(typevar_like_class("ParamSpec"))
        || class == Some(typevar_like_class("TypeVarTuple")))
        && !collected.contains(&value_arg)
    {
        collected.push(value_arg);
    }
}

/// `typing.get_args(tp)`: the alias arguments when the value carries
/// `__args__`, otherwise an empty tuple.
fn alias_args(value_arg: i64) -> i64 {
    if let Some(args) = value::instance_attr(value_arg, "__args__") {
        if value::as_tuple(args).is_some() {
            return args;
        }
    }
    value::tuple_new(Vec::new())
}

/// `typing.get_origin(tp)`: the `__origin__` of an alias, otherwise `None`.
fn alias_origin(value_arg: i64) -> i64 {
    if let Some(origin) = value::instance_attr(value_arg, "__origin__") {
        return origin;
    }
    NONE
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
            let class =
                as_class(constant(name)).unwrap_or_else(|| panic!("{name} must be a class value"));
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
            let class =
                as_class(constant(name)).unwrap_or_else(|| panic!("{name} must be a class value"));
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
        // A function with no assigned `__annotations__` reports no hints.
        let function = value::identity_function_new("f");
        let hints = call("get_type_hints", &[function]);
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

    #[test]
    fn new_typing_names_are_classes_and_cached_internals_are_stable() {
        exc::init();
        for name in [
            "AsyncContextManager",
            "AsyncGenerator",
            "Awaitable",
            "Coroutine",
            "AsyncIterable",
            "AsyncIterator",
            "OrderedDict",
            "Counter",
            "ChainMap",
            "Text",
            "Annotated",
            "Concatenate",
            "FrozenSet",
            "Self",
            "Required",
            "ReadOnly",
            "NotRequired",
            "Never",
            "LiteralString",
            "Unpack",
            "TypeIs",
            "TypeGuard",
            "TypeForm",
            "TypeAlias",
            "TypeAliasType",
            "TypedDict",
            "Doc",
            "SupportsAbs",
            "SupportsBytes",
            "SupportsComplex",
            "SupportsFloat",
            "SupportsIndex",
            "SupportsInt",
            "SupportsRound",
            "ParamSpecArgs",
            "ParamSpecKwargs",
        ] {
            let class =
                as_class(constant(name)).unwrap_or_else(|| panic!("{name} must be a class value"));
            assert_eq!(class.name, name);
        }
        for name in [
            "_Final",
            "_NotIterable",
            "_SpecialForm",
            "_SpecialFrom",
            "_BaseGenericAlias",
            "_GenericAlias",
            "_SpecialGenericAlias",
            "_UnionGenericAlias",
            "_UnionGenericAliasMeta",
            "_AnnotatedAlias",
            "_ConcatenateGenericAlias",
            "_ProtocolMeta",
            "_TypedDictMeta",
            "_TypeVarLikeMeta",
        ] {
            let first = constant(name);
            let second = constant(name);
            assert_eq!(first, second, "{name} identity");
            assert_eq!(
                as_class(first)
                    .unwrap_or_else(|| panic!("{name} must be a class value"))
                    .name,
                name
            );
        }
        assert_eq!(
            value::as_integer(constant("_ASSERT_NEVER_REPR_MAX_LENGTH")),
            Some(100)
        );
        let excluded = constant("EXCLUDED_ATTRIBUTES");
        assert!(value::as_set(excluded)
            .expect("set")
            .iter()
            .any(|item| value::as_str(*item) == Some("__type_params__")));
        assert!(!exc::pending());
    }

    #[test]
    fn paramspec_and_typevartuple_store_their_attributes() {
        exc::init();
        let param = call("ParamSpec", &[text("P")]);
        assert!(!exc::pending());
        assert_eq!(
            value::instance_class_value(param),
            Some(typevar_like_class("ParamSpec"))
        );
        assert_eq!(
            value::instance_attr(param, "__name__").and_then(value::as_str),
            Some("P")
        );
        assert_eq!(
            value::as_str(call("_typevar_like_repr", &[param])),
            Some("~P")
        );

        let tuple_var = call_kw(
            "TypeVarTuple",
            &[text("Ts")],
            &[("default".to_string(), text("unset"))],
        );
        assert_eq!(
            value::instance_attr(tuple_var, "__default__").and_then(value::as_str),
            Some("unset")
        );
        assert_eq!(
            value::instance_attr(tuple_var, "__covariant__"),
            Some(FALSE)
        );

        exc::init();
        call("ParamSpec", &[]);
        assert!(exc::pending());
        exc::clear();
        call_kw("ParamSpec", &[text("P")], &[("bogus".to_string(), NONE)]);
        assert!(exc::pending());
        exc::clear();
    }

    #[test]
    fn typing_internals_helpers_behave() {
        exc::init();
        // `_tp_cache` is an identity decorator.
        let function = text("f");
        assert_eq!(call("_tp_cache", &[function]), function);
        assert!(value::as_function(call("_tp_cache", &[])).is_some());

        // `_type_check`/`_type_convert` map None and strings like CPython.
        let none_type = call("_type_check", &[NONE, text("msg")]);
        assert_eq!(value::class_display_name(none_type), Some("NoneType"));
        let forward = call_kw(
            "_type_convert",
            &[text("int")],
            &[("module".to_string(), text("mod"))],
        );
        assert_eq!(
            value::instance_class_value(forward).and_then(value::class_display_name),
            Some("ForwardRef")
        );
        assert_eq!(
            value::instance_attr(forward, "__forward_module__").and_then(value::as_str),
            Some("mod")
        );

        // `_type_repr` shortens classes and functions.
        let class = constant("Any");
        assert_eq!(value::as_str(call("_type_repr", &[class])), Some("Any"));

        // `_overload_dummy` refuses to run.
        call("_overload_dummy", &[function]);
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), crate::exc::NOT_IMPLEMENTED_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn typing_parameter_helpers() {
        exc::init();
        // `get_origin`/`get_args` read alias metadata.
        let class = constant("Any");
        let parameters = value::tuple_new(vec![class]);
        let alias = call("_generic_class_getitem", &[class, parameters]);
        assert_eq!(call("get_origin", &[alias]), class);
        let args = as_tuple(call("get_args", &[alias])).expect("tuple");
        assert_eq!(args.to_vec(), vec![class]);

        // `_is_param_expr` accepts sequences and parameter expressions.
        assert_eq!(
            call("_is_param_expr", &[value::tuple_new(Vec::new())]),
            TRUE
        );
        assert_eq!(call("_is_param_expr", &[NONE]), FALSE);

        // `_deduplicate` keeps the first occurrence of each value.
        let ints = value::tuple_new(vec![
            value::encode_int(1).unwrap(),
            value::encode_int(1).unwrap(),
            value::encode_int(2).unwrap(),
        ]);
        let unique = as_dict(call("_deduplicate", &[ints])).expect("dict");
        assert_eq!(unique.len(), 2);
        assert_eq!(value::as_integer(unique[0].0), Some(1));
        assert_eq!(value::as_integer(unique[1].0), Some(2));

        // `_collect_type_vars` finds ParamSpec/TypeVarTuple members.
        let param = call("ParamSpec", &[text("P")]);
        let collected = as_tuple(call("_collect_type_vars", &[param])).expect("tuple");
        assert_eq!(collected.to_vec(), vec![param]);
        assert_eq!(call("_check_generic", &[class, parameters]), NONE);
        assert!(!exc::pending());
    }

    /// Looks up a string-keyed entry in a runtime dict.
    fn dict_entry(dict: i64, name: &str) -> Option<i64> {
        as_dict(dict)?
            .iter()
            .find_map(|(key, item)| (value::as_str(*key) == Some(name)).then_some(*item))
    }

    #[test]
    fn get_type_hints_reads_stored_annotations_and_supplied_scopes() {
        exc::init();
        let class = constant("Any");
        let function = value::identity_function_new("f");
        let annotated = value::dict_new(vec![(text("x"), class), (text("y"), text("int"))]);
        assert!(value::function_setattr(
            function,
            "__annotations__",
            annotated
        ));

        // Without scope metadata, concrete annotations are reported unchanged
        // and a string annotation stays a string instead of being guessed.
        let hints = call("get_type_hints", &[function]);
        assert!(!exc::pending());
        assert_eq!(dict_entry(hints, "x"), Some(class));
        assert_eq!(dict_entry(hints, "y").and_then(value::as_str), Some("int"));

        // One scope dict is used for both globals and locals, resolving names.
        let scope = value::dict_new(vec![(text("int"), class)]);
        let resolved = call_kw(
            "get_type_hints",
            &[function],
            &[("globalns".to_string(), scope)],
        );
        assert!(!exc::pending());
        assert_eq!(dict_entry(resolved, "y"), Some(class));

        // `include_extras` and `format` are accepted for signature parity.
        let extras = call_kw(
            "get_type_hints",
            &[function],
            &[
                ("globalns".to_string(), scope),
                ("include_extras".to_string(), TRUE),
                ("format".to_string(), NONE),
            ],
        );
        assert!(!exc::pending());
        assert_eq!(dict_entry(extras, "y"), Some(class));

        // A function with no annotations reports an empty mapping.
        let plain = value::identity_function_new("plain");
        let empty = call("get_type_hints", &[plain]);
        assert!(!exc::pending());
        assert_eq!(as_dict(empty).expect("dict").len(), 0);

        // Class annotations are read from the class namespace.
        let class_annotations = value::dict_new(vec![(text("field"), class)]);
        let annotated_class = value::class_new_full(
            "C".to_string(),
            NONE,
            Vec::new(),
            vec![("__annotations__".to_string(), class_annotations)],
        );
        let class_hints = call("get_type_hints", &[annotated_class]);
        assert!(!exc::pending());
        assert_eq!(dict_entry(class_hints, "field"), Some(class));

        // Unsupported objects fail like CPython instead of pretending to have
        // annotations.
        exc::init();
        call("get_type_hints", &[value::encode_int(3).unwrap()]);
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn get_defaults_maps_recorded_parameter_defaults() {
        exc::init();
        let two = value::encode_int(2).unwrap();
        let five = value::encode_int(5).unwrap();
        let function = value::identity_function_new("f");
        value::function_set_signature(
            function,
            vec![
                value::ParamMeta {
                    name: "a".to_string(),
                    kind: hydro_ir::ParamKind::Positional,
                    default: None,
                },
                value::ParamMeta {
                    name: "b".to_string(),
                    kind: hydro_ir::ParamKind::Positional,
                    default: Some(two),
                },
                value::ParamMeta {
                    name: "c".to_string(),
                    kind: hydro_ir::ParamKind::KwOnly,
                    default: Some(five),
                },
                value::ParamMeta {
                    name: "kw".to_string(),
                    kind: hydro_ir::ParamKind::KwArg,
                    default: Some(NONE),
                },
            ],
        );
        let defaults = call("_get_defaults", &[function]);
        assert!(!exc::pending());
        let entries = as_dict(defaults).expect("dict");
        assert_eq!(entries.len(), 2);
        assert_eq!(dict_entry(defaults, "b"), Some(two));
        assert_eq!(dict_entry(defaults, "c"), Some(five));
        assert_eq!(dict_entry(defaults, "a"), None);
        // Callables without parameter metadata report an empty mapping.
        let empty = call("_get_defaults", &[text("not a function")]);
        assert!(!exc::pending());
        assert_eq!(as_dict(empty).expect("dict").len(), 0);
    }

    #[test]
    fn concatenate_generic_alias_stores_origin_args_and_keyword_flags() {
        exc::init();
        let class = cached_class("_ConcatenateGenericAlias");
        let origin = constant("Callable");
        let parameter = call("ParamSpec", &[text("P")]);
        let arguments = value::tuple_new(vec![constant("Any"), parameter]);
        let typevar_types = value::tuple_new(vec![constant("Any")]);
        let alias = crate::builtins::call_kw(
            class,
            2,
            [origin, arguments].as_ptr(),
            2,
            [
                text("_typevar_types"),
                typevar_types,
                text("_paramspec_tvars"),
                TRUE,
            ]
            .as_ptr(),
        );
        assert!(!exc::pending());
        assert_eq!(
            value::instance_class_value(alias),
            Some(class),
            "construction uses the cached alias class"
        );
        assert_eq!(value::instance_attr(alias, "__origin__"), Some(origin));
        let stored = as_tuple(value::instance_attr(alias, "__args__").expect("__args__"))
            .expect("args tuple");
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[1], parameter);
        assert_eq!(
            value::instance_attr(alias, "_typevar_types"),
            Some(typevar_types)
        );
        assert_eq!(value::instance_attr(alias, "_paramspec_tvars"), Some(TRUE));
        // The parameters are the ParamSpec members of the arguments.
        let parameters =
            as_tuple(value::instance_attr(alias, "__parameters__").expect("__parameters__"))
                .expect("parameters tuple");
        assert_eq!(parameters.to_vec(), vec![parameter]);
        // Both `_is_param_expr` spellings recognize the constructed alias.
        assert_eq!(call("_is_param_expr", &[alias]), TRUE);
        assert_eq!(
            call("get_origin", &[alias]),
            origin,
            "get_origin reads the stored origin"
        );

        // Unknown keyword-only metadata is rejected, not ignored.
        exc::init();
        crate::builtins::call_kw(
            class,
            2,
            [origin, arguments].as_ptr(),
            1,
            [text("bogus"), NONE].as_ptr(),
        );
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        exc::clear();
    }
}
