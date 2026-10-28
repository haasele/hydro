#!/usr/bin/env python3
"""Differential ecosystem compatibility gate for Hydro.

A pinned program must produce byte-identical stdout under three runners:

* the CPython 3.14.9 oracle,
* Hydro JIT (``tea infuse``), and
* Hydro AOT (``tea build`` once, then the produced binary).

All three processes must exit 0. The cases in ``benchmarks/ecosystem/cases``
exercise real NumPy, pandas, pydantic and pydantic-core operations; an import
check alone is never reported as ecosystem compatibility.

    python3 scripts/ecosystem_compat.py
    python3 scripts/ecosystem_compat.py --only numpy --suite smoke
    python3 scripts/ecosystem_compat.py --tea ../../tea/target/debug/tea
    python3 scripts/ecosystem_compat.py --gate --json /tmp/ecosystem.json
    python3 scripts/ecosystem_compat.py --only pydantic --tests /path/to/checkout

The site-packages tree is ``--site``, ``$HYDRO_ECOSYSTEM_SITE_PACKAGES`` or the
``site_packages`` value in ``benchmarks/ecosystem/pins.json``. Exact versions and
dist-info RECORD digests from that tree are validated before cases run; a
version mismatch fails the gate. Each case runs in a fresh temporary working
directory with a per-process timeout. Results are written to
``benchmarks/ecosystem/latest.json`` and to the generated block in
``docs/ECOSYSTEM.md``.

Exit codes: 0 report produced (case failures are data unless ``--gate`` is
given), 1 pin mismatch or ``--gate`` failure, 2 missing toolchain/site/case.
"""

from __future__ import annotations

import argparse
import difflib
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent  # repository root (parent of scripts/)
BENCH_DIR = ROOT / "benchmarks" / "ecosystem"
CASES_DIR = BENCH_DIR / "cases"
PINS_PATH = BENCH_DIR / "pins.json"
DEFAULT_JSON = BENCH_DIR / "latest.json"
DEFAULT_MD = ROOT / "docs" / "ECOSYSTEM.md"

TARGET_PYTHON = "3.14.9"
DEFAULT_TIMEOUT = 120.0
VERSION_TIMEOUT = 10.0
MAX_ERROR_CHARS = 400
MAX_STDOUT_CHARS = 4000
DIFF_LINES = 24

ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
HYDRO_CODE_RE = re.compile(r"error\[(HYD\d+)\]")
TRACEBACK_MARKER = "Traceback (most recent call last)"

MARKER_BEGIN = "<!-- BEGIN GENERATED ECOSYSTEM RESULTS -->"
MARKER_END = "<!-- END GENERATED ECOSYSTEM RESULTS -->"

PYTEST_WRAPPER = """\
import os
import sys

sys.path.insert(0, os.getcwd())

import pytest

raise SystemExit(int(pytest.main(sys.argv[1:])))
"""

HEADER = """\
# Hydro ecosystem compatibility

Differential gate for real third-party wheels. A program is not "compatible"
because an import succeeds: the same source must produce the same stdout under
the pinned CPython 3.14.9 oracle, under Hydro JIT (`tea infuse`) and under a
Hydro AOT binary (`tea build`), and all three processes must exit 0.

The gate runs the pinned NumPy 2.5.0, pandas 3.1.0, pydantic 2.14.1 and
pydantic-core 2.50.1 wheels from a site-packages tree (default `/tmp/pkgs`,
override with `--site` or `$HYDRO_ECOSYSTEM_SITE_PACKAGES`). Exact versions and
dist-info RECORD digests are pinned in `benchmarks/ecosystem/pins.json`; a
version or RECORD mismatch fails the gate. Wheel digests are `null` until the
original artifacts are pinned and are never reported as verified.

```sh
python3 scripts/ecosystem_compat.py                 # imports + smoke, all runners
python3 scripts/ecosystem_compat.py --only numpy
python3 scripts/ecosystem_compat.py --suite smoke --no-aot
python3 scripts/ecosystem_compat.py --gate --json /tmp/ecosystem.json
python3 scripts/ecosystem_compat.py --only pydantic --tests .hydro/ecosystem/sources/pydantic-2.14.1
```

Cases live in `benchmarks/ecosystem/cases/` and print deterministic output (no
timestamps, host paths or unseeded randomness). Each case gets a fresh temporary
working directory, `PYTHONPATH`/`HYDRO_SITE_PACKAGES` point only at the pinned
site-packages tree, and every process is bounded by `--timeout`.

Status classes: `pass`, `cpython_fail` (the oracle itself failed, so the case is
incomparable), `compile_error` (Hydro compile diagnostic), `runtime_error`
(Hydro exited nonzero), `timeout`, `output_diff` (all runners exited 0 but
stdout differs), `aot_build_error` (the AOT build failed or produced no
executable; distinct from a runtime failure) and `pin_mismatch` (a package is
not installed at the pinned version/RECORD digest, so its cases are not run).
Results are written to `benchmarks/ecosystem/latest.json` and the generated
block below. A pass means the listed pinned programs agree across the three
runners; it is not a claim that the libraries are fully compatible, that
arbitrary programs work, or that an upstream test suite passed.

Without `--gate` the exit code is 0 when the report was produced, 1 when the
package pins do not match, and 2 when the harness could not run. `--gate`
additionally exits 1 on any failing case or upstream-test failure.

## Upstream suites

`--tests DIR` is an explicit upstream checkout hook: pytest runs through
`python -m pytest` under the CPython oracle and through a pytest wrapper under
Hydro. The selected test paths come from `upstream_tests` in
`benchmarks/ecosystem/pins.json`; pytest must be importable from the pinned
site-packages tree. Pass the package checkout as `DIR` and use
`--only <package>` when the pinned paths belong to one package. Missing pinned
paths, timeouts and nonzero exits are reported, nothing is skipped silently, and
a missing pytest installation is a failure. No upstream result is claimed until
such a run exists: the generated block only contains an upstream section after a
real `--tests` run.
"""


