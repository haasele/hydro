# Hydro — Native Bindings and Compatibility Tiers

Status: **design + roadmap, 2026-10-29.** This document is normative for
*new* native interfaces and descriptive for existing ones. It does not
implement anything and claims no tier that has no acceptance evidence.
Contract changes to the runtime ABI still require [`ARCHITECTURE.md`](ARCHITECTURE.md) §3.

Scope: how Python code, Rust-native modules, newly compiled libraries,
unmodified CPython C-extension wheels and foreign-language runtimes reach the
Hydro runtime. One process, one tagged-value model, one GC; no `libpython`
(option B, [`C_API.md`](C_API.md)).

Compatibility target: **CPython 3.14.9**, defined once in
`hydro_ir::compat` (`PYTHON_VERSION`, `PYTHON_ABI_TAG = "cpython-314"`).

## 1. Principles

1. **Hydro code stays GIL-free.** No CPython runtime, no GIL and no
   `PyObject` layout in compiled Hydro code. Foreign compatibility is a
   boundary, never the execution model.
2. **Source first.** Python that can run as source is never rewritten into
   another language. Native compilation exists for code that is *not* Python
   (C libraries, the system ABI), not as a substitute for executing Python.
3. **One dispatch table.** A module is described once
   (`ModuleSpec` in `hydro-ir/src/stdlib/specs.rs`) and that description is
   used by the frontend for type checking and by the runtime for arity and
   value validation. No second registration path, no drift.
4. **No silent fallbacks.** Unsupported types, ABI mismatches, ambiguous
   artifacts and missing symbols are compile-time or load-time errors with a
   stable diagnostic. Nothing degrades to `void*`, "best effort" or an empty
   stub.
5. **Boundary cost is explicit.** Conversions must be bounded and measured
   per binding; there is no claim that extension-heavy code reaches native
   Hydro speed.
6. **Everything is differential.** A binding is accepted only when the same
   deterministic program agrees under the CPython 3.14.9 oracle, Hydro JIT
   and Hydro AOT, or when the expected difference is documented and tested.

## 2. Tier model

| Tier | Input | Mechanism | Status (2026-10-29) |
|---|---|---|---|
| **0** | Pure-Python source (CPython `Lib`, site-packages, project `.py`) | Frontend importer compiles the source through the normal pipeline; search path includes `HYDRO_SITE_PACKAGES`, venv/conda site-packages, `PYTHONPATH`, `HYDRO_STDLIB` and the vendored `stdlib/` tree | **Partial.** Importer, packages and site-packages work; 130/297 stdlib modules import (88 pure, 30 builtin/frozen, 12 extension), 167 fail. See [`COMPATIBILITY.md`](COMPATIBILITY.md) |
| **1** | Hydro's own stdlib/native modules | Rust inside `hydro-runtime`, described by `ModuleSpec`, dispatched by `hydro_module_call`/`hydro_module_get`; **no C ABI, no `dlopen`** | **Implemented for the modules listed in `MODULES`** (Batch A plus selected later modules, e.g. `math`, `json`, `csv`, `re`, `socket`, `zlib`, `datetime`); per-module surface coverage and tests remain P7 work |
| **2** | New compiled bindings (Rust-first; C/C++/Zig/Go via C ABI) | **HNA** — Hydro Native ABI: C symbol table, tagged `i64` values, explicit root/error/unwind contract, versioned manifest | **Planned.** No loader, no symbols, no fixtures exist |
| **3** | Unmodified CPython C-extension wheels | Option-B shim in `hydro-runtime/src/capi/`; extensions are ordinary `.so` files whose undefined CPython symbols resolve from the Hydro process | **Partial.** abi3 slice, extension loader, full-ABI symbol surface (link-time complete for the pinned wheels); pydantic imports, NumPy/pandas semantic gates open; 12 extension modules import |
| **4** | JVM/.NET/Node and other foreign runtimes | Tier-2 adapter over the runtime's C ABI, or an explicit sidecar protocol; no automatic cross-language source translation | **None.** Design only |

The tiers are alternatives for *different inputs*, not a migration ladder. A
stdlib module MUST NOT be implemented through Tier 3 when Tier 1 can express
it: the CPython shim exists for third-party wheels, not as Hydro's stdlib
strategy.

### 2.1 Tier 0 — pure Python source

