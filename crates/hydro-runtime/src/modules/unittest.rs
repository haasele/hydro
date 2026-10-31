//! Native `unittest` support.
//!
//! The runtime implements the framework surface the CPython standard-library
//! test suite imports: `TestCase`, `TestSuite`, the result classes, the text
//! runner, the loader, `TestProgram`, the skip/expected-failure decorators and
//! the `unittest.util` helpers.
//!
//! Every user subclass of the native `TestCase` is registered when the class is
//! created (see `value::class_new`), so `unittest.main()` can discover and run
//! the tests without importing a Python `unittest` package.
//!
//! Result accounting is real: [`run_main`] and `TestCase.run(result)` report
//! success, failure, error, skip, expected failure and unexpected success into
//! the caller's result object, using the same `add*` methods CPython calls.

use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::builtins;
use crate::container;
use crate::core::globals;
use crate::exc::{self, ASSERTION_ERROR, ATTRIBUTE_ERROR, SKIPPED, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::iter;
use crate::methods;
use crate::ops;
use crate::value::{self, FALSE, NONE, TRUE};

/// Hidden native method that identifies a `TestCase`-derived class.
const CASE_MARKER: &str = "__unittest_case__";

/// Default result / runner cache key prefixes.
const TEST_CASE_KEY: &str = "unittest.TestCase.class";
const TEST_RESULT_KEY: &str = "unittest.TestResult.class";
const TEXT_RESULT_KEY: &str = "unittest.TextTestResult.class";
const TEXT_RUNNER_KEY: &str = "unittest.TextTestRunner.class";
const TEST_LOADER_KEY: &str = "unittest.TestLoader.class";
const TEST_PROGRAM_KEY: &str = "unittest.TestProgram.class";
const ERROR_HOLDER_KEY: &str = "unittest.util._ErrorHolder.class";
const TEST_LIST_KEY: &str = "unittest.util.TestList.class";
const SUBTEST_KEY: &str = "unittest._SubTest.class";
const RAISES_KEY: &str = "unittest._AssertRaisesContext.class";
const DEFAULT_LOADER_KEY: &str = "unittest.defaultTestLoader";

/// Registered test classes, in creation order.
static REGISTRY: LazyLock<Mutex<Vec<i64>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn registry() -> MutexGuard<'static, Vec<i64>> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Records a test class created by the frontend.
pub(crate) fn register(class: i64) {
    registry().push(class);
    heap::add_thread_root(class);
}

/// Forgets every registered class (used by tests and a fresh runner start).
pub(crate) fn reset() {
    registry().clear();
}

/// Builds (or returns) the cached native class for `name`.
fn cached_class(
    key: &str,
    name: &str,
    base: i64,
    methods: &'static [(&'static str, &'static str, usize)],
) -> i64 {
    if let Some(class) = globals::registry_get(key) {
        return class;
    }
    let functions: Vec<i64> = methods
        .iter()
        .map(|(_, native, arity)| value::native_function_new("unittest", native, *arity))
        .collect();
    let function_roots = heap::root_values(&functions);
    let pairs: Vec<(String, i64)> = methods
        .iter()
        .zip(&functions)
        .map(|((python_name, _, _), function)| ((*python_name).to_string(), *function))
        .collect();
    let class = value::class_new(name.to_string(), base, pairs);
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(key, class);
    heap::release_roots(class_roots);
    heap::release_roots(function_roots);
    class
}

/// Builds a fresh native `TestCase` class.
pub(crate) fn test_case() -> i64 {
    if let Some(class) = globals::registry_get(TEST_CASE_KEY) {
        return class;
    }
    // (Python name, dispatch name, minimum argc including `self`).
    static METHODS: [(&str, &str, usize); 48] = [
        (CASE_MARKER, "_case_marker", 1),
        ("__init__", "TestCase.__init__", 1),
        ("run", "TestCase.run", 1),
        ("__call__", "TestCase.__call__", 1),
        ("debug", "TestCase.debug", 1),
        ("setUp", "TestCase.setUp", 1),
        ("tearDown", "TestCase.tearDown", 1),
        ("addCleanup", "TestCase.addCleanup", 2),
        ("doCleanups", "TestCase.doCleanups", 1),
        ("subTest", "TestCase.subTest", 1),
        ("id", "TestCase.id", 1),
        ("shortDescription", "TestCase.shortDescription", 1),
        ("__str__", "TestCase.__str__", 1),
        ("__repr__", "TestCase.__repr__", 1),
        ("countTestCases", "TestCase.countTestCases", 1),
        ("defaultTestResult", "TestCase.defaultTestResult", 1),
        ("assertEqual", "TestCase.assertEqual", 3),
        ("assertNotEqual", "TestCase.assertNotEqual", 3),
        ("assertTrue", "TestCase.assertTrue", 2),
        ("assertFalse", "TestCase.assertFalse", 2),
        ("assertIs", "TestCase.assertIs", 3),
        ("assertIsNot", "TestCase.assertIsNot", 3),
        ("assertIsNone", "TestCase.assertIsNone", 2),
        ("assertIsNotNone", "TestCase.assertIsNotNone", 2),
        ("assertIn", "TestCase.assertIn", 3),
        ("assertNotIn", "TestCase.assertNotIn", 3),
        ("assertIsInstance", "TestCase.assertIsInstance", 3),
        ("assertNotIsInstance", "TestCase.assertNotIsInstance", 3),
        ("assertGreater", "TestCase.assertGreater", 3),
        ("assertGreaterEqual", "TestCase.assertGreaterEqual", 3),
        ("assertLess", "TestCase.assertLess", 3),
        ("assertLessEqual", "TestCase.assertLessEqual", 3),
        ("assertAlmostEqual", "TestCase.assertAlmostEqual", 3),
        ("assertNotAlmostEqual", "TestCase.assertNotAlmostEqual", 3),
        ("assertRegex", "TestCase.assertRegex", 3),
        ("assertNotRegex", "TestCase.assertNotRegex", 3),
        ("assertCountEqual", "TestCase.assertCountEqual", 3),
        ("assertSequenceEqual", "TestCase.assertSequenceEqual", 3),
        ("assertListEqual", "TestCase.assertListEqual", 3),
        ("assertTupleEqual", "TestCase.assertTupleEqual", 3),
        ("assertSetEqual", "TestCase.assertSetEqual", 3),
        ("assertDictEqual", "TestCase.assertDictEqual", 3),
        ("assertMultiLineEqual", "TestCase.assertMultiLineEqual", 3),
        ("assertRaises", "TestCase.assertRaises", 2),
        ("assertRaisesRegex", "TestCase.assertRaisesRegex", 3),
        ("assertHasAttr", "TestCase.assertHasAttr", 3),
        ("assertNotHasAttr", "TestCase.assertNotHasAttr", 3),
        ("fail", "TestCase.fail", 1),
    ];
    let class = cached_class(TEST_CASE_KEY, "TestCase", NONE, &METHODS);
    // CPython's `TestCase` class attributes that user code reads.
    value::class_set_attr(
        class,
        "failureException".to_string(),
        exc::exception_class(ASSERTION_ERROR),
    );
    value::class_set_attr(class, "longMessage".to_string(), TRUE);
    value::class_set_attr(class, "maxDiff".to_string(), encode_int(80 * 8));
    // `failIf`/`failUnless` are the deprecated spellings of assertFalse/True.
    value::class_set_attr(
        class,
        "failIf".to_string(),
        value::native_function_new("unittest", "TestCase.assertFalse", 3),
    );
    value::class_set_attr(
        class,
        "failUnless".to_string(),
        value::native_function_new("unittest", "TestCase.assertTrue", 3),
    );
    class
}

/// The cached `TestResult` class.
pub(crate) fn test_result_class() -> i64 {
    static METHODS: [(&str, &str, usize); 18] = [
        ("__init__", "TestResult.__init__", 1),
        ("startTest", "TestResult.startTest", 2),
        ("stopTest", "TestResult.stopTest", 2),
        ("startTestRun", "TestResult.startTestRun", 1),
        ("stopTestRun", "TestResult.stopTestRun", 1),
        ("addSuccess", "TestResult.addSuccess", 2),
        ("addError", "TestResult.addError", 3),
        ("addFailure", "TestResult.addFailure", 3),
        ("addSubTest", "TestResult.addSubTest", 4),
        ("addSkip", "TestResult.addSkip", 3),
        ("addExpectedFailure", "TestResult.addExpectedFailure", 3),
        ("addUnexpectedSuccess", "TestResult.addUnexpectedSuccess", 2),
        ("addDuration", "TestResult.addDuration", 3),
        ("wasSuccessful", "TestResult.wasSuccessful", 1),
        ("stop", "TestResult.stop", 1),
        ("printErrors", "TestResult.printErrors", 1),
        ("__repr__", "TestResult.__repr__", 1),
        ("_exc_info_to_string", "TestResult._exc_info_to_string", 3),
    ];
    cached_class(TEST_RESULT_KEY, "TestResult", NONE, &METHODS)
}

/// The cached `TextTestResult` class (a real `TestResult` subclass).
pub(crate) fn text_test_result_class() -> i64 {
    cached_class(TEXT_RESULT_KEY, "TextTestResult", test_result_class(), &[])
}

/// The cached `TextTestRunner` class.
pub(crate) fn text_test_runner_class() -> i64 {
    static METHODS: [(&str, &str, usize); 3] = [
        ("__init__", "TextTestRunner.__init__", 1),
        ("_makeResult", "TextTestRunner._makeResult", 1),
        ("run", "TextTestRunner.run", 2),
    ];
    cached_class(TEXT_RUNNER_KEY, "TextTestRunner", NONE, &METHODS)
}

/// The cached `TestLoader` class.
pub(crate) fn test_loader_class() -> i64 {
    static METHODS: [(&str, &str, usize); 8] = [
        ("__init__", "TestLoader.__init__", 1),
        ("getTestCaseNames", "TestLoader.getTestCaseNames", 2),
        (
            "loadTestsFromTestCase",
            "TestLoader.loadTestsFromTestCase",
            2,
        ),
        ("loadTestsFromModule", "TestLoader.loadTestsFromModule", 2),
        ("loadTestsFromNames", "TestLoader.loadTestsFromNames", 2),
        ("loadTestsFromName", "TestLoader.loadTestsFromName", 2),
        ("discover", "TestLoader.discover", 2),
        ("_makeSuite", "TestLoader._makeSuite", 2),
    ];
    let class = cached_class(TEST_LOADER_KEY, "TestLoader", NONE, &METHODS);
    value::class_set_attr(
        class,
        "testMethodPrefix".to_string(),
        value::str_alloc("test".to_string()),
    );
    value::class_set_attr(
        class,
        "sortTestMethodsUsing".to_string(),
        value::native_function_new("unittest", "_three_way_cmp", 2),
    );
    value::class_set_attr(
        class,
        "suiteClass".to_string(),
        crate::modules::unittest_test_suite_class(),
    );
    class
}

/// `unittest.defaultTestLoader`: the shared `TestLoader` instance.
pub(crate) fn default_test_loader() -> i64 {
    if let Some(loader) = globals::registry_get(DEFAULT_LOADER_KEY) {
        return loader;
    }
    let loader = value::instance_new(test_loader_class());
    let roots = heap::root_values(std::slice::from_ref(&loader));
    globals::registry_set(DEFAULT_LOADER_KEY, loader);
    heap::release_roots(roots);
    loader
}

/// The cached `TestProgram` class.
pub(crate) fn test_program_class() -> i64 {
    static METHODS: [(&str, &str, usize); 3] = [
        ("__init__", "TestProgram.__init__", 1),
        ("runTests", "TestProgram.runTests", 1),
        ("parseArgs", "TestProgram.parseArgs", 2),
    ];
    cached_class(TEST_PROGRAM_KEY, "TestProgram", NONE, &METHODS)
}

/// The cached `_ErrorHolder` class (also exposed by `unittest.util`).
pub(crate) fn error_holder_class() -> i64 {
    static METHODS: [(&str, &str, usize); 7] = [
        ("__init__", "_ErrorHolder.__init__", 2),
        ("id", "_ErrorHolder.id", 1),
        ("shortDescription", "_ErrorHolder.shortDescription", 1),
        ("__repr__", "_ErrorHolder.__repr__", 1),
        ("__str__", "_ErrorHolder.__str__", 1),
        ("run", "_ErrorHolder.run", 2),
        ("__call__", "_ErrorHolder.__call__", 2),
    ];
    let class = cached_class(ERROR_HOLDER_KEY, "_ErrorHolder", NONE, &METHODS);
    value::class_set_attr(class, "failureException".to_string(), NONE);
    class
}

/// The cached `TestList` class (a list-like test container).
pub(crate) fn test_list_class() -> i64 {
    static METHODS: [(&str, &str, usize); 6] = [
        ("__init__", "TestList.__init__", 1),
        ("__repr__", "TestList.__repr__", 1),
        ("__iter__", "TestList.__iter__", 1),
        ("__len__", "TestList.__len__", 1),
        ("__eq__", "TestList.__eq__", 2),
        ("__add__", "TestList.__add__", 2),
    ];
    cached_class(TEST_LIST_KEY, "TestList", NONE, &METHODS)
}

/// The cached `_SubTest` context manager class.
fn subtest_class() -> i64 {
    static METHODS: [(&str, &str, usize); 4] = [
        ("__init__", "_SubTest.__init__", 3),
        ("__enter__", "_SubTest.__enter__", 1),
        ("__exit__", "_SubTest.__exit__", 4),
        ("__str__", "_SubTest.__str__", 1),
    ];
    cached_class(SUBTEST_KEY, "_SubTest", NONE, &METHODS)
}

/// The cached `_AssertRaisesContext` class.
fn raises_context_class() -> i64 {
    static METHODS: [(&str, &str, usize); 3] = [
        ("__init__", "_AssertRaisesContext.__init__", 3),
        ("__enter__", "_AssertRaisesContext.__enter__", 1),
        ("__exit__", "_AssertRaisesContext.__exit__", 4),
    ];
    cached_class(RAISES_KEY, "_AssertRaisesContext", NONE, &METHODS)
}

/// Dispatches a native `unittest` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "main" => run_main(),
        "expectedFailure" => decorator_expected_failure(args),
        "skip" => skip(args),
        "skipIf" => skip_if(args, false),
        "skipUnless" => skip_if(args, true),
        "_skip_apply" => skip_apply(args),
        "_identity" => args.first().copied().unwrap_or(NONE),
        "_three_way_cmp" => three_way_cmp(args),
        // Legacy internal hooks used by earlier native wrappers: they map to
        // the public assertion methods.
        "_case_marker" => NONE,
        "_assert_true" => test_case_method("assertTrue", args),
        "_assert_false" => test_case_method("assertFalse", args),
        "_assert_equal" => test_case_method("assertEqual", args),
        "_assert_is" => test_case_method("assertIs", args),
        "_assert_in" => test_case_method("assertIn", args),
        "_assert_has_attr" => test_case_method("assertHasAttr", args),
        "_fail" => test_case_method("fail", args),
        _ => dispatch(function, args),
    }
}

