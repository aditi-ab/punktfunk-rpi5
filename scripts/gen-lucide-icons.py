#!/usr/bin/env python3
"""Fold each Lucide SVG's shapes into one SVG path string and emit Rust consts.

Usage: gen-lucide.py <svg-dir>  →  prints the const block for icons.rs.
"""
import re
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

NS = "{http://www.w3.org/2000/svg}"


def num(s):
    v = float(s)
    return int(v) if v == int(v) else v


def absolutize_leading_move(d):
    """A leading relative `m` is absolute only at the START of a path; concatenated after
    another element it would move relative to that element's end. Rewrite it as an absolute
    `M` — and since implicit pairs after `m` are RELATIVE linetos, spell the `l` out."""
    m = re.match(r"^m\s*(-?[\d.]+)[\s,]+(-?[\d.]+)\s*(.*)$", d, re.S)
    if not m:
        return d
    x, y, rest = m.group(1), m.group(2), m.group(3)
    if rest and not rest[0].isalpha():
        rest = "l" + rest
    return f"M{x} {y}{rest}"


def shape_to_path(el):
    tag = el.tag.removeprefix(NS)
    a = el.attrib
    if tag == "path":
        return absolutize_leading_move(a["d"])
    if tag == "line":
        x1, y1, x2, y2 = (num(a[k]) for k in ("x1", "y1", "x2", "y2"))
        return f"M{x1} {y1}L{x2} {y2}"
    if tag == "circle":
        cx, cy, r = num(a["cx"]), num(a["cy"]), num(a["r"])
        return (
            f"M{num(cx - r)} {cy}"
            f"a{r} {r} 0 1 0 {num(2 * r)} 0"
            f"a{r} {r} 0 1 0 {num(-2 * r)} 0"
        )
    if tag == "rect":
        x, y = num(a.get("x", "0")), num(a.get("y", "0"))
        w, h = num(a["width"]), num(a["height"])
        rx = num(a.get("rx", "0"))
        ry = num(a.get("ry", str(rx))) or rx
        if not rx:
            return f"M{x} {y}h{w}v{h}h{num(-w)}z"
        return (
            f"M{num(x + rx)} {y}h{num(w - 2 * rx)}"
            f"a{rx} {ry} 0 0 1 {rx} {ry}v{num(h - 2 * ry)}"
            f"a{rx} {ry} 0 0 1 {num(-rx)} {ry}h{num(-(w - 2 * rx))}"
            f"a{rx} {ry} 0 0 1 {num(-rx)} {num(-ry)}v{num(-(h - 2 * ry))}"
            f"a{rx} {ry} 0 0 1 {rx} {num(-ry)}z"
        )
    if tag in ("polyline", "polygon"):
        pts = re.findall(r"[-\d.]+", a["points"])
        pairs = [f"{num(pts[i])} {num(pts[i + 1])}" for i in range(0, len(pts), 2)]
        d = "M" + "L".join(pairs)
        return d + ("z" if tag == "polygon" else "")
    raise SystemExit(f"unhandled element <{tag}>")


def convert(svg_file):
    root = ET.parse(svg_file).getroot()
    assert root.attrib.get("viewBox") == "0 0 24 24", svg_file
    assert root.attrib.get("stroke-width") == "2", svg_file
    return "".join(shape_to_path(el) for el in root)


def main():
    d = Path(sys.argv[1])
    for f in sorted(d.glob("*.svg")):
        name = f.stem.replace("-", "_").upper()
        data = convert(f)
        assert '"' not in data
        print(f'pub(crate) const {name}: Icon = Icon("{data}");')


if __name__ == "__main__":
    main()
