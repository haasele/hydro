//! Shutdown and worker-failure tests for `_thread`, `multiprocessing` and
//! `multiprocessing.Pool`.
//!
//! Every test body runs in its own thread under [`TEST_LOCK`] with a watchdog,
//! so a deadlock (a join that never returns, a collection that waits on a dead
//! thread, a pool teardown that blocks) fails the test instead of hanging the
//! harness. All waits are bounded.
//!
//! Child processes are counted through `/proc`, which makes "no leaked worker"
//! observable without runtime-internal access. Those assertions are Linux-only;
//! every other platform skips them and still runs the in-process checks.
//!
//! Unreachable pools and started processes are torn down by their payload
//! `Drop` hooks during collection: an unclosed `Pool` terminates and reaps its
//! workers, and an unjoined started `Process` is reaped, so neither a live
//! child nor a zombie outlives the object.

use std::sync::mpsc::RecvTimeoutError;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hydro_runtime::abi::*;

/// Serialises the tests: the runtime, the thread registry and the process
/// table view are process-global.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Maximum time a test body may run before it is considered hung. The
/// acceptance budget is 30 s per test; the watchdog leaves headroom.
const WATCHDOG: Duration = Duration::from_secs(25);

/// Bound for "wait until the OS reflects this" probes.
const SETTLE: Duration = Duration::from_secs(5);

fn guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Runs `body` under the test lock with a deadlock watchdog.
fn run<F: FnOnce() + Send + 'static>(name: &str, body: F) {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::Builder::new()
        .name(format!("shutdown-{name}"))
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
            panic!("test '{name}' timed out: shutdown path deadlocked")
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

/// Pool tasks are serialised by name, so the callable must be registered even
/// for the `fork` start method (the child resolves it from the copied
/// registry).
fn register(wrapper: extern "C" fn(*const i64) -> i64, name: &str, arity: usize) {
    hydro_register_function(name.as_ptr(), name.len(), wrapper as *const u8, arity);
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

fn clear_error() {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    hydro_exc_clear();
}

/// Sets the process-global start method to `fork`, which is what every pool
/// and process test here needs (a `spawn` child would re-execute the test
/// binary instead of the target).
fn use_fork() {
    assert_eq!(
        module("multiprocessing", "set_start_method", &[text("fork")]),
        hydro_none()
    );
    assert_eq!(hydro_exc_pending(), 0);
}

fn pool(processes: i64) -> i64 {
    let pool = module("multiprocessing", "Pool", &[int(processes)]);
    assert_eq!(hydro_exc_pending(), 0, "Pool({processes}) must not raise");
    pool
}

fn process(target: i64) -> i64 {
    let process = module("multiprocessing", "Process", &[target]);
    assert_eq!(hydro_exc_pending(), 0, "Process() must not raise");
    process
}

// -------------------------------------------------------------------------
// Child-process introspection (`/proc`)
// -------------------------------------------------------------------------

fn own_pid() -> i32 {
    std::process::id() as i32
}

/// Direct children of `parent` as `(pid, state)`. Linux only; empty elsewhere.
#[cfg(target_os = "linux")]
fn child_processes(parent: i32) -> Vec<(i32, char)> {
    scan_children(parent)
}

/// Direct children of `parent` as `(pid, state)`. Linux only; empty elsewhere.
#[cfg(not(target_os = "linux"))]
fn child_processes(parent: i32) -> Vec<(i32, char)> {
    let _ = parent;
    Vec::new()
}

/// `/proc` scan behind [`child_processes`].
#[cfg(target_os = "linux")]
fn scan_children(parent: i32) -> Vec<(i32, char)> {
    let mut children = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return children;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // `comm` may contain spaces and parentheses; everything after the last
        // ')' is `state ppid ...`.
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let mut fields = stat[close + 1..].split_whitespace();
        let state = fields.next().and_then(|field| field.chars().next());
        let ppid = fields.next().and_then(|field| field.parse::<i32>().ok());
        if ppid == Some(parent) {
            children.push((pid, state.unwrap_or('?')));
        }
    }
    children.sort_unstable();
    children
}

