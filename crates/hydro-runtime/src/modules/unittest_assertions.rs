//! Additional native `unittest.TestCase` assertion methods.
//!
//! [`crate::modules::unittest`] owns the framework core and the assertions it
//! already implemented. This module adds the CPython 3.14 methods that were
//! missing there: `assertIsSubclass`, the `assertStartsWith`/`assertEndsWith`
//! family, `assertWarns`/`assertWarnsRegex`, `assertLogs`/`assertNoLogs` and
//! the deprecated `fail*` aliases. Everything else returns `None` so the caller
//! can fall back to the existing dispatch (no method is implemented twice).
//!
//! Dispatch contract
//! -----------------
//! * [`call`] / [`call_kw`] accept a bare method name (`assertWarns`), the
//!   `TestCase.`-qualified name used by the native class tables, or the same
//!   name with an `unittest.` module prefix. Unknown names return `None`.
//! * `call_kw` binds named arguments (`msg=`, `places=`, `delta=`,
//!   `expected_regex=`, ...) into their positional slots; `msg` is appended to
//!   the default CPython message as `"{standard} : {msg}"` (honouring
//!   `TestCase.longMessage`, like `_formatMessage`).
//! * The context managers are native classes whose methods dispatch back here:
//!   `_AssertWarnsContext.__enter__`/`__exit__`,
//!   `_AssertLogsContext.__enter__`/`__exit__` and
//!   `_CapturingHandler.handle`. Those five names must be registered in the
//!   module's native spec table together with the `TestCase.*` names handled by
//!   [`call`] (see the final report for the exact list).
//!
//! `assertWarns` uses the native `warnings.catch_warnings(record=True)`
//! manager, so it observes exactly the warnings the runtime records.
//! `assertLogs` drives the Python `logging` package: it installs a native
//! capturing handler on the requested logger, then inspects the records.

use crate::builtins;
use crate::container;
use crate::core::globals;
use crate::exc::{self, ASSERTION_ERROR, TYPE_ERROR};
use crate::heap;
use crate::methods;
use crate::value::{self, FALSE, NONE, TRUE};
use hydro_ir::ParamKind;

/// `longMessage` lookup spelling shared by every failure path.
const LONG_MESSAGE: &[u8] = b"longMessage";

/// Level used by `assertLogs` when `level` is omitted (`logging.INFO`).
const LOGGING_INFO: i64 = 20;

/// `_CapturingHandler` format, identical to CPython's `_AssertLogsContext`.
const LOGGING_FORMAT: &str = "%(levelname)s:%(name)s:%(message)s";

// -------------------------------------------------------------------------
// Entry points
// -------------------------------------------------------------------------

/// Positional dispatch. Returns `None` when this module does not own
/// `function`, so the caller can try the previous implementation.
pub(crate) fn call(function: &str, args: &[i64]) -> Option<i64> {
    dispatch(function, args, &[])
}

/// Keyword-aware dispatch. `keywords` are the call-site names; they are bound
/// to the assertion's declared parameters (including `msg`).
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: Vec<(String, i64)>) -> Option<i64> {
    dispatch(function, args, &keywords)
}

/// Attaches the assertions owned by this module to a native `TestCase` class.
///
/// `modules::unittest` builds the class and its method table; the new methods
/// are installed afterwards with [`value::class_set_attr`], which keeps the
/// class definition in one owner and avoids a second dispatch table. Call this
/// right after `unittest::test_case()` (idempotent: it simply reassigns the
/// same native function values).
pub(crate) fn install(class: i64) {
    // (Python-visible name, native dispatch name, minimum argc incl. `self`).
    static METHODS: [(&str, &str, usize); 17] = [
        ("assertIsSubclass", "TestCase.assertIsSubclass", 3),
        ("assertNotIsSubclass", "TestCase.assertNotIsSubclass", 3),
        ("assertWarns", "TestCase.assertWarns", 2),
        ("assertWarnsRegex", "TestCase.assertWarnsRegex", 3),
        ("assertLogs", "TestCase.assertLogs", 1),
        ("assertNoLogs", "TestCase.assertNoLogs", 1),
        ("assertStartsWith", "TestCase.assertStartsWith", 3),
        ("assertNotStartsWith", "TestCase.assertNotStartsWith", 3),
        ("assertEndsWith", "TestCase.assertEndsWith", 3),
        ("assertNotEndsWith", "TestCase.assertNotEndsWith", 3),
        ("failIf", "TestCase.failIf", 2),
        ("failUnless", "TestCase.failUnless", 2),
        ("failIfEqual", "TestCase.failIfEqual", 3),
        ("failUnlessEqual", "TestCase.failUnlessEqual", 3),
        ("failIfAlmostEqual", "TestCase.failIfAlmostEqual", 3),
        ("failUnlessAlmostEqual", "TestCase.failUnlessAlmostEqual", 3),
        ("failUnlessRaises", "TestCase.failUnlessRaises", 2),
    ];
    for (python_name, native_name, arity) in METHODS {
        let function = value::native_function_new("unittest", native_name, arity);
        value::class_set_attr(class, python_name.to_string(), function);
    }
    // `_assertNotWarns` is CPython's private helper behind `assertNotWarns`.
    // It is not in the static spec table (CPython exposes only the private
    // name), so the class attribute carries its own full signature and entry:
    // the shared method binder binds `msg=` and calls the trampoline directly,
    // exactly like the signed public assertions.
    let function = value::native_function_new("unittest", "TestCase.assertWarns", 2);
    value::class_set_attr(class, "_assertNotWarns".to_string(), function);
    let signature = vec![
        value::ParamMeta {
            name: "self".to_string(),
            kind: ParamKind::Positional,
            default: None,
        },
        value::ParamMeta {
            name: "expected_warning".to_string(),
            kind: ParamKind::Positional,
            default: None,
        },
        value::ParamMeta {
            name: "msg".to_string(),
            kind: ParamKind::Positional,
            default: Some(NONE),
        },
    ];
    value::function_set_signature(function, signature);
    value::function_set_full_entry(function, assert_not_warns_entry as *const () as usize);
}

