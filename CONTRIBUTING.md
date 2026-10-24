# Mitwirken

Danke fürs Interesse an PyX. Das Projekt ist in einer frühen Phase; der
aktuelle Arbeitsstand und die nächsten Schritte stehen in
[`docs/TODO.md`](docs/TODO.md) und [`AGENTS.md`](AGENTS.md).

## Setup

- Linux (x86_64/aarch64), Rust **1.85+** mit `rustfmt` und `clippy`.
- Ein C-Compiler (`cc`) wird für AOT-Builds (`pyx build`) benötigt.
- Python 3 nur für die Benchmark-Harnesse unter `benchmarks/`.

```bash
cargo build -p pyx-cli -p pyx-runtime
./target/debug/pyx run examples/hello.py
```

## Gate vor jedem Commit

```bash
./scripts/verify.sh
```

Das Skript prüft `cargo fmt`, Clippy mit `-D warnings`, Build, alle Tests,
JIT-/AOT-E2E-Beispiele, negative Diagnosen und GC-Stress. Dieselben Schritte
laufen in der CI (`.github/workflows/ci.yml`). Ein Commit sollte nur gemerged
werden, wenn das Gate grün ist; neue Features brauchen Tests und – bei
Verhaltensänderungen – aktualisierte Dokumentation.

## Konventionen

- **Verträge zuerst:** IR- und Runtime-ABI-Änderungen erfordern ein Update von
  [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) und aller Nutzer.
- **Keine stillen Fehlcompile:** nicht unterstützte Syntax wird mit einer
  Diagnose abgelehnt, nicht ignoriert.
- **Kein Runtime-Code ohne Test:** neue Sprachelemente/Module brauchen Unit-
  und E2E-Abdeckung; Fehlerpfade gehören in `examples/errors/`.
- **Diagnosen:** Format `file:line:col: error: …`, stabiler Exit-Code,
  `--json` bleibt maschinenlesbar.
- **Commits:** kurze, imperative Betreffzeile (z. B. `runtime: fix dict repr`),
  ein Thema pro Commit; keine generierten Artefakte (`target/`, `.pyx/`,
  `__pycache__/`) einchecken.
- **Formatierung:** `cargo fmt`; keine manuellen Umformatierungen fremder
  Bereiche in Feature-Commits.

## Benchmarks

Nach Änderungen an Codegen/Runtime, die die Performance betreffen können:

```bash
cargo build --release -p pyx-cli -p pyx-runtime
python3 benchmarks/pyx/run_micro.py
python3 scripts/check_benchmarks.py
```

Schlägt ein Schwellenwert an, zuerst die Ursache prüfen. Nur bei beabsichtigter
Veränderung die Baseline mit `python3 scripts/check_benchmarks.py --update`
neu schreiben und die Änderung im Commit begründen. Die Harnesse unter
`benchmarks/bench_*.py` sind Referenzmessungen gegen CPython und werden nicht
an PyX angepasst.

## Lizenz

Beiträge stehen unter derselben Lizenz wie das Projekt: **MIT OR Apache-2.0**
(siehe [`LICENSE-MIT`](LICENSE-MIT) und [`LICENSE-APACHE`](LICENSE-APACHE)).