- Sources are executed, never translated. Third-party pure-Python packages
  install normally into site-packages (Tea/`uv` own installation) and import
  through the existing loader.
- `HYDRO_STDLIB` names an unmodified CPython `Lib` tree; the vendored
  `stdlib/` tree holds Hydro's patched compatibility modules. Both are
  standard-library roots under the search path above. Vendored files carry a
  provenance header and are adaptations, not forks of arbitrary modules
  (P7-02 drift check).
- Import search order is defined in `hydro-frontend/src/lib.rs`
  (`module_search_roots`): importing directory, package-chain root,
  `HYDRO_SITE_PACKAGES`, venv/conda site-packages, `PYTHONPATH`, then the
  standard-library roots (the vendored `stdlib/` tree and `HYDRO_STDLIB`).
  The function is the source of truth for the exact relative order.
- Status limit: import success is not parity. Per-module `Lib/test` subsets
  (P7-05) and representative APIs decide.

### 2.2 Tier 1 — Rust-native modules

- A module is added by one `ModuleSpec` entry: module name, functions
  (`name`, `min_args`, `max_args`, return `Type`, keyword `params`,
  `dynamic_keywords`) and constants, plus an implementation file under
  `crates/hydro-runtime/src/modules/`.
- The runtime dispatches through `hydro_module_call` (direct
  `module.function(...)`), `hydro_module_get` (constants and function
  values) and `call_native` (native function values used as callbacks).
- Rules: implemented in Rust in the runtime crate; no C shim; no dynamic
  loading; fallible paths set a pending exception and return the `None`
  dummy; no panics (see `ARCHITECTURE.md` §3).
- Acceptance: spec entry + implementation + a CPython differential test for
  the documented surface (per module or per batch), plus import-matrix and
  `Lib/test` status. Unsupported surface is a diagnostic, not a partial
  function.

### 2.3 Tier 2 — HNA (planned)

See §3. This is the only supported way for a **new** compiled library to
enter Hydro. A library that needs CPython objects, `PyObject*` layouts or
`PyTypeObject` slots is Tier 3 by definition, not Tier 2.

### 2.4 Tier 3 — CPython C-extension compatibility

- Mechanism (implemented slice): `crates/hydro-runtime/src/capi/` exports the
  limited-API module/scalar/container/error/capsule/buffer surface and the
  larger 3.14 surface (`PyObject_HEAD`, `PyTypeObject`, `PyType_FromSpec`,
  vectorcall, `PyArg_ParseTupleAndKeywords`, buffers, `PyExc_*` data
  symbols); `crates/hydro-runtime/src/modules/extension.rs` loads siblings
  with `dlopen(RTLD_NOW)` + `PyInit_<name>`, supports single- and multi-phase
  init and caches by path. `tea`/AOT binaries are linked with
  `--export-dynamic`; `ldd` must not contain `libpython`.
- Discovery: sibling of the importing source, `HYDRO_EXTENSION_PATH`,
  `PYTHONPATH`, `HYDRO_SITE_PACKAGES`, venv/conda site-packages. Wheel tags
  `.so`, `.abi3.so`, `.cpython-314-<platform>.so` are accepted; other tags
  are compile-time diagnostics.
- Overhead is confined to the boundary: handle creation/conversion,
  refcount bookkeeping and the emulated GIL while foreign code runs. Hydro
  code is not refcounted, does not take the GIL and never pays CPython
  startup or interpreter cost; no CPython runtime is initialized.
- Current evidence: the ABI inventory reports 0 missing required CPython
  symbols across the 68 pinned extension modules (479 referenced, 593
  exported); pydantic 2.14.1 imports through the real `_pydantic_core`;
  NumPy/pandas stop on semantic/dynamic gaps (`numpy.exceptions`,
  `dataclasses.is_dataclass` in the 2026-10-29 ecosystem report), not on link
  errors. Full `PyTypeObject` behavior, `tp_traverse`/`tp_clear`, GIL/thread
  integration and wheel installation remain open (P4-04c/d, P4-05).
- Policy: Tier 3 is never used for Hydro-native stdlib modules, never for
  new Hydro bindings and never as a way to import `libpython`.

### 2.5 Tier 4 — foreign runtimes (none implemented)

Two supported shapes only:

