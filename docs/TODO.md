# PyX — Work Plan & ToDos

Source: [PLAN.md](../PLAN.md). This document is the **only** work list.
Status: `[ ]` open · `[~]` in progress · `[x]` done & verified · `[!]` blocked (reason follows).

ID legend: `P<Phase>-<Nr>`. Phases are milestones, not releases.
A phase only counts as complete once its acceptance criteria (below) are green on
a fresh checkout with `./scripts/verify.sh`.

**As of 2026-10-24:** 771 tests, `scripts/verify.sh` ALL GREEN. Phase 1 is
complete; Phase 2 has real lazy generators and coroutines with a
cooperative event loop. Open are `send`/`throw`/`close`, `yield` expressions,
`yield from`, async generators/comprehensions, GC precision (P2-05/06/07) and
the remaining performance work. P4-04 is blocked in this environment
(`python3-config`/`Python.h` missing; see `docs/C_API.md`).

## Guiding decisions (derived from PLAN.md)

- D1: AOT first, JIT later (only after measurement).
- D2: Own runtime in Rust, CPython is not the default runtime.
- D3: Reuse the compiler backend and package management instead of building them anew.
- D4: Integrate `uv`, do not fork immediately; fork only on an architectural blocker.
- D5: Go-like GC as the target architecture, but correctness before performance.
- D6: Prototype C-API/ABI compatibility early; it decides between
  ecosystem adoption vs. "a new language with Python syntax".
- D7: Python compatibility is a permanent, standalone goal (CPython test suite).

## P0 — Foundation (now)

- [x] P0-01 Repository structure: Cargo workspace with `pyx-ir`, `pyx-frontend`,
      `pyx-runtime`, `pyx-codegen`, `pyx-cli`.
- [x] P0-02 Document architecture decisions (`docs/ARCHITECTURE.md`).
- [x] P0-03 Define the IR as a stable contract between frontend and codegen.
- [x] P0-04 Fix the runtime C ABI (value representation, symbols, error protocol).
- [x] P0-05 Resume protocol `AGENTS.md` (status, next steps, commands).
- [x] P0-06 Example programs + E2E test scaffold.
- [x] P0-07 `scripts/verify.sh`: fmt check, clippy, build, test, examples.
- [x] P0-08 CI (GitHub Actions): Linux x64, fmt + clippy + test + e2e.
      `.github/workflows/ci.yml`: stable Rust, Cargo cache, read-only
      permissions, concurrency cancel, `scripts/verify.sh` as the full gate.
- [~] P0-09 Set up benchmark harness (hyperfine script, CPython baseline).
      `benchmarks/pyx/run_micro.py` (CPython/JIT/AOT, RSS, build time) +
      baseline/thresholds exist; a hyperfine script for the
      existing `bench_*.py` harnesses is still missing.
- [x] P0-10 Error/diagnostic conventions (span, exit codes, stderr format).
      Stable codes (`PYXnnnn`), spans (`end_col`), help/hints, snippets,
      `--json` schema and exit codes 0/1/2 are implemented and tested.
- [x] P0-11 Backend decision: Cranelift now, evaluate LLVM behind `--features llvm`
      (see ARCHITECTURE.md §4).
- [x] P0-12 License, README, versioning (SemVer from the first runnable release).
      `README.md`, `CONTRIBUTING.md`, `CHANGELOG.md`, `LICENSE-MIT` +
      `LICENSE-APACHE` (matching `MIT OR Apache-2.0`); version 0.1.0.
      Tag/release process remains Q-08.

**Acceptance P0:** `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`,
`scripts/verify.sh` green; no undocumented contracts.

## P1 — Minimal compiler (complete)

Goal: Python-like syntax, variables, functions, control flow, type inference,
exceptions and native output. No full Python compatibility yet.

### Frontend & language semantics
- [x] P1-01 Integrate the Python parser (`rustpython-parser`), errors with line/column.
- [x] P1-02 AST → typed IR lowering: modules, functions, scopes.
- [x] P1-03 Literals: `int`, `float`, `bool`, `str`, `None`.
- [x] P1-04 Variables: assignment, augmented assignment, reading; scoping without closures.
- [x] P1-05 Operators: `+ - * / // % **`, unary `- + not`, comparison operators,
      `and`/`or` (short-circuit), `==`/`!=` including strings.
