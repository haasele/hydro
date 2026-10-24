# PyX — Arbeitsplan & ToDos

Quelle: [PLAN.md](../PLAN.md). Dieses Dokument ist die **einzige** Arbeitsliste.
Status: `[ ]` offen · `[~]` in Arbeit · `[x]` erledigt & verifiziert · `[!]` blockiert (Grund dahinter).

Legende IDs: `P<Phase>-<Nr>`. Phasen sind Meilensteine, keine Releases.
Jede Phase gilt erst als abgeschlossen, wenn ihre Akzeptanzkriterien (unten) auf
einem frischen Checkout mit `./scripts/verify.sh` grün sind.

**Stand 2026-10-24:** 721 Tests, `scripts/verify.sh` ALL GREEN. Phase 1 ist
abgeschlossen; Phase 2 ist bis auf Lazy-Generatoren/echte Async- und
GC-Präzision weitgehend fertig. P4-04 ist in dieser Umgebung blockiert
(`python3-config`/`Python.h` fehlen; siehe `docs/C_API.md`).

## Leitentscheidungen (aus PLAN.md abgeleitet)

- D1: AOT zuerst, JIT später (nur nach Messung).
- D2: Eigene Runtime in Rust, CPython ist nicht die Standardlaufzeit.
- D3: Compiler-Backend und Paketverwaltung wiederverwenden statt neu bauen.
- D4: `uv` integrieren, nicht sofort forken; Fork nur bei architektonischem Blocker.
- D5: Go-artiger GC als Zielarchitektur, aber Korrektheit vor Performance.
- D6: C-API-/ABI-Kompatibilität früh prototypisieren; sie entscheidet über
  Ökosystem-Übernahme vs. „neue Sprache mit Python-Syntax“.
- D7: Python-Kompatibilität ist ein dauerhaftes, eigenständiges Ziel (CPython-Testsuite).

## P0 — Fundament (jetzt)

- [x] P0-01 Repository-Struktur: Cargo-Workspace mit `pyx-ir`, `pyx-frontend`,
      `pyx-runtime`, `pyx-codegen`, `pyx-cli`.
- [x] P0-02 Architekturentscheidungen dokumentieren (`docs/ARCHITECTURE.md`).
- [x] P0-03 IR als stabilen Vertrag zwischen Frontend und Codegen definieren.
- [x] P0-04 Runtime-C-ABI (Value-Repräsentation, Symbole, Fehlerprotokoll) fixieren.
- [x] P0-05 Resume-Protokoll `AGENTS.md` (Status, nächste Schritte, Kommandos).
- [x] P0-06 Beispielprogramme + E2E-Testgerüst.
- [x] P0-07 `scripts/verify.sh`: fmt-check, clippy, build, test, Beispiele.
- [x] P0-08 CI (GitHub Actions): Linux x64, fmt + clippy + test + e2e.
      `.github/workflows/ci.yml`: stable Rust, Cargo-Cache, read-only
      Permissions, Concurrency-Cancel, `scripts/verify.sh` als Vollgate.
- [~] P0-09 Benchmark-Harness (Hyperfine-Skript, Baseline CPython) anlegen.
      `benchmarks/pyx/run_micro.py` (CPython/JIT/AOT, RSS, Build-Zeit) +
      Baseline/Schwellen sind vorhanden; ein Hyperfine-Skript für die
      bestehenden `bench_*.py`-Harnesses fehlt weiterhin.
- [x] P0-10 Error-/Diagnostic-Konventionen (Span, Exit-Codes, stderr-Format).
      Stable Codes (`PYXnnnn`), Spans (`end_col`), Help/Hints, Snippets,
      `--json`-Schema und Exit-Codes 0/1/2 sind implementiert und getestet.
- [x] P0-11 Entscheidung Backend: Cranelift jetzt, LLVM hinter `--features llvm`
      evaluieren (siehe ARCHITECTURE.md §4).
- [x] P0-12 Lizenz, README, Versionierung (SemVer ab erstem lauffähigen Release).
      `README.md`, `CONTRIBUTING.md`, `CHANGELOG.md`, `LICENSE-MIT` +
      `LICENSE-APACHE` (passend zu `MIT OR Apache-2.0`); Version 0.1.0.
      Tag-/Release-Prozess bleibt Q-08.

