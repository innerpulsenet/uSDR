//! AIS, the marine Automatic Identification System: 9600 baud GMSK
//! (BT 0.4, ±2.4 kHz) on two VHF channels, A at 161.975 MHz and B at
//! 162.025 MHz, both heard at once from the span.
//!
//! One mixer brings the span down around 162.000 MHz and a half-band
//! chain takes it to 120–240 kS/s, flat and alias-free over both channels
//! (a box filter there folded the whole span's noise into them); each
//! channel then has its own mixer, FIR and FM discriminator at about
//! 48 kS/s, and the discriminator is averaged over a symbol, close to the
//! filter matched to GMSK's pulse. Bursts are short (a position
//! report is 256 bits, 27 ms), too short for a timing loop to settle on,
//! so each channel slices the discriminator at six phases of the symbol
//! clock at once, each phase with its own NRZI and HDLC deframer: whichever
//! lands on the eye passes its X.25 checksum, and the copies the others
//! also got are dropped.
//!
//! Frames carry the ITU-R M.1371 messages; the ones parsed here are the
//! positions (1–3 class A, 18–19 class B, 27 long range), base stations
//! (4), static and voyage data (5, 24), SAR aircraft (9) and aids to
//! navigation (21). Any other type is reported by number.

use crate::nbfm::NbfmDemod;
use num_complex::Complex32;
use scannerd_dsp::{DecimFir, HalfbandChain, Nco};

pub const AIS_A_HZ: f64 = 161_975_000.0;
pub const AIS_B_HZ: f64 = 162_025_000.0;
/// Midway between the channels, where the receiver mixes the span to.
pub const AIS_MID_HZ: f64 = 162_000_000.0;
pub const BAUD: f64 = 9600.0;
/// Peak deviation: modulation index 0.5 at 9600 baud.
pub const DEVIATION_HZ: f32 = 2_400.0;

/// The half-band chain stops at the first rate under this.
const MID_RATE_MAX: f64 = 240_000.0;
const CHAN_RATE: f64 = 48_000.0;
const LANES: usize = 6;

/// One decoded AIS message, with what was parsed from it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AisMessage {
    pub kind: u8,
    pub mmsi: u32,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub sog_kn: Option<f32>,
    pub cog: Option<f32>,
    pub heading: Option<u16>,
    pub status: Option<u8>,
    pub name: Option<String>,
    pub callsign: Option<String>,
    pub ship_type: Option<u8>,
    pub destination: Option<String>,
    pub imo: Option<u32>,
    /// To bow, stern, port and starboard, in meters.
    pub dims: Option<(u16, u16, u8, u8)>,
    pub draught_m: Option<f32>,
    /// MM-DD HH:MM UTC, as the vessel gives it.
    pub eta: Option<String>,
    pub aid_type: Option<u8>,
    pub altitude_m: Option<u16>,
    /// A base station's UTC, `YYYY-MM-DD HH:MM:SS`.
    pub utc: Option<String>,
}

/// A frame heard on a channel: which one, and its message.
#[derive(Clone, Debug, PartialEq)]
pub struct AisFrame {
    /// 'A' (161.975 MHz) or 'B' (162.025 MHz).
    pub channel: char,
    pub msg: AisMessage,
}

/// Both channels, from the span.
pub struct AisReceiver {
    nco: Nco,
    mixed: Vec<Complex32>,
    chain: HalfbandChain,
    mid: Vec<Complex32>,
    chans: [AisChannel; 2],
    /// Stations heard sending a position or their static data: a message
    /// that carries neither is taken only from one of these.
    known: std::collections::HashSet<u32>,
    /// Frames held back for coming from a station not yet known.
    unknown: u64,
}

impl AisReceiver {
    /// `mid_offset_hz`: where 162.000 MHz sits in the span, from its center.
    pub fn new(span_rate: f64, mid_offset_hz: f64) -> Self {
        let mut stages = 0u32;
        while span_rate / f64::from(1u32 << stages) > MID_RATE_MAX {
            stages += 1;
        }
        // Both channels and their filters: ±25 kHz and 8 kHz either side.
        let chain = HalfbandChain::new(span_rate, 36_000.0, stages);
        let mid_rate = chain.fs_out();
        let mut nco = Nco::new();
        nco.set_freq(mid_offset_hz, span_rate);
        Self {
            nco,
            mixed: Vec::new(),
            chain,
            mid: Vec::new(),
            chans: [AisChannel::new('A', AIS_A_HZ - AIS_MID_HZ, mid_rate), AisChannel::new('B', AIS_B_HZ - AIS_MID_HZ, mid_rate)],
            known: std::collections::HashSet::new(),
            unknown: 0,
        }
    }

