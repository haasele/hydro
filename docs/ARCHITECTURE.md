# PyX — Architektur

Stand: **2026-10-24**. Phase 1 ist abgeschlossen; Phase 2 ist weitgehend
implementiert (721 Tests, `scripts/verify.sh` ALL GREEN). Dieses Dokument ist
der **verbindliche Vertrag** zwischen den Crates. Änderungen an ABI/IR nur mit
Update dieses Dokuments und aller Nutzer. Der C-API-Spike (P4-04) ist blockiert
und in [`C_API.md`](C_API.md) dokumentiert.

## 1. Zielbild

PyX ist eine eigenständige Python-Runtime mit AOT-Compiler und Toolchain
(`pyx`), konzeptionell Bun für Python. CPython ist nicht die Standardlaufzeit.
Ein Ziel ist weitgehende Python-Kompatibilität, zunächst über eine klar
definierte Teilmenge (siehe `docs/TODO.md`).

**Unterstützt:** getaggte Werte, Container, Funktionen/Closures, Klassen
(einzelne Basis), Comprehensions, f-Strings/Formatierung, Exceptions,
Datei-I/O, User-Module, native Stdlib-Module, Threads/Prozesse/Pool, ein
konservativer konkurrierender Mark-&-Sweep-GC.

**Bewusst nicht (Stand jetzt):** BigInt, echte lazy Generatoren/Coroutinen mit
Event-Loop, vollständige CPython-Stdlib, C-Extensions/C-API, Decorators,
Pattern Matching, Metaclasses, `bytearray`/`memoryview`, präzise GC-Roots und
ein eigener Größenklassen-Allokator. Details in §10 und `docs/TODO.md`.

## 2. Komponenten & Datenfluss

```
Quelltext .py (+ importierte Geschwistermodule)
  │  rustpython-parser (Python-Grammatik, AST)
  ▼
pyx-frontend ── Lowering ──► typed IR (pyx-ir) ──► pyx-codegen
  │ Typprüfung/Inferenz                          │
  ▼                                              ├─ JIT (pyx run)
Diagnosen (file:line:col)                        └─ Object + cc-Link (pyx build)
                                                        │
                                                  pyx-runtime (libpyx_runtime.a, C-ABI)
```

| Crate | Verantwortung | Abhängigkeiten |
|---|---|---|
| `pyx-ir` | Typisiertes HIR; einziger Frontend↔Codegen-Vertrag; zentrale Stdlib-Spezifikation | – |
| `pyx-frontend` | Parsen, Modul-Loader/Imports, Lowering, Typinferenz, Diagnosen | `pyx-ir`, `rustpython-parser` |
| `pyx-runtime` | Value-Modell, Operatoren, Builtins, Exceptions, GC, Datei-I/O, Module, Concurrency; `rlib` + `staticlib` | `regex` u. a. |
| `pyx-codegen` | Cranelift-Codegen, JIT- und AOT-Pfad | `pyx-ir`, `pyx-runtime`, Cranelift |
| `pyx-cli` | `pyx`-Binary (Projekt, Compiler, Tests, Formatter, Lint, uv-Passthrough) | `pyx-frontend`, `pyx-codegen` |

### 2.1 Ordnerlayout

Die Crates sind seit dem Abschluss-Slice intern in thematische Ordner geteilt
(vollständiger Baum in [`LAYOUT.md`](LAYOUT.md)):

```
crates/pyx-ir/src/{model.rs, ir.rs, stdlib/}
crates/pyx-frontend/src/{lib.rs, diag.rs, lower/, sema/}
crates/pyx-codegen/src/{lib.rs, backend/, lower/}
crates/pyx-runtime/src/{abi.rs, core/, gc/, modules/, concurrency/, support/, csv.rs}
crates/pyx-cli/src/{main.rs, cli/, cli/commands/}
```

`pyx-ir` bleibt logikfrei (Vertragsstrukturen + Typtabellen). Runtime-Code
liegt ausschließlich in `pyx-runtime`; `pyx-codegen` ruft nur
`pyx_runtime::abi`-Symbole auf.

## 3. Runtime-ABI (verbindlich)

Alle Werte sind `i64` (`PyxValue`). Alle Funktionen sind `extern "C"`,
`#[no_mangle]`, im Modul `pyx_runtime::abi`, und dürfen **nicht** panicken
(Nutzerfehler → pending Exception; interne Fehler → `process::abort`).

### 3.1 Tagging

| Bits (`v & 7`) | Bedeutung |
|---|---|
| `000` | `int`: Wert = `v >> 3` (arithmetisch, i64) |
| `001` | Spezial: `None = 1`, `False = 9`, `True = 17` (Payload `0/1/2`) |
| `010` | Heap-Zeiger: `ptr = v & !7`, zeigt auf `PyxObj` |
| Rest | reserviert |

Ints, Bools und None sind immediates; Floats/Strings/Container/native Objekte
liegen auf dem Heap. Heap-Objekte besitzen einen Header mit `kind: u32` und
sind an 8 Byte ausgerichtet.

Objektarten (stabile `kind`-Werte):

| Kind | Objekt | Kind | Objekt |
|---|---|---|---|
| 0 | `Str` | 15 | `MpQueue` |
| 1 | `Float` | 16 | `Pool` |
| 2 | `Exception` | 17 | `File` |
| 3 | `List` | 18 | `CsvReader` |
| 4 | `Dict` | 19 | `CsvWriter` |
| 5 | `Iter` | 20 | `Bytes` |
| 6 | `Tuple` | 21 | `Random` |
| 7 | `Set` | 22 | `Path` |
| 8 | `Range` | 23 | `ArgumentParser` |
| 9 | `Function` | 24 | `Namespace` |
| 10 | `Lock` | 25 | `DefaultDict` |
| 11 | `Thread` | 26 | `Pattern` |
| 12 | `Event` | 27 | `Match` |
| 13 | `Queue` | 28 | `Socket` |
| 14 | `Process` | 29 | `Class` |
| | | 30 | `Instance` |

Listen/Dicts/Sets sind insertion-ordered (Sets deterministisch; Python
garantiert keine Reihenfolge). `Iter` hält Ziel und Index. `Class` trägt Name,
optionale Basis und eine unveränderliche Methodentabelle; `Instance` trägt
Klasse und eine veränderliche Attributtabelle.

### 3.2 Symbole

