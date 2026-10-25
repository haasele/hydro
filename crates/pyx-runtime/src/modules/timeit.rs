//! `timeit`: micro-benchmark timing for callables.
//!
//! Only callable statements are supported. CPython compiles string statements
//! into a generated function; PyX has no runtime compiler, so a `str`/`bytes`
//! `stmt` or `setup` raises `RuntimeError` explaining that source strings are
//! unsupported. Callable statements are invoked once per loop iteration with
//! no arguments.
//!
//! `Timer` is a native class (created by [`timer_class`]) with `stmt` and
//! `timer` attributes and the usual `timeit`/`repeat`/`autorange` methods.
//! The default timer is backed by [`std::time::Instant`], so results are
//! monotonic durations in seconds. `default_timer()` reports seconds since
//! the process's first timing call.

use std::sync::LazyLock;
use std::time::Instant;

use crate::builtins;
use crate::exc::{self, ATTRIBUTE_ERROR, RUNTIME_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value::{self, NONE};

/// CPython's default loop count for `timeit()`.
const DEFAULT_NUMBER: i64 = 1_000_000;

/// CPython's default number of repetitions for `repeat()`.
const DEFAULT_REPEAT: i64 = 5;

/// `autorange` keeps doubling until a run takes at least this long.
const AUTORANGE_TARGET: f64 = 0.2;

/// Monotonic origin of `default_timer()`, matching `time.perf_counter`'s
/// absolute-float interface without depending on wall-clock time.
static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);

fn now_seconds() -> f64 {
    ORIGIN.elapsed().as_secs_f64()
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

/// Dispatches a native `timeit` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "default_timer" => value::float_new(now_seconds()),
        "Timer" => timer_instance(args),
        "_timer_timeit" => timer_timeit(args),
        "_timer_repeat" => timer_repeat(args),
        "_timer_autorange" => timer_autorange(args),
        "timeit" => module_timeit(args),
        "repeat" => module_repeat(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'timeit' has no attribute '{function}'"),
        ),
    }
}

/// Builds the native `Timer` class. A fresh class per construction keeps the
/// method table independent of how often the constructor is materialised.
pub(crate) fn timer_class() -> i64 {
    const METHODS: [(&str, &str, usize); 3] = [
        ("timeit", "_timer_timeit", 2),
        ("repeat", "_timer_repeat", 3),
        ("autorange", "_timer_autorange", 2),
    ];
    let mut functions = [NONE; METHODS.len()];
    for (slot, (_, native, arity)) in functions.iter_mut().zip(METHODS) {
        *slot = value::native_function_new("timeit", native, arity);
    }
    let methods = METHODS
        .iter()
        .zip(functions)
        .map(|((python_name, _, _), function)| ((*python_name).to_string(), function))
        .collect();
    value::class_new("Timer".to_string(), NONE, methods)
}

fn default_timer_function() -> i64 {
    value::native_function_new("timeit", "default_timer", 0)
}

/// Parameter names (excluding `self`) for keyword calls on `Timer` methods.
/// Consumed by the runtime's instance keyword binder.
const TIMER_TIMEIT_KEYWORDS: &[&str] = &["number"];
const TIMER_REPEAT_KEYWORDS: &[&str] = &["repeat", "number"];
const TIMER_AUTORANGE_KEYWORDS: &[&str] = &["callback"];

pub(crate) fn method_keyword_params(internal: &str) -> Option<&'static [&'static str]> {
    match internal {
        "_timer_timeit" => Some(TIMER_TIMEIT_KEYWORDS),
        "_timer_repeat" => Some(TIMER_REPEAT_KEYWORDS),
        "_timer_autorange" => Some(TIMER_AUTORANGE_KEYWORDS),
        _ => None,
    }
}

/// Rejects source strings with CPython-style guidance: PyX cannot compile
/// them at run time.
fn reject_source(candidate: i64, what: &str) -> Result<(), i64> {
    if value::as_str(candidate).is_some() || value::as_bytes(candidate).is_some() {
        return Err(exc::raise_with_message(
            RUNTIME_ERROR,
            &format!(
                "timeit {what} source strings are not supported (no runtime compiler); pass a callable"
            ),
        ));
    }
    Ok(())
}

/// The default timer value stored on a `Timer` without an explicit timer.
fn is_default_timer(candidate: i64) -> bool {
    matches!(
        value::as_function(candidate).map(|function| &function.body),
        Some(value::FunctionBody::Native { module, name })
            if *module == "timeit" && *name == "default_timer"
    )
}