1. **C-ABI adapter:** the foreign runtime is initialized in-process through
   its embedding API (JNI Invocation API, .NET `hostfxr`, Node `node_api`)
   and wrapped in an HNA module. The adapter owns the foreign runtime's
   lifetime, initialization failures are Hydro exceptions, and every call
   crosses the same value/error/root contract as Tier 2.
2. **Sidecar protocol:** the runtime lives in a separate process and speaks a
   versioned, length-prefixed message protocol (for example JSON Lines over
   stdin/stdout or a Unix socket). No shared memory and no shared object
   graph. Preferred default because a crash is isolated and no foreign VM is
   embedded.

Neither shape translates source between languages. A Python module is not
rewritten in Java, a JVM class is not rewritten in Python; the boundary is
explicit messages or C calls.

## 3. HNA v1 — Hydro Native ABI (planned, not implemented)

HNA is the stable contract for compiled modules written for Hydro. It reuses
the tagged-value model of `ARCHITECTURE.md` §3 but adds a module descriptor,
an explicit root discipline and version negotiation that a raw `dlopen`
extension does not have.

### 3.1 Artifact and naming

- A binding is a shared library exporting the HNA entry points; Rust builds
  `crate-type = ["cdylib"]`.
- File name: `<last_component>.hna.so` in the directory that owns the import
  name. Module `pkg._fast` is `pkg/_fast.hna.so` (mirroring existing dotted
  extension discovery). The file stem MUST be a single component; dots stay
  in the import name, not the file name.
- Target: Linux x86_64 is the verified platform. Other platforms are not
  claimed until a binding runs there.
- A binding MUST NOT link `libpython`, include CPython headers, or depend on
  any CPython symbol. If it does, it is a Tier 3 artifact and is loaded
  through the extension path instead.

### 3.2 Entry points and descriptor

Required exports (symbols are fixed and versioned; `_v1` is the major):

```c
uint32_t hydro_native_abi(void);                  /* must return 1 */
uint32_t hydro_native_abi_build(void);            /* build counter, for reports */
const HydroNativeModule* hydro_native_module_v1(void);
```

The descriptor is a read-only structure with a leading `struct_size` so new
fields can be appended without breaking old loaders:

```c
typedef struct HydroNativeModule {
    uint32_t struct_size;        /* size of this struct as built */
    uint32_t hna_version;        /* 1 */
    uint64_t runtime_abi_min;    /* minimum hydro_runtime ABI revision */
    const char* module_name;     /* exact dotted import name, UTF-8, NUL-terminated */
    const char* provider;        /* "distribution version", for diagnostics */
    const char* platform;        /* "linux-x86_64", checked by the loader */
    uint32_t flags;              /* bit 0 thread_safe; other bits reserved, must be 0 */
    int32_t (*init)(void);       /* optional; 0 = ok, sets a pending exception on failure */
    uint32_t function_count;
    const HydroFunction* functions;
    uint32_t constant_count;
    const HydroConstant* constants;
} HydroNativeModule;

typedef struct HydroFunction {
    const char* name;
    uint32_t min_args;
    uint32_t max_args;
    uint32_t flags;              /* reserved, must be 0 */
    const char* params;          /* comma-separated keyword names, or NULL */
    HydroValue (*call)(HydroValue self, uint64_t argc, const HydroValue* argv);
} HydroFunction;

typedef struct HydroConstant {
    const char* name;
    HydroValue (*value)(void);   /* evaluated once by the runtime, then cached */
} HydroConstant;
```

`HydroValue` is `int64_t` with the encoding of `ARCHITECTURE.md` §3.1. All
descriptor structures are `#[repr(C)]`, static, read-only and valid for the
process lifetime. The `self` argument is the module object's tagged value and
can be ignored for plain functions. Keyword calls are resolved by the runtime
against `params`; the call pointer always receives positional arguments in
Python order.

The runtime caches each module object and its constants, roots them
persistently and never calls `init` twice. Descriptor strings are static
storage owned by the module and valid for the process lifetime.

### 3.3 Value, allocation and rooting contract

- Heap layout is private. A binding MUST NOT dereference a heap pointer; it
  uses runtime services (`hydro_str_new`, `hydro_list_new`, `hydro_dict_new`,
  arithmetic/compare/call/attribute functions) to create or inspect values.