```c
i64  pyx_none(void);                  // Konstante 1
i64  pyx_true(void);                  // 17
i64  pyx_false(void);                 // 9
i64  pyx_bool(i64 b);                 // Truthiness -> True/False
i64  pyx_int(i64 v);                  // identity (Bereichsprüfung)
i64  pyx_str_new(const uint8_t* ptr, size_t len);   // kopiert UTF-8
i64  pyx_bytes_new(const uint8_t* ptr, size_t len);
i64  pyx_bytes_build(uint64_t argc, const i64* argv); // bytes(...)
i64  pyx_float_new(double f);
i64  pyx_binop(uint32_t op, i64 a, i64 b);          // inkl. Vergleiche
i64  pyx_unop(uint32_t op, i64 a);
i64  pyx_truthy(i64 v);               // 1 wenn truthy, 0 wenn falsy (C-Stil)
i64  pyx_print(uint64_t argc, const i64* argv);     // space-separiert + '\n'
i64  pyx_len(i64 v);
i64  pyx_to_str(i64 v);
i64  pyx_to_int(i64 v);
i64  pyx_to_float(i64 v);
i64  pyx_abs(i64 v);
i64  pyx_format_value(i64 value, const uint8_t* spec, size_t spec_len,
                      uint8_t conversion);          // f-String-Feld
i64  pyx_list_new(uint64_t argc, const i64* items);
i64  pyx_dict_new(uint64_t argc, const i64* keys, const i64* values);
i64  pyx_tuple_new(uint64_t argc, const i64* items);
i64  pyx_unpack(i64 value, uint64_t expected);      // None oder ValueError
i64  pyx_set_new(uint64_t argc, const i64* items);
i64  pyx_set_from(i64 iterable);
i64  pyx_range_new(i64 start, i64 stop, i64 step);  // step != 0
i64  pyx_list_from(i64 iterable);
i64  pyx_dict_from(i64 iterable);
i64  pyx_sum(i64 iterable, i64 start);
i64  pyx_enumerate(i64 iterable, i64 start);
i64  pyx_zip(uint64_t argc, const i64* iterables);
i64  pyx_list_append(i64 list, i64 value);          // None oder Fehler
i64  pyx_set_add(i64 set, i64 value);
i64  pyx_dict_set(i64 dict, i64 key, i64 value);
i64  pyx_function_new(const uint8_t* wrapper, uint64_t arity,
                      const uint8_t* name, size_t name_len);
i64  pyx_builtin_function(const uint8_t* name, size_t name_len); // f = int
i64  pyx_closure_new(const uint8_t* wrapper, uint64_t arity,
                     const uint8_t* name, size_t name_len,
                     uint64_t capture_count, const i64* captures);
i64  pyx_class_new(const uint8_t* name, size_t name_len, i64 base,
                   uint64_t method_count, const i64* names, const i64* funcs);
i64  pyx_call(i64 fn, uint64_t argc, const i64* argv);
i64  pyx_map(i64 fn, i64 iterable);                 // materialisierte Liste
i64  pyx_filter(i64 fn_or_none, i64 iterable);
i64  pyx_module_call(const uint8_t* module, size_t module_len,
                     const uint8_t* function, size_t function_len,
                     uint64_t argc, const i64* argv);
i64  pyx_module_get(const uint8_t* module, size_t module_len,
                    const uint8_t* name, size_t name_len);
i64  pyx_subscript(i64 obj, i64 index);
i64  pyx_subscript_set(i64 obj, i64 index, i64 value);
i64  pyx_slice(i64 obj, i64 start, i64 stop, i64 step);
i64  pyx_getattr(i64 obj, const uint8_t* name, size_t name_len); // Datenattribut
i64  pyx_setattr(i64 obj, const uint8_t* name, size_t name_len, i64 value);
i64  pyx_method(i64 obj, const uint8_t* name, size_t name_len,
                uint64_t argc, const i64* argv);
i64  pyx_method_kw(i64 obj, const uint8_t* name, size_t name_len,
                   uint64_t argc, const i64* argv,
                   uint64_t kwargc, const i64* kw_argv); // [name, value, ...]
i64  pyx_iter_new(i64 obj);
i64  pyx_iter_next(i64 iter, i64* out);             // 1=Wert, 0=Ende
i64  pyx_gc_collect(void);
i64  pyx_gc_objects(void);
void pyx_safepoint(void);
void pyx_set_file(const uint8_t* ptr, size_t len);
void pyx_set_line(uint32_t line);
void pyx_frame_enter(const uint8_t* name, size_t len);
void pyx_frame_leave(void);
i64  pyx_exc_pending(void);
i64  pyx_exc_tag(void);
i64  pyx_exc_value(void);
i64  pyx_exc_clear(void);
i64  pyx_raise(uint32_t tag, i64 msg);
i64  pyx_raise_msg(uint32_t tag, const uint8_t* p, size_t n);
i64  pyx_raise_value(i64 exc);
i64  pyx_uncaught(void);
void pyx_runtime_init(void);
void pyx_register_function(const uint8_t* name, size_t name_len,
                           const uint8_t* wrapper, size_t arity); // spawn/Pool
int32_t pyx_spawn_child(void);        // Kindmodus nach exec (-1 = normal)
```

`op`-Codes (`pyx_binop`): `0 Add, 1 Sub, 2 Mul, 3 Div, 4 FloorDiv, 5 Mod,
6 Pow, 7 Eq, 8 Ne, 9 Lt, 10 Le, 11 Gt, 12 Ge, 13 In, 14 NotIn`.
`pyx_unop`: `0 Neg, 1 Pos, 2 Not`.

Exception-Tags: `0 ValueError, 1 TypeError, 2 ZeroDivisionError,
3 RuntimeError, 4 OverflowError, 5 KeyboardInterrupt, 6 IndexError,
7 KeyError, 8 StopIteration, 9 AttributeError, 10 OSError`. Namen 1:1 wie in
Python; `IOError` ist ein Alias für `OSError`.

### 3.3 Semantik

#### Numerik, Strings, Formatierung

- `int`/`int`-Arithmetik in i64; Overflow → `OverflowError` (dokumentierte
  Grenze). Kein BigInt.
- `/` liefert immer `float`; Division durch 0 → `ZeroDivisionError`.
- `//`/`%` folgen Python (Boden-Division, Vorzeichen des Divisors).
- `+` auf `str` ist Konkatenation; `==`/`!=`/`<`… auf Strings lexikografisch.
- Gemischt `int`/`float` → `float`; `bool` ist numerisch `0/1`.
- `print` ruft `str()` je Argument auf, trennt mit Leerzeichen, beendet mit
  `\n`.
- `str % args` ist printf-Formatierung, sobald die linke Seite ein String ist
  (`%s`, `%d`, `%f`, `%x`, Flags/Breite/Präzision, `%%`, Tupel-Argumente).