/// Whether `builtins::call` can invoke the value (function, extension
/// function or class constructor).
fn is_callable(candidate: i64) -> bool {
    value::as_function(candidate).is_some()
        || value::as_ext_function(candidate).is_some()
        || value::as_class(candidate).is_some()
}

/// Builds a `Timer` instance from an explicit constructor argument list.
fn timer_from(stmt: Option<i64>, setup: Option<i64>, timer: Option<i64>) -> Result<i64, i64> {
    let Some(stmt) = stmt else {
        return Err(type_error("Timer() missing required argument: 'stmt'"));
    };
    if stmt == NONE {
        return Err(type_error("Timer() argument 1 must be a callable"));
    }
    reject_source(stmt, "statement")?;
    if !is_callable(stmt) {
        return Err(type_error("Timer() statement must be a callable"));
    }
    let setup = setup.unwrap_or(NONE);
    let timer = timer.unwrap_or(NONE);
    if setup != NONE {
        reject_source(setup, "setup")?;
        if !is_callable(setup) {
            return Err(type_error("Timer() setup must be a callable"));
        }
    }
    let class = timer_class();
    let instance = value::instance_new(class);
    value::instance_setattr(instance, "stmt".to_string(), stmt);
    value::instance_setattr(
        instance,
        "timer".to_string(),
        if timer == NONE {
            default_timer_function()
        } else {
            timer
        },
    );
    if setup != NONE {
        value::instance_setattr(instance, "setup".to_string(), setup);
    }
    Ok(instance)
}

/// `Timer(stmt, setup=None, timer=None, globals=None)`.
fn timer_instance(args: &[i64]) -> i64 {
    match timer_from(
        args.first().copied(),
        args.get(1).copied(),
        args.get(2).copied(),
    ) {
        Ok(instance) => instance,
        Err(error) => error,
    }
}

/// Reads an optional loop count; `None` and `None`-valued slots use the
/// CPython default.
fn loop_count(args: &[i64], index: usize, function: &str) -> Result<u64, i64> {
    let Some(slot) = args.get(index).copied() else {
        return Ok(DEFAULT_NUMBER as u64);
    };
    if slot == NONE {
        return Ok(DEFAULT_NUMBER as u64);
    }
    match value::as_integer(slot) {
        Some(number) if number >= 0 => Ok(number as u64),
        Some(_) => Err(exc::raise_with_message(VALUE_ERROR, "number must be >= 0")),
        None => Err(type_error(&format!("{function}() number must be int"))),
    }
}

/// Reads an optional repeat count (negative counts behave like zero).
fn repeat_count(args: &[i64], index: usize) -> Result<usize, i64> {
    let Some(slot) = args.get(index).copied() else {
        return Ok(DEFAULT_REPEAT as usize);
    };
    if slot == NONE {
        return Ok(DEFAULT_REPEAT as usize);
    }
    match value::as_integer(slot) {
        Some(number) if number >= 0 => Ok(number as usize),
        Some(_) => Err(exc::raise_with_message(VALUE_ERROR, "repeat must be >= 0")),
        None => Err(type_error("repeat() argument 2 must be int")),
    }
}

/// Current timer value. The default timer and `None` take the `Instant` fast
/// path; a user timer is called with no arguments and must return a number.
fn timer_now(timer: i64) -> Result<f64, i64> {
    if timer == NONE || is_default_timer(timer) {
        return Ok(now_seconds());
    }
    let result = builtins::call(timer, 0, std::ptr::null());
    if exc::pending() {
        return Err(NONE);
    }
    if let Some(seconds) = value::as_float(result) {
        return Ok(seconds);
    }
    match value::as_integer(result) {
        Some(seconds) => Ok(seconds as f64),
        None => Err(type_error("timer must return a number")),
    }
}

/// Calls the `Timer.setup` callable once, when present.
fn run_setup(instance: i64) -> Result<(), i64> {
    let setup = value::instance_attr(instance, "setup").unwrap_or(NONE);
    if setup == NONE {
        return Ok(());
    }
    builtins::call(setup, 0, std::ptr::null());
    if exc::pending() {
        Err(NONE)
    } else {
        Ok(())
    }
}