    /// Whether both channels sit inside a span `span_rate` wide whose
    /// center is `center_hz`, with room for their filters.
    pub fn hears(center_hz: f64, span_rate: f64) -> bool {
        let half = span_rate * 0.45;
        (AIS_A_HZ - 15e3 - center_hz).abs() < half && (AIS_B_HZ + 15e3 - center_hz).abs() < half
    }

    /// Feed one block of span I/Q; frames decoded in it are appended.
    pub fn process(&mut self, iq: &[Complex32], out: &mut Vec<AisFrame>) {
        self.nco.mix(iq, &mut self.mixed);
        self.chain.process(&self.mixed, &mut self.mid);
        let from = out.len();
        for c in &mut self.chans {
            c.process(&self.mid, out);
        }
        // The fixed-length reports vouch for a station; the free-length
        // ones (binary, safety, acknowledgements, channel management) are
        // where a noise frame that passed its check would get through.
        let mut k = from;
        while k < out.len() {
            let m = &out[k].msg;
            if matches!(m.kind, 1..=5 | 9 | 11 | 18 | 19 | 21 | 24 | 27) {
                self.known.insert(m.mmsi);
                k += 1;
            } else if self.known.contains(&m.mmsi) {
                k += 1;
            } else {
                self.unknown += 1;
                out.remove(k);
            }
        }
    }

    /// Frames held back for coming from a station not yet heard properly.
    pub fn unvouched(&self) -> u64 {
        self.unknown
    }

    /// Frames that passed their checksum and frames that did not, per
    /// channel, since the receiver was made.
    pub fn counts(&self) -> [(u64, u64); 2] {
        [self.chans[0].counts(), self.chans[1].counts()]
    }

    /// Each channel's frame counts and its power since the last call.
    pub fn levels(&mut self) -> [ChannelLevels; 2] {
        [self.chans[0].levels(), self.chans[1].levels()]
    }
}

/// A channel's health: frames passed (and parsed), failed, and passed but
/// implausible; its mean and peak power in the channel filter, dBFS.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChannelLevels {
    pub good: u64,
    pub bad: u64,
    pub odd: u64,
    pub mean_dbfs: f32,
    pub peak_dbfs: f32,
}

struct AisChannel {
    name: char,
    nco: Nco,
    mixed: Vec<Complex32>,
    fir: DecimFir,
    iq: Vec<Complex32>,
    disc: NbfmDemod,
    hz: Vec<f32>,
    dc: f32,
    dc_a: f32,
    prev: f32,
    /// The last symbol's worth of discriminator, for its running mean.
    avg: std::collections::VecDeque<f32>,
    avg_sum: f32,
    avg_n: usize,
    sps: f64,
    /// Samples seen, as the lanes' clock.
    n: f64,
    lanes: Vec<Lane>,
    /// The last frames passed, and when, so a burst copied by several
    /// lanes is reported once.
    recent: Vec<(Vec<u8>, f64)>,
    good: u64,
    bad: u64,
    /// Frames that passed the check but no AIS station would send.
    odd: u64,
    /// The strongest and the mean channel power since `levels` was read.
    peak_pow: f32,
    sum_pow: f64,
    n_pow: u64,
}

impl AisChannel {
    fn new(name: char, offset_hz: f64, mid_rate: f64) -> Self {
        let decim = (mid_rate / CHAN_RATE).round().max(1.0) as usize;
        let rate = mid_rate / decim as f64;
        let mut nco = Nco::new();
        nco.set_freq(offset_hz, mid_rate);
        let sps = rate / BAUD;
        Self {
            name,
            nco,
            mixed: Vec::new(),
            fir: DecimFir::new(7_500.0, mid_rate as f32, decim, 127),
            iq: Vec::new(),
            disc: NbfmDemod::with_deviation(rate, DEVIATION_HZ),
            hz: Vec::new(),
            dc: 0.0,
            // About 20 ms: far longer than the longest run of one level.
            dc_a: 1.0 / (rate as f32 * 0.02),
            prev: 0.0,
            avg: std::collections::VecDeque::new(),
            avg_sum: 0.0,
            avg_n: (sps.round() as usize).max(1),
            sps,
            n: 0.0,
            lanes: (0..LANES).map(|k| Lane::new(k as f64 * sps / LANES as f64)).collect(),
            recent: Vec::new(),
            good: 0,
            bad: 0,
            odd: 0,
            peak_pow: 0.0,
            sum_pow: 0.0,
            n_pow: 0,
        }
    }

