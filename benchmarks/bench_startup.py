#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "fastapi",
#     "uvicorn[standard]",
#     "httpx",
#     "pydantic",
#     "requests",
#     "numpy",
#     "pandas",
# ]
# ///
"""
Startup-Zeit-Benchmark (jeweils in frischen Subprozessen, erster Lauf = Warmup).

Misst: Interpreter-Start (inkl. uv-Overhead), Import-Zeiten, FastAPI-App-Erzeugung,
       uvicorn-Kaltstart bis zur ersten HTTP-200-Antwort.

  uv run bench_startup.py
  uv run bench_startup.py --runs 20 --modules numpy pandas requests
"""
import argparse
import importlib.util
import json
import os
import platform
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import textwrap
import time
import urllib.request
from pathlib import Path

DEFAULT_MODULES = [
    "json", "asyncio", "sqlite3", "csv", "decimal", "typing", "dataclasses", "http.server",
    "numpy", "pandas", "requests", "httpx", "pydantic", "fastapi", "starlette", "uvicorn",
]

APP_SOURCE = textwrap.dedent("""
    from fastapi import FastAPI
    app = FastAPI()

    @app.get("/ping")
    async def ping():
        return {"ok": True}
""")

FASTAPI_SNIPPET = ("from fastapi import FastAPI\n"
                   "app = FastAPI()\n"
                   "@app.get('/x')\n"
                   "def x(): return {}\n")


# --------------------------------------------------------------------------- Ergebnis-Helfer
def log(msg):
    print(msg, file=sys.stderr, flush=True)


def hl(key, label, value, unit, higher_is_better):
    return {"key": key, "label": label, "value": value, "unit": unit,
            "higher_is_better": higher_is_better}


def fmt_val(v, fmt):
    if v is None:
        return "-"
    if fmt and isinstance(v, (int, float)):
        return format(v, fmt)
    return str(v)


def rel_factors(sec):
    p = sec.get("primary")
    if not p:
        return None
    vals = [r.get(p) for r in sec["rows"]]
    nums = [v for v in vals if isinstance(v, (int, float)) and v > 0]
    if not nums:
        return None
    lower = sec.get("lower_is_better", True)
    best = min(nums) if lower else max(nums)
    return [None if not isinstance(v, (int, float)) or v <= 0
            else (v / best if lower else best / v) for v in vals]


def render_text(result):
    print(f"\n### {result['title']}")
    print("    " + "  ".join(f"{k}={v}" for k, v in result["meta"].items()))
    for sec in result["sections"]:
        cols = sec["columns"]
        facs = rel_factors(sec)
        heads = [c["label"] + (f" ({c['unit']})" if c.get("unit") else "") for c in cols]
        rows = [[fmt_val(r.get(c["key"]), c.get("fmt")) for c in cols] for r in sec["rows"]]
        if facs:
            heads.append("rel")
            for row, f in zip(rows, facs):
                row.append("-" if f is None else f"{f:.2f}x")
        widths = [max([len(h)] + [len(r[i]) for r in rows]) for i, h in enumerate(heads)]
        print(f"\n== {sec['title']} ==")
        if sec.get("note"):
            print(f"   {sec['note']}")
        line = lambda cells: "  ".join(
            c.ljust(w) if i == 0 else c.rjust(w) for i, (c, w) in enumerate(zip(cells, widths)))
        print(line(heads))
        print("  ".join("-" * w for w in widths))
        for r in rows:
            print(line(r))
    if result.get("insights"):
        print("\nAuswertung:")
        for s in result["insights"]:
            print(f"  -> {s}")


def emit(result, json_path):
    result["python"] = f"{platform.python_implementation()} {platform.python_version()}"
    render_text(result)
    if json_path:
        Path(json_path).write_text(json.dumps(result, indent=2, ensure_ascii=False))


# --------------------------------------------------------------------------- Messung
def run_times(cmd, runs):
    """Wall-Clock in ms; erster Lauf wird verworfen (Disk-Cache). None bei Fehler."""
    times = []
    for i in range(runs + 1):
        t = time.perf_counter()
        r = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        dt = (time.perf_counter() - t) * 1000
        if r.returncode != 0:
            return None
        if i > 0:
            times.append(dt)
    return times