fn child_pids(parent: i32) -> Vec<i32> {
    child_processes(parent)
        .into_iter()
        .map(|(pid, _)| pid)
        .collect()
}

/// True when the OS can be asked for the process table.
fn process_tests_supported() -> bool {
    // The `/proc` scan is the only Linux-specific part of these tests.
    #[cfg(target_os = "linux")]
    const SUPPORTED: bool = true;
    #[cfg(not(target_os = "linux"))]
    const SUPPORTED: bool = false;
    SUPPORTED
}

/// Polls `check` until it holds or `limit` elapses.
fn wait_until(limit: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits for the baseline: no child process may be left over from an earlier
/// test in this binary.
fn assert_no_children() {
    assert!(
        wait_until(SETTLE, || child_pids(own_pid()).is_empty()),
        "child processes leaked from an earlier stage: {:?}",
        child_processes(own_pid())
    );
}

/// Overwrites the stack below the caller so a conservative scan is unlikely to
/// find the tagged values of a helper frame (kept `inline(never)` so the
/// scrubbed region really is materialised).
#[inline(never)]
fn scrub_stack() {
    let mut buffer = [0u8; 64 * 1024];
    std::hint::black_box(&mut buffer);
    buffer[0] = 0;
    std::hint::black_box(&buffer);
}

// -------------------------------------------------------------------------
// Worker wrappers (generated code has the same `extern "C"` shape)
// -------------------------------------------------------------------------

/// `worker(lock, counter)`: increments `counter[0]` under `lock`.
extern "C" fn bump_worker(argv: *const i64) -> i64 {
    // SAFETY: the caller passes exactly two arguments.
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let (lock, counter) = (args[0], args[1]);
    method(lock, "acquire", &[]);
    let current = hydro_subscript(counter, int(0));
    let next = hydro_binop(0, current, int(1)); // 0 = add
    hydro_subscript_set(counter, int(0), next);
    method(lock, "release", &[]);
    hydro_none()
}

/// Raises a Hydro exception: the thread boundary must contain it.
extern "C" fn raiser_worker(_argv: *const i64) -> i64 {
    hydro_raise_msg(3, b"worker boom".as_ptr(), 11);
    hydro_none()
}

/// Exits the worker process immediately without publishing a pool result.
extern "C" fn dying_worker(_argv: *const i64) -> i64 {
    let _ = module("os", "_exit", &[int(9)]);
    hydro_none()
}

extern "C" fn square_worker(argv: *const i64) -> i64 {
    // SAFETY: registered with arity 1.
    let args = unsafe { std::slice::from_raw_parts(argv, 1) };
    let n = decode(args[0]);
    int(n * n)
}

extern "C" fn noop_worker(_argv: *const i64) -> i64 {
    hydro_none()
}

extern "C" fn exit_seven_worker(_argv: *const i64) -> i64 {
    let _ = module("os", "_exit", &[int(7)]);
    hydro_none()
}

#[inline(never)]
fn make_unreachable_pool(processes: usize) {
    let pool = pool(processes as i64);
    // The pool is only referenced from this frame, which the caller scrubs.
    std::hint::black_box(pool);
}

#[inline(never)]
fn make_unreachable_process(target: i64) {
    let process = process(target);
    assert_eq!(method(process, "start", &[]), hydro_none());
    std::hint::black_box(process);
}

// -------------------------------------------------------------------------
// Threads
// -------------------------------------------------------------------------

/// Spawned threads that are never joined must finish on their own, must not
/// block a collection while doing so, and must still be joinable later (the
/// registry keeps the finished handle).
#[test]
fn unjoined_finished_threads_are_joined_later() {
    run("thread-unjoined", || {
        hydro_runtime_init();
        let lock = new_lock();
        let counter = list(&[int(0)]);
        let workers = 8;

        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let handle = start(
                function(bump_worker, 2, "bump_worker"),
                list(&[lock, counter]),
            );
            assert_eq!(hydro_exc_pending(), 0, "start_new_thread must not raise");
            handles.push(handle);
        }

        assert!(
            wait_until(SETTLE, || decode(hydro_subscript(counter, int(0)))
                == workers as i64),
            "unjoined workers did not finish: counter = {}",
            decode(hydro_subscript(counter, int(0)))
        );
        // A collection with finished-but-unjoined threads must not wait on
        // them forever.
        assert!(hydro_gc_collect() >= 0);

        for &handle in &handles {
            assert_eq!(join(handle), hydro_none(), "late join must succeed");
            assert_eq!(hydro_exc_pending(), 0);
        }
        // The handle was removed by the join, so a second join is an error.
        assert_eq!(join(handles[0]), hydro_none());
        assert_eq!(hydro_exc_tag(), 3, "re-join raises RuntimeError");
        clear_error();
    });
}

