# AGENTS.md — PyX Work Status

> **Resume rule:** Before every continuation, read this file + `docs/TODO.md`.
> After every verified phase step, update the status here.
> A step only counts as done when the command/scenario was actually
> executed; tick off ToDos in `docs/TODO.md` in parallel.

## Project

PyX: standalone Python runtime (Rust) with AOT compiler and toolchain,
conceptually Bun for Python. Source of truth: `PLAN.md` (strategy),
`docs/ARCHITECTURE.md` (contracts), `docs/TODO.md` (work list),
`docs/C_API.md` (P4-04 spike) and `docs/LAYOUT.md` (source tree).

## Current State

| Phase | Status | Verification |
|---|---|---|
| P0 Foundation | largely done | CI/README/license/changelog present; Hyperfine harness open |
| P1 Minimal compiler | **done & verified** | `scripts/verify.sh` ALL GREEN |
| P2 Runtime | **largely done; exceptions (hierarchy/tuples/raise-from), stdlib Batches A/B-native/benchmark/module values/globals, shared heap + concurrent GC + threads/processes/pool + file I/O/bytes/classes/closures/user modules/packages/site-packages + lazy generators/coroutines/event loop (no GIL)** | 1371 tests, JIT+AOT |
| P3 Toolchain | **core present (new/init/test/fmt/lint/clean, build cache, uv passthrough)** | E2E, cache tests |
| P4 Compatibility | **target CPython 3.14.9; P4-04a/b complete for the defined subset; official CPython test gate + import matrix established; full ABI/stdlib/ecosystem open (P4-04c…P4-12)** | `docs/C_API.md`, `docs/CPYTHON_TESTS.md`, `docs/COMPATIBILITY.md` |
| P5 Performance | **baseline + local regression gate** | `benchmarks/pyx/results/latest.md`, `thresholds.json` |
| P6 Polish | **largely done; diagnostics/JSON/doctor/CLI** | codes, snippets, help, ICE |

**Compatibility target:** **CPython 3.14.9** (single definition in
`pyx_ir::compat`; 3.13 is security-only, 3.15 is newer than the oracle).
Official `Lib/test` runs are a release gate (`docs/CPYTHON_TESTS.md`), and the
offline manifest check runs in `scripts/verify.sh`/CI. Future LTS lines are
keyed to CPython minors (P4-12, policy only).

**Current focus:** Phase-2 completion and documentation. Most recently
implemented were real **lazy generators** (stackful fibers), real **coroutines
with a cooperative event loop** (`async def`, `await`, `asyncio.run/sleep/gather`),
the builtins `iter()`/`next()`, `pyx gc [--json]`, the dict hash index,
typed int fastpaths, constant folding and the `bench_pyx.py` integration.
Native module dispatch now uses a thread-local spec cache (math_loop at
parity/faster; 2.2× end-to-end in the 50k micro harness). P4-04 option B is
implemented at the first abi3 slice: an extension `.so` next to the source can
be imported and called under JIT and AOT without `libpython`; the shim covers
the limited-API fixture surface and the loader discovers sibling `.so` files.
Open: capsules/buffer/container constructors, wheels/venvs, the full CPython
3.14 ABI for NumPy/pydantic-core, and the stdlib/ecosystem program P4-09…P4-11.
Still open are async generators/comprehensions, variable payload size classes as
well as precise GC stack maps (P2-05/06/07), packaging/cross-platform/fuzzing
and the C-API implementation itself. Details: `docs/TODO.md`.

**Standard-library / ecosystem checkpoint (2026-10-25):**

- **Package loader:** `check_file` resolves packages (`__init__.py`), dotted
  names, submodules and relative imports. Search order: sibling files, the
  package-chain root, `PYX_SITE_PACKAGES`, the `VIRTUAL_ENV`/
  `CONDA_PREFIX` site-packages trees, then `PYTHONPATH`. Every unit loads
  once per canonical path; imported module bodies run once as initializers.
  Nested imports are recorded; declaration-only site-packages modules that
  are only referenced from nested blocks are preloaded after the graph is
  complete (the entry module is moved back to the end), while executable
  optional modules stay runtime stubs.
- **`rich` progress:** the native `rich` implementation was removed. The
  official pure-Python `rich` distribution is resolved through the loader.
  All console/dataclass/typing compile blockers and the first runtime
  surfaces are implemented: bound native/user methods, dynamic class and
  function keyword calls (`Class(**kwargs)`, `f(*args, **kwargs)`-shaped
  constructors), runtime-evaluated defaults, zero-argument `super().__init__(...)`
  dispatch to the direct base initializer, user `__getitem__`, native
  `threading.Thread` subclass bases, `del`, loop `else`, typing aliases,
  `html.escape/unescape`, JSON keyword compatibility, `str.format` named
  fields, `dict.setdefault`, regex `Match` bound methods, and the rich
  console/table/panel/rule rendering path. A native `rich.status.Status`
  context-manager shim covers the report progress API. The default
  conservative GC still loses live values during heavier rich rendering;
  with `PYX_GC_THRESHOLD` raised the report renders.
- **Import matrix:** **60/297 modules importable** (13 builtin/frozen,
  43 pure, 4 extension; 228 `PYX0002`, 9 modules where CPython 3.14.9
  itself fails). Report: `docs/COMPATIBILITY.md`, JSON
  `benchmarks/compat/latest.json`.
- **`benchmarks/bench_all.py` under PyX: green with a raised GC threshold.**
  `PYX_GC_THRESHOLD=100000000 PYX_SITE_PACKAGES=/usr/lib/python3.14/site-packages
  target/release/pyx run benchmarks/bench_all.py` exits 0 for the full
  suite (startup, CSV, request handling, FastAPI throughput and the PyX
  comparison), builds the AOT binaries and writes `report.{md,html,txt}` +
  `results.json` (155 s). The default GC threshold still loses live `Segment`
  values during the final report rendering (`conservative-root retention
  issue`, P2-06).

**Verification (as of 2026-10-25):**

- `cargo test --workspace`: **1371 tests** — 413 frontend, 48 codegen, 18 IR,
  492 runtime unit, 221 CLI/E2E (143 E2E incl. the abi3 extension fixture,
  24 fibers, 6 closures, 11 user modules, 10 exception tests, 8
  completions, extension path/dotted/multi-phase, 2 unittest-runner tests,
  2 stdlib-batch-a, 2 stdlib-batch-b, 3 stdlib-bench-batch tests and 5 CLI
  argument-parser unit tests), 179 further runtime integration tests
  (allocator, bytes, GC/concurrent GC/STW GC, IO, CSV, stdlib modules,
  stdlib utils, classes, closures, socket, pathlib, pool, threads, attrs,
  format). New suites cover generator send/throw/close + yield expressions,
  C-API containers, extension value/loader conversion, extension import
  lowering, list slice assignment, starred unpacking/calls, deferred
  imports, exception classes as values, the package/site-packages loader and
  the shared `rich` surface.
- `scripts/verify.sh`: fmt, clippy (`-D warnings`), build, tests, E2E for JIT
  and AOT (incl. classes/closures/generators_async/formatting/socket_echo/
  stdlib_modules/pathlib), negative tests, GC threshold/stress — ALL GREEN.
- C-API: the environment is unblocked (Python 3.14.8 with `Python.h`,
  `libpython3.14.so`, `python3-config --embed`; NumPy 2.5.4 and pydantic
  2.14.1 installed). P4-04 decision: **option B** (no `libpython` in the
  production runtime). Probes (`spikes/p4_04/`): embedded CPython init
  6.67 ms + finalize 7.46 ms, `import numpy` 41.64 ms, `import pydantic`
  11.96 ms; a stable-ABI fixture imports with four undefined CPython symbols.
  Full stdlib parity is 297 modules (174 pure Python, 71 extension,
  52 builtin/frozen); program in P4-09…P4-11.
- Native module dispatch cache: `math_loop` AOT 22.50 ms vs. CPython 22.65 ms
  (n=200000; 0.94× JIT, 1.01× AOT), 50k micro harness AOT 6.2 ms vs. 13.6 ms.
- **C-API slice verified:** the limited-API fixture `pyx_capi_probe` builds
  against CPython 3.14 headers (no libpython link), is discovered next to the
  source, and runs under both `pyx run` and an AOT binary (scalars, strings,
  bytes, floats, lists, tuples and dicts round-trip). `ldd` shows no
  `libpython`. Error paths: arity → `TypeError`, missing attribute →
  `AttributeError`, unsupported argument type → `TypeError`.
- **Generator methods + yield expressions verified** against CPython:
  `next`/`send` echo, `close` running `finally`, `throw(caught_exception)`
  re-raised inside the generator, StopIteration after completion.
- **Stdlib compatibility baseline:** `python3 scripts/stdlib_compat.py`
  (CPython 3.14.9, PyX 0.1.0): **60/297 modules importable** (13 builtin/frozen,
  43 pure, 4 extension; 9 modules fail under CPython too). Batches A,
  the native core of B, the benchmark-critical batch and the new native
  `typing`/`abc`/`enum`/`types`/`itertools`/`ast`/`fractions`/`importlib`/
  `__future__` modules are importable and API-tested; `dataclasses` and
  `datetime` are importable through native module specs. Report:
  `docs/COMPATIBILITY.md`, JSON `benchmarks/compat/latest.json`.
- **Official CPython suite baseline:** `python3 scripts/cpython_compat.py`
  (398 `Lib/test` modules, pinned 3.14.9 oracle): **1 passing**
  (`test_errno.py`), 334 `pyx_fail`, 63 `expected_fail`, 0 timeouts. The
  manifest records the real pass and blocks regressions; `--check-manifest`
  runs offline in `scripts/verify.sh`/CI. Remaining failures are mostly
  missing stdlib imports/packages (`test.support`, `unittest` features).

**Phase-2.0 slice (containers, 2026-10-23) — done & verified:**

- `list`/`dict` literals, indexing read/write (negative indices),
  `IndexError`/`KeyError`, `len`, structural equality, `list + list`,
  Python representation (`[1, 'a']`, `{'k': 1}`), truthiness.
- Iteration ABI (`pyx_iter_new`/`pyx_iter_next`), `for` over lists/strings
  (Unicode scalars) and dict keys in insertion order; `break`/`continue`,
  nested loops.
- New exception tags 6–8 (IndexError, KeyError, StopIteration) in IR/runtime.
- `cargo test --workspace`: 79 tests (54 runtime, 7 frontend, 2 IR, 16 E2E) green.
- `scripts/verify.sh`: additionally `collections.py`, `index_error.py` (exit 1,
  traceback), AOT collections identical to JIT — ALL GREEN.
- Differential test against CPython for container repr, negative indices,
  dict overwrite/order, bool/int key equivalence, Unicode indexing,
  `and`/`or` operands, structural equality, error messages: identical.
- Static diagnostics: non-iterable value, wrong index type,
  unhashable dict literal key, slices.

**Phase-2.1 slice (GC interim, 2026-10-23) — done & verified:**

- Conservative mark-&-sweep in `pyx-runtime/src/heap.rs`: thread-local
  registry, mark bit, roots = native stack bounds via `pthread_getattr_np` +
  asm stack loads, all GPRs as register dump, pending exception, explicit
  root for the freshly allocated object; cycles are collected.
- Threshold `PYX_GC_THRESHOLD` (default 10000), `PYX_GC_STRESS=1` collects on
  every allocation; `pyx_gc_collect()`/`pyx_gc_objects()` as ABI;
  `pyx_runtime_init()` resets heap + exception state.
