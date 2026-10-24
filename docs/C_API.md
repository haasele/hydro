# PyX — C API/ABI Strategy (P4-04 Spike)

As of: 2026-10-24. This document is the **decision basis** for
P4-04 and the subsequent items P4-05/P4-06. It deliberately contains **no
measurements**: the spike is blocked in this environment (see below), and
invented runtimes/success rates would not be a basis.

## 1. Question

Should CPython C extensions (NumPy, `pydantic-core`, …) run under PyX, and
if so, via which path? The answer determines whether PyX objects need a
CPython-compatible layout in the long term or whether a translation layer
suffices. It affects the object model, GC, refcounting, and ABI stability.

## 2. Options

### A — Embed CPython (`libpython`)

PyX loads a CPython library and runs C extensions in a real
CPython interpreter instance. PyX values are converted at the boundary
(or the extension works only with CPython objects that PyX wraps).

- **Advantages:** full C API/ABI compatibility, `abi3` extensions run without
  PyX-specific reimplementations; a NumPy smoke test is realistic.
- **Costs:** CPython becomes a runtime dependency (contradicts D2 only for
  the compatibility mode, not for the standard path); two object models and
  two GCs must coexist; conversion/refcounting at every boundary;
  distribution/licensing questions; `libpython` must match the PyX build.
- **Prerequisites:** `Python.h`, `python3-config` or `pkg-config python3`,
  a linkable `libpython3.x` (shared or static), and a matching
  include/library path.

### B — ABI3 shim on PyX objects

PyX implements the CPython C API (at least the `Py_LIMITED_API`/
`abi3` scope) itself on top of its own objects. Extensions are linked against
this shim.

- **Advantages:** CPython does not remain a runtime dependency; one process, one
  GC; theoretically leaner than a second VM.
- **Costs:** very high. `PyObject`/`PyTypeObject` layouts, refcounting,
  `PyArg_ParseTuple`, buffer protocol, module init (`PyModuleDef`), GIL
  expectations, and `abi3` symbols must match exactly. The shim must also
  coordinate refcounts with the conservative mark-and-sweep GC; retrofitting
  it into the tagged `i64` value model is a breach of the ABI invariants
  (heap pointers are not `PyObject*`).
- **Prerequisites:** the same headers for compiling the extensions
  as well as a separate, testable shim library; an `abi3` symbol comparison is
  not possible without headers/toolchain.

### C — Limited compatibility (no C extensions)

PyX supports only pure-Python packages and a curated native stdlib.
`ctypes`/`cffi` are not offered as a CPython ABI bridge.

- **Advantages:** no object model risk, no second VM, clear boundaries.
- **Costs:** no NumPy/Pydantic/FastAPI ecosystem; the FastAPI benchmark stays
  blocked. Sufficient for many scripts, too little for "Bun for Python".

**Recommendation (provisional):** prototype option A as a compatibility mode,
option B only if measurements show that the embedding overhead or
distribution is untenable. Option C is the fallback if P4-04 is permanently
unfundable. This recommendation is **not** a decision; it will be recorded
only after the spike in `docs/ARCHITECTURE.md` §9.

## 3. Environment probe (2026-10-24, Linux x86_64)

Executed in the repository, without installing packages:

| Check | Result |
|---|---|
| `python3 -V` | `Python 3.12.3` |
| `command -v python3-config` | not present (also no `python3.12-config`) |
| `pkg-config --exists python3` | negative; no `python3.pc` |
| `sysconfig.get_paths()["include"]` | `/usr/include/python3.12` |
| `ls /usr/include/python3.12/Python.h` | **missing** (the directory contains unrelated headers, e.g. `Imaging.h`) |
| `LIBRARY`/`INSTSONAME` | `libpython3.12.a` / `libpython3.12.so.1.0` |
| `ls libpython3.12.{so,a}` | both **missing**; only `libpython3.12.so.1.0` (runtime SO) is present |
| `cc`/`gcc` | present (`/usr/bin/cc`, `/usr/bin/gcc`) |

Result: Although a CPython 3.12 interpreter including the runtime SO is
installed, there is **no development package**. Without `Python.h`,
`python3-config`, and a linkable `libpython3.12.so`/`.a`, neither an
embedding smoke test nor an `abi3` shim can be compiled. The existing
runtime SO is not sufficient because the linker expects the unversioned
SONAME entry (`libpython3.12.so`).

**Status P4-04: `[!]` blocked.** The blocker is the environment, not an
architectural indecision. As soon as a CPython development package
(`python3-dev`/`python3.12-dev` or an equivalent sysroot) is available,
the following steps become executable.

## 4. Spike plan (once the environment is in place)

1. **Embedding smoke (option A):** a minimal C program that uses
   `Py_Initialize`, `PyRun_SimpleString("print(1)")`, and `Py_Finalize`,
   links against `libpython`, and runs in a PyX JIT process. Success criterion:
   exit 0, no symbol/linker errors.
2. **`abi3` smoke:** a C module with `#define Py_LIMITED_API 0x030C0000`,
   `PyModuleDef`, and a trivial function; import from CPython and from the
   embedding context. Success criterion: import + call.
3. **NumPy smoke (only if 1–2 are green):** `import numpy` in the embedding
   context, a small array operation; logs import time, RSS, and
   conversion effort at the PyX↔CPython boundary.
4. **Shim effort (option B):** symbol/header comparison against the
   `abi3` scope; list of required types/functions and a
   refcount concept for the tagged heap. No code before 1–3 are measured.
5. **Decision:** enter the result, measurements, and chosen option in
   `docs/ARCHITECTURE.md` §9 and `docs/TODO.md` (P4-04/P4-05).

## 5. Effects on other open items

- **P4-05 (Extension Loading):** depends entirely on the option; without a
  decision, no `.so` loader.
- **P4-06 (`ctypes`/`cffi`):** option C would document the boundaries
  explicitly; option A could ship `ctypes` via CPython.
- **P4-07 (Serialization/Numerics):** independent; `pickle` remains the own,
  non-CPython-compatible PYX1 format until a compatibility layer
  is decided.
- **Object model/GC:** option B would force refcounting; option A allows
  the existing conservative GC in the PyX path and CPython refcounting only in
  compatibility mode.

## 6. What this document does not claim

- No statement that any of the options already works.
- No runtime, memory, or compatibility measurements.
- No promise that NumPy/FastAPI will ever be supported.
- No change to `Cargo.toml`/CI; the spike remains an isolated
  prototype until P4-04 is decided.
