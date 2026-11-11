# Diagnostics and CLI Reference

`tea` reports every user error as a located diagnostic and reserves exit
status 2 for toolchain and internal failures. This page is the compact
reference for the rendering, the machine-readable form and the CLI flags that
control them.

## Global options

Global options are accepted before the command name. Compiler and project
commands also accept them before the first path argument.

| Option | Effect |
| --- | --- |
| `--verbose` | Print phase timings for `run`/`build` (frontend, JIT, codegen, link) and build-cache key/hit/miss decisions on stderr. `--version --verbose` adds target, backend, runtime and Python compatibility lines. |
| `--quiet` | Suppress non-error status: `new`/`init` creation messages, `fmt` write messages, `clean` removal messages, `lint` success summaries and passing `test` lines/output. Errors and diagnostics are always printed. |

`--verbose` and `--quiet` cannot be combined; the CLI reports a usage error and
exits with status 2.

`run` forwards everything after the source file to the program as
`sys.argv[1:]`, so diagnostic options must precede the file:

```sh
tea infuse --color=never main.py --program-flag value
```

`sys.argv[0]` is the source path. A `--` before the source file also ends
option parsing.

## Diagnostic options

Accepted by `check`, `run`, `gc`, `build`, `test`, `lint` and `fmt`:

| Option | Effect |
| --- | --- |
| `--json` | Emit one JSON array on stderr. For `test` the per-file result array goes to stdout; for `lint` the aggregated diagnostics go to stderr. |
| `--color=<auto\|always\|never>` | Colorize human diagnostics. `auto` (the default) colors only when stderr is a terminal and `NO_COLOR` is unset. |
| `--no-color` | Same as `--color=never`. |

## Human format

A human diagnostic is a cargo-style snippet on stderr:

```text
error[HYD0202]: 'int' object is not subscriptable
  --> main.py:2:7
   |
 2 | print(value[0])
   |       ^^^^^
   |
   = help: ...
```

The header contains the stable code. The location is `file:line:col`, followed
by the source line and a caret span. `= help:` is printed only when an
actionable hint exists.

## Codes

| Code | Area |
| --- | --- |
| `HYD0001` | Syntax / parse error |
| `HYD0002` | Unsupported language construct |
| `HYD0100` | Generic semantic error |
| `HYD0101` | Unknown name (with did-you-mean) |
| `HYD0102` | Operand or type mismatch |
| `HYD0103` | Call arity / argument error |
| `HYD0104` | Invalid `return` / `break` / `continue` placement |
| `HYD0201` | Unknown method or attribute |
| `HYD0202` | Indexing, slicing or subscript assignment |
| `HYD0301` | Annotation or declaration validation |

Codes are stable: tests and tooling may match them.

## JSON schema

`--json` prints exactly one array on stderr (no summary line):

```json
[{"file":"main.py","line":2,"col":7,"end_col":12,"code":"HYD0202","message":"'int' object is not subscriptable","help":null}]
```

`file`, `line`, `col`, `end_col` and `code` are always present. `help` is
`null` or a string. `col` is 1-based, byte-based (not a character or Unicode
scalar index) and points at the first byte of the span; `end_col` is exclusive.
A multi-byte identifier therefore spans `end_col - col` bytes.

## Exit status

| Status | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | User or program error: a diagnostic, a failed program, a failed test or an I/O error. `fmt --check` uses 1 when files need formatting. |
| `2` | Usage, setup or internal error, including a compiler panic (ICE). |

## Internal compiler errors

A panic inside the compiler is caught at the CLI boundary. The process prints a
short banner to stderr and exits with status 2:

```text
tea: internal compiler error (ICE)
version 0.1.0 target x86_64-linux
please report this with the command you ran and RUST_BACKTRACE=1 output
```

The panic payload is not copied into the banner. Set `RUST_BACKTRACE=1` for the
full backtrace from the standard panic hook.

## Troubleshooting

### Environment setup

Hydro resolves imports from the importing file's directory and package root
plus the process environment. The same variables apply to `check`, `infuse`,
`build`, `test` and `gc`.

