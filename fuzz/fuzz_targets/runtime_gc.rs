//! libFuzzer target: runtime allocation, containers, cycles, fibers and GC.
//!
//! Run:
//!
//! ```text
//! cargo install cargo-fuzz
//! cargo +nightly fuzz run runtime_gc -- -runs=100000 -timeout=10
//! ```
//!
//! The target is single-threaded by construction: it never spawns a thread,
//! process or pool, because a forked child inside a fuzzing process would not
//! be meaningful (and would fight the sanitizer). Shutdown and process paths
//! are covered by `crates/pyx-runtime/tests/shutdown.rs`.
//!
//! If AddressSanitizer trips over the fiber stack switch (PyX switches stacks
//! without ASan annotations), run the target with `-s none`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pyx_fuzz::runtime_gc;

fuzz_target!(|data: &[u8]| {
    runtime_gc(data);
});