- `cargo test --workspace`: 89 tests (55 runtime unit, 5 GC integration,
  7 frontend, 2 IR, 20 E2E) green; `scripts/verify.sh` incl. GC threshold/
  stress for JIT and AOT as well as container roots — ALL GREEN.
- Stress test (400k temporary strings): peak RSS 15.5 MB with GC vs. 154 MB
  without collection; output identical. Stress mode (`PYX_GC_STRESS=1`)
  correct for all examples.
- Limit: conservative (false-positive retention, uninit-stack caveat on
  non-x86/aarch64); precise shadow-stack roots + concurrent collector
  remain P2-06/P2-07. A dedicated size-class allocator remains P2-05.

**Phase-2.2 slice (`try/else/finally`, 2026-10-23) — done & verified:**

- IR: `Try { body, handlers, else_body, finally_body }`, `Handler.exc: Option`
  (bare `except:`).
- Codegen: cleanup stack with action codes (normal/return/break/continue/
  exception), parked exception for `finally`, correct suppression on
  return/break/continue in the finally, bare `raise` re-raises the in-flight
  exception (not the handled error).
- `return` in try/handlers runs through finally and no longer executes code
  after the try (old bug fixed).
- Verified: examples/finally.py + finally_reraise.py + return_after_try.py
  identical to CPython; 100 tests (25 E2E, 13 frontend, 2 IR, 55 runtime,
  5 GC); AOT + GC stress; `scripts/verify.sh` ALL GREEN.

**Phase-2.3 slice (slices + f-strings, 2026-10-23) — done & verified:**

- Read-only slices for `str`/`list` with full `slice.indices` semantics
  (negative bounds, defaults, Unicode scalars, `step == 0` -> ValueError);
  runtime ABI `pyx_slice`, IR `ExprKind::Slice`.
- Simple f-strings (`{expr}`, `{expr!s}`, escaped braces) are lowered to
  `str(expr)` concatenation; format specs/`!r`/`!a` produce clear
  diagnostics.
- Slice assignment and dict slices are rejected statically.
- Verified: 124 tests (32 E2E, 27 frontend, 2 IR, 58 runtime, 5 GC),
  CPython differential for slice/f-string edge cases, GC threshold, AOT for
  slices/fstrings; `scripts/verify.sh` ALL GREEN.

**Phase-2.4 slice (methods, 2026-10-23) — done & verified:**

- IR `ExprKind::MethodCall`, ABI `pyx_method` (dynamic dispatch), new
  `AttributeError` tag (9).
- `str`: upper/lower/strip/lstrip/rstrip/split/join/replace/startswith/
  endswith/find/count/isdigit/isalpha/isalnum/isspace.
- `list`: append/extend/insert/remove/pop/clear/reverse/copy/count/index.
- `dict`: get/keys/values/pop/update/clear (keys/values return lists, not
  views — documented deviation until tuples/views exist).
- Static method matrix (arity/arguments/result types), `Any` annotation,
  dynamic `Any` receivers with runtime checks; unknown methods/
  attribute access without a call are diagnostics.
- Verified: 158 tests (38 E2E, 40 frontend, 2 IR, 73 runtime, 5 GC),
  CPython differential for method edge cases, GC threshold, AOT methods;
  `scripts/verify.sh` ALL GREEN. Arity error texts are Python-like, but
  not always byte-identical to CPython.

**Phase-2.5 slice (tuples, 2026-10-23) — done & verified:**

- `Type::Tuple`, `ExprKind::Tuple`, `StmtKind::Unpack`, ABI `pyx_tuple_new`
  and `pyx_unpack`; runtime object `Tuple(6)` incl. GC tracing.
- Indexing/slicing/`len`/iteration/repr (`(1,)`, `()`), structural
  equality, `tuple + tuple`, `tuple * int`, methods `count`/`index`.
- Tuples of hashable elements are dict keys; recursive hashability check
  reports the inner unhashable element. `dict.items()` returns a list of
  `(key, value)` tuples (view deviation documented).
- Unpacking `a, b = pair()` and `for k, v in d.items()` (frontend desugars
  ForEach + Unpack); length errors as ValueError, non-list/tuple as
  TypeError. Nested/starred unpacking rejected statically.
- Verified: 198 tests (44 E2E, 60 frontend, 2 IR, 86 runtime, 6 GC),
  CPython differential incl. repr quote choice, GC threshold, AOT tuples;
  `scripts/verify.sh` ALL GREEN.

**Phase-2.6 slice (set + membership, 2026-10-23) — done & verified:**

- `Type::Set`, `ExprKind::Set`, `BinOp::In`/`NotIn` (codes 13/14),
  `Builtin::Set`; runtime `Set(7)` with ABI `pyx_set_new`/`pyx_set_from`.
- Set literals deduplicate (insertion order), `len`/iteration/truthiness/
  structural equality/`repr` (`{1, 2}`, empty `set()`), `set - set`.
  Hashability recursive (tuples), unhashable elements -> TypeError.
- Methods: `add`, `remove`, `discard`, `clear`, `copy`, `union`.
- Membership for list/tuple/dict/str/set; `not in`; Python error messages for
  wrong operands; `in` on non-iterable types rejected statically.
- Fix: `len(x)` with a not-yet-resolved variable wrongly defaulted to
  `str`; now the constraint stays open (`Any`), `len(1)` remains an error.
- Verified: 231 tests (51 E2E, 72 frontend, 2 IR, 100 runtime, 6 GC),
  CPython differential for set/membership edge cases, GC threshold, AOT sets;
  `scripts/verify.sh` ALL GREEN.

**P6.0 slice (first-class diagnostics, 2026-10-23) — done & verified:**

- Diagnostics carry stable codes (`PYX0001` syntax, `PYX0102` type,
  `PYX0101` name, `PYX0201` methods, `PYX0202` index, ...), spans
  (`end_col`), optional help hints (did-you-mean for names/methods) and
  are rendered by the CLI as Cargo-like snippets:
  `error[CODE]`, `--> file:line:col`, gutter, caret, `= help:`.
- `--json` (stderr, stable schema file/line/col/end_col/code/message/help),
  `--color=auto|always|never`/`--no-color` (TTY + NO_COLOR), summary line,
  consistent exit codes; `check` stays silent on success.
- `pyx doctor [--json]` checks version/target/runtime lib/CC/workspace;
  exit 0 when a C compiler is present (JIT without staticlib).
- ICE hygiene: `catch_unwind` around dispatch, banner with version/target/
  report hint, exit 2.
- Verified: 256 tests (60 E2E, 88 frontend, 2 IR, 100 runtime, 6 GC),
  human/JSON/color output, doctor, snippets; `scripts/verify.sh` ALL GREEN.
- Open (P6): `new`/`init`/`test`/`fmt`/`lint`/`clean`, cache (P6-04),
  ICE regression test, sema caret spans, CLI reference/diagnostic catalog.

**Phase-2.7 slice (range + benchmark builtins, 2026-10-23) — done & verified:**

- `range(...)` is a value (range object, iteration/len/membership/repr);
  builtins `list`, `dict`, `sum`, `enumerate`, `zip` added.
- `enumerate`/`zip` materialize lists of tuples (benchmark-suitable);
  `dict(zip(...))`, `list(range(...))`, `sum(range(...))` work.
- Range membership accepts arbitrary left types like CPython (False for
  non-integral/other types); range is not subscriptable (documented).
- Verified: 290 tests (67 E2E, 105 frontend, 2 IR, 110 runtime, 6 GC),
  CPython differential for range/builtin edge cases, GC threshold, AOT builtins;
  `scripts/verify.sh` ALL GREEN.
- Benchmark readiness analyzed; `benchmarks/PYX.md` + PyX micro harness
  (`benchmarks/pyx/run_micro.py`) + first baseline
  (`benchmarks/pyx/results/latest.{json,md}`); existing harnesses remain
  unchanged.
- Baseline (median, 5 runs, release): startup hello CPython 20.4 ms /
  PyX JIT 2.9 ms / PyX AOT 1.0 ms; while_sum 47.5 / 10.6 / 8.0 ms;
  for_sum 38.1 / 13.9 / 11.0 ms; range_sum 23.3 / 7.4 / 7.0 ms;
  list_append 38.1 / 16.3 / 13.5 ms; dict_insert 21.8 / 79.6 / 52.4 ms
  (linear at the time; superseded since the dict hash index); string_concat 23.9 / 13.2 / 12.4 ms (O(n²)).
  Early baseline, no optimization targets.

**Phase-2.8 slice (comprehensions, 2026-10-23) — done & verified:**

- List/set/dict comprehensions and generator expressions; multiple
  `for` clauses, `if` filters, nested comprehensions, tuple targets
  (unpack), outer capture and shadowing without name leak.
- Generator expressions are materialized eagerly as a list (documented
  deviation until real generator objects exist).
- Runtime helpers `pyx_list_append`/`pyx_set_add`/`pyx_dict_set`; codegen builds
  result/iterator roots GC-safely.
- Verified: 307 tests (71 E2E, 115 frontend, 2 IR, 113 runtime, 6 GC),
  CPython differential (order/scope/unpack/nesting), GC threshold, AOT
  comprehensions; `scripts/verify.sh` ALL GREEN.
- Benchmark baseline extended: list_comp 27.8/7.4/5.9 ms, filter_comp
  25.2/9.7/7.5 ms, sum_genexp 27.5/9.8/6.7 ms (CPython/JIT/AOT).

**Phase-2.9 slice (first-class functions, 2026-10-23) — done & verified:**

- Named functions and lambdas are values (`FunctionRef`); indirect calls
  run via `pyx_call`/argv wrappers (JIT + AOT). Lambdas are parameter-only,
  without closures, defaults or star parameters (diagnostics).
- `map`/`filter` materialize lists; `filter(None, x)` uses truthiness;
  callback errors/arity are propagated.
- Function objects are normal heap values (lists/dicts/iterators), GC-traced
  without child values; `pyx_function_new`/`pyx_call`/`pyx_map`/`pyx_filter`.
- Verified: 333 tests (79 E2E, 127 frontend, 2 IR, 119 runtime, 6 GC),
  CPython differential (lambdas/indirect calls/map/filter), GC threshold, AOT
  functions; `scripts/verify.sh` ALL GREEN.
- Benchmark baseline: map_lambda 30.7/13.0/13.4 ms, filter_lambda 28.6/13.9/
  12.9 ms, lambda_call 32.7/15.4/14.4 ms (CPython/JIT/AOT).

**Phase-2.10 slice (modules/imports, 2026-10-23) — done & verified:**

- Central stdlib specification `pyx_ir::stdlib` (math, functools, operator,
  time, sys) for frontend types and runtime validation.
- `import X [as Y]`, `from X import a [as b]`; imports globally visible (also in
  functions), only at module level; module attributes/calls (`math.sqrt`,
  `time.perf_counter`, `sys.argv`) and native function values.
- Runtime: math (trig/log/pow/floor/ceil/trunc/ constants),
  functools.reduce, operator (binops/comparisons/neg/not/truth), time
  (time/perf_counter/monotonic/sleep/ns), sys (argv/platform/version/
  version_info/maxsize).
- Verified: 370 tests (87 E2E, 145 frontend, 2 IR, 130 runtime, 6 GC),
  CPython differential (modules/reduce/math/time/sys), GC threshold, AOT
  modules; `scripts/verify.sh` ALL GREEN.
- Benchmark baseline: reduce_add 27.0/7.2/5.7 ms, math_loop 28.8/22.4/25.8 ms
  (CPython/JIT/AOT; AOT PIC somewhat slower on module dispatch).

