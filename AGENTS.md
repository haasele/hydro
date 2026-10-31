# AGENTS.md — Hydro Work Status

> **Resume rule:** read this file + `docs/TODO.md` before continuing.
> A step counts as done only when the command/scenario was actually executed;
> tick `docs/TODO.md` in the same change.
> History lives in `CHANGELOG.md`; this file keeps only current state.

## Project

Hydro: the standalone Python runtime/compiler monorepo in Rust. The
user-facing toolchain **Tea** is a separate repository (`../tea`) and owns
CLI/project/pytest/REPL/AOT packaging/PGO flavors. Sources of truth:
`PLAN.md` (strategy), `docs/ARCHITECTURE.md` (contracts), `docs/TODO.md`
(work list), `docs/C_API.md` (P4-04), `docs/NATIVE_BINDINGS.md` (binding
tiers, HNA, roadmap), `docs/DIAGNOSTICS.md` (diagnostics) and
`docs/LAYOUT.md` (source tree).

## Current State

| Phase | Status | Verification |
|---|---|---|
| P0 Foundation | done | CI, README, licenses, changelog, hyperfine adapter, release workflow |
| P1 Minimal compiler | **done & verified** | `scripts/verify.sh` ALL GREEN |
| P2 Runtime | **done & verified**: containers/classes/closures, lazy generators + async generators/TaskGroup, exception hierarchy/chaining, precise shadow-stack GC + size-classed payload allocator, threads/processes/pool with shutdown cleanup, TLS/UDP/select networking, argparse/pytest, IR interpreter/REPL, fuzz gate | `scripts/verify.sh` ALL GREEN |
| P3 Toolchain | moved to the separate **Tea** repository (`../tea`) | Tea gate |
| P4 Compatibility | **CPython 3.14.9 target; option B (Hydro-native ABI, no `libpython`); abi3 slice + import matrix + official-test gate; HNA v1/bindgen/sidecar/packaging delivered**; full ABI/stdlib/ecosystem open | `docs/C_API.md`, `docs/COMPATIBILITY.md`, `docs/CPYTHON_TESTS.md`, `docs/NATIVE_BINDINGS.md` |
| P5 Performance | baseline + local threshold gate + CPython/Hydro/PyPy/Go engine comparison | `benchmarks/hydro/results/latest.md`, `thresholds.json`, `compare_engines.py` |
| P6 Polish | diagnostics/JSON/doctor/ICE in Hydro; CLI/manpage/completions in Tea | `docs/DIAGNOSTICS.md`, Tea repo docs |

**Compatibility target (2026-10-29):** **CPython 3.14.9** (single definition in
`hydro_ir::compat`; 3.13 is security-only, 3.15 is newer than the oracle).
Official `Lib/test` runs are a release gate; the offline manifest check runs
in `scripts/verify.sh`/CI. LTS is policy only (P4-12).

**2026-10-30 ecosystem checkpoint:** the pinned CPython-3.14 wheels for NumPy
2.5.0, pandas 3.1.0, pydantic 2.14.1 and pydantic-core 2.50.1 install into
`.hydro/ecosystem/site`. The ecosystem gate now passes **9/13 cases** under
JIT and AOT: pydantic import + model smoke, pydantic-core, python-dateutil,
six, typing-extensions, typing-inspection, annotated-types and tzdata. The
remaining cases are NumPy import/smoke (the `_multiarray_umath` ArrayMethod
DType check) and pandas import/smoke (AOT compilation now stops on the dynamic
`except tuple(...)` handler in `pandas/io/common.py`). The ABI inventory
reports **0 missing required symbols** across 68 extension modules. Recent
unblockers: variadic `map`, `os` link/chown/utime/sync/affinity/lseek and seek
constants, `importlib.util.spec_from_loader`, `types.ClassType`/`ModuleType`,
six's `raise_from` exec shape, `open`/`__path__` in modules, multiple `*`/`**`
call operands, lambda `*args`, `itertools.chain.from_iterable`, codec
lookup/stream classes, `locale.localeconv`, `sys.hash_info/getsizeof`,
`re.Match` subscripts and `csv` quote constants.

