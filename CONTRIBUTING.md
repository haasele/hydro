# Contributing

Thanks for your interest in Hydro. The project is in an early phase; the
current work status and next steps are in
[`docs/TODO.md`](docs/TODO.md) and [`AGENTS.md`](AGENTS.md).

## Setup

- Linux (x86_64/aarch64), Rust **1.85+** with `rustfmt` and `clippy`.
- A C compiler (`cc`) is required for the runtime build script and AOT link.
- Python 3 only for the benchmark harnesses under `benchmarks/`.
- Tea (sibling repo `../tea`) for CLI/E2E/AOT gates and running examples.

```bash
cargo build -p hydro-runtime
../tea/target/debug/tea infuse examples/hello.py
```

## Gate before every commit

```bash
./scripts/verify.sh
```

The script checks `cargo fmt`, Clippy with `-D warnings`, the runtime build,
all workspace tests, the bounded fuzz smoke and the offline CPython manifest.
CLI/E2E/AOT/pytest/PGO gates run in the Tea repository. A commit should only
be merged if both gates are green; new features need tests and — for behavior
changes — updated documentation.

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
  one topic per commit; do not check in generated artifacts (`target/`, `.hydro/`,
  `__pycache__/`).
- **Formatting:** `cargo fmt`; no manual reformatting of other
  areas in feature commits.

## Benchmarks

After changes to codegen/runtime that may affect performance:

```bash
cargo build --release -p hydro-runtime
python3 benchmarks/hydro/run_micro.py
python3 scripts/check_benchmarks.py
```

If a threshold trips, check the cause first. Only for an intentional
change, rewrite the baseline with `python3 scripts/check_benchmarks.py --update`
and justify the change in the commit. Threshold enforcement is local/release
only; CI runs the gate as a non-blocking report and runs
`python3 scripts/test_check_benchmarks.py`. The harnesses under
`benchmarks/bench_*.py` are reference measurements against CPython and are not
adapted to Hydro.

Additional harnesses:

```bash
scripts/bench_hyperfine.sh --quick     # hyperfine wrapper (skips if unavailable)
python3 benchmarks/compare_engines.py  # CPython/Hydro/PyPy/Go loop + startup
```

Both validate outputs and report missing tools/engines as skipped rather than
fabricating numbers; see `benchmarks/HYD.md`.

## Releases

Tagging `vX.Y.Z` triggers `.github/workflows/release.yml`: it runs the tests,
builds `tea` from the tagged workspace with `Cargo.lock` (`--locked`), packages
a Linux x64 tarball with the AOT runtime archive, licenses and manpage,
smoke-tests JIT and AOT, and uploads it to the GitHub release. No repository
secrets are needed (only the automatic `GITHUB_TOKEN`); other platforms
remain Q-09.

## License

Contributions are under the same license as the project: **MIT OR Apache-2.0**
(see [`LICENSE-MIT`](LICENSE-MIT) and [`LICENSE-APACHE`](LICENSE-APACHE)).
