#!/usr/bin/env python3
"""Draw the uSDR app icon at every size the launcher, Windows and the
server's page take.

The innerpulse mark split at its axis: each of the six bars is ember above
the line and a strip of waterfall below it, as if the mark were a row of
carriers writing down the screen. Each strip has its carrier (a bright
vertical trace) and keyed bursts on it, and the four middle bars key U, S, D
and R in Morse, newest at the top. Carbon tile, steel rim lit from above.

Geometry is the brand kit's: bars 22u wide with 4u corners at a 36.8u pitch,
heights 12 · 76.74 · 56.21 · 42 · 82.16 · 12, each bar standing h + 11 above
and below its axis (as `brand::paint_mark` draws it).

128 px and up come from the vector, usdr-icon.svg. 16 to 64 px are laid out
on their own pixel grid (usdr-icon-NN.svg, every edge on a whole pixel) so
the bars stay separate instead of smearing into a block; both go through
rsvg-convert. The 32 px grid is also the server page's favicon.

    python3 crates/desktop/packaging/icon/make_icon.py   # writes assets/icon/*

Needs rsvg-convert and ImageMagick (`magick`) for the .ico.
"""

import subprocess
import sys
import urllib.parse
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE.parent.parent / "assets" / "icon"
SERVER_PAGE = HERE.parents[2] / "server" / "web" / "index.html"

HEIGHTS = [12.0, 76.74, 56.21, 42.0, 82.16, 12.0]
PITCH = 36.8
BAR_W = 22.0
CORNER = 4.0

CARBON_TOP = "#171a1f"
CARBON = "#0b0d10"
EMBER = "#f43a2a"  # theme::EMBER
# The waterfall, from Colormap::Midnight: navy floor, blue, the yellow and
# orange of a strong signal, and the warm white it burns to.
NAVY = "#05207e"  # the oldest lines
BLUE = "#0a40d0"
DEEP = "#0a2fa4"  # between navy and blue: the older half of a strip
AZURE = "#1a80f8"
YELLOW = "#ffe050"
ORANGE = "#ffa020"
HOT = "#fff0e6"

# Morse on each bar, as (start, length) in dot units down from the axis.
DIT, DAH = 1, 3


def morse(code, start):
    out, t = [], start
    for c in code:
        n = DIT if c == "." else DAH
        out.append((t, n))
        t += n + 1
    return out


KEYING = [
    morse(".", 1),
    morse("..-", 1),  # U
    morse("...", 2),  # S
    morse("-..", 1),  # D
    morse(".-.", 2),  # R
    morse(".", 2),
]
# The two tall bars also carry a weaker carrier just up the band.
SECOND = {1, 4}


def noise(i, j):
    """Deterministic grain in 0..1."""
    n = (i * 374761393 + j * 668265263) & 0xFFFFFFFF
    n = ((n ^ (n >> 13)) * 1274126177) & 0xFFFFFFFF
    return ((n ^ (n >> 16)) & 0xFFFF) / 0xFFFF


def f(v):
    return f"{v:.2f}".rstrip("0").rstrip(".")


# ---------- the vector (128 px and up) ----------

K = 0.74  # mark scale in the 200u tile: the mark spans 76% of it
DOT = 4.2  # one Morse unit, in mark units


