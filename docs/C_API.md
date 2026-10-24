# PyX — C API/ABI Strategy (P4-04)

Status: **decided 2026-10-24** — Option B (PyX-native CPython C-API/ABI
layer). The **first abi3 slice is implemented and verified**: a limited-API
extension (`.so`) next to a PyX source file imports and calls under JIT and
AOT with no `libpython` (see §3.1). The full surface, wheels/venvs and the
ecosystem program remain open.

This document records the decision basis for P4-04 and the follow-up items
(P4-05/P4-06/P4-09/P4-10). It contains measured probe results and an explicit
list of what is *not* yet implemented.

## 1. Question

Should CPython C extensions (NumPy, `pydantic-core`, …) run under PyX, and if
so, via which path? The answer determines whether PyX objects need a
CPython-compatible layout in the long term or whether a translation layer
suffices. It affects the object model, GC, refcounting and ABI stability.

## 2. Decision

**Option B: implement the CPython C API/ABI layer on PyX's own runtime; do not
embed `libpython` in the production runtime.**

- CPython 3.14 development files are used at build time (headers, `python3-config`)
  to compile extensions and test fixtures, and the CPython interpreter is used
  as a **differential test oracle** — never as a shipped runtime dependency.
- `ldd pyx` must not list `libpython`. One process, one GC, one object graph.
- Two compatibility tiers, delivered in order:
  1. **`abi3`/limited API** for new and simple extensions (module init,
     `PyArg_Parse*`, scalar/container constructors, errors, capsules,
     buffer protocol basics).
  2. **Full CPython 3.14 ABI** required by NumPy 2.5 and `pydantic-core`:
     `PyTypeObject` layout, refcounted `PyObject` handles, vectorcall,
     `PyType_FromSpec`/slots, GC traverse/clear integration, full buffer
     protocol and the GIL contract at extension boundaries.

Option A (embedding `libpython`) is rejected as the production path because it
imposes CPython's startup, refcount model and a second VM on exactly those
applications that are supposed to benefit from PyX. Option C (no extensions)
is rejected as the long-term plan because NumPy/pandas/pydantic/FastAPI are
required for ecosystem compatibility; it remains only as an explicitly
documented fallback if the shim proves unfundable.

## 3. Evidence (probe, 2026-10-24)

Environment: Linux x86_64, Python **3.14.8** with development files
(`/usr/include/python3.14/Python.h`, `/usr/lib/libpython3.14.so`,
`python3-config --embed`), NumPy 2.5.4 and pydantic 2.14.1 installed.
Reproducible probe: `spikes/p4_04/build.sh` (sources and full output there).

### Option A — embedded CPython cost baseline

| Measurement | Result |
|---|---:|
| `Py_Initialize()` | 6.67 ms |
| `Py_Finalize()` | 7.46 ms |
| embedded `import numpy` | 41.64 ms |
| embedded `import pydantic` | 11.96 ms |
| 100k `PyObject_Call` boundary calls | 1.65 ms |
| peak RSS after probes (`VmHWM`) | 31.5 MB |

For comparison, a PyX AOT `hello` starts in **0.80–1.04 ms** (bench_pyx). Any
process that imports a C-extension package through Option A therefore pays
CPython initialization plus module import before PyX code can run, and the
application graph is split across two object models and two collectors —
exactly the opposite of the startup/efficiency goals.

### Option B — feasibility artifact

A `Py_LIMITED_API = 0x030C0000` extension (`pyx_capi_probe`) builds without
linking `libpython`, imports in CPython and returns `add(2,3) = 5` with
`tag = "abi3"`. Its undefined CPython symbols are the first concrete shim
surface of an `abi3` extension:

```
PyArg_ParseTuple
PyLong_FromLong
PyModule_AddStringConstant
PyModule_Create2
```

This confirms the loader model: extensions are ordinary `.so` files with
undefined CPython symbols that the host runtime resolves. PyX will export
those symbols from its shim (option B) instead of resolving them from
`libpython` (option A).

