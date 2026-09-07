//! DMR (ETSI TS 102 361) conventional reception.
//!
//! DMR is 4-level FSK at 4800 symbols/s with deviations of ±648 and
//! ±1944 Hz, so the same discriminator-plus-boxcar front end the P25 chains
//! use recovers it; only the levels and the framing differ. The air interface
//! is TDMA: a repeating 30 ms frame of one 24-bit CACH (Common Announcement
//! Channel, carrying the timeslot number) followed by one 264-bit burst. A
//! repeater alternates timeslot 1 and 2 in consecutive frames; a simplex
//! radio transmits in every frame.
//!
//! A burst is 54 dibits of payload, a 48-bit centre field, and 54 more
//! dibits of payload. The centre field is one of:
//!
//! - a 48-bit **sync word** (voice or data, BS- or MS-sourced) — voice sync
//!   marks burst A of a six-burst voice superframe;
//! - **EMB + embedded signalling** in voice bursts B–F: 16 bits of EMB (a
//!   QR(16,7,6)-protected field carrying the colour code) around 32 bits of
//!   embedded link control.
//!
//! Data bursts carry a 20-bit Slot Type PDU, Golay(20,8)-protected, split
//! around the centre field; it names the colour code and the burst's data
//! type (voice header, terminator, CSBK, ...).
//!
//! Voice bursts carry three AMBE+2 frames in their 216 payload bits. Each
//! 72-bit frame is the same "3600x2450" codeword P25 Phase 2 uses — C0
//! (Golay(24,12)), C1 (Golay(23,12)), C2 and C3 (uncoded) — under a
//! DMR-specific interleave, and feeds the same `rmbe` decoder.
//!
//! Algorithms and constant tables were cross-checked against dsd, dsd-fme
//! and mbelib (see `refs/`); the code here is original.

mod channel;
mod csbk;
mod data;
mod fec;
mod framer;
mod sync;
pub(crate) mod voice;

pub use channel::{DmrChannelReceiver, DmrSpec};
pub use csbk::{ControlDecoder, Csbk, CsbkError, decode_csbk};
pub(crate) use data::trellis_three_quarter;
pub use data::{
    DataAssembler, DataBlock, DataError, DataHeader, DataMessage, DataPdu, DecodedData,
    EmbeddedLcAssembler, LinkControl, PrivacyHeader, decode_data_pdu,
};
pub use framer::{Burst, BurstKind, DmrReceiver, SyncSource};
#[cfg(test)]
pub(crate) use framer::{encode_cach, encode_data_burst, modulate};
#[cfg(test)]
pub(crate) use sync::SyncKind;
pub use voice::VoiceFrame;

/// Symbol rate on a DMR channel: 4800 symbols/s.
pub const SYMBOL_RATE: f64 = 4800.0;

/// Samples per symbol at the 48 kHz channel rate the DecodeChain targets.
pub const SPS: usize = 10;

/// Channel rate that yields exactly [`SPS`] samples per symbol.
pub const CHANNEL_RATE: f64 = SYMBOL_RATE * SPS as f64;

/// Channel filter for DMR 4FSK: the 12.5 kHz allocation, as for C4FM.
pub const DMR_BANDWIDTH_HZ: f32 = 12_500.0;

/// Outer and inner symbol deviations, in Hz (ETSI TS 102 361-1 §10.2.3).
pub const DEV_OUTER_HZ: f32 = 1944.0;
pub const DEV_INNER_HZ: f32 = 648.0;

/// Dibits in one burst: 54 + 24 (centre field) + 54.
pub const BURST_DIBITS: usize = 132;
/// Dibits in the CACH that precedes each burst.
pub const CACH_DIBITS: usize = 12;
/// One TDMA frame: CACH + burst. 144 dibits at 4800 sym/s is 30 ms.
pub const FRAME_DIBITS: usize = CACH_DIBITS + BURST_DIBITS;

/// The dibit a DMR frequency sample represents. The same Gray map C4FM uses:
/// `01`→+outer, `00`→+inner, `10`→−inner, `11`→−outer.
pub fn slice_dibit(hz: f32) -> u8 {
    slice_at(hz, DEV_OUTER_HZ)
}

