//! Integration tests for the `random`, `platform` and `statistics` modules
//! (module functions, the `Random` object, native function values).
//!
//! The tests drive only the public runtime ABI, exactly like generated code.

use pyx_runtime::abi::*;

const VALUE_ERROR: i64 = 0;
const TYPE_ERROR: i64 = 1;
const INDEX_ERROR: i64 = 6;
const ATTRIBUTE_ERROR: i64 = 9;

// `pyx_binop` op codes (see `ops.rs`).
const OP_ADD: u32 = 0;
const OP_EQ: u32 = 7;
const OP_LT: u32 = 9;
const OP_GE: u32 = 12;
const OP_IN: u32 = 13;

fn int(n: i64) -> i64 {
    n << 3
}

fn raw(value: i64) -> i64 {
    value >> 3
}

fn text(value: &str) -> i64 {
    pyx_str_new(value.as_ptr(), value.len())
}

fn list(items: &[i64]) -> i64 {
    pyx_list_new(items.len(), items.as_ptr())
}

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    pyx_method(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
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

fn get(module: &str, name: &str) -> i64 {
    pyx_module_get(module.as_ptr(), module.len(), name.as_ptr(), name.len())
}

fn call(function: i64, args: &[i64]) -> i64 {
    pyx_call(function, args.len(), args.as_ptr())
}

fn eq(a: i64, b: i64) -> bool {
    pyx_binop(OP_EQ, a, b) == pyx_true()
}

fn bin_bool(op: u32, a: i64, b: i64) -> bool {
    pyx_binop(op, a, b) == pyx_true()
}

fn assert_no_error() {
    assert_eq!(pyx_exc_pending(), 0, "unexpected pending exception");
}

fn assert_int(actual: i64, expected: i64) {
    assert_no_error();
    assert_eq!(actual, int(expected), "integer mismatch");
}

fn assert_str(actual: i64, expected: &str) {
    assert_no_error();
    assert!(
        eq(actual, text(expected)),
        "string mismatch, got {:?}",
        pyx_to_str(actual)
    );
}

fn assert_float(actual: i64, expected: f64) {
    assert_no_error();
    assert!(
        eq(actual, pyx_float_new(expected)),
        "float mismatch, got {:?}",
        pyx_to_str(actual)
    );
}

fn expect_error(tag: i64) {
    assert_eq!(pyx_exc_pending(), 1, "expected a pending exception");
    assert_eq!(pyx_exc_tag(), tag, "wrong exception tag");
    pyx_exc_clear();
}

/// Serialises tests that use the module-level global generator, whose state is
/// shared by every test running in this binary.
fn lock_global_random() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// -------------------------------------------------------------------------
// random: module functions
// -------------------------------------------------------------------------

#[test]
fn random_module_seed_is_deterministic() {
    let _random = lock_global_random();
    pyx_runtime_init();
    let seed = [int(1)];
    let bounds = [int(1), int(3)];
    module("random", "seed", &seed);
    assert_no_error();
    let first: Vec<i64> = (0..8)
        .map(|_| raw(module("random", "randint", &bounds)))
        .collect();
    assert_no_error();
    module("random", "seed", &seed);
    assert_no_error();
    let second: Vec<i64> = (0..8)
        .map(|_| raw(module("random", "randint", &bounds)))
        .collect();
    assert_no_error();
    assert_eq!(first, second);
    for value in first {
        assert!((1..=3).contains(&value), "randint out of range: {value}");
    }

    module("random", "seed", &seed);
    let draw = module("random", "random", &[]);
    assert_no_error();
    assert!(bin_bool(OP_GE, draw, pyx_float_new(0.0)));
    assert!(bin_bool(OP_LT, draw, pyx_float_new(1.0)));
}

#[test]
fn random_module_seed_accepts_float_and_none() {
    let _random = lock_global_random();
    pyx_runtime_init();
    module("random", "seed", &[pyx_float_new(2.5)]);
    assert_no_error();
    let first = raw(module("random", "randrange", &[int(100)]));
    assert_no_error();
    module("random", "seed", &[pyx_float_new(2.5)]);
    assert_no_error();
    let second = raw(module("random", "randrange", &[int(100)]));
    assert_no_error();
    assert_eq!(first, second);

    module("random", "seed", &[pyx_none()]);
    assert_no_error();
    let value = raw(module("random", "randrange", &[int(10)]));
    assert_no_error();
    assert!((0..10).contains(&value));

    module("random", "seed", &[text("no")]);
    expect_error(TYPE_ERROR);
}

#[test]
fn random_randrange_and_randint_bounds_and_errors() {
    let _random = lock_global_random();
    pyx_runtime_init();
    module("random", "seed", &[int(7)]);
    assert_no_error();

    // randrange(stop) starts at zero.
    for _ in 0..16 {
        let value = raw(module("random", "randrange", &[int(5)]));
        assert_no_error();
        assert!((0..5).contains(&value));
    }
    // randrange(start, stop) and a negative step.
    for _ in 0..16 {
        let value = raw(module("random", "randrange", &[int(-3), int(4)]));
        assert_no_error();
        assert!((-3..4).contains(&value));
    }
    for _ in 0..16 {
        let value = raw(module("random", "randrange", &[int(10), int(0), int(-2)]));
        assert_no_error();
        assert!((2..=10).contains(&value) && value % 2 == 0);
    }

    module("random", "randrange", &[int(5), int(5)]);
    expect_error(VALUE_ERROR);
    module("random", "randrange", &[int(0), int(5), int(0)]);
    expect_error(VALUE_ERROR);
    module("random", "randrange", &[pyx_float_new(2.0)]);
    expect_error(TYPE_ERROR);

    module("random", "randint", &[int(5), int(1)]);
    expect_error(VALUE_ERROR);
    module("random", "randint", &[int(1), pyx_float_new(3.0)]);
    expect_error(TYPE_ERROR);
}

#[test]
fn random_choice_by_sequence_kind() {
    let _random = lock_global_random();
    pyx_runtime_init();
    module("random", "seed", &[int(3)]);
    assert_no_error();

    let chars = ["a", "b", "c"];
    for _ in 0..16 {
        let picked = module("random", "choice", &[text("abc")]);
        assert_no_error();
        assert!(chars.iter().any(|candidate| eq(picked, text(candidate))));
    }

    let items = list(&[int(10), int(20), int(30)]);
    for _ in 0..16 {
        let picked = module("random", "choice", &[items]);
        assert_no_error();
        assert!(
            eq(picked, int(10)) || eq(picked, int(20)) || eq(picked, int(30)),
            "unexpected list choice"
        );
    }

    let tuple_items = [int(1), int(2)];
    let tuple = pyx_tuple_new(tuple_items.len(), tuple_items.as_ptr());
    let picked = module("random", "choice", &[tuple]);
    assert_no_error();
    assert!(eq(picked, int(1)) || eq(picked, int(2)));

    let bytes = pyx_bytes_new(b"\x01\x02\x03".as_ptr(), 3);
    for _ in 0..16 {
        let picked = module("random", "choice", &[bytes]);
        assert_no_error();
        let value = raw(picked);
        assert!((1..=3).contains(&value), "bytes choice: {value}");
    }

    module("random", "choice", &[list(&[])]);
    expect_error(INDEX_ERROR);
    module("random", "choice", &[text("")]);
    expect_error(INDEX_ERROR);
    module("random", "choice", &[int(1)]);
    expect_error(TYPE_ERROR);
}

#[test]
fn random_uniform_and_shuffle() {
    let _random = lock_global_random();
    pyx_runtime_init();
    module("random", "seed", &[int(11)]);
    assert_no_error();
    for _ in 0..16 {
        let value = module("random", "uniform", &[int(2), int(5)]);
        assert_no_error();
        assert!(
            bin_bool(OP_GE, value, pyx_float_new(2.0))
                && bin_bool(OP_LT, value, pyx_float_new(5.0)),
            "uniform out of range"
        );
    }
    module("random", "uniform", &[text("a"), int(1)]);
    expect_error(TYPE_ERROR);

    let original: Vec<i64> = (0..10).map(int).collect();
    let shuffled = list(&original);
    assert_int(pyx_len(shuffled), 10);
    module("random", "shuffle", &[shuffled]);
    assert_no_error();
    assert_int(pyx_len(shuffled), 10);

    // Every value 0..9 appears exactly once.
    let mut seen = [false; 10];
    for index in 0..10 {
        let item = pyx_subscript(shuffled, int(index));
        assert_no_error();
        let mut found = false;
        for candidate in 0..10 {
            if eq(item, int(candidate)) {
                assert!(!seen[candidate as usize], "duplicate after shuffle");
                seen[candidate as usize] = true;
                found = true;
            }
        }
        assert!(found, "unexpected element after shuffle");
    }
    assert!(seen.iter().all(|seen| *seen));

    // The same seed reproduces the same permutation.
    let first = list(&original);
    let second = list(&original);
    module("random", "seed", &[int(99)]);
    assert_no_error();
    module("random", "shuffle", &[first]);
    assert_no_error();
    module("random", "seed", &[int(99)]);
    assert_no_error();
    module("random", "shuffle", &[second]);
    assert_no_error();
    assert!(eq(first, second));

    module("random", "shuffle", &[text("abc")]);
    expect_error(TYPE_ERROR);
}

// -------------------------------------------------------------------------
// random: Random objects
// -------------------------------------------------------------------------

#[test]
fn random_object_seed_and_independent_states() {
    pyx_runtime_init();
    let rng = module("random", "Random", &[int(42)]);
    assert_no_error();
    assert_eq!(pyx_truthy(rng), 1);
    assert_str(pyx_to_str(rng), "<random.Random>");

    method(rng, "seed", &[int(42)]);
    assert_no_error();
    let first = raw(method(rng, "randint", &[int(1), int(1000)]));
    assert_no_error();
    method(rng, "seed", &[int(42)]);
    assert_no_error();
    let second = raw(method(rng, "randint", &[int(1), int(1000)]));
    assert_no_error();
    assert_eq!(first, second);

    // Two generators with different seeds advance independently.
    let left = module("random", "Random", &[int(1)]);
    let right = module("random", "Random", &[int(2)]);
    assert_no_error();
    let left_draws: Vec<i64> = (0..6)
        .map(|_| raw(method(left, "randrange", &[int(1000)])))
        .collect();
    assert_no_error();
    let right_draws: Vec<i64> = (0..6)
        .map(|_| raw(method(right, "randrange", &[int(1000)])))
        .collect();
    assert_no_error();
    assert_ne!(left_draws, right_draws);

    // Reseeding one object does not disturb the other.
    method(left, "seed", &[int(7)]);
    assert_no_error();
    let next_left = raw(method(left, "randrange", &[int(1000)]));
    assert_no_error();
    let next_right = raw(method(right, "randrange", &[int(1000)]));
    assert_no_error();
    method(right, "seed", &[int(7)]);
    assert_no_error();
    let reseeded_right = raw(method(right, "randrange", &[int(1000)]));
    assert_no_error();
    assert_eq!(next_left, reseeded_right);
    assert_ne!(next_left, next_right);

    // Constructor and method validation.
    assert_eq!(pyx_truthy(module("random", "Random", &[])), 1);
    assert_no_error();
    module("random", "Random", &[text("bad")]);
    expect_error(TYPE_ERROR);
    method(rng, "randint", &[int(1)]);
    expect_error(TYPE_ERROR);
    method(rng, "bogus", &[]);
    expect_error(ATTRIBUTE_ERROR);
}

// -------------------------------------------------------------------------
// statistics
// -------------------------------------------------------------------------

#[test]
fn statistics_known_values() {
    pyx_runtime_init();
    let one_two_three = list(&[int(1), int(2), int(3)]);
    assert_float(module("statistics", "mean", &[one_two_three]), 2.0);
    assert_float(module("statistics", "fmean", &[one_two_three]), 2.0);
    assert_float(module("statistics", "median", &[one_two_three]), 2.0);
    assert_float(module("statistics", "stdev", &[one_two_three]), 1.0);

    let even = list(&[int(4), int(1), int(3), int(2)]);
    assert_float(module("statistics", "median", &[even]), 2.5);
    assert_float(module("statistics", "mean", &[even]), 2.5);

    // Population standard deviation of this classic sample is exactly 2.
    let classic = list(&[
        int(2),
        int(4),
        int(4),
        int(4),
        int(5),
        int(5),
        int(7),
        int(9),
    ]);
    assert_float(module("statistics", "pstdev", &[classic]), 2.0);

    // range and bytes are accepted sequence kinds.
    let range = pyx_range_new(int(1), int(4), int(1));
    assert_float(module("statistics", "mean", &[range]), 2.0);
    let bytes = pyx_bytes_new(b"\x01\x02\x03".as_ptr(), 3);
    assert_float(module("statistics", "mean", &[bytes]), 2.0);

    // Tuple sequences work too.
    let tuple_items = [int(1), int(2), int(3)];
    let tuple = pyx_tuple_new(tuple_items.len(), tuple_items.as_ptr());
    assert_float(module("statistics", "fmean", &[tuple]), 2.0);
}

#[test]
fn statistics_errors() {
    pyx_runtime_init();
    module("statistics", "mean", &[list(&[])]);
    expect_error(VALUE_ERROR);
    module("statistics", "median", &[list(&[])]);
    expect_error(VALUE_ERROR);
    module("statistics", "pstdev", &[list(&[])]);
    expect_error(VALUE_ERROR);
    module("statistics", "stdev", &[list(&[int(1)])]);
    expect_error(VALUE_ERROR);
    module("statistics", "mean", &[list(&[text("x")])]);
    expect_error(TYPE_ERROR);
    module("statistics", "mean", &[int(1)]);
    expect_error(TYPE_ERROR);
}

// -------------------------------------------------------------------------
// platform
// -------------------------------------------------------------------------

#[test]
fn platform_values_are_native_and_nonempty() {
    pyx_runtime_init();
    let expected_system = if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "macos") {
        "Darwin"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        std::env::consts::OS
    };
    let system = module("platform", "system", &[]);
    assert_str(system, expected_system);

    let machine = module("platform", "machine", &[]);
    assert_str(machine, std::env::consts::ARCH);

    let release = module("platform", "release", &[]);
    assert_no_error();
    if cfg!(target_os = "linux") {
        assert!(!eq(release, text("")), "release must be non-empty on Linux");
    }
    let version = module("platform", "version", &[]);
    assert_no_error();
    if cfg!(target_os = "linux") {
        assert!(!eq(version, text("")), "version must be non-empty on Linux");
    }

    let node = module("platform", "node", &[]);
    assert_no_error();
    assert!(!eq(node, text("")), "node must be non-empty");

    let platform = module("platform", "platform", &[]);
    assert_no_error();
    let prefix = pyx_slice(platform, int(0), int(expected_system.len() as i64), int(1));
    assert_no_error();
    assert!(
        eq(prefix, text(expected_system)),
        "platform() must start with system()"
    );

    let version = module("platform", "python_version", &[]);
    assert_no_error();
    assert!(!eq(version, text("")));
    assert!(bin_bool(OP_IN, text("."), version));

    let implementation = get("platform", "python_implementation");
    assert_str(implementation, "PyX");
}

