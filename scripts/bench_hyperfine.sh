#!/usr/bin/env bash
# Hyperfine adapter for the CPython benchmark harnesses.
#
# Every selected `benchmarks/bench_*.py` runs through `uv run --script` under
# hyperfine. Each timed run also validates the benchmark's JSON output (section
# rows and positive metrics), so a benchmark that silently stops measuring
# fails instead of producing an empty timing.
#
# Missing `hyperfine`/`uv` is a skip by default (exit 0); `--strict` turns it
# into an error (exit 1). `bench_all.py` (orchestrator) and `bench_hydro.py`
# (already measures CPython/JIT/AOT itself) are intentionally not wrapped.
#
# Usage:
#   scripts/bench_hyperfine.sh                 # quick profile, all harnesses
#   scripts/bench_hyperfine.sh loops startup   # subset
#   scripts/bench_hyperfine.sh --full --runs 5 --warmup 2
#   scripts/bench_hyperfine.sh --out bench_results/hyperfine
#   scripts/bench_hyperfine.sh --list
set -euo pipefail

SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
    sed -n '2,18p' "$SELF" | sed 's/^# \{0,1\}//'
}

# --- internal exec mode: run one benchmark and validate its JSON -------------
if [[ "${1:-}" == "--exec" ]]; then
    shift
    key="${1:?--exec needs a benchmark key}"
    json="${2:?--exec needs a JSON path}"
    script="${3:?--exec needs a script path}"
    shift 3
    uv run --script "$script" "$@" --json "$json"
    python3 - "$json" "$key" <<'PY'
import json
import sys

path, key = sys.argv[1], sys.argv[2]
try:
    with open(path, encoding="utf-8") as handle:
        data = json.load(handle)
except (OSError, json.JSONDecodeError) as exc:
    sys.exit(f"bench_hyperfine: invalid JSON from {key}: {exc}")
if not isinstance(data, dict) or data.get("benchmark") != key:
    sys.exit(f"bench_hyperfine: {path} is not a '{key}' result")

sections = data.get("sections")
if not isinstance(sections, list) or not sections:
    sys.exit(f"bench_hyperfine: {key} produced no sections")

rows = 0
for section in sections:
    if not isinstance(section, dict) or not isinstance(section.get("rows"), list):
        sys.exit(f"bench_hyperfine: {key} has a malformed section")
    for row in section["rows"]:
        if not isinstance(row, dict):
            continue
        if any(
            isinstance(value, (int, float)) and not isinstance(value, bool) and value > 0
            for value in row.values()
        ):
            rows += 1
if rows == 0:
    headlines = data.get("headlines") or []
    if not any(
        isinstance(h.get("value"), (int, float)) and h["value"] > 0
        for h in headlines
        if isinstance(h, dict)
    ):
        sys.exit(f"bench_hyperfine: {key} produced no positive measurements")
print(f"bench_hyperfine: validated {key} ({rows} measured rows)", file=sys.stderr)
PY
    exit 0
fi

# --- options -----------------------------------------------------------------
profile=quick
runs=3
warmup=1
out=""
strict=0
keys=()

while (($#)); do
    case "$1" in
        --quick) profile=quick ;;
        --full) profile=full ;;
        --runs)
            [[ $# -ge 2 && "$2" =~ ^[0-9]+$ && "$2" -gt 0 ]] || {
                echo "bench_hyperfine: --runs needs a positive integer" >&2
                exit 2
            }
            runs="$2"
            shift
            ;;
        --warmup)
            [[ $# -ge 2 && "$2" =~ ^[0-9]+$ ]] || {
                echo "bench_hyperfine: --warmup needs an integer" >&2
                exit 2
            }
            warmup="$2"
            shift
            ;;
        --out)
            [[ $# -ge 2 ]] || {
                echo "bench_hyperfine: --out needs a directory" >&2
                exit 2
            }
            out="$2"
            shift
            ;;
        --strict) strict=1 ;;
        --list)
            printf '%s\n' "loops csv startup request fastapi"
            exit 0
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        --)
            shift
            keys+=("$@")
            break
            ;;
        -*)
            echo "bench_hyperfine: unknown option '$1'" >&2
            exit 2
            ;;
        *) keys+=("$1") ;;
    esac
    shift
done