/// Times `number` calls of the instance's statement, returning a float value.
fn timeit_instance(instance: i64, number: u64) -> i64 {
    let Some(stmt) = value::instance_attr(instance, "stmt") else {
        return exc::raise_with_message(
            RUNTIME_ERROR,
            "Timer instance has no statement; construct it with timeit.Timer(...)",
        );
    };
    if let Err(error) = reject_source(stmt, "statement") {
        return error;
    }
    if let Err(error) = run_setup(instance) {
        return error;
    }
    let timer = value::instance_attr(instance, "timer").unwrap_or(NONE);
    let start = match timer_now(timer) {
        Ok(seconds) => seconds,
        Err(error) => return error,
    };
    for _ in 0..number {
        builtins::call(stmt, 0, std::ptr::null());
        if exc::pending() {
            return NONE;
        }
    }
    let end = match timer_now(timer) {
        Ok(seconds) => seconds,
        Err(error) => return error,
    };
    value::float_new(end - start)
}

/// `Timer.timeit(number=None)`.
fn timer_timeit(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("Timer.timeit() missing required argument: 'self'");
    };
    // Internal methods take `self` first; ignore non-instance receivers here
    // and report the attribute error through the missing `stmt`.
    let number = match loop_count(args, 1, "timeit") {
        Ok(number) => number,
        Err(error) => return error,
    };
    timeit_instance(instance, number)
}

/// `Timer.repeat(repeat=5, number=None)`.
fn timer_repeat(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("Timer.repeat() missing required argument: 'self'");
    };
    let repeat = match repeat_count(args, 1) {
        Ok(repeat) => repeat,
        Err(error) => return error,
    };
    let number = match loop_count(args, 2, "repeat") {
        Ok(number) => number,
        Err(error) => return error,
    };
    let mut results = Vec::with_capacity(repeat);
    for _ in 0..repeat {
        let elapsed = timeit_instance(instance, number);
        if exc::pending() {
            return NONE;
        }
        results.push(elapsed);
    }
    value::list_new(results)
}

/// `Timer.autorange(callback=None)`: 1, 2, 5, 10, 20, 50, ... loops until a
/// run reaches [`AUTORANGE_TARGET`] seconds.
fn timer_autorange(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("Timer.autorange() missing required argument: 'self'");
    };
    let callback = args.get(1).copied().unwrap_or(NONE);
    let mut base: i64 = 1;
    loop {
        for multiplier in [1i64, 2, 5] {
            let Some(number) = base.checked_mul(multiplier) else {
                return exc::raise_with_message(
                    RUNTIME_ERROR,
                    "autorange exceeded the supported loop count",
                );
            };
            let Some(encoded) = value::encode_int(number) else {
                return exc::raise_with_message(
                    RUNTIME_ERROR,
                    "autorange exceeded the supported loop count",
                );
            };
            let elapsed = timeit_instance(instance, number as u64);
            if exc::pending() {
                return NONE;
            }
            if callback != NONE {
                let callback_args = [encoded, elapsed];
                builtins::call(callback, 2, callback_args.as_ptr());
                if exc::pending() {
                    return NONE;
                }
            }
            let seconds = value::as_float(elapsed).unwrap_or(0.0);
            if seconds >= AUTORANGE_TARGET {
                return value::tuple_new(vec![encoded, elapsed]);
            }
        }
        base = match base.checked_mul(10) {
            Some(next) => next,
            None => {
                return exc::raise_with_message(
                    RUNTIME_ERROR,
                    "autorange exceeded the supported loop count",
                )
            }
        };
    }
}

/// Module-level `timeit(stmt, setup=None, timer=None, number=None, globals=None)`.
fn module_timeit(args: &[i64]) -> i64 {
    let instance = match timer_from(
        args.first().copied(),
        args.get(1).copied(),
        args.get(2).copied(),
    ) {
        Ok(instance) => instance,
        Err(error) => return error,
    };
    let number = match loop_count(args, 3, "timeit") {
        Ok(number) => number,
        Err(error) => return error,
    };
    timeit_instance(instance, number)
}

