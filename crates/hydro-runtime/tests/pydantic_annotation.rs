//! Integration tests for the `types`/`ast` surface `annotationlib` and
//! pydantic 2.x rely on:
//!
//! * `types.UnionType` subscription/`|` construction and `isinstance`;
//! * `types.FunctionType` identity and construction from a Hydro function;
//! * the `complex`/`slice` builtin class values and their values;
//! * `ast.TemplateStr`/`ast.Interpolation` construction and `unparse`.
//!
//! Every test drives the public runtime ABI like generated code does.

use std::sync::{LazyLock, Mutex, MutexGuard};

use hydro_runtime::abi::*;

const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;
const ATTRIBUTE_ERROR: i64 = 9;
const NOT_IMPLEMENTED_ERROR: i64 = 32;

const OP_EQ: u32 = 7;
const OP_IN: u32 = 13;
const OP_BIT_OR: u32 = 18;

static SERIAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn setup() -> MutexGuard<'static, ()> {
    let guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    hydro_runtime_init();
    guard
}

fn int(n: i64) -> i64 {
    n << 3
}

fn text(value_in: &str) -> i64 {
    hydro_str_new(value_in.as_ptr(), value_in.len())
}

fn tuple(items: &[i64]) -> i64 {
    hydro_tuple_new(items.len(), items.as_ptr())
}

fn list(items: &[i64]) -> i64 {
    hydro_list_new(items.len(), items.as_ptr())
}

fn empty_dict() -> i64 {
    let keys: [i64; 0] = [];
    hydro_dict_new(0, keys.as_ptr(), keys.as_ptr())
}

fn module(module_name: &str, function: &str, args: &[i64]) -> i64 {
    hydro_module_call(
        module_name.as_ptr(),
        module_name.len(),
        function.as_ptr(),
        function.len(),
        args.len(),
        args.as_ptr(),
    )
}

fn module_kw(module_name: &str, function: &str, args: &[i64], keywords: &[(&str, i64)]) -> i64 {
    let mut pairs = Vec::with_capacity(keywords.len() * 2);
    for (key, value_in) in keywords {
        pairs.push(text(key));
        pairs.push(*value_in);
    }
    hydro_module_call_kw(
        module_name.as_ptr(),
        module_name.len(),
        function.as_ptr(),
        function.len(),
        args.len(),
        args.as_ptr(),
        keywords.len(),
        pairs.as_ptr(),
    )
}

fn module_get(module_name: &str, name: &str) -> i64 {
    hydro_module_get(
        module_name.as_ptr(),
        module_name.len(),
        name.as_ptr(),
        name.len(),
    )
}

fn method(object: i64, name: &str, args: &[i64]) -> i64 {
    hydro_method(object, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

fn getattr(object: i64, name: &str) -> i64 {
    hydro_getattr(object, name.as_ptr(), name.len())
}

fn call(callable: i64, args: &[i64]) -> i64 {
    hydro_call(callable, args.len(), args.as_ptr())
}

fn builtin(name: &str) -> i64 {
    hydro_builtin_function(name.as_ptr(), name.len())
}

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn assert_true(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_true(), "expected True");
}

/// Decodes a tagged integer to its raw value. `hydro_to_int` returns a tagged
/// integer (the runtime's value encoding), so the `>> 3` shift is required
/// before comparing against a Rust integer.
fn unbox(actual: i64) -> i64 {
    assert_no_error();
    hydro_to_int(actual) >> 3
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(hydro_to_int(actual) >> 3, expected);
}

fn assert_repr(actual: i64, expected: &str) {
    assert_no_error();
    let rendered = hydro_to_repr(actual);
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_EQ, rendered, text(expected)),
        hydro_true(),
        "unexpected repr, expected {expected:?}"
    );
}

fn assert_none(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_none(), "expected None");
}

fn expect_error(tag: i64) {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    assert_eq!(hydro_exc_tag(), tag, "wrong exception tag");
    hydro_exc_clear();
}

// -------------------------------------------------------------------------
// types.UnionType
// -------------------------------------------------------------------------

