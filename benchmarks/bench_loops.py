#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["numpy"]
# ///
"""
Loop-Benchmark: verschiedene Schleifenvarianten im Vergleich.

Gruppen: sum, list, filter, dict_str, iterate, nested_calls

  uv run bench_loops.py
  uv run bench_loops.py --n 2000000 --repeat 7 --group sum
  uv run bench_loops.py --json out.json
"""
import argparse
import json
import platform
import statistics
import sys
import time
from functools import reduce
from operator import add
from pathlib import Path

import numpy as np


# --------------------------------------------------------------------------- Summieren
def sum_for(n):
    t = 0
    for i in range(n):
        t += i
    return t


def sum_while(n):
    t = i = 0
    while i < n:
        t += i
        i += 1
    return t


def sum_builtin(n):
    return sum(range(n))


def sum_genexp(n):
    return sum(i for i in range(n))


def sum_map(n):
    return sum(map(int, range(n)))


def sum_reduce(n):
    return reduce(add, range(n))


def sum_numpy(n):
    return int(np.arange(n, dtype=np.int64).sum())


# --------------------------------------------------------------------------- Liste bauen
def list_append(n):
    out = []
    for i in range(n):
        out.append(i * 2)
    return out


def list_append_bound(n):
    out = []
    ap = out.append
    for i in range(n):
        ap(i * 2)
    return out


def list_comp(n):
    return [i * 2 for i in range(n)]


def list_map_lambda(n):
    return list(map(lambda i: i * 2, range(n)))


def list_numpy(n):
    return (np.arange(n) * 2).tolist()


# --------------------------------------------------------------------------- Filtern
def filter_for(n):
    out = []
    for i in range(n):
        if i % 3 == 0:
            out.append(i)
    return out


def filter_comp(n):
    return [i for i in range(n) if i % 3 == 0]


def filter_builtin(n):
    return list(filter(lambda i: i % 3 == 0, range(n)))


def filter_slice(n):
    return list(range(0, n, 3))


# --------------------------------------------------------------------------- Dict / String
def dict_for(n):
    d = {}
    for i in range(n):
        d[i] = i * i
    return d


def dict_comp(n):
    return {i: i * i for i in range(n)}


def dict_zip(n):
    return dict(zip(range(n), (i * i for i in range(n))))


