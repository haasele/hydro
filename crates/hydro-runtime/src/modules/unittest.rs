//! Native `unittest` support.
//!
//! The runtime implements the framework surface the CPython standard-library
//! test suite imports: `TestCase`, `TestSuite`, the result classes, the text
//! runner, the loader, `TestProgram`, the skip/expected-failure decorators and
//! the `unittest.util` helpers.
//!
//! Every user subclass of the native `TestCase` is registered when the class is
//! created (see `value::class_new`), so `unittest.main()` can discover and run
//! the tests without importing a Python `unittest` package. Discovery starts
//! from the selected module's namespace, like CPython's `loadTestsFromModule`,
//! and a module-level `load_tests` hook replaces it, exactly as CPython's
//! protocol specifies.
//!
//! Result accounting is real: [`run_main`] and `TestCase.run(result)` report
//! success, failure, error, skip, expected failure and unexpected success into
//! the caller's result object, using the same `add*` methods CPython calls.

use std::sync::{LazyLock, Mutex, MutexGuard};

use hydro_ir::ParamKind;

use crate::builtins;
use crate::container;
use crate::core::globals;
use crate::exc::{
    self, ASSERTION_ERROR, ATTRIBUTE_ERROR, BASE_EXCEPTION, EXCEPTION, KEYBOARD_INTERRUPT, SKIPPED,
    TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::iter;
use crate::methods;
use crate::ops;
use crate::value::{self, FALSE, NONE, NOT_IMPLEMENTED, TRUE};

/// Hidden native method that identifies a `TestCase`-derived class.
const CASE_MARKER: &str = "__unittest_case__";

/// Module name the entry unit's class bodies record in `__module__`.
const MAIN_MODULE: &str = "__main__";

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
const FAILED_TEST_KEY: &str = "unittest.loader._FailedTest.class";
const RAISES_KEY: &str = "unittest._AssertRaisesContext.class";
const DEFAULT_LOADER_KEY: &str = "unittest.defaultTestLoader";
const SHOULD_STOP_KEY: &str = "unittest.case._ShouldStop.class";
const UNEXPECTED_SUCCESS_KEY: &str = "unittest.case._UnexpectedSuccess.class";
const WRITELN_DECORATOR_KEY: &str = "unittest.runner._WritelnDecorator.class";

/// Registered test classes, in creation order.
static REGISTRY: LazyLock<Mutex<Vec<i64>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Cache key of the module-level cleanup list (`unittest.case._module_cleanups`).
const MODULE_CLEANUPS_KEY: &str = "unittest.module_cleanups";

fn registry() -> MutexGuard<'static, Vec<i64>> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Records a test class created by the frontend.
pub(crate) fn register(class: i64) {
    let mut classes = registry();
    if classes.contains(&class) {
        return;
    }
    classes.push(class);
    drop(classes);
    heap::add_thread_root(class);
}

/// Forgets every registered class (used by tests and a fresh runner start).
pub(crate) fn reset() {
    registry().clear();
    if let Some(list) = globals::registry_get(MODULE_CLEANUPS_KEY) {
        if let Some(items) = value::as_list(list) {
            let mut remaining = items.len();
            while remaining > 0 {
                remaining -= 1;
                let index = encode_int(i64::try_from(remaining).unwrap_or(i64::MAX));
                container::delete_subscript(list, index);
            }
        }
    }
}

/// The module-level cleanup list (`unittest.case._module_cleanups`).
///
/// CPython keeps one process-wide list of `(function, args, kwargs)` entries;
/// `addModuleCleanup` appends and `doModuleCleanups` drains it LIFO. The list
/// is a real Python list value cached in the globals registry so its contents
/// stay visible to the collector and to user code that reads the private name.
fn module_cleanups_list() -> i64 {
    if let Some(list) = globals::registry_get(MODULE_CLEANUPS_KEY) {
        return list;
    }
    let list = value::list_new(Vec::new());
    let roots = heap::root_values(std::slice::from_ref(&list));
    globals::registry_set(MODULE_CLEANUPS_KEY, list);
    heap::release_roots(roots);
    list
}

/// `unittest._module_cleanups`: the live cleanup list value.
pub(crate) fn module_cleanups() -> i64 {
    module_cleanups_list()
}

/// The `_class_cleanups` list owned by `class`, created on first use.
///
/// CPython gives every `TestCase` subclass its own list in
/// `__init_subclass__`; the runtime creates it lazily so a subclass never
/// inherits and drains its base class' cleanups.
fn class_cleanups_list(class: i64) -> Option<i64> {
    value::as_class(class)?;
    if let Some(value::ClassEntry::Attr(list)) = value::class_own_lookup(class, "_class_cleanups") {
        if value::as_list(list).is_some() {
            return Some(list);
        }
    }
    let list = value::list_new(Vec::new());
    let roots = heap::root_values(std::slice::from_ref(&list));
    value::class_set_attr(class, "_class_cleanups".to_string(), list);
    heap::release_roots(roots);
    Some(list)
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
    // Each function value is reachable only from this Rust vector until
    // `class_new` copies it into the class. `native_function_new` allocates,
    // so a collection triggered by a later iteration would free every earlier
    // function; their slots are then reused by the following functions, and
    // the class binds a name to a different method (observed: `TestResult`
    // gained `__init__ = addExpectedFailure` after a run that created the
    // cached class late, which made `TestResult()` raise and `unittest.main()`
    // see a `None` result). Root every value as it is produced.
    let function_roots = heap::root_values(&[]);
    let mut functions: Vec<i64> = Vec::with_capacity(methods.len());
    for (_, native, arity) in methods {
        let function = value::native_function_new("unittest", native, *arity);
        heap::push_scoped_root(function);
        functions.push(function);
    }
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

// -------------------------------------------------------------------------
// Assertion signatures
// -------------------------------------------------------------------------

/// One declared parameter of a native `TestCase` assertion: CPython's public
/// name, how it binds, and whether the slot has a default. Every optional slot
/// defaults to `None`, which the assertion implementations already read as
/// "not supplied" (the same value an omitted positional argument has always
/// carried).
type AssertionParam = (&'static str, ParamKind, Option<i64>);

/// Turns the table's `required`/`optional` marker into the literal default the
/// shared binder fills an omitted slot with. (`expr` fragments cannot appear
/// inside a repeated tuple without a trailing separator, so the two legal
/// defaults are spelled as keywords.)
macro_rules! default_slot {
    (required) => {
        None
    };
    (optional) => {
        Some(NONE)
    };
}

/// Declares the native assertion signatures together with their uniform
/// full-argv trampolines.
///
/// A native method binds keyword arguments only when its function value carries
/// both a [`value::ParamMeta`] signature and a full-argv entry (the shape
/// documented for `value::call_instance_dunder`): the shared method binder in
/// `core::methods` fills one slot per declared parameter — receiver first,
/// positionals in order, named arguments matched by parameter name, declared
/// defaults for the rest — and then calls the entry with that slot vector.
/// The trampoline therefore receives exactly `params.len()` values in
/// declaration order, which is the layout the assertion implementations
/// already expect (`self` at index 0 and `msg`/`places`/`delta` in their
/// documented places).
macro_rules! signed_assertions {
    ($($entry:ident: $method:literal [$(($name:literal, $kind:ident, $default:ident)),* $(,)?]),* $(,)?) => {
        $(
            extern "C" fn $entry(arguments: *const i64) -> i64 {
                const PARAMS: &[AssertionParam] =
                    &[$(($name, ParamKind::$kind, default_slot!($default))),*];
                // SAFETY: the shared binder materialises exactly one slot per
                // declared parameter, in declaration order.
                let slots = unsafe { std::slice::from_raw_parts(arguments, PARAMS.len()) };
                assertion_slots($method, slots)
            }
        )*

        /// Attaches every declared signature and full-argv entry to `class`.
        ///
        /// The assertions have two owners: this module's `TestCase` method
        /// table and the deprecated `fail*` aliases installed by
        /// [`crate::modules::unittest_assertions::install`]. Both are class
        /// attributes, so the function values are looked up by name instead of
        /// being taken from a table.
        fn install_assertion_signatures(class: i64) {
            $(
                let params: &[AssertionParam] =
                    &[$(($name, ParamKind::$kind, default_slot!($default))),*];
                sign_assertion(class, $method, $entry as *const () as usize, params);
            )*
        }
    };
}

signed_assertions! {
    assert_equal_entry: "assertEqual" [
        ("self", Positional, required),
        ("first", Positional, required),
        ("second", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_not_equal_entry: "assertNotEqual" [
        ("self", Positional, required),
        ("first", Positional, required),
        ("second", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_true_entry: "assertTrue" [
        ("self", Positional, required),
        ("expr", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_false_entry: "assertFalse" [
        ("self", Positional, required),
        ("expr", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_is_entry: "assertIs" [
        ("self", Positional, required),
        ("expr1", Positional, required),
        ("expr2", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_is_not_entry: "assertIsNot" [
        ("self", Positional, required),
        ("expr1", Positional, required),
        ("expr2", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_is_none_entry: "assertIsNone" [
        ("self", Positional, required),
        ("obj", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_is_not_none_entry: "assertIsNotNone" [
        ("self", Positional, required),
        ("obj", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_in_entry: "assertIn" [
        ("self", Positional, required),
        ("member", Positional, required),
        ("container", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_not_in_entry: "assertNotIn" [
        ("self", Positional, required),
        ("member", Positional, required),
        ("container", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_is_instance_entry: "assertIsInstance" [
        ("self", Positional, required),
        ("obj", Positional, required),
        ("cls", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_not_is_instance_entry: "assertNotIsInstance" [
        ("self", Positional, required),
        ("obj", Positional, required),
        ("cls", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_greater_entry: "assertGreater" [
        ("self", Positional, required),
        ("a", Positional, required),
        ("b", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_greater_equal_entry: "assertGreaterEqual" [
        ("self", Positional, required),
        ("a", Positional, required),
        ("b", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_less_entry: "assertLess" [
        ("self", Positional, required),
        ("a", Positional, required),
        ("b", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_less_equal_entry: "assertLessEqual" [
        ("self", Positional, required),
        ("a", Positional, required),
        ("b", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_almost_equal_entry: "assertAlmostEqual" [
        ("self", Positional, required),
        ("first", Positional, required),
        ("second", Positional, required),
        ("places", Positional, optional),
        ("msg", Positional, optional),
        ("delta", Positional, optional),
    ],
    assert_not_almost_equal_entry: "assertNotAlmostEqual" [
        ("self", Positional, required),
        ("first", Positional, required),
        ("second", Positional, required),
        ("places", Positional, optional),
        ("msg", Positional, optional),
        ("delta", Positional, optional),
    ],
    assert_regex_entry: "assertRegex" [
        ("self", Positional, required),
        ("text", Positional, required),
        ("expected_regex", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_not_regex_entry: "assertNotRegex" [
        ("self", Positional, required),
        ("text", Positional, required),
        ("unexpected_regex", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_count_equal_entry: "assertCountEqual" [
        ("self", Positional, required),
        ("first", Positional, required),
        ("second", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_sequence_equal_entry: "assertSequenceEqual" [
        ("self", Positional, required),
        ("seq1", Positional, required),
        ("seq2", Positional, required),
        ("msg", Positional, optional),
        ("seq_type", Positional, optional),
    ],
    assert_list_equal_entry: "assertListEqual" [
        ("self", Positional, required),
        ("list1", Positional, required),
        ("list2", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_tuple_equal_entry: "assertTupleEqual" [
        ("self", Positional, required),
        ("tuple1", Positional, required),
        ("tuple2", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_set_equal_entry: "assertSetEqual" [
        ("self", Positional, required),
        ("set1", Positional, required),
        ("set2", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_dict_equal_entry: "assertDictEqual" [
        ("self", Positional, required),
        ("d1", Positional, required),
        ("d2", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_multi_line_equal_entry: "assertMultiLineEqual" [
        ("self", Positional, required),
        ("first", Positional, required),
        ("second", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_has_attr_entry: "assertHasAttr" [
        ("self", Positional, required),
        ("obj", Positional, required),
        ("name", Positional, required),
        ("msg", Positional, optional),
    ],
    assert_not_has_attr_entry: "assertNotHasAttr" [
        ("self", Positional, required),
        ("obj", Positional, required),
        ("name", Positional, required),
        ("msg", Positional, optional),
    ],
    fail_entry: "fail" [
        ("self", Positional, required),
        ("msg", Positional, optional),
    ],
    // Deprecated `fail*` spellings, implemented by `unittest_assertions`.
    fail_if_entry: "failIf" [
        ("self", Positional, required),
        ("expr", Positional, required),
        ("msg", Positional, optional),
    ],
    fail_unless_entry: "failUnless" [
        ("self", Positional, required),
        ("expr", Positional, required),
        ("msg", Positional, optional),
    ],
    fail_if_equal_entry: "failIfEqual" [
        ("self", Positional, required),
        ("first", Positional, required),
        ("second", Positional, required),
        ("msg", Positional, optional),
    ],
    fail_unless_equal_entry: "failUnlessEqual" [
        ("self", Positional, required),
        ("first", Positional, required),
        ("second", Positional, required),
        ("msg", Positional, optional),
    ],
    fail_if_almost_equal_entry: "failIfAlmostEqual" [
        ("self", Positional, required),
        ("first", Positional, required),
        ("second", Positional, required),
        ("places", Positional, optional),
        ("msg", Positional, optional),
        ("delta", Positional, optional),
    ],
    fail_unless_almost_equal_entry: "failUnlessAlmostEqual" [
        ("self", Positional, required),
        ("first", Positional, required),
        ("second", Positional, required),
        ("places", Positional, optional),
        ("msg", Positional, optional),
        ("delta", Positional, optional),
    ],
}

/// Pieces the shared binder's slot vector back into the assertion's owner.
///
/// The core assertions are dispatched by [`test_case_method`]; the deprecated
/// `fail*` aliases are owned by [`crate::modules::unittest_assertions`] and
/// accept the same positional layout after their own binding step.
fn assertion_slots(method: &str, slots: &[i64]) -> i64 {
    // The slot values were produced by the binder in native memory; keep them
    // visible to the collector for the duration of the assertion.
    let roots = heap::root_values(slots);
    let result = match method {
        "failIf"
        | "failUnless"
        | "failIfEqual"
        | "failUnlessEqual"
        | "failIfAlmostEqual"
        | "failUnlessAlmostEqual" => {
            match crate::modules::unittest_assertions::call(method, slots) {
                Some(result) => result,
                None => missing_method("TestCase", method),
            }
        }
        _ => test_case_method(method, slots),
    };
    heap::release_roots(roots);
    result
}

/// Stores `params` as the signature of `class`'s `method` attribute and points
/// its full-argv entry at `entry`.
fn sign_assertion(class: i64, method: &str, entry: usize, params: &[AssertionParam]) {
    let Some(attribute) = value::class_lookup(class, method) else {
        return;
    };
    let function = match attribute {
        value::ClassEntry::Method(_, function) | value::ClassEntry::Attr(function) => function,
    };
    let signature = params
        .iter()
        .map(|(name, kind, default)| value::ParamMeta {
            name: (*name).to_string(),
            kind: *kind,
            default: *default,
        })
        .collect();
    value::function_set_signature(function, signature);
    value::function_set_full_entry(function, entry);
}

/// Builds a fresh native `TestCase` class.
pub(crate) fn test_case() -> i64 {
    if let Some(class) = globals::registry_get(TEST_CASE_KEY) {
        return class;
    }
    // (Python name, dispatch name, minimum argc including `self`).
    static METHODS: &[(&str, &str, usize)] = &[
        (CASE_MARKER, "_case_marker", 1),
        ("__init__", "TestCase.__init__", 1),
        ("run", "TestCase.run", 1),
        ("__call__", "TestCase.__call__", 1),
        ("debug", "TestCase.debug", 1),
        ("setUp", "TestCase.setUp", 1),
        ("tearDown", "TestCase.tearDown", 1),
        ("setUpClass", "TestCase.setUpClass", 1),
        ("tearDownClass", "TestCase.tearDownClass", 1),
        ("addCleanup", "TestCase.addCleanup", 2),
        ("doCleanups", "TestCase.doCleanups", 1),
        ("addClassCleanup", "TestCase.addClassCleanup", 2),
        ("doClassCleanups", "TestCase.doClassCleanups", 1),
        ("enterClassContext", "TestCase.enterClassContext", 2),
        ("enterContext", "TestCase.enterContext", 2),
        ("skipTest", "TestCase.skipTest", 1),
        ("subTest", "TestCase.subTest", 1),
        ("id", "TestCase.id", 1),
        ("shortDescription", "TestCase.shortDescription", 1),
        ("__eq__", "TestCase.__eq__", 2),
        ("__hash__", "TestCase.__hash__", 1),
        ("__str__", "TestCase.__str__", 1),
        ("__repr__", "TestCase.__repr__", 1),
        ("_callSetUp", "TestCase._callSetUp", 1),
        ("_callTestMethod", "TestCase._callTestMethod", 2),
        ("_callTearDown", "TestCase._callTearDown", 1),
        ("_callCleanup", "TestCase._callCleanup", 2),
        ("_addDuration", "TestCase._addDuration", 3),
        (
            "_getAssertEqualityFunc",
            "TestCase._getAssertEqualityFunc",
            3,
        ),
        ("countTestCases", "TestCase.countTestCases", 1),
        ("defaultTestResult", "TestCase.defaultTestResult", 1),
        ("_formatMessage", "TestCase._formatMessage", 3),
        ("_baseAssertEqual", "TestCase._baseAssertEqual", 3),
        ("_truncateMessage", "TestCase._truncateMessage", 3),
        ("addTypeEqualityFunc", "TestCase.addTypeEqualityFunc", 3),
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
    let class = cached_class(TEST_CASE_KEY, "TestCase", NONE, METHODS);
    // CPython's `TestCase` class attributes that user code reads.
    value::class_set_attr(
        class,
        "failureException".to_string(),
        exc::exception_class(ASSERTION_ERROR),
    );
    value::class_set_attr(class, "longMessage".to_string(), TRUE);
    value::class_set_attr(class, "maxDiff".to_string(), encode_int(80 * 8));
    value::class_set_attr(
        class,
        "_diffThreshold".to_string(),
        encode_int(2i64.pow(16)),
    );
    value::class_set_attr(class, "_classSetupFailed".to_string(), FALSE);
    value::class_set_attr(
        class,
        "tearDown_exceptions".to_string(),
        value::list_new(Vec::new()),
    );
    let _ = class_cleanups_list(class);
    // Class-scoped hooks are classmethods, so `TestCase.addClassCleanup(fn)`
    // and `Cls.doClassCleanups()` receive the owner class exactly as CPython's
    // descriptors do.
    for (python_name, dispatch_name) in [
        ("addClassCleanup", "TestCase.addClassCleanup"),
        ("doClassCleanups", "TestCase.doClassCleanups"),
        ("enterClassContext", "TestCase.enterClassContext"),
        ("setUpClass", "TestCase.setUpClass"),
        ("tearDownClass", "TestCase.tearDownClass"),
    ] {
        let function = value::native_function_new("unittest", dispatch_name, 1);
        value::class_set_attr(
            class,
            python_name.to_string(),
            value::classmethod_new(function),
        );
    }
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
    crate::modules::unittest_assertions::install(class);
    // CPython removed the deprecated `fail*`/`assertEquals` spellings in 3.12.
    // `unittest.case` no longer defines them, so `getattr` must raise
    // `AttributeError` (`testDeprecatedFailMethods`).
    for name in [
        "failIf",
        "failUnless",
        "failIfEqual",
        "failUnlessEqual",
        "failIfAlmostEqual",
        "failUnlessAlmostEqual",
        "failUnlessRaises",
        "assertNotEquals",
        "assertEquals",
        "assertAlmostEquals",
        "assertNotAlmostEquals",
        "assert_",
        "assertDictContainsSubset",
        "assertRaisesRegexp",
        "assertRegexpMatches",
    ] {
        value::class_delete_attr(class, name);
    }
    // Native assertion methods bind keyword arguments only with real
    // parameter metadata plus a full-argv entry: `self.assertEqual(a, b,
    // msg=...)`, `assertAlmostEqual(..., places=..., delta=...)` and the rest
    // of the CPython signature would otherwise be rejected as an unexpected
    // keyword by the shared method binder. This runs after `install` so the
    // deprecated `fail*` aliases it (re)assigns are signed too.
    install_assertion_signatures(class);
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
    static METHODS: [(&str, &str, usize); 13] = [
        ("__init__", "TextTestResult.__init__", 4),
        ("getDescription", "TextTestResult.getDescription", 2),
        ("startTest", "TextTestResult.startTest", 2),
        ("_write_status", "TextTestResult._write_status", 3),
        ("addSubTest", "TextTestResult.addSubTest", 4),
        ("addSuccess", "TextTestResult.addSuccess", 2),
        ("addError", "TextTestResult.addError", 3),
        ("addFailure", "TextTestResult.addFailure", 3),
        ("addSkip", "TextTestResult.addSkip", 3),
        ("addExpectedFailure", "TextTestResult.addExpectedFailure", 3),
        (
            "addUnexpectedSuccess",
            "TextTestResult.addUnexpectedSuccess",
            2,
        ),
        ("printErrors", "TextTestResult.printErrors", 1),
        ("printErrorList", "TextTestResult.printErrorList", 3),
    ];
    let class = cached_class(
        TEXT_RESULT_KEY,
        "TextTestResult",
        test_result_class(),
        &METHODS,
    );
    value::class_set_attr(
        class,
        "separator1".to_string(),
        value::str_alloc("=".repeat(70)),
    );
    value::class_set_attr(
        class,
        "separator2".to_string(),
        value::str_alloc("-".repeat(70)),
    );
    class
}

/// The cached `TextTestRunner` class.
pub(crate) fn text_test_runner_class() -> i64 {
    static METHODS: [(&str, &str, usize); 3] = [
        ("__init__", "TextTestRunner.__init__", 1),
        ("_makeResult", "TextTestRunner._makeResult", 1),
        ("run", "TextTestRunner.run", 2),
    ];
    let class = cached_class(TEXT_RUNNER_KEY, "TextTestRunner", NONE, &METHODS);
    value::class_set_attr(class, "resultclass".to_string(), text_test_result_class());
    class
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
    value::class_set_attr(class, "testNamePatterns".to_string(), NONE);
    value::class_set_attr(class, "_top_level_dir".to_string(), NONE);
    class
}

/// `unittest.defaultTestLoader`: the shared `TestLoader` instance.
pub(crate) fn default_test_loader() -> i64 {
    if let Some(loader) = globals::registry_get(DEFAULT_LOADER_KEY) {
        return loader;
    }
    let loader = value::instance_new(test_loader_class());
    let roots = heap::root_values(std::slice::from_ref(&loader));
    value::instance_setattr(loader, "errors".to_string(), value::list_new(Vec::new()));
    value::instance_setattr(
        loader,
        "_loading_packages".to_string(),
        value::list_new(Vec::new()),
    );
    value::instance_setattr(loader, "_top_level_dir".to_string(), NONE);
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
///
/// CPython's `_SubTest` is a real `TestCase` subclass; `isinstance(subtest,
/// unittest.TestCase)` is part of the documented result protocol (the skip
/// tests assert it). The class is removed from the discovery registry: it is a
/// framework-internal class, not a test the module defines.
fn subtest_class() -> i64 {
    static METHODS: [(&str, &str, usize); 6] = [
        ("__init__", "_SubTest.__init__", 3),
        ("__enter__", "_SubTest.__enter__", 1),
        ("__exit__", "_SubTest.__exit__", 4),
        ("__str__", "_SubTest.__str__", 1),
        ("id", "_SubTest.id", 1),
        ("shortDescription", "_SubTest.shortDescription", 1),
    ];
    let class = cached_class(SUBTEST_KEY, "_SubTest", test_case(), &METHODS);
    registry().retain(|registered| *registered != class);
    value::class_set_attr(class, "failureException".to_string(), NONE);
    class
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

/// `unittest.case._ShouldStop`: the internal control-flow exception a failing
/// subtest raises to abandon the rest of the test body under `failfast`.
pub(crate) fn should_stop_class() -> i64 {
    cached_class(
        SHOULD_STOP_KEY,
        "_ShouldStop",
        exc::exception_class(EXCEPTION),
        &[],
    )
}

/// Raises the internal `_ShouldStop` control-flow signal.
fn raise_should_stop() -> i64 {
    let instance = builtins::call(should_stop_class(), 0, std::ptr::null());
    exc::raise_value(instance)
}

/// Whether `error` is the internal `_ShouldStop` signal.
fn is_should_stop(error: i64) -> bool {
    value::is_subclass_of(value::type_class(error), should_stop_class())
}

/// `unittest.case._UnexpectedSuccess`: the exception a legacy `TestResult`
/// receives through `addFailure` when an expected failure unexpectedly passes.
fn unexpected_success_class() -> i64 {
    cached_class(
        UNEXPECTED_SUCCESS_KEY,
        "_UnexpectedSuccess",
        exc::exception_class(EXCEPTION),
        &[],
    )
}

/// `unittest.runner._WritelnDecorator`: a stream wrapper with `writeln`.
pub(crate) fn writeln_decorator_class() -> i64 {
    static METHODS: [(&str, &str, usize); 4] = [
        ("__init__", "_WritelnDecorator.__init__", 2),
        ("__getattr__", "_WritelnDecorator.__getattr__", 2),
        ("writeln", "_WritelnDecorator.writeln", 1),
        ("write", "_WritelnDecorator.write", 2),
    ];
    cached_class(WRITELN_DECORATOR_KEY, "_WritelnDecorator", NONE, &METHODS)
}

fn writeln_decorator_method(method: &str, args: &[i64]) -> i64 {
    match method {
        "__init__" => {
            let (Some(&instance), Some(&stream)) = (args.first(), args.get(1)) else {
                return missing_arguments("_WritelnDecorator.__init__");
            };
            value::instance_setattr(instance, "stream".to_string(), stream);
            NONE
        }
        "__getattr__" => {
            let (Some(&instance), Some(&name)) = (args.first(), args.get(1)) else {
                return missing_arguments("_WritelnDecorator.__getattr__");
            };
            let requested = value::as_str(name).unwrap_or("");
            if requested == "stream" || requested == "__getstate__" {
                return exc::raise_with_message(ATTRIBUTE_ERROR, &format!("'{requested}'"));
            }
            let stream = value::instance_attr(instance, "stream").unwrap_or(NONE);
            methods::getattr(stream, requested.as_ptr(), requested.len())
        }
        "writeln" => {
            let Some(&instance) = args.first() else {
                return missing_arguments("_WritelnDecorator.writeln");
            };
            let stream = value::instance_attr(instance, "stream").unwrap_or(NONE);
            if let Some(&argument) = args.get(1) {
                if argument != NONE {
                    stream_write(stream, &value::to_string(argument));
                }
            }
            stream_write(stream, "\n");
            NONE
        }
        "write" => {
            let (Some(&instance), Some(&argument)) = (args.first(), args.get(1)) else {
                return missing_arguments("_WritelnDecorator.write");
            };
            let stream = value::instance_attr(instance, "stream").unwrap_or(NONE);
            let argument = value::str_alloc(value::to_string(argument));
            let roots = heap::root_values(std::slice::from_ref(&argument));
            methods::call(stream, b"write".as_ptr(), 5, 1, &argument as *const i64);
            heap::release_roots(roots);
            NONE
        }
        _ => missing_method("_WritelnDecorator", method),
    }
}

/// Dispatches a native `unittest` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "main" => call_main(args, &[]),
        "expectedFailure" => decorator_expected_failure(args),
        "skip" => skip(args),
        "skipIf" => skip_if(args, false),
        "skipUnless" => skip_if(args, true),
        "_skip_apply" => skip_apply(args),
        "_identity" => args.first().copied().unwrap_or(NONE),
        "_three_way_cmp" => three_way_cmp(args),
        // Module-wide cleanups (CPython's `unittest.case` module functions).
        "addModuleCleanup" => add_module_cleanup(args, &[]),
        "doModuleCleanups" => do_module_cleanups(args),
        "enterModuleContext" => enter_module_context(args),
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
        // Assertion methods that live in their own native module (the
        // `assertIsSubclass`/`assertWarns`/`assertAlmostEqual` families). The
        // table is dynamic, so these are never declared as static spec
        // functions; an unknown name still reports the module-level
        // `AttributeError` below.
        _ => match crate::modules::unittest_assertions::call(function, args) {
            Some(result) => result,
            None => dispatch(function, args),
        },
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
            if function == "TestCase.assertRaises" || function == "TestCase.assertRaisesRegex" {
                return assert_raises_with_keywords(
                    args,
                    &keywords,
                    function == "TestCase.assertRaisesRegex",
                );
            }
            if function == "TestCase.subTest" {
                // `subTest(self, msg=..., **params)`: `msg` is the message,
                // every other name is a subtest parameter. Passing all of them
                // through as `**params` would lose the message entirely.
                let mut call_args = args.to_vec();
                let mut remaining = keywords;
                if let Some(index) = remaining.iter().position(|(name, _)| name == "msg") {
                    if call_args.len() > 1 {
                        return exc::raise_with_message(
                            TYPE_ERROR,
                            "unittest.TestCase.subTest() got multiple values for argument 'msg'",
                        );
                    }
                    let (_, message) = remaining.remove(index);
                    call_args.push(message);
                }
                while call_args.len() < 2 {
                    call_args.push(subtest_message_sentinel());
                }
                let dictionary = keywords_dict(&remaining);
                call_args.push(dictionary);
                return dispatch(function, &call_args);
            }
            let dictionary = keywords_dict(&keywords);
            let mut call_args = args.to_vec();
            call_args.push(dictionary);
            dispatch(function, &call_args)
        }
        // `addClassCleanup(function, /, *args, **kwargs)` keeps the trailing
        // keyword dict as the cleanup's own `**kwargs`, exactly like
        // `addCleanup`.
        "TestCase.addClassCleanup" => {
            let dictionary = keywords_dict(&keywords);
            let mut call_args = args.to_vec();
            call_args.push(dictionary);
            dispatch(function, &call_args)
        }
        // `TestLoader.loadTestsFromModule(module, *, pattern=None)`.
        "TestLoader.loadTestsFromModule" => {
            let mut call_args = args.to_vec();
            let mut remaining = keywords;
            if let Some(index) = remaining.iter().position(|(name, _)| name == "pattern") {
                let (_, pattern) = remaining.remove(index);
                while call_args.len() < 2 {
                    call_args.push(NONE);
                }
                call_args.push(pattern);
            }
            if let Some((name, _)) = remaining.first() {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "unittest.TestLoader.loadTestsFromModule() got an unexpected keyword argument '{name}'"
                    ),
                );
            }
            dispatch(function, &call_args)
        }
        // The skip/expected-failure decorators accept their documented names
        // (`reason=`, `condition=`), including when the spec entry leaves the
        // names dynamic. Binding is strict: a duplicate positional/keyword
        // value and a missing `condition` report the same `TypeError` as
        // CPython's binder instead of silently shifting the slots.
        "skip" | "skipIf" | "skipUnless" => {
            let names: &[&str] = if function == "skip" {
                &["reason"]
            } else {
                &["condition", "reason"]
            };
            let mut slots = args.to_vec();
            for (name, item) in keywords {
                let Some(index) = names.iter().position(|candidate| *candidate == name) else {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!(
                            "unittest.{function}() got an unexpected keyword argument '{name}'"
                        ),
                    );
                };
                if index < slots.len() {
                    return exc::raise_with_message(
                        TYPE_ERROR,
                        &format!("unittest.{function}() got multiple values for argument '{name}'"),
                    );
                }
                while slots.len() < index {
                    slots.push(NONE);
                }
                slots.push(item);
            }
            if function != "skip" && slots.first().is_none_or(|condition| *condition == NONE) {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "unittest.{function}() missing 1 required positional argument: 'condition'"
                    ),
                );
            }
            call(function, &slots)
        }
        "addModuleCleanup" => add_module_cleanup(args, &keywords),
        "enterModuleContext" | "doModuleCleanups" => {
            if let Some((name, _)) = keywords.first() {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("unittest.{function}() got an unexpected keyword argument '{name}'"),
                );
            }
            call(function, args)
        }
        // `unittest.main(module=...)` names the module whose tests run; the
        // argument is also accepted positionally through `call`.
        "main" => call_main(args, &keywords),
        // Constructors bind their documented keyword arguments into the
        // positional slots the native initializers read.
        "TestResult.__init__" => {
            let slots = bind_named_slots(
                args,
                &keywords,
                &["stream", "descriptions", "verbosity"],
                function,
            );
            dispatch(function, &slots)
        }
        "TextTestRunner.__init__" => {
            let slots = bind_named_slots(
                args,
                &keywords,
                &[
                    "stream",
                    "descriptions",
                    "verbosity",
                    "failfast",
                    "buffer",
                    "resultclass",
                    "warnings",
                    "tb_locals",
                    "durations",
                ],
                function,
            );
            dispatch(function, &slots)
        }
        "TestCase.__init__" => {
            let slots = bind_named_slots(args, &keywords, &["methodName"], function);
            dispatch(function, &slots)
        }
        "TestLoader.discover" => {
            let slots = bind_named_slots(
                args,
                &keywords,
                &["start_dir", "pattern", "top_level_dir"],
                function,
            );
            dispatch(function, &slots)
        }
        "TextTestRunner.run" | "TestProgram.__init__" | "TestLoader.__init__" => {
            call(function, args)
        }
        _ => {
            // Dynamic assertion methods (`assertWarns`, `assertIsSubclass`,
            // ...) live in their own module; only names the table does not
            // know fall through to the keyword validation below.
            if let Some(result) =
                crate::modules::unittest_assertions::call_kw(function, args, keywords.clone())
            {
                return result;
            }
            // Existing assertions (`assertEqual`, `assertTrue`, ...) take
            // `msg` as their final positional argument. New assertions were
            // handled above; for the existing table, bind `msg=` into that
            // slot and dispatch.
            let mut remaining = keywords.clone();
            if let Some(index) = remaining.iter().position(|(name, _)| name == "msg") {
                let (_, msg) = remaining.remove(index);
                if remaining.is_empty() && function.starts_with("TestCase.") {
                    let mut call_args = args.to_vec();
                    call_args.push(msg);
                    return dispatch(function, &call_args);
                }
            }
            if remaining.is_empty() {
                call(function, args)
            } else {
                let name = remaining[0].0.clone();
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
    if let Some(method) = function.strip_prefix("TextTestResult.") {
        return text_test_result_method(method, args);
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
    if let Some(method) = function.strip_prefix("_FailedTest.") {
        return failed_test_method(method, args);
    }
    if let Some(method) = function.strip_prefix("_AssertRaisesContext.") {
        return raises_context_method(method, args);
    }
    if let Some(method) = function.strip_prefix("_WritelnDecorator.") {
        return writeln_decorator_method(method, args);
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
    // A function/class item is still being decorated. Anything else is the
    // receiver of a wrapper produced by `apply_skip` and called through an
    // outer decorator: CPython's `skip_wrapper` raises `SkipTest` there, so
    // the skip cannot be erased by a decorator that does not copy attributes.
    if value::as_function(item).is_none() && value::as_class(item).is_none() {
        return exc::raise_with_message(SKIPPED, &text);
    }
    apply_skip(item, &text)
}

fn apply_skip(item: i64, reason: &str) -> i64 {
    set_item_flag(item, "__unittest_skip__", TRUE);
    let reason_value = value::str_alloc(reason.to_string());
    set_item_flag(item, "__unittest_skip_why__", reason_value);
    if value::as_class(item).is_some() {
        return item;
    }
    // CPython replaces a decorated function with a wrapper that raises
    // `SkipTest` when called and carries the skip attributes. The runtime has
    // no function factory, so the wrapper is the internal
    // `_skip_apply(True, reason, receiver)` call: at run time `skip_apply`
    // sees a receiver (not a function/class) and raises the same exception.
    let apply = value::native_function_new("unittest", "_skip_apply", 3);
    let wrapper = value::partial_new(apply, vec![TRUE, reason_value]);
    set_item_flag(wrapper, "__unittest_skip__", TRUE);
    set_item_flag(wrapper, "__unittest_skip_why__", reason_value);
    let name = value::function_name(item);
    if !name.is_empty() {
        value::function_setattr(wrapper, "__name__", value::str_alloc(name));
    }
    let doc = value::function_doc(item);
    if doc != NONE {
        value::function_setattr(wrapper, "__doc__", doc);
    }
    wrapper
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
// Cleanups and context managers
// -------------------------------------------------------------------------

/// Builds one cleanup entry: `(function, args_tuple, kwargs_dict)`.
fn cleanup_entry(function: i64, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let positional = value::tuple_new(args.to_vec());
    let kwargs = keywords_dict(keywords);
    value::tuple_new(vec![function, positional, kwargs])
}

/// Calls a cleanup function with positional and keyword arguments.
///
/// The error (if any) is left pending in the exception slot, exactly like a
/// direct Python call; callers that must continue running decide whether to
/// collect or propagate it.
fn call_cleanup(function: i64, positional: &[i64], keywords: &[(String, i64)]) -> i64 {
    if keywords.is_empty() {
        return builtins::call(function, positional.len(), positional.as_ptr());
    }
    let words = keyword_words(keywords);
    builtins::call_kw(
        function,
        positional.len(),
        positional.as_ptr(),
        keywords.len(),
        words.as_ptr(),
    )
}

/// Runs the entry at `entry` (already popped from its cleanup list).
fn run_cleanup_entry(entry: i64) -> i64 {
    let Some(parts) = value::as_tuple(entry) else {
        return NONE;
    };
    let function = parts.first().copied().unwrap_or(NONE);
    let positional: Vec<i64> = parts
        .get(1)
        .and_then(|args| value::as_tuple(*args))
        .map(<[i64]>::to_vec)
        .unwrap_or_default();
    let keywords = match parts.get(2).copied() {
        Some(kwargs) if value::as_dict(kwargs).is_some() => keywords_from_dict(kwargs),
        _ => Vec::new(),
    };
    let roots = heap::root_values(&[entry, function]);
    let result = call_cleanup(function, &positional, &keywords);
    heap::release_roots(roots);
    result
}

/// Pops and runs every entry of `list`, LIFO, collecting the exceptions.
///
/// Returns the exceptions in execution order; the exception slot is left
/// clear so the caller can report them through a result.
fn drain_cleanups(list: i64) -> Vec<i64> {
    let mut errors = Vec::new();
    while let Some(items) = value::as_list(list) {
        let Some(&entry) = items.last() else {
            break;
        };
        let roots = heap::root_values(&[entry]);
        let index = encode_int(i64::try_from(items.len() - 1).unwrap_or(i64::MAX));
        container::delete_subscript(list, index);
        run_cleanup_entry(entry);
        if exc::pending() {
            errors.push(take_pending());
        }
        heap::release_roots(roots);
    }
    errors
}

/// Raises CPython's `TypeError` for a non-context-manager object.
fn context_protocol_error(cm: i64) -> i64 {
    let class = value::type_class(cm);
    let module = class_module(class).unwrap_or_else(|| "builtins".to_string());
    let name = class_name(class);
    let mut msg = format!("'{module}.{name}' object does not support the context manager protocol");
    let is_async = value::class_lookup(class, "__aenter__").is_some()
        && value::class_lookup(class, "__aexit__").is_some();
    if is_async {
        msg.push_str(
            " but it supports the asynchronous context manager protocol. Did you mean to use enterAsyncContext()?",
        );
    }
    exc::raise_with_message(TYPE_ERROR, &msg)
}

/// Enters `cm` and returns `(enter_result, bound_exit)`.
///
/// CPython looks the special methods up on the type, so a context manager
/// with a missing `__enter__`/`__exit__` reports the protocol error even when
/// the instance shadows the name.
fn enter_context_core(cm: i64) -> Option<(i64, i64)> {
    let class = value::type_class(cm);
    let has_enter = value::class_lookup(class, "__enter__").is_some();
    let has_exit = value::class_lookup(class, "__exit__").is_some();
    if !has_enter || !has_exit {
        context_protocol_error(cm);
        return None;
    }
    let exit = methods::getattr(cm, b"__exit__".as_ptr(), 8);
    if exc::pending() {
        return None;
    }
    let enter = methods::getattr(cm, b"__enter__".as_ptr(), 9);
    if exc::pending() {
        return None;
    }
    let roots = heap::root_values(&[exit, enter]);
    let entered = builtins::call(enter, 0, std::ptr::null());
    heap::release_roots(roots);
    if exc::pending() {
        return None;
    }
    Some((entered, exit))
}

/// Registers `cm.__exit__` as a cleanup after entering it.
fn add_context_cleanup<F>(cm: i64, add: F) -> i64
where
    F: FnOnce(i64),
{
    let Some((entered, exit)) = enter_context_core(cm) else {
        return NONE;
    };
    // `enter_context_core` reads `cm.__exit__`, so the receiver is already
    // bound; passing `cm` again would supply five arguments to a native
    // four-parameter exit (`_GeneratorContextManager.__exit__`).
    let entry = cleanup_entry(exit, &[NONE, NONE, NONE], &[]);
    let roots = heap::root_values(&[entry]);
    add(entry);
    heap::release_roots(roots);
    entered
}

fn append_cleanup(list: i64, entry: i64) -> i64 {
    let roots = heap::root_values(&[list, entry]);
    container::list_append(list, entry);
    heap::release_roots(roots);
    NONE
}

/// `unittest.addModuleCleanup(function, /, *args, **kwargs)`.
fn add_module_cleanup(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let Some(&function) = args.first() else {
        return missing_arguments("addModuleCleanup");
    };
    if function == NONE {
        return exc::raise_with_message(TYPE_ERROR, "addModuleCleanup() requires a callable");
    }
    let entry = cleanup_entry(function, &args[1..], keywords);
    append_cleanup(module_cleanups_list(), entry)
}

/// `unittest.doModuleCleanups()`: run every module cleanup LIFO and raise the
/// first failure after the list is drained.
fn do_module_cleanups(_args: &[i64]) -> i64 {
    let errors = drain_cleanups(module_cleanups_list());
    if let Some(&first) = errors.first() {
        return exc::raise_value(first);
    }
    NONE
}

/// `unittest.enterModuleContext(cm)`.
fn enter_module_context(args: &[i64]) -> i64 {
    let Some(&cm) = args.first() else {
        return missing_arguments("enterModuleContext");
    };
    add_context_cleanup(cm, |entry| {
        append_cleanup(module_cleanups_list(), entry);
    })
}

/// `TestCase.addClassCleanup(function, /, *args, **kwargs)` (classmethod).
fn add_class_cleanup(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let (Some(&class), Some(&function)) = (args.first(), args.get(1)) else {
        return missing_arguments("TestCase.addClassCleanup");
    };
    if value::as_class(class).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            "addClassCleanup() requires a TestCase subclass",
        );
    }
    let Some(list) = class_cleanups_list(class) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "addClassCleanup() requires a TestCase subclass",
        );
    };
    let entry = cleanup_entry(function, &args[2..], keywords);
    append_cleanup(list, entry)
}

/// Runs every class cleanup, storing the `sys.exc_info()` tuples CPython's
/// suite turns into `tearDownClass` errors.
fn do_class_cleanups_core(class: i64) -> i64 {
    let Some(list) = class_cleanups_list(class) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "doClassCleanups() requires a TestCase subclass",
        );
    };
    let exceptions = value::list_new(Vec::new());
    let roots = heap::root_values(&[exceptions]);
    for error in drain_cleanups(list) {
        let info = error_tuple(error);
        let info_roots = heap::root_values(&[info]);
        container::list_append(exceptions, info);
        heap::release_roots(info_roots);
    }
    value::class_set_attr(class, "tearDown_exceptions".to_string(), exceptions);
    heap::release_roots(roots);
    NONE
}

/// `TestCase.doClassCleanups()` (classmethod).
fn do_class_cleanups(args: &[i64]) -> i64 {
    let Some(&class) = args.first() else {
        return missing_arguments("TestCase.doClassCleanups");
    };
    if value::as_class(class).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            "doClassCleanups() requires a TestCase subclass",
        );
    }
    do_class_cleanups_core(class)
}

/// `TestCase.enterClassContext(cm)` (classmethod).
fn enter_class_context(args: &[i64]) -> i64 {
    let (Some(&class), Some(&cm)) = (args.first(), args.get(1)) else {
        return missing_arguments("TestCase.enterClassContext");
    };
    if value::as_class(class).is_none() {
        return exc::raise_with_message(
            TYPE_ERROR,
            "enterClassContext() requires a TestCase subclass",
        );
    }
    add_context_cleanup(cm, |entry| {
        if let Some(list) = class_cleanups_list(class) {
            append_cleanup(list, entry);
        }
    })
}

/// `TestCase.enterContext(cm)`: enter `cm` and register its `__exit__` as a
/// test cleanup, returning the `__enter__` result.
fn enter_context(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&cm)) = (args.first(), args.get(1)) else {
        return missing_arguments("TestCase.enterContext");
    };
    add_context_cleanup(cm, |entry| {
        append_test_cleanup(instance, entry);
    })
}

/// Appends `entry` to `instance._cleanups`, creating the list when needed.
fn append_test_cleanup(instance: i64, entry: i64) -> i64 {
    let cleanups = value::instance_attr(instance, "_cleanups")
        .filter(|list| value::as_list(*list).is_some())
        .unwrap_or_else(|| {
            let list = value::list_new(Vec::new());
            value::instance_setattr(instance, "_cleanups".to_string(), list);
            list
        });
    append_cleanup(cleanups, entry)
}

/// `TestCase.skipTest(reason)`: raise `SkipTest` with the rendered reason.
fn skip_test(args: &[i64]) -> i64 {
    let reason = args.get(1).copied().unwrap_or(NONE);
    exc::raise_with_message(SKIPPED, &reason_text(reason))
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
        "setUp" | "tearDown" | "setUpClass" | "tearDownClass" => NONE,
        "addCleanup" => add_cleanup(args),
        "doCleanups" => {
            let instance = args.first().copied().unwrap_or(NONE);
            value::bool_value(do_cleanups(instance))
        }
        "addClassCleanup" => {
            let mut call_args = args.to_vec();
            let dictionary = match call_args.last().copied() {
                Some(dict) if value::as_dict(dict).is_some() => call_args.pop().unwrap_or(NONE),
                _ => value::dict_new(Vec::new()),
            };
            let keywords = if value::as_dict(dictionary).is_some() {
                keywords_from_dict(dictionary)
            } else {
                Vec::new()
            };
            add_class_cleanup(&call_args, &keywords)
        }
        "doClassCleanups" => do_class_cleanups(args),
        "enterClassContext" => enter_class_context(args),
        "enterContext" => enter_context(args),
        "skipTest" => skip_test(args),
        "subTest" => sub_test(args),
        "id" => test_case_id(args),
        "shortDescription" => short_description(args),
        "__eq__" => test_case_eq(args),
        "__hash__" => test_case_hash(args),
        "__str__" => test_case_str(args),
        "__repr__" => test_case_repr(args),
        "_callSetUp" => call_method(args.first().copied().unwrap_or(NONE), "setUp", &[]),
        "_callTearDown" => call_method(args.first().copied().unwrap_or(NONE), "tearDown", &[]),
        "_callTestMethod" => test_case_call_test_method(args),
        "_callCleanup" => {
            let (Some(&function), Some(&call_args)) = (args.get(1), args.get(2)) else {
                return missing_arguments("TestCase._callCleanup");
            };
            let positional = collect_items(call_args).unwrap_or_default();
            builtins::call(function, positional.len(), positional.as_ptr())
        }
        "_addDuration" => test_case_add_duration(args),
        "_getAssertEqualityFunc" => test_case_get_equality_func(args),
        "countTestCases" => encode_int(1),
        "defaultTestResult" => new_test_result(),
        "fail" => fail(args),
        "_formatMessage" => format_message(args),
        "_baseAssertEqual" => base_assert_equal(args),
        "_truncateMessage" => truncate_message(args),
        "addTypeEqualityFunc" => add_type_equality_func(args),
        "assertEqual" => assert_equal(args),
        "assertListEqual" => assert_sequence_equal(args, Some("list")),
        "assertTupleEqual" => assert_sequence_equal(args, Some("tuple")),
        "assertSetEqual" => assert_set_equal(args),
        "assertDictEqual" => assert_dict_equal(args),
        "assertMultiLineEqual" => assert_multi_line_equal(args),
        "assertSequenceEqual" => assert_sequence_equal(args, None),
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
    let method_name = value::str_alloc(name.clone());
    let cleanups = value::list_new(Vec::new());
    let type_defaults = type_equality_defaults();
    let doc = method_function(class, &name)
        .map(value::function_doc)
        .unwrap_or_else(|| value::str_alloc("No test".to_string()));
    // Attribute insertion can allocate (the table may grow); the fresh
    // values must stay reachable until the instance owns them.
    let roots = heap::root_values(&[method_name, cleanups, type_defaults, doc]);
    value::instance_setattr(instance, "_testMethodName".to_string(), method_name);
    value::instance_setattr(instance, "_cleanups".to_string(), cleanups);
    value::instance_setattr(instance, "_unittest_result".to_string(), NONE);
    value::instance_setattr(instance, "_unittest_expecting".to_string(), FALSE);
    value::instance_setattr(instance, "_unittest_expected_failure".to_string(), NONE);
    value::instance_setattr(instance, "_outcome".to_string(), NONE);
    value::instance_setattr(instance, "_subtest".to_string(), NONE);
    value::instance_setattr(instance, "_type_equality_funcs".to_string(), type_defaults);
    value::instance_setattr(instance, "_testMethodDoc".to_string(), doc);
    heap::release_roots(roots);
    NONE
}

/// CPython's default `_type_equality_funcs`: the type-specific assertions
/// `assertEqual` dispatches to for containers and strings.
fn type_equality_defaults() -> i64 {
    let mut pairs = Vec::new();
    for (type_name, method) in [
        ("dict", "assertDictEqual"),
        ("list", "assertListEqual"),
        ("tuple", "assertTupleEqual"),
        ("set", "assertSetEqual"),
        ("frozenset", "assertSetEqual"),
        ("str", "assertMultiLineEqual"),
    ] {
        let class = value::native_class(type_name);
        if class != NONE {
            pairs.push((class, value::str_alloc(method.to_string())));
        }
    }
    value::dict_new(pairs)
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
        let result = call_method(instance, "defaultTestResult", &[]);
        if exc::pending() {
            return NONE;
        }
        if has_method(result, "startTestRun") {
            call_method(result, "startTestRun", &[]);
        } else {
            warn_runtime("TestResult has no startTestRun method");
        }
        result
    } else {
        supplied
    };
    let roots = heap::root_values(&[result]);
    call_method(result, "startTest", &[instance]);

    let name = value::instance_attr(instance, "_testMethodName")
        .and_then(value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "runTest".to_string());
    // CPython resolves the test method with `getattr` after `startTest`, and a
    // missing method propagates (only `stopTest`/`stopTestRun` run).
    let method = method_function(class, &name);
    if method.is_none() {
        let message = format!(
            "'{}' object has no attribute '{name}'",
            value::type_name(instance)
        );
        finish_run(result, instance, owns_result);
        heap::release_roots(roots);
        return exc::raise_with_message(ATTRIBUTE_ERROR, &message);
    }

    // Class-level or method-level `@unittest.skip`.
    let class_skip = class_flag_truthy(class, "__unittest_skip__");
    let method_skip = method.is_some_and(|function| attr_truthy(function, "__unittest_skip__"));
    if class_skip || method_skip {
        let why = class_attr_text(class, "__unittest_skip_why__")
            .or_else(|| {
                method.and_then(|function| function_attr_text(function, "__unittest_skip_why__"))
            })
            .unwrap_or_default();
        result_add_skip(result, instance, &why);
        finish_run(result, instance, owns_result);
        heap::release_roots(roots);
        return result;
    }

    let expecting = class_flag_truthy(class, "__unittest_expecting_failure__")
        || method.is_some_and(|function| attr_truthy(function, "__unittest_expecting_failure__"));

    // Expose the active result so `subTest` contexts and cleanups report into
    // it, and the expectation so a failing subtest registers the expected
    // failure of the whole test.
    value::instance_setattr(instance, "_unittest_result".to_string(), result);
    value::instance_setattr(
        instance,
        "_unittest_expecting".to_string(),
        value::bool_value(expecting),
    );
    value::instance_setattr(instance, "_unittest_expected_failure".to_string(), NONE);
    value::instance_setattr(instance, "_subtest".to_string(), NONE);

    let started = std::time::Instant::now();
    // CPython's `_Outcome`: `success` gates tearDown and final accounting;
    // `expectedFailure` is the exception an `@expectedFailure` body raised,
    // which still lets tearDown and cleanups run.
    let mut success = true;
    let mut expected_failure: Option<i64> = None;
    let mut interrupted: Option<i64> = None;

    call_method(instance, "setUp", &[]);
    if exc::pending() {
        let error = take_pending();
        if is_keyboard_interrupt(error) {
            interrupted = Some(error);
        } else {
            success = false;
            record_test_error(result, instance, error, false);
        }
    } else {
        let returned = call_instance_method(instance, &name);
        if !exc::pending() && returned != NONE {
            warn_deprecated_return(instance, &name, returned);
        }
        if exc::pending() {
            let error = take_pending();
            // `SkipTest` wins over the expected-failure flag: the test is
            // reported as skipped, not as an expected failure.
            if is_keyboard_interrupt(error) {
                interrupted = Some(error);
            } else if is_should_stop(error) {
                // The failing subtest already reported its own outcome.
            } else if value::exception_data(error).map(|data| data.tag) == Some(SKIPPED) {
                success = false;
                record_test_error(result, instance, error, false);
            } else if expecting {
                expected_failure = Some(error);
            } else {
                success = false;
                record_test_error(result, instance, error, false);
            }
        }
        // A failing subtest with an expected failure may have been recorded by
        // `_SubTest.__exit__` and suppressed; treat it like a body failure.
        if expected_failure.is_none() {
            if let Some(error) = value::instance_attr(instance, "_unittest_expected_failure")
                .filter(|error| *error != NONE)
            {
                expected_failure = Some(error);
                value::instance_setattr(instance, "_unittest_expected_failure".to_string(), NONE);
            }
        }
        // CPython runs `tearDown` after every body outcome except a
        // `KeyboardInterrupt`: on a failure the extra `testPartExecutor`
        // simply records the second outcome. Only a failing `setUp` skips it.
        if interrupted.is_none() {
            // CPython finalizes discarded test temporaries through refcounts;
            // a collection here runs their `__del__` hooks before tearDown,
            // which file/process fixtures rely on.
            crate::heap::collect();
            call_method(instance, "tearDown", &[]);
            if exc::pending() {
                let error = take_pending();
                if is_keyboard_interrupt(error) {
                    interrupted = Some(error);
                } else {
                    success = false;
                    record_test_error(result, instance, error, false);
                }
            }
        }
    }

    // Cleanups always run, even after a failing `setUp`. Each failure is
    // reported as its own outcome and prevents a pass/expected-failure report.
    if interrupted.is_none() && !do_cleanups(instance) {
        success = false;
    }
    if exc::pending() {
        let error = take_pending();
        if is_keyboard_interrupt(error) {
            interrupted = Some(error);
        } else {
            success = false;
            record_test_error(result, instance, error, false);
        }
    }

    let elapsed = started.elapsed().as_secs_f64();

    if let Some(error) = interrupted {
        value::instance_setattr(instance, "_unittest_result".to_string(), NONE);
        value::instance_setattr(instance, "_unittest_expecting".to_string(), FALSE);
        finish_run(result, instance, owns_result);
        heap::release_roots(roots);
        return exc::raise_value(error);
    }

    // CPython records the elapsed time after the cleanups, on every path that
    // reached this point (an early `@skip` returns before the timer starts).
    call_method(
        instance,
        "_addDuration",
        &[result, value::float_new(elapsed)],
    );

    if success {
        if expecting {
            match expected_failure {
                Some(error) => result_add_expected_failure(result, instance, error),
                None => result_add_unexpected_success(result, instance),
            }
        } else {
            call_method(result, "addSuccess", &[instance]);
        }
    }

    value::instance_setattr(instance, "_unittest_result".to_string(), NONE);
    value::instance_setattr(instance, "_unittest_expecting".to_string(), FALSE);
    finish_run(result, instance, owns_result);
    heap::release_roots(roots);
    result
}

/// `result.stopTest(test)` plus `stopTestRun()` when this run owns the result.
fn finish_run(result: i64, instance: i64, owns_result: bool) {
    call_method(result, "stopTest", &[instance]);
    if owns_result {
        if has_method(result, "stopTestRun") {
            call_method(result, "stopTestRun", &[]);
        } else {
            warn_runtime("TestResult has no stopTestRun method");
        }
    }
}

fn is_keyboard_interrupt(error: i64) -> bool {
    value::exception_data(error).map(|data| data.tag) == Some(KEYBOARD_INTERRUPT)
}

fn warn_runtime(message: &str) {
    crate::modules::warnings::warn_category(message, crate::exc::RUNTIME_WARNING);
    if exc::pending() {
        exc::clear();
    }
}

/// `_addSkip`: prefer `result.addSkip`, otherwise warn and record a success
/// (CPython's legacy-result compatibility path).
fn result_add_skip(result: i64, test: i64, reason: &str) {
    if has_method(result, "addSkip") {
        call_method(
            result,
            "addSkip",
            &[test, value::str_alloc(reason.to_string())],
        );
    } else {
        warn_runtime("TestResult has no addSkip method, skips not reported");
        call_method(result, "addSuccess", &[test]);
    }
}

fn result_add_expected_failure(result: i64, test: i64, error: i64) {
    if has_method(result, "addExpectedFailure") {
        call_method(result, "addExpectedFailure", &[test, error_tuple(error)]);
    } else {
        warn_runtime("TestResult has no addExpectedFailure method, reporting as passes");
        call_method(result, "addSuccess", &[test]);
    }
}

fn result_add_unexpected_success(result: i64, test: i64) {
    if has_method(result, "addUnexpectedSuccess") {
        call_method(result, "addUnexpectedSuccess", &[test]);
    } else {
        warn_runtime("TestResult has no addUnexpectedSuccess method, reporting as failure");
        let failure = builtins::call(unexpected_success_class(), 0, std::ptr::null());
        call_method(result, "addFailure", &[test, error_tuple(failure)]);
    }
}

/// Calls `instance.<name>()` through a bound attribute.
///
/// Resolving the attribute first keeps descriptors and `functools.wraps`
/// wrappers bound exactly as Python would; calling the class-namespace entry
/// directly would drop `self` for wrappers stored as plain attributes.
fn call_instance_method(instance: i64, name: &str) -> i64 {
    let bound = methods::getattr(instance, name.as_ptr(), name.len());
    if exc::pending() {
        return NONE;
    }
    let roots = heap::root_values(std::slice::from_ref(&bound));
    let result = builtins::call(bound, 0, std::ptr::null());
    heap::release_roots(roots);
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
    call_instance_method(instance, &name);
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

/// Runs the instance's cleanup list LIFO, reporting every failure through the
/// active result (if any) and returning CPython's `outcome.success`.
///
/// A `SkipTest` raised by a cleanup is reported with `addSkip`; an ordinary
/// exception goes through the same failure/error classification as a test
/// outcome. Every cleanup runs even after an earlier one failed.
fn do_cleanups(instance: i64) -> bool {
    let Some(cleanups) =
        value::instance_attr(instance, "_cleanups").filter(|list| value::as_list(*list).is_some())
    else {
        return true;
    };
    let result = value::instance_attr(instance, "_unittest_result")
        .filter(|result| *result != NONE)
        .or_else(|| {
            value::instance_attr(instance, "_outcome").and_then(|outcome| {
                if outcome == NONE {
                    None
                } else {
                    value::instance_attr(outcome, "result").filter(|result| *result != NONE)
                }
            })
        })
        .unwrap_or(NONE);
    let mut success = true;
    while let Some(items) = value::as_list(cleanups) {
        let Some(&entry) = items.last() else {
            break;
        };
        let roots = heap::root_values(&[entry]);
        let index = encode_int(i64::try_from(items.len() - 1).unwrap_or(i64::MAX));
        container::delete_subscript(cleanups, index);
        run_cleanup_entry(entry);
        if exc::pending() {
            success = false;
            let error = take_pending();
            if is_keyboard_interrupt(error) {
                heap::release_roots(roots);
                exc::raise_value(error);
                return false;
            }
            if result != NONE {
                record_test_error(result, instance, error, false);
            }
        }
        heap::release_roots(roots);
    }
    success
}

/// The unique value CPython uses for an omitted `subTest(msg=...)`.
///
/// `None` cannot serve as the sentinel: `subTest(None)` is an explicit message
/// and must render as `[None]`.
fn subtest_message_sentinel() -> i64 {
    const KEY: &str = "unittest._subtest_msg_sentinel";
    if let Some(sentinel) = globals::registry_get(KEY) {
        return sentinel;
    }
    let sentinel = value::native_function_new("unittest", "_subtest_msg_sentinel", 0);
    let roots = heap::root_values(std::slice::from_ref(&sentinel));
    globals::registry_set(KEY, sentinel);
    heap::add_thread_root(sentinel);
    heap::release_roots(roots);
    sentinel
}

/// CPython's `_OrderedChainMap.new_child(params)` for a nested subtest.
///
/// The child mapping goes first, then the parent's keys that the child does
/// not override; insertion order is preserved so `_subDescription` renders
/// `(baz=2, bar=3, foo=1)` exactly like CPython.
fn chained_subtest_params(parent: i64, params: i64) -> i64 {
    let parent_pairs = value::as_dict(parent).unwrap_or(&[]);
    let child_pairs = value::as_dict(params).unwrap_or(&[]);
    let mut merged: Vec<(i64, i64)> = child_pairs.to_vec();
    for &(key, item) in parent_pairs {
        let overridden = merged
            .iter()
            .any(|(existing, _)| container::equals(*existing, key));
        if !overridden {
            merged.push((key, item));
        }
    }
    value::dict_new(merged)
}

fn sub_test(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return missing_arguments("TestCase.subTest");
    };
    let message = args
        .get(1)
        .copied()
        .unwrap_or_else(subtest_message_sentinel);
    let params = match args.get(2) {
        Some(&dictionary) if value::as_dict(dictionary).is_some() => dictionary,
        _ => value::dict_new(Vec::new()),
    };
    let parent = value::instance_attr(instance, "_subtest").unwrap_or(NONE);
    let params = if parent != NONE {
        let parent_params = value::instance_attr(parent, "params").unwrap_or(NONE);
        chained_subtest_params(parent_params, params)
    } else {
        params
    };
    let class = subtest_class();
    let context = value::instance_new(class);
    let roots = heap::root_values(&[context, params, parent]);
    value::instance_setattr(context, "test_case".to_string(), instance);
    value::instance_setattr(context, "_message".to_string(), message);
    value::instance_setattr(context, "_params".to_string(), params);
    value::instance_setattr(context, "params".to_string(), params);
    value::instance_setattr(context, "_parent".to_string(), parent);
    let failure = self_attribute(instance, "failureException")
        .unwrap_or_else(|| exc::exception_class(ASSERTION_ERROR));
    value::instance_setattr(context, "failureException".to_string(), failure);
    value::instance_setattr(instance, "_subtest".to_string(), context);
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
    value::str_alloc(format!("{}.{name}", class_text(class)))
}

/// `TestCase.__eq__`: equal when both operands are the same exact class and
/// name the same test method; otherwise `NotImplemented`.
fn test_case_eq(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&other)) = (args.first(), args.get(1)) else {
        return missing_arguments("TestCase.__eq__");
    };
    let Some(state) = value::as_instance(instance) else {
        return NOT_IMPLEMENTED;
    };
    let Some(other_state) = value::as_instance(other) else {
        return NOT_IMPLEMENTED;
    };
    if state.class != other_state.class {
        return NOT_IMPLEMENTED;
    }
    let name = value::instance_attr(instance, "_testMethodName").unwrap_or(NONE);
    let other_name = value::instance_attr(other, "_testMethodName").unwrap_or(NONE);
    value::bool_value(container::equals(name, other_name))
}

/// `TestCase.__hash__`: `hash((type(self), self._testMethodName))`.
fn test_case_hash(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return missing_arguments("TestCase.__hash__");
    };
    let class = value::type_class(instance);
    let name = value::instance_attr(instance, "_testMethodName").unwrap_or(NONE);
    let pair = value::tuple_new(vec![class, name]);
    let roots = heap::root_values(std::slice::from_ref(&pair));
    let hash = builtins::hash(pair);
    heap::release_roots(roots);
    hash
}

fn test_case_call_test_method(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&function)) = (args.first(), args.get(1)) else {
        return missing_arguments("TestCase._callTestMethod");
    };
    let result = builtins::call(function, 0, std::ptr::null());
    if exc::pending() {
        return result;
    }
    if result != NONE {
        let name = value::instance_attr(instance, "_testMethodName")
            .and_then(value::as_str)
            .map(str::to_string)
            .unwrap_or_default();
        warn_deprecated_return(instance, &name, result);
    }
    result
}

/// CPython's deprecation warning for a test method returning a value.
fn warn_deprecated_return(instance: i64, name: &str, returned: i64) {
    let name = if name.is_empty() {
        value::instance_attr(instance, "_testMethodName")
            .and_then(value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        name.to_string()
    };
    let mut message = format!(
        "It is deprecated to return a value that is not None from a test case ({name} returned {:?})",
        value::type_name(returned)
    );
    if value::type_name(returned) == "coroutine" {
        message.push_str(". Maybe you forgot to use IsolatedAsyncioTestCase as the base class?");
    }
    crate::modules::warnings::warn_category(&message, crate::exc::DEPRECATION_WARNING);
    if exc::pending() {
        exc::clear();
    }
}

fn test_case_add_duration(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&result), Some(&elapsed)) = (args.first(), args.get(1), args.get(2))
    else {
        return missing_arguments("TestCase._addDuration");
    };
    if result == NONE {
        return NONE;
    }
    if !has_method(result, "addDuration") {
        crate::modules::warnings::warn_category(
            "TestResult has no addDuration method",
            crate::exc::RUNTIME_WARNING,
        );
        if exc::pending() {
            exc::clear();
        }
        return NONE;
    }
    let argv = [instance, elapsed];
    let name = b"addDuration";
    methods::call(result, name.as_ptr(), name.len(), 2, argv.as_ptr())
}

fn test_case_get_equality_func(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&first), Some(&second)) = (args.first(), args.get(1), args.get(2))
    else {
        return missing_arguments("TestCase._getAssertEqualityFunc");
    };
    if value::type_class(first) == value::type_class(second) {
        if let Some(entry) = type_equality_entry(instance, first) {
            return entry;
        }
    }

    value::native_function_new("unittest", "TestCase._baseAssertEqual", 4)
}

fn short_description(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return missing_arguments("TestCase.shortDescription");
    };
    let doc = value::instance_attr(instance, "_testMethodDoc").unwrap_or(NONE);
    let Some(text) = value::as_str(doc) else {
        return NONE;
    };
    let first = text.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        NONE
    } else {
        value::str_alloc(first.to_string())
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
    value::str_alloc(format!("{name} ({}.{name})", class_text(class)))
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
    value::str_alloc(format!("<{} testMethod={name}>", class_text(class)))
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
    let (Some(&instance), Some(&first), Some(&second)) = (args.first(), args.get(1), args.get(2))
    else {
        return missing_arguments("assertEqual");
    };
    if container::equals(first, second) {
        return NONE;
    }
    let msg = args.get(3).copied().unwrap_or(NONE);
    // CPython dispatches to a type-specific assertion when both operands have
    // exactly the same type and the class registered one.
    if value::type_class(first) == value::type_class(second) {
        if let Some(dispatch) = type_equality_entry(instance, first) {
            if let Some(name) = value::as_str(dispatch) {
                let name = name.to_string();
                return test_case_method(&name, &[instance, first, second, msg]);
            }
            let call_args = [first, second, msg];
            let roots = heap::root_values(&[dispatch]);
            let result = builtins::call(dispatch, call_args.len(), call_args.as_ptr());
            heap::release_roots(roots);
            return result;
        }
    }
    let (first_repr, second_repr) = shorten_pair(first, second);
    raise_assertion(assertion_message(
        args,
        3,
        format!("{first_repr} != {second_repr}"),
    ))
}

/// The registered type-specific equality assertion for `value`'s type, if any.
fn type_equality_entry(instance: i64, value_to_check: i64) -> Option<i64> {
    let table = value::instance_attr(instance, "_type_equality_funcs")?;
    let pairs = value::as_dict(table)?;
    let class = value::type_class(value_to_check);
    pairs
        .iter()
        .find(|(key, _)| *key == class)
        .map(|(_, entry)| *entry)
}

/// `self._formatMessage(msg, standardMsg)`.
fn format_message_text(instance: i64, msg: i64, standard: String) -> String {
    let long = self_attribute(instance, "longMessage").is_some_and(value::truthy);
    if !long {
        if msg == NONE {
            return standard;
        }
        return match value::as_str(msg) {
            Some(text) => text.to_string(),
            None => value::to_string(msg),
        };
    }
    if msg == NONE {
        return standard;
    }
    let text = match value::as_str(msg) {
        Some(text) => text.to_string(),
        None => value::to_string(msg),
    };
    format!("{standard} : {text}")
}

/// `self._formatMessage(msg, standardMsg)` as a native method.
fn format_message(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return missing_arguments("TestCase._formatMessage");
    };
    let msg = args.get(1).copied().unwrap_or(NONE);
    let standard = reason_text(args.get(2).copied().unwrap_or(NONE));
    value::str_alloc(format_message_text(instance, msg, standard))
}

/// `self._baseAssertEqual(first, second, msg)`: plain `==`, no type dispatch.
fn base_assert_equal(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&first), Some(&second)) = (args.first(), args.get(1), args.get(2))
    else {
        return missing_arguments("TestCase._baseAssertEqual");
    };
    if container::equals(first, second) {
        return NONE;
    }
    let msg = args.get(3).copied().unwrap_or(NONE);
    let standard = format!("{} != {}", value::repr(first), value::repr(second));
    raise_assertion(format_message_text(instance, msg, standard))
}

/// The receiver attribute, falling back to the class attribute (CPython's
/// normal `self.name` lookup).
fn self_attribute(instance: i64, name: &str) -> Option<i64> {
    if let Some(value) = value::instance_attr(instance, name) {
        return Some(value);
    }
    let class = value::as_instance(instance)
        .map(|state| state.class)
        .unwrap_or(instance);
    match value::class_lookup(class, name) {
        Some(value::ClassEntry::Attr(item)) => Some(item),
        _ => None,
    }
}

/// CPython's `_truncateMessage`: append `diff` unless it exceeds `maxDiff`.
fn truncate_message_text(instance: i64, message: String, diff: String) -> String {
    let max_diff = self_attribute(instance, "maxDiff").unwrap_or(NONE);
    if max_diff == NONE {
        return format!("{message}{diff}");
    }
    let Some(limit) = value::as_integer(max_diff) else {
        return format!("{message}{diff}");
    };
    let length = diff.chars().count();
    if i64::try_from(length).unwrap_or(i64::MAX) <= limit {
        format!("{message}{diff}")
    } else {
        format!("{message}\nDiff is {length} characters long. Set self.maxDiff to None to see it.")
    }
}

/// `self._truncateMessage(message, diff)` as a native method.
fn truncate_message(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return missing_arguments("TestCase._truncateMessage");
    };
    let message = reason_text(args.get(1).copied().unwrap_or(NONE));
    let diff = reason_text(args.get(2).copied().unwrap_or(NONE));
    value::str_alloc(truncate_message_text(instance, message, diff))
}

/// Calls the instance's `_truncateMessage`, honouring a test that replaces it
/// (CPython's tests install their own to assert the diff path was/was not
/// taken). Returns `None` when the call raised, leaving that exception pending.
fn truncate_via_instance(instance: i64, message: String, diff: String) -> Option<String> {
    let bound = methods::getattr(instance, b"_truncateMessage".as_ptr(), 16);
    if exc::pending() {
        exc::clear();
        return Some(truncate_message_text(instance, message, diff));
    }
    let arguments = [value::str_alloc(message), value::str_alloc(diff)];
    let roots = heap::root_values(&arguments);
    let result = builtins::call(bound, 2, arguments.as_ptr());
    let text = if exc::pending() {
        None
    } else {
        value::as_str(result).map(str::to_string)
    };
    heap::release_roots(roots);
    text
}

/// `self.addTypeEqualityFunc(typeobj, function)`.
fn add_type_equality_func(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&typeobj), Some(&function)) =
        (args.first(), args.get(1), args.get(2))
    else {
        return missing_arguments("TestCase.addTypeEqualityFunc");
    };
    let table = value::instance_attr(instance, "_type_equality_funcs")
        .filter(|table| value::as_dict(*table).is_some())
        .unwrap_or_else(|| {
            let table = value::dict_new(Vec::new());
            value::instance_setattr(instance, "_type_equality_funcs".to_string(), table);
            table
        });
    container::dict_set(table, typeobj, function)
}

/// Capitalized form used by sequence failure messages.
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Length of a sequence, or `None` for a non-sequence (the pending
/// `TypeError` is cleared).
fn len_of(item: i64) -> Option<i64> {
    let length = builtins::len(item);
    if exc::pending() {
        exc::clear();
        None
    } else {
        Some(length)
    }
}

/// `seq[index]`, or `None` (with the pending error cleared) on failure.
fn item_at(seq: i64, index: i64) -> Option<i64> {
    let item = container::subscript(seq, encode_int(index));
    if exc::pending() {
        exc::clear();
        None
    } else {
        Some(item)
    }
}

/// `(_common_shorten_repr(first, second))` as a pair of strings.
fn shorten_pair(first: i64, second: i64) -> (String, String) {
    let pair = common_shorten_repr(&[first, second]);
    match value::as_tuple(pair) {
        Some(items) if items.len() == 2 => (
            value::as_str(items[0])
                .map(str::to_string)
                .unwrap_or_else(|| value::repr(items[0])),
            value::as_str(items[1])
                .map(str::to_string)
                .unwrap_or_else(|| value::repr(items[1])),
        ),
        _ => (value::repr(first), value::repr(second)),
    }
}

/// Imports `module` on demand and resolves `module.name`.
///
/// Used only while building a failure message; an unavailable module (or a
/// function this runtime cannot provide) simply means the message omits its
/// diff instead of failing the assertion a second time.
fn imported_function(module: &str, name: &str) -> Option<i64> {
    let module_arg = value::str_alloc(module.to_string());
    let roots = heap::root_values(std::slice::from_ref(&module_arg));
    let _ = crate::modules::importlib::call("import_module", std::slice::from_ref(&module_arg));
    if exc::pending() {
        exc::clear();
        heap::release_roots(roots);
        return None;
    }
    let function = globals::lookup(module, name).or_else(|| {
        let candidate = crate::modules::module_get_by_name(module, name);
        if exc::pending() {
            exc::clear();
            None
        } else {
            Some(candidate)
        }
    });
    heap::release_roots(roots);
    function
}

/// `pprint.pformat(value)` as text, when the module is available.
fn pprint_text(item: i64) -> Option<String> {
    let function = imported_function("pprint", "pformat")?;
    let roots = heap::root_values(std::slice::from_ref(&item));
    let text = builtins::call(function, 1, std::slice::from_ref(&item).as_ptr());
    let result = if exc::pending() {
        exc::clear();
        None
    } else {
        value::as_str(text).map(str::to_string)
    };
    heap::release_roots(roots);
    result
}

/// `difflib.ndiff(first.splitlines(), second.splitlines())` joined by newlines.
fn ndiff_text(first: &str, second: &str) -> Option<String> {
    let function = imported_function("difflib", "ndiff")?;
    let first_lines = value::list_new(
        first
            .lines()
            .map(|line| value::str_alloc(line.to_string()))
            .collect(),
    );
    let second_lines = value::list_new(
        second
            .lines()
            .map(|line| value::str_alloc(line.to_string()))
            .collect(),
    );
    let arguments = [first_lines, second_lines];
    let roots = heap::root_values(&arguments);
    let diff = builtins::call(function, 2, arguments.as_ptr());
    let lines = if exc::pending() {
        exc::clear();
        None
    } else {
        collect_items(diff)
    };
    heap::release_roots(roots);
    let lines = lines?;
    let mut out = String::new();
    for line in lines {
        if let Some(text) = value::as_str(line) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    Some(out)
}

/// `text.splitlines(keepends=True)` as a list of string values.
fn splitlines_keepends(text: i64) -> Vec<i64> {
    let splitlines = methods::getattr(text, b"splitlines".as_ptr(), 10);
    if !exc::pending() {
        let roots = heap::root_values(std::slice::from_ref(&splitlines));
        let keep_ends = TRUE;
        let listed = builtins::call(splitlines, 1, &keep_ends as *const i64);
        let items = if exc::pending() {
            exc::clear();
            None
        } else {
            collect_items(listed)
        };
        heap::release_roots(roots);
        if let Some(items) = items {
            return items;
        }
    } else {
        exc::clear();
    }
    match value::as_str(text) {
        Some(text) => text
            .split_inclusive('\n')
            .map(|line| value::str_alloc(line.to_string()))
            .collect(),
        None => Vec::new(),
    }
}

/// `assertSequenceEqual(seq1, seq2, msg=None, seq_type=None)`.
fn assert_sequence_equal(args: &[i64], seq_type_name: Option<&str>) -> i64 {
    let (Some(&instance), Some(&seq1), Some(&seq2)) = (args.first(), args.get(1), args.get(2))
    else {
        return missing_arguments("assertSequenceEqual");
    };
    let msg = args.get(3).copied().unwrap_or(NONE);
    let seq_type_value = args
        .get(4)
        .copied()
        .filter(|class| *class != NONE && value::as_class(*class).is_some());
    let seq_type = seq_type_name
        .map(str::to_string)
        .or_else(|| seq_type_value.map(class_name));
    let seq_type_name = seq_type.clone().unwrap_or_else(|| "sequence".to_string());
    if let Some(class) = seq_type_value {
        let name = class_name(class);
        for (label, sequence) in [("First", seq1), ("Second", seq2)] {
            if !value::truthy(builtins::isinstance(sequence, class)) {
                return raise_assertion(format!(
                    "{label} sequence is not a {name}: {}",
                    value::repr(sequence)
                ));
            }
        }
    }
    let mut differing = String::new();
    let len1 = len_of(seq1);
    let len2 = len_of(seq2);
    if let (Some(_), Some(_)) = (len1, len2) {
        if container::equals(seq1, seq2) {
            return NONE;
        }
    }
    match (len1, len2) {
        (None, _) => differing.push_str(&format!(
            "First {seq_type_name} has no length.    Non-sequence?"
        )),
        (_, None) => differing.push_str(&format!(
            "Second {seq_type_name} has no length.    Non-sequence?"
        )),
        (Some(len1), Some(len2)) => {
            let (first_repr, second_repr) = shorten_pair(seq1, seq2);
            differing.push_str(&format!(
                "{}s differ: {first_repr} != {second_repr}\n",
                capitalize(&seq_type_name)
            ));
            let mut same_prefix = true;
            for index in 0..len1.min(len2) {
                let Some(item1) = item_at(seq1, index) else {
                    differing.push_str(&format!(
                        "\nUnable to index element {index} of first {seq_type_name}\n"
                    ));
                    same_prefix = false;
                    break;
                };
                let Some(item2) = item_at(seq2, index) else {
                    differing.push_str(&format!(
                        "\nUnable to index element {index} of second {seq_type_name}\n"
                    ));
                    same_prefix = false;
                    break;
                };
                if !container::equals(item1, item2) {
                    let (first_item, second_item) = shorten_pair(item1, item2);
                    differing.push_str(&format!(
                        "\nFirst differing element {index}:\n{first_item}\n{second_item}\n"
                    ));
                    same_prefix = false;
                    break;
                }
            }
            if same_prefix
                && len1 == len2
                && seq_type.is_none()
                && value::type_class(seq1) != value::type_class(seq2)
            {
                return NONE;
            }
            if len1 > len2 {
                differing.push_str(&format!(
                    "\nFirst {seq_type_name} contains {} additional elements.\n",
                    len1 - len2
                ));
                if let Some(extra) = item_at(seq1, len2) {
                    differing.push_str(&format!(
                        "First extra element {len2}:\n{}\n",
                        value::repr(extra)
                    ));
                }
            } else if len1 < len2 {
                differing.push_str(&format!(
                    "\nSecond {seq_type_name} contains {} additional elements.\n",
                    len2 - len1
                ));
                if let Some(extra) = item_at(seq2, len1) {
                    differing.push_str(&format!(
                        "First extra element {len1}:\n{}\n",
                        value::repr(extra)
                    ));
                }
            }
        }
    }
    let diff = match (pprint_text(seq1), pprint_text(seq2)) {
        (Some(first), Some(second)) => ndiff_text(&first, &second)
            .map(|diff| format!("\n{diff}"))
            .unwrap_or_default(),
        _ => String::new(),
    };
    let Some(standard) = truncate_via_instance(instance, differing, diff) else {
        return NONE;
    };
    raise_assertion(format_message_text(instance, msg, standard))
}

/// `assertSetEqual(set1, set2, msg=None)`.
fn assert_set_equal(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&set1), Some(&set2)) = (args.first(), args.get(1), args.get(2))
    else {
        return missing_arguments("assertSetEqual");
    };
    let msg = args.get(3).copied().unwrap_or(NONE);
    let difference = |first: i64, second: i64| {
        let result = methods::call(
            first,
            b"difference".as_ptr(),
            10,
            1,
            std::slice::from_ref(&second).as_ptr(),
        );
        if exc::pending() {
            return None;
        }
        collect_items(result)
    };
    let Some(first_only) = difference(set1, set2) else {
        return raise_assertion("first argument does not support set difference".to_string());
    };
    let Some(second_only) = difference(set2, set1) else {
        return raise_assertion("second argument does not support set difference".to_string());
    };
    if first_only.is_empty() && second_only.is_empty() {
        return NONE;
    }
    let mut lines = Vec::new();
    if !first_only.is_empty() {
        lines.push("Items in the first set but not the second:".to_string());
        lines.extend(first_only.into_iter().map(value::repr));
    }
    if !second_only.is_empty() {
        lines.push("Items in the second set but not the first:".to_string());
        lines.extend(second_only.into_iter().map(value::repr));
    }
    raise_assertion(format_message_text(instance, msg, lines.join("\n")))
}

/// `assertDictEqual(d1, d2, msg=None)`.
fn assert_dict_equal(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&first), Some(&second)) = (args.first(), args.get(1), args.get(2))
    else {
        return missing_arguments("assertDictEqual");
    };
    let msg = args.get(3).copied().unwrap_or(NONE);
    if value::as_dict(first).is_none() {
        return raise_assertion(format_message_text(
            instance,
            msg,
            "First argument is not a dictionary".to_string(),
        ));
    }
    if value::as_dict(second).is_none() {
        return raise_assertion(format_message_text(
            instance,
            msg,
            "Second argument is not a dictionary".to_string(),
        ));
    }
    if container::equals(first, second) {
        return NONE;
    }
    let (first_repr, second_repr) = shorten_pair(first, second);
    let standard = format!("{first_repr} != {second_repr}");
    let diff = match (pprint_text(first), pprint_text(second)) {
        (Some(first), Some(second)) => ndiff_text(&first, &second)
            .map(|diff| format!("\n{diff}"))
            .unwrap_or_default(),
        _ => String::new(),
    };
    let Some(standard) = truncate_via_instance(instance, standard, diff) else {
        return NONE;
    };
    raise_assertion(format_message_text(instance, msg, standard))
}

/// `assertMultiLineEqual(first, second, msg=None)`.
fn assert_multi_line_equal(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&first), Some(&second)) = (args.first(), args.get(1), args.get(2))
    else {
        return missing_arguments("assertMultiLineEqual");
    };
    let msg = args.get(3).copied().unwrap_or(NONE);
    let Some(first_text) = value::as_str(first) else {
        return raise_assertion(format_message_text(
            instance,
            msg,
            "First argument is not a string".to_string(),
        ));
    };
    let Some(second_text) = value::as_str(second) else {
        return raise_assertion(format_message_text(
            instance,
            msg,
            "Second argument is not a string".to_string(),
        ));
    };
    if first_text == second_text {
        return NONE;
    }
    let (short_first, short_second) = shorten_pair(first, second);
    let standard = format!("{short_first} != {short_second}");
    // CPython skips difflib for strings above `_diffThreshold`.
    let threshold = self_attribute(instance, "_diffThreshold")
        .and_then(value::as_integer)
        .unwrap_or(2i64.pow(16));
    let too_long = i64::try_from(first_text.chars().count()).unwrap_or(i64::MAX) > threshold
        || i64::try_from(second_text.chars().count()).unwrap_or(i64::MAX) > threshold;
    if !too_long {
        // Append a newline to either side that lacks one so the diff shows the
        // line break difference; empty strings stay empty.
        let mut first_lines_text = first_text.to_string();
        let mut second_lines_text = second_text.to_string();
        if !first_text.is_empty() && !second_text.is_empty() {
            if !first_text.ends_with('\n') || !second_text.ends_with('\n') {
                first_lines_text.push('\n');
                second_lines_text.push('\n');
            }
        } else if !first_text.is_empty() && !first_text.ends_with('\n') {
            first_lines_text.push('\n');
        } else if !second_text.is_empty() && !second_text.ends_with('\n') {
            second_lines_text.push('\n');
        }
        let first_values = value::list_new(splitlines_keepends(value::str_alloc(first_lines_text)));
        let second_values =
            value::list_new(splitlines_keepends(value::str_alloc(second_lines_text)));
        if let Some(function) = imported_function("difflib", "ndiff") {
            let arguments = [first_values, second_values];
            let roots = heap::root_values(&arguments);
            let diff = builtins::call(function, 2, arguments.as_ptr());
            let lines = if exc::pending() {
                exc::clear();
                None
            } else {
                collect_items(diff)
            };
            heap::release_roots(roots);
            if let Some(lines) = lines {
                let mut text = String::new();
                for line in lines {
                    if let Some(line) = value::as_str(line) {
                        text.push_str(line);
                    }
                }
                let Some(standard) = truncate_via_instance(instance, standard, format!("\n{text}"))
                else {
                    return NONE;
                };
                return raise_assertion(format_message_text(instance, msg, standard));
            }
        }
    }
    raise_assertion(format_message_text(instance, msg, standard))
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
            format!("unexpectedly identical: {}", value::repr(first))
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
            "unexpectedly None".to_string()
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
            format!(
                "{} not found in {}",
                value::repr(member),
                value::repr(collection)
            )
        } else {
            format!(
                "{} unexpectedly found in {}",
                value::repr(member),
                value::repr(collection)
            )
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
    let tuple = value::as_tuple(class);
    if expected {
        let standard = if tuple.is_some() {
            format!(
                "{} is not an instance of any of {}",
                value::repr(expression),
                value::repr(class)
            )
        } else {
            format!(
                "{} is not an instance of {}",
                value::repr(expression),
                value::repr(class)
            )
        };
        return raise_assertion(assertion_message(args, 3, standard));
    }
    // `assertNotIsInstance` reports the one class of a tuple that matched.
    let mut matched = class;
    if let Some(items) = tuple {
        for item in items {
            let candidate = builtins::isinstance(expression, *item);
            if !exc::pending() && value::truthy(candidate) {
                matched = *item;
                break;
            }
            if exc::pending() {
                exc::clear();
            }
        }
    }
    raise_assertion(assertion_message(
        args,
        3,
        format!(
            "{} is an instance of {}",
            value::repr(expression),
            value::repr(matched)
        ),
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
    if container::equals(first, second) {
        return if expected {
            NONE
        } else {
            raise_assertion(assertion_message(
                args,
                4,
                format!(
                    "{} == {} within 7 places",
                    value::repr(first),
                    value::repr(second)
                ),
            ))
        };
    }
    let places = args.get(3).copied().unwrap_or(NONE);
    let msg = args.get(4).copied().unwrap_or(NONE);
    let delta = args.get(5).copied().unwrap_or(NONE);
    if delta != NONE && places != NONE {
        return exc::raise_with_message(TYPE_ERROR, "specify delta or places not both");
    }
    let difference = ops::binop(ops::OP_SUB, first, second);
    if exc::pending() {
        return difference;
    }
    let difference = builtins::abs(difference);
    if exc::pending() {
        return difference;
    }
    if delta != NONE {
        let within = ops::binop(ops::OP_LE, difference, delta);
        if exc::pending() {
            return within;
        }
        let within = value::truthy(within);
        if within == expected {
            return NONE;
        }
        let relation = if expected { "!=" } else { "==" };
        let standard = format!(
            "{} {relation} {} within {} delta ({} difference)",
            value::repr(first),
            value::repr(second),
            value::repr(delta),
            value::repr(difference)
        );
        return raise_assertion(format_message_text(
            args.first().copied().unwrap_or(NONE),
            msg,
            standard,
        ));
    }
    let places = if places == NONE {
        encode_int(7)
    } else {
        places
    };
    let rounded = builtins::round_value(difference, places);
    if exc::pending() {
        return rounded;
    }
    let close = container::equals(rounded, encode_int(0));
    if close == expected {
        return NONE;
    }
    let standard = if expected {
        format!(
            "{} != {} within {} places ({} difference)",
            value::repr(first),
            value::repr(second),
            value::repr(places),
            value::repr(difference)
        )
    } else {
        format!(
            "{} == {} within {} places",
            value::repr(first),
            value::repr(second),
            value::repr(places)
        )
    };
    raise_assertion(format_message_text(
        args.first().copied().unwrap_or(NONE),
        msg,
        standard,
    ))
}

fn assert_regex(args: &[i64], expected: bool) -> i64 {
    let (Some(&text), Some(&pattern)) = (args.get(1), args.get(2)) else {
        return missing_arguments("assertRegex");
    };
    let search_args = vec![pattern, text];
    let found = crate::modules::re::call("search", &search_args);
    if exc::pending() {
        return found;
    }
    let matched = value::truthy(found);
    if matched == expected {
        return NONE;
    }
    let standard = if expected {
        format!(
            "Regex didn't match: {} not found in {}",
            value::repr(pattern),
            value::repr(text)
        )
    } else {
        format!(
            "Regex matched: {} matches {} in {}",
            value::repr(text),
            value::repr(pattern),
            value::repr(text)
        )
    };
    raise_assertion(assertion_message(args, 3, standard))
}

fn assert_count_equal(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&first), Some(&second)) = (args.first(), args.get(1), args.get(2))
    else {
        return missing_arguments("assertCountEqual");
    };
    let msg = args.get(3).copied().unwrap_or(NONE);
    let Some(left) = collect_items(first) else {
        return raise_assertion(format_message_text(
            instance,
            msg,
            "First argument is not iterable".to_string(),
        ));
    };
    let Some(right) = collect_items(second) else {
        return raise_assertion(format_message_text(
            instance,
            msg,
            "Second argument is not iterable".to_string(),
        ));
    };
    let count_in = |items: &[i64], target: i64| {
        items
            .iter()
            .filter(|item| container::equals(**item, target))
            .count()
    };
    let mut seen: Vec<i64> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for &item in left.iter().chain(right.iter()) {
        if seen.iter().any(|known| container::equals(*known, item)) {
            continue;
        }
        seen.push(item);
        let first_count = count_in(&left, item);
        let second_count = count_in(&right, item);
        if first_count != second_count {
            lines.push(format!(
                "First has {first_count}, Second has {second_count}:  {}",
                value::repr(item)
            ));
        }
    }
    if lines.is_empty() {
        return NONE;
    }
    let standard = truncate_message_text(
        instance,
        "Element counts were not equal:\n".to_string(),
        lines.join("\n"),
    );
    raise_assertion(format_message_text(instance, msg, standard))
}

fn assert_raises(args: &[i64], with_regex: bool) -> i64 {
    let (Some(&instance), Some(&expected)) = (args.first(), args.get(1)) else {
        return missing_arguments(if with_regex {
            "assertRaisesRegex"
        } else {
            "assertRaises"
        });
    };
    let regex = if with_regex {
        let Some(&regex) = args.get(2) else {
            return missing_arguments("assertRaisesRegex");
        };
        Some(regex)
    } else {
        None
    };
    let callable_index = if with_regex { 3 } else { 2 };
    if let Some(&callable) = args.get(callable_index) {
        let call_args = &args[callable_index + 1..];
        return assert_raises_callable(
            instance,
            expected,
            regex,
            callable,
            call_args,
            &[],
            with_regex,
        );
    }
    make_raises_context(instance, expected, regex, NONE, NONE, with_regex)
}

/// Keyword-aware entry for `assertRaises*`: the context form accepts `msg=`;
/// the callable form forwards every keyword to the callable.
fn assert_raises_with_keywords(args: &[i64], keywords: &[(String, i64)], with_regex: bool) -> i64 {
    let (Some(&instance), Some(&expected)) = (args.first(), args.get(1)) else {
        return missing_arguments(if with_regex {
            "assertRaisesRegex"
        } else {
            "assertRaises"
        });
    };
    let regex = if with_regex {
        let Some(&regex) = args.get(2) else {
            return missing_arguments("assertRaisesRegex");
        };
        Some(regex)
    } else {
        None
    };
    let callable_index = if with_regex { 3 } else { 2 };
    if let Some(&callable) = args.get(callable_index) {
        let call_args = &args[callable_index + 1..];
        return assert_raises_callable(
            instance, expected, regex, callable, call_args, keywords, with_regex,
        );
    }
    let mut msg = NONE;
    for (name, item) in keywords {
        if name == "msg" {
            msg = *item;
        } else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("{name:?} is an invalid keyword argument for this function"),
            );
        }
    }
    make_raises_context(instance, expected, regex, msg, NONE, with_regex)
}

fn assert_raises_callable(
    instance: i64,
    expected: i64,
    regex: Option<i64>,
    callable: i64,
    call_args: &[i64],
    keywords: &[(String, i64)],
    with_regex: bool,
) -> i64 {
    if !validate_exception_type(expected, with_regex) {
        return NONE;
    }
    if let Some(regex) = regex {
        if !validate_regex(regex) {
            return NONE;
        }
    }
    let name = callable_name(callable);
    if keywords.is_empty() {
        builtins::call(callable, call_args.len(), call_args.as_ptr());
    } else {
        let words = keyword_words(keywords);
        builtins::call_kw(
            callable,
            call_args.len(),
            call_args.as_ptr(),
            keywords.len(),
            words.as_ptr(),
        );
    }
    if !exc::pending() {
        return raise_assertion(missing_exception_message_for(expected, Some(&name)));
    }
    let error = take_pending();
    let matches = builtins::isinstance(error, expected);
    if exc::pending() || !value::truthy(matches) {
        // Re-raise the unexpected exception unchanged.
        return exc::raise_value(error);
    }
    if let Some(regex) = regex {
        if !regex_matches(regex, error) {
            return raise_assertion(format!(
                "\"{}\" does not match \"{}\"",
                regex_pattern_text(regex),
                value::to_string(error)
            ));
        }
    }
    let _ = instance;
    NONE
}

fn make_raises_context(
    instance: i64,
    expected: i64,
    regex: Option<i64>,
    msg: i64,
    obj_name: i64,
    with_regex: bool,
) -> i64 {
    if !validate_exception_type(expected, with_regex) {
        return NONE;
    }
    if let Some(regex) = regex {
        if !validate_regex(regex) {
            return NONE;
        }
    }
    let context = value::instance_new(raises_context_class());
    let roots = heap::root_values(&[context]);
    value::instance_setattr(context, "test_case".to_string(), instance);
    value::instance_setattr(context, "expected".to_string(), expected);
    value::instance_setattr(context, "expected_regex".to_string(), regex.unwrap_or(NONE));
    value::instance_setattr(context, "msg".to_string(), msg);
    value::instance_setattr(context, "obj_name".to_string(), obj_name);
    heap::release_roots(roots);
    context
}

/// CPython's `_is_subtype(expected, BaseException)`.
fn validate_exception_type(expected: i64, with_regex: bool) -> bool {
    let name = if with_regex {
        "assertRaisesRegex"
    } else {
        "assertRaises"
    };
    let base = exc::exception_class(BASE_EXCEPTION);
    let valid = if let Some(items) = value::as_tuple(expected) {
        !items.is_empty()
            && items
                .iter()
                .all(|item| value::as_class(*item).is_some() && value::is_subclass_of(*item, base))
    } else {
        value::as_class(expected).is_some() && value::is_subclass_of(expected, base)
    };
    if valid {
        return true;
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!("{name}() arg 1 must be an exception type or tuple of exception types"),
    );
    false
}

fn validate_regex(regex: i64) -> bool {
    if value::as_str(regex).is_none() {
        let search = methods::getattr(regex, b"search".as_ptr(), 6);
        if !exc::pending() && search != NONE {
            return true;
        }
        if exc::pending() {
            exc::clear();
        }
    }
    crate::modules::re::call("compile", std::slice::from_ref(&regex));
    if exc::pending() {
        return false;
    }
    true
}

/// `expected.__name__` or `str(expected)` for failure messages.
fn expected_name(expected: i64) -> String {
    if let Some(name) = value::class_display_name(expected) {
        return name.to_string();
    }
    if let Some(items) = value::as_tuple(expected) {
        if let Some(first) = items.first() {
            return expected_name(*first);
        }
    }
    value::to_string(expected)
}

fn callable_name(callable: i64) -> String {
    let name = methods::getattr(callable, b"__name__".as_ptr(), 8);
    if exc::pending() {
        exc::clear();
        return value::to_string(callable);
    }
    value::as_str(name)
        .map(str::to_string)
        .unwrap_or_else(|| value::to_string(callable))
}

fn missing_exception_message_for(expected: i64, obj_name: Option<&str>) -> String {
    let name = expected_name(expected);
    match obj_name {
        Some(obj_name) => format!("{name} not raised by {obj_name}"),
        None => format!("{name} not raised"),
    }
}

/// The `pattern` of a compiled regex, or the literal text of a string.
fn regex_pattern_text(regex: i64) -> String {
    if let Some(text) = value::as_str(regex) {
        return text.to_string();
    }
    let pattern = methods::getattr(regex, b"pattern".as_ptr(), 7);
    if exc::pending() {
        exc::clear();
        return value::to_string(regex);
    }
    value::as_str(pattern)
        .map(str::to_string)
        .unwrap_or_else(|| value::to_string(regex))
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
    let found = if exc::pending() {
        if exc::pending_tag() != i64::from(ATTRIBUTE_ERROR) {
            return result;
        }
        exc::clear();
        false
    } else {
        true
    };
    if found == expected {
        return NONE;
    }
    let owner = attribute_owner(object);
    let standard = if expected {
        format!("{owner} has no attribute {name:?}")
    } else {
        format!("{owner} has unexpected attribute {name:?}")
    };
    raise_assertion(assertion_message(args, 3, standard))
}

/// CPython's object description used by `assertHasAttr` failures.
fn attribute_owner(object: i64) -> String {
    if let Some(state) = value::as_module(object) {
        return format!("module '{}'", state.name);
    }
    if value::as_class(object).is_some() {
        return format!("type object '{}'", class_name(object));
    }
    format!("'{}' object", value::type_name(object))
}

fn raise_assertion(message: String) -> i64 {
    exc::raise_with_message(ASSERTION_ERROR, &message)
}

fn assertion_message(args: &[i64], index: usize, standard: String) -> String {
    let instance = args.first().copied().unwrap_or(NONE);
    let msg = args.get(index).copied().unwrap_or(NONE);
    format_message_text(instance, msg, standard)
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
            value::instance_setattr(result, "_mirrorOutput".to_string(), FALSE);
            NONE
        }
        "stopTest" | "startTestRun" | "stopTestRun" | "addSuccess" | "printErrors" => NONE,
        "addError" => {
            maybe_failfast(args.first().copied().unwrap_or(NONE));
            result_add_pair(args, "errors")
        }
        "addFailure" => {
            maybe_failfast(args.first().copied().unwrap_or(NONE));
            result_add_pair(args, "failures")
        }
        "addExpectedFailure" => result_add_pair(args, "expectedFailures"),
        "addSubTest" => result_add_subtest(args),
        "addSkip" => {
            let (Some(&result), Some(&test), Some(&reason)) =
                (args.first(), args.get(1), args.get(2))
            else {
                return missing_arguments("TestResult.addSkip");
            };
            append_pair(result, "skipped", test, reason)
        }
        "addUnexpectedSuccess" => {
            let (Some(&result), Some(&test)) = (args.first(), args.get(1)) else {
                return missing_arguments("TestResult.addUnexpectedSuccess");
            };
            maybe_failfast(result);
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

/// Native methods of `TextTestResult`.
fn text_test_result_method(method: &str, args: &[i64]) -> i64 {
    match method {
        "__init__" => {
            let Some(&result) = args.first() else {
                return missing_arguments("TextTestResult.__init__");
            };
            result_init(args);
            let verbosity = args
                .get(3)
                .copied()
                .and_then(value::as_integer)
                .unwrap_or(1);
            value::instance_setattr(
                result,
                "showAll".to_string(),
                value::bool_value(verbosity > 1),
            );
            value::instance_setattr(
                result,
                "dots".to_string(),
                value::bool_value(verbosity == 1),
            );
            value::instance_setattr(result, "_newline".to_string(), TRUE);
            // `TextTestResult(stream, descriptions, verbosity, *,
            // durations=None)`; the native constructor has no keyword slot,
            // so the runner sets it after construction.
            value::instance_setattr(result, "durations".to_string(), NONE);
            NONE
        }
        "getDescription" => {
            let (Some(&result), Some(&test)) = (args.first(), args.get(1)) else {
                return missing_arguments("TextTestResult.getDescription");
            };
            let described = value::to_string(test);
            let doc = call_method(test, "shortDescription", &[]);
            let descriptions = value::instance_attr(result, "descriptions")
                .map(value::truthy)
                .unwrap_or(true);
            if descriptions && doc != NONE && value::truthy(doc) {
                value::str_alloc(format!("{described}\n{}", value::to_string(doc)))
            } else {
                value::str_alloc(described)
            }
        }
        "startTest" => {
            let (Some(&result), Some(&_test)) = (args.first(), args.get(1)) else {
                return missing_arguments("TextTestResult.startTest");
            };
            test_result_method("startTest", args);
            if result_attr_truthy(result, "showAll") {
                let stream = value::instance_attr(result, "stream").unwrap_or(NONE);
                let description = text_test_result_method("getDescription", args);
                stream_write(stream, &value::to_string(description));
                stream_write(stream, " ... ");
                stream_flush(stream);
                value::instance_setattr(result, "_newline".to_string(), FALSE);
            }
            NONE
        }
        "_write_status" => {
            let (Some(&result), Some(&test), Some(&status)) =
                (args.first(), args.get(1), args.get(2))
            else {
                return missing_arguments("TextTestResult._write_status");
            };
            write_status(result, test, &value::to_string(status));
            NONE
        }
        "addSubTest" => {
            let (Some(&result), Some(&test), Some(&subtest), Some(&error)) =
                (args.first(), args.get(1), args.get(2), args.get(3))
            else {
                return missing_arguments("TextTestResult.addSubTest");
            };
            if error != NONE {
                // CPython classifies against `test.failureException` (the
                // TestCase), not the ephemeral `_SubTest`; a subclass that
                // overrides `failureException` must still record the subtest
                // as a failure rather than an error.
                let failure = error_is_failure(error, test);
                if result_attr_truthy(result, "showAll") {
                    write_status(result, subtest, if failure { "FAIL" } else { "ERROR" });
                } else if result_attr_truthy(result, "dots") {
                    stream_write(
                        value::instance_attr(result, "stream").unwrap_or(NONE),
                        if failure { "F" } else { "E" },
                    );
                    stream_flush(value::instance_attr(result, "stream").unwrap_or(NONE));
                }
                result_add_subtest(args);
            }
            NONE
        }
        "addSuccess" => {
            let (Some(&result), Some(&test)) = (args.first(), args.get(1)) else {
                return missing_arguments("TextTestResult.addSuccess");
            };
            if result_attr_truthy(result, "showAll") {
                write_status(result, test, "ok");
            } else if result_attr_truthy(result, "dots") {
                let stream = value::instance_attr(result, "stream").unwrap_or(NONE);
                stream_write(stream, ".");
                stream_flush(stream);
            }
            NONE
        }
        "addError" => {
            let (Some(&result), Some(&test), Some(&error)) =
                (args.first(), args.get(1), args.get(2))
            else {
                return missing_arguments("TextTestResult.addError");
            };
            maybe_failfast(result);
            let text = value::str_alloc(error_text(error));
            append_pair(result, "errors", test, text);
            value::instance_setattr(result, "_mirrorOutput".to_string(), TRUE);
            text_status(result, test, "ERROR");
            NONE
        }
        "addFailure" => {
            let (Some(&result), Some(&test), Some(&error)) =
                (args.first(), args.get(1), args.get(2))
            else {
                return missing_arguments("TextTestResult.addFailure");
            };
            maybe_failfast(result);
            let text = value::str_alloc(error_text(error));
            append_pair(result, "failures", test, text);
            value::instance_setattr(result, "_mirrorOutput".to_string(), TRUE);
            text_status(result, test, "FAIL");
            NONE
        }
        "addSkip" => {
            let (Some(&result), Some(&test), Some(&reason)) =
                (args.first(), args.get(1), args.get(2))
            else {
                return missing_arguments("TextTestResult.addSkip");
            };
            append_pair(result, "skipped", test, reason);
            let text = format!("skipped {}", value::repr(reason));
            text_status(result, test, &text);
            NONE
        }
        "addExpectedFailure" => {
            let (Some(&result), Some(&test), Some(&error)) =
                (args.first(), args.get(1), args.get(2))
            else {
                return missing_arguments("TextTestResult.addExpectedFailure");
            };
            let text = value::str_alloc(error_text(error));
            append_pair(result, "expectedFailures", test, text);
            if result_attr_truthy(result, "showAll") {
                let stream = value::instance_attr(result, "stream").unwrap_or(NONE);
                stream_writeln(stream, "expected failure");
            } else if result_attr_truthy(result, "dots") {
                let stream = value::instance_attr(result, "stream").unwrap_or(NONE);
                stream_write(stream, "x");
                stream_flush(stream);
            }
            NONE
        }
        "addUnexpectedSuccess" => {
            let (Some(&result), Some(&test)) = (args.first(), args.get(1)) else {
                return missing_arguments("TextTestResult.addUnexpectedSuccess");
            };
            maybe_failfast(result);
            append_value(result, "unexpectedSuccesses", test);
            if result_attr_truthy(result, "showAll") {
                let stream = value::instance_attr(result, "stream").unwrap_or(NONE);
                stream_writeln(stream, "unexpected success");
            } else if result_attr_truthy(result, "dots") {
                let stream = value::instance_attr(result, "stream").unwrap_or(NONE);
                stream_write(stream, "u");
                stream_flush(stream);
            }
            NONE
        }
        "printErrors" => {
            let Some(&result) = args.first() else {
                return missing_arguments("TextTestResult.printErrors");
            };
            let stream = value::instance_attr(result, "stream").unwrap_or(NONE);
            if result_attr_truthy(result, "dots") || result_attr_truthy(result, "showAll") {
                stream_writeln(stream, "");
            }
            let errors = value::instance_attr(result, "errors").unwrap_or(NONE);
            text_print_error_list(result, "ERROR", errors);
            let failures = value::instance_attr(result, "failures").unwrap_or(NONE);
            text_print_error_list(result, "FAIL", failures);
            let unexpected = value::instance_attr(result, "unexpectedSuccesses").unwrap_or(NONE);
            if let Some(items) = value::as_list(unexpected) {
                if !items.is_empty() {
                    let separator = object_attribute(result, "separator1")
                        .map(value::to_string)
                        .unwrap_or_default();
                    stream_writeln(stream, &separator);
                    for &test in items {
                        let description =
                            text_test_result_method("getDescription", &[result, test]);
                        stream_writeln(
                            stream,
                            &format!("UNEXPECTED SUCCESS: {}", value::to_string(description)),
                        );
                    }
                }
            }
            NONE
        }
        "printErrorList" => {
            let (Some(&result), Some(&flavour), Some(&errors)) =
                (args.first(), args.get(1), args.get(2))
            else {
                return missing_arguments("TextTestResult.printErrorList");
            };
            text_print_error_list(result, &value::to_string(flavour), errors);
            NONE
        }
        _ => missing_method("TextTestResult", method),
    }
}

fn result_attr_truthy(result: i64, name: &str) -> bool {
    object_attribute(result, name).is_some_and(value::truthy)
}

/// An instance attribute with the normal class-attribute fallback.
fn object_attribute(instance: i64, name: &str) -> Option<i64> {
    if let Some(value) = value::instance_attr(instance, name) {
        return Some(value);
    }
    let class = value::type_class(instance);
    match value::class_lookup(class, name) {
        Some(value::ClassEntry::Attr(item)) => Some(item),
        _ => None,
    }
}

fn text_status(result: i64, test: i64, status: &str) {
    if result_attr_truthy(result, "showAll") {
        write_status(result, test, status);
    } else if result_attr_truthy(result, "dots") {
        let stream = value::instance_attr(result, "stream").unwrap_or(NONE);
        let dot = match status {
            "ERROR" => "E",
            "FAIL" => "F",
            _ if status.starts_with("skipped") => "s",
            _ => ".",
        };
        stream_write(stream, dot);
        stream_flush(stream);
    } else {
        let _ = test;
    }
}

/// `TextTestResult._write_status`: finish the current line and print the
/// description followed by `status`.
fn write_status(result: i64, test: i64, status: &str) {
    let stream = value::instance_attr(result, "stream").unwrap_or(NONE);
    let newline = result_attr_truthy(result, "_newline");
    let is_subtest = value::as_instance(test)
        .map(|state| state.class == subtest_class())
        .unwrap_or(false);
    if is_subtest || newline {
        if !newline {
            stream_writeln(stream, "");
        }
        if is_subtest {
            stream_write(stream, "  ");
        }
        let description = text_test_result_method("getDescription", &[result, test]);
        stream_write(stream, &value::to_string(description));
        stream_write(stream, " ... ");
    }
    stream_writeln(stream, status);
    stream_flush(stream);
    value::instance_setattr(result, "_newline".to_string(), TRUE);
}

fn text_print_error_list(result: i64, flavour: &str, errors: i64) {
    let Some(items) = value::as_list(errors) else {
        return;
    };
    let stream = value::instance_attr(result, "stream").unwrap_or(NONE);
    let separator1 = object_attribute(result, "separator1")
        .map(value::to_string)
        .unwrap_or_default();
    let separator2 = object_attribute(result, "separator2")
        .map(value::to_string)
        .unwrap_or_default();
    for &item in items {
        let Some(pair) = value::as_tuple(item) else {
            continue;
        };
        let test = pair.first().copied().unwrap_or(NONE);
        let message = pair.get(1).copied().unwrap_or(NONE);
        let description = text_test_result_method("getDescription", &[result, test]);
        stream_writeln(stream, &separator1);
        stream_writeln(
            stream,
            &format!("{flavour}: {}", value::to_string(description)),
        );
        stream_writeln(stream, &separator2);
        stream_writeln(stream, &value::to_string(message));
        stream_flush(stream);
    }
}

fn stream_write(stream: i64, text: &str) {
    if stream == NONE {
        return;
    }
    let argument = value::str_alloc(text.to_string());
    let roots = heap::root_values(std::slice::from_ref(&argument));
    methods::call(stream, b"write".as_ptr(), 5, 1, &argument as *const i64);
    if exc::pending() {
        exc::clear();
    }
    heap::release_roots(roots);
}

fn stream_writeln(stream: i64, text: &str) {
    stream_write(stream, text);
    stream_write(stream, "\n");
}

fn stream_flush(stream: i64) {
    if stream == NONE {
        return;
    }
    methods::call(stream, b"flush".as_ptr(), 5, 0, std::ptr::null());
    if exc::pending() {
        exc::clear();
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
    // CPython's class-level run bookkeeping, also read by `TestSuite`.
    value::instance_setattr(result, "_testRunEntered".to_string(), FALSE);
    value::instance_setattr(result, "_previousTestClass".to_string(), NONE);
    value::instance_setattr(result, "_moduleSetUpFailed".to_string(), FALSE);
    value::instance_setattr(result, "_mirrorOutput".to_string(), FALSE);
    value::instance_setattr(result, "_stdout_buffer".to_string(), NONE);
    value::instance_setattr(result, "_stderr_buffer".to_string(), NONE);
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

/// CPython's `@failfast` wrapper: stop the run before recording an outcome
/// when the result was constructed with `failfast=True`.
fn maybe_failfast(result: i64) {
    if result == NONE {
        return;
    }
    if value::instance_attr(result, "failfast").is_some_and(value::truthy) {
        value::instance_setattr(result, "shouldStop".to_string(), TRUE);
    }
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
    maybe_failfast(result);
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
            let stream = args.get(1).copied().unwrap_or(NONE);
            let stream = if stream == NONE {
                module_attr("sys", "stderr").unwrap_or(NONE)
            } else {
                stream
            };
            value::instance_setattr(runner, "stream".to_string(), stream);
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
                "warnings".to_string(),
                args.get(7).copied().unwrap_or(NONE),
            );
            value::instance_setattr(
                runner,
                "tb_locals".to_string(),
                args.get(8).copied().unwrap_or(FALSE),
            );
            value::instance_setattr(
                runner,
                "durations".to_string(),
                args.get(9).copied().unwrap_or(NONE),
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
            let stream = value::instance_attr(runner, "stream").unwrap_or(NONE);
            let descriptions = value::instance_attr(runner, "descriptions").unwrap_or(TRUE);
            let verbosity =
                value::instance_attr(runner, "verbosity").unwrap_or_else(|| encode_int(1));
            let call_args = [stream, descriptions, verbosity];
            let result = builtins::call(class, call_args.len(), call_args.as_ptr());
            if exc::pending() {
                return NONE;
            }
            if let Some(failfast) = value::instance_attr(runner, "failfast") {
                value::instance_setattr(result, "failfast".to_string(), failfast);
            }
            if let Some(buffer) = value::instance_attr(runner, "buffer") {
                value::instance_setattr(result, "buffer".to_string(), buffer);
            }
            if let Some(tb_locals) = value::instance_attr(runner, "tb_locals") {
                value::instance_setattr(result, "tb_locals".to_string(), tb_locals);
            }
            if value::as_instance(result).is_some() {
                if let Some(durations) = value::instance_attr(runner, "durations") {
                    value::instance_setattr(result, "durations".to_string(), durations);
                }
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
    let roots = heap::root_values(&[result, runner]);
    let started = std::time::Instant::now();
    if has_method(result, "startTestRun") {
        call_method(result, "startTestRun", &[]);
    }
    if is_suite_like(test) {
        let _ = run_suite_root(test, result);
    } else {
        run_test_item(test, result);
    }
    if has_method(result, "stopTestRun") {
        call_method(result, "stopTestRun", &[]);
    }
    if exc::pending() {
        heap::release_roots(roots);
        return NONE;
    }
    let elapsed = started.elapsed().as_secs_f64();
    if has_method(result, "printErrors") {
        call_method(result, "printErrors", &[]);
    }
    if value::instance_attr(runner, "durations").is_some_and(|durations| durations != NONE) {
        print_durations(runner, result);
    }
    let stream = value::instance_attr(runner, "stream").unwrap_or(NONE);
    let separator2 = object_attribute(result, "separator2")
        .filter(|value| *value != NONE)
        .map(value::to_string);
    if let Some(separator) = &separator2 {
        stream_writeln(stream, separator);
    }
    let tests_run = result_integer(result, "testsRun").unwrap_or(0);
    let plural = if tests_run == 1 { "" } else { "s" };
    stream_writeln(
        stream,
        &format!("Ran {tests_run} test{plural} in {elapsed:.3}s"),
    );
    stream_writeln(stream, "");
    let successful = has_method(result, "wasSuccessful")
        && value::truthy(call_method(result, "wasSuccessful", &[]));
    let failures = result_list_len(result, "failures");
    let errors = result_list_len(result, "errors");
    let skipped = result_list_len(result, "skipped");
    let expected = result_list_len(result, "expectedFailures");
    let unexpected = result_list_len(result, "unexpectedSuccesses");
    let mut infos = Vec::new();
    if successful {
        if tests_run == 0 && skipped == 0 {
            stream_write(stream, "NO TESTS RAN");
        } else {
            stream_write(stream, "OK");
        }
    } else {
        stream_write(stream, "FAILED");
        if failures > 0 {
            infos.push(format!("failures={failures}"));
        }
        if errors > 0 {
            infos.push(format!("errors={errors}"));
        }
    }
    if skipped > 0 {
        infos.push(format!("skipped={skipped}"));
    }
    if expected > 0 {
        infos.push(format!("expected failures={expected}"));
    }
    if unexpected > 0 {
        infos.push(format!("unexpected successes={unexpected}"));
    }
    if infos.is_empty() {
        stream_writeln(stream, "");
    } else {
        stream_writeln(stream, &format!(" ({})", infos.join(", ")));
    }
    stream_flush(stream);
    heap::release_roots(roots);
    result
}

/// `TextTestRunner._printDurations`: print the slowest tests, honouring the
/// runner's verbosity and duration limit.
fn print_durations(runner: i64, result: i64) {
    let Some(items) = value::instance_attr(result, "collectedDurations").and_then(value::as_list)
    else {
        return;
    };
    if items.is_empty() {
        return;
    }
    let mut entries: Vec<(f64, i64)> = Vec::with_capacity(items.len());
    for &item in items {
        let Some(pair) = value::as_tuple(item) else {
            continue;
        };
        let Some(label) = pair.first().copied() else {
            continue;
        };
        let elapsed = pair.get(1).copied().and_then(numeric_value).unwrap_or(0.0);
        entries.push((elapsed, label));
    }
    if entries.is_empty() {
        return;
    }
    entries.sort_by(|left, right| right.0.total_cmp(&left.0));
    let limit = value::instance_attr(runner, "durations")
        .and_then(value::as_integer)
        .unwrap_or(-1);
    if limit > 0 {
        entries.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }
    let stream = value::instance_attr(runner, "stream").unwrap_or(NONE);
    stream_writeln(stream, "Slowest test durations");
    if let Some(separator) = object_attribute(result, "separator2") {
        stream_writeln(stream, &value::to_string(separator));
    }
    let verbosity = value::instance_attr(runner, "verbosity")
        .and_then(value::as_integer)
        .unwrap_or(1);
    let mut hidden = false;
    for (elapsed, label) in entries {
        if verbosity < 2 && elapsed < 0.001 {
            hidden = true;
            continue;
        }
        let timing = format!("{elapsed:.3}s");
        stream_writeln(stream, &format!("{timing:<10} {}", value::to_string(label)));
    }
    if hidden {
        stream_writeln(
            stream,
            "\n(durations < 0.001s were hidden; use -v to show these durations)",
        );
    } else {
        stream_writeln(stream, "");
    }
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

/// The cached `_FailedTest` class: a synthetic test case that re-raises the
/// exception a loader could not handle.
fn failed_test_class() -> i64 {
    static METHODS: [(&str, &str, usize); 8] = [
        ("__init__", "_FailedTest.__init__", 2),
        ("__getattr__", "_FailedTest.__getattr__", 2),
        ("run", "_FailedTest.run", 2),
        ("countTestCases", "_FailedTest.countTestCases", 1),
        ("id", "_FailedTest.id", 1),
        ("shortDescription", "_FailedTest.shortDescription", 1),
        ("__str__", "_FailedTest.__str__", 1),
        ("__repr__", "_FailedTest.__repr__", 1),
    ];
    let class = cached_class(FAILED_TEST_KEY, "_FailedTest", NONE, &METHODS);
    value::class_set_attr(class, "failureException".to_string(), NONE);
    class
}

fn failed_test_method(method: &str, args: &[i64]) -> i64 {
    match method {
        "__init__" => {
            let (Some(&instance), Some(&name)) = (args.first(), args.get(1)) else {
                return missing_arguments("_FailedTest.__init__");
            };
            value::instance_setattr(instance, "_testMethodName".to_string(), name);
            value::instance_setattr(
                instance,
                "_exception".to_string(),
                args.get(2).copied().unwrap_or(NONE),
            );
            NONE
        }
        "__getattr__" => {
            let (Some(&instance), Some(&name)) = (args.first(), args.get(1)) else {
                return missing_arguments("_FailedTest.__getattr__");
            };
            let method_name = value::instance_attr(instance, "_testMethodName")
                .and_then(value::as_str)
                .unwrap_or("");
            let requested = value::as_str(name).unwrap_or("");
            if requested != method_name {
                return missing_method("_FailedTest", requested);
            }
            let error = value::instance_attr(instance, "_exception").unwrap_or(NONE);
            if error == NONE {
                return NONE;
            }
            exc::raise_value(error)
        }
        "run" => {
            let (Some(&instance), Some(&result)) = (args.first(), args.get(1)) else {
                return missing_arguments("_FailedTest.run");
            };
            call_method(result, "startTest", &[instance]);
            let error = value::instance_attr(instance, "_exception").unwrap_or(NONE);
            if error != NONE {
                call_method(result, "addError", &[instance, error_tuple(error)]);
            }
            call_method(result, "stopTest", &[instance]);
            result
        }
        "countTestCases" => encode_int(1),
        "id" | "__str__" => {
            let name = args
                .first()
                .and_then(|instance| value::instance_attr(*instance, "_testMethodName"))
                .unwrap_or(NONE);
            value::str_alloc(format!(
                "unittest.loader._FailedTest.{}",
                value::to_string(name)
            ))
        }
        "shortDescription" => NONE,
        "__repr__" => {
            let name = args
                .first()
                .and_then(|instance| value::instance_attr(*instance, "_testMethodName"))
                .unwrap_or(NONE);
            value::str_alloc(format!("<_FailedTest {}>", value::to_string(name)))
        }
        _ => missing_method("_FailedTest", method),
    }
}

/// Builds CPython's failed-test suite and records the message on the loader.
fn make_failed_test(loader: i64, method_name: &str, error: i64, message: String) -> i64 {
    if let Some(errors) =
        value::instance_attr(loader, "errors").filter(|list| value::as_list(*list).is_some())
    {
        let text = value::str_alloc(message);
        let roots = heap::root_values(&[text]);
        container::list_append(errors, text);
        heap::release_roots(roots);
    }
    let instance = value::instance_new(failed_test_class());
    let name = value::str_alloc(method_name.to_string());
    let roots = heap::root_values(&[instance, name]);
    value::instance_setattr(instance, "_testMethodName".to_string(), name);
    value::instance_setattr(instance, "_exception".to_string(), error);
    heap::release_roots(roots);
    make_suite(vec![instance])
}

/// `_make_failed_import_test`: the ImportError's text is the loader message.
fn make_failed_import_test(loader: i64, name: &str, error: i64) -> i64 {
    let message = format!(
        "Failed to import test module: {name}\n{}",
        error_text(error)
    );
    make_failed_test(loader, name, error, message)
}

/// Imports `name` and returns the module value, or `None` with the pending
/// exception left for the caller.
fn import_module_value(name: &str) -> Option<i64> {
    let argument = value::str_alloc(name.to_string());
    let roots = heap::root_values(std::slice::from_ref(&argument));
    let module = crate::modules::importlib::call("import_module", std::slice::from_ref(&argument));
    heap::release_roots(roots);
    if exc::pending() {
        None
    } else {
        Some(module)
    }
}

/// `getattr(obj, name)` where modules resolve through the module surface.
fn traverse_attr(item: i64, name: &str) -> i64 {
    if let Some(state) = value::as_module(item) {
        return crate::modules::module_get_by_name(&state.name.clone(), name);
    }
    methods::getattr(item, name.as_ptr(), name.len())
}

/// Whether `item` is a module package (`__path__` is not None).
fn is_package_value(item: i64) -> bool {
    let Some(state) = value::as_module(item) else {
        return false;
    };
    let name = state.name.clone();
    let path = crate::modules::module_get_by_name(&name, "__path__");
    if exc::pending() {
        exc::clear();
        false
    } else {
        path != NONE
    }
}

/// Whether `item` is callable (used by `loadTestsFromName`).
fn is_callable(item: i64) -> bool {
    if value::as_function(item).is_some() || value::as_class(item).is_some() {
        return true;
    }
    let call = methods::getattr(item, b"__call__".as_ptr(), 8);
    if exc::pending() {
        exc::clear();
        false
    } else {
        call != NONE
    }
}

/// CPython's `loadTestsFromName` object classification.
fn suite_from_object(loader: i64, object: i64, parent: i64, last_name: &str) -> i64 {
    let test_case = test_case();
    if value::as_module(object).is_some() {
        return loader_method("loadTestsFromModule", &[loader, object, NONE]);
    }
    if value::as_class(object).is_some()
        && object != test_case
        && value::is_subclass_of(object, test_case)
    {
        return loader_method("loadTestsFromTestCase", &[loader, object]);
    }
    if is_suite_like(object) {
        return object;
    }
    if value::as_function(object).is_some()
        && value::as_class(parent).is_some()
        && value::is_subclass_of(parent, test_case)
    {
        // `Class.testMethod`: a function descriptor on a TestCase subclass.
        let argument = value::str_alloc(last_name.to_string());
        let roots = heap::root_values(std::slice::from_ref(&argument));
        let instance = builtins::call(parent, 1, std::slice::from_ref(&argument).as_ptr());
        heap::release_roots(roots);
        if exc::pending() {
            return NONE;
        }
        return make_suite(vec![instance]);
    }
    if is_callable(object) {
        let created = builtins::call(object, 0, std::ptr::null());
        if exc::pending() {
            return NONE;
        }
        if is_suite_like(created) {
            return created;
        }
        if value::as_instance(created).is_some() && has_method(created, "run") {
            return make_suite(vec![created]);
        }
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "calling {} returned {}, not a test",
                value::repr(object),
                value::type_name(created)
            ),
        );
    }
    exc::raise_with_message(
        TYPE_ERROR,
        &format!("don't know how to make test from: {}", value::repr(object)),
    )
}

/// CPython's `TestLoader.loadTestsFromName`.
fn load_tests_from_name(loader: i64, name_value: i64, module_value: i64) -> i64 {
    let Some(name) = value::as_str(name_value).map(str::to_string) else {
        return exc::raise_with_message(TYPE_ERROR, "loadTestsFromName() argument must be str");
    };
    if name.is_empty() {
        return exc::raise_with_message(VALUE_ERROR, "Empty module name");
    }
    let parts: Vec<String> = name.split('.').map(str::to_string).collect();
    let (mut object, consumed, error_case, error_message) = if module_value != NONE {
        if value::as_module(module_value).is_none() {
            return exc::raise_with_message(TYPE_ERROR, "module argument must be a module");
        }
        (module_value, 0usize, NONE, String::new())
    } else {
        let mut found = NONE;
        let mut consumed = 0usize;
        let mut error_case = NONE;
        let mut error_message = String::new();
        for count in (1..=parts.len()).rev() {
            let candidate = parts[..count].join(".");
            match import_module_value(&candidate) {
                Some(module) => {
                    found = module;
                    consumed = count;
                    break;
                }
                None => {
                    let error = take_pending();
                    error_case = make_failed_import_test(loader, &parts[count - 1], error);
                    error_message = format!("Failed to import test module: {}", parts[count - 1]);
                    if count == 1 {
                        return error_case;
                    }
                }
            }
        }
        if found == NONE {
            return error_case;
        }
        (found, consumed, error_case, error_message)
    };
    let mut parent = NONE;
    for part in parts.iter().skip(consumed) {
        let current = object;
        let next = traverse_attr(current, part);
        if exc::pending() {
            let error = take_pending();
            // A package with a failed import reports the ImportError; any
            // other missing attribute is its own AttributeError.
            if is_package_value(object) && error_case != NONE {
                let _ = error_message;
                return error_case;
            }
            let message = format!("Failed to access attribute:\n{}", error_text(error));
            return make_failed_test(loader, part, error, message);
        }
        parent = current;
        object = next;
    }
    let last = parts.last().cloned().unwrap_or_default();
    suite_from_object(loader, object, parent, &last)
}

/// A real `discover` for dotted module names and directories.
///
/// Dotted names use the runtime's loaded-module registry (package recursion
/// walks the already-loaded submodules); directories are listed, filtered by
/// the `test*.py` glob and imported individually, matching CPython's
/// failed-import behaviour for modules this runtime cannot compile.
fn discover_tests(loader: i64, start_dir: i64, pattern: i64) -> i64 {
    let Some(start) = value::as_str(start_dir).map(str::to_string) else {
        return exc::raise_with_message(TYPE_ERROR, "discover() start_dir must be str");
    };
    let pattern = value::as_str(pattern)
        .map(str::to_string)
        .unwrap_or_else(|| "test*.py".to_string());
    if !start.is_empty() && std::path::Path::new(&start).is_dir() {
        return discover_directory(loader, &start, &pattern);
    }
    // Dotted module name: load it and recurse into its loaded children.
    let Some(module) = import_module_value(&start) else {
        let error = take_pending();
        return make_failed_import_test(loader, &start, error);
    };
    let mut suites = vec![loader_method(
        "loadTestsFromModule",
        &[loader, module, NONE],
    )];
    let prefix = format!("{start}.");
    for name in globals::module_names() {
        if !name.starts_with(&prefix) {
            continue;
        }
        let leaf = name.strip_prefix(&prefix).unwrap_or(&name);
        if leaf.contains('.') {
            continue;
        }
        if let Some(child) = import_module_value(&name) {
            suites.push(loader_method("loadTestsFromModule", &[loader, child, NONE]));
        } else {
            exc::clear();
        }
    }
    make_suite(suites)
}

/// Glob matching for `discover`'s `test*.py` pattern (`*` and `?` only).
fn glob_matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut pattern_index, mut name_index) = (0usize, 0usize);
    let (mut star, mut matched) = (None, 0usize);
    while name_index < name.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == name[name_index])
        {
            pattern_index += 1;
            name_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star = Some(pattern_index);
            matched = name_index;
            pattern_index += 1;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            matched += 1;
            name_index = matched;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

/// Directory discovery: one suite per matching `*.py` file and package.
fn discover_directory(loader: i64, start_dir: &str, pattern: &str) -> i64 {
    let listdir = crate::modules::module_get_by_name("os", "listdir");
    if exc::pending() {
        exc::clear();
        return make_suite(Vec::new());
    }
    let argument = value::str_alloc(start_dir.to_string());
    let roots = heap::root_values(std::slice::from_ref(&argument));
    let listing = builtins::call(listdir, 1, std::slice::from_ref(&argument).as_ptr());
    let entries = if exc::pending() {
        exc::clear();
        None
    } else {
        collect_items(listing)
    };
    heap::release_roots(roots);
    let Some(mut entries) = entries else {
        return make_suite(Vec::new());
    };
    entries.sort_by_key(|item| value::to_string(*item));
    let mut suites = Vec::new();
    for entry in entries {
        let Some(name) = value::as_str(entry).map(str::to_string) else {
            continue;
        };
        if name == "__pycache__" || name.starts_with('.') {
            continue;
        }
        let path = format!("{start_dir}/{name}");
        if std::path::Path::new(&path).is_dir() {
            let package_init = format!("{path}/__init__.py");
            if !std::path::Path::new(&package_init).is_file() {
                continue;
            }
            match import_module_value(&name) {
                Some(module) => {
                    suites.push(loader_method(
                        "loadTestsFromModule",
                        &[loader, module, NONE],
                    ));
                }
                None => {
                    let error = take_pending();
                    suites.push(make_failed_import_test(loader, &name, error));
                }
            }
            continue;
        }
        if !name.ends_with(".py") || !glob_matches(pattern, &name) {
            continue;
        }
        let module_name = name.trim_end_matches(".py").to_string();
        match import_module_value(&module_name) {
            Some(module) => {
                suites.push(loader_method(
                    "loadTestsFromModule",
                    &[loader, module, NONE],
                ));
            }
            None => {
                let error = take_pending();
                suites.push(make_failed_import_test(loader, &module_name, error));
            }
        }
    }
    make_suite(suites)
}

fn loader_method(method: &str, args: &[i64]) -> i64 {
    match method {
        "__init__" => {
            let Some(&loader) = args.first() else {
                return missing_arguments("TestLoader.__init__");
            };
            if value::as_instance(loader).is_some() {
                value::instance_setattr(loader, "errors".to_string(), value::list_new(Vec::new()));
                value::instance_setattr(
                    loader,
                    "_loading_packages".to_string(),
                    value::list_new(Vec::new()),
                );
                value::instance_setattr(loader, "_top_level_dir".to_string(), NONE);
            }
            NONE
        }
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
                heap::add_thread_root(instance);
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
            let (Some(&loader), Some(&module)) = (args.first(), args.get(1)) else {
                return missing_arguments("TestLoader.loadTestsFromModule");
            };
            let pattern = args.get(2).copied().unwrap_or(NONE);
            let Some(state) = value::as_module(module) else {
                return exc::raise_with_message(TYPE_ERROR, "module argument must be a module");
            };
            let name = state.name.clone();
            let dictionary = crate::modules::module_dict(&name);
            let tests = load_module_tests(dictionary, test_case());
            let suite = make_suite(tests);
            // CPython's `loadTestsFromModule` delegates to the module's
            // `load_tests` hook and returns the suite that hook produces; a
            // failing hook becomes a failed test on the loader's error list.
            match load_tests_hook(&name) {
                Some(hook) => {
                    let roots = heap::root_values(&[suite]);
                    let returned = call_load_tests(hook, suite, pattern);
                    heap::release_roots(roots);
                    if exc::pending() {
                        let error = take_pending();
                        let message = format!("Failed to call load_tests:\n{}", error_text(error));
                        return make_failed_test(loader, &name, error, message);
                    }
                    returned
                }
                None => suite,
            }
        }
        "loadTestsFromName" => {
            let (Some(&loader), Some(&name)) = (args.first(), args.get(1)) else {
                return missing_arguments("TestLoader.loadTestsFromName");
            };
            let module = args.get(2).copied().unwrap_or(NONE);
            load_tests_from_name(loader, name, module)
        }
        "loadTestsFromNames" => {
            let (Some(&loader), Some(&names)) = (args.first(), args.get(1)) else {
                return missing_arguments("TestLoader.loadTestsFromNames");
            };
            let module = args.get(2).copied().unwrap_or(NONE);
            let items = if value::as_str(names).is_some() {
                vec![names]
            } else {
                collect_items(names).unwrap_or_default()
            };
            let roots = heap::root_values(&items);
            let mut suites = Vec::with_capacity(items.len());
            for item in items {
                let suite = load_tests_from_name(loader, item, module);
                if exc::pending() {
                    heap::release_roots(roots);
                    return NONE;
                }
                heap::add_thread_root(suite);
                suites.push(suite);
            }
            heap::release_roots(roots);
            make_suite(suites)
        }
        "discover" => {
            let (Some(&loader), Some(&start_dir)) = (args.first(), args.get(1)) else {
                return missing_arguments("TestLoader.discover");
            };
            let pattern = args.get(2).copied().unwrap_or(NONE);
            discover_tests(loader, start_dir, pattern)
        }
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
        // The vector is not visible to the collector while later tests are
        // constructed; keep each instance alive until the suite owns it.
        heap::add_thread_root(instance);
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

/// Builds a `TestSuite` from a list of tests.
fn make_suite(tests: Vec<i64>) -> i64 {
    // The test values are not owned by anything until the list copies them;
    // every allocation below may collect, so root the input before any of it.
    let test_roots = heap::root_values(&tests);
    let class = crate::modules::unittest_test_suite_class();
    let instance = value::instance_new(class);
    let list = value::list_new(tests);
    let roots = heap::root_values(&[instance, list]);
    value::instance_setattr(instance, "_tests".to_string(), list);
    heap::release_roots(roots);
    heap::release_roots(test_roots);
    instance
}

// -------------------------------------------------------------------------
// TestProgram
// -------------------------------------------------------------------------

fn program_method(method: &str, args: &[i64]) -> i64 {
    match method {
        "__init__" | "runTests" => {
            let module = main_module(&args[1..], &[]).unwrap_or_else(|| MAIN_MODULE.to_string());
            run_main(&module);
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

/// CPython's `_SubTest._subDescription()`.
fn subtest_description(instance: i64) -> String {
    let mut parts = Vec::new();
    let message = value::instance_attr(instance, "_message").unwrap_or(NONE);
    if message != subtest_message_sentinel() {
        parts.push(format!("[{}]", value::to_string(message)));
    }
    let params = value::instance_attr(instance, "params")
        .or_else(|| value::instance_attr(instance, "_params"));
    if let Some(params) = params.and_then(value::as_dict) {
        if !params.is_empty() {
            let rendered = params
                .iter()
                .map(|(key, item)| format!("{}={}", value::to_string(*key), value::repr(*item)))
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!("({rendered})"));
        }
    }
    if parts.is_empty() {
        "(<subtest>)".to_string()
    } else {
        parts.join(" ")
    }
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
            let params = value::dict_new(Vec::new());
            value::instance_setattr(instance, "_params".to_string(), params);
            value::instance_setattr(instance, "params".to_string(), params);
            value::instance_setattr(instance, "_parent".to_string(), NONE);
            NONE
        }
        "__enter__" => args.first().copied().unwrap_or(NONE),
        "__exit__" => subtest_exit(args),
        "id" => {
            let Some(&instance) = args.first() else {
                return missing_arguments("_SubTest.id");
            };
            let test = value::instance_attr(instance, "test_case").unwrap_or(NONE);
            let described = call_method(test, "id", &[]);
            value::str_alloc(format!(
                "{} {}",
                value::to_string(described),
                subtest_description(instance)
            ))
        }
        "shortDescription" => {
            let Some(&instance) = args.first() else {
                return missing_arguments("_SubTest.shortDescription");
            };
            let test = value::instance_attr(instance, "test_case").unwrap_or(NONE);
            call_method(test, "shortDescription", &[])
        }
        "__str__" => {
            let Some(&instance) = args.first() else {
                return missing_arguments("_SubTest.__str__");
            };
            let test = value::instance_attr(instance, "test_case").unwrap_or(NONE);
            value::str_alloc(format!(
                "{} {}",
                value::to_string(test),
                subtest_description(instance)
            ))
        }
        _ => missing_method("_SubTest", method),
    }
}

/// Restores the enclosing subtest (CPython's `finally: self._subtest = parent`
/// in the `subTest` generator).
fn restore_subtest(instance: i64) {
    let test = value::instance_attr(instance, "test_case").unwrap_or(NONE);
    if test == NONE {
        return;
    }
    let parent = value::instance_attr(instance, "_parent").unwrap_or(NONE);
    value::instance_setattr(test, "_subtest".to_string(), parent);
}

fn subtest_exit(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&exc_type)) = (args.first(), args.get(1)) else {
        return missing_arguments("_SubTest.__exit__");
    };
    let test = value::instance_attr(instance, "test_case").unwrap_or(NONE);
    let result = value::instance_attr(test, "_unittest_result").unwrap_or(NONE);
    // A legacy result (or debug mode) has no `addSubTest`: CPython yields the
    // subtest body directly, so an exception must propagate unchanged.
    if result == NONE || !has_method(result, "addSubTest") {
        restore_subtest(instance);
        return FALSE;
    }
    if exc_type == NONE {
        // A successful subtest is reported when the result supports subtests.
        call_method(result, "addSubTest", &[test, instance, NONE]);
        restore_subtest(instance);
        return FALSE;
    }
    let error = match args.get(2).copied() {
        Some(value) if value != NONE => value,
        _ => exc::pending_value(),
    };
    if error == NONE {
        restore_subtest(instance);
        return FALSE;
    }
    let tag = value::exception_data(error).map(|data| data.tag);
    // `KeyboardInterrupt` always escapes, and the internal control-flow signal
    // is for the outer `TestCase.run` to consume.
    if tag == Some(KEYBOARD_INTERRUPT) || is_should_stop(error) {
        restore_subtest(instance);
        return FALSE;
    }
    // `SkipTest` inside a subtest is reported as a skip of that subtest and
    // suppressed, exactly like `_Outcome.testPartExecutor`.
    if tag == Some(SKIPPED) {
        exc::clear();
        let why = value::exception_data(error)
            .map(|data| data.msg.clone())
            .unwrap_or_default();
        result_add_skip(result, instance, &why);
        restore_subtest(instance);
        if value::instance_attr(result, "failfast").is_some_and(value::truthy) {
            raise_should_stop();
        }
        return TRUE;
    }
    // Under `@expectedFailure`, the first failing subtest is the whole test's
    // expected failure and is reported by `TestCase.run` after the body.
    if value::instance_attr(test, "_unittest_expecting").is_some_and(value::truthy) {
        exc::clear();
        if value::instance_attr(test, "_unittest_expected_failure")
            .is_none_or(|stored| stored == NONE)
        {
            value::instance_setattr(test, "_unittest_expected_failure".to_string(), error);
        }
        // CPython stops the test body once the expected failure is known.
        restore_subtest(instance);
        raise_should_stop();
        return TRUE;
    }
    exc::clear();
    let tuple = error_tuple(error);
    call_method(result, "addSubTest", &[test, instance, tuple]);
    let failfast = value::instance_attr(result, "failfast").is_some_and(value::truthy);
    restore_subtest(instance);
    if failfast {
        raise_should_stop();
    }
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
    let test_case = value::instance_attr(instance, "test_case").unwrap_or(NONE);
    let msg = value::instance_attr(instance, "msg").unwrap_or(NONE);
    let obj_name = value::instance_attr(instance, "obj_name").unwrap_or(NONE);
    if exc_type == NONE {
        let name = if obj_name == NONE {
            None
        } else {
            value::as_str(obj_name).map(str::to_string)
        };
        let standard = missing_exception_message_for(expected, name.as_deref());
        raise_assertion(format_message_text(test_case, msg, standard));
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
            let standard = format!(
                "\"{}\" does not match \"{}\"",
                regex_pattern_text(regex),
                value::to_string(error)
            );
            raise_assertion(format_message_text(test_case, msg, standard));
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
            value::str_alloc(class_text(class))
        }
        "safe_repr" => {
            let Some(&object) = args.first() else {
                return missing_arguments("safe_repr");
            };
            let short = args.get(1).copied().is_some_and(value::truthy);
            let mut text = value::repr(object);
            if exc::pending() {
                exc::clear();
                text = format!("<{} object>", value::type_name(object));
            }
            if short && text.chars().count() >= 80 {
                let truncated: String = text.chars().take(80).collect();
                value::str_alloc(format!("{truncated} [truncated]..."))
            } else {
                value::str_alloc(text)
            }
        }
        "sorted_list_difference" => sorted_list_difference(args),
        "unorderable_list_difference" => unorderable_list_difference(args),
        "_count_diff_all_purpose" => count_diff_all_purpose(args),
        "_count_diff_hashable" => count_diff_all_purpose(args),
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

fn count_diff_all_purpose(args: &[i64]) -> i64 {
    let (Some(&actual), Some(&expected)) = (args.first(), args.get(1)) else {
        return missing_arguments("_count_diff_all_purpose");
    };
    let Some(actual) = collect_items(actual) else {
        return NONE;
    };
    let Some(expected) = collect_items(expected) else {
        return NONE;
    };
    let mut actual: Vec<Option<i64>> = actual.into_iter().map(Some).collect();
    let mut expected: Vec<Option<i64>> = expected.into_iter().map(Some).collect();
    let mut differences: Vec<i64> = Vec::new();
    let mut index = 0usize;
    while index < actual.len() {
        let Some(item) = actual[index] else {
            index += 1;
            continue;
        };
        let mut actual_count = 0i64;
        for slot in actual.iter_mut().skip(index) {
            if slot.is_some_and(|candidate| container::equals(candidate, item)) {
                actual_count += 1;
                *slot = None;
            }
        }
        let mut expected_count = 0i64;
        for slot in expected.iter_mut() {
            if slot.is_some_and(|candidate| container::equals(candidate, item)) {
                expected_count += 1;
                *slot = None;
            }
        }
        if actual_count != expected_count {
            differences.push(value::tuple_new(vec![
                encode_int(actual_count),
                encode_int(expected_count),
                item,
            ]));
        }
        index += 1;
    }
    for slot in expected {
        let Some(item) = slot else {
            continue;
        };
        differences.push(value::tuple_new(vec![encode_int(0), encode_int(1), item]));
    }
    value::list_new(differences)
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
    const MAX_LENGTH: usize = 80;
    const PLACEHOLDER_LEN: usize = 12;
    const MIN_BEGIN_LEN: usize = 5;
    const MIN_END_LEN: usize = 5;
    const MIN_COMMON_LEN: usize = 5;
    const MIN_DIFF_LEN: usize = MAX_LENGTH
        - (MIN_BEGIN_LEN + PLACEHOLDER_LEN + MIN_COMMON_LEN + PLACEHOLDER_LEN + MIN_END_LEN);

    let reprs: Vec<String> = args.iter().map(|value| value::repr(*value)).collect();
    let max_len = reprs
        .iter()
        .map(|text| text.chars().count())
        .max()
        .unwrap_or(0);
    if max_len <= MAX_LENGTH {
        return value::tuple_new(
            reprs
                .into_iter()
                .map(value::str_alloc)
                .collect::<Vec<i64>>(),
        );
    }
    let prefix = common_prefix(&reprs);
    let prefix_len = prefix.chars().count();
    let common_len = MAX_LENGTH as i64
        - (max_len.saturating_sub(prefix_len) + MIN_BEGIN_LEN + PLACEHOLDER_LEN) as i64;
    let items = if common_len > MIN_COMMON_LEN as i64 {
        let shortened = shorten_text(&prefix, MIN_BEGIN_LEN, common_len as usize);
        reprs
            .into_iter()
            .map(|text| value::str_alloc(shortened.clone() + &chars_from(&text, prefix_len)))
            .collect::<Vec<i64>>()
    } else {
        let shortened = shorten_text(&prefix, MIN_BEGIN_LEN, MIN_COMMON_LEN);
        reprs
            .into_iter()
            .map(|text| {
                let rest = chars_from(&text, prefix_len);
                value::str_alloc(
                    shortened.clone() + &shorten_text(&rest, MIN_DIFF_LEN, MIN_END_LEN),
                )
            })
            .collect::<Vec<i64>>()
    };
    value::tuple_new(items)
}

/// `_shorten`: replace the middle of `text` with `[N chars]` when it is
/// longer than the placeholder budget.
fn shorten_text(text: &str, prefix_len: usize, suffix_len: usize) -> String {
    const PLACEHOLDER_LEN: usize = 12;
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let skip = len.saturating_sub(prefix_len + suffix_len);
    if skip > PLACEHOLDER_LEN {
        let mut out = String::new();
        out.extend(chars.iter().take(prefix_len));
        out.push_str(&format!("[{skip} chars]"));
        out.extend(chars.iter().skip(len - suffix_len));
        out
    } else {
        text.to_string()
    }
}

/// The suffix of `text` after its first `count` characters.
fn chars_from(text: &str, count: usize) -> String {
    text.chars().skip(count).collect()
}

fn common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut prefix: Vec<char> = first.chars().collect();
    for item in &items[1..] {
        let other: Vec<char> = item.chars().collect();
        let mut length = prefix.len().min(other.len());
        while length > 0 && prefix[..length] != other[..length] {
            length -= 1;
        }
        prefix.truncate(length);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.into_iter().collect()
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
        // User classes lowered by the frontend keep methods in the class
        // attribute table rather than the native method table.
        for (name, item) in &state.attrs {
            if name.starts_with("test")
                && value::as_function(*item).is_some()
                && !names.iter().any(|existing| existing == name)
            {
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

fn class_flag_truthy(class: i64, name: &str) -> bool {
    match value::class_own_lookup(class, name) {
        Some(value::ClassEntry::Attr(item)) => value::truthy(item),
        _ => class_attr_truthy(class, name),
    }
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

/// `unittest.main(...)`: run the tests the selected module defines.
///
/// CPython dispatches to `TestProgram(module=module)`, whose loader walks the
/// module's dictionary. The first positional argument is `module`; the keyword
/// spelling is also accepted (the spec entry is `kw_dynamic`).
fn call_main(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let Some(module) = main_module(args, keywords) else {
        // `main_module` already raised for an unusable argument.
        return NONE;
    };
    run_main(&module)
}

/// Resolves `unittest.main`'s `module` argument to a module name.
///
/// `None` (the default, or omitted) selects the entry unit. A string names the
/// module directly, exactly like CPython's `__import__` path, and a module
/// object contributes its `__name__`. Anything else is a `TypeError`.
fn main_module(args: &[i64], keywords: &[(String, i64)]) -> Option<String> {
    let target = keywords
        .iter()
        .rev()
        .find(|(name, _)| name == "module")
        .map(|(_, target)| *target)
        .or_else(|| args.first().copied())
        .unwrap_or(NONE);
    if target == NONE {
        return Some(MAIN_MODULE.to_string());
    }
    if let Some(name) = value::as_str(target) {
        return Some(name.to_string());
    }
    if let Some(state) = value::as_module(target) {
        if !state.name.is_empty() {
            return Some(state.name.clone());
        }
        return Some(MAIN_MODULE.to_string());
    }
    exc::raise_with_message(
        TYPE_ERROR,
        "unittest.main() module argument must be a module or a string",
    );
    None
}

/// The module name a class body recorded in `__module__`.
///
/// The class statement publishes `__module__` next to `__qualname__`, so a
/// class defined in an imported helper (`test.support.mapping_tests`) carries
/// that dotted name while the entry unit's classes carry `__main__`. A class
/// built through the native APIs (or through `type()` without an explicit
/// `__module__`) records nothing; the caller treats it as an entry-unit class.
fn class_module(class: i64) -> Option<String> {
    let Some(value::ClassEntry::Attr(module)) = value::class_own_lookup(class, "__module__") else {
        return None;
    };
    value::as_str(module).map(str::to_string)
}

/// Test classes `unittest.main(module=...)` should execute.
///
/// CPython's `loadTestsFromModule` walks `dir(module)` and keeps every
/// `TestCase` subclass it finds there: classes the module imported by name are
/// run, while abstract helpers that merely live in an imported module are not
/// part of the namespace. The runtime mirrors a compiled module's namespace
/// into its global store, so that store is the scan source; `globals::entries`
/// walks names alphabetically, the order `dir(module)` yields.
///
/// The entry unit is the exception: it publishes only the globals its own
/// functions read (nothing imports it, see `Lowerer::set_mirror_all_globals`),
/// so its class statements are absent from that store. The class registry
/// records every `TestCase` subclass in creation order, so the classes whose
/// class body recorded this module in `__module__` are appended from there. A
/// class without any recorded module stays in the entry unit's run, so a
/// natively built class never silently disappears.
fn module_test_classes(module: &str) -> Vec<i64> {
    let case = test_case();
    let mut classes: Vec<i64> = Vec::new();
    let mut push = |class: i64| {
        if class != case && !classes.contains(&class) && value::is_subclass_of(class, case) {
            classes.push(class);
        }
    };
    // Namespace order: the `dir(module)` scan CPython performs.
    for (_, item) in globals::entries(module) {
        push(item);
    }
    // Entry-unit fallback: creation order, the source definition order for the
    // class statements the store does not carry.
    let registered = registry().clone();
    for class in registered {
        if class_module(class).as_deref().unwrap_or(MAIN_MODULE) == module {
            push(class);
        }
    }
    classes
}

/// The selected module's `load_tests` hook, when it defines one.
///
/// CPython's `loadTestsFromModule` delegates to this hook and runs the suite it
/// returns; `load_tests = None` is CPython's documented "no hook" value.
fn load_tests_hook(module: &str) -> Option<i64> {
    match globals::lookup(module, "load_tests") {
        Some(hook) if hook != NONE => Some(hook),
        _ => None,
    }
}

/// The loader's standard tests shaped like CPython's loader output: one suite
/// per `TestCase` subclass, collected into a module suite.
fn standard_suite(classes: &[i64]) -> i64 {
    let marker = heap::root_values(&[]);
    let mut suites = Vec::with_capacity(classes.len());
    for &class in classes {
        let tests = load_class_tests(class);
        if exc::pending() {
            break;
        }
        let suite = make_suite(tests);
        heap::push_scoped_root(suite);
        suites.push(suite);
    }
    let suite = make_suite(suites);
    heap::release_roots(marker);
    suite
}

/// Calls a module's `load_tests(loader, tests, pattern)` hook.
///
/// The loader is `unittest.defaultTestLoader`, matching CPython's
/// `TestProgram`; `pattern` is the caller's discovery pattern (or `None`). A
/// non-callable hook raises `TypeError` here.
fn call_load_tests(hook: i64, standard: i64, pattern: i64) -> i64 {
    let loader = default_test_loader();
    let arguments = [loader, standard, pattern];
    let roots = heap::root_values(&arguments);
    let returned = builtins::call(hook, arguments.len(), arguments.as_ptr());
    heap::release_roots(roots);
    returned
}

/// CPython's `load_tests` protocol: run the suite the hook returns instead of
/// the module's standard tests.
///
/// The returned value is executed through the fixture-aware suite runner, so
/// module/class fixtures and cleanups fire exactly as they do for discovery.
/// A pending exception left by the hook aborts the run.
fn run_load_tests(hook: i64, classes: &[i64], result: i64) -> bool {
    let standard = standard_suite(classes);
    if exc::pending() {
        return false;
    }
    let marker = heap::root_values(&[standard]);
    let returned = call_load_tests(hook, standard, NONE);
    heap::push_scoped_root(returned);
    if exc::pending() {
        heap::release_roots(marker);
        return false;
    }
    let completed = run_suite_root(returned, result);
    heap::release_roots(marker);
    completed
}

/// A suite-like value: a native `TestSuite` (or any instance) carrying a
/// `_tests` list. Bare callables and `TestCase` instances are not suites.
fn is_suite_like(item: i64) -> bool {
    value::as_instance(item).is_some()
        && value::instance_attr(item, "_tests")
            .map(|tests| value::as_list(tests).is_some())
            .unwrap_or(false)
}

fn instance_class_value(item: i64) -> i64 {
    value::as_instance(item)
        .map(|state| state.class)
        .unwrap_or(NONE)
}

/// `unittest.util.strclass`: `module.qualname` (just the qualified name for
/// classes without a recorded module).
fn class_text(class: i64) -> String {
    let name = qualified_class_text(class);
    match class_module(class) {
        Some(module) if module != "builtins" => format!("{module}.{name}"),
        _ => name,
    }
}

/// `Cls.__qualname__`, falling back to the plain class name.
fn qualified_class_text(class: i64) -> String {
    value::class_qualname(class)
        .map(str::to_string)
        .or_else(|| value::class_display_name(class).map(str::to_string))
        .unwrap_or_else(|| "TestCase".to_string())
}

/// Resolves `module.name` for fixture hooks: `sys.modules` first (the actual
/// module object the suite sees), then compiled-module globals and the native
/// module surface.
fn module_attr(module: &str, name: &str) -> Option<i64> {
    if let Some(found) = sys_module_value(module) {
        let attr = methods::getattr(found, name.as_ptr(), name.len());
        if exc::pending() {
            exc::clear();
        } else {
            return Some(attr);
        }
    }
    if let Some(value) = globals::lookup(module, name) {
        return Some(value);
    }
    let value = crate::modules::module_get_by_name(module, name);
    if exc::pending() {
        exc::clear();
        None
    } else {
        Some(value)
    }
}

/// `sys.modules.get(name)`, or `None` when the entry is missing/`None`.
fn sys_module_value(name: &str) -> Option<i64> {
    let modules = crate::modules::module_get_by_name("sys", "modules");
    if exc::pending() {
        exc::clear();
        return None;
    }
    let pairs = value::as_dict(modules)?;
    let wanted = value::str_alloc(name.to_string());
    let roots = heap::root_values(std::slice::from_ref(&wanted));
    let found = pairs
        .iter()
        .find(|(key, _)| container::equals(*key, wanted))
        .map(|(_, item)| *item)
        .filter(|item| *item != NONE);
    heap::release_roots(roots);
    found
}

/// Reports a class/module-level fixture failure under CPython's
/// `_ErrorHolder` description (`"setUpClass (module.Class)"`).
fn report_fixture_exception(result: i64, error: i64, description: String, info: Option<i64>) {
    let holder = value::instance_new(error_holder_class());
    let text = value::str_alloc(description);
    let roots = heap::root_values(&[holder, text]);
    value::instance_setattr(holder, "description".to_string(), text);
    let is_skip = value::exception_data(error).map(|data| data.tag) == Some(SKIPPED);
    if is_skip {
        let why = value::exception_data(error)
            .map(|data| data.msg.clone())
            .unwrap_or_default();
        call_method(result, "addSkip", &[holder, value::str_alloc(why)]);
    } else {
        let info = info.unwrap_or_else(|| error_tuple(error));
        call_method(result, "addError", &[holder, info]);
    }
    heap::release_roots(roots);
}

fn previous_test_class(result: i64) -> i64 {
    value::instance_attr(result, "_previousTestClass").unwrap_or(NONE)
}

/// A list stored as a class attribute (`tearDown_exceptions`).
fn class_attr_list(class: i64, name: &str) -> Option<Vec<i64>> {
    match value::class_lookup(class, name) {
        Some(value::ClassEntry::Attr(item)) => value::as_list(item).map(<[i64]>::to_vec),
        _ => None,
    }
}

fn module_setup_failed(result: i64) -> bool {
    value::instance_attr(result, "_moduleSetUpFailed").is_some_and(value::truthy)
}

/// `TestSuite._tearDownPreviousClass(test, result)`.
fn tear_down_previous_class(test: i64, result: i64) {
    let previous = previous_test_class(result);
    if previous == NONE {
        return;
    }
    let current = if test == NONE {
        NONE
    } else {
        instance_class_value(test)
    };
    if current == previous {
        return;
    }
    if class_attr_truthy(previous, "_classSetupFailed") || module_setup_failed(result) {
        return;
    }
    if class_attr_truthy(previous, "__unittest_skip__") {
        return;
    }
    let parent = class_text(previous);
    let has_teardown = value::class_lookup(previous, "tearDownClass").is_some();
    if has_teardown {
        methods::call(previous, b"tearDownClass".as_ptr(), 13, 0, std::ptr::null());
        if exc::pending() {
            let error = take_pending();
            report_fixture_exception(result, error, format!("tearDownClass ({parent})"), None);
        }
    }
    do_class_cleanups_core(previous);
    if let Some(exceptions) = class_attr_list(previous, "tearDown_exceptions") {
        for info in exceptions {
            let error = value::as_tuple(info)
                .and_then(|parts| parts.get(1).copied())
                .unwrap_or(NONE);
            report_fixture_exception(
                result,
                error,
                format!("tearDownClass ({parent})"),
                Some(info),
            );
        }
    }
}

/// `TestSuite._handleClassSetUp(test, result)`.
fn handle_class_setup(test: i64, result: i64) {
    let class = instance_class_value(test);
    if class == NONE {
        return;
    }
    if class == previous_test_class(result) || module_setup_failed(result) {
        return;
    }
    if class_attr_truthy(class, "__unittest_skip__") {
        return;
    }
    value::class_set_attr(class, "_classSetupFailed".to_string(), FALSE);
    if value::class_lookup(class, "setUpClass").is_none() {
        return;
    }
    let parent = class_text(class);
    methods::call(class, b"setUpClass".as_ptr(), 10, 0, std::ptr::null());
    if exc::pending() {
        let error = take_pending();
        value::class_set_attr(class, "_classSetupFailed".to_string(), TRUE);
        report_fixture_exception(result, error, format!("setUpClass ({parent})"), None);
        do_class_cleanups_core(class);
        if let Some(exceptions) = class_attr_list(class, "tearDown_exceptions") {
            for info in exceptions {
                let error = value::as_tuple(info)
                    .and_then(|parts| parts.get(1).copied())
                    .unwrap_or(NONE);
                report_fixture_exception(
                    result,
                    error,
                    format!("setUpClass ({parent})"),
                    Some(info),
                );
            }
        }
    }
}

/// `TestSuite._get_previous_module(result)`.
fn previous_module(result: i64) -> Option<String> {
    let previous = previous_test_class(result);
    if previous == NONE {
        return None;
    }
    class_module(previous)
}

/// `TestSuite._handleModuleFixture(test, result)`.
fn handle_module_fixture(test: i64, result: i64) {
    let class = instance_class_value(test);
    if class == NONE {
        return;
    }
    let previous = previous_module(result);
    let current = class_module(class);
    if current == previous {
        return;
    }
    handle_module_teardown(result);
    value::instance_setattr(result, "_moduleSetUpFailed".to_string(), FALSE);
    let Some(module) = current else {
        return;
    };
    let Some(setup) = module_attr(&module, "setUpModule") else {
        return;
    };
    let roots = heap::root_values(std::slice::from_ref(&setup));
    builtins::call(setup, 0, std::ptr::null());
    heap::release_roots(roots);
    if exc::pending() {
        let error = take_pending();
        value::instance_setattr(result, "_moduleSetUpFailed".to_string(), TRUE);
        report_fixture_exception(result, error, format!("setUpModule ({module})"), None);
        // CPython still runs module cleanups after a failed setUpModule.
        let cleanup_error = do_module_cleanups(&[]);
        if exc::pending() {
            let error = take_pending();
            let _ = cleanup_error;
            report_fixture_exception(result, error, format!("setUpModule ({module})"), None);
        }
    }
}

/// `TestSuite._handleModuleTearDown(result)`.
fn handle_module_teardown(result: i64) {
    let Some(module) = previous_module(result) else {
        return;
    };
    if module_setup_failed(result) {
        return;
    }
    if let Some(teardown) = module_attr(&module, "tearDownModule") {
        let roots = heap::root_values(std::slice::from_ref(&teardown));
        builtins::call(teardown, 0, std::ptr::null());
        heap::release_roots(roots);
        if exc::pending() {
            let error = take_pending();
            report_fixture_exception(result, error, format!("tearDownModule ({module})"), None);
        }
    }
    let _ = do_module_cleanups(&[]);
    if exc::pending() {
        let error = take_pending();
        report_fixture_exception(result, error, format!("tearDownModule ({module})"), None);
    }
}

/// Runs one test item through the CPython suite fixture protocol.
///
/// Returns `false` when an exception escaped the item (a broken result
/// object), which aborts the run.
fn run_one_test_with_fixtures(test: i64, result: i64) -> bool {
    tear_down_previous_class(test, result);
    handle_module_fixture(test, result);
    handle_class_setup(test, result);
    let class = instance_class_value(test);
    value::instance_setattr(result, "_previousTestClass".to_string(), class);
    if class_attr_truthy(class, "_classSetupFailed") || module_setup_failed(result) {
        return true;
    }
    run_test_item(test, result);
    !exc::pending()
}

/// Recursively runs a suite (or single item) with fixtures.
fn run_suite_items(item: i64, result: i64) -> bool {
    if !is_suite_like(item) {
        return run_one_test_with_fixtures(item, result);
    }
    let Some(tests) = value::instance_attr(item, "_tests").and_then(value::as_list) else {
        return true;
    };
    let items: Vec<i64> = tests.to_vec();
    let roots = heap::root_values(&items);
    let mut completed = true;
    for test in items {
        if value::instance_attr(result, "shouldStop").is_some_and(value::truthy) {
            break;
        }
        let test_roots = heap::root_values(std::slice::from_ref(&test));
        completed = run_suite_items(test, result);
        heap::release_roots(test_roots);
        if !completed {
            break;
        }
    }
    heap::release_roots(roots);
    completed
}

/// CPython's `TestSuite.run`: enter the run, recurse, then tear down the last
/// class and module when this call is the top-level suite run.
pub(crate) fn run_suite_root(item: i64, result: i64) -> bool {
    let top_level = !value::instance_attr(result, "_testRunEntered").is_some_and(value::truthy);
    if top_level {
        value::instance_setattr(result, "_testRunEntered".to_string(), TRUE);
    }
    let completed = run_suite_items(item, result);
    if top_level {
        tear_down_previous_class(NONE, result);
        handle_module_teardown(result);
        value::instance_setattr(result, "_testRunEntered".to_string(), FALSE);
    }
    completed && !exc::pending()
}

/// Runs the test classes `module` defines: one fresh `TestResult`, a printed
/// summary, and a raised assertion on failure.
fn run_main(module: &str) -> i64 {
    let classes = module_test_classes(module);
    let result = new_test_result();
    let result_roots = heap::root_values(&[result]);
    call_method(result, "startTestRun", &[]);
    // A module that defines `load_tests` runs exactly what that hook returns,
    // like CPython's `loadTestsFromModule`; otherwise the standard suites run.
    let completed = match load_tests_hook(module) {
        Some(hook) => run_load_tests(hook, &classes, result),
        None => {
            let suite = standard_suite(&classes);
            let roots = heap::root_values(std::slice::from_ref(&suite));
            let completed = run_suite_root(suite, result);
            heap::release_roots(roots);
            completed
        }
    };
    if !completed {
        heap::release_roots(result_roots);
        return NONE;
    }
    call_method(result, "stopTestRun", &[]);

    let tests_run = result_integer(result, "testsRun").unwrap_or(0);
    println!("Ran {tests_run} test(s)");
    print_failures(result);
    let failures = result_list_len(result, "failures")
        + result_list_len(result, "errors")
        + result_list_len(result, "unexpectedSuccesses");
    heap::release_roots(result_roots);
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
    if let Some(items) = value::as_tuple(error) {
        if let Some(exception) = items.get(1).copied() {
            if exception != NONE {
                return error_text(exception);
            }
        }
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
        result_add_skip(result, instance, &why);
        return;
    }
    if expecting {
        result_add_expected_failure(result, instance, error);
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

/// Binds named arguments into the positional slots a native initializer reads.
/// `names` are the parameters after the receiver; an unknown name reports the
/// same `TypeError` CPython's binder does.
fn bind_named_slots(
    args: &[i64],
    keywords: &[(String, i64)],
    names: &[&str],
    function: &str,
) -> Vec<i64> {
    let mut slots = args.to_vec();
    for (name, item) in keywords {
        let Some(index) = names.iter().position(|candidate| *candidate == name) else {
            exc::raise_with_message(
                TYPE_ERROR,
                &format!("{function}() got an unexpected keyword argument '{name}'"),
            );
            return slots;
        };
        while slots.len() <= index + 1 {
            slots.push(NONE);
        }
        slots[index + 1] = *item;
    }
    slots
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
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        crate::heap::heap_reset_test_guard()
    }

    fn int(n: i64) -> i64 {
        value::encode_int(n).expect("tagged int")
    }

    fn registry_len() -> usize {
        registry().len()
    }

    #[test]
    fn test_case_has_the_expected_native_methods() {
        let _guard = serial();
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
        let _guard = serial();
        exc::init();
        let identity = skip_if(&[FALSE, value::str_alloc("why".to_string())], false);
        // The identity callable returns the decorated item unchanged.
        let function = value::identity_function_new("test_identity");
        assert_eq!(builtins::call(identity, 1, [function].as_ptr()), function);
        assert!(!exc::pending());
    }

    #[test]
    fn assertion_helpers_pass_and_fail() {
        let _guard = serial();
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
        let _guard = serial();
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
        assert!(
            !exc::pending(),
            "runner left exception: {}",
            value::to_string(exc::pending_value())
        );

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
        let _guard = serial();
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

    #[test]
    fn main_module_argument_selects_the_entry_unit() {
        let _guard = serial();
        exc::init();
        // No argument and an explicit `None` both mean the entry unit.
        assert_eq!(main_module(&[], &[]).as_deref(), Some(MAIN_MODULE));
        assert_eq!(main_module(&[NONE], &[]).as_deref(), Some(MAIN_MODULE));
        let named = value::str_alloc("pkg.tests".to_string());
        assert_eq!(main_module(&[named], &[]).as_deref(), Some("pkg.tests"));
        assert_eq!(
            main_module(&[], &[("module".to_string(), named)]).as_deref(),
            Some("pkg.tests")
        );
        let module = value::module_value("pkg.mod".to_string());
        assert_eq!(main_module(&[module], &[]).as_deref(), Some("pkg.mod"));
        assert!(!exc::pending());

        // A non-module, non-string argument is a `TypeError`, like CPython's
        // `module.__dict__` access.
        assert_eq!(main_module(&[TRUE], &[]), None);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::clear();
    }

    #[test]
    fn main_scan_merges_module_namespace_and_registry() {
        let _guard = serial();
        reset();
        exc::init();
        let case = test_case();
        // Entry-unit class: the sparse `__main__` store does not carry it, so
        // only the class registry can supply it.
        let defined = value::class_new_full(
            "DefinedCase".to_string(),
            case,
            Vec::new(),
            vec![(
                "__module__".to_string(),
                value::str_alloc(MAIN_MODULE.to_string()),
            )],
        );
        // A class imported by name into the module namespace: CPython's
        // `dir(module)` scan runs it even though its class body records
        // another module.
        let imported = value::class_new_full(
            "ImportedCase".to_string(),
            case,
            Vec::new(),
            vec![(
                "__module__".to_string(),
                value::str_alloc("test.mapping_tests".to_string()),
            )],
        );
        // A native registration with no `__module__` stays in the entry unit's
        // run, so no class silently disappears.
        let native = value::class_new("NativeCase".to_string(), case, Vec::new());
        // Defined in the helper module but never bound in `__main__`.
        let elsewhere = value::class_new_full(
            "ElsewhereCase".to_string(),
            case,
            Vec::new(),
            vec![(
                "__module__".to_string(),
                value::str_alloc("test.mapping_tests".to_string()),
            )],
        );

        globals::set(MAIN_MODULE, "ImportedCase", imported);
        let selected = module_test_classes(MAIN_MODULE);
        assert!(selected.contains(&defined));
        assert!(selected.contains(&imported));
        assert!(selected.contains(&native));
        assert!(!selected.contains(&elsewhere));
        // The namespace scan runs first; the registry supplies the rest.
        let imported_at = selected.iter().position(|class| *class == imported);
        let defined_at = selected.iter().position(|class| *class == defined);
        assert!(
            matches!((imported_at, defined_at), (Some(imported_at), Some(defined_at)) if imported_at < defined_at)
        );

        // An explicit module selection uses the same scan: the helper's own
        // classes are visible, the entry unit's are not.
        let helpers = module_test_classes("test.mapping_tests");
        assert!(helpers.contains(&imported));
        assert!(helpers.contains(&elsewhere));
        assert!(!helpers.contains(&defined));
        assert!(!helpers.contains(&native));

        globals::delete_value(MAIN_MODULE, "ImportedCase");
        assert!(!exc::pending());
        reset();
    }

    #[test]
    fn load_tests_hook_ignores_missing_and_none_entries() {
        let _guard = serial();
        exc::init();
        let module = "unittest.load_tests_probe";
        assert_eq!(load_tests_hook(module), None);
        // CPython documents `load_tests = None` as "no hook".
        globals::set(module, "load_tests", NONE);
        assert_eq!(load_tests_hook(module), None);
        let hook = value::native_function_new("unittest", "_case_marker", 3);
        globals::set(module, "load_tests", hook);
        assert_eq!(load_tests_hook(module), Some(hook));
        globals::delete_value(module, "load_tests");
        assert!(!exc::pending());
    }

    #[test]
    fn load_tests_returning_none_cannot_pass_silently() {
        let _guard = serial();
        exc::init();
        let module = "unittest.load_tests_none";
        // `_case_marker` accepts the hook's three arguments and returns None,
        // which CPython's runner then fails to call.
        let hook = value::native_function_new("unittest", "_case_marker", 3);
        globals::set(module, "load_tests", hook);
        assert_eq!(run_main(module), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::clear();
        globals::delete_value(module, "load_tests");
    }

    #[test]
    fn load_tests_result_replaces_the_standard_tests() {
        let _guard = serial();
        reset();
        exc::init();
        let module = "unittest.load_tests_replace";
        // Standard discovery would run this failing test.
        let standard = value::class_new(
            "StandardProbe".to_string(),
            test_case(),
            vec![(
                "test_bad".to_string(),
                value::native_function_new("unittest", "_fail", 2),
            )],
        );
        value::class_set_attr(
            standard,
            "__module__".to_string(),
            value::str_alloc(module.to_string()),
        );
        // `_identity` returns the loader argument, a value that is not a test:
        // running it raises `TypeError` instead of reporting the standard
        // class' assertion failure, which proves the hook's result ran.
        let hook = value::native_function_new("unittest", "_identity", 3);
        globals::set(module, "load_tests", hook);
        assert_eq!(run_main(module), NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::clear();
        globals::delete_value(module, "load_tests");
        reset();
    }

    #[test]
    fn standard_run_reports_failures_without_a_hook() {
        let _guard = serial();
        reset();
        exc::init();
        let module = "unittest.standard_probe";
        let standard = value::class_new(
            "StandardOnly".to_string(),
            test_case(),
            vec![(
                "test_bad".to_string(),
                value::native_function_new("unittest", "_fail", 2),
            )],
        );
        value::class_set_attr(
            standard,
            "__module__".to_string(),
            value::str_alloc(module.to_string()),
        );
        assert_eq!(run_main(module), NONE);
        assert_eq!(exc::pending_tag(), i64::from(ASSERTION_ERROR));
        assert!(value::to_string(exc::pending_value()).contains("1 test(s) failed"));
        exc::clear();
        reset();
    }
}
