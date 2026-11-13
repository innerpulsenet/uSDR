//! Spots sent to PSKReporter, as the operator's station: FT8, FT4, WSPR,
//! FST4W and JS8 decodes name their sender; the keyboard modes (PSK31/63, RTTY,
//! CW, Olivia, MFSK) give a station only when it signs (see
//! [`signed_calls`]). WSPR decodes go to WSPRnet as well (`wsprnet.rs`).
//!
//! The reporter takes the decodes the receiver already broadcasts, keeps
//! one spot per station, band and mode, and sends them every five minutes
//! as IPFIX over UDP to `report.pskreporter.info:4739`, the format and the
//! etiquette set out at pskreporter.info/pskdev.html: no more than one
//! datagram every five minutes unless one fills, timers not aligned to the
//! clock, a station reported at most once in five minutes (the page's
//! floor, and what WSJT-X does; it prefers once an hour, which loses most of
//! the repeat spots other reporters send), the record templates in the first
//! three datagrams and then hourly, and one source port throughout.
//!
//! Nothing is sent until the operator turns reporting on with a callsign
//! and locator that pass; the server never reports under a guessed call.

use crate::sdr::SdrEvent;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::{self, error::RecvError};
use usdr_proto::{ReporterState, ReporterStatus, Station, normalize_callsign, normalize_grid};

const HOST: &str = "report.pskreporter.info:4739";
/// The shortest spacing between datagrams, and the most added to it at
/// random so a room full of receivers does not send in step.
const INTERVAL: Duration = Duration::from_secs(300);
const JITTER_S: u64 = 30;
/// A station is not reported again sooner than this on the same band and
/// mode.
const REPEAT_MIN: Duration = Duration::from_secs(300);
/// How long a report is remembered: the status counts the stations
/// reported in this span.
const HEARD_KEEP: Duration = Duration::from_secs(3600);
/// A datagram this full goes at once. The page puts a datagram at 80 to
/// 90 records; 1400 bytes keeps it inside one Ethernet frame.
const FULL_SPOTS: usize = 80;
const MAX_DATAGRAM: usize = 1400;
/// The templates go in this many datagrams after a start or a change of
/// station, and then once an hour.
const TEMPLATE_SENDS: u32 = 3;
const TEMPLATE_EVERY: Duration = Duration::from_secs(3600);

const ENTERPRISE: [u8; 4] = [0x00, 0x00, 0x76, 0x8F];
const RECEIVER_TEMPLATE: u16 = 0x9992;
const SENDER_TEMPLATE: u16 = 0x9993;

/// One station heard, ready to report.
#[derive(Clone, Debug, PartialEq)]
pub struct Spot {
    pub call: String,
    pub grid: Option<String>,
    /// The signal's own frequency: the dial plus its audio offset.
    pub freq_hz: u32,
    pub snr_db: i8,
    pub mode: &'static str,
    /// Unix seconds of the slot it was heard in.
    pub at: u32,
}

