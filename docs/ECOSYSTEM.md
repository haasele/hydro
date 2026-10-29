# Hydro ecosystem compatibility

Differential gate for real third-party wheels. A program is not "compatible"
because an import succeeds: the same source must produce the same stdout under
the pinned CPython 3.14.9 oracle, under Hydro JIT (`tea infuse`) and under a
Hydro AOT binary (`tea build`), and all three processes must exit 0.

The gate runs the pinned NumPy 2.5.0, pandas 3.1.0, pydantic 2.14.1 and
pydantic-core 2.50.1 wheels from a site-packages tree (default
`.hydro/ecosystem/site` from `pins.json`; override with `--site` or
`$HYDRO_ECOSYSTEM_SITE_PACKAGES`). Exact versions and
dist-info RECORD digests are pinned in `benchmarks/ecosystem/pins.json`; a
version or RECORD mismatch fails the gate. Wheel digests are `null` until the
original artifacts are pinned and are never reported as verified.

```sh
python3 scripts/ecosystem_compat.py                 # imports + smoke, all runners
python3 scripts/ecosystem_compat.py --only numpy
python3 scripts/ecosystem_compat.py --suite smoke --no-aot
python3 scripts/ecosystem_compat.py --gate --json /tmp/ecosystem.json
python3 scripts/ecosystem_compat.py --only pydantic --tests .hydro/ecosystem/sources/pydantic-2.14.1
```

Cases live in `benchmarks/ecosystem/cases/` and print deterministic output (no
timestamps, host paths or unseeded randomness). Each case gets a fresh temporary
working directory, `PYTHONPATH`/`HYDRO_SITE_PACKAGES` point only at the pinned
site-packages tree, and every process is bounded by `--timeout`.

Status classes: `pass`, `cpython_fail` (the oracle itself failed, so the case is
incomparable), `compile_error` (Hydro compile diagnostic), `runtime_error`
(Hydro exited nonzero), `timeout`, `output_diff` (all runners exited 0 but
stdout differs), `aot_build_error` (the AOT build failed or produced no
executable; distinct from a runtime failure) and `pin_mismatch` (a package is
not installed at the pinned version/RECORD digest, so its cases are not run).
Results are written to `benchmarks/ecosystem/latest.json` and the generated
block below. A pass means the listed pinned programs agree across the three
runners; it is not a claim that the libraries are fully compatible, that
arbitrary programs work, or that an upstream test suite passed.

Without `--gate` the exit code is 0 when the report was produced, 1 when the
package pins do not match, and 2 when the harness could not run. `--gate`
additionally exits 1 on any failing case or upstream-test failure.

## Upstream suites

`--tests DIR` is an explicit upstream checkout hook: pytest runs through
`python -m pytest` under the CPython oracle and through a pytest wrapper under
Hydro. The selected test paths come from `upstream_tests` in
`benchmarks/ecosystem/pins.json`; pytest must be importable from the pinned
site-packages tree. Pass the package checkout as `DIR` and use
`--only <package>` when the pinned paths belong to one package. Missing pinned
paths, timeouts and nonzero exits are reported, nothing is skipped silently, and
a missing pytest installation is a failure. No upstream result is claimed until
such a run exists: the generated block only contains an upstream section after a
real `--tests` run.

## Current status

The pinned tree is installed under `.hydro/ecosystem/site` with:

```sh
uv pip install --target .hydro/ecosystem/site \
  numpy==2.5.0 pandas==3.1.0 pydantic==2.14.1 pydantic-core==2.50.1 \
  python-dateutil==3.0.0 six==1.17.0 typing-extensions==4.16.0 \
  typing-inspection==0.4.4 annotated-types==0.8.0 tzdata==2026.5
```

`HYDRO_STDLIB` defaults to the oracle's CPython 3.14.9 `Lib` tree (or
`--stdlib`/the environment). The checked-in report is the **2026-10-28 import
suite** (`benchmarks/ecosystem/latest.json`): it passes pydantic/pydantic-core
and six dependencies; NumPy stops on `numpy.exceptions`, pandas on
`dataclasses.is_dataclass`, six on `importlib.util.spec_from_loader`, and
annotated-types on a native-method attribute. The link-time ABI inventory is
complete (0 missing symbols); these are semantic/dynamic-import blockers.
Smoke/AOT/upstream runs are not claimed until the runner observes them.

