# Changelog

All notable changes to PyX. The format follows
[Keep a Changelog](https://keepachangelog.com/de/1.1.0/), versioning follows
[SemVer](https://semver.org/lang/de/). As long as no public release
exists, `[Unreleased]` collects the current state; `0.1.0` is the
initial state of the first runnable core.

## [Unreleased]

As of 2026-10-25: **1329 tests**, `scripts/verify.sh` ALL GREEN. The official
CPython suite has its first real pass (`test_errno.py`), recorded in the
manifest. The first abi3
extension slice runs under JIT and AOT without `libpython`; the full C
API/ABI, stdlib parity and ecosystem work remain open (see `docs/C_API.md`).
58/297 standard-library modules are importable (13 builtin/frozen, 41 pure,
4 extension). `benchmarks/bench_all.py` does **not** yet run under PyX: its
first blocker is `rich/console.py` (see the package-loader entry below).

### Added

- **Package/site-packages loader and official `rich` source:** `check_file`
  now resolves packages (`__init__.py`), dotted names, submodules and
  relative imports, in this order: sibling files, the package-chain root,
  `PYX_SITE_PACKAGES`, the `VIRTUAL_ENV`/`CONDA_PREFIX` site-packages trees
  and `PYTHONPATH`. Every unit loads once per canonical path; imported
  module bodies run once as initializers, and nested imports remain deferred
  until the enclosing code runs. A native `rich` implementation is no longer
  shipped: the official pure-Python `rich` distribution resolves through
  this loader. `benchmarks/bench_all.py` still does not run under PyX; the
  first concrete failure is `rich/console.py` (`dataclasses.field`,
  `os.PathLike`, ellipsis, `del`, `get_ipython`, `sys.stdin`/`__stdin__`/
  `__stdout__`/`__stderr__`, `os.get_terminal_size`/`open`/`dup2`,
  `SystemExit`, `input()`, `threading.local`), and further `rich` modules
  will likely add more.
- **List slice assignment:** `xs[start:stop:step] = value` lowers to the new
  `StmtKind::SetSlice` (omitted bounds are `Const::None`) and the
  `pyx_slice_set` ABI. Python's evaluation order is preserved (right-hand
  side first, then the target expression, then the bounds in source order).
  A `step` of 1 splices any iterable into the list (growing, shrinking or
  inserting); extended slices replace the visited positions and raise
  `ValueError` on a length mismatch. Bounds follow `slice.indices`, the
  right-hand side is materialised before mutation (so `xs[:] = xs` works),
  and inserted edges publish GC insertion barriers while the list is locked.
  Immutable `str`/`bytes`/`tuple` targets raise `TypeError` (statically for
  known types), a dict target reports `unhashable type: 'slice'`, and
  augmented slice assignment (`xs[a:b] += v`) remains a clear diagnostic.
  `examples/slices.py` and the runtime/frontend/codegen suites cover head,
  middle, tail, empty and full slices, negative bounds, extended steps,
  length errors, non-list targets and evaluation order.
- **`ord`/`chr`, assignment expressions and chained comparisons:**
  `ord(character)` returns the Unicode scalar value of a one-character `str`
  (`TypeError` for any other length or a non-`str`) and `chr(codepoint)`
  returns the one-character `str` for an integer in `0..=0x10FFFF`
  (`TypeError` for a non-integer, `ValueError` for an out-of-range or
  surrogate code point, which PyX's UTF-8 strings cannot represent). Direct
  calls use the new ABI entries `pyx_ord`/`pyx_chr`, so they allocate nothing.
  `(name := value)` lowers to the new `ExprKind::NamedExpr`, which stores the
  value in the enclosing function's local and yields it; a walrus inside a
  comprehension binds the enclosing scope (rebinding the iteration variable
  is rejected), and a name bound only by a walrus is not part of the
  statement-target pre-pass, so reads before it or captures of it from
  earlier nested functions stay unresolved (documented limitation). `a < b <
  c` lowers to a short-circuit `And` of pairwise comparisons; every middle
  operand is evaluated exactly once through a hidden `$cmpN` local, for
  arbitrary chain length and every comparison operator.
- **Shared `rich/style.py` surface:** `random.getrandbits(k)` (SplitMix64,
  `0 <= k <= 60`), `hash(value)` (`pyx_hash`; dict-key hash, unhashable
  values raise `TypeError`), unary `~` on `int`/`bool` (`UnOp::Invert`,
  `pyx_unop` code 3), the reserved `NotImplemented` singleton constant,
  native `sys.stdout`/`sys.stderr` streams (`write`, `flush`, `isatty`), and
  user-class exception handlers: `raise module.Class(...)` and non-builtin
  `except` targets (including dotted and imported classes) use the existing
  `RuntimeError` fallback, documented as approximate. Non-literal parameter
  defaults (`color_system=ColorSystem.TRUECOLOR`) now compile with `None`
  metadata instead of being rejected.
- **`assert`, `re` flags and the `@rich_repr` class decorator:**
  `assert test[, msg]` lowers to a new `StmtKind::Assert`; codegen branches on
  the test and evaluates `msg` only on failure, raising `AssertionError`
  (tag 14, catchable by `except Exception`). `re` exposes the `RegexFlag`
  members (`VERBOSE`/`X`, `IGNORECASE`/`I`, `MULTILINE`/`M`, `DOTALL`/`S`,
  `ASCII`/`A`, `UNICODE`/`U`, `TEMPLATE`, `LOCALE`, `DEBUG`, `NOFLAG`) as
  ints with CPython's values, `re.compile(pattern, flags=0)` forwards the
  bits it honours to the regex builder, and a compiled pattern keeps them
  for every method call (plus a `.flags` attribute). `@rich_repr`
  (`rich.repr.rich_repr`) is accepted as a no-op class decorator, like
  `@dataclass`: the class keeps the methods it declares and only the
  generated `__repr__` is absent.
- **`tuple`/`hasattr`, starred unpack targets, user-class raises and
  `functools.partial` keywords:** `tuple()`/`tuple(iterable)` build a tuple
  (new ABI `pyx_tuple_from`) and `hasattr(obj, name)` reports attribute
  existence, swallowing only `AttributeError` (`pyx_hasattr`). `a, *rest, b
  = value` binds a fresh list of the middle elements through
  `pyx_unpack_star`; one star is allowed and nested or duplicated stars are
  compile errors. `raise ReprError("msg")` for a class declared in the unit
  lowers to `RuntimeError("ReprError: msg")` (cause preserved, message
  `str()`-rendered); `except ReprError` now matches that `RuntimeError` tag
  (approximate; see the shared style surface entry). Generator
  functions carry their positional parameter names (`pyx_function_params`),
  so `functools.partial(f, kw=value)` forwards keyword names through the new
  `pyx_module_call_kw` ABI and binds them by parameter name when the partial
  is called; targets without names raise `TypeError` instead of dropping the
  keywords.
- **`repr`, general starred calls and bitwise operators:** `repr(value)`
  returns the runtime's Python representation (`pyx_to_repr`); a general
  call with one starred positional operand (`f(a, *items)`) lowers to
  `IndirectCall { star }` and runs through the new ABI
  `pyx_call_star(fn, argc, argv, star)`, which flattens a list/tuple and
  raises `TypeError` otherwise (method calls keep `pyx_method_star`);
  `&`, `|`, `^`, `<<` and `>>` are compiled (`BinOp` codes 17–21) with
  Python int/bool semantics, `TypeError` for other operands and
  `ValueError`/`OverflowError` for negative or out-of-range shifts.
- **Exception classes as values:** built-in exception names (`BaseException`,
  `Exception`, `ValueError`, `TypeError`, `RuntimeError`, `OSError`,
  `FileNotFoundError`, `AssertionError`, `GeneratorExit`, ...) are first-class
  class objects, so `class E(Exception):` and `class F(E):` work, and
  `E.__name__`, inheritance, instantiation and `isinstance` behave like any
  user class. The names resolve in expression contexts after locals, imports,
  functions and module globals; `raise`/`except` keep their tag-based
  lowering. The hidden ABI `pyx_exception_class(tag)` returns one cached
  native class per tag (thread-local cache cleared by `pyx_runtime_init`).
- **Unpacking targets:** `a, b = ...` accepts any iterable source (including
  `iter(...)`, generators, dicts and ranges) and supports nested tuple/list
  targets as well as attribute and subscript targets such as
  `self.left, items[0] = pair`. Binding stays left to right with Python's
  evaluation order; mismatched lengths keep their `ValueError` messages. The
  new ABI `pyx_unpack_seq` returns the sequence to bind (materialising it only
  for general iterables) so codegen can read the elements safely.
- **Deferred imports:** imports nested in functions, classes or compound
  bodies are recorded but no longer loaded while compiling. Names that can be
  linked to an already-visible dependency bind normally; all other bindings
  lower to `DeferredImport`, which calls the new ABI `pyx_deferred_import` and
  raises `RuntimeError: dynamic import of '<module>.<member>' is not
  supported` only if the enclosing code runs. Top-level imports still load
  and initialize eagerly, and optional dependency graphs (for example
  `rich/_extension.py`) stay out of programs that never call them.
- **Formatting:** f-string format specs (`:.2f`, `:04d`, `:#x`, alignment/
  fill character/sign/grouping) as well as `!s`/`!r`/`!a`; `str.format(...)`;
  printf formatting (`"%s:%d" % (...)`); shared spec implementation and
  ABI `pyx_format_value`.
- **Attribute & method keywords:** `pyx_getattr`/`pyx_setattr` and
  data attributes for `pathlib.Path` (`name`, `stem`, `suffix`, `parent`,
  `path`), files (`name`, `mode`, `closed`, `encoding`), `Thread` and
  `Process`; `pyx_method_kw` for native methods (e.g.
  `Path.mkdir(parents=True)`).
- **Builtin function values:** builtins can be used as values (`f = int`,
  `map(f, xs)`).
- **Classes:** `class Name(Base)` with one base, methods, `__init__`,
  inheritance/overrides, instance attributes, and dynamic method dispatch;
  instances/classes have deterministic representations. Class bodies also
  support frozen class attributes (`x = value`, annotated `x: T = value`,
  `__slots__` accepted and ignored), `@property`/`@classmethod`/
  `@staticmethod` binding kinds, ignored `lru_cache`/`cache`/`wraps`/
  `abstractmethod`/`overload`/`runtime_checkable` decorators, class-form
  `typing.NamedTuple` field synthesis, and `instance.__class__` /
  `Class.__name__`.
- **Closures:** nested `def` captures outer locals **by value**;
  ABI `pyx_closure_new`, captures as leading wrapper parameters.
- **Lazy generators:** `yield value`/bare `yield` run in a stackful
  fiber (`core/fiber.rs`, own 128 KiB stack) that starts only on the first
  `pyx_iter_next`; every generator function creates an independent
  object, `return` ends the iteration, exceptions propagate. On x86_64,
  a `global_asm!` `pyx_context_switch` switches only the callee-saved
  registers (`rsp`, `rbx`, `rbp`, `r12`–`r15`, `ret`), without syscalls;
  `ucontext` remains the fallback for other targets. ABI
  `pyx_generator_new`/`pyx_yield`; new builtins `iter(x)` and
  `next(it[, default])` (`pyx_iter`/`pyx_next`). Generator expressions
  remain eagerly materialized.
- **Fiber performance:** the register-only switch costs two switches per
  `yield` without syscalls; a 100k generator sum runs in release at CPython
  11.0 ms / JIT 7.7 ms / AOT 6.8 ms, `strace -c` counts 94 syscalls in total
  instead of 200,097 (two `rt_sigprocmask` per yield).
- **Coroutines/event loop:** `async def` creates coroutine objects
  (`pyx_coroutine_new`), `await`/`pyx_await` drives them; `asyncio.run` runs
  a cooperative single-thread loop, `asyncio.sleep` suspends only the
  current fiber (blocking outside a loop), `asyncio.gather`
  starts concurrent tasks in input order and propagates the
  first error. No GIL.
- **GC statistics:** `pyx gc [--json] <file.py>` (runs like `pyx run`, then
  collections/live_objects/threshold/pause_total_us/pause_max_us/
  mark_total_us/sweep_total_us/freed_total/stress), public API
  `gc_stats()`/`reset_gc_stats()`/`GcStats`; `PYX_GC_STATS=1` remains.
- **Dict hash index:** `DictData` (insertion order + hash index with
  Python equality, first position/last value, numeric key equivalence,
  reindex on `pop`); lookup/insert O(1) on average instead of linear.
- **Int fast paths:** codegen emits checked `+`/`-`/`*` and comparisons for
  statically `Int`-typed operands (including AugAssign and fully
  int-typed `for range` counters); overflow falls back to `pyx_binop`.
- **Frontend optimizations:** constant folding (int/float/str/Binops/
  comparisons/unary, overflow- and division-by-zero-protected) and
  dead-`if`/`while False` elimination.
- **Benchmarks:** `benchmarks/bench_pyx.py` (CPython/JIT/AOT, 11 workloads
  incl. lazy generator sum and async `gather`, output-equality gate),
  registered as `pyx` in `benchmarks/bench_all.py`
  (`uv run benchmarks/bench_all.py --only pyx`).
- **`with` protocol:** `__enter__`/`__exit__` for files, `Pool`, and
  user classes, nested items in reverse order;
  augmented subscript assignment evaluates the target object/index once.
- **User modules:** `import foo` / `from foo import f` via sibling files,
  module cache per canonical path, cycle detection, `__name__`, and
  AOT embedding; imported modules must not have top-level side effects.
- **Nested imports:** imports inside function bodies and compound blocks
  (`if`/`for`/`while`/`try`/`with`/class bodies) are resolved while
  compiling, so `rich/_extension.py`'s `from rich.pretty import install`
  links directly and imported names stay order-independent. A deferred
  import that closes a cycle, or a member of an ancestor package that is
  still initializing, binds late through the runtime global table; cycles
  made only of module-level imports stay errors, and `if TYPE_CHECKING:` /
  `if __name__ == "__main__":` blocks are not linked.
- **Stdlib:** `os.path`, `argparse` (subset, `Namespace`),
  `collections.defaultdict`, `re` (Rust `regex`, `Pattern`/`Match`),
  `socket` (blocking TCP), `asyncio` (`run`/`sleep`/`gather`).
- **CLI/toolchain:** `pyx new`, `init`, `test` (plain programs, `--json`),
  `fmt` (`--check`), `lint` (`--json`), `clean` (`--dry-run`); `add`/`remove`/
  `sync`/`lock` as uv passthrough (`PYX_UV`).
- **Build cache:** `.pyx/cache` with a deterministic key over source code,
  reachable modules, target, version, and runtime archive; `--no-cache`,
  `pyx clean`.
- **Scaling benchmarks:** `pool_serial`, `pool_parallel`, `process_queue` in the
  micro harness and in the baseline/threshold file.
- **Documentation:** `docs/C_API.md` (P4-04 spike incl. environment probe and
  blocker), `docs/LAYOUT.md` (source tree), updated
  `docs/ARCHITECTURE.md`, `README.md`, `AGENTS.md`, `docs/TODO.md`.

### Changed

- **Source restructuring:** `pyx-ir/{model,ir,stdlib}`,
  `pyx-frontend/{lower,sema}`, `pyx-codegen/{backend,lower}`,
  `pyx-runtime/{abi,core,gc,modules,concurrency,support,csv}`,
  `pyx-cli/{cli,cli/commands}`; no behavior change, contracts unchanged.
- Test count increased from 545 to 721 tests (incl. formatting, attributes,
  classes, closures, stdlib modules, socket, user modules, CLI/build cache);
  with this slice to **905** tests (incl. `crates/pyx-cli/tests/fibers.rs`,
  dict hash index, GC statistics, generator iteration).
- AOT builds embed user modules; the cache key takes the
  entire reachable module tree structure into account.
- **Fiber GC roots:** `ctx`/`caller_ctx` (callee-saved register frames) and
  the fiber stack are lifetime-registered conservative roots;
  `push_stack_bounds` aligns the stack scan to a running fiber. The
  outermost resume publishes two aux ranges (native driver stack and
  `caller_ctx`), shared mode mirrors them, local mode gets by without atomics per
  switch. `PYX_GC_STRESS=1` covers interleaved generators.
- `bench_all.py` resolves its output directory absolutely so that child scripts
  write into the same directory.
- **Native module dispatch cache:** `pyx_module_call`/`call_native` borrow
  names without allocating and cache the resolved spec per thread; `math_loop`
  moved from 0.81x to parity/faster (AOT 22.50 ms vs. CPython 22.65 ms at
  n=200000; 50k micro harness 6.2 ms vs. 13.6 ms).
- **Exception hierarchy & chaining:** `except Exception` /
  `BaseException`, tuple handlers (`except (A, B) as e`), explicit
  `raise ... from ...` with deepest-first chained traceback and
  `from None` suppression; causes are GC-traced. Implicit context chaining
  and `except*` remain open.
- **Generator protocol complete:** `yield` expressions (`x = yield v`),
  generator `send(v)`, `throw(exception_instance)`, `close()` (GeneratorExit,
  `finally` execution, RuntimeError when a closing generator yields) and
  `yield from` (send/throw/close forwarding, delegate return value);
  StopIteration on exhaustion; tests against CPython behaviour.
- **CLI polish (P6-07):** `pyx completions <bash|zsh|fish>`, roff manpage
  `docs/pyx.1`, and `pyx --version --verbose` (target, backend, runtime).
- **C-API containers:** `PyUnicode_*`, `PyFloat_*`, `PyBytes_*`, `PyList_*`,
  `PyTuple_*`, `PyDict_*` plus bounded recursive list/tuple/dict conversion
  in both directions (copies, depth 64, cycle rejection).
- **C-API capsules & buffers:** `PyCapsule_New/GetPointer/GetName/IsValid/
  SetPointer` with a PyX `PyCapsule` kind, read-only contiguous bytes
  `PyObject_CheckBuffer/GetBuffer/ReleaseBuffer` (verified `Py_buffer`
  layout), and `PyExc_*` singletons with typed `PyErr_SetString` mapping.
- **Installed extension discovery:** extension lookup also searches
  `PYTHONPATH`, `PYX_SITE_PACKAGES`, `VIRTUAL_ENV`/`CONDA_PREFIX`
  site-packages, and wheel-style `<name>.cpython-314-*.so` filenames.
- **Dotted extension submodules & ABI policy:** `pkg.sub_ext` resolves to
  `<dir>/pkg/sub_ext*.so` without package Python source (aliased and
  from-import forms); incompatible ABI tags (`cpython-312`, `pp310`, ...) are
  compile-time diagnostics instead of dlopen attempts.
- **Multi-phase init:** `PyModuleDef_Init` + bounded `Py_mod_exec` slot
  execution with policy-slot validation, alongside single-phase
  `PyModule_Create2` modules; JIT+AOT fixture.
- **CPython 3.14.9 target:** single source of truth in `pyx_ir::compat`;
  `sys.version`/`version_info` report 3.14.9 (PyX identity via
  `sys.pyx_version`), `platform.python_version()` and the CLI verbose output
  follow; the wheel-tag policy accepts `cpython-314-*`.
- **Official CPython test gate:** `scripts/cpython_compat.py` runs the 3.14.9
  `Lib/test` suite differentially, writes JSON/Markdown, and supports
  `--gate`, `--update-manifest` and offline `--check-manifest`; the manifest
  starts empty and grows only from real passes. Baseline (2026-10-25, 398
  modules): **1 passing** (`test_errno.py`), 334 `pyx_fail`, 63
  `expected_fail`, 0 timeouts. `scripts/verify.sh`/CI check the manifest
  offline.
- **First official Python test:** unmodified `Lib/test/test_errno.py` passes
  under JIT and AOT. Adds first-class standard-library module values
  (`pyx_module_value`, module-aware `getattr`), module-level globals
  (`pyx_global_get`/`pyx_global_set`), `frozenset`, `str.isupper`/`islower`,
  `AssertionError` (tag 14), a native `errno` module (Linux table,
  `errorcode`, `__dict__`) and a minimal native `unittest`
  (`TestCase`/runner/native assertions).
- **Benchmark-critical stdlib batch:** `subprocess`, `tempfile`, `shutil`,
  `textwrap` and `timeit` are native and API-tested; `pyx run` now forwards
  program arguments into `sys.argv`, with `sys.executable`, `sys.exit`,
  `os.environ`/`os.access` and argparse default-argv/help support.
  `bench_all.py --only pyx` completes under CPython; import baseline at that
  point 47/297 (now 58/297).
- **Batch B native codec/text core:** `binascii`, `base64`, `quopri`,
  `fnmatch`, `getopt` and `mimetypes` are native and API-tested, bringing the
  import baseline to 42/297 (now 58/297). The remaining Batch B modules still
  need the pure-Python pipeline.
- **Standard-library program (P7) and Batch A:** added a complete
  module-by-module plan for all 272 missing stdlib modules with dependency
  batches and a per-layer ledger. First slice implements native
  `bisect`/`_bisect`, `heapq`/`_heapq`, `colorsys`, `keyword`,
  `stat`/`_stat`, plus `_operator`, `_functools` and `_io` aliases; import
  baseline rises to 36/297. CPython per-module test subsets remain P7-05.
- **LTS policy:** future PyX LTS lines are keyed to CPython minor versions
  (3.13/3.14/3.15) with frozen patch baseline, ABI/wheel tags, stdlib surface
  and support window (P4-12; no LTS before production).
- **Clippy gate:** `scripts/verify.sh`/CI enforce `-D warnings`; the tree is
  warning-free.
- **Stdlib compatibility baseline:** `scripts/stdlib_compat.py` +
  `docs/COMPATIBILITY.md` + `benchmarks/compat/latest.json`; 23 of 297
  CPython 3.14 modules importable on 2026-10-24 (import success only, not
  test-suite success).
- **First abi3 extension slice:** `pyx-runtime/src/capi/` implements a
  limited-API host surface (`PyModule_Create2`, `PyModule_AddStringConstant`/
  `AddObject`, `PyLong_From/AsLong`, variadic `PyArg_ParseTuple` via a small C
  file, refcount entry points, `PyErr_*` basics, symbol anchor). The frontend
  discovers sibling `.so`/`.abi3.so` files, lowers extension calls/attributes,
  and the loader (`modules/extension.rs`) caches modules by path. `pyx` and AOT
  binaries are linked with `--export-dynamic`; the fixture imports and calls
  under JIT and AOT with no `libpython` (`crates/pyx-cli/tests/extensions.rs`).
  Open: package submodules, multi-phase init, wheel tag policy, full ABI.
- **P4-04 decision (2026-10-24): option B** — a PyX-native CPython C-API/ABI
  layer without embedded `libpython`; CPython 3.14 headers/toolchain are used
  at build time and the interpreter as a differential test oracle. Probes:
  embedded CPython init 6.67 ms + finalize 7.46 ms, `import numpy` 41.64 ms;
  a stable-ABI fixture imports with four undefined CPython symbols
  (`spikes/p4_04/`).
- **Full-compatibility program:** P4-09 (297 stdlib modules: 174 pure Python,
  71 extension, 52 builtin/frozen), P4-10 (ecosystem suites: NumPy, pandas,
  pydantic, FastAPI, Django, …) and P4-11 (ecosystem benchmarks) added to
  `docs/TODO.md`.

### Known limitations

- Generators/coroutines: `send`/`throw`/`close`, `yield` expressions,
  async generators and async comprehensions are missing;
  `asyncio` offers only `run`/`sleep`/`gather`, `async for`/`async with` use
  the synchronous protocol. Generator expressions remain eagerly materialized.
- Performance weaknesses: string concatenation is O(n²) and float boxing is
  per-value; extension boundaries will add conversion cost. Native module
  dispatch and generator resume are no longer bottlenecks (math_loop at
  parity; 100k generator sum AOT 6.8 ms vs. CPython 10.2 ms).
- Classes with only one base and without class attributes/decorators/metaclasses/
  descriptors; closures are by-value (no mutable capture).
- User modules only as sibling `.py` (no packages/dots/relative
  imports), without top-level execution and without runtime module objects.
- `re` uses Rust `regex` syntax (no lookarounds/backreferences);
  `socket` only blocking TCP; `random` not CPython-reproducible.
- GC conservative, without precise roots/stack maps or size-class allocator.
- C API/C extensions: decision B (PyX-native ABI layer, no `libpython`).
  A limited-API fixture runs under JIT and AOT, but the shim covers only the
  first symbol subset (no capsules/buffer/container constructors, no
  wheels/venvs, no full 3.14 ABI). The 297-module stdlib parity, ecosystem
  suites and benchmarks are open (P4-04a…P4-11).

## [0.1.0] - 2026-10-24

Initial state of the first runnable development stage (545 tests): compiler,
runtime, and CLI for a defined Python subset, secured by `scripts/verify.sh`.
Later slices are listed under `[Unreleased]`.

### Added

- **Compiler:** Python parser (`rustpython-parser`), AST lowering into a
  typed IR, type inference/diagnostics in the format `file:line:col: error: …`.
- **Codegen:** Cranelift-based JIT path (`pyx run`) and AOT path
  (`pyx build`, object file + link against `libpyx_runtime.a`).
- **Runtime:** tagged i64 values, strings/floats/exceptions, dynamic
  operators, truthiness, `str()`/repr, tracebacks with frame/line references.
- **CLI:** `pyx check`, `pyx run`, `pyx build`, `pyx doctor`, `--json`,
  `--color`, defined exit codes (0/1/2).
- **Containers:** `list`, `dict`, `tuple`, `set`, `range`, iterators, slices,
  negative indices, structural equality, `len`/`in`.
- **Language features:** comprehensions (list/set/dict/generator expression),
  first-class functions and lambdas (without closures), `map`/`filter`/`reduce`,
  `enumerate`/`zip`/`sum`, `try`/`except`/`else`/`finally`, re-raise,
  f-strings (simple expressions), `with` (close protocol).
- **Modules/imports:** `math`, `functools`, `operator`, `time`, `sys`, `json`,
  `os` (basic), `io`, `csv`, `random`, `platform`, `statistics`, `pathlib`.
- **File I/O:** text and bytes mode (`r/w/a/x`, `rb/wb/ab/xb`), CSV reader/
  writer, `json.dump`/`load`; `bytes` literal/constructor, indexing, slicing,
  iteration, core methods, hex serialization.
- **Memory management:** conservative mark-and-sweep GC with shared heap,
  insertion barriers, concurrent marking, and concurrent sweep;
  `PYX_GC_THRESHOLD`, `PYX_GC_STRESS`, `PYX_GC_STATS`.
- **Concurrency (no GIL):** `_thread`, `threading` (`Thread`, `Lock`,
  `RLock`, `Event`), `queue.Queue`, `multiprocessing` (`Process`, `Queue`,
  `Pool`, fork and spawn), `pickle` with its own PYX1 format.
- **Diagnostics (P6):** stable error codes, spans, source snippets, help/
  hints, `--json` schema, `pyx doctor`, ICE handling with exit code 2.
- **Verification:** `scripts/verify.sh` (fmt, Clippy, build, tests, E2E for
  JIT and AOT, negative tests, GC stress), benchmark harness
  `benchmarks/pyx/run_micro.py` with CPython/JIT/AOT baseline.
- **Documentation:** `docs/ARCHITECTURE.md`, `docs/TODO.md`, `AGENTS.md`,
  `benchmarks/PYX.md`, `PLAN.md`.
- **License:** MIT OR Apache-2.0.

### Known limitations

- No classes, generators/`yield`, closures, `async`/`await`, decorators,
  pattern matching, BigInt, or C API/ABI compatibility (as of this
  initial cut; added later under `[Unreleased]`).
- Only a subset of the CPython stdlib; imports only at module level and only
  for the built-in modules.
- `pickle` is not CPython-compatible; dict insertion is linear, integer
  arithmetic limited to i64.
- Linux only (x86_64/aarch64); `uv`-based toolchain (phase 3) still missing.
