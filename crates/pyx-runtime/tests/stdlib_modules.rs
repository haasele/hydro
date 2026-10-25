//! Integration tests for `argparse`, `collections.defaultdict` and `re`.
//!
//! The tests drive the public runtime ABI exactly like generated code.

use pyx_runtime::abi::*;

const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;
const RUNTIME_ERROR: i64 = 3;
const KEY_ERROR: i64 = 7;

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

fn builtin(name: &str) -> i64 {
    pyx_builtin_function(name.as_ptr(), name.len())
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

fn module_get(module: &str, name: &str) -> i64 {
    pyx_module_get(module.as_ptr(), module.len(), name.as_ptr(), name.len())
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

fn assert_none(actual: i64) {
    assert_no_error();
    assert_eq!(actual, pyx_none(), "expected None");
}

fn same(a: i64, b: i64) -> bool {
    pyx_binop(7, a, b) == pyx_true() // OP_EQ
}

fn expect_error(tag: i64) {
    assert_eq!(pyx_exc_pending(), 1, "expected a pending exception");
    assert_eq!(pyx_exc_tag(), tag, "wrong exception tag");
    pyx_exc_clear();
}

fn parser() -> i64 {
    let value = module("argparse", "ArgumentParser", &[]);
    assert_no_error();
    value
}

// -------------------------------------------------------------------------
// argparse
// -------------------------------------------------------------------------

#[test]
fn argparse_parses_option_with_type_and_default() {
    pyx_runtime_init();
    let parser = parser();
    let argc = [text("--n")];
    let kwargs = [("type", builtin("int")), ("default", int(1))];
    method_kw(parser, "add_argument", &argc, &kwargs);
    assert_no_error();

    let argv = [text("--n"), text("5")];
    let namespace = method(parser, "parse_args", &[list(&argv)]);
    assert_no_error();
    assert_int(getattr(namespace, "n"), 5);

    let empty = method(parser, "parse_args", &[list(&[])]);
    assert_int(getattr(empty, "n"), 1);
}

#[test]
fn argparse_supports_inline_values_store_true_and_positionals() {
    pyx_runtime_init();
    let parser = parser();
    method(parser, "add_argument", &[text("--name")]);
    method_kw(
        parser,
        "add_argument",
        &[text("--verbose")],
        &[("action", text("store_true"))],
    );
    method(parser, "add_argument", &[text("input")]);
    assert_no_error();

    let argv = [text("--name=pyx"), text("--verbose"), text("file.txt")];
    let namespace = method(parser, "parse_args", &[list(&argv)]);
    assert_no_error();
    assert_str(getattr(namespace, "name"), "pyx");
    assert_true(getattr(namespace, "verbose"));
    assert_str(getattr(namespace, "input"), "file.txt");
    assert_str(getattr(namespace, "name"), "pyx");
    assert_eq!(
        method(pyx_to_str(namespace), "startswith", &[text("Namespace(")]),
        pyx_true(),
        "unexpected namespace repr"
    );
}

#[test]
fn argparse_reports_unknown_and_missing_arguments() {
    pyx_runtime_init();
    let parser = parser();
    method_kw(
        parser,
        "add_argument",
        &[text("--n")],
        &[("type", builtin("int")), ("required", pyx_true())],
    );
    assert_no_error();

    method(parser, "parse_args", &[list(&[])]);
    expect_error(RUNTIME_ERROR);

    let argv = [text("--unknown")];
    method(parser, "parse_args", &[list(&argv)]);
    expect_error(RUNTIME_ERROR);
}

#[test]
fn argparse_help_returns_flagged_namespace() {
    pyx_runtime_init();
    let parser = parser();
    method(parser, "add_argument", &[text("--n")]);
    assert_no_error();
    let argv = [text("--help")];
    let namespace = method(parser, "parse_args", &[list(&argv)]);
    assert_true(getattr(namespace, "_help"));
}

#[test]
fn argparse_choices_are_validated() {
    pyx_runtime_init();
    let parser = parser();
    let choices = [text("a"), text("b")];
    method_kw(
        parser,
        "add_argument",
        &[text("--mode")],
        &[("choices", list(&choices))],
    );
    assert_no_error();

    let good = [text("--mode"), text("b")];
    let namespace = method(parser, "parse_args", &[list(&good)]);
    assert_str(getattr(namespace, "mode"), "b");

    let bad = [text("--mode"), text("c")];
    method(parser, "parse_args", &[list(&bad)]);
    expect_error(VALUE_ERROR);
}

// -------------------------------------------------------------------------
// collections.defaultdict
// -------------------------------------------------------------------------

#[test]
fn defaultdict_creates_int_default_and_supports_augmented_assignment() {
    pyx_runtime_init();
    let dd = module("collections", "defaultdict", &[builtin("int")]);
    assert_no_error();

    let key = text("x");
    let current = pyx_subscript(dd, key);
    assert_int(current, 0);
    let incremented = pyx_binop(0, current, int(1)); // OP_ADD
    pyx_subscript_set(dd, key, incremented);
    assert_int(pyx_subscript(dd, key), 1);
    assert_int(pyx_len(dd), 1);
    assert_eq!(pyx_bool(dd), pyx_true());
}

#[test]
fn defaultdict_list_factory_yields_fresh_lists() {
    pyx_runtime_init();
    let dd = module("collections", "defaultdict", &[builtin("list")]);
    assert_no_error();
    let first = pyx_subscript(dd, text("a"));
    let first_is_list = same(pyx_to_str(first), text("[]"));
    assert!(first_is_list);
    let appended = [int(3)];
    pyx_method(first, "append".as_ptr(), 6, 1, appended.as_ptr());
    assert_no_error();
    assert_str(pyx_to_str(pyx_subscript(dd, text("a"))), "[3]");
    assert_str(pyx_to_str(pyx_subscript(dd, text("b"))), "[]");
}

#[test]
fn defaultdict_methods_and_attributes_match_dict() {
    pyx_runtime_init();
    let dd = module("collections", "defaultdict", &[builtin("str")]);
    assert_no_error();
    pyx_subscript_set(dd, text("k"), text("v"));
    assert_str(pyx_to_str(getattr(dd, "default_factory")), "<function str>");
    let key = [text("k")];
    assert_str(
        pyx_to_str(pyx_method(dd, "get".as_ptr(), 3, 1, key.as_ptr())),
        "v",
    );
    assert_no_error();
    let keys = method(dd, "keys", &[]);
    assert_str(pyx_to_str(keys), "['k']");
    let copied = method(dd, "copy", &[]);
    assert_str(pyx_to_str(copied), "defaultdict(<class 'str'>, {'k': 'v'})");
    method(dd, "clear", &[]);
    assert_int(pyx_len(dd), 0);
}

#[test]
fn defaultdict_none_factory_raises_key_error() {
    pyx_runtime_init();
    let dd = module("collections", "defaultdict", &[pyx_none()]);
    assert_no_error();
    pyx_subscript(dd, text("missing"));
    expect_error(KEY_ERROR);
}

// -------------------------------------------------------------------------
// re
// -------------------------------------------------------------------------

#[test]
fn regex_search_findall_and_sub() {
    pyx_runtime_init();
    let found = module("re", "search", &[text("a+"), text("baa")]);
    assert_true(pyx_bool(found));

    let digits = module("re", "findall", &[text("[0-9]+"), text("a1b22")]);
    assert_str(pyx_to_str(digits), "['1', '22']");

    let replaced = module("re", "sub", &[text("a+"), text("x"), text("baa")]);
    assert_str(replaced, "bx");
}

#[test]
fn regex_finditer_returns_match_objects_with_spans() {
    pyx_runtime_init();
    let matches = module("re", "finditer", &[text("[0-9]+"), text("a1b22")]);
    assert_no_error();
    assert_int(pyx_len(matches), 2);

    let first = pyx_subscript(matches, int(0));
    assert_str(method(first, "group", &[]), "1");
    assert_int(method(first, "start", &[]), 1);
    assert_int(method(first, "end", &[]), 2);

    let second = pyx_subscript(matches, int(1));
    assert_str(method(second, "group", &[]), "22");
    assert_int(method(second, "start", &[]), 3);
    assert_int(method(second, "end", &[]), 5);

    // A compiled pattern keeps its flags for `finditer` too.
    let pattern = module("re", "compile", &[text("[a-z]+"), int(2)]);
    assert_no_error();
    let words = method(pattern, "finditer", &[text("One TWO")]);
    assert_no_error();
    assert_int(pyx_len(words), 2);
    assert_str(method(pyx_subscript(words, int(0)), "group", &[]), "One");

    // Module-level `finditer` accepts the positional `flags` argument.
    let ignored = module(
        "re",
        "finditer",
        &[text("A"), text("aA"), int(2 /* IGNORECASE */)],
    );
    assert_no_error();
    assert_int(pyx_len(ignored), 2);

    // Non-string input follows the other module functions.
    module("re", "finditer", &[text("a"), int(1)]);
    expect_error(TYPE_ERROR);
}

#[test]
fn regex_match_fullmatch_and_split() {
    pyx_runtime_init();
    let matched = module("re", "match", &[text("a+"), text("aaab")]);
    assert_true(pyx_bool(matched));
    let not_at_start = module("re", "match", &[text("a+"), text("baa")]);
    assert_none(not_at_start);
    let full = module("re", "fullmatch", &[text("a+"), text("aaa")]);
    assert_true(pyx_bool(full));
    let partial = module("re", "fullmatch", &[text("a+"), text("aaab")]);
    assert_none(partial);

    let parts = module("re", "split", &[text("[, ]+"), text("a, b c")]);
    assert_str(pyx_to_str(parts), "['a', 'b', 'c']");
}

#[test]
fn regex_match_methods_report_groups_and_spans() {
    pyx_runtime_init();
    let matched = module("re", "search", &[text("(a)(b+)"), text("xaabbb")]);
    assert_no_error();
    assert_str(method(matched, "group", &[]), "abbb");
    assert_str(method(matched, "group", &[int(1)]), "a");
    assert_str(method(matched, "group", &[int(2)]), "bbb");
    assert_int(method(matched, "start", &[int(2)]), 3);
    assert_int(method(matched, "end", &[int(2)]), 6);
    assert_str(pyx_to_str(method(matched, "span", &[int(2)])), "(3, 6)");
}

#[test]
fn regex_compile_pattern_methods_and_escape() {
    pyx_runtime_init();
    let pattern = module("re", "compile", &[text("[a-z]+")]);
    assert_no_error();
    assert_str(getattr(pattern, "pattern"), "[a-z]+");
    let words = method(pattern, "findall", &[text("one two")]);
    assert_str(pyx_to_str(words), "['one', 'two']");
    let matched = method(pattern, "search", &[text("--one--")]);
    assert_str(method(matched, "group", &[]), "one");

    let escaped = module("re", "escape", &[text("a+b")]);
    assert_str(escaped, "a\\+b");
}

#[test]
fn regex_errors_are_python_exceptions() {
    pyx_runtime_init();
    module("re", "search", &[text("("), text("text")]);
    expect_error(VALUE_ERROR);
    module("re", "search", &[text("a"), int(1)]);
    expect_error(TYPE_ERROR);
}

#[test]
fn regex_flag_constants_have_cpython_values() {
    pyx_runtime_init();
    assert_eq!(module_get("re", "NOFLAG"), int(0));
    assert_eq!(module_get("re", "TEMPLATE"), int(1));
    assert_eq!(module_get("re", "T"), int(1));
    assert_eq!(module_get("re", "IGNORECASE"), int(2));
    assert_eq!(module_get("re", "I"), int(2));
    assert_eq!(module_get("re", "LOCALE"), int(4));
    assert_eq!(module_get("re", "MULTILINE"), int(8));
    assert_eq!(module_get("re", "M"), int(8));
    assert_eq!(module_get("re", "DOTALL"), int(16));
    assert_eq!(module_get("re", "S"), int(16));
    assert_eq!(module_get("re", "UNICODE"), int(32));
    assert_eq!(module_get("re", "VERBOSE"), int(64));
    assert_eq!(module_get("re", "X"), int(64));
    assert_eq!(module_get("re", "DEBUG"), int(128));
    assert_eq!(module_get("re", "ASCII"), int(256));
    assert_eq!(module_get("re", "A"), int(256));
    assert_no_error();
}

#[test]
fn regex_compile_verbose_ignores_whitespace_and_comments() {
    pyx_runtime_init();
    // The `rich.color` colour pattern: it is only readable because
    // `re.VERBOSE` drops the newlines, and `\#` still matches a literal `#`.
    let source = "^
\\#([0-9a-f]{6})$|
color\\(([0-9]{1,3})\\)$|
rgb\\(([\\d\\s,]+)\\)$
";
    let pattern = module(
        "re",
        "compile",
        &[text(source), module_get("re", "VERBOSE")],
    );
    assert_no_error();
    assert_int(getattr(pattern, "flags"), 64);

    let hex = method(pattern, "search", &[text("#a1b2c3")]);
    assert_str(method(hex, "group", &[int(1)]), "a1b2c3");

    let indexed = method(pattern, "search", &[text("color(42)")]);
    assert_str(method(indexed, "group", &[int(2)]), "42");

    let rgb = method(pattern, "search", &[text("rgb(1, 22, 200)")]);
    assert_str(method(rgb, "group", &[int(3)]), "1, 22, 200");

    // Without the flag the newlines are literal, so nothing matches.
    let plain = module("re", "compile", &[text(source)]);
    assert_none(method(plain, "search", &[text("#a1b2c3")]));
}

#[test]
fn regex_compile_honours_case_multiline_and_dotall_flags() {
    pyx_runtime_init();

    let insensitive = module(
        "re",
        "compile",
        &[text("abc"), module_get("re", "IGNORECASE")],
    );
    assert_true(pyx_bool(method(insensitive, "search", &[text("xABCx")])));
    let sensitive = module("re", "compile", &[text("abc")]);
    assert_none(method(sensitive, "search", &[text("xABCx")]));

    let multiline = module(
        "re",
        "compile",
        &[text("^b"), module_get("re", "MULTILINE")],
    );
    assert_true(pyx_bool(method(multiline, "search", &[text("a\nb")])));
    let anchored = module("re", "compile", &[text("^b")]);
    assert_none(method(anchored, "search", &[text("a\nb")]));

    let dotall = module("re", "compile", &[text("a.b"), module_get("re", "DOTALL")]);
    assert_true(pyx_bool(method(dotall, "search", &[text("a\nb")])));
    let line_local = module("re", "compile", &[text("a.b")]);
    assert_none(method(line_local, "search", &[text("a\nb")]));

    // `re.compile` flags must be integers.
    module("re", "compile", &[text("a"), text("x")]);
    expect_error(TYPE_ERROR);
}
