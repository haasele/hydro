# Benchmark-Report

_Erstellt: 2026-10-23 21:50:53 · Modus: quick · Gesamtdauer: 83 s_

## System

| Eigenschaft | Wert |
| :--- | :--- |
| OS | Linux-7.0.0-38-generic-x86_64-with-glibc2.39 |
| CPU | AMD EPYC-Rome Processor |
| Kerne | 10 |
| RAM | 25.2 GB |
| Python (Runner) | 3.12.3 |
| uv | uv 0.12.11 (x86_64-unknown-linux-gnu) |
| Last (1 min) | 1.19 |

## Läufe

| Benchmark | Status | Dauer |
| :--- | ---: | ---: |
| For-Schleifen | ✅ ok | 2.4 s |
| CSV | ✅ ok | 10.5 s |
| Startzeiten | ✅ ok | 13.9 s |
| Request-Handling | ✅ ok | 28.9 s |
| FastAPI-Durchsatz | ✅ ok | 27.5 s |

## Kennzahlen

| Benchmark | Kennzahl | Wert | Δ Baseline | besser |
| :--- | :--- | ---: | ---: | ---: |
| For-Schleifen | for-Schleife: Summe | **45.40** ns/Iter | - | ↓ |
| For-Schleifen | sum(range) | **15.46** ns/Iter | - | ↓ |
| For-Schleifen | List-Comprehension | **51.56** ns/Iter | - | ↓ |
| For-Schleifen | Schleife mit Funktionsaufruf | **65.09** ns/Iter | - | ↓ |
| CSV | Zeilen generieren | **162,730** Zeilen/s | - | ↑ |
| CSV | csv.writer schreiben | **509,320** Zeilen/s | - | ↑ |
| CSV | csv.reader lesen | **1,038,888** Zeilen/s | - | ↑ |
| CSV | Aggregation (DictReader) | **339,042** Zeilen/s | - | ↑ |
| CSV | pandas.read_csv | **1,890,632** Zeilen/s | - | ↑ |
| Startzeiten | python -c pass | **16.52** ms | - | ↓ |
| Startzeiten | import pandas | **398.4** ms | - | ↓ |
| Startzeiten | import fastapi | **367.8** ms | - | ↓ |
| Startzeiten | uvicorn Kaltstart | **431.0** ms | - | ↓ |
| Request-Handling | Pipeline parse+route+build | **9.44** µs | - | ↓ |
| Request-Handling | Live /ping | **33,705** req/s | - | ↑ |
| Request-Handling | Live /list (50 Obj.) | **10,106** req/s | - | ↑ |
| Request-Handling | Live /ping p99 | **11.67** ms | - | ↓ |
| FastAPI-Durchsatz | ASGI /ping | **2,445** req/s | - | ↑ |
| FastAPI-Durchsatz | ASGI POST /items | **1,710** req/s | - | ↑ |
| FastAPI-Durchsatz | ASGI /json100 | **678.4** req/s | - | ↑ |
| FastAPI-Durchsatz | ASGI /ping p99 | **0.612** ms | - | ↓ |
| FastAPI-Durchsatz | HTTP /ping | **230.7** req/s | - | ↑ |
| FastAPI-Durchsatz | HTTP POST /items | **395.1** req/s | - | ↑ |
| FastAPI-Durchsatz | HTTP /json100 | **409.1** req/s | - | ↑ |
| FastAPI-Durchsatz | HTTP /ping p99 | **541.0** ms | - | ↓ |

## Auswertung

**For-Schleifen**

- List-Comprehension ist 1.10× schneller als append() in einer for-Schleife.
- sum(range(n)) ist 2.9× schneller als die manuelle for-Schleife.
- numpy summiert 29× schneller als builtin sum() (reine C-Schleife).
- ''.join() ist 1.55× schneller als String-Konkatenation mit += .
- Index-Zugriff (data[i]) ist 1.38× langsamer als direkte Iteration.
- Ein Python-Funktionsaufruf kostet ≈ 24 ns zusätzlich pro Iteration.
- [Summieren] Bester Wert: sum_numpy (0.1 ms); schwächster: sum_map (11.6 ms) – Spanne 107.5×
- [Liste bauen] Bester Wert: list_numpy (6.4 ms); schwächster: list_map_lambda (21.2 ms) – Spanne 3.3×
- [Filtern (jedes 3. Element)] Bester Wert: filter_slice (1.4 ms); schwächster: filter_builtin (18.2 ms) – Spanne 13.3×
- [Dict & String bauen] Bester Wert: str_join (2.0 ms); schwächster: dict_zip (33.7 ms) – Spanne 16.8×
- [Iteration über Liste (inkl. Listenaufbau)] Bester Wert: iter_direct (10.9 ms); schwächster: iter_zip (27.7 ms) – Spanne 2.5×
- [Verschachtelt & Funktionsaufrufe] Bester Wert: call_inline (8.2 ms); schwächster: call_lambda (13.2 ms) – Spanne 1.6×

