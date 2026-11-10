# Hydro compatibility strategy

Status: working plan for CPython 3.14.9 compatibility. This document is the
long-running program; `AGENTS.md` keeps current state and `docs/TODO.md` keeps
the actionable list.

## Sources of truth

1. Pinned CPython 3.14.9 source tree and oracle interpreter.
2. `Lib/test/test_*.py` differential results (Hydro vs CPython exit status).
3. The stdlib import matrix (`docs/COMPATIBILITY.md`).
4. Pinned ecosystem wheels (`docs/ECOSYSTEM.md`).
5. The C-API/ABI inventory and native-extension link probes (`docs/C_API.md`).
6. Performance/benchmark gates (`docs/PERFORMANCE.md`).

A test passes only when the oracle passes and Hydro exits 0. Import success is
not behavioral success. No result may be fabricated or marked passing because
it was skipped, timed out, or only the first error was hidden.

## Three-layer model

### 1. Language and runtime core

Implement Python semantics once, not per module: runtime `NameError` for
unknown global reads, C3 MRO and `super()`, descriptor/data-model precedence,
class construction with `__new__`/`__init__`, exception groups, generators,
async, dynamic `compile`/`exec`/`eval`, surrogate-preserving strings.

Core APIs must expose real state (frames, GC, recursion, code objects). If a
test needs an internal CPython detail that Hydro cannot represent, expose a
truthful unsupported error; never fabricate success.

### 2. Standard library

- Pure-Python modules come from the pinned CPython `Lib` tree. Their attributes
  are **dynamic**; the compiler must not require a static name for every
  member. Native specs stay authoritative only for native modules.
- Native modules are implemented in Rust with oracle-derived function names,
  arities, constants, and exception classes.
- Extension modules load from compatible `lib-dynload` artifacts; incompatible
  ABIs produce real `ImportError`/`OSError` diagnostics.
- Modules with a real Python implementation must not be shadowed by a partial
  native shim.

### 3. C-API/ABI

Preserve the CPython-compatible object header, type slots, refcount/GIL model,
tuple/list/unicode layout, buffer protocol, and multi-phase initialization.
Prefer real extension loading over reimplementation when the ABI matches.

## Reliability rules

1. **Root causes before symptoms.** A 100-test cluster usually has one or two
   causes. Never patch the first error in each test.
2. **Dynamic where Python is dynamic.** Static diagnostics are only for native
   modules with authoritative specs and for module-body typos.
3. **No partial shims.** A module either exposes real behavior or reports the
   exact unsupported operation. No empty dict/list/bytes placeholders.
4. **No red commits.** Verify a cluster with the logged runner, then commit a
   focused phase. The WIP branch may hold unverified work, but `main` does not.
5. **Bounded verification.** Serialize test runs, cap rustc/test threads,
   record available memory and process-group RSS, kill on low memory and on
   hard timeout, and keep full per-test logs.
6. **Oracle-derived specs.** New native names/signatures come from the pinned
   interpreter or CPython source, not from guesses.

## Tooling

- `scripts/test-lowmem.sh` — serialized, logged, memory-guarded nextest runner.
- `scripts/cpython_triage.py` — resumable official-suite triage with full logs,
  bounded workers, checkpoint JSONL, and a clustered backlog.
- `scripts/spec_audit.py` — oracle-driven audit of `ModuleSpec` coverage.
- `scripts/cpython_compat.py` — authoritative full differential run/manifest.
- `scripts/stdlib_compat.py`, `scripts/ecosystem_compat.py` — import/ecosystem
  matrices.
- `scripts/progress.py` — user-facing compatibility chart.

## Execution phases

### Phase 0 — Stabilize

- Clear workspace-test regressions.
- Keep `scripts/test-lowmem.sh` as the only local test entry point.
- Commit green sub-phases on `wip/compat-wave`; merge to `main` only when the
  workspace and manifest gates are green.

### Phase 1 — Instrument

- Land the resumable official triage runner and full logs.
- Land the spec audit.
- Produce a clustered backlog: tests affected per root error, not per file.

### Phase 2 — Dynamic imports and surfaces

- Filesystem-derived Python-source module detection (no curated lists).
- Dynamic attribute fallback for all Python-source modules.
- Materialize any stdlib module/package on demand; frozen and nested packages.
- Remove HYD0201 failures caused only by missing static metadata.

### Phase 3 — Core semantics

Priority order by fan-out:

1. class construction/MRO/descriptors/`super()`;
2. unknown-name/runtime globals and `global`/`nonlocal`;
3. exceptions, `except*`/groups, tracebacks;
4. generators/async/context managers;
5. `compile`/`exec`/`eval` and code objects;
6. surrogate-preserving text/codecs;
7. numeric/container protocols.

### Phase 4 — Standard library clusters

Work from the official backlog in parallel slices with disjoint files:
unittest/harness, imports, re/text/tokenize, struct/binary/marshal/array,
datetime/time/locale/decimal/fractions, os/io/tempfile, networking/ssl/urllib,
asyncio, C-API test modules. Each slice gets: failing tests, full logs,
target files, acceptance command, and a focused commit.

### Phase 5 — ABI and ecosystem

- Complete the C-API symbol/behavior inventory.
- Close NumPy dtype/module initialization, pandas dynamic `except`, ctypes.
- Re-run pinned ecosystem wheels under JIT and AOT.
- Add native-extension ABI matrix tests to CI.

### Phase 6 — Gates and release

- Run workspace tests, import matrix, official suite, ecosystem, benchmarks.
- Regenerate `docs/COMPATIBILITY.md`, `docs/CPYTHON_TESTS.md`,
  `docs/ECOSYSTEM.md`, manifest, and progress chart from reports.
- Promote WIP to `main` only with green gates; record exact commands.

## Near-term exit criteria

- `wip/compat-wave` workspace tests green.
- Official triage runner completes all 398 tests without client/SIGTERM loss.
- Spec audit reports no missing names for active native modules.
- Official suite pass count increases in measurable, committed clusters.
- No test process can outlive a run or exhaust host memory.