fn dispatch(function: &str, args: &[i64], keywords: &[(String, i64)]) -> Option<i64> {
    let qualified = function.strip_prefix("unittest.").unwrap_or(function);
    if let Some(method) = qualified.strip_prefix("_AssertWarnsContext.") {
        return Some(warns_context_method(method, args));
    }
    if let Some(method) = qualified.strip_prefix("_AssertLogsContext.") {
        return Some(logs_context_method(method, args));
    }
    if let Some(method) = qualified.strip_prefix("_CapturingHandler.") {
        return Some(handler_method(method, args));
    }
    let method = qualified.strip_prefix("TestCase.").unwrap_or(qualified);
    match method {
        "assertIsSubclass" => Some(assert_is_subclass(args, keywords, true)),
        "assertNotIsSubclass" => Some(assert_is_subclass(args, keywords, false)),
        "assertStartsWith" => Some(assert_tail(args, keywords, Tail::Prefix, true)),
        "assertNotStartsWith" => Some(assert_tail(args, keywords, Tail::Prefix, false)),
        "assertEndsWith" => Some(assert_tail(args, keywords, Tail::Suffix, true)),
        "assertNotEndsWith" => Some(assert_tail(args, keywords, Tail::Suffix, false)),
        "assertWarns" => Some(assert_warns(args, keywords, false)),
        "assertWarnsRegex" => Some(assert_warns(args, keywords, true)),
        "assertLogs" => Some(assert_logs(args, keywords, false)),
        "assertNoLogs" => Some(assert_logs(args, keywords, true)),
        // Deprecated aliases (removed from CPython 3.12+ but still requested by
        // callers of this runtime). Each one delegates to the already
        // implemented assertion so there is a single implementation.
        "failIf" => Some(delegate_alias(
            "assertFalse",
            args,
            keywords,
            BOOL_PARAMS,
            "failIf",
        )),
        "failUnless" => Some(delegate_alias(
            "assertTrue",
            args,
            keywords,
            BOOL_PARAMS,
            "failUnless",
        )),
        "failIfEqual" => Some(delegate_alias(
            "assertNotEqual",
            args,
            keywords,
            EQ_PARAMS,
            "failIfEqual",
        )),
        "failUnlessEqual" => Some(delegate_alias(
            "assertEqual",
            args,
            keywords,
            EQ_PARAMS,
            "failUnlessEqual",
        )),
        "failIfAlmostEqual" => Some(delegate_alias(
            "assertNotAlmostEqual",
            args,
            keywords,
            ALMOST_PARAMS,
            "failIfAlmostEqual",
        )),
        "failUnlessAlmostEqual" => Some(delegate_alias(
            "assertAlmostEqual",
            args,
            keywords,
            ALMOST_PARAMS,
            "failUnlessAlmostEqual",
        )),
        "failUnlessRaises" => Some(crate::modules::unittest::call_kw(
            "TestCase.assertRaises",
            args,
            keywords.to_vec(),
        )),
        _ => None,
    }
}

// -------------------------------------------------------------------------
// Parameter binding
// -------------------------------------------------------------------------

const EQ_PARAMS: &[&[&str]] = &[&["first"], &["second"], &["msg"]];
const BOOL_PARAMS: &[&[&str]] = &[&["expr"], &["msg"]];
const SUBCLASS_PARAMS: &[&[&str]] = &[&["cls"], &["superclass"], &["msg"]];
const ALMOST_PARAMS: &[&[&str]] = &[&["first"], &["second"], &["places"], &["msg"], &["delta"]];
const PREFIX_PARAMS: &[&[&str]] = &[&["s"], &["prefix"], &["msg"]];
const SUFFIX_PARAMS: &[&[&str]] = &[&["s"], &["suffix"], &["msg"]];
const LOGS_PARAMS: &[&[&str]] = &[&["logger"], &["level"]];

/// Binds positional arguments (after `self`) and keywords to `params`.
///
/// `params[i]` lists the accepted keyword spellings for positional slot `i`.
/// Returns `None` after raising `TypeError` when an argument is duplicated or
/// a name is unknown.
fn bind(
    args: &[i64],
    keywords: &[(String, i64)],
    params: &[&[&str]],
    function: &str,
) -> Option<Vec<Option<i64>>> {
    let mut slots = vec![None; params.len()];
    let positional: Vec<i64> = args.iter().skip(1).copied().collect();
    if positional.len() > params.len() {
        exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{}() takes at most {} arguments ({} given)",
                qualified_name(function),
                params.len() + 1,
                positional.len() + 1
            ),
        );
        return None;
    }
    for (index, &item) in positional.iter().enumerate() {
        slots[index] = Some(item);
    }
    for (name, item) in keywords {
        match params
            .iter()
            .position(|accepted| accepted.contains(&name.as_str()))
        {
            Some(index) => {
                if slots[index].is_some() {
                    exc::raise_with_message(
                        TYPE_ERROR,
                        &format!(
                            "{}() got multiple values for argument '{name}'",
                            qualified_name(function)
                        ),
                    );
                    return None;
                }
                slots[index] = Some(*item);
            }
            None => {
                exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "{}() got an unexpected keyword argument '{name}'",
                        qualified_name(function)
                    ),
                );
                return None;
            }
        }
    }
    Some(slots)
}