def str_concat(n):
    s = ""
    for i in range(n // 10):
        s += str(i)
    return s


def str_join(n):
    return "".join([str(i) for i in range(n // 10)])


def str_join_gen(n):
    return "".join(str(i) for i in range(n // 10))


for _f in (str_concat, str_join, str_join_gen):
    _f.div = 10  # diese Varianten iterieren nur n/10 mal


# --------------------------------------------------------------------------- Iteration (inkl. Listenaufbau)
def iter_index(n):
    data = list(range(n))
    t = 0
    for i in range(len(data)):
        t += data[i]
    return t


def iter_direct(n):
    data = list(range(n))
    t = 0
    for x in data:
        t += x
    return t


def iter_enumerate(n):
    data = list(range(n))
    t = 0
    for i, x in enumerate(data):
        t += x
    return t


def iter_zip(n):
    a = list(range(n))
    b = list(range(n))
    t = 0
    for x, y in zip(a, b):
        t += x * y
    return t


# --------------------------------------------------------------------------- Verschachtelt / Funktionen
def nested_loops(n):
    side = int(n ** 0.5)
    t = 0
    for i in range(side):
        for j in range(side):
            t += i * j
    return t


def nested_comp(n):
    side = int(n ** 0.5)
    return sum(i * j for i in range(side) for j in range(side))


def _add(a, b):
    return a + b


def call_function(n):
    t = 0
    for i in range(n):
        t = _add(t, i)
    return t


def call_inline(n):
    t = 0
    for i in range(n):
        t = t + i
    return t


def call_lambda(n):
    f = lambda a, b: a + b
    t = 0
    for i in range(n):
        t = f(t, i)
    return t


GROUPS = {
    "sum": ("Summieren", [sum_for, sum_while, sum_builtin, sum_genexp, sum_map, sum_reduce, sum_numpy]),
    "list": ("Liste bauen", [list_append, list_append_bound, list_comp, list_map_lambda, list_numpy]),
    "filter": ("Filtern (jedes 3. Element)", [filter_for, filter_comp, filter_builtin, filter_slice]),
    "dict_str": ("Dict & String bauen", [dict_for, dict_comp, dict_zip, str_concat, str_join, str_join_gen]),
    "iterate": ("Iteration über Liste (inkl. Listenaufbau)",
                [iter_index, iter_direct, iter_enumerate, iter_zip]),
    "nested_calls": ("Verschachtelt & Funktionsaufrufe",
                     [nested_loops, nested_comp, call_inline, call_function, call_lambda]),
}


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


# --------------------------------------------------------------------------- Benchmark
def measure(fn, n, repeat):
    fn(min(n, 1000))  # Warmup
    times = []
    for _ in range(repeat):
        t = time.perf_counter()
        fn(n)
        times.append(time.perf_counter() - t)
    return min(times), statistics.median(times)


def build_insights(by, n):
    out = []

    def ratio(slow, fast):
        if slow in by and fast in by:
            return by[slow]["best_ms"] / by[fast]["best_ms"]

    if (r := ratio("list_append", "list_comp")):
        out.append(f"List-Comprehension ist {r:.2f}× schneller als append() in einer for-Schleife.")
    if (r := ratio("sum_for", "sum_builtin")):
        out.append(f"sum(range(n)) ist {r:.1f}× schneller als die manuelle for-Schleife.")
    if (r := ratio("sum_builtin", "sum_numpy")):
        out.append(f"numpy summiert {r:.0f}× schneller als builtin sum() (reine C-Schleife).")
    if (r := ratio("str_concat", "str_join")):
        out.append(f"''.join() ist {r:.2f}× schneller als String-Konkatenation mit += .")
    if (r := ratio("iter_index", "iter_direct")):
        out.append(f"Index-Zugriff (data[i]) ist {r:.2f}× langsamer als direkte Iteration.")
    if "call_function" in by and "call_inline" in by:
        extra = (by["call_function"]["best_ms"] - by["call_inline"]["best_ms"]) * 1e6 / n
        out.append(f"Ein Python-Funktionsaufruf kostet ≈ {extra:.0f} ns zusätzlich pro Iteration.")
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--n", type=int, default=1_000_000)
    ap.add_argument("--repeat", type=int, default=5)
    ap.add_argument("--group", choices=list(GROUPS), help="nur eine Gruppe")
    ap.add_argument("--json", help="Ergebnis als JSON in diese Datei schreiben")
    args = ap.parse_args()

    sections, by = [], {}
    for gkey, (gtitle, funcs) in GROUPS.items():
        if args.group and gkey != args.group:
            continue
        log(f"loops: {gtitle}")
        rows = []
        for f in funcs:
            best, med = measure(f, args.n, args.repeat)
            iters = args.n / getattr(f, "div", 1)
            row = {"name": f.__name__, "best_ms": best * 1000, "median_ms": med * 1000,
                   "ns_iter": best / iters * 1e9}
            rows.append(row)
            by[f.__name__] = row
        rows.sort(key=lambda r: r["best_ms"])
        sections.append({
            "title": gtitle,
            "columns": [
                {"key": "name", "label": "Variante"},
                {"key": "best_ms", "label": "best", "unit": "ms", "fmt": ".1f"},
                {"key": "median_ms", "label": "median", "unit": "ms", "fmt": ".1f"},
                {"key": "ns_iter", "label": "pro Iteration", "unit": "ns", "fmt": ".1f"},
            ],
            "rows": rows, "primary": "best_ms", "lower_is_better": True,
        })

    headlines = []
    for name, label in [("sum_for", "for-Schleife: Summe"), ("sum_builtin", "sum(range)"),
                        ("list_comp", "List-Comprehension"), ("call_function", "Schleife mit Funktionsaufruf")]:
        if name in by:
            headlines.append(hl(f"loops.{name}", label, by[name]["ns_iter"], "ns/Iter", False))

    emit({
        "benchmark": "loops", "title": "For-Schleifen",
        "meta": {"n": args.n, "repeat": args.repeat, "numpy": np.__version__},
        "sections": sections, "headlines": headlines,
        "insights": build_insights(by, args.n),
    }, args.json)


if __name__ == "__main__":
    main()