- [x] P1-06 Control flow: `if/elif/else`, `while`, `break`, `continue`, `pass`,
      `for i in range(...)` including negative step.
- [x] P1-07 Functions: `def`, parameters, `return`, recursion, default return `None`.
- [x] P1-08 Builtins: `print`, `len`, `str`, `int`, `float`, `abs`, `range` (only `for`).
- [x] P1-09 Exceptions: `raise`, `try/except [as]`, nesting, re-raise,
      runtime errors (division by 0, type errors, overflow) as Python exceptions.
- [x] P1-10 Type inference: unification over int/float/str/bool/None/Any; contradictions
      as diagnostics; `Any` fallback for dynamic paths.
- [x] P1-11 Diagnostics: `file:line:col: error: ...`, exit code 1; no panics on
      user input.
- [x] P1-12 Reject unsupported constructs cleanly (no silent miscompile).

### Runtime
- [x] P1-13 Value representation (tagged i64) + heap objects (`Str`, `Float`, `Exception`).
- [x] P1-14 Dynamic operators: int/float numerics, string concatenation/comparison.
- [x] P1-15 Truthiness, `str()` conversion, `print` with `sep`/newline.
- [x] P1-16 Exception protocol: pending exception, tags, message, uncaught traceback.
- [x] P1-17 Frame/line tracking for tracebacks (function name, line).
- [x] P1-18 i64 overflow behavior explicit (OverflowError); documentation of the limit.

### Codegen
- [x] P1-19 Cranelift IR generation for all IR statements/expressions.
- [x] P1-20 JIT path (`pyx run`), runtime symbols registered.
- [x] P1-21 AOT path (`pyx build`): object file + link against `libpyx_runtime.a`.
- [x] P1-22 Exception checks after every fallible operation; handler routing.
- [x] P1-23 Line table/frames for tracebacks; generator main as entry point.
- [x] P1-24 Deterministic error output on codegen errors.

### CLI & toolchain base
- [x] P1-25 `pyx check <file>`, `pyx run <file>`, `pyx build <file> -o <out>`.
- [x] P1-26 Exit codes: 0 ok, 1 user/program error, 2 internal error.
- [x] P1-27 `pyx run` passes through the program exit code.

### Verification P1
- [x] P1-28 E2E tests: hello, fib, loops, strings, exceptions, recursion.
- [x] P1-29 Negative tests: syntax errors, type errors, unhandled exception (traceback).
- [x] P1-30 AOT binary executable independently of the source text (smoke in tmpdir).
- [x] P1-31 `cargo test` + `scripts/verify.sh` green.

**Acceptance P1:** `pyx run examples/fib.py` and `pyx build examples/fib.py -o /tmp/fib`
produce identical, correct output; error cases produce Python-like messages;
no CPython is needed at runtime.

## P2 — Runtime

- [x] P2-01 Extend the object model: `list`, `tuple`, `dict`, `set`, `bytes`.
      `bytes` (literals, constructor, index/slice/iteration, ops, hash,
      core methods, serializer) and binary files are done;
      `bytearray`/`memoryview` remain deliberately open.
- [x] P2-02 String semantics: Unicode, indexing/slicing, methods, formatting,
      f-strings, `%` formatting.
      Format specs (`:04d`, `:.2f`, `:#x`, alignment/fill characters), `!s`/`!r`/
      `!a`, `str.format` and `%` formatting are implemented and tested via
      `pyx_format_value` and `core/format.rs`.
- [x] P2-03 Iterators/iteration protocol; `for` over containers; `enumerate`, `zip`.
      File/CSV reader, bytes and real generator iteration are present;
      `iter(x)`/`next(it[, default])` are builtins. `dict` views are
      still materialized as lists.
- [x] P2-04 Modules: import system, builtin modules, `__name__`, module cache.
      Native modules + user modules (`import foo`, `from foo import f`) with
      cache per canonical path, cycle detection, `__name__` (`__main__` in
      the entry file) and AOT embedding. Limits: only sibling `.py`,
      no packages/dots/relative imports, no top-level side effects in
      imported modules, no runtime module object/`sys.modules`.
