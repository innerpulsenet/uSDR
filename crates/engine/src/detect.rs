//! Channel activity detection from the wideband spectrum.
//!
//! The measurement is taken across the whole tuned span rather than per channel,
//! which is the idea borrowed from hfscan (`main.rs:1678`): decide a channel is
//! busy from the RF spectrum, not from audio power after demodulation. One FFT
//! then serves every channel in the span, so watching twenty channels costs
//! about what watching one does.
//!
//! The estimator itself is *not* hfscan's. hfscan compares peak bins against a
//! minimum-statistics floor, which suits a panadapter looking for the faintest
//! possible carrier. Measured on noise here, that combination reads **+13 dB**
//! on an empty channel: the peak of fifty bins runs well above the mean, and a
//! minimum tracker sits well below it, so the two biases add. A squelch built on
//! it would need a threshold so high that ordinary traffic fell under it.
//!
//! So this is a plain energy detector: **mean power across the passband**
//! against the **median of the surrounding bins**. The mean is unbiased and its
//! variance falls with both averaging and passband width; the median is a robust
//! noise estimate that neighbouring carriers cannot drag upward. Taking the
//! reference locally rather than span-wide keeps the tuner's roll-off from being
//! read as signal near the span edges.

use num_complex::Complex32;
use scannerd_dsp::{Spectrum, smooth_bins};

/// How far either side of a channel the noise reference is drawn from,
/// as a multiple of the channel's own bandwidth.
const GUARD_SPAN: f64 = 8.0;

/// Guard bins needed before the median is trusted.
const MIN_GUARD_BINS: usize = 16;

pub struct SpanDetector {
    spec: Spectrum,
    fs: f64,
    center: f64,
    nfft: usize,
    power: Vec<f32>,
    smoothed: Vec<f32>,
    buf: Vec<Complex32>,
    scratch: Vec<f32>,
    fresh: bool,
}

impl SpanDetector {
    pub fn new(nfft: usize, fs: f64, center: f64) -> Self {
        Self {
            spec: Spectrum::new(nfft),
            fs,
            center,
            nfft,
            power: Vec::new(),
            smoothed: Vec::new(),
            buf: Vec::new(),
            scratch: Vec::new(),
            fresh: false,
        }
    }

    pub fn set_center(&mut self, center: f64) {
        self.center = center;
    }

    pub fn bin_hz(&self) -> f64 {
        self.fs / self.nfft as f64
    }

    /// The most recent spectrum, in dB, fftshifted. Empty until one exists.
    pub fn spectrum(&self) -> &[f32] {
        if self.fresh { &self.smoothed } else { &[] }
    }

    /// Absorb samples. Returns true when a new spectrum became available, which
    /// is the only time [`snr_db`](Self::snr_db) has anything new to say.
    ///
    /// Input is buffered to a whole FFT before being handed on. `Spectrum`
    /// leaves its output *untouched* when it has no complete segment, so a
    /// caller that cannot tell "produced" from "unchanged" would keep reading a
    /// stale spectrum and believe it was current. Feeding it at least one full
    /// FFT at a time removes the ambiguity rather than guessing at it.
    pub fn feed(&mut self, iq: &[Complex32]) -> bool {
        self.buf.extend_from_slice(iq);
        if self.buf.len() < self.nfft {
            self.fresh = false;
            return false;
        }
        let batch = std::mem::take(&mut self.buf);
        self.spec.power_db(&batch, &mut self.power);
        if self.power.len() != self.nfft {
            self.fresh = false;
            return false;
        }
        smooth_bins(&self.power, 3, &mut self.smoothed);
        self.fresh = true;
        true
    }

    /// Signal-to-noise for one channel, in dB.
    pub fn snr_db(&self, freq_hz: f64, bandwidth_hz: f64) -> Option<f32> {
        if !self.fresh {
            return None;
        }
        let (lo, hi) = self.bin_range(freq_hz, bandwidth_hz)?;

        // Mean power in the linear domain: an FM channel's energy is spread
        // across its passband and moves around within it, so total power is what
        // stays steady while any individual bin does not.
        let mut acc = 0.0f64;
        for i in lo..=hi {
            acc += 10f64.powf(self.smoothed[i] as f64 / 10.0);
        }
        let signal_db = 10.0 * (acc / (hi - lo + 1) as f64).log10();

        let noise_db = self.guard_median(freq_hz, bandwidth_hz)?;
        Some((signal_db - noise_db) as f32)
    }

    /// Median level of the bins flanking a channel, excluding the channel.
    fn guard_median(&self, freq_hz: f64, bandwidth_hz: f64) -> Option<f64> {
        let (inner_lo, inner_hi) = self.bin_range(freq_hz, bandwidth_hz)?;
        let (outer_lo, outer_hi) = self.bin_range(freq_hz, bandwidth_hz * GUARD_SPAN)?;
        let mut guard: Vec<f32> = (outer_lo..=outer_hi)
            .filter(|i| *i < inner_lo || *i > inner_hi)
            .map(|i| self.smoothed[i])
            .collect();
        if guard.len() < MIN_GUARD_BINS {
            // Near a span edge there may not be room either side; fall back to
            // the whole span, which is still mostly empty on any real scanner.
            guard = self.smoothed.clone();
        }
        if guard.is_empty() {
            return None;
        }
        guard.sort_by(f32::total_cmp);
        Some(guard[guard.len() / 2] as f64)
    }