| Variable | Meaning |
| --- | --- |
| `HYDRO_STDLIB` | Colon-separated list of CPython `Lib` trees. Backs standard-library modules Hydro does not vendor and derives the matching `lib-dynload` extension directories. Vendored compatibility modules in `stdlib/` take precedence over the generic tree for modules Hydro ships. |
| `HYDRO_SITE_PACKAGES` | Colon-separated list of installed-package roots. Searched before the auto-detected `VIRTUAL_ENV`/`CONDA_PREFIX` site-packages trees and used for compiled-extension discovery too. |
| `HYDRO_EXTENSION_PATH` | Colon-separated list of extra roots for compiled extensions. Searched before `PYTHONPATH`, site-packages roots, the `HYDRO_STDLIB` `lib-dynload` directories and the host CPython installations. |

Use absolute paths and only the roots the failing run needs:

```sh
export HYDRO_STDLIB=/opt/cpython-3.14.9/Lib
export HYDRO_SITE_PACKAGES=/path/to/venv/lib/python3.14/site-packages
export HYDRO_EXTENSION_PATH=/opt/cpython-3.14.9/lib-dynload

tea check main.py
tea infuse main.py
```

`HYDRO_STDLIB` also accepts an installed version root such as
`/usr/lib/python3.14`; the sibling (`<prefix>/lib-dynload`) or child
(`<version-root>/lib-dynload`) directory is derived for both layouts.
`PYTHONPATH`, `VIRTUAL_ENV` and `CONDA_PREFIX` are consulted as well, at the
points documented in [`NATIVE_BINDINGS.md`](NATIVE_BINDINGS.md) and
[`C_API.md`](C_API.md); those pages stay authoritative for the exact order.
`tea --version --verbose` prints the target, backend, runtime and Python
compatibility line.

### Common import failures

| Symptom | Check |
| --- | --- |
| `error[HYD0002]: module 'x' not found: not supported on the import search path` | The frontend found no source or extension for `x`. Confirm the file or package exists under a configured root, set `HYDRO_STDLIB` for unvendored standard-library modules and `HYDRO_SITE_PACKAGES` for installed packages, then rerun `tea check --json main.py`. |
| `ModuleNotFoundError: No module named 'x'` at run time, with no compile diagnostic | The import is deferred inside a function, class or conditional, so the frontend did not link it. Check the same roots; an unconditional module-level import turns it into a located `HYD0002` at check time. |
| `error[HYD0201]: module 'x' has no attribute 'y'` | `x` is a native module whose `ModuleSpec` does not expose `y`; this is a coverage limit, not a path problem. Compare [`COMPATIBILITY.md`](COMPATIBILITY.md) and [`NATIVE_BINDINGS.md`](NATIVE_BINDINGS.md). |
| `error[HYD0201]: module 'x' does not export a function named 'y'` | Check the spelling first. If `y` exists, the source defines it dynamically or in an order the lowerer cannot prove; prefer `import x` plus `x.y` at call time, or make the definition static. |
| `No module named 'x': found artifact '…', which is built for a different CPython ABI` | The wheel or extension targets another Python version or platform. Loadable suffixes are `.so`, `.abi3.so` and `.cpython-314-<platform>.so`; install a 3.14 or abi3 build. |
| `ImportError: … undefined symbol: …` | The extension needs a CPython C-API symbol the shim does not provide yet. Keep the extension path and symbol for a C-API report; the current surface is in [`C_API.md`](C_API.md). |
| `AttributeError`/`ImportError` from a partially initialized module | Move the circular import into a function or read the member after initialization. Hydro binds import-cycle back edges late through the runtime global table; initialization order still matters. |
| `ModuleNotFoundError: No module named 'test…'` during a differential run | Use the harnesses below: they set `HYDRO_STDLIB` to the pinned source tree's `Lib`, which is where `test` and `test.support` live. |

A missing vendored module falls through to `HYDRO_STDLIB` when that variable
is set; otherwise resolution relies on the vendored `stdlib/` tree plus the
development-only pinned `.hydro/cpython` cache. Run `tea check` first when the
problem is static, and `tea infuse` when only run-time behavior differs.

### AOT and JIT differences