/// Module-level
/// `repeat(stmt, setup=None, timer=None, repeat=5, number=None, globals=None)`.
fn module_repeat(args: &[i64]) -> i64 {
    let instance = match timer_from(
        args.first().copied(),
        args.get(1).copied(),
        args.get(2).copied(),
    ) {
        Ok(instance) => instance,
        Err(error) => return error,
    };
    let repeat = match repeat_count(args, 3) {
        Ok(repeat) => repeat,
        Err(error) => return error,
    };
    let number = match loop_count(args, 4, "repeat") {
        Ok(number) => number,
        Err(error) => return error,
    };
    let mut results = Vec::with_capacity(repeat);
    for _ in 0..repeat {
        let elapsed = timeit_instance(instance, number);
        if exc::pending() {
            return NONE;
        }
        results.push(elapsed);
    }
    value::list_new(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(raw: &str) -> i64 {
        value::str_alloc(raw.to_string())
    }

    fn number(raw: i64) -> i64 {
        value::encode_int(raw).expect("tagged int")
    }

    fn default_stmt() -> i64 {
        value::native_function_new("timeit", "default_timer", 0)
    }

    #[test]
    fn default_timer_is_monotonic_and_float() {
        exc::init();
        let first = call("default_timer", &[]);
        let second = call("default_timer", &[]);
        assert!(!exc::pending());
        let first = value::as_float(first).expect("float");
        let second = value::as_float(second).expect("float");
        assert!(first.is_finite());
        assert!(second >= first);
    }

    #[test]
    fn timer_class_exposes_expected_methods() {
        let class = timer_class();
        assert!(value::class_method(class, "timeit").is_some());
        assert!(value::class_method(class, "repeat").is_some());
        assert!(value::class_method(class, "autorange").is_some());
    }

    #[test]
    fn timer_constructor_stores_attributes() {
        exc::init();
        let stmt = default_stmt();
        let instance = match timer_from(Some(stmt), None, None) {
            Ok(instance) => instance,
            Err(error) => panic!("timer construction failed: {error}"),
        };
        assert_eq!(value::instance_attr(instance, "stmt"), Some(stmt));
        let timer = value::instance_attr(instance, "timer").expect("timer attr");
        assert!(is_default_timer(timer));
        assert!(!exc::pending());
    }

    #[test]
    fn timeit_and_repeat_measure_calls() {
        exc::init();
        let stmt = default_stmt();
        let instance = match timer_from(Some(stmt), None, None) {
            Ok(instance) => instance,
            Err(error) => panic!("timer construction failed: {error}"),
        };
        let elapsed = call("_timer_timeit", &[instance, number(3)]);
        assert!(!exc::pending());
        assert!(value::as_float(elapsed).expect("float") >= 0.0);

        let results = call("_timer_repeat", &[instance, number(2), number(2)]);
        assert!(!exc::pending());
        let results = value::as_list(results).expect("list");
        assert_eq!(results.len(), 2);
        for result in results {
            assert!(value::as_float(*result).expect("float") >= 0.0);
        }
    }

    #[test]
    fn autorange_returns_number_and_elapsed() {
        exc::init();
        let stmt = default_stmt();
        let instance = match timer_from(Some(stmt), None, None) {
            Ok(instance) => instance,
            Err(error) => panic!("timer construction failed: {error}"),
        };
        let result = call("_timer_autorange", &[instance]);
        assert!(!exc::pending());
        let values = value::as_tuple(result).expect("tuple");
        assert_eq!(values.len(), 2);
        let loops = value::as_integer(values[0]).expect("int");
        assert!(loops >= 1);
        let elapsed = value::as_float(values[1]).expect("float");
        assert!(elapsed >= AUTORANGE_TARGET);
    }

    #[test]
    fn module_level_helpers() {
        exc::init();
        let stmt = default_stmt();
        let elapsed = call("timeit", &[stmt, NONE, NONE, number(2)]);
        assert!(!exc::pending());
        assert!(value::as_float(elapsed).expect("float") >= 0.0);

        let results = call("repeat", &[stmt, NONE, NONE, number(2), number(1)]);
        assert!(!exc::pending());
        assert_eq!(value::as_list(results).expect("list").len(), 2);
    }

    #[test]
    fn string_statements_are_rejected() {
        exc::init();
        assert_eq!(call("timeit", &[text("pass")]), NONE);
        assert!(exc::pending());
        exc::clear();

        assert_eq!(call("Timer", &[text("pass")]), NONE);
        assert!(exc::pending());
        exc::clear();

        assert_eq!(call("timeit", &[default_stmt(), text("x = 1")]), NONE);
        assert!(exc::pending());
        exc::clear();
    }

    #[test]
    fn negative_loop_counts_are_rejected() {
        exc::init();
        assert_eq!(
            call("timeit", &[default_stmt(), NONE, NONE, number(-1)]),
            NONE
        );
        assert!(exc::pending());
        exc::clear();
    }
}
