# Hydro — full CPython 3.14.9 coverage inventory

This is the authoritative, hand-maintained inventory of everything still open
on the road to **100 % CPython 3.14.9 compatibility**. Generated reports remain
authoritative for counts and per-test first errors:

- `docs/CPYTHON_TESTS.md` — generated from the last complete `Lib/test` run:
  every blocked test and its first error.
- `docs/COMPATIBILITY.md` — generated stdlib import matrix.
- `docs/ECOSYSTEM.md` — generated pinned-wheel differential gate.
- `docs/C_API.md` — C-API/ABI inventory and link probes.
- `docs/NATIVE_BINDINGS.md` — HNA/bindgen/sidecar limits.

The exhaustive per-test list must not be duplicated here; it changes with every
run. This document groups the measured failures into root-cause workstreams,
records tests that are currently hidden behind a first error, and defines the
gate that must be green before "100 %" may be claimed.

## Definition of 100 %

The target is CPython **3.14.9** (`hydro_ir::compat` is the single source of
truth). A complete claim requires all of the following, measured on a fresh
checkout with the pinned oracle:

1. **Official suite:** all 398 selected `Lib/test/test_*.py` entries end in
   `hydro_pass` or a documented, oracle-equivalent unsupported result. The
   strict target is `hydro_fail=0`, `timeout=0`, `memkill=0`, `error=0`,
   `expected_fail=0`, `cpython_fail=0`.
2. **No exit-code camouflage:** each passing file must actually execute its
   tests. A file that imports a broken `test.support` helper and exits 0 with
   0 tests run is not a pass; the runner must record executed/skipped counts and
   reject empty successful runs for the coverage gate.
3. **Stdlib:** every module CPython 3.14.9 can import on the verified host is
   importable and behaviorally tested. The current comparable matrix is
   **288/288**; the nine CPython-failing entries are Windows-only modules plus
   `_gdbm` and must stay explicitly classified.
4. **Ecosystem:** every pinned case in `benchmarks/ecosystem/cases/` passes
   under the oracle, Hydro JIT and Hydro AOT with byte-identical stdout; then
   the Tier-1 upstream suites in `docs/TODO.md` pass with no silent skips.
5. **C-API/ABI:** every required symbol for the pinned extension set resolves;
   type/vectorcall/buffer/GC/GIL semantics pass the differential and stress
   gates; real wheels load without `libpython`.
6. **Platforms:** Linux x86_64 remains green, and macOS, Windows and aarch64
   are either verified or explicitly outside the support policy.
7. **No hidden regressions:** workspace tests, clippy `-D warnings`, fmt,
   manifest check, ecosystem gate and benchmarks are green in CI.

## Current measured baseline (last complete reports)

| Gate | Current | Still needed |
| --- | --- | --- |
| Official full run | **36/398** `hydro_pass` (2026-11-10T17:20:05Z; 877 s) | 362 tests to `hydro_pass`, or a documented oracle-equivalent reason |
| Official blocked backlog | **303** (`hydro_fail=296`, `timeout=4`, `memkill=3`) | eliminate every blocked test |
| Verified manifest | **33** known passes; the latest run's 36 includes 4 regressions of older pins | recover the four regressions, then raise the manifest |
| Oracle-environment classes | `expected_fail=56`, `cpython_fail=3`, `error=0` | rerun in a valid oracle environment; reclassify until zero |
| First-error families | **259** distinct rows | collapse by root cause; do not patch one error per test |
| Stdlib imports | **288/297**; **288/288** comparable | behavioral tests for every module |
| Ecosystem differential | **5/13** refreshed 2026-11-10; pydantic now reaches `annotationlib.__eq__` | 8 cases, then upstream suites |
| Workspace | **3261 passed, 1 skipped**; clippy/fmt green | keep green through the compatibility work |

Post-baseline fixes since this run: variadic decorator receiver binding,
PEP 584 `dict | dict`/subclass union, iterable set operands (including
`dict_keys`/`dict_items` against tuples/iterators and reflected views), and
cross-kind keys/items equality. These are covered by workspace tests and
minimal scripts but were not included in this 398-test run.

The four timeouts are `test_bisect.py`, `test_imaplib.py`, `test_queue.py` and
`test_signal.py`. The three watchdog kills are `test_zipfile64.py`,
`test_zlib.py` and `test_zstd.py` at the 1500 MB available-memory threshold.
The three `cpython_fail` entries are `test_extcall.py`, `test_unpack.py` and
`test_unpack_ex.py`: Hydro passed while the oracle failed, so they are not
Hydro passes and cannot be counted as parity. Four previously pinned passes
(`test_dictviews.py`, `test_plistlib.py`, `test_poll.py`, `test_yield_from.py`)
fail on the current binary and must be recovered before the manifest gate can
be claimed green.

