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

use pyx_runtime::abi::*;

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
    pyx_to_int(value) >> 3
}

fn text(value: &str) -> i64 {
    pyx_str_new(value.as_ptr(), value.len())
}

fn list(items: &[i64]) -> i64 {
    pyx_list_new(items.len(), items.as_ptr())
}

fn tuple(items: &[i64]) -> i64 {
    pyx_tuple_new(items.len(), items.as_ptr())
}

fn set(items: &[i64]) -> i64 {
    pyx_set_new(items.len(), items.as_ptr())
}

fn dict(pairs: &[(i64, i64)]) -> i64 {
    let keys: Vec<i64> = pairs.iter().map(|(key, _)| *key).collect();
    let values: Vec<i64> = pairs.iter().map(|(_, value)| *value).collect();
    pyx_dict_new(pairs.len(), keys.as_ptr(), values.as_ptr())
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
    pyx_function_new(wrapper as *const u8, arity, name.as_ptr(), name.len(), 0)
}

fn join(handle: i64) -> i64 {
    module("_thread", "join", &[handle])
}

fn clear_error() {
    assert_eq!(pyx_exc_pending(), 1, "expected a pending exception");
    pyx_exc_clear();
}

/// Asserts that `str(actual)` equals `expected` in the pickle round-trip.
fn assert_repr(actual: i64, expected: i64) {
    assert_eq!(
        pyx_binop(7, pyx_to_str(actual), pyx_to_str(expected)),
        pyx_true(),
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
    let pending = pyx_exc_pending();
    pyx_subscript_set(args[1], int(0), int(pending));
    pyx_exc_clear();
    0
}

/// Waits on `event[0]`, then increments `out[0]`.
extern "C" fn event_worker(argv: *const i64) -> i64 {
    // SAFETY: the test passes exactly two arguments.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let _ = method(args[0], "wait", &[]);
    let current = decode(pyx_subscript(args[1], int(0)));
    pyx_subscript_set(args[1], int(0), int(current + 1));
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
    pyx_subscript_set(args[2], int(0), item);
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
        let _ = module("time", "sleep", &[pyx_float_new(60.0)]);
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
        pyx_runtime_init();
        let lock = module("threading", "RLock", &[]);
        assert_eq!(pyx_exc_pending(), 0);

        assert_eq!(method(lock, "acquire", &[]), pyx_true());
        assert_eq!(method(lock, "acquire", &[]), pyx_true());
        assert_eq!(method(lock, "locked", &[]), pyx_true());
        // One release only drops one recursion level.
        assert_eq!(method(lock, "release", &[]), pyx_none());
        assert_eq!(method(lock, "locked", &[]), pyx_true());

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
        assert_eq!(pyx_exc_pending(), 0);
        assert_eq!(join(handle), pyx_none());
        assert_eq!(decode(pyx_subscript(out, int(0))), 1);

        // The owner releases the last level; a further release is an error.
        assert_eq!(method(lock, "release", &[]), pyx_none());
        assert_eq!(method(lock, "locked", &[]), pyx_false());
        assert_eq!(method(lock, "release", &[]), pyx_none());
        clear_error();
    });
}