**Akzeptanz P0:** `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`,
`scripts/verify.sh` grün; keine undokumentierten Verträge.

## P1 — Minimaler Compiler (abgeschlossen)

Ziel: Python-artige Syntax, Variablen, Funktionen, Kontrollfluss, Typinferenz,
Exceptions und native Ausgabe. Noch keine vollständige Python-Kompatibilität.

### Frontend & Sprachsemantik
- [x] P1-01 Python-Parser integrieren (`rustpython-parser`), Fehler mit Zeile/Spalte.
- [x] P1-02 AST → typed IR Lowering: Module, Funktionen, Scopes.
- [x] P1-03 Literale: `int`, `float`, `bool`, `str`, `None`.
- [x] P1-04 Variablen: Zuweisung, Augmented Assignment, Lesen; Scoping ohne Closures.
- [x] P1-05 Operatoren: `+ - * / // % **`, unäres `- + not`, Vergleichsoperatoren,
      `and`/`or` (short-circuit), `==`/`!=` inkl. Strings.
- [x] P1-06 Kontrollfluss: `if/elif/else`, `while`, `break`, `continue`, `pass`,
      `for i in range(...)` inkl. negativer Schrittweite.
- [x] P1-07 Funktionen: `def`, Parameter, `return`, Rekursion, Default-Return `None`.
- [x] P1-08 Builtins: `print`, `len`, `str`, `int`, `float`, `abs`, `range` (nur `for`).
- [x] P1-09 Exceptions: `raise`, `try/except [as]`, Verschachtelung, Re-Raise,
      Laufzeitfehler (Division durch 0, Typfehler, Overflow) als Python-Exceptions.
- [x] P1-10 Typinferenz: Unifikation über int/float/str/bool/None/Any; Widersprüche
      als Diagnose; `Any`-Fallback für dynamische Pfade.
- [x] P1-11 Diagnosen: `file:line:col: error: ...`, Exit-Code 1; keine Panics bei
      Nutzereingaben.
- [x] P1-12 Nicht unterstützte Konstrukte sauber ablehnen (kein stiller Fehlcompile).

### Runtime
- [x] P1-13 Value-Repräsentation (tagged i64) + Heap-Objekte (`Str`, `Float`, `Exception`).
- [x] P1-14 Operatoren dynamisch: int/float-Numerik, String-Konkatenation/-Vergleich.
- [x] P1-15 Truthiness, `str()`-Konvertierung, `print` mit `sep`/Zeilenumbruch.
- [x] P1-16 Exception-Protokoll: pending exception, Tags, Message, Uncaught-Traceback.
- [x] P1-17 Frame-/Zeilen-Tracking für Tracebacks (Funktionsname, Zeile).
- [x] P1-18 i64-Overflow-Verhalten explizit (OverflowError); Dokumentation der Grenze.

### Codegen
- [x] P1-19 Cranelift-IR-Erzeugung für alle IR-Statements/-Ausdrücke.
- [x] P1-20 JIT-Pfad (`pyx run`), Runtime-Symbole registriert.
- [x] P1-21 AOT-Pfad (`pyx build`): Object-Datei + Link gegen `libpyx_runtime.a`.
- [x] P1-22 Exception-Checks nach jeder fehlerfähigen Operation; Handler-Routing.
- [x] P1-23 Line-Table/Frames für Tracebacks; Generator-Main als Einstiegspunkt.
- [x] P1-24 Deterministische Fehlerausgabe bei Codegen-Fehlern.

### CLI & Toolchain-Basis
- [x] P1-25 `pyx check <file>`, `pyx run <file>`, `pyx build <file> -o <out>`.
- [x] P1-26 Exit-Codes: 0 ok, 1 Nutzer-/Programmfehler, 2 interner Fehler.
- [x] P1-27 `pyx run` gibt Programm-Exitcode weiter.

