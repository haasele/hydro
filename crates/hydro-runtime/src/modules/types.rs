//! `types`: native placeholder classes plus `SimpleNamespace`.
//!
//! Rich imports `FrameType`, `ModuleType`, `TracebackType`, `MappingProxyType`
//! and friends almost exclusively for annotations, `isinstance` checks and
//! dictionary keys. Each name therefore materialises as an empty native class
//! whose identity is fresh on every access, like the `typing` placeholders.
//! `FrameType` and `CodeType` instead report the cached native classes of the
//! real frame and code objects, so `type(sys._getframe(0)) is
//! types.FrameType` holds on a thread.
//!
//! `CellType`, `FunctionType`, `UnionType` and `DynamicClassAttribute` are
//! cached:
//!
//! * `CellType(value)` is a native instance carrying `cell_contents`;
//! * `FunctionType` is the cached `function` class the runtime gives every
//!   function value, so `isinstance(f, types.FunctionType)` holds. The module
//!   also declares it as a constructor, so `types.FunctionType(code, globals,
//!   ...)` lowers as a module call; `isinstance` resolves the native function
//!   target back to the class. The constructor accepts a Hydro function value
//!   as `code` and returns a new delegating callable; rebinding
//!   `argdefs`/`closure`/`kwdefaults` needs a code object the runtime does not
//!   reify and raises a tagged `NotImplementedError`.
//! * `UnionType` is a class of native union values carrying `__args__`.
//!   `types.UnionType[A, B]` and `A | B` (for type-like operands: classes,
//!   `None`, function values and existing unions) construct them; a
//!   one-element union collapses to its member.
//! * `DynamicClassAttribute(fget, fset, fdel, doc)` is a real data descriptor:
//!   instance access calls `fget(instance)`, class access raises
//!   `AttributeError` (unless the getter is abstract, in which case the
//!   descriptor itself is returned), and `getter`/`setter`/`deleter` return
//!   updated copies. `inspect.getmembers` relies on this behaviour.
//!
//! `SimpleNamespace(...)` accepts one optional mapping and returns the native
//! namespace object used by `argparse`, so attribute reads work immediately.
//! CPython spells the attributes as keyword arguments; the frontend has no
//! `**kwargs` lowering for native module functions yet, so the runtime takes
//! the equivalent mapping instead of rejecting the constructor outright.
//! The module value itself is the cached `SimpleNamespace` class carrying a
//! `__repr__` member, because `Lib/pprint.py` keys its dispatch table by
//! `_types.SimpleNamespace.__repr__` while the module body runs.

use hydro_ir::{MethodKind, ParamKind};

use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, NOT_IMPLEMENTED_ERROR, TYPE_ERROR};
use crate::heap;
use crate::methods;
use crate::value::{self, NONE};

/// The class value for `types.<name>` when the name is a real class the
/// attribute lookup must return instead of the module-call constructor.
pub(crate) fn class_value(module: &str, name: &str) -> Option<i64> {
    if module == "types" && name == "DynamicClassAttribute" {
        return Some(dynamic_class_attribute_class());
    }
    // `Lib/pprint.py` registers `_dispatch[_types.SimpleNamespace.__repr__]`
    // while the module body runs. The name is declared as a module function so
    // `SimpleNamespace(...)` lowers as a module call, but the value itself
    // must be a class carrying a real `__repr__` member: a constructor
    // function in that slot raises
    // `AttributeError: 'function' object has no attribute '__repr__'`.
    if module == "types" && name == "SimpleNamespace" {
        return Some(simple_namespace_class());
    }
    None
}

/// Names exposed as empty native classes.
const CLASSES: &[&str] = &[
    "AsyncGeneratorType",
    "BuiltinFunctionType",
    "BuiltinMethodType",
    // Python-2 compatibility alias kept for `six`'s guarded import path.
    "ClassType",
    "ClassMethodDescriptorType",
    "CoroutineType",
    "EllipsisType",
    "GeneratorType",
    "GenericAlias",
    "GetSetDescriptorType",
    "LambdaType",
    "MappingProxyType",
    "MemberDescriptorType",
    "MethodDescriptorType",
    "MethodType",
    "MethodWrapperType",
    "NoneType",
    "TracebackType",
    "WrapperDescriptorType",
];

/// Materialises the module constant `name`.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "CellType" => cell_type_class(),
        // Reified frames and code objects report these real native classes, so
        // `type(sys._getframe(0)) is types.FrameType` holds on a thread.
        "FrameType" => value::frame_class(),
        "CodeType" => value::code_class(),
        // Exposed as a module function too, so that `types.FunctionType(...)`
        // lowers as a call; the class form still answers direct lookups and
        // the `isinstance` route through `type_check_target`.
        "FunctionType" => function_type_class(),
        "UnionType" => union_type_class(),
        "DynamicClassAttribute" => dynamic_class_attribute_class(),
        // The module value is the cached class (see [`class_value`]); the
        // constructor remains the module call handled by [`call`].
        "SimpleNamespace" => simple_namespace_class(),
        // `types.ModuleType(name[, doc])`: a real class with a module-shaped
        // initialiser, so subclasses (`six._LazyModule`) can call
        // `super().__init__(name)`.
        "ModuleType" => module_type_class(),
        "NoneType" => value::type_class(NONE),
        "EllipsisType" => value::type_class(crate::modules::builtins::constant("Ellipsis")),
        "NotImplementedType" => value::type_class(value::NOT_IMPLEMENTED),
        _ if CLASSES.contains(&name) => value::class_new(name.to_string(), NONE, Vec::new()),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'types' has no attribute '{name}'"),
        ),
    }
}

/// The cached `types.CellType` class.
///
/// A cell is a plain native instance carrying a `cell_contents` attribute.
/// The class is cached in the runtime registry so repeated accesses share
/// identity, which `isinstance(..., types.CellType)` compares exactly like
/// CPython's native cell type.
pub(crate) fn cell_type_class() -> i64 {
    if let Some(cached) = globals::registry_get("types.CellType") {
        return cached;
    }
    let class = value::class_new("CellType".to_string(), NONE, Vec::new());
    globals::registry_set("types.CellType", class);
    class
}

/// The cached `types.ModuleType` class.
///
/// Unlike the tag-only classes in [`CLASSES`], this class carries a real
/// `__init__(self, name, doc=None)` so a Python subclass (`six._LazyModule`)
/// can initialise its module attributes through `super().__init__`.
pub(crate) fn module_type_class() -> i64 {
    if let Some(cached) = globals::registry_get("types.ModuleType") {
        return cached;
    }
    let init = value::native_function_new("types", "ModuleType.__init__", 3);
    let class = value::class_new_full(
        "ModuleType".to_string(),
        NONE,
        vec![("__init__".to_string(), init, MethodKind::Normal)],
        Vec::new(),
    );
    globals::registry_set("types.ModuleType", class);
    class
}

