//! The station log: what the receiver heard on HF, and when it was listening.
//!
//! Every decode the server broadcasts passes through here, whether or not a
//! client is connected. Those worth keeping become rows in SQLite beside the
//! settings file: FT8, FT4, WSPR, JS8, HFDL, DSC, SELCAL and the pictures.
//! Nothing else is kept, and nothing at or above 30 MHz, so a new decoder
//! cannot reach the log until it is added here.
//!
//! The second table says when the radio was on an HF decoder. Without it an
//! empty hour could mean a dead band or a radio that was tuned elsewhere, and
//! every statistic built on the log would be wrong in the same quiet way.
//!
//! The station stays anonymous in anything meant for publication. A row
//! keeps two sets of details: `public_json` and `private_json`. Message text
//! can name the station being answered, so it goes in the private set, and
//! no route returns that set. A decode from the station's own callsign is not
//! logged at all.
//!
//! Writes happen on their own thread behind a bounded queue, so a slow disk
//! can never hold up the task that reads the broadcast. What is lost to a full
//! queue or a lagging subscriber is counted and written down as a gap.

use crate::pictures::PictureRow;
use crate::pskreporter::spot_of;
use crate::sdr::{SdrEvent, SdrMode};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use scannerd_engine::DecodeEvent;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::{self, error::RecvError};

/// The band this log covers, in hertz.
const HF_HZ: std::ops::Range<f64> = 100_000.0..30_000_000.0;
/// How long rows stay here. Long-term history belongs to the site.
const KEEP_S: u64 = 14 * 86_400;
/// The most the database may hold. The container has 3.9 GB in all.
const MAX_BYTES: i64 = 1 << 30;
/// The most disk the kept pictures may use. Oldest go first past this.
const MAX_PICTURE_BYTES: i64 = 500 << 20;
/// How long an uploaded picture stays here after it has gone.
const KEEP_UPLOADED_S: u64 = 7 * 86_400;
/// Rows removed at a time once the database is over [`MAX_BYTES`].
const PRUNE_ROWS: i64 = 5_000;
/// Decodes waiting for the writer. An FT8 slot is a few dozen.
const QUEUE: usize = 8_192;
/// The most commands applied in one transaction.
const BATCH: usize = 500;
const PRUNE_EVERY: Duration = Duration::from_secs(3_600);
const GAPS_EVERY: Duration = Duration::from_secs(60);
/// A stay on a frequency shorter than this is someone tuning past it.
const MIN_SEGMENT_S: u64 = 10;
/// How often an open segment reaches the disk, and so the most of the current
/// segment a crash can lose.
const FLUSH_S: u64 = 30;
/// No spectrum frame for this long means the radio has stopped.
const FRAME_STALE: Duration = Duration::from_secs(5);
/// How often a spectrum frame is unpacked for its noise floor and peak.
const SAMPLE_EVERY: Duration = Duration::from_secs(1);
const LIST_DEFAULT: u32 = 200;
const LIST_MAX: u32 = 1_000;
/// The longest window a summary may ask for: all this log keeps.
const SUMMARY_MAX_H: u64 = KEEP_S / 3_600;
/// `peak_iq` at or above this means the ADC clipped during the stay.
const CLIPPED: f64 = 0.98;

const SCHEMA: &str = "
CREATE TABLE observations (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    at_utc       INTEGER NOT NULL,
    dial_hz      INTEGER NOT NULL,
    freq_hz      INTEGER NOT NULL,
    mode         TEXT NOT NULL,
    kind         TEXT NOT NULL,
    ident        TEXT,
    grid         TEXT,
    lat          REAL,
    lon          REAL,
    loc_kind     TEXT NOT NULL,
    snr_db       REAL,
    public_json  TEXT,
    private_json TEXT
);
CREATE INDEX observations_at ON observations (at_utc);
CREATE INDEX observations_mode_at ON observations (mode, at_utc);
CREATE TABLE listening (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    start_utc     INTEGER NOT NULL,
    end_utc       INTEGER NOT NULL,
    dial_hz       INTEGER NOT NULL,
    mode          TEXT NOT NULL,
    cause         TEXT NOT NULL,
    noise_dbfs    REAL,
    peak_iq       REAL,
    rfgr_db       REAL,
    decodes       INTEGER NOT NULL,
    valid_decodes INTEGER NOT NULL
);
CREATE INDEX listening_start ON listening (start_utc);
CREATE TABLE gaps (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    at_utc  INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    dropped INTEGER NOT NULL
);
";

/// `healthy` is 1 after a radio check passes, 0 after one fails or when the
/// stay clipped, and empty until something has said either way.
const MIGRATE_2: &str = "ALTER TABLE listening ADD COLUMN healthy INTEGER;";

/// A row for every picture that ended, kept or not, so the rejects can be
/// counted. `file` is empty for those.
const MIGRATE_3: &str = "
CREATE TABLE pictures (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    start_utc       INTEGER NOT NULL,
    end_utc         INTEGER NOT NULL,
    mode            TEXT NOT NULL,
    dial_hz         INTEGER NOT NULL,
    width           INTEGER NOT NULL,
    lines           INTEGER NOT NULL,
    station         TEXT,
    scheduled_title TEXT,
    file            TEXT,
    bytes           INTEGER NOT NULL,
    row_corr        REAL NOT NULL,
    spread          REAL NOT NULL,
    decision        TEXT NOT NULL,
    why             TEXT NOT NULL,
    uploaded_utc    INTEGER
);
CREATE INDEX pictures_start ON pictures (start_utc);
CREATE TABLE uplink (
    name  TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
";

/// The modes that may leave this host. Default deny, so a decoder added later
/// stays home until someone decides otherwise.
/// The site backend refuses a whole batch with a mode it does not know, which would
/// stall the uplink: a mode joins here only after the site that accepts it is live.
const UPLINK_MODES: [&str; 11] = ["FT8", "FT4", "WSPR", "JS8", "HFDL", "DSC", "SELCAL", "WEFAX", "SSTV", "NAVTEX", "BEACON"];
const UPLINK_LIMIT_HZ: i64 = 30_000_000;

/// A locator as the backend takes it: four or six characters. JS8 and some FT8
/// senders give eight (`DM64pp37`), and one such row once held every batch back
/// for nine hours, so the extra precision stays in the local log. Anything that
/// is not a locator goes as none, and a grid kind follows what is left.
fn wire_grid(grid: Option<String>, loc_kind: String) -> (Option<String>, String) {
    let field = |c: u8| (b'A'..=b'R').contains(&c.to_ascii_uppercase());
    let square = |c: u8| (b'A'..=b'X').contains(&c.to_ascii_uppercase());
    let g = grid.as_deref().map(str::trim).unwrap_or("").as_bytes();
    let four = g.len() >= 4 && field(g[0]) && field(g[1]) && g[2].is_ascii_digit() && g[3].is_ascii_digit();
    let keep = if !four { 0 } else if g.len() >= 6 && square(g[4]) && square(g[5]) { 6 } else { 4 };
    let grid = (keep > 0).then(|| String::from_utf8_lossy(&g[..keep]).into_owned());
    let loc_kind = match (loc_kind.as_str(), keep) {
        ("grid4" | "grid6", 0) => "none".to_string(),
        ("grid4" | "grid6", 4) => "grid4".to_string(),
        ("grid4" | "grid6", _) => "grid6".to_string(),
        _ => loc_kind,
    };
    (grid, loc_kind)
}
/// A stay's health is settled by the next radio check, or by this long passing.
const UPLINK_SETTLE_S: u64 = 3 * 3_600;
/// A stay this recent may still be running.
const UPLINK_OPEN_S: u64 = 120;

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// How a row's location is known, so nothing is drawn more precisely than it
/// was received.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocKind {
    /// The transmitter sent these coordinates.
    Reported,
    Grid4,
    Grid6,
    /// A ground station, placed from its id in the directory.
    GroundStation,
    None,
}

impl LocKind {
    fn as_str(self) -> &'static str {
        match self {
            LocKind::Reported => "reported",
            LocKind::Grid4 => "grid4",
            LocKind::Grid6 => "grid6",
            LocKind::GroundStation => "ground_station",
            LocKind::None => "none",
        }
    }
}

/// One thing heard, ready to store.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub at_utc: u64,
    pub dial_hz: u64,
    pub freq_hz: u64,
    pub mode: &'static str,
    pub kind: String,
    pub ident: Option<String>,
    pub grid: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub loc_kind: LocKind,
    pub snr_db: Option<f64>,
    pub public: Map<String, Value>,
    pub private: Map<String, Value>,
}

fn field<'a>(ev: &'a DecodeEvent, key: &str) -> Option<&'a str> {
    ev.fields.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn number(ev: &DecodeEvent, key: &str) -> Option<f64> {
    field(ev, key)?.parse().ok().filter(|v: &f64| v.is_finite())
}

/// The named fields that are present, as a JSON object.
fn pick(ev: &DecodeEvent, keys: &[&str]) -> Map<String, Value> {
    keys.iter().filter_map(|&k| Some((k.to_string(), Value::from(field(ev, k)?)))).collect()
}

/// Coordinates a decoder reported, if they are coordinates. All zeros is what
/// an unset position looks like, not a ship off West Africa.
fn position(ev: &DecodeEvent) -> Option<(f64, f64)> {
    let (lat, lon) = (number(ev, "lat")?, number(ev, "lon")?);
    let real = (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) && (lat != 0.0 || lon != 0.0);
    real.then_some((lat, lon))
}

/// Whether a heard callsign is this station's, portable forms included.
fn same_station(heard: &str, own: &str) -> bool {
    let parts = |c: &'_ str| c.split('/').filter(|p| p.bytes().any(|b| b.is_ascii_digit())).map(str::to_ascii_uppercase).collect::<Vec<_>>();
    let own = parts(own);
    parts(heard).iter().any(|p| own.contains(p))
}

/// The row a decode makes, or `None` if it is not one this log keeps.
pub fn observe(dial_hz: f64, ev: &DecodeEvent, own_call: &str, now: u64) -> Option<Observation> {
    if !ev.valid || !HF_HZ.contains(&dial_hz) {
        return None;
    }
    let obs = match ev.protocol.as_str() {
        "FT8" | "FT4" | "WSPR" | "JS8" => amateur(dial_hz, ev, own_call)?,
        "HFDL" => hfdl(dial_hz, ev, now)?,
        "DSC" => dsc(dial_hz, ev, now)?,
        "SELCAL" => selcal(dial_hz, ev, now)?,
        "BEACON" => beacon(dial_hz, ev)?,
        "NAVTEX" if ev.kind.starts_with("message") => navtex(dial_hz, ev, now)?,
        "WEFAX" if ev.kind == "picture" => picture("WEFAX", dial_hz, ev, now, &["lines", "width"]),
        "SSTV" if ev.kind == "end" => picture("SSTV", dial_hz, ev, now, &["lines", "complete"]),
        _ => return None,
    };
    HF_HZ.contains(&(obs.freq_hz as f64)).then_some(obs)
}