#[test]
fn union_type_subscription_and_bitor_build_union_values() {
    let _guard = setup();

    let union_class = module_get("types", "UnionType");
    assert_no_error();
    let complex_class = module_get("builtins", "complex");
    let slice_class = module_get("builtins", "slice");
    assert_no_error();

    // `types.UnionType[A, B]`.
    let union = hydro_subscript(union_class, tuple(&[complex_class, slice_class]));
    assert_no_error();
    assert_eq!(unbox(hydro_len(getattr(union, "__args__"))), 2);
    assert_repr(union, "complex | slice");

    // `A | B` (the PEP 604 spelling pydantic's type aliases use).
    let bitor = hydro_binop(OP_BIT_OR, complex_class, slice_class);
    assert_repr(bitor, "complex | slice");

    // `A | None` and union flattening through an existing union operand.
    let optional = hydro_binop(OP_BIT_OR, complex_class, hydro_none());
    assert_repr(optional, "complex | None");
    let flattened = hydro_binop(OP_BIT_OR, optional, slice_class);
    assert_repr(flattened, "complex | None | slice");

    // A one-element union collapses to its member, like CPython.
    let collapsed = hydro_subscript(union_class, complex_class);
    assert_true(hydro_binop(OP_EQ, collapsed, complex_class));

    // The union value is an instance of `types.UnionType`, and `type()` of it
    // is the same class object.
    let isinstance = builtin("isinstance");
    assert_true(call(isinstance, &[union, union_class]));
    let type_builtin = builtin("type");
    assert_true(hydro_binop(
        OP_EQ,
        call(type_builtin, &[union]),
        union_class,
    ));
}

#[test]
fn union_type_subscript_accepts_values_but_bitor_requires_types() {
    let _guard = setup();
    let union_class = module_get("types", "UnionType");
    let complex_class = module_get("builtins", "complex");
    assert_no_error();
    // Subscription accepts arbitrary members (CPython: `UnionType[1, 2]`), and
    // a one-element union collapses to the member.
    let single = hydro_subscript(union_class, tuple(&[int(1)]));
    assert_int(single, 1);
    let pair = hydro_subscript(union_class, tuple(&[int(1), text("x")]));
    assert_repr(pair, "1 | 'x'");
    // The `|` operator still requires type-like operands.
    hydro_binop(OP_BIT_OR, complex_class, int(1));
    expect_error(TYPE_ERROR);
}

// -------------------------------------------------------------------------
// types.FunctionType
// -------------------------------------------------------------------------

#[test]
fn function_type_identifies_and_wraps_hydro_functions() {
    let _guard = setup();

    // Declared as a module function so `types.FunctionType(...)` lowers; the
    // value is still usable as the second argument of `isinstance`.
    let function_type = module_get("types", "FunctionType");
    assert_no_error();
    let len_function = module_get("builtins", "len");
    assert_no_error();
    let isinstance = builtin("isinstance");
    assert_true(call(isinstance, &[len_function, function_type]));

    // A non-function target is not a `FunctionType`.
    assert_eq!(hydro_exc_pending(), 0);
    assert_eq!(
        call(isinstance, &[int(1), function_type]),
        hydro_false(),
        "ints are not FunctionType"
    );
    assert_no_error();

    // `types.FunctionType(code, globals)` with a Hydro function value returns
    // a new callable that forwards its arguments.
    let wrapped = module("types", "FunctionType", &[len_function, empty_dict()]);
    assert_no_error();
    assert_eq!(unbox(call(wrapped, &[text("abc")])), 3);
    assert_no_error();

    // Code objects are not reified: the request fails loudly.
    module("types", "FunctionType", &[text("not code"), empty_dict()]);
    expect_error(NOT_IMPLEMENTED_ERROR);

    // Rebinding argdefs/closure/kwdefaults is not implementable either.
    module(
        "types",
        "FunctionType",
        &[
            len_function,
            empty_dict(),
            hydro_none(),
            hydro_none(),
            tuple(&[]),
        ],
    );
    expect_error(NOT_IMPLEMENTED_ERROR);
}

// -------------------------------------------------------------------------
// builtin complex / slice
// -------------------------------------------------------------------------