#[test]
fn event_and_thread_lifecycle() {
    run("event", || {
        pyx_runtime_init();
        let event = module("threading", "Event", &[]);
        let out = list(&[int(0)]);
        assert_eq!(method(event, "is_set", &[]), pyx_false());
        // A timed wait on an unset event returns False and does not hang.
        assert_eq!(method(event, "wait", &[pyx_float_new(0.01)]), pyx_false());
        assert_eq!(pyx_exc_pending(), 0);

        let thread = module(
            "threading",
            "Thread",
            &[
                function(event_worker, 2, "event_worker"),
                list(&[event, out]),
            ],
        );
        assert_eq!(pyx_exc_pending(), 0);
        assert_eq!(method(thread, "is_alive", &[]), pyx_false());

        // Joining before the start is an error.
        assert_eq!(method(thread, "join", &[]), pyx_none());
        clear_error();

        assert_eq!(method(thread, "start", &[]), pyx_none());
        // The child is blocked on the event, so it is reliably alive.
        assert_eq!(method(thread, "is_alive", &[]), pyx_true());
        // A thread can only be started once.
        assert_eq!(method(thread, "start", &[]), pyx_none());
        clear_error();

        assert_eq!(method(event, "set", &[]), pyx_none());
        assert_eq!(method(event, "is_set", &[]), pyx_true());
        assert_eq!(method(event, "wait", &[]), pyx_true());
        assert_eq!(method(thread, "join", &[]), pyx_none());
        assert_eq!(method(thread, "is_alive", &[]), pyx_false());
        assert_eq!(decode(pyx_subscript(out, int(0))), 1);
        // Joining twice is idempotent.
        assert_eq!(method(thread, "join", &[]), pyx_none());

        // `threading.get_ident` matches the `_thread` identity and
        // `active_count` never drops below the main thread.
        let ident = module("threading", "get_ident", &[]);
        assert_eq!(ident, module("_thread", "get_ident", &[]));
        assert!(decode(module("threading", "active_count", &[])) >= 1);

        assert_eq!(method(event, "clear", &[]), pyx_none());
        assert_eq!(method(event, "is_set", &[]), pyx_false());
    });
}

// -------------------------------------------------------------------------
// queue.Queue
// -------------------------------------------------------------------------

#[test]
fn queue_is_fifo_and_reports_full_empty() {
    run("queue", || {
        pyx_runtime_init();
        let queue = module("queue", "Queue", &[]);
        assert_eq!(method(queue, "qsize", &[]), int(0));
        assert_eq!(method(queue, "empty", &[]), pyx_true());
        assert_eq!(method(queue, "full", &[]), pyx_false());

        assert_eq!(method(queue, "put", &[int(1)]), pyx_none());
        assert_eq!(method(queue, "put", &[int(2)]), pyx_none());
        assert_eq!(method(queue, "put", &[int(3)]), pyx_none());
        assert_eq!(method(queue, "qsize", &[]), int(3));
        assert_eq!(decode(method(queue, "get", &[])), 1);
        assert_eq!(decode(method(queue, "get", &[])), 2);
        assert_eq!(decode(method(queue, "get", &[])), 3);
        assert_eq!(method(queue, "empty", &[]), pyx_true());

        // Non-blocking operations on an empty/full queue raise RuntimeError.
        assert_eq!(method(queue, "get_nowait", &[]), pyx_none());
        clear_error();

        let bounded = module("queue", "Queue", &[int(1)]);
        assert_eq!(method(bounded, "put_nowait", &[int(1)]), pyx_none());
        assert_eq!(method(bounded, "full", &[]), pyx_true());
        assert_eq!(method(bounded, "put_nowait", &[int(2)]), pyx_none());
        clear_error();
        assert_eq!(decode(method(bounded, "get_nowait", &[])), 1);
        assert_eq!(method(bounded, "empty", &[]), pyx_true());

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
        assert_eq!(method(producer, "start", &[]), pyx_none());
        assert_eq!(decode(method(blocking, "get", &[])), 99);
        assert_eq!(method(producer, "join", &[]), pyx_none());
    });
}