### Verifikation P1
- [x] P1-28 E2E-Tests: hello, fib, loops, strings, exceptions, recursion.
- [x] P1-29 Negative Tests: Syntaxfehler, Typfehler, unbehandelte Exception (Traceback).
- [x] P1-30 AOT-Binary unabhängig vom Quelltext ausführbar (Smoke in tmpdir).
- [x] P1-31 `cargo test` + `scripts/verify.sh` grün.

**Akzeptanz P1:** `pyx run examples/fib.py` und `pyx build examples/fib.py -o /tmp/fib`
liefern identische, korrekte Ausgabe; Fehlerfälle liefern Python-artige Meldungen;
kein CPython wird zur Laufzeit benötigt.

## P2 — Runtime

- [x] P2-01 Objektmodell erweitern: `list`, `tuple`, `dict`, `set`, `bytes`.
      `bytes` (Literale, Konstruktor, Index/Slice/Iteration, Ops, Hash,
      Kernmethoden, Serializer) und Binaerdateien fertig;
      `bytearray`/`memoryview` bleiben bewusst offen.
- [x] P2-02 String-Semantik: Unicode, Indexing/Slicing, Methoden, Formatierung,
      f-Strings, `%`-Formatierung.
      Format-Specs (`:04d`, `:.2f`, `:#x`, Ausrichtung/Füllzeichen), `!s`/`!r`/
      `!a`, `str.format` und `%`-Format sind über `pyx_format_value` bzw.
      `core/format.rs` implementiert und getestet.
- [~] P2-03 Iteratoren/Iterationsprotokoll; `for` über Container; `enumerate`, `zip`.
      File-/CSV-Reader- und Bytes-Iteration vorhanden; echte Generator-/
      Lazy-Iterator-Objekte und `dict`-Views offen.
- [x] P2-04 Module: Import-System, Builtin-Module, `__name__`, Modul-Cache.
      Native Module + User-Module (`import foo`, `from foo import f`) mit
      Cache pro kanonischem Pfad, Zyklus-Erkennung, `__name__` (`__main__` in
      der Einstiegsdatei) und AOT-Einbettung. Grenzen: nur Geschwister-`.py`,
      keine Pakete/Punkte/relativen Imports, keine Top-Level-Seiteneffekte in
      importierten Modulen, kein Runtime-Modulobjekt/`sys.modules`.
- [ ] P2-05 Allokator: Größenklassen, Arena/Chunk-Allokation, Alignment, Statistiken.
      Objekt-Registry + GC-Heap vorhanden; eigener Größenklassen-Allokator
      weiterhin offen.
- [~] P2-06 GC: präzise Roots (Shadow-Stack/Frames) vor konservativem Interim;
      Mark-&-Sweep korrekt, dann nebenläufig (Go-artig), Pausen messen.
      Konservativer, konkurrierender Mark-&-Sweep (Edge-Barrieren,
      konkurrierender Sweep, STW-Root-Snapshot/-Rescan) ist implementiert;
      präzise Roots/Handles offen.
- [~] P2-07 GC-Safepoints + Stack-Maps oder Shadow-Stack in Codegen verankern.
      Safepoints an Entry/Backedges (alle 64 Iterationen), konservativer
      Stack-Scan; präzise Stack-Maps/Shadow-Stack offen.
- [~] P2-08 GC-Metriken (`pyx gc --stats`), Heap-Profiling-Grundlage.
      `PYX_GC_STATS=1` (pause_ms/mark_ms/sweep_ms/freed), `pyx_gc_collect`,
      `pyx_gc_objects`; eigenes CLI-`gc --stats`/Heap-Profiling offen.
- [x] P2-09 Klassen/Objekte, Methoden, Vererbung, `__init__`, Attribute.
      Single-Base-Klassen mit Methoden, `__init__`, Vererbung/Overrides,
      `pyx_getattr`/`pyx_setattr`, dynamischem Dispatch und deterministischer
      Repräsentation. Grenzen: keine Class-Attribute, Decorators, Metaclasses,
      Multiple Inheritance oder Descriptor-Protokolle.
- [~] P2-10 Generatoren, `yield`, Closures/Zellvariablen.
      Closures (nested `def`, by value) und **eager** Generatoren (`yield value`
      materialisiert eine Liste) sind fertig. Echte lazy Generatorobjekte,
      `send`/`throw`, `yield from`, Mutable-Captures offen.