- f-Strings: `{expr}`, `{expr!s}`, `{expr!r}`, `{expr!a}` und Format-Specs
  (`:.2f`, `:04d`, `:#x`, Ausrichtung/Füllzeichen/Vorzeichen/Gruppierung).
  Der Codegen ruft `pyx_format_value`; Konvertierung und Spec werden vor der
  Formatierung angewandt. Ungültige Specs → `ValueError`, falsche Typen →
  `TypeError`.
- `str.format(...)` unterstützt positionelle/benannte Felder, Index-/Attribut-
  Pfade und Format-Specs. Die Spec-Logik liegt zentral in
  `pyx-runtime/src/core/format.rs` und wird von f-Strings, `str.format` und
  `%` gemeinsam genutzt.
- `bytes` (Phase 2.19): Literale `b"..."`, `bytes()`-Konstruktor, Indexing
  (Int 0..255), Slicing, Iteration, `+`/`*`, Vergleiche/Equality/Hash,
  Subsequenz-Membership und Kernmethoden (`decode`, `hex`, `split`, `join`,
  `strip`, `replace`, `upper`/`lower`, ...); `str.encode` liefert Bytes.
  Kein `bytearray`/`memoryview`.

#### Container

- Listen/Dicts: Indexing mit negativen Indizes; `IndexError` außerhalb des
  Bereichs; `KeyError` mit repr(key) bei fehlendem Dict-Key; `len` zählt
  Elemente; `for` iteriert Listen/Strings (Unicode-Skalare) und Dict-Keys in
  Einfügereihenfolge; `str(list)`/`str(dict)` nutzen Python-Repräsentation
  (`[1, 'a']`, `{'k': 1}`); `==`/`!=` sind strukturell (Dicts reihenfolge-
  unabhängig); `list + list` konkateniert.
- Slices (`s[a:b:c]`) folgen `slice.indices` (negative Indizes, Defaults,
  `step == 0` → ValueError) und liefern neue Strings (Unicode-Skalare) bzw.
  Listen; Dicts sind nicht slicebar (`TypeError`).
- Tupel: unveränderlich, Indexing/Slicing wie Listen, `len`, Iteration,
  strukturelle Gleichheit, `tuple + tuple` und `tuple * int`; Repräsentation
  `(1, 'a')`, Singleton `(1,)`, leer `()`. Tupel aus hashbaren Elementen sind
  als Dict-Keys erlaubt. Unpacking (`a, b = pair`) akzeptiert Listen/Tupel mit
  exakter Länge; `dict.items()` liefert eine Liste von `(key, value)`-Tupeln
  (dokumentierte Abweichung von der View). Nested/starred Unpacking ist eine
  Diagnose.
- `in`/`not in`: `list`/`tuple` via Python-Gleichheit, `dict` über Keys,
  `str` als Substring (linker Operand muss `str` sein), `set` über Elemente;
  andere rechte Operanden → TypeError. `set`-Literale deduplizieren, bewahren
  aber die Einfügereihenfolge, unterstützen `len`/Iteration/Truthiness/
  strukturelle Gleichheit und `repr` (`{1, 2}`, leer `set()`); unhashbare
  Elemente → TypeError.
- `range(...)` ist ein Wert (`range`-Objekt, Iteration/len/Membership/Repr);
  `list()`/`dict()`/`sum()`/`enumerate()`/`zip()` sind Builtins.
  `enumerate`/`zip` materialisieren Listen von Tupeln.
- Comprehensions (Phase 2.8): `[e for ...]`, `{e for ...}`, `{k: v for ...}`
  und Generatorausdrücke; mehrere `for`-Klauseln und `if`-Filter werden
  geschachtelt ausgewertet, Tupel-Targets entpackt. Generatorausdrücke werden
  **eager als Liste** materialisiert (dokumentierte Abweichung bis echte
  Generatorobjekte existieren); Comprehension-Variablen sind synthetische
  Locals und leaken nicht.
- Dict-Insertion ist linear (O(n) pro Lookup/Insert); Container-Mutation
  während einer Iteration ist indexbasiert und undefiniert.

#### Funktionen, Closures, Klassen

- Benannte Funktionen und Lambdas sind Werte (`FunctionRef`); indirekte Calls
  laufen über `pyx_call` (Wrapper mit argv). Lambdas sind parameter-only; ein
  Zugriff auf äußere Namen im Lambda ist eine Diagnose.
- Builtins sind ebenfalls Werte: `f = int`, `map(f, xs)` etc. materialisieren
  ein `FunctionBody::Builtin`-Objekt über `pyx_builtin_function`.
- **Closures sind by value** (Phase 2.28): `def inner` innerhalb einer Funktion
  erzeugt `pyx_closure_new` mit den *aktuellen Werten* der referenzierten
  Locals. Spätere Änderungen der äußeren Variable sind für eine bereits
  erzeugte Closure unsichtbar; jede Ausführung erzeugt eigene Captures (auch in
  Schleifen). Captures werden als führende Parameter an den Wrapper übergeben
  (`capture_count` in der Signatur). Rekursive verschachtelte Funktionen und
  Closures in Lambdas werden diagnostiziert.
- **Klassen** (Phase 2.27): `class Name(Base):` mit höchstens einer einfachen
  Basis. Erlaubt sind Methoden, `__init__`, `pass` und ein Docstring; der
  Klassen-Namespace ist nach der Erzeugung unveränderlich. `Name(...)` erzeugt
  eine Instanz und ruft `__init__` (über die Klassen-/Basis-Kette) mit der
  Instanz als `self`. Methoden werden über `pyx_method` dynamisch nach
  Empfängertyp und Klassenkette aufgelöst; Attribute über
  `pyx_getattr`/`pyx_setattr`. Nicht unterstützt: Class-Attribute,
  Decorators, Metaclasses, `class`-Keywords, Multiple Inheritance,
  `__repr__`/Descriptor-Protokolle. Instanzen rendern deterministisch als
  `<ClassName object>` (keine Heap-Adresse), Klassen als `<class 'Name'>`.
- **Attribute** (`pyx_getattr`): `pathlib.Path` (`name`, `stem`, `suffix`,
  `parent`, `path`), Dateien (`name`, `mode`, `closed`, `encoding`),
  `threading.Thread` (`name`, `ident`, `daemon`) und
  `multiprocessing.Process` (`name`, `pid`, `exitcode`, `daemon`) liefern
  Datenattribute. Für native Objekte sind Namen ohne Aufruf Datenattribute;
  Methoden müssen aufgerufen werden. Unbekannte Namen → `AttributeError`.
  `pyx_setattr` erlaubt nur Instanz-Attribute; Klassen sind unveränderlich.
