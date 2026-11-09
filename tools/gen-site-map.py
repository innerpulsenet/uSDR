#!/usr/bin/env python3
"""Writes the site's static map data from what the repo already holds:
Natural Earth 110m coastlines and the HFDL ground stations in the directory.
Run it when either changes."""
import json
import os

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DATA = os.path.join(ROOT, "crates/desktop/data")
OUT = os.path.join(ROOT, "site/data")


def main():
    os.makedirs(OUT, exist_ok=True)
    with open(os.path.join(DATA, "ne_110m_coastline.geojson")) as f:
        coast = json.load(f)
    lines = []
    for feature in coast["features"]:
        geometry = feature["geometry"]
        parts = [geometry["coordinates"]] if geometry["type"] == "LineString" else geometry["coordinates"]
        for part in parts:
            lines.append([[round(x, 2), round(y, 2)] for x, y in part])
    with open(os.path.join(OUT, "coast-110m.json"), "w") as f:
        json.dump(lines, f, separators=(",", ":"))

    with open(os.path.join(DATA, "hf-directory.json")) as f:
        directory = json.load(f)
    ground = [{"id": g["gs_id"], "name": g["name"], "lat": g["lat"], "lon": g["lon"]} for g in directory["hfdl"]]
    with open(os.path.join(OUT, "ground-stations.json"), "w") as f:
        json.dump(ground, f, separators=(",", ":"))
    print(len(lines), "coast lines,", len(ground), "ground stations")


if __name__ == "__main__":
    main()