**CSV**

- DictReader ist 2.39× langsamer als csv.reader (Preis für Spaltennamen-Zugriff).
- DictWriter ist 2.13× langsamer als csv.writer.
- pandas.read_csv liest 1.8× schneller als csv.reader (C-Parser).
- Aggregation: pandas ist 4.1× schneller als reines Python.
- [Schreiben (Datei)] Bester Wert: manuell (join) (0.194 s); schwächster: csv.DictWriter (0.419 s) – Spanne 2.2×
- [Lesen] Bester Wert: str.split (0.051 s); schwächster: csv.DictReader (0.230 s) – Spanne 4.5×
- [Aggregation (Gruppieren nach Kategorie)] Bester Wert: Aggregation (pandas groupby) (0.071 s); schwächster: Aggregation (DictReader) (0.295 s) – Spanne 4.1×

**Startzeiten**

- uv run fügt ≈ 10 ms Overhead pro Start (Resolver + Env-Check) hinzu.
- Teuerste Imports: pandas (+398 ms), fastapi (+368 ms), requests (+149 ms).
- uvicorn-Kaltstart 431 ms = FastAPI-Import inkl. Interpreter ≈ 398 ms + Server-Init/Listen/erster Request ≈ 33 ms.
- [Interpreter-Start] Bester Wert: python -S -c pass (ohne site) (10.5 ms); schwächster: uv run --no-project python -c pass (26.1 ms) – Spanne 2.5×

**Request-Handling**

- Die Pipeline allein erlaubt ≈ 105,951 req/s pro Kern; live erreicht /ping 33,705 req/s (32%) – der Rest entfällt auf Socket-I/O, Event-Loop und den Client im selben Host.
- Ein 50-Objekte-JSON senkt den Durchsatz auf 30% von /ping (Serialisierungskosten).
- [Live-Test (asyncio-Server, Keep-Alive)] Bester Wert: GET /ping (33,705 req/s); schwächster: GET /list (50 Objekte) (10,106 req/s) – Spanne 3.3×

**FastAPI-Durchsatz**

- [ASGI] JSON-Antwort mit 100 Objekten kostet 3.6× so viel Zeit pro Request wie /ping.
- [ASGI] POST mit Pydantic-Validierung kostet 1.4× so viel Zeit pro Request wie /ping.
- [ASGI] `def`-Endpoint (Threadpool) erreicht 64% des async-Durchsatzes.
- [HTTP] JSON-Antwort mit 100 Objekten: kein messbarer Mehraufwand ggü. /ping (Server-/Client-Overhead dominiert).
- [HTTP] POST mit Pydantic-Validierung: kein messbarer Mehraufwand ggü. /ping (Server-/Client-Overhead dominiert).
- [HTTP] `def`-Endpoint (Threadpool) ist 1.14× schneller als async.
- Server-/Netzwerk-Stack kostet: ASGI-in-process schafft 10.6× mehr /ping-req/s als HTTP.
- [Modus: ASGI] Bester Wert: GET /ping (async) (2,445 req/s); schwächster: GET /json100 (100 Objekte) (678 req/s) – Spanne 3.6×
- [Modus: HTTP] Bester Wert: GET /items/{id}?q= (Path+Query) (421 req/s); schwächster: GET /ping (async) (231 req/s) – Spanne 1.8×

## Details

### For-Schleifen

`n=200000  repeat=3  numpy=2.5.4`

#### Summieren

