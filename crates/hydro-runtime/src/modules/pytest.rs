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
//!
//! A test parameter that is neither a parametrized value nor a fixture from
//! the module/`conftest.py` table is resolved at run time through
//! [`fixture_builtin`], which installs pytest's builtin fixtures (`request`,
//! `tmp_path`, `monkeypatch`, `capsys`, `caplog`, `recwarn`) and records their
//! finalizers on the same stacks as generator fixtures. Any other name
//! reports a `LookupError` naming the supported set. A test whose parameters
//! are all ordinary arguments is a helper or factory, not a collected test;
//! the frontend leaves such functions unwrapped and the runner never calls
//! them because only zero-argument registrations are collected.

use std::collections::HashMap;
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::builtins;
use crate::container;
use crate::core::globals;
use crate::exc::{
    self, ASSERTION_ERROR, ATTRIBUTE_ERROR, KEY_ERROR, LOOKUP_ERROR, NOT_IMPLEMENTED_ERROR,
    OS_ERROR, RUNTIME_ERROR, SKIPPED, STOP_ITERATION, TYPE_ERROR, XFAILED,
};
use crate::fiber;
use crate::heap;
use crate::iter;
use crate::methods;
use crate::ops;
use crate::spawn;
use crate::value::{self, FALSE, NONE, TRUE};

/// Classes declared as `Test*` with at least one `test*` method.
static CLASSES: LazyLock<Mutex<Vec<i64>>> = LazyLock::new(|| Mutex::new(Vec::new()));
/// One recorded fixture finalizer.
///
/// The value is one of:
/// - a `yield` fixture's generator: advanced once more for teardown;
/// - a `pathlib.Path` temporary directory created by `tmp_path`: removed
///   recursively;
/// - a zero-argument callable (a bound method or `functools.partial`):
///   invoked.
struct Finalizer {
    value: i64,
    /// Persistent-root slot pinning `value` while it only lives in this
    /// Rust-side stack; `usize::MAX` outside the runtime.
    root: usize,
}

/// Finalizers for the running test, in setup order.
static FUNCTION_FINALIZERS: LazyLock<Mutex<Vec<Finalizer>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
/// Finalizers for module-scoped fixtures, in setup order.
static MODULE_FINALIZERS: LazyLock<Mutex<Vec<Finalizer>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
/// Module-scoped fixture values, keyed by fixture name.
static FIXTURE_CACHE: LazyLock<Mutex<HashMap<String, i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn classes() -> MutexGuard<'static, Vec<i64>> {
    CLASSES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn finalizers(
    stack: &'static LazyLock<Mutex<Vec<Finalizer>>>,
) -> MutexGuard<'static, Vec<Finalizer>> {
    stack
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Records `value` as a finalizer of the running test (`module_scope` false)
/// or of the whole module run, pinning it until it runs.
fn schedule_finalizer(value: i64, module_scope: bool) {
    if value == NONE {
        return;
    }
    let root = heap::add_persistent_root(value);
    let stack = if module_scope {
        &MODULE_FINALIZERS
    } else {
        &FUNCTION_FINALIZERS
    };
    finalizers(stack).push(Finalizer { value, root });
}

/// Drops finalizers an aborted test left behind without running them.
fn discard_function_finalizers() {
    let mut stack = finalizers(&FUNCTION_FINALIZERS);
    for finalizer in stack.drain(..) {
        if finalizer.root != usize::MAX {
            heap::remove_persistent_root(finalizer.root);
        }
    }
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
        "main" => main(args),
        "raises" => raises(args),
        "approx" => approx(args),
        "fail" => fail(args),
        "skip" => skip(args),
        "xfail" => xfail(args),
        "fixture" => args.first().copied().unwrap_or(NONE),
        "_fixture_resolve" => fixture_resolve(args[0]),
        "_fixture_builtin" => fixture_builtin(args),
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

/// `pytest.main()`: refuse with a precise `NotImplementedError`.
///
/// The runtime has no pytest session to run: test discovery, plugin handling
/// and report writing belong to the external Hydro test runner, which invokes
/// [`run`] itself and owns the exit code. Pretending the call succeeded would
/// hide the fact that no tests were collected, so the argument list is
/// reported and the call fails at run time.
fn main(args: &[i64]) -> i64 {
    let supplied = if args.is_empty() {
        String::from("no arguments")
    } else {
        format!("{} argument(s)", args.len())
    };
    exc::raise_with_message(
        NOT_IMPLEMENTED_ERROR,
        &format!(
            "pytest.main() is not implemented in Hydro (called with {supplied}): the runtime \
             does not start a pytest session, because test collection, plugins and reporting \
             are owned by the external Hydro test runner. Run the test file with the Hydro \
             runner instead of calling pytest.main()."
        ),
    )
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
    schedule_finalizer(raw, !function_scope);
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

/// Runs every recorded finalizer in reverse setup order.
///
/// Generator fixtures are resumed, `tmp_path` directories are removed and
/// callables are invoked. A failing finalizer does not stop the remaining
/// ones (a `yield` fixture's cleanup must still run after an earlier failure);
/// every error is collected so the runner can report it.
fn run_teardowns(stack: &'static LazyLock<Mutex<Vec<Finalizer>>>) -> Vec<String> {
    let mut values = {
        let mut guard = finalizers(stack);
        std::mem::take(&mut *guard)
    };
    let mut errors = Vec::new();
    while let Some(finalizer) = values.pop() {
        let recorded = finalizer.value;
        if let Some(state) = value::as_fiber(recorded) {
            if state.kind == fiber::FiberKind::Generator {
                let _ = iter::next_value(recorded, NONE, false);
            }
        } else if let Some(path) = value::as_path(recorded) {
            // A `tmp_path` directory: pytest removes everything the test
            // created inside it.
            let _ = std::fs::remove_dir_all(path.path());
        } else if value::as_function(recorded).is_some() {
            let _ = builtins::call(recorded, 0, std::ptr::null());
        } else {
            // Not a finalizer (for example a non-generator fiber); drop it.
            if finalizer.root != usize::MAX {
                heap::remove_persistent_root(finalizer.root);
            }
            continue;
        }
        if exc::pending() {
            let (tag, text) = pending_text();
            exc::clear();
            if tag != STOP_ITERATION {
                errors.push(text);
            }
        }
        if finalizer.root != usize::MAX {
            heap::remove_persistent_root(finalizer.root);
        }
    }
    errors
}

// -------------------------------------------------------------------------
// Builtin fixtures
// -------------------------------------------------------------------------

/// Fixture names [`fixture_builtin`] can supply, in the order a failed lookup
/// lists them.
const BUILTIN_FIXTURES: [&str; 6] = [
    "request",
    "tmp_path",
    "monkeypatch",
    "capsys",
    "caplog",
    "recwarn",
];

/// The test currently executing, for `request.node` and `tmp_path` names.
static CURRENT_TEST: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));
/// Unique suffix for temporary directories and capture files.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn current_test() -> String {
    CURRENT_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn set_current_test(name: &str) {
    let mut current = CURRENT_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    current.clear();
    current.push_str(name);
}

