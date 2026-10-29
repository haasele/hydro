//! Hydro runtime — value model, operators, builtins, exceptions, tracebacks.
//!
//! The [`abi`] module is the C ABI used by generated code. Its signatures are
//! frozen by `docs/ARCHITECTURE.md` §3 and must not change without updating
//! that document, the codegen and all tests.
//!
//! Heap objects are owned by the incremental conservative mark-&-sweep
//! collector ([`gc::heap`], ARCHITECTURE §7).
//!
//! Implementation notes:
//! - Values are tagged `i64`s: `000` int (`v >> 3`), `001` special
//!   (`None = 1`, `False = 9`, `True = 17`), `010` heap pointer (`v & !7`).
//! - Integers use the 61 bits available to the tag; results outside that range
//!   promote to a heap `BigInt` (`num-bigint`).
//! - Every fallible ABI call reports errors through a thread-local pending
//!   exception and returns a dummy value; it never panics on user input.
//!
//! Module layout:
//! - [`core`]: tagged value model, operators, containers, iteration, builtins,
//!   methods and exceptions.
//! - [`gc`]: the conservative mark-&-sweep heap.
//! - [`modules`]: native standard-library modules.
//! - [`module_init`]: on-demand initialisation of compiled module bodies
//!   (registered by codegen, driven by `hydro_initialize_module`).
//! - [`concurrency`]: `multiprocessing` process spawning, pools and workers.
//! - [`support`]: serialization helpers (pickle, JSON).
//! - `csv`: CSV reader/writer support.
//! - `profile`: process-global function-entry counters for profile-guided
//!   codegen (exposed through [`abi`]).

mod capi;
mod concurrency;
mod core;
mod csv;
mod gc;
mod module_init;
mod modules;
mod native;
mod native_manifest;
mod profile;
mod support;

// The historical flat module paths (`crate::value`, `crate::heap`,
// `crate::process`, ...) remain valid through these crate-root re-exports.
pub(crate) use self::concurrency::{pool, process, spawn};
pub(crate) use self::core::{builtins, container, exc, fiber, format, iter, methods, ops, value};
pub(crate) use self::gc::heap;
/// Precise shadow-stack root registry.
///
/// Hidden because it is an implementation boundary between generated code,
/// the collector and `tests/precise_roots.rs`, not part of the frozen C ABI
/// (the C entry points live in [`abi`]).
#[doc(hidden)]
pub mod precise_roots {
    pub use crate::gc::roots::*;

    // Persistent-root entry points. The collector API itself is crate
    // private; these hidden forwarders let the integration tests exercise
    // the exact slot semantics without widening the public surface.
    /// Registers a persistent root and returns its slot index.
    pub fn add_persistent_root(value: i64) -> usize {
        crate::gc::heap::add_persistent_root(value)
    }

    /// Overwrites the persistent root in `slot`.
    pub fn set_persistent_root(slot: usize, value: i64) {
        crate::gc::heap::set_persistent_root(slot, value);
    }

    /// Removes the persistent root in `slot`.
    pub fn remove_persistent_root(slot: usize) {
        crate::gc::heap::remove_persistent_root(slot);
    }
}
// Public collector statistics (used by `tea gc`). The heap module itself
// stays private; only the snapshot API is re-exported.
pub use self::gc::heap::{gc_stats, reset_gc_stats, GcStats};
pub(crate) use self::support::serialize;

/// Runs the tests registered by a compiled Hydro test module.
///
/// The generated module body has already executed; this discovers functions
/// registered by codegen (`test_*` entry functions and `Test*.test_*` methods),
/// runs them, prints a pytest-style report and returns the pytest exit code
/// (0 pass, 1 failure, 5 no tests). `keyword` is the `-k` expression and
/// `maxfail` stops the run after that many failures.
pub fn run_pytest_with(keyword: Option<&str>, maxfail: Option<usize>) -> i32 {
    modules::pytest::run(keyword, maxfail)
}

/// [`run_pytest_with`] without a keyword filter or failure limit.
pub fn run_pytest() -> i32 {
    run_pytest_with(None, None)
}

/// Records the canonical source path of the JIT-compiled program.
///
/// A `multiprocessing` spawn start re-executes the current executable with
/// `__spawn <source>`; without a recorded path (an AOT binary) the executable
/// is started without extra arguments.
pub fn set_jit_source(path: impl Into<std::path::PathBuf>) {
    process::set_jit_source(path.into());
}

/// Process-wide `sys.argv`, set before a JIT program starts or filled from
/// the process arguments by [`abi::hydro_runtime_init`] for AOT binaries.
static ARGV: std::sync::LazyLock<std::sync::Mutex<Option<Vec<String>>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

/// Records the program name and its arguments as `sys.argv`.
///
/// The JIT front end calls this before running the program; an explicit
/// setting is preserved across [`abi::hydro_runtime_init`].
pub fn set_argv(program: String, args: Vec<String>) {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(program);
    argv.extend(args);
    let mut store = ARGV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *store = Some(argv);
}

/// Fills `sys.argv` from the process arguments unless a caller set it.
pub(crate) fn init_argv() {
    let mut store = ARGV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if store.is_none() {
        *store = Some(
            std::env::args_os()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect(),
        );
    }
}

/// Returns a copy of `sys.argv` (empty before initialization).
pub(crate) fn argv() -> Vec<String> {
    let store = ARGV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    store.clone().unwrap_or_default()
}

pub mod abi;

/// Interpreter bridge: function values backed by the tree-walk interpreter.
///
/// The runtime calls the dispatcher installed through
/// [`host::set_dispatch`]; the interpreter registers its own program registry
/// and creates [`host::function_new`] values for IR functions.
pub mod host;