- **Methoden-Keywords** (`pyx_method_kw`): native Methoden mit benannten
  Parametern akzeptieren Keywords. Implementiert sind `str.split`/
  `replace`/`startswith`/`endswith`, `pathlib.Path.mkdir`/`open`/
  `read_text`/`write_text`/`read_bytes`/`write_bytes`/`glob` und
  `argparse`-Methoden; z. B. `Path.mkdir(parents=True, exist_ok=True)`.
  Native **Konstruktoren** (z. B. `Thread(target=..., args=...)`,
  `queue.Queue(maxsize=...)`) binden Keywords über ihre
  `FunctionSpec.params`. Andere native Methoden sowie User-Funktionen/
  Methoden lehnen Keywords ab (Runtime-/Frontend-Diagnose).

#### Generatoren und async (eager)

- **Generatoren** (Phase 2.28): Eine Funktion mit `yield value` wird eager
  loweriert: ein verstecktes `$gen`-Listenlocal wird mit `[]` initialisiert,
  jeder `yield` wird zu `append`, die Funktion gibt die materialisierte Liste
  zurück. `list(gen(...))` funktioniert damit, aber es gibt **keine**
  Generatorobjekte, kein `send`/`throw`/`close`, keine Lazy-Auswertung.
  `yield from`, `yield` in einem Ausdruck, bare `yield` und `return <wert>` im
  Generator sind Diagnosen. Generator Expressions sind ebenfalls eager.
- **async** (Phase 2.29): `async def` wird wie `def` loweriert; es gibt keine
  Coroutine-Objekte und keinen Event-Loop. `await expr` liefert den Wert von
  `expr` direkt (sequenziell), `async with`/`async for` verwenden dieselbe
  synchrone Lowering-/Protokoll-Logik. `asyncio` stellt nur `run(value)`
  (identisch), `sleep(seconds)` (blockierend, GC-sicher geparkt) und
  `gather(iterable)` (sequenziell materialisierte Liste) bereit. Das ist
  **keine** Nebenläufigkeit; echte Coroutinen/Tasks/Event-Loop bleiben offen.

#### `with`-Protokoll

`with a as x, b as y:` wird zu verschachtelten `try/finally`-Blöcken. Jeder
Kontextausdruck wird genau einmal in ein verstecktes `$withN`-Local
ausgewertet, dessen `__enter__()` an das `as`-Ziel gebunden wird. Das
`finally` ruft `__exit__(None, None, None)` auf demselben Objekt auf, sodass
`return`, `break`, `continue` und Exceptions jedes Objekt in umgekehrter
Reihenfolge verlassen. Der Rückgabewert von `__exit__` wird ignoriert:
Exceptions werden **nie** unterdrückt. Dateien, `Pool` und User-Klassen
implementieren das Protokoll; ohne `as` läuft `__enter__` trotzdem.

#### Imports, User-Module, `__name__`

- Native Module werden über `pyx_ir::stdlib` spezifiziert (siehe unten).
  `import X [as Y]` und `from X import a [as b]` sind nur auf Modulebene
  erlaubt; importierte Namen sind in Funktionen sichtbar. `X.f(...)` →
  ModuleCall, `X.const`/`X.f` als Wert → ModuleAttr. Native Funktionen werden
  als Function-Objekte materialisiert; Calls/Arity/Fehler laufen über die
  gemeinsame Spezifikation. Unbekannte Module/Member und Modulwerte als
  Variablen sind Diagnosen.
- **User-Module** (Phase 2.30): `import foo` / `from foo import f` lösen
  `foo.py` neben der importierenden Datei auf. Der Loader lädt jedes Modul
  **einmal pro kanonischem Pfad** (Modul-Cache auf Compile-Zeit), lädt
  Abhängigkeiten zuerst und erkennt Zyklen. Nur Funktionen werden exportiert;
  importierte Module dürfen außer Imports, Funktionsdefinitionen, Docstring,
  `pass` und der injizierten `__name__`-Zuweisung **keine** Top-Level-
  Ausdrücke enthalten (keine Seiteneffekte beim Import). Pakete, Punkte/
  relative Imports und `from x import *` sind nicht unterstützt. Alle
  Funktionen landen in einem `Program`; JIT und AOT führen denselben Code aus
  (AOT bettet die Module ins Binary ein).
- `__name__` ist ein Modulbody-Local und wird nur injiziert, wenn der Body es
  liest: `"__main__"` für die Einstiegsdatei, sonst der Modulname. Ein
  Runtime-Modulobjekt (`sys.modules`, `module.__dict__`) gibt es nicht.

#### Stdlib-Module

Zentrale Spezifikation in `pyx_ir::stdlib` (`MODULES` in
`stdlib/mod.rs`): `math`, `io`, `functools`, `operator`, `time`, `sys`,
`random`, `platform`, `pathlib`, `statistics`, `json`, `csv`, `os`,
`os.path`, `_thread`, `threading`, `queue`, `multiprocessing`, `pickle`,
`argparse`, `collections`, `re`, `socket`, `asyncio`.

- `json`: `dumps`/`loads` mit CPython-Defaults (ensure_ascii, Separatoren,
  Escapes, Surrogate, Fehlerklassen, Depth-/Cycle-Guards) sowie `dump`/`load`
  auf File-Objekten; Parser baut Container GC-sicher inkrementell.
- `os`: `getpid`/`getppid`/`cpu_count`/`getcwd`/`getenv`, `name`/`sep`,
  `remove`/`rename`, `fork`/`waitpid`/`_exit`. `os.path`: `join`, `basename`,
  `dirname`, `exists`, `isfile`, `isdir`, `abspath`, `splitext`, `split`.
- `io`/`open`: UTF-8-Text (`r/w/a/x`) und Bytes (`rb/wb/ab/xb`); kein
  `+`-Modus. `read`/`readline`/`readlines`/`write`/`writelines`/`flush`/
  `tell`/`seek`/`close`/Zeileniteration; Fehler `OSError`, geschlossene Datei
  `ValueError`.
- `csv`: `reader`/`writer` und `DictReader`/`DictWriter` (fieldnames),
  RFC-4180-nahes Quoting/embedded newlines, `writerow(s)`/`writeheader`.
  Reader materialisieren; kein Dialect/restkey, CSV ist kein Streaming.