**Phase-2.11 slice (json + os, 2026-10-23) — done & verified:**

- `json.dumps`/`json.loads` with CPython defaults (ensure_ascii, separators,
  escapes, surrogates, dict keys, error classes, depth/cycle guards); parser
  builds containers incrementally and GC-safely.
- `os` basics: getpid/getppid/cpu_count/getcwd/getenv, name/sep.
- Verified: 386 tests (91 E2E, 145 frontend, 2 IR, 141 runtime, 6 GC,
  1 JSON-GC), CPython differential (JSON edge cases/os), GC threshold, AOT
  json_os; `scripts/verify.sh` ALL GREEN.
- Benchmark baseline: json_dumps 29.0/5.6/2.7 ms, json_loads 26.8/4.9/2.7 ms
  (CPython/JIT/AOT).

**Concurrency roadmap (P2-20, adopted):** Note that heap/GC are currently
**thread-local**. Real shared-memory threading first needs
shared-heap/GC roots + synchronization or a deliberate GIL; processes
(`multiprocessing`, fork/spawn, queues/pickling) are the safe first step.
P2-20a–f cover design, threading, multiprocessing, serialization,
parallel benchmarks and race/stress tests.

**Phase-2.12 slice (shared heap + `_thread`, 2026-10-24) — done & verified:**

- **No GIL**: global shared-heap mode with migration on the first thread,
  thread registry, `_thread.start_new_thread`/`get_ident`/`allocate_lock` and
  `_thread.join` (PyX extension); native lock objects with
  acquire/release/locked.
- While >1 thread is active, the conservative GC is suspended (no foreign
  stack scan); active again after join. An STW/Go-like collector
  remains P2-20a. Shared mutating containers need user locks.
- JIT **and** AOT run with real threads; migration before spawn makes
  existing objects visible.
- Verified: 392 tests (93 E2E, 145 frontend, 2 IR, 141 runtime, 6 GC,
  1 JSON-GC, 4 thread integration), incl. 4×3000 string stress under lock,
  shared-heap GC deferral, JIT/AOT; `scripts/verify.sh` ALL GREEN.
- Thread baseline: serial 130.2/32.9/32.1 ms, parallel 4×200k 73.0/23.3/17.8 ms
  (CPython/JIT/AOT). PyX AOT parallel ~1.8× faster than PyX serial.

**Phase-2.13 slice (STW/safepoint GC, 2026-10-24) — done & verified:**

- Cooperative stop-the-world for the shared heap: ThreadGc registration
  (stack bounds, SP/registers, parked/exited, pending mirror, thread roots),
  `pyx_safepoint()` at function entries and loop backedges, parking on
  blocking locks/join/sleep.
- Collector sets a request, waits for all other parked/exited threads,
  scans their stored stacks/registers + roots + pending, sweeps; heap lock
  only after parking, GC_LOCK via try_lock. No GIL.
- Allocation calls the safepoint, checks policy and collects BEFORE the
  allocation; spawn arguments are registered as thread roots (rooting gap
  fixed).
- Verified: 398 tests (95 E2E, 145 frontend, 2 IR, 141 runtime, 6 GC,
  1 JSON-GC, 3 STW-GC, 5 thread integration), 4×500 string stress under
  threshold=1, GC during threads/sleep/join, JIT+AOT, gate green.
- Safepoint overhead: threads_serial AOT 39.6 ms vs. 32.1 ms without safepoints;
  threads_parallel AOT 14.6 ms. A Go-like concurrent collector (write
  barriers) remains P2-20a; `threading` classes/P2-20b and `multiprocessing`/
  P2-20c open.

**Phase-2.14 slice (threading/queue/multiprocessing/pickle, 2026-10-24) — done & verified:**

- Native objects: `threading.Thread` (start/join/is_alive), `Lock`, `RLock`
  (recursive, owner/depth), `Event` (set/clear/is_set/wait(timeout)),
  `queue.Queue` (put/get incl. blocking/timeout/nowait/qsize/empty/full),
  `multiprocessing.Process` (fork/start/join/is_alive/exitcode),
  `multiprocessing.Queue` (socketpair, 4-byte length frame), `os.fork`,
  `os.waitpid`, `os._exit`.
- Serialization `pickle.dumps/loads` as its own PYX1 hex format (None/bool/
  int/float/str/list/tuple/dict/set/range/function wrapper); cycle-free,
  depth 128, not CPython-compatible. `multiprocessing.Queue` uses the same
  binary format. Functions are reconstructed via wrapper address (only
  same or forked address space).
- Native constructors accept keywords (`Thread(target=..., args=...)`)
  via `FunctionSpec.params`; missing slots are passed as default `None`.
  User functions/methods/builtins remain positional.
- Fork reset in the child: thread registry/GC/join handles, colors, worklist;
  documented limit: fork only from a stable state (locks of vanished
  threads otherwise stay held). Real `spawn` and `Pool` follow in Phase 2.16.
- Verified: 422 tests (101 E2E, 149 frontend, 2 IR, 144 runtime unit,
  3 concurrent GC, 6 GC, 1 JSON-GC, 8 native concurrency, 3 STW-GC,
  5 thread integration), CPython cross-checks for threading/queue/fork,
  pickle own format, JIT+AOT; `scripts/verify.sh` ALL GREEN.

**Phase-2.15 slice (concurrent mark-&-sweep, 2026-10-24) — done & verified:**

- `PyxObj.marked` is an atomic color (WHITE/GREY/BLACK). The collector has
  two short STW windows (root snapshot at the start, root rescan at the end;
  the sweep was made concurrent in Phase 2.17); in between it marks
  concurrently while mutators run.
- **Insertion barriers:** every newly inserted edge (list append/extend/
  insert/subscript_set, dict set/update/subscript, set add, queue put,
  builder/JSON/pickle decode) marks the new child if the parent is already
  BLACK. No parent re-trace anymore; that was previously O(n^2) for large
  lists.
- **Graph lock:** container mutations and markers serialize per payload;
  in single-threaded/local mode the lock is skipped entirely.
  Queue/event/lock mutexes remain unchanged; blocking operations
  park only while waiting, never during a graph mutation.
- **Growth threshold:** collection at `live_nach_letzter_Sammlung +
  PYX_GC_THRESHOLD` instead of an absolute object count; prevents a full mark
  per allocation with a large live set. `PYX_GC_STRESS=1` remains every
  allocation.
- Safepoint fastpath: relaxed load, recheck under registry lock; loop
  safepoints poll only every 64th iteration (function entry/AFK/allocation
  still immediate). `PYX_GC_STATS=1` writes
  `pyx-gc: pause_ms=… mark_ms=… sweep_ms=… freed=…` to stderr.
- Measurement (debug, 110k list, 3 mutators, threshold 20k): marking
  17–44 ms per collection instead of up to 8.5 s with parent re-trace; STW pauses
  36–86 ms (mostly final sweep/stack rescan). Release baseline after
  polling: threads_serial AOT 37.5 ms, threads_parallel AOT 11.5 ms;
  list_append AOT 16.5 ms, dict_insert 51.2 ms, list_comp 6.5 ms.
- Verified: `tests/concurrent_gc.rs` (3 tests, incl. 111k append bound and
  threshold=1 stress), all existing GC/thread/E2E/AOT paths,
  422 tests, `scripts/verify.sh` ALL GREEN.

**Phase-2.16 slice (real spawn + pool, 2026-10-24) — done & verified:**

- **Spawn:** `multiprocessing.set_start_method("spawn")` starts a fresh
  process via `exec` (JIT: `pyx __spawn <canonical source>`, AOT: the binary
  again). `pyx_main` registers all functions (name, wrapper, arity) before
  the module body and calls `pyx_spawn_child()`; with payload `-1` the
  normal body runs, otherwise only the target function. **No module body in
  the child.**
- Payload `[name, args]` in a 0600 temp file (env `PYX_SPAWN_PAYLOAD`), PYX1
  spawn serialization: functions by name/arity, `multiprocessing.Queue`
  via inherited FDs (CLOEXEC cleared); native objects/lambdas -> error.
  Nested spawn, function-by-name arguments and queue transport JIT+AOT green.
- **Pool:** `multiprocessing.Pool([processes])`, `map`/`apply`/`close`/`join`;
  each worker has private task/result queues (no frame interleaving), one task
  in flight. Tasks/results always spawn-serialized (function by name) so that
  fork and spawn workers behave identically. Worker exception -> RuntimeError;
  liveness probe against dead workers; process count validated/limited.
- Native worker entry `__pyx_pool_worker` in the registry; fork workers via
  `fork`+runtime reset, spawn workers via payload/exec.
- Verified: **436 tests** (105 E2E incl. spawn/pool/pool_spawn JIT+AOT and
  GC stress, 149 frontend, 2 IR, 151 runtime unit, 3 concurrent GC, 6 GC,
  1 JSON-GC, 8 native concurrency, 3 STW-GC, 3 pool, 5 thread integration),
  gate ALL GREEN. Limits: function registry only by name (duplicates/
  lambdas), queue maxsize lost in the spawn payload, pool without context
  manager, process/pool scaling benchmarks open.

**Phase-2.17 slice (concurrent sweep + terminate, 2026-10-24) — done & verified:**

- **Concurrent sweep:** After the final STW root rescan, mutators are
  released; white partition and address rebuild run once under the
  heap lock, freeing the boxes (`Box::from_raw`) **outside** all
  locks. New allocations during the sweep are GREY and are protected;
  phase E sets all surviving colors to WHITE and `MARKING=false`.
  `pyx-gc` now reports `pause_ms`, `mark_ms`, `sweep_ms`, `freed` separately.
- **Terminate:** `Process.terminate()` (SIGTERM, idempotent, join/exitcode -15)
  and `Pool.terminate()` (worker SIGTERM + reap, idempotent, no queue write).
- **Pool benchmark:** `benchmarks/pyx/pool_map.py` (4 workers, 4×50k sums)
  added to the micro harness. Release: CPython 59.2 ms, PyX JIT 9.9 ms,
  PyX AOT 6.5 ms.
- Measurement 110k list/3 mutators/threshold 20k: STW pause max. 42 ms
  (previously 86 ms), marking 23–47 ms, sweep 35–85 ms outside the pause.
- Verified: 440 tests (105 E2E, 149 frontend, 2 IR, 151 runtime unit,
  5 concurrent GC, 6 GC, 1 JSON-GC, 9 native concurrency, 4 pool, 3 STW-GC,
  5 thread integration), gate ALL GREEN.

**Phase-2.18 slice (file I/O, `with`, CSV, JSON files, 2026-10-24) — done & verified:**

- **File object:** `io.open`/`open` (keyword slots file/mode/encoding/newline/
  errors), UTF-8 text modes `r/w/a/x`; `read([n])`, `readline`, `readlines`,
  `write`, `writelines`, `flush`, `tell`, `seek`, `closed`, `close`;
  iteration over lines; errors as `OSError` (new tag 10), closed
  files as `ValueError`. `with` desugars to nested
  `try/finally` with `close()` in reverse order (only
  close protocol, no `__enter__`/`__exit__`; `as` only a simple name).
- **CSV:** `csv.reader`/`writer`, `DictReader`/`DictWriter` (fieldnames),
  RFC-4180-like quoting/embedded newlines, `writerow(s)`, `writeheader`,
  `
`; reader materializes at the constructor, DictReader missing fields
  `""`, extra columns ignored. No dialect/restkey.
- **JSON files:** `json.dump(obj,file)`/`json.load(file)` use the
  existing dumps/loads; errors/Unicode as before.
