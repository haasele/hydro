# Hydro — Architecture

As of: **2026-10-25**. Phase 1 is complete; Phase 2 is largely
implemented (current test counts and breakdown: [`AGENTS.md`](../AGENTS.md#current-state);
`scripts/verify.sh` ALL GREEN). This document is
the **normative contract** between the crates. Changes to ABI/IR only with an
update of this document and of all consumers. The C API decision (P4-04,
option B: Hydro-native ABI layer without `libpython`) and its gates are
documented in [`C_API.md`](C_API.md).

## 1. Vision

Hydro is a standalone Python runtime with an AOT compiler and toolchain
(`tea`), conceptually Bun for Python. CPython is not the standard runtime.
One goal is broad Python compatibility, initially via a clearly
defined subset (see `docs/TODO.md`).

**Supported:** tagged values, containers, functions/closures, classes
(single base), comprehensions, f-strings/formatting, exceptions,
file I/O, user modules and packages, native stdlib modules, lazy generators,
async generators and coroutines
with a cooperative event loop and task groups, threads/processes/pool, and a
precise-shadow-frame concurrent mark-&-sweep GC with a size-classed payload
allocator.

**Deliberately not (as of now):** the complete CPython stdlib,
C extensions/C API, decorators, pattern matching, metaclasses,
`bytearray`/`memoryview` and the complete CPython stdlib/ABI. Details in §10
and `docs/TODO.md`.

## 2. Components & Data Flow

```
Source .py (+ imported modules and packages)
  │  rustpython-parser (Python grammar, AST)
  ▼
hydro-frontend ── Lowering ──► typed IR (hydro-ir) ──► hydro-codegen
  │ Type checking/inference                      │
  ▼                                              ├─ JIT (tea infuse)
Diagnostics (file:line:col)                      └─ Object + cc link (tea build)
                                                        │
                                                  hydro-runtime (libhydro_runtime.a, C ABI)
```

| Crate | Responsibility | Dependencies |
|---|---|---|
| `hydro-ir` | Typed HIR; the only frontend↔codegen contract; central stdlib specification | – |
| `hydro-frontend` | Parsing, module loader/imports, lowering, type inference, diagnostics | `hydro-ir`, `rustpython-parser` |
| `hydro-runtime` | Value model, operators, builtins, exceptions, GC, file I/O, modules, concurrency; `rlib` + `staticlib` | `regex` et al. |
| `hydro-codegen` | Cranelift codegen, JIT and AOT path | `hydro-ir`, `hydro-runtime`, Cranelift |
| **Tea** (external) | CLI/toolchain repository: project commands, pytest, REPL, AOT packaging, uv passthrough | `hydro-frontend`, `hydro-codegen`, `hydro-runtime`, `hydro-interp` |

### 2.1 Directory Layout

Since the completion slice the crates are internally split into thematic directories
(full tree in [`LAYOUT.md`](LAYOUT.md)):

```
crates/hydro-ir/src/{model.rs, ir.rs, stdlib/}
crates/hydro-frontend/src/{lib.rs, diag.rs, lower/, sema/}
crates/hydro-codegen/src/{lib.rs, backend/, lower/}
crates/hydro-runtime/src/{abi.rs, core/, gc/, modules/, concurrency/, support/, csv.rs}
../tea/src/{main.rs, cli/, cli/commands/}   (external toolchain)
```

`hydro-ir` remains logic-free (contract structures + type tables). Runtime code
lives exclusively in `hydro-runtime`; `hydro-codegen` only calls
`hydro_runtime::abi` symbols.

## 3. Runtime ABI (Normative)

All values are `i64` (`HydroValue`). All functions are `extern "C"`,
`#[no_mangle]`, in the `hydro_runtime::abi` module, and **must not** panic
(user error → pending exception; internal error → `process::abort`).

### 3.1 Tagging

| Bits (`v & 7`) | Meaning |
|---|---|
| `000` | `int`: value = `v >> 3` (arithmetic, i64) |
| `001` | Special: `None = 1`, `False = 9`, `True = 17` (payload `0/1/2`) |
| `010` | Heap pointer: `ptr = v & !7`, points to `HydroObj` |
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

Kinds 32–37 (`ExtFunction`, `Capsule`, `Module`, `StdStream`, `BigInt`,
`Cell`) were added after this table. Kind 38 is `StrBuilder`, the internal
mutable string accumulator created by `hydro_str_builder_new`: it appends in
place for a proved `s += ...` loop and `hydro_str_builder_finish` returns a
normal `str`, so it never reaches user code.

Lists/dicts/sets are insertion-ordered (sets deterministically; Python
guarantees no order). `Iter` holds target and index. `Class` carries name,
optional base and an immutable method table; `Instance` carries the
class and a mutable attribute table. `Fiber` is a generator, coroutine or
async generator object (`kind`, own stack, architecture context, state,
result/error, cancellation and awaiting fields; see §3.3). On x86_64 the
context is a callee-saved register frame, otherwise a `ucontext_t`.

### 3.2 Symbols

```c
i64  hydro_none(void);                  // constant 1
i64  hydro_true(void);                  // 17
i64  hydro_false(void);                 // 9
i64  hydro_bool(i64 b);                 // Truthiness -> True/False
i64  hydro_int(i64 v);                  // identity (range check)
i64  hydro_str_new(const uint8_t* ptr, size_t len);   // copies UTF-8
i64  hydro_str_builder_new(i64 init);   // mutable accumulator, seeded with init
i64  hydro_str_builder_append(i64 builder, i64 value);
                                      // builder + value, appends a str in place
i64  hydro_str_builder_finish(i64 builder);  // accumulated text as a str
i64  hydro_bytes_new(const uint8_t* ptr, size_t len);
i64  hydro_bytes_build(uint64_t argc, const i64* argv); // bytes(...)
i64  hydro_float_new(double f);
i64  hydro_math_floor_sqrt_int(i64 value); // floor(sqrt(int)) fast/error path
i64  hydro_bigint_from_str(const uint8_t* ptr, size_t len); // decimal literal
i64  hydro_binop(uint32_t op, i64 a, i64 b);          // incl. comparisons
i64  hydro_unop(uint32_t op, i64 a);
i64  hydro_truthy(i64 v);               // 1 if truthy, 0 if falsy (C style)
i64  hydro_print(uint64_t argc, const i64* argv);     // space-separated + '\n'
i64  hydro_len(i64 v);
i64  hydro_to_str(i64 v);
i64  hydro_to_repr(i64 v);                            // repr(v)
i64  hydro_hash(i64 v);                              // hash(v)
i64  hydro_to_int(i64 v);
i64  hydro_to_float(i64 v);
i64  hydro_abs(i64 v);
i64  hydro_all(i64 iterable);                         // all(): True/False
i64  hydro_format_value(i64 value, const uint8_t* spec, size_t spec_len,
                      uint8_t conversion);          // f-string field
i64  hydro_list_new(uint64_t argc, const i64* items);
i64  hydro_dict_new(uint64_t argc, const i64* keys, const i64* values);
i64  hydro_tuple_new(uint64_t argc, const i64* items);
i64  hydro_unpack(i64 value, uint64_t expected);      // None or ValueError
i64  hydro_set_new(uint64_t argc, const i64* items);
i64  hydro_set_from(i64 iterable);
i64  hydro_range_new(i64 start, i64 stop, i64 step);  // step != 0
i64  hydro_unpack_seq(i64 value, uint64_t expected);
                                    // sequence to bind (fresh list if needed)
i64  hydro_unpack_star(i64 value, uint64_t before, uint64_t after);
                                    // a, *rest, b = value: fresh list with
                                    // slot `before` holding the middle list
i64  hydro_list_from(i64 iterable);
i64  hydro_tuple_from(i64 iterable);  // tuple(iterable)
i64  hydro_dict_from(i64 iterable);
i64  hydro_sum(i64 iterable, i64 start);
i64  hydro_enumerate(i64 iterable, i64 start);
i64  hydro_zip(uint64_t argc, const i64* iterables);
i64  hydro_list_append(i64 list, i64 value);          // None or error
i64  hydro_set_add(i64 set, i64 value);
i64  hydro_dict_set(i64 dict, i64 key, i64 value);
i64  hydro_dict_merge(i64 left, i64 right);           // {**left, **right} copy
i64  hydro_function_new(const uint8_t* wrapper, uint64_t arity,
                      const uint8_t* name, size_t name_len, uint32_t flags);
i64  hydro_function_params(i64 fn, uint64_t count, const i64* names);
                                    // 2*count words [ptr, len, ...]:
                                    // user-visible positional parameter names
i64  hydro_function_meta(i64 fn, i64 full_entry, uintptr_t count,
                       const i64* names, const i64* kinds,
                       const i64* defaults, const i64* default_flags);
                                    // names: [ptr,len] pairs; kinds
                                    // 0..3; definition-time defaults
i64  hydro_call_kw(i64 fn, uintptr_t argc, const i64* argv,
                 uintptr_t kwargc, const i64* kw_argv);
i64  hydro_builtin_function(const uint8_t* name, size_t name_len); // f = int
i64  hydro_closure_new(const uint8_t* wrapper, uint64_t arity,
                     const uint8_t* name, size_t name_len,
                     uint64_t capture_count, const i64* captures,
                     uint32_t flags);
i64  hydro_cell_new(i64 value);      // mutable closure binding
i64  hydro_cell_get(i64 cell);       // current value
void hydro_cell_set(i64 cell, i64 value);  // publishes the GC edge
i64  hydro_class_new(const uint8_t* name, size_t name_len, i64 base,
                   uint64_t method_count, const i64* names, const i64* funcs,
                   const i64* kinds,
                   uint64_t attr_count, const i64* attr_names,
                   const i64* attr_values);
i64  hydro_instance_new(i64 class);   // allocate without __init__
i64  hydro_bind_class_kwargs(i64 mapping, const i64* names, uintptr_t count,
                           i64* slots, i64 kwarg_index,
                           const uint8_t* class_name, size_t class_len);
                                    // route **mapping into constructor slots
i64  hydro_exception_class(uint32_t tag);  // cached native class, e.g. ValueError
i64  hydro_call(i64 fn, uint64_t argc, const i64* argv);
i64  hydro_call_star(i64 fn, uint64_t argc, const i64* argv, i64 star);
                            // fn(args..., *star); star is a list/tuple
i64  hydro_map(i64 fn, i64 iterable);                 // materialized list
i64  hydro_filter(i64 fn_or_none, i64 iterable);
i64  hydro_module_call(const uint8_t* module, size_t module_len,
                     const uint8_t* function, size_t function_len,
                     uint64_t argc, const i64* argv);
i64  hydro_module_call_kw(const uint8_t* module, size_t module_len,
                        const uint8_t* function, size_t function_len,
                        uint64_t argc, const i64* argv,
                        uint64_t kwargc, const i64* kw_argv);
                                    // [name, value, ...]; only specs with
                                    // dynamic keywords (functools.partial)
i64  hydro_module_get(const uint8_t* module, size_t module_len,
                    const uint8_t* name, size_t name_len);
i64  hydro_module_value(const uint8_t* module, size_t module_len);
i64  hydro_deferred_import(const uint8_t* module, size_t module_len,
                         const uint8_t* member, size_t member_len);
                     // resolves an already-initialized module global when
                     // possible; otherwise raises RuntimeError
                     // (empty member = module alias)
i64  hydro_global_get(const uint8_t* module, size_t module_len,
                    const uint8_t* name, size_t name_len);
i64  hydro_global_set(const uint8_t* module, size_t module_len,
                    const uint8_t* name, size_t name_len, i64 value);
i64  hydro_subscript(i64 obj, i64 index);
i64  hydro_subscript_set(i64 obj, i64 index, i64 value);
i64  hydro_slice(i64 obj, i64 start, i64 stop, i64 step);
i64  hydro_slice_set(i64 obj, i64 start, i64 stop, i64 step, i64 value);
                     // list slice assignment; returns None, errors via pending
                     // exception
i64  hydro_getattr(i64 obj, const uint8_t* name, size_t name_len); // data attribute
i64  hydro_hasattr(i64 obj, const uint8_t* name, size_t name_len); // True/False
i64  hydro_setattr(i64 obj, const uint8_t* name, size_t name_len, i64 value);
i64  hydro_method(i64 obj, const uint8_t* name, size_t name_len,
                uint64_t argc, const i64* argv);
i64  hydro_method_kw(i64 obj, const uint8_t* name, size_t name_len,
                   uint64_t argc, const i64* argv,
                   uint64_t kwargc, const i64* kw_argv); // [name, value, ...]
i64  hydro_method_star(i64 obj, const uint8_t* name, size_t name_len,
                     i64 star, uint64_t kwargc,
                     const i64* kw_argv);  // obj.m(*list_or_tuple, k=v)
i64  hydro_iter_new(i64 obj);
i64  hydro_iter_next(i64 iter, i64* out);             // 1=value, 0=end
i64  hydro_iter(i64 obj);                             // iter(obj)
i64  hydro_next(i64 obj, i64 default, i64 has_default); // next(obj[, default])
i64  hydro_generator_new(const uint8_t* entry, uint64_t argc, const i64* argv);
i64  hydro_coroutine_new(const uint8_t* entry, uint64_t argc, const i64* argv);
i64  hydro_async_generator_new(const uint8_t* entry, uint64_t argc,
                             const i64* argv);      // lazy async generator
i64  hydro_yield(i64 value);                          // suspends the fiber
i64  hydro_yield_from(i64 iterable);                  // delegates iteration
i64  hydro_await(i64 value);                          // drives a coroutine
i64  hydro_gc_collect(void);
i64  hydro_gc_objects(void);
void hydro_safepoint(void);
void hydro_set_file(const uint8_t* ptr, size_t len);
void hydro_set_line(uint32_t line);
void hydro_frame_enter(const uint8_t* name, size_t len);
void hydro_frame_leave(void);
i64  hydro_exc_pending(void);
i64  hydro_exc_tag(void);
i64  hydro_exc_value(void);
i64  hydro_exc_clear(void);
i64  hydro_raise(uint32_t tag, i64 msg);
i64  hydro_raise_msg(uint32_t tag, const uint8_t* p, size_t n);
i64  hydro_raise_value(i64 exc);
i64  hydro_raise_from(uint32_t tag, i64 msg, i64 cause);
i64  hydro_raise_value_from(i64 exc, i64 cause);
int  hydro_exc_matches(uint32_t handler_tag);
i64  hydro_uncaught(void);
void hydro_runtime_init(void);
void hydro_register_function(const uint8_t* name, size_t name_len,
                           const uint8_t* wrapper, size_t arity); // spawn/Pool
int32_t hydro_spawn_child(void);        // child mode after exec (-1 = normal)
```

`op` codes (`hydro_binop`): `0 Add, 1 Sub, 2 Mul, 3 Div, 4 FloorDiv, 5 Mod,
6 Pow, 7 Eq, 8 Ne, 9 Lt, 10 Le, 11 Gt, 12 Ge, 13 In, 14 NotIn, 15 Is,
16 IsNot, 17 BitAnd, 18 BitOr, 19 BitXor, 20 Shl, 21 Shr`.
`hydro_unop`: `0 Neg, 1 Pos, 2 Not, 3 Invert` (`~` is int/bool only).

`flags` in `hydro_function_new`/`hydro_closure_new`: Bit 0 = `async`, Bit 1 =
generator. If either of the bits is set, the generated wrapper calls
`hydro_async_generator_new` (both bits), `hydro_coroutine_new` (async only) or
`hydro_generator_new` (generator only); `argv` contains the captures followed
by the call arguments. The fiber starts only on the first `hydro_iter_next`
(generator), `hydro_await`/`asyncio.run` (coroutine) or
`__anext__`/`asend` (async generator).

Exception tags: `0 ValueError, 1 TypeError, 2 ZeroDivisionError,
3 RuntimeError, 4 OverflowError, 5 KeyboardInterrupt, 6 IndexError,
7 KeyError, 8 StopIteration, 9 AttributeError, 10 OSError, 11 GeneratorExit,
12 Exception, 13 BaseException, 14 AssertionError, 15 FileNotFoundError,
16 NameError, 17 SystemExit, 18 StopAsyncIteration, 19 CancelledError,
20 Skipped, 21 XFailed, 22 SystemError, 23 MemoryError, 24 RecursionError,
25 SyntaxError, 26 IndentationError, 27 TabError, 28 ArithmeticError,
29 LookupError, 30 ImportError, 31 ModuleNotFoundError, 32 NotImplementedError,
33 EOFError, 34 UnicodeError, 35 UnicodeDecodeError, 36 UnicodeEncodeError,
37 UnicodeTranslateError, 38 BufferError, 39 ReferenceError,
40 ExceptionGroup, 41 BaseExceptionGroup, 42 Warning, 43 UserWarning,
44 DeprecationWarning, 45 PendingDeprecationWarning, 46 SyntaxWarning,
47 RuntimeWarning, 48 FutureWarning, 49 ImportWarning, 50 UnicodeWarning,
51 BytesWarning, 52 ResourceWarning, 53 FloatingPointError,
54 BlockingIOError, 55 ChildProcessError, 56 ConnectionError,
57 BrokenPipeError, 58 ConnectionAbortedError, 59 ConnectionRefusedError,
60 ConnectionResetError, 61 FileExistsError, 62 InterruptedError,
63 IsADirectoryError, 64 NotADirectoryError, 65 PermissionError,
66 ProcessLookupError, 67 TimeoutError`.
Tags `0..=18` are frozen by the ABI; `19..=21` are runtime-internal
(`CancelledError`, `Skipped`, `XFailed`) and the CPython taxonomy was appended
from `22` without renumbering. `IOError` and `EnvironmentError` are aliases of
`OSError` (tag `10`, class name `OSError`), exactly as in CPython.

`except` dispatch walks the CPython parent chain (`exc::parents` in the
runtime): `ArithmeticError` catches `ZeroDivisionError`/`OverflowError`/
`FloatingPointError`, `LookupError` catches `IndexError`/`KeyError`,
`ImportError` catches `ModuleNotFoundError`, `SyntaxError` catches
`IndentationError`, which catches `TabError`, `UnicodeError` derives from
`ValueError`, every `OSError` subclass (`FileNotFoundError`,
`PermissionError`, `TimeoutError`, `ConnectionError` and its four subclasses,
...) is caught by `OSError`, the warning categories derive from `Warning`, and
`ExceptionGroup` derives from both `BaseExceptionGroup` and `Exception`.
`BaseException` catches every tag; `Exception` catches every tag except the
`BaseException`-only children (`SystemExit`, `KeyboardInterrupt`,
`GeneratorExit`, `CancelledError`, `BaseExceptionGroup`). An unknown pending
tag matches only `BaseException`/`Exception`, and an unknown handler tag never
matches. Both the interpreter and codegen delegate this decision to
`hydro_exc_matches`; there is no second hierarchy table.

`raise`/`except` resolve tags from names directly. An unqualified exception
name used as a value lowers to `hydro_exception_class(tag)`, which returns one
cached class object per tag; the class's base is the cached class of the tag's
parent, so `issubclass(SyntaxError, Exception)` holds and `isinstance(e,
OSError)` is true for a `FileNotFoundError` object. `issubclass` is a
source-level two-argument builtin. `isinstance(x, object)` is true for every
Hydro value. Explicit causes are stored on the exception and traced by the GC.
An exception object exposes `args` (the rendered raise arguments),
`__class__`, `__cause__`, `__context__`, `__suppress_context__` and
`__traceback__` (always `None`: Hydro records the raise location but has no
traceback object).

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

- Python `int` is arbitrary precision. Tagged immediates (`payload << 3`)
  cover `-2^60 ..= 2^60-1`; arithmetic results outside the tagged range
  promote to `Type::BigInt`/`ObjData::BigInt` (heap, `num-bigint`) exactly.
  Statically typed `Int` operations keep the checked tagged fast path and
  guard heap operands before untagging; `BigInt` expressions use the generic
  runtime operators.
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
  Codegen calls `hydro_format_value`; conversion and spec are applied before
  formatting. Invalid specs → `ValueError`, wrong types → `TypeError`.
- `str.format(...)` supports positional/named fields, index/attribute
  paths and format specs. The spec logic lives centrally in
  `hydro-runtime/src/core/format.rs` and is shared by f-strings, `str.format` and
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
  evaluated nested, tuple targets unpacked. List/set/dict comprehensions
  build their container eagerly, while a generator expression is lazy: the
  frontend synthesizes a generator function whose outermost iterable is
  evaluated when the generator is created and whose remaining clauses,
  filters and element run on each resume (enclosing locals are passed as
  capture parameters). Comprehension variables are synthetic locals and do
  not leak.
- Dicts store their insertion order in an entry list plus a
  hash index over the key hashes (`DictData`). Buckets are verified with
  Python equality (first position, last value); numeric
  keys are int/float/bool-equivalent. Lookup/insert are therefore O(1) on
  average; `pop` reindexes. Container mutation during an iteration is
  index-based and undefined.

#### Functions, Closures, Classes

- Named functions and lambdas are values (`FunctionRef`); indirect calls
  go through `hydro_call` (wrapper with argv). Lambdas are parameter-only; an
  access to outer names inside a lambda is a diagnostic.
- Builtins are values as well: `f = int`, `map(f, xs)` etc. materialize
  a `FunctionBody::Builtin` object via `hydro_builtin_function`.
- **Closures capture cells for mutable bindings** (Phase 2.39): a local
  that a nested function reads and that is assigned here (or through
  `nonlocal`) lives in a heap `Cell` (`hydro_cell_new/get/set`). The outer
  function reads and writes through the cell and the closure captures the
  cell value, so mutations are shared in both directions. Read-only
  captures keep by-value semantics, and a loop creates a fresh cell per
  iteration for every cell the body mutates and a closure created in that
  body captures (closures created outside keep the enclosing cell).
  `nonlocal name` resolves to
  the nearest enclosing binding; a missing or conflicting binding is a
  compile diagnostic. Captures are passed as leading parameters to the
  wrapper (`capture_count` in the signature). Recursive nested functions and
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
  `hydro_method` by receiver type and class chain; attributes via
  `hydro_getattr`/`hydro_setattr`. Instances expose `__class__`; classes expose
  their frozen attributes and `__name__`. Zero-argument
  `super().__init__(...)` calls the direct base initializer; not supported:
  other class decorators, metaclasses, `class` keywords, multiple inheritance,
  other `super()` forms and the general descriptor protocol. Common user
  dunders (`__repr__`/`__str__`/`__len__`/`__contains__`/operators/`__call__`)
  are dispatched; otherwise instances render
  deterministically as `<ClassName object>` (no heap address), classes as
  `<class 'Name'>`.
- **Attributes** (`hydro_getattr`): `pathlib.Path` (`name`, `stem`, `suffix`,
  `parent`, `path`), files (`name`, `mode`, `closed`, `encoding`),
  `threading.Thread` (`name`, `ident`, `daemon`) and
  `multiprocessing.Process` (`name`, `pid`, `exitcode`, `daemon`) provide
  data attributes. Built-in exception objects expose `args`, `__class__`,
  `__cause__`, `__context__`, `__suppress_context__` and `__traceback__`
  (always `None`). For native objects, names without a call are data
  attributes; methods must be called. Unknown names → `AttributeError`.
  `hydro_setattr` allows only instance attributes; classes are immutable.
- **Method keywords** (`hydro_method_kw`): native methods with named
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
  trailing key slot. `hydro_method_star` flattens one list/tuple operand
  (`t.add_row(*cells, style=style)`). Other native methods reject keywords
  (runtime/frontend diagnostic).

#### Generators and async

- **Generators** (Phase 2.32): A function with `yield` is compiled as a
  fiber (`core/fiber.rs`). The call creates a `Fiber` object with its
  own 128-KiB stack; on x86_64 the context switch is
  `global_asm!`-`hydro_context_switch` over the callee-saved registers `rsp`,
  `rbx`, `rbp`, `r12`–`r15` (`ret`-based, no syscalls); on other
  targets libc `ucontext` remains the fallback. The wrapper runs only on the
  first `hydro_iter_next`. `yield value`/bare `yield` suspend the fiber,
  `return` ends the iteration, exceptions are propagated on the next resume.
  Every object is independent; `for`, `list`, `sum`, `iter(x)` and
  `next(it[, default])` work on it. `send(v)` resumes the generator and
  returns the next yielded value; `x = yield v` evaluates to the sent value
  (`None` for `next`). `throw(exc)` raises an exception *instance* at the
  suspension point (the fiber ABI raises exactly the value it is given, so
  only the one-argument, instance form is supported). `close()` injects
  `GeneratorExit`, so `finally` blocks run; a generator that yields during
  `close()` raises `RuntimeError("generator ignored GeneratorExit")`.
  Exhausted generators raise `StopIteration` from `send`/`throw`.
  `yield from iterable` delegates iteration (`hydro_yield_from`): each
  delegated item is yielded, `send`/`throw` are forwarded to a delegated
  generator, and `close()` closes the delegate first so its `finally` blocks
  run. The expression evaluates to the delegate's return value (`None` for a
  plain iterator). Delegating to a running generator raises `RuntimeError`.
  Not supported: none of the generator protocol is missing; PEP 479
  (converting a body-raised `StopIteration` into `RuntimeError`) is not
  implemented. Generator expressions are lazy (a synthetic generator
  function; the outermost iterable is evaluated at creation). Deviations: `send` into a plain-iterator
  delegate raises `AttributeError` naming the iterator as `'iterator'` (not
  e.g. `'list_iterator'`).