- Values passed as arguments are rooted by the runtime for the duration of
  the call. A binding that stores a value (module state, cached callback,
  container element it created) MUST root it with the process-lifetime root
  API. HNA v1 will export `hydro_root_hold(HydroValue) -> uint64_t` and
  `hydro_root_release(uint64_t)`; these do not exist yet and are part of the
  HNA implementation, not a current claim.
- A binding that holds a derived value across a call which may allocate,
  block or hit a safepoint MUST register a precise scope
  (`hydro_gc_scope_push`/`hydro_gc_scope_pop`, already exported) or use a
  hold token.
- No interior pointers, no borrowed slices over heap objects, no pointers
  valid across a boundary call. Byte data is copied into a runtime-owned
  buffer.
- A binding MUST NOT free anything it did not allocate. Buffers it allocates
  for output are released by the binding after the runtime copies them.

### 3.4 Errors, panics and unwinding

- No unwinding across HNA: no C++ exceptions, no `longjmp`, no Rust panic.
  An exported function that can fail calls a `hydro_raise*` function and
  returns the `None` immediate; the caller observes `hydro_exc_pending()`.
- Generated Rust wrappers catch panics with `catch_unwind`; a caught panic is
  an internal error: the message goes to stderr and the process aborts, as
  `ARCHITECTURE.md` §3 requires for runtime bugs. A panic is never converted
  into a Python-level exception that user code can swallow.
- Projections from ABI types to Python types fail with `TypeError` (wrong
  argument) or `OverflowError` (out-of-range integer), matching CPython
  behavior; the binding decides per parameter, and the generated spec records
  which checks were generated.

### 3.5 Threading and GIL

- HNA has no GIL. Calls run on the calling Hydro thread; the runtime
  registers that thread with the GC before entering the module.
- `thread_safe = true` means reentrant/concurrent calls are allowed. The
  runtime then invokes the module directly. `thread_safe = false` (the safe
  default) makes the runtime serialize calls per module with a mutex; the
  binding must not block while holding runtime state it does not own.
- No Hydro code ever runs while a foreign GIL is held. The only GIL in the
  process is the Tier 3 emulation lock, held solely while a CPython extension
  executes, including extension callbacks.

### 3.6 Versioning

- HNA has one integer major (`1`). Within a major only additive changes are
  allowed: new descriptor tail fields guarded by `struct_size`, new optional
  symbols with a new suffix, new reserved flag bits only after being defined.
  A semantic or layout change requires HNA 2 (`hydro_native_module_v2`) and a
  migration window.
- The runtime exposes its supported range; the loader refuses a module whose
  `hna_version` or `runtime_abi_min` is outside it, and names both sides in
  the diagnostic. The numeric runtime ABI revision is introduced with HNA
  and does not exist today (the current `hydro_runtime` library is linked,
  not version-negotiated).
- The module name in the descriptor MUST equal the dotted name being
  imported; a mismatch is a load error. This prevents a package from
  accidentally satisfying a different import.
- `platform` MUST equal the host target as reported by the runtime; no
  "compatible enough" guesses.

## 4. Loader, discovery and manifests

### 4.1 Discovery

Native artifacts are discovered next to the module that imports them, using
the same roots as the source importer (importing directory, package-chain
root, `HYDRO_SITE_PACKAGES`, venv/conda site-packages, `PYTHONPATH`). An HNA
artifact is looked up as `<name>.hna.so`; CPython extensions continue to use
the existing `.so` / `.abi3.so` / `.cpython-314-*.so` lookup. A new
`HYDRO_NATIVE_PATH` is not required in v1; if added later it follows the same
`split_paths` semantics as `HYDRO_EXTENSION_PATH`.

Rules:

- Compiled artifacts are leaf modules only in v1. A package's `__init__`
  stays Python source (Tier 0) or a Tier 1 builtin; HNA does not provide
  package `__init__` semantics, `__path__` or namespace-package behavior.
- If two compiled artifacts of different kinds match the same import name in
  the same directory (`.hna.so` and a CPython `.so`, or two CPython tags), the
  import is a compile-time error. A distribution ships exactly one artifact
  per platform; there is no precedence guess.
- If a `.py` module and an HNA artifact match the same name, the precedence
  MUST be defined and diagnosed, never silent. The initial rule is the
  existing extension lookup order (a compiled sibling is found before source
  when the frontend performs extension resolution); any future override is an
  explicit, documented setting.
