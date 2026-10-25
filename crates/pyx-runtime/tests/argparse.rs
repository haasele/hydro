//! Integration tests for `argparse` and `SystemExit`.
//!
//! The tests drive the public runtime ABI exactly like generated code would.
//! Parse failures are asserted through the pending `SystemExit` tag and the
//! status returned by `pyx_uncaught()`.

use pyx_runtime::abi::*;

const SYSTEM_EXIT: i64 = 17;

fn int(n: i64) -> i64 {
    // The ABI takes tagged immediates; `pyx_int` treats multiples of 8 as
    // already tagged, so encode explicitly.
    n << 3
}

fn text(value: &str) -> i64 {
    pyx_str_new(value.as_ptr(), value.len())
}

fn list(items: &[i64]) -> i64 {
    pyx_list_new(items.len(), items.as_ptr())
}

fn module(module: &str, function: &str, args: &[i64]) -> i64 {
    pyx_module_call(
        module.as_ptr(),
        module.len(),
        function.as_ptr(),
        function.len(),
        args.len(),
        args.as_ptr(),
    )
}

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    pyx_method(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

fn method_kw(obj: i64, name: &str, args: &[i64], kwargs: &[(&str, i64)]) -> i64 {
    let mut pairs = Vec::with_capacity(kwargs.len() * 2);
    for (key, value) in kwargs {
        pairs.push(text(key));
        pairs.push(*value);
    }
    pyx_method_kw(
        obj,
        name.as_ptr(),
        name.len(),
        args.len(),
        args.as_ptr(),
        kwargs.len(),
        pairs.as_ptr(),
    )
}

fn getattr(obj: i64, name: &str) -> i64 {
    pyx_getattr(obj, name.as_ptr(), name.len())
}

fn assert_no_error() {
    assert_eq!(pyx_exc_pending(), 0, "unexpected pending exception");
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(pyx_to_int(actual), pyx_to_int(int(expected)));
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert_eq!(
        pyx_binop(7, actual, text(expected)),
        pyx_true(),
        "string mismatch, expected {expected:?}"
    );
}

fn assert_true(actual: i64) {
    assert_no_error();
    assert_eq!(actual, pyx_true(), "expected True");
}

fn assert_list(actual: i64, expected: &[i64]) {
    assert_no_error();
    assert_eq!(
        pyx_to_int(pyx_len(actual)),
        pyx_to_int(int(i64::try_from(expected.len()).unwrap()))
    );
    for (index, item) in expected.iter().enumerate() {
        assert_eq!(
            pyx_binop(7, pyx_subscript(actual, int(index as i64)), *item),
            pyx_true(),
            "list element {index} mismatch"
        );
    }
}

fn parser(prog: &str) -> i64 {
    let value = module("argparse", "ArgumentParser", &[text(prog)]);
    assert_no_error();
    value
}

fn add(parser: i64, name: &str, kwargs: &[(&str, i64)]) {
    method_kw(parser, "add_argument", &[text(name)], kwargs);
    assert_no_error();
}

fn parse(parser: i64, argv: &[i64]) -> i64 {
    method(parser, "parse_args", &[list(argv)])
}

/// Asserts that a `SystemExit` is pending and returns its process status.
fn take_exit() -> i64 {
    assert_eq!(pyx_exc_pending(), 1, "expected a pending SystemExit");
    assert_eq!(pyx_exc_tag(), SYSTEM_EXIT, "expected SystemExit");
    pyx_uncaught()
}

// -------------------------------------------------------------------------
// SystemExit
// -------------------------------------------------------------------------

#[test]
fn sys_exit_raises_a_catchable_system_exit() {
    pyx_runtime_init();
    pyx_module_call(
        b"sys".as_ptr(),
        3,
        b"exit".as_ptr(),
        4,
        1,
        [int(5)].as_ptr(),
    );
    assert_eq!(pyx_exc_tag(), SYSTEM_EXIT);
    assert_eq!(pyx_exc_matches(SYSTEM_EXIT as u32), 1);
    assert_eq!(pyx_exc_matches(13), 1, "BaseException catches SystemExit");
    assert_eq!(
        pyx_exc_matches(12),
        0,
        "Exception does not catch SystemExit"
    );
    assert_eq!(pyx_uncaught(), 5);

    // `sys.exit()` and `sys.exit(None)` exit 0.
    pyx_exc_clear();
    pyx_module_call(b"sys".as_ptr(), 3, b"exit".as_ptr(), 4, 0, std::ptr::null());
    assert_eq!(pyx_uncaught(), 0);
    pyx_exc_clear();
    pyx_module_call(
        b"sys".as_ptr(),
        3,
        b"exit".as_ptr(),
        4,
        1,
        [pyx_none()].as_ptr(),
    );
    assert_eq!(pyx_uncaught(), 0);
}

#[test]
fn system_exit_codes_are_masked_and_strings_exit_one() {
    pyx_runtime_init();
    // `raise SystemExit(3)`.
    pyx_raise(SYSTEM_EXIT as u32, int(3));
    assert_eq!(take_exit(), 3);
    pyx_raise(SYSTEM_EXIT as u32, int(256));
    assert_eq!(take_exit(), 0, "exit codes are masked to one byte");
    pyx_raise(SYSTEM_EXIT as u32, int(-1));
    assert_eq!(take_exit(), 255);
    // Bare `raise SystemExit` exits 0.
    pyx_raise(SYSTEM_EXIT as u32, pyx_none());
    assert_eq!(take_exit(), 0);
    // `raise SystemExit("msg")` prints the message and exits 1.
    pyx_raise(SYSTEM_EXIT as u32, text("bad"));
    assert_eq!(take_exit(), 1);
}

#[test]
fn system_exit_can_be_reraised() {
    pyx_runtime_init();
    pyx_module_call(
        b"sys".as_ptr(),
        3,
        b"exit".as_ptr(),
        4,
        1,
        [int(7)].as_ptr(),
    );
    let pending = pyx_exc_value();
    pyx_exc_handler_push(pending);
    pyx_exc_clear();
    pyx_raise_value(pending);
    pyx_exc_handler_pop();
    assert_eq!(pyx_uncaught(), 7);
}

// -------------------------------------------------------------------------
// Scanner and actions
// -------------------------------------------------------------------------

#[test]
fn options_actions_and_inline_values() {
    pyx_runtime_init();
    let parser = parser("demo");
    add(parser, "--name", &[("default", text("anon"))]);
    add(
        parser,
        "-v",
        &[("action", text("count")), ("default", int(0))],
    );
    add(parser, "--tag", &[("action", text("append"))]);
    add(
        parser,
        "--mode",
        &[("choices", list(&[text("a"), text("b")]))],
    );

    let argv = [
        text("--name=pyx"),
        text("-vv"),
        text("--tag"),
        text("x"),
        text("--tag=y"),
        text("--mode"),
        text("b"),
    ];
    let namespace = parse(parser, &argv);
    assert_no_error();
    assert_str(getattr(namespace, "name"), "pyx");
    assert_int(getattr(namespace, "v"), 2);
    assert_list(getattr(namespace, "tag"), &[text("x"), text("y")]);
    assert_str(getattr(namespace, "mode"), "b");

    let defaults = parse(parser, &[]);
    assert_str(getattr(defaults, "name"), "anon");
    assert_int(getattr(defaults, "v"), 0);
}

#[test]
fn nargs_and_positionals() {
    pyx_runtime_init();
    let parser = parser("demo");
    add(parser, "first", &[]);
    method_kw(
        parser,
        "add_argument",
        &[text("rest")],
        &[("nargs", text("*"))],
    );
    assert_no_error();
    method_kw(
        parser,
        "add_argument",
        &[text("--nums")],
        &[
            ("nargs", text("+")),
            ("type", pyx_builtin_function(b"int".as_ptr(), 3)),
        ],
    );
    assert_no_error();

    let argv = [
        text("head"),
        text("a"),
        text("b"),
        text("--nums"),
        text("1"),
        text("2"),
        text("3"),
    ];
    let namespace = parse(parser, &argv);
    assert_no_error();
    assert_str(getattr(namespace, "first"), "head");
    assert_list(getattr(namespace, "rest"), &[text("a"), text("b")]);
    assert_list(getattr(namespace, "nums"), &[int(1), int(2), int(3)]);

    let empty = parse(parser, &[text("only")]);
    assert_no_error();
    assert_list(getattr(empty, "rest"), &[]);
}

#[test]
fn negative_numbers_and_terminator_are_positional() {
    pyx_runtime_init();
    let parser = parser("demo");
    method_kw(
        parser,
        "add_argument",
        &[text("values")],
        &[
            ("nargs", text("+")),
            ("type", pyx_builtin_function(b"int".as_ptr(), 3)),
        ],
    );
    assert_no_error();

    let namespace = parse(parser, &[text("-1"), text("-2"), text("3")]);
    assert_no_error();
    assert_list(getattr(namespace, "values"), &[int(-1), int(-2), int(3)]);

    let terminated = parse(parser, &[text("--"), text("-4")]);
    assert_no_error();
    assert_list(getattr(terminated, "values"), &[int(-4)]);
}

#[test]
fn long_option_abbreviations_are_unique() {
    pyx_runtime_init();
    let parser = parser("demo");
    add(parser, "--verbose", &[("action", text("store_true"))]);
    add(parser, "--value", &[]);
    let namespace = parse(parser, &[text("--verb")]);
    assert_no_error();
    assert_true(getattr(namespace, "verbose"));

    // `--v` is ambiguous between `--verbose` and `--value`.
    parse(parser, &[text("--v")]);
    assert_eq!(take_exit(), 2);
}

#[test]
fn mutually_exclusive_groups_reject_two_options() {
    pyx_runtime_init();
    let parser = parser("demo");
    let group = method(parser, "add_mutually_exclusive_group", &[]);
    assert_no_error();
    method(group, "add_argument", &[text("--foo")]);
    method(group, "add_argument", &[text("--bar")]);
    assert_no_error();

    let namespace = parse(parser, &[text("--foo"), text("1")]);
    assert_no_error();
    assert_str(getattr(namespace, "foo"), "1");

    parse(
        parser,
        &[text("--foo"), text("1"), text("--bar"), text("2")],
    );
    assert_eq!(take_exit(), 2);
}

#[test]
fn required_errors_print_usage_and_exit_two() {
    pyx_runtime_init();
    let p = parser("demo");
    add(p, "--n", &[("required", pyx_true())]);
    parse(p, &[]);
    assert_eq!(take_exit(), 2);

    let p = parser("demo");
    add(p, "--mode", &[("choices", list(&[text("a"), text("b")]))]);
    parse(p, &[text("--mode"), text("c")]);
    assert_eq!(take_exit(), 2);

    let p = parser("demo");
    add(p, "--n", &[]);
    parse(p, &[text("--unknown")]);
    assert_eq!(take_exit(), 2);
}

#[test]
fn help_and_version_exit_zero() {
    pyx_runtime_init();
    let p = parser("demo");
    add(p, "--flag", &[("action", text("store_true"))]);
    let usage = method(p, "format_usage", &[]);
    assert_no_error();
    assert_str(usage, "usage: demo [-h] [--flag]\n");

    parse(p, &[text("-h")]);
    assert_eq!(take_exit(), 0);

    let p = parser("demo");
    method_kw(
        p,
        "add_argument",
        &[text("--version")],
        &[("action", text("version")), ("version", text("demo 1.0"))],
    );
    assert_no_error();
    parse(p, &[text("--version")]);
    assert_eq!(take_exit(), 0);
}

#[test]
fn parse_known_args_returns_extras() {
    pyx_runtime_init();
    let parser = parser("demo");
    add(parser, "--n", &[]);
    let argv = list(&[text("--n"), text("1"), text("--extra")]);
    let result = method(parser, "parse_known_args", &[argv]);
    assert_no_error();
    let namespace = pyx_subscript(result, int(0));
    let extras = pyx_subscript(result, int(1));
    assert_str(getattr(namespace, "n"), "1");
    assert_list(extras, &[text("--extra")]);
}

#[test]
fn set_defaults_and_get_default() {
    pyx_runtime_init();
    let parser = parser("demo");
    add(parser, "--foo", &[]);
    let mut pairs = [text("foo"), int(7)];
    pyx_method_kw(
        parser,
        b"set_defaults".as_ptr(),
        b"set_defaults".len(),
        0,
        std::ptr::null(),
        1,
        pairs.as_mut_ptr(),
    );
    assert_no_error();
    let namespace = parse(parser, &[]);
    assert_int(getattr(namespace, "foo"), 7);
    assert_int(method(parser, "get_default", &[text("foo")]), 7);
}

#[test]
fn subparsers_dispatch_and_expose_dest() {
    pyx_runtime_init();
    let parser = parser("demo");
    let subparsers = method_kw(
        parser,
        "add_subparsers",
        &[],
        &[("dest", text("cmd")), ("required", pyx_true())],
    );
    assert_no_error();
    let child = method(subparsers, "add_parser", &[text("alpha")]);
    assert_no_error();
    method(child, "add_argument", &[text("--x")]);
    assert_no_error();
    let beta = method(subparsers, "add_parser", &[text("beta")]);
    assert_no_error();
    method(beta, "add_argument", &[text("y")]);
    assert_no_error();

    let argv = [text("alpha"), text("--x"), text("9")];
    let namespace = parse(parser, &argv);
    assert_no_error();
    assert_str(getattr(namespace, "cmd"), "alpha");
    assert_str(getattr(namespace, "x"), "9");

    let argv = [text("beta"), text("value")];
    let namespace = parse(parser, &argv);
    assert_no_error();
    assert_str(getattr(namespace, "cmd"), "beta");
    assert_str(getattr(namespace, "y"), "value");

    parse(parser, &[]);
    assert_eq!(take_exit(), 2);
    parse(parser, &[text("missing")]);
    assert_eq!(take_exit(), 2);
}

#[test]
fn namespace_exports_and_explicit_construction() {
    pyx_runtime_init();
    let namespace = pyx_module_call_kw(
        b"argparse".as_ptr(),
        8,
        b"Namespace".as_ptr(),
        9,
        0,
        std::ptr::null(),
        1,
        [text("answer"), int(42)].as_ptr(),
    );
    assert_no_error();
    assert_int(getattr(namespace, "answer"), 42);

    // Exported constants resolve.
    for name in [
        "SUPPRESS",
        "HelpFormatter",
        "RawTextHelpFormatter",
        "RawDescriptionHelpFormatter",
        "ArgumentDefaultsHelpFormatter",
        "MetavarTypeHelpFormatter",
        "Action",
        "BooleanOptionalAction",
    ] {
        let value = pyx_module_get(b"argparse".as_ptr(), 8, name.as_ptr(), name.len());
        assert_no_error();
        assert_ne!(value, pyx_none(), "{name} must not be None");
    }
}

#[test]
fn boolean_optional_and_const_actions() {
    pyx_runtime_init();
    let parser = parser("demo");
    add(
        parser,
        "--feature",
        &[("action", text("BooleanOptionalAction"))],
    );
    add(
        parser,
        "--level",
        &[("action", text("store_const")), ("const", int(3))],
    );

    let namespace = parse(parser, &[text("--no-feature"), text("--level")]);
    assert_no_error();
    assert_eq!(getattr(namespace, "feature"), pyx_false());
    assert_int(getattr(namespace, "level"), 3);

    let namespace = parse(parser, &[text("--feature")]);
    assert_no_error();
    assert_true(getattr(namespace, "feature"));
    assert_eq!(getattr(namespace, "level"), pyx_none());
}
