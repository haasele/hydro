# PyX — Architecture

As of: **2026-10-25**. Phase 1 is complete; Phase 2 is largely
implemented (1377 tests, `scripts/verify.sh` ALL GREEN). This document is
the **normative contract** between the crates. Changes to ABI/IR only with an
update of this document and of all consumers. The C API decision (P4-04,
option B: PyX-native ABI layer without `libpython`) and its gates are
documented in [`C_API.md`](C_API.md).

## 1. Vision

PyX is a standalone Python runtime with an AOT compiler and toolchain
(`pyx`), conceptually Bun for Python. CPython is not the standard runtime.
One goal is broad Python compatibility, initially via a clearly
defined subset (see `docs/TODO.md`).

**Supported:** tagged values, containers, functions/closures, classes
(single base), comprehensions, f-strings/formatting, exceptions,
file I/O, user modules and packages, native stdlib modules, lazy generators
and coroutines
with a cooperative event loop, threads/processes/pool, a conservative
concurrent mark-&-sweep GC.

**Deliberately not (as of now):** BigInt, async generators/comprehensions, the
complete CPython stdlib,
C extensions/C API, decorators, pattern matching, metaclasses,
`bytearray`/`memoryview`, precise GC roots and a dedicated
size-class allocator. Details in §10 and `docs/TODO.md`.

## 2. Components & Data Flow

```
Source .py (+ imported modules and packages)
  │  rustpython-parser (Python grammar, AST)
  ▼
pyx-frontend ── Lowering ──► typed IR (pyx-ir) ──► pyx-codegen
  │ Type checking/inference                      │
  ▼                                              ├─ JIT (pyx run)
Diagnostics (file:line:col)                      └─ Object + cc link (pyx build)
                                                        │
                                                  pyx-runtime (libpyx_runtime.a, C ABI)
```

| Crate | Responsibility | Dependencies |
|---|---|---|
| `pyx-ir` | Typed HIR; the only frontend↔codegen contract; central stdlib specification | – |
| `pyx-frontend` | Parsing, module loader/imports, lowering, type inference, diagnostics | `pyx-ir`, `rustpython-parser` |
| `pyx-runtime` | Value model, operators, builtins, exceptions, GC, file I/O, modules, concurrency; `rlib` + `staticlib` | `regex` et al. |
| `pyx-codegen` | Cranelift codegen, JIT and AOT path | `pyx-ir`, `pyx-runtime`, Cranelift |
| `pyx-cli` | `pyx` binary (project, compiler, tests, formatter, lint, uv passthrough) | `pyx-frontend`, `pyx-codegen` |

### 2.1 Directory Layout

Since the completion slice the crates are internally split into thematic directories
(full tree in [`LAYOUT.md`](LAYOUT.md)):

```
crates/pyx-ir/src/{model.rs, ir.rs, stdlib/}
crates/pyx-frontend/src/{lib.rs, diag.rs, lower/, sema/}
crates/pyx-codegen/src/{lib.rs, backend/, lower/}
crates/pyx-runtime/src/{abi.rs, core/, gc/, modules/, concurrency/, support/, csv.rs}
crates/pyx-cli/src/{main.rs, cli/, cli/commands/}
```

`pyx-ir` remains logic-free (contract structures + type tables). Runtime code
lives exclusively in `pyx-runtime`; `pyx-codegen` only calls
`pyx_runtime::abi` symbols.

## 3. Runtime ABI (Normative)

All values are `i64` (`PyxValue`). All functions are `extern "C"`,
`#[no_mangle]`, in the `pyx_runtime::abi` module, and **must not** panic
(user error → pending exception; internal error → `process::abort`).

### 3.1 Tagging

| Bits (`v & 7`) | Meaning |
|---|---|
| `000` | `int`: value = `v >> 3` (arithmetic, i64) |
| `001` | Special: `None = 1`, `False = 9`, `True = 17` (payload `0/1/2`) |
| `010` | Heap pointer: `ptr = v & !7`, points to `PyxObj` |
| Rest | reserved |

Ints, bools and None are immediates; floats/strings/containers/native objects
live on the heap. Heap objects have a header with `kind: u32` and
are aligned to 8 bytes.

Object kinds (stable `kind` values):

| Kind | Object | Kind | Object |
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
| 30 | `Instance` | 31 | `Fiber` |

Lists/dicts/sets are insertion-ordered (sets deterministically; Python
guarantees no order). `Iter` holds target and index. `Class` carries name,
optional base and an immutable method table; `Instance` carries the
class and a mutable attribute table. `Fiber` is a generator
or coroutine object (`kind`, own stack, architecture context, state,
result/error; see §3.3). On x86_64 the context is a
callee-saved register frame, otherwise a `ucontext_t`.

### 3.2 Symbols

