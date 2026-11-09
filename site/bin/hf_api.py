#!/usr/bin/env python3
"""Takes batches from the receiver's uplink. Only ingest lives here: pages are
static and read JSON built by hf_warm.py, so the public never causes a query.

One request at a time on purpose. There is one sender, once a minute.
"""
import gzip
import hashlib
import hmac
import json
import os
import re
import struct
import sys
import time
import zlib
from http.server import BaseHTTPRequestHandler, HTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from hf_schema import DETAILS, HF_LIMIT_HZ, LOC_KINDS, MODES, connect  # noqa: E402

DB = os.environ.get("HF_DB", "/var/lib/hf-site/hf.sqlite")
PICTURES = os.environ.get("HF_PICTURES", "/home/patrickh/www/hf/pictures")
TOKEN_SHA256 = os.environ.get("HF_TOKEN_SHA256", "").strip().lower()
PORT = int(os.environ.get("HF_API_PORT", "8767"))

MAX_BODY = 8 << 20
MAX_JSON = 48 << 20
MAX_PNG = 16 << 20
IDENT = re.compile(r"^[A-Za-z0-9/._ -]{1,32}$")
GRID = re.compile(r"^[A-Ra-r]{2}[0-9]{2}([A-Xa-x]{2})?$")
STATION_GRID = re.compile(r"^[A-R]{2}[0-9]{2}$")
KIND = re.compile(r"^[A-Za-z0-9_-]{1,32}$")
PNG_MAGIC = b"\x89PNG\r\n\x1a\n"


class Reject(Exception):
    """A batch this backend will not take. Nothing from it is stored."""


def inflate(body):
    d = zlib.decompressobj(wbits=31)
    out = d.decompress(body, MAX_JSON + 1)
    if len(out) > MAX_JSON or d.unconsumed_tail:
        raise Reject("too large")
    return out


def integer(v, name, low=0, high=1 << 53):
    if isinstance(v, bool) or not isinstance(v, int) or not low <= v <= high:
        raise Reject(f"bad {name}")
    return v


def number(v, name, low, high):
    if v is None:
        return None
    if isinstance(v, bool) or not isinstance(v, (int, float)) or not low <= v <= high:
        raise Reject(f"bad {name}")
    return float(v)


def text(v, pattern, name):
    if v is None:
        return None
    if not isinstance(v, str) or not pattern.match(v):
        raise Reject(f"bad {name}")
    return v


def mode_and_hz(row, *keys):
    mode = row.get("mode")
    if mode not in MODES:
        raise Reject("unlisted mode")
    for key in keys:
        if integer(row.get(key), key) >= HF_LIMIT_HZ:
            raise Reject("above 30 MHz")
    return mode


def clean_details(mode, details):
    if details is None:
        return None
    if not isinstance(details, dict):
        raise Reject("bad details")
    kept = {}
    for key in DETAILS[mode] & details.keys():
        value = details[key]
        if isinstance(value, str) and len(value) <= 64 or isinstance(value, (int, float)) and not isinstance(value, bool):
            kept[key] = value
    return json.dumps(kept, separators=(",", ":")) if kept else None


def clean_observation(o):
    mode = mode_and_hz(o, "dial_hz", "freq_hz")
    if o.get("loc_kind") not in LOC_KINDS:
        raise Reject("bad loc_kind")
    return (
        integer(o.get("id"), "id", 1), integer(o.get("at_utc"), "at_utc", 1), o["dial_hz"], o["freq_hz"], mode,
        text(o.get("kind"), KIND, "kind"), text(o.get("ident"), IDENT, "ident"), text(o.get("grid"), GRID, "grid"),
        number(o.get("lat"), "lat", -90, 90), number(o.get("lon"), "lon", -180, 180), o["loc_kind"],
        number(o.get("snr_db"), "snr_db", -80, 80), clean_details(mode, o.get("details")),
    )


def clean_listening(s):
    mode = mode_and_hz(s, "dial_hz")
    healthy = s.get("healthy")
    if healthy not in (None, 0, 1):
        raise Reject("bad healthy")
    if s.get("cause") not in ("schedule", "operator"):
        raise Reject("bad cause")
    return (
        integer(s.get("id"), "id", 1), integer(s.get("start_utc"), "start_utc", 1), integer(s.get("end_utc"), "end_utc", 1),
        s["dial_hz"], mode, s["cause"], number(s.get("noise_dbfs"), "noise_dbfs", -200, 50), number(s.get("peak_iq"), "peak_iq", 0, 2),
        integer(s.get("decodes"), "decodes"), integer(s.get("valid_decodes"), "valid_decodes"), healthy,
    )


def clean_gap(g):
    return (integer(g.get("id"), "id", 1), integer(g.get("at_utc"), "at_utc", 1), text(g.get("kind"), KIND, "kind"), integer(g.get("dropped"), "dropped"))