- **Async generators** (Phase 2.12): `async def` with `yield` compiles to a
  `FiberKind::AsyncGenerator` (`hydro_async_generator_new`). `__aiter__`
  returns the object, `__anext__` resumes it and returns the item directly
  (or raises `StopAsyncIteration`, tag 18), and `asend`/`athrow`/`aclose`
  mirror `send`/`throw`/`close` with async completion semantics. Awaits
  inside the generator propagate their wait request through the enclosing
  coroutine, so `asyncio.sleep` inside an async generator suspends only the
  running task. `async for`/`async with` and asynchronous comprehensions are
  real await loops (`__aiter__`/`__anext__`/`__aenter__`/`__aexit__`).
- **async** (Phase 2.12): `async def` creates a coroutine object;
  `await`/`hydro_await` drives it to completion (non-fiber values are
  returned unchanged) and rejects generators/async generators. Suspensions
  carry runtime-internal wait requests (sleep/join) outwards; an awaiting
  chain is bounded to 512 frames. A thread-local persistent loop
  (`modules/asyncio.rs`) owns a `Driver` whose scheduled-task list is a
  heap persistent root, so tasks stay reachable while queued in Rust
  vectors. `asyncio.run(coro)` drives the root, services `sleep`, cancels
  leftover tasks at shutdown and resets the queues; a reentrant `run` is a
  `RuntimeError`. `create_task(coro)` schedules a task on the running loop
  and returns it; tasks expose `cancel`/`done`/`cancelled`/`result`/
  `exception`, and `current_task()` reports the running task.
  Cancellation queues `CancelledError` (tag 19, a `BaseException` excluded
  from `Exception`) in the fiber and delivers it at the next suspension
  resume; `cancel` before the first step finishes the task without running
  its body. `asyncio.gather(*awaitables)` joins concurrent tasks in input
  order and propagates the first failure. `asyncio.TaskGroup` is a native
  class whose `__aenter__`/`create_task`/`__aexit__` join the group,
  cancel siblings on the first failure, cancel remaining tasks when the
  block raises and propagate the first error. `TaskGroup` is exposed as a
  zero-argument spec function (the frontend types module members as functions)
  whose runtime arm returns a `TaskGroup` instance, so both
  `asyncio.TaskGroup()` and `from asyncio import TaskGroup` construct usable
  async context managers. Deviations: no
  `ExceptionGroup` (the first error propagates directly), no `wait_for`,
  and async generators are not automatically closed when their last
  consumer is cancelled.

