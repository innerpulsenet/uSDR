//! The FAC and SDC blocks once decoded (ES 201 980 clauses 6.3 and 6.4):
//! their CRCs, then the fields. Nothing is returned from a block whose CRC
//! fails.

use super::coding::{self, Bits};

/// FAC channel and service parameters (clause 6.3, robustness modes A-D).
#[derive(Clone, Debug, PartialEq)]
pub struct Fac {
    /// 0: first frame of the super frame (AFS index valid), 1: intermediate,
    /// 2: last, 3: first (AFS index invalid).
    pub identity: u8,
    pub so: u8,
    /// Long (2 s) interleaving; else short (400 ms).
    pub long_interleave: bool,
    /// 0: 64-QAM, 3: 16-QAM; 1 and 2 are reserved (they were the
    /// hierarchical modes of earlier versions and are not decoded).
    pub msc_mode: u8,
    /// SDC in 16-QAM; else 4-QAM.
    pub sdc_qam16: bool,
    pub audio_services: u8,
    pub data_services: u8,
    pub reconfiguration: u8,
    pub service: FacService,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FacService {
    pub id: u32,
    pub short_id: u8,
    pub audio_ca: bool,
    pub language: u8,
    pub data: bool,
    /// Program type of an audio service, application id of a data one.
    pub descriptor: u8,
    pub data_ca: bool,
}

impl Fac {
    /// Which frame of the super frame this is.
    pub fn frame(&self) -> u8 {
        match self.identity {
            1 => 1,
            2 => 2,
            _ => 0,
        }
    }
}

/// Parse the 72 FAC bits (energy dispersal already undone).
pub fn parse_fac(bits: &[u8]) -> Option<Fac> {
    if bits.len() < 72 {
        return None;
    }
    let bytes = coding::pack(&bits[..72]);
    if coding::crc8(&bytes[..8]) != bytes[8] {
        return None;
    }
    let mut b = Bits::new(bits);
    let _base_enhancement = b.take(1);
    let identity = b.take(2) as u8;
    let rm = b.take(1);
    let so = b.take(3) as u8;
    let long_interleave = b.take(1) == 0;
    let msc_mode = b.take(2) as u8;
    let sdc_qam16 = b.take(1) == 0;
    let nserv = b.take(4) as u8;
    let reconfiguration = b.take(3) as u8;
    let _toggle = b.take(1);
    let _rfu = b.take(1);
    if rm != 0 || so > 5 {
        // Mode E, or a reserved occupancy: not ours.
        return None;
    }
    let (audio_services, data_services) = match nserv {
        0 => (4, 0),
        1 => (0, 1),
        2 => (0, 2),
        3 => (0, 3),
        4 => (1, 0),
        5 => (1, 1),
        6 => (1, 2),
        7 => (1, 3),
        8 => (2, 0),
        9 => (2, 1),
        10 => (2, 2),
        12 => (3, 0),
        13 => (3, 1),
        15 => (0, 4),
        _ => return None,
    };
    let id = b.take(24);
    let short_id = b.take(2) as u8;
    let audio_ca = b.take(1) == 1;
    let language = b.take(4) as u8;
    let data = b.take(1) == 1;
    let descriptor = b.take(5) as u8;
    let data_ca = b.take(1) == 1;
    Some(Fac {
        identity,
        so,
        long_interleave,
        msc_mode,
        sdc_qam16,
        audio_services,
        data_services,
        reconfiguration,
        service: FacService { id, short_id, audio_ca, language, data, descriptor, data_ca },
    })
}

/// SDC type 0: the multiplex description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Multiplex {
    pub pl_a: u8,
    pub pl_b: u8,
    /// Bytes of each stream in parts A and B.
    pub streams: Vec<(usize, usize)>,
}

impl Multiplex {
    pub fn len_a(&self) -> usize {
        self.streams.iter().map(|s| s.0).sum()
    }

    /// The bytes of stream `id`, parts A and B, cut from a multiplex frame.
    pub fn stream(&self, frame: &[u8], id: usize) -> Option<(Vec<u8>, usize)> {
        let s = *self.streams.get(id)?;
        let a0: usize = self.streams[..id].iter().map(|s| s.0).sum();
        let b0: usize = self.len_a() + self.streams[..id].iter().map(|s| s.1).sum::<usize>();
        if b0 + s.1 > frame.len() || a0 + s.0 > frame.len() {
            return None;
        }
        let mut v = frame[a0..a0 + s.0].to_vec();
        v.extend_from_slice(&frame[b0..b0 + s.1]);
        Some((v, s.0))
    }
}