def store_batch(conn, batch):
    """Validate everything first, so a bad row leaves nothing behind."""
    if not isinstance(batch, dict) or batch.get("station") != "hf1":
        raise Reject("bad station")
    grid = text(batch.get("grid"), STATION_GRID, "grid")
    rows = {name: batch.get(name) or [] for name in ("observations", "listening", "gaps")}
    if not all(isinstance(v, list) for v in rows.values()):
        raise Reject("bad rows")
    observations = [clean_observation(o) for o in rows["observations"]]
    listening = [clean_listening(s) for s in rows["listening"]]
    gaps = [clean_gap(g) for g in rows["gaps"]]
    with conn:
        conn.executemany("INSERT OR IGNORE INTO observations (id,at_utc,dial_hz,freq_hz,mode,kind,ident,grid,lat,lon,loc_kind,snr_db,details) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)", observations)
        conn.executemany("INSERT OR REPLACE INTO listening (id,start_utc,end_utc,dial_hz,mode,cause,noise_dbfs,peak_iq,decodes,valid_decodes,healthy) VALUES (?,?,?,?,?,?,?,?,?,?,?)", listening)
        conn.executemany("INSERT OR IGNORE INTO gaps (id,at_utc,kind,dropped) VALUES (?,?,?,?)", gaps)
        conn.execute("INSERT OR REPLACE INTO meta (name,value) VALUES ('last_ingest_utc',?)", (str(int(time.time())),))
        if grid:
            conn.execute("INSERT OR REPLACE INTO meta (name,value) VALUES ('grid',?)", (grid,))
    return {"observations": len(observations), "listening": len(listening), "gaps": len(gaps)}


def png_size(body):
    if len(body) < 26 or body[:8] != PNG_MAGIC or body[12:16] != b"IHDR":
        raise Reject("not a PNG")
    return struct.unpack(">II", body[16:24])


def store_picture(conn, meta, body, directory):
    if not isinstance(meta, dict) or meta.get("mode") != "WEFAX":
        raise Reject("unlisted mode")
    pid = integer(meta.get("id"), "id", 1)
    if integer(meta.get("dial_hz"), "dial_hz") >= HF_LIMIT_HZ:
        raise Reject("above 30 MHz")
    width, height = png_size(body)
    if width != meta.get("width") or height != meta.get("lines"):
        raise Reject("size does not match")
    station = text(meta.get("station"), re.compile(r"^[A-Z0-9]{2,8}$"), "station")
    title = meta.get("scheduled_title")
    if title is not None and (not isinstance(title, str) or len(title) > 200):
        raise Reject("bad title")
    start = integer(meta.get("start_utc"), "start_utc", 1)
    name = f"wefax-{start}-{meta['dial_hz']}.png"
    os.makedirs(directory, exist_ok=True)
    temporary = os.path.join(directory, f".{name}.tmp")
    with open(temporary, "wb") as f:
        f.write(body)
    os.chmod(temporary, 0o644)
    os.replace(temporary, os.path.join(directory, name))
    with conn:
        conn.execute("INSERT OR REPLACE INTO pictures (id,start_utc,end_utc,mode,dial_hz,width,lines,station,scheduled_title,file,bytes) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
                     (pid, start, integer(meta.get("end_utc"), "end_utc", 1), "WEFAX", meta["dial_hz"], width, height, station, title, name, len(body)))
    return {"picture": pid}


def token_ok(header):
    if not TOKEN_SHA256 or not header or not header.startswith("Bearer "):
        return False
    return hmac.compare_digest(hashlib.sha256(header[7:].encode()).hexdigest(), TOKEN_SHA256)


class Handler(BaseHTTPRequestHandler):
    timeout = 30
    server_version = "hf"
    sys_version = ""

    def log_message(self, *args):
        pass

    def reply(self, status, value):
        data = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(data)
        self.close_connection = True

    def body(self, limit):
        length = int(self.headers.get("content-length") or -1)
        if length < 0 or length > limit:
            raise Reject("too large")
        return self.rfile.read(length)

    def do_GET(self):
        self.reply(404, {"error": "not found"})

    def do_POST(self):
        if not token_ok(self.headers.get("authorization")):
            return self.reply(401, {"error": "unauthorized"})
        try:
            if self.path == "/ingest":
                body = self.body(MAX_BODY)
                data = inflate(body) if self.headers.get("content-encoding") == "gzip" else None
                if data is None:
                    raise Reject("expected gzip")
                return self.reply(200, store_batch(self.server.conn, json.loads(data)))
            if self.path == "/ingest/picture":
                meta = json.loads(self.headers.get("x-hf-picture") or "null")
                return self.reply(200, store_picture(self.server.conn, meta, self.body(MAX_PNG), PICTURES))
            return self.reply(404, {"error": "not found"})
        except (Reject, ValueError, gzip.BadGzipFile, zlib.error) as e:
            return self.reply(422, {"error": str(e)[:80]})


def main():
    if not TOKEN_SHA256:
        sys.exit("HF_TOKEN_SHA256 is not set")
    server = HTTPServer(("127.0.0.1", PORT), Handler)
    server.conn = connect(DB)
    server.serve_forever()


if __name__ == "__main__":
    main()