// -------------------------------------------------------------------------
// Native function values
// -------------------------------------------------------------------------

#[test]
fn native_function_values_call_through_pyx_call() {
    let _random = lock_global_random();
    pyx_runtime_init();
    module("random", "seed", &[int(123)]);
    assert_no_error();
    let direct = raw(module("random", "randint", &[int(1), int(50)]));
    assert_no_error();

    module("random", "seed", &[int(123)]);
    assert_no_error();
    let randint = get("random", "randint");
    assert_no_error();
    assert_eq!(pyx_truthy(randint), 1);
    let through_value = raw(call(randint, &[int(1), int(50)]));
    assert_no_error();
    assert_eq!(direct, through_value);

    let random = get("random", "random");
    let draw = call(random, &[]);
    assert_no_error();
    assert!(bin_bool(OP_GE, draw, pyx_float_new(0.0)));
    assert!(bin_bool(OP_LT, draw, pyx_float_new(1.0)));

    let system = get("platform", "system");
    let value = call(system, &[]);
    assert_no_error();
    assert!(!eq(value, text("")));

    let mean = get("statistics", "mean");
    let value = call(mean, &[list(&[int(2), int(4)])]);
    assert_float(value, 3.0);

    // A native function value survives only as a function; calling a plain
    // value reports TypeError instead of panicking.
    call(int(1), &[]);
    expect_error(TYPE_ERROR);
}