    fn counts(&self) -> (u64, u64) {
        (self.good, self.bad)
    }

    fn levels(&mut self) -> ChannelLevels {
        let to_db = |p: f64| 10.0 * p.max(1e-20).log10() as f32;
        let l = ChannelLevels { good: self.good, bad: self.bad, odd: self.odd, mean_dbfs: to_db(self.sum_pow / self.n_pow.max(1) as f64), peak_dbfs: to_db(f64::from(self.peak_pow)) };
        (self.peak_pow, self.sum_pow, self.n_pow) = (0.0, 0.0, 0);
        l
    }

    fn process(&mut self, mid: &[Complex32], out: &mut Vec<AisFrame>) {
        self.nco.mix(mid, &mut self.mixed);
        self.fir.process(&self.mixed, &mut self.iq);
        for s in &self.iq {
            let p = s.norm_sqr();
            self.peak_pow = self.peak_pow.max(p);
            self.sum_pow += f64::from(p);
            self.n_pow += 1;
        }
        self.disc.discriminator_hz(&self.iq, &mut self.hz);
        for i in 0..self.hz.len() {
            // A symbol's running mean, then the slow DC: the mean delays by
            // half a symbol, which the lanes' phases take up.
            let raw = self.hz[i];
            self.avg.push_back(raw);
            self.avg_sum += raw;
            if self.avg.len() > self.avg_n {
                self.avg_sum -= self.avg.pop_front().unwrap_or(0.0);
            }
            let x = self.avg_sum / self.avg.len() as f32;
            self.dc += (x - self.dc) * self.dc_a;
            let v = x - self.dc;
            let t0 = self.n;
            for lane in &mut self.lanes {
                // Each lane samples where its clock falls in this sample,
                // between the previous value and this one.
                while lane.next <= t0 {
                    let frac = (lane.next - (t0 - 1.0)) as f32;
                    let y = self.prev + (v - self.prev) * frac.clamp(0.0, 1.0);
                    lane.next += self.sps;
                    let level = y > 0.0;
                    let bit = level == lane.level;
                    lane.level = level;
                    match lane.hdlc.push(bit) {
                        Some(Ok(frame)) => {
                            if !self.recent.iter().any(|(f, at)| *f == frame && t0 - at < self.sps * 600.0) {
                                match parse(&frame) {
                                    Some(msg) => {
                                        self.good += 1;
                                        out.push(AisFrame { channel: self.name, msg });
                                    }
                                    None => self.odd += 1,
                                }
                                self.recent.push((frame, t0));
                                if self.recent.len() > 16 {
                                    self.recent.remove(0);
                                }
                            }
                        }
                        Some(Err(())) => self.bad += 1,
                        None => {}
                    }
                }
            }
            self.prev = v;
            self.n += 1.0;
        }
        // Keep the clock's numbers small: shift everything back together.
        if self.n > 1e9 {
            let k = self.n.floor();
            self.n -= k;
            for l in &mut self.lanes {
                l.next -= k;
            }
            for r in &mut self.recent {
                r.1 -= k;
            }
        }
    }
}

struct Lane {
    next: f64,
    level: bool,
    hdlc: Hdlc,
}

impl Lane {
    fn new(phase: f64) -> Self {
        Self { next: phase, level: false, hdlc: Hdlc::default() }
    }
}

/// HDLC deframing of NRZI-decoded bits: flags, bit-stuffing, LSB-first
/// octets, and the X.25 frame check. A frame too short to be AIS is not
/// counted as a failure.
#[derive(Default)]
struct Hdlc {
    ones: u8,
    in_frame: bool,
    skip_zero: bool,
    buf: Vec<u8>,
    byte: u8,
    bits: u8,
}