```c
i64  pyx_none(void);                  // constant 1
i64  pyx_true(void);                  // 17
i64  pyx_false(void);                 // 9
i64  pyx_bool(i64 b);                 // Truthiness -> True/False
i64  pyx_int(i64 v);                  // identity (range check)
i64  pyx_str_new(const uint8_t* ptr, size_t len);   // copies UTF-8
i64  pyx_bytes_new(const uint8_t* ptr, size_t len);
i64  pyx_bytes_build(uint64_t argc, const i64* argv); // bytes(...)
i64  pyx_float_new(double f);
i64  pyx_binop(uint32_t op, i64 a, i64 b);          // incl. comparisons
i64  pyx_unop(uint32_t op, i64 a);
i64  pyx_truthy(i64 v);               // 1 if truthy, 0 if falsy (C style)
i64  pyx_print(uint64_t argc, const i64* argv);     // space-separated + '\n'
i64  pyx_len(i64 v);
i64  pyx_to_str(i64 v);
i64  pyx_to_repr(i64 v);                            // repr(v)
i64  pyx_hash(i64 v);                              // hash(v)
i64  pyx_to_int(i64 v);
i64  pyx_to_float(i64 v);
i64  pyx_abs(i64 v);
i64  pyx_all(i64 iterable);                         // all(): True/False
i64  pyx_format_value(i64 value, const uint8_t* spec, size_t spec_len,
                      uint8_t conversion);          // f-string field
i64  pyx_list_new(uint64_t argc, const i64* items);
i64  pyx_dict_new(uint64_t argc, const i64* keys, const i64* values);
i64  pyx_tuple_new(uint64_t argc, const i64* items);
i64  pyx_unpack(i64 value, uint64_t expected);      // None or ValueError
i64  pyx_set_new(uint64_t argc, const i64* items);
i64  pyx_set_from(i64 iterable);
i64  pyx_range_new(i64 start, i64 stop, i64 step);  // step != 0
i64  pyx_unpack_seq(i64 value, uint64_t expected);
                                    // sequence to bind (fresh list if needed)
i64  pyx_unpack_star(i64 value, uint64_t before, uint64_t after);
                                    // a, *rest, b = value: fresh list with
                                    // slot `before` holding the middle list
i64  pyx_list_from(i64 iterable);
i64  pyx_tuple_from(i64 iterable);  // tuple(iterable)
i64  pyx_dict_from(i64 iterable);
i64  pyx_sum(i64 iterable, i64 start);
i64  pyx_enumerate(i64 iterable, i64 start);
i64  pyx_zip(uint64_t argc, const i64* iterables);
i64  pyx_list_append(i64 list, i64 value);          // None or error
i64  pyx_set_add(i64 set, i64 value);
i64  pyx_dict_set(i64 dict, i64 key, i64 value);
i64  pyx_dict_merge(i64 left, i64 right);           // {**left, **right} copy
i64  pyx_function_new(const uint8_t* wrapper, uint64_t arity,
                      const uint8_t* name, size_t name_len, uint32_t flags);
i64  pyx_function_params(i64 fn, uint64_t count, const i64* names);
                                    // 2*count words [ptr, len, ...]:
                                    // user-visible positional parameter names
i64  pyx_function_meta(i64 fn, i64 full_entry, uintptr_t count,
                       const i64* names, const i64* kinds,
                       const i64* defaults, const i64* default_flags);
                                    // names: [ptr,len] pairs; kinds
                                    // 0..3; definition-time defaults
i64  pyx_call_kw(i64 fn, uintptr_t argc, const i64* argv,
                 uintptr_t kwargc, const i64* kw_argv);
i64  pyx_builtin_function(const uint8_t* name, size_t name_len); // f = int
i64  pyx_closure_new(const uint8_t* wrapper, uint64_t arity,
                     const uint8_t* name, size_t name_len,
                     uint64_t capture_count, const i64* captures,
                     uint32_t flags);
i64  pyx_class_new(const uint8_t* name, size_t name_len, i64 base,
                   uint64_t method_count, const i64* names, const i64* funcs,
                   const i64* kinds,
                   uint64_t attr_count, const i64* attr_names,
                   const i64* attr_values);
i64  pyx_instance_new(i64 class);   // allocate without __init__
i64  pyx_bind_class_kwargs(i64 mapping, const i64* names, uintptr_t count,
                           i64* slots, i64 kwarg_index,
                           const uint8_t* class_name, size_t class_len);
                                    // route **mapping into constructor slots
i64  pyx_exception_class(uint32_t tag);  // cached native class, e.g. ValueError
i64  pyx_call(i64 fn, uint64_t argc, const i64* argv);
i64  pyx_call_star(i64 fn, uint64_t argc, const i64* argv, i64 star);
                            // fn(args..., *star); star is a list/tuple
i64  pyx_map(i64 fn, i64 iterable);                 // materialized list
i64  pyx_filter(i64 fn_or_none, i64 iterable);
i64  pyx_module_call(const uint8_t* module, size_t module_len,
                     const uint8_t* function, size_t function_len,
                     uint64_t argc, const i64* argv);
i64  pyx_module_call_kw(const uint8_t* module, size_t module_len,
                        const uint8_t* function, size_t function_len,
                        uint64_t argc, const i64* argv,
                        uint64_t kwargc, const i64* kw_argv);
                                    // [name, value, ...]; only specs with
                                    // dynamic keywords (functools.partial)
i64  pyx_module_get(const uint8_t* module, size_t module_len,
                    const uint8_t* name, size_t name_len);
i64  pyx_module_value(const uint8_t* module, size_t module_len);
i64  pyx_deferred_import(const uint8_t* module, size_t module_len,
                         const uint8_t* member, size_t member_len);
                     // resolves an already-initialized module global when
                     // possible; otherwise raises RuntimeError
                     // (empty member = module alias)
i64  pyx_global_get(const uint8_t* module, size_t module_len,
                    const uint8_t* name, size_t name_len);
i64  pyx_global_set(const uint8_t* module, size_t module_len,
                    const uint8_t* name, size_t name_len, i64 value);
i64  pyx_subscript(i64 obj, i64 index);
i64  pyx_subscript_set(i64 obj, i64 index, i64 value);
i64  pyx_slice(i64 obj, i64 start, i64 stop, i64 step);
i64  pyx_slice_set(i64 obj, i64 start, i64 stop, i64 step, i64 value);
                     // list slice assignment; returns None, errors via pending
                     // exception
i64  pyx_getattr(i64 obj, const uint8_t* name, size_t name_len); // data attribute
i64  pyx_hasattr(i64 obj, const uint8_t* name, size_t name_len); // True/False
i64  pyx_setattr(i64 obj, const uint8_t* name, size_t name_len, i64 value);
i64  pyx_method(i64 obj, const uint8_t* name, size_t name_len,
                uint64_t argc, const i64* argv);
i64  pyx_method_kw(i64 obj, const uint8_t* name, size_t name_len,
                   uint64_t argc, const i64* argv,
                   uint64_t kwargc, const i64* kw_argv); // [name, value, ...]
i64  pyx_method_star(i64 obj, const uint8_t* name, size_t name_len,
                     i64 star, uint64_t kwargc,
                     const i64* kw_argv);  // obj.m(*list_or_tuple, k=v)
i64  pyx_iter_new(i64 obj);
i64  pyx_iter_next(i64 iter, i64* out);             // 1=value, 0=end
i64  pyx_iter(i64 obj);                             // iter(obj)
i64  pyx_next(i64 obj, i64 default, i64 has_default); // next(obj[, default])
i64  pyx_generator_new(const uint8_t* entry, uint64_t argc, const i64* argv);
i64  pyx_coroutine_new(const uint8_t* entry, uint64_t argc, const i64* argv);
i64  pyx_yield(i64 value);                          // suspends the fiber
i64  pyx_yield_from(i64 iterable);                  // delegates iteration
i64  pyx_await(i64 value);                          // drives a coroutine
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
i64  pyx_raise_from(uint32_t tag, i64 msg, i64 cause);
i64  pyx_raise_value_from(i64 exc, i64 cause);
int  pyx_exc_matches(uint32_t handler_tag);
i64  pyx_uncaught(void);
void pyx_runtime_init(void);
void pyx_register_function(const uint8_t* name, size_t name_len,
                           const uint8_t* wrapper, size_t arity); // spawn/Pool
int32_t pyx_spawn_child(void);        // child mode after exec (-1 = normal)
```

`op` codes (`pyx_binop`): `0 Add, 1 Sub, 2 Mul, 3 Div, 4 FloorDiv, 5 Mod,
6 Pow, 7 Eq, 8 Ne, 9 Lt, 10 Le, 11 Gt, 12 Ge, 13 In, 14 NotIn, 15 Is,
16 IsNot, 17 BitAnd, 18 BitOr, 19 BitXor, 20 Shl, 21 Shr`.
`pyx_unop`: `0 Neg, 1 Pos, 2 Not, 3 Invert` (`~` is int/bool only).

`flags` in `pyx_function_new`/`pyx_closure_new`: Bit 0 = `async`, Bit 1 =
generator. If either of the bits is set, the generated wrapper calls
`pyx_coroutine_new`/`pyx_generator_new`; `argv` contains the captures
followed by the call arguments. The fiber starts only on the first
`pyx_iter_next` (generator) or `pyx_await`/`asyncio.run` (coroutine).

Exception tags: `0 ValueError, 1 TypeError, 2 ZeroDivisionError,
3 RuntimeError, 4 OverflowError, 5 KeyboardInterrupt, 6 IndexError,
7 KeyError, 8 StopIteration, 9 AttributeError, 10 OSError, 11 GeneratorExit,
12 Exception, 13 BaseException, 14 AssertionError, 15 FileNotFoundError`.
Names are 1:1 as in Python; `IOError` is an alias for `OSError`.
`except Exception` matches every tag except 5/11; `except BaseException`
matches every tag. `raise`/`except` resolve tags from names directly, while
an unqualified exception name used as a value lowers to
`pyx_exception_class(tag)`, which returns one cached empty class object per
tag. Explicit causes are stored on the exception and traced by the GC.

**User exception classes are a documented limitation.** `class ReprError(
Exception): ...` creates an ordinary class (its instances are class
instances, not native exception objects). `raise ReprError("msg")` is not
dropped: the frontend lowers it to `RuntimeError` whose message is
`"ReprError: msg"` (the message is rendered with `str()`), keeping an
explicit `from` cause; a bare `raise ReprError` uses `"ReprError"`. A later
`except ReprError:` is rejected at compile time (handlers only accept
built-in kinds), so the fallback never matches a user handler.

### 3.3 Semantics

#### Numerics, Strings, Formatting

- `int`/`int` arithmetic in i64; overflow → `OverflowError` (documented
  limit). No BigInt.
- `/` always returns `float`; division by 0 → `ZeroDivisionError`.
- `//`/`%` follow Python (floor division, sign of the divisor).
- `+` on `str` is concatenation; `==`/`!=`/`<`… on strings lexicographic.
- Mixed `int`/`float` → `float`; `bool` is numerically `0/1`.
- `print` calls `str()` per argument, separates with spaces, terminates with
  `\n`.
- `hash(v)` reuses the dict-key hash (so `1`, `1.0` and `True` hash equally)
  mapped into a non-negative tagged int; unhashable values raise `TypeError`.
  The value is process-deterministic but not CPython-reproducible.
- Unary `~` is defined for `int`/`bool` (`~x == -x - 1`); any other operand
  raises `TypeError`. `NotImplemented` is a reserved singleton constant usable
  in expressions (e.g. `return NotImplemented`); no `__eq__` fallback protocol
  consumes it yet.
