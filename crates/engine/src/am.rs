//! Amplitude modulation demodulation for the HF airband.
//!
//! AM is not FM with a different detector: the carrier carries the audio as
//! an *envelope*, so the demodulator is rectify-and-average rather than a
//! phase discriminator. What makes it work on a real front end is the AGC —
//! shortwave stations differ by tens of decibels and a receiver built for
//! land-mobile levels would clip or starve — so the detector is followed by
//! a slow mean-level loop that holds the audio near full scale while leaving
//! the modulation itself untouched.
//!
//! The DC the detector produces IS the carrier; it is removed after the AGC
//! measures against it, which keeps deep modulation from pumping the gain.

use crate::biquad::Cascade;
use num_complex::Complex32;
use scannerd_dsp::OnePole;

/// Audio band for AM voice. Wider than narrowband FM's 3 kHz: aviation and
/// broadcast AM carry useful content to 4 kHz plus.
pub const AM_VOICE_LOWPASS_HZ: f32 = 4_000.0;

pub struct AmDemod {
    prev: Complex32,
    /// Mean-level AGC state: the smoothed carrier-plus-trough level.
    agc: f32,
    /// AGC time constant in samples.
    agc_tau: f32,
    voice_lp: Cascade,
    hp: OnePole,
}

impl AmDemod {
    pub fn new(fs: f64) -> Self {
        let fs32 = fs as f32;
        Self {
            prev: Complex32::new(0.0, 0.0),
            // ~50 ms: fast enough to track fading on HF, slow enough that
            // 100% modulated speech does not pump the gain at syllable rate.
            agc: 1.0,
            agc_tau: 0.05 * fs32,
            voice_lp: Cascade::lowpass(AM_VOICE_LOWPASS_HZ, fs32, 2),
            // Removes the detected carrier (DC) without touching the
            // modulation sidebands above it.
            hp: OnePole::new(0.0002 * fs32),
        }
    }

    /// Envelope-detect and normalise one block. Output is nominally ±1.0 for
    /// a fully modulated carrier at any input level within the AGC range.
    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<f32>) {
        out.clear();
        out.reserve(input.len());
        for &x in input {
            // Magnitude of the sample-to-sample product would measure the
            // modulation derivative; the envelope lives on the RF samples
            // themselves, so rectify those directly.
            let mag = x.norm();
            // Slow mean-level loop over the raw envelope (carrier included).
            self.agc += (mag - self.agc) / self.agc_tau.max(1.0);
            // Normalise, then remove the carrier: (mag - agc) is the
            // modulation centred on zero, scaled so 100% modulation peaks
            // near ±1.0 when the AGC has settled onto the carrier level.
            let audio = if self.agc > 1e-6 {
                (mag - self.agc) / self.agc
            } else {
                0.0
            };
            // Extra DC removal mops up AGC ripple before the audio filter.
            let dc_free = audio - self.hp.process(audio);
            out.push(self.voice_lp.process(dc_free).clamp(-1.5, 1.5));
        }
    }

    pub fn reset(&mut self) {
        self.prev = Complex32::new(0.0, 0.0);
        self.agc = 1.0;
        self.voice_lp.reset();
    }
}
