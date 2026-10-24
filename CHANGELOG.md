# Changelog

All notable changes to PyX. The format follows
[Keep a Changelog](https://keepachangelog.com/de/1.1.0/), versioning follows
[SemVer](https://semver.org/lang/de/). As long as no public release
exists, `[Unreleased]` collects the current state; `0.1.0` is the
initial state of the first runnable core.

## [Unreleased]

As of 2026-10-24: **771 tests**, `scripts/verify.sh` ALL GREEN. C API remains
blocked (see `docs/C_API.md`).

### Added

- **Formatting:** f-string format specs (`:.2f`, `:04d`, `:#x`, alignment/
  fill character/sign/grouping) as well as `!s`/`!r`/`!a`; `str.format(...)`;
  printf formatting (`"%s:%d" % (...)`); shared spec implementation and
  ABI `pyx_format_value`.
- **Attribute & method keywords:** `pyx_getattr`/`pyx_setattr` and
  data attributes for `pathlib.Path` (`name`, `stem`, `suffix`, `parent`,
  `path`), files (`name`, `mode`, `closed`, `encoding`), `Thread` and
  `Process`; `pyx_method_kw` for native methods (e.g.
  `Path.mkdir(parents=True)`).
- **Builtin function values:** builtins can be used as values (`f = int`,
  `map(f, xs)`).
- **Classes:** `class Name(Base)` with one base, methods, `__init__`,
  inheritance/overrides, instance attributes, and dynamic method dispatch;
  instances/classes have deterministic representations.
- **Closures:** nested `def` captures outer locals **by value**;
  ABI `pyx_closure_new`, captures as leading wrapper parameters.
- **Lazy generators:** `yield value`/bare `yield` run in a stackful
  fiber (`core/fiber.rs`, own 128 KiB stack) that starts only on the first
  `pyx_iter_next`; every generator function creates an independent
  object, `return` ends the iteration, exceptions propagate. On x86_64,
  a `global_asm!` `pyx_context_switch` switches only the callee-saved
  registers (`rsp`, `rbx`, `rbp`, `r12`–`r15`, `ret`), without syscalls;
  `ucontext` remains the fallback for other targets. ABI
  `pyx_generator_new`/`pyx_yield`; new builtins `iter(x)` and
  `next(it[, default])` (`pyx_iter`/`pyx_next`). Generator expressions
  remain eagerly materialized.
- **Fiber performance:** the register-only switch costs two switches per
  `yield` without syscalls; a 100k generator sum runs in release at CPython
  11.0 ms / JIT 7.7 ms / AOT 6.8 ms, `strace -c` counts 94 syscalls in total
  instead of 200,097 (two `rt_sigprocmask` per yield).
- **Coroutines/event loop:** `async def` creates coroutine objects
  (`pyx_coroutine_new`), `await`/`pyx_await` drives them; `asyncio.run` runs
  a cooperative single-thread loop, `asyncio.sleep` suspends only the
  current fiber (blocking outside a loop), `asyncio.gather`
  starts concurrent tasks in input order and propagates the
  first error. No GIL.
- **GC statistics:** `pyx gc [--json] <file.py>` (runs like `pyx run`, then
  collections/live_objects/threshold/pause_total_us/pause_max_us/
  mark_total_us/sweep_total_us/freed_total/stress), public API
  `gc_stats()`/`reset_gc_stats()`/`GcStats`; `PYX_GC_STATS=1` remains.
- **Dict hash index:** `DictData` (insertion order + hash index with
  Python equality, first position/last value, numeric key equivalence,
  reindex on `pop`); lookup/insert O(1) on average instead of linear.
- **Int fast paths:** codegen emits checked `+`/`-`/`*` and comparisons for
  statically `Int`-typed operands (including AugAssign and fully
  int-typed `for range` counters); overflow falls back to `pyx_binop`.
- **Frontend optimizations:** constant folding (int/float/str/Binops/
  comparisons/unary, overflow- and division-by-zero-protected) and
  dead-`if`/`while False` elimination.
- **Benchmarks:** `benchmarks/bench_pyx.py` (CPython/JIT/AOT, 11 workloads
  incl. lazy generator sum and async `gather`, output-equality gate),
  registered as `pyx` in `benchmarks/bench_all.py`
  (`uv run benchmarks/bench_all.py --only pyx`).
- **`with` protocol:** `__enter__`/`__exit__` for files, `Pool`, and
  user classes, nested items in reverse order;
  augmented subscript assignment evaluates the target object/index once.
- **User modules:** `import foo` / `from foo import f` via sibling files,
  module cache per canonical path, cycle detection, `__name__`, and
  AOT embedding; imported modules must not have top-level side effects.
- **Stdlib:** `os.path`, `argparse` (subset, `Namespace`),
  `collections.defaultdict`, `re` (Rust `regex`, `Pattern`/`Match`),
  `socket` (blocking TCP), `asyncio` (`run`/`sleep`/`gather`).
- **CLI/toolchain:** `pyx new`, `init`, `test` (plain programs, `--json`),
  `fmt` (`--check`), `lint` (`--json`), `clean` (`--dry-run`); `add`/`remove`/
  `sync`/`lock` as uv passthrough (`PYX_UV`).
- **Build cache:** `.pyx/cache` with a deterministic key over source code,
  reachable modules, target, version, and runtime archive; `--no-cache`,
  `pyx clean`.
- **Scaling benchmarks:** `pool_serial`, `pool_parallel`, `process_queue` in the
  micro harness and in the baseline/threshold file.
- **Documentation:** `docs/C_API.md` (P4-04 spike incl. environment probe and
  blocker), `docs/LAYOUT.md` (source tree), updated
  `docs/ARCHITECTURE.md`, `README.md`, `AGENTS.md`, `docs/TODO.md`.

### Changed

- **Source restructuring:** `pyx-ir/{model,ir,stdlib}`,
  `pyx-frontend/{lower,sema}`, `pyx-codegen/{backend,lower}`,
  `pyx-runtime/{abi,core,gc,modules,concurrency,support,csv}`,
  `pyx-cli/{cli,cli/commands}`; no behavior change, contracts unchanged.
- Test count increased from 545 to 721 tests (incl. formatting, attributes,
  classes, closures, stdlib modules, socket, user modules, CLI/build cache);
  with this slice to **771** tests (incl. `crates/pyx-cli/tests/fibers.rs`,
  dict hash index, GC statistics, generator iteration).
- AOT builds embed user modules; the cache key takes the
  entire reachable module tree structure into account.
- **Fiber GC roots:** `ctx`/`caller_ctx` (callee-saved register frames) and
  the fiber stack are lifetime-registered conservative roots;
  `push_stack_bounds` aligns the stack scan to a running fiber. The
  outermost resume publishes two aux ranges (native driver stack and
  `caller_ctx`), shared mode mirrors them, local mode gets by without atomics per
  switch. `PYX_GC_STRESS=1` covers interleaved generators.
- `bench_all.py` resolves its output directory absolutely so that child scripts
  write into the same directory.

### Known limitations

- Generators/coroutines: `send`/`throw`/`close`, `yield` expressions,
  `yield from`, async generators, and async comprehensions are missing;
  `asyncio` offers only `run`/`sleep`/`gather`, `async for`/`async with` use
  the synchronous protocol. Generator expressions remain eagerly materialized.
- Performance weakness: native module dispatch (math_loop ~0.8x); the
  register-only fiber switch is no longer a resume bottleneck (100k generator
  sum AOT 6.8 ms vs. CPython 11.0 ms).
- Classes with only one base and without class attributes/decorators/metaclasses/
  descriptors; closures are by-value (no mutable capture).
- User modules only as sibling `.py` (no packages/dots/relative
  imports), without top-level execution and without runtime module objects.
- `re` uses Rust `regex` syntax (no lookarounds/backreferences);
  `socket` only blocking TCP; `random` not CPython-reproducible.
- GC conservative, without precise roots/stack maps or size-class allocator.
- C API/C extensions (P4-04) blocked: no `python3-config`/`Python.h` in
  the current environment; no measurements/decision.

## [0.1.0] - 2026-10-24

Initial state of the first runnable development stage (545 tests): compiler,
runtime, and CLI for a defined Python subset, secured by `scripts/verify.sh`.
Later slices are listed under `[Unreleased]`.

### Added

- **Compiler:** Python parser (`rustpython-parser`), AST lowering into a
  typed IR, type inference/diagnostics in the format `file:line:col: error: …`.
- **Codegen:** Cranelift-based JIT path (`pyx run`) and AOT path
  (`pyx build`, object file + link against `libpyx_runtime.a`).
- **Runtime:** tagged i64 values, strings/floats/exceptions, dynamic
  operators, truthiness, `str()`/repr, tracebacks with frame/line references.
- **CLI:** `pyx check`, `pyx run`, `pyx build`, `pyx doctor`, `--json`,
  `--color`, defined exit codes (0/1/2).
- **Containers:** `list`, `dict`, `tuple`, `set`, `range`, iterators, slices,
  negative indices, structural equality, `len`/`in`.
- **Language features:** comprehensions (list/set/dict/generator expression),
  first-class functions and lambdas (without closures), `map`/`filter`/`reduce`,
  `enumerate`/`zip`/`sum`, `try`/`except`/`else`/`finally`, re-raise,
  f-strings (simple expressions), `with` (close protocol).
- **Modules/imports:** `math`, `functools`, `operator`, `time`, `sys`, `json`,
  `os` (basic), `io`, `csv`, `random`, `platform`, `statistics`, `pathlib`.
- **File I/O:** text and bytes mode (`r/w/a/x`, `rb/wb/ab/xb`), CSV reader/
  writer, `json.dump`/`load`; `bytes` literal/constructor, indexing, slicing,
  iteration, core methods, hex serialization.
- **Memory management:** conservative mark-and-sweep GC with shared heap,
  insertion barriers, concurrent marking, and concurrent sweep;
  `PYX_GC_THRESHOLD`, `PYX_GC_STRESS`, `PYX_GC_STATS`.
- **Concurrency (no GIL):** `_thread`, `threading` (`Thread`, `Lock`,
  `RLock`, `Event`), `queue.Queue`, `multiprocessing` (`Process`, `Queue`,
  `Pool`, fork and spawn), `pickle` with its own PYX1 format.
- **Diagnostics (P6):** stable error codes, spans, source snippets, help/
  hints, `--json` schema, `pyx doctor`, ICE handling with exit code 2.
- **Verification:** `scripts/verify.sh` (fmt, Clippy, build, tests, E2E for
  JIT and AOT, negative tests, GC stress), benchmark harness
  `benchmarks/pyx/run_micro.py` with CPython/JIT/AOT baseline.
- **Documentation:** `docs/ARCHITECTURE.md`, `docs/TODO.md`, `AGENTS.md`,
  `benchmarks/PYX.md`, `PLAN.md`.
- **License:** MIT OR Apache-2.0.

### Known limitations

- No classes, generators/`yield`, closures, `async`/`await`, decorators,
  pattern matching, BigInt, or C API/ABI compatibility (as of this
  initial cut; added later under `[Unreleased]`).
- Only a subset of the CPython stdlib; imports only at module level and only
  for the built-in modules.
- `pickle` is not CPython-compatible; dict insertion is linear, integer
  arithmetic limited to i64.
- Linux only (x86_64/aarch64); `uv`-based toolchain (phase 3) still missing.
