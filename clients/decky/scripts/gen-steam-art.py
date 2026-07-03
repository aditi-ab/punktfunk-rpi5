#!/usr/bin/env python3
"""Generate the Steam-shortcut artwork for the Decky plugin (committed, like the tray icons).

The plugin registers a non-Steam shortcut ("Punktfunk") whose grid/hero/logo/icon Steam
would otherwise render as a gray placeholder tile. These assets brand it: the lens mark
(same geometry as scripts/gen-tray-icons.py / web's brand-mark.tsx) over the brand-navy
gradient, plus a monoline "punktfunk" wordmark built from stroke segments ("punktfunk"
needs only p·u·n·k·t·f). The frontend applies them via
SteamClient.Apps.SetCustomArtworkForApp / SetShortcutIcon (src/steam.ts).

Outputs (checked in; re-run only when the brand changes):
  clients/decky/assets/grid.png       600 x 900   library capsule (portrait)
  clients/decky/assets/gridwide.png   920 x 430   wide capsule (recent games / search)
  clients/decky/assets/hero.png      1920 x 620   game-page banner
  clients/decky/assets/logo.png     transparent   overlaid on the hero by Steam
  clients/decky/assets/icon.png       256 x 256   list icon (SetShortcutIcon)

Pure stdlib. Unlike the tiny tray icons this rasterizes big surfaces, so edges are
antialiased analytically from signed distances (one sample per pixel) instead of 4x4
supersampling.
"""

import math
import struct
import zlib
from pathlib import Path

HERE = Path(__file__).resolve().parent.parent  # clients/decky
OUT = HERE / "assets"

# Brand-mark geometry in its 1000-unit viewbox (identical to gen-tray-icons.py).
R = 194.41
C1 = (403.037, 597.262)  # light circle, behind
C2 = (597.8075, 402.8525)  # deep circle, in front
BB_MIN = (C1[0] - R, C2[1] - R)
BB_MAX = (C2[0] + R, C1[1] + R)
MARK_CENTER = ((BB_MIN[0] + BB_MAX[0]) / 2, (BB_MIN[1] + BB_MAX[1]) / 2)
MARK_SPAN = BB_MAX[0] - BB_MIN[0]

COL_LIGHT = (0xA7, 0x9F, 0xF8)
COL_DEEP = (0x6C, 0x5B, 0xF3)
COL_HI = (0xD2, 0xC9, 0xFB)
WORD = (0xEF, 0xEC, 0xFD)  # wordmark: near-white lavender
BG_TOP = (0x28, 0x1E, 0x46)
BG_BOT = (0x12, 0x0D, 0x22)


# ------------------------------------------------------------------------------------------
# Wordmark: monoline glyphs as polylines in a unit box (y down; x-height top y=0, baseline
# y=1, ascender to -0.5, descender to +1.5). Arcs are sampled into the polylines, so the
# rasterizer only ever measures distance-to-segment; round caps/joins fall out of that.
# ------------------------------------------------------------------------------------------
def _arc(cx, cy, r, a0, a1, n=24):
    """Polyline along a circle arc; degrees, 0 = +x, angles grow clockwise on screen."""
    pts = []
    for i in range(n + 1):
        a = math.radians(a0 + (a1 - a0) * i / n)
        pts.append((cx + r * math.cos(a), cy + r * math.sin(a)))
    return pts


GLYPHS = {
    # letter: (advance, [polyline, ...])
    "p": (1.05, [[(0, 0), (0, 1.5)], _arc(0.5, 0.5, 0.5, 0, 360)]),
    "u": (1.05, [[(0, 0), (0, 0.5)], _arc(0.5, 0.5, 0.5, 0, 180), [(1, 0), (1, 0.5)]]),
    "n": (1.05, [[(0, 0), (0, 1)], _arc(0.5, 0.5, 0.5, 180, 360), [(1, 0.5), (1, 1)]]),
    "k": (1.0, [[(0, -0.5), (0, 1)], [(0, 0.62), (0.78, 0)], [(0.30, 0.38), (0.85, 1)]]),
    "t": (0.85, [[(0.42, -0.42), (0.42, 1)], [(0, 0), (0.84, 0)]]),
    "f": (
        0.85,
        [[(0.42, 1), (0.42, -0.15)] + _arc(0.75, -0.15, 0.33, 180, 270, 12), [(0, 0), (0.78, 0)]],
    ),
}
GAP = 0.34  # inter-letter gap, in glyph units
STROKE = 0.26  # stroke thickness, in glyph units
ASCENT, DESCENT = -0.5, 1.5  # glyph-space vertical extent


