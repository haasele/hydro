#!/usr/bin/env python3
"""Hydro micro-benchmark harness (CPython vs Hydro JIT vs Hydro AOT).

Runs the import-free programs in this directory, verifies identical output and
records wall-clock medians, optional peak RSS and AOT build time. This is a
baseline harness for the Hydro subset, not a replacement for the CPython/uv
harnesses in benchmarks/.

Usage:
    cargo build --release -p hydro-runtime
    python3 benchmarks/hydro/run_micro.py [--repeats 5] [--no-build]
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
HYD = ROOT.parent / "tea" / "target" / "release" / "tea"
RESULTS = HERE / "results"

WORKLOADS = [
    ("while_sum", "while_sum.py", 5),
    ("for_sum", "for_sum.py", 5),
    ("range_sum", "range_sum.py", 5),
    ("list_append", "list_append.py", 5),
    ("dict_insert", "dict_insert.py", 5),
    ("string_concat", "string_concat.py", 5),
    ("list_comp", "list_comp.py", 5),
    ("filter_comp", "filter_comp.py", 5),
    ("sum_genexp", "sum_genexp.py", 5),
    ("map_lambda", "map_lambda.py", 5),
    ("filter_lambda", "filter_lambda.py", 5),
    ("lambda_call", "lambda_call.py", 5),
    ("reduce_add", "reduce_add.py", 5),
    ("math_loop", "math_loop.py", 5),
    ("json_dumps", "json_dumps.py", 5),
    ("json_loads", "json_loads.py", 5),
    ("threads_serial", "threads_serial.py", 5),
    ("threads_parallel", "threads_parallel.py", 5),
    ("pool_map", "pool_map.py", 5),
    ("pool_serial", "pool_serial.py", 5),
    ("pool_parallel", "pool_parallel.py", 5),
    ("process_queue", "process_queue.py", 5),
]

STARTUP = ("startup_hello", "hello.py", 7)


def log(message: str) -> None:
    print(message, file=sys.stderr, flush=True)


def run_once(cmd: list[str], cwd: Path, timeout: float) -> tuple[str, float, int | None]:
    """Runs a command, returns (stdout, seconds, max_rss_kb)."""
    start = time.perf_counter()
    proc = subprocess.run(
        cmd,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=timeout,
    )
    elapsed = time.perf_counter() - start
    if proc.returncode != 0:
        raise RuntimeError(
            f"command failed ({proc.returncode}): {' '.join(cmd)}\n{proc.stderr[-2000:]}"
        )
    return proc.stdout, elapsed, None


def peak_rss_kb(cmd: list[str], cwd: Path, timeout: float) -> int | None:
    time_bin = shutil.which("/usr/bin/time") or shutil.which("time")
    if not time_bin:
        return None
    with tempfile.NamedTemporaryFile("w+", suffix=".txt") as handle:
        wrapped = [time_bin, "-f", "%M", "-o", handle.name, *cmd]
        subprocess.run(
            wrapped,
            cwd=cwd,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=timeout,
        )
        handle.seek(0)
        text = handle.read().strip()
    try:
        return int(text.splitlines()[-1].strip())
    except (ValueError, IndexError):
        return None


def measure(cmd: list[str], cwd: Path, repeats: int, timeout: float) -> dict:
    # Warmup (also validates the command).
    out, _, _ = run_once(cmd, cwd, timeout)
    times = []
    rss = None
    for _ in range(repeats):
        out2, elapsed, _ = run_once(cmd, cwd, timeout)
        if out2 != out:
            raise RuntimeError(f"non-deterministic output for {' '.join(cmd)}")
        times.append(elapsed)
    try:
        rss = peak_rss_kb(cmd, cwd, timeout)
    except (OSError, subprocess.SubprocessError):
        rss = None
    return {
        "stdout": out,
        "min_ms": min(times) * 1000,
        "median_ms": statistics.median(times) * 1000,
        "max_ms": max(times) * 1000,
        "rss_kb": rss,
    }


def build_release() -> None:
    log("building Hydro release (cargo build --release -p hydro-runtime)")
    subprocess.run(
        ["cargo", "build", "--release", "-p", "hydro-runtime"],
        cwd=ROOT,
        check=True,
    )


def aot_build(source: Path, out: Path, timeout: float) -> float:
    start = time.perf_counter()
    proc = subprocess.run(
        [str(HYD), "build", str(source), "-o", str(out)],
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=timeout,
    )
    elapsed = time.perf_counter() - start
    if proc.returncode != 0:
        raise RuntimeError(f"AOT build failed for {source.name}:\n{proc.stderr[-2000:]}")
    return elapsed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--timeout", type=float, default=120.0)
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--quick", action="store_true", help="fewer repetitions")
    args = parser.parse_args()

    repeats = 2 if args.quick else args.repeats
    if not args.no_build and not HYD.exists():
        build_release()
    if not HYD.exists():
        log(f"error: {HYD} not found (build with --release)")
        return 2
    runtime_lib = HYD.parent / "libhydro_runtime.a"
    if not runtime_lib.exists():
        log(f"error: {runtime_lib} missing; cargo build --release -p hydro-runtime")
        return 2

    results: list[dict] = []
    failures = 0
    with tempfile.TemporaryDirectory(prefix="hydro-bench-") as tmp:
        tmpdir = Path(tmp)
        log("verifying outputs against CPython…")
        baseline: dict[str, str | None] = {}
        for key, filename, _ in [*WORKLOADS, STARTUP]:
            try:
                out, _, _ = run_once([sys.executable, str(HERE / filename)], ROOT, args.timeout)
                baseline[key] = out
            except Exception as exc:  # noqa: BLE001 - a broken CPython reference must not abort the report
                log(f"  CPython reference failed for {key}: {exc}")
                baseline[key] = None

        for key, filename, _ in WORKLOADS:
            source = HERE / filename
            if baseline[key] is None:
                log(f"skipping {key}: CPython reference failed")
                results.append({"workload": key, "impl": "cpython", "build_ms": None,
                                "status": "error", "error": "CPython reference failed"})
                continue
            log(f"benchmarking {key}…")
            commands = [
                ("cpython", [sys.executable, str(source)], None),
                ("hydro-jit", [str(HYD), "infuse", str(source)], None),
            ]
            aot_bin = tmpdir / f"{key}.bin"
            try:
                build_ms = aot_build(source, aot_bin, args.timeout) * 1000
            except Exception as exc:  # noqa: BLE001 - report and continue
                log(f"  AOT build failed: {exc}")
                build_ms = None
                aot_bin = None
            if aot_bin is not None:
                commands.append(("hydro-aot", [str(aot_bin)], build_ms))

            for impl, cmd, build in commands:
                try:
                    stats = measure(cmd, ROOT, repeats, args.timeout)
                    if stats["stdout"] != baseline[key]:
                        raise RuntimeError("output differs from CPython")
                    row = {
                        "workload": key,
                        "impl": impl,
                        "build_ms": build,
                        **{k: v for k, v in stats.items() if k != "stdout"},
                        "status": "ok",
                    }
                except Exception as exc:  # noqa: BLE001 - baseline report
                    failures += 1
                    row = {
                        "workload": key,
                        "impl": impl,
                        "build_ms": build,
                        "status": "error",
                        "error": str(exc),
                    }
                results.append(row)
                if row["status"] == "ok":
                    log(
                        f"  {impl:8s} median={row['median_ms']:.1f} ms "
                        f"rss={row['rss_kb']} KB"
                    )
                else:
                    log(f"  {impl:8s} ERROR: {row['error']}")

        # Startup-only comparison (no workload output beyond 'hello').
        key, filename, _ = STARTUP
        source = HERE / filename
        log("benchmarking startup…")
        startup_cmds = [
            ("cpython", [sys.executable, str(source)], None),
            ("hydro-jit", [str(HYD), "infuse", str(source)], None),
        ]
        aot_bin = tmpdir / "hello.bin"
        try:
            build_ms = aot_build(source, aot_bin, args.timeout) * 1000
            startup_cmds.append(("hydro-aot", [str(aot_bin)], build_ms))
        except Exception as exc:  # noqa: BLE001
            log(f"  AOT build failed: {exc}")
        for impl, cmd, build in startup_cmds:
            try:
                stats = measure(cmd, ROOT, repeats, args.timeout)
                if stats["stdout"] != baseline[key]:
                    raise RuntimeError("output differs from CPython")
                results.append(
                    {
                        "workload": key,
                        "impl": impl,
                        "build_ms": build,
                        **{k: v for k, v in stats.items() if k != "stdout"},
                        "status": "ok",
                    }
                )
                log(f"  {impl:8s} median={stats['median_ms']:.1f} ms")
            except Exception as exc:  # noqa: BLE001
                failures += 1
                results.append(
                    {
                        "workload": key,
                        "impl": impl,
                        "build_ms": build,
                        "status": "error",
                        "error": str(exc),
                    }
                )
                log(f"  {impl:8s} ERROR: {exc}")

    RESULTS.mkdir(parents=True, exist_ok=True)
    payload = {
        "meta": {
            "timestamp": time.strftime("%Y-%m-%d %H:%M:%S"),
            "python": sys.version.split()[0],
            "platform": platform.platform(),
            "cpu": platform.processor() or platform.machine(),
            "repeats": repeats,
            "tea": str(HYD),
        },
        "results": results,
    }
    (RESULTS / "latest.json").write_text(json.dumps(payload, indent=2), encoding="utf-8")

    lines = [
        "# Hydro Micro-Benchmark Baseline",
        "",
        f"_{payload['meta']['timestamp']} · {payload['meta']['platform']} · "
        f"{payload['meta']['cpu']} · repeats={repeats}_",
        "",
        "| workload | impl | median ms | min ms | build ms | RSS MB | status |",
        "|---|---|---:|---:|---:|---:|---|",
    ]
    for row in results:
        if row["status"] == "ok":
            rss = f"{row['rss_kb'] / 1024:.1f}" if row.get("rss_kb") else "-"
            build = f"{row['build_ms']:.0f}" if row.get("build_ms") else "-"
            lines.append(
                f"| {row['workload']} | {row['impl']} | {row['median_ms']:.1f} | "
                f"{row['min_ms']:.1f} | {build} | {rss} | ok |"
            )
        else:
            lines.append(f"| {row['workload']} | {row['impl']} | - | - | - | - | error |")
    lines.extend(
        [
            "",
            "Baseline for the import-free Hydro subset; dicts use a hash index",
            "(insertion order preserved), while string concatenation is still",
            "O(n²), so that load stays small. Not comparable to the CPython/uv",
            "harnesses in benchmarks/.",
        ]
    )
    text = "\n".join(lines)
    print()
    print(text)
    (RESULTS / "latest.md").write_text(text + "\n", encoding="utf-8")
    print(f"\njson: {RESULTS / 'latest.json'}")
    print(f"md:   {RESULTS / 'latest.md'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