// -------------------------------------------------------------------------
// Builtin function values
// -------------------------------------------------------------------------

fn builtin(name: &str) -> i64 {
    pyx_builtin_function(name.as_ptr(), name.len())
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

#[test]
fn builtin_function_values_call_through_pyx_call() {
    pyx_runtime_init();
    let to_int = builtin("int");
    assert_no_error();
    assert_eq!(pyx_truthy(to_int), 1);

    // `f = int; print(f("42") + 1)`.
    let parsed = call(to_int, &[text("42")]);
    assert_int(parsed, 42);
    assert_int(pyx_binop(OP_ADD, parsed, int(1)), 43);

    // Constructors work as values too (preparation for default_factory).
    let empty = call(builtin("dict"), &[]);
    assert_no_error();
    assert_int(pyx_len(empty), 0);
    let empty = call(builtin("set"), &[]);
    assert_no_error();
    assert_int(pyx_len(empty), 0);

    // Unknown builtin names report AttributeError.
    builtin("nope");
    expect_error(ATTRIBUTE_ERROR);

    // Arity follows the direct builtin calls.
    call(builtin("len"), &[]);
    expect_error(TYPE_ERROR);
    call(builtin("int"), &[int(1), int(2)]);
    expect_error(TYPE_ERROR);
}

#[test]
fn builtin_map_and_list_take_function_values() {
    pyx_runtime_init();
    let to_int = builtin("int");
    let source = list(&[text("1"), text("2")]);
    let mapped = call(builtin("map"), &[to_int, source]);
    assert_no_error();
    let result = call(builtin("list"), &[mapped]);
    assert_no_error();
    assert_int(pyx_subscript(result, int(0)), 1);
    assert_int(pyx_subscript(result, int(1)), 2);

    // `range` as a value builds a lazy range object.
    let range = call(builtin("range"), &[int(3)]);
    assert_no_error();
    let values = call(builtin("list"), &[range]);
    assert_no_error();
    assert_int(pyx_len(values), 3);
}

// -------------------------------------------------------------------------
// Method keyword arguments
// -------------------------------------------------------------------------

#[test]
fn string_methods_bind_keyword_arguments() {
    pyx_runtime_init();
    let replaced = method_kw(
        text("a-a-a"),
        "replace",
        &[text("a"), text("b")],
        &[("count", int(1))],
    );
    assert_str(replaced, "b-a-a");

    // Keyword-only calls omit the trailing optional parameters entirely.
    let replaced = method_kw(
        text("a-a"),
        "replace",
        &[],
        &[("old", text("a")), ("new", text("b"))],
    );
    assert_str(replaced, "b-b");
    let parts = method_kw(text("a,b"), "split", &[], &[("sep", text(","))]);
    assert_no_error();
    assert_int(pyx_len(parts), 2);

    let parts = method_kw(text("a b c"), "split", &[], &[("maxsplit", int(1))]);
    assert_no_error();
    assert_int(pyx_len(parts), 2);
    assert_str(pyx_subscript(parts, int(0)), "a");
    assert_str(pyx_subscript(parts, int(1)), "b c");

    let starts = method_kw(
        text("prefix"),
        "startswith",
        &[],
        &[("prefix", text("pre"))],
    );
    assert_no_error();
    assert_eq!(starts, pyx_true());
    let ends = method_kw(text("suffix"), "endswith", &[], &[("suffix", text("fix"))]);
    assert_no_error();
    assert_eq!(ends, pyx_true());
}

#[test]
fn method_keywords_reject_unknown_and_duplicate_names() {
    pyx_runtime_init();

    method_kw(
        text("a-a"),
        "replace",
        &[text("a"), text("b")],
        &[("nope", int(1))],
    );
    expect_error(TYPE_ERROR);

    method_kw(
        text("a-a"),
        "replace",
        &[text("a"), text("b")],
        &[("old", text("c"))],
    );
    expect_error(TYPE_ERROR);

    method_kw(
        text("a-a"),
        "replace",
        &[],
        &[("old", text("a")), ("old", text("b")), ("new", text("c"))],
    );
    expect_error(TYPE_ERROR);

    // Receivers without keyword binding reject keywords explicitly.
    method_kw(list(&[]), "append", &[int(1)], &[("value", int(2))]);
    expect_error(TYPE_ERROR);

    let random = module("random", "Random", &[]);
    assert_no_error();
    method_kw(random, "seed", &[], &[("a", int(1))]);
    expect_error(TYPE_ERROR);
}
