#!/usr/bin/env python3
"""Draw the uSDR app icon at every size the launcher, Windows and the
server's page take.

The innerpulse kit's product icon (`scripts/product-kit.mjs`): the studio
owns the ember plate, so a product's icon is its inverse, a steel plate with
one strike (the beat's tallest bar, the only ember in the icon) and the
product's letters. uSDR's strike is split at its axis: ember above, and
below it a strip of waterfall with its carrier and keyed bursts (a U in
Morse, newest at the top), as if the strike were a signal writing down the
screen. The letters are "uSDR" in Syne 800, on one line right of the
strike; from 32 px down, where four letters do not read, just "u".

Two drawings, as the kit has them. Flat (usdr-icon.svg): one solid steel
tile, the strike and the letters, for the scalable icon a
desktop draws itself (KDE's SVG renderer takes no filters) and for the
server page's favicon; 16 to 48 px lay it out on their own pixel grids
(usdr-icon-NN.svg). Machined (usdr-icon-machined.svg): brushed steel, the
strike's lit half milled through with a lamp in it, the letters in polished
steel; for 64 px and up, rendered to PNG. Both go through rsvg-convert.

The machined drawing is a port of the kit's `machinedParts` (steel: true,
one lit slot, no scale); the geometry, colors and filters are the kit's.

    python3 crates/desktop/packaging/icon/make_icon.py   # writes assets/icon/*

Needs rsvg-convert, ImageMagick (`magick`) for the .ico, and fontTools.
"""

import subprocess
import sys
import urllib.parse
from pathlib import Path

from fontTools.pens.boundsPen import BoundsPen
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont

HERE = Path(__file__).resolve().parent
ASSETS = HERE.parent.parent / "assets"
OUT = ASSETS / "icon"
SYNE = ASSETS / "fonts" / "syne-800.ttf"
SERVER_PAGE = HERE.parents[2] / "server" / "web" / "index.html"

LETTERS = "uSDR"
# What the 16-32 px icons carry: four letters are a smear there, so they
# take the kit's one letter (`--letter`), set larger and centered.
SMALL_LETTERS = "u"

# The kit's colors (pulse.mjs BRAND).
EMBER = "#f43a2a"  # theme::EMBER
HOT = "#ffd9c2"  # a lit slot's lamp
INK2 = "#171a1f"
GUNMETAL = "#2b3037"
TEXT = "#e3e6e9"
# The waterfall, from Colormap::Midnight.
BLUE = "#0a40d0"
DEEP = "#0a2fa4"
NAVY = "#05207e"
AZURE = "#1a80f8"
YELLOW = "#ffe050"
ORANGE = "#ffa020"
WHITE_HOT = "#fff0e6"

# The kit's strike, in plate units (−100…100): the beat's tallest bar.
SX, SY, SW, SH, SR = -70.0, -62.0, 22.0, 124.0, 4.0
CX = SX + SW / 2
DEPTH = 5.0  # how much of a milled slot's lower wall shows

# The letters' box: from the kit's left edge right of the strike, across
# the plate to a margin of 20u.
TEXT_X0, TEXT_W = -32.0, 112.0

# Keyed elements on the strip, as (start, length) in Morse units below the
# axis: U (..-).
DOT = 4.2
KEYING = [(1, 1), (3, 1), (5, 3)]


def f(v):
    s = f"{v:.2f}".rstrip("0").rstrip(".")
    return "0" if s in ("-0", "") else s


def noise(i, j):
    """Deterministic grain in 0..1."""
    n = (i * 374761393 + j * 668265263) & 0xFFFFFFFF
    n = ((n ^ (n >> 13)) * 1274126177) & 0xFFFFFFFF
    return ((n ^ (n >> 16)) & 0xFFFF) / 0xFFFF


def letters_path(fill, text=LETTERS):
    """The letters outlined in Syne 800 with the kit's −0.05 tracking, scaled
    to the box right of the strike and centered on the axis by their ink;
    one letter is set as the kit sets it, at 1.24 and centered at x = 24."""
    font = TTFont(SYNE)
    upm = font["head"].unitsPerEm
    gs = font.getGlyphSet()
    cmap = font.getBestCmap()
    size = 100.0
    scale = size / upm
    pen = SVGPathPen(gs)
    bounds = BoundsPen(gs)
    x = 0.0
    names = [cmap[ord(c)] for c in text]
    for i, name in enumerate(names):
        for p in (pen, bounds):
            gs[name].draw(TransformPen(p, (scale, 0, 0, -scale, x, 0)))
        x += gs[name].width * scale + (-0.05 * size if i < len(names) - 1 else 0)
    d = pen.getCommands()
    x0, top, x1, bottom = bounds.bounds
    if len(text) == 1:
        k = 1.24
        tx = 24 - (x1 - x0) * k / 2 - x0 * k
    else:
        k = TEXT_W / (x1 - x0)
        tx = TEXT_X0 - x0 * k
    ty = -(top + bottom) / 2 * k
    return f'<path transform="translate({f(tx)} {f(ty)}) scale({k:.4f})" d="{d}" fill="{fill}"/>'