- [~] P2-11 Exceptions vollständig: `else`, `finally`, Exception-Hierarchie, Chaining.
      `OSError`/`IOError` zusätzlich; Exception-Hierarchie/Chaining und
      `except*` offen.
- [~] P2-12 `async`/`await`, Event-Loop-Grundlage.
      Eagerer Modus implementiert: `async def` läuft sequenziell, `await` ist
      der Operand, `async with`/`async for` nutzen das synchrone Protokoll;
      `asyncio.run/sleep/gather` als Kompatibilitätsschicht. **Echte**
      Coroutinen/Tasks/Event-Loop offen.
- [~] P2-13 Standardbibliothek-Kern in Rust: `os`, `pathlib`, `json`, `re`, `math`,
      `time`, `random`; Rest über Kompatibilitätsschicht.
      Vorhanden: `math`, `operator`, `functools.reduce`, `time`, `sys`, `json`
      (inkl. dump/load), `os`-Basis + remove/rename/fork/waitpid/_exit,
      `os.path`, `io` (Text+Bytes), `csv`, `random`, `platform`, `statistics`,
      `pathlib.Path`, `argparse` (Teilmenge), `collections.defaultdict`,
      `re` (Rust-`regex`), `socket` (TCP), `asyncio`-Kompatibilität.
      Breite CPython-Stdlib, TLS/UDP, vollständiges `argparse` und
      pytest-Kompatibilität offen.
- [ ] P2-14 Interpreter/Bytecode-Modus für REPL und schnelle Dev-Loops (optional).
- [x] P2-15 Threading/GIL-Entscheidung dokumentieren und implementieren.
      Entscheidung: **kein GIL**, Shared Heap + konkurrierender GC
      (ARCHITECTURE §7.1); Threads/Locks/Queue/Prozesse implementiert.
- [ ] P2-20 Concurrency: echtes Multithreading & Multiprocessing
      (GIL-frei angestrebt, aber Korrektheit zuerst).
      - [x] P2-20a Shared-Heap/GC-Design fuer Threads (Heap aktuell thread-lokal;
        Cross-Thread-Werte, Synchronisation, Safepoints) oder bewusste GIL.
        Shared Heap + konkurrierender Mark-&-Sweep (STW-Root-Snapshot und
        -Rescan, konkurrierende Markierung mit Insertion-Barrieren,
        White-Partition unter Heap-Lock, Freigabe ausserhalb, wachstumsbasierte
        Schwelle, PYX_GC_STATS). Kein GIL; gemeinsam genutzte Container brauchen
        User-Locks. Praezise Roots/Allokator (P2-05/06) und feineres
        Safepoint-Polling bleiben offen.
      - [x] P2-20b `threading`-Teilmenge: Thread, Lock, RLock, Event, Queue;
        Thread-lokale Exception-/Frame-Zustaende, Thread-Spawn/Join.
        Thread/Lock/RLock/Event/queue.Queue implementiert; `_thread.join` bleibt
        PyX-Extension; blockierende waits parken GC-sicher.
      - [x] P2-20c Prozess-API: `multiprocessing` (Process, Queue, Pool),
        `os.fork`/spawn, waitpid, Exit-Codes; echte parallele Prozesse.
        Process/Queue/fork/waitpid/_exit, echtes spawn (exec,
        Funktionsregistry, Payload/Queue-FDs, kein Modulbody im Kind),
        Pool (fork+spawn, per-Worker-Queues, Context-Manager) und
        Process/Pool.terminate.
      - [x] P2-20d Serialisierung fuer Prozessgrenzen (pickle-kompatibel oder
        eigenes Format), inkl. Function-Referenzen.
        Eigenes PYX1-Format (`pickle` = Hex, mp.Queue = binaer;
        spawn = Funktion per Name, Queue-FDs). Nicht CPython-kompatibel.
      - [x] P2-20e Parallel-Benchmarks: Thread-Scaling, Prozess-Scaling,
        Speicher/Startkosten vs. CPython; CI-Schwellen.
        `threads_serial`/`threads_parallel`, `pool_serial`, `pool_parallel`,
        `pool_map`, `process_queue` sind im Micro-Harness, in
        `results/latest.{json,md}` und in `thresholds.json`. Die Schwellen
        laufen lokal (`scripts/check_benchmarks.py`), nicht in der CI
        (Runner-Rauschen; Q-06).
      - [~] P2-20f Race-/Stress-Tests, Fuzzing, Deadlock-/Shutdown-Verhalten.
        Concurrent-GC-/Thread-/Pool-Stresstests und Deadlock-Watchdogs
        vorhanden; Fuzzing (Q-03/Q-04) offen.