- Project, site-packages and `PYTHONPATH` roots precede the standard-library
  roots exactly as today. The Tier 1 spec table owns the module names it
  declares; a filesystem artifact with the same top-level name must not
  silently replace a Tier 1 module.

### 4.2 Package manifest

A Hydro-native distribution ships `hydro-manifest.toml` next to its modules.
The embedded descriptor is authoritative at load time; the manifest exists so
installers and `hydro-bindgen --check` can inspect a distribution without
`dlopen` and can select the right platform artifact.

Required keys (unknown keys are rejected, missing keys are an error):

```toml
manifest_version = 1
hna_version = 1
hydro_abi_min = 1
platform = "linux-x86_64"
provider = "example-native 0.4.0"

[[modules]]
name = "example._fast"
artifact = "example/_fast.hna.so"
thread_safe = false
```

- `artifact` is relative to the distribution root, contains no `..`, and
  must exist.
- At load time, if a manifest is found for the artifact's distribution, the
  loader verifies `module_name`, `hna_version`, `platform`, `provider` and
  `thread_safe` against the embedded descriptor. Any mismatch is a load
  error, never a warning.
- `hydro-bindgen --check` regenerates and compares both the descriptor and
  the manifest in CI, so a rebuilt binary cannot silently disagree with a
  checked-in manifest.

### 4.3 Wheels, extensions and coexistence

- Unmodified CPython wheels are installed as-is; Hydro never rewrites
  `RECORD`, `.dist-info` or the extension file. Extension loading reads the
  installed tree; installation belongs to Tea/`uv` (P4-05).
- A single distribution MAY contain Python source, HNA artifacts and CPython
  extensions, but not for the same import name (see §4.1).
- Hydro-native distributions use a Hydro ABI tag in the wheel name (proposal:
  `py3-none-hydro1_0_<platform>`). For HNA artifacts the loader does not read
  wheel tags; it keys off the `.hna.so` file name and the descriptor. The
  Tier 3 extension loader keeps its existing `.cpython-314-*` tag validation.
  Packaging and tag emission are Tea work and are not claimed here.
- CPython extension discovery and HNA discovery must not change each other's
  caching: each caches by canonical artifact path, and a failed HNA load
  never silently falls back to a CPython artifact or vice versa.

### 4.4 What "no CPython GIL in Hydro code" means operationally

- Hydro compiled code, the interpreter and Tier 1 modules never acquire a
  GIL.
- Tier 3 acquires the emulation lock strictly between entering and leaving an
  extension entry point (including callbacks from that extension). It is not
  a process-wide Python runtime lock and it never wraps Hydro execution.
- Tier 2 and Tier 4 in-process adapters acquire only the foreign runtime's
  own lock, inside the adapter, for the duration of the foreign call.
- A binding that needs to call back into Hydro while holding a foreign lock
  must document it and pass the threaded-callback gate; deadlock is a defect,
  not a supported mode.

## 5. Binding-generation workflow (planned)

No `hydro-bindgen` crate exists in the workspace today. The workflow below is
the accepted target; all steps are roadmap items.

### 5.1 Inputs

- C headers for a C library (the primary case).
- A Rust crate for a Rust library (no headers; a handwritten HNA wrapper).
- C++ libraries through a C wrapper header; C++ templates, exceptions and
  overloaded operators are not part of the input language.
- Go libraries through `-buildmode=c-shared` and the generated C header.
- Zig libraries through `export fn` and a C header.

### 5.2 Pipeline

1. **Header ingestion.** Parse the declared C subset at build time. A Rust
   parser is preferred; `bindgen`/libclang may be used as a development tool.
   The header set, include paths and defines are recorded in the binding's
   configuration so regeneration is reproducible.
2. **Type mapping.** A fixed table, not heuristics:

   | C type | HNA projection | Rule |
   |---|---|---|
   | `int8..int64`, `uint8..uint32`, `bool` | `int` / `bool` | range check at the boundary |
   | `uint64_t`, `size_t` | `int` | `OverflowError` above `i64::MAX` |
   | `float`, `double` | `float` | `long double` rejected |
   | `char*` + length pair, `uint8_t*` + length | `bytes`/`str` | copy in/out; no NUL sniffing |
   | NUL-terminated `const char*` | `str` | UTF-8; invalid input → `UnicodeDecodeError` |
   | opaque handle (`typedef struct X*`) | `int`/capsule | lifetime owned by the wrapper, finalized explicitly |
   | fixed POD struct by value | tuple/dict | size/alignment recorded; bitfields rejected |
   | function pointer + context | callable | context pointer mandatory; no global callback state |
   | enum | `int` | values exported as constants |
   | union | rejected | unless the exact tag+member layout is whitelisted |
   | variadic function | rejected | wrap a fixed-arity subset explicitly |
   | `jmp_buf`, `va_list`, thread-local ABI, VLAs | rejected | no projection |

