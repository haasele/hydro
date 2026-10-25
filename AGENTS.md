# AGENTS.md — PyX Work Status

> **Resume rule:** read this file + `docs/TODO.md` before continuing.
> A step counts as done only when the command/scenario was actually executed;
> tick `docs/TODO.md` in the same change.
> History lives in `CHANGELOG.md`; this file keeps only current state.

## Project

PyX: standalone Python runtime (Rust) with AOT compiler and toolchain,
conceptually Bun for Python. Sources of truth: `PLAN.md` (strategy),
`docs/ARCHITECTURE.md` (contracts), `docs/TODO.md` (single work list),
`docs/C_API.md` (P4-04), `docs/DIAGNOSTICS.md` (diagnostics/CLI) and
`docs/LAYOUT.md` (source tree).

## Current State

| Phase | Status | Verification |
|---|---|---|
| P0 Foundation | done | CI, README, licenses, changelog, hyperfine adapter, release workflow |
| P1 Minimal compiler | **done & verified** | `scripts/verify.sh` ALL GREEN |
| P2 Runtime | largely done; containers, classes, closures, lazy generators, coroutines/event loop, exceptions/chaining, threads/processes/pool, shared heap + concurrent GC (no GIL) | per-crate tests below |
| P3 Toolchain | core present (`new`/`init`/`test`/`fmt`/`lint`/`clean`, build cache, uv passthrough) | E2E/cache tests |
| P4 Compatibility | **CPython 3.14.9 target; option B (PyX-native ABI, no `libpython`); abi3 slice + import matrix + official-test gate**; full ABI/stdlib/ecosystem open | `docs/C_API.md`, `docs/COMPATIBILITY.md`, `docs/CPYTHON_TESTS.md` |
| P5 Performance | baseline + local threshold gate + CPython/PyX/PyPy/Go engine comparison | `benchmarks/pyx/results/latest.md`, `thresholds.json`, `compare_engines.py` |
| P6 Polish | largely done; diagnostics/JSON/doctor/CLI flags/ICE/CLI reference | `docs/DIAGNOSTICS.md`, `docs/pyx.1`, E2E |

**Compatibility target:** **CPython 3.14.9** (single definition in
`pyx_ir::compat`; 3.13 is security-only, 3.15 is newer than the oracle).
Official `Lib/test` runs are a release gate; the offline manifest check runs
in `scripts/verify.sh`/CI. LTS is policy only (P4-12).

### Test counts (canonical — update only here)

`cargo test --workspace` on 2026-10-25: **1463 tests**:

| Suite | Tests |
|---|---:|
| runtime unit | 519 |
| runtime integration | 195 |
| frontend | 428 |
| codegen | 51 |
| IR | 18 |
| CLI/E2E | 252 (incl. 150 E2E) |

Other docs must link here instead of repeating the total; future agents add
tests, so this block is the only exact count.

### Compatibility checkpoint (2026-10-25)

- **Import matrix:** **60/297 modules importable** (13 builtin/frozen, 43 pure,
  4 extension; 39/131/59 + 8 platform missing; 9 modules fail under CPython
  3.14.9 itself). Report: `docs/COMPATIBILITY.md`, JSON
  `benchmarks/compat/latest.json`.
- **Official suite:** **1/398 passing** (`test_errno.py`), 334 `pyx_fail`,
  63 `expected_fail` (CPython also fails), 0 timeouts. Manifest
  `tests/compat/cpython-3.14.9.json` grows only from real `pyx_pass` runs.
- **C-API:** the limited-API fixture `pyx_capi_probe` (scalars, strings, bytes,
  floats, lists, tuples, dicts, capsules, read-only buffers) builds against
  CPython 3.14 headers, loads from a sibling `.so` and runs under JIT and AOT;
  neither the CLI nor the AOT binary links `libpython`. Multi-phase init and
  wheel-tag validation work; wheels/venvs and the full ABI are open.
- **`bench_all.py` under PyX:** the full suite completes with
  `PYX_GC_THRESHOLD` raised (official pure-Python `rich` renders; native
  `rich.status.Status` shim; AOT report written). Default-GC rendering still
  loses live values — precise roots (P2-06/P2-07) are the fix.
- **Benchmarks:** micro harness + tracked baselines; `math_loop` at parity
  (AOT 1.01x, JIT 0.94x at n=200000); 100k generator sum CPython 10.2 /
  JIT 7.7 / AOT 6.8 ms. `compare_engines.py` covers CPython/PyX/PyPy/Go and
  skips missing engines without inventing numbers.

### Known limits (details in `docs/TODO.md` / `ARCHITECTURE.md` §10)

- GC is conservative with a shared heap; precise roots/stack maps and variable
  payload size classes are open.
- Async generators/comprehensions and `asyncio` beyond run/sleep/gather are
  open; generator expressions are lazy (synthetic generator function); mutable
  closure bindings use shared heap cells with `nonlocal` support. `range`
  bounds still use native i64; async generators/comprehensions and full
  `asyncio` are explicit open programs.
