#!/usr/bin/env python3
"""Inventory the CPython C-API symbols required by the pinned wheels.

Every shared object under the ecosystem ``site-packages`` directory is scanned
with ``nm`` (falling back to ``readelf`` when ``nm`` is unavailable or rejects a
file).  Strong and weak undefined symbols that look like CPython API symbols are
collected together with the library and module that reference them.  The
symbols exported by Hydro's runtime archive(s) and by the Tea binary are then
extracted with the same tools and subtracted from the required set.

The result is a deterministic JSON report and a Markdown summary.  ``--check``
exits non-zero when at least one strongly undefined CPython symbol is not
exported by any selected Hydro source.  Weak undefined symbols are reported,
but do not fail the check.  This is a link-time symbol inventory: it says
nothing about whether a symbol behaves correctly once called.

    python3 scripts/abi_inventory.py
    python3 scripts/abi_inventory.py --check
    python3 scripts/abi_inventory.py --site /tmp/pkgs --hydro-lib target/debug/libhydro_runtime.a
    python3 scripts/abi_inventory.py --hydro-lib ../tea/target/debug/tea --json /tmp/abi.json

The site-packages directory is ``--site`` or ``$HYDRO_ECOSYSTEM_SITE_PACKAGES``
(default ``/tmp/pkgs``).  Without ``--hydro-lib`` the script inspects
``target/debug/libhydro_runtime.a``, ``target/release/libhydro_runtime.a`` and
the Tea binary resolved from ``$HYDRO_BIN``, ``../tea/target/release/tea`` or
``../tea/target/debug/tea``.  ``--hydro-lib`` may be repeated and accepts a
file or a directory containing ``libhydro_runtime.a``/``.so`` and ``tea``.

Only the Python standard library and binutils (``nm``/``readelf``) are used.
No wheel is imported and no build is run.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent  # repository root (parent of scripts/)

DEFAULT_SITE = Path(os.environ.get("HYDRO_ECOSYSTEM_SITE_PACKAGES") or "/tmp/pkgs")
DEFAULT_JSON = ROOT / "benchmarks" / "abi" / "latest.json"
DEFAULT_MD = ROOT / "benchmarks" / "abi" / "latest.md"

DEFAULT_ARCHIVES = (
    ROOT / "target" / "debug" / "libhydro_runtime.a",
    ROOT / "target" / "release" / "libhydro_runtime.a",
)
DEFAULT_TEA_CANDIDATES = (
    ROOT.parent / "tea" / "target" / "release" / "tea",
    ROOT.parent / "tea" / "target" / "debug" / "tea",
)

BINUTIL_TIMEOUT = 300.0

# CPython public/private symbols start with "Py" + an uppercase letter or with
# "Py_" (and the same for "_Py...").  This excludes unrelated "Pyth..." names.
CPYTHON_SYMBOL_RE = re.compile(r"^_?Py(?:[A-Z]|_)[A-Za-z0-9_]*$")
NM_VERSION_SUFFIX_RE = re.compile(r"@.*$")
READELF_VERSION_SUFFIX_RE = re.compile(r"@\S+$")
READELF_VERSION_TRAILER_RE = re.compile(r"\s+\(\d+\)$")

# Wheels install extension modules under the package (or a sibling "*.libs"
# directory); report distribution-level names instead of filesystem details.
LIBRARY_ALIASES = {
    "pydantic_core": "pydantic",
}


class InventoryError(RuntimeError):
    """A configuration or inspection failure that prevents a trustworthy report."""


class BinutilError(RuntimeError):
    """A single nm/readelf invocation failed."""


# --------------------------------------------------------------------- helpers
def log(message: str) -> None:
    print(message, file=sys.stderr, flush=True)


def display_path(path: Path) -> str:
    """Render a path relative to the repository, without depending on CWD."""
    try:
        return path.resolve().relative_to(ROOT).as_posix()
    except ValueError:
        try:
            return Path(os.path.relpath(path.resolve(), ROOT)).as_posix()
        except ValueError:  # pragma: no cover - different drives on Windows
            return str(path)


def library_for(relative: Path) -> str:
    top = relative.parts[0]
    if top in LIBRARY_ALIASES:
        return LIBRARY_ALIASES[top]
    if top.endswith(".libs"):
        return top[: -len(".libs")]
    return top


def symbol_family(symbol: str) -> str:
    """Return the API family used to group a CPython symbol.

    ``PyUnicode_DecodeUTF8`` -> ``PyUnicode``; ``PyExc_ValueError`` ->
    ``PyExc``; ``Py_BuildValue`` and every ``Py_*``/``_Py_*`` core entry point
    -> ``Py_<core>``.
    """
    private = symbol.startswith("_")
    body = symbol[1:] if private else symbol
    tail = body[2:]  # strip the "Py" prefix; the regex guarantees it exists
    if tail.startswith("_"):
        return "Py_<core>"
    match = re.match(r"[A-Z][A-Za-z0-9]*", tail)
    return f"Py{match.group(0)}" if match else "Py"


def is_cpython_symbol(symbol: str) -> bool:
    return CPYTHON_SYMBOL_RE.match(symbol) is not None


def coverage(referenced: int, missing: int) -> str:
    if referenced <= 0:
        return "n/a"
    return f"{100.0 * (referenced - missing) / referenced:.1f}%"


def file_kind(path: Path) -> str:
    with path.open("rb") as stream:
        magic = stream.read(8)
    if magic.startswith(b"!<arch>\n"):
        return "static-archive"
    if magic.startswith(b"\x7fELF"):
        return "elf"
    raise InventoryError(f"not an ELF file or ar archive: {path}")


# ------------------------------------------------------------------- binutils
class Binutils:
    def __init__(self) -> None:
        self.nm = shutil.which("nm")
        self.readelf = shutil.which("readelf")
        if not self.nm and not self.readelf:
            raise InventoryError("binutils not found: need 'nm' or 'readelf' on PATH")

    def _run(self, argv: list[str]) -> str:
        try:
            completed = subprocess.run(
                argv,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=BINUTIL_TIMEOUT,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise BinutilError(f"{argv[0]} failed: {exc}") from exc
        if completed.returncode != 0:
            detail = (completed.stderr or completed.stdout).strip().splitlines()
            suffix = f": {detail[0]}" if detail else ""
            raise BinutilError(f"{' '.join(argv)} exited {completed.returncode}{suffix}")
        return completed.stdout

    # -- symbol table parsing --------------------------------------------
    def _defined_nm(self, path: Path, *, dynamic: bool) -> set[str]:
        assert self.nm is not None
        argv = [
            self.nm,
            "--format=posix",
            "-g",
            "--defined-only",
            *(["-D"] if dynamic else []),
            str(path),
        ]
        output = self._run(argv)
        return {
            name
            for name, kind in self._parse_nm_posix(output)
            if kind not in ("U", "w", "v", "?", "-")
        }

    @staticmethod
    def _parse_nm_posix(text: str) -> list[tuple[str, str]]:
        entries: list[tuple[str, str]] = []
        for raw in text.splitlines():
            line = raw.strip()
            if not line or line.endswith(":"):
                continue  # blank separator or "archive-member.o:" header
            fields = line.split()
            if len(fields) < 2 or len(fields[1]) != 1:
                continue
            name = NM_VERSION_SUFFIX_RE.sub("", fields[0])
            if name:
                entries.append((name, fields[1]))
        return entries

    @staticmethod
    def _parse_readelf(text: str) -> tuple[set[str], set[str], set[str]]:
        """Return (strong undefined, weak undefined, defined) names."""
        strong: set[str] = set()
        weak: set[str] = set()
        defined: set[str] = set()
        for raw in text.splitlines():
            fields = raw.split(None, 7)
            if len(fields) < 8:
                continue
            number, _value, _size, _type, bind, _visibility, section, name = fields
            if not number.rstrip(":").isdigit():
                continue
            name = READELF_VERSION_TRAILER_RE.sub("", name)
            name = READELF_VERSION_SUFFIX_RE.sub("", name)
            if not name:
                continue
            if section == "UND":
                if bind == "GLOBAL":
                    strong.add(name)
                elif bind == "WEAK":
                    weak.add(name)
            elif bind in ("GLOBAL", "WEAK"):
                defined.add(name)
        return strong, weak, defined

    # -- queries ----------------------------------------------------------
    def undefined(self, path: Path) -> tuple[set[str], set[str]]:
        """Return (strong, weak) undefined CPython symbols of a shared object."""
        if self.nm:
            try:
                output = self._run(
                    [
                        self.nm,
                        "--format=posix",
                        "-D",
                        "--undefined-only",
                        str(path),
                    ]
                )
                strong: set[str] = set()
                weak: set[str] = set()
                for name, kind in self._parse_nm_posix(output):
                    if not is_cpython_symbol(name):
                        continue
                    if kind == "U":
                        strong.add(name)
                    elif kind in ("w", "v"):
                        weak.add(name)
                return strong, weak
            except BinutilError:
                if not self.readelf:
                    raise
        assert self.readelf is not None
        output = self._run([self.readelf, "--dyn-syms", "--wide", str(path)])
        strong, weak, _ = self._parse_readelf(output)
        return (
            {name for name in strong if is_cpython_symbol(name)},
            {name for name in weak if is_cpython_symbol(name)},
        )

    def _elf_subtype(self, path: Path) -> str:
        if not self.readelf:
            return "elf"
        try:
            output = self._run([self.readelf, "--file-header", "--wide", str(path)])
        except BinutilError:
            return "elf"
        match = re.search(r"^\s*Type:\s+(EXEC|DYN|REL|CORE)\b", output, re.MULTILINE)
        if not match:
            return "elf"
        return {
            "EXEC": "elf-executable",
            "DYN": "elf-dynamic",
            "REL": "elf-object",
            "CORE": "elf-core",
        }[match.group(1)]

    def _defined_readelf(self, path: Path, *, dynamic: bool) -> set[str]:
        assert self.readelf is not None
        flag = "--dyn-syms" if dynamic else "--symbols"
        output = self._run([self.readelf, flag, "--wide", str(path)])
        _, _, defined = self._parse_readelf(output)
        return defined

    def defined(self, path: Path) -> tuple[set[str], str, str]:
        """Return (defined CPython symbols, kind, symbol scope).

        ELF objects expose dynamic symbols to a dynamically loaded extension;
        that table is authoritative when it is populated.  Archives and
        relocatable objects have no dynamic table, and a stripped dynamic
        executable falls back to the full symbol table (scope ``all``).
        """
        kind = file_kind(path)
        if kind == "static-archive":
            symbols = (
                self._defined_readelf(path, dynamic=False)
                if not self.nm
                else self._defined_nm(path, dynamic=False)
            )
            return {name for name in symbols if is_cpython_symbol(name)}, kind, "all"

        subtype = self._elf_subtype(path)
        if subtype == "elf-object":
            symbols = (
                self._defined_readelf(path, dynamic=False)
                if not self.nm
                else self._defined_nm(path, dynamic=False)
            )
            return {name for name in symbols if is_cpython_symbol(name)}, subtype, "all"

        if self.nm:
            dynamic_symbols: set[str] = set()
            try:
                dynamic_symbols = self._defined_nm(path, dynamic=True)
            except BinutilError:
                dynamic_symbols = set()
            if dynamic_symbols:
                return (
                    {name for name in dynamic_symbols if is_cpython_symbol(name)},
                    subtype,
                    "dynamic",
                )
            all_symbols = self._defined_nm(path, dynamic=False)
            return (
                {name for name in all_symbols if is_cpython_symbol(name)},
                subtype,
                "all",
            )

        dynamic_symbols = self._defined_readelf(path, dynamic=True)
        if dynamic_symbols:
            return (
                {name for name in dynamic_symbols if is_cpython_symbol(name)},
                subtype,
                "dynamic",
            )
        all_symbols = self._defined_readelf(path, dynamic=False)
        return (
            {name for name in all_symbols if is_cpython_symbol(name)},
            subtype,
            "all",
        )


# ------------------------------------------------------------------- discovery
def scan_site(
    site: Path, tools: Binutils
) -> tuple[list[dict[str, Any]], dict[str, dict[str, Any]], set[str], set[str]]:
    """Scan every ELF .so* below *site*.

    Returns module metadata, per-symbol provenance, and the global strong/weak
    undefined symbol sets.
    """
    files: list[Path] = []
    seen: set[Path] = set()
    for candidate in sorted(site.rglob("*.so*")):
        if not candidate.is_file():
            continue
        resolved = candidate.resolve()
        if resolved in seen:
            continue
        with candidate.open("rb") as stream:
            if not stream.read(4).startswith(b"\x7fELF"):
                continue
        seen.add(resolved)
        files.append(candidate)

    modules: list[dict[str, Any]] = []
    references: dict[str, dict[str, Any]] = defaultdict(
        lambda: {"libraries": set(), "modules": set()}
    )
    strong_all: set[str] = set()
    weak_all: set[str] = set()

    for path in files:
        relative = path.relative_to(site)
        library = library_for(relative)
        strong, weak = tools.undefined(path)
        strong_all.update(strong)
        weak_all.update(weak)
        for symbol in strong | weak:
            entry = references[symbol]
            entry["libraries"].add(library)
            entry["modules"].add(relative.as_posix())
        modules.append(
            {
                "path": relative.as_posix(),
                "library": library,
                "strong_undefined": len(strong),
                "weak_undefined": len(weak),
            }
        )

    normalized = {
        symbol: {
            "libraries": sorted(entry["libraries"]),
            "modules": sorted(entry["modules"]),
        }
        for symbol, entry in references.items()
    }
    return modules, normalized, strong_all, weak_all


def resolve_tea() -> Path | None:
    env = os.environ.get("HYDRO_BIN")
    if env:
        candidate = Path(env).expanduser()
        if not (candidate.is_file() and os.access(candidate, os.X_OK)):
            raise InventoryError(f"$HYDRO_BIN is not an executable file: {candidate}")
        return candidate.resolve()
    for candidate in DEFAULT_TEA_CANDIDATES:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate.resolve()
    return None


def expand_hydro_paths(explicit: list[Path] | None) -> list[Path]:
    if explicit:
        paths: list[Path] = []
        for raw in explicit:
            candidate = raw.expanduser()
            if candidate.is_dir():
                found = [
                    child
                    for child in (
                        candidate / "libhydro_runtime.a",
                        candidate / "libhydro_runtime.so",
                        candidate / "tea",
                    )
                    if child.is_file()
                ]
                if not found:
                    raise InventoryError(
                        f"--hydro-lib directory has no libhydro_runtime.a/.so or tea: {candidate}"
                    )
                paths.extend(found)
            elif candidate.is_file():
                paths.append(candidate)
            else:
                raise InventoryError(f"--hydro-lib path not found: {candidate}")
        return paths

    paths = [candidate for candidate in DEFAULT_ARCHIVES if candidate.is_file()]
    tea = resolve_tea()
    if tea is not None:
        paths.append(tea)
    return paths


# -------------------------------------------------------------------- reporting
def build_report(
    site: Path,
    modules: list[dict[str, Any]],
    references: dict[str, dict[str, Any]],
    hydro_sources: list[dict[str, Any]],
    hydro_defined: set[str],
    strong_all: set[str],
    weak_all: set[str],
) -> dict[str, Any]:
    referenced = set(references)
    missing_required = strong_all - hydro_defined
    missing_weak = (weak_all - hydro_defined) - strong_all

    libraries = sorted({module["library"] for module in modules})
    modules_by_library: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for module in modules:
        modules_by_library[module["library"]].append(module)

    by_library: dict[str, Any] = {}
    for library in libraries:
        library_modules = modules_by_library[library]
        library_symbols = {
            symbol
            for symbol in referenced
            if library in references[symbol]["libraries"]
        }
        library_strong = {
            symbol for symbol in strong_all if library in references[symbol]["libraries"]
        }
        library_weak = {
            symbol for symbol in weak_all if library in references[symbol]["libraries"]
        }
        library_missing = library_strong - hydro_defined
        library_weak_missing = (library_weak - hydro_defined) - strong_all

        families: dict[str, Any] = {}
        for family in sorted(
            {symbol_family(symbol) for symbol in library_symbols}
        ):
            family_symbols = {
                symbol
                for symbol in library_symbols
                if symbol_family(symbol) == family
            }
            family_strong = {
                symbol
                for symbol in library_strong
                if symbol_family(symbol) == family
            }
            family_weak = {
                symbol for symbol in library_weak if symbol_family(symbol) == family
            }
            families[family] = {
                "referenced": len(family_symbols),
                "missing": len(family_strong - hydro_defined),
                "weak_missing": len((family_weak - hydro_defined) - strong_all),
            }

        by_library[library] = {
            "modules_scanned": len(library_modules),
            "modules_referencing": sum(
                1
                for module in library_modules
                if module["strong_undefined"] or module["weak_undefined"]
            ),
            "referenced_symbols": len(library_symbols),
            "strong_undefined_symbols": len(library_strong),
            "weak_undefined_symbols": len(library_weak),
            "missing_required_symbols": len(library_missing),
            "missing_weak_symbols": len(library_weak_missing),
            "missing_symbols": sorted(library_missing),
            "weak_missing_symbols": sorted(library_weak_missing),
            "families": families,
        }

    by_family: dict[str, Any] = {}
    for family in sorted({symbol_family(symbol) for symbol in referenced}):
        family_referenced = {
            symbol for symbol in referenced if symbol_family(symbol) == family
        }
        family_strong = {
            symbol for symbol in strong_all if symbol_family(symbol) == family
        }
        family_weak = {
            symbol for symbol in weak_all if symbol_family(symbol) == family
        }
        family_missing = family_strong - hydro_defined
        family_weak_missing = (family_weak - hydro_defined) - family_strong
        by_family[family] = {
            "referenced": len(family_referenced),
            "missing": len(family_missing),
            "weak_missing": len(family_weak_missing),
            "missing_symbols": sorted(family_missing),
            "weak_missing_symbols": sorted(family_weak_missing),
        }

    def missing_entries(symbols: set[str]) -> list[dict[str, Any]]:
        entries = []
        for symbol in sorted(symbols):
            entry = references[symbol]
            entries.append(
                {
                    "symbol": symbol,
                    "family": symbol_family(symbol),
                    "private": symbol.startswith("_"),
                    "libraries": entry["libraries"],
                    "modules": entry["modules"],
                }
            )
        return entries

    return {
        "schema": 1,
        "tool": "scripts/abi_inventory.py",
        "site_packages": str(site),
        "hydro_sources": hydro_sources,
        "summary": {
            "libraries": len(libraries),
            "modules_scanned": len(modules),
            "modules_referencing": sum(
                1
                for module in modules
                if module["strong_undefined"] or module["weak_undefined"]
            ),
            "referenced_symbols": len(referenced),
            "strong_undefined_symbols": len(strong_all),
            "weak_undefined_symbols": len(weak_all),
            "hydro_defined_symbols": len(hydro_defined),
            "missing_required_symbols": len(missing_required),
            "missing_weak_symbols": len(missing_weak),
            "check": "fail" if missing_required else "pass",
        },
        "by_library": by_library,
        "by_family": by_family,
        "missing": {
            "required": missing_entries(missing_required),
            "weak": missing_entries(missing_weak),
        },
    }


def render_markdown(report: dict[str, Any]) -> str:
    summary = report["summary"]
    lines: list[str] = [
        "# Hydro CPython C-API ABI gap report",
        "",
        "> Generated by `scripts/abi_inventory.py` from the pinned wheel extension",
        "> modules and the symbols exported by Hydro's runtime. Do not edit by hand.",
        "",
        "This is a link-time inventory of CPython C-API symbols referenced by the",
        "extension modules in the ecosystem site-packages directory. It records",
        "which symbols Hydro exports and which required symbols are missing; it",
        "does not claim that a symbol behaves correctly once called.",
        "",
        "## Inputs",
        "",
        "| Input | Value |",
        "| --- | --- |",
        f"| Site-packages | `{report['site_packages']}` |",
    ]
    for source in report["hydro_sources"]:
        lines.append(
            f"| Hydro source | `{source['path']}` "
            f"({source['kind']}, {source['symbol_scope']}, "
            f"{source['defined_cpython_symbols']} symbols) |"
        )
    if not report["hydro_sources"]:
        lines.append("| Hydro source | none found |")
    lines.extend(
        [
            "",
            "## Summary",
            "",
            "| Metric | Value |",
            "| --- | --- |",
            f"| Extension modules scanned | {summary['modules_scanned']} |",
            f"| Extension modules referencing CPython symbols | "
            f"{summary['modules_referencing']} |",
            f"| Libraries | {summary['libraries']} |",
            f"| Referenced CPython symbols | {summary['referenced_symbols']} |",
            f"| Strong undefined | {summary['strong_undefined_symbols']} |",
            f"| Weak undefined | {summary['weak_undefined_symbols']} |",
            f"| Hydro-defined CPython symbols | {summary['hydro_defined_symbols']} |",
            f"| Missing required | {summary['missing_required_symbols']} |",
            f"| Missing weak | {summary['missing_weak_symbols']} |",
            f"| Check | {summary['check']} |",
            "",
            "## By library",
            "",
            "| Library | Modules (referencing/scanned) | Referenced | Missing | Coverage |",
            "| --- | ---: | ---: | ---: | ---: |",
        ]
    )
    for library, data in report["by_library"].items():
        lines.append(
            f"| {library} | {data['modules_referencing']}/{data['modules_scanned']} "
            f"| {data['referenced_symbols']} | {data['missing_required_symbols']} "
            f"| {coverage(data['strong_undefined_symbols'], data['missing_required_symbols'])} |"
        )

    lines.extend(
        [
            "",
            "Per-library counts overlap when a symbol is referenced by modules from",
            "more than one library; the summary counts distinct symbols.",
            "",
            "## By API family",
            "",
            "| Family | Referenced | Missing | Weak missing | Missing symbols |",
            "| --- | ---: | ---: | ---: | --- |",
        ]
    )
    for family, data in report["by_family"].items():
        missing = ", ".join(f"`{symbol}`" for symbol in data["missing_symbols"])
        lines.append(
            f"| {family} | {data['referenced']} | {data['missing']} "
            f"| {data['weak_missing']} | {missing or '—'} |"
        )

    lines.extend(["", "## Missing required symbols by library", ""])
    required = report["missing"]["required"]
    by_library: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for entry in required:
        for library in entry["libraries"]:
            by_library[library].append(entry)
    if not required:
        lines.append("No required CPython symbols are missing.")
        lines.append("")
    else:
        lines.append(
            "A symbol referenced by more than one library appears under each of"
        )
        lines.append("those libraries; counts therefore overlap.")
        lines.append("")
        for library in sorted(by_library):
            entries = by_library[library]
            lines.extend(
                [
                    f"### {library} ({len(entries)})",
                    "",
                    "| Symbol | Family | Private | Modules |",
                    "| --- | --- | --- | ---: |",
                ]
            )
            for entry in entries:
                lines.append(
                    f"| `{entry['symbol']}` | {entry['family']} "
                    f"| {'yes' if entry['private'] else 'no'} "
                    f"| {len(entry['modules'])} |"
                )
            lines.append("")

    weak = report["missing"]["weak"]
    if weak:
        lines.extend(["## Missing weak symbols", ""])
        for entry in weak:
            lines.append(
                f"- `{entry['symbol']}` ({entry['family']}): "
                + ", ".join(entry["libraries"])
            )
        lines.append("")

    lines.extend(
        [
            "## Check policy",
            "",
            "`--check` exits 1 when at least one strongly undefined CPython symbol",
            "is not exported by any selected Hydro source. Weak undefined symbols",
            "are reported separately and do not fail the check. Private (`_Py…`)",
            "symbols are included because the pinned wheels reference them, even",
            "though they are not part of CPython's public API.",
            "",
        ]
    )
    return "\n".join(lines)


# ------------------------------------------------------------------------ main
def write_text(path: Path, text: str) -> None:
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    except OSError as exc:
        raise InventoryError(f"cannot write {path}: {exc}") from exc


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Inventory CPython C-API symbols required by pinned wheel extensions."
    )
    parser.add_argument(
        "--site",
        type=Path,
        default=DEFAULT_SITE,
        metavar="DIR",
        help="site-packages directory (default: $HYDRO_ECOSYSTEM_SITE_PACKAGES or /tmp/pkgs)",
    )
    parser.add_argument(
        "--hydro-lib",
        type=Path,
        action="append",
        metavar="PATH",
        help="Hydro archive/executable or directory to inspect; repeatable. "
        "Overrides the default target/debug, target/release and Tea sources.",
    )
    parser.add_argument(
        "--json",
        type=Path,
        default=DEFAULT_JSON,
        dest="json_path",
        metavar="PATH",
        help=f"JSON report path (default: {display_path(DEFAULT_JSON)})",
    )
    parser.add_argument(
        "--md",
        type=Path,
        default=DEFAULT_MD,
        dest="md_path",
        metavar="PATH",
        help=f"Markdown report path (default: {display_path(DEFAULT_MD)})",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="exit 1 when required CPython symbols are missing",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        site = args.site.expanduser()
        if not site.is_dir():
            raise InventoryError(f"--site directory not found: {site}")
        site = site.resolve()

        tools = Binutils()
        modules, references, strong_all, weak_all = scan_site(site, tools)

        hydro_paths = expand_hydro_paths(args.hydro_lib)
        hydro_defined: set[str] = set()
        hydro_sources: list[dict[str, Any]] = []
        seen_hydro: set[Path] = set()
        for path in hydro_paths:
            resolved = path.resolve()
            if resolved in seen_hydro:
                continue
            seen_hydro.add(resolved)
            symbols, kind, scope = tools.defined(resolved)
            hydro_defined.update(symbols)
            hydro_sources.append(
                {
                    "path": display_path(resolved),
                    "kind": kind,
                    "symbol_scope": scope,
                    "defined_cpython_symbols": len(symbols),
                }
            )
        hydro_sources.sort(key=lambda source: source["path"])
        if not hydro_sources:
            log(
                "warning: no Hydro archive or Tea binary found; "
                "every referenced CPython symbol is reported missing"
            )

        report = build_report(
            site,
            modules,
            references,
            hydro_sources,
            hydro_defined,
            strong_all,
            weak_all,
        )
        json_text = json.dumps(report, indent=2, sort_keys=True, ensure_ascii=False) + "\n"
        markdown_text = render_markdown(report)
        write_text(args.json_path.expanduser(), json_text)
        write_text(args.md_path.expanduser(), markdown_text)

        summary = report["summary"]
        log(
            f"{summary['referenced_symbols']} referenced, "
            f"{summary['missing_required_symbols']} missing required, "
            f"{summary['missing_weak_symbols']} missing weak, "
            f"{len(hydro_sources)} Hydro source(s)"
        )
        log(f"wrote {args.json_path} and {args.md_path}")
        if args.check and report["summary"]["check"] == "fail":
            log("check failed: required CPython symbols are missing")
            return 1
        return 0
    except InventoryError as exc:
        log(f"error: {exc}")
        return 2


if __name__ == "__main__":
    sys.exit(main())
