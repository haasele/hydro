# CPython test suite gate

PyX targets **CPython 3.14.9**. Behavioural compatibility is measured against
the unmodified CPython standard-library test suite (`Lib/test/test_*.py`) from a
CPython 3.14.9 source tree. Every selected test file is executed twice:

1. by the pinned CPython 3.14.9 interpreter (the differential oracle), with
   `cwd=<source>/Lib` and `PYTHONPATH=<source>/Lib`;
2. by PyX (`pyx run --color=never <testfile>`), from the repository root.

A test counts as passing **only when both interpreters exit 0**. If PyX exits
non-zero while CPython passes, the test is a PyX failure; if both fail, the
result is `expected_fail`; if CPython fails while PyX passes, the row is
`cpython_fail` and cannot be used as evidence of compatibility. A PyX timeout is
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

The oracle is resolved from `--cpython PATH`, else `$PYX_CPYTHON`, else
`uv python find 3.14.9`. When none is available the harness prints the remedy:

```sh
uv python install 3.14.9
```

The source tree comes from `--source PATH`, else the cache at
`.pyx/cpython/Python-3.14.9`. On first use the harness downloads
`https://www.python.org/ftp/python/3.14.9/Python-3.14.9.tgz`, prints its
SHA-256, and reuses the extraction on later runs. The cache is ignored by Git.
`--gate` and `--check-manifest` never download; run the harness once (or pass
`--source`) before relying on the gate in an offline environment.

## Commands

