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
  "minimum_passing": 3,
  "required": [],
  "known_passing": [
    "test_errno.py",
    "test_int_literal.py",
    "test_longexp.py"
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

## Passing tests and `required`

The manifest records three genuine passes from the full 2026-10-29 run:
`test_errno.py` (native `errno`, `unittest`, module values/globals),
`test_int_literal.py` (arbitrary-precision int literals, lexer edge cases) and
`test_longexp.py` (long regular expressions). `required` remains empty until
the project pins a never-regress subset; `minimum_passing` is 3 so the gate
cannot silently drop below the recorded full-run count. The separate
import-only baseline in [`COMPATIBILITY.md`](COMPATIBILITY.md) tracks module
imports and makes no test-suite claim.

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

Source: `/mnt/Vault/GitHub/hydro/.hydro/cpython/Python-3.14.9` · generated 2026-10-28T23:41:12Z

## Summary

| Status | Tests |
| --- | ---: |
| hydro_pass | 3 |
| hydro_fail | 332 |
| cpython_fail | 0 |
| expected_fail | 63 |
| timeout | 0 |
| **Total** | 398 |

Passing: **3 / 398** (a pass requires Hydro and CPython 3.14.9 to both exit 0).

## Passing tests

- `test_errno.py`
- `test_int_literal.py`
- `test_longexp.py`

## Failure classes

| Status | Tests |
| --- | ---: |
| hydro_fail | 332 |
| expected_fail | 63 |

## Test results

| Test | CPython 3.14.9 | Hydro | Status | First error |
| --- | --- | --- | --- | --- |
| `test___all__.py` | ok / 2064.3 ms | exit 1 / 26.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test__colorize.py` | ok / 111.5 ms | exit 1 / 42.2 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test__interpchannels.py` | ok / 870.6 ms | exit 1 / 49.8 ms | hydro_fail | error[HYD0002]: module '_interpreters' not found: not supported on the import search path |
| `test__interpreters.py` | ok / 634.4 ms | exit 1 / 38.2 ms | hydro_fail | error[HYD0002]: module '_interpreters' not found: not supported on the import search path |
| `test__locale.py` | ok / 50.7 ms | exit 1 / 29.4 ms | hydro_fail | error[HYD0002]: module '_locale' not found: not supported on the import search path |
| `test__opcode.py` | ok / 46.8 ms | exit 1 / 66.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test__osx_support.py` | ok / 49.3 ms | exit 1 / 28.9 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_abc.py` | ok / 44.7 ms | exit 1 / 8.9 ms | hydro_fail | error[HYD0002]: pytest fixture 'abc_ABCMeta' is not defined |
| `test_abstract_numbers.py` | ok / 43.1 ms | exit 1 / 52.4 ms | hydro_fail | AssertionError: 7 test(s) failed |
| `test_android.py` | exit 1 / 105.8 ms | exit 1 / 40.6 ms | expected_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_annotationlib.py` | ok / 67.5 ms | exit 1 / 35.6 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_apple.py` | exit 1 / 101 ms | exit 1 / 39.9 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_argparse.py` | ok / 1325.6 ms | exit 1 / 57.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_array.py` | ok / 287.3 ms | exit 1 / 41.2 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_asdl_parser.py` | exit 1 / 31.6 ms | exit 1 / 3.5 ms | expected_fail | error[HYD0201]: module 'importlib.util' has no attribute 'spec_from_loader' |
| `test_asyncgen.py` | ok / 834.2 ms | exit 1 / 31.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_atexit.py` | exit 1 / 117.3 ms | exit 1 / 38.7 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_audit.py` | ok / 1474.3 ms | exit 1 / 28.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_augassign.py` | ok / 42.9 ms | exit 1 / 39.7 ms | hydro_fail | AssertionError: 3 test(s) failed |
| `test_base64.py` | ok / 143.8 ms | exit 1 / 39.6 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_baseexception.py` | ok / 52.6 ms | exit 1 / 27.1 ms | hydro_fail | AssertionError: 11 test(s) failed |
| `test_bdb.py` | exit 1 / 67.1 ms | exit 1 / 62.4 ms | expected_fail | error[HYD0201]: module 'sys' has no attribute 'monitoring' |
| `test_bigaddrspace.py` | ok / 43.6 ms | exit 1 / 25.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_bigmem.py` | ok / 61.3 ms | exit 1 / 31.6 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_binascii.py` | ok / 53.5 ms | exit 1 / 31.4 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_binop.py` | ok / 42.3 ms | exit -6 / 246.6 ms | hydro_fail | fatal runtime error: stack overflow, aborting |
| `test_bisect.py` | ok / 67.3 ms | exit 1 / 30.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_bool.py` | ok / 48.7 ms | exit 1 / 28.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_buffer.py` | ok / 78.4 ms | exit 1 / 37.3 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_bufio.py` | ok / 141.6 ms | exit 1 / 31.2 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_build_details.py` | exit 1 / 120.5 ms | exit 1 / 29.1 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_builtin.py` | ok / 365 ms | exit 1 / 58.5 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_bytes.py` | exit 1 / 88.2 ms | exit 1 / 54 ms | expected_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_bz2.py` | ok / 361 ms | exit 1 / 30.1 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_c_locale_coercion.py` | ok / 807 ms | exit 1 / 45 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_calendar.py` | ok / 1369.5 ms | exit 1 / 154.2 ms | hydro_fail | error[HYD0101]: name 'FEBRUARY' is not defined |
| `test_call.py` | exit 1 / 57.2 ms | exit 1 / 32 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_charmapcodec.py` | ok / 41.2 ms | exit 1 / 2.5 ms | hydro_fail | error[HYD0201]: module 'codecs' has no attribute 'register' |
| `test_class.py` | ok / 59 ms | exit 1 / 27.3 ms | hydro_fail | error[HYD0002]: module '_testinternalcapi' not found: not supported on the import search path |
| `test_clinic.py` | ok / 213.4 ms | exit 1 / 42.7 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_cmath.py` | ok / 85.3 ms | exit 1 / 35 ms | hydro_fail | error[HYD0002]: module 'cmath' not found: not supported on the import search path |
| `test_cmd.py` | ok / 198.4 ms | exit 1 / 52.7 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_cmd_line.py` | exit 1 / 1752.9 ms | exit 1 / 43 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_cmd_line_script.py` | exit 1 / 1293.9 ms | exit 1 / 40.9 ms | expected_fail | error[HYD0002]: module '_frozen_importlib_external' not found: not supported on the import search path |
| `test_code.py` | ok / 207.7 ms | exit 1 / 68.1 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_code_module.py` | ok / 131.1 ms | exit 1 / 28.9 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_codeccallbacks.py` | ok / 56.1 ms | exit 1 / 141.2 ms | hydro_fail | error[HYD0201]: module '_codecs' has no attribute '_unregister_error' |
| `test_codecencodings_cn.py` | ok / 119.6 ms | exit 1 / 144.3 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecencodings_hk.py` | ok / 98.1 ms | exit 1 / 148.1 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecencodings_iso2022.py` | ok / 137.3 ms | exit 1 / 155 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecencodings_jp.py` | ok / 159.4 ms | exit 1 / 145.9 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecencodings_kr.py` | ok / 110.7 ms | exit 1 / 147.5 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecencodings_tw.py` | ok / 137.9 ms | exit 1 / 145 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecmaps_cn.py` | ok / 170.3 ms | exit 1 / 145.2 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecmaps_hk.py` | ok / 109.5 ms | exit 1 / 143.1 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecmaps_jp.py` | ok / 175 ms | exit 1 / 149.9 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecmaps_kr.py` | ok / 169.6 ms | exit 1 / 142.9 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecmaps_tw.py` | ok / 140.3 ms | exit 1 / 144.4 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecs.py` | ok / 308.9 ms | exit 1 / 51.8 ms | hydro_fail | error[HYD0201]: module 'codecs' has no attribute 'code_page_encode' |
| `test_codeop.py` | ok / 51.3 ms | exit 1 / 28 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_collections.py` | ok / 1102.4 ms | exit 1 / 57 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_colorsys.py` | ok / 42.6 ms | exit 1 / 23.9 ms | hydro_fail | AssertionError: 8 test(s) failed |
| `test_compare.py` | ok / 52.9 ms | exit 1 / 26.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_compile.py` | ok / 2353.9 ms | exit 1 / 47 ms | hydro_fail | error[HYD0002]: module '_ast' not found: not supported on the import search path |
| `test_compileall.py` | exit 1 / 65.1 ms | exit 1 / 181 ms | expected_fail | error[HYD0201]: module 'sys' has no attribute 'meta_path' |
| `test_compiler_assemble.py` | ok / 35.4 ms | exit 1 / 62.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_compiler_codegen.py` | ok / 37.6 ms | exit 1 / 30.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_complex.py` | ok / 64.9 ms | exit 1 / 28 ms | hydro_fail | error[HYD0002]: module 'cmath' not found: not supported on the import search path |
| `test_configparser.py` | ok / 395.9 ms | exit 1 / 71.2 ms | hydro_fail | error[HYD0201]: module 'io' has no attribute 'text_encoding' |
| `test_contains.py` | ok / 44.7 ms | exit 1 / 25.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_context.py` | ok / 538.3 ms | exit 1 / 30.9 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_contextlib.py` | ok / 69.3 ms | exit 1 / 31.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_contextlib_async.py` | ok / 70.2 ms | exit 1 / 32.6 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_copy.py` | ok / 74 ms | exit 1 / 38.9 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_copyreg.py` | ok / 79.2 ms | exit 1 / 65.8 ms | hydro_fail | error[HYD0002]: module 'faulthandler' not found: not supported on the import search path |
| `test_coroutines.py` | ok / 272.7 ms | exit 1 / 54.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_cprofile.py` | ok / 503.9 ms | exit 1 / 46.3 ms | hydro_fail | error[HYD0002]: module '_lsprof' not found: not supported on the import search path |
| `test_crossinterp.py` | exit 1 / 52 ms | exit 1 / 28.6 ms | expected_fail | error[HYD0002]: module '_interpreters' not found: not supported on the import search path |
| `test_csv.py` | ok / 240 ms | exit 1 / 42.2 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_curses.py` | ok / 285.4 ms | exit 1 / 46.7 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_datetime.py` | ok / 18371.2 ms | exit 1 / 28 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_dbm.py` | ok / 408.5 ms | exit 1 / 28.3 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'fsencode' |
| `test_dbm_dumb.py` | ok / 65.7 ms | exit 1 / 26.9 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'fsencode' |
| `test_dbm_gnu.py` | exit 1 / 47.3 ms | exit 1 / 28.2 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_dbm_ndbm.py` | ok / 104.3 ms | exit 1 / 25.3 ms | hydro_fail | error[HYD0002]: module '_dbm' not found: not supported on the import search path |
| `test_dbm_sqlite3.py` | ok / 537.3 ms | exit 1 / 26.5 ms | hydro_fail | error[HYD0002]: module '_sqlite3' not found: not supported on the import search path |
| `test_decimal.py` | ok / 1850.4 ms | exit 1 / 53.6 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_decorators.py` | ok / 48.5 ms | exit 1 / 55 ms | hydro_fail | AssertionError: 11 test(s) failed |
| `test_defaultdict.py` | ok / 46.3 ms | exit 1 / 19.6 ms | hydro_fail | error[HYD0103]: defaultdict() got an unexpected keyword argument 'foo' |
| `test_deque.py` | ok / 353.9 ms | exit 1 / 54.7 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_descr.py` | exit 1 / 293 ms | exit 1 / 63 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_descrtut.py` | ok / 140.9 ms | exit 1 / 52.2 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_devpoll.py` | exit 1 / 36.5 ms | exit 1 / 26.4 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_dict.py` | ok / 288.6 ms | exit 1 / 43.2 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_dictcomps.py` | ok / 46.1 ms | exit 1 / 26.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_dictviews.py` | ok / 133.4 ms | exit 1 / 39.6 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_difflib.py` | ok / 267.2 ms | exit 1 / 55.5 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_dis.py` | ok / 207.3 ms | exit 1 / 70.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_docxmlrpc.py` | ok / 4656.3 ms | exit 1 / 78.4 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_dtrace.py` | exit 1 / 53 ms | exit 1 / 33.5 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_dynamic.py` | ok / 46.2 ms | exit 1 / 25.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_dynamicclassattribute.py` | ok / 42.8 ms | exit 1 / 5.5 ms | hydro_fail | error[HYD0201]: module 'unittest' has no attribute 'skipIf' |
| `test_eintr.py` | exit 1 / 83.3 ms | exit 1 / 35.8 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_embed.py` | ok / 61.1 ms | exit 1 / 33.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_ensurepip.py` | ok / 149.2 ms | exit 1 / 42.6 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_enum.py` | exit 1 / 539.1 ms | exit 1 / 85.8 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_enumerate.py` | ok / 72.2 ms | exit 1 / 26.9 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_eof.py` | ok / 288.8 ms | exit 1 / 43.2 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_epoll.py` | ok / 153.2 ms | exit 1 / 25.7 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_errno.py` | ok / 45.7 ms | ok / 8.3 ms | hydro_pass | - |
| `test_except_star.py` | ok / 57.7 ms | exit 1 / 30.6 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_exception_group.py` | ok / 258.1 ms | exit 1 / 29 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_exception_hierarchy.py` | ok / 43.2 ms | exit 1 / 5 ms | hydro_fail | error[HYD0201]: module 'errno' has no attribute 'EEXIST' |
| `test_exception_variations.py` | ok / 43.6 ms | exit 1 / 35.7 ms | hydro_fail | AssertionError: 8 test(s) failed |
| `test_exceptions.py` | exit 1 / 895.9 ms | exit 1 / 54.8 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_extcall.py` | exit 1 / 133.8 ms | exit 1 / 40.7 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_external_inspection.py` | ok / 1343.4 ms | exit 1 / 54.9 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_faulthandler.py` | ok / 18621.7 ms | exit 1 / 40 ms | hydro_fail | error[HYD0002]: module 'faulthandler' not found: not supported on the import search path |
| `test_fcntl.py` | ok / 193.2 ms | exit 1 / 28.7 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_file.py` | ok / 81.3 ms | exit 1 / 30 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_file_eintr.py` | ok / 1397.6 ms | exit 1 / 31 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_filecmp.py` | ok / 61.6 ms | exit 1 / 28.6 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'curdir' |
| `test_fileinput.py` | ok / 126.4 ms | exit 1 / 35.7 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'flags' |
| `test_fileio.py` | ok / 666.3 ms | exit 1 / 42.2 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_fileutils.py` | ok / 124.5 ms | exit 1 / 29 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_finalization.py` | ok / 105.7 ms | exit 1 / 35.3 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_float.py` | ok / 83.1 ms | exit 1 / 30.2 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_flufl.py` | ok / 40.9 ms | exit 1 / 2.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'CO_FUTURE_BARRY_AS_BDFL' |
| `test_fnmatch.py` | ok / 43.3 ms | exit 1 / 13.7 ms | hydro_fail | error[HYD0201]: module 'fnmatch' has no attribute '_translate' |
| `test_fork1.py` | ok / 5130.8 ms | exit 1 / 29.5 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_format.py` | ok / 56.7 ms | exit 1 / 30.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_fractions.py` | ok / 70 ms | exit 1 / 32.6 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_frame.py` | exit 1 / 967.5 ms | exit 1 / 52.8 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_frozen.py` | ok / 47.4 ms | exit 1 / 27.5 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_fstring.py` | exit 1 / 380.1 ms | exit 1 / 4.3 ms | expected_fail | error[HYD0001]: invalid syntax: f-string: expressions nested too deeply |
| `test_ftplib.py` | ok / 6867.9 ms | exit 1 / 49.6 ms | hydro_fail | error[HYD0201]: module 'socket' has no attribute '_GLOBAL_DEFAULT_TIMEOUT' |
| `test_funcattrs.py` | ok / 50 ms | exit 1 / 26.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_functools.py` | ok / 357.8 ms | exit 1 / 56.5 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_gc.py` | exit 1 / 1493.1 ms | exit 1 / 50.1 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_generated_cases.py` | ok / 123.4 ms | exit 1 / 29.5 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_generator_stop.py` | ok / 40.3 ms | exit 1 / 8.3 ms | hydro_fail | AssertionError: 2 test(s) failed |
| `test_generators.py` | ok / 179.2 ms | exit 1 / 55.2 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_genericalias.py` | ok / 160.2 ms | exit 1 / 92.3 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_genericclass.py` | ok / 47.5 ms | exit 1 / 29.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_genericpath.py` | ok / 53.9 ms | exit 1 / 39.5 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'lstat' |
| `test_genexps.py` | ok / 130.3 ms | exit 1 / 39.7 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_getopt.py` | ok / 159.2 ms | exit 1 / 54.8 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_getpass.py` | ok / 123.3 ms | exit 1 / 26.9 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'O_NOCTTY' |
| `test_getpath.py` | ok / 47.5 ms | exit 1 / 41.9 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'lstat' |
| `test_gettext.py` | exit 1 / 149.7 ms | exit 1 / 49.1 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_glob.py` | ok / 105.4 ms | exit 1 / 35.2 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'audit' |
| `test_global.py` | ok / 46.8 ms | exit 1 / 25.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_grammar.py` | ok / 70.5 ms | exit 1 / 5.2 ms | hydro_fail | error[HYD0001]: invalid syntax: invalid syntax. Got unexpected token ',' |
| `test_graphlib.py` | ok / 82.6 ms | exit 1 / 39.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_grp.py` | ok / 243 ms | exit 1 / 33.2 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_gzip.py` | ok / 821.4 ms | exit 1 / 40.5 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_hash.py` | ok / 279.1 ms | exit 1 / 40.6 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_hashlib.py` | ok / 13295.6 ms | exit 1 / 58.8 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_heapq.py` | ok / 236.1 ms | exit 1 / 54.3 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_hmac.py` | ok / 162.6 ms | exit 1 / 45.1 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_html.py` | ok / 45.3 ms | exit 1 / 13.7 ms | hydro_fail | AssertionError: 1 test(s) failed |
| `test_htmlparser.py` | ok / 428.5 ms | exit 1 / 48.6 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_http_cookiejar.py` | ok / 307.8 ms | exit 1 / 66 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_http_cookies.py` | ok / 576.8 ms | exit 1 / 58.3 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_httplib.py` | ok / 3490.9 ms | exit 1 / 55.6 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_httpservers.py` | ok / 4144 ms | exit 1 / 266 ms | hydro_fail | error[HYD0201]: module 'math' has no attribute 'isqrt' |
| `test_idle.py` | ok / 9570.5 ms | exit 1 / 29.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_imaplib.py` | timeout / 30003.2 ms | exit 1 / 47.7 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_index.py` | ok / 46.2 ms | exit 1 / 26.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_int.py` | ok / 21089.6 ms | exit 1 / 28.9 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_int_literal.py` | ok / 40.4 ms | ok / 14.9 ms | hydro_pass | - |
| `test_io.py` | exit 1 / 27815.9 ms | exit 1 / 52.5 ms | expected_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_ioctl.py` | ok / 550.8 ms | exit 1 / 25.6 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_ipaddress.py` | ok / 90.1 ms | exit 1 / 115.3 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_isinstance.py` | ok / 775.3 ms | exit 1 / 25.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_iter.py` | ok / 57.3 ms | exit 1 / 29.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_iterlen.py` | ok / 40.4 ms | exit 1 / 4.7 ms | hydro_fail | error[HYD0201]: module 'operator' has no attribute 'length_hint' |
| `test_itertools.py` | ok / 777.4 ms | exit 1 / 69.2 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_keyword.py` | ok / 42.5 ms | exit 1 / 15.4 ms | hydro_fail | AssertionError: 6 test(s) failed |
| `test_keywordonlyarg.py` | ok / 46.7 ms | exit 1 / 5.6 ms | hydro_fail | error[HYD0103]: keywordonly_sum() got an unexpected keyword argument 'non_existing_arg' |
| `test_kqueue.py` | exit 1 / 40.9 ms | exit 1 / 26.5 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_largefile.py` | ok / 1245 ms | exit 1 / 35.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_launcher.py` | exit 1 / 60 ms | exit 1 / 23.6 ms | expected_fail | error[HYD0002]: module 'winreg' not found: not supported on the import search path |
| `test_linecache.py` | ok / 107.1 ms | exit 1 / 53.7 ms | hydro_fail | error[HYD0002]: module '_frozen_importlib_external' not found: not supported on the import search path |
| `test_list.py` | ok / 364.3 ms | exit 1 / 40.6 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_listcomps.py` | ok / 149.3 ms | exit 1 / 57.1 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_lltrace.py` | ok / 52.3 ms | exit 1 / 79 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_locale.py` | ok / 137.7 ms | exit 1 / 30.3 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_logging.py` | ok / 21931.4 ms | exit 1 / 106.2 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_long.py` | ok / 474.9 ms | exit 1 / 27.6 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_longexp.py` | ok / 102.5 ms | ok / 6.3 ms | hydro_pass | - |
| `test_lzma.py` | ok / 178.5 ms | exit 1 / 31.7 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_mailbox.py` | ok / 1262.2 ms | exit 1 / 154.2 ms | hydro_fail | error[HYD0201]: module 'math' has no attribute 'isqrt' |
| `test_marshal.py` | ok / 211.6 ms | exit 1 / 42.1 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_math.py` | ok / 9072.1 ms | exit 1 / 35.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_math_property.py` | ok / 38.1 ms | exit 1 / 32.6 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_memoryio.py` | ok / 69.6 ms | exit 1 / 34.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_memoryview.py` | ok / 307.8 ms | exit 1 / 29.8 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_metaclass.py` | ok / 131.7 ms | exit 1 / 41.1 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_mimetypes.py` | ok / 418 ms | exit 1 / 49.8 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_minidom.py` | ok / 93.7 ms | exit 1 / 34.4 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_mmap.py` | ok / 109.3 ms | exit 1 / 43.2 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_modulefinder.py` | ok / 278.3 ms | exit 1 / 175.5 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'meta_path' |
| `test_monitoring.py` | ok / 61.3 ms | exit 1 / 80 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_msvcrt.py` | exit 1 / 32.1 ms | exit 1 / 23.3 ms | expected_fail | error[HYD0002]: module '_winapi' not found: not supported on the import search path |
| `test_multibytecodec.py` | ok / 223.4 ms | exit 1 / 25.7 ms | hydro_fail | error[HYD0002]: module '_multibytecodec' not found: not supported on the import search path |
| `test_multiprocessing_main_handling.py` | exit 1 / 1273.2 ms | exit 1 / 41 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_named_expressions.py` | ok / 51.3 ms | exit 1 / 28.4 ms | hydro_fail | error[HYD0002]: assignment expressions inside a generator expression are not supported |
| `test_netrc.py` | ok / 51.2 ms | exit 1 / 27.1 ms | hydro_fail | error[HYD0201]: module 'os.path' has no attribute 'expanduser' |
| `test_ntpath.py` | ok / 274.7 ms | exit 1 / 47.3 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'lstat' |
| `test_nturl2path.py` | ok / 60.5 ms | exit 1 / 126.5 ms | hydro_fail | error[HYD0201]: module 'math' has no attribute 'isqrt' |
| `test_numeric_tower.py` | ok / 67.2 ms | exit 1 / 4.9 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'hash_info' |
| `test_opcache.py` | ok / 277 ms | exit 1 / 82.3 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_opcodes.py` | ok / 46.3 ms | exit 1 / 24.5 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_openpty.py` | ok / 36.3 ms | exit 1 / 2 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'openpty' |
| `test_operator.py` | ok / 61.2 ms | exit 1 / 37.6 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_optimizer.py` | ok / 41.8 ms | exit 1 / 28.9 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_optparse.py` | ok / 127.4 ms | exit 1 / 50 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_ordered_dict.py` | ok / 155.6 ms | exit 1 / 41.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_os.py` | exit 1 / 12655.8 ms | exit 1 / 55.8 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_osx_env.py` | ok / 44.8 ms | exit 1 / 27.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_patma.py` | exit 1 / 49.1 ms | exit 1 / 40.4 ms | expected_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_pdb.py` | exit 1 / 17455 ms | exit 1 / 105.6 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_peepholer.py` | ok / 343 ms | exit 1 / 68.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_pep646_syntax.py` | ok / 222.7 ms | exit 1 / 41.4 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_perf_profiler.py` | ok / 401 ms | exit 1 / 42.9 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_perfmaps.py` | ok / 95.5 ms | exit 1 / 6.5 ms | hydro_fail | ModuleNotFoundError: No module named '_testinternalcapi' |
| `test_pickle.py` | ok / 2082.2 ms | exit 1 / 96.2 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_picklebuffer.py` | ok / 123.6 ms | exit 1 / 36.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_pickletools.py` | ok / 545 ms | exit 1 / 86.5 ms | hydro_fail | error[HYD0002]: module 'faulthandler' not found: not supported on the import search path |
| `test_pkg.py` | ok / 121.1 ms | exit 1 / 6.3 ms | hydro_fail | error[HYD0201]: module 'os.path' has no attribute 'islink' |
| `test_pkgutil.py` | exit 1 / 138.3 ms | exit 1 / 47.2 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_platform.py` | ok / 427.7 ms | exit 1 / 41.5 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_plistlib.py` | ok / 231.3 ms | exit 1 / 33.7 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_poll.py` | ok / 11257.1 ms | exit 1 / 28.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_popen.py` | ok / 158.1 ms | exit 1 / 24.6 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_poplib.py` | ok / 5917.2 ms | exit 1 / 38.8 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'audit' |
| `test_positional_only_arg.py` | ok / 132.8 ms | exit 1 / 65.6 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_posix.py` | exit 1 / 839.7 ms | exit 1 / 46.2 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_posixpath.py` | ok / 332.8 ms | exit 1 / 50 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'lstat' |
| `test_pow.py` | ok / 244.7 ms | exit 1 / 5 ms | hydro_fail | error[HYD0101]: name 'pow' is not defined |
| `test_pprint.py` | ok / 162.9 ms | exit 1 / 42.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'is_dataclass' |
| `test_print.py` | ok / 125 ms | exit 1 / 26.7 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_profile.py` | ok / 288 ms | exit 1 / 147.5 ms | hydro_fail | error[HYD0201]: module 'functools' has no attribute 'cmp_to_key' |
| `test_property.py` | ok / 89.2 ms | exit 1 / 26.3 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_pstats.py` | ok / 158.5 ms | exit 1 / 32.7 ms | hydro_fail | error[HYD0002]: module '_lsprof' not found: not supported on the import search path |
| `test_pty.py` | ok / 107.5 ms | exit 1 / 30.4 ms | hydro_fail | error[HYD0002]: module 'termios' not found: not supported on the import search path |
| `test_pulldom.py` | ok / 161.1 ms | exit 1 / 153 ms | hydro_fail | error[HYD0201]: module 'math' has no attribute 'isqrt' |
| `test_pwd.py` | ok / 98.7 ms | exit 1 / 31.2 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_py_compile.py` | ok / 330.1 ms | exit 1 / 180.9 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'meta_path' |
| `test_pyclbr.py` | exit 1 / 97.2 ms | exit 1 / 31.2 ms | expected_fail | error[HYD0201]: module 'importlib.util' has no attribute '_find_spec_from_path' |
| `test_pyexpat.py` | ok / 278.1 ms | exit 1 / 29.2 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_queue.py` | ok / 7904.4 ms | exit 1 / 38.5 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_quopri.py` | ok / 131.4 ms | exit 1 / 25 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_raise.py` | ok / 85.1 ms | exit 1 / 26.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_random.py` | ok / 748.5 ms | exit 1 / 51.9 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_range.py` | ok / 202.4 ms | exit 1 / 36.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_re.py` | ok / 432.4 ms | exit 1 / 38 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_readline.py` | ok / 241.7 ms | exit 1 / 39.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_regrtest.py` | exit 1 / 1411.1 ms | exit 1 / 80 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_remote_pdb.py` | ok / 2377.2 ms | exit 1 / 60.9 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_repl.py` | ok / 927.2 ms | exit 1 / 40.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_reprlib.py` | ok / 77.9 ms | exit 1 / 32.3 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_resource.py` | ok / 97.6 ms | exit 1 / 31.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_richcmp.py` | ok / 42.4 ms | exit 1 / 25.9 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_rlcompleter.py` | ok / 108.7 ms | exit 1 / 40.4 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_robotparser.py` | ok / 1413.4 ms | exit 1 / 58.5 ms | hydro_fail | error[HYD0201]: module 'tempfile' has no attribute '_TemporaryFileWrapper' |
| `test_runpy.py` | timeout / 30010.9 ms | exit 1 / 70.3 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_sax.py` | ok / 189.3 ms | exit 1 / 60.2 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_sched.py` | ok / 584.8 ms | exit 1 / 27.6 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_scope.py` | ok / 48.2 ms | exit 1 / 35.9 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_script_helper.py` | ok / 196.6 ms | exit 1 / 43.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_secrets.py` | ok / 38 ms | exit 1 / 5.6 ms | hydro_fail | error[HYD0201]: module '_operator' has no attribute '_compare_digest' |
| `test_select.py` | ok / 1554.5 ms | exit 1 / 26 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_selectors.py` | ok / 14649.3 ms | exit 1 / 42.6 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_set.py` | ok / 734.8 ms | exit 1 / 44 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_setcomps.py` | ok / 115.1 ms | exit 1 / 52 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_shelve.py` | ok / 2487.4 ms | exit 1 / 31.4 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'fsencode' |
| `test_shlex.py` | ok / 148.8 ms | exit 1 / 45.5 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_shutil.py` | exit 1 / 454.7 ms | exit 1 / 66.8 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_signal.py` | exit 1 / 29584.1 ms | exit 1 / 38.8 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_site.py` | ok / 2580.6 ms | exit 1 / 67.7 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_slice.py` | ok / 90.9 ms | exit 1 / 39.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_smtplib.py` | ok / 1478.4 ms | exit 1 / 78.7 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_smtpnet.py` | ok / 9856.6 ms | exit 1 / 46.9 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_socket.py` | timeout / 30020.9 ms | exit 1 / 47.8 ms | expected_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_socketserver.py` | ok / 480.3 ms | exit 1 / 33.8 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_sort.py` | ok / 70.7 ms | exit 1 / 25.6 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_source_encoding.py` | ok / 12426 ms | exit 1 / 40.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_ssl.py` | ok / 19404 ms | exit 1 / 86.9 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_stable_abi_ctypes.py` | exit 1 / 117.6 ms | exit 1 / 27.9 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_startfile.py` | exit 1 / 110.8 ms | exit 1 / 26.4 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_stat.py` | ok / 142.1 ms | exit 1 / 28.3 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_statistics.py` | ok / 9524.7 ms | exit 1 / 62.4 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_str.py` | ok / 1114.9 ms | exit 1 / 52.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_strftime.py` | ok / 153.7 ms | exit 1 / 140.9 ms | hydro_fail | error[HYD0101]: name 'FEBRUARY' is not defined |
| `test_string_literals.py` | ok / 123.6 ms | exit 1 / 58 ms | hydro_fail | AssertionError: 19 test(s) failed |
| `test_stringprep.py` | ok / 109.1 ms | exit 1 / 28.6 ms | hydro_fail | error[HYD0201]: module 'unicodedata' has no attribute 'ucd_3_2_0' |
| `test_strptime.py` | ok / 150 ms | exit 1 / 35.2 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_strtod.py` | ok / 226.2 ms | exit 1 / 27 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_struct.py` | ok / 374.6 ms | exit 1 / 38.1 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_structseq.py` | ok / 155.2 ms | exit 1 / 52.9 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_subclassinit.py` | ok / 115.7 ms | exit 1 / 5.5 ms | hydro_fail | error[HYD0002]: class keyword 'otherarg' cannot be forwarded together with metaclass=; Hydro's metaclass hook… |
| `test_subprocess.py` | exit 1 / 15970.8 ms | exit 1 / 53.6 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_sundry.py` | ok / 104.2 ms | exit 1 / 28.7 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_super.py` | ok / 211.4 ms | exit 1 / 45.2 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_support.py` | exit 1 / 4743.8 ms | exit 1 / 39.6 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_symtable.py` | ok / 58.9 ms | exit 1 / 30.6 ms | hydro_fail | error[HYD0002]: module '_symtable' not found: not supported on the import search path |
| `test_syntax.py` | ok / 176.3 ms | exit 1 / 52.9 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_sys.py` | exit 1 / 1189.1 ms | exit 1 / 53.1 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_sys_setprofile.py` | ok / 43 ms | exit 1 / 22 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'is_dataclass' |
| `test_sys_settrace.py` | ok / 776.6 ms | exit 1 / 36.5 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_sysconfig.py` | exit 1 / 191.2 ms | exit 1 / 32.9 ms | expected_fail | error[HYD0002]: module '_sysconfig' not found: not supported on the import search path |
| `test_syslog.py` | ok / 177.3 ms | exit 1 / 32 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_tabnanny.py` | ok / 116.5 ms | exit 1 / 49 ms | hydro_fail | error[HYD0201]: module 'os.path' has no attribute 'islink' |
| `test_tarfile.py` | exit 1 / 5316.1 ms | exit 1 / 76.2 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_tcl.py` | ok / 109.3 ms | exit 1 / 40.9 ms | hydro_fail | error[HYD0002]: module '_tkinter' not found: not supported on the import search path |
| `test_tempfile.py` | ok / 379.6 ms | exit 1 / 50.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_termios.py` | ok / 552.5 ms | exit 1 / 29.2 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_textwrap.py` | ok / 55.4 ms | exit 1 / 36.9 ms | hydro_fail | error[HYD0201]: module 'textwrap' has no attribute 'TextWrapper' |
| `test_thread.py` | ok / 1273.7 ms | exit 1 / 32.5 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_thread_local_bytecode.py` | ok / 52.9 ms | exit 1 / 39.2 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_threadedtempfile.py` | ok / 51.1 ms | exit 1 / 26.3 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_threading.py` | exit 1 / 13972.4 ms | exit 1 / 46.1 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_threading_local.py` | ok / 304.4 ms | exit 1 / 55.8 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_threadsignals.py` | ok / 4892.2 ms | exit 1 / 28.7 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_time.py` | ok / 3077.9 ms | exit 1 / 33.2 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_timeit.py` | ok / 96.3 ms | exit 1 / 28.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_timeout.py` | ok / 9971.5 ms | exit 1 / 26.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_tokenize.py` | ok / 6217.1 ms | exit 1 / 52.9 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_trace.py` | ok / 920.9 ms | exit 1 / 46.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_traceback.py` | exit 1 / 2105.5 ms | exit 1 / 60.4 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_tracemalloc.py` | ok / 640.5 ms | exit 1 / 51.3 ms | hydro_fail | error[HYD0002]: module '_tracemalloc' not found: not supported on the import search path |
| `test_tstring.py` | exit 1 / 275.7 ms | exit 1 / 39.7 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_ttk_textonly.py` | ok / 72.8 ms | exit 1 / 38.6 ms | hydro_fail | error[HYD0002]: module '_tkinter' not found: not supported on the import search path |
| `test_tty.py` | ok / 47.4 ms | exit 1 / 29.2 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_tuple.py` | ok / 223.2 ms | exit 1 / 28.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_turtle.py` | ok / 199.9 ms | exit 1 / 45.9 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_type_aliases.py` | ok / 53.4 ms | exit 1 / 27.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_type_annotations.py` | ok / 48.4 ms | exit 1 / 30.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_type_cache.py` | exit 1 / 46.8 ms | exit 1 / 64.4 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_type_comments.py` | ok / 50.7 ms | exit 1 / 29.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_type_params.py` | ok / 54.3 ms | exit 1 / 6.2 ms | hydro_fail | error[HYD0001]: invalid syntax: invalid syntax. Got unexpected token '=' |
| `test_typechecks.py` | ok / 40.2 ms | exit 1 / 17.2 ms | hydro_fail | AssertionError: 4 test(s) failed |
| `test_types.py` | ok / 236.5 ms | exit 1 / 54.4 ms | hydro_fail | error[HYD0002]: module '_datetime' not found: not supported on the import search path |
| `test_typing.py` | ok / 379.8 ms | exit 1 / 69.2 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_ucn.py` | ok / 133.7 ms | exit 1 / 46.5 ms | hydro_fail | error[HYD0201]: module 'tempfile' has no attribute '_TemporaryFileWrapper' |
| `test_unary.py` | ok / 40.9 ms | exit 1 / 10.7 ms | hydro_fail | AssertionError: 2 test(s) failed |
| `test_unicode_file.py` | ok / 46.2 ms | exit 1 / 34.6 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'audit' |
| `test_unicode_file_functions.py` | ok / 67.8 ms | exit 1 / 26 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_unicode_identifiers.py` | ok / 43.5 ms | exit 1 / 6.5 ms | hydro_fail | AssertionError: 3 test(s) failed |
| `test_unicodedata.py` | ok / 4260.3 ms | exit 1 / 64.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_univnewlines.py` | ok / 58.1 ms | exit 1 / 144.5 ms | hydro_fail | error[HYD0201]: module 'io' has no attribute '__all__' |
| `test_unpack.py` | exit 1 / 141.5 ms | exit 1 / 42 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_unpack_ex.py` | exit 1 / 144.4 ms | exit 1 / 41.2 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_unparse.py` | ok / 5920.7 ms | exit 1 / 27.1 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_urllib.py` | ok / 333.4 ms | exit 1 / 150 ms | hydro_fail | error[HYD0201]: module 'math' has no attribute 'isqrt' |
| `test_urllib2.py` | ok / 2011 ms | exit 1 / 67.4 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_urllib2_localnet.py` | ok / 2267.1 ms | exit 1 / 165.2 ms | hydro_fail | error[HYD0201]: module 'math' has no attribute 'isqrt' |
| `test_urllib2net.py` | ok / 29698.9 ms | exit 1 / 56.7 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_urllib_response.py` | ok / 50.7 ms | exit 1 / 23.6 ms | hydro_fail | error[HYD0201]: module 'tempfile' has no attribute '_TemporaryFileWrapper' |
| `test_urllibnet.py` | ok / 6828.2 ms | exit 1 / 54.7 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_urlparse.py` | ok / 122 ms | exit 1 / 127.3 ms | hydro_fail | error[HYD0201]: module 'math' has no attribute 'isqrt' |
| `test_userdict.py` | ok / 125 ms | exit 1 / 28.5 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_userlist.py` | ok / 136.8 ms | exit 1 / 29.9 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_userstring.py` | ok / 543.1 ms | exit 1 / 36.9 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_utf8_mode.py` | ok / 397.2 ms | exit 1 / 40.6 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_utf8source.py` | ok / 43 ms | exit 1 / 7.2 ms | hydro_fail | AssertionError: 2 test(s) failed |
| `test_uuid.py` | ok / 255 ms | exit 1 / 150.5 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_venv.py` | exit 1 / 6879 ms | exit 1 / 51.1 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_wait3.py` | ok / 5062.5 ms | exit 1 / 26.9 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_wait4.py` | ok / 5055.5 ms | exit 1 / 26.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_wave.py` | ok / 67.3 ms | exit 1 / 25.9 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_weakref.py` | exit 1 / 2402.9 ms | exit 1 / 68.1 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_weakset.py` | ok / 62 ms | exit 1 / 40.2 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_webbrowser.py` | ok / 2067.2 ms | exit 1 / 45.4 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'audit' |
| `test_winapi.py` | exit 1 / 45.2 ms | exit 1 / 36.7 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_winconsoleio.py` | exit 1 / 47.8 ms | exit 1 / 24.6 ms | expected_fail | error[HYD0002]: module '_testconsole' not found: not supported on the import search path |
| `test_winreg.py` | exit 1 / 45.3 ms | exit 1 / 25.7 ms | expected_fail | error[HYD0002]: module 'winreg' not found: not supported on the import search path |
| `test_winsound.py` | exit 1 / 46.8 ms | exit 1 / 28 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_with.py` | ok / 42.5 ms | exit 1 / 13.7 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_wmi.py` | exit 1 / 46.3 ms | exit 1 / 28.3 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_wsgiref.py` | ok / 237.9 ms | exit 1 / 79.5 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_xml_dom_minicompat.py` | ok / 40.8 ms | exit 1 / 17.9 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'flags' |
| `test_xml_dom_xmlbuilder.py` | ok / 167.6 ms | exit 1 / 59.3 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_xml_etree.py` | ok / 813.5 ms | exit 1 / 53.5 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_xml_etree_c.py` | ok / 543.7 ms | exit 1 / 28.6 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_xmlrpc.py` | ok / 27447.6 ms | exit 1 / 90.1 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_xpickle.py` | ok / 2020.9 ms | exit 1 / 79.8 ms | hydro_fail | error[HYD0002]: module 'faulthandler' not found: not supported on the import search path |
| `test_xxlimited.py` | exit 1 / 40.6 ms | exit 1 / 28.2 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_xxtestfuzz.py` | ok / 53.8 ms | exit 1 / 24.8 ms | hydro_fail | error[HYD0002]: module 'faulthandler' not found: not supported on the import search path |
| `test_yield_from.py` | ok / 54.6 ms | exit 1 / 28.8 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_zipapp.py` | ok / 138.5 ms | exit 1 / 53.7 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_zipfile64.py` | timeout / 30030.1 ms | exit 1 / 39.9 ms | expected_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_zipimport.py` | ok / 18218.4 ms | exit 1 / 81.7 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_zipimport_support.py` | ok / 480.9 ms | exit 1 / 74.8 ms | hydro_fail | error[HYD0002]: module '_frozen_importlib_external' not found: not supported on the import search path |
| `test_zlib.py` | ok / 391.8 ms | exit 1 / 32.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'all_feature_names' |
| `test_zstd.py` | ok / 359.1 ms | exit 1 / 40.3 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |

<!-- END GENERATED RESULTS -->