/// `types.ModuleType.__init__(self, name, doc=None)`.
fn module_type_init(args: &[i64]) -> i64 {
    let Some(instance) = args.first().copied() else {
        return exc::raise_with_message(TYPE_ERROR, "ModuleType.__init__ missing self");
    };
    let Some(name) = args.get(1).copied().filter(|value| *value != NONE) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "module() missing required argument 'name' (pos 1)",
        );
    };
    let Some(name) = value::as_str(name) else {
        return exc::raise_with_message(TYPE_ERROR, "module name must be a string");
    };
    let doc = args.get(2).copied().unwrap_or(NONE);
    let name = value::str_alloc(name.to_string());
    let roots = heap::root_values(&[name, doc]);
    value::instance_setattr(instance, "__name__".to_string(), name);
    value::instance_setattr(instance, "__doc__".to_string(), doc);
    value::instance_setattr(instance, "__package__".to_string(), NONE);
    value::instance_setattr(instance, "__loader__".to_string(), NONE);
    value::instance_setattr(instance, "__spec__".to_string(), NONE);
    heap::release_roots(roots);
    NONE
}

/// `types.CellType([value])`: a cell object holding `value` (empty when
/// omitted).
fn cell_type_new(args: &[i64]) -> i64 {
    if args.len() > 1 {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("CellType() takes at most 1 argument ({} given)", args.len()),
        );
    }
    let class = cell_type_class();
    let instance = value::instance_new(class);
    let Some(&contents) = args.first() else {
        return instance;
    };
    // `setattr` may allocate (attribute name/interning), so keep the value
    // reachable across it.
    let roots = heap::root_values(&[contents]);
    let _ = methods::setattr(
        instance,
        b"cell_contents".as_ptr(),
        "cell_contents".len(),
        contents,
    );
    heap::release_roots(roots);
    instance
}

/// Dispatches a native `types` function. Arity is enforced by the central spec.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    if let Some(result) = dynamic_class_attribute_method(function, args) {
        return result;
    }
    match function {
        "SimpleNamespace" => simple_namespace(args.first().copied().unwrap_or(NONE)),
        "prepare_class" => prepare_class(args),
        "CellType" => cell_type_new(args),
        // The mapping proxy is a read-only view; returning the mapping is
        // sufficient for the `tomllib` immutable-table construction.
        "MappingProxyType" => args.first().copied().unwrap_or(NONE),
        "ModuleType.__init__" => module_type_init(args),
        // `types.FunctionType(code, globals, ...)`.
        "FunctionType" => function_type_new(args),
        // `types.UnionType[a, b, ...]` through the module-call path (the
        // subscript spelling goes through [`union_subscript`]).
        "UnionType" => union_new(args),
        "DynamicClassAttribute" => new_dynamic_class_attribute(
            args.first().copied().unwrap_or(NONE),
            args.get(1).copied().unwrap_or(NONE),
            args.get(2).copied().unwrap_or(NONE),
            args.get(3).copied().unwrap_or(NONE),
        ),
        "get_original_bases" => get_original_bases(args.first().copied().unwrap_or(NONE)),
        "resolve_bases" => args.first().copied().unwrap_or(NONE),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'types' has no attribute '{function}'"),
        ),
    }
}

/// `types.prepare_class(name, bases)`.
///
/// CPython returns `(metaclass, namespace, kwds)` from the class-creation
/// protocol. Hydro's class creation resolves the metaclass from the bases and
/// has no separate `__prepare__` handoff for this API yet; return the resolved
/// metaclass and empty mappings so generic-model code can build the namespace
/// it expects.
fn prepare_class(args: &[i64]) -> i64 {
    let bases = args.get(1).copied().unwrap_or(NONE);
    let first = value::as_tuple(bases)
        .and_then(|items| items.first().copied())
        .unwrap_or(NONE);
    let metaclass = value::class_metaclass(first);
    let metaclass = if metaclass == NONE {
        value::native_type_class()
    } else {
        metaclass
    };
    let namespace = value::dict_new(Vec::new());
    let kwds = if args.len() > 2 {
        args[2]
    } else {
        value::dict_new(Vec::new())
    };
    value::tuple_new(vec![metaclass, namespace, kwds])
}

/// Keyword-aware `types` dispatch: `DynamicClassAttribute` accepts the four
/// CPython parameter names.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if function == "SimpleNamespace" {
        if args.len() > 1 {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "SimpleNamespace() takes at most 1 positional argument ({} given)",
                    args.len()
                ),
            );
        }
        let namespace = simple_namespace(args.first().copied().unwrap_or(NONE));
        if exc::pending() {
            return NONE;
        }
        for (name, item) in keywords {
            value::instance_setattr(namespace, name.clone(), *item);
        }
        return namespace;
    }
    if function == "prepare_class" {
        let mut bound = args.to_vec();
        for (name, item) in keywords {
            match name.as_str() {
                "name" => {
                    if !bound.is_empty() {
                        bound[0] = *item;
                    } else {
                        bound.push(*item);
                    }
                }
                "bases" => {
                    if bound.len() > 1 {
                        bound[1] = *item;
                    } else {
                        bound.push(*item);
                    }
                }
                "kwds" => {
                    while bound.len() < 2 {
                        bound.push(NONE);
                    }
                    if bound.len() > 2 {
                        bound[2] = *item;
                    } else {
                        bound.push(*item);
                    }
                }
                other => {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!("prepare_class() got an unexpected keyword argument '{other}'"),
                    )
                }
            }
        }
        if bound.len() < 2 {
            return exc::raise_with_message(
                TYPE_ERROR,
                "prepare_class() missing required arguments",
            );
        }
        return prepare_class(&bound);
    }
    if function != "DynamicClassAttribute" {
        return exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'types' has no attribute '{function}'"),
        );
    }
    const PARAMS: [&str; 4] = ["fget", "fset", "fdel", "doc"];
    if args.len() > PARAMS.len() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "DynamicClassAttribute() takes at most 4 arguments ({} given)",
                args.len()
            ),
        );
    }
    let mut slots: [Option<i64>; 4] = [None; 4];
    for (index, argument) in args.iter().enumerate() {
        slots[index] = Some(*argument);
    }
    for (name, item) in keywords {
        let Some(index) = PARAMS.iter().position(|param| param == name) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("DynamicClassAttribute() got an unexpected keyword argument '{name}'"),
            );
        };
        if slots[index].is_some() {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("DynamicClassAttribute() got multiple values for argument '{name}'"),
            );
        }
        slots[index] = Some(*item);
    }
    let fget = slots[0].unwrap_or(NONE);
    let fset = slots[1].unwrap_or(NONE);
    let fdel = slots[2].unwrap_or(NONE);
    let doc = slots[3].unwrap_or(NONE);
    new_dynamic_class_attribute(fget, fset, fdel, doc)
}