| Variante | best (ms) | median (ms) | pro Iteration (ns) | Faktor |
| :--- | ---: | ---: | ---: | ---: |
| sum_numpy | 0.1 | 0.6 | 0.5 | 1.00× |
| sum_builtin | 3.1 | 3.2 | 15.5 | 28.72× |
| sum_genexp | 6.9 | 7.0 | 34.6 | 64.31× |
| sum_reduce | 8.0 | 8.0 | 39.8 | 73.96× |
| sum_for | 9.1 | 9.1 | 45.4 | 84.33× |
| sum_while | 10.7 | 10.8 | 53.6 | 99.56× |
| sum_map | 11.6 | 14.4 | 57.9 | 107.55× |

#### Liste bauen

| Variante | best (ms) | median (ms) | pro Iteration (ns) | Faktor |
| :--- | ---: | ---: | ---: | ---: |
| list_numpy | 6.4 | 6.7 | 31.9 | 1.00× |
| list_comp | 10.3 | 10.4 | 51.6 | 1.62× |
| list_append | 11.3 | 11.6 | 56.7 | 1.78× |
| list_append_bound | 12.6 | 12.7 | 63.2 | 1.98× |
| list_map_lambda | 21.2 | 21.5 | 106.2 | 3.33× |

#### Filtern (jedes 3. Element)

| Variante | best (ms) | median (ms) | pro Iteration (ns) | Faktor |
| :--- | ---: | ---: | ---: | ---: |
| filter_slice | 1.4 | 1.4 | 6.8 | 1.00× |
| filter_comp | 9.1 | 11.8 | 45.3 | 6.62× |
| filter_for | 9.6 | 9.8 | 48.1 | 7.04× |
| filter_builtin | 18.2 | 18.3 | 91.0 | 13.31× |

#### Dict & String bauen

| Variante | best (ms) | median (ms) | pro Iteration (ns) | Faktor |
| :--- | ---: | ---: | ---: | ---: |
| str_join | 2.0 | 2.0 | 100.1 | 1.00× |
| str_join_gen | 2.2 | 2.6 | 112.2 | 1.12× |
| str_concat | 3.1 | 3.1 | 155.5 | 1.55× |
| dict_comp | 23.3 | 24.1 | 116.3 | 11.61× |
| dict_for | 24.0 | 29.4 | 120.2 | 12.00× |
| dict_zip | 33.7 | 34.7 | 168.4 | 16.82× |

#### Iteration über Liste (inkl. Listenaufbau)

| Variante | best (ms) | median (ms) | pro Iteration (ns) | Faktor |
| :--- | ---: | ---: | ---: | ---: |
| iter_direct | 10.9 | 11.0 | 54.5 | 1.00× |
| iter_index | 15.1 | 16.1 | 75.4 | 1.38× |
| iter_enumerate | 17.5 | 17.7 | 87.7 | 1.61× |
| iter_zip | 27.7 | 27.9 | 138.3 | 2.54× |

#### Verschachtelt & Funktionsaufrufe

| Variante | best (ms) | median (ms) | pro Iteration (ns) | Faktor |
| :--- | ---: | ---: | ---: | ---: |
| call_inline | 8.2 | 8.2 | 41.0 | 1.00× |
| nested_comp | 9.9 | 10.1 | 49.6 | 1.21× |
| nested_loops | 10.5 | 10.7 | 52.4 | 1.28× |
| call_function | 13.0 | 13.2 | 65.1 | 1.59× |
| call_lambda | 13.2 | 13.4 | 65.9 | 1.61× |

### CSV

`rows=100000  repeat=2  dateigroesse_mb=5.4  pandas=3.1.0`

#### In-Memory

_Unabhängige Messungen, kein Vergleich untereinander._

| Test | best (s) | median (s) | Zeilen/s | Durchsatz (MB/s) |
| :--- | ---: | ---: | ---: | ---: |
| Zeilen generieren | 0.615 | 0.641 | 162,730 | - |
| csv.writer -> StringIO | 0.176 | 0.177 | 568,603 | 30.6 |

#### Schreiben (Datei)

| Test | best (s) | median (s) | Zeilen/s | Durchsatz (MB/s) | Faktor |
| :--- | ---: | ---: | ---: | ---: | ---: |
| csv.writer | 0.196 | 0.200 | 509,320 | 27.4 | 1.01× |
| csv.DictWriter | 0.419 | 0.458 | 238,878 | 12.8 | 2.16× |
| manuell (join) | 0.194 | 0.198 | 515,625 | 27.7 | 1.00× |
| pandas.to_csv | 0.344 | 0.359 | 290,637 | 15.6 | 1.77× |

