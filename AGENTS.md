# AGENTS.md — PyX Arbeitsstatus

> **Resume-Regel:** Vor jeder Fortsetzung diese Datei + `docs/TODO.md` lesen.
> Nach jedem verifizierten Phasenschritt Status hier aktualisieren.
> Ein Schritt gilt nur als erledigt, wenn der Befehl/das Szenario tatsächlich
> ausgeführt wurde; ToDos in `docs/TODO.md` parallel abhaken.

## Projekt

PyX: eigenständige Python-Runtime (Rust) mit AOT-Compiler und Toolchain,
konzeptionell Bun für Python. Quelle der Wahrheit: `PLAN.md` (Strategie),
`docs/ARCHITECTURE.md` (Verträge), `docs/TODO.md` (Arbeitsliste),
`docs/C_API.md` (P4-04-Spike) und `docs/LAYOUT.md` (Quelltextbaum).

## Aktueller Stand

| Phase | Status | Verifikation |
|---|---|---|
| P0 Fundament | weitgehend fertig | CI/README/Lizenz/Changelog vorhanden; Hyperfine-Harness offen |
| P1 Minimaler Compiler | **fertig & verifiziert** | `scripts/verify.sh` ALL GREEN |
| P2 Runtime | **weitgehend fertig; Shared Heap + konkurrierender GC + Threads/Prozesse/Pool + Datei-I/O/bytes/Klassen/Closures/User-Module (kein GIL)** | 730 Tests, JIT+AOT |
| P3 Toolchain | **Kern vorhanden (new/init/test/fmt/lint/clean, Build-Cache, uv-Passthrough)** | E2E, Cache-Tests |
| P4 Kompatibilität | **blockiert (P4-04: kein `python3-config`/`Python.h`)** | `docs/C_API.md` |
| P5 Performance | **Baseline + lokales Regressionsgate** | `benchmarks/pyx/results/latest.md`, `thresholds.json` |
| P6 Polish | **weitgehend fertig; Diagnostics/JSON/Doctor/CLI** | Codes, Snippets, Help, ICE |

**Aktueller Fokus:** Phase-2-Abschluss und Dokumentation. Die Slices 2.23–2.30
sind umgesetzt: Formatierung, GetAttr/Methoden-Keywords/Builtin-Werte,
`os.path`/`argparse`/`defaultdict`, `re`/`socket`, Klassen, Closures +
eager Generatoren, eager Async + `with`-Protokoll, User-Module + CLI/Cache.
Offen/blockiert bleiben echte lazy Generatoren/Coroutinen mit Event-Loop,
variable Payload-Groessenklassen sowie praezise GC-Stack-Maps (P2-05/06/07),
Packaging/Cross-Platform/Fuzzing und der C-API-Spike (P4-04,
umgebungsblockiert). Details: `docs/TODO.md`.

**Verifikation (Stand 2026-10-24):**

- `cargo test --workspace`: **730 Tests** — 233 Frontend, 11 Codegen, 3 IR,
  165 Runtime-Unit, 145 CLI/E2E (134 E2E, 6 Closures, 5 User-Module),
  173 weitere Runtime-Integrationstests (Allocator, Bytes, GC/Concurrent-GC,
  IO, CSV, Stdlib-Module, Klassen, Closures, Socket, Pathlib, Pool, Threads).
- `scripts/verify.sh`: fmt, clippy (`-D warnings`), build, Tests, E2E für JIT
  und AOT (inkl. classes/closures/generators_async/formatting/socket_echo/
  stdlib_modules/pathlib), Negative-Tests, GC-Threshold/Stress — ALL GREEN.
- C-API-Umgebungsprobe: `python3` 3.12.3, aber kein `python3-config`,
  kein `Python.h`, keine linkbare `libpython3.12.so`/`.a`; P4-04 daher `[!]`.
  Es wurden keine Embedding-/ABI3-Messungen erfunden.
- Keine Code-/Cargo-/CI-Änderungen in diesem Docs-Slice.

**Phase-2.0-Slice (Container, 2026-10-23) — fertig & verifiziert:**

- `list`/`dict`-Literale, Indexing lesen/schreiben (negative Indizes),
  `IndexError`/`KeyError`, `len`, strukturelle Gleichheit, `list + list`,
  Python-Repräsentation (`[1, 'a']`, `{'k': 1}`), Truthiness.
- Iterations-ABI (`pyx_iter_new`/`pyx_iter_next`), `for` über Listen/Strings
  (Unicode-Skalare) und Dict-Keys in Einfügereihenfolge; `break`/`continue`,
  verschachtelte Schleifen.
- Neue Exception-Tags 6–8 (IndexError, KeyError, StopIteration) in IR/Runtime.
- `cargo test --workspace`: 79 Tests (54 Runtime, 7 Frontend, 2 IR, 16 E2E) grün.
- `scripts/verify.sh`: zusätzlich `collections.py`, `index_error.py` (Exit 1,
  Traceback), AOT-Collections identisch zu JIT — ALL GREEN.
- Differentialtest gegen CPython für Container-Repr, negative Indizes,
  Dict-Überschreiben/-Reihenfolge, Bool-/Int-Key-Äquivalenz, Unicode-Indexing,
  `and`/`or`-Operanden, strukturelle Gleichheit, Fehlermeldungen: identisch.
- Statische Diagnosen: nicht-iterierbarer Wert, falscher Index-Typ,
  unhashable Dict-Literal-Key, Slices.

**Phase-2.1-Slice (GC-Interim, 2026-10-23) — fertig & verifiziert:**

- Konservativer Mark-&-Sweep in `pyx-runtime/src/heap.rs`: Thread-lokale
  Registry, Mark-Bit, Roots = native Stack-Grenzen via `pthread_getattr_np` +
  asm-Stack-Loads, alle GPRs als Register-Dump, pending Exception, explizites
  Root für das frisch allokierte Objekt; Zyklen werden gesammelt.
- Schwellwert `PYX_GC_THRESHOLD` (Default 10000), `PYX_GC_STRESS=1` sammelt bei
  jeder Allokation; `pyx_gc_collect()`/`pyx_gc_objects()` als ABI;
  `pyx_runtime_init()` resetten Heap + Exception-State.
- `cargo test --workspace`: 89 Tests (55 Runtime-Unit, 5 GC-Integration,
  7 Frontend, 2 IR, 20 E2E) grün; `scripts/verify.sh` inkl. GC-Threshold/
  -Stress für JIT und AOT sowie Container-Roots — ALL GREEN.
- Drucktest (400k temporäre Strings): Peak-RSS 15.5 MB mit GC vs. 154 MB ohne
  Sammlung; Ausgabe identisch. Stress-Modus (`PYX_GC_STRESS=1`) für alle
  Beispiele korrekt.
- Grenze: konservativ (False-Positive-Retention, uninit-Stack-Caveat auf
  Nicht-x86/aarch64); präzise Shadow-Stack-Roots + nebenläufiger Collector
  bleiben P2-06/P2-07. Eigener Größenklassen-Allokator bleibt P2-05.

**Phase-2.2-Slice (`try/else/finally`, 2026-10-23) — fertig & verifiziert:**

- IR: `Try { body, handlers, else_body, finally_body }`, `Handler.exc: Option`
  (bare `except:`).
- Codegen: Cleanup-Stack mit Action-Codes (normal/return/break/continue/
  Exception), geparkte Exception fuer `finally`, korrekte Suppression bei
  return/break/continue im finally, Bare-`raise` re-raised die fliegende
  Exception (nicht nach behandeltem Fehler).
- `return` in try/handlern laeuft durch finally und fuehrt keinen Code nach dem
  try mehr aus (alter Bug behoben).
- Verifiziert: examples/finally.py + finally_reraise.py + return_after_try.py
  gegen CPython identisch; 100 Tests (25 E2E, 13 Frontend, 2 IR, 55 Runtime,
  5 GC); AOT + GC-Druck; `scripts/verify.sh` ALL GREEN.

**Phase-2.3-Slice (Slices + f-Strings, 2026-10-23) — fertig & verifiziert:**

- Lesende Slices fuer `str`/`list` mit voller `slice.indices`-Semantik
  (negative Bounds, Defaults, Unicode-Skalare, `step == 0` -> ValueError);
  Runtime-ABI `pyx_slice`, IR `ExprKind::Slice`.
- Einfache f-Strings (`{expr}`, `{expr!s}`, escaped braces) werden zu
  `str(expr)`-Konkatenation geloweret; Format-Specs/`!r`/`!a` liefern klare
  Diagnosen.
- Slice-Assignment und Dict-Slices werden statisch abgelehnt.
- Verifiziert: 124 Tests (32 E2E, 27 Frontend, 2 IR, 58 Runtime, 5 GC),
  CPython-Differential fuer Slice-/f-String-Kanten, GC-Threshold, AOT fuer
  slices/fstrings; `scripts/verify.sh` ALL GREEN.

**Phase-2.4-Slice (Methoden, 2026-10-23) — fertig & verifiziert:**

- IR `ExprKind::MethodCall`, ABI `pyx_method` (dynamischer Dispatch), neuer
  `AttributeError`-Tag (9).
- `str`: upper/lower/strip/lstrip/rstrip/split/join/replace/startswith/
  endswith/find/count/isdigit/isalpha/isalnum/isspace.
- `list`: append/extend/insert/remove/pop/clear/reverse/copy/count/index.
- `dict`: get/keys/values/pop/update/clear (keys/values liefern Listen, keine
  Views — dokumentierte Abweichung bis Tuple/Views existieren).