/// `pytest._fixture_builtin(name)`: supplies one of pytest's builtin fixtures
/// to the generated wrapper of a test that takes it as a parameter.
fn fixture_builtin(args: &[i64]) -> i64 {
    let Some(name) = args.first().copied().and_then(value::as_str) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "pytest._fixture_builtin() requires a fixture name string",
        );
    };
    match name {
        "request" => request_new(),
        "tmp_path" => tmp_path_new(),
        "monkeypatch" => monkeypatch_new(),
        "capsys" => capsys_new(),
        "caplog" => caplog_new(),
        "recwarn" => recwarn_new(),
        other => exc::raise_with_message(
            LOOKUP_ERROR,
            &format!(
                "pytest fixture '{other}' is not defined: Hydro implements the builtin \
                 fixtures {}",
                BUILTIN_FIXTURES.join(", ")
            ),
        ),
    }
}

/// Builds (and caches in the runtime registry) one fixture support class.
/// `methods` pairs the Python attribute name with the native dispatcher name
/// and its arity (the receiver counts).
fn fixture_class(key: &str, name: &str, methods: &[(&'static str, &'static str, usize)]) -> i64 {
    fixture_class_with_attrs(key, name, methods, &[])
}

/// Like [`fixture_class`], plus read-only properties (`attrs`), which
/// `value::class_new` cannot carry.
fn fixture_class_with_attrs(
    key: &str,
    name: &str,
    methods: &[(&'static str, &'static str, usize)],
    attrs: &[(&'static str, &'static str)],
) -> i64 {
    if let Some(class) = globals::registry_get(key) {
        heap::add_thread_root(class);
        return class;
    }
    let functions: Vec<i64> = methods
        .iter()
        .map(|(_, native, arity)| value::native_function_new("pytest", native, *arity))
        .collect();
    let getters: Vec<i64> = attrs
        .iter()
        .map(|(_, native)| value::native_function_new("pytest", native, 1))
        .collect();
    let function_roots = heap::root_values(&functions);
    let getter_roots = heap::root_values(&getters);
    let method_pairs: Vec<(String, i64, hydro_ir::MethodKind)> = methods
        .iter()
        .zip(&functions)
        .map(|((python_name, _, _), function)| {
            (
                (*python_name).to_string(),
                *function,
                hydro_ir::MethodKind::Normal,
            )
        })
        .collect();
    let properties: Vec<i64> = getters
        .iter()
        .map(|getter| value::property_new(*getter, NONE, NONE))
        .collect();
    let property_roots = heap::root_values(&properties);
    let attr_pairs: Vec<(String, i64)> = attrs
        .iter()
        .zip(&properties)
        .map(|((python_name, _), property)| ((*python_name).to_string(), *property))
        .collect();
    let class = value::class_new_full(name.to_string(), NONE, method_pairs, attr_pairs);
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(key, class);
    heap::release_roots(class_roots);
    heap::release_roots(property_roots);
    heap::release_roots(getter_roots);
    heap::release_roots(function_roots);
    class
}

fn getter(object: i64, name: &str) -> i64 {
    methods::getattr(object, name.as_ptr(), name.len())
}

fn setter(object: i64, name: &str, new_value: i64) -> i64 {
    methods::setattr(object, name.as_ptr(), name.len(), new_value)
}

fn invoke(object: i64, name: &str, args: &[i64]) -> i64 {
    let argv = if args.is_empty() {
        std::ptr::null()
    } else {
        args.as_ptr()
    };
    methods::call(object, name.as_ptr(), name.len(), args.len(), argv)
}

fn list_snapshot(list: i64) -> Vec<i64> {
    value::as_list(list)
        .map(|items| items.to_vec())
        .unwrap_or_default()
}

fn instance_index(instance: i64, key: &str) -> Option<usize> {
    value::instance_attr(instance, key)
        .and_then(value::as_integer)
        .and_then(|raw| usize::try_from(raw).ok())
}

