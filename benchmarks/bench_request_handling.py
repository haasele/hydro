#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["uvloop; sys_platform != 'win32'"]
# ///
"""
Request-Handling-Benchmark für Webserver (reine Standardbibliothek + optional uvloop).

Teil 1: Micro-Benchmarks der Pipeline-Stufen (Parsen, Routing, JSON, Response bauen)
Teil 2: Live-Test gegen minimalen asyncio-HTTP-Server (eigener Prozess, Keep-Alive)

  uv run bench_request_handling.py
  uv run bench_request_handling.py --concurrency 100 --duration 10
  uv run bench_request_handling.py --skip-live --no-uvloop
"""
import argparse
import asyncio
import json
import multiprocessing as mp
import platform
import socket
import sys
import time
import timeit
from pathlib import Path

try:
    import uvloop
except ImportError:
    uvloop = None


def run_async(coro, use_uvloop):
    if use_uvloop and uvloop is not None:
        if sys.version_info >= (3, 12):
            return asyncio.run(coro, loop_factory=uvloop.new_event_loop)
        uvloop.install()
    return asyncio.run(coro)


# --------------------------------------------------------------------------- Server-Logik
def parse_request(head: bytes):
    lines = head.decode("latin-1").split("\r\n")
    method, target, _version = lines[0].split(" ", 2)
    headers = {}
    for line in lines[1:]:
        if not line:
            continue
        k, _, v = line.partition(":")
        headers[k.strip().lower()] = v.strip()
    return method, target, headers


def parse_query(qs: str):
    out = {}
    for pair in qs.split("&"):
        if pair:
            k, _, v = pair.partition("=")
            out[k] = v
    return out


def h_ping(params, query, body):
    return 200, {"ok": True}


def h_user(params, query, body):
    return 200, {"id": int(params[0]), "name": f"user-{params[0]}", "verbose": query.get("v") == "1"}


def h_echo(params, query, body):
    return 200, json.loads(body) if body else {}


def h_list(params, query, body):
    return 200, [{"id": i, "value": i * 2, "name": f"item-{i}"} for i in range(50)]


STATIC_ROUTES = {
    ("GET", "/ping"): h_ping,
    ("GET", "/list"): h_list,
    ("POST", "/echo"): h_echo,
}
PREFIX_ROUTES = [("GET", "/users/", h_user)]
REASONS = {200: "OK", 404: "Not Found", 500: "Internal Server Error"}


def route(method, target, body):
    path, _, qs = target.partition("?")
    query = parse_query(qs) if qs else {}
    handler = STATIC_ROUTES.get((method, path))
    if handler:
        return handler((), query, body)
    for m, prefix, h in PREFIX_ROUTES:
        if m == method and path.startswith(prefix):
            return h((path[len(prefix):],), query, body)
    return 404, {"error": "not found"}


def build_response(status, payload, keep_alive=True):
    body = json.dumps(payload, separators=(",", ":")).encode()
    head = (f"HTTP/1.1 {status} {REASONS.get(status, 'OK')}\r\n"
            f"Content-Type: application/json\r\n"
            f"Content-Length: {len(body)}\r\n"
            f"Connection: {'keep-alive' if keep_alive else 'close'}\r\n\r\n")
    return head.encode("latin-1") + body


async def handle_conn(reader, writer):
    try:
        while True:
            head = await reader.readuntil(b"\r\n\r\n")
            method, target, headers = parse_request(head[:-4])
            n = int(headers.get("content-length", 0))
            body = await reader.readexactly(n) if n else b""
            keep = headers.get("connection", "keep-alive").lower() != "close"
            try:
                status, payload = route(method, target, body)
            except Exception:
                status, payload = 500, {"error": "internal"}
            writer.write(build_response(status, payload, keep))
            await writer.drain()
            if not keep:
                break
    except (asyncio.IncompleteReadError, ConnectionError):
        pass
    finally:
        writer.close()


def server_main(port, use_uvloop):
    async def run():
        server = await asyncio.start_server(handle_conn, "127.0.0.1", port, backlog=2048)
        async with server:
            await server.serve_forever()

    run_async(run(), use_uvloop)


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


