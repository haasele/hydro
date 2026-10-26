#!/usr/bin/env bash
# Hydro runtime/compiler gate. The Tea toolchain is a separate repository and
# owns the CLI, E2E and AOT packaging gates.
set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")/.."

echo "== fmt =="
cargo fmt --all -- --check

echo "== clippy =="
cargo clippy --workspace --all-targets -- -D warnings

echo "== build =="
cargo build -p hydro-runtime

echo "== unit/integration tests =="
cargo test --workspace

echo "== fuzz smoke =="
./scripts/fuzz-smoke.sh

echo "== CPython manifest (offline) =="
python3 scripts/cpython_compat.py --check-manifest

echo "ALL GREEN (Hydro)"
