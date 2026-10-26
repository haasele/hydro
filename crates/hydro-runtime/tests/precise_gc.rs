//! Collector integration tests for precise shadow-stack roots (P2-06/P2-07).
//!
//! These tests run the collector with `HYDRO_GC_PRECISE=1` (strict mode): every
//! conservative stack/register scan is disabled, so an object survives only
//! when it is named by a precise root (shadow frame, scope range, persistent
//! slot, pending/handled/thread root or a suspended fiber's `SavedRoots`).
//!
//! The tests are deliberately Rust-level: they exercise the same ABI the
//! generated code uses, on a thread-local heap (no `_thread` is started
//! except by the dedicated shared-mode test). The strict-mode test therefore
//! also proves that unregistered Rust stack words and registers no longer
//! retain garbage. The conservative suites (`gc.rs`, `concurrent_gc.rs`,
//! `stw_gc.rs`, `gc_properties.rs`) intentionally keep their raw-Rust locals
//! on the stack and must run in the default mixed mode.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use hydro_runtime::abi::*;
use hydro_runtime::precise_roots::{
    add_persistent_root, remove_persistent_root, set_persistent_root,
};

/// Serialises the tests: the runtime, the thread registry and the environment
/// are process-global.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn guard() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Enables strict precise mode and starts a fresh runtime on this thread.
fn init_strict() {
    std::env::set_var("HYDRO_GC_PRECISE", "1");
    hydro_runtime_init();
    assert_eq!(hydro_exc_pending(), 0);
    let stats = hydro_runtime::gc_stats();
    assert!(
        stats.precise,
        "HYDRO_GC_PRECISE=1 must be reported by gc_stats()"
    );
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

fn function(wrapper: extern "C" fn(*const i64) -> i64, arity: usize, name: &str) -> i64 {
    hydro_function_new(wrapper as *const u8, arity, name.as_ptr(), name.len(), 0)
}

fn start(func: i64, args: i64) -> i64 {
    module("_thread", "start_new_thread", &[func, args])
}

fn join(handle: i64) -> i64 {
    module("_thread", "join", &[handle])
}

/// (a) A tagged value stored in a registered shadow frame is the only root:
/// strict mode must keep it alive across a forced collection.
#[test]
fn frame_root_keeps_object_alive_in_strict_mode() {
    let _guard = guard();
    init_strict();

    let value = text("precise-alive");
    let mut slots = [hydro_none(); 4];
    let token = hydro_gc_frame_push(slots.as_mut_ptr(), slots.len());
    assert_ne!(token, 0, "frame_push returns a valid token");
    slots[0] = value;
    assert_eq!(slots[0], value);
    hydro_gc_frame_used(token, 1);

    let _freed = hydro_gc_collect();

    // Reading the payload proves the string object itself survived.
    assert_eq!(decode(hydro_len(value)), 13);
    hydro_gc_frame_pop(token);
    assert_eq!(hydro_exc_pending(), 0);
}

/// (b) Overwriting the slot with tagged `None` drops the root: the next
/// strict collection frees the object even though a stale copy may still sit
/// in unregistered register/stack words.
#[test]
fn cleared_frame_slot_releases_object_in_strict_mode() {
    let _guard = guard();
    init_strict();

    let mut slots = [hydro_none(); 2];
    let token = hydro_gc_frame_push(slots.as_mut_ptr(), slots.len());
    slots[0] = text("precise-dead");
    hydro_gc_frame_used(token, 1);

    let before = hydro_gc_objects();
    slots[0] = hydro_none();
    assert_eq!(slots[0], hydro_none());
    let freed = hydro_gc_collect();

    assert!(freed >= 1, "the unreferenced string was not collected");
    assert!(
        hydro_gc_objects() < before,
        "registry did not shrink ({before} -> {})",
        hydro_gc_objects()
    );
    hydro_gc_frame_pop(token);
    assert_eq!(hydro_exc_pending(), 0);
}

/// (c) Scope ranges (`argv`, packed call arguments) root their values exactly
/// like frames, and popping the range releases them.
#[test]
fn scope_range_roots_values_in_strict_mode() {
    let _guard = guard();
    init_strict();

    let first = text("scope-first");
    let second = text("scope-second");
    let args = [first, second];
    let token = hydro_gc_scope_push(args.as_ptr(), args.len());
    assert_ne!(token, 0);

    hydro_gc_collect();
    assert_eq!(decode(hydro_len(first)), 11);
    assert_eq!(decode(hydro_len(second)), 12);

    hydro_gc_scope_pop(token);
    let before = hydro_gc_objects();
    let freed = hydro_gc_collect();
    assert!(freed >= 2, "popped range values were not collected");
    assert!(hydro_gc_objects() < before);
    assert_eq!(hydro_exc_pending(), 0);
}

/// (d) A tagged word on an unregistered native stack is not a root in strict
/// mode: only precise slots, mirrors and persistent roots count.
#[test]
fn stale_unregistered_stack_word_does_not_retain_in_strict_mode() {
    let _guard = guard();
    init_strict();

    // A tagged heap pointer that only lives in a plain stack array. No frame
    // or scope range was pushed, so no collector may treat it as a root.
    let mut leaked = [hydro_none(); 8];
    leaked[3] = text("stale-stack-word");
    std::hint::black_box(&leaked);

    let before = hydro_gc_objects();
    let freed = hydro_gc_collect();
    std::hint::black_box(&leaked);

    assert!(
        freed >= 1,
        "a stale unregistered stack word retained an object in strict mode"
    );
    assert!(
        hydro_gc_objects() < before,
        "registry did not shrink ({before} -> {})",
        hydro_gc_objects()
    );
    assert_eq!(hydro_exc_pending(), 0);
}

/// (e) Persistent roots keep a value alive, can be updated to release the old
/// value, and are released by `remove_persistent_root`.
#[test]
fn persistent_root_slot_lifecycle() {
    let _guard = guard();
    init_strict();

    let first = text("persistent-a");
    let slot = add_persistent_root(first);
    assert_ne!(slot, usize::MAX, "no thread registration for the root");
    hydro_gc_collect();
    assert_eq!(decode(hydro_len(first)), 12);

    // Updating the slot releases the previous value.
    let second = text("persistent-b");
    set_persistent_root(slot, second);
    let before = hydro_gc_objects();
    let freed = hydro_gc_collect();
    assert!(freed >= 1, "the replaced persistent root was not collected");
    assert!(hydro_gc_objects() < before);
    assert_eq!(decode(hydro_len(second)), 12);

    // Removing the slot releases the remaining value.
    remove_persistent_root(slot);
    let before = hydro_gc_objects();
    let freed = hydro_gc_collect();
    assert!(freed >= 1, "the removed persistent root was not collected");
    assert!(hydro_gc_objects() < before);

    // A second add reuses the freed trailing slot index.
    let third = text("persistent-c");
    assert_eq!(add_persistent_root(third), slot);
    hydro_gc_collect();
    assert_eq!(decode(hydro_len(third)), 12);
    remove_persistent_root(slot);
}

/// Shared-heap strict mode: the worker's string exists only in its shadow
/// frame. It must survive collections that park the worker (the collector
/// reads the roots published at the safepoint) and remain intact after the
/// thread joins; the main thread's own lists/keeper are precise roots too.
static WORKER_STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn precise_worker(argv: *const i64) -> i64 {
    // SAFETY: `start` passes exactly two arguments (results list, flag list).
    let args = unsafe { std::slice::from_raw_parts(argv, 2) };
    let (results, flag) = (args[0], args[1]);

    let mut slots = [hydro_none(); 1];
    let token = hydro_gc_frame_push(slots.as_mut_ptr(), slots.len());
    slots[0] = text("strict-shared-root");
    hydro_gc_frame_used(token, 1);

    // Reach a safepoint repeatedly: when the main thread collects, this
    // thread publishes its precise frame before parking.
    while !WORKER_STOP.load(Ordering::Acquire) {
        hydro_safepoint();
        std::thread::yield_now();
    }

    // The value must still be the tagged string it was; a freed object would
    // not report this length.
    let length = hydro_len(slots[0]);
    hydro_list_append(results, slots[0]);
    hydro_gc_frame_pop(token);
    hydro_subscript_set(flag, int(0), length);
    hydro_none()
}

/// Runs `body` on a helper thread with a deadlock watchdog: a collector
/// deadlock then fails the test instead of hanging the whole binary.
fn run_with_timeout(body: impl FnOnce() + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    let helper = std::thread::Builder::new()
        .name("precise-shared-test".to_string())
        .spawn(move || {
            body();
            let _ = tx.send(());
        })
        .expect("spawn test helper");
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(()) => helper.join().expect("test body panicked"),
        Err(RecvTimeoutError::Disconnected) => helper.join().expect("test body panicked"),
        Err(RecvTimeoutError::Timeout) => {
            panic!("precise shared collection timed out (collector deadlock)")
        }
    }
}

