#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# ///
"""
PyX vs CPython: dieselben reinen Python-Workloads unter drei Runnern.

Runner: CPython (sys.executable) · PyX JIT (`pyx run`) · PyX AOT (`pyx build` + Binary).
Jeder Workload läuft als frischer Prozess. Ein Warmup-Lauf je Runner validiert
die Ausgabe gegen CPython, danach werden `--repeat` Läufe gemessen (Median).
Abweichende oder fehlschlagende Läufe werden als Fehlerzeile markiert und
führen zu Exit-Code 1.

  uv run bench_pyx.py
  uv run bench_pyx.py --n 50000 --repeat 2
  uv run bench_pyx.py --pyx ../target/release/pyx --json out.json

Das PyX-Binary wird über --pyx, $PYX_BIN, target/release/pyx oder
target/debug/pyx (relativ zum Repo-Root) gesucht.
"""
import argparse
import json
import os
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent  # Repo-Root (Elternverzeichnis von benchmarks/)
TIMEOUT = 600.0


# --------------------------------------------------------------------------- Workloads
# Platzhalter @N@ wird je Workload durch die skalierte Größe ersetzt.
PROGRAMS = [
    ("hello", """\
print("hello")
"""),
    ("while_sum", """\
n = @N@
total = 0
i = 0
while i < n:
    total += i
    i += 1
print(total)
"""),
    ("for_range_sum", """\
n = @N@
total = 0
for i in range(n):
    total += i
print(total)
"""),
    ("list_append", """\
n = @N@
out = []
i = 0
while i < n:
    out.append(i * 2)
    i += 1
print(len(out), out[0] + out[-1])
"""),
    ("list_comp", """\
n = @N@
out = [i * 2 for i in range(n)]
print(len(out), out[0] + out[-1])
"""),
    ("dict_insert", """\
n = @N@
d = {}
i = 0
while i < n:
    d[i] = i * i
    i += 1
print(len(d), d[n - 1])
"""),
    ("string_concat", """\
n = @N@
s = ""
i = 0
while i < n:
    s += "x"
    i += 1
print(len(s))
"""),
    ("json_roundtrip", """\
import json

data = [{"id": i, "v": i * 2} for i in range(1000)]
text = json.dumps(data)
back = json.loads(text)
print(len(back), back[999]["v"])
"""),
    ("math_loop", """\
import math

total = 0
for i in range(@N@):
    total += math.floor(math.sqrt(i))
print(total)
"""),
    ("generator_sum", """\
def gen(n):
    i = 0
    while i < n:
        yield i
        i += 1


print(sum(gen(@N@)))
"""),
    ("async_gather", """\
import asyncio


async def work(delay):
    await asyncio.sleep(delay)
    return 1


async def main():
    results = await asyncio.gather(work(0.02), work(0.02), work(0.02), work(0.02))
    print(results[0], results[1], results[2], results[3])


asyncio.run(main())
"""),
]

FIXED_SIZE = {"hello", "json_roundtrip", "async_gather"}