- [ ] P2-05 Allocator: size classes, arena/chunk allocation, alignment, statistics.
      Object registry + GC heap + fixed `PyxObj` slab/free-list allocator
      (`gc/alloc.rs`, 64-slot chunks, stats) are present; variable payload
      size classes remain open.
- [~] P2-06 GC: precise roots (shadow stack/frames) before the conservative interim;
      mark & sweep correct, then concurrent (Go-like), measure pauses.
      A conservative, concurrent mark & sweep (edge barriers,
      concurrent sweep, STW root snapshot/rescan) is implemented;
      the fiber stack as well as `ctx`/`caller_ctx` (callee-saved register frames) run
      as lifetime-registered conservative roots; at run time
      `push_stack_bounds` overrides the stack bounds, the outermost resume publishes
      two aux ranges.
      Precise roots/handles/stack maps open.
- [~] P2-07 Anchor GC safepoints + stack maps or shadow stack in codegen.
      Safepoints at entry/backedges (every 64 iterations), conservative
      stack scan including fiber stack bounds; precise stack maps/shadow stack
      open.
- [x] P2-08 GC metrics (`pyx gc [--json]`), heap profiling foundation.
      `pyx gc [--json] <file.py>` runs the program like `pyx run` and
      reports collections/live_objects/threshold/pause_total_us/pause_max_us/
      mark_total_us/sweep_total_us/freed_total/stress; plus
      `gc_stats()`/`reset_gc_stats()`/`GcStats`, `PYX_GC_STATS=1`,
      `pyx_gc_collect`, `pyx_gc_objects`. Deep heap profiling remains P5-05.
- [x] P2-09 Classes/objects, methods, inheritance, `__init__`, attributes.
      Single-base classes with methods, `__init__`, inheritance/overrides,
      `pyx_getattr`/`pyx_setattr`, dynamic dispatch and deterministic
      representation. Limits: no class attributes, decorators, metaclasses,
      multiple inheritance or descriptor protocols.
- [~] P2-10 Generators, `yield`, closures/cell variables.
      Closures (nested `def`, by value) and real **lazy** generators
      (stackful fibers, `yield [value]`, bare `yield`, `return` terminates,
      exceptions propagate, `iter`/`next`) are done. Open: `send`/
      `throw`/`close`, `yield` expressions, `yield from`, mutable captures.
      Generator expressions remain eagerly materialized.
- [~] P2-11 Exceptions complete: `else`, `finally`, exception hierarchy, chaining.
      `OSError`/`IOError` additionally; exception hierarchy/chaining and
      `except*` open.
- [~] P2-12 `async`/`await`, event loop foundation.
      `async def` creates coroutine objects; `await`/`pyx_await` drives them,
      `asyncio.run` drives a cooperative single-thread loop, `sleep`
      suspends only the current fiber (blocking outside a loop),
      `gather` starts concurrent tasks and returns the
      results in input order (first error propagates).
      `async for`/`async with` still use the synchronous protocol.
      Open: async generators/comprehensions and `asyncio` beyond
      `run`/`sleep`/`gather`.
- [~] P2-13 Standard library core in Rust: `os`, `pathlib`, `json`, `re`, `math`,
      `time`, `random`; the rest via the compatibility layer.
      Present: `math`, `operator`, `functools.reduce`, `time`, `sys`, `json`
      (incl. dump/load), `os` base + remove/rename/fork/waitpid/_exit,
      `os.path`, `io` (text+bytes), `csv`, `random`, `platform`, `statistics`,
      `pathlib.Path`, `argparse` (subset), `collections.defaultdict`,
      `re` (Rust `regex`), `socket` (TCP), `asyncio` (`run`/`sleep`/`gather`).
      Broad CPython stdlib, TLS/UDP, non-blocking I/O, complete
      `argparse` and pytest compatibility open.
- [ ] P2-14 Interpreter/bytecode mode for REPL and fast dev loops (optional).
- [x] P2-15 Document and implement the threading/GIL decision.
      Decision: **no GIL**, shared heap + concurrent GC
      (ARCHITECTURE §7.1); threads/locks/queue/processes implemented.
