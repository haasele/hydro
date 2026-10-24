# Changelog

All notable changes to PyX. The format follows
[Keep a Changelog](https://keepachangelog.com/de/1.1.0/), versioning follows
[SemVer](https://semver.org/lang/de/). As long as no public release
exists, `[Unreleased]` collects the current state; `0.1.0` is the
initial state of the first runnable core.

## [Unreleased]

As of 2026-10-24: **875 tests**, `scripts/verify.sh` ALL GREEN. The first abi3
extension slice runs under JIT and AOT without `libpython`; the full C
API/ABI, stdlib parity and ecosystem work remain open (see `docs/C_API.md`).

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
  with this slice to **875** tests (incl. `crates/pyx-cli/tests/fibers.rs`,
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
- **Native module dispatch cache:** `pyx_module_call`/`call_native` borrow
  names without allocating and cache the resolved spec per thread; `math_loop`
  moved from 0.81x to parity/faster (AOT 22.50 ms vs. CPython 22.65 ms at
  n=200000; 50k micro harness 6.2 ms vs. 13.6 ms).
- **Generator protocol complete:** `yield` expressions (`x = yield v`),
  generator `send(v)`, `throw(exception_instance)`, `close()` (GeneratorExit,
  `finally` execution, RuntimeError when a closing generator yields) and
  `yield from` (send/throw/close forwarding, delegate return value);
  StopIteration on exhaustion; tests against CPython behaviour.
- **CLI polish (P6-07):** `pyx completions <bash|zsh|fish>`, roff manpage
  `docs/pyx.1`, and `pyx --version --verbose` (target, backend, runtime).
- **C-API containers:** `PyUnicode_*`, `PyFloat_*`, `PyBytes_*`, `PyList_*`,
  `PyTuple_*`, `PyDict_*` plus bounded recursive list/tuple/dict conversion
  in both directions (copies, depth 64, cycle rejection).
- **C-API capsules & buffers:** `PyCapsule_New/GetPointer/GetName/IsValid/
  SetPointer` with a PyX `PyCapsule` kind, read-only contiguous bytes
  `PyObject_CheckBuffer/GetBuffer/ReleaseBuffer` (verified `Py_buffer`
  layout), and `PyExc_*` singletons with typed `PyErr_SetString` mapping.
- **Installed extension discovery:** extension lookup also searches
  `PYTHONPATH`, `PYX_SITE_PACKAGES`, `VIRTUAL_ENV`/`CONDA_PREFIX`
  site-packages, and wheel-style `<name>.cpython-314-*.so` filenames.
- **Dotted extension submodules & ABI policy:** `pkg.sub_ext` resolves to
  `<dir>/pkg/sub_ext*.so` without package Python source (aliased and
  from-import forms); incompatible ABI tags (`cpython-312`, `pp310`, ...) are
  compile-time diagnostics instead of dlopen attempts.
- **Multi-phase init:** `PyModuleDef_Init` + bounded `Py_mod_exec` slot
  execution with policy-slot validation, alongside single-phase
  `PyModule_Create2` modules; JIT+AOT fixture.
- **Clippy gate:** `scripts/verify.sh`/CI run clippy without `-D warnings`;
  the tree is kept at zero warnings.
- **Stdlib compatibility baseline:** `scripts/stdlib_compat.py` +
  `docs/COMPATIBILITY.md` + `benchmarks/compat/latest.json`; 23 of 297
  CPython 3.14 modules importable on 2026-10-24 (import success only, not
  test-suite success).
- **First abi3 extension slice:** `pyx-runtime/src/capi/` implements a
  limited-API host surface (`PyModule_Create2`, `PyModule_AddStringConstant`/
  `AddObject`, `PyLong_From/AsLong`, variadic `PyArg_ParseTuple` via a small C
  file, refcount entry points, `PyErr_*` basics, symbol anchor). The frontend
  discovers sibling `.so`/`.abi3.so` files, lowers extension calls/attributes,
  and the loader (`modules/extension.rs`) caches modules by path. `pyx` and AOT
  binaries are linked with `--export-dynamic`; the fixture imports and calls
  under JIT and AOT with no `libpython` (`crates/pyx-cli/tests/extensions.rs`).
  Open: package submodules, multi-phase init, wheel tag policy, full ABI.
- **P4-04 decision (2026-10-24): option B** — a PyX-native CPython C-API/ABI
  layer without embedded `libpython`; CPython 3.14 headers/toolchain are used
  at build time and the interpreter as a differential test oracle. Probes:
  embedded CPython init 6.67 ms + finalize 7.46 ms, `import numpy` 41.64 ms;
  a stable-ABI fixture imports with four undefined CPython symbols
  (`spikes/p4_04/`).
- **Full-compatibility program:** P4-09 (297 stdlib modules: 174 pure Python,
  71 extension, 52 builtin/frozen), P4-10 (ecosystem suites: NumPy, pandas,
  pydantic, FastAPI, Django, …) and P4-11 (ecosystem benchmarks) added to
  `docs/TODO.md`.

### Known limitations

- Generators/coroutines: `send`/`throw`/`close`, `yield` expressions,
  async generators and async comprehensions are missing;
  `asyncio` offers only `run`/`sleep`/`gather`, `async for`/`async with` use
  the synchronous protocol. Generator expressions remain eagerly materialized.
- Performance weaknesses: string concatenation is O(n²) and float boxing is
  per-value; extension boundaries will add conversion cost. Native module
  dispatch and generator resume are no longer bottlenecks (math_loop at
  parity; 100k generator sum AOT 6.8 ms vs. CPython 10.2 ms).
- Classes with only one base and without class attributes/decorators/metaclasses/
  descriptors; closures are by-value (no mutable capture).
- User modules only as sibling `.py` (no packages/dots/relative
  imports), without top-level execution and without runtime module objects.
- `re` uses Rust `regex` syntax (no lookarounds/backreferences);
  `socket` only blocking TCP; `random` not CPython-reproducible.
- GC conservative, without precise roots/stack maps or size-class allocator.
- C API/C extensions: decision B (PyX-native ABI layer, no `libpython`).
  A limited-API fixture runs under JIT and AOT, but the shim covers only the
  first symbol subset (no capsules/buffer/container constructors, no
  wheels/venvs, no full 3.14 ABI). The 297-module stdlib parity, ecosystem
  suites and benchmarks are open (P4-04a…P4-11).

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
