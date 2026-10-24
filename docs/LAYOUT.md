# PyX — Quelltext-Layout

Stand 2026-10-24 (nach der Modul-Restrukturierung). Die Crates bleiben der
einzige Frontend↔Codegen-Vertrag; innerhalb der Crates sind die früher flachen
`src/*.rs`-Dateien in thematische Ordner aufgeteilt. Pfade sind relativ zum
Repository-Wurzelverzeichnis.

```text
.
├── AGENTS.md                  Arbeitsstatus, Phasen, bekannte Grenzen
├── PLAN.md                    Strategie/Leitentscheidungen
├── README.md                  Überblick, Quickstart, CLI, Sprachumfang
├── CHANGELOG.md               Keep-a-Changelog-Format
├── CONTRIBUTING.md            Setup, Gate, Commit-Regeln
├── Cargo.toml                 Workspace + gemeinsame Dependency-Versionen
├── scripts/
│   ├── verify.sh              vollständiges Gate (fmt/clippy/build/tests/E2E/AOT/GC)
│   └── check_benchmarks.py    lokales Benchmark-Regressionsgate
├── benchmarks/
│   ├── PYX.md                 Readiness-Matrix der externen Harnesses
│   └── pyx/                   Micro-Benchmarks, run_micro.py, results/, thresholds.json
├── examples/                  JIT-/AOT-Beispiele + expected/*.txt
├── docs/
│   ├── ARCHITECTURE.md        verbindlicher Vertrag (IR/ABI, GC, Module)
│   ├── TODO.md                einzige Arbeitsliste
│   ├── C_API.md               P4-04-Spike: Optionen, Umgebungsprobe, Blocker
│   └── LAYOUT.md              diese Datei
└── crates/
    ├── pyx-ir/                typisierte IR, einziger Frontend↔Codegen-Vertrag
    │   └── src/
    │       ├── lib.rs         Re-Exports + Vertrags-Signaturen
    │       ├── model.rs       Type, Const, Builtin, BinOp, Exception-Tags
    │       ├── ir.rs          Programm/Funktion/Stmt/Expr/Class/Closure
    │       ├── stdlib/        zentrale Modul-/Signaturtabelle
    │       │   ├── mod.rs
    │       │   └── specs.rs
    │       └── tests.rs
    ├── pyx-frontend/          Parsen, Lowering, Typprüfung, User-Module
    │   └── src/
    │       ├── lib.rs         check_file, Modul-Loader/Import-Auflösung
    │       ├── diag.rs        Diagnosen, Spans, Codes
    │       ├── lower/         AST → HIR/Lowering
    │       └── sema/          Typinferenz, Constraints, IR-Emission
    ├── pyx-codegen/           Cranelift-Backend
    │   └── src/
    │       ├── lib.rs
    │       ├── backend/       JIT- und AOT-Treiber
    │       └── lower/         IR → Cranelift (Frames, Cleanup-Stack, Safepoints)
    ├── pyx-runtime/           Wertmodell, ABI, GC, Module (rlib + staticlib)
    │   ├── src/
    │   │   ├── lib.rs
    │   │   ├── abi.rs         alle `extern "C"`-Symbole (docs/ARCHITECTURE §3)
    │   │   ├── core/          value, ops, container, iter, builtins, methods,
    │   │   │                  exc, format
    │   │   ├── gc/            Heap, Farbe/Barrieren, Safepoints, Sweep
    │   │   ├── modules/       math/io/json/os/... + argparse, collections,
    │   │   │                  re, socket, asyncio
    │   │   ├── concurrency/   Threads/Locks/Queue/Prozesse/Pool/spawn
    │   │   ├── support/       PYX1-Serialisierung (pickle/spawn/mp.Queue)
    │   │   └── csv.rs         CSV-Reader/-Writer
    │   └── tests/             Integrationstests je Feature (ABI-Ebene)
    └── pyx-cli/               `pyx`-Binary
        ├── src/
        │   ├── main.rs        ICE-Fang, Exit-Codes
        │   └── cli/
        │       ├── mod.rs     CliError, usage_text, Source-Laden
        │       ├── args.rs    gemeinsame Options-Parser
        │       ├── diag.rs    Snippet-/JSON-/Farb-Rendering
        │       ├── dispatch.rs Subcommand-Tabelle
        │       └── commands/  new, init, check, run, build, test, fmt,
        │                      lint, clean, doctor, help, uv, spawn, scan
        └── tests/             E2E, Closures, User-Module
```

## Zuordnungsregeln

- `pyx-ir` enthält **keine** Logik außer Typtabellen und Vertragsstrukturen.
  Änderungen an `ir.rs`/`model.rs` sind Vertragsänderungen.
- `pyx-frontend::stdlib` (Re-Export aus `pyx-ir`) ist die einzige Stelle, an der
  native Modul-Signaturen für Typprüfung und Runtime-Validierung stehen.
- Runtime-Code liegt ausschließlich in `pyx-runtime`; `pyx-codegen` ruft nur
  `pyx_runtime::abi`-Symbole auf.
- Tests liegen bei der jeweiligen Verantwortung: Unit-Tests inline, ABI-
  Integrationstests unter `crates/pyx-runtime/tests/`, Prozess-/CLI-E2E unter
  `crates/pyx-cli/tests/`.
- Neue CLI-Unterbefehle bekommen eine Datei unter `pyx-cli/src/cli/commands/`
  und einen Eintrag in `dispatch.rs`, `args.rs` und `usage_text()`.