def word_segments(text):
    """The word's stroke segments [(x1,y1,x2,y2)] in glyph units, plus its unit width."""
    segs = []
    x = 0.0
    for ch in text:
        adv, lines = GLYPHS[ch]
        for line in lines:
            for (x1, y1), (x2, y2) in zip(line, line[1:]):
                segs.append((x + x1, y1, x + x2, y2))
        x += adv + GAP
    return segs, x - GAP


def render_word_alpha(text, unit_px):
    """Coverage (0..255) buffer of the word at `unit_px` pixels per glyph unit."""
    segs, width_u = word_segments(text)
    half = STROKE / 2 * unit_px
    pad = half + 1.5
    w = math.ceil(width_u * unit_px + 2 * pad)
    h = math.ceil((DESCENT - ASCENT) * unit_px + 2 * pad)
    ox, oy = pad, pad - ASCENT * unit_px
    px_segs = [(ox + a * unit_px, oy + b * unit_px, ox + c * unit_px, oy + d * unit_px) for a, b, c, d in segs]
    # Bucket segments per pixel column range so each pixel tests only nearby strokes.
    buf = bytearray(w * h)
    for x1, y1, x2, y2 in px_segs:
        lo_x = max(0, math.floor(min(x1, x2) - pad))
        hi_x = min(w, math.ceil(max(x1, x2) + pad))
        lo_y = max(0, math.floor(min(y1, y2) - pad))
        hi_y = min(h, math.ceil(max(y1, y2) + pad))
        dx, dy = x2 - x1, y2 - y1
        len2 = dx * dx + dy * dy
        for py in range(lo_y, hi_y):
            row = py * w
            fy = py + 0.5
            for px in range(lo_x, hi_x):
                fx = px + 0.5
                if len2 > 0:
                    t = max(0.0, min(1.0, ((fx - x1) * dx + (fy - y1) * dy) / len2))
                else:
                    t = 0.0
                d = math.hypot(fx - (x1 + t * dx), fy - (y1 + t * dy))
                cov = 0.5 + (half - d)
                if cov > 0:
                    v = min(255, round(min(1.0, cov) * 255))
                    if v > buf[row + px]:
                        buf[row + px] = v
    return buf, w, h


