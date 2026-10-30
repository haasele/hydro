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
//! The generic builtin classes (`tuple`, `list`, `dict`, `set`, `frozenset`
//! and `type`) are function values at runtime, so their subscription
//! (`tuple[int, ...]`, `list[str]`, `dict[str, int]`, `type[int]`) is served
//! by [`builtin_generic_subscript`]: it builds a `_GenericAlias` carrying
//! `__origin__`/`__args__` and rendering like CPython's `types.GenericAlias`
//! (`tuple[int, ...]`). Any other builtin value (`print[int]`) still raises
//! `TypeError`.
//!
//! `TypeVar`, `ParamSpec` and `TypeVarTuple` are real type-parameter objects:
//! each call allocates a fresh instance carrying the CPython attribute set
//! (`__name__`, `__constraints__`, `__bound__`, the variance flags,
//! `__default__`, `__module__`) and a `~name` representation. A
//! `TypeVarTuple` is iterable like CPython's, yielding `Unpack[Ts]`, so a
//! starred subscript in an alias value evaluates.
//!
//! `TypeAliasType` is a real class too: it backs PEP 695 `type X = expr`
//! statements. Lowering allocates one instance per statement with `__name__`,
//! `__value__`, `__type_params__`, `__parameters__` and `__module__`;
//! `repr` is the alias name, a generic alias is subscriptable, and
//! `types.UnionType` accepts an alias as a `|` member. The value is evaluated
//! eagerly (Hydro has no lazy annotation scope), which is the documented
//! limitation.
//!
//! `NewType` is a real class like CPython 3.10+: `typing.NewType` is the
//! class value (so `isinstance(x, typing.NewType)` works, which is what
//! `typing_inspection.is_newtype` does), and `NewType('UserId', int)` returns
//! a callable instance carrying `__name__`, `__qualname__`, `__supertype__`
//! and `__module__`. The instance call is the identity (`_typing._idfunc`),
//! and `typing_extensions.NewType` shares the class because it aliases the
//! module attribute.
//!
//! `no_type_check(arg)` is the identity decorator that stamps
//! `__no_type_check__` on a callable (recursively over a class's own methods
//! and nested classes) and returns it unchanged; native callables and scalars
//! reject the assignment with `AttributeError`, like CPython.
//! `no_type_check_decorator(decorator)` is the deprecated wrapper that applies
//! the same marking to another decorator's result (CPython removes it in
//! 3.15). `typing.__all__` is built from the central spec, so the public
//! export list tracks the module surface.
//!
//! PEP 695 generic classes work through the same machinery: a
//! `class C[T]:` statement stores its parameter tuple as `__type_params__` on
//! the class, and `C[int]` builds a `_GenericAlias` over the class through
//! [`generic_class_subscript`], so `get_origin`/`get_args`/`__parameters__`
//! and the alias repr agree with CPython. Class creation specializes such an
//! alias base to its origin (`class D(C[int])`), and a subclass inherits the
//! parameters like CPython's inherited `__class_getitem__`.
//!
//! `Literal[...]`, `Union[...]`, `Optional[...]` and `Annotated[...]` are
//! special forms with truthful runtime metadata, because pydantic and
//! `typing_inspection` introspect them:
//!
//! * `Literal[a, b].__args__` is the flattened, `(value, type)`-deduplicated
//!   tuple of literal values, nested `Literal` forms included;
//! * `Union[a, b]` follows CPython 3.14's unified `typing.Union` and builds a
//!   native union value (`a | b`) with flattened, deduplicated members;
//!   `Optional[T]` is `Union[T, None]`;
//! * `Annotated[T, m...]` is a `_GenericAlias` whose `__origin__` is `T`,
//!   `__args__` is `(T,)` and `__metadata__` is the flattened annotation
//!   tuple; `get_args` reports `(T, m...)`.
//!
//! All [`CLASSES`] placeholders are identity-stable per heap generation
//! ([`placeholder_class`]): `typing.Literal is typing.Literal` holds, which is
//! what `typing_extensions`/`typing_inspection` identity checks rely on.
//! `ForwardRef`, `TypeAliasType`, `TypeVar`, `ParamSpec` and `TypeVarTuple`
//! are cached the same way because `isinstance` checks and construction must
//! share one identity.
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

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use hydro_ir::{Builtin, ParamKind};

use crate::container;
use crate::exc::{
    self, ASSERTION_ERROR, ATTRIBUTE_ERROR, DEPRECATION_WARNING, NAME_ERROR, NOT_IMPLEMENTED_ERROR,
    TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::methods;
use crate::value::{self, FALSE, NONE, TRUE};

/// `@overload` stubs registered for `typing.get_overloads`.
///
/// CPython keys the registry by `(filename, qualname, firstlineno)` from the
/// decorated function's code object. Hydro functions carry no code provenance,
/// so the key is `(__module__ if assigned, __qualname__)`; overloads and their
/// implementation share the definition name, which is exactly the lookup
/// `get_overloads(implementation)` performs. Entries are rooted so the
/// collector cannot free a stub that only the registry names, and the
/// generation tag discards values from an earlier heap.
struct OverloadRegistry {
    generation: u64,
    entries: HashMap<(String, String), Vec<i64>>,
}

static OVERLOADS: LazyLock<Mutex<OverloadRegistry>> = LazyLock::new(|| {
    Mutex::new(OverloadRegistry {
        generation: 0,
        entries: HashMap::new(),
    })
});

/// Locks the registry, dropping entries that name a previous heap generation.
fn overload_registry() -> std::sync::MutexGuard<'static, OverloadRegistry> {
    let mut registry = OVERLOADS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let generation = heap::generation();
    if registry.generation != generation {
        registry.entries.clear();
        registry.generation = generation;
    }
    registry
}

/// Registry key of a function value: its assigned `__module__` plus
/// `__qualname__` (the definition name when no qualname was assigned).
fn overload_key(function: i64) -> (String, String) {
    let module = value::function_module(function)
        .and_then(value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    (module, value::function_qualname(function))
}

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
        // `ParamSpec`, `TypeVar` and `TypeVarTuple` are declared as module
        // functions so calls lower as module calls, but the attribute value
        // itself is the cached class (`isinstance`, `from typing import
        // ParamSpec` and subclassing all need it).
        "ParamSpec" | "TypeVarTuple" => Some(typevar_like_class(name)),
        "TypeVar" => Some(typevar_class()),
        // `NewType` is declared as a module function so calls lower as module
        // calls, but the attribute value is the real class: CPython 3.10+
        // makes `typing.NewType` a class and `typing_inspection.is_newtype`
        // (pydantic) is an `isinstance` check against it.
        "NewType" => Some(newtype_class()),
        // The call form is declared as a module function so keyword calls
        // lower as module calls, but the attribute value must stay the real
        // class: `isinstance(x, typing._ConcatenateGenericAlias)` and
        // `super(typing._ConcatenateGenericAlias, self)` both need it.
        "_ConcatenateGenericAlias" => Some(cached_class("_ConcatenateGenericAlias")),
        _ => None,
    }
}

/// Generation-tagged cache for the native `NewType` class.
struct NewTypeState {
    generation: u64,
    class: i64,
}

static NEWTYPE: LazyLock<Mutex<Option<NewTypeState>>> = LazyLock::new(|| Mutex::new(None));

/// A native method function with a declared signature and a full-entry
/// trampoline, the shape the shared binder dispatches (`call_bound_entry`
/// checks `function_signature`/`function_full_entry`). The function name is
/// internal, so it needs no module spec.
fn entry_method(name: &'static str, parameters: &[&str], entry: usize) -> i64 {
    let signature = parameters
        .iter()
        .map(|parameter| value::ParamMeta {
            name: (*parameter).to_string(),
            kind: ParamKind::Positional,
            default: None,
        })
        .collect();
    let function = value::native_function_new("typing", name, usize::MAX);
    value::function_set_signature(function, signature);
    value::function_set_full_entry(function, entry);
    function
}

/// `typing.NewType` as a real class value, like CPython 3.10+.
///
/// The class is what `isinstance(x, typing.NewType)` needs (pydantic's
/// `typing_inspection.is_newtype`); construction returns an instance carrying
/// `__name__`, `__qualname__`, `__supertype__` and `__module__`, and the
/// instance stays callable as the identity function, matching CPython's
/// `_typing._idfunc`.
fn newtype_class() -> i64 {
    let generation = heap::generation();
    {
        let guard = NEWTYPE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(state) = guard.as_ref() {
            if state.generation == generation {
                heap::add_thread_root(state.class);
                return state.class;
            }
        }
    }
    let init = entry_method(
        "_newtype_init",
        &["self", "name", "tp"],
        init_newtype as *const () as usize,
    );
    let call = entry_method(
        "_newtype_call",
        &["self", "x"],
        call_newtype as *const () as usize,
    );
    let repr = entry_method(
        "_newtype_repr",
        &["self"],
        repr_newtype as *const () as usize,
    );
    let roots = heap::root_values(&[init, call, repr]);
    let class = value::class_new(
        "NewType".to_string(),
        NONE,
        vec![
            ("__init__".to_string(), init),
            ("__call__".to_string(), call),
            ("__repr__".to_string(), repr),
        ],
    );
    heap::release_roots(roots);
    heap::add_thread_root(class);
    *NEWTYPE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some(NewTypeState { generation, class });
    class
}

/// `NewType(name, tp)`: the shared construction behind the class call (which
/// runs `__init__` through the binder) and the module-call dispatch.
fn new_newtype(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if args.len() > 2 {
        return type_error(&format!(
            "NewType() takes at most 2 positional arguments ({} given)",
            args.len()
        ));
    }
    let mut name = args.first().copied();
    let mut supertype = args.get(1).copied();
    for (keyword, item) in keywords {
        match keyword.as_str() {
            "name" => {
                if name.is_some() {
                    return type_error("NewType() got multiple values for argument 'name'");
                }
                name = Some(*item);
            }
            "tp" => {
                if supertype.is_some() {
                    return type_error("NewType() got multiple values for argument 'tp'");
                }
                supertype = Some(*item);
            }
            other => {
                return type_error(&format!(
                    "NewType() got an unexpected keyword argument '{other}'"
                ))
            }
        }
    }
    let Some(name) = name else {
        return type_error("NewType() missing required argument 'name' (pos 1)");
    };
    let Some(supertype) = supertype else {
        return type_error("NewType() missing required argument 'tp' (pos 2)");
    };
    // The caller may pass a Rust-side buffer (a module-call dispatcher or a
    // unit test), which the conservative collector does not scan.
    let roots = heap::root_values(&[name, supertype]);
    let class = newtype_class();
    let class_roots = heap::root_values(&[class]);
    let instance = value::instance_new(class);
    let instance_roots = heap::root_values(&[instance]);
    let status = populate_newtype(instance, name, supertype);
    heap::release_roots(instance_roots);
    heap::release_roots(class_roots);
    heap::release_roots(roots);
    if status != NONE || exc::pending() {
        NONE
    } else {
        instance
    }
}

/// Native `NewType.__init__(self, name, tp)`: stores the CPython attribute
/// set. `__module__` is the defining frame's module (CPython's `_caller()`),
/// falling back to `__main__` when no generated activation is on the stack.
extern "C" fn init_newtype(argv: *const i64) -> i64 {
    // SAFETY: the class-call binder passes one slot per declared parameter.
    let instance = unsafe { *argv };
    let name = unsafe { *argv.add(1) };
    let supertype = unsafe { *argv.add(2) };
    populate_newtype(instance, name, supertype)
}

fn populate_newtype(instance: i64, name: i64, supertype: i64) -> i64 {
    let Some(text) = value::as_str(name) else {
        return type_error(&format!(
            "NewType() argument 1 must be a string, not {}",
            value::type_name(name)
        ));
    };
    let roots = heap::root_values(&[instance, name, supertype]);
    let qualname = value::str_alloc(text.to_string());
    let qualname_roots = heap::root_values(&[qualname]);
    // CPython keeps the last dotted component as `__name__` and the full
    // string as `__qualname__`.
    let short = text.rsplit('.').next().unwrap_or(text);
    let short_name = value::str_alloc(short.to_string());
    let short_roots = heap::root_values(&[short_name]);
    let module = exc::frame_module_at(0).unwrap_or_else(|| "__main__".to_string());
    let module = value::str_alloc(module);
    value::instance_setattr(instance, "__qualname__".to_string(), qualname);
    value::instance_setattr(instance, "__name__".to_string(), short_name);
    value::instance_setattr(instance, "__supertype__".to_string(), supertype);
    value::instance_setattr(instance, "__module__".to_string(), module);
    heap::release_roots(short_roots);
    heap::release_roots(qualname_roots);
    heap::release_roots(roots);
    NONE
}

/// Native `NewType.__call__(self, x)`: CPython's `_idfunc`, the identity.
extern "C" fn call_newtype(argv: *const i64) -> i64 {
    // SAFETY: the binder passes `self` followed by the one declared argument.
    unsafe { *argv.add(1) }
}

/// Native `NewType.__repr__(self)`: `<module>.<qualname>`.
extern "C" fn repr_newtype(argv: *const i64) -> i64 {
    // SAFETY: the binder passes `self` first.
    let instance = unsafe { *argv };
    let module = value::instance_attr(instance, "__module__")
        .and_then(value::as_str)
        .unwrap_or("__main__");
    let qualname = value::instance_attr(instance, "__qualname__")
        .and_then(value::as_str)
        .unwrap_or("NewType");
    value::str_alloc(format!("{module}.{qualname}"))
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
    let slots = [
        "__forward_module__",
        "__forward_arg__",
        "__forward_code__",
        "__forward_evaluated__",
        "__forward_value__",
        "__forward_is_argument__",
        "__forward_is_class__",
        "__forward_module__",
    ]
    .map(|name| value::str_alloc(name.to_string()));
    let slots = value::tuple_new(slots.to_vec());
    value::class_set_attr(class, "__slots__".to_string(), slots);
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
    "Hashable",
    "Reversible",
    "Sized",
    "Container",
    "AbstractSet",
    "MutableSet",
    "MutableMapping",
    "MutableSequence",
    "MappingView",
    "KeysView",
    "ItemsView",
    "ValuesView",
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
    let class = match name {
        "_ConcatenateGenericAlias" => concatenate_generic_alias_class(),
        "_GenericAlias" => generic_alias_class(),
        _ => value::class_new(name.to_string(), NONE, Vec::new()),
    };
    crate::core::globals::registry_set(&key, class);
    class
}

