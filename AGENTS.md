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
tiers, HNA, roadmap), `docs/DIAGNOSTICS.md` (diagnostics),
`docs/LAYOUT.md` (source tree), `docs/COMPAT_STRATEGY.md` (long-running
compatibility program) and `docs/FULL_COMPAT.md` (exhaustive 100 % gap
inventory and gate definition).

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

**2026-11-10 ecosystem checkpoint:** the pinned CPython-3.14 wheels for NumPy
2.5.0, pandas 3.1.0, pydantic 2.14.1 and pydantic-core 2.50.1 install into
`.hydro/ecosystem/site`. The differential gate passes **5/13 cases** under
JIT and AOT: python-dateutil, six, typing-inspection, annotated-types and
tzdata. Typing-extensions and pydantic-core pass under JIT but their AOT
builds stop in `enum._proto_member.__set_name__` with
`int('keep')`; pydantic's JIT import now passes the Union generic-alias check
and stops at `annotationlib.Format.__eq__`. NumPy still stops on `_NoValue`;
pandas AOT stops on
`writer() got an unexpected keyword argument 'lineterminator'`. The ABI
inventory reports **0 missing required symbols** across 68 extension modules. Recent
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

`cargo nextest run --workspace` on 2026-11-11: **3274 tests run
(3274 passed, 1 skipped)**; the per-crate breakdown below sums to 3274:

| Crate | Tests |
|---|---:|
| runtime (unit + integration) | 2119 |
| frontend | 910 |
| codegen | 79 |
| IR | 71 |
| bindgen | 42 |
| interpreter | 34 |
| parser | 19 |
| hna | 0 |
| hna-fixture | 0 |
| sqlite-hna | 0 |

Tea (separate repository) `cargo test` is green: **154 E2E tests** plus all
other suites, including HNA 7/7, bindgen 2/2, sidecar 6/6 and wheels 15/15.
Other docs must link here instead of repeating the total.

### Compatibility checkpoint (2026-11-10)

- **Import matrix:** **288/297 modules importable** (52 builtin/frozen, 174 pure,
  62 extension; 9 fail; 9 modules fail under CPython 3.14.9 itself). The
  harness now sets `HYDRO_STDLIB` to the pinned CPython `Lib` tree; the earlier
  159 count used the toolchain's default subset. Report: `docs/COMPATIBILITY.md`, JSON
  `benchmarks/compat/latest.json`.
- **Official suite:** **36/398 full-run harness passes** (CPython and Hydro
  both exit 0; 2026-11-10T17:20:05Z, 877 s); several entries still execute 0
  tests because imported harness modules fail, recorded in
  `docs/CPYTHON_TESTS.md`. The run has 296 `hydro_fail`, 56
  `expected_fail`, 4 `timeout` (`test_bisect`, `test_imaplib`, `test_queue`,
  `test_signal`), 3 watchdog `memkill` (`test_zipfile64`, `test_zlib`,
  `test_zstd` at the 1500 MB threshold) and 3 `cpython_fail`
  (`test_extcall`, `test_unpack`, `test_unpack_ex`). A post-run recheck
  recovered `test_dictviews.py` and `test_plistlib.py`, so the current known
  total is **39**. The offline manifest still pins **33 verified passes**;
  `test_yield_from` remains the single older regressed pin. The full
  inventory is [`docs/FULL_COMPAT.md`](docs/FULL_COMPAT.md).
- **Ecosystem:** **5/13 cases pass** under JIT and AOT (python-dateutil,
  six, typing-inspection, annotated-types, tzdata). Typing-extensions and
  pydantic-core pass JIT but AOT stops in `enum._proto_member.__set_name__`
  (`int('keep')`); pydantic JIT now passes the Union generic-alias check and
  stops at `annotationlib.__eq__`. NumPy 2.5 stops on the
  `_multiarray_umath` missing-`_NoValue` path; pandas AOT stops on
  `writer(..., lineterminator=...)`. The official test runner's shared import
  blockers (`platform.win32_is_iot`, `ctypes.windll`,
  `subprocess._args_from_interpreter_flags`) are fixed, and `test.support`
  now imports after platform-branch folding, dead-import bindings, lambda
  keyword-only support and the missing harness APIs. The ABI inventory
  reports **0 missing required symbols** across 68 pinned extension modules.
  Pinned ecosystem report: `docs/ECOSYSTEM.md`.
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
  ecosystem suites remain open; 288/297 stdlib modules import.
