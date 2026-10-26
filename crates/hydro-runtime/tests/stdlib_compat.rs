//! Integration tests for the `annotationlib` compatibility surface:
//! `warnings.catch_warnings`/`_deprecated`, the `typing` helpers and the
//! runtime `ast` node/parse surface.
//!
//! Every test drives the public runtime ABI like generated code does.

use std::sync::{LazyLock, Mutex, MutexGuard};

use hydro_runtime::abi::*;

const TYPE_ERROR: i64 = 1;
const NAME_ERROR: i64 = 16;
const SYNTAX_ERROR: i64 = 25;
const NOT_IMPLEMENTED_ERROR: i64 = 32;

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

fn assert_none(actual: i64) {
    assert_no_error();
    assert_eq!(actual, hydro_none(), "expected None");
}

fn expect_error(tag: i64) {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    assert_eq!(hydro_exc_tag(), tag, "wrong exception tag");
    hydro_exc_clear();
}

fn length(value: i64) -> i64 {
    hydro_to_int(hydro_len(value)) >> 3
}

/// A recording `catch_warnings` manager entered on the current thread.
fn recording() -> i64 {
    let manager = module_kw(
        "warnings",
        "catch_warnings",
        &[],
        &[("record", hydro_true())],
    );
    assert_no_error();
    let log = method(manager, "__enter__", &[]);
    assert_no_error();
    log
}

fn exit_manager(manager: i64) {
    let result = method(
        manager,
        "__exit__",
        &[hydro_none(), hydro_none(), hydro_none()],
    );
    assert_no_error();
    assert_eq!(result, hydro_false(), "catch_warnings must not suppress");
}

// ---------------------------------------------------------------------------
// warnings
// ---------------------------------------------------------------------------

#[test]
fn catch_warnings_records_warnings_and_restores_filters() {
    let _guard = setup();
    let filters_before = module_get("warnings", "filters");
    let before = length(filters_before);

    let manager = module_kw(
        "warnings",
        "catch_warnings",
        &[],
        &[("record", hydro_true())],
    );
    assert_no_error();
    let log = method(manager, "__enter__", &[]);
    assert_no_error();
    assert_eq!(length(log), 0, "recording list starts empty");

    module("warnings", "warn", &[text("first warning")]);
    assert_no_error();
    assert_eq!(length(log), 1, "warn() is recorded");
    let record = hydro_subscript(log, int(0));
    let message = getattr(record, "message");
    assert_str(hydro_to_str(message), "first warning");
    let category = getattr(record, "category");
    assert_str(hydro_to_str(category), "<class 'UserWarning'>");
    assert_str(getattr(record, "filename"), "<unknown>");

    exit_manager(manager);
    let filters_after = module_get("warnings", "filters");
    assert_eq!(
        length(filters_after),
        before,
        "the filter stack is restored on exit"
    );
}

#[test]
fn catch_warnings_nests_and_returns_none_without_record() {
    let _guard = setup();

    let outer = module_kw(
        "warnings",
        "catch_warnings",
        &[],
        &[("record", hydro_true())],
    );
    let outer_log = method(outer, "__enter__", &[]);
    assert_no_error();
    module("warnings", "warn", &[text("outer")]);
    assert_no_error();
    assert_eq!(length(outer_log), 1);

    let inner = module_kw(
        "warnings",
        "catch_warnings",
        &[],
        &[("record", hydro_true())],
    );
    let inner_log = method(inner, "__enter__", &[]);
    assert_no_error();
    module("warnings", "warn", &[text("inner")]);
    assert_no_error();
    assert_eq!(length(inner_log), 1);
    assert_eq!(length(outer_log), 1, "the outer log is not shared");
    exit_manager(inner);
    assert_eq!(
        length(outer_log),
        1,
        "exiting the inner block keeps the outer recording target"
    );

    // After the inner block the outer filter state is active again.
    module("warnings", "warn", &[text("outer again")]);
    assert_no_error();
    assert_eq!(length(outer_log), 2);
    exit_manager(outer);

    let plain = module("warnings", "catch_warnings", &[]);
    let entered = method(plain, "__enter__", &[]);
    assert_none(entered);
    // Installing a filter inside the block is undone on exit.
    module(
        "warnings",
        "simplefilter",
        &[text("ignore"), hydro_none(), int(0)],
    );
    assert_no_error();
    exit_manager(plain);
}

#[test]
fn deprecated_emits_deprecation_and_pending_warnings() {
    let _guard = setup();
    let log = recording();

    module_kw(
        "warnings",
        "_deprecated",
        &[
            text("ForwardRef._evaluate"),
            text("{name} is a private API and will be removed in Python {remove}"),
        ],
        &[("remove", tuple(&[int(3), int(16)]))],
    );
    assert_no_error();
    assert_eq!(length(log), 1, "3.14 spelling records one warning");
    let message = hydro_to_str(getattr(hydro_subscript(log, int(0)), "message"));
    assert_str(
        message,
        "ForwardRef._evaluate is a private API and will be removed in Python 3.16",
    );

    module_kw(
        "warnings",
        "_deprecated",
        &[text("{name} is deprecated since Python {since}")],
        &[
            ("name", text("old_api")),
            ("since", tuple(&[int(3), int(12)])),
            ("pending", hydro_true()),
        ],
    );
    assert_no_error();
    assert_eq!(length(log), 2, "pending spelling records one warning");
    let pending = hydro_subscript(log, int(1));
    assert_str(
        hydro_to_str(getattr(pending, "category")),
        "<class 'PendingDeprecationWarning'>",
    );

    // Required arguments are validated, not defaulted.
    module("warnings", "_deprecated", &[]);
    expect_error(TYPE_ERROR);
}

