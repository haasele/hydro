# PyX Benchmarks — Readiness & Integration

The existing harnesses (`bench_loops.py`, `bench_csv.py`, `bench_startup.py`,
`bench_request_handling.py`, `bench_fastapi_throughput.py`) run under
CPython/uv and compare CPython variants (numpy, pandas, FastAPI, ...).
PyX is **not** built into these files; instead there is:

- `benchmarks/PYX.md` (this file): readiness matrix and integration plan.
- `benchmarks/pyx/`: PyX-native micro-benchmarks + `run_micro.py`, which
  compares CPython, PyX JIT (`pyx run`), and PyX AOT (`pyx build`).

## Status of the existing harnesses

| Harness | Required PyX features | Status |
|---|---|---|
| `bench_loops.py` | imports (numpy, argparse, json, platform, statistics, pathlib), `reduce`/`operator.add`, comprehensions/lambdas/map/filter/builtins/strings | **partial**: language, argparse/json/platform/statistics/pathlib, reduce/operator present; **numpy** (C API) missing |
| `bench_csv.py` | `csv`/`io`/`os`/`random`/`pandas`, file I/O, `with`, f-strings, dict access | **partial**: csv/io/with/json/os/random/pathlib/argparse/defaultdict present; **pandas** (C API) missing |
| `bench_startup.py` | subprocess adapter, import system + stdlib modules; measures interpreter/imports/FastAPI start | **partial**: PyX start (JIT/AOT) and native/user modules measurable; FastAPI/C extensions missing |
| `bench_request_handling.py` | `asyncio`, `socket`, `json`, `bytes`, async/await, classes | **partial**: json/os/time/sys, bytes, threading/queue/multiprocessing, blocking socket, classes, real coroutines + event loop (`asyncio.run/sleep/gather`) present; non-blocking `select`-like I/O and `asyncio` beyond these three APIs missing |
| `bench_fastapi_throughput.py` | FastAPI/Pydantic/uvicorn/httpx, classes/decorators/typing, C extensions | **blocked**: phase 4 (C API/ABI) + decorators |

## Micro subset (runnable now)

`benchmarks/pyx/` contains predominantly import-free PyX programs:
while/for sums, `sum(range(n))`, list append, dict insert,
string concatenation, list/filter comprehension, generator sum, empty
program/startup. Since the closing slice, comprehensions, lambdas,
classes as well as real lazy generators and coroutines are available in the
language; the micro programs deliberately remain small and import-free.
`run_micro.py` measures CPython vs. PyX JIT vs. PyX AOT (wall time, optional RSS,
build time) and writes `benchmarks/pyx/results/latest.json`; included are
also thread serial/parallel runs and process/pool scaling (see below).
The concurrent GC can be observed with
`PYX_GC_THRESHOLD`/`PYX_GC_STATS=1` (stderr: `pause_ms`, `mark_ms`, `freed`);
`pyx gc [--json] <file.py>` prints the same counters in aggregate.

```bash
cargo build --release -p pyx-cli -p pyx-runtime
python3 benchmarks/pyx/run_micro.py
```

Notes:
- Dicts use a hash index with Python equality (O(1) on average;
  `pop` reindexes); the dict load is deliberately small.
- String concatenation is O(n²); the load is deliberately small.
- Early numbers are **baselines**, not optimization targets; P5-02/P5-03 follow.
- Threading/queue/processes have been usable since phase 2.14/2.16; `multiprocessing`
  supports fork and real spawn (exec, function registry) as well as `Pool`
  with fork/spawn workers. Pickle uses its own PYX1 format. The micro
  harness contains `pool_map` (4 workers) as well as the scaling pairs
  `pool_serial`/`pool_parallel` and `process_queue` (see
  "Process/Pool scaling"). File I/O, `with`, `csv`, and
  `json.dump/load` have existed since phase 2.18 (UTF-8 text).
- Environment: CPython's `multiprocessing` forkserver fails here
  (`ConnectionResetError`). `run_micro.py` logs such
  CPython reference errors as `error`, does not abort the baseline loop,
  and skips the corresponding PyX rows in the baseline gate.
- The user harnesses remain unchanged. An adapter (P5-01a) is not yet
  implemented; language features/stdlib are largely there, but numpy/pandas/
  FastAPI (C API) and non-blocking I/O remain blockers.

## `bench_pyx.py` and `bench_all.py --only pyx`

`benchmarks/bench_pyx.py` runs the same pure Python workloads under three
runners (CPython, PyX JIT via `pyx run`, PyX AOT via `pyx build` + binary):

- 11 workloads: hello, while_sum, for_range_sum, list_append, list_comp,
  dict_insert, string_concat, json_roundtrip, math_loop, generator_sum
  (lazy generator), and async_gather (real coroutines).
- Every run is a fresh process; a warmup run checks the output
  against CPython, after which `--repeat` runs are measured (median). Deviating
  or failing runs mark an error row and exit code 1.