/// Keyword-aware dispatch for class methods whose splat parameters keep
/// keyword names (`addCleanup`, `subTest`, `assertRaises(..., **kwargs)`).
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: Vec<(String, i64)>) -> i64 {
    match function {
        "TestCase.addCleanup"
        | "TestCase.subTest"
        | "TestCase.assertRaises"
        | "TestCase.assertRaisesRegex" => {
            let dictionary = keywords_dict(&keywords);
            let mut call_args = args.to_vec();
            call_args.push(dictionary);
            dispatch(function, &call_args)
        }
        // Constructors and runners keep their declared positional slots; the
        // remaining names are accepted and ignored exactly like CPython's
        // compatibility keyword arguments.
        "main"
        | "TestResult.__init__"
        | "TextTestRunner.__init__"
        | "TextTestRunner.run"
        | "TestProgram.__init__"
        | "TestLoader.__init__" => call(function, args),
        _ => {
            if keywords.is_empty() {
                dispatch(function, args)
            } else {
                let name = keywords[0].0.clone();
                exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("unittest.{function}() got an unexpected keyword argument '{name}'"),
                )
            }
        }
    }
}

fn dispatch(function: &str, args: &[i64]) -> i64 {
    if let Some(method) = function.strip_prefix("TestCase.") {
        return test_case_method(method, args);
    }
    if let Some(method) = function.strip_prefix("TestResult.") {
        return test_result_method(method, args);
    }
    if let Some(method) = function.strip_prefix("TextTestRunner.") {
        return runner_method(method, args);
    }
    if let Some(method) = function.strip_prefix("TestLoader.") {
        return loader_method(method, args);
    }
    if let Some(method) = function.strip_prefix("TestProgram.") {
        return program_method(method, args);
    }
    if let Some(method) = function.strip_prefix("_ErrorHolder.") {
        return error_holder_method(method, args);
    }
    if let Some(method) = function.strip_prefix("TestList.") {
        return test_list_method(method, args);
    }
    if let Some(method) = function.strip_prefix("_SubTest.") {
        return subtest_method(method, args);
    }
    if let Some(method) = function.strip_prefix("_AssertRaisesContext.") {
        return raises_context_method(method, args);
    }
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module 'unittest' has no attribute '{function}'"),
    )
}

// -------------------------------------------------------------------------
// Decorators
// -------------------------------------------------------------------------

/// `unittest.expectedFailure(test_item)`: marks a test (or class) expected to
/// fail and returns it unchanged.
fn decorator_expected_failure(args: &[i64]) -> i64 {
    let Some(&item) = args.first() else {
        return missing_arguments("expectedFailure");
    };
    set_item_flag(item, "__unittest_expecting_failure__", TRUE);
    item
}

/// `unittest.skip(reason)`: the decorator factory, or the decorator itself
/// when used as `@unittest.skip` (the test item arrives as the only argument).
fn skip(args: &[i64]) -> i64 {
    let Some(&reason) = args.first() else {
        return missing_arguments("skip");
    };
    if value::as_function(reason).is_some() || value::as_class(reason).is_some() {
        return apply_skip(reason, "");
    }
    let reason_text = reason_text(reason);
    let apply = value::native_function_new("unittest", "_skip_apply", 3);
    value::partial_new(apply, vec![TRUE, value::str_alloc(reason_text)])
}

fn skip_if(args: &[i64], unless: bool) -> i64 {
    let (Some(&condition), Some(&reason)) = (args.first(), args.get(1)) else {
        return missing_arguments(if unless { "skipUnless" } else { "skipIf" });
    };
    let triggered = value::truthy(condition) != unless;
    if !triggered {
        return value::native_function_new("unittest", "_identity", 1);
    }
    let apply = value::native_function_new("unittest", "_skip_apply", 3);
    value::partial_new(apply, vec![TRUE, value::str_alloc(reason_text(reason))])
}

/// `_skip_apply(condition, reason, test_item)`: the decorator body shared by
/// `skip`/`skipIf`/`skipUnless`.
fn skip_apply(args: &[i64]) -> i64 {
    let (Some(&condition), Some(&reason), Some(&item)) = (args.first(), args.get(1), args.get(2))
    else {
        return missing_arguments("_skip_apply");
    };
    if !value::truthy(condition) {
        return item;
    }
    let text = match value::as_str(reason) {
        Some(text) => text.to_string(),
        None if reason == NONE => String::new(),
        None => value::to_string(reason),
    };
    apply_skip(item, &text)
}

fn apply_skip(item: i64, reason: &str) -> i64 {
    set_item_flag(item, "__unittest_skip__", TRUE);
    let reason = value::str_alloc(reason.to_string());
    set_item_flag(item, "__unittest_skip_why__", reason);
    item
}

/// Stores an attribute on a class or function test item.
fn set_item_flag(item: i64, name: &str, value_to_set: i64) {
    if value::as_class(item).is_some() {
        value::class_set_attr(item, name.to_string(), value_to_set);
        return;
    }
    if !value::function_setattr(item, name, value_to_set) {
        let _ = exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!(
                "'{}' object has no attribute '{name}'",
                value::type_name(item)
            ),
        );
    }
}

fn reason_text(reason: i64) -> String {
    match value::as_str(reason) {
        Some(text) => text.to_string(),
        None if reason == NONE => String::new(),
        None => value::to_string(reason),
    }
}

// -------------------------------------------------------------------------
// TestCase
// -------------------------------------------------------------------------