- [x] P2-17 Comprehensions: List/Set/Dict + Generator Expressions, mehrere
      `for`-Klauseln, `if`-Filter, Tupel-Targets, Shadowing ohne Leak.
      Generator Expressions werden eager als Liste materialisiert (dokumentiert).
- [x] P2-18 First-Class Functions: Funktionswerte, Lambdas (ohne Closures bei
      Lambdas), indirekte Calls, `map`/`filter` (materialisierte Listen);
      Builtin-Funktionswerte (`f = int`), Closure-, Default- und
      Star-Parameter-Diagnosen.
- [~] P2-16 Runtime-Unit-Tests + Property-Tests für Container/GC.
      Stand 2026-10-24: 721 Tests gesamt — 329 Runtime (160 Unit, 169 ABI-
      Integration: u. a. 22 Bytes, 19 IO, 14 Stdlib-Module, 14 Stdlib-Utils,
      12 Classes, 12 Pathlib, 9 Native-Concurrency, 8 CSV, 7 Socket, 7 Format,
      6 Attrs, 6 Closures, 6 GC, 5 Concurrent-GC, 5 Pool, 5 Threads, 5 OS-Path,
      3 STW-GC, 1 JSON-GC), 233 Frontend, 11 Codegen, 3 IR, 145 CLI/E2E.
      Property-Tests offen.

**Akzeptanz P2:** Speicher wächst nicht unbeschränkt in Loop-Benchmarks; GC-Tests
decken Zyklen ab; Container-Semantik gegen CPython-Verhalten getestet.

## P3 — Toolchain (uv)

- [x] P3-01 `pyx new`/`init`, `pyx add`, `pyx remove`, `pyx sync`, `pyx lock`.
      `new`/`init` erzeugen `main.py`, `pyproject.toml`, `.gitignore`;
      add/remove/sync/lock sind ein unveränderter Passthrough an `uv`
      (`PYX_UV`), inklusive Exitstatus.
- [~] P3-02 `pyproject.toml`/Lockfile-Verhalten mit `uv` kompatibel halten.
      Scaffold und Passthrough vorhanden; PyX interpretiert die Dateien nicht,
      uv bleibt Eigentümer. Eigene Lockfile-/Venv-Semantik offen.
- [~] P3-03 uv als Bibliothek/Prozess integrieren; Interpreter-Auswahl auf `pyx`
      umbiegen (`.python-version`, Venv-Marker, `sys.executable`-Äquivalent).
      Prozess-Passthrough implementiert; Interpreter-Injektion/Venv-Marker/
      `sys.executable`-Äquivalent offen.
- [x] P3-04 Build-Cache: Hash über Quelltext + Flags + Runtime-Version, inkrementell.
      `.pyx/cache`, deterministischer Schlüssel über Quelltext, erreichbare
      Module, Ziel, Version und Runtime-Archiv; `--no-cache`, `pyx clean`,
      Treffer-/Invalidierungstests.
- [ ] P3-05 Reproduzierbare Builds (deterministische Objekte, Pfad-Stripping).
- [x] P3-06 `pyx test`, `pyx fmt`, `pyx lint`, `pyx check`.
      Implementiert; `test` ist **nicht** pytest-kompatibel (Erfolg = Exit 0,
      `--json`), `fmt` normalisiert nur Whitespace/Final-Newline.
- [x] P3-07 uv-Fork nur falls Architektur es erzwingt (Entscheidungs-Doku).
      Entscheidung: kein Fork; Passthrough (ARCHITECTURE §8).