/// `types.get_original_bases(cls, /)`: the class's own `__orig_bases__` when
/// it has one (class-body assignment or a class-level attribute), otherwise
/// `__bases__`. A base-less class reports `(object,)` like CPython's implicit
/// edge. Class creation records the declared bases in `__orig_bases__` when
/// `__mro_entries__` rewrote them, so a special-form base is reported as
/// declared rather than as the classes it expanded into. (A generic alias is
/// still normalized to its origin before class creation, so it reports the
/// origin unless the class namespace recorded the alias itself.)
fn get_original_bases(class: i64) -> i64 {
    let Some(state) = value::as_class(class) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "Expected an instance of type, not '{}'",
                value::type_name(class)
            ),
        );
    };
    if let Some(original) = state
        .attrs
        .iter()
        .find(|(name, _)| name.as_str() == "__orig_bases__")
        .map(|(_, value)| *value)
    {
        return original;
    }
    let original = state
        .lock_overlay()
        .iter()
        .find(|(name, _)| name.as_str() == "__orig_bases__")
        .map(|(_, value)| *value);
    if let Some(original) = original {
        return original;
    }
    if state.name == "object" || !state.bases.is_empty() {
        return value::tuple_new(state.bases.clone());
    }
    value::tuple_new(vec![value::object_class()])
}

/// The cached `types.DynamicClassAttribute` class.
pub(crate) fn dynamic_class_attribute_class() -> i64 {
    if let Some(cached) = globals::registry_get("types.DynamicClassAttribute") {
        return cached;
    }
    let mut signature = Vec::with_capacity(5);
    signature.push(value::ParamMeta {
        name: "self".to_string(),
        kind: ParamKind::Positional,
        default: None,
    });
    for name in ["fget", "fset", "fdel", "doc"] {
        signature.push(value::ParamMeta {
            name: name.to_string(),
            kind: ParamKind::Positional,
            default: Some(NONE),
        });
    }
    let init = value::native_function_new("types", "_dca_init", usize::MAX);
    value::function_set_signature(init, signature);
    value::function_set_full_entry(init, init_dynamic_class_attribute as *const () as usize);
    let get = method_entry(
        "types",
        "_dca_get",
        get_dynamic_class_attribute_entry,
        &["self", "obj", "owner"],
    );
    let set = method_entry(
        "types",
        "_dca_set",
        set_dynamic_class_attribute_entry,
        &["self", "obj", "value"],
    );
    let delete = method_entry(
        "types",
        "_dca_delete",
        delete_dynamic_class_attribute_entry,
        &["self", "obj"],
    );
    let getter = method_entry(
        "types",
        "_dca_getter",
        getter_dynamic_class_attribute_entry,
        &["self", "fget"],
    );
    let setter = method_entry(
        "types",
        "_dca_setter",
        setter_dynamic_class_attribute_entry,
        &["self", "fset"],
    );
    let deleter = method_entry(
        "types",
        "_dca_deleter",
        deleter_dynamic_class_attribute_entry,
        &["self", "fdel"],
    );
    let class = value::class_new_full(
        "DynamicClassAttribute".to_string(),
        NONE,
        vec![
            ("__init__".to_string(), init, MethodKind::Normal),
            ("__get__".to_string(), get, MethodKind::Normal),
            ("__set__".to_string(), set, MethodKind::Normal),
            ("__delete__".to_string(), delete, MethodKind::Normal),
            ("getter".to_string(), getter, MethodKind::Normal),
            ("setter".to_string(), setter, MethodKind::Normal),
            ("deleter".to_string(), deleter, MethodKind::Normal),
        ],
        Vec::new(),
    );
    globals::registry_set("types.DynamicClassAttribute", class);
    class
}

/// A native method with a declared signature and full-argv trampoline.
///
/// Descriptor methods are invoked through class binding, which uses the
/// trampoline directly; they are not module-call targets and therefore need
/// no central spec entry.
fn method_entry(
    module: &'static str,
    name: &'static str,
    entry: extern "C" fn(*const i64) -> i64,
    params: &[&str],
) -> i64 {
    let function = value::native_function_new(module, name, usize::MAX);
    let signature: Vec<value::ParamMeta> = params
        .iter()
        .map(|param| value::ParamMeta {
            name: (*param).to_string(),
            kind: ParamKind::Positional,
            default: None,
        })
        .collect();
    value::function_set_signature(function, signature);
    value::function_set_full_entry(function, entry as *const () as usize);
    function
}

extern "C" fn get_dynamic_class_attribute_entry(argv: *const i64) -> i64 {
    // SAFETY: the descriptor binder passes `(self, obj, owner)`.
    dynamic_class_attribute_get(unsafe { *argv }, unsafe { *argv.add(1) }, unsafe {
        *argv.add(2)
    })
}

extern "C" fn set_dynamic_class_attribute_entry(argv: *const i64) -> i64 {
    // SAFETY: the descriptor binder passes `(self, obj, value)`.
    dynamic_class_attribute_set(unsafe { *argv }, unsafe { *argv.add(1) }, unsafe {
        *argv.add(2)
    })
}

extern "C" fn delete_dynamic_class_attribute_entry(argv: *const i64) -> i64 {
    // SAFETY: the descriptor binder passes `(self, obj)`.
    dynamic_class_attribute_delete(unsafe { *argv }, unsafe { *argv.add(1) })
}

extern "C" fn getter_dynamic_class_attribute_entry(argv: *const i64) -> i64 {
    // SAFETY: the method binder passes `(self, function)`.
    dynamic_class_attribute_copy(unsafe { *argv }, 0, unsafe { *argv.add(1) })
}

extern "C" fn setter_dynamic_class_attribute_entry(argv: *const i64) -> i64 {
    dynamic_class_attribute_copy(unsafe { *argv }, 1, unsafe { *argv.add(1) })
}

extern "C" fn deleter_dynamic_class_attribute_entry(argv: *const i64) -> i64 {
    dynamic_class_attribute_copy(unsafe { *argv }, 2, unsafe { *argv.add(1) })
}

/// `DynamicClassAttribute.__init__` through the class-call binder.
extern "C" fn init_dynamic_class_attribute(argv: *const i64) -> i64 {
    // SAFETY: the class-call binder passes `self` then the four parameters.
    let instance = unsafe { *argv };
    let fget = unsafe { *argv.add(1) };
    let fset = unsafe { *argv.add(2) };
    let fdel = unsafe { *argv.add(3) };
    let doc = unsafe { *argv.add(4) };
    initialize_dynamic_class_attribute(instance, fget, fset, fdel, doc)
}

/// `DynamicClassAttribute(fget=None, fset=None, fdel=None, doc=None)`.
fn new_dynamic_class_attribute(fget: i64, fset: i64, fdel: i64, doc: i64) -> i64 {
    let class = dynamic_class_attribute_class();
    let roots = heap::root_values(&[class, fget, fset, fdel, doc]);
    let instance = value::instance_new(class);
    let instance_roots = heap::root_values(&[instance]);
    let result = initialize_dynamic_class_attribute(instance, fget, fset, fdel, doc);
    heap::release_roots(instance_roots);
    heap::release_roots(roots);
    result
}

