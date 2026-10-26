//! Integration tests for `abc` (PEP 3119) and `_opcode`.
//!
//! `abc` tests pin the real `ABCMeta` class: classes created with it, virtual
//! subclass registration that the builtin `isinstance`/`issubclass` consult,
//! the metaclass hooks, abstract-method reporting and the explicit errors for
//! arguments that are not classes.
//!
//! `_opcode` tests pin the truthful bytecode-less surface: the predicates the
//! pure-Python `opcode` module imports answer `False`, operations that would
//! need CPython bytecode raise `NotImplementedError`, and no opcode table is
//! invented.

use hydro_runtime::abi::*;

static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

const NONE: i64 = 1;

const TYPE_ERROR: i64 = 1;
const ATTRIBUTE_ERROR: i64 = 9;
const NOT_IMPLEMENTED_ERROR: i64 = 32;

const OP_EQ: u32 = 7;
const OP_IN: u32 = 13;
const OP_IS: u32 = 15;

fn int(n: i64) -> i64 {
    n << 3
}

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn module(function: &str, args: &[i64]) -> i64 {
    hydro_module_call(
        "_opcode".as_ptr(),
        "_opcode".len(),
        function.as_ptr(),
        function.len(),
        args.len(),
        args.as_ptr(),
    )
}

