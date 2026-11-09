#!/usr/bin/env python3
"""Builds the JSON the pages read. Runs on a timer at low priority, so a page
view never touches the database.

Everything here comes from the log. Time not monitored is reported as such,
and a window with no data stays empty rather than being filled in.
"""
import base64
import json
import math
import os
import re
import shutil
import struct
import sys
import time
import zlib

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from hf_schema import connect  # noqa: E402

DB = os.environ.get("HF_DB", "/var/lib/hf-site/hf.sqlite")
OUT = os.environ.get("HF_DATA", "/home/patrickh/www/hf/data")
PICTURES = os.environ.get("HF_PICTURES", "/home/patrickh/www/hf/pictures")

AMATEUR = ("FT8", "FT4", "WSPR", "JS8")
AMATEUR_SQL = ",".join(f"'{m}'" for m in AMATEUR)
KEEP_RAW_S = 365 * 86400
LOW_DISK_BYTES = 8 << 30
HAM_BANDS = [(1.8, 2.0, "160 m"), (3.5, 4.0, "80 m"), (5.25, 5.45, "60 m"), (7.0, 7.3, "40 m"), (10.1, 10.15, "30 m"),
             (14.0, 14.35, "20 m"), (18.068, 18.168, "17 m"), (21.0, 21.45, "15 m"), (24.89, 24.99, "12 m"), (28.0, 29.7, "10 m")]
BAND_ORDER = [b[2] for b in HAM_BANDS]
# Faster than any airliner, so two fixes this far apart are not one flight.
MAX_KMH = 1400


def band(hz):
    mhz = hz / 1e6
    for low, high, name in HAM_BANDS:
        if low <= mhz <= high:
            return name
    return f"{int(mhz)} MHz"


def unhealthy(column="at_utc"):
    return f"NOT EXISTS (SELECT 1 FROM listening l WHERE l.healthy = 0 AND {column} BETWEEN l.start_utc AND l.end_utc)"


def grid_center(g):
    g = g.upper()
    lon = (ord(g[0]) - 65) * 20 - 180 + int(g[2]) * 2 + 1
    lat = (ord(g[1]) - 65) * 10 - 90 + int(g[3]) + 0.5
    return lat, lon


def km(lat1, lon1, lat2, lon2):
    p1, p2 = math.radians(lat1), math.radians(lat2)
    dp, dl = p2 - p1, math.radians(lon2 - lon1)
    h = math.sin(dp / 2) ** 2 + math.cos(p1) * math.cos(p2) * math.sin(dl / 2) ** 2
    return 6371 * 2 * math.asin(min(1, math.sqrt(h)))


def write(name, value):
    path = os.path.join(OUT, name)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    temporary = path + ".tmp"
    with open(temporary, "w") as f:
        json.dump(value, f, separators=(",", ":"))
    os.chmod(temporary, 0o644)
    os.replace(temporary, path)


def meta(conn, name):
    row = conn.execute("SELECT value FROM meta WHERE name = ?", (name,)).fetchone()
    return row[0] if row else None


def track_segments(points):
    """Split a flight's fixes wherever two could not both be the same aircraft."""
    segments, current = [], []
    for fix in points:
        if current:
            t0, la0, lo0 = current[-1]
            hours = max(fix[0] - t0, 1) / 3600
            if km(la0, lo0, fix[1], fix[2]) / hours > MAX_KMH:
                segments.append(current)
                current = []
        current.append(fix)
    if current:
        segments.append(current)
    return segments