- [ ] P2-20 Concurrency: real multithreading & multiprocessing
      (GIL-free is the goal, but correctness first).
      - [x] P2-20a Shared heap/GC design for threads (heap currently thread-local;
        cross-thread values, synchronization, safepoints) or a deliberate GIL.
        Shared heap + concurrent mark & sweep (STW root snapshot and
        rescan, concurrent marking with insertion barriers,
        white partition under heap lock, free outside, growth-based
        threshold, PYX_GC_STATS). No GIL; shared containers need
        user locks. Precise roots/allocator (P2-05/06) and finer
        safepoint polling remain open.
      - [x] P2-20b `threading` subset: Thread, Lock, RLock, Event, Queue;
        thread-local exception/frame states, thread spawn/join.
        Thread/Lock/RLock/Event/queue.Queue implemented; `_thread.join` remains
        a PyX extension; blocking waits park GC-safely.
      - [x] P2-20c Process API: `multiprocessing` (Process, Queue, Pool),
        `os.fork`/spawn, waitpid, exit codes; real parallel processes.
        Process/Queue/fork/waitpid/_exit, real spawn (exec,
        function registry, payload/queue FDs, no module body in the child),
        Pool (fork+spawn, per-worker queues, context manager) and
        Process/Pool.terminate.
      - [x] P2-20d Serialization for process boundaries (pickle-compatible or
        own format), incl. function references.
        Own PYX1 format (`pickle` = hex, mp.Queue = binary;
        spawn = function by name, queue FDs). Not CPython-compatible.
      - [x] P2-20e Parallel benchmarks: thread scaling, process scaling,
        memory/startup cost vs. CPython; CI thresholds.
        `threads_serial`/`threads_parallel`, `pool_serial`, `pool_parallel`,
        `pool_map`, `process_queue` are in the micro harness, in
        `results/latest.{json,md}` and in `thresholds.json`. The thresholds
        run locally (`scripts/check_benchmarks.py`), not in CI
        (runner noise; Q-06).
      - [~] P2-20f Race/stress tests, fuzzing, deadlock/shutdown behavior.
        Concurrent GC/thread/pool stress tests and deadlock watchdogs
        present; fuzzing (Q-03/Q-04) open.
- [x] P2-17 Comprehensions: list/set/dict + generator expressions, multiple
      `for` clauses, `if` filters, tuple targets, shadowing without leaks.
      Generator expressions are eagerly materialized as a list
      (documented); function generators are lazy since P2-10.
- [x] P2-18 First-class functions: function values, lambdas (without closures for
      lambdas), indirect calls, `map`/`filter` (materialized lists);
      builtin function values (`f = int`), closure, default and
      star-parameter diagnostics.
- [~] P2-16 Runtime unit tests + property tests for containers/GC.
      As of 2026-10-24: 771 tests total — 353 runtime (180 unit, 173 ABI
      integration: among others 22 bytes, 19 IO, 14 stdlib modules, 14 stdlib utils,
      12 classes, 12 pathlib, 9 native concurrency, 8 CSV, 7 socket,
      6 attrs, 6 GC, 6 JSON, 5 concurrent GC, 5 pool, 5 threads, 5 OS path,
      5 closures, 3 STW GC, 1 JSON GC), 244 frontend, 17 codegen, 3 IR,
      154 CLI/E2E (134 E2E, 9 fibers, 6 closures, 5 user modules).
      Property tests open.

**Acceptance P2:** memory does not grow unboundedly in loop benchmarks; GC tests
cover cycles; container semantics tested against CPython behavior.

## P3 — Toolchain (uv)

- [x] P3-01 `pyx new`/`init`, `pyx add`, `pyx remove`, `pyx sync`, `pyx lock`.
      `new`/`init` create `main.py`, `pyproject.toml`, `.gitignore`;
      add/remove/sync/lock are an unchanged passthrough to `uv`
      (`PYX_UV`), including the exit status.
- [~] P3-02 Keep `pyproject.toml`/lockfile behavior compatible with `uv`.
      Scaffold and passthrough present; PyX does not interpret the files,
      uv remains the owner. Own lockfile/venv semantics open.
- [~] P3-03 Integrate uv as a library/process; redirect interpreter selection to
      `pyx` (`.python-version`, venv marker, `sys.executable` equivalent).
      Process passthrough implemented; interpreter injection/venv marker/
      `sys.executable` equivalent open.
