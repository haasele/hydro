#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["rich"]
# ///
"""
Gesammelter Benchmark-Entrypoint: startet alle bench_*.py nacheinander (jeweils via
`uv run --script`, jedes Skript mit eigener Umgebung) und erzeugt einen Report.

  uv run bench_all.py                    # voller Lauf (~5 Min.)
  uv run bench_all.py --quick            # Schnelllauf (~1 Min.), nur für Smoke-Tests
  uv run bench_all.py --only loops csv   # nur bestimmte Benchmarks
  uv run bench_all.py --skip fastapi
  uv run bench_all.py --baseline bench_results/latest.json

Ausgabe in bench_results/<Zeitstempel>/:
  report.md  report.html  report.txt  results.json  raw/*.json  logs/*.log
Außerdem: bench_results/latest.json (dient als Baseline für den nächsten Lauf).
"""
import argparse
import json
import os
import platform
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

from rich import box
from rich.console import Console
from rich.markup import escape
from rich.panel import Panel
from rich.rule import Rule
from rich.table import Table

HERE = Path(__file__).resolve().parent
console = Console(record=True, width=max(shutil.get_terminal_size((120, 40)).columns, 130))


@dataclass
class Bench:
    key: str
    script: str
    title: str
    quick: list


BENCHES = [
    Bench("loops", "bench_loops.py", "For-Schleifen", ["--n", "200000", "--repeat", "3"]),
    Bench("csv", "bench_csv.py", "CSV", ["--rows", "100000", "--repeat", "2"]),
    Bench("startup", "bench_startup.py", "Startzeiten", ["--runs", "3"]),
    Bench("request", "bench_request_handling.py", "Request-Handling",
          ["--duration", "2", "--warmup", "0.5", "--repeat", "3"]),
    Bench("fastapi", "bench_fastapi_throughput.py", "FastAPI-Durchsatz",
          ["--duration", "2", "--warmup", "0.5"]),
    Bench("pyx", "bench_pyx.py", "PyX vs CPython (JIT/AOT)",
          ["--n", "50000", "--repeat", "2"]),
]


# --------------------------------------------------------------------------- System
def cpu_model():
    try:
        if sys.platform.startswith("linux"):
            for line in open("/proc/cpuinfo"):
                if line.lower().startswith("model name"):
                    return line.split(":", 1)[1].strip()
        if sys.platform == "darwin":
            return subprocess.check_output(["sysctl", "-n", "machdep.cpu.brand_string"], text=True).strip()
    except Exception:
        pass
    return platform.processor() or platform.machine()


def mem_gb():
    try:
        return os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES") / 1e9
    except (ValueError, OSError, AttributeError):
        return None


def system_info():
    try:
        uv_v = subprocess.check_output(["uv", "--version"], text=True).strip()
    except Exception:
        uv_v = "?"
    info = {
        "OS": platform.platform(),
        "CPU": cpu_model(),
        "Kerne": os.cpu_count(),
        "RAM": f"{mem_gb():.1f} GB" if mem_gb() else "?",
        "Python (Runner)": platform.python_version(),
        "uv": uv_v,
    }
    try:
        load = os.getloadavg()[0]
        info["Last (1 min)"] = f"{load:.2f}"
        info["_load"] = load
    except (AttributeError, OSError):
        pass
    return info


# --------------------------------------------------------------------------- Formatierung
def fmt_val(v, fmt):
    if v is None:
        return "-"
    if fmt and isinstance(v, (int, float)):
        return format(v, fmt)
    return str(v)


def fmt_num(v):
    if abs(v) >= 1000:
        return f"{v:,.0f}"
    if abs(v) >= 100:
        return f"{v:.1f}"
    if abs(v) >= 1:
        return f"{v:.2f}"
    return f"{v:.3f}"


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


def section_cells(sec):
    cols = sec["columns"]
    heads = [c["label"] + (f" ({c['unit']})" if c.get("unit") else "") for c in cols]
    rows = [[fmt_val(r.get(c["key"]), c.get("fmt")) for c in cols] for r in sec["rows"]]
    return heads, rows, rel_factors(sec)


