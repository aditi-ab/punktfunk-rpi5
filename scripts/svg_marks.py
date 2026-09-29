"""Monochrome SVG mark masters -> inline path tables, shared by gen_launcher_icon_tables.py and
gen_os_mark_table.py."""

from __future__ import annotations

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent


def mark(masters: pathlib.Path, token: str) -> tuple[str, str, float, float]:
    """(token, path data, viewport width, viewport height) for one master.

    A master is one `<path>` whose data fits on one line inside a double-quoted string, so every
    client can inline it verbatim. Anything else stops the generator.
    """
    svg = (masters / f"{token}.svg").read_text()
    box = re.search(r'viewBox="([^"]+)"', svg).group(1)
    paths = re.findall(r'<path[^>]*\sd="([^"]+)"', svg)
    if len(paths) != 1:
        sys.exit(f"{token}: expected exactly one <path>, found {len(paths)}")
    d = paths[0]
    if any(c in d for c in '\n\t"\\'):
        sys.exit(f"{token}: path data must be single-line and free of quotes/backslashes")
    _, _, w, h = box.split()
    return token, d, float(w), float(h)


def comment(banner: str, prefix: str) -> str:
    """`banner` as a comment block, one `prefix` per line."""
    return "\n".join(f"{prefix} {line}".rstrip() for line in banner.splitlines())


def write(rel: str, body: str) -> pathlib.Path:
    """Write `body` to the repo-relative path `rel` and report it."""
    p = ROOT / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(body)
    print(f"  {rel} ({len(body):,} bytes)")
    return p