- [x] P3-04 Build cache: hash over source + flags + runtime version, incremental.
      `.pyx/cache`, deterministic key over source, reachable
      modules, target, version and runtime archive; `--no-cache`, `pyx clean`,
      hit/invalidation tests.
- [ ] P3-05 Reproducible builds (deterministic objects, path stripping).
- [x] P3-06 `pyx test`, `pyx fmt`, `pyx lint`, `pyx check`.
      Implemented; `test` is **not** pytest-compatible (success = exit 0,
      `--json`), `fmt` only normalizes whitespace/final newline.
- [x] P3-07 uv fork only if the architecture forces it (decision doc).
      Decision: no fork; passthrough (ARCHITECTURE §8).
- [ ] P3-08 Packaging: native binaries, cross-compilation, installers.

**Acceptance P3:** a foreign Python project with `pyproject.toml` can be started with
`pyx sync && pyx run` (defined support scope). *Not yet
met*: without uv interpreter injection/packaging this remains a goal.

## P4 — Python compatibility

- [ ] P4-01 CPython test suite subset as harness; progress tracking.
- [ ] P4-02 Language features complete: decorators, comprehensions, pattern matching,
      descriptors, metaclasses, `__dunder__` protocols.
      Partial: classes (single-base), comprehensions, closures, `with`.
- [ ] P4-03 Standard library broad (pure Python + Rust replacement).
      Curated core present (P2-13); broad coverage open.
- [!] P4-04 C API strategy spike: embedding CPython (`libpython`) vs.
      reimplementation vs. limited compatibility; ABI3/NumPy smoke test.
      **Blocked (2026-10-24):** the current environment lacks
      `python3-config`, `Python.h` and a linkable `libpython3.12.so`/`.a`
      (only the runtime SO `libpython3.12.so.1.0` is present). Without
      development headers neither embedding nor an ABI3 shim can be compiled.
      Options/probe/spike plan: `docs/C_API.md`. There are **no** measurements
      and **no** embed-vs-shim decision has been made.
- [ ] P4-05 Native extension loading (`.so`) + CPython ABI shim. Depends on P4-04.
- [ ] P4-06 `ctypes`, define `cffi`-like boundaries. Depends on the P4-04 option.
- [~] P4-07 Pickle/serialization, encoding details, numeric limits (i64 vs BigInt).
      PYX1 is deliberately not CPython-compatible; `bytes`/UTF-8/string escapes
      tested; i64 limit documented. CPython `pickle` compatibility open.
- [ ] P4-08 Publish compatibility matrix (what runs, what does not).
      README/ARCHITECTURE document the scope; a matrix per module/
      feature is missing.

**Acceptance P4:** defined, tested language/stdlib scope; documented
C API limits; no silent semantic deviations.

## P5 — Performance & JIT

- [~] P5-01 Benchmarks vs. CPython/PyPy/Go (startup time, throughput, memory).
      Readiness matrix `benchmarks/PYX.md`; PyX micro harness and baseline
      (`benchmarks/pyx/results/latest.{json,md}`), incl. comprehensions,
      map/filter, lambda calls, reduce, math loop, JSON dumps/loads and
      thread/process/pool scaling. Additionally `benchmarks/bench_pyx.py`
      (11 workloads incl. lazy generator sum and async `gather`, output-
      equality gate) as `pyx` in `bench_all.py`. PyPy/Go comparisons open.
- [~] P5-01a PyX adapter: run the existing `bench_*.py` with PyX as the implementation
      (subprocess/`PYX_BIN`), without modifying the user harnesses.
      Micro harness `benchmarks/pyx/run_micro.py` (CPython/JIT/AOT) and
      `benchmarks/bench_pyx.py` (same pure Python workloads under three
      runners); adapter for the large harnesses still open.
- [~] P5-01b Benchmark readiness per benchmark: loops (comprehensions/lambdas/
      builtins), csv (modules/IO/with), startup (AOT/JIT subprocess),
      request (async/socket/json), fastapi (framework/C API).
      Matrix in `benchmarks/PYX.md`; loops/startup (JIT/AOT) measurable,
      csv partially; request has real coroutines/event loop, but no
      non-blocking I/O yet; fastapi remains blocked by C API/decorators.