- **os:** `os.remove`, `os.rename` (OSError), example cleanup.
- Verified: **478 tests** (107 E2E incl. file/CSV/JSON JIT+AOT,
  158 frontend, 2 IR, 152 runtime unit, 5 concurrent GC, 8 CSV, 6 GC,
  12 IO, 1 JSON-GC, 6 JSON-IO, 9 native concurrency, 4 pool, 3 STW-GC,
  5 thread integration), CPython differential for files/csv_json, gate ALL
  GREEN. Limits: no `bytes`/binary mode, `newline`/`errors` ignored,
  CSV entirely in memory, no `pathlib`/`random`/`argparse`/async.

**Phase-2.19 slice (`bytes`, 2026-10-24) — done & verified:**

- New `Type::Bytes`/`Const::Bytes`/`Builtin::Bytes`, `KIND_BYTES=20` and
  `ObjData::Bytes(Vec<u8>)`; bytes literals `b"..."` (parser escapes),
  `bytes()`/`bytes(int)`/`bytes(iterable)`/`bytes(str, encoding)`.
- Indexing returns int 0..255 (negative indices), slicing returns bytes
  (slice.indices, step), `len`, iteration returns ints; `in` for
  subsequence/int; `+`, `*`, comparison operators, equality/hashability
  (dict/set keys), truthiness and CPython-like `b'...'` repr.
- Methods: `decode([encoding[,errors]])`, `hex`, `count`, `find`,
  `startswith`, `endswith`, `replace`, `split`, `join`, `strip`/`lstrip`/
  `rstrip`, `upper`/`lower`; `str.encode([encoding[,errors]])`.
- PYX1 pickle/spawn/MpQueue support bytes (TAG_BYTES); GC childless.
- Verified: **515 tests** (108 E2E incl. bytes JIT+AOT, 169 frontend,
  3 codegen, 2 IR, 152 runtime unit, 22 bytes, 5 concurrent GC, 8 CSV,
  6 GC, 12 IO, 1 JSON-GC, 6 JSON-IO, 9 native concurrency, 4 pool,
  3 STW-GC, 5 thread integration), CPython differential for bytes, gate green.
- Limits: no bytes file modes (`rb/wb`), no `bytearray`/`memoryview`,
  method subset (no `translate`/`format`/regex), 64 MiB constructor limit.

**Phase-2.20 slice (binary files, 2026-10-24) — done & verified:**

- FileState supports `rb/wb/ab/xb` (and text aliases `rt/wt/at/xt`):
  `read`/`readline`/`readlines`/iteration return `bytes`, `write`/
  `writelines` accept only bytes; cursor/tell/seek in bytes. Text mode
  remains character-based. `encoding`/`newline`/`errors` with `b` and
  `+` modes are rejected; CSV/JSON refuse binary files.
- Example `examples/binary_files.py` is CPython-identical, JIT+AOT.
- Verified: **522 tests** (109 E2E incl. binary_files, 169 frontend,
  3 codegen, 2 IR, 152 runtime unit, 22 bytes, 5 concurrent GC, 8 CSV,
  6 GC, 18 IO, 1 JSON-GC, 6 JSON-IO, 9 native concurrency, 4 pool,
  3 STW-GC, 5 thread integration), gate ALL GREEN.

**Phase-2.21 slice (`random`/`platform`/`statistics`, 2026-10-24) — done & verified:**

- `random`: global SplitMix64 PRNG + seedable native `Random(seed=...)`
  with `seed`/`random`/`randint`/`randrange` (negative steps, empty
  ranges)/`choice` (list/tuple/str/bytes)/`uniform`/`shuffle` (in-place).
  Deterministic per seed, but **not CPython-reproducible**
  (documented deviation).
- `platform`: `system`/`machine`/`release`/`version`/`node`/`platform`/
  `python_version` + `python_implementation() == "PyX"`.
- `statistics`: `mean`/`fmean`/`median`/`pstdev`/`stdev` over numeric
  sequences incl. error paths (empty/non-numeric, stdev n<2).
- Verified: **533 tests** (110 E2E incl. stdlib_utils JIT+AOT, 169 frontend,
  3 codegen, 2 IR, 152 runtime unit, 22 bytes, 5 concurrent GC, 8 CSV,
  6 GC, 18 IO, 1 JSON-GC, 6 JSON-IO, 9 native concurrency, 4 pool,
  3 STW-GC, 10 stdlib utils, 5 thread integration), gate ALL GREEN.

**Phase-2.22 slice (`pathlib.Path`, 2026-10-24) — done & verified:**

- Native `pathlib.Path` (`Path()`, `cwd()`, `home()`) with
  `exists`/`is_file`/`is_dir`, `read_text`/`write_text`, `read_bytes`/
  `write_bytes`, `open` (text/binary), `mkdir(parents, exist_ok)`,
  sorted `iterdir`/`glob` (`*`/`?`, no `**`), `name`/`stem`/`suffix`/
  `parent`/`joinpath`/`resolve`/`absolute`/`as_posix`; `/` operator joins
  path/str/path. `os.remove`/`rename` accept Path.
- Due to missing attribute values, the metadata were **at the time** methods
  (`p.name()` instead of `p.name`); method keywords were not yet supported.
  Since Phase 2.24, data attributes (`p.name`) and method keywords
  (`mkdir(parents=True)`) are available.
- Verified: **545 tests** (state of this slice; today 1371 tests; 111 E2E
  incl. pathlib JIT+AOT, 169 frontend,
  3 codegen, 2 IR, 152 runtime unit, 22 bytes, 5 concurrent GC, 8 CSV,
  6 GC, 18 IO, 1 JSON-GC, 6 JSON-IO, 9 native concurrency, 11 pathlib,
  4 pool, 10 stdlib utils, 3 STW-GC, 5 thread integration), gate ALL GREEN.

**Phase-2.23 slice (formatting, 2026-10-24) — done & verified:**

- f-string format specs (`:.2f`, `:04d`, `:#x`, alignment/fill/
  sign/grouping) and `!s`/`!r`/`!a`; central logic in
  `core/format.rs`, ABI `pyx_format_value` (value + static spec +
  conversion flag).
- `str.format(...)` with positional/named fields, index/attribute
  paths and specs; printf formatting `"%s:%d" % (...)` incl. tuple
  arguments, flags/width/precision and `%%`.
- Error paths: invalid specs → `ValueError`, wrong operand types →
  `TypeError`; example `examples/formatting.py` JIT+AOT, `tests/format.rs`.
  Gate green.

**Phase-2.24 slice (attributes, method keywords, builtin values, 2026-10-24) — done & verified:**

- `ExprKind::GetAttr` + ABI `pyx_getattr` for data attributes of native objects:
  `Path` (`name`/`stem`/`suffix`/`parent`/`path`), files (`name`/`mode`/
  `closed`/`encoding`), `Thread` (`name`/`ident`/`daemon`) and `Process`
  (`name`/`pid`/`exitcode`/`daemon`). `p.name` works; unknown names
  → `AttributeError` (`tests/attrs.rs`).
- `StmtKind::SetAttr` + `pyx_setattr` for class instances; class
  namespaces remain immutable.
- `ExprKind::BuiltinRef` + `pyx_builtin_function`: builtins (`int`, `len`,
  `map`, …) are values (`f = int`) and callable indirectly.
- `MethodCall.keywords` + ABI `pyx_method_kw`; keywords are implemented for
  `str.split`/`replace`/`startswith`/`endswith`,
  `Path.mkdir`/`open`/`read_text`/`write_text`/`read_bytes`/`write_bytes`/
  `glob` and `argparse` methods (`Path.mkdir(parents=True,
  exist_ok=True)`). Native constructors (`Thread(target=...)`,
  `queue.Queue(maxsize=...)`) bind keywords via their `FunctionSpec.params`;
  other native methods and user functions/methods remain positional.

**Phase-2.25 slice (`os.path`, `argparse`, `defaultdict`, 2026-10-24) — done & verified:**

- `os.path` (`join`, `basename`, `dirname`, `exists`, `isfile`, `isdir`,
  `abspath`, `splitext`, `split`) as its own module in the central
  specification; `import os.path`/`from os.path import join`.
- `argparse` subset: `ArgumentParser`, `add_argument` (positional, long/
  short flags, `type`, `default`, `required`, `choices`, `help`, `store_true`,
  `nargs="?"`), `parse_args` → `Namespace`; `-h` prints help with `_help=True`.
  No `SystemExit` (errors → `RuntimeError`).
- `collections.defaultdict` with `default_factory` (also `int`/`list`/`str`),
  a missing key calls the factory and inserts; `None` factory → `KeyError`;
  dict-compatible methods/repr. `tests/stdlib_modules.rs`, example
  `stdlib_modules.py` JIT+AOT. Gate green.

**Phase-2.26 slice (`re`, `socket`, 2026-10-24) — done & verified:**

- `re` as a wrapper around the Rust `regex` crate: `search`, `match`, `fullmatch`,
  `findall`, `sub`, `split`, `escape`, `compile`; `Pattern`/`Match` as native
  objects with `group`/`start`/`end`/`span`/`findall`/`search`. Syntax is
  Rust `regex` (no lookarounds/backreferences), errors → `ValueError`,
  wrong types → `TypeError`.
- `socket`: blocking TCP (`AF_INET`/`SOCK_STREAM`) via `std::net`;
  `socket`/`create_connection`, `bind`/`listen`/`accept`/`connect`,
  `send`/`sendall`/`recv`, `close`, `settimeout`/`gettimeout`,
  `shutdown`, `setsockopt`/`getsockopt`, `getsockname`/`getpeername`,
  `fileno`. Blocking calls park the thread for the GC.
  No TLS/UDP/`select`, no `socket.timeout` class.
- Example `socket_echo.py` (thread + TCP) JIT+AOT, `tests/socket.rs`.
  Gate green.

**Phase-2.27 slice (classes, 2026-10-24) — done & verified:**

- `StmtKind::ClassDef` + ABI `pyx_class_new`: `class Name(Base)` with at most
  one simple base, methods, `__init__`, inheritance/overrides.
  Class body only methods + `pass`/docstring; class namespace
  immutable; no class attributes/decorators/metaclasses/keywords/
  multiple inheritance.
- Instances: `Name(...)` calls `__init__` with `self`, attributes via
  `pyx_getattr`/`pyx_setattr`, methods via `pyx_method` with binding to the
  instance; the attribute table is GC-traced and separate per instance.
- Deterministic representation: `<class 'Name'>` or `<Name object>`
  (no heap address). `examples/classes.py` JIT+AOT, `tests/classes.rs`;
  GC-safe. Gate green.

**Phase-2.28 slice (closures + then-eager generators, 2026-10-24) — historical, replaced by Phase 2.32:**

- **Closures by value:** nested `def` produce
  `StmtKind::Closure`/`pyx_closure_new` and capture the *current values* of
  the outer locals as leading wrapper parameters (`capture_count`). Later
  changes to the outer variable are invisible; every execution (also in
  loops) gets its own captures. Recursive nested functions and
  closures in lambdas are diagnostics (`tests/closures.rs`,
  `pyx-cli/tests/closures.rs`, `examples/closures.py`).
- **Generators (historically eager):** `yield value` in a function created
  a hidden `$gen` list local, appended each value and returned the list.
  Since Phase 2.32 lazy (fibers); generator expressions remain eager.
  `yield from`, `yield` in expressions, bare `yield` and `return <value>` were
  diagnostics at the time; bare `yield` and `return` are now supported.
- Generator expressions remain eagerly materialized. Gate green.