fn sanitized(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn source_path() -> i64 {
    let argv = crate::modules::module_get_by_name("sys", "argv");
    if exc::pending() {
        exc::clear();
        return value::str_alloc(String::new());
    }
    match value::as_list(argv).and_then(|items| items.first().copied()) {
        Some(first) => first,
        None => value::str_alloc(String::new()),
    }
}

fn current_directory() -> String {
    std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// -- request ---------------------------------------------------------------

fn request_class() -> i64 {
    static METHODS: [(&str, &str, usize); 2] = [
        ("addfinalizer", "_request_addfinalizer", 2),
        ("getfixturevalue", "_request_getfixturevalue", 2),
    ];
    fixture_class("pytest._FixtureRequest.class", "FixtureRequest", &METHODS)
}

fn request_node_class() -> i64 {
    fixture_class(
        "pytest._FixtureRequestNode.class",
        "FixtureRequestNode",
        &[],
    )
}

fn request_config_class() -> i64 {
    static METHODS: [(&str, &str, usize); 1] = [("getoption", "_request_getoption", 3)];
    fixture_class(
        "pytest._FixtureRequestConfig.class",
        "FixtureRequestConfig",
        &METHODS,
    )
}

/// `request`: the per-test fixture request. `param`/`param_index` are the
/// parametrization slots (`None`/`0` for an unparametrized test);
/// `addfinalizer` registers a teardown callable on the same stack as
/// generator fixtures.
fn request_new() -> i64 {
    let instance = value::instance_new(request_class());
    let node = value::instance_new(request_node_class());
    let config = value::instance_new(request_config_class());
    let roots = heap::root_values(&[instance, node, config]);
    let name = current_test();
    value::instance_setattr(node, "name".to_string(), value::str_alloc(name.clone()));
    value::instance_setattr(node, "nodeid".to_string(), value::str_alloc(name.clone()));
    let source = source_path();
    value::instance_setattr(node, "fspath".to_string(), source);
    value::instance_setattr(node, "path".to_string(), source);
    value::instance_setattr(
        config,
        "rootdir".to_string(),
        value::str_alloc(current_directory()),
    );
    value::instance_setattr(
        config,
        "option".to_string(),
        value::instance_new(request_node_class()),
    );
    value::instance_setattr(instance, "node".to_string(), node);
    value::instance_setattr(instance, "config".to_string(), config);
    value::instance_setattr(instance, "fixturename".to_string(), value::str_alloc(name));
    value::instance_setattr(
        instance,
        "scope".to_string(),
        value::str_alloc("function".to_string()),
    );
    value::instance_setattr(instance, "param".to_string(), NONE);
    value::instance_setattr(instance, "param_index".to_string(), ops::int_result(0));
    value::instance_setattr(instance, "function".to_string(), NONE);
    value::instance_setattr(instance, "cls".to_string(), NONE);
    value::instance_setattr(instance, "instance".to_string(), NONE);
    value::instance_setattr(instance, "module".to_string(), NONE);
    value::instance_setattr(
        instance,
        "keywords".to_string(),
        value::dict_new(Vec::new()),
    );
    value::instance_setattr(
        instance,
        "fixturenames".to_string(),
        value::list_new(Vec::new()),
    );
    heap::release_roots(roots);
    instance
}

fn request_addfinalizer(args: &[i64]) -> i64 {
    let Some(&finalizer) = args.get(1) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "FixtureRequest.addfinalizer() requires a callable",
        );
    };
    let callable = value::as_function(finalizer).is_some()
        || value::as_instance(finalizer)
            .map(|state| value::class_lookup(state.class, "__call__").is_some())
            .unwrap_or(false);
    if !callable {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "FixtureRequest.addfinalizer() argument must be callable, not '{}'",
                value::type_name_of(finalizer)
            ),
        );
    }
    schedule_finalizer(finalizer, false);
    NONE
}

fn request_getfixturevalue(args: &[i64]) -> i64 {
    match args.get(1).copied() {
        Some(name) => fixture_builtin(&[name]),
        None => exc::raise_with_message(
            TYPE_ERROR,
            "FixtureRequest.getfixturevalue() requires a fixture name",
        ),
    }
}

/// `config.getoption(name, default=None)`: Hydro has no ini/CLI option
/// values, so the call reports the caller's default.
fn request_getoption(args: &[i64]) -> i64 {
    args.get(2).copied().unwrap_or(NONE)
}

// -- tmp_path --------------------------------------------------------------

/// `tmp_path`: a fresh directory under the system temporary directory,
/// removed (recursively) at teardown like pytest does.
fn tmp_path_new() -> i64 {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let test = current_test();
    let leaf = if test.is_empty() {
        format!("test{sequence}")
    } else {
        let leaf = test.rsplit('.').next().unwrap_or("test");
        format!("{}{sequence}", sanitized(leaf))
    };
    let directory = std::env::temp_dir()
        .join(format!("hydro-pytest-{}", std::process::id()))
        .join(leaf);
    if let Err(error) = std::fs::create_dir_all(&directory) {
        return exc::raise_with_message(
            OS_ERROR,
            &format!(
                "pytest fixture 'tmp_path': cannot create {}: {error}",
                directory.display()
            ),
        );
    }
    let path = value::path_new(directory.to_string_lossy().into_owned());
    schedule_finalizer(path, false);
    path
}

// -- monkeypatch -----------------------------------------------------------

const UNDO_ATTRIBUTE: i64 = 0;
const UNDO_ITEM: i64 = 1;
const UNDO_ENV: i64 = 2;
const UNDO_CHDIR: i64 = 3;
const UNDO_SYSPATH: i64 = 4;

fn monkeypatch_class() -> i64 {
    static METHODS: [(&str, &str, usize); 9] = [
        ("setattr", "_monkeypatch_setattr", 5),
        ("delattr", "_monkeypatch_delattr", 4),
        ("setitem", "_monkeypatch_setitem", 4),
        ("delitem", "_monkeypatch_delitem", 4),
        ("setenv", "_monkeypatch_setenv", 4),
        ("delenv", "_monkeypatch_delenv", 3),
        ("chdir", "_monkeypatch_chdir", 2),
        ("syspath_prepend", "_monkeypatch_syspath_prepend", 2),
        ("undo", "_monkeypatch_undo", 1),
    ];
    fixture_class("pytest.MonkeyPatch.class", "MonkeyPatch", &METHODS)
}

fn monkeypatch_new() -> i64 {
    let instance = value::instance_new(monkeypatch_class());
    let undo = value::bound_method_new(instance, "undo".to_string());
    let roots = heap::root_values(&[instance, undo]);
    value::instance_setattr(instance, "_undo".to_string(), value::list_new(Vec::new()));
    heap::release_roots(roots);
    // `undo` runs after every other teardown of the test.
    schedule_finalizer(undo, false);
    instance
}

/// Appends one undo record `(kind, a, b, c, d)` to `this._undo`. The list
/// keeps every recorded value reachable, so a collection cannot drop them.
fn monkeypatch_record(this: i64, kind: i64, fields: [i64; 4]) {
    let entry = value::tuple_new(vec![
        ops::int_result(kind),
        fields[0],
        fields[1],
        fields[2],
        fields[3],
    ]);
    let roots = heap::root_values(&[entry]);
    let log = value::instance_attr(this, "_undo").unwrap_or(NONE);
    let _ = container::list_append(log, entry);
    heap::release_roots(roots);
}

fn monkeypatch_self(args: &[i64], method: &str) -> Option<i64> {
    match args.first().copied() {
        Some(this) if value::as_instance(this).is_some() => Some(this),
        _ => {
            exc::raise_with_message(
                TYPE_ERROR,
                &format!("MonkeyPatch.{method}() missing the receiver"),
            );
            None
        }
    }
}

