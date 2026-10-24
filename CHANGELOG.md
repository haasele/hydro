# Changelog

Alle nennenswerten Änderungen an PyX. Das Format orientiert sich an
[Keep a Changelog](https://keepachangelog.com/de/1.1.0/), die Versionierung an
[SemVer](https://semver.org/lang/de/). Solange kein öffentliches Release
existiert, sammelt `[Unreleased]` den aktuellen Stand; `0.1.0` ist der
Ausgangsstand des ersten lauffähigen Kerns.

## [Unreleased]

Stand 2026-10-24: **730 Tests**, `scripts/verify.sh` ALL GREEN. C-API bleibt
blockiert (siehe `docs/C_API.md`).

### Added

- **Formatierung:** f-String-Format-Specs (`:.2f`, `:04d`, `:#x`, Ausrichtung/
  Füllzeichen/Vorzeichen/Gruppierung) sowie `!s`/`!r`/`!a`; `str.format(...)`;
  printf-Formatierung (`"%s:%d" % (...)`); gemeinsame Spec-Implementierung und
  ABI `pyx_format_value`.
- **Attribute & Methoden-Keywords:** `pyx_getattr`/`pyx_setattr` und
  Datenattribute für `pathlib.Path` (`name`, `stem`, `suffix`, `parent`,
  `path`), Dateien (`name`, `mode`, `closed`, `encoding`), `Thread` und
  `Process`; `pyx_method_kw` für native Methoden (z. B.
  `Path.mkdir(parents=True)`).
- **Builtin-Funktionswerte:** Builtins sind als Werte verwendbar (`f = int`,
  `map(f, xs)`).
- **Klassen:** `class Name(Base)` mit einer Basis, Methoden, `__init__`,
  Vererbung/Overrides, Instanzattributen und dynamischem Methodendispatch;
  Instanzen/Klassen haben deterministische Repräsentationen.
- **Closures:** verschachtelte `def` erfassen äußere Locals **by value**;
  ABI `pyx_closure_new`, Captures als führende Wrapper-Parameter.
- **Generatoren (eager):** `yield value` materialisiert eine Liste in der
  Funktion; Generator Expressions bleiben eager.
- **async (eager):** `async def`/`await`/`async with`/`async for` laufen
  sequenziell; `asyncio.run`/`sleep`/`gather` als Kompatibilitätsschicht.
- **`with`-Protokoll:** `__enter__`/`__exit__` für Dateien, `Pool` und
  User-Klassen, verschachtelte Items in umgekehrter Reihenfolge;
  augmentierte Subscript-Zuweisung wertet Ziel-Objekt/-Index einmal aus.
- **User-Module:** `import foo` / `from foo import f` über Geschwisterdateien,
  Modul-Cache pro kanonischem Pfad, Zyklus-Erkennung, `__name__` und
  AOT-Einbettung; importierte Module dürfen keine Top-Level-Seiteneffekte
  haben.
- **Stdlib:** `os.path`, `argparse` (Teilmenge, `Namespace`),
  `collections.defaultdict`, `re` (Rust-`regex`, `Pattern`/`Match`),
  `socket` (blockierendes TCP), `asyncio`-Kompatibilitätsschicht.
- **CLI/Toolchain:** `pyx new`, `init`, `test` (plain Programme, `--json`),
  `fmt` (`--check`), `lint` (`--json`), `clean` (`--dry-run`); `add`/`remove`/
  `sync`/`lock` als uv-Passthrough (`PYX_UV`).
- **Build-Cache:** `.pyx/cache` mit deterministischem Schlüssel über Quelltext,
  erreichbare Module, Ziel, Version und Runtime-Archiv; `--no-cache`,
  `pyx clean`.
- **Scaling-Benchmarks:** `pool_serial`, `pool_parallel`, `process_queue` im
  Micro-Harness und in der Baseline/Schwellenwertdatei.
- **Dokumentation:** `docs/C_API.md` (P4-04-Spike inkl. Umgebungsprobe und
  Blocker), `docs/LAYOUT.md` (Quelltextbaum), aktualisierte
  `docs/ARCHITECTURE.md`, `README.md`, `AGENTS.md`, `docs/TODO.md`.

### Changed

- **Quelltext-Restrukturierung:** `pyx-ir/{model,ir,stdlib}`,
  `pyx-frontend/{lower,sema}`, `pyx-codegen/{backend,lower}`,
  `pyx-runtime/{abi,core,gc,modules,concurrency,support,csv}`,
  `pyx-cli/{cli,cli/commands}`; keine Verhaltensänderung, Verträge unverändert.
- Teststand von 545 auf 721 Tests erhöht (u. a. Formatierung, Attribute,
  Klassen, Closures, stdlib-Module, Socket, User-Module, CLI/Build-Cache).
- AOT-Builds betten User-Module ein; der Cache-Schlüssel berücksichtigt die
  gesamte erreichbare Modulbaumstruktur.

### Known limitations

- Echte lazy Generatoren/Coroutinen und ein Event-Loop fehlen; `yield from`,
  `yield`-Ausdrücke und `return <wert>` im Generator sind Diagnosen.
- Klassen nur mit einer Basis und ohne Class-Attribute/Decorators/Metaclasses/
  Descriptors; Closures sind by-value (kein Mutable-Capture).
- User-Module nur als Geschwister-`.py` (keine Pakete/Punkte/relativen
  Imports), ohne Top-Level-Ausführung und ohne Runtime-Modulobjekte.
- `re` nutzt die Rust-`regex`-Syntax (keine Lookarounds/Backreferences);
  `socket` nur blockierendes TCP; `random` nicht CPython-reproduzierbar.
- GC konservativ, ohne präzise Roots/Stack-Maps oder Größenklassen-Allokator.
- C-API/C-Extensions (P4-04) blockiert: kein `python3-config`/`Python.h` in
  der aktuellen Umgebung; keine Messungen/Entscheidung.

## [0.1.0] - 2026-10-24

Ausgangsstand des ersten lauffähigen Entwicklungsstands (545 Tests): Compiler,
Runtime und CLI für eine definierte Python-Teilmenge, per `scripts/verify.sh`
abgesichert. Spätere Slices stehen unter `[Unreleased]`.

### Added

- **Compiler:** Python-Parser (`rustpython-parser`), AST-Lowering in eine
  typisierte IR, Typinferenz/Diagnosen im Format `file:line:col: error: …`.
- **Codegen:** Cranelift-basierter JIT-Pfad (`pyx run`) und AOT-Pfad
  (`pyx build`, Object-Datei + Link gegen `libpyx_runtime.a`).
- **Runtime:** getaggte i64-Werte, Strings/Floats/Exceptions, dynamische
  Operatoren, Truthiness, `str()`/Repr, Tracebacks mit Frame-/Zeilenbezug.
- **CLI:** `pyx check`, `pyx run`, `pyx build`, `pyx doctor`, `--json`,
  `--color`, definierte Exit-Codes (0/1/2).
- **Container:** `list`, `dict`, `tuple`, `set`, `range`, Iteratoren, Slices,
  negative Indizes, strukturelle Gleichheit, `len`/`in`.
- **Sprachfeatures:** Comprehensions (List/Set/Dict/Generatorausdruck),
  First-Class-Funktionen und Lambdas (ohne Closures), `map`/`filter`/`reduce`,
  `enumerate`/`zip`/`sum`, `try`/`except`/`else`/`finally`, Re-Raise,
  f-Strings (einfache Ausdrücke), `with` (close-Protokoll).
- **Module/Imports:** `math`, `functools`, `operator`, `time`, `sys`, `json`,
  `os` (Basis), `io`, `csv`, `random`, `platform`, `statistics`, `pathlib`.
- **Datei-I/O:** Text- und Bytes-Modus (`r/w/a/x`, `rb/wb/ab/xb`), CSV-Reader/
  -Writer, `json.dump`/`load`; `bytes`-Literal/-Konstruktor, Indexing, Slicing,
  Iteration, Kernmethoden, Hex-Serialisierung.
- **Speicherverwaltung:** konservativer Mark-&-Sweep-GC mit Shared Heap,
  Insertion-Barrieren, konkurrierender Markierung und konkurrierendem Sweep;
  `PYX_GC_THRESHOLD`, `PYX_GC_STRESS`, `PYX_GC_STATS`.
- **Concurrency (kein GIL):** `_thread`, `threading` (`Thread`, `Lock`,
  `RLock`, `Event`), `queue.Queue`, `multiprocessing` (`Process`, `Queue`,
  `Pool`, fork und spawn), `pickle` mit eigenem PYX1-Format.
- **Diagnostics (P6):** stabile Fehlercodes, Spans, Source-Snippets, Help/
  Hints, `--json`-Schema, `pyx doctor`, ICE-Handling mit Exit-Code 2.
- **Verifikation:** `scripts/verify.sh` (fmt, Clippy, Build, Tests, E2E für
  JIT und AOT, Negative-Tests, GC-Stress), Benchmark-Harness
  `benchmarks/pyx/run_micro.py` mit CPython-/JIT-/AOT-Baseline.
- **Dokumentation:** `docs/ARCHITECTURE.md`, `docs/TODO.md`, `AGENTS.md`,
  `benchmarks/PYX.md`, `PLAN.md`.
- **Lizenz:** MIT OR Apache-2.0.

### Known limitations

- Keine Klassen, Generatoren/`yield`, Closures, `async`/`await`, Decorators,
  Pattern Matching, BigInt oder C-API/ABI-Kompatibilität (Stand dieses
  Ausgangsschnitts; später unter `[Unreleased]` ergänzt).
- Nur eine Teilmenge der CPython-Stdlib; Imports nur auf Modulebene und nur
  für die eingebauten Module.
- `pickle` ist nicht CPython-kompatibel; Dict-Insertion ist linear, die
  Integer-Arithmetik auf i64 begrenzt.
- Nur Linux (x86_64/aarch64); Toolchain auf `uv`-Basis (Phase 3) fehlt noch.