#### Lesen

_str.split parst keine Quotes/Escapes – nur Untergrenze als Referenz._

| Test | best (s) | median (s) | Zeilen/s | Durchsatz (MB/s) | Faktor |
| :--- | ---: | ---: | ---: | ---: | ---: |
| csv.reader | 0.096 | 0.102 | 1,038,888 | 55.8 | 1.87× |
| csv.DictReader | 0.230 | 0.244 | 435,094 | 23.4 | 4.46× |
| str.split | 0.051 | 0.057 | 1,942,010 | 104.4 | 1.00× |
| pandas.read_csv | 0.053 | 0.057 | 1,890,632 | 101.6 | 1.03× |

#### Aggregation (Gruppieren nach Kategorie)

| Test | best (s) | median (s) | Zeilen/s | Durchsatz (MB/s) | Faktor |
| :--- | ---: | ---: | ---: | ---: | ---: |
| Aggregation (DictReader) | 0.295 | 0.303 | 339,042 | 18.2 | 4.15× |
| Aggregation (pandas groupby) | 0.071 | 0.072 | 1,406,437 | 75.6 | 1.00× |

#### Transformation (Lesen → Schreiben)

| Test | best (s) | median (s) | Zeilen/s | Durchsatz (MB/s) |
| :--- | ---: | ---: | ---: | ---: |
| DictReader → Transform → csv.writer | 0.544 | 0.552 | 183,682 | 9.9 |

### Startzeiten

`runs=3  python_exe=python`

#### Interpreter-Start

| Test | min (ms) | median (ms) | max (ms) | Faktor |
| :--- | ---: | ---: | ---: | ---: |
| python -c pass | 16.5 | 16.5 | 17.3 | 1.57× |
| python -S -c pass (ohne site) | 9.9 | 10.5 | 10.6 | 1.00× |
| python -I -c pass (isolated) | 17.5 | 17.9 | 20.0 | 1.70× |
| uv run --no-project python -c pass | 25.8 | 26.1 | 27.7 | 2.47× |

#### Import-Zeiten

_Δ = min minus min des leeren Interpreters (rauschärmer als Median); Balken = Anteil am teuersten Import._

| Test | min (ms) | median (ms) | max (ms) | Δ zu leer (ms) | Anteil am Max |
| :--- | ---: | ---: | ---: | ---: | ---: |
| import pandas | 414.8 | 443.6 | 453.6 | +398.4 | 100% |
| import fastapi | 384.3 | 384.3 | 389.1 | +367.8 | 92% |
| import requests | 165.4 | 172.7 | 237.5 | +148.9 | 37% |
| import uvicorn | 123.4 | 124.8 | 137.2 | +106.9 | 27% |
| import httpx | 103.4 | 111.4 | 121.9 | +86.9 | 22% |
| import numpy | 100.5 | 110.0 | 143.7 | +84.1 | 21% |
| import asyncio | 66.2 | 66.5 | 67.5 | +49.7 | 12% |
| import pydantic | 63.4 | 66.8 | 69.9 | +46.9 | 12% |
| import http.server | 52.3 | 54.6 | 55.5 | +35.9 | 9% |
| import dataclasses | 31.1 | 31.9 | 32.1 | +14.6 | 4% |
| import typing | 23.5 | 25.0 | 25.4 | +7.1 | 2% |
| import json | 21.0 | 21.7 | 22.1 | +4.5 | 1% |
| import csv | 19.9 | 21.5 | 23.1 | +3.4 | 1% |
| import sqlite3 | 19.7 | 19.8 | 20.5 | +3.3 | 1% |
| import decimal | 17.1 | 17.3 | 17.8 | +0.7 | 0% |
| import starlette | 15.9 | 16.4 | 16.5 | +0.0 | - |

#### FastAPI & uvicorn

| Test | min (ms) | median (ms) | max (ms) |
| :--- | ---: | ---: | ---: |
| python: import fastapi + App + 1 Route | 393.3 | 398.3 | 426.5 |
| uvicorn: Prozessstart → erste 200-Antwort | 398.5 | 431.0 | 487.5 |

