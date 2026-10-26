# P4-04: embedded CPython and stable-ABI probes

Two small probes for the CPython-embedding option (option A) and the
stable-C-ABI option (option B). They are measurement/feasibility scaffolding,
not Hydro components.

## What is measured

`embed_probe.c` embeds CPython in a plain C process and reports:

- `init_ms`: wall time around `Py_Initialize()`.
- `pass_ms`: wall time around `PyRun_SimpleString("pass")`.
- `import_numpy_ms`: first import of numpy via `PyImport_ImportModule`.
- `import_pydantic_ms`: first import of pydantic via `PyImport_ImportModule`.
- `call100k_ms`: wall time for 100,000 `PyObject_CallOneArg` calls of a Python
  `inc(x)` function, excluding the one-time function definition.
- `finalize_ms`: wall time around `Py_FinalizeEx()`.
- `rss_kb`: `VmHWM` (peak resident set size) from `/proc/self/status`.

If a Python import or call step fails, its line is printed as
`...=unavailable` and the remaining steps continue. Import timing includes
CPython's module lookup and initialization, not compilation of a user module.

`abi3_probe.c` builds the module `hydro_capi_probe` with
`Py_LIMITED_API=0x030C0000`, without linking `libpython`. It exports:

- `add(a, b)` using `PyArg_ParseTuple` and `PyLong_FromLong`.
- `tag == "abi3"`.

Because the shared object deliberately has unresolved CPython C-API symbols,
the undefined-symbol list printed by `build.sh` is the initial set of symbols a
non-CPython host (or a compatibility shim) would need to provide for this
otherwise-minimal extension. It is not evidence that the full C-API surface or
real-world extensions can be supported.

## Requirements

- Linux (the RSS read uses `/proc/self/status`; `nm -D` reads ELF symbols).
- CPython 3.14 with development headers, an embeddable library, and a working
  `python3-config --embed`.
- A C compiler (`cc`) and the usual binutils/awk/grep tools.
- `numpy` and `pydantic` are optional: if absent, their import lines report
  `unavailable`. The values are only comparable when the packages are present.

## Run

From the repository root:

```sh
spikes/p4_04/build.sh
```

The script resolves all paths relative to itself. It compiles `embed_probe`
with the include flags and embed/link flags reported by `python3-config`,
compiles the extension with `-shared -fPIC` and include flags only (no
libpython), runs the embedded probe, imports the extension in CPython to check
`tag` and `add(2, 3)`, and prints the undefined CPython symbols plus their
count.

## Results

Run on 2026-10-24, Linux x86_64 (Intel Core Ultra 7 255H), Python 3.14.8,
NumPy 2.5.4, pydantic 2.14.1. These are single-run probe numbers for the
decision, not a benchmark suite.

| Metric | Output field(s) | Where the number comes from | Result |
| --- | --- | --- | --- |
| CPython initialization | `init_ms` | Monotonic-clock interval around `Py_Initialize()` in `embed_probe.c` | 6.670 ms |
| `pass` execution | `pass_ms` | Monotonic-clock interval around `PyRun_SimpleString("pass")` | 0.036 ms |
| numpy import | `import_numpy_ms` | Monotonic-clock interval around `PyImport_ImportModule("numpy")` | 41.637 ms |
| pydantic import | `import_pydantic_ms` | Monotonic-clock interval around `PyImport_ImportModule("pydantic")` | 11.959 ms |
| 100k one-argument calls | `call100k_ms` | Monotonic-clock interval around 100,000 `PyObject_CallOneArg` calls | 1.645 ms |
| CPython finalization | `finalize_ms` | Monotonic-clock interval around `Py_FinalizeEx()` | 7.461 ms |
| Peak RSS | `rss_kb` | `VmHWM` in `/proc/self/status`, read after finalization | 31,460 KB |
| ABI3 extension smoke test | `abi3_tag`, `abi3_add_2_3` | CPython imports `hydro_capi_probe.so` and calls the exported members | `abi3`, `5` |
| Undefined CPython symbols | `undefined_py_symbols` | Count of unique `Py*` names from `nm -D --undefined-only hydro_capi_probe.so` | 4 (`PyArg_ParseTuple`, `PyLong_FromLong`, `PyModule_AddStringConstant`, `PyModule_Create2`) |

Interpretation for the P4-04 decision: embedded CPython costs ~14.1 ms for
init+finalize and ~41.6 ms for a NumPy import before any Hydro code runs, while a
Hydro AOT process starts in ~0.8–1.0 ms. The ABI3 extension needs four shim
symbols, confirming that extensions are ordinary shared objects whose CPython
symbols the host can provide. See `../../docs/C_API.md` for the decision and
gates.