# --------------------------------------------------------------------------- Teil 1: Micro
SAMPLE_HEAD = (b"GET /users/42?v=1 HTTP/1.1\r\nHost: localhost:8000\r\n"
               b"User-Agent: bench/1.0\r\nAccept: */*\r\nAccept-Encoding: gzip, deflate\r\n"
               b"Connection: keep-alive\r\nX-Request-Id: 8f14e45f-ceea-467f-a0e6-3b1d0c1c5f21")
SAMPLE_BODY = json.dumps({"name": "Widget", "price": 9.99, "tags": ["a", "b", "c"]}).encode()
SAMPLE_LIST = h_list((), {}, b"")[1]


def micro(repeat):
    cases = [
        ("parse_request (7 Header)", lambda: parse_request(SAMPLE_HEAD)),
        ("parse_query (4 Parameter)", lambda: parse_query("v=1&page=2&sort=name&order=asc")),
        ("route: statisch /ping", lambda: route("GET", "/ping", b"")),
        ("route: dynamisch /users/42?v=1", lambda: route("GET", "/users/42?v=1", b"")),
        ("route: 404 (kein Treffer)", lambda: route("GET", "/nope", b"")),
        ("route: POST /echo (JSON-Body)", lambda: route("POST", "/echo", SAMPLE_BODY)),
        ("json.loads (Body)", lambda: json.loads(SAMPLE_BODY)),
        ("build_response: klein", lambda: build_response(200, {"ok": True})),
        ("build_response: 50 Objekte", lambda: build_response(200, SAMPLE_LIST)),
        ("Pipeline komplett (parse+route+build)", lambda: build_response(
            *route(*parse_request(SAMPLE_HEAD)[:2], b""))),
    ]
    rows = []
    for name, fn in cases:
        log(f"request: micro {name}")
        n, _ = timeit.Timer(fn).autorange()
        best = min(timeit.repeat(fn, number=n, repeat=repeat)) / n
        rows.append({"stage": name, "us_op": best * 1e6, "ops_s": 1 / best})
    return rows


# --------------------------------------------------------------------------- Teil 2: Live
def make_request(method, path, body=None):
    if body is None:
        return (f"{method} {path} HTTP/1.1\r\nHost: localhost\r\n"
                f"Connection: keep-alive\r\n\r\n").encode()
    return (f"{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n"
            f"Content-Length: {len(body)}\r\nConnection: keep-alive\r\n\r\n").encode() + body


LIVE_SCENARIOS = [
    ("ping", "GET /ping", make_request("GET", "/ping")),
    ("user", "GET /users/42?v=1", make_request("GET", "/users/42?v=1")),
    ("echo", "POST /echo", make_request("POST", "/echo", SAMPLE_BODY)),
    ("list", "GET /list (50 Objekte)", make_request("GET", "/list")),
    ("404", "GET /nope (404)", make_request("GET", "/nope")),
]


def pct(vals, p):
    return vals[min(len(vals) - 1, int(len(vals) * p / 100))]


async def client_worker(port, raw, deadline, lat, errors):
    reader, writer = await asyncio.open_connection("127.0.0.1", port)
    try:
        while True:
            t0 = time.perf_counter()
            if t0 >= deadline:
                return
            writer.write(raw)
            head = await reader.readuntil(b"\r\n\r\n")
            status = int(head[9:12])
            length = 0
            for line in head.split(b"\r\n"):
                if line[:15].lower() == b"content-length:":
                    length = int(line[15:])
            await reader.readexactly(length)
            lat.append(time.perf_counter() - t0)
            if status >= 500:
                errors[0] += 1
    except Exception:
        errors[0] += 1
    finally:
        writer.close()


