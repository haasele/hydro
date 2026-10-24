# Contributing

Thanks for your interest in PyX. The project is in an early phase; the
current work status and next steps are in
[`docs/TODO.md`](docs/TODO.md) and [`AGENTS.md`](AGENTS.md).

## Setup

- Linux (x86_64/aarch64), Rust **1.85+** with `rustfmt` and `clippy`.
- A C compiler (`cc`) is required for AOT builds (`pyx build`).
- Python 3 only for the benchmark harnesses under `benchmarks/`.

```bash
cargo build -p pyx-cli -p pyx-runtime
./target/debug/pyx run examples/hello.py
```

## Gate before every commit

```bash
./scripts/verify.sh
```

The script checks `cargo fmt`, Clippy with `-D warnings`, the build, all tests,
the JIT/AOT E2E examples, negative diagnostics, and GC stress. The same steps
run in CI (`.github/workflows/ci.yml`). A commit should only be merged
if the gate is green; new features need tests and — for behavior changes —
updated documentation.

## Conventions

- **Contracts first:** IR and runtime ABI changes require an update of
  [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) and of all users.
- **No silent miscompiles:** unsupported syntax is rejected with a
  diagnostic, not ignored.
- **No runtime code without a test:** new language elements/modules need unit
  and E2E coverage; error paths belong in `examples/errors/`.
- **Diagnostics:** format `file:line:col: error: …`, stable exit code,
  `--json` remains machine-readable.
- **Commits:** short, imperative subject line (e.g. `runtime: fix dict repr`),
  one topic per commit; do not check in generated artifacts (`target/`, `.pyx/`,
  `__pycache__/`).
- **Formatting:** `cargo fmt`; no manual reformatting of other
  areas in feature commits.

## Benchmarks

After changes to codegen/runtime that may affect performance:

```bash
cargo build --release -p pyx-cli -p pyx-runtime
python3 benchmarks/pyx/run_micro.py
python3 scripts/check_benchmarks.py
```

If a threshold trips, check the cause first. Only for an intentional
change, rewrite the baseline with `python3 scripts/check_benchmarks.py --update`
and justify the change in the commit. The harnesses under
`benchmarks/bench_*.py` are reference measurements against CPython and are not
adapted to PyX.

## License

Contributions are under the same license as the project: **MIT OR Apache-2.0**
(see [`LICENSE-MIT`](LICENSE-MIT) and [`LICENSE-APACHE`](LICENSE-APACHE)).
