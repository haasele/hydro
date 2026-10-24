# PyX-Benchmarks — Readiness & Integration

Die vorhandenen Harnesses (`bench_loops.py`, `bench_csv.py`, `bench_startup.py`,
`bench_request_handling.py`, `bench_fastapi_throughput.py`) laufen unter
CPython/uv und vergleichen CPython-Varianten (numpy, pandas, FastAPI, ...).
PyX wird **nicht** in diese Dateien eingebaut; stattdessen gibt es:

- `benchmarks/PYX.md` (diese Datei): Readiness-Matrix und Integrationsplan.
- `benchmarks/pyx/`: PyX-native Micro-Benchmarks + `run_micro.py`, das
  CPython, PyX-JIT (`pyx run`) und PyX-AOT (`pyx build`) vergleicht.

## Status der bestehenden Harnesses

| Harness | Benötigte PyX-Features | Status |
|---|---|---|
| `bench_loops.py` | Importe (numpy, argparse, json, platform, statistics, pathlib), `reduce`/`operator.add`, Comprehensions/Lambdas/map/filter/Builtins/Strings | **teilweise**: Sprache, argparse/json/platform/statistics/pathlib, reduce/operator da; **numpy** (C-API) fehlt |
| `bench_csv.py` | `csv`/`io`/`os`/`random`/`pandas`, Datei-I/O, `with`, f-Strings, Dict-Zugriff | **teilweise**: csv/io/with/json/os/random/pathlib/argparse/defaultdict da; **pandas** (C-API) fehlt |
| `bench_startup.py` | Subprozess-Adapter, Import-System + Stdlib-Module; misst Interpreter/Imports/FastAPI-Start | **teilweise**: PyX-Start (JIT/AOT) und native/User-Module messbar; FastAPI/C-Extensions fehlen |
| `bench_request_handling.py` | `asyncio`, `socket`, `json`, `bytes`, async/await, Klassen | **teilweise**: json/os/time/sys, bytes, threading/queue/multiprocessing, blocking socket, Klassen und eager async/await da; echte Coroutinen/Event-Loop fehlen |
| `bench_fastapi_throughput.py` | FastAPI/Pydantic/uvicorn/httpx, Klassen/Decorators/typing, C-Extensions | **blockiert**: Phase 4 (C-API/ABI) + Decorators |

## Micro-Subset (jetzt lauffähig)

`benchmarks/pyx/` enthält überwiegend importfreie PyX-Programme:
While-/For-Summen, `sum(range(n))`, List-Append, Dict-Insert,
String-Konkatenation, List-/Filter-Comprehension, Generator-Sum, leeres
Programm/Startup. Seit dem Abschluss-Slice sind Comprehensions, Lambdas,
Klassen und eager async/await sprachlich verfügbar; die Micro-Programme
bleiben bewusst klein und importfrei.
`run_micro.py` misst CPython vs. PyX-JIT vs. PyX-AOT (Wall-Time, optional RSS,
Build-Zeit) und schreibt `benchmarks/pyx/results/latest.json`; enthalten sind
auch Thread-Serial/Parallel-Läufe und Prozess-/Pool-Skalierung (siehe unten).
Der konkurrierende GC lässt sich mit
`PYX_GC_THRESHOLD`/`PYX_GC_STATS=1` (stderr: `pause_ms`, `mark_ms`, `freed`)
beobachten.

```bash
cargo build --release -p pyx-cli -p pyx-runtime
python3 benchmarks/pyx/run_micro.py
```

Hinweise:
- Dicts sind linear (O(n)); die Dict-Last ist bewusst klein.
- String-Konkatenation ist O(n²); die Last ist bewusst klein.
- Frühe Zahlen sind **Baselines**, keine Optimierungsziele; P5-02/P5-03 folgen.
- Threading/Queue/Prozesse sind seit Phase 2.14/2.16 nutzbar; `multiprocessing`
  unterstuetzt fork und echtes spawn (exec, Funktionsregistry) sowie `Pool`
  mit fork/spawn-Workern. Pickle nutzt ein eigenes PYX1-Format. Der Micro-
  Harness enthaelt `pool_map` (4 Worker) sowie die Skalierungs-Paare
  `pool_serial`/`pool_parallel` und `process_queue` (siehe
  „Prozess-/Pool-Skalierung“). Datei-I/O, `with`, `csv` und
  `json.dump/load` sind seit Phase 2.18 vorhanden (UTF-8-Text).