async def run_live(port, concurrency, duration, warmup):
    rows = []
    for key, name, raw in LIVE_SCENARIOS:
        log(f"request: live {name}")
        for dur in (warmup, duration):  # erster Durchlauf = Warmup (verworfen)
            lat, errors = [], [0]
            start = time.perf_counter()
            deadline = start + dur
            await asyncio.gather(*(client_worker(port, raw, deadline, lat, errors)
                                   for _ in range(concurrency)))
            elapsed = time.perf_counter() - start
        lat.sort()
        rows.append({
            "key": key, "scenario": name, "req_s": len(lat) / elapsed,
            "mean_ms": sum(lat) / len(lat) * 1000, "p50_ms": pct(lat, 50) * 1000,
            "p95_ms": pct(lat, 95) * 1000, "p99_ms": pct(lat, 99) * 1000, "errors": errors[0],
        })
    return rows


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_for_port(port, timeout=10):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)
    raise RuntimeError("Server nicht erreichbar")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--concurrency", type=int, default=50)
    ap.add_argument("--duration", type=float, default=5.0)
    ap.add_argument("--warmup", type=float, default=1.0)
    ap.add_argument("--repeat", type=int, default=5, help="Wiederholungen der Micro-Benchmarks")
    ap.add_argument("--skip-live", action="store_true")
    ap.add_argument("--no-uvloop", action="store_true")
    ap.add_argument("--json", help="Ergebnis als JSON in diese Datei schreiben")
    args = ap.parse_args()
    use_uvloop = uvloop is not None and not args.no_uvloop

    micro_rows = micro(args.repeat)
    sections = [{
        "title": "Pipeline-Stufen (Micro-Benchmark, 1 Thread, ohne Netzwerk)",
        "columns": [
            {"key": "stage", "label": "Stufe"},
            {"key": "us_op", "label": "Zeit/Op", "unit": "µs", "fmt": ".2f"},
            {"key": "ops_s", "label": "Ops/s", "fmt": ",.0f"},
        ],
        "rows": micro_rows, "share": "us_op",
        "note": "Balken = Anteil an der teuersten Stufe (Stufen sind keine Alternativen).",
    }]
    pipeline_us = micro_rows[-1]["us_op"]
    headlines = [hl("request.pipeline_us", "Pipeline parse+route+build", pipeline_us, "µs", False)]
    insights = []

    if not args.skip_live:
        port = free_port()
        proc = mp.Process(target=server_main, args=(port, use_uvloop), daemon=True)
        proc.start()
        try:
            wait_for_port(port)
            live = run_async(run_live(port, args.concurrency, args.duration, args.warmup), use_uvloop)
        finally:
            proc.terminate()
            proc.join()
        sections.append({
            "title": "Live-Test (asyncio-Server, Keep-Alive)",
            "note": f"concurrency={args.concurrency}, uvloop={'ja' if use_uvloop else 'nein'}",
            "columns": [
                {"key": "scenario", "label": "Szenario"},
                {"key": "req_s", "label": "Durchsatz", "unit": "req/s", "fmt": ",.0f"},
                {"key": "mean_ms", "label": "mean", "unit": "ms", "fmt": ".2f"},
                {"key": "p50_ms", "label": "p50", "unit": "ms", "fmt": ".2f"},
                {"key": "p95_ms", "label": "p95", "unit": "ms", "fmt": ".2f"},
                {"key": "p99_ms", "label": "p99", "unit": "ms", "fmt": ".2f"},
                {"key": "errors", "label": "Fehler"},
            ],
            "rows": live, "primary": "req_s", "lower_is_better": False,
        })
        by = {r["key"]: r for r in live}
        headlines += [
            hl("request.live_ping", "Live /ping", by["ping"]["req_s"], "req/s", True),
            hl("request.live_list", "Live /list (50 Obj.)", by["list"]["req_s"], "req/s", True),
            hl("request.live_ping_p99", "Live /ping p99", by["ping"]["p99_ms"], "ms", False),
        ]
        theo = 1e6 / pipeline_us
        insights.append(
            f"Die Pipeline allein erlaubt ≈ {theo:,.0f} req/s pro Kern; live erreicht /ping "
            f"{by['ping']['req_s']:,.0f} req/s ({by['ping']['req_s'] / theo:.0%}) – der Rest entfällt "
            f"auf Socket-I/O, Event-Loop und den Client im selben Host.")
        insights.append(f"Ein 50-Objekte-JSON senkt den Durchsatz auf "
                        f"{by['list']['req_s'] / by['ping']['req_s']:.0%} von /ping (Serialisierungskosten).")

    emit({
        "benchmark": "request", "title": "Request-Handling (Webserver)",
        "meta": {"concurrency": args.concurrency, "duration_s": args.duration,
                 "uvloop": "ja" if use_uvloop else "nein", "live": "nein" if args.skip_live else "ja"},
        "sections": sections, "headlines": headlines, "insights": insights,
    }, args.json)


if __name__ == "__main__":
    main()