- `str % args` is printf formatting as soon as the left side is a string
  (`%s`, `%d`, `%f`, `%x`, flags/width/precision, `%%`, tuple arguments).
- f-strings: `{expr}`, `{expr!s}`, `{expr!r}`, `{expr!a}` and format specs
  (`:.2f`, `:04d`, `:#x`, alignment/fill/sign/grouping).
  Codegen calls `pyx_format_value`; conversion and spec are applied before
  formatting. Invalid specs → `ValueError`, wrong types → `TypeError`.
- `str.format(...)` supports positional/named fields, index/attribute
  paths and format specs. The spec logic lives centrally in
  `pyx-runtime/src/core/format.rs` and is shared by f-strings, `str.format` and
  `%`.
- `bytes` (Phase 2.19): literals `b"..."`, `bytes()` constructor, indexing
  (int 0..255), slicing, iteration, `+`/`*`, comparisons/equality/hash,
  subsequence membership and core methods (`decode`, `hex`, `split`, `join`,
  `strip`, `replace`, `upper`/`lower`, ...); `str.encode` returns bytes.
  No `bytearray`/`memoryview`.

#### Containers

- Lists/dicts: indexing with negative indices; `IndexError` outside
  the range; `KeyError` with repr(key) on a missing dict key; `len` counts
  elements; `for` iterates lists/strings (Unicode scalars), dict keys in
  insertion order and generator objects;
  `str(list)`/`str(dict)` use the Python representation
  (`[1, 'a']`, `{'k': 1}`); `==`/`!=` are structural (dicts order-
  independent); `list + list` concatenates.
- Slices (`s[a:b:c]`) follow `slice.indices` (negative indices, defaults,
  `step == 0` → ValueError) and return new strings (Unicode scalars) or
  lists; dicts are not sliceable (`TypeError`). List slice assignment
  (`xs[a:b:c] = iterable`) splices for `step == 1`, replaces the visited
  positions for extended slices (length mismatch → ValueError) and rejects
  immutable `str`/`bytes`/`tuple` targets (`TypeError`).
- Tuples: immutable, indexing/slicing like lists, `len`, iteration,
  structural equality, `tuple + tuple` and `tuple * int`; representation
  `(1, 'a')`, singleton `(1,)`, empty `()`. Tuples of hashable elements are
  allowed as dict keys. Unpacking (`a, b = pair`) accepts lists/tuples with
  exact length; `dict.items()` returns a list of `(key, value)` tuples
  (documented deviation from the view). Nested/starred unpacking is a
  diagnostic.
- `in`/`not in`: `list`/`tuple` via Python equality, `dict` over keys,
  `str` as substring (the left operand must be `str`), `set` over elements;
  other right operands → TypeError. `set` literals deduplicate but preserve
  insertion order, support `len`/iteration/truthiness/
  structural equality and `repr` (`{1, 2}`, empty `set()`); unhashable
  elements → TypeError.
- `range(...)` is a value (`range` object, iteration/len/membership/repr);
  `list()`/`dict()`/`sum()`/`enumerate()`/`zip()` are builtins.
  `enumerate`/`zip` materialize lists of tuples.
- Comprehensions (Phase 2.8): `[e for ...]`, `{e for ...}`, `{k: v for ...}`
  and generator expressions; multiple `for` clauses and `if` filters are
  evaluated nested, tuple targets unpacked. Generator expressions are
  still **materialized eagerly as a list** (documented deviation;
  function generators are lazy since Phase 2.32); comprehension variables
  are synthetic locals and do not leak.
- Dicts store their insertion order in an entry list plus a
  hash index over the key hashes (`DictData`). Buckets are verified with
  Python equality (first position, last value); numeric
  keys are int/float/bool-equivalent. Lookup/insert are therefore O(1) on
  average; `pop` reindexes. Container mutation during an iteration is
  index-based and undefined.

#### Functions, Closures, Classes

- Named functions and lambdas are values (`FunctionRef`); indirect calls
  go through `pyx_call` (wrapper with argv). Lambdas are parameter-only; an
  access to outer names inside a lambda is a diagnostic.
- Builtins are values as well: `f = int`, `map(f, xs)` etc. materialize
  a `FunctionBody::Builtin` object via `pyx_builtin_function`.
- **Closures are by value** (Phase 2.28): `def inner` inside a function
  creates `pyx_closure_new` with the *current values* of the referenced
  locals. Later changes of the outer variable are invisible to an already
  created closure; every execution creates its own captures (also in
  loops). Captures are passed as leading parameters to the wrapper
  (`capture_count` in the signature). Recursive nested functions and
  closures in lambdas are diagnosed.
- **Classes** (Phase 2.27): `class Name(Base):` with at most one simple
  base. Allowed are methods, `__init__`, class attributes (`x = value`,
  `x: type = value`, including `__slots__`, which is accepted and ignored),
  bare field declarations (`name: type`), `pass` and a docstring; class
  attributes are evaluated when the class statement runs and the namespace
  is immutable after creation. `@property`, `@classmethod` and
  `@staticmethod` select the method binding kind (property setters/deleters
  are ignored; assignment stores an instance attribute); the no-op decorators
  `lru_cache`, `cache`, `wraps`, `abstractmethod`, `overload` and
  `runtime_checkable` (including parametrized forms) are accepted without
  being evaluated. `@dataclass` (the imported `dataclasses.dataclass` name)
  and `class X(typing.NamedTuple)` synthesize an ordinary
  `__init__(self, <annotated fields...>)` that assigns every declared field
  in order unless the class defines its own. `Name(...)` creates
  an instance and calls `__init__` (via the class/base chain) with the
  receiver selected by the method kind. Methods are resolved dynamically via
  `pyx_method` by receiver type and class chain; attributes via
  `pyx_getattr`/`pyx_setattr`. Instances expose `__class__`; classes expose
  their frozen attributes and `__name__`. Zero-argument
  `super().__init__(...)` calls the direct base initializer; not supported:
  other class decorators, metaclasses, `class` keywords, multiple inheritance,
  other `super()` forms, `__repr__`/descriptor protocols. Instances render
  deterministically as `<ClassName object>` (no heap address), classes as
  `<class 'Name'>`.
- **Attributes** (`pyx_getattr`): `pathlib.Path` (`name`, `stem`, `suffix`,
  `parent`, `path`), files (`name`, `mode`, `closed`, `encoding`),
  `threading.Thread` (`name`, `ident`, `daemon`) and
  `multiprocessing.Process` (`name`, `pid`, `exitcode`, `daemon`) provide
  data attributes. For native objects, names without a call are data
  attributes; methods must be called. Unknown names → `AttributeError`.
  `pyx_setattr` allows only instance attributes; classes are immutable.
- **Method keywords** (`pyx_method_kw`): native methods with named
  parameters accept keywords. Implemented are `str.split`/
  `replace`/`startswith`/`endswith`, `pathlib.Path.mkdir`/`open`/
  `read_text`/`write_text`/`read_bytes`/`write_bytes`/`glob` and
  `argparse` methods; e.g. `Path.mkdir(parents=True, exist_ok=True)`.
  Native **constructors** (e.g. `Thread(target=..., args=...)`,
  `queue.Queue(maxsize=...)`) bind keywords via their
  `FunctionSpec.params`. User **functions** bind keywords at compile time
  against their parameter names (`md_table(rows, left=2)`), diagnose
  unknown/duplicate names and fill omitted trailing parameters from their
  literal defaults (`def bar(f, width=18)`); a first-class call still needs
  the full positional arity. A non-literal default (e.g.
  `color_system=ColorSystem.TRUECOLOR`) is accepted so library signatures
  compile; parameter metadata records `None`, so a direct call that omits it
  receives `None` instead of the source default (documented limitation).
  `max`/`min` pass `key=` as the runtime's
  trailing key slot. `pyx_method_star` flattens one list/tuple operand
  (`t.add_row(*cells, style=style)`). Other native methods reject keywords
  (runtime/frontend diagnostic).

#### Generators and async

