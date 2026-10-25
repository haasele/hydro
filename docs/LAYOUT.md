# PyX — Source Layout

As of 2026-10-24 (after the module restructuring). The crates remain the
only Frontend↔Codegen contract; inside the crates, the formerly flat
`src/*.rs` files are split into topic folders. Paths are relative to the
repository root.

```text
.
├── AGENTS.md                  work status, phases, known limits
├── PLAN.md                    strategy/guiding decisions
├── README.md                  overview, quickstart, CLI, language scope
├── CHANGELOG.md               Keep-a-Changelog format
├── CONTRIBUTING.md            setup, gate, commit rules
├── Cargo.toml                 workspace + shared dependency versions
├── scripts/
│   ├── verify.sh              full gate (fmt/clippy/build/tests/E2E/AOT/GC)
│   ├── check_benchmarks.py    local benchmark regression gate
│   ├── stdlib_compat.py       stdlib import matrix (pinned CPython 3.14.9)
│   └── cpython_compat.py      official Lib/test differential gate
├── benchmarks/
│   ├── PYX.md                 readiness matrix of the external harnesses
│   ├── bench_pyx.py           CPython/JIT/AOT comparison, 11 workloads
│   ├── bench_all.py           orchestrator (e.g. `--only pyx`)
│   └── pyx/                   micro-benchmarks, run_micro.py, results/, thresholds.json
├── spikes/p4_04/              embedded-CPython/abi3 probes + measurements
├── tests/compat/              pinned CPython test manifests (3.14.9)
├── examples/                  JIT/AOT examples + expected/*.txt
├── docs/
│   ├── ARCHITECTURE.md        binding contract (IR/ABI, GC, modules)
│   ├── C_API.md               P4-04 decision, probes, milestones
│   ├── COMPATIBILITY.md       generated stdlib import matrix
│   ├── CPYTHON_TESTS.md       official-test gate policy + baseline
│   ├── pyx.1                  roff manpage
│   ├── TODO.md                single work list
│   └── LAYOUT.md              this file
└── crates/
    ├── pyx-ir/                typed IR, only Frontend↔Codegen contract
    │   └── src/
    │       ├── lib.rs         re-exports + contract signatures
    │       ├── model.rs       Type, Const, Builtin, BinOp, exception tags
    │       ├── compat.rs      CPython target constants (3.14.9, cpython-314)
    │       ├── ir.rs          program/function/Stmt/Expr/Class/Closure
    │       ├── stdlib/        central module/signature table
    │       │   ├── mod.rs
    │       │   └── specs.rs
    │       └── tests.rs
    ├── pyx-frontend/          parsing, lowering, type checking, user modules
    │   └── src/
    │       ├── lib.rs         check_file, module loader/import resolution
    │       ├── diag.rs        diagnostics, spans, codes
    │       ├── lower/         AST → HIR/lowering
    │       └── sema/          type inference, constraints, IR emission
    ├── pyx-codegen/           Cranelift backend
    │   └── src/
    │       ├── lib.rs
    │       ├── backend/       JIT and AOT drivers
    │       └── lower/         IR → Cranelift (frames, cleanup stack, safepoints)
    ├── pyx-runtime/           value model, ABI, GC, modules, C-API shim
    │   ├── build.rs           compiles the C-variadic PyArg_ParseTuple glue
    │   ├── src/
    │   │   ├── lib.rs
    │   │   ├── abi.rs         all `extern "C"` symbols (docs/ARCHITECTURE §3)
    │   │   ├── capi/          CPython limited-API shim (PyModule/PyLong/parser)
    │   │   │                  and `parse.c` variadic adapter
    │   │   ├── core/          value, ops, container, iter, builtins, methods,
    │   │   │                  exc, format, fiber (generators/coroutines;
    │   │   │                  x86_64 register switch, ucontext fallback)
    │   │   ├── gc/            heap, color/barriers, safepoints, sweep
    │   │   ├── modules/       math/io/json/os/... + argparse, collections,
    │   │   │                  re, socket, asyncio; extension.rs (abi3 loader)
    │   │   ├── concurrency/   threads/locks/queue/processes/pool/spawn
    │   │   ├── support/       PYX1 serialization (pickle/spawn/mp.Queue)
    │   │   └── csv.rs         CSV reader/writer
    │   └── tests/             integration tests per feature (ABI level)
    └── pyx-cli/               `pyx` binary
        ├── src/
        │   ├── main.rs        ICE catching, exit codes
        │   └── cli/
        │       ├── mod.rs     CliError, usage_text, source loading
        │       ├── args.rs    shared option parser
        │       ├── diag.rs    snippet/JSON/color rendering
        │       ├── dispatch.rs subcommand table
        │       └── commands/  new, init, check, run, build, test, fmt,
        │                      lint, clean, doctor, help, uv, spawn, scan,
        │                      completions; `gc.rs` = `pyx gc`
        └── tests/             E2E, fibers, closures, user modules, extensions
                               (paths, dotted, multi-phase), completions,
                               exceptions, unittest_runner, stdlib_batch_a,
                               stdlib_batch_b, stdlib_bench_batch
                               (+ fixtures/abi3_probe.c, abi3_multiphase.c)
```

## Mapping rules

- `pyx-ir` contains **no** logic beyond type tables and contract structures.
  Changes to `ir.rs`/`model.rs` are contract changes.
- `pyx-frontend::stdlib` (re-export from `pyx-ir`) is the only place where
  native module signatures for type checking and runtime validation live.
- Runtime code lives exclusively in `pyx-runtime`; `pyx-codegen` only calls
  `pyx_runtime::abi` symbols.
- Tests live with their respective responsibility: unit tests inline, ABI
  integration tests under `crates/pyx-runtime/tests/`, process/CLI E2E under
  `crates/pyx-cli/tests/`.
- New CLI subcommands get a file under `pyx-cli/src/cli/commands/`
  and an entry in `dispatch.rs`, `args.rs`, and `usage_text()`.
