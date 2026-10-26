//! Native `pytest` support: decorator identities, `raises`, `approx`,
//! `skip`/`xfail`, fixture setup/teardown and the test runner.
//!
//! The frontend expands parametrize and fixture injection into generated
//! zero-argument wrappers; this module supplies the runtime hooks those
//! wrappers call and discovers the resulting test functions through the
//! spawn registry (`test_*` entry functions and `Test*.test_*` methods).
//!
//! Generated fixtures return their raw value. A test wrapper passes that
//! value through [`fixture_resolve`]: a generator fixture is advanced once to
//! obtain its fixture value and the generator is resumed after the test for
//! teardown. Module-scoped fixtures go through [`fixture_module`], which
//! caches the resolved value for the whole child run.
//!
//! `pytest.skip()`/`pytest.xfail()` raise the runtime-internal `Skipped` and
//! `XFailed` exceptions (tags 20/21), which the runner reports instead of
//! failing the test.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::builtins;
use crate::exc::{self, ASSERTION_ERROR, RUNTIME_ERROR, SKIPPED, STOP_ITERATION, XFAILED};
use crate::fiber;
use crate::heap;
use crate::iter;
use crate::spawn;
use crate::value::{self, NONE};

/// Classes declared as `Test*` with at least one `test*` method.
static CLASSES: LazyLock<Mutex<Vec<i64>>> = LazyLock::new(|| Mutex::new(Vec::new()));
/// Fibers created by function-scoped `yield` fixtures, in setup order.
static FUNCTION_TEARDOWNS: LazyLock<Mutex<Vec<i64>>> = LazyLock::new(|| Mutex::new(Vec::new()));
/// Fibers created by module-scoped `yield` fixtures.
static MODULE_TEARDOWNS: LazyLock<Mutex<Vec<i64>>> = LazyLock::new(|| Mutex::new(Vec::new()));
/// Module-scoped fixture values, keyed by fixture name.
static FIXTURE_CACHE: LazyLock<Mutex<HashMap<String, i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn classes() -> MutexGuard<'static, Vec<i64>> {
    CLASSES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn function_teardowns() -> MutexGuard<'static, Vec<i64>> {
    FUNCTION_TEARDOWNS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn module_teardowns() -> MutexGuard<'static, Vec<i64>> {
    MODULE_TEARDOWNS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fixture_cache() -> MutexGuard<'static, HashMap<String, i64>> {
    FIXTURE_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Records a `Test*` class created by the runtime. Classes without a `test*`
/// method are ignored, so unrelated `TestSuffix` classes cost nothing.
pub(crate) fn register_class(class: i64) {
    let Some(state) = value::as_class(class) else {
        return;
    };
    if !state.name.starts_with("Test") {
        return;
    }
    if !state
        .methods
        .iter()
        .any(|(name, _, _)| name.starts_with("test"))
    {
        return;
    }
    classes().push(class);
    heap::add_thread_root(class);
}

/// Runtime dispatch for the `pytest` module.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "main" => NONE,
        "raises" => raises(args),
        "approx" => approx(args),
        "fail" => fail(args),
        "skip" => skip(args),
        "xfail" => xfail(args),
        "fixture" => args.first().copied().unwrap_or(NONE),
        "_fixture_resolve" => fixture_resolve(args[0]),
        "_fixture_module" => fixture_module(args),
        "_xfail" => xfail(args),
        "_identity" => identity(args),
        "_raises_enter" => args[0],
        "_raises_exit" => raises_exit(args),
        "_approx_eq" => approx_eq(args),
        _ => NONE,
    }
}

/// Keyword-aware dispatch: fixed parameters were normally folded by the
/// frontend, but calls through a function value arrive with a keyword list.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: Vec<(String, i64)>) -> i64 {
    let mut args = args.to_vec();
    let names: &[&str] = match function {
        "raises" => &["expected", "match"],
        "approx" => &["expected", "rel", "abs"],
        "fail" => &["msg"],
        "skip" | "xfail" => &["reason"],
        _ => &[],
    };
    for (slot, name) in names.iter().enumerate() {
        let Some(position) = keywords.iter().position(|(key, _)| key.as_str() == *name) else {
            continue;
        };
        let (_, value) = keywords[position];
        while args.len() <= slot {
            args.push(NONE);
        }
        args[slot] = value;
    }
    call(function, &args)
}