`tea infuse` compiles with the Cranelift JIT at run time, `tea infuse
--interpret` walks the same typed IR in the interpreter, and `tea build` emits
an object, links it with `cc` against `libhydro_runtime.a` and embeds the
resolved user modules into the binary. All three share the frontend, IR and
runtime, so a wrong result in program output usually reproduces in all of
them.

Treat a failure as engine-specific only when the same source, arguments, stdin
and environment:

- fails during `tea build` (codegen or link) while `tea infuse` runs, or
- produces different output between the JIT run and the built binary.

A frontend diagnostic fails both paths; it is a source or import problem, not
an AOT difference.

The AOT object is PIC because it is linked into a PIE; JIT code is not. AOT
likewise needs the runtime archive (`--runtime-lib PATH` or
`HYDRO_RUNTIME_LIB`) and `cc`. The build cache under `.hydro/cache` is local;
`--no-cache` bypasses it. Current engine-specific items are listed in
[`FULL_COMPAT.md`](FULL_COMPAT.md), not duplicated here.

### GC strict mode

The default GC is mixed mode: precise shadow-stack roots plus a conservative
native-stack and register scan. `HYDRO_GC_PRECISE=1` disables every
conservative scan, so an object survives only through a registered precise
root. Use strict mode to expose missing roots; a crash or changed output under
strict mode is a root bug, not a threshold-tuning problem.

```sh
HYDRO_GC_PRECISE=1 tea infuse main.py
HYDRO_GC_PRECISE=1 HYDRO_GC_STRESS=1 tea infuse main.py
HYDRO_GC_STATS=1 tea gc --json main.py
```

`HYDRO_GC_STRESS=1` collects before every allocation and never adapts the
threshold. `HYDRO_GC_STATS=1` reports pause, mark and sweep time, frees, the
effective threshold and `precise=mixed|strict` on stderr. `tea gc` runs the
file like `tea infuse`, prints the counters afterwards and emits a machine
readable last line with `--json`. `HYDRO_GC_THRESHOLD=<n>` pins a fixed
threshold and disables adaptation; do not leave it raised to hide a collection
bug. Variables, defaults and bounds are documented in
[`ARCHITECTURE.md`](ARCHITECTURE.md) §7.

### Official-suite triage

Run these from the repository root. Both harnesses pin CPython 3.14.9 and
resolve the source tree from `--source` or the `.hydro/cpython/Python-3.14.9`
cache; Hydro resolves from `--tea`, `$HYDRO_BIN`, or the sibling Tea release
or debug binary.

```sh
# One differential case, quick feedback (scratch outputs, not the reports)
python3 scripts/cpython_compat.py --only test_bisect \
  --json /tmp/cpython-one.json --md /tmp/cpython-one.md

# Resumable triage with full per-test logs and a clustered backlog
python3 scripts/cpython_triage.py --limit 1
python3 scripts/cpython_triage.py --only test_math,test_json
python3 scripts/cpython_triage.py --limit 20 --resume
python3 scripts/cpython_triage.py                     # full selected suite

# Release gate and offline manifest checks
python3 scripts/cpython_compat.py --gate
python3 scripts/cpython_compat.py --check-manifest
python3 scripts/cpython_compat.py --update-manifest

# Standard-library import matrix
python3 scripts/stdlib_compat.py
```

`--only` takes comma-separated names with or without `.py`, and `--limit`
caps a run. `cpython_compat.py` writes
`benchmarks/compat/cpython-3.14.9.json` and regenerates
[`CPYTHON_TESTS.md`](CPYTHON_TESTS.md) by default; pass `--json`/`--md` to
write a scratch path when selecting a subset. `stdlib_compat.py` writes
`benchmarks/compat/latest.json` and regenerates
[`COMPATIBILITY.md`](COMPATIBILITY.md) by default. `cpython_triage.py` writes
`.hydro/official/<test>.log`, `.hydro/official/results.jsonl`,
`.hydro/official/triage.json` and `.hydro/official/triage.md`; `--resume`
skips tests already recorded for the same pin, source, timeout and memory
threshold. The triage runner's exit codes are 0 (report written), 1 (usage or
no selected tests), 2 (missing or unusable Hydro, oracle or source tree) and
130 (interrupted; checkpoint and partial report kept). Status meanings and the
differential rules are in [`CPYTHON_TESTS.md`](CPYTHON_TESTS.md) and
[`COMPAT_STRATEGY.md`](COMPAT_STRATEGY.md).