fn monkeypatch_target(args: &[i64], method: &str) -> Option<(i64, i64, String)> {
    let this = monkeypatch_self(args, method)?;
    let Some(target) = args.get(1).copied() else {
        exc::raise_with_message(
            TYPE_ERROR,
            &format!("MonkeyPatch.{method}() missing the target"),
        );
        return None;
    };
    let Some(attribute) = args.get(2).copied().and_then(value::as_str) else {
        exc::raise_with_message(
            TYPE_ERROR,
            &format!("MonkeyPatch.{method}() attribute name must be a str"),
        );
        return None;
    };
    Some((this, target, attribute.to_string()))
}

fn monkeypatch_setattr(args: &[i64]) -> i64 {
    let Some((this, target, attribute)) = monkeypatch_target(args, "setattr") else {
        return NONE;
    };
    let new_value = args.get(3).copied().unwrap_or(NONE);
    let roots = heap::root_values(&[this, target, new_value]);
    let old = getter(target, &attribute);
    let mut existed = true;
    if exc::pending() {
        if exc::pending_tag() != ATTRIBUTE_ERROR as i64 {
            heap::release_roots(roots);
            return old;
        }
        exc::clear();
        existed = false;
    }
    let name_value = value::str_alloc(attribute.clone());
    let existed_value = value::bool_value(existed);
    let record_roots = heap::root_values(&[name_value, existed_value, old]);
    monkeypatch_record(
        this,
        UNDO_ATTRIBUTE,
        [target, name_value, existed_value, old],
    );
    heap::release_roots(record_roots);
    let written = setter(target, &attribute, new_value);
    heap::release_roots(roots);
    written
}

fn monkeypatch_delattr(args: &[i64]) -> i64 {
    let Some((this, target, attribute)) = monkeypatch_target(args, "delattr") else {
        return NONE;
    };
    let raising = args.get(3).copied().map(value::truthy).unwrap_or(true);
    let roots = heap::root_values(&[this, target]);
    let old = getter(target, &attribute);
    if exc::pending() {
        if exc::pending_tag() == ATTRIBUTE_ERROR as i64 {
            exc::clear();
            heap::release_roots(roots);
            if raising {
                return exc::raise_with_message(ATTRIBUTE_ERROR, &attribute);
            }
            return NONE;
        }
        heap::release_roots(roots);
        return old;
    }
    let name_value = value::str_alloc(attribute.clone());
    let record_roots = heap::root_values(&[name_value, old]);
    monkeypatch_record(this, UNDO_ATTRIBUTE, [target, name_value, TRUE, old]);
    heap::release_roots(record_roots);
    let removed = if let Some(module) = value::as_module(target) {
        let module_name = module.name().to_string();
        globals::delete_value(&module_name, &attribute)
    } else if value::as_class(target).is_some() {
        if value::class_delete_attr(target, &attribute) {
            NONE
        } else {
            exc::raise_with_message(ATTRIBUTE_ERROR, &attribute)
        }
    } else {
        exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "MonkeyPatch.delattr() cannot delete an attribute of a class instance: Hydro \
             stores instance attributes in an append-only table. Patch the class or the \
             module instead.",
        )
    };
    heap::release_roots(roots);
    removed
}

fn monkeypatch_setitem(args: &[i64]) -> i64 {
    let Some(this) = monkeypatch_self(args, "setitem") else {
        return NONE;
    };
    let (Some(mapping), Some(key)) = (args.get(1).copied(), args.get(2).copied()) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "MonkeyPatch.setitem() requires a mapping and a key",
        );
    };
    let new_value = args.get(3).copied().unwrap_or(NONE);
    let roots = heap::root_values(&[this, mapping, key, new_value]);
    let old = container::subscript(mapping, key);
    let mut existed = true;
    if exc::pending() {
        if exc::pending_tag() != KEY_ERROR as i64 {
            heap::release_roots(roots);
            return old;
        }
        exc::clear();
        existed = false;
    }
    let existed_value = value::bool_value(existed);
    let record_roots = heap::root_values(&[existed_value, old]);
    monkeypatch_record(this, UNDO_ITEM, [mapping, key, existed_value, old]);
    heap::release_roots(record_roots);
    let written = container::dict_set(mapping, key, new_value);
    heap::release_roots(roots);
    written
}

fn monkeypatch_delitem(args: &[i64]) -> i64 {
    let Some(this) = monkeypatch_self(args, "delitem") else {
        return NONE;
    };
    let (Some(mapping), Some(key)) = (args.get(1).copied(), args.get(2).copied()) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "MonkeyPatch.delitem() requires a mapping and a key",
        );
    };
    let raising = args.get(3).copied().map(value::truthy).unwrap_or(true);
    let roots = heap::root_values(&[this, mapping, key]);
    let old = container::subscript(mapping, key);
    if exc::pending() {
        if exc::pending_tag() == KEY_ERROR as i64 {
            exc::clear();
            let message = value::repr(key);
            heap::release_roots(roots);
            if raising {
                return exc::raise_with_message(KEY_ERROR, &message);
            }
            return NONE;
        }
        heap::release_roots(roots);
        return old;
    }
    let record_roots = heap::root_values(&[old]);
    monkeypatch_record(this, UNDO_ITEM, [mapping, key, TRUE, old]);
    heap::release_roots(record_roots);
    let removed = container::delete_subscript(mapping, key);
    heap::release_roots(roots);
    removed
}

fn monkeypatch_setenv(args: &[i64]) -> i64 {
    let Some(this) = monkeypatch_self(args, "setenv") else {
        return NONE;
    };
    let Some(name) = args.get(1).copied().and_then(value::as_str) else {
        return exc::raise_with_message(TYPE_ERROR, "MonkeyPatch.setenv() name must be a str");
    };
    let Some(text) = args.get(2).copied().and_then(value::as_str) else {
        return exc::raise_with_message(TYPE_ERROR, "MonkeyPatch.setenv() value must be a str");
    };
    let prepend = args.get(3).copied().and_then(value::as_str);
    let name = name.to_string();
    let text = text.to_string();
    let old = std::env::var_os(&name);
    let old_value = match &old {
        Some(existing) => value::str_alloc(existing.to_string_lossy().into_owned()),
        None => NONE,
    };
    let existed_value = value::bool_value(old.is_some());
    let name_value = value::str_alloc(name.clone());
    let roots = heap::root_values(&[this, name_value, existed_value, old_value]);
    monkeypatch_record(this, UNDO_ENV, [name_value, existed_value, old_value, NONE]);
    heap::release_roots(roots);
    let combined = match (prepend, &old) {
        (Some(separator), Some(existing)) => {
            format!("{separator}{}{text}", existing.to_string_lossy())
        }
        _ => text,
    };
    std::env::set_var(&name, combined);
    NONE
}