impl Hdlc {
    fn push(&mut self, bit: bool) -> Option<Result<Vec<u8>, ()>> {
        if bit {
            self.ones += 1;
            if self.ones >= 7 {
                self.abort();
                return None;
            }
            if self.ones == 6 {
                // A flag: it closes the frame before it and opens the next.
                let done = if self.in_frame && self.buf.len() >= 8 {
                    let n = self.buf.len();
                    let fcs = u16::from_le_bytes([self.buf[n - 2], self.buf[n - 1]]);
                    Some(if crc16(&self.buf[..n - 2]) == fcs { Ok(self.buf[..n - 2].to_vec()) } else { Err(()) })
                } else {
                    None
                };
                self.buf.clear();
                self.byte = 0;
                self.bits = 0;
                self.ones = 0;
                self.in_frame = true;
                self.skip_zero = true;
                return done;
            }
        } else {
            if self.ones == 5 {
                // A stuffed zero.
                self.ones = 0;
                return None;
            }
            if self.skip_zero {
                self.skip_zero = false;
                self.ones = 0;
                return None;
            }
            self.ones = 0;
        }
        if !self.in_frame {
            return None;
        }
        if bit {
            self.byte |= 1 << self.bits;
        }
        self.bits += 1;
        if self.bits == 8 {
            self.buf.push(self.byte);
            self.byte = 0;
            self.bits = 0;
            // Five slots at most: 1280 bits.
            if self.buf.len() > 160 {
                self.abort();
            }
        }
        None
    }

    fn abort(&mut self) {
        self.in_frame = false;
        self.skip_zero = false;
        self.buf.clear();
        self.byte = 0;
        self.bits = 0;
        self.ones = 0;
    }
}

/// The X.25 frame check: CRC-16-CCITT, reflected, init and final inverted.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &b in data {
        crc ^= u16::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x8408 } else { crc >> 1 };
        }
    }
    !crc
}

/// The message's bits, read most significant first across its octets.
struct Bits<'a>(&'a [u8]);

impl Bits<'_> {
    fn len(&self) -> usize {
        self.0.len() * 8
    }
    fn u(&self, start: usize, n: usize) -> Option<u32> {
        if start + n > self.len() || n > 32 {
            return None;
        }
        let mut v = 0u32;
        for i in start..start + n {
            v = (v << 1) | u32::from((self.0[i / 8] >> (7 - i % 8)) & 1);
        }
        Some(v)
    }
    fn i(&self, start: usize, n: usize) -> Option<i32> {
        let v = self.u(start, n)?;
        let shift = 32 - n as u32;
        Some(((v << shift) as i32) >> shift)
    }
    /// Six-bit text: 0–31 is '@'–'_', 32–63 is ' '–'?'; '@' pads.
    fn text(&self, start: usize, chars: usize) -> Option<String> {
        let mut s = String::new();
        for k in 0..chars {
            let Some(c) = self.u(start + 6 * k, 6) else { break };
            let c = c as u8;
            s.push(if c < 32 { (c + 64) as char } else { c as char });
        }
        let s = s.trim_end_matches(['@', ' ']).trim().to_string();
        (!s.is_empty()).then_some(s)
    }
}

/// Position in 1/10000 minute: 181° and 91° mean none.
fn lon_lat(b: &Bits, lon_at: usize, lat_at: usize) -> (Option<f64>, Option<f64>) {
    let lon = b.i(lon_at, 28).map(|v| v as f64 / 600_000.0).filter(|v| v.abs() <= 180.0);
    let lat = b.i(lat_at, 27).map(|v| v as f64 / 600_000.0).filter(|v| v.abs() <= 90.0);
    (lon, lat)
}

fn sog(b: &Bits, at: usize) -> Option<f32> {
    b.u(at, 10).filter(|v| *v < 1023).map(|v| v as f32 / 10.0)
}

fn cog(b: &Bits, at: usize) -> Option<f32> {
    b.u(at, 12).filter(|v| *v < 3600).map(|v| v as f32 / 10.0)
}

fn heading(b: &Bits, at: usize) -> Option<u16> {
    b.u(at, 9).filter(|v| *v < 360).map(|v| v as u16)
}

