//! Minimal `unittest` support: a native `TestCase` base class and a runner
//! for registered subclasses.
//!
//! The frontend registers every user class derived from `TestCase` through
//! [`register`] (`value::class_new` calls it when the base carries the hidden
//! [`CASE_MARKER`] method), so `unittest.main()` can discover the classes
//! without importing the Python `unittest` package.
//!
//! The assertion helpers are native methods on `TestCase`; their names are
//! prefixed with `_` and they are internal runtime hooks, validated by the
//! central module spec like any other native function. A failed assertion
//! raises `AssertionError`; the runner counts such failures, prints
//! `FAILED <Class>.<method>` to stderr and keeps going, then raises a single
//! `AssertionError` at the end if anything failed.

use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::builtins;
use crate::container;
use crate::exc::{self, ASSERTION_ERROR, ATTRIBUTE_ERROR, TYPE_ERROR};
use crate::heap;
use crate::methods;
use crate::ops;
use crate::value::{self, NONE};

/// Hidden native method that identifies a `TestCase`-derived class.
const CASE_MARKER: &str = "__unittest_case__";

/// Registered test classes, in creation order.
///
/// Every entry is also installed as a thread root: the registry lives in Rust
/// memory the collector cannot trace. `reset` empties the registry; the old
/// roots stay in place, which only keeps already-dead classes alive.
static REGISTRY: LazyLock<Mutex<Vec<i64>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Locks the registry, ignoring poisoning: no user code runs while it is held.
fn registry() -> MutexGuard<'static, Vec<i64>> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Records a test class created by the frontend. Idempotence is not required:
/// a class is registered exactly once, at creation.
pub(crate) fn register(class: i64) {
    registry().push(class);
    // The registry is a Rust-side owner, so the class must be a collector
    // root. `add_thread_root` is a no-op outside the runtime.
    heap::add_thread_root(class);
}

/// Forgets every registered class (used by tests and a fresh runner start).
pub(crate) fn reset() {
    registry().clear();
}

/// Builds a fresh native `TestCase` class.
///
/// A fresh class per access matches CPython, where `unittest.TestCase` is a
/// single module attribute; returning a new class keeps the registry
/// independent of the number of times the attribute is materialised.
pub(crate) fn test_case() -> i64 {
    // Fixed-size table: every method value stays reachable from this frame
    // (stack/registers) while the following allocations may collect.
    const METHODS: [(&str, &str, usize); 8] = [
        (CASE_MARKER, "_case_marker", 1),
        ("assertHasAttr", "_assert_has_attr", 3),
        ("assertIn", "_assert_in", 3),
        ("assertEqual", "_assert_equal", 3),
        ("assertTrue", "_assert_true", 2),
        ("assertFalse", "_assert_false", 2),
        ("assertIs", "_assert_is", 3),
        ("fail", "_fail", 2),
    ];

    let mut functions = [NONE; METHODS.len()];
    for (slot, (_, native, arity)) in functions.iter_mut().zip(METHODS) {
        *slot = value::native_function_new("unittest", native, arity);
    }
    let methods = METHODS
        .iter()
        .zip(functions)
        .map(|((python_name, _, _), function)| ((*python_name).to_string(), function))
        .collect();
    value::class_new("TestCase".to_string(), NONE, methods)
}

/// Dispatches a native `unittest` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "main" => run_main(),
        _ => dispatch(function, args),
    }
}

/// The runner: runs every registered class' `test*` methods and reports.
fn run_main() -> i64 {
    // Snapshot: test code may create more subclasses while the runner works,
    // and the registry lock must never be held across user code.
    let classes = registry().clone();
    let mut ran = 0usize;
    let mut failures = 0usize;

    for class in classes {
        let class_name = value::class_display_name(class)
            .unwrap_or("TestCase")
            .to_string();
        let methods = test_method_names(class);
        // One instance per class, as required by the frontend registration
        // contract (user subclasses normally define no `__init__`).
        let instance = builtins::call(class, 0, std::ptr::null());
        if exc::pending() {
            failures += 1;
            eprintln!("FAILED {class_name}.__init__: {}", pending_text());
            exc::clear();
            continue;
        }
        for method in methods {
            let Some(function) = value::class_method(class, &method) else {
                continue;
            };
            let args = [instance];
            ran += 1;
            builtins::call(function, args.len(), args.as_ptr());
            if exc::pending() {
                failures += 1;
                eprintln!("FAILED {class_name}.{method}: {}", pending_text());
                exc::clear();
            }
        }
    }

    println!("Ran {ran} test(s)");
    if failures > 0 {
        return exc::raise_with_message(ASSERTION_ERROR, &format!("{failures} test(s) failed"));
    }
    NONE
}