/// `pytest.mark` namespace: every mark is an identity decorator.
pub(crate) fn marks() -> i64 {
    let class = value::class_new("Mark".to_string(), NONE, Vec::new());
    let object = value::instance_new(class);
    for name in ["skip", "skipif", "xfail", "parametrize", "usefixtures"] {
        let function = value::native_function_new("pytest", "_identity", usize::MAX);
        value::instance_setattr(object, name.to_string(), function);
    }
    object
}

fn identity(args: &[i64]) -> i64 {
    for value in args.iter().rev() {
        if value::as_function(*value).is_some()
            || value::as_class(*value).is_some()
            || value::as_instance(*value).is_some()
        {
            return *value;
        }
    }
    value::native_function_new("pytest", "_identity", usize::MAX)
}

// -------------------------------------------------------------------------
// raises / approx
// -------------------------------------------------------------------------

fn raises(args: &[i64]) -> i64 {
    let expected = args[0];
    let pattern = args.get(1).copied().unwrap_or(NONE);
    let methods = vec![
        (
            "__enter__".to_string(),
            value::native_function_new("pytest", "_raises_enter", 1),
        ),
        (
            "__exit__".to_string(),
            value::native_function_new("pytest", "_raises_exit", 4),
        ),
    ];
    let class = value::class_new("ExceptionInfo".to_string(), NONE, methods);
    let object = value::instance_new(class);
    value::instance_setattr(object, "expected".to_string(), expected);
    value::instance_setattr(object, "match".to_string(), pattern);
    value::instance_setattr(object, "value".to_string(), NONE);
    value::instance_setattr(object, "type".to_string(), NONE);
    object
}

/// `ExceptionInfo.__exit__(self, exc_type, exc_value, tb)`.
fn raises_exit(args: &[i64]) -> i64 {
    let object = args[0];
    let exc_type = args[1];
    let exc_value = args[2];
    let expected = value::instance_attr(object, "expected").unwrap_or(NONE);
    let pattern = value::instance_attr(object, "match").unwrap_or(NONE);
    if exc_value == NONE {
        let name = expected_name(expected);
        return exc::raise_with_message(ASSERTION_ERROR, &format!("DID NOT RAISE {name}"));
    }
    let actual_tag = value::exception_data(exc_value).map(|data| data.tag);
    let expected_tag = exc::exception_tag_of_class(expected);
    let matches_type = match (expected_tag, actual_tag) {
        (Some(expected), Some(actual)) => same_family(expected, actual),
        // A user-defined exception class: the runtime has no class tags, so
        // accept the raised value when its class name matches.
        _ => actual_name(exc_value)
            .map(|name| name == expected_name(expected))
            .unwrap_or(false),
    };
    if !matches_type {
        let got = actual_name(exc_value).unwrap_or_else(|| "exception".to_string());
        return exc::raise_with_message(
            ASSERTION_ERROR,
            &format!("DID NOT RAISE {}, got {}", expected_name(expected), got),
        );
    }
    if pattern != NONE {
        let Some(pattern_text) = value::as_str(pattern) else {
            return exc::raise_with_message(RUNTIME_ERROR, "pytest.raises: match must be a str");
        };
        let message = value::exception_data(exc_value)
            .map(|data| data.msg.clone())
            .unwrap_or_default();
        let matched = regex::Regex::new(pattern_text)
            .map(|regex| regex.is_match(&message))
            .unwrap_or_else(|_| message.contains(pattern_text));
        if !matched {
            return exc::raise_with_message(
                ASSERTION_ERROR,
                &format!("regex {pattern_text:?} does not match {message:?}"),
            );
        }
    }
    value::instance_setattr(object, "value".to_string(), exc_value);
    value::instance_setattr(object, "type".to_string(), exc_type);
    value::TRUE
}

/// Whether a raised exception tag is caught by the expected handler tag.
///
/// The runtime owns the complete CPython parent chain, so this is the same
/// relationship `except` uses: `pytest.raises(ArithmeticError)` accepts a
/// `ZeroDivisionError`, `pytest.raises(ImportError)` accepts a
/// `ModuleNotFoundError`, and so on.
fn same_family(expected: u32, actual: u32) -> bool {
    exc::tag_is_subclass(actual, expected)
}

fn actual_name(exc_value: i64) -> Option<String> {
    value::exception_data(exc_value).map(|data| data.name.to_string())
}

fn expected_name(expected: i64) -> String {
    if let Some(tag) = exc::exception_tag_of_class(expected) {
        return exc::tag_name(tag).to_string();
    }
    value::class_display_name(expected)
        .map(str::to_string)
        .unwrap_or_else(|| value::type_name_of(expected))
}