/// Native methods of `TestCase`.
fn test_case_method(method: &str, args: &[i64]) -> i64 {
    match method {
        "__init__" => test_case_init(args),
        "run" => test_case_run(args),
        "__call__" => {
            let mut call_args = Vec::with_capacity(2);
            call_args.push(args.first().copied().unwrap_or(NONE));
            if let Some(&result) = args.get(1) {
                call_args.push(result);
            }
            test_case_run(&call_args)
        }
        "debug" => test_case_debug(args),
        "setUp" | "tearDown" => NONE,
        "addCleanup" => add_cleanup(args),
        "doCleanups" => match do_cleanups(args[0]) {
            Some(error) => {
                let result = value::instance_attr(args[0], "_unittest_result").unwrap_or(NONE);
                if result != NONE {
                    call_method(result, "addError", &[args[0], error_tuple(error)]);
                }
                FALSE
            }
            None => TRUE,
        },
        "subTest" => sub_test(args),
        "id" => test_case_id(args),
        "shortDescription" => short_description(args),
        "__str__" => test_case_str(args),
        "__repr__" => test_case_repr(args),
        "countTestCases" => encode_int(1),
        "defaultTestResult" => new_test_result(),
        "fail" => fail(args),
        "assertEqual"
        | "assertListEqual"
        | "assertTupleEqual"
        | "assertSetEqual"
        | "assertDictEqual"
        | "assertMultiLineEqual"
        | "assertSequenceEqual" => assert_equal(args),
        "assertNotEqual" => assert_not_equal(args),
        "assertTrue" => assert_true(args, true),
        "assertFalse" => assert_true(args, false),
        "assertIs" => assert_is(args, true),
        "assertIsNot" => assert_is(args, false),
        "assertIsNone" => assert_is_none(args, true),
        "assertIsNotNone" => assert_is_none(args, false),
        "assertIn" => assert_in(args, true),
        "assertNotIn" => assert_in(args, false),
        "assertIsInstance" => assert_is_instance(args, true),
        "assertNotIsInstance" => assert_is_instance(args, false),
        "assertGreater" => assert_order(args, ops::OP_GT),
        "assertGreaterEqual" => assert_order(args, ops::OP_GE),
        "assertLess" => assert_order(args, ops::OP_LT),
        "assertLessEqual" => assert_order(args, ops::OP_LE),
        "assertAlmostEqual" => assert_almost_equal(args, true),
        "assertNotAlmostEqual" => assert_almost_equal(args, false),
        "assertRegex" => assert_regex(args, true),
        "assertNotRegex" => assert_regex(args, false),
        "assertCountEqual" => assert_count_equal(args),
        "assertRaises" => assert_raises(args, false),
        "assertRaisesRegex" => assert_raises(args, true),
        "assertHasAttr" => assert_has_attr(args, true),
        "assertNotHasAttr" => assert_has_attr(args, false),
        _ => missing_method("TestCase", method),
    }
}

fn test_case_init(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return missing_arguments("TestCase.__init__");
    };
    let name = match args.get(1).copied() {
        None | Some(NONE) => "runTest".to_string(),
        Some(name) => match value::as_str(name) {
            Some(text) => text.to_string(),
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "TestCase.__init__() argument 'methodName' must be str, not '{}'",
                        value::type_name(name)
                    ),
                )
            }
        },
    };
    let class = value::as_instance(instance)
        .map(|state| state.class)
        .unwrap_or(instance);
    if name != "runTest" && value::class_lookup(class, &name).is_none() {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!(
                "no such test method in {}: {name}",
                value::class_display_name(class).unwrap_or("class")
            ),
        );
    }
    value::instance_setattr(
        instance,
        "_testMethodName".to_string(),
        value::str_alloc(name),
    );
    value::instance_setattr(
        instance,
        "_cleanups".to_string(),
        value::list_new(Vec::new()),
    );
    value::instance_setattr(instance, "_unittest_result".to_string(), NONE);
    NONE
}

/// `TestCase.run(result=None)`.
fn test_case_run(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return missing_arguments("TestCase.run");
    };
    if value::as_instance(instance).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "TestCase.run() called on non-TestCase '{}'",
                value::type_name(instance)
            ),
        );
    }
    let class = value::as_instance(instance)
        .map(|state| state.class)
        .unwrap_or(instance);
    let supplied = args.get(1).copied().unwrap_or(NONE);
    let owns_result = supplied == NONE;
    let result = if owns_result {
        let result = new_test_result();
        call_method(result, "startTestRun", &[]);
        result
    } else {
        supplied
    };
    call_method(result, "startTest", &[instance]);

    let name = value::instance_attr(instance, "_testMethodName")
        .and_then(value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "runTest".to_string());
    let method = method_function(class, &name);

    // Class-level or method-level `@unittest.skip`.
    let class_skip = class_attr_truthy(class, "__unittest_skip__");
    let method_skip = method.is_some_and(|function| attr_truthy(function, "__unittest_skip__"));
    if class_skip || method_skip {
        let why = class_attr_text(class, "__unittest_skip_why__")
            .or_else(|| {
                method.and_then(|function| function_attr_text(function, "__unittest_skip_why__"))
            })
            .unwrap_or_default();
        call_method(result, "addSkip", &[instance, value::str_alloc(why)]);
        call_method(result, "stopTest", &[instance]);
        if owns_result {
            call_method(result, "stopTestRun", &[]);
        }
        return result;
    }

    let expecting = class_attr_truthy(class, "__unittest_expecting_failure__")
        || method.is_some_and(|function| attr_truthy(function, "__unittest_expecting_failure__"));

    // Expose the active result so `subTest` contexts can report into it.
    value::instance_setattr(instance, "_unittest_result".to_string(), result);

    call_method(instance, "setUp", &[]);
    let mut failure: Option<i64> = None;
    let mut from_test = false;
    if exc::pending() {
        // `setUp` failures are always errors; CPython only arms the expected
        // failure flag around the test method itself.
        failure = Some(take_pending());
    } else {
        call_method(instance, &name, &[]);
        if exc::pending() {
            failure = Some(take_pending());
            from_test = true;
        } else {
            // `tearDown` runs only when the test body itself succeeded,
            // exactly like CPython's `_Outcome.success` gate.
            call_method(instance, "tearDown", &[]);
            if exc::pending() {
                failure = Some(take_pending());
            }
        }
    }

    // Cleanups always run, even after a failing `setUp`. A failing cleanup is
    // an error of its own and prevents the run from being reported as a pass.
    let cleanup_failure = do_cleanups(instance);
    if let Some(error) = failure {
        record_test_error(result, instance, error, from_test && expecting);
    } else if cleanup_failure.is_none() {
        if expecting {
            call_method(result, "addUnexpectedSuccess", &[instance]);
        } else {
            call_method(result, "addSuccess", &[instance]);
        }
    }
    if let Some(error) = cleanup_failure {
        call_method(result, "addError", &[instance, error_tuple(error)]);
    }

    value::instance_setattr(instance, "_unittest_result".to_string(), NONE);
    call_method(result, "stopTest", &[instance]);
    if owns_result {
        call_method(result, "stopTestRun", &[]);
    }
    result
}

/// `TestCase.debug()`: run without catching, so the exception escapes.
fn test_case_debug(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return missing_arguments("TestCase.debug");
    };
    let class = value::as_instance(instance)
        .map(|state| state.class)
        .unwrap_or(instance);
    let name = value::instance_attr(instance, "_testMethodName")
        .and_then(value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "runTest".to_string());
    if class_attr_truthy(class, "__unittest_skip__")
        || method_function(class, &name)
            .is_some_and(|function| attr_truthy(function, "__unittest_skip__"))
    {
        let why = class_attr_text(class, "__unittest_skip_why__")
            .or_else(|| {
                method_function(class, &name)
                    .and_then(|function| function_attr_text(function, "__unittest_skip_why__"))
            })
            .unwrap_or_default();
        return exc::raise_with_message(SKIPPED, &why);
    }
    call_method(instance, "setUp", &[]);
    if exc::pending() {
        return NONE;
    }
    call_method(instance, &name, &[]);
    if exc::pending() {
        return NONE;
    }
    call_method(instance, "tearDown", &[]);
    NONE
}

fn add_cleanup(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&function)) = (args.first(), args.get(1)) else {
        return missing_arguments("TestCase.addCleanup");
    };
    let rest = &args[2..];
    // The keyword dispatcher appends a dict of **kwargs as the final argument.
    let (call_args, kwargs) = match rest.last() {
        Some(&dictionary) if value::as_dict(dictionary).is_some() => {
            (&rest[..rest.len() - 1], dictionary)
        }
        _ => (rest, value::dict_new(Vec::new())),
    };
    let entry = value::tuple_new(vec![function, value::tuple_new(call_args.to_vec()), kwargs]);
    let cleanups = value::instance_attr(instance, "_cleanups")
        .filter(|list| value::as_list(*list).is_some())
        .unwrap_or_else(|| {
            let list = value::list_new(Vec::new());
            value::instance_setattr(instance, "_cleanups".to_string(), list);
            list
        });
    let roots = heap::root_values(&[cleanups, entry]);
    container::list_append(cleanups, entry);
    heap::release_roots(roots);
    NONE
}

/// Runtime of the cleanup list: run every stored cleanup LIFO and return the
/// first error, if any. The error is already cleared from the pending slot.
fn do_cleanups(instance: i64) -> Option<i64> {
    let cleanups = value::instance_attr(instance, "_cleanups")?;
    value::as_list(cleanups)?;
    let mut first_error = None;
    while let Some(items) = value::as_list(cleanups) {
        let Some(&entry) = items.last() else {
            break;
        };
        let roots = heap::root_values(&[cleanups, entry]);
        container::delete_subscript(
            cleanups,
            encode_int(i64::try_from(items.len() - 1).unwrap_or(i64::MAX)),
        );
        if let Some(parts) = value::as_tuple(entry) {
            if let (Some(&function), Some(&call_args), Some(&kwargs)) =
                (parts.first(), parts.get(1), parts.get(2))
            {
                let kwargs = if value::as_dict(kwargs).is_some() {
                    keywords_from_dict(kwargs)
                } else {
                    Vec::new()
                };
                let positional: Vec<i64> = value::as_tuple(call_args)
                    .map(<[i64]>::to_vec)
                    .unwrap_or_default();
                if kwargs.is_empty() {
                    builtins::call(function, positional.len(), positional.as_ptr());
                } else {
                    // Native keyword ABI: interleave name/value pointers.
                    let words = keyword_words(&kwargs);
                    builtins::call_kw(
                        function,
                        positional.len(),
                        positional.as_ptr(),
                        kwargs.len(),
                        words.as_ptr(),
                    );
                }
            }
        }
        if exc::pending() {
            let error = take_pending();
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
        heap::release_roots(roots);
    }
    first_error
}

fn sub_test(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&message)) = (args.first(), args.get(1)) else {
        return missing_arguments("TestCase.subTest");
    };
    let params = match args.get(2) {
        Some(&dictionary) if value::as_dict(dictionary).is_some() => dictionary,
        _ => value::dict_new(Vec::new()),
    };
    let class = subtest_class();
    let context = value::instance_new(class);
    let roots = heap::root_values(&[context]);
    value::instance_setattr(context, "test_case".to_string(), instance);
    value::instance_setattr(context, "_message".to_string(), message);
    value::instance_setattr(context, "_params".to_string(), params);
    heap::release_roots(roots);
    context
}

fn test_case_id(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return missing_arguments("TestCase.id");
    };
    let class = value::as_instance(instance)
        .map(|state| state.class)
        .unwrap_or(instance);
    let name = value::instance_attr(instance, "_testMethodName")
        .and_then(value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    value::str_alloc(format!("{}.{name}", class_name(class)))
}