- `argparse` (Phase 2.25): ein `ArgumentParser` mit positionalen Argumenten,
  langen/kurzen Optionen, `type`, `default`, `required`, `choices`, `help`,
  `store_true`, `nargs="?"`; `parse_args` liefert ein `Namespace`. Fehler
  sind `RuntimeError` (kein `SystemExit`); `-h` druckt Hilfe und setzt
  `_help=True`.
- `collections.defaultdict`: insertion-ordered `(key, value)`-Paare plus
  `default_factory`; fehlender Key ruft die Factory auf, fügt ein und gibt
  zurück; `None`-Factory → `KeyError`.
- `re` (Phase 2.26): Wrapper um die `regex`-Crate mit `search`, `match`,
  `fullmatch`, `findall`, `sub`, `split`, `escape`, `compile`; `Pattern` und
  `Match` sind native Objekte mit `group`/`start`/`end`/`span`/`findall`/
  `search`. Syntax ist Rust-`regex` (keine Lookarounds/Backreferences);
  ungültige Patterns → `ValueError`.
- `socket` (Phase 2.26): blockierendes TCP (`AF_INET`/`SOCK_STREAM`) über
  `std::net`: `socket`, `create_connection`, `bind`, `listen`, `accept`,
  `connect`, `send`/`sendall`, `recv`, `close`, `settimeout`, `gettimeout`,
  `shutdown`, `setsockopt`/`getsockopt`, `getsockname`/`getpeername`.
  Blockierende Aufrufe parken den Thread für den GC. Kein TLS/UDP/`select`
  und keine `socket.timeout`-Klasse (Timeout → `OSError`).
- `random`: globaler SplitMix64-PRNG und seedbares `Random(seed=...)` mit
  `seed`/`random`/`randint`/`randrange`/`choice`/`uniform`/`shuffle`;
  deterministisch pro Seed, aber **nicht CPython-reproduzierbar**.
- `platform`: `system`/`machine`/`release`/`version`/`node`/`platform`/
  `python_version`, `python_implementation = "PyX"`.
- `statistics`: `mean`/`fmean`/`median`/`pstdev`/`stdev` inkl. Fehlerpfade.
- `pathlib`: natives `Path` (`Path()`, `cwd()`, `home()`) mit
  `exists`/`is_file`/`is_dir`, `read_text`/`write_text`, `read_bytes`/
  `write_bytes`, `open`, `mkdir`, sortiertem `iterdir`/`glob` (`*`/`?`, kein
  `**`), `joinpath`/`resolve`/`absolute`/`as_posix`, `/`-Join und
  Datenattributen `name`/`stem`/`suffix`/`parent`/`path`. `os.remove`/
  `rename` akzeptieren Path-Objekte.
- `asyncio`: siehe §3.3 „Generatoren und async“ (nur `run`/`sleep`/`gather`).

#### Methoden

`pyx_method`/`pyx_method_kw` dispatchen dynamisch nach Empfängertyp.
`str`: `upper`, `lower`, `strip`, `lstrip`, `rstrip`, `split` (0/1 Arg),
`join`, `replace` (2/3 Args), `startswith`, `endswith`, `find`, `count`,
`isdigit`, `isalpha`, `isalnum`, `isspace`, `format`, `encode` (Bytes).
`list`: `append`, `extend`, `insert`, `remove`, `pop` (0/1 Arg), `clear`,
`reverse`, `copy`, `count`, `index`. `dict`: `get` (1/2 Args), `keys`,
`values`, `items`, `pop` (1/2 Args), `update`, `clear`. `tuple`: `count`,
`index`. `set`: `add`, `remove`, `discard`, `clear`, `copy`, `union`.
`keys`/`values` liefern bewusst Listen (keine Views). Unbekannte Methoden →
`AttributeError`; Attributzugriff ohne Aufruf auf nativen Typen ist entweder
ein Datenattribut (siehe oben) oder `AttributeError`. Für `str`, `list`,
`dict`, `tuple`, `set`, `bytes` prüft das Frontend Methoden/Arität statisch;
`Any`-Empfänger werden zur Laufzeit geprüft.

#### Datei-I/O und Concurrency

- Dateien: siehe `io`; `with` nutzt das `__enter__`/`__exit__`-Protokoll.
- **Kein GIL.** Der Heap wechselt beim ersten `_thread.start_new_thread` in
  einen globalen Shared Heap (bestehende Objekte werden migriert). Threads
  erhalten IDs; `_thread.join` ist eine PyX-Extension. Locks
  (`_thread.allocate_lock`) sind native `Lock`-Objekte; `threading` liefert
  `Thread`, `Lock`, `RLock` (Owner/Depth), `Event` und `queue.Queue`.
- `multiprocessing` bietet fork-basierte `Process`-Objekte, `os.fork`/
  `waitpid`/`_exit` und `multiprocessing.Queue` (socketpair, PYX1-
  Serialisierung). `spawn` startet einen frischen Prozess per `exec`
  (`pyx __spawn` bzw. AOT-Binary); `pyx_main` registriert alle Funktionen vor
  dem Modulbody, ein Payload nennt Zielname + Args, und nur diese Funktion
  läuft im Kind (Queue-FDs werden geerbt; kein Modulbody im Kind).
  `Pool` nutzt je Worker private Task-/Result-Queues für fork und spawn und
  implementiert `__enter__`/`__exit__` (Terminate).
- `pickle.dumps/loads` nutzt ein eigenes, nicht CPython-kompatibles
  PYX1-Hexformat (None/Bool/Int/Float/Str/List/Tuple/Dict/Set/Range/Bytes/
  Function-Wrapper); zyklusfrei, Depth 128. Funktionen werden per
  Wrapper-Adresse/Name nur im gleichen, geforkten oder per spawn
  registrierten Adressraum rekonstruiert; native Objekte/Lambdas werden
  abgelehnt.
- Gemeinsam genutzte veränderliche Container erfordern User-Locks; ohne GIL
  sind ungeschützte Mutationen Datenraces und kein unterstütztes Verhalten.

### 3.4 Fehlerprotokoll

Fehlerfähige Runtime-Aufrufe setzen bei Fehler eine **pending Exception** und
liefern einen Dummy-Wert. Der Codegen prüft nach *jeder* fehlerfähigen Operation
`pyx_exc_pending()` und springt zum nächsten Handler (oder zum Uncaught-Pfad).
Eine Exception trägt Tag, Message, Funktionsname und Zeile zum Zeitpunkt des
`raise`. `pyx_uncaught()` druckt:

```
Traceback (most recent call last):
  File "test.py", line 7, in main
ZeroDivisionError: division by zero
```

und liefert Exit-Code `1`.

## 4. Backend-Entscheidung