/// One typing placeholder from [`CLASSES`], identity-stable per heap
/// generation.
///
/// CPython exposes `typing.Literal` and friends as module singletons, and
/// `typing_inspection`/pydantic compare `get_origin(alias)` against the module
/// attribute with `is` (`typing_objects.is_literal`, `is_union`,
/// `is_annotated`). Allocating a fresh class per attribute read made those
/// identity checks false, so the placeholder is cached in the runtime
/// registry (a process-wide GC root) and tagged with the heap generation:
/// `hydro_runtime_init`/`heap::reset` free the heap, and a stale cached class
/// from the previous run must never be handed out again.
fn placeholder_class(name: &str) -> i64 {
    let key = format!("typing.placeholder.{name}");
    let generation_key = format!("{key}.generation");
    let generation =
        value::encode_int(i64::try_from(heap::generation()).unwrap_or(i64::MAX)).unwrap_or(NONE);
    if crate::core::globals::registry_get(&generation_key) == Some(generation) {
        if let Some(class) = crate::core::globals::registry_get(&key) {
            return class;
        }
    }
    let class = value::class_new(name.to_string(), NONE, Vec::new());
    let roots = heap::root_values(&[class]);
    crate::core::globals::registry_set(&key, class);
    crate::core::globals::registry_set(&generation_key, generation);
    heap::release_roots(roots);
    class
}

/// `typing._GenericAlias`: an alias value carrying `__origin__`/`__args__`
/// and rendering like CPython's aliases (`tuple[int, ...]`, `Vector[int]`).
///
/// The same class backs builtin generic subscriptions (`list[str]`,
/// `dict[str, int]`, `type[int]`); see [`builtin_generic_subscript`].
fn generic_alias_class() -> i64 {
    let render = |name: &'static str| {
        let function = value::native_function_new("typing", name, usize::MAX);
        value::function_set_signature(
            function,
            vec![value::ParamMeta {
                name: "self".to_string(),
                kind: ParamKind::Positional,
                default: None,
            }],
        );
        value::function_set_full_entry(function, generic_alias_repr_entry as *const () as usize);
        function
    };
    value::class_new(
        "_GenericAlias".to_string(),
        NONE,
        vec![
            // `str(list[int])` and `repr(list[int])` are both `list[int]`,
            // like CPython, so interpolation and `print` agree.
            ("__repr__".to_string(), render("_generic_alias_repr")),
            ("__str__".to_string(), render("_generic_alias_str")),
        ],
    )
}

/// Body of `_GenericAlias.__repr__`/`__str__`: `argv[0]` is the alias.
extern "C" fn generic_alias_repr_entry(argv: *const i64) -> i64 {
    // SAFETY: the class-call binder passes `self` first (declared signature).
    let instance = unsafe { *argv };
    value::str_alloc(generic_alias_text(instance, 0))
}

/// Nesting rendered by [`generic_alias_text`] before the remaining arguments
/// are elided as `...`; guards against pathological alias graphs.
const MAX_ALIAS_REPR_DEPTH: usize = 16;

/// The metadata tuple of an `Annotated[...]` alias, or `None`.
///
/// Only Hydro's `_GenericAlias` carries `__metadata__`, so a user object with
/// a same-named attribute is not mistaken for an annotated alias.
fn annotated_metadata(v: i64) -> Option<&'static [i64]> {
    let metadata = value::instance_attr(v, "__metadata__")?;
    if value::instance_class_value(v)? != cached_class("_GenericAlias") {
        return None;
    }
    value::as_tuple(metadata)
}

/// Renders an alias as `origin[arg, ...]`, or just `origin` without
/// arguments.
fn generic_alias_text(instance: i64, depth: usize) -> String {
    // An `Annotated[...]` alias stores the annotated type in `__origin__`,
    // so its repr puts the origin back inside the brackets followed by the
    // metadata values.
    if let Some(metadata) = annotated_metadata(instance) {
        let origin = value::instance_attr(instance, "__origin__").unwrap_or(NONE);
        let mut rendered = String::from("Annotated[");
        rendered.push_str(&alias_arg_text(origin, depth + 1));
        for item in metadata {
            rendered.push_str(", ");
            rendered.push_str(&value::repr(*item));
        }
        rendered.push(']');
        return rendered;
    }
    let origin = value::instance_attr(instance, "__origin__").unwrap_or(NONE);
    let origin_text = alias_origin_text(origin);
    let Some(args) = value::instance_attr(instance, "__args__").and_then(value::as_tuple) else {
        return origin_text;
    };
    if args.is_empty() {
        return origin_text;
    }
    let mut rendered = String::with_capacity(origin_text.len() + 2);
    rendered.push_str(&origin_text);
    rendered.push('[');
    for (position, arg) in args.iter().enumerate() {
        if position > 0 {
            rendered.push_str(", ");
        }
        rendered.push_str(&alias_arg_text(*arg, depth + 1));
    }
    rendered.push(']');
    rendered
}

/// Name of an alias origin: a class or builtin name, otherwise the origin's
/// own `repr` (a PEP 695 alias renders as its name).
fn alias_origin_text(origin: i64) -> String {
    if let Some(name) = value::class_display_name(origin) {
        return name.to_string();
    }
    if let Some(function) = value::as_function(origin) {
        return function.name.clone();
    }
    value::repr(origin)
}