### Immediate regression recovery (manifest gate)

- `test_dictviews.py`: dict-view set operations against tuples/iterators and
  recursive `repr` recursion depth.
- `test_plistlib.py`: `_SubTest` construction calls
  `builtins.__object_init__` with two arguments; the `object.__init__`
  binding/spec path must reject/accept exactly CPython's receiver-only shape.
- `test_poll.py`: `poll.register(..., eventmask=...)` and threaded poll calls
  still route keywords through a native callable that rejects them; the
  `select.poll` method specs need their declared keyword names.
- `test_yield_from.py`: module-level `TypeError: 'str' object is not callable`
  while exercising `yield from ()`; isolate the shadowed callable and add a
  minimal regression.
- `test_repl.py` currently has 16 failing entries after the cleanup,
  descriptor, variadic-decorator and dict-union fixes (more subtests now run
  than the earlier 15-failure state). Remaining first blockers: Popen
  `stdin`/`stdout`/`stderr` stream attributes, `communicate()`, the Popen
  context-manager protocol, and `asyncio/__main__.py`'s relative
  `from . import futures` under `-m`.

## Workstreams — everything still missing

### 1. Harness, runner and manifest fidelity

- Run all 398 files with enough memory/timeout to eliminate `timeout` and
  `memkill` noise without lowering the watchdog into false failures.
- Record executed, skipped and expected-failure counts per file; reject
  "successful" imports that ran zero tests as coverage passes.
- Resolve the 57 `expected_fail` entries: they are not permitted in the strict
  100 % claim unless the final support policy explicitly excludes them.
- Reconcile the three `cpython_fail` entries and keep `minimum_passing`
  monotonic only from real `hydro_pass` results.
- Keep `test.support` and `test.libregrtest` imports working well enough that
  every test module reaches its own test functions.
- Add a release-gate job that runs the full 398 with per-test logs, memory
  watchdog and a machine-readable checkpoint, then regenerates every report.

### 2. Frontend, parser and language semantics

The largest current first-error family is `TypeError: 'str' object is not
callable` (12 tests: `test_codecencodings_kr`, `test_except_star`,
`test_exception_group`, `test_index`, `test_optparse`, `test_strptime`,
`test_sys_settrace`, `test_tempfile`, `test_types`, `test_ucn`,
`test_unparse`, `test_yield_from`), followed by regex backreferences (9
tests). The opcode metadata cluster is reduced after `test__opcode.py` and
several consumers passed.

Observed current families:

- **Class construction/type identity:** `class base of '…' must be a class
  object or None, got 'function'` blocks `test_fractions.py`,
  `test_argparse.py`, `test_htmlparser.py`, `test_enumerate.py`,
  `test_property.py`, `test_types.py`, `test_abc.py` and related files.
  Builtin type spellings, `type(1) is int`, metaclass/`__mro_entries__`,
  and class-vs-function values must be one model.
- **Class scopes and `super()`:** `test_super.py` (nonlocal `__class__`,
  `super().__init__` receiver forms), `test_global.py` (handler binding),
  `test_class.py` (`None` subtraction), `test_descr.py`, `test_set_name.py`.
- **Decorators/descriptors/dunders:** remaining decorator variants,
  descriptor precedence, `__slots__`, property setters, `__set_name__`,
  unary/binary reflected operators, `__index__`, `__round__`, `__getnewargs__`.
- **Imports:** `from x import *`, runtime/dynamic imports, namespace/zip/frozen
  imports, dotted submodule registration, import hooks and lazy module
  materialization. The import matrix is green for importability only.
- **Syntax/compiler gaps:** `test_grammar.py` (unterminated string/EOL),
  `test_type_params.py` (`=` token), `test_annotationlib.py` and
  `test_typing.py` (starred expression lowering), `test_collections.py` and
  `test_generators.py` (`yield` inside lambda), `test_asyncgen.py`
  (async comprehension outside async def), `test_coroutines.py`
  (for-loop target), `test_flufl.py`, `test_with.py` and
  `test_named_expressions.py` (syntax-error agreement).
- **Code objects/introspection:** `test_funcattrs.py`, `test_code.py`,
  `test_dis.py`, `test_compile.py`, `test_compiler_assemble.py`,
  `test_compiler_codegen.py`, `test_opcache.py`, `test_generated_cases.py`,
  `test_exception_hierarchy.py`.