**Cranelift 0.120** (pure Rust) für Phase 1–3; LLVM optional später hinter
Feature-Flag.

Begründung: kein System-LLVM/`llvm-config`/Header nötig, schnelle Builds,
JIT und Object-Emission aus einer Codebasis, solide Baseline-Optimierung.
Kosten: schwächere Optimierung als LLVM (PGO/Inlining, P5) und kein
ORC-JIT-Tiering. Der Codegen bleibt in `pyx-codegen` gekapselt, sodass ein
LLVM-Backend später dieselbe IR konsumieren kann. Entscheidung in P0-11
dokumentiert.

### 4.1 AOT-Linking

`pyx build` erzeugt eine Object-Datei (`.o`) und linkt sie mit `cc` gegen
`libpyx_runtime.a` (staticlib des Workspace). Das erzeugte Binary exportiert
`main(argc, argv) -> int`, welches `pyx_main() -> int` aufruft. `pyx_main`
führt den Modulkörper (inkl. eingebetteter User-Module) aus und liefert den
`pyx_uncaught()`-Exitcode bei pending Exception, sonst `0`. Das Binary
benötigt weder CPython noch `pyx`.

Runtime-Library-Auflösung: `--runtime-lib <path>`, sonst `PYX_RUNTIME_LIB`,
sonst relativ zur CLI-Executable (`target/<profile>/libpyx_runtime.a`).
Fehlt sie, klare Fehlermeldung mit Bauhinweis.

Der Build-Cache liegt unter `.pyx/cache` und ist eine reine lokale
Optimierung. Der Schlüssel umfasst CLI-/Runtime-Version, Ziel-Architektur/OS,
Quelltext, alle erreichbaren `.py`-Module (relativer Pfad + Inhalt) und die
Identität der Runtime-Archive. `--no-cache` umgeht ihn; `pyx clean` entfernt
ihn. Der Cache ist kein geteilter Artefakt-Store.

## 5. IR-Vertrag

`pyx-ir` ist absichtlich klein und vollständig typisiert:

- `Program { functions, main }`; `functions[0]` ist der Modulkörper `<module>`
  der Einstiegsdatei. User-Module belegen zusammenhängende `FunctionId`-
  Bereiche; alle Funktionen liegen in einem `Program`.
- Jede `Function` besitzt `params`, `capture_count`, `ret_ty` und `locals`
  (Index = `LocalId`). `capture_count` führende Parameter sind Closures-
  Captures und werden vom Runtime-Wert geliefert; die sichtbare Arität ist
  `params.len() - capture_count`.
- `Stmt`/`Expr` tragen `line` (1-basiert) für Tracebacks.
- Ausdrücke: `Const`, `Local`, `Bin` (inkl. `In`/`NotIn`), `Un`, `BoolOp`
  (short-circuit), `List`, `Dict`, `Tuple`, `Set`, `Subscript`, `Slice`,
  `ListComp`, `SetComp`, `DictComp`, `GeneratorExp` (eager), `FunctionRef`,
  `BuiltinRef` (`f = int`), `IndirectCall`, `ModuleCall`, `ModuleAttr`,
  `GetAttr` (Datenattribut), `MethodCall { args, keywords }`, `Closure`
  (Wert), `Call { callee: Function(id) | Builtin(b), args }`.
- Statements: `Expr`, `Assign`, `AugAssign`, `If`, `While`, `ForRange`,
  `ForEach`, `Unpack`, `SetSubscript`, `Closure` (nested def),
  `ClassDef { target, name, base, methods }`, `SetAttr`, `Return`, `Break`,
  `Continue`, `Pass`, `Raise`,
  `Try { body, handlers, else_body, finally_body }`.
- `Handler.exc == None` ist ein bare `except:` (fängt jeden Tag). `else` läuft
  nur nach fehlerfreiem Body; `finally` läuft auf jedem Pfad (normal, `return`,
  `break`, `continue`, Exception) und kann die vorherige Aktion überschreiben
  (Return/Break/Continue suppressen eine pending Exception; eine neue Exception
  im finally ersetzt die alte). Exception-Chaining ist nicht implementiert.
  `except*` wird nicht unterstützt.
- `ForEach` iteriert Listen/Strings/Dicts über die Runtime-Iteratoren;
  `ForRange` bleibt die syntaktisch spezialisierte `range(...)`-Form (seit
  Phase 2.7 ist `range` zusätzlich ein Wert). Negative Schrittweite ist
  erlaubt, Schritt `0` ist ein Laufzeit-`ValueError`.
- `Raise { exc: None, msg: None }` ist bare Re-Raise.
- **Eager-Lowering:** `yield value` in einer Funktion wird zu
  `$gen.append(value)` plus Rückgabe der Liste; `await` ist der Operand;
  `with`/`async with` werden zu nested `try/finally` mit
  `__enter__`/`__exit__`; augmentierte Subscript-Zuweisung
  (`a[i] += x`) wertet Objekt und Index genau einmal in `$aug`-Locals aus.
- **User-Module:** Importe emittieren keine IR-Statements; der Loader ersetzt
  sie durch `FunctionRef`s auf die exportierten Funktionen des Zielmoduls.
  `__name__` ist ein optionaler Modulbody-Local.

Typen: `Int, Float, Bool, Str, Bytes, None, Any, Never, List, Dict, Tuple,
Set, Range, Function`.

## 6. Typinferenz (Frontend)

- Unifikationsbasiert über Typvariablen; keine Polymorphie.
- Literale/Operatoren erzeugen Constraints; `int`/`float` unifizieren zu `float`
  (Widening), `bool` ist mit `int` numerisch kompatibel.
- Funktionssignaturen werden aus Rumpf und Aufrufstellen abgeleitet; nicht
  auflösbare Variablen werden `Any`.
- Widersprüche (z. B. `1 + "x"` ohne `Any`) sind Diagnosen, keine Panics.
- Annotationen (`def f(x: int) -> int`) werden respektiert und geprüft.
- Rückgabetyp: `None`, wenn kein `return` mit Wert existiert; `Never` bei
  ausschließlich werfendem Rumpf. Generatorfunktionen liefern `List`.
- Der Modulkörper ist `FunctionId 0` mit `ret_ty = None`.
- Container sind unparameterisiert (Elemente `Any`). `len`, `str`, Indexing
  und `for` akzeptieren sie; Index-Typ muss bei `List`/`Str`/`Bytes` `Int`/
  `Bool` sein (bei `Dict` beliebig). `==`/`!=` auf gleichen Containertypen
  ergibt `Bool`; Ordnungsvergleiche auf Containern werden abgelehnt, bis die
  Runtime sie implementiert.