def vector_svg():
    bw, r = BAR_W * K, CORNER * K
    defs = [
        f'<linearGradient id="tile" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{CARBON_TOP}"/><stop offset=".55" stop-color="{CARBON}"/></linearGradient>',
        '<linearGradient id="rim" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#e3e6e9" stop-opacity=".30"/><stop offset=".35" stop-color="#e3e6e9" stop-opacity=".06"/><stop offset="1" stop-color="#e3e6e9" stop-opacity=".03"/></linearGradient>',
        # Newest line at the top, a touch brighter; older lines sink to navy.
        f'<linearGradient id="wf" x1="0" y1="0" x2="0" y2="70" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="{BLUE}"/><stop offset=".5" stop-color="{DEEP}"/><stop offset="1" stop-color="{NAVY}"/></linearGradient>',
        # A keyed burst: orange at the skirts, yellow toward the carrier.
        f'<linearGradient id="burst" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="{ORANGE}" stop-opacity="0"/><stop offset=".22" stop-color="{ORANGE}" stop-opacity=".9"/><stop offset=".5" stop-color="{YELLOW}"/><stop offset=".78" stop-color="{ORANGE}" stop-opacity=".9"/><stop offset="1" stop-color="{ORANGE}" stop-opacity="0"/></linearGradient>',
        f'<linearGradient id="glow" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="{YELLOW}" stop-opacity="0"/><stop offset=".5" stop-color="{YELLOW}" stop-opacity=".45"/><stop offset="1" stop-color="{YELLOW}" stop-opacity="0"/></linearGradient>',
    ]
    bars = []
    for k, h in enumerate(HEIGHTS):
        cx = (-92.0 + PITCH * k) * K
        half = (h + 11.0) * K
        x0 = cx - bw / 2
        defs.append(f'<clipPath id="b{k}"><rect x="{f(x0)}" y="{f(-half)}" width="{f(bw)}" height="{f(2 * half)}" rx="{f(r)}"/></clipPath>')
        g = [f'<g clip-path="url(#b{k})">']
        g.append(f'<rect x="{f(x0)}" y="{f(-half)}" width="{f(bw)}" height="{f(half)}" fill="{EMBER}"/>')
        g.append(f'<rect x="{f(x0)}" y="0" width="{f(bw)}" height="{f(half)}" fill="url(#wf)"/>')
        # Line-to-line grain: the strip reads as received, not painted.
        y, j = 0.0, 0
        while y < half:
            a = 0.04 + 0.16 * noise(k, j) ** 2
            g.append(f'<rect x="{f(x0)}" y="{f(y)}" width="{f(bw)}" height=".7" fill="{AZURE}" opacity="{a:.2f}"/>')
            y += 1.4
            j += 1
        if k in SECOND:
            g.append(f'<rect x="{f(cx + 4.9)}" y="0" width=".6" height="{f(half)}" fill="{ORANGE}" opacity=".55"/>')
        g.append(f'<rect x="{f(cx - 2.4)}" y="0" width="4.8" height="{f(half)}" fill="url(#glow)"/>')
        g.append(f'<rect x="{f(cx - 0.55)}" y="0" width="1.1" height="{f(half)}" fill="{YELLOW}"/>')
        for t, n in KEYING[k]:
            by, bh = t * DOT, n * DOT * 0.86
            g.append(f'<rect x="{f(cx - 7.2)}" y="{f(by)}" width="14.4" height="{f(bh)}" rx="{f(min(bh, 2.4) / 2)}" fill="url(#burst)"/>')
            g.append(f'<rect x="{f(cx - 0.75)}" y="{f(by)}" width="1.5" height="{f(bh)}" fill="{HOT}"/>')
        g.append("</g>")
        bars.append("".join(g))
    return "\n".join(
        [
            '<svg xmlns="http://www.w3.org/2000/svg" viewBox="-100 -100 200 200" width="1024" height="1024" role="img" aria-label="uSDR">',
            "<defs>",
            *defs,
            "</defs>",
            '<rect x="-100" y="-100" width="200" height="200" rx="45" fill="url(#tile)"/>',
            *bars,
            '<rect x="-99.25" y="-99.25" width="198.5" height="198.5" rx="44.25" fill="none" stroke="url(#rim)" stroke-width="1.5"/>',
            "</svg>",
        ]
    ) + "\n"


# ---------- the pixel grids (16 to 64 px) ----------

# size: (pitch, bar width) in pixels. Odd widths from 48 up leave a center
# column for the carrier.
GRID = {16: (2, 1), 24: (3, 2), 32: (4, 2), 48: (6, 3), 64: (8, 5)}


