//! Integration tests for the concurrency runtime: `threading`, `queue`,
//! `pickle`, `multiprocessing` and the `os` fork primitives.
//!
//! The runtime state is process-global, so every test body runs in a worker
//! thread under `TEST_LOCK` (serialising the tests) with a watchdog: a deadlock
//! in a blocking operation fails the test after a timeout instead of hanging
//! the harness. Forked children only ever call `os._exit`, never the test
//! body, so a child can never continue the harness.

use std::sync::mpsc::RecvTimeoutError;
use std::sync::Mutex;
use std::time::Duration;

use hydro_runtime::abi::*;

/// Serialises the tests in this binary: the runtime is a process-global.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Maximum time a single test body may run before it is considered deadlocked.
const WATCHDOG: Duration = Duration::from_secs(30);

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
            // The body panicked (its sender was dropped) or the thread aborted.
            handle.join().expect("test body panicked");
        }
        Err(RecvTimeoutError::Timeout) => {
            panic!("test '{name}' timed out: blocking operation deadlocked")
        }
    }
}

fn int(n: i64) -> i64 {
    n << 3
}

fn decode(value: i64) -> i64 {
    hydro_to_int(value) >> 3
}

fn text(value: &str) -> i64 {
    hydro_str_new(value.as_ptr(), value.len())
}

fn list(items: &[i64]) -> i64 {
    hydro_list_new(items.len(), items.as_ptr())
}

fn tuple(items: &[i64]) -> i64 {
    hydro_tuple_new(items.len(), items.as_ptr())
}

fn set(items: &[i64]) -> i64 {
    hydro_set_new(items.len(), items.as_ptr())
}

fn dict(pairs: &[(i64, i64)]) -> i64 {
    let keys: Vec<i64> = pairs.iter().map(|(key, _)| *key).collect();
    let values: Vec<i64> = pairs.iter().map(|(_, value)| *value).collect();
    hydro_dict_new(pairs.len(), keys.as_ptr(), values.as_ptr())
}

fn module(module: &str, function: &str, args: &[i64]) -> i64 {
    hydro_module_call(
        module.as_ptr(),
        module.len(),
        function.as_ptr(),
        function.len(),
        args.len(),
        args.as_ptr(),
    )
}

fn method(obj: i64, name: &str, args: &[i64]) -> i64 {
    hydro_method(obj, name.as_ptr(), name.len(), args.len(), args.as_ptr())
}

fn function(wrapper: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
    hydro_function_new(wrapper as *const u8, arity, name.as_ptr(), name.len(), 0)
}

fn join(handle: i64) -> i64 {
    module("_thread", "join", &[handle])
}

fn clear_error() {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    hydro_exc_clear();
}

/// Asserts that `str(actual)` equals `expected` in the pickle round-trip.
fn assert_repr(actual: i64, expected: i64) {
    assert_eq!(
        hydro_binop(7, hydro_to_str(actual), hydro_to_str(expected)),
        hydro_true(),
        "unexpected rendering"
    );
}

// -------------------------------------------------------------------------
// Worker wrappers (same `extern "C"` shape as generated code)
// -------------------------------------------------------------------------

/// Releases an `RLock` it does not own; records whether an exception was
/// raised in `out[0]`.
extern "C" fn rlock_release_worker(argv: *const i64) -> i64 {
    // SAFETY: the test passes exactly two arguments.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let _ = method(args[0], "release", &[]);
    let pending = hydro_exc_pending();
    hydro_subscript_set(args[1], int(0), int(pending));
    hydro_exc_clear();
    0
}

/// Waits on `event[0]`, then increments `out[0]`.
extern "C" fn event_worker(argv: *const i64) -> i64 {
    // SAFETY: the test passes exactly two arguments.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let _ = method(args[0], "wait", &[]);
    let current = decode(hydro_subscript(args[1], int(0)));
    hydro_subscript_set(args[1], int(0), int(current + 1));
    0
}

/// Puts `99` into a `queue.Queue`.
extern "C" fn queue_put_worker(argv: *const i64) -> i64 {
    // SAFETY: the test passes exactly one argument.
    let args = unsafe { std::slice::from_raw_parts(argv, 1) };
    let _ = method(args[0], "put", &[int(99)]);
    0
}

