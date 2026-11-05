//! FT8 and FT4 spots sent to PSKReporter, as the operator's station.
//!
//! The reporter takes the decodes the receiver already broadcasts, keeps
//! one spot per station, band and mode, and sends them every five minutes
//! as IPFIX over UDP to `report.pskreporter.info:4739`, the format and the
//! etiquette set out at pskreporter.info/pskdev.html: no more than one
//! datagram every five minutes unless one fills, timers not aligned to the
//! clock, a station reported at most once in five minutes and, unless what
//! is known of it changes, once an hour, the record templates in the first
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
/// mode, and not within the hour unless its locator became known.
const REPEAT_MIN: Duration = Duration::from_secs(300);
const REPEAT_SAME: Duration = Duration::from_secs(3600);
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

struct Inner {
    station: Station,
    queue: HashMap<Key, Spot>,
    /// When each station was last reported, and whether its locator was.
    reported: HashMap<Key, (Instant, bool)>,
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
    BANDS.iter().find(|(lo, hi)| (*lo..*hi).contains(&khz)).map_or(khz / 1000 * 1000, |b| b.0)
}

impl Reporter {
    pub fn new(station: Station) -> Self {
        let mut inner = Inner {
            station,
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
        let hour = Instant::now().checked_sub(REPEAT_SAME);
        s.heard_hour = g.reported.values().filter(|(t, _)| hour.is_none_or(|h| *t > h)).count();
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

    /// Take a decode the receiver broadcast; FT8 and FT4 decodes that name
    /// their sender become spots, everything else is ignored.
    pub fn offer(&self, inspect_hz: f64, ev: &scannerd_engine::DecodeEvent) {
        let Some(spot) = spot_of(inspect_hz, ev) else { return };
        let mut g = self.inner.lock().expect("reporter");
        if !(g.station.report_spots && g.station.complete()) || spot.call == g.station.callsign {
            return;
        }
        let key: Key = (spot.call.clone(), band_of(spot.freq_hz), spot.mode);
        if let Some((at, had_grid)) = g.reported.get(&key) {
            let since = at.elapsed();
            let news = spot.grid.is_some() && !had_grid;
            if since < REPEAT_MIN || (since < REPEAT_SAME && !news) {
                return;
            }
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
            let had = s.grid.is_some() || g.reported.get(&key).is_some_and(|r| r.1);
            g.reported.insert(key, (now, had));
        }
        // Stations not heard for the hour need no memory.
        g.reported.retain(|_, (t, _)| now.duration_since(*t) < REPEAT_SAME);
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
        loop {
            tokio::select! {
                ev = events.recv() => match ev {
                    Ok(SdrEvent::Decode { inspect_hz, event }) => self.offer(inspect_hz, &event),
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

/// The spot an FT8/FT4 decode makes, when it names a sender that is a
/// real callsign (not a hash the decoder could not resolve).
fn spot_of(inspect_hz: f64, ev: &scannerd_engine::DecodeEvent) -> Option<Spot> {
    let mode = match ev.protocol.as_str() {
        "FT8" => "FT8",
        "FT4" => "FT4",
        _ => return None,
    };
    if !ev.valid {
        return None;
    }
    let de = ev.fields.get("de")?;
    if de.contains('<') {
        return None;
    }
    let call = normalize_callsign(de).ok()?;
    let df: f64 = ev.fields.get("df")?.parse().ok()?;
    if !(0.0..5_000.0).contains(&df) || !(100_000.0..4.0e9).contains(&inspect_hz) {
        return None;
    }
    let snr: f32 = ev.fields.get("snr").and_then(|s| s.parse().ok()).unwrap_or(0.0);
    // RR73 is spelled like a square in the Arctic, and is the sign-off.
    let grid = ev.fields.get("grid").filter(|g| !g.eq_ignore_ascii_case("RR73")).and_then(|g| normalize_grid(g).ok());
    let at = ev.fields.get("slotMs").and_then(|s| s.parse::<u64>().ok()).map_or_else(unix_now, |ms| ms / 1000);
    Some(Spot {
        call,
        grid,
        freq_hz: (inspect_hz + df).round() as u32,
        snr_db: snr.round().clamp(-128.0, 127.0) as i8,
        mode,
        at: at as u32,
    })
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
        // Heard again at once: not reported again within the hour.
        r.offer(14_074_000.0, &ft8("K1ABC", "1200", "-1", Some("FN31")));
        assert_eq!(r.status().queued, 0);
        assert_eq!(r.status().spots_sent, 2);
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

    #[test]
    fn bands() {
        assert_eq!(band_of(14_075_000), 14_000);
        assert_eq!(band_of(7_074_500), 7_000);
        assert_eq!(band_of(50_313_900), 50_000);
        assert_eq!(band_of(4_625_000), 4_000);
    }
}