fn qualified_name(function: &str) -> String {
    if function.starts_with("assert") || function.starts_with("fail") {
        format!("TestCase.{function}")
    } else {
        function.to_string()
    }
}

fn slot(slots: &[Option<i64>], index: usize) -> Option<i64> {
    slots.get(index).copied().flatten()
}

/// `msg` slot normalized so an explicit `None` behaves like an omitted one.
fn message_of(slots: &[Option<i64>], index: usize) -> Option<i64> {
    slot(slots, index).filter(|value| *value != NONE)
}

fn required(slots: &[Option<i64>], index: usize, function: &str, name: &str) -> Option<i64> {
    match slot(slots, index) {
        Some(value) => Some(value),
        None => {
            exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "{}() missing 1 required positional argument: '{name}'",
                    qualified_name(function)
                ),
            );
            None
        }
    }
}

fn missing_method(class: &str, method: &str) -> i64 {
    exc::raise_with_message(
        exc::ATTRIBUTE_ERROR,
        &format!("'{class}' object has no attribute '{method}'"),
    )
}

fn raise_assertion(message: String) -> i64 {
    exc::raise_with_message(ASSERTION_ERROR, &message)
}

// -------------------------------------------------------------------------
// CPython `_formatMessage`
// -------------------------------------------------------------------------

fn text_of(item: i64) -> String {
    match value::as_str(item) {
        Some(text) => text.to_string(),
        None => value::to_string(item),
    }
}

/// Mirrors `TestCase._formatMessage`: with `longMessage` (the default) the
/// explicit message is appended as `"{standard} : {msg}"`; otherwise it
/// replaces the standard text.
fn format_message(test_case: i64, msg: Option<i64>, standard: String) -> String {
    let Some(message) = msg.filter(|value| *value != NONE) else {
        return standard;
    };
    let long = if test_case != NONE {
        let attr = methods::getattr(test_case, LONG_MESSAGE.as_ptr(), LONG_MESSAGE.len());
        if exc::pending() {
            exc::clear();
            true
        } else {
            value::truthy(attr)
        }
    } else {
        true
    };
    if !long {
        return text_of(message);
    }
    format!("{standard} : {}", text_of(message))
}

fn fail_with(test_case: i64, msg: Option<i64>, standard: String) -> i64 {
    raise_assertion(format_message(test_case, msg, standard))
}

fn test_case_of(args: &[i64]) -> i64 {
    args.first().copied().unwrap_or(NONE)
}

// -------------------------------------------------------------------------
// assertIsSubclass / assertNotIsSubclass
// -------------------------------------------------------------------------

fn assert_is_subclass(args: &[i64], keywords: &[(String, i64)], expected: bool) -> i64 {
    let function = if expected {
        "assertIsSubclass"
    } else {
        "assertNotIsSubclass"
    };
    let Some(slots) = bind(args, keywords, SUBCLASS_PARAMS, function) else {
        return NONE;
    };
    let test_case = test_case_of(args);
    let Some(cls) = required(&slots, 0, function, "cls") else {
        return NONE;
    };
    let Some(superclass) = required(&slots, 1, function, "superclass") else {
        return NONE;
    };
    let msg = message_of(&slots, 2);
    let matches = builtins::issubclass(cls, superclass);
    if exc::pending() {
        if exc::pending_tag() == i64::from(TYPE_ERROR) && value::as_class(cls).is_none() {
            exc::clear();
            return fail_with(
                test_case,
                msg,
                format!("{} is not a class", value::repr(cls)),
            );
        }
        return matches;
    }
    let matches = value::truthy(matches);
    if matches == expected {
        return NONE;
    }
    let tuple = value::as_tuple(superclass);
    if expected {
        let standard = if tuple.is_some() {
            format!(
                "{} is not a subclass of any of {}",
                value::repr(cls),
                value::repr(superclass)
            )
        } else {
            format!(
                "{} is not a subclass of {}",
                value::repr(cls),
                value::repr(superclass)
            )
        };
        fail_with(test_case, msg, standard)
    } else {
        let mut target = superclass;
        if let Some(items) = tuple {
            for &item in items {
                let result = builtins::issubclass(cls, item);
                if exc::pending() {
                    return result;
                }
                if value::truthy(result) {
                    target = item;
                    break;
                }
            }
        }
        fail_with(
            test_case,
            msg,
            format!(
                "{} is a subclass of {}",
                value::repr(cls),
                value::repr(target)
            ),
        )
    }
}

// -------------------------------------------------------------------------
// assertStartsWith / assertEndsWith family
// -------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Tail {
    Prefix,
    Suffix,
}

impl Tail {
    fn method(self) -> &'static str {
        match self {
            Tail::Prefix => "startswith",
            Tail::Suffix => "endswith",
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Tail::Prefix => "start",
            Tail::Suffix => "end",
        }
    }
}

