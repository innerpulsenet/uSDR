//! FST4W: the WSPR-like beacon modes of WSJT-X, decoded natively. Receive
//! only, and FST4W only, not FST4.
//!
//! A transmission carries 50 bits (a call, a locator and a power) with a
//! 24-bit CRC under a (240, 74) LDPC code, as 160 symbols of 4-GFSK: five
//! 8-symbol sync words and four runs of 30 data symbols. It starts 1 s into
//! its slot; the T/R period sets the symbol length, and with it the tone
//! spacing and the reach:
//!
//! | period | samples/symbol at 12 kHz | tone spacing | frame |
//! |--------|--------------------------|--------------|-------|
//! | 120 s  | 8200                     | 1.463 Hz     | 109.3 s |
//! | 300 s  | 21504                    | 0.558 Hz     | 286.7 s |
//! | 900 s  | 66560                    | 0.180 Hz     | 887.5 s |
//! | 1800 s | 134400                   | 0.089 Hz     | 1792.0 s |
//!
//! Audio is mixed to the same 375 Hz baseband around 1500 Hz as WSPR
//! ([`crate::wspr::Downconverter`]). A slot's baseband is transformed whole;
//! candidates are peaks of the 4-tone sum over the noise floor; each is cut
//! from that spectrum to 40 samples a symbol, synced in frequency and time
//! against the sync words, then decoded from single symbols and from blocks
//! of 2, 4 and 8 symbols summed coherently (the source's four metric sets):
//! belief propagation first, then an ordered-statistics search. Every decode
//! must pass the 24-bit CRC. The source's no-CRC "Keff=50" pass and its
//! noise blanker are not carried over, and decoded signals are not
//! subtracted (the source does not either).
//!
//! Protocol constants, the code, and the structure of the decoder follow
//! WSJT-X (`lib/fst4/`, `lib/fst4_decode.f90`) by Joe Taylor K1JT and Steven
//! Franke K9AN, GPL-3.

mod demod;
mod ldpc;
mod pack;
mod stream;
mod tables;

pub use pack::{CallHashes, Message};
pub use stream::{Fst4wDecoder, Fst4wStatus};

use crate::wspr::Downconverter;
use num_complex::Complex32;

/// The baseband the decoder works at and the audio frequency it is centered
/// on, shared with WSPR's front end.
pub const BASEBAND_HZ: f32 = 375.0;
pub const CENTER_HZ: f32 = 1_500.0;

/// A T/R period.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Period {
    S120,
    S300,
    S900,
    S1800,
}

impl Period {
    pub const ALL: [Period; 4] = [Period::S120, Period::S300, Period::S900, Period::S1800];

    pub const fn secs(self) -> u32 {
        match self {
            Period::S120 => 120,
            Period::S300 => 300,
            Period::S900 => 900,
            Period::S1800 => 1800,
        }
    }

    /// Slot length, ms.
    pub const fn slot_ms(self) -> u64 {
        self.secs() as u64 * 1000
    }

    /// The spot tag, e.g. "F-120".
    pub const fn tag(self) -> &'static str {
        match self {
            Period::S120 => "F-120",
            Period::S300 => "F-300",
            Period::S900 => "F-900",
            Period::S1800 => "F-1800",
        }
    }

    /// Samples per symbol at 12 kHz, as `fst4_decode.f90` sets them.
    pub const fn nsps(self) -> u32 {
        match self {
            Period::S120 => 8200,
            Period::S300 => 21504,
            Period::S900 => 66560,
            Period::S1800 => 134400,
        }
    }

    /// Tone spacing and symbol rate, Hz.
    pub fn baud(self) -> f32 {
        12_000.0 / self.nsps() as f32
    }

    /// Frame length, s.
    pub fn frame_secs(self) -> f32 {
        160.0 * self.nsps() as f32 / 12_000.0
    }

    /// Audio kept from each slot: past the end of any transmission that
    /// starts within the 3 s search.
    pub const fn keep_secs(self) -> u32 {
        match self {
            Period::S120 => 115,
            Period::S300 => 294,
            Period::S900 => 895,
            Period::S1800 => 1797,
        }
    }

    /// Whole symbols in the slot's transform (the last of them padded): a
    /// whole number makes the downconverted signal exactly 40 samples a
    /// symbol, and for 120 s, where a symbol is 256.25 baseband samples, a
    /// multiple of four makes the transform length whole too.
    fn window_symbols(self) -> usize {
        match self {
            Period::S120 => 172,
            Period::S300 => 165,
            Period::S900 => 162,
            Period::S1800 => 161,
        }
    }

    fn window_samples(self) -> usize {
        self.window_symbols() * self.nsps() as usize / 32
    }
}

/// One transmission decoded from a slot.
#[derive(Clone, Debug, PartialEq)]
pub struct Fst4wDecode {
    pub period: Period,
    /// The period's spot tag ("F-120").
    pub tag: &'static str,
    /// Signal to noise in 2500 Hz.
    pub snr_db: f32,
    /// Start relative to the nominal 1 s into the slot.
    pub dt_s: f32,
    /// Audio frequency of the center of the four tones.
    pub freq_hz: f32,
    /// Audio frequency of the lowest tone, which WSJT-X reports for FST4W.
    pub tone0_hz: f32,
    /// How fast the signal moved, Hz per minute, fit from the decoded tones
    /// and only reported: the decoder does not search over drift, and the
    /// fit is only good to about 0.8 tone across the frame.
    pub drift_hz_per_min: f32,
    pub call: String,
    pub grid: Option<String>,
    /// `None` for a type 3 message.
    pub power_dbm: Option<i8>,
    /// As WSJT-X prints it: "K1ABC FN42 37".
    pub text: String,
    /// "type1", "type2" or "type3".
    pub kind: &'static str,
    /// Code bits corrected against the metric set that decoded: a measure
    /// of how near the edge of decoding this message was.
    pub hard_errors: usize,
}

