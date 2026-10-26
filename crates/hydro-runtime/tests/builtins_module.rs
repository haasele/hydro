//! Integration tests for the native `builtins` module shim.
//!
//! The tests drive the public runtime ABI the way generated code does.

use hydro_runtime::abi::*;

const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;
const RUNTIME_ERROR: i64 = 3;
const ATTRIBUTE_ERROR: i64 = 9;
const NOT_IMPLEMENTED_ERROR: i64 = 32;

const OP_EQ: u32 = 7;
const OP_IS: u32 = 15;

fn int(n: i64) -> i64 {
    n << 3
}

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn list(items: &[i64]) -> i64 {
    hydro_list_new(items.len(), items.as_ptr())
}

fn module(module: &str, function: &str, args: &[i64]) -> i64 {
    hydro_module_call(
        module.as_ptr(),
        module.len(),
        function.as_ptr(),
        function.len(),
        args.len(),
        args.as_ptr(),
    )
}

fn module_get(module: &str, name: &str) -> i64 {
    hydro_module_get(module.as_ptr(), module.len(), name.as_ptr(), name.len())
}

fn getattr(obj: i64, name: &str) -> i64 {
    hydro_getattr(obj, name.as_ptr(), name.len())
}

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn assert_error_tag(expected: i64) {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    assert_eq!(hydro_exc_tag(), expected, "wrong exception tag");
    hydro_exc_clear();
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_EQ, actual, int(expected)),
        hydro_true(),
        "expected integer {expected}"
    );
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_EQ, actual, text(expected)),
        hydro_true(),
        "string mismatch"
    );
}

fn assert_true(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_true(), "expected True");
}

fn assert_false(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_false(), "expected False");
}

fn assert_same(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_IS, actual, expected),
        hydro_true(),
        "expected identity"
    );
}

#[test]
fn len_and_issubclass_work_through_the_module() {
    hydro_runtime_init();
    let items = list(&[int(1), int(2)]);
    assert_int(module("builtins", "len", &[items]), 2);

    let syntax_error = module_get("builtins", "SyntaxError");
    let exception = module_get("builtins", "Exception");
    assert_no_error();
    assert_true(module("builtins", "issubclass", &[syntax_error, exception]));
    assert_false(module("builtins", "issubclass", &[exception, syntax_error]));
}

#[test]
fn singleton_constants_are_the_runtime_values() {
    hydro_runtime_init();
    assert_same(module_get("builtins", "None"), hydro_none());
    assert_same(module_get("builtins", "True"), hydro_true());
    assert_same(module_get("builtins", "False"), hydro_false());
    assert_same(module_get("builtins", "__debug__"), hydro_true());
    // Ellipsis is a cached instance with a stable identity.
    let first = module_get("builtins", "Ellipsis");
    let second = module_get("builtins", "Ellipsis");
    assert_same(first, second);
    // Every taxonomy exception name resolves.
    for name in [
        "BaseException",
        "UnicodeDecodeError",
        "LookupError",
        "NotImplementedError",
        "IOError",
    ] {
        let class = module_get("builtins", name);
        assert_no_error();
        assert_eq!(
            hydro_binop(OP_IS, class, hydro_none()),
            hydro_false(),
            "{name} should be a class value"
        );
    }
    // `__dict__` is the builtins namespace mapping: it carries the actual
    // singleton constants and callable values.
    let namespace = module_get("builtins", "__dict__");
    assert_no_error();
    assert_same(hydro_subscript(namespace, text("None")), hydro_none());
    assert_same(hydro_subscript(namespace, text("True")), hydro_true());
    let len_function = hydro_subscript(namespace, text("len"));
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_IS, len_function, hydro_none()),
        hydro_false(),
        "builtins.__dict__['len'] should be a function value"
    );
}

#[test]
fn object_is_the_universal_cached_base() {
    hydro_runtime_init();
    let object = module_get("builtins", "object");
    assert_no_error();
    let instance = hydro_call(object, 0, std::ptr::null());
    assert_no_error();
    let class = getattr(instance, "__class__");
    assert_same(class, object);
    // `object` is a class value, so calling it with an argument is rejected.
    let args = [int(1)];
    let _ = hydro_call(object, args.len(), args.as_ptr());
    assert_error_tag(TYPE_ERROR);
}

#[test]
fn setattr_vars_delattr_and_dir() {
    hydro_runtime_init();
    let object = module_get("builtins", "object");
    let instance = hydro_instance_new(object);

    let set_args = [instance, text("x"), int(5)];
    let _ = module("builtins", "setattr", &set_args);
    assert_no_error();

    let namespace = module("builtins", "vars", &[instance]);
    assert_no_error();
    assert_int(hydro_subscript(namespace, text("x")), 5);

    let names = module("builtins", "dir", &[instance]);
    assert_no_error();
    let names = hydro_list_from(names);
    assert_no_error();
    assert!(hydro_len(names) > 0);

    let del_args = [instance, text("x")];
    let _ = module("builtins", "delattr", &del_args);
    assert_no_error();
    let _ = getattr(instance, "x");
    assert_error_tag(ATTRIBUTE_ERROR);
}

