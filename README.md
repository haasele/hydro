# PyX

PyX ist eine eigenständige Python-Runtime in Rust mit JIT- und AOT-Compiler und
einer cargo-artigen Toolchain. Der Anspruch ist, normales Python ohne CPython
als Standardlaufzeit auszuführen; die Kompatibilität ist derzeit bewusst auf
eine klar definierte Teilmenge begrenzt (siehe
[Sprachumfang](#sprach--und-stdlib-umfang) und [`docs/TODO.md`](docs/TODO.md)).

> **Status: frühe Entwicklung (0.1.0).** Compiler/Runtime, Container, Klassen,
> Closures, Formatierung, Datei-I/O, User-Module, Stdlib-Kern und
> Concurrency-Primitive laufen. Echte lazy Generatoren/Coroutinen, eine
> vollständige CPython-Stdlib und C-Extensions fehlen weiterhin; PyX ist weder
> vollständig Python-kompatibel noch produktionsreif.

## Status

| Bereich | Stand |
|---|---|
| Phase 1 — Compiler, Runtime-ABI, JIT/AOT | fertig, per `scripts/verify.sh` verifiziert |
| Phase 2 — Runtime, GC, Module, Klassen, Concurrency | weitgehend fertig; Lazy-Generatoren/echte Async- und präzise GC-Roots offen |
| Phase 3 — Toolchain (`new`/`init`/`test`/`fmt`/`lint`/`clean`, Build-Cache, uv-Passthrough) | Kern vorhanden; uv-Interpreter-Injektion/Packaging offen |
| Phase 4 — Python-Kompatibilität, C-API/ABI | **blockiert** (P4-04: kein `python3-config`/`Python.h` in dieser Umgebung), siehe [`docs/C_API.md`](docs/C_API.md) |
| Phase 5 — Performance | Baseline + lokales Regressionsgate vorhanden |
| Phase 6 — Diagnostics/Developer Experience | weitgehend fertig; Completions/Manpage und ICE-Regressionstest offen |

Aktueller Arbeitsstand (Cargo-Version 0.1.0, noch ohne öffentliches Release):
**730 Tests** (Unit, Integration, E2E). JIT (`pyx run`) und AOT
(`pyx build`) teilen sich dieselbe Runtime und IR. Details:
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), Arbeitsliste in
[`docs/TODO.md`](docs/TODO.md), Quelltextbaum in
[`docs/LAYOUT.md`](docs/LAYOUT.md).

## Voraussetzungen

- Linux, primär x86_64 (aarch64-Pfade sind vorhanden, aber nicht
  CI-verifiziert); macOS/Windows werden noch nicht unterstützt.
- Rust **1.85+** (Edition 2021).
- Ein C-Compiler (`cc`), nur für `pyx build` (Linken des AOT-Binaries).
- `uv` nur für `pyx add/remove/sync/lock` (dünner Passthrough).
- Zur Laufzeit von PyX-Programmen wird **kein** CPython benötigt. Python 3 wird
  ausschließlich für die Benchmark-Harnesse und Prüfskripte unter `benchmarks/`
  bzw. `scripts/` verwendet.

## Quickstart

```bash
# CLI + Runtime bauen
cargo build --release -p pyx-cli -p pyx-runtime

# Neues Projekt anlegen
./target/release/pyx new hello
cd hello

# JIT: kompilieren und direkt ausführen
../target/release/pyx run main.py

# AOT: natives Binary erzeugen und ausführen
../target/release/pyx build main.py -o hello-bin
./hello-bin

# Vollständiges Gate (fmt, clippy, Tests, E2E, AOT, GC)
cd .. && ./scripts/verify.sh
```

`scripts/verify.sh` baut das Debug-Binary unter `target/debug/pyx`; die
Beispiele und erwarteten Ausgaben liegen in `examples/` bzw.
`examples/expected/`. Die CI führt denselben Ablauf aus (siehe
`.github/workflows/ci.yml`).

## CLI

```
pyx new <path> [--force]           Projektverzeichnis mit Scaffold anlegen
pyx init [path] [--force]          Projekt im aktuellen/vorgegebenen Verzeichnis anlegen
pyx check <file.py> [options]      nur parsen und typprüfen
pyx run <file.py> [options]        mit dem JIT kompilieren und ausführen
pyx build <file.py> [options]      natives Binary kompilieren und linken
pyx test [paths...] [options]      test_*.py / *_test.py mit `pyx run` ausführen
pyx fmt [paths...] [--check]       Trailing-Whitespace + finalen Newline normalisieren
pyx lint [paths...] [options]      alle .py-Dateien typprüfen
pyx clean [--dry-run]              .pyx/cache und target/pyx-tmp entfernen
pyx add <spec...> [uv options]     Abhängigkeiten mit uv hinzufügen
pyx remove <spec...> [uv options]  Abhängigkeiten mit uv entfernen
pyx sync [uv options]              Umgebung mit uv synchronisieren
pyx lock [uv options]              Lockfile mit uv aktualisieren
pyx doctor [--json]                Toolchain-/Umgebungsstatus melden
pyx help | --help | -h             diese Hilfe
pyx --version | -V                 Version
```

- Diagnose-Optionen (`check`, `run`, `build`, `test`, `lint`): `--json`,
  `--color=<auto|always|never>`, `--no-color`.
- `build`: `-o/--output <OUT>` (Default: Dateistamm im aktuellen Verzeichnis),
  `--runtime-lib <PATH>`, `--no-cache`.
- `test`: Erfolg = Exit-Code 0 des Programms (keine pytest-Assertions);
  `--json` liefert eine Ergebnisliste. `fmt --check` schreibt nicht.
- `clean` entfernt nur `.pyx/cache` und `target/pyx-tmp`, nicht den Rest von
  `target/`.
- `new`/`init` erzeugen `main.py`, `pyproject.toml` und `.gitignore`;
  existierende nicht-leere Dateien werden ohne `--force` nicht überschrieben.
- `add`/`remove`/`sync`/`lock` reichen alle Argumente unverändert an `uv`
  weiter und geben dessen Exitstatus zurück (`PYX_UV` wählt das Executable).
- Exit-Codes: `0` Erfolg, `1` Nutzer-/Programmfehler, `2` interner/Setup-Fehler.
- Umgebung: `CC` (Linker für AOT, Default `cc`), `PYX_RUNTIME_LIB`, `PYX_UV`,
  `NO_COLOR`; zur Laufzeit zusätzlich `PYX_GC_THRESHOLD`, `PYX_GC_STRESS=1`
  und `PYX_GC_STATS=1`.

`--` beendet die Optionsauswertung; zusätzliche Programmargumente werden von
`run` derzeit noch nicht unterstützt.

## Sprach- und Stdlib-Umfang

**Werte und Operatoren**

- `int` (i64, Überlauf → `OverflowError`), `float`, `bool`, `None`, `str`
  (UTF-8, Unicode-Skalare), `bytes`; `list`, `tuple`, `dict`
  (Einfügereihenfolge), `set`, `range`, Iteratoren, Funktions-/Klassenwerte.
- Arithmetik inkl. `//`/`%`/`**`, Vergleiche, `and`/`or` (Short-Circuit),
  `if`/`elif`/`else`, `while`, `for`, `break`, `continue`, Slices, Indexing
  inkl. negativer Indizes, `in`/`not in`.

**Funktionen, Klassen, Closures**

- `def`, Parameter, Default-Return, Rekursion; Funktionen und Lambdas sind
  Werte, Builtins ebenfalls (`f = int`); `map`/`filter` materialisieren Listen.
- **Closures by value:** verschachtelte `def` erfassen die Werte der äußeren
  Locals zum Definitionszeitpunkt. Rekursive verschachtelte Funktionen sind
  eine Diagnose.
- **Klassen:** `class Name(Base)` mit einer Basis, Methoden, `__init__`,
  `self`-Attributen (`pyx_getattr`/`pyx_setattr`) und Vererbung/Overrides.
  Keine Class-Attribute, Decorators, Metaclasses oder Multiple Inheritance.
- Comprehensions: Listen/Sets/Dicts und Generatorausdrücke (letztere **eager
  als Liste**), mehrere `for`-Klauseln, `if`-Filter.

**Formatierung und Attribute**

- f-Strings mit Format-Specs (`:.2f`, `:04d`, `:#x`, Ausrichtung/Füllzeichen)
  sowie `!s`/`!r`/`!a`; `str.format(...)`; printf-Formatierung `"%s:%d" % (...)`;
  zentrale Spec-Implementierung über `pyx_format_value`.
- Datenattribute statt Methoden bei nativen Objekten: `path.name`,
  `file.closed`, `thread.name`, `process.pid` usw.; Methoden-Keywords wie
  `Path.mkdir(parents=True, exist_ok=True)`.

**Kontrollfluss, Exceptions, Kontext**

- `raise`, `try`/`except [as]`/`else`/`finally`, Re-Raise, Python-artige
  Tracebacks; unbehandelte Exceptions beenden mit Exit-Code 1.
- `with a as x, b as y:` über das `__enter__`/`__exit__`-Protokoll (Dateien,
  `Pool`, User-Klassen); Rückgabewerte von `__exit__` unterdrücken keine
  Exceptions. `with`-Items werden in umgekehrter Reihenfolge verlassen.
- Augmentierte Subscript-Zuweisung (`a[i] += x`) wertet Objekt und Index genau
  einmal aus.

**Generatoren und async (eager)**

- `yield value` in einer Funktion materialisiert eine Liste (eager); es gibt
  **keine** lazy Generatorobjekte, `send`/`throw` oder `yield from`.
- `async def`/`await` laufen sequenziell; `await expr` liefert `expr`. Es gibt
  **keine** Coroutinen, Tasks oder Event-Loop. `asyncio.run/sleep/gather`
  sind eine dokumentierte Kompatibilitätsschicht (`sleep` blockiert,
  `gather` sammelt sequenziell).

**Module und Imports**

- Native Module (siehe unten) und **User-Module**: `import foo` /
  `from foo import f` lösen eine Geschwisterdatei `foo.py` auf; Funktionen
  werden exportiert, der Loader cached pro kanonischem Pfad und erkennt
  Zyklen. Importierte Module dürfen keine Top-Level-Seiteneffekte haben
  (nur Imports, Funktionsdefinitionen, Docstring, `pass`). Pakete,
  Punkte/relative Imports und `import *` fehlen.
- `__name__` ist `"__main__"` in der Einstiegsdatei; ein Runtime-Modulobjekt
  (`sys.modules`) gibt es nicht.

**Mitgelieferte Module** (nativ, ohne CPython):

`math`, `functools`, `operator`, `time`, `sys`, `json` (`dumps`/`loads`,
`dump`/`load`), `os` (Basis, `remove`/`rename`, `fork`/`waitpid`/`_exit`),
`os.path` (`join`, `basename`, `dirname`, `exists`, `isfile`, `isdir`,
`abspath`, `splitext`, `split`), `io`/`open` (Text und Bytes), `csv`,
`argparse` (Teilmenge, `Namespace`), `collections.defaultdict`, `re`
(Rust-`regex`: `search`/`match`/`fullmatch`/`findall`/`sub`/`split`,
`Pattern`/`Match`), `socket` (blockierendes TCP), `random`, `platform`,
`statistics`, `pathlib`, `_thread`, `threading`, `queue`, `multiprocessing`,
`pickle`, `asyncio` (nur `run`/`sleep`/`gather`).

**Concurrency und Speicher**

- Kein GIL; Threads teilen sich einen Heap mit konkurrierendem Mark-&-Sweep-GC
  (kurze Stop-the-World-Fenster für Root-Snapshot/-Rescan).
- `threading` (`Thread`, `Lock`, `RLock`, `Event`, `queue.Queue`),
  `multiprocessing` (`Process`, `Queue`, `Pool`, fork und spawn, Context-
  Manager für `Pool`), `pickle` mit eigenem, **nicht CPython-kompatiblem**
  PYX1-Format.
- Gemeinsam genutzte Container müssen von Nutzer-Locks geschützt werden;
  ungeschützte Mutationen sind Datenraces.

**Nicht unterstützt (Auswahl)**

Lazy-Generatoren/Coroutinen mit Event-Loop, `yield from`, echte
`asyncio`-Tasks, Decorators, Pattern Matching, Metaclasses, Descriptor-/
`__dunder__`-Protokolle, Multiple Inheritance, `bytearray`/`memoryview`,
BigInt, Slice-Assignment, Exception-Chaining, `except*`; beliebige
C-Extensions bzw. C-API/ABI, die vollständige CPython-Stdlib, Pakete/
relative Imports, Importe innerhalb von Funktionen sowie Top-Level-Code in
importierten Modulen. Präzise GC-Roots und ein eigener Größenklassen-Allokator
fehlen. Paketverwaltung ist nur ein uv-Passthrough (keine
Interpreter-Injektion/Packaging).

## Architektur

```
Quelltext .py (+ Geschwistermodule) → rustpython-parser → pyx-frontend
                                                          ↓
                                          typisierte IR (pyx-ir)
                                                          ↓
                     pyx-codegen (Cranelift) → JIT (pyx run)
                                             → Object + cc-Link (pyx build)
                                                          ↓
                                pyx-runtime (libpyx_runtime.a, C-ABI)
```

| Crate | Verantwortung |
|---|---|
| `crates/pyx-ir` | typisiertes HIR, Vertrag zwischen Frontend und Codegen, Stdlib-Specs |
| `crates/pyx-frontend` | Parsen, Modul-Loader/User-Module, Lowering, Typinferenz, Diagnosen |
| `crates/pyx-runtime` | Wertmodell, Operatoren, Builtins, Exceptions, GC, I/O, Module, Concurrency |
| `crates/pyx-codegen` | Cranelift-Codegen für JIT- und AOT-Pfad |
| `crates/pyx-cli` | `pyx`-Binary (Projekt, Compiler, Test/Fmt/Lint/Clean, uv) |

Intern sind die Crates in `model`/`ir`/`stdlib`, `lower`/`sema`,
`backend`/`lower`, `core`/`gc`/`modules`/`concurrency`/`support` und
`cli/commands` gegliedert; der vollständige Baum steht in
[`docs/LAYOUT.md`](docs/LAYOUT.md). Die ABI- und IR-Verträge sind in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) verbindlich dokumentiert.

