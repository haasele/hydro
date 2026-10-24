#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["fastapi", "uvicorn[standard]", "httpx"]
# ///
"""
FastAPI-Durchsatz-Benchmark.

Modi:
  asgi  - In-Process via httpx.ASGITransport (reiner Framework-Overhead, kein Netzwerk)
  http  - Echter uvicorn-Server in separatem Prozess + httpx über localhost
  both  - beides (Standard)

  uv run bench_fastapi_throughput.py
  uv run bench_fastapi_throughput.py --mode http --concurrency 64 --duration 10
"""
import argparse
import asyncio
import json
import multiprocessing as mp
import platform
import socket
import sys
import time
from importlib.metadata import version
from pathlib import Path

import httpx
import uvicorn
from fastapi import FastAPI
from pydantic import BaseModel

# --------------------------------------------------------------------------- App
app = FastAPI()


class Item(BaseModel):
    name: str
    price: float
    tags: list[str] = []


@app.get("/ping")
async def ping():
    return {"ok": True}


@app.get("/sync")
def sync_endpoint():
    return {"ok": True}


@app.get("/items/{item_id}")
async def get_item(item_id: int, q: str | None = None):
    return {"id": item_id, "q": q}


@app.post("/items")
async def create_item(item: Item):
    return item


@app.get("/json100")
async def json100():
    return [{"id": i, "value": i * 2, "name": f"item-{i}"} for i in range(100)]


SCENARIOS = [
    ("ping", "GET /ping (async)", "GET", "/ping", None),
    ("sync", "GET /sync (threadpool)", "GET", "/sync", None),
    ("item_get", "GET /items/{id}?q= (Path+Query)", "GET", "/items/42?q=hello", None),
    ("item_post", "POST /items (Pydantic)", "POST", "/items",
     {"name": "Widget", "price": 9.99, "tags": ["a", "b", "c"]}),
    ("json100", "GET /json100 (100 Objekte)", "GET", "/json100", None),
]


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
def pct(sorted_vals, p):
    return sorted_vals[min(len(sorted_vals) - 1, int(len(sorted_vals) * p / 100))]


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def run_uvicorn(port):
    uvicorn.run(app, host="127.0.0.1", port=port, log_level="warning", access_log=False)


def wait_for_port(port, timeout=15):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)
    raise RuntimeError("Server nicht erreichbar")


async def run_scenario(client, method, path, body, concurrency, duration):
    latencies, errors = [], 0
    deadline = time.perf_counter() + duration

    async def worker():
        nonlocal errors
        while True:
            t0 = time.perf_counter()
            if t0 >= deadline:
                return
            try:
                r = await client.request(method, path, json=body)
                if r.status_code >= 400:
                    errors += 1
            except Exception:
                errors += 1
            latencies.append(time.perf_counter() - t0)

    start = time.perf_counter()
    await asyncio.gather(*(worker() for _ in range(concurrency)))
    return latencies, errors, time.perf_counter() - start