#[test]
fn unsupported_names_raise_tagged_errors() {
    hydro_runtime_init();
    let _ = module("builtins", "globals", &[]);
    assert_error_tag(RUNTIME_ERROR);
    let _ = module("builtins", "locals", &[]);
    assert_error_tag(RUNTIME_ERROR);

    let source = text("1 + 1");
    let filename = text("<test>");
    let mode = text("eval");
    let _ = module("builtins", "compile", &[source, filename, mode]);
    assert_error_tag(NOT_IMPLEMENTED_ERROR);

    // `type(name, bases, dict)` is a real class constructor through the
    // `builtins` module as well as through a first-class `type` value.
    let name = text("Made");
    let object = module_get("builtins", "object");
    let base_values = [object];
    let bases = hydro_tuple_new(1, base_values.as_ptr());
    let keys = [text("answer")];
    let values = [int(7)];
    let namespace = hydro_dict_new(1, keys.as_ptr(), values.as_ptr());
    let type_args = [name, bases, namespace];
    let made = module("builtins", "type", &type_args);
    assert_no_error();
    assert_str(getattr(made, "__name__"), "Made");
    assert_int(getattr(made, "answer"), 7);
    let type_function = hydro_builtin_function("type".as_ptr(), 4);
    assert_no_error();
    let made = hydro_call(type_function, type_args.len(), type_args.as_ptr());
    assert_no_error();
    assert_str(getattr(made, "__name__"), "Made");

    // The 2-argument form has no meaning and is an arity error.
    let _ = module("builtins", "type", &[text("Made"), bases]);
    assert_error_tag(TYPE_ERROR);

    // `iter(callable, sentinel)` requires a callable first argument.
    let _ = module("builtins", "iter", &[hydro_true(), hydro_true()]);
    assert_error_tag(TYPE_ERROR);

    // `type(x)` still works for one argument.
    let class = module("builtins", "type", &[int(1)]);
    assert_no_error();
    assert!(hydro_binop(OP_IS, class, hydro_none()) == hydro_false());

    // Without a frame context the zero-argument forms fail loudly.
    let _ = module("builtins", "vars", &[]);
    assert_error_tag(RUNTIME_ERROR);

    // `vars(module)` returns the module namespace mapping, not an error.
    let module_value = hydro_module_value("math".as_ptr(), "math".len());
    let namespace = module("builtins", "vars", &[module_value]);
    assert_no_error();
    assert_str(hydro_subscript(namespace, text("__name__")), "math");
    let sqrt = hydro_subscript(namespace, text("sqrt"));
    assert_no_error();
    assert_eq!(
        hydro_binop(OP_IS, sqrt, hydro_none()),
        hydro_false(),
        "math namespace should expose its functions"
    );
}

#[test]
fn id_and_attribute_name_validation() {
    hydro_runtime_init();
    let one = module("builtins", "id", &[int(7)]);
    let two = module("builtins", "id", &[int(7)]);
    assert_no_error();
    assert_eq!(hydro_binop(OP_EQ, one, two), hydro_true());

    let left_value = text("left");
    let right_value = text("right");
    let left = module("builtins", "id", &[left_value]);
    let right = module("builtins", "id", &[right_value]);
    assert_no_error();
    assert_eq!(hydro_binop(OP_EQ, left, right), hydro_false());

    let object = module_get("builtins", "object");
    let instance = hydro_instance_new(object);
    let _ = module("builtins", "setattr", &[instance, int(1), int(2)]);
    assert_error_tag(TYPE_ERROR);
    let _ = module("builtins", "delattr", &[instance, int(1)]);
    assert_error_tag(TYPE_ERROR);
    // `dir()` without an argument has no frame context, so the runtime entry
    // point reports that instead of inventing a namespace.
    let _ = module("builtins", "dir", &[]);
    assert_error_tag(RUNTIME_ERROR);
    let _ = VALUE_ERROR;
}

#[test]
fn import_builtin_resolves_registered_modules() {
    hydro_runtime_init();
    let math = module("builtins", "__import__", &[text("math")]);
    assert_no_error();
    assert_str(getattr(math, "__name__"), "math");

    let _ = module("builtins", "__import__", &[text("definitely_not_a_module")]);
    assert_eq!(hydro_exc_pending(), 1);
    assert_eq!(hydro_exc_tag(), 31); // ModuleNotFoundError
    hydro_exc_clear();
}