fn short_description(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return missing_arguments("TestCase.shortDescription");
    };
    let class = value::as_instance(instance)
        .map(|state| state.class)
        .unwrap_or(instance);
    let name = value::instance_attr(instance, "_testMethodName")
        .and_then(value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    let Some(function) = method_function(class, &name) else {
        return NONE;
    };
    let doc = value::function_doc(function);
    match value::as_str(doc) {
        Some(text) => {
            let first = text.lines().next().unwrap_or("").trim();
            if first.is_empty() {
                NONE
            } else {
                value::str_alloc(first.to_string())
            }
        }
        None => NONE,
    }
}

fn test_case_str(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return missing_arguments("TestCase.__str__");
    };
    let class = value::as_instance(instance)
        .map(|state| state.class)
        .unwrap_or(instance);
    let name = value::instance_attr(instance, "_testMethodName")
        .and_then(value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    value::str_alloc(format!("{name} ({}.{name})", class_name(class)))
}

fn test_case_repr(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return missing_arguments("TestCase.__repr__");
    };
    let class = value::as_instance(instance)
        .map(|state| state.class)
        .unwrap_or(instance);
    let name = value::instance_attr(instance, "_testMethodName")
        .and_then(value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    value::str_alloc(format!("<{}.{name}>", class_name(class)))
}

// -------------------------------------------------------------------------
// Assertions
// -------------------------------------------------------------------------

fn fail(args: &[i64]) -> i64 {
    let standard = args
        .get(1)
        .copied()
        .filter(|value| *value != NONE)
        .map(value::to_string)
        .unwrap_or_default();
    raise_assertion(standard)
}

fn assert_equal(args: &[i64]) -> i64 {
    let (Some(&first), Some(&second)) = (args.get(1), args.get(2)) else {
        return missing_arguments("assertEqual");
    };
    if container::equals(first, second) {
        return NONE;
    }
    raise_assertion(assertion_message(
        args,
        3,
        format!("{} != {}", value::repr(first), value::repr(second)),
    ))
}

fn assert_not_equal(args: &[i64]) -> i64 {
    let (Some(&first), Some(&second)) = (args.get(1), args.get(2)) else {
        return missing_arguments("assertNotEqual");
    };
    if !container::equals(first, second) {
        return NONE;
    }
    raise_assertion(assertion_message(
        args,
        3,
        format!("{} == {}", value::repr(first), value::repr(second)),
    ))
}

fn assert_true(args: &[i64], expected: bool) -> i64 {
    let Some(&expression) = args.get(1) else {
        return missing_arguments(if expected {
            "assertTrue"
        } else {
            "assertFalse"
        });
    };
    if value::truthy(expression) == expected {
        return NONE;
    }
    raise_assertion(assertion_message(
        args,
        2,
        format!(
            "{} is not {}",
            value::repr(expression),
            if expected { "true" } else { "false" }
        ),
    ))
}

fn assert_is(args: &[i64], expected: bool) -> i64 {
    let (Some(&first), Some(&second)) = (args.get(1), args.get(2)) else {
        return missing_arguments("assertIs");
    };
    if (first == second) == expected {
        return NONE;
    }
    raise_assertion(assertion_message(
        args,
        3,
        if expected {
            format!("{} is not {}", value::repr(first), value::repr(second))
        } else {
            format!("{} is {}", value::repr(first), value::repr(second))
        },
    ))
}

fn assert_is_none(args: &[i64], expected: bool) -> i64 {
    let Some(&expression) = args.get(1) else {
        return missing_arguments("assertIsNone");
    };
    let is_none = expression == NONE;
    if is_none == expected {
        return NONE;
    }
    raise_assertion(assertion_message(
        args,
        2,
        if expected {
            format!("{} is not None", value::repr(expression))
        } else {
            format!("{} is None", value::repr(expression))
        },
    ))
}

fn assert_in(args: &[i64], expected: bool) -> i64 {
    let (Some(&member), Some(&collection)) = (args.get(1), args.get(2)) else {
        return missing_arguments("assertIn");
    };
    let contained = ops::binop(ops::OP_IN, member, collection);
    if exc::pending() {
        return contained;
    }
    if value::truthy(contained) == expected {
        return NONE;
    }
    raise_assertion(assertion_message(
        args,
        3,
        if expected {
            format!("{} not in {}", value::repr(member), value::repr(collection))
        } else {
            format!("{} in {}", value::repr(member), value::repr(collection))
        },
    ))
}

fn assert_is_instance(args: &[i64], expected: bool) -> i64 {
    let (Some(&expression), Some(&class)) = (args.get(1), args.get(2)) else {
        return missing_arguments("assertIsInstance");
    };
    let matches = builtins::isinstance(expression, class);
    if exc::pending() {
        return matches;
    }
    if value::truthy(matches) == expected {
        return NONE;
    }
    raise_assertion(assertion_message(
        args,
        3,
        if expected {
            format!(
                "{} is not an instance of {}",
                value::repr(expression),
                value::repr(class)
            )
        } else {
            format!(
                "{} is an instance of {}",
                value::repr(expression),
                value::repr(class)
            )
        },
    ))
}

fn assert_order(args: &[i64], op: u32) -> i64 {
    let (Some(&first), Some(&second)) = (args.get(1), args.get(2)) else {
        return missing_arguments("assertLess");
    };
    let result = ops::binop(op, first, second);
    if exc::pending() {
        return result;
    }
    if value::truthy(result) {
        return NONE;
    }
    let relation = match op {
        ops::OP_GT => "not greater than",
        ops::OP_GE => "not greater than or equal to",
        ops::OP_LT => "not less than",
        _ => "not less than or equal to",
    };
    raise_assertion(assertion_message(
        args,
        3,
        format!("{} {relation} {}", value::repr(first), value::repr(second)),
    ))
}

fn assert_almost_equal(args: &[i64], expected: bool) -> i64 {
    let (Some(&first), Some(&second)) = (args.get(1), args.get(2)) else {
        return missing_arguments("assertAlmostEqual");
    };
    let Some(first) = numeric_value(first) else {
        return exc::raise_with_message(TYPE_ERROR, "first argument is not a number");
    };
    let Some(second) = numeric_value(second) else {
        return exc::raise_with_message(TYPE_ERROR, "second argument is not a number");
    };
    let places = args
        .get(3)
        .copied()
        .filter(|value| *value != NONE)
        .and_then(value::as_integer)
        .unwrap_or(7);
    let delta = args
        .get(5)
        .copied()
        .filter(|value| *value != NONE)
        .and_then(numeric_value);
    let difference = (first - second).abs();
    let close = match delta {
        Some(delta) => difference <= delta,
        None => {
            let threshold = 10f64.powi(-i32::try_from(places).unwrap_or(7));
            difference < threshold || (first.is_nan() && second.is_nan())
        }
    };
    if close == expected {
        return NONE;
    }
    raise_assertion(assertion_message(
        args,
        4,
        if expected {
            format!("{first} != {second} within {places} places")
        } else {
            format!("{first} == {second} within {places} places")
        },
    ))
}

fn assert_regex(args: &[i64], expected: bool) -> i64 {
    let (Some(&text), Some(&pattern)) = (args.get(1), args.get(2)) else {
        return missing_arguments("assertRegex");
    };
    let Some(text) = value::as_str(text) else {
        return exc::raise_with_message(TYPE_ERROR, "expected string or bytes-like object");
    };
    let search_args = vec![pattern, value::str_alloc(text.to_string())];
    let found = crate::modules::re::call("search", &search_args);
    if exc::pending() {
        return found;
    }
    if value::truthy(found) == expected {
        return NONE;
    }
    raise_assertion(assertion_message(
        args,
        3,
        format!(
            "{} does not match {}",
            value::repr(args[1]),
            value::repr(pattern)
        ),
    ))
}

fn assert_count_equal(args: &[i64]) -> i64 {
    let (Some(&first), Some(&second)) = (args.get(1), args.get(2)) else {
        return missing_arguments("assertCountEqual");
    };
    let first_items = collect_items(first);
    let second_items = collect_items(second);
    if first_items.is_some() && second_items.is_some() {
        let mut left = first_items.unwrap_or_default();
        let mut right = second_items.unwrap_or_default();
        let mut index = 0;
        while index < left.len() {
            if let Some(position) = right
                .iter()
                .position(|item| container::equals(*item, left[index]))
            {
                right.remove(position);
                left.remove(index);
            } else {
                index += 1;
            }
        }
        if left.is_empty() && right.is_empty() {
            return NONE;
        }
    }
    raise_assertion(assertion_message(
        args,
        3,
        format!(
            "Counts differ: {} != {}",
            value::repr(first),
            value::repr(second)
        ),
    ))
}

fn assert_raises(args: &[i64], with_regex: bool) -> i64 {
    let (Some(&instance), Some(&expected)) = (args.first(), args.get(1)) else {
        return missing_arguments(if with_regex {
            "assertRaisesRegex"
        } else {
            "assertRaises"
        });
    };
    let (callable, regex) = if with_regex {
        let Some(&regex) = args.get(2) else {
            return missing_arguments("assertRaisesRegex");
        };
        let mutable = args.get(3).copied().unwrap_or(NONE);
        (if mutable == NONE { NONE } else { mutable }, Some(regex))
    } else {
        (args.get(2).copied().unwrap_or(NONE), None)
    };
    if callable != NONE {
        let call_args = if with_regex { &args[4..] } else { &args[2..] };
        let (positional, keywords) = split_call_kwargs(call_args);
        if keywords.is_empty() {
            builtins::call(callable, positional.len(), positional.as_ptr());
        } else {
            let words = keyword_words(&keywords);
            builtins::call_kw(
                callable,
                positional.len(),
                positional.as_ptr(),
                keywords.len(),
                words.as_ptr(),
            );
        }
        if !exc::pending() {
            return raise_assertion(missing_exception_message(expected));
        }
        let error = take_pending();
        let matches = builtins::isinstance(error, expected);
        if exc::pending() || !value::truthy(matches) {
            // Re-raise the unexpected exception unchanged.
            return exc::raise_value(error);
        }
        if let Some(regex) = regex {
            if !regex_matches(regex, error) {
                return exc::raise_value(error);
            }
        }
        return NONE;
    }
    let class = raises_context_class();
    let context = value::instance_new(class);
    let roots = heap::root_values(&[context]);
    value::instance_setattr(context, "test_case".to_string(), instance);
    value::instance_setattr(context, "expected".to_string(), expected);
    value::instance_setattr(context, "expected_regex".to_string(), regex.unwrap_or(NONE));
    heap::release_roots(roots);
    let _ = with_regex;
    context
}

