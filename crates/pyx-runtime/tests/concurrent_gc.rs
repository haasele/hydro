//! Integration tests for the incremental concurrent collector (P2-20a/P5-04).
//!
//! The collector marks while the other threads keep mutating shared lists,
//! dicts and queues under a user lock; only the initial root snapshot and the
//! final rescan stop the world. The white objects are then partitioned out of
//! the registry and freed while the mutators run again. These tests drive that
//! interleaving: several worker threads mutate shared containers and allocate
//! garbage while the main thread repeatedly calls `pyx_gc_collect()`, and they
//! verify both data integrity and that a mark in progress never sweeps a live
//! object.
//!
//! The runtime is process-global, so every test body runs in a worker thread
//! under `TEST_LOCK` with a watchdog: a deadlock in the collector or in a
//! parked thread fails the test after a timeout instead of hanging the
//! harness. `PYX_GC_THRESHOLD` is set per test (the collector re-reads the
//! configuration on `pyx_runtime_init`).
//!
//! MIXED MODE ONLY: the worker closures keep their tagged values in plain
//! Rust locals and the `argv` slice, which are not precise shadow frames.
//! These tests must run with `PYX_GC_PRECISE` unset (the default); strict
//! precise-root coverage lives in `tests/precise_gc.rs`.

use std::sync::mpsc::RecvTimeoutError;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pyx_runtime::abi::*;

/// Serialises the tests in this binary: the runtime is a process-global.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Maximum time a single test body may run before it is considered deadlocked.
const WATCHDOG: Duration = Duration::from_secs(30);

