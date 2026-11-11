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
import sqlite3
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
KEEP_SPECTRUM_S = 15 * 86400
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
            "daily": daily, "farthest": farthest, "new_squares": new_squares, "selcal": selcal, "navtex": navtex_messages(conn, now),
            "beacons": beacon_grid(conn, now)}


# The NCDXF/IARU beacons in transmit order, and their five frequencies. A beacon
# channel's dial sits 700 Hz below the frequency.
BEACONS = [("4U1UN", "United Nations, New York"), ("VE8AT", "Inuvik, Canada"), ("W6WX", "Mt. Umunhum, California"), ("KH6RS", "Maui, Hawaii"),
           ("ZL6B", "Masterton, New Zealand"), ("VK6RBP", "Rolystone, Australia"), ("JA2IGY", "Mt. Asama, Japan"), ("RR9O", "Novosibirsk, Russia"),
           ("VR2B", "Hong Kong"), ("4S7B", "Colombo, Sri Lanka"), ("ZS6DN", "Pretoria, South Africa"), ("5Z4B", "Kikuyu, Kenya"),
           ("4X6TU", "Tel Aviv, Israel"), ("OH2B", "Lohja, Finland"), ("CS3B", "São Jorge, Madeira"), ("LU4AA", "Buenos Aires, Argentina"),
           ("OA4B", "Lima, Peru"), ("YV5B", "Caracas, Venezuela")]
BEACON_FREQS = [14_100_000, 18_110_000, 21_150_000, 24_930_000, 28_200_000]
BEACON_PITCH_HZ = 700


def beacon_grid(conn, now):
    """Which beacons each frequency heard in the last hour, and how many of the 18
    in each of the last 24 hours, beside the time the receiver listened there.
    None until a beacon channel has listened at all in the last day."""
    day = now - 86400
    hour = now - 3600
    first_hour = now // 3600 * 3600 - 23 * 3600
    listened, by_hour = {}, {}
    for r in conn.execute("""SELECT dial_hz, start_utc, end_utc FROM listening
                             WHERE mode='BEACON' AND end_utc >= ? AND healthy IS NOT 0""", (day,)):
        f = r["dial_hz"] + BEACON_PITCH_HZ
        if f not in BEACON_FREQS:
            continue
        a, b = max(r["start_utc"], hour), min(r["end_utc"], now)
        if b > a:
            listened[f] = listened.get(f, 0) + b - a
        # Spread over the clock hours it covers.
        t = max(r["start_utc"], first_hour)
        while t < r["end_utc"]:
            h = t // 3600 * 3600
            e = min(r["end_utc"], h + 3600)
            by_hour[(f, h)] = by_hour.get((f, h), [set(), 0])
            by_hour[(f, h)][1] += e - t
            t = e
    if not by_hour:
        return None
    heard = {}
    for r in conn.execute("SELECT at_utc, ident, freq_hz, snr_db, details FROM observations WHERE mode='BEACON' AND at_utc >= ?", (first_hour,)):
        f = r["freq_hz"]
        h = r["at_utc"] // 3600 * 3600
        by_hour.setdefault((f, h), [set(), 0])[0].add(r["ident"])
        if r["at_utc"] < hour:
            continue
        steps = int(json.loads(r["details"] or "{}").get("steps", 0) or 0)
        cell = heard.setdefault((r["ident"], f), {"n": 0, "snr": None, "steps": 0, "t": 0})
        cell["n"] += 1
        if r["snr_db"] is not None and (cell["snr"] is None or r["snr_db"] > cell["snr"]):
            cell["snr"] = r["snr_db"]
        cell["steps"] = max(cell["steps"], steps)
        cell["t"] = max(cell["t"], r["at_utc"])
    return {
        "freqs": BEACON_FREQS,
        "listened": {str(f): listened.get(f, 0) for f in BEACON_FREQS},
        "beacons": [{"call": call, "place": place, "heard": {str(f): heard[(call, f)] for f in BEACON_FREQS if (call, f) in heard}} for call, place in BEACONS],
        "first_hour": first_hour,
        "hours": [[[len(by_hour.get((f, first_hour + k * 3600), [set(), 0])[0]), by_hour.get((f, first_hour + k * 3600), [set(), 0])[1]] for k in range(24)] for f in BEACON_FREQS],
    }


# The stations whose slots the receiver visits, and any others a slot catches.
NAVTEX_NAMES = {"A": "Miami", "C": "Moisie", "E": "Charleston", "F": "Boston", "G": "New Orleans", "N": "Chesapeake", "O": "St. John's",
                "Q": "Sydney, Nova Scotia", "R": "San Juan", "U": "Yarmouth", "X": "Cartwright"}