/// Repeated spawn/join cycles must not accumulate runtime state.
#[test]
fn repeated_thread_cycles_stay_bounded() {
    run("thread-cycles", || {
        hydro_runtime_init();
        let lock = new_lock();
        let baseline = hydro_gc_objects();

        for _ in 0..20 {
            let counter = list(&[int(0)]);
            let mut handles = Vec::with_capacity(2);
            for _ in 0..2 {
                let handle = start(
                    function(bump_worker, 2, "bump_worker"),
                    list(&[lock, counter]),
                );
                assert_eq!(hydro_exc_pending(), 0);
                handles.push(handle);
            }
            for handle in handles {
                assert_eq!(join(handle), hydro_none());
                assert_eq!(hydro_exc_pending(), 0);
            }
        }

        assert!(hydro_gc_collect() >= 0);
        let after = hydro_gc_objects();
        assert!(
            after <= baseline + 256,
            "thread cycles leaked heap objects: {baseline} -> {after}"
        );
    });
}

/// A Hydro exception inside a worker thread is contained by the thread boundary:
/// the runtime reports it as uncaught (on stderr, like CPython's `_thread`),
/// `join` succeeds and the caller never sees a pending exception.
#[test]
fn worker_exception_is_contained_by_the_thread_boundary() {
    run("thread-exception", || {
        hydro_runtime_init();
        let depth = hydro_exc_handler_depth();
        let handle = start(function(raiser_worker, 0, "raiser_worker"), list(&[]));
        assert_eq!(hydro_exc_pending(), 0);

        // The traceback printed by the worker is expected test output.
        assert_eq!(join(handle), hydro_none());
        assert_eq!(
            hydro_exc_pending(),
            0,
            "worker exception leaked into the joining thread"
        );
        assert_eq!(
            hydro_exc_handler_depth(),
            depth,
            "the worker's exception handler stack leaked into the caller"
        );

        // The runtime stays usable and the worker registration is gone.
        assert_eq!(decode(hydro_to_int(int(41))), 41);
        assert!(hydro_gc_collect() >= 0);
        let handle = start(function(noop_worker, 0, "noop_worker"), list(&[]));
        assert_eq!(join(handle), hydro_none());
        assert_eq!(hydro_exc_pending(), 0);
    });
}

// -------------------------------------------------------------------------
// Pools
// -------------------------------------------------------------------------

/// `close()` + `join()` reaps every worker: no child process survives.
#[test]
fn close_and_join_reap_every_worker() {
    run("pool-close-join", || {
        if !process_tests_supported() {
            return;
        }
        hydro_runtime_init();
        use_fork();
        register(square_worker, "square_worker", 1);
        assert_no_children();

        let pool = pool(2);
        assert!(
            wait_until(SETTLE, || child_pids(own_pid()).len() == 2),
            "Pool(2) did not start two workers: {:?}",
            child_processes(own_pid())
        );

        let mapped = method(
            pool,
            "map",
            &[
                function(square_worker, 1, "square_worker"),
                list(&[int(1), int(2), int(3), int(4)]),
            ],
        );
        assert_eq!(hydro_exc_pending(), 0, "map must not raise");
        assert_eq!(hydro_len(mapped), int(4));
        // map(2) == 4: the worker really ran the registered callable.
        assert_eq!(decode(hydro_subscript(mapped, int(1))), 4);

        assert_eq!(method(pool, "close", &[]), hydro_none());
        assert_eq!(method(pool, "join", &[]), hydro_none());
        assert_eq!(hydro_exc_pending(), 0);
        assert_no_children();
    });
}