**2026-10-29 native-bindings checkpoint:** HNA v1 is implemented and tested:
the header `include/hydro_native.h`, the `hydro-hna` crate, loader and
persistent root/projection services, `.hna.so` discovery with ambiguity and
manifest diagnostics, and Rust/C/Go fixtures running under JIT and AOT with
strict GC and no `libpython`. `hydro-bindgen` `generate`/`check` emits
deterministic crates; generated libm passes the CPython differential and the
handwritten sqlite HNA binding passes the `sqlite3` differential under
JIT/AOT. `_hydro_sidecar` protocol v1 (spawn/call/restart/close/alive) works
with a CPython worker and a C HNA adapter. The Tea wheel path emits the
`hydro1_0` tag, preserves upstream `RECORD` verbatim, and passes mixed
`.py`/`.hna.so`/CPython-extension artifact tests. Remaining limits:
`init` is lazy until first module load, hold tokens root on the calling thread
and must be released there, the manifest is optional when absent,
`thread_safe = false` serializes calls, bindgen covers a bounded C subset, and
`exec`/`eval` accept only a bounded source subset (no full runtime compiler;
NB-3 panic fixture and NB-2 boundary-cost measurement open).

**All-core gate:** `cargo nextest run --workspace` is the default in
`scripts/verify.sh`/CI (see the canonical counts below).

**2026-10-26 checkpoint:** the four open construction sites received a
substantial implementation slice: import/module registry + `HYDRO_STDLIB`,
the core builtin/frozen shims, PEP 701/750 parser and templates, the P4-02
language/runtime surface, the next full-ABI layer and a working wheel
installer. Full dynamic-import/introspection semantics, `except*` grouping
and the NumPy/pydantic ABI gate remain open; the official CPython baseline
was not regenerated after this slice.

### Test counts (canonical — update only here)

`cargo nextest run --workspace` on 2026-10-30: **2954 tests run
(2954 passed, 1 skipped)**; the per-crate breakdown below sums to 2954
(the skipped test is not included in the listed binaries):

| Crate | Tests |
|---|---:|
| runtime (unit + integration) | 1891 |
| frontend | 835 |
| codegen | 77 |
| IR | 64 |
| bindgen | 42 |
| interpreter | 26 |
| parser | 19 |
| hna | 0 |
| hna-fixture | 0 |
| sqlite-hna | 0 |

Tea (separate repository) `cargo test` is green: **154 E2E tests** plus all
other suites, including HNA 7/7, bindgen 2/2, sidecar 6/6 and wheels 15/15.
Other docs must link here instead of repeating the total.

### Compatibility checkpoint (2026-10-30)

- **Import matrix:** **261/297 modules importable** (50 builtin/frozen, 159 pure,
  52 extension; 36 fail; 9 modules fail under CPython 3.14.9 itself). The
  harness now sets `HYDRO_STDLIB` to the pinned CPython `Lib` tree; the earlier
  159 count used the toolchain's default subset. Report: `docs/COMPATIBILITY.md`, JSON
  `benchmarks/compat/latest.json`.
- **Official suite:** **3/398 passing** (`test_errno.py`,
  `test_int_literal.py`, `test_longexp.py`), 333 `hydro_fail`, 62
  `expected_fail` (CPython also fails), 0 timeouts; full differential run took
  733.8 s. Manifest `tests/compat/cpython-3.14.9.json` grows only from real
  `hydro_pass` runs.
- **Ecosystem:** **9/13 cases pass** under JIT and AOT. Pydantic 2.14.1,
  pydantic-core, python-dateutil, six, typing-extensions,
  typing-inspection, annotated-types and tzdata pass. NumPy 2.5 stops on the
  `_multiarray_umath` DType/ArrayMethod check; pandas AOT compilation now
  stops on the dynamic `except tuple(...)` handler in
  `pandas/io/common.py`. The
  ABI inventory reports **0 missing required symbols** across 68 pinned
  extension modules. Pinned ecosystem report:
  `docs/ECOSYSTEM.md`.
- **Native bindings:** HNA v1, `hydro-bindgen` and `_hydro_sidecar` protocol v1
  are implemented and tested (see the checkpoint above and
  `docs/NATIVE_BINDINGS.md` §3.7); Tier 2 is implemented for the v1 slice and
  Tier 4 is a sidecar/C-adapter pilot.