/// Maximum time the workers may take to finish their rounds.
const WORKER_DEADLINE: Duration = Duration::from_secs(20);

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
            // The body panicked (its sender was dropped).
            handle.join().expect("test body panicked");
        }
        Err(RecvTimeoutError::Timeout) => {
            panic!("test '{name}' timed out: concurrent collection deadlocked")
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

fn dict() -> i64 {
    pyx_dict_new(0, std::ptr::null(), std::ptr::null())
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
// Worker
// -------------------------------------------------------------------------

/// `worker(id, lock, done, shared_list, shared_dict, queue, rounds)`.
///
/// Under `lock` the worker appends to `shared_list`, writes `shared_dict[id]`
/// and round-trips one queue item, then allocates short-lived strings and a
/// list outside the lock. `done[id]` is set to `1` when it finishes and to
/// `2` if the queue round-trip ever returned the wrong item.
extern "C" fn mutator_worker(argv: *const i64) -> i64 {
    // SAFETY: the caller passes exactly seven arguments (the wrapper arity is
    // validated by `_thread.start_new_thread`).
    let args = unsafe { std::slice::from_raw_parts(argv, 7) };
    let (id, lock, done, shared_list, shared_dict, queue, rounds) = (
        args[0], args[1], args[2], args[3], args[4], args[5], args[6],
    );
    let iterations = decode(rounds);

    let mut i = 0i64;
    while i < iterations {
        method(lock, "acquire", &[]);
        pyx_list_append(shared_list, int(i));
        pyx_subscript_set(shared_dict, id, int(i));
        let item = pyx_str_new(b"queue-item".as_ptr(), 10);
        let _ = method(queue, "put_nowait", &[item]);
        let taken = method(queue, "get_nowait", &[]);
        method(lock, "release", &[]);

        // The put/get pair ran inside the critical section, so this thread
        // must receive its own item back even with other workers running.
        if pyx_binop(7, taken, item) != pyx_true() {
            method(lock, "acquire", &[]);
            pyx_subscript_set(done, id, int(2));
            method(lock, "release", &[]);
            return pyx_none();
        }

        // Short-lived heap objects allocated while a mark may be in flight:
        // they are born grey and must never be swept before their trace.
        let junk = pyx_str_new(b"garbage-during-marking".as_ptr(), 22);
        let _holder = pyx_list_new(1, &junk as *const i64);

        i += 1;
    }

    method(lock, "acquire", &[]);
    pyx_subscript_set(done, id, int(1));
    method(lock, "release", &[]);
    pyx_none()
}

// -------------------------------------------------------------------------
// Shared driver
// -------------------------------------------------------------------------

/// Starts `workers` mutators and returns their handles plus the shared state.
fn start_workers(
    workers: usize,
    rounds: i64,
    lock: i64,
    done: i64,
    shared_list: i64,
    shared_dict: i64,
    queue: i64,
) -> Vec<i64> {
    let mut handles = Vec::with_capacity(workers);
    for id in 0..workers {
        let handle = start(
            function(mutator_worker, 7, "mutator_worker"),
            list(&[
                int(id as i64),
                lock,
                done,
                shared_list,
                shared_dict,
                queue,
                int(rounds),
            ]),
        );
        assert_eq!(pyx_exc_pending(), 0, "failed to start worker {id}");
        handles.push(handle);
    }
    handles
}

/// Collects while the workers run, allocating main-thread garbage between
/// collections, and returns the number of collections performed.
fn collect_until_done(workers: usize, lock: i64, done: i64) -> usize {
    let deadline = Instant::now() + WORKER_DEADLINE;
    let mut collections = 0usize;
    loop {
        method(lock, "acquire", &[]);
        let mut finished = 0usize;
        for id in 0..workers {
            if decode(pyx_subscript(done, int(id as i64))) != 0 {
                finished += 1;
            }
        }
        method(lock, "release", &[]);

        if finished == workers {
            return collections;
        }
        assert!(
            Instant::now() < deadline,
            "workers did not finish: only {finished}/{workers} reported done"
        );

        pyx_gc_collect();
        collections += 1;
        // Main-thread garbage keeps the collector busy while the workers
        // mutate the shared containers.
        let _ = pyx_str_new(b"main-garbage".as_ptr(), 12);
        // Give the workers a chance to advance between two stop-the-world
        // rounds; without pacing the main thread would starve them of the
        // safepoint interval they need to make progress.
        std::thread::sleep(Duration::from_micros(50));
    }
}

fn assert_workers_succeeded(workers: usize, lock: i64, done: i64) {
    method(lock, "acquire", &[]);
    for id in 0..workers {
        assert_eq!(
            decode(pyx_subscript(done, int(id as i64))),
            1,
            "worker {id} did not finish cleanly"
        );
    }
    method(lock, "release", &[]);
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

/// Four threads mutate a shared list, dict and queue under a user lock while
/// the main thread collects repeatedly. Every append, dict write and queue
/// round-trip must survive; a mark running concurrently must never free live
/// data or deadlock.
#[test]
fn concurrent_mutation_survives_repeated_collections() {
    run("concurrent-mark", || {
        // Collect only when asked: this test drives the collections itself.
        std::env::set_var("PYX_GC_THRESHOLD", "1000000");
        pyx_runtime_init();

        let workers = 4usize;
        let rounds = 100i64;
        let lock = new_lock();
        let done_items = vec![int(0); workers];
        let done = list(&done_items);
        let shared_list = list(&[]);
        let shared_dict = dict();
        let queue = module("queue", "Queue", &[]);
        assert_eq!(pyx_exc_pending(), 0);

        let handles = start_workers(workers, rounds, lock, done, shared_list, shared_dict, queue);

        let collections = collect_until_done(workers, lock, done);
        assert!(collections > 0, "no collection ran while the workers ran");

        for handle in handles {
            assert_eq!(join(handle), pyx_none());
            assert_eq!(pyx_exc_pending(), 0);
        }
        assert_workers_succeeded(workers, lock, done);

        // No append was lost, every dict key kept its last write and the
        // queue was fully drained by the round-trips.
        assert_eq!(decode(pyx_len(shared_list)), workers as i64 * rounds);
        for id in 0..workers {
            assert_eq!(
                decode(pyx_subscript(shared_dict, int(id as i64))),
                rounds - 1,
                "dict value for worker {id} was corrupted"
            );
        }
        assert_eq!(method(queue, "qsize", &[]), int(0));
        assert_eq!(method(queue, "empty", &[]), pyx_true());

        // A final collection keeps the main thread's roots.
        let keeper = text("concurrent-keeper");
        let keeper_list = list(&[keeper]);
        pyx_gc_collect();
        assert_eq!(
            pyx_binop(7, pyx_subscript(keeper_list, int(0)), keeper),
            pyx_true()
        );
        assert_eq!(pyx_exc_pending(), 0);
    });
}

/// `PYX_GC_THRESHOLD=1` stress: nearly every allocation starts a collection,
/// so objects are frequently created while a mark is in progress. The
/// long-lived roots must survive and every worker round-trip must stay
/// correct.
#[test]
fn threshold_one_stress_keeps_live_allocations() {
    run("threshold-one", || {
        std::env::set_var("PYX_GC_THRESHOLD", "1");
        pyx_runtime_init();

        // Long-lived roots allocated before the workers start.
        let keeper = text("stress-keeper");
        let keeper_list = list(&[keeper]);

        let workers = 2usize;
        let rounds = 15i64;
        let lock = new_lock();
        let done_items = vec![int(0); workers];
        let done = list(&done_items);
        let shared_list = list(&[]);
        let shared_dict = dict();
        let queue = module("queue", "Queue", &[]);
        assert_eq!(pyx_exc_pending(), 0);

        let handles = start_workers(workers, rounds, lock, done, shared_list, shared_dict, queue);

        let collections = collect_until_done(workers, lock, done);
        assert!(
            collections > 0,
            "no explicit collection ran during the stress test"
        );

        // The pre-existing roots must still be reachable and intact after
        // thousands of allocations and collections.
        assert_eq!(
            pyx_binop(7, pyx_subscript(keeper_list, int(0)), keeper),
            pyx_true()
        );

        for handle in handles {
            assert_eq!(join(handle), pyx_none());
            assert_eq!(pyx_exc_pending(), 0);
        }
        assert_workers_succeeded(workers, lock, done);
        assert_eq!(decode(pyx_len(shared_list)), workers as i64 * rounds);
        for id in 0..workers {
            assert_eq!(
                decode(pyx_subscript(shared_dict, int(id as i64))),
                rounds - 1,
                "dict value for worker {id} was corrupted under stress"
            );
        }
        assert_eq!(method(queue, "qsize", &[]), int(0));

        // Objects allocated during an active mark must survive when a root
        // keeps them alive.
        let late = list(&[text("late-allocation")]);
        pyx_gc_collect();
        assert_eq!(pyx_len(late), int(1));
        assert_eq!(
            pyx_binop(7, pyx_subscript(late, int(0)), text("late-allocation")),
            pyx_true()
        );
        assert_eq!(pyx_exc_pending(), 0);
    });
}

// -------------------------------------------------------------------------
// Large-container edge barrier
// -------------------------------------------------------------------------

/// `append_worker(lock, done, list, id, count)`: appends `count` fresh strings
/// to the shared list under the user lock and reports completion in `done[id]`.
///
/// Appending to a large container while a mark is in progress used to re-grey
/// and re-trace the whole parent per insert (O(n²)). The insertion barrier must
/// only mark the new child, so the run stays bounded.
extern "C" fn append_worker(argv: *const i64) -> i64 {
    // SAFETY: the caller passes exactly five arguments (arity checked by
    // `_thread.start_new_thread`).
    let args = unsafe { std::slice::from_raw_parts(argv, 5) };
    let (lock, done, list, id, count) = (args[0], args[1], args[2], args[3], decode(args[4]));
    let mut i = 0i64;
    while i < count {
        method(lock, "acquire", &[]);
        let item = pyx_str_new(b"edge-item".as_ptr(), 9);
        pyx_list_append(list, item);
        method(lock, "release", &[]);
        i += 1;
    }
    method(lock, "acquire", &[]);
    pyx_subscript_set(done, id, int(1));
    method(lock, "release", &[]);
    pyx_none()
}

/// Three mutators grow one shared list to 111k strings while the main thread
/// collects with `PYX_GC_THRESHOLD=20000`. Every item must survive and the
/// whole phase must stay far away from the old parent-re-trace cost.
#[test]
fn large_container_appends_stay_bounded() {
    run("large-container", || {
        std::env::set_var("PYX_GC_THRESHOLD", "20000");
        pyx_runtime_init();

        let workers = 3usize;
        let per_worker = 37_000i64;
        let lock = new_lock();
        let done = list(&vec![int(0); workers]);
        let shared = list(&[]);

        let mut handles = Vec::with_capacity(workers);
        for id in 0..workers {
            handles.push(start(
                function(append_worker, 5, "append_worker"),
                list(&[lock, done, shared, int(id as i64), int(per_worker)]),
            ));
            assert_eq!(pyx_exc_pending(), 0, "failed to start worker {id}");
        }

        let started = Instant::now();
        let deadline = started + WORKER_DEADLINE;
        let mut collections = 0usize;
        let mut last_collected = 0i64;
        let mut finished = 0usize;
        while finished != workers {
            // Poll the shared state under the same user lock the appenders
            // use, so reading the list length never races with a push.
            method(lock, "acquire", &[]);
            finished = (0..workers)
                .filter(|id| decode(pyx_subscript(done, int(*id as i64))) != 0)
                .count();
            let len = decode(pyx_len(shared));
            method(lock, "release", &[]);
            if finished == workers {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "appenders did not finish: only {finished}/{workers} reported done"
            );
            // Collect every few thousand new elements so each mark sees a
            // progressively larger parent, without letting the stop-the-world
            // rounds dominate the runtime.
            if len - last_collected >= 5_000 {
                pyx_gc_collect();
                collections += 1;
                last_collected = len;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        let elapsed = started.elapsed();

        for handle in handles {
            assert_eq!(join(handle), pyx_none());
            assert_eq!(pyx_exc_pending(), 0);
        }
        assert_workers_succeeded(workers, lock, done);

        let total = workers as i64 * per_worker;
        assert_eq!(decode(pyx_len(shared)), total);
        assert!(collections > 0, "no collection ran while the list grew");

        // A lost edge barrier would have swept strings that were appended to
        // a black parent: every element must still be the string that was
        // appended.
        let expected = text("edge-item");
        for index in 0..total {
            assert_eq!(
                pyx_binop(7, pyx_subscript(shared, int(index)), expected),
                pyx_true(),
                "list item {index} was lost"
            );
        }
        assert_eq!(pyx_exc_pending(), 0);

        // The old parent-re-trace barrier needed seconds per collection here;
        // the edge barrier keeps the whole phase in the low seconds even in a
        // debug build.
        assert!(
            elapsed < Duration::from_secs(15),
            "appending to a large shared list while marking took {elapsed:?}: \
             the insertion barrier must not re-trace the parent per append"
        );
    });
}

// -------------------------------------------------------------------------
// Concurrent sweep
// -------------------------------------------------------------------------

/// `sweep_worker(lock, done, shared, id, rounds)`.
///
/// Allocates a short-lived string every round (white as soon as the loop
/// advances, so the sweep has plenty to free) and appends a fresh string to
/// the shared list under the user lock. Every allocation runs a safepoint and
/// can therefore race a collector that is partitioning and freeing whites.
extern "C" fn sweep_worker(argv: *const i64) -> i64 {
    // SAFETY: the caller passes exactly five arguments (arity checked by
    // `_thread.start_new_thread`).
    let args = unsafe { std::slice::from_raw_parts(argv, 5) };
    let (lock, done, shared, id, rounds) = (args[0], args[1], args[2], args[3], decode(args[4]));

    let mut i = 0i64;
    while i < rounds {
        // Garbage: unreachable once the loop advances, swept concurrently.
        let _junk = pyx_str_new(b"sweep-garbage".as_ptr(), 13);
        method(lock, "acquire", &[]);
        let item = pyx_str_new(b"sweep-item".as_ptr(), 10);
        pyx_list_append(shared, item);
        method(lock, "release", &[]);
        i += 1;
    }

    method(lock, "acquire", &[]);
    pyx_subscript_set(done, id, int(1));
    method(lock, "release", &[]);
    pyx_none()
}

/// Collects repeatedly while `workers` sweep workers allocate and mutate.
/// Returns the number of collections and the total number of freed objects.
fn sweep_until_done(workers: usize, lock: i64, done: i64) -> (usize, i64) {
    let deadline = Instant::now() + WORKER_DEADLINE;
    let mut collections = 0usize;
    let mut freed = 0i64;
    loop {
        // Partition and free the whites first, so the collection overlaps the
        // workers' allocations and appends rather than only observing them.
        freed += pyx_gc_collect();
        collections += 1;
        let _ = pyx_str_new(b"main-sweep-garbage".as_ptr(), 18);
        method(lock, "acquire", &[]);
        let finished = (0..workers)
            .filter(|id| decode(pyx_subscript(done, int(*id as i64))) != 0)
            .count();
        method(lock, "release", &[]);
        if finished == workers {
            return (collections, freed);
        }
        assert!(
            Instant::now() < deadline,
            "sweep workers did not finish: only {finished}/{workers} reported done"
        );
        std::thread::sleep(Duration::from_micros(50));
    }
}

/// Verifies the shared list the sweep workers grew: every appended string must
/// still be intact and none may have been freed by a concurrent sweep.
fn assert_sweep_list_intact(shared: i64, total: i64) {
    assert_eq!(
        decode(pyx_len(shared)),
        total,
        "the shared list lost appends during a concurrent sweep"
    );
    let expected = text("sweep-item");
    let mut index = 0i64;
    while index < total {
        assert_eq!(
            pyx_binop(7, pyx_subscript(shared, int(index)), expected),
            pyx_true(),
            "list item {index} was corrupted or freed by the concurrent sweep"
        );
        // Sample the whole list without making the check O(n) per collection.
        index += 97;
    }
    assert_eq!(
        pyx_binop(7, pyx_subscript(shared, int(total - 1)), expected),
        pyx_true(),
        "the last appended item was lost"
    );
}

/// A large live graph plus tens of thousands of white objects: while the main
/// thread collects (final rescan, then registry partition and concurrent
/// freeing), a second thread keeps allocating and appending. The live graph and
/// every appended item must survive; a UAF or deadlock in the concurrent sweep
/// fails the test or trips the watchdog.
#[test]
fn concurrent_sweep_frees_garbage_while_mutators_allocate() {
    run("concurrent-sweep", || {
        // Collect only when asked: this test drives the collections itself.
        std::env::set_var("PYX_GC_THRESHOLD", "1000000");
        pyx_runtime_init();

        // Large live graph: 4000 strings kept by a list and a dict of small
        // lists. The registry is large enough that the final rescan and the
        // colour reset have real work while the sweep frees the whites.
        let keepers: Vec<i64> = (0..4_000).map(|i| text(&format!("keeper-{i}"))).collect();
        let live = list(&keepers);
        let live_dict = dict();
        for i in 0..200usize {
            let inner = list(&keepers[i * 5..i * 5 + 5]);
            pyx_subscript_set(live_dict, int(i as i64), inner);
        }

        // Tens of thousands of white boxes for the sweep to free while the
        // workers are running.
        for _ in 0..40_000 {
            let _ = pyx_str_new(b"pre-sweep-garbage".as_ptr(), 17);
        }

        let workers = 2usize;
        let rounds = 1_500i64;
        let lock = new_lock();
        let done = list(&vec![int(0); workers]);
        let shared = list(&[]);
        assert_eq!(pyx_exc_pending(), 0);

        let mut handles = Vec::with_capacity(workers);
        for id in 0..workers {
            handles.push(start(
                function(sweep_worker, 5, "sweep_worker"),
                list(&[lock, done, shared, int(id as i64), int(rounds)]),
            ));
            assert_eq!(pyx_exc_pending(), 0, "failed to start worker {id}");
        }

        let (collections, freed) = sweep_until_done(workers, lock, done);
        assert!(
            collections > 0,
            "no collection ran while the sweep workers ran"
        );
        // The whites allocated above must have been partitioned out of the
        // registry and freed outside the heap lock.
        assert!(
            freed > 10_000,
            "only {freed} objects were freed over {collections} collections: \
             the concurrent sweep did not reclaim the white objects"
        );

        for handle in handles {
            assert_eq!(join(handle), pyx_none());
            assert_eq!(pyx_exc_pending(), 0);
        }
        assert_workers_succeeded(workers, lock, done);

        let total = workers as i64 * rounds;
        assert_sweep_list_intact(shared, total);

        // The live graph must be untouched by the sweeps that ran concurrently
        // with the workers.
        assert_eq!(decode(pyx_len(live)), 4_000);
        for index in [0i64, 1, 999, 2_000, 3_999] {
            assert_eq!(
                pyx_binop(7, pyx_subscript(live, int(index)), keepers[index as usize]),
                pyx_true(),
                "live object {index} was freed or corrupted by the concurrent sweep"
            );
        }
        let inner = pyx_subscript(live_dict, int(42));
        assert_eq!(decode(pyx_len(inner)), 5);
        assert_eq!(
            pyx_binop(7, pyx_subscript(inner, int(0)), keepers[210]),
            pyx_true(),
            "a dict entry of the live graph was lost"
        );
        assert!(
            pyx_gc_objects() > 4_000,
            "the live graph shrank unexpectedly"
        );

        // A final collection with no concurrent mutator keeps the roots.
        pyx_gc_collect();
        assert_eq!(
            pyx_binop(7, pyx_subscript(live, int(3_999)), keepers[3_999]),
            pyx_true()
        );
        assert_eq!(pyx_exc_pending(), 0);
    });
}

/// `PYX_GC_THRESHOLD=1` stress: nearly every allocation starts a collection,
/// so collectors are started from the mutators themselves while another sweep
/// may be partitioning or freeing. The phase transitions between marking, the
/// final rescan and the concurrent sweep must keep the live graph intact and
/// must not deadlock.
#[test]
fn sweep_threshold_one_stress_keeps_live_graph() {
    run("sweep-threshold-one", || {
        std::env::set_var("PYX_GC_THRESHOLD", "1");
        pyx_runtime_init();

        let keepers: Vec<i64> = (0..300).map(|i| text(&format!("stress-{i}"))).collect();
        let live = list(&keepers);

        let workers = 2usize;
        let rounds = 120i64;
        let lock = new_lock();
        let done = list(&vec![int(0); workers]);
        let shared = list(&[]);
        assert_eq!(pyx_exc_pending(), 0);

        let mut handles = Vec::with_capacity(workers);
        for id in 0..workers {
            handles.push(start(
                function(sweep_worker, 5, "sweep_worker"),
                list(&[lock, done, shared, int(id as i64), int(rounds)]),
            ));
            assert_eq!(pyx_exc_pending(), 0, "failed to start worker {id}");
        }

        // `pyx_gc_collect()` returns 0 when a worker's allocation already owns
        // the collector lock, which is the interleaving this test wants.
        let (_collections, _freed) = sweep_until_done(workers, lock, done);

        for handle in handles {
            assert_eq!(join(handle), pyx_none());
            assert_eq!(pyx_exc_pending(), 0);
        }
        assert_workers_succeeded(workers, lock, done);

        assert_sweep_list_intact(shared, workers as i64 * rounds);
        assert_eq!(decode(pyx_len(live)), 300);
        for (index, keeper) in keepers.iter().enumerate().take(300) {
            assert_eq!(
                pyx_binop(7, pyx_subscript(live, int(index as i64)), *keeper),
                pyx_true(),
                "stress keeper {index} was lost under threshold-one collections"
            );
        }
        assert_eq!(pyx_exc_pending(), 0);
    });
}
