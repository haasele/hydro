#!/usr/bin/env bash
# Reproducible-build check for AOT artifacts.
#
# Compiles one source four times (twice without the build cache, twice with it)
# into different output directories and asserts the artifacts are byte
# identical and behave identically. Needs only an already-built `pyx` and the
# runtime static library: no cargo invocation.
#
# Usage: scripts/repro_check.sh [SOURCE]
#        scripts/repro_check.sh --pyx target/release/pyx examples/modules.py
#
# Exit codes: 0 = identical, 1 = nondeterminism or build/run failure,
# 2 = no usable pyx/runtime library.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_file="$root/examples/hello.py"
pyx="${PYX:-}"
explicit_runtime="${PYX_RUNTIME_LIB:-}"
keep=0

usage() {
    sed -n '2,13p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help)
            usage
            exit 0
            ;;
        --pyx)
            [[ $# -ge 2 ]] || { echo "repro_check: --pyx needs a path" >&2; exit 2; }
            pyx="$2"
            shift 2
            ;;
        --runtime-lib)
            [[ $# -ge 2 ]] || { echo "repro_check: --runtime-lib needs a path" >&2; exit 2; }
            explicit_runtime="$2"
            shift 2
            ;;
        --keep)
            keep=1
            shift
            ;;
        -*)
            echo "repro_check: unknown option '$1'" >&2
            exit 2
            ;;
        *)
            source_file="$1"
            shift
            ;;
    esac
done

if [[ -z "$pyx" ]]; then
    for candidate in "$root/target/release/pyx" "$root/target/debug/pyx"; do
        if [[ -x "$candidate" ]]; then
            pyx="$candidate"
            break
        fi
    done
fi
if [[ -z "$pyx" ]]; then
    pyx="$(command -v pyx || true)"
fi
if [[ -z "$pyx" || ! -x "$pyx" ]]; then
    echo "repro_check: no pyx binary found; pass --pyx <PATH> or set PYX" >&2
    exit 2
fi
pyx="$(cd "$(dirname "$pyx")" && pwd)/$(basename "$pyx")"

if [[ ! -f "$source_file" ]]; then
    echo "repro_check: source not found: $source_file" >&2
    exit 2
fi
# Same path spelling for every build: `program.source_name` is embedded in the
# generated code, so a relative and an absolute path produce different bytes.
source_file="$(cd "$(dirname "$source_file")" && pwd)/$(basename "$source_file")"

runtime_lib=""
if [[ -n "$explicit_runtime" ]]; then
    runtime_lib="$explicit_runtime"
else
    for candidate in \
        "$(dirname "$pyx")/libpyx_runtime.a" \
        "$root/target/release/libpyx_runtime.a" \
        "$root/target/debug/libpyx_runtime.a"; do
        if [[ -f "$candidate" ]]; then
            runtime_lib="$candidate"
            break
        fi
    done
fi
if [[ -z "$runtime_lib" ]]; then
    echo "repro_check: libpyx_runtime.a not found; pass --runtime-lib <PATH>" >&2
    exit 2
fi

work="$(mktemp -d)"
trap 'if [[ "$keep" == 1 ]]; then echo "repro_check: artifacts kept in $work"; else rm -rf "$work"; fi' EXIT
mkdir -p "$work/cwd"

build() {
    local output="$1"
    shift
    mkdir -p "$(dirname "$output")"
    (
        cd "$work/cwd"
        "$pyx" build "$source_file" --runtime-lib "$runtime_lib" -o "$output" "$@"
    )
}

hash_of() {
    sha256sum "$1" | cut -d' ' -f1
}

compare_artifacts() {
    local label="$1" left="$2" right="$3"
    if cmp -s "$left" "$right"; then
        echo "ok: $label identical ($(hash_of "$left"))"
        return 0
    fi
    echo "FAIL: $label differs" >&2
    echo "  left:  $left" >&2
    echo "  right: $right" >&2
    echo "  left  sha256: $(hash_of "$left")" >&2
    echo "  right sha256: $(hash_of "$right")" >&2
    echo "  first differing bytes (offset, left, right):" >&2
    cmp -l "$left" "$right" | head -5 | sed 's/^/    /' >&2 || true
    local left_size right_size
    left_size="$(stat -c %s "$left")"
    right_size="$(stat -c %s "$right")"
    echo "  sizes: left=$left_size right=$right_size" >&2
    return 1
}

compare_run() {
    local label="$1" left="$2" right="$3"
    local left_status=0 right_status=0
    "$left" >"$work/left.out" 2>"$work/left.err" || left_status=$?
    "$right" >"$work/right.out" 2>"$work/right.err" || right_status=$?
    if [[ "$left_status" != "$right_status" ]]; then
        echo "FAIL: $label exit codes differ ($left_status vs $right_status)" >&2
        return 1
    fi
    if ! cmp -s "$work/left.out" "$work/right.out"; then
        echo "FAIL: $label stdout differs" >&2
        diff -u "$work/left.out" "$work/right.out" >&2 || true
        return 1
    fi
    if ! cmp -s "$work/left.err" "$work/right.err"; then
        echo "FAIL: $label stderr differs" >&2
        diff -u "$work/left.err" "$work/right.err" >&2 || true
        return 1
    fi
    echo "ok: $label behavior identical (exit $left_status)"
}

echo "pyx:         $pyx"
echo "runtime lib: $runtime_lib"
echo "source:      $source_file"
echo "cc:          ${CC:-cc} ($("${CC:-cc}" --version 2>/dev/null | head -1 || echo unknown))"
echo

status=0
build "$work/build-a/program" --no-cache || status=1
build "$work/build-b/program" --no-cache || status=1
if [[ "$status" -eq 0 ]]; then
    compare_artifacts "uncached builds (different output paths)" \
        "$work/build-a/program" "$work/build-b/program" || status=1
fi
if [[ "$status" -eq 0 ]]; then
    compare_run "uncached builds" "$work/build-a/program" "$work/build-b/program" || status=1
fi

# The cache stores the already-linked binary; a cache hit must not change bytes.
if [[ "$status" -eq 0 ]]; then
    build "$work/build-c/program" || status=1
    build "$work/build-d/program" || status=1
    if [[ "$status" -eq 0 ]]; then
        compare_artifacts "cached builds (fresh link vs cache hit)" \
            "$work/build-c/program" "$work/build-d/program" || status=1
    fi
    if [[ "$status" -eq 0 ]]; then
        compare_artifacts "cached vs uncached build" \
            "$work/build-a/program" "$work/build-c/program" || status=1
    fi
fi

if [[ "$status" -ne 0 ]]; then
    echo >&2
    echo "repro_check: artifacts are not reproducible on this host." >&2
    echo "report the hashes above; docs/REPRODUCIBLE.md lists the inputs that" >&2
    echo "are part of the contract." >&2
    exit 1
fi

echo
echo "repro_check: all builds identical."