/// Waits on an event, gets one queue item and stores it in `out[0]`.
extern "C" fn queue_get_worker(argv: *const i64) -> i64 {
    // SAFETY: the test passes exactly three arguments.
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let _ = method(args[0], "wait", &[]);
    let item = method(args[1], "get", &[]);
    hydro_subscript_set(args[2], int(0), item);
    0
}

/// Function value used by the pickle test.
extern "C" fn add_one_worker(argv: *const i64) -> i64 {
    // SAFETY: the test passes exactly one argument.
    let args = unsafe { std::slice::from_raw_parts(argv, 1) };
    int(decode(args[0]) + 1)
}

/// Returns immediately; the process exits with code 0.
extern "C" fn noop_worker(_argv: *const i64) -> i64 {
    0
}

/// Terminates the child process with code 7.
extern "C" fn exiting_worker(_argv: *const i64) -> i64 {
    let _ = module("os", "_exit", &[int(7)]);
    0
}

/// Sleeps forever; only a signal (or a terminated parent) ends this child.
extern "C" fn sleeping_worker(_argv: *const i64) -> i64 {
    loop {
        let _ = module("time", "sleep", &[hydro_float_new(60.0)]);
    }
}

/// Puts the first argument into the `multiprocessing.Queue` second argument.
extern "C" fn mp_put_worker(argv: *const i64) -> i64 {
    // SAFETY: the test passes exactly two arguments.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let _ = method(args[0], "put", &[args[1]]);
    0
}

// -------------------------------------------------------------------------
// threading
// -------------------------------------------------------------------------

#[test]
fn rlock_is_reentrant_and_owner_checked() {
    run("rlock", || {
        hydro_runtime_init();
        let lock = module("threading", "RLock", &[]);
        assert_eq!(hydro_exc_pending(), 0);

        assert_eq!(method(lock, "acquire", &[]), hydro_true());
        assert_eq!(method(lock, "acquire", &[]), hydro_true());
        assert_eq!(method(lock, "locked", &[]), hydro_true());
        // One release only drops one recursion level.
        assert_eq!(method(lock, "release", &[]), hydro_none());
        assert_eq!(method(lock, "locked", &[]), hydro_true());

        // A different thread must not be able to release it.
        let out = list(&[int(0)]);
        let handle = module(
            "_thread",
            "start_new_thread",
            &[
                function(rlock_release_worker, 2, "rlock_release_worker"),
                list(&[lock, out]),
            ],
        );
        assert_eq!(hydro_exc_pending(), 0);
        assert_eq!(join(handle), hydro_none());
        assert_eq!(decode(hydro_subscript(out, int(0))), 1);

        // The owner releases the last level; a further release is an error.
        assert_eq!(method(lock, "release", &[]), hydro_none());
        assert_eq!(method(lock, "locked", &[]), hydro_false());
        assert_eq!(method(lock, "release", &[]), hydro_none());
        clear_error();
    });
}

#[test]
fn event_and_thread_lifecycle() {
    run("event", || {
        hydro_runtime_init();
        let event = module("threading", "Event", &[]);
        let out = list(&[int(0)]);
        assert_eq!(method(event, "is_set", &[]), hydro_false());
        // A timed wait on an unset event returns False and does not hang.
        assert_eq!(
            method(event, "wait", &[hydro_float_new(0.01)]),
            hydro_false()
        );
        assert_eq!(hydro_exc_pending(), 0);

        let thread = module(
            "threading",
            "Thread",
            &[
                function(event_worker, 2, "event_worker"),
                list(&[event, out]),
            ],
        );
        assert_eq!(hydro_exc_pending(), 0);
        assert_eq!(method(thread, "is_alive", &[]), hydro_false());

        // Joining before the start is an error.
        assert_eq!(method(thread, "join", &[]), hydro_none());
        clear_error();

        assert_eq!(method(thread, "start", &[]), hydro_none());
        // The child is blocked on the event, so it is reliably alive.
        assert_eq!(method(thread, "is_alive", &[]), hydro_true());
        // A thread can only be started once.
        assert_eq!(method(thread, "start", &[]), hydro_none());
        clear_error();

        assert_eq!(method(event, "set", &[]), hydro_none());
        assert_eq!(method(event, "is_set", &[]), hydro_true());
        assert_eq!(method(event, "wait", &[]), hydro_true());
        assert_eq!(method(thread, "join", &[]), hydro_none());
        assert_eq!(method(thread, "is_alive", &[]), hydro_false());
        assert_eq!(decode(hydro_subscript(out, int(0))), 1);
        // Joining twice is idempotent.
        assert_eq!(method(thread, "join", &[]), hydro_none());

        // `threading.get_ident` matches the `_thread` identity and
        // `active_count` never drops below the main thread.
        let ident = module("threading", "get_ident", &[]);
        assert_eq!(ident, module("_thread", "get_ident", &[]));
        assert!(decode(module("threading", "active_count", &[])) >= 1);

        assert_eq!(method(event, "clear", &[]), hydro_none());
        assert_eq!(method(event, "is_set", &[]), hydro_false());
    });
}