fn assert_tail(args: &[i64], keywords: &[(String, i64)], kind: Tail, expected: bool) -> i64 {
    let function = match (kind, expected) {
        (Tail::Prefix, true) => "assertStartsWith",
        (Tail::Prefix, false) => "assertNotStartsWith",
        (Tail::Suffix, true) => "assertEndsWith",
        (Tail::Suffix, false) => "assertNotEndsWith",
    };
    let params = match kind {
        Tail::Prefix => PREFIX_PARAMS,
        Tail::Suffix => SUFFIX_PARAMS,
    };
    let Some(slots) = bind(args, keywords, params, function) else {
        return NONE;
    };
    let test_case = test_case_of(args);
    let Some(text) = required(&slots, 0, function, "s") else {
        return NONE;
    };
    let Some(tail) = required(
        &slots,
        1,
        function,
        if matches!(kind, Tail::Prefix) {
            "prefix"
        } else {
            "suffix"
        },
    ) else {
        return NONE;
    };
    let msg = message_of(&slots, 2);
    let found = methods::call(
        text,
        kind.method().as_ptr(),
        kind.method().len(),
        1,
        [tail].as_ptr(),
    );
    if exc::pending() {
        let original = exc::pending_value();
        let tag = exc::pending_tag();
        exc::clear();
        if tag == i64::from(exc::ATTRIBUTE_ERROR) || tag == i64::from(TYPE_ERROR) {
            tail_type_check(test_case, text, tail, msg);
            if exc::pending() {
                return NONE;
            }
        }
        return exc::raise_value(original);
    }
    let matches = value::truthy(found);
    if matches == expected {
        return NONE;
    }
    if expected {
        let standard = if value::as_tuple(tail).is_some() {
            format!(
                "{} doesn't {} with any of {}",
                value::repr(text),
                kind.verb(),
                value::repr(tail)
            )
        } else {
            format!(
                "{} doesn't {} with {}",
                value::repr(text),
                kind.verb(),
                value::repr(tail)
            )
        };
        fail_with(test_case, msg, standard)
    } else {
        let mut reported = tail;
        if let Some(items) = value::as_tuple(tail) {
            for &item in items {
                let result = methods::call(
                    text,
                    kind.method().as_ptr(),
                    kind.method().len(),
                    1,
                    [item].as_ptr(),
                );
                if exc::pending() {
                    return result;
                }
                if value::truthy(result) {
                    reported = item;
                    break;
                }
            }
        }
        fail_with(
            test_case,
            msg,
            format!(
                "{} {}s with {}",
                value::repr(text),
                kind.verb(),
                value::repr(reported)
            ),
        )
    }
}

/// CPython `_tail_type_check`: when `startswith`/`endswith` rejected the
/// arguments, report the str/bytes mismatch as an assertion failure.
fn tail_type_check(test_case: i64, text: i64, tails: i64, msg: Option<i64>) {
    let items: Vec<i64> = match value::as_tuple(tails) {
        Some(items) => items.to_vec(),
        None => vec![tails],
    };
    for item in items {
        if value::as_str(item).is_some() {
            if value::as_str(text).is_none() {
                fail_with(
                    test_case,
                    msg,
                    format!("Expected str, not {}", value::type_name(text)),
                );
                return;
            }
        } else if value::as_bytes(item).is_some() && value::as_bytes(text).is_none() {
            fail_with(
                test_case,
                msg,
                format!("Expected bytes, not {}", value::type_name(text)),
            );
            return;
        }
    }
}

// -------------------------------------------------------------------------
// Deprecated aliases
// -------------------------------------------------------------------------

fn delegate_alias(
    target: &str,
    args: &[i64],
    keywords: &[(String, i64)],
    params: &[&[&str]],
    function: &str,
) -> i64 {
    let Some(slots) = bind(args, keywords, params, function) else {
        return NONE;
    };
    let mut positional = Vec::with_capacity(slots.len() + 1);
    positional.push(test_case_of(args));
    for slot in &slots {
        positional.push(slot.unwrap_or(NONE));
    }
    crate::modules::unittest::call(&format!("TestCase.{target}"), &positional)
}

// -------------------------------------------------------------------------
// Native helper classes
// -------------------------------------------------------------------------

fn cached_class(key: &str, name: &str, methods: &[(&'static str, &'static str, usize)]) -> i64 {
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
    let class = value::class_new(name.to_string(), NONE, pairs);
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(key, class);
    heap::release_roots(class_roots);
    heap::release_roots(function_roots);
    class
}

fn warns_context_class() -> i64 {
    static METHODS: [(&str, &str, usize); 2] = [
        ("__enter__", "_AssertWarnsContext.__enter__", 1),
        ("__exit__", "_AssertWarnsContext.__exit__", 4),
    ];
    cached_class(
        "unittest._AssertWarnsContext.class",
        "_AssertWarnsContext",
        &METHODS,
    )
}

fn logs_context_class() -> i64 {
    static METHODS: [(&str, &str, usize); 2] = [
        ("__enter__", "_AssertLogsContext.__enter__", 1),
        ("__exit__", "_AssertLogsContext.__exit__", 4),
    ];
    cached_class(
        "unittest._AssertLogsContext.class",
        "_AssertLogsContext",
        &METHODS,
    )
}

fn logging_watcher_class() -> i64 {
    static METHODS: [(&str, &str, usize); 0] = [];
    cached_class(
        "unittest._LoggingWatcher.class",
        "_LoggingWatcher",
        &METHODS,
    )
}

fn capturing_handler_class() -> i64 {
    static METHODS: [(&str, &str, usize); 1] = [("handle", "_CapturingHandler.handle", 2)];
    cached_class(
        "unittest._CapturingHandler.class",
        "_CapturingHandler",
        &METHODS,
    )
}

// -------------------------------------------------------------------------
// assertWarns / assertWarnsRegex
// -------------------------------------------------------------------------

fn warning_class() -> i64 {
    exc::exception_class(exc::WARNING)
}

fn is_warning_type(candidate: i64) -> bool {
    let matches = builtins::issubclass(candidate, warning_class());
    if exc::pending() {
        exc::clear();
        return false;
    }
    value::truthy(matches)
}

fn valid_warning_argument(expected: i64) -> bool {
    if let Some(items) = value::as_tuple(expected) {
        !items.is_empty() && items.iter().all(|&item| is_warning_type(item))
    } else {
        is_warning_type(expected)
    }
}