#### `with` Protocol

`with a as x, b as y:` becomes nested `try` blocks, one per item, in the
shape CPython uses:

```text
$with = <context expression>
$wexc = None
[x =] $with.__enter__()
try:
    <body>
except BaseException as $wexc:
    if not $with.__exit__(type($wexc), $wexc, None):
        raise
finally:
    if $wexc is None:
        $with.__exit__(None, None, None)
```

Every context expression is evaluated exactly once into a hidden `$withN`
local, and its `__enter__()` result is bound to the `as` target (without
`as`, `__enter__` still runs). The entry happens **before** the `try`, so a
context expression or `__enter__()` that raises never calls that manager's
own `__exit__` (an enclosing manager still gets its exit call, as in
CPython).

The return value of `__exit__` is honoured: a truthy value suppresses the
exception raised by the body and execution continues after the `with`; a
falsy value re-raises the original exception through a bare `raise`. A
`None` return therefore propagates, so files, `Pool` and user classes that
implement the protocol without returning anything keep their old behaviour.

`__exit__` runs exactly once per manager on every path. On an exception the
handler performs the single call, and if that call itself raises, its
exception replaces the in-flight one — the `$wexc` guard then keeps the
`finally` arm from calling it a second time. On normal completion the
`finally` arm calls `__exit__(None, None, None)`; because it is a `finally`,
`return`, `break` and `continue` leaving the body also run it. Managers are
evaluated left to right and exited in reverse order, and a manager that
suppresses the exception leaves the outer managers with no exception to see.
Hydro has no traceback objects, so `__exit__` always receives `None` as its
third argument.

