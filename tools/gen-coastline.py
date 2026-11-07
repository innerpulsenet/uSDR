#!/usr/bin/env python3
"""Write crates/desktop/data/coastline-110m.bin from Natural Earth's 1:110m
coastline (public domain, naturalearthdata.com; the GeoJSON beside it came
from github.com/nvkelso/natural-earth-vector, fetched 2026-11-06).

The binary is the map's basemap: little endian, a u32 count of lines, then
per line a u32 point count and that many (i16 lat, i16 lon) in hundredths
of a degree.

    python3 tools/gen-coastline.py
"""
import json
import pathlib
import struct

ROOT = pathlib.Path(__file__).resolve().parent.parent
src = json.loads((ROOT / "crates/desktop/data/ne_110m_coastline.geojson").read_text())
lines = []
for f in src["features"]:
    g = f["geometry"]
    parts = [g["coordinates"]] if g["type"] == "LineString" else g["coordinates"]
    for part in parts:
        lines.append([(round(lat * 100), round(lon * 100)) for lon, lat in part])
out = bytearray(struct.pack("<I", len(lines)))
for line in lines:
    out += struct.pack("<I", len(line))
    for lat, lon in line:
        out += struct.pack("<hh", lat, lon)
(ROOT / "crates/desktop/data/coastline-110m.bin").write_bytes(out)
print(f"{len(lines)} lines, {sum(map(len, lines))} points, {len(out)} bytes")