fn expected_name(expected: i64) -> String {
    match value::as_class(expected) {
        Some(class) => class.name.to_string(),
        None => value::to_string(expected),
    }
}

fn new_warns_context(test_case: i64, expected: i64, regex: Option<i64>, msg: Option<i64>) -> i64 {
    let context = value::instance_new(warns_context_class());
    let roots = heap::root_values(&[context, test_case, expected]);
    value::instance_setattr(context, "test_case".to_string(), test_case);
    value::instance_setattr(context, "expected".to_string(), expected);
    value::instance_setattr(context, "expected_regex".to_string(), regex.unwrap_or(NONE));
    value::instance_setattr(context, "msg".to_string(), msg.unwrap_or(NONE));
    value::instance_setattr(context, "obj_name".to_string(), NONE);
    value::instance_setattr(context, "not_warns".to_string(), FALSE);
    heap::release_roots(roots);
    context
}

/// Full-argv entry for `_assertNotWarns(self, expected_warning, msg=None)`.
extern "C" fn assert_not_warns_entry(arguments: *const i64) -> i64 {
    // SAFETY: the binder supplies one slot per declared parameter (three).
    let slots = unsafe { std::slice::from_raw_parts(arguments, 3) };
    assert_not_warns(slots)
}

/// `TestCase._assertNotWarns(expected_warning, msg=None)`: the context form of
/// CPython's private helper. `warns_exit_impl` inverts the check when the
/// context's `not_warns` flag is set.
fn assert_not_warns(args: &[i64]) -> i64 {
    let test_case = args.first().copied().unwrap_or(NONE);
    let expected = args.get(1).copied().unwrap_or(NONE);
    let msg = args.get(2).copied().filter(|value| *value != NONE);
    if !valid_warning_argument(expected) {
        exc::raise_with_message(
            TYPE_ERROR,
            "assertWarns() arg 1 must be a warning type or tuple of warning types",
        );
        return NONE;
    }
    let context = new_warns_context(test_case, expected, None, msg);
    if context != NONE {
        value::instance_setattr(context, "not_warns".to_string(), TRUE);
    }
    context
}

fn assert_warns(args: &[i64], keywords: &[(String, i64)], with_regex: bool) -> i64 {
    let function = if with_regex {
        "assertWarnsRegex"
    } else {
        "assertWarns"
    };
    let test_case = test_case_of(args);
    let mut expected = None;
    let mut regex = None;
    let mut forwarded: Vec<(String, i64)> = Vec::new();
    for (name, item) in keywords {
        match name.as_str() {
            "expected_warning" | "expected" if expected.is_none() => expected = Some(*item),
            "expected_regex" | "regex" if with_regex && regex.is_none() => regex = Some(*item),
            _ => forwarded.push((name.clone(), *item)),
        }
    }
    let mut positionals: Vec<i64> = args.iter().skip(1).copied().collect();
    if expected.is_none() {
        if positionals.is_empty() {
            exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "{}() missing 1 required positional argument: 'expected_warning'",
                    qualified_name(function)
                ),
            );
            return NONE;
        }
        expected = Some(positionals.remove(0));
    }
    if with_regex && regex.is_none() {
        if positionals.is_empty() {
            exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "{}() missing 1 required positional argument: 'expected_regex'",
                    qualified_name(function)
                ),
            );
            return NONE;
        }
        regex = Some(positionals.remove(0));
    }
    let expected = expected.unwrap_or(NONE);
    if !valid_warning_argument(expected) {
        exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{}() arg 1 must be a warning type or tuple of warning types",
                qualified_name(function)
            ),
        );
        return NONE;
    }
    if positionals.is_empty() {
        let mut msg = None;
        for (name, item) in &forwarded {
            if name == "msg" {
                msg = Some(*item);
            } else {
                exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "{}() got an unexpected keyword argument '{name}'",
                        qualified_name(function)
                    ),
                );
                return NONE;
            }
        }
        return new_warns_context(test_case, expected, regex, msg);
    }
    let callable = positionals.remove(0);
    let context = new_warns_context(test_case, expected, regex, None);
    let obj_name = value::function_name(callable);
    let roots = heap::root_values(&[context, callable]);
    if !obj_name.is_empty() {
        value::instance_setattr(context, "obj_name".to_string(), value::str_alloc(obj_name));
    } else {
        value::instance_setattr(
            context,
            "obj_name".to_string(),
            value::str_alloc(value::to_string(callable)),
        );
    }
    let entered = warns_enter_impl(context);
    if exc::pending() {
        heap::release_roots(roots);
        return entered;
    }
    let _ = call_with_keywords(callable, &positionals, &forwarded);
    let thrown = if exc::pending() {
        Some(take_pending())
    } else {
        None
    };
    let (exc_type, exc_value) = match thrown {
        Some(error) => {
            let class = value::exception_data(error)
                .map(|data| exc::exception_class(data.tag))
                .unwrap_or(NONE);
            (class, error)
        }
        None => (NONE, NONE),
    };
    let exit_args = [context, exc_type, exc_value, NONE];
    let exit_result = warns_exit_impl(&exit_args);
    heap::release_roots(roots);
    if exc::pending() {
        return exit_result;
    }
    match thrown {
        Some(error) if !value::truthy(exit_result) => exc::raise_value(error),
        _ => NONE,
    }
}