def bar(f, width=18):
    n = max(1, round(min(1.0, 1 / f) * width))
    color = "green" if f <= 1.05 else "yellow" if f <= 2 else "red"
    return f"[{color}]{'█' * n}[/{color}]"


def share_values(sec):
    """Anteil am Maximum (0..1) für Sektionen mit "share"-Key (Kosten-Verteilung statt Vergleich)."""
    key = sec.get("share")
    if not key:
        return None
    vals = [r.get(key) for r in sec["rows"]]
    nums = [v for v in vals if isinstance(v, (int, float)) and v > 0]
    if not nums:
        return None
    mx = max(nums)
    return [v / mx if isinstance(v, (int, float)) and v > 0 else None for v in vals]


def share_bar(s, width=18):
    n = max(1, round(s * width))
    color = "red" if s > 0.66 else "yellow" if s > 0.25 else "green"
    return f"[{color}]{'█' * n}[/{color}]"


def delta_info(h, base):
    """-> (text, kind) kind in good|bad|neutral|None"""
    old = base.get(h["key"]) if base else None
    if not old:
        return None, None
    pct = (h["value"] - old) / old * 100
    if abs(pct) < 3:
        return f"{pct:+.1f} %", "neutral"
    good = (pct > 0) == h["higher_is_better"]
    return f"{pct:+.1f} %", "good" if good else "bad"


def auto_insights(sec):
    facs = rel_factors(sec)
    if not facs:
        return None
    pairs = [(r, f) for r, f in zip(sec["rows"], facs) if f is not None]
    if len(pairs) < 2:
        return None
    best = min(pairs, key=lambda x: x[1])
    worst = max(pairs, key=lambda x: x[1])
    pcol = next(c for c in sec["columns"] if c["key"] == sec["primary"])
    name_key = sec["columns"][0]["key"]

    def val(r):
        return fmt_val(r[sec["primary"]], pcol.get("fmt")) + (f" {pcol['unit']}" if pcol.get("unit") else "")

    return (f"[{sec['title']}] Bester Wert: {best[0][name_key]} ({val(best[0])}); "
            f"schwächster: {worst[0][name_key]} ({val(worst[0])}) – Spanne {worst[1]:.1f}×")


# --------------------------------------------------------------------------- Lauf
def run_bench(b, quick, outdir):
    raw = outdir / "raw"
    logs = outdir / "logs"
    raw.mkdir(parents=True, exist_ok=True)
    logs.mkdir(parents=True, exist_ok=True)
    json_path = raw / f"{b.key}.json"
    cmd = ["uv", "run", "--script", str(HERE / b.script), "--json", str(json_path),
           *(b.quick if quick else [])]
    t0 = time.perf_counter()
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, cwd=HERE)
        rc, out, err = p.returncode, p.stdout, p.stderr
    except Exception as e:  # z.B. uv nicht startbar
        rc, out, err = -1, "", str(e)
    dur = time.perf_counter() - t0
    (logs / f"{b.key}.log").write_text(
        f"$ {' '.join(cmd)}\n\n--- stdout ---\n{out}\n--- stderr ---\n{err}\n")
    result, error = None, None
    if rc == 0 and json_path.exists():
        try:
            result = json.loads(json_path.read_text())
        except Exception as e:
            error = f"JSON nicht lesbar: {e}"
    else:
        error = "\n".join(err.strip().splitlines()[-6:]) or f"Exit-Code {rc}"
    return {"key": b.key, "title": b.title, "status": "ok" if result else "failed",
            "duration_s": dur, "error": error, "result": result}


