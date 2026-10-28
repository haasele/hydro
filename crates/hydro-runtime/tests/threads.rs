//! Integration tests for the shared heap and `_thread` (Phase 2.12).
//!
//! The heap switch, the thread registry and the module-global store are
//! process-global, so every test takes `TEST_LOCK` and starts from
//! `hydro_runtime_init` (which resets both the thread-local and the shared
//! heap, and clears the module globals with their process-wide roots). Threads
//! are always joined before the guard is released, so no test observes
//! another test's state.
//!
//! Values are tagged `i64`s; helpers below decode them via `hydro_to_int`, which
//! is the identity for integer inputs.

use std::sync::Mutex;

use hydro_runtime::abi::*;

/// Serialises the tests in this binary: the shared heap is a process-global.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn int(n: i64) -> i64 {
    // Encode a raw small integer as a tagged immediate (the ABI passes tagged
    // values; `hydro_int` itself is an identity on already-tagged input).
    n << 3
}

/// Decodes a tagged integer produced by the runtime.
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

/// `module.name = value` for a module-level global.
fn set_global(module: &str, name: &str, value: i64) -> i64 {
    hydro_global_set(
        module.as_ptr(),
        module.len(),
        name.as_ptr(),
        name.len(),
        value,
    )
}

/// `module.name` for a module-level global.
fn get_global(module: &str, name: &str) -> i64 {
    hydro_global_get(module.as_ptr(), module.len(), name.as_ptr(), name.len())
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

fn clear_error() {
    assert_eq!(hydro_exc_pending(), 1, "expected a pending exception");
    hydro_exc_clear();
}

// -------------------------------------------------------------------------
// Worker wrappers (generated code has the same `extern "C"` shape)
// -------------------------------------------------------------------------

/// `worker(lock, counter, results, iterations)`: increments `counter[0]` and
/// appends `i` to `results` under `lock`.
extern "C" fn increment_worker(argv: *const i64) -> i64 {
    // SAFETY: the caller passes exactly four arguments.
    let args = unsafe { std::slice::from_raw_parts(argv, 4) };
    let (lock, counter, results, iterations) = (args[0], args[1], args[2], args[3]);
    let zero = int(0);
    let count = decode(iterations);
    let mut i = 0i64;
    while i < count {
        method(lock, "acquire", &[]);
        let current = hydro_subscript(counter, zero);
        let next = hydro_binop(0, current, int(1)); // 0 = add
        hydro_subscript_set(counter, zero, next);
        hydro_list_append(results, int(i));
        method(lock, "release", &[]);
        i += 1;
    }
    hydro_none()
}

/// `worker(out)`: stores the running thread's ident at `out[0]`.
extern "C" fn ident_worker(argv: *const i64) -> i64 {
    // SAFETY: the caller passes exactly one argument.
    let args = unsafe { std::slice::from_raw_parts(argv, 1) };
    let ident = module("_thread", "get_ident", &[]);
    hydro_subscript_set(args[0], int(0), ident);
    hydro_none()
}

/// `worker(lock, stop_list, ready_list)`: signals readiness, then allocates
/// garbage until the parent sets `stop_list[0] = 1` (both list accesses under
/// `lock`).
extern "C" fn garbage_worker(argv: *const i64) -> i64 {
    // SAFETY: the caller passes exactly three arguments.
    let args = unsafe { std::slice::from_raw_parts(argv, 3) };
    let (lock, stop_list, ready_list) = (args[0], args[1], args[2]);

    method(lock, "acquire", &[]);
    hydro_subscript_set(ready_list, int(0), int(1));
    method(lock, "release", &[]);

    loop {
        for _ in 0..256 {
            let _ = hydro_str_new(b"child-garbage".as_ptr(), 13);
        }
        method(lock, "acquire", &[]);
        let stop = decode(hydro_subscript(stop_list, int(0)));
        method(lock, "release", &[]);
        if stop == 1 {
            break;
        }
    }
    hydro_none()
}

extern "C" fn zero_worker(_argv: *const i64) -> i64 {
    hydro_none()
}

/// `worker(lock, ready, out, payload)`: signals readiness, then blocks on
/// `lock` (the parent holds it), and only after acquiring it stores the
/// payload at `out[0]`.
///
/// `payload` is deliberately not loaded before the blocking acquire: until
/// the child resumes, it is reachable only through the thread roots that
/// `start_new_thread` registers (the closure keeps the callable and the
/// argument buffer on the Rust heap, not on the child's stack).
extern "C" fn argument_worker(argv: *const i64) -> i64 {
    // SAFETY: the caller passes exactly four arguments.
    let args = unsafe { std::slice::from_raw_parts(argv, 4) };
    let (lock, ready, out) = (args[0], args[1], args[2]);
    hydro_subscript_set(ready, int(0), int(1));
    method(lock, "acquire", &[]);
    hydro_subscript_set(out, int(0), args[3]);
    method(lock, "release", &[]);
    hydro_none()
}

/// Overwrites the stack region used by the helper frames below, so stale
/// tagged values from the spawn call are unlikely to be found by the
/// conservative scan of the collector's own stack. `inline(never)` keeps the
/// frame (and thus the scrubbed region) materialized.
#[inline(never)]
fn scrub_stack() {
    let mut buffer = [0u8; 64 * 1024];
    std::hint::black_box(&mut buffer);
    buffer[0] = 0;
    std::hint::black_box(&buffer);
}

/// Creates a callable and a freshly allocated string argument, spawns the
/// worker while the parent holds `lock`, and returns the handle. Neither the
/// callable nor the payload escapes this frame.
#[inline(never)]
fn spawn_with_fresh_argument(lock: i64, ready: i64, out: i64) -> i64 {
    let payload = text("argument-payload");
    let worker = function(argument_worker, 4, "argument");
    start(worker, list(&[lock, ready, out, payload]))
}

/// `worker(ready)`: stores a freshly allocated string as a module global,
/// signals readiness and exits.
///
/// The payload is created inside the child, so after the thread exits the only
/// thing keeping it reachable is the process-wide root installed by the global
/// store (spawn arguments and the child stack are gone).
extern "C" fn module_global_writer(argv: *const i64) -> i64 {
    // SAFETY: the caller passes exactly one argument.
    let args = unsafe { std::slice::from_raw_parts(argv, 1) };
    let payload = text("global-payload-survives-thread-exit");
    // Store the payload through the module-global API; generated code ignores
    // the `None` result the same way.
    let _ = set_global("threadglobals", "payload", payload);
    hydro_subscript_set(args[0], int(0), int(1));
    hydro_none()
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[test]
fn concurrent_increments_under_user_lock() {
    let _guard = guard();
    hydro_runtime_init();

    let lock = new_lock();
    // Counter and results are created before the spawn: the shared-heap
    // migration must make them visible to both children.
    let counter = list(&[int(0)]);
    let results = list(&[]);
    let worker = function(increment_worker, 4, "worker");

    let a = start(worker, list(&[lock, counter, results, int(1000)]));
    assert_eq!(hydro_exc_pending(), 0);
    let b = start(worker, list(&[lock, counter, results, int(1000)]));
    assert_eq!(hydro_exc_pending(), 0);
    assert!(decode(a) > 0 && decode(b) > 0);
    assert_ne!(decode(a), decode(b));

    assert_eq!(join(a), hydro_none());
    assert_eq!(hydro_exc_pending(), 0);
    assert_eq!(join(b), hydro_none());
    assert_eq!(hydro_exc_pending(), 0);

    // No increments were lost: the user lock serialises the read-modify-write.
    assert_eq!(decode(hydro_subscript(counter, int(0))), 2000);
    assert_eq!(decode(hydro_len(results)), 2000);
}

#[test]
fn join_ident_and_error_paths() {
    let _guard = guard();
    hydro_runtime_init();

    let main_id = module("_thread", "get_ident", &[]);
    assert_eq!(hydro_exc_pending(), 0);
    assert!(decode(main_id) >= 1);

    let out = list(&[int(0)]);
    let handle = start(function(ident_worker, 1, "ident"), list(&[out]));
    assert_eq!(hydro_exc_pending(), 0);
    assert_eq!(join(handle), hydro_none());
    assert_eq!(hydro_exc_pending(), 0);

    let child_id = hydro_subscript(out, int(0));
    assert!(decode(child_id) > 0);
    assert_ne!(decode(child_id), decode(main_id));

    // A joined handle is gone: joining again raises RuntimeError.
    assert_eq!(join(handle), hydro_none());
    clear_error();
    // Unknown ids are rejected the same way.
    assert_eq!(join(int(9999)), hydro_none());
    clear_error();

    // Arity is prechecked before a thread is spawned.
    assert_eq!(
        start(function(zero_worker, 0, "zero"), list(&[int(1)])),
        hydro_none()
    );
    clear_error();
    // Non-callable and non-sequence arguments are rejected too.
    assert_eq!(start(int(7), list(&[])), hydro_none());
    clear_error();
    assert_eq!(
        start(function(zero_worker, 0, "zero"), int(7)),
        hydro_none()
    );
    clear_error();
}

#[test]
fn lock_semantics() {
    let _guard = guard();
    hydro_runtime_init();

    let lock = new_lock();
    assert_eq!(hydro_exc_pending(), 0);
    assert_eq!(hydro_bool(lock), hydro_true());
    assert_eq!(
        hydro_binop(7, hydro_to_str(lock), text("<lock object>")), // 7 = eq
        hydro_true()
    );

    assert_eq!(method(lock, "locked", &[]), hydro_false());
    assert_eq!(method(lock, "acquire", &[]), hydro_true());
    assert_eq!(method(lock, "locked", &[]), hydro_true());
    // Non-blocking acquire fails while the lock is held.
    assert_eq!(method(lock, "acquire", &[hydro_false()]), hydro_false());
    assert_eq!(method(lock, "release", &[]), hydro_none());
    assert_eq!(method(lock, "locked", &[]), hydro_false());

    // Releasing an unlocked lock raises RuntimeError.
    assert_eq!(method(lock, "release", &[]), hydro_none());
    clear_error();

    // Arity and unknown-method errors go through the shared helper.
    assert_eq!(
        method(lock, "acquire", &[hydro_true(), hydro_true()]),
        hydro_none()
    );
    clear_error();
    assert_eq!(method(lock, "nope", &[]), hydro_none());
    clear_error();
    assert_eq!(hydro_exc_pending(), 0);
}

#[test]
fn stop_the_world_collects_garbage_while_threads_run() {
    let _guard = guard();
    hydro_runtime_init();

    // Reachable shared data, created before the spawn.
    let keeper = text("keeper");
    let keeper_list = list(&[keeper]);
    let lock = new_lock();
    let stop_list = list(&[int(0)]);
    let ready_list = list(&[int(0)]);
    let handle = start(
        function(garbage_worker, 3, "garbage"),
        list(&[lock, stop_list, ready_list]),
    );
    assert_eq!(hydro_exc_pending(), 0);

    // Wait until the child has produced garbage, then collect while it is
    // still allocating. The collector stops the child at a safepoint, scans
    // its saved stack/registers and sweeps the garbage.
    let poll_delay = hydro_float_new(0.0005);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while decode(hydro_subscript(ready_list, int(0))) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "child did not become ready"
        );
        module("time", "sleep", &[poll_delay]);
    }

    let mut freed = 0;
    for _ in 0..8 {
        freed += hydro_gc_collect();
        module("time", "sleep", &[poll_delay]);
    }
    assert!(freed > 0, "the collector freed nothing while a child ran");
    assert_eq!(hydro_exc_pending(), 0);

    // Reachable data survives every stop-the-world collection.
    assert_eq!(hydro_subscript(keeper_list, int(0)), keeper);
    assert_eq!(decode(hydro_len(keeper_list)), 1);

    // Ask the child to stop (under the user lock) and join it.
    method(lock, "acquire", &[]);
    hydro_subscript_set(stop_list, int(0), int(1));
    method(lock, "release", &[]);
    assert_eq!(join(handle), hydro_none());
    assert_eq!(hydro_exc_pending(), 0);

    // With no other thread left the collector still works and keeps roots.
    hydro_gc_collect();
    assert_eq!(hydro_subscript(keeper_list, int(0)), keeper);
    assert_eq!(
        hydro_binop(7, hydro_subscript(keeper_list, int(0)), keeper),
        hydro_true()
    );
}

