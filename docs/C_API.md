# PyX — C-API-/ABI-Strategie (P4-04-Spike)

Stand: 2026-10-24. Dieses Dokument ist die **Entscheidungsgrundlage** für
P4-04 und die nachfolgenden Punkte P4-05/P4-06. Es enthält bewusst **keine
Messwerte**: Der Spike ist in dieser Umgebung blockiert (siehe unten), und
erfundene Laufzeiten/Erfolgsquoten wären keine Grundlage.

## 1. Frage

Sollen CPython-C-Extensions (NumPy, `pydantic-core`, …) unter PyX laufen, und
wenn ja, über welchen Weg? Die Antwort bestimmt, ob PyX-Objekte langfristig
ein CPython-kompatibles Layout brauchen oder ob eine Übersetzungsschicht
genügt. Sie berührt Objektmodell, GC, Refcounting und die ABI-Stabilität.

## 2. Optionen

### A — CPython einbetten (`libpython`)

PyX lädt eine CPython-Bibliothek und führt C-Extensions in einer echten
CPython-Interpreter-Instanz aus. PyX-Werte werden an der Grenze konvertiert
(oder die Extension arbeitet nur mit CPython-Objekten, die PyX kapselt).

- **Vorteile:** volle C-API/ABI-Kompatibilität, `abi3`-Extensions laufen ohne
  PyX-eigene Nachbauten; NumPy-Smoke-Test ist realistisch.
- **Kosten:** CPython wird zur Laufzeitabhängigkeit (widerspricht D2 nur für
  den Kompatibilitätsmodus, nicht für den Standardpfad); zwei Objektmodelle und
  zwei GCs müssen koexistieren; Konvertierung/Refcounting an jeder Grenze;
  Distributions-/Lizenzfragen; `libpython` muss zum PyX-Build passen.
- **Voraussetzungen:** `Python.h`, `python3-config` bzw. `pkg-config python3`,
  eine linkbare `libpython3.x` (shared oder static) und ein passender
  Include-/Library-Pfad.

### B — ABI3-Shim auf PyX-Objekten

PyX implementiert die CPython-C-API (mindestens den `Py_LIMITED_API`/
`abi3`-Umfang) selbst über seinen eigenen Objekten. Extensions werden gegen
diesen Shim gelinkt.

- **Vorteile:** CPython bleibt nicht Laufzeitabhängigkeit; ein Prozess, ein
  GC; theoretisch schlanker als eine zweite VM.
- **Kosten:** sehr hoch. `PyObject`/`PyTypeObject`-Layouts, Refcounting,
  `PyArg_ParseTuple`, Buffer-Protokoll, Module-Init (`PyModuleDef`), GIL-
  Erwartungen und `abi3`-Symbole müssen exakt passen. Der Shim muss zudem
  Refcounts mit dem konservativen Mark-&-Sweep-GC koordinieren; ein
  nachträglicher Einbau in das getaggte `i64`-Wertmodell ist ein Bruch der
  ABI-Invarianten (Heap-Zeiger sind keine `PyObject*`).
- **Voraussetzungen:** dieselben Header für die Kompilierung der Extensions
  sowie eine eigene, testbare Shim-Bibliothek; ein `abi3`-Symbolabgleich ist
  ohne Header/Toolchain nicht möglich.

### C — Begrenzte Kompatibilität (keine C-Extensions)

PyX unterstützt nur pure-Python-Pakete und eine kuratierte native Stdlib.
`ctypes`/`cffi` werden nicht als CPython-ABI-Brücke angeboten.

- **Vorteile:** kein Objektmodell-Risiko, keine zweite VM, klare Grenzen.
- **Kosten:** kein NumPy/Pydantic/FastAPI-Ökosystem; FastAPI-Benchmark bleibt
  blockiert. Für viele Skripte ausreichend, für „Bun für Python“ zu wenig.

**Empfehlung (vorläufig):** Option A als Kompatibilitätsmodus prototypisieren,
Option B nur falls Messungen zeigen, dass der Einbettungs-Overhead oder die
Distribution untragbar ist. Option C ist der Fallback, falls P4-04 dauerhaft
nicht finanzierbar ist. Diese Empfehlung ist **keine** Entscheidung; sie wird
erst nach dem Spike in `docs/ARCHITECTURE.md` §9 festgehalten.

## 3. Umgebungsprobe (2026-10-24, Linux x86_64)

Ausgeführt im Repository, ohne Installation von Paketen:

