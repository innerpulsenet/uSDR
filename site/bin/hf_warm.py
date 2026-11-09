#!/usr/bin/env python3
"""Builds the JSON the pages read. Runs on a timer at low priority, so a page
view never touches the database.

Everything here comes from the log. Time not monitored is reported as such,
and a window with no data stays empty rather than being filled in.
"""
import json
import math
import os
import shutil
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from hf_schema import connect  # noqa: E402

DB = os.environ.get("HF_DB", "/var/lib/hf-site/hf.sqlite")
OUT = os.environ.get("HF_DATA", "/home/patrickh/www/hf/data")
PICTURES = os.environ.get("HF_PICTURES", "/home/patrickh/www/hf/pictures")

AMATEUR = ("FT8", "FT4", "WSPR", "JS8")
AMATEUR_SQL = ",".join(f"'{m}'" for m in AMATEUR)
# A DSC call can be a live distress relay. This is a logbook, not a feed.
DSC_DELAY_S = 6 * 3600
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


def visible(now):
    """SQL: rows that may be shown yet."""
    return f"(mode != 'DSC' OR at_utc <= {now - DSC_DELAY_S})"


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
    os.makedirs(OUT, exist_ok=True)
    path = os.path.join(OUT, name)
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
    modes, bands, spots = [], [], {}
    def index(table, value):
        if value not in table:
            table.append(value)
        return table.index(value)

    for r in conn.execute(f"""SELECT substr(grid,1,4) AS g, mode, dial_hz, (at_utc - ?) / 3600 AS h, COUNT(*) AS n, MAX(snr_db) AS best
                              FROM observations WHERE at_utc >= ? AND mode IN ({AMATEUR_SQL}) AND grid IS NOT NULL
                              GROUP BY g, mode, dial_hz, h""", (start, since)):
        key = (r["g"].upper(), index(modes, r["mode"]), index(bands, band(r["dial_hz"])), r["h"])
        n, best = spots.get(key, (0, None))
        spots[key] = (n + r["n"], max(x for x in (best, r["best"], -999) if x is not None))

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
           for r in conn.execute("SELECT at_utc, lat, lon, ident, kind FROM observations WHERE mode='DSC' AND at_utc >= ? AND at_utc <= ? AND lat IS NOT NULL ORDER BY at_utc DESC LIMIT 400", (since, now - DSC_DELAY_S))]
    monitored = [[r["h"], index(modes, r["mode"]), r["s"]] for r in conn.execute(
        "SELECT (start_utc - ?) / 3600 AS h, mode, SUM(end_utc - start_utc) AS s FROM listening WHERE end_utc >= ? AND healthy IS NOT 0 GROUP BY h, mode", (start, since)) if r["h"] >= 0]
    return {"generated_utc": now, "start_utc": start, "hours": hours, "station_grid": station_grid, "modes": modes, "bands": bands,
            "spots": [[g, m, b, h, n, None if best == -999 else round(best)] for (g, m, b, h), (n, best) in spots.items()],
            "tracks": tracks[:400], "ground": ground, "dsc": dsc, "monitored": monitored, "dsc_delay_s": DSC_DELAY_S}


def heat_rows(labels, counts, seconds):
    return [{"label": label, "cells": [[counts.get((label, h), 0), seconds.get((label, h), 0)] for h in range(24)]} for label in labels]


def build_activity(conn, now, station_grid):
    days = 14
    since = now - days * 86400
    first = conn.execute("SELECT MIN(start_utc) FROM listening").fetchone()[0]
    b_counts, b_secs, m_counts, m_secs = {}, {}, {}, {}
    for r in conn.execute(f"SELECT mode, dial_hz, (at_utc / 3600) % 24 AS h, COUNT(*) AS n FROM observations WHERE at_utc >= ? AND {visible(now)} AND {unhealthy()} GROUP BY mode, dial_hz, h", (since,)):
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
                                               WHERE at_utc >= ? AND {visible(now)} GROUP BY day, mode ORDER BY day""", (now - 30 * 86400,))]
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
    by_mode = [dict(r) for r in conn.execute(f"SELECT mode, COUNT(*) AS n, COUNT(DISTINCT ident) AS idents FROM observations WHERE at_utc >= ? AND {visible(now)} GROUP BY mode ORDER BY n DESC", (since,))]
    monitored = {r["mode"]: r["s"] for r in conn.execute("SELECT mode, SUM(MIN(end_utc, ?) - MAX(start_utc, ?)) AS s FROM listening WHERE end_utc >= ? AND healthy IS NOT 0 GROUP BY mode", (now, since, since))}
    bad = conn.execute("SELECT COALESCE(SUM(end_utc - start_utc), 0) FROM listening WHERE end_utc >= ? AND healthy = 0", (since,)).fetchone()[0]
    last = conn.execute("SELECT mode, dial_hz, start_utc, end_utc FROM listening ORDER BY end_utc DESC LIMIT 1").fetchone()
    latest = [row_out(r) for r in conn.execute(f"SELECT * FROM observations WHERE {visible(now)} ORDER BY at_utc DESC, id DESC LIMIT 40")]
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


def prune(conn, now):
    cutoff = now - KEEP_RAW_S
    gone = [r[0] for r in conn.execute("SELECT file FROM pictures WHERE start_utc < ?", (cutoff,))]
    with conn:
        conn.execute("DELETE FROM observations WHERE at_utc < ?", (cutoff,))
        conn.execute("DELETE FROM listening WHERE end_utc < ?", (cutoff,))
        conn.execute("DELETE FROM pictures WHERE start_utc < ?", (cutoff,))
    for name in gone:
        try:
            os.remove(os.path.join(PICTURES, os.path.basename(name)))
        except OSError:
            pass


def main():
    now = int(time.time())
    conn = connect(DB)
    rollup(conn, now)
    prune(conn, now)
    station_grid = meta(conn, "grid")
    write("status.json", build_status(conn, now, station_grid))
    write("map-24h.json", build_map(conn, now, 24, station_grid))
    write("map-7d.json", build_map(conn, now, 24 * 7, station_grid))
    write("activity.json", build_activity(conn, now, station_grid))
    write("log.json", {"generated_utc": now, "rows": [row_out(r) for r in conn.execute(f"SELECT * FROM observations WHERE {visible(now)} ORDER BY at_utc DESC, id DESC LIMIT 500")]})
    write("pictures.json", {"generated_utc": now, "pictures": [dict(r) for r in conn.execute("SELECT id, start_utc, end_utc, mode, dial_hz, width, lines, station, scheduled_title, file FROM pictures ORDER BY start_utc DESC LIMIT 300")]})
    free = shutil.disk_usage(os.path.dirname(DB)).free
    if free < LOW_DISK_BYTES:
        print(f"hf-site: low disk, {free >> 30} GB free", file=sys.stderr)


if __name__ == "__main__":
    main()