type Key = (String, u32, &'static str);

/// A keyboard-mode text stream: the mode, the decoder's channel and the
/// dial it was heard on.
type StreamKey = (&'static str, u32, u64);

struct Inner {
    station: Station,
    /// What each keyboard-mode stream has printed lately, upper case, and
    /// when it last printed.
    streams: HashMap<StreamKey, (String, Instant)>,
    queue: HashMap<Key, Spot>,
    /// When each station was last reported.
    reported: HashMap<Key, Instant>,
    status: ReporterStatus,
    seq: u32,
    domain: u32,
    templates_left: u32,
    templates_at: Option<Instant>,
    due: Instant,
    /// Something the operator sees changed since the last broadcast.
    dirty: bool,
}

pub struct Reporter {
    inner: Mutex<Inner>,
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A number that differs between runs and hosts, for the observation
/// domain and the timer's jitter, without a random-number crate.
fn entropy() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.write_u32(std::process::id());
    h.finish()
}

fn jittered_interval() -> Duration {
    INTERVAL + Duration::from_secs(entropy() % (JITTER_S + 1))
}

/// The band a frequency is in, as the dedupe key: amateur bands by their
/// lower edge in kHz, anything else by the megahertz.
fn band_of(hz: u32) -> u32 {
    const BANDS: &[(u32, u32)] = &[
        (1_800, 2_000),
        (3_500, 4_000),
        (5_250, 5_450),
        (7_000, 7_300),
        (10_100, 10_150),
        (14_000, 14_350),
        (18_068, 18_168),
        (21_000, 21_450),
        (24_890, 24_990),
        (28_000, 29_700),
        (50_000, 54_000),
        (144_000, 148_000),
    ];
    let khz = hz / 1000;
    if (135..138).contains(&khz) {
        return 135;
    }
    if (472..480).contains(&khz) {
        return 472;
    }
    BANDS.iter().find(|(lo, hi)| (*lo..*hi).contains(&khz)).map_or(khz / 1000 * 1000, |b| b.0)
}

impl Reporter {
    pub fn new(station: Station) -> Self {
        let mut inner = Inner {
            station,
            streams: HashMap::new(),
            queue: HashMap::new(),
            reported: HashMap::new(),
            status: ReporterStatus::default(),
            seq: 0,
            domain: entropy() as u32,
            templates_left: TEMPLATE_SENDS,
            templates_at: None,
            due: Instant::now() + jittered_interval(),
            dirty: true,
        };
        inner.refresh_state();
        Reporter { inner: Mutex::new(inner) }
    }

    pub fn station(&self) -> Station {
        self.inner.lock().expect("reporter").station.clone()
    }

    pub fn status(&self) -> ReporterStatus {
        let g = self.inner.lock().expect("reporter");
        let mut s = g.status.clone();
        s.queued = g.queue.len();
        let hour = Instant::now().checked_sub(HEARD_KEEP);
        s.heard_hour = g.reported.values().filter(|t| hour.is_none_or(|h| **t > h)).count();
        if s.state == ReporterState::Waiting || s.state == ReporterState::Error {
            let left = g.due.saturating_duration_since(Instant::now()).as_secs();
            s.next_send = Some(unix_now() + left);
        }
        s
    }

    /// A new station: a changed receiver record goes out with its
    /// templates again, and spots queued while reporting was off are
    /// dropped rather than sent late under the new settings.
    pub fn set_station(&self, station: Station) {
        let mut g = self.inner.lock().expect("reporter");
        let changed = (&g.station.callsign, &g.station.grid, &g.station.antenna) != (&station.callsign, &station.grid, &station.antenna);
        if changed {
            g.templates_left = TEMPLATE_SENDS;
            g.reported.clear();
        }
        if !station.report_spots {
            g.queue.clear();
        }
        g.station = station;
        g.refresh_state();
        g.dirty = true;
    }

    /// Whether a WSPR decode from this sender should go to WSPRnet.
    pub fn reports(&self, call: &str) -> bool {
        let g = self.inner.lock().expect("reporter");
        g.station.report_spots && g.station.complete() && call != g.station.callsign
    }

    pub fn wsprnet_result(&self, sent: u64, err: Option<String>) {
        let mut g = self.inner.lock().expect("reporter");
        g.status.wsprnet_sent += sent;
        if err.is_some() || sent > 0 {
            g.status.wsprnet_error = err;
        }
        g.dirty = true;
    }

    /// Take a decode the receiver broadcast; those that name their sender,
    /// or keyboard text in which a station signs, become spots, and
    /// everything else is ignored.
    pub fn offer(&self, inspect_hz: f64, ev: &scannerd_engine::DecodeEvent) {
        self.offer_at(Instant::now(), inspect_hz, ev);
    }

    fn offer_at(&self, now: Instant, inspect_hz: f64, ev: &scannerd_engine::DecodeEvent) {
        let mut g = self.inner.lock().expect("reporter");
        if !(g.station.report_spots && g.station.complete()) {
            return;
        }
        let spots = match text_of(ev) {
            Some(t) => g.heard_text(inspect_hz, t),
            None => spot_of(inspect_hz, ev).into_iter().collect(),
        };
        for spot in spots {
            g.queue_spot(spot, now);
        }
    }
}

impl Inner {
    fn queue_spot(&mut self, spot: Spot, now: Instant) {
        let g = self;
        if spot.call == g.station.callsign {
            return;
        }
        let key: Key = (spot.call.clone(), band_of(spot.freq_hz), spot.mode);
        if let Some(at) = g.reported.get(&key)
            && now.saturating_duration_since(*at) < REPEAT_MIN
        {
            return;
        }
        match g.queue.get_mut(&key) {
            Some(q) => {
                // The strongest copy stands for the station; a locator heard
                // in any copy is kept.
                let grid = q.grid.take().or(spot.grid.clone());
                if spot.snr_db > q.snr_db {
                    *q = spot;
                }
                if q.grid.is_none() {
                    q.grid = grid;
                }
            }
            None => {
                g.queue.insert(key, spot);
            }
        }
        g.dirty = true;
    }

    /// Add keyboard text to its stream and return the stations that
    /// signed in it since last time.
    fn heard_text(&mut self, dial_hz: f64, t: TextIn) -> Vec<Spot> {
        let now = Instant::now();
        self.streams.retain(|_, (_, at)| now.duration_since(*at) < STREAM_IDLE);
        if !(100_000.0..4.0e9).contains(&dial_hz) || !(0.0..5_000.0).contains(&t.df) {
            return Vec::new();
        }
        let key: StreamKey = (t.mode, t.stream, dial_hz.round() as u64);
        let (buf, at) = self.streams.entry(key).or_insert_with(|| (String::new(), now));
        *at = now;
        buf.extend(t.text.chars().map(|c| if c.is_ascii() { c.to_ascii_uppercase() } else { ' ' }));
        if buf.len() > STREAM_KEEP {
            let cut = buf.len() - STREAM_KEEP;
            buf.drain(..cut);
        }
        let (calls, used) = signed_calls(buf);
        buf.drain(..used);
        let at = unix_now() as u32;
        calls
            .into_iter()
            .filter_map(|c| normalize_callsign(&c).ok())
            .map(|call| Spot { call, grid: None, freq_hz: (dial_hz + t.df).round() as u32, snr_db: t.snr.round().clamp(-128.0, 127.0) as i8, mode: t.mode, at })
            .collect()
    }
}

/// How much of a keyboard stream is kept to find a call in, and how long
/// a quiet stream is remembered.
const STREAM_KEEP: usize = 240;
const STREAM_IDLE: Duration = Duration::from_secs(600);

impl Reporter {
    /// The datagrams due now, if any, with the bookkeeping for having sent
    /// them done; `None` when it is not time or there is nothing to send.
    fn due_datagrams(&self, now: Instant) -> Option<Vec<Vec<u8>>> {
        let mut g = self.inner.lock().expect("reporter");
        if !(g.station.report_spots && g.station.complete()) || g.queue.is_empty() {
            if now >= g.due {
                g.due = now + jittered_interval();
            }
            return None;
        }
        if now < g.due && g.queue.len() < FULL_SPOTS {
            return None;
        }
        let mut spots: Vec<Spot> = g.queue.drain().map(|(_, s)| s).collect();
        spots.sort_by_key(|s| (s.at, s.freq_hz));
        let hourly = g.templates_at.is_none_or(|t| now.duration_since(t) >= TEMPLATE_EVERY);
        let mut out = Vec::new();
        let unix = unix_now() as u32;
        for chunk in pack_spots(&g.station, &spots) {
            let templates = g.templates_left > 0 || hourly;
            let (dg, records) = encode_datagram(&g.station, chunk, g.seq, g.domain, unix, templates);
            if templates {
                g.templates_left = g.templates_left.saturating_sub(1);
                g.templates_at = Some(now);
            }
            g.seq = g.seq.wrapping_add(records);
            out.push(dg);
        }
        for s in &spots {
            let key: Key = (s.call.clone(), band_of(s.freq_hz), s.mode);
            g.reported.insert(key, now);
        }
        // Stations not reported for the hour need no memory.
        g.reported.retain(|_, t| now.duration_since(*t) < HEARD_KEEP);
        g.status.spots_sent += spots.len() as u64;
        g.due = now + jittered_interval();
        g.dirty = true;
        Some(out)
    }

    fn sent(&self, datagrams: usize, err: Option<String>) {
        let mut g = self.inner.lock().expect("reporter");
        match err {
            None => {
                g.status.datagrams += datagrams as u64;
                g.status.last_sent = Some(unix_now());
                g.status.last_error = None;
            }
            Some(e) => {
                eprintln!("pskreporter: {e}");
                g.status.last_error = Some(e);
                // The next datagram re-teaches the server the templates.
                g.templates_left = TEMPLATE_SENDS;
            }
        }
        g.refresh_state();
        g.dirty = true;
    }

    fn take_dirty(&self) -> bool {
        std::mem::take(&mut self.inner.lock().expect("reporter").dirty)
    }

    /// Follow the receiver's decodes and send on the timer, forever.
    /// `changed` is called, at most once a second, when what the operator
    /// sees of the reporter has changed.
    pub async fn run(self: std::sync::Arc<Self>, mut events: broadcast::Receiver<SdrEvent>, changed: impl Fn()) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut socket: Option<tokio::net::UdpSocket> = None;
        let (wspr_tx, wspr_rx) = tokio::sync::mpsc::channel(512);
        tokio::spawn(crate::wsprnet::run(std::sync::Arc::clone(&self), wspr_rx));
        loop {
            tokio::select! {
                ev = events.recv() => match ev {
                    Ok(SdrEvent::Decode { inspect_hz, event }) => {
                        self.offer(inspect_hz, &event);
                        if let Some(w) = crate::wsprnet::spot_of(inspect_hz, &event)
                            && self.reports(&w.call)
                        {
                            // A full queue means WSPRnet has been away for
                            // hours; the newest spots are the ones dropped.
                            let _ = wspr_tx.try_send(w);
                        }
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
                },
                _ = tick.tick() => {
                    if let Some(datagrams) = self.due_datagrams(Instant::now()) {
                        let err = send_all(&mut socket, &datagrams).await.err();
                        self.sent(datagrams.len(), err);
                    }
                    if self.take_dirty() {
                        changed();
                    }
                }
            }
        }
    }
}

impl Inner {
    fn refresh_state(&mut self) {
        self.status.state = if !self.station.report_spots {
            ReporterState::Off
        } else if !self.station.complete() {
            ReporterState::Incomplete
        } else if self.status.last_error.is_some() {
            ReporterState::Error
        } else {
            ReporterState::Waiting
        };
    }
}

/// One socket for the life of the server, so every datagram leaves from
/// the same port as the page asks; it is opened again only after a failure.
async fn send_all(socket: &mut Option<tokio::net::UdpSocket>, datagrams: &[Vec<u8>]) -> Result<(), String> {
    let addr = tokio::net::lookup_host(HOST)
        .await
        .map_err(|e| format!("looking up {HOST}: {e}"))?
        .find(|a| a.is_ipv4())
        .ok_or_else(|| format!("{HOST} has no IPv4 address"))?;
    if socket.is_none() {
        *socket = Some(tokio::net::UdpSocket::bind("0.0.0.0:0").await.map_err(|e| format!("opening a UDP socket: {e}"))?);
    }
    let s = socket.as_ref().expect("socket");
    for d in datagrams {
        if let Err(e) = s.send_to(d, addr).await {
            *socket = None;
            return Err(format!("sending to {addr}: {e}"));
        }
    }
    Ok(())
}

/// A WSPR type 3 sender is its call in angle brackets once the hash has
/// been heard in full; "<...>" until then, which names no one.
pub fn unbracket(call: &str) -> Option<&str> {
    match call.strip_prefix('<').and_then(|c| c.strip_suffix('>')) {
        Some(c) if c.contains('.') => None,
        Some(c) => Some(c),
        None if call.contains('<') || call.contains('>') => None,
        None => Some(call),
    }
}

/// The spot a decode makes when it names its sender as a real callsign
/// (not a hash the decoder could not resolve): FT8, FT4, WSPR, FST4W and
/// JS8. A hash the decoder did resolve arrives as `<K1ABC>` and is the
/// sender all the same.
pub(crate) fn spot_of(inspect_hz: f64, ev: &scannerd_engine::DecodeEvent) -> Option<Spot> {
    if !ev.valid {
        return None;
    }
    let (mode, sender) = match ev.protocol.as_str() {
        "FT8" => ("FT8", unbracket(ev.fields.get("de")?)?),
        "FT4" => ("FT4", unbracket(ev.fields.get("de")?)?),
        "WSPR" => ("WSPR", unbracket(ev.fields.get("call")?)?),
        // PSKReporter takes FST4W as its own mode; WSPRnet does not.
        "FST4W" => ("FST4W", unbracket(ev.fields.get("call")?)?),
        // Each frame once: the heartbeat, reply or message read from it
        // names the same sender in the same slot.
        "JS8" if ev.kind != "frame" => return None,
        "JS8" => {
            // A heartbeat, a directed frame or a message gives "from"; a
            // compound frame gives the sender's full call as "call".
            let from = ev.fields.get("from").or_else(|| ev.fields.get("call"))?;
            if ev.fields.get("implausibleCall").is_some_and(|bad| bad.split(' ').any(|b| b == from)) {
                return None;
            }
            ("JS8", from.as_str())
        }
        _ => return None,
    };
    if sender.contains('<') || sender.starts_with('@') {
        return None;
    }
    let call = normalize_callsign(sender).ok()?;
    let df: f64 = ev.fields.get("df")?.parse().ok()?;
    if !(0.0..5_000.0).contains(&df) || !(100_000.0..4.0e9).contains(&inspect_hz) {
        return None;
    }
    let snr: f32 = ev.fields.get("snr").and_then(|s| s.parse().ok()).unwrap_or(0.0);
    // RR73 is spelled like a square in the Arctic, and is the sign-off.
    let grid = ev.fields.get("grid").filter(|g| !g.eq_ignore_ascii_case("RR73")).and_then(|g| normalize_grid(g).ok());
    let slot_ms = ev.fields.get("slotMs").or_else(|| if mode == "JS8" { ev.fields.get("slot") } else { None });
    let at = slot_ms.and_then(|s| s.parse::<u64>().ok()).map_or_else(unix_now, |ms| ms / 1000);
    Some(Spot {
        call,
        grid,
        freq_hz: (inspect_hz + df).round() as u32,
        snr_db: snr.round().clamp(-128.0, 127.0) as i8,
        mode,
        at: at as u32,
    })
}

/// A piece of keyboard text from one decoder channel.
struct TextIn<'a> {
    mode: &'static str,
    stream: u32,
    df: f64,
    snr: f32,
    text: &'a str,
}

/// The keyboard modes PSKReporter takes, as their decoders print them.
fn text_of(ev: &scannerd_engine::DecodeEvent) -> Option<TextIn<'_>> {
    let f = |k: &str| ev.fields.get(k);
    let num = |k: &str| f(k).and_then(|v| v.parse::<f64>().ok());
    let channel = f("channel").and_then(|c| c.parse().ok()).unwrap_or(0);
    let (mode, df, snr, text) = match ev.protocol.as_str() {
        "PSK31" => (if f("mode").is_some_and(|m| m == "PSK63") { "PSK63" } else { "PSK31" }, num("df")?, num("snr"), ev.summary.as_str()),
        "RTTY" => ("RTTY", num("df")?, num("snr"), ev.summary.as_str()),
        "CW" => ("CW", num("df")?, num("snr"), f("text")?.as_str()),
        "OLIVIA" => ("OLIVIA", num("centerHz")?, num("snrDb"), f("text")?.as_str()),
        "DOMINOEX" => ("DOMINOEX", num("centerHz")?, num("snrDb"), f("text")?.as_str()),
        "THOR" => ("THOR", num("centerHz")?, num("snrDb"), f("text")?.as_str()),
        "MT63" => ("MT63", num("centerHz")?, num("snrDb"), f("text")?.as_str()),
        "CONTESTIA" => ("CONTESTIA", num("centerHz")?, num("snrDb"), f("text")?.as_str()),
        "MFSK" if ev.kind == "text" => {
            let mode = match f("mode")?.as_str() {
                "MFSK16" => "MFSK16",
                "MFSK32" => "MFSK32",
                "MFSK64" => "MFSK64",
                _ => return None,
            };
            (mode, num("centerHz")?, num("snrDb"), f("text")?.as_str())
        }
        _ => return None,
    };
    Some(TextIn { mode, stream: channel, df, snr: snr.unwrap_or(0.0) as f32, text })
}