/// Stores CPython's descriptor state: the accessors, the effective `__doc__`
/// and the `overwrite_doc` flag.
fn initialize_dynamic_class_attribute(
    instance: i64,
    fget: i64,
    fset: i64,
    fdel: i64,
    doc: i64,
) -> i64 {
    let effective_doc = if doc != NONE {
        doc
    } else if fget != NONE {
        accessor_doc(fget)
    } else {
        NONE
    };
    let is_abstract = if fget == NONE {
        value::FALSE
    } else {
        accessor_flag(fget, "__isabstractmethod__")
    };
    let overwrite_doc = doc == NONE;
    let roots = heap::root_values(&[instance, fget, fset, fdel, effective_doc]);
    for (name, item) in [
        ("fget", fget),
        ("fset", fset),
        ("fdel", fdel),
        ("__doc__", effective_doc),
        ("overwrite_doc", value::bool_value(overwrite_doc)),
        ("__isabstractmethod__", is_abstract),
    ] {
        value::instance_setattr(instance, name.to_string(), item);
    }
    heap::release_roots(roots);
    instance
}

/// Reads `obj.__doc__` without leaking a missing attribute as an exception.
fn accessor_doc(obj: i64) -> i64 {
    let doc = methods::getattr(obj, b"__doc__".as_ptr(), "__doc__".len());
    if exc::pending() {
        exc::clear();
        NONE
    } else {
        doc
    }
}

/// Reads `obj.__isabstractmethod__` as a boolean, defaulting to `False`.
fn accessor_flag(obj: i64, name: &str) -> i64 {
    let flag = methods::getattr(obj, name.as_ptr(), name.len());
    if exc::pending() {
        exc::clear();
        value::FALSE
    } else {
        value::bool_value(value::truthy(flag))
    }
}

fn dynamic_attribute_slot(instance: i64, name: &str) -> i64 {
    value::instance_attr(instance, name).unwrap_or(NONE)
}

/// `DynamicClassAttribute.__get__(instance, ownerclass=None)`.
fn dynamic_class_attribute_get(instance: i64, obj: i64, _owner: i64) -> i64 {
    if obj == NONE {
        if value::truthy(dynamic_attribute_slot(instance, "__isabstractmethod__")) {
            return instance;
        }
        return exc::raise_with_message(ATTRIBUTE_ERROR, "");
    }
    let fget = dynamic_attribute_slot(instance, "fget");
    if fget == NONE {
        return exc::raise_with_message(ATTRIBUTE_ERROR, "unreadable attribute");
    }
    let args = [obj];
    let roots = heap::root_values(&[instance, fget]);
    let result = crate::core::builtins::call(fget, args.len(), args.as_ptr());
    heap::release_roots(roots);
    result
}

/// `DynamicClassAttribute.__set__(instance, value)`.
fn dynamic_class_attribute_set(instance: i64, obj: i64, new_value: i64) -> i64 {
    let fset = dynamic_attribute_slot(instance, "fset");
    if fset == NONE {
        return exc::raise_with_message(ATTRIBUTE_ERROR, "can't set attribute");
    }
    let args = [obj, new_value];
    let roots = heap::root_values(&[instance, fset]);
    let result = crate::core::builtins::call(fset, args.len(), args.as_ptr());
    heap::release_roots(roots);
    result
}

/// `DynamicClassAttribute.__delete__(instance)`.
fn dynamic_class_attribute_delete(instance: i64, obj: i64) -> i64 {
    let fdel = dynamic_attribute_slot(instance, "fdel");
    if fdel == NONE {
        return exc::raise_with_message(ATTRIBUTE_ERROR, "can't delete attribute");
    }
    let args = [obj];
    let roots = heap::root_values(&[instance, fdel]);
    let result = crate::core::builtins::call(fdel, args.len(), args.as_ptr());
    heap::release_roots(roots);
    result
}

/// `getter`/`setter`/`deleter`: returns an updated copy of the same (possibly
/// user-derived) descriptor class.
fn dynamic_class_attribute_copy(instance: i64, which: usize, function: i64) -> i64 {
    let mut fget = dynamic_attribute_slot(instance, "fget");
    let mut fset = dynamic_attribute_slot(instance, "fset");
    let mut fdel = dynamic_attribute_slot(instance, "fdel");
    let doc = dynamic_attribute_slot(instance, "__doc__");
    let overwrite_doc = value::truthy(dynamic_attribute_slot(instance, "overwrite_doc"));
    match which {
        0 => fget = function,
        1 => fset = function,
        _ => fdel = function,
    }
    // CPython: `getter` keeps the previous docstring when the new getter has
    // none (only when the descriptor owns its docstring); `setter`/`deleter`
    // carry the current docstring over verbatim.
    let effective_doc = if which == 0 && overwrite_doc {
        let fdoc = accessor_doc(function);
        if fdoc != NONE {
            fdoc
        } else {
            doc
        }
    } else {
        doc
    };
    let class = value::instance_class_value(instance).unwrap_or_else(dynamic_class_attribute_class);
    let roots = heap::root_values(&[class, fget, fset, fdel, effective_doc]);
    let copy = value::instance_new(class);
    let copy_roots = heap::root_values(&[copy]);
    let result = initialize_dynamic_class_attribute(copy, fget, fset, fdel, effective_doc);
    value::instance_setattr(
        copy,
        "overwrite_doc".to_string(),
        value::bool_value(overwrite_doc),
    );
    heap::release_roots(copy_roots);
    heap::release_roots(roots);
    result
}

/// Dispatches the native methods of `DynamicClassAttribute`.
pub(crate) fn dynamic_class_attribute_method(function: &str, args: &[i64]) -> Option<i64> {
    let method = function.strip_prefix("_dca_")?;
    Some(match method {
        "init" => match args {
            [instance, fget, fset, fdel, doc] => {
                initialize_dynamic_class_attribute(*instance, *fget, *fset, *fdel, *doc)
            }
            _ => exc::raise_with_message(TYPE_ERROR, "_dca_init() takes 5 arguments"),
        },
        "get" => match args {
            [instance, obj, owner] => dynamic_class_attribute_get(*instance, *obj, *owner),
            _ => exc::raise_with_message(TYPE_ERROR, "__get__() takes 3 arguments"),
        },
        "set" => match args {
            [instance, obj, new_value] => dynamic_class_attribute_set(*instance, *obj, *new_value),
            _ => exc::raise_with_message(TYPE_ERROR, "__set__() takes 3 arguments"),
        },
        "delete" => match args {
            [instance, obj] => dynamic_class_attribute_delete(*instance, *obj),
            _ => exc::raise_with_message(TYPE_ERROR, "__delete__() takes 2 arguments"),
        },
        "getter" => match args {
            [instance, function] => dynamic_class_attribute_copy(*instance, 0, *function),
            _ => exc::raise_with_message(TYPE_ERROR, "getter() takes 2 arguments"),
        },
        "setter" => match args {
            [instance, function] => dynamic_class_attribute_copy(*instance, 1, *function),
            _ => exc::raise_with_message(TYPE_ERROR, "setter() takes 2 arguments"),
        },
        "deleter" => match args {
            [instance, function] => dynamic_class_attribute_copy(*instance, 2, *function),
            _ => exc::raise_with_message(TYPE_ERROR, "deleter() takes 2 arguments"),
        },
        _ => return None,
    })
}

