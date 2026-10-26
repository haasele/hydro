//! Integration tests for the `dis`/`sys` prerequisites: the `sys.last_*`
//! attributes and the `hex`/`bin`/`oct` builtins.
//!
//! `sys.last_exc`, `last_type`, `last_value` and `last_traceback` must exist
//! so `dis.distb` (and `code.py`) can read them behind `hasattr`; Hydro is a
//! batch compiler with no interactive frame, so they are `None` rather than a
//! fabricated exception.
//!
//! `hex`, `bin` and `oct` are exercised both through the `builtins` module
//! (the direct-call path) and as first-class builtin function values. Values
//! cover tagged integers, `bool`, signed values, zero and arbitrary-precision
//! `BigInt`s; non-integers report CPython's `TypeError`.

use hydro_runtime::abi::*;

static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

const NONE: i64 = 1;
const TYPE_ERROR: i64 = 1;

fn int(n: i64) -> i64 {
    n << 3
}

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn bigint(value: &str) -> i64 {
    hydro_bigint_from_str(value.as_ptr(), value.len())
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

/// Calls a builtin through its first-class function value (`f = hex; f(1)`).
fn builtin(name: &str, args: &[i64]) -> i64 {
    let function = hydro_builtin_function(name.as_ptr(), name.len());
    assert_eq!(
        hydro_exc_pending(),
        0,
        "builtin function lookup for {name} raised"
    );
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

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

/// Tagged-value equality through the runtime's `==` operator (code 7).
fn value_eq(left: i64, right: i64) -> bool {
    hydro_truthy(hydro_binop(7, left, right)) != 0
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert!(
        value_eq(actual, text(expected)),
        "string mismatch: expected {expected:?}"
    );
}

fn assert_type_error(actual: i64, expected_message: &str) {
    assert_eq!(actual, NONE, "a failing call must return None");
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    assert_eq!(hydro_exc_tag(), TYPE_ERROR, "wrong exception tag");
    let rendered = hydro_to_str(hydro_exc_value());
    assert!(
        value_eq(rendered, text(expected_message)),
        "message mismatch: expected {expected_message:?}"
    );
    hydro_exc_clear();
}

#[test]
fn sys_last_exception_attributes_are_none() {
    let _guard = test_guard();
    hydro_runtime_init();
    // `dis.distb` reads `sys.last_exc` under `hasattr` and then follows the
    // "no last traceback" path; every spelling must resolve instead of
    // raising `AttributeError`.
    for name in ["last_exc", "last_type", "last_value", "last_traceback"] {
        let value = module_get("sys", name);
        assert_no_error();
        assert_eq!(value, NONE, "sys.{name} should be None");
    }
    // The names are also importable through `builtins.__dict__`-style module
    // materialisation, which walks the same spec table.
    let namespace = module_get("sys", "__dict__");
    assert_no_error();
    for name in ["last_exc", "last_type", "last_value", "last_traceback"] {
        let value = hydro_subscript(namespace, text(name));
        assert_no_error();
        assert_eq!(value, NONE, "sys.__dict__[{name:?}] should be None");
    }
}

#[test]
fn hex_bin_oct_direct_calls_are_exact() {
    let _guard = test_guard();
    hydro_runtime_init();
    // The integer literal is materialised immediately before the call so no
    // test-held heap value has to survive another allocation.
    let cases: &[(&str, &str, &str)] = &[
        ("hex", "0", "0x0"),
        ("hex", "10", "0xa"),
        ("hex", "-10", "-0xa"),
        ("hex", "True", "0x1"),
        ("hex", "1180591620717411303424", "0x400000000000000000"),
        ("hex", "-1180591620717411303424", "-0x400000000000000000"),
        ("bin", "0", "0b0"),
        ("bin", "10", "0b1010"),
        ("bin", "-10", "-0b1010"),
        ("bin", "True", "0b1"),
        (
            "bin",
            "1180591620717411303424",
            "0b10000000000000000000000000000000000000000000000000000000000000000000000",
        ),
        (
            "bin",
            "-1180591620717411303424",
            "-0b10000000000000000000000000000000000000000000000000000000000000000000000",
        ),
        ("oct", "0", "0o0"),
        ("oct", "10", "0o12"),
        ("oct", "-10", "-0o12"),
        ("oct", "True", "0o1"),
        (
            "oct",
            "1180591620717411303424",
            "0o200000000000000000000000",
        ),
        (
            "oct",
            "-1180591620717411303424",
            "-0o200000000000000000000000",
        ),
    ];
    for &(function, literal, expected) in cases {
        let argument = if literal == "True" {
            hydro_true()
        } else {
            bigint(literal)
        };
        let result = module("builtins", function, &[argument]);
        assert_str(result, expected);
    }
}

#[test]
fn hex_bin_oct_first_class_calls_match_the_direct_path() {
    let _guard = test_guard();
    hydro_runtime_init();
    for (function, argument, expected) in [
        ("hex", int(10), "0xa"),
        ("hex", int(-10), "-0xa"),
        ("bin", int(10), "0b1010"),
        ("bin", int(-10), "-0b1010"),
        ("oct", int(10), "0o12"),
        ("oct", int(-10), "-0o12"),
    ] {
        let result = builtin(function, &[argument]);
        assert_str(result, expected);
    }
    // A builtin value can be passed around (`map(hex, ...)` shape).
    let function = hydro_builtin_function(b"hex".as_ptr(), 3);
    let result = hydro_call(function, 1, [int(255)].as_ptr());
    assert_str(result, "0xff");
}

#[test]
fn hex_bin_oct_reject_non_integers() {
    let _guard = test_guard();
    hydro_runtime_init();
    for function in ["hex", "bin", "oct"] {
        let result = module("builtins", function, &[hydro_float_new(1.0)]);
        assert_type_error(result, "'float' object cannot be interpreted as an integer");

        let result = module("builtins", function, &[text("a")]);
        assert_type_error(result, "'str' object cannot be interpreted as an integer");

        let result = module("builtins", function, &[hydro_none()]);
        assert_type_error(
            result,
            "'NoneType' object cannot be interpreted as an integer",
        );
    }
}

#[test]
fn hex_bin_oct_enforce_exactly_one_argument() {
    let _guard = test_guard();
    hydro_runtime_init();
    for function in ["hex", "bin", "oct"] {
        let expected = format!("{function}() takes exactly 1 argument(s) (0 given)");
        let result = builtin(function, &[]);
        assert_type_error(result, &expected);

        let expected = format!("{function}() takes exactly 1 argument(s) (2 given)");
        let result = builtin(function, &[int(1), int(2)]);
        assert_type_error(result, &expected);
    }
}
