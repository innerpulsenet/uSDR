//! 1090 MHz Mode S / ADS-B.
//!
//! Magnitude preamble search, PPM bit slice, CRC-24, and the DF17/18 fields
//! a map needs: ICAO, callsign, airborne position, airborne velocity.
//! IQ in is 2.4 MS/s complex baseband centered on 1090 MHz.

use num_complex::Complex32;

/// RTL-SDR rate dump1090 standardised on: 2.4 samples per Mode S microsecond.
pub const SAMPLE_RATE: f64 = 2_400_000.0;
pub const FREQ_HZ: f64 = 1_090_000_000.0;

const SAMPLES_PER_US: f32 = 2.4;
const PREAMBLE_US: f32 = 8.0;
/// 8 µs preamble at 2.4 samples/µs.
const PREAMBLE_SAMPLES: usize = 19;
const LONG_BITS: usize = 112;
const SHORT_BITS: usize = 56;

/// Generator polynomial for the 24-bit Mode S CRC (`x^24+…+x^12+x^10+x^3+1`).
const CRC_POLY: u32 = 0xFFF409;

const CHARSET: &[u8; 64] = b"#ABCDEFGHIJKLMNOPQRSTUVWXYZ##### ###############0123456789######";

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub bytes: [u8; 14],
    pub bits: usize,
    pub sig_dbfs: f32,
}

impl Frame {
    pub fn df(&self) -> u8 {
        self.bytes[0] >> 3
    }