**Status note (2026-10-29):** the report below was *not* regenerated for the
HNA v1/bindgen/sidecar/packaging work or for the CPython-compatible
handle/tuple/list layout. The generated table therefore remains the honest
last observed evidence, not the current state of every case. Current known
blockers beyond that table: pydantic 2.14.1 imports through the vendored
`enum`/typing surface, but model smoke still stops on lazy `pydantic.main`
dynamic imports (unlinked package submodules); NumPy 2.5 now reaches
`_multiarray_umath`'s `Py_BuildValue` after `sys.flags`/`PyType_Ready`/
`PyContextVar_New` fixes; pandas remains blocked behind earlier gaps. No "all
ecosystem" claim is made; there is no passing upstream-suite record yet.

<!-- BEGIN GENERATED ECOSYSTEM RESULTS -->

Environment: CPython 3.14.9 (`/home/aura/.local/share/uv/python/cpython-3.14.9-linux-x86_64-gnu/bin/python3.14`) · Hydro `tea 0.1.0` · binary `/mnt/Vault/GitHub/tea/target/release/tea` · site `/mnt/Vault/GitHub/hydro/.hydro/ecosystem/site`

## Package pins

Versions and dist-info RECORD digests are verified before any case runs. The
NumPy RECORD digest was refreshed on 2026-10-29 for the current
`.hydro/ecosystem/site` tree; wheel digests remain unpinned, so this verifies
tree integrity, not provenance.

| Package | Expected | Actual | RECORD SHA-256 | Wheel SHA-256 | Status |
| --- | --- | --- | --- | --- | --- |
| `numpy` | 2.5.0 | 2.5.0 | `d47a1166cd35d7d8b4ffcbcae0aa3a8890667fcdc76cfa383410db1de3b2e854` | not pinned | ok |
| `pandas` | 3.1.0 | 3.1.0 | `cc43eef24c2d35b044fe9831c50c574f87572638cb4e8189e8705084da57efdc` | not pinned | ok |
| `pydantic` | 2.14.1 | 2.14.1 | `d16db24e7db99e1383a3ad68457d9b3f2a3badaa891a60f2730f06c6f8064277` | not pinned | ok |
| `pydantic-core` | 2.50.1 | 2.50.1 | `34bfca22c843f6195ed3a622c9349998abcbc623ddb5662ce31876761ac5bb53` | not pinned | ok |
| `python-dateutil` | 3.0.0 | 3.0.0 | `a17d86766c46a890c3d8b1c4ccfa5bb1c6014919f3a3a9da4b8c597f47bf5b33` | not pinned | ok |
| `six` | 1.17.0 | 1.17.0 | `d16e50f347969b1e1919fb0c88bac4b9c644f5305bcfcfb00f72b646dbd4065d` | not pinned | ok |
| `typing-extensions` | 4.16.0 | 4.16.0 | `9e07fea845cac76293d927afa7157a14ecba39b318bc8af236b8de530b7656c1` | not pinned | ok |
| `typing-inspection` | 0.4.4 | 0.4.4 | `83fc23669db31500d06056b13ecebe4f088f56a81c1ea86e3b68f1de3b8c7d95` | not pinned | ok |
| `annotated-types` | 0.8.0 | 0.8.0 | `8191d3aa1ac93480688819b4db2385606219a3f7a453b85e086646134ded4f96` | not pinned | ok |
| `tzdata` | 2026.5 | 2026.5 | `bc03ea1afd3394eb010013e6112f91d884785445c8453b7d56ddb7ba0cc4939d` | not pinned | ok |

## Summary

| Status | Cases |
| --- | ---: |
| compile_error | 2 |
| pass | 6 |
| runtime_error | 2 |
| **Total** | 10 |

A case passes only when the oracle, Hydro JIT and (unless `--no-aot`) the AOT binary all exit 0 with byte-identical stdout.

## Cases

| Case | CPython | Hydro JIT | Hydro AOT | Status | Detail |
| --- | --- | --- | --- | --- | --- |
| `numpy:import` | ok | exit 1 | - | runtime_error | jit: ImportError: No module named 'numpy.exceptions' |
| `pandas:import` | ok | exit 1 | - | compile_error | jit: error[HYD0201]: module 'dataclasses' has no attribute 'is_dataclass' |
| `pydantic:import` | ok | ok | - | pass | - |
| `pydantic-core:import` | ok | ok | - | pass | - |
| `python-dateutil:import` | ok | ok | - | pass | - |
| `six:import` | ok | exit 1 | - | compile_error | jit: error[HYD0201]: module 'importlib.util' has no attribute 'spec_from_loader' |
| `typing-extensions:import` | ok | ok | - | pass | - |
| `typing-inspection:import` | ok | ok | - | pass | - |
| `annotated-types:import` | ok | exit 1 | - | runtime_error | jit: AttributeError: 'function' object has no attribute 'islower' |
| `tzdata:import` | ok | ok | - | pass | - |

<!-- END GENERATED ECOSYSTEM RESULTS -->