### 3.1 Implemented slice (2026-10-24)

The first vertical slice of option B is in `pyx-runtime` and verified by
`crates/pyx-cli/tests/extensions.rs`:

- **Shim** (`crates/pyx-runtime/src/capi/`): `PyModule_Create2`,
  `PyModuleDef_Init` (multi-phase results, `Py_mod_exec` slots),
  `PyModule_AddStringConstant`, `PyModule_AddObject`, `PyLong_FromLong`,
  `PyLong_AsLong`, `PyUnicode_FromString`/`AsUTF8`, `PyFloat_FromDouble`/
  `AsDouble`, `PyBytes_FromStringAndSize`/`AsString`/`Size`,
  `PyList_New`/`Size`/`GetItem`/`SetItem`/`Append`,
  `PyTuple_New`/`Size`/`GetItem`/`SetItem`,
  `PyDict_New`/`SetItem`/`SetItemString`/`GetItem`/`GetItemString`,
  `PyCapsule_New`/`GetPointer`/`GetName`/`IsValid`/`SetPointer`,
  `PyObject_CheckBuffer`/`GetBuffer`/`PyBuffer_Release` (read-only contiguous
  bytes, verified `Py_buffer` layout), `PyExc_*` singletons,
  `Py_IncRef`/`Py_DecRef` (+ `_Py_*` aliases required by the 3.12+ limited
  API headers), `PyErr_Occurred`/`SetString`/`Clear`, and the variadic
  `PyArg_ParseTuple` (C shim in `parse.c`, formats `i/l/n/s/O/p/d`, `|`,
  `:`/`;`). Objects live in a process-lifetime arena; `Py_DecRef` never frees
  in this slice (no use-after-free, but no reclamation either).
- **Loader** (`crates/pyx-runtime/src/modules/extension.rs`): discovery of
  `<name>.so`, `<name>.abi3.so` or `<name>.*.so` at compile time in the
  frontend (sibling directory, `PYX_EXTENSION_PATH`, `PYTHONPATH`,
  `PYX_SITE_PACKAGES`, venv/conda `site-packages`); lazy `dlopen(RTLD_NOW)` +
  `PyInit_<name>` resolution, cached by path; single-phase modules use the
  returned module object, multi-phase ones are created from their
  `PyModuleDef` and their `Py_mod_exec` slots run once before the load is
  cached; callable values participate in `pyx_call`; errors map to the
  matching PyX exception.
- **Linking:** `pyx` and AOT binaries are linked with `--export-dynamic` and
  pin the shim symbols via `capi::anchor()` in `pyx_runtime_init`; extension
  symbols resolve from the process, not from `libpython`.
- **Verified fixture:** `pyx_capi_probe.abi3.so` (`Py_LIMITED_API 0x030C0000`)
  prints `5`, `abi3`, `42`, `abi3` under `pyx run` and an AOT binary; a second
  fixture (`abi3_multiphase.c`) returns a `PyModuleDef` from `PyInit_*`, adds
  `tag` from a `Py_mod_exec` slot and prints `multiphase`/`42` under both;
  `ldd` shows no `libpython`; arity errors are `TypeError`, missing members
  are `AttributeError`, unsupported argument types are `TypeError`.
- **Not in this slice:** arbitrary object passing beyond bounded recursive
  list/tuple/dict copies, writable/multi-dimensional buffers, capsule
  destructors, `Py_mod_create`, GIL/thread emulation, full 3.14 ABI
  (NumPy/pydantic-core).

### Scope of "full stdlib"

CPython 3.14 `sys.stdlib_module_names` contains **297** modules:

| Layer | Count | Consequence for PyX |
|---|---:|---|
| pure-Python modules | 174 | must run **unmodified** on the PyX language/runtime |
| extension modules | 71 | native Rust implementation, `abi3`/full-ABI wheel, or documented replacement |
| builtin/frozen modules | 52 | runtime internals (`sys`, `_io`, `_imp`, `_thread`, `itertools`, …) |