- [ ] P3-08 Packaging: native Binaries, Cross-Compilation, Installer.

**Akzeptanz P3:** Ein fremdes Python-Projekt mit `pyproject.toml` lässt sich mit
`pyx sync && pyx run` starten (definierter Support-Umfang). *Noch nicht
erfüllt*: ohne uv-Interpreter-Injektion/Packaging bleibt das ein Ziel.

## P4 — Python-Kompatibilität

- [ ] P4-01 CPython-Testsuite-Teilmenge als Harness; Fortschritts-Tracking.
- [ ] P4-02 Sprachfeatures vollständig: Decorators, Comprehensions, Pattern Matching,
      Descriptors, Metaclasses, `__dunder__`-Protokolle.
      Teilweise: Klassen (Single-Base), Comprehensions, Closures, `with`.
- [ ] P4-03 Standardbibliothek breit (pure Python + Rust-Ersatz).
      Kuratierter Kern vorhanden (P2-13); breite Abdeckung offen.
- [!] P4-04 C-API-Strategie-Spike: CPython einbetten (`libpython`) vs.
      Reimplementierung vs. begrenzte Kompatibilität; ABI3/NumPy-Smoke-Test.
      **Blockiert (2026-10-24):** In der aktuellen Umgebung fehlen
      `python3-config`, `Python.h` und eine linkbare `libpython3.12.so`/`.a`
      (nur die Laufzeit-SO `libpython3.12.so.1.0` ist vorhanden). Ohne
      Entwicklungs-Header ist weder Embedding noch ABI3-Shim kompilierbar.
      Optionen/Probe/Spike-Plan: `docs/C_API.md`. Es gibt **keine** Messungen
      und **keine** getroffene Embed-vs-Shim-Entscheidung.
- [ ] P4-05 Native Extension Loading (`.so`) + CPython-ABI-Shim. Hängt an P4-04.
- [ ] P4-06 `ctypes`, `cffi`-artige Grenzen definieren. Hängt an der P4-04-Option.
- [~] P4-07 Pickle/Serialisierung, Encoding-Details, Numerik-Grenzen (i64 vs BigInt).
      PYX1 ist bewusst nicht CPython-kompatibel; `bytes`/UTF-8/String-Escapes
      getestet; i64-Grenze dokumentiert. CPython-`pickle`-Kompatibilität offen.
- [ ] P4-08 Kompatibilitätsmatrix veröffentlichen (was läuft, was nicht).
      README/ARCHITECTURE dokumentieren den Umfang; eine Matrix je Modul/
      Feature fehlt.

**Akzeptanz P4:** definierter, getesteter Sprach-/Stdlib-Umfang; dokumentierte
C-API-Grenzen; keine stillen Semantikabweichungen.

## P5 — Performance & JIT

- [~] P5-01 Benchmarks vs. CPython/PyPy/Go (Startzeit, Throughput, Speicher).
      Readiness-Matrix `benchmarks/PYX.md`; PyX-Micro-Harness und Baseline
      (`benchmarks/pyx/results/latest.{json,md}`), inkl. Comprehensions,
      map/filter, Lambda-Calls, reduce, math-Loop, JSON dumps/loads und
      Thread-/Prozess-/Pool-Scaling. PyPy/Go-Vergleiche offen.
- [~] P5-01a PyX-Adapter: bestehende `bench_*.py` mit PyX als Implementierung
      ausführen (Subprozess/`PYX_BIN`), ohne die User-Harnesses zu verändern.
      Micro-Harness `benchmarks/pyx/run_micro.py` (CPython/JIT/AOT); Adapter
      für die großen Harnesses weiterhin offen.
- [~] P5-01b Benchmark-Readiness je Benchmark: loops (Comprehensions/Lambdas/
      Builtins), csv (Module/IO/with), startup (AOT/JIT-Subprozess),
      request (async/socket/json), fastapi (Framework/C-API).
      Matrix in `benchmarks/PYX.md`; loops/startup (JIT/AOT) messbar,
      csv teilweise, request/fastapi blockiert (C-API/async).