/// Splits the synthetic trailing keyword dict appended by [`call_kw`] from the
/// positional callable arguments.
fn split_call_kwargs(args: &[i64]) -> (Vec<i64>, Vec<(String, i64)>) {
    match args.last() {
        Some(&dictionary) if value::as_dict(dictionary).is_some() => (
            args[..args.len() - 1].to_vec(),
            keywords_from_dict(dictionary),
        ),
        _ => (args.to_vec(), Vec::new()),
    }
}

fn assert_has_attr(args: &[i64], expected: bool) -> i64 {
    let (Some(&object), Some(&name_value)) = (args.get(1), args.get(2)) else {
        return missing_arguments("assertHasAttr");
    };
    let Some(name) = value::as_str(name_value) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "attribute name must be string, not '{}'",
                value::type_name(name_value)
            ),
        );
    };
    let name = name.to_string();
    let result = methods::getattr(object, name.as_ptr(), name.len());
    if !exc::pending() {
        if !expected {
            raise_assertion(assertion_message(
                args,
                3,
                format!("{} has attribute {name:?}", value::repr(object)),
            ))
        } else {
            NONE
        }
    } else {
        if exc::pending_tag() != i64::from(ATTRIBUTE_ERROR) {
            return result;
        }
        exc::clear();
        if expected {
            raise_assertion(assertion_message(
                args,
                3,
                format!("{} has no attribute {name:?}", value::repr(object)),
            ))
        } else {
            NONE
        }
    }
}

fn raise_assertion(message: String) -> i64 {
    exc::raise_with_message(ASSERTION_ERROR, &message)
}

fn assertion_message(args: &[i64], index: usize, standard: String) -> String {
    match args.get(index).copied() {
        Some(message) if message != NONE => match value::as_str(message) {
            Some(text) => format!("{text}\n{standard}"),
            None => format!("{}\n{standard}", value::repr(message)),
        },
        _ => standard,
    }
}

// -------------------------------------------------------------------------
// TestResult
// -------------------------------------------------------------------------

fn new_test_result() -> i64 {
    builtins::call(test_result_class(), 0, std::ptr::null())
}

fn test_result_method(method: &str, args: &[i64]) -> i64 {
    match method {
        "__init__" => result_init(args),
        "startTest" => {
            let Some(&result) = args.first() else {
                return missing_arguments("TestResult.startTest");
            };
            let count = result_integer(result, "testsRun").unwrap_or(0) + 1;
            value::instance_setattr(result, "testsRun".to_string(), encode_int(count));
            NONE
        }
        "stopTest" | "startTestRun" | "stopTestRun" | "addSuccess" | "printErrors" => NONE,
        "addError" => result_add_pair(args, "errors"),
        "addFailure" => result_add_pair(args, "failures"),
        "addExpectedFailure" => result_add_pair(args, "expectedFailures"),
        "addSubTest" => result_add_subtest(args),
        "addSkip" => {
            let (Some(&result), Some(&test), Some(&reason)) =
                (args.first(), args.get(1), args.get(2))
            else {
                return missing_arguments("TestResult.addSkip");
            };
            let reason = value::str_alloc(reason_text(reason));
            append_pair(result, "skipped", test, reason)
        }
        "addUnexpectedSuccess" => {
            let (Some(&result), Some(&test)) = (args.first(), args.get(1)) else {
                return missing_arguments("TestResult.addUnexpectedSuccess");
            };
            append_value(result, "unexpectedSuccesses", test)
        }
        "addDuration" => {
            let (Some(&result), Some(&test), Some(&elapsed)) =
                (args.first(), args.get(1), args.get(2))
            else {
                return missing_arguments("TestResult.addDuration");
            };
            let label = value::str_alloc(value::to_string(test));
            append_pair(result, "collectedDurations", label, elapsed)
        }
        "wasSuccessful" => {
            let Some(&result) = args.first() else {
                return missing_arguments("TestResult.wasSuccessful");
            };
            value::bool_value(
                result_list_len(result, "failures") == 0
                    && result_list_len(result, "errors") == 0
                    && result_list_len(result, "unexpectedSuccesses") == 0,
            )
        }
        "stop" => {
            let Some(&result) = args.first() else {
                return missing_arguments("TestResult.stop");
            };
            value::instance_setattr(result, "shouldStop".to_string(), TRUE);
            NONE
        }
        "__repr__" => {
            let Some(&result) = args.first() else {
                return missing_arguments("TestResult.__repr__");
            };
            value::str_alloc(format!(
                "<TestResult run={} errors={} failures={}>",
                result_integer(result, "testsRun").unwrap_or(0),
                result_list_len(result, "errors"),
                result_list_len(result, "failures")
            ))
        }
        "_exc_info_to_string" => match args.get(1).copied() {
            Some(error) => value::str_alloc(error_text(error)),
            None => value::str_alloc(String::new()),
        },
        _ => missing_method("TestResult", method),
    }
}

fn result_init(args: &[i64]) -> i64 {
    let Some(&result) = args.first() else {
        return missing_arguments("TestResult.__init__");
    };
    let roots = heap::root_values(&[result]);
    for name in [
        "failures",
        "errors",
        "skipped",
        "expectedFailures",
        "unexpectedSuccesses",
        "collectedDurations",
    ] {
        value::instance_setattr(result, name.to_string(), value::list_new(Vec::new()));
    }
    value::instance_setattr(result, "testsRun".to_string(), encode_int(0));
    value::instance_setattr(result, "shouldStop".to_string(), FALSE);
    value::instance_setattr(result, "failfast".to_string(), FALSE);
    value::instance_setattr(result, "buffer".to_string(), FALSE);
    value::instance_setattr(result, "tb_locals".to_string(), FALSE);
    value::instance_setattr(
        result,
        "stream".to_string(),
        args.get(1).copied().unwrap_or(NONE),
    );
    value::instance_setattr(
        result,
        "descriptions".to_string(),
        args.get(2).copied().unwrap_or(TRUE),
    );
    value::instance_setattr(
        result,
        "verbosity".to_string(),
        args.get(3).copied().unwrap_or_else(|| encode_int(1)),
    );
    heap::release_roots(roots);
    NONE
}

fn result_add_pair(args: &[i64], list: &str) -> i64 {
    let (Some(&result), Some(&test), Some(&error)) = (args.first(), args.get(1), args.get(2))
    else {
        return missing_arguments("TestResult.addError");
    };
    let text = value::str_alloc(error_text(error));
    append_pair(result, list, test, text)
}

fn result_add_subtest(args: &[i64]) -> i64 {
    let (Some(&result), Some(&subtest), Some(&error)) = (args.first(), args.get(2), args.get(3))
    else {
        return missing_arguments("TestResult.addSubTest");
    };
    if error == NONE {
        return NONE;
    }
    let test = args.get(1).copied().unwrap_or(NONE);
    let list = if error_is_failure(error, test) {
        "failures"
    } else {
        "errors"
    };
    let text = value::str_alloc(error_text(error));
    append_pair(result, list, subtest, text)
}

fn error_is_failure(error: i64, test: i64) -> bool {
    let failure_class = value::as_instance(test)
        .map(|state| state.class)
        .and_then(
            |class| match value::class_lookup(class, "failureException") {
                Some(value::ClassEntry::Attr(item)) => Some(item),
                _ => None,
            },
        )
        .unwrap_or_else(|| exc::exception_class(ASSERTION_ERROR));
    let error_class = match value::as_tuple(error).and_then(|items| items.first().copied()) {
        Some(class) if value::as_class(class).is_some() => class,
        _ => match value::as_tuple(error).and_then(|items| items.get(1).copied()) {
            Some(value) => value::type_class(value),
            None => return false,
        },
    };
    value::is_subclass_of(error_class, failure_class)
}

fn append_pair(result: i64, list: &str, first: i64, second: i64) -> i64 {
    let entry = value::tuple_new(vec![first, second]);
    append_value(result, list, entry)
}

fn append_value(result: i64, list: &str, entry: i64) -> i64 {
    let target = value::instance_attr(result, list)
        .filter(|value| value::as_list(*value).is_some())
        .unwrap_or_else(|| {
            let created = value::list_new(Vec::new());
            value::instance_setattr(result, list.to_string(), created);
            created
        });
    let roots = heap::root_values(&[target, entry]);
    container::list_append(target, entry);
    heap::release_roots(roots);
    NONE
}

fn result_list_len(result: i64, list: &str) -> usize {
    value::instance_attr(result, list)
        .and_then(value::as_list)
        .map_or(0, <[i64]>::len)
}

fn result_integer(result: i64, name: &str) -> Option<i64> {
    value::instance_attr(result, name).and_then(value::as_integer)
}

// -------------------------------------------------------------------------
// Text runner
// -------------------------------------------------------------------------

fn runner_method(method: &str, args: &[i64]) -> i64 {
    match method {
        "__init__" => {
            let Some(&runner) = args.first() else {
                return missing_arguments("TextTestRunner.__init__");
            };
            let roots = heap::root_values(&[runner]);
            value::instance_setattr(
                runner,
                "stream".to_string(),
                args.get(1).copied().unwrap_or(NONE),
            );
            value::instance_setattr(
                runner,
                "descriptions".to_string(),
                args.get(2).copied().unwrap_or(TRUE),
            );
            value::instance_setattr(
                runner,
                "verbosity".to_string(),
                args.get(3).copied().unwrap_or_else(|| encode_int(1)),
            );
            value::instance_setattr(
                runner,
                "failfast".to_string(),
                args.get(4).copied().unwrap_or(FALSE),
            );
            value::instance_setattr(
                runner,
                "buffer".to_string(),
                args.get(5).copied().unwrap_or(FALSE),
            );
            value::instance_setattr(
                runner,
                "resultclass".to_string(),
                args.get(6).copied().unwrap_or(NONE),
            );
            value::instance_setattr(
                runner,
                "tb_locals".to_string(),
                args.get(7).copied().unwrap_or(FALSE),
            );
            heap::release_roots(roots);
            NONE
        }
        "_makeResult" => {
            let Some(&runner) = args.first() else {
                return missing_arguments("TextTestRunner._makeResult");
            };
            let resultclass = value::instance_attr(runner, "resultclass")
                .filter(|value| value::as_class(*value).is_some());
            let class = resultclass.unwrap_or_else(text_test_result_class);
            let mut call_args = Vec::new();
            if let Some(stream) = value::instance_attr(runner, "stream") {
                if stream != NONE {
                    call_args.push(stream);
                }
            }
            let result = builtins::call(class, call_args.len(), call_args.as_ptr());
            if let Some(failfast) = value::instance_attr(runner, "failfast") {
                value::instance_setattr(result, "failfast".to_string(), failfast);
            }
            if let Some(buffer) = value::instance_attr(runner, "buffer") {
                value::instance_setattr(result, "buffer".to_string(), buffer);
            }
            result
        }
        "run" => runner_run(args),
        _ => missing_method("TextTestRunner", method),
    }
}