#[test]
fn complex_builtin_parses_and_round_trips_values() {
    let _guard = setup();

    let complex_class = module_get("builtins", "complex");
    assert_no_error();
    assert_repr(complex_class, "<class 'complex'>");

    let value = call(complex_class, &[text("1+2j")]);
    assert_no_error();
    assert_repr(value, "(1+2j)");
    assert_repr(getattr(value, "real"), "1.0");
    assert_repr(getattr(value, "imag"), "2.0");

    let isinstance = builtin("isinstance");
    assert_true(call(isinstance, &[value, complex_class]));

    // `type(value) in (complex, slice)`.
    let type_builtin = builtin("type");
    let slice_class = module_get("builtins", "slice");
    let type_value = call(type_builtin, &[value]);
    assert_true(hydro_binop(
        OP_IN,
        type_value,
        tuple(&[complex_class, slice_class]),
    ));

    // Two-argument construction and copy construction.
    assert_repr(call(complex_class, &[int(1), int(2)]), "(1+2j)");
    assert_repr(call(complex_class, &[value]), "(1+2j)");
    assert_repr(call(complex_class, &[text("2j")]), "2j");
    assert_repr(call(complex_class, &[]), "0j");

    // Equality and `conjugate()`.
    let equal = call(complex_class, &[text("1+2j")]);
    assert_true(hydro_binop(OP_EQ, value, equal));
    assert_repr(method(value, "conjugate", &[]), "(1-2j)");

    // Malformed strings raise `ValueError`, non-numbers `TypeError`.
    call(complex_class, &[text("1 + 2j")]);
    expect_error(VALUE_ERROR);
    call(complex_class, &[tuple(&[])]);
    expect_error(TYPE_ERROR);
}

#[test]
fn slice_builtin_wraps_bounds_and_computes_indices() {
    let _guard = setup();

    let slice_class = module_get("builtins", "slice");
    assert_no_error();

    let two_arg = call(slice_class, &[int(1), int(5)]);
    assert_no_error();
    assert_repr(two_arg, "slice(1, 5, None)");
    assert_int(getattr(two_arg, "start"), 1);
    assert_none(getattr(two_arg, "step"));

    let one_arg = call(slice_class, &[int(5)]);
    assert_repr(one_arg, "slice(None, 5, None)");

    let isinstance = builtin("isinstance");
    assert_true(call(isinstance, &[two_arg, slice_class]));

    // `slice.indices(len)` follows CPython's clamping rules.
    let indices = method(two_arg, "indices", &[int(10)]);
    assert_no_error();
    assert_int(hydro_subscript(indices, int(0)), 1);
    assert_int(hydro_subscript(indices, int(1)), 5);
    assert_int(hydro_subscript(indices, int(2)), 1);

    let reverse = call(slice_class, &[hydro_none(), hydro_none(), int(-1)]);
    let reverse_indices = method(reverse, "indices", &[int(5)]);
    assert_no_error();
    assert_int(hydro_subscript(reverse_indices, int(0)), 4);
    assert_int(hydro_subscript(reverse_indices, int(1)), -1);
    assert_int(hydro_subscript(reverse_indices, int(2)), -1);

    // Zero step is a `ValueError`, non-integer bounds a `TypeError`.
    let zero_step = call(slice_class, &[int(0), int(5), int(0)]);
    method(zero_step, "indices", &[int(10)]);
    expect_error(VALUE_ERROR);
    let bad_bound = call(slice_class, &[text("a"), int(5)]);
    method(bad_bound, "indices", &[int(10)]);
    expect_error(TYPE_ERROR);
}

#[test]
fn slice_and_complex_equality_is_value_based() {
    let _guard = setup();
    let slice_class = module_get("builtins", "slice");
    assert_no_error();
    let left = call(slice_class, &[int(1), int(5)]);
    let right = call(slice_class, &[int(1), int(5)]);
    assert_true(hydro_binop(OP_EQ, left, right));
    let different = call(slice_class, &[int(1), int(6)]);
    assert_eq!(hydro_binop(OP_EQ, left, different), hydro_false());
    assert_no_error();
}

// -------------------------------------------------------------------------
// ast.TemplateStr / ast.Interpolation
// -------------------------------------------------------------------------

