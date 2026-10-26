#!/usr/bin/env python3
"""Regression gate for the Hydro micro-benchmarks.

Reads the medians written by ``benchmarks/hydro/run_micro.py`` and compares them
against ``benchmarks/hydro/thresholds.json``. Only workloads/implementations that
are present in *both* files are compared, so the gate keeps working when the
harness gains or loses a workload; missing entries are reported as skipped.

Exit codes: 0 = within thresholds (or nothing to compare), 1 = regression or
unusable input.

Usage:
    python3 benchmarks/hydro/run_micro.py       # fresh results/latest.json
    python3 scripts/check_benchmarks.py       # same as --check
    python3 scripts/check_benchmarks.py --check
    python3 scripts/check_benchmarks.py -v    # also list skipped workloads

    # Re-baseline after an intentional performance change:
    python3 scripts/check_benchmarks.py --update

Thresholds are deliberately generous (a regression has to be clearly outside
run-to-run noise). They are not a CI gate for shared/cloud runners: timing on
noisy virtual machines varies too much for that.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any, NoReturn

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_RESULTS = ROOT / "benchmarks" / "tea" / "results" / "latest.json"
DEFAULT_THRESHOLDS = ROOT / "benchmarks" / "tea" / "thresholds.json"

# Implementations that the gate enforces. CPython is measured for context in
# the baseline but intentionally not gated: it is not part of this repository.
GATED_IMPLS = ("hydro-jit", "hydro-aot")

DEFAULT_FACTOR = 1.75
# Concurrency and process-startup numbers are dominated by scheduling noise.
FACTORS = {
    "threads_serial": 3.0,
    "threads_parallel": 3.0,
    "pool_map": 3.0,
    "pool_serial": 3.0,
    "pool_parallel": 3.0,
    "process_queue": 3.0,
    "startup_hello": 4.0,
    "json_dumps": 2.0,
    "json_loads": 2.0,
}
# Small medians need absolute slack on top of the relative factor.
FLOOR_MS = 2.0


def die(message: str) -> NoReturn:
    print(f"check_benchmarks: {message}", file=sys.stderr)
    raise SystemExit(1)


def load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        die(f"{path} not found (run benchmarks/hydro/run_micro.py first)")
    except (OSError, json.JSONDecodeError) as exc:
        die(f"cannot read {path}: {exc}")


def load_results(path: Path) -> tuple[dict[tuple[str, str], float], dict[str, Any]]:
    payload = load_json(path)
    if not isinstance(payload, dict) or not isinstance(payload.get("results"), list):
        die(f"{path}: expected an object with a 'results' array")

    medians: dict[tuple[str, str], float] = {}
    for row in payload["results"]:
        if not isinstance(row, dict) or row.get("status") != "ok":
            continue
        workload = row.get("workload")
        impl = row.get("impl")
        median = row.get("median_ms")
        if not isinstance(workload, str) or not isinstance(impl, str):
            continue
        if not isinstance(median, (int, float)) or median <= 0:
            continue
        medians[(workload, impl)] = float(median)
    if not medians:
        die(f"{path}: no successful benchmark rows")
    return medians, payload.get("meta", {})


def load_thresholds(path: Path) -> dict[str, Any]:
    payload = load_json(path)
    if not isinstance(payload, dict) or not isinstance(payload.get("workloads"), dict):
        die(f"{path}: expected an object with a 'workloads' mapping")
    return payload


def threshold_for(entry: dict[str, Any]) -> float:
    max_ms = entry.get("max_ms")
    if not isinstance(max_ms, (int, float)) or max_ms <= 0:
        die("thresholds entry without a positive 'max_ms'")
    return float(max_ms)


def cmd_check(results_path: Path, thresholds_path: Path, verbose: bool) -> int:
    thresholds = load_thresholds(thresholds_path)
    medians, meta = load_results(results_path)

    checked: list[tuple[str, str, float, float]] = []
    skipped: list[tuple[str, str]] = []
    for workload in sorted(thresholds["workloads"]):
        entries = thresholds["workloads"][workload]
        if not isinstance(entries, dict):
            die(f"{thresholds_path}: workload '{workload}' must map to an object")
        for impl in GATED_IMPLS:
            entry = entries.get(impl)
            if not isinstance(entry, dict):
                continue
            median = medians.get((workload, impl))
            if median is None:
                skipped.append((workload, impl))
                continue
            checked.append((workload, impl, median, threshold_for(entry)))

    if not checked:
        print(
            f"no comparable benchmarks in {results_path} "
            f"(thresholds: {thresholds_path})"
        )
        return 0

    print(f"thresholds: {thresholds_path}")
    timestamp = meta.get("timestamp", "unknown")
    print(f"results:    {results_path} (timestamp {timestamp})")
    print()
    print(f"{'workload':<18} {'impl':<8} {'median ms':>10} {'max ms':>9} {'headroom':>9}  status")
    print("-" * 66)

    failures = 0
    for workload, impl, median, limit in checked:
        status = "ok" if median <= limit else "FAIL"
        failures += status == "FAIL"
        headroom = (limit - median) / median * 100.0
        print(
            f"{workload:<18} {impl:<8} {median:>10.3f} {limit:>9.3f} "
            f"{headroom:>8.1f}%  {status}"
        )

    if verbose:
        for workload, impl in skipped:
            print(f"{workload:<18} {impl:<8} {'-':>10} {'-':>9} {'-':>9}  skipped")
        thresholded = {
            (workload, impl)
            for workload, entries in thresholds["workloads"].items()
            for impl in GATED_IMPLS
            if impl in entries
        }
        for workload, impl in sorted(set(medians) - thresholded):
            if impl in GATED_IMPLS:
                print(
                    f"{workload:<18} {impl:<8} {'-':>10} {'-':>9} {'-':>9}  "
                    "no threshold"
                )

    print()
    print(
        f"{len(checked) - failures} passed, {failures} failed, "
        f"{len(skipped)} skipped"
    )
    if failures:
        print(
            "benchmark regression: median exceeds threshold "
            "(re-run run_micro.py; if the change is intentional, use --update)",
            file=sys.stderr,
        )
        return 1
    return 0


def cmd_update(results_path: Path, thresholds_path: Path, default_factor: float) -> int:
    medians, meta = load_results(results_path)

    workloads: dict[str, dict[str, Any]] = {}
    for (workload, impl), median in sorted(medians.items()):
        if impl not in GATED_IMPLS:
            continue
        factor = FACTORS.get(workload, default_factor)
        limit = max(median * factor, median + FLOOR_MS)
        workloads.setdefault(workload, {})[impl] = {
            "baseline_ms": round(median, 3),
            "max_ms": round(limit, 3),
        }

    payload = {
        "schema": 1,
        "generated": meta.get("timestamp"),
        "source": str(results_path.relative_to(ROOT))
        if results_path.is_relative_to(ROOT)
        else str(results_path),
        "default_factor": default_factor,
        "factors": FACTORS,
        "floor_ms": FLOOR_MS,
        "note": (
            "Generated from the Hydro micro-benchmark baseline. max_ms is the "
            "allowed median for a failing run; regenerate with "
            "`python3 scripts/check_benchmarks.py --update`."
        ),
        "workloads": workloads,
    }
    try:
        thresholds_path.write_text(
            json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    except OSError as exc:
        die(f"cannot write {thresholds_path}: {exc}")

    entries = sum(len(impls) for impls in workloads.values())
    print(f"wrote {entries} thresholds for {len(workloads)} workloads to {thresholds_path}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check Hydro micro-benchmarks against recorded thresholds."
    )
    parser.add_argument(
        "--results",
        type=Path,
        default=DEFAULT_RESULTS,
        help=f"benchmark results JSON (default: {DEFAULT_RESULTS.relative_to(ROOT)})",
    )
    parser.add_argument(
        "--thresholds",
        type=Path,
        default=DEFAULT_THRESHOLDS,
        help=f"threshold file (default: {DEFAULT_THRESHOLDS.relative_to(ROOT)})",
    )
    parser.add_argument(
        "--factor",
        type=float,
        default=None,
        help=(
            "default relative slack used by --update for workloads without a "
            f"specific factor (default: {DEFAULT_FACTOR})"
        ),
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--check",
        action="store_true",
        help="compare the results against the thresholds (the default mode)",
    )
    mode.add_argument(
        "--update",
        action="store_true",
        help="re-baseline thresholds from --results instead of checking",
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="also list workloads that were skipped",
    )
    args = parser.parse_args()

    factor = DEFAULT_FACTOR if args.factor is None else args.factor
    if factor <= 1.0:
        parser.error("--factor must be greater than 1.0")
    if args.update:
        return cmd_update(args.results, args.thresholds, factor)
    return cmd_check(args.results, args.thresholds, args.verbose)


if __name__ == "__main__":
    raise SystemExit(main())