# --------------------------------------------------------------------- helpers
def log(message: str) -> None:
    print(message, file=sys.stderr, flush=True)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8", "replace")).hexdigest()


def normalized_name(name: str) -> str:
    return re.sub(r"[-_.]+", "-", name).strip().lower()


def resolve_tea(cli_value: str | None) -> tuple[Path | None, str | None]:
    """Resolve the Hydro binary; an explicit --tea path must be usable."""
    if cli_value:
        candidate = Path(cli_value).expanduser()
        if not (candidate.is_file() and os.access(candidate, os.X_OK)):
            return None, f"Hydro binary not found or not executable: {candidate}"
        return candidate.resolve(), None
    env = os.environ.get("HYDRO_BIN")
    if env:
        candidate = Path(env).expanduser()
        if not (candidate.is_file() and os.access(candidate, os.X_OK)):
            return None, f"$HYDRO_BIN is not an executable file: {candidate}"
        return candidate.resolve(), None
    for candidate in (
        ROOT.parent / "tea" / "target" / "release" / "tea",
        ROOT.parent / "tea" / "target" / "debug" / "tea",
    ):
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate.resolve(), None
    return None, (
        "Hydro binary not found (tried --tea, $HYDRO_BIN, "
        f"{ROOT.parent / 'tea' / 'target' / 'release' / 'tea'}, "
        f"{ROOT.parent / 'tea' / 'target' / 'debug' / 'tea'})"
    )