- `Set` ist unparameterisiert; `In`/`NotIn` → Bool mit denselben Regeln wie
  §3.3. `range()` → Range, `list()` → List, `dict()` → Dict, `sum()` → Int
  bei Range sonst Any, `enumerate`/`zip`/`map`/`filter` → List.
- Comprehensions: ListComp → List, SetComp → Set, DictComp → Dict,
  GeneratorExp → List (eager). Targets binden synthetische Locals.
- Name/Lambda → Function; indirekter Call: Callee Function/Any, Ergebnis Any;
  `f = int` ist eine Funktion (`BuiltinRef`). Closures erfassen ausschließlich
  vorhandene Locals; `capture_count` wird aus der ersten Verwendung im
  verschachtelten Body abgeleitet.
- Module/Imports: native Modul-Attribute/Calls werden über
  `pyx_ir::stdlib` typisiert; User-Modul-Importe binden exportierte
  Funktionssignaturen. Importe sind global sichtbar, müssen aber auf
  Modulebene stehen.
- Klassen: `class` bindet einen dynamischen Klassenwert (kein statischer
  Instanztyp); Methodenaufrufe über Instanzen werden dynamisch geprüft,
  Aufrufe mit `Any` fallen auf Runtime-Checks zurück.

## 7. Speicher & GC

Phase 1/2.0 allokierte Heap-Objekte über `Box::into_raw` und leakte sie. Seit
Phase 2.1/2.15 gibt es einen **konservativen, konkurrierenden Mark-&-Sweep-
Collector als Interim**:

- Jede Heap-Allokation wird in einer Thread-lokalen Registry (Objektliste +
  Adress-Set) registriert und mit einer Atomfarbe WEISS/GRAU/SCHWARZ
  versehen.
- Roots: konservativer Scan des nativen Stacks (Grenzen via
  `pthread_getattr_np`), eines Register-Dumps (x86_64/aarch64) und der
  pending Exception. Getaggte Heap-Werte werden nur akzeptiert, wenn ihre
  Objektadresse exakt in der Registry liegt; Fehltreffer konservieren höchstens.
- Der Register-Dump erfolgt in **einem** `asm!`-Block, der alle
  General-Purpose-Register (x86_64: inkl. `rsp`; aarch64: `x0..x30` + `sp`)
  in einen lokalen Puffer schreibt und der danach komplett als Roots markiert
  wird. Wählt der Compiler ein Register für den Pufferzeiger, hat er dessen
  vorherigen Live-Wert zuvor auf den Stack gespült; der Stack-Scan deckt ihn
  ab.
- Stack-Wörter werden per Inline-Asm geladen (x86_64 `mov`, aarch64 `ldr`),
  nicht via `ptr::read_unaligned`: Stack-Slots oberhalb von `sp` können
  uninitialisiert sein, und ein Maschinenload ist für den Compiler opak.
  Andere Architekturen fallen auf `read_unaligned` zurück; dort akzeptiert der
  Interim-Collector diese theoretische UB-Lücke (keine unterstützten Ziele).
- Mark traversiert `List`-Elemente, `Dict`-Keys/Values, `Iter.target`,
  Captures/Basisklassen/Instanz-Attribute und alle übrigen getraceten Kinder;
  Sweep gibt unmarkierte Objekte frei und kompaktiert die Registry.
- Ein Shared-Heap-Zyklus hat zwei kurze Stop-the-World-Fenster: initialer
  Root-Snapshot (Stacks, Register, Pending, Thread-Roots) und finaler
  Root-Rescan. Dazwischen markiert ein Collector-Thread konkurrierend; der
  Sweep läuft nach dem Rescan ebenfalls konkurrierend (White-Partition kurz
  unter Heap-Lock, `Box`-Freigabe außerhalb, neue Allokationen bleiben
  geschützt).
- **Insertion-Barrieren** pro neuer Kante (list/dict/set/queue/instance attr)
  markieren das neue Kind, wenn der Parent bereits SCHWARZ ist;
  Container-Mutation und Marker serialisiert ein Graph-Lock pro Payload. Im
  local-Modus entfällt das Lock.
- Auslösung bei Wachstum seit der letzten Sammlung: `live + PYX_GC_THRESHOLD`
  (Default 10000), nicht bei absoluter Objektzahl. `PYX_GC_STRESS=1` sammelt
  vor jeder Allokation. `PYX_GC_STATS=1` meldet Pause-, Mark- und Sweep-Zeit
  sowie Freigaben.
- `gc/alloc.rs` verwaltet feste `PyxObj`-Zellen in Slabs (64er-Chunks,
  Free-List, Alignment, Reuse/Stats); variable Payload-Groessenklassen sind
  eine eigene Heap-Architekturphase.
- `pyx_gc_collect()` erzwingt eine Sammlung; `pyx_gc_objects()` liefert die
  Live-Objektzahl. `pyx_runtime_init()` setzt Heap und Exception-Zustand zurück.

Das ist bewusst ein **Interim**: konservatives Scannen kann False Positives
konservieren; der finale Root-Rescan ist STW, die White-Partition läuft kurz
unter dem Heap-Lock. P2-05 (eigener Größenklassen-Allokator), P2-06/P2-07
(präzise Shadow-Stack-Roots, feineres Safepoint-Polling) bleiben offen. Die
`unsafe`-Policy: Die GC-Scan-/Free-Pfade sind in `pyx-runtime/src/gc/`
gekapselt und begründen ihre Invarianten dort.

### 7.1 Concurrency (P2-20)

**Kein GIL.** Seit Phase 2.12 gibt es einen globalen Shared-Heap-Modus
(Heap-Migration beim ersten Thread), Thread-Registry und native
`_thread`-Primitive. Seit Phase 2.13/2.15 sammelt ein **konkurrierender
Mark-&-Sweep-Collector** auch während Threads laufen: Der Codegen ruft
`pyx_safepoint()` an Funktionseingängen und (seit Phase 2.15 nur jede 64.
Loop-Iteration) an Loop-Backedges; jeder Thread registriert Stack-Grenzen,
gespeicherte SP/Register, Thread-Roots und einen Pending-Exception-Spiegel.
Der Collector setzt `GC_REQUESTED` nur für Root-Snapshot und finalen
Rescan/Sweep, wartet dort auf geparkte/exited Threads und markiert dazwischen
konkurrierend mit Insertion-Barrieren. Der Heap-Lock wird erst nach dem
Parken genommen, `GC_LOCK` nur per try_lock. Blockierende
Locks/join/sleep/Queue-/Socket-Waits parken den Thread während des Wartens;
Container-Mutationen und Marker serialisiert ein Graph-Lock. Sweep und
Markierung laufen konkurrierend. Mutierende Container müssen von User-Locks
geschützt werden; ungeschützte Mutationen sind Datenraces. Safepoints kosten
weiterhin Durchsatz (Baseline `threads_serial` AOT ca. 40–45 ms).