/// `terminate()` without `close()` still stops and reaps every worker.
#[test]
fn terminate_without_close_reaps_every_worker() {
    run("pool-terminate", || {
        if !process_tests_supported() {
            return;
        }
        hydro_runtime_init();
        use_fork();
        assert_no_children();

        let pool = pool(2);
        assert!(wait_until(SETTLE, || child_pids(own_pid()).len() == 2));

        assert_eq!(method(pool, "terminate", &[]), hydro_none());
        // `join` after `terminate` is a documented no-op that still reaps.
        assert_eq!(method(pool, "join", &[]), hydro_none());
        assert_eq!(hydro_exc_pending(), 0);
        assert_no_children();

        // Repeated terminate/join is idempotent and never resurrects a child.
        assert_eq!(method(pool, "terminate", &[]), hydro_none());
        assert_eq!(method(pool, "join", &[]), hydro_none());
        assert_no_children();
    });
}

/// Repeated pool create/terminate cycles leave no worker behind and the heap
/// of terminated pools is reclaimable.
#[test]
fn repeated_pool_cycles_leave_no_children() {
    run("pool-cycles", || {
        if !process_tests_supported() {
            return;
        }
        hydro_runtime_init();
        use_fork();
        assert_no_children();

        for cycle in 0..5i64 {
            let processes = 1 + cycle % 2;
            let pool = pool(processes);
            assert!(
                !child_pids(own_pid()).is_empty(),
                "cycle {cycle}: pool created no worker"
            );
            assert_eq!(method(pool, "terminate", &[]), hydro_none());
            assert_eq!(method(pool, "join", &[]), hydro_none());
            assert!(
                wait_until(SETTLE, || child_pids(own_pid()).is_empty()),
                "cycle {cycle} leaked a worker: {:?}",
                child_processes(own_pid())
            );
            // The terminated pool's queues become garbage; a collection must
            // handle them without touching a reaped worker.
            assert!(hydro_gc_collect() >= 0);
            assert_eq!(hydro_exc_pending(), 0);
        }
    });
}

/// A worker that dies without publishing a result must surface as a
/// `RuntimeError` instead of hanging the caller, and the pool must still be
/// terminable.
#[test]
fn pool_worker_death_is_reported_without_hanging() {
    run("pool-worker-death", || {
        if !process_tests_supported() {
            return;
        }
        hydro_runtime_init();
        use_fork();
        register(dying_worker, "dying_worker", 0);
        assert_no_children();

        let pool = pool(1);
        assert!(wait_until(SETTLE, || child_pids(own_pid()).len() == 1));

        let mapped = method(
            pool,
            "map",
            &[function(dying_worker, 0, "dying_worker"), list(&[int(1)])],
        );
        assert_eq!(mapped, hydro_none());
        assert_eq!(hydro_exc_pending(), 1, "a dead worker must raise");
        assert_eq!(hydro_exc_tag(), 3, "a dead worker raises RuntimeError");
        clear_error();

        // The dead worker was reaped by the liveness probe; teardown handles
        // the missing pid and leaves no process behind.
        assert_eq!(method(pool, "terminate", &[]), hydro_none());
        assert_eq!(method(pool, "join", &[]), hydro_none());
        assert_eq!(hydro_exc_pending(), 0);
        assert_no_children();
    });
}

/// An unclosed `Pool` that becomes unreachable is torn down by
/// `PoolState::drop`: the collector's sweep terminates (`SIGTERM`) and reaps
/// every worker even though `close`, `join` and `terminate` never ran.
#[test]
fn unreachable_pool_is_reaped_by_the_collector() {
    run("pool-unreachable", || {
        if !process_tests_supported() {
            return;
        }
        hydro_runtime_init();
        use_fork();
        assert_no_children();

        make_unreachable_pool(2);
        scrub_stack();
        for _ in 0..3 {
            assert!(hydro_gc_collect() >= 0);
        }
        assert!(
            wait_until(SETTLE, || child_pids(own_pid()).is_empty()),
            "an unreachable, unclosed Pool left workers running: {:?}",
            child_processes(own_pid())
        );
    });
}

// -------------------------------------------------------------------------
// Processes
// -------------------------------------------------------------------------