fn warns_context_method(method: &str, args: &[i64]) -> i64 {
    match method {
        "__enter__" => match args.first().copied() {
            Some(instance) => warns_enter_impl(instance),
            None => exc::raise_with_message(TYPE_ERROR, "__enter__() missing self"),
        },
        "__exit__" => warns_exit_impl(args),
        _ => missing_method("_AssertWarnsContext", method),
    }
}

fn warns_enter_impl(instance: i64) -> i64 {
    let manager = crate::modules::warnings::call("catch_warnings", &[TRUE]);
    if exc::pending() {
        return manager;
    }
    let recorded = crate::modules::warnings::call("_catch_warnings_enter", &[manager]);
    if exc::pending() {
        return recorded;
    }
    let roots = heap::root_values(&[instance, manager, recorded]);
    value::instance_setattr(instance, "_manager".to_string(), manager);
    value::instance_setattr(instance, "_warnings".to_string(), recorded);
    heap::release_roots(roots);
    instance
}

fn warns_exit_impl(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "__exit__() missing self");
    };
    let exc_type = args.get(1).copied().unwrap_or(NONE);
    let exc_value = args.get(2).copied().unwrap_or(NONE);
    let traceback = args.get(3).copied().unwrap_or(NONE);
    let manager = value::instance_attr(instance, "_manager").unwrap_or(NONE);
    let recorded = value::instance_attr(instance, "_warnings").unwrap_or(NONE);
    if manager != NONE {
        let result = crate::modules::warnings::call(
            "_catch_warnings_exit",
            &[manager, exc_type, exc_value, traceback],
        );
        if exc::pending() {
            return result;
        }
    }
    if exc_type != NONE {
        return FALSE;
    }
    if value::instance_attr(instance, "not_warns").is_some_and(value::truthy) {
        let expected = value::instance_attr(instance, "expected").unwrap_or(NONE);
        let test_case = value::instance_attr(instance, "test_case").unwrap_or(NONE);
        let msg = value::instance_attr(instance, "msg").filter(|message| *message != NONE);
        let roots = heap::root_values(&[instance, expected, recorded]);
        for &item in value::as_list(recorded).unwrap_or(&[]) {
            let message = value::instance_attr(item, "message").unwrap_or(NONE);
            let matches = builtins::isinstance(message, expected);
            if exc::pending() {
                heap::release_roots(roots);
                return matches;
            }
            if value::truthy(matches) {
                let standard = format!("{} triggered", expected_name(expected));
                fail_with(test_case, msg, standard);
                heap::release_roots(roots);
                return FALSE;
            }
        }
        heap::release_roots(roots);
        return NONE;
    }
    let expected = value::instance_attr(instance, "expected").unwrap_or(NONE);
    let regex = value::instance_attr(instance, "expected_regex").unwrap_or(NONE);
    // `_catch_warnings_exit` drops the recorded list's persistent root, so the
    // list (and the classes it is compared against) is pinned for the scan.
    let roots = heap::root_values(&[instance, expected, regex, recorded]);
    let result = warns_check(instance, expected, regex, recorded);
    heap::release_roots(roots);
    result
}

fn warns_check(instance: i64, expected: i64, regex: i64, recorded: i64) -> i64 {
    let items = value::as_list(recorded).unwrap_or(&[]);
    let mut first_matching = None;
    for &item in items {
        let message = value::instance_attr(item, "message").unwrap_or(NONE);
        let matches = builtins::isinstance(message, expected);
        if exc::pending() {
            return matches;
        }
        if !value::truthy(matches) {
            continue;
        }
        if first_matching.is_none() {
            first_matching = Some(message);
        }
        if regex != NONE {
            let search = crate::modules::re::call(
                "search",
                &[regex, value::str_alloc(value::to_string(message))],
            );
            if exc::pending() {
                return search;
            }
            if !value::truthy(search) {
                continue;
            }
        }
        value::instance_setattr(instance, "warning".to_string(), message);
        value::instance_setattr(
            instance,
            "filename".to_string(),
            value::instance_attr(item, "filename").unwrap_or(NONE),
        );
        value::instance_setattr(
            instance,
            "lineno".to_string(),
            value::instance_attr(item, "lineno").unwrap_or(NONE),
        );
        return TRUE;
    }
    let test_case = value::instance_attr(instance, "test_case").unwrap_or(NONE);
    let msg = value::instance_attr(instance, "msg").unwrap_or(NONE);
    let msg = if msg == NONE { None } else { Some(msg) };
    let standard = match first_matching {
        Some(first) => {
            let pattern = pattern_text(regex);
            format!(
                "\"{pattern}\" does not match \"{}\"",
                value::to_string(first)
            )
        }
        None => {
            let exc_name = expected_name(expected);
            let obj_name = value::instance_attr(instance, "obj_name").unwrap_or(NONE);
            if obj_name != NONE {
                format!("{exc_name} not triggered by {}", text_of(obj_name))
            } else {
                format!("{exc_name} not triggered")
            }
        }
    };
    fail_with(test_case, msg, standard);
    FALSE
}

fn pattern_text(regex: i64) -> String {
    if regex == NONE {
        return String::new();
    }
    let pattern = methods::getattr(regex, b"pattern".as_ptr(), b"pattern".len());
    if exc::pending() {
        exc::clear();
        return value::repr(regex);
    }
    text_of(pattern)
}

// -------------------------------------------------------------------------
// assertLogs / assertNoLogs
// -------------------------------------------------------------------------

