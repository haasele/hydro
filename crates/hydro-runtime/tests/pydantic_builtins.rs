//! Integration tests for the builtin surfaces the `annotationlib`/pydantic
//! bootstrap needs beyond the earlier shims:
//!
//! * `object` as the universal cached base class (`object()`,
//!   `isinstance(x, object)`, `issubclass(C, object)`, `class C(object)`);
//! * `zip(..., strict=...)` in its direct and first-class keyword forms,
//!   including CPython's `ValueError` diagnostics;
//! * `super(type, object)` as a first-class builtin value;
//! * the `iter`/`next`/`type` surfaces those code paths rely on.
//!
//! Every test drives the public runtime ABI the way generated code does.

use std::sync::{LazyLock, Mutex, MutexGuard};

use hydro_runtime::abi::*;

const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;
const RUNTIME_ERROR: i64 = 3;
const STOP_ITERATION: i64 = 8;

const OP_EQ: u32 = 7;
const OP_IS: u32 = 15;

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

fn list(items: &[i64]) -> i64 {
    hydro_list_new(items.len(), items.as_ptr())
}

fn module_get(module_name: &str, name: &str) -> i64 {
    hydro_module_get(
        module_name.as_ptr(),
        module_name.len(),
        name.as_ptr(),
        name.len(),
    )
}

fn builtin(name: &str) -> i64 {
    hydro_builtin_function(name.as_ptr(), name.len())
}

fn call(callable: i64, args: &[i64]) -> i64 {
    hydro_call(
        callable,
        args.len(),
        if args.is_empty() {
            std::ptr::null()
        } else {
            args.as_ptr()
        },
    )
}

fn call_kw(callable: i64, args: &[i64], keywords: &[(&str, i64)]) -> i64 {
    let mut pairs = Vec::with_capacity(keywords.len() * 2);
    for (key, value_in) in keywords {
        pairs.push(text(key));
        pairs.push(*value_in);
    }
    hydro_call_kw(
        callable,
        args.len(),
        if args.is_empty() {
            std::ptr::null()
        } else {
            args.as_ptr()
        },
        keywords.len(),
        if pairs.is_empty() {
            std::ptr::null()
        } else {
            pairs.as_ptr()
        },
    )
}

fn function(wrapper: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
    hydro_function_new(wrapper as *const u8, arity, name.as_ptr(), name.len(), 0)
}

fn class(name: &str, base: i64, methods: &[(&str, i64)]) -> i64 {
    let names: Vec<i64> = methods.iter().map(|(name, _)| text(name)).collect();
    let funcs: Vec<i64> = methods.iter().map(|(_, func)| *func).collect();
    let kinds: Vec<i64> = vec![0; methods.len()];
    let (names_ptr, funcs_ptr, kinds_ptr) = if methods.is_empty() {
        (std::ptr::null(), std::ptr::null(), std::ptr::null())
    } else {
        (names.as_ptr(), funcs.as_ptr(), kinds.as_ptr())
    };
    hydro_class_new(
        name.as_ptr(),
        name.len(),
        base,
        methods.len(),
        names_ptr,
        funcs_ptr,
        kinds_ptr,
        0,
        std::ptr::null(),
        std::ptr::null(),
    )
}

fn method(object: i64, name: &str, args: &[i64]) -> i64 {
    hydro_method(
        object,
        name.as_ptr(),
        name.len(),
        args.len(),
        if args.is_empty() {
            std::ptr::null()
        } else {
            args.as_ptr()
        },
    )
}

fn getattr(object: i64, name: &str) -> i64 {
    hydro_getattr(object, name.as_ptr(), name.len())
}

fn len(value_in: i64) -> i64 {
    hydro_len(value_in) >> 3
}

fn same(a: i64, b: i64) -> bool {
    hydro_binop(OP_EQ, a, b) == hydro_true()
}