- Imports: packages, dotted/relative imports and site-packages work; no general
  `sys.path`/`sys.modules`, namespace/zip/frozen imports or `from x import *`.
- Language: no user decorators, metaclasses, multiple inheritance, pattern
  matching, `except*`, positional-only params, dict views or
  `bytearray`/`memoryview`; `*args` + `**kwargs` in one call is still deferred.
- Stdlib is a curated subset; `re` is Rust syntax, `socket` is blocking TCP,
  `argparse`/`unittest` are subsets, pickle uses PYX1.
- C-API is a defined limited subset; no real wheels, NumPy/pydantic-core, GIL
  emulation or `ctypes`/`cffi` yet.
- `uv` is a passthrough: no interpreter injection/lockfile interpretation;
  reproducible builds and packaging are open.
- Linux x86_64 is the verified target; aarch64 is untested; macOS/Windows and
  cross-compilation are open.

## Next Steps

1. **P2-06/P2-07:** precise GC roots/stack maps + variable payload size
   classes; this unblocks the default-GC `bench_all.py` gate (P7-22) and
   P5-04 tuning.
2. **P7 stdlib:** continue Batches B/C/D/E/F, wire P7-05 per-module
   `Lib/test` subsets, then the dependency-ordered remaining batches.
3. **P4:** P4-04c/d full ABI/GIL, P4-05 wheel install, P4-06 `ctypes`/`cffi`,
   then ecosystem suites/benchmarks (P4-10/P4-11).
4. **P2-12:** async generators/comprehensions, async for/with and broader
   asyncio.
5. **P3:** uv interpreter injection, reproducible builds (P3-05), packaging
   (P3-08).
6. **P6:** troubleshooting/migration docs (P6-09), progress/exit summary
   (P6-10).
7. **Cross-cutting:** fuzzing (Q-03/Q-04), `unsafe` security review (Q-05),
   debugger (Q-01), cross-platform (Q-09).

## Commands

```bash
export PATH="$HOME/.cargo/bin:$PATH"   # rustc/cargo live here
cargo build                            # whole workspace
cargo test                             # unit/integration tests
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
./scripts/verify.sh                    # full phase gate
scripts/repro_check.sh                 # same-host AOT determinism (docs/REPRODUCIBLE.md)
cargo run -q -p pyx-cli -- new /tmp/demo
cargo run -q -p pyx-cli -- run examples/fib.py
cargo run -q -p pyx-cli -- build examples/fib.py -o /tmp/fib && /tmp/fib
cargo run -q -p pyx-cli -- test examples
cargo run -q -p pyx-cli -- fmt --check examples
cargo run -q -p pyx-cli -- lint examples
cargo run -q -p pyx-cli -- gc --json examples/fib.py
python3 benchmarks/pyx/run_micro.py       # CPython vs. JIT vs. AOT
uv run benchmarks/bench_pyx.py            # 11 workloads, three runners
uv run benchmarks/bench_all.py --only pyx
python3 scripts/check_benchmarks.py       # local threshold gate (--check/--update)
python3 scripts/test_check_benchmarks.py
scripts/bench_hyperfine.sh                # hyperfine adapter (skips when missing)
python3 benchmarks/compare_engines.py     # CPython/PyX/PyPy/Go loop + startup
python3 scripts/stdlib_compat.py          # regenerate import matrix
python3 scripts/cpython_compat.py --check-manifest
```

## Architecture invariants (short version)

- IR is the only frontend↔codegen contract; signatures in
  `crates/pyx-ir/src/ir.rs`/`model.rs`.
- ABI is fixed in `docs/ARCHITECTURE.md` §3; runtime functions live in
  `pyx_runtime::abi`, `extern "C"`, `#[no_mangle]`, **without panics**.
- Values: tagged `i64`; ints/bools/None immediate (BigInt promoted on
  overflow); heap objects 8-byte aligned with a kind header.
- Errors go through the pending exception + `pyx_exc_pending()` checks in
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
  ABI, arithmetic, formatting, JSON, keys and PYX1 serialization.
- Implicit `__context__` exception chaining with `__suppress_context__`;
  tracebacks render "During handling ..." deepest-first.
- CLI `--verbose`/`--quiet` (conflicts exit 2), phase timings/cache decisions,
  multi-character semantic spans and an ICE classification test; diagnostic
  catalog (`docs/DIAGNOSTICS.md`) and manpage refreshed.
- Deterministic container/GC property tests (SplitMix64 models, no new deps)
  and dependency-free fuzz smoke tests for the frontend and runtime ABI.
- Same-host AOT reproducibility: contract/evidence in
  `docs/REPRODUCIBLE.md`, checked by `scripts/repro_check.sh` and before
  release packaging.
- Hyperfine adapter, engine comparison, explicit threshold `--check` +
  deterministic gate tests, non-blocking CI threshold report, tag-driven
  `release.yml` for Linux x64.
- Stdlib import matrix advanced to 60/297 (html/contextlib included); full
  `bench_all.py` runs under PyX with a raised GC threshold.