fn runner_run(args: &[i64]) -> i64 {
    let (Some(&runner), Some(&test)) = (args.first(), args.get(1)) else {
        return missing_arguments("TextTestRunner.run");
    };
    let result = runner_method("_makeResult", &[runner]);
    if exc::pending() {
        return NONE;
    }
    call_method(result, "startTestRun", &[]);
    run_test_item(test, result);
    if exc::pending() {
        return NONE;
    }
    call_method(result, "stopTestRun", &[]);
    if !value::truthy(call_method(result, "wasSuccessful", &[])) {
        call_method(result, "printErrors", &[]);
        print_failures(result);
    }
    let verbosity = value::instance_attr(runner, "verbosity")
        .and_then(value::as_integer)
        .unwrap_or(1);
    if verbosity > 0 {
        println!(
            "Ran {} test(s)",
            result_integer(result, "testsRun").unwrap_or(0)
        );
    }
    result
}

/// Runs a test case, suite or any callable test item against `result`.
fn run_test_item(test: i64, result: i64) -> i64 {
    if value::as_instance(test).is_some() && has_method(test, "run") {
        return call_method(test, "run", &[result]);
    }
    let argv = [result];
    builtins::call(test, 1, argv.as_ptr())
}

// -------------------------------------------------------------------------
// TestLoader
// -------------------------------------------------------------------------

fn loader_method(method: &str, args: &[i64]) -> i64 {
    match method {
        "__init__" => NONE,
        "getTestCaseNames" => {
            let Some(&_loader) = args.first() else {
                return missing_arguments("TestLoader.getTestCaseNames");
            };
            let Some(&class) = args.get(1) else {
                return missing_arguments("TestLoader.getTestCaseNames");
            };
            let names = sorted_test_method_names(class);
            let items = names
                .into_iter()
                .map(value::str_alloc)
                .collect::<Vec<i64>>();
            value::list_new(items)
        }
        "loadTestsFromTestCase" => {
            let Some(&test_class) = args.get(1) else {
                return missing_arguments("TestLoader.loadTestsFromTestCase");
            };
            if value::as_class(test_class).is_none() {
                return exc::raise_with_message(TYPE_ERROR, "testCaseClass must be a class");
            }
            let mut tests = Vec::new();
            for name in sorted_test_method_names(test_class) {
                let argument = value::str_alloc(name);
                let roots = heap::root_values(&[argument]);
                let instance =
                    builtins::call(test_class, 1, std::slice::from_ref(&argument).as_ptr());
                heap::release_roots(roots);
                if exc::pending() {
                    return NONE;
                }
                tests.push(instance);
            }
            if tests.is_empty() && value::class_lookup(test_class, "runTest").is_some() {
                let instance = builtins::call(test_class, 0, std::ptr::null());
                if exc::pending() {
                    return NONE;
                }
                tests.push(instance);
            }
            make_suite(tests)
        }
        "loadTestsFromModule" => {
            let Some(&module) = args.get(1) else {
                return missing_arguments("TestLoader.loadTestsFromModule");
            };
            let Some(state) = value::as_module(module) else {
                return exc::raise_with_message(TYPE_ERROR, "module argument must be a module");
            };
            let name = state.name.clone();
            let dictionary = crate::modules::module_dict(&name);
            let tests = load_module_tests(dictionary, test_case());
            make_suite(tests)
        }
        "loadTestsFromNames" | "loadTestsFromName" => {
            // Resolve names that denote a registered test class; anything else
            // yields an empty suite instead of fabricating a test.
            let Some(&names) = args.get(1) else {
                return missing_arguments("TestLoader.loadTestsFromName");
            };
            let mut tests = Vec::new();
            if let Some(text) = value::as_str(names) {
                if let Some(class) = find_registered_class(text) {
                    tests.extend(load_class_tests(class));
                }
            } else if let Some(items) = value::as_list(names) {
                for &item in items {
                    if let Some(text) = value::as_str(item) {
                        if let Some(class) = find_registered_class(text) {
                            tests.extend(load_class_tests(class));
                        }
                    }
                }
            }
            make_suite(tests)
        }
        "discover" => make_suite(Vec::new()),
        "_makeSuite" => {
            let Some(&tests) = args.get(1) else {
                return missing_arguments("TestLoader._makeSuite");
            };
            make_suite(collect_items(tests).unwrap_or_default())
        }
        _ => missing_method("TestLoader", method),
    }
}

fn sorted_test_method_names(class: i64) -> Vec<String> {
    let mut names = test_method_names(class);
    names.retain(|name| name.starts_with("test"));
    names.sort();
    names
}

fn load_class_tests(class: i64) -> Vec<i64> {
    let mut tests = Vec::new();
    for name in sorted_test_method_names(class) {
        let argument = value::str_alloc(name);
        let roots = heap::root_values(&[argument]);
        let instance = builtins::call(class, 1, std::slice::from_ref(&argument).as_ptr());
        heap::release_roots(roots);
        if exc::pending() {
            break;
        }
        tests.push(instance);
    }
    tests
}

fn load_module_tests(dictionary: i64, test_case_class: i64) -> Vec<i64> {
    let mut tests = Vec::new();
    let Some(pairs) = value::as_dict(dictionary) else {
        return tests;
    };
    let mut seen = Vec::new();
    for &(_, item) in pairs {
        if seen.contains(&item) || value::as_class(item).is_none() {
            continue;
        }
        seen.push(item);
        if item != test_case_class && value::is_subclass_of(item, test_case_class) {
            tests.extend(load_class_tests(item));
        }
    }
    tests
}

fn find_registered_class(name: &str) -> Option<i64> {
    // `package.Class` and `Class` both denote the registered class; a
    // `Class.method` name selects the owning class.
    let short = name.rsplit('.').next().unwrap_or(name);
    registry().iter().copied().find(|class| {
        let class_name = value::class_display_name(*class).unwrap_or("");
        class_name == name || class_name == short
    })
}

/// Builds a `TestSuite` from a list of tests.
fn make_suite(tests: Vec<i64>) -> i64 {
    let class = crate::modules::unittest_test_suite_class();
    let instance = value::instance_new(class);
    let list = value::list_new(tests);
    let roots = heap::root_values(&[instance, list]);
    value::instance_setattr(instance, "_tests".to_string(), list);
    heap::release_roots(roots);
    instance
}

// -------------------------------------------------------------------------
// TestProgram
// -------------------------------------------------------------------------

fn program_method(method: &str, _args: &[i64]) -> i64 {
    match method {
        "__init__" | "runTests" => {
            run_main();
            NONE
        }
        "parseArgs" => NONE,
        _ => missing_method("TestProgram", method),
    }
}

// -------------------------------------------------------------------------
// _ErrorHolder / TestList / _SubTest / _AssertRaisesContext
// -------------------------------------------------------------------------

fn error_holder_method(method: &str, args: &[i64]) -> i64 {
    match method {
        "__init__" => {
            let (Some(&instance), Some(&description)) = (args.first(), args.get(1)) else {
                return missing_arguments("_ErrorHolder.__init__");
            };
            value::instance_setattr(instance, "description".to_string(), description);
            NONE
        }
        "id" | "__str__" => args
            .first()
            .and_then(|instance| value::instance_attr(*instance, "description"))
            .unwrap_or(NONE),
        "shortDescription" => NONE,
        "__repr__" => {
            let description = args
                .first()
                .and_then(|instance| value::instance_attr(*instance, "description"))
                .unwrap_or(NONE);
            value::str_alloc(format!(
                "<ErrorHolder description={}>",
                value::repr(description)
            ))
        }
        "run" | "__call__" => NONE,
        _ => missing_method("_ErrorHolder", method),
    }
}

fn test_list_method(method: &str, args: &[i64]) -> i64 {
    match method {
        "__init__" => {
            let Some(&instance) = args.first() else {
                return missing_arguments("TestList.__init__");
            };
            let tests = match args.get(1).copied() {
                Some(NONE) | None => value::list_new(Vec::new()),
                Some(given) => collect_items(given)
                    .map(value::list_new)
                    .unwrap_or_else(|| value::list_new(Vec::new())),
            };
            let roots = heap::root_values(&[instance, tests]);
            value::instance_setattr(instance, "_tests".to_string(), tests);
            heap::release_roots(roots);
            NONE
        }
        "__repr__" => {
            let tests = args
                .first()
                .and_then(|instance| value::instance_attr(*instance, "_tests"))
                .unwrap_or(NONE);
            value::str_alloc(format!("<TestList tests={}>", value::repr(tests)))
        }
        "__iter__" => {
            let tests = args
                .first()
                .and_then(|instance| value::instance_attr(*instance, "_tests"))
                .unwrap_or(NONE);
            iter::iter_new(tests)
        }
        "__len__" => encode_int(
            args.first()
                .and_then(|instance| value::instance_attr(*instance, "_tests"))
                .and_then(value::as_list)
                .map_or(0, |items| items.len() as i64),
        ),
        "__eq__" => {
            let (Some(&instance), Some(&other)) = (args.first(), args.get(1)) else {
                return missing_arguments("TestList.__eq__");
            };
            let left = value::instance_attr(instance, "_tests").unwrap_or(NONE);
            let right = if value::as_instance(other).is_some() {
                value::instance_attr(other, "_tests").unwrap_or(other)
            } else {
                other
            };
            value::bool_value(container::equals(left, right))
        }
        "__add__" => {
            let (Some(&instance), Some(&other)) = (args.first(), args.get(1)) else {
                return missing_arguments("TestList.__add__");
            };
            let mut tests = value::instance_attr(instance, "_tests")
                .and_then(value::as_list)
                .map(<[i64]>::to_vec)
                .unwrap_or_default();
            tests.extend(collect_items(other).unwrap_or_default());
            make_test_list(tests)
        }
        _ => missing_method("TestList", method),
    }
}

fn make_test_list(tests: Vec<i64>) -> i64 {
    let instance = value::instance_new(test_list_class());
    let list = value::list_new(tests);
    let roots = heap::root_values(&[instance, list]);
    value::instance_setattr(instance, "_tests".to_string(), list);
    heap::release_roots(roots);
    instance
}

fn subtest_method(method: &str, args: &[i64]) -> i64 {
    match method {
        "__init__" => {
            let (Some(&instance), Some(&test), Some(&message)) =
                (args.first(), args.get(1), args.get(2))
            else {
                return missing_arguments("_SubTest.__init__");
            };
            value::instance_setattr(instance, "test_case".to_string(), test);
            value::instance_setattr(instance, "_message".to_string(), message);
            value::instance_setattr(instance, "_params".to_string(), value::dict_new(Vec::new()));
            NONE
        }
        "__enter__" => args.first().copied().unwrap_or(NONE),
        "__exit__" => subtest_exit(args),
        "__str__" => {
            let message = args
                .first()
                .and_then(|instance| value::instance_attr(*instance, "_message"))
                .unwrap_or(NONE);
            if message == NONE {
                value::str_alloc("subTest".to_string())
            } else {
                value::str_alloc(format!("subTest {}", value::to_string(message)))
            }
        }
        _ => missing_method("_SubTest", method),
    }
}