- **Generators** (Phase 2.32): A function with `yield` is compiled as a
  fiber (`core/fiber.rs`). The call creates a `Fiber` object with its
  own 128-KiB stack; on x86_64 the context switch is
  `global_asm!`-`pyx_context_switch` over the callee-saved registers `rsp`,
  `rbx`, `rbp`, `r12`–`r15` (`ret`-based, no syscalls); on other
  targets libc `ucontext` remains the fallback. The wrapper runs only on the
  first `pyx_iter_next`. `yield value`/bare `yield` suspend the fiber,
  `return` ends the iteration, exceptions are propagated on the next resume.
  Every object is independent; `for`, `list`, `sum`, `iter(x)` and
  `next(it[, default])` work on it. `send(v)` resumes the generator and
  returns the next yielded value; `x = yield v` evaluates to the sent value
  (`None` for `next`). `throw(exc)` raises an exception *instance* at the
  suspension point (PyX has no exception class objects yet, so only the
  one-argument, instance form is supported). `close()` injects
  `GeneratorExit`, so `finally` blocks run; a generator that yields during
  `close()` raises `RuntimeError("generator ignored GeneratorExit")`.
  Exhausted generators raise `StopIteration` from `send`/`throw`.
  `yield from iterable` delegates iteration (`pyx_yield_from`): each
  delegated item is yielded, `send`/`throw` are forwarded to a delegated
  generator, and `close()` closes the delegate first so its `finally` blocks
  run. The expression evaluates to the delegate's return value (`None` for a
  plain iterator). Delegating to a running generator raises `RuntimeError`.
  Not supported: async generators (`async def` + `yield` is a diagnostic) and
  async comprehensions. Generator expressions are eagerly materialized into
  an iterator (still not lazy)
  lists (documented deviation). Deviations: `send` into a plain-iterator
  delegate raises `AttributeError` naming the iterator as `'iterator'` (not
  e.g. `'list_iterator'`), and PEP 479 (converting a body-raised
  `StopIteration` into `RuntimeError`) is not implemented.
- **async** (Phase 2.32): `async def` creates a coroutine object;
  `await`/`pyx_await` drives it to completion (non-fiber values are
  returned unchanged). `asyncio.run(coro)` runs a cooperative
  single-thread event loop: `asyncio.sleep(secs)` suspends only the current
  fiber (blocking outside a loop), `asyncio.gather(*awaitables)` starts all
  tasks concurrently, returns the results in input order and propagates the
  first error. Tasks switch only at explicit suspension points; there is no
  GIL. `async for`/`async with` still use the synchronous
  lowering/protocol logic. Beyond that, `asyncio` offers only
  `run`/`sleep`/`gather`; real `__aiter__`/`__anext__` protocols and
  non-blocking I/O are missing.

#### `with` Protocol

`with a as x, b as y:` becomes nested `try/finally` blocks. Every
context expression is evaluated exactly once into a hidden `$withN` local,
whose `__enter__()` is bound to the `as` target. The
`finally` calls `__exit__(None, None, None)` on the same object, so that
`return`, `break`, `continue` and exceptions leave every object in reverse
order. The return value of `__exit__` is ignored:
exceptions are **never** suppressed. Files, `Pool` and user classes
implement the protocol; without `as`, `__enter__` still runs.

#### Imports, User Modules, `__name__`

- Native modules are specified via `pyx_ir::stdlib` (see below).
  `import X [as Y]` and `from X import a [as b]` bind eagerly at module
  level; imported names are visible inside functions. Imports nested in a
  function, class or compound body are **deferred**: the loader records them
  without loading the module, names that can still be linked to an
  already-visible dependency bind normally, and every other binding lowers to
  `DeferredImport`, which calls `pyx_deferred_import` and raises
  `RuntimeError` (`dynamic import ... is not supported`) when evaluated.
  `X.f(...)` →
  ModuleCall, `X.const`/`X.f` as a value → ModuleAttr. Native functions are
  materialized as function objects; calls/arity/errors go through the
  shared specification. A stdlib module used as a value lowers to
  `ModuleRef` and materializes a runtime module object, so
  `getattr(module, name)` works. User/extension module values remain
  diagnostics; unknown members are compile-time diagnostics.
  Module-level simple-name assignments that are read by a function are
  mirrored into a keyed global store (`GlobalSet`/`GlobalGet`,
  `pyx_global_set`/`pyx_global_get`); other module locals remain invisible
  inside functions. Imported units mirror every export because an importer may
  read any name; the entry module mirrors only names its own functions read
  (precomputed before lowering), so top-level loops do not emit a global store
  per iteration.
- **User modules and packages** (Phase 2.30; package loader extended in the
  P7 checkpoint): `import foo` / `from foo import f` resolve `foo.py` and
  packages (`foo/__init__.py`), including dotted names, submodules and
  relative imports. Search order: sibling files, the package-chain root,
  `PYX_SITE_PACKAGES`, the `VIRTUAL_ENV`/`CONDA_PREFIX` site-packages trees,
  then `PYTHONPATH`. The loader loads every unit **once per canonical path**
  (module cache at compile time), loads dependencies first and diagnoses
  cycles made only of module-level imports. Imported module bodies are
  compiled into the `Program` and run once as initializers, so import side
  effects execute in dependency order; an exception raised by an initializer
  is reported through the uncaught path before the next module or the entry
  body runs, so its pending error cannot be silently overwritten.
  Top-level functions, classes and mirrored module-level values are
  exported, while `from x import *` is not supported. Module-level bindings are also mirrored into the runtime global
  table, so importers read them regardless of lowering order. All units end
  up in one `Program`; JIT and AOT execute the same code (AOT embeds the
  modules into the binary).
- `__name__` is a module body local and is injected only if the unit
  reads it: `"__main__"` for the entry file, otherwise the module name.
  The entry file additionally binds `__file__` (its source path) and
  `__doc__` (the module docstring, or `None`); references from functions and
  methods are detected too. Each binding is an `Assign` plus the matching
  `GlobalSet`, so functions read it with `GlobalGet`.
  There is no general runtime module object or `sys.modules`; stdlib module
  values are a native `module` kind, and `errno` exposes a synthetic
  `__dict__` for compatibility.

#### Stdlib Modules

Central specification in `pyx_ir::stdlib` (`MODULES` in
`stdlib/mod.rs`): `math`, `io`, `functools`, `operator`, `time`, `sys`,
`random`, `platform`, `pathlib`, `statistics`, `json`, `csv`, `os`,
`os.path`, `_thread`, `threading`, `queue`, `multiprocessing`, `pickle`,
`argparse`, `collections`, `re`, `socket`, `asyncio`, `errno`, `unittest`,
`dataclasses`, `datetime`, `typing`, `abc`, `enum`, `types`, `itertools`,
`ast`, `fractions`, `importlib` and `__future__`.

- `json`: `dumps`/`loads` with CPython defaults (ensure_ascii, separators,
  escapes, surrogates, error classes, depth/cycle guards) as well as `dump`/`load`
  on file objects; the parser builds containers incrementally in a GC-safe way.
- `os`: `getpid`/`getppid`/`cpu_count`/`getcwd`/`getenv`, `name`/`sep`,
  `remove`/`rename`, `fork`/`waitpid`/`_exit`. `os.path`: `join`, `basename`,
  `dirname`, `exists`, `isfile`, `isdir`, `abspath`, `splitext`, `split`.
- `io`/`open`: UTF-8 text (`r/w/a/x`) and bytes (`rb/wb/ab/xb`); no
  `+` mode. `read`/`readline`/`readlines`/`write`/`writelines`/`flush`/
  `tell`/`seek`/`close`/line iteration; errors `OSError`, closed file
  `ValueError`.
- `csv`: `reader`/`writer` and `DictReader`/`DictWriter` (fieldnames),
  RFC-4180-like quoting/embedded newlines, `writerow(s)`/`writeheader`.
  Readers materialize; no dialect/restkey, CSV is not streaming.
- `argparse` (Phase 2.25): an `ArgumentParser` with positional arguments,
  long/short options, `type`, `default`, `required`, `choices`, `help`,
  `store_true`, `nargs="?"`; `parse_args` returns a `Namespace`. Errors
  are `RuntimeError` (no `SystemExit`). With no explicit argv it reads
  `sys.argv[1:]`, and `-h`/`--help` prints help and exits 0; an explicit
  argv list keeps `_help=True` without exiting.