- [x] P5-01c CI regression thresholds + baseline artifacts for PyX.
      Baseline artifacts + `benchmarks/pyx/thresholds.json` +
      `scripts/check_benchmarks.py` as a local gate (deliberately not in CI,
      runner noise); `bench_all.py` resolves its output directory absolutely,
      AOT builds are excluded from the measured runs and
      deviating outputs result in exit code 1. Threshold enforcement in
      CI remains Q-06.
- [x] P5-01e Parallel benchmarks (after P2-20): threads/processes, scaling,
      startup cost, memory; CPython/PyX comparison.
      `threads_serial`, `threads_parallel`, `pool_serial`, `pool_parallel`,
      `pool_map`, `process_queue` (JIT/AOT/CPython, RSS/startup time).
- [ ] P5-02 PGO + LTO for AOT. (`lto = "thin"` is set in the profile;
      PGO/LLVM LTO measurement open.)
- [~] P5-03 Inlining, constant folding, escape analysis, devirtualization.
      Constant folding (int/float/str/binops/comparisons/unary, overflow- and
      zero-division-protected), dead `if`/`while False` and typed
      int fast paths (`+`/`-`/`*`, comparisons, AugAssign, fully
      int-typed `for range` counters; checked overflow →
      `pyx_binop` slow path) are implemented. Measured (release,
      n=200000): while_sum 8.6x JIT / 11.9x AOT versus CPython,
      for_range_sum 7.0x/13.1x. The x86_64 fiber switch is a
      register-only `global_asm!` switch without syscalls; a
      100k generator sum runs in release with CPython 11.0 / JIT 7.7 / AOT
      6.8 ms (`strace -c`: 94 syscalls total). Inlining/escape analysis/
      devirtualization open; safepoints at backedges remain measurable.
- [~] P5-04 GC tuning based on measurements (pauses, allocation rate).
      Growth-based threshold, relaxed-safepoint fast path,
      edge barriers, concurrent sweep, `PYX_GC_STATS`, `gc_stats()`/
      `GcStats` and `pyx gc [--json]` provide pause/mark/sweep times.
      The dict hash index lowers insert/lookup costs (dict_insert AOT 2.1 ms
      vs. CPython 12.7 ms). Fiber `ctx`/`caller_ctx` are
      lifetime-registered roots; shared mode mirrors the two aux ranges,
      local mode gets by without atomics per switch. Precise roots/allocator
      (P2-05/06) and pause tuning remain open.
- [ ] P5-05 Profiling (CPU/heap) + trace integration.
- [ ] P5-06 JIT decision **only** with measurement data; otherwise discard.
- [ ] P5-07 If JIT: LLVM ORC or Cranelift, tiering, deopt strategy.

**Acceptance P5:** measurable, documented improvement without correctness regression.

## Cross-cutting (parallel, permanent)

- [ ] Q-01 Debugger integration (DAP), source maps/DWARF.
- [~] Q-02 Python-like tracebacks incl. chained exceptions; stable error codes.
      Tracebacks and stable codes present; exception chaining open.
- [ ] Q-03 Fuzzing: parser, lowering, codegen (cargo-fuzz).
- [ ] Q-04 Fuzzing: runtime/GC (allocation/object operations).
- [ ] Q-05 Security review of `unsafe` code; policy in ARCHITECTURE.md.
      `unsafe` is encapsulated and justified in GC/ABI; formal review open.
- [ ] Q-06 Performance regression CI (thresholds).
- [x] Q-07 Documentation: language scope, CLI, architecture, contribution.
      README/ARCHITECTURE/LAYOUT/C_API/TODO/AGENTS/CHANGELOG updated;
      CLI reference/diagnostic catalog remain P6-09.
- [~] Q-08 Release process, changelog, SemVer.
      Changelog (Unreleased/0.1.0) and SemVer scheme documented;
      tag/release process open.
- [ ] Q-09 Cross-platform (macOS, Windows, aarch64) — after Linux stability.
- [~] Q-10 Telemetry-free defaults; build reproducibility.
      No telemetry; reproducible builds (P3-05) open.

## P6 — Polishing & developer experience (cargo-like)