fn monkeypatch_delenv(args: &[i64]) -> i64 {
    let Some(this) = monkeypatch_self(args, "delenv") else {
        return NONE;
    };
    let Some(name) = args.get(1).copied().and_then(value::as_str) else {
        return exc::raise_with_message(TYPE_ERROR, "MonkeyPatch.delenv() name must be a str");
    };
    let raising = args.get(2).copied().map(value::truthy).unwrap_or(true);
    let name = name.to_string();
    let old = std::env::var_os(&name);
    if old.is_none() && raising {
        return exc::raise_with_message(KEY_ERROR, &name);
    }
    let old_value = match &old {
        Some(existing) => value::str_alloc(existing.to_string_lossy().into_owned()),
        None => NONE,
    };
    let existed_value = value::bool_value(old.is_some());
    let name_value = value::str_alloc(name.clone());
    let roots = heap::root_values(&[this, name_value, existed_value, old_value]);
    monkeypatch_record(this, UNDO_ENV, [name_value, existed_value, old_value, NONE]);
    heap::release_roots(roots);
    std::env::remove_var(&name);
    NONE
}

fn monkeypatch_chdir(args: &[i64]) -> i64 {
    let Some(this) = monkeypatch_self(args, "chdir") else {
        return NONE;
    };
    let Some(target) = args.get(1).copied().and_then(value::as_path_like) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "MonkeyPatch.chdir() requires a str or pathlib.Path",
        );
    };
    let target = target.to_string();
    let previous = value::str_alloc(current_directory());
    let roots = heap::root_values(&[this, previous]);
    monkeypatch_record(this, UNDO_CHDIR, [previous, NONE, NONE, NONE]);
    heap::release_roots(roots);
    match std::env::set_current_dir(&target) {
        Ok(()) => NONE,
        Err(error) => exc::raise_with_message(
            OS_ERROR,
            &format!("MonkeyPatch.chdir(): cannot change to '{target}': {error}"),
        ),
    }
}

fn monkeypatch_syspath_prepend(args: &[i64]) -> i64 {
    let Some(this) = monkeypatch_self(args, "syspath_prepend") else {
        return NONE;
    };
    let Some(entry) = args.get(1).copied() else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "MonkeyPatch.syspath_prepend() requires a path",
        );
    };
    let path_list = crate::modules::module_get_by_name("sys", "path");
    if exc::pending() {
        return path_list;
    }
    let snapshot = value::list_new(list_snapshot(path_list));
    let roots = heap::root_values(&[this, path_list, snapshot, entry]);
    monkeypatch_record(this, UNDO_SYSPATH, [path_list, snapshot, NONE, NONE]);
    let index = ops::int_result(0);
    let insert_args = [index, entry];
    let inserted = methods::call(
        path_list,
        b"insert".as_ptr(),
        6,
        insert_args.len(),
        insert_args.as_ptr(),
    );
    heap::release_roots(roots);
    inserted
}

fn monkeypatch_undo(args: &[i64]) -> i64 {
    let Some(this) = monkeypatch_self(args, "undo") else {
        return NONE;
    };
    let log = value::instance_attr(this, "_undo").unwrap_or(NONE);
    let entries = list_snapshot(log);
    let roots = heap::root_values(&entries);
    for entry in entries.into_iter().rev() {
        let Some(fields) = value::as_tuple(entry) else {
            continue;
        };
        if fields.len() != 5 {
            continue;
        }
        let kind = value::as_integer(fields[0]).unwrap_or(-1);
        match kind {
            UNDO_ATTRIBUTE => {
                let target = fields[1];
                let Some(attribute) = value::as_str(fields[2]) else {
                    continue;
                };
                let attribute = attribute.to_string();
                if fields[3] == TRUE {
                    let _ = setter(target, &attribute, fields[4]);
                } else {
                    remove_attribute(target, &attribute);
                }
            }
            UNDO_ITEM => {
                let mapping = fields[1];
                let key = fields[2];
                if fields[3] == TRUE {
                    let _ = container::dict_set(mapping, key, fields[4]);
                } else {
                    let _ = container::delete_subscript(mapping, key);
                }
            }
            UNDO_ENV => {
                let Some(name) = value::as_str(fields[1]) else {
                    continue;
                };
                let name = name.to_string();
                if fields[2] == TRUE {
                    if let Some(text) = value::as_str(fields[3]) {
                        std::env::set_var(&name, text);
                    }
                } else {
                    std::env::remove_var(&name);
                }
            }
            UNDO_CHDIR => {
                if let Some(text) = value::as_str(fields[1]) {
                    let _ = std::env::set_current_dir(text);
                }
            }
            UNDO_SYSPATH => {
                let path_list = fields[1];
                let snapshot = fields[2];
                let restored = list_snapshot(snapshot);
                if let Some(mut items) = value::list_items_mut(path_list) {
                    items.clear();
                    items.extend_from_slice(&restored);
                }
            }
            _ => {}
        }
        if exc::pending() {
            break;
        }
    }
    heap::release_roots(roots);
    NONE
}

/// Removes an attribute the test added; only modules and classes keep a
/// deletion tombstone in the runtime.
fn remove_attribute(target: i64, attribute: &str) {
    if let Some(module) = value::as_module(target) {
        let name = module.name().to_string();
        let _ = globals::delete_value(&name, attribute);
        if exc::pending() {
            exc::clear();
        }
        return;
    }
    if value::as_class(target).is_some() && value::class_delete_attr(target, attribute) {
        return;
    }
    let _ = exc::raise_with_message(
        NOT_IMPLEMENTED_ERROR,
        "MonkeyPatch.undo() cannot remove an attribute that did not exist before the test: \
         Hydro cannot delete attributes of class instances",
    );
}

// -- capsys ----------------------------------------------------------------

/// One active descriptor capture. Output and error streams go to separate
/// files so `readouterr()` can report them independently.
struct Capture {
    saved_out: i32,
    saved_err: i32,
    out_path: PathBuf,
    err_path: PathBuf,
    out_position: u64,
    err_position: u64,
    active: bool,
}