- Statische Methodenmatrix (Aritaet/Argumente/Ergebnistypen), `Any`-Annotation,
  dynamische `Any`-Empfaenger mit Runtime-Checks; unbekannte Methoden/
  Attributzugriff ohne Aufruf sind Diagnosen.
- Verifiziert: 158 Tests (38 E2E, 40 Frontend, 2 IR, 73 Runtime, 5 GC),
  CPython-Differential fuer Methodenkanten, GC-Threshold, AOT methods;
  `scripts/verify.sh` ALL GREEN. Arity-Fehlertexte sind Python-aehnlich, aber
  nicht immer byte-identisch zu CPython.

**Phase-2.5-Slice (Tupel, 2026-10-23) — fertig & verifiziert:**

- `Type::Tuple`, `ExprKind::Tuple`, `StmtKind::Unpack`, ABI `pyx_tuple_new`
  und `pyx_unpack`; Runtime-Objekt `Tuple(6)` inkl. GC-Tracing.
- Indexing/Slicing/`len`/Iteration/Repr (`(1,)`, `()`), strukturelle
  Gleichheit, `tuple + tuple`, `tuple * int`, Methoden `count`/`index`.
- Tupel aus hashbaren Elementen sind Dict-Keys; rekursive Hashbarkeitsprüfung
  meldet das innere unhashbare Element. `dict.items()` liefert eine Liste von
  `(key, value)`-Tupeln (View-Abweichung dokumentiert).
- Unpacking `a, b = pair()` und `for k, v in d.items()` (Frontend desugart
  ForEach + Unpack); Längenfehler als ValueError, Nicht-List/Tupel als
  TypeError. Nested/starred Unpacking statisch abgelehnt.
- Verifiziert: 198 Tests (44 E2E, 60 Frontend, 2 IR, 86 Runtime, 6 GC),
  CPython-Differential inkl. repr-Quote-Wahl, GC-Threshold, AOT tuples;
  `scripts/verify.sh` ALL GREEN.

**Phase-2.6-Slice (Set + Membership, 2026-10-23) — fertig & verifiziert:**

- `Type::Set`, `ExprKind::Set`, `BinOp::In`/`NotIn` (Codes 13/14),
  `Builtin::Set`; Runtime `Set(7)` mit ABI `pyx_set_new`/`pyx_set_from`.
- Set-Literale deduplizieren (Einfügereihenfolge), `len`/Iteration/Truthiness/
  strukturelle Gleichheit/`repr` (`{1, 2}`, leer `set()`), `set - set`.
  Hashbarkeit rekursiv (Tupel), unhashbare Elemente -> TypeError.
- Methods: `add`, `remove`, `discard`, `clear`, `copy`, `union`.
- Membership für list/tuple/dict/str/set; `not in`; Python-Fehlermeldungen für
  falsche Operanden; `in` auf nicht-iterierbaren Typen statisch abgelehnt.
- Fix: `len(x)` mit noch unaufgelöster Variable defaultete faelschlich auf
  `str`; jetzt bleibt die Constraint offen (`Any`), `len(1)` bleibt ein Fehler.
- Verifiziert: 231 Tests (51 E2E, 72 Frontend, 2 IR, 100 Runtime, 6 GC),
  CPython-Differential fuer Set-/Membership-Kanten, GC-Threshold, AOT sets;
  `scripts/verify.sh` ALL GREEN.

**P6.0-Slice (First-Class-Diagnostics, 2026-10-23) — fertig & verifiziert:**

- Diagnosen tragen stabile Codes (`PYX0001` Syntax, `PYX0102` Typ,
  `PYX0101` Name, `PYX0201` Methoden, `PYX0202` Index, ...), Spans
  (`end_col`), optionale Help-Hints (did-you-mean fuer Namen/Methoden) und
  werden von der CLI als Cargo-aehnliche Snippets gerendert:
  `error[CODE]`, `--> file:line:col`, Gutter, Caret, `= help:`.
- `--json` (stderr, stabiles Schema file/line/col/end_col/code/message/help),
  `--color=auto|always|never`/`--no-color` (TTY + NO_COLOR), Summary-Zeile,
  konsistente Exit-Codes; `check` bleibt bei Erfolg still.
- `pyx doctor [--json]` prueft Version/Target/Runtime-Lib/CC/Workspace;
  Exit 0 bei vorhandenem C-Compiler (JIT ohne Staticlib).
- ICE-Hygiene: `catch_unwind` um den Dispatch, Banner mit Version/Target/
  Report-Hinweis, Exit 2.
- Verifiziert: 256 Tests (60 E2E, 88 Frontend, 2 IR, 100 Runtime, 6 GC),
  Human-/JSON-/Color-Ausgabe, Doctor, Snippets; `scripts/verify.sh` ALL GREEN.
- Offen (P6): `new`/`init`/`test`/`fmt`/`lint`/`clean`, Cache (P6-04),
  ICE-Regressionstest, sema-Caret-Spans, CLI-Referenz/Diagnose-Katalog.

**Phase-2.7-Slice (Range + Benchmark-Builtins, 2026-10-23) — fertig & verifiziert:**

- `range(...)` ist ein Wert (Range-Objekt, Iteration/len/Membership/Repr);
  Builtins `list`, `dict`, `sum`, `enumerate`, `zip` ergänzt.
- `enumerate`/`zip` materialisieren Listen von Tupeln (benchmark-tauglich);
  `dict(zip(...))`, `list(range(...))`, `sum(range(...))` funktionieren.
- Range-Membership akzeptiert beliebige linke Typen wie CPython (False für
  nicht-integrale/andere Typen); Range ist nicht subskriptierbar (dokumentiert).
- Verifiziert: 290 Tests (67 E2E, 105 Frontend, 2 IR, 110 Runtime, 6 GC),
  CPython-Differential für Range-/Builtin-Kanten, GC-Threshold, AOT builtins;
  `scripts/verify.sh` ALL GREEN.
- Benchmark-Readiness analysiert; `benchmarks/PYX.md` + PyX-Micro-Harness
  (`benchmarks/pyx/run_micro.py`) + erste Baseline
  (`benchmarks/pyx/results/latest.{json,md}`); bestehende Harnesses bleiben
  unverändert.
- Baseline (Median, 5 Läufe, Release): Startup hello CPython 20.4 ms /
  PyX-JIT 2.9 ms / PyX-AOT 1.0 ms; while_sum 47.5 / 10.6 / 8.0 ms;
  for_sum 38.1 / 13.9 / 11.0 ms; range_sum 23.3 / 7.4 / 7.0 ms;
  list_append 38.1 / 16.3 / 13.5 ms; dict_insert 21.8 / 79.6 / 52.4 ms
  (linear, O(n²)-artig); string_concat 23.9 / 13.2 / 12.4 ms (O(n²)).
  Frühe Baseline, keine Optimierungsziele.

**Phase-2.8-Slice (Comprehensions, 2026-10-23) — fertig & verifiziert:**

- List/Set/Dict-Comprehensions und Generator Expressions; mehrere
  `for`-Klauseln, `if`-Filter, verschachtelte Comprehensions, Tupel-Targets
  (Unpack), Outer-Capture und Shadowing ohne Namens-Leak.
- Generator Expressions werden eager als Liste materialisiert (dokumentierte
  Abweichung bis echte Generatorobjekte existieren).
- Runtime-Helfer `pyx_list_append`/`pyx_set_add`/`pyx_dict_set`; Codegen baut
  Ergebnis-/Iterator-Roots GC-sicher.
- Verifiziert: 307 Tests (71 E2E, 115 Frontend, 2 IR, 113 Runtime, 6 GC),
  CPython-Differential (Order/Scope/Unpack/Nesting), GC-Threshold, AOT
  comprehensions; `scripts/verify.sh` ALL GREEN.
- Benchmark-Baseline erweitert: list_comp 27.8/7.4/5.9 ms, filter_comp
  25.2/9.7/7.5 ms, sum_genexp 27.5/9.8/6.7 ms (CPython/JIT/AOT).

**Phase-2.9-Slice (First-Class Functions, 2026-10-23) — fertig & verifiziert:**

- Benannte Funktionen und Lambdas sind Werte (`FunctionRef`); indirekte Calls
  laufen ueber `pyx_call`/argv-Wrapper (JIT + AOT). Lambdas sind parameter-only,
  ohne Closures, Defaults oder Star-Parameter (Diagnosen).
- `map`/`filter` materialisieren Listen; `filter(None, x)` nutzt Truthiness;
  Callback-Fehler/Arity werden propagiert.
- Function-Objekte sind normale Heap-Werte (Listen/Dicts/Iteratoren), GC-trace
  ohne Kindwerte; `pyx_function_new`/`pyx_call`/`pyx_map`/`pyx_filter`.
- Verifiziert: 333 Tests (79 E2E, 127 Frontend, 2 IR, 119 Runtime, 6 GC),
  CPython-Differential (Lambdas/Indirect-Calls/map/filter), GC-Threshold, AOT
  functions; `scripts/verify.sh` ALL GREEN.
- Benchmark-Baseline: map_lambda 30.7/13.0/13.4 ms, filter_lambda 28.6/13.9/
  12.9 ms, lambda_call 32.7/15.4/14.4 ms (CPython/JIT/AOT).

**Phase-2.10-Slice (Module/Imports, 2026-10-23) — fertig & verifiziert:**

- Zentrale Stdlib-Spezifikation `pyx_ir::stdlib` (math, functools, operator,
  time, sys) fuer Frontend-Typen und Runtime-Validierung.