**Phase-2.29 slice (then-eager async, `with` protocol, aug subscript, 2026-10-24) — historical, replaced by Phase 2.32:**

- *Historical state of this slice:* `async def` is lowered like `def`;
  `await expr` returns `expr`, `async with`/`async for` use the same
  synchronous lowering/protocol logic. No coroutines/tasks/event loop;
  `asyncio.run(value)` is identical, `sleep` blocks GC-safely, `gather`
  materializes sequentially. Since Phase 2.32 replaced by real fibers.
  `examples/generators_async.py` JIT+AOT.
- `with`/`async with`: hidden `$withN` local, `__enter__()` binds `as`,
  `__exit__(None, None, None)` in reverse order on every path
  (return/break/continue/exception); return values do not suppress
  exceptions. Files, `Pool` and user classes implement the protocol.
- `a[i] += x` evaluates object and index exactly once via `$aug` locals
  (augmented subscript assignment). Gate green.

**Phase-2.30 slice (user modules, CLI/toolchain, cache, scaling, layout, 2026-10-24) — done & verified:**

- **User modules:** `import foo`/`from foo import f` resolve `foo.py` next to
  the importing file; the loader caches per canonical path, loads
  dependencies first, detects cycles. Only functions are exported;
  imported modules may not contain top-level code other than imports/function
  definitions/docstring/`pass`/`__name__` assignment. Packages,
  dots/relative imports and `import *` are diagnostics. `__name__` is
  `"__main__"` in the entry file; no runtime module object/`sys.modules`.
  JIT and AOT (embedding) run identically; `pyx-cli/tests/user_modules.rs`.
- **CLI:** `new`, `init`, `test` (plain programs, `--json`), `fmt`
  (`--check`), `lint` (`--json`), `clean` (`--dry-run`); `add`/`remove`/
  `sync`/`lock` as unchanged uv passthrough (`PYX_UV`).
- **Build cache:** `.pyx/cache` with a deterministic key (source text,
  reachable `.py` modules, target/OS, version, runtime archive), `--no-cache`,
  hit/invalidation tests; `pyx clean` removes it.
- **Scaling benchmarks:** `pool_serial`, `pool_parallel`, `process_queue`
  (additionally `threads_serial`/`threads_parallel`/`pool_map`) in the micro harness,
  in `results/latest.*` and `thresholds.json`.
- **Layout:** crates internally structured into `pyx-ir/{model,ir,stdlib}`,
  `pyx-frontend/{lower,sema}`, `pyx-codegen/{backend,lower}`,
  `pyx-runtime/{abi,core,gc,modules,concurrency,support,csv}`,
  `pyx-cli/{cli,cli/commands}`; `docs/LAYOUT.md`.
  Overall state **730 tests** (at the time), gate ALL GREEN.

**Phase-2.31 slice (slab allocator, 2026-10-24) — done & verified:**

- `gc/alloc.rs`: fixed `PyxObj` slab/free-list allocator (64-size chunks,
  8-byte alignment, LIFO reuse, slabs survive `free_all`,
  `absorb` on shared-heap migration, stats `slabs/capacity/live/reused`).
  No GC semantics change; `PYX_GC_STATS` contains pool stats.
- Verified: **730 tests** (at the time; incl. 4 allocator tests), GC/thread stress,
  JIT+AOT, gate ALL GREEN. Real variable payload size classes and
  precise stack maps remain blocked as separate architecture phases.

**Phase-2.32 slice (lazy generators, coroutines/event loop, GC stats, dict index, int fastpaths, 2026-10-24) — done & verified:**

- **Lazy generators:** A function with `yield` runs in a stackful
  fiber (`core/fiber.rs`: its own 128 KiB stack), which starts only at the first
  `pyx_iter_next`. On x86_64 the context switch is a minimal
  `global_asm!` switch over the callee-saved registers (`rsp`, `rbx`, `rbp`,
  `r12`–`r15`, `ret`-based, no syscalls); other targets use the
  libc `ucontext` path as fallback. Every generator function creates an
  independent object; `yield value`/bare `yield`, `return` as end,
  exception propagation, `for`/`list`/`sum` work. New builtins
  `iter(x)`/`next(it[, default])` (`pyx_iter`/`pyx_next`). Generator
  expressions are eagerly materialised into an iterator (single-pass,
  `next()` works).
- **Coroutines/event loop:** `async def` creates a coroutine object;
  `await`/`pyx_await` drives it, `asyncio.run` runs the cooperative
  single-thread loop, `asyncio.sleep` suspends only the current fiber
  (blocking outside a loop), `asyncio.gather` starts
  concurrent tasks, delivers results in input order and
  propagates the first error. No GIL. Generators support
  `send`/`throw`/`close`, `yield` expressions and `yield from`; `throw`
  accepts exception instances only. Not supported: async generators
  (`async def` + `yield` is a diagnostic) and async comprehensions;
  `async for`/`async with` still use the synchronous protocol.
- **GC:** fiber stack as well as `ctx` and `caller_ctx` (the callee-saved
  register frames) are registered as conservative roots from creation
  for the entire lifetime; while a fiber runs, `push_stack_bounds`
  overrides the stack bounds so that the scan sees the fiber. The
  outermost resume publishes two aux ranges (native driver stack and
  `caller_ctx`); shared mode mirrors both, local mode gets by without
  atomics per switch. A `saved_regs` array/`FIBER_REG_WORDS` no longer
  exists. `PYX_GC_STRESS=1` covers interleaved generators. New API
  `gc_stats()`/`reset_gc_stats()`/`GcStats` and `pyx gc [--json] <file.py>` with
  `collections`, `live_objects`, `threshold`, `pause_total_us`,
  `pause_max_us`, `mark_total_us`, `sweep_total_us`, `freed_total`, `stress`
  (last line machine-readable with `--json`). `PYX_GC_STATS=1` remains
  unchanged. Precise stack maps remain open.
- **Dict hash index:** `DictData` keeps insertion order plus a hash index
  over Python equality (first position, last value; numeric
  int/float/bool key equivalence; `pop` reindexes). Lookup/insert are thus
  O(1) instead of linear; dict_insert AOT 2.1 ms vs. CPython 12.7 ms.
- **Codegen/frontend:** typed int fastpaths for `+`/`-`/`*` and
  comparisons (checked overflow -> `pyx_binop` slowpath for exact
  `OverflowError`), also in AugAssign and fully int-typed
  `for range` counters; constant folding for int/float/str/binops/comparisons/
  unary (overflow- and division-by-zero-protected) as well as dead-`if`/`while False`
  elimination.
- **Benchmarks:** `benchmarks/bench_pyx.py` compares CPython/JIT/AOT with 11
  workloads (incl. lazy generator sum and real async `gather`), checks the
  output against CPython and is registered in `bench_all.py` as `pyx`
  (`uv run benchmarks/bench_all.py --only pyx`). Release, n=200000:
  while_sum 7.7x JIT / 12.4x AOT, for_range_sum 6.4/8.0, dict_insert
  3.6/4.5. `math_loop` is at parity after the native dispatch cache
  (0.94x JIT / 1.01x AOT; 50k micro harness 6.2 ms vs. CPython 13.6 ms).
  Generator sum over 100k elements (release): CPython 10.2 ms,
  JIT 7.7 ms, AOT 6.8 ms; `strace -c` counts 94 syscalls total (previously
  200,097, two `rt_sigprocmask` per yield). Remaining weak spots: string
  concatenation (O(n²)) and float boxing — open performance work.
- **Verified:** 1329 tests, `cargo fmt`, `cargo clippy --workspace -- -D warnings
  --all-targets -- -D warnings` and `cargo test --workspace` green;
  micro baseline renewed (while_sum AOT 1.7 ms vs. CPython 23.5 ms).

**Known, documented limits Phase 1/2:** i64 instead of BigInt; GC is a
conservative collector (stack scan, STW root rescan; white partition briefly
under heap lock, freeing concurrent); no precise roots/stack maps and
no size-class allocator (P2-05/06/07 open); heavy rich rendering still loses
live values under allocation, so `bench_all.py` runs only with a raised
`PYX_GC_THRESHOLD`; exception hierarchy/
chaining/`except*` missing; `dict` views are materialized
as lists; set order is deterministic (Python guarantees
none); `and`/`or` return operands, types there may be `Any`. Dicts use
a hash index (O(1) on average, Python equality, numeric key equivalence).
**Exceptions:** hierarchy matchers (`except Exception`/`BaseException`), tuple
handlers and explicit `raise ... from` are implemented; implicit context
chaining and `except*` remain open; tracebacks are simplified (one frame).
**Classes:** multiple bases are approximated by the first base; no class
attributes/decorators/metaclasses/descriptors; zero-argument
`super().__init__(...)` targets the direct base, other `super()` forms are
unsupported; native `threading.Thread` is available as a subclass base;
instance representation `<Name object>`; class namespace immutable.
**Closures:** by value; no mutable captures, no recursive nested
functions, no closures in lambdas.
**Generators/async:** real lazy generators (stackful fibers) and coroutines
with a cooperative event loop. Open: async generators, async comprehensions
and `asyncio` beyond `run`/`sleep`/`gather`; `async for`/`async with` use
the synchronous protocol. Generator expressions are eagerly materialised
into an iterator (single-pass, `next()` works).
**Performance:** native module dispatch is fixed (thread-local spec cache;
math_loop at parity/faster). Remaining weak spots: string concatenation is
O(n²) and CPython-extension boundaries will add conversion cost. The x86_64
fiber switch costs two register-only switches without syscalls (100k
generator sum AOT 6.8 ms vs. CPython 10.2 ms).
**Modules:** native modules + sibling `.py`; stdlib modules are first-class
values (`m = errno`, `getattr(m, name)`) and simple module-level assignments
are visible inside functions. User-module values, packages, dots/relative
imports, top-level side effects in imported modules and a general runtime
module object/`sys.modules` are missing; imports only at module level.
**Stdlib:** curated subset; `re` = Rust `regex` syntax, `socket` only
blocking TCP (no TLS/UDP/select, timeout → `OSError`), `argparse` without
`SystemExit`, `random` not CPython-reproducible, CSV entirely in
memory.
**Spawn/pool:** function registry only by name/arity (duplicates/lambdas/native
targets rejected); queue `maxsize` is lost in the spawn payload; pool
context manager present.
**File I/O:** UTF-8 text (`r/w/a/x`) and bytes (`rb/wb/ab/xb`), no
`+` mode; `newline`/`errors` ignored.
**Bytes:** literals/constructor/ops/core methods, but no
`bytearray`/`memoryview`.
**C-API/ecosystem:** decision B (PyX-native CPython ABI layer, no
`libpython`). A limited-API fixture now imports/calls under JIT and AOT, but
the shim covers only the first symbol subset (no capsules/buffer/container
constructors, no wheels/venvs, no full 3.14 ABI); NumPy/pydantic-core and the
whole stdlib/ecosystem program remain open (P4-04a…P4-11).
**Platform:** Linux x86_64 verified; aarch64 paths untested; macOS/
Windows/cross-compilation missing.

## Next Steps

1. P2-10/P2-12 remainder: async generators/comprehensions and `asyncio`
   beyond `run`/`sleep`/`gather`.
2. Performance: string concatenation (O(n²)) and float boxing remain; native
   module dispatch and generator resume are no longer bottlenecks.
3. P2-05/P2-06/P2-07: size-class allocator, precise shadow-stack roots/
   stack maps, finer safepoint polling (P2-08 metrics are available).