3. **Code generation.** Emit:
   - `bindings.rs`: `extern "C"` declarations, a safe Rust wrapper per
     exported function, and one `HydroValue` entry wrapper that converts
     arguments, handles null/range failures and calls `hydro_raise*` on
     error;
   - `module.spec`: the `ModuleSpec` fragment (functions, arity, keyword
     names, constants) for `hydro-ir`; generated, checked in, and used by the
     frontend without a second declaration;
   - `hydro-manifest.toml` and the descriptor table;
   - a type-mapping report naming every projected and every rejected type.
4. **Rust cdylib template.** `crate-type = ["cdylib"]`, edition 2021,
   `panic = "unwind"` (the generated entry points catch panics), no
   `libpython` linkage, no exported `#[no_mangle]` symbols beyond the three
   HNA entry points. Per-function wrappers are reached through the descriptor
   table, not by exported name.
5. **Spec integration.** The generated spec is merged into the module table
   by hand or a checked-in script; generation does not run during a normal
   build, so compiler input stays deterministic and reviewable.
6. **Differential tests.** Every binding ships:
   - deterministic fixtures run under CPython (via `ctypes`/`cffi` or the
     upstream Python package) and under Hydro JIT and AOT;
   - arity, keyword, wrong-type and overflow cases asserting the same
     exception classes;
   - a strict-rooting test (`HYDRO_GC_PRECISE=1`) that calls the binding in a
     loop with collections forced;
   - a panic/abort test with a deliberately broken fixture, asserting the
     process aborts without unwinding into Hydro.

### 5.3 Failure policy

Codegen fails loudly. A header containing an unsupported construct fails with
the symbol, the source location and the reason; generation stops. There is no
`--force`, no automatic `void*` escape hatch and no generated function that
silently drops a parameter. A deliberate opt-in for a type is a reviewed,
checked-in entry in the binding configuration, and the generated report marks
it as hand-audited.

### 5.4 First targets

1. `libm`-style scalar functions as the end-to-end smoke (arity, floats,
   errors, AOT).
2. `libsqlite3` as the real pilot: opaque handles, callbacks with context,
   string/bytes lifetimes, error codes. It matches the `_sqlite3` work in
   P7-04 and forces the root/lifetime rules to be real.
   Acceptance compares outputs against CPython's `sqlite3` module on the same
   database operations.

## 6. Pure-Python package strategy

The requirement is that third-party **pure-Python** packages run through the
normal importer with no per-package translation and no Hydro-specific fork.
Today that is the implemented path: site-packages wheels resolve through the
frontend loader, and packages with `__init__.py`, dotted/relative imports and
package submodules run as source.

When a pure package fails, the cause is a missing primitive, not its Python
source. The response is:

1. **Classify** the failure with the import matrix and the real traceback:
   missing builtin, missing extension module, missing language feature,
   missing dynamic-import behavior.
2. **Implement the primitive in dependency order** (P7 batches): builtin/
   frozen shims first (P7-03), then native extension modules (P7-04), then
   the pure modules that depend on them. Never patch the third-party source.
3. **Use the language surface, not workarounds.** Missing descriptor,
   metaclass or import semantics are fixed in the frontend/runtime (P4-02/
   P4-03); a downstream package is not special-cased.
4. **Gate every step** with the import matrix (`scripts/stdlib_compat.py`),
   per-module `Lib/test` subsets (P7-05) and the ecosystem differential
   (`scripts/ecosystem_compat.py`). An import alone is never reported as
   compatibility.
5. **Only document adaptations in the vendored `stdlib/` tree**, with a
   provenance header and a drift check against the pinned 3.14.9 tree
   (P7-02). `stdlib/enum.py` is the only vendored adaptation today; the tree
   is not a place to fork packages.

