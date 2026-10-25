# PyX

PyX is a standalone Python runtime in Rust with a JIT and AOT compiler and a
cargo-style toolchain. The goal is to run regular Python without CPython as the
default runtime; compatibility is currently deliberately limited to a clearly
defined subset (see [Language Scope](#language-and-stdlib-scope) and
[`docs/TODO.md`](docs/TODO.md)).

> **Status: early development (0.1.0).** Compiler/runtime, containers, classes,
> closures, formatting, file I/O, user modules, the stdlib core, lazy/async
> generators, task groups, precise GC roots, threads/processes, TLS/UDP
> networking, argparse/pytest, and an IR interpreter/REPL are working. A
> complete CPython stdlib and C extensions are still missing; PyX is neither
> fully Python-compatible nor production-ready.

## Status

| Area | State |
|---|---|
| Phase 1 — Compiler, runtime ABI, JIT/AOT | done, verified by `scripts/verify.sh` |
| Phase 2 — Runtime, GC, modules, classes, concurrency | largely done; lazy generators/coroutines + event loop, exception hierarchy/chaining, module values/globals; precise GC roots and async generators open |
| Phase 3 — Toolchain (`new`/`init`/`test`/`fmt`/`lint`/`clean`, build cache, uv passthrough) | core available; uv interpreter injection/packaging open |
| Phase 4 — Python compatibility, C API/ABI | **decision B; first abi3 slice works** (limited-API `.so` imports/calls under JIT+AOT, no `libpython`); full ABI/stdlib/ecosystem open (P4-04a…P4-11), see [`docs/C_API.md`](docs/C_API.md) |
| Phase 5 — Performance | baseline + local regression gate + CPython/PyX/PyPy/Go engine comparison |
| Phase 6 — Diagnostics/Developer Experience | largely done; CLI/diagnostic references, `--verbose`/`--quiet`, ICE and span tests; troubleshooting/migration open |

Current state of work (Cargo version 0.1.0, no public release yet): the suite
covers unit, integration and E2E tests (current count and breakdown:
[`AGENTS.md`](AGENTS.md#current-state)). Compatibility target: **CPython
3.14.9**; the official `Lib/test` baseline is 1/398 (`test_errno.py`), tracked in [`docs/CPYTHON_TESTS.md`](docs/CPYTHON_TESTS.md). JIT (`pyx run`) and AOT
(`pyx build`) share the same runtime and IR. Details:
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), work list in
[`docs/TODO.md`](docs/TODO.md), source tree in
[`docs/LAYOUT.md`](docs/LAYOUT.md).

## Performance

Medians over 5 runs, a fresh process per run, AOT excluding build time. Measured on Linux x86_64 (Intel Core Ultra 7 255H, 16 cores, 24 GB) with CPython **3.14.9** as the benchmark runner and compatibility oracle. Every subprocess is capped at 10 seconds.

```bash
PYX_SITE_PACKAGES=/usr/lib/python3.14/site-packages \
  target/release/pyx run benchmarks/bench_all.py --only pyx
```

| Workload | CPython | PyX JIT | PyX AOT | CPython/JIT | CPython/AOT |
|---|---:|---:|---:|---:|---:|
| hello | 8.40 ms | 1.88 ms | 1.19 ms | 4.48× | 7.04× |
| while_sum | 24.79 ms | 3.94 ms | 2.96 ms | 6.30× | 8.37× |
| for_range_sum | 15.48 ms | 2.84 ms | 1.98 ms | 5.45× | 7.83× |
| list_append | 15.79 ms | 6.33 ms | 5.97 ms | 2.49× | 2.65× |
| list_comp | 13.74 ms | 4.70 ms | 3.91 ms | 2.92× | 3.51× |
| dict_insert | 10.08 ms | 3.29 ms | 1.82 ms | 3.06× | 5.54× |
| string_concat | 9.64 ms | 3.21 ms | 1.95 ms | 3.00× | 4.93× |
| json_roundtrip | 16.00 ms | 3.53 ms | 2.47 ms | 4.53× | 6.47× |
| math_loop | 25.52 ms | 4.52 ms | 2.62 ms | 5.64× | 9.73× |
| generator_sum | 11.23 ms | 16.07 ms | 14.61 ms | 0.70× | 0.77× |
| async_gather | 71.94 ms | 24.04 ms | 22.11 ms | 2.99× | 3.25× |

PyX is faster on 10 of 11 workloads (JIT median **3.06×**, AOT **5.54×**); the exception is generator resume, where per-frame precise-root bookkeeping is currently the dominant cost. Startup remains substantially lower (`pyx JIT hello` 1.88 ms, `pyx AOT hello` 1.19 ms, CPython 9.00 ms). Closing the generator gap is tracked under P5.

## Requirements

- Linux, primarily x86_64 (aarch64 paths exist but are not
  CI-verified); macOS/Windows are not yet supported.
- Rust **1.85+** (edition 2021).
- A C compiler (`cc`), only for `pyx build` (linking the AOT binary).
- `uv` only for `pyx add/remove/sync/lock` (thin passthrough).
- No CPython is required at runtime for PyX programs. Python 3 is
  used exclusively for the benchmark harnesses and check scripts under
  `benchmarks/` and `scripts/`.

## Quickstart

```bash
# Build CLI + runtime
cargo build --release -p pyx-cli -p pyx-runtime

# Create a new project
./target/release/pyx new hello
cd hello

# JIT: compile and run directly
../target/release/pyx run main.py

# AOT: produce a native binary and run it
../target/release/pyx build main.py -o hello-bin
./hello-bin

# Full gate (fmt, clippy, tests, E2E, AOT, GC)
cd .. && ./scripts/verify.sh
```

`scripts/verify.sh` builds the debug binary at `target/debug/pyx`; the
examples and expected outputs are located in `examples/` and
`examples/expected/` respectively. CI runs the same procedure (see
`.github/workflows/ci.yml`).

## CLI

```
pyx new <path> [--force]           create project directory with scaffold
pyx init [path] [--force]          create project in current/given directory
pyx check [options] <file.py>      parse and type-check only
pyx run [options] <file.py> [args...]  compile and run with the JIT (args → sys.argv[1:])
pyx build <file.py> [options]      compile and link a native binary
pyx test [paths...] [options]      run test_*.py / *_test.py with `pyx run`
pyx fmt [paths...] [--check]       normalize trailing whitespace + final newline
pyx lint [paths...] [options]      type-check all .py files
pyx clean [--dry-run]              remove .pyx/cache and target/pyx-tmp
pyx gc [--json] <file.py>          run program, then print GC statistics
pyx add <spec...> [uv options]     add dependencies with uv
pyx remove <spec...> [uv options]  remove dependencies with uv
pyx sync [uv options]              synchronize environment with uv
pyx lock [uv options]              update lockfile with uv
pyx doctor [--json]                report toolchain/environment status
pyx completions <shell>            print bash/zsh/fish completion script
pyx help | --help | -h             this help
pyx --version | -V                 version
```

- Global `--verbose`/`--quiet` (cannot be combined) are accepted before the
  command and, for compiler/project commands, before the first path.
  `--version --verbose` adds target, backend, runtime and Python compatibility;
  `docs/DIAGNOSTICS.md` is the diagnostic/CLI reference.
- Diagnostic options (`check`, `run`, `build`, `test`, `lint`): `--json`,
  `--color=<auto|always|never>`, `--no-color`.
- `build`: `-o/--output <OUT>` (default: file stem in the current directory),
  `--runtime-lib <PATH>`, `--no-cache`.
- `test`: success = exit code 0 of the program (no pytest assertions);
  `--json` returns a result list. `fmt --check` does not write.
- `clean` removes only `.pyx/cache` and `target/pyx-tmp`, not the rest of
  `target/`.
- `gc` runs the program like `run` and afterwards reports `collections`,
  `live_objects`, `threshold`, `pause_total_us`, `pause_max_us`,
  `mark_total_us`, `sweep_total_us`, `freed_total`, `stress`; `--json` prints
  an object as the last stdout line.
- `new`/`init` create `main.py`, `pyproject.toml` and `.gitignore`;
  existing non-empty files are not overwritten without `--force`.
- `add`/`remove`/`sync`/`lock` pass all arguments unchanged to `uv`
  and return its exit status (`PYX_UV` selects the executable).
- Exit codes: `0` success, `1` user/program error, `2` internal/setup error.
- Environment: `CC` (linker for AOT, default `cc`), `PYX_RUNTIME_LIB`, `PYX_UV`,
  `NO_COLOR`; additionally at runtime `PYX_GC_THRESHOLD`, `PYX_GC_STRESS=1`,
  `PYX_GC_STATS=1` and `PYX_GC_PRECISE=1` (strict precise-root mode: no
  conservative stack/register scanning).

`--` ends option parsing. Everything after the source file is forwarded to the
program as `sys.argv[1:]` (`sys.argv[0]` is the source path), so diagnostic
options must precede the file.

## Language and Stdlib Scope

**Values and operators**

- `int` (arbitrary precision: tagged i64 fast path with automatic `BigInt`
  promotion; native i64 boundaries such as `range` bounds remain documented), `float`, `bool`,
  `None`, `str` (UTF-8, Unicode scalar values), `bytes`; `list`, `tuple`,
  `dict` (insertion order, O(1) lookup via hash index), `set`, `range`,
  iterators, generator/coroutine objects, function/class values.
  `iter(x)` and `next(it[, default])` are builtins.
- Arithmetic incl. `//`/`%`/`**`, comparisons, `and`/`or` (short-circuit),
  `if`/`elif`/`else`, `while`, `for`, `break`, `continue`, slices, indexing
  incl. negative indices, `in`/`not in`.

**Functions, classes, closures**

- `def`, parameters, default return, recursion; functions and lambdas are
  values, builtins likewise (`f = int`); `map`/`filter` materialize lists.
- **Closures:** mutable captured bindings use shared heap cells and support
  `nonlocal`; read-only captures keep by-value semantics. Recursive nested
  functions are a diagnostic.
- **Classes:** `class Name(Base)` with a single base, methods, `__init__`,
  `self` attributes (`pyx_getattr`/`pyx_setattr`), inheritance/overrides,
  class attributes (`x = value`), `@property`/`@classmethod`/
  `@staticmethod` and ignored `lru_cache`/`cache`/`wraps`/`abstractmethod`/
  `overload`/`runtime_checkable` decorators.
  No general decorators, metaclasses or multiple inheritance.
- **Integers:** arbitrary-precision `int` with a tagged i64 fast path and
  automatic `BigInt` promotion; exact arithmetic, comparisons, formatting
  and JSON beyond 64 bits.
- Comprehensions: lists/sets/dicts (eager containers) and **lazy** generator
  expressions, multiple `for` clauses, `if` filters.

**Formatting and attributes**

- f-strings with format specs (`:.2f`, `:04d`, `:#x`, alignment/fill characters)
  as well as `!s`/`!r`/`!a`; `str.format(...)`; printf formatting `"%s:%d" % (...)`;
  central spec implementation via `pyx_format_value`.
- Data attributes instead of methods on native objects: `path.name`,
  `file.closed`, `thread.name`, `process.pid` etc.; method keywords like
  `Path.mkdir(parents=True, exist_ok=True)`.

**Control flow, exceptions, context**

- `raise` (incl. `raise ... from ...`), `try`/`except [as]`/`else`/`finally`,
  re-raise, `except Exception`/`BaseException`, tuple handlers and Python-style
  tracebacks with explicit and implicit (`__context__`/`During handling ...`)
  chaining; unhandled exceptions exit with exit code 1.
- `with a as x, b as y:` via the `__enter__`/`__exit__` protocol (files,
  `Pool`, user classes); return values of `__exit__` do not suppress
  exceptions. `with` items are exited in reverse order.
- Augmented subscript assignment (`a[i] += x`) evaluates object and index
  exactly once.

**Generators and async**

- **Lazy generators:** a function containing `yield` creates a
  generator object when called; the body only runs on the first `next()`/`for`.
  `yield value`, bare `yield`, `return` as the end and exception propagation
  work; `for`/`list`/`sum`/`iter`/`next` operate on them. Each
  object is independent (stackful fiber, its own 128 KiB stack). On x86_64
  the context switch is a register-only `global_asm!` switch over the
  callee-saved registers without syscalls (`ucontext` only as a fallback for other
  targets).
- **Coroutines/event loop:** `async def` creates a coroutine object;
  `await` drives it. `asyncio.run` runs a cooperative single-thread loop,
  `asyncio.sleep` suspends only the current fiber (blocking outside a loop),
  `asyncio.gather` starts concurrent tasks, returns the
  results in input order and propagates the first error.
  Generators support `send`/`throw`/`close`, `yield` expressions and
  `yield from` (`throw` accepts exception instances only).
  Not supported: async generators, async comprehensions; `async for`/
  `async with` use the synchronous protocol. Generator expressions are lazy
  (synthetic generator function; the outermost iterable is evaluated at
  creation, the remaining clauses run per resume).

**Modules and imports**

- Native modules (see below) and **user modules**: `import foo` /
  `from foo import f` resolve a sibling file, a package (`foo/__init__.py`),
  dotted and relative submodules, `PYX_SITE_PACKAGES`, venv/conda
  site-packages or `PYTHONPATH`. Each unit loads once per canonical path,
  imported module bodies run once as initializers, and import cycles are
  diagnosed. `from x import *` and a general `sys.modules`/`sys.path` model
  are missing.
- Standard-library modules are first-class values (`m = errno`,
  `getattr(errno, name)`) and expose their constants through `pyx_module_get`.
  Module-level assignments are visible inside functions and methods.
- `__name__` is `"__main__"` in the entry file. There is no general runtime
  module object or `sys.modules`; user-module values are still rejected.

**Bundled modules** (native, without CPython):

`math`, `functools`, `operator`, `time`, `sys`, `json` (`dumps`/`loads`,
`dump`/`load`), `os` (basic, `remove`/`rename`, `fork`/`waitpid`/`_exit`),
`os.path` (`join`, `basename`, `dirname`, `exists`, `isfile`, `isdir`,
`abspath`, `splitext`, `split`), `io`/`open` (text and bytes), `csv`,
`argparse` (subset, `Namespace`), `collections.defaultdict`, `re`
(Rust `regex`: `search`/`match`/`fullmatch`/`findall`/`sub`/`split`,
`Pattern`/`Match`), `socket` (blocking TCP), `random`, `platform`,
`statistics`, `pathlib`, `_thread`, `threading`, `queue`, `multiprocessing`,
`pickle`, `asyncio` (only `run`/`sleep`/`gather`), `errno` (full Linux
constant table, `errorcode`, module `__dict__`), `typing`, `abc`, `enum`,
`types`, `itertools`, `ast`, `fractions`, `importlib`, `__future__`,
`dataclasses`, `datetime`, `contextlib`, `html`, a minimal `unittest`
(`TestCase` + runner via `main`, native assertions), and Batch A of the
standard-library program (`bisect`, `heapq`, `colorsys`, `keyword`, `stat`,
plus the `_bisect`/`_heapq`/`_stat`/`_operator`/`_functools`/`_io` aliases
and `binascii`, `base64`, `quopri`, `fnmatch`, `getopt`, `mimetypes`,
plus the benchmark-critical `subprocess`, `tempfile`, `shutil`, `textwrap`,
`timeit`).

**Concurrency and memory**

- No GIL; threads share a heap with a concurrent mark-&-sweep GC
  (short stop-the-world windows for root snapshot/rescan).
- `threading` (`Thread`, `Lock`, `RLock`, `Event`, `queue.Queue`),
  `multiprocessing` (`Process`, `Queue`, `Pool`, fork and spawn, context
  manager for `Pool`), `pickle` with its own, **not CPython-compatible**
  PYX1 format.
- Shared containers must be protected by user locks;
  unprotected mutations are data races.

**Not supported (selection)**

Async generators and async comprehensions (`asyncio` only
`run`/`sleep`/`gather`), user decorators, pattern matching, metaclasses,
multiple inheritance, the general descriptor protocol, `bytearray`/
`memoryview`, `except*`; arbitrary C extensions or the C API/ABI and the
complete CPython stdlib (63/297 modules importable; see
[`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md)). Precise GC roots/stack maps
and variable payload size classes are missing. A general `sys.path`/
`sys.modules` model, namespace/zip/frozen imports and `from x import *` are
open; packages, dotted/relative imports and `site-packages` resolution work
through the module loader, and imports inside functions are deferred until
the enclosing code runs. Package management is only a uv passthrough (no
interpreter injection/packaging).

## Architecture

```
Source .py (+ imported sibling modules, packages, site-packages) → rustpython-parser → pyx-frontend
                                                          ↓
                                          typed IR (pyx-ir)
                                                          ↓
                     pyx-codegen (Cranelift) → JIT (pyx run)
                                             → object + cc link (pyx build)
                                                          ↓
                                pyx-runtime (libpyx_runtime.a, C ABI)
```

| Crate | Responsibility |
|---|---|
| `crates/pyx-ir` | typed HIR, contract between frontend and codegen, stdlib specs |
| `crates/pyx-frontend` | parsing, module loader/user modules, lowering, type inference, diagnostics |
| `crates/pyx-runtime` | value model, operators, builtins, exceptions, GC, I/O, modules, concurrency |
| `crates/pyx-codegen` | Cranelift codegen for the JIT and AOT path |
| `crates/pyx-cli` | `pyx` binary (project, compiler, test/fmt/lint/clean, uv) |

Internally the crates are structured into `model`/`ir`/`stdlib`, `lower`/`sema`,
`backend`/`lower`, `core`/`gc`/`modules`/`concurrency`/`support` and
`cli/commands`; the complete tree is in
[`docs/LAYOUT.md`](docs/LAYOUT.md). The ABI and IR contracts are authoritatively
documented in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Tests and Benchmarks

- `./scripts/verify.sh`: `cargo fmt --check`, Clippy (`-D warnings`), build,
  `cargo test --workspace`, E2E comparison JIT vs. AOT,
  negative tests (diagnostics/tracebacks) and GC stress tests.
- CI: [`.github/workflows/ci.yml`](.github/workflows/ci.yml) (Ubuntu, stable
  Rust, fmt/clippy/test/verify, no secrets).
- Benchmarks: [`benchmarks/PYX.md`](benchmarks/PYX.md) describes readiness and
  integration of the CPython harnesses; `benchmarks/pyx/run_micro.py` measures
  CPython vs. PyX JIT vs. PyX AOT (incl. thread/process/pool scaling) and
  writes `benchmarks/pyx/results/latest.json`. `benchmarks/bench_pyx.py`
  compares 11 pure Python workloads (incl. lazy generator and async
  `gather`) under CPython/JIT/AOT and is registered as `pyx` in `bench_all.py`;
  the 10-second per-run configuration is documented in the Performance
  section. Generic aliased string concatenation remains O(n²), and generator
  suspension is the narrowest compute-bound result.
- Regression thresholds: `benchmarks/pyx/thresholds.json` check the JIT/AOT
  medians of the core workloads:

  ```bash
  python3 benchmarks/pyx/run_micro.py
  python3 scripts/check_benchmarks.py            # --check, exit 1 when exceeded
  python3 scripts/check_benchmarks.py --update   # rewrite baseline
  ```

  The gate compares only workloads that are present in both the result and
  threshold file. Enforcement stays local/release-only: timing on shared
  runners is too noisy for a blocking CI gate. CI runs the check as a
  non-blocking report and runs `scripts/test_check_benchmarks.py`.
- Harnesses: `scripts/bench_hyperfine.sh` wraps the `bench_*.py` scripts in
  hyperfine with output validation (skips cleanly when hyperfine/uv is
  missing); `benchmarks/compare_engines.py` compares loop/startup under
  CPython, PyX, PyPy and Go and reports missing engines as skipped.
- Releases: tag-driven `.github/workflows/release.yml` builds and packages the
  Linux x64 `pyx` binary plus AOT runtime archive with tests, checksums
  and a smoke test, without
  repository secrets (other platforms: Q-09). Same-host artifact
  determinism is asserted before packaging and documented in
  [`docs/REPRODUCIBLE.md`](docs/REPRODUCIBLE.md).

## Contributing

Setup, gate and commit rules: [`CONTRIBUTING.md`](CONTRIBUTING.md). Before
larger changes, a look at [`AGENTS.md`](AGENTS.md) and
[`docs/TODO.md`](docs/TODO.md) is worthwhile (the current state of work is documented there).

## License

Licensed under **MIT OR Apache-2.0** (see `Cargo.toml`). The
full texts are in [`LICENSE-MIT`](LICENSE-MIT) and
[`LICENSE-APACHE`](LICENSE-APACHE); you may choose the terms of either
license.