async def bench_mode(mode, args):
    proc = None
    if mode == "asgi":
        client = httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://test")
    else:
        port = free_port()
        proc = mp.Process(target=run_uvicorn, args=(port,), daemon=True)
        proc.start()
        wait_for_port(port)
        limits = httpx.Limits(max_connections=args.concurrency * 2,
                              max_keepalive_connections=args.concurrency * 2)
        client = httpx.AsyncClient(base_url=f"http://127.0.0.1:{port}", limits=limits)

    rows = []
    try:
        for key, name, method, path, body in SCENARIOS:
            log(f"fastapi[{mode}]: {name}")
            await run_scenario(client, method, path, body, args.concurrency, args.warmup)
            lat, errs, elapsed = await run_scenario(client, method, path, body, args.concurrency, args.duration)
            lat.sort()
            rows.append({
                "key": key, "scenario": name, "req_s": len(lat) / elapsed,
                "mean_ms": sum(lat) / len(lat) * 1000, "p50_ms": pct(lat, 50) * 1000,
                "p95_ms": pct(lat, 95) * 1000, "p99_ms": pct(lat, 99) * 1000, "errors": errs,
            })
    finally:
        await client.aclose()
        if proc:
            proc.terminate()
            proc.join()
    return rows


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--mode", choices=["asgi", "http", "both"], default="both")
    ap.add_argument("--concurrency", type=int, default=32)
    ap.add_argument("--duration", type=float, default=5.0)
    ap.add_argument("--warmup", type=float, default=1.0)
    ap.add_argument("--json", help="Ergebnis als JSON in diese Datei schreiben")
    args = ap.parse_args()

    modes = ["asgi", "http"] if args.mode == "both" else [args.mode]
    notes = {"asgi": "In-Process (ASGITransport): reiner Framework-Overhead ohne Netzwerk/Server.",
             "http": "uvicorn in eigenem Prozess, httpx über localhost, Keep-Alive. "
            "httpx ist selbst CPU-lastig: Werte sind eine Untergrenze, nicht das Server-Maximum."}
    columns = [
        {"key": "scenario", "label": "Szenario"},
        {"key": "req_s", "label": "Durchsatz", "unit": "req/s", "fmt": ",.0f"},
        {"key": "mean_ms", "label": "mean", "unit": "ms", "fmt": ".2f"},
        {"key": "p50_ms", "label": "p50", "unit": "ms", "fmt": ".2f"},
        {"key": "p95_ms", "label": "p95", "unit": "ms", "fmt": ".2f"},
        {"key": "p99_ms", "label": "p99", "unit": "ms", "fmt": ".2f"},
        {"key": "errors", "label": "Fehler"},
    ]

    async def run_all():
        return {m: await bench_mode(m, args) for m in modes}

    data = asyncio.run(run_all())

    sections, headlines, insights = [], [], []
    by = {}
    for m, rows in data.items():
        sections.append({"title": f"Modus: {m.upper()}", "note": notes[m], "columns": columns,
                         "rows": rows, "primary": "req_s", "lower_is_better": False})
        by[m] = {r["key"]: r for r in rows}
        for key, label in [("ping", "/ping"), ("item_post", "POST /items"), ("json100", "/json100")]:
            headlines.append(hl(f"fastapi.{m}.{key}", f"{m.upper()} {label}", by[m][key]["req_s"], "req/s", True))
        headlines.append(hl(f"fastapi.{m}.ping_p99", f"{m.upper()} /ping p99", by[m]["ping"]["p99_ms"], "ms", False))

        tag = m.upper()

        def cost(what, other):
            r = by[m]["ping"]["req_s"] / by[m][other]["req_s"]
            if r >= 1.15:
                insights.append(f"[{tag}] {what} kostet {r:.1f}× so viel Zeit pro Request wie /ping.")
            else:
                insights.append(f"[{tag}] {what}: kein messbarer Mehraufwand ggü. /ping "
                                f"(Server-/Client-Overhead dominiert).")

        cost("JSON-Antwort mit 100 Objekten", "json100")
        cost("POST mit Pydantic-Validierung", "item_post")
        r = by[m]["ping"]["req_s"] / by[m]["sync"]["req_s"]
        if r > 1.05:
            insights.append(f"[{tag}] `def`-Endpoint (Threadpool) erreicht {1 / r:.0%} des async-Durchsatzes.")
        elif r < 0.95:
            insights.append(f"[{tag}] `def`-Endpoint (Threadpool) ist {1 / r:.2f}× schneller als async.")
        else:
            insights.append(f"[{tag}] `def`- und `async def`-Endpoint liegen gleichauf.")
    if len(modes) == 2:
        r = by["asgi"]["ping"]["req_s"] / by["http"]["ping"]["req_s"]
        insights.append(f"Server-/Netzwerk-Stack kostet: ASGI-in-process schafft {r:.1f}× mehr /ping-req/s als HTTP.")

    emit({
        "benchmark": "fastapi", "title": "FastAPI-Durchsatz",
        "meta": {"modes": ",".join(modes), "concurrency": args.concurrency, "duration_s": args.duration,
                 "fastapi": version("fastapi"), "uvicorn": version("uvicorn"), "httpx": version("httpx")},
        "sections": sections, "headlines": headlines, "insights": insights,
    }, args.json)


if __name__ == "__main__":
    main()