struct Found {
    bits: u64,
    period: Period,
    freq: f32,
    start: usize,
    snr_db: f32,
    drift_tones: f32,
    hard_errors: usize,
}

/// Decode a slot of baseband at [`BASEBAND_HZ`] centered on [`CENTER_HZ`],
/// starting at the slot boundary and `period` long (or up to its keep).
/// Calls heard in full are added to `hashes`, and type 3 hashes are looked up
/// in it, including those whose call was heard in the same slot.
pub fn decode_baseband(z: &[Complex32], period: Period, hashes: &mut CallHashes) -> Vec<Fst4wDecode> {
    let ms = z.iter().map(|v| v.norm_sqr()).sum::<f32>() / z.len().max(1) as f32;
    if !(ms > 0.0 && ms.is_finite()) || z.len() < 1000 {
        return Vec::new();
    }
    let scale = 1.0 / ms.sqrt();
    let z: Vec<Complex32> = z.iter().map(|v| v * scale).collect();
    let refs = demod::Refs::new();
    let mut bb = demod::Baseband::new(&z, period);
    let baud = period.baud();
    let fs2 = demod::NSS as f32 * baud;
    let mut synced: Vec<(f32, usize, f32)> = Vec::new();
    for cand in bb.candidates() {
        let c2 = bb.downsample(cand.f);
        let (fcbest, isbest) = demod::sync_search(&c2, period, &refs);
        let f = cand.f + fcbest;
        if synced.iter().any(|&(g, s, _)| (g - f).abs() < 0.10 * baud && (s as i64 - isbest as i64).abs() <= 2) {
            continue;
        }
        synced.push((f, isbest, cand.base));
    }
    let mut found: Vec<Found> = Vec::new();
    'cand: for &(f, start, base) in &synced {
        if found.iter().any(|d| (d.freq - f).abs() < 0.5 * baud) {
            continue;
        }
        let c2 = bb.downsample(f);
        for jitter in [0i64, 1, -1] {
            let s = start as i64 + jitter;
            if s < 0 {
                continue;
            }
            let Some(cs) = demod::tone_correlations(&c2, s as usize, &refs) else { continue };
            let Some(metrics) = demod::bit_metrics(&cs) else { continue };
            for m in &metrics {
                let llr = demod::llrs(m);
                let Some(d) = ldpc::decode(&llr) else { continue };
                let Some(bits) = ldpc::payload(&d.message) else { continue };
                if found.iter().any(|x| x.bits == bits) {
                    continue 'cand;
                }
                let tones = demod::tones_of(&d.codeword);
                let (df, drift_tones) = demod::fit_residual(&cs, &tones);
                let snr_db = demod::snr_db(&cs, &tones, bb.n0(base), baud);
                found.push(Found { bits, period, freq: f + df * baud, start: s as usize, snr_db, drift_tones, hard_errors: d.hard_errors });
                continue 'cand;
            }
        }
    }
    let frame = period.frame_secs();
    // Once to learn every call sent in full, then again to name the hashes.
    let first: Vec<Option<Message>> = found.iter().map(|f| pack::unpack(f.bits, hashes)).collect();
    let mut out: Vec<Fst4wDecode> = found
        .iter()
        .zip(first)
        .filter_map(|(f, m)| {
            let m = m?;
            let m = if m.kind == "type3" { pack::unpack(f.bits, hashes)? } else { m };
            Some(Fst4wDecode {
                period: f.period,
                tag: f.period.tag(),
                snr_db: f.snr_db.clamp(-50.0, 40.0).round(),
                dt_s: ((f.start as f32 - fs2) / fs2 * 10.0).round() / 10.0,
                freq_hz: CENTER_HZ + f.freq,
                tone0_hz: CENTER_HZ + f.freq - 1.5 * baud,
                drift_hz_per_min: f.drift_tones * baud * 60.0 / frame,
                call: m.call,
                grid: m.grid,
                power_dbm: m.power_dbm,
                text: m.text,
                kind: m.kind,
                hard_errors: f.hard_errors,
            })
        })
        .collect();
    out.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));
    out
}

/// Decode one slot of real audio at `fs` Hz that starts at the slot boundary.
pub fn decode_audio(audio: &[f32], fs: f32, period: Period, hashes: &mut CallHashes) -> Vec<Fst4wDecode> {
    let mut dc = Downconverter::new(fs);
    let mut out = Vec::new();
    dc.push(audio, &mut out);
    dc.flush(&mut out);
    let z: Vec<Complex32> = out.into_iter().map(|(_, v)| v).collect();
    decode_baseband(&z, period, hashes)
}

/// Pack `text` ("K1ABC FN42 37", "PJ4/K1ABC 37", "<K1ABC> FN42AX"), encode
/// it, and give the 160 channel tones (0..3); `None` if not an FST4W message.
pub fn channel_tones(text: &str) -> Option<[u8; demod::NN]> {
    let bits = pack::pack(text)?;
    Some(demod::tones_of(&ldpc::encode(&ldpc::with_crc(bits))))
}

#[cfg(test)]
mod tests;