/// Names of the `test*` methods of `class` and its bases, declaration order,
/// without duplicates.
fn test_method_names(class: i64) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut current = class;
    while let Some(state) = value::as_class(current) {
        for (name, _) in &state.methods {
            if name.starts_with("test") && !names.iter().any(|existing| existing == name) {
                names.push(name.clone());
            }
        }
        current = state.base;
    }
    names
}

/// Renders the pending exception for a `FAILED` line. The text is owned
/// before the caller clears the exception.
fn pending_text() -> String {
    match value::exception_data(exc::pending_value()) {
        Some(data) if data.msg.is_empty() => data.name.to_string(),
        Some(data) => format!("{}: {}", data.name, data.msg),
        None => "exception".to_string(),
    }
}

/// `AssertionError` with `message`; returns the `None` dummy value.
fn raise_assertion(message: String) -> i64 {
    exc::raise_with_message(ASSERTION_ERROR, &message)
}

/// Builds the assertion message: the optional user message (a Python `str`,
/// or its `repr` for other values) followed by the standard text, like
/// CPython's `_formatMessage`.
fn assertion_message(args: &[i64], index: usize, standard: String) -> String {
    match args.get(index).copied() {
        Some(message) if message != NONE => match value::as_str(message) {
            Some(text) => format!("{text}\n{standard}"),
            None => format!("{}\n{standard}", value::repr(message)),
        },
        _ => standard,
    }
}