def workload_size(key, n):
    """Skaliert Workloads mit n; feste Größen behalten ihre eigene Größe."""
    if key in FIXED_SIZE:
        return None
    if key in ("list_append", "list_comp", "generator_sum"):
        return max(1, n // 2)
    if key in ("dict_insert", "string_concat"):
        return max(1, n // 40)
    return max(1, n)


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


# --------------------------------------------------------------------------- Prozesse
def exec_once(cmd):
    """Führt ein Kommando aus und liefert (exit, stdout, stderr, dauer_ms)."""
    t0 = time.perf_counter()
    try:
        p = subprocess.run(cmd, cwd=str(ROOT), capture_output=True, text=True, timeout=TIMEOUT)
        rc, out, err = p.returncode, p.stdout, p.stderr
    except Exception as e:  # noqa: BLE001 - jeder Startfehler ist ein Messfehler
        rc, out, err = -1, "", str(e)
    return rc, out, err, (time.perf_counter() - t0) * 1000.0


def last_line(text):
    lines = [line for line in (text or "").strip().splitlines() if line.strip()]
    return lines[-1].strip() if lines else ""


def measure_runner(label, cmd, expected, repeat):
    """Warmup + `repeat` Messungen; -> (median_ms | None, fehler | None)."""
    rc, out, err, _ = exec_once(cmd)
    if rc != 0:
        return None, f"{label}: Lauf fehlgeschlagen (Exit {rc}): {last_line(err)}"
    if out.strip() != expected:
        return None, (f"{label}: Ausgabe weicht von CPython ab "
                      f"(erwartet {expected!r}, erhalten {out.strip()!r})")
    times = []
    for _ in range(repeat):
        rc, out, err, ms = exec_once(cmd)
        if rc != 0:
            return None, f"{label}: Lauf fehlgeschlagen (Exit {rc}): {last_line(err)}"
        if out.strip() != expected:
            return None, (f"{label}: Ausgabe weicht von CPython ab "
                          f"(erwartet {expected!r}, erhalten {out.strip()!r})")
        times.append(ms)
    return statistics.median(times), None


def measure_startup(repeat):
    cmd = [sys.executable, "-c", "pass"]
    _, _, _, _ = exec_once(cmd)  # Warmup
    times = []
    for _ in range(repeat):
        rc, _, err, ms = exec_once(cmd)
        if rc != 0:
            log(f"FEHLER python -c pass: Exit {rc}: {last_line(err)}")
            return None
        times.append(ms)
    return statistics.median(times)


def find_pyx(cli_value):
    candidates = []
    if cli_value:
        candidates.append(Path(cli_value).expanduser())
    env = os.environ.get("PYX_BIN")
    if env:
        candidates.append(Path(env).expanduser())
    candidates.append(ROOT / "target" / "release" / "pyx")
    candidates.append(ROOT / "target" / "debug" / "pyx")
    for c in candidates:
        if c.is_file() and os.access(c, os.X_OK):
            return c.resolve()
    return None


# --------------------------------------------------------------------------- Messung
def run_workloads(pyx, n, repeat, workdir):
    rows, errors = [], []
    build_total = 0.0
    built = 0

    for key, template in PROGRAMS:
        size = workload_size(key, n)
        source = workdir / f"{key}.py"
        source.write_text(template.replace("@N@", str(size) if size is not None else "0"),
                          encoding="utf-8")
        log(f"pyx: {key}" + (f" (n={size})" if size is not None else " (fixe Größe)"))

        row = {"name": key, "cpython_ms": None, "jit_ms": None, "aot_ms": None}
        problems = []

        rc, out, err, _ = exec_once([sys.executable, str(source)])
        if rc != 0:
            problems.append(f"CPython-Referenz fehlgeschlagen (Exit {rc}): {last_line(err)}")
            expected = None
        else:
            expected = out.strip()

        if expected is not None:
            ms, problem = measure_runner("CPython", [sys.executable, str(source)], expected, repeat)
            row["cpython_ms"] = ms
            if problem:
                problems.append(problem)

            ms, problem = measure_runner("PyX JIT", [str(pyx), "run", str(source)], expected, repeat)
            row["jit_ms"] = ms
            if problem:
                problems.append(problem)

            binary = workdir / f"{key}.bin"
            rc, out, err, build_ms = exec_once(
                [str(pyx), "build", str(source), "-o", str(binary)])
            if rc != 0 or not binary.is_file():
                detail = last_line(err) or last_line(out) or "kein Binary erzeugt"
                problems.append(f"AOT-Build fehlgeschlagen (Exit {rc}): {detail}")
            else:
                build_total += build_ms
                built += 1
                ms, problem = measure_runner("PyX AOT", [str(binary)], expected, repeat)
                row["aot_ms"] = ms
                if problem:
                    problems.append(problem)

        if row["cpython_ms"] and row["jit_ms"]:
            row["jit_speedup"] = row["cpython_ms"] / row["jit_ms"]
        if row["cpython_ms"] and row["aot_ms"]:
            row["aot_speedup"] = row["cpython_ms"] / row["aot_ms"]

        if problems:
            row["error"] = " | ".join(problems)
            for problem in problems:
                log(f"FEHLER {key}: {problem}")
            errors.append(f"{key}: {problems[0]}")

        rows.append(row)

    return rows, errors, build_total, built


def build_insights(rows, errors):
    out = []
    jit = [(r["name"], r["cpython_ms"] / r["jit_ms"]) for r in rows
           if r.get("cpython_ms") and r.get("jit_ms")]
    aot = [(r["name"], r["cpython_ms"] / r["aot_ms"]) for r in rows
           if r.get("cpython_ms") and r.get("aot_ms")]

    if jit:
        faster = sum(1 for _, s in jit if s > 1)
        out.append(f"PyX JIT (`pyx run`): Median {statistics.median([s for _, s in jit]):.2f}× "
                   f"(CPython/JIT), schneller bei {faster}/{len(jit)} Workloads.")
        best = max(jit, key=lambda x: x[1])
        worst = min(jit, key=lambda x: x[1])
        out.append(f"JIT größter Gewinn: {best[0]} ({best[1]:.2f}×), geringster: "
                   f"{worst[0]} ({worst[1]:.2f}×).")
    if aot:
        faster = sum(1 for _, s in aot if s > 1)
        out.append(f"PyX AOT: Median {statistics.median([s for _, s in aot]):.2f}× "
                   f"(CPython/AOT), schneller bei {faster}/{len(aot)} Workloads.")
        best = max(aot, key=lambda x: x[1])
        worst = min(aot, key=lambda x: x[1])
        out.append(f"AOT größter Gewinn: {best[0]} ({best[1]:.2f}×), geringster: "
                   f"{worst[0]} ({worst[1]:.2f}×).")

    row = next((r for r in rows if r["name"] == "async_gather"), None)
    if row and row.get("cpython_ms"):
        parts = [f"CPython {row['cpython_ms']:.1f} ms"]
        if row.get("jit_ms"):
            parts.append(f"JIT {row['jit_ms']:.1f} ms")
        if row.get("aot_ms"):
            parts.append(f"AOT {row['aot_ms']:.1f} ms")
        out.append(f"async_gather: 4 Coroutinen mit je `asyncio.sleep(0.02)` – "
                   + ", ".join(parts)
                   + " (jeweils inkl. Prozessstart). Die Wanduhr entspricht ≈ einem Sleep, "
                     "nicht den ~80 ms einer sequenziellen Ausführung: die Coroutinen "
                     "laufen nebenläufig über den Event-Loop.")

    if errors:
        out.append(f"{len(errors)} Workload(s) mit Fehlern – Zeiten sind dort nicht belastbar.")
        out.extend(f"Fehler: {e}" for e in errors)
    return out


# --------------------------------------------------------------------------- main
def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--n", type=int, default=200_000, help="Basisgröße der Workloads")
    ap.add_argument("--repeat", type=int, default=5, help="gemessene Läufe je Runner/Workload")
    ap.add_argument("--pyx", help="Pfad zum PyX-Binary (sonst $PYX_BIN oder target/{release,debug}/pyx)")
    ap.add_argument("--json", help="Ergebnis als JSON in diese Datei schreiben")
    args = ap.parse_args()

    repeat = max(1, args.repeat)
    if args.pyx:
        explicit = Path(args.pyx).expanduser()
        if not (explicit.is_file() and os.access(explicit, os.X_OK)):
            sys.exit(f"--pyx: '{args.pyx}' ist keine ausführbare Datei")
    pyx = find_pyx(args.pyx)
    if pyx is None:
        sys.exit("pyx-Binary nicht gefunden: --pyx/PYX_BIN prüfen oder bauen mit "
                 "`cargo build --release -p pyx-cli -p pyx-runtime` "
                 f"(gesucht: {ROOT / 'target' / 'release' / 'pyx'}, {ROOT / 'target' / 'debug' / 'pyx'})")

    log(f"pyx: {pyx}")
    startup_ms = measure_startup(repeat)

    with tempfile.TemporaryDirectory(prefix="pyx-bench-") as tmp:
        workdir = Path(tmp)
        rows, errors, build_total, built = run_workloads(pyx, args.n, repeat, workdir)

    by_name = {r["name"]: r for r in rows}
    hello = by_name.get("hello", {})

    start_rows = []
    if startup_ms is not None:
        start_rows.append({"name": "python -c pass", "ms": startup_ms})
    start_rows.append({"name": "pyx JIT hello", "ms": hello.get("jit_ms")})
    start_rows.append({"name": "pyx AOT hello", "ms": hello.get("aot_ms")})
    start_rows.append({"name": "AOT build (gesamt)", "ms": build_total if built else None})

    sections = [
        {
            "title": "Wall-Time je Workload",
            "note": "Median aus --repeat Läufen, frischer Prozess je Lauf; AOT ohne Build-Zeit. "
                    "Speedups > 1× bedeuten schneller als CPython.",
            "columns": [
                {"key": "name", "label": "Workload"},
                {"key": "cpython_ms", "label": "CPython", "unit": "ms", "fmt": ".2f"},
                {"key": "jit_ms", "label": "PyX JIT", "unit": "ms", "fmt": ".2f"},
                {"key": "aot_ms", "label": "PyX AOT", "unit": "ms", "fmt": ".2f"},
                {"key": "jit_speedup", "label": "CPython/JIT", "unit": "×", "fmt": ".2f"},
                {"key": "aot_speedup", "label": "CPython/AOT", "unit": "×", "fmt": ".2f"},
            ],
            "rows": rows,
        },
        {
            "title": "Start & Build",
            "note": "Prozessstart bzw. Build-Kosten; hello enthält den vollen Runner-Start.",
            "columns": [
                {"key": "name", "label": "Messung"},
                {"key": "ms", "label": "Median", "unit": "ms", "fmt": ".2f"},
            ],
            "rows": start_rows,
        },
    ]

    headlines = []
    for r in rows:
        for field, suffix in (("cpython_ms", "cpython_ms"), ("jit_ms", "jit_ms"),
                              ("aot_ms", "aot_ms")):
            value = r.get(field)
            if value is not None:
                runner = {"cpython_ms": "CPython", "jit_ms": "PyX JIT", "aot_ms": "PyX AOT"}[field]
                headlines.append(hl(f"pyx.{r['name']}.{suffix}", f"{r['name']} · {runner}",
                                    value, "ms", False))
    if hello.get("jit_ms") is not None:
        headlines.append(hl("pyx.startup.jit_ms", "Start · PyX JIT (hello)",
                            hello["jit_ms"], "ms", False))
    if hello.get("aot_ms") is not None:
        headlines.append(hl("pyx.startup.aot_ms", "Start · PyX AOT (hello)",
                            hello["aot_ms"], "ms", False))

    result = {
        "benchmark": "pyx",
        "title": "PyX vs CPython (JIT/AOT)",
        "meta": {"n": args.n, "repeat": repeat, "pyx": str(pyx),
                 "aot_build_ms": round(build_total, 1)},
        "sections": sections,
        "headlines": headlines,
        "insights": build_insights(rows, errors),
    }
    emit(result, args.json)
    sys.exit(1 if errors else 0)


if __name__ == "__main__":
    main()