def stat_row(name, times, base=None):
    med = statistics.median(times)
    return {"name": name, "min_ms": min(times), "median_ms": med, "max_ms": max(times),
            "delta_ms": max(0.0, min(times) - base) if base is not None else None}


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def uvicorn_startup(tmpdir, timeout=30):
    port = free_port()
    t = time.perf_counter()
    p = subprocess.Popen(
        [sys.executable, "-m", "uvicorn", "app_mod:app", "--port", str(port), "--log-level", "warning"],
        cwd=tmpdir, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        while time.perf_counter() - t < timeout:
            if p.poll() is not None:
                return None
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{port}/ping", timeout=0.5) as r:
                    if r.status == 200:
                        return (time.perf_counter() - t) * 1000
            except Exception:
                time.sleep(0.005)
        return None
    finally:
        p.terminate()
        try:
            p.wait(5)
        except subprocess.TimeoutExpired:
            p.kill()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--runs", type=int, default=10)
    ap.add_argument("--modules", nargs="*", default=DEFAULT_MODULES)
    ap.add_argument("--json", help="Ergebnis als JSON in diese Datei schreiben")
    args = ap.parse_args()
    py = sys.executable

    cols = [
        {"key": "name", "label": "Test"},
        {"key": "min_ms", "label": "min", "unit": "ms", "fmt": ".1f"},
        {"key": "median_ms", "label": "median", "unit": "ms", "fmt": ".1f"},
        {"key": "max_ms", "label": "max", "unit": "ms", "fmt": ".1f"},
    ]
    delta_col = {"key": "delta_ms", "label": "Δ zu leer", "unit": "ms", "fmt": "+.1f"}
    sections, insights, headlines = [], [], []

    # 1. Interpreter
    log("startup: Interpreter")
    base_t = run_times([py, "-c", "pass"], args.runs)
    base = statistics.median(base_t)
    base_min = min(base_t)
    rows = [stat_row("python -c pass", base_t),
            stat_row("python -S -c pass (ohne site)", run_times([py, "-S", "-c", "pass"], args.runs) or base_t),
            stat_row("python -I -c pass (isolated)", run_times([py, "-I", "-c", "pass"], args.runs) or base_t)]
    if shutil.which("uv"):
        t = run_times(["uv", "run", "--no-project", "--quiet", "python", "-c", "pass"], args.runs)
        if t:
            rows.append(stat_row("uv run --no-project python -c pass", t))
            insights.append(f"uv run fügt ≈ {statistics.median(t) - base:.0f} ms Overhead pro Start "
                            f"(Resolver + Env-Check) hinzu.")
    sections.append({"title": "Interpreter-Start", "columns": cols, "rows": rows,
                     "primary": "median_ms", "lower_is_better": True})
    headlines.append(hl("startup.python_empty", "python -c pass", base, "ms", False))

    # 2. Imports
    imp_rows = []
    for mod in args.modules:
        try:
            if importlib.util.find_spec(mod) is None:
                continue
        except (ImportError, ValueError):
            continue
        log(f"startup: import {mod}")
        t = run_times([py, "-c", f"import {mod}"], args.runs)
        if t:
            imp_rows.append(stat_row(f"import {mod}", t, base_min))
    imp_rows.sort(key=lambda r: -r["delta_ms"])
    sections.append({"title": "Import-Zeiten", "columns": cols + [delta_col], "rows": imp_rows,
                     "share": "delta_ms",
                     "note": "Δ = min minus min des leeren Interpreters (rauschärmer als Median); "
                             "Balken = Anteil am teuersten Import."})
    heavy = imp_rows[:3]
    if heavy:
        insights.append("Teuerste Imports: " + ", ".join(
            f"{r['name'].split()[1]} (+{r['delta_ms']:.0f} ms)" for r in heavy) + ".")
    for r in imp_rows:
        if r["name"] in ("import fastapi", "import pandas"):
            headlines.append(hl(f"startup.{r['name'].replace(' ', '_')}", r["name"],
                                r["delta_ms"], "ms", False))

    # 3. FastAPI / uvicorn
    app_rows, fa_t, uv_ms = [], None, None
    if importlib.util.find_spec("fastapi") is not None:
        log("startup: FastAPI App")
        fa_t = run_times([py, "-c", FASTAPI_SNIPPET], args.runs)
        if fa_t:
            app_rows.append(stat_row("python: import fastapi + App + 1 Route", fa_t))
        if importlib.util.find_spec("uvicorn") is not None:
            log("startup: uvicorn Kaltstart")
            with tempfile.TemporaryDirectory() as tmp:
                Path(tmp, "app_mod.py").write_text(APP_SOURCE)
                ut = [t for t in (uvicorn_startup(tmp) for _ in range(max(3, args.runs // 2))) if t]
            if ut:
                app_rows.append(stat_row("uvicorn: Prozessstart → erste 200-Antwort", ut))
                uv_ms = statistics.median(ut)
                headlines.append(hl("startup.uvicorn_cold", "uvicorn Kaltstart", uv_ms, "ms", False))
    if app_rows:
        sections.append({"title": "FastAPI & uvicorn", "columns": cols, "rows": app_rows})
    if uv_ms and fa_t:
        fa_med = statistics.median(fa_t)
        rest = uv_ms - fa_med
        if rest > 10:
            insights.append(f"uvicorn-Kaltstart {uv_ms:.0f} ms = FastAPI-Import inkl. Interpreter ≈ {fa_med:.0f} ms "
                            f"+ Server-Init/Listen/erster Request ≈ {rest:.0f} ms.")
        else:
            insights.append(f"uvicorn-Kaltstart ({uv_ms:.0f} ms) wird fast komplett vom FastAPI-Import "
                            f"({fa_med:.0f} ms) bestimmt; Server-Init ist vernachlässigbar (<10 ms).")

    emit({
        "benchmark": "startup", "title": "Startzeiten",
        "meta": {"runs": args.runs, "python_exe": os.path.basename(py)},
        "sections": sections, "headlines": headlines, "insights": insights,
    }, args.json)


if __name__ == "__main__":
    main()