/// SDC type 9: how an audio service is coded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioInfo {
    pub short_id: u8,
    pub stream_id: u8,
    /// 0: AAC, 3: xHE-AAC; 1 and 2 reserved (CELP and HVXC in earlier
    /// versions of the standard).
    pub coding: u8,
    pub sbr: bool,
    /// 0: mono, 1: parametric stereo, 2: stereo.
    pub audio_mode: u8,
    /// The 3-bit sampling rate code, read per `coding`.
    pub rate_code: u8,
    pub text: bool,
    pub enhancement: bool,
    /// MPEG Surround mode (3 bits).
    pub mps: u8,
    /// The body from the audio coding field on: two bytes, then the
    /// codec specific configuration (xHE-AAC's static config).
    pub raw: Vec<u8>,
}

impl AudioInfo {
    /// The core coder's sampling rate, Hz, or `None` for a reserved code.
    pub fn sample_rate_hz(&self) -> Option<u32> {
        match self.coding {
            0 => match self.rate_code {
                1 => Some(12_000),
                3 => Some(24_000),
                5 => Some(48_000),
                _ => None,
            },
            3 => Some([9_600, 12_000, 16_000, 19_200, 24_000, 32_000, 38_400, 48_000][self.rate_code as usize & 7]),
            // CELP and HVXC (ES 201 980 V2): 8, 16 kHz codes.
            1 | 2 => match self.rate_code {
                0 => Some(8_000),
                2 => Some(16_000),
                _ => None,
            },
            _ => None,
        }
    }
}

/// SDC type 8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeDate {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    /// Local time offset from UTC in minutes, when sent.
    pub local_offset_min: Option<i32>,
}

/// SDC type 12.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguageCountry {
    pub short_id: u8,
    pub language: String,
    pub country: String,
}

/// SDC type 3: other frequencies carrying this multiplex.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AltFrequencies {
    pub synchronous: bool,
    /// Short ids carried there (bit n: short id n); 0xF for all.
    pub services: u8,
    /// Frequencies in kHz.
    pub khz: Vec<u32>,
}

/// One SDC block's contents.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sdc {
    pub afs_index: u8,
    pub multiplex: Option<Multiplex>,
    pub labels: Vec<(u8, String)>,
    pub audio: Vec<AudioInfo>,
    pub time: Option<TimeDate>,
    pub languages: Vec<LanguageCountry>,
    pub alt_frequencies: Vec<AltFrequencies>,
    /// Entity types seen, for diagnostics.
    pub types: Vec<u8>,
}

/// Modified Julian Date to calendar date (the algorithm of ETSI EN 300 468
/// annex C).
pub fn mjd_to_date(mjd: u32) -> (i32, u8, u8) {
    let mjd = mjd as f64;
    let yp = ((mjd - 15078.2) / 365.25).floor();
    let mp = ((mjd - 14956.1 - (yp * 365.25).floor()) / 30.6001).floor();
    let day = mjd - 14956.0 - (yp * 365.25).floor() - (mp * 30.6001).floor();
    let k = if mp == 14.0 || mp == 15.0 { 1.0 } else { 0.0 };
    let year = yp + k + 1900.0;
    let month = mp - 1.0 - k * 12.0;
    (year as i32, month as u8, day as u8)
}

/// A label's bytes as text: UTF-8, with the optional text control byte
/// (0x01 to 0x0F in front, clause 6.4.3.2) left off.
pub fn label_text(bytes: &[u8]) -> String {
    let b = match bytes.first() {
        Some(&c) if (1..=15).contains(&c) => &bytes[1..],
        _ => bytes,
    };
    String::from_utf8_lossy(b).trim_end_matches('\0').to_string()
}