## Tests und Benchmarks

- `./scripts/verify.sh`: `cargo fmt --check`, Clippy (`-D warnings`), Build,
  `cargo test --workspace` (730 Tests), E2E-Vergleich JIT vs. AOT,
  Negative-Tests (Diagnosen/Tracebacks) und GC-Stresstests.
- CI: [`.github/workflows/ci.yml`](.github/workflows/ci.yml) (Ubuntu, stable
  Rust, fmt/clippy/test/verify, keine Secrets).
- Benchmarks: [`benchmarks/PYX.md`](benchmarks/PYX.md) beschreibt Readiness und
  Integration der CPython-Harnesse; `benchmarks/pyx/run_micro.py` misst
  CPython vs. PyX-JIT vs. PyX-AOT (inkl. Thread-/Prozess-/Pool-Scaling) und
  schreibt `benchmarks/pyx/results/latest.json`.
- Regressionsschwellen: `benchmarks/pyx/thresholds.json` prüfen die JIT-/AOT-
  Medians der Kern-Workloads:

  ```bash
  python3 benchmarks/pyx/run_micro.py
  python3 scripts/check_benchmarks.py            # Exit 1 bei Überschreitung
  python3 scripts/check_benchmarks.py --update   # Baseline neu schreiben
  ```

  Das Gate vergleicht nur Workloads, die in Ergebnis- und Schwellenwertdatei
  vorhanden sind. Es ist bewusst nicht Teil der CI: Timing auf geteilten
  Runnern ist zu starken Schwankungen unterworfen.

## Mitwirken

Setup, Gate und Commit-Regeln: [`CONTRIBUTING.md`](CONTRIBUTING.md). Vor
größeren Änderungen lohnt ein Blick in [`AGENTS.md`](AGENTS.md) und
[`docs/TODO.md`](docs/TODO.md) (dort steht der aktuelle Arbeitsstand).

## Lizenz

Lizenziert unter **MIT OR Apache-2.0** (siehe `Cargo.toml`). Die
vollständigen Texte liegen in [`LICENSE-MIT`](LICENSE-MIT) und
[`LICENSE-APACHE`](LICENSE-APACHE); du darfst die Bedingungen einer der beiden
Lizenzen wählen.