#[test]
fn template_ast_nodes_construct_and_unparse_as_tstrings() {
    let _guard = setup();

    let ast_class = module_get("ast", "AST");
    let constant_class = module_get("ast", "Constant");
    let interpolation_class = module_get("ast", "Interpolation");
    let template_class = module_get("ast", "TemplateStr");
    assert_no_error();

    let literal = call(constant_class, &[text("a")]);
    let name = module_kw("ast", "Name", &[], &[("id", text("x"))]);
    let interpolation = call(
        interpolation_class,
        &[name, text("x"), int(114), hydro_none()],
    );
    let trailing = call(constant_class, &[text("}")]);
    let template = call(template_class, &[list(&[literal, interpolation, trailing])]);
    assert_no_error();

    assert_repr(template_class, "<class 'TemplateStr'>");
    let isinstance = builtin("isinstance");
    assert_true(call(isinstance, &[interpolation, ast_class]));

    let unparser = module("ast", "unparse", &[template]);
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_EQ, hydro_to_str(unparser), text("t'a{x!r}}}'")),
        hydro_true()
    );

    // Format specs render inside the interpolation field.
    let format_spec = call(constant_class, &[text(">3")]);
    let formatted = module_kw(
        "ast",
        "Interpolation",
        &[],
        &[
            ("value", name),
            ("str", text("x")),
            ("conversion", int(-1)),
            ("format_spec", format_spec),
        ],
    );
    let formatted_template =
        module_kw("ast", "TemplateStr", &[], &[("values", list(&[formatted]))]);
    let rendered = module("ast", "unparse", &[formatted_template]);
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_EQ, hydro_to_str(rendered), text("t'{x:>3}'")),
        hydro_true()
    );

    // Empty template.
    let empty = call(template_class, &[list(&[])]);
    let rendered = module("ast", "unparse", &[empty]);
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_EQ, hydro_to_str(rendered), text("t''")),
        hydro_true()
    );

    // An interpolation outside a template is not renderable.
    module("ast", "unparse", &[interpolation]);
    expect_error(NOT_IMPLEMENTED_ERROR);
}

#[test]
fn template_node_names_are_ast_classes() {
    let _guard = setup();
    let template_class = module_get("ast", "TemplateStr");
    let _interpolation_class = module_get("ast", "Interpolation");
    let ast_class = module_get("ast", "AST");
    assert_no_error();
    let isinstance = builtin("isinstance");
    let template = call(template_class, &[list(&[])]);
    let interpolation = module_kw(
        "ast",
        "Interpolation",
        &[],
        &[
            ("value", hydro_none()),
            ("str", text("x")),
            ("conversion", int(-1)),
            ("format_spec", hydro_none()),
        ],
    );
    assert_true(call(isinstance, &[template, ast_class]));
    assert_true(call(isinstance, &[interpolation, ast_class]));
    // Unknown template attributes stay an AttributeError instead of a fake
    // class.
    let missing = module_get("ast", "NoSuchTemplateNode");
    let _ = missing;
    expect_error(ATTRIBUTE_ERROR);
}

// -------------------------------------------------------------------------
// Module attribute assignment (pydantic patches typing at import time)
// -------------------------------------------------------------------------

#[test]
fn module_attribute_assignment_overrides_and_restores_native_specs() {
    let _guard = setup();
    // pydantic/typing_extensions execute `typing._collect_type_vars = ...`
    // at import time; the module object must accept the assignment.
    let typing = hydro_module_value(b"typing".as_ptr(), 6);
    assert_no_error();
    let replacement = module_get("builtins", "len");
    assert_no_error();
    assert_none(hydro_setattr(
        typing,
        b"_collect_type_vars".as_ptr(),
        18,
        replacement,
    ));

    // Reads and calls resolve the override.
    assert_true(hydro_binop(
        OP_EQ,
        getattr(typing, "_collect_type_vars"),
        replacement,
    ));
    let namespace = module("builtins", "vars", &[typing]);
    assert_true(hydro_binop(
        OP_EQ,
        hydro_subscript(namespace, text("_collect_type_vars")),
        replacement,
    ));
    assert_int(
        module(
            "typing",
            "_collect_type_vars",
            &[list(&[int(1), int(2), int(3)])],
        ),
        3,
    );

    // Deleting drops the override, so the spec default is visible again.
    assert_none(hydro_delete_attr(
        typing,
        b"_collect_type_vars".as_ptr(),
        18,
    ));
    let restored = getattr(typing, "_collect_type_vars");
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_EQ, restored, replacement),
        hydro_false(),
        "the spec default must come back after deletion"
    );
}