def version_of(executable: Path) -> str | None:
    try:
        proc = subprocess.run(
            [str(executable), "--version"],
            cwd=str(ROOT),
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=VERSION_TIMEOUT,
            stdin=subprocess.DEVNULL,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    text = (proc.stdout or "").strip() or (proc.stderr or "").strip()
    first = text.splitlines()[0].strip() if text else ""
    return first or None


def cpython_version(executable: Path) -> str | None:
    raw = version_of(executable)
    if raw is None:
        return None
    if raw.startswith("Python "):
        raw = raw[len("Python ") :].strip()
    return raw or None


def hydro_version(tea: Path) -> str:
    raw = version_of(tea)
    if raw is None:
        return "unknown"
    lines = [line.strip() for line in raw.splitlines() if line.strip()]
    return lines[0] if lines else "unknown"


def _uv_python_find() -> tuple[Path | None, str]:
    uv = shutil.which("uv")
    if not uv:
        return None, "uv is not installed"
    try:
        proc = subprocess.run(
            [uv, "python", "find", TARGET_PYTHON],
            cwd=str(ROOT),
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=VERSION_TIMEOUT,
            stdin=subprocess.DEVNULL,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return None, f"cannot run `uv python find`: {exc}"
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "").strip().splitlines()
        tail = detail[-1] if detail else f"exit {proc.returncode}"
        return None, f"`uv python find {TARGET_PYTHON}` failed: {tail}"
    first = next(
        (line.strip() for line in (proc.stdout or "").splitlines() if line.strip()),
        "",
    )
    if not first:
        return None, f"`uv python find {TARGET_PYTHON}` returned no path"
    candidate = Path(first).expanduser()
    if not (candidate.is_file() and os.access(candidate, os.X_OK)):
        return None, f"uv returned a non-executable path: {candidate}"
    return candidate, ""


def resolve_cpython(cli_value: str | None) -> tuple[Path | None, str | None, str | None]:
    """Return (oracle, version, error); the oracle must be exactly 3.14.9."""
    explicit = cli_value or os.environ.get("HYDRO_CPYTHON")
    if explicit:
        origin = "--cpython" if cli_value else "$HYDRO_CPYTHON"
        candidate = Path(explicit).expanduser()
        if not (candidate.is_file() and os.access(candidate, os.X_OK)):
            return None, None, f"{origin} is not an executable file: {candidate}"
        version = cpython_version(candidate)
        if version is None:
            return None, None, f"cannot determine the version of {origin} {candidate}"
        if version != TARGET_PYTHON:
            return None, version, (
                f"{origin} {candidate} reports {version}; this gate pins CPython "
                f"{TARGET_PYTHON}. Install it with `uv python install {TARGET_PYTHON}` "
                "or select the pinned interpreter."
            )
        return candidate.resolve(), version, None

    candidate, reason = _uv_python_find()
    if candidate is not None:
        version = cpython_version(candidate)
        if version == TARGET_PYTHON:
            return candidate.resolve(), version, None
        reason = f"{candidate} reports {version or 'an unknown version'}"
    return None, None, (
        f"CPython {TARGET_PYTHON} oracle unavailable ({reason}). Install it with "
        f"`uv python install {TARGET_PYTHON}` or pass --cpython PATH; this gate "
        "does not fall back to another interpreter."
    )


def split_paths(raw: str) -> list[Path]:
    parts: list[str] = []
    for chunk in raw.split(os.pathsep):
        for piece in chunk.split(","):
            piece = piece.strip()
            if piece:
                parts.append(piece)
    return [Path(part).expanduser() for part in parts]


def resolve_site(cli_value: str | None, pins: dict) -> tuple[list[Path] | None, str | None]:
    raw = cli_value or os.environ.get("HYDRO_ECOSYSTEM_SITE_PACKAGES")
    if not raw:
        raw = pins.get("site_packages") or ""
    if not raw:
        return None, (
            "no site-packages tree configured (use --site DIR, "
            "$HYDRO_ECOSYSTEM_SITE_PACKAGES or pins.json site_packages)"
        )
    sites: list[Path] = []
    for candidate in split_paths(str(raw)):
        if not candidate.is_dir():
            return None, f"site-packages directory not found: {candidate}"
        resolved = candidate.resolve()
        if resolved not in sites:
            sites.append(resolved)
    if not sites:
        return None, f"no usable site-packages directory in {raw!r}"
    return sites, None


def load_pins(path: Path) -> dict:
    with path.open(encoding="utf-8") as handle:
        return json.load(handle)


# ------------------------------------------------------------------------ pins
def scan_site(sites: list[Path]) -> dict[str, dict]:
    """Map normalized distribution name -> installed metadata."""
    found: dict[str, dict] = {}
    for site in sites:
        for dist_info in sorted(site.glob("*.dist-info")):
            metadata = dist_info / "METADATA"
            if not metadata.is_file():
                continue
            name = version = None
            try:
                with metadata.open(encoding="utf-8", errors="replace") as handle:
                    for line in handle:
                        if name is None and line.startswith("Name:"):
                            name = line.split(":", 1)[1].strip()
                        elif version is None and line.startswith("Version:"):
                            version = line.split(":", 1)[1].strip()
                        if name is not None and version is not None:
                            break
            except OSError:
                continue
            if not name or not version:
                continue
            found.setdefault(
                normalized_name(name),
                {
                    "name": name,
                    "version": version,
                    "dist_info": dist_info.name,
                    "path": dist_info,
                    "site": site,
                },
            )
    return found


def validate_pins(pins: dict, sites: list[Path]) -> list[dict]:
    """Verify exact versions and RECORD digests; never raise for drift."""
    installed = scan_site(sites)
    rows: list[dict] = []
    for package in pins.get("packages", []):
        name = str(package.get("name", ""))
        expected = str(package.get("version", ""))
        info = installed.get(normalized_name(name))
        row = {
            "name": name,
            "role": package.get("role"),
            "expected": expected,
            "actual": None,
            "dist_info": package.get("dist_info"),
            "record_sha256_expected": package.get("dist_info_record_sha256"),
            "record_sha256_actual": None,
            "record_ok": None,
            "wheel_sha256": package.get("wheel_sha256"),
            "wheel_pinned": package.get("wheel_sha256") is not None,
            "ok": False,
            "error": None,
        }
        if info is None:
            row["error"] = "not installed in the configured site-packages tree"
            rows.append(row)
            continue
        row["actual"] = info["version"]
        row["dist_info"] = info["dist_info"]
        if info["version"] != expected:
            row["error"] = f"version {info['version']} does not match the pinned {expected}"
            rows.append(row)
            continue
        record = info["path"] / "RECORD"
        if not record.is_file():
            row["error"] = f"{info['dist_info']}/RECORD is missing"
            rows.append(row)
            continue
        actual_hash = sha256_file(record)
        row["record_sha256_actual"] = actual_hash
        pinned_hash = package.get("dist_info_record_sha256")
        if pinned_hash is None:
            row["error"] = "dist-info RECORD digest is not pinned"
            rows.append(row)
            continue
        row["record_ok"] = actual_hash == pinned_hash
        if not row["record_ok"]:
            row["error"] = "dist-info RECORD digest differs from the pinned value"
            rows.append(row)
            continue
        row["ok"] = True
        rows.append(row)
    return rows


# ------------------------------------------------------------------- execution
def make_env(sites: list[Path], stdlib: Path | None = None) -> dict[str, str]:
    env = dict(os.environ)
    joined = os.pathsep.join(str(site) for site in sites)
    # Only the pinned tree is importable; a host PYTHONPATH or venv must not
    # silently supply a different build of the same distribution.
    env["PYTHONPATH"] = joined
    env["HYDRO_SITE_PACKAGES"] = joined
    if stdlib is not None:
        env["HYDRO_STDLIB"] = str(stdlib)
    env.pop("VIRTUAL_ENV", None)
    env.pop("CONDA_PREFIX", None)
    env.pop("PYTHONHOME", None)
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    env["PYTHONHASHSEED"] = "0"
    env["PYTHONIOENCODING"] = "utf-8"
    env["PYTHONUTF8"] = "1"
    env["PYTHONNOUSERSITE"] = "1"
    env.setdefault("NO_COLOR", "1")
    return env


def run_process(cmd: list[str], timeout: float, env: dict[str, str], cwd: Path) -> dict:
    """Run one program; never raises for a failing or timed-out program."""
    started = time.perf_counter()

    def text(value: object) -> str:
        if value is None:
            return ""
        if isinstance(value, bytes):
            return value.decode("utf-8", "replace")
        return str(value)

    try:
        proc = subprocess.run(
            cmd,
            cwd=str(cwd),
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
            stdin=subprocess.DEVNULL,
            env=env,
        )
        return {
            "exit": proc.returncode,
            "stdout": proc.stdout or "",
            "stderr": proc.stderr or "",
            "timed_out": False,
            "duration_ms": round((time.perf_counter() - started) * 1000.0, 1),
        }
    except subprocess.TimeoutExpired as exc:
        return {
            "exit": None,
            "stdout": text(exc.stdout),
            "stderr": text(exc.stderr),
            "timed_out": True,
            "duration_ms": round((time.perf_counter() - started) * 1000.0, 1),
        }
    except OSError as exc:
        return {
            "exit": None,
            "stdout": "",
            "stderr": str(exc),
            "timed_out": False,
            "duration_ms": round((time.perf_counter() - started) * 1000.0, 1),
        }


def slim_run(run: dict) -> dict:
    stdout = run.get("stdout") or ""
    stderr = run.get("stderr") or ""
    if len(stdout) > MAX_STDOUT_CHARS:
        stdout = stdout[:MAX_STDOUT_CHARS] + "\n… (truncated)"
    return {
        "exit": run.get("exit"),
        "timed_out": bool(run.get("timed_out")),
        "duration_ms": run.get("duration_ms"),
        "stdout_sha256": sha256_text(run.get("stdout") or ""),
        "stdout_bytes": len((run.get("stdout") or "").encode("utf-8", "replace")),
        "stdout": stdout,
        "stderr_tail": stderr[-MAX_STDOUT_CHARS:],
    }


def last_non_empty_line(text: str) -> str:
    for line in reversed((text or "").splitlines()):
        stripped = line.strip()
        if stripped:
            return stripped
    return ""


def extract_error(stderr: str, stdout: str, workdir: Path) -> tuple[str | None, str]:
    """Return (HYD code, human-readable first error) for a Hydro run."""
    text = ANSI_RE.sub("", stderr or "")
    code: str | None = None
    message = ""
    match = HYDRO_CODE_RE.search(text)
    if match:
        code = match.group(1)
        for line in text.splitlines():
            if match.group(0) in line:
                message = line.strip()
                break
    if not message and TRACEBACK_MARKER in text:
        message = last_non_empty_line(text)
    if not message:
        message = last_non_empty_line(text) or last_non_empty_line(stdout or "")
    if workdir:
        message = message.replace(str(workdir), "<tmp>")
    return code, message[:MAX_ERROR_CHARS]


def diff_text(expected: str, actual: str, label: str) -> str:
    expected_lines = expected.splitlines()
    actual_lines = actual.splitlines()
    if expected_lines == actual_lines:
        return (
            "stdout differs only in line endings or trailing bytes "
            f"(cpython {len(expected)} bytes, {label} {len(actual)} bytes)"
        )
    diff = list(
        difflib.unified_diff(
            expected_lines,
            actual_lines,
            fromfile="cpython",
            tofile=label,
            lineterm="",
            n=2,
        )
    )
    if len(diff) > DIFF_LINES:
        diff = diff[:DIFF_LINES] + ["… (truncated)"]
    return "\n".join(diff)


# ---------------------------------------------------------------------- cases
def build_cases(pins: dict) -> list[dict]:
    cases: list[dict] = []
    for package in pins.get("packages", []):
        name = str(package.get("name", ""))
        import_name = package.get("import")
        if import_name:
            cases.append(
                {
                    "id": f"{name}:import",
                    "package": name,
                    "kind": "import",
                    "import_name": str(import_name),
                    "source": None,
                }
            )
        case_name = package.get("case")
        if case_name:
            cases.append(
                {
                    "id": f"{name}:smoke",
                    "package": name,
                    "kind": "smoke",
                    "import_name": None,
                    "source": CASES_DIR / str(case_name),
                }
            )
    return cases


def case_matches(case: dict, token: str) -> bool:
    fields = {case["id"], case["package"], case["kind"]}
    source = case.get("source")
    if source is not None:
        fields.add(source.name)
        fields.add(source.stem)
    return token in fields


def select_cases(cases: list[dict], suite: str, only: str | None) -> tuple[list[dict], list[str]]:
    selected = list(cases)
    unknown: list[str] = []
    if only:
        tokens = [part.strip() for part in only.split(",") if part.strip()]
        for token in tokens:
            if not any(case_matches(case, token) for case in cases):
                unknown.append(token)
        selected = [
            case for case in selected if any(case_matches(case, token) for token in tokens)
        ]
    if suite != "all":
        selected = [case for case in selected if case["kind"] == suite]
    return selected, unknown


def classify_and_detail(
    cpython: dict,
    jit: dict,
    aot: dict | None,
    build: dict | None,
    build_ok: bool,
    use_aot: bool,
    timeout: float,
    workdir: Path,
) -> tuple[str, str | None, dict[str, str]]:
    runs: list[tuple[str, dict]] = [("jit", jit)]
    if use_aot:
        runs.append(("aot", aot))  # type: ignore[arg-type]

    if cpython["timed_out"]:
        return "timeout", f"CPython oracle timed out after {timeout:g}s", {}
    if cpython["exit"] != 0:
        detail = last_non_empty_line(cpython["stderr"]) or f"CPython exited {cpython['exit']}"
        return "cpython_fail", detail[:MAX_ERROR_CHARS], {}

    if use_aot and not build_ok:
        if build is None:
            return "aot_build_error", "AOT build did not run", {}
        if build["timed_out"]:
            return "timeout", f"AOT build timed out after {timeout:g}s", {}
        code, message = extract_error(build["stderr"], build["stdout"], workdir)
        detail = message or last_non_empty_line(build["stderr"]) or last_non_empty_line(build["stdout"])
        if not detail:
            detail = "AOT build produced no executable"
        prefix = "AOT build failed"
        if build["exit"] is not None:
            prefix += f" (exit {build['exit']})"
        if code:
            prefix += f" [{code}]"
        return "aot_build_error", f"{prefix}: {detail}"[:MAX_ERROR_CHARS], {}

    for name, run in runs:
        if run["timed_out"]:
            return "timeout", f"{name} timed out after {timeout:g}s", {}
    for name, run in runs:
        if run["exit"] != 0:
            code, message = extract_error(run["stderr"], run["stdout"], workdir)
            detail = message or f"{name} exited {run['exit']}"
            if code:
                return "compile_error", f"{name}: {detail}"[:MAX_ERROR_CHARS], {}
            return "runtime_error", f"{name}: {detail}"[:MAX_ERROR_CHARS], {}

    differences: dict[str, str] = {}
    for name, run in runs:
        if run["stdout"] != cpython["stdout"]:
            differences[name] = diff_text(cpython["stdout"], run["stdout"], name)
    if differences:
        detail = "stdout differs: " + ", ".join(sorted(differences))
        return "output_diff", detail, differences
    return "pass", None, {}


def run_case(
    case: dict,
    cpython: Path,
    tea: Path,
    sites: list[Path],
    stdlib: Path | None,
    timeout: float,
    use_aot: bool,
) -> dict:
    with tempfile.TemporaryDirectory(prefix="hydro-ecosystem-") as tmp:
        workdir = Path(tmp)
        if case["kind"] == "import":
            stem = re.sub(r"[^A-Za-z0-9_]+", "_", case["package"])
            script = workdir / f"import_{stem}.py"
            script.write_text(f"import {case['import_name']}\n", encoding="utf-8")
        else:
            source = case["source"]
            script = workdir / source.name
            shutil.copyfile(source, script)

        env = make_env(sites, stdlib)
        cpython_run = run_process([str(cpython), str(script)], timeout, env, workdir)
        row: dict = {
            "id": case["id"],
            "package": case["package"],
            "kind": case["kind"],
            "status": None,
            "detail": None,
            "differences": {},
            "runs": {"cpython": slim_run(cpython_run), "jit": None, "aot": None},
            "aot_build": None,
        }

        jit_run = run_process(
            [str(tea), "infuse", "--color=never", str(script)],
            timeout,
            env,
            workdir,
        )
        row["runs"]["jit"] = slim_run(jit_run)
        aot_run = None
        build_run = None
        build_ok = False
        if use_aot:
            binary = workdir / "aot_binary"
            build_run = run_process(
                [
                    str(tea),
                    "build",
                    "--color=never",
                    str(script),
                    "-o",
                    str(binary),
                ],
                timeout,
                env,
                workdir,
            )
            build_ok = (
                not build_run["timed_out"]
                and build_run["exit"] == 0
                and binary.is_file()
                and os.access(binary, os.X_OK)
            )
            row["aot_build"] = slim_run(build_run)
            if build_ok:
                aot_run = run_process([str(binary)], timeout, env, workdir)
                row["runs"]["aot"] = slim_run(aot_run)

        status, detail, differences = classify_and_detail(
            cpython_run,
            jit_run,  # type: ignore[arg-type]
            aot_run,
            build_run,
            build_ok,
            use_aot,
            timeout,
            workdir,
        )
        row["status"] = status
        row["detail"] = detail
        row["differences"] = differences
        return row


def skipped_case(case: dict, status: str, detail: str) -> dict:
    return {
        "id": case["id"],
        "package": case["package"],
        "kind": case["kind"],
        "status": status,
        "detail": detail,
        "differences": {},
        "runs": {"cpython": None, "jit": None, "aot": None},
        "aot_build": None,
    }


# ------------------------------------------------------------------- upstream
def upstream_entries(pins: dict, only: str | None) -> list[dict]:
    entries = list(pins.get("upstream_tests") or [])
    if not only:
        return entries
    tokens = [part.strip() for part in only.split(",") if part.strip()]
    return [
        entry
        for entry in entries
        if any(token == entry.get("package") for token in tokens)
    ]


def check_upstream(
    entries: list[dict],
    tests_dir: Path,
    cpython: Path,
    tea: Path,
    sites: list[Path],
    stdlib: Path | None,
    timeout: float,
) -> list[dict]:
    env = make_env(sites, stdlib)
    rows: list[dict] = []
    for entry in entries:
        package = str(entry.get("package", ""))
        relative_paths = [str(path) for path in entry.get("paths") or []]
        paths = [tests_dir / relative for relative in relative_paths]
        missing = [
            relative
            for relative, path in zip(relative_paths, paths)
            if not path.is_file()
        ]
        if missing:
            rows.append(
                {
                    "package": package,
                    "paths": relative_paths,
                    "root": str(tests_dir),
                    "status": "missing_tests",
                    "detail": "missing pinned test files: " + ", ".join(missing),
                    "cpython": None,
                    "hydro": None,
                }
            )
            continue

        args = [
            "-q",
            "--no-header",
            "-p",
            "no:cacheprovider",
            "-o",
            "addopts=",
            *[str(path) for path in paths],
        ]
        cpython_run = run_process(
            [str(cpython), "-m", "pytest", *args], timeout, env, tests_dir
        )
        with tempfile.TemporaryDirectory(prefix="hydro-ecosystem-pytest-") as tmp:
            wrapper = Path(tmp) / "run_pytest.py"
            wrapper.write_text(PYTEST_WRAPPER, encoding="utf-8")
            hydro_run = run_process(
                [str(tea), "infuse", "--color=never", str(wrapper), *args],
                timeout,
                env,
                tests_dir,
            )

        if cpython_run["timed_out"]:
            status = "cpython_fail"
            detail = f"CPython oracle timed out after {timeout:g}s"
        elif cpython_run["exit"] != 0:
            status = "cpython_fail"
            detail = last_non_empty_line(cpython_run["stderr"]) or (
                f"CPython pytest exited {cpython_run['exit']}"
            )
        elif hydro_run["timed_out"]:
            status = "timeout"
            detail = f"Hydro pytest timed out after {timeout:g}s"
        elif hydro_run["exit"] != 0:
            status = "hydro_fail"
            code, message = extract_error(hydro_run["stderr"], hydro_run["stdout"], tests_dir)
            detail = message or f"Hydro pytest exited {hydro_run['exit']}"
            if code:
                detail = f"{code}: {detail}"
            if hydro_run["exit"] == 5:
                detail = "no tests collected (pytest exit 5): " + detail
        else:
            status = "pass"
            detail = None

        rows.append(
            {
                "package": package,
                "paths": relative_paths,
                "root": str(tests_dir),
                "status": status,
                "detail": detail[:MAX_ERROR_CHARS] if detail else None,
                "cpython": slim_run(cpython_run),
                "hydro": slim_run(hydro_run),
            }
        )
    return rows


# --------------------------------------------------------------------- reports
def summarize(cases: list[dict]) -> dict:
    by_status = Counter(case["status"] for case in cases)
    total = len(cases)
    passed = by_status.get("pass", 0)
    return {
        "total": total,
        "pass": passed,
        "failed": total - passed,
        "by_status": dict(sorted(by_status.items())),
    }


def md_cell(value: object, limit: int | None = None) -> str:
    text = " ".join(str(value).split())
    if limit is not None and len(text) > limit:
        text = text[: limit - 1].rstrip() + "…"
    return text.replace("|", "\\|") or "-"


def run_cell(run: dict | None) -> str:
    if run is None:
        return "-"
    if run.get("timed_out"):
        return "timeout"
    if run.get("exit") == 0:
        return "ok"
    if run.get("exit") is None:
        return "spawn error"
    return f"exit {run['exit']}"


def render_pins_table(rows: list[dict]) -> list[str]:
    lines = [
        "| Package | Expected | Actual | RECORD SHA-256 | Wheel SHA-256 | Status |",
        "| --- | --- | --- | --- | --- | --- |",
    ]
    for row in rows:
        wheel = row["wheel_sha256"] if row["wheel_pinned"] else "not pinned"
        record = row["record_sha256_actual"] or row["record_sha256_expected"] or "-"
        status = "ok" if row["ok"] else "FAILED: " + (row["error"] or "invalid")
        lines.append(
            "| `{}` | {} | {} | `{}` | {} | {} |".format(
                md_cell(row["name"]),
                md_cell(row["expected"]),
                md_cell(row["actual"] or "missing"),
                md_cell(record),
                md_cell(wheel),
                md_cell(status),
            )
        )
    return lines


def render_case_table(cases: list[dict]) -> list[str]:
    lines = [
        "| Case | CPython | Hydro JIT | Hydro AOT | Status | Detail |",
        "| --- | --- | --- | --- | --- | --- |",
    ]
    for case in cases:
        runs = case.get("runs") or {}
        jit = runs.get("jit")
        aot = runs.get("aot")
        lines.append(
            "| `{}` | {} | {} | {} | {} | {} |".format(
                md_cell(case["id"]),
                run_cell(runs.get("cpython")),
                run_cell(jit),
                run_cell(aot),
                md_cell(case["status"]),
                md_cell(case.get("detail") or "-", 120),
            )
        )
    return lines


def render_upstream_table(rows: list[dict]) -> list[str]:
    lines = [
        "| Package | Tests | CPython | Hydro | Status | Detail |",
        "| --- | --- | --- | --- | --- | --- |",
    ]
    for row in rows:
        lines.append(
            "| `{}` | {} | {} | {} | {} | {} |".format(
                md_cell(row["package"]),
                md_cell(", ".join(row["paths"])),
                run_cell(row.get("cpython")),
                run_cell(row.get("hydro")),
                md_cell(row["status"]),
                md_cell(row.get("detail") or "-", 120),
            )
        )
    return lines


def render_markdown(
    meta: dict,
    pins_rows: list[dict],
    counts: dict,
    cases: list[dict],
    upstream: list[dict] | None,
) -> str:
    out: list[str] = []
    out.append(
        "Environment: {} (`{}`) · Hydro `{}` · binary `{}` · site `{}`".format(
            meta["python"],
            meta["python_executable"],
            meta["tea"]["version"],
            meta["tea"]["path"],
            os.pathsep.join(meta["site_packages"]),
        )
    )
    out.append("")
    out.append("## Package pins")
    out.append("")
    out.append("Versions and dist-info RECORD digests are verified before any case runs.")
    out.append("")
    out.extend(render_pins_table(pins_rows))
    out.append("")

    out.append("## Summary")
    out.append("")
    out.append("| Status | Cases |")
    out.append("| --- | ---: |")
    for status, count in counts["by_status"].items():
        out.append(f"| {md_cell(status)} | {count} |")
    out.append(f"| **Total** | {counts['total']} |")
    out.append("")
    out.append(
        "A case passes only when the oracle, Hydro JIT and (unless `--no-aot`) the "
        "AOT binary all exit 0 with byte-identical stdout."
    )
    out.append("")

    out.append("## Cases")
    out.append("")
    out.extend(render_case_table(cases))
    out.append("")

    if upstream:
        out.append("## Upstream tests (`--tests`)")
        out.append("")
        out.append(
            "A row passes only when `python -m pytest` exits 0 under the CPython "
            "oracle and the Hydro pytest wrapper. Missing pinned files are failures, "
            "not skips."
        )
        out.append("")
        out.extend(render_upstream_table(upstream))
        out.append("")

    return "\n".join(out).rstrip() + "\n"


def write_report(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def splice_markdown(path: Path, body: str) -> None:
    """Replace only the generated block; keep hand-written prose around it."""
    block = f"{MARKER_BEGIN}\n\n{body}\n{MARKER_END}\n"
    if path.is_file():
        existing = path.read_text(encoding="utf-8")
        begin = existing.find(MARKER_BEGIN)
        end = existing.find(MARKER_END)
        if begin != -1 and end != -1 and begin < end:
            document = (
                existing[:begin]
                + block
                + existing[end + len(MARKER_END) :].lstrip("\n")
            )
            write_report(path, document)
            return
    write_report(path, HEADER + "\n" + block)


# ------------------------------------------------------------------------ main
def resolve_stdlib(cli_value: str | None, cpython: Path) -> Path | None:
    """Resolve the CPython-3.14.9 `Lib` tree used as HYDRO_STDLIB."""
    candidates: list[Path] = []
    if cli_value:
        candidates.append(Path(cli_value).expanduser())
    env = os.environ.get("HYDRO_STDLIB")
    if env:
        candidates.append(Path(env).expanduser())
    major, minor = TARGET_PYTHON.split(".")[:2]
    candidates.append(cpython.parent.parent / "lib" / f"python{major}.{minor}")
    for candidate in candidates:
        if candidate.is_dir():
            return candidate.resolve()
    return None


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    ap = argparse.ArgumentParser(
        description="Differential ecosystem gate: CPython vs Hydro JIT/AOT.",
    )
    ap.add_argument("--tea", help="path to the Hydro binary (default: $HYDRO_BIN, release, debug)")
    ap.add_argument(
        "--cpython",
        metavar="PATH",
        help=f"CPython {TARGET_PYTHON} oracle (default: $HYDRO_CPYTHON, uv lookup)",
    )
    ap.add_argument(
        "--site",
        metavar="DIR",
        help="site-packages tree with the pinned wheels "
        "(default: $HYDRO_ECOSYSTEM_SITE_PACKAGES, pins.json site_packages)",
    )
    ap.add_argument("--stdlib", metavar="DIR", help="CPython 3.14.9 Lib tree for HYDRO_STDLIB (default: $HYDRO_STDLIB or the oracle's Lib)")
    ap.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT, help="per-process timeout in seconds")
    ap.add_argument("--only", help="comma-separated package/case names to run")
    ap.add_argument(
        "--suite",
        choices=("import", "smoke", "all"),
        default="all",
        help="import probes, smoke cases or both (default: all)",
    )
    ap.add_argument("--json", metavar="PATH", help=f"JSON output path (default: {DEFAULT_JSON})")
    ap.add_argument("--md", metavar="PATH", help=f"Markdown output path (default: {DEFAULT_MD})")
    ap.add_argument("--gate", action="store_true", help="exit 1 when any case or test fails")
    ap.add_argument("--no-aot", action="store_true", help="skip the Hydro AOT build and run")
    ap.add_argument("--tests", metavar="DIR", help="pinned upstream pytest files under DIR")
    return ap.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)

    if args.timeout <= 0:
        log("error: --timeout must be positive")
        return 1

    pins_path = PINS_PATH
    if not pins_path.is_file():
        log(f"error: pin file not found: {pins_path}")
        return 2
    try:
        pins = load_pins(pins_path)
    except (OSError, json.JSONDecodeError) as exc:
        log(f"error: cannot read {pins_path}: {exc}")
        return 2

    pinned_python = str(pins.get("python") or "")
    if pinned_python != TARGET_PYTHON:
        log(
            f"error: {pins_path} pins Python {pinned_python or '(missing)'}; "
            f"this gate targets {TARGET_PYTHON}"
        )
        return 2

    tea, hydro_error = resolve_tea(args.tea)
    if tea is None:
        log(f"error: {hydro_error}")
        return 2

    cpython, cpython_ver, cpython_error = resolve_cpython(args.cpython)
    if cpython is None or cpython_ver is None:
        log(f"error: {cpython_error}")
        return 2
    stdlib = resolve_stdlib(args.stdlib, cpython)

    sites, site_error = resolve_site(args.site, pins)
    if sites is None:
        log(f"error: {site_error}")
        return 2

    pins_rows = validate_pins(pins, sites)
    pins_ok = all(row["ok"] for row in pins_rows) and bool(pins_rows)
    valid_packages = {normalized_name(row["name"]) for row in pins_rows if row["ok"]}
    if not pins_ok:
        for row in pins_rows:
            if not row["ok"]:
                log(
                    f"error: pin mismatch for {row['name']}: "
                    f"{row['error'] or 'invalid pin'}"
                )

    all_cases = build_cases(pins)
    cases, unknown_tokens = select_cases(all_cases, args.suite, args.only)
    for token in unknown_tokens:
        log(f"warning: --only token matches no case: {token}")
    if not cases:
        log("error: no cases selected")
        return 2

    missing_sources = [
        str(case["source"])
        for case in cases
        if case.get("source") is not None and not case["source"].is_file()
    ]
    if missing_sources:
        log("error: case file(s) missing: " + ", ".join(missing_sources))
        return 2

    tests_dir: Path | None = None
    if args.tests:
        tests_dir = Path(args.tests).expanduser().resolve()
        if not tests_dir.is_dir():
            log(f"error: --tests directory not found: {tests_dir}")
            return 2

    started = time.perf_counter()
    results: list[dict] = []
    for index, case in enumerate(cases, 1):
        log(f"[{index}/{len(cases)}] {case['id']}")
        if normalized_name(case["package"]) not in valid_packages:
            results.append(
                skipped_case(
                    case,
                    "pin_mismatch",
                    "package is not installed at the pinned version/digest",
                )
            )
            continue
        results.append(
            run_case(
                case,
                cpython,
                tea,
                sites,
                stdlib,
                args.timeout,
                not args.no_aot,
            )
        )

    counts = summarize(results)

    upstream_rows: list[dict] | None = None
    upstream_note = None
    if tests_dir is not None:
        entries = upstream_entries(pins, args.only)
        if not entries:
            upstream_note = "no pinned upstream test entry selected by --only"
            upstream_rows = []
            log("warning: " + upstream_note)
        else:
            log(f"upstream: {len(entries)} package suite(s) under {tests_dir}")
            upstream_rows = check_upstream(
                entries,
                tests_dir,
                cpython,
                tea,
                sites,
                args.timeout,
            )
        if upstream_rows:
            for row in upstream_rows:
                log(f"upstream {row['package']}: {row['status']}")

    elapsed = round(time.perf_counter() - started, 1)
    meta = {
        "date": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "python": f"CPython {cpython_ver}",
        "python_version": cpython_ver,
        "python_executable": str(cpython),
        "python_target": TARGET_PYTHON,
        "python_pinned": cpython_ver == TARGET_PYTHON,
        "tea": {"path": str(tea), "version": hydro_version(tea)},
        "site_packages": [str(site) for site in sites],
        "pins": str(pins_path),
        "options": {
            "suite": args.suite,
            "only": [part.strip() for part in args.only.split(",") if part.strip()]
            if args.only
            else None,
            "timeout": args.timeout,
            "aot": not args.no_aot,
            "tests": str(tests_dir) if tests_dir else None,
        },
        "cwd": str(ROOT),
        "elapsed_seconds": elapsed,
        "pins_ok": pins_ok,
        "upstream_note": upstream_note,
    }
    payload = {
        "meta": meta,
        "pins": pins_rows,
        "counts": counts,
        "cases": results,
    }
    if upstream_rows is not None:
        payload["upstream"] = upstream_rows
        payload["meta"]["upstream_counts"] = dict(
            sorted(Counter(row["status"] for row in upstream_rows).items())
        )

    body = render_markdown(meta, pins_rows, counts, results, upstream_rows)
    json_path = Path(args.json) if args.json else DEFAULT_JSON
    md_path = Path(args.md) if args.md else DEFAULT_MD

    write_report(json_path, json.dumps(payload, indent=2, ensure_ascii=False) + "\n")
    splice_markdown(md_path, body)

    log(
        f"cases: {counts['total']} ({counts['pass']} pass, {counts['failed']} failed) "
        f"in {elapsed:g}s -> {json_path}, {md_path}"
    )
    if pins_rows:
        log(f"pins: {'ok' if pins_ok else 'MISMATCH'} ({len(pins_rows)} packages)")

    failed_upstream = bool(upstream_rows) and any(
        row["status"] != "pass" for row in upstream_rows
    )
    if not pins_ok:
        return 1
    if args.gate and (counts["failed"] > 0 or failed_upstream):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