    fn bin_range(&self, freq_hz: f64, bandwidth_hz: f64) -> Option<(usize, usize)> {
        let bin_hz = self.bin_hz();
        let mid = self.nfft as f64 / 2.0;
        let to_bin = |f: f64| ((f - self.center) / bin_hz + mid).round();
        let lo = to_bin(freq_hz - bandwidth_hz / 2.0);
        let hi = to_bin(freq_hz + bandwidth_hz / 2.0);
        if hi < 0.0 || lo >= self.nfft as f64 {
            return None;
        }
        let lo = lo.max(0.0) as usize;
        let hi = hi.min(self.nfft as f64 - 1.0) as usize;
        (lo <= hi).then_some((lo, hi))
    }

    /// Reusable scratch, kept so callers can borrow without allocating.
    pub fn scratch(&mut self) -> &mut Vec<f32> {
        &mut self.scratch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    const CENTER: f64 = 155_000_000.0;
    const FS: f64 = 1_024_000.0;
    const NFFT: usize = 4096;

    /// Noise, optionally with a carrier at `offset_hz`.
    fn samples(n: usize, offset_hz: f64, amp: f32, seed: u32) -> Vec<Complex32> {
        let mut s = seed;
        let mut rnd = move || {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s >> 8) as f32 / 8388608.0 - 1.0
        };
        (0..n)
            .map(|k| {
                let (nr, ni) = (rnd(), rnd());
                let p = TAU * offset_hz * k as f64 / FS;
                Complex32::new(
                    amp * p.cos() as f32 + nr * 0.01,
                    amp * p.sin() as f32 + ni * 0.01,
                )
            })
            .collect()
    }

    /// Feed a realistic batch: the device delivers 16k blocks, so the Welch
    /// average has several segments rather than one.
    fn fed(offset_hz: f64, amp: f32) -> SpanDetector {
        let mut d = SpanDetector::new(NFFT, FS, CENTER);
        for i in 0..4 {
            assert!(d.feed(&samples(NFFT * 4, offset_hz, amp, 7 + i)));
        }
        d
    }

    #[test]
    fn a_carrier_in_the_passband_stands_above_the_noise() {
        let d = fed(7500.0, 0.3);
        let snr = d
            .snr_db(CENTER + 7500.0, 12_500.0)
            .expect("a spectrum by now");
        assert!(
            snr > 15.0,
            "carrier SNR {snr:.1} dB should be well above noise"
        );
    }

    /// The bias this module exists to control: an empty channel must read near
    /// zero, so a squelch threshold can be set close to the noise.
    #[test]
    fn an_empty_channel_reads_near_zero() {
        let d = fed(0.0, 0.0);
        let snr = d.snr_db(CENTER + 200_000.0, 12_500.0).expect("in span");
        assert!(snr.abs() < 4.0, "quiet channel read {snr:.1} dB");
    }

    /// A carrier must not lift channels it is not in, or every scanner channel
    /// opens whenever any one of them does.
    #[test]
    fn a_carrier_outside_the_passband_is_not_counted() {
        let d = fed(7500.0, 0.3);
        let far = d.snr_db(CENTER + 300_000.0, 12_500.0).expect("in span");
        assert!(far.abs() < 4.0, "an unrelated channel read {far:.1} dB");
    }

    /// A strong neighbour is exactly what the median guard is for: it must not
    /// become the noise reference and desensitise the channel next door.
    #[test]
    fn a_strong_neighbour_does_not_raise_the_noise_reference() {
        let mut d = SpanDetector::new(NFFT, FS, CENTER);
        for i in 0..4 {
            let mut block = samples(NFFT * 4, 25_000.0, 0.5, 11 + i);
            // Add a second, weaker carrier 50 kHz away.
            for (k, s) in block.iter_mut().enumerate() {
                let p = TAU * 75_000.0 * k as f64 / FS;
                *s += Complex32::new(0.02 * p.cos() as f32, 0.02 * p.sin() as f32);
            }
            d.feed(&block);
        }
        let weak = d.snr_db(CENTER + 75_000.0, 12_500.0).expect("in span");
        assert!(weak > 8.0, "the weaker neighbour read only {weak:.1} dB");
    }

    #[test]
    fn a_channel_outside_the_span_has_no_answer() {
        let d = fed(0.0, 0.0);
        assert!(d.snr_db(156_000_000.0, 12_500.0).is_none());
    }

    #[test]
    fn nothing_is_reported_before_a_full_segment() {
        let mut d = SpanDetector::new(NFFT, FS, CENTER);
        assert!(!d.feed(&samples(64, 0.0, 0.1, 3)));
        assert!(d.snr_db(CENTER, 12_500.0).is_none());
    }
}