Current state: 130/297 stdlib modules import; pydantic/pydantic-core import
through the Tier 3 extension; NumPy and pandas do not import yet; the pinned
ecosystem tree is installed and reported by [`ECOSYSTEM.md`](ECOSYSTEM.md).

## 7. Automatic Python→Rust conversion

**Not a general strategy, and not in the roadmap.** Hydro executes Python
source and compiles the *same semantics* through its own IR; it does not
translate Python into Rust. A transpiler is not a faster route to the goals
because the hard part is not code generation, it is preserving Python's
dynamic semantics.

Risks, in order of severity:

| Risk | Why translation is unsound | Consequence |
|---|---|---|
| Dynamic name resolution | `getattr`/`setattr`, `globals()`, `eval`/`exec`, import hooks and monkeypatching resolve names at runtime; static Rust cannot resolve them | either reject valid programs or emit a runtime name-resolver, i.e. an interpreter |
| Descriptors and object model | `__getattribute__`, `__getattr__`, `__set__`, `__set_name__`, properties, metaclasses, MRO, zero-argument `super()`, `__class__` assignment and `__slots__` are lookup protocols, not field accesses | must reimplement CPython's lookup machinery in generated code |
| Dynamic typing and operators | A variable's type changes at runtime; `+` dispatches `__add__`/`__radd__`; truthiness and comparison are user-overridable | static inference cannot prove types; the faithful lowering is boxed `Any` dispatch, slower than the existing JIT |
| GC and finalization | `id`, `weakref`, `__del__` timing, cycles, generator `close()`/`finally`, exceptions held by tracebacks | translated code changes reachability and finalizer timing; programs observe the difference |
| C-API coupling | Extensions receive real `PyObject*`, borrowed references and type-slot pointers; callbacks expect a CPython frame | transpiled Rust cannot stand in for a C extension; Tier 3 is still required |
| Generators/async | suspension, `send`/`throw`, `yield from`, cancellation and `finally` interact with stack inspection | hand-built state machines diverge at exactly the edge cases users rely on |
| Introspection and errors | tracebacks, `sys._getframe`, `locals()`, `inspect`, decorator metadata and line numbers | diagnostic and framework compatibility breaks silently |
| Build economics | whole-program inference and codegen for dynamic code, per-module recompilation and cache invalidation | compile time and binary size without a correctness proof; the JIT already reaches native speed on the measured suite |

Equivalence is also untestable in general: the oracle would have to cover
every dynamic input. A silently mistranslated program is worse than a slower
correct one, and Hydro's measured JIT/AOT results already come from executing
Python semantics: the pinned `bench_hydro.py --repeat 9` medians are 3.51×
(JIT) and 5.45× (AOT) versus CPython, faster on 11/11 workloads (see
`benchmarks/hydro/results/latest.md`).

**Where a narrow port is allowed:** after a profile shows a hotspot, a
function may be ported to Rust by hand (or with assistance) when all of the
following hold:

- pure: no I/O, no imports, no `globals()`, no `eval`/`exec`, no reflection;
- closed over stable argument types, no monkeypatching, deterministic;
- the Python source stays the reference implementation in the repository;
- a differential test covers the normal corpus, boundary values, error paths
  and fuzzed inputs, and runs in CI;
- the port is per function, never per module, and is justified by a measured
  threshold, not by aesthetics.

This is exactly how `re`, `json`, `csv` and the Batch A modules are native
today: targeted Rust implementations with CPython-shaped tests, not
transpiled Python.

## 8. Roadmap and acceptance gates

Phases are ordered by dependency. Tier 1/P7 work continues in parallel with
NB-2..NB-4. No phase is marked done without the listed evidence; the status
column is the authoritative claim.