/// Process exit codes and reaping: a normal return exits 0, an uncaught
/// exception exits 1, `os._exit(7)` is propagated, and a joined process leaves
/// no child entry behind.
#[test]
fn process_exit_codes_and_reaping() {
    run("process-exit", || {
        if !process_tests_supported() {
            return;
        }
        hydro_runtime_init();
        use_fork();
        assert_no_children();

        let normal = process(function(noop_worker, 0, "noop_worker"));
        assert_eq!(method(normal, "start", &[]), hydro_none());
        assert_eq!(method(normal, "join", &[]), hydro_none());
        assert_eq!(decode(method(normal, "exitcode", &[])), 0);
        assert_no_children();

        let exiting = process(function(exit_seven_worker, 0, "exit_seven_worker"));
        assert_eq!(method(exiting, "start", &[]), hydro_none());
        assert_eq!(method(exiting, "join", &[]), hydro_none());
        assert_eq!(decode(method(exiting, "exitcode", &[])), 7);
        assert_no_children();

        // A child whose target raises reports it as uncaught (the traceback on
        // stderr is expected) and exits with code 1.
        let failing = process(function(raiser_worker, 0, "raiser_worker"));
        assert_eq!(method(failing, "start", &[]), hydro_none());
        assert_eq!(method(failing, "join", &[]), hydro_none());
        assert_eq!(decode(method(failing, "exitcode", &[])), 1);
        assert_eq!(hydro_exc_pending(), 0, "the child's exception is its own");
        assert_no_children();
    });
}

/// A started `Process` that is never joined is reaped when its payload is
/// dropped: `ProcessState::drop` stops the child if needed and waits it out, so
/// it never survives as a zombie.
#[test]
fn unjoined_started_process_is_reaped_by_the_runtime() {
    run("process-unjoined", || {
        if !process_tests_supported() {
            return;
        }
        hydro_runtime_init();
        use_fork();
        assert_no_children();

        make_unreachable_process(function(exit_seven_worker, 0, "exit_seven_worker"));
        scrub_stack();
        // The child exits immediately but stays visible as a zombie until it
        // is reaped; wait for that state instead of sleeping blindly.
        let became_zombie = wait_until(SETTLE, || {
            child_processes(own_pid())
                .iter()
                .any(|&(_, state)| state == 'Z')
        });
        assert!(
            became_zombie,
            "the child never became a zombie: {:?}",
            child_processes(own_pid())
        );
        assert!(hydro_gc_collect() >= 0);
        assert!(
            wait_until(SETTLE, || child_pids(own_pid()).is_empty()),
            "an unjoined started Process was left unreaped: {:?}",
            child_processes(own_pid())
        );
    });
}

/// Repeated unreachable pools and started processes leave no child behind:
/// every collector pass both stops and reaps what the previous cycle created.
#[test]
fn repeated_unreachable_objects_leave_no_children() {
    run("unreachable-cycles", || {
        if !process_tests_supported() {
            return;
        }
        hydro_runtime_init();
        use_fork();
        assert_no_children();

        for cycle in 0..3i64 {
            let processes = (1 + cycle % 2) as usize;
            make_unreachable_pool(processes);
            assert!(
                wait_until(SETTLE, || child_pids(own_pid()).len() == processes),
                "cycle {cycle}: Pool({processes}) did not start its workers: {:?}",
                child_processes(own_pid())
            );
            scrub_stack();
            for _ in 0..3 {
                assert!(hydro_gc_collect() >= 0);
            }
            assert!(
                wait_until(SETTLE, || child_pids(own_pid()).is_empty()),
                "cycle {cycle}: an unreachable Pool leaked a worker: {:?}",
                child_processes(own_pid())
            );

            make_unreachable_process(function(noop_worker, 0, "noop_worker"));
            scrub_stack();
            for _ in 0..3 {
                assert!(hydro_gc_collect() >= 0);
            }
            assert!(
                wait_until(SETTLE, || child_pids(own_pid()).is_empty()),
                "cycle {cycle}: an unreachable Process leaked its child: {:?}",
                child_processes(own_pid())
            );
            assert_eq!(hydro_exc_pending(), 0);
        }
        assert_no_children();
    });
}