# --------------------------------------------------------------------------- Rich-Report
def rich_section(sec):
    heads, rows, facs = section_cells(sec)
    t = Table(box=box.SIMPLE_HEAD, title=escape(sec["title"]), title_justify="left",
              title_style="bold cyan", caption=escape(sec["note"]) if sec.get("note") else None,
              caption_justify="left", caption_style="dim", header_style="bold", pad_edge=False)
    for i, h in enumerate(heads):
        t.add_column(escape(h), justify="left" if i == 0 else "right", no_wrap=(i == 0))
    shares = share_values(sec)
    if facs:
        t.add_column("Faktor", justify="right")
        t.add_column("", no_wrap=True)
    elif shares:
        t.add_column("Anteil", justify="right")
        t.add_column("", no_wrap=True)
    for idx, row in enumerate(rows):
        cells = [escape(c) for c in row]
        style = None
        if shares and not facs:
            s = shares[idx]
            cells += ["-", ""] if s is None else [f"{s:.0%}", share_bar(s)]
        if facs:
            f = facs[idx]
            cells += ["-", ""] if f is None else [f"{f:.2f}×", bar(f)]
            if f is not None and f <= 1.0001:
                style = "bold green"
        t.add_row(*cells, style=style)
    return t


def render_console(runs, all_headlines, base, outdir_label, meta, total_s, base_note):
    console.print()
    console.print(Rule("[bold]Zusammenfassung", style="cyan"))

    # Lauf-Status
    t = Table(box=box.SIMPLE_HEAD, header_style="bold", pad_edge=False)
    t.add_column("Benchmark")
    t.add_column("Status")
    t.add_column("Dauer", justify="right")
    for r in runs:
        status = "[green]✔ ok[/green]" if r["status"] == "ok" else "[red]✘ fehlgeschlagen[/red]"
        t.add_row(escape(r["title"]), status, f"{r['duration_s']:.1f} s")
    console.print(t)
    for r in runs:
        if r["status"] != "ok":
            console.print(Panel(escape(r["error"] or ""), title=f"Fehler: {r['title']}", border_style="red"))

    # Kennzahlen
    if all_headlines:
        t = Table(box=box.SIMPLE_HEAD, title="Kennzahlen", title_justify="left", title_style="bold cyan",
                  caption=base_note, caption_justify="left", caption_style="dim", header_style="bold",
                  pad_edge=False)
        t.add_column("Benchmark")
        t.add_column("Kennzahl")
        t.add_column("Wert", justify="right")
        t.add_column("", style="dim")
        t.add_column("Δ Baseline", justify="right")
        t.add_column("besser wenn", style="dim")
        for h in all_headlines:
            text, kind = delta_info(h, base)
            color = {"good": "green", "bad": "red", "neutral": "dim"}.get(kind, "dim")
            t.add_row(escape(h["benchmark"]), escape(h["label"]), f"[bold]{fmt_num(h['value'])}[/bold]",
                      escape(h["unit"]), f"[{color}]{text}[/{color}]" if text else "-",
                      "↑ höher" if h["higher_is_better"] else "↓ niedriger")
        console.print(t)

    # Auswertung
    console.print(Rule("[bold]Auswertung", style="cyan"))
    for r in runs:
        res = r["result"]
        if not res:
            continue
        lines = list(res.get("insights", []))
        lines += [x for x in (auto_insights(s) for s in res["sections"]) if x]
        if lines:
            console.print(f"[bold]{escape(r['title'])}[/bold]")
            for s in lines:
                console.print(f"  • {escape(s)}")

    # Details
    console.print()
    console.print(Rule("[bold]Details", style="cyan"))
    for r in runs:
        res = r["result"]
        if not res:
            continue
        meta_s = "  ".join(f"{k}={v}" for k, v in res["meta"].items())
        console.print(Panel.fit(f"[bold]{escape(r['title'])}[/bold]\n[dim]{escape(meta_s)}[/dim]",
                                border_style="blue"))
        for sec in res["sections"]:
            console.print(rich_section(sec))


# --------------------------------------------------------------------------- Markdown
def md_table(heads, rows, left=1):
    esc = lambda s: str(s).replace("|", "\\|")
    out = ["| " + " | ".join(esc(h) for h in heads) + " |",
           "| " + " | ".join(":---" if i < left else "---:" for i in range(len(heads))) + " |"]
    out += ["| " + " | ".join(esc(c) for c in r) + " |" for r in rows]
    return "\n".join(out)