| Phase | Scope | Status | Acceptance gate |
|---|---|---|---|
| **NB-0** Baseline | Tier 0 importer, Tier 1 module table, Tier 3 abi3 slice + loader, no `libpython` | **done (baseline slice)**; Tier 3 remains partial | `scripts/verify.sh` green; import matrix and ABI inventory regenerated from real runs; `ldd` shows no `libpython` |
| **NB-1** Tier 1 completeness | P7-03/P7-04 native modules; one spec entry per module | **in progress** (Batch A and selected modules done) | every Tier 1 module has a spec entry, an implementation and a differential test; no C shim in Tier 1; matrix regenerated |
| **NB-2** HNA v1 | Descriptor, loader, root-hold API, panic containment, version negotiation, C fixture | **planned** | one Rust cdylib and one C `cc -shared` module (plus a Zig or Go `c-shared` fixture) load under `tea infuse` and `tea build`; version-mismatch and platform-mismatch fixtures fail with the documented diagnostic; strict-GC loop test passes; boundary cost measured; `ldd` clean; no libpython |
| **NB-3** Bindgen | Header ingestion, type table, generated specs/manifests, `--check`, sqlite3 pilot | **planned** (no crate) | `hydro-bindgen --check` reproduces checked-in output in CI; unsupported-type fixture fails naming each symbol; `libm` smoke and `libsqlite3` differential vs CPython `sqlite3` pass under JIT/AOT; panic fixture aborts cleanly |
| **NB-4** Tier 3 completion | P4-04c/d full ABI, NumPy/pandas semantics, GIL/threads, P4-05 wheel install | **partial** (symbol surface + pydantic import) | NumPy 2.5 array create/dtype/ops smoke matches the oracle; pydantic model validation matches; pandas import and basic DataFrame smoke pass; threaded extension callbacks + GC stress show no deadlock/race; an abi3 wheel installs and imports under JIT/AOT; `ldd` clean |
| **NB-5** Packaging and coexistence | `hydro-manifest.toml`, Hydro wheel tag, ambiguity diagnostics, mixed packages | **planned** | an unmodified CPython wheel installed via Tea/uv imports with `RECORD` unchanged; a distribution with `.py` + HNA loads the right artifact; a duplicate-name distribution fails at compile time; manifest/descriptor mismatch fails at load |
| **NB-6** Foreign runtime bridge | One sidecar pilot, one C-ABI adapter, versioned protocol | **none** | sidecar echo/call/error round-trip matches the CPython oracle and survives a worker restart; adapter fixture initializes and shuts down cleanly; unknown protocol version is rejected with a diagnostic; no source translation anywhere |
| **NB-7** Stdlib parity | P7 batches, 297 modules accounted | **in progress** (130/297 importable; official suite 3/398) | 297/297 accounted for: importable and green in the mapped `Lib/test` subset, or listed as unsupported with a reason; both generated reports agree |

Dependencies: NB-3 → NB-2; NB-5 → NB-2 and NB-4; NB-6 → NB-2. NB-1 and NB-7
share the P7 work list; NB-4 follows P4-04c/d. A later phase MUST NOT start
by weakening an earlier gate (for example, shipping a Tier 2 shim before the
root contract is enforced).

### Status discipline

- "Implemented" means the listed slice runs under JIT and AOT with tests in
  the repository. Tier 3 is **partial**, Tier 2 is **planned**, Tier 4 is
  **none**; nothing in this document upgrades those words.
- A tier is not "supported" because a fixture imports. Each gate above names
  the observable behavior that must agree with CPython or with the documented
  contract.
- No number in this document is extrapolated from another measurement. The
  current figures are copied from [`COMPATIBILITY.md`](COMPATIBILITY.md),
  [`CPYTHON_TESTS.md`](CPYTHON_TESTS.md), [`ECOSYSTEM.md`](ECOSYSTEM.md) and
  [`C_API.md`](C_API.md), which remain the sources of truth.

## 9. References

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — tagged-value ABI (§3), module
  model, GC roots, C API/ABI decision (§9).
- [`C_API.md`](C_API.md) — P4-04 option-B rationale, probes, ABI milestones.
- [`COMPATIBILITY.md`](COMPATIBILITY.md) — generated 297-module import matrix.
- [`CPYTHON_TESTS.md`](CPYTHON_TESTS.md) — official `Lib/test` differential
  gate and baseline.
- [`ECOSYSTEM.md`](ECOSYSTEM.md) — pinned third-party wheel definitions and
  ecosystem cases.
- [`TODO.md`](TODO.md) — P4 and P7 work lists; [`LAYOUT.md`](LAYOUT.md) —
  source tree.
- `hydro-ir/src/stdlib/specs.rs` and `hydro-runtime/src/modules/` — Tier 1
  registration and dispatch.
- `hydro-runtime/src/capi/` and `hydro-runtime/src/modules/extension.rs` —
  Tier 3 shim and loader.