- `import X [as Y]`, `from X import a [as b]`; Importe global sichtbar (auch in
  Funktionen), nur auf Modulebene; Modul-Attribute/Calls (`math.sqrt`,
  `time.perf_counter`, `sys.argv`) und native Function-Werte.
- Runtime: math (Trig/Log/Pow/Floor/Ceil/Trunc/ Konstanten),
  functools.reduce, operator (BinOps/Vergleiche/neg/not/truth), time
  (time/perf_counter/monotonic/sleep/ns), sys (argv/platform/version/
  version_info/maxsize).
- Verifiziert: 370 Tests (87 E2E, 145 Frontend, 2 IR, 130 Runtime, 6 GC),
  CPython-Differential (Module/Reduce/Math/Time/Sys), GC-Threshold, AOT
  modules; `scripts/verify.sh` ALL GREEN.
- Benchmark-Baseline: reduce_add 27.0/7.2/5.7 ms, math_loop 28.8/22.4/25.8 ms
  (CPython/JIT/AOT; AOT-PIC etwas langsamer bei Modul-Dispatch).

**Phase-2.11-Slice (json + os, 2026-10-23) — fertig & verifiziert:**

- `json.dumps`/`json.loads` mit CPython-Defaults (ensure_ascii, Separatoren,
  Escapes, Surrogate, Dict-Keys, Fehlerklassen, Depth-/Cycle-Guards); Parser
  baut Container GC-sicher inkrementell.
- `os`-Basis: getpid/getppid/cpu_count/getcwd/getenv, name/sep.
- Verifiziert: 386 Tests (91 E2E, 145 Frontend, 2 IR, 141 Runtime, 6 GC,
  1 JSON-GC), CPython-Differential (JSON-Kanten/os), GC-Threshold, AOT
  json_os; `scripts/verify.sh` ALL GREEN.
- Benchmark-Baseline: json_dumps 29.0/5.6/2.7 ms, json_loads 26.8/4.9/2.7 ms
  (CPython/JIT/AOT).

**Concurrency-Roadmap (P2-20, aufgenommen):** Zu beachten ist, dass Heap/GC
aktuell **thread-lokal** sind. Echtes Shared-Memory-Threading braucht zuerst
shared-heap/GC-Roots + Synchronisation oder eine bewusste GIL; Prozesse
(`multiprocessing`, fork/spawn, Queues/Pickling) sind der sichere erste Schritt.
P2-20a–f decken Design, threading, multiprocessing, Serialisierung,
Parallel-Benchmarks und Race-/Stress-Tests ab.

**Phase-2.12-Slice (Shared Heap + `_thread`, 2026-10-24) — fertig & verifiziert:**

- **Kein GIL**: globaler Shared-Heap-Modus mit Migration beim ersten Thread,
  Thread-Registry, `_thread.start_new_thread`/`get_ident`/`allocate_lock` und
  `_thread.join` (PyX-Extension); native Lock-Objekte mit
  acquire/release/locked.
- Waehrend >1 Thread aktiv ist, wird die konservative GC ausgesetzt (kein
  fremder Stack-Scan); nach Join wieder aktiv. STW-/Go-artiger Collector
  bleibt P2-20a. Gemeinsame mutierende Container brauchen User-Locks.
- JIT **und** AOT laufen mit echten Threads; Migration vor dem Spawn macht
  vorhandene Objekte sichtbar.
- Verifiziert: 392 Tests (93 E2E, 145 Frontend, 2 IR, 141 Runtime, 6 GC,
  1 JSON-GC, 4 Thread-Integration), inkl. 4×3000-String-Stress unter Lock,
  Shared-Heap-GC-Deferral, JIT/AOT; `scripts/verify.sh` ALL GREEN.
- Thread-Baseline: serial 130.2/32.9/32.1 ms, parallel 4×200k 73.0/23.3/17.8 ms
  (CPython/JIT/AOT). PyX AOT parallel ~1.8× schneller als PyX serial.

**Phase-2.13-Slice (STW-/Safepoint-GC, 2026-10-24) — fertig & verifiziert:**

- Kooperatives Stop-the-World fuer den Shared Heap: ThreadGc-Registrierung
  (Stack-Grenzen, SP/Register, parked/exited, Pending-Mirror, Thread-Roots),
  `pyx_safepoint()` an Funktions-Eingaengen und Loop-Backedges, Parking bei
  blockierenden Locks/join/sleep.
- Collector setzt Request, wartet auf alle anderen geparkten/exited Threads,
  scannt deren gespeicherte Stacks/Register + Roots + Pending, sweept; Heap-Lock
  nur nach dem Parken, GC_LOCK per try_lock. Kein GIL.
- Allocation ruft Safepoint, prueft Policy und sammelt VOR der Allokation;
  Spawn-Argumente sind als Thread-Roots registriert (Rooting-Luecke behoben).
- Verifiziert: 398 Tests (95 E2E, 145 Frontend, 2 IR, 141 Runtime, 6 GC,
  1 JSON-GC, 3 STW-GC, 5 Thread-Integration), 4×500-String-Stress unter
  Threshold=1, GC waehrend Threads/Sleep/Join, JIT+AOT, Gate gruen.
- Safepoint-Overhead: threads_serial AOT 39.6 ms vs. 32.1 ms ohne Safepoints;
  threads_parallel AOT 14.6 ms. Go-artiger nebenlaeufiger Collector (Write-
  Barriers) bleibt P2-20a; `threading`-Klassen/P2-20b und `multiprocessing`/
  P2-20c offen.

**Phase-2.14-Slice (threading/queue/multiprocessing/pickle, 2026-10-24) — fertig & verifiziert:**

- Native Objekte: `threading.Thread` (start/join/is_alive), `Lock`, `RLock`
  (rekursiv, Owner/Dropth), `Event` (set/clear/is_set/wait(timeout)),
  `queue.Queue` (put/get inkl. blockierend/Timeout/nowait/qsize/empty/full),
  `multiprocessing.Process` (fork/start/join/is_alive/exitcode),
  `multiprocessing.Queue` (socketpair, 4-Byte-Laengenrahmen), `os.fork`,
  `os.waitpid`, `os._exit`.
- Serialisierung `pickle.dumps/loads` als eigenes PYX1-Hexformat (None/Bool/
  Int/Float/Str/List/Tuple/Dict/Set/Range/Function-Wrapper); zyklusfrei,
  Depth 128, nicht CPython-kompatibel. `multiprocessing.Queue` nutzt dasselbe
  Binaerformat. Funktionen werden per Wrapper-Adresse rekonstruiert (nur
  gleicher bzw. geforkter Adressraum).
- Native Konstruktoren akzeptieren Keywords (`Thread(target=..., args=...)`)
  ueber `FunctionSpec.params`; fehlende Slots werden als Default-`None`
  uebergeben. User-Funktionen/Methoden/Builtins bleiben positional.
- Fork-Reset im Kind: Thread-Registry/GC/Join-Handles, Farben, Worklist;
  dokumentierte Grenze: Fork nur aus stabilem Zustand (Locks verschwundener
  Threads bleiben sonst belegt). Echtes `spawn` und `Pool` folgen in Phase 2.16.
- Verifiziert: 422 Tests (101 E2E, 149 Frontend, 2 IR, 144 Runtime-Unit,
  3 Concurrent-GC, 6 GC, 1 JSON-GC, 8 Native-Concurrency, 3 STW-GC,
  5 Thread-Integration), CPython-Gegenproben fuer Threading/Queue/Fork,
  Pickle eigenes Format, JIT+AOT; `scripts/verify.sh` ALL GREEN.

**Phase-2.15-Slice (konkurrierender Mark-&-Sweep, 2026-10-24) — fertig & verifiziert:**

- `PyxObj.marked` ist eine Atomfarbe (WHITE/GREY/BLACK). Der Collector hat
  zwei kurze STW-Fenster (Root-Snapshot am Anfang, Root-Rescan am Ende; der
  Sweep wurde in Phase 2.17 konkurrierend gemacht); dazwischen markiert er
  konkurrierend, waehrend Mutatoren laufen.
- **Insertion-Barrieren:** jede neu eingefuegte Kante (list append/extend/
  insert/subscript_set, dict set/update/subscript, set add, queue put,
  Builder/JSON/Pickle-Decode) markiert das neue Kind, wenn der Parent bereits
  BLACK ist. Kein Parent-Re-Trace mehr; das war zuvor O(n^2) bei grossen
  Listen.
- **Graph-Lock:** Container-Mutationen und Marker serialisieren pro Payload;
  im Single-Threaded-/Local-Modus wird der Lock komplett uebersprungen.
  Queue-/Event-/Lock-Mutexe bleiben unveraendert; blockierende Operationen
  parken nur waehrend des Wartens, nie waehrend einer Graphmutation.
- **Wachstums-Schwelle:** Sammlung bei `live_nach_letzter_Sammlung +
  PYX_GC_THRESHOLD` statt absoluter Objektzahl; verhindert eine Vollmarkierung
  pro Allokation bei grossem Live-Set. `PYX_GC_STRESS=1` bleibt jede Allokation.
- Safepoint-Fastpath: Relaxed-Load, Recheck unter Registry-Lock; Loop-
  Safepoints pollen nur jede 64. Iteration (Funktionseintritt/AFK/Allokation
  weiterhin sofort). `PYX_GC_STATS=1` schreibt
  `pyx-gc: pause_ms=… mark_ms=… sweep_ms=… freed=…` auf stderr.