- `collections.defaultdict`: insertion-ordered `(key, value)` pairs plus
  `default_factory`; a missing key calls the factory, inserts and returns;
  `None` factory → `KeyError`.
- `re` (Phase 2.26): wrapper around the `regex` crate with `search`, `match`,
  `fullmatch`, `findall`, `sub`, `split`, `escape`, `compile`; `Pattern` and
  `Match` are native objects with `group`/`groups`/`start`/`end`/`span`/
  `groupdict` (named groups are not retained, so named styles are absent) and
  bound method values (`m.span`). Syntax is Rust `regex` (no
  lookarounds/backreferences); invalid patterns → `ValueError`.
- `html`: native `escape`/`unescape` (the two helpers used by HTML
  exporters). `rich.status`: native `Status` context-manager shim for report
  progress; the remaining `rich` modules come from site-packages.
- `socket` (Phase 2.26): blocking TCP (`AF_INET`/`SOCK_STREAM`) via
  `std::net`: `socket`, `create_connection`, `bind`, `listen`, `accept`,
  `connect`, `send`/`sendall`, `recv`, `close`, `settimeout`, `gettimeout`,
  `shutdown`, `setsockopt`/`getsockopt`, `getsockname`/`getpeername`.
  Blocking calls park the thread for the GC. No TLS/UDP/`select`
  and no `socket.timeout` class (timeout → `OSError`).
- `random`: global SplitMix64 PRNG and seedable `Random(seed=...)` with
  `seed`/`random`/`randint`/`randrange`/`choice`/`uniform`/`shuffle`/
  `getrandbits`; `getrandbits(k)` returns a non-negative `k`-bit int and
  raises `OverflowError` for `k > 60` (documented tagged-int limit).
  Deterministic per seed, but **not CPython-reproducible**.
- `sys.stdout`/`sys.stderr`: small native text streams over fd 1/2 with
  `write(str)` (returns the character count, flushes), `flush()` and
  `isatty()`. They are fresh GC-safe objects per resolution (no PyX
  children) and expose no other attributes.
- `platform`: `system`/`machine`/`release`/`version`/`node`/`platform`/
  `python_version`, `python_implementation() == "PyX"`.
- `statistics`: `mean`/`fmean`/`median`/`pstdev`/`stdev` including error paths.
- `pathlib`: native `Path` (`Path()`, `cwd()`, `home()`) with
  `exists`/`is_file`/`is_dir`, `read_text`/`write_text`, `read_bytes`/
  `write_bytes`, `open`, `mkdir`, sorted `iterdir`/`glob` (`*`/`?`, no
  `**`), `joinpath`/`resolve`/`absolute`/`as_posix`, `/` join and
  data attributes `name`/`stem`/`suffix`/`parent`/`path`. `os.remove`/
  `rename` accept path objects.
- `asyncio`: see §3.3 "Generators and async" (only `run`/`sleep`/`gather`).

#### Methods

`pyx_method`/`pyx_method_kw` dispatch dynamically by receiver type.
`str`: `upper`, `lower`, `strip`, `lstrip`, `rstrip`, `split` (0/1 arg),
`join`, `replace` (2/3 args), `startswith`, `endswith`, `find`, `count`,
`isdigit`, `isalpha`, `isalnum`, `isspace`, `format`, `encode` (bytes).
`list`: `append`, `extend`, `insert`, `remove`, `pop` (0/1 arg), `clear`,
`reverse`, `copy`, `count`, `index`. `dict`: `get` (1/2 args), `keys`,
`values`, `items`, `pop` (1/2 args), `update`, `clear`. `tuple`: `count`,
`index`. `set`: `add`, `remove`, `discard`, `clear`, `copy`, `union`.
`keys`/`values` deliberately return lists (no views). Unknown methods →
`AttributeError`; attribute access without a call on native types is either
a data attribute (see above) or `AttributeError`. For `str`, `list`,
`dict`, `tuple`, `set`, `bytes` the frontend checks methods/arity statically;
`Any` receivers are checked at runtime.

#### File I/O and Concurrency

- Files: see `io`; `with` uses the `__enter__`/`__exit__` protocol.
- **No GIL.** On the first `_thread.start_new_thread` the heap switches to
  a global shared heap (existing objects are migrated). Threads
  receive IDs; `_thread.join` is a PyX extension. Locks
  (`_thread.allocate_lock`) are native `Lock` objects; `threading` provides
  `Thread`, `Lock`, `RLock` (owner/depth), `Event` and `queue.Queue`.
- `multiprocessing` provides fork-based `Process` objects, `os.fork`/
  `waitpid`/`_exit` and `multiprocessing.Queue` (socketpair, PYX1
  serialization). `spawn` starts a fresh process via `exec`
  (`pyx __spawn` resp. AOT binary); `pyx_main` registers all functions before
  the module body, a payload names the target name + args, and only that function
  runs in the child (queue FDs are inherited; no module body in the child).
  `Pool` uses per-worker private task/result queues for fork and spawn and
  implements `__enter__`/`__exit__` (terminate).
- `pickle.dumps/loads` uses its own PYX1 hex format that is not CPython-compatible
  (None/Bool/Int/Float/Str/List/Tuple/Dict/Set/Range/Bytes/
  function wrapper); cycle-free, depth 128. Functions are reconstructed by
  wrapper address/name only in the same, forked or spawn-
  registered address space; native objects/lambdas are
  rejected.
- Shared mutable containers require user locks; without a GIL,
  unprotected mutations are data races and not supported behavior.

### 3.4 Error Protocol

Error-capable runtime calls set a **pending exception** on failure and
return a dummy value. After *every* error-capable operation, codegen checks
`pyx_exc_pending()` and jumps to the next handler (or to the uncaught path).
An exception carries tag, message, function name and line at the time of the
`raise`. User exception classes have no runtime tag: `raise UserClass(msg)`
and `raise module.UserClass(msg)` lower to `RuntimeError` with a
`"<class name>: <message>"` string, and handlers for those classes (by name
or dotted name, including imported classes) match `RuntimeError`. This
handler matching is documented as approximate: every user class shares the
`RuntimeError` tag, so unrelated user-class raises are caught too.
`pyx_uncaught()` prints:

```
Traceback (most recent call last):
  File "test.py", line 7, in main
ZeroDivisionError: division by zero
```

and returns exit code `1`.

## 4. Backend Decision

**Cranelift 0.120** (pure Rust) for Phases 1–3; LLVM optionally later behind
a feature flag.

Rationale: no system LLVM/`llvm-config`/headers needed, fast builds,
JIT and object emission from one codebase, solid baseline optimization.
Costs: weaker optimization than LLVM (PGO/inlining, P5) and no
ORC-JIT tiering. Codegen remains encapsulated in `pyx-codegen`, so an
LLVM backend can later consume the same IR. The decision is documented in P0-11.

### 4.1 AOT Linking

`pyx build` produces an object file (`.o`) and links it with `cc` against
`libpyx_runtime.a` (staticlib of the workspace). The generated binary exports
`main(argc, argv) -> int`, which calls `pyx_main() -> int`. `pyx_main`
executes the module body (incl. embedded user modules) and returns the
`pyx_uncaught()` exit code on a pending exception, otherwise `0`. The binary
needs neither CPython nor `pyx`.

Runtime library resolution: `--runtime-lib <path>`, otherwise `PYX_RUNTIME_LIB`,
otherwise relative to the CLI executable (`target/<profile>/libpyx_runtime.a`).
If it is missing, a clear error message with a build hint.

The build cache lives under `.pyx/cache` and is a purely local
optimization. The key covers CLI/runtime version, target architecture/OS,
source text, all reachable `.py` modules (relative path + content) and the
identity of the runtime archives. `--no-cache` bypasses it; `pyx clean` removes
it. The cache is not a shared artifact store.

## 5. IR Contract

`pyx-ir` is deliberately small and fully typed:

- `Program { functions, main }`; `functions[0]` is the module body `<module>`
  of the entry file. User modules occupy contiguous `FunctionId`
  ranges; all functions live in one `Program`.
- Every `Function` has `params`, `capture_count`, `ret_ty` and `locals`
  (index = `LocalId`). The `capture_count` leading parameters are closure
  captures and are supplied by the runtime value; the visible arity is
  `params.len() - capture_count`.