fn subtest_exit(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&exc_type)) = (args.first(), args.get(1)) else {
        return missing_arguments("_SubTest.__exit__");
    };
    if exc_type == NONE {
        return FALSE;
    }
    let test = value::instance_attr(instance, "test_case").unwrap_or(NONE);
    let result = value::instance_attr(test, "_unittest_result").unwrap_or(NONE);
    if result == NONE {
        return FALSE;
    }
    let error = match args.get(2).copied() {
        Some(value) if value != NONE => value,
        _ => exc::pending_value(),
    };
    if error == NONE {
        return FALSE;
    }
    exc::clear();
    let tuple = error_tuple(error);
    call_method(result, "addSubTest", &[test, instance, tuple]);
    TRUE
}

fn raises_context_method(method: &str, args: &[i64]) -> i64 {
    match method {
        "__init__" => {
            let (Some(&instance), Some(&test), Some(&expected)) =
                (args.first(), args.get(1), args.get(2))
            else {
                return missing_arguments("_AssertRaisesContext.__init__");
            };
            value::instance_setattr(instance, "test_case".to_string(), test);
            value::instance_setattr(instance, "expected".to_string(), expected);
            value::instance_setattr(
                instance,
                "expected_regex".to_string(),
                args.get(3).copied().unwrap_or(NONE),
            );
            NONE
        }
        "__enter__" => args.first().copied().unwrap_or(NONE),
        "__exit__" => raises_context_exit(args),
        _ => missing_method("_AssertRaisesContext", method),
    }
}

fn raises_context_exit(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&exc_type)) = (args.first(), args.get(1)) else {
        return missing_arguments("_AssertRaisesContext.__exit__");
    };
    let expected = value::instance_attr(instance, "expected").unwrap_or(NONE);
    if exc_type == NONE {
        raise_assertion(missing_exception_message(expected));
        return FALSE;
    }
    let error = match args.get(2).copied() {
        Some(value) if value != NONE => value,
        _ => exc::pending_value(),
    };
    if !value::truthy(builtins::isinstance(error, expected)) {
        // Not the expected type: let the original exception propagate.
        return FALSE;
    }
    if let Some(regex) = value::instance_attr(instance, "expected_regex") {
        if regex != NONE && !regex_matches(regex, error) {
            raise_assertion(format!("Regex didn't match: {}", value::repr(regex)));
            return FALSE;
        }
    }
    value::instance_setattr(instance, "exception".to_string(), error);
    TRUE
}

// -------------------------------------------------------------------------
// unittest.util
// -------------------------------------------------------------------------

/// Dispatches a native `unittest.util` function or class method.
pub(crate) fn util_call(function: &str, args: &[i64]) -> i64 {
    match function {
        "strclass" => {
            let Some(&class) = args.first() else {
                return missing_arguments("strclass");
            };
            value::str_alloc(class_name(class))
        }
        "safe_repr" => {
            let Some(&object) = args.first() else {
                return missing_arguments("safe_repr");
            };
            let short = args.get(1).copied().is_some_and(value::truthy);
            let text = value::repr(object);
            if short && text.len() >= 80 {
                value::str_alloc(format!("{} [truncated]...", &text[..80]))
            } else {
                value::str_alloc(text)
            }
        }
        "sorted_list_difference" => sorted_list_difference(args),
        "unorderable_list_difference" => unorderable_list_difference(args),
        "three_way_cmp" => three_way_cmp(args),
        "_common_shorten_repr" => common_shorten_repr(args),
        _ => {
            if let Some(method) = function.strip_prefix("_ErrorHolder.") {
                return error_holder_method(method, args);
            }
            if let Some(method) = function.strip_prefix("TestList.") {
                return test_list_method(method, args);
            }
            exc::raise_with_message(
                ATTRIBUTE_ERROR,
                &format!("module 'unittest.util' has no attribute '{function}'"),
            )
        }
    }
}

fn three_way_cmp(args: &[i64]) -> i64 {
    let (Some(&left), Some(&right)) = (args.first(), args.get(1)) else {
        return missing_arguments("three_way_cmp");
    };
    let greater = ops::binop(ops::OP_GT, left, right);
    if exc::pending() {
        return greater;
    }
    let less = ops::binop(ops::OP_LT, left, right);
    if exc::pending() {
        return less;
    }
    let greater = value::truthy(greater);
    let less = value::truthy(less);
    encode_int(i64::from(greater) - i64::from(less))
}

fn sorted_list_difference(args: &[i64]) -> i64 {
    let (Some(&expected), Some(&actual)) = (args.first(), args.get(1)) else {
        return missing_arguments("sorted_list_difference");
    };
    let Some(expected) = collect_items(expected) else {
        return exc::raise_with_message(TYPE_ERROR, "expected must be a sequence");
    };
    let Some(actual) = collect_items(actual) else {
        return exc::raise_with_message(TYPE_ERROR, "actual must be a sequence");
    };
    let mut missing = Vec::new();
    let mut unexpected = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    loop {
        if i >= expected.len() || j >= actual.len() {
            missing.extend_from_slice(&expected[i..]);
            unexpected.extend_from_slice(&actual[j..]);
            break;
        }
        let e = expected[i];
        let a = actual[j];
        if container::equals(e, a) {
            i += 1;
            while i < expected.len() && container::equals(expected[i], e) {
                i += 1;
            }
            j += 1;
            while j < actual.len() && container::equals(actual[j], a) {
                j += 1;
            }
        } else {
            let e_less = ops::binop(ops::OP_LT, e, a);
            if exc::pending() {
                return NONE;
            }
            if value::truthy(e_less) {
                missing.push(e);
                i += 1;
            } else {
                unexpected.push(a);
                j += 1;
            }
        }
    }
    let missing = value::list_new(missing);
    let unexpected = value::list_new(unexpected);
    value::tuple_new(vec![missing, unexpected])
}

fn unorderable_list_difference(args: &[i64]) -> i64 {
    let (Some(&expected), Some(&actual)) = (args.first(), args.get(1)) else {
        return missing_arguments("unorderable_list_difference");
    };
    let mut expected_items = collect_items(expected).unwrap_or_default();
    let mut actual_items = collect_items(actual).unwrap_or_default();
    let mut missing = Vec::new();
    while let Some(item) = expected_items.pop() {
        match actual_items
            .iter()
            .position(|candidate| container::equals(*candidate, item))
        {
            Some(index) => {
                actual_items.remove(index);
            }
            None => missing.push(item),
        }
    }
    value::tuple_new(vec![
        value::list_new(missing),
        value::list_new(actual_items),
    ])
}

fn common_shorten_repr(args: &[i64]) -> i64 {
    let reprs: Vec<String> = args.iter().map(|value| value::repr(*value)).collect();
    let max_len = reprs.iter().map(String::len).max().unwrap_or(0);
    if max_len <= 80 {
        return value::tuple_new(
            reprs
                .into_iter()
                .map(value::str_alloc)
                .collect::<Vec<i64>>(),
        );
    }
    // Keep the common prefix and shorten the differing tails, mirroring
    // CPython's display budget closely enough for failure messages.
    let prefix = common_prefix(&reprs);
    let items = reprs
        .into_iter()
        .map(|text| {
            let rest = &text[prefix.len()..];
            if rest.len() > 12 {
                value::str_alloc(format!(
                    "{}[{} chars]{}",
                    &text[..prefix.len().min(5)],
                    rest.len() - 5,
                    &rest[rest.len().saturating_sub(5)..]
                ))
            } else {
                value::str_alloc(text)
            }
        })
        .collect::<Vec<i64>>();
    value::tuple_new(items)
}

fn common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut length = first.len();
    for item in &items[1..] {
        length = length.min(item.len());
        while length > 0 && first.as_bytes()[..length] != item.as_bytes()[..length] {
            length -= 1;
        }
    }
    first[..length].to_string()
}

// -------------------------------------------------------------------------
// Runner internals
// -------------------------------------------------------------------------

/// Names of the `test*` methods of `class` and its bases, declaration order,
/// without duplicates.
fn test_method_names(class: i64) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut current = class;
    while let Some(state) = value::as_class(current) {
        for (name, _, _) in &state.methods {
            if name.starts_with("test") && !names.iter().any(|existing| existing == name) {
                names.push(name.clone());
            }
        }
        current = state.base;
    }
    names
}

fn method_function(class: i64, name: &str) -> Option<i64> {
    match value::class_lookup(class, name)? {
        value::ClassEntry::Method(_, function) => Some(function),
        value::ClassEntry::Attr(item) => Some(item),
    }
}

fn has_method(instance: i64, name: &str) -> bool {
    value::as_instance(instance)
        .map(|state| value::class_lookup(state.class, name).is_some())
        .unwrap_or(false)
}

fn class_attr_truthy(class: i64, name: &str) -> bool {
    match value::class_lookup(class, name) {
        Some(value::ClassEntry::Attr(item)) => value::truthy(item),
        _ => false,
    }
}

fn class_attr_text(class: i64, name: &str) -> Option<String> {
    match value::class_lookup(class, name) {
        Some(value::ClassEntry::Attr(item)) => Some(reason_text(item)),
        _ => None,
    }
}

fn attr_truthy(function: i64, name: &str) -> bool {
    value::function_attribute(function, name).is_some_and(value::truthy)
}

fn function_attr_text(function: i64, name: &str) -> Option<String> {
    value::function_attribute(function, name).map(reason_text)
}

fn class_name(class: i64) -> String {
    value::class_display_name(class)
        .map(str::to_string)
        .or_else(|| value::as_class(class).map(|state| state.name.clone()))
        .unwrap_or_else(|| "TestCase".to_string())
}