fn base(dial_hz: f64, mode: &'static str, ev: &DecodeEvent, now: u64) -> Observation {
    let hz = dial_hz.round() as u64;
    Observation {
        at_utc: now,
        dial_hz: hz,
        freq_hz: hz,
        mode,
        kind: ev.kind.clone(),
        ident: None,
        grid: None,
        lat: None,
        lon: None,
        loc_kind: LocKind::None,
        snr_db: None,
        public: Map::new(),
        private: Map::new(),
    }
}

/// The amateur modes, by the same reading of their decodes that PSKReporter
/// gets: a real sending callsign, its offset and its locator if it sent one.
fn amateur(dial_hz: f64, ev: &DecodeEvent, own_call: &str) -> Option<Observation> {
    let spot = spot_of(dial_hz, ev)?;
    if same_station(&spot.call, own_call) {
        return None;
    }
    let loc_kind = match spot.grid.as_deref().map(str::len) {
        Some(4) => LocKind::Grid4,
        Some(6..) => LocKind::Grid6,
        _ => LocKind::None,
    };
    let mut private = Map::new();
    private.insert("text".into(), Value::from(ev.summary.as_str()));
    Some(Observation {
        at_utc: u64::from(spot.at),
        dial_hz: dial_hz.round() as u64,
        freq_hz: u64::from(spot.freq_hz),
        mode: spot.mode,
        kind: ev.kind.clone(),
        ident: Some(spot.call),
        grid: spot.grid,
        lat: None,
        lon: None,
        loc_kind,
        snr_db: Some(f64::from(spot.snr_db)),
        public: pick(ev, &["dbm"]),
        private,
    })
}

/// When an HFDL position was true. Aircraft queue these reports and can send
/// several at once, oldest last, so `at_utc` says when one was heard and only
/// its own time of day says when the aircraft was there. That time has no
/// date: it is the latest such time not after `now`, allowing for an aircraft
/// clock a few minutes ahead of this one.
fn fix_utc(time_of_day: &str, now: u64) -> Option<u64> {
    const DAY_S: u64 = 86_400;
    const CLOCK_AHEAD_S: u64 = 300;
    let mut parts = time_of_day.split(':').map(|p| p.parse::<u64>().ok());
    let (h, m, s) = (parts.next()??, parts.next()??, parts.next()??);
    if parts.next().is_some() || h > 23 || m > 59 || s > 59 {
        return None;
    }
    let fix = now - now % DAY_S + h * 3600 + m * 60 + s;
    if fix > now + CLOCK_AHEAD_S {
        fix.checked_sub(DAY_S)
    } else {
        Some(fix)
    }
}

fn hfdl(dial_hz: f64, ev: &DecodeEvent, now: u64) -> Option<Observation> {
    let mut o = base(dial_hz, "HFDL", ev, now);
    o.snr_db = number(ev, "snr");
    match ev.kind.as_str() {
        "position" => {
            let (lat, lon) = position(ev)?;
            (o.lat, o.lon, o.loc_kind) = (Some(lat), Some(lon), LocKind::Reported);
            o.ident = field(ev, "flight").or(field(ev, "icao")).map(String::from);
            o.public = pick(ev, &["flight", "icao", "gsId", "direction", "source"]);
            if let Some(fix) = field(ev, "utc").and_then(|t| fix_utc(t, now)) {
                o.public.insert("fixUtc".into(), Value::from(fix));
            }
        }
        "acars" => {
            if field(ev, "acarsCrcOk") != Some("true") {
                return None;
            }
            o.ident = field(ev, "flight").or(field(ev, "reg")).map(String::from);
            o.public = pick(ev, &["flight", "reg", "gsId", "direction"]);
            o.private = pick(ev, &["text"]);
        }
        "squitter" => {
            let gs = field(ev, "gsId")?;
            o.ident = Some(format!("GS{gs}"));
            o.loc_kind = LocKind::GroundStation;
            o.public = pick(ev, &["gsId"]);
        }
        _ => return None,
    }
    Some(o)
}

fn dsc(dial_hz: f64, ev: &DecodeEvent, now: u64) -> Option<Observation> {
    let mut o = base(dial_hz, "DSC", ev, now);
    o.ident = Some(field(ev, "from")?.to_string());
    if let Some((lat, lon)) = position(ev) {
        (o.lat, o.lon, o.loc_kind) = (Some(lat), Some(lon), LocKind::Reported);
    }
    o.public = pick(ev, &["callType", "categoryName", "natureName", "tc1Name", "fromKind", "fromMid"]);
    o.private = pick(ev, &["to", "distressId"]);
    o.private.insert("text".into(), Value::from(ev.summary.as_str()));
    Some(o)
}

fn selcal(dial_hz: f64, ev: &DecodeEvent, now: u64) -> Option<Observation> {
    let mut o = base(dial_hz, "SELCAL", ev, now);
    o.ident = Some(field(ev, "code")?.to_string());
    o.snr_db = number(ev, "snrDb");
    o.public = pick(ev, &["repeat", "offsetHz"]);
    Some(o)
}

/// An NCDXF/IARU beacon heard in its slot, at the beacon's own locator.
fn beacon(dial_hz: f64, ev: &DecodeEvent) -> Option<Observation> {
    let mut o = base(dial_hz, "BEACON", ev, number(ev, "slot")? as u64);
    o.freq_hz = number(ev, "freqHz")? as u64;
    o.ident = Some(field(ev, "call")?.to_string());
    o.grid = Some(field(ev, "grid")?.to_string());
    o.loc_kind = LocKind::Grid6;
    o.snr_db = number(ev, "snr");
    o.public = pick(ev, &["steps", "dash1", "dash2", "dash3", "dash4"]);
    Some(o)
}

/// The longest NAVTEX text kept. Messages run to a couple of thousand
/// characters; anything longer is a decoder that never saw the end.
const NAVTEX_TEXT_MAX: usize = 4_000;

/// A NAVTEX message, by its four-character id: station, subject, serial.
fn navtex(dial_hz: f64, ev: &DecodeEvent, now: u64) -> Option<Observation> {
    let mut o = base(dial_hz, "NAVTEX", ev, now);
    o.kind = "message".into();
    o.ident = Some(field(ev, "id")?.to_string());
    o.snr_db = number(ev, "snr");
    o.public = pick(ev, &["station", "subject", "subjectName", "serial", "complete"]);
    let text: String = ev.summary.trim().chars().take(NAVTEX_TEXT_MAX).collect();
    o.public.insert("text".into(), Value::from(text));
    Some(o)
}

fn picture(mode: &'static str, dial_hz: f64, ev: &DecodeEvent, now: u64, keys: &[&str]) -> Observation {
    let mut o = base(dial_hz, mode, ev, now);
    o.public = pick(ev, keys);
    o
}

/// The decoders whose time on the air is worth accounting for.
pub fn monitored_mode(mode: SdrMode) -> Option<&'static str> {
    Some(match mode {
        SdrMode::Ft8 => "FT8",
        SdrMode::Ft4 => "FT4",
        SdrMode::Wspr => "WSPR",
        SdrMode::Js8 => "JS8",
        SdrMode::Hfdl => "HFDL",
        SdrMode::Dsc => "DSC",
        SdrMode::Selcal => "SELCAL",
        SdrMode::Wefax => "WEFAX",
        SdrMode::Sstv => "SSTV",
        SdrMode::Navtex => "NAVTEX",
        SdrMode::Beacon => "BEACON",
        _ => return None,
    })
}

/// A stretch of time on one frequency and decoder.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    /// Identifies the stretch within this run, so a later copy updates the
    /// same row.
    pub seq: u64,
    pub start_utc: u64,
    pub end_utc: u64,
    pub dial_hz: u64,
    pub mode: &'static str,
    /// `operator` or `schedule`.
    pub cause: &'static str,
    pub noise_dbfs: Option<f64>,
    pub peak_iq: Option<f64>,
    pub rfgr_db: Option<f64>,
    pub decodes: u64,
    pub valid_decodes: u64,
}

/// What the radio is doing right now.
#[derive(Clone, Copy, Debug)]
pub struct RadioNow {
    pub dial_hz: f64,
    pub mode: SdrMode,
    pub rfgr_db: Option<f64>,
    pub cause: &'static str,
}

struct Open {
    seg: Segment,
    noise_sum: f64,
    noise_n: u32,
    flushed_at: Option<u64>,
}

impl Open {
    fn snapshot(&self) -> Segment {
        let mut s = self.seg.clone();
        s.noise_dbfs = (self.noise_n > 0).then(|| self.noise_sum / f64::from(self.noise_n));
        s
    }

    fn lasted(&self) -> u64 {
        self.seg.end_utc.saturating_sub(self.seg.start_utc)
    }
}

/// Turns the radio's state into segments. Clock-injected, like the scanner.
/// A segment ends when the frequency or decoder changes, when the radio stops,
/// and on the hour, so a question about one hour of the day has an exact answer.
#[derive(Default)]
pub struct Listener {
    open: Option<Open>,
    next_seq: u64,
}

impl Listener {
    /// A listener whose segments number from `base`, to stay clear of another's.
    pub fn numbered_from(base: u64) -> Self {
        Self { open: None, next_seq: base }
    }

    pub fn decode(&mut self, valid: bool) {
        if let Some(o) = &mut self.open {
            o.seg.decodes += 1;
            o.seg.valid_decodes += u64::from(valid);
        }
    }

    pub fn frame(&mut self, noise_dbfs: f32, peak_iq: f32) {
        let Some(o) = &mut self.open else { return };
        if noise_dbfs.is_finite() {
            o.noise_sum += f64::from(noise_dbfs);
            o.noise_n += 1;
        }
        if peak_iq.is_finite() {
            let peak = f64::from(peak_iq);
            o.seg.peak_iq = Some(o.seg.peak_iq.map_or(peak, |p| p.max(peak)));
        }
    }