/// Whether a frame is as long as its message type is (ITU-R M.1371): a
/// sixteen-bit check passes one noise frame in 65536, and the length and
/// the MMSI are what tell those from the air. Lengths are whole octets
/// here, so a type's bit count is rounded up to the octet.
fn plausible_length(kind: u8, octets: usize) -> bool {
    let bits = octets * 8;
    match kind {
        1..=4 | 9 | 11 | 18 => bits == 168,
        5 => (424..=432).contains(&bits),
        19 => bits == 312,
        21 => (272..=368).contains(&bits),
        24 => bits == 160 || bits == 168,
        27 => bits == 96,
        10 => bits == 72,
        22 | 23 => bits == 168 || bits == 160,
        7 | 13 => (72..=168).contains(&bits),
        6 | 8 | 12 | 14 | 25 | 26 => (40..=1008).contains(&bits),
        15 => (88..=168).contains(&bits),
        16 => bits == 96 || bits == 144,
        17 => (80..=816).contains(&bits),
        20 => (72..=160).contains(&bits),
        _ => false,
    }
}

/// Whether an MMSI is one a station can have: a ship's begins with a
/// country's MID (201–775), a coast station's with 00, a group's with 0,
/// an aid's with 99, a SAR aircraft's with 111, and SARTs, MOB and EPIRB
/// beacons with 970, 972 and 974.
pub fn plausible_mmsi(m: u32) -> bool {
    let mid3 = m / 1_000_000;
    let mid = |x: u32| (201..=775).contains(&x);
    mid(mid3) || (m < 10_000_000 && mid(m / 10_000)) || (m < 100_000_000 && mid(m / 100_000)) || (m / 10_000_000 == 99 && mid((m / 10_000) % 1000)) || (m / 1_000_000 == 111 && mid((m / 1_000) % 1000)) || matches!(mid3, 970 | 972 | 974)
}

/// Parse an AIS frame's payload (the octets inside the HDLC frame); `None`
/// for a frame no AIS station would send.
pub fn parse(frame: &[u8]) -> Option<AisMessage> {
    let b = Bits(frame);
    let kind = b.u(0, 6)? as u8;
    let mmsi = b.u(8, 30)?;
    if !plausible_length(kind, frame.len()) || !plausible_mmsi(mmsi) {
        return None;
    }
    let mut m = AisMessage { kind, mmsi, ..Default::default() };
    match kind {
        1..=3 => {
            m.status = b.u(38, 4).map(|v| v as u8);
            m.sog_kn = sog(&b, 50);
            (m.lon, m.lat) = lon_lat(&b, 61, 89);
            m.cog = cog(&b, 116);
            m.heading = heading(&b, 128);
        }
        4 | 11 => {
            if let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) = (b.u(38, 14), b.u(52, 4), b.u(56, 5), b.u(61, 5), b.u(66, 6), b.u(72, 6))
                && y > 0
                && mo > 0
            {
                m.utc = Some(format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}"));
            }
            (m.lon, m.lat) = lon_lat(&b, 79, 107);
        }
        5 => {
            m.imo = b.u(40, 30).filter(|v| *v > 0);
            m.callsign = b.text(70, 7);
            m.name = b.text(112, 20);
            m.ship_type = b.u(232, 8).map(|v| v as u8);
            if let (Some(a), Some(bb), Some(c), Some(d)) = (b.u(240, 9), b.u(249, 9), b.u(258, 6), b.u(264, 6)) {
                m.dims = Some((a as u16, bb as u16, c as u8, d as u8));
            }
            if let (Some(mo), Some(d), Some(h), Some(mi)) = (b.u(274, 4), b.u(278, 5), b.u(283, 5), b.u(288, 6))
                && mo > 0
                && d > 0
            {
                m.eta = Some(format!("{mo:02}-{d:02} {h:02}:{mi:02}"));
            }
            m.draught_m = b.u(294, 8).filter(|v| *v > 0).map(|v| v as f32 / 10.0);
            m.destination = b.text(302, 20);
        }
        9 => {
            m.altitude_m = b.u(38, 12).filter(|v| *v < 4095).map(|v| v as u16);
            m.sog_kn = b.u(50, 10).filter(|v| *v < 1023).map(|v| v as f32);
            (m.lon, m.lat) = lon_lat(&b, 61, 89);
            m.cog = cog(&b, 116);
        }
        18 | 19 => {
            m.sog_kn = sog(&b, 46);
            (m.lon, m.lat) = lon_lat(&b, 57, 85);
            m.cog = cog(&b, 112);
            m.heading = heading(&b, 124);
            if kind == 19 {
                m.name = b.text(143, 20);
                m.ship_type = b.u(263, 8).map(|v| v as u8);
            }
        }
        21 => {
            m.aid_type = b.u(38, 5).map(|v| v as u8);
            m.name = b.text(43, 20);
            (m.lon, m.lat) = lon_lat(&b, 164, 192);
        }
        24 => match b.u(38, 2)? {
            0 => m.name = b.text(40, 20),
            1 => {
                m.ship_type = b.u(40, 8).map(|v| v as u8);
                m.callsign = b.text(90, 7);
            }
            _ => {}
        },
        27 => {
            m.status = b.u(40, 4).map(|v| v as u8);
            m.lon = b.i(44, 18).map(|v| v as f64 / 600.0).filter(|v| v.abs() <= 180.0);
            m.lat = b.i(62, 17).map(|v| v as f64 / 600.0).filter(|v| v.abs() <= 90.0);
            m.sog_kn = b.u(79, 6).filter(|v| *v < 63).map(|v| v as f32);
            m.cog = b.u(85, 9).filter(|v| *v < 511).map(|v| v as f32);
        }
        _ => {}
    }
    Some(m)
}

