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
(work list), `docs/C_API.md` (P4-04), `docs/DIAGNOSTICS.md` (diagnostics)
and `docs/LAYOUT.md` (source tree).

## Current State

| Phase | Status | Verification |
|---|---|---|
| P0 Foundation | done | CI, README, licenses, changelog, hyperfine adapter, release workflow |
| P1 Minimal compiler | **done & verified** | `scripts/verify.sh` ALL GREEN |
| P2 Runtime | **done & verified**: containers/classes/closures, lazy generators + async generators/TaskGroup, exception hierarchy/chaining, precise shadow-stack GC + size-classed payload allocator, threads/processes/pool with shutdown cleanup, TLS/UDP/select networking, argparse/pytest, IR interpreter/REPL, fuzz gate | `scripts/verify.sh` ALL GREEN |
| P3 Toolchain | moved to the separate **Tea** repository (`../tea`) | Tea gate |
| P4 Compatibility | **CPython 3.14.9 target; option B (Hydro-native ABI, no `libpython`); abi3 slice + import matrix + official-test gate**; full ABI/stdlib/ecosystem open | `docs/C_API.md`, `docs/COMPATIBILITY.md`, `docs/CPYTHON_TESTS.md` |
| P5 Performance | baseline + local threshold gate + CPython/Hydro/PyPy/Go engine comparison | `benchmarks/hydro/results/latest.md`, `thresholds.json`, `compare_engines.py` |
| P6 Polish | diagnostics/JSON/doctor/ICE in Hydro; CLI/manpage/completions in Tea | `docs/DIAGNOSTICS.md`, Tea repo docs |

**Compatibility target:** **CPython 3.14.9** (single definition in
`hydro_ir::compat`; 3.13 is security-only, 3.15 is newer than the oracle).
Official `Lib/test` runs are a release gate; the offline manifest check runs
in `scripts/verify.sh`/CI. LTS is policy only (P4-12).

### Test counts (canonical — update only here)

`cargo test --workspace` on 2026-10-26: **1376 Hydro tests**:

| Suite | Tests |
|---|---:|
| runtime unit | 532 |
| runtime integration | 278 |
| frontend | 476 (incl. 4 fuzz smoke) |
| codegen | 61 |
| IR | 18 |
| interpreter unit | 11 |

Tea (separate repository) carries **325** CLI/E2E/AOT/pytest/REPL/PGO tests.
Other docs must link here instead of repeating the total.

### Compatibility checkpoint (2026-10-25)

- **Import matrix:** **63/297 modules importable** (13 builtin/frozen, 45 pure,
  5 extension; 39/129/58 + 8 platform missing; 9 modules fail under CPython
  3.14.9 itself). Report: `docs/COMPATIBILITY.md`, JSON
  `benchmarks/compat/latest.json`.
- **Official suite:** **1/398 passing** (`test_errno.py`), 334 `hydro_fail`,
  63 `expected_fail` (CPython also fails), 0 timeouts. Manifest
  `tests/compat/cpython-3.14.9.json` grows only from real `hydro_pass` runs.
- **C-API:** the limited-API fixture `hydro_capi_probe` (scalars, strings, bytes,
  floats, lists, tuples, dicts, capsules, read-only buffers) builds against
  CPython 3.14 headers, loads from a sibling `.so` and runs under JIT and AOT;
  neither Tea nor the AOT binary links `libpython`. Multi-phase init and
  wheel-tag validation work; wheels/venvs and the full ABI are open.
- **`bench_all.py` under Hydro:** the Hydro group completes at the **default GC
  threshold** (official pure-Python `rich` renders; AOT report written). The
  full suite was previously green with `HYDRO_GC_THRESHOLD` raised; the default
  gate is now the normal path. Precise roots are active by default, with
  `HYDRO_GC_PRECISE=1` strict and `HYDRO_GC_PRECISE=0` conservative fallback.
