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

Source: `/mnt/Vault/GitHub/hydro/.hydro/cpython/Python-3.14.9` · generated 2026-10-29T12:42:29Z

## Summary

| Status | Tests |
| --- | ---: |
| hydro_pass | 3 |
| hydro_fail | 333 |
| cpython_fail | 0 |
| expected_fail | 62 |
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
| hydro_fail | 333 |
| expected_fail | 62 |

## Test results

| Test | CPython 3.14.9 | Hydro | Status | First error |
| --- | --- | --- | --- | --- |
| `test___all__.py` | ok / 1902.1 ms | exit 1 / 38.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test__colorize.py` | ok / 109.9 ms | exit 1 / 40.1 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test__interpchannels.py` | ok / 770.5 ms | exit 1 / 45.3 ms | hydro_fail | error[HYD0002]: module '_interpreters' not found: not supported on the import search path |
| `test__interpreters.py` | ok / 646.7 ms | exit 1 / 42.8 ms | hydro_fail | error[HYD0002]: module '_interpreters' not found: not supported on the import search path |
| `test__locale.py` | ok / 57.8 ms | exit 1 / 23.5 ms | hydro_fail | error[HYD0002]: module '_locale' not found: not supported on the import search path |
| `test__opcode.py` | ok / 48 ms | exit 1 / 74.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test__osx_support.py` | ok / 51.2 ms | exit 1 / 39.8 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_abc.py` | ok / 47.4 ms | exit 1 / 8.5 ms | hydro_fail | error[HYD0002]: pytest fixture 'abc_ABCMeta' is not defined |
| `test_abstract_numbers.py` | ok / 43.3 ms | exit 1 / 54.1 ms | hydro_fail | AssertionError: 7 test(s) failed |
| `test_android.py` | exit 1 / 108 ms | exit 1 / 39.8 ms | expected_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_annotationlib.py` | ok / 67.3 ms | exit 1 / 45.3 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_apple.py` | exit 1 / 100.7 ms | exit 1 / 38.7 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_argparse.py` | ok / 1290.7 ms | exit 1 / 55.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_array.py` | ok / 289.2 ms | exit 1 / 41.6 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_asdl_parser.py` | exit 1 / 31.9 ms | exit 1 / 3.1 ms | expected_fail | error[HYD0201]: module 'importlib.util' has no attribute 'spec_from_loader' |
| `test_asyncgen.py` | ok / 833.8 ms | exit 1 / 41.8 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_atexit.py` | exit 1 / 109.8 ms | exit 1 / 50.4 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_audit.py` | ok / 1634.1 ms | exit 1 / 39.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_augassign.py` | ok / 41.1 ms | exit 1 / 40.6 ms | hydro_fail | AssertionError: 3 test(s) failed |
| `test_base64.py` | ok / 129.8 ms | exit 1 / 40.9 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_baseexception.py` | ok / 53.2 ms | exit 1 / 32.5 ms | hydro_fail | AssertionError: 11 test(s) failed |
| `test_bdb.py` | exit 1 / 71.4 ms | exit 1 / 61.8 ms | expected_fail | error[HYD0201]: module 'sys' has no attribute 'monitoring' |
| `test_bigaddrspace.py` | ok / 46.1 ms | exit 1 / 36.5 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_bigmem.py` | ok / 60 ms | exit 1 / 45.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_binascii.py` | ok / 55 ms | exit 1 / 34.1 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_binop.py` | ok / 45.9 ms | exit -6 / 231.1 ms | hydro_fail | fatal runtime error: stack overflow, aborting |
| `test_bisect.py` | ok / 59.1 ms | exit 1 / 41.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_bool.py` | ok / 47.8 ms | exit 1 / 39 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_buffer.py` | ok / 81.5 ms | exit 1 / 39.6 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_bufio.py` | ok / 136.4 ms | exit 1 / 40.4 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_build_details.py` | exit 1 / 119.8 ms | exit 1 / 40.2 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_builtin.py` | ok / 359.7 ms | exit 1 / 57 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_bytes.py` | exit 1 / 86.3 ms | exit 1 / 53.1 ms | expected_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_bz2.py` | ok / 367 ms | exit 1 / 31.7 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_c_locale_coercion.py` | ok / 789.3 ms | exit 1 / 52.3 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_calendar.py` | ok / 1322.5 ms | exit 1 / 148.6 ms | hydro_fail | error[HYD0101]: name 'FEBRUARY' is not defined |
| `test_call.py` | exit 1 / 58.3 ms | exit 1 / 40.2 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_charmapcodec.py` | ok / 41.1 ms | exit 1 / 2.6 ms | hydro_fail | error[HYD0201]: module 'codecs' has no attribute 'register' |
| `test_class.py` | ok / 58.9 ms | exit 1 / 27.7 ms | hydro_fail | error[HYD0002]: module '_testinternalcapi' not found: not supported on the import search path |
| `test_clinic.py` | ok / 212.3 ms | exit 1 / 51.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_cmath.py` | ok / 85.2 ms | exit 1 / 36.3 ms | hydro_fail | error[HYD0002]: module 'cmath' not found: not supported on the import search path |
| `test_cmd.py` | ok / 192.7 ms | exit 1 / 54.8 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_cmd_line.py` | exit 1 / 1719.4 ms | exit 1 / 53.4 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_cmd_line_script.py` | exit 1 / 1305.2 ms | exit 1 / 39.9 ms | expected_fail | error[HYD0002]: module '_frozen_importlib_external' not found: not supported on the import search path |
| `test_code.py` | ok / 200.4 ms | exit 1 / 63.9 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_code_module.py` | ok / 131.6 ms | exit 1 / 38.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_codeccallbacks.py` | ok / 58.2 ms | exit 1 / 144.3 ms | hydro_fail | error[HYD0201]: module '_codecs' has no attribute '_unregister_error' |
| `test_codecencodings_cn.py` | ok / 138.3 ms | exit 1 / 151.5 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecencodings_hk.py` | ok / 94.6 ms | exit 1 / 143.4 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecencodings_iso2022.py` | ok / 135.2 ms | exit 1 / 146 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecencodings_jp.py` | ok / 158.1 ms | exit 1 / 144.9 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecencodings_kr.py` | ok / 108.6 ms | exit 1 / 144.2 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecencodings_tw.py` | ok / 97 ms | exit 1 / 143 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecmaps_cn.py` | ok / 167.9 ms | exit 1 / 142.7 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecmaps_hk.py` | ok / 123.9 ms | exit 1 / 148.9 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecmaps_jp.py` | ok / 168.5 ms | exit 1 / 145.6 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecmaps_kr.py` | ok / 160.9 ms | exit 1 / 144.7 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecmaps_tw.py` | ok / 139 ms | exit 1 / 151.9 ms | hydro_fail | error[HYD0103]: sub() does not accept keyword arguments |
| `test_codecs.py` | ok / 337.4 ms | exit 1 / 48.1 ms | hydro_fail | error[HYD0201]: module 'codecs' has no attribute 'code_page_encode' |
| `test_codeop.py` | ok / 52.7 ms | exit 1 / 37.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_collections.py` | ok / 1091.4 ms | exit 1 / 58.4 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_colorsys.py` | ok / 41.3 ms | exit 1 / 24.7 ms | hydro_fail | AssertionError: 8 test(s) failed |
| `test_compare.py` | ok / 60.1 ms | exit 1 / 39.4 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_compile.py` | ok / 2224.3 ms | exit 1 / 48.6 ms | hydro_fail | error[HYD0002]: module '_ast' not found: not supported on the import search path |
| `test_compileall.py` | exit 1 / 63.3 ms | exit 1 / 178.8 ms | expected_fail | error[HYD0201]: module 'sys' has no attribute 'meta_path' |
| `test_compiler_assemble.py` | ok / 35 ms | exit 1 / 72.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_compiler_codegen.py` | ok / 36.7 ms | exit 1 / 39.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_complex.py` | ok / 62.7 ms | exit 1 / 28.2 ms | hydro_fail | error[HYD0002]: module 'cmath' not found: not supported on the import search path |
| `test_configparser.py` | ok / 394.9 ms | exit 1 / 72 ms | hydro_fail | error[HYD0201]: module 'io' has no attribute 'text_encoding' |
| `test_contains.py` | ok / 45.3 ms | exit 1 / 36.6 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_context.py` | ok / 519.9 ms | exit 1 / 40 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_contextlib.py` | ok / 67.4 ms | exit 1 / 40.6 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_contextlib_async.py` | ok / 70.7 ms | exit 1 / 44.4 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_copy.py` | ok / 77.8 ms | exit 1 / 51.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_copyreg.py` | ok / 90 ms | exit 1 / 68.1 ms | hydro_fail | error[HYD0002]: module 'faulthandler' not found: not supported on the import search path |
| `test_coroutines.py` | ok / 261.2 ms | exit 1 / 63.6 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_cprofile.py` | ok / 488.3 ms | exit 1 / 43.7 ms | hydro_fail | error[HYD0002]: module '_lsprof' not found: not supported on the import search path |
| `test_crossinterp.py` | exit 1 / 52.1 ms | exit 1 / 27.1 ms | expected_fail | error[HYD0002]: module '_interpreters' not found: not supported on the import search path |
| `test_csv.py` | ok / 227.4 ms | exit 1 / 52.7 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_curses.py` | ok / 279.9 ms | exit 1 / 47 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_datetime.py` | ok / 18593.9 ms | exit 1 / 38.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_dbm.py` | ok / 384.8 ms | exit 1 / 27.4 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'fsencode' |
| `test_dbm_dumb.py` | ok / 66.6 ms | exit 1 / 28.5 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'fsencode' |
| `test_dbm_gnu.py` | exit 1 / 47.9 ms | exit 1 / 42.1 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_dbm_ndbm.py` | ok / 97.8 ms | exit 1 / 27.1 ms | hydro_fail | error[HYD0002]: module '_dbm' not found: not supported on the import search path |
| `test_dbm_sqlite3.py` | ok / 560.9 ms | exit 1 / 26.8 ms | hydro_fail | error[HYD0002]: module '_sqlite3' not found: not supported on the import search path |
| `test_decimal.py` | ok / 1803.6 ms | exit 1 / 58.7 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_decorators.py` | ok / 41.3 ms | exit 1 / 53.4 ms | hydro_fail | AssertionError: 11 test(s) failed |
| `test_defaultdict.py` | ok / 43.4 ms | exit 1 / 19.7 ms | hydro_fail | error[HYD0103]: defaultdict() got an unexpected keyword argument 'foo' |
| `test_deque.py` | ok / 363.3 ms | exit 1 / 62.3 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_descr.py` | exit 1 / 296.9 ms | exit 1 / 73.6 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_descrtut.py` | ok / 126.5 ms | exit 1 / 52.8 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_devpoll.py` | exit 1 / 36.8 ms | exit 1 / 38.6 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_dict.py` | ok / 287.2 ms | exit 1 / 53.4 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_dictcomps.py` | ok / 50.2 ms | exit 1 / 39.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_dictviews.py` | ok / 139.3 ms | exit 1 / 48 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_difflib.py` | ok / 231.9 ms | exit 1 / 53 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_dis.py` | ok / 208.1 ms | exit 1 / 80.6 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_docxmlrpc.py` | ok / 4656.8 ms | exit 1 / 76.6 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_dtrace.py` | exit 1 / 51.4 ms | exit 1 / 34.2 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_dynamic.py` | ok / 46.7 ms | exit 1 / 38.5 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_dynamicclassattribute.py` | ok / 42.6 ms | exit 1 / 6.1 ms | hydro_fail | error[HYD0201]: module 'unittest' has no attribute 'skipIf' |
| `test_eintr.py` | exit 1 / 86.1 ms | exit 1 / 36 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_embed.py` | ok / 60.5 ms | exit 1 / 42.5 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_ensurepip.py` | ok / 147.7 ms | exit 1 / 43.9 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_enum.py` | exit 1 / 537.4 ms | exit 1 / 90.4 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_enumerate.py` | ok / 72 ms | exit 1 / 38.3 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_eof.py` | ok / 277.7 ms | exit 1 / 49.4 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_epoll.py` | ok / 152.5 ms | exit 1 / 37.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_errno.py` | ok / 41.1 ms | ok / 8.6 ms | hydro_pass | - |
| `test_except_star.py` | ok / 52.5 ms | exit 1 / 38.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_exception_group.py` | ok / 252.3 ms | exit 1 / 44.6 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_exception_hierarchy.py` | ok / 49.6 ms | exit 1 / 4.6 ms | hydro_fail | error[HYD0201]: module 'socket' has no attribute 'herror' |
| `test_exception_variations.py` | ok / 44.2 ms | exit 1 / 39.9 ms | hydro_fail | AssertionError: 8 test(s) failed |
| `test_exceptions.py` | exit 1 / 886 ms | exit 1 / 65 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_extcall.py` | exit 1 / 133 ms | exit 1 / 40.7 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_external_inspection.py` | ok / 1305.1 ms | exit 1 / 59.1 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_faulthandler.py` | ok / 18626.7 ms | exit 1 / 40.3 ms | hydro_fail | error[HYD0002]: module 'faulthandler' not found: not supported on the import search path |
| `test_fcntl.py` | ok / 189.8 ms | exit 1 / 39.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_file.py` | ok / 78.8 ms | exit 1 / 29.8 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_file_eintr.py` | ok / 1396 ms | exit 1 / 31.8 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_filecmp.py` | ok / 63.5 ms | exit 1 / 29 ms | hydro_fail | error[HYD0201]: module 'getopt' has no attribute 'GetoptError' |
| `test_fileinput.py` | ok / 134.6 ms | exit 1 / 37.3 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'flags' |
| `test_fileio.py` | ok / 505.7 ms | exit 1 / 45.6 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_fileutils.py` | ok / 127.1 ms | exit 1 / 40.7 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_finalization.py` | ok / 107.1 ms | exit 1 / 44.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_float.py` | ok / 81.5 ms | exit 1 / 41.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_flufl.py` | ok / 39.8 ms | exit 1 / 2.4 ms | hydro_fail | error[HYD0201]: module '__future__' has no attribute 'CO_FUTURE_BARRY_AS_BDFL' |
| `test_fnmatch.py` | ok / 44.5 ms | exit 1 / 13.2 ms | hydro_fail | error[HYD0201]: module 'fnmatch' has no attribute '_translate' |
| `test_fork1.py` | ok / 5129.3 ms | exit 1 / 28 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_format.py` | ok / 50.6 ms | exit 1 / 38.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_fractions.py` | ok / 69.7 ms | exit 1 / 43.5 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_frame.py` | exit 1 / 927.4 ms | exit 1 / 62.3 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_frozen.py` | ok / 46.1 ms | exit 1 / 38.3 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_fstring.py` | exit 1 / 382.9 ms | exit 1 / 4.3 ms | expected_fail | error[HYD0001]: invalid syntax: f-string: expressions nested too deeply |
| `test_ftplib.py` | ok / 6853 ms | exit 1 / 51.5 ms | hydro_fail | error[HYD0201]: module 'socket' has no attribute '_GLOBAL_DEFAULT_TIMEOUT' |
| `test_funcattrs.py` | ok / 52.8 ms | exit 1 / 37.4 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_functools.py` | ok / 351.2 ms | exit 1 / 52 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_gc.py` | exit 1 / 1528.7 ms | exit 1 / 58.7 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_generated_cases.py` | ok / 122.7 ms | exit 1 / 41.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_generator_stop.py` | ok / 40.3 ms | exit 1 / 8.2 ms | hydro_fail | AssertionError: 2 test(s) failed |
| `test_generators.py` | ok / 179.1 ms | exit 1 / 54.1 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_genericalias.py` | ok / 159.4 ms | exit 1 / 88.9 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_genericclass.py` | ok / 48.5 ms | exit 1 / 40 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_genericpath.py` | ok / 53.1 ms | exit 1 / 39.6 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'fstat' |
| `test_genexps.py` | ok / 130 ms | exit 1 / 41.5 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_getopt.py` | ok / 158.9 ms | exit 1 / 54.2 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_getpass.py` | ok / 114.5 ms | exit 1 / 25.4 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'getuid' |
| `test_getpath.py` | ok / 41.1 ms | exit 1 / 40.7 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'fstat' |
| `test_gettext.py` | exit 1 / 161.4 ms | exit 1 / 47.3 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_glob.py` | ok / 107.7 ms | exit 1 / 37.4 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'audit' |
| `test_global.py` | ok / 49.6 ms | exit 1 / 39.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_grammar.py` | ok / 67.1 ms | exit 1 / 5.1 ms | hydro_fail | error[HYD0001]: invalid syntax: invalid syntax. Got unexpected token ',' |
| `test_graphlib.py` | ok / 83.6 ms | exit 1 / 50.4 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_grp.py` | ok / 254.6 ms | exit 1 / 46 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_gzip.py` | ok / 823.4 ms | exit 1 / 47.8 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_hash.py` | ok / 280.9 ms | exit 1 / 47.8 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_hashlib.py` | ok / 13784.7 ms | exit 1 / 55.4 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_heapq.py` | ok / 246.7 ms | exit 1 / 56.7 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_hmac.py` | ok / 163.7 ms | exit 1 / 46.3 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_html.py` | ok / 45.5 ms | exit 1 / 14.1 ms | hydro_fail | AssertionError: 1 test(s) failed |
| `test_htmlparser.py` | ok / 410.1 ms | exit 1 / 49.3 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_http_cookiejar.py` | ok / 259.5 ms | exit 1 / 74.7 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_http_cookies.py` | ok / 555.8 ms | exit 1 / 60.2 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_httplib.py` | ok / 4208.6 ms | exit 1 / 35.6 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_httpservers.py` | ok / 4300.3 ms | exit 1 / 253.6 ms | hydro_fail | error[HYD0201]: module 'math' has no attribute 'isqrt' |
| `test_idle.py` | ok / 9220.1 ms | exit 1 / 38.7 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_imaplib.py` | timeout / 30004.5 ms | exit 1 / 54.4 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_index.py` | ok / 44.3 ms | exit 1 / 37 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_int.py` | ok / 20418.4 ms | exit 1 / 38.7 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_int_literal.py` | ok / 40.2 ms | ok / 15.2 ms | hydro_pass | - |
| `test_io.py` | exit 1 / 28911.3 ms | exit 1 / 52 ms | expected_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_ioctl.py` | ok / 549.2 ms | exit 1 / 24.6 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_ipaddress.py` | ok / 82.1 ms | exit 1 / 119.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_isinstance.py` | ok / 754.2 ms | exit 1 / 37 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_iter.py` | ok / 57.3 ms | exit 1 / 44.3 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_iterlen.py` | ok / 45.2 ms | exit 1 / 5 ms | hydro_fail | error[HYD0201]: module 'operator' has no attribute 'length_hint' |
| `test_itertools.py` | ok / 755 ms | exit 1 / 71.5 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_keyword.py` | ok / 40.6 ms | exit 1 / 14.5 ms | hydro_fail | AssertionError: 6 test(s) failed |
| `test_keywordonlyarg.py` | ok / 42.4 ms | exit 1 / 4.7 ms | hydro_fail | error[HYD0103]: keywordonly_sum() got an unexpected keyword argument 'non_existing_arg' |
| `test_kqueue.py` | exit 1 / 36.8 ms | exit 1 / 36.3 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_largefile.py` | ok / 847.7 ms | exit 1 / 42.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_launcher.py` | exit 1 / 52 ms | exit 1 / 23 ms | expected_fail | error[HYD0002]: module 'winreg' not found: not supported on the import search path |
| `test_linecache.py` | ok / 116.8 ms | exit 1 / 49.2 ms | hydro_fail | error[HYD0002]: module '_frozen_importlib_external' not found: not supported on the import search path |
| `test_list.py` | ok / 352 ms | exit 1 / 39.1 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_listcomps.py` | ok / 158.9 ms | exit 1 / 50.6 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_lltrace.py` | ok / 47.2 ms | exit 1 / 82 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_locale.py` | ok / 144.5 ms | exit 1 / 41 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_logging.py` | ok / 20888.8 ms | exit 1 / 115 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_long.py` | ok / 533.4 ms | exit 1 / 32.8 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_longexp.py` | ok / 131.8 ms | ok / 7.7 ms | hydro_pass | - |
| `test_lzma.py` | ok / 173.7 ms | exit 1 / 39.6 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_mailbox.py` | ok / 1298.9 ms | exit 1 / 173.7 ms | hydro_fail | error[HYD0201]: module 'math' has no attribute 'isqrt' |
| `test_marshal.py` | ok / 230.2 ms | exit 1 / 46.1 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_math.py` | ok / 9021.2 ms | exit 1 / 43 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_math_property.py` | ok / 35.2 ms | exit 1 / 41.4 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_memoryio.py` | ok / 77 ms | exit 1 / 45 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_memoryview.py` | ok / 281.8 ms | exit 1 / 29 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_metaclass.py` | ok / 124 ms | exit 1 / 40.3 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_mimetypes.py` | ok / 424 ms | exit 1 / 51.3 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_minidom.py` | ok / 76.9 ms | exit 1 / 34.9 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_mmap.py` | ok / 102.6 ms | exit 1 / 49.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_modulefinder.py` | ok / 280.7 ms | exit 1 / 168.9 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'meta_path' |
| `test_monitoring.py` | ok / 58.5 ms | exit 1 / 87.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_msvcrt.py` | exit 1 / 35.1 ms | exit 1 / 24 ms | expected_fail | error[HYD0002]: module '_winapi' not found: not supported on the import search path |
| `test_multibytecodec.py` | ok / 209.1 ms | exit 1 / 25 ms | hydro_fail | error[HYD0002]: module '_multibytecodec' not found: not supported on the import search path |
| `test_multiprocessing_main_handling.py` | exit 1 / 1212.2 ms | exit 1 / 49 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_named_expressions.py` | ok / 43.4 ms | exit 1 / 26.6 ms | hydro_fail | error[HYD0002]: assignment expressions inside a generator expression are not supported |
| `test_netrc.py` | ok / 48.4 ms | exit 1 / 26.4 ms | hydro_fail | error[HYD0201]: module 'os.path' has no attribute 'expanduser' |
| `test_ntpath.py` | ok / 264.5 ms | exit 1 / 45.3 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'fstat' |
| `test_nturl2path.py` | ok / 55.9 ms | exit 1 / 124 ms | hydro_fail | error[HYD0201]: module 'math' has no attribute 'isqrt' |
| `test_numeric_tower.py` | ok / 63 ms | exit 1 / 4.8 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'hash_info' |
| `test_opcache.py` | ok / 255.9 ms | exit 1 / 85.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_opcodes.py` | ok / 45.2 ms | exit 1 / 36.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_openpty.py` | ok / 39 ms | exit 1 / 2 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'openpty' |
| `test_operator.py` | ok / 64.3 ms | exit 1 / 46.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_optimizer.py` | ok / 45.6 ms | exit 1 / 38.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_optparse.py` | ok / 136.2 ms | exit 1 / 55 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_ordered_dict.py` | ok / 157.4 ms | exit 1 / 57.4 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_os.py` | exit 1 / 12603.5 ms | exit 1 / 53.2 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_osx_env.py` | ok / 43.2 ms | exit 1 / 37.8 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_patma.py` | exit 1 / 53 ms | exit 1 / 41.2 ms | expected_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_pdb.py` | exit 1 / 16884.8 ms | exit 1 / 98.8 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_peepholer.py` | ok / 329.3 ms | exit 1 / 79.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_pep646_syntax.py` | ok / 213.2 ms | exit 1 / 40.4 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_perf_profiler.py` | ok / 395.2 ms | exit 1 / 61.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_perfmaps.py` | ok / 107.1 ms | exit 1 / 7.5 ms | hydro_fail | ModuleNotFoundError: No module named '_testinternalcapi' |
| `test_pickle.py` | ok / 1958.2 ms | exit 1 / 88.9 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_picklebuffer.py` | ok / 123.6 ms | exit 1 / 44.4 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_pickletools.py` | ok / 509.5 ms | exit 1 / 91.5 ms | hydro_fail | error[HYD0002]: module 'faulthandler' not found: not supported on the import search path |
| `test_pkg.py` | ok / 123.6 ms | exit 1 / 6 ms | hydro_fail | error[HYD0201]: module 'os.path' has no attribute 'islink' |
| `test_pkgutil.py` | exit 1 / 146.3 ms | exit 1 / 56.5 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_platform.py` | ok / 398.4 ms | exit 1 / 48.7 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_plistlib.py` | ok / 220.4 ms | exit 1 / 33.9 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_poll.py` | ok / 11257.1 ms | exit 1 / 38.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_popen.py` | ok / 151 ms | exit 1 / 36.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_poplib.py` | ok / 5867.8 ms | exit 1 / 38.4 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'audit' |
| `test_positional_only_arg.py` | ok / 123.1 ms | exit 1 / 70.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_posix.py` | exit 1 / 834.6 ms | exit 1 / 42.6 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_posixpath.py` | ok / 313.1 ms | exit 1 / 43.7 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'fstat' |
| `test_pow.py` | ok / 284.7 ms | exit 1 / 4.9 ms | hydro_fail | error[HYD0101]: name 'pow' is not defined |
| `test_pprint.py` | ok / 161.7 ms | exit 1 / 42 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'is_dataclass' |
| `test_print.py` | ok / 116.4 ms | exit 1 / 36.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_profile.py` | ok / 278.6 ms | exit 1 / 147.8 ms | hydro_fail | error[HYD0201]: module 'functools' has no attribute 'cmp_to_key' |
| `test_property.py` | ok / 94.9 ms | exit 1 / 39.7 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_pstats.py` | ok / 154 ms | exit 1 / 32.5 ms | hydro_fail | error[HYD0002]: module '_lsprof' not found: not supported on the import search path |
| `test_pty.py` | ok / 102.3 ms | exit 1 / 30.1 ms | hydro_fail | error[HYD0002]: module 'termios' not found: not supported on the import search path |
| `test_pulldom.py` | ok / 152.2 ms | exit 1 / 148.6 ms | hydro_fail | error[HYD0201]: module 'math' has no attribute 'isqrt' |
| `test_pwd.py` | ok / 92.7 ms | exit 1 / 40.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_py_compile.py` | ok / 371.5 ms | exit 1 / 166.7 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'meta_path' |
| `test_pyclbr.py` | exit 1 / 89.2 ms | exit 1 / 30.6 ms | expected_fail | error[HYD0201]: module 'importlib.util' has no attribute '_find_spec_from_path' |
| `test_pyexpat.py` | ok / 210.9 ms | exit 1 / 27.1 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_queue.py` | ok / 7694.6 ms | exit 1 / 46 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_quopri.py` | ok / 125.1 ms | exit 1 / 36.3 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_raise.py` | ok / 86 ms | exit 1 / 35.6 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_random.py` | ok / 723.7 ms | exit 1 / 42.6 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_range.py` | ok / 185 ms | exit 1 / 38 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_re.py` | ok / 409.2 ms | exit 1 / 45.7 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_readline.py` | ok / 227.9 ms | exit 1 / 48.5 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_regrtest.py` | exit 1 / 1371.6 ms | exit 1 / 71 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_remote_pdb.py` | ok / 2328.5 ms | exit 1 / 57.7 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_repl.py` | ok / 868.7 ms | exit 1 / 48.8 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_reprlib.py` | ok / 66.2 ms | exit 1 / 39.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_resource.py` | ok / 82.9 ms | exit 1 / 40.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_richcmp.py` | ok / 44.2 ms | exit 1 / 40.6 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_rlcompleter.py` | ok / 104.7 ms | exit 1 / 39.7 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_robotparser.py` | ok / 1256.1 ms | exit 1 / 65.9 ms | hydro_fail | error[HYD0201]: module 'tempfile' has no attribute '_TemporaryFileWrapper' |
| `test_runpy.py` | timeout / 30011.5 ms | exit 1 / 42.1 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_sax.py` | ok / 176.1 ms | exit 1 / 56.3 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_sched.py` | ok / 582.8 ms | exit 1 / 38.4 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_scope.py` | ok / 45.5 ms | exit 1 / 42.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_script_helper.py` | ok / 176.5 ms | exit 1 / 47.5 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_secrets.py` | ok / 36.9 ms | exit 1 / 5.6 ms | hydro_fail | error[HYD0201]: module '_operator' has no attribute '_compare_digest' |
| `test_select.py` | ok / 1549.7 ms | exit 1 / 36.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_selectors.py` | ok / 14674 ms | exit 1 / 41 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_set.py` | ok / 717.3 ms | exit 1 / 49.7 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_setcomps.py` | ok / 115.7 ms | exit 1 / 48.3 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_shelve.py` | ok / 2472.7 ms | exit 1 / 33.2 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'fsencode' |
| `test_shlex.py` | ok / 134.8 ms | exit 1 / 44.4 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_shutil.py` | exit 1 / 423.6 ms | exit 1 / 64 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_signal.py` | timeout / 30017.6 ms | exit 1 / 38 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_site.py` | ok / 1933.6 ms | exit 1 / 73.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_slice.py` | ok / 49.9 ms | exit 1 / 46.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_smtplib.py` | ok / 1422.2 ms | exit 1 / 81.2 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_smtpnet.py` | ok / 9230 ms | exit 1 / 57.8 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_socket.py` | ok / 27203.7 ms | exit 1 / 45.9 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_socketserver.py` | ok / 486 ms | exit 1 / 34 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_sort.py` | ok / 82.5 ms | exit 1 / 36.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_source_encoding.py` | ok / 11748.4 ms | exit 1 / 48 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_ssl.py` | ok / 19280.3 ms | exit 1 / 86.3 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_stable_abi_ctypes.py` | exit 1 / 119.9 ms | exit 1 / 37.8 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_startfile.py` | exit 1 / 108.7 ms | exit 1 / 36.6 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_stat.py` | ok / 138.8 ms | exit 1 / 38.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_statistics.py` | ok / 8906.4 ms | exit 1 / 59.9 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_str.py` | ok / 1022 ms | exit 1 / 60.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_strftime.py` | ok / 151.6 ms | exit 1 / 136.8 ms | hydro_fail | error[HYD0101]: name 'FEBRUARY' is not defined |
| `test_string_literals.py` | ok / 123.7 ms | exit 1 / 61.7 ms | hydro_fail | AssertionError: 19 test(s) failed |
| `test_stringprep.py` | ok / 106.2 ms | exit 1 / 27.1 ms | hydro_fail | error[HYD0201]: module 'unicodedata' has no attribute 'ucd_3_2_0' |
| `test_strptime.py` | ok / 152.9 ms | exit 1 / 49.7 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_strtod.py` | ok / 226.9 ms | exit 1 / 35.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_struct.py` | ok / 352.1 ms | exit 1 / 38.1 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_structseq.py` | ok / 150.5 ms | exit 1 / 61.7 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_subclassinit.py` | ok / 112.2 ms | exit 1 / 5.4 ms | hydro_fail | error[HYD0002]: class keyword 'otherarg' cannot be forwarded together with metaclass=; Hydro's metaclass hook… |
| `test_subprocess.py` | exit 1 / 15199.3 ms | exit 1 / 45.2 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_sundry.py` | ok / 102.6 ms | exit 1 / 42.3 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_super.py` | ok / 215.5 ms | exit 1 / 42.9 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_support.py` | exit 1 / 4716 ms | exit 1 / 40.9 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_symtable.py` | ok / 55.7 ms | exit 1 / 30 ms | hydro_fail | error[HYD0002]: module '_symtable' not found: not supported on the import search path |
| `test_syntax.py` | ok / 178.2 ms | exit 1 / 51.1 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_sys.py` | exit 1 / 1152.7 ms | exit 1 / 58.9 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_sys_setprofile.py` | ok / 42.6 ms | exit 1 / 20.6 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'is_dataclass' |
| `test_sys_settrace.py` | ok / 765 ms | exit 1 / 45.6 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_sysconfig.py` | exit 1 / 198.5 ms | exit 1 / 34 ms | expected_fail | error[HYD0002]: module '_sysconfig' not found: not supported on the import search path |
| `test_syslog.py` | ok / 184 ms | exit 1 / 40.8 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_tabnanny.py` | ok / 118.4 ms | exit 1 / 47 ms | hydro_fail | error[HYD0201]: module 'os.path' has no attribute 'islink' |
| `test_tarfile.py` | exit 1 / 5234.9 ms | exit 1 / 74.3 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_tcl.py` | ok / 114.9 ms | exit 1 / 41.3 ms | hydro_fail | error[HYD0002]: module '_tkinter' not found: not supported on the import search path |
| `test_tempfile.py` | ok / 385.6 ms | exit 1 / 59.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_termios.py` | ok / 555.3 ms | exit 1 / 41.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_textwrap.py` | ok / 45.6 ms | exit 1 / 33.5 ms | hydro_fail | error[HYD0201]: module 'textwrap' has no attribute 'TextWrapper' |
| `test_thread.py` | ok / 1275.8 ms | exit 1 / 44.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_thread_local_bytecode.py` | ok / 49.7 ms | exit 1 / 49.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_threadedtempfile.py` | ok / 50.6 ms | exit 1 / 37 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_threading.py` | exit 1 / 13967.5 ms | exit 1 / 47.6 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_threading_local.py` | ok / 294 ms | exit 1 / 55.2 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_threadsignals.py` | ok / 4898.4 ms | exit 1 / 36.4 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_time.py` | ok / 3098.5 ms | exit 1 / 45.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_timeit.py` | ok / 56.2 ms | exit 1 / 37.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_timeout.py` | ok / 9927.2 ms | exit 1 / 38 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_tokenize.py` | ok / 6076.8 ms | exit 1 / 55.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_trace.py` | ok / 930.3 ms | exit 1 / 58.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_traceback.py` | exit 1 / 2107.3 ms | exit 1 / 60.1 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_tracemalloc.py` | ok / 642.1 ms | exit 1 / 58 ms | hydro_fail | error[HYD0002]: module '_tracemalloc' not found: not supported on the import search path |
| `test_tstring.py` | exit 1 / 275.6 ms | exit 1 / 50.4 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_ttk_textonly.py` | ok / 73.2 ms | exit 1 / 40.7 ms | hydro_fail | error[HYD0002]: module '_tkinter' not found: not supported on the import search path |
| `test_tty.py` | ok / 53.5 ms | exit 1 / 41.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_tuple.py` | ok / 215.4 ms | exit 1 / 38.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_turtle.py` | ok / 181.6 ms | exit 1 / 43.2 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_type_aliases.py` | ok / 50.1 ms | exit 1 / 38.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_type_annotations.py` | ok / 47.7 ms | exit 1 / 40.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_type_cache.py` | exit 1 / 45.1 ms | exit 1 / 76.8 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_type_comments.py` | ok / 52.3 ms | exit 1 / 38.8 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_type_params.py` | ok / 55.9 ms | exit 1 / 4.7 ms | hydro_fail | error[HYD0001]: invalid syntax: invalid syntax. Got unexpected token '=' |
| `test_typechecks.py` | ok / 40.3 ms | exit 1 / 17.2 ms | hydro_fail | AssertionError: 4 test(s) failed |
| `test_types.py` | ok / 223.9 ms | exit 1 / 54.7 ms | hydro_fail | error[HYD0002]: module '_datetime' not found: not supported on the import search path |
| `test_typing.py` | ok / 370.3 ms | exit 1 / 73.1 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_ucn.py` | ok / 135.6 ms | exit 1 / 46.8 ms | hydro_fail | error[HYD0201]: module 'tempfile' has no attribute '_TemporaryFileWrapper' |
| `test_unary.py` | ok / 41 ms | exit 1 / 10.4 ms | hydro_fail | AssertionError: 2 test(s) failed |
| `test_unicode_file.py` | ok / 46.5 ms | exit 1 / 33.7 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'audit' |
| `test_unicode_file_functions.py` | ok / 67.1 ms | exit 1 / 38.5 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_unicode_identifiers.py` | ok / 40.2 ms | exit 1 / 7.6 ms | hydro_fail | AssertionError: 3 test(s) failed |
| `test_unicodedata.py` | ok / 4213.8 ms | exit 1 / 68.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_univnewlines.py` | ok / 55.2 ms | exit 1 / 140.3 ms | hydro_fail | error[HYD0201]: module 'io' has no attribute '__all__' |
| `test_unpack.py` | exit 1 / 140.3 ms | exit 1 / 42.3 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_unpack_ex.py` | exit 1 / 141.9 ms | exit 1 / 40.7 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_unparse.py` | ok / 5837.7 ms | exit 1 / 38.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_urllib.py` | ok / 216 ms | exit 1 / 150.2 ms | hydro_fail | error[HYD0201]: module 'math' has no attribute 'isqrt' |
| `test_urllib2.py` | ok / 1988.9 ms | exit 1 / 63.3 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_urllib2_localnet.py` | ok / 2066.9 ms | exit 1 / 166.6 ms | hydro_fail | error[HYD0201]: module 'math' has no attribute 'isqrt' |
| `test_urllib2net.py` | ok / 29798.3 ms | exit 1 / 72.8 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_urllib_response.py` | ok / 57.4 ms | exit 1 / 23.4 ms | hydro_fail | error[HYD0201]: module 'tempfile' has no attribute '_TemporaryFileWrapper' |
| `test_urllibnet.py` | ok / 7625.9 ms | exit 1 / 66.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_urlparse.py` | ok / 124.2 ms | exit 1 / 128.2 ms | hydro_fail | error[HYD0201]: module 'math' has no attribute 'isqrt' |
| `test_userdict.py` | ok / 132.4 ms | exit 1 / 38 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_userlist.py` | ok / 131 ms | exit 1 / 42.7 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_userstring.py` | ok / 554.4 ms | exit 1 / 50.1 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_utf8_mode.py` | ok / 398 ms | exit 1 / 50 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_utf8source.py` | ok / 41 ms | exit 1 / 6.8 ms | hydro_fail | AssertionError: 2 test(s) failed |
| `test_uuid.py` | ok / 267.8 ms | exit 1 / 160.7 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_venv.py` | exit 1 / 6663.4 ms | exit 1 / 49.4 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_wait3.py` | ok / 5062.2 ms | exit 1 / 39.4 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_wait4.py` | ok / 5053.8 ms | exit 1 / 37.3 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_wave.py` | ok / 64.5 ms | exit 1 / 26.9 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_weakref.py` | exit 1 / 2324.3 ms | exit 1 / 66.6 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_weakset.py` | ok / 63.1 ms | exit 1 / 49.9 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_webbrowser.py` | ok / 1941.3 ms | exit 1 / 48.8 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'audit' |
| `test_winapi.py` | exit 1 / 53.4 ms | exit 1 / 42.2 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_winconsoleio.py` | exit 1 / 44.5 ms | exit 1 / 23.8 ms | expected_fail | error[HYD0002]: module '_testconsole' not found: not supported on the import search path |
| `test_winreg.py` | exit 1 / 47.1 ms | exit 1 / 25.5 ms | expected_fail | error[HYD0002]: module 'winreg' not found: not supported on the import search path |
| `test_winsound.py` | exit 1 / 41.4 ms | exit 1 / 39.2 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_with.py` | ok / 43.2 ms | exit 1 / 26.4 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_wmi.py` | exit 1 / 46.5 ms | exit 1 / 38.5 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_wsgiref.py` | ok / 227.8 ms | exit 1 / 66.2 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_xml_dom_minicompat.py` | ok / 39.2 ms | exit 1 / 18.2 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'flags' |
| `test_xml_dom_xmlbuilder.py` | ok / 167.6 ms | exit 1 / 40.2 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_xml_etree.py` | ok / 777.4 ms | exit 1 / 61 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_xml_etree_c.py` | ok / 490.3 ms | exit 1 / 38.5 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_xmlrpc.py` | ok / 27420.5 ms | exit 1 / 86.7 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_xpickle.py` | ok / 2086.3 ms | exit 1 / 68.6 ms | hydro_fail | error[HYD0002]: module 'faulthandler' not found: not supported on the import search path |
| `test_xxlimited.py` | exit 1 / 47.2 ms | exit 1 / 38.7 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_xxtestfuzz.py` | ok / 53.7 ms | exit 1 / 24.1 ms | hydro_fail | error[HYD0002]: module 'faulthandler' not found: not supported on the import search path |
| `test_yield_from.py` | ok / 55.4 ms | exit 1 / 39.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_zipapp.py` | ok / 136.8 ms | exit 1 / 54.4 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_zipfile64.py` | timeout / 30031.2 ms | exit 1 / 47.7 ms | expected_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_zipimport.py` | ok / 16776.8 ms | exit 1 / 78 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_zipimport_support.py` | ok / 497.4 ms | exit 1 / 74.3 ms | hydro_fail | error[HYD0002]: module '_frozen_importlib_external' not found: not supported on the import search path |
| `test_zlib.py` | ok / 391.7 ms | exit 1 / 41.7 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_zstd.py` | ok / 354.1 ms | exit 1 / 40.3 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |

<!-- END GENERATED RESULTS -->
