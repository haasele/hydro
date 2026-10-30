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

**2026-10-29 native-bindings checkpoint:**
Objective
- Continue the remaining TODO work from the previous session: ecosystem dynamic imports/smokes (NumPy, pandas, pydantic), stdlib blockers, native accelerator modules, then final verification/documentation.
Requirements
- A step counts as done only when the command/scenario was actually executed; update docs/TODO.md in the same change.
- No invented measurements or "done" claims; test counts live only in AGENTS.md.
- Do not change IR/ABI contracts without reporting deviations.
- Keep changes consistent with repository style and preserve uncommitted user work.
Decisions
- Fixed NumPy's map(lambda x, y: ..., saveout, outneeds[n]) compile failure via the variadic hydro_map_many runtime path.
- Added os.path.getsize/getmtime/getatime/getctime, time.asctime, and a frontend star-import shadowing rule to unblock NumPy.
- Treat preload-linked deferred imports as soft ordering edges (deferred_deps): their statements run only when the enclosing function executes, so they may be relaxed to break an initializer cycle. This fixed NumPy's _dtype_like/_array_like ordering.
- Base-less classes must resolve super() against object (e.g. super().__new__(cls)), so class_lookup_after now falls back to object when object is not already in the MRO.
- Preserve the narrow construction fallback: class_lookup still reports no __init__/__new__ for base-less classes; only super() resolution sees object.
Work State
Completed
- Variadic map codegen + runtime dispatch already existed and compiles.
- Added os.path.getsize, getmtime, getatime, getctime to OS_PATH spec/runtime.
- Added time.asctime with shared time_sequence_fields.
- Fixed crackfortran.py false-positive arity error via fn_definition_starts + star-import shadowing.
- Rebuilt Hydro release staticlib (cargo build --release -p hydro-runtime) and copied it to ../tea/target/release/libhydro_runtime.a; hydro_map_many is now present (AOT not yet rerun).
- NumPy ordering fix verified: unit_initializer_order now places numpy before numpy._typing._dtype_like and numpy._typing._array_like; import advances past AttributeError: module 'numpy' has no attribute 'dtype'.
- Fixed base-less super().__new__: minimal supernew.py now prints ok <A object>; NumPy import advances past 'super' object has no attribute '__new__'.
- pydantic:import and pydantic:smoke passed in the last full ecosystem report.
Active
- Fixing NumPy extension init: _multiarray_umath now fails at TypeError: ArrayMethod provided object <class 'numpy.dtypes.Float16DType'> is not a DType.(method: half_sort).
- Investigating PyType_Ready/adopt_static_type overwriting a heap dtype class's ob_type (PyArrayDTypeMeta_Type) with TYPE_TYPE, so PyObject_TypeCheck(Float16DType, PyArrayDTypeMeta_Type) fails.
Blocked
- NumPy import: ArrayMethod DType check during register_all_sorts/set_typeinfo in _multiarray_umath_exec.
- pandas import/smoke not yet retried; previously blocked on os.symlink/os.link.
- six import still blocked on importlib.util.spec_from_loader and types.ClassType.
- Full ecosystem report/docs/verification not yet rerun after this slice.
- Temporary debug instrumentation must be removed before finalizing.
Next Move
1. In crates/hydro-runtime/src/capi/type.rs, preserve a known non-NULL metatype in adopt_static_type instead of forcing ob_type = TYPE_TYPE; update is_type_pointer/as_type so types whose ob_type is a remembered custom metatype (e.g. numpy._DTypeMeta) are still recognized as type objects.
2. Consider implementing real PyType_FromMetaclass support for non-type metaclasses if NumPy relies on it; verify with gdb breakpoints.
3. Rebuild Tea release (cd ../tea && cargo build --release) and rerun python3 scripts/ecosystem_compat.py --only numpy --timeout 180; iterate on the next missing stdlib/ABI member.
4. Then run pandas and six, regenerate the full ecosystem report, and update docs/ECOSYSTEM.md, docs/TODO.md, AGENTS.md only with observed results.
5. Remove HYDRO_DEBUG_INIT_ORDER and HYDRO_DEBUG_SUPER instrumentation and debug markers in /tmp/opencode/site-debug before finalizing.
6. Add regression tests: variadic map, os.path.getsize/time, time.asctime, star-import/local-def shadowing, deferred-preload ordering, and base-less super().__new__.
7. Run cargo nextest run --workspace, cargo fmt --all -- --check, cargo clippy --workspace --all-targets -- -D warnings, ./scripts/verify.sh.
Relevant Files
- /mnt/Vault/GitHub/hydro/crates/hydro-frontend/src/lib.rs: deferred_deps field/recording/remapping, initializer_edge_is_soft, unit_initializer_order, temporary HYDRO_DEBUG_INIT_ORDER.
- /mnt/Vault/GitHub/hydro/crates/hydro-runtime/src/capi/type.rs: adopt_static_type, is_type_pointer, as_type, hydro_class_metatype, PyType_Ready.
- /mnt/Vault/GitHub/hydro/crates/hydro-runtime/src/capi/mod.rs: PyType_FromMetaclass, kind_type, PyObject_SetAttrString.
- /mnt/Vault/GitHub/hydro/crates/hydro-runtime/src/core/value.rs: class_lookup_after object fallback, class_lookup, object_protocol_slot.
- /mnt/Vault/GitHub/hydro/crates/hydro-runtime/src/core/methods.rs: super_getattr/super_method, temporary HYDRO_DEBUG_SUPER.
- /mnt/Vault/GitHub/hydro/crates/hydro-runtime/src/core/builtins.rs: updated object_protocol_slots_resolve_past_a_plain_class test.
- /mnt/Vault/GitHub/hydro/crates/hydro-runtime/src/core/container.rs: container/value helpers touched in this slice.
- /mnt/Vault/GitHub/hydro/crates/hydro-runtime/src/modules/mod.rs: os.path/time dispatch.
- /mnt/Vault/GitHub/hydro/crates/hydro-ir/src/stdlib/specs.rs: OS_PATH/TIME specs.
- /mnt/Vault/GitHub/hydro/crates/hydro-codegen/src/lower/mod.rs: map_many codegen arm.
- /mnt/Vault/GitHub/hydro/scripts/ecosystem_compat.py: ecosystem gate runner.
- /mnt/Vault/GitHub/hydro/docs/ECOSYSTEM.md: current ecosystem status.
- /mnt/Vault/GitHub/hydro/docs/TODO.md: NB-4/NB-1 work list.
- /mnt/Vault/GitHub/tea: Tea repo; release staticlib copy and rebuild.
- /tmp/opencode/site-debug/numpy/_core/overrides.py: debug copy with markers (not repo).
Important Context
- Build: export PATH="$HOME/.cargo/bin:$PATH"; cargo build --release -p hydro-runtime; then cp target/release/libhydro_runtime.a ../tea/target/release/libhydro_runtime.a; cd ../tea && cargo build --release.
- Probe env: PYTHONPATH=/mnt/Vault/GitHub/hydro/.hydro/ecosystem/site HYDRO_SITE_PACKAGES=/mnt/Vault/GitHub/hydro/.hydro/ecosystem/site HYDRO_STDLIB=/usr/lib/python3.14.
- Current NumPy failure: Traceback ... File "import_numpy.py", line 83, in numpy._core.overrides.<module> / TypeError: ArrayMethod provided object <class 'numpy.dtypes.Float16DType'> is not a DType.(method: half_sort).
- gdb shows failure in _multiarray_umath_exec -> set_typeinfo -> register_all_sorts -> make_sorts_<...> -> PyArrayMethod_FromSpec_int -> PyErr_Format.
- gdb shows dtype classes are readied via PyType_Ready (e.g. numpy.dtypes.Float16DType), with no PyType_FromSpec/PyType_FromMetaclass calls observed; adopt_static_type sets ob_type = TYPE_TYPE.
- is_type_pointer currently requires ob_type == TYPE_TYPE; preserving custom metatypes requires relaxing this consistently.
- hydro_class_metatype creates a fresh synthetic metatype deriving from type per Hydro class instead of returning the actual metaclass; may matter for Hydro classes derived from extension types.
- Separate pre-existing bug found: print(namedtuple_instance) causes a stack overflow (namedtuple3.py); creation and attribute access work.
- Working tree is large and uncommitted (~33 modified files, ~8k insertions) on top of 4f5bf23; do not discard.
- Test count canonical location remains AGENTS.md; update only with real runs.

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

