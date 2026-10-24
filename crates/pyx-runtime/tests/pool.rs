//! Integration tests for `multiprocessing.Pool`.
//!
//! The runtime is process-global, so every test body runs in a worker thread
//! under `TEST_LOCK` with a watchdog: a deadlock in task submission, result
//! collection or a worker teardown fails after a timeout instead of hanging
//! the harness.
//!
//! The tests use the `fork` start method. The `spawn` path re-executes the
//! current executable and is exercised by the CLI/AOT end-to-end tests; a
//! test binary does not run `pyx_spawn_child` at startup.

use std::sync::mpsc::RecvTimeoutError;
use std::sync::Mutex;
use std::time::Duration;

use pyx_runtime::abi::*;

/// Serialises the tests in this binary: the runtime is a process-global.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Maximum time a test body may run before it is considered deadlocked.
const WATCHDOG: Duration = Duration::from_secs(60);

fn guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Runs `body` under the test lock with a deadlock watchdog.
fn run<F: FnOnce() + Send + 'static>(name: &str, body: F) {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::Builder::new()
        .name(format!("test-{name}"))
        .spawn(move || {
            let _guard = guard();
            body();
            let _ = tx.send(());
        })
        .expect("watchdog thread");
    match rx.recv_timeout(WATCHDOG) {
        Ok(()) => handle.join().expect("test body panicked"),
        Err(RecvTimeoutError::Disconnected) => {
            handle.join().expect("test body panicked");
        }
        Err(RecvTimeoutError::Timeout) => {
            panic!("test '{name}' timed out: Pool operation deadlocked")
        }
    }
}

fn int(n: i64) -> i64 {
    n << 3
}

fn decode(value: i64) -> i64 {
    pyx_to_int(value) >> 3
}

fn text(value: &str) -> i64 {
    pyx_str_new(value.as_ptr(), value.len())
}

