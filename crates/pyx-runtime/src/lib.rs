//! PyX runtime — value model, operators, builtins, exceptions, tracebacks.
//!
//! The [`abi`] module is the C ABI used by generated code. Its signatures are
//! frozen by `docs/ARCHITECTURE.md` §3 and must not change without updating
//! that document, the codegen and all tests.
//!
//! Phase 1 allocation policy was to leak heap objects. Since Phase 2.1 an
//! interim conservative mark-&-sweep collector (see [`gc::heap`] and
//! ARCHITECTURE §7) owns them.
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
//! - [`concurrency`]: `multiprocessing` process spawning, pools and workers.
//! - [`support`]: serialization helpers (pickle, JSON).
//! - `csv`: CSV reader/writer support.

mod capi;
mod concurrency;
mod core;
mod csv;
mod gc;
mod modules;
mod support;

// The historical flat module paths (`crate::value`, `crate::heap`,
// `crate::process`, ...) remain valid through these crate-root re-exports.
pub(crate) use self::concurrency::{pool, process, spawn};
pub(crate) use self::core::{builtins, container, exc, fiber, format, iter, methods, ops, value};
pub(crate) use self::gc::heap;
// Public collector statistics (used by `pyx gc`). The heap module itself
// stays private; only the snapshot API is re-exported.
pub use self::gc::heap::{gc_stats, reset_gc_stats, GcStats};
pub(crate) use self::support::serialize;

/// Records the canonical source path of the JIT-compiled program.
///
/// A `multiprocessing` spawn start re-executes the current executable with
/// `__spawn <source>`; without a recorded path (an AOT binary) the executable
/// is started without extra arguments.
pub fn set_jit_source(path: impl Into<std::path::PathBuf>) {
    process::set_jit_source(path.into());
}

/// Process-wide `sys.argv`, set before a JIT program starts or filled from
/// the process arguments by [`abi::pyx_runtime_init`] for AOT binaries.
static ARGV: std::sync::LazyLock<std::sync::Mutex<Option<Vec<String>>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

/// Records the program name and its arguments as `sys.argv`.
///
/// The JIT front end calls this before running the program; an explicit
/// setting is preserved across [`abi::pyx_runtime_init`].
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