#### Imports, User Modules, `__name__`

- Native modules are specified via `hydro_ir::stdlib` (see below).
  `import X [as Y]` and `from X import a [as b]` bind eagerly at module
  level; imported names are visible inside functions. Imports nested in a
  function, class or compound body are **deferred**: the loader records them
  without loading the module, names that can still be linked to an
  already-visible dependency bind normally, and every other binding lowers to
  `DeferredImport`, which calls `hydro_deferred_import` and raises
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
  `hydro_global_set`/`hydro_global_get`); other module locals remain invisible
  inside functions. Imported units mirror every export because an importer may
  read any name; the entry module mirrors only names its own functions read
  (precomputed before lowering), so top-level loops do not emit a global store
  per iteration.
- **User modules and packages** (Phase 2.30; package loader extended in the
  P7 checkpoint): `import foo` / `from foo import f` resolve `foo.py` and
  packages (`foo/__init__.py`), including dotted names, submodules and
  relative imports. Search order: sibling files, the package-chain root,
  `HYDRO_SITE_PACKAGES`, the `VIRTUAL_ENV`/`CONDA_PREFIX` site-packages trees,
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

Central specification in `hydro_ir::stdlib` (`MODULES` in
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
- `argparse`: an `ArgumentParser` with the common constructor parameters
  (`prog`, `usage`, `description`, `epilog`, `parents`, `formatter_class`,
  `prefix_chars`, `fromfile_prefix_chars`, `argument_default`,
  `conflict_handler`, `add_help`, `allow_abbrev`, `exit_on_error`), the
  `store`/`store_true`/`store_false`/`store_const`/`append`/`append_const`/
  `count`/`help`/`version`/`BooleanOptionalAction` actions, `nargs`
  (`None`, `?`, `*`, `+`, `N`), `type`, `choices`, `default`, `required`,
  `const`, `metavar` and `dest`; argument groups, mutually exclusive groups
  and subparsers; `set_defaults`, `parse_known_args` and a `Namespace`
  result. The token scanner handles `--opt=value`, short clusters, `--`,
  unique long-option abbreviations and negative numbers. Parse errors print
  the usage block to stderr and raise `SystemExit(2)`; `-h`/`--help` and
  `--version` print to stdout and raise `SystemExit(0)`. `parse_args` reads
  `sys.argv[1:]` by default. The legacy non-exiting explicit-argv behaviour
  is confined to the internal `_hydro_test_mode` used by the runtime tests.
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
  `getrandbits`; `getrandbits(k)` returns an exact non-negative `k`-bit
  tagged/BigInt integer (bounded by the runtime BigInt cap).
  Deterministic per seed, but **not CPython-reproducible**.
- `sys.stdout`/`sys.stderr`: small native text streams over fd 1/2 with
  `write(str)` (returns the character count, flushes), `flush()` and
  `isatty()`. They are fresh GC-safe objects per resolution (no Hydro
  children) and expose no other attributes.
- `platform`: `system`/`machine`/`release`/`version`/`node`/`platform`/
  `python_version`, `python_implementation() == "Hydro"`.
- `statistics`: `mean`/`fmean`/`median`/`pstdev`/`stdev` including error paths.
- `pathlib`: native `Path` (`Path()`, `cwd()`, `home()`) with
  `exists`/`is_file`/`is_dir`, `read_text`/`write_text`, `read_bytes`/
  `write_bytes`, `open`, `mkdir`, sorted `iterdir`/`glob` (`*`/`?`, no
  `**`), `joinpath`/`resolve`/`absolute`/`as_posix`, `/` join and
  data attributes `name`/`stem`/`suffix`/`parent`/`path`. `os.remove`/
  `rename` accept path objects.
- `asyncio`: see §3.3 "Generators and async" (`run`/`sleep`/`gather`,
  `create_task`/`current_task`, the `TaskGroup` class and the
  `CancelledError` constant).

#### Methods

`hydro_method`/`hydro_method_kw` dispatch dynamically by receiver type.
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
  receive IDs; `_thread.join` is a Hydro extension. Locks
  (`_thread.allocate_lock`) are native `Lock` objects; `threading` provides
  `Thread`, `Lock`, `RLock` (owner/depth), `Event` and `queue.Queue`.
- `multiprocessing` provides fork-based `Process` objects, `os.fork`/
  `waitpid`/`_exit` and `multiprocessing.Queue` (socketpair, HYD1
  serialization). `spawn` starts a fresh process via `exec`
  (`tea __spawn` resp. AOT binary); `hydro_main` registers all functions before
  the module body, a payload names the target name + args, and only that function
  runs in the child (queue FDs are inherited; no module body in the child).
  `Pool` uses per-worker private task/result queues for fork and spawn and
  implements `__enter__`/`__exit__` (terminate).
- `pickle.dumps/loads` uses its own HYD1 hex format that is not CPython-compatible
  (None/Bool/Int/Float/Str/List/Tuple/Dict/Set/Range/Bytes/
  function wrapper); cycle-free, depth 128. Functions are reconstructed by
  wrapper address/name only in the same, forked or spawn-
  registered address space; native objects/lambdas are
  rejected.
- Shared mutable containers require user locks; without a GIL,
  unprotected mutations are data races and not supported behavior.

#### Profile collection ABI (Tea PGO)

Hydro exposes three profile-entry symbols used by `tea steep` and by AOT
builds with `--flavor`:

| Symbol | Signature | Contract |
|---|---|---|
| `hydro_profile_reset` | `() -> void` | Clear the process-profile map. |
| `hydro_profile_hit` | `(const u8 *name, usize len) -> void` | Increment the entry count for a function name; null/non-UTF-8 names are ignored. |
| `hydro_profile_dump` | `(const u8 *path, usize len) -> i32` | Write `# hydro-profile-v1` plus sorted `<count>\t<name>` lines; returns 0 on success, -1 on invalid path/IO. |

Codegen uses `CodegenOptions { collect_profile, hot_functions }`:
`collect_profile` emits a `hydro_profile_hit` at each IR-function entry; a
non-empty `hot_functions` map lowers real function bodies in descending
observed-count order (stable by original id) so hot code is emitted first.
Default options emit no profile calls and preserve the historical object.
Tea owns the durable flavor JSON and merges profile runs; Hydro only consumes
the hot-function map.

### 3.4 Error Protocol

Error-capable runtime calls set a **pending exception** on failure and
return a dummy value. After *every* error-capable operation, codegen checks
`hydro_exc_pending()` and jumps to the next handler (or to the uncaught path).
An exception carries tag, message, rendered `args`, function name and line at
the time of the `raise`. User exception classes have no runtime tag: `raise UserClass(msg)`
and `raise module.UserClass(msg)` lower to `RuntimeError` with a
`"<class name>: <message>"` string, and handlers for those classes (by name
or dotted name, including imported classes) match `RuntimeError`. This
handler matching is documented as approximate: every user class shares the
`RuntimeError` tag, so unrelated user-class raises are caught too.
`hydro_uncaught()` prints:

```
Traceback (most recent call last):
  File "test.py", line 7, in main
ZeroDivisionError: division by zero
```

and returns exit code `1`. An uncaught `SystemExit` prints no traceback:
an integer code becomes the process status (masked to one byte), a message
is printed to stderr with status 1, and `SystemExit()` exits 0.

## 4. Backend Decision

**Cranelift 0.120** (pure Rust) for Phases 1–3; LLVM optionally later behind
a feature flag.

Rationale: no system LLVM/`llvm-config`/headers needed, fast builds,
JIT and object emission from one codebase, solid baseline optimization.
Costs: weaker optimization than LLVM (PGO/inlining, P5) and no
ORC-JIT tiering. Codegen remains encapsulated in `hydro-codegen`, so an
LLVM backend can later consume the same IR. The decision is documented in P0-11.

The shared `build_isa` enables `opt_level=speed` and `enable_alias_analysis`;
the Cranelift IR verifier runs only in debug/test builds, so release JIT
startup and AOT compilation do not pay for it. The JIT uses
`is_pic=false`; AOT emits PIC (`is_pic=true`) because the produced object is
linked into a PIE. `use_colocated_libcalls` stays `false`: runtime symbols
are external, so assuming colocation would be incorrect. A future optional
non-PIE AOT mode and Cranelift PGO/LLVM remain measurement-driven P5 work.

### 4.1 AOT Linking

`tea build` produces an object file (`.o`) and links it with `cc` against
`libhydro_runtime.a` (staticlib of the workspace). The generated binary exports
`main(argc, argv) -> int`, which calls `hydro_main() -> int`. `hydro_main`
executes the module body (incl. embedded user modules) and returns the
`hydro_uncaught()` exit code on a pending exception, otherwise `0`. The binary
needs neither CPython nor `tea`.

Runtime library resolution: `--runtime-lib <path>`, otherwise `HYDRO_RUNTIME_LIB`,
otherwise relative to the CLI executable (`target/<profile>/libhydro_runtime.a`).
If it is missing, a clear error message with a build hint.

The build cache lives under `.hydro/cache` and is a purely local
optimization. The key covers CLI/runtime version, target architecture/OS,
source text, all reachable `.py` modules (relative path + content) and the
identity of the runtime archives. `--no-cache` bypasses it; `tea clean` removes
it. The cache is not a shared artifact store.

## 5. IR Contract

`hydro-ir` is deliberately small and fully typed:

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
  `ListComp`, `SetComp`, `DictComp`, `GeneratorExp` (lazy), `Await`,
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
  cause, rendered deepest-first; `from None` suppresses it. A raise inside an
  active handler stores the caught exception as `__context__` and renders
  "During handling of the above exception ...", suppressed by
  `__suppress_context__`; `except*` is not implemented.
- `ForEach` iterates lists/strings/dicts via the runtime iterators;
  `ForRange` remains the syntactically specialized `range(...)` form (since
  Phase 2.7, `range` is additionally a value). Negative step is
  allowed, step `0` is a runtime `ValueError`.
- `Delete { targets }` evaluates targets left to right and deletes each one
  immediately: locals (slot cleared to `None`; Hydro does not track unbound
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
- `tuple()`/`tuple(iterable)` materialise a tuple (`hydro_tuple_from`), and
  `hasattr(obj, name)` runs the attribute lookup, swallowing only
  `AttributeError`. Starred unpack targets (`a, *rest, b = value`) bind a
  fresh list of the middle elements through `hydro_unpack_star`; one star is
  allowed and it may not nest inside another container target.
- `functools.partial(func, *args, **kwargs)` keeps keyword names: the
  frontend forwards them through `hydro_module_call_kw`, the runtime stores
  `(name, value)` pairs in the partial and applies them at call time using
  the parameter names codegen attaches to every generated function value
  (`hydro_function_params`). A callable without parameter names rejects
  keywords with `TypeError`; a hole before a named parameter is reported
  instead of silently passing `None`.
- Functions carry `is_generator` and `is_async`; codegen derives the
  flags in `hydro_function_new`/`hydro_closure_new` from them (bit 0 async, bit 1
  generator) and generates a wrapper that calls `hydro_async_generator_new`
  when both are set, else `hydro_coroutine_new` resp. `hydro_generator_new`.
  `Yield` statements become `hydro_yield` (result
  discarded); `Yield` expressions keep the returned sent value; `YieldFrom`
  becomes `hydro_yield_from`. `Await` becomes `hydro_await`.
- **Lowering:** `with`/`async with` become nested `try/finally` with
  `__enter__`/`__exit__`; async managers bind `__aenter__`/`__aexit__`
  through awaited method calls. Augmented subscript assignment
  (`a[i] += x`) evaluates object and index exactly once into `$aug` locals.
  `async for` lowers to `__aiter__()` plus a `while` loop that awaits
  `__anext__()` and catches `StopAsyncIteration`.
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
  `format()` → Str, `isinstance()`/`issubclass()` → Bool, `repr()` → Str,
  `hash()` → Int,
  `max`/`min`/`round` → Any. Bitwise `&`/`|`/`^`/`<<`/`>>` on
  `Int`/`Bool` operands → Int; other operand combinations stay `Any` and
  raise `TypeError` at runtime.
  `Is`/`IsNot` → Bool for every operand pair and does not narrow either
  operand (`x is None` leaves `x` dynamic). `IfExp` joins the branch types;
  incompatible branches become `Any`.
- Comprehensions: ListComp → List, SetComp → Set, DictComp → Dict.
  Generator expressions are lowered to a synthetic generator function and
  typed dynamically. Targets bind synthetic locals.
- Name/Lambda → Function; indirect call: callee Function/Any, result Any;
  `f = int` is a function (`BuiltinRef`). Closures capture exclusively
  existing locals; `capture_count` is derived from the first use in the
  nested body.
- Modules/imports: native module attributes/calls are typed via
  `hydro_ir::stdlib`; user module imports bind exported function signatures.
  Module-level imports are globally visible. A name bound by a nested import
  is visible too, but has the dynamic `DeferredImport` type: using it raises
  at runtime instead of failing the type check.
- Classes: `class` binds a dynamic class value (no static
  instance type); method calls via instances are checked dynamically,
  calls with `Any` fall back to runtime checks.
- Statically `Int`-typed operands use checked int fast paths in codegen:
  `+`/`-`/`*` (untag, `sadd_overflow`/`ssub_overflow`/
  `smul_overflow`, range check, retag) and comparisons; on overflow or a
  non-`Int` result it goes through `hydro_binop`. The same applies to `AugAssign`
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
- Roots: **precise shadow frames** plus a conservative fallback. Generated
  code registers every frame with `hydro_gc_frame_push` (all slots `None`-filled,
  scanned prefix narrowed with `hydro_gc_frame_used`) and every caller-owned
  value array with `hydro_gc_scope_push`; the collector marks those slots
  exactly. A mutator publishes its thread-local frame/range set at each
  safepoint slow path and before `blocking_enter` parks, so a shared
  collection snapshots every stopped thread's roots (`snapshot_all`); the
  local collector reads them directly (`snapshot_current`). A frame is dropped
  on thread exit and in a forked child (`clear_current`/`clear_thread`), so no
  dangling slot is ever dereferenced. Tagged values are still validated
  against the registry address index, so a stale slot (or a conservative false
  hit) can only be ignored, never dereferenced.
- **Root modes:** by default the collector is *mixed*: precise slots are
  marked exactly and their byte extents are excluded from the conservative
  native-stack/register scan (merged ranges are skipped), while unregistered
  Rust frames and suspended fiber stacks are still scanned. `HYDRO_GC_PRECISE=1`
  selects *strict* mode: every conservative stack and register scan is
  disabled, so only precise frames/ranges, pending/handled/thread roots,
  suspended fibers' `SavedRoots`, C-API shim roots and persistent roots keep
  objects alive. Strict mode therefore requires native code to root its tagged
  locals explicitly; the runtime's own shims, module globals and test suites
  that hold raw Rust locals (`gc.rs`, `concurrent_gc.rs`, `stw_gc.rs`,
  `gc_properties.rs`) are mixed-mode only, and `tests/precise_gc.rs` proves
  strict mode end to end.
- **Persistent roots:** `heap::add_persistent_root`/`set_persistent_root`/
  `remove_persistent_root` maintain a per-thread slot vector in `ThreadGc`
  that is marked by every collection. They are for long-lived Rust-side owners
  (asyncio loop tasks) and are cleared on thread exit and by
  `hydro_runtime_init` (which also resets the asyncio loop state before the heap
  is freed). The hidden `hydro_runtime::precise_roots` module re-exports them for
  the integration tests.
- Conservative fallback (mixed mode): native stack scan (bounds via
  `pthread_getattr_np`), a register dump (x86_64/aarch64) and the pending
  exception.
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
  with already registered lifetime ranges. `HYDRO_GC_STRESS=1` covers
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
- Trigger on growth since the last collection: `live + HYDRO_GC_THRESHOLD`
  (default 10000), not on absolute object count. `HYDRO_GC_STRESS=1` collects
  before every allocation. `HYDRO_GC_STATS=1` reports pause, mark and sweep time
  as well as frees per collection on stderr.
- `hydro_runtime::gc_stats()` returns a `GcStats` snapshot
  (`collections`, `pause_total_us`, `pause_max_us`, `mark_total_us`,
  `sweep_total_us`, `freed_total`, `live_objects`, `threshold`, `stress`,
  `precise`, `precise_frames`); `precise` reports strict mode and
  `precise_frames` the frames marked by the latest collection. The
  `HYDRO_GC_STATS=1` line carries the same `precise=mixed|strict` and
  `precise_frames=` fields. `reset_gc_stats()` resets the counters.
  `tea gc [--json] <file.py>`
  runs the program like `tea infuse` and prints the statistics afterwards;
  with `--json` the last stdout line is an object with snake_case keys.
- `gc/alloc.rs` keeps a fixed 16-byte `HydroObj` header separate from a typed
  payload block. Payloads use 18 size classes (8..4096 bytes) with per-class
  slabs/free lists and an exact large-object fallback; the header has its own
  slab class. Stats cover per-class slabs/live/reused/bytes, large blocks and
  peak live bytes. `ObjData` remains the construction descriptor and
  `ObjDataRef`/`ObjDataMut` provide typed access.
- `hydro_gc_collect()` forces a collection; `hydro_gc_objects()` returns the
  live object count. `hydro_runtime_init()` resets heap and exception state.

This is deliberately an **interim**: in mixed mode conservative scanning can
preserve false positives; the final root rescan is STW and the white partition
runs briefly under the heap lock. Strict precise mode removes the conservative
fallback entirely but requires every native stack to root its tagged locals.
The
`unsafe` policy: The GC scan/free paths are encapsulated in `hydro-runtime/src/gc/`
and justify their invariants there.

### 7.1 Concurrency (P2-20)

**No GIL.** Since Phase 2.12 there is a global shared-heap mode
(heap migration on the first thread), a thread registry and native
`_thread` primitives. Since Phase 2.13/2.15 a **concurrent
mark-&-sweep collector** collects even while threads run: Codegen calls
`hydro_safepoint()` at function entries and (since Phase 2.15 only every 64th
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
`tea help`):

`new`, `init`, `check`, `run`, `build`, `test`, `fmt`, `lint`, `clean`,
`gc`, `add`, `remove`, `sync`, `lock`, `doctor`, `help`, `--version`.

- `new`/`init` create `main.py`, `pyproject.toml` and `.gitignore`;
  existing non-empty files are not overwritten without `--force`.
- `test` runs `test_*.py`/`*_test.py` with `tea infuse`; success = exit 0
  (no pytest assertions). `--json` returns a result list.
- `fmt` normalizes trailing whitespace and exactly one final newline
  (no AST formatter); `--check` does not write. `lint` type-checks all
  `.py` files; `--json` aggregates diagnostics.
- `clean` removes `.hydro/cache` and `target/hydro-tmp` (`--dry-run`).
- `gc` compiles and starts a program like `run` and prints the
  GC statistics afterwards (`--json` for a machine-readable last line).
- `add`/`remove`/`sync`/`lock` are a **passthrough** to `uv`
  (`HYDRO_UV` overrides the executable). Hydro interprets neither specs nor
  `pyproject.toml`; the uv exit status is returned.
- The build cache (§4.1) is implemented; `--no-cache` bypasses it.

`uv` remains an external dependency for now: Hydro uses uv for resolution,
lockfiles and venvs, but does **not yet** inject the Hydro interpreter
(`.python-version`, venv markers, `sys.executable` equivalent). A dedicated
fork is intended only in case of an architectural blocker. Own files:
`pyproject.toml` (compatible), `tea.lock` (optionally additional), build cache
under `.hydro/cache/`.

## 9. C API/ABI Strategy (Phase 4)

**Decision (2026-10-24): option B — a Hydro-native CPython C-API/ABI layer.
No `libpython` in the production runtime.**

CPython 3.14 development files are used at build time to compile extensions
and test fixtures; the CPython interpreter serves as a differential test
oracle only. `ldd tea` must not contain `libpython`. One process keeps the
Hydro tagged-value model, the concurrent GC and the fast startup path; foreign
code sees refcounted `PyObject` handles whose lifetime is integrated with the
collector (refcount as an additional root, `tp_traverse`/`tp_clear` mirroring
GC edges). The GIL is emulated only while foreign code runs; Hydro code remains
GIL-free.

Two tiers: (1) `abi3`/limited API (module init, `PyArg_Parse*`, scalar and
container constructors, errors, capsules, buffer basics); (2) the full
CPython 3.14 ABI required by NumPy 2.5 and `pydantic-core` (`PyTypeObject`
layout, vectorcall, full buffer protocol, GC integration, package/wheel
loading).

**Implemented (2026-10-24, first abi3 slice):** the shim in
`hydro-runtime/src/capi/` exports the limited-API module/scalar/error surface
with a C-variadic `PyArg_ParseTuple`; the frontend resolves sibling
`.so`/`.abi3.so` files at compile time and lowers `ext.member(...)`/
`from ext import member` to `Callee::Extension`/`ExprKind::ExtensionAttr`;
`hydro_extension_call`/`hydro_extension_get` load and cache modules lazily
(`modules/extension.rs`). Scalars, strings, bytes, floats, lists, tuples and
dicts convert recursively in both directions (bounded copies); capsules and
read-only contiguous bytes buffers are supported. Extension discovery includes
sibling files, `HYDRO_EXTENSION_PATH`, `PYTHONPATH`, `HYDRO_SITE_PACKAGES` and
venv/conda `site-packages`; dotted submodules (`pkg.sub_ext`) resolve through
a directory path without package Python source. Wheel tags are validated
(`.so`, `.abi3.so`, `.cpython-314-*`; others are compile-time diagnostics), and
both single- and multi-phase init (`PyModuleDef_Init`/`Py_mod_exec`) are
supported. `tea` and AOT binaries export the shim symbols with
`--export-dynamic`; fixtures import and call under both with no `libpython`.
The full 3.14 ABI (NumPy/pydantic-core) remains open.

Evidence: embedded CPython costs ~6.7 ms `Py_Initialize` + ~7.5 ms
`Py_Finalize` and ~41.6 ms for `import numpy`, versus ~0.8–1.0 ms for a Hydro
AOT process; a stable-ABI probe imports with four undefined CPython symbols
(the initial shim surface). Full rationale, milestones and acceptance gates:
[`C_API.md`](C_API.md). The 297-module stdlib program (174 pure Python,
63 extension, 52 builtin/frozen, 8 platform-specific modules), the ecosystem
suites and their
benchmarks are tracked as P4-01…P4-11 in [`TODO.md`](TODO.md).

## 10. Verification and Documented Limits

- `scripts/verify.sh`: fmt-check, clippy (`-D warnings`), build,
  `cargo test --workspace`, E2E examples (JIT **and** AOT), negative tests
  (diagnostics/tracebacks), GC threshold/stress.
- Test counts and the per-crate breakdown are canonical in
  [`AGENTS.md`](../AGENTS.md#current-state). E2E coverage
  includes, among others, formatting, classes, closures, lazy generators/coroutines
  (`fibers.rs`), user modules, `unittest` runner, stdlib Batches A/B/bench,
  `argparse`/`defaultdict`/`re`,
  socket echo, pathlib, build cache, `tea gc` and uv passthrough.
- Generated stdlib import matrix (2026-10-26): **63/297 importable** (13
  builtin/frozen, 45 pure, 5 extension; report in
  [`COMPATIBILITY.md`](COMPATIBILITY.md)). `benchmarks/bench_all.py --only hydro`
  runs at the default GC threshold; rich console/table/panel/rule rendering
  works and a native `rich.status.Status` shim covers `Console.status`.
  Precise roots removed the former live-value loss.
- New unit tests cover the dict hash index, the GC statistics, fiber
  iteration, async generators, task cancellation and TaskGroup;
  `examples/generators_async.py` and `examples/async_generators.py` run
  under JIT and AOT.
- E2E compares stdout/exit code of `tea infuse` and the AOT binary; negative tests
  check diagnostics and tracebacks (stderr, exit code).
- No silent miscompilation: unsupported syntax → diagnostic.

**Limits (deliberate, documented):**

- **Language:** arbitrary-precision `int` is implemented via a tagged fast
  path plus `BigInt`; `range` bounds and a few native i64 interfaces remain.
  No general decorators (only
  `property`/`classmethod`/`staticmethod` on methods and the ignored
  `lru_cache`/`cache`/`wraps`/`abstractmethod`/`overload`/
  `runtime_checkable`), pattern matching,
  metaclasses, the general descriptor protocol, `except*`,
  `bytearray`/`memoryview`; other `super()` forms; class namespace immutable. Multiple inheritance is approximated
  by the first base (additional bases are ignored). General dynamic calls
  combine at most one `*` operand; `*args` plus `**kwargs` together still
  need a deferred call, while class constructors can bind `**mapping`.
- **Closures:** captured bindings are by value unless the outer function
  mutates them (or a descendant declares `nonlocal`), in which case the
  binding lives in a traced heap cell shared with every closure; recursive
  nested functions and closures in lambdas are diagnostics.
- **Generators/async:** real lazy generators, async generators and coroutines
  on stackful fibers; `async for`/`async with`, async comprehensions,
  cancellation, task methods and `TaskGroup` are implemented. Open:
  `ExceptionGroup`, `wait_for` and the wider `asyncio` API; `throw` accepts
  exception instances only (no three-argument form).
- **GC:** generated frames and call buffers are precise shadow-stack roots;
  `HYDRO_GC_PRECISE=1` runs the collector without conservative scanning. The
  default mixed mode keeps the conservative fallback for Rust-side frames.
- **Performance:** native module dispatch is fixed (thread-local spec cache).
  The precise root-frame bookkeeping currently makes generator resume the
  weak spot; the default-GC benchmark records generator_sum at JIT 16.07 ms /
  AOT 14.61 ms versus CPython 11.23 ms. String concatenation and float boxing
  are otherwise improved. The x86_64 fiber switch remains register-only.
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
- **Stdlib:** curated core (see §3.3); `re` uses Rust `regex` syntax,
  `socket` supports TCP/UDP/IPv6, TLS (client), non-blocking I/O and
  `select`/`poll`/`selectors`, argparse is CPython-compatible for the
  documented surface, and `random` is SplitMix64 (not CPython-reproducible).
  Full P7 module parity remains open.
- **GC:** precise shadow frames for generated code plus a conservative
  fallback in the default mixed mode (`HYDRO_GC_PRECISE=1` = strict, no
  conservative scan); size-classed slab/free-list allocator; STW root
  window.
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
