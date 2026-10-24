#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["pandas"]
# ///
"""
CSV-Benchmark: Generierung, Schreiben, Lesen, Verarbeitung (stdlib vs. pandas).

  uv run bench_csv.py
  uv run bench_csv.py --rows 1000000 --repeat 5
  uv run bench_csv.py --no-pandas --json out.json
"""
import argparse
import csv
import io
import json
import os
import platform
import random
import statistics
import sys
import tempfile
import time
from collections import defaultdict
from pathlib import Path

CATEGORIES = ["books", "games", "food", "tools", "toys", "music", "garden", "sport"]
FIELDS = ["id", "category", "name", "price", "quantity", "active", "created"]


def generate_rows(n, seed=42):
    rnd = random.Random(seed)
    return [
        [i, rnd.choice(CATEGORIES), f"product-{rnd.randrange(10**6)}",
         round(rnd.uniform(1, 500), 2), rnd.randrange(1, 100),
         rnd.choice(("true", "false")), f"2024-{rnd.randrange(1, 13):02d}-{rnd.randrange(1, 29):02d}"]
        for i in range(n)
    ]


# ----------------------------------------------------------------- Operationen
def write_writer(path, rows):
    with open(path, "w", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        w.writerow(FIELDS)
        w.writerows(rows)


def write_dictwriter(path, rows):
    with open(path, "w", newline="", encoding="utf-8") as f:
        w = csv.DictWriter(f, fieldnames=FIELDS)
        w.writeheader()
        for r in rows:
            w.writerow(dict(zip(FIELDS, r)))


def write_manual(path, rows):
    with open(path, "w", encoding="utf-8") as f:
        f.write(",".join(FIELDS) + "\n")
        f.writelines(",".join(map(str, r)) + "\n" for r in rows)


def write_stringio(rows):
    buf = io.StringIO()
    csv.writer(buf).writerows(rows)
    return buf.tell()


def read_reader(path):
    with open(path, newline="", encoding="utf-8") as f:
        r = csv.reader(f)
        next(r)
        return sum(1 for _ in r)


def read_dictreader(path):
    with open(path, newline="", encoding="utf-8") as f:
        return sum(1 for _ in csv.DictReader(f))


def read_split(path):
    with open(path, encoding="utf-8") as f:
        next(f)
        return sum(1 for line in f if line.rstrip("\n").split(","))


def process_aggregate(path):
    """Gruppieren nach Kategorie: Umsatz & Ø-Preis, nur aktive Zeilen."""
    revenue = defaultdict(float)
    count = defaultdict(int)
    price_sum = defaultdict(float)
    with open(path, newline="", encoding="utf-8") as f:
        for row in csv.DictReader(f):
            if row["active"] != "true":
                continue
            p, q = float(row["price"]), int(row["quantity"])
            c = row["category"]
            revenue[c] += p * q
            price_sum[c] += p
            count[c] += 1
    return {c: (revenue[c], price_sum[c] / count[c]) for c in revenue}


def process_transform(src, dst):
    """Lesen -> transformieren -> schreiben (Streaming)."""
    with open(src, newline="", encoding="utf-8") as fin, \
            open(dst, "w", newline="", encoding="utf-8") as fout:
        w = csv.writer(fout)
        w.writerow(["id", "category", "total", "name_upper"])
        for row in csv.DictReader(fin):
            w.writerow([row["id"], row["category"],
                        round(float(row["price"]) * int(row["quantity"]), 2),
                        row["name"].upper()])


def bench(fn, repeat):
    times = []
    for _ in range(repeat):
        t = time.perf_counter()
        fn()
        times.append(time.perf_counter() - t)
    return min(times), statistics.median(times)


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


# --------------------------------------------------------------------------- main
def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--rows", type=int, default=500_000)
    ap.add_argument("--repeat", type=int, default=3)
    ap.add_argument("--no-pandas", action="store_true", help="pandas-Varianten überspringen")
    ap.add_argument("--json", help="Ergebnis als JSON in diese Datei schreiben")
    args = ap.parse_args()

    pd = None
    if not args.no_pandas:
        import pandas as pd  # noqa: F811

    n = args.rows
    groups = {"In-Memory": [], "Schreiben (Datei)": [], "Lesen": [],
              "Aggregation (Gruppieren nach Kategorie)": [], "Transformation (Lesen → Schreiben)": []}
    by = {}

    with tempfile.TemporaryDirectory() as tmp:
        src = os.path.join(tmp, "data.csv")
        dst = os.path.join(tmp, "out.csv")

        log("csv: generiere Testdaten")
        rows = generate_rows(n)
        write_writer(src, rows)
        size_mb = os.path.getsize(src) / 1e6

        def run(group, key, name, fn, mb=True):
            log(f"csv: {name}")
            best, med = bench(fn, args.repeat)
            row = {"name": name, "best_s": best, "median_s": med, "rows_s": n / best,
                   "mb_s": size_mb / best if mb else None}
            groups[group].append(row)
            by[key] = row

        run("In-Memory", "generate", "Zeilen generieren", lambda: generate_rows(n), mb=False)
        run("In-Memory", "stringio", "csv.writer -> StringIO", lambda: write_stringio(rows))

        run("Schreiben (Datei)", "write_writer", "csv.writer", lambda: write_writer(dst, rows))
        run("Schreiben (Datei)", "write_dict", "csv.DictWriter", lambda: write_dictwriter(dst, rows))
        run("Schreiben (Datei)", "write_manual", "manuell (join)", lambda: write_manual(dst, rows))
        if pd:
            run("Schreiben (Datei)", "write_pandas", "pandas.to_csv",
                lambda: pd.DataFrame(rows, columns=FIELDS).to_csv(dst, index=False))

        run("Lesen", "read_reader", "csv.reader", lambda: read_reader(src))
        run("Lesen", "read_dict", "csv.DictReader", lambda: read_dictreader(src))
        run("Lesen", "read_split", "str.split", lambda: read_split(src))
        if pd:
            run("Lesen", "read_pandas", "pandas.read_csv", lambda: len(pd.read_csv(src)))

        run("Aggregation (Gruppieren nach Kategorie)", "agg", "Aggregation (DictReader)", lambda: process_aggregate(src))
        if pd:
            def pandas_groupby():
                df = pd.read_csv(src)
                df = df[df["active"]].assign(rev=lambda d: d["price"] * d["quantity"])
                return df.groupby("category").agg(rev=("rev", "sum"), avg=("price", "mean"))
            run("Aggregation (Gruppieren nach Kategorie)", "agg_pandas", "Aggregation (pandas groupby)", pandas_groupby)
        run("Transformation (Lesen → Schreiben)", "transform", "DictReader → Transform → csv.writer",
            lambda: process_transform(src, dst))

    columns = [
        {"key": "name", "label": "Test"},
        {"key": "best_s", "label": "best", "unit": "s", "fmt": ".3f"},
        {"key": "median_s", "label": "median", "unit": "s", "fmt": ".3f"},
        {"key": "rows_s", "label": "Zeilen/s", "fmt": ",.0f"},
        {"key": "mb_s", "label": "Durchsatz", "unit": "MB/s", "fmt": ".1f"},
    ]
    compare = {"Schreiben (Datei)", "Lesen", "Aggregation (Gruppieren nach Kategorie)"}
    notes = {"Lesen": "str.split parst keine Quotes/Escapes – nur Untergrenze als Referenz.",
             "In-Memory": "Unabhängige Messungen, kein Vergleich untereinander."}
    sections = [{"title": g, "columns": columns, "rows": r,
                 **({"primary": "best_s", "lower_is_better": True} if g in compare else {}),
                 **({"note": notes[g]} if g in notes else {})}
                for g, r in groups.items() if r]

    insights = []
    if "read_reader" in by and "read_dict" in by:
        insights.append(f"DictReader ist {by['read_dict']['best_s'] / by['read_reader']['best_s']:.2f}× "
                        f"langsamer als csv.reader (Preis für Spaltennamen-Zugriff).")
    if "write_writer" in by and "write_dict" in by:
        insights.append(f"DictWriter ist {by['write_dict']['best_s'] / by['write_writer']['best_s']:.2f}× "
                        f"langsamer als csv.writer.")
    if "read_pandas" in by:
        r = by["read_reader"]["best_s"] / by["read_pandas"]["best_s"]
        if 0.9 <= r <= 1.1:
            insights.append("pandas.read_csv und csv.reader liegen beim reinen Einlesen gleichauf.")
        elif r > 1.1:
            insights.append(f"pandas.read_csv liest {r:.1f}× schneller als csv.reader (C-Parser).")
        else:
            insights.append(f"pandas.read_csv ist {1 / r:.1f}× langsamer als csv.reader (baut DataFrame auf).")
    if "agg_pandas" in by:
        r = by["agg"]["best_s"] / by["agg_pandas"]["best_s"]
        insights.append(f"Aggregation: pandas ist {r:.1f}× schneller als reines Python."
                        if r >= 1 else f"Aggregation: reines Python ist {1 / r:.1f}× schneller als pandas.")

    headlines = [
        hl("csv.generate", "Zeilen generieren", by["generate"]["rows_s"], "Zeilen/s", True),
        hl("csv.write", "csv.writer schreiben", by["write_writer"]["rows_s"], "Zeilen/s", True),
        hl("csv.read", "csv.reader lesen", by["read_reader"]["rows_s"], "Zeilen/s", True),
        hl("csv.aggregate", "Aggregation (DictReader)", by["agg"]["rows_s"], "Zeilen/s", True),
    ]
    if "read_pandas" in by:
        headlines.append(hl("csv.read_pandas", "pandas.read_csv", by["read_pandas"]["rows_s"], "Zeilen/s", True))

    emit({
        "benchmark": "csv", "title": "CSV generieren / lesen / schreiben / verarbeiten",
        "meta": {"rows": n, "repeat": args.repeat, "dateigroesse_mb": round(size_mb, 1),
                 "pandas": pd.__version__ if pd else "aus"},
        "sections": sections, "headlines": headlines, "insights": insights,
    }, args.json)


if __name__ == "__main__":
    main()