/// `isinstance` membership for the `types` names that are exposed as module
/// functions (so the module value is the native function, not the class).
pub(crate) fn type_check_target(value_arg: i64, name: &str) -> Option<bool> {
    match name {
        "FunctionType" => Some(value::as_function(value_arg).is_some()),
        "UnionType" => Some(is_union(value_arg)),
        _ => None,
    }
}

/// The cached `function` class.
///
/// Every function value reports this class from `type()`, so
/// `types.FunctionType` must be the same heap value rather than a new class.
///
/// The registry entry is validated on every hit: a class from a previous heap
/// generation (or one whose cell no longer holds a class, for example after a
/// `reset`) is rebuilt instead of being handed out. The returned class is
/// additionally rooted on the calling thread, so the identity cache never
/// depends on a scoped root that an in-flight `release_roots` marker could
/// truncate.
pub(crate) fn function_type_class() -> i64 {
    if let Some(class) = globals::registry_get("types.function") {
        if heap::is_registered_class_named(class, "function") {
            heap::add_thread_root(class);
            return class;
        }
    }
    // `type()` of a function value resolves the runtime's cached `function`
    // class; a throwaway probe avoids exposing a private `value` helper.
    let probe = value::native_function_new("types", "_function_probe", 0);
    let roots = heap::root_values(&[probe]);
    let class = value::type_class(probe);
    heap::release_roots(roots);
    // Root before publishing: a collection on another thread must never see
    // the registry entry before its value is marked reachable.
    heap::add_thread_root(class);
    globals::registry_set("types.function", class);
    class
}

/// `types.FunctionType(code, globals, name=None, argdefs=None, closure=None,
/// kwdefaults=None)`.
///
/// Hydro functions already carry their compiled globals; the constructor can
/// therefore only reproduce a function from another function value. The
/// result is a new delegating callable (a `functools.partial` with no bound
/// arguments). Rebinding `argdefs`/`closure`/`kwdefaults` or accepting a code
/// object requires a reified code type the runtime does not have, so those
/// forms raise a tagged `NotImplementedError` instead of silently ignoring the
/// request.
fn function_type_new(args: &[i64]) -> i64 {
    let code = args.first().copied().unwrap_or(NONE);
    let globals_namespace = args.get(1).copied().unwrap_or(NONE);
    let name = args.get(2).copied().unwrap_or(NONE);
    let argdefs = args.get(3).copied().unwrap_or(NONE);
    let closure = args.get(4).copied().unwrap_or(NONE);
    let kwdefaults = args.get(5).copied().unwrap_or(NONE);
    function_type_from(code, globals_namespace, name, argdefs, closure, kwdefaults)
}

fn function_type_from(
    code: i64,
    globals_namespace: i64,
    name: i64,
    argdefs: i64,
    closure: i64,
    kwdefaults: i64,
) -> i64 {
    if globals_namespace != NONE
        && value::as_dict(globals_namespace).is_none()
        && value::as_instance(globals_namespace).is_none()
    {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "FunctionType() globals must be a dict, not {}",
                value::type_name(globals_namespace)
            ),
        );
    }
    if name != NONE && value::as_str(name).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "FunctionType() name must be a str, not {}",
                value::type_name(name)
            ),
        );
    }
    if value::as_function(code).is_none() {
        return exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "types.FunctionType() requires a Hydro function value as 'code': \
             code objects are not reified by the runtime",
        );
    }
    if argdefs != NONE || closure != NONE || kwdefaults != NONE {
        return exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "types.FunctionType() cannot rebind argdefs/closure/kwdefaults: \
             Hydro functions close over their globals at compile time",
        );
    }
    // A new callable forwarding every argument to `code`, matching CPython's
    // distinct-object result without pretending a different globals dict was
    // installed.
    value::partial_new(code, Vec::new())
}

/// The cached `types.UnionType` class of native union values.
pub(crate) fn union_type_class() -> i64 {
    if let Some(class) = globals::registry_get("types.UnionType") {
        return class;
    }
    let class = value::class_new(
        "UnionType".to_string(),
        NONE,
        vec![
            (
                "__repr__".to_string(),
                union_native_method(union_repr_method, 1, "__repr__"),
            ),
            (
                "__or__".to_string(),
                union_native_method(union_or_method, 2, "__or__"),
            ),
            (
                "__ror__".to_string(),
                union_native_method(union_ror_method, 2, "__ror__"),
            ),
        ],
    );
    globals::registry_set("types.UnionType", class);
    class
}

fn union_native_method(entry: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
    value::function_new(entry as *const () as usize, arity, name.to_string())
}

/// Whether `v` is a native union value.
pub(crate) fn is_union(v: i64) -> bool {
    // Only instances can be unions; this keeps the registry lookup (which
    // allocates a key) off the scalar fast path of `A | B`.
    let Some(class) = value::instance_class_value(v) else {
        return false;
    };
    value::class_display_name(class) == Some("UnionType")
        && globals::registry_get("types.UnionType") == Some(class)
}

/// Values accepted as union members: classes, `None` (standing for
/// `NoneType`), existing unions, function values and PEP 695 type aliases.
/// Function values act as types in this runtime (`isinstance(x, f)` consults
/// the builtin/native name, and `types.FunctionType` is itself a module
/// function), so both operand orders of `A | B` used by annotation aliases
/// work. `typing.TypeAliasType` instances join so an alias defined with
/// `type X = ...` composes into a union exactly like the class it names.
fn is_union_member(v: i64) -> bool {
    value::as_class(v).is_some()
        || v == NONE
        || value::as_function(v).is_some()
        || is_union(v)
        || crate::modules::typing::is_type_alias_value(v)
}

/// Appends `value_arg` to `out`, flattening tuple input and the `__args__` of
/// an existing union like CPython's `types.UnionType`.
fn flatten_union_member(value_arg: i64, out: &mut Vec<i64>) {
    if let Some(items) = value::as_tuple(value_arg) {
        for item in items {
            flatten_union_member(*item, out);
        }
        return;
    }
    if is_union(value_arg) {
        if let Some(args) = value::instance_attr(value_arg, "__args__").and_then(value::as_tuple) {
            for item in args {
                flatten_union_member(*item, out);
            }
            return;
        }
    }
    out.push(value_arg);
}

