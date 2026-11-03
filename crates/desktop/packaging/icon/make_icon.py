#!/usr/bin/env python3
"""Draw the uSDR app icon at every size the launcher and Windows take.

The innerpulse tile and mark with a receiver's twist: the six bars stand as
carriers on a panadapter, the tallest is tuned (an amber passband and VFO
line through it), and below the frequency scale the carriers write into a
waterfall in the Midnight ramp. Carbon ground, steel rim lit from above.

128 px and up come from the vector (usdr-icon.svg, rendered by
rsvg-convert). 16 to 64 px are laid out on their own pixel grid, as the brand
kit's small icons are, so the bars stay separate instead of smearing into a
block: 16 and 24 carry only the tuned mark, 32 to 64 add the scale and the
waterfall.

    python3 packaging/icon/make_icon.py        # writes assets/icon/*
"""

import math
import subprocess
import sys
from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
OUT = HERE.parent.parent / "assets" / "icon"

HEIGHTS = [12.0, 76.74, 56.21, 42.0, 82.16, 12.0]
PITCH = 36.8
BAR_W = 22.0
TUNED = 4  # the tallest carrier

CARBON_TOP = (0x17, 0x1a, 0x1f)
CARBON = (0x0b, 0x0d, 0x10)
EMBER = (0xf4, 0x3a, 0x2a)
AMBER = (0xff, 0xb3, 0x47)
STEEL = (0x8f, 0x94, 0x9b)

# Colormap::Midnight, as in src/colormap.rs.
MIDNIGHT = [
    (0.00, (0x00, 0x00, 0x00)),
    (0.34, (0x00, 0x03, 0x10)),
    (0.46, (0x00, 0x18, 0x5A)),
    (0.56, (0x0A, 0x3C, 0xC0)),
    (0.64, (0x2A, 0x7C, 0xF0)),
    (0.71, (0x8A, 0xC8, 0xFF)),
    (0.78, (0xFF, 0xE8, 0x5A)),
    (0.87, (0xFF, 0xA0, 0x20)),
    (0.94, (0xFF, 0x3A, 0x18)),
    (1.00, (0xFF, 0xF0, 0xE6)),
]


def ramp(t):
    t = min(max(t, 0.0), 1.0)
    for (t0, a), (t1, b) in zip(MIDNIGHT, MIDNIGHT[1:]):
        if t <= t1:
            u = (t - t0) / (t1 - t0)
            return tuple(round(x + (y - x) * u) for x, y in zip(a, b))
    return MIDNIGHT[-1][1]


def hexc(c):
    return "#%02x%02x%02x" % c


def noise(i, j):
    """Deterministic grain in 0..1 for waterfall cell (i, j)."""
    n = (i * 374761393 + j * 668265263) & 0xFFFFFFFF
    n = ((n ^ (n >> 13)) * 1274126177) & 0xFFFFFFFF
    return ((n ^ (n >> 16)) & 0xFFFF) / 0xFFFF


def level(x, age, i, j):
    """Waterfall level at mark-x `x` (−110…110), `age` 0 (newest) … 1.

    Each carrier's strength breathes a little line to line, so the columns
    read as received signals rather than painted stripes."""
    v = 0.22 + 0.16 * noise(i, j) ** 2
    for k, h in enumerate(HEIGHTS):
        cx = -92.0 + PITCH * k
        d = (x - cx) / 6.0
        swell = 0.86 + 0.14 * math.sin(age * 9.0 + k * 1.7)
        v += (h / 82.16) * 0.78 * swell * math.exp(-d * d)
    return v * (1.0 - 0.18 * age)


# ---------- the vector (128 px and up) ----------

K = 0.68  # mark scale: pitch 25u, the snapped icons' 0.125 of the tile
CY = -20.0  # mark center
BASE = 50.0  # the frequency scale