// ---------------------------------------------------------------------------
// typing
// ---------------------------------------------------------------------------

#[test]
fn typing_deprecation_helper_emits_the_cpython_warning() {
    let _guard = setup();
    let log = recording();

    module(
        "typing",
        "_deprecation_warning_for_no_type_params_passed",
        &[text("typing._eval_type")],
    );
    assert_no_error();
    assert_eq!(length(log), 1);
    let message = getattr(hydro_subscript(log, int(0)), "message");
    assert_eq!(
        method(
            hydro_to_str(message),
            "startswith",
            &[text("Failing to pass a value")],
        ),
        hydro_true()
    );

    module(
        "typing",
        "_deprecation_warning_for_no_type_params_passed",
        &[],
    );
    expect_error(TYPE_ERROR);
}

#[test]
fn typing_evaluate_forward_ref_resolves_plain_names_only() {
    let _guard = setup();

    // Missing / non-ForwardRef arguments are call errors.
    module("typing", "evaluate_forward_ref", &[]);
    expect_error(TYPE_ERROR);
    module("typing", "evaluate_forward_ref", &[hydro_none()]);
    expect_error(TYPE_ERROR);

    // A simple name resolves through `globals`.
    let reference = module("typing", "ForwardRef", &[text("Hero")]);
    assert_no_error();
    let globals = hydro_dict_new(1, [text("Hero")].as_ptr(), [int(7)].as_ptr());
    let resolved = module_kw(
        "typing",
        "evaluate_forward_ref",
        &[reference],
        &[("globals", globals)],
    );
    assert_int(resolved, 7);

    // Non-name expressions report a tagged NotImplementedError at call time.
    let expression = module("typing", "ForwardRef", &[text("a + b")]);
    assert_no_error();
    module("typing", "evaluate_forward_ref", &[expression]);
    expect_error(NOT_IMPLEMENTED_ERROR);

    // Unknown simple names raise NameError like CPython.
    let unknown = module("typing", "ForwardRef", &[text("Missing")]);
    assert_no_error();
    module("typing", "evaluate_forward_ref", &[unknown]);
    expect_error(NAME_ERROR);
}

// ---------------------------------------------------------------------------
// ast
// ---------------------------------------------------------------------------

#[test]
fn ast_parse_and_unparse_round_trip_expressions() {
    let _guard = setup();

    let tree = module_kw(
        "ast",
        "parse",
        &[text("int | None")],
        &[("mode", text("eval"))],
    );
    assert_no_error();
    assert_str(module("ast", "unparse", &[tree]), "int | None");

    let body = getattr(tree, "body");
    assert_str(module("ast", "unparse", &[body]), "int | None");

    let module_tree = module("ast", "parse", &[text("1 + 2")]);
    assert_no_error();
    assert_str(module("ast", "unparse", &[module_tree]), "1 + 2");
    let statements = getattr(module_tree, "body");
    assert_eq!(length(statements), 1);
    let statement = hydro_subscript(statements, int(0));
    assert_str(
        module("ast", "unparse", &[getattr(statement, "value")]),
        "1 + 2",
    );
}

#[test]
fn ast_unparse_reconstructs_subscripts_and_constructed_nodes() {
    let _guard = setup();

    let tree = module_kw("ast", "parse", &[text("a.b[c]")], &[("mode", text("eval"))]);
    assert_no_error();
    let body = getattr(tree, "body");
    assert_str(module("ast", "unparse", &[body]), "a.b[c]");

    // Node constructors accept keyword fields.
    let name = module_kw("ast", "Name", &[], &[("id", text("x"))]);
    assert_no_error();
    assert_str(module("ast", "unparse", &[name]), "x");

    // The node class value supports indirect construction, as used by
    // `annotationlib._Stringifier`.
    let list_class = module_get("ast", "List");
    assert_no_error();
    let one = module_kw("ast", "Constant", &[], &[("value", int(1))]);
    let two = module_kw("ast", "Constant", &[], &[("value", int(2))]);
    let constructed = call(list_class, &[list(&[one, two])]);
    assert_no_error();
    assert_str(module("ast", "unparse", &[constructed]), "[1, 2]");

    // `ast.AST` is a class value, so isinstance-style checks see nodes.
    let ast_class = module_get("ast", "AST");
    assert_no_error();
    let isinstance = hydro_builtin_function("isinstance".as_ptr(), "isinstance".len());
    assert_eq!(
        call(isinstance, &[name, ast_class]),
        hydro_true(),
        "constructed nodes are AST instances"
    );
}

#[test]
fn ast_parse_reports_syntax_errors_and_unsupported_statements() {
    let _guard = setup();

    module_kw("ast", "parse", &[text("(")], &[("mode", text("eval"))]);
    expect_error(SYNTAX_ERROR);

    module("ast", "parse", &[text("def f():\n    pass\n")]);
    expect_error(NOT_IMPLEMENTED_ERROR);
}