fn assert_logs(args: &[i64], keywords: &[(String, i64)], no_logs: bool) -> i64 {
    let function = if no_logs {
        "assertNoLogs"
    } else {
        "assertLogs"
    };
    let Some(slots) = bind(args, keywords, LOGS_PARAMS, function) else {
        return NONE;
    };
    let test_case = test_case_of(args);
    let logger_name = slot(&slots, 0).unwrap_or(NONE);
    let level = slot(&slots, 1).unwrap_or(NONE);
    let context = value::instance_new(logs_context_class());
    let roots = heap::root_values(&[context, test_case, logger_name, level]);
    value::instance_setattr(context, "test_case".to_string(), test_case);
    value::instance_setattr(context, "logger_name".to_string(), logger_name);
    value::instance_setattr(context, "level".to_string(), level);
    value::instance_setattr(
        context,
        "no_logs".to_string(),
        if no_logs { TRUE } else { FALSE },
    );
    value::instance_setattr(context, "msg".to_string(), NONE);
    heap::release_roots(roots);
    context
}

fn logs_context_method(method: &str, args: &[i64]) -> i64 {
    match method {
        "__enter__" => match args.first().copied() {
            Some(instance) => logs_enter_impl(instance),
            None => exc::raise_with_message(TYPE_ERROR, "__enter__() missing self"),
        },
        "__exit__" => logs_exit_impl(args),
        _ => missing_method("_AssertLogsContext", method),
    }
}

fn logging_module() -> i64 {
    crate::modules::importlib::call("import_module", &[value::str_alloc("logging".to_string())])
}

fn attr(object: i64, name: &str) -> i64 {
    methods::getattr(object, name.as_ptr(), name.len())
}

fn set_attr(object: i64, name: &str, new_value: i64) -> i64 {
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

fn list_copy(items: i64) -> i64 {
    let values: Vec<i64> = value::as_list(items)
        .map(|span| span.to_vec())
        .unwrap_or_default();
    value::list_new(values)
}

fn resolve_level(module: i64, raw: i64) -> i64 {
    if raw == NONE {
        return value::encode_int(LOGGING_INFO).unwrap_or(NONE);
    }
    let Some(name) = value::as_str(raw) else {
        return raw;
    };
    let table = attr(module, "_nameToLevel");
    if exc::pending() {
        exc::clear();
        return raw;
    }
    if let Some(pairs) = value::as_dict(table) {
        for (key, item) in pairs {
            if value::as_str(*key) == Some(name) {
                return *item;
            }
        }
    }
    raw
}

fn level_name(level: i64) -> String {
    match level {
        50 => "CRITICAL".to_string(),
        40 => "ERROR".to_string(),
        30 => "WARNING".to_string(),
        20 => "INFO".to_string(),
        10 => "DEBUG".to_string(),
        0 => "NOTSET".to_string(),
        other => format!("Level {other}"),
    }
}

fn logs_enter_impl(instance: i64) -> i64 {
    let module = logging_module();
    if exc::pending() {
        return module;
    }
    let roots = heap::root_values(&[instance, module]);
    let result = logs_enter_inner(instance, module);
    heap::release_roots(roots);
    result
}

fn logs_enter_inner(instance: i64, module: i64) -> i64 {
    let logger_name = value::instance_attr(instance, "logger_name").unwrap_or(NONE);
    let logger_class = attr(module, "Logger");
    if exc::pending() {
        return logger_class;
    }
    let is_logger = builtins::isinstance(logger_name, logger_class);
    if exc::pending() {
        return is_logger;
    }
    let logger = if value::truthy(is_logger) {
        logger_name
    } else {
        let get_logger = attr(module, "getLogger");
        if exc::pending() {
            return get_logger;
        }
        builtins::call(get_logger, 1, [logger_name].as_ptr())
    };
    if exc::pending() {
        return logger;
    }
    let logger_roots = heap::root_values(&[logger]);
    let result = logs_enter_configure(instance, module, logger);
    heap::release_roots(logger_roots);
    result
}

fn logs_enter_configure(instance: i64, module: i64, logger: i64) -> i64 {
    let formatter_class = attr(module, "Formatter");
    if exc::pending() {
        return formatter_class;
    }
    let format_text = value::str_alloc(LOGGING_FORMAT.to_string());
    let format_roots = heap::root_values(&[format_text]);
    let formatter = builtins::call(formatter_class, 1, [format_text].as_ptr());
    heap::release_roots(format_roots);
    if exc::pending() {
        return formatter;
    }
    // Everything the context keeps is stored on `instance` (which the caller
    // roots) as soon as it is created, so the collector always sees it.
    value::instance_setattr(instance, "_formatter".to_string(), formatter);
    let raw_level = value::instance_attr(instance, "level").unwrap_or(NONE);
    let level = resolve_level(module, raw_level);
    value::instance_setattr(instance, "_effective_level".to_string(), level);
    let watcher = value::instance_new(logging_watcher_class());
    value::instance_setattr(instance, "watcher".to_string(), watcher);
    value::instance_setattr(watcher, "records".to_string(), value::list_new(Vec::new()));
    value::instance_setattr(watcher, "output".to_string(), value::list_new(Vec::new()));
    let handler = value::instance_new(capturing_handler_class());
    value::instance_setattr(instance, "_handler".to_string(), handler);
    value::instance_setattr(handler, "watcher".to_string(), watcher);
    value::instance_setattr(handler, "level".to_string(), level);
    value::instance_setattr(handler, "formatter".to_string(), formatter);
    let handlers = attr(logger, "handlers");
    if exc::pending() {
        return handlers;
    }
    let old_level = attr(logger, "level");
    if exc::pending() {
        return old_level;
    }
    let old_propagate = attr(logger, "propagate");
    if exc::pending() {
        return old_propagate;
    }
    let old_handlers = list_copy(handlers);
    value::instance_setattr(instance, "logger".to_string(), logger);
    value::instance_setattr(instance, "old_handlers".to_string(), old_handlers);
    value::instance_setattr(instance, "old_level".to_string(), old_level);
    value::instance_setattr(instance, "old_propagate".to_string(), old_propagate);
    let root_handlers = value::list_new(vec![handler]);
    let install_roots = heap::root_values(&[root_handlers]);
    let install = set_attr(logger, "handlers", root_handlers);
    heap::release_roots(install_roots);
    if exc::pending() {
        return install;
    }
    let set_level = invoke(logger, "setLevel", &[level]);
    if exc::pending() {
        return set_level;
    }
    let propagate = set_attr(logger, "propagate", FALSE);
    if exc::pending() {
        return propagate;
    }
    let no_logs = value::instance_attr(instance, "no_logs").unwrap_or(FALSE);
    if no_logs == TRUE {
        NONE
    } else {
        watcher
    }
}

fn logs_exit_impl(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "__exit__() missing self");
    };
    let exc_type = args.get(1).copied().unwrap_or(NONE);
    let logger = value::instance_attr(instance, "logger").unwrap_or(NONE);
    if logger == NONE {
        return FALSE;
    }
    let old_handlers = value::instance_attr(instance, "old_handlers").unwrap_or(NONE);
    let old_level = value::instance_attr(instance, "old_level").unwrap_or(NONE);
    let old_propagate = value::instance_attr(instance, "old_propagate").unwrap_or(NONE);
    let restore = set_attr(logger, "handlers", old_handlers);
    if exc::pending() {
        return restore;
    }
    let restore_level = invoke(logger, "setLevel", &[old_level]);
    if exc::pending() {
        return restore_level;
    }
    let restore_propagate = set_attr(logger, "propagate", old_propagate);
    if exc::pending() {
        return restore_propagate;
    }
    if exc_type != NONE {
        return FALSE;
    }
    let watcher = value::instance_attr(instance, "watcher").unwrap_or(NONE);
    let records = value::instance_attr(watcher, "records").unwrap_or(NONE);
    let output = value::instance_attr(watcher, "output").unwrap_or(NONE);
    let no_logs = value::instance_attr(instance, "no_logs").unwrap_or(FALSE) == TRUE;
    if no_logs {
        if value::as_list(records).is_some_and(|items| !items.is_empty()) {
            raise_assertion(format!("Unexpected logs found: {}", value::repr(output)));
        }
        return NONE;
    }
    if value::as_list(records).is_none_or(|items| items.is_empty()) {
        let level = value::instance_attr(instance, "_effective_level")
            .or_else(|| value::instance_attr(instance, "level"))
            .unwrap_or(NONE);
        let level = value::as_integer(level).unwrap_or(LOGGING_INFO);
        let name = attr(logger, "name");
        let name = if exc::pending() {
            exc::clear();
            String::new()
        } else {
            text_of(name)
        };
        raise_assertion(format!(
            "no logs of level {} or higher triggered on {name}",
            level_name(level)
        ));
    }
    NONE
}