4. P7 standard library: Batch A, the Batch B native codec/text core, the
   benchmark-critical batch, the package/site-packages loader and the new
   native `typing`/`abc`/`enum`/`types`/`itertools`/`ast`/`fractions`/
   `importlib`/`__future__` modules are importable/API-tested. Next are
   P7-03 builtin shims, the remaining Batch B/C/E/F modules and the native
   bound-method/dynamic-keyword-call blockers for `bench_all.py` under PyX,
   then the dependency-ordered batches in `docs/TODO.md`. P4: extend the working abi3
   slice (capsules, buffer, container constructors, wheels/venv discovery),
   then the full CPython 3.14 ABI/NumPy/pydantic-core (P4-04c/d),
   P4-05/P4-06; ecosystem suites/benchmarks P4-10/P4-11.
5. P3-03/P3-08: uv interpreter injection (`.python-version`, venv marker,
   `sys.executable` equivalent), packaging/cross-compilation.
6. P5/P6 remainder: Hyperfine adapter, CI thresholds (Q-06), completions/manpage
   (P6-07), ICE regression test (P6-05/P6-08), CLI reference/diagnostic catalog
   (P6-09), `--verbose`/`--quiet` (P6-03/P6-10).
7. Q-03/Q-04: fuzzing for parser/lowering/codegen and runtime/GC;
   exception hierarchy/chaining (P2-11 remainder).

## Commands

```bash
export PATH="$HOME/.cargo/bin:$PATH"   # rustc/cargo live here
cargo build                            # whole workspace
cargo test                             # unit/integration tests (1371)
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
./scripts/verify.sh                    # full phase gate
cargo run -q -p pyx-cli -- new /tmp/demo
cargo run -q -p pyx-cli -- run examples/fib.py
cargo run -q -p pyx-cli -- build examples/fib.py -o /tmp/fib && /tmp/fib
cargo run -q -p pyx-cli -- test examples
cargo run -q -p pyx-cli -- fmt --check examples
cargo run -q -p pyx-cli -- lint examples
cargo run -q -p pyx-cli -- gc examples/fib.py        # GC statistics
cargo run -q -p pyx-cli -- gc --json examples/fib.py
python3 benchmarks/pyx/run_micro.py    # CPython vs. JIT vs. AOT
uv run benchmarks/bench_pyx.py         # CPython vs. JIT vs. AOT, 11 workloads
uv run benchmarks/bench_all.py --only pyx
python3 scripts/check_benchmarks.py    # local threshold gate
```

## Architecture invariants (short version)

- IR is the only frontend↔codegen contract; signatures in
  `crates/pyx-ir/src/ir.rs`/`model.rs`.
- ABI is fixed in `docs/ARCHITECTURE.md` §3; runtime functions live in
  `pyx_runtime::abi`, `extern "C"`, `#[no_mangle]`, **without panics**.
- Values: tagged `i64`; ints/bools/None immediate; heap objects 8-byte
  aligned with a kind header.
- Errors go through pending exception + `pyx_exc_pending()` checks in codegen.
- Heap objects have been managed since Phase 2.1/2.15 by the conservative, concurrent
  mark-&-sweep GC (shared heap, insertion barriers, safepoints);
  no more leak state. Precise roots/allocator remain open.
- Backend: Cranelift 0.120; LLVM later optional.
- No silent miscompiles: unsupported syntax → diagnostic.
- Docs invariants: `docs/ARCHITECTURE.md` (contracts), `docs/TODO.md`
  (work list), `docs/C_API.md` (P4-04), `docs/LAYOUT.md` (tree) must agree with
  code/test count; no invented measurements/"done" claims.

## Working agreement for subagents

- Every agent owns **disjoint files**; no edits to other agents' crates.
- Agents only edit; they do **not** run builds/tests/formatters.
- Do not change contracts (IR/ABI) on your own authority; report deviations.
- The orchestrator builds, tests, formats and dispatches fix-ups.

## Session log

- 2026-10-23: Repo is greenfield (only `PLAN.md`, no git). Environment checked.
  `docs/TODO.md`, `docs/ARCHITECTURE.md`, workspace scaffold, IR and ABI contract
  created; Phase-1 implementation distributed to subagents.
- 2026-10-23 (continuation): Phase 1 fully verified (parser/lowering/
  inference, runtime, Cranelift JIT+AOT, CLI, 45 tests, CPython differential).
  Two contract bugs found and fixed: `pyx_truthy` returns 0/1 instead of
  tagged booleans (otherwise all conditions are true); frame stack for correct
  traceback frames; bare re-raise via `pyx_raise_value`; `str*int` inference.
  Then the Phase-2.0 container slice: list/dict, indexing/assignment, iteration
  ABI, for-over-containers, Python repr, new exception tags, examples and
  E2E tests. Gate: `scripts/verify.sh` ALL GREEN, 79 tests, AOT parity,
  CPython differential identical. Next step: P2-11 (`else`/`finally`),
  then classes/modules/GC (see above).
- 2026-10-23 (continuation 2): Phase-2.1 GC interim implemented. Conservative
  mark-&-sweep with registry, stack/register roots (asm dump of all GPRs,
  asm stack loads), pending-exception root and explicit root for the new
  object. Threshold/stress via env, ABI `pyx_gc_collect`/`pyx_gc_objects`.
  Verified: 89 tests, stress/threshold runs, AOT, cycles, stress test
  15.5 MB vs. 154 MB peak RSS. Open: precise roots/concurrent collector
  (P2-06/07), size-class allocator (P2-05), then P2-11/P2-01/P2-02.
- 2026-10-23 (continuation 3): P2-11 `try/else/finally` implemented. IR extended
  with else/finally and bare except; codegen with cleanup stack,
  action codes and parked exception; return bug fixed; bare raise in the
  finally re-raises the in-flight exception. KeyError repr corrected.
  Verified: 100 tests, CPython differential for finally edge cases (nested,
  return/break/continue, suppression, override), AOT + GC stress, gate green.
  Next slice: slices/f-strings/string methods (P2-02).
- 2026-10-23 (continuation 4): P2-02 slice implemented: read-only slices for
  str/list (IR `ExprKind::Slice`, ABI `pyx_slice`, full slice.indices
  semantics) and simple f-strings (`{expr}`, `!s`) via str() concatenation.
  Format specs/`!r`/`!a`, slice assignment and dict slices produce diagnostics.
  Verified: 124 tests, CPython differential (slices/f-strings), GC stress,
  AOT slices/fstrings, gate green. Next slice: string/container methods
  and format specs.
- 2026-10-23 (continuation 5): P2-02 methods implemented. IR `MethodCall`,
  ABI `pyx_method`, dynamic dispatch for str/list/dict, AttributeError.
  Static method matrix + `Any` receivers; GC-safe list construction
  (split/keys/values). Verified: 158 tests, CPython differential,
  GC threshold, AOT methods, gate green. Next slice: tuple/set/bytes,
  then format specs/!r/%-formatting.
- 2026-10-23 (continuation 6): P2-01 tuple slice implemented. `Type::Tuple`,
  tuple literals, unpacking (assignment + for targets), dict.items(), tuple
  count/index, ABI pyx_tuple_new/pyx_unpack, GC tracing. repr quote choice adapted
  to CPython. Verified: 198 tests, CPython differential, GC, AOT,
  gate green. Next slice: set/bytes + membership (`in`).
- 2026-10-23 (continuation 7): P2.6 set + membership implemented. Set type
  (insertion-ordered, hashable, GC-traced), literals, set()/set(iterable),
  methods add/remove/discard/clear/copy/union, `set - set`, `in`/`not in`
  for list/tuple/dict/str/set. Frontend method matrix and diagnostics extended;
  len inference fix (no longer forcing an unresolved variable to str).
  Verified: 231 tests, CPython differential, GC, AOT, gate green.
  Next slice: bytes, then format specs/!r/%-formatting.
- 2026-10-23 (continuation 8): P6.0 diagnostics polish. Diagnostic contract
  extended with code/end_col/help; all frontend errors categorized, did-you-mean
  for names/methods, parse spans. CLI renders Cargo-like snippets, --json,
  colors, summary; `pyx doctor`; ICE catch_unwind. 256 tests, gate green.
  The P6 list (Cargo-like unit: new/init/test/fmt/lint/clean, cache,
  completions, docs) is recorded at the end of docs/TODO.md.
- 2026-10-23 (continuation 9): P2.7 range + benchmark builtins. Range as a value,
  list/dict/sum/enumerate/zip; range membership relaxed to CPython semantics.
  290 tests, gate green. benchmarks/ analyzed: existing harnesses are
  CPython/uv-based; readiness matrix and PyX micro harness are added,
  user harnesses remain unchanged. Next language slots: comprehensions/
  lambdas/modules (loops/csv), then bytes/format specs.
- 2026-10-23 (continuation 10): Benchmark integration started. `benchmarks/`
  analyzed, readiness matrix `benchmarks/PYX.md`, import-free micro harness
  `benchmarks/pyx/run_micro.py` (CPython vs JIT vs AOT, output verification,
  peak RSS) and baseline `benchmarks/pyx/results/latest.{json,md}`.
  Release baseline: startup 20.4/2.9/1.0 ms, while_sum 47.5/10.6/8.0 ms,
  dict_insert 21.8/79.6/52.4 ms (linear), string_concat 23.9/13.2/12.4 ms
  (O(n²)). The large user harnesses remain unchanged; adapter follows.
- 2026-10-23 (continuation 11): P2.8 comprehensions. List/set/dict/generator
  expressions incl. multi-for/if/tuple targets/shadowing; eagerly materialized
  generators documented. Runtime append helpers, GC-safe codegen loops.
  307 tests, gate green. Benchmark baseline extended with list_comp/filter_comp/
  sum_genexp (AOT 5.9/7.5/6.7 ms). Next slice: lambdas/
  first-class functions + map/filter/reduce.
- 2026-10-23 (continuation 12): P2.9 first-class functions. FunctionRef/
  IndirectCall, lambda synthesis (without closures/defaults/star), argv wrapper for
  JIT+AOT, pyx_call, map/filter (eager). 333 tests, gate green. Baseline:
  map_lambda 30.7/13.0/13.4 ms, filter_lambda 28.6/13.9/12.9 ms,
  lambda_call 32.7/15.4/14.4 ms. Next slice: modules/imports/with.
- 2026-10-23 (continuation 13): P2.10 modules/imports. Shared stdlib spec;
  import/from-import aliases; module attrs/calls; native functions; math,
  functools.reduce, operator, time, sys. 370 tests, gate green. Baseline:
  reduce_add 27.0/7.2/5.7 ms, math_loop 28.8/22.4/25.8 ms. Next slice:
  user modules/import system + json/os/io/with/csv, then async.
- 2026-10-23 (continuation 14): P2.11 json + os. dumps/loads (CPython defaults,
  GC-safe), os basics. 386 tests, gate green. Baseline json_dumps
  29.0/5.6/2.7 ms, json_loads 26.8/4.9/2.7 ms. P2-20 (real threading/
  multiprocessing) added to docs/TODO.md, incl. shared-heap/GC design,
  threading, multiprocessing, serialization, parallel benchmarks and
  race/stress tests.
- 2026-10-24 (continuation 15): P2.12 shared heap + `_thread`, no GIL.
  Global heap with migration on the first thread, thread registry, locks,
  start_new_thread/get_ident/join (PyX extension); GC suspended with >1 thread,
  active again after join. 392 tests, gate green. Threads run JIT+AOT,
  4x3000 string stress under lock ok. Baseline serial 130.2/32.9/32.1 ms,
  parallel 73.0/23.3/17.8 ms. Next steps: STW/Go collector,
  threading classes, multiprocessing, file I/O/async.