/// A message type's name.
pub fn kind_name(kind: u8) -> &'static str {
    match kind {
        1..=3 => "position report",
        4 => "base station",
        5 => "static and voyage",
        6 | 8 | 25 | 26 => "binary message",
        7 | 13 => "acknowledgement",
        9 => "SAR aircraft",
        10 | 11 => "UTC report",
        12 | 14 => "safety message",
        15 | 16 | 17 | 20 | 22 | 23 => "channel management",
        18 => "class B position",
        19 => "class B extended",
        21 => "aid to navigation",
        24 => "class B static",
        27 => "long-range position",
        _ => "message",
    }
}

/// A navigational status's meaning.
pub fn status_name(s: u8) -> &'static str {
    match s {
        0 => "under way",
        1 => "at anchor",
        2 => "not under command",
        3 => "restricted maneuverability",
        4 => "constrained by draught",
        5 => "moored",
        6 => "aground",
        7 => "fishing",
        8 => "under way sailing",
        14 => "AIS-SART active",
        _ => "",
    }
}

/// A ship type code's meaning, by its decade.
pub fn ship_type_name(t: u8) -> &'static str {
    match t {
        20..=29 => "wing in ground",
        30 => "fishing",
        31 | 32 => "towing",
        33 => "dredging",
        34 => "diving",
        35 => "military",
        36 => "sailing",
        37 => "pleasure craft",
        40..=49 => "high-speed craft",
        50 => "pilot vessel",
        51 => "search and rescue",
        52 => "tug",
        53 => "port tender",
        55 => "law enforcement",
        58 => "medical transport",
        60..=69 => "passenger",
        70..=79 => "cargo",
        80..=89 => "tanker",
        90..=99 => "other",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An NMEA AIVDM payload's six-bit armor, back to octets.
    fn unarmor(s: &str) -> Vec<u8> {
        let mut bits = Vec::new();
        for c in s.bytes() {
            let mut v = c - 48;
            if v > 40 {
                v -= 8;
            }
            for k in (0..6).rev() {
                bits.push((v >> k) & 1);
            }
        }
        bits.chunks(8).map(|ch| ch.iter().enumerate().fold(0u8, |a, (i, b)| a | (b << (7 - i)))).collect()
    }

    #[test]
    fn a_class_a_position_parses() {
        // !AIVDM,1,1,,A,15RTgt0PAso;90TKcjM8h6g208CQ,0*4A
        let m = parse(&unarmor("15RTgt0PAso;90TKcjM8h6g208CQ")).unwrap();
        assert_eq!((m.kind, m.mmsi), (1, 371_798_000));
        assert_eq!(m.sog_kn, Some(12.3));
        assert!((m.lon.unwrap() + 123.395_383).abs() < 1e-5, "{:?}", m.lon);
        assert!((m.lat.unwrap() - 48.381_633).abs() < 1e-5, "{:?}", m.lat);
        assert_eq!((m.cog, m.heading), (Some(224.0), Some(215)));
    }

    #[test]
    fn noise_shaped_frames_are_not_taken_for_ships() {
        // The two frames heard on the first live run: a class B extended
        // report 21 octets long (it is 39), and an MMSI no country has.
        let mut f = unarmor("15RTgt0PAso;90TKcjM8h6g208CQ");
        f[0] = (19 << 2) | (f[0] & 3);
        assert!(parse(&f).is_none());
        assert!(!plausible_mmsi(873_117_578));
        assert!(plausible_mmsi(371_798_000) && plausible_mmsi(3_669_999) && plausible_mmsi(993_669_001) && plausible_mmsi(970_123_456) && plausible_mmsi(111_366_001));
    }

    #[test]
    fn a_binary_message_needs_its_sender_heard_first() {
        let rate = 960_000.0;
        let pos = unarmor("15RTgt0PAso;90TKcjM8h6g208CQ");
        // A type 8 broadcast from the same MMSI: 56 bits of header and data.
        let mut bin = pos[..7].to_vec();
        bin[0] = (8 << 2) | (bin[0] & 3);
        bin.extend([0x12, 0x34, 0x56]);
        let run = |frames: &[&[u8]]| {
            let mut iq = vec![Complex32::new(0.0, 0.0); 10_000];
            for f in frames {
                iq.extend(burst(f, rate, AIS_A_HZ - AIS_MID_HZ));
                iq.extend(vec![Complex32::new(0.0, 0.0); 10_000]);
            }
            let mut rx = AisReceiver::new(rate, 0.0);
            let mut out = Vec::new();
            for b in iq.chunks(8192) {
                rx.process(b, &mut out);
            }
            out.iter().map(|f| f.msg.kind).collect::<Vec<_>>()
        };
        assert_eq!(run(&[&bin]), Vec::<u8>::new(), "a stranger's binary message is held back");
        assert_eq!(run(&[&pos, &bin]), vec![1, 8], "once its position is heard, it is taken");
    }

    #[test]
    fn six_bit_text_reads_and_trims() {
        // "AB 1@@" in six-bit: A=1, B=2, ' '=32, '1'=49, '@'=0.
        let vals = [1u8, 2, 32, 49, 0, 0];
        let mut bits = Vec::new();
        for v in vals {
            for k in (0..6).rev() {
                bits.push((v >> k) & 1);
            }
        }
        let bytes: Vec<u8> = bits.chunks(8).map(|ch| ch.iter().enumerate().fold(0u8, |a, (i, b)| a | (b << (7 - i)))).collect();
        assert_eq!(Bits(&bytes).text(0, 6).as_deref(), Some("AB 1"));
    }

    /// An AIS burst at `rate`: training, flag, the stuffed frame with its
    /// FCS, flag, NRZI, Gaussian-filtered (BT 0.4) frequency, `offset_hz`
    /// off zero.
    fn burst(payload: &[u8], rate: f64, offset_hz: f64) -> Vec<Complex32> {
        let mut frame = payload.to_vec();
        frame.extend_from_slice(&crc16(payload).to_le_bytes());
        let mut bits: Vec<bool> = (0..24).map(|i| i % 2 == 1).collect();
        let flag = |bits: &mut Vec<bool>| (0..8).for_each(|i| bits.push((0x7Eu8 >> i) & 1 == 1));
        flag(&mut bits);
        let mut ones = 0;
        for &byte in &frame {
            for i in 0..8 {
                let b = (byte >> i) & 1 == 1;
                bits.push(b);
                ones = if b { ones + 1 } else { 0 };
                if ones == 5 {
                    bits.push(false);
                    ones = 0;
                }
            }
        }
        flag(&mut bits);
        bits.extend([false; 8]);
        // NRZI: a zero changes the level.
        let mut level = 1.0f32;
        let nrz: Vec<f32> = bits
            .iter()
            .map(|&b| {
                if !b {
                    level = -level;
                }
                level
            })
            .collect();
        let sps = rate / BAUD;
        let n = (nrz.len() as f64 * sps) as usize;
        let raw: Vec<f32> = (0..n).map(|i| nrz[((i as f64) / sps) as usize]).collect();
        // Gaussian pulse shaping, BT 0.4.
        let sigma = (2f64.ln()).sqrt() / (2.0 * std::f64::consts::PI * 0.4) * sps;
        let half = (3.0 * sigma) as i64;
        let taps: Vec<f32> = (-half..=half).map(|k| (-(k as f64).powi(2) / (2.0 * sigma * sigma)).exp() as f32).collect();
        let sum: f32 = taps.iter().sum();
        let mut phase = 0.0f64;
        (0..n)
            .map(|i| {
                let f: f32 = taps.iter().enumerate().map(|(j, t)| t * raw[(i as i64 + j as i64 - half).clamp(0, n as i64 - 1) as usize]).sum::<f32>() / sum;
                phase += 2.0 * std::f64::consts::PI * (f64::from(f * DEVIATION_HZ) + offset_hz) / rate;
                Complex32::from_polar(1.0, phase as f32)
            })
            .collect()
    }

    #[test]
    fn a_burst_on_each_channel_is_heard_through_the_span() {
        let rate = 960_000.0;
        let a = unarmor("15RTgt0PAso;90TKcjM8h6g208CQ");
        let mut b = a.clone();
        b[1] ^= 0x01; // another MMSI
        // 162.000 MHz at the span's center; quiet, a burst on A, quiet, B.
        let mut iq = vec![Complex32::new(0.0, 0.0); 20_000];
        iq.extend(burst(&a, rate, AIS_A_HZ - AIS_MID_HZ));
        iq.extend(vec![Complex32::new(0.0, 0.0); 20_000]);
        iq.extend(burst(&b, rate, AIS_B_HZ - AIS_MID_HZ));
        iq.extend(vec![Complex32::new(0.0, 0.0); 20_000]);
        // A little noise, from a fixed sequence.
        let mut seed = 1u32;
        for s in &mut iq {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let r = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            *s += Complex32::new(r * 0.05, -r * 0.05);
        }
        let mut rx = AisReceiver::new(rate, 0.0);
        let mut out = Vec::new();
        for block in iq.chunks(8192) {
            rx.process(block, &mut out);
        }
        let got: Vec<(char, u32)> = out.iter().map(|f| (f.channel, f.msg.mmsi)).collect();
        let want_b = parse(&b).unwrap().mmsi;
        assert_eq!(got, vec![('A', 371_798_000), ('B', want_b)], "counts {:?}", rx.counts());
    }

    #[test]
    fn a_burst_is_copied_at_fifteen_db_in_the_channel() {
        // Gaussian noise across the span, so the channel filter sees about
        // 15 dB SNR (noise over 17 kHz of a 960 kHz span). A discriminator
        // receiver's packets start to fail near 12 dB (6 in 10 copied there
        // when this was written), as real AIS receivers' do.
        let rate = 960_000.0;
        let a = unarmor("15RTgt0PAso;90TKcjM8h6g208CQ");
        let sigma = (0.0316f64 / (2.0 * 17_000.0 / rate)).sqrt() as f32;
        let mut seed = 7u64;
        let mut gauss = || {
            let mut u = || {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                ((seed >> 11) as f64 / (1u64 << 53) as f64).max(1e-12)
            };
            let (u1, u2) = (u(), u());
            ((-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()) as f32
        };
        let mut copied = 0;
        for _ in 0..10 {
            let mut iq = vec![Complex32::new(0.0, 0.0); 10_000];
            iq.extend(burst(&a, rate, AIS_A_HZ - AIS_MID_HZ));
            iq.extend(vec![Complex32::new(0.0, 0.0); 10_000]);
            for s in &mut iq {
                *s += Complex32::new(gauss() * sigma, gauss() * sigma);
            }
            let mut rx = AisReceiver::new(rate, 0.0);
            let mut out = Vec::new();
            for b in iq.chunks(8192) {
                rx.process(b, &mut out);
            }
            copied += out.iter().filter(|f| f.msg.mmsi == 371_798_000).count().min(1);
        }
        assert!(copied >= 9, "copied {copied} of 10 bursts at 15 dB");
    }

    #[test]
    fn the_receiver_knows_when_the_channels_are_in_the_span() {
        assert!(AisReceiver::hears(162.0e6, 2e6));
        assert!(AisReceiver::hears(161.5e6, 2e6));
        assert!(!AisReceiver::hears(146.0e6, 2e6));
    }
}