    /// Advance to `now`. Returns the segments that should reach the disk.
    pub fn tick(&mut self, now: u64, radio: Option<RadioNow>, alive: bool) -> Vec<Segment> {
        let want = radio.filter(|r| alive && HF_HZ.contains(&r.dial_hz)).and_then(|r| {
            let mode = monitored_mode(r.mode)?;
            Some((r.dial_hz.round() as u64, mode, r.rfgr_db, r.cause))
        });
        let mut out = Vec::new();
        let on_same = matches!((&self.open, &want), (Some(o), Some((hz, mode, _, _))) if o.seg.dial_hz == *hz && o.seg.mode == *mode);
        if let Some(o) = &mut self.open
            && on_same
        {
            o.seg.end_utc = now;
            if let Some((_, _, gain, _)) = want {
                o.seg.rfgr_db = gain.or(o.seg.rfgr_db);
            }
            if o.seg.start_utc / 3_600 == now / 3_600 {
                let due = o.flushed_at.is_none_or(|at| now.saturating_sub(at) >= FLUSH_S);
                if o.lasted() >= MIN_SEGMENT_S && due {
                    o.flushed_at = Some(now);
                    out.push(o.snapshot());
                }
                return out;
            }
        }
        if let Some(o) = self.open.take()
            && o.lasted() >= MIN_SEGMENT_S
        {
            out.push(o.snapshot());
        }
        if let Some((dial_hz, mode, rfgr_db, cause)) = want {
            let seq = self.next_seq;
            self.next_seq += 1;
            self.open = Some(Open {
                seg: Segment { seq, start_utc: now, end_utc: now, dial_hz, mode, cause, noise_dbfs: None, peak_iq: None, rfgr_db, decodes: 0, valid_decodes: 0 },
                noise_sum: 0.0,
                noise_n: 0,
                flushed_at: None,
            });
        }
        out
    }
}

enum Cmd {
    Observe(Box<Observation>),
    Segment(Segment),
    Health { since: u64, until: u64, healthy: bool },
    Picture(Box<PictureRow>),
    PictureDecide { id: i64, publish: bool },
    Uplinked(UplinkCursors),
    PictureUploaded { id: i64, at: u64 },
}

/// How far the backend has acknowledged each table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UplinkCursors {
    pub observations: i64,
    pub listening: i64,
    pub gaps: i64,
}

/// The next rows to send. The cursors can move past rows that are not sent,
/// when the rules keep those at home.
#[derive(Debug, Default)]
pub struct UplinkBatch {
    pub observations: Vec<Value>,
    pub listening: Vec<Value>,
    pub gaps: Vec<Value>,
    pub cursors: UplinkCursors,
    /// Where the cursors stood before this batch.
    pub was: UplinkCursors,
    /// A table filled the batch, so more are waiting.
    pub full: bool,
}

impl UplinkBatch {
    pub fn is_empty(&self) -> bool {
        self.observations.is_empty() && self.listening.is_empty() && self.gaps.is_empty()
    }

    pub fn moved(&self) -> bool {
        self.cursors != self.was
    }
}

/// A picture ready to send.
pub struct UplinkPicture {
    pub id: i64,
    pub path: PathBuf,
    pub meta: Value,
}

fn json_or_null(m: &Map<String, Value>) -> Option<String> {
    (!m.is_empty()).then(|| Value::Object(m.clone()).to_string())
}

/// The writer's connection.
/// Segments that can be open together: the receiver and its channels, with room.
const OPEN_ROWS: usize = 32;

struct Store {
    conn: Connection,
    /// The rows the segments being written live in, newest last: the main
    /// receiver's and any wideband channels'.
    current: std::collections::VecDeque<(u64, i64)>,
    pictures: PathBuf,
}

fn pictures_dir(database: &Path) -> PathBuf {
    database.with_file_name("pictures")
}

impl Store {
    fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            conn.execute_batch(&format!("BEGIN; {SCHEMA} PRAGMA user_version = 1; COMMIT;"))?;
        }
        if version < 2 {
            conn.execute_batch(&format!("BEGIN; {MIGRATE_2} PRAGMA user_version = 2; COMMIT;"))?;
        }
        if version < 3 {
            conn.execute_batch(&format!("BEGIN; {MIGRATE_3} PRAGMA user_version = 3; COMMIT;"))?;
        }
        Ok(Store { conn, current: std::collections::VecDeque::new(), pictures: pictures_dir(path) })
    }

    fn apply(&mut self, cmds: &[Cmd]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        for cmd in cmds {
            match cmd {
                Cmd::Observe(o) => {
                    tx.prepare_cached(
                        "INSERT INTO observations (at_utc, dial_hz, freq_hz, mode, kind, ident, grid, lat, lon, loc_kind, snr_db, public_json, private_json)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    )?
                    .execute(params![
                        o.at_utc as i64,
                        o.dial_hz as i64,
                        o.freq_hz as i64,
                        o.mode,
                        o.kind,
                        o.ident,
                        o.grid,
                        o.lat,
                        o.lon,
                        o.loc_kind.as_str(),
                        o.snr_db,
                        json_or_null(&o.public),
                        json_or_null(&o.private),
                    ])?;
                }
                Cmd::Picture(p) => {
                    tx.prepare_cached(
                        "INSERT INTO pictures (start_utc, end_utc, mode, dial_hz, width, lines, station, scheduled_title, file, bytes, row_corr, spread, decision, why)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    )?
                    .execute(params![
                        p.start_utc as i64,
                        p.end_utc as i64,
                        p.mode,
                        p.dial_hz as i64,
                        p.width,
                        p.lines,
                        p.station,
                        p.scheduled_title,
                        p.file,
                        p.bytes as i64,
                        p.row_corr,
                        p.spread,
                        p.decision,
                        p.why,
                    ])?;
                }
                Cmd::Uplinked(c) => {
                    for (name, value) in [("observations", c.observations), ("listening", c.listening), ("gaps", c.gaps)] {
                        tx.prepare_cached("INSERT INTO uplink (name, value) VALUES (?1, ?2) ON CONFLICT (name) DO UPDATE SET value = MAX(value, excluded.value)")?.execute(params![name, value])?;
                    }
                }
                Cmd::PictureDecide { id, publish } => {
                    let (decision, why) = if *publish { ("publish", "approved") } else { ("reject", "declined") };
                    tx.prepare_cached("UPDATE pictures SET decision = ?2, why = ?3 WHERE id = ?1 AND decision = 'hold'")?.execute(params![id, decision, why])?;
                }
                Cmd::PictureUploaded { id, at } => {
                    tx.prepare_cached("UPDATE pictures SET uploaded_utc = ?2 WHERE id = ?1")?.execute(params![id, *at as i64])?;
                }
                Cmd::Health { since, until, healthy } => {
                    // A stay already marked bad for clipping stays bad.
                    tx.prepare_cached("UPDATE listening SET healthy = ?3 WHERE end_utc >= ?1 AND start_utc <= ?2 AND healthy IS NOT 0")?
                        .execute(params![*since as i64, *until as i64, i64::from(*healthy)])?;
                }
                Cmd::Segment(s) => {
                    let clipped = s.peak_iq.is_some_and(|p| p >= CLIPPED).then_some(0_i64);
                    let row = self.current.iter().find(|(seq, _)| *seq == s.seq).map(|(_, row)| *row);
                    let row = match row {
                        Some(row) => {
                            tx.prepare_cached("UPDATE listening SET end_utc = ?1, noise_dbfs = ?2, peak_iq = ?3, rfgr_db = ?4, decodes = ?5, valid_decodes = ?6, healthy = COALESCE(?8, healthy) WHERE id = ?7")?
                                .execute(params![s.end_utc as i64, s.noise_dbfs, s.peak_iq, s.rfgr_db, s.decodes as i64, s.valid_decodes as i64, row, clipped])?;
                            row
                        }
                        None => {
                            tx.prepare_cached(
                                "INSERT INTO listening (start_utc, end_utc, dial_hz, mode, cause, noise_dbfs, peak_iq, rfgr_db, decodes, valid_decodes, healthy)
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                            )?
                            .execute(params![s.start_utc as i64, s.end_utc as i64, s.dial_hz as i64, s.mode, s.cause, s.noise_dbfs, s.peak_iq, s.rfgr_db, s.decodes as i64, s.valid_decodes as i64, clipped])?;
                            tx.last_insert_rowid()
                        }
                    };
                    if !self.current.iter().any(|(seq, _)| *seq == s.seq) {
                        if self.current.len() == OPEN_ROWS {
                            self.current.pop_front();
                        }
                        self.current.push_back((s.seq, row));
                    }
                }
            }
        }
        tx.commit()
    }

    fn gap(&self, now: u64, kind: &str, dropped: u64) -> rusqlite::Result<()> {
        self.conn.execute("INSERT INTO gaps (at_utc, kind, dropped) VALUES (?1, ?2, ?3)", params![now as i64, kind, dropped as i64]).map(drop)
    }

    fn used_bytes(&self) -> rusqlite::Result<i64> {
        let pragma = |name: &str| self.conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, i64>(0));
        Ok((pragma("page_count")? - pragma("freelist_count")?) * pragma("page_size")?)
    }

    /// Drop what is past its time, then the oldest rows until it fits.
    /// Remove these pictures, rows and files together.
    fn delete_pictures(&self, select: &str, value: i64) -> rusqlite::Result<usize> {
        let doomed: Vec<(i64, Option<String>)> = self.conn.prepare(select)?.query_map([value], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        for (id, file) in &doomed {
            if let Some(file) = file {
                let _ = std::fs::remove_file(self.pictures.join(file));
            }
            self.conn.execute("DELETE FROM pictures WHERE id = ?1", [id])?;
        }
        Ok(doomed.len())
    }

    /// Rejects age out with the rest of the log, uploaded pictures a week after
    /// they went, and the oldest of whatever is left once the folder is too big.
    fn prune_pictures(&self, now: u64, keep_s: u64, max_bytes: i64) -> rusqlite::Result<()> {
        self.delete_pictures("SELECT id, file FROM pictures WHERE file IS NULL AND start_utc < ?1", now.saturating_sub(keep_s) as i64)?;
        self.delete_pictures("SELECT id, file FROM pictures WHERE uploaded_utc IS NOT NULL AND uploaded_utc < ?1", now.saturating_sub(KEEP_UPLOADED_S) as i64)?;
        loop {
            let used: i64 = self.conn.query_row("SELECT COALESCE(SUM(bytes), 0) FROM pictures", [], |r| r.get(0))?;
            if used <= max_bytes || self.delete_pictures("SELECT id, file FROM pictures WHERE file IS NOT NULL ORDER BY id LIMIT ?1", 1)? == 0 {
                return Ok(());
            }
        }
    }

    fn prune(&self, now: u64, keep_s: u64, max_bytes: i64) -> rusqlite::Result<()> {
        let cutoff = now.saturating_sub(keep_s) as i64;
        self.conn.execute("DELETE FROM observations WHERE at_utc < ?1", [cutoff])?;
        self.conn.execute("DELETE FROM listening WHERE end_utc < ?1", [cutoff])?;
        self.conn.execute("DELETE FROM gaps WHERE at_utc < ?1", [cutoff])?;
        while self.used_bytes()? > max_bytes {
            let gone = self.conn.execute("DELETE FROM observations WHERE id IN (SELECT id FROM observations ORDER BY id LIMIT ?1)", [PRUNE_ROWS])?;
            if gone == 0 {
                break;
            }
        }
        Ok(())
    }
}

/// The writer thread. Exits when the log is dropped.
fn write_loop(mut store: Store, rx: Receiver<Cmd>, counters: Arc<Counters>) {
    let mut batch = Vec::with_capacity(BATCH);
    let mut last_prune: Option<Instant> = None;
    let mut last_gaps = Instant::now();
    let (mut lost_queue, mut lost_events) = (0, 0);
    loop {
        let mut closed = false;
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(cmd) => batch.push(cmd),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => closed = true,
        }
        while batch.len() < BATCH {
            let Ok(cmd) = rx.try_recv() else { break };
            batch.push(cmd);
        }
        if !batch.is_empty() {
            if let Err(e) = store.apply(&batch) {
                eprintln!("station log: writing {} rows: {e}", batch.len());
            }
            batch.clear();
        }
        if closed {
            return;
        }
        if last_gaps.elapsed() >= GAPS_EVERY {
            last_gaps = Instant::now();
            let now = unix_now();
            for (kind, seen, counter) in [("queue", &mut lost_queue, &counters.queue_dropped), ("events", &mut lost_events, &counters.events_lagged)] {
                let total = counter.load(Ordering::Relaxed);
                if total > *seen {
                    if let Err(e) = store.gap(now, kind, total - *seen) {
                        eprintln!("station log: writing a gap: {e}");
                    }
                    *seen = total;
                }
            }
        }
        if last_prune.is_none_or(|t| t.elapsed() >= PRUNE_EVERY) {
            last_prune = Some(Instant::now());
            if let Err(e) = store.prune(unix_now(), KEEP_S, MAX_BYTES).and_then(|()| store.prune_pictures(unix_now(), KEEP_S, MAX_PICTURE_BYTES)) {
                eprintln!("station log: pruning: {e}");
            }
        }
    }
}