- Messung (Debug, 110k-Liste, 3 Mutatoren, Threshold 20k): Markierung
  17–44 ms pro Sammlung statt bis 8.5 s mit Parent-Re-Trace; STW-Pausen
  36–86 ms (überwiegend finaler Sweep/Stack-Rescan). Release-Baseline nach
  Polling: threads_serial AOT 37.5 ms, threads_parallel AOT 11.5 ms;
  list_append AOT 16.5 ms, dict_insert 51.2 ms, list_comp 6.5 ms.
- Verifiziert: `tests/concurrent_gc.rs` (3 Tests, inkl. 111k-Append-Bound und
  Threshold=1-Stress), alle bestehenden GC-/Thread-/E2E-/AOT-Pfade,
  422 Tests, `scripts/verify.sh` ALL GREEN.

**Phase-2.16-Slice (echtes spawn + Pool, 2026-10-24) — fertig & verifiziert:**

- **Spawn:** `multiprocessing.set_start_method("spawn")` startet einen frischen
  Prozess per `exec` (JIT: `pyx __spawn <kanonische Quelle>`, AOT: Binary
  erneut). `pyx_main` registriert alle Funktionen (Name, Wrapper, Arity) vor
  dem Modulbody und ruft `pyx_spawn_child()`; bei Payload `-1` laeuft der
  normale Body, sonst nur die Ziel-Funktion. **Kein Modulbody im Kind.**
- Payload `[name, args]` in 0600-Tempdatei (Env `PYX_SPAWN_PAYLOAD`), PYX1-
  Spawn-Serialisierung: Funktionen per Name/Arity, `multiprocessing.Queue`
  per geerbten FDs (CLOEXEC geloescht); native Objekte/Lambdas -> Fehler.
  Nested spawn, Function-by-name-Argumente und Queue-Transport JIT+AOT gruen.
- **Pool:** `multiprocessing.Pool([processes])`, `map`/`apply`/`close`/`join`;
  je Worker private Task-/Result-Queue (keine Frame-Interleaving), ein Task
  in flight. Tasks/Results immer spawn-serialisiert (Funktion per Name), damit
  fork- und spawn-Worker identisch laufen. Worker-Exception -> RuntimeError;
  liveness probe gegen tote Worker; prozessanzahl validiert/begrenzt.
- Native Worker-Entry `__pyx_pool_worker` in der Registry; fork-Worker via
  `fork`+Runtime-Reset, spawn-Worker via Payload/exec.
- Verifiziert: **436 Tests** (105 E2E inkl. spawn/pool/pool_spawn JIT+AOT und
  GC-Stress, 149 Frontend, 2 IR, 151 Runtime-Unit, 3 Concurrent-GC, 6 GC,
  1 JSON-GC, 8 Native-Concurrency, 3 STW-GC, 3 Pool, 5 Thread-Integration),
  Gate ALL GREEN. Grenzen: Funktionsregistry nur nach Namen (Duplikate/
  Lambdas), Queue-maxsize im Spawn-Payload verloren, Pool ohne Context-Manager,
  Prozess-/Pool-Scaling-Benchmarks offen.

**Phase-2.17-Slice (konkurrierender Sweep + terminate, 2026-10-24) — fertig & verifiziert:**

- **Konkurrierender Sweep:** Nach dem finalen STW-Root-Rescan werden Mutatoren
  freigegeben; White-Partition und Address-Rebuild laufen einmalig unter dem
  Heap-Lock, das Freigeben der Boxen (`Box::from_raw`) **ausserhalb** aller
  Locks. Neue Allokationen waehrend des Sweeps sind GREY und werden geschuetzt;
  Phase E setzt alle ueberlebenden Farben auf WHITE und `MARKING=false`.
  `pyx-gc` meldet jetzt `pause_ms`, `mark_ms`, `sweep_ms`, `freed` getrennt.
- **Terminate:** `Process.terminate()` (SIGTERM, idempotent, Join/Exitcode -15)
  und `Pool.terminate()` (Worker SIGTERM + reap, idempotent, kein Queue-Write).
- **Pool-Benchmark:** `benchmarks/pyx/pool_map.py` (4 Worker, 4×50k Summen)
  in den Micro-Harness aufgenommen. Release: CPython 59.2 ms, PyX JIT 9.9 ms,
  PyX AOT 6.5 ms.
- Messung 110k-Liste/3 Mutatoren/Threshold 20k: STW-Pause max. 42 ms
  (vorher 86 ms), Markierung 23–47 ms, Sweep 35–85 ms ausserhalb der Pause.
- Verifiziert: 440 Tests (105 E2E, 149 Frontend, 2 IR, 151 Runtime-Unit,
  5 Concurrent-GC, 6 GC, 1 JSON-GC, 9 Native-Concurrency, 4 Pool, 3 STW-GC,
  5 Thread-Integration), Gate ALL GREEN.

**Phase-2.18-Slice (Datei-I/O, `with`, CSV, JSON-Dateien, 2026-10-24) — fertig & verifiziert:**

- **File-Objekt:** `io.open`/`open` (Keyword-Slots file/mode/encoding/newline/
  errors), UTF-8-Textmodi `r/w/a/x`; `read([n])`, `readline`, `readlines`,
  `write`, `writelines`, `flush`, `tell`, `seek`, `closed`, `close`;
  Iteration über Zeilen; Fehler als `OSError` (neuer Tag 10), geschlossene
  Dateien als `ValueError`. `with` desugart zu verschachteltem
  `try/finally` mit `close()` in umgekehrter Reihenfolge (nur
  close-Protokoll, kein `__enter__`/`__exit__`; `as` nur einfacher Name).
- **CSV:** `csv.reader`/`writer`, `DictReader`/`DictWriter` (fieldnames),
  RFC-4180-nahes Quoting/embedded newlines, `writerow(s)`, `writeheader`,
  `
`; Reader materialisiert beim Konstruktor, DictReader fehlende Felder
  `""`, Extra-Spalten ignoriert. Kein Dialect/restkey.
- **JSON-Dateien:** `json.dump(obj,file)`/`json.load(file)` nutzen das
  vorhandene Dumps/Loads; Fehler/Unicode wie bisher.
- **os:** `os.remove`, `os.rename` (OSError), Beispiel-Cleanup.
- Verifiziert: **478 Tests** (107 E2E inkl. Datei-/CSV-/JSON-JIT+AOT,
  158 Frontend, 2 IR, 152 Runtime-Unit, 5 Concurrent-GC, 8 CSV, 6 GC,
  12 IO, 1 JSON-GC, 6 JSON-IO, 9 Native-Concurrency, 4 Pool, 3 STW-GC,
  5 Thread-Integration), CPython-Differential für files/csv_json, Gate ALL
  GREEN. Grenzen: kein `bytes`/Binärmodus, `newline`/`errors` ignoriert,
  CSV vollständig im Speicher, keine `pathlib`/`random`/`argparse`/async.

**Phase-2.19-Slice (`bytes`, 2026-10-24) — fertig & verifiziert:**

- Neue `Type::Bytes`/`Const::Bytes`/`Builtin::Bytes`, `KIND_BYTES=20` und
  `ObjData::Bytes(Vec<u8>)`; Bytes-Literale `b"..."` (Parser-Escapes),
  `bytes()`/`bytes(int)`/`bytes(iterable)`/`bytes(str, encoding)`.
- Indexing liefert Int 0..255 (negative Indizes), Slicing liefert Bytes
  (slice.indices, step), `len`, Iteration liefert Ints; `in` fuer
  Subsequenz/Int; `+`, `*`, Vergleichsoperatoren, Gleichheit/Hashbarkeit
  (Dict-/Set-Keys), Truthiness und CPython-nahes `b'...'`-Repr.
- Methoden: `decode([encoding[,errors]])`, `hex`, `count`, `find`,
  `startswith`, `endswith`, `replace`, `split`, `join`, `strip`/`lstrip`/
  `rstrip`, `upper`/`lower`; `str.encode([encoding[,errors]])`.
- PYX1-Pickle/Spawn/MpQueue unterstuetzt Bytes (TAG_BYTES); GC childless.
- Verifiziert: **515 Tests** (108 E2E inkl. bytes JIT+AOT, 169 Frontend,
  3 Codegen, 2 IR, 152 Runtime-Unit, 22 Bytes, 5 Concurrent-GC, 8 CSV,
  6 GC, 12 IO, 1 JSON-GC, 6 JSON-IO, 9 Native-Concurrency, 4 Pool,
  3 STW-GC, 5 Thread-Integration), CPython-Differential für bytes, Gate gruen.
- Grenzen: keine Bytes-Dateimodi (`rb/wb`), kein `bytearray`/`memoryview`,
  Methodensubset (kein `translate`/`format`/Regex), 64-MiB-Konstruktorlimit.

**Phase-2.20-Slice (Binärdateien, 2026-10-24) — fertig & verifiziert:**

- FileState unterstuetzt `rb/wb/ab/xb` (und Text-Aliase `rt/wt/at/xt`):
  `read`/`readline`/`readlines`/Iteration liefern `bytes`, `write`/
  `writelines` akzeptieren nur Bytes; Cursor/Tell/Seek in Bytes. Textmodus
  bleibt zeichenbasiert. `encoding`/`newline`/`errors` mit `b` und
  `+`-Modi werden abgelehnt; CSV/JSON verweigern Binaerdateien.
- Beispiel `examples/binary_files.py` ist CPython-identisch, JIT+AOT.
- Verifiziert: **522 Tests** (109 E2E inkl. binary_files, 169 Frontend,
  3 Codegen, 2 IR, 152 Runtime-Unit, 22 Bytes, 5 Concurrent-GC, 8 CSV,
  6 GC, 18 IO, 1 JSON-GC, 6 JSON-IO, 9 Native-Concurrency, 4 Pool,
  3 STW-GC, 5 Thread-Integration), Gate ALL GREEN.

