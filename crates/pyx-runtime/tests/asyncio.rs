//! Runtime tests for async generators, tasks and the persistent asyncio loop.
//!
//! The ABI entry points are exercised directly: a test defines raw fiber
//! bodies (extern "C" functions) and drives them through `asyncio.run`, just
//! like generated wrappers do. Every test resets the runtime under a lock, so
//! the process-wide heap and thread registry are isolated.

use std::sync::{Mutex, MutexGuard};

use pyx_runtime::abi::{
    pyx_async_generator_new, pyx_await, pyx_binop, pyx_builtin_function, pyx_call,
    pyx_coroutine_new, pyx_exc_clear, pyx_exc_pending, pyx_exc_tag, pyx_exc_value,
    pyx_exception_class, pyx_false, pyx_float_new, pyx_gc_collect, pyx_generator_new, pyx_getattr,
    pyx_global_get, pyx_global_set, pyx_int, pyx_len, pyx_list_append, pyx_list_new, pyx_method,
    pyx_module_call, pyx_module_get, pyx_none, pyx_raise_msg, pyx_raise_value, pyx_runtime_init,
    pyx_str_new, pyx_subscript, pyx_to_int, pyx_to_str, pyx_true, pyx_truthy, pyx_yield,
};

static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Holds the test lock and leaves the runtime initialised for the body.
struct RuntimeTest {
    _guard: MutexGuard<'static, ()>,
    stress: bool,
}

impl RuntimeTest {
    fn new() -> RuntimeTest {
        RuntimeTest::with_stress(false)
    }

    fn with_stress(stress: bool) -> RuntimeTest {
        let guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if stress {
            std::env::set_var("PYX_GC_STRESS", "1");
        }
        pyx_runtime_init();
        RuntimeTest {
            _guard: guard,
            stress,
        }
    }
}

impl Drop for RuntimeTest {
    fn drop(&mut self) {
        if self.stress {
            std::env::remove_var("PYX_GC_STRESS");
        }
    }
}

fn int(n: i64) -> i64 {
    pyx_int(n)
}

fn decode(value: i64) -> i64 {
    pyx_to_int(value) >> 3
}

fn text(value: &str) -> i64 {
    pyx_str_new(value.as_ptr(), value.len())
}

/// Tagged-value equality through the runtime's `==` operator (code 7).
fn value_eq(left: i64, right: i64) -> bool {
    pyx_truthy(pyx_binop(7, left, right)) != 0
}

/// Asserts that `value` stringifies to `expected` (exception messages).
fn assert_message(value: i64, expected: &str) {
    let rendered = pyx_to_str(value);
    assert!(
        value_eq(rendered, text(expected)),
        "message mismatch: expected {expected:?}"
    );
}

/// Asserts that `value` is a list of exactly `expected` strings.
fn assert_string_list(value: i64, expected: &[&str]) {
    assert_eq!(decode(pyx_len(value)), expected.len() as i64);
    for (index, want) in expected.iter().enumerate() {
        let item = pyx_subscript(value, pyx_int(index as i64));
        assert!(
            value_eq(item, text(want)),
            "list element {index}: expected {want:?}"
        );
    }
}

fn list(items: &[i64]) -> i64 {
    pyx_list_new(items.len(), items.as_ptr())
}