fn approx(args: &[i64]) -> i64 {
    let expected = args[0];
    let rel = args.get(1).copied().unwrap_or(NONE);
    let abs = args.get(2).copied().unwrap_or(NONE);
    let method = value::native_function_new("pytest", "_approx_eq", 2);
    let class = value::class_new(
        "Approx".to_string(),
        NONE,
        vec![("__eq__".to_string(), method)],
    );
    let object = value::instance_new(class);
    value::instance_setattr(object, "expected".to_string(), expected);
    value::instance_setattr(object, "rel".to_string(), rel);
    value::instance_setattr(object, "abs".to_string(), abs);
    object
}

fn approx_eq(args: &[i64]) -> i64 {
    let object = args[0];
    let other = args[1];
    let expected = value::instance_attr(object, "expected").unwrap_or(NONE);
    let rel = numeric_setting(value::instance_attr(object, "rel").unwrap_or(NONE)).unwrap_or(1e-6);
    let abs = numeric_setting(value::instance_attr(object, "abs").unwrap_or(NONE)).unwrap_or(0.0);
    value::bool_value(approx_equal(expected, other, rel, abs))
}

fn numeric_setting(value: i64) -> Option<f64> {
    if value == NONE {
        return None;
    }
    number(value)
}

fn number(value: i64) -> Option<f64> {
    if let Some(float) = value::as_float(value) {
        return Some(float);
    }
    if let Some(integer) = value::as_integer(value) {
        return Some(integer as f64);
    }
    None
}

fn approx_equal(expected: i64, actual: i64, rel: f64, abs: f64) -> bool {
    if let (Some(left), Some(right)) = (number(expected), number(actual)) {
        let difference = (left - right).abs();
        let tolerance = abs.max(rel * left.abs().max(right.abs()));
        return difference <= tolerance;
    }
    let left = value::as_list(expected).or_else(|| value::as_tuple(expected));
    let right = value::as_list(actual).or_else(|| value::as_tuple(actual));
    if let (Some(left), Some(right)) = (left, right) {
        if left.len() != right.len() {
            return false;
        }
        return left
            .iter()
            .zip(right)
            .all(|(left, right)| approx_equal(*left, *right, rel, abs));
    }
    if let (Some(left), Some(right)) = (value::as_str(expected), value::as_str(actual)) {
        return left == right;
    }
    expected == actual
}

// -------------------------------------------------------------------------
// fail / skip / xfail
// -------------------------------------------------------------------------

fn message_argument(args: &[i64], fallback: &str) -> String {
    match args.first().copied() {
        Some(value) if value != NONE => match value::as_str(value) {
            Some(text) => text.to_string(),
            None => value::repr(value),
        },
        _ => fallback.to_string(),
    }
}

fn fail(args: &[i64]) -> i64 {
    exc::raise_with_message(ASSERTION_ERROR, &message_argument(args, "failed"))
}

fn skip(args: &[i64]) -> i64 {
    exc::raise_with_message(SKIPPED, &message_argument(args, "skipped"))
}

fn xfail(args: &[i64]) -> i64 {
    exc::raise_with_message(XFAILED, &message_argument(args, "xfailed"))
}

// -------------------------------------------------------------------------
// fixtures
// -------------------------------------------------------------------------

/// Advances a `yield` fixture to its setup value and records the generator for
/// teardown. Non-generator fixtures are returned unchanged.
fn fixture_resolve(raw: i64) -> i64 {
    resolve_fixture(raw, true)
}

fn resolve_fixture(raw: i64, function_scope: bool) -> i64 {
    let Some(state) = value::as_fiber(raw) else {
        return raw;
    };
    if state.kind != fiber::FiberKind::Generator {
        return raw;
    }
    let setup = iter::next_value(raw, NONE, false);
    if exc::pending() {
        if let Some(data) = value::exception_data(exc::pending_value()) {
            if data.tag == STOP_ITERATION {
                exc::clear();
                return exc::raise_with_message(RUNTIME_ERROR, "fixture did not yield a value");
            }
        }
        return NONE;
    }
    if function_scope {
        function_teardowns().push(raw);
    } else {
        module_teardowns().push(raw);
    }
    heap::add_thread_root(raw);
    setup
}

/// `pytest._fixture_module(name, thunk)`: resolves a module-scoped fixture at
/// most once per child process.
fn fixture_module(args: &[i64]) -> i64 {
    let Some(name) = value::as_str(args[0]) else {
        return exc::raise_with_message(RUNTIME_ERROR, "fixture name must be a str");
    };
    if let Some(cached) = fixture_cache().get(name).copied() {
        return cached;
    }
    let thunk = args[1];
    let raw = builtins::call(thunk, 0, std::ptr::null());
    if exc::pending() {
        return NONE;
    }
    let resolved = resolve_fixture(raw, false);
    if exc::pending() {
        return NONE;
    }
    fixture_cache().insert(name.to_string(), resolved);
    heap::add_thread_root(resolved);
    resolved
}