#[test]
fn shared_strict_collection_marks_published_precise_roots() {
    run_with_timeout(|| {
        let _guard = guard();
        WORKER_STOP.store(false, Ordering::Release);
        init_strict();

        // The main thread's live tagged values live in a precise frame:
        // strict mode ignores its native stack and registers.
        let results = list(&[]);
        let flag = list(&[int(0)]);
        let keeper = text("main-strict-keeper");
        let mut slots = [hydro_none(); 6];
        let token = hydro_gc_frame_push(slots.as_mut_ptr(), slots.len());
        slots[0] = results;
        slots[1] = flag;
        slots[2] = keeper;
        // Root the callable and its argument list before `start` can allocate.
        slots[3] = function(precise_worker, 2, "precise_worker");
        slots[4] = list(&[results, flag]);

        let handle = start(slots[3], slots[4]);
        assert_eq!(hydro_exc_pending(), 0);
        slots[5] = handle;

        for _ in 0..32 {
            hydro_gc_collect();
            assert_eq!(hydro_exc_pending(), 0);
            std::thread::sleep(Duration::from_micros(200));
        }

        WORKER_STOP.store(true, Ordering::Release);
        assert_eq!(join(slots[5]), hydro_none());
        assert_eq!(hydro_exc_pending(), 0);

        // The worker's only-rooted string survived every collection.
        assert_eq!(decode(hydro_len(results)), 1);
        let shared = hydro_subscript(results, int(0));
        assert_eq!(decode(hydro_len(shared)), 18);
        assert_eq!(decode(hydro_subscript(flag, int(0))), 18);

        // Main-thread precise roots are still intact after the collection.
        assert_eq!(decode(hydro_len(keeper)), 18);
        hydro_gc_frame_pop(token);
    });
}