/// A callsign as it is written on the air, strictly: a prefix of one to
/// three letters and digits with a letter in it, a digit, and one to four
/// letters, with at most one portable part on either side of a slash.
/// Strict on purpose: keyboard text is full of 5NN, TU73 and decoding
/// noise, and a station that did not transmit must never be spotted.
fn on_air_call(t: &str) -> bool {
    let base = |b: &str| {
        let b = b.as_bytes();
        let letters = b.iter().rev().take_while(|c| c.is_ascii_uppercase()).count();
        if !(1..=4).contains(&letters) || b.len() < letters + 2 {
            return false;
        }
        let rest = &b[..b.len() - letters];
        let (prefix, digit) = (&rest[..rest.len() - 1], rest[rest.len() - 1]);
        digit.is_ascii_digit() && (1..=3).contains(&prefix.len()) && prefix.iter().all(u8::is_ascii_alphanumeric) && prefix.iter().any(u8::is_ascii_uppercase)
    };
    let parts: Vec<&str> = t.split('/').collect();
    let small = |p: &str| (1..=4).contains(&p.len()) && p.bytes().all(|c| c.is_ascii_alphanumeric());
    match parts.as_slice() {
        [b] => base(b),
        [a, b] => (base(a) && small(b)) || (small(a) && base(b)),
        [a, b, c] => small(a) && base(b) && small(c),
        _ => false,
    }
}