#[test]
fn queue_blocking_put_waits_for_capacity() {
    run("queue-put", || {
        pyx_runtime_init();
        let queue = module("queue", "Queue", &[int(1)]);
        let event = module("threading", "Event", &[]);
        let out = list(&[int(0)]);
        assert_eq!(method(queue, "put_nowait", &[int(1)]), pyx_none());

        let consumer = module(
            "threading",
            "Thread",
            &[
                function(queue_get_worker, 3, "queue_get_worker"),
                list(&[event, queue, out]),
            ],
        );
        assert_eq!(method(consumer, "start", &[]), pyx_none());
        assert_eq!(method(event, "set", &[]), pyx_none());
        // The queue is full until the consumer takes item 1, so this put
        // blocks and must be woken by `not_full`.
        assert_eq!(method(queue, "put", &[int(2)]), pyx_none());
        assert_eq!(method(consumer, "join", &[]), pyx_none());
        assert_eq!(decode(pyx_subscript(out, int(0))), 1);
        assert_eq!(decode(method(queue, "get", &[])), 2);

        // A timed-out get reports an empty queue instead of hanging.
        assert_eq!(
            method(queue, "get", &[pyx_true(), pyx_float_new(0.01)]),
            pyx_none()
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
        pyx_runtime_init();
        let nested = dict(&[
            (text("list"), list(&[int(1), pyx_true(), text("x")])),
            (text("tuple"), tuple(&[int(2), pyx_none()])),
            (text("set"), set(&[int(3), int(4)])),
            (text("range"), pyx_range_new(int(1), int(7), int(2))),
            (text("float"), pyx_float_new(1.5)),
        ]);
        let expected = pyx_to_str(nested);
        let encoded = module("pickle", "dumps", &[nested]);
        assert_eq!(pyx_exc_pending(), 0);
        let decoded = module("pickle", "loads", &[encoded]);
        assert_eq!(pyx_exc_pending(), 0);
        assert_repr(decoded, nested);
        assert_eq!(pyx_binop(7, pyx_to_str(decoded), expected), pyx_true());

        // Function values round-trip through their wrapper address.
        let encoded = module("pickle", "dumps", &[function(add_one_worker, 1, "add_one")]);
        assert_eq!(pyx_exc_pending(), 0);
        let restored = module("pickle", "loads", &[encoded]);
        assert_eq!(pyx_exc_pending(), 0);
        assert_eq!(decode(pyx_call(restored, 1, [int(41)].as_ptr())), 42);

        // Unsupported values and malformed payloads are reported.
        let lock = module("threading", "Lock", &[]);
        assert_eq!(module("pickle", "dumps", &[lock]), pyx_none());
        assert_eq!(pyx_exc_tag(), 1); // TypeError
        pyx_exc_clear();
        assert_eq!(module("pickle", "loads", &[text("zz")]), pyx_none());
        assert_eq!(pyx_exc_tag(), 0); // ValueError
        pyx_exc_clear();
        assert_eq!(module("pickle", "loads", &[text("0000")]), pyx_none());
        pyx_exc_clear();
    });
}

// -------------------------------------------------------------------------
// multiprocessing / fork
// -------------------------------------------------------------------------

#[test]
fn process_start_join_exitcode_and_start_method() {
    run("process", || {
        pyx_runtime_init();
        let process = module(
            "multiprocessing",
            "Process",
            &[function(noop_worker, 0, "noop_worker")],
        );
        assert_eq!(pyx_exc_pending(), 0);
        assert_eq!(method(process, "is_alive", &[]), pyx_false());
        assert_eq!(method(process, "exitcode", &[]), pyx_none());
        // Joining before the start is an error.
        assert_eq!(method(process, "join", &[]), pyx_none());
        clear_error();

        assert_eq!(method(process, "start", &[]), pyx_none());
        assert_eq!(method(process, "join", &[]), pyx_none());
        assert_eq!(decode(method(process, "exitcode", &[])), 0);
        assert_eq!(method(process, "is_alive", &[]), pyx_false());
        // Idempotent join and a second start is rejected.
        assert_eq!(method(process, "join", &[]), pyx_none());
        assert_eq!(method(process, "start", &[]), pyx_none());
        clear_error();

        let exiting = module(
            "multiprocessing",
            "Process",
            &[function(exiting_worker, 0, "exiting_worker")],
        );
        assert_eq!(method(exiting, "start", &[]), pyx_none());
        assert_eq!(method(exiting, "join", &[]), pyx_none());
        assert_eq!(decode(method(exiting, "exitcode", &[])), 7);

        // Both start methods are selectable; unknown names are rejected.
        assert_eq!(
            pyx_binop(
                7,
                module("multiprocessing", "get_start_method", &[]),
                text("fork")
            ),
            pyx_true()
        );
        assert_eq!(
            module("multiprocessing", "set_start_method", &[text("fork")]),
            pyx_none()
        );
        assert_eq!(
            module("multiprocessing", "set_start_method", &[text("spawn")]),
            pyx_none()
        );
        assert_eq!(pyx_exc_pending(), 0);
        assert_eq!(
            pyx_binop(
                7,
                module("multiprocessing", "get_start_method", &[]),
                text("spawn")
            ),
            pyx_true()
        );
        assert_eq!(
            module("multiprocessing", "set_start_method", &[text("bogus")]),
            pyx_none()
        );
        clear_error();
        // Leave the process-global start method at its default for other
        // tests in this binary.
        assert_eq!(
            module("multiprocessing", "set_start_method", &[text("fork")]),
            pyx_none()
        );
        assert!(decode(module("multiprocessing", "cpu_count", &[])) >= 1);
    });
}

#[test]
fn process_terminate_stops_and_reaps() {
    run("process-terminate", || {
        pyx_runtime_init();
        let process = module(
            "multiprocessing",
            "Process",
            &[function(sleeping_worker, 0, "sleeping_worker")],
        );
        assert_eq!(pyx_exc_pending(), 0);

        // Terminating before the start is an error.
        assert_eq!(method(process, "terminate", &[]), pyx_none());
        assert_eq!(pyx_exc_tag(), 3, "unstarted terminate raises RuntimeError");
        clear_error();

        assert_eq!(method(process, "start", &[]), pyx_none());
        assert_eq!(method(process, "terminate", &[]), pyx_none());
        assert_eq!(pyx_exc_pending(), 0);
        // Idempotent: a repeated terminate neither hangs nor fails.
        assert_eq!(method(process, "terminate", &[]), pyx_none());
        assert_eq!(pyx_exc_pending(), 0);

        // `join` after terminate still reaps, so the exit code is the signal.
        assert_eq!(method(process, "join", &[]), pyx_none());
        assert_eq!(pyx_exc_pending(), 0);
        assert_eq!(decode(method(process, "exitcode", &[])), -15);
        assert_eq!(method(process, "is_alive", &[]), pyx_false());

        // A reaped process ignores a further terminate.
        assert_eq!(method(process, "terminate", &[]), pyx_none());
        assert_eq!(pyx_exc_pending(), 0);
    });
}

#[test]
fn multiprocessing_queue_crosses_fork() {
    run("mp-queue", || {
        pyx_runtime_init();
        // The queue must exist before the fork so its descriptors are
        // inherited by the child.
        let queue = module("multiprocessing", "Queue", &[]);
        assert_eq!(pyx_exc_pending(), 0);
        let process = module(
            "multiprocessing",
            "Process",
            &[
                function(mp_put_worker, 2, "mp_put_worker"),
                list(&[queue, int(42)]),
            ],
        );
        assert_eq!(method(process, "start", &[]), pyx_none());
        // Receives the frame written by the child (bounded to avoid a hung
        // test if the child fails before writing).
        let item = method(queue, "get", &[pyx_true(), pyx_float_new(10.0)]);
        assert_eq!(pyx_exc_pending(), 0);
        assert_eq!(decode(item), 42);
        assert_eq!(method(process, "join", &[]), pyx_none());
        assert_eq!(decode(method(process, "exitcode", &[])), 0);

        // A non-blocking read reports an empty queue.
        assert_eq!(method(queue, "get_nowait", &[]), pyx_none());
        clear_error();
    });
}

#[test]
fn os_fork_waitpid_and_exit() {
    run("os-fork", || {
        pyx_runtime_init();
        let pid = module("os", "fork", &[]);
        assert_eq!(pyx_exc_pending(), 0);
        if decode(pid) == 0 {
            // Child: never return into the test body/harness.
            let _ = module("os", "_exit", &[int(7)]);
            unreachable!("os._exit returned");
        }
        let pair = module("os", "waitpid", &[pid, int(0)]);
        assert_eq!(pyx_exc_pending(), 0);
        assert_eq!(decode(pyx_len(pair)), 2);
        assert_eq!(pyx_subscript(pair, int(0)), pid);
        let status = decode(pyx_subscript(pair, int(1)));
        assert_eq!((status >> 8) & 0xff, 7);
    });
}
