# Hydro — Source Layout

As of 2026-10-26 (after the P2 completion slice). The crates remain the
only Frontend↔Codegen contract; inside the crates, the formerly flat
`src/*.rs` files are split into topic folders. Paths are relative to the
repository root.

```text
.
├── AGENTS.md                  resume status, commands, architecture invariants
├── PLAN.md                    strategy and current-status pointer
├── README.md                  overview, quickstart, CLI, language scope
├── CHANGELOG.md               Keep-a-Changelog format
├── CONTRIBUTING.md            setup, gate, commit rules
├── Cargo.toml                 workspace + shared dependency versions
├── scripts/
│   ├── verify.sh              full gate (fmt/clippy/build/tests/E2E/AOT/GC)
│   ├── check_benchmarks.py    local benchmark regression gate (--check/--update)
│   ├── test_check_benchmarks.py  deterministic tests for the threshold gate
│   ├── bench_hyperfine.sh     hyperfine adapter for benchmark harnesses
│   ├── fuzz-smoke.sh          bounded libFuzzer/stable fuzz smoke
│   ├── fuzz-parallel.sh       multi-core stable fuzz soak (independent seeds)
│   ├── stdlib_compat.py       stdlib import matrix (pinned CPython 3.14.9)
│   └── cpython_compat.py      official Lib/test differential gate
├── benchmarks/
│   ├── HYD.md                 readiness matrix of the external harnesses
│   ├── bench_hydro.py           CPython/JIT/AOT comparison, 11 workloads
│   ├── bench_all.py           orchestrator (e.g. `--only hydro`)
│   ├── compare_engines.py     CPython/Hydro/PyPy/Go loop + startup comparison
│   └── hydro/                 micro-benchmarks, run_micro.py, results/, thresholds.json
├── spikes/p4_04/              embedded-CPython/abi3 probes + measurements
├── fuzz/                      libFuzzer targets + stable fallback harness
├── tests/compat/              pinned CPython test manifests (3.14.9)
├── examples/                  JIT/AOT examples + expected/*.txt
├── docs/
│   ├── ARCHITECTURE.md        binding contract (IR/ABI, GC, modules)
│   ├── C_API.md               P4-04 decision, probes, milestones
│   ├── COMPATIBILITY.md       generated stdlib import matrix
│   ├── CPYTHON_TESTS.md       official-test gate policy + baseline
│   ├── DIAGNOSTICS.md         diagnostic codes/spans/JSON, CLI flags, exit codes
│   ├── TODO.md                single work list
│   └── LAYOUT.md              this file
└── crates/
    ├── hydro-ir/                typed IR, only Frontend↔Codegen contract
    │   └── src/
    │       ├── lib.rs         re-exports + contract signatures
    │       ├── model.rs       Type, Const, Builtin, BinOp, exception tags
    │       ├── compat.rs      CPython target constants (3.14.9, cpython-314)
    │       ├── ir.rs          program/function/Stmt/Expr/Class/Closure
    │       ├── stdlib/        central module/signature table
    │       │   ├── mod.rs
    │       │   └── specs.rs
    │       └── tests.rs
    ├── hydro-frontend/          parsing, lowering, type checking, user modules
    │   ├── src/
    │   │   ├── lib.rs         check_file, module loader/import resolution
    │   │   │                  (siblings, packages, site-packages, PYTHONPATH)
    │   │   ├── diag.rs        diagnostics, spans, codes
    │   │   ├── lower/         AST → HIR/lowering
    │   │   └── sema/          type inference, constraints, IR emission
    │   └── tests/             fuzz-smoke parse/lower/check coverage
    ├── hydro-codegen/           Cranelift backend
    │   └── src/
    │       ├── lib.rs
    │       ├── backend/       JIT and AOT drivers
    │       └── lower/         IR → Cranelift (frames, cleanup stack, safepoints)
    ├── hydro-runtime/           value model, ABI, GC, modules, C-API shim
    │   ├── build.rs           compiles the C-variadic PyArg_ParseTuple glue
    │   ├── src/
    │   │   ├── lib.rs
    │   │   ├── abi.rs         all `extern "C"` symbols (docs/ARCHITECTURE §3)
    │   │   ├── capi/          CPython limited-API shim (PyModule/PyLong/parser)
    │   │   │                  and `parse.c` variadic adapter
    │   │   ├── core/          value, ops, container, iter, builtins, methods,
    │   │   │                  exc, format, fiber (generators/coroutines;
    │   │   │                  x86_64 register switch, ucontext fallback)
    │   │   ├── gc/            heap, color/barriers, safepoints, sweep, roots
│   │   │                  (precise shadow frames)
    │   │   ├── modules/       math/io/json/os/... + argparse, collections,
    │   │   │                  re, socket, asyncio, typing, abc, enum, types,
    │   │   │                  itertools, ast, fractions, importlib,
    │   │   │                  __future__; extension.rs (abi3 loader)
    │   │   ├── concurrency/   threads/locks/queue/processes/pool/spawn
    │   │   ├── support/       HYD1 serialization (pickle/spawn/mp.Queue)
    │   │   └── csv.rs         CSV reader/writer
    │   └── tests/             integration tests per feature (ABI level)
    ├── hydro-interp/            IR tree-walk interpreter + REPL session
    │   └── src/               frame roots, eval, stmt, host callbacks
    └── (Tea is a separate repository: ../tea/)
        ├── src/
        │   ├── main.rs        ICE catching, exit codes
        │   └── cli/
        │       ├── mod.rs     CliError, usage_text, source loading
        │       ├── args.rs    shared option parser
        │       ├── diag.rs    snippet/JSON/color rendering
        │       ├── dispatch.rs subcommand table
        │       └── commands/  new, init, check, run (JIT/--interpret),
        │                      build, test, pytest, repl, fmt, lint, clean,
        │                      doctor, help, uv (passthrough), spawn
        │                      (internal), scan (file discovery), completions;
        │                      `gc.rs` = `tea gc` (external)
        └── tests/             E2E, fibers, closures, user modules, extensions
                               (paths, dotted, multi-phase), completions,
                               exceptions, unittest_runner, stdlib_batch_a,
                               stdlib_batch_b, stdlib_bench_batch, stdlib_core,
                               interpret, repl, pytest_runner, argparse_complete,
                               precise_gc (+ fixtures/abi3_probe.c,
                               abi3_multiphase.c)
```

## Mapping rules

- `hydro-ir` contains **no** logic beyond type tables and contract structures.
  Changes to `ir.rs`/`model.rs` are contract changes.
- `hydro-frontend::stdlib` (re-export from `hydro-ir`) is the only place where
  native module signatures for type checking and runtime validation live.
- Runtime code lives exclusively in `hydro-runtime`; `hydro-codegen` only calls
  `hydro_runtime::abi` symbols.
- Tests live with their respective responsibility: unit tests inline, ABI
  integration tests under `crates/hydro-runtime/tests/`, process/CLI E2E under
  the Tea repository's `tests/`.
- New CLI subcommands live in the Tea repository (`../tea/src/cli/commands/`).
  and an entry in `dispatch.rs`, `args.rs`, and `usage_text()`.