/// Slice against a fitted outer deviation, as in P25 Phase 2.
pub fn slice_at(hz: f32, outer: f32) -> u8 {
    let mid = outer * (DEV_OUTER_HZ + DEV_INNER_HZ) / (2.0 * DEV_OUTER_HZ);
    if hz > mid {
        0b01
    } else if hz > 0.0 {
        0b00
    } else if hz > -mid {
        0b10
    } else {
        0b11
    }
}

/// Ideal frequency for a dibit, the inverse of [`slice_dibit`].
pub fn dibit_level(dibit: u8) -> f32 {
    match dibit & 0b11 {
        0b01 => DEV_OUTER_HZ,
        0b00 => DEV_INNER_HZ,
        0b10 => -DEV_INNER_HZ,
        _ => -DEV_OUTER_HZ,
    }
}

/// Exact samples per symbol at this channel rate. The analog scanner's
/// 2.048 MS/s decimates to 2_048_000/43 Hz, which is not 10.000 samples
/// per DMR symbol — the framer steps by this fraction, not the rounded
/// boxcar length from [`samples_per_symbol`].
pub fn symbol_period(fs: f64) -> f64 {
    fs / SYMBOL_RATE
}

/// Nearest whole samples per symbol: the FM boxcar length. Not the stride
/// the framer uses to walk bursts; that is [`symbol_period`].
pub fn samples_per_symbol(fs: f64) -> usize {
    symbol_period(fs).round().max(1.0) as usize
}

/// Least-squares fit of `samples ≈ amp · ideal + offset`.
///
/// The signed slope doubles as a polarity detector: an RF chain that inverts
/// the signal (a lower-sideband mix, say) fits a negative amplitude, and the
/// dibit map then flips its sign bit.
pub fn fit_symbols(samples: &[f32], ideal: &[f32]) -> (f32, f32) {
    let n = samples.len().min(ideal.len()) as f32;
    if n < 2.0 {
        return (DEV_OUTER_HZ, 0.0);
    }
    let (mut sx, mut sy, mut sxy, mut sxx) = (0.0, 0.0, 0.0, 0.0);
    for (y, x) in samples.iter().zip(ideal) {
        sx += x;
        sy += y;
        sxy += x * y;
        sxx += x * x;
    }
    let den = n * sxx - sx * sx;
    if den.abs() < 1e-6 {
        return (DEV_OUTER_HZ, 0.0);
    }
    let slope = (n * sxy - sx * sy) / den;
    let offset = (sy - slope * sx) / n;
    ((slope * DEV_OUTER_HZ).abs().max(100.0), offset)
}

/// Two-times linear resampler: 8 kHz vocoder PCM to the 16 kHz audio rate.
///
/// The same trivial interpolator the P25 voice follower uses; the leveler
/// downstream smooths whatever imaging survives.
pub struct Resampler8k {
    prev: i16,
    started: bool,
}

impl Resampler8k {
    pub fn new() -> Self {
        Self {
            prev: 0,
            started: false,
        }
    }

    pub fn process(&mut self, pcm: &[i16]) -> Vec<f32> {
        let mut out = Vec::with_capacity(pcm.len() * 2);
        for &s in pcm {
            if self.started {
                // i16 + i16 overflows at full-scale extremes (the vocoder
                // can emit both samples near ±32767); widen before adding.
                out.push((i32::from(self.prev) + i32::from(s)) as f32 / 65536.0);
            } else {
                out.push(f32::from(s) / 32768.0);
                self.started = true;
            }
            out.push(f32::from(s) / 32768.0);
            self.prev = s;
        }
        out
    }
}

impl Default for Resampler8k {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dibit_map_round_trips_through_the_slicer() {
        for d in 0..4u8 {
            assert_eq!(slice_dibit(dibit_level(d)), d, "dibit {d}");
        }
    }

    #[test]
    fn dmr_geometry_is_consistent() {
        assert_eq!(BURST_DIBITS, 54 + 24 + 54);
        assert_eq!(FRAME_DIBITS, 144);
        // A DMR TDMA frame is exactly 30 ms (144 symbols at 4800 baud).
        assert_eq!(FRAME_DIBITS as f64 / SYMBOL_RATE, 0.030);
    }

    #[test]
    fn analog_decimation_is_not_an_integer_number_of_samples_per_symbol() {
        let fs = 2_048_000.0 / 43.0;
        let period = symbol_period(fs);
        assert!((period - 10.0).abs() > 0.05, "sps={period}");
        assert_eq!(samples_per_symbol(fs), 10);
    }
}