```sh
# Offline manifest validation only: no download, no CPython/PyX runs.
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
  "minimum_passing": 0,
  "required": [],
  "known_passing": [],
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

## Why `required` starts empty

The initial manifest deliberately claims **zero** passing tests. PyX does not
yet implement enough of the stdlib (`unittest`, `test.support`, imports, the
CPython test package layout) for official `Lib/test` modules to run, and
inventing passes would defeat the purpose of a differential gate. The empty
`required` list is an honest baseline, not a completed checklist. The separate
import-only baseline in [`COMPATIBILITY.md`](COMPATIBILITY.md) tracks module
imports and makes no test-suite claim.

Once a test genuinely passes under both interpreters, record it:

```sh
python3 scripts/cpython_compat.py --update-manifest
```

Then add the tests that must never regress to `required` and raise
`minimum_passing` as appropriate. From that point the gate is a release
blocker.

## Generated results

<!-- BEGIN GENERATED RESULTS -->

Environment: CPython `3.14.9` (`/home/aura/.local/share/uv/python/cpython-3.14.9-linux-x86_64-gnu/bin/python3.14`) · PyX `pyx 0.1.0` (`/mnt/Vault/GitHub/pyx/target/release/pyx`)

Source: `/mnt/Vault/GitHub/pyx/.pyx/cpython/Python-3.14.9` · generated 2026-10-24T21:13:36Z

## Summary

| Status | Tests |
| --- | ---: |
| pyx_pass | 0 |
| pyx_fail | 322 |
| cpython_fail | 0 |
| expected_fail | 76 |
| timeout | 0 |
| **Total** | 398 |

Passing: **0 / 398** (a pass requires PyX and CPython 3.14.9 to both exit 0).

## Passing tests

No tests pass yet. This is expected while the stdlib and unittest prerequisites are missing; do not add tests to the manifest's `required` list until they genuinely pass.

## Failure classes

| Status | Tests |
| --- | ---: |
| pyx_fail | 322 |
| expected_fail | 76 |

## Test results

| Test | CPython 3.14.9 | PyX | Status | First error |
| --- | --- | --- | --- | --- |
| `test___all__.py` | ok / 1915.7 ms | exit 1 / 2.2 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test__colorize.py` | ok / 257.9 ms | exit 1 / 3.2 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test__interpchannels.py` | ok / 1500.7 ms | exit 1 / 6.7 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test__interpreters.py` | ok / 1782.9 ms | exit 1 / 5.3 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test__locale.py` | ok / 174.6 ms | exit 1 / 4.3 ms | pyx_fail | error[PYX0002]: module '_locale' not found: not supported unless a sibling _locale.py exists |
| `test__opcode.py` | ok / 172.6 ms | exit 1 / 3.1 ms | pyx_fail | error[PYX0002]: module 'dis' not found: not supported unless a sibling dis.py exists |
| `test__osx_support.py` | ok / 186.3 ms | exit 1 / 2.8 ms | pyx_fail | error[PYX0002]: module 'stat' not found: not supported unless a sibling stat.py exists |
| `test_abc.py` | ok / 164.9 ms | exit 1 / 4.2 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_abstract_numbers.py` | ok / 154.8 ms | exit 1 / 2.9 ms | pyx_fail | error[PYX0002]: module 'abc' not found: not supported unless a sibling abc.py exists |
| `test_android.py` | exit 1 / 243.8 ms | exit 1 / 3.8 ms | expected_fail | error[PYX0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_annotationlib.py` | ok / 204.5 ms | exit 1 / 2.8 ms | pyx_fail | error[PYX0001]: invalid syntax: invalid syntax. Got unexpected token "{a}" |
| `test_apple.py` | exit 1 / 236.3 ms | exit 1 / 1.8 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_argparse.py` | ok / 2550.6 ms | exit 1 / 27.6 ms | pyx_fail | error[PYX0002]: module '_colorize' not found: not supported unless a sibling _colorize.py exists |
| `test_array.py` | ok / 519.1 ms | exit 1 / 10.7 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'collections.abc' |
| `test_asdl_parser.py` | exit 1 / 149.9 ms | exit 1 / 2.5 ms | expected_fail | error[PYX0002]: dotted imports are not supported for user modules: 'importlib.machinery' |
| `test_asyncgen.py` | ok / 1047.7 ms | exit 1 / 8.5 ms | pyx_fail | error[PYX0002]: module 'inspect' not found: not supported unless a sibling inspect.py exists |
| `test_atexit.py` | exit 1 / 307.2 ms | exit 1 / 2.8 ms | expected_fail | error[PYX0002]: module 'atexit' not found: not supported unless a sibling atexit.py exists |
| `test_audit.py` | ok / 5636.5 ms | exit 1 / 3.5 ms | pyx_fail | error[PYX0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_augassign.py` | ok / 178.7 ms | exit 1 / 3.3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_base64.py` | ok / 403.7 ms | exit 1 / 8 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_baseexception.py` | ok / 216.6 ms | exit 1 / 2.8 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_bdb.py` | ok / 316.1 ms | exit 1 / 6.3 ms | pyx_fail | error[PYX0002]: module 'bdb' not found: not supported unless a sibling bdb.py exists |
| `test_bigaddrspace.py` | ok / 414.5 ms | exit 1 / 16.7 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_bigmem.py` | ok / 431.5 ms | exit 1 / 9.3 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_binascii.py` | ok / 240.9 ms | exit 1 / 5 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_binop.py` | ok / 181.5 ms | exit 1 / 3.9 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_bisect.py` | ok / 212.9 ms | exit 1 / 4.3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_bool.py` | ok / 208.7 ms | exit 1 / 3.6 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_buffer.py` | ok / 246.3 ms | exit 1 / 30.5 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_bufio.py` | ok / 275.9 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_build_details.py` | exit 1 / 360 ms | exit 1 / 6.1 ms | expected_fail | error[PYX0002]: module 'importlib' not found: not supported unless a sibling importlib.py exists |
| `test_builtin.py` | ok / 859.6 ms | exit 1 / 19.3 ms | pyx_fail | error[PYX0002]: module 'ast' not found: not supported unless a sibling ast.py exists |
| `test_bytes.py` | exit 1 / 252.5 ms | exit 1 / 15.4 ms | expected_fail | error[PYX0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_bz2.py` | ok / 613.8 ms | exit 1 / 8 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_c_locale_coercion.py` | ok / 1472.5 ms | exit 1 / 3.3 ms | pyx_fail | error[PYX0002]: module 'locale' not found: not supported unless a sibling locale.py exists |
| `test_calendar.py` | ok / 1570.8 ms | exit 1 / 6.3 ms | pyx_fail | error[PYX0002]: module 'calendar' not found: not supported unless a sibling calendar.py exists |
| `test_call.py` | exit 1 / 186.1 ms | exit 1 / 5.6 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_charmapcodec.py` | ok / 139.8 ms | exit 1 / 1.9 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_class.py` | ok / 190.5 ms | exit 1 / 5.3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_clinic.py` | ok / 357.5 ms | exit 1 / 15.9 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_cmath.py` | ok / 215.9 ms | exit 1 / 4.1 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_cmd.py` | ok / 362.1 ms | exit 1 / 2.4 ms | pyx_fail | error[PYX0002]: module 'cmd' not found: not supported unless a sibling cmd.py exists |
| `test_cmd_line.py` | exit 1 / 2746.6 ms | exit 1 / 6.8 ms | expected_fail | error[PYX0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_cmd_line_script.py` | exit 1 / 1613.8 ms | exit 1 / 5 ms | expected_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_code.py` | ok / 282.5 ms | exit 1 / 6.2 ms | pyx_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_code_module.py` | ok / 198 ms | exit 1 / 3.6 ms | pyx_fail | error[PYX0002]: module 'traceback' not found: not supported unless a sibling traceback.py exists |
| `test_codeccallbacks.py` | ok / 93.5 ms | exit 1 / 5.9 ms | pyx_fail | error[PYX0002]: module '_codecs' not found: not supported unless a sibling _codecs.py exists |
| `test_codecencodings_cn.py` | ok / 179.5 ms | exit 1 / 2.7 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecencodings_hk.py` | ok / 151.8 ms | exit 1 / 1.8 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecencodings_iso2022.py` | ok / 211.2 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecencodings_jp.py` | ok / 229.1 ms | exit 1 / 2.4 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecencodings_kr.py` | ok / 169 ms | exit 1 / 2.4 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecencodings_tw.py` | ok / 156.8 ms | exit 1 / 1.6 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecmaps_cn.py` | ok / 4134.4 ms | exit 1 / 2.3 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecmaps_hk.py` | ok / 997.2 ms | exit 1 / 1.5 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecmaps_jp.py` | ok / 4932 ms | exit 1 / 1.6 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecmaps_kr.py` | ok / 3086.1 ms | exit 1 / 1.9 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecmaps_tw.py` | ok / 1932.4 ms | exit 1 / 1.8 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_codecs.py` | ok / 411.1 ms | exit 1 / 14.2 ms | pyx_fail | error[PYX0002]: module 'codecs' not found: not supported unless a sibling codecs.py exists |
| `test_codeop.py` | ok / 119.1 ms | exit 1 / 3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_collections.py` | ok / 1274 ms | exit 1 / 11.8 ms | pyx_fail | error[PYX0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_colorsys.py` | ok / 78.8 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_compare.py` | ok / 111.1 ms | exit 1 / 3.7 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_compile.py` | ok / 2470.7 ms | exit 1 / 14.7 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_compileall.py` | exit 1 / 135.6 ms | exit 1 / 7 ms | expected_fail | error[PYX0002]: module 'compileall' not found: not supported unless a sibling compileall.py exists |
| `test_compiler_assemble.py` | ok / 91 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: module 'dis' not found: not supported unless a sibling dis.py exists |
| `test_compiler_codegen.py` | ok / 98.3 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: module 'textwrap' not found: not supported unless a sibling textwrap.py exists |
| `test_complex.py` | ok / 132.1 ms | exit 1 / 7.2 ms | pyx_fail | error[PYX0002]: module 'errno' not found: not supported unless a sibling errno.py exists |
| `test_configparser.py` | ok / 517.9 ms | exit 1 / 11.2 ms | pyx_fail | error[PYX0002]: module 'configparser' not found: not supported unless a sibling configparser.py exists |
| `test_contains.py` | ok / 96.8 ms | exit 1 / 2.1 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_context.py` | ok / 647.5 ms | exit 1 / 6.1 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'collections.abc' |
| `test_contextlib.py` | ok / 130.6 ms | exit 1 / 6 ms | pyx_fail | error[PYX0002]: module 'tempfile' not found: not supported unless a sibling tempfile.py exists |
| `test_contextlib_async.py` | ok / 129.8 ms | exit 1 / 4.5 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_copy.py` | ok / 133.8 ms | exit 1 / 5.7 ms | pyx_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_copyreg.py` | ok / 162.9 ms | exit 1 / 2.3 ms | pyx_fail | error[PYX0002]: module 'copyreg' not found: not supported unless a sibling copyreg.py exists |
| `test_coroutines.py` | ok / 355.6 ms | exit 1 / 8.2 ms | pyx_fail | error[PYX0002]: module 'ast' not found: not supported unless a sibling ast.py exists |
| `test_cprofile.py` | ok / 618.3 ms | exit 1 / 2.7 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_crossinterp.py` | exit 1 / 116 ms | exit 1 / 6.4 ms | expected_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_csv.py` | ok / 334.8 ms | exit 1 / 8.9 ms | pyx_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_curses.py` | ok / 382.3 ms | exit 1 / 9.3 ms | pyx_fail | error[PYX0002]: module 'inspect' not found: not supported unless a sibling inspect.py exists |
| `test_datetime.py` | timeout / 10003.7 ms | exit 1 / 2.4 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_dbm.py` | ok / 435.2 ms | exit 1 / 3.1 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_dbm_dumb.py` | ok / 123.1 ms | exit 1 / 3.4 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_dbm_gnu.py` | exit 1 / 98.5 ms | exit 1 / 2.5 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_dbm_ndbm.py` | ok / 158.2 ms | exit 1 / 2.6 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_dbm_sqlite3.py` | ok / 654.8 ms | exit 1 / 3.6 ms | pyx_fail | error[PYX0002]: module 'stat' not found: not supported unless a sibling stat.py exists |
| `test_decimal.py` | ok / 2064 ms | exit 1 / 26 ms | pyx_fail | error[PYX0002]: module 'logging' not found: not supported unless a sibling logging.py exists |
| `test_decorators.py` | ok / 73.9 ms | exit 1 / 3.2 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_defaultdict.py` | ok / 80 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_deque.py` | ok / 473.3 ms | exit 1 / 6.7 ms | pyx_fail | error[PYX0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_descr.py` | exit 1 / 394.9 ms | exit 1 / 22.4 ms | expected_fail | error[PYX0002]: module 'builtins' not found: not supported unless a sibling builtins.py exists |
| `test_descrtut.py` | ok / 207 ms | exit 1 / 2.4 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_devpoll.py` | exit 1 / 96.9 ms | exit 1 / 2.5 ms | expected_fail | error[PYX0002]: module 'select' not found: not supported unless a sibling select.py exists |
| `test_dict.py` | ok / 376.7 ms | exit 1 / 7.9 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'collections.abc' |
| `test_dictcomps.py` | ok / 113 ms | exit 1 / 3.9 ms | pyx_fail | error[PYX0002]: module 'traceback' not found: not supported unless a sibling traceback.py exists |
| `test_dictviews.py` | ok / 245.3 ms | exit 1 / 5.4 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'collections.abc' |
| `test_difflib.py` | ok / 381.1 ms | exit 1 / 5.1 ms | pyx_fail | error[PYX0002]: module 'difflib' not found: not supported unless a sibling difflib.py exists |
| `test_dis.py` | ok / 364.7 ms | exit 1 / 13.1 ms | pyx_fail | error[PYX0002]: module 'ast' not found: not supported unless a sibling ast.py exists |
| `test_docxmlrpc.py` | ok / 2261.4 ms | exit 1 / 2.6 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'xmlrpc.server' |
| `test_dtrace.py` | exit 1 / 146.3 ms | exit 1 / 4.2 ms | expected_fail | error[PYX0002]: module 'dis' not found: not supported unless a sibling dis.py exists |
| `test_dynamic.py` | ok / 130.9 ms | exit 1 / 3.1 ms | pyx_fail | error[PYX0002]: module 'builtins' not found: not supported unless a sibling builtins.py exists |
| `test_dynamicclassattribute.py` | ok / 90.9 ms | exit 1 / 2.9 ms | pyx_fail | error[PYX0002]: module 'abc' not found: not supported unless a sibling abc.py exists |
| `test_eintr.py` | exit 1 / 205.8 ms | exit 1 / 2.1 ms | expected_fail | error[PYX0002]: module 'signal' not found: not supported unless a sibling signal.py exists |
| `test_embed.py` | ok / 156.6 ms | exit 1 / 9.2 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_ensurepip.py` | ok / 267.9 ms | exit 1 / 3.6 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_enum.py` | exit 1 / 785.1 ms | exit 1 / 26.8 ms | expected_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_enumerate.py` | ok / 161.6 ms | exit 1 / 3.6 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_eof.py` | ok / 417.1 ms | exit 1 / 2.6 ms | pyx_fail | error[PYX0002]: module 'codecs' not found: not supported unless a sibling codecs.py exists |
| `test_epoll.py` | ok / 214.7 ms | exit 1 / 3.3 ms | pyx_fail | error[PYX0002]: module 'errno' not found: not supported unless a sibling errno.py exists |
| `test_errno.py` | ok / 79.3 ms | exit 1 / 1.8 ms | pyx_fail | error[PYX0002]: module 'errno' not found: not supported unless a sibling errno.py exists |
| `test_except_star.py` | ok / 111.7 ms | exit 1 / 4.9 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_exception_group.py` | ok / 323.8 ms | exit 1 / 4.9 ms | pyx_fail | error[PYX0002]: module 'types' not found: not supported unless a sibling types.py exists |
| `test_exception_hierarchy.py` | ok / 81 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: module 'builtins' not found: not supported unless a sibling builtins.py exists |
| `test_exception_variations.py` | ok / 80.4 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_exceptions.py` | exit 1 / 1042.2 ms | exit 1 / 10.3 ms | expected_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_extcall.py` | exit 1 / 211 ms | exit 1 / 1.9 ms | expected_fail | error[PYX0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_external_inspection.py` | ok / 1765.1 ms | exit 1 / 6.4 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_faulthandler.py` | timeout / 10005.5 ms | exit 1 / 4.6 ms | expected_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_fcntl.py` | ok / 477.9 ms | exit 1 / 3.8 ms | pyx_fail | error[PYX0002]: module 'errno' not found: not supported unless a sibling errno.py exists |
| `test_file.py` | ok / 171 ms | exit 1 / 3.6 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_file_eintr.py` | ok / 1561 ms | exit 1 / 3 ms | pyx_fail | error[PYX0002]: module 'select' not found: not supported unless a sibling select.py exists |
| `test_filecmp.py` | ok / 138.6 ms | exit 1 / 3.4 ms | pyx_fail | error[PYX0002]: module 'filecmp' not found: not supported unless a sibling filecmp.py exists |
| `test_fileinput.py` | ok / 213.5 ms | exit 1 / 5.3 ms | pyx_fail | error[PYX0002]: module 'fileinput' not found: not supported unless a sibling fileinput.py exists |
| `test_fileio.py` | ok / 628.7 ms | exit 1 / 5.2 ms | pyx_fail | error[PYX0002]: module 'errno' not found: not supported unless a sibling errno.py exists |
| `test_fileutils.py` | ok / 209.5 ms | exit 1 / 2.1 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_finalization.py` | ok / 170.3 ms | exit 1 / 4.1 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_float.py` | ok / 176.8 ms | exit 1 / 10.6 ms | pyx_fail | error[PYX0002]: module 'fractions' not found: not supported unless a sibling fractions.py exists |
| `test_flufl.py` | ok / 93.8 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: module '__future__' not found: not supported unless a sibling __future__.py exists |
| `test_fnmatch.py` | ok / 88.7 ms | exit 1 / 3.6 ms | pyx_fail | error[PYX0002]: module 'string' not found: not supported unless a sibling string.py exists |
| `test_fork1.py` | ok / 5208.9 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: module '_imp' not found: not supported unless a sibling _imp.py exists |
| `test_format.py` | ok / 123.2 ms | exit 1 / 5.9 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_fractions.py` | ok / 151.9 ms | exit 1 / 11.9 ms | pyx_fail | error[PYX0002]: module 'decimal' not found: not supported unless a sibling decimal.py exists |
| `test_frame.py` | exit 1 / 1084 ms | exit 1 / 4.4 ms | expected_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_frozen.py` | ok / 119.5 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'importlib.machinery' |
| `test_fstring.py` | ok / 596.8 ms | exit 1 / 5.1 ms | pyx_fail | error[PYX0001]: invalid syntax: f-string: expecting '}' |
| `test_ftplib.py` | ok / 7382.3 ms | exit 1 / 6.5 ms | pyx_fail | error[PYX0002]: module 'ftplib' not found: not supported unless a sibling ftplib.py exists |
| `test_funcattrs.py` | ok / 117.7 ms | exit 1 / 3.4 ms | pyx_fail | error[PYX0002]: module 'textwrap' not found: not supported unless a sibling textwrap.py exists |
| `test_functools.py` | ok / 460.6 ms | exit 1 / 15.9 ms | pyx_fail | error[PYX0002]: module 'abc' not found: not supported unless a sibling abc.py exists |
| `test_gc.py` | exit 1 / 1601.4 ms | exit 1 / 5.8 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_generated_cases.py` | ok / 188.4 ms | exit 1 / 4.3 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_generator_stop.py` | ok / 74.2 ms | exit 1 / 1.9 ms | pyx_fail | error[PYX0002]: module '__future__' not found: not supported unless a sibling __future__.py exists |
| `test_generators.py` | ok / 263.4 ms | exit 1 / 6.7 ms | pyx_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_genericalias.py` | ok / 282 ms | exit 1 / 6.9 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_genericclass.py` | ok / 111.2 ms | exit 1 / 2.7 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_genericpath.py` | ok / 121.8 ms | exit 1 / 3.9 ms | pyx_fail | error[PYX0002]: module 'genericpath' not found: not supported unless a sibling genericpath.py exists |
| `test_genexps.py` | ok / 212.1 ms | exit 1 / 1.8 ms | pyx_fail | error[PYX0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_getopt.py` | ok / 239.2 ms | exit 1 / 3.2 ms | pyx_fail | error[PYX0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_getpass.py` | ok / 185.8 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: module 'getpass' not found: not supported unless a sibling getpass.py exists |
| `test_getpath.py` | ok / 91.6 ms | exit 1 / 5 ms | pyx_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_gettext.py` | ok / 224.3 ms | exit 1 / 4.9 ms | pyx_fail | error[PYX0002]: module 'base64' not found: not supported unless a sibling base64.py exists |
| `test_glob.py` | ok / 156.2 ms | exit 1 / 5 ms | pyx_fail | error[PYX0002]: module 'glob' not found: not supported unless a sibling glob.py exists |
| `test_global.py` | ok / 105.7 ms | exit 1 / 2.4 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_grammar.py` | ok / 129.9 ms | exit 1 / 5.9 ms | pyx_fail | error[PYX0001]: invalid syntax: invalid syntax. Got unexpected token ',' |
| `test_graphlib.py` | ok / 148.1 ms | exit 1 / 2.9 ms | pyx_fail | error[PYX0002]: module 'graphlib' not found: not supported unless a sibling graphlib.py exists |
| `test_grp.py` | ok / 322.1 ms | exit 1 / 2.1 ms | pyx_fail | error[PYX0002]: module 'string' not found: not supported unless a sibling string.py exists |
| `test_gzip.py` | ok / 986.5 ms | exit 1 / 6.1 ms | pyx_fail | error[PYX0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_hash.py` | ok / 363.3 ms | exit 1 / 3.4 ms | pyx_fail | error[PYX0002]: module 'datetime' not found: not supported unless a sibling datetime.py exists |
| `test_hashlib.py` | timeout / 10004.2 ms | exit 1 / 7.4 ms | expected_fail | error[PYX0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_heapq.py` | ok / 321.1 ms | exit 1 / 4.5 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_hmac.py` | ok / 232.7 ms | exit 1 / 6 ms | pyx_fail | error[PYX0002]: module 'binascii' not found: not supported unless a sibling binascii.py exists |
| `test_html.py` | ok / 77.7 ms | exit 1 / 2.3 ms | pyx_fail | error[PYX0002]: module 'html' not found: not supported unless a sibling html.py exists |
| `test_htmlparser.py` | ok / 554.6 ms | exit 1 / 7.8 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'html.parser' |
| `test_http_cookiejar.py` | ok / 350.3 ms | exit 1 / 8.9 ms | pyx_fail | error[PYX0002]: module 'stat' not found: not supported unless a sibling stat.py exists |
| `test_http_cookies.py` | ok / 787.7 ms | exit 1 / 4 ms | pyx_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_httplib.py` | ok / 3761.9 ms | exit 1 / 11.4 ms | pyx_fail | error[PYX0002]: module 'enum' not found: not supported unless a sibling enum.py exists |
| `test_httpservers.py` | ok / 4386.4 ms | exit 1 / 9 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'http.server' |
| `test_idle.py` | timeout / 10009 ms | exit 1 / 2.6 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_imaplib.py` | timeout / 10004.6 ms | exit 1 / 11.2 ms | expected_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_index.py` | ok / 104.3 ms | exit 1 / 2.9 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_int.py` | timeout / 10004.8 ms | exit 1 / 5.7 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_int_literal.py` | ok / 96.9 ms | exit 1 / 2.4 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_io.py` | timeout / 10005.4 ms | exit 1 / 19.1 ms | expected_fail | error[PYX0002]: module 'abc' not found: not supported unless a sibling abc.py exists |
| `test_ioctl.py` | ok / 606.7 ms | exit 1 / 3.2 ms | pyx_fail | error[PYX0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_ipaddress.py` | ok / 175 ms | exit 1 / 13.5 ms | pyx_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_isinstance.py` | ok / 972 ms | exit 1 / 3.4 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_iter.py` | ok / 136.3 ms | exit 1 / 6.3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_iterlen.py` | ok / 79.1 ms | exit 1 / 2 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_itertools.py` | ok / 854.7 ms | exit 1 / 14.9 ms | pyx_fail | error[PYX0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_keyword.py` | ok / 115.7 ms | exit 1 / 2.2 ms | pyx_fail | error[PYX0002]: module 'keyword' not found: not supported unless a sibling keyword.py exists |
| `test_keywordonlyarg.py` | ok / 75.5 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_kqueue.py` | exit 1 / 89.9 ms | exit 1 / 2.9 ms | expected_fail | error[PYX0002]: module 'errno' not found: not supported unless a sibling errno.py exists |
| `test_largefile.py` | ok / 1615.2 ms | exit 1 / 3.4 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_launcher.py` | exit 1 / 131.7 ms | exit 1 / 4.8 ms | expected_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_linecache.py` | ok / 179.4 ms | exit 1 / 4.6 ms | pyx_fail | error[PYX0002]: module 'importlib' not found: not supported unless a sibling importlib.py exists |
| `test_list.py` | ok / 460.3 ms | exit 1 / 3.4 ms | pyx_fail | error[PYX0002]: module 'signal' not found: not supported unless a sibling signal.py exists |
| `test_listcomps.py` | ok / 227.7 ms | exit 1 / 4 ms | pyx_fail | error[PYX0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_lltrace.py` | ok / 121.7 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: module 'dis' not found: not supported unless a sibling dis.py exists |
| `test_locale.py` | ok / 217.5 ms | exit 1 / 4.6 ms | pyx_fail | error[PYX0002]: module 'decimal' not found: not supported unless a sibling decimal.py exists |
| `test_logging.py` | timeout / 10009.2 ms | exit 1 / 25.4 ms | expected_fail | error[PYX0002]: module 'logging' not found: not supported unless a sibling logging.py exists |
| `test_long.py` | ok / 559.2 ms | exit 1 / 10.6 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_longexp.py` | ok / 157.6 ms | exit 1 / 2.3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_lzma.py` | ok / 223 ms | exit 1 / 8.3 ms | pyx_fail | error[PYX0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_mailbox.py` | ok / 1346.5 ms | exit 1 / 11.2 ms | pyx_fail | error[PYX0002]: module 'email' not found: not supported unless a sibling email.py exists |
| `test_marshal.py` | ok / 264.6 ms | exit 1 / 6.5 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_math.py` | timeout / 10012.4 ms | exit 1 / 17.1 ms | expected_fail | error[PYX0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_math_property.py` | ok / 95.8 ms | exit 1 / 1.5 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_memoryio.py` | ok / 151.9 ms | exit 1 / 5.9 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_memoryview.py` | ok / 452.1 ms | exit 1 / 5.2 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_metaclass.py` | ok / 173.6 ms | exit 1 / 1.6 ms | pyx_fail | error[PYX0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_mimetypes.py` | ok / 536.6 ms | exit 1 / 4.6 ms | pyx_fail | error[PYX0002]: module 'mimetypes' not found: not supported unless a sibling mimetypes.py exists |
| `test_minidom.py` | ok / 166.6 ms | exit 1 / 9.2 ms | pyx_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_mmap.py` | ok / 177.1 ms | exit 1 / 5.8 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_modulefinder.py` | ok / 339 ms | exit 1 / 2.8 ms | pyx_fail | error[PYX0002]: module 'errno' not found: not supported unless a sibling errno.py exists |
| `test_monitoring.py` | ok / 117.8 ms | exit 1 / 8.9 ms | pyx_fail | error[PYX0002]: module 'dis' not found: not supported unless a sibling dis.py exists |
| `test_msvcrt.py` | exit 1 / 91.9 ms | exit 1 / 2.2 ms | expected_fail | error[PYX0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_multibytecodec.py` | ok / 268.4 ms | exit 1 / 3.4 ms | pyx_fail | error[PYX0002]: module '_multibytecodec' not found: not supported unless a sibling _multibytecodec.py exists |
| `test_multiprocessing_main_handling.py` | exit 1 / 1478.2 ms | exit 1 / 3.6 ms | expected_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_named_expressions.py` | ok / 125.6 ms | exit 1 / 5.3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_netrc.py` | ok / 140.1 ms | exit 1 / 3 ms | pyx_fail | error[PYX0002]: module 'netrc' not found: not supported unless a sibling netrc.py exists |
| `test_ntpath.py` | ok / 408.8 ms | exit 1 / 10.9 ms | pyx_fail | error[PYX0002]: module 'inspect' not found: not supported unless a sibling inspect.py exists |
| `test_nturl2path.py` | ok / 141 ms | exit 1 / 3.6 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_numeric_tower.py` | ok / 122.7 ms | exit 1 / 3.3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_opcache.py` | ok / 488.3 ms | exit 1 / 7.7 ms | pyx_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_opcodes.py` | ok / 124 ms | exit 1 / 3.1 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_openpty.py` | ok / 103.2 ms | exit 1 / 1.9 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_operator.py` | ok / 151.7 ms | exit 1 / 6 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_optimizer.py` | ok / 121 ms | exit 1 / 2.3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_optparse.py` | ok / 228.4 ms | exit 1 / 6.8 ms | pyx_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_ordered_dict.py` | ok / 286.5 ms | exit 1 / 8.4 ms | pyx_fail | error[PYX0002]: module 'builtins' not found: not supported unless a sibling builtins.py exists |
| `test_os.py` | timeout / 10007.3 ms | exit 1 / 25.3 ms | expected_fail | error[PYX0002]: module 'codecs' not found: not supported unless a sibling codecs.py exists |
| `test_osx_env.py` | ok / 117.1 ms | exit 1 / 2.2 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'test.support.os_helper' |
| `test_patma.py` | exit 1 / 102.3 ms | exit 1 / 11.8 ms | expected_fail | error[PYX0002]: module 'array' not found: not supported unless a sibling array.py exists |
| `test_pdb.py` | timeout / 10004.1 ms | exit 1 / 10.1 ms | expected_fail | error[PYX0002]: module '_colorize' not found: not supported unless a sibling _colorize.py exists |
| `test_peepholer.py` | ok / 416.8 ms | exit 1 / 11.9 ms | pyx_fail | error[PYX0002]: module 'ast' not found: not supported unless a sibling ast.py exists |
| `test_pep646_syntax.py` | ok / 253 ms | exit 1 / 1.9 ms | pyx_fail | error[PYX0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_perf_profiler.py` | ok / 434.6 ms | exit 1 / 3.4 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_perfmaps.py` | ok / 125.5 ms | exit 1 / 1.9 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_pickle.py` | ok / 2285.9 ms | exit 1 / 5.6 ms | pyx_fail | error[PYX0002]: module '_compat_pickle' not found: not supported unless a sibling _compat_pickle.py exists |
| `test_picklebuffer.py` | ok / 175.6 ms | exit 1 / 3.2 ms | pyx_fail | error[PYX0002]: module 'gc' not found: not supported unless a sibling gc.py exists |
| `test_pickletools.py` | ok / 597.1 ms | exit 1 / 3.1 ms | pyx_fail | error[PYX0002]: module 'pickletools' not found: not supported unless a sibling pickletools.py exists |
| `test_pkg.py` | ok / 145.4 ms | exit 1 / 2.6 ms | pyx_fail | error[PYX0002]: module 'tempfile' not found: not supported unless a sibling tempfile.py exists |
| `test_pkgutil.py` | exit 1 / 211.7 ms | exit 1 / 4.1 ms | expected_fail | error[PYX0002]: dotted imports are not supported for user modules: 'test.support.import_helper' |
| `test_platform.py` | ok / 453.8 ms | exit 1 / 4.9 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_plistlib.py` | ok / 286.1 ms | exit 1 / 5.6 ms | pyx_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_poll.py` | timeout / 10012.7 ms | exit 1 / 2.4 ms | expected_fail | error[PYX0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_popen.py` | ok / 196 ms | exit 1 / 1.9 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_poplib.py` | ok / 6432.1 ms | exit 1 / 3.7 ms | pyx_fail | error[PYX0002]: module 'poplib' not found: not supported unless a sibling poplib.py exists |
| `test_positional_only_arg.py` | ok / 224.6 ms | exit 1 / 4.6 ms | pyx_fail | error[PYX0002]: module 'dis' not found: not supported unless a sibling dis.py exists |
| `test_posix.py` | exit 1 / 1142.7 ms | exit 1 / 10.4 ms | expected_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_posixpath.py` | ok / 371.8 ms | exit 1 / 7.4 ms | pyx_fail | error[PYX0002]: module 'inspect' not found: not supported unless a sibling inspect.py exists |
| `test_pow.py` | ok / 314.5 ms | exit 1 / 2.8 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_pprint.py` | ok / 221.8 ms | exit 1 / 4.9 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_print.py` | ok / 160.1 ms | exit 1 / 2.3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_profile.py` | ok / 350.6 ms | exit 1 / 2.7 ms | pyx_fail | error[PYX0002]: module 'pstats' not found: not supported unless a sibling pstats.py exists |
| `test_property.py` | ok / 118.7 ms | exit 1 / 3.2 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_pstats.py` | ok / 235.7 ms | exit 1 / 3.3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_pty.py` | ok / 142.9 ms | exit 1 / 3.1 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_pulldom.py` | ok / 199.1 ms | exit 1 / 3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_pwd.py` | ok / 136.4 ms | exit 1 / 2.4 ms | pyx_fail | error[PYX0002]: module 'string' not found: not supported unless a sibling string.py exists |
| `test_py_compile.py` | ok / 391.7 ms | exit 1 / 3.3 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'importlib.util' |
| `test_pyclbr.py` | exit 1 / 140.3 ms | exit 1 / 3.2 ms | expected_fail | error[PYX0002]: dotted imports are not supported for user modules: 'importlib.machinery' |
| `test_pyexpat.py` | ok / 262.5 ms | exit 1 / 5.3 ms | pyx_fail | error[PYX0002]: module 'abc' not found: not supported unless a sibling abc.py exists |
| `test_queue.py` | ok / 7965.3 ms | exit 1 / 6 ms | pyx_fail | error[PYX0002]: module 'itertools' not found: not supported unless a sibling itertools.py exists |
| `test_quopri.py` | ok / 216.9 ms | exit 1 / 3.3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_raise.py` | ok / 231.4 ms | exit 1 / 8.2 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_random.py` | ok / 1075 ms | exit 1 / 9.6 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_range.py` | ok / 268.5 ms | exit 1 / 6.2 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_re.py` | ok / 606.1 ms | exit 1 / 19.5 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_readline.py` | ok / 385.3 ms | exit 1 / 5 ms | pyx_fail | error[PYX0002]: module 'codecs' not found: not supported unless a sibling codecs.py exists |
| `test_regrtest.py` | exit 1 / 2254 ms | exit 1 / 12.3 ms | expected_fail | error[PYX0002]: module '_colorize' not found: not supported unless a sibling _colorize.py exists |
| `test_remote_pdb.py` | ok / 2682.5 ms | exit 1 / 7.2 ms | pyx_fail | error[PYX0002]: module 'itertools' not found: not supported unless a sibling itertools.py exists |
| `test_repl.py` | ok / 1044.8 ms | exit 1 / 3.2 ms | pyx_fail | error[PYX0002]: module 'select' not found: not supported unless a sibling select.py exists |
| `test_reprlib.py` | ok / 81.9 ms | exit 1 / 4.7 ms | pyx_fail | error[PYX0002]: module 'annotationlib' not found: not supported unless a sibling annotationlib.py exists |
| `test_resource.py` | ok / 83.8 ms | exit 1 / 2.6 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_richcmp.py` | ok / 46 ms | exit 1 / 2.9 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_rlcompleter.py` | ok / 109.6 ms | exit 1 / 3.7 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_robotparser.py` | ok / 1318.7 ms | exit 1 / 5.8 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_runpy.py` | timeout / 10007.6 ms | exit 1 / 5.4 ms | expected_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_sax.py` | ok / 224.1 ms | exit 1 / 7.2 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'xml.sax' |
| `test_sched.py` | ok / 583 ms | exit 1 / 3.3 ms | pyx_fail | error[PYX0002]: module 'sched' not found: not supported unless a sibling sched.py exists |
| `test_scope.py` | ok / 53.9 ms | exit 1 / 3.9 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_script_helper.py` | ok / 192 ms | exit 1 / 2.2 ms | pyx_fail | error[PYX0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_secrets.py` | ok / 43.8 ms | exit 1 / 2.4 ms | pyx_fail | error[PYX0002]: module 'secrets' not found: not supported unless a sibling secrets.py exists |
| `test_select.py` | ok / 1556.3 ms | exit 1 / 2.7 ms | pyx_fail | error[PYX0002]: module 'errno' not found: not supported unless a sibling errno.py exists |
| `test_selectors.py` | timeout / 10005.3 ms | exit 1 / 4.8 ms | expected_fail | error[PYX0002]: module 'errno' not found: not supported unless a sibling errno.py exists |
| `test_set.py` | ok / 765.4 ms | exit 1 / 10.1 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'collections.abc' |
| `test_setcomps.py` | ok / 124.1 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_shelve.py` | ok / 2615.8 ms | exit 1 / 2.6 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_shlex.py` | ok / 138.9 ms | exit 1 / 3.6 ms | pyx_fail | error[PYX0002]: module 'itertools' not found: not supported unless a sibling itertools.py exists |
| `test_shutil.py` | exit 1 / 492 ms | exit 1 / 16.1 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_signal.py` | timeout / 10004.2 ms | exit 1 / 5 ms | expected_fail | error[PYX0002]: module 'enum' not found: not supported unless a sibling enum.py exists |
| `test_site.py` | ok / 2459.7 ms | exit 1 / 6.1 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_slice.py` | ok / 54.3 ms | exit 1 / 3.5 ms | pyx_fail | error[PYX0002]: module 'itertools' not found: not supported unless a sibling itertools.py exists |
| `test_smtplib.py` | ok / 1440.1 ms | exit 1 / 8.1 ms | pyx_fail | error[PYX0002]: module 'base64' not found: not supported unless a sibling base64.py exists |
| `test_smtpnet.py` | timeout / 10003.8 ms | exit 1 / 2.2 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_socket.py` | timeout / 10006.4 ms | exit 1 / 29 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_socketserver.py` | ok / 520.5 ms | exit 1 / 3.7 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_sort.py` | ok / 73.5 ms | exit 1 / 2.9 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_source_encoding.py` | timeout / 10013.4 ms | exit 1 / 4 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_ssl.py` | timeout / 10005.5 ms | exit 1 / 21.8 ms | expected_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_stable_abi_ctypes.py` | exit 1 / 119.8 ms | exit 1 / 3.2 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_startfile.py` | exit 1 / 119.1 ms | exit 1 / 1.8 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_stat.py` | ok / 147 ms | exit 1 / 3.1 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_statistics.py` | ok / 9836.9 ms | exit 1 / 14.4 ms | pyx_fail | error[PYX0002]: module 'bisect' not found: not supported unless a sibling bisect.py exists |
| `test_str.py` | ok / 1145.1 ms | exit 1 / 14.2 ms | pyx_fail | error[PYX0002]: module '_string' not found: not supported unless a sibling _string.py exists |
| `test_strftime.py` | ok / 174.9 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: module 'calendar' not found: not supported unless a sibling calendar.py exists |
| `test_string_literals.py` | ok / 136.7 ms | exit 1 / 3.5 ms | pyx_fail | error[PYX0002]: module 'shutil' not found: not supported unless a sibling shutil.py exists |
| `test_stringprep.py` | ok / 124.5 ms | exit 1 / 1.8 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_strptime.py` | ok / 170.7 ms | exit 1 / 5.7 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_strtod.py` | ok / 247.1 ms | exit 1 / 3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_struct.py` | ok / 393.4 ms | exit 1 / 8.3 ms | pyx_fail | error[PYX0002]: module 'itertools' not found: not supported unless a sibling itertools.py exists |
| `test_structseq.py` | ok / 160.8 ms | exit 1 / 3.4 ms | pyx_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_subclassinit.py` | ok / 124.8 ms | exit 1 / 2.2 ms | pyx_fail | error[PYX0002]: module 'types' not found: not supported unless a sibling types.py exists |
| `test_subprocess.py` | timeout / 10004.5 ms | exit 1 / 15.5 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_sundry.py` | ok / 145.9 ms | exit 1 / 1.9 ms | pyx_fail | error[PYX0002]: module 'importlib' not found: not supported unless a sibling importlib.py exists |
| `test_super.py` | ok / 262 ms | exit 1 / 3.9 ms | pyx_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_support.py` | exit 1 / 5629.2 ms | exit 1 / 5.4 ms | expected_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_symtable.py` | ok / 63.6 ms | exit 1 / 3.3 ms | pyx_fail | error[PYX0002]: module 'textwrap' not found: not supported unless a sibling textwrap.py exists |
| `test_syntax.py` | ok / 193.4 ms | exit 1 / 4.2 ms | pyx_fail | error[PYX0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_sys.py` | exit 1 / 1265.4 ms | exit 1 / 11.9 ms | expected_fail | error[PYX0002]: module 'builtins' not found: not supported unless a sibling builtins.py exists |
| `test_sys_setprofile.py` | ok / 53.7 ms | exit 1 / 3.5 ms | pyx_fail | error[PYX0002]: module 'gc' not found: not supported unless a sibling gc.py exists |
| `test_sys_settrace.py` | ok / 797 ms | exit 1 / 9.8 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_sysconfig.py` | exit 1 / 195.6 ms | exit 1 / 5.2 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_syslog.py` | ok / 195.9 ms | exit 1 / 2.7 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_tabnanny.py` | ok / 136.6 ms | exit 1 / 3.3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_tarfile.py` | exit 1 / 5621.9 ms | exit 1 / 18.3 ms | expected_fail | error[PYX0002]: module 'errno' not found: not supported unless a sibling errno.py exists |
| `test_tcl.py` | ok / 116.1 ms | exit 1 / 6.2 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_tempfile.py` | ok / 397.1 ms | exit 1 / 8.8 ms | pyx_fail | error[PYX0002]: module 'tempfile' not found: not supported unless a sibling tempfile.py exists |
| `test_termios.py` | ok / 557.8 ms | exit 1 / 3.3 ms | pyx_fail | error[PYX0002]: module 'errno' not found: not supported unless a sibling errno.py exists |
| `test_textwrap.py` | ok / 43.9 ms | exit 1 / 4.5 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_thread.py` | ok / 1298 ms | exit 1 / 3.8 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_thread_local_bytecode.py` | ok / 56.3 ms | exit 1 / 2.1 ms | pyx_fail | error[PYX0002]: module 'textwrap' not found: not supported unless a sibling textwrap.py exists |
| `test_threadedtempfile.py` | ok / 57.7 ms | exit 1 / 2 ms | pyx_fail | error[PYX0002]: module 'tempfile' not found: not supported unless a sibling tempfile.py exists |
| `test_threading.py` | timeout / 10003.3 ms | exit 1 / 7.2 ms | expected_fail | error[PYX0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_threading_local.py` | ok / 345.8 ms | exit 1 / 2.7 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_threadsignals.py` | ok / 4900.8 ms | exit 1 / 2.8 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_time.py` | ok / 3087.7 ms | exit 1 / 6.4 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_timeit.py` | ok / 63.8 ms | exit 1 / 2.8 ms | pyx_fail | error[PYX0002]: module 'timeit' not found: not supported unless a sibling timeit.py exists |
| `test_timeout.py` | timeout / 10004.2 ms | exit 1 / 2.2 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_tokenize.py` | ok / 6617.7 ms | exit 1 / 9.2 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_trace.py` | ok / 993.7 ms | exit 1 / 3.6 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_traceback.py` | exit 1 / 2277.2 ms | exit 1 / 20.3 ms | expected_fail | error[PYX0002]: module 'linecache' not found: not supported unless a sibling linecache.py exists |
| `test_tracemalloc.py` | ok / 731.5 ms | exit 1 / 5.4 ms | pyx_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_tstring.py` | exit 1 / 289.7 ms | exit 1 / 1.4 ms | expected_fail | error[PYX0001]: invalid syntax: invalid syntax. Got unexpected token "Hello" |
| `test_ttk_textonly.py` | ok / 73.2 ms | exit 1 / 3.1 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_tty.py` | ok / 47.3 ms | exit 1 / 2.1 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_tuple.py` | ok / 231.9 ms | exit 1 / 3.4 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_turtle.py` | ok / 199.4 ms | exit 1 / 4.2 ms | pyx_fail | error[PYX0002]: module 'tempfile' not found: not supported unless a sibling tempfile.py exists |
| `test_type_aliases.py` | ok / 55.4 ms | exit 1 / 3.7 ms | pyx_fail | error[PYX0002]: module 'types' not found: not supported unless a sibling types.py exists |
| `test_type_annotations.py` | ok / 59.6 ms | exit 1 / 4.5 ms | pyx_fail | error[PYX0002]: module 'annotationlib' not found: not supported unless a sibling annotationlib.py exists |
| `test_type_cache.py` | exit 1 / 55.7 ms | exit 1 / 2.4 ms | expected_fail | error[PYX0002]: dotted imports are not supported for user modules: 'collections.abc' |
| `test_type_comments.py` | ok / 54.3 ms | exit 1 / 3.2 ms | pyx_fail | error[PYX0002]: module 'ast' not found: not supported unless a sibling ast.py exists |
| `test_type_params.py` | ok / 59.7 ms | exit 1 / 6.1 ms | pyx_fail | error[PYX0001]: invalid syntax: invalid syntax. Got unexpected token '=' |
| `test_typechecks.py` | ok / 44 ms | exit 1 / 2 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_types.py` | ok / 279 ms | exit 1 / 12.2 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_typing.py` | ok / 407.3 ms | exit 1 / 41.6 ms | pyx_fail | error[PYX0002]: module 'annotationlib' not found: not supported unless a sibling annotationlib.py exists |
| `test_ucn.py` | ok / 669.1 ms | exit 1 / 2.8 ms | pyx_fail | error[PYX0002]: module 'ast' not found: not supported unless a sibling ast.py exists |
| `test_unary.py` | ok / 43.1 ms | exit 1 / 1.8 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_unicode_file.py` | ok / 50.1 ms | exit 1 / 2.1 ms | pyx_fail | error[PYX0002]: module 'glob' not found: not supported unless a sibling glob.py exists |
| `test_unicode_file_functions.py` | ok / 74.9 ms | exit 1 / 2.9 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_unicode_identifiers.py` | ok / 47 ms | exit 1 / 1.7 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_unicodedata.py` | ok / 5847.9 ms | exit 1 / 6.2 ms | pyx_fail | error[PYX0002]: module 'hashlib' not found: not supported unless a sibling hashlib.py exists |
| `test_univnewlines.py` | ok / 64.8 ms | exit 1 / 2 ms | pyx_fail | error[PYX0002]: module '_pyio' not found: not supported unless a sibling _pyio.py exists |
| `test_unpack.py` | exit 1 / 157.3 ms | exit 1 / 1.8 ms | expected_fail | error[PYX0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_unpack_ex.py` | exit 1 / 151.8 ms | exit 1 / 1.7 ms | expected_fail | error[PYX0002]: module 'doctest' not found: not supported unless a sibling doctest.py exists |
| `test_unparse.py` | ok / 6218.4 ms | exit 1 / 4.2 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_urllib.py` | ok / 248.4 ms | exit 1 / 8.6 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'urllib.parse' |
| `test_urllib2.py` | ok / 1860.1 ms | exit 1 / 9.3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_urllib2_localnet.py` | ok / 2170.8 ms | exit 1 / 3.9 ms | pyx_fail | error[PYX0002]: module 'base64' not found: not supported unless a sibling base64.py exists |
| `test_urllib2net.py` | timeout / 10011.7 ms | exit 1 / 2.9 ms | expected_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_urllib_response.py` | ok / 56.9 ms | exit 1 / 1.9 ms | pyx_fail | error[PYX0002]: module 'tempfile' not found: not supported unless a sibling tempfile.py exists |
| `test_urllibnet.py` | ok / 7551.2 ms | exit 1 / 2.7 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_urlparse.py` | ok / 134.6 ms | exit 1 / 11.1 ms | pyx_fail | error[PYX0002]: module 'unicodedata' not found: not supported unless a sibling unicodedata.py exists |
| `test_userdict.py` | ok / 144.4 ms | exit 1 / 3.4 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_userlist.py` | ok / 135.8 ms | exit 1 / 2.7 ms | pyx_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_userstring.py` | ok / 579.8 ms | exit 1 / 2 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_utf8_mode.py` | ok / 433.9 ms | exit 1 / 2.9 ms | pyx_fail | error[PYX0002]: module 'locale' not found: not supported unless a sibling locale.py exists |
| `test_utf8source.py` | ok / 44.7 ms | exit 1 / 1.9 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_uuid.py` | ok / 286.8 ms | exit 1 / 7.1 ms | pyx_fail | error[PYX0002]: module 'builtins' not found: not supported unless a sibling builtins.py exists |
| `test_venv.py` | exit 1 / 5611.3 ms | exit 1 / 6.3 ms | expected_fail | error[PYX0002]: module 'contextlib' not found: not supported unless a sibling contextlib.py exists |
| `test_wait3.py` | ok / 5068.1 ms | exit 1 / 2.5 ms | pyx_fail | error[PYX0002]: module 'subprocess' not found: not supported unless a sibling subprocess.py exists |
| `test_wait4.py` | ok / 5056.2 ms | exit 1 / 1.9 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_wave.py` | ok / 61.9 ms | exit 1 / 2.3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_weakref.py` | exit 1 / 2604.1 ms | exit 1 / 8.8 ms | expected_fail | error[PYX0002]: module 'gc' not found: not supported unless a sibling gc.py exists |
| `test_weakset.py` | ok / 73.4 ms | exit 1 / 3.8 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_webbrowser.py` | ok / 2109 ms | exit 1 / 4.3 ms | pyx_fail | error[PYX0002]: module 'shlex' not found: not supported unless a sibling shlex.py exists |
| `test_winapi.py` | exit 1 / 54.6 ms | exit 1 / 2.2 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_winconsoleio.py` | exit 1 / 50.6 ms | exit 1 / 2.4 ms | expected_fail | error[PYX0002]: module 'tempfile' not found: not supported unless a sibling tempfile.py exists |
| `test_winreg.py` | exit 1 / 52.7 ms | exit 1 / 3.5 ms | expected_fail | error[PYX0002]: module 'gc' not found: not supported unless a sibling gc.py exists |
| `test_winsound.py` | exit 1 / 48.3 ms | exit 1 / 2.1 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_with.py` | ok / 48 ms | exit 1 / 4.4 ms | pyx_fail | error[PYX0002]: module 'traceback' not found: not supported unless a sibling traceback.py exists |
| `test_wmi.py` | exit 1 / 51.8 ms | exit 1 / 1.9 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_wsgiref.py` | ok / 235.7 ms | exit 1 / 5.3 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_xml_dom_minicompat.py` | ok / 50.8 ms | exit 1 / 2.3 ms | pyx_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_xml_dom_xmlbuilder.py` | ok / 167.2 ms | exit 1 / 1.8 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_xml_etree.py` | ok / 793.8 ms | exit 1 / 19.3 ms | pyx_fail | error[PYX0002]: module 'copy' not found: not supported unless a sibling copy.py exists |
| `test_xml_etree_c.py` | ok / 521.4 ms | exit 1 / 2.7 ms | pyx_fail | error[PYX0002]: module 'struct' not found: not supported unless a sibling struct.py exists |
| `test_xmlrpc.py` | timeout / 10003.7 ms | exit 1 / 8.2 ms | expected_fail | error[PYX0002]: module 'base64' not found: not supported unless a sibling base64.py exists |
| `test_xpickle.py` | ok / 2018 ms | exit 1 / 2.6 ms | pyx_fail | error[PYX0002]: module 'struct' not found: not supported unless a sibling struct.py exists |
| `test_xxlimited.py` | exit 1 / 48.5 ms | exit 1 / 1.8 ms | expected_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_xxtestfuzz.py` | ok / 53.6 ms | exit 1 / 1.8 ms | pyx_fail | error[PYX0002]: module 'faulthandler' not found: not supported unless a sibling faulthandler.py exists |
| `test_yield_from.py` | ok / 61.7 ms | exit 1 / 6.9 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_zipapp.py` | ok / 150 ms | exit 1 / 3.6 ms | pyx_fail | error[PYX0002]: module 'stat' not found: not supported unless a sibling stat.py exists |
| `test_zipfile64.py` | timeout / 10022.7 ms | exit 1 / 2.2 ms | expected_fail | error[PYX0002]: module 'test' not found: not supported unless a sibling test.py exists |
| `test_zipimport.py` | timeout / 10013.5 ms | exit 1 / 9.4 ms | expected_fail | error[PYX0002]: module 'marshal' not found: not supported unless a sibling marshal.py exists |
| `test_zipimport_support.py` | ok / 513.7 ms | exit 1 / 3 ms | pyx_fail | error[PYX0002]: dotted imports are not supported for user modules: 'test.support' |
| `test_zlib.py` | ok / 461.6 ms | exit 1 / 5.6 ms | pyx_fail | error[PYX0002]: module 'unittest' not found: not supported unless a sibling unittest.py exists |
| `test_zstd.py` | ok / 534.6 ms | exit 1 / 12 ms | pyx_fail | error[PYX0002]: module 'array' not found: not supported unless a sibling array.py exists |

<!-- END GENERATED RESULTS -->