def vector_svg():
    cx_t = (-92.0 + PITCH * TUNED) * K
    out = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="-100 -100 200 200" width="1024" height="1024" role="img" aria-label="uSDR">',
        "<defs>",
        f'<linearGradient id="tile" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{hexc(CARBON_TOP)}"/><stop offset=".55" stop-color="{hexc(CARBON)}"/></linearGradient>',
        '<linearGradient id="rim" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#e3e6e9" stop-opacity=".30"/><stop offset=".35" stop-color="#e3e6e9" stop-opacity=".06"/><stop offset="1" stop-color="#e3e6e9" stop-opacity=".03"/></linearGradient>',
        f'<linearGradient id="pass" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{hexc(AMBER)}" stop-opacity=".04"/><stop offset="1" stop-color="{hexc(AMBER)}" stop-opacity=".16"/></linearGradient>',
        f'<linearGradient id="fade" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{hexc(CARBON)}" stop-opacity="0"/><stop offset="1" stop-color="{hexc(CARBON)}" stop-opacity=".92"/></linearGradient>',
        '<clipPath id="clip"><rect x="-100" y="-100" width="200" height="200" rx="45"/></clipPath>',
        "</defs>",
        '<rect x="-100" y="-100" width="200" height="200" rx="45" fill="url(#tile)"/>',
        '<g clip-path="url(#clip)">',
    ]
    # Graticule: three faint divisions over the panadapter.
    for y in (-70.0, -40.0, -10.0, 20.0):
        out.append(f'<rect x="-100" y="{y - 0.3:.2f}" width="200" height=".6" fill="#e3e6e9" opacity=".05"/>')
    # The waterfall, newest line at the top.
    cell_w, cell_h = 2.5, 3.0
    rows = int((100 - BASE - 1.5) / cell_h) + 1
    cols = int(200 / cell_w)
    for j in range(rows):
        y = BASE + 1.5 + j * cell_h
        age = j / max(rows - 1, 1)
        for i in range(cols):
            x = -100 + i * cell_w
            c = ramp(level((x + cell_w / 2) / K, age, i, j))
            if c != (0, 0, 0):
                out.append(f'<rect x="{x:.2f}" y="{y:.2f}" width="{cell_w + 0.05:.2f}" height="{cell_h + 0.05:.2f}" fill="{hexc(c)}"/>')
    out.append(f'<rect x="-100" y="{BASE}" width="200" height="{100 - BASE}" fill="url(#fade)"/>')
    # The passband and the VFO line behind the tuned carrier.
    out.append(f'<rect x="{cx_t - 15:.2f}" y="-100" width="30" height="{BASE + 100:.2f}" fill="url(#pass)"/>')
    out.append(f'<rect x="{cx_t - 15:.2f}" y="-100" width=".8" height="{BASE + 100:.2f}" fill="{hexc(AMBER)}" opacity=".35"/>')
    out.append(f'<rect x="{cx_t + 14.2:.2f}" y="-100" width=".8" height="{BASE + 100:.2f}" fill="{hexc(AMBER)}" opacity=".35"/>')
    out.append(f'<rect x="{cx_t - 0.9:.2f}" y="-100" width="1.8" height="{BASE + 100:.2f}" fill="{hexc(AMBER)}"/>')
    # The frequency scale and its ticks.
    out.append(f'<rect x="-100" y="{BASE - 0.6}" width="200" height="1.2" fill="{hexc(STEEL)}" opacity=".7"/>')
    for k in range(-3, 4):
        x = k * PITCH * K - 0.6
        out.append(f'<rect x="{x:.2f}" y="{BASE - 5}" width="1.2" height="4.4" fill="{hexc(STEEL)}" opacity=".7"/>')
    out.append("</g>")
    # The mark.
    out.append(f'<g fill="{hexc(EMBER)}">')
    for k, h in enumerate(HEIGHTS):
        x = (-92.0 + PITCH * k - BAR_W / 2) * K
        half = (h + 11.0) * K
        out.append(f'<rect x="{x:.2f}" y="{CY - half:.2f}" width="{BAR_W * K:.2f}" height="{2 * half:.2f}" rx="{4 * K:.2f}"/>')
    out.append("</g>")
    out.append('<rect x="-99.25" y="-99.25" width="198.5" height="198.5" rx="44.25" fill="none" stroke="url(#rim)" stroke-width="1.5"/>')
    out.append("</svg>")
    return "\n".join(out) + "\n"