fn same_object(a: i64, b: i64) -> bool {
    hydro_binop(OP_IS, a, b) == hydro_true()
}

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn assert_true(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_true(), "expected True");
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert!(
        same(actual, int(expected)),
        "value mismatch, got {:?}",
        hydro_to_str(actual)
    );
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert!(
        same(actual, text(expected)),
        "value mismatch, got {:?}",
        hydro_to_str(actual)
    );
}

fn assert_error_tag(expected: i64) {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    assert_eq!(hydro_exc_tag(), expected, "wrong exception tag");
    hydro_exc_clear();
}

fn assert_error(expected_tag: i64, expected_message: &str) {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    assert_eq!(hydro_exc_tag(), expected_tag, "wrong exception tag");
    let message = hydro_to_str(hydro_exc_value());
    assert!(
        same(message, text(expected_message)),
        "message mismatch, got {:?}",
        hydro_to_str(message)
    );
    hydro_exc_clear();
}

/// `m(self)`: returns 7, ignoring the receiver.
extern "C" fn base_method(_argv: *const i64) -> i64 {
    int(7)
}

#[test]
fn object_is_the_universal_base_class() {
    let _guard = setup();
    let object = module_get("builtins", "object");
    assert_no_error();

    // `object()` builds an instance whose `__class__` is `object` itself.
    let instance = call(object, &[]);
    assert_no_error();
    assert!(same_object(getattr(instance, "__class__"), object));

    // `object` keeps CPython's constructor contract.
    let _ = call(object, &[int(1)]);
    assert_error(TYPE_ERROR, "object() takes no arguments");

    // `isinstance` covers every value kind, since `object` is universal.
    let isinstance = builtin("isinstance");
    for value_in in [int(1), text("x"), list(&[]), hydro_none()] {
        assert_true(call(isinstance, &[value_in, object]));
    }
    assert_true(call(isinstance, &[instance, object]));

    // Classes are objects too, and every class derives from the base.
    let plain = class("Plain", hydro_none(), &[]);
    assert_true(call(isinstance, &[plain, object]));
    assert_true(call(builtin("issubclass"), &[plain, object]));

    // An explicit `object` base is accepted and stays the same class value.
    let explicit = class("Explicit", object, &[]);
    let explicit_instance = call(explicit, &[]);
    assert_no_error();
    assert!(same_object(
        getattr(explicit_instance, "__class__"),
        explicit
    ));
    assert_true(call(isinstance, &[explicit_instance, object]));
    assert_true(call(builtin("issubclass"), &[explicit, object]));
}

#[test]
fn zip_strict_keyword_forms() {
    let _guard = setup();
    let zip = builtin("zip");
    let left = list(&[int(1), int(2)]);
    let right = list(&[int(3)]);

    // Without `strict`, `zip` truncates to the shortest source.
    let zipped = call(zip, &[left, right]);
    assert_no_error();
    assert_eq!(len(zipped), 1);

    // `strict=True` reports the first source that ended after an earlier one.
    let _ = call_kw(zip, &[left, right], &[("strict", hydro_true())]);
    assert_error(VALUE_ERROR, "zip() argument 2 is shorter than argument 1");

    // `strict=False` keeps the truncating behaviour.
    let zipped = call_kw(zip, &[left, right], &[("strict", hydro_false())]);
    assert_no_error();
    assert_eq!(len(zipped), 1);

    // The flag is evaluated by Python truthiness, not by type.
    let _ = call_kw(zip, &[left, right], &[("strict", int(1))]);
    assert_error(VALUE_ERROR, "zip() argument 2 is shorter than argument 1");

    // A later source that still yields is reported as longer.
    let first = list(&[int(1)]);
    let second = list(&[int(2), int(3)]);
    let _ = call_kw(zip, &[first, second], &[("strict", hydro_true())]);
    assert_error(VALUE_ERROR, "zip() argument 2 is longer than argument 1");

    // Three sources use CPython's plural diagnostics.
    let a = list(&[int(1), int(2), int(3)]);
    let b = list(&[int(4), int(5), int(6)]);
    let c = list(&[int(7), int(8)]);
    let _ = call_kw(zip, &[a, b, c], &[("strict", hydro_true())]);
    assert_error(
        VALUE_ERROR,
        "zip() argument 3 is shorter than arguments 1-2",
    );

    // Equal lengths succeed and keep every row.
    let even = call_kw(
        zip,
        &[left, list(&[int(4), int(5)])],
        &[("strict", hydro_true())],
    );
    assert_no_error();
    assert_eq!(len(even), 2);

    // Unknown and duplicate keywords are rejected without dropping them.
    let _ = call_kw(zip, &[left, right], &[("mode", hydro_true())]);
    assert_error(
        TYPE_ERROR,
        "zip() got an unexpected keyword argument 'mode'",
    );
    let _ = call_kw(
        zip,
        &[left, right],
        &[("strict", hydro_true()), ("strict", hydro_true())],
    );
    assert_error(
        TYPE_ERROR,
        "zip() got multiple values for keyword argument 'strict'",
    );

    // The first-class value resolves through the `builtins` module too; a
    // plain positional call keeps the truncating behaviour.
    let module_zip = module_get("builtins", "zip");
    let zipped = call(module_zip, &[left, right]);
    assert_no_error();
    assert_eq!(len(zipped), 1);
}

