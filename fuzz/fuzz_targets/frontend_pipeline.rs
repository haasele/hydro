//! libFuzzer target: parse -> lower -> type-check -> native codegen.
//!
//! Run:
//!
//! ```text
//! cargo install cargo-fuzz
//! cargo +nightly fuzz run frontend_pipeline -- -runs=10000 -timeout=10
//! ```
//!
//! The bounded smoke run lives in `scripts/fuzz-smoke.sh`. The program is
//! never executed, so a candidate cannot loop, read the filesystem or open a
//! socket.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pyx_fuzz::frontend_pipeline;

fuzz_target!(|data: &[u8]| {
    frontend_pipeline(data);
});
