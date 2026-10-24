# PyX — Strategy and Plan

## Strategy

PyX is a standalone Python runtime with its own compiler and toolchain — conceptually comparable to Bun in the JavaScript ecosystem, but for Python. Developers write ordinary Python and invoke a single command such as `pyx run main.py`; the toolchain handles type checking, compilation, execution and dependency management. CPython is not the default runtime, but Python compatibility remains a first-class goal.

A runtime, a garbage collector and a native compiler are three separate components. They must be designed together, but they do not all have to be built from scratch. The compiler backend and package management should be reused wherever possible; the runtime and object model are where the project's own work concentrates.

## Scope

- **Runtime in Rust** — object model, exceptions, modules, built-ins, memory management and runtime support functions.
- **Native compiler** — parse Python syntax, check types, optimize code and emit machine code.
- **Go-like garbage collector** — concurrent mark-and-sweep, efficient allocation, short pauses and profiling.
- **Toolchain on top of uv** — reuse package installation, virtual environments, lockfiles and project management, but invoke the PyX interpreter/compiler.

uv is integrated first, not forked immediately. A fork or a dedicated wrapper is only justified if uv's architecture cannot accommodate the runtime choice.

## AOT vs JIT

AOT is the primary compiler; JIT is a later optimization, gated on measurement.

**AOT (primary).** Code is translated to native machine code before startup. Benefits: fast and predictable startup after the build, no JIT warm-up, simple deployment and native optimization.

**JIT (later).** Hot functions are compiled or re-optimized at runtime. Benefits: the compiler can observe real types and runtime behavior. Costs: added complexity, warm-up time and memory footprint. LLVM's ORC provides JIT infrastructure, including lazy and parallel compilation, so no JIT has to be built from scratch.

Roadmap for a system that should run normal Python projects with minimal friction:

1. AOT compiler with cache.
2. Optional interpreter or bytecode mode for development and the REPL.
3. Profiling and benchmarks.
4. Only then a JIT for functions where measurements show a real benefit.

A JIT is not an automatic performance win. For statically typed code, AOT already produces efficient machine code; a JIT becomes interesting mainly for optimizing dynamic Python code at runtime.

**Backend:** Cranelift 0.120 is the current backend. LLVM is a future option behind a feature flag, evaluated only if its optimization potential justifies its cost; it is not the backend today.

## Missing Components

Beyond compiler, runtime, GC and package manager, the project needs at least:

- **Python frontend and language semantics** — parser, AST, scopes, imports, exceptions, classes, decorators, generators, async/await and dynamic features. A parser alone is not enough; the language's behavior must be reproduced.
- **Standard library and Python interop** — `os`, `pathlib`, `json`, `re`, `socket`, `asyncio`, `subprocess`, `threading` and further modules. Each module must be either reimplemented or routed through a compatibility layer.
- **Module and ABI compatibility** — native Python extensions commonly use the CPython C API. Loading NumPy or PyTorch unchanged requires a compatible ABI layer, CPython as an embedded runtime, or explicitly limited compatibility. This is one of the largest parts of the project.
- **CLI and build system** — `run`, `build`, `test`, `fmt`, `lint`, `check`, `add`, `remove`, `sync` and `lock`, plus reproducible builds, incremental compilation and a local build cache.
- **Debugger, diagnostics and profiling** — Python-like tracebacks, source maps or debug metadata, native debugger integration, CPU and heap profilers, and GC metrics. Without these tools the language is hard to use day to day.

## Phase Ordering

1. **Phase 1 — Minimal compiler.** Python-like syntax, variables, functions, control flow, type inference, exceptions and native output. Not yet full Python compatibility.
2. **Phase 2 — Runtime.** Object model, strings, lists, dictionaries, modules, memory allocator and a GC that is correct first. Performance work follows measurement.
3. **Phase 3 — Toolchain.** Project files, dependency resolution, lockfiles, build cache and a `run` command. Integrate uv rather than forking it prematurely.
4. **Phase 4 — Python compatibility.** Tests against the CPython test suite, standard library, exceptions, dynamic features, then C extensions and ABI compatibility.
5. **Phase 5 — Performance.** AOT optimization, profile-guided optimization, GC tuning and only then a possible JIT. Benchmarks against CPython, PyPy, Go and comparable implementations.
6. **Phase 6 — Polish and developer experience.** First-class diagnostics, machine-readable error output, a coherent CLI and documentation.

Full Python compatibility is large enough to be a permanent, independent goal. Projects such as Nuitka and PyPy show how much effort already goes into compiler, runtime and compatibility.

## Current Status

Phases 1–6 are tracked item by item in `docs/TODO.md`. The gate is `scripts/verify.sh` on a fresh checkout; the suite currently holds 771 tests.

- **Phase 1 — done.** Compiler, JIT and AOT paths, CLI and verification are complete.
- **Phase 2 — largely done.** Includes real lazy generators and coroutines on a cooperative event loop, and a concurrent GC with a shared heap and no GIL. Open: `send`/`throw`/`close`, `yield from`, async generators/comprehensions, and precise GC roots and stack maps.
- **Phase 3 — core done.** `new`/`init`/`add`/`remove`/`sync`/`lock`, the build cache and `run` work. Open: uv interpreter injection, own lockfile/venv semantics, reproducible builds and packaging.
- **Phase 4 — started, C-API blocked.** The C-API strategy spike is environment-blocked (no `python3-config`, `Python.h` or linkable `libpython`); see `docs/C_API.md`.
- **Phase 5 — baseline in place.** Micro-benchmark harness, baseline artifacts and local regression thresholds exist; PyPy/Go comparisons, PGO and a JIT decision remain open.
- **Phase 6 — diagnostics mostly done.** Stable codes, spans, snippets, `--json` output and exit codes are implemented; ICE regression tests, completions, manpage and the CLI/diagnostic reference are open.

## Risks and Open Decisions

- **C-API/ABI compatibility** is the largest cost driver. The early spike decides the object model and refcount design; it currently has neither measurements nor an embed-vs-shim decision.
- **i64 instead of BigInt** is a documented boundary; BigInt is a later feature.
- **Cranelift vs LLVM** trades faster startup against optimization depth. LLVM remains optional.
- **Precise GC roots** are still open: conservative scanning of fiber stacks and context register frames is an interim measure, not the target design.
- **uv coupling** may force a fork; the current decision is passthrough with no fork without an architectural blocker.
- **Dynamic features** (monkey-patching, `eval`, metaclasses) conflict with AOT; they need tiering or an explicitly defined support scope.
- **Generator/coroutine remainder** — `send`/`throw`/`close`, `yield` expressions, `yield from`, async generators; generator expressions are still materialized eagerly.
- **Native module dispatch** is a known performance weakness. No optimization is claimed without measurement.

## Recommendation

AOT first, JIT later. Target a Go-like GC but prioritize correctness and measurability. Integrate uv where possible. Prototype Python C-API compatibility early, before committing to a fully new object and runtime design: ABI compatibility may decide whether the system can absorb the Python ecosystem or remains a new language with familiar syntax.
