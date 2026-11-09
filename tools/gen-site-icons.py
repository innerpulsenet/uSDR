#!/usr/bin/env python3
"""Draws the HF Log icons from the mark: the six bars on an amber boom.
Writes site/favicon.svg, site/favicon.ico and site/apple-touch-icon.png.
Needs rsvg-convert and Pillow. Run it when the mark changes."""
import io
import os
import subprocess

from PIL import Image

SITE = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "site")
INK, EMBER, DEEP, AMBER, AMBER_DEEP = "#0b0d10", "#f43a2a", "#cf2a1c", "#ffb347", "#b06a00"

# On a 16-pixel grid every bar is one whole pixel wide and the boom two tall.
PIXEL_BARS = '<rect x="3" y="7" width="1" height="2"/><rect x="5" y="3" width="1" height="10"/><rect x="7" y="4" width="1" height="8"/><rect x="9" y="5" width="1" height="6"/><rect x="11" y="3" width="1" height="10"/><rect x="13" y="7" width="1" height="2"/>'
PIXEL_BOOM = '<rect x="4" y="7" width="9" height="2"/>'
BARS = '<rect x="-103" y="-23" width="22" height="46" rx="4"/><rect x="-66.2" y="-87.74" width="22" height="175.48" rx="4"/><rect x="-29.4" y="-67.21" width="22" height="134.41" rx="4"/><rect x="7.4" y="-53" width="22" height="106" rx="4"/><rect x="44.2" y="-93.16" width="22" height="186.33" rx="4"/><rect x="81" y="-23" width="22" height="46" rx="4"/>'


def png(svg, size):
    out = subprocess.run(["rsvg-convert", "-w", str(size), "-h", str(size)], input=svg.encode(), capture_output=True, check=True).stdout
    return Image.open(io.BytesIO(out)).convert("RGBA")


def main():
    with open(os.path.join(SITE, "favicon.svg"), "w") as f:
        f.write(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><style>.b{{fill:{DEEP}}}.m{{fill:{AMBER_DEEP}}}@media (prefers-color-scheme:dark){{.b{{fill:{EMBER}}}.m{{fill:{AMBER}}}}}</style><g shape-rendering="crispEdges"><g class="m">{PIXEL_BOOM}</g><g class="b">{PIXEL_BARS}</g></g></svg>')
    tile = f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><rect width="16" height="16" rx="3" fill="{INK}"/><g shape-rendering="crispEdges"><g fill="{AMBER}">{PIXEL_BOOM}</g><g fill="{EMBER}">{PIXEL_BARS}</g></g></svg>'
    # Each size is drawn on its own, so the 16-pixel one keeps whole pixels.
    png(tile, 48).save(os.path.join(SITE, "favicon.ico"), sizes=[(16, 16), (32, 32), (48, 48)], append_images=[png(tile, 16), png(tile, 32)])
    touch = f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 180 180"><rect width="180" height="180" rx="36" fill="{INK}"/><g transform="translate(90 90) scale(.66)"><rect x="-98" y="-5" width="196" height="10" rx="5" fill="{AMBER}"/><g fill="{EMBER}">{BARS}</g></g></svg>'
    png(touch, 180).save(os.path.join(SITE, "apple-touch-icon.png"))


if __name__ == "__main__":
    main()
