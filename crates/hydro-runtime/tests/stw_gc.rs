//! Stop-the-world GC integration tests (Phase 2.13).
//!
//! Every test spawns real `_thread` threads that allocate while the main
//! thread forces `hydro_gc_collect()`. The process-global heap and thread
//! registry are serialised with `TEST_LOCK`, and every body runs under a
//! watchdog so a collector deadlock fails the test instead of hanging the
//! test binary forever.
//!
//! Values are tagged `i64`s; `hydro_to_int` decodes tagged integers.
//!
//! MIXED MODE ONLY: the worker closures and the main test body hold tagged
//! values in unregistered Rust locals/registers. Strict precise-root coverage
//! (the only mode that disables conservative scanning) lives in
//! `tests/precise_gc.rs`, which roots every value explicitly.

use std::panic::AssertUnwindSafe;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use hydro_runtime::abi::*;

/// Serialises the tests in this binary: the heap and thread registry are
/// process-global.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Runs `body` on a helper thread and fails if it does not finish in time.
fn run_with_timeout<F>(body: F)
where
    F: FnOnce() + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let helper = std::thread::Builder::new()
        .name("stw-test-body".to_string())
        .spawn(move || {
            let result = std::panic::catch_unwind(AssertUnwindSafe(body));
            let _ = tx.send(result.is_ok());
        })
        .expect("spawn test helper");
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(true) => {
            let _ = helper.join();
        }
        Ok(false) => {
            let _ = helper.join();
            panic!("stop-the-world test body panicked");
        }
        Err(_) => panic!("stop-the-world test timed out (collector deadlock)"),
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

fn new_lock() -> i64 {
    module("_thread", "allocate_lock", &[])
}

fn start(func: i64, args: i64) -> i64 {
    module("_thread", "start_new_thread", &[func, args])
}

fn join(handle: i64) -> i64 {
    module("_thread", "join", &[handle])
}

// -------------------------------------------------------------------------
// Workers (the same `extern "C"` shape generated functions have)
// -------------------------------------------------------------------------

/// `worker(lock, results, iterations)`: per iteration allocates one string
/// that is appended to the shared list under `lock` and one string that is
/// immediately garbage.
extern "C" fn alloc_worker(argv: *const i64) -> i64 {
    // SAFETY: the caller passes exactly three arguments.
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let (lock, results, iterations) = (args[0], args[1], args[2]);
    let count = decode(iterations);
    let mut i = 0;
    while i < count {
        let kept = hydro_str_new(b"payload".as_ptr(), 7);
        let _ = hydro_str_new(b"garbage-garbage".as_ptr(), 15);
        method(lock, "acquire", &[]);
        hydro_list_append(results, kept);
        method(lock, "release", &[]);
        i += 1;
    }
    hydro_none()
}

/// `worker(seconds, flag)`: sleeps (parked for the collector), then sets
/// `flag[0] = 1`.
extern "C" fn sleep_worker(argv: *const i64) -> i64 {
    // SAFETY: the caller passes exactly two arguments.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let (seconds, flag) = (args[0], args[1]);
    module("time", "sleep", &[seconds]);
    hydro_subscript_set(flag, int(0), int(1));
    hydro_none()
}

/// `worker(seconds, flag)`: raises an exception (whose object only the
/// pending mirror keeps alive), sleeps while the parent collects, then clears
/// the exception before exiting so no traceback is printed.
extern "C" fn raise_worker(argv: *const i64) -> i64 {
    // SAFETY: the caller passes exactly two arguments.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let (seconds, flag) = (args[0], args[1]);
    // 3 = RuntimeError.
    hydro_raise_msg(3, b"boom".as_ptr(), 4);
    module("time", "sleep", &[seconds]);
    hydro_exc_clear();
    hydro_subscript_set(flag, int(0), int(1));
    hydro_none()
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

/// Four threads allocate strings and append to a shared list under a user
/// lock while the main thread repeatedly stops the world. All shared data
/// must survive and every thread must join.
#[test]
fn four_allocating_threads_survive_repeated_collections() {
    run_with_timeout(|| {
        let _guard = guard();
        hydro_runtime_init();

        let lock = new_lock();
        let results = list(&[]);
        let worker = function(alloc_worker, 3, "alloc_worker");

        // Shared data owned by the main thread only.
        let keeper = text("main-keeper");
        let keeper_list = list(&[keeper]);

        let mut handles = Vec::new();
        for _ in 0..4 {
            let handle = start(worker, list(&[lock, results, int(500)]));
            assert_eq!(hydro_exc_pending(), 0);
            handles.push(handle);
        }

        // Stop the world repeatedly while the workers are allocating.
        for _ in 0..64 {
            hydro_gc_collect();
            module("time", "sleep", &[hydro_float_new(0.0002)]);
            assert_eq!(hydro_exc_pending(), 0);
            // Main-thread roots must survive every collection.
            assert_eq!(hydro_subscript(keeper_list, int(0)), keeper);
        }

        for handle in handles {
            assert_eq!(join(handle), hydro_none());
            assert_eq!(hydro_exc_pending(), 0);
        }

        // 4 workers x 500 iterations, no lost append.
        assert_eq!(decode(hydro_len(results)), 2000);
        assert_eq!(hydro_subscript(keeper_list, int(0)), keeper);
        assert_eq!(
            hydro_binop(7, hydro_subscript(keeper_list, int(0)), keeper),
            hydro_true()
        );
    });
}

/// A child that sleeps is parked for the collector; a collection started by
/// the main thread must complete while the child blocks, and the join must
/// not deadlock afterwards.
#[test]
fn collection_during_blocking_sleep_and_join() {
    run_with_timeout(|| {
        let _guard = guard();
        hydro_runtime_init();

        let keeper = text("sleep-keeper");
        let flag = list(&[int(0)]);
        let handle = start(
            function(sleep_worker, 2, "sleep_worker"),
            list(&[hydro_float_new(0.15), flag]),
        );
        assert_eq!(hydro_exc_pending(), 0);

        // The child is inside `time.sleep` and therefore parked.
        for _ in 0..16 {
            hydro_gc_collect();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(hydro_binop(7, keeper, text("sleep-keeper")), hydro_true());

        assert_eq!(join(handle), hydro_none());
        assert_eq!(hydro_exc_pending(), 0);
        assert_eq!(decode(hydro_subscript(flag, int(0))), 1);
    });
}

/// A child's pending exception object is only reachable through its pending
/// mirror while the child is parked. Collecting during its sleep must keep
/// that object alive (the subsequent clear and exit would otherwise touch
/// freed memory).
#[test]
fn pending_exception_mirror_survives_collection() {
    run_with_timeout(|| {
        let _guard = guard();
        hydro_runtime_init();

        let flag = list(&[int(0)]);
        let handle = start(
            function(raise_worker, 2, "raise_worker"),
            list(&[hydro_float_new(0.15), flag]),
        );
        assert_eq!(hydro_exc_pending(), 0);

        for _ in 0..16 {
            hydro_gc_collect();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(hydro_exc_pending(), 0, "child exceptions stay thread-local");

        assert_eq!(join(handle), hydro_none());
        assert_eq!(hydro_exc_pending(), 0);
        assert_eq!(decode(hydro_subscript(flag, int(0))), 1);
    });
}