/// Native methods on `TestCase` and internal helpers, all receiving `self` as
/// `args[0]`.
fn dispatch(function: &str, args: &[i64]) -> i64 {
    match function {
        // Frontend marker: only its presence is meaningful.
        "_case_marker" => NONE,
        "_assert_true" => {
            let Some(&expr) = args.get(1) else {
                return missing_arguments(function);
            };
            if value::truthy(expr) {
                return NONE;
            }
            raise_assertion(assertion_message(
                args,
                2,
                format!("{} is not true", value::repr(expr)),
            ))
        }
        "_assert_false" => {
            let Some(&expr) = args.get(1) else {
                return missing_arguments(function);
            };
            if !value::truthy(expr) {
                return NONE;
            }
            raise_assertion(assertion_message(
                args,
                2,
                format!("{} is not false", value::repr(expr)),
            ))
        }
        "_assert_equal" => {
            let (Some(&first), Some(&second)) = (args.get(1), args.get(2)) else {
                return missing_arguments(function);
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
        "_assert_is" => {
            let (Some(&first), Some(&second)) = (args.get(1), args.get(2)) else {
                return missing_arguments(function);
            };
            // Tagged identity: specials and small ints compare by tag, heap
            // values by object address.
            if first == second {
                return NONE;
            }
            raise_assertion(assertion_message(
                args,
                3,
                format!("{} is not {}", value::repr(first), value::repr(second)),
            ))
        }
        "_assert_in" => {
            let (Some(&member), Some(&container)) = (args.get(1), args.get(2)) else {
                return missing_arguments(function);
            };
            let contained = ops::binop(ops::OP_IN, member, container);
            if exc::pending() {
                // A container that cannot be searched keeps its own error
                // (e.g. `TypeError`) instead of becoming an assertion failure.
                return contained;
            }
            if value::truthy(contained) {
                return NONE;
            }
            raise_assertion(assertion_message(
                args,
                3,
                format!("{} not in {}", value::repr(member), value::repr(container)),
            ))
        }
        "_assert_has_attr" => {
            let (Some(&object), Some(&name_value)) = (args.get(1), args.get(2)) else {
                return missing_arguments(function);
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
                return result;
            }
            if exc::pending_tag() != i64::from(ATTRIBUTE_ERROR) {
                // Propagate the original failure unchanged.
                return result;
            }
            let standard = match value::exception_data(exc::pending_value()) {
                Some(data) if !data.msg.is_empty() => data.msg.clone(),
                _ => format!("attribute {name:?} not found"),
            };
            exc::clear();
            raise_assertion(assertion_message(args, 3, standard))
        }
        "_fail" => {
            let Some(&message) = args.get(1) else {
                return missing_arguments(function);
            };
            let text = match value::as_str(message) {
                Some(text) => text.to_string(),
                None if message == NONE => String::new(),
                None => value::repr(message),
            };
            raise_assertion(text)
        }
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'unittest' has no attribute '{function}'"),
        ),
    }
}

/// Defensive arity error for direct dispatch; the central spec already
/// validates native calls.
fn missing_arguments(function: &str) -> i64 {
    exc::raise_with_message(
        TYPE_ERROR,
        &format!("unittest.{function}() received too few arguments"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::TRUE;

    fn int(n: i64) -> i64 {
        value::encode_int(n).expect("tagged int")
    }

    fn registry_len() -> usize {
        registry().len()
    }

    #[test]
    fn test_case_is_a_native_class_with_the_internal_methods() {
        let case = test_case();
        let class = value::as_class(case).expect("class value");
        assert_eq!(class.name, "TestCase");
        assert_eq!(class.base, NONE);
        let names: Vec<&str> = class
            .methods
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                CASE_MARKER,
                "assertHasAttr",
                "assertIn",
                "assertEqual",
                "assertTrue",
                "assertFalse",
                "assertIs",
                "fail",
            ]
        );
        assert!(value::class_method(case, CASE_MARKER).is_some());
    }

    #[test]
    fn assertion_helpers_pass_and_fail() {
        exc::init();
        assert_eq!(dispatch("_assert_true", &[NONE, TRUE]), NONE);
        assert!(!exc::pending());

        exc::init();
        assert_eq!(dispatch("_assert_false", &[NONE, value::FALSE]), NONE);
        assert!(!exc::pending());

        exc::init();
        assert_eq!(dispatch("_assert_equal", &[NONE, int(5), int(5)]), NONE);
        assert!(!exc::pending());

        exc::init();
        assert_eq!(dispatch("_assert_is", &[NONE, NONE, NONE]), NONE);
        assert!(!exc::pending());

        let list = value::list_new(vec![int(1), int(2), int(3)]);
        exc::init();
        assert_eq!(dispatch("_assert_in", &[NONE, int(2), list]), NONE);
        assert!(!exc::pending());

        // A failing assertion reports AssertionError, optionally prefixed by
        // the user message.
        exc::init();
        assert_eq!(dispatch("_assert_true", &[NONE, value::FALSE]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(ASSERTION_ERROR));
        assert!(pending_text().contains("is not true"));

        exc::init();
        assert_eq!(dispatch("_assert_equal", &[NONE, int(1), int(2)]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(ASSERTION_ERROR));

        exc::init();
        assert_eq!(
            dispatch(
                "_assert_in",
                &[NONE, int(9), list, value::str_alloc("nine".into())]
            ),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(ASSERTION_ERROR));
        assert!(pending_text().contains("nine"));

        exc::init();
        assert_eq!(
            dispatch("_fail", &[NONE, value::str_alloc("boom".into())]),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(ASSERTION_ERROR));
        assert!(pending_text().contains("boom"));

        // `assertHasAttr` converts a missing attribute into an assertion but
        // lets other errors through.
        let module = value::module_value("math".to_string());
        exc::init();
        let pi = dispatch(
            "_assert_has_attr",
            &[NONE, module, value::str_alloc("pi".into())],
        );
        assert!(!exc::pending());
        assert!(value::as_float(pi).is_some());

        exc::init();
        assert_eq!(
            dispatch(
                "_assert_has_attr",
                &[NONE, module, value::str_alloc("nope".into())]
            ),
            NONE
        );
        assert_eq!(exc::pending_tag(), i64::from(ASSERTION_ERROR));
        assert!(pending_text().contains("no attribute"));
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
        // `class_new` registers subclasses of the native TestCase.
        assert!(value::as_class(passing).is_some());
        assert_eq!(registry_len(), 1);

        assert_eq!(call("main", &[]), NONE);
        assert!(!exc::pending());

        let failing = value::class_new(
            "FailingCase".to_string(),
            test_case(),
            vec![(
                "test_bad".to_string(),
                // Called with `[self]`; the spec requires two arguments, so
                // the runner observes a failure and reports it.
                value::native_function_new("unittest", "_fail", 2),
            )],
        );
        assert!(value::as_class(failing).is_some());
        assert_eq!(registry_len(), 2);

        assert_eq!(call("main", &[]), NONE);
        assert_eq!(exc::pending_tag(), i64::from(ASSERTION_ERROR));
        assert!(pending_text().contains("1 test(s) failed"));

        exc::init();
        reset();
        assert_eq!(registry_len(), 0);
    }
}
