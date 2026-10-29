//! On-demand initialisation of compiled module bodies.
//!
//! Codegen lowers every imported unit's module body to an `extern "C" fn() ->
//! i64` and lists them in `Program::initializers`. `hydro_main` runs them once,
//! in dependency order, before the entry module. A C extension that imports a
//! compiled module while the program is running (`PyImport_ImportModule`) must
//! be able to run the same body, even though the startup loop has not reached
//! it yet (the NumPy case: its extension imports `numpy.exceptions` from
//! inside its own module initializer).
//!
//! Generated code therefore registers every initializer here, keyed by the
//! import name (the function name with the `.<module>` suffix stripped), before
//! the startup loop. The C API bridge calls [`initialize`] for names the runtime
//! registry does not know yet; the startup loop goes through the same path, so
//! a body runs exactly once whichever entry point reaches it first.
//!
//! Registrations live in process-global storage because C extensions see every
//! thread's registry through the same `sys.modules`. A JIT run owns freshly
//! compiled code, so [`reset`] (called by `hydro_runtime_init`) discards the
//! pointers of the previous run; registering the same name again also re-arms
//! it.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, MutexGuard};

/// Compiled module body entry point, as emitted by codegen.
pub(crate) type Entry = unsafe extern "C" fn() -> i64;

#[derive(Default)]
struct Registry {
    /// Module import name (`numpy.exceptions`) to its module body.
    entries: HashMap<String, Entry>,
    /// Names whose body already ran (successfully or with a pending
    /// exception); a second attempt is a no-op.
    initialized: HashSet<String>,
    /// Names whose body is executing right now. Guards against re-entrant
    /// initialisation (a body that imports itself) without holding the lock
    /// across the call.
    running: HashSet<String>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));

/// Locks the registry, ignoring poisoning: the map stays a consistent set of
/// function pointers after a panicking test.
fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Registers (or re-arms) the module body of `name`.
///
/// Re-registering a name discards an earlier "already initialised" mark: the
/// new code was compiled for the current run and must execute when asked for.
pub(crate) fn register(name: &str, entry: Entry) {
    let mut registry = registry();
    registry.initialized.remove(name);
    registry.running.remove(name);
    registry.entries.insert(name.to_string(), entry);
}

/// Runs the initializer registered for `name` unless it already ran.
///
/// Returns `1` when the body executed during this call and `0` when no
/// initializer is registered, the name was already initialised, or the body is
/// currently running on another thread.
///
/// A pending exception raised by the body is left pending for the caller
/// (`hydro_exc_pending`); the name is still marked initialised, so a failing
/// body is never silently retried. Only a successful body publishes its module
/// value into `sys.modules` ([`crate::modules::module_value_by_name`]).
pub(crate) fn initialize(name: &str) -> i64 {
    let entry = {
        let mut registry = registry();
        if registry.initialized.contains(name) || registry.running.contains(name) {
            return 0;
        }
        let Some(entry) = registry.entries.get(name).copied() else {
            return 0;
        };
        registry.running.insert(name.to_string());
        entry
    };
    // SAFETY: entries are codegen-registered addresses of generated
    // `extern "C" fn() -> i64` module bodies; the code is alive for the
    // duration of the run (the JIT module outlives `hydro_main`).
    unsafe { entry() };
    let failed = crate::exc::pending();
    {
        let mut registry = registry();
        registry.running.remove(name);
        registry.initialized.insert(name.to_string());
    }
    if !failed {
        // Match a normal import: the module value is materialised once and
        // `sys.modules` keeps returning it.
        crate::modules::module_value_by_name(name);
    }
    1
}

/// Clears every registration and initialisation mark.
///
/// `hydro_runtime_init` calls this before `hydro_main` re-registers the
/// initializers of the freshly compiled program. The function pointers of a
/// previous JIT run are dangling after its module is dropped, so they must
/// never survive into the next run.
pub(crate) fn reset() {
    let mut registry = registry();
    registry.entries.clear();
    registry.initialized.clear();
    registry.running.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Keeps `hydro_runtime_init` (called by concurrent ABI tests) from
    /// clearing the registry in the middle of a test: the reset takes the heap
    /// reset lock, so holding it makes the registration and the initialize
    /// call atomic with respect to a reset.
    fn test_guard() -> MutexGuard<'static, ()> {
        crate::heap::heap_reset_test_guard()
    }

    static ONCE_RUNS: AtomicUsize = AtomicUsize::new(0);
    static REARM_RUNS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn once_entry() -> i64 {
        ONCE_RUNS.fetch_add(1, Ordering::SeqCst);
        1
    }

    unsafe extern "C" fn rearm_entry() -> i64 {
        REARM_RUNS.fetch_add(1, Ordering::SeqCst);
        1
    }

    unsafe extern "C" fn raising_entry() -> i64 {
        crate::exc::raise_with_message(crate::exc::TYPE_ERROR, "module body failed")
    }

    #[test]
    fn registered_initializer_runs_exactly_once() {
        let _guard = test_guard();
        let name = "hydro_test.module_init.once";
        ONCE_RUNS.store(0, Ordering::SeqCst);
        register(name, once_entry);
        assert_eq!(initialize(name), 1, "the first call must execute the body");
        assert_eq!(initialize(name), 0, "a second call must be a no-op");
        assert_eq!(ONCE_RUNS.load(Ordering::SeqCst), 1);
        // Like a normal import, the executed module is visible in the runtime
        // registry afterwards.
        assert!(crate::modules::registry_lookup(name).is_some());
    }

    #[test]
    fn unknown_initializer_is_a_no_op() {
        let _guard = test_guard();
        assert_eq!(initialize("hydro_test.module_init.unknown"), 0);
    }

    #[test]
    fn failing_initializer_leaves_the_pending_exception_and_does_not_retry() {
        let _guard = test_guard();
        let name = "hydro_test.module_init.raising";
        register(name, raising_entry);
        crate::exc::clear();
        assert_eq!(initialize(name), 1, "the body executed");
        assert_eq!(
            crate::exc::pending_tag(),
            crate::exc::TYPE_ERROR as i64,
            "the body's exception stays pending for the caller"
        );
        crate::exc::clear();
        assert_eq!(initialize(name), 0, "a failed body must not silently retry");
        assert!(!crate::exc::pending(), "no new exception was raised");
    }

    #[test]
    fn re_registration_re_arms_the_initializer() {
        let _guard = test_guard();
        let name = "hydro_test.module_init.rearmed";
        REARM_RUNS.store(0, Ordering::SeqCst);
        register(name, rearm_entry);
        assert_eq!(initialize(name), 1);
        register(name, rearm_entry);
        assert_eq!(initialize(name), 1, "a re-registered body runs again");
        assert_eq!(REARM_RUNS.load(Ordering::SeqCst), 2);
    }
}
