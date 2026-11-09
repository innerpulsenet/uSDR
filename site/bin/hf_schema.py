"""The backend's database, and the rules for what it will store.

The daemon applies the mode and frequency limits first. These are applied again
here because this is the side the public can reach.
"""
import os
import sqlite3

MODES = {"FT8", "FT4", "WSPR", "JS8", "HFDL", "DSC", "SELCAL", "WEFAX", "SSTV"}
HF_LIMIT_HZ = 30_000_000
LOC_KINDS = {"reported", "grid4", "grid6", "ground_station", "none"}

# Default deny, by mode: a detail not named here is not stored. ACARS text and
# DSC addressees are not named, and the daemon does not send them anyway.
DETAILS = {
    "FT8": set(), "FT4": set(), "JS8": set(),
    "WSPR": {"dbm"},
    "HFDL": {"flight", "icao", "reg", "gsId", "direction", "source", "fixUtc"},
    "DSC": {"callType", "categoryName", "natureName", "tc1Name", "fromKind", "fromMid"},
    "SELCAL": {"repeat", "offsetHz"},
    "WEFAX": {"lines", "width"},
    "SSTV": {"lines", "complete"},
}

SCHEMA = """
CREATE TABLE IF NOT EXISTS observations (
    id INTEGER PRIMARY KEY, at_utc INTEGER NOT NULL, dial_hz INTEGER NOT NULL, freq_hz INTEGER NOT NULL,
    mode TEXT NOT NULL, kind TEXT NOT NULL, ident TEXT, grid TEXT, lat REAL, lon REAL,
    loc_kind TEXT NOT NULL, snr_db REAL, details TEXT
);
CREATE INDEX IF NOT EXISTS observations_at ON observations (at_utc);
CREATE INDEX IF NOT EXISTS observations_mode_at ON observations (mode, at_utc);
CREATE TABLE IF NOT EXISTS listening (
    id INTEGER PRIMARY KEY, start_utc INTEGER NOT NULL, end_utc INTEGER NOT NULL, dial_hz INTEGER NOT NULL,
    mode TEXT NOT NULL, cause TEXT NOT NULL, noise_dbfs REAL, peak_iq REAL,
    decodes INTEGER NOT NULL, valid_decodes INTEGER NOT NULL, healthy INTEGER
);
CREATE INDEX IF NOT EXISTS listening_start ON listening (start_utc);
CREATE TABLE IF NOT EXISTS gaps (id INTEGER PRIMARY KEY, at_utc INTEGER NOT NULL, kind TEXT NOT NULL, dropped INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS pictures (
    id INTEGER PRIMARY KEY, start_utc INTEGER NOT NULL, end_utc INTEGER NOT NULL, mode TEXT NOT NULL, dial_hz INTEGER NOT NULL,
    width INTEGER NOT NULL, lines INTEGER NOT NULL, station TEXT, scheduled_title TEXT, file TEXT NOT NULL, bytes INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS pictures_start ON pictures (start_utc);
CREATE TABLE IF NOT EXISTS meta (name TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS heard (
    ident TEXT NOT NULL, mode TEXT NOT NULL, band TEXT NOT NULL, shard TEXT NOT NULL, first_utc INTEGER NOT NULL,
    last_utc INTEGER NOT NULL, n INTEGER NOT NULL, best REAL, grid TEXT, PRIMARY KEY (ident, mode, band)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS heard_shard ON heard (shard);
"""


def connect(path, read_only=False):
    if read_only:
        conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=30)
    else:
        os.makedirs(os.path.dirname(path), exist_ok=True)
        conn = sqlite3.connect(path, timeout=30, check_same_thread=False)
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("PRAGMA synchronous=NORMAL")
        conn.executescript(SCHEMA)
    conn.row_factory = sqlite3.Row
    return conn