// -------------------------------------------------------------------------
// queue.Queue
// -------------------------------------------------------------------------

#[test]
fn queue_is_fifo_and_reports_full_empty() {
    run("queue", || {
        hydro_runtime_init();
        let queue = module("queue", "Queue", &[]);
        assert_eq!(method(queue, "qsize", &[]), int(0));
        assert_eq!(method(queue, "empty", &[]), hydro_true());
        assert_eq!(method(queue, "full", &[]), hydro_false());

        assert_eq!(method(queue, "put", &[int(1)]), hydro_none());
        assert_eq!(method(queue, "put", &[int(2)]), hydro_none());
        assert_eq!(method(queue, "put", &[int(3)]), hydro_none());
        assert_eq!(method(queue, "qsize", &[]), int(3));
        assert_eq!(decode(method(queue, "get", &[])), 1);
        assert_eq!(decode(method(queue, "get", &[])), 2);
        assert_eq!(decode(method(queue, "get", &[])), 3);
        assert_eq!(method(queue, "empty", &[]), hydro_true());

        // Non-blocking operations on an empty/full queue raise RuntimeError.
        assert_eq!(method(queue, "get_nowait", &[]), hydro_none());
        clear_error();

        let bounded = module("queue", "Queue", &[int(1)]);
        assert_eq!(method(bounded, "put_nowait", &[int(1)]), hydro_none());
        assert_eq!(method(bounded, "full", &[]), hydro_true());
        assert_eq!(method(bounded, "put_nowait", &[int(2)]), hydro_none());
        clear_error();
        assert_eq!(decode(method(bounded, "get_nowait", &[])), 1);
        assert_eq!(method(bounded, "empty", &[]), hydro_true());

        // A blocking get is served by a producer thread.
        let blocking = module("queue", "Queue", &[]);
        let producer = module(
            "threading",
            "Thread",
            &[
                function(queue_put_worker, 1, "queue_put_worker"),
                list(&[blocking]),
            ],
        );
        assert_eq!(method(producer, "start", &[]), hydro_none());
        assert_eq!(decode(method(blocking, "get", &[])), 99);
        assert_eq!(method(producer, "join", &[]), hydro_none());
    });
}

#[test]
fn queue_blocking_put_waits_for_capacity() {
    run("queue-put", || {
        hydro_runtime_init();
        let queue = module("queue", "Queue", &[int(1)]);
        let event = module("threading", "Event", &[]);
        let out = list(&[int(0)]);
        assert_eq!(method(queue, "put_nowait", &[int(1)]), hydro_none());

        let consumer = module(
            "threading",
            "Thread",
            &[
                function(queue_get_worker, 3, "queue_get_worker"),
                list(&[event, queue, out]),
            ],
        );
        assert_eq!(method(consumer, "start", &[]), hydro_none());
        assert_eq!(method(event, "set", &[]), hydro_none());
        // The queue is full until the consumer takes item 1, so this put
        // blocks and must be woken by `not_full`.
        assert_eq!(method(queue, "put", &[int(2)]), hydro_none());
        assert_eq!(method(consumer, "join", &[]), hydro_none());
        assert_eq!(decode(hydro_subscript(out, int(0))), 1);
        assert_eq!(decode(method(queue, "get", &[])), 2);

        // A timed-out get reports an empty queue instead of hanging.
        assert_eq!(
            method(queue, "get", &[hydro_true(), hydro_float_new(0.01)]),
            hydro_none()
        );
        clear_error();
    });
}