The stdlib program is tracked in `docs/TODO.md` (P4-01…P4-11) and gated by a
machine-readable compatibility report, not by prose claims.

## 4. Object-model consequences

- PyX values remain tagged `i64`; heap objects keep the kind header and the
  conservative/concurrent GC.
- A CPython-visible object is a **refcounted handle** allocated by the shim:
  - For immutable scalars (`int`, `float`, `str`, `bytes`, `None`, `bool`) the
    handle may cache/convert the tagged value.
  - For containers and user objects the handle points at the PyX object and
    raises its refcount; the collector treats the refcount as an additional
    root, and the shim's `tp_traverse`/`tp_clear` mirror GC edges.
  - Extensions that require real `PyObject` structs (NumPy) see the full
    struct; PyX-native code never does.
- The GIL is **emulated only around extension entry points** (one global lock
  held while foreign code runs). PyX code remains GIL-free; extension
  callbacks acquire the same lock.
- Identity (`is`) and refcount semantics are preserved within the CPython
  handle graph; mixing raw PyX values into extension APIs requires an explicit
  conversion function and is rejected with a clear error where not supported.

## 5. Milestones and acceptance gates

| Milestone | Content | Acceptance |
|---|---|---|
| P4-04a | `abi3` shim: module init, `PyArg_Parse*`, scalar/container constructors, error API, capsuled types, buffer basics | **Done for the defined subset:** module/scalar/container APIs, capsules, read-only bytes buffers, `PyExc_*`; fixture imports/runs under PyX with no `libpython`. Further symbols are driven by P4-10 |
| P4-04b | Extension loader: `dlopen` + `PyInit_*`, wheel ABI tags, package submodules, multi-phase init | **Done:** sibling/env/venv discovery, wheel tags validated at compile time, dotted package submodules, single- and multi-phase init (exec slots, clear slot errors), caching and error paths green |
| P4-04c | Full 3.14 ABI: `PyTypeObject`, refcounts/GC integration, vectorcall, full buffer protocol | NumPy 2.5 smoke (array create/ops/dtype) and `pydantic-core` model validation |
| P4-04d | GIL emulation and callback safety | threaded extension callbacks + GC stress; no deadlock/data race |
| P4-05 | `ctypes`/`cffi` boundaries | `ctypes.CDLL` on libc, `cffi` ABI-mode smoke |
| P4-09 | Stdlib parity (297 modules) | per-module matrix: import + CPython test subset pass, or documented unsupported reason |
| P4-10 | Ecosystem suites (numpy, pandas, pydantic, fastapi, django, …) | pinned versions, upstream test subset + app smoke, reproducible report |
| P4-11 | Ecosystem benchmarks | startup/import/throughput/RSS versus CPython with thresholds |

## 6. Risks

- **Full ABI coupling:** NumPy/pydantic-core use the version-specific
  `PyTypeObject` layout, not only `abi3`. Supporting one CPython minor version
  at a time is acceptable; the matrix must state it.
- **Effort:** `PyArg_Parse*`, type objects, buffer protocol, GIL emulation and
  refcount/GC coordination are multi-month work; the milestones above are
  ordered so each one is independently testable.
- **Performance claims:** boundary conversion costs must be measured per
  library; no promise that extension-heavy code matches native PyX speed.
- **Licensing/distribution:** CPython headers are used at build time only;
  extensions keep their own licenses.

## 7. What this document does not claim

- No general CPython C extension support: only the defined `abi3` subset in
  §3.1 (plus whatever symbols a real extension happens to need) runs today.
- No NumPy/pandas/pydantic/FastAPI compatibility is achieved or promised by
  this decision; the gates in §5 define when a claim is allowed.
- No benchmark number is extrapolated beyond the measured probe outputs in §3.
