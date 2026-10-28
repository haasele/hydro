#!/usr/bin/env bash
# Hydro runtime/compiler gate. The Tea toolchain is a separate repository and
# owns the CLI, E2E and AOT packaging gates.
set -euo pipefail

export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")/.."

# Cargo builds with one job per logical CPU by default. Make the test runner
# explicit too: each test binary gets one worker per logical CPU, while Cargo
# still executes test binaries sequentially (use `cargo nextest` locally for
# cross-binary parallelism). Override with HYDRO_TEST_JOBS on constrained hosts.
JOBS="${HYDRO_TEST_JOBS:-$(nproc 2>/dev/null || echo 1)}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-$JOBS}"

echo "== fmt =="
cargo fmt --all -- --check

echo "== clippy =="
cargo clippy --workspace --all-targets -- -D warnings

echo "== build =="
cargo build -p hydro-runtime

echo "== unit/integration tests (jobs=$JOBS) =="
# Cargo executes test binaries sequentially even when individual tests run in
# parallel. nextest runs every test in its own process and schedules the whole
# workspace across all cores, which is the default gate. The fallback keeps
# constrained hosts working but is visibly slower and cross-binary serial.
if command -v cargo-nextest >/dev/null 2>&1; then
    cargo nextest run --workspace --no-fail-fast
else
    echo "warning: cargo-nextest not found; cargo test runs binaries sequentially" >&2
    cargo test --workspace --no-fail-fast -- --test-threads="$JOBS"
fi

echo "== fuzz smoke =="
./scripts/fuzz-smoke.sh

echo "== CPython manifest (offline) =="
python3 scripts/cpython_compat.py --check-manifest

echo "ALL GREEN (Hydro)"