- [x] P5-01c CI-Regressionsschwellen + Baseline-Artefakte für PyX.
      Baseline-Artefakte + `benchmarks/pyx/thresholds.json` +
      `scripts/check_benchmarks.py` als lokales Gate (bewusst nicht in der CI,
      Runner-Rauschen). Schwellen-Enforcement in der CI bleibt Q-06.
- [x] P5-01e Parallel-Benchmarks (nach P2-20): Threads/Prozesse, Scaling,
      Startkosten, Speicher; Vergleich CPython/PyX.
      `threads_serial`, `threads_parallel`, `pool_serial`, `pool_parallel`,
      `pool_map`, `process_queue` (JIT/AOT/CPython, RSS/Startzeit).
- [ ] P5-02 PGO + LTO für AOT. (`lto = "thin"` ist im Profil gesetzt;
      PGO/LLVM-LTO-Messung offen.)
- [~] P5-03 Inlining, Constant Folding, Escape Analysis, Devirtualisierung.
      Safepoint-Checks an Loop-Backedges messbar; Optimierung offen.
- [~] P5-04 GC-Tuning anhand Messungen (Pausen, Allokationsrate).
      Wachstumsbasierte Schwelle, Relaxed-Safepoint-Fastpath,
      Edge-Barrieren, konkurrierender Sweep, PYX_GC_STATS. Präzise Roots/
      Allokator (P2-05/06) und Pausen-Tuning offen.
- [ ] P5-05 Profiling (CPU/Heap) + Trace-Integration.
- [ ] P5-06 JIT-Entscheidung **nur** mit Messdaten; sonst verwerfen.
- [ ] P5-07 Falls JIT: LLVM ORC oder Cranelift, Tiering, Deopt-Strategie.

**Akzeptanz P5:** messbare, dokumentierte Verbesserung ohne Korrektheitsregression.

## Querschnitt (parallel, dauerhaft)

- [ ] Q-01 Debugger-Integration (DAP), Source Maps/DWARF.
- [~] Q-02 Python-artige Tracebacks inkl. Chained Exceptions; stabile Fehlercodes.
      Tracebacks und stabile Codes vorhanden; Exception-Chaining offen.
- [ ] Q-03 Fuzzing: Parser, Lowering, Codegen (cargo-fuzz).
- [ ] Q-04 Fuzzing: Runtime/GC (Allokations-/Objekt-Operationen).
- [ ] Q-05 Security-Review `unsafe`-Code; Policy in ARCHITECTURE.md.
      `unsafe` ist in GC/ABI gekapselt und begründet; formales Review offen.
- [ ] Q-06 Performance-Regression-CI (Schwellwerte).
- [x] Q-07 Dokumentation: Sprachumfang, CLI, Architektur, Contribution.
      README/ARCHITECTURE/LAYOUT/C_API/TODO/AGENTS/CHANGELOG aktualisiert;
      CLI-Referenz/Diagnose-Katalog bleiben P6-09.
- [~] Q-08 Release-Prozess, Changelog, SemVer.
      Changelog (Unreleased/0.1.0) und SemVer-Schema dokumentiert;
      Tag-/Release-Prozess offen.
- [ ] Q-09 Cross-Plattform (macOS, Windows, aarch64) — nach Linux-Stabilität.
- [~] Q-10 Telemetrie-freie Defaults; Build-Reproduzierbarkeit.
      Keine Telemetrie; reproduzierbare Builds (P3-05) offen.

## P6 — Polishing & Developer Experience (cargo-like)

Ziel: `pyx` fühlt sich wie ein einziges, vollständiges Werkzeug an — nicht wie
eine Sammlung von Skripten. Fehler sind erstklassige Produkte, nicht Beiwerk.

- [~] P6-01 First-Class-Diagnostics: stabile Codes (`PYXnnnn`), Spans,
  Source-Snippets mit Caret, Help/Hints (did-you-mean), mehrere Fehler,
  Summary und konsistente Exit-Codes.
      Codes/Spans/Snippets/Help/Summary/Exit-Codes implementiert;
      sema-Carets sind teils 1 Zeichen, ICE-Pfad noch nicht getestet.