### Request-Handling

`concurrency=50  duration_s=2.0  uvloop=ja  live=ja`

#### Pipeline-Stufen (Micro-Benchmark, 1 Thread, ohne Netzwerk)

_Balken = Anteil an der teuersten Stufe (Stufen sind keine Alternativen)._

| Stufe | Zeit/Op (µs) | Ops/s | Anteil am Max |
| :--- | ---: | ---: | ---: |
| parse_request (7 Header) | 2.75 | 364,054 | 7% |
| parse_query (4 Parameter) | 1.04 | 960,079 | 3% |
| route: statisch /ping | 0.46 | 2,176,454 | 1% |
| route: dynamisch /users/42?v=1 | 1.46 | 685,019 | 4% |
| route: 404 (kein Treffer) | 0.55 | 1,812,658 | 1% |
| route: POST /echo (JSON-Body) | 3.85 | 259,898 | 10% |
| json.loads (Body) | 3.36 | 298,048 | 9% |
| build_response: klein | 3.90 | 256,409 | 10% |
| build_response: 50 Objekte | 38.98 | 25,655 | 100% |
| Pipeline komplett (parse+route+build) | 9.44 | 105,951 | 24% |

#### Live-Test (asyncio-Server, Keep-Alive)

_concurrency=50, uvloop=ja_

| Szenario | Durchsatz (req/s) | mean (ms) | p50 (ms) | p95 (ms) | p99 (ms) | Fehler | Faktor |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| GET /ping | 33,705 | 1.48 | 1.20 | 2.40 | 11.67 | 0 | 1.00× |
| GET /users/42?v=1 | 29,909 | 1.67 | 1.41 | 2.53 | 11.81 | 0 | 1.13× |
| POST /echo | 19,934 | 2.50 | 2.13 | 3.71 | 12.96 | 0 | 1.69× |
| GET /list (50 Objekte) | 10,106 | 4.93 | 4.13 | 11.58 | 16.21 | 0 | 3.34× |
| GET /nope (404) | 31,836 | 1.57 | 1.30 | 2.62 | 11.40 | 0 | 1.06× |

### FastAPI-Durchsatz

`modes=asgi,http  concurrency=32  duration_s=2.0  fastapi=0.162.0  uvicorn=0.54.0  httpx=0.28.1`

#### Modus: ASGI

_In-Process (ASGITransport): reiner Framework-Overhead ohne Netzwerk/Server._

| Szenario | Durchsatz (req/s) | mean (ms) | p50 (ms) | p95 (ms) | p99 (ms) | Fehler | Faktor |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| GET /ping (async) | 2,445 | 0.41 | 0.35 | 0.49 | 0.61 | 0 | 1.00× |
| GET /sync (threadpool) | 1,560 | 20.43 | 20.22 | 26.91 | 30.51 | 0 | 1.57× |
| GET /items/{id}?q= (Path+Query) | 2,024 | 0.49 | 0.41 | 0.58 | 0.75 | 0 | 1.21× |
| POST /items (Pydantic) | 1,710 | 0.58 | 0.49 | 0.73 | 0.98 | 0 | 1.43× |
| GET /json100 (100 Objekte) | 678 | 1.47 | 1.29 | 1.72 | 11.68 | 0 | 3.60× |

#### Modus: HTTP

_uvicorn in eigenem Prozess, httpx über localhost, Keep-Alive. httpx ist selbst CPU-lastig: Werte sind eine Untergrenze, nicht das Server-Maximum._

| Szenario | Durchsatz (req/s) | mean (ms) | p50 (ms) | p95 (ms) | p99 (ms) | Fehler | Faktor |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| GET /ping (async) | 231 | 135.61 | 106.33 | 354.56 | 540.98 | 0 | 1.82× |
| GET /sync (threadpool) | 263 | 119.92 | 82.15 | 331.90 | 545.55 | 0 | 1.60× |
| GET /items/{id}?q= (Path+Query) | 421 | 74.83 | 53.15 | 206.31 | 295.02 | 0 | 1.00× |
| POST /items (Pydantic) | 395 | 79.97 | 55.16 | 233.04 | 335.53 | 0 | 1.07× |
| GET /json100 (100 Objekte) | 409 | 76.90 | 52.83 | 221.00 | 330.62 | 0 | 1.03× |
