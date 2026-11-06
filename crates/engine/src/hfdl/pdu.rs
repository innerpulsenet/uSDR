//! The link layer: squitters (SPDUs), MPDUs and the LPDUs inside them, the
//! HF network PDUs those carry, and ACARS inside enveloped data (ARINC 635
//! section 5 and Attachments 2-7 to 2-10, ARINC 618 for ACARS).
//!
//! Nothing is reported from a PDU whose frame check sequence fails, and a
//! field is only named when it was read from a defined layout; whatever is
//! left over is passed on as raw bytes, labeled as such.

use super::fec::{acars_residue, fcs_ok};

/// The burst a PDU came in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BurstInfo {
    pub rate_bps: u16,
    /// The 4.2 second interleaver (a double slot); otherwise 1.8 seconds.
    pub long_interleaver: bool,
    /// Carrier offset from the nominal 1440 Hz, Hz.
    pub freq_error_hz: f32,
    /// Where the carrier was heard in the audio, Hz above the dial.
    pub center_hz: f32,
    /// Signal to noise in 3 kHz, dB, from the equalizer's error on the
    /// known symbols.
    pub snr_db: f32,
}

impl BurstInfo {
    pub fn slot(&self) -> &'static str {
        if self.long_interleaver {
            "double"
        } else {
            "single"
        }
    }
    pub fn interleaver(&self) -> &'static str {
        if self.long_interleaver {
            "4.2 s"
        } else {
            "1.8 s"
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Ground to air.
    Uplink,
    /// Air to ground.
    Downlink,
}

/// Who an MPDU's LPDU is between. `aircraft_id` is the alias the ground
/// station assigned at logon (255 before one is assigned, 0 for an uplink
/// broadcast).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    pub direction: Direction,
    pub gs_id: u8,
    pub aircraft_id: u8,
}

impl Link {
    fn arrow(&self) -> String {
        match self.direction {
            Direction::Uplink => format!("gs {} → ac {}", self.gs_id, self.aircraft_id),
            Direction::Downlink => format!("ac {} → gs {}", self.aircraft_id, self.gs_id),
        }
    }
}

/// One ground station's entry in a squitter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GsStatus {
    pub gs_id: u8,
    /// The station's squitters are synchronized to UTC.
    pub utc_sync: bool,
    /// Frequency IDs (0 to 19) the station has in use; the frequencies
    /// themselves are in the system table.
    pub freq_ids: Vec<u8>,
}

/// A ground station's squitter, sent at the start of every 32 second frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Squitter {
    pub gs_id: u8,
    pub utc_sync: bool,
    pub version: u8,
    /// The station supports the reliable link service.
    pub rls: bool,
    /// The station supports ISO 8208 as well as enveloped data.
    pub iso8208: bool,
    /// The frequency is heavily used; aircraft should log on elsewhere.
    pub busy: bool,
    /// 0 none, 1 frequency change coming, 2 channel down, 3 ground station
    /// down.
    pub change_note: u8,
    /// 32 second frame of the day (0 to 2699 when synchronized).
    pub frame_index: u16,
    /// The squitter's slot (0 to 12) within the frame.
    pub frame_offset: u8,
    /// The squitter slot's time of day, seconds after 00:00 UTC, worked out
    /// from the frame index and offset as section 5.2.1.7 defines them;
    /// `None` when the station says it is not synchronized to UTC or the
    /// index is out of range.
    pub utc_s: Option<f32>,
    pub min_priority: u8,
    pub systable_version: u16,
    /// This station first, then the two others it reports on.
    pub stations: Vec<GsStatus>,
    /// The acknowledgment and slot assignment octets (bytes 4 to 51), not
    /// parsed.
    pub slots_raw: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogonKind {
    Request,
    RequestDls,
    Resume,
    Confirm,
    ResumeConfirm,
    Denied,
    Logoff,
}

