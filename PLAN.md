# PyX — Strategy and Plan

## Strategy

PyX is a standalone Python runtime with its own compiler and toolchain —
conceptually Bun for Python: one command (`pyx run main.py`) handles
compilation, execution and dependency management. CPython is not the default
runtime, but Python compatibility is a first-class, permanent goal.

A runtime, a garbage collector and a native compiler must be designed together,
but not everything is built from scratch: reuse the compiler backend and uv for
package management; concentrate the project's own work on the runtime and
object model.

## Scope

- **Runtime in Rust** — object model, exceptions, modules, builtins, memory
  management, C-ABI shim.
- **Native compiler** — parse Python, check types, optimize, emit machine code
  (Cranelift; AOT primary, JIT behind measurement).
- **Go-like garbage collector** — concurrent mark-and-sweep, short pauses,
  shared heap, no GIL.
- **Toolchain on top of uv** — reuse project management, dependency resolution
  and lockfiles; integrate uv first, fork only on an architectural blocker.

## AOT vs JIT

AOT is the primary compiler; a JIT is a later optimization gated on
measurement. AOT gives predictable startup and simple deployment. A JIT only
pays off for dynamic Python paths that AOT cannot specialize; LLVM ORC would
provide the infrastructure, so no JIT is built from scratch.

Backend today: **Cranelift 0.120**. LLVM remains a future option behind a
feature flag, evaluated only if its optimization depth justifies the cost.

## Workstreams

Beyond compiler, runtime, GC and package manager, the project owns:

- Python frontend and semantics (parser alone is insufficient).
- Standard library and Python interop.
- Module and C-ABI compatibility — the largest cost driver.
- CLI/build system: cache, reproducible builds, diagnostics.
- Debugger, profiling and GC metrics.

## Phase Ordering

1. **Minimal compiler** — syntax, functions, control flow, inference,
   exceptions, native output.
2. **Runtime** — object model, containers, modules, allocator, correct GC
   first; performance follows measurement.
3. **Toolchain** — project files, uv integration, lockfiles, build cache, run.
4. **Python compatibility** — CPython test suite, stdlib, language features,
   C extensions/ABI.
5. **Performance** — AOT optimization, PGO, GC tuning, then a possible JIT;
   benchmarks against CPython, PyPy and Go.
6. **Polish** — first-class diagnostics, machine-readable output, coherent CLI,
   documentation.

## Current Status

Phases and acceptance criteria are tracked item by item in
[`docs/TODO.md`](docs/TODO.md); the resume status, commands and current test
counts are in [`AGENTS.md`](AGENTS.md#current-state). The gate is
`scripts/verify.sh` on a fresh checkout.

- **Phase 1 — done.**
- **Phase 2 — largely done.** Lazy generators and generator expressions,
  coroutines/event loop, shared heap with concurrent GC and no GIL. Open:
  precise GC roots/stack maps, async generators/comprehensions, mutable
  closures.
- **Phase 3 — core present.** `new`/`init`/`test`/`fmt`/`lint`/`clean`, build
  cache, uv passthrough. Open: uv interpreter injection, reproducible builds,
  packaging.
- **Phase 4 — unblocked; option B.** CPython 3.14.9 is the pinned oracle; the
  CPython C API/ABI is implemented on PyX objects without `libpython` in
  production (first abi3 slice works). The stdlib/ecosystem program is P4-01…
  P4-12 and the executable module batches P7; see [`docs/C_API.md`](docs/C_API.md)
  and [`docs/CPYTHON_TESTS.md`](docs/CPYTHON_TESTS.md).
- **Phase 5 — baseline in place.** Micro harness, tracked baseline, local
  threshold gate and CPython/PyX/PyPy/Go engine comparison. Open: inlining/
  escape analysis, PGO, GC tuning after precise roots.
- **Phase 6 — diagnostics largely done.** Stable codes, spans, snippets,
  `--json`, `--verbose`/`--quiet`, CLI/diagnostic references. Open:
  troubleshooting/migration docs, progress feedback.

## Compatibility Targets and LTS

PyX pins conformance to one CPython patch release at a time; the current
target is **CPython 3.14.9**. 3.13 is security-only and 3.15 is newer than the
oracle. The target is defined once in `pyx_ir::compat` and surfaces through
`sys.version`/`sys.version_info`, `platform.python_version()`,
`--version --verbose`, wheel tags and the test harnesses.

The 3.14 standard library has 297 modules (174 pure Python, 63 extension,
52 builtin/frozen, 8 platform-specific); module-by-module status is generated
in [`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md).

**LTS policy (future, not before production):** LTS lines are keyed to CPython
minors and freeze the patch baseline, C-ABI/wheel tags, stdlib/language surface
with its matrix, and security-patch intake/support window. No LTS line is
declared now; pre-production releases track the moving 3.14.x target (P4-12).

## Risks and Open Decisions

- **C-API/ABI compatibility** is the largest cost driver; option B preserves
  the PyX startup/GC advantages, but the full `PyTypeObject`/refcount/GC/GIL
  surface for NumPy and `pydantic-core` remains (P4-04c/d).
- **Precise GC roots/stack maps** are still open; conservative scanning is an
  interim measure, not the target design (P2-06/P2-07).
- **uv coupling** may force a fork; current decision is passthrough with no
  fork without an architectural blocker.
- **Dynamic features** (monkey-patching, `eval`, metaclasses) conflict with
  AOT; they need tiering or an explicitly defined support scope.
- **Generator/coroutine remainder** — async generators; generator expressions
  are lazy (synthetic generator function over the fiber ABI).
- **Performance claims** are made only from tracked measurements.

## Recommendation

AOT first, JIT later. Target a Go-like GC but prioritize correctness and
measurability. Integrate uv where possible. Continue the CPython C-API
prototype before committing further to a new object/runtime design: ABI
compatibility decides whether the system can absorb the Python ecosystem or
remains a new language with familiar syntax.
