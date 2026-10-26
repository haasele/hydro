# CPython test suite gate

Hydro targets **CPython 3.14.9**. Behavioural compatibility is measured against
the unmodified CPython standard-library test suite (`Lib/test/test_*.py`) from a
CPython 3.14.9 source tree. Every selected test file is executed twice:

1. by the pinned CPython 3.14.9 interpreter (the differential oracle), with
   `cwd=<source>/Lib` and `PYTHONPATH=<source>/Lib`;
2. by Hydro (`tea infuse --color=never <testfile>`), from the repository root.

A test counts as passing **only when both interpreters exit 0**. If Hydro exits
non-zero while CPython passes, the test is a Hydro failure; if both fail, the
result is `expected_fail`; if CPython fails while Hydro passes, the row is
`cpython_fail` and cannot be used as evidence of compatibility. A Hydro timeout is
never a pass. Nothing in the report is hand-written: `--update-manifest` copies
the tests that actually passed in that run.

## The 3.14.9 pin

- 3.14.9 is the main compatibility target. 3.15 is too new to use as the
  conformance oracle, and 3.13 is already in security-only maintenance.
- The harness verifies the oracle's `--version` starts with `3.14.9` and that a
  `--source` tree declares 3.14.9 in `Include/patchlevel.h`. A different
  interpreter or tree is rejected rather than silently used.
- Future LTS lines are keyed to CPython minor versions (3.13, 3.14, 3.15).
  Changing the target means adding a new manifest (`tests/compat/cpython-X.Y.Z.json`)
  and a new baseline, not editing 3.14.9 in place.

The oracle is resolved from `--cpython PATH`, else `$HYDRO_CPYTHON`, else
`uv python find 3.14.9`. When none is available the harness prints the remedy:

```sh
uv python install 3.14.9
```

The source tree comes from `--source PATH`, else the cache at
`.hydro/cpython/Python-3.14.9`. On first use the harness downloads
`https://www.python.org/ftp/python/3.14.9/Python-3.14.9.tgz`, prints its
SHA-256, and reuses the extraction on later runs. The cache is ignored by Git.
`--gate` and `--check-manifest` never download; run the harness once (or pass
`--source`) before relying on the gate in an offline environment.

## Commands

```sh
# Offline manifest validation only: no download, no CPython/Hydro runs.
python3 scripts/cpython_compat.py --check-manifest

# Release gate: run the full suite and enforce the committed manifest.
python3 scripts/cpython_compat.py --gate

# Record the current passing set (writes tests/compat/cpython-3.14.9.json).
python3 scripts/cpython_compat.py --update-manifest

# Full report: benchmarks/compat/cpython-3.14.9.json + this document.
python3 scripts/cpython_compat.py

# Debug/smoke variants (never valid for gating).
python3 scripts/cpython_compat.py --only test_bisect,test_json --timeout 10
python3 scripts/cpython_compat.py --limit 5 --source /path/to/Python-3.14.9
```

`--gate` and `--update-manifest` require a full run; `--only`/`--limit` are
rejected in those modes so a partial run can never satisfy the gate. The
generated results below are refreshed by every report run; the policy text is
preserved.

## Manifest v1

`tests/compat/cpython-3.14.9.json` is the release gate:

```json
{
  "python": "3.14.9",
  "minimum_passing": 1,
  "required": [],
  "known_passing": [
    "test_errno.py"
  ],
  "notes": "Release gate; grow as stdlib/language support lands. Import-only baseline is separate in docs/COMPATIBILITY.md."
}
```

- `python` must be exactly `3.14.9`.
- `known_passing` is the set of tests that passed in the recorded run.
- `required` is the subset that must pass for a release; it must also appear in
  `known_passing`.
- `minimum_passing` is the floor for the total pass count and may never exceed
  `len(known_passing)` or the current pass count.
- `--update-manifest` is monotonic: it refuses to drop a `required` test that
  stopped passing and never lowers `minimum_passing`. Regressions are fixed or
  removed from the gate deliberately, never silently.

The gate fails when the manifest's `python` differs from 3.14.9, when any
`required` test is absent from the run or does not pass, or when the current
pass count is below `minimum_passing`. As tests are added and the manifest
grows, this blocks release and catches regressions.

## The first passing test and `required`

The manifest started empty and now records the first genuine pass,
`test_errno.py`: Hydro has a native `errno` module plus enough `unittest`
(`TestCase`, native assertions, runner), module values/`getattr`, module
globals and `frozenset` for the unmodified official file to exit 0 under both
interpreters. `required` remains empty until the project pins a
never-regress set; `minimum_passing` is 1. The separate import-only baseline
in [`COMPATIBILITY.md`](COMPATIBILITY.md) tracks module imports and makes no
test-suite claim.

Once another test genuinely passes under both interpreters, record it:

```sh
python3 scripts/cpython_compat.py --update-manifest
```

Then add the tests that must never regress to `required` and raise
`minimum_passing` as appropriate. From that point the gate is a release
blocker.

## Generated results

<!-- BEGIN GENERATED RESULTS -->

Environment: CPython `3.14.9` (`/home/aura/.local/share/uv/python/cpython-3.14.9-linux-x86_64-gnu/bin/python3.14`) · Hydro `tea 0.1.0` (`/mnt/Vault/GitHub/tea/target/release/tea`)

Source: `/mnt/Vault/GitHub/tea/.hydro/cpython/Python-3.14.9` · generated 2026-10-24T22:16:44Z

## Summary

| Status | Tests |
| --- | ---: |
| hydro_pass | 1 |
| hydro_fail | 334 |
| cpython_fail | 0 |
| expected_fail | 63 |
| timeout | 0 |
| **Total** | 398 |

Passing: **1 / 398** (a pass requires Hydro and CPython 3.14.9 to both exit 0).

## Passing tests

- `test_errno.py`

## Failure classes

| Status | Tests |
| --- | ---: |
| hydro_fail | 334 |
| expected_fail | 63 |

## Test results