#[test]
fn spawn_arguments_survive_collection_while_child_is_parked() {
    let _guard = guard();
    hydro_runtime_init();

    // The parent holds the lock for the whole handshake, so the child parks
    // at the blocking acquire without ever touching its payload argument.
    let lock = new_lock();
    let ready = list(&[int(0)]);
    let out = list(&[hydro_none()]);
    assert_eq!(method(lock, "acquire", &[]), hydro_true());

    let handle = spawn_with_fresh_argument(lock, ready, out);
    assert_eq!(hydro_exc_pending(), 0);

    // Wait (bounded) until the child has registered its roots and reached the
    // blocking acquire. `time.sleep` parks the parent, so a concurrent
    // collection cannot wait on it either.
    let poll_delay = hydro_float_new(0.0005);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while decode(hydro_subscript(ready, int(0))) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "child did not reach the park point"
        );
        module("time", "sleep", &[poll_delay]);
    }

    // Drop the spawn helper's stale stack words, then collect while the child
    // is parked. The payload string, the callable and the argument buffer are
    // only reachable through the child's registered roots.
    scrub_stack();
    let _freed = hydro_gc_collect();
    // Allocate into the (possibly) freed blocks before the child resumes, so
    // a collected payload would be overwritten rather than merely leaked.
    for _ in 0..4096 {
        let _ = hydro_str_new(b"post-collection-garbage".as_ptr(), 23);
    }
    assert_eq!(hydro_exc_pending(), 0);

    assert_eq!(method(lock, "release", &[]), hydro_none());
    assert_eq!(join(handle), hydro_none());
    assert_eq!(hydro_exc_pending(), 0);

    // The child stored the very object it was spawned with; reading its
    // contents proves it was neither collected nor recycled.
    let child_value = hydro_subscript(out, int(0));
    let expected = "argument-payload";
    assert_eq!(hydro_len(child_value), int(expected.len() as i64));
    assert_eq!(hydro_binop(7, child_value, text(expected)), hydro_true());
    assert_eq!(hydro_exc_pending(), 0);
}