def strip(id_, detail):
    """The waterfall in the strike's lower half, inside the strike's clip."""
    half = SH / 2
    out = [f'<rect x="{f(SX)}" y="0" width="{f(SW)}" height="{f(half)}" fill="url(#{id_}wf)"/>']
    if detail:
        # Line-to-line grain, so the strip reads as received, not painted.
        y, j = 0.0, 0
        while y < half:
            a = 0.04 + 0.16 * noise(3, j) ** 2
            out.append(f'<rect x="{f(SX)}" y="{f(y)}" width="{f(SW)}" height=".7" fill="{AZURE}" opacity="{a:.2f}"/>')
            y += 1.4
            j += 1
        out.append(f'<rect x="{f(CX - 3.2)}" y="0" width="6.4" height="{f(half)}" fill="url(#{id_}gl0)"/>')
    line = 1.6 if detail else 3.0
    out.append(f'<rect x="{f(CX - line / 2)}" y="0" width="{f(line)}" height="{f(half)}" fill="{YELLOW}"/>')
    for t, n in KEYING:
        by, bh = t * DOT, n * DOT * 0.86
        if detail:
            out.append(f'<rect x="{f(CX - 9.5)}" y="{f(by)}" width="19" height="{f(bh)}" rx="{f(min(bh, 2.4) / 2)}" fill="url(#{id_}bu)"/>')
            out.append(f'<rect x="{f(CX - 1)}" y="{f(by)}" width="2" height="{f(bh)}" fill="{WHITE_HOT}"/>')
        else:
            out.append(f'<rect x="{f(CX - 7)}" y="{f(by)}" width="14" height="{f(bh)}" fill="{ORANGE}"/>')
    return "".join(out)


def strip_defs(id_):
    return (
        f'<linearGradient id="{id_}wf" x1="0" y1="0" x2="0" y2="{f(SH / 2)}" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="{BLUE}"/><stop offset=".5" stop-color="{DEEP}"/><stop offset="1" stop-color="{NAVY}"/></linearGradient>'
        f'<linearGradient id="{id_}bu" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="{ORANGE}" stop-opacity="0"/><stop offset=".2" stop-color="{ORANGE}" stop-opacity=".9"/><stop offset=".5" stop-color="{YELLOW}"/><stop offset=".8" stop-color="{ORANGE}" stop-opacity=".9"/><stop offset="1" stop-color="{ORANGE}" stop-opacity="0"/></linearGradient>'
        f'<linearGradient id="{id_}gl0" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="{YELLOW}" stop-opacity="0"/><stop offset=".5" stop-color="{YELLOW}" stop-opacity=".45"/><stop offset="1" stop-color="{YELLOW}" stop-opacity="0"/></linearGradient>'
        f'<clipPath id="{id_}sc"><rect x="{f(SX)}" y="{f(SY)}" width="{f(SW)}" height="{f(SH)}" rx="{f(SR)}"/></clipPath>'
    )


def head(size):
    return f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="-100 -100 200 200" width="{size}" height="{size}" role="img" aria-label="uSDR">'


def flat_svg(size=512):
    """The flat product icon: the kit's iconFlat with the strike split."""
    id_ = "usf"
    strike = f'<rect x="{f(SX)}" y="{f(SY)}" width="{f(SW)}" height="{f(SH)}" rx="{f(SR)}"'
    return (
        head(size)
        + f"<defs>{strip_defs(id_)}</defs>"
        + f'<rect x="-100" y="-100" width="200" height="200" rx="26" fill="{INK2}"/>'
        + f'<rect x="-99" y="-99" width="198" height="198" rx="25" fill="none" stroke="{GUNMETAL}" stroke-width="2"/>'
        + f'<g clip-path="url(#{id_}sc)">{strike} fill="{EMBER}"/>{strip(id_, False)}</g>'
        + letters_path(TEXT)
        + "</svg>\n"
    )