# ------------------------------------------------------------------------------------------
# Canvas: RGBA bytearray, straight alpha, painted back to front.
# ------------------------------------------------------------------------------------------
class Canvas:
    def __init__(self, w, h):
        self.w, self.h = w, h
        self.buf = bytearray(w * h * 4)

    def fill_gradient(self, top, bottom):
        for y in range(self.h):
            t = y / max(1, self.h - 1)
            c = bytes(
                (
                    round(top[0] + (bottom[0] - top[0]) * t),
                    round(top[1] + (bottom[1] - top[1]) * t),
                    round(top[2] + (bottom[2] - top[2]) * t),
                    255,
                )
            )
            self.buf[y * self.w * 4 : (y + 1) * self.w * 4] = c * self.w

    def _blend(self, i, rgb, a):
        """`rgb` over the pixel at byte offset i with coverage a (0..1)."""
        if a <= 0:
            return
        b = self.buf
        ia = 1.0 - a
        da = b[i + 3] / 255.0
        oa = a + da * ia
        if oa <= 0:
            return
        for k in range(3):
            b[i + k] = round((rgb[k] * a + b[i + k] * da * ia) / oa)
        b[i + 3] = round(oa * 255)

    def glow(self, cx, cy, radius, rgb, strength):
        """Soft gaussian-ish radial glow (for the mark's halo on the big surfaces)."""
        lo_x = max(0, math.floor(cx - 2.2 * radius))
        hi_x = min(self.w, math.ceil(cx + 2.2 * radius))
        lo_y = max(0, math.floor(cy - 2.2 * radius))
        hi_y = min(self.h, math.ceil(cy + 2.2 * radius))
        for y in range(lo_y, hi_y):
            for x in range(lo_x, hi_x):
                d2 = ((x + 0.5 - cx) ** 2 + (y + 0.5 - cy) ** 2) / (radius * radius)
                a = strength * math.exp(-2.5 * d2)
                if a > 1 / 255:
                    self._blend((y * self.w + x) * 4, rgb, a)

    def mark(self, cx, cy, span):
        """The lens mark centered at (cx, cy) with the given pixel span."""
        scale = span / MARK_SPAN
        c1 = (cx + (C1[0] - MARK_CENTER[0]) * scale, cy + (C1[1] - MARK_CENTER[1]) * scale)
        c2 = (cx + (C2[0] - MARK_CENTER[0]) * scale, cy + (C2[1] - MARK_CENTER[1]) * scale)
        r = R * scale
        lo_x = max(0, math.floor(min(c1[0], c2[0]) - r - 2))
        hi_x = min(self.w, math.ceil(max(c1[0], c2[0]) + r + 2))
        lo_y = max(0, math.floor(min(c1[1], c2[1]) - r - 2))
        hi_y = min(self.h, math.ceil(max(c1[1], c2[1]) + r + 2))
        for y in range(lo_y, hi_y):
            for x in range(lo_x, hi_x):
                fx, fy = x + 0.5, y + 0.5
                cov1 = min(1.0, max(0.0, 0.5 + r - math.hypot(fx - c1[0], fy - c1[1])))
                cov2 = min(1.0, max(0.0, 0.5 + r - math.hypot(fx - c2[0], fy - c2[1])))
                if cov1 <= 0 and cov2 <= 0:
                    continue
                i = (y * self.w + x) * 4
                self._blend(i, COL_LIGHT, cov1)
                self._blend(i, COL_DEEP, cov2)
                self._blend(i, COL_HI, min(cov1, cov2))

    def word(self, text, unit_px, cx, cy):
        """The wordmark centered at (cx, cy); `unit_px` = pixels per glyph unit."""
        alpha, w, h = render_word_alpha(text, unit_px)
        ox = round(cx - w / 2)
        # Optical vertical centering on the x-height band (0..1 in glyph units), not the
        # ascender/descender box — the word reads centered that way.
        pad = STROKE / 2 * unit_px + 1.5
        band_mid = pad - ASCENT * unit_px + 0.5 * unit_px
        oy = round(cy - band_mid)
        for y in range(h):
            ty = y + oy
            if not 0 <= ty < self.h:
                continue
            for x in range(w):
                a = alpha[y * w + x]
                if a:
                    tx = x + ox
                    if 0 <= tx < self.w:
                        self._blend((ty * self.w + tx) * 4, WORD, a / 255.0)

    def round_corners(self, radius):
        """Multiply alpha with a rounded-rect mask (icon)."""
        for y in range(self.h):
            for x in range(self.w):
                dx = max(0.0, max(radius - (x + 0.5), (x + 0.5) - (self.w - radius)))
                dy = max(0.0, max(radius - (y + 0.5), (y + 0.5) - (self.h - radius)))
                if dx > 0 and dy > 0:
                    cov = min(1.0, max(0.0, 0.5 + radius - math.hypot(dx, dy)))
                    i = (y * self.w + x) * 4
                    self.buf[i + 3] = round(self.buf[i + 3] * cov)

    def png(self):
        def chunk(tag, data):
            return (
                struct.pack(">I", len(data))
                + tag
                + data
                + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
            )

        ihdr = struct.pack(">IIBBBBB", self.w, self.h, 8, 6, 0, 0, 0)
        raw = b"".join(
            b"\x00" + bytes(self.buf[y * self.w * 4 : (y + 1) * self.w * 4]) for y in range(self.h)
        )
        return (
            b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", ihdr)
            + chunk(b"IDAT", zlib.compress(raw, 9))
            + chunk(b"IEND", b"")
        )


def save(name, canvas):
    OUT.mkdir(parents=True, exist_ok=True)
    out = OUT / name
    out.write_bytes(canvas.png())
    print(f"wrote {out.relative_to(HERE.parent.parent)} ({canvas.w}x{canvas.h})")


def main():
    # Portrait capsule: mark in the upper half, wordmark beneath.
    c = Canvas(600, 900)
    c.fill_gradient(BG_TOP, BG_BOT)
    c.glow(300, 340, 260, COL_DEEP, 0.35)
    c.mark(300, 340, 320)
    c.word("punktfunk", 44, 300, 640)
    save("grid.png", c)

    # Wide capsule: mark left, wordmark right of it.
    c = Canvas(920, 430)
    c.fill_gradient(BG_TOP, BG_BOT)
    c.glow(230, 215, 200, COL_DEEP, 0.35)
    c.mark(230, 215, 240)
    c.word("punktfunk", 40, 620, 220)
    save("gridwide.png", c)

    # Hero: ambient banner — the mark rides the right third; Steam overlays logo.png itself.
    c = Canvas(1920, 620)
    c.fill_gradient(BG_TOP, BG_BOT)
    c.glow(1500, 310, 330, COL_DEEP, 0.4)
    c.mark(1500, 310, 400)
    save("hero.png", c)

    # Logo (transparent): mark + wordmark side by side, overlaid on the hero by Steam.
    c = Canvas(1120, 300)
    c.mark(150, 150, 240)
    c.word("punktfunk", 62, 660, 155)
    save("logo.png", c)

    # Icon: brand tile, rounded corners, mark only.
    c = Canvas(256, 256)
    c.fill_gradient(BG_TOP, BG_BOT)
    c.glow(128, 128, 110, COL_DEEP, 0.3)
    c.mark(128, 128, 190)
    c.round_corners(36)
    save("icon.png", c)


if __name__ == "__main__":
    main()
