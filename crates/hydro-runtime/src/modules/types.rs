//! `types`: native placeholder classes plus `SimpleNamespace`.
//!
//! Rich imports `FrameType`, `ModuleType`, `TracebackType`, `MappingProxyType`
//! and friends almost exclusively for annotations, `isinstance` checks and
//! dictionary keys. Each name therefore materialises as an empty native class
//! whose identity is fresh on every access, like the `typing` placeholders.
//! `CodeType` and `NoneType` follow the same pattern; no introspection API is
//! attached to them.
//!
//! `CellType`, `FunctionType` and `UnionType` are cached:
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
//!
//! `SimpleNamespace(...)` accepts one optional mapping and returns the native
//! namespace object used by `argparse`, so attribute reads work immediately.
//! CPython spells the attributes as keyword arguments; the frontend has no
//! `**kwargs` lowering for native module functions yet, so the runtime takes
//! the equivalent mapping instead of rejecting the constructor outright.

use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, NOT_IMPLEMENTED_ERROR, TYPE_ERROR};
use crate::heap;
use crate::methods;
use crate::value::{self, NONE};

/// Names exposed as empty native classes.
const CLASSES: &[&str] = &[
    "AsyncGeneratorType",
    "BuiltinFunctionType",
    "BuiltinMethodType",
    "ClassMethodDescriptorType",
    "CodeType",
    "CoroutineType",
    "EllipsisType",
    "FrameType",
    "GeneratorType",
    "GenericAlias",
    "GetSetDescriptorType",
    "LambdaType",
    "MappingProxyType",
    "MemberDescriptorType",
    "MethodDescriptorType",
    "MethodType",
    "MethodWrapperType",
    "ModuleType",
    "NoneType",
    "TracebackType",
    "WrapperDescriptorType",
];

/// Materialises the module constant `name`.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "CellType" => cell_type_class(),
        // Exposed as a module function too, so that `types.FunctionType(...)`
        // lowers as a call; the class form still answers direct lookups and
        // the `isinstance` route through `type_check_target`.
        "FunctionType" => function_type_class(),
        "UnionType" => union_type_class(),
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
    match function {
        "SimpleNamespace" => simple_namespace(args.first().copied().unwrap_or(NONE)),
        "CellType" => cell_type_new(args),
        // `types.FunctionType(code, globals, ...)`.
        "FunctionType" => function_type_new(args),
        // `types.UnionType[a, b, ...]` through the module-call path (the
        // subscript spelling goes through [`union_subscript`]).
        "UnionType" => union_new(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'types' has no attribute '{function}'"),
        ),
    }
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
pub(crate) fn function_type_class() -> i64 {
    if let Some(class) = globals::registry_get("types.function") {
        return class;
    }
    // `type()` of a function value resolves the runtime's cached `function`
    // class; a throwaway probe avoids exposing a private `value` helper.
    let probe = value::native_function_new("types", "_function_probe", 0);
    let roots = heap::root_values(&[probe]);
    let class = value::type_class(probe);
    heap::release_roots(roots);
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
/// `NoneType`), existing unions and function values. Function values act as
/// types in this runtime (`isinstance(x, f)` consults the builtin/native name,
/// and `types.FunctionType` is itself a module function), so both operand
/// orders of `A | B` used by annotation aliases work.
fn is_union_member(v: i64) -> bool {
    value::as_class(v).is_some() || v == NONE || value::as_function(v).is_some() || is_union(v)
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
            let tail = heap::root_values(&[instance, args_tuple]);
            value::instance_setattr(instance, "__args__".to_string(), args_tuple);
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
            let class = as_class(constant(name)).expect("class value");
            assert_eq!(class.name, *name);
            assert_eq!(class.base, NONE);
            assert!(class.methods.is_empty());
        }
        assert!(!exc::pending());
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
}