static CAPTURES: LazyLock<Mutex<Vec<Capture>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn captures() -> MutexGuard<'static, Vec<Capture>> {
    CAPTURES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn capsys_class() -> i64 {
    static METHODS: [(&str, &str, usize); 2] = [
        ("readouterr", "_capsys_readouterr", 1),
        ("_close", "_capsys_close", 1),
    ];
    fixture_class("pytest.CaptureFixture.class", "CaptureFixture", &METHODS)
}

fn capsys_result_class() -> i64 {
    fixture_class("pytest.CaptureResult.class", "CaptureResult", &[])
}

fn capsys_new() -> i64 {
    let instance = value::instance_new(capsys_class());
    let roots = heap::root_values(&[instance]);
    let result = match capture_begin() {
        Ok(index) => {
            value::instance_setattr(
                instance,
                "_capture".to_string(),
                ops::int_result(index as i64),
            );
            schedule_finalizer(
                value::bound_method_new(instance, "_close".to_string()),
                false,
            );
            instance
        }
        Err(message) => exc::raise_with_message(OS_ERROR, &message),
    };
    heap::release_roots(roots);
    result
}

fn capture_begin() -> Result<usize, String> {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir();
    let out_path = directory.join(format!(
        "hydro-capsys-out-{}-{sequence}",
        std::process::id()
    ));
    let err_path = directory.join(format!(
        "hydro-capsys-err-{}-{sequence}",
        std::process::id()
    ));
    let out_file = std::fs::File::create(&out_path).map_err(|error| {
        format!(
            "pytest fixture 'capsys': cannot create {}: {error}",
            out_path.display()
        )
    })?;
    let err_file = std::fs::File::create(&err_path).map_err(|error| {
        format!(
            "pytest fixture 'capsys': cannot create {}: {error}",
            err_path.display()
        )
    })?;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // SAFETY: `dup`/`dup2` only touch this process' descriptor table; the
    // saved descriptors are closed by `capture_end`.
    let saved_out = unsafe { libc::dup(libc::STDOUT_FILENO) };
    if saved_out < 0 {
        return Err("pytest fixture 'capsys': cannot duplicate stdout".to_string());
    }
    let saved_err = unsafe { libc::dup(libc::STDERR_FILENO) };
    if saved_err < 0 {
        unsafe { libc::close(saved_out) };
        return Err("pytest fixture 'capsys': cannot duplicate stderr".to_string());
    }
    unsafe {
        libc::dup2(out_file.as_raw_fd(), libc::STDOUT_FILENO);
        libc::dup2(err_file.as_raw_fd(), libc::STDERR_FILENO);
    }
    let mut stack = captures();
    // Indices are stored on the live fixture objects, so only compact when
    // nothing is active; an open capture keeps its slot.
    if !stack.iter().any(|capture| capture.active) {
        stack.clear();
    }
    let index = stack.len();
    stack.push(Capture {
        saved_out,
        saved_err,
        out_path,
        err_path,
        out_position: 0,
        err_position: 0,
        active: true,
    });
    Ok(index)
}

fn capture_end(index: usize) {
    let mut stack = captures();
    let Some(capture) = stack.get_mut(index) else {
        return;
    };
    if !capture.active {
        return;
    }
    capture.active = false;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // SAFETY: both descriptors were produced by `dup` in `capture_begin`.
    unsafe {
        libc::dup2(capture.saved_out, libc::STDOUT_FILENO);
        libc::close(capture.saved_out);
        libc::dup2(capture.saved_err, libc::STDERR_FILENO);
        libc::close(capture.saved_err);
    }
    let _ = std::fs::remove_file(&capture.out_path);
    let _ = std::fs::remove_file(&capture.err_path);
}

/// Restores every capture that is still active; the runner calls this before
/// printing its report so a leaked capture cannot swallow it.
fn capture_reset() {
    let count = captures().len();
    for index in 0..count {
        capture_end(index);
    }
    captures().retain(|capture| capture.active);
}

fn read_capture(path: &Path, position: &mut u64) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    let start = (*position as usize).min(bytes.len());
    *position = bytes.len() as u64;
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

fn capsys_close(args: &[i64]) -> i64 {
    if let Some(index) = args
        .first()
        .copied()
        .and_then(|this| instance_index(this, "_capture"))
    {
        capture_end(index);
    }
    NONE
}

fn capsys_readouterr(args: &[i64]) -> i64 {
    let Some(index) = args
        .first()
        .copied()
        .and_then(|this| instance_index(this, "_capture"))
    else {
        return exc::raise_with_message(
            RUNTIME_ERROR,
            "CaptureFixture is missing its capture state",
        );
    };
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    let (out, err) = {
        let mut stack = captures();
        match stack.get_mut(index) {
            Some(capture) if capture.active => {
                let out_path = capture.out_path.clone();
                let err_path = capture.err_path.clone();
                let out = read_capture(&out_path, &mut capture.out_position);
                let err = read_capture(&err_path, &mut capture.err_position);
                (out, err)
            }
            _ => (String::new(), String::new()),
        }
    };
    let result = value::instance_new(capsys_result_class());
    let roots = heap::root_values(&[result]);
    value::instance_setattr(result, "out".to_string(), value::str_alloc(out));
    value::instance_setattr(result, "err".to_string(), value::str_alloc(err));
    heap::release_roots(roots);
    result
}

// -- caplog ----------------------------------------------------------------

fn logging_module() -> i64 {
    crate::modules::importlib::call("import_module", &[value::str_alloc("logging".to_string())])
}

fn caplog_class() -> i64 {
    static METHODS: [(&str, &str, usize); 4] = [
        ("set_level", "_caplog_set_level", 3),
        ("at_level", "_caplog_at_level", 4),
        ("clear", "_caplog_clear", 1),
        ("_close", "_caplog_close", 1),
    ];
    static ATTRS: [(&str, &str); 3] = [
        ("records", "_caplog_records"),
        ("messages", "_caplog_messages"),
        ("text", "_caplog_text"),
    ];
    fixture_class_with_attrs(
        "pytest.LogCaptureFixture.class",
        "LogCaptureFixture",
        &METHODS,
        &ATTRS,
    )
}

fn caplog_handler_class() -> i64 {
    static METHODS: [(&str, &str, usize); 1] = [("handle", "_caplog_handle", 2)];
    fixture_class(
        "pytest._CapturingHandler.class",
        "_CapturingHandler",
        &METHODS,
    )
}