| Test | CPython 3.14.9 | Hydro | Status | First error |
| --- | --- | --- | --- | --- |
| `test___all__.py` | ok / 1902 ms | exit 1 / 2.1 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test__colorize.py` | ok / 117.7 ms | exit 1 / 2.5 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test__interpchannels.py` | ok / 794.9 ms | exit 1 / 7.1 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test__interpreters.py` | ok / 656.2 ms | exit 1 / 4.8 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test__locale.py` | ok / 56.6 ms | exit 1 / 3.5 ms | hydro_fail | error[HYD0002]: module '_locale' not found: not supported unless a sibling _locale.py exists |
| `test__opcode.py` | ok / 53.6 ms | exit 1 / 2.9 ms | hydro_fail | error[HYD0002]: module 'dis' not found: not supported unless a sibling dis.py exists |
| `test__osx_support.py` | ok / 55.8 ms | exit 1 / 3.2 ms | hydro_fail | error[HYD0002]: module 'stat' not found: not supported unless a sibling stat.py exists |
| `test_abc.py` | ok / 50.5 ms | exit 1 / 3.5 ms | hydro_fail | error[HYD0002]: module 'abc' not found: not supported unless a sibling abc.py exists |
| `test_abstract_numbers.py` | ok / 45.7 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: module 'abc' not found: not supported unless a sibling abc.py exists |
| `test_android.py` | exit 1 / 126 ms | exit 1 / 3.4 ms | expected_fail | error[HYD0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_annotationlib.py` | ok / 77.3 ms | exit 1 / 2.8 ms | hydro_fail | error[HYD0001]: invalid syntax: invalid syntax. Got unexpected token "{a}" |
| `test_apple.py` | exit 1 / 119.4 ms | exit 1 / 2.5 ms | expected_fail | error[HYD0002]: module '_apple_support' not found: not supported unless a sibling _apple_support.py exists |
| `test_argparse.py` | ok / 1392.3 ms | exit 1 / 26.5 ms | hydro_fail | error[HYD0002]: module '_colorize' not found: not supported unless a sibling _colorize.py exists |
| `test_array.py` | ok / 300.1 ms | exit 1 / 8.6 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'collections.abc' |
| `test_asdl_parser.py` | exit 1 / 33.8 ms | exit 1 / 2.2 ms | expected_fail | error[HYD0002]: dotted imports are not supported for user modules: 'importlib.machinery' |
| `test_asyncgen.py` | ok / 843.4 ms | exit 1 / 6.1 ms | hydro_fail | error[HYD0002]: module 'inspect' not found: not supported unless a sibling inspect.py exists |
| `test_atexit.py` | exit 1 / 114.2 ms | exit 1 / 2.4 ms | expected_fail | error[HYD0002]: module 'atexit' not found: not supported unless a sibling atexit.py exists |
| `test_audit.py` | ok / 1663.8 ms | exit 1 / 3.1 ms | hydro_fail | error[HYD0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_augassign.py` | ok / 45.6 ms | exit 1 / 3.7 ms | hydro_fail | error[HYD0002]: bitwise operators are not supported in Phase 1 |
| `test_base64.py` | ok / 151.5 ms | exit 1 / 5.4 ms | hydro_fail | error[HYD0002]: module 'base64' not found: not supported unless a sibling base64.py exists |
| `test_baseexception.py` | ok / 52 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: module 'builtins' not found: not supported unless a sibling builtins.py exists |
| `test_bdb.py` | exit 1 / 72.6 ms | exit 1 / 6.1 ms | expected_fail | error[HYD0002]: module 'bdb' not found: not supported unless a sibling bdb.py exists |
| `test_bigaddrspace.py` | ok / 44.1 ms | exit 1 / 1.8 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_bigmem.py` | ok / 62.6 ms | exit 1 / 6.4 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_binascii.py` | ok / 52.5 ms | exit 1 / 3.7 ms | hydro_fail | error[HYD0002]: module 'binascii' not found: not supported unless a sibling binascii.py exists |
| `test_binop.py` | ok / 41.9 ms | exit 1 / 2.9 ms | hydro_fail | error[HYD0002]: module 'abc' not found: not supported unless a sibling abc.py exists |
| `test_bisect.py` | ok / 56.5 ms | exit 1 / 3.6 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_bool.py` | ok / 53.3 ms | exit 1 / 3.1 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_buffer.py` | ok / 86.8 ms | exit 1 / 21.3 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_bufio.py` | ok / 150.3 ms | exit 1 / 2.1 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_build_details.py` | exit 1 / 119.5 ms | exit 1 / 2.9 ms | expected_fail | error[HYD0002]: module 'importlib' not found: not supported unless a sibling importlib.py exists |
| `test_builtin.py` | ok / 377.4 ms | exit 1 / 15 ms | hydro_fail | error[HYD0002]: module 'ast' not found: not supported unless a sibling ast.py exists |
| `test_bytes.py` | exit 1 / 98.8 ms | exit 1 / 15.3 ms | expected_fail | error[HYD0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_bz2.py` | ok / 382.8 ms | exit 1 / 6 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_c_locale_coercion.py` | ok / 839.2 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: module 'locale' not found: not supported unless a sibling locale.py exists |
| `test_calendar.py` | ok / 1417.7 ms | exit 1 / 6.6 ms | hydro_fail | error[HYD0002]: module 'calendar' not found: not supported unless a sibling calendar.py exists |
| `test_call.py` | exit 1 / 60.3 ms | exit 1 / 5.1 ms | expected_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_charmapcodec.py` | ok / 43.7 ms | exit 1 / 2.3 ms | hydro_fail | error[HYD0002]: module 'codecs' not found: not supported unless a sibling codecs.py exists |
| `test_class.py` | ok / 61.5 ms | exit 1 / 5.3 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_clinic.py` | ok / 235 ms | exit 1 / 14.8 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_cmath.py` | ok / 107.6 ms | exit 1 / 4.5 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_cmd.py` | ok / 211.5 ms | exit 1 / 2.3 ms | hydro_fail | error[HYD0002]: module 'cmd' not found: not supported unless a sibling cmd.py exists |
| `test_cmd_line.py` | exit 1 / 1797.7 ms | exit 1 / 7 ms | expected_fail | error[HYD0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_cmd_line_script.py` | exit 1 / 1445.4 ms | exit 1 / 5.2 ms | expected_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_code.py` | ok / 211 ms | exit 1 / 5.8 ms | hydro_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_code_module.py` | ok / 135.1 ms | exit 1 / 3.1 ms | hydro_fail | error[HYD0002]: module 'traceback' not found: not supported unless a sibling traceback.py exists |
| `test_codeccallbacks.py` | ok / 62.3 ms | exit 1 / 5.7 ms | hydro_fail | error[HYD0002]: module '_codecs' not found: not supported unless a sibling _codecs.py exists |
| `test_codecencodings_cn.py` | ok / 135.9 ms | exit 1 / 2.4 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecencodings_hk.py` | ok / 111.9 ms | exit 1 / 2.8 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecencodings_iso2022.py` | ok / 163.6 ms | exit 1 / 2.5 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecencodings_jp.py` | ok / 186.1 ms | exit 1 / 2.4 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecencodings_kr.py` | ok / 126.7 ms | exit 1 / 2.1 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecencodings_tw.py` | ok / 108.1 ms | exit 1 / 1.8 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecmaps_cn.py` | ok / 180.5 ms | exit 1 / 1.7 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecmaps_hk.py` | ok / 115.3 ms | exit 1 / 1.7 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecmaps_jp.py` | ok / 184.2 ms | exit 1 / 1.9 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecmaps_kr.py` | ok / 180.6 ms | exit 1 / 1.9 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecmaps_tw.py` | ok / 148.1 ms | exit 1 / 1.4 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecs.py` | ok / 336.9 ms | exit 1 / 16.3 ms | hydro_fail | error[HYD0002]: module 'codecs' not found: not supported unless a sibling codecs.py exists |
| `test_codeop.py` | ok / 55.4 ms | exit 1 / 2.9 ms | hydro_fail | error[HYD0002]: module 'warnings' not found: not supported unless a sibling warnings.py exists |
| `test_collections.py` | ok / 1196.5 ms | exit 1 / 11.5 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_colorsys.py` | ok / 44.7 ms | exit 1 / 2.1 ms | hydro_fail | error[HYD0002]: module 'colorsys' not found: not supported unless a sibling colorsys.py exists |
| `test_compare.py` | ok / 56.8 ms | exit 1 / 3.9 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_compile.py` | ok / 2200 ms | exit 1 / 11.6 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_compileall.py` | exit 1 / 66.1 ms | exit 1 / 6.2 ms | expected_fail | error[HYD0002]: module 'compileall' not found: not supported unless a sibling compileall.py exists |
| `test_compiler_assemble.py` | ok / 37.8 ms | exit 1 / 2.8 ms | hydro_fail | error[HYD0002]: module 'dis' not found: not supported unless a sibling dis.py exists |
| `test_compiler_codegen.py` | ok / 37.9 ms | exit 1 / 2.6 ms | hydro_fail | error[HYD0002]: module 'textwrap' not found: not supported unless a sibling textwrap.py exists |
| `test_complex.py` | ok / 66.4 ms | exit 1 / 6.9 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_configparser.py` | ok / 413.7 ms | exit 1 / 9.6 ms | hydro_fail | error[HYD0002]: module 'configparser' not found: not supported unless a sibling configparser.py exists |
| `test_contains.py` | ok / 52.4 ms | exit 1 / 1.9 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_context.py` | ok / 510.9 ms | exit 1 / 5.8 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'collections.abc' |
| `test_contextlib.py` | ok / 71.2 ms | exit 1 / 6 ms | hydro_fail | error[HYD0002]: module 'tempfile' not found: not supported unless a sibling tempfile.py exists |
| `test_contextlib_async.py` | ok / 72.6 ms | exit 1 / 3.4 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_copy.py` | ok / 75.2 ms | exit 1 / 5.1 ms | hydro_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_copyreg.py` | ok / 85.4 ms | exit 1 / 2.1 ms | hydro_fail | error[HYD0002]: module 'copyreg' not found: not supported unless a sibling copyreg.py exists |
| `test_coroutines.py` | ok / 278.5 ms | exit 1 / 7.8 ms | hydro_fail | error[HYD0002]: module 'ast' not found: not supported unless a sibling ast.py exists |
| `test_cprofile.py` | ok / 542.1 ms | exit 1 / 2.8 ms | hydro_fail | error[HYD0002]: module 'cProfile' not found: not supported unless a sibling cProfile.py exists |
| `test_crossinterp.py` | exit 1 / 59.1 ms | exit 1 / 6.7 ms | expected_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_csv.py` | ok / 248.8 ms | exit 1 / 9.3 ms | hydro_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_curses.py` | ok / 305.9 ms | exit 1 / 10.3 ms | hydro_fail | error[HYD0002]: module 'inspect' not found: not supported unless a sibling inspect.py exists |
| `test_datetime.py` | ok / 19598.5 ms | exit 1 / 1.8 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support.import_helper' |
| `test_dbm.py` | ok / 414.8 ms | exit 1 / 3.9 ms | hydro_fail | error[HYD0002]: module 'dbm' not found: not supported unless a sibling dbm.py exists |
| `test_dbm_dumb.py` | ok / 75.7 ms | exit 1 / 3.2 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_dbm_gnu.py` | exit 1 / 51.1 ms | exit 1 / 2.4 ms | expected_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_dbm_ndbm.py` | ok / 111.4 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_dbm_sqlite3.py` | ok / 583.7 ms | exit 1 / 3.1 ms | hydro_fail | error[HYD0002]: module 'stat' not found: not supported unless a sibling stat.py exists |
| `test_decimal.py` | ok / 1911.3 ms | exit 1 / 24.4 ms | hydro_fail | error[HYD0002]: module 'logging' not found: not supported unless a sibling logging.py exists |
| `test_decorators.py` | ok / 48.5 ms | exit 1 / 6.1 ms | hydro_fail | error[HYD0002]: **kwargs parameters are not supported in Phase 1 |
| `test_defaultdict.py` | ok / 49.6 ms | exit 1 / 2.2 ms | hydro_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_deque.py` | ok / 378.1 ms | exit 1 / 5.8 ms | hydro_fail | error[HYD0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_descr.py` | exit 1 / 310.6 ms | exit 1 / 21.9 ms | expected_fail | error[HYD0002]: module 'builtins' not found: not supported unless a sibling builtins.py exists |
| `test_descrtut.py` | ok / 137.3 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_devpoll.py` | exit 1 / 44.9 ms | exit 1 / 2.3 ms | expected_fail | error[HYD0002]: module 'select' not found: not supported unless a sibling select.py exists |
| `test_dict.py` | ok / 321.3 ms | exit 1 / 8 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'collections.abc' |
| `test_dictcomps.py` | ok / 47.7 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: module 'traceback' not found: not supported unless a sibling traceback.py exists |
| `test_dictviews.py` | ok / 133.6 ms | exit 1 / 4.1 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'collections.abc' |
| `test_difflib.py` | ok / 239.9 ms | exit 1 / 4.3 ms | hydro_fail | error[HYD0002]: module 'difflib' not found: not supported unless a sibling difflib.py exists |
| `test_dis.py` | ok / 216.6 ms | exit 1 / 11 ms | hydro_fail | error[HYD0002]: module 'ast' not found: not supported unless a sibling ast.py exists |
| `test_docxmlrpc.py` | ok / 4196.7 ms | exit 1 / 2.5 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'xmlrpc.server' |
| `test_dtrace.py` | exit 1 / 52.8 ms | exit 1 / 2.9 ms | expected_fail | error[HYD0002]: module 'dis' not found: not supported unless a sibling dis.py exists |
| `test_dynamic.py` | ok / 48.1 ms | exit 1 / 2.5 ms | hydro_fail | error[HYD0002]: module 'builtins' not found: not supported unless a sibling builtins.py exists |
| `test_dynamicclassattribute.py` | ok / 44.8 ms | exit 1 / 3 ms | hydro_fail | error[HYD0002]: module 'abc' not found: not supported unless a sibling abc.py exists |
| `test_eintr.py` | exit 1 / 102 ms | exit 1 / 2 ms | expected_fail | error[HYD0002]: module 'signal' not found: not supported unless a sibling signal.py exists |
| `test_embed.py` | ok / 71.5 ms | exit 1 / 8.1 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_ensurepip.py` | ok / 161.3 ms | exit 1 / 2.5 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_enum.py` | exit 1 / 579.9 ms | exit 1 / 22.9 ms | expected_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_enumerate.py` | ok / 87.5 ms | exit 1 / 2.8 ms | hydro_fail | error[HYD0002]: module 'gc' not found: not supported unless a sibling gc.py exists |
| `test_eof.py` | ok / 294 ms | exit 1 / 2.3 ms | hydro_fail | error[HYD0002]: module 'codecs' not found: not supported unless a sibling codecs.py exists |
| `test_epoll.py` | ok / 153.7 ms | exit 1 / 2.4 ms | hydro_fail | error[HYD0002]: module 'select' not found: not supported unless a sibling select.py exists |
| `test_errno.py` | ok / 44 ms | ok / 12.1 ms | hydro_pass | - |
| `test_except_star.py` | ok / 55.6 ms | exit 1 / 5.2 ms | hydro_fail | error[HYD0002]: module 'textwrap' not found: not supported unless a sibling textwrap.py exists |
| `test_exception_group.py` | ok / 270.5 ms | exit 1 / 5.7 ms | hydro_fail | error[HYD0002]: module 'types' not found: not supported unless a sibling types.py exists |
| `test_exception_hierarchy.py` | ok / 61.4 ms | exit 1 / 3.1 ms | hydro_fail | error[HYD0002]: module 'builtins' not found: not supported unless a sibling builtins.py exists |
| `test_exception_variations.py` | ok / 49.3 ms | exit 1 / 6.1 ms | hydro_fail | error[HYD0002]: try/except* is not supported in Phase 1 |
| `test_exceptions.py` | exit 1 / 959.1 ms | exit 1 / 9.1 ms | expected_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_extcall.py` | exit 1 / 150.3 ms | exit 1 / 1.7 ms | expected_fail | error[HYD0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_external_inspection.py` | ok / 1436.7 ms | exit 1 / 6.3 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_faulthandler.py` | ok / 18665.4 ms | exit 1 / 4.2 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_fcntl.py` | ok / 208 ms | exit 1 / 2.4 ms | hydro_fail | error[HYD0002]: module 'struct' not found: not supported unless a sibling struct.py exists |
| `test_file.py` | ok / 82.3 ms | exit 1 / 3 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_file_eintr.py` | ok / 1438.3 ms | exit 1 / 2.6 ms | hydro_fail | error[HYD0002]: module 'select' not found: not supported unless a sibling select.py exists |
| `test_filecmp.py` | ok / 66.2 ms | exit 1 / 4.4 ms | hydro_fail | error[HYD0002]: module 'filecmp' not found: not supported unless a sibling filecmp.py exists |
| `test_fileinput.py` | ok / 125.9 ms | exit 1 / 5 ms | hydro_fail | error[HYD0002]: module 'fileinput' not found: not supported unless a sibling fileinput.py exists |
| `test_fileio.py` | ok / 530.9 ms | exit 1 / 4.7 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_fileutils.py` | ok / 132.9 ms | exit 1 / 1.5 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_finalization.py` | ok / 100 ms | exit 1 / 3.1 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_float.py` | ok / 87.2 ms | exit 1 / 9.5 ms | hydro_fail | error[HYD0002]: module 'fractions' not found: not supported unless a sibling fractions.py exists |
| `test_flufl.py` | ok / 43 ms | exit 1 / 1.6 ms | hydro_fail | error[HYD0002]: module '__future__' not found: not supported unless a sibling __future__.py exists |
| `test_fnmatch.py` | ok / 46.2 ms | exit 1 / 3.5 ms | hydro_fail | error[HYD0002]: module 'string' not found: not supported unless a sibling string.py exists |
| `test_fork1.py` | ok / 5133 ms | exit 1 / 2.8 ms | hydro_fail | error[HYD0002]: module '_imp' not found: not supported unless a sibling _imp.py exists |
| `test_format.py` | ok / 56 ms | exit 1 / 4.2 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_fractions.py` | ok / 71.7 ms | exit 1 / 10.3 ms | hydro_fail | error[HYD0002]: module 'decimal' not found: not supported unless a sibling decimal.py exists |
| `test_frame.py` | exit 1 / 941.6 ms | exit 1 / 4.2 ms | expected_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_frozen.py` | ok / 53.1 ms | exit 1 / 2.2 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'importlib.machinery' |
| `test_fstring.py` | exit 1 / 403.5 ms | exit 1 / 5.4 ms | expected_fail | error[HYD0001]: invalid syntax: f-string: expecting '}' |
| `test_ftplib.py` | ok / 7553.1 ms | exit 1 / 5.8 ms | hydro_fail | error[HYD0002]: module 'ftplib' not found: not supported unless a sibling ftplib.py exists |
| `test_funcattrs.py` | ok / 54.1 ms | exit 1 / 3.8 ms | hydro_fail | error[HYD0002]: module 'textwrap' not found: not supported unless a sibling textwrap.py exists |
| `test_functools.py` | ok / 365.8 ms | exit 1 / 17.4 ms | hydro_fail | error[HYD0002]: module 'abc' not found: not supported unless a sibling abc.py exists |
| `test_gc.py` | exit 1 / 1550.3 ms | exit 1 / 5.5 ms | expected_fail | error[HYD0002]: dotted imports are not supported for user modules: 'unittest.mock' |
| `test_generated_cases.py` | ok / 127.4 ms | exit 1 / 4.1 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_generator_stop.py` | ok / 44.8 ms | exit 1 / 1.9 ms | hydro_fail | error[HYD0002]: module '__future__' not found: not supported unless a sibling __future__.py exists |
| `test_generators.py` | ok / 192.8 ms | exit 1 / 6.2 ms | hydro_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_genericalias.py` | ok / 175.6 ms | exit 1 / 5.2 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_genericclass.py` | ok / 57.5 ms | exit 1 / 3 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_genericpath.py` | ok / 56.8 ms | exit 1 / 4.6 ms | hydro_fail | error[HYD0002]: module 'genericpath' not found: not supported unless a sibling genericpath.py exists |
| `test_genexps.py` | ok / 138.3 ms | exit 1 / 2.2 ms | hydro_fail | error[HYD0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_getopt.py` | ok / 171.1 ms | exit 1 / 3.8 ms | hydro_fail | error[HYD0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_getpass.py` | ok / 120.6 ms | exit 1 / 2.6 ms | hydro_fail | error[HYD0002]: module 'getpass' not found: not supported unless a sibling getpass.py exists |
| `test_getpath.py` | ok / 42.3 ms | exit 1 / 5.2 ms | hydro_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_gettext.py` | exit 1 / 171.3 ms | exit 1 / 6 ms | expected_fail | error[HYD0002]: module 'base64' not found: not supported unless a sibling base64.py exists |
| `test_glob.py` | ok / 109.1 ms | exit 1 / 6.1 ms | hydro_fail | error[HYD0002]: module 'glob' not found: not supported unless a sibling glob.py exists |
| `test_global.py` | ok / 53.2 ms | exit 1 / 2.1 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_grammar.py` | ok / 68.9 ms | exit 1 / 5.1 ms | hydro_fail | error[HYD0001]: invalid syntax: invalid syntax. Got unexpected token ',' |
| `test_graphlib.py` | ok / 88 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: module 'graphlib' not found: not supported unless a sibling graphlib.py exists |
| `test_grp.py` | ok / 264.1 ms | exit 1 / 2.4 ms | hydro_fail | error[HYD0002]: module 'string' not found: not supported unless a sibling string.py exists |
| `test_gzip.py` | ok / 869.7 ms | exit 1 / 5.9 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_hash.py` | ok / 318.1 ms | exit 1 / 2.6 ms | hydro_fail | error[HYD0002]: module 'datetime' not found: not supported unless a sibling datetime.py exists |
| `test_hashlib.py` | ok / 18764.3 ms | exit 1 / 7.3 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_heapq.py` | ok / 248.3 ms | exit 1 / 3.8 ms | hydro_fail | error[HYD0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_hmac.py` | ok / 177.5 ms | exit 1 / 6.1 ms | hydro_fail | error[HYD0002]: module 'binascii' not found: not supported unless a sibling binascii.py exists |
| `test_html.py` | ok / 49 ms | exit 1 / 2.1 ms | hydro_fail | error[HYD0002]: module 'html' not found: not supported unless a sibling html.py exists |
| `test_htmlparser.py` | ok / 456.7 ms | exit 1 / 6.8 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'html.parser' |
| `test_http_cookiejar.py` | ok / 278.2 ms | exit 1 / 8 ms | hydro_fail | error[HYD0002]: module 'stat' not found: not supported unless a sibling stat.py exists |
| `test_http_cookies.py` | ok / 557.7 ms | exit 1 / 4.6 ms | hydro_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_httplib.py` | ok / 3656.8 ms | exit 1 / 11.1 ms | hydro_fail | error[HYD0002]: module 'enum' not found: not supported unless a sibling enum.py exists |
| `test_httpservers.py` | ok / 4152.2 ms | exit 1 / 7.6 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'http.server' |
| `test_idle.py` | ok / 9949.2 ms | exit 1 / 1.9 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support.import_helper' |
| `test_imaplib.py` | timeout / 30005 ms | exit 1 / 10.3 ms | expected_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_index.py` | ok / 52.5 ms | exit 1 / 2.4 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_int.py` | ok / 22107 ms | exit 1 / 5.3 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_int_literal.py` | ok / 41 ms | exit 1 / 3 ms | hydro_fail | error[HYD0002]: integer literal is out of the i64 range |
| `test_io.py` | exit 1 / 28258.1 ms | exit 1 / 20 ms | expected_fail | error[HYD0002]: module 'abc' not found: not supported unless a sibling abc.py exists |
| `test_ioctl.py` | ok / 553.4 ms | exit 1 / 2.6 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_ipaddress.py` | ok / 89.7 ms | exit 1 / 11.6 ms | hydro_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_isinstance.py` | ok / 830.8 ms | exit 1 / 3 ms | hydro_fail | error[HYD0002]: module 'typing' not found: not supported unless a sibling typing.py exists |
| `test_iter.py` | ok / 63.5 ms | exit 1 / 6 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_iterlen.py` | ok / 42.5 ms | exit 1 / 2.2 ms | hydro_fail | error[HYD0002]: module 'itertools' not found: not supported unless a sibling itertools.py exists |
| `test_itertools.py` | ok / 788.5 ms | exit 1 / 16.8 ms | hydro_fail | error[HYD0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_keyword.py` | ok / 41.6 ms | exit 1 / 1.9 ms | hydro_fail | error[HYD0002]: module 'keyword' not found: not supported unless a sibling keyword.py exists |
| `test_keywordonlyarg.py` | ok / 49.8 ms | exit 1 / 4.2 ms | hydro_fail | error[HYD0002]: *args parameters are not supported in Phase 1 |
| `test_kqueue.py` | exit 1 / 42.1 ms | exit 1 / 3.1 ms | expected_fail | error[HYD0002]: module 'select' not found: not supported unless a sibling select.py exists |
| `test_largefile.py` | ok / 796.9 ms | exit 1 / 4 ms | hydro_fail | error[HYD0002]: module 'shutil' not found: not supported unless a sibling shutil.py exists |
| `test_launcher.py` | exit 1 / 55.4 ms | exit 1 / 5.9 ms | expected_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_linecache.py` | ok / 115.8 ms | exit 1 / 4 ms | hydro_fail | error[HYD0002]: module 'importlib' not found: not supported unless a sibling importlib.py exists |
| `test_list.py` | ok / 399.4 ms | exit 1 / 3.7 ms | hydro_fail | error[HYD0002]: module 'signal' not found: not supported unless a sibling signal.py exists |
| `test_listcomps.py` | ok / 162.3 ms | exit 1 / 4.3 ms | hydro_fail | error[HYD0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_lltrace.py` | ok / 49.8 ms | exit 1 / 2.2 ms | hydro_fail | error[HYD0002]: module 'dis' not found: not supported unless a sibling dis.py exists |
| `test_locale.py` | ok / 141.7 ms | exit 1 / 3.8 ms | hydro_fail | error[HYD0002]: module 'decimal' not found: not supported unless a sibling decimal.py exists |
| `test_logging.py` | ok / 20853.6 ms | exit 1 / 26.7 ms | hydro_fail | error[HYD0002]: module 'logging' not found: not supported unless a sibling logging.py exists |
| `test_long.py` | ok / 515.2 ms | exit 1 / 8.2 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_longexp.py` | ok / 113.8 ms | exit 1 / 1.9 ms | hydro_fail | error[HYD0101]: name 'eval' is not defined |
| `test_lzma.py` | ok / 167 ms | exit 1 / 8.6 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_mailbox.py` | ok / 1238.5 ms | exit 1 / 13.6 ms | hydro_fail | error[HYD0002]: module 'email' not found: not supported unless a sibling email.py exists |
| `test_marshal.py` | ok / 226.4 ms | exit 1 / 5.2 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_math.py` | ok / 9599.1 ms | exit 1 / 15 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_math_property.py` | ok / 37.5 ms | exit 1 / 1.7 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_memoryio.py` | ok / 79.8 ms | exit 1 / 6.7 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_memoryview.py` | ok / 308.8 ms | exit 1 / 5.4 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_metaclass.py` | ok / 131.7 ms | exit 1 / 1.8 ms | hydro_fail | error[HYD0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_mimetypes.py` | ok / 449.9 ms | exit 1 / 4.9 ms | hydro_fail | error[HYD0002]: module 'mimetypes' not found: not supported unless a sibling mimetypes.py exists |
| `test_minidom.py` | ok / 89.9 ms | exit 1 / 8.5 ms | hydro_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_mmap.py` | ok / 117.5 ms | exit 1 / 7.3 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_modulefinder.py` | ok / 288.8 ms | exit 1 / 2.4 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'importlib.machinery' |
| `test_monitoring.py` | ok / 60.5 ms | exit 1 / 9.1 ms | hydro_fail | error[HYD0002]: module 'dis' not found: not supported unless a sibling dis.py exists |
| `test_msvcrt.py` | exit 1 / 36.6 ms | exit 1 / 2.6 ms | expected_fail | error[HYD0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_multibytecodec.py` | ok / 233.5 ms | exit 1 / 4.2 ms | hydro_fail | error[HYD0002]: module '_multibytecodec' not found: not supported unless a sibling _multibytecodec.py exists |
| `test_multiprocessing_main_handling.py` | exit 1 / 1358.9 ms | exit 1 / 3.1 ms | expected_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_named_expressions.py` | ok / 54.9 ms | exit 1 / 29.5 ms | hydro_fail | error[HYD0101]: name 'exec' is not defined |
| `test_netrc.py` | ok / 54.7 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: module 'netrc' not found: not supported unless a sibling netrc.py exists |
| `test_ntpath.py` | ok / 292.8 ms | exit 1 / 9.5 ms | hydro_fail | error[HYD0002]: module 'inspect' not found: not supported unless a sibling inspect.py exists |
| `test_nturl2path.py` | ok / 69.9 ms | exit 1 / 3 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'urllib.parse' |
| `test_numeric_tower.py` | ok / 84.2 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: module 'decimal' not found: not supported unless a sibling decimal.py exists |
| `test_opcache.py` | ok / 346.8 ms | exit 1 / 7.2 ms | hydro_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_opcodes.py` | ok / 69.5 ms | exit 1 / 3 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_openpty.py` | ok / 46.8 ms | exit 1 / 1.8 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'openpty' |
| `test_operator.py` | ok / 69.3 ms | exit 1 / 5.6 ms | hydro_fail | error[HYD0002]: module 'inspect' not found: not supported unless a sibling inspect.py exists |
| `test_optimizer.py` | ok / 55.2 ms | exit 1 / 2.2 ms | hydro_fail | error[HYD0002]: module 'types' not found: not supported unless a sibling types.py exists |
| `test_optparse.py` | ok / 143.7 ms | exit 1 / 7 ms | hydro_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_ordered_dict.py` | ok / 188.5 ms | exit 1 / 6.4 ms | hydro_fail | error[HYD0002]: module 'builtins' not found: not supported unless a sibling builtins.py exists |
| `test_os.py` | exit 1 / 12681.8 ms | exit 1 / 21.3 ms | expected_fail | error[HYD0002]: module 'codecs' not found: not supported unless a sibling codecs.py exists |
| `test_osx_env.py` | ok / 57.8 ms | exit 1 / 2 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support.os_helper' |
| `test_patma.py` | exit 1 / 55.2 ms | exit 1 / 13.9 ms | expected_fail | error[HYD0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_pdb.py` | exit 1 / 19120.6 ms | exit 1 / 10.1 ms | expected_fail | error[HYD0002]: module '_colorize' not found: not supported unless a sibling _colorize.py exists |
| `test_peepholer.py` | ok / 446.5 ms | exit 1 / 14 ms | hydro_fail | error[HYD0002]: module 'ast' not found: not supported unless a sibling ast.py exists |
| `test_pep646_syntax.py` | ok / 270.4 ms | exit 1 / 1.8 ms | hydro_fail | error[HYD0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_perf_profiler.py` | ok / 402.9 ms | exit 1 / 4.3 ms | hydro_fail | error[HYD0002]: module 'string' not found: not supported unless a sibling string.py exists |
| `test_perfmaps.py` | ok / 104 ms | exit 1 / 1.8 ms | hydro_fail | error[HYD0101]: name 'write_perf_map_entry' is not defined |
| `test_pickle.py` | ok / 2155.6 ms | exit 1 / 6.1 ms | hydro_fail | error[HYD0002]: module '_compat_pickle' not found: not supported unless a sibling _compat_pickle.py exists |
| `test_picklebuffer.py` | ok / 141.4 ms | exit 1 / 2.6 ms | hydro_fail | error[HYD0002]: module 'gc' not found: not supported unless a sibling gc.py exists |
| `test_pickletools.py` | ok / 570.7 ms | exit 1 / 3.3 ms | hydro_fail | error[HYD0002]: module 'pickletools' not found: not supported unless a sibling pickletools.py exists |
| `test_pkg.py` | ok / 122.1 ms | exit 1 / 2.9 ms | hydro_fail | error[HYD0002]: module 'tempfile' not found: not supported unless a sibling tempfile.py exists |
| `test_pkgutil.py` | exit 1 / 150.5 ms | exit 1 / 4.1 ms | expected_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support.import_helper' |
| `test_platform.py` | ok / 421 ms | exit 1 / 4.8 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_plistlib.py` | ok / 264.9 ms | exit 1 / 5.7 ms | hydro_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_poll.py` | ok / 11270.8 ms | exit 1 / 3.1 ms | hydro_fail | error[HYD0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_popen.py` | ok / 171.2 ms | exit 1 / 2 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_poplib.py` | ok / 6533.7 ms | exit 1 / 3.7 ms | hydro_fail | error[HYD0002]: module 'poplib' not found: not supported unless a sibling poplib.py exists |
| `test_positional_only_arg.py` | ok / 134.3 ms | exit 1 / 4 ms | hydro_fail | error[HYD0002]: module 'dis' not found: not supported unless a sibling dis.py exists |
| `test_posix.py` | exit 1 / 881 ms | exit 1 / 11.7 ms | expected_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_posixpath.py` | ok / 333.4 ms | exit 1 / 6.3 ms | hydro_fail | error[HYD0002]: module 'inspect' not found: not supported unless a sibling inspect.py exists |
| `test_pow.py` | ok / 292.2 ms | exit 1 / 4.6 ms | hydro_fail | error[HYD0101]: name 'pow' is not defined |
| `test_pprint.py` | ok / 172.5 ms | exit 1 / 5.3 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_print.py` | ok / 142.1 ms | exit 1 / 2.8 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_profile.py` | ok / 307.7 ms | exit 1 / 2.4 ms | hydro_fail | error[HYD0002]: module 'pstats' not found: not supported unless a sibling pstats.py exists |
| `test_property.py` | ok / 96.2 ms | exit 1 / 3.8 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_pstats.py` | ok / 164.5 ms | exit 1 / 2.5 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_pty.py` | ok / 113.8 ms | exit 1 / 4.3 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_pulldom.py` | ok / 177.3 ms | exit 1 / 2.5 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'xml.sax' |
| `test_pwd.py` | ok / 101.3 ms | exit 1 / 2.3 ms | hydro_fail | error[HYD0002]: module 'string' not found: not supported unless a sibling string.py exists |
| `test_py_compile.py` | ok / 348.6 ms | exit 1 / 3 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'importlib.util' |
| `test_pyclbr.py` | exit 1 / 109.2 ms | exit 1 / 3.4 ms | expected_fail | error[HYD0002]: dotted imports are not supported for user modules: 'importlib.machinery' |
| `test_pyexpat.py` | ok / 220.5 ms | exit 1 / 5.5 ms | hydro_fail | error[HYD0002]: module 'abc' not found: not supported unless a sibling abc.py exists |
| `test_queue.py` | ok / 7733.3 ms | exit 1 / 4.9 ms | hydro_fail | error[HYD0002]: module 'itertools' not found: not supported unless a sibling itertools.py exists |
| `test_quopri.py` | ok / 137.2 ms | exit 1 / 2.1 ms | hydro_fail | error[HYD0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_raise.py` | ok / 93.3 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_random.py` | ok / 808.3 ms | exit 1 / 6.9 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'unittest.mock' |
| `test_range.py` | ok / 200.5 ms | exit 1 / 4.8 ms | hydro_fail | error[HYD0002]: module 'itertools' not found: not supported unless a sibling itertools.py exists |
| `test_re.py` | ok / 453.2 ms | exit 1 / 18.6 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_readline.py` | ok / 273.3 ms | exit 1 / 3.9 ms | hydro_fail | error[HYD0002]: module 'codecs' not found: not supported unless a sibling codecs.py exists |
| `test_regrtest.py` | exit 1 / 1590.6 ms | exit 1 / 10.9 ms | expected_fail | error[HYD0002]: module '_colorize' not found: not supported unless a sibling _colorize.py exists |
| `test_remote_pdb.py` | ok / 2447.6 ms | exit 1 / 7.1 ms | hydro_fail | error[HYD0002]: module 'itertools' not found: not supported unless a sibling itertools.py exists |
| `test_repl.py` | ok / 1024.7 ms | exit 1 / 2.8 ms | hydro_fail | error[HYD0002]: module 'select' not found: not supported unless a sibling select.py exists |
| `test_reprlib.py` | ok / 70.5 ms | exit 1 / 4.4 ms | hydro_fail | error[HYD0002]: module 'annotationlib' not found: not supported unless a sibling annotationlib.py exists |
| `test_resource.py` | ok / 85.6 ms | exit 1 / 2.5 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_richcmp.py` | ok / 42.5 ms | exit 1 / 2.9 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_rlcompleter.py` | ok / 109.4 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'unittest.mock' |
| `test_robotparser.py` | ok / 1150.1 ms | exit 1 / 3.4 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'urllib.robotparser' |
| `test_runpy.py` | timeout / 30009.5 ms | exit 1 / 5.2 ms | expected_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_sax.py` | ok / 190.8 ms | exit 1 / 6.1 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'xml.sax' |
| `test_sched.py` | ok / 585.3 ms | exit 1 / 3.6 ms | hydro_fail | error[HYD0002]: module 'sched' not found: not supported unless a sibling sched.py exists |
| `test_scope.py` | ok / 51.5 ms | exit 1 / 3.5 ms | hydro_fail | error[HYD0002]: module 'weakref' not found: not supported unless a sibling weakref.py exists |
| `test_script_helper.py` | ok / 190.2 ms | exit 1 / 2.1 ms | hydro_fail | error[HYD0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_secrets.py` | ok / 39.8 ms | exit 1 / 2.5 ms | hydro_fail | error[HYD0002]: module 'secrets' not found: not supported unless a sibling secrets.py exists |
| `test_select.py` | ok / 1556.6 ms | exit 1 / 2.5 ms | hydro_fail | error[HYD0002]: module 'select' not found: not supported unless a sibling select.py exists |
| `test_selectors.py` | ok / 14735.2 ms | exit 1 / 4.8 ms | hydro_fail | error[HYD0002]: module 'selectors' not found: not supported unless a sibling selectors.py exists |
| `test_set.py` | ok / 749.2 ms | exit 1 / 10.1 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'collections.abc' |
| `test_setcomps.py` | ok / 129.5 ms | exit 1 / 2.6 ms | hydro_fail | error[HYD0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_shelve.py` | ok / 2552.9 ms | exit 1 / 2.6 ms | hydro_fail | error[HYD0002]: module 'dbm' not found: not supported unless a sibling dbm.py exists |
| `test_shlex.py` | ok / 141.2 ms | exit 1 / 3.4 ms | hydro_fail | error[HYD0002]: module 'itertools' not found: not supported unless a sibling itertools.py exists |
| `test_shutil.py` | exit 1 / 488.1 ms | exit 1 / 16.2 ms | expected_fail | error[HYD0002]: dotted imports are not supported for user modules: 'unittest.mock' |
| `test_signal.py` | timeout / 30018.8 ms | exit 1 / 5.4 ms | expected_fail | error[HYD0002]: module 'enum' not found: not supported unless a sibling enum.py exists |
| `test_site.py` | ok / 2315.3 ms | exit 1 / 5.5 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_slice.py` | ok / 56.8 ms | exit 1 / 3.5 ms | hydro_fail | error[HYD0002]: module 'itertools' not found: not supported unless a sibling itertools.py exists |
| `test_smtplib.py` | ok / 1434.4 ms | exit 1 / 6.9 ms | hydro_fail | error[HYD0002]: module 'base64' not found: not supported unless a sibling base64.py exists |
| `test_smtpnet.py` | ok / 10504.7 ms | exit 1 / 2.2 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_socket.py` | timeout / 30011.4 ms | exit 1 / 26.7 ms | expected_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_socketserver.py` | ok / 515.3 ms | exit 1 / 3.6 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_sort.py` | ok / 81.4 ms | exit 1 / 2.9 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_source_encoding.py` | ok / 13287.4 ms | exit 1 / 3.9 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_ssl.py` | ok / 21400.4 ms | exit 1 / 21.9 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_stable_abi_ctypes.py` | exit 1 / 120.5 ms | exit 1 / 2.7 ms | expected_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support.import_helper' |
| `test_startfile.py` | exit 1 / 120.5 ms | exit 1 / 1.9 ms | expected_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_stat.py` | ok / 149.5 ms | exit 1 / 2.8 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_statistics.py` | ok / 9719.4 ms | exit 1 / 14.6 ms | hydro_fail | error[HYD0002]: module 'bisect' not found: not supported unless a sibling bisect.py exists |
| `test_str.py` | ok / 1152 ms | exit 1 / 15.1 ms | hydro_fail | error[HYD0002]: module '_string' not found: not supported unless a sibling _string.py exists |
| `test_strftime.py` | ok / 169.6 ms | exit 1 / 2.5 ms | hydro_fail | error[HYD0002]: module 'calendar' not found: not supported unless a sibling calendar.py exists |
| `test_string_literals.py` | ok / 132 ms | exit 1 / 3.1 ms | hydro_fail | error[HYD0002]: module 'shutil' not found: not supported unless a sibling shutil.py exists |
| `test_stringprep.py` | ok / 116 ms | exit 1 / 1.7 ms | hydro_fail | error[HYD0002]: module 'stringprep' not found: not supported unless a sibling stringprep.py exists |
| `test_strptime.py` | ok / 156 ms | exit 1 / 6 ms | hydro_fail | error[HYD0002]: module 'locale' not found: not supported unless a sibling locale.py exists |
| `test_strtod.py` | ok / 236.6 ms | exit 1 / 2.4 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_struct.py` | ok / 397.1 ms | exit 1 / 6.5 ms | hydro_fail | error[HYD0002]: module 'itertools' not found: not supported unless a sibling itertools.py exists |
| `test_structseq.py` | ok / 162 ms | exit 1 / 3.1 ms | hydro_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_subclassinit.py` | ok / 118.6 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: module 'types' not found: not supported unless a sibling types.py exists |
| `test_subprocess.py` | exit 1 / 15509.9 ms | exit 1 / 14.1 ms | expected_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_sundry.py` | ok / 105.8 ms | exit 1 / 1.6 ms | hydro_fail | error[HYD0002]: module 'importlib' not found: not supported unless a sibling importlib.py exists |
| `test_super.py` | ok / 231.5 ms | exit 1 / 3.2 ms | hydro_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_support.py` | exit 1 / 4944.9 ms | exit 1 / 5.3 ms | expected_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_symtable.py` | ok / 61.9 ms | exit 1 / 3.4 ms | hydro_fail | error[HYD0002]: module 'textwrap' not found: not supported unless a sibling textwrap.py exists |
| `test_syntax.py` | ok / 188 ms | exit 1 / 4.7 ms | hydro_fail | error[HYD0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_sys.py` | exit 1 / 1245 ms | exit 1 / 9.4 ms | expected_fail | error[HYD0002]: module 'builtins' not found: not supported unless a sibling builtins.py exists |
| `test_sys_setprofile.py` | ok / 45.9 ms | exit 1 / 3.8 ms | hydro_fail | error[HYD0002]: module 'gc' not found: not supported unless a sibling gc.py exists |
| `test_sys_settrace.py` | ok / 808.6 ms | exit 1 / 10.7 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_sysconfig.py` | exit 1 / 196.8 ms | exit 1 / 5.9 ms | expected_fail | error[HYD0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_syslog.py` | ok / 187.8 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_tabnanny.py` | ok / 126.4 ms | exit 1 / 2.8 ms | hydro_fail | error[HYD0002]: module 'tabnanny' not found: not supported unless a sibling tabnanny.py exists |
| `test_tarfile.py` | exit 1 / 5601.9 ms | exit 1 / 19.6 ms | expected_fail | error[HYD0002]: module 'hashlib' not found: not supported unless a sibling hashlib.py exists |
| `test_tcl.py` | ok / 121.6 ms | exit 1 / 5.5 ms | hydro_fail | error[HYD0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_tempfile.py` | ok / 483.1 ms | exit 1 / 9.4 ms | hydro_fail | error[HYD0002]: module 'tempfile' not found: not supported unless a sibling tempfile.py exists |
| `test_termios.py` | ok / 577.1 ms | exit 1 / 3.2 ms | hydro_fail | error[HYD0002]: module 'tempfile' not found: not supported unless a sibling tempfile.py exists |
| `test_textwrap.py` | ok / 48.9 ms | exit 1 / 4.9 ms | hydro_fail | error[HYD0002]: module 'textwrap' not found: not supported unless a sibling textwrap.py exists |
| `test_thread.py` | ok / 1282 ms | exit 1 / 3.2 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_thread_local_bytecode.py` | ok / 55 ms | exit 1 / 2.1 ms | hydro_fail | error[HYD0002]: module 'textwrap' not found: not supported unless a sibling textwrap.py exists |
| `test_threadedtempfile.py` | ok / 53.1 ms | exit 1 / 2.3 ms | hydro_fail | error[HYD0002]: module 'tempfile' not found: not supported unless a sibling tempfile.py exists |
| `test_threading.py` | exit 1 / 14090 ms | exit 1 / 6.7 ms | expected_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_threading_local.py` | ok / 298.2 ms | exit 1 / 2.4 ms | hydro_fail | error[HYD0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_threadsignals.py` | ok / 4897.5 ms | exit 1 / 2.1 ms | hydro_fail | error[HYD0002]: module 'signal' not found: not supported unless a sibling signal.py exists |
| `test_time.py` | ok / 3080.5 ms | exit 1 / 5.3 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_timeit.py` | ok / 57.9 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: module 'timeit' not found: not supported unless a sibling timeit.py exists |
| `test_timeout.py` | ok / 9988.7 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_tokenize.py` | ok / 6487.7 ms | exit 1 / 8.4 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_trace.py` | ok / 976.2 ms | exit 1 / 3.9 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_traceback.py` | exit 1 / 2287.7 ms | exit 1 / 16.7 ms | expected_fail | error[HYD0002]: module 'linecache' not found: not supported unless a sibling linecache.py exists |
| `test_tracemalloc.py` | ok / 682.4 ms | exit 1 / 5.9 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_tstring.py` | exit 1 / 282.4 ms | exit 1 / 1.7 ms | expected_fail | error[HYD0001]: invalid syntax: invalid syntax. Got unexpected token "Hello" |
| `test_ttk_textonly.py` | ok / 77.1 ms | exit 1 / 4 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_tty.py` | ok / 52.9 ms | exit 1 / 3.1 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support.import_helper' |
| `test_tuple.py` | ok / 235.3 ms | exit 1 / 3.3 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_turtle.py` | ok / 202.5 ms | exit 1 / 4 ms | hydro_fail | error[HYD0002]: module 'tempfile' not found: not supported unless a sibling tempfile.py exists |
| `test_type_aliases.py` | ok / 56.6 ms | exit 1 / 4 ms | hydro_fail | error[HYD0002]: module 'types' not found: not supported unless a sibling types.py exists |
| `test_type_annotations.py` | ok / 52.1 ms | exit 1 / 4.2 ms | hydro_fail | error[HYD0002]: module 'annotationlib' not found: not supported unless a sibling annotationlib.py exists |
| `test_type_cache.py` | exit 1 / 48.9 ms | exit 1 / 2.6 ms | expected_fail | error[HYD0002]: dotted imports are not supported for user modules: 'collections.abc' |
| `test_type_comments.py` | ok / 49.3 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: module 'ast' not found: not supported unless a sibling ast.py exists |
| `test_type_params.py` | ok / 56 ms | exit 1 / 4.8 ms | hydro_fail | error[HYD0001]: invalid syntax: invalid syntax. Got unexpected token '=' |
| `test_typechecks.py` | ok / 44.4 ms | exit 1 / 2.4 ms | hydro_fail | error[HYD0002]: class keywords (metaclass, ...) are not supported yet |
| `test_types.py` | ok / 238 ms | exit 1 / 12.1 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_typing.py` | ok / 399.9 ms | exit 1 / 39.2 ms | hydro_fail | error[HYD0002]: module 'annotationlib' not found: not supported unless a sibling annotationlib.py exists |
| `test_ucn.py` | ok / 137 ms | exit 1 / 3.1 ms | hydro_fail | error[HYD0002]: module 'ast' not found: not supported unless a sibling ast.py exists |
| `test_unary.py` | ok / 46.4 ms | exit 1 / 2.1 ms | hydro_fail | error[HYD0002]: complex literals are not supported in Phase 1 |
| `test_unicode_file.py` | ok / 47.7 ms | exit 1 / 2 ms | hydro_fail | error[HYD0002]: module 'glob' not found: not supported unless a sibling glob.py exists |
| `test_unicode_file_functions.py` | ok / 69.3 ms | exit 1 / 2.3 ms | hydro_fail | error[HYD0002]: module 'warnings' not found: not supported unless a sibling warnings.py exists |
| `test_unicode_identifiers.py` | ok / 44.2 ms | exit 1 / 1.8 ms | hydro_fail | error[HYD0002]: nested class definitions are not supported yet |
| `test_unicodedata.py` | ok / 4359.5 ms | exit 1 / 6.8 ms | hydro_fail | error[HYD0002]: module 'hashlib' not found: not supported unless a sibling hashlib.py exists |
| `test_univnewlines.py` | ok / 62.6 ms | exit 1 / 2.1 ms | hydro_fail | error[HYD0002]: module '_pyio' not found: not supported unless a sibling _pyio.py exists |
| `test_unpack.py` | exit 1 / 161.2 ms | exit 1 / 2.3 ms | expected_fail | error[HYD0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_unpack_ex.py` | exit 1 / 156.6 ms | exit 1 / 1.7 ms | expected_fail | error[HYD0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_unparse.py` | ok / 7398.5 ms | exit 1 / 6 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_urllib.py` | ok / 505.4 ms | exit 1 / 7.3 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'urllib.parse' |
| `test_urllib2.py` | ok / 2071.9 ms | exit 1 / 10.1 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_urllib2_localnet.py` | ok / 1975.7 ms | exit 1 / 4.1 ms | hydro_fail | error[HYD0002]: module 'base64' not found: not supported unless a sibling base64.py exists |
| `test_urllib2net.py` | ok / 29492.2 ms | exit 1 / 2.8 ms | hydro_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_urllib_response.py` | ok / 54.8 ms | exit 1 / 2.3 ms | hydro_fail | error[HYD0002]: module 'tempfile' not found: not supported unless a sibling tempfile.py exists |
| `test_urllibnet.py` | ok / 7514.1 ms | exit 1 / 2.3 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_urlparse.py` | ok / 145 ms | exit 1 / 10.3 ms | hydro_fail | error[HYD0002]: module 'unicodedata' not found: not supported unless a sibling unicodedata.py exists |
| `test_userdict.py` | ok / 156.2 ms | exit 1 / 3.9 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_userlist.py` | ok / 169.9 ms | exit 1 / 3.1 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_userstring.py` | ok / 650.5 ms | exit 1 / 3 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_utf8_mode.py` | ok / 457.4 ms | exit 1 / 3.3 ms | hydro_fail | error[HYD0002]: module 'locale' not found: not supported unless a sibling locale.py exists |
| `test_utf8source.py` | ok / 50.4 ms | exit 1 / 2.2 ms | hydro_fail | error[HYD0002]: imports are only supported at module level in Phase 2.10 |
| `test_uuid.py` | ok / 284.5 ms | exit 1 / 8.1 ms | hydro_fail | error[HYD0002]: module 'builtins' not found: not supported unless a sibling builtins.py exists |
| `test_venv.py` | exit 1 / 7650.2 ms | exit 1 / 6.6 ms | expected_fail | error[HYD0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_wait3.py` | ok / 5095.7 ms | exit 1 / 2.1 ms | hydro_fail | error[HYD0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_wait4.py` | ok / 5057.6 ms | exit 1 / 2.1 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.fork_wait' |
| `test_wave.py` | ok / 69.4 ms | exit 1 / 3.3 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_weakref.py` | exit 1 / 2571.3 ms | exit 1 / 8.8 ms | expected_fail | error[HYD0002]: module 'gc' not found: not supported unless a sibling gc.py exists |
| `test_weakset.py` | ok / 69.5 ms | exit 1 / 4.1 ms | hydro_fail | error[HYD0002]: module 'weakref' not found: not supported unless a sibling weakref.py exists |
| `test_webbrowser.py` | ok / 2173.1 ms | exit 1 / 4 ms | hydro_fail | error[HYD0002]: module 'shlex' not found: not supported unless a sibling shlex.py exists |
| `test_winapi.py` | exit 1 / 52.8 ms | exit 1 / 2.5 ms | expected_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_winconsoleio.py` | exit 1 / 52.5 ms | exit 1 / 2.6 ms | expected_fail | error[HYD0002]: module 'tempfile' not found: not supported unless a sibling tempfile.py exists |
| `test_winreg.py` | exit 1 / 58 ms | exit 1 / 3.5 ms | expected_fail | error[HYD0002]: module 'gc' not found: not supported unless a sibling gc.py exists |
| `test_winsound.py` | exit 1 / 50.8 ms | exit 1 / 2.4 ms | expected_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_with.py` | ok / 46.9 ms | exit 1 / 3.9 ms | hydro_fail | error[HYD0002]: module 'traceback' not found: not supported unless a sibling traceback.py exists |
| `test_wmi.py` | exit 1 / 47.5 ms | exit 1 / 2 ms | expected_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_wsgiref.py` | ok / 233.8 ms | exit 1 / 4.8 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_xml_dom_minicompat.py` | ok / 50.7 ms | exit 1 / 2.9 ms | hydro_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_xml_dom_xmlbuilder.py` | ok / 182.1 ms | exit 1 / 2 ms | hydro_fail | error[HYD0002]: module 'http' not found: not supported unless a sibling http.py exists |
| `test_xml_etree.py` | ok / 797.1 ms | exit 1 / 18.4 ms | hydro_fail | error[HYD0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_xml_etree_c.py` | ok / 520.4 ms | exit 1 / 3 ms | hydro_fail | error[HYD0002]: module 'struct' not found: not supported unless a sibling struct.py exists |
| `test_xmlrpc.py` | ok / 27421.4 ms | exit 1 / 7.2 ms | hydro_fail | error[HYD0002]: module 'base64' not found: not supported unless a sibling base64.py exists |
| `test_xpickle.py` | ok / 2387.7 ms | exit 1 / 2.7 ms | hydro_fail | error[HYD0002]: module 'struct' not found: not supported unless a sibling struct.py exists |
| `test_xxlimited.py` | exit 1 / 51.4 ms | exit 1 / 1.9 ms | expected_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_xxtestfuzz.py` | ok / 55.5 ms | exit 1 / 2 ms | hydro_fail | error[HYD0002]: module 'faulthandler' not found: not supported unless a sibling faulthandler.py exists |
| `test_yield_from.py` | ok / 59.1 ms | exit 1 / 5.6 ms | hydro_fail | error[HYD0002]: module 'inspect' not found: not supported unless a sibling inspect.py exists |
| `test_zipapp.py` | ok / 150.9 ms | exit 1 / 3.5 ms | hydro_fail | error[HYD0002]: module 'stat' not found: not supported unless a sibling stat.py exists |
| `test_zipfile64.py` | timeout / 30030.8 ms | exit 1 / 2.4 ms | expected_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_zipimport.py` | ok / 18396.4 ms | exit 1 / 6.9 ms | hydro_fail | error[HYD0002]: module 'marshal' not found: not supported unless a sibling marshal.py exists |
| `test_zipimport_support.py` | ok / 495.1 ms | exit 1 / 2.8 ms | hydro_fail | error[HYD0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_zlib.py` | ok / 434.7 ms | exit 1 / 5.9 ms | hydro_fail | error[HYD0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_zstd.py` | ok / 359.9 ms | exit 1 / 11.8 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported unless a sibling array.py exists |

<!-- END GENERATED RESULTS -->
