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
//!   raise `OverflowError` (documented Phase-1 limit, no BigInt).
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

mod concurrency;
mod core;
mod csv;
mod gc;
mod modules;
mod support;

// The historical flat module paths (`crate::value`, `crate::heap`,
// `crate::process`, ...) remain valid through these crate-root re-exports.
pub(crate) use self::concurrency::{pool, process, spawn};
pub(crate) use self::core::{builtins, container, exc, format, iter, methods, ops, value};
pub(crate) use self::gc::heap;
pub(crate) use self::support::serialize;

/// Records the canonical source path of the JIT-compiled program.
///
/// A `multiprocessing` spawn start re-executes the current executable with
/// `__spawn <source>`; without a recorded path (an AOT binary) the executable
/// is started without extra arguments.
pub fn set_jit_source(path: impl Into<std::path::PathBuf>) {
    process::set_jit_source(path.into());
}

pub mod abi;