- **Benchmarks:** micro harness + tracked baselines; default-GC n=200000
  Hydro group: hello JIT/AOT 1.88/1.19 ms, while_sum 3.94/2.96 ms,
  math_loop 4.52/2.62 ms, async_gather 24.04/22.11 ms. Root-frame emission
  made generator resume the current weak spot (JIT/AOT 16.07/14.61 ms vs
  CPython 11.23 ms); P5 performance work owns that regression.
  `compare_engines.py` covers CPython/Hydro/PyPy/Go and skips missing engines.

### Known limits (details in `docs/TODO.md` / `ARCHITECTURE.md` §10)

- GC is precise for generated frames/arrays, with a conservative fallback for
  Rust-side frames/tests; strict mode (`HYDRO_GC_PRECISE=1`) removes it.
  Native Rust owners must still root tagged locals explicitly.
- Async generators/comprehensions, `async for`/`async with`, cancellation and
  `TaskGroup` are implemented; `ExceptionGroup`, `wait_for` and the wider
  `asyncio` API remain open. Generator expressions are lazy; loops root-heavy
  generator resumes are the current performance weak spot.
- Full CPython stdlib/P7 parity, C extensions/full ABI, and the ecosystem
  suites remain open; P2-13 covers the core named modules only.
- Imports: packages, dotted/relative imports and site-packages work; no general
  `sys.path`/`sys.modules`, namespace/zip/frozen imports or `from x import *`.
- Language: no user decorators, metaclasses, multiple inheritance, pattern
  matching, `except*`, positional-only params, dict views or
  `bytearray`/`memoryview`; `*args` + `**kwargs` in one call is still deferred.
- Stdlib is a curated subset; `re` is Rust syntax, `socket` supports TCP/UDP/
  IPv6, TLS client, non-blocking I/O and selectors; argparse covers the
  documented CPython surface, pytest is the native runner, and pickle uses
  HYD1. Full P7 parity is open.
- C-API is a defined limited subset; no real wheels, NumPy/pydantic-core, GIL
  emulation or `ctypes`/`cffi` yet.
- Tea owns uv interoperability/injection and packaging; Hydro does not embed
  `uv` or a package manager.
- Linux x86_64 is the verified target; aarch64 is untested; macOS/Windows and
  cross-compilation are open.

## Next Steps

1. **P7 stdlib:** continue Batches B/C/D/E/F, wire P7-05 per-module
   `Lib/test` subsets, then the dependency-ordered remaining batches.
2. **P4:** P4-04c/d full ABI/GIL, P4-05 wheel install, P4-06 `ctypes`/`cffi`,
   then ecosystem suites/benchmarks (P4-10/P4-11).
3. **P5:** recover the generator-resume root-frame overhead (default-GC
   `generator_sum` is currently slower than CPython), then PGO/inlining and
   threshold tuning.
4. **P3:** uv interpreter injection, reproducible builds (P3-05), packaging
   (P3-08).
5. **P6:** troubleshooting/migration docs (P6-09), progress/exit summary
   (P6-10).
6. **Cross-cutting:** long fuzz campaigns (Q-03/Q-04), `unsafe` security
   review (Q-05), debugger (Q-01), cross-platform (Q-09).

## Commands

```bash
export PATH="$HOME/.cargo/bin:$PATH"   # rustc/cargo live here
cargo build                            # Hydro workspace
cargo test --workspace                 # Hydro unit/integration tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
./scripts/verify.sh                    # Hydro gate (tests, fuzz smoke, manifest)
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
  `DIAGNOSTICS`, `LAYOUT` and this file must agree; test counts live here and
  nowhere else; no invented measurements or "done" claims.

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
  (`docs/REPRODUCIBLE.md`, `scripts/repro_check.sh`).
- Hyperfine adapter, engine comparison, explicit threshold `--check` +
  deterministic gate tests, non-blocking CI threshold report, tag-driven
  `release.yml` for Linux x64.
- Stdlib import matrix advanced to 60/297 (html/contextlib included); full
  `bench_all.py` runs under Hydro with a raised GC threshold.