### Reporting a Hydro-vs-CPython difference

A difference is actionable only when CPython 3.14.9 passes the same snippet and
Hydro fails. `expected_fail` and `cpython_fail` are oracle-environment results,
not Hydro failures ([`COMPAT_STRATEGY.md`](COMPAT_STRATEGY.md)).

Include in the report:

1. a minimal `repro.py` and the exact commands used for both engines;
2. `tea --version --verbose` and the CPython version (`python3.14 -VV`, or
   `"$(uv python find 3.14.9)" -VV`);
3. expected and actual stdout/stderr, plus the exit status (0, 1 or 2 as
   defined above) and `tea check --json repro.py` when a diagnostic is
   involved;
4. whether `tea infuse`, `tea infuse --interpret` and the AOT binary reproduce
   it;
5. the import roots in effect (`HYDRO_STDLIB`, `HYDRO_SITE_PACKAGES`,
   `HYDRO_EXTENSION_PATH`) and the full traceback;
6. the matching entry in [`FULL_COMPAT.md`](FULL_COMPAT.md) or
   [`COMPATIBILITY.md`](COMPATIBILITY.md), if the area is already tracked.

Open the report in the Hydro repository's issue tracker; keep Tea CLI-only
problems in the Tea repository.

## Migrating from CPython

Hydro targets **CPython 3.14.9**. The oracle/source pin and the rule that a
difference counts only when the oracle passes and Hydro exits 0 are in
[`CPYTHON_TESTS.md`](CPYTHON_TESTS.md) and
[`COMPAT_STRATEGY.md`](COMPAT_STRATEGY.md). CPython 3.13 is security-only and
3.15 is newer than the supported oracle ([`COMPATIBILITY.md`](COMPATIBILITY.md)).

Start with the language and stdlib scope in [`README.md`](../README.md). The
supported surface is a subset, not full CPython parity, and `tea run` executes
CPython through uv rather than the Hydro engine; use `tea check`, `tea infuse`
and `tea build` to evaluate Hydro behavior.

Native modules have limits:

- Standard-library modules are Hydro-native Rust modules, pinned CPython
  pure-Python sources, or C extensions loaded through the Hydro C-API shim.
  Import success does not imply API or semantic parity
  ([`COMPATIBILITY.md`](COMPATIBILITY.md)).
- Unmodified CPython C extensions load only for supported ABI tags and the
  implemented shim surface ([`C_API.md`](C_API.md)); arbitrary extensions,
  `ctypes`/`cffi` and the full ABI remain open
  ([`NATIVE_BINDINGS.md`](NATIVE_BINDINGS.md)).
- `pickle` uses Hydro's own HYD1 format and is not CPython-compatible; use it
  only between Hydro processes.

Known deviations are tracked by area: [`FULL_COMPAT.md`](FULL_COMPAT.md) for
language/stdlib gaps and the strict gate, [`COMPATIBILITY.md`](COMPATIBILITY.md)
for per-module import status, and [`C_API.md`](C_API.md)/
[`NATIVE_BINDINGS.md`](NATIVE_BINDINGS.md) for native and ABI limits.

The support policy lives in [`COMPAT_STRATEGY.md`](COMPAT_STRATEGY.md)
(sources of truth, rules and gates) and [`FULL_COMPAT.md`](FULL_COMPAT.md)
(strict 100% gate and exclusions). Per-module status is in
[`COMPATIBILITY.md`](COMPATIBILITY.md); the official-suite pin and commands
are in [`CPYTHON_TESTS.md`](CPYTHON_TESTS.md). The formal P4-08 support policy
is tracked in [`TODO.md`](TODO.md).

## See also

- `docs/ARCHITECTURE.md` for the frontend diagnostic contract and GC details.
- `docs/tea.1` for the command-line manual page.
- `docs/COMPAT_STRATEGY.md` for the compatibility rules and triage tooling.
- `docs/CPYTHON_TESTS.md` for the official-suite pin, statuses and backlog.