- Die User-Harnesses bleiben unverändert. Ein Adapter (P5-01a) ist noch nicht
  implementiert; Sprachfeatures/Stdlib sind weitgehend da, aber numpy/pandas/
  FastAPI (C-API) und echte Async-Infrastruktur bleiben Blocker.

## Prozess-/Pool-Skalierung (P2-20e)

Die Prozess-Workloads machen Startkosten und 1-vs-4-Worker-Skalierung
sichtbar. Sie sind PyX-native Programme unter `benchmarks/pyx/`; die
CPython/uv-Harnesses (`bench_*.py`) bleiben unverändert und werden nicht
umgebogen.

| Workload | Aufbau | Nutzen |
|---|---|---|
| `threads_serial` | 4 Blöcke à 200k Iterationen im Main-Thread | Thread-Baseline ohne Nebenläufigkeit |
| `threads_parallel` | 4 Threads, gleiche Arbeit, Lock/Flag-Sync | Thread-Queue-Baseline |
| `pool_map` | `Pool(4)`, 4 Aufgaben à 50k | Pool-Overhead bei kleiner Last |
| `pool_serial` | `Pool(1)`, 4 Aufgaben à 250k | Prozess-Overhead + serielle Arbeit |
| `pool_parallel` | `Pool(4)`, identische 4 Aufgaben | Skalierung bei 4 Workern |
| `process_queue` | 2 `Process`es, je eine `Queue`, `join` + Summe | Start-/IPC-Kosten pro Prozess |

Auswertung:

- `pool_serial` und `pool_parallel` berechnen exakt dieselben Chunks und
  drucken dieselbe Summe; `run_micro.py` prüft die Ausgabe gegen CPython.
  Der Speedup ist `median(pool_serial) / median(pool_parallel)` und enthält
  bewusst die Prozess-Startkosten (fork, Worker-Aufbau, Ergebnis-Transport).
- `process_queue` isoliert zwei Prozesse mit je einer Queue; die Summe ist
  reihenfolgeunabhängig, die Ausgabe damit deterministisch.
- `threads_*` dienen als Vergleichsbasis: CPython-Threads sind durch den GIL
  bei CPU-Arbeit praktisch seriell, während Prozesse echte Kerne nutzen.

Einschränkungen:

- Der Harness nutzt die Default-Startmethode (`fork`) und vergleicht **nicht**
  fork gegen spawn; die Zahlen sind daher keine Aussage über spawn-Startkosten.
- Gemessen wird die gesamte Wall-Time inklusive Interpreter-/JIT-Start,
  Prozess-Erzeugung und Serialisierung (PYX1). Kleine Arbeitsmengen werden
  von diesen Fixkosten dominiert.
- Concurrency-Messwerte schwanken mit der Maschinenlast; die Schwellen sind
  deshalb großzügig (Faktor 3.0) und das Gate läuft nicht in der CI.

## Regressionsschwellen (P5-01c)

`benchmarks/pyx/thresholds.json` hält für die Kern-Workloads die JIT-/AOT-
Baseline-Medians und die erlaubte Obergrenze (`max_ms`) fest.
`scripts/check_benchmarks.py` liest `benchmarks/pyx/results/latest.json`,
vergleicht nur Workloads, die in beiden Dateien vorkommen, und beendet sich bei
einer Überschreitung mit Exit-Code 1:

```bash
python3 benchmarks/pyx/run_micro.py
python3 scripts/check_benchmarks.py            # Gate
python3 scripts/check_benchmarks.py --update   # Baseline neu schreiben
```

Die Faktoren sind bewusst großzügig (Default 1.75; Threads, Pool und Prozesse
3.0; Prozess-Startup 4.0) und werden zusätzlich durch `floor_ms` abgesichert. Das
Gate läuft **nicht** in der CI, weil Timing auf geteilten Runnern zu stark
schwankt; es ist als lokales/Release-Gate gedacht. CPython-Messwerte werden
nicht gegated.


## Nächste Ausbaustufen für die vollen Harnesses

1. PyX-Adapter (`PYX_BIN`, Subprozess) für die unveränderten `bench_*.py`;
   verbleibende Blocker je Harness stehen in der Tabelle oben.
2. `numpy`/`pandas`/FastAPI brauchen C-API/ABI (P4-04, blockiert) sowie
   Decorators/echte Async-Infrastruktur.
3. Echte Coroutinen/Event-Loop und nicht-blockierendes `select`-artiges I/O
   für `bench_request_handling.py`; blockierendes TCP und eager async sind da.
4. Packaging/Cross-Platform, damit die Harnesses ohne manuellen Pfad-Setup
   laufen (P3-08/Q-09).