fn append(marker: i64, value: i64) {
    pyx_list_append(marker, value);
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

fn module_get(module: &str, name: &str) -> i64 {
    pyx_module_get(module.as_ptr(), module.len(), name.as_ptr(), name.len())
}

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    pyx_method(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

fn set_global(name: &str, value: i64) {
    pyx_global_set(b"test".as_ptr(), 4, name.as_ptr(), name.len(), value);
}

fn get_global(name: &str) -> i64 {
    pyx_global_get(b"test".as_ptr(), 4, name.as_ptr(), name.len())
}

fn make_exception(tag: u32, message: &str) -> i64 {
    pyx_raise_msg(tag, message.as_ptr(), message.len());
    let value = pyx_exc_value();
    pyx_exc_clear();
    value
}

fn coro(entry: extern "C" fn(*const i64) -> i64, args: &[i64]) -> i64 {
    let argv = if args.is_empty() {
        std::ptr::null()
    } else {
        args.as_ptr()
    };
    pyx_coroutine_new(entry as *const u8, args.len(), argv)
}

fn async_gen(entry: extern "C" fn(*const i64) -> i64, args: &[i64]) -> i64 {
    let argv = if args.is_empty() {
        std::ptr::null()
    } else {
        args.as_ptr()
    };
    pyx_async_generator_new(entry as *const u8, args.len(), argv)
}

fn run(entry: extern "C" fn(*const i64) -> i64, args: &[i64]) -> i64 {
    let root = coro(entry, args);
    module("asyncio", "run", &[root])
}

fn pending_tag() -> i64 {
    if pyx_exc_pending() == 0 {
        -1
    } else {
        pyx_exc_tag()
    }
}

// -------------------------------------------------------------------------
// Fiber bodies
// -------------------------------------------------------------------------

/// `yield 1; yield <sent>; return None`.
extern "C" fn async_sent_values(_argv: *const i64) -> i64 {
    let received = pyx_yield(pyx_int(1));
    pyx_yield(received);
    pyx_none()
}

/// `yield 1; yield 2; return None`.
extern "C" fn async_two_values(_argv: *const i64) -> i64 {
    pyx_yield(pyx_int(1));
    pyx_yield(pyx_int(2));
    pyx_none()
}

extern "C" fn returns_7(_argv: *const i64) -> i64 {
    pyx_int(7)
}

extern "C" fn returns_9(_argv: *const i64) -> i64 {
    pyx_int(9)
}

extern "C" fn awaits_sleep(_argv: *const i64) -> i64 {
    module("asyncio", "sleep", &[pyx_float_new(0.001)]);
    pyx_int(9)
}

/// Sleeps for the seconds in `argv[0]`, then returns `argv[1]`.
extern "C" fn sleep_then_return(argv: *const i64) -> i64 {
    // SAFETY: the fiber was created with two arguments.
    let (seconds, result) = unsafe { (*argv, *argv.add(1)) };
    module("asyncio", "sleep", &[seconds]);
    result
}

/// Appends `argv[1]`, sleeps, appends `argv[2]`, and returns `argv[3]`.
extern "C" fn marker_task(argv: *const i64) -> i64 {
    // SAFETY: the fiber was created with four arguments.
    let (marker, first, second, result) =
        unsafe { (*argv, *argv.add(1), *argv.add(2), *argv.add(3)) };
    append(marker, first);
    module("asyncio", "sleep", &[pyx_float_new(0.0)]);
    append(marker, second);
    result
}

/// Sleeps 5 seconds, then records whether it was cancelled or ran.
extern "C" fn slow_task(argv: *const i64) -> i64 {
    // SAFETY: the fiber was created with one argument.
    let marker = unsafe { *argv };
    module("asyncio", "sleep", &[pyx_float_new(5.0)]);
    if pyx_exc_pending() != 0 {
        pyx_exc_clear();
        append(marker, text("cancelled"));
    } else {
        append(marker, text("ran"));
    }
    pyx_none()
}

/// Sleeps 5 seconds; records the cancellation and lets it propagate.
extern "C" fn recording_slow_task(argv: *const i64) -> i64 {
    // SAFETY: the fiber was created with one argument.
    let marker = unsafe { *argv };
    module("asyncio", "sleep", &[pyx_float_new(5.0)]);
    if pyx_exc_pending() != 0 {
        let error = pyx_exc_value();
        pyx_exc_clear();
        append(marker, text("cancelled"));
        pyx_raise_value(error);
    }
    pyx_none()
}

/// Records that its body ran (used by cancel-before-start).
extern "C" fn records_run(argv: *const i64) -> i64 {
    // SAFETY: the fiber was created with one argument.
    append(unsafe { *argv }, text("ran"));
    pyx_int(1)
}

extern "C" fn raises_value_error(_argv: *const i64) -> i64 {
    pyx_raise_msg(0, b"boom".as_ptr(), 4);
    pyx_none()
}

extern "C" fn reentrant_root(_argv: *const i64) -> i64 {
    let inner = coro(returns_7, &[]);
    let _ = module("asyncio", "run", &[inner]);
    pyx_none()
}

extern "C" fn cancel_at_sleep_root(argv: *const i64) -> i64 {
    // SAFETY: the root was created with one argument.
    let marker = unsafe { *argv };
    let task = module(
        "asyncio",
        "create_task",
        &[coro(recording_slow_task, &[marker])],
    );
    // Let the child start and park in `sleep(5)`.
    module("asyncio", "sleep", &[pyx_float_new(0.0)]);
    method(task, "cancel", &[]);
    pyx_await(task);
    pyx_none()
}

extern "C" fn cancel_before_start_root(argv: *const i64) -> i64 {
    // SAFETY: the root was created with one argument.
    let marker = unsafe { *argv };
    let task = module("asyncio", "create_task", &[coro(records_run, &[marker])]);
    method(task, "cancel", &[]);
    pyx_await(task);
    pyx_none()
}

extern "C" fn probe_task_root(_argv: *const i64) -> i64 {
    let task = module("asyncio", "create_task", &[coro(returns_7, &[])]);
    module("asyncio", "sleep", &[pyx_float_new(0.0)]);
    let done = if method(task, "done", &[]) == pyx_true() {
        1
    } else {
        0
    };
    let result = method(task, "result", &[]);
    let exception = method(task, "exception", &[]);
    let cancelled = if method(task, "cancelled", &[]) == pyx_false() {
        1
    } else {
        0
    };
    if done == 1
        && decode(result) == 7
        && exception == pyx_none()
        && cancelled == 1
        && pyx_exc_pending() == 0
    {
        pyx_int(1)
    } else {
        pyx_int(0)
    }
}

extern "C" fn await_task_root(_argv: *const i64) -> i64 {
    let task = module(
        "asyncio",
        "create_task",
        &[coro(sleep_then_return, &[pyx_float_new(0.001), pyx_int(0)])],
    );
    let value = pyx_await(task);
    pyx_int(decode(value) + 1)
}

extern "C" fn task_group_success_root(_argv: *const i64) -> i64 {
    let class = module_get("asyncio", "TaskGroup");
    let group = pyx_call(class, 0, std::ptr::null());
    method(group, "__aenter__", &[]);
    let first = method(
        group,
        "create_task",
        &[coro(sleep_then_return, &[pyx_float_new(0.001), pyx_int(1)])],
    );
    let second = method(
        group,
        "create_task",
        &[coro(sleep_then_return, &[pyx_float_new(0.002), pyx_int(2)])],
    );
    method(group, "__aexit__", &[pyx_none(), pyx_none(), pyx_none()]);
    pyx_int(decode(method(first, "result", &[])) + decode(method(second, "result", &[])))
}

extern "C" fn task_group_interleave_root(argv: *const i64) -> i64 {
    // SAFETY: the root was created with one argument.
    let marker = unsafe { *argv };
    let class = module_get("asyncio", "TaskGroup");
    let group = pyx_call(class, 0, std::ptr::null());
    method(group, "__aenter__", &[]);
    method(
        group,
        "create_task",
        &[coro(
            marker_task,
            &[marker, text("a"), text("a2"), pyx_int(1)],
        )],
    );
    method(
        group,
        "create_task",
        &[coro(
            marker_task,
            &[marker, text("b"), text("b2"), pyx_int(2)],
        )],
    );
    method(group, "__aexit__", &[pyx_none(), pyx_none(), pyx_none()]);
    pyx_none()
}

extern "C" fn task_group_error_root(argv: *const i64) -> i64 {
    // SAFETY: the root was created with one argument.
    let marker = unsafe { *argv };
    let class = module_get("asyncio", "TaskGroup");
    let group = pyx_call(class, 0, std::ptr::null());
    method(group, "__aenter__", &[]);
    method(group, "create_task", &[coro(slow_task, &[marker])]);
    method(group, "create_task", &[coro(raises_value_error, &[])]);
    method(group, "__aexit__", &[pyx_none(), pyx_none(), pyx_none()]);
    pyx_none()
}

extern "C" fn task_group_body_error_root(argv: *const i64) -> i64 {
    // SAFETY: the root was created with one argument.
    let marker = unsafe { *argv };
    let class = module_get("asyncio", "TaskGroup");
    let group = pyx_call(class, 0, std::ptr::null());
    method(group, "__aenter__", &[]);
    method(group, "create_task", &[coro(slow_task, &[marker])]);
    // Let the child start and park in `sleep(5)` so the body exception
    // exercises cancellation at a suspension point.
    module("asyncio", "sleep", &[pyx_float_new(0.0)]);
    let error = make_exception(0, "body");
    let exit = method(group, "__aexit__", &[pyx_none(), error, pyx_none()]);
    if pyx_truthy(exit) == 0 {
        // `async with` re-raises the body exception when `__aexit__` is falsy.
        pyx_raise_value(error);
    }
    pyx_none()
}

extern "C" fn leftover_root(argv: *const i64) -> i64 {
    // SAFETY: the root was created with one argument.
    let marker = unsafe { *argv };
    let _ = module("asyncio", "create_task", &[coro(slow_task, &[marker])]);
    module("asyncio", "sleep", &[pyx_float_new(0.0)]);
    pyx_int(5)
}

extern "C" fn current_task_root(_argv: *const i64) -> i64 {
    let current = module("asyncio", "current_task", &[]);
    if current != pyx_none() {
        pyx_int(1)
    } else {
        pyx_int(0)
    }
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[test]
fn exception_class_calls_and_type_identity() {
    let _rt = RuntimeTest::new();
    let type_fn = pyx_builtin_function(b"type".as_ptr(), 4);
    for (tag, name) in [(0u32, "ValueError"), (10, "OSError")] {
        let class = pyx_exception_class(tag);
        let error = pyx_call(class, 1, [text("x")].as_ptr());
        assert_eq!(pyx_exc_pending(), 0);
        assert_message(error, "x");

        let actual_type = pyx_call(type_fn, 1, [error].as_ptr());
        assert!(
            pyx_truthy(pyx_binop(15, actual_type, class)) != 0,
            "type({name}()) is {name}"
        );
        let rendered = pyx_getattr(class, b"__name__".as_ptr(), 8);
        assert!(
            value_eq(rendered, text(name)),
            "{name}.__name__ == {name:?}"
        );
    }
}

#[test]
fn task_group_factory_is_callable_directly_and_as_a_value() {
    let _rt = RuntimeTest::new();
    let first = module("asyncio", "TaskGroup", &[]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_eq!(method(first, "__aenter__", &[]), first);
    assert_eq!(
        method(first, "__aexit__", &[pyx_none(), pyx_none(), pyx_none()]),
        pyx_false()
    );

    let factory = module_get("asyncio", "TaskGroup");
    let second = pyx_call(factory, 0, std::ptr::null());
    assert_eq!(pyx_exc_pending(), 0);
    assert_eq!(method(second, "__aenter__", &[]), second);
    assert_eq!(
        method(second, "__aexit__", &[pyx_none(), pyx_none(), pyx_none()]),
        pyx_false()
    );
}

#[test]
fn async_generator_anext_asend_and_completion() {
    let _rt = RuntimeTest::new();
    let agen = async_gen(async_sent_values, &[]);
    assert_eq!(method(agen, "__aiter__", &[]), agen);
    assert_eq!(decode(method(agen, "__anext__", &[])), 1);
    assert_eq!(decode(method(agen, "asend", &[int(5)])), 5);
    assert_eq!(method(agen, "__anext__", &[]), pyx_none());
    assert_eq!(pending_tag(), 18);
    pyx_exc_clear();
    // An exhausted async generator keeps raising StopAsyncIteration.
    assert_eq!(method(agen, "__anext__", &[]), pyx_none());
    assert_eq!(pending_tag(), 18);
}

#[test]
fn async_generator_athrow_and_aclose() {
    let _rt = RuntimeTest::new();
    let thrown = async_gen(async_sent_values, &[]);
    assert_eq!(decode(method(thrown, "__anext__", &[])), 1);
    let exception = make_exception(0, "thrown");
    assert_eq!(method(thrown, "athrow", &[exception]), pyx_none());
    assert_eq!(pending_tag(), 0);
    assert_message(pyx_exc_value(), "thrown");
    pyx_exc_clear();

    let closed = async_gen(async_two_values, &[]);
    assert_eq!(decode(method(closed, "__anext__", &[])), 1);
    assert_eq!(method(closed, "aclose", &[]), pyx_none());
    // The body yielded again instead of exiting, which CPython reports as a
    // RuntimeError from aclose().
    assert_eq!(pending_tag(), 3);
    assert_message(pyx_exc_value(), "async generator ignored GeneratorExit");
    pyx_exc_clear();
    assert_eq!(method(closed, "__anext__", &[]), pyx_none());
    assert_eq!(pending_tag(), 18);
    pyx_exc_clear();

    // Closing a never-started generator runs no body and does not raise.
    let fresh = async_gen(async_two_values, &[]);
    assert_eq!(method(fresh, "aclose", &[]), pyx_none());
    assert_eq!(pyx_exc_pending(), 0);
}

#[test]
fn async_generator_survives_gc_while_suspended() {
    let _rt = RuntimeTest::with_stress(true);
    let agen = async_gen(async_two_values, &[]);
    set_global("agen", agen);
    assert_eq!(decode(method(agen, "__anext__", &[])), 1);
    pyx_gc_collect();
    let agen = get_global("agen");
    assert_eq!(decode(method(agen, "__anext__", &[])), 2);
    pyx_gc_collect();
    assert_eq!(method(agen, "__anext__", &[]), pyx_none());
    assert_eq!(pending_tag(), 18);
}

#[test]
fn await_propagates_sleep_through_nested_coroutines() {
    let _rt = RuntimeTest::new();
    let root = coro(awaits_sleep, &[]);
    let result = module("asyncio", "run", &[root]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_eq!(decode(result), 9);
}

#[test]
fn cancel_interrupts_a_sleeping_task() {
    let _rt = RuntimeTest::new();
    let marker = list(&[]);
    set_global("marker", marker);
    let result = run(cancel_at_sleep_root, &[marker]);
    assert_eq!(result, pyx_none());
    assert_eq!(pending_tag(), 19, "CancelledError propagates to the runner");
    assert_string_list(get_global("marker"), &["cancelled"]);
}

#[test]
fn cancel_before_start_does_not_run_the_body() {
    let _rt = RuntimeTest::new();
    let marker = list(&[]);
    set_global("marker", marker);
    let result = run(cancel_before_start_root, &[marker]);
    assert_eq!(result, pyx_none());
    assert_eq!(pending_tag(), 19);
    assert_string_list(get_global("marker"), &[]);
}

#[test]
fn task_probes_report_done_result_and_exception() {
    let _rt = RuntimeTest::new();
    let result = run(probe_task_root, &[]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_eq!(decode(result), 1);
}

#[test]
fn current_task_is_none_outside_a_step_and_set_inside() {
    let _rt = RuntimeTest::new();
    assert_eq!(module("asyncio", "current_task", &[]), pyx_none());
    let result = run(current_task_root, &[]);
    assert_eq!(decode(result), 1);
    assert_eq!(module("asyncio", "current_task", &[]), pyx_none());
}

#[test]
fn awaiting_a_scheduled_task_waits_without_double_driving() {
    let _rt = RuntimeTest::new();
    let result = run(await_task_root, &[]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_eq!(decode(result), 1);
}

#[test]
fn task_group_joins_tasks_and_returns_their_results() {
    let _rt = RuntimeTest::new();
    let result = run(task_group_success_root, &[]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_eq!(decode(result), 3);
}

#[test]
fn task_group_tasks_interleave_at_suspension_points() {
    let _rt = RuntimeTest::new();
    let marker = list(&[]);
    set_global("marker", marker);
    let result = run(task_group_interleave_root, &[marker]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_eq!(result, pyx_none());
    assert_string_list(get_global("marker"), &["a", "b", "a2", "b2"]);
}

#[test]
fn task_group_cancels_siblings_and_propagates_the_first_failure() {
    let _rt = RuntimeTest::new();
    let marker = list(&[]);
    set_global("marker", marker);
    let result = run(task_group_error_root, &[marker]);
    assert_eq!(result, pyx_none());
    assert_eq!(pending_tag(), 0);
    assert_message(pyx_exc_value(), "boom");
    assert_string_list(get_global("marker"), &["cancelled"]);
}

#[test]
fn task_group_cancels_tasks_when_the_body_raises() {
    let _rt = RuntimeTest::new();
    let marker = list(&[]);
    set_global("marker", marker);
    let result = run(task_group_body_error_root, &[marker]);
    assert_eq!(result, pyx_none());
    assert_eq!(pending_tag(), 0);
    assert_message(pyx_exc_value(), "body");
    assert_string_list(get_global("marker"), &["cancelled"]);
}

#[test]
fn run_cancels_leftover_tasks_and_keeps_the_loop_persistent() {
    let _rt = RuntimeTest::new();
    let marker = list(&[]);
    set_global("marker", marker);
    let result = run(leftover_root, &[marker]);
    assert_eq!(pyx_exc_pending(), 0);
    assert_eq!(decode(result), 5);
    // The leftover task was cancelled during shutdown and observed it.
    assert_string_list(get_global("marker"), &["cancelled"]);
    // The same thread-local loop drives a second, independent run.
    let marker = list(&[]);
    set_global("marker", marker);
    let result = run(leftover_root, &[marker]);
    assert_eq!(decode(result), 5);
    assert_string_list(get_global("marker"), &["cancelled"]);
}

#[test]
fn reentrant_run_is_a_runtime_error() {
    let _rt = RuntimeTest::new();
    let result = run(reentrant_root, &[]);
    assert_eq!(result, pyx_none());
    assert_eq!(pending_tag(), 3);
    assert_message(
        pyx_exc_value(),
        "asyncio.run() cannot be called from a running event loop",
    );
}

#[test]
fn asyncio_run_requires_a_coroutine() {
    let _rt = RuntimeTest::new();
    let generator = pyx_generator_new(returns_9 as *const u8, 0, std::ptr::null());
    let result = module("asyncio", "run", &[generator]);
    assert_eq!(result, pyx_none());
    assert_eq!(pending_tag(), 1);
}
