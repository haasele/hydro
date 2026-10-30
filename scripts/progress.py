#!/usr/bin/env python3
"""Render the README progress chart from the generated compatibility reports.

The chart is intentionally small: it shows the two release-facing measures
(CPython's own test suite and the stdlib import matrix) plus the stdlib layers
so it is obvious what is implemented and what is still missing.

    python3 scripts/progress.py            # write docs/progress.svg
    python3 scripts/progress.py --check    # fail when the SVG is stale
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys
from xml.sax.saxutils import escape

ROOT = Path(__file__).resolve().parent.parent
CPYTHON_REPORT = ROOT / "benchmarks/compat/cpython-3.14.9.json"
STDLIB_REPORT = ROOT / "benchmarks/compat/latest.json"
OUTPUT = ROOT / "docs/progress.svg"

GREEN = "#2da44e"
TRACK = "#d0d7de"
TEXT = "#6e7781"
STRONG = "#57606a"


def load(path: Path) -> dict:
    with path.open(encoding="utf-8") as handle:
        return json.load(handle)


def percentage(done: int, total: int) -> str:
    return f"{100 * done / total:.0f}%" if total else "n/a"


def bar(done: int, total: int, x: int, y: int, width: int, height: int) -> str:
    filled = 0 if not total else round(width * done / total)
    return (
        f'<rect x="{x}" y="{y}" width="{width}" height="{height}" rx="4" fill="{TRACK}"/>'
        f'<rect x="{x}" y="{y}" width="{filled}" height="{height}" rx="4" fill="{GREEN}"/>'
    )


def row(label: str, done: int, total: int, y: int, strong: bool = False) -> str:
    label_colour = STRONG if strong else TEXT
    font_weight = ' font-weight="600"' if strong else ""
    return (
        f'<text x="24" y="{y + 15}" fill="{label_colour}" font-size="14"{font_weight}>'
        f"{escape(label)}</text>"
        f"{bar(done, total, 250, y, 460, 14)}"
        f'<text x="790" y="{y + 15}" fill="{TEXT}" font-size="12">'
        f"{done} done · {total - done} left · {percentage(done, total)}</text>"
    )


def render(cpython: dict, stdlib: dict) -> str:
    suite = cpython["summary"]
    counts = stdlib["meta"]["counts"]
    layers = counts["by_layer"]
    order = ("builtin/frozen", "pure", "extension", "unknown")
    layer_rows = []
    for index, name in enumerate(order):
        layer = layers.get(name, {"ok": 0, "total": 0})
        layer_rows.append(
            row(f"    {name}", layer["ok"], layer["total"], 108 + index * 30)
        )
    svg_rows = [
        row("CPython 3.14.9 Lib/test", suite["passed"], suite["total"], 42, True),
        row("Stdlib modules importable", counts["ok"], counts["total"], 72, True),
        *layer_rows,
    ]
    date = stdlib["meta"]["date"][:10]
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="960" height="234" viewBox="0 0 960 234" font-family="Arial, Helvetica, sans-serif" role="img" aria-label="Hydro compatibility progress">
<title>Hydro compatibility progress</title>
<text x="24" y="22" fill="{STRONG}" font-size="16" font-weight="600">Hydro compatibility progress</text>
<text x="936" y="22" text-anchor="end" fill="{TEXT}" font-size="12">{date}</text>
{chr(10).join(svg_rows)}
</svg>
"""


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail if stale")
    args = parser.parse_args()
    rendered = render(load(CPYTHON_REPORT), load(STDLIB_REPORT))
    if args.check:
        current = OUTPUT.read_text(encoding="utf-8") if OUTPUT.exists() else ""
        if current != rendered:
            print(f"{OUTPUT.relative_to(ROOT)} is stale; run scripts/progress.py", file=sys.stderr)
            return 1
        return 0
    OUTPUT.write_text(rendered, encoding="utf-8")
    print(f"wrote {OUTPUT.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