fn caplog_at_level_class() -> i64 {
    static METHODS: [(&str, &str, usize); 2] = [
        ("__enter__", "_caplog_at_enter", 1),
        ("__exit__", "_caplog_at_exit", 4),
    ];
    fixture_class("pytest._AtLevelContext.class", "_AtLevelContext", &METHODS)
}

/// `caplog`: installs a capturing handler on the root logger through the
/// Python `logging` package, lowering the logger level so every record reaches
/// the handler. Teardown restores the previous handlers and level.
fn caplog_new() -> i64 {
    let module = logging_module();
    if exc::pending() {
        return module;
    }
    let instance = value::instance_new(caplog_class());
    let roots = heap::root_values(&[instance, module]);
    let result = caplog_install(instance, module);
    heap::release_roots(roots);
    result
}

fn caplog_install(instance: i64, module: i64) -> i64 {
    let records = value::list_new(Vec::new());
    let roots = heap::root_values(&[instance, module, records]);
    value::instance_setattr(instance, "_records".to_string(), records);
    value::instance_setattr(instance, "_level".to_string(), ops::int_result(0));
    let get_logger = getter(module, "getLogger");
    if exc::pending() {
        heap::release_roots(roots);
        return get_logger;
    }
    let logger = builtins::call(get_logger, 0, std::ptr::null());
    if exc::pending() {
        heap::release_roots(roots);
        return logger;
    }
    value::instance_setattr(instance, "_logger".to_string(), logger);

    let handler = value::instance_new(caplog_handler_class());
    value::instance_setattr(instance, "_handler".to_string(), handler);
    value::instance_setattr(handler, "records".to_string(), records);
    value::instance_setattr(handler, "level".to_string(), ops::int_result(0));
    value::instance_setattr(handler, "filters".to_string(), value::list_new(Vec::new()));

    let handlers = getter(logger, "handlers");
    if exc::pending() {
        heap::release_roots(roots);
        return handlers;
    }
    let previous_handlers = value::list_new(list_snapshot(handlers));
    let previous_level = getter(logger, "level");
    if exc::pending() {
        exc::clear();
    }
    value::instance_setattr(instance, "_handlers".to_string(), previous_handlers);
    value::instance_setattr(instance, "_logger_level".to_string(), previous_level);
    let appended = container::list_append(handlers, handler);
    if exc::pending() {
        heap::release_roots(roots);
        return appended;
    }
    let lowered = setter(logger, "level", ops::int_result(0));
    if exc::pending() {
        heap::release_roots(roots);
        return lowered;
    }
    let closed = value::bound_method_new(instance, "_close".to_string());
    schedule_finalizer(closed, false);
    heap::release_roots(roots);
    instance
}

fn caplog_handle(args: &[i64]) -> i64 {
    let Some(&this) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "handler.handle() missing the receiver");
    };
    let Some(&record) = args.get(1) else {
        return exc::raise_with_message(TYPE_ERROR, "handler.handle() missing the record");
    };
    let records = value::instance_attr(this, "records").unwrap_or(NONE);
    let roots = heap::root_values(&[records, record]);
    let _ = container::list_append(records, record);
    heap::release_roots(roots);
    TRUE
}

fn caplog_records_property(args: &[i64]) -> i64 {
    args.first()
        .and_then(|this| value::instance_attr(*this, "_records"))
        .unwrap_or(NONE)
}

fn caplog_messages_property(args: &[i64]) -> i64 {
    let Some(&this) = args.first() else {
        return NONE;
    };
    let records = value::instance_attr(this, "_records").unwrap_or(NONE);
    let entries = list_snapshot(records);
    let list = value::list_new(Vec::new());
    let roots = heap::root_values(&[list, records]);
    let entry_roots = heap::root_values(&entries);
    for record in entries {
        let message = invoke(record, "getMessage", &[]);
        if exc::pending() {
            break;
        }
        let _ = container::list_append(list, message);
    }
    heap::release_roots(entry_roots);
    heap::release_roots(roots);
    list
}

fn caplog_text_property(args: &[i64]) -> i64 {
    let Some(&this) = args.first() else {
        return NONE;
    };
    let records = value::instance_attr(this, "_records").unwrap_or(NONE);
    let entries = list_snapshot(records);
    let roots = heap::root_values(&entries);
    let mut text = String::new();
    for record in entries {
        let message = invoke(record, "getMessage", &[]);
        if exc::pending() {
            heap::release_roots(roots);
            return NONE;
        }
        if let Some(message) = value::as_str(message) {
            text.push_str(message);
            text.push('\n');
        }
    }
    heap::release_roots(roots);
    value::str_alloc(text)
}

fn caplog_clear(args: &[i64]) -> i64 {
    let Some(&this) = args.first() else {
        return NONE;
    };
    let records = value::instance_attr(this, "_records").unwrap_or(NONE);
    if let Some(mut items) = value::list_items_mut(records) {
        items.clear();
    }
    NONE
}

fn resolve_log_level(raw: i64) -> i64 {
    if let Some(number) = value::as_integer(raw) {
        return ops::int_result(number);
    }
    let Some(name) = value::as_str(raw) else {
        return ops::int_result(0);
    };
    let module = logging_module();
    if exc::pending() {
        exc::clear();
        return ops::int_result(0);
    }
    let table = getter(module, "_nameToLevel");
    if exc::pending() {
        exc::clear();
        return ops::int_result(0);
    }
    if let Some(pairs) = value::as_dict(table) {
        for (key, item) in pairs {
            if value::as_str(*key) == Some(name) {
                return *item;
            }
        }
    }
    ops::int_result(0)
}

fn apply_log_level(fixture: i64, level: i64, logger: i64) {
    let handler = value::instance_attr(fixture, "_handler").unwrap_or(NONE);
    let target = if logger == NONE {
        value::instance_attr(fixture, "_logger").unwrap_or(NONE)
    } else {
        logger
    };
    let roots = heap::root_values(&[fixture, handler, target, level]);
    let _ = setter(handler, "level", level);
    if !exc::pending() {
        let _ = setter(target, "level", level);
        value::instance_setattr(fixture, "_level".to_string(), level);
    }
    heap::release_roots(roots);
}