def write_markdown(path, runs, all_headlines, base, info, meta, total_s, base_note):
    L = [f"# Benchmark-Report", "",
         f"_Erstellt: {meta['timestamp']} · Modus: {'quick' if meta['quick'] else 'voll'} · "
         f"Gesamtdauer: {total_s:.0f} s_", "", "## System", ""]
    L.append(md_table(["Eigenschaft", "Wert"], [[k, v] for k, v in info.items() if not k.startswith("_")], left=2))
    L += ["", "## Läufe", ""]
    L.append(md_table(["Benchmark", "Status", "Dauer"],
                      [[r["title"], "✅ ok" if r["status"] == "ok" else "❌ fehlgeschlagen",
                        f"{r['duration_s']:.1f} s"] for r in runs], left=1))
    if all_headlines:
        L += ["", "## Kennzahlen", ""]
        if base_note:
            L += [f"_{base_note}_", ""]
        rows = []
        for h in all_headlines:
            text, kind = delta_info(h, base)
            mark = {"good": "🟢 ", "bad": "🔴 ", "neutral": "⚪ "}.get(kind, "")
            rows.append([h["benchmark"], h["label"], f"**{fmt_num(h['value'])}** {h['unit']}",
                         f"{mark}{text}" if text else "-", "↑" if h["higher_is_better"] else "↓"])
        L.append(md_table(["Benchmark", "Kennzahl", "Wert", "Δ Baseline", "besser"], rows, left=2))
    L += ["", "## Auswertung", ""]
    for r in runs:
        res = r["result"]
        if not res:
            continue
        lines = list(res.get("insights", [])) + [x for x in (auto_insights(s) for s in res["sections"]) if x]
        if lines:
            L += [f"**{r['title']}**", ""] + [f"- {s}" for s in lines] + [""]
    L += ["## Details", ""]
    for r in runs:
        res = r["result"]
        if not res:
            L += [f"### {r['title']}", "", f"❌ fehlgeschlagen: `{r['error']}`", ""]
            continue
        L += [f"### {r['title']}", "", "`" + "  ".join(f"{k}={v}" for k, v in res["meta"].items()) + "`", ""]
        for sec in res["sections"]:
            heads, rows, facs = section_cells(sec)
            shares = share_values(sec)
            if facs:
                heads = heads + ["Faktor"]
                rows = [row + ["-" if f is None else f"{f:.2f}×"] for row, f in zip(rows, facs)]
            elif shares:
                heads = heads + ["Anteil am Max"]
                rows = [row + ["-" if s is None else f"{s:.0%}"] for row, s in zip(rows, shares)]
            L += [f"#### {sec['title']}", ""]
            if sec.get("note"):
                L += [f"_{sec['note']}_", ""]
            L += [md_table(heads, rows), ""]
    Path(path).write_text("\n".join(L), encoding="utf-8")