/// One alias argument, spelled the way CPython's `types.GenericAlias` repr
/// does: type values by name (`int`, not `<class 'int'>`), nested aliases
/// recursively, `...` for the variadic parameter, and `repr` otherwise.
fn alias_arg_text(arg: i64, depth: usize) -> String {
    if depth > MAX_ALIAS_REPR_DEPTH {
        return "...".to_string();
    }
    if value::instance_class_value(arg) == Some(cached_class("_GenericAlias")) {
        return generic_alias_text(arg, depth);
    }
    if let Some(name) = value::class_display_name(arg) {
        return name.to_string();
    }
    if let Some(function) = value::as_function(arg) {
        return function.name.clone();
    }
    // Hydro materialises the `...` literal as `None` (see
    // `Const::Ellipsis`), so the variadic parameter arrives as `None`; it is
    // the only `None` a type argument carries in practice, and CPython spells
    // that slot `...`.
    if arg == NONE {
        return "...".to_string();
    }
    value::repr(arg)
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

/// Module-call form of `typing._ConcatenateGenericAlias(origin, args, *,
/// _typevar_types, _paramspec_tvars)`.
///
/// `typing_extensions` reaches the class through the module (`typing._ConcatenateGenericAlias`)
/// and through its local alias, so the keyword-bearing call must work when it
/// lowers as a module call as well. The native class call path binds the
/// arguments, so both spellings share one validation: the `__init__` body
/// above accepts the two keyword-only metadata names and rejects everything
/// else with CPython's `TypeError`.
fn concatenate_generic_alias_new(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let class = cached_class("_ConcatenateGenericAlias");
    let class_roots = heap::root_values(&[class]);
    // Keyword names are freshly allocated strings; root each one before the
    // next allocation can trigger a collection.
    let mut words: Vec<i64> = Vec::with_capacity(keywords.len() * 2);
    let mut name_roots: Vec<usize> = Vec::with_capacity(keywords.len());
    for (name, keyword) in keywords {
        let key = value::str_alloc(name.clone());
        name_roots.push(heap::root_values(&[key]));
        words.push(key);
        words.push(*keyword);
    }
    let result = crate::builtins::call_kw(
        class,
        args.len(),
        args.as_ptr(),
        keywords.len(),
        words.as_ptr(),
    );
    for start in name_roots.into_iter().rev() {
        heap::release_roots(start);
    }
    heap::release_roots(class_roots);
    result
}

/// `typing.overload(func)`: records an overload stub and returns the callable.
///
/// CPython replaces the decorated function with an `_overload_dummy`
/// placeholder; Hydro returns the function itself so a direct call still
/// works, while the registry (the observable state) matches CPython's.
fn overload(func: i64) -> i64 {
    if value::as_function(func).is_some() {
        let key = overload_key(func);
        // The registry is Rust-side memory the collector cannot scan; keep the
        // stub alive exactly like a module global.
        heap::add_thread_root(func);
        overload_registry()
            .entries
            .entry(key)
            .or_default()
            .push(func);
    }
    func
}

/// `typing.get_overloads(func)`: the stubs registered for `func`'s name.
fn get_overloads(func: i64) -> i64 {
    if value::as_function(func).is_none() {
        return exc::raise_with_message(TYPE_ERROR, "get_overloads() argument must be a function");
    }
    let key = overload_key(func);
    let entries = {
        let registry = overload_registry();
        registry.entries.get(&key).cloned().unwrap_or_default()
    };
    value::list_new(entries)
}

/// `typing.clear_overloads()`: forgets every registered stub.
///
/// The stubs' conservative roots stay installed (the globals store keeps
/// replaced values rooted too), so this can never free a function a compiled
/// module still names.
fn clear_overloads() -> i64 {
    overload_registry().entries.clear();
    NONE
}

/// `typing.disjoint_base(cls)` (PEP 800): a marker decorator.
///
/// CPython 3.15 records a flag on the class so the compiler can reject
/// incompatible diamond inheritance; Hydro has no such compiler flag, and the
/// decorator returns the class unchanged, as both CPython's implementation and
/// `typing_extensions`' fallback do.
fn disjoint_base(cls: i64) -> i64 {
    if value::as_class(cls).is_none() {
        return type_error("disjoint_base() argument must be a class");
    }
    cls
}

/// `typing.NoDefault` / `typing.NoExtraItems`: the CPython 3.13+ singleton
/// sentinels. Each is an instance of a cached `*Type` class whose `__repr__`
/// is the module-qualified spelling, so identity comparisons and the
/// introspection pydantic/typing_extensions perform agree with CPython.
fn sentinel(which: &str) -> i64 {
    let key = format!("typing.{which}");
    if let Some(cached) = crate::core::globals::registry_get(&key) {
        return cached;
    }
    let class_key = format!("typing.{which}Type");
    let class = match crate::core::globals::registry_get(&class_key) {
        Some(class) => class,
        None => {
            let (function_name, entry): (&'static str, extern "C" fn(*const i64) -> i64) =
                match which {
                    "NoDefault" => ("_no_default_repr", no_default_repr_entry),
                    _ => ("_no_extra_items_repr", no_extra_items_repr_entry),
                };
            let repr = value::native_function_new("typing", function_name, 1);
            value::function_set_signature(
                repr,
                vec![value::ParamMeta {
                    name: "self".to_string(),
                    kind: ParamKind::Positional,
                    default: None,
                }],
            );
            value::function_set_full_entry(repr, entry as *const () as usize);
            let roots = heap::root_values(&[repr]);
            let class = value::class_new(
                format!("{which}Type"),
                NONE,
                vec![("__repr__".to_string(), repr)],
            );
            let class_roots = heap::root_values(&[class]);
            crate::core::globals::registry_set(&class_key, class);
            heap::release_roots(class_roots);
            heap::release_roots(roots);
            class
        }
    };
    let instance = value::instance_new(class);
    let roots = heap::root_values(&[instance]);
    crate::core::globals::registry_set(&key, instance);
    heap::release_roots(roots);
    instance
}

extern "C" fn no_default_repr_entry(_argv: *const i64) -> i64 {
    value::str_alloc("typing.NoDefault".to_string())
}

extern "C" fn no_extra_items_repr_entry(_argv: *const i64) -> i64 {
    value::str_alloc("typing.NoExtraItems".to_string())
}

/// Whether a class (or an ancestor) is named `name`. Hydro represents
/// `Protocol`/`TypedDict` as native placeholder classes, so the ancestry is
/// the available runtime marker.
fn class_chain_contains(class: i64, name: &str) -> bool {
    value::class_mro(class)
        .iter()
        .any(|&current| value::as_class(current).is_some_and(|state| state.name == name))
}

/// `typing.is_typeddict(tp)`: a class whose ancestry reaches the TypedDict
/// placeholder. Non-classes are always false, like `isinstance(tp, _TypedDictMeta)`.
pub(crate) fn is_typeddict_class(tp: i64) -> bool {
    is_typeddict(tp)
}

fn is_typeddict(tp: i64) -> bool {
    if value::as_class(tp).is_none() {
        return false;
    }
    // Compare the *visible* name: `_TypedDictMeta` renames every class it
    // creates, so the frozen placeholder name `"Protocol"` never identifies
    // the result.
    value::class_display_name(tp) != Some("TypedDict")
        && (class_chain_contains(tp, "TypedDict")
            || class_chain_contains(tp, "_TypedDict")
            || value::class_lookup(tp, "__required_keys__").is_some()
            || value::class_lookup(tp, "__optional_keys__").is_some())
}

/// `typing.is_protocol(tp)`: a class marked `_is_protocol` (typing_extensions
/// sets the marker explicitly) or one deriving from `Protocol`. `Protocol`
/// itself is not a protocol.
fn is_protocol(tp: i64) -> bool {
    if value::as_class(tp).is_none() {
        return false;
    }
    if value::class_display_name(tp) == Some("Protocol") {
        return false;
    }
    if let Some(value::ClassEntry::Attr(marker)) = value::class_lookup(tp, "_is_protocol") {
        return value::truthy(marker);
    }
    class_chain_contains(tp, "Protocol")
}

/// `typing.get_protocol_members(tp)`: the names a protocol declares, walked
/// over the MRO while skipping `Protocol`/`Generic`/`object` and the special
/// attributes CPython excludes. Hydro has one set value type, so the result
/// is a set (the observable behaviour of CPython's frozenset).
fn get_protocol_members(tp: i64) -> i64 {
    if !is_protocol(tp) {
        return type_error(&format!("{} is not a Protocol", value::repr(tp)));
    }
    let mut names: Vec<&str> = Vec::new();
    for &current in &value::class_mro(tp) {
        let Some(state) = value::as_class(current) else {
            continue;
        };
        if matches!(state.name.as_str(), "Protocol" | "Generic" | "object") {
            continue;
        }
        for (name, _, _) in &state.methods {
            if !name.starts_with("_abc_") && !EXCLUDED_ATTRIBUTES.contains(&name.as_str()) {
                names.push(name.as_str());
            }
        }
        for (name, _) in &state.attrs {
            if !name.starts_with("_abc_") && !EXCLUDED_ATTRIBUTES.contains(&name.as_str()) {
                names.push(name.as_str());
            }
        }
    }
    names.sort_unstable();
    names.dedup();
    let items: Vec<i64> = names
        .into_iter()
        .map(|name| value::str_alloc(name.to_string()))
        .collect();
    let roots = heap::root_values(&items);
    let set = value::set_new(items);
    heap::release_roots(roots);
    set
}

/// `@override`: identity decorator that records `__override__ = True` when the
/// target has a writable attribute surface (functions and classes do; C-level
/// callables/instances may not, exactly like CPython's silent skip).
fn override_marker(method: i64) -> i64 {
    let _ = methods::setattr(method, b"__override__".as_ptr(), "__override__".len(), TRUE);
    if exc::pending() {
        exc::clear();
    }
    method
}

/// Name stamped by `no_type_check`.
const NO_TYPE_CHECK_ATTRIBUTE: &str = "__no_type_check__";

/// Depth guard for [`stamp_no_type_check_members`]: a class attribute may
/// reference the class itself, and CPython's `dir()`-style walk must
/// terminate.
const MAX_NO_TYPE_CHECK_DEPTH: usize = 16;

/// `typing.no_type_check(arg)`: marks a callable as exempt from type checking
/// and returns it unchanged.
///
/// CPython sets `arg.__no_type_check__ = True`; for a class it first walks the
/// class's own members (directly defined functions and nested classes) and
/// stamps those. Native callables and scalars reject the assignment with
/// `AttributeError`, exactly as CPython does (its `except TypeError` does not
/// cover that).
fn no_type_check_call(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let mut target = args.first().copied();
    for (keyword, item) in keywords {
        if keyword != "arg" {
            return type_error(&format!(
                "no_type_check() got an unexpected keyword argument '{keyword}'"
            ));
        }
        if target.is_some() {
            return type_error("no_type_check() got multiple values for argument 'arg'");
        }
        target = Some(*item);
    }
    let Some(target) = target else {
        return type_error("no_type_check() missing required argument 'arg' (pos 1)");
    };
    no_type_check_value(target)
}

fn no_type_check_value(target: i64) -> i64 {
    let roots = heap::root_values(&[target]);
    if value::as_class(target).is_some() {
        stamp_no_type_check_members(target, 0);
        if exc::pending() {
            heap::release_roots(roots);
            return NONE;
        }
        value::class_set_attr(target, NO_TYPE_CHECK_ATTRIBUTE.to_string(), TRUE);
        heap::release_roots(roots);
        return target;
    }
    let _ = methods::setattr(
        target,
        NO_TYPE_CHECK_ATTRIBUTE.as_ptr(),
        NO_TYPE_CHECK_ATTRIBUTE.len(),
        TRUE,
    );
    let pending = exc::pending();
    heap::release_roots(roots);
    if pending {
        NONE
    } else {
        target
    }
}

/// Stamps `__no_type_check__` on a class's own namespace (its methods,
/// function attributes and nested classes). Inherited members are untouched,
/// matching CPython's direct-definition filter.
fn stamp_no_type_check_members(class: i64, depth: usize) {
    if depth > MAX_NO_TYPE_CHECK_DEPTH {
        return;
    }
    for (_, item) in value::class_namespace(class) {
        stamp_no_type_check_member(item, depth);
    }
}

fn stamp_no_type_check_member(item: i64, depth: usize) {
    if value::as_class(item).is_some() {
        // CPython recurses with the full decorator, so the nested class is
        // stamped as well as its own members.
        stamp_no_type_check_members(item, depth + 1);
        value::class_set_attr(item, NO_TYPE_CHECK_ATTRIBUTE.to_string(), TRUE);
        return;
    }
    if value::as_function(item).is_some() {
        stamp_no_type_check_function(item);
        return;
    }
    // A `classmethod`/`staticmethod`/`property` descriptor holds its callable
    // in `target` or the property components; CPython stamps `__func__`.
    let Some(state) = value::as_descriptor(item) else {
        return;
    };
    match state.kind {
        value::DescriptorKind::Property => {
            for function in [state.fget(), state.fset(), state.fdel()] {
                if function != NONE && value::as_function(function).is_some() {
                    stamp_no_type_check_function(function);
                }
            }
        }
        value::DescriptorKind::Classmethod | value::DescriptorKind::Staticmethod => {
            stamp_no_type_check_member(state.target, depth + 1);
        }
        value::DescriptorKind::TypeAnnotations => {}
    }
}

/// Stamps one class-member function. A member the runtime cannot annotate (a
/// native callable) is skipped instead of failing the whole decorator, like
/// CPython skipping objects that are not directly defined in the class.
fn stamp_no_type_check_function(function: i64) {
    let _ = methods::setattr(
        function,
        NO_TYPE_CHECK_ATTRIBUTE.as_ptr(),
        NO_TYPE_CHECK_ATTRIBUTE.len(),
        TRUE,
    );
    if exc::pending() {
        exc::clear();
    }
}

/// `typing.no_type_check_decorator(decorator)` (deprecated; removed in CPython
/// 3.15): wraps `decorator` so every result it produces is passed through
/// [`no_type_check_value`].
fn no_type_check_decorator_call(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let mut decorator = args.first().copied();
    for (keyword, item) in keywords {
        match keyword.as_str() {
            "decorator" => {
                if decorator.is_some() {
                    return type_error(
                        "no_type_check_decorator() got multiple values for argument 'decorator'",
                    );
                }
                decorator = Some(*item);
            }
            other => {
                return type_error(&format!(
                    "no_type_check_decorator() got an unexpected keyword argument '{other}'"
                ))
            }
        }
    }
    let Some(decorator) = decorator else {
        return type_error(
            "no_type_check_decorator() missing required argument 'decorator' (pos 1)",
        );
    };
    let roots = heap::root_values(&[decorator]);
    let _ = super::warnings::warn_category(
        "'typing.no_type_check_decorator' is deprecated and slated for removal in Python 3.15",
        DEPRECATION_WARNING,
    );
    if exc::pending() {
        heap::release_roots(roots);
        return NONE;
    }
    let wrapper = value::closure_new_with_flags(
        wrapped_decorator_entry as *const () as usize,
        0,
        "wrapped_decorator".to_string(),
        vec![decorator],
        value::FUNCTION_FLAG_VARARGS,
    );
    heap::release_roots(roots);
    wrapper
}

/// Body of the `no_type_check_decorator` wrapper.
///
/// The varargs wrapper buffer is `[tagged total, <captured decorator>,
/// user args...]` (see `builtins::call`'s `FunctionBody::Wrapper` arm).
extern "C" fn wrapped_decorator_entry(argv: *const i64) -> i64 {
    // SAFETY: the wrapper ABI always supplies the tagged count and the one
    // capture before the user arguments.
    let total = value::as_integer(unsafe { *argv }).unwrap_or(1).max(1) as usize;
    let decorator = unsafe { *argv.add(1) };
    let argc = total.saturating_sub(1);
    let args: Vec<i64> = (0..argc)
        .map(|index| unsafe { *argv.add(2 + index) })
        .collect();
    let roots = heap::root_values(&args);
    let decorator_roots = heap::root_values(&[decorator]);
    let decorated = crate::builtins::call(decorator, args.len(), args.as_ptr());
    let result = if exc::pending() {
        decorated
    } else {
        no_type_check_value(decorated)
    };
    heap::release_roots(decorator_roots);
    heap::release_roots(roots);
    result
}

/// `typing.__all__`: the public names this runtime's `typing` module exports.
///
/// Built from the central spec so the list cannot drift from the module
/// surface: public function names first, then public constants. Private
/// (`_`-prefixed) names and `__all__` itself are omitted, like CPython.
fn typing_all() -> i64 {
    let Some(spec) = hydro_ir::stdlib::module("typing") else {
        return value::list_new(Vec::new());
    };
    let mut names: Vec<&'static str> = Vec::new();
    for function in spec.functions {
        if !function.name.starts_with('_') && !names.contains(&function.name) {
            names.push(function.name);
        }
    }
    for constant in spec.constants {
        if constant.name == "__all__" || constant.name.starts_with('_') {
            continue;
        }
        if !names.contains(&constant.name) {
            names.push(constant.name);
        }
    }
    let items: Vec<i64> = names
        .into_iter()
        .map(|name| value::str_alloc(name.to_string()))
        .collect();
    let roots = heap::root_values(&items);
    let list = value::list_new(items);
    heap::release_roots(roots);
    list
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
    let mut methods = vec![
        ("__init__".to_string(), init),
        ("__repr__".to_string(), repr),
    ];
    if name == "TypeVarTuple" {
        // CPython's `TypeVarTuple` is iterable: iterating it yields
        // `Unpack[Ts]`. That is what makes a variadic alias value such as
        // `type X[*Ts] = tuple[*Ts]` evaluate (`tuple[*Ts]` evaluates the
        // starred operand through `tuple()`), and the same protocol serves
        // runtime-evaluated PEP 646 annotations.
        let iter = value::native_function_new("typing", "_typevar_tuple_iter", usize::MAX);
        value::function_set_signature(
            iter,
            vec![value::ParamMeta {
                name: "self".to_string(),
                kind: ParamKind::Positional,
                default: None,
            }],
        );
        value::function_set_full_entry(iter, typevar_tuple_iter_entry as *const () as usize);
        methods.push(("__iter__".to_string(), iter));
    }
    let class = value::class_new(name.to_string(), NONE, methods);
    crate::core::globals::registry_set(&key, class);
    class
}

/// `TypeVarTuple.__iter__`: yields the single member `Unpack[Ts]`.
extern "C" fn typevar_tuple_iter_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self` first.
    let this = unsafe { *argv };
    // The caller's argument buffer is not a GC root, so root the receiver
    // before the first allocation (`constant` builds the `Unpack` class).
    let roots = heap::root_values(&[this]);
    let unpack = constant("Unpack");
    if exc::pending() {
        heap::release_roots(roots);
        return unpack;
    }
    let unpack_roots = heap::root_values(&[unpack]);
    // `typing.Unpack[Ts]` is a class subscript, which yields the class value.
    let item = crate::core::container::subscript(unpack, this);
    if exc::pending() {
        heap::release_roots(unpack_roots);
        heap::release_roots(roots);
        return item;
    }
    let item_roots = heap::root_values(&[item]);
    let items = value::tuple_new(vec![item]);
    let items_roots = heap::root_values(&[items]);
    let iter = crate::core::iter::iter_new(items);
    heap::release_roots(items_roots);
    heap::release_roots(item_roots);
    heap::release_roots(unpack_roots);
    heap::release_roots(roots);
    iter
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

/// The cached `TypeVar` class.
///
/// CPython creates a fresh type variable on every call. The class is shared so
/// `isinstance(t, TypeVar)` and `from typing import TypeVar` keep working,
/// while each construction allocates its own instance and attributes.
fn typevar_class() -> i64 {
    let key = "typing.TypeVar";
    if let Some(cached) = crate::core::globals::registry_get(key) {
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
            name: "constraints".to_string(),
            kind: ParamKind::VarArg,
            default: None,
        },
        value::ParamMeta {
            name: "kwargs".to_string(),
            kind: ParamKind::KwArg,
            default: Some(NONE),
        },
    ];
    let init = value::native_function_new("typing", "_typevar_init", usize::MAX);
    value::function_set_signature(init, signature);
    value::function_set_full_entry(init, typevar_init_entry as *const () as usize);
    let repr = value::native_function_new("typing", "_typevar_repr", usize::MAX);
    value::function_set_signature(
        repr,
        vec![value::ParamMeta {
            name: "self".to_string(),
            kind: ParamKind::Positional,
            default: None,
        }],
    );
    value::function_set_full_entry(repr, typevar_repr_entry as *const () as usize);
    let class = value::class_new(
        "TypeVar".to_string(),
        NONE,
        vec![
            ("__init__".to_string(), init),
            ("__repr__".to_string(), repr),
        ],
    );
    crate::core::globals::registry_set(key, class);
    class
}

/// `TypeVar.__init__(self, name, *constraints, **kwargs)`.
///
/// The class-call binder passes `constraints` as a list (`*args`) and the
/// keyword flags as a dict (`**kwargs`).
extern "C" fn typevar_init_entry(argv: *const i64) -> i64 {
    // SAFETY: the class-call binder passes one slot per declared parameter.
    let instance = unsafe { *argv };
    let name = unsafe { *argv.add(1) };
    let constraints = unsafe { *argv.add(2) };
    let kwargs = unsafe { *argv.add(3) };
    populate_type_var(instance, name, constraints, kwargs)
}

/// Stores CPython's type-variable attributes on a fresh instance.
fn populate_type_var(instance: i64, name: i64, constraints: i64, kwargs: i64) -> i64 {
    let Some(name_text) = value::as_str(name) else {
        return type_error(&format!(
            "TypeVar() argument 'name' must be str, not {}",
            value::type_name(name)
        ));
    };
    let name_text = name_text.to_string();
    // Constraints arrive as a list from `*args`; the attribute is a tuple.
    let constraints: Vec<i64> = value::as_list(constraints)
        .or_else(|| value::as_tuple(constraints))
        .map(|items| items.to_vec())
        .unwrap_or_default();
    let mut bound = NONE;
    let mut covariant = false;
    let mut contravariant = false;
    let mut infer_variance = false;
    let mut default = NONE;
    let mut module = None;
    if let Some(pairs) = value::as_dict(kwargs) {
        for (key, item) in pairs {
            let Some(keyword) = value::as_str(*key) else {
                return type_error("keyword names must be str");
            };
            match keyword {
                "bound" => bound = *item,
                "covariant" => covariant = value::truthy(*item),
                "contravariant" => contravariant = value::truthy(*item),
                "infer_variance" => infer_variance = value::truthy(*item),
                "default" => default = *item,
                "module" => module = Some(*item),
                other => {
                    return type_error(&format!(
                        "TypeVar() got an unexpected keyword argument '{other}'"
                    ))
                }
            }
        }
    }
    if covariant && contravariant {
        return exc::raise_with_message(VALUE_ERROR, "Bivariant types are not supported.");
    }
    let name_value = value::str_alloc(name_text);
    let name_roots = heap::root_values(&[instance, name_value, bound, default]);
    let module = module.unwrap_or_else(|| value::str_alloc("typing".to_string()));
    let module_roots = heap::root_values(&[module]);
    let constraints = value::tuple_new(constraints);
    let constraint_roots = heap::root_values(&[constraints]);
    for (name, item) in [
        ("__name__", name_value),
        ("__constraints__", constraints),
        ("__bound__", bound),
        ("__covariant__", value::bool_value(covariant)),
        ("__contravariant__", value::bool_value(contravariant)),
        ("__infer_variance__", value::bool_value(infer_variance)),
        ("__default__", default),
        ("__module__", module),
    ] {
        value::instance_setattr(instance, name.to_string(), item);
    }
    heap::release_roots(constraint_roots);
    heap::release_roots(module_roots);
    heap::release_roots(name_roots);
    NONE
}

/// `repr(TypeVar('T')) == '~T'`.
fn typevar_repr(instance: i64) -> i64 {
    let name = value::instance_attr(instance, "__name__")
        .and_then(value::as_str)
        .unwrap_or("?");
    value::str_alloc(format!("~{name}"))
}

extern "C" fn typevar_repr_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self` first.
    typevar_repr(unsafe { *argv })
}

/// `typing.TypeVar(name, *constraints, *, bound=None, covariant=False,
/// contravariant=False, infer_variance=False, default=None, module=None)`
/// through the module-call path.
fn new_typevar(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let mut name = args.first().copied();
    let mut extra: Vec<(i64, i64)> = Vec::new();
    for (keyword, item) in keywords {
        if keyword == "name" {
            if name.is_some() {
                return type_error("TypeVar() got multiple values for argument 'name'");
            }
            name = Some(*item);
        } else {
            extra.push((value::str_alloc(keyword.clone()), *item));
        }
    }
    let Some(name) = name else {
        return type_error("TypeVar() missing required argument 'name'");
    };
    let constraints = args.get(1..).unwrap_or_default().to_vec();
    let constraints = value::list_new(constraints);
    let roots = heap::root_values(&[name, constraints]);
    let kwargs = value::dict_new(extra);
    let kwargs_roots = heap::root_values(&[kwargs]);
    let instance = value::instance_new(typevar_class());
    let slots = [instance, name, constraints, kwargs];
    let instance_roots = heap::root_values(&slots);
    let result = typevar_init_entry(slots.as_ptr());
    heap::release_roots(instance_roots);
    heap::release_roots(kwargs_roots);
    heap::release_roots(roots);
    if exc::pending() {
        return result;
    }
    instance
}

/// `typing.TypeAliasType` (PEP 695) as a real class value.
///
/// Hydro lowers `type X = expr` eagerly, so an alias instance carries the
/// evaluated value instead of re-evaluating it on first attribute access. The
/// attribute surface matches CPython 3.14 (`__name__`, `__value__`,
/// `__type_params__`, `__parameters__`, `__module__`), `repr` is the alias
/// name, and `__getitem__` builds a generic alias for a generic `type X[T]` so
/// `X[int]` yields an alias carrying `__origin__`/`__args__`. Subscripting a
/// non-generic alias raises CPython's `TypeError`.
fn type_alias_type_class() -> i64 {
    let key = "typing.TypeAliasType";
    if let Some(cached) = crate::core::globals::registry_get(key) {
        return cached;
    }
    let self_param = || value::ParamMeta {
        name: "self".to_string(),
        kind: ParamKind::Positional,
        default: None,
    };
    let signature = vec![
        self_param(),
        value::ParamMeta {
            name: "name".to_string(),
            kind: ParamKind::Positional,
            default: None,
        },
        value::ParamMeta {
            name: "value".to_string(),
            kind: ParamKind::Positional,
            default: None,
        },
        value::ParamMeta {
            name: "kwargs".to_string(),
            kind: ParamKind::KwArg,
            default: Some(NONE),
        },
    ];
    let init = value::native_function_new("typing", "_type_alias_type_init", usize::MAX);
    value::function_set_signature(init, signature);
    value::function_set_full_entry(init, init_type_alias_type as *const () as usize);
    let repr = value::native_function_new("typing", "_type_alias_type_repr", usize::MAX);
    value::function_set_signature(repr, vec![self_param()]);
    value::function_set_full_entry(repr, type_alias_type_repr_entry as *const () as usize);
    let getitem = value::native_function_new("typing", "_type_alias_type_getitem", usize::MAX);
    value::function_set_signature(
        getitem,
        vec![
            self_param(),
            value::ParamMeta {
                name: "parameters".to_string(),
                kind: ParamKind::Positional,
                default: None,
            },
        ],
    );
    value::function_set_full_entry(getitem, type_alias_type_getitem_entry as *const () as usize);
    let class = value::class_new(
        "TypeAliasType".to_string(),
        NONE,
        vec![
            ("__init__".to_string(), init),
            ("__repr__".to_string(), repr),
            ("__getitem__".to_string(), getitem),
        ],
    );
    crate::core::globals::registry_set(key, class);
    class
}

/// True when `v` is a `typing.TypeAliasType` instance (a PEP 695 alias).
///
/// `types.UnionType` accepts aliases as `|` members; the check reads the
/// cached class from the registry so an ordinary instance never allocates it.
pub(crate) fn is_type_alias_value(v: i64) -> bool {
    let Some(class) = value::instance_class_value(v) else {
        return false;
    };
    crate::core::globals::registry_get("typing.TypeAliasType") == Some(class)
}

/// `TypeAliasType(name, value, *, type_params=(), module=...)`.
///
/// `module` is Hydro-specific: CPython derives the defining module from the
/// caller frame, which a native class cannot inspect, so the lowering passes
/// the compilation unit's name explicitly. A direct CPython-shaped call falls
/// back to the entry module.
extern "C" fn init_type_alias_type(argv: *const i64) -> i64 {
    // SAFETY: the class-call binder passes one slot per declared parameter.
    let instance = unsafe { *argv };
    let name = unsafe { *argv.add(1) };
    let value_arg = unsafe { *argv.add(2) };
    let kwargs = unsafe { *argv.add(3) };
    let mut type_params = value::tuple_new(Vec::new());
    let mut module = NONE;
    if let Some(pairs) = value::as_dict(kwargs) {
        for (key, item) in pairs {
            let Some(keyword) = value::as_str(*key) else {
                return type_error("keyword names must be str");
            };
            match keyword {
                "type_params" => type_params = *item,
                "module" => module = *item,
                other => {
                    return type_error(&format!(
                        "typealias() got an unexpected keyword argument '{other}'"
                    ))
                }
            }
        }
    }
    let roots = heap::root_values(&[instance, name, value_arg, type_params, module]);
    let result = populate_type_alias(instance, name, value_arg, type_params, module);
    heap::release_roots(roots);
    result
}

/// Stores the CPython `TypeAliasType` attributes on a fresh instance.
fn populate_type_alias(
    instance: i64,
    name: i64,
    value_arg: i64,
    type_params: i64,
    module: i64,
) -> i64 {
    let Some(name_text) = value::as_str(name) else {
        return type_error(&format!(
            "typealias() argument 'name' must be str, not {}",
            value::type_name(name)
        ));
    };
    let name = value::str_alloc(name_text.to_string());
    if value::as_tuple(type_params).is_none() {
        return type_error("type_params must be a tuple");
    }
    let module = if value::as_str(module).is_some() {
        module
    } else if module == NONE {
        value::str_alloc("__main__".to_string())
    } else {
        return type_error(&format!(
            "typealias() argument 'module' must be str, not {}",
            value::type_name(module)
        ));
    };
    let roots = heap::root_values(&[instance, name, value_arg, type_params, module]);
    for (attribute, item) in [
        ("__name__", name),
        ("__value__", value_arg),
        ("__type_params__", type_params),
        ("__parameters__", type_params),
        ("__module__", module),
    ] {
        value::instance_setattr(instance, attribute.to_string(), item);
    }
    heap::release_roots(roots);
    NONE
}

/// `typing._type_alias_new(name, value, type_params, module)`: the lowering
/// target for `type X = expr` / `type X[T] = expr`.
fn new_type_alias(args: &[i64]) -> i64 {
    let name = args.first().copied().unwrap_or(NONE);
    let value_arg = args.get(1).copied().unwrap_or(NONE);
    let type_params = args.get(2).copied().unwrap_or(NONE);
    let module = args.get(3).copied().unwrap_or(NONE);
    let instance = value::instance_new(type_alias_type_class());
    let roots = heap::root_values(&[instance, name, value_arg, type_params, module]);
    let result = populate_type_alias(instance, name, value_arg, type_params, module);
    heap::release_roots(roots);
    if exc::pending() {
        return result;
    }
    instance
}

/// `repr(alias) == alias.__name__` (CPython 3.14).
extern "C" fn type_alias_type_repr_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self` first.
    let instance = unsafe { *argv };
    match value::instance_attr(instance, "__name__") {
        Some(name) if value::as_str(name).is_some() => name,
        _ => value::str_alloc("TypeAliasType".to_string()),
    }
}

extern "C" fn type_alias_type_getitem_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `self` then the index operand.
    let instance = unsafe { *argv };
    let parameters = unsafe { *argv.add(1) };
    type_alias_getitem(instance, parameters)
}

/// `alias[parameters]`: only a generic alias (`type X[T] = ...`) accepts one.
fn type_alias_getitem(alias: i64, parameters: i64) -> i64 {
    let generic = value::instance_attr(alias, "__type_params__")
        .and_then(value::as_tuple)
        .is_some_and(|items| !items.is_empty());
    if !generic {
        return type_error("Only generic type aliases are subscriptable");
    }
    generic_alias_of(alias, parameters)
}

/// Materialises the module constant `name`.
pub(crate) fn constant(name: &str) -> i64 {
    if name == "TYPE_CHECKING" {
        return FALSE;
    }
    if name == "ForwardRef" {
        return forward_ref_class();
    }
    if name == "TypeAliasType" {
        return type_alias_type_class();
    }
    if name == "ParamSpec" || name == "TypeVarTuple" {
        return typevar_like_class(name);
    }
    if name == "NoDefault" || name == "NoExtraItems" {
        return sentinel(name);
    }
    if CLASSES.contains(&name) {
        return placeholder_class(name);
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
        "__all__" => typing_all(),
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
        // `assert_never(arg, /)` marks unreachable code: it always raises.
        "assert_never" => assert_never(args.first().copied().unwrap_or(NONE)),
        "runtime_checkable" => args.first().copied().unwrap_or(NONE),
        // `@overload` records the stub for `get_overloads`; the callable is
        // returned so a direct call still reaches the annotated body.
        "overload" => overload(args.first().copied().unwrap_or(NONE)),
        // `@no_type_check` is the identity decorator that stamps
        // `__no_type_check__` (recursively on a class's own members).
        "no_type_check" => no_type_check_call(args, &[]),
        "no_type_check_decorator" => no_type_check_decorator_call(args, &[]),
        "get_overloads" => get_overloads(args.first().copied().unwrap_or(NONE)),
        "clear_overloads" => clear_overloads(),
        "disjoint_base" => disjoint_base(args.first().copied().unwrap_or(NONE)),
        // PEP 695/type-assertion helpers: the value argument is returned
        // unchanged. `reveal_type` prints a diagnostic in CPython, but the
        // runtime's contract here is the identity result (the frontend
        // already consumes annotations and emits no reveal diagnostics).
        "assert_type" => args.first().copied().unwrap_or(NONE),
        "reveal_type" => args.first().copied().unwrap_or(NONE),
        "override" => override_marker(args.first().copied().unwrap_or(NONE)),
        "is_typeddict" => value::bool_value(is_typeddict(args.first().copied().unwrap_or(NONE))),
        "is_protocol" => value::bool_value(is_protocol(args.first().copied().unwrap_or(NONE))),
        "get_protocol_members" => get_protocol_members(args.first().copied().unwrap_or(NONE)),
        "_ConcatenateGenericAlias" => concatenate_generic_alias_new(args, &[]),
        // `TypeVar` carries the full type-parameter attribute set; the
        // frontend still consumes annotations itself, but the value is real
        // enough for `repr`/attribute introspection.
        "TypeVar" => new_typevar(args, &[]),
        // Lowering target for a PEP 695 `type X = expr` statement.
        "_type_alias_new" => new_type_alias(args),
        // `NewType("Name", tp)` returns a callable NewType instance carrying
        // `__supertype__` (CPython 3.10+ class semantics).
        "NewType" => new_newtype(args, &[]),
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
        "_ConcatenateGenericAlias" => concatenate_generic_alias_new(args, keywords),
        "ForwardRef" => new_forward_ref(args, keywords),
        "_deprecation_warning_for_no_type_params_passed" => no_type_params(args, keywords),
        "evaluate_forward_ref" => evaluate_forward_ref(args, keywords),
        "ParamSpec" | "TypeVarTuple" => new_typevar_like(function, args, keywords),
        "TypeVar" => new_typevar(args, keywords),
        "NewType" => new_newtype(args, keywords),
        "_tp_cache" => tp_cache(args, keywords),
        "_type_convert" => type_convert(args, keywords),
        "_type_check" => type_check(args, keywords),
        "_eval_type" => eval_type(args, keywords),
        "_deduplicate" => deduplicate(args, keywords),
        "dataclass_transform" => {
            value::identity_function_new("typing.dataclass_transform.<locals>.decorator")
        }
        "no_type_check" => no_type_check_call(args, keywords),
        "no_type_check_decorator" => no_type_check_decorator_call(args, keywords),
        "get_type_hints" => get_type_hints(args, keywords),
        "_get_defaults" => get_defaults(args.first().copied().unwrap_or(NONE)),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'typing' has no attribute '{function}'"),
        ),
    }
}

/// `typing.assert_never(arg, /)`: the branch this call sits in is statically
/// unreachable, so reaching it is a bug. Raises `AssertionError` with the
/// argument's `repr`, truncated to `_ASSERT_NEVER_REPR_MAX_LENGTH` characters
/// exactly like CPython.
fn assert_never(arg: i64) -> i64 {
    let limit = usize::try_from(ASSERT_NEVER_REPR_MAX_LENGTH).unwrap_or(usize::MAX);
    let mut value = value::repr(arg);
    if let Some((cut, _)) = value.char_indices().nth(limit) {
        value.truncate(cut);
        value.push_str("...");
    }
    exc::raise_with_message(
        ASSERTION_ERROR,
        &format!("Expected code to be unreachable, but got: {value}"),
    )
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
    generic_alias_of(cls, parameters)
}

/// Builds a `typing._GenericAlias` with `origin` and `parameters` (one
/// argument when `parameters` is not a tuple). Shared by the `typing` alias
/// constructors, [`builtin_generic_subscript`] and
/// [`generic_class_subscript`].
///
/// CPython's `_GenericAlias.__init__` derives `__parameters__` from the
/// arguments, so `C[int]` reports no parameters while `C[T]` keeps `T`.
fn generic_alias_of(origin: i64, parameters: i64) -> i64 {
    let items: Vec<i64> = match value::as_tuple(parameters) {
        Some(items) => items.to_vec(),
        None => vec![parameters],
    };
    let class = cached_class("_GenericAlias");
    let args_tuple = value::tuple_new(items);
    let roots = heap::root_values(&[origin, args_tuple, class]);
    let collected = collect_type_vars(args_tuple);
    let parameter_roots = heap::root_values(&[collected]);
    let instance = value::instance_new(class);
    value::instance_setattr(instance, "__origin__".to_string(), origin);
    value::instance_setattr(instance, "__args__".to_string(), args_tuple);
    value::instance_setattr(instance, "__parameters__".to_string(), collected);
    heap::release_roots(parameter_roots);
    heap::release_roots(roots);
    instance
}

/// Builtin class values that accept type parameters: CPython serves
/// `tuple[int, ...]`, `list[str]`, `dict[str, int]`, `set[T]`,
/// `frozenset[T]` and `type[T]` through `__class_getitem__`.
fn is_generic_builtin(builtin: Builtin) -> bool {
    matches!(
        builtin,
        Builtin::Tuple
            | Builtin::List
            | Builtin::Dict
            | Builtin::Set
            | Builtin::Frozenset
            | Builtin::Type
    )
}

/// `tuple[int, ...]` and friends: a builtin class value subscribed with type
/// parameters. Returns `None` for any other value, so non-generic builtins
/// (`print[int]`) keep raising `TypeError`.
///
/// The bare names lower to `FunctionBody::Builtin`; the `builtins` module
/// attribute (`builtins.list[int]`) is a native function over the same
/// constructor. CPython builds a `types.GenericAlias`; Hydro's aliases carry
/// the same `__origin__`/`__args__` attributes as the `typing` aliases, so
/// `typing.get_origin`/`get_args` and `repr` agree with them.
pub(crate) fn builtin_generic_subscript(obj: i64, index: i64) -> Option<i64> {
    let function = value::as_function(obj)?;
    let generic = match &function.body {
        value::FunctionBody::Builtin(builtin) => is_generic_builtin(*builtin),
        value::FunctionBody::Native { module, name } if *module == "builtins" => {
            Builtin::from_name(name).is_some_and(is_generic_builtin)
        }
        _ => false,
    };
    if !generic {
        return None;
    }
    Some(generic_alias_of(obj, index))
}

/// `C[int]` for a PEP 695 generic class (`class C[T]: ...`).
///
/// The frontend stores the runtime parameter tuple as the class attribute
/// `__type_params__`, and CPython serves the subscription through
/// `Generic.__class_getitem__`, which builds a `typing._GenericAlias` whose
/// origin is the class. `None` means "not a generic class": the caller keeps
/// its existing tolerance (the class value itself), so ordinary classes stay
/// subscriptable without changing their meaning.
///
/// The lookup follows the MRO like ordinary attribute access: CPython's
/// `__class_getitem__`/`__type_params__` are inherited, so a subclass of a
/// generic class specializes its base's parameters.
pub(crate) fn generic_class_subscript(class: i64, index: i64) -> Option<i64> {
    // The special forms pydantic introspects answer with real alias
    // metadata. The name gate keeps the identity lookups off ordinary class
    // subscripts; the identity check keeps a user class that merely shares a
    // name (`class Literal: ...`) out of the special forms.
    if let Some(name) = value::class_display_name(class) {
        let special = match name {
            "Literal" if class == placeholder_class("Literal") => Some(literal_alias(class, index)),
            "Union" if class == placeholder_class("Union") => Some(union_alias(class, index)),
            "Optional" if class == placeholder_class("Optional") => Some(optional_alias(index)),
            "Annotated" if class == placeholder_class("Annotated") => Some(annotated_alias(index)),
            _ => None,
        };
        if let Some(alias) = special {
            return Some(alias);
        }
    }
    let params = match value::class_lookup(class, "__type_params__")? {
        value::ClassEntry::Attr(params) => params,
        value::ClassEntry::Method(..) => return None,
    };
    if value::as_tuple(params)?.is_empty() {
        return None;
    }
    Some(generic_alias_of(class, index))
}

/// One argument list of a class subscription.
///
/// `X[a, b]` passes the tuple `(a, b)`, while `X[a]` passes `a` itself, so a
/// single-argument subscription must not be mistaken for a one-element tuple.
fn subscript_args(index: i64) -> Vec<i64> {
    match value::as_tuple(index) {
        Some(items) => items.to_vec(),
        None => vec![index],
    }
}

/// Whether two literal/union members are the same value *and* runtime type.
///
/// CPython's `Literal`/`Union` deduplication keys on `(value, type(value))`,
/// so `Literal[True, 1]` keeps both entries while two equal strings keep one.
fn same_member(a: i64, b: i64) -> bool {
    a == b || (container::equals(a, b) && value::type_class(a) == value::type_class(b))
}

/// `types.NoneType`, the class of `None` and a `Union` member.
fn none_type() -> i64 {
    crate::modules::types::constant("NoneType")
}

/// `Literal[...]`: a `_GenericAlias` whose arguments are the literal values.
///
/// Nested `Literal` aliases are spliced into the outer argument list and exact
/// `(value, type)` duplicates are dropped, matching CPython 3.14's
/// `_flatten_literal_params` plus `_deduplicate`.
fn literal_alias(origin: i64, index: i64) -> i64 {
    let args = subscript_args(index);
    let input_roots = heap::root_values(&args);
    let mut flattened: Vec<i64> = Vec::new();
    for item in args {
        if generic_alias_origin(item) == Some(origin) {
            if let Some(inner) = value::instance_attr(item, "__args__").and_then(value::as_tuple) {
                flattened.extend_from_slice(inner);
                continue;
            }
        }
        flattened.push(item);
    }
    let mut unique: Vec<i64> = Vec::with_capacity(flattened.len());
    for item in flattened {
        if !unique.iter().any(|existing| same_member(*existing, item)) {
            unique.push(item);
        }
    }
    let member_roots = heap::root_values(&unique);
    let alias = generic_alias_of(origin, value::tuple_new(unique));
    heap::release_roots(member_roots);
    heap::release_roots(input_roots);
    alias
}

/// Appends one `Union` member, splicing nested `Union` aliases and native
/// `A | B` unions into the outer argument list.
fn flatten_union_member(item: i64, origin: i64, out: &mut Vec<i64>) {
    if generic_alias_origin(item) == Some(origin) {
        if let Some(inner) = value::instance_attr(item, "__args__").and_then(value::as_tuple) {
            out.extend_from_slice(inner);
            return;
        }
    }
    if crate::modules::types::is_union(item) {
        if let Some(inner) = value::instance_attr(item, "__args__").and_then(value::as_tuple) {
            out.extend_from_slice(inner);
            return;
        }
    }
    out.push(item);
}

/// `Union[...]`: flatten nested unions, drop duplicates and collapse to the
/// single member like CPython 3.14.
///
/// Multi-member results are native `types.UnionType` values (`int | str`),
/// which is what CPython 3.14 produces since `typing.Union` and
/// `types.UnionType` were unified; the instance also reports the `Union` class
/// as `__origin__` and its collected `__parameters__`.
fn union_alias(origin: i64, index: i64) -> i64 {
    let args = subscript_args(index);
    let input_roots = heap::root_values(&args);
    let mut members: Vec<i64> = Vec::new();
    for item in args {
        flatten_union_member(item, origin, &mut members);
    }
    let mut unique: Vec<i64> = Vec::with_capacity(members.len());
    for member in members {
        // `None` and `NoneType` name the same union member.
        let candidate = if member == NONE { none_type() } else { member };
        let duplicate = unique.iter().any(|existing| {
            let existing = if *existing == NONE {
                none_type()
            } else {
                *existing
            };
            same_member(existing, candidate)
        });
        if !duplicate {
            unique.push(member);
        }
    }
    let result = match unique.len() {
        0 => type_error("Cannot take a Union of no types."),
        1 => {
            if unique[0] == NONE {
                none_type()
            } else {
                unique[0]
            }
        }
        _ => {
            let member_roots = heap::root_values(&unique);
            let union = crate::modules::types::union_new(&unique);
            let union_roots = heap::root_values(&[union]);
            if union != NONE && !exc::pending() {
                if let Some(arguments) = value::instance_attr(union, "__args__") {
                    value::instance_setattr(union, "__origin__".to_string(), origin);
                    let parameters = collect_type_vars(arguments);
                    value::instance_setattr(union, "__parameters__".to_string(), parameters);
                }
            }
            heap::release_roots(union_roots);
            heap::release_roots(member_roots);
            union
        }
    };
    heap::release_roots(input_roots);
    result
}

/// `Optional[...]`: `Union[T, NoneType]`.
fn optional_alias(index: i64) -> i64 {
    let args = subscript_args(index);
    if args.len() != 1 {
        return type_error("typing.Optional requires a single type.");
    }
    let members = value::tuple_new(vec![args[0], NONE]);
    let roots = heap::root_values(&[members]);
    let result = union_alias(placeholder_class("Union"), members);
    heap::release_roots(roots);
    result
}

/// `Annotated[...]`: a `_GenericAlias` whose `__origin__` is the annotated
/// type, `__args__` the one-element type tuple and `__metadata__` the
/// annotation values.
///
/// Nested `Annotated` forms flatten their metadata into the outer alias
/// (`Annotated[Annotated[int, "a"], "b"]` reports `("a", "b")`), matching
/// CPython.
fn annotated_alias(index: i64) -> i64 {
    let args = subscript_args(index);
    if args.len() < 2 {
        return type_error(
            "Annotated[...] should be used with at least two arguments (a type and an annotation).",
        );
    }
    let input_roots = heap::root_values(&args);
    let mut base = args[0];
    let mut metadata: Vec<i64> = Vec::new();
    if let Some(inner) = annotated_metadata(base) {
        metadata.extend_from_slice(inner);
        base = value::instance_attr(base, "__origin__").unwrap_or(base);
    }
    metadata.extend_from_slice(&args[1..]);
    let class = cached_class("_GenericAlias");
    let inner = value::tuple_new(vec![base]);
    let inner_roots = heap::root_values(&[class, inner]);
    let meta = value::tuple_new(metadata);
    let meta_roots = heap::root_values(&[meta]);
    let parameters = collect_type_vars(inner);
    let parameter_roots = heap::root_values(&[parameters]);
    let instance = value::instance_new(class);
    value::instance_setattr(instance, "__origin__".to_string(), base);
    value::instance_setattr(instance, "__args__".to_string(), inner);
    value::instance_setattr(instance, "__metadata__".to_string(), meta);
    value::instance_setattr(instance, "__parameters__".to_string(), parameters);
    heap::release_roots(parameter_roots);
    heap::release_roots(meta_roots);
    heap::release_roots(inner_roots);
    heap::release_roots(input_roots);
    instance
}

/// The origin of a `typing._GenericAlias` value (`C` in `C[int]`), or `None`
/// for every other value.
///
/// Class creation uses it so a specialized base keeps its origin, like
/// CPython's `class D(C[int])`.
pub(crate) fn generic_alias_origin(v: i64) -> Option<i64> {
    // Read the instance class before touching the cache: this helper runs
    // while `_GenericAlias` itself is being constructed, and looking the
    // cached class up unconditionally would recurse into class creation.
    let class = value::instance_class_value(v)?;
    if class != cached_class("_GenericAlias") {
        return None;
    }
    value::instance_attr(v, "__origin__")
}

/// `typing._collect_type_vars`/`_collect_parameters`: flat collection of
/// `TypeVar`/`ParamSpec`/`TypeVarTuple` members, recursing into tuples and
/// lists.
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
        || class == Some(typevar_like_class("TypeVarTuple"))
        || class == Some(typevar_class()))
        && !collected.contains(&value_arg)
    {
        collected.push(value_arg);
    }
}

/// `typing.get_args(tp)`: the alias arguments when the value carries
/// `__args__`, otherwise an empty tuple.
///
/// `Annotated[...]` reports `(type, *metadata)` and a native `A | B` union
/// reports `NoneType` where its stored argument list holds `None`, both
/// matching CPython 3.14. A `TypeVar` carries no `__args__`, so it reports the
/// empty tuple like CPython.
fn alias_args(value_arg: i64) -> i64 {
    if let Some(metadata) = annotated_metadata(value_arg) {
        let origin = value::instance_attr(value_arg, "__origin__").unwrap_or(NONE);
        let mut items = Vec::with_capacity(metadata.len() + 1);
        items.push(origin);
        items.extend_from_slice(metadata);
        let roots = heap::root_values(&items);
        let result = value::tuple_new(items);
        heap::release_roots(roots);
        return result;
    }
    if crate::modules::types::is_union(value_arg) {
        if let Some(args) = value::instance_attr(value_arg, "__args__") {
            if let Some(items) = value::as_tuple(args) {
                let mut normalized: Vec<i64> = items.to_vec();
                let mut changed = false;
                for item in &mut normalized {
                    if *item == NONE {
                        *item = none_type();
                        changed = true;
                    }
                }
                if changed {
                    let roots = heap::root_values(&normalized);
                    let result = value::tuple_new(normalized);
                    heap::release_roots(roots);
                    return result;
                }
                return args;
            }
        }
    }
    if let Some(args) = value::instance_attr(value_arg, "__args__") {
        if value::as_tuple(args).is_some() {
            return args;
        }
    }
    value::tuple_new(Vec::new())
}

/// `typing.get_origin(tp)`: the alias origin, otherwise `None`.
///
/// An `Annotated` alias reports the canonical `Annotated` class, and a native
/// `A | B` union reports the canonical `Union` class, like CPython 3.14's
/// unified `typing.Union`/`types.UnionType`.
fn alias_origin(value_arg: i64) -> i64 {
    if annotated_metadata(value_arg).is_some() {
        return placeholder_class("Annotated");
    }
    if crate::modules::types::is_union(value_arg) {
        return placeholder_class("Union");
    }
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

    fn pending_message() -> String {
        value::to_string(exc::pending_value())
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
    fn literal_aliases_carry_their_values() {
        exc::init();
        let literal = constant("Literal");
        assert_eq!(
            constant("Literal"),
            literal,
            "placeholder identity is stable (`typing.Literal is typing.Literal`)"
        );
        let alias = crate::core::container::subscript(
            literal,
            value::tuple_new(vec![text("a"), text("b")]),
        );
        assert!(!exc::pending(), "{:?}", pending_message());
        assert_eq!(value::repr(alias), "Literal['a', 'b']");
        assert_eq!(call("get_origin", &[alias]), literal);
        let args = as_tuple(call("get_args", &[alias])).expect("args").to_vec();
        let rendered: Vec<&str> = args
            .iter()
            .filter_map(|item| value::as_str(*item))
            .collect();
        assert_eq!(rendered, vec!["a", "b"]);

        // Exact `(value, type)` duplicates collapse; a different runtime type
        // is a different literal (`Literal[1, True]` keeps both).
        let one = value::encode_int(1).expect("representable int");
        let duplicated = crate::core::container::subscript(
            literal,
            value::tuple_new(vec![text("a"), text("a"), one, TRUE]),
        );
        let args = as_tuple(call("get_args", &[duplicated]))
            .expect("args")
            .to_vec();
        assert_eq!(args.len(), 3, "one `a`, the int 1 and True");
        assert_eq!(value::as_str(args[0]), Some("a"));
        assert_eq!(args[1], one);
        assert_eq!(args[2], TRUE);
        assert_eq!(value::repr(duplicated), "Literal['a', 1, True]");

        // Nested `Literal` arguments are spliced into the outer alias.
        let nested = crate::core::container::subscript(literal, alias);
        let args = as_tuple(call("get_args", &[nested]))
            .expect("args")
            .to_vec();
        assert_eq!(args.len(), 2);
        assert_eq!(value::repr(nested), "Literal['a', 'b']");
        assert!(!exc::pending());
    }

    #[test]
    fn union_aliases_flatten_and_report_cpython_metadata() {
        exc::init();
        let union = constant("Union");
        let int_class = value::class_new("int".to_string(), NONE, Vec::new());
        let str_class = value::class_new("str".to_string(), NONE, Vec::new());
        let combined =
            crate::core::container::subscript(union, value::tuple_new(vec![int_class, str_class]));
        assert!(!exc::pending(), "{:?}", pending_message());
        // CPython 3.14 unifies `typing.Union` with `types.UnionType`, so the
        // result is a native `int | str` union.
        assert_eq!(value::repr(combined), "int | str");
        assert_eq!(call("get_origin", &[combined]), union);
        let args = as_tuple(call("get_args", &[combined]))
            .expect("args")
            .to_vec();
        assert_eq!(args, vec![int_class, str_class]);

        // Nested unions flatten, duplicates collapse, and a one-member union
        // decays to that member.
        let nested =
            crate::core::container::subscript(union, value::tuple_new(vec![combined, str_class]));
        assert_eq!(
            as_tuple(call("get_args", &[nested])).map(|items| items.len()),
            Some(2)
        );
        // A one-member union decays to that member.
        let single = crate::core::container::subscript(union, int_class);
        assert_eq!(single, int_class);

        // `Optional[T]` is `Union[T, None]`; `get_args` reports `NoneType`
        // while the repr keeps CPython's `None` spelling.
        let optional = crate::core::container::subscript(constant("Optional"), int_class);
        assert_eq!(value::repr(optional), "int | None");
        assert_eq!(call("get_origin", &[optional]), union);
        let optional_args = as_tuple(call("get_args", &[optional]))
            .expect("args")
            .to_vec();
        assert_eq!(optional_args, vec![int_class, value::type_class(NONE)]);

        // A PEP 604 union reports the same origin and `NoneType` argument.
        let native = crate::modules::types::union_binary_or(int_class, NONE).expect("union");
        assert_eq!(call("get_origin", &[native]), union);
        let native_args = as_tuple(call("get_args", &[native]))
            .expect("args")
            .to_vec();
        assert_eq!(native_args, vec![int_class, value::type_class(NONE)]);

        // `Union[T, int]` keeps the type parameter visible through
        // `get_args` and `__parameters__`, which pydantic's generic handling
        // substitutes against.
        let typevar = call("TypeVar", &[text("T")]);
        let parameterized =
            crate::core::container::subscript(union, value::tuple_new(vec![typevar, int_class]));
        assert_eq!(
            as_tuple(call("get_args", &[parameterized])).map(|items| items.to_vec()),
            Some(vec![typevar, int_class])
        );
        assert_eq!(
            as_tuple(
                value::instance_attr(parameterized, "__parameters__").expect("__parameters__")
            )
            .map(|items| items.to_vec()),
            Some(vec![typevar])
        );

        // An empty subscription is rejected, and a user class named like the
        // placeholder is not treated as the special form.
        crate::core::container::subscript(union, value::tuple_new(Vec::new()));
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        let user_union = value::class_new("Union".to_string(), NONE, Vec::new());
        assert_eq!(
            crate::core::container::subscript(user_union, int_class),
            user_union
        );
        assert!(!exc::pending());
    }

    #[test]
    fn annotated_aliases_expose_origin_metadata_and_args() {
        exc::init();
        let annotated_class = constant("Annotated");
        let int_class = value::class_new("int".to_string(), NONE, Vec::new());
        let alias = crate::core::container::subscript(
            annotated_class,
            value::tuple_new(vec![int_class, text("meta")]),
        );
        assert!(!exc::pending(), "{:?}", pending_message());
        assert_eq!(value::repr(alias), "Annotated[int, 'meta']");
        assert_eq!(call("get_origin", &[alias]), annotated_class);
        assert_eq!(value::instance_attr(alias, "__origin__"), Some(int_class));
        assert_eq!(
            as_tuple(value::instance_attr(alias, "__args__").expect("__args__"))
                .map(|items| items.to_vec()),
            Some(vec![int_class])
        );
        let metadata = as_tuple(value::instance_attr(alias, "__metadata__").expect("__metadata__"))
            .expect("metadata");
        assert_eq!(metadata.len(), 1);
        assert_eq!(value::as_str(metadata[0]), Some("meta"));
        // `get_args` prepends the annotated type, like CPython.
        let args = as_tuple(call("get_args", &[alias])).expect("args").to_vec();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], int_class);
        assert_eq!(value::as_str(args[1]), Some("meta"));

        // Nested annotations flatten their metadata.
        let nested = crate::core::container::subscript(
            annotated_class,
            value::tuple_new(vec![alias, text("outer")]),
        );
        assert_eq!(call("get_origin", &[nested]), annotated_class);
        assert_eq!(value::instance_attr(nested, "__origin__"), Some(int_class));
        let metadata =
            as_tuple(value::instance_attr(nested, "__metadata__").expect("__metadata__"))
                .expect("metadata");
        assert_eq!(metadata.len(), 2);
        assert_eq!(value::as_str(metadata[0]), Some("meta"));
        assert_eq!(value::as_str(metadata[1]), Some("outer"));

        // A single argument is rejected.
        crate::core::container::subscript(annotated_class, int_class);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert!(pending_message().contains("at least two arguments"));
        exc::clear();
        assert!(!exc::pending());
    }

    #[test]
    fn typevars_have_no_args_like_cpython() {
        exc::init();
        let typevar = call("TypeVar", &[text("T")]);
        assert!(!exc::pending(), "{:?}", pending_message());
        assert_eq!(
            as_tuple(call("get_args", &[typevar])).map(|items| items.len()),
            Some(0)
        );
        assert_eq!(value::instance_attr(typevar, "__args__"), None);
        assert_eq!(call("get_origin", &[typevar]), NONE);
    }

    #[test]
    fn builtin_classes_subscribe_into_generic_aliases() {
        exc::init();
        let tuple_type = value::builtin_function_new(Builtin::Tuple);
        let list_type = value::builtin_function_new(Builtin::List);
        let dict_type = value::builtin_function_new(Builtin::Dict);
        let type_type = value::builtin_function_new(Builtin::Type);
        let int_type = value::builtin_function_new(Builtin::Int);
        let str_type = value::builtin_function_new(Builtin::Str);

        // `tuple[int, ...]`: the multi-operand index arrives as a tuple, and
        // Hydro's `...` literal is `None`.
        let variadic =
            crate::core::container::subscript(tuple_type, value::tuple_new(vec![int_type, NONE]));
        assert!(!exc::pending(), "{:?}", pending_message());
        assert_eq!(value::repr(variadic), "tuple[int, ...]");
        // `print(alias)` and interpolation go through `str`.
        assert_eq!(value::to_string(variadic), "tuple[int, ...]");
        assert_eq!(
            value::instance_attr(variadic, "__origin__"),
            Some(tuple_type)
        );
        let expected_args = value::instance_attr(variadic, "__args__").expect("__args__");
        assert_eq!(
            as_tuple(expected_args).map(|items| items.to_vec()),
            Some(vec![int_type, NONE])
        );
        // `typing.get_origin`/`get_args` read the same attributes back.
        assert_eq!(call("get_origin", &[variadic]), tuple_type);
        assert_eq!(call("get_args", &[variadic]), expected_args);

        // `list[str]`: a single operand becomes a one-element argument tuple.
        let listed = crate::core::container::subscript(list_type, str_type);
        assert_eq!(value::repr(listed), "list[str]");
        assert_eq!(
            value::instance_attr(listed, "__args__")
                .and_then(as_tuple)
                .map(|items| items.to_vec()),
            Some(vec![str_type])
        );

        // `dict[str, int]`.
        let mapped = crate::core::container::subscript(
            dict_type,
            value::tuple_new(vec![str_type, int_type]),
        );
        assert_eq!(value::repr(mapped), "dict[str, int]");

        // `type[int]`.
        let typed = crate::core::container::subscript(type_type, int_type);
        assert_eq!(value::repr(typed), "type[int]");
        assert_eq!(call("get_origin", &[typed]), type_type);

        // An alias nests as an argument of another alias.
        let nested = crate::core::container::subscript(list_type, variadic);
        assert_eq!(value::repr(nested), "list[tuple[int, ...]]");
        let deep =
            crate::core::container::subscript(dict_type, value::tuple_new(vec![str_type, nested]));
        assert_eq!(value::repr(deep), "dict[str, list[tuple[int, ...]]]");
        assert!(!exc::pending());
    }

    #[test]
    fn generic_class_subscript_builds_an_alias_over_the_class() {
        exc::init();
        // `class Box[T]: ...` publishes the runtime parameter tuple as
        // `__type_params__`; the frontend's static and dynamic class paths both
        // store it as a class attribute.
        let parameter = call("TypeVar", &[text("T")]);
        assert!(!exc::pending());
        let class = value::class_new_full(
            "Box".to_string(),
            NONE,
            Vec::new(),
            vec![(
                "__type_params__".to_string(),
                value::tuple_new(vec![parameter]),
            )],
        );
        let int_class = value::class_new("int".to_string(), NONE, Vec::new());

        // `Box[int]` is a `_GenericAlias` carrying origin, arguments and the
        // parameters derived from those arguments.
        let alias = crate::core::container::subscript(class, int_class);
        assert!(!exc::pending(), "{:?}", pending_message());
        assert_eq!(
            value::instance_class_value(alias),
            Some(cached_class("_GenericAlias"))
        );
        assert_eq!(value::instance_attr(alias, "__origin__"), Some(class));
        assert_eq!(
            value::instance_attr(alias, "__args__")
                .and_then(as_tuple)
                .map(|items| items.to_vec()),
            Some(vec![int_class])
        );
        assert_eq!(
            value::instance_attr(alias, "__parameters__")
                .and_then(as_tuple)
                .map(|items| items.len()),
            Some(0),
            "`Box[int]` binds its parameter"
        );
        assert_eq!(value::repr(alias), "Box[int]");
        assert_eq!(call("get_origin", &[alias]), class);
        let stored_args = value::instance_attr(alias, "__args__").expect("__args__");
        assert_eq!(call("get_args", &[alias]), stored_args);

        // Specializing with a type parameter keeps it as a parameter.
        let specialized = crate::core::container::subscript(class, parameter);
        assert_eq!(
            value::instance_attr(specialized, "__parameters__")
                .and_then(as_tuple)
                .map(|items| items.to_vec()),
            Some(vec![parameter])
        );
        assert_eq!(value::repr(specialized), "Box[~T]");

        // A subclass inherits the parameters, like CPython's inherited
        // `__class_getitem__`.
        let subclass = value::class_new("Derived".to_string(), class, Vec::new());
        let inherited = crate::core::container::subscript(subclass, int_class);
        assert_eq!(
            value::instance_attr(inherited, "__origin__"),
            Some(subclass)
        );

        // `class Child(Box[int])`: class creation specializes the base to its
        // origin instead of rejecting the alias.
        let child = value::class_new("Child".to_string(), alias, Vec::new());
        assert!(!exc::pending(), "{:?}", pending_message());
        assert_eq!(value::class_display_name(child), Some("Child"));
        let base = as_class(child).map(|state| state.base).unwrap_or(NONE);
        assert_eq!(value::class_display_name(base), Some("Box"));

        // A class without `__type_params__` keeps the existing tolerance: the
        // class value itself is the alias.
        let plain = value::class_new("Plain".to_string(), NONE, Vec::new());
        assert_eq!(crate::core::container::subscript(plain, int_class), plain);
        assert!(!exc::pending());
    }

    #[test]
    fn non_generic_builtins_stay_unsubscriptable() {
        exc::init();
        // `print` is a function value at runtime and takes no type parameters.
        let print_type = value::builtin_function_new(Builtin::Print);
        assert_eq!(
            crate::core::container::subscript(print_type, text("int")),
            NONE
        );
        assert_eq!(exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        assert_eq!(pending_message(), "'function' object is not subscriptable");
        exc::clear();
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
    fn assert_never_always_raises_assertion_error() {
        exc::init();
        call("assert_never", &[text("boom")]);
        assert_eq!(exc::pending_tag(), crate::exc::ASSERTION_ERROR as i64);
        assert_eq!(
            pending_message(),
            "Expected code to be unreachable, but got: 'boom'"
        );
        exc::clear();
        assert!(!exc::pending());
    }

    #[test]
    fn assert_never_truncates_the_rendered_argument() {
        exc::init();
        // `repr` adds the surrounding quotes, so a 101-character string is one
        // over the limit and keeps only its first 100 rendered characters.
        let long = text(&"x".repeat(101));
        let rendered = format!("'{long_repr}'", long_repr = "x".repeat(101));
        let limit = ASSERT_NEVER_REPR_MAX_LENGTH as usize;
        let expected = format!(
            "Expected code to be unreachable, but got: {}...",
            &rendered[..limit]
        );
        call("assert_never", &[long]);
        assert_eq!(exc::pending_tag(), crate::exc::ASSERTION_ERROR as i64);
        assert_eq!(pending_message(), expected);

        // Exactly at the limit nothing is truncated.
        exc::clear();
        let short = text(&"y".repeat(limit - 2));
        let short_repr = format!("'{}'", "y".repeat(limit - 2));
        assert_eq!(short_repr.chars().count(), limit);
        call("assert_never", &[short]);
        assert_eq!(
            pending_message(),
            format!("Expected code to be unreachable, but got: {short_repr}")
        );
        exc::clear();
    }

    #[test]
    fn decorators_are_identity_and_typevar_attributes_are_stored() {
        exc::init();
        let function = text("f");
        assert_eq!(call("runtime_checkable", &[function]), function);
        assert_eq!(call("overload", &[function]), function);
        // `NewType` is a real class on 3.10+ (see
        // `newtype_is_a_class_with_cpython_attributes`).
        let newtype = call("NewType", &[text("TaskID"), function]);
        assert_eq!(value::instance_class_value(newtype), Some(newtype_class()));

        let plain = call("TypeVar", &[text("T")]);
        assert!(!exc::pending());
        assert_eq!(value::instance_class_value(plain), Some(typevar_class()));
        assert_eq!(
            value::instance_attr(plain, "__name__").and_then(value::as_str),
            Some("T")
        );
        assert_eq!(
            as_tuple(value::instance_attr(plain, "__constraints__").expect("__constraints__"))
                .expect("tuple")
                .len(),
            0
        );
        assert_eq!(value::instance_attr(plain, "__bound__"), Some(NONE));
        assert_eq!(value::instance_attr(plain, "__covariant__"), Some(FALSE));
        assert_eq!(
            value::instance_attr(plain, "__contravariant__"),
            Some(FALSE)
        );
        assert_eq!(
            value::instance_attr(plain, "__infer_variance__"),
            Some(FALSE)
        );
        assert_eq!(value::instance_attr(plain, "__default__"), Some(NONE));
        assert_eq!(
            value::instance_attr(plain, "__module__").and_then(value::as_str),
            Some("typing")
        );
        assert_eq!(value::as_str(typevar_repr(plain)), Some("~T"));

        // CPython allocates a fresh type variable per call.
        let again = call("TypeVar", &[text("T")]);
        assert_ne!(again, plain);
        assert!(!exc::pending());

        // Positional arguments after the name are constraints.
        let constrained = call("TypeVar", &[text("K"), text("int"), text("str")]);
        assert!(!exc::pending());
        let constraints = as_tuple(
            value::instance_attr(constrained, "__constraints__").expect("__constraints__"),
        )
        .expect("tuple");
        assert_eq!(constraints.len(), 2);
        assert_eq!(value::as_str(constraints[0]), Some("int"));
        assert_eq!(value::as_str(constraints[1]), Some("str"));

        // Keyword-only flags are stored as given.
        let parameter = call_kw(
            "TypeVar",
            &[text("V")],
            &[
                ("bound".to_string(), text("int")),
                ("covariant".to_string(), TRUE),
                ("infer_variance".to_string(), TRUE),
                ("default".to_string(), text("str")),
                ("module".to_string(), text("app")),
            ],
        );
        assert!(!exc::pending());
        assert_eq!(
            value::instance_attr(parameter, "__bound__").and_then(value::as_str),
            Some("int")
        );
        assert_eq!(value::instance_attr(parameter, "__covariant__"), Some(TRUE));
        assert_eq!(
            value::instance_attr(parameter, "__contravariant__"),
            Some(FALSE)
        );
        assert_eq!(
            value::instance_attr(parameter, "__infer_variance__"),
            Some(TRUE)
        );
        assert_eq!(
            value::instance_attr(parameter, "__default__").and_then(value::as_str),
            Some("str")
        );
        assert_eq!(
            value::instance_attr(parameter, "__module__").and_then(value::as_str),
            Some("app")
        );

        // `name` may be keyword-passed by the module binder.
        let named = call_kw("TypeVar", &[], &[("name".to_string(), text("N"))]);
        assert!(!exc::pending());
        assert_eq!(
            value::instance_attr(named, "__name__").and_then(value::as_str),
            Some("N")
        );
    }

    #[test]
    fn no_type_check_stamps_functions_and_classes() {
        exc::init();
        // `@typing.no_type_check` on a function: identity result, stamped.
        let function = value::identity_function_new("f");
        let function_roots = heap::root_values(&[function]);
        assert_eq!(call("no_type_check", &[function]), function);
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(
            value::function_attribute(function, "__no_type_check__"),
            Some(TRUE)
        );
        // Keyword form (CPython's parameter is named `arg`).
        let other = value::identity_function_new("g");
        let other_roots = heap::root_values(&[other]);
        assert_eq!(
            call_kw("no_type_check", &[], &[("arg".to_string(), other)]),
            other
        );
        assert_eq!(
            value::function_attribute(other, "__no_type_check__"),
            Some(TRUE)
        );

        // The module attribute and the `_typing` alias expose the decorator,
        // and the spec backs its call dispatch (a native function value).
        for module in ["typing", "_typing"] {
            let exported = crate::modules::module_get_by_name(module, "no_type_check");
            assert!(!exc::pending(), "{}", pending_message());
            assert!(value::as_function(exported).is_some());
            assert_eq!(value::function_name(exported), "no_type_check");
        }
        let exported = crate::modules::module_get_by_name("typing", "no_type_check");
        let target = value::identity_function_new("h");
        let roots = heap::root_values(&[exported, target]);
        let decorated = crate::builtins::call(exported, 1, [target].as_ptr());
        heap::release_roots(roots);
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(decorated, target);
        assert_eq!(
            value::function_attribute(target, "__no_type_check__"),
            Some(TRUE)
        );

        // Native callables reject the attribute assignment, like CPython
        // (its `except TypeError` does not cover `AttributeError`).
        let native = value::native_function_new("typing", "get_type_hints", 0);
        call("no_type_check", &[native]);
        assert_eq!(exc::pending_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
        exc::clear();
        heap::release_roots(other_roots);
        heap::release_roots(function_roots);
    }

    #[test]
    fn no_type_check_stamps_a_class_namespace() {
        exc::init();
        let method = value::identity_function_new("method");
        let nested = value::class_new("Nested".to_string(), NONE, Vec::new());
        let class = value::class_new_full(
            "Model".to_string(),
            NONE,
            vec![("method".to_string(), method, hydro_ir::MethodKind::Normal)],
            vec![("nested".to_string(), nested)],
        );
        let roots = heap::root_values(&[class]);
        assert_eq!(call("no_type_check", &[class]), class);
        assert!(!exc::pending(), "{}", pending_message());
        assert!(matches!(
            value::class_lookup(class, "__no_type_check__"),
            Some(value::ClassEntry::Attr(flag)) if flag == TRUE
        ));
        assert_eq!(
            value::function_attribute(method, "__no_type_check__"),
            Some(TRUE),
            "directly defined methods are stamped"
        );
        assert!(matches!(
            value::class_lookup(nested, "__no_type_check__"),
            Some(value::ClassEntry::Attr(flag)) if flag == TRUE
        ));
        heap::release_roots(roots);
    }

    #[test]
    fn no_type_check_decorator_wraps_and_marks_results() {
        exc::init();
        let identity = value::identity_function_new("decorator");
        let identity_roots = heap::root_values(&[identity]);
        let wrapped = call("no_type_check_decorator", &[identity]);
        assert!(!exc::pending(), "{}", pending_message());
        assert!(value::as_function(wrapped).is_some());
        assert_eq!(value::function_name(wrapped), "wrapped_decorator");

        // The wrapper calls the wrapped decorator, then stamps its result.
        let target = value::identity_function_new("decorated");
        let roots = heap::root_values(&[wrapped, target]);
        let decorated = crate::builtins::call(wrapped, 1, [target].as_ptr());
        heap::release_roots(roots);
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(decorated, target);
        assert_eq!(
            value::function_attribute(target, "__no_type_check__"),
            Some(TRUE)
        );

        // Keyword form, and a missing argument raises (CPython does not
        // validate the callable until the wrapper runs).
        let keyword = call_kw(
            "no_type_check_decorator",
            &[],
            &[("decorator".to_string(), identity)],
        );
        assert!(!exc::pending(), "{}", pending_message());
        assert!(value::as_function(keyword).is_some());
        call("no_type_check_decorator", &[]);
        assert_eq!(exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        exc::clear();
        heap::release_roots(identity_roots);
    }

    #[test]
    fn typing_all_lists_the_public_module_surface() {
        exc::init();
        let all = constant("__all__");
        assert!(!exc::pending(), "{}", pending_message());
        let names = value::as_list(all).expect("__all__ must be a list");
        let public: Vec<&str> = names
            .iter()
            .filter_map(|item| value::as_str(*item))
            .collect();
        for expected in ["Any", "NewType", "Union", "no_type_check", "overload"] {
            assert!(public.contains(&expected), "__all__ is missing {expected}");
        }
        assert!(
            public.iter().all(|name| !name.starts_with('_')),
            "__all__ must not list private names"
        );
        // The spec constant makes the module attribute readable, and the
        // `_typing` alias shares it.
        for module in ["typing", "_typing"] {
            let exported = crate::modules::module_get_by_name(module, "__all__");
            assert!(!exc::pending(), "{}", pending_message());
            assert!(value::as_list(exported).is_some());
        }
    }

    #[test]
    fn newtype_is_a_class_with_cpython_attributes() {
        exc::init();
        let class = crate::modules::module_get_by_name("typing", "NewType");
        assert!(!exc::pending(), "{}", pending_message());
        assert!(
            value::as_class(class).is_some(),
            "typing.NewType must be a class value"
        );
        assert_eq!(value::class_display_name(class), Some("NewType"));
        // Module reads share one identity (`typing_extensions.NewType is
        // typing.NewType` on 3.10+).
        assert_eq!(
            crate::modules::module_get_by_name("typing", "NewType"),
            class
        );

        let supertype = value::class_new("int".to_string(), NONE, Vec::new());
        let value_roots = heap::root_values(&[class, supertype]);
        let arguments = [text("UserId"), supertype];
        let argument_roots = heap::root_values(&arguments);
        let newtype = crate::builtins::call(class, 2, arguments.as_ptr());
        heap::release_roots(argument_roots);
        assert!(!exc::pending(), "{}", pending_message());
        let newtype_roots = heap::root_values(&[newtype]);
        assert_eq!(value::instance_class_value(newtype), Some(class));
        assert_eq!(
            value::instance_attr(newtype, "__name__").and_then(value::as_str),
            Some("UserId")
        );
        assert_eq!(
            value::instance_attr(newtype, "__qualname__").and_then(value::as_str),
            Some("UserId")
        );
        assert_eq!(
            value::instance_attr(newtype, "__supertype__"),
            Some(supertype)
        );
        // A unit test has no generated activation, so `__module__` falls back
        // to `__main__` (CPython's `_caller()` default for a missing frame).
        assert_eq!(
            value::instance_attr(newtype, "__module__").and_then(value::as_str),
            Some("__main__")
        );
        assert_eq!(value::repr(newtype), "__main__.UserId");

        // `isinstance(nt, typing.NewType)` — pydantic's `is_newtype`.
        let isinstance_args = [newtype, class];
        let isinstance_roots = heap::root_values(&isinstance_args);
        assert_eq!(
            crate::builtins::call_builtin(Builtin::Isinstance, 2, isinstance_args.as_ptr()),
            TRUE
        );
        heap::release_roots(isinstance_roots);
        assert!(!exc::pending(), "{}", pending_message());

        // The value is callable as CPython's `_idfunc`: the identity, not a
        // `supertype` conversion.
        let argument = value::encode_int(7).expect("representable int");
        let call_args = [argument];
        let call_roots = heap::root_values(&call_args);
        assert_eq!(
            crate::builtins::call(newtype, 1, call_args.as_ptr()),
            argument
        );
        heap::release_roots(call_roots);
        assert!(!exc::pending(), "{}", pending_message());

        // Class construction and the module-call spelling agree, including
        // the dotted-name rule (`__qualname__` keeps the full name).
        let module_call = call("NewType", &[text("pkg.UserId"), supertype]);
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(value::instance_class_value(module_call), Some(class));
        assert_eq!(
            value::instance_attr(module_call, "__name__").and_then(value::as_str),
            Some("UserId")
        );
        assert_eq!(
            value::instance_attr(module_call, "__qualname__").and_then(value::as_str),
            Some("pkg.UserId")
        );
        assert_eq!(
            value::instance_attr(module_call, "__supertype__"),
            Some(supertype)
        );

        // Keyword construction through the module binder.
        let keyword = call_kw(
            "NewType",
            &[],
            &[
                ("name".to_string(), text("Key")),
                ("tp".to_string(), supertype),
            ],
        );
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(
            value::instance_attr(keyword, "__name__").and_then(value::as_str),
            Some("Key")
        );

        // Both arguments are required, and the name must be a string.
        exc::clear();
        call("NewType", &[text("Only")]);
        assert_eq!(exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        exc::clear();
        call("NewType", &[value::encode_int(1).expect("int"), supertype]);
        assert_eq!(exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        exc::clear();
        heap::release_roots(newtype_roots);
        heap::release_roots(value_roots);
        assert!(!exc::pending());
    }

    #[test]
    fn typevar_validates_name_and_variance() {
        exc::init();
        call("TypeVar", &[value::encode_int(1).unwrap()]);
        assert_eq!(exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        exc::clear();
        call("TypeVar", &[]);
        assert_eq!(exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        exc::clear();
        // Both variance flags are rejected together.
        call_kw(
            "TypeVar",
            &[text("T")],
            &[
                ("covariant".to_string(), TRUE),
                ("contravariant".to_string(), TRUE),
            ],
        );
        assert_eq!(exc::pending_tag(), crate::exc::VALUE_ERROR as i64);
        exc::clear();
        call_kw("TypeVar", &[text("T")], &[("bogus".to_string(), NONE)]);
        assert_eq!(exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        exc::clear();
        call_kw("TypeVar", &[text("T")], &[("name".to_string(), text("S"))]);
        assert_eq!(exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn type_alias_type_stores_attributes_and_reprs_as_its_name() {
        exc::init();
        let class = constant("TypeAliasType");
        assert_eq!(
            as_class(class).map(|state| state.name.clone()).as_deref(),
            Some("TypeAliasType")
        );
        // Cached: `isinstance` targets and construction share one identity.
        assert_eq!(constant("TypeAliasType"), class);
        assert!(!exc::pending());

        let value = constant("Tuple");
        let empty = value::tuple_new(Vec::new());
        let alias = call(
            "_type_alias_new",
            &[text("Shape"), value, empty, text("__main__")],
        );
        assert!(!exc::pending(), "{:?}", pending_message());
        assert_eq!(value::instance_class_value(alias), Some(class));
        assert_eq!(
            value::instance_attr(alias, "__name__").and_then(value::as_str),
            Some("Shape")
        );
        assert_eq!(value::instance_attr(alias, "__value__"), Some(value));
        assert_eq!(
            value::instance_attr(alias, "__type_params__")
                .and_then(as_tuple)
                .map(|items| items.len()),
            Some(0)
        );
        assert_eq!(
            value::instance_attr(alias, "__parameters__")
                .and_then(as_tuple)
                .map(|items| items.len()),
            Some(0)
        );
        assert_eq!(
            value::instance_attr(alias, "__module__").and_then(value::as_str),
            Some("__main__")
        );
        assert_eq!(value::repr(alias), "Shape");

        // A non-generic alias rejects subscription like CPython.
        let class_int = value::class_new("int".to_string(), NONE, Vec::new());
        let rejected = crate::core::container::subscript(alias, class_int);
        assert_eq!(rejected, NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "Only generic type aliases are subscriptable"
        );
        exc::clear();
    }

    #[test]
    fn generic_type_alias_is_subscriptable_and_composes_into_unions() {
        exc::init();
        let param = call("TypeVar", &[text("T")]);
        assert!(!exc::pending());
        let params = value::tuple_new(vec![param]);
        let tuple_class = constant("Tuple");
        let alias = call(
            "_type_alias_new",
            &[text("Vector"), tuple_class, params, text("pkg")],
        );
        assert!(!exc::pending(), "{:?}", pending_message());
        assert_eq!(
            value::instance_attr(alias, "__type_params__")
                .and_then(as_tuple)
                .map(|items| items.to_vec()),
            Some(vec![param])
        );
        assert_eq!(
            value::instance_attr(alias, "__module__").and_then(value::as_str),
            Some("pkg")
        );

        // `Vector[int]` is a `_GenericAlias` over the alias value.
        let class_int = value::class_new("int".to_string(), NONE, Vec::new());
        let specialized = crate::core::container::subscript(alias, class_int);
        assert!(!exc::pending(), "{:?}", pending_message());
        assert_eq!(value::instance_attr(specialized, "__origin__"), Some(alias));
        assert_eq!(
            value::instance_attr(specialized, "__args__")
                .and_then(as_tuple)
                .map(|items| items.to_vec()),
            Some(vec![class_int])
        );

        // A PEP 695 alias is a union member on either operand side.
        let union = crate::modules::types::union_binary_or(alias, class_int).expect("union");
        assert_eq!(value::repr(union), "Vector | int");
        let union = crate::modules::types::union_binary_or(class_int, alias).expect("union");
        assert_eq!(value::repr(union), "int | Vector");
        assert!(!exc::pending());
    }

    #[test]
    fn type_alias_constructor_validates_name_type_params_and_module() {
        exc::init();
        let class = constant("TypeAliasType");
        let instance = value::instance_new(class);
        let empty = value::tuple_new(Vec::new());
        let bad_name = populate_type_alias(
            instance,
            value::encode_int(1).expect("int"),
            NONE,
            empty,
            NONE,
        );
        assert_eq!(bad_name, NONE);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        assert_eq!(
            pending_message(),
            "typealias() argument 'name' must be str, not int"
        );
        exc::clear();

        let bad_params = populate_type_alias(instance, text("X"), NONE, NONE, NONE);
        assert_eq!(bad_params, NONE);
        assert_eq!(pending_message(), "type_params must be a tuple");
        exc::clear();

        // `_type_alias_new` defaults `__module__` to the entry module.
        let alias = call("_type_alias_new", &[text("X"), NONE, empty, NONE]);
        assert!(!exc::pending(), "{:?}", pending_message());
        assert_eq!(
            value::instance_attr(alias, "__module__").and_then(value::as_str),
            Some("__main__")
        );
    }

    #[test]
    fn type_var_tuple_is_iterable_and_yields_unpack() {
        exc::init();
        let tuple_var = call("TypeVarTuple", &[text("Ts")]);
        assert!(!exc::pending());
        let iter = crate::core::iter::iter_of(tuple_var);
        let first = crate::core::iter::next_value(iter, NONE, false);
        assert!(!exc::pending(), "{:?}", pending_message());
        assert_eq!(value::class_display_name(first), Some("Unpack"));
        let end = crate::core::iter::next_value(iter, NONE, true);
        assert!(!exc::pending());
        assert_eq!(end, NONE);
        // `tuple[*Ts]` lowers to `tuple(Ts)`, so the conversion consumes the
        // same iteration protocol and produces the one-element member tuple.
        let converted = crate::core::builtins::tuple_from(tuple_var);
        assert!(!exc::pending(), "{:?}", pending_message());
        let converted_items = as_tuple(converted).expect("tuple");
        assert_eq!(converted_items.len(), 1);
        assert_eq!(
            value::class_display_name(converted_items[0]),
            Some("Unpack")
        );
        // `ParamSpec` is not iterable, like CPython's.
        let param = call("ParamSpec", &[text("P")]);
        crate::core::iter::iter_of(param);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
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
        let _heap_guard = crate::heap::heap_reset_test_guard();
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
        assert!(!exc::pending(), "{}", pending_message());
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

    #[test]
    fn module_call_form_builds_the_concatenate_alias() {
        exc::init();
        let origin = constant("Callable");
        let parameter = call("ParamSpec", &[text("P")]);
        let arguments = value::tuple_new(vec![constant("Any"), parameter]);
        let typevar_types = value::tuple_new(vec![constant("Any")]);
        let alias = call_kw(
            "_ConcatenateGenericAlias",
            &[origin, arguments],
            &[
                ("_typevar_types".to_string(), typevar_types),
                ("_paramspec_tvars".to_string(), TRUE),
            ],
        );
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(
            value::instance_class_value(alias),
            Some(cached_class("_ConcatenateGenericAlias"))
        );
        assert_eq!(value::instance_attr(alias, "__origin__"), Some(origin));
        assert_eq!(
            value::instance_attr(alias, "_typevar_types"),
            Some(typevar_types)
        );
        assert_eq!(value::instance_attr(alias, "_paramspec_tvars"), Some(TRUE));

        // The module entry defers to the same class constructor, which rejects
        // an unknown keyword instead of dropping it.
        exc::init();
        let _ = call_kw(
            "_ConcatenateGenericAlias",
            &[origin, arguments],
            &[("bogus".to_string(), NONE)],
        );
        assert_eq!(exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        assert!(pending_message().contains("unexpected keyword argument 'bogus'"));
        exc::clear();
    }

    #[test]
    fn overload_registry_reports_and_clears() {
        exc::init();
        let first = value::native_function_new("pydantic", "validate", 1);
        let second = value::native_function_new("pydantic", "validate", 1);
        // `@overload` returns the callable it registered.
        assert_eq!(call("overload", &[first]), first);
        assert_eq!(call("overload", &[second]), second);
        let listed = call("get_overloads", &[first]);
        assert!(!exc::pending(), "{}", pending_message());
        assert_eq!(
            value::as_list(listed).map(|items| items.to_vec()),
            Some(vec![first, second])
        );

        assert_eq!(call("clear_overloads", &[]), NONE);
        let cleared = call("get_overloads", &[first]);
        assert_eq!(value::as_list(cleared).map(|items| items.len()), Some(0));

        // A non-function argument is a `TypeError`, like `func.__code__`
        // access failing in CPython.
        exc::init();
        let _ = call("get_overloads", &[NONE]);
        assert_eq!(exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn disjoint_base_is_a_class_marker() {
        exc::init();
        let class = value::class_new("Node".to_string(), NONE, Vec::new());
        assert_eq!(call("disjoint_base", &[class]), class);
        assert!(!exc::pending());
        exc::init();
        let _ = call("disjoint_base", &[NONE]);
        assert_eq!(exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        exc::clear();
    }

    #[test]
    fn no_default_and_no_extra_items_are_identity_stable_sentinels() {
        exc::init();
        let no_default = constant("NoDefault");
        assert_eq!(no_default, constant("NoDefault"));
        assert_eq!(value::repr(no_default), "typing.NoDefault");
        assert_eq!(
            value::as_class(value::type_class(no_default)).map(|class| class.name.as_str()),
            Some("NoDefaultType")
        );

        let no_extra = constant("NoExtraItems");
        assert_eq!(no_extra, constant("NoExtraItems"));
        assert_ne!(no_extra, no_default);
        assert_eq!(value::repr(no_extra), "typing.NoExtraItems");
        assert_eq!(
            value::as_class(value::type_class(no_extra)).map(|class| class.name.as_str()),
            Some("NoExtraItemsType")
        );
        assert!(!exc::pending());
    }

    #[test]
    fn pydantic_type_helpers_use_runtime_class_shape() {
        exc::init();
        let plain = value::class_new("Node".to_string(), NONE, Vec::new());
        let payload = text("payload");
        assert_eq!(call("assert_type", &[payload, plain]), payload);
        assert_eq!(call("reveal_type", &[payload]), payload);
        assert_eq!(call("override", &[plain]), plain);
        assert!(matches!(
            value::class_lookup(plain, "__override__"),
            Some(value::ClassEntry::Attr(TRUE))
        ));
        assert_eq!(call("is_typeddict", &[plain]), FALSE);

        assert_eq!(call("is_typeddict", &[constant("TypedDict")]), FALSE);
        let typed_dict = value::class_new("Film".to_string(), constant("TypedDict"), Vec::new());
        assert_eq!(call("is_typeddict", &[typed_dict]), TRUE);

        let method = value::native_function_new("test", "read", 1);
        let marker = text("marker");
        let protocol = value::class_new_full(
            "Readable".to_string(),
            constant("Protocol"),
            vec![("read".to_string(), method, hydro_ir::MethodKind::Normal)],
            vec![("name".to_string(), marker)],
        );
        assert_eq!(call("is_protocol", &[protocol]), TRUE);
        let members = call("get_protocol_members", &[protocol]);
        let names: Vec<&str> = value::as_set(members)
            .expect("set")
            .iter()
            .filter_map(|item| value::as_str(*item))
            .collect();
        assert_eq!(names, vec!["name", "read"]);
        assert_eq!(call("is_protocol", &[plain]), FALSE);
        assert!(!exc::pending());

        // `Protocol` itself and non-class arguments are not protocols.
        assert_eq!(call("is_protocol", &[constant("Protocol")]), FALSE);
        exc::init();
        let _ = call("get_protocol_members", &[plain]);
        assert_eq!(exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        assert!(pending_message().contains("is not a Protocol"));
        exc::clear();
    }

    #[test]
    fn typed_dict_metadata_and_rename_match_the_typing_extensions_shape() {
        exc::init();
        let typed_dict_base = constant("TypedDict");
        // `_TypedDictMeta.__new__` allocates under the placeholder name
        // `Protocol` and immediately renames the class. The writable name slot
        // must win over the frozen definition name everywhere it is observed.
        let class = value::class_new("Protocol".to_string(), typed_dict_base, Vec::new());
        assert!(value::class_set_attr(
            class,
            "__name__".to_string(),
            text("ConfigDict")
        ));
        assert_eq!(value::class_display_name(class), Some("ConfigDict"));
        assert_eq!(value::class_qualname(class), Some("ConfigDict"));
        assert_eq!(value::repr(class), "<class 'ConfigDict'>");

        // The metaclass publishes the field mapping and key sets afterwards.
        let int_field = value::class_new("int".to_string(), NONE, Vec::new());
        let annotations = value::dict_new(vec![(text("x"), int_field)]);
        value::class_set_attr(class, "__annotations__".to_string(), annotations);
        value::class_set_attr(
            class,
            "__required_keys__".to_string(),
            value::set_new(vec![text("x")]),
        );
        value::class_set_attr(
            class,
            "__optional_keys__".to_string(),
            value::set_new(vec![text("y")]),
        );
        assert_eq!(value::class_annotations(class), Some(annotations));

        // `is_typeddict` follows the TypedDict ancestry, never the placeholder
        // name that was overwritten by the rename.
        assert_eq!(call("is_typeddict", &[class]), TRUE);
        assert_eq!(call("is_typeddict", &[typed_dict_base]), FALSE);
        let plain = value::class_new("Plain".to_string(), NONE, Vec::new());
        assert_eq!(call("is_typeddict", &[plain]), FALSE);
        assert!(!exc::pending());

        // `ConfigDict(x=1)` builds a dict through the TypedDict fallback: a
        // real dict, not an instance of the class.
        let keywords = [text("x"), value::encode_int(1).expect("representable int")];
        let keyword_roots = crate::heap::root_values(&keywords);
        let built =
            crate::core::builtins::call_kw(class, 0, std::ptr::null(), 1, keywords.as_ptr());
        assert!(!exc::pending(), "{:?}", pending_message());
        assert_eq!(value::repr(built), "{'x': 1}");
        assert_ne!(value::instance_class_value(built), Some(class));
        crate::heap::release_roots(keyword_roots);
    }
}