if ((${#keys[@]} == 0)); then
    keys=(loops csv startup request fastapi)
fi

for key in "${keys[@]}"; do
    case "$key" in
        loops | csv | startup | request | fastapi) ;;
        *)
            echo "bench_hyperfine: unknown benchmark '$key' (see --list)" >&2
            exit 2
            ;;
    esac
done

# --- tool detection ----------------------------------------------------------
skip() {
    echo "SKIP: $*" >&2
    if ((strict)); then
        exit 1
    fi
    exit 0
}

command -v python3 >/dev/null 2>&1 || {
    echo "bench_hyperfine: python3 is required" >&2
    exit 1
}
command -v hyperfine >/dev/null 2>&1 ||
    skip "hyperfine not found (install: cargo install hyperfine / apt install hyperfine)"
command -v uv >/dev/null 2>&1 ||
    skip "uv not found (install: https://docs.astral.sh/uv/)"

if [[ -z "$out" ]]; then
    out="$ROOT/bench_results/hyperfine-$(date +%Y%m%d-%H%M%S)"
fi
mkdir -p "$out/raw"

# --- per-benchmark command + profile ----------------------------------------
SCRIPT=""
ARGS=()
configure() {
    local key="$1"
    ARGS=()
    case "$key" in
        loops)
            SCRIPT="$ROOT/benchmarks/bench_loops.py"
            if [[ "$profile" == full ]]; then
                ARGS=(--n 500000 --repeat 1)
            else
                ARGS=(--n 100000 --repeat 1 --group sum)
            fi
            ;;
        csv)
            SCRIPT="$ROOT/benchmarks/bench_csv.py"
            if [[ "$profile" == full ]]; then
                ARGS=(--rows 500000 --repeat 1)
            else
                ARGS=(--rows 50000 --repeat 1 --no-pandas)
            fi
            ;;
        startup)
            SCRIPT="$ROOT/benchmarks/bench_startup.py"
            if [[ "$profile" == full ]]; then
                ARGS=(--runs 3)
            else
                ARGS=(--runs 1 --modules json asyncio)
            fi
            ;;
        request)
            SCRIPT="$ROOT/benchmarks/bench_request_handling.py"
            if [[ "$profile" == full ]]; then
                ARGS=(--duration 2 --warmup 0.3 --repeat 1)
            else
                ARGS=(--duration 1 --warmup 0.2 --repeat 1 --skip-live --no-uvloop)
            fi
            ;;
        fastapi)
            SCRIPT="$ROOT/benchmarks/bench_fastapi_throughput.py"
            if [[ "$profile" == full ]]; then
                ARGS=(--mode both --duration 2 --warmup 0.3)
            else
                ARGS=(--mode asgi --duration 1 --warmup 0.2)
            fi
            ;;
    esac
    [[ -f "$SCRIPT" ]] || {
        echo "bench_hyperfine: missing $SCRIPT" >&2
        exit 1
    }
}

echo "bench_hyperfine: profile=$profile runs=$runs warmup=$warmup"
echo "bench_hyperfine: output=$out"

cd "$ROOT"
for key in "${keys[@]}"; do
    configure "$key"
    raw="$out/raw/$key.json"
    cmd=("$SELF" --exec "$key" "$raw" "$SCRIPT" "${ARGS[@]}")
    printf -v command '%q ' "${cmd[@]}"
    echo "== $key =="
    hyperfine \
        --warmup "$warmup" \
        --runs "$runs" \
        --shell bash \
        --command-name "$key" \
        --export-json "$out/hyperfine-$key.json" \
        "$command" || {
        echo "bench_hyperfine: hyperfine failed for '$key'" >&2
        exit 1
    }
done

# --- summary -----------------------------------------------------------------
python3 - "$out" <<'PY'
import json
import sys
from pathlib import Path

out = Path(sys.argv[1])
rows = []
for path in sorted(out.glob("hyperfine-*.json")):
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
        result = payload["results"][0]
    except (OSError, KeyError, IndexError, json.JSONDecodeError) as exc:
        sys.exit(f"bench_hyperfine: cannot summarise {path}: {exc}")
    rows.append(
        (
            result.get("command", path.stem.removeprefix("hyperfine-")),
            result.get("median", 0.0),
            result.get("mean", 0.0),
            result.get("min", 0.0),
            result.get("max", 0.0),
            result.get("stddev", 0.0),
        )
    )
lines = [
    "# Hyperfine benchmark report",
    "",
    "| benchmark | median (s) | mean (s) | min (s) | max (s) | stddev (s) |",
    "|---|---:|---:|---:|---:|---:|",
]
for name, median, mean, low, high, stddev in rows:
    lines.append(
        f"| {name} | {median:.3f} | {mean:.3f} | {low:.3f} | {high:.3f} | {stddev:.3f} |"
    )
lines.append("")
lines.append("Generated by `scripts/bench_hyperfine.sh`; timings include `uv` startup.")
(out / "summary.md").write_text("\n".join(lines), encoding="utf-8")
print(f"bench_hyperfine: wrote {out / 'summary.md'}")
PY

echo "bench_hyperfine: done"