**Phase-2.21-Slice (`random`/`platform`/`statistics`, 2026-10-24) — fertig & verifiziert:**

- `random`: globaler SplitMix64-PRNG + seedbares natives `Random(seed=...)`
  mit `seed`/`random`/`randint`/`randrange` (negative Schritte, leere
  Bereiche)/`choice` (list/tuple/str/bytes)/`uniform`/`shuffle` (in-place).
  Deterministisch pro Seed, aber **nicht CPython-reproduzierbar**
  (dokumentierte Abweichung).
- `platform`: `system`/`machine`/`release`/`version`/`node`/`platform`/
  `python_version` + `python_implementation = "PyX"`.
- `statistics`: `mean`/`fmean`/`median`/`pstdev`/`stdev` ueber numerische
  Sequenzen inkl. Fehlerpfade (leer/nicht numerisch, stdev n<2).
- Verifiziert: **533 Tests** (110 E2E inkl. stdlib_utils JIT+AOT, 169 Frontend,
  3 Codegen, 2 IR, 152 Runtime-Unit, 22 Bytes, 5 Concurrent-GC, 8 CSV,
  6 GC, 18 IO, 1 JSON-GC, 6 JSON-IO, 9 Native-Concurrency, 4 Pool,
  3 STW-GC, 10 Stdlib-Utils, 5 Thread-Integration), Gate ALL GREEN.

**Phase-2.22-Slice (`pathlib.Path`, 2026-10-24) — fertig & verifiziert:**

- Natives `pathlib.Path` (`Path()`, `cwd()`, `home()`) mit
  `exists`/`is_file`/`is_dir`, `read_text`/`write_text`, `read_bytes`/
  `write_bytes`, `open` (Text/Binary), `mkdir(parents, exist_ok)`,
  sortiertem `iterdir`/`glob` (`*`/`?`, kein `**`), `name`/`stem`/`suffix`/
  `parent`/`joinpath`/`resolve`/`absolute`/`as_posix`; `/`-Operator joinet
  Path/str/Path. `os.remove`/`rename` akzeptieren Path.
- Wegen fehlender Attributwerte sind die Metadaten **damals** Methoden
  (`p.name()` statt `p.name`); Methoden-Keywords waren noch nicht unterstuetzt.
  Seit Phase 2.24 sind Datenattribute (`p.name`) und Methoden-Keywords
  (`mkdir(parents=True)`) verfügbar.
- Verifiziert: **545 Tests** (Stand dieses Slices; heute 721 Tests; 111 E2E
  inkl. pathlib JIT+AOT, 169 Frontend,
  3 Codegen, 2 IR, 152 Runtime-Unit, 22 Bytes, 5 Concurrent-GC, 8 CSV,
  6 GC, 18 IO, 1 JSON-GC, 6 JSON-IO, 9 Native-Concurrency, 11 Pathlib,
  4 Pool, 10 Stdlib-Utils, 3 STW-GC, 5 Thread-Integration), Gate ALL GREEN.

**Phase-2.23-Slice (Formatierung, 2026-10-24) — fertig & verifiziert:**

- f-String-Format-Specs (`:.2f`, `:04d`, `:#x`, Ausrichtung/Füllzeichen/
  Vorzeichen/Gruppierung) und `!s`/`!r`/`!a`; zentrale Logik in
  `core/format.rs`, ABI `pyx_format_value` (Wert + statischer Spec +
  Konversionsflag).
- `str.format(...)` mit positionellen/benannten Feldern, Index-/Attribut-
  Pfaden und Specs; printf-Formatierung `"%s:%d" % (...)` inkl. Tupel-
  Argumenten, Flags/Breite/Präzision und `%%`.
- Fehlerpfade: ungültige Specs → `ValueError`, falsche Operandentypen →
  `TypeError`; Beispiel `examples/formatting.py` JIT+AOT, `tests/format.rs`.
  Gate grün.

**Phase-2.24-Slice (Attribute, Methoden-Keywords, Builtin-Werte, 2026-10-24) — fertig & verifiziert:**

- `ExprKind::GetAttr` + ABI `pyx_getattr` für Datenattribute nativer Objekte:
  `Path` (`name`/`stem`/`suffix`/`parent`/`path`), Dateien (`name`/`mode`/
  `closed`/`encoding`), `Thread` (`name`/`ident`/`daemon`) und `Process`
  (`name`/`pid`/`exitcode`/`daemon`). `p.name` funktioniert; unbekannte Namen
  → `AttributeError` (`tests/attrs.rs`).
- `StmtKind::SetAttr` + `pyx_setattr` für Klasseninstanzen; Klassen-
  Namespaces bleiben unveränderlich.
- `ExprKind::BuiltinRef` + `pyx_builtin_function`: Builtins (`int`, `len`,
  `map`, …) sind Werte (`f = int`) und indirekt aufrufbar.
- `MethodCall.keywords` + ABI `pyx_method_kw`; Keywords sind für
  `str.split`/`replace`/`startswith`/`endswith`,
  `Path.mkdir`/`open`/`read_text`/`write_text`/`read_bytes`/`write_bytes`/
  `glob` und `argparse`-Methoden implementiert (`Path.mkdir(parents=True,
  exist_ok=True)`). Native Konstruktoren (`Thread(target=...)`,
  `queue.Queue(maxsize=...)`) binden Keywords über ihre `FunctionSpec.params`;
  andere native Methoden und User-Funktionen/-Methoden bleiben positional.

**Phase-2.25-Slice (`os.path`, `argparse`, `defaultdict`, 2026-10-24) — fertig & verifiziert:**

- `os.path` (`join`, `basename`, `dirname`, `exists`, `isfile`, `isdir`,
  `abspath`, `splitext`, `split`) als eigenes Modul in der zentralen
  Spezifikation; `import os.path`/`from os.path import join`.
- `argparse`-Teilmenge: `ArgumentParser`, `add_argument` (positional, lange/
  kurze Flags, `type`, `default`, `required`, `choices`, `help`, `store_true`,
  `nargs="?"`), `parse_args` → `Namespace`; `-h` druckt Hilfe mit `_help=True`.
  Kein `SystemExit` (Fehler → `RuntimeError`).
- `collections.defaultdict` mit `default_factory` (auch `int`/`list`/`str`),
  fehlender Key ruft Factory auf und fügt ein; `None`-Factory → `KeyError`;
  dict-kompatible Methoden/Repr. `tests/stdlib_modules.rs`, Beispiel
  `stdlib_modules.py` JIT+AOT. Gate grün.

**Phase-2.26-Slice (`re`, `socket`, 2026-10-24) — fertig & verifiziert:**

- `re` als Wrapper um die Rust-`regex`-Crate: `search`, `match`, `fullmatch`,
  `findall`, `sub`, `split`, `escape`, `compile`; `Pattern`/`Match` als native
  Objekte mit `group`/`start`/`end`/`span`/`findall`/`search`. Syntax ist
  Rust-`regex` (keine Lookarounds/Backreferences), Fehler → `ValueError`,
  falsche Typen → `TypeError`.
- `socket`: blockierendes TCP (`AF_INET`/`SOCK_STREAM`) über `std::net`;
  `socket`/`create_connection`, `bind`/`listen`/`accept`/`connect`,
  `send`/`sendall`/`recv`, `close`, `settimeout`/`gettimeout`,
  `shutdown`, `setsockopt`/`getsockopt`, `getsockname`/`getpeername`,
  `fileno`. Blockierende Aufrufe parken den Thread für den GC.
  Kein TLS/UDP/`select`, keine `socket.timeout`-Klasse.
- Beispiel `socket_echo.py` (Thread + TCP) JIT+AOT, `tests/socket.rs`.
  Gate grün.

**Phase-2.27-Slice (Klassen, 2026-10-24) — fertig & verifiziert:**

- `StmtKind::ClassDef` + ABI `pyx_class_new`: `class Name(Base)` mit
  höchstens einer einfachen Basis, Methoden, `__init__`, Vererbung/Overrides.
  Klassenkörper nur Methoden + `pass`/Docstring; Klassen-Namespace
  unveränderlich; keine Class-Attribute/Decorators/Metaclasses/Keywords/
  Multiple Inheritance.
- Instanzen: `Name(...)` ruft `__init__` mit `self`, Attribute über
  `pyx_getattr`/`pyx_setattr`, Methoden über `pyx_method` mit Bindung an die
  Instanz; Attribute-Tabelle ist GC-getraced und pro Instanz getrennt.
- Deterministische Repräsentation: `<class 'Name'>` bzw. `<Name object>`
  (keine Heap-Adresse). `examples/classes.py` JIT+AOT, `tests/classes.rs`;
  GC-sicher. Gate grün.

**Phase-2.28-Slice (Closures + eager Generatoren, 2026-10-24) — fertig & verifiziert:**

- **Closures by value:** verschachtelte `def` erzeugen
  `StmtKind::Closure`/`pyx_closure_new` und erfassen die *aktuellen Werte* der
  äußeren Locals als führende Wrapper-Parameter (`capture_count`). Spätere
  Änderungen der äußeren Variable sind unsichtbar; jede Ausführung (auch in
  Schleifen) erhält eigene Captures. Rekursive verschachtelte Funktionen und
  Closures in Lambdas sind Diagnosen (`tests/closures.rs`,
  `pyx-cli/tests/closures.rs`, `examples/closures.py`).
