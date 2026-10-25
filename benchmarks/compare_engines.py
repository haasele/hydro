#!/usr/bin/env python3
"""Compare CPython, PyX, PyPy and Go on representative loop/startup workloads.

The Python workloads are plain scripts run in fresh processes. PyX runs them
through `pyx run`; Go measures an equivalent compiled Go program and is a
native reference, not a Python runtime. Missing engines (no `pypy3`, no built
`pyx`, no Go toolchain) are reported as skipped with the reason; timings are
never invented. A detected engine that fails or prints the wrong result is an
error and makes the command exit 1.

Usage:
    python3 benchmarks/compare_engines.py
    python3 benchmarks/compare_engines.py --n 2000000 --runs 7 --warmup 2
    python3 benchmarks/compare_engines.py --json /tmp/engines.json \
        --markdown /tmp/engines.md
    python3 benchmarks/compare_engines.py --pyx target/release/pyx

Exit codes: 0 = at least one successful measurement and no engine error,
1 = an engine failed, nothing could be measured, or --strict found a skip.
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
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_N = 1_000_000
DEFAULT_RUNS = 5
DEFAULT_WARMUP = 1
DEFAULT_TIMEOUT = 120.0

WORKLOADS = (
    ("loop", "while loop sum"),
    ("startup", "print + interpreter startup"),
)


@dataclass
class Engine:
    key: str
    label: str
    status: str  # ok | skipped
    note: str
    version: str = "-"
    path: str = "-"
    kind: str = ""  # python | pyx | go


@dataclass
class Result:
    workload: str
    engine: str
    status: str  # ok | skipped | error
    median_ms: float | None = None
    min_ms: float | None = None
    max_ms: float | None = None
    runs: list[float] | None = None
    ratio_vs_cpython: float | None = None
    note: str = ""


def log(message: str) -> None:
    print(message, file=sys.stderr, flush=True)


def version_of(command: list[str]) -> str:
    try:
        proc = subprocess.run(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=15,
        )
    except (OSError, subprocess.TimeoutExpired):
        return "unknown"
    line = proc.stdout.strip().splitlines()
    return line[0].strip() if line else "unknown"


def detect_python() -> Engine:
    if platform.python_implementation() == "CPython":
        return Engine(
            "cpython",
            "CPython",
            "ok",
            "",
            version=f"CPython {platform.python_version()}",
            path=sys.executable,
            kind="python",
        )
    for name in ("python3", "python"):
        candidate = shutil.which(name)
        if not candidate:
            continue
        probe = version_of(
            [candidate, "-c", "import platform; print(platform.python_implementation())"]
        )
        if probe == "CPython":
            version = version_of(
                [candidate, "-c", "import platform; print(platform.python_version())"]
            )
            return Engine(
                "cpython",
                "CPython",
                "ok",
                "",
                version=f"CPython {version}",
                path=candidate,
                kind="python",
            )
    return Engine("cpython", "CPython", "skipped", "no CPython interpreter found")


def detect_pypy() -> Engine:
    for name in ("pypy3", "pypy"):
        candidate = shutil.which(name)
        if candidate:
            return Engine(
                "pypy",
                "PyPy",
                "ok",
                "",
                version=version_of([candidate, "--version"]),
                path=candidate,
                kind="python",
            )
    return Engine("pypy", "PyPy", "skipped", "pypy3/pypy not found on PATH")


def detect_pyx(explicit: str | None) -> Engine:
    if explicit:
        candidate = Path(explicit).expanduser()
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return Engine(
                "pyx",
                "PyX (JIT)",
                "ok",
                "",
                version=version_of([str(candidate), "--version"]),
                path=str(candidate),
                kind="pyx",
            )
        return Engine(
            "pyx",
            "PyX (JIT)",
            "skipped",
            f"requested --pyx is not executable: {candidate}",
        )

    candidates = []
    env = os.environ.get("PYX_BIN")
    if env:
        candidates.append(Path(env).expanduser())
    which = shutil.which("pyx")
    if which:
        candidates.append(Path(which))
    candidates.extend((ROOT / "target/release/pyx", ROOT / "target/debug/pyx"))

    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return Engine(
                "pyx",
                "PyX (JIT)",
                "ok",
                "",
                version=version_of([str(candidate), "--version"]),
                path=str(candidate),
                kind="pyx",
            )
    return Engine(
        "pyx",
        "PyX (JIT)",
        "skipped",
        "pyx binary not found (build it or pass --pyx/$PYX_BIN)",
    )


def detect_go() -> Engine:
    candidate = shutil.which("go")
    if not candidate:
        return Engine("go", "Go", "skipped", "go not found on PATH")
    return Engine(
        "go",
        "Go",
        "ok",
        "",
        version=version_of([candidate, "version"]),
        path=candidate,
        kind="go",
    )


def python_script(workload: str, n: int) -> str:
    if workload == "startup":
        return 'print("hello")\n'
    return (
        f"n = {n}\n"
        "total = 0\n"
        "i = 0\n"
        "while i < n:\n"
        "    total += i\n"
        "    i += 1\n"
        "print(total)\n"
    )


def go_source(workload: str, n: int) -> str:
    if workload == "startup":
        return 'package main\n\nimport "fmt"\n\nfunc main() {\n\tfmt.Println("hello")\n}\n'
    return (
        "package main\n\n"
        'import "fmt"\n\n'
        "func main() {\n"
        f"\tn := {n}\n"
        "\ttotal := 0\n"
        "\tfor i := 0; i < n; i++ {\n"
        "\t\ttotal += i\n"
        "\t}\n"
        "\tfmt.Println(total)\n"
        "}\n"
    )


def expected_output(workload: str, n: int) -> str:
    if workload == "startup":
        return "hello"
    return str(n * (n - 1) // 2)


def build_go(engine: Engine, workdir: Path, n: int) -> dict[str, Path]:
    binaries: dict[str, Path] = {}
    for workload, _ in WORKLOADS:
        build_dir = workdir / f"go-{workload}"
        build_dir.mkdir(parents=True, exist_ok=True)
        (build_dir / "go.mod").write_text("module compare_engines\n\ngo 1.20\n", encoding="utf-8")
        (build_dir / "main.go").write_text(go_source(workload, n), encoding="utf-8")
        binary = workdir / f"go-{workload}.bin"
        proc = subprocess.run(
            [engine.path, "build", "-trimpath", "-o", str(binary), "main.go"],
            cwd=build_dir,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=180,
        )
        if proc.returncode != 0:
            raise RuntimeError(f"go build failed for {workload}: {proc.stderr.strip()[-2000:]}")
        binaries[workload] = binary
    return binaries


def command_for(
    engine: Engine,
    workload: str,
    scripts: dict[str, Path],
    go_binaries: dict[str, Path],
) -> list[str]:
    if engine.kind == "go":
        return [str(go_binaries[workload])]
    script = str(scripts[workload])
    if engine.kind == "pyx":
        return [engine.path, "run", script]
    return [engine.path, script]


def run_once(command: list[str], expected: str, timeout: float) -> float:
    start = time.perf_counter()
    proc = subprocess.run(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=timeout,
    )
    elapsed = time.perf_counter() - start
    if proc.returncode != 0:
        raise RuntimeError(
            f"exit {proc.returncode}: {proc.stderr.strip()[-2000:] or proc.stdout.strip()[-500:]}"
        )
    actual = proc.stdout.strip()
    if actual != expected:
        raise RuntimeError(f"output mismatch: {actual!r} != {expected!r}")
    return elapsed


def measure(command: list[str], expected: str, runs: int, warmup: int, timeout: float) -> list[float]:
    for _ in range(warmup):
        run_once(command, expected, timeout)
    return [run_once(command, expected, timeout) for _ in range(runs)]


def markdown_report(payload: dict) -> str:
    meta = payload["meta"]
    labels = {engine["key"]: engine["label"] for engine in payload["engines"]}
    lines = [
        "# PyX engine comparison",
        "",
        f"- Generated: {payload['generated']}",
        f"- Host: {meta['platform']} / {meta['machine']} / {meta['cpus']} CPUs",
        f"- Loop size: n={meta['n']}; runs={meta['runs']}; warmup={meta['warmup']}",
        "- Go measures an equivalent compiled Go program (native reference, not a Python runtime).",
        "",
        "| workload | engine | status | median ms | min ms | CPython/engine | note |",
        "|---|---|---|---:|---:|---:|---|",
    ]
    for row in payload["results"]:
        median = "-" if row["median_ms"] is None else f"{row['median_ms']:.3f}"
        low = "-" if row["min_ms"] is None else f"{row['min_ms']:.3f}"
        ratio = (
            "-"
            if row["ratio_vs_cpython"] is None
            else f"{row['ratio_vs_cpython']:.2f}x"
        )
        note = row["note"].replace("|", "\\|")
        lines.append(
            f"| {row['workload']} | {labels.get(row['engine'], row['engine'])} | {row['status']} | "
            f"{median} | {low} | {ratio} | {note} |"
        )
    counts = {status: 0 for status in ("ok", "skipped", "error")}
    for row in payload["results"]:
        counts[row["status"]] = counts.get(row["status"], 0) + 1
    lines += [
        "",
        f"ok: {counts['ok']}, skipped: {counts['skipped']}, errors: {counts['error']}",
        "ratio = CPython median / engine median (>1 means the engine is faster).",
        "",
    ]
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Compare CPython, PyX, PyPy and Go on loop/startup workloads."
    )
    parser.add_argument("--n", type=int, default=DEFAULT_N, help=f"loop iterations (default {DEFAULT_N})")
    parser.add_argument("--runs", type=int, default=DEFAULT_RUNS, help=f"measured runs (default {DEFAULT_RUNS})")
    parser.add_argument("--warmup", type=int, default=DEFAULT_WARMUP, help=f"warmup runs (default {DEFAULT_WARMUP})")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT, help="per-run timeout in seconds")
    parser.add_argument("--pyx", help="PyX binary (default: $PYX_BIN, PATH, target/{release,debug}/pyx)")
    parser.add_argument("--json", type=Path, help="write the machine-readable result here")
    parser.add_argument("--markdown", type=Path, help="write the markdown report here")
    parser.add_argument("--strict", action="store_true", help="fail when any engine is skipped")
    args = parser.parse_args()

    if args.n < 1:
        parser.error("--n must be positive")
    if args.runs < 1:
        parser.error("--runs must be positive")
    if args.warmup < 0:
        parser.error("--warmup must not be negative")

    engines = [detect_python(), detect_pyx(args.pyx), detect_pypy(), detect_go()]
    by_key = {engine.key: engine for engine in engines}
    for engine in engines:
        if engine.status == "ok":
            log(f"{engine.key}: {engine.version} ({engine.path})")
        else:
            log(f"{engine.key}: skipped - {engine.note}")

    results: list[Result] = []
    with tempfile.TemporaryDirectory(prefix="pyx-compare-") as tmp:
        workdir = Path(tmp)
        scripts: dict[str, Path] = {}
        for workload, _ in WORKLOADS:
            path = workdir / f"{workload}.py"
            path.write_text(python_script(workload, args.n), encoding="utf-8")
            scripts[workload] = path

        go_binaries: dict[str, Path] = {}
        go_engine = by_key["go"]
        if go_engine.status == "ok":
            try:
                go_binaries = build_go(go_engine, workdir, args.n)
            except (OSError, subprocess.TimeoutExpired, RuntimeError) as exc:
                go_engine.status = "skipped"
                go_engine.note = f"Go build failed: {exc}"
                log(f"go: skipped - {go_engine.note}")

        for workload, _ in WORKLOADS:
            expected = expected_output(workload, args.n)
            for engine in engines:
                if engine.status != "ok":
                    results.append(
                        Result(workload, engine.key, "skipped", note=engine.note)
                    )
                    continue
                try:
                    command = command_for(engine, workload, scripts, go_binaries)
                    times = measure(command, expected, args.runs, args.warmup, args.timeout)
                except (OSError, subprocess.TimeoutExpired, RuntimeError) as exc:
                    results.append(
                        Result(workload, engine.key, "error", note=str(exc)[-2000:])
                    )
                    continue
                results.append(
                    Result(
                        workload,
                        engine.key,
                        "ok",
                        median_ms=statistics.median(times) * 1000.0,
                        min_ms=min(times) * 1000.0,
                        max_ms=max(times) * 1000.0,
                        runs=[value * 1000.0 for value in times],
                    )
                )

            cpython = next(
                (
                    row
                    for row in results
                    if row.workload == workload
                    and row.engine == "cpython"
                    and row.status == "ok"
                ),
                None,
            )
            if cpython and cpython.median_ms:
                for row in results:
                    if (
                        row.workload == workload
                        and row.status == "ok"
                        and row.median_ms is not None
                    ):
                        row.ratio_vs_cpython = cpython.median_ms / row.median_ms

    payload = {
        "schema": 1,
        "generated": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "meta": {
            "n": args.n,
            "runs": args.runs,
            "warmup": args.warmup,
            "timeout_s": args.timeout,
            "platform": platform.platform(),
            "machine": platform.machine(),
            "cpus": os.cpu_count(),
        },
        "engines": [
            {
                "key": engine.key,
                "label": engine.label,
                "status": engine.status,
                "version": engine.version,
                "path": engine.path,
                "note": engine.note,
            }
            for engine in engines
        ],
        "workloads": [
            {"key": key, "description": description}
            for key, description in WORKLOADS
        ],
        "results": [
            {
                "workload": row.workload,
                "engine": row.engine,
                "status": row.status,
                "median_ms": row.median_ms,
                "min_ms": row.min_ms,
                "max_ms": row.max_ms,
                "runs_ms": row.runs,
                "ratio_vs_cpython": row.ratio_vs_cpython,
                "note": row.note,
            }
            for row in results
        ],
    }

    report = markdown_report(payload)
    print(report)
    if args.markdown:
        args.markdown.parent.mkdir(parents=True, exist_ok=True)
        args.markdown.write_text(report, encoding="utf-8")
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

    errors = any(row.status == "error" for row in results)
    measured = any(row.status == "ok" for row in results)
    skipped = any(row.status == "skipped" for row in results)
    if errors or not measured:
        return 1
    if args.strict and skipped:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