- **Operators/typing/equality:** `test_contains.py` (`None` left operand),
  `test_richcmp.py` (`int < None` guarded path), `test_opcodes.py`
  (string-formatting subclass), `test_pow.py` (large exponent/BigInt),
  `test_isinstance.py` (stack overflow/recursion), `test_format.py`
  (non-unwinding panic).
- **Positional-only/keyword binding:** `test_positional_only_arg.py`,
  `test_keywordonlyarg.py`, `test_functools.py` (`partialmethod` multiple
  values), plus native-call keyword routing. Popen now accepts declared
  keywords and `executable=`; the general binder still needs a regression
  matrix covering dynamic calls, `functools.partial`, `**mapping` and
  positional-only parameters.

### 3. Object model, builtins and runtime

- **Exceptions:** `UnboundLocalError` and full builtin hierarchy identity
  (`test_baseexception.py`), exception groups/`except*` grouping, traceback
  frames and `__context__`/`__cause__` edge cases (`test_raise.py`,
  `test_exception_group.py`), subclass constructors and `__notes__`.
- **Strings/bytes/format:** `%` formatting (`test_str.py`, `test_userstring.py`,
  `test_opcodes.py`), `str.format_map` and format mini-language, string
  subclasses, bytes/bytearray methods (`test_bytes.py`, `test_binascii.py`,
  `test_buffer.py`), memoryview/cast/buffer protocol (`test_memoryview.py`,
  `test_base64.py`), codec callbacks (`test_codeccallbacks.py`), Unicode
  identifiers/source encodings (`test_unicode_identifiers.py`,
  `test_utf8source.py`, `test_source_encoding.py`).
- **Containers/iteration:** `dict | dict` now works for exact dicts and
  subclasses (right-hand entries win, plain-dict result); in-place
  `dict |=` still rebinds through the generic AugAssign path and needs an
  in-place ABI. Set operators now accept arbitrary iterable right operands
  and reflected dict-view operands. Also open: `dict.update` with no positional
  argument (`test_dict.py`), list/tuple keyword constructors, unhashable set
  behavior (`test_set.py`, `test_pprint.py`), dict-view ABC registration,
  comparison errors and copy protocols, recursive view reprs
  (`test_dictviews.py`), iterator/unpacking errors (`test_iter.py`), sequence
  index/protocol behavior (`test_index.py`, `test_bigaddrspace.py`), and
  ordered comparison type errors.
- **Numbers:** BigInt exponent/convert paths (`test_pow.py`, `test_long.py`),
  float formatting/rounding (`test_float.py`, `test_int.py`), Decimal hashes
  and numeric tower (`test_numeric_tower.py`), Fraction construction
  (`test_fractions.py`), complex methods (`test_complex.py`).
- **Generators/async/context managers:** `yield` in lambda, async
  comprehensions, async generators, cancellation/TaskGroup edge cases,
  `contextlib` generator managers (`_GeneratorContextManager` callable/enter/
  exit arity), `with` assignment targets and exception suppression.
- **Threads/processes/frames:** `_thread.stack_size`, `sys.thread_info`,
  signal handling, fork/exec interactions, `sys._current_frames` depth,
  recursion limits, `test_threadsignals.py`, `test_signal.py`, `test_queue.py`.
- **GC/heap:** strict-mode root gaps, conservative fallback removal,
  large-allocation/RSS behavior, `test_zipfile64.py`/`test_zlib.py` memory
  watchdog, GC stress for extension callbacks.

### 4. Standard library (P7 batches and native modules)

The import matrix says 288 modules import; it does not say their APIs work.
Known module-specific blockers from the generated backlog:

- **opcode metadata (was a 23-test first-error cluster):** real
  `HAVE_ARGUMENT`, `opmap`, `opname`, inline-cache tables and `opcode` version
  metadata. `test__opcode.py` and several consumers now pass, so the current
  run no longer groups all 23 under one first error, but remaining opcode/
  code-object consumers such as `test_cmd`, `test_deque`, `test_doctest`,
  `test_pdb`, `test_pickletools`, `test_profile`, `test_pstats`, `test_trace`
  and `test_tracemalloc` still need auditing.
- **`_testinternalcapi` (10 tests):** `new_instruction_sequence`,
  `EncodeLocaleEx`, `_PyTime_FromSeconds`, `_PyTraceMalloc_GetTraceback`,
  `get_getpath_codeobject`, and the code/compile/compiler/dis family.