**2026-10-30 ecosystem checkpoint:** the pinned CPython-3.14 wheels for NumPy
2.5.0, pandas 3.1.0, pydantic 2.14.1 and pydantic-core 2.50.1 install into
`.hydro/ecosystem/site`. The ABI inventory reports **0 missing required
CPython symbols** across 68 extension modules, and Tier 3 matches the CPython
handle, tuple and list item layout with real `PyGC_Collect` traverse/clear.
Pydantic passes `pydantic_core` import and the enum/uuid/object protocol
surface but its package import is blocked by native-scalar subclass
allocation for `IntEnum` (`AnnotationSource.ANY`) before model construction;
NumPy advances through module init to a DType/ArrayMethod type check; pandas
is not yet reached. These are semantic/object-model
blockers, not link-time symbol gaps. **All-core gate:** `cargo nextest run
--workspace` is the default in `scripts/verify.sh`/CI (see the canonical
counts below).

**2026-10-26 checkpoint:** the four open construction sites received a
substantial implementation slice: import/module registry + `HYDRO_STDLIB`,
the core builtin/frozen shims, PEP 701/750 parser and templates, the P4-02
language/runtime surface, the next full-ABI layer and a working wheel
installer. Full dynamic-import/introspection semantics, `except*` grouping
and the NumPy/pydantic ABI gate remain open; the official CPython baseline
was not regenerated after this slice.