# --------------------------------------------------------------------------- main
def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--quick", action="store_true", help="kleine Parameter (Smoke-Test, nicht vergleichbar mit vollem Lauf)")
    ap.add_argument("--only", nargs="+", choices=[b.key for b in BENCHES])
    ap.add_argument("--skip", nargs="+", choices=[b.key for b in BENCHES], default=[])
    ap.add_argument("--out", help="Ausgabeverzeichnis (Standard: bench_results/<Zeitstempel>)")
    ap.add_argument("--baseline", help="results.json eines früheren Laufs für Δ-Vergleich")
    ap.add_argument("--no-baseline", action="store_true", help="keinen automatischen Vergleich mit latest.json")
    args = ap.parse_args()

    if not shutil.which("uv"):
        sys.exit("uv nicht gefunden – Installation: https://docs.astral.sh/uv/")

    selected = [b for b in BENCHES if (not args.only or b.key in args.only) and b.key not in args.skip]
    if not selected:
        sys.exit("Keine Benchmarks ausgewählt.")

    now = datetime.now()
    root = Path("bench_results")
    # Absolute so the bench scripts (run with cwd=benchmarks/) and the runner
    # agree on where raw/*.json is written, regardless of the caller's cwd.
    outdir = (Path(args.out) if args.out else root / now.strftime("%Y%m%d-%H%M%S")).resolve()
    outdir.mkdir(parents=True, exist_ok=True)

    # Baseline laden
    base, base_note = {}, None
    base_path = Path(args.baseline) if args.baseline else (None if args.no_baseline else root / "latest.json")
    if base_path and base_path.exists():
        try:
            prev = json.loads(base_path.read_text())
            if prev["meta"].get("quick") == args.quick:
                base = {h["key"]: h["value"] for h in prev.get("headlines", [])}
                base_note = f"Δ gegenüber Lauf vom {prev['meta']['timestamp']}"
            else:
                base_note = "Baseline übersprungen (quick/voll-Modus passt nicht)"
        except Exception:
            base_note = "Baseline nicht lesbar"

    info = system_info()
    sys_tbl = Table.grid(padding=(0, 2))
    for k, v in info.items():
        if not k.startswith("_"):
            sys_tbl.add_row(f"[dim]{k}[/dim]", escape(str(v)))
    console.print(Panel(sys_tbl, title="[bold]Benchmark-Suite" + (" (quick)" if args.quick else ""),
                        border_style="cyan", expand=False))
    if (os.cpu_count() or 1) < 4:
        console.print(f"[yellow]⚠ Nur {os.cpu_count()} CPU-Kern(e): Last-Generator und Server konkurrieren um CPU – "
                      f"HTTP-Durchsätze sind dann nur grobe Richtwerte.[/yellow]")
    if info.get("_load", 0) > (os.cpu_count() or 1) * 0.5:
        console.print("[yellow]⚠ Hohe Systemlast vor dem Start – Ergebnisse können verfälscht sein.[/yellow]")

    runs = []
    t_start = time.perf_counter()
    try:
        for i, b in enumerate(selected, 1):
            with console.status(f"[bold cyan][{i}/{len(selected)}] {b.title} läuft …"):
                r = run_bench(b, args.quick, outdir)
            runs.append(r)
            ok = r["status"] == "ok"
            console.print(f"{'[green]✔[/green]' if ok else '[red]✘[/red]'} {b.title} ({r['duration_s']:.1f} s)")
    except KeyboardInterrupt:
        console.print("[yellow]Abgebrochen – Report enthält nur bereits abgeschlossene Benchmarks.[/yellow]")
    total_s = time.perf_counter() - t_start

    all_headlines = []
    for r in runs:
        if r["result"]:
            for h in r["result"].get("headlines", []):
                all_headlines.append({**h, "benchmark": r["title"]})

    meta = {"timestamp": now.strftime("%Y-%m-%d %H:%M:%S"), "quick": args.quick}
    render_console(runs, all_headlines, base, str(outdir), meta, total_s, base_note)

    # Dateien schreiben
    summary = {"meta": {**meta, "total_s": total_s, "system": {k: v for k, v in info.items() if not k.startswith("_")}},
               "runs": [{k: r[k] for k in ("key", "title", "status", "duration_s", "error")} for r in runs],
               "benchmarks": {r["key"]: r["result"] for r in runs},
               "headlines": all_headlines}
    (outdir / "results.json").write_text(json.dumps(summary, indent=2, ensure_ascii=False))
    write_markdown(outdir / "report.md", runs, all_headlines, base, info, meta, total_s, base_note)
    console.print(f"\n[bold]Gesamtdauer:[/bold] {total_s:.0f} s")
    console.print(f"[bold]Report:[/bold] {outdir}/report.md · report.html · report.txt · results.json")
    console.save_html(str(outdir / "report.html"))
    console.save_text(str(outdir / "report.txt"))
    root.mkdir(exist_ok=True)
    shutil.copyfile(outdir / "results.json", root / "latest.json")

    sys.exit(0 if all(r["status"] == "ok" for r in runs) else 1)


if __name__ == "__main__":
    main()