#[test]
fn super_two_argument_builtin() {
    let _guard = setup();
    let base = class(
        "Base",
        hydro_none(),
        &[("m", function(base_method, 1, "m"))],
    );
    let derived = class("Derived", base, &[]);
    let instance = call(derived, &[]);
    assert_no_error();

    let super_fn = builtin("super");

    // `super(Derived, instance).m()` starts after `Derived` and finds `Base.m`.
    let proxy = call(super_fn, &[derived, instance]);
    assert_no_error();
    assert_int(method(proxy, "m", &[]), 7);

    // The second argument may be a class (the `classmethod` form).
    let proxy = call(super_fn, &[derived, derived]);
    assert_no_error();
    assert_int(method(proxy, "m", &[]), 7);

    // A class outside the MRO is rejected like CPython.
    let other = class("Other", hydro_none(), &[]);
    let _ = call(super_fn, &[other, instance]);
    assert_error(
        TYPE_ERROR,
        "super(type, obj): obj must be an instance or subtype of type",
    );

    // The first argument must be a class.
    let _ = call(super_fn, &[int(1), instance]);
    assert_error(TYPE_ERROR, "super() argument 1 must be a type, not 'int'");

    // A zero-argument first-class call has no frame context.
    let _ = call(super_fn, &[]);
    assert_error(RUNTIME_ERROR, "super(): no arguments");
}

#[test]
fn iter_next_and_type_surfaces() {
    let _guard = setup();
    let iter_fn = builtin("iter");
    let next_fn = builtin("next");

    let iterator = call(iter_fn, &[list(&[int(5), int(6)])]);
    assert_no_error();
    assert_int(call(next_fn, &[iterator]), 5);
    assert_int(call(next_fn, &[iterator]), 6);

    // The default suppresses `StopIteration`.
    assert_int(call(next_fn, &[iterator, int(-1)]), -1);

    // Without a default the exhaustion is reported.
    let _ = call(next_fn, &[iterator]);
    assert_error_tag(STOP_ITERATION);

    // `type(x)` returns a class value that `isinstance` and `__name__` accept.
    let type_fn = builtin("type");
    let class_of_int = call(type_fn, &[int(1)]);
    assert_no_error();
    assert_str(getattr(class_of_int, "__name__"), "int");
    assert_true(call(builtin("isinstance"), &[int(1), class_of_int]));

    let base = class("Thing", hydro_none(), &[]);
    let thing = call(base, &[]);
    assert_no_error();
    let class_of_thing = call(type_fn, &[thing]);
    assert_no_error();
    assert_str(getattr(class_of_thing, "__name__"), "Thing");
    assert!(same_object(class_of_thing, base));
    assert_true(call(builtin("isinstance"), &[thing, class_of_thing]));
}