### Test counts (canonical — update only here)

`cargo nextest run --workspace` on 2026-10-30: **2905 tests run
(2905 passed, 1 skipped)**; the per-crate breakdown below sums to 2905:

| Crate | Tests |
|---|---:|
| runtime (unit + integration) | 1860 |
| frontend | 819 |
| codegen | 76 |
| IR | 64 |
| bindgen | 42 |
| interpreter | 25 |
| parser | 19 |
| hna | 0 |
| hna-fixture | 0 |
| sqlite-hna | 0 |

Tea (separate repository) `cargo test` is green: **154 E2E tests** plus all
other suites, including HNA 7/7, bindgen 2/2, sidecar 6/6 and wheels 15/15.
Other docs must link here instead of repeating the total.

### Compatibility checkpoint (2026-10-30)

- **Import matrix:** **233/297 modules importable** (50 builtin/frozen, 133 pure,
  50 extension, 1 CPython-failing; 64 fail; 9 modules fail under CPython 3.14.9
  itself). Report: `docs/COMPATIBILITY.md`, JSON
  `benchmarks/compat/latest.json`.
- **Official suite:** **3/398 passing** (`test_errno.py`,
  `test_int_literal.py`, `test_longexp.py`), 333 `hydro_fail`, 62
  `expected_fail` (CPython also fails), 0 timeouts; full differential run took
  733.8 s. Manifest `tests/compat/cpython-3.14.9.json` grows only from real
  `hydro_pass` runs.
- **Ecosystem:** the ABI inventory reports **0 missing required symbols** across
  68 pinned extension modules. Pydantic-core 2.14.1/2.50.1 loads through the
  real extension, but importing `pydantic` is blocked by native-scalar
  subclass allocation needed for `IntEnum` (`AnnotationSource.ANY`). NumPy
  2.5 passes `sys.flags`, `PyType_Ready` base adoption, `PyContextVar_New`,
  `Py_BuildValue` and `PyErr_Format` and now stops on a DType/ArrayMethod type
  check; pandas is not reached. Pinned ecosystem report:
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
  ecosystem suites remain open; 233/297 stdlib modules import.
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
  remain open; pydantic imports, model smoke does not yet.
- HNA v1 limits: `init` is lazy until first module load; hold tokens root on
  the calling thread and must be released there (the token table is
  process-wide); the manifest is optional when absent;
  `thread_safe = false` serializes calls per module; leaf modules only.
  Bindgen supports a bounded C declaration subset (no variadics, function
  pointers, `long double`, struct-by-value or mutable output pointers); there
  is no general `exec`/`eval` (bounded source subset, no runtime Python
  compiler), and boundary cost is not yet measured. The sidecar protocol is
  v1 (16 MiB frames, depth 64); no JVM/.NET/Node adapter exists. Ecosystem
  blockers: pydantic model typed-dict naming/annotations, NumPy delayed
  dtype/module initialization, and pandas behind those gaps.
- Tea owns uv interoperability/injection and packaging; Hydro does not embed
  `uv` or a package manager.
- Linux x86_64 is the verified target; aarch64 is untested; macOS/Windows and
  cross-compilation are open.

## Next Steps

1. **NB-4 ecosystem:** close the typed-dict naming/annotation blocker in
   pydantic model smoke, NumPy delayed dtype/module init and the pandas
   import blockers, then thread/GIL and `ctypes` paths (P4-04c/d, P4-05,
   P4-06, P4-10/P4-11).
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
