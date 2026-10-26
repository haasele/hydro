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

Environment: CPython `3.14.9` (`/home/aura/.local/share/uv/python/cpython-3.14.9-linux-x86_64-gnu/bin/python3.14`) · Hydro `tea 0.1.0` (`/mnt/Vault/GitHub/tea/target/debug/tea`)

Source: `/mnt/Vault/GitHub/hydro/.hydro/cpython/Python-3.14.9` · generated 2026-10-26T07:52:22Z

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
- `test_perfmaps.py`

## Failure classes

| Status | Tests |
| --- | ---: |
| hydro_fail | 332 |
| expected_fail | 63 |

## Test results

| Test | CPython 3.14.9 | Hydro | Status | First error |
| --- | --- | --- | --- | --- |
| `test___all__.py` | ok / 1672.8 ms | exit 1 / 726.5 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test__colorize.py` | ok / 116.3 ms | exit 1 / 596 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test__interpchannels.py` | ok / 795.3 ms | exit 1 / 545.1 ms | hydro_fail | error[HYD0002]: module '_interpreters' not found: not supported on the import search path |
| `test__interpreters.py` | ok / 666.1 ms | exit 1 / 504 ms | hydro_fail | error[HYD0002]: module '_interpreters' not found: not supported on the import search path |
| `test__locale.py` | ok / 56 ms | exit 1 / 511.7 ms | hydro_fail | error[HYD0002]: module '_locale' not found: not supported on the import search path |
| `test__opcode.py` | ok / 51.8 ms | exit 1 / 471.2 ms | hydro_fail | error[HYD0201]: module '_opcode' has no attribute 'stack_effect' |
| `test__osx_support.py` | ok / 55.6 ms | exit 1 / 749.1 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_abc.py` | ok / 52.7 ms | exit 1 / 598.5 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_abstract_numbers.py` | ok / 46.4 ms | exit 1 / 29.9 ms | hydro_fail | error[HYD0201]: module 'abc' has no attribute 'ABCMeta' |
| `test_android.py` | exit 1 / 110.9 ms | exit 1 / 578.5 ms | expected_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_annotationlib.py` | ok / 76.4 ms | exit 1 / 808.7 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_apple.py` | exit 1 / 109.5 ms | exit 1 / 570.6 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_argparse.py` | ok / 1387.3 ms | exit 1 / 719.2 ms | hydro_fail | error[HYD0201]: module 'dataclasses' has no attribute 'Field' |
| `test_array.py` | ok / 286.4 ms | exit 1 / 535.8 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_asdl_parser.py` | exit 1 / 33.8 ms | exit 1 / 641.5 ms | expected_fail | error[HYD0002]: decorators are not supported in Phase 1 |
| `test_asyncgen.py` | ok / 849 ms | exit 1 / 771.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_atexit.py` | exit 1 / 142.3 ms | exit 1 / 788.1 ms | expected_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_audit.py` | ok / 1717 ms | exit 1 / 743.4 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_augassign.py` | ok / 43.5 ms | exit 1 / 19.1 ms | hydro_fail | error[HYD0101]: name 'compile' is not defined |
| `test_base64.py` | ok / 143.9 ms | exit 1 / 496.2 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_baseexception.py` | ok / 49.8 ms | exit 1 / 19.1 ms | hydro_fail | error[HYD0101]: name 'object' is not defined |
| `test_bdb.py` | exit 1 / 76.3 ms | exit 1 / 616.5 ms | expected_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_bigaddrspace.py` | ok / 45.9 ms | exit 1 / 713.3 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_bigmem.py` | ok / 63.3 ms | exit 1 / 816.7 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_binascii.py` | ok / 64.4 ms | exit 1 / 466.3 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_binop.py` | ok / 44 ms | exit 1 / 85.1 ms | hydro_fail | error[HYD0201]: module 'abc' has no attribute 'ABCMeta' |
| `test_bisect.py` | ok / 61.2 ms | exit 1 / 785.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_bool.py` | ok / 49.8 ms | exit 1 / 769.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_buffer.py` | ok / 95 ms | exit 1 / 601.1 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_bufio.py` | ok / 147 ms | exit 1 / 796.1 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_build_details.py` | exit 1 / 131.3 ms | exit 1 / 555.3 ms | expected_fail | error[HYD0201]: module 'os.path' has no attribute 'realpath' |
| `test_builtin.py` | ok / 377.3 ms | exit 1 / 780.5 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_bytes.py` | exit 1 / 92.7 ms | exit 1 / 707 ms | expected_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_bz2.py` | ok / 382.7 ms | exit 1 / 506.9 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_c_locale_coercion.py` | ok / 832.4 ms | exit 1 / 696.8 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_calendar.py` | ok / 1427.4 ms | exit 1 / 712.7 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_call.py` | exit 1 / 62.3 ms | exit 1 / 479.6 ms | expected_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_charmapcodec.py` | ok / 45.5 ms | exit 1 / 177.8 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_class.py` | ok / 65.3 ms | exit 1 / 491.1 ms | hydro_fail | error[HYD0002]: module '_testinternalcapi' not found: not supported on the import search path |
| `test_clinic.py` | ok / 242.9 ms | exit 1 / 892.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_cmath.py` | ok / 92.2 ms | exit 1 / 597.2 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_cmd.py` | ok / 201.6 ms | exit 1 / 675.9 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_cmd_line.py` | exit 1 / 1783.6 ms | exit 1 / 610.3 ms | expected_fail | error[HYD0201]: module 'os.path' has no attribute 'realpath' |
| `test_cmd_line_script.py` | exit 1 / 1456.8 ms | exit 1 / 498.7 ms | expected_fail | error[HYD0002]: module '_frozen_importlib_external' not found: not supported on the import search path |
| `test_code.py` | ok / 213.5 ms | exit 1 / 745.3 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_code_module.py` | ok / 135.8 ms | exit 1 / 481.4 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'stat' |
| `test_codeccallbacks.py` | ok / 62.5 ms | exit 1 / 118 ms | hydro_fail | error[HYD0002]: module 'unicodedata' not found: not supported on the import search path |
| `test_codecencodings_cn.py` | ok / 128.2 ms | exit 1 / 787 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_codecencodings_hk.py` | ok / 96 ms | exit 1 / 779.6 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_codecencodings_iso2022.py` | ok / 144.3 ms | exit 1 / 823.7 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_codecencodings_jp.py` | ok / 183.9 ms | exit 1 / 854.6 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_codecencodings_kr.py` | ok / 110.9 ms | exit 1 / 784.6 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_codecencodings_tw.py` | ok / 104 ms | exit 1 / 768.9 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_codecmaps_cn.py` | ok / 182.9 ms | exit 1 / 800.2 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_codecmaps_hk.py` | ok / 116.9 ms | exit 1 / 796.4 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_codecmaps_jp.py` | ok / 174.5 ms | exit 1 / 795.1 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_codecmaps_kr.py` | ok / 169 ms | exit 1 / 813.7 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_codecmaps_tw.py` | ok / 143.1 ms | exit 1 / 786.6 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_codecs.py` | ok / 340.9 ms | exit 1 / 747.6 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_codeop.py` | ok / 54.3 ms | exit 1 / 758.7 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_collections.py` | ok / 1143.3 ms | exit 1 / 749.6 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_colorsys.py` | ok / 46.1 ms | exit 1 / 315.7 ms | hydro_fail | AssertionError: 8 test(s) failed |
| `test_compare.py` | ok / 61.2 ms | exit 1 / 746.3 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_compile.py` | ok / 2289 ms | exit 1 / 536.9 ms | hydro_fail | error[HYD0002]: module '_ast' not found: not supported on the import search path |
| `test_compileall.py` | exit 1 / 66.1 ms | exit 1 / 548.9 ms | expected_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_compiler_assemble.py` | ok / 40.8 ms | exit 1 / 457.3 ms | hydro_fail | error[HYD0201]: module '_opcode' has no attribute 'stack_effect' |
| `test_compiler_codegen.py` | ok / 37.5 ms | exit 1 / 740.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_complex.py` | ok / 68.1 ms | exit 1 / 499.8 ms | hydro_fail | error[HYD0002]: module 'cmath' not found: not supported on the import search path |
| `test_configparser.py` | ok / 415.2 ms | exit 1 / 1043.8 ms | hydro_fail | error[HYD0201]: module 'collections' has no attribute 'ChainMap' |
| `test_contains.py` | ok / 57.6 ms | exit 1 / 768.2 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_context.py` | ok / 534.9 ms | exit 1 / 472.7 ms | hydro_fail | error[HYD0002]: module '_contextvars' not found: not supported on the import search path |
| `test_contextlib.py` | ok / 71.2 ms | exit 1 / 507.2 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'stat' |
| `test_contextlib_async.py` | ok / 75.8 ms | exit 1 / 805.4 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_copy.py` | ok / 79.9 ms | exit 1 / 534.1 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_copyreg.py` | ok / 81.5 ms | exit 1 / 921.5 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_coroutines.py` | ok / 268.5 ms | exit 1 / 603.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_cprofile.py` | ok / 529.2 ms | exit 1 / 551.6 ms | hydro_fail | error[HYD0002]: module '_lsprof' not found: not supported on the import search path |
| `test_crossinterp.py` | exit 1 / 55.1 ms | exit 1 / 480.5 ms | expected_fail | error[HYD0002]: module '_interpreters' not found: not supported on the import search path |
| `test_csv.py` | ok / 240.2 ms | exit 1 / 590.4 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_curses.py` | ok / 291 ms | exit 1 / 665.7 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_datetime.py` | ok / 18911.1 ms | exit 1 / 765.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_dbm.py` | ok / 386.7 ms | exit 1 / 463.2 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_dbm_dumb.py` | ok / 76.1 ms | exit 1 / 458.6 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_dbm_gnu.py` | exit 1 / 49.9 ms | exit 1 / 770.8 ms | expected_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_dbm_ndbm.py` | ok / 107.4 ms | exit 1 / 461.3 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_dbm_sqlite3.py` | ok / 589.4 ms | exit 1 / 477 ms | hydro_fail | error[HYD0002]: module '_sqlite3' not found: not supported on the import search path |
| `test_decimal.py` | ok / 1939.5 ms | exit 1 / 721.9 ms | hydro_fail | error[HYD0002]: module '_contextvars' not found: not supported on the import search path |
| `test_decorators.py` | ok / 45.2 ms | exit 1 / 39.2 ms | hydro_fail | error[HYD0101]: name 'compile' is not defined |
| `test_defaultdict.py` | ok / 44.3 ms | exit 1 / 107.4 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_deque.py` | ok / 374.2 ms | exit 1 / 739.8 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_descr.py` | exit 1 / 336.3 ms | exit 1 / 633.7 ms | expected_fail | error[HYD0101]: name 'complex' is not defined |
| `test_descrtut.py` | ok / 141.8 ms | exit 1 / 653.7 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_devpoll.py` | exit 1 / 39 ms | exit 1 / 743.1 ms | expected_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_dict.py` | ok / 304.5 ms | exit 1 / 567.3 ms | hydro_fail | error[HYD0201]: module 'collections' has no attribute 'ChainMap' |
| `test_dictcomps.py` | ok / 48.6 ms | exit 1 / 469.2 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'stat' |
| `test_dictviews.py` | ok / 143 ms | exit 1 / 545 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_difflib.py` | ok / 238.7 ms | exit 1 / 702.3 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_dis.py` | ok / 225.5 ms | exit 1 / 568.4 ms | hydro_fail | error[HYD0201]: module '_opcode' has no attribute 'stack_effect' |
| `test_docxmlrpc.py` | ok / 4668.1 ms | exit 1 / 852.7 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_dtrace.py` | exit 1 / 55 ms | exit 1 / 477.9 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_dynamic.py` | ok / 50.3 ms | exit 1 / 754.4 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_dynamicclassattribute.py` | ok / 44.3 ms | exit 1 / 26.4 ms | hydro_fail | error[HYD0201]: module 'types' has no attribute 'DynamicClassAttribute' |
| `test_eintr.py` | exit 1 / 99.5 ms | exit 1 / 468.1 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_embed.py` | ok / 63.9 ms | exit 1 / 833 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_ensurepip.py` | ok / 157.6 ms | exit 1 / 615.8 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_enum.py` | exit 1 / 579.6 ms | exit 1 / 1007.4 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_enumerate.py` | ok / 80.5 ms | exit 1 / 750.4 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_eof.py` | ok / 311.2 ms | exit 1 / 637.7 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_epoll.py` | ok / 153.6 ms | exit 1 / 726.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_errno.py` | ok / 42.8 ms | ok / 111.9 ms | hydro_pass | - |
| `test_except_star.py` | ok / 56.3 ms | exit 1 / 754.2 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_exception_group.py` | ok / 262.1 ms | exit 1 / 756.2 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_exception_hierarchy.py` | ok / 47.1 ms | exit 1 / 24.5 ms | hydro_fail | error[HYD0201]: module 'errno' has no attribute 'EEXIST' |
| `test_exception_variations.py` | ok / 44.8 ms | exit 1 / 553 ms | hydro_fail | AssertionError: 8 test(s) failed |
| `test_exceptions.py` | exit 1 / 949.4 ms | exit 1 / 617.2 ms | expected_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_extcall.py` | exit 1 / 143.6 ms | exit 1 / 545.9 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_external_inspection.py` | ok / 1427.8 ms | exit 1 / 659.7 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_faulthandler.py` | ok / 18705.6 ms | exit 1 / 482.8 ms | hydro_fail | error[HYD0002]: module 'faulthandler' not found: not supported on the import search path |
| `test_fcntl.py` | ok / 200.8 ms | exit 1 / 490.4 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_file.py` | ok / 84.5 ms | exit 1 / 497.3 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_file_eintr.py` | ok / 1436.6 ms | exit 1 / 497 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_filecmp.py` | ok / 76.2 ms | exit 1 / 484.6 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'stat' |
| `test_fileinput.py` | ok / 129.7 ms | exit 1 / 537.5 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'fspath' |
| `test_fileio.py` | ok / 530.9 ms | exit 1 / 558.6 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_fileutils.py` | ok / 125.8 ms | exit 1 / 756.2 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_finalization.py` | ok / 99.3 ms | exit 1 / 515.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_float.py` | ok / 95.7 ms | exit 1 / 514.9 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_flufl.py` | ok / 45.9 ms | exit 1 / 9.2 ms | hydro_fail | error[HYD0101]: name 'compile' is not defined |
| `test_fnmatch.py` | ok / 52.9 ms | exit 1 / 49.6 ms | hydro_fail | error[HYD0201]: module 'collections' has no attribute 'ChainMap' |
| `test_fork1.py` | ok / 5130.1 ms | exit 1 / 454.1 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_format.py` | ok / 54.7 ms | exit 1 / 829.2 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_fractions.py` | ok / 77 ms | exit 1 / 510.4 ms | hydro_fail | error[HYD0002]: 'from _decimal import *' is not supported for a module that is not loaded |
| `test_frame.py` | exit 1 / 937.4 ms | exit 1 / 591.1 ms | expected_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_frozen.py` | ok / 53.2 ms | exit 1 / 994.8 ms | hydro_fail | error[HYD0002]: decorators are not supported in Phase 1 |
| `test_fstring.py` | exit 1 / 393.9 ms | exit 1 / 36.8 ms | expected_fail | error[HYD0001]: invalid syntax: f-string: expressions nested too deeply |
| `test_ftplib.py` | ok / 7063.1 ms | exit 1 / 701.7 ms | hydro_fail | error[HYD0201]: module 'socket' has no attribute '_GLOBAL_DEFAULT_TIMEOUT' |
| `test_funcattrs.py` | ok / 55.8 ms | exit 1 / 750.9 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_functools.py` | ok / 373.8 ms | exit 1 / 758.5 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_gc.py` | exit 1 / 1539.3 ms | exit 1 / 646.5 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_generated_cases.py` | ok / 120.7 ms | exit 1 / 757 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_generator_stop.py` | ok / 42.5 ms | exit 1 / 93.8 ms | hydro_fail | AssertionError: 2 test(s) failed |
| `test_generators.py` | ok / 185.1 ms | exit 1 / 676.2 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_genericalias.py` | ok / 171.6 ms | exit 1 / 890.3 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_genericclass.py` | ok / 53.7 ms | exit 1 / 782.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_genericpath.py` | ok / 57 ms | exit 1 / 507.5 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'stat' |
| `test_genexps.py` | ok / 135.5 ms | exit 1 / 553.8 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_getopt.py` | ok / 165.4 ms | exit 1 / 722.8 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_getpass.py` | ok / 124.3 ms | exit 1 / 464 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'O_NOCTTY' |
| `test_getpath.py` | ok / 47.6 ms | exit 1 / 590.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_gettext.py` | exit 1 / 163.7 ms | exit 1 / 625.5 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_glob.py` | ok / 106.8 ms | exit 1 / 569 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'audit' |
| `test_global.py` | ok / 50.3 ms | exit 1 / 706.7 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_grammar.py` | ok / 69.2 ms | exit 1 / 39.2 ms | hydro_fail | error[HYD0001]: invalid syntax: invalid syntax. Got unexpected token ',' |
| `test_graphlib.py` | ok / 92.8 ms | exit 1 / 504.1 ms | hydro_fail | error[HYD0103]: raise with a message requires exactly one argument |
| `test_grp.py` | ok / 329.8 ms | exit 1 / 488.1 ms | hydro_fail | error[HYD0201]: module 'collections' has no attribute 'ChainMap' |
| `test_gzip.py` | ok / 877 ms | exit 1 / 501.8 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_hash.py` | ok / 304.3 ms | exit 1 / 785.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_hashlib.py` | ok / 14294.1 ms | exit 1 / 704.4 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_heapq.py` | ok / 246.6 ms | exit 1 / 731.1 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_hmac.py` | ok / 169.9 ms | exit 1 / 651.2 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_html.py` | ok / 50.2 ms | exit 1 / 176.9 ms | hydro_fail | AssertionError: 1 test(s) failed |
| `test_htmlparser.py` | ok / 459.9 ms | exit 1 / 683.6 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_http_cookiejar.py` | ok / 279.5 ms | exit 1 / 1146.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_http_cookies.py` | ok / 623.6 ms | exit 1 / 788.5 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_httplib.py` | ok / 3581.3 ms | exit 1 / 727.5 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_httpservers.py` | ok / 3937.1 ms | exit 1 / 803.8 ms | hydro_fail | error[HYD0201]: module 'enum' has no attribute 'StrEnum' |
| `test_idle.py` | ok / 9791.9 ms | exit 1 / 713.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_imaplib.py` | timeout / 30004.1 ms | exit 1 / 949.6 ms | expected_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_index.py` | ok / 49.5 ms | exit 1 / 699.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_int.py` | ok / 20882.3 ms | exit 1 / 712.4 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_int_literal.py` | ok / 40.2 ms | ok / 200.5 ms | hydro_pass | - |
| `test_io.py` | exit 1 / 28633.3 ms | exit 1 / 704.1 ms | expected_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_ioctl.py` | ok / 555.2 ms | exit 1 / 442.8 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_ipaddress.py` | ok / 92.4 ms | exit 1 / 606.2 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_isinstance.py` | ok / 810.5 ms | exit 1 / 702.5 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_iter.py` | ok / 65.4 ms | exit 1 / 724.1 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_iterlen.py` | ok / 43.4 ms | exit 1 / 24.4 ms | hydro_fail | error[HYD0201]: module 'collections' has no attribute 'deque' |
| `test_itertools.py` | ok / 767.9 ms | exit 1 / 785.4 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_keyword.py` | ok / 42.1 ms | exit 1 / 6.4 ms | hydro_fail | error[HYD0101]: name 'setattr' is not defined |
| `test_keywordonlyarg.py` | ok / 42.1 ms | exit 1 / 29.3 ms | hydro_fail | error[HYD0101]: name 'compile' is not defined |
| `test_kqueue.py` | exit 1 / 38.9 ms | exit 1 / 698 ms | expected_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_largefile.py` | ok / 1319.6 ms | exit 1 / 783.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_launcher.py` | exit 1 / 66.2 ms | exit 1 / 451.2 ms | expected_fail | error[HYD0002]: module 'winreg' not found: not supported on the import search path |
| `test_linecache.py` | ok / 122.6 ms | exit 1 / 556.5 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_list.py` | ok / 367.7 ms | exit 1 / 477.6 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_listcomps.py` | ok / 163.9 ms | exit 1 / 667.6 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_lltrace.py` | ok / 49 ms | exit 1 / 472.5 ms | hydro_fail | error[HYD0201]: module '_opcode' has no attribute 'stack_effect' |
| `test_locale.py` | ok / 148.1 ms | exit 1 / 499.5 ms | hydro_fail | error[HYD0002]: 'from _decimal import *' is not supported for a module that is not loaded |
| `test_logging.py` | ok / 20489.5 ms | exit 1 / 1275.8 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_long.py` | ok / 486.1 ms | exit 1 / 465.7 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_longexp.py` | ok / 104.8 ms | exit 1 / 3.6 ms | hydro_fail | error[HYD0101]: name 'eval' is not defined |
| `test_lzma.py` | ok / 174.1 ms | exit 1 / 484.2 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_mailbox.py` | ok / 1276.9 ms | exit 1 / 1665.9 ms | hydro_fail | error[HYD0002]: decorators are not supported in Phase 1 |
| `test_marshal.py` | ok / 223.2 ms | exit 1 / 454.5 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_math.py` | ok / 9054 ms | exit 1 / 523.3 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_math_property.py` | ok / 35.2 ms | exit 1 / 702.3 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_memoryio.py` | ok / 98.9 ms | exit 1 / 754.5 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_memoryview.py` | ok / 289.8 ms | exit 1 / 474 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_metaclass.py` | ok / 123 ms | exit 1 / 539.6 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_mimetypes.py` | ok / 426.2 ms | exit 1 / 626.9 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_minidom.py` | ok / 84.2 ms | exit 1 / 533.4 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_mmap.py` | ok / 109.3 ms | exit 1 / 752.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_modulefinder.py` | ok / 274.4 ms | exit 1 / 949.7 ms | hydro_fail | error[HYD0002]: decorators are not supported in Phase 1 |
| `test_monitoring.py` | ok / 61.6 ms | exit 1 / 534.3 ms | hydro_fail | error[HYD0201]: module '_opcode' has no attribute 'stack_effect' |
| `test_msvcrt.py` | exit 1 / 35.3 ms | exit 1 / 415 ms | expected_fail | error[HYD0002]: module '_winapi' not found: not supported on the import search path |
| `test_multibytecodec.py` | ok / 217.5 ms | exit 1 / 427.2 ms | hydro_fail | error[HYD0002]: module '_multibytecodec' not found: not supported on the import search path |
| `test_multiprocessing_main_handling.py` | exit 1 / 1271.2 ms | exit 1 / 740.3 ms | expected_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_named_expressions.py` | ok / 49.4 ms | exit 1 / 230.2 ms | hydro_fail | error[HYD0101]: name 'exec' is not defined |
| `test_netrc.py` | ok / 51.7 ms | exit 1 / 443.2 ms | hydro_fail | error[HYD0201]: module 'os.path' has no attribute 'expanduser' |
| `test_ntpath.py` | ok / 275.4 ms | exit 1 / 838.3 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_nturl2path.py` | ok / 55.5 ms | exit 1 / 1312.6 ms | hydro_fail | error[HYD0002]: decorators are not supported in Phase 1 |
| `test_numeric_tower.py` | ok / 65.7 ms | exit 1 / 10.9 ms | hydro_fail | error[HYD0002]: 'from _decimal import *' is not supported for a module that is not loaded |
| `test_opcache.py` | ok / 282.3 ms | exit 1 / 552.9 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_opcodes.py` | ok / 47.1 ms | exit 1 / 697.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_openpty.py` | ok / 48.2 ms | exit 1 / 3.9 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'openpty' |
| `test_operator.py` | ok / 81.3 ms | exit 1 / 759.9 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_optimizer.py` | ok / 45.7 ms | exit 1 / 716.7 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_optparse.py` | ok / 122.7 ms | exit 1 / 585 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_ordered_dict.py` | ok / 174.6 ms | exit 1 / 485.6 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_os.py` | exit 1 / 12665.7 ms | exit 1 / 674.2 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_osx_env.py` | ok / 44.6 ms | exit 1 / 693.7 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_patma.py` | exit 1 / 48.8 ms | exit 1 / 485.9 ms | expected_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_pdb.py` | exit 1 / 17418 ms | exit 1 / 978.3 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_peepholer.py` | ok / 355.5 ms | exit 1 / 519.9 ms | hydro_fail | error[HYD0201]: module '_opcode' has no attribute 'stack_effect' |
| `test_pep646_syntax.py` | ok / 219 ms | exit 1 / 548.9 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_perf_profiler.py` | ok / 394.5 ms | exit 1 / 465.7 ms | hydro_fail | error[HYD0201]: module 'collections' has no attribute 'ChainMap' |
| `test_perfmaps.py` | ok / 104.6 ms | ok / 59.9 ms | hydro_pass | - |
| `test_pickle.py` | ok / 2070.3 ms | exit 1 / 1083.5 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_picklebuffer.py` | ok / 128 ms | exit 1 / 535.4 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_pickletools.py` | ok / 528.9 ms | exit 1 / 1024.9 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_pkg.py` | ok / 118 ms | exit 1 / 29.6 ms | hydro_fail | error[HYD0002]: decorators are not supported in Phase 1 |
| `test_pkgutil.py` | exit 1 / 137.7 ms | exit 1 / 561.9 ms | expected_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_platform.py` | ok / 466.9 ms | exit 1 / 519.5 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_plistlib.py` | ok / 225.3 ms | exit 1 / 490.3 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_poll.py` | ok / 11275.2 ms | exit 1 / 706.5 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_popen.py` | ok / 165.4 ms | exit 1 / 705.1 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_poplib.py` | ok / 6006 ms | exit 1 / 515.5 ms | hydro_fail | error[HYD0201]: module 'sys' has no attribute 'audit' |
| `test_positional_only_arg.py` | ok / 132.9 ms | exit 1 / 450.3 ms | hydro_fail | error[HYD0201]: module '_opcode' has no attribute 'stack_effect' |
| `test_posix.py` | exit 1 / 815 ms | exit 1 / 532.5 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_posixpath.py` | ok / 322.2 ms | exit 1 / 852.2 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_pow.py` | ok / 239 ms | exit 1 / 26.9 ms | hydro_fail | error[HYD0101]: name 'pow' is not defined |
| `test_pprint.py` | ok / 170.4 ms | exit 1 / 621 ms | hydro_fail | error[HYD0201]: module 'io' has no attribute 'StringIO' |
| `test_print.py` | ok / 119.6 ms | exit 1 / 687.5 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_profile.py` | ok / 370 ms | exit 1 / 626.9 ms | hydro_fail | error[HYD0201]: module 'enum' has no attribute 'StrEnum' |
| `test_property.py` | ok / 88.2 ms | exit 1 / 727.1 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_pstats.py` | ok / 157.2 ms | exit 1 / 463.7 ms | hydro_fail | error[HYD0002]: module '_lsprof' not found: not supported on the import search path |
| `test_pty.py` | ok / 103.4 ms | exit 1 / 447.8 ms | hydro_fail | error[HYD0002]: module 'termios' not found: not supported on the import search path |
| `test_pulldom.py` | ok / 153 ms | exit 1 / 457.2 ms | hydro_fail | error[HYD0101]: unknown exception type 'exception' |
| `test_pwd.py` | ok / 111 ms | exit 1 / 479.9 ms | hydro_fail | error[HYD0201]: module 'collections' has no attribute 'ChainMap' |
| `test_py_compile.py` | ok / 340.8 ms | exit 1 / 927.9 ms | hydro_fail | error[HYD0002]: decorators are not supported in Phase 1 |
| `test_pyclbr.py` | exit 1 / 90.8 ms | exit 1 / 959.4 ms | expected_fail | error[HYD0002]: decorators are not supported in Phase 1 |
| `test_pyexpat.py` | ok / 222.4 ms | exit 1 / 503.5 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_queue.py` | ok / 7829.5 ms | exit 1 / 509.5 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_quopri.py` | ok / 131.6 ms | exit 1 / 724.3 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_raise.py` | ok / 90.6 ms | exit 1 / 694.7 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_random.py` | ok / 719.5 ms | exit 1 / 589.1 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_range.py` | ok / 198.7 ms | exit 1 / 743.1 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_re.py` | ok / 426.8 ms | exit 1 / 868.2 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_readline.py` | ok / 240.9 ms | exit 1 / 645.8 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_regrtest.py` | exit 1 / 1495.5 ms | exit 1 / 876.2 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_remote_pdb.py` | ok / 2402.3 ms | exit 1 / 733.6 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_repl.py` | ok / 920.1 ms | exit 1 / 768.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_reprlib.py` | ok / 68 ms | exit 1 / 726.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_resource.py` | ok / 84.4 ms | exit 1 / 732.3 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_richcmp.py` | ok / 39.5 ms | exit 1 / 706.1 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_rlcompleter.py` | ok / 112.2 ms | exit 1 / 577 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_robotparser.py` | ok / 1359.3 ms | exit 1 / 737.7 ms | hydro_fail | error[HYD0101]: name 'super' is not defined |
| `test_runpy.py` | timeout / 30010.9 ms | exit 1 / 482.1 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_sax.py` | ok / 177.1 ms | exit 1 / 721.7 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_sched.py` | ok / 587.9 ms | exit 1 / 437.9 ms | hydro_fail | error[HYD0201]: module 'collections' has no attribute 'namedtuple' |
| `test_scope.py` | ok / 45.8 ms | exit 1 / 494.9 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_script_helper.py` | ok / 188.9 ms | exit 1 / 738.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_secrets.py` | ok / 39.5 ms | exit 1 / 35.5 ms | hydro_fail | error[HYD0201]: module '_operator' has no attribute '_compare_digest' |
| `test_select.py` | ok / 1556.2 ms | exit 1 / 696.7 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_selectors.py` | ok / 14695.4 ms | exit 1 / 591 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_set.py` | ok / 713.1 ms | exit 1 / 547.9 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_setcomps.py` | ok / 120.3 ms | exit 1 / 614.6 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_shelve.py` | ok / 2580.9 ms | exit 1 / 539.3 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_shlex.py` | ok / 155.5 ms | exit 1 / 659.5 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_shutil.py` | exit 1 / 422.8 ms | exit 1 / 812.5 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_signal.py` | exit 1 / 15518.7 ms | exit 1 / 661.8 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_site.py` | ok / 2085.4 ms | exit 1 / 1164.5 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_slice.py` | ok / 62.1 ms | exit 1 / 540.4 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_smtplib.py` | ok / 1448 ms | exit 1 / 1024.4 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_smtpnet.py` | ok / 9206.5 ms | exit 1 / 1027.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_socket.py` | ok / 27916.4 ms | exit 1 / 698.9 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_socketserver.py` | ok / 483.4 ms | exit 1 / 471 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_sort.py` | ok / 77 ms | exit 1 / 738.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_source_encoding.py` | ok / 13358.7 ms | exit 1 / 782.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_ssl.py` | ok / 21594.4 ms | exit 1 / 1056.4 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_stable_abi_ctypes.py` | exit 1 / 130 ms | exit 1 / 785.3 ms | expected_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_startfile.py` | exit 1 / 116.2 ms | exit 1 / 758.6 ms | expected_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_stat.py` | ok / 146.2 ms | exit 1 / 761.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_statistics.py` | ok / 9563.6 ms | exit 1 / 842.1 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_str.py` | ok / 1132.3 ms | exit 1 / 642.8 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_strftime.py` | ok / 167 ms | exit 1 / 676.6 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_string_literals.py` | ok / 142.8 ms | exit 1 / 92.7 ms | hydro_fail | error[HYD0101]: name 'eval' is not defined |
| `test_stringprep.py` | ok / 116.2 ms | exit 1 / 40.3 ms | hydro_fail | error[HYD0002]: module 'unicodedata' not found: not supported on the import search path |
| `test_strptime.py` | ok / 148.2 ms | exit 1 / 718.4 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_strtod.py` | ok / 244.4 ms | exit 1 / 749.2 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_struct.py` | ok / 393.9 ms | exit 1 / 534.9 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_structseq.py` | ok / 163.6 ms | exit 1 / 556.5 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_subclassinit.py` | ok / 125 ms | exit 1 / 21.2 ms | hydro_fail | error[HYD0002]: nested class definitions are not supported yet |
| `test_subprocess.py` | exit 1 / 15700.5 ms | exit 1 / 574.4 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_sundry.py` | ok / 104.5 ms | exit 1 / 772.5 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_super.py` | ok / 214.2 ms | exit 1 / 643.9 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_support.py` | exit 1 / 4947.2 ms | exit 1 / 516.3 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_symtable.py` | ok / 63.8 ms | exit 1 / 480.5 ms | hydro_fail | error[HYD0002]: module '_symtable' not found: not supported on the import search path |
| `test_syntax.py` | ok / 190.2 ms | exit 1 / 699 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_sys.py` | exit 1 / 1217.9 ms | exit 1 / 584.5 ms | expected_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_sys_setprofile.py` | ok / 47 ms | exit 1 / 196.7 ms | hydro_fail | error[HYD0201]: module 'io' has no attribute 'StringIO' |
| `test_sys_settrace.py` | ok / 850.5 ms | exit 1 / 839.9 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_sysconfig.py` | exit 1 / 222 ms | exit 1 / 565.1 ms | expected_fail | error[HYD0002]: circular import: test.support.venv -> venv |
| `test_syslog.py` | ok / 197.1 ms | exit 1 / 794.9 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_tabnanny.py` | ok / 120.4 ms | exit 1 / 631 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_tarfile.py` | exit 1 / 5640.1 ms | exit 1 / 834.9 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_tcl.py` | ok / 120.7 ms | exit 1 / 592.2 ms | hydro_fail | error[HYD0002]: module '_tkinter' not found: not supported on the import search path |
| `test_tempfile.py` | ok / 389.9 ms | exit 1 / 581.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_termios.py` | ok / 562.3 ms | exit 1 / 762.3 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_textwrap.py` | ok / 49.2 ms | exit 1 / 317.8 ms | hydro_fail | error[HYD0201]: module 'textwrap' has no attribute 'TextWrapper' |
| `test_thread.py` | ok / 1285.4 ms | exit 1 / 787.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_thread_local_bytecode.py` | ok / 51.6 ms | exit 1 / 767 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_threadedtempfile.py` | ok / 55.5 ms | exit 1 / 740.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_threading.py` | exit 1 / 14139.9 ms | exit 1 / 580.9 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_threading_local.py` | ok / 345.4 ms | exit 1 / 686.7 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_threadsignals.py` | ok / 4961.8 ms | exit 1 / 437.6 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_time.py` | ok / 3084.4 ms | exit 1 / 748.9 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_timeit.py` | ok / 60.8 ms | exit 1 / 738.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_timeout.py` | ok / 10131.2 ms | exit 1 / 735.7 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_tokenize.py` | ok / 6422.8 ms | exit 1 / 550.7 ms | hydro_fail | error[HYD0201]: module 'collections' has no attribute 'ChainMap' |
| `test_trace.py` | ok / 1010.5 ms | exit 1 / 797.9 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_traceback.py` | exit 1 / 2336.2 ms | exit 1 / 736.6 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_tracemalloc.py` | ok / 685.1 ms | exit 1 / 671.6 ms | hydro_fail | error[HYD0002]: module '_tracemalloc' not found: not supported on the import search path |
| `test_tstring.py` | exit 1 / 293.2 ms | exit 1 / 784.6 ms | expected_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_ttk_textonly.py` | ok / 75.2 ms | exit 1 / 583.7 ms | hydro_fail | error[HYD0002]: module '_tkinter' not found: not supported on the import search path |
| `test_tty.py` | ok / 49 ms | exit 1 / 751.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_tuple.py` | ok / 223 ms | exit 1 / 765.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_turtle.py` | ok / 192.5 ms | exit 1 / 626.6 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_type_aliases.py` | ok / 55.9 ms | exit 1 / 715.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_type_annotations.py` | ok / 50.6 ms | exit 1 / 773.6 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_type_cache.py` | exit 1 / 47.7 ms | exit 1 / 483.8 ms | expected_fail | error[HYD0201]: module '_opcode' has no attribute 'stack_effect' |
| `test_type_comments.py` | ok / 53.9 ms | exit 1 / 760.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_type_params.py` | ok / 59.1 ms | exit 1 / 36.6 ms | hydro_fail | error[HYD0001]: invalid syntax: invalid syntax. Got unexpected token '=' |
| `test_typechecks.py` | ok / 44.7 ms | exit 1 / 230.2 ms | hydro_fail | AssertionError: 4 test(s) failed |
| `test_types.py` | ok / 232.7 ms | exit 1 / 752.6 ms | hydro_fail | error[HYD0002]: module '_datetime' not found: not supported on the import search path |
| `test_typing.py` | ok / 398.4 ms | exit 1 / 977 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_ucn.py` | ok / 148.7 ms | exit 1 / 639.7 ms | hydro_fail | error[HYD0002]: module 'unicodedata' not found: not supported on the import search path |
| `test_unary.py` | ok / 42.6 ms | exit 1 / 7.4 ms | hydro_fail | error[HYD0002]: complex literals are not supported in Phase 1 |
| `test_unicode_file.py` | ok / 48.1 ms | exit 1 / 461.9 ms | hydro_fail | error[HYD0002]: module 'unicodedata' not found: not supported on the import search path |
| `test_unicode_file_functions.py` | ok / 75 ms | exit 1 / 446.7 ms | hydro_fail | error[HYD0002]: module 'unicodedata' not found: not supported on the import search path |
| `test_unicode_identifiers.py` | ok / 42.9 ms | exit 1 / 4.7 ms | hydro_fail | error[HYD0002]: nested class definitions are not supported yet |
| `test_unicodedata.py` | ok / 4434.5 ms | exit 1 / 710.9 ms | hydro_fail | error[HYD0002]: module 'unicodedata' not found: not supported on the import search path |
| `test_univnewlines.py` | ok / 59.9 ms | exit 1 / 662.8 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_unpack.py` | exit 1 / 144.8 ms | exit 1 / 552.1 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_unpack_ex.py` | exit 1 / 150.7 ms | exit 1 / 532.4 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_unparse.py` | ok / 6225.2 ms | exit 1 / 766.1 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_urllib.py` | ok / 406.3 ms | exit 1 / 1601.8 ms | hydro_fail | error[HYD0002]: decorators are not supported in Phase 1 |
| `test_urllib2.py` | ok / 1500.5 ms | exit 1 / 839 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_urllib2_localnet.py` | ok / 2164.8 ms | exit 1 / 1671.7 ms | hydro_fail | error[HYD0002]: decorators are not supported in Phase 1 |
| `test_urllib2net.py` | timeout / 30003.5 ms | exit 1 / 839.8 ms | expected_fail | error[HYD0201]: module 'os.path' has no attribute 'realpath' |
| `test_urllib_response.py` | ok / 53.7 ms | exit 1 / 448.3 ms | hydro_fail | error[HYD0101]: name 'super' is not defined |
| `test_urllibnet.py` | ok / 7746.8 ms | exit 1 / 976.3 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_urlparse.py` | ok / 132.5 ms | exit 1 / 563.3 ms | hydro_fail | error[HYD0002]: module 'unicodedata' not found: not supported on the import search path |
| `test_userdict.py` | ok / 146.7 ms | exit 1 / 778.4 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_userlist.py` | ok / 136.8 ms | exit 1 / 780.5 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_userstring.py` | ok / 598.2 ms | exit 1 / 531.5 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_utf8_mode.py` | ok / 429.1 ms | exit 1 / 696.7 ms | hydro_fail | error[HYD0101]: name 'id' is not defined |
| `test_utf8source.py` | ok / 43.4 ms | exit 1 / 5.1 ms | hydro_fail | error[HYD0101]: name 'compile' is not defined |
| `test_uuid.py` | ok / 274.3 ms | exit 1 / 591.3 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_venv.py` | exit 1 / 6904.5 ms | exit 1 / 653.6 ms | expected_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_wait3.py` | ok / 5065.9 ms | exit 1 / 745.3 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_wait4.py` | ok / 5057.7 ms | exit 1 / 748 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_wave.py` | ok / 70.8 ms | exit 1 / 493.9 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_weakref.py` | exit 1 / 2509.3 ms | exit 1 / 797.7 ms | expected_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_weakset.py` | ok / 73.1 ms | exit 1 / 556 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_webbrowser.py` | ok / 2277.3 ms | exit 1 / 661.8 ms | hydro_fail | error[HYD0201]: module 'io' has no attribute 'StringIO' |
| `test_winapi.py` | exit 1 / 63.8 ms | exit 1 / 924.1 ms | expected_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_winconsoleio.py` | exit 1 / 64.9 ms | exit 1 / 523.6 ms | expected_fail | error[HYD0002]: module '_testconsole' not found: not supported on the import search path |
| `test_winreg.py` | exit 1 / 75.4 ms | exit 1 / 570.9 ms | expected_fail | error[HYD0002]: module 'winreg' not found: not supported on the import search path |
| `test_winsound.py` | exit 1 / 57.5 ms | exit 1 / 902.2 ms | expected_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_with.py` | ok / 53.1 ms | exit 1 / 132.6 ms | hydro_fail | error[HYD0201]: module 'os' has no attribute 'stat' |
| `test_wmi.py` | exit 1 / 50.9 ms | exit 1 / 757.8 ms | expected_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_wsgiref.py` | ok / 222.5 ms | exit 1 / 873.5 ms | hydro_fail | error[HYD0002]: module '_signal' not found: not supported on the import search path |
| `test_xml_dom_minicompat.py` | ok / 55.4 ms | exit 1 / 107.1 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_xml_dom_xmlbuilder.py` | ok / 172.7 ms | exit 1 / 807.2 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |
| `test_xml_etree.py` | ok / 803.4 ms | exit 1 / 783.6 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_xml_etree_c.py` | ok / 516.8 ms | exit 1 / 473.5 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_xmlrpc.py` | ok / 27422.6 ms | exit 1 / 972.7 ms | hydro_fail | error[HYD0002]: module 'pyexpat' not found: not supported on the import search path |
| `test_xpickle.py` | ok / 2241.4 ms | exit 1 / 903.5 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_xxlimited.py` | exit 1 / 51.5 ms | exit 1 / 817.7 ms | expected_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_xxtestfuzz.py` | ok / 55 ms | exit 1 / 455.8 ms | hydro_fail | error[HYD0002]: module 'faulthandler' not found: not supported on the import search path |
| `test_yield_from.py` | ok / 54 ms | exit 1 / 757.7 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_zipapp.py` | ok / 149.5 ms | exit 1 / 713 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_zipfile64.py` | timeout / 30029.6 ms | exit 1 / 547.4 ms | expected_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_zipimport.py` | ok / 18909.9 ms | exit 1 / 949.1 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_zipimport_support.py` | ok / 511.4 ms | exit 1 / 825.3 ms | hydro_fail | error[HYD0002]: module '_struct' not found: not supported on the import search path |
| `test_zlib.py` | ok / 419.9 ms | exit 1 / 801.8 ms | hydro_fail | error[HYD0002]: class body may only contain method definitions, field declarations and attributes (plus pass… |
| `test_zstd.py` | ok / 375.7 ms | exit 1 / 550.9 ms | hydro_fail | error[HYD0002]: module 'array' not found: not supported on the import search path |

<!-- END GENERATED RESULTS -->