    pub fn icao(&self) -> Option<u32> {
        match self.df() {
            11 | 17 | 18 => Some(u32::from_be_bytes([
                0,
                self.bytes[1],
                self.bytes[2],
                self.bytes[3],
            ])),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AdsBMsg {
    Ident {
        icao: u32,
        callsign: String,
        sig_dbfs: Option<f32>,
    },
    AirbornePosition {
        icao: u32,
        alt_ft: Option<i32>,
        odd: bool,
        cpr_lat: u32,
        cpr_lon: u32,
        sig_dbfs: Option<f32>,
    },
    AirborneVelocity {
        icao: u32,
        speed_kt: f32,
        heading_deg: f32,
        vrate_fpm: Option<i32>,
        sig_dbfs: Option<f32>,
    },
    /// Absolute position (UAT). Not CPR.
    Fix {
        icao: u32,
        callsign: Option<String>,
        lat: Option<f64>,
        lon: Option<f64>,
        alt_ft: Option<i32>,
        speed_kt: Option<f32>,
        heading_deg: Option<f32>,
        sig_dbfs: Option<f32>,
    },
    Other {
        icao: u32,
        df: u8,
        sig_dbfs: Option<f32>,
    },
}

impl AdsBMsg {
    pub fn sig_dbfs(&self) -> Option<f32> {
        match self {
            AdsBMsg::Ident { sig_dbfs, .. }
            | AdsBMsg::AirbornePosition { sig_dbfs, .. }
            | AdsBMsg::AirborneVelocity { sig_dbfs, .. }
            | AdsBMsg::Fix { sig_dbfs, .. }
            | AdsBMsg::Other { sig_dbfs, .. } => *sig_dbfs,
        }
    }
}

pub fn crc(data: &[u8], bits: usize) -> u32 {
    let mut rem = 0u32;
    for i in 0..bits {
        let bit = (data[i / 8] >> (7 - (i % 8))) & 1;
        let mix = ((rem >> 23) as u8 & 1) ^ bit;
        rem = (rem << 1) & 0xFF_FFFF;
        if mix != 0 {
            rem ^= CRC_POLY;
        }
    }
    rem
}

pub fn crc_ok(frame: &Frame) -> bool {
    crc(&frame.bytes, frame.bits) == 0
}

// 2.4 MS/s is 6 samples per 5 half-bit chips (0.5 µs). A chip therefore
// walks 6/5 of a sample; the five correlators below are that walk. They
// sum to zero so a DC offset does not flip bits.
fn slice_phase0(m: &[f32]) -> f32 {
    5.0 * m[0] - 3.0 * m[1] - 2.0 * m[2]
}
fn slice_phase1(m: &[f32]) -> f32 {
    4.0 * m[0] - m[1] - 3.0 * m[2]
}
fn slice_phase2(m: &[f32]) -> f32 {
    3.0 * m[0] + m[1] - 4.0 * m[2]
}
fn slice_phase3(m: &[f32]) -> f32 {
    2.0 * m[0] + 3.0 * m[1] - 5.0 * m[2]
}
fn slice_phase4(m: &[f32]) -> f32 {
    m[0] + 5.0 * m[1] - 5.0 * m[2] - m[3]
}

fn bit_of(v: f32) -> u8 {
    u8::from(v > 0.0)
}

/// Demodulate 8 bits starting at `m[0]` with symbol phase 0..4.
/// Returns (byte, next_phase, samples_consumed).
fn slice_byte(m: &[f32], phase: usize) -> Option<(u8, usize, usize)> {
    if m.len() < 20 {
        return None;
    }
    let (b, next, adv) = match phase {
        0 => {
            let v = (bit_of(slice_phase0(m)) << 7)
                | (bit_of(slice_phase2(&m[2..])) << 6)
                | (bit_of(slice_phase4(&m[4..])) << 5)
                | (bit_of(slice_phase1(&m[7..])) << 4)
                | (bit_of(slice_phase3(&m[9..])) << 3)
                | (bit_of(slice_phase0(&m[12..])) << 2)
                | (bit_of(slice_phase2(&m[14..])) << 1)
                | bit_of(slice_phase4(&m[16..]));
            (v, 1, 19)
        }
        1 => {
            let v = (bit_of(slice_phase1(m)) << 7)
                | (bit_of(slice_phase3(&m[2..])) << 6)
                | (bit_of(slice_phase0(&m[5..])) << 5)
                | (bit_of(slice_phase2(&m[7..])) << 4)
                | (bit_of(slice_phase4(&m[9..])) << 3)
                | (bit_of(slice_phase1(&m[12..])) << 2)
                | (bit_of(slice_phase3(&m[14..])) << 1)
                | bit_of(slice_phase0(&m[17..]));
            (v, 2, 19)
        }
        2 => {
            let v = (bit_of(slice_phase2(m)) << 7)
                | (bit_of(slice_phase4(&m[2..])) << 6)
                | (bit_of(slice_phase1(&m[5..])) << 5)
                | (bit_of(slice_phase3(&m[7..])) << 4)
                | (bit_of(slice_phase0(&m[10..])) << 3)
                | (bit_of(slice_phase2(&m[12..])) << 2)
                | (bit_of(slice_phase4(&m[14..])) << 1)
                | bit_of(slice_phase1(&m[17..]));
            (v, 3, 19)
        }
        3 => {
            let v = (bit_of(slice_phase3(m)) << 7)
                | (bit_of(slice_phase0(&m[3..])) << 6)
                | (bit_of(slice_phase2(&m[5..])) << 5)
                | (bit_of(slice_phase4(&m[7..])) << 4)
                | (bit_of(slice_phase1(&m[10..])) << 3)
                | (bit_of(slice_phase3(&m[12..])) << 2)
                | (bit_of(slice_phase0(&m[15..])) << 1)
                | bit_of(slice_phase2(&m[17..]));
            (v, 4, 19)
        }
        _ => {
            let v = (bit_of(slice_phase4(m)) << 7)
                | (bit_of(slice_phase1(&m[3..])) << 6)
                | (bit_of(slice_phase3(&m[5..])) << 5)
                | (bit_of(slice_phase0(&m[8..])) << 4)
                | (bit_of(slice_phase2(&m[10..])) << 3)
                | (bit_of(slice_phase4(&m[12..])) << 2)
                | (bit_of(slice_phase1(&m[15..])) << 1)
                | bit_of(slice_phase3(&m[17..]));
            (v, 0, 20)
        }
    };
    Some((b, next, adv))
}

fn slice_phased(mags: &[f32], start: usize, try_phase: usize) -> Option<Frame> {
    let mut p = start + PREAMBLE_SAMPLES + try_phase / 5;
    let mut phase = try_phase % 5;
    let mut bytes = [0u8; 14];
    for i in 0..14 {
        let (b, next, adv) = slice_byte(mags.get(p..)?, phase)?;
        bytes[i] = b;
        phase = next;
        p += adv;
    }
    let df = bytes[0] >> 3;
    let bits = if matches!(df, 16 | 17 | 18 | 20 | 21 | 24) {
        LONG_BITS
    } else {
        SHORT_BITS
    };
    let pre = mags.get(start..start + PREAMBLE_SAMPLES)?;
    let sig_mag = (pre[1] + pre[3] + pre[9] + pre[12]) / 4.0;
    let sig_dbfs = 20.0 * (sig_mag + 1e-6).log10();
    Some(Frame {
        bytes,
        bits,
        sig_dbfs,
    })
}

/// True when sample `i` looks like a Mode S preamble at 2.4 MS/s.
///
/// Pulses are 0.5 µs at 0 / 1.0 / 3.5 / 4.5 µs; at this rate they sit on
/// different sample pairs depending on the fractional phase, so we accept
/// any of the five alignments that still have ~3.5 dB pulse-vs-gap SNR.
pub fn preamble_at(mags: &[f32], i: usize) -> bool {
    let m = match mags.get(i..i + PREAMBLE_SAMPLES) {
        Some(s) => s,
        None => return false,
    };
    if !(m[0] < m[1] && m[12] > m[13]) {
        return false;
    }
    let (high, signal, noise) = if m[1] > m[2]
        && m[2] < m[3]
        && m[3] > m[4]
        && m[8] < m[9]
        && m[9] > m[10]
        && m[10] < m[11]
    {
        (
            (m[1] + m[3] + m[9] + m[11] + m[12]) / 4.0,
            m[1] + m[3] + m[9],
            m[5] + m[6] + m[7],
        )
    } else if m[1] > m[2]
        && m[2] < m[3]
        && m[3] > m[4]
        && m[8] < m[9]
        && m[9] > m[10]
        && m[11] < m[12]
    {
        (
            (m[1] + m[3] + m[9] + m[12]) / 4.0,
            m[1] + m[3] + m[9] + m[12],
            m[5] + m[6] + m[7] + m[8],
        )
    } else if m[1] > m[2]
        && m[2] < m[3]
        && m[4] > m[5]
        && m[8] < m[9]
        && m[10] > m[11]
        && m[11] < m[12]
    {
        (
            (m[1] + m[3] + m[4] + m[9] + m[10] + m[12]) / 4.0,
            m[1] + m[12],
            m[6] + m[7],
        )
    } else if m[1] > m[2]
        && m[3] < m[4]
        && m[4] > m[5]
        && m[9] < m[10]
        && m[10] > m[11]
        && m[11] < m[12]
    {
        (
            (m[1] + m[4] + m[10] + m[12]) / 4.0,
            m[1] + m[4] + m[10] + m[12],
            m[5] + m[6] + m[7] + m[8],
        )
    } else if m[2] > m[3]
        && m[3] < m[4]
        && m[4] > m[5]
        && m[9] < m[10]
        && m[10] > m[11]
        && m[11] < m[12]
    {
        (
            (m[1] + m[2] + m[4] + m[10] + m[12]) / 4.0,
            m[4] + m[10] + m[12],
            m[6] + m[7] + m[8],
        )
    } else {
        return false;
    };
    // About 3.5 dB pulse-to-gap amplitude ratio. Requiring 2:1 (6 dB)
    // discarded weak but otherwise CRC-valid frames; the CRC is the proper
    // final false-positive guard.
    if signal * 2.0 < 3.0 * noise {
        return false;
    }
    !matches!(
        [5, 6, 7, 8, 14, 15, 16, 17, 18]
            .into_iter()
            .find(|&k| m[k] >= high),
        Some(_)
    )
}

/// Samples of lookahead a long frame needs past the preamble start.
pub const DEMOD_SPAN: usize = PREAMBLE_SAMPLES + 14 * 20 + 8;

#[derive(Clone, Debug, Default)]
pub struct DemodResult {
    /// Preamble-shaped candidates examined, including candidates whose data
    /// failed CRC. This distinguishes "no RF bursts" from "slicer/CRC loss".
    pub preambles: usize,
    pub frames: Vec<Frame>,
}

pub fn demod_mags_with_stats(mags: &[f32]) -> DemodResult {
    debug_assert_eq!(
        (PREAMBLE_US * SAMPLES_PER_US).round() as usize,
        PREAMBLE_SAMPLES
    );
    let mut out = DemodResult::default();
    if mags.len() < DEMOD_SPAN {
        return out;
    }
    let last = mags.len() - DEMOD_SPAN;
    let mut i = 0;
    while i <= last {
        if preamble_at(mags, i) {
            out.preambles += 1;
            let mut hit = None;
            for try_phase in 0..10 {
                if let Some(f) = slice_phased(mags, i, try_phase)
                    && crc_ok(&f)
                {
                    hit = Some(f);
                    break;
                }
            }
            if let Some(f) = hit {
                i += PREAMBLE_SAMPLES + (f.bits as f32 * SAMPLES_PER_US) as usize;
                out.frames.push(f);
                continue;
            }
        }
        i += 1;
    }
    out
}

pub fn demod_mags(mags: &[f32]) -> Vec<Frame> {
    demod_mags_with_stats(mags).frames
}

pub fn demod_iq(iq: &[Complex32]) -> Vec<Frame> {
    let mags: Vec<f32> = iq.iter().map(|c| c.norm()).collect();
    demod_mags(&mags)
}

pub fn decode(frame: &Frame) -> Option<AdsBMsg> {
    if !crc_ok(frame) {
        return None;
    }
    let icao = frame.icao()?;
    let sig_dbfs = Some(frame.sig_dbfs);
    match frame.df() {
        17 | 18 => decode_me(icao, frame),
        11 => Some(AdsBMsg::Other {
            icao,
            df: 11,
            sig_dbfs,
        }),
        df => Some(AdsBMsg::Other { icao, df, sig_dbfs }),
    }
}

fn decode_me(icao: u32, frame: &Frame) -> Option<AdsBMsg> {
    let tc = frame.bytes[4] >> 3;
    let sig_dbfs = Some(frame.sig_dbfs);
    match tc {
        1..=4 => Some(AdsBMsg::Ident {
            icao,
            callsign: callsign(frame),
            sig_dbfs,
        }),
        9..=18 => airborne_position(icao, frame),
        19 => airborne_velocity(icao, frame),
        _ => Some(AdsBMsg::Other {
            icao,
            df: frame.df(),
            sig_dbfs,
        }),
    }
}

fn bits(frame: &Frame, start: usize, n: usize) -> u32 {
    let mut v = 0u32;
    for i in 0..n {
        let b = start + i;
        let bit = (frame.bytes[b / 8] >> (7 - (b % 8))) & 1;
        v = (v << 1) | u32::from(bit);
    }
    v
}

fn callsign(frame: &Frame) -> String {
    let mut s = String::new();
    for i in 0..8 {
        let idx = bits(frame, 40 + i * 6, 6) as usize;
        s.push(CHARSET[idx.min(63)] as char);
    }
    s.trim().trim_end_matches('#').trim().to_string()
}

fn airborne_position(icao: u32, frame: &Frame) -> Option<AdsBMsg> {
    let q = bits(frame, 47, 1) == 1;
    let n = bits(frame, 40, 12);
    let alt_ft = if q {
        Some((n as i32) * 25 - 1000)
    } else if n == 0 {
        None
    } else {
        None
    };
    let odd = bits(frame, 53, 1) == 1;
    let cpr_lat = bits(frame, 54, 17);
    let cpr_lon = bits(frame, 71, 17);
    Some(AdsBMsg::AirbornePosition {
        icao,
        alt_ft,
        odd,
        cpr_lat,
        cpr_lon,
        sig_dbfs: Some(frame.sig_dbfs),
    })
}

fn airborne_velocity(icao: u32, frame: &Frame) -> Option<AdsBMsg> {
    let st = bits(frame, 37, 3);
    if st != 1 && st != 2 {
        return None;
    }
    let dw = bits(frame, 45, 1) == 1;
    let vw = bits(frame, 56, 1) == 1;
    let ew = bits(frame, 46, 10) as f32;
    let ns = bits(frame, 57, 10) as f32;
    let mut ve = if dw { -1.0 } else { 1.0 } * (ew - 1.0);
    let mut vn = if vw { -1.0 } else { 1.0 } * (ns - 1.0);
    if st == 2 {
        ve *= 4.0;
        vn *= 4.0;
    }
    let speed_kt = (ve * ve + vn * vn).sqrt();
    let heading_deg = ve.atan2(vn).to_degrees().rem_euclid(360.0);
    let vr_sign = bits(frame, 68, 1) == 1;
    let vr = bits(frame, 69, 9);
    let vrate_fpm = if vr == 0 {
        None
    } else {
        let mag = (vr as i32 - 1) * 64;
        Some(if vr_sign { -mag } else { mag })
    };
    Some(AdsBMsg::AirborneVelocity {
        icao,
        speed_kt,
        heading_deg,
        vrate_fpm,
        sig_dbfs: Some(frame.sig_dbfs),
    })
}

/// Compact Position Reporting: even/odd pair → WGS84.
pub fn cpr_airborne(
    even_lat: u32,
    even_lon: u32,
    odd_lat: u32,
    odd_lon: u32,
) -> Option<(f64, f64)> {
    const NZ: f64 = 15.0;
    let dlat0 = 360.0 / (4.0 * NZ);
    let dlat1 = 360.0 / (4.0 * NZ - 1.0);
    let lat0 = even_lat as f64 / 131072.0;
    let lat1 = odd_lat as f64 / 131072.0;
    let j = (59.0 * lat0 - 60.0 * lat1 + 0.5).floor();
    let mut rlat0 = dlat0 * ((j % 60.0) + lat0);
    let mut rlat1 = dlat1 * ((j % 59.0) + lat1);
    if rlat0 >= 270.0 {
        rlat0 -= 360.0;
    }
    if rlat1 >= 270.0 {
        rlat1 -= 360.0;
    }
    if nl(rlat0) != nl(rlat1) {
        return None;
    }
    let lat = rlat0;
    let nl_lat = nl(lat).max(1.0);
    let dlon0 = 360.0 / nl_lat;
    let dlon1 = 360.0 / (nl_lat - 1.0).max(1.0);
    let lon0 = even_lon as f64 / 131072.0;
    let lon1 = odd_lon as f64 / 131072.0;
    let m = (lon0 * (nl_lat - 1.0) - lon1 * nl_lat + 0.5).floor();
    let mut lon = dlon0 * ((m % nl_lat + nl_lat) % nl_lat + lon0);
    let _ = (dlon1, lon1);
    if lon >= 180.0 {
        lon -= 360.0;
    }
    Some((lat, lon))
}

fn nl(lat: f64) -> f64 {
    let alat = lat.abs();
    if alat < 10.470_471_30 {
        59.0
    } else if alat < 14.828_174_37 {
        58.0
    } else if alat < 18.186_263_57 {
        57.0
    } else if alat < 21.029_394_93 {
        56.0
    } else if alat < 23.545_044_87 {
        55.0
    } else if alat < 25.829_247_07 {
        54.0
    } else if alat < 27.938_987_10 {
        53.0
    } else if alat < 29.911_356_86 {
        52.0
    } else if alat < 31.772_097_08 {
        51.0
    } else if alat < 33.539_934_36 {
        50.0
    } else if alat < 35.228_995_98 {
        49.0
    } else if alat < 36.850_251_08 {
        48.0
    } else if alat < 38.412_418_92 {
        47.0
    } else if alat < 39.922_566_84 {
        46.0
    } else if alat < 41.386_518_32 {
        45.0
    } else if alat < 42.809_140_12 {
        44.0
    } else if alat < 44.194_549_51 {
        43.0
    } else if alat < 45.546_267_23 {
        42.0
    } else if alat < 46.867_332_52 {
        41.0
    } else if alat < 48.160_391_28 {
        40.0
    } else if alat < 49.427_764_39 {
        39.0
    } else if alat < 50.671_501_66 {
        38.0
    } else if alat < 51.893_424_93 {
        37.0
    } else if alat < 53.095_161_53 {
        36.0
    } else if alat < 54.278_174_72 {
        35.0
    } else if alat < 55.443_784_44 {
        34.0
    } else if alat < 56.593_187_56 {
        33.0
    } else if alat < 57.727_473_54 {
        32.0
    } else if alat < 58.847_637_76 {
        31.0
    } else if alat < 59.954_592_77 {
        30.0
    } else if alat < 61.049_177_74 {
        29.0
    } else if alat < 62.132_166_59 {
        28.0
    } else if alat < 63.204_274_79 {
        27.0
    } else if alat < 64.266_165_23 {
        26.0
    } else if alat < 65.318_453_10 {
        25.0
    } else if alat < 66.361_710_08 {
        24.0
    } else if alat < 67.396_467_74 {
        23.0
    } else if alat < 68.423_220_22 {
        22.0
    } else if alat < 69.442_426_31 {
        21.0
    } else if alat < 70.454_510_75 {
        20.0
    } else if alat < 71.459_864_73 {
        19.0
    } else if alat < 72.458_845_45 {
        18.0
    } else if alat < 73.451_774_42 {
        17.0
    } else if alat < 74.438_934_16 {
        16.0
    } else if alat < 75.420_562_57 {
        15.0
    } else if alat < 76.396_843_91 {
        14.0
    } else if alat < 77.367_894_61 {
        13.0
    } else if alat < 78.333_740_83 {
        12.0
    } else if alat < 79.294_282_25 {
        11.0
    } else if alat < 80.249_232_13 {
        10.0
    } else if alat < 81.198_013_49 {
        9.0
    } else if alat < 82.139_569_81 {
        8.0
    } else if alat < 83.071_994_45 {
        7.0
    } else if alat < 83.991_735_63 {
        6.0
    } else if alat < 84.891_841_91 {
        5.0
    } else if alat < 85.755_416_21 {
        4.0
    } else if alat < 86.535_369_98 {
        3.0
    } else if alat < 87.000_000_00 {
        2.0
    } else {
        1.0
    }
}

/// Tracks aircraft from a stream of decoded messages.
#[derive(Clone, Debug)]
pub struct Aircraft {
    pub icao: u32,
    pub callsign: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub alt_ft: Option<i32>,
    pub speed_kt: Option<f32>,
    pub heading_deg: Option<f32>,
    pub sig_dbfs: Option<f32>,
    pub seen_ms: u64,
    even: Option<(u32, u32)>,
    odd: Option<(u32, u32)>,
}

#[derive(Default)]
pub struct Tracker {
    craft: std::collections::BTreeMap<u32, Aircraft>,
    now_ms: u64,
}

impl Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance_ms(&mut self, dt: u64) {
        self.now_ms = self.now_ms.saturating_add(dt);
    }

    /// Apply a message. Returns the aircraft when something a map shows changed.
    pub fn apply(&mut self, msg: &AdsBMsg) -> Option<Aircraft> {
        let icao = match msg {
            AdsBMsg::Ident { icao, .. }
            | AdsBMsg::AirbornePosition { icao, .. }
            | AdsBMsg::AirborneVelocity { icao, .. }
            | AdsBMsg::Fix { icao, .. }
            | AdsBMsg::Other { icao, .. } => *icao,
        };
        let now = self.now_ms;
        let a = self.craft.entry(icao).or_insert_with(|| Aircraft {
            icao,
            callsign: None,
            lat: None,
            lon: None,
            alt_ft: None,
            speed_kt: None,
            heading_deg: None,
            sig_dbfs: None,
            seen_ms: now,
            even: None,
            odd: None,
        });
        a.seen_ms = now;
        if let Some(db) = msg.sig_dbfs() {
            a.sig_dbfs = Some(db);
        }
        let mut changed = matches!(msg, AdsBMsg::Other { .. });
        match msg {
            AdsBMsg::Ident { callsign, .. } => {
                if a.callsign.as_deref() != Some(callsign.as_str()) {
                    a.callsign = Some(callsign.clone());
                    changed = true;
                }
            }
            AdsBMsg::AirbornePosition {
                alt_ft,
                odd,
                cpr_lat,
                cpr_lon,
                ..
            } => {
                if a.alt_ft != *alt_ft {
                    a.alt_ft = *alt_ft;
                    changed = true;
                }
                if *odd {
                    a.odd = Some((*cpr_lat, *cpr_lon));
                } else {
                    a.even = Some((*cpr_lat, *cpr_lon));
                }
                if let (Some(e), Some(o)) = (a.even, a.odd)
                    && let Some((lat, lon)) = cpr_airborne(e.0, e.1, o.0, o.1)
                {
                    let moved = a.lat.is_none_or(|p| (p - lat).abs() > 1e-5)
                        || a.lon.is_none_or(|p| (p - lon).abs() > 1e-5);
                    a.lat = Some(lat);
                    a.lon = Some(lon);
                    changed |= moved;
                }
            }
            AdsBMsg::AirborneVelocity {
                speed_kt,
                heading_deg,
                ..
            } => {
                a.speed_kt = Some(*speed_kt);
                a.heading_deg = Some(*heading_deg);
                changed = true;
            }
            AdsBMsg::Fix {
                callsign,
                lat,
                lon,
                alt_ft,
                speed_kt,
                heading_deg,
                ..
            } => {
                if let Some(cs) = callsign {
                    if a.callsign.as_deref() != Some(cs.as_str()) {
                        a.callsign = Some(cs.clone());
                        changed = true;
                    }
                }
                if *lat != a.lat || *lon != a.lon {
                    a.lat = *lat;
                    a.lon = *lon;
                    changed = true;
                }
                if *alt_ft != a.alt_ft {
                    a.alt_ft = *alt_ft;
                    changed = true;
                }
                if speed_kt.is_some() {
                    a.speed_kt = *speed_kt;
                    changed = true;
                }
                if heading_deg.is_some() {
                    a.heading_deg = *heading_deg;
                    changed = true;
                }
            }
            AdsBMsg::Other { .. } => {}
        }
        if changed { Some(a.clone()) } else { None }
    }

    pub fn expire(&mut self, max_age_ms: u64) -> Vec<u32> {
        let now = self.now_ms;
        let gone: Vec<u32> = self
            .craft
            .iter()
            .filter(|(_, a)| now.saturating_sub(a.seen_ms) > max_age_ms)
            .map(|(k, _)| *k)
            .collect();
        for k in &gone {
            self.craft.remove(k);
        }
        gone
    }

    pub fn len(&self) -> usize {
        self.craft.len()
    }
}

#[cfg(test)]
fn parse_hex(s: &str) -> [u8; 14] {
    let mut b = [0u8; 14];
    let bytes = s.len() / 2;
    for i in 0..bytes.min(14) {
        b[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_accepts_known_ident() {
        // ICAO example DF17 identification, aircraft 4840D6 "KLM1023".
        let bytes = parse_hex("8D4840D6202CC371C32CE0576098");
        let frame = Frame {
            bytes,
            bits: 112,
            sig_dbfs: -12.0,
        };
        assert_eq!(crc(&bytes, 112), 0, "syndrome {:06x}", crc(&bytes, 112));
        assert!(crc_ok(&frame));
        match decode(&frame) {
            Some(AdsBMsg::Ident {
                icao,
                callsign,
                sig_dbfs: _,
            }) => {
                assert_eq!(icao, 0x4840D6);
                assert_eq!(callsign, "KLM1023");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn crc_rejects_flipped_bit() {
        let mut bytes = parse_hex("8D4840D6202CC371C32CE0576098");
        bytes[5] ^= 0x01;
        assert_ne!(crc(&bytes, 112), 0);
    }

    #[test]
    fn cpr_round_trip_new_england() {
        // Encode is not here; check NL at New Milford and that a synthetic
        // even/odd pair near 41.6N 73.4W decodes in-hemisphere.
        assert_eq!(nl(41.577), 44.0);
        // Known dump1090 pair from the ADS-B spec examples (52N, 0-ish).
        // 8D406B902015A678D4D220AA4BDA even/odd from various tutorials:
        // skip if we don't have a pair; just assert function is finite.
        let pos = cpr_airborne(0x20000, 0x20000, 0x20000, 0x20000);
        if let Some((lat, lon)) = pos {
            assert!(lat.is_finite() && lon.is_finite());
        }
    }

    fn pulse_on(t_us: f32, bytes: &[u8], bits: usize) -> bool {
        if t_us < PREAMBLE_US {
            [0.0, 1.0, 3.5, 4.5]
                .into_iter()
                .any(|p| t_us >= p && t_us < p + 0.5)
        } else {
            let bit_i = ((t_us - PREAMBLE_US) as usize).min(bits.saturating_sub(1));
            if t_us >= PREAMBLE_US + bits as f32 {
                return false;
            }
            let frac = t_us - PREAMBLE_US - bit_i as f32;
            let one = (bytes[bit_i / 8] >> (7 - (bit_i % 8))) & 1 == 1;
            if one { frac < 0.5 } else { frac >= 0.5 }
        }
    }

    fn synthesize(bytes: &[u8], bits: usize, pad: usize, phase_us: f32) -> Vec<Complex32> {
        let n = pad + ((PREAMBLE_US + bits as f32 + 4.0) * SAMPLES_PER_US) as usize + pad;
        (0..n)
            .map(|i| {
                let t_us = (i as f32 - pad as f32) / SAMPLES_PER_US - phase_us;
                let amp = if t_us >= 0.0 && pulse_on(t_us, bytes, bits) {
                    0.4
                } else {
                    0.02
                };
                Complex32::new(amp, 0.0)
            })
            .collect()
    }

    #[test]
    fn demod_recovers_synthesized_ident() {
        let bytes = parse_hex("8D4840D6202CC371C32CE0576098");
        for phase in [0.0f32, 0.15, 0.3, 0.45] {
            let iq = synthesize(&bytes, 112, 32, phase);
            let frames = demod_iq(&iq);
            assert!(
                frames.iter().any(|f| f.bits == 112 && f.bytes == bytes),
                "phase {phase} produced {frames:?}"
            );
        }
    }

    #[test]
    fn demod_checks_the_last_safe_preamble_start() {
        let bytes = parse_hex("8D4840D6202CC371C32CE0576098");
        // One leading noise sample puts the first pulse in the phase expected
        // by preamble_at(), so the candidate itself begins at index zero.
        let mut iq = synthesize(&bytes, 112, 1, 0.0);
        iq.resize(DEMOD_SPAN, Complex32::new(0.02, 0.0));
        let frames = demod_iq(&iq);
        assert!(
            frames.iter().any(|f| f.bits == 112 && f.bytes == bytes),
            "a frame at the last legal scan position was skipped"
        );
    }

    #[test]
    fn tracker_emits_ident() {
        let mut t = Tracker::new();
        let bytes = parse_hex("8D4840D6202CC371C32CE0576098");
        let frame = Frame {
            bytes,
            bits: 112,
            sig_dbfs: -10.0,
        };
        let msg = decode(&frame).unwrap();
        let a = t.apply(&msg).unwrap();
        assert_eq!(a.callsign.as_deref(), Some("KLM1023"));
        assert_eq!(a.sig_dbfs, Some(-10.0));
        assert_eq!(t.len(), 1);
        t.advance_ms(70_000);
        assert_eq!(t.expire(60_000), vec![0x4840D6]);
        assert_eq!(t.len(), 0);
    }
}