/// Resumes every recorded teardown in reverse setup order.
fn run_teardowns(stack: &Mutex<Vec<i64>>) -> Vec<String> {
    let mut values = {
        let mut guard = stack
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *guard)
    };
    let mut errors = Vec::new();
    while let Some(fiber_value) = values.pop() {
        let _ = iter::next_value(fiber_value, NONE, false);
        if exc::pending() {
            let (tag, text) = pending_text();
            exc::clear();
            if tag != STOP_ITERATION {
                errors.push(text);
            }
        }
    }
    errors
}

// -------------------------------------------------------------------------
// Runner
// -------------------------------------------------------------------------

/// Outcome of one test.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Pass,
    Fail,
    Skip,
    Xfail,
    Xpass,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Fail => "fail",
            Outcome::Skip => "skip",
            Outcome::Xfail => "xfail",
            Outcome::Xpass => "xpass",
        }
    }
}

struct TestResult {
    name: String,
    outcome: Outcome,
    message: String,
}

/// Runs every registered test. Returns the pytest exit code: 0 pass, 1
/// failure, 5 no tests collected (2 is reserved for CLI collection errors).
pub fn run(keyword: Option<&str>, maxfail: Option<usize>) -> i32 {
    let mut results: Vec<TestResult> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let maxfail = maxfail.unwrap_or(usize::MAX);

    let names: Vec<(String, usize, usize)> = spawn::registered();
    let mut tests: Vec<(String, usize)> = Vec::new();
    for (name, _, arity) in &names {
        if *arity != 0 {
            continue;
        }
        if is_plain_test(name) {
            tests.push((name.clone(), 0));
        }
    }
    tests.sort_by(|left, right| left.0.cmp(&right.0));

    let class_tests = class_test_names();
    for (name, _function) in tests {
        if let Some(filter) = keyword {
            if !matches_filter(&name, filter) {
                continue;
            }
        }
        let Some((wrapper, 0)) = spawn::lookup(&name) else {
            continue;
        };
        function_teardowns().clear();
        let value = call_wrapper(wrapper);
        let result = classify(&name, value);
        let teardown_errors = run_teardowns(&FUNCTION_TEARDOWNS);
        errors.extend(teardown_errors);
        results.push(result);
        if results
            .iter()
            .filter(|r| r.outcome == Outcome::Fail)
            .count()
            >= maxfail
        {
            break;
        }
    }

    let mut class_failures = 0usize;
    for (name, class, method) in class_tests {
        if let Some(filter) = keyword {
            if !matches_filter(&name, filter) {
                continue;
            }
        }
        function_teardowns().clear();
        let instance = builtins::call(class, 0, std::ptr::null());
        if exc::pending() {
            let (_, text) = pending_text();
            exc::clear();
            results.push(TestResult {
                name: name.clone(),
                outcome: Outcome::Fail,
                message: format!("fixture/init error: {text}"),
            });
            class_failures += 1;
            if results
                .iter()
                .filter(|r| r.outcome == Outcome::Fail)
                .count()
                >= maxfail
            {
                break;
            }
            continue;
        }
        let Some(function) = value::class_method(class, &method) else {
            continue;
        };
        let args = [instance];
        let value = builtins::call(function, args.len(), args.as_ptr());
        let result = classify(&name, value);
        let teardown_errors = run_teardowns(&FUNCTION_TEARDOWNS);
        errors.extend(teardown_errors);
        results.push(result);
        if results
            .iter()
            .filter(|r| r.outcome == Outcome::Fail)
            .count()
            >= maxfail
        {
            break;
        }
    }

    errors.extend(run_teardowns(&MODULE_TEARDOWNS));

    let machine = std::env::var_os("HYDRO_PYTEST_MACHINE").is_some_and(|value| !value.is_empty());
    if machine {
        for result in &results {
            println!(
                "TEST\t{}\t{}\t{}",
                result.outcome.as_str(),
                escape(&result.name),
                escape(&result.message)
            );
        }
        for error in &errors {
            println!("ERROR\t{}", escape(error));
        }
    } else {
        for result in &results {
            match result.outcome {
                Outcome::Pass => println!("PASS {}", result.name),
                Outcome::Fail => println!("FAIL {} - {}", result.name, result.message),
                Outcome::Skip => println!("SKIP {} - {}", result.name, result.message),
                Outcome::Xfail => println!("XFAIL {} - {}", result.name, result.message),
                Outcome::Xpass => println!("XPASS {}", result.name),
            }
        }
        for error in &errors {
            println!("ERROR {error}");
        }
    }

    let failed = results
        .iter()
        .filter(|result| result.outcome == Outcome::Fail)
        .count();
    let passed = results
        .iter()
        .filter(|result| result.outcome == Outcome::Pass)
        .count();
    let skipped = results
        .iter()
        .filter(|result| result.outcome == Outcome::Skip)
        .count();
    let xfailed = results
        .iter()
        .filter(|result| result.outcome == Outcome::Xfail)
        .count();
    let xpassed = results
        .iter()
        .filter(|result| result.outcome == Outcome::Xpass)
        .count();
    if machine {
        println!(
            "SUMMARY\t{passed}\t{failed}\t{skipped}\t{xfailed}\t{xpassed}\t{}",
            errors.len()
        );
    } else {
        println!(
            "\n{passed} passed, {failed} failed, {skipped} skipped, {xfailed} xfailed, {xpassed} xpassed"
        );
        if !errors.is_empty() {
            println!("{} teardown error(s)", errors.len());
        }
    }

    if results.is_empty() && errors.is_empty() {
        return 5;
    }
    if failed > 0 || class_failures > 0 || !errors.is_empty() {
        return 1;
    }
    0
}

