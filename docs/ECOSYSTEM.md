# Hydro ecosystem compatibility

Differential gate for real third-party wheels. A program is not "compatible"
because an import succeeds: the same source must produce the same stdout under
the pinned CPython 3.14.9 oracle, under Hydro JIT (`tea infuse`) and under a
Hydro AOT binary (`tea build`), and all three processes must exit 0.

The gate runs the pinned NumPy 2.5.0, pandas 3.1.0, pydantic 2.14.1 and
pydantic-core 2.50.1 wheels from a site-packages tree (default `/tmp/pkgs`,
override with `--site` or `$HYDRO_ECOSYSTEM_SITE_PACKAGES`). Exact versions and
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

`HYDRO_STDLIB` should point at the CPython 3.14.9 `Lib` tree for unvendored
stdlib modules. As of 2026-10-28 the link-time ABI inventory is complete
(0 missing symbols), but imports still stop on semantic blockers: NumPy
`datetime_CAPI`, pandas' mixed-type conditional type check, and PyO3's
exception-class validation in pydantic-core. No case is reported as passing
until the differential runner actually observes it.

<!-- BEGIN GENERATED ECOSYSTEM RESULTS -->

_No ecosystem run recorded yet._ Run `python3 scripts/ecosystem_compat.py` to
generate this block. No import, smoke or upstream result is claimed before that
run; in particular, upstream suites have not been executed.

<!-- END GENERATED ECOSYSTEM RESULTS -->