#[test]
fn module_globals_are_process_wide_across_os_threads() {
    let _guard = guard();
    hydro_runtime_init();

    // Two plain OS threads (no runtime registration): the module store is
    // process-wide, so each thread sees the module the other one created.
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let alpha = {
        let barrier = std::sync::Arc::clone(&barrier);
        std::thread::spawn(move || {
            assert_eq!(
                set_global("threadmod.alpha", "value", int(11)),
                hydro_none()
            );
            barrier.wait();
            assert_eq!(decode(get_global("threadmod.beta", "value")), 22);
        })
    };
    let beta = {
        let barrier = std::sync::Arc::clone(&barrier);
        std::thread::spawn(move || {
            assert_eq!(set_global("threadmod.beta", "value", int(22)), hydro_none());
            barrier.wait();
            assert_eq!(decode(get_global("threadmod.alpha", "value")), 11);
        })
    };
    alpha.join().expect("alpha writer thread");
    beta.join().expect("beta writer thread");
    assert_eq!(hydro_exc_pending(), 0);

    // Both writers are gone; their bindings and modules remain observable.
    assert_eq!(decode(get_global("threadmod.alpha", "value")), 11);
    assert_eq!(decode(get_global("threadmod.beta", "value")), 22);

    // The registry discovers user modules from the global store, so
    // `sys.modules` reports both even though no thread ever imported them.
    let modules = hydro_module_get(b"sys".as_ptr(), 3, b"modules".as_ptr(), 7);
    let alpha_module = hydro_module_value(b"threadmod.alpha".as_ptr(), 15);
    let beta_module = hydro_module_value(b"threadmod.beta".as_ptr(), 14);
    assert_ne!(alpha_module, hydro_none());
    assert_ne!(beta_module, hydro_none());
    assert_ne!(alpha_module, beta_module);
    assert_eq!(
        hydro_subscript(modules, text("threadmod.alpha")),
        alpha_module
    );
    assert_eq!(
        hydro_subscript(modules, text("threadmod.beta")),
        beta_module
    );
    assert_eq!(hydro_exc_pending(), 0);
}