- Imports: packages, dotted/relative imports, site-packages and vendored
  `stdlib/` work; dynamic imports of unlinked package submodules, namespace/
  zip/frozen imports and `from x import *` remain incomplete.
- Language: pattern matching, `except*` and some decorator/descriptor/
  positional-only/dict-view edge cases remain incomplete; metaclasses,
  multiple inheritance, `bytearray`/`memoryview` and `__set_name__` exist.
- Stdlib is a curated subset; `re` is Rust syntax, `socket` supports TCP/UDP/
  IPv6, TLS client, non-blocking I/O and selectors; argparse covers the
  documented CPython surface, pytest is the native runner, and pickle uses
  HYD1. `subprocess.Popen` now accepts declared keywords and `executable=`, but
  still lacks pipe `stdin`/`stdout`/`stderr` handles, `communicate()` and the
  context-manager protocol. Full P7 parity is open.
- C-API: full link-time symbol coverage for the pinned wheels and the
  CPython-compatible handle/tuple/list layout, but full `PyTypeObject`
  behavior, `tp_traverse`/`tp_clear`, NumPy/pandas semantic ABI, full
  GIL/thread integration, wheel ecosystem suites and some `ctypes` paths
  remain open; pydantic now advances to `annotationlib.Format.__eq__`;
  NumPy/pandas semantics do not yet.
- HNA v1 limits: `init` is lazy until first module load; hold tokens root on
  the calling thread and must be released there (the token table is
  process-wide); the manifest is optional when absent;
  `thread_safe = false` serializes calls per module; leaf modules only.
  Bindgen supports a bounded C declaration subset (no variadics, function
  pointers, `long double`, struct-by-value or mutable output pointers); there
  is no general `exec`/`eval` (bounded source subset, no runtime Python
  compiler), and boundary cost is not yet measured. The sidecar protocol is
  v1 (16 MiB frames, depth 64); no JVM/.NET/Node adapter exists. Ecosystem
  blockers: NumPy dtype/module initialization and pandas AOT/`lineterminator`;
  pydantic reaches `annotationlib.Format.__eq__`; see
  [`docs/FULL_COMPAT.md`](docs/FULL_COMPAT.md).
- Tea owns uv interoperability/injection and packaging; Hydro does not embed
  `uv` or a package manager.
- Linux x86_64 is the verified target; aarch64 is untested; macOS/Windows and
  cross-compilation are open.

## Next Steps

1. **100 % coverage execution:** follow [`docs/FULL_COMPAT.md`](docs/FULL_COMPAT.md)
   and the generated backlog. Immediate slices: Popen pipe streams/
   `communicate`/context manager, `contextlib._contextmanager_exit` arity,
   real `opcode` metadata (23 tests), `_testinternalcapi` (10), regex
   backreferences (8), then class/type identity and the `test.support`
   harness.
2. **NB-4 ecosystem:** close the NumPy ArrayMethod/DType check and add dynamic
   `except`-expression handlers for pandas, then thread/GIL and `ctypes`
   paths (P4-04c/d, P4-05, P4-06, P4-10/P4-11).
3. **NB-3/NB-2:** add the bindgen panic fixture, measure HNA boundary cost,
   and register `hydro-bindgen --check` in CI.
4. **NB-1/P7 stdlib:** continue Batches B/C/D/E/F and native modules, wire
   P7-05 per-module `Lib/test` subsets, then the dependency-ordered remaining
   batches.
5. **P5:** generator resume is now faster than CPython; continue with
   PGO/inlining and threshold tuning.
6. **P3:** uv interpreter injection, reproducible builds (P3-05), packaging
   (P3-08).
7. **P6:** troubleshooting/migration docs (P6-09), progress/exit summary
   (P6-10).
8. **Cross-cutting:** long fuzz campaigns (Q-03/Q-04), `unsafe` security
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

## Recent milestones (2026-11-10)

See `CHANGELOG.md` for details; this is only the resume pointer.

- Stdlib import matrix is **288/288 CPython-comparable modules**; the nine
  remaining failures also fail under CPython 3.14.9 on this host.
- Function-local native imports now shadow module aliases, `math` covers the
  CPython 3.14 scalar/iterable surface, and `opcode`/`code`/`tty` resolve to
  their pure CPython modules; `ast` exposes its compile flags through `_ast`.
- Workspace gate: **3262 tests run (3262 passed, 1 skipped)**; clippy and fmt
  are clean. The current report set is regenerated in
  `benchmarks/compat/latest.json` and `docs/COMPATIBILITY.md`.