#[derive(Default)]
struct Counters {
    queue_dropped: AtomicU64,
    events_lagged: AtomicU64,
}

/// A list request. Everything is optional.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    pub since: Option<u64>,
    pub until: Option<u64>,
    pub mode: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SummaryQuery {
    pub hours: Option<u64>,
}

pub struct StationLog {
    tx: SyncSender<Cmd>,
    path: PathBuf,
    counters: Arc<Counters>,
}

impl StationLog {
    /// Open the database at `path`, creating it if need be, and start its writer.
    pub fn open(path: PathBuf) -> Result<Arc<Self>> {
        let store = Store::open(&path)?;
        let (tx, rx) = sync_channel(QUEUE);
        let counters = Arc::new(Counters::default());
        let writer_counters = Arc::clone(&counters);
        std::thread::Builder::new()
            .name("station-log".into())
            .spawn(move || write_loop(store, rx, writer_counters))
            .context("starting the station log writer")?;
        Ok(Arc::new(StationLog { tx, path, counters }))
    }

    fn submit(&self, cmd: Cmd) {
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) = self.tx.try_send(cmd) {
            self.counters.queue_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Where the kept pictures live, beside the database.
    pub fn pictures_dir(&self) -> PathBuf {
        pictures_dir(&self.path)
    }

    /// File what became of a finished picture.
    pub fn add_picture(&self, row: PictureRow) {
        self.submit(Cmd::Picture(Box::new(row)));
    }

    /// Record what a radio check found about the listening between two times.
    pub fn mark_health(&self, since: u64, until: u64, healthy: bool) {
        self.submit(Cmd::Health { since, until, healthy });
    }

    /// Follow the broadcast until it closes. `radio` reads the receiver's state
    /// and `own_call` the station's callsign, both as they are now.
    pub async fn run(self: Arc<Self>, mut events: broadcast::Receiver<SdrEvent>, radio: impl Fn() -> Option<RadioNow>, own_call: impl Fn() -> String) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut listener = Listener::default();
        let mut last_frame: Option<Instant> = None;
        let mut last_sample: Option<Instant> = None;
        loop {
            tokio::select! {
                ev = events.recv() => match ev {
                    Ok(SdrEvent::Decode { inspect_hz, event }) => {
                        // A wideband channel keeps its own account of its airtime.
                        if !event.fields.contains_key(crate::wideband::CHANNEL_FIELD) {
                            listener.decode(event.valid);
                        }
                        if let Some(o) = observe(inspect_hz, &event, &own_call(), unix_now()) {
                            self.submit(Cmd::Observe(Box::new(o)));
                        }
                    }
                    Ok(SdrEvent::FftBytes { frame }) => {
                        let now = Instant::now();
                        last_frame = Some(now);
                        if last_sample.is_none_or(|t| now.duration_since(t) >= SAMPLE_EVERY) {
                            last_sample = Some(now);
                            if let Ok(f) = usdr_proto::decode_fft(&frame) {
                                listener.frame(f.noise_dbfs, f.peak_iq);
                            }
                        }
                    }
                    Ok(SdrEvent::Fft { .. }) => last_frame = Some(Instant::now()),
                    Ok(_) => {}
                    Err(RecvError::Lagged(n)) => {
                        self.counters.events_lagged.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(RecvError::Closed) => return,
                },
                _ = tick.tick() => {
                    let alive = last_frame.is_some_and(|t| t.elapsed() < FRAME_STALE);
                    for s in listener.tick(unix_now(), radio(), alive) {
                        self.submit(Cmd::Segment(s));
                    }
                }
            }
        }
    }

    fn reader(&self) -> Result<Connection> {
        let conn = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).context("opening the station log to read")?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(conn)
    }

    /// Recent observations, newest first. Only the public details are returned.
    pub fn observations(&self, q: &ListQuery) -> Result<Value> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare_cached(
            "SELECT id, at_utc, dial_hz, freq_hz, mode, kind, ident, grid, lat, lon, loc_kind, snr_db, public_json FROM observations
             WHERE at_utc >= ?1 AND at_utc <= ?2 AND (?3 IS NULL OR mode = ?3) ORDER BY id DESC LIMIT ?4",
        )?;
        let (since, until, limit) = bounds(q);
        let rows = stmt
            .query_map(params![since, until, q.mode, limit], |r| {
                let public: Option<String> = r.get(12)?;
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "at_utc": r.get::<_, i64>(1)?,
                    "dial_hz": r.get::<_, i64>(2)?,
                    "freq_hz": r.get::<_, i64>(3)?,
                    "mode": r.get::<_, String>(4)?,
                    "kind": r.get::<_, String>(5)?,
                    "ident": r.get::<_, Option<String>>(6)?,
                    "grid": r.get::<_, Option<String>>(7)?,
                    "lat": r.get::<_, Option<f64>>(8)?,
                    "lon": r.get::<_, Option<f64>>(9)?,
                    "loc_kind": r.get::<_, String>(10)?,
                    "snr_db": r.get::<_, Option<f64>>(11)?,
                    "details": public.and_then(|p| serde_json::from_str::<Value>(&p).ok()),
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({ "observations": rows }))
    }

    /// When the radio was listening, newest first.
    /// What should go to the backend next. Only named columns are read, and
    /// `private_json` is not one of them.
    pub fn uplink_batch(&self, now: u64, limit: u32) -> Result<UplinkBatch> {
        let conn = self.reader()?;
        let cursor = |name: &str| -> Result<i64> { Ok(conn.query_row("SELECT value FROM uplink WHERE name = ?1", [name], |r| r.get(0)).optional()?.unwrap_or(0)) };
        let allowed = |mode: &str, dial: i64, freq: i64| UPLINK_MODES.contains(&mode) && dial < UPLINK_LIMIT_HZ && freq < UPLINK_LIMIT_HZ;
        let mut b = UplinkBatch { was: UplinkCursors { observations: cursor("observations")?, listening: cursor("listening")?, gaps: cursor("gaps")? }, ..Default::default() };
        b.cursors = b.was;

        let mut stmt = conn.prepare_cached(
            "SELECT id, at_utc, dial_hz, freq_hz, mode, kind, ident, grid, lat, lon, loc_kind, snr_db, public_json FROM observations
             WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![b.was.observations, limit])?;
        let mut seen = 0;
        while let Some(r) = rows.next()? {
            seen += 1;
            b.cursors.observations = r.get(0)?;
            let (mode, dial, freq): (String, i64, i64) = (r.get(4)?, r.get(2)?, r.get(3)?);
            if !allowed(&mode, dial, freq) {
                continue;
            }
            let details = r.get::<_, Option<String>>(12)?.and_then(|t| serde_json::from_str::<Value>(&t).ok());
            let (grid, loc_kind) = wire_grid(r.get(7)?, r.get(10)?);
            b.observations.push(json!({
                "id": b.cursors.observations, "at_utc": r.get::<_, i64>(1)?, "dial_hz": dial, "freq_hz": freq, "mode": mode,
                "kind": r.get::<_, String>(5)?, "ident": r.get::<_, Option<String>>(6)?, "grid": grid,
                "lat": r.get::<_, Option<f64>>(8)?, "lon": r.get::<_, Option<f64>>(9)?, "loc_kind": loc_kind,
                "snr_db": r.get::<_, Option<f64>>(11)?, "details": details,
            }));
        }
        b.full |= seen == limit;

        let mut stmt = conn.prepare_cached(
            "SELECT id, start_utc, end_utc, dial_hz, mode, cause, noise_dbfs, peak_iq, decodes, valid_decodes, healthy FROM listening
             WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![b.was.listening, limit])?;
        let mut seen = 0;
        while let Some(r) = rows.next()? {
            let (end, healthy): (i64, Option<i64>) = (r.get(2)?, r.get(10)?);
            let age = now.saturating_sub(end.max(0) as u64);
            // In order: a stay still running or awaiting its check holds the later ones back too.
            if age < UPLINK_OPEN_S || (healthy.is_none() && age < UPLINK_SETTLE_S) {
                break;
            }
            seen += 1;
            b.cursors.listening = r.get(0)?;
            let (mode, dial): (String, i64) = (r.get(4)?, r.get(3)?);
            if !allowed(&mode, dial, dial) {
                continue;
            }
            b.listening.push(json!({
                "id": b.cursors.listening, "start_utc": r.get::<_, i64>(1)?, "end_utc": end, "dial_hz": dial, "mode": mode,
                "cause": r.get::<_, String>(5)?, "noise_dbfs": r.get::<_, Option<f64>>(6)?, "peak_iq": r.get::<_, Option<f64>>(7)?,
                "decodes": r.get::<_, i64>(8)?, "valid_decodes": r.get::<_, i64>(9)?, "healthy": healthy,
            }));
        }
        b.full |= seen == limit;

        let mut stmt = conn.prepare_cached("SELECT id, at_utc, kind, dropped FROM gaps WHERE id > ?1 ORDER BY id LIMIT ?2")?;
        let mut rows = stmt.query(params![b.was.gaps, limit])?;
        while let Some(r) = rows.next()? {
            b.cursors.gaps = r.get(0)?;
            b.gaps.push(json!({ "id": b.cursors.gaps, "at_utc": r.get::<_, i64>(1)?, "kind": r.get::<_, String>(2)?, "dropped": r.get::<_, i64>(3)? }));
        }
        Ok(b)
    }

