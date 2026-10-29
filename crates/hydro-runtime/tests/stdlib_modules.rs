//! Integration tests for `argparse`, `collections.defaultdict` and `re`.
//!
//! The tests drive the public runtime ABI exactly like generated code.

use hydro_runtime::abi::*;

const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;
const RUNTIME_ERROR: i64 = 3;
const KEY_ERROR: i64 = 7;

fn int(n: i64) -> i64 {
    // The ABI takes tagged immediates; `hydro_int` treats multiples of 8 as
    // already tagged, so encode explicitly.
    n << 3
}

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn list(items: &[i64]) -> i64 {
    hydro_list_new(items.len(), items.as_ptr())
}

fn tuple(items: &[i64]) -> i64 {
    hydro_tuple_new(items.len(), items.as_ptr())
}

fn dict(keys: &[i64], values: &[i64]) -> i64 {
    hydro_dict_new(keys.len(), keys.as_ptr(), values.as_ptr())
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

fn builtin(name: &str) -> i64 {
    hydro_builtin_function(name.as_ptr(), name.len())
}

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    hydro_method(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

fn method_kw(obj: i64, name: &str, args: &[i64], kwargs: &[(&str, i64)]) -> i64 {
    let mut pairs = Vec::with_capacity(kwargs.len() * 2);
    for (key, value) in kwargs {
        pairs.push(text(key));
        pairs.push(*value);
    }
    hydro_method_kw(
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
    hydro_getattr(obj, name.as_ptr(), name.len())
}

fn module_get(module: &str, name: &str) -> i64 {
    hydro_module_get(module.as_ptr(), module.len(), name.as_ptr(), name.len())
}

fn assert_no_error() {
    assert_eq!(hydro_exc_pending(), 0, "unexpected pending exception");
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(hydro_to_int(actual), hydro_to_int(int(expected)));
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert_eq!(
        hydro_binop(7, actual, text(expected)),
        hydro_true(),
        "string mismatch, expected {expected:?}"
    );
}

fn assert_true(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_true(), "expected True");
}

fn assert_none(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_none(), "expected None");
}

fn same(a: i64, b: i64) -> bool {
    hydro_binop(7, a, b) == hydro_true() // OP_EQ
}

fn expect_error(tag: i64) {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    assert_eq!(hydro_exc_tag(), tag, "wrong exception tag");
    hydro_exc_clear();
}

fn parser() -> i64 {
    let value = module("argparse", "ArgumentParser", &[]);
    assert_no_error();
    // The runtime tests exercise the legacy non-exiting parsing mode; the
    // default path raises `SystemExit` like CPython.
    method(value, "_hydro_test_mode", &[]);
    assert_no_error();
    value
}

// -------------------------------------------------------------------------
// argparse
// -------------------------------------------------------------------------

#[test]
fn argparse_parses_option_with_type_and_default() {
    hydro_runtime_init();
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
    hydro_runtime_init();
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

    let argv = [text("--name=tea"), text("--verbose"), text("file.txt")];
    let namespace = method(parser, "parse_args", &[list(&argv)]);
    assert_no_error();
    assert_str(getattr(namespace, "name"), "tea");
    assert_true(getattr(namespace, "verbose"));
    assert_str(getattr(namespace, "input"), "file.txt");
    assert_str(getattr(namespace, "name"), "tea");
    assert_eq!(
        method(hydro_to_str(namespace), "startswith", &[text("Namespace(")]),
        hydro_true(),
        "unexpected namespace repr"
    );
}

#[test]
fn argparse_reports_unknown_and_missing_arguments() {
    hydro_runtime_init();
    let parser = parser();
    method_kw(
        parser,
        "add_argument",
        &[text("--n")],
        &[("type", builtin("int")), ("required", hydro_true())],
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
    hydro_runtime_init();
    let parser = parser();
    method(parser, "add_argument", &[text("--n")]);
    assert_no_error();
    let argv = [text("--help")];
    let namespace = method(parser, "parse_args", &[list(&argv)]);
    assert_true(getattr(namespace, "_help"));
}

#[test]
fn argparse_choices_are_validated() {
    hydro_runtime_init();
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
    hydro_runtime_init();
    let dd = module("collections", "defaultdict", &[builtin("int")]);
    assert_no_error();

    let key = text("x");
    let current = hydro_subscript(dd, key);
    assert_int(current, 0);
    let incremented = hydro_binop(0, current, int(1)); // OP_ADD
    hydro_subscript_set(dd, key, incremented);
    assert_int(hydro_subscript(dd, key), 1);
    assert_int(hydro_len(dd), 1);
    assert_eq!(hydro_bool(dd), hydro_true());
}

#[test]
fn defaultdict_list_factory_yields_fresh_lists() {
    hydro_runtime_init();
    let dd = module("collections", "defaultdict", &[builtin("list")]);
    assert_no_error();
    let first = hydro_subscript(dd, text("a"));
    let first_is_list = same(hydro_to_str(first), text("[]"));
    assert!(first_is_list);
    let appended = [int(3)];
    hydro_method(first, "append".as_ptr(), 6, 1, appended.as_ptr());
    assert_no_error();
    assert_str(hydro_to_str(hydro_subscript(dd, text("a"))), "[3]");
    assert_str(hydro_to_str(hydro_subscript(dd, text("b"))), "[]");
}

#[test]
fn defaultdict_methods_and_attributes_match_dict() {
    hydro_runtime_init();
    let dd = module("collections", "defaultdict", &[builtin("str")]);
    assert_no_error();
    hydro_subscript_set(dd, text("k"), text("v"));
    assert_str(
        hydro_to_str(getattr(dd, "default_factory")),
        "<function str>",
    );
    let key = [text("k")];
    assert_str(
        hydro_to_str(hydro_method(dd, "get".as_ptr(), 3, 1, key.as_ptr())),
        "v",
    );
    assert_no_error();
    let keys = method(dd, "keys", &[]);
    assert_str(hydro_to_str(keys), "['k']");
    let copied = method(dd, "copy", &[]);
    assert_str(
        hydro_to_str(copied),
        "defaultdict(<class 'str'>, {'k': 'v'})",
    );
    method(dd, "clear", &[]);
    assert_int(hydro_len(dd), 0);
}

#[test]
fn defaultdict_none_factory_raises_key_error() {
    hydro_runtime_init();
    let dd = module("collections", "defaultdict", &[hydro_none()]);
    assert_no_error();
    hydro_subscript(dd, text("missing"));
    expect_error(KEY_ERROR);
}

#[test]
fn defaultdict_accepts_zero_one_and_two_positional_arguments() {
    hydro_runtime_init();

    // Zero arguments: `None` factory and an empty mapping.
    let empty = module("collections", "defaultdict", &[]);
    assert_no_error();
    assert_str(hydro_to_str(getattr(empty, "default_factory")), "None");
    assert_int(hydro_len(empty), 0);

    // One argument: the factory alone.
    let factory_only = module("collections", "defaultdict", &[builtin("list")]);
    assert_no_error();
    assert_str(
        hydro_to_str(getattr(factory_only, "default_factory")),
        "<function list>",
    );
    assert_int(hydro_len(factory_only), 0);

    // Two arguments: factory plus an initially empty mapping.
    let initial = module(
        "collections",
        "defaultdict",
        &[builtin("list"), dict(&[], &[])],
    );
    assert_no_error();
    assert_int(hydro_len(initial), 0);
    assert_str(hydro_to_str(hydro_subscript(initial, text("new"))), "[]");
}

#[test]
fn defaultdict_initial_mapping_preserves_order_and_does_not_use_the_factory() {
    hydro_runtime_init();
    let initial = dict(&[text("a"), text("b")], &[list(&[int(1)]), int(2)]);
    let dd = module("collections", "defaultdict", &[builtin("list"), initial]);
    assert_no_error();
    // Construction copies the entries only; `list` is never called for them.
    assert_str(
        hydro_to_str(dd),
        "defaultdict(<class 'list'>, {'a': [1], 'b': 2})",
    );
    assert_int(hydro_len(dd), 2);
    // Missing keys still use the factory.
    assert_str(hydro_to_str(hydro_subscript(dd, text("c"))), "[]");
    assert_int(hydro_len(dd), 3);
}

#[test]
fn defaultdict_initial_iterable_accepts_pairs_and_last_duplicate_wins() {
    hydro_runtime_init();
    let source = list(&[
        tuple(&[text("a"), int(1)]),
        tuple(&[text("b"), int(2)]),
        tuple(&[text("a"), int(3)]),
    ]);
    let dd = module("collections", "defaultdict", &[builtin("list"), source]);
    assert_no_error();
    // The duplicate key keeps its first position and the last value.
    assert_str(
        hydro_to_str(dd),
        "defaultdict(<class 'list'>, {'a': 3, 'b': 2})",
    );
    assert_int(hydro_len(dd), 2);
    assert_str(hydro_to_str(hydro_subscript(dd, text("missing"))), "[]");
}

#[test]
fn defaultdict_rejects_invalid_initial_values() {
    hydro_runtime_init();

    module("collections", "defaultdict", &[builtin("list"), int(5)]);
    expect_error(TYPE_ERROR);

    let bad_element = list(&[int(1)]);
    module(
        "collections",
        "defaultdict",
        &[builtin("list"), bad_element],
    );
    expect_error(TYPE_ERROR);

    let bad_length = list(&[tuple(&[text("a"), int(1), int(2)])]);
    module("collections", "defaultdict", &[builtin("list"), bad_length]);
    expect_error(VALUE_ERROR);
}

#[test]
fn defaultdict_keywords_are_data_keys() {
    hydro_runtime_init();
    let initial = dict(&[text("a")], &[int(1)]);
    let dd = module_kw(
        "collections",
        "defaultdict",
        &[builtin("list"), initial],
        &[("a", int(2)), ("b", int(3))],
    );
    assert_no_error();
    // Keywords overwrite an initial key in place and append the new one.
    assert_str(
        hydro_to_str(dd),
        "defaultdict(<class 'list'>, {'a': 2, 'b': 3})",
    );
    assert_str(hydro_to_str(hydro_subscript(dd, text("missing"))), "[]");
}

// -------------------------------------------------------------------------
// re
// -------------------------------------------------------------------------

#[test]
fn regex_search_findall_and_sub() {
    hydro_runtime_init();
    let found = module("re", "search", &[text("a+"), text("baa")]);
    assert_true(hydro_bool(found));

    let digits = module("re", "findall", &[text("[0-9]+"), text("a1b22")]);
    assert_str(hydro_to_str(digits), "['1', '22']");

    let replaced = module("re", "sub", &[text("a+"), text("x"), text("baa")]);
    assert_str(replaced, "bx");
}

#[test]
fn regex_module_functions_accept_positional_flags() {
    hydro_runtime_init();
    // `re.search(pattern, string, flags)` — pandas passes `re.I` as the third
    // positional argument; the flag must reach the compiler instead of being
    // ignored.
    let insensitive = module_get("re", "I");
    let found = module("re", "search", &[text("abc"), text("xABCx"), insensitive]);
    assert_true(hydro_bool(found));

    // Without the flag the same pattern does not match.
    let exact = module("re", "search", &[text("abc"), text("xABCx")]);
    assert_none(exact);

    // `match`/`fullmatch`/`findall` share the trailing flags slot.
    let anchored = module("re", "match", &[text("abc"), text("ABCx"), insensitive]);
    assert_true(hydro_bool(anchored));
    let complete = module("re", "fullmatch", &[text("abc"), text("ABC"), insensitive]);
    assert_true(hydro_bool(complete));
    let all = module(
        "re",
        "findall",
        &[text("[a-z]+"), text("AB cd"), insensitive],
    );
    assert_str(hydro_to_str(all), "['AB', 'cd']");
}

#[test]
fn regex_finditer_returns_match_objects_with_spans() {
    hydro_runtime_init();
    let matches = module("re", "finditer", &[text("[0-9]+"), text("a1b22")]);
    assert_no_error();
    assert_int(hydro_len(matches), 2);

    let first = hydro_subscript(matches, int(0));
    assert_str(method(first, "group", &[]), "1");
    assert_int(method(first, "start", &[]), 1);
    assert_int(method(first, "end", &[]), 2);

    let second = hydro_subscript(matches, int(1));
    assert_str(method(second, "group", &[]), "22");
    assert_int(method(second, "start", &[]), 3);
    assert_int(method(second, "end", &[]), 5);

    // A compiled pattern keeps its flags for `finditer` too.
    let pattern = module("re", "compile", &[text("[a-z]+"), int(2)]);
    assert_no_error();
    let words = method(pattern, "finditer", &[text("One TWO")]);
    assert_no_error();
    assert_int(hydro_len(words), 2);
    assert_str(method(hydro_subscript(words, int(0)), "group", &[]), "One");

    // Module-level `finditer` accepts the positional `flags` argument.
    let ignored = module(
        "re",
        "finditer",
        &[text("A"), text("aA"), int(2 /* IGNORECASE */)],
    );
    assert_no_error();
    assert_int(hydro_len(ignored), 2);

    // Non-string input follows the other module functions.
    module("re", "finditer", &[text("a"), int(1)]);
    expect_error(TYPE_ERROR);
}

#[test]
fn regex_match_fullmatch_and_split() {
    hydro_runtime_init();
    let matched = module("re", "match", &[text("a+"), text("aaab")]);
    assert_true(hydro_bool(matched));
    let not_at_start = module("re", "match", &[text("a+"), text("baa")]);
    assert_none(not_at_start);
    let full = module("re", "fullmatch", &[text("a+"), text("aaa")]);
    assert_true(hydro_bool(full));
    let partial = module("re", "fullmatch", &[text("a+"), text("aaab")]);
    assert_none(partial);

    let parts = module("re", "split", &[text("[, ]+"), text("a, b c")]);
    assert_str(hydro_to_str(parts), "['a', 'b', 'c']");
}

#[test]
fn regex_match_methods_report_groups_and_spans() {
    hydro_runtime_init();
    let matched = module("re", "search", &[text("(a)(b+)"), text("xaabbb")]);
    assert_no_error();
    assert_str(method(matched, "group", &[]), "abbb");
    assert_str(method(matched, "group", &[int(1)]), "a");
    assert_str(method(matched, "group", &[int(2)]), "bbb");
    assert_int(method(matched, "start", &[int(2)]), 3);
    assert_int(method(matched, "end", &[int(2)]), 6);
    assert_str(hydro_to_str(method(matched, "span", &[int(2)])), "(3, 6)");
}

#[test]
fn regex_compile_pattern_methods_and_escape() {
    hydro_runtime_init();
    let pattern = module("re", "compile", &[text("[a-z]+")]);
    assert_no_error();
    assert_str(getattr(pattern, "pattern"), "[a-z]+");
    let words = method(pattern, "findall", &[text("one two")]);
    assert_str(hydro_to_str(words), "['one', 'two']");
    let matched = method(pattern, "search", &[text("--one--")]);
    assert_str(method(matched, "group", &[]), "one");

    let escaped = module("re", "escape", &[text("a+b")]);
    assert_str(escaped, "a\\+b");
}

#[test]
fn regex_errors_are_python_exceptions() {
    hydro_runtime_init();
    module("re", "search", &[text("("), text("text")]);
    expect_error(VALUE_ERROR);
    module("re", "search", &[text("a"), int(1)]);
    expect_error(TYPE_ERROR);
}

#[test]
fn regex_flag_constants_have_cpython_values() {
    hydro_runtime_init();
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
    hydro_runtime_init();
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
    hydro_runtime_init();

    let insensitive = module(
        "re",
        "compile",
        &[text("abc"), module_get("re", "IGNORECASE")],
    );
    assert_true(hydro_bool(method(insensitive, "search", &[text("xABCx")])));
    let sensitive = module("re", "compile", &[text("abc")]);
    assert_none(method(sensitive, "search", &[text("xABCx")]));

    let multiline = module(
        "re",
        "compile",
        &[text("^b"), module_get("re", "MULTILINE")],
    );
    assert_true(hydro_bool(method(multiline, "search", &[text("a\nb")])));
    let anchored = module("re", "compile", &[text("^b")]);
    assert_none(method(anchored, "search", &[text("a\nb")]));

    let dotall = module("re", "compile", &[text("a.b"), module_get("re", "DOTALL")]);
    assert_true(hydro_bool(method(dotall, "search", &[text("a\nb")])));
    let line_local = module("re", "compile", &[text("a.b")]);
    assert_none(method(line_local, "search", &[text("a\nb")]));

    // `re.compile` flags must be integers.
    module("re", "compile", &[text("a"), text("x")]);
    expect_error(TYPE_ERROR);
}

// -------------------------------------------------------------------------
// collections.Counter
// -------------------------------------------------------------------------

fn module_kw(module_name: &str, function: &str, args: &[i64], keywords: &[(&str, i64)]) -> i64 {
    let mut pairs = Vec::with_capacity(keywords.len() * 2);
    for (key, value) in keywords {
        pairs.push(text(key));
        pairs.push(*value);
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

/// `Counter({key: count, ...})` through the mapping constructor.
fn counter_items(counts: &[(&str, i64)]) -> i64 {
    let keys: Vec<i64> = counts.iter().map(|(key, _)| text(key)).collect();
    let values: Vec<i64> = counts.iter().map(|(_, count)| int(*count)).collect();
    let source = hydro_dict_new(keys.len(), keys.as_ptr(), values.as_ptr());
    module("collections", "Counter", &[source])
}

#[test]
fn counter_counts_iterables_and_reads_missing_keys_as_zero() {
    hydro_runtime_init();
    let counts = [text("a"), text("b"), text("a")];
    let c = module("collections", "Counter", &[list(&counts)]);
    assert_no_error();
    assert_str(hydro_to_str(c), "Counter({'a': 2, 'b': 1})");
    assert_int(hydro_subscript(c, text("a")), 2);
    assert_int(hydro_subscript(c, text("z")), 0);
    assert_int(hydro_len(c), 2);
    assert_true(hydro_binop(13, text("a"), c)); // OP_IN
    assert_eq!(hydro_binop(14, text("z"), c), hydro_true()); // OP_NOT_IN

    // Instance-defined `__setitem__`/`__delitem__` return None and must not
    // leave a pending exception (the dunder lookup used to miss the method).
    assert_none(hydro_subscript_set(c, text("z"), int(4)));
    assert_int(hydro_subscript(c, text("z")), 4);
    assert_int(hydro_len(c), 3);
    assert_none(hydro_delete_subscript(c, text("z")));
    assert_int(hydro_len(c), 2);
}

#[test]
fn counter_mapping_and_keyword_constructors_add_counts() {
    hydro_runtime_init();
    let counts = counter_items(&[("x", 2), ("y", 1)]);
    assert_none(hydro_subscript_set(counts, text("x"), int(5)));
    assert_int(hydro_subscript(counts, text("x")), 5);

    let keywords = [("a", int(2)), ("b", int(1))];
    let c = module_kw("collections", "Counter", &[], &keywords);
    assert_no_error();
    assert_str(hydro_to_str(c), "Counter({'a': 2, 'b': 1})");

    // A positional iterable and keyword counts combine additively.
    let words = [text("a")];
    let combined = module_kw("collections", "Counter", &[list(&words)], &keywords);
    assert_no_error();
    assert_int(hydro_subscript(combined, text("a")), 3);
}

#[test]
fn counter_update_subtract_elements_and_most_common() {
    hydro_runtime_init();
    let c = counter_items(&[("a", 2), ("b", 1)]);
    assert_no_error();

    let extra = [text("a"), text("c")];
    method(c, "update", &[list(&extra)]);
    assert_no_error();
    assert_str(hydro_to_str(c), "Counter({'a': 3, 'b': 1, 'c': 1})");
    method(c, "subtract", &[list(&[text("b"), text("b")])]);
    assert_no_error();
    assert_int(hydro_subscript(c, text("b")), -1);
    assert_int(method(c, "total", &[]), 3);

    let elements = method(c, "elements", &[]);
    assert_no_error();
    assert_str(
        hydro_to_str(hydro_list_from(elements)),
        "['a', 'a', 'a', 'c']",
    );

    let common = method(c, "most_common", &[]);
    assert_no_error();
    assert_str(hydro_to_str(common), "[('a', 3), ('c', 1), ('b', -1)]");
    let top = method(c, "most_common", &[int(1)]);
    assert_no_error();
    assert_str(hydro_to_str(top), "[('a', 3)]");

    let copied = method(c, "copy", &[]);
    assert_no_error();
    assert_str(hydro_to_str(copied), "Counter({'a': 3, 'b': -1, 'c': 1})");
    method(copied, "clear", &[]);
    assert_no_error();
    assert_str(hydro_to_str(copied), "Counter()");
}

#[test]
fn counter_arithmetic_and_multiset_equality() {
    hydro_runtime_init();
    let left = counter_items(&[("a", 2), ("b", 1)]);
    let right = counter_items(&[("a", 1), ("c", 1)]);
    assert_no_error();

    let sum = hydro_binop(0, left, right); // OP_ADD
    assert_no_error();
    assert_str(hydro_to_str(sum), "Counter({'a': 3, 'b': 1, 'c': 1})");

    let difference = hydro_binop(1, left, right); // OP_SUB
    assert_no_error();
    assert_str(hydro_to_str(difference), "Counter({'a': 1, 'b': 1})");

    assert_eq!(
        hydro_binop(7, left, counter_items(&[("b", 1), ("a", 2)])),
        hydro_true()
    );
    assert_eq!(hydro_binop(7, left, right), hydro_false());
    assert_eq!(hydro_binop(8, left, right), hydro_true());
}

#[test]
fn counter_class_value_reports_dict_subclassing() {
    hydro_runtime_init();
    let class = module_get("collections", "Counter");
    assert_no_error();
    let words = [text("a"), text("a"), text("b")];
    let args = [list(&words)];
    let c = hydro_call(class, args.len(), args.as_ptr());
    assert_no_error();
    assert_str(hydro_to_str(c), "Counter({'a': 2, 'b': 1})");

    let isinstance = builtin("isinstance");
    let dict = builtin("dict");
    let checked = [c, dict];
    assert_true(hydro_call(isinstance, checked.len(), checked.as_ptr()));
}