// -------------------------------------------------------------------------
// pickle
// -------------------------------------------------------------------------

#[test]
fn pickle_roundtrips_containers_and_functions() {
    run("pickle", || {
        hydro_runtime_init();
        let nested = dict(&[
            (text("list"), list(&[int(1), hydro_true(), text("x")])),
            (text("tuple"), tuple(&[int(2), hydro_none()])),
            (text("set"), set(&[int(3), int(4)])),
            (text("range"), hydro_range_new(int(1), int(7), int(2))),
            (text("float"), hydro_float_new(1.5)),
        ]);
        let expected = hydro_to_str(nested);
        let encoded = module("pickle", "dumps", &[nested]);
        assert_eq!(hydro_exc_pending(), 0);
        let decoded = module("pickle", "loads", &[encoded]);
        assert_eq!(hydro_exc_pending(), 0);
        assert_repr(decoded, nested);
        assert_eq!(
            hydro_binop(7, hydro_to_str(decoded), expected),
            hydro_true()
        );

        // Function values round-trip through their wrapper address.
        let encoded = module("pickle", "dumps", &[function(add_one_worker, 1, "add_one")]);
        assert_eq!(hydro_exc_pending(), 0);
        let restored = module("pickle", "loads", &[encoded]);
        assert_eq!(hydro_exc_pending(), 0);
        assert_eq!(decode(hydro_call(restored, 1, [int(41)].as_ptr())), 42);

        // Unsupported values and malformed payloads are reported.
        let lock = module("threading", "Lock", &[]);
        assert_eq!(module("pickle", "dumps", &[lock]), hydro_none());
        assert_eq!(hydro_exc_tag(), 1); // TypeError
        hydro_exc_clear();
        assert_eq!(module("pickle", "loads", &[text("zz")]), hydro_none());
        assert_eq!(hydro_exc_tag(), 0); // ValueError
        hydro_exc_clear();
        assert_eq!(module("pickle", "loads", &[text("0000")]), hydro_none());
        hydro_exc_clear();
    });
}

// -------------------------------------------------------------------------
// multiprocessing / fork
// -------------------------------------------------------------------------

#[test]
fn process_start_join_exitcode_and_start_method() {
    run("process", || {
        hydro_runtime_init();
        let process = module(
            "multiprocessing",
            "Process",
            &[function(noop_worker, 0, "noop_worker")],
        );
        assert_eq!(hydro_exc_pending(), 0);
        assert_eq!(method(process, "is_alive", &[]), hydro_false());
        assert_eq!(method(process, "exitcode", &[]), hydro_none());
        // Joining before the start is an error.
        assert_eq!(method(process, "join", &[]), hydro_none());
        clear_error();

        assert_eq!(method(process, "start", &[]), hydro_none());
        assert_eq!(method(process, "join", &[]), hydro_none());
        assert_eq!(decode(method(process, "exitcode", &[])), 0);
        assert_eq!(method(process, "is_alive", &[]), hydro_false());
        // Idempotent join and a second start is rejected.
        assert_eq!(method(process, "join", &[]), hydro_none());
        assert_eq!(method(process, "start", &[]), hydro_none());
        clear_error();

        let exiting = module(
            "multiprocessing",
            "Process",
            &[function(exiting_worker, 0, "exiting_worker")],
        );
        assert_eq!(method(exiting, "start", &[]), hydro_none());
        assert_eq!(method(exiting, "join", &[]), hydro_none());
        assert_eq!(decode(method(exiting, "exitcode", &[])), 7);

        // Both start methods are selectable; unknown names are rejected.
        assert_eq!(
            hydro_binop(
                7,
                module("multiprocessing", "get_start_method", &[]),
                text("fork")
            ),
            hydro_true()
        );
        assert_eq!(
            module("multiprocessing", "set_start_method", &[text("fork")]),
            hydro_none()
        );
        assert_eq!(
            module("multiprocessing", "set_start_method", &[text("spawn")]),
            hydro_none()
        );
        assert_eq!(hydro_exc_pending(), 0);
        assert_eq!(
            hydro_binop(
                7,
                module("multiprocessing", "get_start_method", &[]),
                text("spawn")
            ),
            hydro_true()
        );
        assert_eq!(
            module("multiprocessing", "set_start_method", &[text("bogus")]),
            hydro_none()
        );
        clear_error();
        // Leave the process-global start method at its default for other
        // tests in this binary.
        assert_eq!(
            module("multiprocessing", "set_start_method", &[text("fork")]),
            hydro_none()
        );
        assert!(decode(module("multiprocessing", "cpu_count", &[])) >= 1);
    });
}