/// Runs every registered test class' `test*` methods against one fresh
/// `TestResult`, printing a summary and raising on failure.
fn run_main() -> i64 {
    let classes = registry().clone();
    let result = new_test_result();
    call_method(result, "startTestRun", &[]);
    for class in classes {
        for name in test_method_names(class) {
            let argument = value::str_alloc(name.clone());
            let roots = heap::root_values(&[argument]);
            let instance = builtins::call(class, 1, std::slice::from_ref(&argument).as_ptr());
            heap::release_roots(roots);
            if exc::pending() {
                let error = take_pending();
                let holder = value::instance_new(error_holder_class());
                let description = value::str_alloc(format!("{}.{}", class_name(class), name));
                let holder_roots = heap::root_values(&[holder, description]);
                value::instance_setattr(holder, "description".to_string(), description);
                call_method(result, "addError", &[holder, error_tuple(error)]);
                heap::release_roots(holder_roots);
                continue;
            }
            test_case_run(&[instance, result]);
            if exc::pending() {
                // A result object that raised aborts the run.
                return NONE;
            }
            if value::instance_attr(result, "shouldStop").is_some_and(value::truthy) {
                break;
            }
        }
        if value::instance_attr(result, "shouldStop").is_some_and(value::truthy) {
            break;
        }
    }
    call_method(result, "stopTestRun", &[]);

    let tests_run = result_integer(result, "testsRun").unwrap_or(0);
    println!("Ran {tests_run} test(s)");
    print_failures(result);
    let failures = result_list_len(result, "failures")
        + result_list_len(result, "errors")
        + result_list_len(result, "unexpectedSuccesses");
    if failures > 0 {
        return raise_assertion(format!("{failures} test(s) failed"));
    }
    NONE
}

fn print_failures(result: i64) {
    for list in ["failures", "errors"] {
        let Some(items) = value::instance_attr(result, list).and_then(value::as_list) else {
            continue;
        };
        for &item in items {
            let Some(pair) = value::as_tuple(item) else {
                continue;
            };
            let test = pair.first().copied().unwrap_or(NONE);
            let text = pair.get(1).copied().and_then(value::as_str).unwrap_or("");
            let last = text
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("");
            eprintln!("FAILED {}: {last}", value::repr(test));
        }
    }
}

fn call_method(receiver: i64, name: &str, args: &[i64]) -> i64 {
    let argv = if args.is_empty() {
        std::ptr::null()
    } else {
        args.as_ptr()
    };
    methods::call(receiver, name.as_ptr(), name.len(), args.len(), argv)
}

fn take_pending() -> i64 {
    let value = exc::pending_value();
    exc::clear();
    value
}

fn error_tuple(error: i64) -> i64 {
    let class = value::type_class(error);
    value::tuple_new(vec![class, error, NONE])
}

fn error_text(error: i64) -> String {
    if error == NONE {
        return String::new();
    }
    if let Some(text) = exc::render_exception(error) {
        return text;
    }
    match value::exception_data(error) {
        Some(data) if data.msg.is_empty() => data.name.to_string(),
        Some(data) => format!("{}: {}", data.name, data.msg),
        None => value::repr(error),
    }
}

fn record_test_error(result: i64, instance: i64, error: i64, expecting: bool) {
    let tag = value::exception_data(error).map(|data| data.tag);
    if tag == Some(SKIPPED) {
        let why = value::exception_data(error)
            .map(|data| data.msg.clone())
            .unwrap_or_default();
        call_method(result, "addSkip", &[instance, value::str_alloc(why)]);
        return;
    }
    if expecting {
        call_method(
            result,
            "addExpectedFailure",
            &[instance, error_tuple(error)],
        );
        return;
    }
    if error_is_failure(error_tuple(error), instance) {
        call_method(result, "addFailure", &[instance, error_tuple(error)]);
    } else {
        call_method(result, "addError", &[instance, error_tuple(error)]);
    }
}

fn numeric_value(value_to_convert: i64) -> Option<f64> {
    value::as_float(value_to_convert)
        .or_else(|| value::as_integer(value_to_convert).map(|integer| integer as f64))
}

fn collect_items(iterable: i64) -> Option<Vec<i64>> {
    if let Some(items) = value::as_list(iterable) {
        return Some(items.to_vec());
    }
    if let Some(items) = value::as_tuple(iterable) {
        return Some(items.to_vec());
    }
    let iterator = iter::iter_of(iterable);
    if exc::pending() {
        return None;
    }
    let mut items = Vec::new();
    loop {
        let mut item = NONE;
        let status = iter::iter_next(iterator, &mut item);
        if exc::pending() {
            return None;
        }
        if status == 0 {
            break;
        }
        items.push(item);
    }
    Some(items)
}

fn keywords_dict(keywords: &[(String, i64)]) -> i64 {
    let dictionary = value::dict_new(Vec::new());
    let roots = heap::root_values(&[dictionary]);
    for (name, item) in keywords {
        let key = value::str_alloc(name.clone());
        let pair_roots = heap::root_values(&[key, *item]);
        container::dict_set(dictionary, key, *item);
        heap::release_roots(pair_roots);
    }
    heap::release_roots(roots);
    dictionary
}

fn keywords_from_dict(dictionary: i64) -> Vec<(String, i64)> {
    value::as_dict(dictionary)
        .map(|pairs| {
            pairs
                .iter()
                .filter_map(|(key, item)| value::as_str(*key).map(|name| (name.to_string(), *item)))
                .collect()
        })
        .unwrap_or_default()
}

fn keyword_words(keywords: &[(String, i64)]) -> Vec<i64> {
    let mut words = Vec::with_capacity(keywords.len() * 2);
    for (name, item) in keywords {
        words.push(value::str_alloc(name.clone()));
        words.push(*item);
    }
    words
}

fn regex_matches(pattern: i64, error: i64) -> bool {
    let text = value::str_alloc(value::to_string(error));
    let result = crate::modules::re::call("search", &[pattern, text]);
    !exc::pending() && value::truthy(result)
}

fn missing_exception_message(expected: i64) -> String {
    format!("{} not raised", value::repr(expected))
}

fn missing_method(class: &str, method: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("'{class}' object has no attribute '{method}'"),
    )
}

fn missing_arguments(function: &str) -> i64 {
    exc::raise_with_message(
        TYPE_ERROR,
        &format!("unittest.{function}() received too few arguments"),
    )
}

fn encode_int(value_to_encode: i64) -> i64 {
    value::encode_int(value_to_encode).unwrap_or(NONE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> i64 {
        value::encode_int(n).expect("tagged int")
    }

    fn registry_len() -> usize {
        registry().len()
    }

    #[test]
    fn test_case_has_the_expected_native_methods() {
        let case = test_case();
        assert_eq!(value::as_class(case).expect("class value").name, "TestCase");
        for name in [
            CASE_MARKER,
            "__init__",
            "run",
            "setUp",
            "tearDown",
            "addCleanup",
            "assertEqual",
            "assertRaises",
            "fail",
        ] {
            assert!(value::class_method(case, name).is_some(), "{name}");
        }
    }

    #[test]
    fn skip_if_false_is_the_identity_decorator() {
        exc::init();
        let identity = skip_if(&[FALSE, value::str_alloc("why".to_string())], false);
        // The identity callable returns the decorated item unchanged.
        let function = value::identity_function_new("test_identity");
        assert_eq!(builtins::call(identity, 1, [function].as_ptr()), function);
        assert!(!exc::pending());
    }

    #[test]
    fn assertion_helpers_pass_and_fail() {
        exc::init();
        assert_eq!(test_case_method("assertTrue", &[NONE, TRUE]), NONE);
        assert!(!exc::pending());

        exc::init();
        assert_eq!(test_case_method("assertFalse", &[NONE, FALSE]), NONE);
        assert!(!exc::pending());

        exc::init();
        assert_eq!(
            test_case_method("assertEqual", &[NONE, int(5), int(5)]),
            NONE
        );
        assert!(!exc::pending());

        exc::init();
        assert_eq!(test_case_method("assertIs", &[NONE, NONE, NONE]), NONE);
        assert!(!exc::pending());

        let list = value::list_new(vec![int(1), int(2), int(3)]);
        exc::init();
        assert_eq!(test_case_method("assertIn", &[NONE, int(2), list]), NONE);
        assert!(!exc::pending());

        exc::init();
        assert_eq!(test_case_method("assertTrue", &[NONE, FALSE]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(ASSERTION_ERROR));
        assert!(exc::pending_value() != NONE);
        exc::clear();

        exc::init();
        assert_eq!(
            test_case_method("fail", &[NONE, value::str_alloc("boom".to_string())]),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(ASSERTION_ERROR));
        exc::clear();
    }

    #[test]
    fn registry_and_runner() {
        reset();
        exc::init();
        assert_eq!(registry_len(), 0);

        let case = test_case();
        let passing = value::class_new(
            "PassingCase".to_string(),
            case,
            vec![(
                "test_ok".to_string(),
                value::native_function_new("unittest", "_case_marker", 1),
            )],
        );
        assert!(value::as_class(passing).is_some());
        assert_eq!(registry_len(), 1);

        assert_eq!(call("main", &[]), NONE);
        assert!(!exc::pending());

        let failing = value::class_new(
            "FailingCase".to_string(),
            test_case(),
            vec![(
                "test_bad".to_string(),
                value::native_function_new("unittest", "_fail", 2),
            )],
        );
        assert!(value::as_class(failing).is_some());
        assert_eq!(registry_len(), 2);

        assert_eq!(call("main", &[]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(ASSERTION_ERROR));
        assert!(value::to_string(exc::pending_value()).contains("1 test(s) failed"));

        exc::init();
        reset();
        assert_eq!(registry_len(), 0);
    }

    #[test]
    fn result_accounting_for_expected_failure_and_skip() {
        reset();
        exc::init();
        let case = test_case();
        let expected_class = value::class_new(
            "ExpectedCase".to_string(),
            case,
            vec![(
                "test_expected".to_string(),
                value::native_function_new("unittest", "TestCase.fail", 2),
            )],
        );
        value::class_set_attr(
            expected_class,
            "__unittest_expecting_failure__".to_string(),
            TRUE,
        );
        let skipped_class = value::class_new(
            "SkippedCase".to_string(),
            test_case(),
            vec![(
                "test_skipped".to_string(),
                value::native_function_new("unittest", "_case_marker", 1),
            )],
        );
        value::class_set_attr(skipped_class, "__unittest_skip__".to_string(), TRUE);
        value::class_set_attr(
            skipped_class,
            "__unittest_skip_why__".to_string(),
            value::str_alloc("not today".to_string()),
        );

        let result = new_test_result();
        let name = value::str_alloc("test_expected".to_string());
        let instance = builtins::call(expected_class, 1, std::slice::from_ref(&name).as_ptr());
        test_case_run(&[instance, result]);
        let name = value::str_alloc("test_skipped".to_string());
        let instance = builtins::call(skipped_class, 1, std::slice::from_ref(&name).as_ptr());
        test_case_run(&[instance, result]);

        assert_eq!(result_list_len(result, "expectedFailures"), 1);
        assert_eq!(result_list_len(result, "skipped"), 1);
        assert_eq!(result_list_len(result, "failures"), 0);
        assert_eq!(result_list_len(result, "errors"), 0);
        assert_eq!(result_integer(result, "testsRun"), Some(2));
        assert!(!exc::pending());
        reset();
    }
}