## 8. Toolchain / CLI / uv (Phase 3)

Die CLI ist eine einzige Binary. Unterbefehle (vollständige Hilfe via
`pyx help`):

`new`, `init`, `check`, `run`, `build`, `test`, `fmt`, `lint`, `clean`,
`add`, `remove`, `sync`, `lock`, `doctor`, `help`, `--version`.

- `new`/`init` erzeugen `main.py`, `pyproject.toml` und `.gitignore`;
  existierende nicht-leere Dateien werden ohne `--force` nicht überschrieben.
- `test` führt `test_*.py`/`*_test.py` mit `pyx run` aus; Erfolg = Exit 0
  (keine pytest-Assertions). `--json` liefert eine Ergebnisliste.
- `fmt` normalisiert Trailing-Whitespace und genau einen finalen Newline
  (kein AST-Formatter); `--check` schreibt nicht. `lint` typprüft alle
  `.py`-Dateien; `--json` aggregiert Diagnosen.
- `clean` entfernt `.pyx/cache` und `target/pyx-tmp` (`--dry-run`).
- `add`/`remove`/`sync`/`lock` sind ein **Passthrough** an `uv`
  (`PYX_UV` überschreibt das Executable). PyX interpretiert weder Specs noch
  `pyproject.toml`; der uv-Exitstatus wird zurückgegeben.
- Der Build-Cache (§4.1) ist implementiert; `--no-cache` umgeht ihn.

`uv` bleibt zunächst externe Abhängigkeit: PyX nutzt uv für Resolution,
Lockfiles und Venvs, injiziert aber **noch nicht** den PyX-Interpreter
(`.python-version`, Venv-Marker, `sys.executable`-Äquivalent). Ein eigener
Fork ist nur bei einem architektonischen Blocker vorgesehen. Eigene Dateien:
`pyproject.toml` (kompatibel), `pyx.lock` (optional zusätzlich), Build-Cache
unter `.pyx/cache/`.

## 9. C-API-/ABI-Strategie (Phase 4)

Optionen: (a) CPython einbetten (`libpython`, gleiche ABI), (b) C-API-Shim auf
PyX-Objekten (ABI3/limited API), (c) explizit begrenzte Kompatibilität.

**Status: P4-04 ist blockiert.** Die Umgebungsprobe (2026-10-24) fand
`python3` 3.12.3, aber kein `python3-config`, kein `Python.h` und keine
linkbare `libpython3.12.so`/`.a` (nur die Laufzeit-SO
`libpython3.12.so.1.0`). Ohne Entwicklungs-Header lässt sich weder ein
Embedding- noch ein ABI3-Smoke-Test bauen. Details, Optionen und der
Spike-Plan stehen in [`C_API.md`](C_API.md); es gibt **keine** erfundenen
Messungen und keine getroffene Entscheidung. Der Spike entscheidet über
Objektmodell, Refcounting und BigInt/Descriptor-Design, bevor diese
eingefroren werden.

## 10. Verifikation und dokumentierte Grenzen

- `scripts/verify.sh`: fmt-check, clippy (`-D warnings`), Build,
  `cargo test --workspace`, E2E-Beispiele (JIT **und** AOT), Negative-Tests
  (Diagnosen/Tracebacks), GC-Threshold/Stress.
- Stand 2026-10-24: **721 Tests** (233 Frontend, 329 Runtime inkl. 169
  ABI-Integrationstests, 11 Codegen, 3 IR, 145 CLI/E2E). Die E2E-Abdeckung
  enthält u. a. Formatierung, Klassen, Closures, Generatoren/async,
  User-Module, `argparse`/`defaultdict`/`re`, Socket-Echo, Pathlib,
  Build-Cache und uv-Passthrough.
- E2E vergleicht stdout/Exit-Code von `pyx run` und AOT-Binary; negative Tests
  prüfen Diagnosen und Tracebacks (stderr, Exit-Code).
- Keine stillen Fehlcompile: nicht unterstützte Syntax → Diagnose.

**Grenzen (bewusst, dokumentiert):**

- **Sprache:** kein BigInt (i64), keine Decorators, Pattern Matching,
  Metaclasses, Descriptor-/`__dunder__`-Protokolle, `except*`,
  Exception-Chaining, `bytearray`/`memoryview`; kein Slice-Assignment; keine
  Klassen-Attribute/Multiple Inheritance; Klassen-Namespace unveränderlich.
- **Closures:** by value zum Definitionszeitpunkt; rekursive verschachtelte
  Funktionen und Closures in Lambdas sind Diagnosen. Kein Zell-/Mutable-
  Capture.
- **Generatoren/async:** eager (Liste) bzw. sequenziell ohne Coroutinen,
  Tasks oder Event-Loop; `yield from`/`yield`-Ausdrücke/`return value` im
  Generator und echte `asyncio`-APIs fehlen.
- **Module:** nur native Module und Geschwister-`.py`-Dateien; keine Pakete,
  Punkte/relativen Imports, Top-Level-Seiteneffekte in importierten Modulen,
  keine Runtime-Modulobjekte/`sys.modules`.
- **Stdlib:** kuratierte Teilmenge (siehe §3.3); `re` nutzt die Rust-
  `regex`-Syntax, `socket` nur blockierendes TCP, `argparse` ohne
  `SystemExit`, `random` nicht CPython-reproduzierbar.
- **GC:** konservativ, keine präzisen Roots/Stack-Maps, kein
  Größenklassen-Allokator; STW-Root-Fenster. P2-05/06/07 offen.
- **Concurrency:** kein GIL; ungeschützte Mutationen geteilter Container sind
  Datenraces. Funktionsregistry für spawn/Pool nur nach Name/Arity
  (Duplikate/Lambdas/native Targets abgelehnt); Queue-`maxsize` geht im
  Spawn-Payload verloren.
- **Datei-I/O:** UTF-8-Text und Bytes, kein `+`-Modus, `newline`/`errors`
  ignoriert; CSV vollständig im Speicher.
- **Plattform:** Linux x86_64 ist das verifizierte Ziel; aarch64-Pfade sind
  vorhanden, aber nicht CI-verifiziert. macOS/Windows und Cross-Compilation
  fehlen. C-API/C-Extensions sind blockiert (P4-04).
