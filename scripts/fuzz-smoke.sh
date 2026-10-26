#!/usr/bin/env bash
# Bounded fuzz smoke run: every target runs for a small fixed budget and a
# crash fails the script. This is what CI runs; it is NOT the 24 h campaign.
#
#   ./scripts/fuzz-smoke.sh              # bounded smoke (~20 s)
#   FUZZ_ITERATIONS=2000 ./scripts/fuzz-smoke.sh
#
# Two backends, same harness code (fuzz/src/lib.rs):
#
# 1. libFuzzer (`cargo-fuzz` + nightly), bounded by `-runs`/`-max_total_time`:
#      cargo install cargo-fuzz
#      rustup toolchain install nightly
#      ./scripts/fuzz-smoke.sh
#
# 2. Stable fallback (`fuzz/stable`, no nightly, no libfuzzer-sys), used
#    automatically when cargo-fuzz/nightly is missing:
#      cargo build --release --manifest-path fuzz/stable/Cargo.toml
#      fuzz/stable/target/release/hydro-fuzz-stable frontend_pipeline --iterations 50
#      fuzz/stable/target/release/hydro-fuzz-stable runtime_gc --iterations 2000
#
# Long campaign (resumable: the corpus lives in fuzz/corpus/<target>):
#      cargo +nightly fuzz run frontend_pipeline -- -max_total_time=86400 -timeout=10
#      cargo +nightly fuzz run runtime_gc        -- -max_total_time=86400 -timeout=10
#   or, without nightly, a long stable soak:
#      fuzz/stable/target/release/hydro-fuzz-stable runtime_gc --iterations 500000
#
# Environment:
#   FUZZ_SECONDS     per-target libFuzzer wall clock (default 15)
#   FUZZ_RUNS        per-target libFuzzer iteration cap (default 2000)
#   FUZZ_ITERATIONS  stable-fallback generated inputs per target (default 200)
#   FUZZ_ALLOW_NONE  "1" runs the runtime target with `-s none` (use this if
#                    AddressSanitizer reports on the fiber stack switch)
set -euo pipefail

cd "$(dirname "$0")/.."

TARGETS=(frontend_pipeline runtime_gc)
RUNS="${FUZZ_RUNS:-2000}"
SECONDS_PER_TARGET="${FUZZ_SECONDS:-15}"
STABLE_ITERATIONS="${FUZZ_ITERATIONS:-200}"

have_libfuzzer() {
    command -v cargo-fuzz >/dev/null 2>&1 || return 1
    if command -v rustup >/dev/null 2>&1; then
        rustup toolchain list 2>/dev/null | grep -qi '^nightly' || return 1
    else
        # Without rustup, only assume nightly when cargo-fuzz is present and
        # the active toolchain is nightly.
        rustc -vV 2>/dev/null | grep -q '^release: .*nightly' || return 1
    fi
    return 0
}

if have_libfuzzer; then
    echo "== fuzz smoke (libFuzzer): ${RUNS} runs / ${SECONDS_PER_TARGET}s per target =="
    for target in "${TARGETS[@]}"; do
        sanitizer=()
        if [[ "$target" == "runtime_gc" && "${FUZZ_ALLOW_NONE:-0}" == "1" ]]; then
            # Hydro switches fiber stacks without ASan annotations.
            sanitizer=(-s none)
        fi
        echo "-- $target"
        cargo +nightly fuzz run "$target" "${sanitizer[@]}" -- \
            "-runs=${RUNS}" "-max_total_time=${SECONDS_PER_TARGET}" -timeout=10
    done
    echo "== fuzz smoke: ok =="
    exit 0
fi

echo "== fuzz smoke (stable fallback): ${STABLE_ITERATIONS} inputs for runtime_gc =="
cargo build --release --manifest-path fuzz/stable/Cargo.toml
runner="fuzz/stable/target/release/hydro-fuzz-stable"

# Codegen runs per input, so the frontend budget is an order of magnitude
# smaller than the runtime budget; both stay well under a second per target.
frontend_iterations=$(( STABLE_ITERATIONS / 4 + 1 ))
"$runner" frontend_pipeline --iterations "$frontend_iterations"
"$runner" runtime_gc --iterations "$STABLE_ITERATIONS"

echo "== fuzz smoke: ok =="
