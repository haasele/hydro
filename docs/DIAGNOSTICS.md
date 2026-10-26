# Diagnostics and CLI Reference

`tea` reports every user error as a located diagnostic and reserves exit
status 2 for toolchain and internal failures. This page is the compact
reference for the rendering, the machine-readable form and the CLI flags that
control them.

## Global options

Global options are accepted before the command name. Compiler and project
commands also accept them before the first path argument.

| Option | Effect |
| --- | --- |
| `--verbose` | Print phase timings for `run`/`build` (frontend, JIT, codegen, link) and build-cache key/hit/miss decisions on stderr. `--version --verbose` adds target, backend, runtime and Python compatibility lines. |
| `--quiet` | Suppress non-error status: `new`/`init` creation messages, `fmt` write messages, `clean` removal messages, `lint` success summaries and passing `test` lines/output. Errors and diagnostics are always printed. |

`--verbose` and `--quiet` cannot be combined; the CLI reports a usage error and
exits with status 2.

`run` forwards everything after the source file to the program as
`sys.argv[1:]`, so diagnostic options must precede the file:

```sh
tea infuse --color=never main.py --program-flag value
```

`sys.argv[0]` is the source path. A `--` before the source file also ends
option parsing.

## Diagnostic options

Accepted by `check`, `run`, `gc`, `build`, `test`, `lint` and `fmt`:

| Option | Effect |
| --- | --- |
| `--json` | Emit one JSON array on stderr. For `test` the per-file result array goes to stdout; for `lint` the aggregated diagnostics go to stderr. |
| `--color=<auto\|always\|never>` | Colorize human diagnostics. `auto` (the default) colors only when stderr is a terminal and `NO_COLOR` is unset. |
| `--no-color` | Same as `--color=never`. |

## Human format

A human diagnostic is a cargo-style snippet on stderr:

```text
error[HYD0202]: 'int' object is not subscriptable
  --> main.py:2:7
   |
 2 | print(value[0])
   |       ^^^^^
   |
   = help: ...
```

The header contains the stable code. The location is `file:line:col`, followed
by the source line and a caret span. `= help:` is printed only when an
actionable hint exists.

## Codes

| Code | Area |
| --- | --- |
| `HYD0001` | Syntax / parse error |
| `HYD0002` | Unsupported language construct |
| `HYD0100` | Generic semantic error |
| `HYD0101` | Unknown name (with did-you-mean) |
| `HYD0102` | Operand or type mismatch |
| `HYD0103` | Call arity / argument error |
| `HYD0104` | Invalid `return` / `break` / `continue` placement |
| `HYD0201` | Unknown method or attribute |
| `HYD0202` | Indexing, slicing or subscript assignment |
| `HYD0301` | Annotation or declaration validation |

Codes are stable: tests and tooling may match them.

## JSON schema

`--json` prints exactly one array on stderr (no summary line):

```json
[{"file":"main.py","line":2,"col":7,"end_col":12,"code":"HYD0202","message":"'int' object is not subscriptable","help":null}]
```

`file`, `line`, `col`, `end_col` and `code` are always present. `help` is
`null` or a string. `col` is 1-based, byte-based (not a character or Unicode
scalar index) and points at the first byte of the span; `end_col` is exclusive.
A multi-byte identifier therefore spans `end_col - col` bytes.

## Exit status

| Status | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | User or program error: a diagnostic, a failed program, a failed test or an I/O error. `fmt --check` uses 1 when files need formatting. |
| `2` | Usage, setup or internal error, including a compiler panic (ICE). |

## Internal compiler errors

A panic inside the compiler is caught at the CLI boundary. The process prints a
short banner to stderr and exits with status 2:

```text
tea: internal compiler error (ICE)
version 0.1.0 target x86_64-linux
please report this with the command you ran and RUST_BACKTRACE=1 output
```

The panic payload is not copied into the banner. Set `RUST_BACKTRACE=1` for the
full backtrace from the standard panic hook.

## See also

- `docs/ARCHITECTURE.md` for the frontend diagnostic contract.
- `docs/tea.1` for the command-line manual page.
