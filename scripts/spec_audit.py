#!/usr/bin/env python3
"""Audit Hydro's ``ModuleSpec`` tables against the pinned CPython oracle.

The native standard-library surface is described statically in
``crates/hydro-ir/src/stdlib/specs.rs``.  This script parses those
``pub static <NAME>: ModuleSpec`` blocks (including shared ``const`` tables and
alias specs such as ``_json``/``json``) and asks a CPython 3.14.9 interpreter
what each corresponding module actually exports.

For modules that CPython implements in C (builtin, frozen or extension
modules), the audit compares:

* public names in ``dir(module)`` with the spec's functions and constants;
* ``inspect.signature``/positional arity for public callables;

and reports names missing from the spec, names the spec declares that CPython
does not export, and arity drift.  Modules whose CPython implementation is pure
Python are reported separately as dynamic-fallback candidates: Tier 0 can run
the original source tree instead of relying on a fixed native table.

The script never builds or runs Hydro and never edits ``specs.rs``.  The
CPython source tree defaults to the cached
``.hydro/cpython/Python-3.14.9`` checkout (the same tree used by
``scripts/cpython_compat.py``).  Importing from that tree is read-only
(``PYTHONDONTWRITEBYTECODE=1``).

Usage:
    python3 scripts/spec_audit.py
    python3 scripts/spec_audit.py --module math
    python3 scripts/spec_audit.py --module 'time,_socket' --json -
    python3 scripts/spec_audit.py --source ../cpython-3.14.9 \
        --json benchmarks/compat/spec-audit.json \
        --md benchmarks/compat/spec-audit.md

Exit codes: 0 = report written (findings are data, not failure), 1 = invalid
usage or unparsable spec file, 2 = missing/incompatible CPython source or
oracle.
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent

TARGET_PYTHON = "3.14.9"
DEFAULT_SOURCE = ROOT / ".hydro" / "cpython" / f"Python-{TARGET_PYTHON}"
DEFAULT_SPECS = ROOT / "crates" / "hydro-ir" / "src" / "stdlib" / "specs.rs"
DEFAULT_JSON = ROOT / "benchmarks" / "compat" / "spec-audit.json"
DEFAULT_MD = ROOT / "benchmarks" / "compat" / "spec-audit.md"

DEFAULT_TIMEOUT = 20.0
MAX_ERROR_CHARS = 400
MARKDOWN_NAME_LIMIT = 24
MARKDOWN_MODULE_LIMIT = 60

VERSION_RE = re.compile(r'#define\s+PY_VERSION\s+"([^"]+)"')
MODULE_RE = re.compile(
    r"\bpub\s+static\s+([A-Za-z_]\w*)\s*:\s*ModuleSpec\s*=\s*ModuleSpec\s*\{"
)
CONST_RE = re.compile(
    r"\b(?:pub\s+)?(?:const|static)\s+([A-Za-z_]\w*)\s*:\s*&\s*\[\s*"
    r"(FunctionSpec|ConstantSpec)\s*\]\s*=\s*&\[",
    re.S,
)
CALL_RE = re.compile(r"^(f|kw|kw_dynamic|c)\s*\(")
STRUCT_RE = re.compile(r"^(FunctionSpec|ConstantSpec)\s*\{")

PURE_KINDS = ("pure", "sourceless")
NATIVE_KINDS = ("builtin", "frozen", "extension", "unknown")


class ParseError(RuntimeError):
    """A ``specs.rs`` construct the parser could not understand."""


# --------------------------------------------------------------------- text
def strip_comments(source: str) -> str:
    """Replace Rust comments with spaces, preserving offsets and newlines."""
    out = list(source)
    i = 0
    n = len(source)
    while i < n:
        ch = source[i]
        if ch == '"':
            i += 1
            while i < n:
                if source[i] == "\\":
                    i += 2
                elif source[i] == '"':
                    i += 1
                    break
                else:
                    i += 1
            continue
        if ch == "/" and i + 1 < n and source[i + 1] == "/":
            out[i] = out[i + 1] = " "
            i += 2
            while i < n and source[i] != "\n":
                out[i] = " "
                i += 1
            continue
        if ch == "/" and i + 1 < n and source[i + 1] == "*":
            depth = 1
            out[i] = out[i + 1] = " "
            i += 2
            while i < n and depth:
                if source[i] == "/" and i + 1 < n and source[i + 1] == "*":
                    depth += 1
                    out[i] = out[i + 1] = " "
                    i += 2
                elif source[i] == "*" and i + 1 < n and source[i + 1] == "/":
                    depth -= 1
                    out[i] = out[i + 1] = " "
                    i += 2
                else:
                    if source[i] != "\n":
                        out[i] = " "
                    i += 1
            continue
        i += 1
    return "".join(out)


def skip_string(text: str, index: int) -> int:
    """Return the index just past the string literal starting at *index*."""
    i = index + 1
    n = len(text)
    while i < n:
        if text[i] == "\\":
            i += 2
        elif text[i] == '"':
            return i + 1
        else:
            i += 1
    return n


def find_matching(text: str, open_index: int) -> int:
    """Index of the bracket closing ``text[open_index]``."""
    pairs = {"(": ")", "[": "]", "{": "}"}
    opener = text[open_index]
    if opener not in pairs:
        raise ParseError(f"not an opening bracket at offset {open_index}: {opener!r}")
    stack = [pairs[opener]]
    i = open_index + 1
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == '"':
            i = skip_string(text, i)
            continue
        if ch in pairs:
            stack.append(pairs[ch])
        elif ch in ")]}":
            if not stack or ch != stack[-1]:
                raise ParseError(f"unbalanced bracket at offset {i}: {ch!r}")
            stack.pop()
            if not stack:
                return i
        i += 1
    raise ParseError(f"unclosed bracket at offset {open_index}")


def split_top_level(text: str, separator: str = ",") -> list[str]:
    """Split on *separator* outside strings and (), [], {} groups."""
    parts: list[str] = []
    start = 0
    depth = 0
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == '"':
            i = skip_string(text, i)
            continue
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        elif ch == separator and depth == 0:
            parts.append(text[start:i])
            start = i + 1
        i += 1
    parts.append(text[start:])
    return parts


def split_field(text: str) -> tuple[str, str] | None:
    """Split a ``key: value`` field at its first top-level colon."""
    depth = 0
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == '"':
            i = skip_string(text, i)
            continue
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        elif ch == ":" and depth == 0:
            if i + 1 < n and text[i + 1] == ":":
                i += 2
                continue
            return text[:i].strip(), text[i + 1 :].strip()
        i += 1
    return None


def unquote(token: str) -> str:
    token = token.strip()
    if len(token) < 2 or token[0] != '"' or token[-1] != '"':
        raise ParseError(f"expected a string literal, got {token!r}")
    body = token[1:-1]
    escapes = {
        "n": "\n",
        "r": "\r",
        "t": "\t",
        "0": "\0",
        '"': '"',
        "\\": "\\",
    }

    def replace(match: re.Match[str]) -> str:
        ch = match.group(1)
        return escapes.get(ch, "\\" + ch)

    return re.sub(r"\\(.)", replace, body, flags=re.S)


def string_literals(text: str) -> list[str]:
    return [unquote(match.group(0)) for match in re.finditer(r'"(?:[^"\\]|\\.)*"', text)]


def parse_usize(token: str) -> int | None:
    token = token.strip()
    if token in ("usize::MAX", "usize :: MAX"):
        return None
    if token.endswith("::MAX"):
        return None
    try:
        return int(token.replace("_", ""), 0)
    except ValueError as exc:
        raise ParseError(f"expected an integer arity, got {token!r}") from exc


# --------------------------------------------------------------------- model
@dataclass
class FunctionEntry:
    name: str
    min_args: int
    max_args: int | None
    params: list[str] = field(default_factory=list)
    dynamic_keywords: bool = False


@dataclass
class SpecModule:
    const_name: str
    name: str
    line: int
    functions_expr: str
    constants_expr: str
    functions: list[FunctionEntry] = field(default_factory=list)
    constants: list[str] = field(default_factory=list)

    @property
    def declared_names(self) -> list[str]:
        names = [entry.name for entry in self.functions] + list(self.constants)
        return sorted(set(names))


def parse_call_arguments(item: str) -> tuple[str, list[str]]:
    match = CALL_RE.match(item)
    if not match:
        raise ParseError(f"unsupported spec entry: {item[:80]!r}")
    open_index = item.find("(", match.end() - 1)
    close_index = find_matching(item, open_index)
    return match.group(1), split_top_level(item[open_index + 1 : close_index])


def parse_function_entry(item: str) -> FunctionEntry:
    item = item.strip()
    if STRUCT_RE.match(item):
        open_index = item.find("{")
        close_index = find_matching(item, open_index)
        fields: dict[str, str] = {}
        for part in split_top_level(item[open_index + 1 : close_index]):
            if not part.strip():
                continue
            pair = split_field(part)
            if pair is not None:
                fields[pair[0]] = pair[1]
        return FunctionEntry(
            name=unquote(fields["name"]),
            min_args=parse_usize(fields["min_args"]) or 0,
            max_args=parse_usize(fields["max_args"]),
            params=string_literals(fields.get("params", "&[]")),
            dynamic_keywords=fields.get("dynamic_keywords", "false").strip() == "true",
        )

    kind, args = parse_call_arguments(item)
    if kind not in ("f", "kw", "kw_dynamic"):
        raise ParseError(f"expected a function entry, got {item[:80]!r}")
    if len(args) < 3:
        raise ParseError(f"function entry has too few arguments: {item[:80]!r}")
    params = string_literals(args[4]) if len(args) >= 5 else []
    return FunctionEntry(
        name=unquote(args[0]),
        min_args=parse_usize(args[1]) or 0,
        max_args=parse_usize(args[2]),
        params=params,
        dynamic_keywords=kind == "kw_dynamic",
    )


def parse_constant_entry(item: str) -> str:
    item = item.strip()
    if STRUCT_RE.match(item):
        open_index = item.find("{")
        close_index = find_matching(item, open_index)
        for part in split_top_level(item[open_index + 1 : close_index]):
            pair = split_field(part)
            if pair is not None and pair[0] == "name":
                return unquote(pair[1])
        raise ParseError(f"ConstantSpec without a name: {item[:80]!r}")

    kind, args = parse_call_arguments(item)
    if kind != "c":
        raise ParseError(f"expected a constant entry, got {item[:80]!r}")
    if not args:
        raise ParseError(f"constant entry has no name: {item[:80]!r}")
    return unquote(args[0])


def parse_array_entries(content: str, kind: str) -> list[Any]:
    entries: list[Any] = []
    for item in split_top_level(content):
        item = item.strip()
        if not item:
            continue
        if kind == "FunctionSpec":
            entries.append(parse_function_entry(item))
        else:
            entries.append(parse_constant_entry(item))
    return entries


def line_of(source: str, offset: int) -> int:
    return source.count("\n", 0, offset) + 1


def parse_specs(path: Path) -> list[SpecModule]:
    raw = path.read_text(encoding="utf-8")
    source = strip_comments(raw)

    const_slices: dict[str, tuple[str, list[Any]]] = {}
    for match in CONST_RE.finditer(source):
        name = match.group(1)
        kind = match.group(2)
        open_index = match.end() - 1
        close_index = find_matching(source, open_index)
        const_slices[name] = (
            kind,
            parse_array_entries(source[open_index + 1 : close_index], kind),
        )

    modules: dict[str, SpecModule] = {}
    order: list[SpecModule] = []
    for match in MODULE_RE.finditer(source):
        const_name = match.group(1)
        open_index = match.end() - 1
        close_index = find_matching(source, open_index)
        body = source[open_index + 1 : close_index]
        fields: dict[str, str] = {}
        for part in split_top_level(body):
            pair = split_field(part)
            if pair is not None:
                fields[pair[0]] = pair[1]
        if "name" not in fields:
            raise ParseError(f"ModuleSpec {const_name} has no name field")
        module = SpecModule(
            const_name=const_name,
            name=unquote(fields["name"]),
            line=line_of(source, match.start()),
            functions_expr=fields.get("functions", "&[]"),
            constants_expr=fields.get("constants", "&[]"),
        )
        modules[const_name] = module
        order.append(module)

    function_cache: dict[str, list[FunctionEntry]] = {}
    constant_cache: dict[str, list[str]] = {}

    def resolve_functions(expr: str, stack: tuple[str, ...] = ()) -> list[FunctionEntry]:
        expr = expr.strip()
        if not expr:
            return []
        if expr in function_cache:
            return function_cache[expr]
        if expr in stack:
            raise ParseError(f"cyclic function table reference: {' -> '.join(stack + (expr,))}")
        if expr.startswith("&["):
            open_index = expr.index("[")
            close_index = find_matching(expr, open_index)
            entries = parse_array_entries(expr[open_index + 1 : close_index], "FunctionSpec")
        else:
            match = re.fullmatch(r"([A-Za-z_]\w*)\.functions", expr)
            if match:
                target = modules.get(match.group(1))
                if target is None:
                    raise ParseError(f"unknown ModuleSpec in functions expression: {expr!r}")
                entries = resolve_functions(target.functions_expr, stack + (expr,))
            elif expr in const_slices:
                kind, raw_entries = const_slices[expr]
                if kind != "FunctionSpec":
                    raise ParseError(f"{expr} is not a FunctionSpec table")
                entries = raw_entries
            else:
                raise ParseError(f"unsupported functions expression: {expr!r}")
        function_cache[expr] = entries
        return entries

    def resolve_constants(expr: str, stack: tuple[str, ...] = ()) -> list[str]:
        expr = expr.strip()
        if not expr:
            return []
        if expr in constant_cache:
            return constant_cache[expr]
        if expr in stack:
            raise ParseError(f"cyclic constant table reference: {' -> '.join(stack + (expr,))}")
        if expr.startswith("&["):
            open_index = expr.index("[")
            close_index = find_matching(expr, open_index)
            entries = parse_array_entries(expr[open_index + 1 : close_index], "ConstantSpec")
        else:
            match = re.fullmatch(r"([A-Za-z_]\w*)\.constants", expr)
            if match:
                target = modules.get(match.group(1))
                if target is None:
                    raise ParseError(f"unknown ModuleSpec in constants expression: {expr!r}")
                entries = resolve_constants(target.constants_expr, stack + (expr,))
            elif expr in const_slices:
                kind, raw_entries = const_slices[expr]
                if kind != "ConstantSpec":
                    raise ParseError(f"{expr} is not a ConstantSpec table")
                entries = raw_entries
            else:
                raise ParseError(f"unsupported constants expression: {expr!r}")
        constant_cache[expr] = entries
        return entries

    for module in order:
        module.functions = resolve_functions(module.functions_expr)
        module.constants = resolve_constants(module.constants_expr)

    return order


# --------------------------------------------------------------------- oracle
PROBE = r'''
import contextlib
import importlib
import importlib.machinery
import inspect
import io
import json
import sys


def classify(module, spec):
    loader = getattr(spec, "loader", None)
    loader_name = type(loader).__name__ if loader is not None else ""
    origin = getattr(spec, "origin", None)
    if origin is None:
        origin = getattr(module, "__file__", None)
    package = getattr(spec, "submodule_search_locations", None) is not None

    if origin in ("built-in", "builtin"):
        kind = "builtin"
    elif origin == "frozen":
        kind = "frozen"
    elif loader_name == "ExtensionFileLoader" or (
        isinstance(origin, str)
        and any(origin.endswith(suffix) for suffix in importlib.machinery.EXTENSION_SUFFIXES)
    ):
        kind = "extension"
    elif loader_name == "SourceFileLoader" or (
        isinstance(origin, str) and origin.endswith(".py")
    ):
        kind = "pure"
    elif loader_name == "SourcelessFileLoader" or (
        isinstance(origin, str) and origin.endswith(".pyc")
    ):
        kind = "sourceless"
    elif loader_name == "NamespaceLoader":
        kind = "namespace"
    else:
        kind = "unknown"
    return {
        "kind": kind,
        "origin": origin,
        "loader": loader_name,
        "is_package": package,
    }


def signature_info(value):
    record = {}
    try:
        signature = inspect.signature(value)
    except BaseException as exc:
        record["signature_error"] = type(exc).__name__ + ": " + str(exc)[:200]
        text_signature = getattr(value, "__text_signature__", None)
        if isinstance(text_signature, str):
            record["text_signature"] = text_signature
        return record

    record["signature"] = str(signature)
    params = list(signature.parameters.values())
    positional = [
        param
        for param in params
        if param.kind
        in (inspect.Parameter.POSITIONAL_ONLY, inspect.Parameter.POSITIONAL_OR_KEYWORD)
    ]
    record["min_args"] = sum(
        1 for param in positional if param.default is inspect.Parameter.empty
    )
    record["max_args"] = (
        None
        if any(param.kind == inspect.Parameter.VAR_POSITIONAL for param in params)
        else len(positional)
    )
    record["varargs"] = any(
        param.kind == inspect.Parameter.VAR_POSITIONAL for param in params
    )
    record["varkw"] = any(
        param.kind == inspect.Parameter.VAR_KEYWORD for param in params
    )
    record["keyword_only"] = [
        param.name
        for param in params
        if param.kind == inspect.Parameter.KEYWORD_ONLY
    ]
    return record


def main():
    name = sys.argv[1]
    deep = len(sys.argv) > 2 and sys.argv[2] == "1"
    try:
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
            io.StringIO()
        ):
            module = importlib.import_module(name)
    except BaseException as exc:
        print(
            json.dumps(
                {
                    "ok": False,
                    "error": type(exc).__name__ + ": " + str(exc)[:400],
                },
                ensure_ascii=False,
            )
        )
        return

    spec = getattr(module, "__spec__", None)
    information = classify(module, spec)
    try:
        names = sorted(dir(module))
    except BaseException as exc:
        names = []
        information["dir_error"] = type(exc).__name__ + ": " + str(exc)[:200]
    public = [name for name in names if not name.startswith("_")]

    callables = {}
    if deep and information["kind"] in (
        "builtin",
        "frozen",
        "extension",
        "unknown",
    ):
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
            io.StringIO()
        ):
            for public_name in public:
                try:
                    value = getattr(module, public_name)
                except BaseException:
                    continue
                if not callable(value):
                    continue
                callables[public_name] = signature_info(value)

    all_value = getattr(module, "__all__", None)
    exported = (
        [str(item) for item in all_value]
        if isinstance(all_value, (list, tuple))
        else None
    )

    print(
        json.dumps(
            {
                "ok": True,
                "name": getattr(module, "__name__", name),
                **information,
                "public_names": public,
                "all_names": names,
                "dunder_all": exported,
                "callables": callables,
            },
            ensure_ascii=False,
        )
    )


main()
'''


def die(message: str, code: int = 1) -> None:
    print(message, file=sys.stderr)
    raise SystemExit(code)


def resolve_source(cli_value: str | None) -> Path:
    path = Path(cli_value).expanduser() if cli_value else DEFAULT_SOURCE
    if not path.is_dir():
        die(
            f"spec_audit: CPython source tree not found: {path}\n"
            f"run scripts/cpython_compat.py once to populate {DEFAULT_SOURCE}, "
            "or pass --source PATH",
            2,
        )
    if not (path / "Lib").is_dir():
        die(f"spec_audit: {path} has no Lib directory", 2)
    patchlevel = path / "Include" / "patchlevel.h"
    try:
        text = patchlevel.read_text(encoding="utf-8", errors="replace")
    except OSError as exc:
        die(f"spec_audit: cannot read {patchlevel}: {exc}", 2)
    match = VERSION_RE.search(text)
    if not match:
        die(f"spec_audit: no PY_VERSION in {patchlevel}", 2)
    version = match.group(1)
    if version != TARGET_PYTHON:
        die(
            f"spec_audit: source tree is CPython {version}, expected {TARGET_PYTHON}",
            2,
        )
    return path.resolve()


def resolve_python(cli_value: str | None) -> Path:
    candidate = cli_value or os.environ.get("HYDRO_CPYTHON") or sys.executable
    found = shutil.which(candidate) or candidate
    path = Path(found).expanduser()
    if not path.is_file():
        die(f"spec_audit: CPython oracle not found: {candidate}", 2)
    try:
        result = subprocess.run(
            [str(path), "-S", "-c", "import sys; print(sys.version.split()[0])"],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        die(f"spec_audit: cannot run {path}: {exc}", 2)
    version = result.stdout.strip()
    if result.returncode != 0 or version != TARGET_PYTHON:
        detail = version or result.stderr.strip()[:200] or f"exit {result.returncode}"
        die(
            f"spec_audit: oracle {path} reports {detail!r}, expected {TARGET_PYTHON}",
            2,
        )
    return path.resolve()


def probe_module(
    python: Path,
    source: Path,
    module: str,
    timeout: float,
    cwd: Path,
) -> dict[str, Any]:
    env = dict(os.environ)
    env["PYTHONPATH"] = str(source / "Lib")
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    env["PYTHONNOUSERSITE"] = "1"
    env["PYTHONIOENCODING"] = "utf-8"
    env.pop("PYTHONHOME", None)
    command = [str(python), "-S", "-c", PROBE, module, "1"]
    try:
        result = subprocess.run(
            command,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
            cwd=cwd,
            env=env,
        )
    except subprocess.TimeoutExpired:
        return {"ok": False, "status": "timeout", "error": f"timeout after {timeout:g}s"}
    except OSError as exc:
        return {"ok": False, "status": "probe_failed", "error": str(exc)}

    lines = [line for line in result.stdout.splitlines() if line.strip()]
    payload: dict[str, Any] | None = None
    if lines:
        try:
            payload = json.loads(lines[-1])
        except json.JSONDecodeError:
            payload = None
    if payload is None:
        stderr = result.stderr.strip().replace("\n", " ")
        return {
            "ok": False,
            "status": "probe_failed",
            "error": f"exit {result.returncode}: {stderr[:MAX_ERROR_CHARS]}",
        }
    if not payload.get("ok"):
        payload.setdefault("status", "import_failed")
    return payload


# --------------------------------------------------------------------- audit
def arity_text(minimum: Any, maximum: Any) -> str:
    if maximum is None:
        return f"{minimum}..*"
    return f"{minimum}..{maximum}"


def compare_module(spec: SpecModule, probe: dict[str, Any]) -> dict[str, Any]:
    record: dict[str, Any] = {
        "spec": spec.const_name,
        "name": spec.name,
        "line": spec.line,
        "functions": [entry.name for entry in spec.functions],
        "constants": list(spec.constants),
        "function_count": len(spec.functions),
        "constant_count": len(spec.constants),
        "oracle": probe,
    }
    if not probe.get("ok"):
        record.update(
            {
                "status": probe.get("status", "import_failed"),
                "native": False,
                "pure_python": False,
                "dynamic_fallback": False,
                "missing_public": [],
                "extra_public": [],
                "extra_private": [],
                "arity_mismatches": [],
            }
        )
        return record

    kind = probe.get("kind", "unknown")
    native = kind in NATIVE_KINDS
    pure = kind in PURE_KINDS
    declared = spec.declared_names
    declared_set = set(declared)
    oracle_names = set(probe.get("all_names", []))
    public_names = probe.get("public_names", [])
    callables = probe.get("callables", {})

    missing_public: list[str] = []
    extra_public: list[str] = []
    extra_private: list[str] = []
    arity_mismatches: list[dict[str, Any]] = []

    if native:
        missing_public = sorted(set(public_names) - declared_set)
        extra_public = sorted(
            name for name in declared if not name.startswith("_") and name not in oracle_names
        )
        extra_private = sorted(
            name for name in declared if name.startswith("_") and name not in oracle_names
        )
        for entry in spec.functions:
            if entry.name.startswith("_"):
                continue
            oracle_callable = callables.get(entry.name)
            if not oracle_callable or "signature" not in oracle_callable:
                continue
            oracle_min = oracle_callable.get("min_args")
            oracle_max = oracle_callable.get("max_args")
            if oracle_min is None:
                continue
            if entry.min_args != oracle_min or entry.max_args != oracle_max:
                arity_mismatches.append(
                    {
                        "name": entry.name,
                        "spec": {
                            "min": entry.min_args,
                            "max": entry.max_args,
                        },
                        "oracle": {
                            "min": oracle_min,
                            "max": oracle_max,
                        },
                        "signature": oracle_callable["signature"],
                    }
                )

    record.update(
        {
            "status": "ok",
            "kind": kind,
            "native": native,
            "pure_python": pure,
            "dynamic_fallback": pure,
            "origin": probe.get("origin"),
            "loader": probe.get("loader"),
            "is_package": probe.get("is_package", False),
            "public_name_count": len(public_names),
            "spec_name_count": len(declared),
            "missing_public": missing_public,
            "extra_public": extra_public,
            "extra_private": extra_private,
            "arity_mismatches": arity_mismatches,
        }
    )
    return record


def format_names(names: Iterable[str], limit: int = MARKDOWN_NAME_LIMIT) -> str:
    values = list(names)
    if not values:
        return "none"
    shown = ", ".join(f"`{name}`" for name in values[:limit])
    remaining = len(values) - limit
    if remaining > 0:
        shown += f" … (+{remaining} more)"
    return shown


def markdown_report(payload: dict[str, Any]) -> str:
    meta = payload["meta"]
    counts = meta["counts"]
    modules = payload["modules"]
    lines: list[str] = [
        "# ModuleSpec audit",
        "",
        "Generated by `scripts/spec_audit.py`; do not edit by hand.",
        "",
        f"- Spec file: `{meta['specs']}`",
        f"- CPython source: `{meta['source']}`",
        f"- Oracle: `{meta['python_executable']}` (CPython {meta['python_version']})",
        f"- Generated: {meta['generated_at']}",
        "",
        "## Summary",
        "",
        f"- Parsed module specs: **{counts['specs']}**",
        f"- Oracle imports succeeded: **{counts['importable']}** "
        f"({counts['unavailable']} unavailable)",
        f"- Native/runtime modules audited: **{counts['native']}**",
        f"- Pure-Python specs (dynamic fallback candidates): **{counts['pure_python']}**",
        f"- Modules with missing public names: **{counts['with_missing_public']}** "
        f"({counts['total_missing_public']} names)",
        f"- Modules with extra public names: **{counts['with_extra_public']}** "
        f"({counts['total_extra_public']} names)",
        f"- Arity mismatches: **{counts['with_arity_mismatch']}** "
        f"({counts['total_arity_mismatch']} callables)",
        "",
    ]

    if meta.get("module_filter"):
        lines.extend(
            [
                "Filtered to: "
                + ", ".join(f"`{name}`" for name in meta["module_filter"]),
                "",
            ]
        )

    detailed = [
        module
        for module in modules
        if module.get("native")
        and (
            module.get("missing_public")
            or module.get("extra_public")
            or module.get("arity_mismatches")
        )
    ]
    detailed.sort(key=lambda item: item["name"])

    lines.extend(["## Native module gaps", ""])
    if not detailed:
        lines.extend(["No native module gaps found.", ""])
    else:
        lines.extend(
            [
                "| module | kind | missing public | extra public | arity |",
                "|---|---|---:|---:|---:|",
            ]
        )
        for module in detailed:
            lines.append(
                "| `{name}` | {kind} | {missing} | {extra} | {arity} |".format(
                    name=module["name"],
                    kind=module.get("kind", "?"),
                    missing=len(module["missing_public"]),
                    extra=len(module["extra_public"]),
                    arity=len(module["arity_mismatches"]),
                )
            )
        lines.append("")

        for module in detailed[:MARKDOWN_MODULE_LIMIT]:
            lines.extend(
                [
                    f"### `{module['name']}` ({module.get('kind', '?')})",
                    "",
                    f"Missing public names: {format_names(module['missing_public'])}",
                    "",
                    f"Extra public names: {format_names(module['extra_public'])}",
                ]
            )
            if module.get("extra_private"):
                lines.append(
                    f"Extra private implementation names: "
                    f"{len(module['extra_private'])} (see JSON)"
                )
            if module.get("arity_mismatches"):
                lines.extend(
                    [
                        "",
                        "| callable | spec arity | oracle arity | signature |",
                        "|---|---|---|---|",
                    ]
                )
                for mismatch in module["arity_mismatches"][:MARKDOWN_NAME_LIMIT]:
                    signature = mismatch["signature"].replace("|", "\\|")
                    lines.append(
                        "| `{name}` | {spec} | {oracle} | `{signature}` |".format(
                            name=mismatch["name"],
                            spec=arity_text(
                                mismatch["spec"]["min"], mismatch["spec"]["max"]
                            ),
                            oracle=arity_text(
                                mismatch["oracle"]["min"], mismatch["oracle"]["max"]
                            ),
                            signature=signature,
                        )
                    )
                remaining = len(module["arity_mismatches"]) - MARKDOWN_NAME_LIMIT
                if remaining > 0:
                    lines.append(f"| … | | | (+{remaining} more) |")
            lines.append("")
        if len(detailed) > MARKDOWN_MODULE_LIMIT:
            lines.append(
                f"… and {len(detailed) - MARKDOWN_MODULE_LIMIT} more modules "
                "(see JSON)."
            )
            lines.append("")

    pure = sorted(
        (module for module in modules if module.get("pure_python")),
        key=lambda item: item["name"],
    )
    lines.extend(["## Pure-Python specs (dynamic fallback candidates)", ""])
    if not pure:
        lines.extend(["None.", ""])
    else:
        lines.extend(
            [
                "These specs shadow a CPython module whose implementation is "
                "pure Python. Tier 0 can run the original source from",
                "`HYDRO_STDLIB`/the vendored tree instead of maintaining a "
                "fixed native table.",
                "",
                "| module | spec | origin |",
                "|---|---|---|",
            ]
        )
        for module in pure:
            origin = module.get("origin") or "?"
            lines.append(
                f"| `{module['name']}` | `{module['spec']}` | `{origin}` |"
            )
        lines.append("")

    unavailable = sorted(
        (module for module in modules if module.get("status") != "ok"),
        key=lambda item: item["name"],
    )
    lines.extend(["## Modules unavailable in the oracle", ""])
    if not unavailable:
        lines.extend(["None.", ""])
    else:
        lines.extend(
            [
                "| module | status | error |",
                "|---|---|---|",
            ]
        )
        for module in unavailable:
            error = str(module.get("oracle", {}).get("error", "")).replace("|", "\\|")
            lines.append(
                f"| `{module['name']}` | {module['status']} | {error[:180]} |"
            )
        lines.append("")

    return "\n".join(lines).rstrip() + "\n"


def build_payload(
    specs_path: Path,
    source: Path,
    python: Path,
    specs: list[SpecModule],
    results: list[dict[str, Any]],
    patterns: list[str],
) -> dict[str, Any]:
    native = [item for item in results if item.get("native")]
    pure = [item for item in results if item.get("pure_python")]
    unavailable = [item for item in results if item.get("status") != "ok"]
    missing = [item for item in native if item.get("missing_public")]
    extra = [item for item in native if item.get("extra_public")]
    arity = [item for item in native if item.get("arity_mismatches")]
    counts = {
        "specs": len(specs),
        "modules": len(results),
        "importable": len(results) - len(unavailable),
        "unavailable": len(unavailable),
        "native": len(native),
        "pure_python": len(pure),
        "with_missing_public": len(missing),
        "total_missing_public": sum(len(item["missing_public"]) for item in missing),
        "with_extra_public": len(extra),
        "total_extra_public": sum(len(item["extra_public"]) for item in extra),
        "with_arity_mismatch": len(arity),
        "total_arity_mismatch": sum(len(item["arity_mismatches"]) for item in arity),
    }
    return {
        "meta": {
            "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "python_target": TARGET_PYTHON,
            "python_executable": str(python),
            "python_version": TARGET_PYTHON,
            "source": str(source),
            "specs": str(specs_path),
            "module_filter": patterns,
            "counts": counts,
        },
        "modules": results,
    }


def write_output(path: str, text: str) -> None:
    if path == "-":
        sys.stdout.write(text)
        if not text.endswith("\n"):
            sys.stdout.write("\n")
        return
    target = Path(path).expanduser()
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text, encoding="utf-8")


def select_modules(specs: list[SpecModule], patterns: list[str]) -> list[SpecModule]:
    if not patterns:
        return specs
    selected = [
        spec
        for spec in specs
        if any(
            fnmatch.fnmatchcase(spec.name, pattern) or spec.const_name == pattern
            for pattern in patterns
        )
    ]
    if not selected:
        raise SystemExit(
            "spec_audit: --module matched no ModuleSpec: " + ", ".join(patterns)
        )
    return selected


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Audit Hydro ModuleSpec tables in specs.rs against the pinned "
            "CPython 3.14.9 oracle."
        ),
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument(
        "--module",
        action="append",
        default=[],
        metavar="NAME[,NAME...]",
        help=(
            "audit only the named module(s); accepts exact names, spec constant "
            "names, and shell-style globs. Repeatable and comma-separated."
        ),
    )
    parser.add_argument(
        "--json",
        default=str(DEFAULT_JSON),
        metavar="PATH",
        help="write the machine-readable report (use - for stdout)",
    )
    parser.add_argument(
        "--md",
        default=str(DEFAULT_MD),
        metavar="PATH",
        help="write the Markdown summary (use - for stdout)",
    )
    parser.add_argument(
        "--specs",
        default=str(DEFAULT_SPECS),
        metavar="PATH",
        help="specs.rs to parse",
    )
    parser.add_argument(
        "--source",
        default=None,
        metavar="PATH",
        help=f"CPython {TARGET_PYTHON} source tree (default: cached checkout)",
    )
    parser.add_argument(
        "--python",
        "--cpython",
        dest="python",
        default=None,
        metavar="PATH",
        help=(
            "CPython 3.14.9 oracle (default: $HYDRO_CPYTHON, then "
            "the interpreter running this script)"
        ),
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=DEFAULT_TIMEOUT,
        metavar="SECONDS",
        help="per-module import/signature timeout",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    patterns: list[str] = []
    for value in args.module:
        patterns.extend(part.strip() for part in value.split(",") if part.strip())

    if args.json == "-" and args.md == "-":
        raise SystemExit("spec_audit: --json and --md cannot both write to stdout")
    if args.timeout <= 0:
        raise SystemExit("spec_audit: --timeout must be positive")

    specs_path = Path(args.specs).expanduser()
    if not specs_path.is_file():
        raise SystemExit(f"spec_audit: specs file not found: {specs_path}")
    try:
        specs = parse_specs(specs_path)
    except (OSError, ParseError) as exc:
        print(f"spec_audit: cannot parse {specs_path}: {exc}", file=sys.stderr)
        return 1

    selected = select_modules(specs, patterns)
    source = resolve_source(args.source)
    python = resolve_python(args.python)

    log = lambda message: print(message, file=sys.stderr, flush=True)
    log(
        f"spec_audit: {len(selected)} ModuleSpec(s), oracle CPython {TARGET_PYTHON}, "
        f"source {source}"
    )

    results: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="hydro-spec-audit-") as work:
        cwd = Path(work)
        for index, spec in enumerate(selected, 1):
            probe = probe_module(python, source, spec.name, args.timeout, cwd)
            results.append(compare_module(spec, probe))
            log(f"  [{index}/{len(selected)}] {spec.name}")

    results.sort(key=lambda item: item["name"])
    payload = build_payload(specs_path, source, python, selected, results, patterns)
    write_output(args.json, json.dumps(payload, indent=2, ensure_ascii=False) + "\n")
    write_output(args.md, markdown_report(payload))

    counts = payload["meta"]["counts"]
    log(
        "spec_audit: "
        f"{counts['native']} native, {counts['pure_python']} pure-Python, "
        f"{counts['unavailable']} unavailable; "
        f"{counts['total_missing_public']} missing public names, "
        f"{counts['total_extra_public']} extra public names, "
        f"{counts['total_arity_mismatch']} arity mismatches"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