#[test]
fn module_global_values_survive_writer_thread_exit() {
    let _guard = guard();
    hydro_runtime_init();

    let ready = list(&[int(0)]);
    let handle = start(
        function(module_global_writer, 1, "global-writer"),
        list(&[ready]),
    );
    assert_eq!(hydro_exc_pending(), 0);

    let poll_delay = hydro_float_new(0.0005);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while decode(hydro_subscript(ready, int(0))) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "writer did not become ready"
        );
        module("time", "sleep", &[poll_delay]);
    }
    assert_eq!(join(handle), hydro_none());
    assert_eq!(hydro_exc_pending(), 0);

    // The writer's stack and spawn roots are gone: the payload is reachable
    // only through the process-wide root that the global store installed.
    // Collect repeatedly while allocating into any freed blocks.
    scrub_stack();
    let mut freed = 0;
    for _ in 0..8 {
        for _ in 0..2048 {
            let _ = hydro_str_new(b"post-writer-garbage!".as_ptr(), 20);
        }
        freed += hydro_gc_collect();
    }
    assert!(
        freed > 0,
        "the collector freed no garbage after the writer exited"
    );
    assert_eq!(hydro_exc_pending(), 0);

    let payload = get_global("threadglobals", "payload");
    assert_ne!(payload, hydro_none());
    let expected = "global-payload-survives-thread-exit";
    assert_eq!(decode(hydro_len(payload)), expected.len() as i64);
    assert_eq!(hydro_binop(7, payload, text(expected)), hydro_true());
    assert_eq!(hydro_exc_pending(), 0);

    let modules = hydro_module_get(b"sys".as_ptr(), 3, b"modules".as_ptr(), 7);
    let module_value = hydro_module_value(b"threadglobals".as_ptr(), 13);
    assert_eq!(
        hydro_subscript(modules, text("threadglobals")),
        module_value
    );
    assert_eq!(hydro_exc_pending(), 0);
}