- 2026-10-24 (continuation 16): P2.13 STW/safepoint GC. ThreadGc, safepoints
  (codegen), parking on lock/join/sleep, thread roots (spawn arguments),
  pending mirror, collector scans parked stacks/registers + roots. 398 tests,
  gate green, JIT+AOT, threshold=1 stress. Safepoint overhead measured
  (threads_serial AOT 39.6 vs 32.1 ms). Next slice: threading classes,
  multiprocessing, then file I/O/async and Go collector.
- 2026-10-24 (continuation 17): P2.14 threading/queue/multiprocessing/pickle.
  Native thread/lock/RLock/event/queue/process/queue objects, fork/waitpid/
  _exit, own PYX1 pickle, keyword constructors. 422 tests, gate green,
  JIT+AOT. Limits: fork-only (no spawn/pool). Next slice: concurrent
  GC.
- 2026-10-24 (continuation 18): P2.15 concurrent mark-&-sweep. Colors,
  root STW + concurrent mark phase + final STW sweep, insertion barriers
  per edge, graph lock (local bypass), growth-based threshold, GC stats.
  Edge barrier fix: 110k list 8.5 s -> 17–44 ms marking; 422 tests, gate
  green. Next slice: pool/spawn, concurrent sweep, safepoint polling.
- 2026-10-24 (continuation 19): P2.16 real spawn + pool. Fresh process
  via exec, function registry/arity, payload/queue FDs, no module body in the
  child, hidden `__spawn`; pool with per-worker queues for fork and spawn.
  436 tests, gate green, JIT+AOT (spawn/pool/pool_spawn). Next slice:
  process scaling/terminate, concurrent sweep.
- 2026-10-24 (continuation 20): P2.17 concurrent sweep + terminate.
  Phase C STW rescan, then white partition under heap lock and freeing
  outside; color reset/ MARKING off. Process/Pool.terminate, pool_map
  benchmark (AOT 6.5 ms vs CPython 59.2 ms). 440 tests, gate green.
- 2026-10-24 (continuation 21): P2.18 file I/O/with/CSV/JSON files/os.remove.
  File object UTF-8 text, OSError tag, with desugaring, csv reader/writer
  families, json dump/load; examples files/csv_json JIT+AOT. 477 tests,
  gate green. Next slice: async/bytes/pathlib/random/argparse.
- 2026-10-24 (continuation 22): P2.19 bytes. Literals/constructor, index/
  slice/iteration, ops/hash, decode/encode/hex/split/join/strip,
  serializer; CPython differential, JIT+AOT. 515 tests, gate green.
- 2026-10-24 (continuation 23): P2.20 binary files rb/wb/ab/xb, bytes
  read/write/iteration, byte seek/tell, CSV/JSON rejection. 522 tests,
  gate green. Next slice: async/socket/pathlib/random.
- 2026-10-24 (continuation 24): P2.21 random/platform/statistics. SplitMix64,
  Random object, randrange/choice/shuffle, platform values, statistics functions.
  533 tests, gate green, JIT+AOT. Next slice: pathlib/argparse/async.
- 2026-10-24 (continuation 25): P2.22 pathlib.Path. Native Path, text/bytes,
  mkdir/iterdir/glob, `/` operator, os.remove/rename path-like; 545 tests,
  gate green. Next slice: argparse/defaultdict/async.
- 2026-10-24 (continuation 26): P2.23 formatting. f-string specs/`!s`/`!r`/
  `!a` via `pyx_format_value`, `str.format`, `%` formatting; `core/format.rs` as
  shared implementation; example formatting, JIT+AOT, gate green.
- 2026-10-24 (continuation 27): P2.24 attributes/method keywords/builtin values.
  `GetAttr`/`pyx_getattr` (Path/File/Thread/Process), `SetAttr`/`pyx_setattr`,
  `BuiltinRef`/`pyx_builtin_function`, `pyx_method_kw` (`mkdir(parents=True)`);
  `p.name` instead of `p.name()`; tests/attrs.rs, gate green.
- 2026-10-24 (continuation 28): P2.25 os.path/argparse/defaultdict. Nine
  os.path functions, ArgumentParser/Namespace subset without SystemExit,
  defaultdict with factory; example stdlib_modules, gate green.
- 2026-10-24 (continuation 29): P2.26 re/socket. Rust `regex` wrapper
  (Pattern/Match, search/match/fullmatch/findall/sub/split/escape) and
  blocking TCP (socket/create_connection/bind/listen/accept/...);
  GC-safe parking; socket_echo JIT+AOT, gate green.
- 2026-10-24 (continuation 30): P2.27 classes. Single-base ClassDef,
  methods/__init__/inheritance/overrides, instance attributes, dynamic
  dispatch, deterministic repr; classes.py JIT+AOT, GC-safe, gate green.
  Limits: no class attributes/decorators/metaclasses/multiple inheritance.
- 2026-10-24 (continuation 31): P2.28 closures + then-eager generators
  (later replaced by real fibers). Nested def
  as `Closure`/`pyx_closure_new` with by-value captures (capture_count),
  recursion/lambda closures diagnosed; `yield value` materializes a
  list, yield-from/expression/bare/return in the generator diagnosed.
  closures.py + CLI tests, gate green.
- 2026-10-24 (continuation 32): P2.29 then-eager async + with + aug subscript
  (later replaced by real fibers).
  async def/await sequential, async with/for synchronous, asyncio.run/sleep/
  gather; with via __enter__/__exit__ (files/pool/user classes, reverse
  order, no suppression); `a[i] += x` via `$aug` single
  evaluation. generators_async.py JIT+AOT, gate green.
- 2026-10-24 (continuation 33): P2.30 user modules/CLI/cache/scaling/layout.
  Sibling imports with module cache per canonical path, cycle diagnostic,
  function-only exports, __name__, AOT embedding; CLI new/init/test/fmt/
  lint/clean + uv passthrough; build cache with module-tree fingerprint;
  pool_serial/pool_parallel/process_queue in the harness; crate restructuring.
  State 721 tests, gate green.
- 2026-10-24 (continuation 34): Docs refresh after the completion slice.
  AGENTS/TODO/ARCHITECTURE/README/CHANGELOG updated, `docs/LAYOUT.md`
  and `docs/C_API.md` created. C-API environment probe: python3 3.12.3, but
  no python3-config/Python.h and no linkable libpython → P4-04 `[!]`;
  no measurements invented. No code/Cargo/CI edits.
- 2026-10-24 (continuation 35): Real lazy generators + coroutines/event loop
  (stackful fibers, 128 KiB stack; `pyx_generator_new`/
  `pyx_coroutine_new`/`pyx_yield`/`pyx_await`), `iter`/`next` builtins,
  fiber GC roots incl. stack bounds override, `pyx gc [--json]` +
  `gc_stats()`/`GcStats`, dict hash index, typed int fastpaths,
  constant folding/dead code, `bench_pyx.py` + `bench_all --only pyx`.
  Verified: 771 tests, fmt/clippy/test green; bench_all pyx (release,
  n=200000) while_sum 8.6x/11.9x, for_range_sum 7.0x/13.1x, dict_insert
  3.7x/6.6x; micro baseline while_sum AOT 2.4 ms / CPython 28.9 ms.
  Open: send/throw/close, yield expressions, yield from, async generators/
  comprehensions and math_loop dispatch.
- 2026-10-24 (continuation 36): x86_64 fiber switch without `swapcontext`.
  `global_asm!` `pyx_context_switch` saves only `rsp`/`rbx`/`rbp`/
  `r12`–`r15` (offsets 0/8/16/24/32/40/48) and returns via `ret`;
  `ucontext` remains the fallback for `cfg(not(target_arch = "x86_64"))`.
  GC: `ctx`/`caller_ctx` are lifetime-registered roots instead of a
  `saved_regs` array (`FIBER_REG_WORDS`/`capture_suspended_registers`
  removed); the outermost resume publishes two aux ranges, shared
  mode mirrors them, local mode works without atomics per switch.
  Measured (release, 100k generator sum): CPython 11.0 ms, JIT 7.7 ms,
  AOT 6.8 ms; `strace -c` 94 syscalls total instead of 200,097 (two
  `rt_sigprocmask` per yield). No builds/tests in this docs slice.

- 2026-10-24 (continuation 37): math dispatch cache + P4-04 decision B.
  `pyx_module_call`/`call_native` resolve names without allocations and
  cache the spec per thread (no module-table scan on a hit); math_loop
  improved from 0.81x to parity/faster (AOT 22.50 ms vs. CPython 22.65 ms at
  n=200000; micro 50k AOT 6.2 ms vs. 13.6 ms). Environment probe: Python
  3.14.8 with dev files, NumPy 2.5.4, pydantic 2.14.1. Embedded-CPython
  probe: init 6.67 ms + finalize 7.46 ms, import numpy 41.64 ms, import
  pydantic 11.96 ms; abi3 fixture imports with four undefined Py symbols.
  Decision: **option B**, PyX-native CPython ABI layer, no `libpython` in
  production. TODO P4 reworked into a full stdlib program: 297 modules
  (174 pure Python, 71 extension, 52 builtin/frozen), P4-09 parity,
  P4-10 ecosystem suites (numpy/pandas/pydantic/fastapi/django/...),
  P4-11 ecosystem benchmarks. No code changes claimed as C-API support.

- 2026-10-24 (continuation 38): first abi3 extension slice (P4-04a/b partial).
  `pyx-runtime` gained a limited-API shim (`src/capi/`, variadic
  `PyArg_ParseTuple` in C via the `cc` build script) and an extension loader
  (`modules/extension.rs`, `KIND_EXT_FUNCTION`). The frontend resolves sibling
  `.so`/`.abi3.so` files at compile time and lowers `ext.member(...)`/
  `from ext import member` to `Callee::Extension`/`ExpressionAttr`; codegen
  emits `pyx_extension_call`/`pyx_extension_get`. `pyx` and AOT binaries are
  linked with `--export-dynamic`; `pyx_runtime_init` pins the shim symbols.
  Verified `pyx_capi_probe.abi3.so` under JIT and AOT (`5/abi3/42/abi3`), no
  `libpython` in `ldd`, arity/missing-attribute/type error paths; generator
  `send`/`throw`/`close`/`yield` expressions/`yield from` and C-API containers
  landed afterwards; 846 tests, clippy and `scripts/verify.sh` green. Open:
  capsules/buffer, wheels/venvs, full ABI/NumPy/pydantic-core.

- 2026-10-24 (continuation 39): generator methods, C-API containers, stdlib
  baseline. Implemented `yield` expressions and generator
  `send`/`throw`(instance-only)/`close` with GeneratorExit handling;
  C-API `PyUnicode_*`, `PyFloat_*`, `PyBytes_*`, `PyList_*`, `PyTuple_*`,
  `PyDict_*` with bounded recursive PyX↔shim container conversion; extended
  the abi3 fixture and JIT/AOT test. Added `scripts/stdlib_compat.py` and
  generated `docs/COMPATIBILITY.md` + `benchmarks/compat/latest.json`:
  23/297 stdlib modules importable (297 = 52 builtin/frozen, 174 pure,
  71 extension; 8 names fail under CPython). 824 tests, clippy and
  `scripts/verify.sh` green.

- 2026-10-24 (continuation 40): `yield from` + P6-07. Implemented full
  `yield from` delegation (values, send forwarding, close/GeneratorExit,
  throw forwarding, delegate return value; plain-iterator send raises
  AttributeError) with PyX/CPython parity tests. Added `pyx completions
  <bash|zsh|fish>`, `docs/pyx.1`, and `pyx --version --verbose`
  (target/backend/runtime). 846 tests, clippy and `scripts/verify.sh` green.