| Prüfung | Ergebnis |
|---|---|
| `python3 -V` | `Python 3.12.3` |
| `command -v python3-config` | nicht vorhanden (auch kein `python3.12-config`) |
| `pkg-config --exists python3` | negativ; kein `python3.pc` |
| `sysconfig.get_paths()["include"]` | `/usr/include/python3.12` |
| `ls /usr/include/python3.12/Python.h` | **fehlt** (das Verzeichnis enthält fremde Header, u. a. `Imaging.h`) |
| `LIBRARY`/`INSTSONAME` | `libpython3.12.a` / `libpython3.12.so.1.0` |
| `ls libpython3.12.{so,a}` | beide **fehlen**; nur `libpython3.12.so.1.0` (Laufzeit-SO) ist vorhanden |
| `cc`/`gcc` | vorhanden (`/usr/bin/cc`, `/usr/bin/gcc`) |

Ergebnis: Zwar ist ein CPython-3.12-Interpreter samt Laufzeit-SO installiert,
aber **kein Entwicklungs-Paket**. Ohne `Python.h`, `python3-config` und eine
linkbare `libpython3.12.so`/`.a` lässt sich weder ein Embedding-Smoke-Test noch
ein `abi3`-Shim kompilieren. Die vorhandene Runtime-SO reicht nicht, weil der
Linker den unversionierten SONAME-Eintrag (`libpython3.12.so`) erwartet.

**Status P4-04: `[!]` blockiert.** Der Blocker ist die Umgebung, nicht eine
architektonische Unentscheidbarkeit. Sobald ein CPython-Entwicklungspaket
(`python3-dev`/`python3.12-dev` oder ein äquivalentes Sysroot) bereitsteht,
sind die folgenden Schritte ausführbar.

## 4. Spike-Plan (sobald die Umgebung steht)

1. **Embedding-Smoke (Option A):** minimales C-Programm, das
   `Py_Initialize`, `PyRun_SimpleString("print(1)")` und `Py_Finalize` nutzt,
   gegen `libpython` linkt und in einem PyX-JIT-Prozess läuft. Erfolgskriterium:
   Exit 0, keine Symbol-/Linkerfehler.
2. **`abi3`-Smoke:** ein C-Modul mit `#define Py_LIMITED_API 0x030C0000`,
   `PyModuleDef` und einer trivialen Funktion; Import aus CPython und aus dem
   Embedding-Kontext. Erfolgskriterium: Import + Aufruf.
3. **NumPy-Smoke (nur wenn 1–2 grün):** `import numpy` im Embedding-Kontext,
   eine kleine Array-Operation; protokolliert Importzeit, RSS und
   Konvertierungsaufwand an der PyX↔CPython-Grenze.
4. **Shim-Aufwand (Option B):** Symbol-/Header-Abgleich gegen den
   `abi3`-Umfang; Liste der benötigten Typen/Funktionen und ein
   Refcount-Konzept für den getaggten Heap. Kein Code, bevor 1–3 vermessen
   sind.
5. **Entscheidung:** Ergebnis, Messwerte und gewählte Option in
   `docs/ARCHITECTURE.md` §9 und `docs/TODO.md` (P4-04/P4-05) eintragen.

## 5. Auswirkungen auf andere offene Punkte

- **P4-05 (Extension Loading):** hängt vollständig an der Option; ohne
  Entscheidung kein `.so`-Loader.
- **P4-06 (`ctypes`/`cffi`):** Option C würde die Grenzen explizit
  dokumentieren; Option A könnte `ctypes` über CPython mitliefern.
- **P4-07 (Serialisierung/Numerik):** unabhängig; `pickle` bleibt das eigene,
  nicht CPython-kompatible PYX1-Format, bis eine Kompatibilitätsschicht
  beschlossen ist.
- **Objektmodell/GC:** Option B würde Refcounting erzwingen; Option A erlaubt
  den bestehenden konservativen GC im PyX-Pfad und CPython-Refcounting nur im
  Kompatibilitätsmodus.

## 6. Was dieses Dokument nicht behauptet

- Keine Aussage, dass eine der Optionen bereits funktioniert.
- Keine Laufzeit-, Speicher- oder Kompatibilitätsmessungen.
- Keine Zusage, dass NumPy/FastAPI jemals unterstützt werden.
- Keine Änderung an `Cargo.toml`/CI; der Spike bleibt ein isolierter
  Prototyp, bis P4-04 entschieden ist.