/// Builds `A | B | ...`, flattening tuple/union input and collapsing
/// duplicates and one-element results like CPython's `types.UnionType`.
///
/// Subscription accepts arbitrary members (CPython's `types.UnionType[1, 2]`
/// builds a union of the two ints), so this does not validate them; the
/// `A | B` operator path checks the operand kinds in [`union_binary_or`].
pub(crate) fn union_new(values: &[i64]) -> i64 {
    let mut members: Vec<i64> = Vec::with_capacity(values.len());
    for value_arg in values {
        flatten_union_member(*value_arg, &mut members);
    }
    let mut unique: Vec<i64> = Vec::with_capacity(members.len());
    for member in members {
        if !unique.contains(&member) {
            unique.push(member);
        }
    }
    match unique.len() {
        0 => exc::raise_with_message(TYPE_ERROR, "types.UnionType needs at least one member"),
        1 => unique[0],
        _ => {
            let roots = heap::root_values(&unique);
            let class = union_type_class();
            let instance = value::instance_new(class);
            let args_tuple = value::tuple_new(unique.clone());
            let name = value::str_alloc("Union".to_string());
            let qualname = value::str_alloc("Union".to_string());
            let module = value::str_alloc("types".to_string());
            let tail = heap::root_values(&[instance, args_tuple, name, qualname, module]);
            value::instance_setattr(instance, "__args__".to_string(), args_tuple);
            value::instance_setattr(instance, "__name__".to_string(), name);
            value::instance_setattr(instance, "__qualname__".to_string(), qualname);
            value::instance_setattr(instance, "__module__".to_string(), module);
            heap::release_roots(tail);
            heap::release_roots(roots);
            instance
        }
    }
}

/// `types.UnionType[index]` for the class-subscript path. Returns `None` when
/// `class` is not the union class so the caller keeps generic-alias behaviour.
pub(crate) fn union_subscript(class: i64, index: i64) -> Option<i64> {
    // Cheap name gate: class subscripts are common (`list[int]`,
    // `typing.Tuple[...]`), and the registry lookup allocates a key.
    if value::class_display_name(class) != Some("UnionType") {
        return None;
    }
    if globals::registry_get("types.UnionType") != Some(class) {
        return None;
    }
    let values: Vec<i64> = match value::as_tuple(index) {
        Some(items) => items.to_vec(),
        None => vec![index],
    };
    Some(union_new(&values))
}

/// `A | B` for supported type values; `None` means "not a type union" and the
/// native bitwise path reports its usual `TypeError`.
pub(crate) fn union_binary_or(left: i64, right: i64) -> Option<i64> {
    if !is_union_member(left) || !is_union_member(right) {
        return None;
    }
    let roots = heap::root_values(&[left, right]);
    let result = union_new(&[left, right]);
    heap::release_roots(roots);
    Some(result)
}

fn union_member_text(value_arg: i64) -> String {
    if let Some(name) = value::class_display_name(value_arg) {
        return name.to_string();
    }
    if is_union(value_arg) {
        return union_repr_text(value_arg);
    }
    if value_arg == NONE {
        return "None".to_string();
    }
    if let Some(function) = value::as_function(value_arg) {
        return function.name.clone();
    }
    value::repr(value_arg)
}

fn union_repr_text(union_value: i64) -> String {
    let Some(args) = value::instance_attr(union_value, "__args__") else {
        return "None".to_string();
    };
    let Some(items) = value::as_tuple(args) else {
        return "None".to_string();
    };
    let roots = heap::root_values(&[args]);
    let rendered: Vec<String> = items.iter().map(|item| union_member_text(*item)).collect();
    heap::release_roots(roots);
    rendered.join(" | ")
}

extern "C" fn union_repr_method(argv: *const i64) -> i64 {
    // SAFETY: bound method call passes `self` first.
    let this = unsafe { *argv };
    value::str_alloc(union_repr_text(this))
}

extern "C" fn union_or_method(argv: *const i64) -> i64 {
    // SAFETY: `__or__` receives `self` and the other operand.
    let left = unsafe { *argv };
    let right = unsafe { *argv.add(1) };
    match union_binary_or(left, right) {
        Some(result) => result,
        None => exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "unsupported operand type(s) for |: '{}' and '{}'",
                value::type_name_of(left),
                value::type_name_of(right)
            ),
        ),
    }
}

extern "C" fn union_ror_method(argv: *const i64) -> i64 {
    // SAFETY: `__ror__` receives `self` and the left operand.
    let this = unsafe { *argv };
    let other = unsafe { *argv.add(1) };
    match union_binary_or(other, this) {
        Some(result) => result,
        None => exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "unsupported operand type(s) for |: '{}' and '{}'",
                value::type_name_of(other),
                value::type_name_of(this)
            ),
        ),
    }
}

/// Identity-stable `types.SimpleNamespace` class.
///
/// `pprint` keys its dispatch table by `_types.SimpleNamespace.__repr__`, so
/// the module value must be a class carrying a `__repr__` member (a module
/// function has no `__repr__` attribute at all). The member is stored as a
/// class attribute rather than a method entry, so every read returns the same
/// function object, which is what a dispatch-table key needs. Its body is the
/// builtin `repr`, so the class-level entry renders a namespace payload the
/// same way `repr(ns)` does.
///
/// Construction stays on the module-call path ([`simple_namespace`]), which
/// returns the native namespace object `argparse` and `pprint` read
/// attributes through. The class exists for the type surface `pprint`,
/// `isinstance` and dispatch tables need.
pub(crate) fn simple_namespace_class() -> i64 {
    const KEY: &str = "types.SimpleNamespace.class";
    if let Some(cached) = globals::registry_get(KEY) {
        return cached;
    }
    let repr = value::native_function_new("builtins", "repr", 1);
    let roots = heap::root_values(std::slice::from_ref(&repr));
    let class = value::class_new_full(
        "SimpleNamespace".to_string(),
        NONE,
        Vec::new(),
        vec![("__repr__".to_string(), repr)],
    );
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(KEY, class);
    heap::release_roots(class_roots);
    heap::release_roots(roots);
    class
}