- **regex/text:** Rust `re` backreferences (8 tests), `_sre.MAXREPEAT`,
  `test_re.py`, `test_tokenize.py`, `test_textwrap.py`, `test_strptime.py`,
  `test_difflib.py`, `test_robotparser.py`, `test_sax.py`, `test_pulldom.py`.
- **serialization/data:** CPython pickle protocols 2–5 (not HYD1),
  `pickle.PickleBuffer`, `marshal`, `array.ArrayType`, `struct` edge cases,
  `test_copyreg`, `test_pickletools`, `test_xpickle`, `test_csv.py`
  (`writer(lineterminator=...)`), `test_configparser.py`.
- **filesystem/process:** Popen stream attributes (`stdin`/`stdout`/`stderr`),
  `communicate()`, context-manager protocol, `universal_newlines`,
  `creationflags`; `test_popen`, `test_subprocess`, `test_file_eintr`,
  `test_repl`, `test_pty` (`tty.tcgetattr`/ioctl), `tempfile.mktemp`,
  `os.supports_dir_fd`, `posix._path_splitroot_ex`, `Path.touch(mode=)`,
  `test_fileinput`, `test_bufio`, `test_filecmp`, `test_glob`.
- **terminal/REPL:** `tty`/`termios` functions, pty ioctl, interactive
  source/linecache, `PYTHONSTARTUP` behavior, contextvars in the REPL.
- **unittest/harness:** `unittest.mock`, `unittest.result`, test registration,
  `skipUnless`/`skipIf` keywords, suite teardown/context manager arity,
  `test.support` helpers; `test_copy`, `test_tempfile`, `test_unittest`,
  `test_mock`, `test_contextlib`.
- **networking:** `http.server` classes (`SimpleHTTPRequestHandler`,
  `HTTPSServer`), socket context manager and constants
  (`IOCTL_VM_SOCKETS_GET_LOCAL_CID`), `ssl.TLSVersion`, selectors/default
  selector, urllib/urllib2, `test_httplib`, `test_socket`, `test_ssl`,
  `test_selectors`, `test_timeout`, `test_wsgiref`, `test_httpservers`,
  `test_ftplib`, `test_poplib`, `test_smtplib`, `test_imaplib`.
- **asyncio/threads/subinterpreters:** `asyncio.taskgroups`,
  `test_external_inspection`, wider asyncio API, `_interpreters`,
  `_interpchannels`, `_interpqueues`, `_remote_debugging`; `test_context`,
  `test_decimal` (`Thread(context=...)`), `test__interpreters`,
  `test__interpchannels`.
- **datetime/locale/calendar:** `_datetime`, timezone/locale data,
  `test_datetime` (`Namespace.__dict__`), `test_locale` (`en_US.US-ASCII`
  normalization), `test_calendar` (output formatting), `test_time`,
  `test_strptime`, `test_zoneinfo`.
- **compression/archives:** `gzip.GzipFile`, bz2/lzma/zstd keyword APIs,
  `test_gzip`, `test_bz2`, `test_lzma`, `test_zstd`, `test_zipfile`,
  `test_zipfile64`, `test_tarfile`.
- **XML/HTML:** `xml.dom.expatbuilder`, minidom NodeList methods,
  `test_minidom`, `test_xml_etree_c`, `test_xml_dom_xmlbuilder`,
  `test_sax`, `test_pulldom`.
- **dbm/sqlite:** `dbm.dumb` encodings, `dbm.ndbm` truthiness,
  `dbm.sqlite3` `Path.touch(mode=)`, `sqlite3` adapters/registration;
  `test_dbm*`, `test_shelve`, `test_sqlite3`.
- **platform/introspection:** `sys._clear_internal_caches`, `sys._git`,
  `sys.intern`, `sys.thread_info`, `sys.getrefcount`, `platform`
  implementation/version metadata, `test_platform`, `test_marshal`,
  `test_glob`, `test_webbrowser`, `test_nturl2path`.
- **large native surfaces:** Tk/curses/turtle/idle (`test_curses`,
  `test_turtle`, `test_idle`, `test_ttk_textonly`), DB/network daemons,
  `multiprocessing.context`, `concurrent.futures`, `ctypes`, C-extension
  test modules.

### 5. C-API, ABI and extension loading

- Undefined-symbol import failures remain for real extension test modules:
  `_interpreters` (`_PyErr_ExceptionMatches`), `_xxtestfuzz`
  (`Py_CompileStringExFlags`), `_testclinic` (`_PyTuple_FromArray`).