/// The calls a station signed with in keyboard text, and how far into the
/// text they were found (the caller drops that much). A station signs as
/// "DE K1ABC K1ABC", "CQ K1ABC K1ABC" or "CQ TEST K1ABC K1ABC": the call
/// after DE, CQ or TEST and the same call again within three words. The
/// call a station is calling ("W2XYZ W2XYZ DE K1ABC") is not taken, nor a
/// call sent once, which a decoding error could have made.
fn signed_calls(text: &str) -> (Vec<String>, usize) {
    let mut words: Vec<(usize, usize)> = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        let word = c.is_ascii_alphanumeric() || c == '/';
        match (word, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                words.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    // A word still being printed is not complete yet.
    let complete = words.len();
    let w = |i: usize| &text[words[i].0..words[i].1];
    let (mut calls, mut used) = (Vec::new(), 0);
    let mut i = 0;
    while i + 2 < complete {
        if matches!(w(i), "DE" | "CQ" | "TEST") && on_air_call(w(i + 1))
            && let Some(j) = (i + 2..complete.min(i + 5)).find(|&j| w(j) == w(i + 1))
        {
            calls.push(w(i + 1).to_string());
            used = words[j].1;
            i = j + 1;
            continue;
        }
        i += 1;
    }
    (calls, used)
}

fn put_str(b: &mut Vec<u8>, s: &str) {
    // A length byte, then the bytes: the page caps a field at 254.
    let s = &s.as_bytes()[..s.len().min(254)];
    b.push(s.len() as u8);
    b.extend_from_slice(s);
}

fn pad4(b: &mut Vec<u8>, start: usize) {
    while !(b.len() - start).is_multiple_of(4) {
        b.push(0);
    }
}

fn set_len(b: &mut [u8], start: usize) {
    let len = (b.len() - start) as u16;
    b[start + 2..start + 4].copy_from_slice(&len.to_be_bytes());
}

fn field(b: &mut Vec<u8>, id: u16, len: u16) {
    b.extend_from_slice(&(0x8000 | id).to_be_bytes());
    b.extend_from_slice(&len.to_be_bytes());
    b.extend_from_slice(&ENTERPRISE);
}

/// The receiver template: callsign, locator, software, and the antenna
/// when the operator described one.
fn receiver_template(b: &mut Vec<u8>, antenna: bool) {
    let start = b.len();
    b.extend_from_slice(&[0x00, 0x03, 0, 0]);
    b.extend_from_slice(&RECEIVER_TEMPLATE.to_be_bytes());
    b.extend_from_slice(&(if antenna { 4u16 } else { 3 }).to_be_bytes());
    // One scope field, as the page's descriptors have.
    b.extend_from_slice(&1u16.to_be_bytes());
    field(b, 2, 0xFFFF);
    field(b, 4, 0xFFFF);
    field(b, 8, 0xFFFF);
    if antenna {
        field(b, 9, 0xFFFF);
    }
    pad4(b, start);
    set_len(b, start);
}

/// The sender template: callsign, frequency, SNR, mode, source, locator
/// and the time heard.
fn sender_template(b: &mut Vec<u8>) {
    let start = b.len();
    b.extend_from_slice(&[0x00, 0x02, 0, 0]);
    b.extend_from_slice(&SENDER_TEMPLATE.to_be_bytes());
    b.extend_from_slice(&7u16.to_be_bytes());
    field(b, 1, 0xFFFF);
    field(b, 5, 4);
    field(b, 6, 1);
    field(b, 10, 0xFFFF);
    field(b, 11, 1);
    field(b, 3, 0xFFFF);
    // flowStartSeconds, a standard element.
    b.extend_from_slice(&[0x00, 0x96, 0x00, 0x04]);
    set_len(b, start);
}

fn sender_record(b: &mut Vec<u8>, s: &Spot) {
    put_str(b, &s.call);
    b.extend_from_slice(&s.freq_hz.to_be_bytes());
    b.push(s.snr_db as u8);
    put_str(b, s.mode);
    // Automatically extracted.
    b.push(1);
    put_str(b, s.grid.as_deref().unwrap_or(""));
    b.extend_from_slice(&s.at.to_be_bytes());
}

fn software() -> String {
    format!("uSDR {}", env!("CARGO_PKG_VERSION"))
}

/// Spots split into groups that each fit one datagram with the templates.
fn pack_spots<'a>(station: &Station, spots: &'a [Spot]) -> Vec<&'a [Spot]> {
    let fixed = 16 + 48 + 64 + 8 + 4 * 4 + station.callsign.len() + station.grid.len() + station.antenna.len().min(254) + software().len() + 8;
    let mut out = Vec::new();
    let (mut from, mut size) = (0, fixed);
    for (i, s) in spots.iter().enumerate() {
        let rec = 3 + s.call.len() + 4 + 1 + s.mode.len() + 1 + s.grid.as_ref().map_or(0, |g| g.len()) + 4;
        if size + rec > MAX_DATAGRAM && i > from {
            out.push(&spots[from..i]);
            from = i;
            size = fixed;
        }
        size += rec;
    }
    if from < spots.len() {
        out.push(&spots[from..]);
    }
    out
}