fn abc_module(function: &str, args: &[i64]) -> i64 {
    hydro_module_call(
        "abc".as_ptr(),
        3,
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

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    hydro_method(
        obj,
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

fn call(function: i64, args: &[i64]) -> i64 {
    hydro_call(
        function,
        args.len(),
        if args.is_empty() {
            std::ptr::null()
        } else {
            args.as_ptr()
        },
    )
}

fn call_builtin(name: &str, args: &[i64]) -> i64 {
    let function = hydro_builtin_function(name.as_ptr(), name.len());
    call(function, args)
}

/// First-class value of a builtin (`int`, `bool`, `type`, …).
///
/// The length is always taken from the string: a hand-counted literal length
/// makes the ABI read past the buffer, which is exactly the defect that
/// produced a pending exception instead of the `int` type value.
fn builtin(name: &str) -> i64 {
    hydro_builtin_function(name.as_ptr(), name.len())
}

fn class(name: &str, base: i64) -> i64 {
    hydro_class_new(
        name.as_ptr(),
        name.len(),
        base,
        0,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        0,
        std::ptr::null(),
        std::ptr::null(),
    )
}

fn class_multi_with_meta(
    name: &str,
    bases: &[i64],
    metaclass: i64,
    methods: &[(&str, i64, i64)],
    attrs: &[(&str, i64)],
) -> i64 {
    let names: Vec<i64> = methods.iter().map(|(name, _, _)| text(name)).collect();
    let funcs: Vec<i64> = methods.iter().map(|(_, function, _)| *function).collect();
    let kinds: Vec<i64> = methods.iter().map(|(_, _, kind)| *kind).collect();
    let attr_names: Vec<i64> = attrs.iter().map(|(name, _)| text(name)).collect();
    let attr_values: Vec<i64> = attrs.iter().map(|(_, value)| *value).collect();
    hydro_class_new_multi(
        name.as_ptr(),
        name.len(),
        bases.as_ptr(),
        bases.len(),
        metaclass,
        if methods.is_empty() {
            std::ptr::null()
        } else {
            names.as_ptr()
        },
        if methods.is_empty() {
            std::ptr::null()
        } else {
            funcs.as_ptr()
        },
        if methods.is_empty() {
            std::ptr::null()
        } else {
            kinds.as_ptr()
        },
        methods.len(),
        if attrs.is_empty() {
            std::ptr::null()
        } else {
            attr_names.as_ptr()
        },
        if attrs.is_empty() {
            std::ptr::null()
        } else {
            attr_values.as_ptr()
        },
        attrs.len(),
    )
}

extern "C" fn render_stub(_: *const i64) -> i64 {
    NONE
}

fn function(entry: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
    hydro_function_new(entry as *const u8, arity, name.as_ptr(), name.len(), 0)
}

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn assert_error_tag(expected: i64) {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    assert_eq!(hydro_exc_tag(), expected, "wrong exception tag");
    hydro_exc_clear();
}

fn assert_true(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_true());
}

fn assert_false(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_false());
}

fn assert_same(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(hydro_binop(OP_IS, actual, expected), hydro_true());
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert_eq!(hydro_binop(OP_EQ, actual, text(expected)), hydro_true());
}

fn assert_contains(container: i64, item: i64) {
    assert_no_error();
    assert_eq!(hydro_binop(OP_IN, item, container), hydro_true());
}

// -------------------------------------------------------------------------
// abc surface
// -------------------------------------------------------------------------

#[test]
fn abc_meta_is_a_real_class_and_abc_uses_it() {
    let _guard = test_guard();
    hydro_runtime_init();

    let metaclass = module_get("abc", "ABCMeta");
    let abc = module_get("abc", "ABC");
    assert_no_error();
    assert_str(getattr(metaclass, "__name__"), "ABCMeta");
    assert_str(getattr(abc, "__name__"), "ABC");

    // `type(ABCMeta)` is the native `type` class; `type(ABC)` is ABCMeta.
    let type_builtin = builtin("type");
    let plain = class("Plain", NONE);
    let native_type = call(type_builtin, &[plain]);
    assert_same(call(type_builtin, &[metaclass]), native_type);
    assert_same(call(type_builtin, &[abc]), metaclass);
    assert_true(call_builtin("isinstance", &[abc, metaclass]));

    // `class Number(metaclass=ABCMeta)`.
    let number = class_multi_with_meta("Number", &[], metaclass, &[], &[]);
    assert_no_error();
    assert_str(getattr(number, "__name__"), "Number");
    assert_same(call(type_builtin, &[number]), metaclass);
    assert_true(call_builtin("isinstance", &[number, metaclass]));

    // `class Child(ABC)` inherits the metaclass.
    let child = class("Child", abc);
    assert_no_error();
    assert_same(call(type_builtin, &[child]), metaclass);
}

#[test]
fn registration_changes_the_builtin_checks() {
    let _guard = test_guard();
    hydro_runtime_init();
    let metaclass = module_get("abc", "ABCMeta");
    let number = class_multi_with_meta("Number", &[], metaclass, &[], &[]);

    let real = class("Real", NONE);
    assert_false(call_builtin("issubclass", &[real, number]));

    // `Number.register(Real)` is reached through the metaclass fallback.
    let registered = method(number, "register", &[real]);
    assert_same(registered, real);
    assert_true(call_builtin("issubclass", &[real, number]));
    assert_true(call_builtin(
        "isinstance",
        &[hydro_instance_new(real), number],
    ));

    // Subclasses of a registered class are virtual subclasses too.
    let derived = class("Derived", real);
    assert_true(call_builtin("issubclass", &[derived, number]));
    assert_true(call_builtin(
        "isinstance",
        &[hydro_instance_new(derived), number],
    ));

    // Unrelated values still do not match.
    assert_false(call_builtin("isinstance", &[text("x"), number]));

    // The hooks answer the same questions.
    assert_true(method(number, "__subclasscheck__", &[real]));
    assert_true(method(
        number,
        "__instancecheck__",
        &[hydro_instance_new(real)],
    ));
    assert_false(method(number, "__instancecheck__", &[text("x")]));

    // Registration is idempotent; the cache token moved once.
    assert_same(method(number, "register", &[real]), real);
    assert_eq!(
        hydro_binop(OP_EQ, abc_module("get_cache_token", &[]), int(1)),
        hydro_true()
    );
    assert_no_error();
}

#[test]
fn registration_accepts_native_type_spellings() {
    let _guard = test_guard();
    hydro_runtime_init();
    let metaclass = module_get("abc", "ABCMeta");
    let number = class_multi_with_meta("Number", &[], metaclass, &[], &[]);

    // `numbers.Number.register(int)`-style registration: Hydro's `int` is a
    // builtin function value that denotes the native scalar type.
    let int_builtin = builtin("int");
    assert_same(method(number, "register", &[int_builtin]), int_builtin);

    assert_true(call_builtin("isinstance", &[int(1), number]));
    // `bool` is an `int` subclass.
    assert_true(call_builtin("isinstance", &[hydro_true(), number]));
    assert_false(call_builtin("isinstance", &[text("x"), number]));
    assert_false(call_builtin("isinstance", &[hydro_float_new(1.5), number]));
}

#[test]
fn registration_rejects_non_classes_and_non_abcs() {
    let _guard = test_guard();
    hydro_runtime_init();
    let metaclass = module_get("abc", "ABCMeta");
    let number = class_multi_with_meta("Number", &[], metaclass, &[], &[]);

    // Not a class: CPython's "Can only register classes".
    let result = method(number, "register", &[text("payload")]);
    assert_eq!(result, hydro_none());
    assert_error_tag(TYPE_ERROR);

    // A class that is not an ABC cannot register virtual subclasses:
    // `ABCMeta.register(Plain, Real)` reports the same error the class-level
    // lookup would (there is no `Plain.register` attribute at all).
    let plain = class("Plain", NONE);
    let real = class("Real", NONE);
    method(metaclass, "register", &[plain, real]);
    assert_error_tag(TYPE_ERROR);
}

#[test]
fn abstract_methods_are_reported_through_the_metaclass() {
    let _guard = test_guard();
    hydro_runtime_init();
    let metaclass = module_get("abc", "ABCMeta");

    // `@abstractmethod` marks the function and returns it unchanged.
    let abstract_fn = function(render_stub, 1, "render");
    assert_same(abc_module("abstractmethod", &[abstract_fn]), abstract_fn);

    let base = class_multi_with_meta("Base", &[], metaclass, &[("render", abstract_fn, 0)], &[]);
    let abstracts = getattr(base, "__abstractmethods__");
    assert_no_error();
    assert_eq!(hydro_len(abstracts), int(1));
    assert_contains(abstracts, text("render"));

    // A concrete override clears the name; the property is computed from the
    // live namespace and MRO.
    let concrete = function(render_stub, 1, "render");
    let derived = class_multi_with_meta("Derived", &[base], NONE, &[("render", concrete, 0)], &[]);
    assert_eq!(hydro_len(getattr(derived, "__abstractmethods__")), int(0));
    assert_no_error();

    // `update_abstractmethods` is the identity for this runtime.
    assert_same(abc_module("update_abstractmethods", &[base]), base);
    // The deprecated descriptors are importable but explicitly unsupported.
    abc_module("abstractproperty", &[abstract_fn]);
    assert_error_tag(NOT_IMPLEMENTED_ERROR);
}

// -------------------------------------------------------------------------
// _opcode surface
// -------------------------------------------------------------------------

#[test]
fn opcode_import_surface_exists() {
    let _guard = test_guard();
    hydro_runtime_init();

    // Every name CPython 3.14's `opcode.py` (and `dis.py`) imports from
    // `_opcode`, plus the introspection helpers.
    for name in [
        "stack_effect",
        "has_arg",
        "has_const",
        "has_name",
        "has_jump",
        "has_free",
        "has_local",
        "has_exc",
        "is_valid",
        "get_executor",
        "get_specialization_stats",
        "get_intrinsic1_descs",
        "get_intrinsic2_descs",
        "get_special_method_names",
        "get_nb_ops",
        "ENABLE_SPECIALIZATION",
        "ENABLE_SPECIALIZATION_FT",
    ] {
        let member = module_get("_opcode", name);
        assert_no_error();
        assert_ne!(member, hydro_none(), "_opcode.{name} missing");
    }

    // `opmap`/`opname` are not `_opcode` attributes (they live in
    // `_opcode_metadata` in CPython 3.14).
    module_get("_opcode", "opmap");
    assert_error_tag(ATTRIBUTE_ERROR);
    module_get("_opcode", "opname");
    assert_error_tag(ATTRIBUTE_ERROR);

    // Specialization is disabled.
    assert_eq!(module_get("_opcode", "ENABLE_SPECIALIZATION"), int(0));
    assert_eq!(module_get("_opcode", "ENABLE_SPECIALIZATION_FT"), int(0));
    assert_no_error();
}

#[test]
fn opcode_predicates_report_no_opcodes() {
    let _guard = test_guard();
    hydro_runtime_init();
    for name in [
        "has_arg",
        "has_const",
        "has_name",
        "has_jump",
        "has_free",
        "has_local",
        "has_exc",
        "is_valid",
    ] {
        assert_false(module(name, &[int(1)]));
    }
}

#[test]
fn opcode_unsupported_operations_raise_not_implemented() {
    let _guard = test_guard();
    hydro_runtime_init();

    let result = module("stack_effect", &[int(1), int(0)]);
    assert_eq!(result, hydro_none());
    assert_error_tag(NOT_IMPLEMENTED_ERROR);

    module("get_executor", &[int(1), int(0)]);
    assert_error_tag(NOT_IMPLEMENTED_ERROR);

    module("get_specialization_stats", &[]);
    assert_error_tag(NOT_IMPLEMENTED_ERROR);
}

#[test]
fn opcode_metadata_helpers_are_empty() {
    let _guard = test_guard();
    hydro_runtime_init();
    for name in ["get_intrinsic1_descs", "get_intrinsic2_descs", "get_nb_ops"] {
        assert_eq!(hydro_len(module(name, &[])), int(0), "{name}");
    }
    assert_eq!(hydro_len(module("get_special_method_names", &[])), int(0));
}

#[test]
fn opcode_unknown_name_raises_attribute_error() {
    let _guard = test_guard();
    hydro_runtime_init();
    module("missing", &[]);
    assert_error_tag(ATTRIBUTE_ERROR);
}