fn caplog_set_level(args: &[i64]) -> i64 {
    let Some(&this) = args.first() else {
        return NONE;
    };
    let level = resolve_log_level(args.get(1).copied().unwrap_or(NONE));
    let logger = args.get(2).copied().unwrap_or(NONE);
    apply_log_level(this, level, logger);
    NONE
}

fn caplog_at_level(args: &[i64]) -> i64 {
    let Some(&this) = args.first() else {
        return NONE;
    };
    let level = args.get(1).copied().unwrap_or(NONE);
    let logger = args.get(2).copied().unwrap_or(NONE);
    let context = value::instance_new(caplog_at_level_class());
    let roots = heap::root_values(&[context, this, level, logger]);
    value::instance_setattr(context, "_fixture".to_string(), this);
    value::instance_setattr(context, "_level".to_string(), level);
    value::instance_setattr(context, "_logger".to_string(), logger);
    value::instance_setattr(context, "_previous".to_string(), NONE);
    heap::release_roots(roots);
    context
}

fn caplog_at_enter(args: &[i64]) -> i64 {
    let Some(&context) = args.first() else {
        return NONE;
    };
    let fixture = value::instance_attr(context, "_fixture").unwrap_or(NONE);
    let level = resolve_log_level(value::instance_attr(context, "_level").unwrap_or(NONE));
    let logger = value::instance_attr(context, "_logger").unwrap_or(NONE);
    let target = if logger == NONE {
        value::instance_attr(fixture, "_logger").unwrap_or(NONE)
    } else {
        logger
    };
    let previous = getter(target, "level");
    if exc::pending() {
        exc::clear();
    }
    let roots = heap::root_values(&[context, fixture, target, previous, level]);
    value::instance_setattr(context, "_previous".to_string(), previous);
    apply_log_level(fixture, level, logger);
    heap::release_roots(roots);
    NONE
}

fn caplog_at_exit(args: &[i64]) -> i64 {
    let Some(&context) = args.first() else {
        return FALSE;
    };
    let fixture = value::instance_attr(context, "_fixture").unwrap_or(NONE);
    let previous = value::instance_attr(context, "_previous").unwrap_or(NONE);
    let logger = value::instance_attr(context, "_logger").unwrap_or(NONE);
    let target = if logger == NONE {
        value::instance_attr(fixture, "_logger").unwrap_or(NONE)
    } else {
        logger
    };
    let roots = heap::root_values(&[fixture, target, previous]);
    if previous != NONE {
        let _ = setter(target, "level", previous);
        value::instance_setattr(fixture, "_level".to_string(), previous);
    }
    heap::release_roots(roots);
    FALSE
}

fn caplog_close(args: &[i64]) -> i64 {
    let Some(&this) = args.first() else {
        return NONE;
    };
    let logger = value::instance_attr(this, "_logger").unwrap_or(NONE);
    if logger == NONE {
        return NONE;
    }
    let previous_handlers = value::instance_attr(this, "_handlers").unwrap_or(NONE);
    let previous_level = value::instance_attr(this, "_logger_level").unwrap_or(NONE);
    let roots = heap::root_values(&[this, logger, previous_handlers, previous_level]);
    if previous_handlers != NONE {
        let _ = setter(logger, "handlers", previous_handlers);
    }
    if !exc::pending() && previous_level != NONE {
        let _ = setter(logger, "level", previous_level);
    }
    heap::release_roots(roots);
    NONE
}

// -- recwarn ---------------------------------------------------------------

/// `recwarn`: a `warnings.catch_warnings(record=True)` recorder. Entering it
/// returns the live record list, which is what pytest's `WarningsRecorder`
/// exposes (iteration, `pop()`, indexing); teardown exits the manager so the
/// warning filters are restored.
fn recwarn_new() -> i64 {
    let manager = crate::modules::warnings::call("catch_warnings", &[TRUE]);
    if exc::pending() {
        return manager;
    }
    let roots = heap::root_values(&[manager]);
    let recorded = invoke(manager, "__enter__", &[]);
    if exc::pending() {
        heap::release_roots(roots);
        return recorded;
    }
    schedule_finalizer(
        value::bound_method_new(manager, "__exit__".to_string()),
        false,
    );
    heap::release_roots(roots);
    recorded
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
        discard_function_finalizers();
        set_current_test(&name);
        let value = call_wrapper(wrapper);
        let result = finish_test(&name, value, &mut errors);
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
        discard_function_finalizers();
        set_current_test(&name);
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
        let result = finish_test(&name, value, &mut errors);
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

    errors.extend(run_teardowns(&MODULE_FINALIZERS));
    // A test that aborted mid-capture must not swallow the report below.
    capture_reset();

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

/// Classifies one test and then runs its fixture finalizers.
///
/// A teardown error turns a passing test into a failure (pytest reports an
/// error for the same case) and is also reported separately; the finalizers
/// themselves keep running, so a broken cleanup cannot strand the rest of the
/// fixtures.
fn finish_test(name: &str, value: i64, errors: &mut Vec<String>) -> TestResult {
    let mut result = classify(name, value);
    let teardown_errors = run_teardowns(&FUNCTION_FINALIZERS);
    if let Some(error) = teardown_errors.first() {
        if result.outcome == Outcome::Pass {
            result.outcome = Outcome::Fail;
        }
        result.message = if result.message.is_empty() {
            format!("teardown error: {error}")
        } else {
            format!("{} (teardown error: {error})", result.message)
        };
    }
    errors.extend(teardown_errors);
    result
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exc::NOT_IMPLEMENTED_ERROR;

    #[test]
    fn main_reports_that_no_session_exists() {
        exc::init();
        assert_eq!(call("main", &[]), NONE);
        assert_eq!(exc::pending_tag(), NOT_IMPLEMENTED_ERROR as i64);
        let (tag, message) = pending_text();
        assert_eq!(tag, NOT_IMPLEMENTED_ERROR);
        assert!(message.contains("pytest.main()"), "{message}");
        assert!(message.contains("no arguments"), "{message}");
        assert!(message.contains("external Hydro test runner"), "{message}");
        exc::clear();
    }

    #[test]
    fn main_reports_how_many_arguments_were_supplied() {
        exc::init();
        assert_eq!(call("main", &[NONE, NONE]), NONE);
        let (_, message) = pending_text();
        assert!(message.contains("2 argument(s)"), "{message}");
        exc::clear();
    }
}