Goal: `pyx` should feel like a single, complete tool — not like
a collection of scripts. Errors are first-class products, not an afterthought.

- [~] P6-01 First-class diagnostics: stable codes (`PYXnnnn`), spans,
  source snippets with carets, help/hints (did-you-mean), multiple errors,
  summary and consistent exit codes.
      Codes/spans/snippets/help/summary/exit codes implemented;
      sema carets are partly 1 character, ICE path not yet tested.
- [x] P6-02 Machine-readable diagnostics: `--json`/message format with a stable
  schema (file/line/col/end_col/code/message/help) — implemented & tested.
- [~] P6-03 Unified CLI UX: `new`, `init`, `check`, `run`, `build`, `test`,
  `fmt`, `lint`, `clean`, `doctor`, `add`, `remove`, `sync`, `lock` from one
  binary; clear help, `--verbose`/`--quiet`, `--color`.
      All named subcommands, help, `--color`/`--json` and defined
      exit codes are implemented. **Open:** `--verbose`/`--quiet`.
- [~] P6-04 Build cache & incremental builds (`.pyx/cache`), reproducible,
  cache invalidation via source + flags + runtime version.
      Cache incl. module tree fingerprint, `--no-cache`, `pyx clean` and
      hit/invalidation tests implemented. Reproducible binaries
      (deterministic objects/path stripping) remain P3-05.
- [~] P6-05 ICE handling: internal panics as `internal compiler error` with
  version/target/hint, exit code 2, `RUST_BACKTRACE` hint; no panics
  on user input (fuzzing P6-08).
      `catch_unwind` + ICE banner; regression test open.
- [x] P6-06 Project scaffolding/templates (`pyx new`, `pyx init`) so that a
  project is ready to go without third-party tools; `pyproject.toml` compatibility.
      Scaffold (`main.py`, `pyproject.toml`, `.gitignore`) with `--force`
      policy; `pyproject.toml` remains uv-compatible.
- [ ] P6-07 Shell completions, manpage, `--version --verbose` (target, LLVM/
  Cranelift, runtime version).
- [~] P6-08 Diagnostic regression tests: codes, spans, JSON schema, colors/
  TTY behavior, exit codes, ICE path.
      E2E covers human/JSON/color/doctor/caret/syntax code; ICE and
      detail-span tests open.
- [~] P6-09 Documentation: CLI reference, diagnostic catalog, troubleshooting guide,
  migration from CPython.
      README/ARCHITECTURE/LAYOUT/C_API/TODO updated; a dedicated
      CLI reference/diagnostic catalog open.
- [ ] P6-10 Build feedback: timing, `--verbose`, progress, exit-code summary.

**Acceptance P6:** a user can initialize, build, run and diagnose a project with
`pyx` without knowing other tools; every error class has a
code, span, help and stable exit code; `--json` is documented and tested.

## Risks / open decisions

- R1: C-API/ABI compatibility is the biggest cost driver (PLAN.md §3).
  P4-04 is currently **environment-blocked** (no CPython dev headers/
  `python3-config`); the result determines object model/refcount design.
- R2: i64 instead of BigInt — documented limit; BigInt later behind a feature.
- R3: Cranelift instead of LLVM: faster startup, less optimization; LLVM optional.
- R4: GC with native frames needs precise roots; conservative scanning is the
  interim (P2-06/P2-07). Fiber stack and the two context register frames
  have been lifetime-registered conservative roots since Phase 2.32 (with
  stack-bounds override); precise stack maps remain open.
- R5: `uv` interpreter coupling may force a fork; currently passthrough,
  decision documented (no fork without a blocker).
- R6: Dynamic features (monkey-patching, `eval`, metaclasses) vs. AOT — tiering
  or an explicit support scope needed.
- R7: Generator/coroutine remainder: `send`/`throw`/`close`, `yield` expressions,
  `yield from`, async generators/comprehensions. Generator expressions
  remain eagerly materialized; `async for`/`async with` use the synchronous
  protocol.
- R8: Performance weak spot: native module dispatch (math_loop ~0.8x).
  The x86_64 fiber switch is register-only; the 100k generator sum is
  AOT 6.8 / JIT 7.7 ms in release versus CPython 11.0 ms. No
  optimization claims without measurement.