#[test]
fn process_terminate_stops_and_reaps() {
    run("process-terminate", || {
        hydro_runtime_init();
        let process = module(
            "multiprocessing",
            "Process",
            &[function(sleeping_worker, 0, "sleeping_worker")],
        );
        assert_eq!(hydro_exc_pending(), 0);

        // Terminating before the start is an error.
        assert_eq!(method(process, "terminate", &[]), hydro_none());
        assert_eq!(
            hydro_exc_tag(),
            3,
            "unstarted terminate raises RuntimeError"
        );
        clear_error();

        assert_eq!(method(process, "start", &[]), hydro_none());
        assert_eq!(method(process, "terminate", &[]), hydro_none());
        assert_eq!(hydro_exc_pending(), 0);
        // Idempotent: a repeated terminate neither hangs nor fails.
        assert_eq!(method(process, "terminate", &[]), hydro_none());
        assert_eq!(hydro_exc_pending(), 0);

        // `join` after terminate still reaps, so the exit code is the signal.
        assert_eq!(method(process, "join", &[]), hydro_none());
        assert_eq!(hydro_exc_pending(), 0);
        assert_eq!(decode(method(process, "exitcode", &[])), -15);
        assert_eq!(method(process, "is_alive", &[]), hydro_false());

        // A reaped process ignores a further terminate.
        assert_eq!(method(process, "terminate", &[]), hydro_none());
        assert_eq!(hydro_exc_pending(), 0);
    });
}

#[test]
fn multiprocessing_queue_crosses_fork() {
    run("mp-queue", || {
        hydro_runtime_init();
        // The queue must exist before the fork so its descriptors are
        // inherited by the child.
        let queue = module("multiprocessing", "Queue", &[]);
        assert_eq!(hydro_exc_pending(), 0);
        let process = module(
            "multiprocessing",
            "Process",
            &[
                function(mp_put_worker, 2, "mp_put_worker"),
                list(&[queue, int(42)]),
            ],
        );
        assert_eq!(method(process, "start", &[]), hydro_none());
        // Receives the frame written by the child (bounded to avoid a hung
        // test if the child fails before writing).
        let item = method(queue, "get", &[hydro_true(), hydro_float_new(10.0)]);
        assert_eq!(hydro_exc_pending(), 0);
        assert_eq!(decode(item), 42);
        assert_eq!(method(process, "join", &[]), hydro_none());
        assert_eq!(decode(method(process, "exitcode", &[])), 0);

        // A non-blocking read reports an empty queue.
        assert_eq!(method(queue, "get_nowait", &[]), hydro_none());
        clear_error();
    });
}

#[test]
fn os_fork_waitpid_and_exit() {
    run("os-fork", || {
        hydro_runtime_init();
        let pid = module("os", "fork", &[]);
        assert_eq!(hydro_exc_pending(), 0);
        if decode(pid) == 0 {
            // Child: never return into the test body/harness.
            let _ = module("os", "_exit", &[int(7)]);
            unreachable!("os._exit returned");
        }
        let pair = module("os", "waitpid", &[pid, int(0)]);
        assert_eq!(hydro_exc_pending(), 0);
        assert_eq!(decode(hydro_len(pair)), 2);
        assert_eq!(hydro_subscript(pair, int(0)), pid);
        let status = decode(hydro_subscript(pair, int(1)));
        assert_eq!((status >> 8) & 0xff, 7);
    });
}
