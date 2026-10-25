#!/usr/bin/env bash
# Parallel stable-fallback fuzz soak.
#
# The stable harness is single-threaded per process, so saturate every CPU
# with independent seeds. Each worker gets its own generated-input stream and
# log; any crash fails the whole run.
#
#   ./scripts/fuzz-parallel.sh
#   FUZZ_WORKERS=8 FUZZ_ITERATIONS=100000 ./scripts/fuzz-parallel.sh
#   FUZZ_TARGET=frontend_pipeline FUZZ_WORKERS=16 ./scripts/fuzz-parallel.sh
#
# This is the bounded CI/local smoke campaign, not the 24h libFuzzer campaign:
#   cargo +nightly fuzz run runtime_gc -- -max_total_time=86400 -timeout=10
set -euo pipefail

cd "$(dirname "$0")/.."

TARGET="${FUZZ_TARGET:-runtime_gc}"
WORKERS="${FUZZ_WORKERS:-$(nproc)}"
ITERATIONS="${FUZZ_ITERATIONS:-100000}"
BASE_SEED="${FUZZ_SEED:-242424}"
LOG_DIR="${FUZZ_LOG_DIR:-fuzz/logs}"
RUNNER="fuzz/stable/target/release/pyx-fuzz-stable"

mkdir -p "$LOG_DIR"
cargo build --release --manifest-path fuzz/stable/Cargo.toml

pids=()
logs=()
cleanup() {
    for pid in "${pids[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
}
trap cleanup EXIT INT TERM

echo "== parallel fuzz: target=$TARGET workers=$WORKERS iterations/worker=$ITERATIONS =="
for ((worker = 0; worker < WORKERS; worker++)); do
    seed=$((BASE_SEED + worker * 104729))
    log="$LOG_DIR/${TARGET}-${worker}-${seed}.log"
    logs+=("$log")
    "$RUNNER" "$TARGET" --iterations "$ITERATIONS" --seed "$seed" >"$log" 2>&1 &
    pids+=("$!")
done

failed=0
for ((worker = 0; worker < WORKERS; worker++)); do
    if ! wait "${pids[$worker]}"; then
        echo "worker $worker failed; tail of ${logs[$worker]}:" >&2
        tail -20 "${logs[$worker]}" >&2 || true
        failed=1
    fi
done

if ((failed)); then
    echo "== parallel fuzz: FAILED ==" >&2
    exit 1
fi

echo "== parallel fuzz: ok ($((WORKERS * ITERATIONS)) inputs) =="
