#!/usr/bin/env python3
"""Resumable, chunked triage of CPython's official ``Lib/test`` suite against Hydro.

Unlike ``scripts/cpython_compat.py`` (the release-gate report), this harness is
built for long, interruptible triage sessions:

* every selected ``test_*.py`` is run twice -- once by the pinned CPython
  oracle and once by Hydro (``tea infuse``) -- with a per-run timeout and
  SIGTERM/SIGKILL escalation on the whole process group;
* at most two tests run concurrently, and a watchdog reads ``/proc/meminfo``
  and kills the running process group when ``MemAvailable`` drops below
  ``--min-avail-mb`` (default ``$HYDRO_MIN_AVAIL_MB`` or 1500);
* the *complete* stdout/stderr of both interpreters is preserved per test in
  ``.hydro/official/<test>.log`` (streamed to disk, never truncated);
* each finished test is appended to ``.hydro/official/results.jsonl`` with
  ``fsync``, so ``--resume`` skips tests already recorded for the same pin,
  source tree, timeout and memory threshold;
* the final JSON plus Markdown backlog groups Hydro-attributable failures by
  first error and by implicated module, sorted by the number of affected tests.

Source/oracle pinning, test discovery, differential classification and the
error regexes are imported from ``cpython_compat.py``; this script only adds
the session-isolated execution, checkpointing and triage reports.

Smoke test (documented for Main):

    python3 scripts/cpython_triage.py --limit 1

That runs one test, prints its status, and writes:

    .hydro/official/<test>.log          full stdout/stderr for both runs
    .hydro/official/results.jsonl       checkpoint (one JSON object per test)
    .hydro/official/triage.json         machine-readable report
    .hydro/official/triage.md           grouped Markdown backlog

Then continue the same run after an interruption with:

    python3 scripts/cpython_triage.py --limit 20 --resume

Exit codes: 0 = report written (failures are data, not a harness error),
1 = invalid usage or no selected tests, 2 = missing/unusable Hydro, CPython
oracle, or source tree, 130 = interrupted (partial report and checkpoint kept).
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import cpython_compat as compat  # noqa: E402  (same directory, shared pinning)

ROOT = compat.ROOT
OUT_ROOT = ROOT / ".hydro" / "official"
DEFAULT_JSON = OUT_ROOT / "triage.json"
DEFAULT_MD = OUT_ROOT / "triage.md"
DEFAULT_CHECKPOINT = OUT_ROOT / "results.jsonl"

DEFAULT_WORKERS = 2
MAX_WORKERS = 2
DEFAULT_TIMEOUT = 60.0
KILL_AFTER = 2.0
MEM_POLL_SECONDS = 0.25
MEM_WAIT_SECONDS = 10.0
MEMORY_STATUS = "memkill"
ERROR_STATUS = "error"
STATUSES = (*compat.STATUSES, MEMORY_STATUS, ERROR_STATUS)
# Failures that point at Hydro (or at the harness) rather than at the oracle.
BACKLOG_STATUSES = ("hydro_fail", "timeout", MEMORY_STATUS, ERROR_STATUS)

MODULE_PATTERNS = (
    re.compile(r"module '([^']+)'"),
    re.compile(r"No module named '([^']+)'"),
    re.compile(r"No module named ([A-Za-z_][\w.]*)"),
    re.compile(r"cannot import name '[^']+' from '([^']+)'"),
    re.compile(r"'from ([A-Za-z_][\w.]*) import"),
)

_ACTIVE: set[subprocess.Popen] = set()
_ACTIVE_LOCK = threading.Lock()
_STOP = threading.Event()


def log(message: str) -> None:
    compat.log(message)


def utc_now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def env_int(name: str, default: int) -> int:
    raw = os.environ.get(name)
    if not raw:
        return default
    try:
        return int(raw)
    except ValueError:
        log(f"warning: ignoring non-integer ${name}={raw!r}")
        return default


# --------------------------------------------------------------- memory watchdog
def available_mb() -> int | None:
    """Return ``MemAvailable`` in MiB, or None when /proc/meminfo is absent."""
    try:
        with open("/proc/meminfo", encoding="ascii", errors="replace") as handle:
            for line in handle:
                if line.startswith("MemAvailable:"):
                    try:
                        return int(line.split()[1]) // 1024
                    except (IndexError, ValueError):
                        return None
    except OSError:
        return None
    return None


def wait_for_memory(min_avail_mb: int) -> int | None:
    """Wait briefly for memory to recover; return the still-low value if not."""
    if min_avail_mb <= 0:
        return None
    avail = available_mb()
    if avail is None or avail >= min_avail_mb:
        return None
    deadline = time.monotonic() + MEM_WAIT_SECONDS
    log(
        f"memory watchdog: {avail} MB available < {min_avail_mb} MB; "
        f"waiting up to {MEM_WAIT_SECONDS:g}s for recovery"
    )
    while time.monotonic() < deadline:
        if _STOP.wait(min(1.0, MEM_WAIT_SECONDS)):
            break
        avail = available_mb()
        if avail is None or avail >= min_avail_mb:
            return None
    return avail


# ----------------------------------------------------------- process-group runs
def _signal_group(proc: subprocess.Popen, sig: int) -> None:
    if proc.poll() is not None:
        return
    try:
        if hasattr(os, "killpg"):
            os.killpg(proc.pid, sig)
        elif sig == signal.SIGKILL:
            proc.kill()
        else:
            proc.terminate()
    except (ProcessLookupError, PermissionError):
        pass


def kill_group(proc: subprocess.Popen, *, grace: float = KILL_AFTER) -> None:
    """SIGTERM the process group, then SIGKILL after *grace* seconds."""
    _signal_group(proc, signal.SIGTERM)
    try:
        proc.wait(timeout=grace)
        return
    except subprocess.TimeoutExpired:
        pass
    _signal_group(proc, signal.SIGKILL)
    try:
        proc.wait(timeout=10.0)
    except subprocess.TimeoutExpired:
        log(f"warning: process group {proc.pid} survived SIGKILL; abandoning it")


def terminate_active() -> None:
    with _ACTIVE_LOCK:
        procs = list(_ACTIVE)
    for proc in procs:
        kill_group(proc)


def run_in_group(
    cmd: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
    timeout: float,
    min_avail_mb: int,
    stdout_path: Path,
    stderr_path: Path,
) -> dict:
    """Run *cmd* in its own session, streaming stdout/stderr to files.

    The whole process group is killed on timeout or when ``MemAvailable``
    falls below *min_avail_mb*.  Full output is never held in memory.
    """
    started = time.perf_counter()
    result = {
        "exit": None,
        "timed_out": False,
        "mem_killed": False,
        "mem_avail_mb": None,
        "skipped": False,
        "error": None,
        "duration_ms": 0.0,
    }
    if _STOP.is_set():
        result["skipped"] = True
        result["error"] = "run stopped before launch"
        return result

    low = wait_for_memory(min_avail_mb)
    if low is not None:
        result["mem_killed"] = True
        result["mem_avail_mb"] = low
        result["error"] = (
            f"not started: MemAvailable {low} MB below watchdog threshold "
            f"{min_avail_mb} MB"
        )
        return result

    with stdout_path.open("wb") as out_handle, stderr_path.open("wb") as err_handle:
        try:
            proc = subprocess.Popen(
                cmd,
                cwd=str(cwd),
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=out_handle,
                stderr=err_handle,
                start_new_session=True,
                close_fds=True,
            )
        except OSError as exc:
            message = f"failed to start {cmd[0]}: {exc}\n"
            err_handle.write(message.encode("utf-8", "replace"))
            result["error"] = message.strip()
            result["duration_ms"] = round((time.perf_counter() - started) * 1000.0, 1)
            return result

        with _ACTIVE_LOCK:
            _ACTIVE.add(proc)
        deadline = started + timeout
        try:
            while True:
                if proc.poll() is not None:
                    break
                if _STOP.is_set():
                    kill_group(proc)
                    result["error"] = "run stopped by interrupt"
                    break
                avail = available_mb()
                if min_avail_mb > 0 and avail is not None and avail < min_avail_mb:
                    result["mem_killed"] = True
                    result["mem_avail_mb"] = avail
                    kill_group(proc, grace=min(1.0, KILL_AFTER))
                    break
                now = time.monotonic()
                if now >= deadline:
                    result["timed_out"] = True
                    kill_group(proc)
                    break
                time.sleep(min(MEM_POLL_SECONDS, max(0.01, deadline - now)))
            # Reap whatever is left (the loop may have broken out after a kill).
            try:
                proc.wait(timeout=10.0)
            except subprocess.TimeoutExpired:
                log(f"warning: pid {proc.pid} still running after wait")
            result["exit"] = proc.poll()
        finally:
            with _ACTIVE_LOCK:
                _ACTIVE.discard(proc)

    result["duration_ms"] = round((time.perf_counter() - started) * 1000.0, 1)
    return result


# ------------------------------------------------------------ error attribution
def first_error_from_files(stderr_path: Path, stdout_path: Path) -> str | None:
    """First ``error[HYDnnnn]`` line, else the first unittest ``FAILED`` line,
    else the first exception, else the last non-empty line."""
    for path in (stderr_path, stdout_path):
        first_exception: str | None = None
        first_failed: str | None = None
        try:
            with path.open("r", encoding="utf-8", errors="replace") as handle:
                for line in handle:
                    clean = compat.ANSI_RE.sub("", line).strip()
                    if not clean:
                        continue
                    if compat.HYDRO_CODE_RE.search(clean):
                        return clean[: compat.MAX_ERROR_CHARS]
                    if first_failed is None and clean.startswith("FAILED "):
                        # Hydro's unittest runner: `FAILED <Test.test>: (<class ...>, exc, ...)`
                        first_failed = clean[: compat.MAX_ERROR_CHARS]
                    if first_exception is None and compat.EXCEPTION_RE.match(clean):
                        first_exception = clean[: compat.MAX_ERROR_CHARS]
        except OSError:
            continue
        if first_failed:
            return first_failed
        if first_exception:
            return first_exception

    for path in (stderr_path, stdout_path):
        tail = ""
        try:
            with path.open("r", encoding="utf-8", errors="replace") as handle:
                for line in handle:
                    stripped = compat.ANSI_RE.sub("", line).strip()
                    if stripped:
                        tail = stripped
        except OSError:
            continue
        if tail:
            return tail[: compat.MAX_ERROR_CHARS]
    return None


def module_from_error(error: str | None) -> str:
    if not error:
        return "(no error captured)"
    for pattern in MODULE_PATTERNS:
        match = pattern.search(error)
        if match:
            return match.group(1)
    return "(unclassified)"


def classify(cpython_run: dict, hydro_run: dict) -> str:
    if cpython_run.get("error") or hydro_run.get("error"):
        if hydro_run.get("mem_killed") or cpython_run.get("mem_killed"):
            return MEMORY_STATUS
        return ERROR_STATUS
    if hydro_run.get("mem_killed") or cpython_run.get("mem_killed"):
        return MEMORY_STATUS
    if hydro_run.get("timed_out"):
        return "timeout"
    return compat.classify(cpython_run, hydro_run)


def describe_failure(
    status: str,
    *,
    timeout: float,
    min_avail_mb: int,
    cpython_run: dict,
    hydro_run: dict,
    cpython_stderr: Path,
    cpython_stdout: Path,
    hydro_stderr: Path,
    hydro_stdout: Path,
) -> str | None:
    if status == MEMORY_STATUS:
        parts: list[str] = []
        for label, run in (("CPython oracle", cpython_run), ("Hydro", hydro_run)):
            if not run.get("mem_killed"):
                continue
            if run.get("error"):
                parts.append(f"{label}: {run['error']}")
            elif run.get("mem_avail_mb") is not None:
                parts.append(
                    f"{label}: watchdog kill at {run['mem_avail_mb']} MB available "
                    f"(threshold {min_avail_mb} MB)"
                )
            else:
                parts.append(
                    f"{label}: watchdog kill (threshold {min_avail_mb} MB)"
                )
        return "; ".join(parts)[: compat.MAX_ERROR_CHARS] or "memory watchdog triggered"
    if status == ERROR_STATUS:
        detail = hydro_run.get("error") or cpython_run.get("error") or "unknown run error"
        return f"harness: {detail}"[: compat.MAX_ERROR_CHARS]
    if status == "timeout":
        return f"Hydro timed out after {timeout:g}s"
    if status == "cpython_fail":
        if cpython_run.get("timed_out"):
            return f"CPython oracle timed out after {timeout:g}s while Hydro passed"
        return (
            f"CPython oracle exited {cpython_run.get('exit')} while Hydro passed"
        )

    error = first_error_from_files(hydro_stderr, hydro_stdout)
    if error:
        return error
    error = first_error_from_files(cpython_stderr, cpython_stdout)
    if error:
        return error
    if status == compat.PASS_STATUS:
        return None
    return f"Hydro exited {hydro_run.get('exit')} with no captured error"


# ------------------------------------------------------------------ log output
def copy_text(src: Path, dst) -> None:
    if not src.is_file() or src.stat().st_size == 0:
        dst.write("(empty)\n")
        return
    with src.open("r", encoding="utf-8", errors="replace") as handle:
        shutil.copyfileobj(handle, dst)
    dst.write("\n")


def write_log(
    log_path: Path,
    *,
    test_path: Path,
    source: Path,
    cpython: Path,
    tea: Path,
    cpython_run: dict,
    hydro_run: dict,
    cpython_stdout: Path,
    cpython_stderr: Path,
    hydro_stdout: Path,
    hydro_stderr: Path,
    status: str,
    error: str | None,
) -> None:
    log_path.parent.mkdir(parents=True, exist_ok=True)
    handle = tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        errors="replace",
        dir=str(log_path.parent),
        prefix=f".{log_path.name}.",
        suffix=".tmp",
        delete=False,
    )
    tmp_path = Path(handle.name)
    try:
        with handle:
            handle.write(f"# {test_path.name}\n")
            handle.write(f"generated: {utc_now()}\n")
            handle.write(f"source: {source}\n")
            handle.write(f"status: {status}\n")
            handle.write(f"first error: {error or '-'}\n\n")
            for label, cmd, cwd, run, stdout, stderr in (
                (
                    "CPython oracle",
                    [str(cpython), str(test_path)],
                    source / "Lib",
                    cpython_run,
                    cpython_stdout,
                    cpython_stderr,
                ),
                (
                    "Hydro",
                    [str(tea), "infuse", "--color=never", str(test_path)],
                    ROOT,
                    hydro_run,
                    hydro_stdout,
                    hydro_stderr,
                ),
            ):
                handle.write(f"## {label}\n")
                handle.write("command: " + " ".join(cmd) + "\n")
                handle.write(f"cwd: {cwd}\n")
                handle.write(
                    "exit: {exit} timed_out={timed_out} mem_killed={mem_killed} "
                    "duration_ms={duration_ms}\n".format(**run)
                )
                if run.get("error"):
                    handle.write(f"run error: {run['error']}\n")
                handle.write("\n### stdout\n")
                copy_text(stdout, handle)
                handle.write("### stderr\n")
                copy_text(stderr, handle)
                handle.write("\n")
        os.chmod(tmp_path, 0o644)
        os.replace(tmp_path, log_path)
    except BaseException:
        tmp_path.unlink(missing_ok=True)
        raise


# ------------------------------------------------------------------ checkpoint
def load_checkpoint(path: Path) -> dict[str, dict]:
    records: dict[str, dict] = {}
    if not path.is_file():
        return records
    try:
        with path.open("r", encoding="utf-8", errors="replace") as handle:
            for lineno, line in enumerate(handle, 1):
                line = line.strip()
                if not line:
                    continue
                try:
                    record = json.loads(line)
                except ValueError:
                    log(f"warning: ignoring malformed checkpoint line {lineno} in {path}")
                    continue
                if isinstance(record, dict) and isinstance(record.get("name"), str):
                    records[record["name"]] = record
    except OSError as exc:
        log(f"warning: cannot read checkpoint {path}: {exc}")
    return records


def reset_checkpoint(path: Path) -> None:
    if not path.exists():
        return
    backup = path.with_name(path.name + ".prev")
    try:
        os.replace(path, backup)
        log(f"previous checkpoint rotated to {backup}")
    except OSError as exc:
        log(f"warning: cannot rotate checkpoint {path}: {exc}")


def append_checkpoint(path: Path, record: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(record, sort_keys=True, ensure_ascii=False) + "\n")
        handle.flush()
        os.fsync(handle.fileno())


def record_matches(
    record: dict, *, source: Path, timeout: float, min_avail_mb: int
) -> bool:
    return (
        record.get("python_target") == compat.TARGET_PYTHON
        and record.get("source") == str(source)
        and record.get("timeout") == timeout
        and record.get("min_avail_mb") == min_avail_mb
        and record.get("status") in STATUSES
    )


# -------------------------------------------------------------------- per test
def evaluate_test(
    path: Path,
    *,
    source: Path,
    cpython: Path,
    tea: Path,
    timeout: float,
    min_avail_mb: int,
    cpython_env: dict[str, str],
    hydro_env: dict[str, str],
    log_path: Path,
) -> dict:
    lib_dir = source / "Lib"
    workdir = Path(tempfile.mkdtemp(prefix=f"{path.stem}.", dir=str(log_path.parent)))
    cpython_stdout = workdir / "cpython.stdout"
    cpython_stderr = workdir / "cpython.stderr"
    hydro_stdout = workdir / "hydro.stdout"
    hydro_stderr = workdir / "hydro.stderr"
    try:
        cpython_run = run_in_group(
            [str(cpython), str(path)],
            cwd=lib_dir,
            env=cpython_env,
            timeout=timeout,
            min_avail_mb=min_avail_mb,
            stdout_path=cpython_stdout,
            stderr_path=cpython_stderr,
        )
        if cpython_run.get("mem_killed"):
            # Do not start a second interpreter under the same memory pressure.
            hydro_run = {
                "exit": None,
                "timed_out": False,
                "mem_killed": False,
                "mem_avail_mb": None,
                "skipped": True,
                "error": "not started: CPython oracle hit the memory watchdog",
                "duration_ms": 0.0,
            }
        else:
            hydro_run = run_in_group(
                [str(tea), "infuse", "--color=never", str(path)],
                cwd=ROOT,
                env=hydro_env,
                timeout=timeout,
                min_avail_mb=min_avail_mb,
                stdout_path=hydro_stdout,
                stderr_path=hydro_stderr,
            )
        status = classify(cpython_run, hydro_run)
        error = describe_failure(
            status,
            timeout=timeout,
            min_avail_mb=min_avail_mb,
            cpython_run=cpython_run,
            hydro_run=hydro_run,
            cpython_stderr=cpython_stderr,
            cpython_stdout=cpython_stdout,
            hydro_stderr=hydro_stderr,
            hydro_stdout=hydro_stdout,
        )
        write_log(
            log_path,
            test_path=path,
            source=source,
            cpython=cpython,
            tea=tea,
            cpython_run=cpython_run,
            hydro_run=hydro_run,
            cpython_stdout=cpython_stdout,
            cpython_stderr=cpython_stderr,
            hydro_stdout=hydro_stdout,
            hydro_stderr=hydro_stderr,
            status=status,
            error=error,
        )
        return {
            "name": path.name,
            "status": status,
            "error": error,
            "module": module_from_error(error),
            "cpython": {
                "exit": cpython_run["exit"],
                "timed_out": cpython_run["timed_out"],
                "mem_killed": cpython_run["mem_killed"],
                "duration_ms": cpython_run["duration_ms"],
            },
            "hydro": {
                "exit": hydro_run["exit"],
                "timed_out": hydro_run["timed_out"],
                "mem_killed": hydro_run["mem_killed"],
                "duration_ms": hydro_run["duration_ms"],
            },
            "log": str(log_path),
            "python_target": compat.TARGET_PYTHON,
            "source": str(source),
            "timeout": timeout,
            "min_avail_mb": min_avail_mb,
            "generated": utc_now(),
        }
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def failed_record(
    path: Path,
    *,
    source: Path,
    timeout: float,
    min_avail_mb: int,
    log_path: Path,
    error: str,
) -> dict:
    if not log_path.exists():
        try:
            log_path.parent.mkdir(parents=True, exist_ok=True)
            log_path.write_text(
                f"# {path.name}\nstatus: {ERROR_STATUS}\n"
                f"first error: harness: {error}\n",
                encoding="utf-8",
            )
        except OSError as exc:
            log(f"warning: cannot write harness log {log_path}: {exc}")
    return {
        "name": path.name,
        "status": ERROR_STATUS,
        "error": f"harness: {error}"[: compat.MAX_ERROR_CHARS],
        "module": "(harness)",
        "cpython": {
            "exit": None,
            "timed_out": False,
            "mem_killed": False,
            "duration_ms": 0.0,
        },
        "hydro": {
            "exit": None,
            "timed_out": False,
            "mem_killed": False,
            "duration_ms": 0.0,
        },
        "log": str(log_path),
        "python_target": compat.TARGET_PYTHON,
        "source": str(source),
        "timeout": timeout,
        "min_avail_mb": min_avail_mb,
        "generated": utc_now(),
    }


# ---------------------------------------------------------------------- reports
def group_failures(records: list[dict]) -> dict[str, list[dict]]:
    by_error: dict[str, list[dict]] = defaultdict(list)
    by_module: dict[str, list[dict]] = defaultdict(list)
    for record in records:
        if record.get("status") not in BACKLOG_STATUSES:
            continue
        key = record.get("error") or "(no error captured)"
        by_error[key].append(record)
        by_module[record.get("module") or "(unclassified)"].append(record)

    def pack(groups: dict[str, list[dict]]) -> list[dict]:
        packed = []
        for key, items in groups.items():
            statuses = Counter(item["status"] for item in items)
            packed.append(
                {
                    "key": key,
                    "count": len(items),
                    "by_status": dict(sorted(statuses.items())),
                    "tests": sorted(item["name"] for item in items),
                }
            )
        packed.sort(key=lambda group: (-group["count"], group["key"]))
        return packed

    return {"by_error": pack(by_error), "by_module": pack(by_module)}


def summarize(records: list[dict], selected: int) -> dict:
    by_status = Counter(record["status"] for record in records)
    counts = {status: by_status.get(status, 0) for status in STATUSES}
    passed = counts[compat.PASS_STATUS]
    return {
        "selected": selected,
        "completed": len(records),
        "passed": passed,
        "failed": len(records) - passed,
        "backlog": sum(counts[status] for status in BACKLOG_STATUSES),
        "by_status": counts,
    }


def md_group_table(groups: list[dict], key_header: str) -> list[str]:
    lines = [
        f"| # | {key_header} | Tests | Statuses | Examples |",
        "| ---: | --- | ---: | --- | --- |",
    ]
    if not groups:
        return lines + ["| - | No Hydro-attributable failures. | 0 | - | - |"]
    for index, group in enumerate(groups, 1):
        statuses = ", ".join(
            f"{status}={count}" for status, count in group["by_status"].items()
        )
        examples = ", ".join(f"`{name}`" for name in group["tests"][:5])
        if len(group["tests"]) > 5:
            examples += f", … (+{len(group['tests']) - 5})"
        lines.append(
            f"| {index} | {compat.md_cell(group['key'], 160)} | {group['count']} | "
            f"{compat.md_cell(statuses)} | {compat.md_cell(examples) or '-'} |"
        )
    return lines


def render_markdown(
    *,
    meta: dict,
    summary: dict,
    groups: dict[str, list[dict]],
) -> str:
    cpython = meta["cpython"]
    hydro = meta["hydro"]
    out: list[str] = []
    out.append("# CPython official-suite triage backlog")
    out.append("")
    out.append("Generated by `scripts/cpython_triage.py`.")
    out.append("")
    out.append(
        f"Environment: CPython `{cpython['version']}` (`{cpython['path']}`) · "
        f"Hydro `{hydro['version']}` (`{hydro['path']}`)"
    )
    out.append("")
    out.append(f"Source: `{meta['source']}` · generated {meta['date']}")
    out.append("")
    out.append(f"Checkpoint: `{meta['checkpoint']}` · logs: `{meta['log_dir']}`")
    out.append("")
    out.append(
        f"Run complete: {'yes' if meta['complete'] else 'no'} · "
        f"{summary['completed']}/{summary['selected']} selected tests recorded · "
        f"{meta['elapsed_seconds']:g}s"
    )
    out.append("")
    out.append("## Summary")
    out.append("")
    out.append("| Status | Tests |")
    out.append("| --- | ---: |")
    for status in STATUSES:
        out.append(f"| {status} | {summary['by_status'][status]} |")
    out.append(f"| **Total** | {summary['completed']} |")
    out.append("")
    out.append(
        f"Backlog: **{summary['backlog']}** tests need Hydro work "
        "(`hydro_fail`, `timeout`, `memkill`, `error`). `expected_fail` means the "
        "CPython oracle also fails in this environment; `cpython_fail` means the "
        "oracle failed while Hydro passed."
    )
    out.append("")
    out.append("## Backlog by first error")
    out.append("")
    out.extend(md_group_table(groups["by_error"], "First error"))
    out.append("")
    out.append("## Backlog by module")
    out.append("")
    out.extend(md_group_table(groups["by_module"], "Module"))
    out.append("")
    out.append("## Per-test logs")
    out.append("")
    out.append(
        "Full stdout/stderr for every completed test is in "
        f"`{meta['log_dir']}/<test>.log`; the machine-readable report is "
        f"`{meta['json_path']}`."
    )
    out.append("")
    return "\n".join(out)


def build_payload(
    *,
    meta: dict,
    summary: dict,
    groups: dict[str, list[dict]],
    records: list[dict],
) -> dict:
    return {
        "meta": meta,
        "summary": summary,
        "backlog": groups,
        "tests": records,
    }


# ------------------------------------------------------------------------ main
def env_for_runs(source: Path) -> tuple[dict[str, str], dict[str, str]]:
    env = dict(os.environ)
    env["BROWSER"] = "true"  # keep `import antigravity`/webbrowser from opening a window
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    env.setdefault("NO_COLOR", "1")
    cpython_env = dict(env)
    cpython_env["PYTHONPATH"] = str(source / "Lib")
    hydro_env = dict(env)
    hydro_env.pop("PYTHONPATH", None)  # the oracle's path must not leak into Hydro
    hydro_env["HYDRO_STDLIB"] = str(source / "Lib")
    return cpython_env, hydro_env


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    ap = argparse.ArgumentParser(
        description=(
            "Resumable triage of CPython's official Lib/test suite against Hydro "
            "(full per-test logs, memory watchdog, JSONL checkpoint)."
        ),
    )
    ap.add_argument("--source", metavar="PATH", help="CPython source tree (default: pinned cache)")
    ap.add_argument("--cpython", metavar="PATH", help="CPython oracle executable (default: pinned)")
    ap.add_argument("--tea", help="path to the Hydro binary (default: $HYDRO_BIN, release, debug)")
    ap.add_argument("--limit", type=int, help="run at most N selected tests (chunked triage)")
    ap.add_argument("--only", help="comma-separated test names (with or without .py)")
    ap.add_argument(
        "--workers",
        type=int,
        choices=range(1, MAX_WORKERS + 1),
        default=DEFAULT_WORKERS,
        metavar=f"1-{MAX_WORKERS}",
        help=f"concurrent tests (1-{MAX_WORKERS}, default {DEFAULT_WORKERS})",
    )
    ap.add_argument(
        "--timeout",
        type=float,
        default=DEFAULT_TIMEOUT,
        help=f"per-interpreter timeout in seconds, then SIGTERM/SIGKILL (default {DEFAULT_TIMEOUT:g})",
    )
    ap.add_argument(
        "--min-avail-mb",
        type=int,
        default=env_int("HYDRO_MIN_AVAIL_MB", 1500),
        help="kill the test group below this MemAvailable threshold in MB "
        "(default: $HYDRO_MIN_AVAIL_MB or 1500; 0 disables)",
    )
    ap.add_argument(
        "--resume",
        action="store_true",
        help="keep the JSONL checkpoint and skip tests already recorded for this source/timeout",
    )
    ap.add_argument("--json", metavar="PATH", help=f"JSON report path (default: {DEFAULT_JSON})")
    ap.add_argument("--md", metavar="PATH", help=f"Markdown backlog path (default: {DEFAULT_MD})")
    return ap.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    _STOP.clear()

    if args.timeout <= 0:
        log("error: --timeout must be positive")
        return 1
    if args.limit is not None and args.limit < 0:
        log("error: --limit must be non-negative")
        return 1
    if args.min_avail_mb < 0:
        log("error: --min-avail-mb must be non-negative (0 disables the watchdog)")
        return 1

    tea, hydro_error = compat.resolve_hydro(args.tea)
    if tea is None:
        log(f"error: {hydro_error}")
        return 2

    cpython, cpython_version, cpython_error = compat.resolve_cpython(args.cpython)
    if cpython is None or cpython_version is None:
        log(f"error: {cpython_error}")
        return 2

    source, source_error = compat.resolve_source(args.source, allow_download=True)
    if source is None:
        log(f"error: {source_error}")
        return 2

    test_dir = source / "Lib" / "test"
    paths, unknown = compat.discover_tests(test_dir, args.only, args.limit)
    for name in unknown:
        log(f"warning: test not found in {test_dir}: {name}")
    if not paths:
        log(f"error: no test_*.py selected in {test_dir}")
        return 1

    json_path = Path(args.json).expanduser() if args.json else DEFAULT_JSON
    md_path = Path(args.md).expanduser() if args.md else DEFAULT_MD
    checkpoint_path = DEFAULT_CHECKPOINT
    log_dir = OUT_ROOT
    log_dir.mkdir(parents=True, exist_ok=True)
    if args.min_avail_mb > 0:
        avail = available_mb()
        if avail is None:
            log("warning: /proc/meminfo unavailable; memory watchdog disabled")
        else:
            log(
                f"memory watchdog: {avail} MB available, "
                f"killing below {args.min_avail_mb} MB"
            )

    if not args.resume:
        reset_checkpoint(checkpoint_path)
    checkpoint = load_checkpoint(checkpoint_path)
    resumable = {
        name: record
        for name, record in checkpoint.items()
        if record_matches(
            record,
            source=source,
            timeout=args.timeout,
            min_avail_mb=args.min_avail_mb,
        )
    }
    selected_names = [path.name for path in paths]
    results: dict[str, dict] = {
        name: resumable[name] for name in selected_names if name in resumable
    }
    pending = [path for path in paths if path.name not in results]

    if resumable:
        log(f"resume: {sum(1 for p in paths if p.name in resumable)} test(s) already recorded")
    if not pending:
        log("nothing to run; rebuilding reports from the checkpoint")

    cpython_env, hydro_env = env_for_runs(source)
    started = time.perf_counter()
    interrupted = False
    total = len(pending)

    if pending:
        executor = ThreadPoolExecutor(max_workers=args.workers)
        futures = {}
        try:
            for path in pending:
                futures[
                    executor.submit(
                        evaluate_test,
                        path,
                        source=source,
                        cpython=cpython,
                        tea=tea,
                        timeout=args.timeout,
                        min_avail_mb=args.min_avail_mb,
                        cpython_env=cpython_env,
                        hydro_env=hydro_env,
                        log_path=log_dir / f"{path.stem}.log",
                    )
                ] = path
            for index, future in enumerate(as_completed(futures), 1):
                path = futures[future]
                try:
                    record = future.result()
                except Exception as exc:  # a worker bug must not lose the run
                    log(f"error: {path.name} crashed the worker: {exc}")
                    record = failed_record(
                        path,
                        source=source,
                        timeout=args.timeout,
                        min_avail_mb=args.min_avail_mb,
                        log_path=log_dir / f"{path.stem}.log",
                        error=str(exc),
                    )
                results[path.name] = record
                try:
                    append_checkpoint(checkpoint_path, record)
                except OSError as exc:
                    log(f"error: cannot append checkpoint {checkpoint_path}: {exc}")
                log(
                    f"[{index}/{total}] {path.name}: {record['status']}"
                    + (f" — {record['error']}" if record.get("error") else "")
                )
        except KeyboardInterrupt:
            interrupted = True
            _STOP.set()
            log("interrupt: terminating running test groups; checkpoint is intact")
            terminate_active()
        finally:
            executor.shutdown(wait=True, cancel_futures=interrupted)
    else:
        counts = Counter(record["status"] for record in results.values())
        detail = ", ".join(
            f"{status}={count}" for status, count in sorted(counts.items())
        )
        log("checkpoint: " + (detail or "empty"))

    elapsed = round(time.perf_counter() - started, 1)
    date = utc_now()
    records = [results[name] for name in selected_names if name in results]
    summary = summarize(records, len(paths))
    groups = group_failures(records)

    only = (
        sorted(part.strip() for part in args.only.split(",") if part.strip())
        if args.only
        else None
    )
    meta = {
        "date": date,
        "python_target": compat.TARGET_PYTHON,
        "cpython": {"path": str(cpython), "version": cpython_version},
        "hydro": {"path": str(tea), "version": compat.hydro_version(tea)},
        "source": str(source),
        "checkpoint": str(checkpoint_path),
        "log_dir": str(log_dir),
        "json_path": str(json_path),
        "md_path": str(md_path),
        "options": {
            "timeout": args.timeout,
            "min_avail_mb": args.min_avail_mb,
            "workers": args.workers,
            "limit": args.limit,
            "only": only,
            "resume": args.resume,
        },
        "complete": len(records) == len(paths) and not interrupted,
        "interrupted": interrupted,
        "elapsed_seconds": elapsed,
        "cwd": str(ROOT),
    }
    payload = build_payload(meta=meta, summary=summary, groups=groups, records=records)
    compat.write_text(
        json_path,
        json.dumps(payload, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
    )
    compat.write_text(md_path, render_markdown(meta=meta, summary=summary, groups=groups))

    log(
        f"triage: {summary['completed']}/{summary['selected']} recorded "
        f"({summary['passed']} pass, {summary['failed']} fail, "
        f"{summary['backlog']} backlog) in {elapsed:g}s -> {json_path}, {md_path}"
    )
    for status in STATUSES:
        if summary["by_status"][status]:
            log(f"  {status}: {summary['by_status'][status]}")
    if interrupted:
        log(f"interrupted; continue with: {sys.argv[0]} --resume")
        return 130
    return 0


if __name__ == "__main__":
    sys.exit(main())