- **Generatoren eager:** `yield value` in einer Funktion legt ein verstecktes
  `$gen`-Listenlocal an, hängt jeden Wert an und gibt die Liste zurück.
  `yield from`, `yield` in Ausdrücken, bare `yield` und `return <wert>` im
  Generator sind Diagnosen. Keine lazy Generatorobjekte/send/throw/close.
- Generator Expressions bleiben eager materialisiert. Gate grün.

**Phase-2.29-Slice (eager Async, `with`-Protokoll, Aug-Subscript, 2026-10-24) — fertig & verifiziert:**

- `async def` wird wie `def` loweriert; `await expr` liefert `expr`,
  `async with`/`async for` nutzen dieselbe synchrone Lowering-/Protokoll-Logik.
  Keine Coroutinen/Tasks/Event-Loop; `asyncio.run(value)` ist identisch,
  `sleep` blockiert GC-sicher, `gather` materialisiert sequenziell.
  `examples/generators_async.py` JIT+AOT.
- `with`/`async with`: verstecktes `$withN`-Local, `__enter__()` bindet `as`,
  `__exit__(None, None, None)` in umgekehrter Reihenfolge auf jedem Pfad
  (return/break/continue/Exception); Rückgabewerte unterdrücken keine
  Exceptions. Dateien, `Pool` und User-Klassen implementieren das Protokoll.
- `a[i] += x` wertet Objekt und Index genau einmal über `$aug`-Locals aus
  (augmentierte Subscript-Zuweisung). Gate grün.

**Phase-2.30-Slice (User-Module, CLI/Toolchain, Cache, Scaling, Layout, 2026-10-24) — fertig & verifiziert:**

- **User-Module:** `import foo`/`from foo import f` lösen `foo.py` neben der
  importierenden Datei auf; Loader cachet pro kanonischem Pfad, lädt
  Abhängigkeiten zuerst, erkennt Zyklen. Nur Funktionen werden exportiert;
  importierte Module dürfen außer Imports/Funktionsdefinitionen/Docstring/
  `pass`/`__name__`-Zuweisung keinen Top-Level-Code enthalten. Pakete,
  Punkte/relative Imports und `import *` sind Diagnosen. `__name__` ist
  `"__main__"` in der Einstiegsdatei; kein Runtime-Modulobjekt/`sys.modules`.
  JIT und AOT (Einbettung) laufen identisch; `pyx-cli/tests/user_modules.rs`.
- **CLI:** `new`, `init`, `test` (plain Programme, `--json`), `fmt`
  (`--check`), `lint` (`--json`), `clean` (`--dry-run`); `add`/`remove`/
  `sync`/`lock` als unveränderter uv-Passthrough (`PYX_UV`).
- **Build-Cache:** `.pyx/cache` mit deterministischem Schlüssel (Quelltext,
  erreichbare `.py`-Module, Ziel/OS, Version, Runtime-Archiv), `--no-cache`,
  Treffer-/Invalidierungstests; `pyx clean` entfernt ihn.
- **Scaling-Benchmarks:** `pool_serial`, `pool_parallel`, `process_queue`
  (zusätzlich `threads_serial`/`threads_parallel`/`pool_map`) im Micro-Harness,
  in `results/latest.*` und `thresholds.json`.
- **Layout:** Crates intern in `pyx-ir/{model,ir,stdlib}`,
  `pyx-frontend/{lower,sema}`, `pyx-codegen/{backend,lower}`,
  `pyx-runtime/{abi,core,gc,modules,concurrency,support,csv}`,
  `pyx-cli/{cli,cli/commands}` strukturiert; `docs/LAYOUT.md`.
  Gesamtstand **730 Tests**, Gate ALL GREEN.

**Phase-2.31-Slice (Slab-Allokator, 2026-10-24) — fertig & verifiziert:**

- `gc/alloc.rs`: fester `PyxObj`-Slab/Free-List-Allokator (64er-Chunks,
  8-Byte-Alignment, LIFO-Reuse, Slabs bleiben ueber `free_all` erhalten,
  `absorb` bei Shared-Heap-Migration, Stats `slabs/capacity/live/reused`).
  Keine GC-Semantikaenderung; `PYX_GC_STATS` enthaelt Pool-Stats.
- Verifiziert: **730 Tests** (inkl. 4 Allocator-Tests), GC-/Thread-Stress,
  JIT+AOT, Gate ALL GREEN. Echte variable Payload-Groessenklassen und
  praezise Stack-Maps bleiben als eigene Architekturphasen blockiert.

**Bekannte, dokumentierte Grenzen Phase 1/2:** i64 statt BigInt; GC ist ein
konservativer Collector (Stack-Scan, STW-Root-Rescan; White-Partition kurz
unter Heap-Lock, Freigabe konkurrierend); keine präzisen Roots/Stack-Maps und
kein Größenklassen-Allokator (P2-05/06/07 offen); Exception-Hierarchie/
Chaining/`except*` fehlen; keine Slice-Zuweisung; `dict`-Views werden als
Listen materialisiert; Set-Reihenfolge ist deterministisch (Python garantiert
keine); `and`/`or` liefern Operanden, Typen sind dort ggf. `Any`; Dicts sind
linear (O(n)).
**Klassen:** nur eine Basis, keine Class-Attribute/Decorators/Metaclasses/
Descriptors; Instanz-Repräsentation `<Name object>`; Klassen-Namespace unveränderlich.
**Closures:** by value; keine Mutable-Captures, keine rekursiven verschachtelten
Funktionen, keine Closures in Lambdas.
**Generatoren/async:** eager materialisierte Listen bzw. sequenzieller Code
ohne Coroutinen/Tasks/Event-Loop; `yield from`/`yield`-Ausdrücke/`return value`
im Generator, echte `asyncio`-APIs und `async for`-Semantik einer Ereignis-
schleife fehlen.
**Module:** nur native Module + Geschwister-`.py`; keine Pakete, Punkte/
relativen Imports, Top-Level-Seiteneffekte in importierten Modulen oder
Runtime-Modulobjekte/`sys.modules`; Importe nur auf Modulebene.
**Stdlib:** kuratierte Teilmenge; `re` = Rust-`regex`-Syntax, `socket` nur
blockierendes TCP (kein TLS/UDP/select, Timeout → `OSError`), `argparse` ohne
`SystemExit`, `random` nicht CPython-reproduzierbar, CSV vollständig im
Speicher.
**Spawn/Pool:** Funktionsregistry nur nach Name/Arity (Duplikate/Lambdas/native
Targets abgelehnt); Queue-`maxsize` geht im Spawn-Payload verloren; Pool-
Context-Manager vorhanden.
**Datei-I/O:** UTF-8-Text (`r/w/a/x`) und Bytes (`rb/wb/ab/xb`), kein
`+`-Modus; `newline`/`errors` ignoriert.
**Bytes:** Literale/Konstruktor/Ops/Kernmethoden, aber kein
`bytearray`/`memoryview`.
**C-API:** P4-04 blockiert (kein `python3-config`/`Python.h`/linkbares
`libpython`); keine Embedding-/ABI3-Messungen, keine Entscheidung.
**Plattform:** Linux x86_64 verifiziert; aarch64-Pfade ungetestet; macOS/
Windows/Cross-Compilation fehlen.

## Nächste konkrete Schritte

1. P2-10/P2-12-Rest: echte lazy Generatorobjekte (`send`/`throw`/`yield from`)
   und echte Coroutinen/Tasks/Event-Loop; `asyncio` über die drei
   Kompatibilitätsfunktionen hinaus.
2. P2-05/P2-06/P2-07: Größenklassen-Allokator, präzise Shadow-Stack-Roots/
   Stack-Maps, feineres Safepoint-Polling und Pausenmetriken (P2-08).
3. P4-04: blockierende Umgebungsfrage klären (CPython-Dev-Paket/Sysroot),
   dann Embedding-, ABI3- und NumPy-Smoke gemäß `docs/C_API.md`; erst danach
   P4-05/P4-06.
4. P3-03/P3-08: uv-Interpreter-Injektion (`.python-version`, Venv-Marker,
   `sys.executable`-Äquivalent), Packaging/Cross-Compilation.
5. P5/P6-Rest: Hyperfine-Adapter, CI-Schwellen (Q-06), Completions/Manpage
   (P6-07), ICE-Regressionstest (P6-05/P6-08), CLI-Referenz/Diagnose-Katalog
   (P6-09), `--verbose`/`--quiet` (P6-03/P6-10).
6. Q-03/Q-04: Fuzzing für Parser/Lowering/Codegen und Runtime/GC;
   Exception-Hierarchie/Chaining (P2-11-Rest).

## Kommandos

```bash
export PATH="$HOME/.cargo/bin:$PATH"   # rustc/cargo liegen hier
cargo build                            # gesamter Workspace
cargo test                             # Unit-/Integrationstests (721)
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
./scripts/verify.sh                    # vollständiger Phasen-Gate
cargo run -q -p pyx-cli -- new /tmp/demo
cargo run -q -p pyx-cli -- run examples/fib.py
cargo run -q -p pyx-cli -- build examples/fib.py -o /tmp/fib && /tmp/fib
cargo run -q -p pyx-cli -- test examples
cargo run -q -p pyx-cli -- fmt --check examples
cargo run -q -p pyx-cli -- lint examples
python3 benchmarks/pyx/run_micro.py    # CPython vs. JIT vs. AOT
python3 scripts/check_benchmarks.py    # lokales Schwellen-Gate
```

## Architektur-Invarianten (Kurzfassung)

- IR ist der einzige Frontend↔Codegen-Vertrag; Signaturen in
  `crates/pyx-ir/src/ir.rs`/`model.rs`.