- **C-API:** limited-API fixture runs under JIT/AOT without `libpython`;
  multi-phase init, wheel tags, heap types, vectorcall, buffers, the
  CPython-compatible handle prefix and `PyTuple`/`PyList` item access, and the
  pinned-wheel link-time symbol set work. `PyGC_Collect` now drives real
  `tp_traverse`/`tp_clear` (visitproc order fixed), and threaded
  callback/GC stress plus pydantic-core/NumPy link probes pass. NumPy/pandas
  ecosystem model and lazy module imports remain open.
- **Benchmarks:** default-GC micro suite (2026-10-29) passes all 22 workloads
  plus startup: hello JIT/AOT 1.54/1.35 ms, while_sum 4.28/1.83 ms,
  math_loop 3.28/1.58 ms, async_gather 24.08/22.37 ms; generator_sum is now
  faster than CPython (JIT/AOT 5.08/4.71 ms vs 9.46 ms). Pinned
  `bench_hydro.py --repeat 9` medians: JIT 3.51×, AOT 5.45×, faster on 11/11
  workloads.

### Known limits (details in `docs/TODO.md` / `ARCHITECTURE.md` §10)

- GC is precise for generated frames/arrays, with a conservative fallback for
  Rust-side frames/tests; strict mode (`HYDRO_GC_PRECISE=1`) removes it.
  Native Rust owners must still root tagged locals explicitly.
- Async generators/comprehensions, `async for`/`async with`, cancellation,
  `TaskGroup`, `ExceptionGroup` and `wait_for` are implemented; the wider
  `asyncio` API remains open. Generator expressions are lazy; generator
  resumes remain the narrowest compute result but are now faster than CPython
  (P5 focuses on PGO/inlining).
- Full CPython stdlib/P7 parity, C extensions/full ABI behavior, and the
  ecosystem suites remain open; 261/297 stdlib modules import.
- Imports: packages, dotted/relative imports, site-packages and vendored
  `stdlib/` work; dynamic imports of unlinked package submodules, namespace/
  zip/frozen imports and `from x import *` remain incomplete.
- Language: pattern matching, `except*` and some decorator/descriptor/
  positional-only/dict-view edge cases remain incomplete; metaclasses,
  multiple inheritance, `bytearray`/`memoryview` and `__set_name__` exist.
- Stdlib is a curated subset; `re` is Rust syntax, `socket` supports TCP/UDP/
  IPv6, TLS client, non-blocking I/O and selectors; argparse covers the
  documented CPython surface, pytest is the native runner, and pickle uses
  HYD1. Full P7 parity is open.
- C-API: full link-time symbol coverage for the pinned wheels and the
  CPython-compatible handle/tuple/list layout, but full `PyTypeObject`
  behavior, `tp_traverse`/`tp_clear`, NumPy/pandas semantic ABI, full
  GIL/thread integration, wheel ecosystem suites and some `ctypes` paths
  remain open; pydantic import and model smoke pass, NumPy/pandas semantics do
  not yet.
- HNA v1 limits: `init` is lazy until first module load; hold tokens root on
  the calling thread and must be released there (the token table is
  process-wide); the manifest is optional when absent;
  `thread_safe = false` serializes calls per module; leaf modules only.
  Bindgen supports a bounded C declaration subset (no variadics, function
  pointers, `long double`, struct-by-value or mutable output pointers); there
  is no general `exec`/`eval` (bounded source subset, no runtime Python
  compiler), and boundary cost is not yet measured. The sidecar protocol is
  v1 (16 MiB frames, depth 64); no JVM/.NET/Node adapter exists. Ecosystem
  blockers: NumPy dtype/module initialization and the pandas static-typing
  false positive; pydantic now passes.
- Tea owns uv interoperability/injection and packaging; Hydro does not embed
  `uv` or a package manager.
- Linux x86_64 is the verified target; aarch64 is untested; macOS/Windows and
  cross-compilation are open.

## Next Steps

1. **NB-4 ecosystem:** close the NumPy ArrayMethod/DType check and add dynamic
   `except`-expression handlers for pandas, then thread/GIL and `ctypes`
   paths (P4-04c/d, P4-05, P4-06, P4-10/P4-11).
2. **NB-3/NB-2:** add the bindgen panic fixture, measure HNA boundary cost,
   and register `hydro-bindgen --check` in CI.