# ---------- whole-pixel icons (16 to 64 px) ----------


def tile_mask(size, radius):
    """The rounded tile's coverage, supersampled, as a list of rows."""
    ss = 8
    big = size * ss
    r = radius * ss
    cov = []
    for y in range(size):
        row = []
        for x in range(size):
            hit = 0
            for sy in range(ss):
                py = y * ss + sy + 0.5
                for sx in range(ss):
                    px = x * ss + sx + 0.5
                    dx = max(r - px, px - (big - r), 0)
                    dy = max(r - py, py - (big - r), 0)
                    hit += dx * dx + dy * dy <= r * r
            row.append(hit / (ss * ss))
        cov.append(row)
    return cov


def snapped(size):
    tiny = size <= 24
    radius = size * (0.15 if tiny else 0.225)
    cov = tile_mask(size, radius)
    px = [[None] * size for _ in range(size)]

    def put(x, y, c, a=1.0):
        if 0 <= x < size and 0 <= y < size:
            base = px[y][x]
            px[y][x] = tuple(round(b + (cc - b) * a) for b, cc in zip(base, c))

    for y in range(size):
        t = min(y / (size * 0.55), 1.0)
        bg = tuple(round(a + (b - a) * t) for a, b in zip(CARBON_TOP, CARBON))
        for x in range(size):
            px[y][x] = bg

    if tiny:
        # The brand kit's tray layout: the mark centered, a wider pitch at 24.
        pitch = math.floor(size * 0.19) if size == 24 else max(2, math.floor(size * 0.147))
        cy, base = size / 2, None
    else:
        pitch = math.floor(size * 0.125)
        cy, base = round(size * 0.41), round(size * 0.765)
    w = math.ceil(pitch / 2)
    unit = pitch / PITCH
    first = size / 2 - pitch * 5 / 2
    centers = [round(first + pitch * k - w / 2) for k in range(6)]
    bottom = base if base else size

    # Passband: the tuned carrier's column, faint amber, its center line full.
    tx = centers[TUNED]
    side = 1 if tiny else w // 2 + max(1, pitch // 3)
    for y in range(bottom):
        for x in range(tx - side, tx + w + side):
            put(x, y, AMBER, (0.20 if tiny else 0.16) + 0.08 * y / bottom)
    if not tiny:
        line_x = tx + w // 2 if w % 2 else tx + w // 2 - 1
        for y in range(bottom):
            put(line_x, y, AMBER, 0.9)

    if base:
        rows = size - base - 1
        for j in range(rows):
            age = j / max(rows - 1, 1)
            for x in range(size):
                mx = (x + 0.5 - size / 2) / (unit * 1.0)
                c = ramp(level(mx, age, x, j))
                put(x, base + 1 + j, c)
                # fade into the tile at the bottom
                put(x, base + 1 + j, CARBON, 0.75 * age)
        for x in range(size):
            put(x, base, STEEL, 0.75)

    for k, h in enumerate(HEIGHTS):
        half = max(w, round(h * unit + w / 2))
        top = round(cy - half)
        for y in range(top, top + 2 * half):
            for x in range(centers[k], centers[k] + w):
                put(x, y, EMBER)

    img = Image.new("RGBA", (size, size))
    img.putdata([(*px[y][x], round(255 * cov[y][x])) for y in range(size) for x in range(size)])
    return img


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    svg = OUT / "usdr-icon.svg"
    svg.write_text(vector_svg())
    for n in (128, 256, 512, 1024):
        subprocess.run(["rsvg-convert", "-w", str(n), "-h", str(n), str(svg), "-o", str(OUT / f"usdr-{n}.png")], check=True)
    for n in (16, 24, 32, 48, 64):
        snapped(n).save(OUT / f"usdr-{n}.png")
    frames = [str(OUT / f"usdr-{n}.png") for n in (16, 24, 32, 48, 64, 128, 256)]
    subprocess.run(["magick", *frames, str(OUT / "usdr.ico")], check=True)
    print("wrote", OUT)


if __name__ == "__main__":
    sys.exit(main())