- ABI ist in `docs/ARCHITECTURE.md` §3 fixiert; Runtime-Funktionen liegen in
  `pyx_runtime::abi`, `extern "C"`, `#[no_mangle]`, **ohne Panics**.
- Werte: tagged `i64`; Ints/Bools/None immediate; Heap-Objekte an 8 Byte
  ausgerichtet mit Kind-Header.
- Fehler laufen über pending Exception + `pyx_exc_pending()`-Checks im Codegen.
- Heap-Objekte werden seit Phase 2.1/2.15 vom konservativen, konkurrierenden
  Mark-&-Sweep-GC verwaltet (Shared Heap, Insertion-Barrieren, Safepoints);
  kein Leak-Zustand mehr. Präzise Roots/Allokator bleiben offen.
- Backend: Cranelift 0.120; LLVM später optional.
- Keine stillen Fehlcompile: nicht unterstützte Syntax → Diagnose.
- Docs-Invarianten: `docs/ARCHITECTURE.md` (Verträge), `docs/TODO.md`
  (Arbeitsliste), `docs/C_API.md` (P4-04), `docs/LAYOUT.md` (Baum) müssen mit
  Code/Testzahl übereinstimmen; keine erfundenen Messungen/„fertig“-Claims.

## Arbeitsweise für Subagents

- Jeder Agent besitzt **disjunkte Dateien**; keine Edits an Crates anderer Agents.
- Agents editieren nur; sie führen **keine** Builds/Tests/Formatter aus.
- Verträge (IR/ABI) nicht eigenmächtig ändern; Abweichung melden.
- Der Orchestrator baut, testet, formatiert und dispatcht Fix-Ups.

## Session-Log

- 2026-10-23: Repo ist greenfield (nur `PLAN.md`, kein Git). Umgebung geprüft.
  `docs/TODO.md`, `docs/ARCHITECTURE.md`, Workspace-Scaffold, IR und ABI-Vertrag
  angelegt; Phase-1-Implementierung an Subagents verteilt.
- 2026-10-23 (Fortsetzung): Phase 1 vollständig verifiziert (Parser/Lowering/
  Inferenz, Runtime, Cranelift JIT+AOT, CLI, 45 Tests, CPython-Differential).
  Zwei Vertragsfehler gefunden und behoben: `pyx_truthy` liefert 0/1 statt
  getaggter Booles (sonst sind alle Bedingungen wahr); Frame-Stack für korrekte
  Traceback-Frames; bare re-raise über `pyx_raise_value`; `str*int`-Inferenz.
  Danach Phase-2.0-Container-Slice: list/dict, Indexing/Assignment, Iterations-
  ABI, for-über-Container, Python-Repr, neue Exception-Tags, Beispiele und
  E2E-Tests. Gate: `scripts/verify.sh` ALL GREEN, 79 Tests, AOT-Parität,
  CPython-Differential identisch. Nächster Schritt: P2-11 (`else`/`finally`),
  dann Klassen/Module/GC (siehe oben).
- 2026-10-23 (Fortsetzung 2): Phase-2.1-GC-Interim implementiert. Konservativer
  Mark-&-Sweep mit Registry, Stack-/Register-Roots (asm-Dump aller GPRs,
  asm-Stack-Loads), pending-Exception-Root und explizitem Root für das neue
  Objekt. Schwellwert/Stress per Env, ABI `pyx_gc_collect`/`pyx_gc_objects`.
  Verifiziert: 89 Tests, Stress-/Threshold-Läufe, AOT, Zyklen, Drucktest
  15.5 MB vs. 154 MB Peak-RSS. Offen: präzise Roots/nebenläufiger Collector
  (P2-06/07), Größenklassen-Allokator (P2-05), danach P2-11/P2-01/P2-02.
- 2026-10-23 (Fortsetzung 3): P2-11 `try/else/finally` implementiert. IR um
  else/finally und bare except erweitert; Codegen mit Cleanup-Stack,
  Action-Codes und geparkter Exception; Return-Bug behoben; Bare-raise im
  finally re-raised die fliegende Exception. KeyError-Repr korrigiert.
  Verifiziert: 100 Tests, CPython-Differential fuer finally-Kanten (nested,
  return/break/continue, Suppression, Override), AOT + GC-Stress, Gate gruen.
  Naechster Slice: Slices/f-Strings/String-Methoden (P2-02).
- 2026-10-23 (Fortsetzung 4): P2-02-Slice umgesetzt: lesende Slices fuer
  str/list (IR `ExprKind::Slice`, ABI `pyx_slice`, volle slice.indices-
  Semantik) und einfache f-Strings (`{expr}`, `!s`) via str()-Konkatenation.
  Format-Specs/`!r`/`!a`, Slice-Assignment und Dict-Slices liefern Diagnosen.
  Verifiziert: 124 Tests, CPython-Differential (Slices/f-Strings), GC-Stress,
  AOT slices/fstrings, Gate gruen. Naechster Slice: String-/Container-Methoden
  und Format-Specs.
- 2026-10-23 (Fortsetzung 5): P2-02-Methoden umgesetzt. IR `MethodCall`,
  ABI `pyx_method`, dynamischer Dispatch fuer str/list/dict, AttributeError.
  Statische Methodenmatrix + `Any`-Empfaenger; GC-sichere Listenkonstruktion
  (split/keys/values). Verifiziert: 158 Tests, CPython-Differential,
  GC-Threshold, AOT methods, Gate gruen. Naechster Slice: tuple/set/bytes,
  danach Format-Specs/!r/%-Formatierung.
- 2026-10-23 (Fortsetzung 6): P2-01-Tupel-Slice umgesetzt. `Type::Tuple`,
  Tuple-Literale, Unpacking (Assignment + for-Targets), dict.items(), tuple
  count/index, ABI pyx_tuple_new/pyx_unpack, GC-Tracing. repr-Quote-Wahl an
  CPython angepasst. Verifiziert: 198 Tests, CPython-Differential, GC, AOT,
  Gate gruen. Naechster Slice: set/bytes + Membership (`in`).
- 2026-10-23 (Fortsetzung 7): P2.6 Set + Membership implementiert. Set-Typ
  (insertion-ordered, hashbar, GC-getraced), Literale, set()/set(iterable),
  Methoden add/remove/discard/clear/copy/union, `set - set`, `in`/`not in`
  fuer list/tuple/dict/str/set. Frontend-Methodenmatrix und Diagnosen erweitert;
  len-Inferenz-Fix (unaufgeloeste Variable nicht mehr auf str zwingen).
  Verifiziert: 231 Tests, CPython-Differential, GC, AOT, Gate gruen.
  Naechster Slice: bytes, danach Format-Specs/!r/%-Formatierung.
- 2026-10-23 (Fortsetzung 8): P6.0-Diagnostics-Polish. Diagnostic-Vertrag um
  code/end_col/help erweitert; alle Frontend-Fehler kategorisiert, did-you-mean
  fuer Namen/Methoden, Parse-Spans. CLI rendert Cargo-aehnliche Snippets, --json,
  Farben, Summary; `pyx doctor`; ICE-catch_unwind. 256 Tests, Gate gruen.
  Die P6-Liste (Cargo-aehnliche Einheit: new/init/test/fmt/lint/clean, Cache,
  Completions, Docs) ist in docs/TODO.md am Ende aufgenommen.
- 2026-10-23 (Fortsetzung 9): P2.7 Range + Benchmark-Builtins. Range als Wert,
  list/dict/sum/enumerate/zip; Range-Membership CPython-konform gelockert.
  290 Tests, Gate gruen. benchmarks/ analysiert: bestehende Harnesses sind
  CPython/uv-basiert; Readiness-Matrix und PyX-Micro-Harness werden ergänzt,
  User-Harnesses bleiben unverändert. Naechste Sprachslots: Comprehensions/
  Lambdas/Module (loops/csv), danach bytes/Format-Specs.
- 2026-10-23 (Fortsetzung 10): Benchmark-Integration gestartet. `benchmarks/`
  analysiert, Readiness-Matrix `benchmarks/PYX.md`, importfreier Micro-Harness
  `benchmarks/pyx/run_micro.py` (CPython vs JIT vs AOT, Output-Verifikation,
  Peak-RSS) und Baseline `benchmarks/pyx/results/latest.{json,md}`.
  Release-Baseline: Startup 20.4/2.9/1.0 ms, while_sum 47.5/10.6/8.0 ms,
  dict_insert 21.8/79.6/52.4 ms (linear), string_concat 23.9/13.2/12.4 ms
  (O(n²)). Die großen User-Harnesses bleiben unverändert; Adapter folgt.
- 2026-10-23 (Fortsetzung 11): P2.8 Comprehensions. List/Set/Dict/Generator-
  Expressions inkl. Multi-for/if/Tupel-Targets/Shadowing; eager materialisierte
  Generatoren dokumentiert. Runtime-Append-Helfer, GC-sichere Codegen-Loops.
  307 Tests, Gate gruen. Benchmark-Baseline um list_comp/filter_comp/
  sum_genexp erweitert (AOT 5.9/7.5/6.7 ms). Naechster Slice: Lambdas/
  First-Class-Functions + map/filter/reduce.
- 2026-10-23 (Fortsetzung 12): P2.9 First-Class Functions. FunctionRef/
  IndirectCall, Lambda-Synthese (ohne Closures/Defaults/Star), argv-Wrapper fuer
  JIT+AOT, pyx_call, map/filter (eager). 333 Tests, Gate gruen. Baseline:
  map_lambda 30.7/13.0/13.4 ms, filter_lambda 28.6/13.9/12.9 ms,
  lambda_call 32.7/15.4/14.4 ms. Naechster Slice: Module/Imports/with.