3. **NB-1/P7 stdlib:** continue Batches B/C/D/E/F and native modules, wire
   P7-05 per-module `Lib/test` subsets, then the dependency-ordered remaining
   batches.
4. **P5:** generator resume is now faster than CPython; continue with
   PGO/inlining and threshold tuning.
5. **P3:** uv interpreter injection, reproducible builds (P3-05), packaging
   (P3-08).
6. **P6:** troubleshooting/migration docs (P6-09), progress/exit summary
   (P6-10).
7. **Cross-cutting:** long fuzz campaigns (Q-03/Q-04), `unsafe` security
   review (Q-05), debugger (Q-01), cross-platform (Q-09).

## Commands

```bash
export PATH="$HOME/.cargo/bin:$PATH"   # rustc/cargo live here
cargo build                            # Hydro workspace
cargo nextest run --workspace          # all-core unit/integration tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
./scripts/verify.sh                    # Hydro gate (tests, fuzz smoke, manifest)
python3 scripts/progress.py            # README compatibility chart
cargo build --release -p hydro-runtime # staticlib for Tea/AOT
../tea/scripts/verify.sh               # Tea CLI/E2E/AOT/PGO gate
../tea/target/release/tea infuse examples/fib.py
../tea/target/release/tea build examples/fib.py -o /tmp/fib
python3 benchmarks/hydro/run_micro.py
python3 scripts/check_benchmarks.py
```


## Architecture invariants (short version)

- IR is the only frontend↔codegen contract; signatures in
  `crates/hydro-ir/src/ir.rs`/`model.rs`.
- ABI is fixed in `docs/ARCHITECTURE.md` §3; runtime functions live in
  `hydro_runtime::abi`, `extern "C"`, `#[no_mangle]`, **without panics**.
- Values: tagged `i64`; ints/bools/None immediate (BigInt promoted on
  overflow); heap objects 8-byte aligned with a kind header.
- Errors go through the pending exception + `hydro_exc_pending()` checks in
  codegen; explicit and implicit chaining are rendered deepest-first.
- Heap objects are managed by the conservative concurrent mark-&-sweep GC
  (shared heap, insertion barriers, safepoints); precise roots are open.
- Backend: Cranelift 0.120; LLVM later optional.
- No silent miscompiles: unsupported syntax → diagnostic.
- Docs invariants: `ARCHITECTURE` (contracts), `TODO` (work list), `C_API`,
  `NATIVE_BINDINGS` (binding tiers/HNA roadmap), `DIAGNOSTICS`, `LAYOUT` and
  this file must agree; test counts live here and nowhere else; no invented
  measurements or "done" claims.

## Working agreement for subagents

- Every agent owns **disjoint files**; no edits to other agents' crates.
- Agents only edit; they do **not** run builds/tests/formatters.
- Do not change contracts (IR/ABI) on your own authority; report deviations.
- The orchestrator builds, tests, formats and dispatches fix-ups.

## Recent milestones (2026-10-25)

See `CHANGELOG.md` for details; this is only the resume pointer.

- BigInt exact integers (tagged i64 fast path + heap promotion) across IR,
  ABI, arithmetic, formatting, JSON, keys and HYD1 serialization.
- Implicit `__context__` exception chaining with `__suppress_context__`;
  tracebacks render "During handling ..." deepest-first.
- CLI `--verbose`/`--quiet` (conflicts exit 2), phase timings/cache decisions,
  multi-character semantic spans and an ICE classification test; diagnostic
  catalog (`docs/DIAGNOSTICS.md`) and manpage refreshed.
- Deterministic container/GC property tests (SplitMix64 models, no new deps)
  and dependency-free fuzz smoke tests for the frontend and runtime ABI.
- Same-host AOT reproducibility lives in the Tea repository
  (Tea repository: `docs/REPRODUCIBLE.md`, `scripts/repro_check.sh`).
- Hyperfine adapter, engine comparison, explicit threshold `--check` +
  deterministic gate tests, non-blocking CI threshold report, tag-driven
  `release.yml` for Linux x64.
- Stdlib import matrix advanced to 60/297 (html/contextlib included); full
  `bench_all.py` runs under Hydro with a raised GC threshold.