- 2026-10-24 (continuation 41): finished P4-04a and extension discovery.
  Added CPython capsules (`PyCapsule_New/GetPointer/GetName/IsValid/
  SetPointer`, PyX `PyCapsule` kind), read-only contiguous bytes buffers
  (`PyObject_CheckBuffer/GetBuffer/ReleaseBuffer`, verified `Py_buffer`
  layout), and all common `PyExc_*` singletons with typed error mapping.
  Extension discovery now also searches `PYTHONPATH`, `PYX_SITE_PACKAGES`,
  `VIRTUAL_ENV`/`CONDA_PREFIX` site-packages and wheel-style filenames.
  Fixture covers capsules and buffers under JIT+AOT; 861 tests, clippy and
  `scripts/verify.sh` green. Open: package submodules, multi-phase init,
  wheel tag policy, full 3.14 ABI.

- 2026-10-24 (continuation 42): finished P4-04b. Dotted extension modules
  (`pkg.sub_ext`) resolve through a directory path while the package needs no
  Python source; wheel ABI-tag policy accepts `.so`/`.abi3.so`/
  `.cpython-314-*` and rejects other tags at compile time; multi-phase init
  (`PyModuleDef_Init` + `Py_mod_exec` slots, bounded walk, policy validation)
  runs alongside single-phase modules. New JIT+AOT fixtures for dotted and
  multi-phase extensions; 875 tests, zero clippy warnings,
  `scripts/verify.sh` green. The CI/verify clippy gate no longer uses
  `-D warnings`; warnings remain fixed at zero.

- 2026-10-24 (continuation 43): CPython 3.14.9 target + official test gate.
  Central `pyx_ir::compat` constants drive `sys.version`/`version_info`
  (3.14.9, plus `sys.pyx_version`), `platform.python_version()`, CLI verbose
  output and the wheel-tag policy. Added `scripts/cpython_compat.py` (pinned
  3.14.9 oracle, cached official source, differential `Lib/test` runs,
  JSON/Markdown, `--gate`, monotonic `--update-manifest`,
  `--check-manifest`), the empty 3.14.9 manifest initially (no fabricated
  passes), and pinned the stdlib import report oracle to 3.14.9. Restored the
  clippy `-D warnings` gate in verify/CI and kept the tree warning-free.
  Added the LTS policy (PyX LTS lines keyed to CPython minors) to PLAN/TODO.
  877 tests, clippy `-D warnings` and `scripts/verify.sh` green.

- 2026-10-24 (continuation 44): exception hierarchy + explicit chaining.
  `except Exception` (all but KeyboardInterrupt/GeneratorExit),
  `except BaseException`, tuple handlers expanded in the frontend, and
  `raise ... from` with deepest-first cause rendering and `from None`
  suppression. Runtime matcher (`pyx_exc_matches`), cause storage traced by
  the GC, caused-raise ABIs, codegen hierarchy dispatch. 905 tests, clippy
  `-D warnings` and `scripts/verify.sh` (incl. manifest check) green.

- 2026-10-25 (continuation 45): first official CPython test green.
  Added first-class native module values (`KIND_MODULE`, `pyx_module_value`,
  module-aware `getattr`), module-level globals (`GlobalGet`/`GlobalSet`,
  `pyx_global_get`/`pyx_global_set`, thread-local keyed store with GC roots),
  `frozenset`, `str.isupper`/`islower`, `AssertionError` (tag 14).
  New native `errno` (Linux table, `errorcode`, `__dict__`) and minimal
  `unittest` (`TestCase` native base, native assertions, registered-subclass
  runner). `Lib/test/test_errno.py` passes unmodified under JIT and AOT and is
  recorded in the manifest. Official baseline **1/398**; stdlib imports
  **25/297**; **934 tests**, clippy `-D warnings` and `scripts/verify.sh`
  green.

- 2026-10-25 (continuation 46): standard-library program + Batch A.
  Added the dedicated P7 topic to `docs/TODO.md` with all 272 missing
  modules inventoried by layer/batch and workstreams P7-01…P7-06. Implemented
  the first slice: native `bisect`/`_bisect`, `heapq`/`_heapq`, `colorsys`,
  `keyword`, `stat`/`_stat`, plus `_operator`, `_functools` and `_io`
  aliases. Stdlib imports **36/297** (10 builtin/frozen, 23 pure,
  3 extension); 261 modules remain. **968 tests**, clippy `-D warnings` and
  `scripts/verify.sh` green.

- 2026-10-25 (continuation 47): Batch B native codec/text core.
  Added native `binascii`, `base64`, `quopri`, `fnmatch`, `getopt` and
  `mimetypes` with specs/dispatch and end-to-end coverage. Stdlib imports
  **42/297** (11 builtin/frozen, 27 pure, 4 extension); 255 modules remain.
  **1011 tests**, clippy `-D warnings` and `scripts/verify.sh` green.

- 2026-10-25 (continuation 48): benchmark-critical stdlib batch.
  Added native `subprocess`, `tempfile`, `shutil`, `textwrap`, `timeit`
  (the stdlib surface used by `benchmarks/bench_all.py` and the bench
  scripts), plus `pyx run` program-argument forwarding, `sys.argv`,
  `sys.executable`, `sys.exit`, `os.environ`/`os.access` and argparse
  default-argv/help handling. Stdlib imports **47/297** (11 builtin/frozen,
  32 pure, 4 extension); 250 modules remain. `bench_all.py --only pyx`
  completes. **1064 tests**, clippy `-D warnings` and `scripts/verify.sh`
  green.

- 2026-10-25 (continuation 49): list slice assignment.
  `xs[a:b:c] = value` lowers to `StmtKind::SetSlice` (omitted bounds are
  `Const::None`) and the new `pyx_slice_set` ABI; codegen keeps Python's
  evaluation order (right-hand side, then target, then bounds). The runtime
  implements `slice.indices` bounds, step-1 splice with any iterable,
  extended-slice positional replacement (`ValueError` on length mismatch),
  an iterable right-hand side materialised before mutation, immutable
  target `TypeError`s, the dict `unhashable type: 'slice'` error and GC
  insertion barriers. Augmented slice assignment stays a clear frontend
  diagnostic. `examples/slices.py` (+ expected output and the E2E/AOT/GC
  runs) covers head/middle/tail/empty/full slices, negative bounds, extended
  steps, errors and evaluation order.

- 2026-10-25 (continuation 50): package/site-packages loader and `rich`
  progress. `check_file` now resolves packages (`__init__.py`), dotted names,
  submodules and relative imports, searching sibling files, the package-chain
  root, `PYX_SITE_PACKAGES`, the `VIRTUAL_ENV`/`CONDA_PREFIX` site-packages
  trees and `PYTHONPATH`; imported module bodies run once as initializers and
  nested imports stay deferred. The native `rich` implementation is removed
  and the official pure-Python `rich` package resolves through the loader.
  Stdlib imports **58/297** (13 builtin/frozen, 41 pure, 4 extension; 9
  modules where CPython itself fails); **239 remain**. `bench_all.py` under
  PyX is **not yet green**: the first blocker is `rich/console.py`
  (`dataclasses.field`, `os.PathLike`, ellipsis, `del`, `get_ipython`,
  `sys.stdin`/`__stdin__`/`__stdout__`/`__stderr__`, `os.get_terminal_size`/
  `open`/`dup2`, `SystemExit`, `input()`, `threading.local`), with further
  `rich` modules likely to add more. **1329 tests**, clippy `-D warnings` and
  `scripts/verify.sh` green.

- 2026-10-25 (continuation 51): rich console compatibility surface.
  Added `del` statements (name/global/attribute/subscript/slice with
  left-to-right evaluation and runtime errors), loop `else` clauses,
  `...`, `input()`, `get_ipython()`, `NameError`/`SystemExit` tags,
  `dataclasses.field` defaults/factories with keyword construction and
  `dataclasses.replace`, subscriptable `typing` aliases, `os.PathLike`/
  `devnull`/`open`/`dup2`/`get_terminal_size`, native stdin/stdout/stderr
  streams and `threading.local`. Imported-module initializer exceptions now
  stop the program with the original error instead of being overwritten by a
  later entry-body failure. `rich/console.py` compiles; native bound-method
  values and dynamic `*args`/`**kwargs` calls are the next observed blockers.
  **1368 tests**, clippy `-D warnings` and `scripts/verify.sh` green.

- 2026-10-25 (continuation 52): rich report rendering and runtime dispatch.
  Added `pyx_function_meta` (full parameter signatures/defaults and a uniform
  full-argv trampoline), `pyx_call_kw`/`CallKw`, runtime-bound class
  constructors (`pyx_instance_new`, `pyx_bind_class_kwargs`), bound method
  values, user `__call__`/`__str__`/`__repr__`/`__len__`/`__contains__`,
  `__dict__`/`__new__` support, user operator dunders, classmethod `cls(...)`
  construction, cross-unit class/function registries, nested-import linking
  and deferred-import globals, CPython-style regex class normalization plus
  lookaround stripping, callable `re.sub` and character-offset `Match`
  spans, `str.rsplit`/`partition`/`rpartition`, argparse `nargs='+'`, native
  `contextlib.suppress`/io base classes, GC rooting for container builders
  and wrapper argument buffers, and 1 MiB fiber stacks. Rich
  console/table/panel/rule output renders (with a raised GC threshold);
  `bench_all.py` is blocked at `rich.status`/`Live` native Thread
  subclassing + `super()`. **1370 tests**, clippy `-D warnings` and
  `scripts/verify.sh` green.

- 2026-10-25 (continuation 53): `bench_all.py --only pyx` runs under PyX.
  Added bound regex `Match` methods and `groupdict`, user `__getitem__`
  dispatch, a native `threading.Thread` base plus zero-argument
  `super().__init__(...)` dispatch to the direct base, native `rich.status.Status`,
  `dict.setdefault`, `str.format` named fields, extended `json.dumps`
  keywords, native `html.escape`/`unescape`, a width-valid Unicode
  `CellTable` placeholder, path-like `subprocess.cwd`, and transactional
  post-load preloading/reordering of declaration-only nested
  site-packages modules. `PYX_GC_THRESHOLD=100000000
  PYX_SITE_PACKAGES=/usr/lib/python3.14/site-packages
  target/release/pyx run benchmarks/bench_all.py --only pyx` exits 0 and
  writes the full rich report; all eleven AOT binaries build (hello AOT
  0.97 ms, while_sum 31.52 ms, for_range 15.68 ms, list_comp 2.64 ms,
  dict_insert 1.72 ms, json 3.24 ms, math_loop 49.33 ms, generator sum
  8.04 ms, async gather 21.35 ms). Default GC still loses live `Segment`
  values in the final report (P2-06). **1370 tests**, clippy
  `-D warnings` and `scripts/verify.sh` green.

- 2026-10-25 (continuation 54): full benchmark harness under PyX.
  Reran `bench_all.py` without `--only`; fixed the three runtime gaps it
  exposed: generator expressions now wrap their eager list in an iterator
  (`next(genexp)` and single-pass joins work), `*` call/method/display
  operands accept arbitrary iterables, and `str`/`bytes.join` accept
  iterators. Full suite exit 0 in 155 s (startup, CSV, request handling,
  FastAPI throughput, PyX comparison), rich report written. Default GC
  still needs the raised threshold; precise roots remain P2-06.
  **1371 tests**, clippy `-D warnings` and `scripts/verify.sh` green.