/// Parse an SDC block of `bits` (energy dispersal undone). The data field
/// is floor((L - 20) / 8) bytes (table 21).
pub fn parse_sdc(bits: &[u8]) -> Option<Sdc> {
    if bits.len() < 28 {
        return None;
    }
    let n = (bits.len() - 20) / 8;
    // CRC over the AFS index as a byte (four zero bits in front) and the
    // data field.
    let mut crc_in = vec![0u8; 4];
    crc_in.extend_from_slice(&bits[..4 + 8 * n]);
    let bytes = coding::pack(&crc_in);
    let crc = coding::pack(&bits[4 + 8 * n..4 + 8 * n + 16]);
    if coding::crc16(&bytes) != u16::from_be_bytes([crc[0], crc[1]]) {
        return None;
    }
    let field = &bits[4..4 + 8 * n];
    let mut sdc = Sdc { afs_index: Bits::new(bits).take(4) as u8, ..Sdc::default() };
    let mut b = Bits::new(field);
    while b.left() >= 12 {
        let len = b.take(7) as usize;
        let version = b.take(1);
        let kind = b.take(4) as u8;
        if len == 0 && kind == 0 && version == 0 {
            // Padding.
            break;
        }
        let body_bits = 4 + 8 * len;
        if b.left() < body_bits {
            break;
        }
        let body = &field[b.pos..b.pos + body_bits];
        b.skip(body_bits);
        sdc.types.push(kind);
        let mut e = Bits::new(body);
        match kind {
            // Version flag 1 announces the next configuration: not yet ours.
            0 if version == 0 => {
                let pl_a = e.take(2) as u8;
                let pl_b = e.take(2) as u8;
                let streams = (0..len / 3).map(|_| (e.take(12) as usize, e.take(12) as usize)).collect();
                sdc.multiplex = Some(Multiplex { pl_a, pl_b, streams });
            }
            1 => {
                let short_id = e.take(2) as u8;
                e.skip(2);
                let bytes: Vec<u8> = (0..len.min(64)).map(|_| e.take(8) as u8).collect();
                sdc.labels.push((short_id, label_text(&bytes)));
            }
            3 => {
                let synchronous = e.take(1) == 1;
                let _layer = e.take(1);
                let restricted = e.take(1) == 1;
                let region = e.take(1) == 1;
                let mut left = len as isize;
                let mut services = 0xF;
                if restricted {
                    services = e.take(4) as u8;
                    e.skip(4);
                    left -= 1;
                }
                if region {
                    e.skip(8);
                    left -= 1;
                }
                if left >= 0 && left % 2 == 0 {
                    let khz = (0..left / 2)
                        .map(|_| {
                            let mult = e.take(1);
                            let v = e.take(15);
                            if mult == 1 { v * 10 } else { v }
                        })
                        .collect();
                    sdc.alt_frequencies.push(AltFrequencies { synchronous, services, khz });
                }
            }
            8 if len == 3 || len == 4 => {
                let mjd = e.take(17);
                let hour = e.take(5) as u8;
                let minute = e.take(6) as u8;
                let local_offset_min = if len == 4 {
                    let rfu = e.take(2);
                    let neg = e.take(1) == 1;
                    let v = e.take(5) as i32 * 30;
                    if rfu == 0 { Some(if neg { -v } else { v }) } else { None }
                } else {
                    None
                };
                if hour < 24 && minute < 60 {
                    let (year, month, day) = mjd_to_date(mjd);
                    sdc.time = Some(TimeDate { year, month, day, hour, minute, local_offset_min });
                }
            }
            9 if version == 0 && len >= 2 => {
                let short_id = e.take(2) as u8;
                let stream_id = e.take(2) as u8;
                let coding_ = e.take(2) as u8;
                let sbr = e.take(1) == 1;
                let audio_mode = e.take(2) as u8;
                let rate_code = e.take(3) as u8;
                let text = e.take(1) == 1;
                let enhancement = e.take(1) == 1;
                let mps = e.take(3) as u8;
                // The body after the two ids: the fixed bytes and the
                // codec specific configuration, as a decoder takes them.
                let raw = coding::pack(&body[4..]);
                sdc.audio.push(AudioInfo {
                    short_id,
                    stream_id,
                    coding: coding_,
                    sbr,
                    audio_mode,
                    rate_code,
                    text,
                    enhancement,
                    mps,
                    raw,
                });
            }
            12 if len == 5 => {
                let short_id = e.take(2) as u8;
                e.skip(2);
                let lang: Vec<u8> = (0..3).map(|_| e.take(8) as u8).collect();
                let country: Vec<u8> = (0..2).map(|_| e.take(8) as u8).collect();
                sdc.languages.push(LanguageCountry {
                    short_id,
                    language: String::from_utf8_lossy(&lang).to_string(),
                    country: String::from_utf8_lossy(&country).to_string(),
                });
            }
            _ => {}
        }
    }
    Some(sdc)
}