fn list(items: &[i64]) -> i64 {
    pyx_list_new(items.len(), items.as_ptr())
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

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    pyx_method(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

fn function(wrapper: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
    pyx_function_new(wrapper as *const u8, arity, name.as_ptr(), name.len())
}

fn register(wrapper: extern "C" fn(*const i64) -> i64, name: &str, arity: usize) {
    pyx_register_function(name.as_ptr(), name.len(), wrapper as *const u8, arity);
}

fn clear_error() {
    pyx_exc_clear();
}

fn equal(a: i64, b: i64) -> bool {
    // `pyx_binop` op 7 is `==`.
    pyx_binop(7, a, b) == pyx_true()
}

fn rendered(actual: i64, expected: &str) -> bool {
    equal(pyx_to_str(actual), text(expected))
}

// -------------------------------------------------------------------------
// Wrappers executed in the worker processes
// -------------------------------------------------------------------------

extern "C" fn square(argv: *const i64) -> i64 {
    // SAFETY: registered with arity 1.
    let args = unsafe { std::slice::from_raw_parts(argv, 1) };
    let n = decode(args[0]);
    int(n * n)
}

extern "C" fn add(argv: *const i64) -> i64 {
    // SAFETY: registered with arity 2.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    int(decode(args[0]) + decode(args[1]))
}

extern "C" fn forty_two(argv: *const i64) -> i64 {
    let _ = argv;
    int(42)
}

extern "C" fn boom(argv: *const i64) -> i64 {
    let _ = argv;
    pyx_raise_msg(3, b"boom".as_ptr(), 4);
    pyx_none()
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[test]
fn fork_pool_map_apply_close_join() {
    run("pool-fork", || {
        pyx_runtime_init();
        assert_eq!(
            module("multiprocessing", "set_start_method", &[text("fork")]),
            pyx_none()
        );
        register(square, "square", 1);
        register(add, "add", 2);
        register(forty_two, "forty_two", 0);

        let pool = module("multiprocessing", "Pool", &[int(2)]);
        assert_eq!(pyx_exc_pending(), 0, "Pool(2) must not raise");
        assert!(rendered(pool, "<Pool(2 processes)>"));
        // The pool is the only owner of its queue objects; a collection must
        // trace those queue edges.
        pyx_gc_collect();
        assert_eq!(pyx_exc_pending(), 0);

        let square_fn = function(square, 1, "square");
        let values = list(&[int(1), int(2), int(3), int(4), int(5), int(6), int(7)]);
        let mapped = method(pool, "map", &[square_fn, values]);
        assert_eq!(pyx_exc_pending(), 0, "map must not raise");
        assert!(equal(
            mapped,
            list(&[int(1), int(4), int(9), int(16), int(25), int(36), int(49)])
        ));

        // An empty iterable must not touch the workers.
        let empty = method(pool, "map", &[square_fn, list(&[])]);
        assert_eq!(pyx_exc_pending(), 0);
        assert!(rendered(empty, "[]"));

        // `apply` with an explicit argument sequence and with the default.
        let applied = method(
            pool,
            "apply",
            &[function(add, 2, "add"), list(&[int(20), int(22)])],
        );
        assert_eq!(pyx_exc_pending(), 0);
        assert_eq!(decode(applied), 42);

        let noargs = method(pool, "apply", &[function(forty_two, 0, "forty_two")]);
        assert_eq!(pyx_exc_pending(), 0);
        assert_eq!(decode(noargs), 42);

        // Idempotent close and join.
        assert_eq!(method(pool, "close", &[]), pyx_none());
        assert_eq!(method(pool, "close", &[]), pyx_none());
        assert_eq!(method(pool, "join", &[]), pyx_none());
        assert_eq!(method(pool, "join", &[]), pyx_none());

        // A closed pool rejects new work with RuntimeError.
        method(pool, "map", &[square_fn, values]);
        assert_eq!(pyx_exc_pending(), 1);
        assert_eq!(pyx_exc_tag(), 3, "closed Pool raises RuntimeError");
        clear_error();

        method(pool, "bogus", &[]);
        assert_eq!(pyx_exc_pending(), 1);
        assert_eq!(pyx_exc_tag(), 9, "unknown methods raise AttributeError");
        assert!(rendered(
            pyx_exc_value(),
            "'Pool' object has no attribute 'bogus'"
        ));
        clear_error();

        // Leave the process-global start method at its default.
        module("multiprocessing", "set_start_method", &[text("fork")]);
    });
}

#[test]
fn fork_pool_propagates_worker_exception() {
    run("pool-fork-error", || {
        pyx_runtime_init();
        assert_eq!(
            module("multiprocessing", "set_start_method", &[text("fork")]),
            pyx_none()
        );
        register(boom, "boom", 1);
        register(square, "square", 1);

        let pool = module("multiprocessing", "Pool", &[int(2)]);
        assert_eq!(pyx_exc_pending(), 0);

        let out = method(
            pool,
            "map",
            &[function(boom, 1, "boom"), list(&[int(1), int(2), int(3)])],
        );
        assert_eq!(out, pyx_none());
        assert_eq!(pyx_exc_pending(), 1);
        assert_eq!(pyx_exc_tag(), 3, "worker failure surfaces as RuntimeError");
        assert!(rendered(pyx_exc_value(), "RuntimeError: boom"));
        clear_error();

        // Every queued result was drained, so the pool remains usable.
        let recovered = method(
            pool,
            "map",
            &[function(square, 1, "square"), list(&[int(3), int(4)])],
        );
        assert_eq!(pyx_exc_pending(), 0);
        assert!(equal(recovered, list(&[int(9), int(16)])));

        // ... and close/join still work.
        assert_eq!(method(pool, "close", &[]), pyx_none());
        assert_eq!(method(pool, "join", &[]), pyx_none());
        assert_eq!(pyx_exc_pending(), 0);
        module("multiprocessing", "set_start_method", &[text("fork")]);
    });
}

#[test]
fn fork_pool_terminate_kills_workers() {
    run("pool-terminate", || {
        pyx_runtime_init();
        assert_eq!(
            module("multiprocessing", "set_start_method", &[text("fork")]),
            pyx_none()
        );
        register(square, "square", 1);

        // Workers are blocked reading their (empty) task queue; terminate
        // must end them with a signal instead of a queue sentinel.
        let pool = module("multiprocessing", "Pool", &[int(2)]);
        assert_eq!(pyx_exc_pending(), 0);
        assert_eq!(method(pool, "terminate", &[]), pyx_none());
        assert_eq!(pyx_exc_pending(), 0);
        // Idempotent, and join/close after terminate still succeed.
        assert_eq!(method(pool, "terminate", &[]), pyx_none());
        assert_eq!(method(pool, "join", &[]), pyx_none());
        assert_eq!(method(pool, "close", &[]), pyx_none());
        assert_eq!(method(pool, "terminate", &[]), pyx_none());
        assert_eq!(pyx_exc_pending(), 0);

        // A terminated pool rejects new work.
        method(
            pool,
            "map",
            &[function(square, 1, "square"), list(&[int(1)])],
        );
        assert_eq!(pyx_exc_pending(), 1);
        assert_eq!(pyx_exc_tag(), 3, "terminated Pool raises RuntimeError");
        clear_error();

        // Terminate also works on a closed pool that was never joined.
        let closed = module("multiprocessing", "Pool", &[int(1)]);
        assert_eq!(pyx_exc_pending(), 0);
        assert_eq!(method(closed, "close", &[]), pyx_none());
        assert_eq!(method(closed, "terminate", &[]), pyx_none());
        assert_eq!(method(closed, "join", &[]), pyx_none());
        assert_eq!(pyx_exc_pending(), 0);

        module("multiprocessing", "set_start_method", &[text("fork")]);
    });
}

#[test]
fn pool_context_manager_enters_self_and_terminates() {
    run("pool-context", || {
        pyx_runtime_init();
        assert_eq!(
            module("multiprocessing", "set_start_method", &[text("fork")]),
            pyx_none()
        );
        register(square, "square", 1);

        let pool = module("multiprocessing", "Pool", &[int(2)]);
        assert_eq!(pyx_exc_pending(), 0);

        // `with Pool(...) as p` enters the pool itself.
        assert_eq!(method(pool, "__enter__", &[]), pool);
        let square_fn = function(square, 1, "square");
        let mapped = method(pool, "map", &[square_fn, list(&[int(2), int(3)])]);
        assert_eq!(pyx_exc_pending(), 0);
        assert!(equal(mapped, list(&[int(4), int(9)])));

        // `__exit__` terminates the workers and never suppresses errors.
        let none = [pyx_none(), pyx_none(), pyx_none()];
        assert_eq!(method(pool, "__exit__", &none), pyx_none());
        assert_eq!(pyx_exc_pending(), 0);
        // Idempotent, like `terminate()`.
        assert_eq!(method(pool, "__exit__", &none), pyx_none());
        assert_eq!(pyx_exc_pending(), 0);

        method(pool, "map", &[square_fn, list(&[int(1)])]);
        assert_eq!(pyx_exc_pending(), 1);
        assert_eq!(pyx_exc_tag(), 3, "terminated Pool raises RuntimeError");
        clear_error();

        module("multiprocessing", "set_start_method", &[text("fork")]);
    });
}

#[test]
fn pool_rejects_invalid_process_counts() {
    run("pool-config", || {
        pyx_runtime_init();
        assert_eq!(
            module("multiprocessing", "set_start_method", &[text("fork")]),
            pyx_none()
        );

        assert_eq!(module("multiprocessing", "Pool", &[int(0)]), pyx_none());
        assert_eq!(pyx_exc_pending(), 1);
        assert_eq!(pyx_exc_tag(), 0, "Pool(0) raises ValueError");
        clear_error();

        assert_eq!(module("multiprocessing", "Pool", &[int(-3)]), pyx_none());
        assert_eq!(pyx_exc_pending(), 1);
        assert_eq!(pyx_exc_tag(), 0, "Pool(-3) raises ValueError");
        clear_error();

        assert_eq!(
            module("multiprocessing", "Pool", &[text("many")]),
            pyx_none()
        );
        assert_eq!(pyx_exc_pending(), 1);
        assert_eq!(pyx_exc_tag(), 1, "Pool('many') raises TypeError");
        clear_error();

        module("multiprocessing", "set_start_method", &[text("fork")]);
    });
}