    pub fn uplink_advance(&self, cursors: UplinkCursors) {
        self.submit(Cmd::Uplinked(cursors));
    }

    /// The oldest picture cleared to go and not yet gone.
    pub fn next_picture(&self) -> Result<Option<UplinkPicture>> {
        let conn = self.reader()?;
        let found = conn
            .query_row(
                "SELECT id, start_utc, end_utc, mode, dial_hz, width, lines, station, scheduled_title, file FROM pictures
                 WHERE decision = 'publish' AND file IS NOT NULL AND uploaded_utc IS NULL AND dial_hz < ?1 ORDER BY id LIMIT 1",
                [UPLINK_LIMIT_HZ],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(9)?,
                        json!({
                            "id": r.get::<_, i64>(0)?, "start_utc": r.get::<_, i64>(1)?, "end_utc": r.get::<_, i64>(2)?, "mode": r.get::<_, String>(3)?,
                            "dial_hz": r.get::<_, i64>(4)?, "width": r.get::<_, i64>(5)?, "lines": r.get::<_, i64>(6)?,
                            "station": r.get::<_, Option<String>>(7)?, "scheduled_title": r.get::<_, Option<String>>(8)?,
                        }),
                    ))
                },
            )
            .optional()?;
        Ok(found.map(|(id, file, meta)| UplinkPicture { id, path: self.pictures_dir().join(file), meta }))
    }

    /// A stretch of listening reported by something other than the main receiver.
    pub fn add_segment(&self, segment: Segment) {
        self.submit(Cmd::Segment(segment));
    }

    /// Publish or decline a picture held for review. Only a held one changes.
    pub fn decide_picture(&self, id: i64, publish: bool) {
        self.submit(Cmd::PictureDecide { id, publish });
    }

    pub fn picture_uploaded(&self, id: i64, at: u64) {
        self.submit(Cmd::PictureUploaded { id, at });
    }

    /// Pictures that ended, newest first.
    pub fn pictures(&self, q: &ListQuery) -> Result<Value> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare_cached(
            "SELECT id, start_utc, end_utc, mode, dial_hz, width, lines, station, scheduled_title, file IS NOT NULL, bytes, row_corr, spread, decision, why, uploaded_utc FROM pictures
             WHERE start_utc >= ?1 AND start_utc <= ?2 AND (?3 IS NULL OR mode = ?3) ORDER BY id DESC LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![q.since.unwrap_or(0) as i64, q.until.unwrap_or(i64::MAX as u64) as i64, q.mode, q.limit.unwrap_or(LIST_DEFAULT).min(LIST_MAX)], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "start_utc": r.get::<_, i64>(1)?,
                "end_utc": r.get::<_, i64>(2)?,
                "mode": r.get::<_, String>(3)?,
                "dial_hz": r.get::<_, i64>(4)?,
                "width": r.get::<_, i64>(5)?,
                "lines": r.get::<_, i64>(6)?,
                "station": r.get::<_, Option<String>>(7)?,
                "scheduled_title": r.get::<_, Option<String>>(8)?,
                "has_file": r.get::<_, bool>(9)?,
                "bytes": r.get::<_, i64>(10)?,
                "row_corr": r.get::<_, f64>(11)?,
                "spread": r.get::<_, f64>(12)?,
                "decision": r.get::<_, String>(13)?,
                "why": r.get::<_, String>(14)?,
                "uploaded_utc": r.get::<_, Option<i64>>(15)?,
            }))
        })?;
        Ok(json!({ "pictures": rows.collect::<rusqlite::Result<Vec<_>>>()? }))
    }

    /// The PNG of a kept picture, if there is one.
    pub fn picture_png(&self, id: i64) -> Result<Option<Vec<u8>>> {
        let conn = self.reader()?;
        let file: Option<Option<String>> = conn.query_row("SELECT file FROM pictures WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        let Some(file) = file.flatten() else { return Ok(None) };
        Ok(Some(std::fs::read(self.pictures_dir().join(file)).context("reading a picture")?))
    }

    pub fn listening(&self, q: &ListQuery) -> Result<Value> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare_cached(
            "SELECT id, start_utc, end_utc, dial_hz, mode, cause, noise_dbfs, peak_iq, rfgr_db, decodes, valid_decodes FROM listening
             WHERE end_utc >= ?1 AND start_utc <= ?2 AND (?3 IS NULL OR mode = ?3) ORDER BY id DESC LIMIT ?4",
        )?;
        let (since, until, limit) = bounds(q);
        let rows = stmt
            .query_map(params![since, until, q.mode, limit], |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "start_utc": r.get::<_, i64>(1)?,
                    "end_utc": r.get::<_, i64>(2)?,
                    "dial_hz": r.get::<_, i64>(3)?,
                    "mode": r.get::<_, String>(4)?,
                    "cause": r.get::<_, String>(5)?,
                    "noise_dbfs": r.get::<_, Option<f64>>(6)?,
                    "peak_iq": r.get::<_, Option<f64>>(7)?,
                    "rfgr_db": r.get::<_, Option<f64>>(8)?,
                    "decodes": r.get::<_, i64>(9)?,
                    "valid_decodes": r.get::<_, i64>(10)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({ "listening": rows }))
    }

    /// What has been measured on a working radio, by mode, frequency and hour
    /// of the day, for choosing where to listen.
    pub fn yields(&self) -> Result<crate::schedule::Yields> {
        let conn = self.reader()?;
        let mut out = crate::schedule::Yields::new();
        let mut stmt = conn.prepare_cached("SELECT mode, dial_hz, (start_utc / 3600) % 24, SUM(end_utc - start_utc) FROM listening WHERE healthy IS NOT 0 GROUP BY 1, 2, 3")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)))?;
        for row in rows {
            let (mode, dial, hour, seconds) = row?;
            out.entry((mode, dial as u64, hour as u8)).or_default().monitored_s = seconds as u64;
        }
        let mut stmt = conn.prepare_cached("SELECT mode, dial_hz, (at_utc / 3600) % 24, COUNT(*) FROM observations GROUP BY 1, 2, 3")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)))?;
        for row in rows {
            let (mode, dial, hour, n) = row?;
            out.entry((mode, dial as u64, hour as u8)).or_default().observations = n as u64;
        }
        Ok(out)
    }

    /// Totals over the last `hours`, by mode and by frequency, each with the
    /// time actually spent listening there.
    pub fn summary(&self, q: &SummaryQuery, now: u64) -> Result<Value> {
        let conn = self.reader()?;
        let hours = q.hours.unwrap_or(24).clamp(1, SUMMARY_MAX_H);
        let (since, until) = (now.saturating_sub(hours * 3_600) as i64, now as i64);

        #[derive(Default)]
        struct Row {
            observations: i64,
            monitored_s: i64,
            decodes: i64,
            valid_decodes: i64,
        }
        let mut dials: BTreeMap<(String, i64), Row> = BTreeMap::new();
        let mut stmt = conn.prepare_cached("SELECT mode, dial_hz, COUNT(*) FROM observations WHERE at_utc >= ?1 AND at_utc <= ?2 GROUP BY mode, dial_hz")?;
        let heard = stmt.query_map([since, until], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?;
        for row in heard {
            let (mode, dial, n) = row?;
            dials.entry((mode, dial)).or_default().observations = n;
        }
        let mut stmt = conn.prepare_cached(
            "SELECT mode, dial_hz, SUM(MIN(end_utc, ?2) - MAX(start_utc, ?1)), SUM(decodes), SUM(valid_decodes) FROM listening
             WHERE end_utc > ?1 AND start_utc < ?2 GROUP BY mode, dial_hz",
        )?;
        let listened = stmt.query_map([since, until], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?)))?;
        for row in listened {
            let (mode, dial, seconds, decodes, valid) = row?;
            let e = dials.entry((mode, dial)).or_default();
            (e.monitored_s, e.decodes, e.valid_decodes) = (seconds, decodes, valid);
        }

        let mut stmt = conn.prepare_cached("SELECT mode, COUNT(DISTINCT ident) FROM observations WHERE at_utc >= ?1 AND at_utc <= ?2 GROUP BY mode")?;
        let stations: BTreeMap<String, i64> = stmt.query_map([since, until], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;

        let mut modes: BTreeMap<&str, Row> = BTreeMap::new();
        for ((mode, _), r) in &dials {
            let e = modes.entry(mode).or_default();
            e.observations += r.observations;
            e.monitored_s += r.monitored_s;
            e.decodes += r.decodes;
            e.valid_decodes += r.valid_decodes;
        }
        let by_mode: Vec<Value> = modes
            .iter()
            .map(|(mode, r)| json!({ "mode": mode, "observations": r.observations, "idents": stations.get(*mode).copied().unwrap_or(0), "monitored_s": r.monitored_s, "decodes": r.decodes, "valid_decodes": r.valid_decodes }))
            .collect();
        let by_dial: Vec<Value> = dials
            .iter()
            .map(|((mode, dial), r)| json!({ "mode": mode, "dial_hz": dial, "observations": r.observations, "monitored_s": r.monitored_s, "decodes": r.decodes, "valid_decodes": r.valid_decodes }))
            .collect();
        let page: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let pages: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        Ok(json!({
            "since_utc": since,
            "until_utc": until,
            "hours": hours,
            "total_observations": by_mode.iter().filter_map(|m| m["observations"].as_i64()).sum::<i64>(),
            "monitored_s": modes.values().map(|r| r.monitored_s).sum::<i64>(),
            "by_mode": by_mode,
            "by_dial": by_dial,
            "drops": {
                "queue": self.counters.queue_dropped.load(Ordering::Relaxed),
                "events": self.counters.events_lagged.load(Ordering::Relaxed),
            },
            "db_bytes": page * pages,
        }))
    }
}