def flat_snapped(n, text=LETTERS):
    """The flat icon on an n-pixel grid for 16 to 48 px: the strike's edges,
    its axis and the keyed lines on whole pixels, so the split stays sharp
    where the vector would smear ember and blue together. From 24 px the
    strike is an odd number of pixels wide, so the carrier has a center
    column (keyed lines only at 48, where there is room for them); at 16 the
    strip is blue under a glowing newest line."""
    u = n / 200.0
    x0 = round((SX + 100) * u)
    w = max(2, round(SW * u))
    if w >= 3 and w % 2 == 0:
        w -= 1
    top, bottom, axis = round((SY + 100) * u), round((SY + SH + 100) * u), n // 2
    r = 1 if n >= 32 else 0
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {n} {n}" width="{n}" height="{n}" role="img" aria-label="uSDR">',
        f'<rect width="{n}" height="{n}" rx="{max(2, round(n * 0.13))}" fill="{INK2}"/>',
    ]
    if n >= 32:
        out.append(f'<rect x=".5" y=".5" width="{n - 1}" height="{n - 1}" rx="{max(2, round(n * 0.13)) - 0.5}" fill="none" stroke="{GUNMETAL}"/>')
    out.append(f'<clipPath id="ss"><rect x="{x0}" y="{top}" width="{w}" height="{bottom - top}" rx="{r}"/></clipPath><g clip-path="url(#ss)">')
    out.append(f'<rect x="{x0}" y="{top}" width="{w}" height="{axis - top}" fill="{EMBER}"/>')
    out.append(f'<rect x="{x0}" y="{axis}" width="{w}" height="{bottom - axis}" fill="{BLUE}"/>')
    if w < 3:
        out.append(f'<rect x="{x0}" y="{axis}" width="{w}" height="1" fill="{YELLOW}"/>')
    else:
        mid = x0 + w // 2
        out.append(f'<rect x="{mid}" y="{axis}" width="1" height="{bottom - axis}" fill="{YELLOW}"/>')
        rows = set()
        for t, m in KEYING if n >= 48 else []:
            y0 = round(t * DOT * u)
            y1 = max(y0 + 1, round((t + m * 0.86) * DOT * u))
            rows.update(range(y0, y1))
        for j in sorted(rows):
            if axis + j < bottom - 1:
                out.append(f'<rect x="{x0}" y="{axis + j}" width="{w}" height="1" fill="{ORANGE}"/>')
                out.append(f'<rect x="{mid}" y="{axis + j}" width="1" height="1" fill="{WHITE_HOT}"/>')
    out.append("</g>")
    out.append(f'<g transform="scale({u:.5f}) translate(100 100)">{letters_path(TEXT, text)}</g>')
    out.append("</svg>")
    return "\n".join(out) + "\n"