def build_map(conn, now, hours, station_grid):
    since = now - hours * 3600
    start = since - since % 3600
    modes, bands, spots, squares, idents = [], [], {}, {}, {}
    def index(table, value):
        if value not in table:
            table.append(value)
        return table.index(value)

    # One row per station, so the page can count stations exactly under any filter
    # and say who was heard in a square. The check is skipped when nothing failed.
    failed = conn.execute("SELECT 1 FROM listening WHERE healthy = 0 AND end_utc >= ? LIMIT 1", (since,)).fetchone()
    healthy_only = f"AND {unhealthy()}" if failed else ""
    for r in conn.execute(f"""SELECT substr(grid,1,4) AS g, mode, dial_hz, (at_utc - ?) / 3600 AS h, ident, COUNT(*) AS n, MAX(snr_db) AS best, MAX(at_utc) AS t
                              FROM observations WHERE at_utc >= ? AND mode IN ({AMATEUR_SQL}) AND grid IS NOT NULL AND ident IS NOT NULL {healthy_only}
                              GROUP BY g, mode, dial_hz, h, ident""", (start, since)):
        key = (squares.setdefault(r["g"].upper(), len(squares)), index(modes, r["mode"]), index(bands, band(r["dial_hz"])), r["h"], idents.setdefault(r["ident"], len(idents)))
        n, best, t = spots.get(key, (0, None, 0))
        spots[key] = (n + r["n"], max(x for x in (best, r["best"], -999) if x is not None), max(t, r["t"]))

    flights = {}
    # Each fix's own time, never the time heard: aircraft send queued reports.
    for r in conn.execute("""SELECT ident, lat, lon, CAST(json_extract(details,'$.fixUtc') AS INTEGER) AS t FROM observations
                             WHERE mode = 'HFDL' AND at_utc >= ? AND lat IS NOT NULL AND json_extract(details,'$.fixUtc') IS NOT NULL""", (since,)):
        if r["t"] >= since and r["ident"]:
            flights.setdefault(r["ident"], {})[r["t"]] = (round(r["lat"], 3), round(r["lon"], 3))
    tracks = []
    for ident, fixes in flights.items():
        points = [[t, *fixes[t]] for t in sorted(fixes)]
        tracks.append({"i": ident, "s": track_segments(points)})
    tracks.sort(key=lambda x: -max(s[-1][0] for s in x["s"]))

    ground = {str(r["g"]): r["n"] for r in conn.execute("SELECT json_extract(details,'$.gsId') AS g, COUNT(*) AS n FROM observations WHERE mode='HFDL' AND at_utc >= ? AND g IS NOT NULL GROUP BY g", (since,))}
    dsc = [{"t": r["at_utc"], "lat": round(r["lat"], 3), "lon": round(r["lon"], 3), "i": r["ident"], "k": r["kind"]}
           for r in conn.execute("SELECT at_utc, lat, lon, ident, kind FROM observations WHERE mode='DSC' AND at_utc >= ? AND lat IS NOT NULL ORDER BY at_utc DESC LIMIT 400", (since,))]
    monitored = [[r["h"], index(modes, r["mode"]), r["s"]] for r in conn.execute(
        "SELECT (start_utc - ?) / 3600 AS h, mode, SUM(end_utc - start_utc) AS s FROM listening WHERE end_utc >= ? AND healthy IS NOT 0 GROUP BY h, mode", (start, since)) if r["h"] >= 0]
    # Time spent on the amateur modes per band, since the map cannot be read without it.
    air = {}
    for r in conn.execute(f"""SELECT (start_utc - ?) / 3600 AS h, dial_hz, SUM(end_utc - start_utc) AS s FROM listening
                              WHERE end_utc >= ? AND healthy IS NOT 0 AND mode IN ({AMATEUR_SQL}) GROUP BY h, dial_hz""", (start, since)):
        if r["h"] >= 0:
            key = (r["h"], index(bands, band(r["dial_hz"])))
            air[key] = air.get(key, 0) + r["s"]
    return {"generated_utc": now, "start_utc": start, "hours": hours, "station_grid": station_grid, "modes": modes, "bands": bands,
            "air": [[h, b, s] for (h, b), s in air.items()],
            "squares": list(squares), "idents": list(idents),
            "spots": [[s, m, b, h, i, n, None if best == -999 else round(best), (t - start) // 60] for (s, m, b, h, i), (n, best, t) in spots.items()],
            "tracks": tracks[:400], "ground": ground, "dsc": dsc, "monitored": monitored}


def heat_rows(labels, counts, seconds):
    return [{"label": label, "cells": [[counts.get((label, h), 0), seconds.get((label, h), 0)] for h in range(24)]} for label in labels]


def build_activity(conn, now, station_grid):
    days = 14
    since = now - days * 86400
    first = conn.execute("SELECT MIN(start_utc) FROM listening").fetchone()[0]
    b_counts, b_secs, m_counts, m_secs = {}, {}, {}, {}
    for r in conn.execute(f"SELECT mode, dial_hz, (at_utc / 3600) % 24 AS h, COUNT(*) AS n FROM observations WHERE at_utc >= ? AND {unhealthy()} GROUP BY mode, dial_hz, h", (since,)):
        m_counts[(r["mode"], r["h"])] = m_counts.get((r["mode"], r["h"]), 0) + r["n"]
        if r["mode"] in AMATEUR:
            key = (band(r["dial_hz"]), r["h"])
            b_counts[key] = b_counts.get(key, 0) + r["n"]
    for r in conn.execute("SELECT mode, dial_hz, (start_utc / 3600) % 24 AS h, SUM(end_utc - start_utc) AS s FROM listening WHERE end_utc >= ? AND healthy IS NOT 0 GROUP BY mode, dial_hz, h", (since,)):
        m_secs[(r["mode"], r["h"])] = m_secs.get((r["mode"], r["h"]), 0) + r["s"]
        if r["mode"] in AMATEUR:
            key = (band(r["dial_hz"]), r["h"])
            b_secs[key] = b_secs.get(key, 0) + r["s"]
    labels = sorted({k[0] for k in b_secs}, key=lambda b: BAND_ORDER.index(b) if b in BAND_ORDER else 99)
    mode_labels = sorted({k[0] for k in m_secs})

    daily = [dict(r) for r in conn.execute(f"""SELECT at_utc / 86400 * 86400 AS day, mode, COUNT(*) AS n, COUNT(DISTINCT ident) AS idents FROM observations
                                               WHERE at_utc >= ? GROUP BY day, mode ORDER BY day""", (now - 30 * 86400,))]
    farthest = []
    if station_grid:
        home = grid_center(station_grid)
        seen = {}
        for r in conn.execute(f"SELECT substr(grid,1,4) AS g, ident, mode, dial_hz, MAX(at_utc) AS t, MAX(snr_db) AS best FROM observations WHERE at_utc >= ? AND mode IN ({AMATEUR_SQL}) AND grid IS NOT NULL GROUP BY g, ident", (since,)):
            d = km(*home, *grid_center(r["g"]))
            if r["g"] not in seen or d > seen[r["g"]]["km"]:
                seen[r["g"]] = {"grid": r["g"].upper(), "ident": r["ident"], "mode": r["mode"], "band": band(r["dial_hz"]), "t": r["t"], "snr": r["best"], "km": round(d)}
        farthest = sorted(seen.values(), key=lambda x: -x["km"])[:15]
    new_squares = [{"grid": r["g"].upper(), "t": r["t"]} for r in conn.execute(
        f"SELECT substr(grid,1,4) AS g, MIN(at_utc) AS t FROM observations WHERE mode IN ({AMATEUR_SQL}) AND grid IS NOT NULL GROUP BY g HAVING t >= ? ORDER BY t DESC LIMIT 100", (now - 86400,))]
    selcal = [dict(r) for r in conn.execute("SELECT at_utc AS t, ident AS code, dial_hz FROM observations WHERE mode='SELCAL' ORDER BY id DESC LIMIT 100")]
    return {"generated_utc": now, "first_listening_utc": first, "days": days, "bands": heat_rows(labels, b_counts, b_secs), "modes": heat_rows(mode_labels, m_counts, m_secs),
            "daily": daily, "farthest": farthest, "new_squares": new_squares, "selcal": selcal}


def row_out(r):
    out = {"t": r["at_utc"], "m": r["mode"], "f": r["freq_hz"], "k": r["kind"], "i": r["ident"], "g": r["grid"], "s": r["snr_db"]}
    if r["lat"] is not None:
        out.update(la=round(r["lat"], 2), lo=round(r["lon"], 2))
    return out


def build_status(conn, now, station_grid):
    since = now - 86400
    by_mode = [dict(r) for r in conn.execute(f"SELECT mode, COUNT(*) AS n, COUNT(DISTINCT ident) AS idents FROM observations WHERE at_utc >= ? GROUP BY mode ORDER BY n DESC", (since,))]
    monitored = {r["mode"]: r["s"] for r in conn.execute("SELECT mode, SUM(MIN(end_utc, ?) - MAX(start_utc, ?)) AS s FROM listening WHERE end_utc >= ? AND healthy IS NOT 0 GROUP BY mode", (now, since, since))}
    bad = conn.execute("SELECT COALESCE(SUM(end_utc - start_utc), 0) FROM listening WHERE end_utc >= ? AND healthy = 0", (since,)).fetchone()[0]
    last = conn.execute("SELECT mode, dial_hz, start_utc, end_utc FROM listening ORDER BY end_utc DESC LIMIT 1").fetchone()
    latest = [row_out(r) for r in conn.execute(f"SELECT * FROM observations ORDER BY at_utc DESC, id DESC LIMIT 40")]
    lost = conn.execute("SELECT COALESCE(SUM(dropped), 0) FROM gaps WHERE at_utc >= ?", (since,)).fetchone()[0]
    ingest = meta(conn, "last_ingest_utc")
    for m in by_mode:
        m["monitored_s"] = monitored.get(m["mode"], 0)
    return {"generated_utc": now, "last_ingest_utc": int(ingest) if ingest else None, "station_grid": station_grid, "by_mode": by_mode, "monitored_s": sum(monitored.values()),
            "monitored_by_mode": monitored, "unhealthy_s": bad, "lost_decodes": lost, "last": dict(last) if last else None, "latest": latest}


def rollup(conn, now):
    """Hourly totals outlive the raw rows."""
    conn.execute("""CREATE TABLE IF NOT EXISTS hourly (hour INTEGER NOT NULL, mode TEXT NOT NULL, observations INTEGER NOT NULL, idents INTEGER NOT NULL, PRIMARY KEY (hour, mode))""")
    with conn:
        conn.execute("INSERT OR REPLACE INTO hourly SELECT at_utc / 3600 * 3600, mode, COUNT(*), COUNT(DISTINCT ident) FROM observations WHERE at_utc >= ? GROUP BY 1, 2", (now - 48 * 3600,))


def shard_of(ident):
    """Which lookup file a callsign or flight lives in. The page works this out the same way."""
    return re.sub("[^A-Z0-9]", "_", ident[:2].upper()).ljust(2, "_")


def roll_heard(conn):
    """Adds rows that arrived since the last run to the running per-station totals,
    and returns the lookup files they changed."""
    done = int(meta(conn, "heard_id") or 0)
    top = conn.execute("SELECT COALESCE(MAX(id), 0) FROM observations").fetchone()[0]
    totals = {}
    for r in conn.execute(f"""SELECT ident, mode, dial_hz, at_utc, grid, snr_db FROM observations
                              WHERE id > ? AND id <= ? AND ident IS NOT NULL AND mode IN ({AMATEUR_SQL},'HFDL','DSC') ORDER BY id""", (done, top)):
        # ACARS pads registrations with dots and sends dashes when it has none.
        ident = r["ident"].lstrip(".")
        if not re.search("[A-Z0-9]", ident):
            continue
        key = (ident, r["mode"], band(r["dial_hz"]))
        t = totals.setdefault(key, {"first": r["at_utc"], "last": 0, "n": 0, "best": None, "grid": None})
        t["first"] = min(t["first"], r["at_utc"])
        t["n"] += 1
        if r["snr_db"] is not None and (t["best"] is None or r["snr_db"] > t["best"]):
            t["best"] = r["snr_db"]
        if r["at_utc"] >= t["last"]:
            t["last"] = r["at_utc"]
            t["grid"] = r["grid"][:4].upper() if r["grid"] else t["grid"]
    with conn:
        conn.executemany("""INSERT INTO heard (ident, mode, band, shard, first_utc, last_utc, n, best, grid) VALUES (?,?,?,?,?,?,?,?,?)
                            ON CONFLICT (ident, mode, band) DO UPDATE SET
                                first_utc = min(first_utc, excluded.first_utc), last_utc = max(last_utc, excluded.last_utc), n = n + excluded.n,
                                best = NULLIF(max(COALESCE(best, -999), COALESCE(excluded.best, -999)), -999),
                                grid = CASE WHEN excluded.grid IS NOT NULL AND excluded.last_utc >= last_utc THEN excluded.grid ELSE grid END""",
                         [(i, m, b, shard_of(i), t["first"], t["last"], t["n"], t["best"], t["grid"]) for (i, m, b), t in totals.items()])
        conn.execute("INSERT OR REPLACE INTO meta (name, value) VALUES ('heard_id', ?)", (str(top),))
    return {shard_of(i) for i, _, _ in totals}


def write_lookups(conn, now, shards):
    for shard in shards:
        stations = {}
        for r in conn.execute("SELECT ident, mode, band, first_utc, last_utc, n, best, grid FROM heard WHERE shard = ?", (shard,)):
            stations.setdefault(r["ident"], []).append([r["mode"], r["band"], r["first_utc"], r["last_utc"], r["n"], None if r["best"] is None else round(r["best"]), r["grid"]])
        write(f"who/{shard}.json", {"generated_utc": now, "stations": stations})


def build_health(conn, now):
    """When the receiver listened, and what the noise and peaks looked like."""
    since = now - 86400
    timeline = [[r["start_utc"], r["end_utc"], r["mode"], r["dial_hz"], r["healthy"]] for r in conn.execute(
        "SELECT start_utc, end_utc, mode, dial_hz, healthy FROM listening WHERE end_utc >= ? ORDER BY start_utc", (since,))]
    cells, lowest = {}, {}
    for r in conn.execute("SELECT dial_hz, (start_utc / 3600) % 24 AS h, noise_dbfs, peak_iq, end_utc - start_utc AS s FROM listening WHERE end_utc >= ? AND noise_dbfs IS NOT NULL", (now - 14 * 86400,)):
        label = band(r["dial_hz"])
        lowest[label] = min(lowest.get(label, r["dial_hz"]), r["dial_hz"])
        c = cells.setdefault((label, r["h"]), {"sum": 0.0, "s": 0, "peak": 0.0})
        weight = max(r["s"], 1)
        c["sum"] += r["noise_dbfs"] * weight
        c["s"] += weight
        c["peak"] = max(c["peak"], r["peak_iq"] or 0)
    labels = sorted(lowest, key=lowest.get)
    bands = [{"label": label, "cells": [[round(cells[(label, h)]["sum"] / cells[(label, h)]["s"], 1), round(cells[(label, h)]["peak"], 3), cells[(label, h)]["s"]] if (label, h) in cells else None for h in range(24)]} for label in labels]
    return {"generated_utc": now, "since": since, "timeline": timeline, "bands": bands}


def details_of(row):
    try:
        return json.loads(row["details"] or "{}")
    except ValueError:
        return {}


def build_traffic(conn, now):
    flights = {}
    for r in conn.execute("SELECT at_utc, ident, lat, lon, details FROM observations WHERE mode = 'HFDL' AND at_utc >= ? AND ident IS NOT NULL ORDER BY at_utc", (now - 86400,)):
        ident = r["ident"].lstrip(".")
        # Ground stations announce themselves on the same channels: GS4 is no flight.
        if not re.search("[A-Z0-9]", ident) or re.fullmatch(r"GS\d+", ident):
            continue
        d = details_of(r)
        f = flights.setdefault(ident, {"i": ident, "n": 0, "t": 0})
        f["n"] += 1
        f["t"] = r["at_utc"]
        if d.get("reg"):
            f["reg"] = d["reg"].lstrip(".")
        if d.get("gsId"):
            f["gs"] = d["gsId"]
        if r["lat"] is not None:
            f["la"], f["lo"] = round(r["lat"], 2), round(r["lon"], 2)
    ships = []
    for r in conn.execute("SELECT at_utc, ident, kind, dial_hz, lat, lon, details FROM observations WHERE mode = 'DSC' AND at_utc >= ? ORDER BY at_utc DESC LIMIT 500", (now - 30 * 86400,)):
        d = details_of(r)
        ships.append({"t": r["at_utc"], "i": r["ident"], "k": r["kind"], "f": r["dial_hz"], "cat": d.get("categoryName"), "what": d.get("natureName") or d.get("tc1Name"), "from": d.get("fromKind"), "mid": d.get("fromMid"), "la": r["lat"], "lo": r["lon"]})
    return {"generated_utc": now, "flights": sorted(flights.values(), key=lambda x: -x["t"])[:500], "ships": ships}


OG_EVERY_S = 3300
BAND_COLORS = [(0, 163, 184), (225, 102, 74), (134, 92, 231), (143, 148, 155)]
HERE = os.path.dirname(os.path.abspath(__file__))


def band_group(label):
    digits = re.match(r"\d+", label)
    if not digits or not label.endswith(" m"):
        return 3
    meters = int(digits.group())
    return 0 if meters >= 40 else 1 if meters >= 17 else 2


def write_card(m, now):
    """The share card: the real stations plotted on the prepared artwork, and how many."""
    target = os.path.join(OUT, "og.png")
    if os.path.exists(target) and now - os.path.getmtime(target) < OG_EVERY_S:
        return
    with open(os.path.join(HERE, "og-assets.json")) as f:
        assets = json.load(f)
    with open(os.path.join(HERE, "og-base.bin"), "rb") as f:
        px = bytearray(zlib.decompress(f.read()))
    width, height = assets["width"], assets["height"]
    globe = assets["globe"]
    lon0, lat0 = math.radians(globe["lon0"]), math.radians(globe["lat0"])

    def to_card(lat, lon):
        p, l = math.radians(lat), math.radians(lon) - lon0
        x = math.cos(p) * math.sin(l)
        y = math.cos(lat0) * math.sin(p) - math.sin(lat0) * math.cos(p) * math.cos(l)
        z = math.sin(lat0) * math.sin(p) + math.cos(lat0) * math.cos(p) * math.cos(l)
        return globe["cx"] + globe["r"] * x, globe["cy"] - globe["r"] * y, z

    def blend(x, y, color, alpha):
        if 0 <= x < width and 0 <= y < height:
            i = (y * width + x) * 3
            for c in range(3):
                px[i + c] = int(px[i + c] + (color[c] - px[i + c]) * alpha)

    def disc(cx, cy, radius, color, alpha=1.0):
        reach = int(radius + 2)
        for y in range(int(cy) - reach, int(cy) + reach + 1):
            for x in range(int(cx) - reach, int(cx) + reach + 1):
                cover = max(0.0, min(1.0, radius + 0.5 - math.hypot(x - cx, y - cy)))
                if cover:
                    blend(x, y, color, cover * alpha)

    squares = {}
    for s, _, b, _, ident, n, _, _ in m["spots"]:
        e = squares.setdefault(s, {"ids": set(), "groups": [0, 0, 0, 0]})
        e["ids"].add(ident)
        e["groups"][band_group(m["bands"][b])] += n
    dots = []
    for s, e in squares.items():
        lat, lon = grid_center(m["squares"][s])
        x, y, z = to_card(lat, lon)
        if z > 0.03:
            dots.append((len(e["ids"]), x, y, e["groups"].index(max(e["groups"])), lat, lon))
    dots.sort(key=lambda d: d[0])

    if m["station_grid"]:
        home = grid_center(m["station_grid"])
        h1, l1 = math.radians(home[0]), math.radians(home[1])
        a = (math.cos(h1) * math.cos(l1), math.cos(h1) * math.sin(l1), math.sin(h1))
        for count, _, _, group, lat, lon in sorted(dots, key=lambda d: -d[0])[:34]:
            p2, l2 = math.radians(lat), math.radians(lon)
            b = (math.cos(p2) * math.cos(l2), math.cos(p2) * math.sin(l2), math.sin(p2))
            w = math.acos(max(-1.0, min(1.0, sum(i * j for i, j in zip(a, b)))))
            if w < 0.05:
                continue
            for k in range(0, 321):
                t = k / 320
                s1, s2 = math.sin((1 - t) * w) / math.sin(w), math.sin(t * w) / math.sin(w)
                v = [s1 * i + s2 * j for i, j in zip(a, b)]
                x, y, z = to_card(math.degrees(math.asin(max(-1.0, min(1.0, v[2])))), math.degrees(math.atan2(v[1], v[0])))
                if z > 0:
                    blend(int(x), int(y), BAND_COLORS[group], 0.2)

    for count, x, y, group, _, _ in dots:
        r = 1.6 if count < 2 else 2.4 if count < 5 else 3.4 if count < 20 else 5.0
        disc(x, y, r + 1.2, (12, 14, 17), 0.85)
        disc(x, y, r, BAND_COLORS[group])
    if m["station_grid"]:
        x, y, z = to_card(*grid_center(m["station_grid"]))
        if z > 0:
            for k in range(96):
                blend(int(x + 9 * math.cos(k / 15.28)), int(y + 9 * math.sin(k / 15.28)), (227, 230, 233), 0.95)
            disc(x, y, 2.4, (227, 230, 233))

    for stat, value in zip(assets["stats"], (len(m["idents"]), len(m["squares"]))):
        pen = stat["x"]
        for ch in f"{value:,}":
            g = assets["glyphs"][ch]
            alpha = zlib.decompress(base64.b64decode(g["a"]))
            for row in range(assets["cell_h"]):
                for col in range(g["w"]):
                    a = alpha[row * g["w"] + col]
                    if a:
                        blend(pen + col, stat["y"] + row - 8, (227, 230, 233), a / 255)
            pen += g["advance"]

    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))
    raw = b"".join(b"\0" + bytes(px[y * width * 3:(y + 1) * width * 3]) for y in range(height))
    with open(target + ".tmp", "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw, 7)) + chunk(b"IEND", b""))
    os.chmod(target + ".tmp", 0o644)
    os.replace(target + ".tmp", target)


THUMB_WIDTH = 480
THUMBS_PER_RUN = 5


def png_rows(path):
    """The rows of an 8-bit grayscale PNG, or None for any other kind."""
    with open(path, "rb") as f:
        data = f.read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        return None
    pos, idat, width, height = 8, [], 0, 0
    while pos < len(data):
        size, kind = struct.unpack(">I4s", data[pos:pos + 8])
        body = data[pos + 8:pos + 8 + size]
        pos += 12 + size
        if kind == b"IHDR":
            width, height, depth, color, _, _, interlace = struct.unpack(">IIBBBBB", body)
            if depth != 8 or color != 0 or interlace:
                return None
        elif kind == b"IDAT":
            idat.append(body)
    raw = zlib.decompress(b"".join(idat))
    stride = width + 1
    if len(raw) < stride * height:
        return None
    rows, prev = [], bytes(width)
    for y in range(height):
        kind = raw[y * stride]
        line = bytearray(raw[y * stride + 1:(y + 1) * stride])
        if kind == 1:
            for x in range(1, width):
                line[x] = (line[x] + line[x - 1]) & 255
        elif kind == 2:
            for x in range(width):
                line[x] = (line[x] + prev[x]) & 255
        elif kind == 3:
            for x in range(width):
                line[x] = (line[x] + ((line[x - 1] if x else 0) + prev[x]) // 2) & 255
        elif kind == 4:
            for x in range(width):
                a, b, c = line[x - 1] if x else 0, prev[x], prev[x - 1] if x else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                line[x] = (line[x] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        rows.append(bytes(line))
        prev = line
    return width, rows


def write_thumbnail(path, target):
    """A smaller copy starting below the black phasing band. False if it can't be made."""
    decoded = png_rows(path)
    if decoded is None:
        return False
    width, rows = decoded
    step = max(1, width // THUMB_WIDTH)
    first = 0
    for y, row in enumerate(rows[:300]):
        if sum(row[::8]) * 8 / width > 128:
            first = y
            break
    out = [b"\0" + bytes(sum(row[x:x + step]) // step for x in range(0, width - step + 1, step)) for row in rows[first::step]]
    w, h = len(out[0]) - 1, len(out)
    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))
    with open(target + ".tmp", "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 0, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(b"".join(out), 6)) + chunk(b"IEND", b""))
    os.chmod(target + ".tmp", 0o644)
    os.replace(target + ".tmp", target)
    return True


def picture_list(conn, now):
    pictures, made = [], 0
    for r in conn.execute("SELECT id, start_utc, end_utc, mode, dial_hz, width, lines, station, scheduled_title, file FROM pictures ORDER BY start_utc DESC LIMIT 300"):
        p = dict(r)
        name = f"thumb-{os.path.basename(p['file'])}"
        target = os.path.join(PICTURES, name)
        if not os.path.exists(target) and made < THUMBS_PER_RUN:
            made += 1
            try:
                write_thumbnail(os.path.join(PICTURES, os.path.basename(p["file"])), target)
            except (OSError, ValueError, IndexError, struct.error, zlib.error) as e:
                print(f"hf-site: thumbnail for {p['file']}: {e}", file=sys.stderr)
        if os.path.exists(target):
            p["thumb"] = name
        pictures.append(p)
    return {"generated_utc": now, "pictures": pictures}


def prune(conn, now):
    cutoff = now - KEEP_RAW_S
    gone = [r[0] for r in conn.execute("SELECT file FROM pictures WHERE start_utc < ?", (cutoff,))]
    with conn:
        conn.execute("DELETE FROM observations WHERE at_utc < ?", (cutoff,))
        conn.execute("DELETE FROM listening WHERE end_utc < ?", (cutoff,))
        conn.execute("DELETE FROM pictures WHERE start_utc < ?", (cutoff,))
    for name in gone:
        for victim in (os.path.basename(name), f"thumb-{os.path.basename(name)}"):
            try:
                os.remove(os.path.join(PICTURES, victim))
            except OSError:
                pass


def main():
    now = int(time.time())
    conn = connect(DB)
    rollup(conn, now)
    write_lookups(conn, now, roll_heard(conn))
    prune(conn, now)
    station_grid = meta(conn, "grid")
    write("status.json", build_status(conn, now, station_grid))
    day_map = build_map(conn, now, 24, station_grid)
    write("map-24h.json", day_map)
    try:
        write_card(day_map, now)
    except (OSError, ValueError, KeyError, zlib.error) as e:
        print(f"hf-site: share card: {e}", file=sys.stderr)
    write("map-7d.json", build_map(conn, now, 24 * 7, station_grid))
    write("activity.json", build_activity(conn, now, station_grid))
    write("log.json", {"generated_utc": now, "rows": [row_out(r) for r in conn.execute(f"SELECT * FROM observations ORDER BY at_utc DESC, id DESC LIMIT 500")]})
    write("pictures.json", picture_list(conn, now))
    write("health.json", build_health(conn, now))
    write("traffic.json", build_traffic(conn, now))
    free = shutil.disk_usage(os.path.dirname(DB)).free
    if free < LOW_DISK_BYTES:
        print(f"hf-site: low disk, {free >> 30} GB free", file=sys.stderr)


if __name__ == "__main__":
    main()
