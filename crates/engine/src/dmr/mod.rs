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

/// Two-times interpolator: 8 kHz vocoder PCM to the 16 kHz audio rate.
///
/// Doubling the rate mirrors the vocoder's 0–4 kHz speech into 4–8 kHz,
/// and nothing downstream low-passes it away. Linear interpolation (the
/// old filter here) left a 3 kHz sibilant's image only 7 dB down and drooped
/// the sibilant itself by 3 dB: the metallic edge on P25, DMR and NXDN
/// voice. This is a Kaiser-windowed halfband instead, 47 taps long: the
/// input samples pass through untouched on the even outputs, and each odd
/// output is a symmetric 24-sample sum — 12 multiplies per input sample.
/// Flat to 0.1 dB through 3.3 kHz, images at least 58 dB down from 4.7 kHz
/// up (the mirror of 3.3 kHz), and 12 input samples (1.5 ms) of delay.
pub struct Resampler8k {
    /// The last `2 · HB_HALF` input samples, oldest first, as a ring.
    hist: [f32; 2 * HB_HALF],
    head: usize,
    coeffs: [f32; HB_HALF],
}

/// Odd-phase taps on each side of the interpolated point.
const HB_HALF: usize = 12;

impl Resampler8k {
    pub fn new() -> Self {
        Self {
            hist: [0.0; 2 * HB_HALF],
            head: 0,
            coeffs: halfband_coeffs(),
        }
    }

    pub fn process(&mut self, pcm: &[i16]) -> Vec<f32> {
        let n = self.hist.len();
        let mut out = Vec::with_capacity(pcm.len() * 2);
        for &s in pcm {
            self.hist[self.head] = f32::from(s) / 32768.0;
            self.head = (self.head + 1) % n;
            // `head` now indexes the oldest sample; the interpolated point
            // sits between ring offsets HB_HALF - 1 and HB_HALF from it.
            let at = |k: usize| self.hist[(self.head + k) % n];
            let mut mid = 0.0;
            for (j, &c) in self.coeffs.iter().enumerate() {
                mid += c * (at(HB_HALF - 1 - j) + at(HB_HALF + j));
            }
            out.push(at(HB_HALF - 1));
            out.push(mid);
        }
        out
    }
}

/// The halfband's odd-phase taps, nearest the center first: `sinc(n / 2)`
/// at odd `n` under a Kaiser window (β = 5.7, about 60 dB), scaled so the
/// interpolated phase has unit gain at DC like the pass-through phase.
fn halfband_coeffs() -> [f32; HB_HALF] {
    const BETA: f64 = 5.7;
    // Zeroth-order modified Bessel function of the first kind, by series.
    fn i0(x: f64) -> f64 {
        let (mut sum, mut term) = (1.0, 1.0);
        for k in 1..30 {
            term *= (x / (2.0 * k as f64)).powi(2);
            sum += term;
        }
        sum
    }
    let half_len = (2 * HB_HALF) as f64;
    let mut c = [0f64; HB_HALF];
    for (j, v) in c.iter_mut().enumerate() {
        let n = (2 * j + 1) as f64;
        let x = std::f64::consts::PI * n / 2.0;
        let sinc = x.sin() / x;
        let w = i0(BETA * (1.0 - (n / half_len).powi(2)).sqrt()) / i0(BETA);
        *v = sinc * w;
    }
    let dc: f64 = 2.0 * c.iter().sum::<f64>();
    std::array::from_fn(|j| (c[j] / dc) as f32)
}

/// Edges for the vocoder frames a receiver has to drop.
///
/// An AMBE+2 frame that fails its FEC is replaced with 20 ms of silence,
/// and `rmbe` has no erasure input to synthesize through the gap. A bare
/// `[0; 160]` stepped from wherever the waveform was to zero and back, two
/// broadband clicks per dropped frame — loudest exactly when the signal is
/// marginal and frames are dropping often. This ramps the last good sample
/// down to zero across the first few ms of the gap and fades the next good
/// frame in, so a drop sounds like a short mute rather than a pop.
pub(crate) struct Concealer {
    last: f32,
    erased: bool,
}

/// Ramp length: 5 ms at the vocoder's 8 kHz.
const CONCEAL_FADE: usize = 40;

impl Concealer {
    pub(crate) fn new() -> Self {
        // Starting "after a drop" fades the first frame of a call in too.
        Self { last: 0.0, erased: true }
    }

    /// Ramp weight `k` samples into a fade, rising from 0 to 1.
    fn rise(k: usize) -> f32 {
        0.5 - 0.5 * (std::f32::consts::PI * k as f32 / CONCEAL_FADE as f32).cos()
    }

    /// A frame that decoded: faded in if the one before it was dropped.
    pub(crate) fn good(&mut self, samples: &[i16], pcm: &mut Vec<i16>) {
        let fade = if self.erased { CONCEAL_FADE.min(samples.len()) } else { 0 };
        pcm.extend(samples.iter().enumerate().map(|(k, &s)| {
            if k < fade { (f32::from(s) * Self::rise(k)) as i16 } else { s }
        }));
        if let Some(&s) = samples.last() {
            self.last = f32::from(s);
        }
        self.erased = false;
    }