// -------------------------------------------------------------------------
// _CapturingHandler
// -------------------------------------------------------------------------

fn handler_method(method: &str, args: &[i64]) -> i64 {
    match method {
        "handle" => handler_handle(args),
        _ => missing_method("_CapturingHandler", method),
    }
}

fn handler_handle(args: &[i64]) -> i64 {
    let (Some(&instance), Some(&record)) = (args.first(), args.get(1)) else {
        return exc::raise_with_message(TYPE_ERROR, "handle() missing record");
    };
    let level = value::instance_attr(instance, "level")
        .and_then(value::as_integer)
        .unwrap_or(0);
    let record_level = attr(record, "levelno");
    if exc::pending() {
        return record_level;
    }
    let record_level = value::as_integer(record_level).unwrap_or(0);
    if record_level < level {
        return FALSE;
    }
    let watcher = value::instance_attr(instance, "watcher").unwrap_or(NONE);
    if watcher == NONE {
        return FALSE;
    }
    let formatter = value::instance_attr(instance, "formatter").unwrap_or(NONE);
    let formatted = if formatter != NONE {
        methods::call(
            formatter,
            "format".as_ptr(),
            "format".len(),
            1,
            [record].as_ptr(),
        )
    } else {
        NONE
    };
    if exc::pending() {
        return formatted;
    }
    let formatted = if formatted == NONE {
        invoke(record, "getMessage", &[])
    } else {
        formatted
    };
    if exc::pending() {
        return formatted;
    }
    let records = value::instance_attr(watcher, "records").unwrap_or(NONE);
    let output = value::instance_attr(watcher, "output").unwrap_or(NONE);
    let roots = heap::root_values(&[record, formatted]);
    container::list_append(records, record);
    container::list_append(output, formatted);
    heap::release_roots(roots);
    TRUE
}

// -------------------------------------------------------------------------
// Shared call helpers
// -------------------------------------------------------------------------

fn take_pending() -> i64 {
    let item = exc::pending_value();
    exc::clear();
    item
}

fn call_with_keywords(callable: i64, positional: &[i64], keywords: &[(String, i64)]) -> i64 {
    let argv = if positional.is_empty() {
        std::ptr::null()
    } else {
        positional.as_ptr()
    };
    if keywords.is_empty() {
        return builtins::call(callable, positional.len(), argv);
    }
    let mut words = Vec::with_capacity(keywords.len() * 2);
    for (name, item) in keywords {
        words.push(value::str_alloc(name.clone()));
        words.push(*item);
    }
    let roots = heap::root_values(&words);
    let result = builtins::call_kw(
        callable,
        positional.len(),
        argv,
        keywords.len(),
        words.as_ptr(),
    );
    heap::release_roots(roots);
    result
}