- Full `PyTypeObject` behavior, heap types, `tp_traverse`/`tp_clear`,
  vectorcall, buffer protocol, refcount/GIL integration and multi-phase init
  edge cases are incomplete.
- NumPy requires dtype/module initialization and `_NoValue` handling;
  pandas requires dynamic `except (...)` expressions, `csv.writer`
  `lineterminator`, and deeper C-API semantics; pydantic-core/typing-extensions
  need AOT enum `_proto_member.__set_name__`/`int('keep')` behavior.
- `ctypes`/`cffi` ABI-mode boundaries, pointer/struct/array conversion and
  callback/thread safety are not covered.
- The wheel path, abi3 fixture and 68-module link inventory are green, but the
  pinned-wheel semantic gates are not.

### 6. Ecosystem (pinned differential)

Current measured 5/13 (python-dateutil, six, typing-inspection,
annotated-types, tzdata). Still open:

- NumPy import/smoke: `NameError: _NoValue`, then dtype/ArrayMethod semantics.
- pandas import/smoke: AOT `writer(lineterminator=...)`; JIT import/runtime and
  dynamic `except` handler shape.
- pydantic import/smoke: `annotationlib.Format.__eq__` and any later
  annotationlib/typing gaps; pydantic-core passes JIT but not AOT.
- typing-extensions: AOT `enum._proto_member.__set_name__`
  (`ValueError: int('keep')`).
- After the 13 cases: Tier-1 upstream suites (FastAPI/Starlette/uvicorn,
  Django, requests, SQLAlchemy, pytest) and Tier-2 packages from P4-10.

### 7. Platform, environment and resources

- Linux x86_64 is the verified target. aarch64 is untested; macOS/Windows and
  cross-compilation are open (Q-09).
- Nine stdlib modules remain CPython-failing on this host: Windows-only modules
  plus `_gdbm`; they must not be counted as Hydro failures, but each needs an
  explicit platform support declaration.
- Locale/UTF-8 availability, fork semantics, signal delivery and pty behavior
  depend on the host; the gate must record and bound these rather than silently
  skipping.
- The 1500 MB watchdog killed `test_zlib` and `test_zstd`; raise headroom
  only with measured RSS accounting, or reduce Hydro's peak allocation. The
  earlier 580 MB threshold also lost `test_zipfile64`.
- Fuzz campaigns (Q-03/Q-04), `unsafe` review (Q-05), debugger/DAP (Q-01) and
  long-running stress are still open.

## Execution order

1. **Stop the bleeding:** restore the workspace gate; finish the current
   Popen/subprocess/contextlib slices; rerun the full official suite and
   ecosystem gate to get a clean baseline.
2. **Root-cause clusters with the largest fan-out:** `'str' object is not
   callable` (12), regex backreferences (9), `_testinternalcapi` (6),
   class/type identity (6+), opcode/code-object consumers, Popen/pty/REPL,
   `test.support`/unittest, compile/eval.
3. **Core semantics:** class construction/MRO/descriptors/super, imports,
   exceptions/except*, generators/async, strings/codecs, numeric/container
   protocols. Each cluster gets a focused commit and a manifest update only
   when the oracle and Hydro both pass.
4. **Stdlib batches:** P7 Batches B–L in dependency order, with per-module
   CPython test subsets wired into the official runner.
5. **ABI/ecosystem:** NumPy/pandas/pydantic, then C-API semantic tests,
   ctypes, GIL/GC stress and upstream ecosystem suites.
6. **Release:** full gate on a clean checkout, regenerate all reports and the
   progress chart, run CI, then record the exact commands and results.

## Completion checklist

- [ ] Official `hydro_pass=398` (or strict final policy with zero unexplained
      tests).
- [ ] No `timeout`, `memkill`, `error`, `expected_fail` or `cpython_fail`
      without a documented oracle-equivalent reason.
- [ ] Every passing file executes its tests; no 0-test success.
- [ ] Stdlib comparable import matrix 100 % and per-module behavioral subsets.
- [ ] Ecosystem 13/13 and Tier-1 upstream suites.
- [ ] C-API/ABI inventory and semantic stress gates green.
- [ ] Workspace tests, clippy, fmt, manifest, benchmarks and CI green.
- [ ] Reports regenerated: `AGENTS.md`, `CHANGELOG.md`, `README.md`,
      `docs/TODO.md`, `docs/COMPATIBILITY.md`, `docs/CPYTHON_TESTS.md`,
      `docs/ECOSYSTEM.md`, `docs/C_API.md`, `docs/progress.svg`.