- AOT builds are excluded from the measured runs; the PyX binary
  is found via `--pyx`, `$PYX_BIN`, `target/release/pyx`, or `target/debug/pyx`.
- `benchmarks/bench_all.py` registers the script as `pyx` and resolves its
  output directory absolutely so that child processes write into the same
  directory. Invocation: `uv run benchmarks/bench_all.py --only pyx`.

Measured speedups (release, n=200000, median): while_sum 7.7x JIT / 12.4x
AOT, for_range_sum 6.4x/8.0x, list_append 3.6x/4.5x, list_comp 3.6x/4.5x,
dict_insert 3.6x/4.5x, json_roundtrip 4.9x/6.2x, async_gather 2.5x/2.7x.
math_loop is at parity after the thread-local native-module dispatch cache
(0.94x JIT, 1.01x AOT; previously 0.81x); in the 50k micro harness it is
2.2x faster end-to-end (AOT 6.2 ms vs. CPython 13.6 ms). For the lazy
generator sum over 100k elements, CPython measures 10.2 ms, JIT 7.7 ms, and
AOT 6.8 ms; the x86_64 switch is register-only, and `strace -c` counts 94
syscalls in total instead of 200,097 (two `rt_sigprocmask` per yield).
Remaining open performance work: string concatenation (O(n²)) and float
boxing; no "solved" claims beyond the measurements above. The micro harness
(`run_micro.py`) shows with the same release build, among others, while_sum
AOT 1.7 ms vs. CPython 23.5 ms and for_sum AOT 1.4 ms vs. 19.3 ms.

## Process/Pool scaling (P2-20e)

The process workloads make startup costs and 1-vs-4-worker scaling
visible. They are PyX-native programs under `benchmarks/pyx/`; the
CPython/uv harnesses (`bench_*.py`) remain unchanged and are not
bent.

| Workload | Setup | Purpose |
|---|---|---|
| `threads_serial` | 4 blocks of 200k iterations in the main thread | thread baseline without concurrency |
| `threads_parallel` | 4 threads, same work, lock/flag sync | thread queue baseline |
| `pool_map` | `Pool(4)`, 4 tasks of 50k | pool overhead at small load |
| `pool_serial` | `Pool(1)`, 4 tasks of 250k | process overhead + serial work |
| `pool_parallel` | `Pool(4)`, identical 4 tasks | scaling with 4 workers |
| `process_queue` | 2 `Process`es, one `Queue` each, `join` + sum | startup/IPC cost per process |

Evaluation:

- `pool_serial` and `pool_parallel` compute exactly the same chunks and
  print the same sum; `run_micro.py` checks the output against CPython.
  The speedup is `median(pool_serial) / median(pool_parallel)` and deliberately
  includes the process startup costs (fork, worker setup, result transport).
- `process_queue` isolates two processes with one queue each; the sum is
  order-independent, making the output deterministic.
- `threads_*` serve as a comparison basis: CPython threads are effectively
  serial for CPU work due to the GIL, while processes use real cores.

Limitations:

- The harness uses the default start method (`fork`) and does **not** compare
  fork against spawn; the numbers are therefore not a statement about spawn startup costs.
- What is measured is the total wall time including interpreter/JIT start,
  process creation, and serialization (PYX1). Small work amounts are
  dominated by these fixed costs.
- Concurrency measurements fluctuate with machine load; the thresholds are
  therefore generous (factor 3.0) and the gate does not run in CI.

## Regression thresholds (P5-01c)

`benchmarks/pyx/thresholds.json` records the JIT/AOT
baseline medians and the permitted upper bound (`max_ms`) for the core workloads.
`scripts/check_benchmarks.py` reads `benchmarks/pyx/results/latest.json`,
compares only workloads that appear in both files, and exits with
exit code 1 when a limit is exceeded:

```bash
python3 benchmarks/pyx/run_micro.py
python3 scripts/check_benchmarks.py            # gate
python3 scripts/check_benchmarks.py --update   # rewrite baseline
```

The factors are deliberately generous (default 1.75; threads, pool, and processes
3.0; process startup 4.0) and are additionally backed by `floor_ms`. The
gate does **not** run in CI, because timing fluctuates too much on shared
runners; it is intended as a local/release gate. CPython measurements are
not gated.


## Next expansion stages for the full harnesses

1. PyX adapter (`PYX_BIN`, subprocess) for the unchanged `bench_*.py`;
   remaining blockers per harness are listed in the table above.
2. `numpy`/`pandas`/FastAPI need the C API/ABI (P4-04 decision B: PyX-native
   ABI shim) and decorators; real async infrastructure is present for pure
   coroutines. Ecosystem suites/benchmarks are P4-10/P4-11.
3. Non-blocking `select`-like I/O and `asyncio` beyond
   `run`/`sleep`/`gather` for `bench_request_handling.py`;
   blocking TCP, real coroutines, and the event loop are there.
4. Packaging/cross-platform so that the harnesses run without manual path
   setup (P3-08/Q-09).