fn call_wrapper(wrapper: usize) -> i64 {
    // SAFETY: the registry only holds generated wrappers with the frozen
    // `extern "C" fn(*const i64) -> i64` ABI. This test has arity 0, so no
    // argument buffer is read.
    let function: extern "C" fn(*const i64) -> i64 = unsafe { std::mem::transmute(wrapper) };
    function(std::ptr::null())
}

fn is_plain_test(name: &str) -> bool {
    !name.contains('.') && name.starts_with("test") && !name.ends_with("__hydro_impl")
}

fn class_test_names() -> Vec<(String, i64, String)> {
    let snapshot = classes().clone();
    let mut tests = Vec::new();
    for class in snapshot {
        let Some(state) = value::as_class(class) else {
            continue;
        };
        let class_name = state.name.to_string();
        let mut seen = Vec::new();
        let mut current = class;
        while let Some(state) = value::as_class(current) {
            for (name, _, _) in &state.methods {
                if name.starts_with("test") && !seen.iter().any(|item| item == name) {
                    seen.push(name.clone());
                }
            }
            current = state.base;
        }
        seen.sort();
        for method in seen {
            tests.push((format!("{class_name}.{method}"), class, method));
        }
    }
    tests.sort_by(|left, right| left.0.cmp(&right.0));
    tests
}

fn classify(name: &str, _value: i64) -> TestResult {
    if !exc::pending() {
        return TestResult {
            name: name.to_string(),
            outcome: Outcome::Pass,
            message: String::new(),
        };
    }
    let (tag, message) = pending_text();
    exc::clear();
    let outcome = match tag {
        SKIPPED => Outcome::Skip,
        XFAILED => Outcome::Xfail,
        ASSERTION_ERROR => Outcome::Fail,
        _ => Outcome::Fail,
    };
    TestResult {
        name: name.to_string(),
        outcome,
        message,
    }
}

fn pending_text() -> (u32, String) {
    match value::exception_data(exc::pending_value()) {
        Some(data) if data.msg.is_empty() => (data.tag, data.name.to_string()),
        Some(data) => (data.tag, format!("{}: {}", data.name, data.msg)),
        None => (0, "exception".to_string()),
    }
}

/// Minimal `-k` matcher: substrings combined with `not`, `and` and `or`.
fn matches_filter(name: &str, expression: &str) -> bool {
    let expression = expression.trim();
    if expression.is_empty() {
        return true;
    }
    if let Some(rest) = expression.strip_prefix("not ") {
        return !matches_filter(name, rest);
    }
    if let Some(index) = find_operator(expression, " or ") {
        return matches_filter(name, &expression[..index])
            || matches_filter(name, &expression[index + 4..]);
    }
    if let Some(index) = find_operator(expression, " and ") {
        return matches_filter(name, &expression[..index])
            && matches_filter(name, &expression[index + 5..]);
    }
    let expression = expression.trim_matches(|c| c == '(' || c == ')').trim();
    name.contains(expression)
}

fn find_operator(expression: &str, operator: &str) -> Option<usize> {
    expression.find(operator)
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}