- 2026-10-23 (Fortsetzung 13): P2.10 Module/Imports. Shared stdlib spec;
  import/from-import aliases; module attrs/calls; native functions; math,
  functools.reduce, operator, time, sys. 370 Tests, Gate gruen. Baseline:
  reduce_add 27.0/7.2/5.7 ms, math_loop 28.8/22.4/25.8 ms. Naechster Slice:
  User-Module/Import-System + json/os/io/with/csv, dann async.
- 2026-10-23 (Fortsetzung 14): P2.11 json + os. dumps/loads (CPython-Defaults,
  GC-sicher), os-Basis. 386 Tests, Gate gruen. Baseline json_dumps
  29.0/5.6/2.7 ms, json_loads 26.8/4.9/2.7 ms. P2-20 (echtes Threading/
  Multiprocessing) in docs/TODO.md aufgenommen, inkl. Shared-Heap/GC-Design,
  threading, multiprocessing, Serialisierung, Parallel-Benchmarks und
  Race-/Stress-Tests.
- 2026-10-24 (Fortsetzung 15): P2.12 Shared Heap + `_thread`, kein GIL.
  Globaler Heap mit Migration beim ersten Thread, Thread-Registry, Locks,
  start_new_thread/get_ident/join (PyX-Extension); GC bei >1 Thread ausgesetzt,
  nach Join wieder aktiv. 392 Tests, Gate gruen. Threads laufen JIT+AOT,
  4x3000-String-Stress unter Lock ok. Baseline serial 130.2/32.9/32.1 ms,
  parallel 73.0/23.3/17.8 ms. Naechste Schritte: STW-/Go-Collector,
  threading-Klassen, multiprocessing, Datei-I/O/async.
- 2026-10-24 (Fortsetzung 16): P2.13 STW-/Safepoint-GC. ThreadGc, Safepoints
  (Codegen), Parking bei Lock/join/sleep, Thread-Roots (Spawn-Argumente),
  Pending-Mirror, Collector scannt geparkte Stacks/Register + Roots. 398 Tests,
  Gate gruen, JIT+AOT, Threshold=1 Stress. Safepoint-Overhead gemessen
  (threads_serial AOT 39.6 vs 32.1 ms). Naechster Slice: threading-Klassen,
  multiprocessing, dann Datei-I/O/async und Go-Collector.
- 2026-10-24 (Fortsetzung 17): P2.14 threading/queue/multiprocessing/pickle.
  Native Thread/Lock/RLock/Event/Queue/Process/Queue-Objekte, fork/waitpid/
  _exit, eigenes PYX1-Pickle, Keyword-Konstruktoren. 422 Tests, Gate gruen,
  JIT+AOT. Grenzen: fork-only (kein spawn/Pool). Naechster Slice: konkurrierender
  GC.
- 2026-10-24 (Fortsetzung 18): P2.15 konkurrierender Mark-&-Sweep. Farben,
  Root-STW + konkurrierende Mark-Phase + finaler STW-Sweep, Insertion-Barrieren
  pro Kante, Graph-Lock (local bypass), wachstumsbasierte Schwelle, GC-Stats.
  Edge-Barrier-Fix: 110k-Liste 8.5 s -> 17–44 ms Markierung; 422 Tests, Gate
  gruen. Naechster Slice: Pool/spawn, nebenlaeufiger Sweep, Safepoint-Polling.
- 2026-10-24 (Fortsetzung 19): P2.16 echtes spawn + Pool. Frischer Prozess
  per exec, Funktionsregistry/Arity, Payload/Queue-FDs, kein Modulbody im
  Kind, hidden `__spawn`; Pool mit per-Worker-Queues fuer fork und spawn.
  436 Tests, Gate gruen, JIT+AOT (spawn/pool/pool_spawn). Naechster Slice:
  Prozess-Scaling/terminate, nebenlaeufiger Sweep.
- 2026-10-24 (Fortsetzung 20): P2.17 konkurrierender Sweep + terminate.
  Phase C STW-Rescan, danach White-Partition unter Heap-Lock und Freigabe
  ausserhalb; Farb-Reset/ MARKING off. Process/Pool.terminate, pool_map-
  Benchmark (AOT 6.5 ms vs CPython 59.2 ms). 440 Tests, Gate gruen.
- 2026-10-24 (Fortsetzung 21): P2.18 Datei-I/O/with/CSV/JSON-Dateien/os.remove.
  File-Objekt UTF-8-Text, OSError-Tag, with-Desugaring, csv reader/writer-
  Familien, json dump/load; Beispiele files/csv_json JIT+AOT. 477 Tests,
  Gate gruen. Naechster Slice: async/bytes/pathlib/random/argparse.
- 2026-10-24 (Fortsetzung 22): P2.19 Bytes. Literale/Konstruktor, Index/
  Slice/Iteration, Ops/Hash, decode/encode/hex/split/join/strip,
  Serializer; CPython-Differential, JIT+AOT. 515 Tests, Gate gruen.
- 2026-10-24 (Fortsetzung 23): P2.20 Binaerdateien rb/wb/ab/xb, Bytes-
  Read/Write/Iteration, Byte-Seek/Tell, CSV/JSON-Abweisung. 522 Tests,
  Gate gruen. Naechster Slice: async/socket/pathlib/random.
- 2026-10-24 (Fortsetzung 24): P2.21 random/platform/statistics. SplitMix64,
  Random-Objekt, randrange/choice/shuffle, Platform-Werte, Statistikfunktionen.
  533 Tests, Gate gruen, JIT+AOT. Naechster Slice: pathlib/argparse/async.
- 2026-10-24 (Fortsetzung 25): P2.22 pathlib.Path. Nativer Path, Text/Bytes,
  mkdir/iterdir/glob, `/`-Operator, os.remove/rename Path-like; 545 Tests,
  Gate gruen. Naechster Slice: argparse/defaultdict/async.
- 2026-10-24 (Fortsetzung 26): P2.23 Formatierung. f-String-Specs/`!s`/`!r`/
  `!a` über `pyx_format_value`, `str.format`, `%`-Format; `core/format.rs` als
  gemeinsame Implementierung; Beispiel formatting, JIT+AOT, Gate gruen.
- 2026-10-24 (Fortsetzung 27): P2.24 Attribute/Methoden-Keywords/Builtin-Werte.
  `GetAttr`/`pyx_getattr` (Path/File/Thread/Process), `SetAttr`/`pyx_setattr`,
  `BuiltinRef`/`pyx_builtin_function`, `pyx_method_kw` (`mkdir(parents=True)`);
  `p.name` statt `p.name()`; tests/attrs.rs, Gate gruen.
- 2026-10-24 (Fortsetzung 28): P2.25 os.path/argparse/defaultdict. Neun
  os.path-Funktionen, ArgumentParser/Namespace-Teilmenge ohne SystemExit,
  defaultdict mit Factory; Beispiel stdlib_modules, Gate gruen.
- 2026-10-24 (Fortsetzung 29): P2.26 re/socket. Rust-`regex`-Wrapper
  (Pattern/Match, search/match/fullmatch/findall/sub/split/escape) und
  blockierendes TCP (socket/create_connection/bind/listen/accept/...);
  GC-sicheres Parken; socket_echo JIT+AOT, Gate gruen.
- 2026-10-24 (Fortsetzung 30): P2.27 Klassen. Single-Base-ClassDef,
  Methoden/__init__/Vererbung/Overrides, Instanz-Attribute, dynamischer
  Dispatch, deterministische Repr; classes.py JIT+AOT, GC-sicher, Gate gruen.
  Grenzen: keine Class-Attribute/Decorators/Metaclasses/Multiple Inheritance.
- 2026-10-24 (Fortsetzung 31): P2.28 Closures + eager Generatoren. Nested def
  als `Closure`/`pyx_closure_new` mit by-value-Captures (capture_count),
  Rekursion/Lambda-Closures diagnostiziert; `yield value` materialisiert eine
  Liste, yield-from/Ausdruck/bare/return im Generator diagnostiziert.
  closures.py + CLI-Tests, Gate gruen.
- 2026-10-24 (Fortsetzung 32): P2.29 eager async + with + Aug-Subscript.
  async def/await sequenziell, async with/for synchron, asyncio.run/sleep/
  gather; with über __enter__/__exit__ (Dateien/Pool/User-Klassen, reverse
  Reihenfolge, keine Suppression); `a[i] += x` via `$aug` einmalige
  Auswertung. generators_async.py JIT+AOT, Gate gruen.
- 2026-10-24 (Fortsetzung 33): P2.30 User-Module/CLI/Cache/Scaling/Layout.
  Sibling-Imports mit Modul-Cache pro kanonischem Pfad, Zyklus-Diagnose,
  funktions-only-Exporte, __name__, AOT-Einbettung; CLI new/init/test/fmt/
  lint/clean + uv-Passthrough; Build-Cache mit Modulbaum-Fingerprint;
  pool_serial/pool_parallel/process_queue im Harness; Crate-Restrukturierung.
  Stand 721 Tests, Gate gruen.
- 2026-10-24 (Fortsetzung 34): Docs-Refresh nach dem Abschluss-Slice.
  AGENTS/TODO/ARCHITECTURE/README/CHANGELOG aktualisiert, `docs/LAYOUT.md`
  und `docs/C_API.md` angelegt. C-API-Umgebungsprobe: python3 3.12.3, aber
  kein python3-config/Python.h und keine linkbare libpython → P4-04 `[!]`;
  keine Messungen erfunden. Keine Code-/Cargo-/CI-Edits.