impl LogonKind {
    pub fn name(&self) -> &'static str {
        match self {
            LogonKind::Request => "logon request",
            LogonKind::RequestDls => "logon request (DLS)",
            LogonKind::Resume => "logon resume",
            LogonKind::Confirm => "logon confirm",
            LogonKind::ResumeConfirm => "logon resume confirm",
            LogonKind::Denied => "logon denied",
            LogonKind::Logoff => "logoff request",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Logon {
    pub link: Link,
    pub kind: LogonKind,
    /// The aircraft's ICAO 24-bit address.
    pub icao: u32,
    /// The aircraft ID the ground station assigns (logon confirms).
    pub assigned_id: Option<u8>,
    /// The reason code (logon denied, logoff).
    pub reason: Option<u8>,
}

/// An ACARS message carried as enveloped data.
#[derive(Clone, Debug, PartialEq)]
pub struct Acars {
    pub link: Link,
    pub mode: char,
    /// The aircraft registration, as sent (dot padded).
    pub reg: String,
    /// The technical acknowledgment character, NAK shown as `!`.
    pub ack: char,
    pub label: String,
    pub block_id: char,
    /// Downlinks only.
    pub msg_no: Option<String>,
    pub flight: Option<String>,
    pub text: String,
    /// ETX ends the message; ETB means more blocks follow.
    pub final_block: bool,
    /// The ACARS block check passed (the LPDU's own check always has).
    pub crc_ok: bool,
}

/// One ground station's line in a frequency data report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropagatingFreqs {
    pub gs_id: u8,
    /// Frequency IDs the aircraft hears.
    pub heard: Vec<u8>,
    /// Frequency IDs it has tried.
    pub tuned: Vec<u8>,
}

/// An aircraft position from an HFNPDU (performance data or frequency
/// data), both of which carry latitude and longitude in fixed fields.
#[derive(Clone, Debug, PartialEq)]
pub struct Position {
    pub link: Link,
    /// "performance data" or "frequency data".
    pub source: &'static str,
    pub flight: String,
    pub lat: f64,
    pub lon: f64,
    /// The report's time of day, seconds after 00:00 UTC.
    pub utc_s: u32,
    /// From the logon request this report rode in, if any.
    pub icao: Option<u32>,
    /// Performance data: the ground station and frequency ID in use.
    pub gs_id: Option<u8>,
    pub freq_id: Option<u8>,
    /// Frequency data: what the aircraft hears from each station.
    pub propagating: Vec<PropagatingFreqs>,
}

/// Any other PDU that passed its check: what it is, and its bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct OtherPdu {
    pub link: Option<Link>,
    pub what: String,
    /// The PDU's bytes past what was parsed, not interpreted.
    pub raw: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HfdlPdu {
    Squitter(Squitter),
    Logon(Logon),
    Acars(Acars),
    Position(Position),
    Other(OtherPdu),
}

/// One decoded PDU.
#[derive(Clone, Debug, PartialEq)]
pub struct HfdlEvent {
    pub burst: BurstInfo,
    pub pdu: HfdlPdu,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

fn hms(s: f64) -> String {
    let t = s.max(0.0) as u32;
    format!("{:02}:{:02}:{:02}", t / 3600 % 24, t / 60 % 60, t % 60)
}

fn id_list(ids: &[u8]) -> String {
    ids.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn clip(s: &str, n: usize) -> String {
    let one: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let one = one.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > n {
        format!("{}…", one.chars().take(n).collect::<String>())
    } else {
        one
    }
}

impl HfdlEvent {
    pub fn kind(&self) -> &'static str {
        match &self.pdu {
            HfdlPdu::Squitter(_) => "squitter",
            HfdlPdu::Logon(_) => "logon",
            HfdlPdu::Acars(_) => "acars",
            HfdlPdu::Position(_) => "position",
            HfdlPdu::Other(_) => "pdu",
        }
    }

    pub fn summary(&self) -> String {
        let rate = format!("{} bps", self.burst.rate_bps);
        match &self.pdu {
            HfdlPdu::Squitter(s) => {
                let mut out = format!("Squitter · gs {}", s.gs_id);
                out += &format!(" · frame {} slot {}", s.frame_index, s.frame_offset);
                if let Some(u) = s.utc_s {
                    out += &format!(" · {} UTC", hms(u as f64));
                }
                if let Some(me) = s.stations.first() {
                    out += &format!(" · freqs {}", id_list(&me.freq_ids));
                }
                out + " · " + &rate
            }
            HfdlPdu::Logon(l) => {
                let mut out = format!(
                    "{} · {} · icao {:06X}",
                    cap(l.kind.name()),
                    l.link.arrow(),
                    l.icao
                );
                if let Some(id) = l.assigned_id {
                    out += &format!(" · assigned ac {id}");
                }
                if let Some(r) = l.reason {
                    out += &format!(" · reason {r}");
                }
                out
            }
            HfdlPdu::Acars(a) => {
                let mut out = format!(
                    "ACARS · {} · {}",
                    a.link.arrow(),
                    a.reg.trim_start_matches('.')
                );
                if let Some(f) = &a.flight {
                    out += &format!(" · {f}");
                }
                out += &format!(" · label {}", a.label);
                let t = clip(&a.text, 60);
                if !t.is_empty() {
                    out += &format!(" · {t}");
                }
                out
            }
            HfdlPdu::Position(p) => format!(
                "Position · {} · {} · {:.4}, {:.4} · {} UTC",
                p.link.arrow(),
                if p.flight.is_empty() { "-" } else { &p.flight },
                p.lat,
                p.lon,
                hms(p.utc_s as f64)
            ),
            HfdlPdu::Other(o) => {
                let mut out = cap(&o.what);
                if let Some(l) = &o.link {
                    out += &format!(" · {}", l.arrow());
                }
                if !o.raw.is_empty() {
                    out += &format!(" · {} bytes raw", o.raw.len());
                }
                out
            }
        }
    }

    /// Key and value pairs, camelCase keys.
    pub fn fields(&self) -> Vec<(&'static str, String)> {
        let mut f: Vec<(&'static str, String)> = Vec::new();
        let link = |f: &mut Vec<(&'static str, String)>, l: &Link| {
            f.push((
                "direction",
                match l.direction {
                    Direction::Uplink => "uplink".into(),
                    Direction::Downlink => "downlink".into(),
                },
            ));
            f.push(("gsId", l.gs_id.to_string()));
            f.push(("aircraftId", l.aircraft_id.to_string()));
        };
        match &self.pdu {
            HfdlPdu::Squitter(s) => {
                f.push(("gsId", s.gs_id.to_string()));
                f.push(("utcSync", s.utc_sync.to_string()));
                f.push(("frameIndex", s.frame_index.to_string()));
                f.push(("frameOffset", s.frame_offset.to_string()));
                if let Some(u) = s.utc_s {
                    f.push(("utc", hms(u as f64)));
                }
                f.push(("version", s.version.to_string()));
                f.push(("rls", s.rls.to_string()));
                f.push(("iso8208", s.iso8208.to_string()));
                f.push(("busy", s.busy.to_string()));
                f.push(("changeNote", s.change_note.to_string()));
                f.push(("minPriority", s.min_priority.to_string()));
                f.push(("systableVersion", s.systable_version.to_string()));
                if let Some(me) = s.stations.first() {
                    f.push(("freqIds", id_list(&me.freq_ids)));
                }
                let others: Vec<String> = s
                    .stations
                    .iter()
                    .skip(1)
                    .map(|g| {
                        format!(
                            "{}{}:{}",
                            g.gs_id,
                            if g.utc_sync { "" } else { "*" },
                            id_list(&g.freq_ids)
                        )
                    })
                    .collect();
                f.push(("otherStations", others.join(" ")));
                f.push(("slotsRawHex", hex(&s.slots_raw)));
            }
            HfdlPdu::Logon(l) => {
                link(&mut f, &l.link);
                f.push(("lpdu", l.kind.name().into()));
                f.push(("icao", format!("{:06X}", l.icao)));
                if let Some(id) = l.assigned_id {
                    f.push(("assignedId", id.to_string()));
                }
                if let Some(r) = l.reason {
                    f.push(("reason", r.to_string()));
                }
            }
            HfdlPdu::Acars(a) => {
                link(&mut f, &a.link);
                f.push(("mode", a.mode.to_string()));
                f.push(("reg", a.reg.clone()));
                f.push(("ack", a.ack.to_string()));
                f.push(("label", a.label.clone()));
                f.push(("blockId", a.block_id.to_string()));
                if let Some(m) = &a.msg_no {
                    f.push(("msgNo", m.clone()));
                }
                if let Some(fl) = &a.flight {
                    f.push(("flight", fl.clone()));
                }
                f.push(("text", a.text.clone()));
                f.push(("more", (!a.final_block).to_string()));
                f.push(("acarsCrcOk", a.crc_ok.to_string()));
            }
            HfdlPdu::Position(p) => {
                link(&mut f, &p.link);
                f.push(("source", p.source.into()));
                f.push(("flight", p.flight.clone()));
                f.push(("lat", format!("{:.5}", p.lat)));
                f.push(("lon", format!("{:.5}", p.lon)));
                f.push(("utc", hms(p.utc_s as f64)));
                if let Some(i) = p.icao {
                    f.push(("icao", format!("{i:06X}")));
                }
                if let Some(g) = p.gs_id {
                    f.push(("reportGsId", g.to_string()));
                }
                if let Some(q) = p.freq_id {
                    f.push(("freqId", q.to_string()));
                }
                if !p.propagating.is_empty() {
                    let v: Vec<String> = p
                        .propagating
                        .iter()
                        .map(|g| format!("{}:{}/{}", g.gs_id, id_list(&g.heard), id_list(&g.tuned)))
                        .collect();
                    f.push(("propagating", v.join(" ")));
                }
            }
            HfdlPdu::Other(o) => {
                if let Some(l) = &o.link {
                    link(&mut f, l);
                }
                f.push(("what", o.what.clone()));
                if !o.raw.is_empty() {
                    f.push(("rawHex", hex(&o.raw)));
                }
            }
        }
        let b = &self.burst;
        f.push(("rate", b.rate_bps.to_string()));
        f.push(("slot", b.slot().into()));
        f.push(("interleaver", b.interleaver().into()));
        f.push(("freqError", format!("{:+.1}", b.freq_error_hz)));
        f.push(("centerHz", format!("{:.1}", b.center_hz)));
        f.push(("snr", format!("{:.1}", b.snr_db)));
        f
    }
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Bits set in a frequency mask, as frequency IDs.
fn mask_ids(mask: u32, n: u8) -> Vec<u8> {
    (0..n).filter(|i| mask >> i & 1 != 0).collect()
}

/// An ICAO address: sent most significant byte first, but each byte with
/// its bits in the opposite order to numeric fields.
fn icao(b: &[u8]) -> u32 {
    (b[0].reverse_bits() as u32) << 16
        | (b[1].reverse_bits() as u32) << 8
        | b[2].reverse_bits() as u32
}

/// A 20-bit two's complement coordinate, 180 degrees full scale.
fn coord(c: u32) -> f64 {
    let v = ((c << 12) as i32) >> 12;
    v as f64 * 180.0 / 0x7ffff as f64
}

fn text(b: &[u8]) -> String {
    b.iter()
        .map(|&c| {
            let c = c & 0x7f;
            if (0x20..0x7f).contains(&c) || c == b'\r' || c == b'\n' {
                c as char
            } else {
                '.'
            }
        })
        .collect()
}

/// What one burst carried. `Err` when no PDU in it passed its check.
pub struct Parsed {
    pub pdus: Vec<HfdlPdu>,
    /// LPDUs inside a good MPDU that failed their own check.
    pub lpdu_failed: u32,
}

/// Parse the decoded bytes of a burst. Returns `None` when the SPDU or the
/// MPDU header fails its check.
pub fn parse_burst(buf: &[u8]) -> Option<Parsed> {
    if buf.is_empty() {
        return None;
    }
    if buf[0] & 1 == 0 {
        parse_spdu(buf)
    } else {
        parse_mpdu(buf)
    }
}

const SPDU_LEN: usize = 66;

fn parse_spdu(b: &[u8]) -> Option<Parsed> {
    if b.len() < SPDU_LEN || !fcs_ok(b, SPDU_LEN - 2) {
        return None;
    }
    let gs_id = b[1] & 0x7f;
    let utc_sync = b[1] & 0x80 != 0;
    let frame_index = b[2] as u16 | ((b[3] as u16 & 0x0f) << 8);
    let frame_offset = b[3] >> 4;
    let stations = vec![
        GsStatus {
            gs_id,
            utc_sync,
            freq_ids: mask_ids(
                (b[54] >> 4) as u32 | (b[55] as u32) << 4 | (b[56] as u32) << 12,
                20,
            ),
        },
        GsStatus {
            gs_id: b[57] & 0x7f,
            utc_sync: b[57] & 0x80 != 0,
            freq_ids: mask_ids(
                b[58] as u32 | (b[59] as u32) << 8 | (b[60] as u32 & 0x0f) << 16,
                20,
            ),
        },
        GsStatus {
            gs_id: (b[60] >> 4) | (b[61] & 0x07) << 4,
            utc_sync: b[61] & 0x08 != 0,
            freq_ids: mask_ids(
                (b[61] >> 4) as u32 | (b[62] as u32) << 4 | (b[63] as u32) << 12,
                20,
            ),
        },
    ];
    // 2700 frames of 32 s a day, 13 slots a frame.
    let utc_s = (utc_sync && frame_index < 2700 && frame_offset < 13)
        .then(|| frame_index as f32 * 32.0 + frame_offset as f32 * 32.0 / 13.0);
    let sq = Squitter {
        gs_id,
        utc_sync,
        version: (b[0] >> 2) & 3,
        rls: b[0] & 0x02 != 0,
        busy: b[0] & 0x10 != 0,
        iso8208: b[0] & 0x20 != 0,
        change_note: b[0] >> 6,
        frame_index,
        frame_offset,
        utc_s,
        min_priority: b[52] & 0x0f,
        systable_version: b[53] as u16 | ((b[54] as u16 & 0x0f) << 8),
        stations,
        slots_raw: b[4..52].to_vec(),
    };
    Some(Parsed {
        pdus: vec![HfdlPdu::Squitter(sq)],
        lpdu_failed: 0,
    })
}

fn parse_mpdu(b: &[u8]) -> Option<Parsed> {
    let mut out = Parsed {
        pdus: Vec::new(),
        lpdu_failed: 0,
    };
    if b[0] & 0x02 != 0 {
        // Downlink: MID, GS ID, aircraft ID, reservation request, two
        // acknowledgment octets, one size octet per LPDU.
        let nlp = ((b[0] >> 2) & 0x0f) as usize;
        let hdr = 6 + nlp;
        if b.len() < hdr + 2 || !fcs_ok(b, hdr) {
            return None;
        }
        let link = Link {
            direction: Direction::Downlink,
            gs_id: b[1] & 0x7f,
            aircraft_id: b[2],
        };
        let sizes: Vec<usize> = b[6..hdr].iter().map(|&s| s as usize + 1).collect();
        lpdus(b, hdr + 2, &sizes, link, &mut out);
        if nlp == 0 {
            out.pdus.push(HfdlPdu::Other(OtherPdu {
                link: Some(link),
                what: "downlink MPDU, no LPDUs".into(),
                raw: Vec::new(),
            }));
        }
    } else {
        // Uplink: MID, GS ID, then per aircraft its ID, the MDF octet (the
        // LPDU count in its high nibble) and one size octet per LPDU.
        let nac = ((b[0] >> 4) & 0x07) as usize + 1;
        let mut pos = 2;
        let mut dests = Vec::new();
        for _ in 0..nac {
            if b.len() < pos + 2 {
                return None;
            }
            let aid = b[pos];
            let nlp = (b[pos + 1] >> 4) as usize;
            if b.len() < pos + 2 + nlp {
                return None;
            }
            let sizes: Vec<usize> = b[pos + 2..pos + 2 + nlp]
                .iter()
                .map(|&s| s as usize + 1)
                .collect();
            dests.push((aid, sizes));
            pos += 2 + nlp;
        }
        if b.len() < pos + 2 || !fcs_ok(b, pos) {
            return None;
        }
        let gs_id = b[1] & 0x7f;
        let mut data = pos + 2;
        for (aid, sizes) in dests {
            let link = Link {
                direction: Direction::Uplink,
                gs_id,
                aircraft_id: aid,
            };
            data = lpdus(b, data, &sizes, link, &mut out);
        }
    }
    Some(out)
}

/// Parse LPDUs of the given sizes from `start`; returns where they end.
fn lpdus(b: &[u8], start: usize, sizes: &[usize], link: Link, out: &mut Parsed) -> usize {
    let mut p = start;
    for &len in sizes {
        if p + len > b.len() {
            out.lpdu_failed += 1;
            return b.len();
        }
        let l = &b[p..p + len];
        p += len;
        if len < 3 || !fcs_ok(l, len - 2) {
            out.lpdu_failed += 1;
            continue;
        }
        lpdu(&l[..len - 2], link, out);
    }
    p
}

fn lpdu(l: &[u8], link: Link, out: &mut Parsed) {
    let kind = match l[0] {
        0x2f => Some(LogonKind::Denied),
        0x3f => Some(LogonKind::Logoff),
        0x4f => Some(LogonKind::Resume),
        0x5f => Some(LogonKind::ResumeConfirm),
        0x8f => Some(LogonKind::Request),
        0x9f => Some(LogonKind::Confirm),
        0xbf => Some(LogonKind::RequestDls),
        _ => None,
    };
    let mut icao_addr = None;
    let rest = match (l[0], kind) {
        (0x0d, _) | (0x1d, _) => &l[1..],
        (_, Some(k)) => {
            let (need, assigned, reason) = match k {
                LogonKind::Confirm | LogonKind::ResumeConfirm => (8, l.get(4).copied(), None),
                LogonKind::Denied | LogonKind::Logoff => (5, None, l.get(4).copied()),
                _ => (4, None, None),
            };
            if l.len() < need {
                out.pdus.push(HfdlPdu::Other(OtherPdu {
                    link: Some(link),
                    what: format!("{} (too short)", k.name()),
                    raw: l[1..].to_vec(),
                }));
                return;
            }
            let a = icao(&l[1..4]);
            icao_addr = Some(a);
            out.pdus.push(HfdlPdu::Logon(Logon {
                link,
                kind: k,
                icao: a,
                assigned_id: assigned,
                reason,
            }));
            &l[need..]
        }
        _ => {
            let what = if l[0] & 1 == 0 {
                "numbered data LPDU (not parsed)".to_string()
            } else {
                format!("LPDU type {:02X} (not parsed)", l[0])
            };
            out.pdus.push(HfdlPdu::Other(OtherPdu {
                link: Some(link),
                what,
                raw: l[1..].to_vec(),
            }));
            return;
        }
    };
    if !rest.is_empty() {
        hfnpdu(rest, link, icao_addr, out);
    }
}

fn u16le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn flight(b: &[u8]) -> String {
    text(b)
        .trim_matches(|c: char| c == ' ' || c == '.')
        .to_string()
}

fn hfnpdu(h: &[u8], link: Link, icao_addr: Option<u32>, out: &mut Parsed) {
    let other = |what: String, raw: &[u8]| {
        HfdlPdu::Other(OtherPdu {
            link: Some(link),
            what,
            raw: raw.to_vec(),
        })
    };
    if h[0] != 0xff || h.len() < 2 {
        out.pdus.push(other("LPDU data, not an HFNPDU".into(), h));
        return;
    }
    let pos = |h: &[u8]| {
        let lat = coord(h[8] as u32 | (h[9] as u32) << 8 | (h[10] as u32 & 0x0f) << 16);
        let lon = coord((h[10] >> 4) as u32 | (h[11] as u32) << 4 | (h[12] as u32) << 12);
        (lat, lon, 2 * u16le(&h[13..15]) as u32)
    };
    match h[1] {
        0xd1 if h.len() >= 47 => {
            let (lat, lon, utc_s) = pos(h);
            if lat.abs() <= 90.0 && lon.abs() <= 180.0 {
                out.pdus.push(HfdlPdu::Position(Position {
                    link,
                    source: "performance data",
                    flight: flight(&h[2..8]),
                    lat,
                    lon,
                    utc_s,
                    icao: icao_addr,
                    gs_id: Some(h[17] & 0x7f),
                    freq_id: Some(h[18]),
                    propagating: Vec::new(),
                }));
            } else {
                out.pdus.push(other(
                    "performance data (position out of range)".into(),
                    &h[2..],
                ));
            }
        }
        0xd5 if h.len() >= 15 => {
            let (lat, lon, utc_s) = pos(h);
            let propagating = h[15..]
                .chunks_exact(6)
                .take(6)
                .map(|g| PropagatingFreqs {
                    gs_id: g[0] & 0x7f,
                    heard: mask_ids(
                        g[1] as u32 | (g[2] as u32) << 8 | (g[3] as u32 & 0x0f) << 16,
                        20,
                    ),
                    tuned: mask_ids(
                        (g[3] >> 4) as u32 | (g[4] as u32) << 4 | (g[5] as u32) << 12,
                        20,
                    ),
                })
                .collect();
            if lat.abs() <= 90.0 && lon.abs() <= 180.0 {
                out.pdus.push(HfdlPdu::Position(Position {
                    link,
                    source: "frequency data",
                    flight: flight(&h[2..8]),
                    lat,
                    lon,
                    utc_s,
                    icao: icao_addr,
                    gs_id: None,
                    freq_id: None,
                    propagating,
                }));
            } else {
                out.pdus.push(other(
                    "frequency data (position out of range)".into(),
                    &h[2..],
                ));
            }
        }
        0xd0 if h.len() >= 5 => {
            let what = format!(
                "system table part {} of {}, version {}",
                (h[2] & 0x0f) + 1,
                (h[2] >> 4) + 1,
                (h[3] >> 4) as u16 | (h[4] as u16) << 4
            );
            out.pdus.push(other(what, &h[5..]));
        }
        0xd2 if h.len() >= 4 => {
            out.pdus.push(other(
                format!("system table request {:04X}", u16le(&h[2..4])),
                &h[4..],
            ));
        }
        0xde => out.pdus.push(other("delayed echo".into(), &h[2..])),
        0xff => match acars(&h[2..], link) {
            Some(a) => out.pdus.push(HfdlPdu::Acars(a)),
            None => out
                .pdus
                .push(other("enveloped data, not ACARS".into(), &h[2..])),
        },
        t => out
            .pdus
            .push(other(format!("HFNPDU type {t:02X}"), &h[2..])),
    }
}

const SOH: u8 = 0x01;
const STX: u8 = 0x02;
const ETX: u8 = 0x03;
const ETB: u8 = 0x17;
const DEL: u8 = 0x7f;
const NAK: u8 = 0x15;
const ACK: u8 = 0x06;

/// ARINC 618 ACARS: SOH, mode, seven character registration, technical
/// acknowledgment, two character label, block ID, then STX and the text
/// (downlinks start it with a four character message number and a six
/// character flight ID), ETX or ETB, the two block check bytes, and DEL.
fn acars(b: &[u8], link: Link) -> Option<Acars> {
    if b.len() < 2 || b[0] != SOH {
        return None;
    }
    let mut b = &b[1..];
    if *b.last()? != DEL {
        return None;
    }
    b = &b[..b.len() - 1];
    // Mode through ETX, the block check, at least.
    if b.len() < 15 {
        return None;
    }
    let crc_ok = acars_residue(b) == 0;
    let body: Vec<u8> = b[..b.len() - 2].iter().map(|c| c & 0x7f).collect();
    let final_block = match *body.last()? {
        ETX => true,
        ETB => false,
        _ => return None,
    };
    let body = &body[..body.len() - 1];
    let mode = body[0] as char;
    let reg = text(&body[1..8]);
    let ack = match body[8] {
        NAK => '!',
        ACK => '^',
        c => c as char,
    };
    let label = format!(
        "{}{}",
        body[9] as char,
        if body[10] == DEL {
            'd'
        } else {
            body[10] as char
        }
    );
    let block_id = if body[11] == 0 { ' ' } else { body[11] as char };
    let downlink_block = block_id.is_ascii_digit();
    let mut t = &body[12..];
    let (mut msg_no, mut flight_id) = (None, None);
    if !t.is_empty() {
        if t[0] != STX {
            return None;
        }
        t = &t[1..];
        if downlink_block {
            if t.len() < 10 {
                return None;
            }
            msg_no = Some(text(&t[..4]));
            flight_id = Some(text(&t[4..10]).trim().to_string());
            t = &t[10..];
        }
    } else if downlink_block {
        return None;
    }
    Some(Acars {
        link,
        mode,
        reg,
        ack,
        label,
        block_id,
        msg_no,
        flight: flight_id,
        text: text(t),
        final_block,
        crc_ok,
    })
}