/// `SimpleNamespace(mapping=None)`: copies the mapping into a namespace.
fn simple_namespace(mapping: i64) -> i64 {
    if mapping == NONE {
        return value::namespace_new(Vec::new());
    }
    let Some(pairs) = value::as_dict(mapping) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "SimpleNamespace() argument must be a mapping, not {}",
                value::type_name(mapping)
            ),
        );
    };
    // The snapshot keeps the borrow independent of the allocations performed
    // while decoding the attribute names; the values themselves stay reachable
    // through the mapping argument.
    let snapshot = pairs.to_vec();
    let mut attributes = Vec::with_capacity(snapshot.len());
    for (key, item) in snapshot {
        let Some(name) = value::as_str(key) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "SimpleNamespace() attribute names must be str, not {}",
                    value::type_name(key)
                ),
            );
        };
        attributes.push((name.to_string(), item));
    }
    value::namespace_new(attributes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{as_class, as_namespace, encode_int};

    fn text(s: &str) -> i64 {
        value::str_alloc(s.to_string())
    }

    #[test]
    fn placeholder_names_are_native_classes() {
        exc::init();
        for name in CLASSES {
            if matches!(*name, "NoneType" | "EllipsisType" | "NotImplementedType") {
                continue;
            }
            let class = as_class(constant(name)).expect("class value");
            assert_eq!(class.name, *name);
            assert_eq!(class.base, NONE);
            assert!(class.methods.is_empty());
        }
        assert!(!exc::pending());
    }

    /// Regression for the GC-rooting bug: a scoped `root_values` marker taken
    /// before the shared function class is created must not be able to release
    /// it, and the registry cache must validate/re-root on every hit.
    ///
    /// This mirrors the pydantic_core sequence (`import pydantic_core; import
    /// gc; gc.collect(); type(lambda: 0)`), where a container builder's release
    /// marker used to truncate the class root and let the collector recycle the
    /// cell, so `types.FunctionType` became a string or a function.
    #[test]
    fn function_type_class_survives_release_and_collection() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        crate::heap::reset();
        let probe = value::native_function_new("types", "_function_type_probe", 0);
        // The marker exists before the class is rooted.
        let outer = crate::heap::root_values(&[probe]);
        let class = function_type_class();
        assert_eq!(value::type_class(probe), class);
        crate::heap::release_roots(outer);
        let _ = crate::heap::collect();
        assert!(crate::heap::is_registered_class_value(class));
        assert_eq!(function_type_class(), class);
        assert_eq!(
            as_class(class).map(|state| state.name.as_str()),
            Some("function")
        );
        // A function created after the collection still reports the class.
        let function = value::native_function_new("types", "_function_after_gc", 0);
        let function_root = crate::heap::root_values(&[function]);
        assert_eq!(value::type_class(function), class);
        crate::heap::release_roots(function_root);
        assert_eq!(constant("FunctionType"), class);
        // Repeated hits re-root idempotently: no permanent-root growth.
        let count = crate::heap::permanent_root_count();
        assert_eq!(function_type_class(), class);
        assert_eq!(crate::heap::permanent_root_count(), count);
    }

    /// The `types.function` registry entry belongs to the heap generation that
    /// created it; a new generation (reset) must rebuild it instead of handing
    /// back a tagged value whose cell the freed heap reused.
    #[test]
    fn function_type_class_rebuilds_for_a_new_generation() {
        let _guard = crate::heap::heap_reset_test_guard();
        exc::init();
        crate::heap::reset();
        let first = function_type_class();
        assert!(crate::heap::is_registered_class_value(first));
        // Skip `exc::init` so the registry entry survives into the new heap.
        crate::heap::reset();
        let second = function_type_class();
        assert!(crate::heap::is_registered_class_value(second));
        assert_eq!(
            as_class(second).map(|state| state.name.as_str()),
            Some("function")
        );
        assert_eq!(function_type_class(), second);
    }

    #[test]
    fn simple_namespace_without_attributes_is_empty() {
        exc::init();
        let namespace = call("SimpleNamespace", &[]);
        assert_eq!(as_namespace(namespace).map(|pairs| pairs.len()), Some(0));
        assert!(!exc::pending());
    }

    #[test]
    fn simple_namespace_copies_mapping_attributes() {
        exc::init();
        let mapping = value::dict_new(vec![
            (text("name"), text("rich")),
            (text("width"), encode_int(80).unwrap()),
        ]);
        let namespace = call("SimpleNamespace", &[mapping]);
        assert!(!exc::pending());
        let pairs = as_namespace(namespace).expect("namespace");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "name");
        assert_eq!(value::as_str(pairs[0].1), Some("rich"));
        assert_eq!(pairs[1].0, "width");
        assert_eq!(value::as_integer(pairs[1].1), Some(80));
    }

    #[test]
    fn simple_namespace_is_a_class_with_a_pprint_repr_dispatch_key() {
        exc::init();
        let class = simple_namespace_class();
        assert_eq!(class, constant("SimpleNamespace"));
        assert_eq!(
            class,
            class_value("types", "SimpleNamespace").expect("class value")
        );
        // `pprint` reads the name off its `_types` alias; both spellings must
        // hand out the identical class.
        assert_eq!(
            class,
            crate::modules::module_get_by_name("_types", "SimpleNamespace")
        );
        assert!(!exc::pending(), "_types.SimpleNamespace raised");
        assert!(as_class(class).is_some(), "module value must be a class");
        // `pprint` needs `SimpleNamespace.__repr__` to exist and be callable
        // while the module body runs; the dispatch table also requires a
        // stable key, so repeated class reads must be one function object.
        assert!(
            matches!(
                value::class_lookup(class, "__repr__"),
                Some(value::ClassEntry::Attr(_))
            ),
            "SimpleNamespace.__repr__ must be a class attribute"
        );
        let attr_name = "__repr__";
        let key = methods::getattr(class, attr_name.as_ptr(), attr_name.len());
        assert!(!exc::pending(), "SimpleNamespace.__repr__ raised");
        assert!(
            value::as_function(key).is_some(),
            "SimpleNamespace.__repr__ must be callable"
        );
        assert_eq!(
            key,
            methods::getattr(class, attr_name.as_ptr(), attr_name.len()),
            "repeated __repr__ reads must share identity"
        );
        let dispatch = value::dict_new(Vec::new());
        let handler = value::native_function_new("pprint", "_pprint_simplenamespace", 0);
        crate::container::dict_set(dispatch, key, handler);
        assert!(!exc::pending(), "dispatch insert raised");
        assert!(
            crate::container::dict_find(value::as_dict(dispatch).expect("dispatch dict"), key)
                .is_some(),
            "dispatch lost the class repr key"
        );
        // Construction keeps returning the namespace payload consumers read.
        let namespace = call("SimpleNamespace", &[]);
        assert!(!exc::pending());
        assert_eq!(as_namespace(namespace).map(|pairs| pairs.len()), Some(0));
    }

    #[test]
    fn simple_namespace_rejects_non_mappings() {
        exc::init();
        call("SimpleNamespace", &[text("not a mapping")]);
        assert!(exc::pending());
        exc::clear();
    }

    #[test]
    fn unknown_names_raise_attribute_error() {
        exc::init();
        constant("Missing");
        assert!(exc::pending());
        exc::clear();
        call("missing", &[]);
        assert!(exc::pending());
        exc::clear();
    }

    #[test]
    fn cell_type_is_a_cached_class_with_contents() {
        exc::init();
        let class = cell_type_class();
        assert_eq!(class, constant("CellType"));
        assert_eq!(
            as_class(class).map(|class| class.name.as_str()),
            Some("CellType")
        );

        let value = encode_int(42).unwrap();
        let cell = call("CellType", &[value]);
        assert!(!exc::pending());
        assert_eq!(value::instance_class_value(cell), Some(class));
        let read = methods::getattr(cell, b"cell_contents".as_ptr(), "cell_contents".len());
        assert_eq!(value::as_integer(read), Some(42));
    }

    extern "C" fn get_value(argv: *const i64) -> i64 {
        // SAFETY: `__get__` passes the instance first.
        let instance = unsafe { *argv };
        value::instance_attr(instance, "value").unwrap_or(NONE)
    }

    extern "C" fn set_value(argv: *const i64) -> i64 {
        // SAFETY: `__set__` passes `(instance, value)`.
        let instance = unsafe { *argv };
        let new_value = unsafe { *argv.add(1) };
        value::instance_setattr(instance, "value".to_string(), new_value);
        NONE
    }

    extern "C" fn delete_value(_argv: *const i64) -> i64 {
        encode_int(777).unwrap()
    }

    fn wrapper(entry: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
        value::function_new(entry as *const () as usize, arity, name.to_string())
    }

    fn plain_instance() -> i64 {
        let class = value::class_new("Holder".to_string(), NONE, Vec::new());
        value::instance_new(class)
    }

    #[test]
    fn dynamic_class_attribute_is_cached_and_validates() {
        exc::init();
        let class = dynamic_class_attribute_class();
        assert_eq!(class, constant("DynamicClassAttribute"));
        assert_eq!(
            class,
            class_value("types", "DynamicClassAttribute").expect("class")
        );
        assert!(class_value("other", "DynamicClassAttribute").is_none());

        let fget = wrapper(get_value, 1, "get_value");
        let descriptor = call("DynamicClassAttribute", &[fget]);
        assert!(!exc::pending());
        assert_eq!(value::instance_attr(descriptor, "fget"), Some(fget));
        assert_eq!(value::instance_attr(descriptor, "fset"), Some(NONE));
        assert_eq!(
            value::instance_attr(descriptor, "overwrite_doc"),
            Some(value::TRUE)
        );

        // Keyword construction through the keyword-aware entry point.
        exc::init();
        let from_kw = call_kw("DynamicClassAttribute", &[], &[("fget".to_string(), fget)]);
        assert_eq!(value::instance_attr(from_kw, "fget"), Some(fget));
        assert!(!exc::pending());
        call_kw("DynamicClassAttribute", &[], &[("bogus".to_string(), NONE)]);
        assert!(exc::pending());
        exc::clear();
    }

    #[test]
    fn dynamic_class_attribute_dispatches_accessors() {
        exc::init();
        let fget = wrapper(get_value, 1, "get_value");
        let fset = wrapper(set_value, 2, "set_value");
        let fdel = wrapper(delete_value, 1, "delete_value");
        let descriptor = call("DynamicClassAttribute", &[fget, fset, fdel]);
        let instance = plain_instance();
        value::instance_setattr(instance, "value".to_string(), encode_int(5).unwrap());

        let read = call("_dca_get", &[descriptor, instance, NONE]);
        assert_eq!(value::as_integer(read), Some(5));

        // `__set__` forwards to fset.
        call("_dca_set", &[descriptor, instance, encode_int(9).unwrap()]);
        assert_eq!(
            value::instance_attr(instance, "value").and_then(value::as_integer),
            Some(9)
        );

        // `__delete__` forwards to fdel and returns its result.
        let deleted = call("_dca_delete", &[descriptor, instance]);
        assert_eq!(value::as_integer(deleted), Some(777));

        // Class access raises AttributeError, like CPython.
        exc::init();
        call("_dca_get", &[descriptor, NONE, NONE]);
        assert!(exc::pending());
        assert_eq!(exc::pending_tag(), crate::exc::ATTRIBUTE_ERROR as i64);
        exc::clear();

        // An abstract getter makes class access return the descriptor.
        let is_abstract = call("DynamicClassAttribute", &[fget]);
        value::instance_setattr(is_abstract, "__isabstractmethod__".to_string(), value::TRUE);
        assert_eq!(call("_dca_get", &[is_abstract, NONE, NONE]), is_abstract);
        assert!(!exc::pending());
    }

    #[test]
    fn dynamic_class_attribute_missing_accessors_raise() {
        exc::init();
        let descriptor = call("DynamicClassAttribute", &[]);
        let instance = plain_instance();
        exc::init();
        call("_dca_get", &[descriptor, instance, NONE]);
        assert!(exc::pending());
        assert_eq!(
            value::to_string(exc::pending_value()),
            "unreadable attribute"
        );
        exc::init();
        call("_dca_set", &[descriptor, instance, NONE]);
        assert!(exc::pending());
        assert_eq!(
            value::to_string(exc::pending_value()),
            "can't set attribute"
        );
        exc::init();
        call("_dca_delete", &[descriptor, instance]);
        assert!(exc::pending());
        assert_eq!(
            value::to_string(exc::pending_value()),
            "can't delete attribute"
        );
        exc::clear();
    }

    #[test]
    fn dynamic_class_attribute_copies_keep_the_class_and_accessors() {
        exc::init();
        let fget = wrapper(get_value, 1, "get_value");
        let fset = wrapper(set_value, 2, "set_value");
        let descriptor = call("DynamicClassAttribute", &[fget, fset]);
        let new_getter = wrapper(get_value, 1, "new_getter");
        let copy = call("_dca_getter", &[descriptor, new_getter]);
        assert_eq!(
            value::instance_class_value(copy),
            Some(dynamic_class_attribute_class())
        );
        assert_eq!(value::instance_attr(copy, "fget"), Some(new_getter));
        assert_eq!(value::instance_attr(copy, "fset"), Some(fset));

        let new_setter = wrapper(set_value, 2, "new_setter");
        let copy = call("_dca_setter", &[descriptor, new_setter]);
        assert_eq!(value::instance_attr(copy, "fget"), Some(fget));
        assert_eq!(value::instance_attr(copy, "fset"), Some(new_setter));

        let new_deleter = wrapper(delete_value, 1, "new_deleter");
        let copy = call("_dca_deleter", &[descriptor, new_deleter]);
        assert_eq!(value::instance_attr(copy, "fdel"), Some(new_deleter));
        assert!(!exc::pending());
    }

    #[test]
    fn get_original_bases_reports_declared_and_recorded_bases() {
        exc::init();
        let bare = value::class_new("Bare".to_string(), NONE, Vec::new());
        assert_eq!(
            value::as_tuple(call("get_original_bases", &[bare])).map(|items| items.to_vec()),
            Some(vec![value::object_class()])
        );

        let base = value::class_new("Base".to_string(), NONE, Vec::new());
        let derived = value::class_new("Derived".to_string(), base, Vec::new());
        assert_eq!(
            value::as_tuple(call("get_original_bases", &[derived])).map(|items| items.to_vec()),
            Some(vec![base])
        );

        // `__orig_bases__` in the class namespace is authoritative.
        let original = value::tuple_new(vec![base]);
        let recorded = value::class_new_full(
            "Recorded".to_string(),
            NONE,
            Vec::new(),
            vec![("__orig_bases__".to_string(), original)],
        );
        assert_eq!(call("get_original_bases", &[recorded]), original);
        assert!(!exc::pending());

        exc::init();
        let _ = call("get_original_bases", &[NONE]);
        assert_eq!(exc::pending_tag(), crate::exc::TYPE_ERROR as i64);
        assert!(value::to_string(exc::pending_value())
            .contains("Expected an instance of type, not 'NoneType'"));
        exc::clear();
    }
}