- `Stmt`/`Expr` carry `line` (1-based) for tracebacks.
- Expressions: `Const`, `Local`, `Bin` (incl. `In`/`NotIn`,
  `Is`/`IsNot` and the bitwise operators), `Un`, `BoolOp` (short-circuit),
  `IfExp` (`a if c else b`,
  only the selected branch evaluates), `List`, `Dict`, `Tuple`, `Set`,
  `Subscript`, `Slice`,
  `ListComp`, `SetComp`, `DictComp`, `GeneratorExp` (eager), `Await`,
  `Yield` (expression form; evaluates to the sent value), `YieldFrom`
  (delegation; evaluates to the delegate's result), `FunctionRef`,
  `BuiltinRef` (`f = int`), `IndirectCall { callee, args, star }`
  (`callee(args, *star)`; `star` is one optional list/tuple operand),
  `ModuleCall`, `ModuleAttr`,
  `GetAttr` (data attribute), `MethodCall { args, star, keywords }`,
  `Closure` (value), `Call { callee: Function(id) | Builtin(b), args }`,
  `CallKw { callee, args, keywords }` (runtime keyword/default binder),
  `ClassCall { class, init, args, dynamic_kwargs, param_names,
  kwarg_index }` (class constructors, including `*args`/`**mapping`).
- Statements: `Expr`, `Assign`, `AugAssign`, `If`, `While`, `ForRange`,
  `ForEach`, `Unpack`, `SetSubscript`, `SetSlice` (list slice assignment;
  omitted bounds are `Const::None`), `Closure` (nested def), `Yield`,
  `ClassDef { target, name, base, methods }`, `SetAttr`, `Delete { targets }`,
  `Return`, `Break`, `Continue`, `Pass`, `Raise`,
  `Try { body, handlers, else_body, finally_body }`.
- `Handler.exc == None` is a bare `except:` (catches every tag). `else` runs
  only after an error-free body; `finally` runs on every path (normal, `return`,
  `break`, `continue`, exception) and can override the previous action
  (return/break/continue suppress a pending exception; a new exception
  in finally replaces the old one). Tuple handlers are expanded by the
  frontend into ordered handlers. `raise ... from ...` stores an explicit
  cause, rendered deepest-first; `from None` suppresses it. Implicit context
  chaining ("During handling ...") and `except*` are not implemented.
- `ForEach` iterates lists/strings/dicts via the runtime iterators;
  `ForRange` remains the syntactically specialized `range(...)` form (since
  Phase 2.7, `range` is additionally a value). Negative step is
  allowed, step `0` is a runtime `ValueError`.
- `Delete { targets }` evaluates targets left to right and deletes each one
  immediately: locals (slot cleared to `None`; PyX does not track unbound
  locals), module globals (`NameError` when missing), instance attributes,
  list/dict subscripts and list slices. Receivers/bounds are evaluated in
  Python order, immutable receivers and class namespaces raise `TypeError`,
  and the first failing target stops the statement. Slice bounds are boxed
  in the IR because a delete slice carries four `Expr` nodes.
- `While`/`ForRange`/`ForEach` carry an `else_body` that runs only when the
  loop exhausts normally: `break`, `return` and exceptions skip it, while
  `continue` does not. Codegen branches normal exhaustion to the else entry
  and `break` past it, and emits the clause after popping the loop, so loop
  actions inside it bind to an enclosing loop. A loop without `else` keeps
  the previous block shape, and `while False: ... else: ...` keeps only the
  else body.
- `Raise { exc, msg, cause }`: `exc == msg == None` is a bare re-raise;
  `cause` is the optional explicit chaining expression. A raise of a class
  declared in the unit lowers to `RuntimeError` with a
  `"<ClassName>: <message>"` message (see the exception-tag notes above).
- `tuple()`/`tuple(iterable)` materialise a tuple (`pyx_tuple_from`), and
  `hasattr(obj, name)` runs the attribute lookup, swallowing only
  `AttributeError`. Starred unpack targets (`a, *rest, b = value`) bind a
  fresh list of the middle elements through `pyx_unpack_star`; one star is
  allowed and it may not nest inside another container target.
- `functools.partial(func, *args, **kwargs)` keeps keyword names: the
  frontend forwards them through `pyx_module_call_kw`, the runtime stores
  `(name, value)` pairs in the partial and applies them at call time using
  the parameter names codegen attaches to every generated function value
  (`pyx_function_params`). A callable without parameter names rejects
  keywords with `TypeError`; a hole before a named parameter is reported
  instead of silently passing `None`.
- Functions carry `is_generator` and `is_async`; codegen derives the
  flags in `pyx_function_new`/`pyx_closure_new` from them (bit 0 async, bit 1
  generator) and generates a wrapper that calls `pyx_coroutine_new` resp.
  `pyx_generator_new`. `Yield` statements become `pyx_yield` (result
  discarded); `Yield` expressions keep the returned sent value; `YieldFrom`
  becomes `pyx_yield_from`. `Await` becomes `pyx_await`.
- **Lowering:** `with`/`async with` become nested `try/finally` with
  `__enter__`/`__exit__`; augmented subscript assignment
  (`a[i] += x`) evaluates object and index exactly once into `$aug` locals.
  `async for` uses the synchronous `for` lowering logic.
- **User modules:** imports emit no IR statements; the loader replaces
  them with `FunctionRef`s to the exported functions of the target module.
  `__name__` is an optional module body local.

Types: `Int, Float, Bool, Str, Bytes, None, Any, Never, List, Dict, Tuple,
Set, Range, Function`.

## 6. Type Inference (Frontend)

- Unification-based over type variables; no polymorphism.
- Literals/operators create constraints; `int`/`float` unify to `float`
  (widening), `bool` is numerically compatible with `int`.
- Function signatures are derived from body and call sites; unresolvable
  variables become `Any`.
- Contradictions (e.g. `1 + "x"` without `Any`) are diagnostics, not panics.
- Annotations (`def f(x: int) -> int`) are respected and checked.
- Return type: `None` if no `return` with a value exists; `Never` for an
  exclusively throwing body. A call of a generator or
  `async` function returns an object (generator/coroutine) and is typed as
  `Any`; the annotation describes the yielded values.
- The module body is `FunctionId 0` with `ret_ty = None`.
- Containers are unparameterized (elements `Any`). `len`, `str`, indexing
  and `for` accept them; the index type must be `Int`/
  `Bool` for `List`/`Str`/`Bytes` (arbitrary for `Dict`). `==`/`!=` on equal
  container types yields `Bool`; ordering comparisons on containers are
  rejected until the runtime implements them.
- `Set` is unparameterized; `In`/`NotIn` → Bool with the same rules as
  §3.3. `range()` → Range, `list()` → List, `dict()` → Dict, `sum()` → Int
  for Range, otherwise Any, `enumerate`/`zip`/`map`/`filter` → List,
  `format()` → Str, `isinstance()` → Bool, `repr()` → Str,
  `hash()` → Int,
  `max`/`min`/`round` → Any. Bitwise `&`/`|`/`^`/`<<`/`>>` on
  `Int`/`Bool` operands → Int; other operand combinations stay `Any` and
  raise `TypeError` at runtime.
  `Is`/`IsNot` → Bool for every operand pair and does not narrow either
  operand (`x is None` leaves `x` dynamic). `IfExp` joins the branch types;
  incompatible branches become `Any`.
- Comprehensions: ListComp → List, SetComp → Set, DictComp → Dict,
  GeneratorExp → List (eager). Targets bind synthetic locals.
- Name/Lambda → Function; indirect call: callee Function/Any, result Any;
  `f = int` is a function (`BuiltinRef`). Closures capture exclusively
  existing locals; `capture_count` is derived from the first use in the
  nested body.
- Modules/imports: native module attributes/calls are typed via
  `pyx_ir::stdlib`; user module imports bind exported function signatures.
  Module-level imports are globally visible. A name bound by a nested import
  is visible too, but has the dynamic `DeferredImport` type: using it raises
  at runtime instead of failing the type check.
- Classes: `class` binds a dynamic class value (no static
  instance type); method calls via instances are checked dynamically,
  calls with `Any` fall back to runtime checks.
- Statically `Int`-typed operands use checked int fast paths in codegen:
  `+`/`-`/`*` (untag, `sadd_overflow`/`ssub_overflow`/
  `smul_overflow`, range check, retag) and comparisons; on overflow or a
  non-`Int` result it goes through `pyx_binop`. The same applies to `AugAssign`
  and to fully `Int`-typed `for range` counters. Constant folding and
  dead-`if`/`while False` already run in the frontend (only with guaranteed
  identical semantics).

## 7. Memory & GC

Phase 1/2.0 allocated heap objects via `Box::into_raw` and leaked them. Since
Phase 2.1/2.15 there is a **conservative, concurrent mark-&-sweep
collector as an interim**:

- Every heap allocation is registered in a thread-local registry (object list +
  address set) and given an atomic color WHITE/GREY/BLACK. The address set
  uses a deterministic non-cryptographic SplitMix64 hasher: its keys are
  runtime-owned aligned heap addresses, never adversarial bytes, so SipHash
  seeding is unnecessary while equality still compares the full address.
- Roots: conservative scan of the native stack (bounds via
  `pthread_getattr_np`), a register dump (x86_64/aarch64) and the
  pending exception. Tagged heap values are accepted only if their
  object address lies exactly in the registry; false hits can only cause
  over-retention.
- **Fiber roots:** While a generator/coroutine fiber runs, the
  thread stack points to the fiber stack; `push_stack_bounds` overrides the
  stack bounds so that the scan sees the running fiber stack. The
  fiber stack as well as `ctx` and `caller_ctx` are registered as conservative
  root areas from object creation for the entire lifetime (`Drop`
  removes them). On x86_64 the two contexts contain the callee-saved
  register words that may hold tagged values during a suspension;
  a suspend/resume does not touch the registry. The outermost resume
  publishes two aux ranges (native driver stack and `caller_ctx`)
  via `set_aux_stack`; in shared mode both are mirrored into the `ThreadGc`,
  in local mode it stays with thread-local ranges without atomics
  per switch. Nested resumes publish nothing because the caller is a fiber
  with already registered lifetime ranges. `PYX_GC_STRESS=1` covers
  interleaved generators.
- The register dump happens in **one** `asm!` block that writes all
  general-purpose registers (x86_64: incl. `rsp`; aarch64: `x0..x30` + `sp`)
  into a local buffer, which is then marked completely as roots. If the
  compiler chooses a register for the buffer pointer, it has previously
  spilled its prior live value to the stack; the stack scan covers it.
- Stack words are loaded via inline asm (x86_64 `mov`, aarch64 `ldr`),
  not via `ptr::read_unaligned`: stack slots above `sp` may be
  uninitialized, and a machine load is opaque to the compiler.
  Other architectures fall back to `read_unaligned`; there the
  interim collector accepts this theoretical UB gap (no supported targets).
- Mark traverses `List` elements, `Dict` keys/values, `Iter.target`,
  captures/base classes/instance attributes and all other traced children;
  sweep frees unmarked objects and compacts the registry. The local sweep
  rebuilds the address set **in place** (`clear` + `extend`) into its existing
  allocation instead of allocating a fresh `HashSet` per collection; both
  registries keep their capacity unless it is grossly oversized (`> max(1024,
  4 × live)`). Measured on a 300k-short-lived-list AOT workload: median
  runtime 44.95 ms → 40.91 ms (≈9% faster) with identical output.
- A shared-heap cycle has two short stop-the-world windows: initial
  root snapshot (stacks, registers, pending, thread roots) and final
  root rescan. In between, a collector thread marks concurrently; the
  sweep also runs concurrently after the rescan (white partition briefly
  under the heap lock, `Box` release outside, new allocations stay
  protected).
- **Insertion barriers** per new edge (list/dict/set/queue/instance attr)
  mark the new child if the parent is already BLACK;
  container mutation and marker are serialized by a graph lock per payload. In
  local mode the lock is omitted.
- Trigger on growth since the last collection: `live + PYX_GC_THRESHOLD`
  (default 10000), not on absolute object count. `PYX_GC_STRESS=1` collects
  before every allocation. `PYX_GC_STATS=1` reports pause, mark and sweep time
  as well as frees per collection on stderr.
- `pyx_runtime::gc_stats()` returns a `GcStats` snapshot
  (`collections`, `pause_total_us`, `pause_max_us`, `mark_total_us`,
  `sweep_total_us`, `freed_total`, `live_objects`, `threshold`, `stress`);
  `reset_gc_stats()` resets the counters. `pyx gc [--json] <file.py>`
  runs the program like `pyx run` and prints the statistics afterwards;
  with `--json` the last stdout line is an object with snake_case keys.
- `gc/alloc.rs` manages fixed `PyxObj` cells in slabs (chunks of 64,
  free list, alignment, reuse/stats); variable payload size classes are
  a separate heap architecture phase.
- `pyx_gc_collect()` forces a collection; `pyx_gc_objects()` returns the
  live object count. `pyx_runtime_init()` resets heap and exception state.

This is deliberately an **interim**: conservative scanning can preserve false
positives; the final root rescan is STW, the white partition runs briefly
under the heap lock. P2-05 (dedicated size-class allocator), P2-06/P2-07
(precise shadow-stack roots, finer safepoint polling) remain open. The
`unsafe` policy: The GC scan/free paths are encapsulated in `pyx-runtime/src/gc/`
and justify their invariants there.

### 7.1 Concurrency (P2-20)

**No GIL.** Since Phase 2.12 there is a global shared-heap mode
(heap migration on the first thread), a thread registry and native
`_thread` primitives. Since Phase 2.13/2.15 a **concurrent
mark-&-sweep collector** collects even while threads run: Codegen calls
`pyx_safepoint()` at function entries and (since Phase 2.15 only every 64th
loop iteration) at loop backedges; every thread registers stack bounds,
saved SP/registers, thread roots and a pending-exception mirror.
The collector sets `GC_REQUESTED` only for the root snapshot and final
rescan/sweep, waits there for parked/exited threads, and marks concurrently
in between with insertion barriers. The heap lock is taken only after
parking, `GC_LOCK` only via try_lock. Blocking
locks/join/sleep/queue/socket waits park the thread while waiting;
container mutations and markers are serialized by a graph lock. Sweep and
marking run concurrently. Mutating containers must be protected by user locks;
unprotected mutations are data races. Safepoints still cost
throughput (baseline `threads_serial` AOT approx. 40–45 ms).

## 8. Toolchain / CLI / uv (Phase 3)

The CLI is a single binary. Subcommands (full help via
`pyx help`):

`new`, `init`, `check`, `run`, `build`, `test`, `fmt`, `lint`, `clean`,
`gc`, `add`, `remove`, `sync`, `lock`, `doctor`, `help`, `--version`.

- `new`/`init` create `main.py`, `pyproject.toml` and `.gitignore`;
  existing non-empty files are not overwritten without `--force`.
- `test` runs `test_*.py`/`*_test.py` with `pyx run`; success = exit 0
  (no pytest assertions). `--json` returns a result list.
- `fmt` normalizes trailing whitespace and exactly one final newline
  (no AST formatter); `--check` does not write. `lint` type-checks all
  `.py` files; `--json` aggregates diagnostics.
- `clean` removes `.pyx/cache` and `target/pyx-tmp` (`--dry-run`).
- `gc` compiles and starts a program like `run` and prints the
  GC statistics afterwards (`--json` for a machine-readable last line).
- `add`/`remove`/`sync`/`lock` are a **passthrough** to `uv`
  (`PYX_UV` overrides the executable). PyX interprets neither specs nor
  `pyproject.toml`; the uv exit status is returned.
- The build cache (§4.1) is implemented; `--no-cache` bypasses it.

`uv` remains an external dependency for now: PyX uses uv for resolution,
lockfiles and venvs, but does **not yet** inject the PyX interpreter
(`.python-version`, venv markers, `sys.executable` equivalent). A dedicated
fork is intended only in case of an architectural blocker. Own files:
`pyproject.toml` (compatible), `pyx.lock` (optionally additional), build cache
under `.pyx/cache/`.

## 9. C API/ABI Strategy (Phase 4)

**Decision (2026-10-24): option B — a PyX-native CPython C-API/ABI layer.
No `libpython` in the production runtime.**

CPython 3.14 development files are used at build time to compile extensions
and test fixtures; the CPython interpreter serves as a differential test
oracle only. `ldd pyx` must not contain `libpython`. One process keeps the
PyX tagged-value model, the concurrent GC and the fast startup path; foreign
code sees refcounted `PyObject` handles whose lifetime is integrated with the
collector (refcount as an additional root, `tp_traverse`/`tp_clear` mirroring
GC edges). The GIL is emulated only while foreign code runs; PyX code remains
GIL-free.

Two tiers: (1) `abi3`/limited API (module init, `PyArg_Parse*`, scalar and
container constructors, errors, capsules, buffer basics); (2) the full
CPython 3.14 ABI required by NumPy 2.5 and `pydantic-core` (`PyTypeObject`
layout, vectorcall, full buffer protocol, GC integration, package/wheel
loading).

**Implemented (2026-10-24, first abi3 slice):** the shim in
`pyx-runtime/src/capi/` exports the limited-API module/scalar/error surface
with a C-variadic `PyArg_ParseTuple`; the frontend resolves sibling
`.so`/`.abi3.so` files at compile time and lowers `ext.member(...)`/
`from ext import member` to `Callee::Extension`/`ExprKind::ExtensionAttr`;
`pyx_extension_call`/`pyx_extension_get` load and cache modules lazily
(`modules/extension.rs`). Scalars, strings, bytes, floats, lists, tuples and
dicts convert recursively in both directions (bounded copies); capsules and
read-only contiguous bytes buffers are supported. Extension discovery includes
sibling files, `PYX_EXTENSION_PATH`, `PYTHONPATH`, `PYX_SITE_PACKAGES` and
venv/conda `site-packages`; dotted submodules (`pkg.sub_ext`) resolve through
a directory path without package Python source. Wheel tags are validated
(`.so`, `.abi3.so`, `.cpython-314-*`; others are compile-time diagnostics), and
both single- and multi-phase init (`PyModuleDef_Init`/`Py_mod_exec`) are
supported. `pyx` and AOT binaries export the shim symbols with
`--export-dynamic`; fixtures import and call under both with no `libpython`.
The full 3.14 ABI (NumPy/pydantic-core) remains open.

Evidence: embedded CPython costs ~6.7 ms `Py_Initialize` + ~7.5 ms
`Py_Finalize` and ~41.6 ms for `import numpy`, versus ~0.8–1.0 ms for a PyX
AOT process; a stable-ABI probe imports with four undefined CPython symbols
(the initial shim surface). Full rationale, milestones and acceptance gates:
[`C_API.md`](C_API.md). The 297-module stdlib program (174 pure Python,
71 extension, 52 builtin/frozen modules), the ecosystem suites and their
benchmarks are tracked as P4-01…P4-11 in [`TODO.md`](TODO.md).

## 10. Verification and Documented Limits

- `scripts/verify.sh`: fmt-check, clippy (`-D warnings`), build,
  `cargo test --workspace`, E2E examples (JIT **and** AOT), negative tests
  (diagnostics/tracebacks), GC threshold/stress.
- As of 2026-10-25: **1377 tests** (417 frontend, 48 codegen, 18 IR, 494
  runtime unit, 179 runtime integration, 221 CLI/E2E). E2E coverage
  includes, among others, formatting, classes, closures, lazy generators/coroutines
  (`fibers.rs`), user modules, `unittest` runner, stdlib Batches A/B/bench,
  `argparse`/`defaultdict`/`re`,
  socket echo, pathlib, build cache, `pyx gc` and uv passthrough.
- Generated stdlib import matrix (2026-10-25): **60/297 importable** (13
  builtin/frozen, 43 pure, 4 extension; report in
  [`COMPATIBILITY.md`](COMPATIBILITY.md)). `benchmarks/bench_all.py` does
  run under PyX with `PYX_GC_THRESHOLD` raised; rich
  console/table/panel/rule rendering works and a native `rich.status.Status`
  shim covers `Console.status`. The default conservative collector still
  loses live values under heavier rich rendering.
- New unit tests cover the dict hash index, the GC statistics and
  fiber iteration; `examples/generators_async.py` runs under JIT and AOT.
- E2E compares stdout/exit code of `pyx run` and the AOT binary; negative tests
  check diagnostics and tracebacks (stderr, exit code).
- No silent miscompilation: unsupported syntax → diagnostic.

**Limits (deliberate, documented):**

- **Language:** no BigInt (i64), no general decorators (only
  `property`/`classmethod`/`staticmethod` on methods and the ignored
  `lru_cache`/`cache`/`wraps`/`abstractmethod`/`overload`/
  `runtime_checkable`), pattern matching,
  metaclasses, descriptor/`__dunder__` protocols, `except*`,
  implicit exception context, `bytearray`/`memoryview`; other `super()` forms; class namespace immutable. Multiple inheritance is approximated
  by the first base (additional bases are ignored). General dynamic calls
  combine at most one `*` operand; `*args` plus `**kwargs` together still
  need a deferred call, while class constructors can bind `**mapping`.
- **Closures:** by value at definition time; recursive nested
  functions and closures in lambdas are diagnostics. No cell/mutable
  capture.
- **Generators/async:** real lazy generators (stackful fibers) and
  coroutines with a cooperative event loop. Open: async generators, async
  comprehensions and `asyncio` beyond `run`/`sleep`/`gather`; generator
  expressions are eagerly materialized into an iterator (not lazy),
  `async for`/`async with` use the
  synchronous protocol. `throw` accepts exception instances only (no
  exception class objects) and has no three-argument form.
- **GC:** the conservative collector roots container builders and generated
  call buffers, but heavy rich rendering still exposes live values lost
  under allocation (`P2-06` precise roots remain the fix).
- **Performance:** native module dispatch is fixed (thread-local spec cache;
  math_loop at parity/faster, 50k micro harness AOT 6.2 ms vs. CPython
  13.6 ms). Remaining open weak spots: string concatenation (O(n²)) and float
  boxing; CPython-extension boundaries will add conversion cost. The x86_64
  fiber switch is register-only (two switches per yield, no syscalls); a
  100k generator sum reaches release AOT 6.8 ms / JIT 7.7 ms versus CPython
  10.2 ms (`strace -c`: 94 syscalls total).
- **Nested imports:** declaration-only site-packages modules referenced
  only from nested blocks are preloaded after the import graph is complete
  and inserted before their importer; modules with executable top-level
  code remain runtime-error stubs.
- **Modules:** native modules, sibling `.py` files and packages
  (`__init__.py`, dotted names, submodules, relative imports,
  site-packages). Imported module bodies run once as initializers. Stdlib
  module values work (with `getattr`), and simple module-level assignments
  are visible inside functions. Nested imports resolve module globals when
  the module is already initialized; import-free site-packages modules are
  preloaded for this path. No general runtime module objects/`sys.modules`,
  namespace packages or frozen modules; user-module
  values remain diagnostics.
- **Stdlib:** curated subset (see §3.3); `re` uses the Rust
  `regex` syntax, `socket` only blocking TCP, `argparse` with a fixed help
  renderer (and no `SystemExit` for an explicit argv), `random` not
  CPython-reproducible.
- **GC:** conservative, no precise roots/stack maps, no
  size-class allocator; STW root window. P2-05/06/07 open.
- **Concurrency:** no GIL; unprotected mutations of shared containers are
  data races. Function registry for spawn/Pool only by name/arity
  (duplicates/lambdas/native targets rejected); queue `maxsize` is lost in the
  spawn payload.
- **File I/O:** UTF-8 text and bytes, no `+` mode, `newline`/`errors`
  ignored; CSV entirely in memory.
- **Platform:** Linux x86_64 is the verified target; the x86_64 register
  switch is measured here, the `ucontext` fallback for aarch64/other targets
  exists but is not verified on this machine. macOS/Windows and
  cross-compilation are missing. The C API/ABI path is decided (P4-04 option B)
  and the first abi3 slice runs a limited-API fixture under JIT/AOT without
  `libpython`; capsules/buffer/containers, wheels/venvs, the full 3.14 ABI and
  the 297-module stdlib program are open (P4-04a…P4-11).