/// One datagram: the IPFIX header, the templates when they are due, the
/// receiver record, and the spots. Returns it with the count of data
/// records in it, which the sequence number advances by.
fn encode_datagram(station: &Station, spots: &[Spot], seq: u32, domain: u32, unix: u32, templates: bool) -> (Vec<u8>, u32) {
    let antenna = !station.antenna.trim().is_empty();
    let mut b = Vec::with_capacity(MAX_DATAGRAM);
    b.extend_from_slice(&[0x00, 0x0A, 0, 0]);
    b.extend_from_slice(&unix.to_be_bytes());
    b.extend_from_slice(&seq.to_be_bytes());
    b.extend_from_slice(&domain.to_be_bytes());
    if templates {
        receiver_template(&mut b, antenna);
        sender_template(&mut b);
    }
    let start = b.len();
    b.extend_from_slice(&RECEIVER_TEMPLATE.to_be_bytes());
    b.extend_from_slice(&[0, 0]);
    put_str(&mut b, &station.callsign);
    put_str(&mut b, &station.grid);
    put_str(&mut b, &software());
    if antenna {
        put_str(&mut b, station.antenna.trim());
    }
    pad4(&mut b, start);
    set_len(&mut b, start);
    if !spots.is_empty() {
        let start = b.len();
        b.extend_from_slice(&SENDER_TEMPLATE.to_be_bytes());
        b.extend_from_slice(&[0, 0]);
        for s in spots {
            sender_record(&mut b, s);
        }
        pad4(&mut b, start);
        set_len(&mut b, start);
    }
    let len = b.len() as u16;
    b[2..4].copy_from_slice(&len.to_be_bytes());
    (b, 1 + spots.len() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn station() -> Station {
        Station { callsign: "N1DQ".into(), grid: "FN42hn".into(), antenna: String::new(), report_spots: true }
    }

    fn ft8(de: &str, df: &str, snr: &str, grid: Option<&str>) -> scannerd_engine::DecodeEvent {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("de".into(), de.into());
        fields.insert("df".into(), df.into());
        fields.insert("snr".into(), snr.into());
        fields.insert("slotMs".into(), "1700000010000".into());
        if let Some(g) = grid {
            fields.insert("grid".into(), g.into());
        }
        scannerd_engine::DecodeEvent { protocol: "FT8".into(), kind: "CQ".into(), summary: String::new(), valid: true, fields }
    }

    /// A reader for what the page specifies, written apart from the
    /// encoder: it walks the sets and returns the receiver's strings and
    /// each sender record as (call, freq, snr, mode, source, grid, time).
    #[allow(clippy::type_complexity)]
    fn parse(d: &[u8]) -> (Vec<String>, Vec<(String, u32, i8, String, u8, String, u32)>, bool) {
        assert_eq!(&d[0..2], &[0x00, 0x0A]);
        assert_eq!(u16::from_be_bytes([d[2], d[3]]) as usize, d.len(), "header length");
        let mut at = 16;
        let (mut rx, mut tx, mut templates) = (Vec::new(), Vec::new(), false);
        let strs = |b: &[u8], at: &mut usize| {
            let n = b[*at] as usize;
            let s = String::from_utf8(b[*at + 1..*at + 1 + n].to_vec()).unwrap();
            *at += 1 + n;
            s
        };
        while at < d.len() {
            let id = u16::from_be_bytes([d[at], d[at + 1]]);
            let len = u16::from_be_bytes([d[at + 2], d[at + 3]]) as usize;
            assert_eq!(len % 4, 0, "set {id:#x} is padded to four bytes");
            let body = &d[at + 4..at + len];
            match id {
                2 | 3 => templates = true,
                0x9992 => {
                    let mut p = 0;
                    while p < body.len() && body[p] != 0 {
                        rx.push(strs(body, &mut p));
                    }
                }
                0x9993 => {
                    let mut p = 0;
                    while p < body.len() && body[p] != 0 {
                        let call = strs(body, &mut p);
                        let f = u32::from_be_bytes(body[p..p + 4].try_into().unwrap());
                        let snr = body[p + 4] as i8;
                        p += 5;
                        let mode = strs(body, &mut p);
                        let src = body[p];
                        p += 1;
                        let grid = strs(body, &mut p);
                        let t = u32::from_be_bytes(body[p..p + 4].try_into().unwrap());
                        p += 4;
                        tx.push((call, f, snr, mode, src, grid, t));
                    }
                }
                other => panic!("unexpected set {other:#x}"),
            }
            at += len;
        }
        (rx, tx, templates)
    }

    #[test]
    fn templates_match_the_page_s_bytes() {
        // The receiver descriptor exactly as the page gives it (option A).
        let mut b = Vec::new();
        receiver_template(&mut b, false);
        let want: Vec<u8> = [
            "00 03 00 24 99 92 00 03 00 01",
            "80 02 FF FF 00 00 76 8F",
            "80 04 FF FF 00 00 76 8F",
            "80 08 FF FF 00 00 76 8F",
            "00 00",
        ]
        .join(" ")
        .split(' ')
        .map(|h| u8::from_str_radix(h, 16).unwrap())
        .collect();
        assert_eq!(b, want);
        let mut b = Vec::new();
        receiver_template(&mut b, true);
        assert_eq!(&b[..10], &[0x00, 0x03, 0x00, 0x2C, 0x99, 0x92, 0x00, 0x04, 0x00, 0x01]);
        // The sender's: seven fields, flowStartSeconds last.
        let mut b = Vec::new();
        sender_template(&mut b);
        assert_eq!(&b[..8], &[0x00, 0x02, 0x00, 0x3C, 0x99, 0x93, 0x00, 0x07]);
        assert_eq!(b.len(), 0x3C);
        assert_eq!(&b[b.len() - 4..], &[0x00, 0x96, 0x00, 0x04]);
    }

    #[test]
    fn a_datagram_reads_back() {
        let spots = vec![
            Spot { call: "K1ABC".into(), grid: Some("FN31".into()), freq_hz: 14_075_234, snr_db: -12, mode: "FT8", at: 1_700_000_010 },
            Spot { call: "VE3/W1AW".into(), grid: None, freq_hz: 14_074_800, snr_db: 3, mode: "FT8", at: 1_700_000_025 },
        ];
        let (d, records) = encode_datagram(&station(), &spots, 9, 0xDEADBEEF, 1_700_000_100, true);
        assert_eq!(records, 3);
        assert_eq!(&d[8..16], &[0, 0, 0, 9, 0xDE, 0xAD, 0xBE, 0xEF]);
        let (rx, tx, templates) = parse(&d);
        assert!(templates);
        assert_eq!(rx, vec!["N1DQ".to_string(), "FN42hn".into(), software()]);
        assert_eq!(tx[0], ("K1ABC".into(), 14_075_234, -12, "FT8".into(), 1, "FN31".into(), 1_700_000_010));
        assert_eq!(tx[1].0, "VE3/W1AW");
        assert_eq!(tx[1].5, "");
        let (d, _) = encode_datagram(&station(), &spots, 9, 1, 1, false);
        assert!(!parse(&d).2, "no templates when they are not due");
    }

    #[test]
    fn decodes_become_spots_only_when_they_should() {
        let s = spot_of(14_074_000.0, &ft8("k1abc", "1234", "-7", Some("FN31"))).unwrap();
        assert_eq!((s.call.as_str(), s.freq_hz, s.snr_db, s.grid.as_deref(), s.at), ("K1ABC", 14_075_234, -7, Some("FN31"), 1_700_000_010));
        assert!(spot_of(14_074_000.0, &ft8("<...>", "1234", "-7", None)).is_none(), "an unresolved hash");
        let s = spot_of(14_074_000.0, &ft8("<W1AW/7>", "1234", "-7", None)).unwrap();
        assert_eq!(s.call, "W1AW/7", "a hash the decoder resolved is the sender");
        let mut ft4 = ft8("<K6VHF/HR9>", "1234", "-7", None);
        ft4.protocol = "FT4".into();
        assert_eq!(spot_of(14_080_000.0, &ft4).unwrap().mode, "FT4");
        assert!(spot_of(14_074_000.0, &ft8("<W1AW", "1234", "-7", None)).is_none(), "a stray bracket");
        assert!(spot_of(14_074_000.0, &ft8("CQ", "1234", "-7", None)).is_none(), "not a callsign");
        let mut rtty = ft8("K1ABC", "1234", "0", None);
        rtty.protocol = "RTTY".into();
        assert!(spot_of(14_074_000.0, &rtty).is_none());
        let s = spot_of(14_074_000.0, &ft8("K1ABC", "1234", "-7", Some("RR73"))).unwrap();
        assert_eq!(s.grid, None, "RR73 is not a locator");
    }

    #[test]
    fn spots_are_merged_and_not_repeated() {
        let r = Reporter::new(station());
        r.offer(14_074_000.0, &ft8("K1ABC", "1200", "-15", None));
        r.offer(14_074_000.0, &ft8("K1ABC", "1201", "-3", None));
        r.offer(14_074_000.0, &ft8("K1ABC", "1202", "-9", Some("FN31")));
        r.offer(7_074_000.0, &ft8("K1ABC", "1200", "-20", None));
        r.offer(14_074_000.0, &ft8("N1DQ", "900", "0", None));
        assert_eq!(r.status().queued, 2, "one per band, and never our own call");
        assert!(r.due_datagrams(Instant::now()).is_none(), "not before the interval");
        let later = Instant::now() + INTERVAL + Duration::from_secs(JITTER_S + 1);
        let d = r.due_datagrams(later).unwrap();
        let (_, tx, templates) = parse(&d[0]);
        assert!(templates);
        let k20 = tx.iter().find(|t| t.1 > 14_000_000).unwrap();
        assert_eq!((k20.2, k20.5.as_str()), (-3, "FN31"), "the strongest copy, and the locator from any");
        // Heard again at once: not reported again within five minutes, and
        // reported again once they are up.
        r.offer(14_074_000.0, &ft8("K1ABC", "1200", "-1", Some("FN31")));
        assert_eq!(r.status().queued, 0);
        assert_eq!(r.status().spots_sent, 2);
        r.offer_at(later + REPEAT_MIN - Duration::from_secs(1), 14_074_000.0, &ft8("K1ABC", "1200", "-1", None));
        assert_eq!(r.status().queued, 0, "four minutes fifty-nine seconds is too soon");
        r.offer_at(later + REPEAT_MIN, 14_074_000.0, &ft8("K1ABC", "1200", "-1", None));
        assert_eq!(r.status().queued, 1, "five minutes later it is a new spot");
        r.inner.lock().unwrap().queue.clear();
        // The first three datagrams carry the templates, then not.
        for _ in 0..2 {
            r.offer(14_074_000.0, &ft8(&format!("W{}AA", r.status().spots_sent), "1000", "0", None));
            let next = r.inner.lock().unwrap().due;
            let d = r.due_datagrams(next).unwrap();
            assert!(parse(&d[0]).2);
        }
        r.offer(14_074_000.0, &ft8("W9ZZ", "1000", "0", None));
        let next = r.inner.lock().unwrap().due;
        assert!(!parse(&r.due_datagrams(next).unwrap()[0]).2);
    }

    #[test]
    fn nothing_is_queued_without_a_station() {
        let r = Reporter::new(Station { report_spots: true, ..Default::default() });
        assert_eq!(r.status().state, ReporterState::Incomplete);
        r.offer(14_074_000.0, &ft8("K1ABC", "1200", "-15", None));
        assert_eq!(r.status().queued, 0);
        let r = Reporter::new(Station { report_spots: false, ..station() });
        assert_eq!(r.status().state, ReporterState::Off);
        r.offer(14_074_000.0, &ft8("K1ABC", "1200", "-15", None));
        assert_eq!(r.status().queued, 0);
    }

    #[test]
    fn a_full_band_splits_into_datagrams_that_fit() {
        let st = Station { antenna: "Inverted L, 40 m of wire".into(), ..station() };
        let spots: Vec<Spot> = (0..200)
            .map(|i| Spot { call: format!("VE{}/K{i}ABC", i % 10), grid: Some("FN31pr".into()), freq_hz: 14_074_000 + i, snr_db: -20, mode: "FT8", at: 1 })
            .collect();
        let groups = pack_spots(&st, &spots);
        assert!(groups.len() > 1);
        assert_eq!(groups.iter().map(|g| g.len()).sum::<usize>(), 200);
        for g in groups {
            let (d, _) = encode_datagram(&st, g, 0, 0, 0, true);
            assert!(d.len() <= MAX_DATAGRAM, "{}", d.len());
            let (rx, tx, _) = parse(&d);
            assert_eq!(rx.len(), 4);
            assert_eq!(tx.len(), g.len());
        }
    }

    fn ev(protocol: &str, kind: &str, kv: &[(&str, &str)]) -> scannerd_engine::DecodeEvent {
        let fields = kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        scannerd_engine::DecodeEvent { protocol: protocol.into(), kind: kind.into(), summary: String::new(), valid: true, fields }
    }

    #[test]
    fn wspr_and_js8_name_their_sender() {
        let w = ev("WSPR", "type1", &[("call", "K1ABC"), ("grid", "FN42"), ("df", "1496"), ("snr", "-21"), ("slotMs", "1700000040000")]);
        let s = spot_of(14_095_600.0, &w).unwrap();
        assert_eq!((s.call.as_str(), s.mode, s.freq_hz, s.snr_db, s.grid.as_deref(), s.at), ("K1ABC", "WSPR", 14_097_096, -21, Some("FN42"), 1_700_000_040));
        assert!(spot_of(14_095_600.0, &ev("WSPR", "type3", &[("call", "<...>"), ("df", "1496")])).is_none());
        assert_eq!(spot_of(14_095_600.0, &ev("WSPR", "type3", &[("call", "<K1ABC>"), ("df", "1496")])).unwrap().call, "K1ABC");
        let j = ev("JS8", "frame", &[("from", "K1ABC"), ("grid", "FN31"), ("df", "1200.0"), ("snr", "-5"), ("slot", "1700000015000")]);
        let s = spot_of(7_078_000.0, &j).unwrap();
        assert_eq!((s.call.as_str(), s.mode, s.freq_hz, s.at), ("K1ABC", "JS8", 7_079_200, 1_700_000_015));
        let f = ev("FST4W", "type1", &[("call", "K1ABC"), ("grid", "FN42"), ("df", "1502"), ("snr", "-24"), ("slotMs", "1700000100000")]);
        let s = spot_of(14_095_600.0, &f).unwrap();
        assert_eq!((s.call.as_str(), s.mode, s.freq_hz, s.snr_db, s.grid.as_deref(), s.at), ("K1ABC", "FST4W", 14_097_102, -24, Some("FN42"), 1_700_000_100));
        assert!(spot_of(14_095_600.0, &ev("FST4W", "type3", &[("call", "<...>"), ("df", "1502")])).is_none(), "an unresolved hash");
        assert!(spot_of(7_078_000.0, &ev("JS8", "frame", &[("from", "Q0QQ"), ("implausibleCall", "Q0QQ"), ("df", "1200")])).is_none());
        assert!(spot_of(7_078_000.0, &ev("JS8", "frame", &[("to", "@ALLCALL"), ("df", "1200")])).is_none(), "no sender");
        let hb = ev("JS8", "heartbeat", &[("from", "K1ABC"), ("grid", "FN31"), ("df", "1200.0"), ("snr", "-5"), ("slot", "1700000015000")]);
        assert!(spot_of(7_078_000.0, &hb).is_none(), "its frame has already been counted");
    }

    #[test]
    fn keyboard_text_gives_only_stations_that_sign() {
        let (c, _) = signed_calls("CQ CQ CQ DE K1ABC K1ABC K1ABC PSE K ");
        assert_eq!(c, vec!["K1ABC"]);
        let (c, _) = signed_calls("CQ TEST VE3/W1AW VE3/W1AW ");
        assert_eq!(c, vec!["VE3/W1AW"]);
        assert!(signed_calls("W2XYZ W2XYZ DE K1ABC KN ").0.is_empty(), "the called station did not transmit, and K1ABC was sent once");
        assert!(signed_calls("DE K1ABC K1AB").0.is_empty(), "the repeat is still being printed");
        assert!(signed_calls("UR RST 5NN 5NN TU 73 73 DE 5NN 5NN ").0.is_empty());
        for bad in ["5NN", "TU73", "73", "CQ", "K1", "K1ABCDE", "1ABC", "K/1AB/C/D"] {
            assert!(!on_air_call(bad), "{bad}");
        }
        for good in ["K1ABC", "9A1AA", "2E0ABC", "3DA0XX", "W1AW/P", "VE3/W1AW", "DL/K1ABC/P", "HB9X"] {
            assert!(on_air_call(good), "{good}");
        }
    }

    #[test]
    fn keyboard_spots_build_up_across_decodes() {
        let r = Reporter::new(station());
        let mut psk = ev("PSK31", "text", &[("channel", "3"), ("df", "1500"), ("snr", "12"), ("mode", "PSK31")]);
        for piece in ["CQ CQ de K1", "ABC K1ABC", " pse k\n"] {
            psk.summary = piece.into();
            r.offer(14_070_000.0, &psk);
        }
        let mut cw = ev("CW", "text", &[("channel", "1"), ("df", "700"), ("snr", "20"), ("text", "CQ DE W2XYZ W2XYZ K ")]);
        r.offer(7_030_000.0, &cw);
        cw.fields.insert("text".into(), "CQ DE W2XYZ W2XYZ K ".into());
        r.offer(7_030_000.0, &cw);
        let g = r.inner.lock().unwrap();
        let mut q: Vec<(&str, u32, &str)> = g.queue.values().map(|s| (s.call.as_str(), s.freq_hz, s.mode)).collect();
        q.sort();
        assert_eq!(q, vec![("K1ABC", 14_071_500, "PSK31"), ("W2XYZ", 7_030_700, "CW")]);
    }

    #[test]
    fn bands() {
        assert_eq!(band_of(14_075_000), 14_000);
        assert_eq!(band_of(7_074_500), 7_000);
        assert_eq!(band_of(50_313_900), 50_000);
        assert_eq!(band_of(4_625_000), 4_000);
        assert_eq!(band_of(475_700), 472);
        assert_eq!(band_of(137_500), 135);
    }
}