def machined_svg(size=512):
    """The machined product icon: the kit's machinedParts(steel, one lit
    slot, no scale), the strike's lower half a waterfall well, and the
    letters in polished steel."""
    id_ = "usm"
    body0 = ["#2c3138", "#20242a", "#16191d"]
    wall = ["#4a515b", "#6a727d"]
    rim = ["#000000", "#aab2bc"]

    def grad(name, stops):
        s = "".join(f'<stop offset="{o}" stop-color="{c}"' + (f' stop-opacity="{a}"' if a is not None else "") + "/>" for o, c, a in stops)
        return f'<linearGradient id="{id_}{name}" x1="0" y1="0" x2="0" y2="1">{s}</linearGradient>'

    def grain(name, seed, alpha):
        return (
            f'<filter id="{id_}{name}" x="0" y="0" width="100%" height="100%" color-interpolation-filters="sRGB">'
            f'<feTurbulence type="fractalNoise" baseFrequency="0.005 0.8" numOctaves="2" seed="{seed}" result="n"/>'
            f'<feColorMatrix in="n" type="matrix" values="{alpha}" result="g"/><feComposite in="g" in2="SourceGraphic" operator="in"/></filter>'
        )

    defs = (
        grad("p", [(0, body0[0], None), (0.5, body0[1], None), (1, body0[2], None)])
        + grad("b", [(0, "#ffffff", 0.4), (0.25, "#ffffff", 0.08), (0.75, "#000000", 0.1), (1, "#000000", 0.5)])
        + grad("w", [(0, wall[0], None), (1, wall[1], None)])
        + grad("d", [(0, "#030405", None), (1, "#15181c", None)])
        + grad("r", [(0, rim[0], 0.8), (1, rim[1], 0.55)])
        + grain("gl", 11, "0 0 0 0 1  0 0 0 0 1  0 0 0 0 1  1.5 0 0 0 -0.62")
        + grain("gd", 29, "0 0 0 0 0  0 0 0 0 0  0 0 0 0 0  1.5 0 0 0 -0.62")
        + f'<filter id="{id_}s" x="-100%" y="-20%" width="300%" height="140%"><feGaussianBlur stdDeviation="1.6"/></filter>'
        + f'<filter id="{id_}o" x="-60%" y="-60%" width="220%" height="220%"><feGaussianBlur stdDeviation="7"/></filter>'
        + f'<linearGradient id="{id_}t" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#f6f7f8"/><stop offset=".45" stop-color="#b4bac2"/><stop offset=".55" stop-color="#e2e5e9"/><stop offset="1" stop-color="#8b929b"/></linearGradient>'
        + strip_defs(id_)
    )

    def tile(extra):
        return f'<rect x="-100" y="-100" width="200" height="200" rx="26" {extra}/>'

    at = f'x="{f(SX)}" width="{f(SW)}" rx="{f(SR)}"'
    span = f'y="{f(SY)}" height="{f(SH)}"'
    # The lit slot as the kit draws it (I = 1): the lamp-lit wall, the hot
    # well and a soft lamp, in the upper half; the lower half is the
    # waterfall's well, a hairline of shade under the lit half's edge.
    well = (
        f'<rect {at} {span} fill="{HOT}"/>'
        f'<g clip-path="url(#{id_}sc)"><rect {at} y="{f(SY - DEPTH)}" height="{f(SH)}" fill="#ff5232"/>'
        f'<rect x="{f(SX + SW * 0.3)}" y="{f(SY - DEPTH + SW * 0.3)}" width="{f(SW * 0.4)}" height="{f(SH / 2 - SW * 0.3)}" rx="{f(SW * 0.2)}" fill="{HOT}" opacity="0.85" filter="url(#{id_}s)"/>'
        f'<rect x="{f(SX)}" y="0" width="{f(SW)}" height="{f(SH / 2)}" fill="url(#{id_}d)"/>'
        + strip(id_, True)
        + f'<rect x="{f(SX)}" y="0" width="{f(SW)}" height=".8" fill="#000" opacity=".35"/></g>'
        f'<rect {at} {span} fill="none" stroke="url(#{id_}r)" stroke-width="1"/>'
    )
    bloom = f'<g filter="url(#{id_}o)"><rect {at} y="{f(SY)}" height="{f(SH / 2)}" fill="#ff8a60" opacity="0.6"/></g>'
    body = (
        tile(f'fill="url(#{id_}p)"')
        + tile(f'fill="#fff" filter="url(#{id_}gl)" opacity=".1"')
        + tile(f'fill="#000" filter="url(#{id_}gd)" opacity=".2"')
        + '<rect x="-99.2" y="-99.2" width="198.4" height="198.4" rx="25.2" fill="none" stroke="url(#usmb)" stroke-width="1.6"/>'
        + well
        + bloom
    )
    return head(size) + f"<defs>{defs}</defs>" + body + letters_path(f"url(#{id_}t)") + "</svg>\n"


def render(svg_path, size, png_path):
    subprocess.run(["rsvg-convert", "-w", str(size), "-h", str(size), str(svg_path), "-o", str(png_path)], check=True)


def favicon_href():
    """The 32 px icon as a data URI for the server's page."""
    svg = flat_snapped(32, SMALL_LETTERS or LETTERS).replace('"', "'").replace("\n", "").strip()
    return "data:image/svg+xml," + urllib.parse.quote(svg, safe=" '=:/,.-()")


SMALL = (16, 24, 32, 48)  # the flat icon on each one's own pixel grid
LARGE = (64, 128, 256, 512, 1024)


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    # The waterfall icon's per-size grids are gone: the kit draws the flat
    # icon at every small size.
    flat = OUT / "usdr-icon.svg"
    flat.write_text(flat_svg())
    machined = OUT / "usdr-icon-machined.svg"
    machined.write_text(machined_svg())
    for n in SMALL:
        grid = OUT / f"usdr-icon-{n}.svg"
        grid.write_text(flat_snapped(n, SMALL_LETTERS if SMALL_LETTERS and n <= 32 else LETTERS))
        render(grid, n, OUT / f"usdr-{n}.png")
    for n in LARGE:
        render(machined, n, OUT / f"usdr-{n}.png")
    frames = [str(OUT / f"usdr-{n}.png") for n in (16, 24, 32, 48, 64, 128, 256)]
    subprocess.run(["magick", *frames, str(OUT / "usdr.ico")], check=True)

    page = SERVER_PAGE.read_text()
    start = page.index('<link rel="icon" href="') + len('<link rel="icon" href="')
    end = page.index('"', start)
    SERVER_PAGE.write_text(page[:start] + favicon_href() + page[end:])
    print("wrote", OUT, "and the favicon in", SERVER_PAGE)


if __name__ == "__main__":
    sys.exit(main())