- [x] P6-02 Maschinenlesbare Diagnosen: `--json`/Message-Format mit stabilem
  Schema (file/line/col/end_col/code/message/help) — implementiert & getestet.
- [~] P6-03 Einheitliche CLI-UX: `new`, `init`, `check`, `run`, `build`, `test`,
  `fmt`, `lint`, `clean`, `doctor`, `add`, `remove`, `sync`, `lock` aus einer
  Binary; klare Hilfe, `--verbose`/`--quiet`, `--color`.
      Alle genannten Unterbefehle, Hilfe, `--color`/`--json` und definierte
      Exit-Codes sind implementiert. **Offen:** `--verbose`/`--quiet`.
- [~] P6-04 Build-Cache & inkrementelle Builds (`.pyx/cache`), reproduzierbar,
  Cache-Invalidierung über Quelltext + Flags + Runtime-Version.
      Cache inkl. Modulbaum-Fingerprint, `--no-cache`, `pyx clean` und
      Treffer-/Invalidierungstests implementiert. Reproduzierbare Binaries
      (deterministische Objekte/Pfad-Stripping) bleiben P3-05.
- [~] P6-05 ICE-Handling: interne Panics als `internal compiler error` mit
  Version/Target/Hinweis, Exit-Code 2, `RUST_BACKTRACE`-Hinweis; keine Panics
  bei Nutzereingaben (Fuzzing P6-08).
      `catch_unwind` + ICE-Banner; Regressionstest offen.
- [x] P6-06 Projekt-Scaffolding/Templates (`pyx new`, `pyx init`), damit ein
  Projekt ohne Fremdwerkzeuge startklar ist; `pyproject.toml`-Kompatibilität.
      Scaffold (`main.py`, `pyproject.toml`, `.gitignore`) mit `--force`-
      Politik; `pyproject.toml` bleibt uv-kompatibel.
- [ ] P6-07 Shell-Completions, Manpage, `--version --verbose` (Target, LLVM/
  Cranelift, Runtime-Version).
- [~] P6-08 Diagnostic-Regressionstests: Codes, Spans, JSON-Schema, Farben/
  TTY-Verhalten, Exit-Codes, ICE-Pfad.
      E2E deckt Human/JSON/Color/Doctor/Caret/Syntax-Code ab; ICE- und
      Detail-Span-Tests offen.
- [~] P6-09 Dokumentation: CLI-Referenz, Diagnose-Katalog, Fehlerbehebungsguide,
  Migration von CPython.
      README/ARCHITECTURE/LAYOUT/C_API/TODO aktualisiert; eigener
      CLI-Referenz-/Diagnose-Katalog offen.
- [ ] P6-10 Build-Feedback: Timing, `--verbose`, Fortschritt, Exit-Code-Summary.

**Akzeptanz P6:** Ein Nutzer kann ein Projekt mit `pyx` initialisieren, bauen,
starten und diagnostizieren, ohne andere Tools zu kennen; jede Fehlerklasse hat
Code, Span, Help und stabilen Exit-Code; `--json` ist dokumentiert und getestet.

## Risiken / offene Entscheidungen

- R1: C-API-/ABI-Kompatibilität ist der größte Aufwandstreiber (PLAN.md §3).
  P4-04 ist aktuell **umgebungsblockiert** (keine CPython-Dev-Header/
  `python3-config`); Ergebnis bestimmt Objektmodell-/Refcount-Design.
- R2: i64 statt BigInt — dokumentierte Grenze; BigInt später hinter Feature.
- R3: Cranelift statt LLVM: schnellerer Start, weniger Optimierung; LLVM optional.
- R4: GC mit nativen Frames braucht präzise Roots; konservativer Scan ist
  Interim (P2-06/P2-07).
- R5: `uv`-Interpreter-Kopplung kann Fork erzwingen; aktuell Passthrough,
  Entscheidung dokumentiert (kein Fork ohne Blocker).
- R6: Dynamische Features (monkey-patching, `eval`, Metaclasses) vs. AOT — Tiering
  oder expliziter Unterstützungsumfang nötig.