def grid_svg(n):
    pitch, w = GRID[n]
    unit = pitch / PITCH
    axis = n // 2
    x_first = (n - (5 * pitch + w)) // 2
    rx = 0.15 * n if n <= 24 else 0.225 * n
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {n} {n}" width="{n}" height="{n}" role="img" aria-label="uSDR">',
        f'<defs><linearGradient id="tile" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{CARBON_TOP}"/><stop offset=".55" stop-color="{CARBON}"/></linearGradient></defs>',
        f'<rect width="{n}" height="{n}" rx="{f(rx)}" fill="url(#tile)"/>',
    ]
    if n >= 48:
        out.append(f'<rect x=".5" y=".5" width="{n - 1}" height="{n - 1}" rx="{f(rx - 0.5)}" fill="none" stroke="#e3e6e9" stroke-opacity=".10"/>')
    for k, h in enumerate(HEIGHTS):
        x = x_first + pitch * k
        half = max(1, round((h + 11.0) * unit))
        out.append(f'<rect x="{x}" y="{axis - half}" width="{w}" height="{half}" fill="{EMBER}"/>')
        # The strip: brighter blue at small sizes, where navy would vanish
        # into the tile.
        for j in range(half):
            age = j / max(half - 1, 1)
            c = BLUE if n <= 48 or age < 0.5 else DEEP
            out.append(f'<rect x="{x}" y="{axis + j}" width="{w}" height="1" fill="{c}"/>')
        keyed = set()
        for t, m in KEYING[k]:
            keyed.update(range(t, t + m))
        if n <= 24:
            # Too narrow for a carrier: the newest line glows across each
            # strip, and that seam is the whole waterfall at this size.
            out.append(f'<rect x="{x}" y="{axis}" width="{w}" height="1" fill="{YELLOW}"/>')
            continue
        if n <= 32:
            # Two pixels: the seam, then each bar's first keyed element.
            out.append(f'<rect x="{x}" y="{axis}" width="{w}" height="1" fill="{YELLOW}"/>')
            t, m = KEYING[k][0]
            t = max(t, 2)
            for j in range(t, min(t + m, half - 1)):
                out.append(f'<rect x="{x}" y="{axis + j}" width="{w}" height="1" fill="{ORANGE}"/>')
            continue
        mid = x + w // 2
        out.append(f'<rect x="{mid}" y="{axis}" width="1" height="{half}" fill="{YELLOW}"/>')
        for j in range(half):
            if j in keyed:
                out.append(f'<rect x="{mid - 1}" y="{axis + j}" width="3" height="1" fill="{ORANGE}"/>')
                out.append(f'<rect x="{mid}" y="{axis + j}" width="1" height="1" fill="{HOT}"/>')
    out.append("</svg>")
    return "\n".join(out) + "\n"


def render(svg_path, size, png_path):
    subprocess.run(["rsvg-convert", "-w", str(size), "-h", str(size), str(svg_path), "-o", str(png_path)], check=True)


def favicon_href():
    """The 32 px grid as a data URI for the server's page."""
    svg = " ".join(grid_svg(32).split("\n")).replace('"', "'")
    return "data:image/svg+xml," + urllib.parse.quote(svg, safe=" '=:/,.-")


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    svg = OUT / "usdr-icon.svg"
    svg.write_text(vector_svg())
    for n in (128, 256, 512, 1024):
        render(svg, n, OUT / f"usdr-{n}.png")
    for n in GRID:
        small = OUT / f"usdr-icon-{n}.svg"
        small.write_text(grid_svg(n))
        render(small, n, OUT / f"usdr-{n}.png")
    frames = [str(OUT / f"usdr-{n}.png") for n in (16, 24, 32, 48, 64, 128, 256)]
    subprocess.run(["magick", *frames, str(OUT / "usdr.ico")], check=True)

    page = SERVER_PAGE.read_text()
    start = page.index('<link rel="icon" href="') + len('<link rel="icon" href="')
    end = page.index('"', start)
    SERVER_PAGE.write_text(page[:start] + favicon_href() + page[end:])
    print("wrote", OUT, "and the favicon in", SERVER_PAGE)


if __name__ == "__main__":
    sys.exit(main())