fn bounds(q: &ListQuery) -> (i64, i64, u32) {
    let since = q.since.unwrap_or(0).min(i64::MAX as u64) as i64;
    let until = q.until.unwrap_or(i64::MAX as u64).min(i64::MAX as u64) as i64;
    (since, until, q.limit.unwrap_or(LIST_DEFAULT).clamp(1, LIST_MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const OWN: &str = "W1AW";
    const NOW: u64 = 1_800_000_000;

    fn event(protocol: &str, kind: &str, fields: &[(&str, &str)]) -> DecodeEvent {
        DecodeEvent {
            protocol: protocol.into(),
            kind: kind.into(),
            summary: "raw message text".into(),
            valid: true,
            fields: fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>(),
        }
    }

    fn ft8(de: &str, to: &str, grid: Option<&str>) -> DecodeEvent {
        let mut fields = vec![("de", de), ("to", to), ("df", "1200"), ("snr", "-7"), ("slotMs", "1800000015000")];
        if let Some(g) = grid {
            fields.push(("grid", g));
        }
        event("FT8", "CQ", &fields)
    }

    fn radio(dial_hz: f64, mode: SdrMode) -> Option<RadioNow> {
        Some(RadioNow { dial_hz, mode, rfgr_db: None, cause: "operator" })
    }

    fn temp_db(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("usdr-stationlog-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("station.sqlite")
    }

    fn obs_row(ident: &str) -> Observation {
        observe(14_074_000.0, &ft8(ident, "K1ABC", Some("FN31")), OWN, NOW).expect("an observation")
    }

    fn segment(seq: u64, start: u64, end: u64) -> Segment {
        Segment { seq, start_utc: start, end_utc: end, dial_hz: 14_074_000, mode: "FT8", cause: "operator", noise_dbfs: Some(-90.0), peak_iq: Some(0.2), rfgr_db: None, decodes: 5, valid_decodes: 5 }
    }

    /// A log whose writer has been shut down, so everything has reached disk.
    fn flushed(path: PathBuf, cmds: Vec<Cmd>) -> StationLog {
        let mut store = Store::open(&path).unwrap();
        store.apply(&cmds).unwrap();
        let (tx, _rx) = sync_channel(1);
        StationLog { tx, path, counters: Arc::new(Counters::default()) }
    }

    #[test]
    fn an_ft8_spot_keeps_the_sender_and_its_square() {
        let o = observe(14_074_000.0, &ft8("K1ABC", "CQ", Some("FN31")), OWN, NOW).unwrap();
        assert_eq!(o.ident.as_deref(), Some("K1ABC"));
        assert_eq!(o.grid.as_deref(), Some("FN31"));
        assert_eq!(o.loc_kind, LocKind::Grid4);
        assert_eq!(o.freq_hz, 14_075_200);
        assert_eq!(o.at_utc, 1_800_000_015);
        assert_eq!(o.snr_db, Some(-7.0));
    }

    #[test]
    fn the_station_never_logs_itself() {
        assert!(observe(14_074_000.0, &ft8("W1AW", "CQ", None), OWN, NOW).is_none());
        assert!(observe(14_074_000.0, &ft8("W1AW/P", "CQ", None), OWN, NOW).is_none());
        assert!(observe(14_074_000.0, &ft8("VE3/W1AW", "CQ", None), OWN, NOW).is_none());
        assert!(observe(14_074_000.0, &ft8("K1ABC", "CQ", None), OWN, NOW).is_some());
    }

    #[test]
    fn a_message_naming_the_station_stays_private() {
        let o = observe(14_074_000.0, &ft8("K1ABC", OWN, None), OWN, NOW).unwrap();
        let public = Value::Object(o.public.clone()).to_string();
        assert!(!public.contains(OWN), "{public}");
        assert!(!public.contains("raw message text"), "{public}");
        assert_eq!(o.private["text"], "raw message text");
    }

    #[test]
    fn only_listed_protocols_below_30_mhz_are_kept() {
        for protocol in ["POCSAG", "FLEX", "P25", "DMR", "NXDN", "AIS", "RTTY"] {
            assert!(observe(14_074_000.0, &event(protocol, "text", &[("de", "K1ABC"), ("df", "100")]), OWN, NOW).is_none(), "{protocol}");
        }
        assert!(observe(50_313_000.0, &ft8("K1ABC", "CQ", None), OWN, NOW).is_none());
        assert!(observe(29_999_000.0, &ft8("K1ABC", "CQ", None), OWN, NOW).is_none(), "the audio offset carries it past 30 MHz");
        let mut bad = ft8("K1ABC", "CQ", None);
        bad.valid = false;
        assert!(observe(14_074_000.0, &bad, OWN, NOW).is_none());
    }

    #[test]
    fn an_unresolved_hash_is_not_a_station() {
        assert!(observe(14_074_000.0, &ft8("<...>", "CQ", None), OWN, NOW).is_none());
    }

    #[test]
    fn an_hfdl_position_is_reported_and_its_acars_text_is_not_public() {
        let p = event("HFDL", "position", &[("flight", " BA117 "), ("lat", "51.5"), ("lon", "-30.25"), ("gsId", "3"), ("snr", "11.5")]);
        let o = observe(11_387_000.0, &p, OWN, NOW).unwrap();
        assert_eq!((o.ident.as_deref(), o.lat, o.lon, o.loc_kind), (Some("BA117"), Some(51.5), Some(-30.25), LocKind::Reported));
        assert_eq!(o.snr_db, Some(11.5));

        let a = event("HFDL", "acars", &[("flight", "BA117"), ("reg", "G-XWBA"), ("text", "CREW NOTE"), ("acarsCrcOk", "true")]);
        let o = observe(11_387_000.0, &a, OWN, NOW).unwrap();
        assert!(!Value::Object(o.public.clone()).to_string().contains("CREW"));
        assert_eq!(o.private["text"], "CREW NOTE");

        let corrupt = event("HFDL", "acars", &[("flight", "BA117"), ("text", "x"), ("acarsCrcOk", "false")]);
        assert!(observe(11_387_000.0, &corrupt, OWN, NOW).is_none());
    }

    #[test]
    fn an_hfdl_fix_keeps_its_own_time_beside_the_time_it_was_heard() {
        let noon = NOW - NOW % 86_400 + 12 * 3600;
        let fix = |utc: &str, now| {
            let p = event("HFDL", "position", &[("flight", "BA117"), ("lat", "51.5"), ("lon", "-30.25"), ("utc", utc)]);
            let o = observe(11_387_000.0, &p, OWN, now).unwrap();
            assert_eq!(o.at_utc, now, "the time heard stays the time heard");
            o.public.get("fixUtc").and_then(Value::as_u64)
        };
        // A queued report sent long after it was true.
        assert_eq!(fix("10:30:00", noon), Some(noon - 90 * 60));
        // Heard just after midnight: the 23:59 fix was yesterday's.
        let midnight = noon + 12 * 3600;
        assert_eq!(fix("23:59:50", midnight + 20), Some(midnight - 10));
        // An aircraft clock slightly ahead of ours is not yesterday.
        assert_eq!(fix("12:02:00", noon), Some(noon + 120));
        // Nothing usable, nothing claimed; the position still counts.
        assert_eq!(fix("99:00:00", noon), None);
        assert_eq!(fix("noon", noon), None);
    }

    #[test]
    fn a_squitter_names_its_ground_station_and_a_blank_position_is_dropped() {
        let o = observe(11_387_000.0, &event("HFDL", "squitter", &[("gsId", "4")]), OWN, NOW).unwrap();
        assert_eq!((o.ident.as_deref(), o.loc_kind), (Some("GS4"), LocKind::GroundStation));
        let blank = event("HFDL", "position", &[("flight", "X1"), ("lat", "0.0"), ("lon", "0.0")]);
        assert!(observe(11_387_000.0, &blank, OWN, NOW).is_none());
        let impossible = event("HFDL", "position", &[("flight", "X1"), ("lat", "120"), ("lon", "10")]);
        assert!(observe(11_387_000.0, &impossible, OWN, NOW).is_none());
    }

    #[test]
    fn a_dsc_call_keeps_its_position_and_hides_the_addressee() {
        let e = event("DSC", "distress", &[("from", "366123456"), ("to", "all"), ("lat", "40.1"), ("lon", "-70.2"), ("natureName", "fire")]);
        let o = observe(8_414_500.0, &e, OWN, NOW).unwrap();
        assert_eq!((o.ident.as_deref(), o.loc_kind), (Some("366123456"), LocKind::Reported));
        assert_eq!(o.public["natureName"], "fire");
        assert!(!o.public.contains_key("to"));
        let nowhere = event("DSC", "routine", &[("from", "366123456")]);
        assert_eq!(observe(8_414_500.0, &nowhere, OWN, NOW).unwrap().loc_kind, LocKind::None);
    }

    #[test]
    fn selcal_has_a_code_and_no_location() {
        let o = observe(5_598_000.0, &event("SELCAL", "call", &[("code", "AB-CD"), ("snrDb", "9.5"), ("repeat", "false")]), OWN, NOW).unwrap();
        assert_eq!((o.ident.as_deref(), o.loc_kind, o.snr_db), (Some("AB-CD"), LocKind::None, Some(9.5)));
    }

    #[test]
    fn a_navtex_message_keeps_its_id_and_text() {
        let fields = [("id", "FA12"), ("station", "F"), ("subject", "A"), ("subjectName", "Navigational warning"), ("serial", "12"), ("complete", "yes"), ("snr", "14")];
        let mut ev = event("NAVTEX", "message", &fields);
        ev.summary = "ZCZC FA12\nBOSTON\nNNNN".into();
        let o = observe(517_000.0, &ev, OWN, NOW).unwrap();
        assert_eq!((o.ident.as_deref(), o.kind.as_str(), o.snr_db), (Some("FA12"), "message", Some(14.0)));
        assert_eq!(o.public["text"], "ZCZC FA12\nBOSTON\nNNNN");
        ev.kind = "message (cut off)".into();
        let cut = observe(517_000.0, &ev, OWN, NOW).unwrap();
        assert_eq!(cut.kind, "message", "the site takes no spaces in a kind; `complete` says it");
        ev.kind = "text".into();
        assert!(observe(517_000.0, &ev, OWN, NOW).is_none(), "loose text is not a message");
    }

    #[test]
    fn only_a_finished_picture_counts() {
        assert!(observe(13_882_500.0, &event("WEFAX", "picture", &[("lines", "1200")]), OWN, NOW).is_some());
        assert!(observe(13_882_500.0, &event("WEFAX", "start", &[]), OWN, NOW).is_none());
        assert!(observe(14_230_000.0, &event("SSTV", "end", &[("lines", "256")]), OWN, NOW).is_some());
        assert!(observe(14_230_000.0, &event("SSTV", "start", &[]), OWN, NOW).is_none());
    }

    #[test]
    fn tuning_past_a_frequency_leaves_no_segment() {
        let mut l = Listener::default();
        assert!(l.tick(NOW, radio(14_074_000.0, SdrMode::Ft8), true).is_empty());
        assert!(l.tick(NOW + 3, radio(14_075_000.0, SdrMode::Ft8), true).is_empty());
        assert!(l.tick(NOW + 6, radio(14_076_000.0, SdrMode::Ft8), true).is_empty());
    }

    #[test]
    fn a_stay_is_written_as_it_goes_and_closes_on_a_hop() {
        let t = NOW - NOW % 3_600;
        let mut l = Listener::default();
        l.tick(t, radio(14_074_000.0, SdrMode::Ft8), true);
        l.frame(-92.0, 0.1);
        l.frame(-90.0, 0.4);
        l.decode(true);
        l.decode(false);
        let first = l.tick(t + 12, radio(14_074_000.0, SdrMode::Ft8), true);
        assert_eq!(first.len(), 1, "written once it has lasted long enough");
        assert_eq!((first[0].decodes, first[0].valid_decodes), (2, 1));
        assert_eq!((first[0].noise_dbfs, first[0].peak_iq), (Some(-91.0), Some(f64::from(0.4_f32))));
        assert!(l.tick(t + 20, radio(14_074_000.0, SdrMode::Ft8), true).is_empty(), "not again until the next flush");
        assert_eq!(l.tick(t + 42, radio(14_074_000.0, SdrMode::Ft8), true).len(), 1);

        let closed = l.tick(t + 50, radio(10_136_000.0, SdrMode::Ft8), true);
        assert_eq!(closed.len(), 1);
        assert_eq!((closed[0].seq, closed[0].end_utc), (first[0].seq, t + 42));
    }

    #[test]
    fn a_segment_ends_on_the_hour() {
        let t = NOW - NOW % 3_600;
        let mut l = Listener::default();
        l.tick(t + 3_500, radio(14_074_000.0, SdrMode::Ft8), true);
        let rolled = l.tick(t + 3_601, radio(14_074_000.0, SdrMode::Ft8), true);
        assert_eq!(rolled.last().unwrap().end_utc, t + 3_601);
        let next = l.tick(t + 3_615, radio(14_074_000.0, SdrMode::Ft8), true);
        assert_eq!(next[0].start_utc, t + 3_601);
        assert_ne!(next[0].seq, rolled.last().unwrap().seq);
    }

    #[test]
    fn other_modes_and_a_stopped_radio_are_not_listening() {
        let t = NOW - NOW % 3_600;
        let mut l = Listener::default();
        assert!(l.tick(t, radio(100_500_000.0, SdrMode::Wfm), true).is_empty());
        assert!(l.open.is_none());
        l.tick(t, radio(146_520_000.0, SdrMode::Ft8), true);
        assert!(l.open.is_none(), "an HF decoder on a VHF frequency is not HF monitoring");
        l.tick(t, radio(14_074_000.0, SdrMode::Usb), true);
        assert!(l.open.is_none());

        l.tick(t, radio(14_074_000.0, SdrMode::Ft8), true);
        l.tick(t + 20, radio(14_074_000.0, SdrMode::Ft8), true);
        let closed = l.tick(t + 21, radio(14_074_000.0, SdrMode::Ft8), false);
        assert_eq!(closed.len(), 1);
        assert!(l.open.is_none());
    }

    #[test]
    fn rows_survive_a_restart_and_a_segment_is_updated_in_place() {
        let path = temp_db("restart");
        {
            let mut store = Store::open(&path).unwrap();
            store.apply(&[Cmd::Observe(Box::new(obs_row("K1ABC"))), Cmd::Segment(segment(0, NOW, NOW + 20))]).unwrap();
            store.apply(&[Cmd::Segment(segment(0, NOW, NOW + 50))]).unwrap();
        }
        let log = flushed(path, vec![Cmd::Observe(Box::new(obs_row("K2DEF")))]);
        let got = log.observations(&ListQuery::default()).unwrap();
        assert_eq!(got["observations"].as_array().unwrap().len(), 2);
        let listening = log.listening(&ListQuery::default()).unwrap();
        let rows = listening["listening"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["end_utc"], NOW + 50);
    }

    #[test]
    fn no_route_returns_the_private_details() {
        let log = flushed(temp_db("private"), vec![Cmd::Observe(Box::new(observe(14_074_000.0, &ft8("K1ABC", OWN, Some("FN31")), OWN, NOW).unwrap()))]);
        let text = log.observations(&ListQuery::default()).unwrap().to_string();
        assert!(!text.contains(OWN), "{text}");
        assert!(!text.contains("raw message text"), "{text}");
    }

    #[test]
    fn a_summary_counts_only_time_inside_its_window() {
        let log = flushed(temp_db("summary"), vec![Cmd::Segment(segment(0, NOW - 4_000, NOW - 3_000)), Cmd::Observe(Box::new(obs_row("K1ABC")))]);
        // Observation time comes from its slot, which is at NOW + 15 here.
        let s = log.summary(&SummaryQuery { hours: Some(1) }, NOW + 100).unwrap();
        assert_eq!(s["monitored_s"], 500);
        assert_eq!(s["total_observations"], 1);
        assert_eq!(s["by_mode"][0]["mode"], "FT8");
        assert_eq!(s["by_mode"][0]["idents"], 1);
        assert_eq!(s["by_dial"][0]["dial_hz"], 14_074_000);
    }

    fn picture_row(start: u64, file: Option<&str>, bytes: u64) -> PictureRow {
        PictureRow {
            start_utc: start,
            end_utc: start + 600,
            mode: "WEFAX",
            dial_hz: 8_501_100,
            width: 1809,
            lines: 1338,
            station: Some("NMG".into()),
            scheduled_title: Some("Surface analysis".into()),
            file: file.map(String::from),
            bytes,
            row_corr: 0.8,
            spread: 0.4,
            decision: if file.is_some() { "publish" } else { "reject" },
            why: "",
        }
    }

    #[test]
    fn a_picture_row_lists_without_its_path_and_pruning_takes_its_file() {
        let path = temp_db("pictures");
        let mut store = Store::open(&path).unwrap();
        let dir = pictures_dir(&path);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["old.png", "new.png"] {
            std::fs::write(dir.join(name), b"png").unwrap();
        }
        let cmds = [Cmd::Picture(Box::new(picture_row(NOW - 100, Some("old.png"), 60))), Cmd::Picture(Box::new(picture_row(NOW - 50, Some("new.png"), 60))), Cmd::Picture(Box::new(picture_row(NOW - KEEP_S - 1, None, 0)))];
        store.apply(&cmds).unwrap();

        let (tx, _rx) = sync_channel(1);
        let log = StationLog { tx, path: path.clone(), counters: Arc::default() };
        let listed = log.pictures(&ListQuery::default()).unwrap();
        assert_eq!(listed["pictures"].as_array().unwrap().len(), 3);
        assert_eq!(listed["pictures"][1]["has_file"], true);
        assert!(listed["pictures"][1].get("file").is_none(), "the folder layout stays private");
        let id = listed["pictures"][1]["id"].as_i64().unwrap();
        assert_eq!(log.picture_png(id).unwrap().as_deref(), Some(&b"png"[..]));
        assert_eq!(log.picture_png(9_999).unwrap(), None);

        store.prune_pictures(NOW, KEEP_S, 100).unwrap();
        let left = log.pictures(&ListQuery::default()).unwrap();
        let left = left["pictures"].as_array().unwrap();
        assert_eq!(left.len(), 1, "the old reject and the oldest file went");
        assert_eq!(left[0]["start_utc"], NOW - 50);
        assert!(!dir.join("old.png").exists());
        assert!(dir.join("new.png").exists());
    }

    fn uplink_fixture(name: &str) -> (Store, StationLog) {
        let path = temp_db(name);
        let store = Store::open(&path).unwrap();
        let (tx, _rx) = sync_channel(1);
        std::mem::forget(_rx);
        (store, StationLog { tx, path, counters: Arc::default() })
    }

    fn row(mode: &'static str, dial: u64, id: Option<&str>) -> Observation {
        Observation { at_utc: NOW, dial_hz: dial, freq_hz: dial, mode, kind: "x".into(), ident: id.map(String::from), grid: None, lat: None, lon: None, loc_kind: LocKind::None, snr_db: None, public: Map::new(), private: Map::new() }
    }

    #[test]
    fn the_uplink_sends_only_what_may_leave_and_never_the_private_column() {
        let (mut store, log) = uplink_fixture("uplink-rows");
        let mut secret = row("HFDL", 8_977_000, Some("BA117"));
        secret.private.insert("text".into(), Value::from("CREW NOTE"));
        let cmds = [Cmd::Observe(Box::new(secret)), Cmd::Observe(Box::new(row("RTTY", 10_100_800, Some("X")))), Cmd::Observe(Box::new(row("FT8", 50_313_000, Some("Y")))), Cmd::Observe(Box::new(row("FT8", 14_074_000, Some("Z"))))];
        store.apply(&cmds).unwrap();
        let b = log.uplink_batch(NOW, 100).unwrap();
        let idents: Vec<_> = b.observations.iter().map(|o| o["ident"].as_str().unwrap()).collect();
        assert_eq!(idents, ["BA117", "Z"]);
        assert_eq!(b.cursors.observations, 4, "what stays home is still passed over");
        let text = serde_json::to_string(&b.observations).unwrap();
        assert!(!text.contains("CREW"), "{text}");
    }

    #[test]
    fn a_long_locator_goes_up_at_six_characters() {
        let (mut store, log) = uplink_fixture("uplink-grid");
        let mut long = row("JS8", 7_078_000, Some("KI5MFF"));
        (long.grid, long.loc_kind) = (Some("DM64pp37".into()), LocKind::Grid6);
        let mut odd = row("FT8", 14_074_000, Some("Z"));
        (odd.grid, odd.loc_kind) = (Some("FN3".into()), LocKind::Grid4);
        store.apply(&[Cmd::Observe(Box::new(long)), Cmd::Observe(Box::new(odd))]).unwrap();
        let b = log.uplink_batch(NOW, 100).unwrap();
        assert_eq!((&b.observations[0]["grid"], &b.observations[0]["loc_kind"]), (&json!("DM64pp"), &json!("grid6")));
        assert_eq!((&b.observations[1]["grid"], &b.observations[1]["loc_kind"]), (&Value::Null, &json!("none")));
        assert_eq!(wire_grid(Some("FN20xk".into()), "reported".into()), (Some("FN20xk".into()), "reported".into()));
        assert_eq!(wire_grid(Some("FN20zz".into()), "grid6".into()), (Some("FN20".into()), "grid4".into()));
    }

    #[test]
    fn the_cursor_moves_only_on_success_and_a_retry_sends_the_same_rows() {
        let (mut store, log) = uplink_fixture("uplink-cursor");
        store.apply(&[Cmd::Observe(Box::new(row("FT8", 14_074_000, Some("Z"))))]).unwrap();
        let first = log.uplink_batch(NOW, 100).unwrap();
        // Nothing acknowledged it, so the same row comes again.
        let again = log.uplink_batch(NOW, 100).unwrap();
        assert_eq!(first.observations, again.observations);
        store.apply(&[Cmd::Uplinked(first.cursors)]).unwrap();
        assert!(log.uplink_batch(NOW, 100).unwrap().is_empty());
        // A stale acknowledgment cannot move it backward.
        store.apply(&[Cmd::Uplinked(UplinkCursors::default())]).unwrap();
        assert!(log.uplink_batch(NOW, 100).unwrap().is_empty());
    }

    #[test]
    fn a_stay_goes_up_only_once_it_has_ended_and_been_judged() {
        let (mut store, log) = uplink_fixture("uplink-stays");
        let mut s = segment(0, NOW - 400, NOW - 300);
        store.apply(&[Cmd::Segment(s.clone())]).unwrap();
        assert!(log.uplink_batch(NOW, 100).unwrap().listening.is_empty(), "no verdict yet");
        store.apply(&[Cmd::Health { since: NOW - 500, until: NOW, healthy: true }]).unwrap();
        assert_eq!(log.uplink_batch(NOW, 100).unwrap().listening.len(), 1);
        assert_eq!(log.uplink_batch(NOW + UPLINK_SETTLE_S, 100).unwrap().listening.len(), 1);
        s.end_utc = NOW;
        store.apply(&[Cmd::Segment(s)]).unwrap();
        assert!(log.uplink_batch(NOW, 100).unwrap().listening.is_empty(), "it is still running");
    }

    #[test]
    fn only_a_cleared_chart_is_offered_to_the_uplink() {
        let (mut store, log) = uplink_fixture("uplink-pictures");
        let mut held = picture_row(NOW - 200, Some("h.png"), 10);
        held.decision = "hold";
        let open = picture_row(NOW - 100, Some("o.png"), 10);
        store.apply(&[Cmd::Picture(Box::new(held)), Cmd::Picture(Box::new(open))]).unwrap();
        let next = log.next_picture().unwrap().expect("the published one");
        assert!(next.path.ends_with("o.png"));
        assert_eq!(next.meta["station"], "NMG");
        store.apply(&[Cmd::PictureUploaded { id: next.id, at: NOW }]).unwrap();
        assert!(log.next_picture().unwrap().is_none());
    }

    #[test]
    fn segments_open_together_each_keep_their_own_row() {
        let (mut store, _log) = uplink_fixture("open-together");
        for end in [NOW + 20, NOW + 40, NOW + 60] {
            store.apply(&[Cmd::Segment(segment(0, NOW, end)), Cmd::Segment(segment(1 << 40, NOW, end))]).unwrap();
        }
        let rows: i64 = store.conn.query_row("SELECT COUNT(*) FROM listening", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 2);
        let ended: i64 = store.conn.query_row("SELECT MIN(end_utc) FROM listening", [], |r| r.get(0)).unwrap();
        assert_eq!(ended as u64, NOW + 60);
    }

    #[test]
    fn a_held_picture_goes_only_when_approved() {
        let (mut store, log) = uplink_fixture("uplink-approve");
        let mut declined = picture_row(NOW - 200, Some("d.png"), 10);
        declined.decision = "hold";
        store.apply(&[Cmd::Picture(Box::new(declined))]).unwrap();
        assert!(log.next_picture().unwrap().is_none());
        store.apply(&[Cmd::PictureDecide { id: 1, publish: false }]).unwrap();
        store.apply(&[Cmd::PictureDecide { id: 1, publish: true }]).unwrap();
        assert!(log.next_picture().unwrap().is_none(), "a decision is not reversed");

        let mut second = picture_row(NOW - 100, Some("s.png"), 10);
        second.decision = "hold";
        store.apply(&[Cmd::Picture(Box::new(second)), Cmd::PictureDecide { id: 2, publish: true }]).unwrap();
        assert!(log.next_picture().unwrap().expect("approved").path.ends_with("s.png"));
    }

    #[test]
    fn list_limits_hold_and_a_mode_filter_applies() {
        let cmds = (0..3).map(|i| Cmd::Observe(Box::new(obs_row(&format!("K{i}ABC"))))).collect();
        let log = flushed(temp_db("limit"), cmds);
        let one = log.observations(&ListQuery { limit: Some(1), ..Default::default() }).unwrap();
        assert_eq!(one["observations"].as_array().unwrap().len(), 1);
        assert_eq!(one["observations"][0]["ident"], "K2ABC", "newest first");
        let none = log.observations(&ListQuery { mode: Some("HFDL".into()), ..Default::default() }).unwrap();
        assert!(none["observations"].as_array().unwrap().is_empty());
    }

    #[test]
    fn old_rows_expire_and_the_size_cap_removes_the_oldest() {
        let path = temp_db("prune");
        let mut store = Store::open(&path).unwrap();
        let mut old = obs_row("K1OLD");
        old.at_utc = NOW - KEEP_S - 1;
        store.apply(&[Cmd::Observe(Box::new(old)), Cmd::Observe(Box::new(obs_row("K1NEW"))), Cmd::Segment(segment(0, NOW - KEEP_S - 100, NOW - KEEP_S - 50))]).unwrap();
        store.prune(NOW, KEEP_S, i64::MAX).unwrap();
        let count = |sql: &str| store.conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(count("SELECT COUNT(*) FROM observations"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM listening"), 0);

        store.prune(NOW, KEEP_S, 0).unwrap();
        assert_eq!(count("SELECT COUNT(*) FROM observations"), 0, "over the cap, rows go until none are left");
    }

    fn healthy_of(path: &Path) -> Option<i64> {
        Connection::open(path).unwrap().query_row("SELECT healthy FROM listening", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_stay_that_clipped_is_unhealthy_and_a_passed_check_does_not_clear_it() {
        let path = temp_db("clipped");
        let mut store = Store::open(&path).unwrap();
        let mut s = segment(0, NOW, NOW + 60);
        s.peak_iq = Some(1.0);
        store.apply(&[Cmd::Segment(s)]).unwrap();
        assert_eq!(healthy_of(&path), Some(0));
        store.apply(&[Cmd::Health { since: NOW - 10, until: NOW + 100, healthy: true }]).unwrap();
        assert_eq!(healthy_of(&path), Some(0));
    }

    #[test]
    fn a_check_marks_the_stays_since_the_last_one() {
        let path = temp_db("marks");
        let mut store = Store::open(&path).unwrap();
        store.apply(&[Cmd::Segment(segment(0, NOW, NOW + 60))]).unwrap();
        assert_eq!(healthy_of(&path), None);
        store.apply(&[Cmd::Health { since: NOW - 10, until: NOW + 100, healthy: false }]).unwrap();
        assert_eq!(healthy_of(&path), Some(0));
    }

    #[test]
    fn a_version_1_database_is_migrated() {
        let path = temp_db("migrate");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&format!("BEGIN; {SCHEMA} PRAGMA user_version = 1; COMMIT;")).unwrap();
        conn.execute("INSERT INTO listening (start_utc, end_utc, dial_hz, mode, cause, decodes, valid_decodes) VALUES (1, 2, 3, 'FT8', 'operator', 0, 0)", []).unwrap();
        drop(conn);
        drop(Store::open(&path).unwrap());
        assert_eq!(healthy_of(&path), None);
        drop(Store::open(&path).unwrap());
    }

    #[test]
    fn yields_leave_out_time_on_a_broken_radio() {
        let mut bad = segment(1, NOW, NOW + 500);
        bad.peak_iq = Some(1.0);
        let hour = (NOW / 3_600 % 24) as u8;
        let log = flushed(temp_db("yields"), vec![Cmd::Segment(segment(0, NOW, NOW + 100)), Cmd::Segment(bad), Cmd::Observe(Box::new(obs_row("K1ABC")))]);
        let y = log.yields().unwrap();
        assert_eq!(y[&("FT8".to_string(), 14_074_000, hour)], crate::schedule::Yield { monitored_s: 100, observations: 1 });
    }

    #[tokio::test]
    async fn decodes_flow_from_the_broadcast_to_the_database() {
        let path = temp_db("flow");
        let log = StationLog::open(path.clone()).unwrap();
        let (tx, rx) = broadcast::channel(16);
        let task = tokio::spawn(Arc::clone(&log).run(rx, || None, || OWN.to_string()));
        tx.send(SdrEvent::Decode { inspect_hz: 14_074_000.0, event: ft8("K1ABC", "CQ", Some("FN31")) }).unwrap();
        tx.send(SdrEvent::Decode { inspect_hz: 14_074_000.0, event: ft8(OWN, "CQ", None) }).unwrap();
        drop(tx);
        task.await.unwrap();
        // The writer commits within its one-second wait.
        for _ in 0..40 {
            let got = log.observations(&ListQuery::default()).unwrap();
            if !got["observations"].as_array().unwrap().is_empty() {
                assert_eq!(got["observations"].as_array().unwrap().len(), 1);
                assert_eq!(got["observations"][0]["ident"], "K1ABC");
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the decode never reached the database");
    }
}
