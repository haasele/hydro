# Hydro — Work Plan & ToDos

Source: [PLAN.md](../PLAN.md). This document is the **only** work list.
Status: `[ ]` open, scoped program · `[x]` done & verified · `[!]` blocked (reason follows).

ID legend: `P<Phase>-<Nr>`. Phases are milestones, not releases.
A phase only counts as complete once its acceptance criteria (below) are green on
a fresh checkout with `./scripts/verify.sh`.

**As of 2026-10-26 (latest):** CPython **3.14.9** compatibility target, `scripts/verify.sh` ALL GREEN. The current test count and per-crate breakdown live in [`AGENTS.md`](../AGENTS.md#current-state).

**2026-10-26 four-scope checkpoint.** Delivered and tested: a runtime module
registry (`sys.path`, `sys.modules`, module `__name__`, namespace packages,
`from x import *`/`__all__`, `HYDRO_STDLIB`), the full 3.14 exception
taxonomy + `isinstance`/`issubclass`/exception attributes, the core
builtins/frozen shims (`builtins`, `_imp`, `_opcode`, `_collections`,
`_codecs`, `_string`, `_weakref` with callbacks, `warnings`, `marshal`,
`_tokenize`, `_collections_abc`, `gc`, `atexit`, `site`), a vendored
`hydro-python-parser` with PEP 701 f-strings and PEP 750 t-strings
end-to-end, the P4-02 lowering/runtime surface (decorators, positional-only,
`*args`+`**kwargs`, `match`, `except*` syntax with explicit runtime gap,
`classmethod`/`staticmethod`/`property`, bytearray/memoryview, dict views,
descriptors, C3 multiple inheritance, metaclasses, `super()`), the next
full-ABI layer (`PyObject_HEAD`/`PyTypeObject`, vectorcall, buffers, GIL,
GC tracking, constants/keywords), and `tea install` for wheels with an
abi3 smoke. **Still open and blocking full parity:** runtime dynamic
imports/`compile`/`eval`/`globals`/`locals`/`vars`, class-body statements
such as `del`, `builtins.__dict__`, `ast.parse/unparse`, `except*` grouping,
and the NumPy/pandas ABI gate; the official CPython baseline has not been
regenerated after this slice. Full P7/P4-10 remains open.
Phase 1 is complete; Phase 2 is complete. It includes lazy/async generators,
coroutines and task groups, precise shadow-stack roots (strict mode
`HYDRO_GC_PRECISE=1`), the size-classed payload allocator, argparse/pytest,
TLS/UDP/select networking, and the IR interpreter/REPL. Remaining work is P7
stdlib/ecosystem parity and P5 performance tuning (PGO/inlining). **P4-04 is unblocked and decided:** CPython
3.14 development files are available, and the project follows **option B** —
a Hydro-native CPython C-API/ABI layer without embedded `libpython`; evidence and
gates in [`docs/C_API.md`](C_API.md). The **first abi3 slice is implemented**:
a limited-API extension (`.so`) next to the source can be imported and called
under JIT and AOT without `libpython` (limited ABI slice; P4-04a/b). The
**P4-09/P4-10/P4-11** program tracks full stdlib parity (297 modules),
ecosystem suites (NumPy, pandas, pydantic, FastAPI, Django, …) and their
benchmarks. **New in this slice:** generator `send`/`throw`/`close` and
`yield` expressions; C-API container/unicode/float/bytes accessors; a
generated stdlib compatibility baseline (`docs/COMPATIBILITY.md`,
60/297 modules importable on 2026-10-25 with Batch A, the Batch B native
core, the benchmark-critical batch, the package/site-packages loader and the
new native `typing`/`abc`/`enum`/`types`/`itertools`/`ast`/`fractions`/
`importlib`/`__future__` modules) and the first official CPython test pass
(`test_errno.py`). `benchmarks/bench_all.py --only hydro` now renders the rich
report at the default GC threshold; the full-suite report is written under
Hydro. Full P7 stdlib/ecosystem parity remains open.

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

- [x] P0-01 Repository structure: Cargo workspace with `hydro-ir`, `hydro-frontend`,
      `hydro-runtime`, `hydro-codegen`, `tea`.
- [x] P0-02 Document architecture decisions (`docs/ARCHITECTURE.md`).
- [x] P0-03 Define the IR as a stable contract between frontend and codegen.
- [x] P0-04 Fix the runtime C ABI (value representation, symbols, error protocol).
- [x] P0-05 Resume protocol `AGENTS.md` (status, next steps, commands).
- [x] P0-06 Example programs + E2E test scaffold.
- [x] P0-07 `scripts/verify.sh`: fmt check, clippy, build, test, examples.
- [x] P0-08 CI (GitHub Actions): Linux x64, fmt + clippy + test + e2e.
      `.github/workflows/ci.yml`: stable Rust, Cargo cache, read-only
      permissions, concurrency cancel, `scripts/verify.sh` as the full gate.
- [x] P0-09 Set up benchmark harness (hyperfine script, CPython baseline).
      `benchmarks/hydro/run_micro.py` (CPython/JIT/AOT, RSS, build time) + the
      tracked baseline/thresholds; `scripts/bench_hyperfine.sh` wraps the
      existing `bench_*.py` harnesses with output validation and clean
      skip/error behavior when hyperfine or uv is missing.
- [x] P0-10 Error/diagnostic conventions (span, exit codes, stderr format).
      Stable codes (`HYDnnnn`), spans (`end_col`), help/hints, snippets,
      `--json` schema and exit codes 0/1/2 are implemented and tested.
- [x] P0-11 Backend decision: Cranelift now, evaluate LLVM behind `--features llvm`
      (see ARCHITECTURE.md §4).
- [x] P0-12 License, README, versioning (SemVer from the first runnable release).
      `README.md`, `CONTRIBUTING.md`, `CHANGELOG.md`, `LICENSE-MIT` +
      `LICENSE-APACHE` (matching `MIT OR Apache-2.0`); version 0.1.0.
      Tag-driven releases are Q-08.

**Acceptance P0:** `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`,
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
- [x] P1-18 Integer overflow behavior explicit: tagged immediates promote to `BigInt`; native i64 boundaries documented.

### Codegen
- [x] P1-19 Cranelift IR generation for all IR statements/expressions.
- [x] P1-20 JIT path (`tea infuse`), runtime symbols registered.
- [x] P1-21 AOT path (`tea build`): object file + link against `libhydro_runtime.a`.
- [x] P1-22 Exception checks after every fallible operation; handler routing.
- [x] P1-23 Line table/frames for tracebacks; generator main as entry point.
- [x] P1-24 Deterministic error output on codegen errors.

### CLI & toolchain base
- [x] P1-25 `tea check <file>`, `tea infuse <file>`, `tea build <file> -o <out>`.
- [x] P1-26 Exit codes: 0 ok, 1 user/program error, 2 internal error.
- [x] P1-27 `tea infuse` passes through the program exit code.

### Verification P1
- [x] P1-28 E2E tests: hello, fib, loops, strings, exceptions, recursion.
- [x] P1-29 Negative tests: syntax errors, type errors, unhandled exception (traceback).
- [x] P1-30 AOT binary executable independently of the source text (smoke in tmpdir).
- [x] P1-31 `cargo test` + `scripts/verify.sh` green.

**Acceptance P1:** `tea infuse examples/fib.py` and `tea build examples/fib.py -o /tmp/fib`
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
      `hydro_format_value` and `core/format.rs`.
- [x] P2-03 Iterators/iteration protocol; `for` over containers; `enumerate`, `zip`.
      File/CSV reader, bytes and real generator iteration are present;
      `iter(x)`/`next(it[, default])` are builtins. `dict` views are
      still materialized as lists.
- [x] P2-04 Modules: import system, builtin modules, `__name__`, module cache.
      Native modules + user modules (`import foo`, `from foo import f`) with
      cache per canonical path, cycle detection, `__name__` (`__main__` in
      the entry file) and AOT embedding. Packages (`__init__.py`, dotted and
      relative imports), site-packages resolution and one-time module
      initializers work (P7-01). Stdlib modules are first-class values
      (`m = errno`, `getattr(m, name)`); module-level assignments are visible
      inside functions via a keyed global store. Limits: user-module values are
      rejected; no general runtime module object/`sys.modules` or
      multiple-module `from x import *` (P4-03/P7-01).
- [x] P2-05 Allocator: size classes, arena/chunk allocation, alignment, statistics.
      Delivered: split 16-byte `HydroObj` header and typed payload blocks;
      18 size classes (8..4096 bytes) with per-class slabs/free lists, a large
      exact-allocation fallback, 16-byte alignment, `absorb`/`clear` across
      classes, and per-class/peak-byte statistics. `ObjData` remains the
      construction descriptor; typed `ObjDataRef`/`ObjDataMut` accessors and
      kind-dispatched drop preserve the collector contract. Tests cover class
      selection, alignment, per-class reuse, large fallback, clear/absorb,
      mixed-payload reclamation and bounded RSS; all objects still enter through
      the registry.
- [x] P2-06 GC: precise roots (shadow stack/frames) replacing the conservative interim;
      concurrent mark & sweep, then measure pauses.
      Delivered: a conservative, concurrent mark & sweep (edge barriers,
      concurrent sweep, STW root snapshot/rescan). The fiber stack as well as
      `ctx`/`caller_ctx` (callee-saved register frames) are lifetime-registered
      conservative roots; at run time stack bounds derive from the running
      fiber, and the native caller stack is published once as an aux range.
      Program:
      precise root sets for JIT/AOT frames (shadow stack or stack maps) and
      handles, with no conservative retainer of dead values. Acceptance:
      `tests/concurrent_gc.rs`/`stw_gc.rs` stay green under heap pressure;
      `bench_all.py` runs at the default GC threshold; a stress program with
      known dead temporaries does not retain them across collections.
      Delivered: `gc/roots.rs` shadow frames/ranges published at safepoints,
      persistent root slots, `HYDRO_GC_PRECISE=1` strict mode, and
      `tests/precise_gc.rs` (frame/scope/stale-stack/persistent/shared).
- [x] P2-07 Anchor GC safepoints + stack maps or shadow stack in codegen.
      Delivered: safepoints at entry/backedges (every 64 iterations) and
      conservative stack scanning including fiber stack bounds. Program:
      codegen-emitted stack maps or a shadow stack for every safepoint.
      Acceptance: the collector reads only precise roots at safepoints;
      P2-06's stress gate and the GC tests pass with conservative scanning
      disabled; codegen has no unsafepoint call into allocating runtime code.
- [x] P2-08 GC metrics (`tea gc [--json]`), heap profiling foundation.
      `tea gc [--json] <file.py>` runs the program like `tea infuse` and
      reports collections/live_objects/threshold/pause_total_us/pause_max_us/
      mark_total_us/sweep_total_us/freed_total/stress; plus
      `gc_stats()`/`reset_gc_stats()`/`GcStats`, `HYDRO_GC_STATS=1`,
      `hydro_gc_collect`, `hydro_gc_objects`. Deep heap profiling remains P5-05.
- [x] P2-09 Classes/objects, methods, inheritance, `__init__`, attributes.
      Single-base classes with methods, `__init__`, inheritance/overrides,
      `hydro_getattr`/`hydro_setattr`, dynamic dispatch, frozen class attributes,
      `@property`/`@classmethod`/`@staticmethod`, and deterministic
      representation. Metaclasses, multiple inheritance and the general
      descriptor protocol are delivered; remaining language gaps are
      tracked under P4-02.
- [x] P2-10 Generators, `yield`, closures/cell variables.
      Delivered: closures with mutable cell captures and `nonlocal`, real **lazy**
      generators (stackful fibers, `yield [value]`, bare `yield`, `return`
      terminates, exceptions propagate, `iter`/`next`, `send`/`throw`/`close`,
      `yield` expressions and `yield from`) and lazy generator expressions
      (a synthetic generator function: the outermost iterable is evaluated at
      creation, the remaining clauses/filters and the element run per resume,
      and enclosing locals are capture parameters; an assignment expression
      inside one is rejected unless its target is declared `global` in the
      enclosing scope, because it would otherwise have to bind in the
      enclosing frame). Cells are GC-traced; outer/inner mutations are shared, sibling
      closures share a cell, and loop-created closures get per-iteration cells.
      Tests cover mutation, `nonlocal`, missing-binding diagnostics, GC stress,
      JIT+AOT, laziness/partial consumption and exception propagation.
- [x] P2-11 Exceptions complete: `else`, `finally`, exception hierarchy, chaining.
      `except Exception`/`BaseException`, tuple handlers
      (`except (A, B) as e`), explicit `raise ... from ...` and implicit
      `__context__` chaining (`__suppress_context__`, "During handling of the
      above exception ...") are implemented with deepest-first
      tracebacks. `except*` remains a language-completeness item under P4-02.
- [x] P2-12 `async`/`await`, event loop foundation.
      Delivered: `async def` creates coroutine objects; `await`/`hydro_await`
      drives them, `asyncio.run` drives a cooperative single-thread loop on a
      persistent thread-local driver whose scheduled-task list is a heap
      persistent root, `sleep` suspends only the current fiber (blocking
      outside a loop), and `gather` starts concurrent tasks and returns the
      results in input order (first error propagates). Async generators are
      real stackful fibers selected by codegen from the async+generator flags,
      with `__aiter__`/`__anext__`/`asend`/`athrow`/`aclose`,
      `StopAsyncIteration` (tag 18) at completion, `async for`/`async with`
      lowering, and async comprehensions/generator expressions. Tasks expose
      `cancel`/`done`/`cancelled`/`result`/`exception`; cancellation is
      delivered as `CancelledError` (tag 19, a `BaseException` excluded from
      `Exception`) at the next suspension resume, including sleeps and awaits,
      with leftover tasks cancelled at `run` shutdown and a reentrant `run`
      rejected. `asyncio.TaskGroup` joins its `create_task` tasks, cancels
      siblings on the first failure, cancels remaining work when the block
      raises and propagates the first error (no `ExceptionGroup`; `wait_for`
      remains unsupported). Fiber fields are traced, `await` chains are
      depth-bounded, and `asyncio::reset` drops the loop from the collector's
      reset hook. Tests cover the protocol methods, GC stress across a
      suspended async generator, cancellation at/before a suspension,
      TaskGroup success/interleaving/error/body-error cancellation, the
      persistent loop, and JIT+AOT examples.
- [x] P2-13 Standard library core in Rust: `os`, `pathlib`, `json`, `re`, `math`,
      `time`, `random`; the rest via the compatibility layer.
      Delivered: `math`, `operator`, `functools.reduce`, `time`, `sys`, `json`
      (incl. dump/load), `os` base + remove/rename/fork/waitpid/_exit,
      `os.path`, `io` (text+bytes), `csv`, `random`, `platform`, `statistics`,
      `pathlib.Path`, `argparse`, `collections.defaultdict`, `re` (Rust
      `regex`), `asyncio`, and native `socket` with TCP/UDP/IPv6, TLS,
      non-blocking I/O, `select`/`poll`/`selectors`. The final P2-13 slice adds
      catchable `SystemExit`/`sys.exit`, the complete argparse surface and a
      pytest-compatible runner. `tea/tests/stdlib_core.rs` exercises
      representative math/time/random/json/re/os/pathlib/io behavior plus error
      paths, so no module is counted from import alone. Full 297-module/P7
      parity remains the separate P7 program; it is not claimed here.
- [x] P2-14 Interpreter/bytecode mode for REPL and fast dev loops (optional).
      Delivered the IR tree-walk interpreter (`crates/hydro-interp`) used by
      `tea infuse --interpret`; it covers every IR node, shares the runtime ABI,
      registers precise roots for frames/temporaries, and materialises IR
      functions as host callables for native callbacks. `tea repl`, `tea -c`
      and `tea -i` provide a persistent session with multi-line buffering,
      expression echo, `_`, import-once and global flush. A separate bytecode
      VM was deliberately not added: the IR interpreter is the second execution
      path and avoids duplicating semantics; `dis`/code objects remain P7.
- [x] P2-15 Document and implement the threading/GIL decision.
      Decision: **no GIL**, shared heap + concurrent GC
      (ARCHITECTURE §7.1); threads/locks/queue/processes implemented.
- [x] P2-20 Concurrency: real multithreading & multiprocessing
      (GIL-free is the goal, but correctness first).
      - [x] P2-20a Shared heap/GC design for threads (heap currently thread-local;
        cross-thread values, synchronization, safepoints) or a deliberate GIL.
        Shared heap + concurrent mark & sweep (STW root snapshot and
        rescan, concurrent marking with insertion barriers,
        white partition under heap lock, free outside, growth-based
        threshold, HYDRO_GC_STATS). No GIL; shared containers need
        user locks. Precise roots and the size-classed allocator are now
        implemented (P2-05/06); optional finer safepoint polling and codegen
        stack maps remain future work.
      - [x] P2-20b `threading` subset: Thread, Lock, RLock, Event, Queue;
        thread-local exception/frame states, thread spawn/join.
        Thread/Lock/RLock/Event/queue.Queue implemented; `_thread.join` remains
        a Hydro extension; blocking waits park GC-safely.
      - [x] P2-20c Process API: `multiprocessing` (Process, Queue, Pool),
        `os.fork`/spawn, waitpid, exit codes; real parallel processes.
        Process/Queue/fork/waitpid/_exit, real spawn (exec,
        function registry, payload/queue FDs, no module body in the child),
        Pool (fork+spawn, per-worker queues, context manager) and
        Process/Pool.terminate.
      - [x] P2-20d Serialization for process boundaries (pickle-compatible or
        own format), incl. function references.
        Own HYD1 format (`pickle` = hex, mp.Queue = binary;
        spawn = function by name, queue FDs). Not CPython-compatible.
      - [x] P2-20e Parallel benchmarks: thread scaling, process scaling,
        memory/startup cost vs. CPython; CI thresholds.
        `threads_serial`/`threads_parallel`, `pool_serial`, `pool_parallel`,
        `pool_map`, `process_queue` are in the micro harness, in
        `results/latest.{json,md}` and in `thresholds.json`. Thresholds are
        enforced locally (`scripts/check_benchmarks.py`) and reported
        non-blockingly in CI (runner noise; P5-01c/Q-06).
      - [x] P2-20f Race/stress tests, fuzzing, deadlock/shutdown behavior.
        Delivered: concurrent GC/thread/pool stress tests and deadlock
        watchdogs (`concurrent_gc.rs`, `stw_gc.rs`, `pool.rs`,
        `native_concurrency.rs`); libFuzzer plus a stable fallback harness
        (`fuzz/`, `scripts/fuzz-smoke.sh`), bounded CI job, and a parallel
        multi-core soak script (`scripts/fuzz-parallel.sh`). Shutdown now has
        Drop hooks for unreachable started processes, pools and multiprocessing
        queue endpoints; `tests/shutdown.rs` runs 11 active tests, including
        child-free checks through `/proc`. Fuzzing found and fixed a
        cyclic-container repr stack overflow plus unrooted list-builder paths
        (splitlines, `list.extend` over generators, instance `__dict__`).
        The 24 h campaign command remains documented; CI runs the bounded
        corpus, not a 24 h job.
- [x] P2-17 Comprehensions: list/set/dict + generator expressions, multiple
      `for` clauses, `if` filters, tuple targets, shadowing without leaks.
      List/set/dict comprehensions stay eager containers; generator
      expressions are lazy since P2-10 (synthesized generator function over
      the fiber ABI).
- [x] P2-18 First-class functions: function values, lambdas (without closures for
      lambdas), indirect calls, `map`/`filter` (materialized lists);
      builtin function values (`f = int`), closure, default and
      star-parameter diagnostics.
- [x] P2-16 Runtime unit tests + property tests for containers/GC.
      The current total and per-crate breakdown are maintained in
      [`AGENTS.md`](../AGENTS.md#current-state); do not duplicate them here.
      Deterministic property tests use a SplitMix64 model without new
      dependencies: `tests/container_properties.rs` (list/dict/set/tuple
      operations vs. Vec/HashMap/HashSet models) and `tests/gc_properties.rs`
      (randomized churn, reachable-graph tracing, allocation/collection
      invariants, cycle reclamation). Fuzzing remains Q-03/Q-04. The CPython
      official-test manifest is checked offline by
      `scripts/verify.sh`/CI; the full suite is a release gate.

**Acceptance P2:** memory does not grow unboundedly in loop benchmarks; GC tests
cover cycles; container semantics tested against CPython behavior.

## P3 — Toolchain (uv)

- [x] P3-01 `tea new`/`init`, `tea add`, `tea remove`, `tea sync`, `tea lock`.
      `new`/`init` create `main.py`, `pyproject.toml`, `.gitignore`;
      add/remove/sync/lock are an unchanged passthrough to `uv`
      (`HYDRO_UV`), including the exit status.
- [ ] P3-02 Keep `pyproject.toml`/lockfile behavior compatible with `uv`.
      Delivered: scaffold and an unchanged uv passthrough (`add`/`remove`/
      `sync`/`lock`); Hydro does not interpret the files. Program: read
      `pyproject.toml` for the entry point and dependency intent, honor
      `uv.lock`/`pyproject.toml` without forking uv semantics, and document
      which fields Hydro interprets. Acceptance: a foreign project with
      `pyproject.toml` + `uv.lock` runs via `tea sync && tea infuse`; a fixture
      asserts that Hydro and `uv` agree on the environment, with no silent
      rewrite of project files.
- [ ] P3-03 Integrate uv as a library/process; redirect interpreter selection to
      `tea` (`.python-version`, venv marker, `sys.executable`).
      Delivered: process passthrough and a real `sys.executable`
      (`current_exe`). Program: write a venv marker/`.python-version` that
      makes uv select Hydro instead of CPython, and inject the Hydro interpreter
      into uv-driven environments. Acceptance: after `tea sync`, `tea infuse`
      and `uv run` in the same project use Hydro (no CPython interpreter
      needed), with the marker/version content covered by a test.
- [x] P3-04 Build cache: hash over source + flags + runtime version, incremental.
      `.tea/cache`, deterministic key over source, reachable
      modules, target, version and runtime archive; `--no-cache`, `tea clean`,
      hit/invalidation tests.
- [ ] P3-05 Reproducible builds (deterministic objects, path stripping).
      Moved to the Tea repository: `scripts/repro_check.sh` compares two
      `--no-cache` builds, cache/fresh links, hashes and program output;
      the Tea repository’s `docs/REPRODUCIBLE.md` records the current contract. Same spelling/path is deterministic;
      relative vs absolute source paths still change embedded path immediates.
      Program: path-independent output, `SOURCE_DATE_EPOCH`, stripped
      absolute paths (Q-10 relies on this). Acceptance: two builds from
      different checkout paths/times are byte-identical and the gate compares
      checksums.
- [x] P3-06 `tea test`, `tea fmt`, `tea lint`, `tea check`.
      Implemented; `test` is **not** pytest-compatible (success = exit 0,
      `--json`), `fmt` only normalizes whitespace/final newline.
- [x] P3-07 uv fork only if the architecture forces it (decision doc).
      Decision: no fork; passthrough (ARCHITECTURE §8).
- [ ] P3-08 Packaging: native binaries, cross-compilation, installers.

**Acceptance P3:** a foreign Python project with `pyproject.toml` can be started with
`tea sync && tea infuse` (defined support scope). *Not yet
met*: without uv interpreter injection/packaging this remains a goal.

## P4 — Python compatibility & ecosystem

Goal: run unmodified Python code — the CPython standard library and the common
ecosystem — on the Hydro runtime while keeping the Hydro advantages (single VM,
concurrent GC, fast startup). The decision for the C-API path is recorded in
[`docs/C_API.md`](C_API.md): **option B, a Hydro-native CPython ABI layer; no
`libpython` in the production runtime.**

- [x] P4-01 CPython test-suite harness and official gate.
      **Implemented:** `scripts/stdlib_compat.py` (import matrix,
      `docs/COMPATIBILITY.md`) and `scripts/cpython_compat.py` (official
      `Lib/test/test_*.py` differential runs against the pinned CPython
      **3.14.9** oracle, JSON + `docs/CPYTHON_TESTS.md`, release gate
      `--gate`, offline `--check-manifest`, monotonic manifest updates).
      Manifest `tests/compat/cpython-3.14.9.json` records `minimum_passing=3`
      for the 2026-10-29 full run's **3/398** genuine passes
      (`test_errno.py`, `test_int_literal.py`, `test_longexp.py`); `required`
      stays empty until a never-regress subset is pinned. It grows only from
      real `hydro_pass` results.
      Stdlib import baseline 2026-10-30: **258/297 importable** (50
      builtin/frozen, 156 pure, 52 extension; 9 CPython-failing; 39 failed). Batch A of
      the P7 standard-library program (`_bisect`/`bisect`,
      `_heapq`/`heapq`, `colorsys`, `keyword`, `_stat`/`stat`,
      `_operator`, `_functools`, `_io`), the Batch B native core
      (`binascii`, `base64`, `quopri`, `fnmatch`, `getopt`, `mimetypes`),
      the benchmark-critical
      `subprocess`, `tempfile`, `shutil`, `textwrap` and `timeit` batch, the
      package/site-packages loader and the native
      `typing`/`abc`/`enum`/`types`/`itertools`/`ast`/`fractions`/`importlib`/
      `__future__` modules are importable and API-tested.
      Official-suite baseline 2026-10-29: **3/398 passing** (333
      `hydro_fail`, 62 `expected_fail` where CPython also fails; 0 timeouts). No test is marked
      passing unless CPython 3.14.9 and Hydro both exit 0.
      The harness runs a defined subset of CPython 3.14 `Lib/test` against
      `tea` and emits a machine-readable report (per test: pass/skip/fail,
      plus the reason).
      Acceptance: `scripts/stdlib_compat.py` generates
      `docs/COMPATIBILITY.md` + `benchmarks/compat/latest.json`, and
      `scripts/cpython_compat.py` generates `docs/CPYTHON_TESTS.md` +
      `benchmarks/compat/cpython-3.14.9.json` and updates the manifest only
      from real `hydro_pass` runs. Both are deterministic and offline-checkable
      (`--check-manifest`); no hand-written pass claims. Gating: a test counts
      as passed only if CPython 3.14.9 and Hydro both exit 0. Growing the
      manifest is P7-05/P7-06.
- [ ] P4-02 Language completeness required by the stdlib and ecosystem.
      Delivered: arbitrary-precision `int` (tagged i64 fast path + `BigInt`),
      `del`, loop `else`, dataclass field defaults, keyword-only/`*args`/
      `**kwargs` binding, class attributes and properties, user
      `__iter__`/`__next__`, the common `__dunder__` operators and
      `__call__`/`__str__`/`__repr__`/`__len__`/`__contains__`/`__dict__`/
      `__new__`, and native `Thread` subclassing (including zero-argument
      `super().__init__`).
      Program: user decorators (function/class, parametrised), `except*`,
      `match`, positional-only parameters, `__slots__`, the full format
      mini-language, a single call combining dynamic `*args` **and**
      `**kwargs`, and the remaining `super()` forms. Metaclasses, multiple
      inheritance, the general descriptor protocol and
      `bytearray`/`memoryview` are delivered rather than open.
      Acceptance: the CPython language-feature tests in scope pass; every
      remaining deviation is listed in `docs/COMPATIBILITY.md`.
- [ ] P4-03 Import system and module model for real packages.
      Delivered: packages (`__init__.py`), dotted and relative imports,
      `HYDRO_SITE_PACKAGES`/`VIRTUAL_ENV`/`CONDA_PREFIX`/`PYTHONPATH` search,
      one-time module initializers, cycle diagnostics and `importlib` core
      modules. Program: a general runtime `sys.path`/`sys.modules` model,
      `site`/`.pth`, namespace packages, zip and frozen imports, module
      `__all__`, and full top-level side-effect semantics.
      Acceptance: a standard venv `site-packages` tree can be imported;
      package/relative-import fixtures pass; `sys.modules` and namespace/zip
      fixtures behave like CPython 3.14.9.
- [x] P4-04 C-API/ABI strategy: decision recorded (2026-10-24).
      **Option B** (Hydro-native ABI shim, no `libpython` in production; CPython
      3.14 headers at build time and the CPython interpreter as differential
      test oracle only). Evidence: embedded CPython costs ~6.7 ms init +
      ~7.5 ms finalize and ~41.6 ms for `import numpy`; Hydro AOT startup is
      ~0.8–1.0 ms. A stable-ABI probe module builds and imports with just four
      undefined CPython symbols, confirming the loader/shim model. Details in
      [`docs/C_API.md`](C_API.md).
      - [x] P4-04a `abi3` shim: module init (`PyModule_Create2`), `PyArg_Parse*`,
        scalar/container constructors, error API, `PyCapsule`, buffer basics.
        **Implemented:** module creation/attributes (`PyModule_AddStringConstant`/
        `AddObject`), scalars (`PyLong_*`, `PyFloat_*`, `PyUnicode_*`,
        `PyBytes_*`), containers (`PyList_*`/`PyTuple_*`/`PyDict_*`),
        capsules (`PyCapsule_New/GetPointer/GetName/IsValid/SetPointer`),
        read-only contiguous bytes buffer (`PyObject_CheckBuffer/GetBuffer/
        ReleaseBuffer`), variadic `PyArg_ParseTuple` (`i/l/n/d/s/O/p`, `|`,
        `:`/`;`), refcount entry points, `PyErr_*` + `PyExc_*` singletons,
        symbol anchor and C-variadic glue. Gate proven for the fixture:
        `hydro_capi_probe.abi3.so` imports/calls under JIT and AOT, and neither
        the CLI nor the AOT binary links `libpython`
        (`tea/tests/extensions.rs`). This is the defined limited-API
        subset; additional `abi3` symbols required by real extensions are
        driven by P4-10.
      - [x] P4-04b Extension loader: `dlopen` + `PyInit_*`, wheel ABI tags,
        package submodules, multi-phase init, clear init-failure errors.
        **Implemented:** compile-time discovery of `<name>.so`/`<name>.abi3.so`/
        `<name>.*.so` next to the source, lazy per-path load/caching,
        `PyInit_<name>` resolution, symbol resolution from the `tea` process,
        callable values via `hydro_getattr`/`hydro_call`, and clear
        init/attribute/argument errors. Discovery also searches
        `HYDRO_EXTENSION_PATH`, `PYTHONPATH`, `HYDRO_SITE_PACKAGES` and
        `VIRTUAL_ENV`/`CONDA_PREFIX` site-packages, and resolves dotted
        package submodules (`pkg/sub_ext.abi3.so` without package Python
        source). Wheel tags are validated at compile time: only `.so`,
        `.abi3.so` and `.cpython-314-<platform>.so` load; other tags are
        reported as incompatible before any `dlopen`. Multi-phase init is
        supported: `PyModuleDef_Init` results are created through the same
        module core and their `Py_mod_exec` slots run once per path; invalid
        slots and exec failures are load errors.
      - [ ] P4-04c Full CPython 3.14 ABI: `PyTypeObject` layout, refcounted
        `PyObject` handles over Hydro objects, vectorcall, full buffer protocol,
        `PyType_FromSpec`, GC traverse/clear integration.
        Gate: NumPy 2.5 array smoke and `pydantic-core` model validation.
      - [ ] P4-04d GIL emulation at extension boundaries and callback safety.
        Gate: threaded extension callbacks under GC stress without
        deadlock/data race.
- [ ] P4-05 Native extension loading (`.so`) + wheel support.
      Delivered: `.so` loading, sibling/`HYDRO_EXTENSION_PATH`/`PYTHONPATH`/
      venv `site-packages` discovery, package submodules
      (`pkg/sub_ext.abi3.so`), wheel ABI-tag validation (`.so`, `.abi3.so`,
      `.cpython-314-<platform>.so`; other tags are compile-time errors),
      symbol resolution from the Hydro shim, explicit init/ABI errors, and
      multi-phase (`PyModuleDef_Init`) initialisation. Program: unpack and
      install `.whl` archives into a venv-like prefix, resolve
      `*.dist-info`/RECORD paths, and import extensions from the installed
      tree. Acceptance: an `abi3` wheel installs via the documented command
      and imports under JIT/AOT without `libpython`; real ecosystem wheels
      remain gated by P4-04c/d.
- [ ] P4-06 `ctypes`/`cffi` boundaries.
      `ctypes.CDLL` on libc and `cffi` ABI-mode smoke; pointer/struct/array
      conversion rules documented. Depends on P4-04a.
- [ ] P4-07 Pickle/serialization, encoding details, numeric limits.
      HYD1 is deliberately not CPython-compatible; `bytes`/UTF-8/string escapes
      are tested; BigInt promotion is documented.
      Program: CPython pickle protocols 2–5, a `marshal` subset and
      `datetime`/`decimal` encodings. Acceptance: CPython-written pickles
      round-trip for the supported types and the protocol matrix is tested;
      every unsupported opcode/protocol is an explicit error.
      - [x] P4-07a BigInt in HYD1: the serializer stores sign + big-endian
        magnitude and normalizes a canonical small BigInt back to the tagged
        form (`support/serialize.rs`; covered by the serialization tests).
- [ ] P4-08 Compatibility matrix and support policy.
      Delivered: a generated per-module import/status matrix
      (`docs/COMPATIBILITY.md`) with the pinned CPython version and
      unsupported reasons. Program: per-library and C-ABI columns plus
      startup/RSS numbers, covering stdlib (P4-09), C-ABI (P4-04/05) and
      ecosystem suites (P4-10). Acceptance: one reproducible matrix is
      generated by a script, contains no hand-written cells, and a documented
      support policy states what an importable entry guarantees.
- [ ] P4-09 Full standard-library parity (297 CPython **3.14.9** modules).
      Split tracked in the compatibility report; no module is "done" without
      its CPython test subset. The executable module-by-module program
      (workstreams, dependency batches, full 272-module ledger) is
      **P7 — Standard library**; this item keeps the umbrella policy and the
      P4-03/P4-04 dependencies.
      - [ ] P4-09a Import infrastructure (depends on P4-03).
      - [ ] P4-09b Pure-Python stdlib: run the 174 pure-Python modules
        unmodified from a CPython 3.14 stdlib tree (PSF license, optional
        sysroot); adapt through shims, never forks; per-module test runs.
      - [ ] P4-09c Native/extension modules (63): implement in Rust where
        performance/semantics matter (e.g. `_io`, `_json`, `_csv`, `_struct`,
        `_datetime`, `_decimal`, `_hashlib`, `_socket`, `_ssl`, `_sqlite3`,
        `select`, `zlib`, `_bz2`, `_lzma`, `array`, `binascii`,
        `unicodedata`); pure-Python fallbacks where semantics allow; explicit
        unsupported list otherwise.
      - [ ] P4-09d Builtin/frozen modules (52): `sys`, `builtins`, `_imp`,
        `_thread`, `_warnings`, `_weakref`, `marshal`, `errno`, `posix`,
        `time`, `itertools`, `_io`, `_collections`, `_functools`, `_operator`,
        `_signal`, `_stat`, `_tracemalloc`, etc.
      - [ ] P4-09e CPython stdlib test runs per module, wired into P4-01.
      Acceptance: every module is either green in its CPython test subset or
      listed as unsupported with a reason; the report is reproducible.
- [ ] P4-10 Ecosystem compatibility suites (pinned versions, Python 3.14).
      Install wheels into a venv with `uv`, run upstream test subsets plus a
      real application smoke; record pass/fail per library.
      - [ ] Tier 1 (release gate): `numpy`, `pandas`, `pydantic`, `fastapi`
        (+`starlette`/`uvicorn`/`httpx`), `django`, `requests`, `sqlalchemy`,
        `pytest`.
      - [ ] Tier 2: `scipy`, `matplotlib`, `flask`, `aiohttp`, `pillow`,
        `cryptography`, `protobuf`, `grpcio`, `boto3`, `polars`.
      Per-library acceptance: upstream test subset result, app smoke result,
      unsupported-feature list, and a reproducible command. No silent skips.
- [ ] P4-11 Ecosystem benchmarks and budgets.
      For each Tier-1 library/app: startup, import time, request/throughput,
      dataframe/array operation, RSS versus CPython 3.14.9; thresholds in
      `benchmarks/ecosystem/` and a report comparable to `bench_hydro.py`.
      Acceptance: reproducible report + documented budgets; regressions fail
      the local gate. This is the evidence that keeps the Hydro advantages
      rather than trading them for CPython overhead.
- [ ] P4-12 LTS compatibility lines (future; no LTS before production).
      Define Hydro LTS lines keyed to CPython minor versions (3.13/3.14/3.15…):
      frozen CPython patch baseline, supported C-ABI/wheel tags, frozen
      stdlib/language surface + compatibility matrix, security-patch intake and
      a documented support window. Acceptance: one manifest/baseline per LTS
      line, a release checklist and an explicit deprecation policy; 3.14.9 is
      the only active target today and is not an LTS commitment.

**Acceptance P4:** defined, tested language/stdlib/ecosystem scope; a generated
compatibility matrix; C-API limits documented; no silent semantic deviations;
ecosystem benchmarks show where Hydro is faster, equal or slower than CPython
(the latter documented, not hidden).

## P5 — Performance & JIT

- [x] P5-01 Benchmarks vs. CPython/PyPy/Go (startup time, throughput, memory).
      Readiness matrix `benchmarks/HYD.md`; Hydro micro harness and baseline
      (`benchmarks/hydro/results/latest.{json,md}`) cover throughput, RSS and
      build/startup time, incl. comprehensions, map/filter, lambda calls,
      reduce, math loop, JSON dumps/loads and thread/process/pool scaling.
      `benchmarks/bench_hydro.py` runs 11 workloads (incl. lazy generator sum
      and async `gather`) under CPython/JIT/AOT and is registered as `hydro` in
      `bench_all.py`. `benchmarks/compare_engines.py` compares loop and
      startup under CPython/Hydro/PyPy/Go (Go = equivalent compiled native
      reference) with output validation; missing engines are reported as
      skipped, so PyPy numbers require an installed PyPy. Memory/RSS is
      measured by the Hydro micro harness; cross-engine RSS is not claimed.
- [ ] P5-01a Hydro adapter: run the existing `bench_*.py` with Hydro as the implementation
      (subprocess/`HYDRO_BIN`), without modifying the user harnesses.
      Delivered: micro harness `benchmarks/hydro/run_micro.py` (CPython/JIT/AOT),
      `benchmarks/bench_hydro.py` (same pure Python workloads under three
      runners) and `compare_engines.py` (equivalent loop/startup scripts).
      Program: a `HYDRO_BIN` adapter that runs `bench_loops.py`, `bench_csv.py`,
      `bench_startup.py` and `bench_request_handling.py` unchanged with Hydro.
      Acceptance: each harness produces validated output through the adapter;
      missing `HYDRO_BIN` is a skip, not a fabricated number.
- [ ] P5-01b Benchmark readiness per benchmark: loops (comprehensions/lambdas/
      builtins), csv (modules/IO/with), startup (AOT/JIT subprocess),
      request (async/socket/json), fastapi (framework/C API).
      Delivered: matrix in `benchmarks/HYD.md`; loops and startup (JIT/AOT)
      measurable; csv rows documented. Program: request needs non-blocking I/O
      (P7-17/P2-13); fastapi needs decorators + C API (P4-02/P4-04c) and the
      adapter from P5-01a. Acceptance: every matrix row has a measured,
      output-validated CPython/Hydro result or an explicit blocker with the
      blocking item.
- [x] P5-01c CI regression thresholds + baseline artifacts for Hydro.
      Baseline artifacts + `benchmarks/hydro/thresholds.json` +
      `scripts/check_benchmarks.py` (`--check`/`--update`) as a local/release
      gate; `bench_all.py` resolves its output directory absolutely, AOT
      builds are excluded from the measured runs and deviating outputs result
      in exit code 1. Q-06 is resolved as local enforcement plus a
      non-blocking CI report and deterministic harness self-tests.
- [x] P5-01e Parallel benchmarks (after P2-20): threads/processes, scaling,
      startup cost, memory; CPython/Hydro comparison.
      `threads_serial`, `threads_parallel`, `pool_serial`, `pool_parallel`,
      `pool_map`, `process_queue` (JIT/AOT/CPython, RSS/startup time).
- [ ] P5-02 PGO + LTO for AOT. (`lto = "thin"` is set in the profile;
      PGO/LLVM LTO measurement open.)
- [ ] P5-03 Inlining, constant folding, escape analysis, devirtualization.
      Delivered: constant folding (int/float/str/binops/comparisons/unary,
      overflow- and zero-division-protected), dead `if`/`while False`, typed
      int fast paths (`+`/`-`/`*`, comparisons, AugAssign, fully int-typed
      `for range` counters; checked overflow → `hydro_binop` slow path), native
      module dispatch through a thread-local spec cache, and the
      register-only x86_64 fiber switch. The 2026-10-29 hot-path slice also
      added pointer-based precise-root switching, lazy traceback frames,
      stack bounds derived from the running fiber, and scalar guards around
      instance-only registry lookups. Measured (release, n=200000):
      `math_loop` 6.96x/14.48x (JIT/AOT), while_sum 4.41x/10.31x,
      for_range_sum 5.84x/11.21x; 100k generator sum CPython 9.46 / JIT 5.08 /
      AOT 4.71 ms. Program: an inliner, escape analysis/devirtualization and
      unboxed floats. Acceptance: each optimization is behind a measurement
      that shows a gain without correctness regressions; no optimization
      claim without a tracked result.
- [ ] P5-04 GC tuning based on measurements (pauses, allocation rate).
      Delivered: growth-based threshold, relaxed-safepoint fast path,
      edge barriers, concurrent sweep, `HYDRO_GC_STATS`, `gc_stats()`/
      `GcStats` and `tea gc [--json]` pause/mark/sweep times; dict hash
      index; fiber `ctx`/`caller_ctx` lifetime roots; a fast non-cryptographic
      address hasher and in-place sweep rebuild (300k-short-lived-list AOT:
      44.95 → 40.91 ms median, ≈9%). Program: pause/allocation-rate targets
      driven by the P2-05/P2-06 precise allocator and roots, including
      safepoint frequency at backedges. Acceptance: a documented pause/RSS
      budget is met on the tracked micro harness at default settings, with
      before/after measurements.
- [ ] P5-05 Profiling (CPU/heap) + trace integration.
- [ ] P5-06 JIT decision **only** with measurement data; otherwise discard.
- [ ] P5-07 If JIT: LLVM ORC or Cranelift, tiering, deopt strategy.

**Acceptance P5:** measurable, documented improvement without correctness regression.

## Cross-cutting (parallel, permanent)

- [ ] Q-01 Debugger integration (DAP), source maps/DWARF.
- [x] Q-02 Python-like tracebacks incl. chained exceptions; stable error codes.
      Simplified tracebacks, stable codes, explicit `raise ... from`
      (deepest-first, cycle-guarded) and implicit `__context__` chaining
      (with `__suppress_context__` and "During handling ...") are present
      (P2-11). Frame fidelity is function/line-level; full CPython frame
      objects are not claimed.
- [ ] Q-03 Fuzzing: parser, lowering, codegen (cargo-fuzz).
      Delivered: deterministic `tests/fuzz_smoke.rs` token/Unicode/mutation
      corpus in the frontend (no panic, valid diagnostics). Cargo-fuzz targets
      remain open.
- [ ] Q-04 Fuzzing: runtime/GC (allocation/object operations).
      Delivered: deterministic ABI operation-sequence fuzz smoke in
      `crates/hydro-runtime/tests/fuzz_smoke.rs`; cargo-fuzz targets remain
      open.
- [ ] Q-05 Security review of `unsafe` code; policy in ARCHITECTURE.md.
      `unsafe` is encapsulated and justified in GC/ABI; formal review open.
- [x] Q-06 Performance regression CI (thresholds).
      Owned by P5-01c: threshold enforcement is local/release-only
      (`scripts/check_benchmarks.py --check`), CI validates the tracked
      baseline as a non-blocking report (`continue-on-error`) and runs
      `scripts/test_check_benchmarks.py`. Shared-runner timing never fails CI.
- [x] Q-07 Documentation: language scope, CLI, architecture, contribution.
      README/ARCHITECTURE/LAYOUT/C_API/DIAGNOSTICS/TODO/AGENTS/CHANGELOG
      updated; P6-09 owns the remaining troubleshooting/migration text.
- [x] Q-08 Release process, changelog, SemVer.
      Changelog (Unreleased/0.1.0) and SemVer scheme documented;
      `.github/workflows/release.yml` runs the tests on a `v*` tag, builds
      `--locked`, packages/checksums/smoke-tests the Linux x64 JIT/AOT artifact and
      publishes it with the automatic `GITHUB_TOKEN` (no repository secrets).
      Artifacts for other platforms remain Q-09.
- [ ] Q-09 Cross-platform (macOS, Windows, aarch64) — after Linux stability.
- [ ] Q-10 Telemetry-free defaults; build reproducibility.
      No telemetry is emitted. Reproducible builds are owned by P3-05; this
      item closes with that acceptance plus a check that no network/telemetry
      call exists in the runtime/CLI.

## P6 — Polishing & developer experience (cargo-like)

Goal: `tea` should feel like a single, complete tool — not like
a collection of scripts. Errors are first-class products, not an afterthought.

- [x] P6-01 First-class diagnostics: stable codes (`HYDnnnn`), spans,
  source snippets with carets, help/hints (did-you-mean), multiple errors,
  summary and consistent exit codes.
      Codes/spans/snippets/help/summary/exit codes are implemented; semantic
      diagnostics now carry multi-character expression spans
      (`sema::simple_expr_end` + `check_spans_multichar_expression`), and the
      ICE path is covered by unit/E2E tests (P6-05/P6-08).
- [x] P6-02 Machine-readable diagnostics: `--json`/message format with a stable
  schema (file/line/col/end_col/code/message/help) — implemented & tested.
- [x] P6-03 Unified CLI UX: `new`, `init`, `check`, `run`, `build`, `test`,
  `fmt`, `lint`, `clean`, `doctor`, `add`, `remove`, `sync`, `lock` from one
  binary; clear help, `--verbose`/`--quiet`, `--color`.
      All named subcommands, help, `--color`/`--json`, global/per-command
      `--verbose`/`--quiet` (conflict → exit 2) and defined exit codes are
      implemented and covered in `tea/tests/e2e.rs`.
- [x] P6-04 Build cache & incremental builds (`.hydro/cache`), cache
  invalidation via source + flags + runtime version.
      Owned by P3-04: cache incl. module tree fingerprint, compiler
      fingerprint, `--no-cache`, `tea clean` and hit/invalidation tests.
      Reproducible binaries are P3-05.
- [x] P6-05 ICE handling: internal panics as `internal compiler error` with
  version/target/hint, exit code 2, `RUST_BACKTRACE` hint; no panics
  on user input (fuzzing tracked in Q-03/Q-04).
      `catch_unwind` + ICE banner are covered by
      `main.rs::panic_maps_to_an_ice_without_the_payload`; the payload is
      never printed.
- [x] P6-06 Project scaffolding/templates (`tea new`, `tea init`) so that a
  project is ready to go without third-party tools; `pyproject.toml` compatibility.
      Scaffold (`main.py`, `pyproject.toml`, `.gitignore`) with `--force`
      policy; `pyproject.toml` remains uv-compatible.
- [x] P6-07 Shell completions, manpage, `--version --verbose` (target,
  Cranelift, runtime version).
      `tea completions <bash|zsh|fish>` emits complete scripts;
      Tea's `docs/tea.1` is the roff manpage; `tea --version --verbose` prints
      version, target, backend and runtime version. Tests in
      `tea/tests/completions.rs`.
- [x] P6-08 Diagnostic regression tests: codes, spans, JSON schema, colors/
  TTY behavior, exit codes, ICE path.
      E2E covers human/JSON/color/doctor/caret/syntax code, the multi-character
      expression span (`check_spans_multichar_expression`) and the ICE
      classification unit test.
- [ ] P6-09 Documentation: CLI reference, diagnostic catalog, troubleshooting guide,
  migration from CPython.
      Delivered: `docs/DIAGNOSTICS.md` (codes, spans, JSON schema) and the
      Tea `docs/tea.1` CLI reference. Program: a troubleshooting
      section (common failures → checks) and a CPython migration scope
      (supported subset, known deviations, where each is tracked). Acceptance:
      every documented command/flag matches `tea --help`; every emitted
      `HYDnnnn` code appears in the catalog; a reader can migrate a small
      CPython script without consulting source.
- [ ] P6-10 Build feedback: timing, `--verbose`, progress, exit-code summary.
      Delivered: `--verbose` phase timings and cache decisions for `run` and
      `build`; `--quiet` suppresses status output. Program: progress reporting
      for long operations and a final exit-code summary for multi-file
      commands (`test`/`lint`). Acceptance: long builds/tests show progress
      without `--verbose`; the summary reports counts and the exit code
      matches the process status.

**Acceptance P6:** a user can initialize, build, run and diagnose a project with
`tea` without knowing other tools; every error class has a
code, span, help and stable exit code; `--json` is documented and tested.

## P7 — Standard library

Goal: the CPython 3.14.9 standard library works on the Hydro runtime —
module by module, with evidence. `benchmarks/compat/latest.json` is the
source of truth for the import matrix (`docs/COMPATIBILITY.md` is generated
from it). Status 2026-10-30: **258/297 modules importable**, **39 failed**
(including 9 modules where CPython 3.14.9 itself fails on this host).
The latest batch closed `_pydatetime`, `_pyio`, `logging`, `venv` and
`imaplib` by fixing sequential class-body lookup, property setters,
unary dunders, subclass `__new__`, literal regex braces and `str.rfind`.

The package/site-packages loader (P7-01) is in place: packages with
`__init__.py`, dotted names, submodules and relative imports resolve through
sibling files, the package-chain root, `HYDRO_SITE_PACKAGES`, the
`VIRTUAL_ENV`/`CONDA_PREFIX` site-packages trees and `PYTHONPATH`.
Imported module bodies run once as initializers, and nested imports stay
deferred. No native `rich` is shipped: the official pure-Python `rich`
distribution is resolved through this loader.

**`bench_all.py` under Hydro: green at the default GC threshold.** Rich
`console`/`table`/`panel`/`rule` rendering works, native Thread subclassing
and zero-argument `super().__init__` are supported, and a native
`rich.status.Status` shim covers `Console.status`. The report renders without
raising `HYDRO_GC_THRESHOLD`; precise roots fixed the former live-value loss.

Acceptance: 297/297 modules accounted for on CPython 3.14.9 — every module
is either importable **and** green in its CPython `Lib/test/test_*.py`
subset, or explicitly listed as platform-unsupported with a reason. An
import that merely succeeds is not parity: representative API calls and
per-module CPython test runs decide.

This topic is the executable module-by-module program for the umbrella
goal P4-09; P4-09 keeps the policy and the dependency on P4-03/P4-04.

### Workstreams

- [ ] P7-01 Import and package foundation: packages with `__init__.py`,
      dotted and relative imports, `sys.path`/`sys.modules` handling,
      namespace packages, frozen modules and circular imports. Depends on P4-03.
      **Delivered:** packages (dotted names, submodules, relative imports),
      the search path (sibling files, package-chain root, `HYDRO_SITE_PACKAGES`,
      `VIRTUAL_ENV`/`CONDA_PREFIX` site-packages, `PYTHONPATH`), load-once per
      canonical path, cycle diagnostics for module-level import cycles, and
      imported module bodies as one-time initializers.
      **Program:** namespace packages, frozen modules, a general
      `sys.path`/`sys.modules` runtime model and multiple-module
      `from x import *`. Acceptance: namespace/zip/frozen fixtures import as
      on CPython 3.14.9; `sys.modules` reflects loaded units and can be
      inspected; `from pkg import *` honors `__all__`.
- [ ] P7-02 Pure-Python stdlib source and shim pipeline: consume the CPython
      3.14 stdlib tree unmodified (PSF license, optional sysroot), resolve
      imports through Hydro shims, never fork sources; per-module provenance
      and drift checks against the pinned 3.14.9 tree.
- [ ] P7-03 Builtin/frozen native shims: `builtins`, `sys`, `_imp`,
      `_frozen_importlib*`, `_warnings`, `_weakref`, `_collections`,
      `itertools`, `marshal`, `_io`, `_abc`, `gc`, `atexit`, `faulthandler`,
      `site`, `_codecs`, `_sre`, `_thread`-adjacent gaps — the baseline every
      other batch imports.
      Delivered: `sys`, `itertools`, `abc`, `typing`, `types`, `enum`,
      `__future__`, `_io`, `_operator`, `_functools`, `_stat`, `_bisect`,
      `_heapq` are importable; `sys` lacks `path`/`modules`. Program: the
      remaining import/lib shims above, enough to import the pure-Python
      batches.
- [ ] P7-04 Extension modules in Rust: implement or back with Rust where
      semantics/performance matter (`_socket`, `_ssl`, `_hashlib`, `zlib`,
      `_json`, `_csv`, `_struct`, `_datetime`, `_decimal`, `_sqlite3`,
      `select`, `array`, `binascii`, `unicodedata`); pure-Python fallbacks
      only where the semantics allow; explicit unsupported list otherwise.
      Delivered in Rust: `_bisect`, `_heapq`, `_stat`, `_operator`,
      `_functools`, `_io`, `binascii`, plus the native `math`/`json`/`csv`/
      `re`/`socket` surfaces. Program: the remaining extension modules and
      their CPython test subsets.
- [ ] P7-05 Per-module CPython test integration: map every module to its
      `Lib/test/test_*.py` subset, run it through the P4-01 differential
      harness, record `hydro_pass`/`expected_fail` only from real runs, and
      fail the gate on regressions.
      Delivered: the differential harness, manifest and `--check-manifest`
      gate exist, and `scripts/stdlib_compat.py --tests` is the hook.
      Program: the per-module mapping and a regression gate at batch
      granularity. Acceptance: each importable module has a mapped subset and
      a recorded status; a previously green subset that regresses fails the
      gate.
- [ ] P7-06 Dependency batches and reporting: keep the batch order below
      current, regenerate the matrix after every batch, and report per
      module (importable, tests green, unsupported reason) in
      `docs/COMPATIBILITY.md` and `docs/CPYTHON_TESTS.md`.
      Delivered: dependency-ordered batches and the generated import matrix.
      Program: add the tests-green column per module and regenerate both
      reports after each batch. Acceptance: the two generated reports agree
      on every module's status and the batch list stays dependency-ordered.

### Batches (dependency-ordered)

Each module below is assigned to exactly one batch; 35 ledger entries are
importable now, 229 remain open and 8 are platform-blocked. Later
batches may start only when their prerequisites are importable; the module
ledger below is the per-layer checklist that tracks the actual work.
The benchmark-critical batch (P7-22) owns `tempfile`, `shutil` and `textwrap`;
its `subprocess` and `timeit` members are counted in Batches D/F.
Batch acceptance (applies to every `[ ]` batch): each listed module imports
through `tea`, its representative API matches CPython 3.14.9, and its mapped
`Lib/test/test_*.py` subset (P7-05) is green — or the module is explicitly
listed as unsupported with a reason.

- [x] P7-10 Batch A — 11 modules: simple native primitives. No dependencies;
      establishes the native-module pattern for later batches.
      **First slice: import + representative API verified** (see below);
      CPython per-module test subsets remain under P7-05. Contracts:
      `bisect`/`_bisect` expose `bisect_left`, `bisect_right`,
      `bisect`, `insort_left`, `insort_right`, `insort` (2–5 args,
      `lo`/`hi`/`key` optional); `heapq`/`_heapq` expose `heappush`,
      `heappop`, `heapify`, `heappushpop`, `heapreplace`, `nlargest`,
      `nsmallest`; `colorsys` the six `rgb_*`/`*_rgb` conversions;
      `keyword` `kwlist`/`softkwlist`/`iskeyword`/`issoftkeyword`;
      `stat`/`_stat` `S_*`/`ST_*` constants plus `S_IMODE`, `S_IFMT`,
      `S_IS*`, `filemode`; `_operator`, `_functools`, `_io` re-export
      the `operator` surface, `reduce` and `open`.
      **Delivered:** stdlib specs
      (`crates/hydro-ir/src/stdlib/`), runtime dispatch and submodule
      declarations (`crates/hydro-runtime/src/modules/`), native
      implementations, and the CLI regression
      `tea/tests/stdlib_batch_a.rs`.
      Acceptance: all 11 modules import through `tea`; a
      representative API call matches CPython 3.14.9 (bisect
      insertion points, heap pop ordering, HLS/HSV round-trips,
      `filemode` strings, `operator.add`/`functools.reduce`/
      `_io.open`); and `stdlib_batch_a.rs` covers import + behaviour
      for each. The import/API slice is done; the per-module CPython
      subsets remain P7-05 work and are not yet green.
      Modules (ledger entries are checked for importable + API-tested):
      `_bisect`, `bisect`, `_heapq`, `heapq`, `colorsys`, `keyword`,
      `_stat`, `stat`, `_operator`, `_functools`, `_io`

- [ ] P7-11 Batch B — 23 modules: text, string and shell utilities. The
      self-contained native codec/text core is done; the rest needs the
      pure-Python source pipeline (P7-02) and the `_string` shim (P7-03).
      Importable now (7/23): `binascii`, `base64`, `quopri`, `fnmatch`,
      `getopt`, `mimetypes`, `html`.
      Remaining (ledger entries are checked for importable + API):
      `string`, `_string`, `stringprep`, `glob`, `filecmp`, `fileinput`,
      `shlex`, `reprlib`, `pprint`, `difflib`, `linecache`,
      `_markupbase`, `cmd`, `mailbox`, `email`, `wave`

- [ ] P7-12 Batch C — 45 modules: import, builtins and object model. Needs P7-01;
      unblocks every remaining batch. Importable now (8/45): `abc`,
      `itertools`, `types`, `typing`, `enum`, `importlib`, `__future__`,
      `contextlib`.
      Remaining (ledger entries start unchecked):
      `_abc`, `_codecs`, `_collections`, `_collections_abc`,
      `_frozen_importlib`, `_frozen_importlib_external`, `_imp`, `_sre`,
      `tracemalloc`, `_tracemalloc`, `_types`, `_typing`, `_warnings`,
      `_py_warnings`, `_weakref`, `_weakrefset`, `_py_abc`, `atexit`,
      `builtins`, `codecs`, `encodings`, `faulthandler`, `gc`,
      `runpy`, `site`, `_sitebuiltins`, `zipimport`,
      `warnings`, `weakref`, `copy`,
      `pkgutil`, `modulefinder`, `_suggestions`, `_colorize`,
      `sre_compile`, `sre_constants`, `sre_parse`

- [ ] P7-13 Batch D — 24 modules: filesystem and process. Needs P7-01/P7-03
      (`posix`, `genericpath`). `subprocess` is done as part of the
      benchmark-critical batch; the remaining modules need the import
      foundation.
      Modules (remaining; `subprocess` is checked in the ledger):
      `genericpath`, `posixpath`, `ntpath`, `nturl2path`, `posix`, `pwd`,
      `_signal`, `signal`, `getpass`, `_posixsubprocess`,
      `_pyio`, `fcntl`, `grp`, `resource`, `pty`, `tty`, `termios`,
      `select`, `selectors`, `syslog`, `mmap`, `sched`, `webbrowser`

- [ ] P7-14 Batch E — 42 modules: data formats, datetime, serialization and
      numerics. Uses the implemented BigInt type and P7-04 for
      `_json`/`_struct`/`_pickle`/`_csv`. Importable now (3/42):
      `dataclasses` and `datetime` (native module specs, with frontend
      dataclass expansion) and `fractions` (native). Remaining (ledger
      entries start unchecked):
      `_datetime`, `_pydatetime`, `_strptime`, `calendar`,
      `struct`, `_struct`, `marshal`, `pickletools`, `copyreg`,
      `_compat_pickle`, `plistlib`, `tomllib`, `decimal`,
      `_decimal`, `_pydecimal`, `_pylong`, `numbers`,
      `_statistics`, `uuid`, `_uuid`, `ipaddress`, `zoneinfo`, `_zoneinfo`,
      `dbm`, `_dbm`, `_gdbm`, `shelve`, `sqlite3`, `_sqlite3`, `_json`,
      `_csv`, `_pickle`, `array`, `cmath`, `graphlib`, `xml`,
      `_elementtree`, `pyexpat`, `_random`

- [ ] P7-15 Batch F — 39 modules: introspection, REPL and developer tooling.
      Needs C (import) and code-object metadata. `timeit` is done as part
      of the benchmark-critical batch; `ast` is importable now (2/39).
      Remaining (ledger entries start unchecked):
      `_ast`, `_ast_unparse`, `_opcode`, `_opcode_metadata`,
      `opcode`, `dis`, `_symtable`, `symtable`, `_tokenize`, `token`,
      `tokenize`, `inspect`, `annotationlib`, `_pyrepl`, `code`, `codeop`,
      `compileall`, `py_compile`, `tabnanny`, `doctest`, `pydoc`,
      `pydoc_data`, `pyclbr`, `rlcompleter`, `idlelib`, `bdb`, `cProfile`,
      `profile`, `pstats`, `pdb`, `trace`, `traceback`, `_lsprof`,
      `_remote_debugging`, `readline`, `this`, `antigravity`

- [ ] P7-16 Batch G — 11 modules: logging, configuration and site/packaging
      tooling. Needs C (`_sysconfig`, `locale`) and D.
      Modules (all `[ ]` in the ledger):
      `_locale`, `locale`, `gettext`, `logging`, `configparser`, `optparse`,
      `ensurepip`, `venv`, `zipapp`, `_sysconfig`, `sysconfig`

- [ ] P7-17 Batch H — 23 modules: networking and crypto. Needs P7-04 (`_socket`,
      `_ssl`, `_hashlib`).
      Modules (all `[ ]` in the ledger):
      `ftplib`, `http`, `urllib`, `netrc`, `secrets`, `hashlib`, `_hashlib`,
      `_md5`, `_sha1`, `_sha2`, `_sha3`, `_blake2`, `hmac`, `_hmac`,
      `imaplib`, `poplib`, `smtplib`, `socketserver`, `wsgiref`, `xmlrpc`,
      `_socket`, `_ssl`, `ssl`

- [ ] P7-18 Batch I — 10 modules: compression and archives. Needs P7-04 (`zlib`,
      `_bz2`, `_lzma`, `_zstd`). `binascii` is implemented in Batch B and is
      not repeated here.
      Modules (remaining; all `[ ]` in the ledger):
      `bz2`, `_bz2`, `gzip`, `lzma`, `_lzma`, `zlib`, `_zstd`,
      `compression`, `zipfile`, `tarfile`

- [ ] P7-19 Batch J — 11 modules: concurrency, async and subinterpreters. Needs P2
      scheduler/GC work and P7-04.
      Modules (all `[ ]` in the ledger):
      `_contextvars`, `contextvars`, `_asyncio`, `concurrent`,
      `_threading_local`, `_interpchannels`, `_interpqueues`,
      `_interpreters`, `_multiprocessing`, `_posixshmem`, `_queue`

- [ ] P7-20 Batch K — 17 modules: large native extension surfaces (Tk, curses,
      Unicode data, multibyte codecs and `ctypes`). Needs P4-04c and
      P7-04.
      Modules (all `[ ]` in the ledger):
      `ctypes`, `_ctypes`, `curses`, `_curses`, `_curses_panel`,
      `_codecs_cn`, `_codecs_hk`, `_codecs_iso2022`, `_codecs_jp`,
      `_codecs_kr`, `_codecs_tw`, `_multibytecodec`, `_tkinter`, `tkinter`,
      `turtle`, `turtledemo`, `unicodedata`

- [ ] P7-22 Benchmark-critical batch — 5 modules used by
      `benchmarks/bench_all.py` and the bench scripts: `subprocess`,
      `tempfile`, `shutil`, `textwrap`, `timeit`.
      Delivered: all five import and their benchmark-used APIs pass under
      CPython; `bench_all.py` completes under Hydro when `HYDRO_GC_THRESHOLD` is
      raised (official pure-Python `rich` renders console/table/panel/rule, a
      native `rich.status.Status` shim provides the progress context manager,
      and the AOT report is written). `bench_hydro.py` runs under CPython and
      drives `tea infuse`/`tea build` over embedded workloads.
      Program: the full `bench_all.py` completes under Hydro at the **default**
      GC threshold. Acceptance: `tea infuse benchmarks/bench_all.py` without
      `HYDRO_GC_THRESHOLD` writes the complete report; the fix belongs to
      P2-06/P2-07 precise roots, not to a threshold workaround.

- [ ] P7-21 Batch L — 13 modules: platform-specific modules. The 8
      Windows/Apple-only modules are `[!]` unsupported on Linux; the 5
      support modules are pure-Python shims.
      Modules (all `[ ]` in the ledger):
      `_aix_support`, `_android_support`, `_apple_support`, `_ios_support`,
      `_osx_support`, `_overlapped`, `_scproxy`, `_winapi`, `_wmi`,
      `msvcrt`, `nt`, `winreg`, `winsound`

### Module ledger (272 originally missing)

Derived from the 272-module baseline in `benchmarks/compat/latest.json`
(the modules already importable at the baseline are not listed). 35 entries
are checked; `[x]` means importable plus representative-API testing where the
batch notes say so, not CPython test-subset parity (P7-05). The checked
entries are the import/API slices delivered by Batches A/B/benchmark, the
native `typing`/`abc`/`enum`/`types`/`itertools`/`ast`/`fractions`/
`importlib`/`__future__` modules, and `html`/`contextlib`. `[!]`
marks a module CPython ships only for a
platform Hydro does not target on this host (Windows/Apple), tracked as
platform-unsupported rather than implemented.

Current matrix: 258/297 importable, 39 failed (including 9 modules CPython
3.14.9 itself fails). The ledger is the work list for the failed entries;
checked entries count as delivered slices, not as complete parity.

#### Builtin / frozen (46: 7 importable, 39 open)

- [ ] `_abc`
- [ ] `_ast`
- [ ] `_codecs`
- [ ] `_collections`
- [ ] `_collections_abc`
- [ ] `_contextvars`
- [ ] `_datetime`
- [ ] `_frozen_importlib`
- [ ] `_frozen_importlib_external`
- [x] `_functools`
- [ ] `_imp`
- [x] `_io`
- [ ] `_locale`
- [ ] `_opcode`
- [x] `_operator`
- [ ] `_signal`
- [ ] `_sitebuiltins`
- [ ] `_sre`
- [x] `_stat`
- [ ] `_string`
- [ ] `_suggestions`
- [ ] `_symtable`
- [ ] `_sysconfig`
- [ ] `_tokenize`
- [ ] `_tracemalloc`
- [ ] `_types`
- [ ] `_typing`
- [ ] `_warnings`
- [ ] `_weakref`
- [x] `abc`
- [ ] `atexit`
- [ ] `builtins`
- [ ] `codecs`
- [ ] `faulthandler`
- [ ] `gc`
- [ ] `genericpath`
- [x] `itertools`
- [ ] `marshal`
- [ ] `ntpath`
- [ ] `posix`
- [ ] `posixpath`
- [ ] `pwd`
- [ ] `runpy`
- [ ] `site`
- [x] `stat`
- [ ] `zipimport`

#### Pure Python (156: 27 importable, 129 open)

- [x] `__future__`
- [ ] `_aix_support`
- [ ] `_android_support`
- [ ] `_apple_support`
- [ ] `_ast_unparse`
- [ ] `_colorize`
- [ ] `_compat_pickle`
- [ ] `_ios_support`
- [ ] `_markupbase`
- [ ] `_opcode_metadata`
- [ ] `_osx_support`
- [ ] `_py_abc`
- [ ] `_py_warnings`
- [ ] `_pydatetime`
- [ ] `_pydecimal`
- [ ] `_pyio`
- [ ] `_pylong`
- [ ] `_pyrepl`
- [ ] `_strptime`
- [ ] `_threading_local`
- [ ] `_weakrefset`
- [ ] `annotationlib` — blocked on these exact runtime surfaces:
  - `typing._deprecation_warning_for_no_type_params_passed`
  - `typing.evaluate_forward_ref`
  - `warnings._deprecated`
  - `ast.parse` / `ast.unparse` (Hydro's `ast` shim only implements `literal_eval`)
  - `NameError` keyword arguments (`NameError(msg, name=...)`)
- [ ] `antigravity`
- [x] `ast`
- [x] `base64`
- [ ] `bdb`
- [x] `bisect`
- [ ] `bz2`
- [ ] `cProfile`
- [ ] `calendar`
- [ ] `cmd`
- [ ] `code`
- [ ] `codeop`
- [x] `colorsys`
- [ ] `compileall`
- [ ] `compression`
- [ ] `concurrent`
- [ ] `configparser`
- [x] `contextlib`
- [ ] `contextvars`
- [ ] `copy`
- [ ] `copyreg`
- [ ] `ctypes`
- [ ] `curses`
- [x] `dataclasses`
- [x] `datetime`
- [ ] `dbm`
- [ ] `decimal`
- [ ] `difflib`
- [ ] `dis`
- [ ] `doctest`
- [ ] `email`
- [ ] `encodings`
- [ ] `ensurepip`
- [x] `enum`
- [ ] `filecmp`
- [ ] `fileinput`
- [x] `fnmatch`
- [x] `fractions`
- [ ] `ftplib`
- [x] `getopt`
- [ ] `getpass`
- [ ] `gettext`
- [ ] `glob`
- [ ] `graphlib`
- [ ] `gzip`
- [ ] `hashlib`
- [x] `heapq`
- [ ] `hmac`
- [x] `html`
- [ ] `http`
- [ ] `idlelib`
- [ ] `imaplib`
- [x] `importlib`
- [ ] `inspect`
- [ ] `ipaddress`
- [x] `keyword`
- [ ] `linecache`
- [ ] `locale`
- [ ] `logging`
- [ ] `lzma`
- [ ] `mailbox`
- [x] `mimetypes`
- [ ] `modulefinder`
- [ ] `netrc`
- [ ] `nturl2path`
- [ ] `numbers`
- [ ] `opcode`
- [ ] `optparse`
- [ ] `pdb`
- [ ] `pickletools`
- [ ] `pkgutil`
- [ ] `plistlib`
- [ ] `poplib`
- [ ] `pprint`
- [ ] `profile`
- [ ] `pstats`
- [ ] `pty`
- [ ] `py_compile`
- [ ] `pyclbr`
- [ ] `pydoc`
- [ ] `pydoc_data`
- [x] `quopri`
- [ ] `reprlib`
- [ ] `rlcompleter`
- [ ] `sched`
- [ ] `secrets`
- [ ] `selectors`
- [ ] `shelve`
- [ ] `shlex`
- [x] `shutil`
- [ ] `signal`
- [ ] `smtplib`
- [ ] `socketserver`
- [ ] `sqlite3`
- [ ] `sre_compile`
- [ ] `sre_constants`
- [ ] `sre_parse`
- [ ] `ssl`
- [ ] `string`
- [ ] `stringprep`
- [ ] `struct`
- [x] `subprocess`
- [ ] `symtable`
- [ ] `sysconfig`
- [ ] `tabnanny`
- [ ] `tarfile`
- [x] `tempfile`
- [x] `textwrap`
- [ ] `this`
- [x] `timeit`
- [ ] `tkinter`
- [ ] `token`
- [ ] `tokenize`
- [ ] `tomllib`
- [ ] `trace`
- [ ] `traceback`
- [ ] `tracemalloc`
- [ ] `tty`
- [ ] `turtle`
- [ ] `turtledemo`
- [x] `types`
- [x] `typing`
- [ ] `urllib`
- [ ] `uuid`
- [ ] `venv`
- [ ] `warnings`
- [ ] `wave`
- [ ] `weakref`
- [ ] `webbrowser`
- [ ] `wsgiref`
- [ ] `xml`
- [ ] `xmlrpc`
- [ ] `zipapp`
- [ ] `zipfile`
- [ ] `zoneinfo`

#### Extension (62: 4 importable, 58 open)

- [ ] `_asyncio`
- [x] `_bisect`
- [ ] `_blake2`
- [ ] `_bz2`
- [ ] `_codecs_cn`
- [ ] `_codecs_hk`
- [ ] `_codecs_iso2022`
- [ ] `_codecs_jp`
- [ ] `_codecs_kr`
- [ ] `_codecs_tw`
- [ ] `_csv`
- [ ] `_ctypes`
- [ ] `_curses`
- [ ] `_curses_panel`
- [ ] `_dbm`
- [ ] `_decimal`
- [ ] `_elementtree`
- [ ] `_gdbm`
- [ ] `_hashlib`
- [x] `_heapq`
- [ ] `_hmac`
- [ ] `_interpchannels`
- [ ] `_interpqueues`
- [ ] `_interpreters`
- [ ] `_json`
- [ ] `_lsprof`
- [ ] `_lzma`
- [ ] `_md5`
- [ ] `_multibytecodec`
- [ ] `_multiprocessing`
- [ ] `_pickle`
- [ ] `_posixshmem`
- [ ] `_posixsubprocess`
- [ ] `_queue`
- [ ] `_random`
- [ ] `_remote_debugging`
- [ ] `_sha1`
- [ ] `_sha2`
- [ ] `_sha3`
- [ ] `_socket`
- [ ] `_sqlite3`
- [ ] `_ssl`
- [ ] `_statistics`
- [ ] `_struct`
- [ ] `_tkinter`
- [ ] `_uuid`
- [ ] `_zoneinfo`
- [ ] `_zstd`
- [ ] `array`
- [x] `binascii`
- [ ] `cmath`
- [ ] `fcntl`
- [ ] `grp`
- [ ] `mmap`
- [ ] `pyexpat`
- [ ] `readline`
- [ ] `resource`
- [ ] `select`
- [ ] `syslog`
- [ ] `termios`
- [ ] `unicodedata`
- [ ] `zlib`

#### Unknown / platform-specific (8)

- [!] `_overlapped`
- [!] `_scproxy`
- [!] `_winapi`
- [!] `_wmi`
- [!] `msvcrt`
- [!] `nt`
- [!] `winreg`
- [!] `winsound`

**Acceptance P7:** all 272 ledger entries are accounted for, and every
297/297 module is either parity — importable, representative API matching
CPython 3.14.9, and its mapped `Lib/test/test_*.py` subset green — or
documented platform-unsupported. `[x]` in the ledger marks the import/API
slice only; parity is decided by P7-05, never by an import alone. The
generated compatibility matrix must show the same status and counts.

## Native Bindings (NB)

Umbrella program for compiled bindings, the CPython ABI layer and foreign
runtimes. The normative contract and acceptance gates live in
[`NATIVE_BINDINGS.md`](NATIVE_BINDINGS.md) §8; HNA is specified in §3 and
`include/hydro_native.h`. Tier 3 rules are in [`C_API.md`](C_API.md) and the
current stdlib matrix in [`COMPATIBILITY.md`](COMPATIBILITY.md). Statuses here
mirror that table and do not upgrade it: Tier 3/NB-4 is partial, Tier 2/NB-2
is implemented for the v1 slice, and Tier 4/NB-6 is a tested pilot.

- [x] NB-0 Baseline: Tier 0 importer, Tier 1 module table, Tier 3 abi3 slice +
      loader, no `libpython`.
- [x] NB-2 HNA v1: `hydro_native.h`, `hydro-hna` descriptor crate, loader and
      `.hna.so` discovery, persistent `hydro_root_hold`/`hydro_root_release`
      roots, typed projections, version/platform/name validation, manifest
      verification when present, panic containment, Rust/C/Go fixtures under
      JIT and AOT, strict-GC loop, `ldd` clean; boundary cost measured at
      677.5 ns/call JIT and 680.9 ns/call AOT (release, 1M calls).
- [x] NB-3 Bindgen (delivered slice): `hydro-bindgen generate|check`, libclang
      header ingestion, C type table, generated descriptor/spec/manifest and
      JSON report, byte-for-byte `--check`, generated libm differential and
      handwritten sqlite HNA differential under JIT/AOT.
- [x] NB-3 panic fixture: a deliberately panicking HNA Rust fixture aborts
      the process (SIGABRT) without unwinding into Hydro.
- [x] NB-5 Packaging and coexistence: manifest parsing/verification, `hydro1_0`
      wheel tag, verbatim upstream `RECORD`, ambiguity diagnostics and mixed
      `.py`/`.hna.so`/CPython-extension artifact tests (Tea).
- [x] NB-6 Foreign runtime bridge (pilot): `_hydro_sidecar` protocol v1
      (`spawn`/`call`/`restart`/`close`/`alive`) with a CPython worker, plus a
      C HNA adapter fixture; no JVM/.NET/Node adapter.
- [ ] NB-1 Tier 1 completeness: every native stdlib module has one `ModuleSpec`
      entry, a Rust implementation and a CPython differential test. Batch A,
      the POSIX basics, `array`, `cmath`, `_signal`/`termios`, `hashlib`/
      SHA-3/HMAC/UUID, AST nodes, importlib finders, `_sqlite3`, `_bz2`,
      `_lzma`, `pyexpat`, `readline`, `_curses`, CJK/ISO-2022 codec
      accelerators, `_dbm`/`_gdbm`, `_zstd` and the related OS/IO surfaces are
      delivered; `_ssl` (the `ssl` module itself has a context/factory
      surface), `_tkinter`, `_curses_panel`, `_lsprof`, `_symtable`,
      `_tracemalloc`, `_interpreters`/`_interpchannels`/`_interpqueues`,
      `_posixshmem`, `_posixsubprocess`, `_elementtree` and
      `_remote_debugging` remain open.
- [ ] NB-4 Tier 3 completion (ecosystem):
      - [x] Link-time symbol surface for the pinned wheels; CPython-compatible
            handle/tuple/list layout; pydantic 2.14.1 import (with the vendored
            `enum`/typing surface).
      - [x] pydantic 2.14.1 import and model smoke pass under JIT and AOT
            (TypedDict naming, lazy `pydantic.main` dispatch, enum/uuid/object
            protocol and model-field construction).
      - [ ] NumPy 2.5 import/smoke: module init passes `sys.flags`,
            `PyType_Ready` base adoption, `PyContextVar_New`, `Py_BuildValue`
            and `PyErr_Format`; it stops on a DType/ArrayMethod type check.
      - [ ] pandas 3.1 import/smoke: compilation now reaches
            `pandas/io/common.py`; the remaining blocker is `except` with a
            dynamic exception expression (`except tuple(...)`).
      - [x] Full `tp_traverse`/`tp_clear`, GIL/thread integration and the
            threaded callback + GC stress gate (`PyGC_Collect` production
            pass; visitproc order fixed).
- [ ] NB-7 Stdlib parity: 258/297 importable; 297/297 must be accounted for
      (import + mapped `Lib/test` subset green, or documented unsupported).
      Tracked by P7 above; P7 is **not** complete.

## Risks / open decisions

- R1: C-API/ABI compatibility is the biggest cost driver (PLAN.md §3).
  The environment is unblocked (CPython 3.14 dev files) and the decision is
  option B (Hydro-native ABI layer, no `libpython`); the remaining risk is the
  full 3.14 `PyTypeObject`/refcount/GC/GIL surface required by NumPy and
  `pydantic-core`.
- R2: tagged-int fast path plus BigInt promotion is implemented; remaining native i64 boundaries are documented.
- R3: Cranelift instead of LLVM: faster startup, less optimization; LLVM optional.
- R4: GC with native frames needs precise roots; conservative scanning is the
  interim (P2-06/P2-07). The fiber stack and the two context register frames
  are lifetime-registered conservative roots (with stack-bounds override);
  precise stack maps remain open.
- R5: `uv` interpreter coupling may force a fork; currently passthrough,
  decision documented (no fork without a blocker).
- R6: Dynamic features (monkey-patching, `eval`, metaclasses) vs. AOT — tiering
  or an explicit support scope needed.
- R7: Generator/coroutine remainder: async generators/comprehensions and
  mutable closure captures. Generator expressions are lazy (synthetic
  generator function); `async for`/`async with` use the synchronous protocol.
- R8: Performance weak spots: string concatenation remains O(n²)
  (`string_concat` ~1.3–1.5x faster than CPython only because of lower
  constant factors) and CPython-extension boundaries will add conversion
  cost. Native module dispatch is fixed (thread-local spec cache; `math_loop`
  at parity/faster). The x86_64 fiber switch is register-only; the 100k
  generator sum is AOT 6.8 / JIT 7.7 ms versus CPython 10.2 ms (same run).
  No optimization claims without measurement.