def navtex_messages(conn, now, days=7, limit=40):
    """Each NAVTEX message once, newest first. Stations repeat a message at every
    broadcast until it is canceled; the best copy is the newest complete one."""
    best = {}
    for r in conn.execute("SELECT at_utc, ident, details FROM observations WHERE mode='NAVTEX' AND at_utc >= ? ORDER BY at_utc DESC", (now - days * 86400,)):
        d = json.loads(r["details"] or "{}")
        complete = d.get("complete") == "yes"
        seen = best.get(r["ident"])
        if seen is None:
            best[r["ident"]] = {"t": r["at_utc"], "first": r["at_utc"], "n": 1, "id": r["ident"], "station": d.get("station"), "name": NAVTEX_NAMES.get(d.get("station")),
                                "subject": d.get("subjectName"), "complete": complete, "text": d.get("text", "")}
            continue
        seen["n"] += 1
        seen["first"] = r["at_utc"]
        if complete and not seen["complete"]:
            seen.update(complete=True, text=d.get("text", ""))
    return sorted(best.values(), key=lambda m: -m["t"])[:limit]


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
    # A beacon is the same eighteen stations every three minutes; they have their own grid.
    latest = [row_out(r) for r in conn.execute(f"SELECT * FROM observations WHERE mode != 'BEACON' ORDER BY at_utc DESC, id DESC LIMIT 40")]
    lost = conn.execute("SELECT COALESCE(SUM(dropped), 0) FROM gaps WHERE at_utc >= ?", (since,)).fetchone()[0]
    ingest = meta(conn, "last_ingest_utc")
    for m in by_mode:
        m["monitored_s"] = monitored.get(m["mode"], 0)
    # The receiver's own time. Wideband channels listen alongside it, so they are counted per channel only.
    receiver_s = conn.execute("SELECT COALESCE(SUM(MIN(end_utc, ?) - MAX(start_utc, ?)), 0) FROM listening WHERE end_utc >= ? AND healthy IS NOT 0 AND cause != 'wideband'", (now, since, since)).fetchone()[0]
    return {"generated_utc": now, "last_ingest_utc": int(ingest) if ingest else None, "station_grid": station_grid, "by_mode": by_mode, "monitored_s": receiver_s,
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
    timeline = [[r["start_utc"], r["end_utc"], r["mode"], r["dial_hz"], r["healthy"], r["valid_decodes"], r["cause"]] for r in conn.execute(
        "SELECT start_utc, end_utc, mode, dial_hz, healthy, valid_decodes, cause FROM listening WHERE end_utc >= ? ORDER BY start_utc", (since,))]
    judged = conn.execute("SELECT COALESCE(SUM(healthy = 1), 0) AS passed, COALESCE(SUM(healthy = 0), 0) AS failed FROM listening WHERE end_utc >= ? AND cause != 'wideband'", (since,)).fetchone()
    # What is really being decoded alongside: amateur bands by name ("FT8 80 / 40 / 30 m"),
    # everything else by frequency ("HFDL 5.65 / 6.66 / 8.91 MHz"), each once.
    alongside = {}
    for r in conn.execute("SELECT DISTINCT mode, dial_hz FROM listening WHERE cause = 'wideband' AND end_utc >= ? ORDER BY dial_hz", (since,)):
        name = band(r["dial_hz"])
        ham = name.endswith(" m") and (r["mode"] in AMATEUR or r["mode"] == "BEACON")
        names = alongside.setdefault((r["mode"], ham), [])
        label = name.removesuffix(" m") if ham else f"{r['dial_hz'] / 1e6:.2f}"
        if label not in names:
            names.append(label)
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
    return {"generated_utc": now, "since": since, "timeline": timeline, "bands": bands, "checks": {"passed": judged["passed"], "failed": judged["failed"]},
            "alongside": ", ".join(f"{mode} {' / '.join(names)} {'m' if ham else 'MHz'}" for (mode, ham), names in alongside.items())}


def details_of(row):
    try:
        return json.loads(row["details"] or "{}")
    except ValueError:
        return {}


def first_heard(conn, since):
    """Idents whose first appearance anywhere in the log is after `since`, and when the log began."""
    began = conn.execute("SELECT MIN(first_utc) FROM heard").fetchone()[0]
    new = {r[0] for r in conn.execute("SELECT ident FROM heard GROUP BY ident HAVING MIN(first_utc) >= ?", (since,))}
    return began, new


def build_traffic(conn, now):
    """Aircraft over the last day and ships over the last week, one row each."""
    _, new = first_heard(conn, now - 86400)
    flights = {}
    for r in conn.execute("SELECT at_utc, ident, lat, lon, details FROM observations WHERE mode = 'HFDL' AND at_utc >= ? AND ident IS NOT NULL ORDER BY at_utc", (now - 86400,)):
        ident = r["ident"].lstrip(".")
        # Ground stations announce themselves on the same channels: GS4 is no flight.
        if not re.search("[A-Z0-9]", ident) or re.fullmatch(r"GS\d+", ident):
            continue
        d = details_of(r)
        f = flights.setdefault(ident, {"i": ident, "n": 0, "a": r["at_utc"], "t": 0})
        f["n"] += 1
        f["t"] = r["at_utc"]
        if d.get("reg"):
            f["reg"] = d["reg"].lstrip(".")
        if d.get("gsId"):
            f["gs"] = d["gsId"]
        if r["lat"] is not None:
            f["la"], f["lo"] = round(r["lat"], 2), round(r["lon"], 2)
    for ident, f in flights.items():
        if ident in new:
            f["new"] = 1
    # One row per MMSI: most calls are routine tests, and a list of every call buries the ships.
    ships = {}
    for r in conn.execute("SELECT at_utc, ident, kind, dial_hz, lat, lon, details FROM observations WHERE mode = 'DSC' AND at_utc >= ? AND ident IS NOT NULL ORDER BY at_utc", (now - 7 * 86400,)):
        d = details_of(r)
        s = ships.setdefault(r["ident"], {"i": r["ident"], "n": 0, "a": r["at_utc"], "mhz": []})
        s["n"] += 1
        s["t"] = r["at_utc"]
        s["k"] = r["kind"]
        s["cat"] = d.get("categoryName")
        s["what"] = d.get("natureName") or d.get("tc1Name")
        if d.get("fromKind"):
            s["from"] = d["fromKind"]
        if d.get("fromMid"):
            s["mid"] = d["fromMid"]
        mhz = int(r["dial_hz"] / 1e6)
        if mhz not in s["mhz"]:
            s["mhz"].append(mhz)
        if r["lat"] is not None:
            s["la"], s["lo"], s["pt"] = round(r["lat"], 3), round(r["lon"], 3), r["at_utc"]
    for ident, s in ships.items():
        s["mhz"].sort()
        if ident in new:
            s["new"] = 1
    return {"generated_utc": now, "flights": sorted(flights.values(), key=lambda x: -x["t"])[:4000],
            "ships": sorted(ships.values(), key=lambda x: -x["t"])[:3000]}


def build_log(conn, now):
    """Every amateur station heard in the last day, one row each, and the SELCAL codes of
    the last week. Aircraft and ships come from traffic.json, which the Log page reads too."""
    since = now - 86400
    began, new = first_heard(conn, since)
    failed = conn.execute("SELECT 1 FROM listening WHERE healthy = 0 AND end_utc >= ? LIMIT 1", (since,)).fetchone()
    healthy_only = f"AND {unhealthy()}" if failed else ""
    rows = {}
    for r in conn.execute(f"""SELECT ident, mode, dial_hz, COUNT(*) AS n, MIN(at_utc) AS a, MAX(at_utc) AS b, MAX(snr_db) AS best
                              FROM observations WHERE at_utc >= ? AND mode IN ({AMATEUR_SQL}) AND ident IS NOT NULL {healthy_only}
                              GROUP BY ident, mode, dial_hz""", (since,)):
        if not re.fullmatch(r"[A-Z0-9/]+", r["ident"]):
            continue
        s = rows.setdefault(r["ident"], {"modes": 0, "bands": 0, "n": 0, "a": r["a"], "b": 0, "best": None, "grid": None})
        s["modes"] |= 1 << AMATEUR.index(r["mode"])
        label = band(r["dial_hz"])
        if label in BAND_ORDER:
            s["bands"] |= 1 << BAND_ORDER.index(label)
        s["n"] += r["n"]
        s["a"] = min(s["a"], r["a"])
        s["b"] = max(s["b"], r["b"])
        if r["best"] is not None and (s["best"] is None or r["best"] > s["best"]):
            s["best"] = r["best"]
    # The square from each station's latest report that carried one: SQLite takes the bare
    # column from the row that holds the MAX.
    for r in conn.execute(f"""SELECT ident, substr(grid,1,4) AS g, MAX(at_utc) FROM observations
                              WHERE at_utc >= ? AND mode IN ({AMATEUR_SQL}) AND ident IS NOT NULL AND grid IS NOT NULL GROUP BY ident""", (since,)):
        if r["ident"] in rows and re.fullmatch(r"[A-Ra-r]{2}\d{2}", r["g"] or ""):
            rows[r["ident"]]["grid"] = r["g"].upper()
    amateur = [[i, s["modes"], s["bands"], s["n"], s["a"], s["b"], None if s["best"] is None else round(s["best"]), s["grid"], 1 if i in new else 0]
               for i, s in sorted(rows.items(), key=lambda kv: -kv[1]["b"])]
    selcal = {}
    for r in conn.execute("SELECT ident, dial_hz, at_utc FROM observations WHERE mode = 'SELCAL' AND at_utc >= ? AND ident IS NOT NULL ORDER BY at_utc", (now - 7 * 86400,)):
        c = selcal.setdefault(r["ident"], {"code": r["ident"], "n": 0, "a": r["at_utc"], "khz": []})
        c["n"] += 1
        c["t"] = r["at_utc"]
        khz = round(r["dial_hz"] / 1000, 1)
        if khz not in c["khz"]:
            c["khz"].append(khz)
    return {"generated_utc": now, "since": since, "log_began": began, "modes": list(AMATEUR), "bands": BAND_ORDER,
            "amateur": amateur, "selcal": sorted(selcal.values(), key=lambda x: -x["t"])}


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


# The all-day spectrum: one column a minute, 2 to 30 MHz, four 10 kHz cells to a pixel.
SPECTRUM_LO_CELL = 200
SPECTRUM_HI_CELL = 3000
SPECTRUM_MERGE = 4
SPECTRUM_MINUTES = 1440
SPECTRUM_FLOOR_CHUNK = 50
SPECTRUM_QUIET_DB = 6
SPECTRUM_TOP_DB = 40
# Dark to pale along one hue, so the decodes drawn over it in mode colors stand out.
SPECTRUM_STOPS = [(30, 24, 16), (92, 56, 18), (190, 120, 30), (245, 180, 90), (255, 240, 210)]


def above_floor(data):
    """How far each cell stands over the noise near it, in dB, and None where the
    tuner was not. The floor is local, so a gain change or the filter's skirts move
    the floor and not the picture."""
    anchors = []
    for start in range(0, len(data), SPECTRUM_FLOOR_CHUNK):
        part = sorted(v for v in data[start:start + SPECTRUM_FLOOR_CHUNK] if v)
        if len(part) >= 10:
            anchors.append((start + SPECTRUM_FLOOR_CHUNK / 2, part[len(part) // 5]))
    out = [None] * len(data)
    if not anchors:
        return out
    at = 0
    for i, v in enumerate(data):
        if not v:
            continue
        while at + 1 < len(anchors) - 1 and anchors[at + 1][0] <= i:
            at += 1
        if len(anchors) == 1:
            floor = anchors[0][1]
        else:
            (x0, f0), (x1, f1) = anchors[at], anchors[at + 1]
            floor = f0 + (f1 - f0) * min(1, max(0, (i - x0) / (x1 - x0)))
        out[i] = max(0, (v - floor) / 2)
    return out


def spectrum_palette():
    """Index 0 is not covered and transparent, 1 is covered and quiet, the rest climb."""
    colors = [(0, 0, 0), SPECTRUM_STOPS[0]]
    for i in range(254):
        t = (i / 253) ** 0.8 * (len(SPECTRUM_STOPS) - 1)
        low = min(int(t), len(SPECTRUM_STOPS) - 2)
        a, b = SPECTRUM_STOPS[low], SPECTRUM_STOPS[low + 1]
        colors.append(tuple(round(a[c] + (b[c] - a[c]) * (t - low)) for c in range(3)))
    return colors


def write_indexed_png(target, width, height, px, palette):
    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))
    raw = b"".join(b"\0" + bytes(px[y * width:(y + 1) * width]) for y in range(height))
    with open(target + ".tmp", "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 3, 0, 0, 0)) + chunk(b"PLTE", b"".join(bytes(c) for c in palette))
                + chunk(b"tRNS", b"\0") + chunk(b"IDAT", zlib.compress(raw, 7)) + chunk(b"IEND", b""))
    os.chmod(target + ".tmp", 0o644)
    os.replace(target + ".tmp", target)


def build_spectrum(conn, now):
    """The last day of the span as an image, with what was decoded where."""
    last = now // 60 - 1
    first = last - SPECTRUM_MINUTES + 1
    width, height = SPECTRUM_MINUTES, (SPECTRUM_HI_CELL - SPECTRUM_LO_CELL) // SPECTRUM_MERGE
    px = bytearray(width * height)
    rows = 0
    earliest = None
    for minute, lo, data in conn.execute("SELECT minute, lo, data FROM spectrum WHERE minute BETWEEN ? AND ? ORDER BY minute", (first, last)):
        x = minute - first
        rows += 1
        earliest = minute if earliest is None else earliest
        for i, over in enumerate(above_floor(data)):
            cell = lo + i
            if over is None or not SPECTRUM_LO_CELL <= cell < SPECTRUM_HI_CELL:
                continue
            value = 1 if over < SPECTRUM_QUIET_DB else 2 + min(253, int((over - SPECTRUM_QUIET_DB) / (SPECTRUM_TOP_DB - SPECTRUM_QUIET_DB) * 253))
            at = (height - 1 - (cell - SPECTRUM_LO_CELL) // SPECTRUM_MERGE) * width + x
            if value > px[at]:
                px[at] = value
    palette = spectrum_palette()
    if rows:
        write_indexed_png(os.path.join(OUT, "spectrum.png"), width, height, px, palette)
    marks = [list(r) for r in conn.execute(f"""SELECT (at_utc / 60 - ?) / 5 * 5 AS x, freq_hz / 5000 * 5 AS khz, mode, COUNT(*) FROM observations
        WHERE at_utc >= ? AND at_utc < ? AND freq_hz BETWEEN 2000000 AND 29999999 AND {unhealthy()} GROUP BY 1, 2, 3""", (first, first * 60, (last + 1) * 60))]
    return {"generated_utc": now, "first_minute": first, "minutes": width, "height": height, "lo_hz": SPECTRUM_LO_CELL * 10_000, "hi_hz": SPECTRUM_HI_CELL * 10_000,
            "rows": rows, "earliest_minute": earliest, "quiet_db": SPECTRUM_QUIET_DB, "top_db": SPECTRUM_TOP_DB, "palette": palette, "marks": marks}


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
    """The last 30 days of pictures, an index of every month, and one file per month, so
    the page can reach back a year without ever loading it all at once."""
    pictures, made = [], 0
    for r in conn.execute("SELECT id, start_utc, end_utc, mode, dial_hz, width, lines, station, scheduled_title, file FROM pictures ORDER BY start_utc DESC"):
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
    by_month = {}
    for p in pictures:
        by_month.setdefault(time.strftime("%Y-%m", time.gmtime(p["start_utc"])), []).append(p)
    for month, items in by_month.items():
        write(f"pictures-{month}.json", {"generated_utc": now, "month": month, "pictures": items})
    # A month whose pictures have all been pruned loses its file too.
    for name in os.listdir(OUT) if os.path.isdir(OUT) else []:
        m = re.fullmatch(r"pictures-(\d{4}-\d{2})\.json", name)
        if m and m.group(1) not in by_month:
            os.remove(os.path.join(OUT, name))
    months = [{"month": m, "n": len(items), "modes": {mode: sum(1 for p in items if p["mode"] == mode) for mode in sorted({p["mode"] for p in items})}}
              for m, items in by_month.items()]
    recent = [p for p in pictures if p["start_utc"] >= now - 30 * 86400]
    return {"generated_utc": now, "since": now - 30 * 86400, "months": months, "pictures": recent}


def prune(conn, now):
    cutoff = now - KEEP_RAW_S
    gone = [r[0] for r in conn.execute("SELECT file FROM pictures WHERE start_utc < ?", (cutoff,))]
    with conn:
        conn.execute("DELETE FROM observations WHERE at_utc < ?", (cutoff,))
        conn.execute("DELETE FROM listening WHERE end_utc < ?", (cutoff,))
        conn.execute("DELETE FROM pictures WHERE start_utc < ?", (cutoff,))
        conn.execute("DELETE FROM spectrum WHERE minute < ?", ((now - KEEP_SPECTRUM_S) // 60,))
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
    write("log.json", build_log(conn, now))
    write("pictures.json", picture_list(conn, now))
    write("health.json", build_health(conn, now))
    write("traffic.json", build_traffic(conn, now))
    try:
        write("spectrum.json", build_spectrum(conn, now))
    except (OSError, ValueError, sqlite3.Error) as e:
        print(f"hf-site: spectrum: {e}", file=sys.stderr)
    free = shutil.disk_usage(os.path.dirname(DB)).free
    if free < LOW_DISK_BYTES:
        print(f"hf-site: low disk, {free >> 30} GB free", file=sys.stderr)


if __name__ == "__main__":
    main()