    /// A dropped frame: the last good sample eased to zero, then silence.
    pub(crate) fn erased(&mut self, pcm: &mut Vec<i16>) {
        let from = if self.erased { 0.0 } else { self.last };
        pcm.extend((0..rmbe::SAMPLES_PER_FRAME).map(|k| {
            if k < CONCEAL_FADE { (from * (1.0 - Self::rise(k + 1))) as i16 } else { 0 }
        }));
        self.last = 0.0;
        self.erased = true;
    }
}

impl Default for Concealer {
    fn default() -> Self {
        Self::new()
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

    /// Power of `x` at `hz` (sample rate `fs`), by single-bin DFT.
    fn power_at(x: &[f32], hz: f64, fs: f64) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (n, &v) in x.iter().enumerate() {
            let a = std::f64::consts::TAU * hz * n as f64 / fs;
            re += f64::from(v) * a.cos();
            im -= f64::from(v) * a.sin();
        }
        (re * re + im * im) / (x.len() as f64).powi(2)
    }

    /// (passband gain, image level relative to the tone) in dB for a tone at
    /// `hz` through the 8→16 kHz upsampler.
    fn upsample_response(hz: f64) -> (f64, f64) {
        // A whole number of cycles over the measured window keeps the DFT
        // bins clean: 8000 input samples is one second.
        let pcm: Vec<i16> = (0..8_400)
            .map(|n| (16_000.0 * (std::f64::consts::TAU * hz * n as f64 / 8_000.0).sin()) as i16)
            .collect();
        let mut rs = Resampler8k::new();
        let out = rs.process(&pcm);
        let window = &out[400..400 + 16_000];
        let tone = power_at(window, hz, 16_000.0);
        let image = power_at(window, 8_000.0 - hz, 16_000.0);
        let ideal = (16_000.0f64 / 32_768.0).powi(2) / 4.0;
        (10.0 * (tone / ideal).log10(), 10.0 * (image / tone).log10())
    }

    /// The vocoder's 0–4 kHz speech mirrors into 4–8 kHz when it is doubled
    /// to the 16 kHz audio rate. Linear interpolation left those images
    /// only 7 dB under a 3 kHz sibilant and drooped the tone itself by 3 dB;
    /// nothing downstream low-passes, so the images reached the listener as
    /// a metallic edge on every P25, DMR and NXDN call.
    #[test]
    fn the_upsampler_rejects_its_images_and_keeps_the_voice_band_flat() {
        for hz in [300.0, 1_000.0, 2_000.0, 3_000.0, 3_300.0] {
            let (gain, image) = upsample_response(hz);
            eprintln!("{hz:>6.0} Hz: gain {gain:+.2} dB, image {image:.1} dB");
            assert!(gain.abs() < 0.5, "{hz} Hz: passband gain {gain:.2} dB");
            assert!(image < -50.0, "{hz} Hz: image only {image:.1} dB down");
        }
    }

    /// Ten frames of a 1012.5 Hz tone (each frame ends a quarter cycle on,
    /// at a crest) with frames 2, 5 and 6 dropped. Returns the 8 kHz PCM
    /// built with the concealer, and with bare zeros.
    fn dropped_frames() -> (Vec<i16>, Vec<i16>) {
        let n = rmbe::SAMPLES_PER_FRAME;
        let mut concealer = Concealer::new();
        let (mut smooth, mut hard) = (Vec::new(), Vec::new());
        for f in 0..10 {
            let frame: Vec<i16> = (0..n)
                .map(|k| {
                    let t = (f * n + k) as f64 / 8_000.0;
                    (16_000.0 * (std::f64::consts::TAU * 1_012.5 * t).sin()) as i16
                })
                .collect();
            if [2, 5, 6].contains(&f) {
                concealer.erased(&mut smooth);
                hard.extend([0i16; rmbe::SAMPLES_PER_FRAME]);
            } else {
                concealer.good(&frame, &mut smooth);
                hard.extend(&frame);
            }
        }
        (smooth, hard)
    }

    /// Energy 3.4–4 kHz, where the tone has none: what a click splatters.
    fn splatter(pcm: &[i16]) -> f64 {
        let x: Vec<f32> = pcm.iter().map(|&s| f32::from(s) / 32768.0).collect();
        (0..13).map(|k| power_at(&x, 3_400.0 + 50.0 * k as f64, 8_000.0)).sum()
    }

    /// A dropped AMBE+2 frame used to be a bare 20 ms of zeros: a step from
    /// the waveform to silence and back, two clicks. Eased edges keep the
    /// gap's splatter well under the hard-edged version's, and never jump
    /// further between samples than the tone itself does.
    #[test]
    fn dropped_frames_fade_instead_of_clicking() {
        let (smooth, hard) = dropped_frames();
        assert_eq!(smooth.len(), hard.len());
        let gain_db = 10.0 * (splatter(&smooth) / splatter(&hard)).log10();
        let max_step = |pcm: &[i16]| {
            pcm.windows(2).map(|w| (i32::from(w[1]) - i32::from(w[0])).abs()).max().unwrap()
        };
        eprintln!(
            "splatter {gain_db:.1} dB vs bare zeros; max step {} vs {}",
            max_step(&smooth),
            max_step(&hard)
        );
        assert!(gain_db < -15.0, "splatter only {gain_db:.1} dB below bare zeros");
        // The tone's own steepest sample-to-sample change is about 12 700.
        assert!(max_step(&smooth) < 13_000, "max step {}", max_step(&smooth));
    }

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
