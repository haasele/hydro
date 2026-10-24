Dein Projekt lässt sich inzwischen als eigenständige Python-Runtime mit Compiler und Paketökosystem definieren – konzeptionell ähnlich zu Bun im JavaScript-Ökosystem, nur für Python.

Das Ziel: Entwickler schreiben normalen Python-Code, verwenden einen eigenen Befehl wie `pyx run main.py`, und deine Toolchain übernimmt Typprüfung, Kompilierung, Ausführung und Abhängigkeitsverwaltung. CPython soll nicht die Standardlaufzeit sein; Python-Kompatibilität ist dennoch ein zentrales Ziel.

Eine wichtige technische Klarstellung: Eine neue Runtime, ein eigener Garbage Collector und ein nativer Compiler sind drei verschiedene Komponenten. Sie müssen gemeinsam entworfen werden, aber nicht alle von Grund auf neu entstehen. Insbesondere Compiler-Backend und Paketverwaltung würde ich möglichst wiederverwenden.

### 1\. Der bisherige Scope

Eigene Runtime in Rust

Objektmodell, Exceptions, Module, Built-ins, Speicherverwaltung und Laufzeitfunktionen.

Eigener nativer Compiler

Python-Syntax parsen, Typen prüfen, Code optimieren und Maschinencode erzeugen.

Go-artiger Garbage Collector

Nebenläufiger Mark-and-Sweep-Ansatz, effiziente Allokation, kurze Pausen und Profiling.

Eigene Toolchain auf Basis von uv

Paketinstallation, virtuelle Umgebungen, Lockfiles und Projektverwaltung weiterverwenden, aber den eigenen Interpreter beziehungsweise Compiler aufrufen.

Dabei würde ich `uv` zunächst integrieren und nicht sofort forken. Wenn seine Architektur deine Runtime-Auswahl nicht sauber zulässt, kannst du gezielt forken oder einen eigenen Wrapper bauen.

### 2\. JIT oder AOT?

Ich würde AOT als primären Compiler und JIT als spätere Optimierung planen.

### AOT — Ahead-of-Time

Primär

Der Code wird vor dem Start in nativen Maschinencode übersetzt.

Vorteile: schnelle und vorhersehbare Startzeit nach dem Build, keine JIT-Aufwärmphase, einfache Bereitstellung und native Optimierungen.

### JIT — Just-in-Time

Später

Häufig ausgeführte Funktionen werden während der Laufzeit kompiliert oder erneut optimiert.

Vorteile: Der Compiler kann reale Typen und Laufzeitverhalten beobachten. Nachteile: zusätzliche Komplexität, Aufwärmkosten und Speicherbedarf.

LLVM bietet mit ORC eine Infrastruktur für JIT-Kompilierung, einschließlich verzögerter und paralleler Kompilierung. Du müsstest dafür keinen JIT von Grund auf entwickeln.

Für ein System, das normale Python-Projekte möglichst unkompliziert ausführen soll, wäre meine Roadmap:

* AOT-Compiler mit Cache.

* Optionaler Interpreter oder Bytecode-Modus für Entwicklung und REPL.

* Profiling und Benchmarks.

* Erst dann ein JIT für Funktionen, bei denen Messungen einen tatsächlichen Vorteil zeigen.

Ein JIT ist kein automatischer Performancegewinn. Bei einer statisch typisierten Sprache kann AOT bereits sehr effizienten Code erzeugen. Der JIT wird besonders interessant, wenn dynamischer Python-Code zur Laufzeit optimiert werden soll.

### 3\. Was in deinem Scope noch fehlt

Neben Compiler, Runtime, GC und Paketmanager brauchst du mindestens diese Komponenten:

Python-Frontend und Sprachsemantik

Parser für Python-Syntax, AST, Scopes, Imports, Exceptions, Klassen, Decorators, Generatoren, Async/Await und dynamische Features. Ein Parser allein genügt nicht: Du musst auch das Verhalten der Sprache nachbilden.

Standardbibliothek und Python-Interop

`os`, `pathlib`, `json`, `re`, `socket`, `asyncio`, `subprocess`, `threading` und weitere Module. Du musst entscheiden, welche du neu implementierst und welche über eine Kompatibilitätsschicht laufen.

Modul- und ABI-Kompatibilität

Native Python-Erweiterungen verwenden häufig die CPython-C-API. Wenn du beispielsweise NumPy oder PyTorch unverändert laden willst, brauchst du eine kompatible ABI-Schicht, CPython als eingebettete Runtime oder eine explizit begrenzte Kompatibilität. Das ist einer der größten Teile des Projekts.

CLI und Build-System

`run`, `build`, `test`, `fmt`, `lint`, `check`, `add`, `remove`, `sync` und `lock`. Dazu kommen reproduzierbare Builds, inkrementelle Kompilierung und ein lokaler Build-Cache.

Debugger, Fehlermeldungen und Profiling

Python-artige Tracebacks, Source Maps beziehungsweise Debug-Metadaten, native Debugger-Integration, CPU- und Heap-Profiler sowie GC-Metriken. Ohne diese Werkzeuge wäre die Sprache im Alltag schwer nutzbar.

### 4\. Wie ich den Gesamtumfang priorisieren würde

Empfohlene Entwicklungsreihenfolge

* Phase 1 — Minimaler Compiler

    Python-artige Syntax, Variablen, Funktionen, Kontrollfluss, Typinferenz, Exceptions und native Ausgabe über LLVM. Noch keine vollständige Python-Kompatibilität.

* Phase 2 — Runtime

    Objektmodell, Strings, Listen, Dictionaries, Module, Speicherallokator und zunächst ein korrekt funktionierender Garbage Collector. Performanceoptimierungen folgen nach Messungen.

* Phase 3 — Toolchain

    Projektdateien, Paketauflösung, Lockfiles, Build-Cache und ein `run`-Befehl. `uv` zunächst integrieren, statt es unnötig früh zu forken.

* Phase 4 — Python-Kompatibilität

    Tests gegen die CPython-Testsuite, Standardbibliothek, Exceptions, dynamische Features, anschließend C-Extensions und ABI-Kompatibilität.

* Phase 5 — Performance

    AOT-Optimierung, Profile-Guided Optimization, GC-Tuning und erst danach gegebenenfalls JIT. Benchmarks gegen CPython, PyPy, Go und vergleichbare Implementierungen.

Ein Projekt wie dieses ist groß genug, dass die vollständige Python-Kompatibilität ein eigenständiges langfristiges Entwicklungsziel sein sollte. Projekte wie Nuitka und PyPy zeigen, wie viel Aufwand bereits in Compiler, Runtime und Kompatibilität steckt.

Meine abschließende Empfehlung: AOT zuerst, JIT später; einen Go-artigen GC als Zielarchitektur, aber zunächst Korrektheit und Messbarkeit priorisieren; `uv` möglichst integrieren; und die Python-C-API-Kompatibilität früh prototypisieren, bevor du dich auf einen vollständig neuen Objekt- und Laufzeitentwurf festlegst. Gerade die ABI-Kompatibilität könnte darüber entscheiden, ob dein System tatsächlich das Python-Ökosystem übernehmen kann oder letztlich eine neue Sprache mit lediglich vertrauter Syntax wird.

