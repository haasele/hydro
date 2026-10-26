# Changelog

All notable changes to Hydro. The format follows
[Keep a Changelog](https://keepachangelog.com/de/1.1.0/), versioning follows
[SemVer](https://semver.org/lang/de/). As long as no public release
exists, `[Unreleased]` collects the current state; `0.1.0` is the
initial state of the first runnable core.

## [Unreleased]

### Added — 2026-10-26 four-scope checkpoint

- Import/module registry: `sys.path`/`sys.modules`/`sys.builtin_module_names`,
  stable module values, namespace packages, `HYDRO_STDLIB`, star imports and
  literal `__all__`; `importlib.import_module` consults the registry.
- Full CPython 3.14 exception taxonomy with parent-chain matching,
  exception attributes, `isinstance`/`issubclass`, and user-exception
  instance identity (methods, attributes, re-raise).
- Core shims: native `builtins`, `_imp`, `_opcode`, `_collections`,
  `_codecs`, `_string`, `_weakref` (real callbacks), `warnings`, `marshal`,
  `_tokenize`, `_collections_abc`, `gc`, `atexit`, `site`.
- Vendored `hydro-python-parser` fork: PEP 701 f-strings and PEP 750
  t-strings with real `Template`/`Interpolation` runtime values.
- P4-02 surface: general decorators, positional-only parameters, combined
  `*args`/`**kwargs`, `match`, `except*` syntax, classmethod/staticmethod/
  property descriptors, bytearray/memoryview, live dict views, descriptors,
  C3 multiple inheritance, metaclasses and general `super()`.
- Full-ABI foundation: CPython-compatible object/type headers, refcounts,
  vectorcall/FASTCALL, full buffer protocol, GIL emulation, GC tracking,
  `PyArg_ParseTupleAndKeywords`, constants and exception symbols.
- Tea `install` command with atomic wheel extraction, tag validation and
  extension import through the existing loader.

**Still open:** dynamic import of source, `compile`/`eval`/`globals`/`locals`/
`vars`, class-body `del`, `builtins.__dict__`, `ast.parse/unparse`,
`except*` grouping, NumPy/pydantic ABI compatibility, and full stdlib/
ecosystem parity.

As of 2026-10-26 the full suite is green (`scripts/verify.sh` ALL GREEN;
current test count and breakdown in [`AGENTS.md`](AGENTS.md#current-state)).
P2 is complete: precise shadow-stack roots, the size-classed payload
allocator, async generators/TaskGroup, the standard-library core plus
TLS/UDP/non-blocking networking, argparse/pytest, the IR interpreter/REPL,
and the fuzz/shutdown gate are in place. The official CPython suite still has
one real pass (`test_errno.py`); the first abi3 extension slice runs under JIT
and AOT without `libpython`. Full P7 stdlib parity, the C API/ABI and
ecosystem work remain open. `benchmarks/bench_all.py --only hydro` now renders
the rich report at the **default** GC threshold.

### Changed

- **Project split into Hydro + Tea.** This repository is now the Hydro
  runtime/compiler monorepo. The user-facing toolchain, E2E/AOT/pytest/REPL
  tests and packaging live in the separate **Tea** repository. The former
  `pyx` ABI prefix is now `hydro_` (`hydro_run`, `hydro_main`, `hydro_*`
  runtime symbols), environment variables are `HYDRO_*`, caches are
  `.hydro/`, and the Python-facing implementation name is `Hydro`. CLI
  commands use `tea`; `tea run` is uv compatibility mode, `tea infuse` runs
  the Hydro JIT, `tea steep` records PGO flavors, and `tea brew`/`tea build`
  emits AOT binaries (optionally `--flavor`).
- **Profile-guided flavor support (Hydro side):** new
  `hydro_codegen::CodegenOptions`/`run_jit_with_options`/
  `compile_object_with_options`, `hydro_profile_hit/reset/dump` ABI, and
  hot-function ordering for AOT builds. Default codegen output is unchanged.

### Added

- **Size-classed payload allocator (P2-05):** `HydroObj` is a 16-byte header with
  a typed payload pointer; 18 payload size classes, per-class slabs/free lists,
  exact large-object fallback, 16-byte alignment, cross-class absorb/clear and
  per-class/peak-byte statistics. Typed `ObjDataRef`/`ObjDataMut` views and
  kind-dispatched drop replace the single inline enum cell. Tests cover class
  selection, alignment, reuse, large objects, mixed reclamation and bounded
  RSS.
- **Parallel fuzz and shutdown gate (P2-20f):** libFuzzer plus stable
  fallback targets for frontend/codegen and runtime/GC, a bounded CI smoke
  script, and `scripts/fuzz-parallel.sh` to soak every core with independent
  seeds. Unreachable started processes, pools and multiprocessing queue
  endpoints now clean themselves up through bounded Drop hooks;
  `tests/shutdown.rs` verifies child-free shutdown through `/proc`. The fuzz
  campaign found and fixed recursive cyclic-container repr, unrooted
  `splitlines`/`list.extend(generator)` builders and instance `__dict__`
  construction.
- **Persistent REPL and `-c`/`-i` (P2-14):** `tea repl` buffers incomplete
  blocks to a blank line, echoes the trailing expression, stores `_`, keeps
  imports initialized once, continues after exceptions, and flushes
  source-bound module names into the session global table. `tea -c` and
  `tea -i` share the same interpreter session without resetting the heap.
- **pytest-compatible runner (P2-13):** `tea pytest [paths...]` discovers
  `test_*.py`/`*_test.py` files and runs each in an isolated child process,
  aggregating results with exit codes 0 (pass), 1 (failure), 2
  (collection/internal) and 5 (no tests). The native `pytest` module supplies
  `raises` (real `with` suppression, `match=`, `.value`/`.type`), `approx`,
  `fail`, `skip`, `xfail`, `fixture` and `mark.*` identities. Test modules use
  `@pytest.fixture` (function and module scope, `yield` teardown), ancestor
  `conftest.py` fixtures, compile-time `parametrize` expansion, skip/skipif/
  xfail markers, and pytest assertion messages (the statement source text).
  `Test*` classes with `test*` methods are collected through a runtime class
  registry. `-q`, `-x`, `--maxfail`, `-k` and `--json` are supported;
  `tea test` keeps its existing exit-status contract.
- **argparse and `SystemExit` (P2-13):** `sys.exit()` now raises a catchable
  `SystemExit` instead of terminating the process; uncaught it prints no
  traceback and uses the stored code (`None`/`0` → 0, integers masked to one
  byte, strings → stderr plus 1). `argparse` gained the constructor
  parameters, all standard actions, `nargs` (`None`/`?`/`*`/`+`/`N`),
  argument and mutually exclusive groups, subparsers, `set_defaults`,
  `parse_known_args`, the scanner (`--opt=value`, short clusters, `--`,
  abbreviations, negative numbers), CPython-style help/usage rendering and
  the `Namespace`/formatter/`SUPPRESS`/`FileType`/`Action` exports. Parse
  errors raise `SystemExit(2)`; `-h`/`--version` raise `SystemExit(0)`.
- **IR tree-walk interpreter (P2-14):** `tea infuse --interpret <file.py>` runs
  the typed IR without Cranelift through the new `hydro-interp` crate. It shares
  the runtime value model and ABI operators, executes module initializers
  once, covers every `ExprKind`/`StmtKind` (evaluation order, loops/`else`,
  `try`/`except`/`else`/`finally` with bare and chained raise, classes,
  closures/cells, comprehensions, unpacking, slices, generators and async via
  runtime fibers), and registers precise GC roots for every frame and
  temporary. IR functions materialise as runtime host function values, so
  native callbacks (`sorted(key=...)`, `threading.Thread` targets,
  `functools.partial`) and class/method dispatch re-enter the interpreter.
- **Async generators, tasks and `TaskGroup` (P2-12):** `async def` with
  `yield` creates a lazy async generator with the real
  `__aiter__`/`__anext__`/`asend`/`athrow`/`aclose` protocol and
  `StopAsyncIteration` completion; `async for`, `async with` and async
  comprehensions use it. `asyncio` keeps a persistent thread-local loop whose
  scheduled tasks are a heap persistent root, adds `create_task`/
  `current_task` and task methods (`cancel`/`done`/`cancelled`/`result`/
  `exception`), delivers `CancelledError` at suspension resume and cancels
  leftover tasks at `run` shutdown. `asyncio.TaskGroup` joins its tasks,
  cancels siblings on the first failure and on a raising body, and propagates
  the first error (no `ExceptionGroup`). Built-in exception classes now
  construct instances (`ValueError('x')`) and `type(e)` returns the same
  cached class object that the exception name resolves to.
- **Precise GC roots (P2-06/P2-07):** generated frames and caller-owned value
  arrays are registered in a shadow-stack registry (`hydro_gc_frame_push`/
  `hydro_gc_scope_push`), published at safepoint/blocking slow paths and marked
  exactly by the collector; their byte extents are excluded from conservative
  stack scans. `HYDRO_GC_PRECISE=1` selects strict mode, which disables
  conservative stack/register scanning entirely. Persistent root slots cover
  long-lived Rust-side owners (asyncio loop tasks), and `GcStats`/`tea gc`
  report the mode plus the precise frame count.
- **Math intrinsic fast path:** `math.floor(math.sqrt(int))` lowers to an
  unboxed Cranelift f64 `sqrt`/floor with a runtime fallback for negatives and
  non-integers, removing one heap `float` per iteration and both native-module
  dispatches. `math.sqrt(int)` also avoids dispatch on the fast path.
- **Generator `sum` fast path:** `sum(generator)` drives the generator fiber
  directly instead of allocating/wrapping an iterator and re-checking the
  fiber kind/output pointer per yield. Errors, exhaustion, `start` values and
  `send(None)` semantics are unchanged.
- **Linear string accumulators:** a loop that only appends to a string local
  (`s = ""` ... `s += x` / `s = s + x`) now lowers to a mutable runtime string
  builder instead of copying the whole prefix per iteration, which removes the
  O(n²) growth of the `string_concat` benchmark pattern. The frontend applies
  the rewrite only after proving the local is a pure accumulator (seeded with
  a `str`, appended unconditionally at the top level of the loop body, never
  read, aliased, captured or deleted inside the loop, and read after it);
  everything else keeps the generic `hydro_binop` path, so aliasing, conditional
  appends, closure captures, early returns, module globals observed by other
  functions and non-`str` operands behave exactly as before (non-string
  operands still raise the same `TypeError`). The builder is GC-traced, is
  created right after the seed and finished right after the loop, so an empty
  loop yields the seed and `break` yields the partial text. ABI additions:
  `hydro_str_builder_new/append/finish` (registered for JIT and exported for
  AOT).
- **Mutable closures and `nonlocal`:** locals captured and mutated by a
  nested function now live in traced heap cells (`hydro_cell_new/get/set`,
  registered for JIT and exported for AOT), so outer mutations and
  `nonlocal` writes are shared between the enclosing function and every
  closure. Read-only captures keep by-value semantics; loop bodies create
  a fresh cell per iteration for mutated cells. `nonlocal name` resolves to
  the nearest enclosing binding and reports a stable diagnostic when the
  binding is missing or conflicts with `global`. ABI additions:
  `hydro_cell_new/get/set`.
- **Dependency-free fuzz smoke tests:** `crates/hydro-frontend/tests/fuzz_smoke.rs`
  drives random token soup, random Unicode, mutations of a valid program and a
  pathological corpus through `check_source`, asserting the public contract
  (no panic, non-empty diagnostics with the requested filename).
  `crates/hydro-runtime/tests/fuzz_smoke.rs` drives random ABI operation
  sequences (containers, operators, methods, iteration, unpacking, raising)
  built only from ABI values, asserting every pending exception exposes a
  tag/value and clears. Both use a fixed-seed SplitMix64 and stay under the
  2 s budget; no new dependencies.
- **Reproducible-build contract:** `docs/REPRODUCIBLE.md` records the AOT
  inputs (source bytes, source-name spelling, runtime archive, `CC`, linker),
  the same-host determinism evidence, and the open cross-host/path
  normalization items. `scripts/repro_check.sh` compiles one source four times
  (two uncached output paths, cached link plus cache hit), compares sha256 sums
  and runtime behavior, and runs in `release.yml` before packaging. The source
  path spelling is part of the contract: relative and absolute builds differ
  only in the embedded source name.
- **CI threshold policy test:** `scripts/test_check_benchmarks.py` now checks
  that `ci.yml` runs `check_benchmarks.py --check` exactly once with
  `continue-on-error: true`, that the harness self-tests are blocking, and that
  `--check` reports the same regression as the default mode. `benchmarks/HYD.md`
  no longer claims the gate is absent from CI.
- **Lazy generator expressions:** `(e for ...)` no longer builds an eager
  list. The frontend synthesizes a generator function whose hidden first
  parameter is the outermost iterable (evaluated when the generator is
  created) plus one parameter per captured enclosing local, and codegen calls
  it through the existing fiber ABI. Every later clause, filter and the
  element run only on resume, so `next`/`for`/`sum`/`list` consume a single
  pass, exhaustion raises `StopIteration`, and body exceptions propagate on
  resume. List/set/dict comprehensions stay eager. Assignment expressions
  inside a generator expression are rejected (their target would have to bind
  in the enclosing frame). Tests cover laziness/partial consumption, captures,
  tuple targets, multiple clauses/filters, exceptions, and JIT+AOT.
- **Implicit exception context:** a `raise` inside an active handler now
  records `__context__` and renders "During handling of the above exception,
  another exception occurred:" with deepest-first tracebacks; `__cause__`
  still wins, and `__suppress_context__`/`raise ... from None` suppress the
  implicit chain. Unit tests cover chain rendering and suppression.
- **CLI feedback and diagnostic spans:** global/per-command
  `--verbose`/`--quiet` (conflict exits 2), phase timings and cache decisions
  for `run`/`build`, multi-character semantic spans
  (`sema::simple_expr_end`), and ICE classification tests; the diagnostic
  catalog is `docs/DIAGNOSTICS.md`.
- **Native `html` and `contextlib` shims:** `html.escape`/`unescape` and
  `contextlib.suppress` (+ the internal suppress enter/exit) are importable,
  advancing the import matrix to 60/297.
- **BigInt serialization:** HYD1 stores a sign byte plus big-endian magnitude
  and normalizes a canonical small payload back to the tagged integer;
  containers, pickle and process queues round-trip arbitrary-precision ints.
- **Deterministic container/GC property tests:** `hydro-runtime/tests/container_properties.rs`
  and `gc_properties.rs` drive list/dict/set/tuple and allocation/collection
  sequences with a dependency-free SplitMix64 generator and compare against
  Rust models. The GC tests cover randomized churn, nested reachable graphs,
  collection invariants and cycle reclamation without exact-count assertions.
- **Benchmark tooling:** `scripts/bench_hyperfine.sh` wraps the existing
  `bench_*.py` harnesses in hyperfine with JSON output validation and clean
  skip/error behavior; `benchmarks/compare_engines.py` compares loop/startup
  under CPython/Hydro/PyPy/Go and reports missing engines as skipped. The
  threshold gate gained an explicit `--check` mode, deterministic
  `scripts/test_check_benchmarks.py` tests, and a non-blocking CI report;
  enforcement stays local/release-only (P5-01c/Q-06).
- **Tag-driven releases:** `.github/workflows/release.yml` tests the tagged
  workspace, builds with `--locked`, packages/checksums/smoke-tests the Linux
  x64 JIT and AOT artifact (binary plus runtime archive) and publishes it
  without repository secrets.
- **Arbitrary-precision integers:** Python `int` now uses a tagged i64 fast
  path with automatic exact promotion to a heap `BigInt` (`num-bigint`).
  New IR/type `Type::BigInt`, `Const::BigInt`, ABI
  `hydro_bigint_from_str`, checked tagged-int fast paths with Heap-tag guards,
  exact mixed arithmetic (`+ - * // % **`, bitwise/shifts, comparisons,
  divmod), `int()` parsing, formatting, JSON and dict/set key equivalence.
  Typed `Int` loops keep the inline fast path; only genuinely big operands
  take the heap path.
- **Cranelift release tuning:** `opt_level=speed` and alias analysis are
  explicit; the IR verifier is debug-only. The JIT keeps
  `is_pic=false`; AOT stays PIC and external libcalls non-colocated for
  correctness.
- **GC address-index optimization:** the local collector's address set now
  uses a deterministic non-cryptographic SplitMix64 hasher instead of SipHash,
  and sweep rebuilds the set in place with reused capacity instead of
  allocating a fresh HashSet and shrinking both registries every collection.
  A 300k-short-lived-list AOT workload drops from 44.95 ms to 40.91 ms median
  (≈9%), with unchanged output and GC semantics; two GC unit tests cover the
  address-set invariant and shrink policy.
- **Full benchmark harness under Hydro:** generator expressions now wrap their
  eager list in an iterator (so `next()`/single-pass use work), `*` call and
  display operands accept arbitrary iterables, and `str`/`bytes.join` accept
  iterators. `bench_all.py` without `--only` completes under Hydro.
- **Runtime dispatch and rich report slice:** bound `Match` methods,
  user `__getitem__` dispatch, native `threading.Thread` subclass bases
  with `start`/`join`/`is_alive`, zero-argument `super().__init__(...)` dispatch to the direct base,
  `dict.setdefault`, `str.format` named fields, JSON dumps keyword
  compatibility (`skipkeys`/`check_circular`/`allow_nan`/`default`/`cls`),
  native `html.escape`/`unescape`, a Unicode `CellTable` placeholder that
  keeps rich's width search valid, `subprocess` path-like `cwd`, and a
  declaration-only nested-module preload/reorder pass. `bench_all.py
  --only hydro` now renders the complete rich report; a native
  `rich.status.Status` shim covers the progress context manager.
- **`for ... else` / `while ... else`:** a loop `else` clause now lowers to
  the new `else_body` on `StmtKind::ForRange`/`StmtKind::ForEach`/
  `StmtKind::While` and runs only when the loop exhausts normally: `break`,
  `return` and exceptions skip it while `continue` does not. The clause is
  emitted outside the loop, so `break`/`continue` inside it bind to an
  enclosing loop (and remain "outside a loop" diagnostics at module level),
  and an exception in the body still runs the enclosing `finally` without
  running `else`. Loops without `else` keep their previous code shape, and
  `while False: ... else: ...` keeps only the else body.
- **Fixed:** an exception raised while an imported module body initializes now
  stops the program with the original traceback instead of letting a later
  module or the entry body overwrite the pending error.

- **`del`, loop `else`, console probes and dataclass fields:** `del` removes
  locals/globals, instance attributes, list/dict items and list slices,
  evaluating targets left to right; local deletion clears the slot to
  `None` (documented limitation). `for`/`while` `else` clauses run only on
  normal exhaustion. `...` lowers to `None`, `input()` reads a line,
  `get_ipython()` reports a non-Jupyter shell, and `NameError`/`SystemExit`
  are catchable tags. `dataclasses.field(default=..., default_factory=...)`
  synthesizes defaults, `replace()` copies instances, and keyword
  construction binds by field name. Native `typing` aliases are empty
  classes and classes are subscriptable, so module-level aliases such as
  `Span = Tuple[int, int]` evaluate. `os.PathLike`/`devnull`/`open`/`dup2`/
  `get_terminal_size`, `sys.stdin`/`stdout`/`stderr` and
  `threading.local` are available; imported-module initializer exceptions
  now stop the program with the original error.

- **Runtime dispatch and rich report rendering:** generated function values
  now carry full parameter metadata (names, kinds, definition-time defaults)
  and a uniform full-argv trampoline; `CallKw`/`hydro_call_kw`,
  `ClassCall`/`hydro_instance_new`, `hydro_bind_class_kwargs`, bound native/user
  methods, classmethod `cls(...)`, imported class constructors and deferred
  imports are dispatched at runtime. User classes gain `__call__`, `__str__`,
  `__repr__`, `__len__`, `__contains__`, `__dict__`, `__new__`, operator
  dunders, keyword-only defaults and 1 MiB generator/coroutine stacks.
  Nested imports of import-free site-packages modules are linked; CPython
  regex bracket/lookaround forms are normalized (lookarounds are dropped),
  callable `re.sub`, `Match` character offsets, `str.rsplit`/`partition`,
  `argparse nargs='+'`, `contextlib.suppress` and io base classes are
  available. Container builders and wrapper argument buffers are rooted
  across GC allocation. With a raised GC threshold the official rich
  console/table/panel/rule report renders; `bench_all.py` remains blocked at
  `rich.status`/`Live`.

- **Package/site-packages loader and official `rich` source:** `check_file`
  now resolves packages (`__init__.py`), dotted names, submodules and
  relative imports, in this order: sibling files, the package-chain root,
  `HYDRO_SITE_PACKAGES`, the `VIRTUAL_ENV`/`CONDA_PREFIX` site-packages trees
  and `PYTHONPATH`. Every unit loads once per canonical path; imported
  module bodies run once as initializers, and nested imports remain deferred
  until the enclosing code runs. A native `rich` implementation is no longer
  shipped: the official pure-Python `rich` distribution resolves through
  this loader. `benchmarks/bench_all.py` still does not run under Hydro; the
  console-compatibility surface (`dataclasses.field`, `os.PathLike`,
  ellipsis, `del`, `get_ipython`, `sys.stdin`/`__stdin__`/`__stdout__`/
  `__stderr__`, `os.get_terminal_size`/`open`/`dup2`, `SystemExit`,
  `input()`, `threading.local`, subscriptable `typing` aliases) is
  implemented, and `rich/console.py` compiles. Native bound-method values
  and dynamic `*args`/`**kwargs` calls are the next observed blockers.
- **List slice assignment:** `xs[start:stop:step] = value` lowers to the new
  `StmtKind::SetSlice` (omitted bounds are `Const::None`) and the
  `hydro_slice_set` ABI. Python's evaluation order is preserved (right-hand
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
  surrogate code point, which Hydro's UTF-8 strings cannot represent). Direct
  calls use the new ABI entries `hydro_ord`/`hydro_chr`, so they allocate nothing.
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
  `0 <= k <= 60`), `hash(value)` (`hydro_hash`; dict-key hash, unhashable
  values raise `TypeError`), unary `~` on `int`/`bool` (`UnOp::Invert`,
  `hydro_unop` code 3), the reserved `NotImplemented` singleton constant,
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
  (new ABI `hydro_tuple_from`) and `hasattr(obj, name)` reports attribute
  existence, swallowing only `AttributeError` (`hydro_hasattr`). `a, *rest, b
  = value` binds a fresh list of the middle elements through
  `hydro_unpack_star`; one star is allowed and nested or duplicated stars are
  compile errors. `raise ReprError("msg")` for a class declared in the unit
  lowers to `RuntimeError("ReprError: msg")` (cause preserved, message
  `str()`-rendered); `except ReprError` now matches that `RuntimeError` tag
  (approximate; see the shared style surface entry). Generator
  functions carry their positional parameter names (`hydro_function_params`),
  so `functools.partial(f, kw=value)` forwards keyword names through the new
  `hydro_module_call_kw` ABI and binds them by parameter name when the partial
  is called; targets without names raise `TypeError` instead of dropping the
  keywords.
- **`repr`, general starred calls and bitwise operators:** `repr(value)`
  returns the runtime's Python representation (`hydro_to_repr`); a general
  call with one starred positional operand (`f(a, *items)`) lowers to
  `IndirectCall { star }` and runs through the new ABI
  `hydro_call_star(fn, argc, argv, star)`, which flattens a list/tuple and
  raises `TypeError` otherwise (method calls keep `hydro_method_star`);
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
  lowering. The hidden ABI `hydro_exception_class(tag)` returns one cached
  native class per tag (thread-local cache cleared by `hydro_runtime_init`).
- **Unpacking targets:** `a, b = ...` accepts any iterable source (including
  `iter(...)`, generators, dicts and ranges) and supports nested tuple/list
  targets as well as attribute and subscript targets such as
  `self.left, items[0] = pair`. Binding stays left to right with Python's
  evaluation order; mismatched lengths keep their `ValueError` messages. The
  new ABI `hydro_unpack_seq` returns the sequence to bind (materialising it only
  for general iterables) so codegen can read the elements safely.
- **Deferred imports:** imports nested in functions, classes or compound
  bodies are recorded but no longer loaded while compiling. Names that can be
  linked to an already-visible dependency bind normally; all other bindings
  lower to `DeferredImport`, which calls the new ABI `hydro_deferred_import` and
  raises `RuntimeError: dynamic import of '<module>.<member>' is not
  supported` only if the enclosing code runs. Top-level imports still load
  and initialize eagerly, and optional dependency graphs (for example
  `rich/_extension.py`) stay out of programs that never call them.
- **Formatting:** f-string format specs (`:.2f`, `:04d`, `:#x`, alignment/
  fill character/sign/grouping) as well as `!s`/`!r`/`!a`; `str.format(...)`;
  printf formatting (`"%s:%d" % (...)`); shared spec implementation and
  ABI `hydro_format_value`.
- **Attribute & method keywords:** `hydro_getattr`/`hydro_setattr` and
  data attributes for `pathlib.Path` (`name`, `stem`, `suffix`, `parent`,
  `path`), files (`name`, `mode`, `closed`, `encoding`), `Thread` and
  `Process`; `hydro_method_kw` for native methods (e.g.
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
  ABI `hydro_closure_new`, captures as leading wrapper parameters.
- **Lazy generators:** `yield value`/bare `yield` run in a stackful
  fiber (`core/fiber.rs`, own 128 KiB stack) that starts only on the first
  `hydro_iter_next`; every generator function creates an independent
  object, `return` ends the iteration, exceptions propagate. On x86_64,
  a `global_asm!` `hydro_context_switch` switches only the callee-saved
  registers (`rsp`, `rbx`, `rbp`, `r12`–`r15`, `ret`), without syscalls;
  `ucontext` remains the fallback for other targets. ABI
  `hydro_generator_new`/`hydro_yield`; new builtins `iter(x)` and
  `next(it[, default])` (`hydro_iter`/`hydro_next`). Generator expressions
  remain eagerly materialized.
- **Fiber performance:** the register-only switch costs two switches per
  `yield` without syscalls; a 100k generator sum runs in release at CPython
  11.0 ms / JIT 7.7 ms / AOT 6.8 ms, `strace -c` counts 94 syscalls in total
  instead of 200,097 (two `rt_sigprocmask` per yield).
- **Coroutines/event loop:** `async def` creates coroutine objects
  (`hydro_coroutine_new`), `await`/`hydro_await` drives them; `asyncio.run` runs
  a cooperative single-thread loop, `asyncio.sleep` suspends only the
  current fiber (blocking outside a loop), `asyncio.gather`
  starts concurrent tasks in input order and propagates the
  first error. No GIL.
- **GC statistics:** `tea gc [--json] <file.py>` (runs like `tea infuse`, then
  collections/live_objects/threshold/pause_total_us/pause_max_us/
  mark_total_us/sweep_total_us/freed_total/stress), public API
  `gc_stats()`/`reset_gc_stats()`/`GcStats`; `HYDRO_GC_STATS=1` remains.
- **Dict hash index:** `DictData` (insertion order + hash index with
  Python equality, first position/last value, numeric key equivalence,
  reindex on `pop`); lookup/insert O(1) on average instead of linear.
- **Int fast paths:** codegen emits checked `+`/`-`/`*` and comparisons for
  statically `Int`-typed operands (including AugAssign and fully
  int-typed `for range` counters); overflow falls back to `hydro_binop`.
- **Frontend optimizations:** constant folding (int/float/str/Binops/
  comparisons/unary, overflow- and division-by-zero-protected) and
  dead-`if`/`while False` elimination.
- **Benchmarks:** `benchmarks/bench_hydro.py` (CPython/JIT/AOT, 11 workloads
  incl. lazy generator sum and async `gather`, output-equality gate),
  registered as `hydro` in `benchmarks/bench_all.py`
  (`uv run benchmarks/bench_all.py --only hydro`).
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
- **CLI/toolchain:** `tea new`, `init`, `test` (plain programs, `--json`),
  `fmt` (`--check`), `lint` (`--json`), `clean` (`--dry-run`); `add`/`remove`/
  `sync`/`lock` as uv passthrough (`HYDRO_UV`).
- **Build cache:** `.hydro/cache` with a deterministic key over source code,
  reachable modules, target, version, and runtime archive; `--no-cache`,
  `tea clean`.
- **Scaling benchmarks:** `pool_serial`, `pool_parallel`, `process_queue` in the
  micro harness and in the baseline/threshold file.
- **Documentation:** `docs/C_API.md` (P4-04 spike incl. environment probe and
  blocker), `docs/LAYOUT.md` (source tree), updated
  `docs/ARCHITECTURE.md`, `README.md`, `AGENTS.md`, `docs/TODO.md`.

### Changed

- **Entry-module global mirroring:** only names read by the entry module's own
  functions are mirrored into the runtime global table; imported modules keep
  full mirroring. This removes a `GlobalSet` per top-level loop iteration
  (300k-iteration top-level while: ~30.5 ms → ~3.1 ms) while preserving
  function access to module globals. AOT cache keys now include the compiler
  executable's size and mtime, so frontend/codegen rebuilds invalidate stale
  binaries instead of silently reusing them in benchmarks.
- **Source restructuring:** `hydro-ir/{model,ir,stdlib}`,
  `hydro-frontend/{lower,sema}`, `hydro-codegen/{backend,lower}`,
  `hydro-runtime/{abi,core,gc,modules,concurrency,support,csv}`,
  `hydro-cli/{cli,cli/commands}`; no behavior change, contracts unchanged.
- Test count increased from 545 to 721 tests (incl. formatting, attributes,
  classes, closures, stdlib modules, socket, user modules, CLI/build cache);
  with this slice to **905** tests (incl. `crates/hydro-cli/tests/fibers.rs`,
  dict hash index, GC statistics, generator iteration).
- AOT builds embed user modules; the cache key takes the
  entire reachable module tree structure into account.
- **Fiber GC roots:** `ctx`/`caller_ctx` (callee-saved register frames) and
  the fiber stack are lifetime-registered conservative roots;
  `push_stack_bounds` aligns the stack scan to a running fiber. The
  outermost resume publishes two aux ranges (native driver stack and
  `caller_ctx`), shared mode mirrors them, local mode gets by without atomics per
  switch. `HYDRO_GC_STRESS=1` covers interleaved generators.
- `bench_all.py` resolves its output directory absolutely so that child scripts
  write into the same directory.
- **Native module dispatch cache:** `hydro_module_call`/`call_native` borrow
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
- **CLI polish (P6-07):** `tea completions <bash|zsh|fish>`, roff manpage
  `docs/tea.1`, and `tea --version --verbose` (target, backend, runtime).
- **C-API containers:** `PyUnicode_*`, `PyFloat_*`, `PyBytes_*`, `PyList_*`,
  `PyTuple_*`, `PyDict_*` plus bounded recursive list/tuple/dict conversion
  in both directions (copies, depth 64, cycle rejection).
- **C-API capsules & buffers:** `PyCapsule_New/GetPointer/GetName/IsValid/
  SetPointer` with a Hydro `PyCapsule` kind, read-only contiguous bytes
  `PyObject_CheckBuffer/GetBuffer/ReleaseBuffer` (verified `Py_buffer`
  layout), and `PyExc_*` singletons with typed `PyErr_SetString` mapping.
- **Installed extension discovery:** extension lookup also searches
  `PYTHONPATH`, `HYDRO_SITE_PACKAGES`, `VIRTUAL_ENV`/`CONDA_PREFIX`
  site-packages, and wheel-style `<name>.cpython-314-*.so` filenames.
- **Dotted extension submodules & ABI policy:** `pkg.sub_ext` resolves to
  `<dir>/pkg/sub_ext*.so` without package Python source (aliased and
  from-import forms); incompatible ABI tags (`cpython-312`, `pp310`, ...) are
  compile-time diagnostics instead of dlopen attempts.
- **Multi-phase init:** `PyModuleDef_Init` + bounded `Py_mod_exec` slot
  execution with policy-slot validation, alongside single-phase
  `PyModule_Create2` modules; JIT+AOT fixture.
- **CPython 3.14.9 target:** single source of truth in `hydro_ir::compat`;
  `sys.version`/`version_info` report 3.14.9 (Hydro identity via
  `sys.hydro_version`), `platform.python_version()` and the CLI verbose output
  follow; the wheel-tag policy accepts `cpython-314-*`.
- **Official CPython test gate:** `scripts/cpython_compat.py` runs the 3.14.9
  `Lib/test` suite differentially, writes JSON/Markdown, and supports
  `--gate`, `--update-manifest` and offline `--check-manifest`; the manifest
  starts empty and grows only from real passes. Baseline (2026-10-25, 398
  modules): **1 passing** (`test_errno.py`), 334 `hydro_fail`, 63
  `expected_fail`, 0 timeouts. `scripts/verify.sh`/CI check the manifest
  offline.
- **First official Python test:** unmodified `Lib/test/test_errno.py` passes
  under JIT and AOT. Adds first-class standard-library module values
  (`hydro_module_value`, module-aware `getattr`), module-level globals
  (`hydro_global_get`/`hydro_global_set`), `frozenset`, `str.isupper`/`islower`,
  `AssertionError` (tag 14), a native `errno` module (Linux table,
  `errorcode`, `__dict__`) and a minimal native `unittest`
  (`TestCase`/runner/native assertions).
- **Benchmark-critical stdlib batch:** `subprocess`, `tempfile`, `shutil`,
  `textwrap` and `timeit` are native and API-tested; `tea infuse` now forwards
  program arguments into `sys.argv`, with `sys.executable`, `sys.exit`,
  `os.environ`/`os.access` and argparse default-argv/help support.
  `bench_all.py --only hydro` completes under CPython; the import baseline at
  that point was 47/297.
- **Batch B native codec/text core:** `binascii`, `base64`, `quopri`,
  `fnmatch`, `getopt` and `mimetypes` are native and API-tested, bringing the
  import baseline to 42/297 at that point. The remaining Batch B modules still
  need the pure-Python pipeline.
- **Standard-library program (P7) and Batch A:** added a complete
  module-by-module plan for all 272 missing stdlib modules with dependency
  batches and a per-layer ledger. First slice implements native
  `bisect`/`_bisect`, `heapq`/`_heapq`, `colorsys`, `keyword`,
  `stat`/`_stat`, plus `_operator`, `_functools` and `_io` aliases; import
  baseline rises to 36/297. CPython per-module test subsets remain P7-05.
- **LTS policy:** future Hydro LTS lines are keyed to CPython minor versions
  (3.13/3.14/3.15) with frozen patch baseline, ABI/wheel tags, stdlib surface
  and support window (P4-12; no LTS before production).
- **Clippy gate:** `scripts/verify.sh`/CI enforce `-D warnings`; the tree is
  warning-free.
- **Stdlib compatibility baseline:** `scripts/stdlib_compat.py` +
  `docs/COMPATIBILITY.md` + `benchmarks/compat/latest.json`; 23 of 297
  CPython 3.14 modules importable on 2026-10-24 (import success only, not
  test-suite success).
- **First abi3 extension slice:** `hydro-runtime/src/capi/` implements a
  limited-API host surface (`PyModule_Create2`, `PyModule_AddStringConstant`/
  `AddObject`, `PyLong_From/AsLong`, variadic `PyArg_ParseTuple` via a small C
  file, refcount entry points, `PyErr_*` basics, symbol anchor). The frontend
  discovers sibling `.so`/`.abi3.so` files, lowers extension calls/attributes,
  and the loader (`modules/extension.rs`) caches modules by path. `tea` and AOT
  binaries are linked with `--export-dynamic`; the fixture imports and calls
  under JIT and AOT with no `libpython` (`crates/hydro-cli/tests/extensions.rs`).
  Open: package submodules, multi-phase init, wheel tag policy, full ABI.
- **P4-04 decision (2026-10-24): option B** — a Hydro-native CPython C-API/ABI
  layer without embedded `libpython`; CPython 3.14 headers/toolchain are used
  at build time and the interpreter as a differential test oracle. Probes:
  embedded CPython init 6.67 ms + finalize 7.46 ms, `import numpy` 41.64 ms;
  a stable-ABI fixture imports with four undefined CPython symbols
  (`spikes/p4_04/`).
- **Full-compatibility program:** P4-09 (297 stdlib modules: 174 pure Python,
  63 extension, 52 builtin/frozen, 8 platform-specific), P4-10 (ecosystem suites: NumPy, pandas,
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
- C API/C extensions: decision B (Hydro-native ABI layer, no `libpython`).
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
- **Codegen:** Cranelift-based JIT path (`tea infuse`) and AOT path
  (`tea build`, object file + link against `libhydro_runtime.a`).
- **Runtime:** tagged i64 values, strings/floats/exceptions, dynamic
  operators, truthiness, `str()`/repr, tracebacks with frame/line references.
- **CLI:** `tea check`, `tea infuse`, `tea build`, `tea doctor`, `--json`,
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
  `HYDRO_GC_THRESHOLD`, `HYDRO_GC_STRESS`, `HYDRO_GC_STATS`.
- **Concurrency (no GIL):** `_thread`, `threading` (`Thread`, `Lock`,
  `RLock`, `Event`), `queue.Queue`, `multiprocessing` (`Process`, `Queue`,
  `Pool`, fork and spawn), `pickle` with its own HYD1 format.
- **Diagnostics (P6):** stable error codes, spans, source snippets, help/
  hints, `--json` schema, `tea doctor`, ICE handling with exit code 2.
- **Verification:** `scripts/verify.sh` (fmt, Clippy, build, tests, E2E for
  JIT and AOT, negative tests, GC stress), benchmark harness
  `benchmarks/tea/run_micro.py` with CPython/JIT/AOT baseline.
- **Documentation:** `docs/ARCHITECTURE.md`, `docs/TODO.md`, `AGENTS.md`,
  `benchmarks/HYD.md`, `PLAN.md`.
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
