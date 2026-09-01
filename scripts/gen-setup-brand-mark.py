#!/usr/bin/env python3
"""Rasterize the lens mark for the Windows setup wizard.

web/public/favicon.svg is pure geometry — two equal circles whose intersection is the lens —
so this renders it the way ui/logo.rs does for the terminal: a point-in-circle test per
(super)sample, no SVG library. Reactor's image element is file:///-URI raster only (see
clients/windows/src/app/os_icons.rs), which is why the wizard ships a PNG at all.

Writes crates/punktfunk-setup-win/assets/lens-mark.png. Stdlib only; run from the repo root
and commit the result — the PNG changes only when the mark does.
"""

import struct
import zlib
from pathlib import Path

# Circle centres and radius in the favicon's 1000x1000 viewBox, read off its two arcs —
# the same constants ui/logo.rs carries.
R = 194.41
LIGHT_C = (403.037, 597.262)
DEEP_C = (597.808, 402.853)
LIGHT = (0xA7, 0x9F, 0xF8)
DEEP = (0x6C, 0x5B, 0xF3)
LENS = (0xD2, 0xC9, 0xFB)

WIDTH = 512  # display size tops out ~96 px; 512 leaves headroom for any DPI
SS = 4  # supersamples per axis (16 per pixel) — the anti-aliasing

OUT = Path(__file__).resolve().parent.parent / "crates/punktfunk-setup-win/assets/lens-mark.png"


def color_at(x: float, y: float):
    in_light = (x - LIGHT_C[0]) ** 2 + (y - LIGHT_C[1]) ** 2 <= R * R
    in_deep = (x - DEEP_C[0]) ** 2 + (y - DEEP_C[1]) ** 2 <= R * R
    if in_light and in_deep:
        return LENS
    if in_light:
        return LIGHT
    if in_deep:
        return DEEP
    return None


def render():
    pad = 4.0  # viewBox units, so the AA edge never clips
    x0 = min(LIGHT_C[0], DEEP_C[0]) - R - pad
    y0 = min(LIGHT_C[1], DEEP_C[1]) - R - pad
    x1 = max(LIGHT_C[0], DEEP_C[0]) + R + pad
    y1 = max(LIGHT_C[1], DEEP_C[1]) + R + pad
    scale = (x1 - x0) / WIDTH
    height = round((y1 - y0) / scale)

    rows = []
    for py in range(height):
        row = bytearray([0])  # filter type 0
        for px in range(WIDTH):
            r = g = b = a = 0
            for sy in range(SS):
                for sx in range(SS):
                    x = x0 + (px + (sx + 0.5) / SS) * scale
                    y = y0 + (py + (sy + 0.5) / SS) * scale
                    c = color_at(x, y)
                    if c is not None:
                        r += c[0]
                        g += c[1]
                        b += c[2]
                        a += 255
            n = SS * SS
            # Premultiplied average, unpremultiplied for straight-alpha PNG.
            if a:
                row += bytes((round(r * 255 / a), round(g * 255 / a), round(b * 255 / a), round(a / n)))
            else:
                row += bytes((0, 0, 0, 0))
        rows.append(bytes(row))
    return height, b"".join(rows)


def chunk(tag: bytes, data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data))


def main():
    height, raw = render()
    ihdr = struct.pack(">IIBBBBB", WIDTH, height, 8, 6, 0, 0, 0)  # 8-bit RGBA
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_bytes(png)
    print(f"wrote {OUT} ({WIDTH}x{height}, {len(png)} bytes)")


if __name__ == "__main__":
    main()
